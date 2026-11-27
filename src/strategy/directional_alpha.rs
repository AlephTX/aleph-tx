//! Track 01 Directional Alpha Engines:
//! - [`OracleDiscoveryMomentum`]: Trades in the direction of sustained `mark_px - oracle_px`
//!   price discovery when perp price leads a lagging equity/RWA oracle.
//! - [`OverreactionReversion`]: Fades extreme 15m–60m return overreactions (`|ret| >= hurdle`)
//!   with spread/funding gating, volatility-optional normalization, and risk-bounded exits.
//!
//! Design invariants:
//! - Default-off (`enabled: false`) with `#[serde(default, deny_unknown_fields)]` on all configs.
//! - ZERO `use crate::exchanges::*` imports.
//! - ZERO hot-path heap allocations (`Copy` snapshots + fixed-capacity stack ring buffers).
//! - Strict fail-closed guards on non-monotonic clocks, future timestamps, stale feeds,
//!   gap resets, and `NaN`/`Inf`/crossed/non-positive inputs.
//! - Full [`DefenseFsm`] integration: `RiskIncreasingEntry` scales via `DefenseState::entry_size_scale`
//!   and blocks under `SideDefense`/`ReduceOnly`/`Halt`, while `ReduceOnlyExit` is ALWAYS permitted.

use crate::order_budget::OrderIntentClass;
use crate::strategy::defense_fsm::{DefenseFsm, DefenseSide, DefenseState};
use crate::strategy::stat_arb::L1Quote;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

const BPS_SCALE: f64 = 10_000.0;
const MIN_SIGMA_BAR_BPS: f64 = 1e-6;

/// Maximum stack ring-buffer capacity for causal bar prices in [`OverreactionReversion`].
pub const MAX_OVERREACTION_BARS: usize = 256;

/// Mark, oracle, and hourly funding context snapshot (stack `Copy`, zero allocations).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct OracleCtxSnapshot {
    pub ts_ms: u64,
    pub mark_px: f64,
    pub oracle_px: f64,
    /// Hourly funding rate in bps/hr (`> 0`: longs pay shorts; `< 0`: shorts pay longs).
    pub funding_rate_bps: f64,
}

/// High-level action emitted by [`OracleDiscoveryMomentum`] and [`OverreactionReversion`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectionalAction {
    Enter,
    Exit,
    Hold,
    Idle,
    Blocked,
}

/// Fail-closed or gating reason when an entry or tick is blocked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectionalBlockReason {
    Disabled,
    InvalidInput,
    ClockRegression,
    StaleQuote,
    StaleContext,
    WarmingUp,
    Cooldown,
    WideSpread,
    StructuralBreak,
    LowVolZ,
    AdverseFunding,
    DefenseBlocked,
}

/// Exit trigger reason for an open directional position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectionalExitReason {
    TakeProfit,
    MeanReversion,
    SignalDecay,
    StopLoss,
    MaxHoldTime,
}

/// Active open position tracked by either directional alpha engine.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DirectionalPosition {
    pub side: DefenseSide,
    pub entry_ms: u64,
    pub entry_px: f64,
    pub entry_signal_bps: f64,
}

// ============================================================================
// Engine 1: OracleDiscoveryMomentum
// ============================================================================

/// Configuration for [`OracleDiscoveryMomentum`]. Default-off (`enabled = false`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OracleDiscoveryMomentumConfig {
    /// Master enable switch (default `false`).
    pub enabled: bool,
    /// Use `ctx.mark_px` vs `ctx.oracle_px` when `true`; otherwise `perp_mid` vs `ctx.oracle_px`.
    pub use_mark_px: bool,
    /// Optional causal EWMA demeaning of `mark - oracle` when `true` (default `false`).
    pub demean_equilibrium: bool,
    /// EWMA time constant (ms) if `demean_equilibrium` is enabled.
    pub eq_ewma_tau_ms: u64,
    /// Minimum continuous observation time (ms) before permitting entries.
    pub warmup_ms: u64,
    /// Minimum valid observation samples before permitting entries.
    pub min_samples: usize,
    /// Maximum perp BBO quote age (ms).
    pub max_perp_age_ms: u64,
    /// Maximum context (`mark`/`oracle`/`funding`) age (ms).
    pub max_ctx_age_ms: u64,
    /// Maximum inter-tick gap (ms) before resetting warmup/equilibrium state.
    pub max_gap_ms: u64,
    /// Minimum `|signal_bps|` threshold required to trigger momentum entry.
    pub min_dislocation_bps: f64,
    /// Hurdle multiplier against L1 spread: `max(min_dislocation_bps, spread_mult * spread_bps)`.
    pub spread_mult: f64,
    /// Upper structural-break / stale-oracle cap (`|signal_bps| >= max_dislocation_bps` blocks entry).
    pub max_dislocation_bps: f64,
    /// Maximum L1 spread (bps) allowed on entry.
    pub max_entry_spread_bps: f64,
    /// Require funding alignment (`Buy` rejects `funding > max_adverse`, `Sell` rejects `funding < -max_adverse`).
    pub require_funding_alignment: bool,
    /// Maximum adverse hourly funding (bps/hr) allowed when `require_funding_alignment` is on.
    pub max_adverse_funding_bps: f64,
    /// Optional take-profit target in bps (`0.0` disables take-profit).
    pub take_profit_bps: f64,
    /// Optional stop-loss threshold in bps (`0.0` disables price stop-loss).
    pub stop_loss_bps: f64,
    /// Optional exit when `|signal_bps|` decays to `<= exit_dislocation_bps` after `min_hold_ms`.
    pub exit_on_signal_decay: bool,
    /// Signal decay exit threshold in bps (must be `< min_dislocation_bps`).
    pub exit_dislocation_bps: f64,
    /// Minimum holding duration (ms) before signal-decay exit can trigger.
    pub min_hold_ms: u64,
    /// Maximum holding duration (ms) before mandatory reduce-only time stop.
    pub max_hold_ms: u64,
    /// Post-exit cooldown (ms) before allowing a new entry.
    pub cooldown_ms: u64,
    /// Entry notional scale applied under `DefenseState::Caution` or `DefenseState::SideDefense`.
    pub caution_size_scale: f64,
}

impl Default for OracleDiscoveryMomentumConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            use_mark_px: true,
            demean_equilibrium: false,
            eq_ewma_tau_ms: 3_600_000,
            warmup_ms: 60_000,
            min_samples: 5,
            max_perp_age_ms: 5_000,
            max_ctx_age_ms: 15_000,
            max_gap_ms: 60_000,
            min_dislocation_bps: 15.0,
            spread_mult: 1.5,
            max_dislocation_bps: 250.0,
            max_entry_spread_bps: 20.0,
            require_funding_alignment: true,
            max_adverse_funding_bps: 1.0,
            take_profit_bps: 12.0,
            stop_loss_bps: 20.0,
            exit_on_signal_decay: false,
            exit_dislocation_bps: 3.0,
            min_hold_ms: 60_000,
            max_hold_ms: 900_000,
            cooldown_ms: 60_000,
            caution_size_scale: 0.50,
        }
    }
}

impl OracleDiscoveryMomentumConfig {
    /// Validate finite bounds and monotonic ordering of thresholds.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.eq_ewma_tau_ms >= 1_000,
            "oracle_mom.eq_ewma_tau_ms must be >= 1000"
        );
        ensure!(self.min_samples > 0, "oracle_mom.min_samples must be > 0");
        ensure!(
            self.max_perp_age_ms > 0 && self.max_ctx_age_ms > 0 && self.max_gap_ms > 0,
            "oracle_mom age and gap limits must be > 0"
        );
        ensure!(
            self.min_dislocation_bps.is_finite()
                && self.min_dislocation_bps > 0.0
                && self.spread_mult.is_finite()
                && self.spread_mult >= 0.0,
            "oracle_mom hurdle params must be finite and valid"
        );
        ensure!(
            self.max_dislocation_bps.is_finite()
                && self.max_dislocation_bps > self.min_dislocation_bps,
            "oracle_mom.max_dislocation_bps must be finite and > min_dislocation_bps"
        );
        ensure!(
            self.max_entry_spread_bps.is_finite() && self.max_entry_spread_bps > 0.0,
            "oracle_mom.max_entry_spread_bps must be finite and > 0"
        );
        ensure!(
            self.max_adverse_funding_bps.is_finite(),
            "oracle_mom.max_adverse_funding_bps must be finite"
        );
        ensure!(
            self.take_profit_bps.is_finite() && self.take_profit_bps >= 0.0,
            "oracle_mom.take_profit_bps must be finite and >= 0"
        );
        ensure!(
            self.stop_loss_bps.is_finite() && self.stop_loss_bps >= 0.0,
            "oracle_mom.stop_loss_bps must be finite and >= 0"
        );
        ensure!(
            self.exit_dislocation_bps.is_finite()
                && self.exit_dislocation_bps >= 0.0
                && self.exit_dislocation_bps < self.min_dislocation_bps,
            "oracle_mom.exit_dislocation_bps must lie in [0, min_dislocation_bps)"
        );
        ensure!(
            self.max_hold_ms > 0 && self.min_hold_ms <= self.max_hold_ms,
            "oracle_mom hold bounds must satisfy min_hold_ms <= max_hold_ms and max_hold_ms > 0"
        );
        ensure!(
            self.caution_size_scale.is_finite()
                && self.caution_size_scale > 0.0
                && self.caution_size_scale <= 1.0,
            "oracle_mom.caution_size_scale must lie in (0.0, 1.0]"
        );
        Ok(())
    }
}

