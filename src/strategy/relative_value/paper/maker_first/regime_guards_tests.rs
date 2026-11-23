use super::*;

fn pair_book(cfg: &MakerFirstConfig, at: u64, maker_mid: f64, hedge_mid: f64) -> MakerFirstEvent {
    let half_spread = 0.1_f64;
    let fmt = |v: f64| -> Decimal { Decimal::from_f64_retain(v).unwrap().round_dp(4) };
    let level = |px: f64, contracts: &str| Level {
        price: fmt(px),
        contracts: d(contracts),
    };
    let m_bid = maker_mid - half_spread;
    let m_ask = maker_mid + half_spread;
    let h_bid = hedge_mid - half_spread;
    let h_ask = hedge_mid + half_spread;
    MakerFirstEvent::Book {
        at_ms: at,
        books: [
            Book {
                market: cfg.pair.legs[0].market.clone(),
                timestamp_ms: at,
                tradable: true,
                bids: vec![level(m_bid, "1")],
                asks: vec![level(m_ask, "1")],
                bounds: None,
                available_notional: Decimal::from(500),
            },
            Book {
                market: cfg.pair.legs[1].market.clone(),
                timestamp_ms: at,
                tradable: true,
                bids: vec![level(h_bid, "1")],
                asks: vec![level(h_ask, "1")],
                bounds: None,
                available_notional: Decimal::from(500),
            },
        ],
        reference: None,
        equity_session: None,
        leg_oracles: None,
    }
}

