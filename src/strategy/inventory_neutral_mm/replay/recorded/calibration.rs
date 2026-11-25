//! Opt-in maker calibration telemetry for the recorded replay (`S10-1`).
//!
//! Emits the raw inputs needed by `scripts/calibrate_mm_markout_and_as.py`:
//! - resting-quote exposure time and maker fills by distance from mid (for
//!   the `lambda(delta) = A * exp(-kappa * delta)` fit);
//! - per-fill markout histograms in bps at each labelled horizon;
//! - realized mid log-return variance per second (pooled `sigma`).
//!
//! All storage is fixed-size and preallocated; nothing here allocates on the
//! replay hot path. Exposure intersects each order lifetime with the observed lifetime
//! of each fresh public mid. The fit is endogenous to the distances the strategy actually
//! quoted, so it must not be extrapolated outside populated bins.
use serde::Serialize;
use std::collections::VecDeque;

pub(super) const DISTANCE_BIN_BPS: f64 = 0.5;
/// Bins `[0, 0.5), [0.5, 1.0), ... [19.5, 20.0)` plus one overflow bin.
pub(super) const DISTANCE_BINS: usize = 41;
pub(super) const MARKOUT_BIN_BPS: f64 = 0.5;
pub(super) const MARKOUT_MIN_BPS: f64 = -50.0;
/// Bins covering `[-50, +50)` bps; tails are counted separately.
pub(super) const MARKOUT_BINS: usize = 200;
pub(super) const HORIZONS: usize = 3;
/// Mid-to-mid intervals longer than this are a data gap, not volatility.
const MAX_SIGMA_GAP_MS: u64 = 5_000;

#[derive(Clone, Copy)]
pub(super) struct MarkoutHistogram {
    counts: [u64; MARKOUT_BINS],
    underflow: u64,
    overflow: u64,
}

impl Default for MarkoutHistogram {
    fn default() -> Self {
        Self {
            counts: [0; MARKOUT_BINS],
            underflow: 0,
            overflow: 0,
        }
    }
}

impl MarkoutHistogram {
    fn record(&mut self, bps: f64) {
        if !bps.is_finite() {
            return;
        }
        let pos = (bps - MARKOUT_MIN_BPS) / MARKOUT_BIN_BPS;
        if pos < 0.0 {
            self.underflow += 1;
        } else if pos >= MARKOUT_BINS as f64 {
            self.overflow += 1;
        } else {
            self.counts[pos as usize] += 1;
        }
    }
}

#[derive(Clone, Copy)]
struct ObservedMid {
    received_ms: u64,
    mid: Option<f64>,
    valid_until_ms: u64,
}

#[derive(Default, Clone, Copy)]
struct SigmaAccum {
    sum_sq_log_return: f64,
    observed_ms: u64,
    samples: u64,
    gaps: u64,
}

impl SigmaAccum {
    fn record(&mut self, dt: u64, expired: bool, last_mid: f64, mid: f64) {
        if dt > MAX_SIGMA_GAP_MS || expired {
            self.gaps += 1;
        } else if dt > 0 {
            let r = (mid / last_mid).ln();
            if r.is_finite() {
                self.sum_sq_log_return += r * r;
                self.observed_ms += dt;
                self.samples += 1;
            }
        }
    }

    fn observed_secs(&self) -> f64 {
        self.observed_ms as f64 / 1_000.0
    }

    fn per_sqrt_sec(&self) -> Option<f64> {
        (self.observed_ms > 0).then(|| (self.sum_sq_log_return / self.observed_secs()).sqrt())
    }
}

pub(super) struct CalibrationAccum {
    horizons_ms: [u64; HORIZONS],
    exposure_ms: [u64; DISTANCE_BINS],
    fills: [u64; DISTANCE_BINS],
    fill_turnover: [f64; DISTANCE_BINS],
    unpriced_fills: u64,
    markouts: [MarkoutHistogram; HORIZONS],
    observations: VecDeque<ObservedMid>,
    retention_ms: u64,
    sigma: SigmaAccum,
    decision_sigma_interval: Option<(u64, u64)>,
    decision_sigma: SigmaAccum,
}

