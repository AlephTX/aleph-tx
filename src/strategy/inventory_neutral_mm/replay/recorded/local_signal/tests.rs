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
        vol_shock_mult: None,
        vol_recovery_mult: None,
        min_shock_bps_floor: None,
        min_recovery_bps_floor: None,
        entry_touch_offset_bps: None,
        exit_touch_offset_bps: None,
        exit_min_profit_bps: None,
        exit_chase_start_fraction: None,
        post_exit_cooldown_ms: 0,
        hold_exit_while_flow_favorable_min_imbalance: None,
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

#[test]
fn vol_adaptive_thresholds_unlock_calm_regime_reversals_while_enforcing_floor_and_flow() {
    let static_strict = LocalReversalFilter {
        shock_bps: 5.0,
        recovery_bps: 1.5,
        ..policy()
    };
    let mut s_static = LocalSignal::new(static_strict);
    let adaptive = LocalReversalFilter {
        shock_bps: 5.0,
        recovery_bps: 1.5,
        vol_shock_mult: Some(1.6),
        vol_recovery_mult: Some(0.5),
        min_shock_bps_floor: Some(0.5),
        ..policy()
    };
    adaptive.validate().unwrap();
    let mut s_adapt = LocalSignal::new(adaptive);

    for sig in [&mut s_static, &mut s_adapt] {
        sig.feed(10_000, true);
        // Calm 2.0 bps down-shock + 1.0 bps bounce (mid = 100.00 -> 99.98 -> 99.99)
        for at in (10_000..=12_300).step_by(100) {
            let delta = if at <= 10_300 {
                0.0
            } else if at <= 11_800 {
                -0.02
            } else {
                -0.01
            };
            sig.book(at, &book(at, 100.0 + delta));
            sig.trade(at, at, 1_000.0, at >= 11_800);
        }
    }
    assert_eq!(s_static.allowed(12_300), Some([false, false]));
    assert_eq!(s_adapt.allowed(12_300), Some([true, false]));

    // Validation rejects adaptive modifiers without vol_shock_mult or inverted floor.
    assert!(
        LocalReversalFilter {
            min_shock_bps_floor: Some(0.5),
            ..policy()
        }
        .validate()
        .is_err()
    );
    assert!(
        LocalReversalFilter {
            shock_bps: 1.0,
            vol_shock_mult: Some(2.0),
            min_shock_bps_floor: Some(1.5),
            ..policy()
        }
        .validate()
        .is_err()
    );
}

#[test]
fn inverted_vol_recovery_and_implicit_default_floors_fail_preflight_and_never_panic() {
    let warm_signal = |pol: LocalReversalFilter| -> LocalSignal {
        let mut sig = LocalSignal::new(pol);
        sig.feed(10_000, true);
        for at in (10_000..=12_300).step_by(100) {
            let delta = if at <= 10_300 {
                0.0
            } else if at <= 11_800 {
                -0.025
            } else {
                -0.015
            };
            sig.book(at, &book(at, 100.0 + delta));
            sig.trade(at, at, 1_000.0, at >= 11_800);
        }
        sig
    };

    // Exact Codex 14:05 CST bug repro: floor=0.8 -> implicit recovery floor 0.16 > recovery_bps 0.1.
    let codex_repro = LocalReversalFilter {
        shock_bps: 2.0,
        recovery_bps: 0.1,
        vol_shock_mult: Some(2.0),
        min_shock_bps_floor: Some(0.8),
        ..policy()
    };
    assert!(codex_repro.validate().is_err());
    assert!(warm_signal(codex_repro).allowed(12_300).is_some());

    // Implicit default floor=0.5 > shock_bps=0.3, and implicit recovery=0.10 > recovery_bps=0.05.
    let small_shock = LocalReversalFilter {
        shock_bps: 0.3,
        recovery_bps: 0.1,
        vol_shock_mult: Some(2.0),
        ..policy()
    };
    assert!(small_shock.validate().is_err());
    assert!(warm_signal(small_shock).allowed(12_300).is_some());

    let small_recovery = LocalReversalFilter {
        shock_bps: 0.6,
        recovery_bps: 0.05,
        vol_shock_mult: Some(2.0),
        ..policy()
    };
    assert!(small_recovery.validate().is_err());
    assert!(warm_signal(small_recovery).allowed(12_300).is_some());

    // Explicit valid min_recovery_bps_floor unlocks low-recovery policies cleanly.
    let explicit_ok = LocalReversalFilter {
        shock_bps: 2.0,
        recovery_bps: 0.1,
        vol_shock_mult: Some(2.0),
        min_shock_bps_floor: Some(0.8),
        min_recovery_bps_floor: Some(0.05),
        ..policy()
    };
    explicit_ok.validate().unwrap();
    assert_eq!(
        warm_signal(explicit_ok).allowed(12_300),
        Some([true, false])
    );
}

