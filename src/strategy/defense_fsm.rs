//! Deterministic 5-state event defense finite state machine (`OPT-07`).
//!
//! Zero-allocation, fail-closed defensive controller driven by [`DefenseTelemetrySnapshot`]:
//! - Immediate monotonic escalation on deterioration (feed staleness/gaps, volatility shock,
//!   trailing adverse markout, oracle divergence, daily loss utilization, budget cooldown/halt,
//!   or non-finite telemetry).
//! - Mandatory monotonic clock guard rejecting backward timestamps without allowing de-escalation.
//! - `min_dwell_ms` and recovery-hysteresis gating on de-escalation to prevent flapping.
//! - Non-auto-resetting `Halt` requiring explicit [`DefenseFsm::acknowledge_reconciled_reset`].
//! - Hard compile-time and runtime invariant: [`OrderIntentClass::ReduceOnlyExit`],
//!   [`OrderIntentClass::EmergencyHedge`], and [`OrderIntentClass::Cancel`] are ALWAYS permitted
//!   in every [`DefenseState`], including `ReduceOnly` and `Halt`.

use crate::order_budget::OrderIntentClass;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

/// Directional side for entry intent evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DefenseSide {
    Buy,
    Sell,
}

/// Which entry side(s) are blocked under [`DefenseState::SideDefense`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockedSide {
    Buy,
    Sell,
    Both,
}

impl BlockedSide {
    #[inline]
    pub const fn blocks_buy(self) -> bool {
        matches!(self, Self::Buy | Self::Both)
    }

    #[inline]
    pub const fn blocks_sell(self) -> bool {
        matches!(self, Self::Sell | Self::Both)
    }

    #[inline]
    pub const fn from_flags(block_buy: bool, block_sell: bool) -> Option<Self> {
        match (block_buy, block_sell) {
            (false, false) => None,
            (true, false) => Some(Self::Buy),
            (false, true) => Some(Self::Sell),
            (true, true) => Some(Self::Both),
        }
    }

    #[inline]
    pub const fn combine(self, other: Self) -> Self {
        match (self, other) {
            (Self::Both, _)
            | (_, Self::Both)
            | (Self::Buy, Self::Sell)
            | (Self::Sell, Self::Buy) => Self::Both,
            (Self::Buy, Self::Buy) => Self::Buy,
            (Self::Sell, Self::Sell) => Self::Sell,
        }
    }
}

/// Five-state deterministic defense regime (`OPT-07`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum DefenseState {
    #[default]
    Normal,
    Caution,
    SideDefense {
        blocked_side: BlockedSide,
    },
    ReduceOnly,
    Halt,
}

impl DefenseState {
    /// Monotonic severity rank (`0 = Normal` .. `4 = Halt`).
    #[inline]
    pub const fn severity(self) -> u8 {
        match self {
            Self::Normal => 0,
            Self::Caution => 1,
            Self::SideDefense { .. } => 2,
            Self::ReduceOnly => 3,
            Self::Halt => 4,
        }
    }

    #[inline]
    pub const fn is_halted(self) -> bool {
        matches!(self, Self::Halt)
    }

    #[inline]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Caution => "caution",
            Self::SideDefense {
                blocked_side: BlockedSide::Buy,
            } => "side_defense_buy",
            Self::SideDefense {
                blocked_side: BlockedSide::Sell,
            } => "side_defense_sell",
            Self::SideDefense {
                blocked_side: BlockedSide::Both,
            } => "side_defense_both",
            Self::ReduceOnly => "reduce_only",
            Self::Halt => "halt",
        }
    }

    /// Hard safety invariant: `ReduceOnlyExit`, `EmergencyHedge`, and `Cancel` are
    /// ALWAYS permitted in every state (including `ReduceOnly` and `Halt`).
    ///
    /// `RiskIncreasingEntry` is permitted in `Normal` and `Caution`, selectively blocked in
    /// `SideDefense` (failing closed if `side` is `None`), and strictly blocked in `ReduceOnly` / `Halt`.
    #[inline]
    pub const fn permits_intent(self, intent: OrderIntentClass, side: Option<DefenseSide>) -> bool {
        match intent {
            OrderIntentClass::ReduceOnlyExit
            | OrderIntentClass::EmergencyHedge
            | OrderIntentClass::Cancel => true,
            OrderIntentClass::RiskIncreasingEntry => match self {
                Self::Normal | Self::Caution => true,
                Self::SideDefense { blocked_side } => match side {
                    Some(DefenseSide::Buy) => !blocked_side.blocks_buy(),
                    Some(DefenseSide::Sell) => !blocked_side.blocks_sell(),
                    None => false,
                },
                Self::ReduceOnly | Self::Halt => false,
            },
        }
    }

    /// Entry notional scale factor in `[0.0, 1.0]` for `RiskIncreasingEntry`.
    #[inline]
    pub fn entry_size_scale(self, caution_size_scale: f64, side: Option<DefenseSide>) -> f64 {
        if !self.permits_intent(OrderIntentClass::RiskIncreasingEntry, side) {
            return 0.0;
        }
        match self {
            Self::Caution => caution_size_scale,
            Self::Normal | Self::SideDefense { .. } => 1.0,
            Self::ReduceOnly | Self::Halt => 0.0,
        }
    }

    /// Combine two defense states by taking the more restrictive state (or unioning blocked sides).
    #[inline]
    pub const fn combine(self, other: Self) -> Self {
        match (self, other) {
            (Self::SideDefense { blocked_side: a }, Self::SideDefense { blocked_side: b }) => {
                Self::SideDefense {
                    blocked_side: a.combine(b),
                }
            }
            _ if self.severity() >= other.severity() => self,
            _ => other,
        }
    }

    /// Returns `true` if `self` is strictly more restrictive than `current`.
    #[inline]
    pub const fn escalates_beyond(self, current: Self) -> bool {
        if self.severity() > current.severity() {
            return true;
        }
        matches!(
            (self, current),
            (
                Self::SideDefense { blocked_side: next },
                Self::SideDefense { blocked_side: cur },
            ) if (next.blocks_buy() && !cur.blocks_buy())
                || (next.blocks_sell() && !cur.blocks_sell())
        )
    }

    /// Returns `true` if `self` is strictly less restrictive than `current`.
    #[inline]
    pub const fn is_strictly_less_restrictive_than(self, current: Self) -> bool {
        if self.severity() < current.severity() {
            return true;
        }
        matches!(
            (self, current),
            (
                Self::SideDefense { blocked_side: next },
                Self::SideDefense { blocked_side: cur },
            ) if cur.blocks_buy() && cur.blocks_sell() && !(next.blocks_buy() && next.blocks_sell())
        )
    }
}

