//! Realized adverse selection from fill markouts.
//!
//! A maker fill's markout is the fair-value move `horizon` after the fill,
//! signed so positive means the fill earned edge (bought below / sold above
//! later fair value). A persistently negative EWMA indicates informed flow,
//! so quotes may widen. The multiplier is >= 1 and never tightens quotes.
//!
//! Timing: `FillPrint::at` is the local time the fill event was applied, not
//! the exchange's trade time. A fill is marked against the first fresh fair
//! value observed in `[horizon, horizon + tolerance]` after that; samples
//! first seen later are dropped and counted as expired, so a stall cannot pass
//! a 60 s move off as a 5 s markout. Each fill has equal weight regardless of
//! size: this measures how often fills are picked off, not notional loss.

use crate::order_tracker::{FillPrint, OrderSide, OrderTracker, RECENT_FILLS};
use std::time::{Duration, Instant};

/// Pending fills awaiting their horizon. Fixed so the hot path never allocates.
const PENDING: usize = 64;
/// Fills marked before the estimate influences quotes.
const MIN_SAMPLES: u32 = 8;
/// Upper bound on the spread multiplier from markouts.
pub(super) const MAX_MARKOUT_SPREAD_MULT: f64 = 1.5;
/// Lateness tolerated beyond the horizon, as a fraction of the horizon.
const LATE_TOLERANCE_FRACTION: f64 = 0.2;
/// Minimum absolute lateness tolerance, covering the poll cadence.
const MIN_LATE_TOLERANCE: Duration = Duration::from_millis(500);

/// Sample accounting for observability; monotonic over the session.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct MarkoutStats {
    /// Fills marked within the horizon tolerance.
    pub marked: u64,
    /// Fills first observable after the tolerance; dropped.
    pub expired: u64,
    /// Fills evicted from a full pending queue before marking.
    pub overwritten: u64,
    /// Fills that left the tracker's ring before being read.
    pub missed: u64,
    /// Lateness of the most recent marked sample beyond the horizon (ms).
    pub last_delay_ms: f64,
    /// Bias-corrected EWMA markout in bps, once warm.
    pub ewma_bps: Option<f64>,
}

#[derive(Clone, Copy)]
struct Pending {
    print: FillPrint,
}

pub(super) struct MarkoutTracker {
    horizon: Duration,
    tolerance: Duration,
    alpha: f64,
    threshold_bps: f64,
    pending: [Option<Pending>; PENDING],
    last_sequence: u64,
    /// Raw EWMA accumulator; divided by `weight` to remove the zero-start bias.
    ewma_bps: f64,
    weight: f64,
    samples: u32,
    stats: MarkoutStats,
}

impl MarkoutTracker {
    /// `halflife_fills` sets the EWMA memory in fills; `threshold_bps` is the
    /// tolerated average markout (e.g. -1.0) before widening starts.
    pub(super) fn new(horizon_secs: f64, halflife_fills: f64, threshold_bps: f64) -> Self {
        let horizon = Duration::from_secs_f64(horizon_secs.max(0.1));
        Self {
            horizon,
            tolerance: horizon
                .mul_f64(LATE_TOLERANCE_FRACTION)
                .max(MIN_LATE_TOLERANCE),
            alpha: 1.0 - 0.5_f64.powf(1.0 / halflife_fills.max(1.0)),
            threshold_bps,
            pending: [None; PENDING],
            last_sequence: 0,
            ewma_bps: 0.0,
            weight: 0.0,
            samples: 0,
            stats: MarkoutStats::default(),
        }
    }

