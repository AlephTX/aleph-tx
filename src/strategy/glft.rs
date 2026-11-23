//! Normalized Guéant-Lehalle-Fernandez-Tapia (GLFT) closed-form quoting and
//! shallow-book (`0..=70` ticks) log-linear fill-intensity calibration (`BARBELL §2.2 / §6.3.3`).
//!
//! Designed so offline strategy research and benchmarks can evaluate normalized
//! inventory skew `x = clamp(q / Q_max, -1, 1)`, fee-floor / vol-cap clamping
//! diagnostics (highlighting `t50_iso0`-style parameter lockups), and conservative
//! shallow-book OLS intensity fitting without touching frozen baseline paths.

use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};

fn default_one() -> f64 {
    1.0
}

fn default_max_ticks() -> u32 {
    70
}

fn default_min_buckets() -> usize {
    4
}

fn default_min_total_samples() -> u64 {
    50
}

fn default_min_r_squared() -> f64 {
    0.30
}

fn default_min_decay_k() -> f64 {
    1e-4
}

fn default_max_decay_k() -> f64 {
    100.0
}

/// Input parameters for the normalized GLFT closed-form quoter.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GlftParams {
    /// Inventory risk-aversion parameter (`> 0`).
    pub gamma: f64,
    /// Terminal/adverse inventory penalty parameter (`> 0`).
    pub xi: f64,
    /// Discrete order clip size multiplier `Δ` (`> 0`, defaults to `1.0`).
    #[serde(default = "default_one")]
    pub delta_qty: f64,
    /// Baseline shallow-book order arrival intensity `A` in `s^-1` (`> 0`).
    pub intensity_a: f64,
    /// Order arrival exponential decay `k` in `bps^-1` (`> 0`).
    pub decay_k: f64,
    /// Short-horizon price volatility `σ` in `bps / sqrt(s)` (`>= 0`).
    pub sigma_bps_per_sqrt_s: f64,
    /// Normalization scale `Q_max` in base units (`> 0`).
    pub max_inventory_base: f64,
    /// Skew multiplier applied to normalized inventory `x = clamp(q / Q_max, -1, 1)` (`> 0`).
    #[serde(default = "default_one")]
    pub inventory_skew_scale: f64,
    /// Minimum admissible one-sided half-spread in bps (`>= 0`).
    pub fee_floor_bps: f64,
    /// Maximum admissible one-sided half-spread in bps (`> fee_floor_bps`).
    pub max_half_spread_bps: f64,
}

impl GlftParams {
    /// Fail closed on non-finite or out-of-domain GLFT model parameters.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.gamma.is_finite() && self.gamma > 0.0,
            "gamma must be finite and > 0"
        );
        ensure!(
            self.xi.is_finite() && self.xi > 0.0,
            "xi must be finite and > 0"
        );
        ensure!(
            self.delta_qty.is_finite() && self.delta_qty > 0.0,
            "delta_qty must be finite and > 0"
        );
        ensure!(
            self.intensity_a.is_finite() && self.intensity_a > 0.0,
            "intensity_a must be finite and > 0"
        );
        ensure!(
            self.decay_k.is_finite() && self.decay_k > 0.0,
            "decay_k must be finite and > 0"
        );
        ensure!(
            self.sigma_bps_per_sqrt_s.is_finite() && self.sigma_bps_per_sqrt_s >= 0.0,
            "sigma_bps_per_sqrt_s must be finite and >= 0"
        );
        ensure!(
            self.max_inventory_base.is_finite() && self.max_inventory_base > 0.0,
            "max_inventory_base must be finite and > 0"
        );
        ensure!(
            self.inventory_skew_scale.is_finite() && self.inventory_skew_scale > 0.0,
            "inventory_skew_scale must be finite and > 0"
        );
        ensure!(
            self.fee_floor_bps.is_finite() && self.fee_floor_bps >= 0.0,
            "fee_floor_bps must be finite and >= 0"
        );
        ensure!(
            self.max_half_spread_bps.is_finite() && self.max_half_spread_bps > self.fee_floor_bps,
            "max_half_spread_bps must be finite and > fee_floor_bps"
        );
        Ok(())
    }
}