fn distance_bin(price: f64, is_ask: bool, mid: f64) -> Option<usize> {
    if !(price.is_finite() && mid.is_finite() && mid > 0.0) {
        return None;
    }
    let signed = if is_ask { price - mid } else { mid - price };
    let bps = (signed / mid * 10_000.0).max(0.0);
    Some(((bps / DISTANCE_BIN_BPS) as usize).min(DISTANCE_BINS - 1))
}

impl CalibrationAccum {
    pub(super) fn new(horizons_ms: [u64; HORIZONS], retention_ms: u64) -> Self {
        Self {
            horizons_ms,
            exposure_ms: [0; DISTANCE_BINS],
            fills: [0; DISTANCE_BINS],
            fill_turnover: [0.0; DISTANCE_BINS],
            unpriced_fills: 0,
            markouts: [MarkoutHistogram::default(); HORIZONS],
            observations: VecDeque::with_capacity(retention_ms as usize + 2),
            retention_ms,
            sigma: SigmaAccum::default(),
            decision_sigma_interval: None,
            decision_sigma: SigmaAccum::default(),
        }
    }

    pub(super) fn with_decision_sigma_interval(mut self, trade_from_ms: u64, end_ms: u64) -> Self {
        debug_assert!(trade_from_ms < end_ms);
        if trade_from_ms < end_ms {
            self.decision_sigma_interval = Some((trade_from_ms, end_ms));
        }
        self
    }

    /// The caller supplies the orders live just before a book changes, and
    /// separately accrues any order removed between consecutive book events.
    pub(super) fn accrue_order(
        &mut self,
        end_ms: u64,
        activated_ms: u64,
        price: f64,
        is_ask: bool,
    ) {
        if let Some(obs) = self.observations.back()
            && let Some(bin) = obs.mid.and_then(|mid| distance_bin(price, is_ask, mid))
        {
            let end = end_ms.min(obs.valid_until_ms);
            self.exposure_ms[bin] += end.saturating_sub(obs.received_ms.max(activated_ms));
        }
    }

    pub(super) fn on_book<I>(&mut self, now_ms: u64, next: Option<(f64, u64)>, live: I)
    where
        I: IntoIterator<Item = (u64, f64, bool)>,
    {
        for (activated_ms, price, is_ask) in live {
            self.accrue_order(now_ms, activated_ms, price, is_ask);
        }
        if let Some(prev) = self.observations.back()
            && let (Some(last_mid), Some((mid, _))) = (prev.mid, next)
        {
            let dt = now_ms.saturating_sub(prev.received_ms);
            let expired = now_ms > prev.valid_until_ms;
            self.sigma.record(dt, expired, last_mid, mid);
            if let Some((start, end)) = self.decision_sigma_interval
                && (start..end).contains(&prev.received_ms)
                && (start..end).contains(&now_ms)
            {
                self.decision_sigma.record(dt, expired, last_mid, mid);
            }
        }
        // Keep the final observation before the earliest admissible print as
        // an anchor, plus at most one update per millisecond through receipt.
        let oldest = now_ms.saturating_sub(self.retention_ms);
        while self
            .observations
            .get(1)
            .is_some_and(|obs| obs.received_ms < oldest)
        {
            self.observations.pop_front();
        }
        if self
            .observations
            .back()
            .is_some_and(|obs| obs.received_ms == now_ms)
        {
            self.observations.pop_back();
        }
        self.observations.push_back(ObservedMid {
            received_ms: now_ms,
            mid: next.map(|(mid, _)| mid),
            valid_until_ms: next.map_or(now_ms, |(_, until)| until),
        });
    }

    /// Use the last book actually received strictly before the earliest
    /// possible execution. An observation of an offline feed breaks the join.
    pub(super) fn observable_mid(&self, at_ms: u64) -> Option<f64> {
        let end = self.observations.partition_point(|o| o.received_ms < at_ms);
        let obs = self.observations.get(end.checked_sub(1)?)?;
        obs.mid.filter(|_| at_ms <= obs.valid_until_ms)
    }

    pub(super) fn on_fill(&mut self, price: f64, is_ask: bool, qty: f64, mid: Option<f64>) {
        match mid.and_then(|mid| distance_bin(price, is_ask, mid)) {
            Some(bin) => {
                self.fills[bin] += 1;
                self.fill_turnover[bin] += qty * price;
            }
            None => self.unpriced_fills += 1,
        }
    }