#[test]
fn slow_cascade_throttles_opening_bid_halts_severe_drop_and_never_touches_closing() {
    const H: u64 = 3_600_000;
    let bar_ms = 30_000_u64;
    let base_cfg = |entry_side: HedgeSide| {
        let mut cfg = config();
        cfg.start_ms = H + 100_000;
        cfg.end_ms = H + 3_500_000;
        cfg.entry_side = Some(entry_side);
        cfg.quote_notional = Decimal::from(200);
        cfg.pair.planner.max_quote_age_ms = 5_000;
        cfg.pair.planner.max_quote_skew_ms = 5_000;
        cfg.quote_lifetime_ms = 5_000;
        cfg.hold_ms = 1_000;
        cfg.pair.legs.iter_mut().for_each(|leg| leg.tick = d("0.0001"));
        cfg.slow_cascade = Some(SlowCascadeConfig {
            bar_ms,
            window_bars: 10,
            vol_lookback_bars: 20,
            exit_z: 0.7,
            warn_z: 1.8,
            halt_z: 3.0,
            min_hold_ms: 120_000,
            min_entry_scale: 0.25,
            max_bar_gap_ms: 120_000,
        });
        cfg
    };

    // Build tape aligned inside one hour: 25 baseline bars, 6 moderate-drop bars (warning band),
    // then 20 severe-drop bars (halt band).
    let build_events = |cfg: &MakerFirstConfig| {
        let t0 = cfg.start_ms;
        let mut events = vec![MakerFirstEvent::TradeFeed {
            at_ms: t0,
            leg: 0,
            online: true,
        }];
        let mut px = 1000.0_f64;
        let mut step = 0_u64;
        for i in 0..25_u64 {
            let ret: f64 = if i % 2 == 0 { 0.0004 } else { -0.0004 };
            px *= ret.exp();
            events.push(pair_book(cfg, t0 + step * bar_ms, px, px));
            step += 1;
        }
        // Moderate decline enters Downward warning band (`0.25 <= bid_entry_scale < 1.0`).
        for _ in 0..8_u64 {
            px *= (-0.00055_f64).exp();
            events.push(pair_book(cfg, t0 + step * bar_ms, px, px));
            step += 1;
        }
        // Extra frame right after a quote is submitted so we can catch resting cancel on severe plunge.
        for _ in 0..25_u64 {
            px *= (-0.0012_f64).exp();
            let at = t0 + step * bar_ms;
            events.push(pair_book(cfg, at, px, px));
            step += 1;
        }
        events.push(MakerFirstEvent::End { at_ms: cfg.end_ms });
        events
    };

    // 1. Opening Buy gets scaled down during warning band, then halted during severe drop.
    let buy_cfg = base_cfg(HedgeSide::Buy);
    let buy_rep = run_maker_first(
        &buy_cfg,
        build_events(&buy_cfg).into_iter().map(Ok),
        &VenueRegistry::default(),
    )
    .unwrap();
    let sc_buy = buy_rep.slow_cascade.expect("slow_cascade report");
    assert_eq!(sc_buy.active_side, CascadeSide::Downward);
    assert!(sc_buy.scaled_opening_entries > 0, "{sc_buy:?}");
    assert!(sc_buy.blocked_opening_entries > 0, "{sc_buy:?}");
    assert_eq!(buy_rep.entry_blocked.get("slow_cascade_halt").copied(), Some(sc_buy.blocked_opening_entries));

    // 2. Opening Sell during the exact same downward cascade stays at 1.0x (never blocked or scaled).
    let sell_cfg = base_cfg(HedgeSide::Sell);
    let sell_rep = run_maker_first(
        &sell_cfg,
        build_events(&sell_cfg).into_iter().map(Ok),
        &VenueRegistry::default(),
    )
    .unwrap();
    let sc_sell = sell_rep.slow_cascade.expect("slow_cascade report");
    assert_eq!(sc_sell.active_side, CascadeSide::Downward);
    assert_eq!(sc_sell.scaled_opening_entries, 0, "{sc_sell:?}");
    assert_eq!(sc_sell.blocked_opening_entries, 0, "{sc_sell:?}");
    assert_eq!(sc_sell.cancelled_resting_entries, 0, "{sc_sell:?}");

    // 3. Severe drop cancels a resting opening Buy quote while NEVER cancelling a closing Buy quote.
    let mut r = new_replay(&buy_cfg).unwrap();
    r.feed_online = true;
    let mut px = 1000.0_f64;
    let mut step = 0_u64;
    let t0 = buy_cfg.start_ms;
    for i in 0..25_u64 {
        px *= (if i % 2 == 0 { 0.0004_f64 } else { -0.0004_f64 }).exp();
        if let MakerFirstEvent::Book { books, .. } = pair_book(&buy_cfg, t0 + step * bar_ms, px, px)
        {
            r.set_books(books, t0 + step * bar_ms).unwrap();
        }
        step += 1;
    }
    let calm_at = t0 + step * bar_ms;
    if let MakerFirstEvent::Book { books, .. } = pair_book(&buy_cfg, calm_at, px, px) {
        r.set_books(books, calm_at).unwrap();
    }
    // Seed a resting opening Buy quote before plunging into Halt band.
    r.quote = Some(Quote {
        posture: TurnoverPosture::Neutral,
        side: HedgeSide::Buy,
        price: d("999.0"),
        left: d("0.2"),
        arrival_ms: calm_at + 100,
        activated_ms: Some(calm_at + 100),
        expires_ms: calm_at + 2_000_000,
        cancelled_ms: None,
        ahead: Decimal::ZERO,
        queue_depth: None,
        hedge_limit: Some(d("1000.0")),
        passive_entry: false,
        closing: false,
        closing_touched: false,
        closing_filled: false,
    });
    for _ in 0..20_u64 {
        step += 1;
        px *= (-0.002_f64).exp();
        let at = t0 + step * bar_ms;
        if let MakerFirstEvent::Book { books, .. } = pair_book(&buy_cfg, at, px, px) {
            r.set_books(books, at).unwrap();
        }
    }
    let halt_at = t0 + step * bar_ms;
    r.advance_quote(halt_at, true);
    assert!(
        r.quote
            .as_ref()
            .is_some_and(|q| !q.closing && q.cancelled_ms.is_some()),
        "resting opening Buy must cancel under severe downward slow cascade"
    );
    assert_eq!(
        r.report.opening_cancel_requested.get("slow_cascade_halt"),
        Some(&1)
    );

    // Same severe downward cascade snapshot with a CLOSING Buy quote must leave closing quote untouched.
    r.ledger.positions = [d("-0.2"), d("0.2")];
    r.quote = Some(Quote {
        posture: TurnoverPosture::Neutral,
        side: HedgeSide::Buy,
        price: Decimal::from_f64_retain(px - 0.1).unwrap().round_dp(4),
        left: d("0.2"),
        arrival_ms: halt_at,
        activated_ms: Some(halt_at),
        expires_ms: halt_at + 600_000,
        cancelled_ms: None,
        ahead: Decimal::ZERO,
        queue_depth: None,
        hedge_limit: Some(d("999.0")),
        passive_entry: false,
        closing: true,
        closing_touched: false,
        closing_filled: false,
    });
    r.advance_quote(halt_at, true);
    assert!(
        r.quote
            .as_ref()
            .is_some_and(|q| q.closing && q.cancelled_ms.is_none()),
        "slow_cascade must NEVER cancel a closing quote"
    );
    assert!(
        r.report
            .cycle_attribution
            .closing_quotes
            .cancel_requested
            .is_empty()
    );
}