/// Closed-form GLFT constants computed directly from [`GlftParams`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GlftCoefficients {
    pub c1_bps: f64,
    pub c2: f64,
    pub raw_half_spread_bps: f64,
    pub skew_per_unit_bps: f64,
}

impl GlftCoefficients {
    /// Compute BARBELL §2.2 closed-form GLFT coefficients and fail closed on overflow.
    pub fn from_params(params: &GlftParams) -> Result<Self> {
        params.validate()?;
        let xd = params.xi * params.delta_qty;
        ensure!(xd.is_finite() && xd > 0.0, "xi * delta_qty overflowed");

        let ratio = 1.0 + xd / params.decay_k;
        ensure!(ratio.is_finite() && ratio > 1.0, "invalid GLFT log ratio");

        let c1_bps = ratio.ln() / xd;
        let exponent = params.decay_k / xd + 1.0;
        let inner = (params.gamma / (2.0 * params.intensity_a * params.delta_qty * params.decay_k))
            * ratio.powf(exponent);
        ensure!(
            inner.is_finite() && inner > 0.0,
            "GLFT inner variance factor overflowed or became non-positive"
        );

        let c2 = inner.sqrt();
        let raw_half_spread_bps =
            c1_bps + 0.5 * params.delta_qty * params.sigma_bps_per_sqrt_s * c2;
        let skew_per_unit_bps = params.sigma_bps_per_sqrt_s * c2;

        ensure!(
            c1_bps.is_finite()
                && c1_bps > 0.0
                && c2.is_finite()
                && c2 >= 0.0
                && raw_half_spread_bps.is_finite()
                && raw_half_spread_bps > 0.0
                && skew_per_unit_bps.is_finite()
                && skew_per_unit_bps >= 0.0,
            "GLFT closed-form coefficients overflowed to non-finite values"
        );

        Ok(Self {
            c1_bps,
            c2,
            raw_half_spread_bps,
            skew_per_unit_bps,
        })
    }
}

/// Two-sided quote distances from mid produced by [`GlftQuoter::quote`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GlftQuote {
    pub normalized_inventory: f64,
    pub inventory_breached: bool,
    pub reservation_skew_bps: f64,
    pub raw_bid_offset_bps: f64,
    pub raw_ask_offset_bps: f64,
    pub bid_offset_bps: f64,
    pub ask_offset_bps: f64,
    pub effective_full_spread_bps: f64,
    pub bid_clamped_by_floor: bool,
    pub ask_clamped_by_floor: bool,
    pub bid_clamped_by_max: bool,
    pub ask_clamped_by_max: bool,
    pub clamped_by_fee_floor: bool,
    pub clamped_by_max_spread: bool,
}

/// Stateless-per-tick normalized GLFT quoter (zero heap allocations on `quote`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GlftQuoter {
    params: GlftParams,
    coeffs: GlftCoefficients,
}

impl GlftQuoter {
    /// Construct and validate a normalized GLFT quoter.
    pub fn new(params: GlftParams) -> Result<Self> {
        let coeffs = GlftCoefficients::from_params(&params)?;
        Ok(Self { params, coeffs })
    }

    pub fn params(&self) -> &GlftParams {
        &self.params
    }

    pub fn coefficients(&self) -> &GlftCoefficients {
        &self.coeffs
    }

    /// Update fitted intensity `(A, k)` atomically after validation.
    pub fn update_intensity(&mut self, intensity_a: f64, decay_k: f64) -> Result<()> {
        let mut next = self.params;
        next.intensity_a = intensity_a;
        next.decay_k = decay_k;
        let coeffs = GlftCoefficients::from_params(&next)?;
        self.params = next;
        self.coeffs = coeffs;
        Ok(())
    }

