//! Diagnostic clocks for Maker fills. These observations never admit/reject a
//! fill or alter queue, cash, position, cancellation or quote behavior.
//!
//! The old `cancel_pending` field describes the state at print receipt. A
//! delayed print may have traded before that request. Classify the assumed
//! input-source interval separately, with equal millisecond boundaries left
//! uncertain. This does not verify that `source_ms` is an execution timestamp
//! or that the configured guard bounds source/local clock error. Historical
//! Lighter tapes may use block-opening time, which cannot locate execution.
use super::{MARKOUT_HORIZONS_MS, PendingMarkout};
use serde::Serialize;

#[derive(Clone, Copy)]
pub(super) struct CancelWindow {
    pub requested_ms: u64,
    pub effective_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum SourceTiming {
    NoCancelRequestObserved,
    BeforeCancelRequest,
    RequestBoundaryOverlap,
    DuringCancelWindow,
    EffectiveBoundaryOverlap,
    BothBoundariesOverlap,
    AfterCancelEffective,
}

impl SourceTiming {
    const ALL: [Self; 7] = [
        Self::NoCancelRequestObserved,
        Self::BeforeCancelRequest,
        Self::RequestBoundaryOverlap,
        Self::DuringCancelWindow,
        Self::EffectiveBoundaryOverlap,
        Self::BothBoundariesOverlap,
        Self::AfterCancelEffective,
    ];

    fn index(self) -> usize {
        self as usize
    }

    fn classify(
        source_ms: u64,
        receipt_ms: u64,
        guard_ms: u64,
        cancel: Option<CancelWindow>,
    ) -> Self {
        let Some(cancel) = cancel else {
            return Self::NoCancelRequestObserved;
        };
        let lower = source_ms.saturating_sub(guard_ms);
        // A print cannot execute after its local receipt even when the venue
        // timestamp is allowed to lead the local clock by the configured guard.
        let upper = source_ms.saturating_add(guard_ms).min(receipt_ms);
        if upper < cancel.requested_ms {
            return Self::BeforeCancelRequest;
        }
        let request_overlap = lower <= cancel.requested_ms && upper >= cancel.requested_ms;
        let effective_overlap = lower <= cancel.effective_ms && upper >= cancel.effective_ms;
        match (request_overlap, effective_overlap) {
            (true, true) => Self::BothBoundariesOverlap,
            (true, false) => Self::RequestBoundaryOverlap,
            (false, true) => Self::EffectiveBoundaryOverlap,
            (false, false) if lower > cancel.effective_ms => Self::AfterCancelEffective,
            (false, false) => Self::DuringCancelWindow,
        }
    }
}

#[derive(Default, Serialize)]
struct TimeSamples {
    count: u64,
    sum_ms: i64,
    min_ms: Option<i64>,
    max_ms: Option<i64>,
}

impl TimeSamples {
    fn observe(&mut self, later: u64, earlier: u64) {
        // Validated replay epochs are below 2.3e12 ms. Signed differences keep
        // permitted source-clock leads visible instead of clipping them to 0.
        let value = later as i64 - earlier as i64;
        self.count += 1;
        self.sum_ms += value;
        self.min_ms = Some(self.min_ms.map_or(value, |prior| prior.min(value)));
        self.max_ms = Some(self.max_ms.map_or(value, |prior| prior.max(value)));
    }
}

#[derive(Default, Serialize)]
struct CohortMarkout {
    horizon_ms: u64,
    marked_fills: u64,
    marked_turnover: f64,
    markout_quote: f64,
    expired_fills: u64,
    pending_at_cutoff: u64,
}

#[derive(Serialize)]
struct Cohort {
    source_timing: SourceTiming,
    fills: u64,
    turnover: f64,
    fees: f64,
    markouts: [CohortMarkout; 3],
}

impl Cohort {
    fn new(source_timing: SourceTiming) -> Self {
        Self {
            source_timing,
            fills: 0,
            turnover: 0.0,
            fees: 0.0,
            markouts: MARKOUT_HORIZONS_MS.map(|horizon_ms| CohortMarkout {
                horizon_ms,
                ..Default::default()
            }),
        }
    }
}

#[derive(Serialize)]
pub(super) struct Diagnostics {
    clock_guard_ms: u64,
    source_clock: &'static str,
    source_clock_execution_verified: bool,
    guard_calibrated: bool,
    source_interval: &'static str,
    effect_time_anchor: &'static str,
    markout_anchor: &'static str,
    fill_receipt_lag_ms: TimeSamples,
    source_queue_age_ms: TimeSamples,
    receipt_queue_age_ms: TimeSamples,
    cancel_request_to_receipt_ms: TimeSamples,
    cancel_effective_to_receipt_ms: TimeSamples,
    late_receipt_fills: u64,
    late_receipt_turnover: f64,
    cohorts: [Cohort; 7],
}

impl Default for Diagnostics {
    fn default() -> Self {
        Self::new(0)
    }
}

impl Diagnostics {
    pub(super) fn new(clock_guard_ms: u64) -> Self {
        Self {
            clock_guard_ms,
            source_clock: "input_source_clock_under_configured_guard",
            source_clock_execution_verified: false,
            guard_calibrated: false,
            source_interval: "[source_ms - guard, min(source_ms + guard, receipt_ms)]",
            effect_time_anchor: "simulated_scheduled_deadline_or_observed_simulated_cancel",
            markout_anchor: "simulated_fill_receipt",
            fill_receipt_lag_ms: TimeSamples::default(),
            source_queue_age_ms: TimeSamples::default(),
            receipt_queue_age_ms: TimeSamples::default(),
            cancel_request_to_receipt_ms: TimeSamples::default(),
            cancel_effective_to_receipt_ms: TimeSamples::default(),
            late_receipt_fills: 0,
            late_receipt_turnover: 0.0,
            cohorts: SourceTiming::ALL.map(Cohort::new),
        }
    }

