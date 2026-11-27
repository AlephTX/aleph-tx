//! Speed-bump-immune 1m–4h Statistical Arbitrage (`PairsStatArb`) and
//! Perp-vs-Oracle Dislocation Reversion (`OracleDislocationReversion`).
//!
//! Design invariants:
//! - Zero hot-path heap allocations: all rolling statistics and AR(1) / OU estimation
//!   state live in fixed-capacity stack arrays (`Copy` / inline buffers).
//! - Strictly causal & monotonic clock: backward timestamps or future quote timestamps
//!   fail closed without mutating filter state.
//! - Stale/gap fail-closed: stale feeds block new entries and gaps exceeding `max_gap_ms`
//!   reset rolling estimators and require fresh warmup.
//! - `DefenseFsm` integration & reduce-only exit safety: `RiskIncreasingEntry` scales by
//!   `DefenseState::entry_size_scale` and is blocked under `ReduceOnly`/`Halt`, whereas
//!   `ReduceOnlyExit` on open positions is ALWAYS permitted.

use crate::order_budget::OrderIntentClass;
use crate::strategy::defense_fsm::{DefenseFsm, DefenseSide, DefenseState};
use crate::strategy::kalman_basis::StationarityStatus;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

/// Maximum stack ring-buffer capacity for causal AR(1) / OU estimation.
pub const MAX_STAT_ARB_WINDOW: usize = 256;

/// Minimum residual sum-of-squares before estimating AR(1) slope `phi`.
const MIN_AR1_SUM_XX: f64 = 1e-12;

/// Minimum EWMA variance of X (`log(px_x)`) before updating OLS hedge ratio `beta_t`.
const MIN_EWMA_VAR_X: f64 = 1e-12;

/// L1 top-of-book quote snapshot (stack `Copy`, zero allocations).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct L1Quote {
    pub ts_ms: u64,
    pub bid_px: f64,
    pub bid_sz: f64,
    pub ask_px: f64,
    pub ask_sz: f64,
}

impl L1Quote {
    /// Validate quote prices/sizes and return `(mid_px, spread_bps)`.
    #[inline]
    pub fn mid_and_spread_bps(self) -> Option<(f64, f64)> {
        if !self.bid_px.is_finite()
            || !self.ask_px.is_finite()
            || self.bid_px <= 0.0
            || self.ask_px <= self.bid_px
        {
            return None;
        }
        let mid = 0.5 * (self.bid_px + self.ask_px);
        if !mid.is_finite() || mid <= 0.0 {
            return None;
        }
        let spread_bps = ((self.ask_px - self.bid_px) / mid) * 10_000.0;
        if !spread_bps.is_finite() || spread_bps < 0.0 {
            return None;
        }
        Some((mid, spread_bps))
    }
}

/// High-level action returned by [`PairsStatArb`] and [`OracleDislocationReversion`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatArbAction {
    Enter,
    Exit,
    Hold,
    Idle,
    Blocked,
}

/// Fail-closed or throttling reason when an action is blocked or suppressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatArbBlockReason {
    Disabled,
    InvalidInput,
    ClockRegression,
    StaleQuote,
    StaleOracle,
    WarmingUp,
    NonStationary,
    StructuralBreak,
    WideSpread,
    AdverseFunding,
    DefenseBlocked,
}

/// Exit trigger reason for an open stat-arb or oracle-reversion position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatArbExitReason {
    MeanReversion,
    StopLoss,
    MaxHoldTime,
    RegimeBreak,
}

/// Directional stance for a two-leg pairs trade (`Y - beta_t * X`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PairsPositionSide {
    /// Spread is depressed (`z <= -z_entry`): buy Y, sell X.
    LongYShortX,
    /// Spread is elevated (`z >= z_entry`): sell Y, buy X.
    ShortYLongX,
}

impl PairsPositionSide {
    #[inline]
    pub const fn leg_sides(self) -> (DefenseSide, DefenseSide) {
        match self {
            Self::LongYShortX => (DefenseSide::Buy, DefenseSide::Sell),
            Self::ShortYLongX => (DefenseSide::Sell, DefenseSide::Buy),
        }
    }
}

/// Configuration for [`PairsStatArb`]. All strategy knobs are default-off (`enabled = false`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PairsStatArbConfig {
    /// Master enable switch (default `false` per safe rollout invariant).
    pub enabled: bool,
    /// Causal EWMA time constant (ms) for online OLS hedge ratio `beta_t` and spread vol.
    pub ewma_tau_ms: u64,
    /// Minimum elapsed continuous observation time (ms) before allowing entry.
    pub warmup_ms: u64,
    /// Minimum distinct-timestamp samples before leaving `WarmingUp` (`8..=MAX_STAT_ARB_WINDOW`).
    pub min_samples: usize,
    /// Stack ring-buffer length for causal AR(1) persistence estimation (`8..=MAX_STAT_ARB_WINDOW`).
    pub ou_window: usize,
    /// Minimum elapsed time (ms) between consecutive AR(1) ring-buffer samples (`>= 1`).
    pub ar_sample_ms: u64,
    /// Maximum allowed L1 quote age (ms) on either leg before failing closed.
    pub max_quote_age_ms: u64,
    /// Maximum inter-tick gap (ms) before resetting rolling EWMA/AR state.
    pub max_gap_ms: u64,
    /// Minimum permitted hedge ratio `beta_t` (`> 0.0`).
    pub beta_min: f64,
    /// Maximum permitted hedge ratio `beta_t` (`>= beta_min`).
    pub beta_max: f64,
    /// Entry Z-score threshold (`> z_exit`).
    pub z_entry: f64,
    /// Convergence exit Z-score band (`>= 0.0` and `< z_entry`).
    pub z_exit: f64,
    /// Stop-loss / structural-break Z-score cap (`> z_entry`).
    pub z_stop: f64,
    /// Maximum AR(1) persistence `phi` considered stationary (`0.5 < phi_max_stationary < 1.0`).
    pub phi_max_stationary: f64,
    /// Maximum holding duration (ms) before mandatory reduce-only time stop.
    pub max_hold_ms: u64,
    /// Floor on spread volatility (bps) to avoid extreme Z-scores during flat intervals.
    pub min_spread_vol_bps: f64,
    /// Maximum combined L1 spread (`spread_y_bps + beta * spread_x_bps`) allowed on entry.
    pub max_combined_spread_bps: f64,
    /// Entry notional scale factor applied when `DefenseState` is `Caution` or `SideDefense`.
    pub caution_size_scale: f64,
}

impl Default for PairsStatArbConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            ewma_tau_ms: 3_600_000,
            warmup_ms: 900_000,
            min_samples: 30,
            ou_window: 64,
            ar_sample_ms: 10_000,
            max_quote_age_ms: 5_000,
            max_gap_ms: 30_000,
            beta_min: 0.10,
            beta_max: 5.00,
            z_entry: 2.50,
            z_exit: 0.25,
            z_stop: 4.50,
            phi_max_stationary: 0.985,
            max_hold_ms: 14_400_000,
            min_spread_vol_bps: 2.0,
            max_combined_spread_bps: 25.0,
            caution_size_scale: 0.50,
        }
    }
}

