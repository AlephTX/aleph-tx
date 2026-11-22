//! Causal 5–60 minute slow-cascade asymmetric entry-side throttle guard (`CRASH-IMP-02`).
//!
//! Detects sustained directional drift over a `window_bars` completed-bar horizon normalized by
//! trailing realized bar volatility (`z_trend = cum_log_ret / (sigma_bar * sqrt(window_bars))`).
//! Enforces:
//! - Strictly completed-bar causality (intra-bar ticks update only the pending bar close and never
//!   push completed bars until a higher `bar_idx` arrives).
//! - Asymmetric entry throttling: downward cascades throttle only `bid_entry_scale` (keeping
//!   `ask_entry_scale == 1.0` so inventory-reducing asks remain unconstrained), and upward rallies
//!   throttle only `ask_entry_scale` (keeping `bid_entry_scale == 1.0`).
//! - Bounded scales `0.0 <= bid_entry_scale, ask_entry_scale <= 1.0` enforced at all times.
//! - Anti-flap hysteresis requiring both `|z_trend| <= exit_z` and `min_hold_ms` elapsed before
//!   releasing an active throttle (while clamping scale `<= last_scale` during hold), plus direct
//!   reversal if the opposite `warn_z` threshold breaches.
//! - Zero heap allocations during `observe` via fixed `[f64; MAX_BARS]` stack ring buffers.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

/// Maximum supported completed-bar ring capacity (stack-allocated, zero hot-path allocation).
pub const MAX_BARS: usize = 256;

/// Minimum RMS bar return floor preventing division by zero on flat tape.
const MIN_SIGMA_BAR: f64 = 1e-9;

/// Flat-tape threshold below which bar returns are treated as identically zero (`z_trend = 0.0`).
const FLAT_TAPE_EPS: f64 = 1e-12;

/// Active slow-cascade directional regime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum CascadeSide {
    None,
    Downward,
    Upward,
}

/// Configuration for [`SlowCascadeGuard`].
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SlowCascadeConfig {
    /// Completed bar duration in milliseconds (`10_000..=600_000`).
    pub bar_ms: u64,
    /// Number of completed bars in the cumulative drift window (`5..=120`).
    pub window_bars: usize,
    /// Trailing completed-bar return count for RMS volatility (`window_bars..=240`).
    pub vol_lookback_bars: usize,
    /// Hysteresis release threshold on `|z_trend|` (`> 0.0`).
    pub exit_z: f64,
    /// Throttle entry threshold on `|z_trend|` (`> exit_z`).
    pub warn_z: f64,
    /// Full entry-halt threshold on `|z_trend|` (`> warn_z`).
    pub halt_z: f64,
    /// Minimum entry scale at the upper warning edge before halting at `halt_z` (`0.0..=1.0`).
    pub min_entry_scale: f64,
    /// Minimum hold duration in milliseconds before an active cascade throttle may release.
    pub min_hold_ms: u64,
    /// Maximum tolerated gap between observations before resetting bar history (`>= 2 * bar_ms`).
    pub max_bar_gap_ms: u64,
}

impl Default for SlowCascadeConfig {
    fn default() -> Self {
        Self {
            bar_ms: 60_000,
            window_bars: 30,
            vol_lookback_bars: 60,
            exit_z: 1.0,
            warn_z: 2.0,
            halt_z: 3.5,
            min_entry_scale: 0.25,
            min_hold_ms: 300_000,
            max_bar_gap_ms: 180_000,
        }
    }
}

impl SlowCascadeConfig {
    /// Fail closed on non-finite thresholds, out-of-range windows, or inverted ordering.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (10_000..=600_000).contains(&self.bar_ms),
            "bar_ms must be in 10_000..=600_000"
        );
        ensure!(
            (5..=120).contains(&self.window_bars),
            "window_bars must be in 5..=120"
        );
        ensure!(
            (self.window_bars..=240).contains(&self.vol_lookback_bars),
            "vol_lookback_bars must be in window_bars..=240"
        );
        ensure!(
            self.exit_z.is_finite() && self.exit_z > 0.0,
            "exit_z must be finite and > 0.0"
        );
        ensure!(
            self.warn_z.is_finite() && self.warn_z > self.exit_z,
            "warn_z must be finite and > exit_z"
        );
        ensure!(
            self.halt_z.is_finite() && self.halt_z > self.warn_z,
            "halt_z must be finite and > warn_z"
        );
        ensure!(
            self.min_entry_scale.is_finite() && (0.0..=1.0).contains(&self.min_entry_scale),
            "min_entry_scale must be finite and in [0.0, 1.0]"
        );
        let min_gap = self.bar_ms.saturating_mul(2);
        ensure!(
            self.max_bar_gap_ms >= min_gap,
            "max_bar_gap_ms must be >= 2 * bar_ms"
        );
        Ok(())
    }
}