    /// `signed_quantity > 0` is a maker buy. Markout is in bps of fill price.
    pub(super) fn on_mark(&mut self, horizon: usize, signed_quantity: f64, price: f64, mid: f64) {
        if horizon < HORIZONS && price > 0.0 && signed_quantity != 0.0 {
            let bps = signed_quantity.signum() * (mid - price) / price * 10_000.0;
            self.markouts[horizon].record(bps);
        }
    }

    pub(super) fn finish(&self) -> CalibrationReport {
        let distance_bins = (0..DISTANCE_BINS)
            .map(|i| DistanceBin {
                lo_bps: i as f64 * DISTANCE_BIN_BPS,
                hi_bps: (i + 1 < DISTANCE_BINS).then_some((i + 1) as f64 * DISTANCE_BIN_BPS),
                exposure_secs: self.exposure_ms[i] as f64 / 1_000.0,
                fill_count: self.fills[i],
                fill_turnover: self.fill_turnover[i],
            })
            .collect();
        let markout_histograms = self
            .horizons_ms
            .iter()
            .zip(&self.markouts)
            .map(|(horizon_ms, hist)| HorizonHistogram {
                horizon_ms: *horizon_ms,
                counts: hist.counts.to_vec(),
                underflow: hist.underflow,
                overflow: hist.overflow,
            })
            .collect();
        CalibrationReport {
            exposure_model: "resting_lifetime_fresh_book",
            fill_distance_model: "pre_execution_received_mid",
            distance_bin_bps: DISTANCE_BIN_BPS,
            distance_bins,
            unpriced_fills: self.unpriced_fills,
            markout_bin_bps: MARKOUT_BIN_BPS,
            markout_min_bps: MARKOUT_MIN_BPS,
            markout_histograms,
            sum_sq_log_return: self.sigma.sum_sq_log_return,
            sigma_observed_secs: self.sigma.observed_secs(),
            sigma_samples: self.sigma.samples,
            sigma_gaps: self.sigma.gaps,
            sigma_per_sqrt_sec: self.sigma.per_sqrt_sec(),
            decision_phase_sigma: self.decision_sigma_interval.map(|(trade_from_ms, end_ms)| {
                DecisionPhaseSigma {
                    scope: "receipt_half_open",
                    trade_from_ms,
                    end_ms,
                    sum_sq_log_return: self.decision_sigma.sum_sq_log_return,
                    sigma_observed_secs: self.decision_sigma.observed_secs(),
                    sigma_samples: self.decision_sigma.samples,
                    sigma_gaps: self.decision_sigma.gaps,
                    sigma_per_sqrt_sec: self.decision_sigma.per_sqrt_sec(),
                }
            }),
        }
    }
}

#[derive(Serialize)]
pub(super) struct DistanceBin {
    lo_bps: f64,
    hi_bps: Option<f64>,
    exposure_secs: f64,
    fill_count: u64,
    fill_turnover: f64,
}

#[derive(Serialize)]
pub(super) struct HorizonHistogram {
    horizon_ms: u64,
    counts: Vec<u64>,
    underflow: u64,
    overflow: u64,
}

#[derive(Serialize)]
pub(super) struct DecisionPhaseSigma {
    scope: &'static str,
    trade_from_ms: u64,
    end_ms: u64,
    sum_sq_log_return: f64,
    sigma_observed_secs: f64,
    sigma_samples: u64,
    sigma_gaps: u64,
    sigma_per_sqrt_sec: Option<f64>,
}

#[derive(Serialize)]
pub(super) struct CalibrationReport {
    exposure_model: &'static str,
    fill_distance_model: &'static str,
    distance_bin_bps: f64,
    distance_bins: Vec<DistanceBin>,
    unpriced_fills: u64,
    markout_bin_bps: f64,
    markout_min_bps: f64,
    markout_histograms: Vec<HorizonHistogram>,
    sum_sq_log_return: f64,
    sigma_observed_secs: f64,
    sigma_samples: u64,
    sigma_gaps: u64,
    sigma_per_sqrt_sec: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    decision_phase_sigma: Option<DecisionPhaseSigma>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accrues_only_the_part_of_each_order_lifetime_observed_at_a_fresh_mid() {
        let mut a = CalibrationAccum::new([1_000, 5_000, 30_000], 2_200);
        a.on_book(1_000, Some((100.0, 2_200)), []);
        assert!(a.observable_mid(999).is_none());
        assert_eq!(a.observable_mid(1_001), Some(100.0));
        a.accrue_order(1_990, 1_000, 99.988, false);
        a.on_book(2_000, Some((100.0, 2_200)), [(1_999, 99.988, false)]);
        a.on_book(3_000, None, [(1_999, 99.988, false)]);
        a.accrue_order(4_000, 1_999, 99.988, false);
        assert!(a.observable_mid(4_000).is_none());
        assert_eq!(a.exposure_ms[2], 990 + 1 + 200);
    }