    /// Pull new fills from the tracker and mark those whose horizon has come.
    /// `fair` must be a fresh fair value observed at `now`; pass NaN when none
    /// is available so due fills can still expire instead of waiting.
    pub(super) fn update(&mut self, tracker: &OrderTracker, fair: f64, now: Instant) {
        let latest = tracker.total_fill_stats().0;
        // Fills older than the ring are gone; skip ahead instead of stalling.
        let oldest_retained = latest.saturating_sub(RECENT_FILLS as u64 - 1);
        let next = self.last_sequence.saturating_add(1);
        if next < oldest_retained {
            self.stats.missed += oldest_retained - next;
        }
        for sequence in next.max(oldest_retained)..=latest {
            match tracker.fill_at(sequence) {
                Some(print) => self.enqueue(print),
                None => self.stats.missed += 1,
            }
        }
        self.last_sequence = self.last_sequence.max(latest);
        let fair_valid = fair.is_finite() && fair > 0.0;
        for index in 0..PENDING {
            let Some(entry) = self.pending[index] else {
                continue;
            };
            let age = now.saturating_duration_since(entry.print.at);
            if age < self.horizon {
                continue;
            }
            let late = age - self.horizon;
            if late > self.tolerance {
                self.pending[index] = None;
                self.stats.expired += 1;
                continue;
            }
            if !fair_valid {
                continue; // wait within tolerance for a fresh fair value
            }
            self.pending[index] = None;
            match markout_bps(entry.print, fair) {
                Some(bps) => {
                    self.record(bps);
                    self.stats.marked += 1;
                    self.stats.last_delay_ms = late.as_secs_f64() * 1000.0;
                }
                None => self.stats.expired += 1,
            }
        }
        self.stats.ewma_bps = self.ewma_bps();
    }

    pub(crate) fn stats(&self) -> MarkoutStats {
        self.stats
    }

    fn enqueue(&mut self, print: FillPrint) {
        if !(print.price.is_finite() && print.price > 0.0) {
            return;
        }
        // Prefer a free slot; under a fill burst replace the oldest pending fill.
        let mut target = 0;
        let mut oldest = u64::MAX;
        for (index, slot) in self.pending.iter().enumerate() {
            match slot {
                None => {
                    target = index;
                    break;
                }
                Some(entry) if entry.print.sequence < oldest => {
                    oldest = entry.print.sequence;
                    target = index;
                }
                Some(_) => {}
            }
        }
        if self.pending[target].is_some() {
            self.stats.overwritten += 1;
        }
        self.pending[target] = Some(Pending { print });
    }

    fn record(&mut self, bps: f64) {
        self.ewma_bps += self.alpha * (bps - self.ewma_bps);
        self.weight += self.alpha * (1.0 - self.weight);
        self.samples = self.samples.saturating_add(1);
    }

    /// Bias-corrected average markout once enough fills have been marked.
    pub(super) fn ewma_bps(&self) -> Option<f64> {
        (self.samples >= MIN_SAMPLES && self.weight > 0.0).then(|| self.ewma_bps / self.weight)
    }

    /// Spread multiplier in [1, MAX_MARKOUT_SPREAD_MULT]: +10% per bp of
    /// average markout below the threshold.
    pub(super) fn spread_multiplier(&self) -> f64 {
        match self.ewma_bps() {
            Some(bps) if bps < self.threshold_bps => {
                (1.0 + 0.1 * (self.threshold_bps - bps)).min(MAX_MARKOUT_SPREAD_MULT)
            }
            _ => 1.0,
        }
    }
}

