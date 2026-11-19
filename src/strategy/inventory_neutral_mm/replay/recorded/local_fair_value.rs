//! Recorded-only local fair values. The risk ledger and displayed book keep their native mid.
use super::*;

const MAX_AGE_MS: u64 = 250;
const LOOKBACK_MS: u64 = 1_000;

#[derive(Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Policy {
    TouchMicro,
    /// Five L1 imbalance bins, three top-five imbalance bins, three one-second return bins.
    ShrunkImbalance {
        expected_bps: Box<[[[f64; 3]; 3]; 5]>,
    },
}

impl Policy {
    pub(super) fn validate(&self) -> Result<()> {
        if let Self::ShrunkImbalance { expected_bps } = self {
            ensure!(
                expected_bps
                    .iter()
                    .flatten()
                    .flatten()
                    .all(|x| x.is_finite() && x.abs() <= 2.0),
                "invalid local expected return"
            );
        }
        Ok(())
    }

    pub(super) fn label(&self) -> &'static str {
        match self {
            Self::TouchMicro => "local_touch_microprice",
            Self::ShrunkImbalance { .. } => "local_shrunk_imbalance",
        }
    }
}

#[derive(Clone, Copy)]
struct Observation {
    at: u64,
    source: u64,
    mid: f64,
    spread: f64,
    imbalance: f64,
    depth_imbalance: f64,
}

pub(in crate::strategy::inventory_neutral_mm) struct Signal {
    policy: Policy,
    history: VecDeque<Observation>,
}

impl Signal {
    pub(super) fn new(policy: Policy) -> Self {
        Self {
            policy,
            history: VecDeque::new(),
        }
    }

    pub(super) fn book(&mut self, at: u64, book: &Book) {
        let (Some(&[bid, bid_size]), Some(&[ask, ask_size])) =
            (book.bids.first(), book.asks.first())
        else {
            self.history.clear();
            return;
        };
        if book.timestamp_ms == 0
            || book.timestamp_ms > at
            || !bid.is_finite()
            || !ask.is_finite()
            || !bid_size.is_finite()
            || !ask_size.is_finite()
            || bid <= 0.0
            || ask <= bid
            || bid_size <= 0.0
            || ask_size <= 0.0
            || at - book.timestamp_ms > MAX_AGE_MS
            || self
                .history
                .back()
                .is_some_and(|old| book.timestamp_ms < old.source)
        {
            self.history.clear();
            return;
        }
        let depth = |levels: &[[f64; 2]]| -> Option<f64> {
            let mut sum = 0.0;
            for v in levels.iter().take(5) {
                if !v[1].is_finite() || v[1] < 0.0 {
                    return None;
                }
                sum += v[1];
            }
            sum.is_finite().then_some(sum)
        };
        let (Some(depth_bids), Some(depth_asks)) = (depth(&book.bids), depth(&book.asks)) else {
            self.history.clear();
            return;
        };
        if !(depth_bids + depth_asks).is_finite() || depth_bids + depth_asks <= 0.0 {
            self.history.clear();
            return;
        }
        // Keep one observation before the cutoff so an as-of lookup never interpolates prices.
        let oldest = at.saturating_sub(LOOKBACK_MS + MAX_AGE_MS);
        while self.history.get(1).is_some_and(|o| o.at <= oldest) {
            self.history.pop_front();
        }
        let share = |x: f64, y: f64| (x - y) / (x + y);
        let o = Observation {
            at,
            source: book.timestamp_ms,
            mid: 0.5 * bid + 0.5 * ask,
            spread: ask - bid,
            imbalance: share(bid_size, ask_size),
            depth_imbalance: share(depth_bids, depth_asks),
        };
        if !(o.mid.is_finite()
            && o.mid > 0.0
            && o.spread.is_finite()
            && o.spread > 0.0
            && o.imbalance.is_finite()
            && o.depth_imbalance.is_finite())
        {
            self.history.clear();
            return;
        }
        if self.history.back().is_some_and(|old| old.at == at) {
            self.history.pop_back();
        }
        self.history.push_back(o);
    }