/// Deterministic decision emitted by [`OracleDiscoveryMomentum::observe`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct OracleMomentumDecision {
    pub now_ms: u64,
    pub action: DirectionalAction,
    pub block_reason: Option<DirectionalBlockReason>,
    pub exit_reason: Option<DirectionalExitReason>,
    pub intent_class: Option<OrderIntentClass>,
    pub side: Option<DefenseSide>,
    pub entry_scale: f64,
    pub reduce_only: bool,
    pub raw_dislocation_bps: f64,
    pub signal_bps: f64,
    pub hurdle_bps: f64,
    pub spread_bps: f64,
    pub unrealized_bps: f64,
    pub funding_rate_bps: f64,
    pub sample_count: usize,
}

/// Zero-allocation causal `mark - oracle` momentum engine.
#[derive(Debug, Clone)]
pub struct OracleDiscoveryMomentum {
    config: OracleDiscoveryMomentumConfig,
    initialized: bool,
    first_obs_ms: Option<u64>,
    last_obs_ms: Option<u64>,
    equilibrium_bps: f64,
    sample_count: usize,
    position: Option<DirectionalPosition>,
    last_exit_ms: Option<u64>,
    clock_regressions: u64,
    gap_resets: u64,
    rejected_inputs: u64,
}

impl OracleDiscoveryMomentum {
    /// Construct a validated [`OracleDiscoveryMomentum`] controller.
    pub fn new(config: OracleDiscoveryMomentumConfig) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            config,
            initialized: false,
            first_obs_ms: None,
            last_obs_ms: None,
            equilibrium_bps: 0.0,
            sample_count: 0,
            position: None,
            last_exit_ms: None,
            clock_regressions: 0,
            gap_resets: 0,
            rejected_inputs: 0,
        })
    }

    pub fn config(&self) -> &OracleDiscoveryMomentumConfig {
        &self.config
    }

    pub fn position(&self) -> Option<DirectionalPosition> {
        self.position
    }

    pub fn clock_regressions(&self) -> u64 {
        self.clock_regressions
    }

    pub fn gap_resets(&self) -> u64 {
        self.gap_resets
    }

    pub fn rejected_inputs(&self) -> u64 {
        self.rejected_inputs
    }

    /// Clear open position state once exit execution is confirmed flat, recording cooldown anchor.
    pub fn notify_exit_filled(&mut self, exit_ms: u64) {
        self.position = None;
        self.last_exit_ms = Some(exit_ms);
    }

    /// Reset causal warmup/equilibrium state while preserving diagnostic counters.
    pub fn reset_filter(&mut self) {
        self.initialized = false;
        self.first_obs_ms = None;
        self.last_obs_ms = None;
        self.equilibrium_bps = 0.0;
        self.sample_count = 0;
    }

    /// Convenience wrapper reading `fsm.state()`.
    pub fn observe_with_fsm(
        &mut self,
        now_ms: u64,
        perp_quote: L1Quote,
        ctx: OracleCtxSnapshot,
        fsm: &DefenseFsm,
    ) -> OracleMomentumDecision {
        self.observe(now_ms, perp_quote, ctx, fsm.state())
    }

    /// Advance the causal `OracleDiscoveryMomentum` engine by one tick.
    ///
    /// Signal convention:
    /// - `raw_disloc_bps = (ref_px - oracle_px) / oracle_px * 10_000`
    /// - `signal_bps >= hurdle_bps` -> `Buy` (follow upward price discovery)
    /// - `signal_bps <= -hurdle_bps` -> `Sell` (follow downward price discovery)
    pub fn observe(
        &mut self,
        now_ms: u64,
        perp_quote: L1Quote,
        ctx: OracleCtxSnapshot,
        defense: DefenseState,
    ) -> OracleMomentumDecision {
        if self.last_obs_ms.is_some_and(|prev| now_ms < prev) {
            self.clock_regressions = self.clock_regressions.saturating_add(1);
            return self.blocked_decision(
                now_ms,
                DirectionalBlockReason::ClockRegression,
                0.0,
                0.0,
                0.0,
                0.0,
            );
        }

        if perp_quote.ts_ms > now_ms || ctx.ts_ms > now_ms {
            self.rejected_inputs = self.rejected_inputs.saturating_add(1);
            return self.blocked_decision(
                now_ms,
                DirectionalBlockReason::InvalidInput,
                0.0,
                0.0,
                0.0,
                0.0,
            );
        }

        let Some((perp_mid, spread_bps)) = perp_quote.mid_and_spread_bps() else {
            self.rejected_inputs = self.rejected_inputs.saturating_add(1);
            return self.blocked_decision(
                now_ms,
                DirectionalBlockReason::InvalidInput,
                0.0,
                0.0,
                0.0,
                0.0,
            );
        };

        if !ctx.oracle_px.is_finite()
            || ctx.oracle_px <= 0.0
            || !ctx.mark_px.is_finite()
            || ctx.mark_px <= 0.0
            || !ctx.funding_rate_bps.is_finite()
        {
            self.rejected_inputs = self.rejected_inputs.saturating_add(1);
            return self.blocked_decision(
                now_ms,
                DirectionalBlockReason::InvalidInput,
                0.0,
                0.0,
                spread_bps,
                0.0,
            );
        }

        let ref_px = if self.config.use_mark_px {
            ctx.mark_px
        } else {
            perp_mid
        };
        let raw_dislocation_bps = ((ref_px - ctx.oracle_px) / ctx.oracle_px) * BPS_SCALE;
        if !raw_dislocation_bps.is_finite() {
            self.rejected_inputs = self.rejected_inputs.saturating_add(1);
            return self.blocked_decision(
                now_ms,
                DirectionalBlockReason::InvalidInput,
                0.0,
                0.0,
                spread_bps,
                ctx.funding_rate_bps,
            );
        }

        if (now_ms - perp_quote.ts_ms) > self.config.max_perp_age_ms {
            return self.blocked_decision(
                now_ms,
                DirectionalBlockReason::StaleQuote,
                raw_dislocation_bps,
                0.0,
                spread_bps,
                ctx.funding_rate_bps,
            );
        }
        if (now_ms - ctx.ts_ms) > self.config.max_ctx_age_ms {
            return self.blocked_decision(
                now_ms,
                DirectionalBlockReason::StaleContext,
                raw_dislocation_bps,
                0.0,
                spread_bps,
                ctx.funding_rate_bps,
            );
        }

        if self
            .last_obs_ms
            .is_some_and(|prev| now_ms - prev > self.config.max_gap_ms)
        {
            self.gap_resets = self.gap_resets.saturating_add(1);
            self.reset_filter();
        }

        let signal_bps;
        if !self.initialized {
            self.initialized = true;
            self.first_obs_ms = Some(now_ms);
            self.last_obs_ms = Some(now_ms);
            self.equilibrium_bps = if self.config.demean_equilibrium {
                raw_dislocation_bps
            } else {
                0.0
            };
            self.sample_count = 1;
            signal_bps = if self.config.demean_equilibrium {
                0.0
            } else {
                raw_dislocation_bps
            };
        } else {
            let prev_ms = self.last_obs_ms.unwrap_or(now_ms);
            let dt_ms = now_ms - prev_ms;
            signal_bps = if self.config.demean_equilibrium {
                raw_dislocation_bps - self.equilibrium_bps
            } else {
                raw_dislocation_bps
            };
            if dt_ms > 0 {
                if self.config.demean_equilibrium {
                    let alpha = 1.0 - (-(dt_ms as f64) / (self.config.eq_ewma_tau_ms as f64)).exp();
                    self.equilibrium_bps += alpha * (raw_dislocation_bps - self.equilibrium_bps);
                }
                self.last_obs_ms = Some(now_ms);
                self.sample_count = self.sample_count.saturating_add(1);
            }
        }

        let hurdle_bps = self
            .config
            .min_dislocation_bps
            .max(self.config.spread_mult * spread_bps);

        // Open position management ALWAYS takes precedence and runs reduce-only.
        if let Some(pos) = self.position {
            let held_ms = now_ms.saturating_sub(pos.entry_ms);
            let exit_px = match pos.side {
                DefenseSide::Buy => perp_quote.bid_px,
                DefenseSide::Sell => perp_quote.ask_px,
            };
            let unrealized_bps = match pos.side {
                DefenseSide::Buy => ((exit_px / pos.entry_px) - 1.0) * BPS_SCALE,
                DefenseSide::Sell => ((pos.entry_px / exit_px) - 1.0) * BPS_SCALE,
            };

            let exit_reason = if held_ms >= self.config.max_hold_ms {
                Some(DirectionalExitReason::MaxHoldTime)
            } else if self.config.stop_loss_bps > 0.0
                && unrealized_bps <= -self.config.stop_loss_bps
            {
                Some(DirectionalExitReason::StopLoss)
            } else if self.config.take_profit_bps > 0.0
                && unrealized_bps >= self.config.take_profit_bps
            {
                Some(DirectionalExitReason::TakeProfit)
            } else if self.config.exit_on_signal_decay
                && held_ms >= self.config.min_hold_ms
                && match pos.side {
                    DefenseSide::Buy => signal_bps <= self.config.exit_dislocation_bps,
                    DefenseSide::Sell => signal_bps >= -self.config.exit_dislocation_bps,
                }
            {
                Some(DirectionalExitReason::SignalDecay)
            } else {
                None
            };

            if let Some(reason) = exit_reason {
                return OracleMomentumDecision {
                    now_ms,
                    action: DirectionalAction::Exit,
                    block_reason: None,
                    exit_reason: Some(reason),
                    intent_class: Some(OrderIntentClass::ReduceOnlyExit),
                    side: Some(pos.side),
                    entry_scale: 1.0,
                    reduce_only: true,
                    raw_dislocation_bps,
                    signal_bps,
                    hurdle_bps,
                    spread_bps,
                    unrealized_bps,
                    funding_rate_bps: ctx.funding_rate_bps,
                    sample_count: self.sample_count,
                };
            }

            return OracleMomentumDecision {
                now_ms,
                action: DirectionalAction::Hold,
                block_reason: None,
                exit_reason: None,
                intent_class: None,
                side: Some(pos.side),
                entry_scale: 0.0,
                reduce_only: false,
                raw_dislocation_bps,
                signal_bps,
                hurdle_bps,
                spread_bps,
                unrealized_bps,
                funding_rate_bps: ctx.funding_rate_bps,
                sample_count: self.sample_count,
            };
        }

        if !self.config.enabled {
            return self.blocked_decision(
                now_ms,
                DirectionalBlockReason::Disabled,
                raw_dislocation_bps,
                signal_bps,
                spread_bps,
                ctx.funding_rate_bps,
            );
        }

        let warmed = self
            .first_obs_ms
            .is_some_and(|t0| now_ms.saturating_sub(t0) >= self.config.warmup_ms)
            && self.sample_count >= self.config.min_samples;
        if !warmed {
            return self.blocked_decision(
                now_ms,
                DirectionalBlockReason::WarmingUp,
                raw_dislocation_bps,
                signal_bps,
                spread_bps,
                ctx.funding_rate_bps,
            );
        }

        if self
            .last_exit_ms
            .is_some_and(|t_ex| now_ms.saturating_sub(t_ex) < self.config.cooldown_ms)
        {
            return self.blocked_decision(
                now_ms,
                DirectionalBlockReason::Cooldown,
                raw_dislocation_bps,
                signal_bps,
                spread_bps,
                ctx.funding_rate_bps,
            );
        }

        if signal_bps.abs() >= self.config.max_dislocation_bps {
            return self.blocked_decision(
                now_ms,
                DirectionalBlockReason::StructuralBreak,
                raw_dislocation_bps,
                signal_bps,
                spread_bps,
                ctx.funding_rate_bps,
            );
        }

        if spread_bps > self.config.max_entry_spread_bps {
            return self.blocked_decision(
                now_ms,
                DirectionalBlockReason::WideSpread,
                raw_dislocation_bps,
                signal_bps,
                spread_bps,
                ctx.funding_rate_bps,
            );
        }

        // MOMENTUM: enter in the SAME direction as `mark - oracle`.
        let candidate_side = if signal_bps >= hurdle_bps {
            Some(DefenseSide::Buy)
        } else if signal_bps <= -hurdle_bps {
            Some(DefenseSide::Sell)
        } else {
            None
        };

        let Some(side) = candidate_side else {
            return OracleMomentumDecision {
                now_ms,
                action: DirectionalAction::Idle,
                block_reason: None,
                exit_reason: None,
                intent_class: None,
                side: None,
                entry_scale: 0.0,
                reduce_only: false,
                raw_dislocation_bps,
                signal_bps,
                hurdle_bps,
                spread_bps,
                unrealized_bps: 0.0,
                funding_rate_bps: ctx.funding_rate_bps,
                sample_count: self.sample_count,
            };
        };

        if self.config.require_funding_alignment {
            let funding_ok = match side {
                DefenseSide::Buy => ctx.funding_rate_bps <= self.config.max_adverse_funding_bps,
                DefenseSide::Sell => ctx.funding_rate_bps >= -self.config.max_adverse_funding_bps,
            };
            if !funding_ok {
                return self.blocked_decision(
                    now_ms,
                    DirectionalBlockReason::AdverseFunding,
                    raw_dislocation_bps,
                    signal_bps,
                    spread_bps,
                    ctx.funding_rate_bps,
                );
            }
        }

        if !defense.permits_intent(OrderIntentClass::RiskIncreasingEntry, Some(side)) {
            return self.blocked_decision(
                now_ms,
                DirectionalBlockReason::DefenseBlocked,
                raw_dislocation_bps,
                signal_bps,
                spread_bps,
                ctx.funding_rate_bps,
            );
        }

        let entry_scale = defense
            .entry_size_scale(self.config.caution_size_scale, Some(side))
            .clamp(0.0, 1.0);
        if entry_scale <= 0.0 {
            return self.blocked_decision(
                now_ms,
                DirectionalBlockReason::DefenseBlocked,
                raw_dislocation_bps,
                signal_bps,
                spread_bps,
                ctx.funding_rate_bps,
            );
        }

        let entry_px = match side {
            DefenseSide::Buy => perp_quote.ask_px,
            DefenseSide::Sell => perp_quote.bid_px,
        };
        self.position = Some(DirectionalPosition {
            side,
            entry_ms: now_ms,
            entry_px,
            entry_signal_bps: signal_bps,
        });

        OracleMomentumDecision {
            now_ms,
            action: DirectionalAction::Enter,
            block_reason: None,
            exit_reason: None,
            intent_class: Some(OrderIntentClass::RiskIncreasingEntry),
            side: Some(side),
            entry_scale,
            reduce_only: false,
            raw_dislocation_bps,
            signal_bps,
            hurdle_bps,
            spread_bps,
            unrealized_bps: 0.0,
            funding_rate_bps: ctx.funding_rate_bps,
            sample_count: self.sample_count,
        }
    }

    fn blocked_decision(
        &self,
        now_ms: u64,
        reason: DirectionalBlockReason,
        raw_dislocation_bps: f64,
        signal_bps: f64,
        spread_bps: f64,
        funding_rate_bps: f64,
    ) -> OracleMomentumDecision {
        let hurdle_bps = self
            .config
            .min_dislocation_bps
            .max(self.config.spread_mult * spread_bps);
        OracleMomentumDecision {
            now_ms,
            action: DirectionalAction::Blocked,
            block_reason: Some(reason),
            exit_reason: None,
            intent_class: None,
            side: self.position.map(|p| p.side),
            entry_scale: 0.0,
            reduce_only: false,
            raw_dislocation_bps,
            signal_bps,
            hurdle_bps,
            spread_bps,
            unrealized_bps: 0.0,
            funding_rate_bps,
            sample_count: self.sample_count,
        }
    }
}