impl PairsStatArbConfig {
    /// Fail closed on non-finite parameters or inconsistent thresholds.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.ewma_tau_ms >= 1_000,
            "pairs_stat_arb.ewma_tau_ms must be >= 1000"
        );
        ensure!(
            self.max_quote_age_ms > 0,
            "pairs_stat_arb.max_quote_age_ms must be > 0"
        );
        ensure!(self.max_gap_ms > 0, "pairs_stat_arb.max_gap_ms must be > 0");
        ensure!(
            self.ar_sample_ms > 0,
            "pairs_stat_arb.ar_sample_ms must be > 0"
        );
        ensure!(
            (8..=MAX_STAT_ARB_WINDOW).contains(&self.ou_window),
            "pairs_stat_arb.ou_window must be in 8..={MAX_STAT_ARB_WINDOW}"
        );
        ensure!(
            (4..=self.ou_window).contains(&self.min_samples),
            "pairs_stat_arb.min_samples must be in 4..=ou_window"
        );
        ensure!(
            self.beta_min.is_finite()
                && self.beta_max.is_finite()
                && self.beta_min > 0.0
                && self.beta_max >= self.beta_min,
            "pairs_stat_arb beta bounds must satisfy 0 < beta_min <= beta_max"
        );
        ensure!(
            self.z_exit.is_finite()
                && self.z_entry.is_finite()
                && self.z_stop.is_finite()
                && self.z_exit >= 0.0
                && self.z_entry > self.z_exit
                && self.z_stop > self.z_entry,
            "pairs_stat_arb Z thresholds must satisfy 0 <= z_exit < z_entry < z_stop"
        );
        ensure!(
            self.phi_max_stationary.is_finite()
                && self.phi_max_stationary > 0.5
                && self.phi_max_stationary < 1.0,
            "pairs_stat_arb.phi_max_stationary must lie strictly in (0.5, 1.0)"
        );
        ensure!(
            self.max_hold_ms > 0,
            "pairs_stat_arb.max_hold_ms must be > 0"
        );
        ensure!(
            self.min_spread_vol_bps.is_finite() && self.min_spread_vol_bps > 0.0,
            "pairs_stat_arb.min_spread_vol_bps must be finite and > 0"
        );
        ensure!(
            self.max_combined_spread_bps.is_finite() && self.max_combined_spread_bps > 0.0,
            "pairs_stat_arb.max_combined_spread_bps must be finite and > 0"
        );
        ensure!(
            self.caution_size_scale.is_finite()
                && self.caution_size_scale > 0.0
                && self.caution_size_scale <= 1.0,
            "pairs_stat_arb.caution_size_scale must lie in (0.0, 1.0]"
        );
        Ok(())
    }
}

/// Active open position metadata for [`PairsStatArb`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PairsPosition {
    pub side: PairsPositionSide,
    pub entry_ms: u64,
    pub entry_beta: f64,
    pub entry_z: f64,
}

/// Deterministic decision snapshot emitted by [`PairsStatArb::observe`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct PairsStatArbDecision {
    pub now_ms: u64,
    pub action: StatArbAction,
    pub block_reason: Option<StatArbBlockReason>,
    pub exit_reason: Option<StatArbExitReason>,
    pub intent_class: Option<OrderIntentClass>,
    pub side: Option<PairsPositionSide>,
    pub entry_scale: f64,
    pub reduce_only: bool,
    pub beta: f64,
    pub spread_bps: f64,
    pub spread_vol_bps: f64,
    pub z_score: f64,
    pub combined_spread_bps: f64,
    pub phi: Option<f64>,
    pub half_life_ms: Option<f64>,
    pub stationarity: StationarityStatus,
    pub sample_count: usize,
}

/// Zero-allocation stack ring buffer for causal AR(1) / OU estimation.
#[derive(Debug, Clone, Copy)]
struct Ar1Ring {
    window: usize,
    residuals: [f64; MAX_STAT_ARB_WINDOW],
    dt_secs: [f64; MAX_STAT_ARB_WINDOW],
    head: usize,
    len: usize,
    last_sample_ms: Option<u64>,
}

impl Ar1Ring {
    fn new(window: usize) -> Self {
        Self {
            window,
            residuals: [0.0; MAX_STAT_ARB_WINDOW],
            dt_secs: [0.0; MAX_STAT_ARB_WINDOW],
            head: 0,
            len: 0,
            last_sample_ms: None,
        }
    }

    fn reset(&mut self) {
        self.head = 0;
        self.len = 0;
        self.last_sample_ms = None;
    }

    fn maybe_push(&mut self, now_ms: u64, value: f64, min_cadence_ms: u64) {
        if !value.is_finite() {
            return;
        }
        let dt_sec = match self.last_sample_ms {
            None => 0.0,
            Some(prev) => {
                let dt_ms = now_ms.saturating_sub(prev);
                if dt_ms < min_cadence_ms {
                    return;
                }
                (dt_ms as f64) / 1000.0
            }
        };
        self.residuals[self.head] = value;
        self.dt_secs[self.head] = dt_sec;
        self.head = (self.head + 1) % self.window;
        if self.len < self.window {
            self.len += 1;
        }
        self.last_sample_ms = Some(now_ms);
    }

    fn estimate(
        &self,
        min_samples: usize,
        phi_max_stationary: f64,
    ) -> (Option<f64>, Option<f64>, StationarityStatus) {
        let n = self.len;
        if n < min_samples || n < 2 {
            return (None, None, StationarityStatus::WarmingUp);
        }
        let w = self.window;
        let oldest = (self.head + w - n) % w;

        let mut sum_xx = 0.0_f64;
        let mut sum_xy = 0.0_f64;
        let mut sum_dt = 0.0_f64;
        let mut pair_count = 0usize;

        for i in 1..n {
            let prev_idx = (oldest + i - 1) % w;
            let curr_idx = (oldest + i) % w;
            let x_prev = self.residuals[prev_idx];
            let x_curr = self.residuals[curr_idx];
            let dt_sec = self.dt_secs[curr_idx];
            if dt_sec > 0.0 && x_prev.is_finite() && x_curr.is_finite() && dt_sec.is_finite() {
                sum_xx += x_prev * x_prev;
                sum_xy += x_prev * x_curr;
                sum_dt += dt_sec;
                pair_count += 1;
            }
        }

        if pair_count == 0
            || !sum_xx.is_finite()
            || !sum_xy.is_finite()
            || !sum_dt.is_finite()
            || sum_dt <= 0.0
            || sum_xx <= MIN_AR1_SUM_XX
        {
            return (None, None, StationarityStatus::WarmingUp);
        }

        let phi = sum_xy / sum_xx;
        if !phi.is_finite() || phi <= 0.0 || phi >= phi_max_stationary {
            return (
                phi.is_finite().then_some(phi),
                None,
                StationarityStatus::NonStationary,
            );
        }

        let mean_dt_sec = sum_dt / (pair_count as f64);
        let theta_per_sec = -phi.ln() / mean_dt_sec;
        let half_life_ms = (std::f64::consts::LN_2 / theta_per_sec) * 1000.0;
        if !theta_per_sec.is_finite()
            || theta_per_sec <= 0.0
            || !half_life_ms.is_finite()
            || half_life_ms <= 0.0
        {
            return (Some(phi), None, StationarityStatus::NonStationary);
        }

        (
            Some(phi),
            Some(half_life_ms),
            StationarityStatus::Stationary,
        )
    }
}

/// Zero-allocation causal online EWMA OLS pairs statistical arbitrage engine.
#[derive(Debug, Clone)]
pub struct PairsStatArb {
    config: PairsStatArbConfig,
    initialized: bool,
    first_obs_ms: Option<u64>,
    last_obs_ms: Option<u64>,
    mu_y: f64,
    mu_x: f64,
    var_x: f64,
    cov_xy: f64,
    beta: f64,
    spread_var_bps2: f64,
    ar_ring: Ar1Ring,
    position: Option<PairsPosition>,
    clock_regressions: u64,
    gap_resets: u64,
    rejected_inputs: u64,
}