    #[test]
    fn delayed_print_uses_the_observed_pre_execution_book_and_respects_gaps() {
        let mut a = CalibrationAccum::new([1_000, 5_000, 30_000], 30);
        a.on_book(10, Some((100.0, 50)), []);
        a.on_book(20, Some((101.0, 50)), []);
        a.on_book(20, Some((102.0, 50)), []);
        a.on_book(25, None, []);
        a.on_book(30, Some((103.0, 50)), []);
        assert_eq!(a.observable_mid(20), Some(100.0));
        assert_eq!(a.observable_mid(23), Some(102.0));
        assert!(a.observable_mid(28).is_none());
        assert!(a.observable_mid(51).is_none());
        for ms in 31..200 {
            a.on_book(ms, Some((104.0, ms + 20)), []);
        }
        assert!(a.observations.len() <= 32);
        assert_eq!(a.observable_mid(169), Some(104.0));
        assert!(a.observable_mid(100).is_none());
    }

    #[test]
    fn exposure_fills_markouts_and_sigma_are_binned_without_lookahead() {
        let mut acc = CalibrationAccum::new([1_000, 5_000, 30_000], 2_200);
        // First event only anchors the mid; no interval to attribute yet.
        acc.on_book(0, Some((100.0, 2_200)), []);
        // 1.2 bp bid and 2.7 bp ask live for 400 ms measured against mid 100.0.
        acc.on_book(
            400,
            Some((100.01, 2_600)),
            [(0, 99.988, false), (0, 100.027, true)],
        );
        // Only the first 2.2 seconds of this gap have a fresh observed mid.
        acc.on_book(10_400, Some((100.01, 12_600)), [(0, 99.988, false)]);
        acc.on_fill(99.988, false, 0.5, Some(100.0));
        acc.on_fill(100.0, true, 0.5, None);
        acc.on_mark(0, 0.5, 100.0, 99.953); // buy marked lower -> -4.7 bps
        acc.on_mark(0, -0.5, 100.0, 99.953); // sell marked lower -> +4.7 bps
        acc.on_mark(1, 0.5, 100.0, 99.0); // -100 bps underflow
        let rep = acc.finish();
        let json = serde_json::to_value(&rep).unwrap();
        let bins = json["distance_bins"].as_array().unwrap();
        assert_eq!(bins.len(), DISTANCE_BINS);
        // 1.2 bp -> bin 2 ([1.0, 1.5)) for 0.4 s; 2.7 bp ask -> bin 5 for 0.4 s.
        // The fresh part of the interval is measured against mid 100.01 -> 2.2 bp -> bin 4.
        assert!((bins[2]["exposure_secs"].as_f64().unwrap() - 0.4).abs() < 1e-9);
        assert!((bins[4]["exposure_secs"].as_f64().unwrap() - 2.2).abs() < 1e-9);
        assert!((bins[5]["exposure_secs"].as_f64().unwrap() - 0.4).abs() < 1e-9);
        assert_eq!(bins[2]["fill_count"].as_u64().unwrap(), 1);
        assert_eq!(rep.unpriced_fills, 1);
        assert!(bins[DISTANCE_BINS - 1]["hi_bps"].is_null());
        let h1 = &json["markout_histograms"][0]["counts"];
        assert_eq!(h1[90].as_u64().unwrap(), 1); // [-5.0, -4.5)
        assert_eq!(h1[109].as_u64().unwrap(), 1); // [+4.5, +5.0)
        assert_eq!(
            json["markout_histograms"][1]["underflow"].as_u64().unwrap(),
            1
        );
        assert_eq!(rep.sigma_samples, 1);
        assert_eq!(rep.sigma_gaps, 1);
        let expected = (100.01f64 / 100.0).ln().powi(2) / 0.4;
        assert!((rep.sigma_per_sqrt_sec.unwrap() - expected.sqrt()).abs() < 1e-12);
    }