// ============================================================================
// Engine 2: OverreactionReversion
// ============================================================================

/// Configuration for [`OverreactionReversion`]. Default-off (`enabled = false`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OverreactionReversionConfig {
    /// Master enable switch (default `false`).
    pub enabled: bool,
    /// Bar sampling interval in ms (e.g. `60_000` for 1-minute bars).
    pub bar_ms: u64,
    /// Number of completed bars in the lookback return window (`1..=MAX_OVERREACTION_BARS - 1`).
    pub lookback_bars: usize,
    /// Maximum perp BBO quote age (ms).
    pub max_perp_age_ms: u64,
    /// Maximum inter-tick gap (ms) before resetting bar history.
    pub max_gap_ms: u64,
    /// Minimum absolute lookback return (bps) required to trigger reversion entry.
    pub min_return_bps: f64,
    /// Hurdle multiplier against L1 spread: `max(min_return_bps, spread_mult * spread_bps)`.
    pub spread_mult: f64,
    /// Upper structural-break cap (`|ret_bps| >= max_return_bps` blocks entry).
    pub max_return_bps: f64,
    /// Optional minimum volatility-normalized Z-score (`0.0` disables Z-score gate).
    pub min_vol_z: f64,
    /// Maximum L1 spread (bps) allowed on entry.
    pub max_entry_spread_bps: f64,
    /// Require funding alignment (`Buy` rejects `funding > max_adverse`, `Sell` rejects `funding < -max_adverse`).
    pub require_funding_alignment: bool,
    /// Maximum adverse hourly funding (bps/hr) when `require_funding_alignment` is on.
    pub max_adverse_funding_bps: f64,
    /// Target fraction of the triggering move to capture as reversion (`0.0` disables fraction TP).
    pub reversion_target_fraction: f64,
    /// Fixed take-profit floor/target in bps (`0.0` disables fixed TP).
    pub take_profit_bps: f64,
    /// Stop-loss threshold in bps (`0.0` disables price stop-loss).
    pub stop_loss_bps: f64,
    /// Maximum holding duration (ms) before mandatory reduce-only time stop.
    pub max_hold_ms: u64,
    /// Post-exit cooldown (ms) before allowing a new entry.
    pub cooldown_ms: u64,
    /// Entry notional scale applied under `DefenseState::Caution` or `DefenseState::SideDefense`.
    pub caution_size_scale: f64,
}