/// Signed markout in bps; positive means the fill earned edge.
fn markout_bps(print: FillPrint, fair: f64) -> Option<f64> {
    let raw = (fair / print.price - 1.0) * 10_000.0;
    let signed = match print.side {
        OrderSide::Buy => raw,
        OrderSide::Sell => -raw,
    };
    signed.is_finite().then_some(signed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn print(sequence: u64, side: OrderSide, price: f64, at: Instant) -> FillPrint {
        FillPrint {
            sequence,
            side,
            price,
            size: 0.01,
            at,
        }
    }

    #[test]
    fn markout_sign_rewards_edge_and_penalizes_adverse_moves() {
        let now = Instant::now();
        // Bought at 100, fair later 101: +100 bps edge.
        assert!(
            (markout_bps(print(1, OrderSide::Buy, 100.0, now), 101.0).unwrap() - 100.0).abs()
                < 1e-9
        );
        // Sold at 100, fair later 101: -100 bps (picked off).
        assert!(
            (markout_bps(print(1, OrderSide::Sell, 100.0, now), 101.0).unwrap() + 100.0).abs()
                < 1e-9
        );
    }

    #[test]
    fn only_widens_after_enough_adverse_samples() {
        let start = Instant::now();
        let mut tracker = MarkoutTracker::new(5.0, 20.0, -1.0);
        for sequence in 1..=u64::from(MIN_SAMPLES) {
            tracker.enqueue(print(sequence, OrderSide::Sell, 2000.0, start));
        }
        let fair = 2000.0 * 1.0003; // price rose 3 bps after every sell
        let tracker_state = OrderTracker::new(); // no new fills to pull
        // Before the horizon nothing is marked.
        tracker.update(&tracker_state, fair, start + Duration::from_secs(1));
        assert_eq!(tracker.ewma_bps(), None);
        assert_eq!(tracker.spread_multiplier(), 1.0);
        // After the horizon every pending sell is marked adverse.
        tracker.update(&tracker_state, fair, start + Duration::from_secs(6));
        assert!(tracker.ewma_bps().unwrap() < -1.0);
        let mult = tracker.spread_multiplier();
        assert!(mult > 1.0 && mult <= MAX_MARKOUT_SPREAD_MULT);
    }

    #[test]
    fn favourable_markouts_never_tighten() {
        let mut favourable = MarkoutTracker::new(5.0, 20.0, -1.0);
        for _ in 0..MIN_SAMPLES {
            favourable.record(5.0);
        }
        assert!((favourable.ewma_bps().unwrap() - 5.0).abs() < 1e-9);
        assert_eq!(favourable.spread_multiplier(), 1.0);

        let mut toxic = MarkoutTracker::new(5.0, 20.0, -1.0);
        for _ in 0..MIN_SAMPLES {
            toxic.record(-100.0);
        }
        assert_eq!(toxic.spread_multiplier(), MAX_MARKOUT_SPREAD_MULT);
    }

    #[test]
    fn full_pending_queue_replaces_the_oldest_fill() {
        let now = Instant::now();
        let mut tracker = MarkoutTracker::new(5.0, 20.0, -1.0);
        for sequence in 1..=(PENDING as u64 + 3) {
            tracker.enqueue(print(sequence, OrderSide::Buy, 100.0, now));
        }
        let mut kept: Vec<u64> = tracker
            .pending
            .iter()
            .flatten()
            .map(|p| p.print.sequence)
            .collect();
        kept.sort_unstable();
        assert_eq!(kept.len(), PENDING);
        assert_eq!(kept[0], 4);
        assert_eq!(*kept.last().unwrap(), PENDING as u64 + 3);
    }

    /// Real tracker path: OrderCreated + OrderFilled through apply_event.
    fn apply_fill(
        tracker: &OrderTracker,
        coid: i64,
        side: OrderSide,
        price: f64,
        size: f64,
        remaining: f64,
        trade_id: u64,
        sequence: u64,
    ) {
        use crate::types::ShmPrivateEventV2;
        let exchange_id = 50_000 + coid as u64;
        let is_ask = side == OrderSide::Sell;
        if tracker.get_order_index(coid).is_none() {
            tracker.start_tracking(coid, side, price, size + remaining);
            let created = ShmPrivateEventV2::order_created(
                sequence,
                2,
                0,
                exchange_id,
                coid,
                coid,
                price,
                size + remaining,
                is_ask,
                0,
            );
            tracker.apply_event(&created).unwrap();
        }
        let fill = ShmPrivateEventV2::order_filled(
            sequence + 1,
            2,
            0,
            exchange_id,
            coid,
            coid,
            price,
            size,
            remaining,
            0.0,
            is_ask,
            0,
            trade_id,
        );
        tracker.apply_event(&fill).unwrap();
    }

    #[test]
    fn stalled_fair_value_expires_instead_of_counting_as_horizon_markout() {
        let tracker = OrderTracker::new();
        let mut markout = MarkoutTracker::new(5.0, 20.0, -1.0);
        let start = Instant::now();
        apply_fill(&tracker, 1, OrderSide::Sell, 2000.0, 0.01, 0.0, 1, 1);
        // No fresh fair value for 60 s (NaN), then a large move arrives.
        markout.update(&tracker, f64::NAN, start);
        markout.update(&tracker, f64::NAN, start + Duration::from_secs(5));
        markout.update(&tracker, 2100.0, start + Duration::from_secs(60));
        let stats = markout.stats();
        assert_eq!((stats.marked, stats.expired), (0, 1));
        assert_eq!(markout.ewma_bps(), None);
    }

    #[test]
    fn due_fill_waits_within_tolerance_for_fresh_fair_value() {
        let tracker = OrderTracker::new();
        let mut markout = MarkoutTracker::new(5.0, 20.0, -1.0);
        apply_fill(&tracker, 1, OrderSide::Buy, 2000.0, 0.01, 0.0, 1, 1);
        // Fill time is the local apply time recorded by the tracker.
        let at = tracker.fill_at(1).unwrap().at;
        markout.update(&tracker, f64::NAN, at + Duration::from_millis(5_200));
        assert_eq!(markout.stats().marked, 0);
        // 0.8 s late is within the max(20% of 5 s, 0.5 s) = 1 s tolerance.
        markout.update(&tracker, 2001.0, at + Duration::from_millis(5_800));
        let stats = markout.stats();
        assert_eq!((stats.marked, stats.expired), (1, 0));
        assert!((stats.last_delay_ms - 800.0).abs() < 1.0);
    }

    #[test]
    fn tracker_ring_overrun_and_full_pending_queue_are_counted() {
        let tracker = OrderTracker::new();
        let mut markout = MarkoutTracker::new(5.0, 20.0, -1.0);
        // More fills than the tracker ring retains before the first update.
        let total = RECENT_FILLS as u64 + 10;
        for coid in 1..=total {
            apply_fill(
                &tracker,
                coid as i64,
                OrderSide::Buy,
                2000.0,
                0.01,
                0.0,
                coid,
                coid * 2,
            );
        }
        markout.update(&tracker, 2000.0, Instant::now());
        let stats = markout.stats();
        assert_eq!(stats.missed, 10);
        // RECENT_FILLS were read, PENDING kept, the rest overwritten.
        assert_eq!(stats.overwritten, RECENT_FILLS as u64 - PENDING as u64);
        assert_eq!(markout.pending.iter().flatten().count(), PENDING);
    }

    #[test]
    fn duplicate_and_partial_fills_are_counted_once_each() {
        let tracker = OrderTracker::new();
        let mut markout = MarkoutTracker::new(5.0, 20.0, -1.0);
        // Two partial fills of one order, then a retransmitted duplicate.
        apply_fill(&tracker, 7, OrderSide::Sell, 2000.0, 0.01, 0.01, 701, 1);
        apply_fill(&tracker, 7, OrderSide::Sell, 2000.0, 0.01, 0.0, 702, 3);
        apply_fill(&tracker, 7, OrderSide::Sell, 2000.0, 0.01, 0.0, 702, 5);
        assert_eq!(tracker.total_fill_stats().0, 2);
        let at = tracker.fill_at(2).unwrap().at;
        markout.update(&tracker, f64::NAN, at);
        markout.update(&tracker, 2000.0, at + Duration::from_secs(5));
        assert_eq!(markout.stats().marked, 2);
    }

    #[test]
    fn recovery_after_outage_marks_only_fills_still_within_tolerance() {
        // During a feed outage the strategy loop does not call update; on
        // recovery the first fresh fair value marks what is still in window.
        let tracker = OrderTracker::new();
        let mut markout = MarkoutTracker::new(5.0, 20.0, -1.0);
        let t0 = Instant::now();
        markout.enqueue(print(1, OrderSide::Buy, 2000.0, t0));
        markout.enqueue(print(
            2,
            OrderSide::Buy,
            2000.0,
            t0 + Duration::from_secs(25),
        ));
        markout.update(&tracker, 2002.0, t0 + Duration::from_millis(30_500));
        let stats = markout.stats();
        // Fill 1 is 25.5 s late and dropped; fill 2 is 0.5 s late and marked.
        assert_eq!((stats.marked, stats.expired), (1, 1));
        assert!((stats.last_delay_ms - 500.0).abs() < 1.0);
    }
}
