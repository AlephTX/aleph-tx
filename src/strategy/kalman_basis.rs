//! Causal 1D Kalman slow-equilibrium basis filter and OU mean-reversion guard (`ARB-IMP-04`).
//!
//! Tracks a slow-moving equilibrium basis `alpha_bps` across session regimes using a
//! spread-adaptive observation noise model (`R_t = r_base_bps2 + spread_r_coeff * spread_bps^2`),
//! estimates causal AR(1) / Ornstein–Uhlenbeck persistence `phi` and `half_life_ms` from
//! fixed-capacity stack ring buffers (zero heap allocations during `observe`), enforces a
//! Leung–Li bounded entry band `[min_dislocation_bps, max_dislocation_bps]`, and provides a
//! fail-closed half-life time-stop guard.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

/// Maximum supported causal OU ring buffer capacity (stack-allocated, zero hot-path allocation).
pub const MAX_OU_WINDOW: usize = 512;

/// Minimum residual sum-of-squares required before estimating AR(1) mean-reversion slope.
const MIN_OU_SUM_XX: f64 = 1e-12;

/// Trading session regime used to reset slow-equilibrium priors across structural boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub enum SessionRegime {
    OffHours,
    PreMarket,
    UsRth,
    PostMarket,
}

/// Causal stationarity classification from the trailing AR(1) / OU residual estimator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum StationarityStatus {
    WarmingUp,
    Stationary,
    NonStationary,
}

/// Favorable maker entry side under mean reversion toward `alpha_bps`.
///
/// Convention: `basis_bps = maker_mid - hedge_mid` (in basis points).
/// - When `dislocation_bps = basis_bps - alpha_bps < 0`, maker is cheap relative to hedge -> `Buy`.
/// - When `dislocation_bps = basis_bps - alpha_bps > 0`, maker is rich relative to hedge -> `Sell`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum MakerEntrySide {
    Buy,
    Sell,
}

/// Configuration for [`KalmanBasisFilter`].
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct KalmanBasisConfig {
    /// Process variance drift rate in `bps^2 / s` (`> 0.0`).
    pub q_per_sec: f64,
    /// Base observation variance in `bps^2` (`> 0.0`).
    pub r_base_bps2: f64,
    /// Spread-adaptive variance weight (`R_t = r_base_bps2 + spread_r_coeff * spread^2`).
    pub spread_r_coeff: f64,
    /// Initial posterior variance `P_0` in `bps^2` (`> 0.0`).
    pub initial_variance_p: f64,
    /// Posterior variance assigned after a regime change or stale gap reset (`> 0.0`).
    pub reset_variance_p: f64,
    /// Maximum gap in milliseconds before resetting filter state (`1..=86_400_000`).
    pub max_gap_ms: u64,
    /// Fixed causal OU estimation window length in distinct-timestamp samples (`8..=512`).
    pub ou_window: usize,
    /// Minimum samples required before leaving [`StationarityStatus::WarmingUp`] (`4..=ou_window`).
    pub ou_min_samples: usize,
    /// Strict upper bound on AR(1) slope `phi` for stationarity (`0.5 < phi_max_stationary < 1.0`).
    pub phi_max_stationary: f64,
    /// Minimum absolute dislocation in bps required to enter (`> 0.0`).
    pub min_dislocation_bps: f64,
    /// Structural-break cap on absolute dislocation in bps (`> min_dislocation_bps`, `<= 1000.0`).
    pub max_dislocation_bps: f64,
    /// Half-life multiplier triggering a convergence time-stop (`> 0.0`).
    pub time_stop_half_life_mult: f64,
    /// Absolute maximum holding duration cap in milliseconds (`> 0`).
    pub max_hold_ms_cap: u64,
}

impl Default for KalmanBasisConfig {
    fn default() -> Self {
        Self {
            q_per_sec: 0.005,
            r_base_bps2: 4.0,
            spread_r_coeff: 1.0,
            initial_variance_p: 25.0,
            reset_variance_p: 25.0,
            max_gap_ms: 30_000,
            ou_window: 64,
            ou_min_samples: 16,
            phi_max_stationary: 0.98,
            min_dislocation_bps: 3.0,
            max_dislocation_bps: 30.0,
            time_stop_half_life_mult: 3.0,
            max_hold_ms_cap: 300_000,
        }
    }
}