/// Asymmetric entry-scale decision emitted by [`SlowCascadeGuard::observe`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct SlowCascadeDecision {
    pub now_ms: u64,
    pub side: CascadeSide,
    pub bid_entry_scale: f64,
    pub ask_entry_scale: f64,
    pub z_trend: f64,
    pub cum_log_ret: f64,
    pub sigma_bar: f64,
    pub completed_bars: usize,
    pub warming_up: bool,
    pub data_healthy: bool,
}

/// Zero-allocation causal slow-cascade asymmetric entry-throttle guard.
#[derive(Debug, Clone)]
pub struct SlowCascadeGuard {
    config: SlowCascadeConfig,
    last_ms: Option<u64>,
    current_bar: Option<(u64, f64)>,
    closes: [f64; MAX_BARS],
    log_rets: [f64; MAX_BARS],
    head: usize,
    bar_count: usize,
    active_side: CascadeSide,
    active_since_ms: Option<u64>,
    active_scale: f64,
    gap_resets: u64,
    rejected_inputs: u64,
}

impl SlowCascadeGuard {
    /// Construct a validated [`SlowCascadeGuard`].
    pub fn new(config: SlowCascadeConfig) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            config,
            last_ms: None,
            current_bar: None,
            closes: [0.0; MAX_BARS],
            log_rets: [0.0; MAX_BARS],
            head: 0,
            bar_count: 0,
            active_side: CascadeSide::None,
            active_since_ms: None,
            active_scale: 1.0,
            gap_resets: 0,
            rejected_inputs: 0,
        })
    }

    /// Clear bar history and active hysteresis while preserving cumulative counters.
    pub fn reset(&mut self) {
        self.last_ms = None;
        self.current_bar = None;
        self.head = 0;
        self.bar_count = 0;
        self.active_side = CascadeSide::None;
        self.active_since_ms = None;
        self.active_scale = 1.0;
    }

    /// Guard configuration.
    pub fn config(&self) -> &SlowCascadeConfig {
        &self.config
    }

    /// Currently latched slow-cascade direction.
    pub fn side(&self) -> CascadeSide {
        self.active_side
    }

    /// Number of completed bars currently retained in the causal ring buffer.
    pub fn completed_bars(&self) -> usize {
        self.bar_count
    }

    /// Cumulative stale-gap resets observed.
    pub fn gap_resets(&self) -> u64 {
        self.gap_resets
    }

    /// Cumulative rejected invalid or out-of-order observations.
    pub fn rejected_inputs(&self) -> u64 {
        self.rejected_inputs
    }

    /// Observe a mid-price tick at `now_ms` and return asymmetric `(bid, ask)` entry scales.
    ///
    /// Causality and fail-closed guarantees:
    /// - Non-finite or `<= 0.0` prices, or backward timestamps (`now_ms < prev_ms`), fail closed
    ///   with `(bid_entry_scale = 0.0, ask_entry_scale = 0.0, data_healthy = false)` without
    ///   mutating completed-bar state or pending bar closes.
    /// - Gaps exceeding `max_bar_gap_ms` reset bar history and hysteresis cleanly into warm-up.
    /// - Intra-bar ticks (`bar_idx == prev_idx`) only update `current_bar`'s pending close; a bar
    ///   is finalized into the completed-bar ring buffer strictly when a higher `bar_idx` arrives.
    ///   Skipped bars within `max_bar_gap_ms` finalize the prior bar once without fabricating
    ///   synthetic intermediate returns.
    pub fn observe(&mut self, now_ms: u64, mid_price: f64) -> SlowCascadeDecision {
        let backward_ts = self.last_ms.is_some_and(|prev| now_ms < prev);
        if !mid_price.is_finite() || mid_price <= 0.0 || backward_ts {
            return self.fail_closed(now_ms);
        }

        if self
            .last_ms
            .is_some_and(|prev| now_ms - prev > self.config.max_bar_gap_ms)
        {
            self.gap_resets += 1;
            self.reset();
        }

        let bar_idx = now_ms / self.config.bar_ms;
        match self.current_bar {
            None => {
                self.current_bar = Some((bar_idx, mid_price));
            }
            Some((prev_idx, prev_close)) if bar_idx > prev_idx => {
                if !self.push_completed_bar(prev_close) {
                    return self.fail_closed(now_ms);
                }
                self.current_bar = Some((bar_idx, mid_price));
            }
            Some((prev_idx, _)) => {
                self.current_bar = Some((prev_idx, mid_price));
            }
        }
        self.last_ms = Some(now_ms);

        let required_bars = self.config.vol_lookback_bars.max(self.config.window_bars) + 1;
        if self.bar_count < required_bars {
            return SlowCascadeDecision {
                now_ms,
                side: CascadeSide::None,
                bid_entry_scale: 1.0,
                ask_entry_scale: 1.0,
                z_trend: 0.0,
                cum_log_ret: 0.0,
                sigma_bar: 0.0,
                completed_bars: self.bar_count,
                warming_up: true,
                data_healthy: true,
            };
        }

        let Some((z_trend, cum_log_ret, sigma_bar)) = self.compute_trend_z() else {
            return self.fail_closed(now_ms);
        };

        let (side, bid_entry_scale, ask_entry_scale) = self.update_hysteresis(now_ms, z_trend);
        debug_assert!((0.0..=1.0).contains(&bid_entry_scale));
        debug_assert!((0.0..=1.0).contains(&ask_entry_scale));

        SlowCascadeDecision {
            now_ms,
            side,
            bid_entry_scale: bid_entry_scale.clamp(0.0, 1.0),
            ask_entry_scale: ask_entry_scale.clamp(0.0, 1.0),
            z_trend,
            cum_log_ret,
            sigma_bar,
            completed_bars: self.bar_count,
            warming_up: false,
            data_healthy: true,
        }
    }

    fn fail_closed(&mut self, now_ms: u64) -> SlowCascadeDecision {
        self.rejected_inputs += 1;
        SlowCascadeDecision {
            now_ms,
            side: self.active_side,
            bid_entry_scale: 0.0,
            ask_entry_scale: 0.0,
            z_trend: 0.0,
            cum_log_ret: 0.0,
            sigma_bar: 0.0,
            completed_bars: self.bar_count,
            warming_up: false,
            data_healthy: false,
        }
    }

    fn push_completed_bar(&mut self, close_px: f64) -> bool {
        if !close_px.is_finite() || close_px <= 0.0 {
            return false;
        }
        let log_ret = if self.bar_count == 0 {
            0.0
        } else {
            let prev_idx = (self.head + MAX_BARS - 1) % MAX_BARS;
            let prev_close = self.closes[prev_idx];
            let ratio = close_px / prev_close;
            let r = ratio.ln();
            if !r.is_finite() {
                return false;
            }
            r
        };

        self.closes[self.head] = close_px;
        self.log_rets[self.head] = log_ret;
        self.head = (self.head + 1) % MAX_BARS;
        if self.bar_count < MAX_BARS {
            self.bar_count += 1;
        }
        true
    }

    fn compute_trend_z(&self) -> Option<(f64, f64, f64)> {
        let last_idx = (self.head + MAX_BARS - 1) % MAX_BARS;
        let start_idx = (self.head + MAX_BARS - 1 - self.config.window_bars) % MAX_BARS;
        let close_last = self.closes[last_idx];
        let close_start = self.closes[start_idx];
        let cum_log_ret = (close_last / close_start).ln();
        if !cum_log_ret.is_finite() {
            return None;
        }

        let lookback = self.config.vol_lookback_bars;
        let mut sum_sq = 0.0_f64;
        for k in 0..lookback {
            let idx = (self.head + MAX_BARS - 1 - k) % MAX_BARS;
            let r = self.log_rets[idx];
            sum_sq += r * r;
        }
        if !sum_sq.is_finite() {
            return None;
        }

        let raw_sigma = (sum_sq / (lookback as f64)).sqrt();
        if !raw_sigma.is_finite() {
            return None;
        }
        let sigma_bar = raw_sigma.max(MIN_SIGMA_BAR);

        let z_trend = if raw_sigma <= FLAT_TAPE_EPS && cum_log_ret.abs() <= FLAT_TAPE_EPS {
            0.0
        } else {
            cum_log_ret / (sigma_bar * (self.config.window_bars as f64).sqrt())
        };
        if !z_trend.is_finite() {
            return None;
        }

        Some((z_trend, cum_log_ret, sigma_bar))
    }

    fn raw_throttle_scale(&self, abs_z: f64) -> f64 {
        if !abs_z.is_finite() {
            return 0.0;
        }
        if abs_z >= self.config.halt_z {
            0.0
        } else if abs_z >= self.config.warn_z {
            let span = self.config.halt_z - self.config.warn_z;
            let frac = ((abs_z - self.config.warn_z) / span).clamp(0.0, 1.0);
            (1.0 - frac * (1.0 - self.config.min_entry_scale)).clamp(0.0, 1.0)
        } else {
            1.0
        }
    }

    /// Update the asymmetric hysteresis state machine.
    ///
    /// While latched in `Downward` or `Upward`, `active_scale` ratchets monotonically downward
    /// (`active_scale = active_scale.min(raw_scale)`) so entry sizing cannot jump prematurely back
    /// toward `1.0` during intermediate bounces before both `|z_trend| <= exit_z` and `min_hold_ms`
    /// are satisfied.
    fn update_hysteresis(&mut self, now_ms: u64, z_trend: f64) -> (CascadeSide, f64, f64) {
        if z_trend <= -self.config.warn_z {
            let raw = self.raw_throttle_scale(-z_trend);
            if self.active_side != CascadeSide::Downward {
                self.active_side = CascadeSide::Downward;
                self.active_since_ms = Some(now_ms);
                self.active_scale = raw;
            } else {
                self.active_scale = self.active_scale.min(raw);
            }
            return (
                CascadeSide::Downward,
                self.active_scale.clamp(0.0, 1.0),
                1.0,
            );
        }

        if z_trend >= self.config.warn_z {
            let raw = self.raw_throttle_scale(z_trend);
            if self.active_side != CascadeSide::Upward {
                self.active_side = CascadeSide::Upward;
                self.active_since_ms = Some(now_ms);
                self.active_scale = raw;
            } else {
                self.active_scale = self.active_scale.min(raw);
            }
            return (CascadeSide::Upward, 1.0, self.active_scale.clamp(0.0, 1.0));
        }

        let held_long_enough = self
            .active_since_ms
            .is_some_and(|since| now_ms.saturating_sub(since) >= self.config.min_hold_ms);

        match self.active_side {
            CascadeSide::None => (CascadeSide::None, 1.0, 1.0),
            CascadeSide::Downward => {
                if z_trend >= -self.config.exit_z && held_long_enough {
                    self.active_side = CascadeSide::None;
                    self.active_since_ms = None;
                    self.active_scale = 1.0;
                    (CascadeSide::None, 1.0, 1.0)
                } else {
                    (
                        CascadeSide::Downward,
                        self.active_scale.clamp(0.0, 1.0),
                        1.0,
                    )
                }
            }
            CascadeSide::Upward => {
                if z_trend <= self.config.exit_z && held_long_enough {
                    self.active_side = CascadeSide::None;
                    self.active_since_ms = None;
                    self.active_scale = 1.0;
                    (CascadeSide::None, 1.0, 1.0)
                } else {
                    (CascadeSide::Upward, 1.0, self.active_scale.clamp(0.0, 1.0))
                }
            }
        }
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

    fn feed_quiet_baseline(
        guard: &mut SlowCascadeGuard,
        start_bar: u64,
        bars: usize,
        bar_ms: u64,
        start_px: f64,
        amp: f64,
    ) -> f64 {
        let mut px = start_px;
        for i in 0..bars {
            let dec = guard.observe((start_bar + i as u64) * bar_ms, px);
            assert!(dec.data_healthy);
            assert_close(dec.bid_entry_scale, 1.0, 1e-12);
            assert_close(dec.ask_entry_scale, 1.0, 1e-12);
            let step = if i % 2 == 0 { amp } else { -amp };
            px *= step.exp();
        }
        px
    }

    #[test]
    fn config_validation_rejects_invalid_bounds_and_non_finite() {
        assert!(SlowCascadeConfig::default().validate().is_ok());

        let invalid_cases = [
            SlowCascadeConfig {
                bar_ms: 9_999,
                ..SlowCascadeConfig::default()
            },
            SlowCascadeConfig {
                bar_ms: 600_001,
                max_bar_gap_ms: 1_500_000,
                ..SlowCascadeConfig::default()
            },
            SlowCascadeConfig {
                window_bars: 4,
                ..SlowCascadeConfig::default()
            },
            SlowCascadeConfig {
                window_bars: 121,
                vol_lookback_bars: 121,
                ..SlowCascadeConfig::default()
            },
            SlowCascadeConfig {
                window_bars: 30,
                vol_lookback_bars: 29,
                ..SlowCascadeConfig::default()
            },
            SlowCascadeConfig {
                vol_lookback_bars: 241,
                ..SlowCascadeConfig::default()
            },
            SlowCascadeConfig {
                exit_z: 0.0,
                ..SlowCascadeConfig::default()
            },
            SlowCascadeConfig {
                exit_z: f64::NAN,
                ..SlowCascadeConfig::default()
            },
            SlowCascadeConfig {
                exit_z: 1.5,
                warn_z: 1.5,
                ..SlowCascadeConfig::default()
            },
            SlowCascadeConfig {
                warn_z: 2.5,
                halt_z: 2.5,
                ..SlowCascadeConfig::default()
            },
            SlowCascadeConfig {
                min_entry_scale: -0.01,
                ..SlowCascadeConfig::default()
            },
            SlowCascadeConfig {
                min_entry_scale: 1.01,
                ..SlowCascadeConfig::default()
            },
            SlowCascadeConfig {
                bar_ms: 60_000,
                max_bar_gap_ms: 119_999,
                ..SlowCascadeConfig::default()
            },
        ];

        for bad in invalid_cases {
            assert!(bad.validate().is_err());
            assert!(SlowCascadeGuard::new(bad).is_err());
        }
    }

    #[test]
    fn steady_downward_slow_cascade_throttles_bid_only() {
        let cfg = SlowCascadeConfig::default();
        let mut guard = SlowCascadeGuard::new(cfg).unwrap();
        let mut px = feed_quiet_baseline(&mut guard, 0, 61, cfg.bar_ms, 100.0, 0.0004);

        let mut warned = None;
        for step in 0..16u64 {
            px *= (-0.00045_f64).exp();
            let dec = guard.observe((61 + step) * cfg.bar_ms, px);
            assert!(dec.data_healthy);
            assert_close(dec.ask_entry_scale, 1.0, 1e-12);
            if dec.side == CascadeSide::Downward && dec.bid_entry_scale > 0.0 {
                warned = Some(dec);
                break;
            }
        }

        let dec = warned.expect("moderate downward drift enters warning band");
        assert_eq!(dec.side, CascadeSide::Downward);
        assert!(dec.z_trend <= -cfg.warn_z && dec.z_trend > -cfg.halt_z);
        assert!(dec.bid_entry_scale >= cfg.min_entry_scale && dec.bid_entry_scale < 1.0);
        assert_close(dec.ask_entry_scale, 1.0, 1e-12);
    }

    #[test]
    fn steady_upward_slow_rally_throttles_ask_only() {
        let cfg = SlowCascadeConfig::default();
        let mut guard = SlowCascadeGuard::new(cfg).unwrap();
        let mut px = feed_quiet_baseline(&mut guard, 0, 61, cfg.bar_ms, 100.0, 0.0004);

        let mut warned = None;
        for step in 0..16u64 {
            px *= 0.00045_f64.exp();
            let dec = guard.observe((61 + step) * cfg.bar_ms, px);
            assert!(dec.data_healthy);
            assert_close(dec.bid_entry_scale, 1.0, 1e-12);
            if dec.side == CascadeSide::Upward && dec.ask_entry_scale > 0.0 {
                warned = Some(dec);
                break;
            }
        }

        let dec = warned.expect("moderate upward drift enters warning band");
        assert_eq!(dec.side, CascadeSide::Upward);
        assert!(dec.z_trend >= cfg.warn_z && dec.z_trend < cfg.halt_z);
        assert!(dec.ask_entry_scale >= cfg.min_entry_scale && dec.ask_entry_scale < 1.0);
        assert_close(dec.bid_entry_scale, 1.0, 1e-12);
    }

    #[test]
    fn severe_downward_cascade_halts_bid_entries_while_preserving_asks() {
        let cfg = SlowCascadeConfig::default();
        let mut guard = SlowCascadeGuard::new(cfg).unwrap();
        let mut px = feed_quiet_baseline(&mut guard, 0, 61, cfg.bar_ms, 100.0, 0.0002);

        let mut last = None;
        for step in 0..31u64 {
            px *= (-0.0010_f64).exp();
            let dec = guard.observe((61 + step) * cfg.bar_ms, px);
            assert!(dec.data_healthy);
            last = Some(dec);
        }

        let dec = last.unwrap();
        assert_eq!(dec.side, CascadeSide::Downward);
        assert!(dec.z_trend <= -cfg.halt_z);
        assert_close(dec.bid_entry_scale, 0.0, 1e-12);
        assert_close(dec.ask_entry_scale, 1.0, 1e-12);
    }

    #[test]
    fn scales_never_exceed_one_across_diverse_regimes() {
        let cfg = SlowCascadeConfig {
            bar_ms: 30_000,
            window_bars: 10,
            vol_lookback_bars: 20,
            min_hold_ms: 120_000,
            max_bar_gap_ms: 120_000,
            ..SlowCascadeConfig::default()
        };
        let mut guard = SlowCascadeGuard::new(cfg).unwrap();

        // Phase 1: flat zero-volatility tape.
        for i in 0..30u64 {
            let dec = guard.observe(i * cfg.bar_ms, 100.0);
            assert!(dec.data_healthy);
            assert!((0.0..=1.0).contains(&dec.bid_entry_scale));
            assert!((0.0..=1.0).contains(&dec.ask_entry_scale));
            if !dec.warming_up {
                assert_close(dec.z_trend, 0.0, 1e-12);
            }
        }

        // Phase 2: violent alternating swings, crash, rally, and recovery.
        let mut px = 100.0_f64;
        for i in 30..160u64 {
            let ret: f64 = match i {
                30..=55 => {
                    if i % 2 == 0 {
                        0.008
                    } else {
                        -0.007
                    }
                }
                56..=80 => -0.004,
                81..=110 => 0.005,
                _ => {
                    if i % 3 == 0 {
                        0.0005
                    } else {
                        -0.0003
                    }
                }
            };
            px *= ret.exp();
            let dec = guard.observe(i * cfg.bar_ms, px);
            assert!(dec.data_healthy);
            assert!(
                (0.0..=1.0).contains(&dec.bid_entry_scale),
                "bid scale out of bounds: {}",
                dec.bid_entry_scale
            );
            assert!(
                (0.0..=1.0).contains(&dec.ask_entry_scale),
                "ask scale out of bounds: {}",
                dec.ask_entry_scale
            );
        }
    }

    #[test]
    fn hysteresis_and_min_hold_prevent_premature_release() {
        let cfg = SlowCascadeConfig {
            bar_ms: 60_000,
            window_bars: 10,
            vol_lookback_bars: 20,
            exit_z: 1.0,
            warn_z: 2.0,
            halt_z: 3.5,
            min_entry_scale: 0.25,
            min_hold_ms: 660_000, // 11 minutes
            max_bar_gap_ms: 180_000,
        };
        let mut guard = SlowCascadeGuard::new(cfg).unwrap();
        let mut px = feed_quiet_baseline(&mut guard, 0, 22, cfg.bar_ms, 100.0, 0.0002);

        // Drive downward cascade into Downward throttle.
        let mut trigger_bar = None;
        let mut bar_idx = 22u64;
        for _ in 0..8 {
            px *= (-0.0008_f64).exp();
            let dec = guard.observe(bar_idx * cfg.bar_ms, px);
            if dec.side == CascadeSide::Downward && trigger_bar.is_none() {
                trigger_bar = Some(bar_idx);
            }
            bar_idx += 1;
        }
        let trig_ms = trigger_bar.expect("cascade triggered") * cfg.bar_ms;

        // Gentle rebound: z_trend crosses above -warn_z and then above -exit_z,
        // but remains latched until BOTH z_trend >= -exit_z AND min_hold_ms elapses.
        let mut saw_mid_band_hold = false;
        let mut saw_exit_z_hold_by_timer = false;
        let mut released_at = None;

        for _ in 0..20 {
            px *= 0.00032_f64.exp();
            let now_ms = bar_idx * cfg.bar_ms;
            let dec = guard.observe(now_ms, px);
            let elapsed = now_ms - trig_ms;
            if dec.z_trend > -cfg.warn_z && dec.z_trend < -cfg.exit_z {
                assert_eq!(dec.side, CascadeSide::Downward);
                assert!(dec.bid_entry_scale < 1.0);
                saw_mid_band_hold = true;
            }
            if dec.z_trend >= -cfg.exit_z && dec.z_trend < cfg.warn_z && elapsed < cfg.min_hold_ms {
                assert_eq!(dec.side, CascadeSide::Downward);
                assert!(dec.bid_entry_scale < 1.0);
                saw_exit_z_hold_by_timer = true;
            }
            if dec.side == CascadeSide::None && released_at.is_none() {
                assert!(dec.z_trend >= -cfg.exit_z);
                assert!(elapsed >= cfg.min_hold_ms);
                assert_close(dec.bid_entry_scale, 1.0, 1e-12);
                assert_close(dec.ask_entry_scale, 1.0, 1e-12);
                released_at = Some(now_ms);
                break;
            }
            bar_idx += 1;
        }

        assert!(saw_mid_band_hold);
        assert!(saw_exit_z_hold_by_timer);
        assert!(released_at.is_some());

        // Direct reversal: trigger Downward, then rally past +warn_z -> switches to Upward.
        for _ in 0..7 {
            bar_idx += 1;
            px *= (-0.0012_f64).exp();
            let _ = guard.observe(bar_idx * cfg.bar_ms, px);
        }
        assert_eq!(guard.side(), CascadeSide::Downward);

        let mut flipped = false;
        for _ in 0..16 {
            bar_idx += 1;
            px *= 0.0020_f64.exp();
            let dec = guard.observe(bar_idx * cfg.bar_ms, px);
            if dec.side == CascadeSide::Upward {
                assert_close(dec.bid_entry_scale, 1.0, 1e-12);
                assert!(dec.ask_entry_scale < 1.0);
                flipped = true;
                break;
            }
        }
        assert!(flipped);
    }

    #[test]
    fn strict_completed_bar_causality_ignores_intraday_crash_until_bar_close() {
        let cfg = SlowCascadeConfig {
            bar_ms: 60_000,
            window_bars: 5,
            vol_lookback_bars: 25,
            min_hold_ms: 60_000,
            max_bar_gap_ms: 180_000,
            ..SlowCascadeConfig::default()
        };
        let mut guard = SlowCascadeGuard::new(cfg).unwrap();
        let px = feed_quiet_baseline(&mut guard, 0, 27, cfg.bar_ms, 100.0, 0.0002);
        let open_bar_27 = guard.observe(27 * cfg.bar_ms, px);
        assert!(open_bar_27.data_healthy);
        assert!(!open_bar_27.warming_up);
        let completed_before = guard.completed_bars();
        assert_eq!(completed_before, 27);
        assert_eq!(open_bar_27.side, CascadeSide::None);

        // Multiple intra-bar crash ticks inside bar 27 MUST NOT finalize bar 27 prematurely.
        let crash_px = px * 0.90;
        for offset in [1_000, 15_000, 45_000, 59_999] {
            let dec = guard.observe(27 * cfg.bar_ms + offset, crash_px);
            assert!(dec.data_healthy);
            assert_eq!(dec.completed_bars, completed_before);
            assert_eq!(dec.side, CascadeSide::None);
            assert_close(dec.bid_entry_scale, 1.0, 1e-12);
        }

        // First tick of bar 28 finalizes bar 27 with `crash_px` and immediately throttles bids.
        let bar_28 = guard.observe(28 * cfg.bar_ms, crash_px);
        assert_eq!(bar_28.completed_bars, completed_before + 1);
        assert_eq!(bar_28.side, CascadeSide::Downward);
        assert!(bar_28.bid_entry_scale < 1.0);
        assert_close(bar_28.ask_entry_scale, 1.0, 1e-12);
    }

    #[test]
    fn stale_bar_gap_resets_history_into_warmup_without_false_trigger() {
        let cfg = SlowCascadeConfig {
            bar_ms: 60_000,
            window_bars: 5,
            vol_lookback_bars: 10,
            min_hold_ms: 60_000,
            max_bar_gap_ms: 180_000,
            ..SlowCascadeConfig::default()
        };
        let mut guard = SlowCascadeGuard::new(cfg).unwrap();
        let mut px = feed_quiet_baseline(&mut guard, 0, 12, cfg.bar_ms, 100.0, 0.0002);

        // Trigger active Downward cascade.
        for step in 12..18u64 {
            px *= (-0.003_f64).exp();
            let _ = guard.observe(step * cfg.bar_ms, px);
        }
        assert_eq!(guard.side(), CascadeSide::Downward);
        let last_ts = 17 * cfg.bar_ms;

        // Gap > max_bar_gap_ms with a different price resets cleanly into warm-up.
        let post_gap = guard.observe(last_ts + cfg.max_bar_gap_ms + 1, px * 0.80);
        assert!(post_gap.data_healthy);
        assert!(post_gap.warming_up);
        assert_eq!(post_gap.completed_bars, 0);
        assert_eq!(post_gap.side, CascadeSide::None);
        assert_close(post_gap.bid_entry_scale, 1.0, 1e-12);
        assert_close(post_gap.ask_entry_scale, 1.0, 1e-12);
        assert_eq!(guard.gap_resets(), 1);
    }

    #[test]
    fn non_finite_non_positive_and_backward_timestamp_fail_closed() {
        let cfg = SlowCascadeConfig {
            bar_ms: 60_000,
            window_bars: 5,
            vol_lookback_bars: 10,
            min_hold_ms: 60_000,
            max_bar_gap_ms: 180_000,
            ..SlowCascadeConfig::default()
        };
        let mut guard = SlowCascadeGuard::new(cfg).unwrap();
        let px = feed_quiet_baseline(&mut guard, 0, 13, cfg.bar_ms, 100.0, 0.0002);
        let saved_bars = guard.completed_bars();

        let bad_inputs = [
            (13 * cfg.bar_ms, f64::NAN),
            (13 * cfg.bar_ms, f64::INFINITY),
            (13 * cfg.bar_ms, 0.0),
            (13 * cfg.bar_ms, -10.0),
            (12 * cfg.bar_ms - 1, px),
        ];

        for (idx, (ts, bad_px)) in bad_inputs.into_iter().enumerate() {
            let dec = guard.observe(ts, bad_px);
            assert!(!dec.data_healthy);
            assert_close(dec.bid_entry_scale, 0.0, 1e-12);
            assert_close(dec.ask_entry_scale, 0.0, 1e-12);
            assert_eq!(guard.rejected_inputs(), (idx + 1) as u64);
            assert_eq!(guard.completed_bars(), saved_bars);
        }

        // Subsequent valid tick recovers using the uncorrupted prior bar history.
        let ok = guard.observe(13 * cfg.bar_ms, px);
        assert!(ok.data_healthy);
        assert_eq!(ok.completed_bars, saved_bars + 1);
        assert_close(ok.bid_entry_scale, 1.0, 1e-12);
        assert_close(ok.ask_entry_scale, 1.0, 1e-12);
    }
}
