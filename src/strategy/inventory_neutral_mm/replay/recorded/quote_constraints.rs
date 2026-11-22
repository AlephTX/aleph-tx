//! Read-only pricing-stage counters. A scope covers only the real quote evaluation,
//! never the cross-market attribution counterfactual or an execution/await boundary.
use super::{CancelReason, MARKOUT_HORIZONS_MS, PendingMarkout, cancel_timing, now_ms};
use serde::Serialize;
use std::cell::{Cell, RefCell};

thread_local! {
    static REPORT: RefCell<Option<Report>> = const { RefCell::new(None) };
    static ACTIVE: Cell<bool> = const { Cell::new(false) };
    static CANDIDATES: Cell<Option<[Candidate; 2]>> = const { Cell::new(None) };
    static DISPATCH: Cell<Option<[Candidate; 2]>> = const { Cell::new(None) };
}

#[derive(Default, Serialize)]
pub(super) struct Report {
    evaluated: u64,
    quoted: u64,
    unquoted: u64,
    base_spread: SpreadCounts,
    bid: SideCounts,
    ask: SideCounts,
    #[serde(skip_serializing_if = "Option::is_none")]
    fill_linkage: Option<FillLinkage>,
}

/// Mutually exclusive outcomes of the *base* fee/volatility clamp, before
/// momentum, inventory flattening, markout and directional reserves.
#[derive(Default, Serialize)]
struct SpreadCounts {
    floor_raised: u64,
    floor_equal: u64,
    interior: u64,
    cap_equal: u64,
    cap_lowered: u64,
    floor_equals_cap: u64,
}

#[derive(Default, Serialize)]
struct SideCounts {
    entry_or_flat: StageCounts,
    toward_flat: StageCounts,
}

#[derive(Default, Serialize)]
struct StageCounts {
    evaluated: u64,
    touch_or_stabilization_changed: u64,
    placement_tightened: u64,
    placement_equal: u64,
    placement_slack: u64,
    passive_exit_changed: u64,
    final_at_placement_bound: u64,
}

pub(in crate::strategy::inventory_neutral_mm) struct Sample {
    pub position: f64,
    pub tick: f64,
    pub base_raw_half_spread: f64,
    pub base_floor_half_spread: f64,
    pub base_cap_half_spread: f64,
    /// Actual reducing-side fee clamp, before momentum/age/reserves.
    pub reducing_spread: [Option<[f64; 3]>; 2],
    /// All price pairs are [bid, ask]. These are quote candidates, not orders.
    pub raw: [f64; 2],
    pub stable: [f64; 2],
    pub placement: [f64; 2],
    pub final_price: [f64; 2],
}

pub(super) fn reset(enabled: bool, link_fills: bool) {
    ACTIVE.set(false);
    CANDIDATES.set(None);
    DISPATCH.set(None);
    REPORT.replace(enabled.then(|| Report {
        fill_linkage: link_fills.then(FillLinkage::default),
        ..Default::default()
    }));
}

pub(super) fn take() -> Option<Report> {
    REPORT.take()
}

pub(in crate::strategy::inventory_neutral_mm) struct Evaluation;

impl Evaluation {
    pub(in crate::strategy::inventory_neutral_mm) fn open() -> Self {
        CANDIDATES.set(None);
        REPORT.with_borrow_mut(|report| {
            if let Some(report) = report {
                debug_assert!(!ACTIVE.get());
                report.evaluated += 1;
                ACTIVE.set(true);
            }
        });
        Self
    }
}

impl Drop for Evaluation {
    fn drop(&mut self) {
        if ACTIVE.replace(false) {
            REPORT.with_borrow_mut(|report| {
                if let Some(report) = report {
                    report.unquoted += 1;
                }
            });
        }
    }
}