    /// Evaluate normalized bid/ask mid offsets in bps with floor/ceiling clamping flags.
    #[inline]
    pub fn quote(&self, inventory_base: f64) -> Result<GlftQuote> {
        ensure!(
            inventory_base.is_finite(),
            "inventory_base must be finite, got {inventory_base}"
        );

        let raw_ratio = inventory_base / self.params.max_inventory_base;
        ensure!(raw_ratio.is_finite(), "normalized inventory overflowed");

        let inventory_breached = raw_ratio.abs() > 1.0;
        let normalized_inventory = raw_ratio.clamp(-1.0, 1.0);
        let reservation_skew_bps =
            normalized_inventory * self.coeffs.skew_per_unit_bps * self.params.inventory_skew_scale;

        // Positive inventory (long) widens the bid offset and tightens the ask offset.
        let raw_bid_offset_bps = self.coeffs.raw_half_spread_bps + reservation_skew_bps;
        let raw_ask_offset_bps = self.coeffs.raw_half_spread_bps - reservation_skew_bps;

        let (bid_offset_bps, bid_clamped_by_floor, bid_clamped_by_max) =
            clamp_offset(raw_bid_offset_bps, &self.params);
        let (ask_offset_bps, ask_clamped_by_floor, ask_clamped_by_max) =
            clamp_offset(raw_ask_offset_bps, &self.params);

        let effective_full_spread_bps = bid_offset_bps + ask_offset_bps;
        ensure!(
            effective_full_spread_bps.is_finite() && effective_full_spread_bps > 0.0,
            "clamped GLFT quote collapsed to non-positive spread"
        );

        Ok(GlftQuote {
            normalized_inventory,
            inventory_breached,
            reservation_skew_bps,
            raw_bid_offset_bps,
            raw_ask_offset_bps,
            bid_offset_bps,
            ask_offset_bps,
            effective_full_spread_bps,
            bid_clamped_by_floor,
            ask_clamped_by_floor,
            bid_clamped_by_max,
            ask_clamped_by_max,
            clamped_by_fee_floor: bid_clamped_by_floor || ask_clamped_by_floor,
            clamped_by_max_spread: bid_clamped_by_max || ask_clamped_by_max,
        })
    }
}

#[inline]
fn clamp_offset(raw_bps: f64, params: &GlftParams) -> (f64, bool, bool) {
    if raw_bps < params.fee_floor_bps {
        (params.fee_floor_bps, true, false)
    } else if raw_bps > params.max_half_spread_bps {
        (params.max_half_spread_bps, false, true)
    } else {
        (raw_bps, false, false)
    }
}

/// Empirical shallow-book arrival observation binned by quote distance from mid.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShallowFillObservation {
    pub distance_ticks: u32,
    pub distance_bps: f64,
    pub arrival_rate_per_sec: f64,
    pub sample_count: u64,
}

/// Configuration for `0..=max_ticks` log-linear fill-intensity OLS regression.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShallowCalibrationConfig {
    #[serde(default = "default_max_ticks")]
    pub max_ticks: u32,
    #[serde(default = "default_min_buckets")]
    pub min_buckets: usize,
    #[serde(default = "default_min_total_samples")]
    pub min_total_samples: u64,
    #[serde(default = "default_min_r_squared")]
    pub min_r_squared: f64,
    #[serde(default = "default_min_decay_k")]
    pub min_decay_k: f64,
    #[serde(default = "default_max_decay_k")]
    pub max_decay_k: f64,
    pub fallback_a: f64,
    pub fallback_k: f64,
}

impl Default for ShallowCalibrationConfig {
    fn default() -> Self {
        Self {
            max_ticks: default_max_ticks(),
            min_buckets: default_min_buckets(),
            min_total_samples: default_min_total_samples(),
            min_r_squared: default_min_r_squared(),
            min_decay_k: default_min_decay_k(),
            max_decay_k: default_max_decay_k(),
            fallback_a: 2.0,
            fallback_k: 0.85,
        }
    }
}

impl ShallowCalibrationConfig {
    /// Fail closed if calibration thresholds or conservative fallbacks are invalid.
    pub fn validate(&self) -> Result<()> {
        ensure!(self.min_buckets >= 2, "min_buckets must be >= 2");
        ensure!(
            self.min_total_samples >= 2,
            "min_total_samples must be >= 2"
        );
        ensure!(
            self.min_r_squared.is_finite() && (0.0..=1.0).contains(&self.min_r_squared),
            "min_r_squared must be finite and in [0, 1]"
        );
        ensure!(
            self.min_decay_k.is_finite() && self.min_decay_k > 0.0,
            "min_decay_k must be finite and > 0"
        );
        ensure!(
            self.max_decay_k.is_finite() && self.max_decay_k > self.min_decay_k,
            "max_decay_k must be finite and > min_decay_k"
        );
        ensure!(
            self.fallback_a.is_finite() && self.fallback_a > 0.0,
            "fallback_a must be finite and > 0"
        );
        ensure!(
            self.fallback_k.is_finite() && self.fallback_k > 0.0,
            "fallback_k must be finite and > 0"
        );
        Ok(())
    }
}

