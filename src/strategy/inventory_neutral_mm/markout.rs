//! Realized adverse selection from fill markouts.
//!
//! A maker fill's markout is the fair-value move `horizon` after the fill,
//! signed so positive means the fill earned edge (bought below / sold above
//! later fair value). A persistently negative EWMA indicates informed flow,
//! so quotes widen. The multiplier only ever widens (>= 1), never tightens.

use crate::order_tracker::{FillPrint, OrderSide, OrderTracker, RECENT_FILLS};
use std::time::{Duration, Instant};

/// Pending fills awaiting their horizon. Fixed so the hot path never allocates.
const PENDING: usize = 64;
/// Fills marked before the estimate influences quotes.
const MIN_SAMPLES: u32 = 8;
/// Upper bound on the spread multiplier from markouts.
pub(super) const MAX_MARKOUT_SPREAD_MULT: f64 = 1.5;

#[derive(Clone, Copy)]
struct Pending {
    print: FillPrint,
}

pub(super) struct MarkoutTracker {
    horizon: Duration,
    alpha: f64,
    threshold_bps: f64,
    pending: [Option<Pending>; PENDING],
    last_sequence: u64,
    /// Raw EWMA accumulator; divided by `weight` to remove the zero-start bias.
    ewma_bps: f64,
    weight: f64,
    samples: u32,
}

impl MarkoutTracker {
    /// `halflife_fills` sets the EWMA memory in fills; `threshold_bps` is the
    /// tolerated average markout (e.g. -1.0) before widening starts.
    pub(super) fn new(horizon_secs: f64, halflife_fills: f64, threshold_bps: f64) -> Self {
        Self {
            horizon: Duration::from_secs_f64(horizon_secs.max(0.1)),
            alpha: 1.0 - 0.5_f64.powf(1.0 / halflife_fills.max(1.0)),
            threshold_bps,
            pending: [None; PENDING],
            last_sequence: 0,
            ewma_bps: 0.0,
            weight: 0.0,
            samples: 0,
        }
    }

    /// Pull new fills from the tracker and mark those past the horizon.
    pub(super) fn update(&mut self, tracker: &OrderTracker, fair: f64, now: Instant) {
        let latest = tracker.total_fill_stats().0;
        // Fills older than the ring are gone; skip ahead instead of stalling.
        let first = self
            .last_sequence
            .saturating_add(1)
            .max(latest.saturating_sub(RECENT_FILLS as u64 - 1));
        for sequence in first..=latest {
            if let Some(print) = tracker.fill_at(sequence) {
                self.enqueue(print);
            }
        }
        self.last_sequence = self.last_sequence.max(latest);
        if !(fair.is_finite() && fair > 0.0) {
            return;
        }
        for index in 0..PENDING {
            let Some(entry) = self.pending[index] else {
                continue;
            };
            if now.saturating_duration_since(entry.print.at) < self.horizon {
                continue;
            }
            self.pending[index] = None;
            if let Some(bps) = markout_bps(entry.print, fair) {
                self.record(bps);
            }
        }
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
}
