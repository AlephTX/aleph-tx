//! Per-venue sliding-window order budget, soft-cap entry suppression, and 429 cooldown/halt guard.
//!
//! Risk-reducing exits, emergency hedges, and cancels are strictly exempt from all caps,
//! cooldowns, and halts so open exposure can always be flattened.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

/// Classification of an order intent for per-venue budget admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderIntentClass {
    RiskIncreasingEntry,
    ReduceOnlyExit,
    EmergencyHedge,
    Cancel,
}

/// Per-venue sliding order weight budget and 429 rate-limit escalation settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OrderBudgetConfig {
    pub window_ms: u64,
    pub max_weight_per_window: u32,
    pub entry_soft_cap_pct: u8,
    pub cooldown_ms_on_rate_limit: u64,
    pub max_consecutive_rate_limits_before_halt: u32,
}

impl Default for OrderBudgetConfig {
    fn default() -> Self {
        Self {
            window_ms: 60_000,
            max_weight_per_window: 60,
            entry_soft_cap_pct: 80,
            cooldown_ms_on_rate_limit: 15_000,
            max_consecutive_rate_limits_before_halt: 5,
        }
    }
}

impl OrderBudgetConfig {
    /// Validate that every window, cap, cooldown, and escalation threshold is strictly positive and bounded.
    pub fn validate(&self) -> Result<()> {
        ensure!(self.window_ms > 0, "order_budget.window_ms must be > 0");
        ensure!(
            self.max_weight_per_window > 0,
            "order_budget.max_weight_per_window must be > 0"
        );
        ensure!(
            (1..=100).contains(&self.entry_soft_cap_pct),
            "order_budget.entry_soft_cap_pct must be in 1..=100"
        );
        ensure!(
            self.cooldown_ms_on_rate_limit > 0,
            "order_budget.cooldown_ms_on_rate_limit must be > 0"
        );
        ensure!(
            self.max_consecutive_rate_limits_before_halt > 0,
            "order_budget.max_consecutive_rate_limits_before_halt must be > 0"
        );
        Ok(())
    }

    /// Soft entry weight threshold within one sliding window (`>= 1`).
    pub fn entry_soft_limit(&self) -> u32 {
        ((u64::from(self.max_weight_per_window) * u64::from(self.entry_soft_cap_pct)) / 100)
            .max(1)
            .min(u64::from(u32::MAX)) as u32
    }
}

/// Admission decision for an order intent against a single venue's budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum BudgetDecision {
    Allowed,
    BlockedSoftCap { used_weight: u32, soft_limit: u32 },
    BlockedHardCap { used_weight: u32, max_weight: u32 },
    BlockedCooldown { remaining_ms: u64 },
    BlockedHalted { consecutive_rate_limits: u32 },
}

impl BudgetDecision {
    pub fn is_allowed(self) -> bool {
        matches!(self, Self::Allowed)
    }

    pub fn is_halted(self) -> bool {
        matches!(self, Self::BlockedHalted { .. })
    }
}

/// Sliding-window order budget and 429 rate-limit circuit breaker for one venue.
#[derive(Debug, Clone)]
pub struct VenueOrderBudget {
    config: OrderBudgetConfig,
    events: VecDeque<(u64, u32)>,
    window_weight: u32,
    last_now_ms: u64,
    cooldown_until_ms: u64,
    consecutive_rate_limits: u32,
    halted: bool,
}