    pub(super) fn fill(
        &mut self,
        source_ms: u64,
        receipt_ms: u64,
        activated_ms: u64,
        cancel: Option<CancelWindow>,
        turnover: f64,
        fee_bps: f64,
    ) -> SourceTiming {
        let timing = SourceTiming::classify(source_ms, receipt_ms, self.clock_guard_ms, cancel);
        self.fill_receipt_lag_ms.observe(receipt_ms, source_ms);
        self.source_queue_age_ms.observe(source_ms, activated_ms);
        self.receipt_queue_age_ms.observe(receipt_ms, activated_ms);
        if let Some(cancel) = cancel {
            self.cancel_request_to_receipt_ms
                .observe(receipt_ms, cancel.requested_ms);
            self.cancel_effective_to_receipt_ms
                .observe(receipt_ms, cancel.effective_ms);
            if receipt_ms >= cancel.effective_ms {
                self.late_receipt_fills += 1;
                self.late_receipt_turnover += turnover;
            }
        }
        let cohort = &mut self.cohorts[timing.index()];
        cohort.fills += 1;
        cohort.turnover += turnover;
        cohort.fees += turnover * fee_bps / 10_000.0;
        timing
    }

    pub(super) fn mark(&mut self, horizon: usize, fill: &PendingMarkout, mid: f64) {
        let mark = &mut self.cohorts[fill.cancel_source_timing.index()].markouts[horizon];
        mark.marked_fills += 1;
        mark.marked_turnover += fill.signed_quantity.abs() * fill.price;
        mark.markout_quote += fill.signed_quantity * (mid - fill.price);
    }

    pub(super) fn expire(&mut self, horizon: usize, timing: SourceTiming) {
        self.cohorts[timing.index()].markouts[horizon].expired_fills += 1;
    }