// Compile-time guarantees that risk-reducing and cancel intents are ALWAYS permitted
// across every DefenseState variant and every BlockedSide variant.
const _: () = {
    const STATES: [DefenseState; 7] = [
        DefenseState::Normal,
        DefenseState::Caution,
        DefenseState::SideDefense {
            blocked_side: BlockedSide::Buy,
        },
        DefenseState::SideDefense {
            blocked_side: BlockedSide::Sell,
        },
        DefenseState::SideDefense {
            blocked_side: BlockedSide::Both,
        },
        DefenseState::ReduceOnly,
        DefenseState::Halt,
    ];
    const EXEMPT: [OrderIntentClass; 3] = [
        OrderIntentClass::ReduceOnlyExit,
        OrderIntentClass::EmergencyHedge,
        OrderIntentClass::Cancel,
    ];
    let mut s_idx = 0;
    while s_idx < STATES.len() {
        let state = STATES[s_idx];
        let mut e_idx = 0;
        while e_idx < EXEMPT.len() {
            let intent = EXEMPT[e_idx];
            assert!(state.permits_intent(intent, None));
            assert!(state.permits_intent(intent, Some(DefenseSide::Buy)));
            assert!(state.permits_intent(intent, Some(DefenseSide::Sell)));
            e_idx += 1;
        }
        s_idx += 1;
    }
    assert!(!DefenseState::ReduceOnly.permits_intent(
        OrderIntentClass::RiskIncreasingEntry,
        Some(DefenseSide::Buy)
    ));
    assert!(!DefenseState::ReduceOnly.permits_intent(
        OrderIntentClass::RiskIncreasingEntry,
        Some(DefenseSide::Sell)
    ));
    assert!(!DefenseState::Halt.permits_intent(
        OrderIntentClass::RiskIncreasingEntry,
        Some(DefenseSide::Buy)
    ));
    assert!(!DefenseState::Halt.permits_intent(
        OrderIntentClass::RiskIncreasingEntry,
        Some(DefenseSide::Sell)
    ));
};

/// Structured telemetry snapshot driving [`DefenseFsm::evaluate`] (Copy, zero-alloc).
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct DefenseTelemetrySnapshot {
    /// Current evaluation clock (Unix ms); must advance monotonically.
    pub now_ms: u64,
    /// Age of the stalest active market or reference feed (ms).
    pub feed_staleness_ms: u64,
    /// Instantaneous inter-arrival / freeze gap observed on feed (ms).
    pub feed_gap_ms: u64,
    /// Short-horizon volatility divided by regime baseline (`1.0` = baseline).
    pub vol_shock_ratio: f64,
    /// Signed trailing bid markout EWMA (bps; negative = adverse on buys).
    pub trailing_bid_markout_bps: f64,
    /// Signed trailing ask markout EWMA (bps; negative = adverse on sells).
    pub trailing_ask_markout_bps: f64,
    /// Cross-venue / oracle divergence magnitude (bps).
    pub oracle_divergence_bps: f64,
    /// Daily loss utilization fraction (`0.0` = none used, `>= 1.0` = breached).
    pub daily_loss_utilization: f64,
    /// Whether per-venue order budget is currently in 429 cooldown.
    pub budget_cooldown_active: bool,
    /// Whether per-venue order budget has triggered sticky 429 halt.
    pub budget_halted: bool,
    /// Optional upstream flag indicating a feed clock regression occurred on this tick.
    pub clock_regressed_externally: bool,
}

