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

impl OrderIntentClass {
    /// Returns `true` for risk-reducing / cancel classes that are strictly exempt
    /// from soft caps, hard caps, rate-limit cooldowns, and halts.
    #[inline]
    pub const fn is_exempt(self) -> bool {
        !matches!(self, Self::RiskIncreasingEntry)
    }
}

/// How a multi-transaction batch (`SendTxBatch`) accounts quota weight (`OPT-05`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BatchQuotaMode {
    /// Legacy flat-weight accounting: one non-empty batch consumes 1 quota unit total.
    #[default]
    SingleUnit,
    /// Lighter `SendTxBatch` semantics: each admitted transaction consumes 1 quota unit.
    PerTransaction,
}

/// Per-batch admission breakdown returned by [`VenueOrderBudget::admit_batch`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BatchBudgetDecision {
    pub mode: BatchQuotaMode,
    /// Per-intent decision aligned 1:1 with the input `&[OrderIntentClass]` slice.
    pub item_decisions: Vec<BudgetDecision>,
    pub admitted_count: usize,
    pub exempt_admitted: usize,
    pub entry_admitted: usize,
    pub entry_blocked: usize,
    /// Exact weight to pass to [`VenueOrderBudget::record_dispatch`] when sending the admitted items.
    pub admitted_weight: u32,
}

impl BatchBudgetDecision {
    pub fn is_allowed(&self, idx: usize) -> bool {
        self.item_decisions.get(idx).is_some_and(|d| d.is_allowed())
    }

    pub fn all_allowed(&self) -> bool {
        self.entry_blocked == 0
    }

    pub fn any_allowed(&self) -> bool {
        self.admitted_count > 0
    }
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
    batch_mode: BatchQuotaMode,
    events: VecDeque<(u64, u32)>,
    window_weight: u32,
    last_now_ms: u64,
    cooldown_until_ms: u64,
    consecutive_rate_limits: u32,
    halted: bool,
}

impl VenueOrderBudget {
    /// Construct a validated per-venue order budget (defaults to [`BatchQuotaMode::SingleUnit`]).
    pub fn new(config: OrderBudgetConfig) -> Result<Self> {
        Self::with_batch_mode(config, BatchQuotaMode::SingleUnit)
    }