impl PairsStatArb {
    /// Construct a validated [`PairsStatArb`] instance.
    pub fn new(config: PairsStatArbConfig) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            config,
            initialized: false,
            first_obs_ms: None,
            last_obs_ms: None,
            mu_y: 0.0,
            mu_x: 0.0,
            var_x: 0.0,
            cov_xy: 0.0,
            beta: 1.0,
            spread_var_bps2: 0.0,
            ar_ring: Ar1Ring::new(config.ou_window),
            position: None,
            clock_regressions: 0,
            gap_resets: 0,
            rejected_inputs: 0,
        })
    }

    pub fn config(&self) -> &PairsStatArbConfig {
        &self.config
    }

    pub fn position(&self) -> Option<PairsPosition> {
        self.position
    }

    pub fn beta(&self) -> f64 {
        self.beta
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

    /// Clear open position state once both legs have settled or been reconciled flat.
    pub fn notify_exit_filled(&mut self) {
        self.position = None;
    }

    /// Reset rolling estimator state (preserving cumulative diagnostic counters).
    pub fn reset_filter(&mut self) {
        self.initialized = false;
        self.first_obs_ms = None;
        self.last_obs_ms = None;
        self.mu_y = 0.0;
        self.mu_x = 0.0;
        self.var_x = 0.0;
        self.cov_xy = 0.0;
        self.beta = 1.0_f64.clamp(self.config.beta_min, self.config.beta_max);
        self.spread_var_bps2 = 0.0;
        self.ar_ring.reset();
    }

    /// Convenience wrapper reading `fsm.state()`.
    pub fn observe_with_fsm(
        &mut self,
        now_ms: u64,
        quote_y: L1Quote,
        quote_x: L1Quote,
        fsm: &DefenseFsm,
    ) -> PairsStatArbDecision {
        self.observe(now_ms, quote_y, quote_x, fsm.state())
    }

    /// Advance the causal pairs engine with L1 quotes for legs `Y` and `X`.
    pub fn observe(
        &mut self,
        now_ms: u64,
        quote_y: L1Quote,
        quote_x: L1Quote,
        defense: DefenseState,
    ) -> PairsStatArbDecision {
        if self.last_obs_ms.is_some_and(|prev| now_ms < prev) {
            self.clock_regressions = self.clock_regressions.saturating_add(1);
            return self.blocked_decision(now_ms, StatArbBlockReason::ClockRegression, 0.0, 0.0);
        }

        if quote_y.ts_ms > now_ms || quote_x.ts_ms > now_ms {
            self.rejected_inputs = self.rejected_inputs.saturating_add(1);
            return self.blocked_decision(now_ms, StatArbBlockReason::InvalidInput, 0.0, 0.0);
        }

        let Some((mid_y, spr_y_bps)) = quote_y.mid_and_spread_bps() else {
            self.rejected_inputs = self.rejected_inputs.saturating_add(1);
            return self.blocked_decision(now_ms, StatArbBlockReason::InvalidInput, 0.0, 0.0);
        };
        let Some((mid_x, spr_x_bps)) = quote_x.mid_and_spread_bps() else {
            self.rejected_inputs = self.rejected_inputs.saturating_add(1);
            return self.blocked_decision(now_ms, StatArbBlockReason::InvalidInput, 0.0, 0.0);
        };

        let y_stale = (now_ms - quote_y.ts_ms) > self.config.max_quote_age_ms;
        let x_stale = (now_ms - quote_x.ts_ms) > self.config.max_quote_age_ms;
        if y_stale || x_stale {
            let combined_spr = spr_y_bps + self.beta * spr_x_bps;
            return self.blocked_decision(
                now_ms,
                StatArbBlockReason::StaleQuote,
                0.0,
                combined_spr,
            );
        }

        let log_y = mid_y.ln();
        let log_x = mid_x.ln();
        if !log_y.is_finite() || !log_x.is_finite() {
            self.rejected_inputs = self.rejected_inputs.saturating_add(1);
            return self.blocked_decision(now_ms, StatArbBlockReason::InvalidInput, 0.0, 0.0);
        }

        let gap_exceeded = self
            .last_obs_ms
            .is_some_and(|prev| now_ms - prev > self.config.max_gap_ms);
        if gap_exceeded {
            self.gap_resets = self.gap_resets.saturating_add(1);
            self.reset_filter();
        }

        let spread_bps;
        if !self.initialized {
            self.initialized = true;
            self.first_obs_ms = Some(now_ms);
            self.last_obs_ms = Some(now_ms);
            self.mu_y = log_y;
            self.mu_x = log_x;
            self.var_x = 0.0;
            self.cov_xy = 0.0;
            self.beta = 1.0_f64.clamp(self.config.beta_min, self.config.beta_max);
            self.spread_var_bps2 = 0.0;
            spread_bps = 0.0;
            self.ar_ring
                .maybe_push(now_ms, spread_bps, self.config.ar_sample_ms);
        } else {
            let prev_ms = self.last_obs_ms.unwrap_or(now_ms);
            let dt_ms = now_ms - prev_ms;
            let alpha = if dt_ms == 0 {
                0.0
            } else {
                1.0 - (-(dt_ms as f64) / (self.config.ewma_tau_ms as f64)).exp()
            };

            let dx = log_x - self.mu_x;
            let dy = log_y - self.mu_y;

            // Causal spread relative to prior EWMA equilibrium and prior beta.
            spread_bps = (dy - self.beta * dx) * 10_000.0;
            if !spread_bps.is_finite() {
                self.rejected_inputs = self.rejected_inputs.saturating_add(1);
                return self.blocked_decision(now_ms, StatArbBlockReason::InvalidInput, 0.0, 0.0);
            }

            if dt_ms > 0 {
                let one_minus_a = 1.0 - alpha;
                self.mu_x += alpha * dx;
                self.mu_y += alpha * dy;
                self.var_x = (one_minus_a * (self.var_x + alpha * dx * dx)).max(0.0);
                self.cov_xy = one_minus_a * (self.cov_xy + alpha * dx * dy);
                if self.var_x > MIN_EWMA_VAR_X && self.cov_xy.is_finite() {
                    self.beta = (self.cov_xy / self.var_x)
                        .clamp(self.config.beta_min, self.config.beta_max);
                }
                self.spread_var_bps2 =
                    (one_minus_a * self.spread_var_bps2 + alpha * spread_bps * spread_bps).max(0.0);
                self.ar_ring
                    .maybe_push(now_ms, spread_bps, self.config.ar_sample_ms);
                self.last_obs_ms = Some(now_ms);
            }
        }

        let spread_vol_bps = self
            .spread_var_bps2
            .sqrt()
            .max(self.config.min_spread_vol_bps);
        let z_score = spread_bps / spread_vol_bps;
        let combined_spread_bps = spr_y_bps + self.beta * spr_x_bps;
        let (phi, half_life_ms, stationarity) = self
            .ar_ring
            .estimate(self.config.min_samples, self.config.phi_max_stationary);

        // Open position management takes precedence and ALWAYS executes as ReduceOnlyExit.
        if let Some(pos) = self.position {
            let held_ms = now_ms.saturating_sub(pos.entry_ms);
            let exit_reason = if held_ms >= self.config.max_hold_ms {
                Some(StatArbExitReason::MaxHoldTime)
            } else if match pos.side {
                PairsPositionSide::LongYShortX => z_score <= -self.config.z_stop,
                PairsPositionSide::ShortYLongX => z_score >= self.config.z_stop,
            } {
                Some(StatArbExitReason::StopLoss)
            } else if match pos.side {
                PairsPositionSide::LongYShortX => z_score >= -self.config.z_exit,
                PairsPositionSide::ShortYLongX => z_score <= self.config.z_exit,
            } {
                Some(StatArbExitReason::MeanReversion)
            } else {
                None
            };

            if let Some(reason) = exit_reason {
                return PairsStatArbDecision {
                    now_ms,
                    action: StatArbAction::Exit,
                    block_reason: None,
                    exit_reason: Some(reason),
                    intent_class: Some(OrderIntentClass::ReduceOnlyExit),
                    side: Some(pos.side),
                    entry_scale: 1.0,
                    reduce_only: true,
                    beta: self.beta,
                    spread_bps,
                    spread_vol_bps,
                    z_score,
                    combined_spread_bps,
                    phi,
                    half_life_ms,
                    stationarity,
                    sample_count: self.ar_ring.len,
                };
            }

            return PairsStatArbDecision {
                now_ms,
                action: StatArbAction::Hold,
                block_reason: None,
                exit_reason: None,
                intent_class: None,
                side: Some(pos.side),
                entry_scale: 0.0,
                reduce_only: false,
                beta: self.beta,
                spread_bps,
                spread_vol_bps,
                z_score,
                combined_spread_bps,
                phi,
                half_life_ms,
                stationarity,
                sample_count: self.ar_ring.len,
            };
        }

        if !self.config.enabled {
            return self.make_blocked(
                now_ms,
                StatArbBlockReason::Disabled,
                spread_bps,
                spread_vol_bps,
                z_score,
                combined_spread_bps,
                phi,
                half_life_ms,
                stationarity,
            );
        }

        let warmed = self
            .first_obs_ms
            .is_some_and(|t0| now_ms.saturating_sub(t0) >= self.config.warmup_ms);
        if !warmed || stationarity == StationarityStatus::WarmingUp {
            return self.make_blocked(
                now_ms,
                StatArbBlockReason::WarmingUp,
                spread_bps,
                spread_vol_bps,
                z_score,
                combined_spread_bps,
                phi,
                half_life_ms,
                stationarity,
            );
        }

        if stationarity != StationarityStatus::Stationary {
            return self.make_blocked(
                now_ms,
                StatArbBlockReason::NonStationary,
                spread_bps,
                spread_vol_bps,
                z_score,
                combined_spread_bps,
                phi,
                half_life_ms,
                stationarity,
            );
        }

        if z_score.abs() >= self.config.z_stop {
            return self.make_blocked(
                now_ms,
                StatArbBlockReason::StructuralBreak,
                spread_bps,
                spread_vol_bps,
                z_score,
                combined_spread_bps,
                phi,
                half_life_ms,
                stationarity,
            );
        }

        if combined_spread_bps > self.config.max_combined_spread_bps {
            return self.make_blocked(
                now_ms,
                StatArbBlockReason::WideSpread,
                spread_bps,
                spread_vol_bps,
                z_score,
                combined_spread_bps,
                phi,
                half_life_ms,
                stationarity,
            );
        }

        let candidate_side = if z_score <= -self.config.z_entry {
            Some(PairsPositionSide::LongYShortX)
        } else if z_score >= self.config.z_entry {
            Some(PairsPositionSide::ShortYLongX)
        } else {
            None
        };

        let Some(side) = candidate_side else {
            return PairsStatArbDecision {
                now_ms,
                action: StatArbAction::Idle,
                block_reason: None,
                exit_reason: None,
                intent_class: None,
                side: None,
                entry_scale: 0.0,
                reduce_only: false,
                beta: self.beta,
                spread_bps,
                spread_vol_bps,
                z_score,
                combined_spread_bps,
                phi,
                half_life_ms,
                stationarity,
                sample_count: self.ar_ring.len,
            };
        };

        let (leg_y_side, leg_x_side) = side.leg_sides();
        let permits_y =
            defense.permits_intent(OrderIntentClass::RiskIncreasingEntry, Some(leg_y_side));
        let permits_x =
            defense.permits_intent(OrderIntentClass::RiskIncreasingEntry, Some(leg_x_side));
        if !permits_y || !permits_x {
            return self.make_blocked(
                now_ms,
                StatArbBlockReason::DefenseBlocked,
                spread_bps,
                spread_vol_bps,
                z_score,
                combined_spread_bps,
                phi,
                half_life_ms,
                stationarity,
            );
        }

        let scale_y = defense.entry_size_scale(self.config.caution_size_scale, Some(leg_y_side));
        let scale_x = defense.entry_size_scale(self.config.caution_size_scale, Some(leg_x_side));
        let entry_scale = scale_y.min(scale_x).clamp(0.0, 1.0);
        if entry_scale <= 0.0 {
            return self.make_blocked(
                now_ms,
                StatArbBlockReason::DefenseBlocked,
                spread_bps,
                spread_vol_bps,
                z_score,
                combined_spread_bps,
                phi,
                half_life_ms,
                stationarity,
            );
        }

        self.position = Some(PairsPosition {
            side,
            entry_ms: now_ms,
            entry_beta: self.beta,
            entry_z: z_score,
        });

        PairsStatArbDecision {
            now_ms,
            action: StatArbAction::Enter,
            block_reason: None,
            exit_reason: None,
            intent_class: Some(OrderIntentClass::RiskIncreasingEntry),
            side: Some(side),
            entry_scale,
            reduce_only: false,
            beta: self.beta,
            spread_bps,
            spread_vol_bps,
            z_score,
            combined_spread_bps,
            phi,
            half_life_ms,
            stationarity,
            sample_count: self.ar_ring.len,
        }
    }

    fn blocked_decision(
        &self,
        now_ms: u64,
        reason: StatArbBlockReason,
        spread_bps: f64,
        combined_spread_bps: f64,
    ) -> PairsStatArbDecision {
        let spread_vol_bps = self
            .spread_var_bps2
            .sqrt()
            .max(self.config.min_spread_vol_bps);
        PairsStatArbDecision {
            now_ms,
            action: StatArbAction::Blocked,
            block_reason: Some(reason),
            exit_reason: None,
            intent_class: None,
            side: self.position.map(|p| p.side),
            entry_scale: 0.0,
            reduce_only: false,
            beta: self.beta,
            spread_bps,
            spread_vol_bps,
            z_score: 0.0,
            combined_spread_bps,
            phi: None,
            half_life_ms: None,
            stationarity: StationarityStatus::NonStationary,
            sample_count: self.ar_ring.len,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn make_blocked(
        &self,
        now_ms: u64,
        reason: StatArbBlockReason,
        spread_bps: f64,
        spread_vol_bps: f64,
        z_score: f64,
        combined_spread_bps: f64,
        phi: Option<f64>,
        half_life_ms: Option<f64>,
        stationarity: StationarityStatus,
    ) -> PairsStatArbDecision {
        PairsStatArbDecision {
            now_ms,
            action: StatArbAction::Blocked,
            block_reason: Some(reason),
            exit_reason: None,
            intent_class: None,
            side: None,
            entry_scale: 0.0,
            reduce_only: false,
            beta: self.beta,
            spread_bps,
            spread_vol_bps,
            z_score,
            combined_spread_bps,
            phi,
            half_life_ms,
            stationarity,
            sample_count: self.ar_ring.len,
        }
    }
}

/// Oracle and funding snapshot from `funding_and_ctx` (stack `Copy`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct OracleSnapshot {
    pub ts_ms: u64,
    pub oracle_px: f64,
    /// Hourly funding rate in bps/hr (`> 0`: longs pay shorts; `< 0`: shorts pay longs).
    pub funding_rate_bps: f64,
}

/// Configuration for [`OracleDislocationReversion`]. Default-off (`enabled = false`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OracleDislocationConfig {
    /// Master enable switch (default `false`).
    pub enabled: bool,
    /// Causal slow EWMA time constant (ms) for equilibrium `perp_mid - oracle_px` (bps).
    pub eq_ewma_tau_ms: u64,
    /// Minimum warmup elapsed time (ms) before allowing entry.
    pub warmup_ms: u64,
    /// Minimum valid observation count before allowing entry.
    pub min_samples: usize,
    /// Maximum perp BBO quote age (ms).
    pub max_perp_age_ms: u64,
    /// Maximum oracle snapshot age (ms).
    pub max_oracle_age_ms: u64,
    /// Maximum inter-tick gap (ms) before resetting equilibrium state.
    pub max_gap_ms: u64,
    /// Minimum demeaned dislocation threshold (bps).
    pub min_dislocation_bps: f64,
    /// Spread multiplier for hurdle `max(min_dislocation_bps, spread_mult * spread_bps)`.
    pub spread_mult: f64,
    /// Maximum absolute dislocation (bps) allowed on entry (structural break / depeg guard).
    pub max_dislocation_bps: f64,
    /// Convergence exit threshold (`|dislocation_bps| <= exit_dislocation_bps`).
    pub exit_dislocation_bps: f64,
    /// Stop-loss dislocation magnitude (`> max_dislocation_bps`).
    pub stop_dislocation_bps: f64,
    /// Maximum holding duration (ms) before mandatory reduce-only time stop.
    pub max_hold_ms: u64,
    /// Require funding alignment (block entries paying > `max_adverse_funding_bps`/hr).
    pub require_funding_alignment: bool,
    /// Maximum adverse hourly funding (bps/hr) permitted when `require_funding_alignment` is on.
    pub max_adverse_funding_bps: f64,
    /// Maximum perp L1 spread (bps) allowed on entry.
    pub max_entry_spread_bps: f64,
    /// Entry notional scale factor applied when `DefenseState` is `Caution` or `SideDefense`.
    pub caution_size_scale: f64,
}