    #[test]
    fn decision_phase_sigma_scopes_receipt_endpoints_and_preserves_other_telemetry() {
        let mut legacy = CalibrationAccum::new([1_000, 5_000, 30_000], 5_000);
        let mut scoped = CalibrationAccum::new([1_000, 5_000, 30_000], 5_000)
            .with_decision_sigma_interval(1_000, 1_500);
        for (receipt, source, mid) in [
            (900, 850, 95.0),
            (1_000, 990, 110.0),
            (1_250, 1_210, 111.0),
            (1_499, 1_490, 113.0),
            (1_500, 1_491, 130.0),
            (1_600, 1_590, 120.0),
        ] {
            let book = Some((mid, source + 500));
            let live = [(1_000, 95.0, false)];
            legacy.on_book(receipt, book, live);
            scoped.on_book(receipt, book, live);
        }
        assert_eq!(scoped.observable_mid(1_000), Some(95.0));
        assert_eq!(scoped.observable_mid(1_601), Some(120.0));

        let legacy_json = serde_json::to_value(legacy.finish()).unwrap();
        assert!(legacy_json.get("decision_phase_sigma").is_none());
        let scoped_json = serde_json::to_value(scoped.finish()).unwrap();
        assert_eq!(scoped_json["sigma_observed_secs"], 0.7);
        assert_eq!(scoped_json["sigma_samples"], 5);
        assert_eq!(
            scoped_json["decision_phase_sigma"]["scope"],
            "receipt_half_open"
        );
        assert_eq!(scoped_json["decision_phase_sigma"]["trade_from_ms"], 1_000);
        assert_eq!(scoped_json["decision_phase_sigma"]["end_ms"], 1_500);
        assert_eq!(
            scoped_json["decision_phase_sigma"]["sigma_observed_secs"],
            0.499
        );
        assert_eq!(scoped_json["decision_phase_sigma"]["sigma_samples"], 2);
        let expected_sq = (111.0_f64 / 110.0).ln().powi(2) + (113.0_f64 / 111.0).ln().powi(2);
        assert!(
            (scoped_json["decision_phase_sigma"]["sum_sq_log_return"]
                .as_f64()
                .unwrap()
                - expected_sq)
                .abs()
                < 1e-15
        );

        let mut equality = CalibrationAccum::new([1_000, 5_000, 30_000], 5_000)
            .with_decision_sigma_interval(1_000, 1_500);
        equality.on_book(1_001, Some((100.0, 1_020)), []);
        equality.on_book(1_020, Some((101.0, 1_100)), []);
        let eq_json = serde_json::to_value(equality.finish()).unwrap();
        assert_eq!(eq_json["decision_phase_sigma"]["sigma_samples"], 1);
        assert_eq!(eq_json["decision_phase_sigma"]["sigma_gaps"], 0);
        assert_eq!(
            eq_json["decision_phase_sigma"]["sigma_observed_secs"],
            0.019
        );

        let mut expired = CalibrationAccum::new([1_000, 5_000, 30_000], 5_000)
            .with_decision_sigma_interval(1_000, 1_500);
        expired.on_book(1_001, Some((100.0, 1_020)), []);
        expired.on_book(1_021, Some((101.0, 1_100)), []);
        let exp_json = serde_json::to_value(expired.finish()).unwrap();
        assert_eq!(exp_json["decision_phase_sigma"]["sigma_samples"], 0);
        assert_eq!(exp_json["decision_phase_sigma"]["sigma_gaps"], 1);
        assert!(exp_json["decision_phase_sigma"]["sigma_per_sqrt_sec"].is_null());

        let mut single_active = CalibrationAccum::new([1_000, 5_000, 30_000], 5_000)
            .with_decision_sigma_interval(1_000, 1_500);
        for at in [950, 999, 1_000, 1_500, 1_600] {
            single_active.on_book(at, Some((100.0, at + 500)), []);
        }
        let single_json = serde_json::to_value(single_active.finish()).unwrap();
        assert_eq!(single_json["decision_phase_sigma"]["sigma_samples"], 0);
        assert_eq!(single_json["decision_phase_sigma"]["sigma_gaps"], 0);
        assert!(single_json["decision_phase_sigma"]["sigma_per_sqrt_sec"].is_null());
    }
}