    /// Construct a validated per-venue order budget with an explicit [`BatchQuotaMode`].
    pub fn with_batch_mode(config: OrderBudgetConfig, batch_mode: BatchQuotaMode) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            config,
            batch_mode,
            events: VecDeque::new(),
            window_weight: 0,
            last_now_ms: 0,
            cooldown_until_ms: 0,
            consecutive_rate_limits: 0,
            halted: false,
        })
    }

    pub fn batch_mode(&self) -> BatchQuotaMode {
        self.batch_mode
    }

    pub fn set_batch_mode(&mut self, batch_mode: BatchQuotaMode) {
        self.batch_mode = batch_mode;
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

    fn check_entry_at(&self, now: u64, baseline_weight: u32, add_weight: u32) -> BudgetDecision {
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
        let projected = baseline_weight.saturating_add(add_weight);
        if projected > self.config.max_weight_per_window {
            return BudgetDecision::BlockedHardCap {
                used_weight: baseline_weight,
                max_weight: self.config.max_weight_per_window,
            };
        }
        let soft_limit = self.config.entry_soft_limit();
        if projected > soft_limit {
            return BudgetDecision::BlockedSoftCap {
                used_weight: baseline_weight,
                soft_limit,
            };
        }
        BudgetDecision::Allowed
    }

    /// Admit or block an intent at `now_ms`.
    ///
    /// Critical safety invariant: `ReduceOnlyExit`, `EmergencyHedge`, and `Cancel` are
    /// NEVER blocked by soft cap, hard cap, 429 cooldown, or halt.
    pub fn admit(&mut self, now_ms: u64, class: OrderIntentClass, weight: u32) -> BudgetDecision {
        let now = self.clamp_and_prune(now_ms);
        if class.is_exempt() {
            return BudgetDecision::Allowed;
        }
        self.check_entry_at(now, self.window_weight, weight)
    }

    /// Admit or partially filter a batch of intents using the budget's configured [`BatchQuotaMode`].
    ///
    /// Critical safety invariant: `ReduceOnlyExit`, `EmergencyHedge`, and `Cancel` items in a batch
    /// are ALWAYS admitted first and NEVER blocked or starved by `RiskIncreasingEntry` items in the
    /// same batch (regardless of slice ordering). Under [`BatchQuotaMode::PerTransaction`], each
    /// admitted transaction (exempt or entry) consumes 1 weight unit (`SendTxBatch` semantics), so
    /// exempt items reserve their tx weight before entry headroom is allocated.
    pub fn admit_batch(
        &mut self,
        now_ms: u64,
        intents: &[OrderIntentClass],
    ) -> BatchBudgetDecision {
        self.admit_batch_with_mode(now_ms, intents, self.batch_mode)
    }

    /// Admit or partially filter a batch of intents using an explicit [`BatchQuotaMode`].
    pub fn admit_batch_with_mode(
        &mut self,
        now_ms: u64,
        intents: &[OrderIntentClass],
        mode: BatchQuotaMode,
    ) -> BatchBudgetDecision {
        let now = self.clamp_and_prune(now_ms);
        if intents.is_empty() {
            return BatchBudgetDecision {
                mode,
                item_decisions: Vec::new(),
                admitted_count: 0,
                exempt_admitted: 0,
                entry_admitted: 0,
                entry_blocked: 0,
                admitted_weight: 0,
            };
        }

        let exempt_count = intents.iter().filter(|c| c.is_exempt()).count();
        let mut item_decisions = vec![BudgetDecision::Allowed; intents.len()];
        let mut entry_admitted = 0_usize;
        let mut entry_blocked = 0_usize;

        match mode {
            BatchQuotaMode::SingleUnit => {
                let entry_decision = self.check_entry_at(now, self.window_weight, 1);
                for (idx, class) in intents.iter().enumerate() {
                    if class.is_exempt() {
                        item_decisions[idx] = BudgetDecision::Allowed;
                    } else if entry_decision.is_allowed() {
                        item_decisions[idx] = BudgetDecision::Allowed;
                        entry_admitted += 1;
                    } else {
                        item_decisions[idx] = entry_decision;
                        entry_blocked += 1;
                    }
                }
                let admitted_count = exempt_count + entry_admitted;
                BatchBudgetDecision {
                    mode,
                    item_decisions,
                    admitted_count,
                    exempt_admitted: exempt_count,
                    entry_admitted,
                    entry_blocked,
                    admitted_weight: u32::from(admitted_count > 0),
                }
            }
            BatchQuotaMode::PerTransaction => {
                // Reserve 1 weight unit per exempt item first so entries cannot push total
                // batch dispatch over soft/hard caps and entries can never starve exits/cancels.
                let exempt_weight = u32::try_from(exempt_count).unwrap_or(u32::MAX);
                let mut running_weight = self.window_weight.saturating_add(exempt_weight);

                for (idx, class) in intents.iter().enumerate() {
                    if class.is_exempt() {
                        item_decisions[idx] = BudgetDecision::Allowed;
                        continue;
                    }
                    let decision = self.check_entry_at(now, running_weight, 1);
                    if decision.is_allowed() {
                        item_decisions[idx] = BudgetDecision::Allowed;
                        running_weight = running_weight.saturating_add(1);
                        entry_admitted += 1;
                    } else {
                        item_decisions[idx] = decision;
                        entry_blocked += 1;
                    }
                }
                let admitted_count = exempt_count + entry_admitted;
                let admitted_weight = u32::try_from(admitted_count).unwrap_or(u32::MAX);
                BatchBudgetDecision {
                    mode,
                    item_decisions,
                    admitted_count,
                    exempt_admitted: exempt_count,
                    entry_admitted,
                    entry_blocked,
                    admitted_weight,
                }
            }
        }
    }

    /// Record a dispatched order weight in the sliding window.
    pub fn record_dispatch(&mut self, now_ms: u64, weight: u32) {
        let now = self.clamp_and_prune(now_ms);
        if weight > 0 {
            self.events.push_back((now, weight));
            self.window_weight = self.window_weight.saturating_add(weight);
        }
    }

    /// Convenience helper to record `decision.admitted_weight` from [`Self::admit_batch`].
    pub fn record_batch_dispatch(&mut self, now_ms: u64, decision: &BatchBudgetDecision) {
        self.record_dispatch(now_ms, decision.admitted_weight);
    }

    /// Remaining weight headroom before hitting the entry soft cap (0 if halted or in cooldown).
    pub fn remaining_entry_headroom(&mut self, now_ms: u64) -> u32 {
        let now = self.clamp_and_prune(now_ms);
        if self.halted || now < self.cooldown_until_ms {
            return 0;
        }
        self.config
            .entry_soft_limit()
            .saturating_sub(self.window_weight)
    }

    /// Current sliding-window weight utilization as a percentage of `max_weight_per_window`.
    pub fn utilization_pct(&mut self, now_ms: u64) -> f64 {
        self.clamp_and_prune(now_ms);
        (f64::from(self.window_weight) / f64::from(self.config.max_weight_per_window)) * 100.0
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

    #[test]
    fn batch_single_unit_vs_per_transaction_accounts_weight_and_exempts_risk_reducing() {
        let cfg = OrderBudgetConfig {
            window_ms: 10_000,
            max_weight_per_window: 10,
            entry_soft_cap_pct: 80, // soft_limit = 8
            cooldown_ms_on_rate_limit: 5_000,
            max_consecutive_rate_limits_before_halt: 5,
        };

        // SingleUnit mode: 4 entries in one batch consume 1 unit total.
        let mut single = VenueOrderBudget::new(cfg).unwrap();
        assert_eq!(single.batch_mode(), BatchQuotaMode::SingleUnit);
        assert_eq!(single.remaining_entry_headroom(1_000), 8);
        assert!((single.utilization_pct(1_000) - 0.0).abs() < 1e-12);

        let batch = [
            OrderIntentClass::RiskIncreasingEntry,
            OrderIntentClass::RiskIncreasingEntry,
            OrderIntentClass::Cancel,
            OrderIntentClass::ReduceOnlyExit,
        ];
        let dec = single.admit_batch(1_000, &batch);
        assert_eq!(dec.mode, BatchQuotaMode::SingleUnit);
        assert!(dec.all_allowed());
        assert_eq!(dec.admitted_count, 4);
        assert_eq!(dec.exempt_admitted, 2);
        assert_eq!(dec.entry_admitted, 2);
        assert_eq!(dec.entry_blocked, 0);
        assert_eq!(dec.admitted_weight, 1);
        single.record_batch_dispatch(1_000, &dec);
        assert_eq!(single.used_weight(1_000), 1);
        assert_eq!(single.remaining_entry_headroom(1_000), 7);
        assert!((single.utilization_pct(1_000) - 10.0).abs() < 1e-12);

        // PerTransaction (Lighter SendTxBatch) mode: 1 weight unit per tx.
        let mut per_tx =
            VenueOrderBudget::with_batch_mode(cfg, BatchQuotaMode::PerTransaction).unwrap();
        assert_eq!(per_tx.batch_mode(), BatchQuotaMode::PerTransaction);

        // Seed 5 units -> soft headroom = 3.
        per_tx.record_dispatch(1_000, 5);
        assert_eq!(per_tx.remaining_entry_headroom(1_000), 3);
        assert!((per_tx.utilization_pct(1_000) - 50.0).abs() < 1e-12);

        // Mixed batch with entries BEFORE exempt items:
        // Exempt items (ReduceOnlyExit + Cancel = 2 units) reserve first -> 5 + 2 = 7 used,
        // leaving exactly 1 entry slot before soft_limit=8.
        // Thus entry[0] is Allowed (7->8), entry[1] is BlockedSoftCap, and BOTH exempt items
        // remain strictly Allowed (never starved by earlier entries!).
        let mixed = [
            OrderIntentClass::RiskIncreasingEntry,
            OrderIntentClass::RiskIncreasingEntry,
            OrderIntentClass::ReduceOnlyExit,
            OrderIntentClass::Cancel,
            OrderIntentClass::EmergencyHedge,
        ];
        // Wait: 3 exempt items -> 5 + 3 = 8 == soft_limit -> 0 entries allowed, all 3 exempt allowed!
        let dec_3_exempt = per_tx.admit_batch(1_100, &mixed);
        assert_eq!(dec_3_exempt.exempt_admitted, 3);
        assert_eq!(dec_3_exempt.entry_admitted, 0);
        assert_eq!(dec_3_exempt.entry_blocked, 2);
        assert_eq!(dec_3_exempt.admitted_weight, 3);
        assert!(!dec_3_exempt.is_allowed(0));
        assert!(!dec_3_exempt.is_allowed(1));
        assert!(dec_3_exempt.is_allowed(2));
        assert!(dec_3_exempt.is_allowed(3));
        assert!(dec_3_exempt.is_allowed(4));

        // Now with 1 exempt item (Cancel): 5 + 1 = 6 -> 2 entries fit (6->7->8), 3rd blocked!
        let partial_batch = [
            OrderIntentClass::RiskIncreasingEntry,
            OrderIntentClass::RiskIncreasingEntry,
            OrderIntentClass::RiskIncreasingEntry,
            OrderIntentClass::Cancel,
        ];
        let dec_partial = per_tx.admit_batch(1_200, &partial_batch);
        assert_eq!(dec_partial.exempt_admitted, 1);
        assert_eq!(dec_partial.entry_admitted, 2);
        assert_eq!(dec_partial.entry_blocked, 1);
        assert_eq!(dec_partial.admitted_weight, 3);
        assert!(dec_partial.is_allowed(0));
        assert!(dec_partial.is_allowed(1));
        assert!(!dec_partial.is_allowed(2));
        assert!(dec_partial.is_allowed(3));
        per_tx.record_batch_dispatch(1_200, &dec_partial);
        assert_eq!(per_tx.used_weight(1_200), 8);
        assert_eq!(per_tx.remaining_entry_headroom(1_200), 0);
        assert!((per_tx.utilization_pct(1_200) - 80.0).abs() < 1e-12);
    }

    #[test]
    fn batch_cooldown_and_halt_block_entries_while_admitting_all_exempt_items() {
        let cfg = OrderBudgetConfig {
            window_ms: 10_000,
            max_weight_per_window: 10,
            entry_soft_cap_pct: 80,
            cooldown_ms_on_rate_limit: 5_000,
            max_consecutive_rate_limits_before_halt: 2,
        };
        let mut budget =
            VenueOrderBudget::with_batch_mode(cfg, BatchQuotaMode::PerTransaction).unwrap();
        let batch = [
            OrderIntentClass::RiskIncreasingEntry,
            OrderIntentClass::ReduceOnlyExit,
            OrderIntentClass::EmergencyHedge,
            OrderIntentClass::Cancel,
        ];

        // 1st rate limit -> cooldown active -> headroom == 0, entries blocked, exempt items admitted.
        assert!(!budget.record_rate_limit(1_000));
        assert_eq!(budget.remaining_entry_headroom(1_500), 0);
        let cd = budget.admit_batch(1_500, &batch);
        assert_eq!(cd.entry_admitted, 0);
        assert_eq!(cd.entry_blocked, 1);
        assert_eq!(cd.exempt_admitted, 3);
        assert_eq!(cd.admitted_weight, 3);
        assert!(matches!(
            cd.item_decisions[0],
            BudgetDecision::BlockedCooldown {
                remaining_ms: 4_500
            }
        ));
        assert!(cd.is_allowed(1) && cd.is_allowed(2) && cd.is_allowed(3));

        // 2nd rate limit -> halted -> headroom == 0, entries blocked, exempt items admitted even past hard cap.
        assert!(budget.record_rate_limit(2_000));
        budget.record_dispatch(2_000, 12); // 120% utilization
        assert!((budget.utilization_pct(2_000) - 120.0).abs() < 1e-12);
        assert_eq!(budget.remaining_entry_headroom(2_000), 0);
        let halted = budget.admit_batch(2_000, &batch);
        assert_eq!(halted.entry_admitted, 0);
        assert_eq!(halted.entry_blocked, 1);
        assert_eq!(halted.exempt_admitted, 3);
        assert_eq!(halted.admitted_weight, 3);
        assert!(halted.item_decisions[0].is_halted());
        assert!(halted.is_allowed(1) && halted.is_allowed(2) && halted.is_allowed(3));

        // Empty batch returns zero admitted weight.
        let empty = budget.admit_batch(2_000, &[]);
        assert_eq!(empty.admitted_count, 0);
        assert_eq!(empty.admitted_weight, 0);
        assert!(empty.all_allowed());
        assert!(!empty.any_allowed());
    }
}