#[test]
fn kalman_basis_gates_opening_cancels_resting_on_regime_shift_and_triggers_time_stop() {
    const H: u64 = 3_600_000;
    let registry = VenueRegistry::default();

    // 1. Validation: rejects non-PairHedge mode and mutual exclusion with basis_reversion.
    let mut bad_mode = config();
    bad_mode.pair.mode = PaperMode::Turnover {
        max_turnover_quote: d("1000"),
        max_fees_quote: d("10"),
        max_realized_loss_quote: None,
        incentive: None,
    };
    bad_mode.kalman_basis = Some(KalmanBasisConfig::default());
    assert!(bad_mode.validate(&registry).is_err());

    let mut conflict = config();
    conflict.basis_reversion = Some(MakerBasisReversion {
        warmup_ms: 5_000,
        half_life_ms: 5_000,
        max_gap_ms: 2_000,
        min_dislocation_bps: d("1"),
        cancel_dislocation_bps: Decimal::ZERO,
        reversion_fraction: d("0.5"),
        use_quote_level_dislocation: false,
        max_dislocation_bps: None,
        min_persist_ms: None,
    });
    conflict.kalman_basis = Some(KalmanBasisConfig::default());
    assert!(conflict.validate(&registry).is_err());

    let kb_cfg = KalmanBasisConfig {
        q_per_sec: 0.0005,
        r_base_bps2: 4.0,
        spread_r_coeff: 0.0,
        initial_variance_p: 0.25,
        reset_variance_p: 0.25,
        max_gap_ms: 10_000,
        ou_window: 32,
        ou_min_samples: 8,
        phi_max_stationary: 0.98,
        min_dislocation_bps: 3.0,
        max_dislocation_bps: 25.0,
        time_stop_half_life_mult: 2.0,
        max_hold_ms_cap: 120_000,
    };

    let make_cfg = |entry_side: Option<HedgeSide>| {
        let mut cfg = config();
        cfg.start_ms = H + 100_000;
        cfg.end_ms = H + 600_000;
        cfg.entry_side = entry_side;
        cfg.maker_fee_bps = Decimal::ZERO;
        cfg.pair.legs[0].taker_fee_bps = Decimal::ZERO;
        cfg.pair.legs[1].taker_fee_bps = Decimal::ZERO;
        cfg.pair.planner.max_quote_age_ms = 5_000;
        cfg.pair.planner.max_quote_skew_ms = 5_000;
        cfg.quote_lifetime_ms = 5_000;
        cfg.pair.legs.iter_mut().for_each(|leg| leg.tick = d("0.0001"));
        cfg.hedged_exit = Some(HedgedExit {
            max_hold_ms: 180_000,
            min_saving_bps: Decimal::ZERO,
            max_adverse_basis_bps: d("100"),
            use_cycle_deadline: false,
            closing_quote_lifetime_ms: None,
            patience_ms: 0,
            target_cycle_edge_bps: None,
            closing_max_levels: 1,
            max_hedged_book_stale_ms: None,
            max_hedged_leg_adverse_bps: None,
            hedged_leg_collateral_quote: None,
            allow_partner_step_deadband: false,
            max_hedged_both_blind_ms: None,
        });
        cfg.kalman_basis = Some(kb_cfg);
        cfg
    };

    // Deterministic stationary oscillation around +6 bps followed by -6 bps favorable Buy shock,
    // and then a linear random-walk breakout to force NonStationary cancellation.
    let stationary_series = |steps: usize| -> Vec<f64> {
        let mut state = 42_u64;
        let phi = (-std::f64::consts::LN_2 / 4.0_f64).exp();
        let mut y = 0.0_f64;
        (0..steps)
            .map(|_| {
                let mut u = 0.0;
                for _ in 0..6 {
                    state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                    u += ((state >> 32) as f64) / (u32::MAX as f64);
                }
                let z = (u - 3.0) * std::f64::consts::SQRT_2;
                y = phi * y + 1.5 * z;
                6.0 + y
            })
            .collect()
    };

    // 2. Warmup blocks opening quotes; side mismatch blocks Sell when dislocation favors Buy.
    let sell_cfg = make_cfg(Some(HedgeSide::Sell));
    let mut sell_replay = new_replay(&sell_cfg).unwrap();
    sell_replay.feed_online = true;
    let t0 = sell_replay.config.start_ms;
    let series = stationary_series(48);
    for (idx, basis_bps) in series.iter().enumerate() {
        let at = t0 + (idx as u64) * 1_000;
        let maker_mid = 1000.0 * (1.0 + basis_bps / 10_000.0);
        if let MakerFirstEvent::Book {
            books,
            reference,
            equity_session,
            leg_oracles,
            ..
        } = pair_book(&sell_replay.config, at, maker_mid, 1000.0)
        {
            sell_replay.set_books(books, at).unwrap();
            sell_replay.observe_kalman_basis(at, reference, equity_session, leg_oracles);
        }
        if idx < 7 {
            sell_replay.next_quote_ms = at;
            sell_replay.quote(at).unwrap();
        }
    }
    // Now push a clean -6 bps basis (disloc ~ -12 bps -> favors Buy, mismatches Sell).
    let disloc_at = t0 + 48_000;
    let neg_maker_mid = 1000.0 * (1.0 - 6.0 / 10_000.0);
    if let MakerFirstEvent::Book {
        books,
        reference,
        equity_session,
        leg_oracles,
        ..
    } = pair_book(&sell_replay.config, disloc_at, neg_maker_mid, 1000.0)
    {
        sell_replay.set_books(books, disloc_at).unwrap();
        sell_replay.observe_kalman_basis(disloc_at, reference, equity_session, leg_oracles);
    }
    sell_replay.next_quote_ms = disloc_at;
    sell_replay.quote(disloc_at).unwrap();
    let sell_kb = sell_replay.kalman_basis.as_ref().unwrap();
    assert!(sell_kb.blocked_warmup_or_invalid > 0, "{:?}", sell_kb.report());
    assert!(sell_kb.blocked_side_mismatch > 0, "{:?}", sell_kb.report());

    // 3. With Buy permitted: admits opening quote, cancels resting Buy on NonStationary trend,
    //    never cancels closing Buy, and triggers paired time-stop drain once hold >= cap.
    let buy_cfg = make_cfg(Some(HedgeSide::Buy));
    let mut buy_replay = new_replay(&buy_cfg).unwrap();
    buy_replay.feed_online = true;
    for (idx, basis_bps) in series.iter().enumerate() {
        let at = t0 + (idx as u64) * 1_000;
        let maker_mid = 1000.0 * (1.0 + basis_bps / 10_000.0);
        if let MakerFirstEvent::Book {
            books,
            reference,
            equity_session,
            leg_oracles,
            ..
        } = pair_book(&buy_replay.config, at, maker_mid, 1000.0)
        {
            buy_replay.set_books(books, at).unwrap();
            buy_replay.observe_kalman_basis(at, reference, equity_session, leg_oracles);
        }
    }
    let buy_snap_before = buy_replay.kalman_basis.as_ref().unwrap().last_snapshot.unwrap();
    assert_eq!(buy_snap_before.stationarity, StationarityStatus::Stationary);

    if let MakerFirstEvent::Book {
        books,
        reference,
        equity_session,
        leg_oracles,
        ..
    } = pair_book(&buy_replay.config, disloc_at, neg_maker_mid, 1000.0)
    {
        buy_replay.set_books(books, disloc_at).unwrap();
        buy_replay.observe_kalman_basis(disloc_at, reference, equity_session, leg_oracles);
    }
    assert!(
        buy_replay
            .kalman_basis
            .as_ref()
            .unwrap()
            .check_entry(HedgeSide::Buy)
            .is_ok(),
        "expected stationary favorable Buy entry: {:?}",
        buy_replay.kalman_basis.as_ref().unwrap().last_snapshot
    );
    buy_replay.next_quote_ms = disloc_at;
    buy_replay.quote(disloc_at).unwrap();
    let act_at = disloc_at + 100;
    if let MakerFirstEvent::Book {
        books,
        reference,
        equity_session,
        leg_oracles,
        ..
    } = pair_book(&buy_replay.config, act_at, neg_maker_mid, 1000.0)
    {
        buy_replay.set_books(books, act_at).unwrap();
        buy_replay.observe_kalman_basis(act_at, reference, equity_session, leg_oracles);
    }
    buy_replay.advance_quote(act_at, true);
    assert!(
        buy_replay.quote.as_ref().is_some_and(|q| !q.closing
            && q.side == HedgeSide::Buy
            && q.activated_ms == Some(act_at)
            && q.cancelled_ms.is_none()),
        "Buy opening quote should activate cleanly at arrival under stationary dislocation"
    );

    // Monotone runaway basis trend breaks stationarity -> cancels resting opening quote.
    let mut trend_at = disloc_at;
    let mut saw_cancel_requested = false;
    for step in 1..=35_u64 {
        trend_at = disloc_at + step * 1_000;
        let trend_bps = 6.0 + 3.0 * (step as f64);
        let maker_mid = 1000.0 * (1.0 + trend_bps / 10_000.0);
        if let MakerFirstEvent::Book {
            books,
            reference,
            equity_session,
            leg_oracles,
            ..
        } = pair_book(&buy_replay.config, trend_at, maker_mid, 1000.0)
        {
            buy_replay.set_books(books, trend_at).unwrap();
            buy_replay.observe_kalman_basis(trend_at, reference, equity_session, leg_oracles);
        }
        buy_replay.advance_quote(trend_at, true);
        if buy_replay
            .quote
            .as_ref()
            .is_some_and(|q| !q.closing && q.cancelled_ms.is_some())
        {
            saw_cancel_requested = true;
        }
    }
    assert!(
        saw_cancel_requested,
        "kalman_basis shift must cancel resting opening quote"
    );
    assert!(
        buy_replay.quote.is_none(),
        "cancelled opening quote should retire after grace window"
    );
    assert_eq!(
        buy_replay
            .report
            .opening_cancel_requested
            .get("kalman_basis_regime"),
        Some(&1)
    );
    assert_eq!(
        buy_replay
            .kalman_basis
            .as_ref()
            .unwrap()
            .cancelled_resting_entries,
        1
    );

    // Same NonStationary snapshot must NEVER cancel a closing quote, and paired cycle triggers time_stop!
    buy_replay.ledger.positions = [d("-0.1"), d("0.1")];
    buy_replay.quote = Some(Quote {
        posture: TurnoverPosture::Neutral,
        side: HedgeSide::Buy,
        price: d("999.9"),
        left: d("0.1"),
        arrival_ms: trend_at,
        activated_ms: Some(trend_at),
        expires_ms: trend_at + 10_000,
        cancelled_ms: None,
        ahead: Decimal::ZERO,
        queue_depth: None,
        hedge_limit: Some(d("999.0")),
        passive_entry: false,
        closing: true,
        closing_touched: false,
        closing_filled: false,
    });
    let paired_cycle = Cycle {
        id: 1,
        posture: TurnoverPosture::Neutral,
        started_ms: t0,
        initial_cash: Decimal::ZERO,
        initial_turnover: Decimal::ZERO,
        initial_fees: Decimal::ZERO,
        draining: false,
        externalizing: false,
        maker_turnover: Decimal::ZERO,
        passive_side: None,
        entry_ref_mid: None,
        paired_at_ms: Some(disloc_at),
        hedge_fill_at_ms: Some(disloc_at),
        paired_basis_bps: Some(d("-6")),
        paired_mids: [Some(d("999.4")), Some(d("1000.0"))],
        closing_maker_filled: false,
        forced_ioc_filled: false,
        sub_min_top_up_attempts: 0,
        sub_min_recovery_pending_base: Decimal::ZERO,
        used_partner_step_deadband: false,
        cash_exit_triggered: false,
    };
    buy_replay.cycle = Some(paired_cycle);
    buy_replay.advance_quote(trend_at, true);
    assert!(
        buy_replay
            .quote
            .as_ref()
            .is_some_and(|q| q.closing && q.cancelled_ms.is_none()),
        "kalman_basis must NEVER cancel closing quote"
    );
    buy_replay.advance(trend_at).unwrap();
    assert!(
        buy_replay.cycle.as_ref().is_some_and(|c| c.draining),
        "kalman_basis NonStationary / time-stop must trigger draining"
    );
    assert_eq!(
        buy_replay.kalman_basis.as_ref().unwrap().time_stop_triggers,
        1
    );
}