impl Default for OracleDislocationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            eq_ewma_tau_ms: 3_600_000,
            warmup_ms: 900_000,
            min_samples: 20,
            max_perp_age_ms: 5_000,
            max_oracle_age_ms: 15_000,
            max_gap_ms: 30_000,
            min_dislocation_bps: 4.0,
            spread_mult: 1.5,
            max_dislocation_bps: 80.0,
            exit_dislocation_bps: 0.5,
            stop_dislocation_bps: 100.0,
            max_hold_ms: 3_600_000,
            require_funding_alignment: false,
            max_adverse_funding_bps: 0.5,
            max_entry_spread_bps: 20.0,
            caution_size_scale: 0.50,
        }
    }
}

impl OracleDislocationConfig {
    /// Fail closed on non-finite parameters or inconsistent thresholds.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.eq_ewma_tau_ms >= 1_000,
            "oracle_disloc.eq_ewma_tau_ms must be >= 1000"
        );
        ensure!(
            self.min_samples > 0,
            "oracle_disloc.min_samples must be > 0"
        );
        ensure!(
            self.max_perp_age_ms > 0 && self.max_oracle_age_ms > 0 && self.max_gap_ms > 0,
            "oracle_disloc age/gap limits must be > 0"
        );
        ensure!(
            self.min_dislocation_bps.is_finite()
                && self.min_dislocation_bps > 0.0
                && self.spread_mult.is_finite()
                && self.spread_mult >= 0.0,
            "oracle_disloc hurdle parameters must be finite and valid"
        );
        ensure!(
            self.exit_dislocation_bps.is_finite()
                && self.exit_dislocation_bps >= 0.0
                && self.exit_dislocation_bps < self.min_dislocation_bps,
            "oracle_disloc.exit_dislocation_bps must lie in [0, min_dislocation_bps)"
        );
        ensure!(
            self.max_dislocation_bps.is_finite()
                && self.max_dislocation_bps > self.min_dislocation_bps
                && self.stop_dislocation_bps.is_finite()
                && self.stop_dislocation_bps > self.max_dislocation_bps,
            "oracle_disloc bounds must satisfy min < max_dislocation_bps < stop_dislocation_bps"
        );
        ensure!(
            self.max_hold_ms > 0,
            "oracle_disloc.max_hold_ms must be > 0"
        );
        ensure!(
            self.max_adverse_funding_bps.is_finite(),
            "oracle_disloc.max_adverse_funding_bps must be finite"
        );
        ensure!(
            self.max_entry_spread_bps.is_finite() && self.max_entry_spread_bps > 0.0,
            "oracle_disloc.max_entry_spread_bps must be finite and > 0"
        );
        ensure!(
            self.caution_size_scale.is_finite()
                && self.caution_size_scale > 0.0
                && self.caution_size_scale <= 1.0,
            "oracle_disloc.caution_size_scale must lie in (0.0, 1.0]"
        );
        Ok(())
    }
}