impl KalmanBasisConfig {
    /// Fail closed on non-finite parameters or inconsistent bounds.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.q_per_sec.is_finite() && self.q_per_sec > 0.0,
            "q_per_sec must be finite and > 0.0"
        );
        ensure!(
            self.r_base_bps2.is_finite() && self.r_base_bps2 > 0.0,
            "r_base_bps2 must be finite and > 0.0"
        );
        ensure!(
            self.spread_r_coeff.is_finite() && self.spread_r_coeff >= 0.0,
            "spread_r_coeff must be finite and >= 0.0"
        );
        ensure!(
            self.initial_variance_p.is_finite() && self.initial_variance_p > 0.0,
            "initial_variance_p must be finite and > 0.0"
        );
        ensure!(
            self.reset_variance_p.is_finite() && self.reset_variance_p > 0.0,
            "reset_variance_p must be finite and > 0.0"
        );
        ensure!(
            (1..=86_400_000).contains(&self.max_gap_ms),
            "max_gap_ms must be in 1..=86_400_000"
        );
        ensure!(
            (8..=MAX_OU_WINDOW).contains(&self.ou_window),
            "ou_window must be in 8..={MAX_OU_WINDOW}"
        );
        ensure!(
            (4..=self.ou_window).contains(&self.ou_min_samples),
            "ou_min_samples must be in 4..=ou_window"
        );
        ensure!(
            self.phi_max_stationary.is_finite()
                && self.phi_max_stationary > 0.5
                && self.phi_max_stationary < 1.0,
            "phi_max_stationary must be finite and strictly in (0.5, 1.0)"
        );
        ensure!(
            self.min_dislocation_bps.is_finite() && self.min_dislocation_bps > 0.0,
            "min_dislocation_bps must be finite and > 0.0"
        );
        ensure!(
            self.max_dislocation_bps.is_finite()
                && self.max_dislocation_bps > self.min_dislocation_bps
                && self.max_dislocation_bps <= 1000.0,
            "max_dislocation_bps must be finite, > min_dislocation_bps, and <= 1000.0"
        );
        ensure!(
            self.time_stop_half_life_mult.is_finite() && self.time_stop_half_life_mult > 0.0,
            "time_stop_half_life_mult must be finite and > 0.0"
        );
        ensure!(self.max_hold_ms_cap > 0, "max_hold_ms_cap must be > 0");
        Ok(())
    }
}

/// Causal filter and OU stationarity snapshot emitted by [`KalmanBasisFilter::observe`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct KalmanBasisSnapshot {
    pub now_ms: u64,
    pub regime: SessionRegime,
    pub alpha_bps: f64,
    pub variance_p: f64,
    pub kalman_gain: f64,
    pub observation_r: f64,
    /// Signed residual `basis_bps - alpha_bps` (`< 0` favors maker buy; `> 0` favors maker sell).
    pub dislocation_bps: f64,
    pub phi: Option<f64>,
    pub half_life_ms: Option<f64>,
    pub stationarity: StationarityStatus,
    pub sample_count: usize,
    pub valid_observation: bool,
}

/// Directional maker entry evaluation derived from [`KalmanBasisSnapshot`].
///
/// Follows the repository convention `basis_bps = maker_mid - hedge_mid`:
/// - `maker_buy_dislocation_bps = -dislocation_bps = alpha_bps - basis_bps`
/// - `maker_sell_dislocation_bps = dislocation_bps = basis_bps - alpha_bps`
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct BasisEntryEval {
    pub allowed: bool,
    pub preferred_side: Option<MakerEntrySide>,
    pub abs_dislocation_bps: f64,
    pub maker_buy_dislocation_bps: f64,
    pub maker_sell_dislocation_bps: f64,
}

/// Zero-allocation 1D Kalman slow-equilibrium basis filter with causal OU estimation.
#[derive(Debug, Clone)]
pub struct KalmanBasisFilter {
    config: KalmanBasisConfig,
    initialized: bool,
    current_regime: Option<SessionRegime>,
    last_ms: Option<u64>,
    alpha_bps: f64,
    variance_p: f64,
    residuals_bps: [f64; MAX_OU_WINDOW],
    dt_secs: [f64; MAX_OU_WINDOW],
    ring_head: usize,
    ring_len: usize,
    regime_resets: u64,
    gap_resets: u64,
    rejected_inputs: u64,
}

impl KalmanBasisFilter {
    /// Construct a validated [`KalmanBasisFilter`].
    pub fn new(config: KalmanBasisConfig) -> Result<Self> {
        config.validate()?;
        let initial_p = config.initial_variance_p;
        Ok(Self {
            config,
            initialized: false,
            current_regime: None,
            last_ms: None,
            alpha_bps: 0.0,
            variance_p: initial_p,
            residuals_bps: [0.0; MAX_OU_WINDOW],
            dt_secs: [0.0; MAX_OU_WINDOW],
            ring_head: 0,
            ring_len: 0,
            regime_resets: 0,
            gap_resets: 0,
            rejected_inputs: 0,
        })
    }

    /// Reset active filter state and OU ring buffer while preserving cumulative counters.
    pub fn reset(&mut self) {
        self.initialized = false;
        self.current_regime = None;
        self.last_ms = None;
        self.alpha_bps = 0.0;
        self.variance_p = self.config.initial_variance_p;
        self.ring_head = 0;
        self.ring_len = 0;
    }

    /// Filter configuration.
    pub fn config(&self) -> &KalmanBasisConfig {
        &self.config
    }

    /// Current posterior slow-equilibrium estimate in basis points.
    pub fn alpha_bps(&self) -> f64 {
        self.alpha_bps
    }

    /// Current posterior variance `P` in `bps^2`.
    pub fn variance_p(&self) -> f64 {
        self.variance_p
    }

    /// Active session regime if initialized.
    pub fn current_regime(&self) -> Option<SessionRegime> {
        self.current_regime
    }

    /// Whether at least one valid observation was ingested since initialization or reset.
    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    /// Cumulative session regime transitions observed.
    pub fn regime_resets(&self) -> u64 {
        self.regime_resets
    }

