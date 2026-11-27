//! Track 02 v2 (`02_hedge_arbitrage`): Joint Multi-Hour Funding Carry + Live Executable Basis-Timing.
//!
//! Pure decision and execution-state logic with ZERO `crate::exchanges::*` imports and ZERO
//! hot-path heap allocations. Combines causal EWMA cross-venue funding differentials (`bps/hr`)
//! over horizon `H` with live executable L1 basis Z-scores (`short_leg_bid - long_leg_ask`)
//! while enforcing:
//! - Strict monotonic clock, staleness, gap, and NaN/Inf fail-closed guards;
//! - Joint carry + basis-timing hurdle vs round-trip fees, live L1 spreads, and FX drag;
//! - Explicit USDG/USDC FX drag bound and per-pair notional risk cap;
//! - `PassiveFirstCarryExecState` (`Idle -> WorkingPassiveLeg -> HedgingTakerLeg -> ActiveCarry -> Unwinding -> FailClosedHalt`);
//! - Basis-convergence early take-profit, adverse funding flip exit, and basis stop-loss.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

const BPS_SCALE: f64 = 10_000.0;

/// Top-of-book L1 snapshot with displayed sizes and local monotonic receipt timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
pub struct CarryLegTouch {
    pub bid: f64,
    pub ask: f64,
    pub bid_sz: f64,
    pub ask_sz: f64,
    pub recv_ms: u64,
}

impl CarryLegTouch {
    /// Returns `true` when prices and sizes are finite, positive, and uncrossed.
    pub fn is_valid(&self) -> bool {
        self.bid.is_finite()
            && self.ask.is_finite()
            && self.bid_sz.is_finite()
            && self.ask_sz.is_finite()
            && self.bid > 0.0
            && self.ask > self.bid
            && self.bid_sz >= 0.0
            && self.ask_sz >= 0.0
            && self.recv_ms > 0
    }

    pub fn mid(&self) -> f64 {
        0.5 * (self.bid + self.ask)
    }

    pub fn spread_bps(&self) -> f64 {
        let m = self.mid();
        if m > 0.0 && m.is_finite() {
            ((self.ask - self.bid) / m) * BPS_SCALE
        } else {
            f64::NAN
        }
    }

    pub fn bid_notional_usd(&self) -> f64 {
        self.bid * self.bid_sz
    }

    pub fn ask_notional_usd(&self) -> f64 {
        self.ask * self.ask_sz
    }
}

/// Hourly funding rate sample in `bps/hr` (positive = longs pay shorts).
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
pub struct FundingObservation {
    pub rate_bps_hr: f64,
    pub observed_ms: u64,
}

impl FundingObservation {
    pub fn is_valid(&self) -> bool {
        self.rate_bps_hr.is_finite() && self.observed_ms > 0
    }
}

/// Settlement currency cohort for explicit USDG/USDC FX drag segregation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PairSettlementKind {
    SameStablecoin,
    CrossStablecoinUsdgUsdc,
}

/// Per-pair fee, FX drag, and notional risk cap specification.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CarryPairEconomics {
    pub leg_a_maker_fee_bps: f64,
    pub leg_a_taker_fee_bps: f64,
    pub leg_b_maker_fee_bps: f64,
    pub leg_b_taker_fee_bps: f64,
    pub settlement_kind: PairSettlementKind,
    /// Round-trip USDG/USDC conversion/basis drag in bps (used only when `CrossStablecoinUsdgUsdc`).
    pub fx_drag_bps: f64,
    /// Hard fail-closed ceiling on `fx_drag_bps` for cross-stablecoin pairs.
    pub max_fx_drag_bps: f64,
    /// Maximum gross leg notional permitted for this pair in USD.
    pub max_pair_notional_usd: f64,
}

impl Default for CarryPairEconomics {
    fn default() -> Self {
        Self {
            leg_a_maker_fee_bps: 0.0,
            leg_a_taker_fee_bps: 0.0,
            leg_b_maker_fee_bps: 0.0,
            leg_b_taker_fee_bps: 0.0,
            settlement_kind: PairSettlementKind::SameStablecoin,
            fx_drag_bps: 0.0,
            max_fx_drag_bps: 5.0,
            max_pair_notional_usd: 1_000.0,
        }
    }
}

impl CarryPairEconomics {
    pub fn validate(&self) -> Result<()> {
        for (name, val) in [
            ("leg_a_maker_fee_bps", self.leg_a_maker_fee_bps),
            ("leg_a_taker_fee_bps", self.leg_a_taker_fee_bps),
            ("leg_b_maker_fee_bps", self.leg_b_maker_fee_bps),
            ("leg_b_taker_fee_bps", self.leg_b_taker_fee_bps),
            ("fx_drag_bps", self.fx_drag_bps),
            ("max_fx_drag_bps", self.max_fx_drag_bps),
            ("max_pair_notional_usd", self.max_pair_notional_usd),
        ] {
            ensure!(val.is_finite(), "{name} must be finite");
        }
        ensure!(
            self.leg_a_taker_fee_bps >= 0.0
                && self.leg_b_taker_fee_bps >= 0.0
                && self.fx_drag_bps >= 0.0
                && self.max_fx_drag_bps >= 0.0
                && self.max_pair_notional_usd > 0.0,
            "taker fees, fx drag must be >= 0 and max_pair_notional_usd > 0"
        );
        Ok(())
    }

    pub fn effective_fx_drag_bps(&self) -> f64 {
        match self.settlement_kind {
            PairSettlementKind::SameStablecoin => 0.0,
            PairSettlementKind::CrossStablecoinUsdgUsdc => self.fx_drag_bps,
        }
    }

    pub fn exceeds_fx_bound(&self) -> bool {
        self.settlement_kind == PairSettlementKind::CrossStablecoinUsdgUsdc
            && self.fx_drag_bps > self.max_fx_drag_bps
    }

    /// Round-trip fee in bps when both entry and exit cross the spread as taker.
    pub fn rt_taker_fee_bps(&self) -> f64 {
        2.0 * (self.leg_a_taker_fee_bps + self.leg_b_taker_fee_bps)
    }

    /// Round-trip fee in bps when entry rests passive on one leg + takers the hedge, and exit takes both.
    pub fn rt_passive_first_fee_bps(&self, passive_is_a: bool) -> f64 {
        let entry = if passive_is_a {
            self.leg_a_maker_fee_bps + self.leg_b_taker_fee_bps
        } else {
            self.leg_b_maker_fee_bps + self.leg_a_taker_fee_bps
        };
        let exit = self.leg_a_taker_fee_bps + self.leg_b_taker_fee_bps;
        entry + exit
    }
}

/// Direction of the delta-neutral carry position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CarryDirection {
    /// Buy Leg A, Sell Leg B (earns `funding_B - funding_A`).
    LongAShortB,
    /// Sell Leg A, Buy Leg B (earns `funding_A - funding_B`).
    ShortALongB,
}

impl CarryDirection {
    pub fn sign_short_a(self) -> f64 {
        match self {
            Self::ShortALongB => 1.0,
            Self::LongAShortB => -1.0,
        }
    }
}

/// Reason an active carry position initiated unwind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CarryExitReason {
    BasisConvergenceTakeProfit,
    AdverseFundingFlip,
    BasisStopLoss,
    MaxHoldExpired,
    WindowDrain,
}

/// Reason the controller latched `FailClosedHalt`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CarryHaltReason {
    NonMonotonicClock,
    InvalidInput,
    StaleBook,
    StaleFunding,
    FundingGap,
    BookGap,
    PassiveTimeout,
    PassiveHazardEscalation,
    FxDragBoundExceeded,
}

