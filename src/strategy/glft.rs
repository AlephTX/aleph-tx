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
        ensure!(
            reservation_skew_bps.is_finite()
                && raw_bid_offset_bps.is_finite()
                && raw_ask_offset_bps.is_finite(),
            "normalized GLFT raw quote overflowed"
        );

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

/// Volatility regime for two-state dynamic $\kappa$ calibration (`S1-3`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VolRegime {
    Calm,
    HighVol,
}

/// Configuration for two-regime rolling shallow-book $\kappa$ calibration with `S12-3` stability gates.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RegimeKappaConfig {
    /// Trailing rolling lookback horizon (`ms`, default `600_000` = 10 min).
    pub window_ms: u64,
    /// Minimum cadence between recalibrations in the same regime (`ms`, default `5_000`).
    pub recalibration_cadence_ms: u64,
    /// Short-horizon sigma threshold entering [`VolRegime::HighVol`] (`bps / sqrt(s)`).
    pub high_vol_enter_sigma_bps: f64,
    /// Lower hysteresis threshold returning to [`VolRegime::Calm`] (`bps / sqrt(s)`).
    pub high_vol_exit_sigma_bps: f64,
    /// Maximum allowed single-window notional share before `S12-3` rejects calibration (`< 0.35`).
    pub max_single_window_volume_share: f64,
    /// Shallow calibration settings & conservative fallback in calm regime.
    pub calm_shallow: ShallowCalibrationConfig,
    /// Shallow calibration settings & conservative fallback in high-vol regime.
    pub high_vol_shallow: ShallowCalibrationConfig,
}

impl Default for RegimeKappaConfig {
    fn default() -> Self {
        Self {
            window_ms: 600_000,
            recalibration_cadence_ms: 5_000,
            high_vol_enter_sigma_bps: 1.5,
            high_vol_exit_sigma_bps: 1.2,
            max_single_window_volume_share: 0.35,
            calm_shallow: ShallowCalibrationConfig {
                fallback_a: 2.5,
                fallback_k: 1.85,
                ..ShallowCalibrationConfig::default()
            },
            high_vol_shallow: ShallowCalibrationConfig {
                fallback_a: 1.2,
                fallback_k: 0.55,
                ..ShallowCalibrationConfig::default()
            },
        }
    }
}

impl RegimeKappaConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.window_ms > 0 && self.recalibration_cadence_ms > 0,
            "window_ms and recalibration_cadence_ms must be > 0"
        );
        ensure!(
            self.recalibration_cadence_ms <= self.window_ms,
            "recalibration_cadence_ms must be <= window_ms"
        );
        ensure!(
            self.high_vol_exit_sigma_bps.is_finite()
                && self.high_vol_enter_sigma_bps.is_finite()
                && self.high_vol_exit_sigma_bps > 0.0
                && self.high_vol_enter_sigma_bps > self.high_vol_exit_sigma_bps,
            "require finite 0 < high_vol_exit_sigma_bps < high_vol_enter_sigma_bps"
        );
        ensure!(
            self.max_single_window_volume_share.is_finite()
                && self.max_single_window_volume_share > 0.0
                && self.max_single_window_volume_share <= 1.0,
            "max_single_window_volume_share must be finite and in (0, 1]"
        );
        self.calm_shallow.validate()?;
        self.high_vol_shallow.validate()?;
        Ok(())
    }
}

/// Non-overlapping fill window fed into [`RegimeDynamicKappaCalibrator`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegimeFillWindow {
    pub start_ms: u64,
    pub end_ms: u64,
    pub notional_volume_usd: f64,
    pub observations: Vec<ShallowFillObservation>,
}

/// Result of a regime-aware dynamic $\kappa$ evaluation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegimeKappaCalibration {
    pub regime: VolRegime,
    pub fit: ShallowIntensityFit,
    pub max_window_volume_share: f64,
    pub stability_gate_passed: bool,
    pub gate_reason: Option<String>,
    pub next_recalibration_after_ms: u64,
}

/// Causal two-regime dynamic $\kappa$ calibrator with hysteresis and `S12-3` stability gating.
#[derive(Debug, Clone)]
pub struct RegimeDynamicKappaCalibrator {
    config: RegimeKappaConfig,
    regime: VolRegime,
    last_eval_ms: Option<u64>,
    cached: Option<RegimeKappaCalibration>,
}