/// Configuration thresholds and hysteresis parameters for [`DefenseFsm`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DefenseFsmConfig {
    pub min_dwell_ms: u64,
    pub caution_feed_staleness_ms: u64,
    pub reduce_only_feed_staleness_ms: u64,
    pub halt_feed_staleness_ms: u64,
    pub caution_feed_gap_ms: u64,
    pub reduce_only_feed_gap_ms: u64,
    pub halt_feed_gap_ms: u64,
    pub caution_vol_shock_ratio: f64,
    pub reduce_only_vol_shock_ratio: f64,
    pub halt_vol_shock_ratio: f64,
    pub side_defense_adverse_markout_bps: f64,
    pub reduce_only_adverse_markout_bps: f64,
    pub caution_oracle_divergence_bps: f64,
    pub reduce_only_oracle_divergence_bps: f64,
    pub halt_oracle_divergence_bps: f64,
    pub caution_daily_loss_utilization: f64,
    pub reduce_only_daily_loss_utilization: f64,
    pub halt_daily_loss_utilization: f64,
    pub caution_size_scale: f64,
    pub recovery_hysteresis_ratio: f64,
}

impl Default for DefenseFsmConfig {
    fn default() -> Self {
        Self {
            min_dwell_ms: 3_000,
            caution_feed_staleness_ms: 2_000,
            reduce_only_feed_staleness_ms: 4_000,
            halt_feed_staleness_ms: 10_000,
            caution_feed_gap_ms: 2_500,
            reduce_only_feed_gap_ms: 5_000,
            halt_feed_gap_ms: 15_000,
            caution_vol_shock_ratio: 2.0,
            reduce_only_vol_shock_ratio: 4.0,
            halt_vol_shock_ratio: 8.0,
            side_defense_adverse_markout_bps: 3.0,
            reduce_only_adverse_markout_bps: 8.0,
            caution_oracle_divergence_bps: 15.0,
            reduce_only_oracle_divergence_bps: 35.0,
            halt_oracle_divergence_bps: 75.0,
            caution_daily_loss_utilization: 0.50,
            reduce_only_daily_loss_utilization: 0.85,
            halt_daily_loss_utilization: 1.00,
            caution_size_scale: 0.50,
            recovery_hysteresis_ratio: 0.80,
        }
    }
}

impl DefenseFsmConfig {
    /// Validate monotonic threshold ordering and finite bounds.
    pub fn validate(&self) -> Result<()> {
        ensure!(self.min_dwell_ms > 0, "defense.min_dwell_ms must be > 0");
        ensure!(
            self.caution_feed_staleness_ms > 0
                && self.caution_feed_staleness_ms <= self.reduce_only_feed_staleness_ms
                && self.reduce_only_feed_staleness_ms <= self.halt_feed_staleness_ms,
            "defense feed_staleness_ms thresholds must satisfy 0 < caution <= reduce_only <= halt"
        );
        ensure!(
            self.caution_feed_gap_ms > 0
                && self.caution_feed_gap_ms <= self.reduce_only_feed_gap_ms
                && self.reduce_only_feed_gap_ms <= self.halt_feed_gap_ms,
            "defense feed_gap_ms thresholds must satisfy 0 < caution <= reduce_only <= halt"
        );
        ensure!(
            self.caution_vol_shock_ratio.is_finite()
                && self.reduce_only_vol_shock_ratio.is_finite()
                && self.halt_vol_shock_ratio.is_finite()
                && self.caution_vol_shock_ratio > 0.0
                && self.caution_vol_shock_ratio <= self.reduce_only_vol_shock_ratio
                && self.reduce_only_vol_shock_ratio <= self.halt_vol_shock_ratio,
            "defense vol_shock_ratio thresholds must be finite and satisfy 0 < caution <= reduce_only <= halt"
        );
        ensure!(
            self.side_defense_adverse_markout_bps.is_finite()
                && self.reduce_only_adverse_markout_bps.is_finite()
                && self.side_defense_adverse_markout_bps > 0.0
                && self.side_defense_adverse_markout_bps <= self.reduce_only_adverse_markout_bps,
            "defense adverse_markout_bps thresholds must be finite and satisfy 0 < side_defense <= reduce_only"
        );
        ensure!(
            self.caution_oracle_divergence_bps.is_finite()
                && self.reduce_only_oracle_divergence_bps.is_finite()
                && self.halt_oracle_divergence_bps.is_finite()
                && self.caution_oracle_divergence_bps > 0.0
                && self.caution_oracle_divergence_bps <= self.reduce_only_oracle_divergence_bps
                && self.reduce_only_oracle_divergence_bps <= self.halt_oracle_divergence_bps,
            "defense oracle_divergence_bps thresholds must be finite and satisfy 0 < caution <= reduce_only <= halt"
        );
        ensure!(
            self.caution_daily_loss_utilization.is_finite()
                && self.reduce_only_daily_loss_utilization.is_finite()
                && self.halt_daily_loss_utilization.is_finite()
                && self.caution_daily_loss_utilization > 0.0
                && self.caution_daily_loss_utilization <= self.reduce_only_daily_loss_utilization
                && self.reduce_only_daily_loss_utilization <= self.halt_daily_loss_utilization,
            "defense daily_loss_utilization thresholds must be finite and satisfy 0 < caution <= reduce_only <= halt"
        );
        ensure!(
            self.caution_size_scale.is_finite()
                && self.caution_size_scale > 0.0
                && self.caution_size_scale <= 1.0,
            "defense.caution_size_scale must be finite and in (0, 1]"
        );
        ensure!(
            self.recovery_hysteresis_ratio.is_finite()
                && self.recovery_hysteresis_ratio > 0.0
                && self.recovery_hysteresis_ratio <= 1.0,
            "defense.recovery_hysteresis_ratio must be finite and in (0, 1]"
        );
        Ok(())
    }
}