    /// Cumulative stale-gap resets observed.
    pub fn gap_resets(&self) -> u64 {
        self.gap_resets
    }

    /// Cumulative rejected invalid/non-finite/out-of-order observations.
    pub fn rejected_inputs(&self) -> u64 {
        self.rejected_inputs
    }

    /// Advance the Kalman equilibrium and causal OU estimator with one receipt-causal observation.
    ///
    /// Fail-closed behavior:
    /// - Non-finite `basis_bps`, non-finite or negative `combined_spread_bps`, backward timestamps
    ///   (`now_ms < prev_ms`), or arithmetic overflow increment `rejected_inputs` and return a
    ///   fail-closed snapshot (`valid_observation = false`, `StationarityStatus::NonStationary`)
    ///   without mutating valid state.
    /// - Regime changes (`prev_regime != regime`) and gaps (`now_ms - prev_ms > max_gap_ms`) reset
    ///   `alpha_bps = basis_bps`, `variance_p = reset_variance_p`, and clear the OU ring buffer.
    /// - Repeated identical timestamps (`now_ms == prev_ms`) update the Kalman filter with
    ///   `dt_sec = 0.0` without appending a zero-duration sample to the OU ring buffer.
    pub fn observe(
        &mut self,
        now_ms: u64,
        basis_bps: f64,
        combined_spread_bps: f64,
        regime: SessionRegime,
    ) -> KalmanBasisSnapshot {
        let backward_ts = self.last_ms.is_some_and(|prev| now_ms < prev);
        if !basis_bps.is_finite()
            || !combined_spread_bps.is_finite()
            || combined_spread_bps < 0.0
            || backward_ts
        {
            return self.reject_observation(now_ms, regime);
        }

        let observation_r = self.observation_variance(combined_spread_bps);
        if !observation_r.is_finite() || observation_r <= 0.0 {
            return self.reject_observation(now_ms, regime);
        }

        if !self.initialized {
            return self.initialize_state(
                now_ms,
                basis_bps,
                observation_r,
                regime,
                self.config.initial_variance_p,
            );
        }

        if self.current_regime.is_some_and(|prev| prev != regime) {
            self.regime_resets += 1;
            return self.initialize_state(
                now_ms,
                basis_bps,
                observation_r,
                regime,
                self.config.reset_variance_p,
            );
        }

        if self
            .last_ms
            .is_some_and(|prev| now_ms - prev > self.config.max_gap_ms)
        {
            self.gap_resets += 1;
            return self.initialize_state(
                now_ms,
                basis_bps,
                observation_r,
                regime,
                self.config.reset_variance_p,
            );
        }

        let prev_ms = self.last_ms.unwrap_or(now_ms);
        let dt_ms = now_ms - prev_ms;
        let dt_sec = (dt_ms as f64) / 1000.0;
        let p_pred = self.variance_p + self.config.q_per_sec * dt_sec;
        let denom = p_pred + observation_r;
        if !p_pred.is_finite() || !denom.is_finite() || denom <= 0.0 {
            return self.reject_observation(now_ms, regime);
        }

        let kalman_gain = (p_pred / denom).clamp(0.0, 1.0);
        let innovation = basis_bps - self.alpha_bps;
        let alpha_new = self.alpha_bps + kalman_gain * innovation;
        let p_new = ((1.0 - kalman_gain) * p_pred).max(0.0);
        let dislocation_bps = basis_bps - alpha_new;
        if !alpha_new.is_finite() || !p_new.is_finite() || !dislocation_bps.is_finite() {
            return self.reject_observation(now_ms, regime);
        }

        self.alpha_bps = alpha_new;
        self.variance_p = p_new;
        self.last_ms = Some(now_ms);
        self.current_regime = Some(regime);

        if dt_ms > 0 {
            self.push_ou_sample(dislocation_bps, dt_sec);
        }

        let (phi, half_life_ms, stationarity) = self.estimate_ou();
        KalmanBasisSnapshot {
            now_ms,
            regime,
            alpha_bps: alpha_new,
            variance_p: p_new,
            kalman_gain,
            observation_r,
            dislocation_bps,
            phi,
            half_life_ms,
            stationarity,
            sample_count: self.ring_len,
            valid_observation: true,
        }
    }

    /// Return `true` iff `snap` is valid, [`StationarityStatus::Stationary`], and
    /// `|dislocation_bps|` lies within the bounded Leung–Li entry band `[min, max]`.
    pub fn entry_zone_allowed(&self, snap: &KalmanBasisSnapshot) -> bool {
        if !snap.valid_observation
            || snap.stationarity != StationarityStatus::Stationary
            || !snap.dislocation_bps.is_finite()
        {
            return false;
        }
        let abs_disloc = snap.dislocation_bps.abs();
        abs_disloc >= self.config.min_dislocation_bps
            && abs_disloc <= self.config.max_dislocation_bps
    }