    pub(super) fn pending_at_cutoff(&mut self, horizon: usize, timing: SourceTiming) {
        self.cohorts[timing.index()].markouts[horizon].pending_at_cutoff += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_intervals_partition_both_cancel_boundaries() {
        let cancel = Some(CancelWindow {
            requested_ms: 1_000,
            effective_ms: 1_550,
        });
        for (source, expected) in [
            (899, SourceTiming::BeforeCancelRequest),
            (900, SourceTiming::RequestBoundaryOverlap),
            (1_000, SourceTiming::RequestBoundaryOverlap),
            (1_100, SourceTiming::RequestBoundaryOverlap),
            (1_101, SourceTiming::DuringCancelWindow),
            (1_449, SourceTiming::DuringCancelWindow),
            (1_450, SourceTiming::EffectiveBoundaryOverlap),
            (1_550, SourceTiming::EffectiveBoundaryOverlap),
            (1_650, SourceTiming::EffectiveBoundaryOverlap),
            (1_651, SourceTiming::AfterCancelEffective),
        ] {
            assert_eq!(SourceTiming::classify(source, 2_000, 100, cancel), expected);
        }
        assert_eq!(
            SourceTiming::classify(1_100, 2_000, 600, cancel),
            SourceTiming::BothBoundariesOverlap
        );
        assert_eq!(
            SourceTiming::classify(1_100, 1_200, 100, None),
            SourceTiming::NoCancelRequestObserved
        );
    }

    #[test]
    fn exact_boundaries_and_zero_delay_remain_uncertain() {
        let cancel = Some(CancelWindow {
            requested_ms: 1_000,
            effective_ms: 1_000,
        });
        assert_eq!(
            SourceTiming::classify(1_000, 1_100, 0, cancel),
            SourceTiming::BothBoundariesOverlap
        );
        assert_eq!(
            SourceTiming::classify(999, 1_100, 0, cancel),
            SourceTiming::BeforeCancelRequest
        );
        assert_eq!(
            SourceTiming::classify(1_001, 1_100, 0, cancel),
            SourceTiming::AfterCancelEffective
        );
        // The source clock may lead receipt, but source uncertainty cannot
        // place an execution after the print was already received.
        assert_eq!(
            SourceTiming::classify(1_000, 999, 100, cancel),
            SourceTiming::BeforeCancelRequest
        );
    }

    #[test]
    fn delayed_receipt_does_not_turn_a_pre_request_trade_into_a_cancel_race() {
        let mut stats = Diagnostics::new(100);
        let timing = stats.fill(
            800,
            1_700,
            100,
            Some(CancelWindow {
                requested_ms: 1_000,
                effective_ms: 1_550,
            }),
            50.0,
            0.4,
        );
        assert_eq!(timing, SourceTiming::BeforeCancelRequest);
        assert_eq!(stats.late_receipt_fills, 1);
        assert_eq!(stats.fill_receipt_lag_ms.sum_ms, 900);
        assert_eq!(stats.source_queue_age_ms.sum_ms, 700);
        assert_eq!(stats.receipt_queue_age_ms.sum_ms, 1_600);
        assert_eq!(stats.cancel_request_to_receipt_ms.sum_ms, 700);
        assert_eq!(stats.cancel_effective_to_receipt_ms.sum_ms, 150);
        assert_eq!(stats.cohorts[timing.index()].fills, 1);
        assert_eq!(stats.cohorts[timing.index()].fees, 0.002);

        stats.fill(1_750, 1_700, 100, None, 20.0, 0.4);
        assert_eq!(stats.fill_receipt_lag_ms.min_ms, Some(-50));
        assert_eq!(stats.fill_receipt_lag_ms.count, 2);
        assert_eq!(stats.cancel_request_to_receipt_ms.count, 1);
        assert_eq!(stats.cohorts.iter().map(|c| c.fills).sum::<u64>(), 2);
    }
}