pub(in crate::strategy::inventory_neutral_mm) fn record(sample: Sample) {
    if !ACTIVE.replace(false) {
        return;
    }
    REPORT.with_borrow_mut(|report| {
        let Some(report) = report else { return };
        report.quoted += 1;
        if report.fill_linkage.is_some() {
            CANDIDATES.set(Some([0, 1].map(|side| Candidate {
                decision_ms: now_ms(),
                tag: Tag::new(&sample, side),
                anchor: sample.final_price[side],
                placement: sample.placement[side],
                tick: sample.tick,
            })));
        }
        let spread = &mut report.base_spread;
        let raw = sample.base_raw_half_spread;
        let floor = sample.base_floor_half_spread;
        let cap = sample.base_cap_half_spread;
        if floor == cap {
            spread.floor_equals_cap += 1;
        } else if raw < floor {
            spread.floor_raised += 1;
        } else if raw == floor {
            spread.floor_equal += 1;
        } else if raw > cap {
            spread.cap_lowered += 1;
        } else if raw == cap {
            spread.cap_equal += 1;
        } else {
            spread.interior += 1;
        }
        for (side, counts) in [&mut report.bid, &mut report.ask].into_iter().enumerate() {
            let reducing = if side == 0 {
                sample.position < 0.0
            } else {
                sample.position > 0.0
            };
            let counts = if reducing {
                &mut counts.toward_flat
            } else {
                &mut counts.entry_or_flat
            };
            counts.evaluated += 1;
            // Tick-scale tolerance avoids treating floating-point roundoff as a
            // change. It is many orders below one executable tick.
            let eps = sample.tick * 1e-6;
            counts.touch_or_stabilization_changed +=
                u64::from((sample.raw[side] - sample.stable[side]).abs() > eps);
            let signed_gap =
                (sample.stable[side] - sample.placement[side]) * if side == 0 { 1.0 } else { -1.0 };
            if signed_gap > eps {
                counts.placement_tightened += 1;
            } else if signed_gap < -eps {
                counts.placement_slack += 1;
            } else {
                counts.placement_equal += 1;
            }
            let bounded = if side == 0 {
                sample.stable[side].min(sample.placement[side])
            } else {
                sample.stable[side].max(sample.placement[side])
            };
            counts.passive_exit_changed +=
                u64::from((sample.final_price[side] - bounded).abs() > eps);
            counts.final_at_placement_bound +=
                u64::from((sample.final_price[side] - sample.placement[side]).abs() <= eps);
        }
    });
}