    /// Evaluate bounded entry eligibility alongside signed maker-buy / maker-sell dislocations.
    pub fn evaluate_entry(&self, snap: &KalmanBasisSnapshot) -> BasisEntryEval {
        let allowed = self.entry_zone_allowed(snap);
        let finite_disloc = if snap.dislocation_bps.is_finite() {
            snap.dislocation_bps
        } else {
            0.0
        };
        let maker_buy_dislocation_bps = -finite_disloc;
        let maker_sell_dislocation_bps = finite_disloc;
        let preferred_side = if !allowed {
            None
        } else if finite_disloc < 0.0 {
            Some(MakerEntrySide::Buy)
        } else if finite_disloc > 0.0 {
            Some(MakerEntrySide::Sell)
        } else {
            None
        };
        BasisEntryEval {
            allowed,
            preferred_side,
            abs_dislocation_bps: finite_disloc.abs(),
            maker_buy_dislocation_bps,
            maker_sell_dislocation_bps,
        }
    }

    /// Return `true` when an open basis trade must exit: invalid observation,
    /// [`StationarityStatus::NonStationary`], `hold_ms >= max_hold_ms_cap`, or
    /// `hold_ms >= time_stop_half_life_mult * half_life_ms`.
    pub fn should_time_stop(&self, snap: &KalmanBasisSnapshot, hold_ms: u64) -> bool {
        if !snap.valid_observation
            || snap.stationarity == StationarityStatus::NonStationary
            || hold_ms >= self.config.max_hold_ms_cap
        {
            return true;
        }
        snap.half_life_ms.is_some_and(|hl| {
            !hl.is_finite() || (hold_ms as f64) >= self.config.time_stop_half_life_mult * hl
        })
    }

    fn observation_variance(&self, combined_spread_bps: f64) -> f64 {
        self.config.r_base_bps2
            + self.config.spread_r_coeff * combined_spread_bps * combined_spread_bps
    }

    fn reject_observation(&mut self, now_ms: u64, regime: SessionRegime) -> KalmanBasisSnapshot {
        self.rejected_inputs += 1;
        KalmanBasisSnapshot {
            now_ms,
            regime,
            alpha_bps: self.alpha_bps,
            variance_p: self.variance_p,
            kalman_gain: 0.0,
            observation_r: 0.0,
            dislocation_bps: 0.0,
            phi: None,
            half_life_ms: None,
            stationarity: StationarityStatus::NonStationary,
            sample_count: self.ring_len,
            valid_observation: false,
        }
    }

    fn initialize_state(
        &mut self,
        now_ms: u64,
        basis_bps: f64,
        observation_r: f64,
        regime: SessionRegime,
        variance_p: f64,
    ) -> KalmanBasisSnapshot {
        self.initialized = true;
        self.current_regime = Some(regime);
        self.last_ms = Some(now_ms);
        self.alpha_bps = basis_bps;
        self.variance_p = variance_p;
        self.ring_head = 0;
        self.ring_len = 0;
        self.push_ou_sample(0.0, 0.0);

        KalmanBasisSnapshot {
            now_ms,
            regime,
            alpha_bps: basis_bps,
            variance_p,
            kalman_gain: 1.0,
            observation_r,
            dislocation_bps: 0.0,
            phi: None,
            half_life_ms: None,
            stationarity: StationarityStatus::WarmingUp,
            sample_count: self.ring_len,
            valid_observation: true,
        }
    }

    fn push_ou_sample(&mut self, residual_bps: f64, dt_sec: f64) {
        let w = self.config.ou_window;
        self.residuals_bps[self.ring_head] = residual_bps;
        self.dt_secs[self.ring_head] = dt_sec;
        self.ring_head = (self.ring_head + 1) % w;
        if self.ring_len < w {
            self.ring_len += 1;
        }
    }