/// Execution state machine for passive-first funding + basis carry.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PassiveFirstCarryExecState {
    Idle,
    WorkingPassiveLeg {
        dir: CarryDirection,
        passive_is_a: bool,
        posted_at_ms: u64,
        rest_price: f64,
        anchor_hedge_mid: f64,
        target_notional_usd: f64,
    },
    HedgingTakerLeg {
        dir: CarryDirection,
        passive_is_a: bool,
        filled_at_ms: u64,
        filled_notional_usd: f64,
    },
    ActiveCarry {
        dir: CarryDirection,
        entered_at_ms: u64,
        entry_mid_basis_bps: f64,
        entry_exec_basis_bps: f64,
        notional_usd: f64,
    },
    Unwinding {
        dir: CarryDirection,
        started_at_ms: u64,
        reason: CarryExitReason,
    },
    FailClosedHalt {
        halted_at_ms: u64,
        reason: CarryHaltReason,
    },
}

/// Why an entry candidate was blocked on a clean tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CarrySignalBlock {
    WarmingUp,
    CarryBelowMin,
    BasisZscoreVeto,
    HurdleNotCleared,
    InsufficientL1Depth,
    FxDragExceeded,
    PairCapExceeded,
    Halted,
    InvalidOrStale,
}

/// Action recommended by [`FundingBasisCarryController::evaluate_signal`].
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum CarryAction {
    Hold,
    Blocked {
        reason: CarrySignalBlock,
    },
    EnterTakerBoth {
        dir: CarryDirection,
        notional_usd: f64,
        expected_carry_bps: f64,
        basis_timing_advantage_bps: f64,
        basis_zscore: f64,
        hurdle_bps: f64,
    },
    PostPassiveLeg {
        dir: CarryDirection,
        passive_is_a: bool,
        rest_price: f64,
        notional_usd: f64,
        expected_carry_bps: f64,
        basis_timing_advantage_bps: f64,
        basis_zscore: f64,
        hurdle_bps: f64,
    },
    CancelPassiveToIdle {
        reason: CarryHaltReason,
    },
    DispatchTakerHedge {
        dir: CarryDirection,
        hedge_is_a: bool,
        notional_usd: f64,
    },
    ExitUnwind {
        dir: CarryDirection,
        reason: CarryExitReason,
    },
    Halt {
        reason: CarryHaltReason,
    },
}

/// Causal diagnostics returned with every evaluation step.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
pub struct CarryDecision {
    pub action: CarryAction,
    pub state: PassiveFirstCarryExecState,
    pub funding_ema_bps_hr: f64,
    pub fair_mid_basis_bps: f64,
    pub basis_sigma_bps: f64,
    pub exec_basis_bps: f64,
    pub basis_zscore: f64,
    pub expected_carry_bps: f64,
    pub basis_timing_advantage_bps: f64,
    pub hurdle_bps: f64,
}

/// Configuration for [`FundingBasisCarryController`].
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct FundingBasisCarryConfig {
    pub funding_tau_ms: u64,
    pub basis_fair_tau_ms: u64,
    pub basis_vol_tau_ms: u64,
    pub horizon_hours: f64,
    pub hurdle_mult: f64,
    pub min_expected_carry_bps: f64,
    pub min_basis_zscore: f64,
    pub basis_vol_floor_bps: f64,
    pub basis_timing_weight: f64,
    pub basis_timing_cap_bps: f64,
    pub take_profit_basis_bps: f64,
    pub take_profit_zscore: f64,
    pub adverse_funding_exit_bps_hr: f64,
    pub max_basis_stop_bps: f64,
    pub max_hold_ms: u64,
    pub max_book_age_ms: u64,
    pub max_funding_age_ms: u64,
    pub max_gap_ms: u64,
    pub min_l1_notional_usd: f64,
    pub passive_first_enabled: bool,
    pub max_passive_wait_ms: u64,
    pub hazard_adverse_bps: f64,
    pub passive_entry_maker_latency_ms: u64,
    pub taker_hedge_latency_ms: u64,
    pub min_warmup_samples: u32,
}

impl Default for FundingBasisCarryConfig {
    fn default() -> Self {
        Self {
            funding_tau_ms: 24 * 3_600_000,
            basis_fair_tau_ms: 6 * 3_600_000,
            basis_vol_tau_ms: 3_600_000,
            horizon_hours: 12.0,
            hurdle_mult: 1.5,
            min_expected_carry_bps: 1.0,
            min_basis_zscore: 1.0,
            basis_vol_floor_bps: 1.0,
            basis_timing_weight: 1.0,
            basis_timing_cap_bps: 25.0,
            take_profit_basis_bps: 6.0,
            take_profit_zscore: 0.0,
            adverse_funding_exit_bps_hr: 0.0,
            max_basis_stop_bps: 25.0,
            max_hold_ms: 48 * 3_600_000,
            max_book_age_ms: 5_000,
            max_funding_age_ms: 2 * 3_600_000,
            max_gap_ms: 3 * 3_600_000,
            min_l1_notional_usd: 500.0,
            passive_first_enabled: false,
            max_passive_wait_ms: 2_000,
            hazard_adverse_bps: 3.0,
            passive_entry_maker_latency_ms: 200,
            taker_hedge_latency_ms: 330,
            min_warmup_samples: 12,
        }
    }
}

impl FundingBasisCarryConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.funding_tau_ms > 0 && self.basis_fair_tau_ms > 0 && self.basis_vol_tau_ms > 0,
            "EWMA taus must be > 0"
        );
        for (name, val) in [
            ("horizon_hours", self.horizon_hours),
            ("hurdle_mult", self.hurdle_mult),
            ("min_expected_carry_bps", self.min_expected_carry_bps),
            ("min_basis_zscore", self.min_basis_zscore),
            ("basis_vol_floor_bps", self.basis_vol_floor_bps),
            ("basis_timing_weight", self.basis_timing_weight),
            ("basis_timing_cap_bps", self.basis_timing_cap_bps),
            ("take_profit_basis_bps", self.take_profit_basis_bps),
            ("take_profit_zscore", self.take_profit_zscore),
            (
                "adverse_funding_exit_bps_hr",
                self.adverse_funding_exit_bps_hr,
            ),
            ("max_basis_stop_bps", self.max_basis_stop_bps),
            ("min_l1_notional_usd", self.min_l1_notional_usd),
            ("hazard_adverse_bps", self.hazard_adverse_bps),
        ] {
            ensure!(val.is_finite(), "{name} must be finite");
        }
        ensure!(
            self.horizon_hours > 0.0
                && self.hurdle_mult >= 1.0
                && self.min_expected_carry_bps >= 0.0
                && self.basis_vol_floor_bps > 0.0
                && self.basis_timing_weight >= 0.0
                && self.basis_timing_cap_bps >= 0.0
                && self.take_profit_basis_bps >= 0.0
                && self.max_basis_stop_bps > 0.0
                && self.min_l1_notional_usd >= 0.0
                && self.hazard_adverse_bps > 0.0,
            "invalid FundingBasisCarryConfig bounds"
        );
        ensure!(
            self.max_book_age_ms > 0 && self.max_funding_age_ms > 0 && self.max_gap_ms > 0,
            "staleness and gap limits must be > 0"
        );
        Ok(())
    }
}

/// Zero-allocation causal controller for joint funding carry + executable L1 basis timing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FundingBasisCarryController {
    config: FundingBasisCarryConfig,
    economics: CarryPairEconomics,
    state: PassiveFirstCarryExecState,
    funding_ema_bps_hr: Option<f64>,
    last_funding_ms: Option<u64>,
    fair_mid_basis_bps: Option<f64>,
    basis_var_bps2: Option<f64>,
    last_book_ms: Option<u64>,
    last_eval_ms: Option<u64>,
    warmup_samples: u32,
}