impl Default for OverreactionReversionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bar_ms: 60_000,
            lookback_bars: 60,
            max_perp_age_ms: 5_000,
            max_gap_ms: 180_000,
            min_return_bps: 30.0,
            spread_mult: 2.0,
            max_return_bps: 500.0,
            min_vol_z: 0.0,
            max_entry_spread_bps: 20.0,
            require_funding_alignment: true,
            max_adverse_funding_bps: 1.0,
            reversion_target_fraction: 0.40,
            take_profit_bps: 0.0,
            stop_loss_bps: 35.0,
            max_hold_ms: 3_600_000,
            cooldown_ms: 300_000,
            caution_size_scale: 0.50,
        }
    }
}

impl OverreactionReversionConfig {
    /// Validate finite bounds and ring-buffer capacity constraints.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1_000..=600_000).contains(&self.bar_ms),
            "overreaction.bar_ms must lie in 1_000..=600_000"
        );
        ensure!(
            self.lookback_bars >= 1 && self.lookback_bars < MAX_OVERREACTION_BARS,
            "overreaction.lookback_bars must lie in 1..MAX_OVERREACTION_BARS"
        );
        ensure!(
            self.max_perp_age_ms > 0 && self.max_gap_ms >= self.bar_ms,
            "overreaction.max_perp_age_ms > 0 and max_gap_ms >= bar_ms required"
        );
        ensure!(
            self.min_return_bps.is_finite()
                && self.min_return_bps > 0.0
                && self.spread_mult.is_finite()
                && self.spread_mult >= 0.0,
            "overreaction hurdle params must be finite and valid"
        );
        ensure!(
            self.max_return_bps.is_finite() && self.max_return_bps > self.min_return_bps,
            "overreaction.max_return_bps must be finite and > min_return_bps"
        );
        ensure!(
            self.min_vol_z.is_finite() && self.min_vol_z >= 0.0,
            "overreaction.min_vol_z must be finite and >= 0"
        );
        ensure!(
            self.max_entry_spread_bps.is_finite() && self.max_entry_spread_bps > 0.0,
            "overreaction.max_entry_spread_bps must be finite and > 0"
        );
        ensure!(
            self.max_adverse_funding_bps.is_finite(),
            "overreaction.max_adverse_funding_bps must be finite"
        );
        ensure!(
            self.reversion_target_fraction.is_finite()
                && (0.0..=2.0).contains(&self.reversion_target_fraction),
            "overreaction.reversion_target_fraction must lie in [0.0, 2.0]"
        );
        ensure!(
            self.take_profit_bps.is_finite() && self.take_profit_bps >= 0.0,
            "overreaction.take_profit_bps must be finite and >= 0"
        );
        ensure!(
            self.stop_loss_bps.is_finite() && self.stop_loss_bps >= 0.0,
            "overreaction.stop_loss_bps must be finite and >= 0"
        );
        ensure!(self.max_hold_ms > 0, "overreaction.max_hold_ms must be > 0");
        ensure!(
            self.caution_size_scale.is_finite()
                && self.caution_size_scale > 0.0
                && self.caution_size_scale <= 1.0,
            "overreaction.caution_size_scale must lie in (0.0, 1.0]"
        );
        Ok(())
    }
}

/// Deterministic decision emitted by [`OverreactionReversion::observe`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct OverreactionDecision {
    pub now_ms: u64,
    pub action: DirectionalAction,
    pub block_reason: Option<DirectionalBlockReason>,
    pub exit_reason: Option<DirectionalExitReason>,
    pub intent_class: Option<OrderIntentClass>,
    pub side: Option<DefenseSide>,
    pub entry_scale: f64,
    pub reduce_only: bool,
    pub lookback_return_bps: f64,
    pub vol_z_score: f64,
    pub hurdle_bps: f64,
    pub spread_bps: f64,
    pub unrealized_bps: f64,
    pub funding_rate_bps: f64,
    pub completed_bars: usize,
}

/// Zero-allocation causal return-overreaction mean-reversion engine.
#[derive(Debug, Clone)]
pub struct OverreactionReversion {
    config: OverreactionReversionConfig,
    last_obs_ms: Option<u64>,
    current_bar: Option<(u64, f64)>,
    closes: [f64; MAX_OVERREACTION_BARS],
    bar_rets_bps: [f64; MAX_OVERREACTION_BARS],
    head: usize,
    bar_count: usize,
    position: Option<DirectionalPosition>,
    last_exit_ms: Option<u64>,
    clock_regressions: u64,
    gap_resets: u64,
    rejected_inputs: u64,
}