/// Active open position for [`OracleDislocationReversion`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct OraclePosition {
    pub side: DefenseSide,
    pub entry_ms: u64,
    pub entry_dislocation_bps: f64,
}

/// Deterministic decision snapshot emitted by [`OracleDislocationReversion::observe`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct OracleDislocationDecision {
    pub now_ms: u64,
    pub action: StatArbAction,
    pub block_reason: Option<StatArbBlockReason>,
    pub exit_reason: Option<StatArbExitReason>,
    pub intent_class: Option<OrderIntentClass>,
    pub side: Option<DefenseSide>,
    pub entry_scale: f64,
    pub reduce_only: bool,
    pub raw_dislocation_bps: f64,
    pub equilibrium_bps: f64,
    pub dislocation_bps: f64,
    pub hurdle_bps: f64,
    pub spread_bps: f64,
    pub funding_rate_bps: f64,
    pub sample_count: usize,
}

/// Zero-allocation causal perp-vs-oracle dislocation reversion engine.
#[derive(Debug, Clone)]
pub struct OracleDislocationReversion {
    config: OracleDislocationConfig,
    initialized: bool,
    first_obs_ms: Option<u64>,
    last_obs_ms: Option<u64>,
    equilibrium_bps: f64,
    sample_count: usize,
    position: Option<OraclePosition>,
    clock_regressions: u64,
    gap_resets: u64,
    rejected_inputs: u64,
}