    pub(in crate::strategy::inventory_neutral_mm) fn price(
        &self,
        now: u64,
    ) -> Option<(f64, &'static str, u64)> {
        let o = self.history.back()?;
        if o.at > now || now - o.source > MAX_AGE_MS {
            return None;
        }
        let (bps, oldest_source) = match &self.policy {
            Policy::TouchMicro => (5_000.0 * o.spread / o.mid * o.imbalance, o.source),
            Policy::ShrunkImbalance { expected_bps } => {
                let at = now.checked_sub(LOOKBACK_MS)?;
                let i = self
                    .history
                    .partition_point(|p| p.at <= at)
                    .checked_sub(1)?;
                let past = self.history.get(i)?;
                if at - past.source > MAX_AGE_MS {
                    return None;
                }
                let momentum = 10_000.0 * (o.mid - past.mid) / past.mid;
                let l1 = [-0.6, -0.2, 0.2, 0.6].partition_point(|&edge| edge <= o.imbalance);
                let l5 = [-0.35, 0.35].partition_point(|&edge| edge <= o.depth_imbalance);
                let movement = [-0.5, 0.5].partition_point(|&edge| edge <= momentum);
                (expected_bps[l1][l5][movement], o.source.min(past.source))
            }
        };
        let price = o.mid * (1.0 + bps.clamp(-2.0, 2.0) / 10_000.0);
        (price.is_finite() && price > 0.0).then_some((price, self.policy.label(), oldest_source))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn book(t: u64) -> Book {
        Book {
            timestamp_ms: t,
            bids: vec![[99.99, 9.0]],
            asks: vec![[100.01, 1.0]],
        }
    }

    #[test]
    fn fitted_price_needs_causal_fresh_lookback_and_resets_on_missing_depth() {
        let mut weights = [[[0.0; 3]; 3]; 5];
        weights[4][2][1] = 1.5;
        let mut s = Signal::new(Policy::ShrunkImbalance {
            expected_bps: Box::new(weights),
        });
        for t in [1000, 1250, 1500, 1750, 2000] {
            s.book(t, &book(t));
        }
        assert_eq!(s.price(1999), None);
        let (price, label, source) = s.price(2000).unwrap();
        assert!((price - 100.015).abs() < 1e-9);
        assert_eq!((label, source), ("local_shrunk_imbalance", 1000));
        assert_eq!(s.price(2251), None);
        s.book(2252, &Book::default());
        s.book(2253, &book(2253));
        assert_eq!(s.price(2253), None);
    }

    #[test]
    fn zero_or_nonfinite_sizes_and_source_regression_clear_history_without_nan_bin_bias() {
        let mut weights = [[[0.0; 3]; 3]; 5];
        weights[0][0][0] = 2.0; // Bin zero must never be hit by NaN imbalance!
        let mut s = Signal::new(Policy::ShrunkImbalance {
            expected_bps: Box::new(weights),
        });
        for t in [1000, 1250, 1500, 1750, 2000] {
            s.book(t, &book(t));
        }
        assert!(s.price(2000).is_some());

        // Zero sizes or NaN sizes must clear history rather than yielding NaN in partition_point.
        s.book(
            2050,
            &Book {
                timestamp_ms: 2050,
                bids: vec![[99.99, 0.0]],
                asks: vec![[100.01, 0.0]],
            },
        );
        assert_eq!(s.price(2050), None);

        for t in [3000, 3250, 3500, 3750, 4000] {
            s.book(t, &book(t));
        }
        assert!(s.price(4000).is_some());

        // Regressing source timestamp clears causal lookback history.
        s.book(4050, &book(3999));
        assert_eq!(s.price(4050), None);
    }
}