#[tokio::test(start_paused = true)]
async fn entry_touch_offset_tightens_admitted_contrarian_quote_within_post_only_and_fee_bounds() {
    let _clock = ReplayClock::open(12_300, false);
    let mut config = InventoryNeutralMMConfig {
        tick_size: 0.01,
        step_size: 0.01,
        maker_fee_bps: 0.0,
        ..Default::default()
    };
    let pol = LocalReversalFilter {
        entry_touch_offset_bps: Some(0.0),
        ..policy()
    };
    pol.validate().unwrap();
    let mut sig = LocalSignal::new(pol);
    sig.feed(10_000, true);
    for at in (10_000..=12_300).step_by(100) {
        let delta = if at <= 10_300 {
            0.0
        } else if at <= 11_800 {
            -0.02
        } else {
            -0.01
        };
        sig.book(at, &book(at, 100.0 + delta));
        sig.trade(at, at, 1_000.0, at >= 11_800);
    }
    let mut gate = LocalParticipation::new(pol, Arc::new(PlMutex::new(sig)));
    let mut target = QuoteTarget {
        bid_price: 99.90,
        ask_price: 100.08,
        bid_size: 1.0,
        ask_size: 1.0,
    };
    // Contrarian buy signal is admitted while ask entry is withdrawn; deep 8-bps bid joins 99.98 touch.
    gate.apply_with_book(
        &mut target,
        [0.0, 0.0],
        &config,
        Some((99.98, 100.00, 99.99)),
    );
    assert_eq!(target.ask_size, 0.0);
    assert!((target.bid_price - 99.98).abs() < 1e-9);

    // Fee breakeven strictly caps aggressive touch join when maker_fee_bps > 0.
    config.maker_fee_bps = 2.0; // 2 bps on 99.99 -> max_bid <= 99.97
    let mut fee_target = QuoteTarget {
        bid_price: 99.90,
        ask_price: 100.08,
        bid_size: 1.0,
        ask_size: 1.0,
    };
    gate.apply_with_book(
        &mut fee_target,
        [0.0, 0.0],
        &config,
        Some((99.99, 100.00, 99.99)),
    );
    assert!(fee_target.bid_price <= 99.97 + 1e-9);
}