impl VenueOrderBudget {
    /// Construct a validated per-venue order budget.
    pub fn new(config: OrderBudgetConfig) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            config,
            events: VecDeque::new(),
            window_weight: 0,
            last_now_ms: 0,
            cooldown_until_ms: 0,
            consecutive_rate_limits: 0,
            halted: false,
        })
    }

    fn clamp_and_prune(&mut self, now_ms: u64) -> u64 {
        let now = self.last_now_ms.max(now_ms);
        self.last_now_ms = now;
        while let Some(&(ts, w)) = self.events.front() {
            if ts.saturating_add(self.config.window_ms) <= now {
                self.events.pop_front();
                self.window_weight = self.window_weight.saturating_sub(w);
            } else {
                break;
            }
        }
        if self.events.is_empty() {
            self.window_weight = 0;
        }
        now
    }

    /// Admit or block an intent at `now_ms`.
    ///
    /// Critical safety invariant: `ReduceOnlyExit`, `EmergencyHedge`, and `Cancel` are
    /// NEVER blocked by soft cap, hard cap, 429 cooldown, or halt.
    pub fn admit(&mut self, now_ms: u64, class: OrderIntentClass, weight: u32) -> BudgetDecision {
        let now = self.clamp_and_prune(now_ms);
        if class != OrderIntentClass::RiskIncreasingEntry {
            return BudgetDecision::Allowed;
        }
        if self.halted {
            return BudgetDecision::BlockedHalted {
                consecutive_rate_limits: self.consecutive_rate_limits,
            };
        }
        if now < self.cooldown_until_ms {
            return BudgetDecision::BlockedCooldown {
                remaining_ms: self.cooldown_until_ms - now,
            };
        }
        let projected = self.window_weight.saturating_add(weight);
        if projected > self.config.max_weight_per_window {
            return BudgetDecision::BlockedHardCap {
                used_weight: self.window_weight,
                max_weight: self.config.max_weight_per_window,
            };
        }
        let soft_limit = self.config.entry_soft_limit();
        if projected > soft_limit {
            return BudgetDecision::BlockedSoftCap {
                used_weight: self.window_weight,
                soft_limit,
            };
        }
        BudgetDecision::Allowed
    }

    /// Record a dispatched order weight in the sliding window.
    pub fn record_dispatch(&mut self, now_ms: u64, weight: u32) {
        let now = self.clamp_and_prune(now_ms);
        if weight > 0 {
            self.events.push_back((now, weight));
            self.window_weight = self.window_weight.saturating_add(weight);
        }
    }

    /// Record a rate-limit (HTTP 429 / throttle) response, arming cooldown and escalating
    /// to sticky `halted` after `max_consecutive_rate_limits_before_halt`. Returns `true` if halted.
    pub fn record_rate_limit(&mut self, now_ms: u64) -> bool {
        let now = self.clamp_and_prune(now_ms);
        self.consecutive_rate_limits = self.consecutive_rate_limits.saturating_add(1);
        self.cooldown_until_ms = self
            .cooldown_until_ms
            .max(now.saturating_add(self.config.cooldown_ms_on_rate_limit));
        if self.consecutive_rate_limits >= self.config.max_consecutive_rate_limits_before_halt {
            self.halted = true;
        }
        self.halted
    }

    /// Reset the consecutive rate-limit streak after a clean non-rate-limited success.
    ///
    /// Once `halted` is triggered by sustained rate limits, both the sticky halt flag and its
    /// breach streak are preserved fail-closed.
    pub fn record_success(&mut self) {
        if !self.halted {
            self.consecutive_rate_limits = 0;
        }
    }

    pub fn is_halted(&self) -> bool {
        self.halted
    }

    pub fn used_weight(&mut self, now_ms: u64) -> u32 {
        self.clamp_and_prune(now_ms);
        self.window_weight
    }

    pub fn cooldown_remaining_ms(&mut self, now_ms: u64) -> u64 {
        let now = self.clamp_and_prune(now_ms);
        self.cooldown_until_ms.saturating_sub(now)
    }
}