#[test]
fn peer_0259_candidate_kalman_deduplicates_without_change_at_equal_cutoffs() {
    let mut cfg = config();
    cfg.kalman_basis = Some(KalmanBasisConfig {
        ou_window: 32,
        ou_min_samples: 8,
        min_dislocation_bps: 3.0,
        max_dislocation_bps: 25.0,
        ..KalmanBasisConfig::default()
    });
    cfg.pair.planner.max_quote_age_ms = 5_000;
    cfg.pair.planner.max_quote_skew_ms = 5_000;
    cfg.pair.legs.iter_mut().for_each(|leg| leg.tick = d("0.0001"));
    let t0 = cfg.start_ms;
    let template = pair_book(&cfg, t0, 1000.5, 1000.0);
    let MakerFirstEvent::Book { books, .. } = template else {
        panic!("need book")
    };
    let mut replay = new_replay(&cfg).unwrap();
    replay.feed_online = true;
    replay.set_books(books.clone(), t0).unwrap();
    replay.observe_kalman_basis(t0, None, None, None);
    let initial = replay.kalman_basis.as_ref().unwrap().last_snapshot.unwrap();
    assert_eq!(initial.sample_count, 1);
    assert_eq!(initial.stationarity, StationarityStatus::WarmingUp);
    for i in 1..=12 {
        let receipt_ms = t0 + i * 100;
        replay.set_books(books.clone(), receipt_ms).unwrap();
        replay.observe_kalman_basis(receipt_ms, None, None, None);
    }
    let end = replay.kalman_basis.as_ref().unwrap().last_snapshot.unwrap();
    assert_eq!(end, initial);
    assert_eq!(end.stationarity, StationarityStatus::WarmingUp);
    assert_eq!(end.variance_p, initial.variance_p);

    let mut independent = new_replay(&cfg).unwrap();
    independent.feed_online = true;
    independent.set_books(books, t0).unwrap();
    independent.observe_kalman_basis(t0, None, None, None);
    for (offset, maker_mid) in [(1_300, 1002.0), (1_400, 1001.3)] {
        let at = t0 + offset;
        let MakerFirstEvent::Book { books, .. } = pair_book(&cfg, at, maker_mid, 1000.0) else {
            panic!("need book")
        };
        for run in [&mut replay, &mut independent] {
            run.set_books(books.clone(), at).unwrap();
            run.observe_kalman_basis(at, None, None, None);
        }
    }
    let padded = replay.kalman_basis.as_ref().unwrap();
    let genuine = independent.kalman_basis.as_ref().unwrap();
    let padded_snap = padded.last_snapshot.unwrap();
    let genuine_snap = genuine.last_snapshot.unwrap();
    assert_eq!(genuine_snap.sample_count, 3);
    assert_eq!(padded_snap, genuine_snap);
    assert_eq!(
        genuine.check_entry(HedgeSide::Sell),
        Err("kalman_basis_warmup_or_invalid")
    );
    assert_eq!(
        padded.check_entry(HedgeSide::Sell),
        Err("kalman_basis_warmup_or_invalid")
    );
}