/// Explicit reason why [`calibrate_shallow_intensity`] fell back to conservative `(A, k)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CalibrationFallbackReason {
    InsufficientBuckets,
    InsufficientSamples,
    DegenerateDistanceVariance,
    NonPositiveOrOutOfRangeDecay,
    LowRSquared,
    NonFiniteFit,
}

/// Result of fitting `ln(lambda(delta)) = ln(A) - k * delta_bps` on shallow ticks.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ShallowIntensityFit {
    pub intensity_a: f64,
    pub decay_k: f64,
    pub r_squared: Option<f64>,
    pub retained_buckets: usize,
    pub discarded_deep_buckets: usize,
    pub discarded_invalid_buckets: usize,
    pub total_samples: u64,
    pub used_fallback: bool,
    pub fallback_reason: Option<CalibrationFallbackReason>,
}

fn fallback_fit(
    config: &ShallowCalibrationConfig,
    r_squared: Option<f64>,
    retained_buckets: usize,
    discarded_deep_buckets: usize,
    discarded_invalid_buckets: usize,
    total_samples: u64,
    reason: CalibrationFallbackReason,
) -> ShallowIntensityFit {
    ShallowIntensityFit {
        intensity_a: config.fallback_a,
        decay_k: config.fallback_k,
        r_squared,
        retained_buckets,
        discarded_deep_buckets,
        discarded_invalid_buckets,
        total_samples,
        used_fallback: true,
        fallback_reason: Some(reason),
    }
}