impl OracleDislocationReversion {
    /// Construct a validated [`OracleDislocationReversion`] instance.
    pub fn new(config: OracleDislocationConfig) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            config,
            initialized: false,
            first_obs_ms: None,
            last_obs_ms: None,
            equilibrium_bps: 0.0,
            sample_count: 0,
            position: None,
            clock_regressions: 0,
            gap_resets: 0,
            rejected_inputs: 0,
        })
    }

    pub fn config(&self) -> &OracleDislocationConfig {
        &self.config
    }

    pub fn position(&self) -> Option<OraclePosition> {
        self.position
    }

    pub fn equilibrium_bps(&self) -> f64 {
        self.equilibrium_bps
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

    /// Clear open position state once exit execution is confirmed flat.
    pub fn notify_exit_filled(&mut self) {
        self.position = None;
    }

    /// Reset slow equilibrium estimator (preserving diagnostic counters).
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
        oracle: OracleSnapshot,
        fsm: &DefenseFsm,
    ) -> OracleDislocationDecision {
        self.observe(now_ms, perp_quote, oracle, fsm.state())
    }

    /// Advance the causal oracle dislocation engine.
    ///
    /// Convention: `raw_dislocation_bps = (perp_mid - oracle_px) / oracle_px * 10_000`.
    /// - `dislocation_bps = raw_dislocation_bps - equilibrium_bps`.
    /// - When `dislocation_bps <= -hurdle_bps` -> perp is cheap vs oracle -> `Buy`.
    /// - When `dislocation_bps >= hurdle_bps` -> perp is rich vs oracle -> `Sell`.
    pub fn observe(
        &mut self,
        now_ms: u64,
        perp_quote: L1Quote,
        oracle: OracleSnapshot,
        defense: DefenseState,
    ) -> OracleDislocationDecision {
        if self.last_obs_ms.is_some_and(|prev| now_ms < prev) {
            self.clock_regressions = self.clock_regressions.saturating_add(1);
            return self.blocked_decision(
                now_ms,
                StatArbBlockReason::ClockRegression,
                0.0,
                0.0,
                0.0,
            );
        }

        if perp_quote.ts_ms > now_ms || oracle.ts_ms > now_ms {
            self.rejected_inputs = self.rejected_inputs.saturating_add(1);
            return self.blocked_decision(now_ms, StatArbBlockReason::InvalidInput, 0.0, 0.0, 0.0);
        }

        let Some((perp_mid, spread_bps)) = perp_quote.mid_and_spread_bps() else {
            self.rejected_inputs = self.rejected_inputs.saturating_add(1);
            return self.blocked_decision(now_ms, StatArbBlockReason::InvalidInput, 0.0, 0.0, 0.0);
        };

        if !oracle.oracle_px.is_finite()
            || oracle.oracle_px <= 0.0
            || !oracle.funding_rate_bps.is_finite()
        {
            self.rejected_inputs = self.rejected_inputs.saturating_add(1);
            return self.blocked_decision(
                now_ms,
                StatArbBlockReason::InvalidInput,
                0.0,
                spread_bps,
                0.0,
            );
        }

        let raw_dislocation_bps = ((perp_mid - oracle.oracle_px) / oracle.oracle_px) * 10_000.0;
        if !raw_dislocation_bps.is_finite() {
            self.rejected_inputs = self.rejected_inputs.saturating_add(1);
            return self.blocked_decision(
                now_ms,
                StatArbBlockReason::InvalidInput,
                0.0,
                spread_bps,
                oracle.funding_rate_bps,
            );
        }

        if (now_ms - perp_quote.ts_ms) > self.config.max_perp_age_ms {
            return self.blocked_decision(
                now_ms,
                StatArbBlockReason::StaleQuote,
                raw_dislocation_bps,
                spread_bps,
                oracle.funding_rate_bps,
            );
        }
        if (now_ms - oracle.ts_ms) > self.config.max_oracle_age_ms {
            return self.blocked_decision(
                now_ms,
                StatArbBlockReason::StaleOracle,
                raw_dislocation_bps,
                spread_bps,
                oracle.funding_rate_bps,
            );
        }

        if self
            .last_obs_ms
            .is_some_and(|prev| now_ms - prev > self.config.max_gap_ms)
        {
            self.gap_resets = self.gap_resets.saturating_add(1);
            self.reset_filter();
        }

        let dislocation_bps;
        if !self.initialized {
            self.initialized = true;
            self.first_obs_ms = Some(now_ms);
            self.last_obs_ms = Some(now_ms);
            self.equilibrium_bps = raw_dislocation_bps;
            self.sample_count = 1;
            dislocation_bps = 0.0;
        } else {
            let prev_ms = self.last_obs_ms.unwrap_or(now_ms);
            let dt_ms = now_ms - prev_ms;
            dislocation_bps = raw_dislocation_bps - self.equilibrium_bps;
            if dt_ms > 0 {
                let alpha = 1.0 - (-(dt_ms as f64) / (self.config.eq_ewma_tau_ms as f64)).exp();
                self.equilibrium_bps += alpha * (raw_dislocation_bps - self.equilibrium_bps);
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
            let exit_reason = if held_ms >= self.config.max_hold_ms {
                Some(StatArbExitReason::MaxHoldTime)
            } else if match pos.side {
                DefenseSide::Buy => dislocation_bps <= -self.config.stop_dislocation_bps,
                DefenseSide::Sell => dislocation_bps >= self.config.stop_dislocation_bps,
            } {
                Some(StatArbExitReason::StopLoss)
            } else if match pos.side {
                DefenseSide::Buy => dislocation_bps >= -self.config.exit_dislocation_bps,
                DefenseSide::Sell => dislocation_bps <= self.config.exit_dislocation_bps,
            } {
                Some(StatArbExitReason::MeanReversion)
            } else {
                None
            };

            if let Some(reason) = exit_reason {
                return OracleDislocationDecision {
                    now_ms,
                    action: StatArbAction::Exit,
                    block_reason: None,
                    exit_reason: Some(reason),
                    intent_class: Some(OrderIntentClass::ReduceOnlyExit),
                    side: Some(pos.side),
                    entry_scale: 1.0,
                    reduce_only: true,
                    raw_dislocation_bps,
                    equilibrium_bps: self.equilibrium_bps,
                    dislocation_bps,
                    hurdle_bps,
                    spread_bps,
                    funding_rate_bps: oracle.funding_rate_bps,
                    sample_count: self.sample_count,
                };
            }

            return OracleDislocationDecision {
                now_ms,
                action: StatArbAction::Hold,
                block_reason: None,
                exit_reason: None,
                intent_class: None,
                side: Some(pos.side),
                entry_scale: 0.0,
                reduce_only: false,
                raw_dislocation_bps,
                equilibrium_bps: self.equilibrium_bps,
                dislocation_bps,
                hurdle_bps,
                spread_bps,
                funding_rate_bps: oracle.funding_rate_bps,
                sample_count: self.sample_count,
            };
        }

        if !self.config.enabled {
            return self.make_blocked(
                now_ms,
                StatArbBlockReason::Disabled,
                raw_dislocation_bps,
                dislocation_bps,
                hurdle_bps,
                spread_bps,
                oracle.funding_rate_bps,
            );
        }

        let warmed = self
            .first_obs_ms
            .is_some_and(|t0| now_ms.saturating_sub(t0) >= self.config.warmup_ms)
            && self.sample_count >= self.config.min_samples;
        if !warmed {
            return self.make_blocked(
                now_ms,
                StatArbBlockReason::WarmingUp,
                raw_dislocation_bps,
                dislocation_bps,
                hurdle_bps,
                spread_bps,
                oracle.funding_rate_bps,
            );
        }

        if dislocation_bps.abs() >= self.config.max_dislocation_bps {
            return self.make_blocked(
                now_ms,
                StatArbBlockReason::StructuralBreak,
                raw_dislocation_bps,
                dislocation_bps,
                hurdle_bps,
                spread_bps,
                oracle.funding_rate_bps,
            );
        }

        if spread_bps > self.config.max_entry_spread_bps {
            return self.make_blocked(
                now_ms,
                StatArbBlockReason::WideSpread,
                raw_dislocation_bps,
                dislocation_bps,
                hurdle_bps,
                spread_bps,
                oracle.funding_rate_bps,
            );
        }

        let candidate_side = if dislocation_bps <= -hurdle_bps {
            Some(DefenseSide::Buy)
        } else if dislocation_bps >= hurdle_bps {
            Some(DefenseSide::Sell)
        } else {
            None
        };

        let Some(side) = candidate_side else {
            return OracleDislocationDecision {
                now_ms,
                action: StatArbAction::Idle,
                block_reason: None,
                exit_reason: None,
                intent_class: None,
                side: None,
                entry_scale: 0.0,
                reduce_only: false,
                raw_dislocation_bps,
                equilibrium_bps: self.equilibrium_bps,
                dislocation_bps,
                hurdle_bps,
                spread_bps,
                funding_rate_bps: oracle.funding_rate_bps,
                sample_count: self.sample_count,
            };
        };

        if self.config.require_funding_alignment {
            let funding_ok = match side {
                // Long pays funding when funding_rate_bps > 0
                DefenseSide::Buy => oracle.funding_rate_bps <= self.config.max_adverse_funding_bps,
                // Short pays funding when funding_rate_bps < 0
                DefenseSide::Sell => {
                    oracle.funding_rate_bps >= -self.config.max_adverse_funding_bps
                }
            };
            if !funding_ok {
                return self.make_blocked(
                    now_ms,
                    StatArbBlockReason::AdverseFunding,
                    raw_dislocation_bps,
                    dislocation_bps,
                    hurdle_bps,
                    spread_bps,
                    oracle.funding_rate_bps,
                );
            }
        }

        if !defense.permits_intent(OrderIntentClass::RiskIncreasingEntry, Some(side)) {
            return self.make_blocked(
                now_ms,
                StatArbBlockReason::DefenseBlocked,
                raw_dislocation_bps,
                dislocation_bps,
                hurdle_bps,
                spread_bps,
                oracle.funding_rate_bps,
            );
        }

        let entry_scale = defense
            .entry_size_scale(self.config.caution_size_scale, Some(side))
            .clamp(0.0, 1.0);
        if entry_scale <= 0.0 {
            return self.make_blocked(
                now_ms,
                StatArbBlockReason::DefenseBlocked,
                raw_dislocation_bps,
                dislocation_bps,
                hurdle_bps,
                spread_bps,
                oracle.funding_rate_bps,
            );
        }

        self.position = Some(OraclePosition {
            side,
            entry_ms: now_ms,
            entry_dislocation_bps: dislocation_bps,
        });

        OracleDislocationDecision {
            now_ms,
            action: StatArbAction::Enter,
            block_reason: None,
            exit_reason: None,
            intent_class: Some(OrderIntentClass::RiskIncreasingEntry),
            side: Some(side),
            entry_scale,
            reduce_only: false,
            raw_dislocation_bps,
            equilibrium_bps: self.equilibrium_bps,
            dislocation_bps,
            hurdle_bps,
            spread_bps,
            funding_rate_bps: oracle.funding_rate_bps,
            sample_count: self.sample_count,
        }
    }

    fn blocked_decision(
        &self,
        now_ms: u64,
        reason: StatArbBlockReason,
        raw_dislocation_bps: f64,
        spread_bps: f64,
        funding_rate_bps: f64,
    ) -> OracleDislocationDecision {
        let hurdle_bps = self
            .config
            .min_dislocation_bps
            .max(self.config.spread_mult * spread_bps);
        OracleDislocationDecision {
            now_ms,
            action: StatArbAction::Blocked,
            block_reason: Some(reason),
            exit_reason: None,
            intent_class: None,
            side: self.position.map(|p| p.side),
            entry_scale: 0.0,
            reduce_only: false,
            raw_dislocation_bps,
            equilibrium_bps: self.equilibrium_bps,
            dislocation_bps: 0.0,
            hurdle_bps,
            spread_bps,
            funding_rate_bps,
            sample_count: self.sample_count,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn make_blocked(
        &self,
        now_ms: u64,
        reason: StatArbBlockReason,
        raw_dislocation_bps: f64,
        dislocation_bps: f64,
        hurdle_bps: f64,
        spread_bps: f64,
        funding_rate_bps: f64,
    ) -> OracleDislocationDecision {
        OracleDislocationDecision {
            now_ms,
            action: StatArbAction::Blocked,
            block_reason: Some(reason),
            exit_reason: None,
            intent_class: None,
            side: None,
            entry_scale: 0.0,
            reduce_only: false,
            raw_dislocation_bps,
            equilibrium_bps: self.equilibrium_bps,
            dislocation_bps,
            hurdle_bps,
            spread_bps,
            funding_rate_bps,
            sample_count: self.sample_count,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strategy::defense_fsm::BlockedSide;

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

    fn pairs_cfg() -> PairsStatArbConfig {
        PairsStatArbConfig {
            enabled: true,
            ewma_tau_ms: 60_000,
            warmup_ms: 10_000,
            min_samples: 8,
            ou_window: 32,
            ar_sample_ms: 1_000,
            max_quote_age_ms: 5_000,
            max_gap_ms: 30_000,
            beta_min: 0.1,
            beta_max: 5.0,
            z_entry: 2.0,
            z_exit: 0.25,
            z_stop: 5.0,
            phi_max_stationary: 0.98,
            max_hold_ms: 120_000,
            min_spread_vol_bps: 1.0,
            max_combined_spread_bps: 20.0,
            caution_size_scale: 0.5,
        }
    }

    fn oracle_cfg() -> OracleDislocationConfig {
        OracleDislocationConfig {
            enabled: true,
            eq_ewma_tau_ms: 60_000,
            warmup_ms: 5_000,
            min_samples: 5,
            max_perp_age_ms: 5_000,
            max_oracle_age_ms: 10_000,
            max_gap_ms: 30_000,
            min_dislocation_bps: 5.0,
            spread_mult: 1.5,
            max_dislocation_bps: 50.0,
            exit_dislocation_bps: 1.0,
            stop_dislocation_bps: 80.0,
            max_hold_ms: 60_000,
            require_funding_alignment: true,
            max_adverse_funding_bps: 0.2,
            max_entry_spread_bps: 15.0,
            caution_size_scale: 0.5,
        }
    }

    #[test]
    fn defaults_are_disabled_and_invalid_configs_fail_closed() {
        let p_def = PairsStatArbConfig::default();
        assert!(!p_def.enabled);
        assert!(p_def.validate().is_ok());

        let mut p_bad = p_def;
        p_bad.z_entry = f64::NAN;
        assert!(p_bad.validate().is_err());
        let mut p_bad2 = p_def;
        p_bad2.phi_max_stationary = 1.0;
        assert!(p_bad2.validate().is_err());

        let o_def = OracleDislocationConfig::default();
        assert!(!o_def.enabled);
        assert!(o_def.validate().is_ok());

        let mut o_bad = o_def;
        o_bad.stop_dislocation_bps = o_bad.max_dislocation_bps;
        assert!(o_bad.validate().is_err());
    }

    #[test]
    fn pairs_ewma_beta_converges_and_trades_mean_reversion() {
        let mut eng = PairsStatArb::new(pairs_cfg()).unwrap();
        let mut ts = 10_000_u64;
        // Feed cointegrated stationary spread with beta = 2.0 and OU-like wave (phi ~ 0.88 < 0.98)
        for i in 0..40 {
            ts += 1_000;
            let x_ret = 0.002 * ((i as f64) * 0.3).sin();
            let noise: f64 = 0.00012 * ((i as f64) * 0.5).sin();
            let px_x = 100.0 * x_ret.exp();
            let px_y = 200.0 * (2.0 * x_ret + noise).exp();
            let d = eng.observe(
                ts,
                mk_quote(ts, px_y, 0.5),
                mk_quote(ts, px_x, 0.5),
                DefenseState::Normal,
            );
            assert!(d.action == StatArbAction::Idle || d.action == StatArbAction::Blocked);
        }
        assert!(
            (eng.beta() - 2.0).abs() < 0.35,
            "expected causal EWMA beta near 2.0, got {}",
            eng.beta()
        );

        // Shock Y downward by -5 bps relative to X -> z <= -2.0 (inside [z_entry, z_stop)) -> Enter LongYShortX
        ts += 1_000;
        let x_ret_now = 0.002 * (40.0_f64 * 0.3).sin();
        let x_now = 100.0 * x_ret_now.exp();
        let y_shocked = 200.0 * (2.0 * x_ret_now - 0.0005).exp();
        let ent = eng.observe(
            ts,
            mk_quote(ts, y_shocked, 0.5),
            mk_quote(ts, x_now, 0.5),
            DefenseState::Normal,
        );
        assert_eq!(ent.action, StatArbAction::Enter);
        assert_eq!(ent.side, Some(PairsPositionSide::LongYShortX));
        assert_eq!(
            ent.intent_class,
            Some(OrderIntentClass::RiskIncreasingEntry)
        );
        assert!((ent.entry_scale - 1.0).abs() < 1e-12);

        // Mean revert back to equilibrium -> Exit with MeanReversion (reduce-only), even under Halt!
        ts += 1_000;
        let x_ret_rev = 0.002 * (41.0_f64 * 0.3).sin();
        let ex = eng.observe(
            ts,
            mk_quote(ts, 200.0 * (2.0 * x_ret_rev).exp(), 0.5),
            mk_quote(ts, 100.0 * x_ret_rev.exp(), 0.5),
            DefenseState::Halt,
        );
        assert_eq!(ex.action, StatArbAction::Exit);
        assert_eq!(ex.exit_reason, Some(StatArbExitReason::MeanReversion));
        assert_eq!(ex.intent_class, Some(OrderIntentClass::ReduceOnlyExit));
        assert!(ex.reduce_only);
        eng.notify_exit_filled();
        assert!(eng.position().is_none());
    }

    #[test]
    fn pairs_stationarity_gate_blocks_trending_regime_break() {
        let mut cfg = pairs_cfg();
        cfg.phi_max_stationary = 0.90;
        let mut eng = PairsStatArb::new(cfg).unwrap();
        let mut ts = 10_000_u64;
        // Feed a steadily trending drift in Y relative to X (random walk / trend -> phi >= 0.90)
        for i in 0..30 {
            ts += 1_000;
            let px_x = 100.0;
            let px_y = 200.0 * (0.0002 * (i as f64)).exp();
            let _ = eng.observe(
                ts,
                mk_quote(ts, px_y, 0.5),
                mk_quote(ts, px_x, 0.5),
                DefenseState::Normal,
            );
        }
        ts += 1_000;
        let d = eng.observe(
            ts,
            mk_quote(ts, 203.0, 0.5),
            mk_quote(ts, 100.0, 0.5),
            DefenseState::Normal,
        );
        assert_eq!(d.action, StatArbAction::Blocked);
        assert_eq!(d.block_reason, Some(StatArbBlockReason::NonStationary));
    }

    #[test]
    fn pairs_defense_fsm_scaling_and_max_hold_and_clock_fail_closed() {
        let mut eng = PairsStatArb::new(pairs_cfg()).unwrap();
        let mut ts = 10_000_u64;
        for i in 0..30 {
            ts += 1_000;
            let noise: f64 = 0.00012 * ((i as f64) * 0.5).sin();
            let _ = eng.observe(
                ts,
                mk_quote(ts, 200.0 * noise.exp(), 0.5),
                mk_quote(ts, 100.0, 0.5),
                DefenseState::Normal,
            );
        }

        // Backward clock fails closed without mutating position
        let back = eng.observe(
            ts - 1,
            mk_quote(ts - 1, 200.0, 0.5),
            mk_quote(ts - 1, 100.0, 0.5),
            DefenseState::Normal,
        );
        assert_eq!(back.action, StatArbAction::Blocked);
        assert_eq!(back.block_reason, Some(StatArbBlockReason::ClockRegression));
        assert_eq!(eng.clock_regressions(), 1);

        // SideDefense blocking Buy blocks LongYShortX (since Y is Buy)
        ts += 1_000;
        let blk = eng.observe(
            ts,
            mk_quote(ts, 200.0 * (-0.00045_f64).exp(), 0.5),
            mk_quote(ts, 100.0, 0.5),
            DefenseState::SideDefense {
                blocked_side: BlockedSide::Buy,
            },
        );
        assert_eq!(blk.action, StatArbAction::Blocked);
        assert_eq!(blk.block_reason, Some(StatArbBlockReason::DefenseBlocked));

        // Caution permits entry with caution_size_scale = 0.5
        ts += 1_000;
        let ent = eng.observe(
            ts,
            mk_quote(ts, 200.0 * (-0.00045_f64).exp(), 0.5),
            mk_quote(ts, 100.0, 0.5),
            DefenseState::Caution,
        );
        assert_eq!(ent.action, StatArbAction::Enter);
        assert!((ent.entry_scale - 0.5).abs() < 1e-12);

        // Max hold stop fires even under ReduceOnly
        let mut hold_ts = ts + 120_000;
        // Advance through intermediate ticks <= max_gap_ms so filter doesn't reset
        while ts + 20_000 < hold_ts {
            ts += 20_000;
            let _ = eng.observe(
                ts,
                mk_quote(ts, 200.0 * (-0.0008_f64).exp(), 0.5),
                mk_quote(ts, 100.0, 0.5),
                DefenseState::ReduceOnly,
            );
        }
        hold_ts = ts + 20_000;
        let ex = eng.observe(
            hold_ts,
            mk_quote(hold_ts, 200.0 * (-0.0008_f64).exp(), 0.5),
            mk_quote(hold_ts, 100.0, 0.5),
            DefenseState::ReduceOnly,
        );
        assert_eq!(ex.action, StatArbAction::Exit);
        assert_eq!(ex.exit_reason, Some(StatArbExitReason::MaxHoldTime));
        assert!(ex.reduce_only);
    }

    #[test]
    fn oracle_dislocation_hurdle_funding_and_convergence_exit() {
        let mut eng = OracleDislocationReversion::new(oracle_cfg()).unwrap();
        let mut ts = 1_000_u64;
        for _ in 0..8 {
            ts += 1_000;
            let d = eng.observe(
                ts,
                mk_quote(ts, 100.0, 2.0), // 4 bps spread -> hurdle = max(5, 1.5*4) = 6 bps
                OracleSnapshot {
                    ts_ms: ts,
                    oracle_px: 100.0,
                    funding_rate_bps: 0.0,
                },
                DefenseState::Normal,
            );
            assert!(d.action == StatArbAction::Idle || d.action == StatArbAction::Blocked);
        }

        // Perp cheap by -5.5 bps, spread is 4 bps -> hurdle is 6.0 bps -> Idle (below hurdle)
        ts += 1_000;
        let below = eng.observe(
            ts,
            mk_quote(ts, 99.945, 2.0),
            OracleSnapshot {
                ts_ms: ts,
                oracle_px: 100.0,
                funding_rate_bps: -0.5,
            },
            DefenseState::Normal,
        );
        assert_eq!(below.action, StatArbAction::Idle);

        // Perp cheap by -10 bps, but funding is +1.0 bps/hr (longs pay > 0.2) -> AdverseFunding!
        ts += 1_000;
        let adv_fnd = eng.observe(
            ts,
            mk_quote(ts, 99.90, 2.0),
            OracleSnapshot {
                ts_ms: ts,
                oracle_px: 100.0,
                funding_rate_bps: 1.0,
            },
            DefenseState::Normal,
        );
        assert_eq!(adv_fnd.action, StatArbAction::Blocked);
        assert_eq!(
            adv_fnd.block_reason,
            Some(StatArbBlockReason::AdverseFunding)
        );

        // Stale oracle -> StaleOracle fail-closed
        ts += 1_000;
        let stale = eng.observe(
            ts,
            mk_quote(ts, 99.90, 2.0),
            OracleSnapshot {
                ts_ms: ts - 12_000,
                oracle_px: 100.0,
                funding_rate_bps: -0.5,
            },
            DefenseState::Normal,
        );
        assert_eq!(stale.action, StatArbAction::Blocked);
        assert_eq!(stale.block_reason, Some(StatArbBlockReason::StaleOracle));

        // Perp cheap by -10 bps with favorable funding (-0.5 bps/hr) -> Enter Buy
        ts += 1_000;
        let ent = eng.observe(
            ts,
            mk_quote(ts, 99.90, 2.0),
            OracleSnapshot {
                ts_ms: ts,
                oracle_px: 100.0,
                funding_rate_bps: -0.5,
            },
            DefenseState::Normal,
        );
        assert_eq!(ent.action, StatArbAction::Enter);
        assert_eq!(ent.side, Some(DefenseSide::Buy));
        assert_eq!(
            ent.intent_class,
            Some(OrderIntentClass::RiskIncreasingEntry)
        );

        // Convergence back to oracle -> Exit MeanReversion (even under ReduceOnly)
        ts += 1_000;
        let ex = eng.observe(
            ts,
            mk_quote(ts, 100.0, 2.0),
            OracleSnapshot {
                ts_ms: ts,
                oracle_px: 100.0,
                funding_rate_bps: -0.5,
            },
            DefenseState::ReduceOnly,
        );
        assert_eq!(ex.action, StatArbAction::Exit);
        assert_eq!(ex.exit_reason, Some(StatArbExitReason::MeanReversion));
        assert!(ex.reduce_only);
        eng.notify_exit_filled();
        assert!(eng.position().is_none());
    }
}