#[test]
fn peer_0259_candidate_kalman_validates_each_leg_and_preserves_gap_boundary() {
    let mut cfg = config();
    cfg.kalman_basis = Some(KalmanBasisConfig::default());
    cfg.pair.planner.max_quote_age_ms = 500;
    cfg.pair.planner.max_quote_skew_ms = 1_000;
    cfg.pair.legs.iter_mut().for_each(|leg| leg.tick = d("0.0001"));
    let t0 = cfg.start_ms;
    let pair = |offset| {
        let MakerFirstEvent::Book { books, .. } = pair_book(&cfg, t0 + offset, 1000.2, 1000.0)
        else {
            panic!("fixture must be a book");
        };
        books
    };
    let apply = |r: &mut Replay, books, at| {
        r.set_books(books, t0 + at).unwrap();
        r.observe_kalman_basis(t0 + at, None, None, None);
        r.kalman_basis.as_ref().unwrap().last_snapshot
    };
    let mut r = new_replay(&cfg).unwrap();
    r.feed_online = true;
    let first = pair(0);
    let initial = apply(&mut r, first.clone(), 0).unwrap();

    let mut wrong_market = first.clone();
    wrong_market[1].market = "WRONG-MARKET".into();
    assert!(apply(&mut r, wrong_market, 1).is_none());
    assert!(apply(&mut r, first.clone(), 2).is_none());
    let fresh = pair(3);
    let second = apply(&mut r, fresh.clone(), 3).unwrap();
    assert_eq!(second.sample_count, initial.sample_count + 1);

    r.feed_online = false;
    assert!(apply(&mut r, fresh.clone(), 4).is_none());
    r.feed_online = true;
    assert!(apply(&mut r, fresh.clone(), 5).is_none());
    assert_eq!(
        apply(&mut r, pair(6), 6).unwrap().sample_count,
        second.sample_count + 1
    );

    let mut ambiguous = pair(6);
    ambiguous[0].bids[0].price -= d("0.01");
    assert!(apply(&mut r, ambiguous, 7).is_none());
    assert!(apply(&mut r, pair(6), 8).is_none());

    let mut out_of_order = pair(9);
    out_of_order[0].timestamp_ms = t0 + 5;
    assert!(apply(&mut r, out_of_order, 9).is_none());
    let restored = apply(&mut r, pair(10), 10).unwrap();
    assert_eq!(restored.sample_count, second.sample_count + 2);

    let mut slower_at_limit = pair(510);
    slower_at_limit[1].timestamp_ms = t0 + 10;
    let accepted = apply(&mut r, slower_at_limit.clone(), 510).unwrap();
    assert_eq!(accepted.sample_count, restored.sample_count + 1);
    slower_at_limit[0].timestamp_ms = t0 + 511;
    assert!(apply(&mut r, slower_at_limit, 511).is_none());
}