#[tokio::test(start_paused = true)]
async fn passive_exit_touch_chase_holds_profit_early_then_decays_to_scratch_before_timeout() {
    let _clock = ReplayClock::open(12_300, false);
    let config = InventoryNeutralMMConfig {
        tick_size: 0.01,
        step_size: 0.01,
        maker_fee_bps: 0.0,
        ..Default::default()
    };
    let bad = LocalReversalFilter {
        exit_min_profit_bps: Some(5.0),
        ..policy()
    };
    assert!(bad.validate().is_err());

    let pol = LocalReversalFilter {
        entry_touch_offset_bps: Some(0.0),
        exit_touch_offset_bps: Some(0.0),
        exit_min_profit_bps: Some(5.0),
        exit_chase_start_fraction: Some(0.4),
        ..policy()
    };
    pol.validate().unwrap();
    let mut sig = LocalSignal::new(pol);
    sig.feed(10_000, true);
    for at in (10_000..=12_300).step_by(100) {
        let delta = if at <= 10_300 {
            0.0
        } else if at <= 11_800 {
            -0.02
        } else {
            -0.01
        };
        sig.book(at, &book(at, 100.0 + delta));
        sig.trade(at, at, 1_000.0, at >= 11_800);
    }
    let mut gate = LocalParticipation::new(pol, Arc::new(PlMutex::new(sig)));
    assert_eq!(gate.active_touch_sides([0.5, 0.5], 0.01), [true, true]);
    assert_eq!(
        gate.is_reducing_exit_active([0.5, 0.5], 0.01),
        [false, true]
    );
    assert_eq!(
        gate.is_reducing_exit_active([-0.5, -0.5], 0.01),
        [true, false]
    );

    // Long @ 100.00 entry VWAP, early age (0.1 < 0.4): profit floor is 100.00 * (1 + 5 bps) = 100.05.
    let mut early_target = QuoteTarget {
        bid_price: 99.80,
        ask_price: 100.12,
        bid_size: 1.0,
        ask_size: 1.0,
    };
    gate.apply_with_context(
        &mut early_target,
        [0.5, 0.5],
        &config,
        Some(LocalBookContext {
            bid_touch: 99.99,
            ask_touch: 100.00,
            pricing_mid: 99.995,
            entry_vwap: Some(100.00),
            age_fraction: 0.1,
        }),
    );
    assert!((early_target.ask_price - 100.05).abs() < 1e-9);
    assert!((early_target.ask_size - 0.5).abs() < 1e-9);

    // Late age (1.0): profit floor decays to zero, chasing ask touch 100.00 (never crossing 99.99 bid).
    let mut late_target = QuoteTarget {
        bid_price: 99.80,
        ask_price: 100.12,
        bid_size: 1.0,
        ask_size: 1.0,
    };
    gate.apply_with_context(
        &mut late_target,
        [0.5, 0.5],
        &config,
        Some(LocalBookContext {
            bid_touch: 99.99,
            ask_touch: 100.00,
            pricing_mid: 99.995,
            entry_vwap: Some(100.00),
            age_fraction: 1.0,
        }),
    );
    assert!((late_target.ask_price - 100.00).abs() < 1e-9);
    assert!((late_target.ask_size - 0.5).abs() < 1e-9);

    // Short @ 100.00 entry VWAP, late age (1.0): chases bid touch 99.99 without crossing 100.00 ask.
    let mut short_late = QuoteTarget {
        bid_price: 99.80,
        ask_price: 100.12,
        bid_size: 1.0,
        ask_size: 1.0,
    };
    gate.apply_with_context(
        &mut short_late,
        [-0.5, -0.5],
        &config,
        Some(LocalBookContext {
            bid_touch: 99.99,
            ask_touch: 100.00,
            pricing_mid: 99.995,
            entry_vwap: Some(100.00),
            age_fraction: 1.0,
        }),
    );
    assert!((short_late.bid_price - 99.99).abs() < 1e-9);
    assert!((short_late.bid_size - 0.5).abs() < 1e-9);
}