    /// Estimate causal AR(1) slope `phi` and OU half-life over consecutive residual pairs.
    ///
    /// When residual variance is degenerate (`sum(x_{k-1}^2) <= 1e-12`, e.g., an identically flat
    /// series), the decay rate is unidentifiable; we fail safely to `WarmingUp` (`phi = None`,
    /// `half_life_ms = None`), which blocks new entries without falsely classifying flat
    /// converged equilibrium as a structural break.
    fn estimate_ou(&self) -> (Option<f64>, Option<f64>, StationarityStatus) {
        let n = self.ring_len;
        if n < self.config.ou_min_samples || n < 2 {
            return (None, None, StationarityStatus::WarmingUp);
        }

        let w = self.config.ou_window;
        let oldest = (self.ring_head + w - n) % w;

        let mut sum_xx = 0.0_f64;
        let mut sum_xy = 0.0_f64;
        let mut sum_dt = 0.0_f64;
        let mut pair_count = 0usize;

        for i in 1..n {
            let prev_idx = (oldest + i - 1) % w;
            let curr_idx = (oldest + i) % w;
            let x_prev = self.residuals_bps[prev_idx];
            let x_curr = self.residuals_bps[curr_idx];
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
            || sum_xx <= MIN_OU_SUM_XX
        {
            return (None, None, StationarityStatus::WarmingUp);
        }

        let phi = sum_xy / sum_xx;
        if !phi.is_finite() || phi <= 0.0 || phi >= self.config.phi_max_stationary {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_close(actual: f64, expected: f64, tol: f64) {
        assert!(
            actual.is_finite() && (actual - expected).abs() <= tol,
            "expected {expected} +/- {tol}, got {actual}"
        );
    }

    fn splitmix64(state: &mut u64) -> u64 {
        *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = *state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn uniform01(state: &mut u64) -> f64 {
        let bits = splitmix64(state) >> 11;
        (bits as f64) / ((1u64 << 53) as f64)
    }

    fn irwin_hall_normal(state: &mut u64) -> f64 {
        let mut sum = 0.0;
        for _ in 0..12 {
            sum += uniform01(state);
        }
        sum - 6.0
    }

    fn run_stationary_warmup(
        filter: &mut KalmanBasisFilter,
        start_ms: u64,
        steps: usize,
        amp: f64,
    ) {
        let phi_true = (-std::f64::consts::LN_2 / 5.0_f64).exp();
        let mut rng = 42u64;
        let mut y = 0.0_f64;
        for step in 0..steps {
            let z = irwin_hall_normal(&mut rng);
            y = phi_true * y + amp * z;
            let snap = filter.observe(
                start_ms + (step as u64) * 1_000,
                y,
                1.0,
                SessionRegime::UsRth,
            );
            assert!(snap.valid_observation);
        }
    }

    #[test]
    fn config_validation_rejects_invalid_and_non_finite_parameters() {
        assert!(KalmanBasisConfig::default().validate().is_ok());

        let invalid_cases = [
            KalmanBasisConfig {
                q_per_sec: 0.0,
                ..KalmanBasisConfig::default()
            },
            KalmanBasisConfig {
                q_per_sec: f64::NAN,
                ..KalmanBasisConfig::default()
            },
            KalmanBasisConfig {
                r_base_bps2: -1.0,
                ..KalmanBasisConfig::default()
            },
            KalmanBasisConfig {
                spread_r_coeff: -0.1,
                ..KalmanBasisConfig::default()
            },
            KalmanBasisConfig {
                initial_variance_p: 0.0,
                ..KalmanBasisConfig::default()
            },
            KalmanBasisConfig {
                reset_variance_p: f64::INFINITY,
                ..KalmanBasisConfig::default()
            },
            KalmanBasisConfig {
                max_gap_ms: 0,
                ..KalmanBasisConfig::default()
            },
            KalmanBasisConfig {
                max_gap_ms: 86_400_001,
                ..KalmanBasisConfig::default()
            },
            KalmanBasisConfig {
                ou_window: 7,
                ou_min_samples: 4,
                ..KalmanBasisConfig::default()
            },
            KalmanBasisConfig {
                ou_window: MAX_OU_WINDOW + 1,
                ..KalmanBasisConfig::default()
            },
            KalmanBasisConfig {
                ou_window: 32,
                ou_min_samples: 3,
                ..KalmanBasisConfig::default()
            },
            KalmanBasisConfig {
                ou_window: 32,
                ou_min_samples: 33,
                ..KalmanBasisConfig::default()
            },
            KalmanBasisConfig {
                phi_max_stationary: 0.5,
                ..KalmanBasisConfig::default()
            },
            KalmanBasisConfig {
                phi_max_stationary: 1.0,
                ..KalmanBasisConfig::default()
            },
            KalmanBasisConfig {
                min_dislocation_bps: 0.0,
                ..KalmanBasisConfig::default()
            },
            KalmanBasisConfig {
                min_dislocation_bps: 10.0,
                max_dislocation_bps: 10.0,
                ..KalmanBasisConfig::default()
            },
            KalmanBasisConfig {
                max_dislocation_bps: 1000.1,
                ..KalmanBasisConfig::default()
            },
            KalmanBasisConfig {
                time_stop_half_life_mult: 0.0,
                ..KalmanBasisConfig::default()
            },
            KalmanBasisConfig {
                max_hold_ms_cap: 0,
                ..KalmanBasisConfig::default()
            },
        ];

        for bad in invalid_cases {
            assert!(bad.validate().is_err());
            assert!(KalmanBasisFilter::new(bad).is_err());
        }
    }

    #[test]
    fn wide_spread_dampens_kalman_gain_and_equilibrium_shift() {
        let cfg = KalmanBasisConfig {
            r_base_bps2: 2.0,
            spread_r_coeff: 1.0,
            initial_variance_p: 10.0,
            ..KalmanBasisConfig::default()
        };
        let mut tight = KalmanBasisFilter::new(cfg).unwrap();
        let mut wide = KalmanBasisFilter::new(cfg).unwrap();

        let _ = tight.observe(1_000, 0.0, 1.0, SessionRegime::UsRth);
        let _ = wide.observe(1_000, 0.0, 1.0, SessionRegime::UsRth);

        let snap_tight = tight.observe(2_000, 20.0, 1.0, SessionRegime::UsRth);
        let snap_wide = wide.observe(2_000, 20.0, 40.0, SessionRegime::UsRth);

        assert!(snap_wide.observation_r > snap_tight.observation_r);
        assert!(snap_wide.kalman_gain < snap_tight.kalman_gain);
        assert!(snap_wide.alpha_bps < snap_tight.alpha_bps);
        assert!(snap_tight.alpha_bps > 10.0);
        assert!(snap_wide.alpha_bps < 1.0);
    }

    #[test]
    fn session_regime_transition_resets_filter_and_ou_buffer() {
        let cfg = KalmanBasisConfig {
            ou_window: 16,
            ou_min_samples: 4,
            reset_variance_p: 36.0,
            initial_variance_p: 16.0,
            ..KalmanBasisConfig::default()
        };
        let mut filter = KalmanBasisFilter::new(cfg).unwrap();

        for i in 0..8 {
            let basis = if i % 2 == 0 { 2.0 } else { -2.0 };
            let _ = filter.observe(1_000 + i * 1_000, basis, 1.0, SessionRegime::OffHours);
        }
        assert_eq!(filter.regime_resets(), 0);
        assert_eq!(filter.current_regime(), Some(SessionRegime::OffHours));

        let reset_snap = filter.observe(9_000, 12.5, 1.5, SessionRegime::UsRth);
        assert!(reset_snap.valid_observation);
        assert_eq!(reset_snap.regime, SessionRegime::UsRth);
        assert_eq!(filter.regime_resets(), 1);
        assert_eq!(filter.gap_resets(), 0);
        assert_eq!(reset_snap.sample_count, 1);
        assert_eq!(reset_snap.stationarity, StationarityStatus::WarmingUp);
        assert_close(reset_snap.alpha_bps, 12.5, 1e-12);
        assert_close(reset_snap.variance_p, 36.0, 1e-12);
        assert_close(reset_snap.dislocation_bps, 0.0, 1e-12);
    }

    #[test]
    fn stale_gap_resets_filter_and_ou_buffer() {
        let cfg = KalmanBasisConfig {
            max_gap_ms: 5_000,
            ou_window: 16,
            ou_min_samples: 4,
            reset_variance_p: 30.0,
            initial_variance_p: 10.0,
            ..KalmanBasisConfig::default()
        };
        let mut filter = KalmanBasisFilter::new(cfg).unwrap();

        for i in 0..6 {
            let _ = filter.observe(
                10_000 + i * 1_000,
                1.0 + i as f64,
                1.0,
                SessionRegime::UsRth,
            );
        }
        assert_eq!(filter.gap_resets(), 0);

        let gap_snap = filter.observe(21_000, -7.5, 2.0, SessionRegime::UsRth);
        assert!(gap_snap.valid_observation);
        assert_eq!(filter.gap_resets(), 1);
        assert_eq!(filter.regime_resets(), 0);
        assert_eq!(gap_snap.sample_count, 1);
        assert_eq!(gap_snap.stationarity, StationarityStatus::WarmingUp);
        assert_close(gap_snap.alpha_bps, -7.5, 1e-12);
        assert_close(gap_snap.variance_p, 30.0, 1e-12);
    }

    #[test]
    fn synthetic_stationary_ou_recovers_stationarity_and_half_life() {
        let cfg = KalmanBasisConfig {
            q_per_sec: 0.0005,
            r_base_bps2: 4.0,
            spread_r_coeff: 0.0,
            initial_variance_p: 0.25,
            reset_variance_p: 0.25,
            max_gap_ms: 10_000,
            ou_window: 128,
            ou_min_samples: 32,
            phi_max_stationary: 0.98,
            ..KalmanBasisConfig::default()
        };
        let mut filter = KalmanBasisFilter::new(cfg).unwrap();
        let phi_true = (-std::f64::consts::LN_2 / 5.0_f64).exp();
        let eq = 6.0_f64;
        let mut rng = 42u64;
        let mut y = 0.0_f64;
        let mut last_snap = None;

        for step in 0..256u64 {
            let z = irwin_hall_normal(&mut rng);
            y = phi_true * y + 1.5 * z;
            last_snap = Some(filter.observe(step * 1_000, eq + y, 1.0, SessionRegime::UsRth));
        }

        let snap = last_snap.unwrap();
        assert!(snap.valid_observation);
        assert_eq!(snap.stationarity, StationarityStatus::Stationary);
        assert_eq!(snap.sample_count, 128);
        let phi_hat = snap.phi.expect("phi estimated");
        assert!(
            (phi_hat - phi_true).abs() < 0.08,
            "phi_hat={phi_hat}, phi_true={phi_true}"
        );
        let hl_ms = snap.half_life_ms.expect("half_life_ms estimated");
        assert!(
            (2_500.0..=8_000.0).contains(&hl_ms),
            "recovered half_life_ms={hl_ms} outside expected tolerance around 5000ms"
        );
        assert!((snap.alpha_bps - eq).abs() < 1.0);
    }

    #[test]
    fn trending_and_random_walk_series_are_non_stationary_and_block_entry() {
        let cfg = KalmanBasisConfig {
            q_per_sec: 0.005,
            r_base_bps2: 4.0,
            spread_r_coeff: 0.0,
            ou_window: 64,
            ou_min_samples: 16,
            phi_max_stationary: 0.98,
            min_dislocation_bps: 1.0,
            max_dislocation_bps: 200.0,
            ..KalmanBasisConfig::default()
        };

        // Linear trend -> NonStationary.
        let mut trend_filter = KalmanBasisFilter::new(cfg).unwrap();
        let mut trend_snap = None;
        for step in 0..40u64 {
            trend_snap = Some(trend_filter.observe(
                step * 1_000,
                0.4 * (step as f64),
                1.0,
                SessionRegime::UsRth,
            ));
        }
        let t_snap = trend_snap.unwrap();
        assert_eq!(t_snap.stationarity, StationarityStatus::NonStationary);
        assert!(t_snap.half_life_ms.is_none());
        assert!(!trend_filter.entry_zone_allowed(&t_snap));

        // Random walk with drift -> NonStationary.
        let mut rw_filter = KalmanBasisFilter::new(cfg).unwrap();
        let mut rng = 123u64;
        let mut rw = 0.0_f64;
        let mut rw_snap = None;
        for step in 0..64u64 {
            rw += 0.25 + 0.5 * irwin_hall_normal(&mut rng);
            rw_snap = Some(rw_filter.observe(step * 1_000, rw, 1.0, SessionRegime::UsRth));
        }
        let r_snap = rw_snap.unwrap();
        assert_eq!(r_snap.stationarity, StationarityStatus::NonStationary);
        assert!(r_snap.half_life_ms.is_none());
        assert!(!rw_filter.entry_zone_allowed(&r_snap));
    }

    #[test]
    fn leung_li_bounded_entry_band_and_maker_direction() {
        let cfg = KalmanBasisConfig {
            q_per_sec: 0.0005,
            r_base_bps2: 4.0,
            spread_r_coeff: 0.0,
            initial_variance_p: 0.25,
            reset_variance_p: 0.25,
            max_gap_ms: 10_000,
            ou_window: 128,
            ou_min_samples: 32,
            phi_max_stationary: 0.98,
            min_dislocation_bps: 3.0,
            max_dislocation_bps: 25.0,
            ..KalmanBasisConfig::default()
        };

        // Live stationary probes inside, below, and above the Leung-Li band.
        let mut base = KalmanBasisFilter::new(cfg).unwrap();
        run_stationary_warmup(&mut base, 0, 256, 2.5);
        let anchor_alpha = base.alpha_bps();

        let mut f_small = base.clone();
        let small = f_small.observe(256_000, anchor_alpha + 1.0, 1.0, SessionRegime::UsRth);
        assert_eq!(small.stationarity, StationarityStatus::Stationary);
        assert!(!f_small.entry_zone_allowed(&small));

        let mut f_sell = base.clone();
        let rich = f_sell.observe(256_000, anchor_alpha + 8.0, 1.0, SessionRegime::UsRth);
        assert_eq!(rich.stationarity, StationarityStatus::Stationary);
        assert!(f_sell.entry_zone_allowed(&rich));
        let eval_sell = f_sell.evaluate_entry(&rich);
        assert!(eval_sell.allowed);
        assert_eq!(eval_sell.preferred_side, Some(MakerEntrySide::Sell));
        assert!(eval_sell.maker_sell_dislocation_bps >= 3.0);

        let mut f_buy = base.clone();
        let cheap = f_buy.observe(256_000, anchor_alpha - 8.0, 1.0, SessionRegime::UsRth);
        assert_eq!(cheap.stationarity, StationarityStatus::Stationary);
        assert!(f_buy.entry_zone_allowed(&cheap));
        let eval_buy = f_buy.evaluate_entry(&cheap);
        assert!(eval_buy.allowed);
        assert_eq!(eval_buy.preferred_side, Some(MakerEntrySide::Buy));
        assert!(eval_buy.maker_buy_dislocation_bps >= 3.0);

        let mut f_break = base;
        let structural_break =
            f_break.observe(256_000, anchor_alpha + 45.0, 1.0, SessionRegime::UsRth);
        assert_eq!(
            structural_break.stationarity,
            StationarityStatus::Stationary
        );
        assert!(!f_break.entry_zone_allowed(&structural_break));

        // Exact inclusive boundary checks on [min_dislocation_bps, max_dislocation_bps].
        let synth = |dislocation_bps: f64| KalmanBasisSnapshot {
            now_ms: 1_000,
            regime: SessionRegime::UsRth,
            alpha_bps: 0.0,
            variance_p: 1.0,
            kalman_gain: 0.1,
            observation_r: 4.0,
            dislocation_bps,
            phi: Some(0.85),
            half_life_ms: Some(4_000.0),
            stationarity: StationarityStatus::Stationary,
            sample_count: 64,
            valid_observation: true,
        };
        assert!(!f_break.entry_zone_allowed(&synth(2.999)));
        assert!(f_break.entry_zone_allowed(&synth(3.0)));
        assert!(f_break.entry_zone_allowed(&synth(-25.0)));
        assert!(!f_break.entry_zone_allowed(&synth(25.001)));
    }

    #[test]
    fn time_stop_triggers_on_half_life_non_stationary_and_max_hold_cap() {
        let cfg = KalmanBasisConfig {
            time_stop_half_life_mult: 3.0,
            max_hold_ms_cap: 50_000,
            ..KalmanBasisConfig::default()
        };
        let filter = KalmanBasisFilter::new(cfg).unwrap();

        let stationary = KalmanBasisSnapshot {
            now_ms: 10_000,
            regime: SessionRegime::UsRth,
            alpha_bps: 0.0,
            variance_p: 1.0,
            kalman_gain: 0.1,
            observation_r: 4.0,
            dislocation_bps: 5.0,
            phi: Some(0.85),
            half_life_ms: Some(4_000.0),
            stationarity: StationarityStatus::Stationary,
            sample_count: 32,
            valid_observation: true,
        };
        assert!(!filter.should_time_stop(&stationary, 11_999));
        assert!(filter.should_time_stop(&stationary, 12_000));

        let non_stat = KalmanBasisSnapshot {
            stationarity: StationarityStatus::NonStationary,
            half_life_ms: None,
            ..stationary
        };
        assert!(filter.should_time_stop(&non_stat, 0));

        let invalid = KalmanBasisSnapshot {
            valid_observation: false,
            ..stationary
        };
        assert!(filter.should_time_stop(&invalid, 0));

        let warming = KalmanBasisSnapshot {
            stationarity: StationarityStatus::WarmingUp,
            half_life_ms: None,
            ..stationary
        };
        assert!(!filter.should_time_stop(&warming, 49_999));
        assert!(filter.should_time_stop(&warming, 50_000));
    }

    #[test]
    fn non_finite_negative_spread_and_backward_timestamp_fail_closed() {
        let mut filter = KalmanBasisFilter::new(KalmanBasisConfig::default()).unwrap();
        let _ = filter.observe(10_000, 4.0, 1.0, SessionRegime::UsRth);
        let good = filter.observe(11_000, 6.0, 1.0, SessionRegime::UsRth);
        let saved_alpha = good.alpha_bps;
        let saved_p = good.variance_p;
        let saved_count = good.sample_count;

        let bad_cases = [
            (12_000, f64::NAN, 1.0, SessionRegime::OffHours),
            (12_000, f64::INFINITY, 1.0, SessionRegime::UsRth),
            (12_000, 5.0, f64::NAN, SessionRegime::UsRth),
            (12_000, 5.0, -0.01, SessionRegime::UsRth),
            (10_999, 5.0, 1.0, SessionRegime::UsRth),
        ];

        for (idx, (ts, basis, spread, regime)) in bad_cases.into_iter().enumerate() {
            let snap = filter.observe(ts, basis, spread, regime);
            assert!(!snap.valid_observation);
            assert_eq!(snap.stationarity, StationarityStatus::NonStationary);
            assert!(!filter.entry_zone_allowed(&snap));
            assert!(filter.should_time_stop(&snap, 0));
            assert_eq!(filter.rejected_inputs(), (idx + 1) as u64);
            assert_close(filter.alpha_bps(), saved_alpha, 1e-12);
            assert_close(filter.variance_p(), saved_p, 1e-12);
            assert_eq!(filter.current_regime(), Some(SessionRegime::UsRth));
            assert_eq!(filter.regime_resets(), 0);
            assert_eq!(snap.sample_count, saved_count);
        }
    }

    #[test]
    fn ring_buffer_rollover_stays_bounded_and_handles_same_timestamp() {
        let cfg = KalmanBasisConfig {
            q_per_sec: 0.0005,
            r_base_bps2: 4.0,
            spread_r_coeff: 0.0,
            initial_variance_p: 0.25,
            reset_variance_p: 0.25,
            max_gap_ms: 10_000,
            ou_window: 32,
            ou_min_samples: 12,
            phi_max_stationary: 0.98,
            ..KalmanBasisConfig::default()
        };
        let mut filter = KalmanBasisFilter::new(cfg).unwrap();

        // Start with trending points (would be NonStationary before rollover).
        for step in 0..20u64 {
            let _ = filter.observe(step * 1_000, 0.5 * (step as f64), 1.0, SessionRegime::UsRth);
        }

        // Feed >3x ou_window of stationary OU data so all trending samples roll out.
        let mut rng = 77u64;
        let phi_true = (-std::f64::consts::LN_2 / 4.0_f64).exp();
        let mut y = 0.0_f64;
        let mut last = None;
        for step in 20..140u64 {
            y = phi_true * y + 2.0 * irwin_hall_normal(&mut rng);
            let snap = filter.observe(step * 1_000, 10.0 + y, 1.0, SessionRegime::UsRth);
            assert!(snap.sample_count <= cfg.ou_window);
            last = Some(snap);
        }

        let after_rollover = last.unwrap();
        assert_eq!(after_rollover.sample_count, cfg.ou_window);
        assert_eq!(after_rollover.stationarity, StationarityStatus::Stationary);

        // Duplicate timestamp updates Kalman estimate without incrementing OU ring count or NaN.
        let dup = filter.observe(139_000, 10.0, 1.0, SessionRegime::UsRth);
        assert!(dup.valid_observation);
        assert_eq!(dup.sample_count, cfg.ou_window);
        assert_eq!(dup.stationarity, StationarityStatus::Stationary);
        assert!(dup.half_life_ms.is_some_and(f64::is_finite));

        // Explicit reset preserves cumulative counters.
        let rej_before = filter.rejected_inputs();
        let _ = filter.observe(100_000, 0.0, 1.0, SessionRegime::UsRth);
        assert_eq!(filter.rejected_inputs(), rej_before + 1);
        filter.reset();
        assert!(!filter.is_initialized());
        assert_eq!(filter.rejected_inputs(), rej_before + 1);
    }
}