/// Calibrate GLFT `(A, k)` via sample-weighted log-linear OLS over `0..=max_ticks`.
pub fn calibrate_shallow_intensity(
    observations: &[ShallowFillObservation],
    config: &ShallowCalibrationConfig,
) -> Result<ShallowIntensityFit> {
    config.validate()?;

    let mut retained: Vec<(f64, f64, f64)> = Vec::with_capacity(observations.len());
    let mut discarded_deep_buckets = 0usize;
    let mut discarded_invalid_buckets = 0usize;
    let mut total_samples = 0u64;

    for obs in observations {
        if obs.sample_count == 0
            || !obs.distance_bps.is_finite()
            || obs.distance_bps < 0.0
            || !obs.arrival_rate_per_sec.is_finite()
            || obs.arrival_rate_per_sec <= 0.0
        {
            discarded_invalid_buckets += 1;
            continue;
        }
        if obs.distance_ticks > config.max_ticks {
            discarded_deep_buckets += 1;
            continue;
        }
        let log_rate = obs.arrival_rate_per_sec.ln();
        if !log_rate.is_finite() {
            discarded_invalid_buckets += 1;
            continue;
        }
        total_samples = total_samples
            .checked_add(obs.sample_count)
            .ok_or_else(|| anyhow::anyhow!("sample_count sum overflowed u64"))?;
        retained.push((obs.sample_count as f64, obs.distance_bps, log_rate));
    }

    let retained_buckets = retained.len();
    if retained_buckets < config.min_buckets {
        return Ok(fallback_fit(
            config,
            None,
            retained_buckets,
            discarded_deep_buckets,
            discarded_invalid_buckets,
            total_samples,
            CalibrationFallbackReason::InsufficientBuckets,
        ));
    }
    if total_samples < config.min_total_samples {
        return Ok(fallback_fit(
            config,
            None,
            retained_buckets,
            discarded_deep_buckets,
            discarded_invalid_buckets,
            total_samples,
            CalibrationFallbackReason::InsufficientSamples,
        ));
    }

    let sum_w: f64 = retained.iter().map(|(w, _, _)| *w).sum();
    if !sum_w.is_finite() || sum_w <= 0.0 {
        return Ok(fallback_fit(
            config,
            None,
            retained_buckets,
            discarded_deep_buckets,
            discarded_invalid_buckets,
            total_samples,
            CalibrationFallbackReason::NonFiniteFit,
        ));
    }
    let x_bar = retained.iter().map(|(w, x, _)| w * x).sum::<f64>() / sum_w;
    let y_bar = retained.iter().map(|(w, _, y)| w * y).sum::<f64>() / sum_w;

    let mut sxx = 0.0;
    let mut sxy = 0.0;
    let mut sst = 0.0;
    for &(w, x, y) in &retained {
        let dx = x - x_bar;
        let dy = y - y_bar;
        sxx += w * dx * dx;
        sxy += w * dx * dy;
        sst += w * dy * dy;
    }

    if !sxx.is_finite() || !sxy.is_finite() || !sst.is_finite() {
        return Ok(fallback_fit(
            config,
            None,
            retained_buckets,
            discarded_deep_buckets,
            discarded_invalid_buckets,
            total_samples,
            CalibrationFallbackReason::NonFiniteFit,
        ));
    }
    if sxx <= 1e-12 {
        return Ok(fallback_fit(
            config,
            None,
            retained_buckets,
            discarded_deep_buckets,
            discarded_invalid_buckets,
            total_samples,
            CalibrationFallbackReason::DegenerateDistanceVariance,
        ));
    }

    let slope = sxy / sxx;
    let intercept = y_bar - slope * x_bar;
    let decay_k = -slope;
    let intensity_a = intercept.exp();

    let mut sse = 0.0;
    for &(w, x, y) in &retained {
        let residual = y - (intercept + slope * x);
        sse += w * residual * residual;
    }
    let r_squared = if sst <= 1e-12 {
        if sse <= 1e-12 { 1.0 } else { 0.0 }
    } else {
        (1.0 - sse / sst).clamp(0.0, 1.0)
    };

    if !decay_k.is_finite()
        || !intensity_a.is_finite()
        || intensity_a <= 0.0
        || !r_squared.is_finite()
    {
        return Ok(fallback_fit(
            config,
            None,
            retained_buckets,
            discarded_deep_buckets,
            discarded_invalid_buckets,
            total_samples,
            CalibrationFallbackReason::NonFiniteFit,
        ));
    }
    if decay_k < config.min_decay_k || decay_k > config.max_decay_k {
        return Ok(fallback_fit(
            config,
            Some(r_squared),
            retained_buckets,
            discarded_deep_buckets,
            discarded_invalid_buckets,
            total_samples,
            CalibrationFallbackReason::NonPositiveOrOutOfRangeDecay,
        ));
    }
    if r_squared < config.min_r_squared {
        return Ok(fallback_fit(
            config,
            Some(r_squared),
            retained_buckets,
            discarded_deep_buckets,
            discarded_invalid_buckets,
            total_samples,
            CalibrationFallbackReason::LowRSquared,
        ));
    }

    Ok(ShallowIntensityFit {
        intensity_a,
        decay_k,
        r_squared: Some(r_squared),
        retained_buckets,
        discarded_deep_buckets,
        discarded_invalid_buckets,
        total_samples,
        used_fallback: false,
        fallback_reason: None,
    })
}

/// Summary of the BARBELL §2.2 canonical benchmark evaluation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GlftReferenceSummary {
    pub c1_bps: f64,
    pub c2: f64,
    pub raw_half_spread_bps: f64,
    pub skew_per_unit_bps: f64,
    pub neutral_quote: GlftQuote,
    pub long_max_quote: GlftQuote,
    pub clamped_by_fee_floor: bool,
}

/// Structured offline output emitted by `cross_venue glft-bench`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GlftBenchmarkReport {
    pub reference_2_2: GlftReferenceSummary,
    pub clamping_trap_demo: GlftQuote,
    pub shallow_calibration: ShallowIntensityFit,
}