/// Result of one [`DefenseFsm::evaluate`] tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DefenseTransition {
    pub previous_state: DefenseState,
    pub current_state: DefenseState,
    pub transitioned: bool,
    pub clock_rejected: bool,
}

/// Reason an explicit [`DefenseFsm::acknowledge_reconciled_reset`] call was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResetBlockedReason {
    NotHalted,
    MonotonicClockViolation,
    DwellNotElapsed { remaining_ms: u64 },
    RecoverySignalsTooWeak { desired_state: DefenseState },
}

/// Zero-alloc deterministic 5-state defense FSM (`OPT-07`).
#[derive(Debug, Clone, Copy)]
pub struct DefenseFsm {
    config: DefenseFsmConfig,
    state: DefenseState,
    state_entered_at_ms: u64,
    last_now_ms: u64,
    has_seen_timestamp: bool,
    transitions: u64,
    clock_regressions: u64,
}

impl DefenseFsm {
    /// Construct a validated [`DefenseFsm`] starting in [`DefenseState::Normal`].
    pub fn new(config: DefenseFsmConfig) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            config,
            state: DefenseState::Normal,
            state_entered_at_ms: 0,
            last_now_ms: 0,
            has_seen_timestamp: false,
            transitions: 0,
            clock_regressions: 0,
        })
    }

    #[inline]
    pub const fn config(&self) -> &DefenseFsmConfig {
        &self.config
    }

    #[inline]
    pub const fn state(&self) -> DefenseState {
        self.state
    }

    #[inline]
    pub const fn transitions(&self) -> u64 {
        self.transitions
    }

    #[inline]
    pub const fn clock_regressions(&self) -> u64 {
        self.clock_regressions
    }

    #[inline]
    pub const fn last_now_ms(&self) -> Option<u64> {
        if self.has_seen_timestamp {
            Some(self.last_now_ms)
        } else {
            None
        }
    }

    #[inline]
    pub const fn state_entered_at_ms(&self) -> u64 {
        self.state_entered_at_ms
    }

    #[inline]
    pub const fn permits_intent(
        &self,
        intent: OrderIntentClass,
        side: Option<DefenseSide>,
    ) -> bool {
        self.state.permits_intent(intent, side)
    }

    #[inline]
    pub fn entry_size_scale(&self, side: Option<DefenseSide>) -> f64 {
        self.state
            .entry_size_scale(self.config.caution_size_scale, side)
    }

    #[inline]
    fn scaled_u64(threshold: u64, scale: f64) -> u64 {
        if scale >= 1.0 {
            threshold
        } else {
            ((threshold as f64) * scale).floor().max(1.0) as u64
        }
    }

    fn classify_state(&self, snapshot: &DefenseTelemetrySnapshot, scale: f64) -> DefenseState {
        let cfg = &self.config;
        // Non-finite or negative unsigned-domain telemetry fails closed to Halt.
        if !snapshot.vol_shock_ratio.is_finite()
            || snapshot.vol_shock_ratio < 0.0
            || !snapshot.oracle_divergence_bps.is_finite()
            || !snapshot.daily_loss_utilization.is_finite()
            || snapshot.daily_loss_utilization < 0.0
        {
            return DefenseState::Halt;
        }
        let oracle_div = snapshot.oracle_divergence_bps.abs();

        if snapshot.feed_staleness_ms >= Self::scaled_u64(cfg.halt_feed_staleness_ms, scale)
            || snapshot.feed_gap_ms >= Self::scaled_u64(cfg.halt_feed_gap_ms, scale)
            || snapshot.vol_shock_ratio >= cfg.halt_vol_shock_ratio * scale
            || oracle_div >= cfg.halt_oracle_divergence_bps * scale
            || snapshot.daily_loss_utilization >= cfg.halt_daily_loss_utilization * scale
        {
            return DefenseState::Halt;
        }

        if !snapshot.trailing_bid_markout_bps.is_finite()
            || !snapshot.trailing_ask_markout_bps.is_finite()
            || snapshot.clock_regressed_externally
            || snapshot.budget_halted
            || snapshot.feed_staleness_ms
                >= Self::scaled_u64(cfg.reduce_only_feed_staleness_ms, scale)
            || snapshot.feed_gap_ms >= Self::scaled_u64(cfg.reduce_only_feed_gap_ms, scale)
            || snapshot.vol_shock_ratio >= cfg.reduce_only_vol_shock_ratio * scale
            || oracle_div >= cfg.reduce_only_oracle_divergence_bps * scale
            || snapshot.daily_loss_utilization >= cfg.reduce_only_daily_loss_utilization * scale
            || snapshot.trailing_bid_markout_bps <= -cfg.reduce_only_adverse_markout_bps * scale
            || snapshot.trailing_ask_markout_bps <= -cfg.reduce_only_adverse_markout_bps * scale
        {
            return DefenseState::ReduceOnly;
        }

        let block_buy =
            snapshot.trailing_bid_markout_bps <= -cfg.side_defense_adverse_markout_bps * scale;
        let block_sell =
            snapshot.trailing_ask_markout_bps <= -cfg.side_defense_adverse_markout_bps * scale;
        if let Some(blocked_side) = BlockedSide::from_flags(block_buy, block_sell) {
            return DefenseState::SideDefense { blocked_side };
        }

        if snapshot.budget_cooldown_active
            || snapshot.feed_staleness_ms >= Self::scaled_u64(cfg.caution_feed_staleness_ms, scale)
            || snapshot.feed_gap_ms >= Self::scaled_u64(cfg.caution_feed_gap_ms, scale)
            || snapshot.vol_shock_ratio >= cfg.caution_vol_shock_ratio * scale
            || oracle_div >= cfg.caution_oracle_divergence_bps * scale
            || snapshot.daily_loss_utilization >= cfg.caution_daily_loss_utilization * scale
        {
            return DefenseState::Caution;
        }

        DefenseState::Normal
    }

    /// Evaluate `snapshot` deterministically with monotonic clock enforcement, immediate
    /// escalation, `min_dwell_ms` + recovery hysteresis on de-escalation, and sticky `Halt`.
    pub fn evaluate(&mut self, snapshot: &DefenseTelemetrySnapshot) -> DefenseTransition {
        let prev = self.state;

        // Mandatory monotonic clock guard: reject backward timestamps fail-closed and never de-escalate.
        if self.has_seen_timestamp && snapshot.now_ms < self.last_now_ms {
            self.clock_regressions = self.clock_regressions.saturating_add(1);
            let fail_closed = if prev == DefenseState::Halt {
                DefenseState::Halt
            } else {
                prev.combine(DefenseState::ReduceOnly)
            };
            let transitioned = fail_closed != prev;
            if transitioned {
                self.state = fail_closed;
                self.state_entered_at_ms = self.last_now_ms;
                self.transitions = self.transitions.saturating_add(1);
            }
            return DefenseTransition {
                previous_state: prev,
                current_state: self.state,
                transitioned,
                clock_rejected: true,
            };
        }

        if !self.has_seen_timestamp {
            self.has_seen_timestamp = true;
            self.state_entered_at_ms = snapshot.now_ms;
        }
        self.last_now_ms = snapshot.now_ms;

        let desired_escalated = self.classify_state(snapshot, 1.0);

        // Non-auto-resetting Halt: never clears inside `evaluate()` regardless of dwell or recovery.
        if prev == DefenseState::Halt {
            if desired_escalated == DefenseState::Halt {
                self.state_entered_at_ms = snapshot.now_ms;
            }
            return DefenseTransition {
                previous_state: prev,
                current_state: self.state,
                transitioned: false,
                clock_rejected: false,
            };
        }

        if desired_escalated.escalates_beyond(prev) {
            let next = prev.combine(desired_escalated);
            let transitioned = next != prev;
            self.state = next;
            self.state_entered_at_ms = snapshot.now_ms;
            if transitioned {
                self.transitions = self.transitions.saturating_add(1);
            }
            return DefenseTransition {
                previous_state: prev,
                current_state: self.state,
                transitioned,
                clock_rejected: false,
            };
        }

        if desired_escalated == prev {
            if self.state != DefenseState::Normal {
                // Refresh dwell anchor while stressor remains active at current severity.
                self.state_entered_at_ms = snapshot.now_ms;
            }
            return DefenseTransition {
                previous_state: prev,
                current_state: self.state,
                transitioned: false,
                clock_rejected: false,
            };
        }

        // Candidate de-escalation: enforce min_dwell_ms AND hysteresis margin.
        let dwell_elapsed = snapshot.now_ms.saturating_sub(self.state_entered_at_ms);
        if dwell_elapsed < self.config.min_dwell_ms {
            return DefenseTransition {
                previous_state: prev,
                current_state: self.state,
                transitioned: false,
                clock_rejected: false,
            };
        }

        let hysteresis_target =
            self.classify_state(snapshot, self.config.recovery_hysteresis_ratio);
        if hysteresis_target.is_strictly_less_restrictive_than(prev) {
            self.state = hysteresis_target;
            self.state_entered_at_ms = snapshot.now_ms;
            self.transitions = self.transitions.saturating_add(1);
            return DefenseTransition {
                previous_state: prev,
                current_state: self.state,
                transitioned: true,
                clock_rejected: false,
            };
        }

        DefenseTransition {
            previous_state: prev,
            current_state: self.state,
            transitioned: false,
            clock_rejected: false,
        }
    }

    /// Explicitly lift [`DefenseState::Halt`] back to [`DefenseState::Normal`] only after:
    /// 1. The FSM is currently in `Halt`,
    /// 2. `snapshot.now_ms` obeys the monotonic clock guard,
    /// 3. `min_dwell_ms` has elapsed since entering/refreshing `Halt`, and
    /// 4. Recovery signals are cleanly below the hysteresis-adjusted thresholds (`Normal`).
    pub fn acknowledge_reconciled_reset(
        &mut self,
        snapshot: &DefenseTelemetrySnapshot,
    ) -> std::result::Result<DefenseState, ResetBlockedReason> {
        if self.state != DefenseState::Halt {
            return Err(ResetBlockedReason::NotHalted);
        }
        if self.has_seen_timestamp && snapshot.now_ms < self.last_now_ms {
            self.clock_regressions = self.clock_regressions.saturating_add(1);
            return Err(ResetBlockedReason::MonotonicClockViolation);
        }
        let dwell_elapsed = snapshot.now_ms.saturating_sub(self.state_entered_at_ms);
        if dwell_elapsed < self.config.min_dwell_ms {
            return Err(ResetBlockedReason::DwellNotElapsed {
                remaining_ms: self.config.min_dwell_ms - dwell_elapsed,
            });
        }
        let hysteresis_target =
            self.classify_state(snapshot, self.config.recovery_hysteresis_ratio);
        if hysteresis_target != DefenseState::Normal {
            return Err(ResetBlockedReason::RecoverySignalsTooWeak {
                desired_state: hysteresis_target,
            });
        }
        self.has_seen_timestamp = true;
        self.last_now_ms = snapshot.now_ms;
        self.state = DefenseState::Normal;
        self.state_entered_at_ms = snapshot.now_ms;
        self.transitions = self.transitions.saturating_add(1);
        Ok(DefenseState::Normal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn healthy_snapshot(now_ms: u64) -> DefenseTelemetrySnapshot {
        DefenseTelemetrySnapshot {
            now_ms,
            feed_staleness_ms: 50,
            feed_gap_ms: 100,
            vol_shock_ratio: 1.0,
            trailing_bid_markout_bps: 0.2,
            trailing_ask_markout_bps: 0.2,
            oracle_divergence_bps: 1.0,
            daily_loss_utilization: 0.05,
            budget_cooldown_active: false,
            budget_halted: false,
            clock_regressed_externally: false,
        }
    }

    #[test]
    fn config_validation_rejects_inverted_and_out_of_range_thresholds() {
        assert!(DefenseFsmConfig::default().validate().is_ok());

        let mut bad = DefenseFsmConfig {
            min_dwell_ms: 0,
            ..DefenseFsmConfig::default()
        };
        assert!(bad.validate().is_err());

        bad = DefenseFsmConfig {
            caution_feed_staleness_ms: 5_000,
            reduce_only_feed_staleness_ms: 4_000,
            ..DefenseFsmConfig::default()
        };
        assert!(bad.validate().is_err());

        bad = DefenseFsmConfig {
            caution_vol_shock_ratio: f64::NAN,
            ..DefenseFsmConfig::default()
        };
        assert!(bad.validate().is_err());

        bad = DefenseFsmConfig {
            side_defense_adverse_markout_bps: 10.0,
            reduce_only_adverse_markout_bps: 8.0,
            ..DefenseFsmConfig::default()
        };
        assert!(bad.validate().is_err());

        bad = DefenseFsmConfig {
            recovery_hysteresis_ratio: 1.2,
            ..DefenseFsmConfig::default()
        };
        assert!(bad.validate().is_err());

        bad = DefenseFsmConfig {
            caution_size_scale: 0.0,
            ..DefenseFsmConfig::default()
        };
        assert!(bad.validate().is_err());
    }

    #[test]
    fn immediate_escalation_through_all_five_states_and_intent_exemptions() {
        let mut fsm = DefenseFsm::new(DefenseFsmConfig::default()).unwrap();
        assert_eq!(fsm.state(), DefenseState::Normal);
        assert!((fsm.entry_size_scale(Some(DefenseSide::Buy)) - 1.0).abs() < 1e-12);

        // Caution on vol shock >= 2.0 -> entry size scaled to 0.50.
        let mut snap = healthy_snapshot(1_000);
        snap.vol_shock_ratio = 2.2;
        let tr = fsm.evaluate(&snap);
        assert!(tr.transitioned);
        assert_eq!(fsm.state(), DefenseState::Caution);
        assert!((fsm.entry_size_scale(Some(DefenseSide::Buy)) - 0.50).abs() < 1e-12);

        // SideDefense on adverse bid markout <= -3.0 bps -> blocks Buy, permits Sell.
        snap = healthy_snapshot(1_200);
        snap.trailing_bid_markout_bps = -3.5;
        let tr = fsm.evaluate(&snap);
        assert!(tr.transitioned);
        assert_eq!(
            fsm.state(),
            DefenseState::SideDefense {
                blocked_side: BlockedSide::Buy
            }
        );
        assert!(!fsm.permits_intent(
            OrderIntentClass::RiskIncreasingEntry,
            Some(DefenseSide::Buy)
        ));
        assert!(fsm.permits_intent(
            OrderIntentClass::RiskIncreasingEntry,
            Some(DefenseSide::Sell)
        ));
        assert!(!fsm.permits_intent(OrderIntentClass::RiskIncreasingEntry, None));

        // Simultaneous or sequential ask adverse markout escalates SideDefense{Buy} -> SideDefense{Both}.
        snap.trailing_bid_markout_bps = 0.0;
        snap.trailing_ask_markout_bps = -4.0;
        let tr = fsm.evaluate(&snap);
        assert!(tr.transitioned);
        assert_eq!(
            fsm.state(),
            DefenseState::SideDefense {
                blocked_side: BlockedSide::Both
            }
        );
        assert!(!fsm.permits_intent(
            OrderIntentClass::RiskIncreasingEntry,
            Some(DefenseSide::Sell)
        ));

        // ReduceOnly on budget_halted or high oracle divergence.
        snap = healthy_snapshot(1_400);
        snap.budget_halted = true;
        let tr = fsm.evaluate(&snap);
        assert!(tr.transitioned);
        assert_eq!(fsm.state(), DefenseState::ReduceOnly);
        assert!(!fsm.permits_intent(
            OrderIntentClass::RiskIncreasingEntry,
            Some(DefenseSide::Buy)
        ));
        assert!(!fsm.permits_intent(
            OrderIntentClass::RiskIncreasingEntry,
            Some(DefenseSide::Sell)
        ));

        // Halt on daily_loss_utilization >= 1.0 or feed staleness >= 10s.
        snap = healthy_snapshot(1_600);
        snap.daily_loss_utilization = 1.0;
        let tr = fsm.evaluate(&snap);
        assert!(tr.transitioned);
        assert_eq!(fsm.state(), DefenseState::Halt);
        assert!(fsm.state().is_halted());

        // Hard invariant across EVERY state: ReduceOnlyExit, EmergencyHedge, Cancel ALWAYS allowed.
        for state in [
            DefenseState::Normal,
            DefenseState::Caution,
            DefenseState::SideDefense {
                blocked_side: BlockedSide::Buy,
            },
            DefenseState::SideDefense {
                blocked_side: BlockedSide::Sell,
            },
            DefenseState::SideDefense {
                blocked_side: BlockedSide::Both,
            },
            DefenseState::ReduceOnly,
            DefenseState::Halt,
        ] {
            for exempt in [
                OrderIntentClass::ReduceOnlyExit,
                OrderIntentClass::EmergencyHedge,
                OrderIntentClass::Cancel,
            ] {
                assert!(state.permits_intent(exempt, None));
                assert!(state.permits_intent(exempt, Some(DefenseSide::Buy)));
                assert!(state.permits_intent(exempt, Some(DefenseSide::Sell)));
            }
        }
    }

    #[test]
    fn min_dwell_and_hysteresis_prevent_flapping_on_weak_recovery() {
        let mut fsm = DefenseFsm::new(DefenseFsmConfig {
            min_dwell_ms: 3_000,
            caution_vol_shock_ratio: 2.0,
            recovery_hysteresis_ratio: 0.80, // must drop below 1.60 to leave Caution
            ..DefenseFsmConfig::default()
        })
        .unwrap();

        // Enter Caution at t=1_000.
        let mut snap = healthy_snapshot(1_000);
        snap.vol_shock_ratio = 2.1;
        assert_eq!(fsm.evaluate(&snap).current_state, DefenseState::Caution);

        // Strong recovery signal at t=3_999 (< 3_000ms dwell) -> stays Caution.
        let clean = healthy_snapshot(3_999);
        let tr = fsm.evaluate(&clean);
        assert!(!tr.transitioned);
        assert_eq!(tr.current_state, DefenseState::Caution);

        // Weak recovery signal at t=5_000 (vol_shock_ratio = 1.75 >= 1.60 hysteresis floor) -> stays Caution!
        let mut weak = healthy_snapshot(5_000);
        weak.vol_shock_ratio = 1.75;
        let tr = fsm.evaluate(&weak);
        assert!(!tr.transitioned);
        assert_eq!(tr.current_state, DefenseState::Caution);

        // Strong recovery signal below hysteresis floor at t=5_100 -> steps down to Normal.
        let strong = healthy_snapshot(5_100);
        let tr = fsm.evaluate(&strong);
        assert!(tr.transitioned);
        assert_eq!(tr.current_state, DefenseState::Normal);
        assert_eq!(fsm.transitions(), 2);
    }

    #[test]
    fn monotonic_clock_guard_rejects_backward_timestamps_fail_closed() {
        let mut fsm = DefenseFsm::new(DefenseFsmConfig::default()).unwrap();
        assert_eq!(
            fsm.evaluate(&healthy_snapshot(10_000)).current_state,
            DefenseState::Normal
        );

        // Backward clock at 9_500 -> rejected fail-closed to ReduceOnly, clock stays at 10_000.
        let tr = fsm.evaluate(&healthy_snapshot(9_500));
        assert!(tr.clock_rejected);
        assert!(tr.transitioned);
        assert_eq!(tr.current_state, DefenseState::ReduceOnly);
        assert_eq!(fsm.clock_regressions(), 1);
        assert_eq!(fsm.last_now_ms(), Some(10_000));
        // Exits and cancels remain permitted even after clock rejection.
        assert!(fsm.permits_intent(OrderIntentClass::ReduceOnlyExit, None));
        assert!(fsm.permits_intent(OrderIntentClass::Cancel, None));

        // Healthy tick at 12_999 (< 10_000 + 3_000 dwell) stays ReduceOnly.
        assert_eq!(
            fsm.evaluate(&healthy_snapshot(12_999)).current_state,
            DefenseState::ReduceOnly
        );
        // Healthy tick at 13_000 (>= 3_000ms dwell from 10_000) recovers to Normal.
        assert_eq!(
            fsm.evaluate(&healthy_snapshot(13_000)).current_state,
            DefenseState::Normal
        );
    }

    #[test]
    fn halt_never_auto_clears_and_requires_acknowledged_reconciled_recovery() {
        let mut fsm = DefenseFsm::new(DefenseFsmConfig::default()).unwrap();

        // Not halted -> reset rejected with NotHalted.
        assert_eq!(
            fsm.acknowledge_reconciled_reset(&healthy_snapshot(1_000)),
            Err(ResetBlockedReason::NotHalted)
        );

        // Trigger Halt via feed staleness >= 10_000ms at t=2_000.
        let mut bad = healthy_snapshot(2_000);
        bad.feed_staleness_ms = 12_000;
        assert_eq!(fsm.evaluate(&bad).current_state, DefenseState::Halt);

        // Even 100s later with pristine healthy telemetry, evaluate() MUST NOT auto-reset Halt.
        for dt in [3_000, 10_000, 102_000] {
            let tr = fsm.evaluate(&healthy_snapshot(dt));
            assert!(!tr.transitioned);
            assert_eq!(tr.current_state, DefenseState::Halt);
        }

        // Backward clock reset attempt is rejected.
        assert_eq!(
            fsm.acknowledge_reconciled_reset(&healthy_snapshot(101_999)),
            Err(ResetBlockedReason::MonotonicClockViolation)
        );
        assert_eq!(fsm.state(), DefenseState::Halt);

        // Re-trigger Halt stress at t=103_000 so dwell timer resets to 103_000.
        bad.now_ms = 103_000;
        assert_eq!(fsm.evaluate(&bad).current_state, DefenseState::Halt);

        // Reset attempt before min_dwell_ms (3_000ms) is rejected.
        assert_eq!(
            fsm.acknowledge_reconciled_reset(&healthy_snapshot(105_000)),
            Err(ResetBlockedReason::DwellNotElapsed {
                remaining_ms: 1_000
            })
        );
        assert_eq!(fsm.state(), DefenseState::Halt);

        // Reset attempt with weak recovery signal (below raw caution 2.0, above 1.60 hysteresis) is rejected!
        let mut weak = healthy_snapshot(107_000);
        weak.vol_shock_ratio = 1.75;
        assert_eq!(
            fsm.acknowledge_reconciled_reset(&weak),
            Err(ResetBlockedReason::RecoverySignalsTooWeak {
                desired_state: DefenseState::Caution
            })
        );
        assert_eq!(fsm.state(), DefenseState::Halt);

        // Valid reconciled reset after dwell + clean recovery lifts Halt -> Normal.
        assert_eq!(
            fsm.acknowledge_reconciled_reset(&healthy_snapshot(107_500)),
            Ok(DefenseState::Normal)
        );
        assert_eq!(fsm.state(), DefenseState::Normal);
    }

    #[test]
    fn non_finite_telemetry_fails_closed() {
        let mut fsm = DefenseFsm::new(DefenseFsmConfig::default()).unwrap();
        let mut nan_vol = healthy_snapshot(1_000);
        nan_vol.vol_shock_ratio = f64::NAN;
        assert_eq!(fsm.evaluate(&nan_vol).current_state, DefenseState::Halt);
    }
}
