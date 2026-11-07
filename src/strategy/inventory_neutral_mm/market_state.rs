use crate::shm_reader::{NUM_EXCHANGES, ShmBboMessage};

#[derive(Debug, Clone, Copy)]
pub(super) struct MarketState {
    pub exchanges: [(u8, ShmBboMessage); NUM_EXCHANGES],
    pub bbo: ShmBboMessage,
}

pub(super) fn select_exchange_bbo(
    exchanges: &[(u8, ShmBboMessage); NUM_EXCHANGES],
    exchange_id: u8,
) -> Option<ShmBboMessage> {
    exchanges
        .iter()
        .find(|(exch_id, _)| *exch_id == exchange_id)
        .map(|(_, msg)| *msg)
        .filter(valid_quoteable_book)
}

pub(super) fn data_age_ms(timestamp_ns: u64, now_ns: u64) -> u64 {
    now_ns.saturating_sub(timestamp_ns) / 1_000_000
}

pub(super) fn is_stale_bbo(timestamp_ns: u64, now_ns: u64, threshold_ms: u64) -> bool {
    timestamp_ns == 0 || timestamp_ns > now_ns || data_age_ms(timestamp_ns, now_ns) > threshold_ms
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StaleBboAction {
    Fresh,
    Freeze,
    Cancel,
}

fn valid_quoteable_book(bbo: &ShmBboMessage) -> bool {
    bbo.bid_price.is_finite()
        && bbo.ask_price.is_finite()
        && bbo.bid_size.is_finite()
        && bbo.ask_size.is_finite()
        && bbo.bid_price >= 0.0
        && bbo.ask_price >= 0.0
        && bbo.bid_size >= 0.0
        && bbo.ask_size >= 0.0
        && (bbo.bid_price > 0.0 || bbo.ask_price > 0.0)
        && (bbo.bid_price == 0.0 || bbo.ask_price == 0.0 || bbo.bid_price < bbo.ask_price)
}

/// Slow EWMA of the venue basis `local_mid / external_mid - 1`.
///
/// Lighter persistently trades at a premium or discount to the Binance/HL
/// median (funding and index differences). Quoting around the raw external
/// median therefore skews fills to one side. The basis is clamped and, until
/// enough time has been observed, shrunk toward zero.
#[derive(Debug, Clone, Copy)]
pub(super) struct BasisTracker {
    tau_secs: f64,
    max_basis: f64,
    basis: f64,
    weight: f64,
    last_ns: u64,
}

impl BasisTracker {
    /// Gaps longer than this do not count as observed time.
    const MAX_DT_SECS: f64 = 10.0;

    pub(super) fn new(tau_secs: f64, max_basis_bps: f64) -> Self {
        Self {
            tau_secs: tau_secs.max(1.0),
            max_basis: (max_basis_bps / 10_000.0).max(0.0),
            basis: 0.0,
            weight: 0.0,
            last_ns: 0,
        }
    }

    /// Record a fresh two-sided local mid against the external median.
    pub(super) fn observe(&mut self, local_mid: f64, external_mid: f64, now_ns: u64) {
        if !(local_mid.is_finite() && external_mid.is_finite())
            || local_mid <= 0.0
            || external_mid <= 0.0
        {
            return;
        }
        let sample = (local_mid / external_mid - 1.0).clamp(-self.max_basis, self.max_basis);
        if self.last_ns == 0 || now_ns <= self.last_ns {
            self.last_ns = self.last_ns.max(now_ns);
            if self.weight == 0.0 {
                self.basis = sample;
            }
            return;
        }
        let dt = ((now_ns - self.last_ns) as f64 / 1e9).min(Self::MAX_DT_SECS);
        self.last_ns = now_ns;
        let alpha = 1.0 - (-dt / self.tau_secs).exp();
        self.basis += alpha * (sample - self.basis);
        self.weight += alpha * (1.0 - self.weight);
    }

    /// Basis to apply, shrunk toward zero while the estimate is young.
    pub(super) fn applied_basis(&self) -> f64 {
        (self.basis * self.weight).clamp(-self.max_basis, self.max_basis)
    }

    /// External median shifted onto the local venue's price level.
    pub(super) fn correct(&self, external_mid: f64) -> f64 {
        external_mid * (1.0 + self.applied_basis())
    }
}

pub(super) fn classify_stale_bbo(
    bbo: &ShmBboMessage,
    now_ns: u64,
    freeze_threshold_ms: u64,
    cancel_after_ms: u64,
) -> StaleBboAction {
    // A static book is not proof of a live feed. Age the source timestamp,
    // rather than starting another timeout only after a stale read is observed.
    if bbo.timestamp_ns == 0 || bbo.timestamp_ns > now_ns || !valid_quoteable_book(bbo) {
        return StaleBboAction::Cancel;
    }
    let age_ms = data_age_ms(bbo.timestamp_ns, now_ns);
    if age_ms >= cancel_after_ms {
        StaleBboAction::Cancel
    } else if age_ms > freeze_threshold_ms {
        StaleBboAction::Freeze
    } else {
        StaleBboAction::Fresh
    }
}

pub(super) fn build_market_state(
    exchanges: [(u8, ShmBboMessage); NUM_EXCHANGES],
    exchange_id: u8,
) -> Option<MarketState> {
    let bbo = select_exchange_bbo(&exchanges, exchange_id)?;
    Some(MarketState { exchanges, bbo })
}

/// Primary fair value anchor from external high-liquidity exchanges.
///
/// Takes ALL non-local exchange BBOs (exchange_id != local AND != 0),
/// filters stale data (> staleness_ms), calculates mid for each valid
/// exchange, and returns the median (robust against single-exchange outlier).
/// Returns None if no valid external data is available.
///
/// v6.0.1: Added DEBUG logging to track staleness and fallback behavior.
pub(super) fn external_fair_value_mid(
    exchanges: &[(u8, ShmBboMessage); NUM_EXCHANGES],
    local_exchange_id: u8,
    now_ns: u64,
    staleness_ms: u64,
) -> Option<f64> {
    // Exchange count is fixed by the SHM ABI: keep the fair-value hot path
    // on the stack, including median sorting.
    let mut valid_mids = [0.0_f64; NUM_EXCHANGES];
    let mut count = 0;
    for (exchange_id, bbo) in exchanges {
        if *exchange_id == local_exchange_id
            || *exchange_id == 0
            || !valid_quoteable_book(bbo)
            || bbo.bid_price == 0.0
            || bbo.ask_price == 0.0
            || is_stale_bbo(bbo.timestamp_ns, now_ns, staleness_ms)
        {
            continue;
        }
        // Halve before adding to avoid overflow on otherwise finite prices.
        valid_mids[count] = bbo.bid_price * 0.5 + bbo.ask_price * 0.5;
        count += 1;
    }
    if count == 0 {
        return None;
    }
    valid_mids[..count].sort_unstable_by(f64::total_cmp);
    let middle = count / 2;
    Some(if count % 2 == 1 {
        valid_mids[middle]
    } else {
        valid_mids[middle - 1] * 0.5 + valid_mids[middle] * 0.5
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basis_tracker_converges_to_persistent_premium() {
        let mut basis = BasisTracker::new(60.0, 20.0);
        let external = 2000.0;
        let local = external * (1.0 + 5e-4); // 5 bps premium
        for i in 0..1_200u64 {
            basis.observe(local, external, 1 + i * 500_000_000); // 2 Hz for 10 min
        }
        assert!((basis.applied_basis() - 5e-4).abs() < 1e-5);
        assert!((basis.correct(external) - local).abs() < 0.02);
    }

    #[test]
    fn basis_tracker_is_shrunk_while_young_and_clamped() {
        let mut basis = BasisTracker::new(300.0, 20.0);
        let external = 2000.0;
        basis.observe(external * 1.0005, external, 1);
        basis.observe(external * 1.0005, external, 1_000_000_001);
        // One second of history against a 300 s time constant applies ~0.3%.
        assert!(basis.applied_basis() > 0.0 && basis.applied_basis() < 1e-5);

        // A 1% dislocation is clamped to the 20 bps cap.
        for i in 2..10_000u64 {
            basis.observe(external * 1.01, external, i * 1_000_000_000);
        }
        assert!((basis.applied_basis() - 20e-4).abs() < 1e-9);
    }

    #[test]
    fn basis_tracker_disabled_and_invalid_inputs() {
        let mut off = BasisTracker::new(300.0, 0.0);
        for i in 1..100u64 {
            off.observe(2001.0, 2000.0, i * 1_000_000_000);
        }
        assert_eq!(off.correct(2000.0), 2000.0);

        let mut basis = BasisTracker::new(60.0, 20.0);
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            basis.observe(bad, 2000.0, 1_000_000_000);
            basis.observe(2000.0, bad, 2_000_000_000);
        }
        assert_eq!(basis.applied_basis(), 0.0);
        // Non-monotonic clock must not produce a negative dt.
        basis.observe(2001.0, 2000.0, 5_000_000_000);
        basis.observe(2001.0, 2000.0, 4_000_000_000);
        assert!(basis.applied_basis().is_finite());
    }

    fn msg(bid_price: f64, ask_price: f64, timestamp_ns: u64) -> ShmBboMessage {
        ShmBboMessage {
            seqlock: 0,
            msg_type: 0,
            exchange_id: 0,
            symbol_id: 0,
            timestamp_ns,
            bid_price,
            bid_size: 1.0,
            ask_price,
            ask_size: 1.0,
            _reserved: [0; 16],
        }
    }

    fn default_exchange() -> (u8, ShmBboMessage) {
        (0, ShmBboMessage::default())
    }

    #[test]
    fn build_market_state_selects_local_bbo_when_one_side_exists() {
        let exchanges = [
            default_exchange(),
            (1, msg(2100.0, 0.0, 1)),
            default_exchange(),
            default_exchange(),
            default_exchange(),
            default_exchange(),
            default_exchange(),
        ];

        let state = build_market_state(exchanges, 1).expect("local bbo should exist");

        assert_eq!(state.bbo.bid_price, 2100.0);
        assert_eq!(state.bbo.ask_price, 0.0);
    }

    #[test]
    fn is_stale_bbo_respects_threshold() {
        let now_ns = 10_000_000_000;
        assert!(!is_stale_bbo(9_996_000_000, now_ns, 5));
        assert!(is_stale_bbo(9_994_000_000, now_ns, 5));
    }

    #[test]
    fn stale_static_books_freeze_and_cancel_based_on_source_age() {
        let now = 40_000_000_000;
        for book in [
            msg(2100.0, 2101.0, 34_000_000_000),
            msg(2100.0, 0.0, 34_000_000_000),
        ] {
            assert_eq!(
                classify_stale_bbo(&book, now, 5_000, 15_000),
                StaleBboAction::Freeze
            );
            assert_eq!(
                classify_stale_bbo(&book, now + 10_000_000_000, 5_000, 15_000),
                StaleBboAction::Cancel
            );
        }
        assert_eq!(
            classify_stale_bbo(&msg(2100.0, 2101.0, now), now, 5_000, 15_000),
            StaleBboAction::Fresh
        );
    }

    #[test]
    fn invalid_prices_and_timestamps_never_become_fair_value() {
        let now = 10_000_000_000;
        for book in [
            msg(f64::NAN, 2101.0, now),
            msg(2100.0, f64::INFINITY, now),
            msg(2101.0, 2100.0, now),
            msg(2100.0, 2101.0, 0),
            msg(2100.0, 2101.0, now + 1),
        ] {
            let mut exchanges = [default_exchange(); NUM_EXCHANGES];
            exchanges[1] = (1, book);
            assert!(external_fair_value_mid(&exchanges, 2, now, 2_000).is_none());
            assert_eq!(
                classify_stale_bbo(&book, now, 5_000, 15_000),
                StaleBboAction::Cancel
            );
        }
    }

    #[test]
    fn external_fair_value_mid_uses_only_fresh_non_local_two_sided_books() {
        let now_ns = 10_000_000_000;
        let exchanges = [
            default_exchange(),
            (1, msg(2100.0, 2101.0, 9_999_000_000)),
            (2, msg(2000.0, 2001.0, 9_999_000_000)),
            (3, msg(2200.0, 2201.0, 9_999_000_000)),
            (4, msg(2300.0, 0.0, 9_999_000_000)), // one-sided, filtered out
            (5, msg(2400.0, 2401.0, 9_000_000_000)), // stale at 500ms threshold
            default_exchange(),
        ];

        // local=2 excluded; exch 4 one-sided excluded; exch 5 stale excluded
        // remaining: exch 1 (2100.5), exch 3 (2200.5) → median = (2100.5+2200.5)/2 = 2150.5
        let external_mid =
            external_fair_value_mid(&exchanges, 2, now_ns, 500).expect("external mid");

        assert!((external_mid - 2150.5).abs() < 1e-9);
    }

    #[test]
    fn external_fair_value_mid_uses_median_not_mean() {
        let now_ns = 10_000_000_000;
        let exchanges = [
            default_exchange(),
            (1, msg(2100.0, 2101.0, 9_999_000_000)),
            (2, msg(2100.0, 2101.0, 9_999_000_000)),
            (3, msg(2500.0, 2501.0, 9_999_000_000)),
            default_exchange(),
            default_exchange(),
            default_exchange(),
        ];

        // local=0 excluded; 3 valid mids: 2100.5, 2100.5, 2500.5 → median = 2100.5
        let external_mid =
            external_fair_value_mid(&exchanges, 0, now_ns, 500).expect("external mid");

        assert!((external_mid - 2100.5).abs() < 1e-9);
    }

    #[test]
    fn external_fair_value_mid_returns_none_when_all_stale() {
        let now_ns = 10_000_000_000;
        let exchanges = [
            default_exchange(),
            (1, msg(2100.0, 2101.0, 7_000_000_000)), // 3000ms old, stale at 2000ms
            default_exchange(),
            default_exchange(),
            default_exchange(),
            default_exchange(),
            default_exchange(),
        ];

        let external_mid = external_fair_value_mid(&exchanges, 0, now_ns, 2000);

        assert!(external_mid.is_none());
    }
    #[test]
    #[ignore = "manual release-mode performance benchmark"]
    fn benchmark_stack_median_vs_allocating_anchor() {
        use std::hint::black_box;
        use std::time::Instant;
        let now = 10_000_000_000;
        let exchanges =
            std::array::from_fn(|i| (i as u8, msg(2100.0 + i as f64, 2101.0 + i as f64, now)));
        let iterations = 200_000;
        let started = Instant::now();
        for _ in 0..iterations {
            black_box(external_fair_value_mid(black_box(&exchanges), 2, now, 2000));
        }
        let stack = started.elapsed();
        let started = Instant::now();
        for _ in 0..iterations {
            // Previous implementation: collect external BBOs, grow a second
            // vector of valid mids, then stable-sort to obtain the median.
            let external: Vec<_> = black_box(&exchanges)
                .iter()
                .filter(|(id, _)| *id != 2 && *id != 0)
                .copied()
                .collect();
            let mut mids = Vec::new();
            for (_, bbo) in external {
                if valid_quoteable_book(&bbo) && !is_stale_bbo(bbo.timestamp_ns, now, 2000) {
                    mids.push((bbo.bid_price + bbo.ask_price) / 2.0);
                }
            }
            mids.sort_by(f64::total_cmp);
            black_box(mids[mids.len() / 2]);
        }
        let allocated = started.elapsed();
        println!(
            "fair-value median: {iterations} updates; stack={stack:?}, previous_allocating={allocated:?}, speedup={:.2}x",
            allocated.as_secs_f64() / stack.as_secs_f64()
        );
    }
}
