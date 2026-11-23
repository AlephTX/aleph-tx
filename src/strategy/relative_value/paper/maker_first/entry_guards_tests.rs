use super::*;

fn custom_pair_books(
    cfg: &MakerFirstConfig,
    at0: u64,
    at1: u64,
    m_bid: &str,
    m_ask: &str,
    h_bid: &str,
    h_ask: &str,
) -> MakerFirstEvent {
    let level = |price: &str, contracts: &str| Level {
        price: d(price),
        contracts: d(contracts),
    };
    MakerFirstEvent::Book {
        at_ms: at0.max(at1),
        books: [
            Book {
                market: cfg.pair.legs[0].market.clone(),
                timestamp_ms: at0,
                tradable: true,
                bids: vec![level(m_bid, "1")],
                asks: vec![level(m_ask, "1")],
                bounds: None,
                available_notional: Decimal::from(500),
            },
            Book {
                market: cfg.pair.legs[1].market.clone(),
                timestamp_ms: at1,
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
fn inventory_ladder_requires_the_net_edge_gate_on_replay_entry() {
    let registry = VenueRegistry::default();
    let mut cfg = config();
    cfg.require_net_edge = false;
    assert!(cfg.validate(&registry).is_ok());

    cfg.inventory_ladder = Some(InventoryLadderConfig {
        floor_frac: 0.25,
        scale_bps: d("4"),
        max_position_notional: d("200"),
        convex_power: 2.0,
    });
    let events = std::iter::once_with(|| -> Result<MakerFirstEvent> {
        panic!("invalid policy must fail before reading the tape");
    });
    let error = run_maker_first(&cfg, events, &registry)
        .err()
        .expect("ladder without an edge gate must be rejected");
    assert_eq!(
        error.to_string(),
        "inventory_ladder requires require_net_edge = true"
    );

    cfg.require_net_edge = true;
    assert!(cfg.validate(&registry).is_ok());
}

#[test]
fn entry_edge_persistence_requires_the_net_edge_gate_on_replay_entry() {
    let registry = VenueRegistry::default();
    let mut turnover_cfg = config();
    turnover_cfg.require_net_edge = true;
    turnover_cfg.pair.mode = PaperMode::Turnover {
        max_turnover_quote: d("1000"),
        max_fees_quote: d("10"),
        max_realized_loss_quote: None,
        incentive: None,
    };
    turnover_cfg.entry_edge_persistence = Some(EntryEdgePersistence {
        min_persist_ms: 200,
        max_gap_ms: 250,
    });
    assert!(turnover_cfg.validate(&registry).is_err());

    let mut cfg = config();
    cfg.require_net_edge = false;
    cfg.entry_edge_persistence = Some(EntryEdgePersistence {
        min_persist_ms: 200,
        max_gap_ms: 250,
    });
    let events = std::iter::once_with(|| -> Result<MakerFirstEvent> {
        panic!("invalid persistence policy must fail before reading the tape");
    });
    let error = run_maker_first(&cfg, events, &registry)
        .err()
        .expect("persistence without require_net_edge must be rejected");
    assert_eq!(
        error.to_string(),
        "entry_edge_persistence requires require_net_edge = true"
    );

    cfg.require_net_edge = true;
    assert!(cfg.validate(&registry).is_ok());
}

#[test]
fn inventory_ladder_rejects_turnover_enforces_convex_surcharge_and_zero_on_reduce() {
    let registry = VenueRegistry::default();

    // 1. Validation: strictly forbidden in Turnover mode; invalid bounds rejected.
    let mut turnover_cfg = config();
    turnover_cfg.pair.mode = PaperMode::Turnover {
        max_turnover_quote: d("1000"),
        max_fees_quote: d("10"),
        max_realized_loss_quote: None,
        incentive: None,
    };
    turnover_cfg.inventory_ladder = Some(InventoryLadderConfig {
        floor_frac: 0.25,
        scale_bps: d("4"),
        max_position_notional: d("200"),
        convex_power: 2.0,
    });
    assert!(turnover_cfg.validate(&registry).is_err());

    let mut bad_power = config();
    bad_power.require_net_edge = true;
    bad_power.inventory_ladder = Some(InventoryLadderConfig {
        floor_frac: 0.25,
        scale_bps: d("4"),
        max_position_notional: d("200"),
        convex_power: 0.5,
    });
    assert!(bad_power.validate(&registry).is_err());

    // 2. Unit curve checks: zero below floor or when reducing exposure; exact linear & convex points.
    let convex = InventoryLadderConfig {
        floor_frac: 0.25,
        scale_bps: d("4"),
        max_position_notional: d("200"),
        convex_power: 2.0,
    };
    assert_eq!(convex.extra_edge_bps(d("50"), true), Decimal::ZERO);
    assert_eq!(convex.extra_edge_bps(d("100"), false), Decimal::ZERO);
    // u = 0.5 -> norm = (0.5 - 0.25) / 0.75 = 1/3 -> factor = 1/9 -> 4/9 bps.
    let extra_100 = convex.extra_edge_bps(d("100"), true);
    assert!(
        (extra_100 - d("0.44444444")).abs() <= d("0.000001"),
        "extra_100={extra_100}"
    );
    assert_eq!(convex.extra_edge_bps(d("200"), true), d("4"));

    // 3. Replay check: min_edge <= edge < min_edge + extra blocked as `inventory_ladder_edge`,
    //    whereas edge >= min_edge + extra admits opening, and closing never pays surcharge.
    let mut cfg = config();
    cfg.maker_fee_bps = Decimal::ZERO;
    cfg.pair.legs[0].taker_fee_bps = Decimal::ZERO;
    cfg.pair.legs[1].taker_fee_bps = Decimal::ZERO;
    cfg.pair.legs.iter_mut().for_each(|l| l.tick = d("0.01"));
    cfg.pair.planner.max_base_quantity = 0.2;
    cfg.pair.planner.min_net_edge_bps = 1.0;
    cfg.require_net_edge = true;
    cfg.entry_side = Some(HedgeSide::Buy);
    cfg.inventory_ladder = Some(convex);

    let t0 = cfg.start_ms;
    let blocked_events = vec![
        MakerFirstEvent::TradeFeed {
            at_ms: t0,
            leg: 0,
            online: true,
        },
        // Buy 0.1 @ 1000.0, hedge sell @ 1000.30, reverse @ 1000.20, hedge_exit @ 1000.36
        // => paired_cash = +0.03 - 0.016 = +0.014 on 100.036 => ~1.399 bps (passes 1.0, fails 1.444).
        custom_pair_books(&cfg, t0, t0, "1000.00", "1000.20", "1000.30", "1000.36"),
        MakerFirstEvent::End { at_ms: cfg.end_ms },
    ];
    let rep_blocked = run_maker_first(&cfg, blocked_events.into_iter().map(Ok), &registry).unwrap();
    assert_eq!(rep_blocked.quotes_submitted, 0);
    assert_eq!(
        rep_blocked.entry_blocked.get("inventory_ladder_edge"),
        Some(&1)
    );
    let lad_blocked = rep_blocked.inventory_ladder.unwrap();
    assert_eq!(lad_blocked.evaluations, 1);
    assert_eq!(lad_blocked.blocked_entries, 1);

    let open_events = vec![
        MakerFirstEvent::TradeFeed {
            at_ms: t0,
            leg: 0,
            online: true,
        },
        // Widening hedge_bid to 1000.32 gives ~1.599 bps >= 1.444 bps -> admitted.
        custom_pair_books(&cfg, t0, t0, "1000.00", "1000.20", "1000.32", "1000.36"),
        MakerFirstEvent::End { at_ms: cfg.end_ms },
    ];
    let rep_open = run_maker_first(&cfg, open_events.into_iter().map(Ok), &registry).unwrap();
    assert_eq!(rep_open.quotes_submitted, 1);
    assert_eq!(rep_open.entry_blocked.get("inventory_ladder_edge"), None);
    let lad_open = rep_open.inventory_ladder.unwrap();
    assert_eq!(lad_open.blocked_entries, 0);
    assert!(lad_open.max_extra_edge_bps > Decimal::ZERO);
}

#[test]
fn entry_edge_persistence_requires_sustained_causal_edge_and_resets_on_dip_or_gap() {
    let registry = VenueRegistry::default();
    let mut cfg = config();
    cfg.maker_fee_bps = Decimal::ZERO;
    cfg.pair.legs[0].taker_fee_bps = Decimal::ZERO;
    cfg.pair.legs[1].taker_fee_bps = Decimal::ZERO;
    cfg.pair.legs.iter_mut().for_each(|l| l.tick = d("0.01"));
    cfg.pair.planner.max_base_quantity = 0.2;
    cfg.pair.planner.min_net_edge_bps = 1.0;
    cfg.pair.planner.max_quote_age_ms = 500;
    cfg.reprice_ms = 50;
    cfg.require_net_edge = true;
    cfg.entry_side = Some(HedgeSide::Buy);
    cfg.entry_edge_persistence = Some(EntryEdgePersistence {
        min_persist_ms: 200,
        max_gap_ms: 250,
    });

    let t0 = cfg.start_ms;
    let good = |at: u64| custom_pair_books(&cfg, at, at, "1000.00", "1000.20", "1000.32", "1000.36");
    let bad = |at: u64| custom_pair_books(&cfg, at, at, "1000.00", "1000.20", "1000.10", "1000.36");

    // 1. Dip resets persistence clock cleanly before opening quote can post.
    let dip_events = vec![
        MakerFirstEvent::TradeFeed {
            at_ms: t0,
            leg: 0,
            online: true,
        },
        good(t0),
        good(t0 + 100), // elapsed = 100 < 200 -> blocked ("entry_edge_persist")
        bad(t0 + 150),  // dip -> blocked ("entry_net_edge"), resets timer
        good(t0 + 200), // restarts clock at t0+200
        good(t0 + 300), // elapsed = 100 < 200 -> blocked
        good(t0 + 400), // elapsed = 200 >= 200 -> posts!
        MakerFirstEvent::End { at_ms: cfg.end_ms },
    ];
    let rep_dip = run_maker_first(&cfg, dip_events.into_iter().map(Ok), &registry).unwrap();
    assert_eq!(rep_dip.quotes_submitted, 1, "{rep_dip:?}");
    assert_eq!(rep_dip.entry_blocked.get("entry_edge_persist"), Some(&4));
    assert_eq!(rep_dip.entry_blocked.get("entry_net_edge"), Some(&1));
    let persist = rep_dip.entry_edge_persistence.unwrap();
    assert_eq!(persist.blocked_entries, 4);
    assert!(persist.resets >= 1);

    // 2. Gap exceeding max_gap_ms (400 > 250) resets persistence timer; requires another 200 ms.
    let gap_events = vec![
        MakerFirstEvent::TradeFeed {
            at_ms: t0,
            leg: 0,
            online: true,
        },
        good(t0),
        good(t0 + 100), // still < 200
        good(t0 + 500), // 500 - 100 = 400 > 250 -> resets clock to 500
        good(t0 + 600), // 100 < 200 -> blocked
        good(t0 + 700), // 200 >= 200 -> posts!
        MakerFirstEvent::End { at_ms: cfg.end_ms },
    ];
    let rep_gap = run_maker_first(&cfg, gap_events.into_iter().map(Ok), &registry).unwrap();
    assert_eq!(rep_gap.quotes_submitted, 1);
    assert_eq!(rep_gap.entry_blocked.get("entry_edge_persist"), Some(&4));
}

#[test]
fn entry_edge_persistence_resets_on_throttled_dip_without_bypassing_reprice_or_hiding_gaps() {
    let registry = VenueRegistry::default();
    let mut cfg = config();
    cfg.maker_fee_bps = Decimal::ZERO;
    cfg.pair.legs.iter_mut().for_each(|l| {
        l.taker_fee_bps = Decimal::ZERO;
        l.tick = d("0.01");
    });
    cfg.pair.planner.max_base_quantity = 0.2;
    cfg.pair.planner.min_net_edge_bps = 1.0;
    cfg.pair.planner.max_quote_age_ms = 500;
    cfg.reprice_ms = 100;
    cfg.require_net_edge = true;
    cfg.entry_side = Some(HedgeSide::Buy);
    cfg.entry_edge_persistence = Some(EntryEdgePersistence {
        min_persist_ms: 200,
        max_gap_ms: 250,
    });

    let t0 = cfg.start_ms;
    let book = |cfg: &MakerFirstConfig, offset: u64, hedge_bid: &str| {
        custom_pair_books(
            cfg,
            t0 + offset,
            t0 + offset,
            "1000.00",
            "1000.20",
            hedge_bid,
            "1000.36",
        )
    };

    // 1. +150 ms dip arrives between reprice_ms=100 checks (reproduces `#367` finding):
    //    resets persistence clock without altering reprice cadence or inflating entry_checks.
    let mid_dip = vec![
        MakerFirstEvent::TradeFeed {
            at_ms: t0,
            leg: 0,
            online: true,
        },
        book(&cfg, 0, "1000.32"),
        book(&cfg, 100, "1000.32"),
        book(&cfg, 150, "1000.10"), // throttled dip (< next_quote_ms=200)
        book(&cfg, 200, "1000.32"), // must restart clock at 200, NOT quote!
        MakerFirstEvent::End { at_ms: cfg.end_ms },
    ];
    let rep_mid = run_maker_first(&cfg, mid_dip.into_iter().map(Ok), &registry).unwrap();
    assert_eq!(rep_mid.synchronized_book_frames, 4);
    assert_eq!(rep_mid.quotes_submitted, 0);
    assert_eq!(rep_mid.entry_checks, 4); // TradeFeed(t0) + Books at 0/100/200; throttled 150 ms excluded
    assert_eq!(rep_mid.entry_blocked.get("entry_edge_persist"), Some(&3));
    assert_eq!(rep_mid.entry_blocked.get("entry_net_edge"), None);
    assert_eq!(rep_mid.entry_edge_persistence.unwrap().resets, 1);

    // Continuing with sustained good books at 300/400 ms posts at 400 ms (reprice cadence preserved).
    let recovered = vec![
        MakerFirstEvent::TradeFeed {
            at_ms: t0,
            leg: 0,
            online: true,
        },
        book(&cfg, 0, "1000.32"),
        book(&cfg, 100, "1000.32"),
        book(&cfg, 150, "1000.10"),
        book(&cfg, 200, "1000.32"),
        book(&cfg, 300, "1000.32"),
        book(&cfg, 400, "1000.32"),
        MakerFirstEvent::End { at_ms: cfg.end_ms },
    ];
    let rep_rec = run_maker_first(&cfg, recovered.into_iter().map(Ok), &registry).unwrap();
    assert_eq!(rep_rec.quotes_submitted, 1);
    assert_eq!(rep_rec.entry_edge_persistence.unwrap().resets, 1);

    // 2. Inventory ladder + persistence combined: a throttled dip below min_edge + ladder_extra
    //    (even though above raw min_edge) also resets persistence at +150 ms.
    cfg.inventory_ladder = Some(InventoryLadderConfig {
        floor_frac: 0.25,
        scale_bps: d("4"),
        max_position_notional: d("200"),
        convex_power: 2.0,
    });
    let ladder_dip = vec![
        MakerFirstEvent::TradeFeed {
            at_ms: t0,
            leg: 0,
            online: true,
        },
        book(&cfg, 0, "1000.32"),   // ~1.599 bps >= 1.444 bps
        book(&cfg, 100, "1000.32"), // ~1.599 bps >= 1.444 bps
        book(&cfg, 150, "1000.30"), // ~1.399 bps: >= 1.0 raw min_edge, < 1.444 ladder threshold
        book(&cfg, 200, "1000.32"), // restarted at 200 -> blocked
        MakerFirstEvent::End { at_ms: cfg.end_ms },
    ];
    let rep_lad = run_maker_first(&cfg, ladder_dip.into_iter().map(Ok), &registry).unwrap();
    assert_eq!(rep_lad.quotes_submitted, 0);
    assert_eq!(rep_lad.entry_edge_persistence.unwrap().resets, 1);
    assert_eq!(rep_lad.inventory_ladder.unwrap().evaluations, 3);
    cfg.inventory_ladder = None;

    // 3. Valid intermediate Book arrivals keep persistence alive when max_gap_ms < reprice_ms,
    //    without charging extra blocked_entries on throttled frames.
    cfg.entry_edge_persistence = Some(EntryEdgePersistence {
        min_persist_ms: 200,
        max_gap_ms: 80,
    });
    let smooth_books = vec![
        MakerFirstEvent::TradeFeed {
            at_ms: t0,
            leg: 0,
            online: true,
        },
        book(&cfg, 0, "1000.32"),
        book(&cfg, 50, "1000.32"),  // throttled, keeps gap <= 50 <= 80
        book(&cfg, 100, "1000.32"), // 100 - 50 <= 80
        book(&cfg, 150, "1000.32"), // throttled, keeps gap <= 50 <= 80
        book(&cfg, 200, "1000.32"), // 200 - 150 <= 80 -> submits!
        MakerFirstEvent::End { at_ms: cfg.end_ms },
    ];
    let rep_smooth = run_maker_first(&cfg, smooth_books.into_iter().map(Ok), &registry).unwrap();
    assert_eq!(rep_smooth.quotes_submitted, 1);
    let smooth_p = rep_smooth.entry_edge_persistence.unwrap();
    assert_eq!(smooth_p.resets, 0);
    assert_eq!(smooth_p.blocked_entries, 2);

    // 4. Non-book events (e.g., Trade at 150 ms) cannot mask an actual book gap > max_gap_ms.
    cfg.entry_edge_persistence = Some(EntryEdgePersistence {
        min_persist_ms: 200,
        max_gap_ms: 150,
    });
    let trade_gap = vec![
        MakerFirstEvent::TradeFeed {
            at_ms: t0,
            leg: 0,
            online: true,
        },
        book(&cfg, 0, "1000.32"),
        book(&cfg, 100, "1000.32"),
        MakerFirstEvent::Trade {
            at_ms: t0 + 150,
            source_ms: t0 + 150,
            leg: 0,
            aggressor: HedgeSide::Sell,
            price: d("1000.10"),
            contracts: d("0.1"),
        },
        book(&cfg, 300, "1000.32"), // book gap = 300 - 100 = 200 > 150 -> resets!
        MakerFirstEvent::End { at_ms: cfg.end_ms },
    ];
    let rep_tgap = run_maker_first(&cfg, trade_gap.into_iter().map(Ok), &registry).unwrap();
    assert_eq!(rep_tgap.quotes_submitted, 0);
    assert_eq!(rep_tgap.entry_edge_persistence.unwrap().resets, 1);
}

#[test]
fn entry_edge_persistence_rejects_duplicate_source_replay_same_ms_conflict_and_passive_wait() {
    let registry = VenueRegistry::default();
    let mut cfg = config();
    cfg.maker_fee_bps = Decimal::ZERO;
    cfg.pair.legs.iter_mut().for_each(|l| {
        l.taker_fee_bps = Decimal::ZERO;
        l.tick = d("0.01");
    });
    cfg.pair.planner.max_base_quantity = 0.2;
    cfg.pair.planner.min_net_edge_bps = 1.0;
    cfg.pair.planner.max_quote_age_ms = 500;
    cfg.reprice_ms = 100;
    cfg.require_net_edge = true;
    cfg.entry_side = Some(HedgeSide::Buy);
    cfg.entry_edge_persistence = Some(EntryEdgePersistence {
        min_persist_ms: 200,
        max_gap_ms: 150,
    });

    let t0 = cfg.start_ms;
    let book = |c: &MakerFirstConfig, at_ms: u64, src_ms: u64, hedge_bid: &str| {
        let mut ev = custom_pair_books(
            c,
            t0 + src_ms,
            t0 + src_ms,
            "1000.00",
            "1000.20",
            hedge_bid,
            "1000.36",
        );
        if let MakerFirstEvent::Book { at_ms: ref mut ev_at, .. } = ev {
            *ev_at = t0 + at_ms;
        }
        ev
    };

    // 1. Reproduces all 3 `#370` `persist-clock-probe.rs` scenarios:
    //    neither an unmatched Trade nor a duplicate same-source Book replay can bridge the 200 ms gap.
    for case in ["no_mid_event", "unmatched_trade", "same_pair_new_receipt"] {
        let mut rows = vec![
            MakerFirstEvent::TradeFeed {
                at_ms: t0,
                leg: 0,
                online: true,
            },
            book(&cfg, 0, 0, "1000.32"),
        ];
        match case {
            "unmatched_trade" => rows.push(MakerFirstEvent::Trade {
                at_ms: t0 + 100,
                source_ms: t0 + 100,
                leg: 0,
                aggressor: HedgeSide::Buy,
                price: d("1000.00"),
                contracts: d("0.1"),
            }),
            "same_pair_new_receipt" => rows.push(book(&cfg, 100, 0, "1000.32")),
            _ => {}
        }
        rows.extend([
            book(&cfg, 200, 200, "1000.32"),
            MakerFirstEvent::End { at_ms: cfg.end_ms },
        ]);
        let rep = run_maker_first(&cfg, rows.into_iter().map(Ok), &registry).unwrap();
        assert_eq!(rep.maker_fills, 0, "case={case}");
        assert_eq!(rep.quotes_submitted, 0, "case={case}");
        assert_eq!(rep.entry_edge_persistence.unwrap().resets, 1, "case={case}");
    }

    // Subsequent continuous new source updates recover admission cleanly.
    let recovered = vec![
        MakerFirstEvent::TradeFeed {
            at_ms: t0,
            leg: 0,
            online: true,
        },
        book(&cfg, 0, 0, "1000.32"),
        book(&cfg, 100, 0, "1000.32"), // duplicate cached replay
        book(&cfg, 200, 200, "1000.32"),
        book(&cfg, 300, 300, "1000.32"),
        book(&cfg, 400, 400, "1000.32"),
        MakerFirstEvent::End { at_ms: cfg.end_ms },
    ];
    let rep_rec = run_maker_first(&cfg, recovered.into_iter().map(Ok), &registry).unwrap();
    assert_eq!(rep_rec.quotes_submitted, 1);
    assert_eq!(rep_rec.entry_edge_persistence.unwrap().resets, 1);

    // 2. Exact boundary: 150 ms persists, 151 ms resets (min_persist_ms=150, max_gap_ms=150).
    cfg.entry_edge_persistence = Some(EntryEdgePersistence {
        min_persist_ms: 150,
        max_gap_ms: 150,
    });
    let at_150 = vec![
        MakerFirstEvent::TradeFeed {
            at_ms: t0,
            leg: 0,
            online: true,
        },
        book(&cfg, 0, 0, "1000.32"),
        book(&cfg, 150, 150, "1000.32"),
        MakerFirstEvent::End { at_ms: cfg.end_ms },
    ];
    let rep_150 = run_maker_first(&cfg, at_150.into_iter().map(Ok), &registry).unwrap();
    assert_eq!(rep_150.quotes_submitted, 1);
    assert_eq!(rep_150.entry_edge_persistence.unwrap().resets, 0);

    let at_151 = vec![
        MakerFirstEvent::TradeFeed {
            at_ms: t0,
            leg: 0,
            online: true,
        },
        book(&cfg, 0, 0, "1000.32"),
        book(&cfg, 151, 151, "1000.32"),
        MakerFirstEvent::End { at_ms: cfg.end_ms },
    ];
    let rep_151 = run_maker_first(&cfg, at_151.into_iter().map(Ok), &registry).unwrap();
    assert_eq!(rep_151.quotes_submitted, 0);
    assert_eq!(rep_151.entry_edge_persistence.unwrap().resets, 1);

    // 3. Passive waiting / duplicate book replay cannot reach min_persist_ms without a new source book.
    cfg.entry_edge_persistence = Some(EntryEdgePersistence {
        min_persist_ms: 200,
        max_gap_ms: 250,
    });
    let passive_wait = vec![
        MakerFirstEvent::TradeFeed {
            at_ms: t0,
            leg: 0,
            online: true,
        },
        book(&cfg, 0, 0, "1000.32"),
        book(&cfg, 100, 100, "1000.32"),
        book(&cfg, 200, 100, "1000.32"), // duplicate source=100 at receipt=200 cannot reach 200ms
        MakerFirstEvent::End { at_ms: cfg.end_ms },
    ];
    let rep_wait = run_maker_first(&cfg, passive_wait.into_iter().map(Ok), &registry).unwrap();
    assert_eq!(rep_wait.quotes_submitted, 0);

    // 4. Same-source_ms conflicting snapshot invalidates immediately and stays invalidated on replay.
    let same_ms_conflict = vec![
        MakerFirstEvent::TradeFeed {
            at_ms: t0,
            leg: 0,
            online: true,
        },
        book(&cfg, 0, 0, "1000.32"),
        book(&cfg, 50, 0, "1000.33"),   // same source_ms=0, conflicting book content
        book(&cfg, 100, 0, "1000.32"),  // replay original at source_ms=0 cannot clear conflict
        book(&cfg, 200, 0, "1000.32"),  // still locked out from reaching persistence on stale conflict
        MakerFirstEvent::End { at_ms: cfg.end_ms },
    ];
    let rep_conflict =
        run_maker_first(&cfg, same_ms_conflict.into_iter().map(Ok), &registry).unwrap();
    assert_eq!(rep_conflict.quotes_submitted, 0);
    assert_eq!(rep_conflict.entry_edge_persistence.unwrap().resets, 1);
}

#[test]
fn post_fill_book_refresh_locks_opening_until_any_or_both_legs_advance_without_blocking_closes() {
    let make_cfg = |mode: PostFillRefreshMode| {
        let mut cfg = config();
        cfg.maker_fee_bps = Decimal::ZERO;
        cfg.pair.legs[0].taker_fee_bps = Decimal::ZERO;
        cfg.pair.legs[1].taker_fee_bps = Decimal::ZERO;
        cfg.pair.legs.iter_mut().for_each(|l| l.tick = d("0.01"));
        cfg.pair.planner.max_quote_age_ms = 5_000;
        cfg.pair.planner.max_quote_skew_ms = 5_000;
        cfg.entry_side = Some(HedgeSide::Buy);
        cfg.post_fill_book_refresh = Some(mode);
        cfg
    };

    // 1. BothLegs: single-leg book refresh keeps opening locked; advancing both legs unlocks.
    // Closing quote while locked is NEVER blocked.
    let both_cfg = make_cfg(PostFillRefreshMode::BothLegs);
    let mut r = new_replay(&both_cfg).unwrap();
    r.feed_online = true;
    let t0 = both_cfg.start_ms;
    if let MakerFirstEvent::Book { books, .. } =
        custom_pair_books(&both_cfg, t0, t0, "1000.00", "1000.20", "1000.20", "1000.40")
    {
        r.set_books(books, t0).unwrap();
    }
    r.post_fill_refresh.arm(r.last_source_ms);
    assert!(r.post_fill_refresh.is_locked());

    // Opening attempt before any book refresh -> blocked by post_fill_book_refresh.
    r.next_quote_ms = t0;
    r.quote(t0).unwrap();
    assert_eq!(r.report.quotes_submitted, 0);
    assert_eq!(
        r.report.entry_blocked.get("post_fill_book_refresh"),
        Some(&1)
    );

    // Refresh ONLY leg 0 (`t0 + 100`, `t0`) -> BothLegs remains locked!
    if let MakerFirstEvent::Book { books, .. } =
        custom_pair_books(&both_cfg, t0 + 100, t0, "1000.00", "1000.20", "1000.20", "1000.40")
    {
        r.set_books(books, t0 + 100).unwrap();
    }
    assert!(r.post_fill_refresh.is_locked());
    r.next_quote_ms = t0 + 100;
    r.quote(t0 + 100).unwrap();
    assert_eq!(
        r.report.entry_blocked.get("post_fill_book_refresh"),
        Some(&2)
    );

    // While locked under BothLegs, a CLOSING quote must NEVER be blocked.
    r.ledger.positions = [d("0.1"), d("-0.1")];
    r.cycle = Some(Cycle {
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
        paired_at_ms: Some(t0),
        hedge_fill_at_ms: Some(t0),
        paired_basis_bps: Some(Decimal::ZERO),
        paired_mids: [Some(d("1000.1")), Some(d("1000.3"))],
        closing_maker_filled: false,
        forced_ioc_filled: false,
        sub_min_top_up_attempts: 0,
        sub_min_recovery_pending_base: Decimal::ZERO,
        used_partner_step_deadband: false,
        cash_exit_triggered: false,
    });
    r.next_quote_ms = t0 + 100;
    r.quote(t0 + 100).unwrap();
    assert_eq!(r.report.quotes_submitted, 1);
    assert!(r.quote.as_ref().is_some_and(|q| q.closing));

    // Reset flat and advance leg 1 too (`t0 + 100`, `t0 + 200`) -> lock clears and opening posts!
    r.quote = None;
    r.cycle = None;
    r.ledger.positions = [Decimal::ZERO; 2];
    if let MakerFirstEvent::Book { books, .. } = custom_pair_books(
        &both_cfg,
        t0 + 100,
        t0 + 200,
        "1000.00",
        "1000.20",
        "1000.20",
        "1000.40",
    ) {
        r.set_books(books, t0 + 200).unwrap();
    }
    assert!(!r.post_fill_refresh.is_locked());
    r.next_quote_ms = t0 + 200;
    r.quote(t0 + 200).unwrap();
    assert_eq!(r.report.quotes_submitted, 2);

    // 2. AnyLeg end-to-end replay: partial fill arms lock; single-leg book refresh unlocks later opening.
    let any_cfg = make_cfg(PostFillRefreshMode::AnyLeg);
    let mut r_any = new_replay(&any_cfg).unwrap();
    r_any.feed_online = true;
    if let MakerFirstEvent::Book { books, .. } =
        custom_pair_books(&any_cfg, t0, t0, "1000.00", "1000.20", "1000.20", "1000.40")
    {
        r_any.set_books(books, t0).unwrap();
    }
    r_any.post_fill_refresh.arm(r_any.last_source_ms);
    assert!(r_any.post_fill_refresh.is_locked());
    if let MakerFirstEvent::Book { books, .. } =
        custom_pair_books(&any_cfg, t0 + 100, t0, "1000.00", "1000.20", "1000.20", "1000.40")
    {
        r_any.set_books(books, t0 + 100).unwrap();
    }
    assert!(!r_any.post_fill_refresh.is_locked());
    let any_rep = r_any.post_fill_refresh.report(PostFillRefreshMode::AnyLeg);
    assert_eq!(any_rep.locks_armed, 1);
    assert_eq!(any_rep.locks_cleared, 1);
}

#[test]
fn entry_edge_persistence_per_leg_conflict_latch_blocks_until_conflicting_leg_advances() {
    let mut cfg = config();
    let registry = VenueRegistry::default();
    cfg.maker_fee_bps = Decimal::ZERO;
    cfg.pair.legs.iter_mut().for_each(|leg| {
        leg.tick = d("0.01");
        leg.taker_fee_bps = Decimal::ZERO;
    });
    cfg.pair.planner.max_base_quantity = 0.2;
    cfg.pair.planner.min_net_edge_bps = 1.0;
    cfg.pair.planner.max_quote_age_ms = 500;
    cfg.pair.planner.max_quote_skew_ms = 500;
    cfg.entry_side = Some(HedgeSide::Buy);
    cfg.require_net_edge = true;
    cfg.reprice_ms = 100;
    cfg.entry_edge_persistence = Some(EntryEdgePersistence {
        min_persist_ms: 200,
        max_gap_ms: 150,
    });
    let t0 = cfg.start_ms;
    let at = |receipt: u64, fast_source: u64, slow_source: u64, hedge_bid: &str| {
        let mut event = custom_pair_books(
            &cfg,
            t0 + fast_source,
            t0 + slow_source,
            "1000.00",
            "1000.20",
            hedge_bid,
            "1000.36",
        );
        let MakerFirstEvent::Book { at_ms, .. } = &mut event else {
            panic!("fixture must produce books");
        };
        *at_ms = t0 + receipt;
        event
    };

    for (
        case,
        first_fast,
        first_slow_bid,
        fast_advances,
        slow_recovers_ms,
        later_slow_bid,
        expected_quotes,
    ) in [
        ("no_source_changes", 0, "1000.33", false, None, "1000.33", 0),
        ("only_fast_source_advances", 0, "1000.33", true, None, "1000.33", 0),
        ("slow_source_replaces_conflict", 0, "1000.33", true, Some(110), "1000.33", 1),
        ("asynchronous_frame_reveals_slow_conflict", 50, "1000.33", true, None, "1000.33", 0),
        ("older_slow_contents_repeated_without_source_advance", 0, "1000.33", true, None, "1000.32", 0),
        ("regressing_slow_timestamp_cannot_clear_conflict", 0, "1000.33", true, Some(5), "1000.32", 0),
        ("valid_slow_snapshot_can_repeat", 50, "1000.32", true, None, "1000.32", 1),
    ] {
        let mut events = vec![
            MakerFirstEvent::TradeFeed {
                at_ms: t0,
                leg: 0,
                online: true,
            },
            at(10, 10, 10, "1000.32"),
            at(60, 10 + first_fast, 10, first_slow_bid),
        ];
        for offset in [110_u64, 210, 310, 460] {
            let fast = if fast_advances { offset } else { 10 };
            let slow = slow_recovers_ms.unwrap_or(10);
            events.push(at(offset, fast, slow, later_slow_bid));
        }
        events.push(MakerFirstEvent::End { at_ms: cfg.end_ms });
        let report = run_maker_first(&cfg, events.into_iter().map(Ok), &registry).unwrap();
        assert_eq!(
            report.quotes_submitted, expected_quotes,
            "unexpected quotes_submitted for case={case}"
        );
        assert!(
            report.entry_edge_persistence.is_some(),
            "persistence summary missing for case={case}"
        );
    }
}

#[test]
fn entry_edge_persistence_latches_slow_leg_conflict_while_fast_leg_nontradable() {
    let mut cfg = config();
    let registry = VenueRegistry::default();
    cfg.maker_fee_bps = Decimal::ZERO;
    cfg.pair.legs.iter_mut().for_each(|leg| {
        leg.tick = d("0.01");
        leg.taker_fee_bps = Decimal::ZERO;
    });
    cfg.pair.planner.max_base_quantity = 0.2;
    cfg.pair.planner.min_net_edge_bps = 1.0;
    cfg.pair.planner.max_quote_age_ms = 500;
    cfg.pair.planner.max_quote_skew_ms = 500;
    cfg.entry_side = Some(HedgeSide::Buy);
    cfg.require_net_edge = true;
    cfg.reprice_ms = 100;
    cfg.entry_edge_persistence = Some(EntryEdgePersistence {
        min_persist_ms: 200,
        max_gap_ms: 150,
    });
    let t0 = cfg.start_ms;
    let pair = |receipt, fast_source, slow_source, hedge_bid, fast_tradable| {
        let mut event = custom_pair_books(
            &cfg,
            t0 + fast_source,
            t0 + slow_source,
            "1000.00",
            "1000.20",
            hedge_bid,
            "1000.36",
        );
        let MakerFirstEvent::Book { at_ms, books, .. } = &mut event else {
            panic!("fixture must produce books");
        };
        *at_ms = t0 + receipt;
        books[0].tradable = fast_tradable;
        event
    };
    for (case, fast_tradable_at_conflict, new_slow_after_conflict, quotes) in [
        ("both_legs_tradable_when_conflict_seen", true, false, 0),
        ("fast_unavailable_when_slow_conflict_seen", false, false, 0),
        ("fast_unavailable_then_slow_genuinely_refreshes", false, true, 1),
    ] {
        let mut events = vec![
            MakerFirstEvent::TradeFeed {
                at_ms: t0,
                leg: 0,
                online: true,
            },
            pair(0, 0, 0, "1000.32", true),
            pair(50, 0, 0, "1000.33", fast_tradable_at_conflict),
        ];
        for offset in [100, 200, 300, 450] {
            let slow = if new_slow_after_conflict { 100 } else { 0 };
            events.push(pair(offset, offset, slow, "1000.32", true));
        }
        events.push(MakerFirstEvent::End { at_ms: cfg.end_ms });
        let result = run_maker_first(&cfg, events.into_iter().map(Ok), &registry).unwrap();
        assert!(
            result.entry_edge_persistence.unwrap().resets >= 1,
            "expected reset for case={case}"
        );
        assert_eq!(result.quotes_submitted, quotes, "case={case}");
        assert_eq!(result.quotes_accepted, quotes, "case={case}");
    }
}

#[test]
fn entry_edge_persistence_ignores_incomplete_regressed_or_future_leg_evidence() {
    use super::super::entry_guards::{EntryEdgePersistState, fingerprint_leg_book};

    let cfg = config();
    let t0 = cfg.start_ms;
    let pair = |fast: u64, slow: u64, hedge_bid: &str| {
        let MakerFirstEvent::Book { books, .. } = custom_pair_books(
            &cfg,
            t0 + fast,
            t0 + slow,
            "1000.00",
            "1000.20",
            hedge_bid,
            "1000.36",
        ) else {
            panic!("fixture must produce books");
        };
        books
    };
    let fingerprints = |books: [Book; 2]| books.each_ref().map(|book| Some(fingerprint_leg_book(book)));
    let mut state = EntryEdgePersistState::default();
    assert_eq!(
        state.classify_fingerprints(fingerprints(pair(10, 10, "1000.32")), false),
        BookEvidence::None,
        "an offline or skewed pair can track leg identity without producing quote evidence",
    );
    let incomplete = [None, Some(fingerprint_leg_book(&pair(10, 10, "1000.33")[1]))];
    assert_eq!(
        state.classify_fingerprints(incomplete, true),
        BookEvidence::None
    );
    assert_eq!(
        state.classify_fingerprints(fingerprints(pair(20, 5, "1000.32")), true),
        BookEvidence::None,
        "source regression cannot clear the conflict or count as evidence",
    );
    assert_eq!(
        state.classify_fingerprints(fingerprints(pair(30, 10, "1000.32")), true),
        BookEvidence::None,
        "returning to the original contents cannot clear the conflict",
    );
    let refreshed = fingerprints(pair(40, 20, "1000.32"));
    assert_eq!(
        state.classify_fingerprints(refreshed, false),
        BookEvidence::None
    );
    assert_eq!(
        state.classify_fingerprints(refreshed, true),
        BookEvidence::New
    );
    assert_eq!(
        state.classify_fingerprints(refreshed, true),
        BookEvidence::Duplicate
    );

    let mut cfg = config();
    cfg.require_net_edge = true;
    cfg.entry_edge_persistence = Some(EntryEdgePersistence {
        min_persist_ms: 200,
        max_gap_ms: 150,
    });
    let registry = VenueRegistry::default();
    let mut future = custom_pair_books(
        &cfg,
        t0 + 50,
        t0 + 51,
        "1000.00",
        "1000.20",
        "1000.33",
        "1000.36",
    );
    let MakerFirstEvent::Book { at_ms, books, .. } = &mut future else {
        panic!("fixture must produce books");
    };
    *at_ms = t0 + 50;
    books[0].tradable = false;
    let error = run_maker_first(&cfg, [Ok(future)], &registry).unwrap_err();
    assert!(error.to_string().contains("book is from the future"));
}