/// Run the offline BARBELL §2.2 GLFT formula check, floor-trap demo, and `0..=70` tick calibration.
pub fn run_glft_benchmark() -> Result<GlftBenchmarkReport> {
    let ref_params = GlftParams {
        gamma: 0.08,
        xi: 0.08,
        delta_qty: 1.0,
        intensity_a: 2.98,
        decay_k: 0.85,
        sigma_bps_per_sqrt_s: 1.335,
        max_inventory_base: 10.0,
        inventory_skew_scale: 1.0,
        fee_floor_bps: 0.40,
        max_half_spread_bps: 50.0,
    };
    let ref_quoter = GlftQuoter::new(ref_params)?;
    let coeffs = ref_quoter.coefficients();
    let neutral_quote = ref_quoter.quote(0.0)?;
    let long_max_quote = ref_quoter.quote(10.0)?;

    let mut trap_params = ref_params;
    trap_params.fee_floor_bps = 2.50;
    let trap_quoter = GlftQuoter::new(trap_params)?;
    let clamping_trap_demo = trap_quoter.quote(0.0)?;

    let true_a = 2.98_f64;
    let true_k = 0.85_f64;
    let mut observations = vec![
        ShallowFillObservation {
            distance_ticks: 1,
            distance_bps: 0.25,
            arrival_rate_per_sec: true_a * (-true_k * 0.25).exp(),
            sample_count: 100,
        },
        ShallowFillObservation {
            distance_ticks: 5,
            distance_bps: 0.75,
            arrival_rate_per_sec: true_a * (-true_k * 0.75).exp(),
            sample_count: 90,
        },
        ShallowFillObservation {
            distance_ticks: 15,
            distance_bps: 1.50,
            arrival_rate_per_sec: true_a * (-true_k * 1.50).exp(),
            sample_count: 80,
        },
        ShallowFillObservation {
            distance_ticks: 40,
            distance_bps: 2.50,
            arrival_rate_per_sec: true_a * (-true_k * 2.50).exp(),
            sample_count: 60,
        },
        ShallowFillObservation {
            distance_ticks: 70,
            distance_bps: 3.50,
            arrival_rate_per_sec: true_a * (-true_k * 3.50).exp(),
            sample_count: 40,
        },
    ];
    // Deep buckets beyond 70 ticks intentionally violate log-linear decay and must be excluded.
    observations.extend([
        ShallowFillObservation {
            distance_ticks: 75,
            distance_bps: 5.0,
            arrival_rate_per_sec: 12.0,
            sample_count: 500,
        },
        ShallowFillObservation {
            distance_ticks: 90,
            distance_bps: 8.0,
            arrival_rate_per_sec: 20.0,
            sample_count: 500,
        },
        ShallowFillObservation {
            distance_ticks: 120,
            distance_bps: 12.0,
            arrival_rate_per_sec: 30.0,
            sample_count: 500,
        },
    ]);

    let shallow_calibration =
        calibrate_shallow_intensity(&observations, &ShallowCalibrationConfig::default())?;

    if shallow_calibration.used_fallback {
        bail!("expected valid shallow calibration in GLFT benchmark");
    }

    Ok(GlftBenchmarkReport {
        reference_2_2: GlftReferenceSummary {
            c1_bps: coeffs.c1_bps,
            c2: coeffs.c2,
            raw_half_spread_bps: coeffs.raw_half_spread_bps,
            skew_per_unit_bps: coeffs.skew_per_unit_bps,
            neutral_quote,
            long_max_quote,
            clamped_by_fee_floor: neutral_quote.clamped_by_fee_floor,
        },
        clamping_trap_demo,
        shallow_calibration,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn barbell_reference_params() -> GlftParams {
        GlftParams {
            gamma: 0.08,
            xi: 0.08,
            delta_qty: 1.0,
            intensity_a: 2.98,
            decay_k: 0.85,
            sigma_bps_per_sqrt_s: 1.335,
            max_inventory_base: 10.0,
            inventory_skew_scale: 1.0,
            fee_floor_bps: 0.40,
            max_half_spread_bps: 50.0,
        }
    }

    #[test]
    fn barbell_2_2_closed_form_coefficients_match_exact_audit() {
        let coeffs = GlftCoefficients::from_params(&barbell_reference_params()).unwrap();
        assert!(
            (coeffs.c1_bps - 1.124).abs() < 0.001,
            "c1={}",
            coeffs.c1_bps
        );
        assert!((coeffs.c2 - 0.212).abs() < 0.001, "c2={}", coeffs.c2);
        assert!(
            (coeffs.raw_half_spread_bps - 1.266).abs() < 0.001,
            "half_spread={}",
            coeffs.raw_half_spread_bps
        );
        assert!(
            (coeffs.skew_per_unit_bps - 0.283).abs() < 0.001,
            "skew={}",
            coeffs.skew_per_unit_bps
        );
    }

    #[test]
    fn normalized_inventory_skew_is_symmetric_and_clamps_breach() {
        let quoter = GlftQuoter::new(barbell_reference_params()).unwrap();
        let half = quoter.coefficients().raw_half_spread_bps;
        let skew = quoter.coefficients().skew_per_unit_bps;

        let flat = quoter.quote(0.0).unwrap();
        assert_eq!(flat.normalized_inventory, 0.0);
        assert!(!flat.inventory_breached);
        assert!((flat.bid_offset_bps - half).abs() < 1e-12);
        assert!((flat.ask_offset_bps - half).abs() < 1e-12);

        let long_half = quoter.quote(5.0).unwrap();
        let short_half = quoter.quote(-5.0).unwrap();
        assert!((long_half.normalized_inventory - 0.5).abs() < 1e-12);
        assert!((long_half.bid_offset_bps - (half + 0.5 * skew)).abs() < 1e-12);
        assert!((long_half.ask_offset_bps - (half - 0.5 * skew)).abs() < 1e-12);
        assert!((long_half.bid_offset_bps - short_half.ask_offset_bps).abs() < 1e-12);
        assert!((long_half.ask_offset_bps - short_half.bid_offset_bps).abs() < 1e-12);

        let breached = quoter.quote(25.0).unwrap();
        assert!(breached.inventory_breached);
        assert_eq!(breached.normalized_inventory, 1.0);
        assert!((breached.bid_offset_bps - (half + skew)).abs() < 1e-12);
        assert!((breached.ask_offset_bps - (half - skew)).abs() < 1e-12);
    }

    #[test]
    fn floor_and_ceiling_clamping_diagnostics_surface_lockups() {
        let mut floor_params = barbell_reference_params();
        floor_params.fee_floor_bps = 2.50;
        let floor_quoter = GlftQuoter::new(floor_params).unwrap();
        let floored = floor_quoter.quote(0.0).unwrap();
        assert!(floored.clamped_by_fee_floor);
        assert!(floored.bid_clamped_by_floor);
        assert!(floored.ask_clamped_by_floor);
        assert!(!floored.clamped_by_max_spread);
        assert_eq!(floored.bid_offset_bps, 2.50);
        assert_eq!(floored.ask_offset_bps, 2.50);

        let mut max_params = barbell_reference_params();
        max_params.max_half_spread_bps = 1.40;
        max_params.inventory_skew_scale = 2.0;
        let max_quoter = GlftQuoter::new(max_params).unwrap();
        let capped = max_quoter.quote(10.0).unwrap();
        assert!(capped.clamped_by_max_spread);
        assert!(capped.bid_clamped_by_max);
        assert!(!capped.ask_clamped_by_max);
        assert_eq!(capped.bid_offset_bps, 1.40);
        assert!(
            (capped.ask_offset_bps
                - (max_quoter.coefficients().raw_half_spread_bps
                    - 2.0 * max_quoter.coefficients().skew_per_unit_bps))
                .abs()
                < 1e-12
        );
    }

    #[test]
    fn shallow_calibrator_recovers_true_decay_and_ignores_deep_ticks() {
        let report = run_glft_benchmark().unwrap();
        let fit = report.shallow_calibration;
        assert!(!fit.used_fallback);
        assert_eq!(fit.fallback_reason, None);
        assert_eq!(fit.retained_buckets, 5);
        assert_eq!(fit.discarded_deep_buckets, 3);
        assert_eq!(fit.discarded_invalid_buckets, 0);
        assert!((fit.intensity_a - 2.98).abs() < 1e-6);
        assert!((fit.decay_k - 0.85).abs() < 1e-6);
        assert!(fit.r_squared.unwrap() > 0.999999);
    }

    #[test]
    fn shallow_calibrator_falls_back_conservatively_on_sparse_degenerate_or_upward_data() {
        let cfg = ShallowCalibrationConfig::default();

        // Sparse bucket count fallback.
        let sparse = calibrate_shallow_intensity(
            &[ShallowFillObservation {
                distance_ticks: 1,
                distance_bps: 0.5,
                arrival_rate_per_sec: 2.0,
                sample_count: 100,
            }],
            &cfg,
        )
        .unwrap();
        assert!(sparse.used_fallback);
        assert_eq!(
            sparse.fallback_reason,
            Some(CalibrationFallbackReason::InsufficientBuckets)
        );
        assert_eq!(sparse.intensity_a, cfg.fallback_a);
        assert_eq!(sparse.decay_k, cfg.fallback_k);

        // Insufficient sample count fallback.
        let low_samples = calibrate_shallow_intensity(
            &[1, 2, 3, 4]
                .into_iter()
                .map(|i| ShallowFillObservation {
                    distance_ticks: i,
                    distance_bps: i as f64 * 0.5,
                    arrival_rate_per_sec: (2.0_f64).exp() * (-0.5 * i as f64).exp(),
                    sample_count: 5,
                })
                .collect::<Vec<_>>(),
            &cfg,
        )
        .unwrap();
        assert!(low_samples.used_fallback);
        assert_eq!(
            low_samples.fallback_reason,
            Some(CalibrationFallbackReason::InsufficientSamples)
        );

        // Zero distance variance fallback.
        let degenerate = calibrate_shallow_intensity(
            &[1, 2, 3, 4]
                .into_iter()
                .map(|i| ShallowFillObservation {
                    distance_ticks: i,
                    distance_bps: 1.0,
                    arrival_rate_per_sec: 2.0,
                    sample_count: 25,
                })
                .collect::<Vec<_>>(),
            &cfg,
        )
        .unwrap();
        assert!(degenerate.used_fallback);
        assert_eq!(
            degenerate.fallback_reason,
            Some(CalibrationFallbackReason::DegenerateDistanceVariance)
        );

        // Upward-sloping intensity (k < 0) fallback.
        let upward = calibrate_shallow_intensity(
            &[1, 2, 3, 4]
                .into_iter()
                .map(|i| ShallowFillObservation {
                    distance_ticks: i,
                    distance_bps: i as f64,
                    arrival_rate_per_sec: (i as f64) * 1.5,
                    sample_count: 25,
                })
                .collect::<Vec<_>>(),
            &cfg,
        )
        .unwrap();
        assert!(upward.used_fallback);
        assert_eq!(
            upward.fallback_reason,
            Some(CalibrationFallbackReason::NonPositiveOrOutOfRangeDecay)
        );

        // Low R^2 noisy data fallback.
        let noisy = calibrate_shallow_intensity(
            &[
                ShallowFillObservation {
                    distance_ticks: 1,
                    distance_bps: 0.5,
                    arrival_rate_per_sec: 3.0,
                    sample_count: 25,
                },
                ShallowFillObservation {
                    distance_ticks: 5,
                    distance_bps: 1.5,
                    arrival_rate_per_sec: 0.5,
                    sample_count: 25,
                },
                ShallowFillObservation {
                    distance_ticks: 10,
                    distance_bps: 2.5,
                    arrival_rate_per_sec: 2.9,
                    sample_count: 25,
                },
                ShallowFillObservation {
                    distance_ticks: 20,
                    distance_bps: 3.5,
                    arrival_rate_per_sec: 0.4,
                    sample_count: 25,
                },
            ],
            &ShallowCalibrationConfig {
                min_r_squared: 0.85,
                ..cfg
            },
        )
        .unwrap();
        assert!(noisy.used_fallback);
        assert_eq!(
            noisy.fallback_reason,
            Some(CalibrationFallbackReason::LowRSquared)
        );
    }

    #[test]
    fn glft_rejects_non_finite_and_invalid_domain_inputs() {
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 0.0, -0.5] {
            let mut p = barbell_reference_params();
            p.gamma = bad;
            assert!(GlftQuoter::new(p).is_err());

            let mut p2 = barbell_reference_params();
            p2.decay_k = bad;
            assert!(GlftQuoter::new(p2).is_err());

            let mut p3 = barbell_reference_params();
            p3.max_inventory_base = bad;
            assert!(GlftQuoter::new(p3).is_err());
        }

        let quoter = GlftQuoter::new(barbell_reference_params()).unwrap();
        assert!(quoter.quote(f64::NAN).is_err());
        assert!(quoter.quote(f64::INFINITY).is_err());

        let bad_cfg = ShallowCalibrationConfig {
            fallback_a: -1.0,
            ..ShallowCalibrationConfig::default()
        };
        assert!(calibrate_shallow_intensity(&[], &bad_cfg).is_err());
    }
}