#[test]
fn peer_0259_candidate_new_session_waits_for_source_update_on_equal_stamps() {
    let mut cfg = config();
    cfg.kalman_basis = Some(KalmanBasisConfig::default());
    cfg.pair.asset_class = AssetClass::Equity;
    cfg.pair.legs.iter_mut().for_each(|leg| leg.tick = d("0.0001"));
    let t0 = cfg.start_ms;
    let MakerFirstEvent::Book { books: first, .. } = pair_book(&cfg, t0, 1000.2, 1000.0) else {
        panic!("fixture must be a book");
    };
    let mut replay = new_replay(&cfg).unwrap();
    replay.feed_online = true;
    replay.set_books(first.clone(), t0).unwrap();
    replay.observe_kalman_basis(t0, None, None, None);
    assert_eq!(
        replay
            .kalman_basis
            .as_ref()
            .unwrap()
            .last_snapshot
            .unwrap()
            .regime,
        SessionRegime::OffHours
    );
    let session = EquitySession {
        observed_at_ms: t0 + 1,
        opens_at_ms: t0 + 1,
        closes_at_ms: t0 + 30_000,
        clear_until_ms: t0 + 31_000,
        halted: false,
    };
    replay.set_books(first, t0 + 1).unwrap();
    replay.observe_kalman_basis(t0 + 1, None, Some(session), None);
    assert!(replay.kalman_basis.as_ref().unwrap().last_snapshot.is_none());
    let MakerFirstEvent::Book { books: next, .. } = pair_book(&cfg, t0 + 2, 1000.2, 1000.0) else {
        panic!("fixture must be a book");
    };
    replay.set_books(next, t0 + 2).unwrap();
    replay.observe_kalman_basis(t0 + 2, None, Some(session), None);
    let state = replay.kalman_basis.as_ref().unwrap();
    assert_eq!(state.last_snapshot.unwrap().regime, SessionRegime::UsRth);
    assert_eq!(state.last_snapshot.unwrap().sample_count, 1);
    assert_eq!(state.filter.regime_resets(), 1);
}