impl FundingBasisCarryController {
    pub fn new(config: FundingBasisCarryConfig, economics: CarryPairEconomics) -> Result<Self> {
        config.validate()?;
        economics.validate()?;
        Ok(Self {
            config,
            economics,
            state: PassiveFirstCarryExecState::Idle,
            funding_ema_bps_hr: None,
            last_funding_ms: None,
            fair_mid_basis_bps: None,
            basis_var_bps2: None,
            last_book_ms: None,
            last_eval_ms: None,
            warmup_samples: 0,
        })
    }

    pub fn state(&self) -> PassiveFirstCarryExecState {
        self.state
    }

    pub fn config(&self) -> &FundingBasisCarryConfig {
        &self.config
    }

    pub fn economics(&self) -> &CarryPairEconomics {
        &self.economics
    }

    fn latch_halt(&mut self, now_ms: u64, reason: CarryHaltReason) -> PassiveFirstCarryExecState {
        if !matches!(
            self.state,
            PassiveFirstCarryExecState::FailClosedHalt { .. }
        ) {
            self.state = PassiveFirstCarryExecState::FailClosedHalt {
                halted_at_ms: now_ms,
                reason,
            };
        }
        self.state
    }

    /// Explicit operator/reconciliation reset out of [`PassiveFirstCarryExecState::FailClosedHalt`].
    pub fn acknowledge_reconciled_reset(&mut self, now_ms: u64) -> Result<()> {
        if let Some(prev) = self.last_eval_ms {
            ensure!(now_ms >= prev, "non-monotonic clock on reset");
        }
        self.last_eval_ms = Some(now_ms);
        self.state = PassiveFirstCarryExecState::Idle;
        Ok(())
    }

    /// Update causal EWMA funding differential (`rate_A - rate_B` in bps/hr).
    pub fn on_funding(
        &mut self,
        now_ms: u64,
        obs_a: FundingObservation,
        obs_b: FundingObservation,
    ) -> Result<(), CarryHaltReason> {
        if let Some(prev) = self.last_funding_ms
            && now_ms < prev
        {
            self.latch_halt(now_ms, CarryHaltReason::NonMonotonicClock);
            return Err(CarryHaltReason::NonMonotonicClock);
        }
        if !obs_a.is_valid()
            || !obs_b.is_valid()
            || obs_a.observed_ms > now_ms
            || obs_b.observed_ms > now_ms
        {
            self.latch_halt(now_ms, CarryHaltReason::InvalidInput);
            return Err(CarryHaltReason::InvalidInput);
        }
        if now_ms - obs_a.observed_ms > self.config.max_funding_age_ms
            || now_ms - obs_b.observed_ms > self.config.max_funding_age_ms
        {
            self.latch_halt(now_ms, CarryHaltReason::StaleFunding);
            return Err(CarryHaltReason::StaleFunding);
        }

        let diff = obs_a.rate_bps_hr - obs_b.rate_bps_hr;
        match (self.funding_ema_bps_hr, self.last_funding_ms) {
            (Some(prev_ema), Some(prev_ms)) if now_ms - prev_ms <= self.config.max_gap_ms => {
                let dt = (now_ms - prev_ms) as f64;
                if dt > 0.0 {
                    let alpha = 1.0 - (-dt / (self.config.funding_tau_ms as f64)).exp();
                    self.funding_ema_bps_hr = Some(prev_ema + alpha * (diff - prev_ema));
                } else {
                    self.funding_ema_bps_hr = Some(diff);
                }
            }
            _ => {
                self.funding_ema_bps_hr = Some(diff);
            }
        }
        self.last_funding_ms = Some(now_ms);
        Ok(())
    }

    /// Notify controller that the passive leg filled, moving `WorkingPassiveLeg -> HedgingTakerLeg`.
    pub fn on_passive_fill(
        &mut self,
        now_ms: u64,
        filled_notional_usd: f64,
    ) -> Result<CarryAction, CarryHaltReason> {
        if let Some(prev) = self.last_eval_ms
            && now_ms < prev
        {
            self.latch_halt(now_ms, CarryHaltReason::NonMonotonicClock);
            return Err(CarryHaltReason::NonMonotonicClock);
        }
        self.last_eval_ms = Some(now_ms);
        if !filled_notional_usd.is_finite() || filled_notional_usd <= 0.0 {
            self.latch_halt(now_ms, CarryHaltReason::InvalidInput);
            return Err(CarryHaltReason::InvalidInput);
        }
        match self.state {
            PassiveFirstCarryExecState::WorkingPassiveLeg {
                dir, passive_is_a, ..
            } => {
                self.state = PassiveFirstCarryExecState::HedgingTakerLeg {
                    dir,
                    passive_is_a,
                    filled_at_ms: now_ms,
                    filled_notional_usd,
                };
                Ok(CarryAction::DispatchTakerHedge {
                    dir,
                    hedge_is_a: !passive_is_a,
                    notional_usd: filled_notional_usd,
                })
            }
            _ => {
                self.latch_halt(now_ms, CarryHaltReason::InvalidInput);
                Err(CarryHaltReason::InvalidInput)
            }
        }
    }

    /// Notify controller that the taker hedge leg completed, moving `HedgingTakerLeg -> ActiveCarry`.
    pub fn on_taker_hedge_complete(
        &mut self,
        now_ms: u64,
        entry_mid_basis_bps: f64,
        entry_exec_basis_bps: f64,
        notional_usd: f64,
    ) -> Result<(), CarryHaltReason> {
        if let Some(prev) = self.last_eval_ms
            && now_ms < prev
        {
            self.latch_halt(now_ms, CarryHaltReason::NonMonotonicClock);
            return Err(CarryHaltReason::NonMonotonicClock);
        }
        self.last_eval_ms = Some(now_ms);
        if !entry_mid_basis_bps.is_finite()
            || !entry_exec_basis_bps.is_finite()
            || !notional_usd.is_finite()
            || notional_usd <= 0.0
        {
            self.latch_halt(now_ms, CarryHaltReason::InvalidInput);
            return Err(CarryHaltReason::InvalidInput);
        }
        match self.state {
            PassiveFirstCarryExecState::HedgingTakerLeg { dir, .. } => {
                self.state = PassiveFirstCarryExecState::ActiveCarry {
                    dir,
                    entered_at_ms: now_ms,
                    entry_mid_basis_bps,
                    entry_exec_basis_bps,
                    notional_usd,
                };
                Ok(())
            }
            _ => {
                self.latch_halt(now_ms, CarryHaltReason::InvalidInput);
                Err(CarryHaltReason::InvalidInput)
            }
        }
    }

    /// Notify controller that an unwind completed cleanly, returning `Unwinding -> Idle`.
    pub fn on_unwind_complete(&mut self, now_ms: u64) -> Result<(), CarryHaltReason> {
        if let Some(prev) = self.last_eval_ms
            && now_ms < prev
        {
            self.latch_halt(now_ms, CarryHaltReason::NonMonotonicClock);
            return Err(CarryHaltReason::NonMonotonicClock);
        }
        self.last_eval_ms = Some(now_ms);
        match self.state {
            PassiveFirstCarryExecState::Unwinding { .. } => {
                self.state = PassiveFirstCarryExecState::Idle;
                Ok(())
            }
            _ => {
                self.latch_halt(now_ms, CarryHaltReason::InvalidInput);
                Err(CarryHaltReason::InvalidInput)
            }
        }
    }