#[tokio::test(start_paused = true)]
async fn passive_exit_handles_lagging_account_snapshots_and_sign_conflicts_without_overshoot() {
    let _clock = ReplayClock::open(12_300, false);
    let config = InventoryNeutralMMConfig {
        tick_size: 0.01,
        step_size: 0.01,
        maker_fee_bps: 0.0,
        ..Default::default()
    };
    let pol = LocalReversalFilter {
        entry_touch_offset_bps: Some(0.0),
        exit_touch_offset_bps: Some(0.0),
        exit_min_profit_bps: Some(2.0),
        exit_chase_start_fraction: Some(0.4),
        ..policy()
    };
    let mut sig = LocalSignal::new(pol);
    sig.feed(10_000, true);
    for at in (10_000..=12_300).step_by(100) {
        sig.book(at, &book(at, 100.0));
        sig.trade(at, at, 100.0, true); // no reversal signal admitted -> [false, false]
    }
    let mut gate = LocalParticipation::new(pol, Arc::new(PlMutex::new(sig)));
    let ctx = Some(LocalBookContext {
        bid_touch: 99.99,
        ask_touch: 100.00,
        pricing_mid: 99.995,
        entry_vwap: Some(100.00),
        age_fraction: 1.0,
    });

    // 1. Fresh buy fill confirmed (+0.5) while account snapshot lags at 0.0 -> immediate capped ask exit @ 0.5, bid withdrawn.
    assert_eq!(
        gate.is_reducing_exit_active([0.0, 0.5], 0.01),
        [false, true]
    );
    let mut t1 = QuoteTarget {
        bid_price: 99.80,
        ask_price: 100.12,
        bid_size: 1.0,
        ask_size: 1.0,
    };
    gate.apply_with_context(&mut t1, [0.0, 0.5], &config, ctx);
    assert_eq!(t1.bid_size, 0.0);
    assert!((t1.ask_price - 100.00).abs() < 1e-9);
    assert!((t1.ask_size - 0.5).abs() < 1e-9);

    // 2. Exit filled to flat ([+0.5, 0.0] with lagging account snapshot) -> stops ask exit and withdraws un-admitted short!
    assert_eq!(
        gate.is_reducing_exit_active([0.5, 0.0], 0.01),
        [false, false]
    );
    let mut t2 = QuoteTarget {
        bid_price: 99.80,
        ask_price: 100.12,
        bid_size: 1.0,
        ask_size: 1.0,
    };
    gate.apply_with_context(&mut t2, [0.5, 0.0], &config, ctx);
    assert_eq!((t2.bid_size, t2.ask_size), (0.0, 0.0));

    // 3. Same for short return-to-flat ([-0.5, 0.0]).
    let mut t3 = QuoteTarget {
        bid_price: 99.80,
        ask_price: 100.12,
        bid_size: 1.0,
        ask_size: 1.0,
    };
    gate.apply_with_context(&mut t3, [-0.5, 0.0], &config, ctx);
    assert_eq!((t3.bid_size, t3.ask_size), (0.0, 0.0));

    // 4. Both positive, account=1.0 vs confirmed=0.4 -> caps reducing ask to min = 0.4.
    let mut t4 = QuoteTarget {
        bid_price: 99.80,
        ask_price: 100.12,
        bid_size: 1.0,
        ask_size: 1.0,
    };
    gate.apply_with_context(&mut t4, [1.0, 0.4], &config, ctx);
    assert!((t4.ask_size - 0.4).abs() < 1e-9);

    // 5. Genuine opposite-sign conflict ([+0.5, -0.5]) -> no reducing touch chase and un-admitted sides withdrawn.
    assert_eq!(
        gate.is_reducing_exit_active([0.5, -0.5], 0.01),
        [false, false]
    );
    let mut t5 = QuoteTarget {
        bid_price: 99.80,
        ask_price: 100.12,
        bid_size: 1.0,
        ask_size: 1.0,
    };
    gate.apply_with_context(&mut t5, [0.5, -0.5], &config, ctx);
    assert_eq!((t5.bid_size, t5.ask_size), (0.0, 0.0));
}