// A candidate is frozen at the actual quote decision, copied at dispatch before
// transport sleeps, and retained on the accepted queue through partial/late fills.
// It never follows a later quote evaluation for an already resting order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum SpreadClass {
    FloorRaised,
    FloorEqual,
    Interior,
    CapEqual,
    CapLowered,
    FloorEqualsCap,
}
fn spread_class([raw, floor, cap]: [f64; 3]) -> SpreadClass {
    if floor == cap {
        SpreadClass::FloorEqualsCap
    } else if raw < floor {
        SpreadClass::FloorRaised
    } else if raw == floor {
        SpreadClass::FloorEqual
    } else if raw > cap {
        SpreadClass::CapLowered
    } else if raw == cap {
        SpreadClass::CapEqual
    } else {
        SpreadClass::Interior
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Placement {
    Tightened,
    Equal,
    Slack,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
struct Tag {
    is_ask: bool,
    toward_flat_at_decision: bool,
    base_spread: SpreadClass,
    reducing_spread: Option<SpreadClass>,
    placement: Placement,
    final_at_placement_bound: bool,
    touch_or_stabilization_changed: bool,
    passive_exit_changed: bool,
}
impl Tag {
    fn new(s: &Sample, side: usize) -> Self {
        let eps = s.tick * 1e-6;
        let gap = (s.stable[side] - s.placement[side]) * if side == 0 { 1.0 } else { -1.0 };
        let bounded = if side == 0 {
            s.stable[side].min(s.placement[side])
        } else {
            s.stable[side].max(s.placement[side])
        };
        Self {
            is_ask: side == 1,
            toward_flat_at_decision: if side == 0 {
                s.position < 0.0
            } else {
                s.position > 0.0
            },
            base_spread: spread_class([
                s.base_raw_half_spread,
                s.base_floor_half_spread,
                s.base_cap_half_spread,
            ]),
            reducing_spread: s.reducing_spread[side].map(spread_class),
            placement: if gap > eps {
                Placement::Tightened
            } else if gap < -eps {
                Placement::Slack
            } else {
                Placement::Equal
            },
            final_at_placement_bound: (s.final_price[side] - s.placement[side]).abs() <= eps,
            touch_or_stabilization_changed: (s.raw[side] - s.stable[side]).abs() > eps,
            passive_exit_changed: (s.final_price[side] - bounded).abs() > eps,
        }
    }
}
#[derive(Clone, Copy)]
struct Candidate {
    decision_ms: u64,
    tag: Tag,
    anchor: f64,
    placement: f64,
    tick: f64,
}
#[derive(Clone, Copy)]
pub(super) struct OrderTag {
    candidate: Candidate,
    submit_ms: u64,
    book_source_ms: u64,
    order_at_placement_bound: bool,
    order_matches_quote_anchor: bool,
}

impl OrderTag {
    pub(super) fn toward_flat(self) -> bool {
        self.candidate.tag.toward_flat_at_decision
    }
}

pub(in crate::strategy::inventory_neutral_mm) struct Submission(Option<[Candidate; 2]>);
impl Submission {
    pub(in crate::strategy::inventory_neutral_mm) fn open() -> Self {
        Self(DISPATCH.replace(CANDIDATES.take()))
    }
}
impl Drop for Submission {
    fn drop(&mut self) {
        DISPATCH.set(self.0.take());
    }
}

pub(super) fn submitted(is_ask: bool, book_source_ms: u64, price: f64) -> Option<OrderTag> {
    let tag = DISPATCH.get().map(|s| OrderTag {
        candidate: s[usize::from(is_ask)],
        submit_ms: now_ms(),
        book_source_ms,
        order_at_placement_bound: (price - s[usize::from(is_ask)].placement).abs()
            <= s[usize::from(is_ask)].tick * 1e-6,
        order_matches_quote_anchor: (price - s[usize::from(is_ask)].anchor).abs()
            <= s[usize::from(is_ask)].tick * 1e-6,
    });
    with_linkage(|l| {
        l.maker_submitted += 1;
        l.submitted_with_tag += u64::from(tag.is_some());
    });
    tag
}
pub(super) fn accepted(tag: Option<OrderTag>) {
    with_linkage(|l| {
        l.maker_accepted += 1;
        l.accepted_with_tag += u64::from(tag.is_some());
    });
}
#[derive(Default, Serialize)]
struct FillLinkage {
    maker_submitted: u64,
    submitted_with_tag: u64,
    maker_accepted: u64,
    accepted_with_tag: u64,
    cohorts: Vec<FillCohort>,
}
#[derive(PartialEq, Serialize)]
struct Key {
    quote: Option<Tag>,
    order_reduce_only: bool,
    order_at_placement_bound: Option<bool>,
    order_matches_quote_anchor: Option<bool>,
    cancel_pending_at_receipt: bool,
    cancel_reason: Option<&'static str>,
    source_timing: cancel_timing::SourceTiming,
}
#[derive(Default, Serialize)]
struct Mark {
    horizon_ms: u64,
    marked_fills: u64,
    marked_turnover: f64,
    markout_quote: f64,
    negative_markout_quote: f64,
    positive_markout_quote: f64,
    expired_fills: u64,
    pending_at_cutoff: u64,
}
#[derive(Serialize)]
struct FillCohort {
    key: Key,
    fills: u64,
    turnover: f64,
    fees: f64,
    total_decision_to_submit_ms: u64,
    total_submit_to_activation_ms: u64,
    total_decision_to_fill_receipt_ms: u64,
    total_source_to_receipt_ms: i64,
    total_submit_book_age_ms: i64,
    markouts: [Mark; 3],
}
fn with_linkage<T>(f: impl FnOnce(&mut FillLinkage) -> T) -> Option<T> {
    REPORT.with_borrow_mut(|r| r.as_mut()?.fill_linkage.as_mut().map(f))
}
#[allow(clippy::too_many_arguments)]
pub(super) fn fill(
    tag: Option<OrderTag>,
    reduce_only: bool,
    cancel_pending: bool,
    reason: Option<CancelReason>,
    timing: cancel_timing::SourceTiming,
    activated_ms: u64,
    source_ms: u64,
    turnover: f64,
    fee_bps: f64,
) -> Option<usize> {
    with_linkage(|l| {
        let key = Key {
            quote: tag.map(|t| t.candidate.tag),
            order_reduce_only: reduce_only,
            order_at_placement_bound: tag.map(|t| t.order_at_placement_bound),
            order_matches_quote_anchor: tag.map(|t| t.order_matches_quote_anchor),
            cancel_pending_at_receipt: cancel_pending,
            cancel_reason: reason.map(CancelReason::as_str),
            source_timing: timing,
        };
        let index = l
            .cohorts
            .iter()
            .position(|c| c.key == key)
            .unwrap_or_else(|| {
                l.cohorts.push(FillCohort {
                    key,
                    fills: 0,
                    turnover: 0.0,
                    fees: 0.0,
                    total_decision_to_submit_ms: 0,
                    total_submit_to_activation_ms: 0,
                    total_decision_to_fill_receipt_ms: 0,
                    total_source_to_receipt_ms: 0,
                    total_submit_book_age_ms: 0,
                    markouts: MARKOUT_HORIZONS_MS.map(|horizon_ms| Mark {
                        horizon_ms,
                        ..Default::default()
                    }),
                });
                l.cohorts.len() - 1
            });
        let c = &mut l.cohorts[index];
        c.fills += 1;
        c.turnover += turnover;
        c.fees += turnover * fee_bps / 10_000.0;
        let at = now_ms();
        c.total_source_to_receipt_ms += at as i64 - source_ms as i64;
        if let Some(t) = tag {
            c.total_decision_to_submit_ms += t.submit_ms.saturating_sub(t.candidate.decision_ms);
            c.total_submit_to_activation_ms += activated_ms.saturating_sub(t.submit_ms);
            c.total_decision_to_fill_receipt_ms += at.saturating_sub(t.candidate.decision_ms);
            c.total_submit_book_age_ms += t.submit_ms as i64 - t.book_source_ms as i64;
        }
        index
    })
}
pub(super) fn mark(fill: &PendingMarkout, horizon: usize, mid: Option<f64>, cutoff: bool) {
    let Some(index) = fill.quote_cohort else {
        return;
    };
    with_linkage(|l| {
        let m = &mut l.cohorts[index].markouts[horizon];
        if cutoff {
            m.pending_at_cutoff += 1;
        } else if let Some(mid) = mid {
            let value = fill.signed_quantity * (mid - fill.price);
            m.marked_fills += 1;
            m.marked_turnover += fill.signed_quantity.abs() * fill.price;
            m.markout_quote += value;
            m.negative_markout_quote += value.min(0.0);
            m.positive_markout_quote += value.max(0.0);
        } else {
            m.expired_fills += 1;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn accepted_tag_survives_requotes_partial_and_late_cancel_fills() {
        let _clock = super::super::ReplayClock::open(1_000, false);
        reset(true, true);
        {
            let _scope = Evaluation::open();
            record(sample());
        }
        let original = {
            let _dispatch = Submission::open();
            submitted(false, 990, 99.90).unwrap()
        };
        accepted(Some(original));
        // A later real quote and an attribution counterfactual cannot retag it.
        {
            let _scope = Evaluation::open();
            record(Sample {
                position: -1.0,
                ..sample()
            });
        }
        record(sample());
        tokio::time::advance(std::time::Duration::from_millis(100)).await;
        let first = fill(
            Some(original),
            false,
            false,
            None,
            cancel_timing::SourceTiming::NoCancelRequestObserved,
            1_050,
            1_090,
            10.0,
            1.0,
        )
        .unwrap();
        let late = fill(
            Some(original),
            false,
            true,
            Some(CancelReason::BoundBreach),
            cancel_timing::SourceTiming::BeforeCancelRequest,
            1_050,
            1_080,
            5.0,
            1.0,
        )
        .unwrap();
        assert_ne!(first, late);
        let f = PendingMarkout {
            due_ms: 2_100,
            signed_quantity: 0.1,
            price: 100.0,
            changed_side: false,
            cancel_pending: true,
            cancel_source_timing: cancel_timing::SourceTiming::BeforeCancelRequest,
            cancel_reason: Some(CancelReason::BoundBreach),
            quote_cohort: Some(late),
        };
        mark(&f, 0, Some(99.0), false);
        mark(&f, 1, None, false);
        mark(&f, 2, None, true);
        let link = take().unwrap().fill_linkage.unwrap();
        assert_eq!(link.cohorts.iter().map(|c| c.fills).sum::<u64>(), 2);
        for c in &link.cohorts {
            assert!(!c.key.quote.unwrap().toward_flat_at_decision);
        }
        let c = &link.cohorts[late];
        assert_eq!(c.key.cancel_reason, Some("bound_breach"));
        assert_eq!(c.key.order_at_placement_bound, Some(false));
        assert_eq!(c.key.order_matches_quote_anchor, Some(false));
        assert_eq!(
            c.key.source_timing,
            cancel_timing::SourceTiming::BeforeCancelRequest
        );
        assert_eq!(c.total_submit_to_activation_ms, 50);
        assert_eq!(c.total_source_to_receipt_ms, 20);
        assert_eq!(c.markouts[0].negative_markout_quote, -0.1);
        assert_eq!(c.markouts[1].expired_fills, 1);
        assert_eq!(c.markouts[2].pending_at_cutoff, 1);
        assert!(DISPATCH.get().is_none());
        reset(false, false);
        assert!(submitted(false, 0, 99.90).is_none());
    }

    fn sample() -> Sample {
        Sample {
            position: 1.0,
            tick: 0.01,
            base_raw_half_spread: 0.01,
            base_floor_half_spread: 0.08,
            base_cap_half_spread: 0.2,
            reducing_spread: [None; 2],
            raw: [99.98, 100.02],
            stable: [99.97, 100.03],
            placement: [99.92, 100.08],
            final_price: [99.92, 100.04],
        }
    }

    #[test]
    fn separates_floor_touch_placement_and_passive_exit_by_side() {
        reset(true, false);
        {
            let _scope = Evaluation::open();
            record(sample());
        }
        // This represents a later counterfactual pricing call: it must not count.
        record(sample());
        {
            let _scope = Evaluation::open();
        } // Failed pricing evaluation.
        let r = take().unwrap();
        assert_eq!((r.evaluated, r.quoted, r.unquoted), (2, 1, 1));
        assert_eq!(r.base_spread.floor_raised, 1);
        assert_eq!(r.bid.entry_or_flat.placement_tightened, 1);
        assert_eq!(r.bid.entry_or_flat.final_at_placement_bound, 1);
        assert_eq!(r.ask.toward_flat.placement_tightened, 1);
        assert_eq!(r.ask.toward_flat.passive_exit_changed, 1);
        assert_eq!(r.ask.toward_flat.final_at_placement_bound, 0);
        assert_eq!(r.bid.toward_flat.evaluated, 0);
    }

    #[test]
    fn spread_classes_partition_evaluations_and_reset_does_not_leak() {
        reset(true, false);
        for raw in [0.01, 0.08, 0.1, 0.2, 0.3] {
            let _scope = Evaluation::open();
            record(Sample {
                base_raw_half_spread: raw,
                ..sample()
            });
        }
        {
            let _scope = Evaluation::open();
            record(Sample {
                base_cap_half_spread: 0.08,
                ..sample()
            });
        }
        let r = take().unwrap();
        let s = r.base_spread;
        assert_eq!(
            [
                s.floor_raised,
                s.floor_equal,
                s.interior,
                s.cap_equal,
                s.cap_lowered,
                s.floor_equals_cap
            ],
            [1; 6]
        );
        assert_eq!(r.quoted, 6);
        reset(false, false);
        {
            let _scope = Evaluation::open();
            record(sample());
        }
        assert!(take().is_none());
    }
}