/// Detect HTTP 429 / rate-limit / throttling messages case-insensitively without allocating.
pub fn looks_like_rate_limit_error(msg: &str) -> bool {
    const PATTERNS: [&[u8]; 5] = [
        b"429",
        b"rate limit",
        b"rate_limit",
        b"too many requests",
        b"throttl",
    ];
    let bytes = msg.as_bytes();
    PATTERNS.iter().any(|needle| {
        bytes
            .windows(needle.len())
            .any(|window| window.eq_ignore_ascii_case(needle))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_validation_rejects_zero_and_out_of_range_bounds() {
        let valid = OrderBudgetConfig::default();
        assert!(valid.validate().is_ok());
        assert_eq!(valid.entry_soft_limit(), 48);

        let mut cfg = valid;
        cfg.window_ms = 0;
        assert!(cfg.validate().is_err());

        cfg = valid;
        cfg.max_weight_per_window = 0;
        assert!(cfg.validate().is_err());

        cfg = valid;
        cfg.entry_soft_cap_pct = 0;
        assert!(cfg.validate().is_err());

        cfg = valid;
        cfg.entry_soft_cap_pct = 101;
        assert!(cfg.validate().is_err());

        cfg = valid;
        cfg.cooldown_ms_on_rate_limit = 0;
        assert!(cfg.validate().is_err());

        cfg = valid;
        cfg.max_consecutive_rate_limits_before_halt = 0;
        assert!(cfg.validate().is_err());

        // Small max_weight floors soft limit at 1.
        cfg = valid;
        cfg.max_weight_per_window = 5;
        cfg.entry_soft_cap_pct = 10;
        assert!(cfg.validate().is_ok());
        assert_eq!(cfg.entry_soft_limit(), 1);
        assert!(
            VenueOrderBudget::new(OrderBudgetConfig {
                window_ms: 0,
                ..valid
            })
            .is_err()
        );
    }

    #[test]
    fn decision_helpers_reflect_allowed_and_halted_variants() {
        assert!(BudgetDecision::Allowed.is_allowed());
        assert!(!BudgetDecision::Allowed.is_halted());

        let soft = BudgetDecision::BlockedSoftCap {
            used_weight: 8,
            soft_limit: 8,
        };
        assert!(!soft.is_allowed());
        assert!(!soft.is_halted());

        let hard = BudgetDecision::BlockedHardCap {
            used_weight: 10,
            max_weight: 10,
        };
        assert!(!hard.is_allowed());
        assert!(!hard.is_halted());

        let cd = BudgetDecision::BlockedCooldown { remaining_ms: 500 };
        assert!(!cd.is_allowed());
        assert!(!cd.is_halted());

        let halt = BudgetDecision::BlockedHalted {
            consecutive_rate_limits: 5,
        };
        assert!(!halt.is_allowed());
        assert!(halt.is_halted());
    }

    #[test]
    fn soft_cap_and_hard_cap_block_entries_while_exempting_risk_reducing_classes() {
        let cfg = OrderBudgetConfig {
            window_ms: 10_000,
            max_weight_per_window: 10,
            entry_soft_cap_pct: 80, // soft limit = 8
            cooldown_ms_on_rate_limit: 5_000,
            max_consecutive_rate_limits_before_halt: 5,
        };
        let mut budget = VenueOrderBudget::new(cfg).unwrap();

        for i in 0..8 {
            assert_eq!(
                budget.admit(1_000 + i * 100, OrderIntentClass::RiskIncreasingEntry, 1),
                BudgetDecision::Allowed
            );
            budget.record_dispatch(1_000 + i * 100, 1);
        }
        assert_eq!(budget.used_weight(1_800), 8);

        // 9th unit exceeds soft cap (8) -> entry blocked with BlockedSoftCap.
        assert_eq!(
            budget.admit(1_800, OrderIntentClass::RiskIncreasingEntry, 1),
            BudgetDecision::BlockedSoftCap {
                used_weight: 8,
                soft_limit: 8,
            }
        );

        // Risk-reducing classes remain strictly allowed and can consume through hard cap.
        for class in [
            OrderIntentClass::ReduceOnlyExit,
            OrderIntentClass::EmergencyHedge,
            OrderIntentClass::Cancel,
        ] {
            assert_eq!(budget.admit(1_800, class, 1), BudgetDecision::Allowed);
        }
        budget.record_dispatch(1_800, 2); // weight now at 10 (hard cap)
        budget.record_dispatch(1_850, 1); // weight now at 11 (> hard cap)
        assert_eq!(budget.used_weight(1_850), 11);

        // Entry now hits BlockedHardCap while exempt classes remain Allowed.
        assert_eq!(
            budget.admit(1_900, OrderIntentClass::RiskIncreasingEntry, 1),
            BudgetDecision::BlockedHardCap {
                used_weight: 11,
                max_weight: 10,
            }
        );
        for class in [
            OrderIntentClass::ReduceOnlyExit,
            OrderIntentClass::EmergencyHedge,
            OrderIntentClass::Cancel,
        ] {
            assert_eq!(budget.admit(1_900, class, 5), BudgetDecision::Allowed);
        }
    }

    #[test]
    fn sliding_window_recovers_after_window_ms_and_clamps_backward_clocks() {
        let cfg = OrderBudgetConfig {
            window_ms: 1_000,
            max_weight_per_window: 10,
            entry_soft_cap_pct: 50, // soft limit = 5
            cooldown_ms_on_rate_limit: 2_000,
            max_consecutive_rate_limits_before_halt: 3,
        };
        let mut budget = VenueOrderBudget::new(cfg).unwrap();

        budget.record_dispatch(10_000, 3);
        budget.record_dispatch(10_500, 2);
        budget.record_dispatch(10_600, 0); // zero weight ignored
        assert_eq!(budget.used_weight(10_600), 5);
        assert_eq!(
            budget.admit(10_600, OrderIntentClass::RiskIncreasingEntry, 1),
            BudgetDecision::BlockedSoftCap {
                used_weight: 5,
                soft_limit: 5,
            }
        );

        // Just before first event expires (10_999 < 10_000 + 1_000): still blocked.
        assert_eq!(budget.used_weight(10_999), 5);
        assert!(
            !budget
                .admit(10_999, OrderIntentClass::RiskIncreasingEntry, 1)
                .is_allowed()
        );

        // At 11_000: first event (ts=10_000, w=3) expires; remaining weight is 2.
        assert_eq!(budget.used_weight(11_000), 2);
        assert_eq!(
            budget.admit(11_000, OrderIntentClass::RiskIncreasingEntry, 3),
            BudgetDecision::Allowed
        );

        // Out-of-order earlier timestamp clamps to 11_000 and does not resurrect expired weight.
        assert_eq!(budget.used_weight(10_100), 2);

        // At 11_500: second event (ts=10_500, w=2) expires; window is completely clear.
        assert_eq!(budget.used_weight(11_500), 0);
    }

    #[test]
    fn rate_limit_cooldown_success_reset_and_sticky_halt_escalation() {
        let cfg = OrderBudgetConfig {
            window_ms: 60_000,
            max_weight_per_window: 60,
            entry_soft_cap_pct: 80,
            cooldown_ms_on_rate_limit: 15_000,
            max_consecutive_rate_limits_before_halt: 5,
        };
        let mut budget = VenueOrderBudget::new(cfg).unwrap();

        // 4 rate limits: cooldown active, not yet halted.
        for i in 0..4 {
            let t = 1_000 + i * 1_000;
            assert!(!budget.record_rate_limit(t));
            assert!(!budget.is_halted());
        }
        // Cooldown until 4_000 + 15_000 = 19_000.
        assert_eq!(budget.cooldown_remaining_ms(10_000), 9_000);
        assert_eq!(
            budget.admit(10_000, OrderIntentClass::RiskIncreasingEntry, 1),
            BudgetDecision::BlockedCooldown {
                remaining_ms: 9_000
            }
        );
        // Exempt intents remain allowed during cooldown.
        for class in [
            OrderIntentClass::ReduceOnlyExit,
            OrderIntentClass::EmergencyHedge,
            OrderIntentClass::Cancel,
        ] {
            assert_eq!(budget.admit(10_000, class, 1), BudgetDecision::Allowed);
        }

        // Cooldown expires at 19_000; record_success resets consecutive streak.
        assert_eq!(budget.cooldown_remaining_ms(19_000), 0);
        assert_eq!(
            budget.admit(19_000, OrderIntentClass::RiskIncreasingEntry, 1),
            BudgetDecision::Allowed
        );
        budget.record_success();

        // Another 4 rate limits do not halt because streak was reset; 5th halts.
        for i in 0..4 {
            assert!(!budget.record_rate_limit(20_000 + i * 100));
        }
        assert!(budget.record_rate_limit(20_500));
        assert!(budget.is_halted());

        // record_success does NOT clear sticky halt.
        budget.record_success();
        assert!(budget.is_halted());
        assert_eq!(
            budget.admit(100_000, OrderIntentClass::RiskIncreasingEntry, 1),
            BudgetDecision::BlockedHalted {
                consecutive_rate_limits: 5,
            }
        );
        assert!(
            budget
                .admit(100_000, OrderIntentClass::RiskIncreasingEntry, 1)
                .is_halted()
        );

        // Exempt risk-reducing intents remain strictly allowed even when halted.
        for class in [
            OrderIntentClass::ReduceOnlyExit,
            OrderIntentClass::EmergencyHedge,
            OrderIntentClass::Cancel,
        ] {
            assert_eq!(budget.admit(100_000, class, 1), BudgetDecision::Allowed);
        }
    }

    #[test]
    fn looks_like_rate_limit_error_matches_case_insensitively() {
        for msg in [
            "HTTP 429 Too Many Requests",
            "Arcus rejected /v1/scheduleCancel with HTTP 429: cancel schedule rejected",
            "Rate Limit exceeded on exchange endpoint",
            "ERR_RATE_LIMIT: slow down",
            "too many requests from IP",
            "API request Throttled by gateway",
        ] {
            assert!(looks_like_rate_limit_error(msg), "expected match for {msg}");
        }
        for msg in [
            "HTTP 503 Service Unavailable",
            "insufficient margin",
            "timeout waiting for gateway",
            "42",
            "utf8 café \u{1f600}",
        ] {
            assert!(
                !looks_like_rate_limit_error(msg),
                "unexpected match for {msg}"
            );
        }
    }
}