#[test]
fn slow_cascade_ignores_duplicate_source_books_and_fails_closed_on_same_ms_ambiguity() {
    let mut cfg = config();
    cfg.slow_cascade = Some(SlowCascadeConfig {
        bar_ms: 30_000,
        window_bars: 5,
        vol_lookback_bars: 8,
        exit_z: 0.7,
        warn_z: 1.8,
        halt_z: 3.0,
        min_hold_ms: 60_000,
        min_entry_scale: 0.25,
        max_bar_gap_ms: 120_000,
    });
    cfg.pair.planner.max_quote_age_ms = 200_000;
    cfg.pair.planner.max_quote_skew_ms = 200_000;
    cfg.pair.legs.iter_mut().for_each(|leg| leg.tick = d("0.0001"));

    let t0 = cfg.start_ms;
    let MakerFirstEvent::Book { books: first, .. } = pair_book(&cfg, t0, 1000.0, 1000.0) else {
        panic!("need book")
    };
    let mut r = new_replay(&cfg).unwrap();
    r.feed_online = true;
    r.set_books(first.clone(), t0).unwrap();
    assert_eq!(r.slow_cascade.as_ref().unwrap().guard.completed_bars(), 0);

    // Repeating identical source_ms across later bars cannot advance bars or inject cached prices.
    for bar in 1..=5_u64 {
        r.set_books(first.clone(), t0 + bar * 30_000).unwrap();
    }
    assert_eq!(r.slow_cascade.as_ref().unwrap().guard.completed_bars(), 0);

    // Offline drop clears last_decision; replaying cached `first` cannot restore it until fresh source_ms arrives.
    r.feed_online = false;
    r.set_books(first.clone(), t0 + 160_000).unwrap();
    assert!(r.slow_cascade.as_ref().unwrap().last_decision.is_none());
    r.feed_online = true;
    r.set_books(first.clone(), t0 + 160_100).unwrap();
    assert!(r.slow_cascade.as_ref().unwrap().last_decision.is_none());

    // Genuine new source_ms recovers observation cleanly; same-ms top-of-book ambiguity fails closed.
    let MakerFirstEvent::Book { books: second, .. } = pair_book(&cfg, t0 + 160_200, 1000.0, 999.5)
    else {
        panic!("need book")
    };
    r.set_books(second.clone(), t0 + 160_200).unwrap();
    assert!(r.slow_cascade.as_ref().unwrap().last_decision.is_some());
    let mut ambiguous = second;
    ambiguous[1].bids[0].price -= d("0.1");
    r.set_books(ambiguous, t0 + 160_300).unwrap();
    assert!(r.slow_cascade.as_ref().unwrap().last_decision.is_none());
}