#[tokio::test(start_paused = true)]
async fn post_exit_cooldown_and_flow_favorable_profit_hold_behave_deterministically() {
    let start = 12_300_u64;
    let _clock = ReplayClock::open(start, false);
    let config = InventoryNeutralMMConfig {
        tick_size: 0.01,
        step_size: 0.01,
        maker_fee_bps: 0.0,
        ..Default::default()
    };
    let pol = LocalReversalFilter {
        entry_touch_offset_bps: Some(0.0),
        exit_touch_offset_bps: Some(0.0),
        exit_min_profit_bps: Some(10.0),
        exit_chase_start_fraction: Some(0.4),
        post_exit_cooldown_ms: 2_000,
        hold_exit_while_flow_favorable_min_imbalance: Some(0.5),
        ..policy()
    };
    pol.validate().unwrap();

    let sig = reversal(false);
    // Keep last buckets strongly buy-dominated (+1.0 imbalance over recovery_ms = 500ms).
    let sig_ref = Arc::new(PlMutex::new(sig));
    let mut gate = LocalParticipation::new(pol, Arc::clone(&sig_ref));

    // 1. Open long position -> exit_min_profit_bps (10 bps) holds at age_fraction = 0.7 (< 0.85)
    //    because recent recovery flow imbalance = +1.0 >= 0.5 (favorable to Long Ask close).
    let base_ctx = LocalBookContext {
        bid_touch: 99.99,
        ask_touch: 100.00,
        pricing_mid: 100.00,
        entry_vwap: Some(100.00),
        age_fraction: 0.70,
    };
    let hold_ctx = Some(base_ctx);
    let mut exit_target = QuoteTarget {
        bid_price: 99.80,
        ask_price: 100.50,
        bid_size: 1.0,
        ask_size: 1.0,
    };
    gate.apply_with_context(&mut exit_target, [0.5, 0.5], &config, hold_ctx);
    // Full 10 bps hold over 100.00 entry VWAP = 100.10 (does NOT decay halfway toward touch 100.00!).
    assert!(
        (exit_target.ask_price - 100.10).abs() < 1e-9,
        "favorable flow should hold 10 bps floor @ 100.10, got {}",
        exit_target.ask_price
    );

    // Above 0.85 safety cap, decay resumes even if flow is favorable.
    let cap_ctx = Some(LocalBookContext {
        age_fraction: 0.90,
        ..base_ctx
    });
    let mut cap_target = QuoteTarget {
        bid_price: 99.80,
        ask_price: 100.50,
        bid_size: 1.0,
        ask_size: 1.0,
    };
    gate.apply_with_context(&mut cap_target, [0.5, 0.5], &config, cap_ctx);
    assert!(cap_target.ask_price < 100.05);

    // 2. Transition to flat [0.0, 0.0] triggers 2000ms post_exit_cooldown_ms even while reversal signal is fresh.
    let mut flat_target = QuoteTarget {
        bid_price: 99.80,
        ask_price: 100.20,
        bid_size: 1.0,
        ask_size: 1.0,
    };
    gate.apply_with_context(&mut flat_target, [0.0, 0.0], &config, hold_ctx);
    assert_eq!((flat_target.bid_size, flat_target.ask_size), (0.0, 0.0));
    assert!(gate.cooldown_withdrawn_targets >= 1);

    // 3. After cooldown expires (advance 3000ms for a clean fresh window and refresh book+trade), reversal entry re-admits cleanly.
    tokio::time::advance(Duration::from_millis(3_000)).await;
    let now = start + 3_000;
    {
        let mut s = sig_ref.lock();
        // Keep recovery_ms=500ms drop-then-bounce active on the new clock:
        // [now-2300..=now-2000] = 100.0 -> [now-1900..=now-500] = 99.98 (-2 bps shock, sell flow)
        // -> [now-400..=now] = 99.99 (+1 bps recovery, buy flow).
        for t in (now - 2_300..=now - 2_000).step_by(100) {
            s.book(t, &book(t, 100.0));
            s.trade(t, t, 1_000.0, false);
        }
        for t in (now - 1_900..=now - 600).step_by(100) {
            s.book(t, &book(t, 99.98));
            s.trade(t, t, 1_000.0, false);
        }
        for t in (now - 500..=now).step_by(100) {
            s.book(t, &book(t, if t == now - 500 { 99.98 } else { 99.99 }));
            s.trade(t, t, 200.0, true);
        }
    }
    let mut retry_target = QuoteTarget {
        bid_price: 99.80,
        ask_price: 100.20,
        bid_size: 1.0,
        ask_size: 1.0,
    };
    gate.apply_with_context(&mut retry_target, [0.0, 0.0], &config, hold_ctx);
    assert!(retry_target.bid_size > 0.0);
}
