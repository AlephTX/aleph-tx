use super::*;

fn policy() -> LocalReversalFilter {
    LocalReversalFilter {
        lookback_ms: 2_000,
        recovery_ms: 500,
        max_age_ms: 500,
        shock_bps: 0.5,
        recovery_bps: 0.1,
        flow_imbalance: 0.6,
        min_shock_notional: 1_000.0,
        entry_hold_ms: 0,
    }
}

fn book(at: u64, mid: f64) -> Book {
    Book {
        timestamp_ms: at,
        bids: vec![[mid - 0.001, 1.0]],
        asks: vec![[mid + 0.001, 1.0]],
    }
}

fn reversal(up: bool) -> LocalSignal {
    let mut signal = LocalSignal::new(policy());
    signal.feed(10_000, true);
    for at in (10_000..=12_300).step_by(100) {
        let move_by = if at <= 10_300 {
            0.0
        } else if at <= 11_800 {
            0.02
        } else {
            0.01
        };
        let mid = 100.0 + if up { move_by } else { -move_by };
        signal.book(at, &book(at, mid));
        signal.trade(at, at, 1_000.0, (at < 11_800) == up);
    }
    signal
}

#[test]
fn opposite_flow_and_price_recovery_admit_only_the_contrarian_entry() {
    assert_eq!(reversal(false).allowed(12_300), Some([true, false]));
    assert_eq!(reversal(true).allowed(12_300), Some([false, true]));
    let mut signal = reversal(false);
    signal.book(12_300, &book(12_300, 99.97));
    assert_eq!(signal.allowed(12_300), Some([false, false]));
}

#[test]
fn future_and_late_prints_cannot_become_predictors() {
    let mut signal = reversal(false);
    signal.trade(12_300, 12_301, 1e9, false);
    signal.trade(12_300, 11_799, 1e9, false);
    assert_eq!(signal.rejected_trade_times, 2);
    assert_eq!(signal.allowed(12_300), Some([true, false]));
    // A just-arrived batch belongs to the incomplete current receipt bucket.
    signal.trade(12_300, 12_300, 1e9, false);
    assert_eq!(signal.allowed(12_300), Some([true, false]));
    assert_eq!(signal.allowed(12_801), None);
}

#[test]
fn unknown_feed_disconnect_and_missing_history_fail_closed() {
    let mut signal = LocalSignal::new(policy());
    signal.book(12_300, &book(12_300, 100.0));
    signal.trade(12_300, 12_300, 1e6, true);
    assert_eq!(signal.allowed(12_300), None);
    let mut signal = reversal(false);
    signal.feed(12_300, false);
    assert_eq!(signal.allowed(12_300), None);
    signal.feed(12_301, true);
    signal.trade(12_302, 12_000, 1e6, false);
    assert_eq!(signal.rejected_trade_times, 1);
    assert_eq!(signal.allowed(12_302), None);
}

#[test]
fn historical_lookup_never_reads_a_later_publication_in_the_bucket() {
    let mut signal = reversal(false);
    signal.book(12_350, &book(12_350, 200.0));
    let current = signal.book_before(12_300).unwrap();
    assert!(current.book_at <= 12_300);
    assert!(current.mid < 101.0);
    // Reused ring slots cannot silently supply the missing history.
    signal.book(20_000, &book(20_000, 100.0));
    signal.trade(20_000, 20_000, 1_000.0, true);
    assert_eq!(signal.allowed(20_000), None);
}

#[tokio::test(start_paused = true)]
async fn an_unready_filter_withdraws_entries_but_preserves_possible_exits() {
    let _clock = ReplayClock::open(12_300, false);
    let config = InventoryNeutralMMConfig::default();
    let signal = Arc::new(PlMutex::new(LocalSignal::new(policy())));
    let mut gate = LocalParticipation::new(policy(), signal);
    for (positions, expected) in [
        ([0.0, 0.0], [0.0, 0.0]),
        ([1.0, 1.0], [0.0, 1.0]),
        ([-1.0, -1.0], [1.0, 0.0]),
        ([1.0, -1.0], [1.0, 1.0]),
    ] {
        let mut target = QuoteTarget {
            bid_price: 100.0,
            ask_price: 101.0,
            bid_size: 1.0,
            ask_size: 1.0,
        };
        gate.apply(&mut target, positions, &config);
        assert_eq!([target.bid_size, target.ask_size], expected);
    }
    assert_eq!(gate.ready_decisions, 0);
}

#[test]
fn invalid_policy_cannot_request_unbounded_or_noncausal_history() {
    policy().validate().unwrap();
    for value in [
        LocalReversalFilter {
            lookback_ms: 10_000,
            ..policy()
        },
        LocalReversalFilter {
            recovery_ms: 2_000,
            ..policy()
        },
        LocalReversalFilter {
            shock_bps: f64::NAN,
            ..policy()
        },
        LocalReversalFilter {
            flow_imbalance: 1.0,
            ..policy()
        },
        LocalReversalFilter {
            entry_hold_ms: 1_001,
            ..policy()
        },
    ] {
        assert!(value.validate().is_err());
    }
}

#[test]
fn held_entry_is_bounded_per_side_and_cannot_survive_unready_evidence() {
    let p = LocalReversalFilter {
        entry_hold_ms: 1_000,
        ..policy()
    };
    let signal = Arc::new(PlMutex::new(LocalSignal::new(p)));
    let mut gate = LocalParticipation::new(p, signal);
    assert_eq!(gate.with_hold(10_000, Some([true, false])), [true, false]);
    assert_eq!(gate.with_hold(10_500, Some([true, false])), [true, false]);
    // Repeated true samples do not keep extending the original permit.
    assert_eq!(gate.with_hold(10_999, Some([false, false])), [true, false]);
    assert_eq!(gate.with_hold(11_000, Some([false, false])), [false, false]);
    assert_eq!(gate.with_hold(11_100, Some([false, true])), [false, true]);
    assert_eq!(gate.with_hold(11_101, None), [false, false]);
    assert_eq!(gate.with_hold(11_102, Some([false, false])), [false, false]);
}