impl OverreactionReversion {
    /// Construct a validated [`OverreactionReversion`] controller.
    pub fn new(config: OverreactionReversionConfig) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            config,
            last_obs_ms: None,
            current_bar: None,
            closes: [0.0; MAX_OVERREACTION_BARS],
            bar_rets_bps: [0.0; MAX_OVERREACTION_BARS],
            head: 0,
            bar_count: 0,
            position: None,
            last_exit_ms: None,
            clock_regressions: 0,
            gap_resets: 0,
            rejected_inputs: 0,
        })
    }

    pub fn config(&self) -> &OverreactionReversionConfig {
        &self.config
    }

    pub fn position(&self) -> Option<DirectionalPosition> {
        self.position
    }

    pub fn completed_bars(&self) -> usize {
        self.bar_count
    }

    pub fn clock_regressions(&self) -> u64 {
        self.clock_regressions
    }

    pub fn gap_resets(&self) -> u64 {
        self.gap_resets
    }

    pub fn rejected_inputs(&self) -> u64 {
        self.rejected_inputs
    }

    /// Clear open position state once exit execution is confirmed flat, recording cooldown anchor.
    pub fn notify_exit_filled(&mut self, exit_ms: u64) {
        self.position = None;
        self.last_exit_ms = Some(exit_ms);
    }

    /// Reset causal bar history while preserving diagnostic counters.
    pub fn reset_history(&mut self) {
        self.last_obs_ms = None;
        self.current_bar = None;
        self.head = 0;
        self.bar_count = 0;
    }

    /// Convenience wrapper reading `fsm.state()`.
    pub fn observe_with_fsm(
        &mut self,
        now_ms: u64,
        perp_quote: L1Quote,
        funding_rate_bps: f64,
        fsm: &DefenseFsm,
    ) -> OverreactionDecision {
        self.observe(now_ms, perp_quote, funding_rate_bps, fsm.state())
    }

    #[inline]
    fn push_completed_bar(&mut self, close_px: f64) {
        let prev_close = if self.bar_count > 0 {
            let prev_idx = (self.head + MAX_OVERREACTION_BARS - 1) % MAX_OVERREACTION_BARS;
            Some(self.closes[prev_idx])
        } else {
            None
        };
        let bar_ret_bps = match prev_close {
            Some(p) if p > 0.0 => ((close_px / p) - 1.0) * BPS_SCALE,
            _ => 0.0,
        };
        self.closes[self.head] = close_px;
        self.bar_rets_bps[self.head] = bar_ret_bps;
        self.head = (self.head + 1) % MAX_OVERREACTION_BARS;
        if self.bar_count < MAX_OVERREACTION_BARS {
            self.bar_count += 1;
        }
    }

    #[inline]
    fn bar_at_lag(&self, lag: usize) -> Option<f64> {
        if lag == 0 || lag > self.bar_count {
            return None;
        }
        let idx = (self.head + MAX_OVERREACTION_BARS - lag) % MAX_OVERREACTION_BARS;
        Some(self.closes[idx])
    }

    #[inline]
    fn rms_bar_vol_bps(&self) -> f64 {
        let n = self.bar_count.saturating_sub(1);
        if n == 0 {
            return MIN_SIGMA_BAR_BPS;
        }
        let mut sum_sq = 0.0;
        for lag in 1..=n {
            let idx = (self.head + MAX_OVERREACTION_BARS - lag) % MAX_OVERREACTION_BARS;
            let r = self.bar_rets_bps[idx];
            sum_sq += r * r;
        }
        (sum_sq / (n as f64)).sqrt().max(MIN_SIGMA_BAR_BPS)
    }

    /// Advance the causal `OverreactionReversion` engine by one tick.
    ///
    /// Signal convention:
    /// - `lookback_return_bps >= hurdle_bps` -> price overreacted UP -> `Sell`
    /// - `lookback_return_bps <= -hurdle_bps` -> price overreacted DOWN -> `Buy`
    pub fn observe(
        &mut self,
        now_ms: u64,
        perp_quote: L1Quote,
        funding_rate_bps: f64,
        defense: DefenseState,
    ) -> OverreactionDecision {
        if self.last_obs_ms.is_some_and(|prev| now_ms < prev) {
            self.clock_regressions = self.clock_regressions.saturating_add(1);
            return self.blocked_decision(
                now_ms,
                DirectionalBlockReason::ClockRegression,
                0.0,
                0.0,
                0.0,
                0.0,
            );
        }

        if perp_quote.ts_ms > now_ms || !funding_rate_bps.is_finite() {
            self.rejected_inputs = self.rejected_inputs.saturating_add(1);
            return self.blocked_decision(
                now_ms,
                DirectionalBlockReason::InvalidInput,
                0.0,
                0.0,
                0.0,
                0.0,
            );
        }

        let Some((perp_mid, spread_bps)) = perp_quote.mid_and_spread_bps() else {
            self.rejected_inputs = self.rejected_inputs.saturating_add(1);
            return self.blocked_decision(
                now_ms,
                DirectionalBlockReason::InvalidInput,
                0.0,
                0.0,
                0.0,
                funding_rate_bps,
            );
        };

        if (now_ms - perp_quote.ts_ms) > self.config.max_perp_age_ms {
            return self.blocked_decision(
                now_ms,
                DirectionalBlockReason::StaleQuote,
                0.0,
                0.0,
                spread_bps,
                funding_rate_bps,
            );
        }

        if self
            .last_obs_ms
            .is_some_and(|prev| now_ms - prev > self.config.max_gap_ms)
        {
            self.gap_resets = self.gap_resets.saturating_add(1);
            self.reset_history();
        }
        self.last_obs_ms = Some(now_ms);

        // Strictly completed-bar updates in fixed stack ring buffer.
        let bar_idx = now_ms / self.config.bar_ms;
        match self.current_bar {
            None => {
                self.current_bar = Some((bar_idx, perp_mid));
            }
            Some((cur_idx, cur_close)) if bar_idx > cur_idx => {
                self.push_completed_bar(cur_close);
                self.current_bar = Some((bar_idx, perp_mid));
            }
            Some((cur_idx, _)) => {
                self.current_bar = Some((cur_idx, perp_mid));
            }
        }

        let hurdle_bps = self
            .config
            .min_return_bps
            .max(self.config.spread_mult * spread_bps);

        let (lookback_return_bps, vol_z_score, has_lookback) =
            if let Some(past_px) = self.bar_at_lag(self.config.lookback_bars) {
                let ret_bps = ((perp_mid / past_px) - 1.0) * BPS_SCALE;
                let sigma_bar = self.rms_bar_vol_bps();
                let z = ret_bps / (sigma_bar * (self.config.lookback_bars as f64).sqrt());
                (ret_bps, z, ret_bps.is_finite() && z.is_finite())
            } else {
                (0.0, 0.0, false)
            };

        // Open position management ALWAYS takes precedence and runs reduce-only.
        if let Some(pos) = self.position {
            let held_ms = now_ms.saturating_sub(pos.entry_ms);
            let exit_px = match pos.side {
                DefenseSide::Buy => perp_quote.bid_px,
                DefenseSide::Sell => perp_quote.ask_px,
            };
            let unrealized_bps = match pos.side {
                DefenseSide::Buy => ((exit_px / pos.entry_px) - 1.0) * BPS_SCALE,
                DefenseSide::Sell => ((pos.entry_px / exit_px) - 1.0) * BPS_SCALE,
            };
            let rev_target_bps = self.config.reversion_target_fraction * pos.entry_signal_bps.abs();

            let exit_reason = if held_ms >= self.config.max_hold_ms {
                Some(DirectionalExitReason::MaxHoldTime)
            } else if self.config.stop_loss_bps > 0.0
                && unrealized_bps <= -self.config.stop_loss_bps
            {
                Some(DirectionalExitReason::StopLoss)
            } else if self.config.reversion_target_fraction > 0.0
                && unrealized_bps >= rev_target_bps
            {
                Some(DirectionalExitReason::MeanReversion)
            } else if self.config.take_profit_bps > 0.0
                && unrealized_bps >= self.config.take_profit_bps
            {
                Some(DirectionalExitReason::TakeProfit)
            } else {
                None
            };

            if let Some(reason) = exit_reason {
                return OverreactionDecision {
                    now_ms,
                    action: DirectionalAction::Exit,
                    block_reason: None,
                    exit_reason: Some(reason),
                    intent_class: Some(OrderIntentClass::ReduceOnlyExit),
                    side: Some(pos.side),
                    entry_scale: 1.0,
                    reduce_only: true,
                    lookback_return_bps,
                    vol_z_score,
                    hurdle_bps,
                    spread_bps,
                    unrealized_bps,
                    funding_rate_bps,
                    completed_bars: self.bar_count,
                };
            }

            return OverreactionDecision {
                now_ms,
                action: DirectionalAction::Hold,
                block_reason: None,
                exit_reason: None,
                intent_class: None,
                side: Some(pos.side),
                entry_scale: 0.0,
                reduce_only: false,
                lookback_return_bps,
                vol_z_score,
                hurdle_bps,
                spread_bps,
                unrealized_bps,
                funding_rate_bps,
                completed_bars: self.bar_count,
            };
        }

        if !self.config.enabled {
            return self.blocked_decision(
                now_ms,
                DirectionalBlockReason::Disabled,
                lookback_return_bps,
                vol_z_score,
                spread_bps,
                funding_rate_bps,
            );
        }

        if !has_lookback {
            return self.blocked_decision(
                now_ms,
                DirectionalBlockReason::WarmingUp,
                lookback_return_bps,
                vol_z_score,
                spread_bps,
                funding_rate_bps,
            );
        }

        if self
            .last_exit_ms
            .is_some_and(|t_ex| now_ms.saturating_sub(t_ex) < self.config.cooldown_ms)
        {
            return self.blocked_decision(
                now_ms,
                DirectionalBlockReason::Cooldown,
                lookback_return_bps,
                vol_z_score,
                spread_bps,
                funding_rate_bps,
            );
        }

        if lookback_return_bps.abs() >= self.config.max_return_bps {
            return self.blocked_decision(
                now_ms,
                DirectionalBlockReason::StructuralBreak,
                lookback_return_bps,
                vol_z_score,
                spread_bps,
                funding_rate_bps,
            );
        }

        if spread_bps > self.config.max_entry_spread_bps {
            return self.blocked_decision(
                now_ms,
                DirectionalBlockReason::WideSpread,
                lookback_return_bps,
                vol_z_score,
                spread_bps,
                funding_rate_bps,
            );
        }

        // REVERSION: fade extreme overreactions.
        let candidate_side = if lookback_return_bps <= -hurdle_bps {
            Some(DefenseSide::Buy)
        } else if lookback_return_bps >= hurdle_bps {
            Some(DefenseSide::Sell)
        } else {
            None
        };

        let Some(side) = candidate_side else {
            return OverreactionDecision {
                now_ms,
                action: DirectionalAction::Idle,
                block_reason: None,
                exit_reason: None,
                intent_class: None,
                side: None,
                entry_scale: 0.0,
                reduce_only: false,
                lookback_return_bps,
                vol_z_score,
                hurdle_bps,
                spread_bps,
                unrealized_bps: 0.0,
                funding_rate_bps,
                completed_bars: self.bar_count,
            };
        };

        if self.config.min_vol_z > 0.0 && vol_z_score.abs() < self.config.min_vol_z {
            return self.blocked_decision(
                now_ms,
                DirectionalBlockReason::LowVolZ,
                lookback_return_bps,
                vol_z_score,
                spread_bps,
                funding_rate_bps,
            );
        }

        if self.config.require_funding_alignment {
            let funding_ok = match side {
                DefenseSide::Buy => funding_rate_bps <= self.config.max_adverse_funding_bps,
                DefenseSide::Sell => funding_rate_bps >= -self.config.max_adverse_funding_bps,
            };
            if !funding_ok {
                return self.blocked_decision(
                    now_ms,
                    DirectionalBlockReason::AdverseFunding,
                    lookback_return_bps,
                    vol_z_score,
                    spread_bps,
                    funding_rate_bps,
                );
            }
        }

        if !defense.permits_intent(OrderIntentClass::RiskIncreasingEntry, Some(side)) {
            return self.blocked_decision(
                now_ms,
                DirectionalBlockReason::DefenseBlocked,
                lookback_return_bps,
                vol_z_score,
                spread_bps,
                funding_rate_bps,
            );
        }

        let entry_scale = defense
            .entry_size_scale(self.config.caution_size_scale, Some(side))
            .clamp(0.0, 1.0);
        if entry_scale <= 0.0 {
            return self.blocked_decision(
                now_ms,
                DirectionalBlockReason::DefenseBlocked,
                lookback_return_bps,
                vol_z_score,
                spread_bps,
                funding_rate_bps,
            );
        }

        let entry_px = match side {
            DefenseSide::Buy => perp_quote.ask_px,
            DefenseSide::Sell => perp_quote.bid_px,
        };
        self.position = Some(DirectionalPosition {
            side,
            entry_ms: now_ms,
            entry_px,
            entry_signal_bps: lookback_return_bps,
        });

        OverreactionDecision {
            now_ms,
            action: DirectionalAction::Enter,
            block_reason: None,
            exit_reason: None,
            intent_class: Some(OrderIntentClass::RiskIncreasingEntry),
            side: Some(side),
            entry_scale,
            reduce_only: false,
            lookback_return_bps,
            vol_z_score,
            hurdle_bps,
            spread_bps,
            unrealized_bps: 0.0,
            funding_rate_bps,
            completed_bars: self.bar_count,
        }
    }

    fn blocked_decision(
        &self,
        now_ms: u64,
        reason: DirectionalBlockReason,
        lookback_return_bps: f64,
        vol_z_score: f64,
        spread_bps: f64,
        funding_rate_bps: f64,
    ) -> OverreactionDecision {
        let hurdle_bps = self
            .config
            .min_return_bps
            .max(self.config.spread_mult * spread_bps);
        OverreactionDecision {
            now_ms,
            action: DirectionalAction::Blocked,
            block_reason: Some(reason),
            exit_reason: None,
            intent_class: None,
            side: self.position.map(|p| p.side),
            entry_scale: 0.0,
            reduce_only: false,
            lookback_return_bps,
            vol_z_score,
            hurdle_bps,
            spread_bps,
            unrealized_bps: 0.0,
            funding_rate_bps,
            completed_bars: self.bar_count,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strategy::defense_fsm::{BlockedSide, DefenseFsmConfig, DefenseTelemetrySnapshot};

    fn mk_quote(ts_ms: u64, mid: f64, half_spr_bps: f64) -> L1Quote {
        let half = mid * half_spr_bps * 1e-4;
        L1Quote {
            ts_ms,
            bid_px: mid - half,
            bid_sz: 10.0,
            ask_px: mid + half,
            ask_sz: 10.0,
        }
    }

    fn mom_cfg() -> OracleDiscoveryMomentumConfig {
        OracleDiscoveryMomentumConfig {
            enabled: true,
            use_mark_px: true,
            demean_equilibrium: false,
            eq_ewma_tau_ms: 60_000,
            warmup_ms: 4_000,
            min_samples: 4,
            max_perp_age_ms: 5_000,
            max_ctx_age_ms: 10_000,
            max_gap_ms: 30_000,
            min_dislocation_bps: 15.0,
            spread_mult: 1.5,
            max_dislocation_bps: 200.0,
            max_entry_spread_bps: 15.0,
            require_funding_alignment: true,
            max_adverse_funding_bps: 0.5,
            take_profit_bps: 12.0,
            stop_loss_bps: 20.0,
            exit_on_signal_decay: false,
            exit_dislocation_bps: 3.0,
            min_hold_ms: 5_000,
            max_hold_ms: 60_000,
            cooldown_ms: 10_000,
            caution_size_scale: 0.40,
        }
    }

    fn rev_cfg() -> OverreactionReversionConfig {
        OverreactionReversionConfig {
            enabled: true,
            bar_ms: 60_000,
            lookback_bars: 5,
            max_perp_age_ms: 5_000,
            max_gap_ms: 180_000,
            min_return_bps: 30.0,
            spread_mult: 2.0,
            max_return_bps: 300.0,
            min_vol_z: 0.0,
            max_entry_spread_bps: 20.0,
            require_funding_alignment: true,
            max_adverse_funding_bps: 0.5,
            reversion_target_fraction: 0.40,
            take_profit_bps: 0.0,
            stop_loss_bps: 35.0,
            max_hold_ms: 300_000,
            cooldown_ms: 60_000,
            caution_size_scale: 0.50,
        }
    }

    #[test]
    fn default_configs_are_disabled_and_invalid_configs_fail_closed() {
        let m_def = OracleDiscoveryMomentumConfig::default();
        assert!(!m_def.enabled);
        assert!(m_def.validate().is_ok());
        let mut m_bad = m_def;
        m_bad.min_dislocation_bps = f64::NAN;
        assert!(m_bad.validate().is_err());
        let mut m_bad2 = m_def;
        m_bad2.max_dislocation_bps = m_bad2.min_dislocation_bps;
        assert!(m_bad2.validate().is_err());

        let r_def = OverreactionReversionConfig::default();
        assert!(!r_def.enabled);
        assert!(r_def.validate().is_ok());
        let mut r_bad = r_def;
        r_bad.lookback_bars = MAX_OVERREACTION_BARS;
        assert!(r_bad.validate().is_err());
        let mut r_bad2 = r_def;
        r_bad2.min_return_bps = -5.0;
        assert!(r_bad2.validate().is_err());

        // Default-off engines block entries even on strong signals.
        let mut eng_m = OracleDiscoveryMomentum::new(m_def).unwrap();
        let mut ts = 10_000_u64;
        for _ in 0..10 {
            ts += 20_000;
            let d = eng_m.observe(
                ts,
                mk_quote(ts, 100.2, 1.0),
                OracleCtxSnapshot {
                    ts_ms: ts,
                    mark_px: 100.2,
                    oracle_px: 100.0,
                    funding_rate_bps: 0.0,
                },
                DefenseState::Normal,
            );
            assert_eq!(d.action, DirectionalAction::Blocked);
            assert_eq!(d.block_reason, Some(DirectionalBlockReason::Disabled));
        }
    }

    #[test]
    fn oracle_discovery_momentum_follows_mark_vs_oracle_and_exits_reduce_only() {
        let mut eng = OracleDiscoveryMomentum::new(mom_cfg()).unwrap();
        let mut ts = 1_000_u64;
        for _ in 0..5 {
            ts += 1_000;
            let d = eng.observe(
                ts,
                mk_quote(ts, 100.0, 1.0),
                OracleCtxSnapshot {
                    ts_ms: ts,
                    mark_px: 100.0,
                    oracle_px: 100.0,
                    funding_rate_bps: 0.0,
                },
                DefenseState::Normal,
            );
            assert!(d.action == DirectionalAction::Idle || d.action == DirectionalAction::Blocked);
        }

        // mark > oracle by +20 bps (hurdle = 15 bps) -> momentum BUY (not Sell!)
        ts += 1_000;
        let ent = eng.observe(
            ts,
            mk_quote(ts, 100.20, 1.0),
            OracleCtxSnapshot {
                ts_ms: ts,
                mark_px: 100.20,
                oracle_px: 100.00,
                funding_rate_bps: 0.1,
            },
            DefenseState::Normal,
        );
        assert_eq!(ent.action, DirectionalAction::Enter);
        assert_eq!(ent.side, Some(DefenseSide::Buy));
        assert_eq!(
            ent.intent_class,
            Some(OrderIntentClass::RiskIncreasingEntry)
        );
        assert!(!ent.reduce_only);
        assert!((ent.entry_scale - 1.0).abs() < 1e-12);

        // Discovery continues upward (+15 bps above entry) -> TakeProfit ReduceOnlyExit even under Halt!
        ts += 5_000;
        let ex = eng.observe(
            ts,
            mk_quote(ts, 100.36, 1.0),
            OracleCtxSnapshot {
                ts_ms: ts,
                mark_px: 100.36,
                oracle_px: 100.20,
                funding_rate_bps: 0.1,
            },
            DefenseState::Halt,
        );
        assert_eq!(ex.action, DirectionalAction::Exit);
        assert_eq!(ex.exit_reason, Some(DirectionalExitReason::TakeProfit));
        assert_eq!(ex.intent_class, Some(OrderIntentClass::ReduceOnlyExit));
        assert!(ex.reduce_only);
        eng.notify_exit_filled(ts);
        assert!(eng.position().is_none());

        // Cooldown blocks immediate re-entry before cooldown_ms elapses
        ts += 2_000;
        let cd = eng.observe(
            ts,
            mk_quote(ts, 100.36, 1.0),
            OracleCtxSnapshot {
                ts_ms: ts,
                mark_px: 100.36,
                oracle_px: 100.00,
                funding_rate_bps: 0.0,
            },
            DefenseState::Normal,
        );
        assert_eq!(cd.action, DirectionalAction::Blocked);
        assert_eq!(cd.block_reason, Some(DirectionalBlockReason::Cooldown));
    }

    #[test]
    fn overreaction_reversion_fades_60m_style_spike_and_takes_mean_reversion_exit() {
        let mut eng = OverreactionReversion::new(rev_cfg()).unwrap();
        let mut ts = 60_000_u64;
        for _ in 0..7 {
            let d = eng.observe(ts, mk_quote(ts, 100.0, 1.0), 0.0, DefenseState::Normal);
            assert!(d.action == DirectionalAction::Idle || d.action == DirectionalAction::Blocked);
            ts += 60_000;
        }
        assert!(eng.completed_bars() >= 6);

        // Price crashes -40 bps -> OverreactionReversion buys to fade crash
        let ent = eng.observe(ts, mk_quote(ts, 99.60, 1.0), -0.2, DefenseState::Normal);
        assert_eq!(ent.action, DirectionalAction::Enter);
        assert_eq!(ent.side, Some(DefenseSide::Buy));
        assert!(ent.lookback_return_bps <= -30.0);
        assert_eq!(
            ent.intent_class,
            Some(OrderIntentClass::RiskIncreasingEntry)
        );

        // Partial reversion of >= 40% of 40 bps = +16 bps -> MeanReversion ReduceOnlyExit
        ts += 60_000;
        let ex = eng.observe(ts, mk_quote(ts, 99.80, 1.0), -0.2, DefenseState::ReduceOnly);
        assert_eq!(ex.action, DirectionalAction::Exit);
        assert_eq!(ex.exit_reason, Some(DirectionalExitReason::MeanReversion));
        assert_eq!(ex.intent_class, Some(OrderIntentClass::ReduceOnlyExit));
        assert!(ex.reduce_only);
        eng.notify_exit_filled(ts);
        assert!(eng.position().is_none());
    }

    #[test]
    fn defense_fsm_scales_caution_blocks_side_and_permits_reduce_only_exits() {
        let mut fsm = DefenseFsm::new(DefenseFsmConfig {
            caution_size_scale: 0.40,
            ..DefenseFsmConfig::default()
        })
        .unwrap();
        let mut eng = OracleDiscoveryMomentum::new(mom_cfg()).unwrap();

        let mut ts = 10_000_u64;
        for _ in 0..5 {
            ts += 1_000;
            let _ = fsm.evaluate(&DefenseTelemetrySnapshot {
                now_ms: ts,
                vol_shock_ratio: 1.0,
                ..DefenseTelemetrySnapshot::default()
            });
            let _ = eng.observe_with_fsm(
                ts,
                mk_quote(ts, 100.0, 1.0),
                OracleCtxSnapshot {
                    ts_ms: ts,
                    mark_px: 100.0,
                    oracle_px: 100.0,
                    funding_rate_bps: 0.0,
                },
                &fsm,
            );
        }

        // SideDefense blocking Buy rejects upward discovery Buy entry
        ts += 1_000;
        let blocked = eng.observe(
            ts,
            mk_quote(ts, 100.25, 1.0),
            OracleCtxSnapshot {
                ts_ms: ts,
                mark_px: 100.25,
                oracle_px: 100.00,
                funding_rate_bps: 0.0,
            },
            DefenseState::SideDefense {
                blocked_side: BlockedSide::Buy,
            },
        );
        assert_eq!(blocked.action, DirectionalAction::Blocked);
        assert_eq!(
            blocked.block_reason,
            Some(DirectionalBlockReason::DefenseBlocked)
        );

        // Caution scales exposure down to 0.40
        ts += 1_000;
        let _ = fsm.evaluate(&DefenseTelemetrySnapshot {
            now_ms: ts,
            vol_shock_ratio: 2.5,
            ..DefenseTelemetrySnapshot::default()
        });
        assert_eq!(fsm.state(), DefenseState::Caution);
        let scaled = eng.observe_with_fsm(
            ts,
            mk_quote(ts, 100.25, 1.0),
            OracleCtxSnapshot {
                ts_ms: ts,
                mark_px: 100.25,
                oracle_px: 100.00,
                funding_rate_bps: 0.0,
            },
            &fsm,
        );
        assert_eq!(scaled.action, DirectionalAction::Enter);
        assert!((scaled.entry_scale - 0.40).abs() < 1e-12);

        // Escalate FSM to Halt; max_hold_ms still fires ReduceOnlyExit!
        ts += 60_000;
        let _ = fsm.evaluate(&DefenseTelemetrySnapshot {
            now_ms: ts,
            vol_shock_ratio: 10.0,
            ..DefenseTelemetrySnapshot::default()
        });
        assert_eq!(fsm.state(), DefenseState::Halt);
        // Advance intermediate ticks <= max_gap_ms to avoid gap reset
        let mut cur_t = scaled.now_ms;
        while cur_t + 20_000 < ts {
            cur_t += 20_000;
            let _ = eng.observe(
                cur_t,
                mk_quote(cur_t, 100.25, 1.0),
                OracleCtxSnapshot {
                    ts_ms: cur_t,
                    mark_px: 100.25,
                    oracle_px: 100.00,
                    funding_rate_bps: 0.0,
                },
                DefenseState::Halt,
            );
        }
        let ex = eng.observe_with_fsm(
            ts,
            mk_quote(ts, 100.25, 1.0),
            OracleCtxSnapshot {
                ts_ms: ts,
                mark_px: 100.25,
                oracle_px: 100.00,
                funding_rate_bps: 0.0,
            },
            &fsm,
        );
        assert_eq!(ex.action, DirectionalAction::Exit);
        assert_eq!(ex.exit_reason, Some(DirectionalExitReason::MaxHoldTime));
        assert_eq!(ex.intent_class, Some(OrderIntentClass::ReduceOnlyExit));
        assert!(ex.reduce_only);
    }

    #[test]
    fn fail_closed_on_stale_non_monotonic_and_nan_inputs() {
        let mut eng_m = OracleDiscoveryMomentum::new(mom_cfg()).unwrap();
        let mut eng_r = OverreactionReversion::new(rev_cfg()).unwrap();

        let t0 = 100_000_u64;
        let _ = eng_m.observe(
            t0,
            mk_quote(t0, 100.0, 1.0),
            OracleCtxSnapshot {
                ts_ms: t0,
                mark_px: 100.0,
                oracle_px: 100.0,
                funding_rate_bps: 0.0,
            },
            DefenseState::Normal,
        );
        let _ = eng_r.observe(t0, mk_quote(t0, 100.0, 1.0), 0.0, DefenseState::Normal);

        // 1. Backward clock -> ClockRegression
        let reg_m = eng_m.observe(
            t0 - 1,
            mk_quote(t0 - 1, 100.0, 1.0),
            OracleCtxSnapshot {
                ts_ms: t0 - 1,
                mark_px: 100.0,
                oracle_px: 100.0,
                funding_rate_bps: 0.0,
            },
            DefenseState::Normal,
        );
        assert_eq!(reg_m.action, DirectionalAction::Blocked);
        assert_eq!(
            reg_m.block_reason,
            Some(DirectionalBlockReason::ClockRegression)
        );
        assert_eq!(eng_m.clock_regressions(), 1);

        let reg_r = eng_r.observe(
            t0 - 1,
            mk_quote(t0 - 1, 100.0, 1.0),
            0.0,
            DefenseState::Normal,
        );
        assert_eq!(reg_r.action, DirectionalAction::Blocked);
        assert_eq!(
            reg_r.block_reason,
            Some(DirectionalBlockReason::ClockRegression)
        );
        assert_eq!(eng_r.clock_regressions(), 1);

        // 2. NaN / Inf / crossed prices -> InvalidInput
        let nan_q = L1Quote {
            ts_ms: t0 + 1_000,
            bid_px: f64::NAN,
            bid_sz: 5.0,
            ask_px: 100.1,
            ask_sz: 5.0,
        };
        let inv_m = eng_m.observe(
            t0 + 1_000,
            nan_q,
            OracleCtxSnapshot {
                ts_ms: t0 + 1_000,
                mark_px: 100.0,
                oracle_px: 100.0,
                funding_rate_bps: 0.0,
            },
            DefenseState::Normal,
        );
        assert_eq!(
            inv_m.block_reason,
            Some(DirectionalBlockReason::InvalidInput)
        );
        let inv_r = eng_r.observe(t0 + 1_000, nan_q, f64::INFINITY, DefenseState::Normal);
        assert_eq!(
            inv_r.block_reason,
            Some(DirectionalBlockReason::InvalidInput)
        );

        // 3. Stale quote & stale context -> StaleQuote / StaleContext
        let t_late = t0 + 12_000;
        let stale_q = eng_m.observe(
            t_late,
            mk_quote(t0, 100.3, 1.0),
            OracleCtxSnapshot {
                ts_ms: t_late,
                mark_px: 100.3,
                oracle_px: 100.0,
                funding_rate_bps: 0.0,
            },
            DefenseState::Normal,
        );
        assert_eq!(
            stale_q.block_reason,
            Some(DirectionalBlockReason::StaleQuote)
        );

        let stale_ctx = eng_m.observe(
            t_late,
            mk_quote(t_late, 100.3, 1.0),
            OracleCtxSnapshot {
                ts_ms: t0,
                mark_px: 100.3,
                oracle_px: 100.0,
                funding_rate_bps: 0.0,
            },
            DefenseState::Normal,
        );
        assert_eq!(
            stale_ctx.block_reason,
            Some(DirectionalBlockReason::StaleContext)
        );
        let stale_r = eng_r.observe(t_late, mk_quote(t0, 100.0, 1.0), 0.0, DefenseState::Normal);
        assert_eq!(
            stale_r.block_reason,
            Some(DirectionalBlockReason::StaleQuote)
        );
    }

    #[test]
    fn funding_alignment_and_stop_loss_fire_deterministically() {
        let mut eng = OracleDiscoveryMomentum::new(mom_cfg()).unwrap();
        let mut ts = 1_000_u64;
        for _ in 0..5 {
            ts += 1_000;
            let _ = eng.observe(
                ts,
                mk_quote(ts, 100.0, 1.0),
                OracleCtxSnapshot {
                    ts_ms: ts,
                    mark_px: 100.0,
                    oracle_px: 100.0,
                    funding_rate_bps: 0.0,
                },
                DefenseState::Normal,
            );
        }

        // Adverse positive funding > 0.5 bps/hr blocks Buy momentum
        ts += 1_000;
        let blk = eng.observe(
            ts,
            mk_quote(ts, 100.20, 1.0),
            OracleCtxSnapshot {
                ts_ms: ts,
                mark_px: 100.20,
                oracle_px: 100.0,
                funding_rate_bps: 1.5,
            },
            DefenseState::Normal,
        );
        assert_eq!(blk.action, DirectionalAction::Blocked);
        assert_eq!(
            blk.block_reason,
            Some(DirectionalBlockReason::AdverseFunding)
        );

        // Permissive funding enters Sell momentum on negative mark-oracle
        ts += 1_000;
        let ent = eng.observe(
            ts,
            mk_quote(ts, 99.80, 1.0),
            OracleCtxSnapshot {
                ts_ms: ts,
                mark_px: 99.80,
                oracle_px: 100.0,
                funding_rate_bps: 0.2,
            },
            DefenseState::Normal,
        );
        assert_eq!(ent.action, DirectionalAction::Enter);
        assert_eq!(ent.side, Some(DefenseSide::Sell));

        // Price rips up +25 bps (> stop_loss_bps = 20 bps) -> StopLoss ReduceOnlyExit
        ts += 2_000;
        let sl = eng.observe(
            ts,
            mk_quote(ts, 100.05, 1.0),
            OracleCtxSnapshot {
                ts_ms: ts,
                mark_px: 100.05,
                oracle_px: 100.0,
                funding_rate_bps: 0.2,
            },
            DefenseState::Normal,
        );
        assert_eq!(sl.action, DirectionalAction::Exit);
        assert_eq!(sl.exit_reason, Some(DirectionalExitReason::StopLoss));
        assert!(sl.reduce_only);
    }

    #[test]
    fn gap_reset_reenters_warmup_and_future_timestamp_fails_closed() {
        let mut eng = OracleDiscoveryMomentum::new(mom_cfg()).unwrap();
        let mut ts = 10_000_u64;
        for _ in 0..5 {
            ts += 1_000;
            let _ = eng.observe(
                ts,
                mk_quote(ts, 100.0, 1.0),
                OracleCtxSnapshot {
                    ts_ms: ts,
                    mark_px: 100.0,
                    oracle_px: 100.0,
                    funding_rate_bps: 0.0,
                },
                DefenseState::Normal,
            );
        }

        // Future quote timestamp fails closed without resetting filter
        let fut = eng.observe(
            ts + 1_000,
            mk_quote(ts + 5_000, 100.3, 1.0),
            OracleCtxSnapshot {
                ts_ms: ts + 1_000,
                mark_px: 100.3,
                oracle_px: 100.0,
                funding_rate_bps: 0.0,
            },
            DefenseState::Normal,
        );
        assert_eq!(fut.action, DirectionalAction::Blocked);
        assert_eq!(fut.block_reason, Some(DirectionalBlockReason::InvalidInput));
        assert_eq!(eng.rejected_inputs(), 1);

        // Gap > max_gap_ms (30_000) resets warmup
        ts += 40_000;
        let gap_d = eng.observe(
            ts,
            mk_quote(ts, 100.3, 1.0),
            OracleCtxSnapshot {
                ts_ms: ts,
                mark_px: 100.3,
                oracle_px: 100.0,
                funding_rate_bps: 0.0,
            },
            DefenseState::Normal,
        );
        assert_eq!(gap_d.action, DirectionalAction::Blocked);
        assert_eq!(gap_d.block_reason, Some(DirectionalBlockReason::WarmingUp));
        assert_eq!(eng.gap_resets(), 1);
    }

    #[test]
    fn overreaction_spread_hurdle_and_vol_z_gate_block_noisy_entries() {
        let mut cfg = rev_cfg();
        cfg.min_vol_z = 2.0;
        cfg.max_entry_spread_bps = 12.0;
        let mut eng = OverreactionReversion::new(cfg).unwrap();

        let mut ts = 60_000_u64;
        // High volatility bars (~25 bps per bar) so a 35 bps move over 5 bars has low Z-score
        for i in 0..8 {
            let px = if i % 2 == 0 { 100.0 } else { 100.25 };
            let _ = eng.observe(ts, mk_quote(ts, px, 1.0), 0.0, DefenseState::Normal);
            ts += 60_000;
        }

        // Wide spread (> 12 bps) fails WideSpread guard first
        let wide = eng.observe(ts, mk_quote(ts, 99.60, 7.0), 0.0, DefenseState::Normal);
        assert_eq!(wide.action, DirectionalAction::Blocked);
        assert_eq!(wide.block_reason, Some(DirectionalBlockReason::WideSpread));

        // Normal spread, -35 bps move, but high bar sigma -> LowVolZ blocks entry
        ts += 60_000;
        let low_z = eng.observe(ts, mk_quote(ts, 99.65, 1.0), 0.0, DefenseState::Normal);
        assert_eq!(low_z.action, DirectionalAction::Blocked);
        assert_eq!(low_z.block_reason, Some(DirectionalBlockReason::LowVolZ));
    }
}