impl RegimeDynamicKappaCalibrator {
    pub fn new(config: RegimeKappaConfig) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            config,
            regime: VolRegime::Calm,
            last_eval_ms: None,
            cached: None,
        })
    }

    pub fn current_regime(&self) -> VolRegime {
        self.regime
    }

    pub fn update_and_calibrate(
        &mut self,
        now_ms: u64,
        sigma_bps_per_sqrt_s: f64,
        windows: &[RegimeFillWindow],
    ) -> Result<RegimeKappaCalibration> {
        ensure!(now_ms > 0, "now_ms must be > 0");
        ensure!(
            sigma_bps_per_sqrt_s.is_finite() && sigma_bps_per_sqrt_s >= 0.0,
            "sigma_bps_per_sqrt_s must be finite and >= 0"
        );
        if let Some(prev_ms) = self.last_eval_ms {
            ensure!(
                now_ms >= prev_ms,
                "regressed calibration timestamp {now_ms} < {prev_ms}"
            );
        }

        // Validate ALL windows up front BEFORE mutating `self.regime` or cached state.
        // Enforce strict S12-3 non-overlapping chronological window invariant.
        for (idx, w) in windows.iter().enumerate() {
            ensure!(
                w.start_ms > 0 && w.end_ms > w.start_ms && w.end_ms <= now_ms,
                "invalid or future fill window bounds at index {idx}"
            );
            ensure!(
                w.notional_volume_usd.is_finite() && w.notional_volume_usd >= 0.0,
                "invalid window notional volume at index {idx}"
            );
            if idx > 0 {
                ensure!(
                    w.start_ms >= windows[idx - 1].end_ms,
                    "overlapping or out-of-order fill window at index {idx}"
                );
            }
        }

        let prev_regime = self.regime;
        let next_regime = match prev_regime {
            VolRegime::Calm if sigma_bps_per_sqrt_s >= self.config.high_vol_enter_sigma_bps => {
                VolRegime::HighVol
            }
            VolRegime::HighVol if sigma_bps_per_sqrt_s <= self.config.high_vol_exit_sigma_bps => {
                VolRegime::Calm
            }
            other => other,
        };
        self.regime = next_regime;

        if next_regime == prev_regime
            && let Some(cached) = self.cached.as_ref()
            && now_ms < cached.next_recalibration_after_ms
        {
            self.last_eval_ms = Some(now_ms);
            return Ok(cached.clone());
        }

        let cutoff = now_ms.saturating_sub(self.config.window_ms);
        let active_cfg = match next_regime {
            VolRegime::Calm => &self.config.calm_shallow,
            VolRegime::HighVol => &self.config.high_vol_shallow,
        };

        let mut total_vol = 0.0_f64;
        let mut max_win_vol = 0.0_f64;
        let mut active_windows = 0usize;
        // Pool observations by discrete `distance_ticks` across active windows so repeating
        // a sparse set of ticks across windows cannot artificially pass `min_buckets`.
        let mut pooled: std::collections::BTreeMap<u32, (u64, f64, f64)> =
            std::collections::BTreeMap::new();
        for w in windows.iter().filter(|w| w.end_ms > cutoff) {
            active_windows += 1;
            total_vol += w.notional_volume_usd;
            if w.notional_volume_usd > max_win_vol {
                max_win_vol = w.notional_volume_usd;
            }
            for obs in &w.observations {
                if obs.sample_count == 0
                    || !obs.distance_bps.is_finite()
                    || !obs.arrival_rate_per_sec.is_finite()
                    || obs.distance_bps <= 0.0
                    || obs.arrival_rate_per_sec <= 0.0
                {
                    continue;
                }
                let entry = pooled.entry(obs.distance_ticks).or_insert((0, 0.0, 0.0));
                entry.0 = entry.0.saturating_add(obs.sample_count);
                let weight = obs.sample_count as f64;
                entry.1 += weight * obs.distance_bps;
                entry.2 += weight * obs.arrival_rate_per_sec;
            }
        }
        let combined_obs: Vec<ShallowFillObservation> = pooled
            .into_iter()
            .filter(|(_, (cnt, _, _))| *cnt > 0)
            .map(|(distance_ticks, (cnt, sum_dist, sum_rate))| {
                let w = cnt as f64;
                ShallowFillObservation {
                    distance_ticks,
                    distance_bps: sum_dist / w,
                    arrival_rate_per_sec: sum_rate / w,
                    sample_count: cnt,
                }
            })
            .collect();

        let max_share = if total_vol > 0.0 {
            max_win_vol / total_vol
        } else {
            1.0
        };
        let next_after = now_ms.saturating_add(self.config.recalibration_cadence_ms);

        let result = if active_windows < 2 || total_vol <= 0.0 {
            let fit = calibrate_shallow_intensity(&[], active_cfg)?;
            RegimeKappaCalibration {
                regime: next_regime,
                fit,
                max_window_volume_share: max_share,
                stability_gate_passed: false,
                gate_reason: Some("insufficient_non_overlapping_windows".into()),
                next_recalibration_after_ms: next_after,
            }
        } else if max_share >= self.config.max_single_window_volume_share {
            let fit = calibrate_shallow_intensity(&[], active_cfg)?;
            RegimeKappaCalibration {
                regime: next_regime,
                fit,
                max_window_volume_share: max_share,
                stability_gate_passed: false,
                gate_reason: Some("single_window_volume_concentration".into()),
                next_recalibration_after_ms: next_after,
            }
        } else {
            let fit = calibrate_shallow_intensity(&combined_obs, active_cfg)?;
            RegimeKappaCalibration {
                regime: next_regime,
                fit,
                max_window_volume_share: max_share,
                stability_gate_passed: !fit.used_fallback,
                gate_reason: fit
                    .used_fallback
                    .then(|| format!("shallow_fallback:{:?}", fit.fallback_reason)),
                next_recalibration_after_ms: next_after,
            }
        };

        self.last_eval_ms = Some(now_ms);
        self.cached = Some(result.clone());
        Ok(result)
    }
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

    #[test]
    fn glft_rejects_raw_quote_overflow_while_preserving_extreme_finite_clamps() {
        for inventory_sign in [1.0_f64, -1.0_f64] {
            let mut skew_params = barbell_reference_params();
            skew_params.sigma_bps_per_sqrt_s = 13.35;
            skew_params.inventory_skew_scale = f64::MAX;
            let skew_overflow = GlftQuoter::new(skew_params).unwrap();
            let quantity = inventory_sign * skew_params.max_inventory_base;
            assert!((skew_overflow.coefficients().skew_per_unit_bps * f64::MAX).is_infinite());
            assert_eq!(
                skew_overflow.quote(quantity).unwrap_err().to_string(),
                "normalized GLFT raw quote overflowed"
            );

            let mut raw_params = barbell_reference_params();
            raw_params.sigma_bps_per_sqrt_s = 1.6e308;
            raw_params.inventory_skew_scale = 5.0;
            let sum_overflow = GlftQuoter::new(raw_params).unwrap();
            let coeffs = sum_overflow.coefficients();
            let skew = coeffs.skew_per_unit_bps * raw_params.inventory_skew_scale;
            assert!(skew.is_finite() && (coeffs.raw_half_spread_bps + skew).is_infinite());
            assert_eq!(
                sum_overflow.quote(quantity).unwrap_err().to_string(),
                "normalized GLFT raw quote overflowed"
            );
        }

        let mut extreme_finite = barbell_reference_params();
        extreme_finite.inventory_skew_scale = f64::MAX;
        let q = GlftQuoter::new(extreme_finite).unwrap();
        let res = q.quote(extreme_finite.max_inventory_base).unwrap();
        assert!(res.reservation_skew_bps.is_finite());
        assert!(res.raw_bid_offset_bps.is_finite() && res.raw_ask_offset_bps.is_finite());
        assert_eq!(res.bid_offset_bps, 50.0);
        assert_eq!(res.ask_offset_bps, 0.40);
    }

    fn synth_shallow_obs(a: f64, k: f64) -> Vec<ShallowFillObservation> {
        [(1_u32, 0.25), (5, 0.75), (15, 1.50), (40, 2.50)]
            .into_iter()
            .map(|(distance_ticks, distance_bps)| ShallowFillObservation {
                distance_ticks,
                distance_bps,
                arrival_rate_per_sec: a * (-k * distance_bps).exp(),
                sample_count: 50,
            })
            .collect()
    }

    #[test]
    fn two_regime_dynamic_kappa_enforces_hysteresis_cadence_and_s12_3_volume_gate() {
        let mut cal = RegimeDynamicKappaCalibrator::new(RegimeKappaConfig::default()).unwrap();
        let calm_obs = synth_shallow_obs(2.8, 1.9);
        let hv_obs = synth_shallow_obs(1.1, 0.6);

        // Balanced 3 non-overlapping 2-minute windows in Calm regime -> passes S12-3.
        let calm_windows = vec![
            RegimeFillWindow {
                start_ms: 1_000,
                end_ms: 121_000,
                notional_volume_usd: 1_000.0,
                observations: calm_obs.clone(),
            },
            RegimeFillWindow {
                start_ms: 121_000,
                end_ms: 241_000,
                notional_volume_usd: 1_100.0,
                observations: calm_obs.clone(),
            },
            RegimeFillWindow {
                start_ms: 241_000,
                end_ms: 361_000,
                notional_volume_usd: 1_050.0,
                observations: calm_obs,
            },
        ];
        let c1 = cal
            .update_and_calibrate(361_000, 1.0, &calm_windows)
            .unwrap();
        assert_eq!(c1.regime, VolRegime::Calm);
        assert!(c1.stability_gate_passed);
        assert!((c1.fit.decay_k - 1.9).abs() < 1e-3);

        // Within 5s cadence in same regime -> reuses cached calibration without jittering.
        let c_cached = cal
            .update_and_calibrate(364_000, 1.35, &calm_windows)
            .unwrap();
        assert_eq!(c_cached.regime, VolRegime::Calm);
        assert_eq!(c_cached.fit.decay_k, c1.fit.decay_k);

        // Single-window dominance (>= 35% of volume, like July 1 60% spike) triggers S12-3 fallback!
        let skewed_windows = vec![
            RegimeFillWindow {
                start_ms: 1_000,
                end_ms: 121_000,
                notional_volume_usd: 6_000.0,
                observations: hv_obs.clone(),
            },
            RegimeFillWindow {
                start_ms: 121_000,
                end_ms: 241_000,
                notional_volume_usd: 2_000.0,
                observations: hv_obs.clone(),
            },
            RegimeFillWindow {
                start_ms: 241_000,
                end_ms: 361_000,
                notional_volume_usd: 2_000.0,
                observations: hv_obs.clone(),
            },
        ];
        let c_skew = cal
            .update_and_calibrate(370_000, 1.6, &skewed_windows)
            .unwrap();
        assert_eq!(c_skew.regime, VolRegime::HighVol);
        assert!(!c_skew.stability_gate_passed);
        assert_eq!(
            c_skew.gate_reason.as_deref(),
            Some("single_window_volume_concentration")
        );
        assert!(c_skew.fit.used_fallback);
        assert!((c_skew.fit.decay_k - 0.55).abs() < 1e-9);

        // Hysteresis keeps HighVol at sigma=1.35 (> 1.2 exit), drops back to Calm at 1.15.
        let balanced_hv: Vec<_> = skewed_windows
            .iter()
            .map(|w| RegimeFillWindow {
                notional_volume_usd: 3_000.0,
                ..w.clone()
            })
            .collect();
        let c_hold = cal
            .update_and_calibrate(376_000, 1.35, &balanced_hv)
            .unwrap();
        assert_eq!(c_hold.regime, VolRegime::HighVol);
        assert!(c_hold.stability_gate_passed);
        assert!((c_hold.fit.decay_k - 0.6).abs() < 1e-3);

        let c_exit = cal
            .update_and_calibrate(382_000, 1.15, &balanced_hv)
            .unwrap();
        assert_eq!(c_exit.regime, VolRegime::Calm);

        // Overlapping windows fail closed.
        let mut overlapping = balanced_hv;
        overlapping[1].start_ms = overlapping[0].end_ms - 1;
        assert!(
            cal.update_and_calibrate(390_000, 1.0, &overlapping)
                .is_err()
        );
    }

    #[test]
    fn two_regime_dynamic_kappa_keeps_regime_atomic_on_error_and_pools_duplicate_ticks() {
        let mut cal = RegimeDynamicKappaCalibrator::new(RegimeKappaConfig::default()).unwrap();
        assert_eq!(cal.current_regime(), VolRegime::Calm);

        // Invalid overlapping windows with high-vol sigma must fail closed WITHOUT mutating `self.regime`.
        let obs2 = vec![
            ShallowFillObservation {
                distance_ticks: 1,
                distance_bps: 0.4,
                arrival_rate_per_sec: 3.0,
                sample_count: 100,
            },
            ShallowFillObservation {
                distance_ticks: 2,
                distance_bps: 0.8,
                arrival_rate_per_sec: 1.5,
                sample_count: 100,
            },
        ];
        let invalid_windows = vec![
            RegimeFillWindow {
                start_ms: 1_000,
                end_ms: 50_000,
                notional_volume_usd: 1_000.0,
                observations: obs2.clone(),
            },
            RegimeFillWindow {
                start_ms: 40_000,
                end_ms: 90_000,
                notional_volume_usd: 1_000.0,
                observations: obs2.clone(),
            },
        ];
        assert!(
            cal.update_and_calibrate(90_000, 2.5, &invalid_windows)
                .is_err()
        );
        assert_eq!(cal.current_regime(), VolRegime::Calm);

        // 4 valid windows that each only cover ticks {1, 2} must pool into 2 buckets
        // and fail min_buckets = 4 fallback rather than counting 8 raw entries.
        let sparse_windows: Vec<_> = (0..4_u64)
            .map(|i| RegimeFillWindow {
                start_ms: 1_000 + i * 30_000,
                end_ms: 31_000 + i * 30_000,
                notional_volume_usd: 1_000.0,
                observations: obs2.clone(),
            })
            .collect();
        let pooled_out = cal
            .update_and_calibrate(121_000, 1.0, &sparse_windows)
            .unwrap();
        assert!(!pooled_out.stability_gate_passed);
        assert_eq!(pooled_out.fit.retained_buckets, 2);
        assert_eq!(
            pooled_out.fit.fallback_reason,
            Some(CalibrationFallbackReason::InsufficientBuckets)
        );
    }
}
