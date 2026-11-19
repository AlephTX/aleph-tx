//! Reconcile displayed depth and public prints in source time before crediting cancellations.
use std::collections::VecDeque;
use std::ops::Sub;

/// Depth is market-by-price. A book decrease is ambiguous until the last admissible
/// receipt of a trade from that interval. Overlapping timestamp uncertainty is
/// charged to both intervals, conservatively reducing inferred cancellations.
pub(super) struct ReplayQueueDepth<Q> {
    resume_after: u64,
    anchor: Option<(u64, Q)>,
    books: VecDeque<(u64, Q)>,
    prints: Vec<(u64, Q)>,
}

impl<Q: Copy + Default + PartialOrd + Sub<Output = Q>> ReplayQueueDepth<Q> {
    pub(super) fn new(now: u64) -> Self {
        Self {
            resume_after: now,
            anchor: None,
            books: VecDeque::new(),
            prints: Vec::new(),
        }
    }

    pub(super) fn reset(&mut self, now: u64) {
        *self = Self::new(self.resume_after.max(now));
    }

    pub(super) fn trade(&mut self, source: u64, qty: Q, now: u64, guard: u64, max_age: u64) {
        if let Some((last, _)) = self.books.back().copied().or(self.anchor)
            && now.saturating_sub(last) > max_age
        {
            self.reset(now);
        }
        // A later visibility/feed reset drops stored prints. The next baseline
        // must still come after their entire source-time uncertainty interval.
        self.resume_after = self.resume_after.max(source.saturating_add(guard));
        if self
            .anchor
            .is_some_and(|(at, _)| source.saturating_add(guard) >= at)
        {
            self.prints.push((source, qty));
        }
    }

    pub(super) fn book(
        &mut self,
        source: u64,
        depth: Q,
        now: u64,
        timing: (u64, u64, u64),
        mut cancellation: impl FnMut(Q, Q),
    ) {
        let (guard, grace, max_age) = timing;
        if source > now || now - source > max_age {
            self.reset(now);
            return;
        }
        let last = self.books.back().copied().or(self.anchor);
        if let Some((ts, prior)) = last
            && (source < ts
                || source == ts && depth != prior
                || source.saturating_sub(ts) > max_age)
        {
            self.reset(now);
            return;
        }
        if self.anchor.is_none() {
            if source > self.resume_after {
                self.anchor = Some((source, depth));
            }
            return;
        }
        if last.is_some_and(|(ts, _)| ts < source) {
            self.books.push_back((source, depth));
        }
        while let Some(&(end, after)) = self.books.front() {
            let through = end.saturating_add(guard);
            if now <= through.saturating_add(grace) {
                break;
            }
            self.books.pop_front();
            let (start, before) = self.anchor.replace((end, after)).expect("anchored depth");
            let from = start.saturating_sub(guard);
            let residual = self
                .prints
                .iter()
                .filter(|(stamp, _)| *stamp >= from && *stamp <= through)
                .fold(before, |remaining, &(_, qty)| {
                    if remaining > qty {
                        remaining - qty
                    } else {
                        Q::default()
                    }
                });
            if residual > after {
                cancellation(residual, after);
            }
            self.prints
                .retain(|&(stamp, _)| stamp >= end.saturating_sub(guard));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn late_touch_and_visibility_gap_bound_inferred_cancellations() {
        let mut history = ReplayQueueDepth::new(0);
        let mut drops = Vec::new();
        let timing = (10, 30, 200);
        history.book(50, 100, 50, timing, |a, b| drops.push(a - b));
        history.book(100, 60, 100, timing, |a, b| drops.push(a - b));
        history.trade(105, 30, 130, 10, 200);
        history.book(100, 60, 140, timing, |a, b| drops.push(a - b));
        assert!(drops.is_empty());
        history.book(141, 60, 141, timing, |a, b| drops.push(a - b));
        assert_eq!(drops, [10]);
        history.trade(145, 40, 145, 10, 200);
        history.reset(146);
        // Depth at 150 could still predate the already seen trade.
        history.book(150, 100, 150, timing, |a, b| drops.push(a - b));
        history.book(158, 60, 158, timing, |a, b| drops.push(a - b));
        history.book(200, 60, 200, timing, |a, b| drops.push(a - b));
        assert_eq!(drops, [10]);
        history.book(230, 40, 230, timing, |a, b| drops.push(a - b));
        history.book(271, 40, 271, timing, |a, b| drops.push(a - b));
        assert_eq!(drops, [10, 20]);
    }
}