    /// Evaluate live L1 books + causal funding differential, advancing state safely.
    pub fn evaluate_signal(
        &mut self,
        now_ms: u64,
        touch_a: CarryLegTouch,
        touch_b: CarryLegTouch,
        requested_notional_usd: f64,
    ) -> CarryDecision {
        let halt_decision = |ctrl: &mut Self, reason: CarryHaltReason| -> CarryDecision {
            let st = ctrl.latch_halt(now_ms, reason);
            CarryDecision {
                action: CarryAction::Halt { reason },
                state: st,
                funding_ema_bps_hr: ctrl.funding_ema_bps_hr.unwrap_or(f64::NAN),
                fair_mid_basis_bps: ctrl.fair_mid_basis_bps.unwrap_or(f64::NAN),
                basis_sigma_bps: ctrl
                    .basis_var_bps2
                    .filter(|v| *v > 0.0)
                    .map(f64::sqrt)
                    .unwrap_or(0.0),
                exec_basis_bps: f64::NAN,
                basis_zscore: f64::NAN,
                expected_carry_bps: 0.0,
                basis_timing_advantage_bps: 0.0,
                hurdle_bps: 0.0,
            }
        };

        if let Some(prev) = self.last_eval_ms
            && now_ms < prev
        {
            return halt_decision(self, CarryHaltReason::NonMonotonicClock);
        }
        self.last_eval_ms = Some(now_ms);

        if matches!(
            self.state,
            PassiveFirstCarryExecState::FailClosedHalt { .. }
        ) {
            return CarryDecision {
                action: CarryAction::Blocked {
                    reason: CarrySignalBlock::Halted,
                },
                state: self.state,
                funding_ema_bps_hr: self.funding_ema_bps_hr.unwrap_or(f64::NAN),
                fair_mid_basis_bps: self.fair_mid_basis_bps.unwrap_or(f64::NAN),
                basis_sigma_bps: self
                    .basis_var_bps2
                    .filter(|v| *v > 0.0)
                    .map(f64::sqrt)
                    .unwrap_or(0.0),
                exec_basis_bps: f64::NAN,
                basis_zscore: f64::NAN,
                expected_carry_bps: 0.0,
                basis_timing_advantage_bps: 0.0,
                hurdle_bps: 0.0,
            };
        }

        if !touch_a.is_valid()
            || !touch_b.is_valid()
            || touch_a.recv_ms > now_ms
            || touch_b.recv_ms > now_ms
            || !requested_notional_usd.is_finite()
        {
            if matches!(
                self.state,
                PassiveFirstCarryExecState::WorkingPassiveLeg { .. }
                    | PassiveFirstCarryExecState::HedgingTakerLeg { .. }
            ) {
                return halt_decision(self, CarryHaltReason::InvalidInput);
            }
            return CarryDecision {
                action: CarryAction::Blocked {
                    reason: CarrySignalBlock::InvalidOrStale,
                },
                state: self.state,
                funding_ema_bps_hr: self.funding_ema_bps_hr.unwrap_or(f64::NAN),
                fair_mid_basis_bps: self.fair_mid_basis_bps.unwrap_or(f64::NAN),
                basis_sigma_bps: 0.0,
                exec_basis_bps: f64::NAN,
                basis_zscore: f64::NAN,
                expected_carry_bps: 0.0,
                basis_timing_advantage_bps: 0.0,
                hurdle_bps: 0.0,
            };
        }

        let stale_book = now_ms - touch_a.recv_ms > self.config.max_book_age_ms
            || now_ms - touch_b.recv_ms > self.config.max_book_age_ms;
        if stale_book {
            if matches!(
                self.state,
                PassiveFirstCarryExecState::WorkingPassiveLeg { .. }
                    | PassiveFirstCarryExecState::HedgingTakerLeg { .. }
            ) {
                return halt_decision(self, CarryHaltReason::StaleBook);
            }
            return CarryDecision {
                action: CarryAction::Blocked {
                    reason: CarrySignalBlock::InvalidOrStale,
                },
                state: self.state,
                funding_ema_bps_hr: self.funding_ema_bps_hr.unwrap_or(f64::NAN),
                fair_mid_basis_bps: self.fair_mid_basis_bps.unwrap_or(f64::NAN),
                basis_sigma_bps: 0.0,
                exec_basis_bps: f64::NAN,
                basis_zscore: f64::NAN,
                expected_carry_bps: 0.0,
                basis_timing_advantage_bps: 0.0,
                hurdle_bps: 0.0,
            };
        }

        let mid_a = touch_a.mid();
        let mid_b = touch_b.mid();
        let ref_mid = 0.5 * (mid_a + mid_b);
        if !(ref_mid > 0.0 && ref_mid.is_finite()) {
            return halt_decision(self, CarryHaltReason::InvalidInput);
        }
        // Raw mid basis (Leg A - Leg B) in bps.
        let raw_mid_basis_ab_bps = ((mid_a - mid_b) / ref_mid) * BPS_SCALE;

        // Snapshot STRICTLY PRIOR causal statistics before updating with current observation.
        let prior_fair_ab = self.fair_mid_basis_bps;
        let prior_sigma = self
            .basis_var_bps2
            .filter(|v| *v > 0.0)
            .map(f64::sqrt)
            .unwrap_or(0.0);
        let prior_funding_ab = self.funding_ema_bps_hr;
        let prior_funding_fresh = self
            .last_funding_ms
            .is_some_and(|t| now_ms.saturating_sub(t) <= self.config.max_funding_age_ms);

        // Update causal fair basis + variance with current valid book.
        match (self.fair_mid_basis_bps, self.last_book_ms) {
            (Some(prev_fair), Some(prev_ms)) if now_ms - prev_ms <= self.config.max_gap_ms => {
                let dt = (now_ms - prev_ms) as f64;
                if dt > 0.0 {
                    let alpha_f = 1.0 - (-dt / (self.config.basis_fair_tau_ms as f64)).exp();
                    let alpha_v = 1.0 - (-dt / (self.config.basis_vol_tau_ms as f64)).exp();
                    let dev = raw_mid_basis_ab_bps - prev_fair;
                    let new_var = match self.basis_var_bps2 {
                        Some(v) => (1.0 - alpha_v) * v + alpha_v * dev * dev,
                        None => dev * dev,
                    };
                    self.basis_var_bps2 = Some(new_var);
                    self.fair_mid_basis_bps = Some(prev_fair + alpha_f * dev);
                }
            }
            _ => {
                self.fair_mid_basis_bps = Some(raw_mid_basis_ab_bps);
                self.basis_var_bps2 = None;
                self.warmup_samples = 0;
            }
        }
        self.last_book_ms = Some(now_ms);
        self.warmup_samples = self.warmup_samples.saturating_add(1);

        // Manage active states first.
        match self.state {
            PassiveFirstCarryExecState::FailClosedHalt { .. } => unreachable!(),
            PassiveFirstCarryExecState::HedgingTakerLeg {
                dir,
                passive_is_a,
                filled_notional_usd,
                ..
            } => {
                return CarryDecision {
                    action: CarryAction::DispatchTakerHedge {
                        dir,
                        hedge_is_a: !passive_is_a,
                        notional_usd: filled_notional_usd,
                    },
                    state: self.state,
                    funding_ema_bps_hr: prior_funding_ab.unwrap_or(0.0),
                    fair_mid_basis_bps: prior_fair_ab.unwrap_or(raw_mid_basis_ab_bps),
                    basis_sigma_bps: prior_sigma,
                    exec_basis_bps: 0.0,
                    basis_zscore: 0.0,
                    expected_carry_bps: 0.0,
                    basis_timing_advantage_bps: 0.0,
                    hurdle_bps: 0.0,
                };
            }
            PassiveFirstCarryExecState::WorkingPassiveLeg {
                dir,
                passive_is_a,
                posted_at_ms,
                anchor_hedge_mid,
                ..
            } => {
                let cur_hedge_mid = if passive_is_a { mid_b } else { mid_a };
                // Adverse move on the hedge leg against the pending carry entry direction.
                let hedge_is_long = matches!(
                    (dir, passive_is_a),
                    (CarryDirection::ShortALongB, true) | (CarryDirection::LongAShortB, false)
                );
                let adv_bps = if hedge_is_long {
                    ((cur_hedge_mid / anchor_hedge_mid) - 1.0) * BPS_SCALE
                } else {
                    (1.0 - (cur_hedge_mid / anchor_hedge_mid)) * BPS_SCALE
                };
                if adv_bps >= self.config.hazard_adverse_bps {
                    self.state = PassiveFirstCarryExecState::Idle;
                    return CarryDecision {
                        action: CarryAction::CancelPassiveToIdle {
                            reason: CarryHaltReason::PassiveHazardEscalation,
                        },
                        state: self.state,
                        funding_ema_bps_hr: prior_funding_ab.unwrap_or(0.0),
                        fair_mid_basis_bps: prior_fair_ab.unwrap_or(raw_mid_basis_ab_bps),
                        basis_sigma_bps: prior_sigma,
                        exec_basis_bps: 0.0,
                        basis_zscore: 0.0,
                        expected_carry_bps: 0.0,
                        basis_timing_advantage_bps: 0.0,
                        hurdle_bps: 0.0,
                    };
                }
                if now_ms.saturating_sub(posted_at_ms) >= self.config.max_passive_wait_ms {
                    self.state = PassiveFirstCarryExecState::Idle;
                    return CarryDecision {
                        action: CarryAction::CancelPassiveToIdle {
                            reason: CarryHaltReason::PassiveTimeout,
                        },
                        state: self.state,
                        funding_ema_bps_hr: prior_funding_ab.unwrap_or(0.0),
                        fair_mid_basis_bps: prior_fair_ab.unwrap_or(raw_mid_basis_ab_bps),
                        basis_sigma_bps: prior_sigma,
                        exec_basis_bps: 0.0,
                        basis_zscore: 0.0,
                        expected_carry_bps: 0.0,
                        basis_timing_advantage_bps: 0.0,
                        hurdle_bps: 0.0,
                    };
                }
                return CarryDecision {
                    action: CarryAction::Hold,
                    state: self.state,
                    funding_ema_bps_hr: prior_funding_ab.unwrap_or(0.0),
                    fair_mid_basis_bps: prior_fair_ab.unwrap_or(raw_mid_basis_ab_bps),
                    basis_sigma_bps: prior_sigma,
                    exec_basis_bps: 0.0,
                    basis_zscore: 0.0,
                    expected_carry_bps: 0.0,
                    basis_timing_advantage_bps: 0.0,
                    hurdle_bps: 0.0,
                };
            }
            PassiveFirstCarryExecState::ActiveCarry {
                dir,
                entered_at_ms,
                entry_mid_basis_bps,
                ..
            } => {
                let dir_sgn = dir.sign_short_a();
                let dir_mid_basis_bps = dir_sgn * raw_mid_basis_ab_bps;
                let dir_fair_bps = dir_sgn * prior_fair_ab.unwrap_or(raw_mid_basis_ab_bps);
                let denom_bps = prior_sigma.max(self.config.basis_vol_floor_bps);
                let dir_z = (dir_mid_basis_bps - dir_fair_bps) / denom_bps;
                // Positive means basis converged favorably since short-rich / long-cheap entry.
                let convergence_bps = entry_mid_basis_bps - dir_mid_basis_bps;
                let dir_funding_ema = dir_sgn * prior_funding_ab.unwrap_or(0.0);

                let exit_reason = if !prior_funding_fresh
                    || dir_funding_ema <= self.config.adverse_funding_exit_bps_hr
                {
                    Some(CarryExitReason::AdverseFundingFlip)
                } else if -convergence_bps >= self.config.max_basis_stop_bps {
                    Some(CarryExitReason::BasisStopLoss)
                } else if self.config.take_profit_basis_bps > 0.0
                    && convergence_bps >= self.config.take_profit_basis_bps
                    && dir_z <= self.config.take_profit_zscore
                {
                    Some(CarryExitReason::BasisConvergenceTakeProfit)
                } else if self.config.max_hold_ms > 0
                    && now_ms.saturating_sub(entered_at_ms) >= self.config.max_hold_ms
                {
                    Some(CarryExitReason::MaxHoldExpired)
                } else {
                    None
                };

                if let Some(reason) = exit_reason {
                    self.state = PassiveFirstCarryExecState::Unwinding {
                        dir,
                        started_at_ms: now_ms,
                        reason,
                    };
                    return CarryDecision {
                        action: CarryAction::ExitUnwind { dir, reason },
                        state: self.state,
                        funding_ema_bps_hr: prior_funding_ab.unwrap_or(0.0),
                        fair_mid_basis_bps: prior_fair_ab.unwrap_or(raw_mid_basis_ab_bps),
                        basis_sigma_bps: prior_sigma,
                        exec_basis_bps: match dir {
                            CarryDirection::ShortALongB => {
                                ((touch_a.bid - touch_b.ask) / ref_mid) * BPS_SCALE
                            }
                            CarryDirection::LongAShortB => {
                                ((touch_b.bid - touch_a.ask) / ref_mid) * BPS_SCALE
                            }
                        },
                        basis_zscore: dir_z,
                        expected_carry_bps: dir_funding_ema * self.config.horizon_hours,
                        basis_timing_advantage_bps: convergence_bps,
                        hurdle_bps: 0.0,
                    };
                }
                return CarryDecision {
                    action: CarryAction::Hold,
                    state: self.state,
                    funding_ema_bps_hr: prior_funding_ab.unwrap_or(0.0),
                    fair_mid_basis_bps: prior_fair_ab.unwrap_or(raw_mid_basis_ab_bps),
                    basis_sigma_bps: prior_sigma,
                    exec_basis_bps: match dir {
                        CarryDirection::ShortALongB => {
                            ((touch_a.bid - touch_b.ask) / ref_mid) * BPS_SCALE
                        }
                        CarryDirection::LongAShortB => {
                            ((touch_b.bid - touch_a.ask) / ref_mid) * BPS_SCALE
                        }
                    },
                    basis_zscore: dir_z,
                    expected_carry_bps: dir_funding_ema * self.config.horizon_hours,
                    basis_timing_advantage_bps: convergence_bps,
                    hurdle_bps: 0.0,
                };
            }
            PassiveFirstCarryExecState::Unwinding { dir, reason, .. } => {
                return CarryDecision {
                    action: CarryAction::ExitUnwind { dir, reason },
                    state: self.state,
                    funding_ema_bps_hr: prior_funding_ab.unwrap_or(0.0),
                    fair_mid_basis_bps: prior_fair_ab.unwrap_or(raw_mid_basis_ab_bps),
                    basis_sigma_bps: prior_sigma,
                    exec_basis_bps: 0.0,
                    basis_zscore: 0.0,
                    expected_carry_bps: 0.0,
                    basis_timing_advantage_bps: 0.0,
                    hurdle_bps: 0.0,
                };
            }
            PassiveFirstCarryExecState::Idle => {}
        }

        // Idle entry evaluation.
        if self.economics.exceeds_fx_bound() {
            return CarryDecision {
                action: CarryAction::Blocked {
                    reason: CarrySignalBlock::FxDragExceeded,
                },
                state: self.state,
                funding_ema_bps_hr: prior_funding_ab.unwrap_or(0.0),
                fair_mid_basis_bps: prior_fair_ab.unwrap_or(raw_mid_basis_ab_bps),
                basis_sigma_bps: prior_sigma,
                exec_basis_bps: 0.0,
                basis_zscore: 0.0,
                expected_carry_bps: 0.0,
                basis_timing_advantage_bps: 0.0,
                hurdle_bps: 0.0,
            };
        }
        if requested_notional_usd <= 0.0
            || requested_notional_usd > self.economics.max_pair_notional_usd
        {
            return CarryDecision {
                action: CarryAction::Blocked {
                    reason: CarrySignalBlock::PairCapExceeded,
                },
                state: self.state,
                funding_ema_bps_hr: prior_funding_ab.unwrap_or(0.0),
                fair_mid_basis_bps: prior_fair_ab.unwrap_or(raw_mid_basis_ab_bps),
                basis_sigma_bps: prior_sigma,
                exec_basis_bps: 0.0,
                basis_zscore: 0.0,
                expected_carry_bps: 0.0,
                basis_timing_advantage_bps: 0.0,
                hurdle_bps: 0.0,
            };
        }
        if self.warmup_samples < self.config.min_warmup_samples
            || prior_fair_ab.is_none()
            || prior_funding_ab.is_none()
            || !prior_funding_fresh
        {
            return CarryDecision {
                action: CarryAction::Blocked {
                    reason: CarrySignalBlock::WarmingUp,
                },
                state: self.state,
                funding_ema_bps_hr: prior_funding_ab.unwrap_or(0.0),
                fair_mid_basis_bps: prior_fair_ab.unwrap_or(raw_mid_basis_ab_bps),
                basis_sigma_bps: prior_sigma,
                exec_basis_bps: 0.0,
                basis_zscore: 0.0,
                expected_carry_bps: 0.0,
                basis_timing_advantage_bps: 0.0,
                hurdle_bps: 0.0,
            };
        }

        let f_ab = prior_funding_ab.unwrap_or(0.0);
        let dir = if f_ab >= 0.0 {
            CarryDirection::ShortALongB
        } else {
            CarryDirection::LongAShortB
        };
        let dir_sgn = dir.sign_short_a();
        let expected_carry_bps = (dir_sgn * f_ab) * self.config.horizon_hours;

        // Executable L1 basis: short_leg_bid - long_leg_ask normalized in bps.
        let exec_basis_bps = match dir {
            CarryDirection::ShortALongB => ((touch_a.bid - touch_b.ask) / ref_mid) * BPS_SCALE,
            CarryDirection::LongAShortB => ((touch_b.bid - touch_a.ask) / ref_mid) * BPS_SCALE,
        };
        let dir_mid_basis_bps = dir_sgn * raw_mid_basis_ab_bps;
        let dir_fair_bps = dir_sgn * prior_fair_ab.unwrap_or(raw_mid_basis_ab_bps);
        let denom_bps = prior_sigma.max(self.config.basis_vol_floor_bps);
        let basis_zscore = (dir_mid_basis_bps - dir_fair_bps) / denom_bps;

        let raw_timing_adv = (dir_mid_basis_bps - dir_fair_bps) * self.config.basis_timing_weight;
        let basis_timing_advantage_bps = raw_timing_adv.clamp(
            -self.config.basis_timing_cap_bps,
            self.config.basis_timing_cap_bps,
        );

        let spr_a = touch_a.spread_bps();
        let spr_b = touch_b.spread_bps();
        let passive_is_a = spr_a >= spr_b;
        let rt_fee_bps = if self.config.passive_first_enabled {
            self.economics.rt_passive_first_fee_bps(passive_is_a)
        } else {
            self.economics.rt_taker_fee_bps()
        };
        let rt_spread_bps = if self.config.passive_first_enabled {
            let wide = spr_a.max(spr_b);
            let tight = spr_a.min(spr_b);
            0.5 * wide + 1.0 * tight
        } else {
            spr_a + spr_b
        };
        let fx_drag_bps = self.economics.effective_fx_drag_bps();
        let hurdle_bps = self.config.hurdle_mult * (rt_fee_bps + rt_spread_bps + fx_drag_bps);

        if expected_carry_bps < self.config.min_expected_carry_bps {
            return CarryDecision {
                action: CarryAction::Blocked {
                    reason: CarrySignalBlock::CarryBelowMin,
                },
                state: self.state,
                funding_ema_bps_hr: f_ab,
                fair_mid_basis_bps: prior_fair_ab.unwrap_or(raw_mid_basis_ab_bps),
                basis_sigma_bps: prior_sigma,
                exec_basis_bps,
                basis_zscore,
                expected_carry_bps,
                basis_timing_advantage_bps,
                hurdle_bps,
            };
        }

        if basis_zscore < self.config.min_basis_zscore {
            return CarryDecision {
                action: CarryAction::Blocked {
                    reason: CarrySignalBlock::BasisZscoreVeto,
                },
                state: self.state,
                funding_ema_bps_hr: f_ab,
                fair_mid_basis_bps: prior_fair_ab.unwrap_or(raw_mid_basis_ab_bps),
                basis_sigma_bps: prior_sigma,
                exec_basis_bps,
                basis_zscore,
                expected_carry_bps,
                basis_timing_advantage_bps,
                hurdle_bps,
            };
        }

        if expected_carry_bps + basis_timing_advantage_bps < hurdle_bps {
            return CarryDecision {
                action: CarryAction::Blocked {
                    reason: CarrySignalBlock::HurdleNotCleared,
                },
                state: self.state,
                funding_ema_bps_hr: f_ab,
                fair_mid_basis_bps: prior_fair_ab.unwrap_or(raw_mid_basis_ab_bps),
                basis_sigma_bps: prior_sigma,
                exec_basis_bps,
                basis_zscore,
                expected_carry_bps,
                basis_timing_advantage_bps,
                hurdle_bps,
            };
        }

        // Check displayed L1 depth on the legs to be crossed/posted.
        let (l1_a, l1_b) = match dir {
            CarryDirection::ShortALongB => (touch_a.bid_notional_usd(), touch_b.ask_notional_usd()),
            CarryDirection::LongAShortB => (touch_a.ask_notional_usd(), touch_b.bid_notional_usd()),
        };
        if l1_a < self.config.min_l1_notional_usd || l1_b < self.config.min_l1_notional_usd {
            return CarryDecision {
                action: CarryAction::Blocked {
                    reason: CarrySignalBlock::InsufficientL1Depth,
                },
                state: self.state,
                funding_ema_bps_hr: f_ab,
                fair_mid_basis_bps: prior_fair_ab.unwrap_or(raw_mid_basis_ab_bps),
                basis_sigma_bps: prior_sigma,
                exec_basis_bps,
                basis_zscore,
                expected_carry_bps,
                basis_timing_advantage_bps,
                hurdle_bps,
            };
        }

        if self.config.passive_first_enabled {
            let rest_price = match (dir, passive_is_a) {
                (CarryDirection::ShortALongB, true) => touch_a.ask,
                (CarryDirection::ShortALongB, false) => touch_b.bid,
                (CarryDirection::LongAShortB, true) => touch_a.bid,
                (CarryDirection::LongAShortB, false) => touch_b.ask,
            };
            let anchor_hedge_mid = if passive_is_a { mid_b } else { mid_a };
            self.state = PassiveFirstCarryExecState::WorkingPassiveLeg {
                dir,
                passive_is_a,
                posted_at_ms: now_ms,
                rest_price,
                anchor_hedge_mid,
                target_notional_usd: requested_notional_usd,
            };
            return CarryDecision {
                action: CarryAction::PostPassiveLeg {
                    dir,
                    passive_is_a,
                    rest_price,
                    notional_usd: requested_notional_usd,
                    expected_carry_bps,
                    basis_timing_advantage_bps,
                    basis_zscore,
                    hurdle_bps,
                },
                state: self.state,
                funding_ema_bps_hr: f_ab,
                fair_mid_basis_bps: prior_fair_ab.unwrap_or(raw_mid_basis_ab_bps),
                basis_sigma_bps: prior_sigma,
                exec_basis_bps,
                basis_zscore,
                expected_carry_bps,
                basis_timing_advantage_bps,
                hurdle_bps,
            };
        }

        self.state = PassiveFirstCarryExecState::ActiveCarry {
            dir,
            entered_at_ms: now_ms,
            entry_mid_basis_bps: dir_mid_basis_bps,
            entry_exec_basis_bps: exec_basis_bps,
            notional_usd: requested_notional_usd,
        };
        CarryDecision {
            action: CarryAction::EnterTakerBoth {
                dir,
                notional_usd: requested_notional_usd,
                expected_carry_bps,
                basis_timing_advantage_bps,
                basis_zscore,
                hurdle_bps,
            },
            state: self.state,
            funding_ema_bps_hr: f_ab,
            fair_mid_basis_bps: prior_fair_ab.unwrap_or(raw_mid_basis_ab_bps),
            basis_sigma_bps: prior_sigma,
            exec_basis_bps,
            basis_zscore,
            expected_carry_bps,
            basis_timing_advantage_bps,
            hurdle_bps,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_touch(bid: f64, ask: f64, recv_ms: u64) -> CarryLegTouch {
        CarryLegTouch {
            bid,
            ask,
            bid_sz: 100.0,
            ask_sz: 100.0,
            recv_ms,
        }
    }

    fn warm_controller(
        cfg: FundingBasisCarryConfig,
        econ: CarryPairEconomics,
        rate_a: f64,
        rate_b: f64,
    ) -> FundingBasisCarryController {
        let mut ctrl = FundingBasisCarryController::new(cfg, econ).unwrap();
        let mut t = 1_000_000;
        for i in 0..15 {
            t += 1_000;
            ctrl.on_funding(
                t,
                FundingObservation {
                    rate_bps_hr: rate_a,
                    observed_ms: t,
                },
                FundingObservation {
                    rate_bps_hr: rate_b,
                    observed_ms: t,
                },
            )
            .unwrap();
            let offset = if i % 2 == 0 { 0.02 } else { -0.02 };
            let _ = ctrl.evaluate_signal(
                t,
                mk_touch(100.0 + offset - 0.01, 100.0 + offset + 0.01, t),
                mk_touch(99.99, 100.01, t),
                500.0,
            );
        }
        ctrl
    }

    #[test]
    fn basis_timing_vetoes_carry_when_zscore_unfavorable_and_approves_when_favorable() {
        let cfg = FundingBasisCarryConfig {
            horizon_hours: 12.0,
            hurdle_mult: 1.5,
            min_expected_carry_bps: 2.0,
            min_basis_zscore: 1.5,
            basis_vol_floor_bps: 1.0,
            ..Default::default()
        };
        let econ = CarryPairEconomics {
            leg_a_taker_fee_bps: 0.9,
            leg_b_taker_fee_bps: 2.8,
            max_pair_notional_usd: 1_000.0,
            ..Default::default()
        };
        let mut ctrl = warm_controller(cfg, econ, 1.5, 0.0);
        let t = 1_020_000;

        // Unfavorable basis: Leg A (short leg) is cheap relative to Leg B -> zscore < 0 -> VETO!
        let veto = ctrl.evaluate_signal(
            t,
            mk_touch(99.90, 99.92, t),
            mk_touch(99.99, 100.01, t),
            500.0,
        );
        assert_eq!(
            veto.action,
            CarryAction::Blocked {
                reason: CarrySignalBlock::BasisZscoreVeto
            }
        );
        assert_eq!(ctrl.state(), PassiveFirstCarryExecState::Idle);

        // Favorable basis: Leg A (short leg) is rich relative to Leg B -> zscore >= 1.5 & clears hurdle!
        let t2 = t + 1_000;
        let ok = ctrl.evaluate_signal(
            t2,
            mk_touch(100.09, 100.11, t2),
            mk_touch(99.99, 100.01, t2),
            500.0,
        );
        assert!(matches!(
            ok.action,
            CarryAction::EnterTakerBoth {
                dir: CarryDirection::ShortALongB,
                ..
            }
        ));
        assert!(matches!(
            ctrl.state(),
            PassiveFirstCarryExecState::ActiveCarry {
                dir: CarryDirection::ShortALongB,
                ..
            }
        ));
    }

    #[test]
    fn passive_first_lifecycle_and_fail_closed_hedge_on_stale_book() {
        let cfg = FundingBasisCarryConfig {
            passive_first_enabled: true,
            min_basis_zscore: 1.0,
            max_book_age_ms: 1_000,
            max_passive_wait_ms: 2_000,
            ..Default::default()
        };
        let econ = CarryPairEconomics {
            leg_a_maker_fee_bps: 0.3,
            leg_a_taker_fee_bps: 0.9,
            leg_b_maker_fee_bps: 0.0,
            leg_b_taker_fee_bps: 0.0,
            max_pair_notional_usd: 1_000.0,
            ..Default::default()
        };
        let mut ctrl = warm_controller(cfg, econ, 1.5, 0.0);
        let t = 1_020_000;

        // Leg A has wider spread (0.04 vs 0.02) -> passive rests on Leg A ask.
        let dec = ctrl.evaluate_signal(
            t,
            mk_touch(100.08, 100.12, t),
            mk_touch(99.99, 100.01, t),
            500.0,
        );
        assert!(matches!(
            dec.action,
            CarryAction::PostPassiveLeg {
                passive_is_a: true,
                ..
            }
        ));
        assert!(matches!(
            ctrl.state(),
            PassiveFirstCarryExecState::WorkingPassiveLeg { .. }
        ));

        // Passive fills -> HedgingTakerLeg
        let hedge_act = ctrl.on_passive_fill(t + 200, 500.0).unwrap();
        assert!(matches!(
            hedge_act,
            CarryAction::DispatchTakerHedge {
                hedge_is_a: false,
                ..
            }
        ));
        assert!(matches!(
            ctrl.state(),
            PassiveFirstCarryExecState::HedgingTakerLeg { .. }
        ));

        // Stale book while HedgingTakerLeg -> FailClosedHalt (non-auto-resetting!)
        let stale = ctrl.evaluate_signal(
            t + 5_000,
            mk_touch(100.08, 100.12, t + 5_000),
            mk_touch(99.99, 100.01, t + 200),
            500.0,
        );
        assert_eq!(
            stale.action,
            CarryAction::Halt {
                reason: CarryHaltReason::StaleBook
            }
        );
        assert!(matches!(
            ctrl.state(),
            PassiveFirstCarryExecState::FailClosedHalt {
                reason: CarryHaltReason::StaleBook,
                ..
            }
        ));

        // Subsequent fresh ticks stay blocked until explicit acknowledge_reconciled_reset
        let blocked = ctrl.evaluate_signal(
            t + 5_500,
            mk_touch(100.08, 100.12, t + 5_500),
            mk_touch(99.99, 100.01, t + 5_500),
            500.0,
        );
        assert_eq!(
            blocked.action,
            CarryAction::Blocked {
                reason: CarrySignalBlock::Halted
            }
        );
        ctrl.acknowledge_reconciled_reset(t + 6_000).unwrap();
        assert_eq!(ctrl.state(), PassiveFirstCarryExecState::Idle);
    }

    #[test]
    fn basis_convergence_take_profit_and_adverse_funding_flip_exits() {
        let cfg = FundingBasisCarryConfig {
            min_basis_zscore: 1.0,
            take_profit_basis_bps: 5.0,
            take_profit_zscore: 0.25,
            adverse_funding_exit_bps_hr: 0.0,
            max_basis_stop_bps: 20.0,
            ..Default::default()
        };
        let mut ctrl = warm_controller(cfg, CarryPairEconomics::default(), 1.5, 0.0);
        let t = 1_020_000;
        let enter = ctrl.evaluate_signal(
            t,
            mk_touch(100.09, 100.11, t),
            mk_touch(99.99, 100.01, t),
            500.0,
        );
        assert!(matches!(enter.action, CarryAction::EnterTakerBoth { .. }));

        // Basis mean-reverts >= 5 bps and zscore <= 0.25 -> BasisConvergenceTakeProfit!
        let tp = ctrl.evaluate_signal(
            t + 1_000,
            mk_touch(99.99, 100.01, t + 1_000),
            mk_touch(99.99, 100.01, t + 1_000),
            500.0,
        );
        assert_eq!(
            tp.action,
            CarryAction::ExitUnwind {
                dir: CarryDirection::ShortALongB,
                reason: CarryExitReason::BasisConvergenceTakeProfit
            }
        );
        ctrl.on_unwind_complete(t + 1_500).unwrap();
        assert_eq!(ctrl.state(), PassiveFirstCarryExecState::Idle);

        // Re-enter and test AdverseFundingFlip exit
        let mut ctrl2 = warm_controller(cfg, CarryPairEconomics::default(), 1.5, 0.0);
        let _ = ctrl2.evaluate_signal(
            t,
            mk_touch(100.09, 100.11, t),
            mk_touch(99.99, 100.01, t),
            500.0,
        );
        for step in 1..=60 {
            let ts = t + step * 3_600_000;
            ctrl2
                .on_funding(
                    ts,
                    FundingObservation {
                        rate_bps_hr: -2.0,
                        observed_ms: ts,
                    },
                    FundingObservation {
                        rate_bps_hr: 0.0,
                        observed_ms: ts,
                    },
                )
                .unwrap();
            let dec = ctrl2.evaluate_signal(
                ts,
                mk_touch(100.09, 100.11, ts),
                mk_touch(99.99, 100.01, ts),
                500.0,
            );
            if matches!(
                dec.action,
                CarryAction::ExitUnwind {
                    reason: CarryExitReason::AdverseFundingFlip,
                    ..
                }
            ) {
                return;
            }
        }
        panic!("expected AdverseFundingFlip exit");
    }

    #[test]
    fn fx_drag_bound_per_pair_cap_and_nan_inf_stale_rejection() {
        let cfg = FundingBasisCarryConfig::default();
        let fx_econ = CarryPairEconomics {
            settlement_kind: PairSettlementKind::CrossStablecoinUsdgUsdc,
            fx_drag_bps: 3.5,
            max_fx_drag_bps: 3.0,
            max_pair_notional_usd: 1_000.0,
            ..Default::default()
        };
        let mut ctrl = warm_controller(cfg, fx_econ, 2.0, 0.0);
        let t = 1_020_000;

        // FX drag > max_fx_drag_bps -> blocked fail-closed
        let fx_block = ctrl.evaluate_signal(
            t,
            mk_touch(100.09, 100.11, t),
            mk_touch(99.99, 100.01, t),
            500.0,
        );
        assert_eq!(
            fx_block.action,
            CarryAction::Blocked {
                reason: CarrySignalBlock::FxDragExceeded
            }
        );

        // Per-pair cap exceeded
        let ok_fx_econ = CarryPairEconomics {
            settlement_kind: PairSettlementKind::CrossStablecoinUsdgUsdc,
            fx_drag_bps: 1.0,
            max_fx_drag_bps: 3.0,
            max_pair_notional_usd: 1_000.0,
            ..Default::default()
        };
        let mut ctrl2 = warm_controller(cfg, ok_fx_econ, 2.0, 0.0);
        let cap_block = ctrl2.evaluate_signal(
            t,
            mk_touch(100.09, 100.11, t),
            mk_touch(99.99, 100.01, t),
            1_500.0,
        );
        assert_eq!(
            cap_block.action,
            CarryAction::Blocked {
                reason: CarrySignalBlock::PairCapExceeded
            }
        );

        // NaN / Inf / crossed / stale book rejection
        let nan_touch = CarryLegTouch {
            bid: f64::NAN,
            ask: 100.0,
            bid_sz: 10.0,
            ask_sz: 10.0,
            recv_ms: t + 100,
        };
        let dec_nan =
            ctrl2.evaluate_signal(t + 100, nan_touch, mk_touch(99.99, 100.01, t + 100), 500.0);
        assert_eq!(
            dec_nan.action,
            CarryAction::Blocked {
                reason: CarrySignalBlock::InvalidOrStale
            }
        );

        let inf_funding = ctrl2.on_funding(
            t + 200,
            FundingObservation {
                rate_bps_hr: f64::INFINITY,
                observed_ms: t + 200,
            },
            FundingObservation {
                rate_bps_hr: 0.0,
                observed_ms: t + 200,
            },
        );
        assert_eq!(inf_funding, Err(CarryHaltReason::InvalidInput));
        assert!(matches!(
            ctrl2.state(),
            PassiveFirstCarryExecState::FailClosedHalt {
                reason: CarryHaltReason::InvalidInput,
                ..
            }
        ));

        // Clock regression latches NonMonotonicClock
        ctrl2.acknowledge_reconciled_reset(t + 500).unwrap();
        let reg = ctrl2.evaluate_signal(
            t + 400,
            mk_touch(100.0, 100.02, t + 400),
            mk_touch(99.99, 100.01, t + 400),
            500.0,
        );
        assert_eq!(
            reg.action,
            CarryAction::Halt {
                reason: CarryHaltReason::NonMonotonicClock
            }
        );
    }

    #[test]
    fn taker_hedge_complete_rejects_non_monotonic_clock_and_halts() {
        let cfg = FundingBasisCarryConfig {
            passive_first_enabled: true,
            min_basis_zscore: 1.0,
            ..Default::default()
        };
        let econ = CarryPairEconomics {
            settlement_kind: PairSettlementKind::CrossStablecoinUsdgUsdc,
            fx_drag_bps: 0.0,
            max_fx_drag_bps: 3.0,
            max_pair_notional_usd: 1_000.0,
            ..Default::default()
        };
        let mut ctrl = warm_controller(cfg, econ, 2.0, 0.0);
        let t = 1_020_000;
        let dec = ctrl.evaluate_signal(
            t,
            mk_touch(100.08, 100.12, t),
            mk_touch(99.99, 100.01, t),
            500.0,
        );
        assert!(matches!(dec.action, CarryAction::PostPassiveLeg { .. }));

        let hedge = ctrl.on_passive_fill(t + 250, 500.0).unwrap();
        assert!(matches!(hedge, CarryAction::DispatchTakerHedge { .. }));

        let reg = ctrl.on_taker_hedge_complete(t + 200, 10.0, 9.0, 500.0);
        assert_eq!(reg, Err(CarryHaltReason::NonMonotonicClock));
        assert!(matches!(
            ctrl.state(),
            PassiveFirstCarryExecState::FailClosedHalt {
                reason: CarryHaltReason::NonMonotonicClock,
                ..
            }
        ));
    }
}
