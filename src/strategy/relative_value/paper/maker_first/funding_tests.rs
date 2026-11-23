use super::*;

const HOUR: u64 = 3_600_000;

fn funding_config() -> MakerFirstConfig {
    let mut cfg = config();
    cfg.start_ms = HOUR - 30_000;
    cfg.end_ms = HOUR + 30_000;
    cfg.flat_at_funding = Some(FlatAtFunding {
        settlement_guard_ms: 1_000,
    });
    cfg
}

fn run(cfg: &MakerFirstConfig, mut rows: Vec<MakerFirstEvent>) -> MakerFirstReport {
    rows.push(MakerFirstEvent::End { at_ms: cfg.end_ms });
    rows.sort_by_key(MakerFirstEvent::time);
    run_maker_first(cfg, rows.into_iter().map(Ok), &VenueRegistry::default()).unwrap()
}

fn cycle(at: u64) -> Cycle {
    Cycle {
        id: 0,
        posture: TurnoverPosture::Neutral,
        started_ms: at,
        initial_cash: Decimal::ZERO,
        initial_turnover: Decimal::ZERO,
        initial_fees: Decimal::ZERO,
        draining: false,
        externalizing: false,
        maker_turnover: Decimal::ZERO,
        passive_side: None,
        entry_ref_mid: None,
        paired_at_ms: None,
        hedge_fill_at_ms: None,
        paired_basis_bps: None,
        paired_mids: [None, None],
        closing_maker_filled: false,
        forced_ioc_filled: false,
        sub_min_top_up_attempts: 0,
        sub_min_recovery_pending_base: Decimal::ZERO,
        used_partner_step_deadband: false,
        cash_exit_triggered: false,
    }
}

#[test]
fn ten_hours_use_every_timer_with_sparse_tape_and_different_leg_schedules() {
    let mut cfg = funding_config();
    cfg.end_ms = cfg.start_ms + 10 * HOUR;
    cfg.pair.legs[1].funding.interval_ms = 8 * HOUR;
    let r = run(&cfg, vec![books(&cfg, cfg.start_ms, "1000")]);
    assert_eq!(r.incomplete, None);
    assert_eq!(r.net_pnl, Some(Decimal::ZERO));
    let f = r.funding_flat.unwrap();
    assert_eq!(f.planned_slots_by_leg, [10, 1]);
    assert_eq!(f.verified_slots_by_leg, [10, 1]);
    assert_eq!(f.boundary_checks, 20);
    assert_eq!(f.pause_windows, 10);
    assert_eq!(f.planned_pause_ms, 10 * 6_001);
}

#[test]
fn configuration_requires_cancel_delivery_close_and_clock_budgets() {
    let mut cfg = funding_config();
    cfg.trade_clock_guard_ms = 1_000;
    assert!(cfg.validate(&VenueRegistry::default()).is_err());
    cfg.trade_clock_guard_ms = 0;
    cfg.trade_delivery_grace_ms = 5_000;
    assert!(cfg.validate(&VenueRegistry::default()).is_err());
    cfg.drain_ms = 10_000;
    assert!(cfg.validate(&VenueRegistry::default()).is_ok());
}

#[test]
fn risk_and_loss_budgets_are_not_reset_by_a_funding_pause() {
    let mut cfg = funding_config();
    cfg.pair.mode = PaperMode::Turnover {
        max_turnover_quote: d("1000"),
        max_fees_quote: d("100"),
        max_realized_loss_quote: Some(d("1")),
        incentive: None,
    };
    cfg.require_net_edge = true;
    let mut r = new_replay(&cfg).unwrap();
    r.ledger.cash = d("-2");
    r.ledger.fees = d("2");
    r.ledger.turnover = d("200");
    r.cost_feedback.excess_reserve_bps = d("3");
    r.cost_feedback.cycles_used = 4;
    r.feed_online = true;
    r.funding_timers(HOUR + 1_001).unwrap();
    assert_eq!(r.ledger.cash, d("-2"));
    assert_eq!(r.ledger.fees, d("2"));
    assert_eq!(r.ledger.turnover, d("200"));
    assert_eq!(r.cost_feedback.cycles_used, 4);
    assert_eq!(r.cost_feedback.excess_reserve_bps, d("3"));
    if let MakerFirstEvent::Book { books, .. } = books(&cfg, HOUR + 1_002, "1000") {
        r.set_books(books, HOUR + 1_002).unwrap();
    }
    r.quote(HOUR + 1_002).unwrap();
    assert!(r.quote.is_none());
    assert_eq!(r.report.entry_blocked.get("turnover_loss_limit"), Some(&1));
}

#[test]
fn net_zero_is_not_a_flat_funding_position() {
    let cfg = funding_config();
    let mut r = new_replay(&cfg).unwrap();
    r.ledger.positions = [d("0.1"), d("-0.1")];
    r.cycle = Some(cycle(cfg.start_ms));
    r.funding_timers(HOUR).unwrap();
    assert_eq!(
        r.report.incomplete,
        Some("funding_boundary_not_flat_and_resolved")
    );
    assert_eq!(r.funding.unwrap().report.verified_slots_by_leg, [0, 0]);
}

#[test]
fn outstanding_fill_notice_blocks_boundary_even_when_ledger_is_flat() {
    let cfg = funding_config();
    let mut r = new_replay(&cfg).unwrap();
    r.notices.push_back((HOUR + 2_000, d("0.1")));
    r.funding_timers(HOUR).unwrap();
    assert_eq!(
        r.report.incomplete,
        Some("funding_boundary_not_flat_and_resolved")
    );
}

#[test]
fn an_unresolved_ioc_cannot_cross_the_funding_guard() {
    let cfg = funding_config();
    let mut r = new_replay(&cfg).unwrap();
    r.ioc = Some(Ioc {
        leg: 1,
        side: HedgeSide::Sell,
        quantity: d("0.1"),
        limit: d("1000"),
        touch: d("1000"),
        arrival_ms: HOUR + 1,
        expires_ms: HOUR + 2_000,
        unwind: true,
        closing_hedge: false,
        sub_min_top_up: false,
    });
    r.funding_timers(HOUR).unwrap();
    assert_eq!(
        r.report.incomplete,
        Some("funding_boundary_not_flat_and_resolved")
    );
}

#[test]
fn expired_delivery_is_invalid_even_after_a_quote_has_disappeared() {
    let cfg = funding_config();
    let r = run(
        &cfg,
        vec![
            books(&cfg, cfg.start_ms, "1000"),
            partial(HOUR + 2_000, HOUR - 1, "0.3"),
        ],
    );
    assert_eq!(r.incomplete, Some("funding_trade_delivery_bound_violated"));
    assert_eq!(r.net_pnl, None);
    assert_eq!(r.final_liquidation_pnl, None);
    assert_eq!(r.funding_flat.unwrap().failure, r.incomplete);
}

#[test]
fn old_reconnect_history_cannot_fill_a_new_order_or_invalidate_flatness() {
    let cfg = funding_config();
    let r = run(
        &cfg,
        vec![
            books(&cfg, cfg.start_ms, "1000"),
            partial(HOUR + 2_000, cfg.start_ms - 1, "10"),
        ],
    );
    assert_eq!(r.incomplete, None);
    assert_eq!(r.maker_fills, 0);
    assert_eq!(r.funding_flat.unwrap().ignored_prestart_prints, 1);
}

#[test]
fn future_print_fails_closed() {
    let cfg = funding_config();
    let r = run(
        &cfg,
        vec![
            books(&cfg, cfg.start_ms, "1000"),
            partial(cfg.start_ms + 1, cfg.start_ms + 2, "0.3"),
        ],
    );
    assert_eq!(r.incomplete, Some("funding_trade_delivery_bound_violated"));
    assert_eq!(r.net_pnl, None);
}

#[test]
fn overlap_and_replay_edge_guards_remain_paused() {
    let mut cfg = funding_config();
    cfg.start_ms = HOUR + 10;
    cfg.end_ms = 2 * HOUR + 10;
    cfg.pair.legs[1].funding.offset_ms = 2_000;
    let state = FundingState::new(&cfg).unwrap().unwrap();
    assert!(state.paused(cfg.start_ms));
    assert!(state.paused(HOUR + 1_500));
    assert!(state.paused(HOUR + 3_000));
    assert!(!state.paused(HOUR + 3_001));
    let r = run(&cfg, vec![books(&cfg, cfg.start_ms, "1000")]);
    assert_eq!(r.incomplete, None);
    assert_eq!(r.funding_flat.unwrap().verified_slots_by_leg, [1, 1]);
}

#[test]
fn recovery_requires_a_post_guard_book_and_never_reuses_funding_pause_depth() {
    let cfg = funding_config();
    let mut r = new_replay(&cfg).unwrap();
    if let MakerFirstEvent::Book { books, .. } = books(&cfg, HOUR, "1000") {
        r.set_books(books, HOUR).unwrap();
    }
    r.funding_timers(HOUR + 1_001).unwrap();
    assert_eq!(r.usable, [false; 2]);
    if let MakerFirstEvent::Book { books, .. } = books(&cfg, HOUR, "1000") {
        r.set_books(books, HOUR + 1_002).unwrap();
    }
    assert_eq!(r.usable, [false; 2]);
}

#[test]
fn a_late_precancel_fill_is_booked_and_cannot_disappear_at_the_boundary() {
    let cfg = funding_config();
    let mut r = new_replay(&cfg).unwrap();
    r.quote = Some(Quote {
        posture: TurnoverPosture::Neutral,
        side: HedgeSide::Buy,
        price: d("1000"),
        left: d("0.1"),
        ahead: Decimal::ZERO,
        queue_depth: None,
        arrival_ms: HOUR - 5_500,
        activated_ms: Some(HOUR - 5_500),
        expires_ms: HOUR - 4_700,
        cancelled_ms: None,
        hedge_limit: Some(d("999")),
        passive_entry: false,
        closing: false,
        closing_touched: false,
        closing_filled: false,
    });
    r.funding_timers(HOUR - 5_000).unwrap();
    assert_eq!(r.quote.as_ref().unwrap().cancelled_ms, Some(HOUR - 4_800));
    r.trade(
        HOUR - 4_700,
        HOUR - 4_850,
        HedgeSide::Sell,
        d("1000"),
        d("0.04"),
    )
    .unwrap();
    assert_eq!(r.ledger.positions[0], d("0.04"));
    assert_eq!(r.report.maker_fills, 1);
    r.funding_timers(HOUR).unwrap();
    assert_eq!(
        r.report.incomplete,
        Some("funding_boundary_not_flat_and_resolved")
    );
}

#[test]
fn two_real_cycles_keep_one_cash_ledger_across_two_slots() {
    let mut cfg = posture_config(TurnoverPosture::LongFirst);
    cfg.start_ms = HOUR - 30_000;
    cfg.end_ms = 2 * HOUR + 30_000;
    cfg.flat_at_funding = Some(FlatAtFunding {
        settlement_guard_ms: 1_000,
    });
    let first = posture_tape(&cfg, TurnoverPosture::LongFirst, None)
        .into_iter()
        .map(Result::unwrap)
        .filter(|e| !matches!(e, MakerFirstEvent::End { .. }))
        .collect::<Vec<_>>();
    let single = run(&cfg, first);
    let mut both = posture_tape(&cfg, TurnoverPosture::LongFirst, None)
        .into_iter()
        .map(Result::unwrap)
        .filter(|e| !matches!(e, MakerFirstEvent::End { .. }))
        .collect::<Vec<_>>();
    let mut next = posture_config(TurnoverPosture::LongFirst);
    next.start_ms = cfg.start_ms + HOUR;
    next.end_ms = cfg.end_ms;
    both.extend(
        posture_tape(&next, TurnoverPosture::LongFirst, None)
            .into_iter()
            .map(Result::unwrap)
            .filter(|e| !matches!(e, MakerFirstEvent::End { .. })),
    );
    let double = run(&cfg, both);
    assert_eq!(single.incomplete, None, "{single:?}");
    assert_eq!(double.incomplete, None, "{double:?}");
    assert!(single.closed_cycles > 0);
    assert_eq!(double.closed_cycles, 2 * single.closed_cycles);
    assert_eq!(double.maker_turnover, single.maker_turnover * d("2"));
    assert_eq!(double.net_pnl, single.net_pnl.map(|p| p * d("2")));
    assert_eq!(double.funding_flat.unwrap().verified_slots_by_leg, [2, 2]);
}

#[test]
fn a_trade_gap_during_an_unactivated_order_is_not_flat_evidence() {
    let cfg = funding_config();
    let r = run(
        &cfg,
        vec![
            MakerFirstEvent::TradeFeed {
                at_ms: cfg.start_ms,
                leg: 0,
                online: true,
            },
            books(&cfg, cfg.start_ms, "1000"),
            MakerFirstEvent::TradeFeed {
                at_ms: cfg.start_ms + 50,
                leg: 0,
                online: false,
            },
        ],
    );
    assert!(r.quotes_submitted > 0);
    assert_eq!(r.incomplete, Some("resting_order_during_trade_gap"));
    assert_eq!(r.net_pnl, None);
    assert_eq!(r.final_liquidation_pnl, None);
}

#[test]
fn funding_drain_closes_both_legs_and_keeps_all_exit_costs() {
    let cfg = funding_config();
    let mut r = new_replay(&cfg).unwrap();
    r.ledger.positions = [d("0.01"), d("-0.01")];
    r.cycle = Some(cycle(HOUR - 5_100));
    if let MakerFirstEvent::Book { books, .. } = books(&cfg, HOUR - 5_001, "1000") {
        r.set_books(books, HOUR - 5_001).unwrap();
    }
    r.funding_timers(HOUR - 5_000).unwrap();
    assert!(r.ioc.as_ref().is_some_and(|ioc| ioc.unwind));
    for now in [HOUR - 4_800, HOUR - 4_600] {
        if let MakerFirstEvent::Book { books, .. } = books(&cfg, now, "1000") {
            r.set_books(books, now).unwrap();
        }
        r.advance(now).unwrap();
    }
    assert!(r.funding_flat());
    assert!(r.ledger.fees > Decimal::ZERO);
    assert!(r.ledger.cash < Decimal::ZERO);
    let cash = r.ledger.cash;
    let fees = r.ledger.fees;
    let report = r
        .run([Ok(MakerFirstEvent::End { at_ms: cfg.end_ms })])
        .unwrap();
    assert_eq!(report.incomplete, None);
    assert_eq!(report.net_pnl, Some(cash));
    assert_eq!(report.fees, fees);
    assert_eq!(report.forced_closes, 1);
    assert_eq!(report.funding_flat.unwrap().verified_slots_by_leg, [1, 1]);
}

fn settlement_config() -> MakerFirstConfig {
    let mut cfg = config();
    cfg.start_ms = HOUR - 30_000;
    cfg.end_ms = HOUR + 30_000;
    cfg.hold_ms = 20_000;
    cfg.unhedged_timeout_ms = 10_000;
    cfg.drain_ms = 10_000;
    cfg.quote_lifetime_ms = 5_000;
    cfg.trade_delivery_grace_ms = 3_000;
    cfg.funding_settlement = Some(FundingSettlementPolicy {
        settlement_guard_ms: 1_000,
        max_settlement_arrival_lag_ms: 10_000,
    });
    cfg
}

fn witnessed(at_ms: u64, rate: &str, low_per_base: &str, high_per_base: &str) -> WitnessedSettlement {
    WitnessedSettlement {
        at_ms,
        exchange_time_ms: at_ms + 50,
        rate: d(rate),
        long_cash_low_per_base: d(low_per_base),
        long_cash_high_per_base: d(high_per_base),
    }
}

#[test]
fn funding_settlement_preflight_admits_cross_slot_and_rejects_mutual_exclusion() {
    let registry = VenueRegistry::default();
    let mut bare = config();
    bare.start_ms = HOUR - 30_000;
    bare.end_ms = HOUR + 30_000;
    let pre = bare.preflight(&registry).unwrap();
    assert!(!pre.execution_supported);
    assert_eq!(pre.funding_slots_by_leg, [1, 1]);
    assert_eq!(pre.blockers, vec!["maker_first_funding_lifecycle_not_supported"]);

    let mut cfg = settlement_config();
    let ok_pre = cfg.preflight(&registry).unwrap();
    assert!(ok_pre.execution_supported);
    assert!(ok_pre.blockers.is_empty());
    assert!(ok_pre.required_engine_work.is_empty());
    assert!(
        ok_pre
            .required_evidence
            .contains(&"witnessed_settlement_for_every_held_slot")
    );

    let mut eq_cfg = settlement_config();
    eq_cfg.pair.asset_class = AssetClass::Equity;
    eq_cfg.pair.oracle_max_age_ms = 5_000;
    eq_cfg.pair.equity_close_buffer_ms = 1_000;
    eq_cfg.enforce_equity_session_gate = true;
    assert!(eq_cfg.preflight(&registry).unwrap().execution_supported);

    cfg.flat_at_funding = Some(FlatAtFunding {
        settlement_guard_ms: 1_000,
    });
    assert!(cfg.preflight(&registry).is_err());
}

#[test]
fn late_pre_slot_maker_fill_and_post_slot_fill_attribute_by_source_time() {
    let mut cfg = settlement_config();
    cfg.end_ms = HOUR + 45_000;
    cfg.diagnose_cycles = true;
    cfg.entry_side = Some(HedgeSide::Buy);
    cfg.hold_ms = 20_000;
    cfg.drain_ms = 8_000;
    cfg.trade_from_ms = Some(HOUR - 3_000);

    let mut rows = vec![
        MakerFirstEvent::TradeFeed {
            at_ms: cfg.start_ms,
            leg: 0,
            online: true,
        },
        books(&cfg, HOUR - 3_000, "1000"),
        books(&cfg, HOUR - 2_000, "1000"),
        // Executed at HOUR - 1_500 (< HOUR - guard 1_000, before cancel at HOUR - 1_000),
        // but delivered late at HOUR + 200 (after funding slot HOUR).
        partial(HOUR + 200, HOUR - 1_500, "0.14"),
        books(&cfg, HOUR + 1_100, "1000"),
        books(&cfg, HOUR + 1_200, "1000"),
        books(&cfg, HOUR + 1_500, "1000"),
        books(&cfg, HOUR + 2_500, "1000"),
        MakerFirstEvent::FundingSettlement {
            at_ms: HOUR + 2_600,
            leg: 0,
            settlement: witnessed(HOUR, "0.0005", "-0.50", "-0.50"),
        },
        books(&cfg, HOUR + 3_500, "1000"),
        books(&cfg, HOUR + 20_200, "1000"),
        books(&cfg, HOUR + 23_400, "1000"),
        books(&cfg, HOUR + 23_600, "1000"),
        books(&cfg, HOUR + 23_800, "1000"),
    ];
    rows.sort_by_key(MakerFirstEvent::time);
    let r = run(&cfg, rows);
    assert_eq!(r.incomplete, None, "{r:?}");
    let lc = r.funding_lifecycle.expect("lifecycle report");
    assert_eq!(lc.planned_slots_by_leg, [1, 1]);
    assert_eq!(lc.resolved_slots_by_leg, [1, 1]);
    assert_eq!(lc.held_slots_by_leg, [1, 0]);
    assert_eq!(lc.flat_slots_by_leg, [0, 1]);
    assert_eq!(lc.witnessed_payments, 1);
    assert_eq!(lc.late_pre_slot_fills_attributed, 1);
    assert!(lc.post_slot_fills_excluded >= 1);
    assert_eq!(
        r.opening_cancel_requested.get("funding_boundary_guard"),
        Some(&1)
    );
    // Leg 0 held +0.04 by source_time <= HOUR; 0.04 * (-0.50) = -0.02 USD.
    assert_eq!(lc.witnessed_funding_cash_usd, d("-0.02"));
    assert_eq!(lc.witnessed_funding_cash_upper_usd, d("-0.02"));
    assert!(lc.cash_identity_reconciled);
    let net_pnl = r.net_pnl.expect("net_pnl");
    let price_pnl = lc.price_pnl_usd.expect("price_pnl_usd");
    assert_eq!(price_pnl + lc.witnessed_funding_cash_usd - r.fees, net_pnl);
    let costs = r.execution_costs.expect("execution costs");
    assert!(costs.whole_ledger_cash_reconciled);

    // Also verify a fresh flat book arriving inside the guard window is blocked before quoting.
    let guard_probe = run(
        &cfg,
        vec![
            MakerFirstEvent::TradeFeed {
                at_ms: cfg.start_ms,
                leg: 0,
                online: true,
            },
            books(&cfg, HOUR - 500, "1000"),
        ],
    );
    assert_eq!(guard_probe.quotes_submitted, 0);
    assert_eq!(
        guard_probe.entry_blocked.get("funding_boundary_guard"),
        Some(&1)
    );
    assert_eq!(
        guard_probe.funding_lifecycle.unwrap().boundary_guard_blocks,
        1
    );
}

#[test]
fn multi_hour_funding_settlement_reconciles_continuous_cash_and_cycles() {
    let mut cfg = settlement_config();
    cfg.start_ms = HOUR - 30_000;
    cfg.end_ms = 2 * HOUR + 30_000;
    cfg.hold_ms = 12_000;
    cfg.drain_ms = 10_000;
    cfg.trade_delivery_grace_ms = 4_000;
    cfg.diagnose_cycles = true;
    cfg.entry_side = Some(HedgeSide::Buy);
    cfg.pair.legs[1].funding.interval_ms = 8 * HOUR;

    let mut rows = vec![
        MakerFirstEvent::TradeFeed {
            at_ms: cfg.start_ms,
            leg: 0,
            online: true,
        },
        // Cycle 1: opens before HOUR, drains and closes at ~HOUR + 3_000 BEFORE watermark (HOUR + 4_000).
        books(&cfg, HOUR - 10_000, "1000"),
        books(&cfg, HOUR - 9_500, "1000"),
        partial(HOUR - 9_400, HOUR - 9_450, "0.14"),
        books(&cfg, HOUR - 9_000, "1000"),
        books(&cfg, HOUR - 8_800, "1000"),
        books(&cfg, HOUR - 2_000, "1000"),
        books(&cfg, HOUR + 2_600, "1000"),
        books(&cfg, HOUR + 2_800, "1000"),
        books(&cfg, HOUR + 3_000, "1000"),
        MakerFirstEvent::FundingSettlement {
            at_ms: HOUR + 3_100,
            leg: 0,
            settlement: witnessed(HOUR, "0.0005", "-0.50", "-0.45"),
        },
        books(&cfg, HOUR + 4_500, "1000"),
        // Cycle 2: opens at 2*HOUR - 5_000, holds past watermark (2*HOUR + 4_000), closes at ~2*HOUR + 8_000.
        books(&cfg, 2 * HOUR - 5_000, "1000"),
        books(&cfg, 2 * HOUR - 4_500, "1000"),
        partial(2 * HOUR - 4_400, 2 * HOUR - 4_450, "0.14"),
        books(&cfg, 2 * HOUR - 4_000, "1000"),
        books(&cfg, 2 * HOUR - 3_800, "1000"),
        books(&cfg, 2 * HOUR - 2_000, "1000"),
        books(&cfg, 2 * HOUR + 1_200, "1000"),
        MakerFirstEvent::FundingSettlement {
            at_ms: 2 * HOUR + 3_000,
            leg: 0,
            settlement: witnessed(2 * HOUR, "0.0010", "-1.00", "-0.95"),
        },
        books(&cfg, 2 * HOUR + 4_100, "1000"),
        books(&cfg, 2 * HOUR + 7_600, "1000"),
        books(&cfg, 2 * HOUR + 11_800, "1000"),
        books(&cfg, 2 * HOUR + 12_000, "1000"),
        books(&cfg, 2 * HOUR + 12_200, "1000"),
    ];
    rows.sort_by_key(MakerFirstEvent::time);
    let r = run(&cfg, rows);
    assert_eq!(r.incomplete, None, "{r:?}");
    let lc = r.funding_lifecycle.expect("lifecycle report");
    assert_eq!(lc.planned_slots_by_leg, [2, 0]);
    assert_eq!(lc.resolved_slots_by_leg, [2, 0]);
    assert_eq!(lc.held_slots_by_leg, [2, 0]);
    assert_eq!(lc.witnessed_payments, 2);
    // Slot 1: 0.04 * (-0.50) = -0.02; Slot 2: 0.04 * (-1.00) = -0.04; total = -0.06 USD.
    assert_eq!(lc.witnessed_funding_cash_usd, d("-0.06"));
    assert_eq!(lc.witnessed_funding_cash_upper_usd, d("-0.056"));
    assert!(lc.cash_identity_reconciled);
    let net_pnl = r.net_pnl.expect("net_pnl");
    let price_pnl = lc.price_pnl_usd.expect("price_pnl_usd");
    assert_eq!(price_pnl + lc.witnessed_funding_cash_usd - r.fees, net_pnl);
    assert_eq!(r.realized_pnl, net_pnl);
    let costs = r.execution_costs.expect("execution costs");
    assert_eq!(costs.post_cycle_funding_cash_quote, Decimal::ZERO);
    assert_eq!(costs.cycles.len(), 2);
    assert_eq!(costs.cycles[0].funding_cash_quote, d("-0.02"));
    assert_eq!(costs.cycles[1].funding_cash_quote, d("-0.04"));
    assert!(costs.cycles[0].cash_reconciled && costs.cycles[1].cash_reconciled);
    assert!(costs.whole_ledger_cash_reconciled);
}

#[test]
fn funding_settlement_fails_closed_on_missing_witness_ambiguous_fill_and_non_causal() {
    let mut cfg = settlement_config();
    cfg.end_ms = HOUR + 40_000;
    cfg.entry_side = Some(HedgeSide::Buy);
    cfg.pair.legs[1].funding.interval_ms = 8 * HOUR;

    // 1. Missing witness for a held position at deadline -> incomplete
    let missing_rows = vec![
        MakerFirstEvent::TradeFeed {
            at_ms: cfg.start_ms,
            leg: 0,
            online: true,
        },
        books(&cfg, HOUR - 5_000, "1000"),
        books(&cfg, HOUR - 4_500, "1000"),
        partial(HOUR - 4_400, HOUR - 4_450, "0.14"),
        books(&cfg, HOUR - 4_000, "1000"),
        books(&cfg, HOUR - 3_800, "1000"),
        books(&cfg, HOUR + 15_000, "1000"),
        books(&cfg, HOUR + 15_200, "1000"),
    ];
    let missing = run(&cfg, missing_rows);
    assert_eq!(missing.incomplete, Some("missing_witnessed_funding_settlement"));
    assert_eq!(missing.net_pnl, None);
    assert_eq!(missing.final_liquidation_pnl, None);

    // 2. Ambiguous fill inside [HOUR - guard, HOUR + guard] -> fail closed
    let mut r = new_replay(&cfg).unwrap();
    r.record_funding_lifecycle_fill(0, Some(0), HOUR - 500, HOUR, d("0.04"));
    assert_eq!(r.report.incomplete, Some("funding_boundary_ambiguous_fill"));

    // 3. Duplicate witnessed settlement -> fail closed
    let dup_rows = vec![
        MakerFirstEvent::FundingSettlement {
            at_ms: HOUR + 100,
            leg: 0,
            settlement: witnessed(HOUR, "0.0005", "-0.50", "-0.50"),
        },
        MakerFirstEvent::FundingSettlement {
            at_ms: HOUR + 200,
            leg: 0,
            settlement: witnessed(HOUR, "0.0005", "-0.50", "-0.50"),
        },
        books(&cfg, cfg.start_ms, "1000"),
    ];
    let dup = run(&cfg, dup_rows);
    assert_eq!(dup.incomplete, Some("duplicate_funding_settlement"));

    // 4. Non-causal witnessed settlement (at_ms > now) -> Err
    let err_rows = vec![
        Ok(books(&cfg, cfg.start_ms, "1000")),
        Ok(MakerFirstEvent::FundingSettlement {
            at_ms: HOUR - 10,
            leg: 0,
            settlement: witnessed(HOUR, "0.0005", "-0.50", "-0.50"),
        }),
        Ok(MakerFirstEvent::End { at_ms: cfg.end_ms }),
    ];
    let err = run_maker_first(&cfg, err_rows, &VenueRegistry::default()).unwrap_err();
    assert!(err.to_string().contains("non-causal"), "{err:#}");
}

#[test]
fn carry_drift_adjustment_penalizes_adverse_caps_favorable_and_respects_freshness() {
    // Disallow CarryDriftAdjustment in Turnover incentive mode.
    let mut turnover_cfg = posture_config(TurnoverPosture::LongFirst);
    turnover_cfg.carry_drift_adjustment = Some(CarryDriftAdjustment {
        expected_hold_ms: HOUR,
        max_rate_age_ms: 10_000,
        favorable_credit_fraction: d("0.25"),
        max_favorable_credit_bps: d("1"),
        max_adverse_penalty_bps: d("20"),
        require_fresh_rates: true,
    });
    assert!(turnover_cfg.preflight(&VenueRegistry::default()).is_err());

    let make_cfg = |req_fresh: bool, fav_frac: &str| {
        let mut cfg = config();
        cfg.maker_fee_bps = Decimal::ZERO;
        cfg.pair.legs[0].taker_fee_bps = Decimal::ZERO;
        cfg.pair.legs[1].taker_fee_bps = Decimal::ZERO;
        cfg.pair.legs[0].tick = d("0.01");
        cfg.pair.legs[1].tick = d("0.01");
        cfg.opening_max_levels = 1;
        cfg.require_net_edge = true;
        cfg.pair.planner.min_net_edge_bps = 1.0;
        cfg.entry_side = Some(HedgeSide::Buy);
        cfg.carry_drift_adjustment = Some(CarryDriftAdjustment {
            expected_hold_ms: HOUR,
            max_rate_age_ms: 5_000,
            favorable_credit_fraction: d(fav_frac),
            max_favorable_credit_bps: d("1.5"),
            max_adverse_penalty_bps: d("8"),
            require_fresh_rates: req_fresh,
        });
        cfg
    };
    let custom_books = |cfg: &MakerFirstConfig, at: u64, leg0_ask: &str, leg1_ask: &str| MakerFirstEvent::Book {
        at_ms: at,
        books: [
            Book {
                market: cfg.pair.legs[0].market.clone(),
                timestamp_ms: at,
                tradable: true,
                bids: vec![Level {
                    price: d("1000.0"),
                    contracts: d("0.1"),
                }],
                asks: vec![Level {
                    price: d(leg0_ask),
                    contracts: d("1"),
                }],
                bounds: None,
                available_notional: Decimal::from(150),
            },
            Book {
                market: cfg.pair.legs[1].market.clone(),
                timestamp_ms: at,
                tradable: true,
                bids: vec![Level {
                    price: d("1000.20"),
                    contracts: d("0.1"),
                }],
                asks: vec![Level {
                    price: d(leg1_ask),
                    contracts: d("1"),
                }],
                bounds: None,
                available_notional: Decimal::from(150),
            },
        ],
        reference: None,
        equity_session: None,
        leg_oracles: None,
    };

    // 1. Stale rates block when require_fresh_rates = true, fall back to 0 when false.
    let stale_cfg = make_cfg(true, "0.25");
    let stale_blocked = run(
        &stale_cfg,
        vec![
            MakerFirstEvent::TradeFeed {
                at_ms: stale_cfg.start_ms,
                leg: 0,
                online: true,
            },
            MakerFirstEvent::FundingRate {
                at_ms: stale_cfg.start_ms,
                leg: 0,
                source_ms: stale_cfg.start_ms,
                rate_per_settlement: d("0.0005"),
            },
            MakerFirstEvent::FundingRate {
                at_ms: stale_cfg.start_ms,
                leg: 1,
                source_ms: stale_cfg.start_ms,
                rate_per_settlement: Decimal::ZERO,
            },
            custom_books(&stale_cfg, stale_cfg.start_ms + 6_000, "1000.20", "1000.25"),
        ],
    );
    assert_eq!(stale_blocked.quotes_submitted, 0);
    assert_eq!(
        stale_blocked.entry_blocked.get("carry_rate_missing_or_stale"),
        Some(&1)
    );
    assert_eq!(stale_blocked.carry_drift.unwrap().stale_or_missing_blocks, 1);

    // Without fresh rates (require_fresh_rates = false), raw +1.5 bp passes with 0 bps carry adj.
    let fallback_cfg = make_cfg(false, "0.25");
    let fallback_ok = run(
        &fallback_cfg,
        vec![
            MakerFirstEvent::TradeFeed {
                at_ms: fallback_cfg.start_ms,
                leg: 0,
                online: true,
            },
            custom_books(&fallback_cfg, fallback_cfg.start_ms, "1000.20", "1000.25"),
        ],
    );
    assert_eq!(fallback_ok.quotes_submitted, 1);
    let fb_cd = fallback_ok.carry_drift.unwrap();
    assert_eq!(fb_cd.stale_or_missing_blocks, 0);
    assert_eq!(fb_cd.min_adjustment_bps, Some(Decimal::ZERO));

    // 2. Adverse carry (-5 bp) blocks raw +1.5 bp -> -3.5 bp (< 1 bp min_edge).
    let adv_cfg = make_cfg(true, "0.25");
    let t0 = adv_cfg.start_ms;
    let adv = run(
        &adv_cfg,
        vec![
            MakerFirstEvent::TradeFeed {
                at_ms: t0,
                leg: 0,
                online: true,
            },
            MakerFirstEvent::FundingRate {
                at_ms: t0,
                leg: 0,
                source_ms: t0,
                rate_per_settlement: d("0.0006"),
            },
            MakerFirstEvent::FundingRate {
                at_ms: t0,
                leg: 1,
                source_ms: t0,
                rate_per_settlement: d("0.0001"),
            },
            custom_books(&adv_cfg, t0, "1000.20", "1000.25"),
        ],
    );
    assert_eq!(adv.quotes_submitted, 0);
    let cd = adv.carry_drift.unwrap();
    assert_eq!(cd.adverse_adjustments, 1);
    assert_eq!(cd.min_adjustment_bps, Some(d("-5")));
    assert_eq!(cd.adverse_carry_blocked_entries, 1);
    assert_eq!(adv.entry_blocked.get("entry_net_edge"), Some(&1));

    // 3. Favorable carry (+40 bp/h * 0.25 capped at 1.5 bp) promotes raw -0.4 bp -> +1.1 bp >= 1 bp.
    let fav_cfg = make_cfg(true, "0.25");
    let fav = run(
        &fav_cfg,
        vec![
            MakerFirstEvent::TradeFeed {
                at_ms: t0,
                leg: 0,
                online: true,
            },
            MakerFirstEvent::FundingRate {
                at_ms: t0,
                leg: 0,
                source_ms: t0,
                rate_per_settlement: Decimal::ZERO,
            },
            MakerFirstEvent::FundingRate {
                at_ms: t0,
                leg: 1,
                source_ms: t0,
                rate_per_settlement: d("0.0040"),
            },
            custom_books(&fav_cfg, t0, "1000.04", "1000.28"),
        ],
    );
    assert_eq!(fav.quotes_submitted, 1);
    let fav_cd = fav.carry_drift.unwrap();
    assert_eq!(fav_cd.favorable_adjustments, 1);
    assert_eq!(fav_cd.max_adjustment_bps, Some(d("1.5")));

    // 4. With favorable_credit_fraction = 0, raw -0.4 bp remains < 1 bp (no carry credit).
    let no_credit_cfg = make_cfg(true, "0");
    let no_credit = run(
        &no_credit_cfg,
        vec![
            MakerFirstEvent::TradeFeed {
                at_ms: t0,
                leg: 0,
                online: true,
            },
            MakerFirstEvent::FundingRate {
                at_ms: t0,
                leg: 0,
                source_ms: t0,
                rate_per_settlement: Decimal::ZERO,
            },
            MakerFirstEvent::FundingRate {
                at_ms: t0,
                leg: 1,
                source_ms: t0,
                rate_per_settlement: d("0.0040"),
            },
            custom_books(&no_credit_cfg, t0, "1000.04", "1000.28"),
        ],
    );
    assert_eq!(no_credit.quotes_submitted, 0);
    assert_eq!(
        no_credit.carry_drift.unwrap().max_adjustment_bps,
        Some(Decimal::ZERO)
    );
}

#[test]
fn funding_settlement_inclusive_arrival_deadline_accepts_exact_limit_and_rejects_plus_1ms() {
    for (lag, expected) in [
        (Some(9_999), None),
        (Some(10_000), None),
        (Some(10_001), Some("missing_witnessed_funding_settlement")),
        (None, Some("missing_witnessed_funding_settlement")),
    ] {
        let mut cfg = settlement_config();
        cfg.end_ms = HOUR + 45_000;
        cfg.diagnose_cycles = true;
        cfg.entry_side = Some(HedgeSide::Buy);
        cfg.hold_ms = 20_000;
        cfg.drain_ms = 8_000;
        cfg.trade_from_ms = Some(HOUR - 3_000);

        let mut rows = vec![
            MakerFirstEvent::TradeFeed {
                at_ms: cfg.start_ms,
                leg: 0,
                online: true,
            },
            books(&cfg, HOUR - 3_000, "1000"),
            books(&cfg, HOUR - 2_000, "1000"),
            partial(HOUR + 200, HOUR - 1_500, "0.14"),
            books(&cfg, HOUR + 1_100, "1000"),
            books(&cfg, HOUR + 1_200, "1000"),
            books(&cfg, HOUR + 1_500, "1000"),
            books(&cfg, HOUR + 2_500, "1000"),
            books(&cfg, HOUR + 3_500, "1000"),
            books(&cfg, HOUR + 20_200, "1000"),
            books(&cfg, HOUR + 23_400, "1000"),
            books(&cfg, HOUR + 23_600, "1000"),
            books(&cfg, HOUR + 23_800, "1000"),
        ];
        if let Some(lag_ms) = lag {
            rows.push(MakerFirstEvent::FundingSettlement {
                at_ms: HOUR + lag_ms,
                leg: 0,
                settlement: witnessed(HOUR, "0.0005", "-0.50", "-0.50"),
            });
        }
        rows.sort_by_key(MakerFirstEvent::time);
        let rep = run(&cfg, rows);
        assert_eq!(rep.incomplete, expected, "lag={lag:?}: {rep:?}");
        if expected.is_none() {
            assert!(
                rep.funding_lifecycle
                    .as_ref()
                    .unwrap()
                    .cash_identity_reconciled
            );
        } else {
            assert_eq!(rep.net_pnl, None);
        }
    }

    // Also verify the exact `end_ms == deadline_ms` cutoff boundary:
    // a witness at `at_ms == end_ms == deadline_ms` is accepted, whereas a missing
    // witness at `end_ms == deadline_ms` fails closed with `missing_witnessed_funding_settlement`.
    for (include_witness, expected) in [
        (true, None),
        (false, Some("missing_witnessed_funding_settlement")),
    ] {
        let mut cfg = settlement_config();
        cfg.end_ms = HOUR + 10_000;
        cfg.trade_from_ms = Some(HOUR - 3_000);
        cfg.hold_ms = 3_000;
        cfg.drain_ms = 5_000;
        cfg.quote_lifetime_ms = 2_000;
        cfg.diagnose_cycles = true;
        cfg.entry_side = Some(HedgeSide::Buy);
        cfg.pair.legs[1].funding.interval_ms = 8 * HOUR;

        let mut rows = vec![
            MakerFirstEvent::TradeFeed {
                at_ms: cfg.start_ms,
                leg: 0,
                online: true,
            },
            books(&cfg, HOUR - 3_000, "1000"),
            books(&cfg, HOUR - 2_000, "1000"),
            partial(HOUR + 200, HOUR - 1_500, "0.14"),
            books(&cfg, HOUR + 1_100, "1000"),
            books(&cfg, HOUR + 1_200, "1000"),
            books(&cfg, HOUR + 1_500, "1000"),
            books(&cfg, HOUR + 2_500, "1000"),
            books(&cfg, HOUR + 3_200, "1000"),
            books(&cfg, HOUR + 6_400, "1000"),
            books(&cfg, HOUR + 6_600, "1000"),
            books(&cfg, HOUR + 6_800, "1000"),
        ];
        if include_witness {
            rows.push(MakerFirstEvent::FundingSettlement {
                at_ms: HOUR + 10_000,
                leg: 0,
                settlement: witnessed(HOUR, "0.0005", "-0.50", "-0.50"),
            });
        }
        rows.sort_by_key(MakerFirstEvent::time);
        let rep = run(&cfg, rows);
        assert_eq!(rep.incomplete, expected, "include_witness={include_witness}: {rep:?}");
        if expected.is_none() {
            let lc = rep.funding_lifecycle.as_ref().unwrap();
            assert!(lc.cash_identity_reconciled);
            assert_eq!(lc.witnessed_funding_cash_usd, d("-0.02"));
            let costs = rep.execution_costs.as_ref().unwrap();
            assert_eq!(costs.cycles.len(), 1);
            assert_eq!(costs.cycles[0].funding_cash_quote, d("-0.02"));
            assert!(costs.whole_ledger_cash_reconciled);
        } else {
            assert_eq!(rep.net_pnl, None);
        }
    }
}

#[test]
fn funding_settlement_late_witness_attributes_to_economic_owning_cycle_not_next_open_cycle() {
    let mut cfg = settlement_config();
    cfg.start_ms = HOUR - 30_000;
    cfg.end_ms = HOUR + 42_000;
    cfg.hold_ms = 12_000;
    cfg.drain_ms = 10_000;
    cfg.trade_delivery_grace_ms = 4_000;
    cfg.diagnose_cycles = true;
    cfg.entry_side = Some(HedgeSide::Buy);
    cfg.pair.legs[1].funding.interval_ms = 8 * HOUR;

    let mut rows = vec![
        MakerFirstEvent::TradeFeed {
            at_ms: cfg.start_ms,
            leg: 0,
            online: true,
        },
        // Cycle 0 opens before HOUR and closes at ~HOUR + 3_000 while still waiting for witness.
        books(&cfg, HOUR - 10_000, "1000"),
        books(&cfg, HOUR - 9_500, "1000"),
        partial(HOUR - 9_400, HOUR - 9_450, "0.14"),
        books(&cfg, HOUR - 9_000, "1000"),
        books(&cfg, HOUR - 8_800, "1000"),
        books(&cfg, HOUR - 2_000, "1000"),
        books(&cfg, HOUR + 2_600, "1000"),
        books(&cfg, HOUR + 2_800, "1000"),
        books(&cfg, HOUR + 3_000, "1000"),
        // Cycle 0 late settlement arrives at HOUR + 9_300 AFTER Cycle 1 has already opened!
        MakerFirstEvent::FundingSettlement {
            at_ms: HOUR + 9_300,
            leg: 0,
            settlement: witnessed(HOUR, "0.0005", "-0.50", "-0.45"),
        },
        books(&cfg, HOUR + 4_500, "1000"),
        books(&cfg, HOUR + 5_000, "1000"),
        books(&cfg, HOUR + 5_500, "1000"),
        partial(HOUR + 5_600, HOUR + 5_550, "0.14"),
        books(&cfg, HOUR + 6_000, "1000"),
        books(&cfg, HOUR + 6_200, "1000"),
        books(&cfg, HOUR + 8_000, "1000"),
        books(&cfg, HOUR + 11_200, "1000"),
        books(&cfg, HOUR + 14_100, "1000"),
        books(&cfg, HOUR + 17_600, "1000"),
        books(&cfg, HOUR + 21_800, "1000"),
        books(&cfg, HOUR + 22_000, "1000"),
        books(&cfg, HOUR + 22_200, "1000"),
    ];
    rows.sort_by_key(MakerFirstEvent::time);
    let rep = run(&cfg, rows);
    assert_eq!(rep.incomplete, None, "{rep:?}");

    let lc = rep.funding_lifecycle.expect("lifecycle report");
    assert_eq!(lc.witnessed_funding_cash_usd, d("-0.02"));
    assert_eq!(lc.unattributed_cross_cycle_payments, 0);
    assert!(lc.cash_identity_reconciled);

    let costs = rep.execution_costs.expect("cycle costs");
    assert_eq!(costs.cycles.len(), 2);
    assert_eq!(costs.cycles[0].funding_cash_quote, d("-0.02"));
    assert_eq!(costs.cycles[1].funding_cash_quote, Decimal::ZERO);
    assert_eq!(costs.post_cycle_funding_cash_quote, Decimal::ZERO);
    assert!(costs.cycles[0].cash_reconciled);
    assert!(costs.cycles[0].price_decomposition_reconciled);
    assert!(costs.cycles[1].cash_reconciled);
    assert!(costs.cycles[1].price_decomposition_reconciled);
    assert!(costs.whole_ledger_cash_reconciled);

    let net = rep.net_pnl.expect("net pnl");
    assert_eq!(rep.realized_pnl, net);
    assert_eq!(rep.forced_closes, 2);
    assert_eq!(rep.forced_close_pnl, net);
    assert_eq!(rep.turnover_posture.realized_pnl[0], net);
    let c0_net = costs.cycles[0].net_before_overhead.unwrap();
    let c1_net = costs.cycles[1].net_before_overhead.unwrap();
    assert_eq!(c0_net + c1_net, net);
    assert_eq!(costs.cycles[0].price_loss_quote, costs.cycles[1].price_loss_quote);
    assert_eq!(c0_net - c1_net, d("-0.02"));
}

#[test]
fn carry_drift_same_source_ms_conflicting_rates_fail_closed_and_identical_is_idempotent() {
    let policy = CarryDriftAdjustment {
        expected_hold_ms: HOUR,
        max_rate_age_ms: 60_000,
        favorable_credit_fraction: Decimal::ONE,
        max_favorable_credit_bps: d("100"),
        max_adverse_penalty_bps: d("100"),
        require_fresh_rates: false,
    };

    let mut strict_policy = policy;
    strict_policy.require_fresh_rates = true;

    // 1. Conflicting rates at identical source_ms are order-independent and suspend adjustment.
    for (first, second) in [("0.0005", "-0.0005"), ("-0.0005", "0.0005")] {
        let mut state = CarryDriftState::default();
        state
            .observe_rate(0, HOUR, HOUR, d(first), HOUR)
            .unwrap();
        // Out-of-order older rate update must be ignored without distorting state.
        state
            .observe_rate(0, HOUR - 1, HOUR, d("0.0099"), HOUR)
            .unwrap();
        state
            .observe_rate(0, HOUR, HOUR, d(second), HOUR)
            .unwrap();
        // Subsequent observation at the already-conflicted source_ms stays fail-closed.
        state
            .observe_rate(0, HOUR, HOUR, d("0.0005"), HOUR)
            .unwrap();
        state
            .observe_rate(0, HOUR - 1, HOUR, d("0.0001"), HOUR)
            .unwrap();
        state
            .observe_rate(1, HOUR, HOUR, Decimal::ZERO, HOUR)
            .unwrap();
        assert_eq!(
            state.adjustment_bps(policy, 0, HedgeSide::Buy, HOUR),
            Err("carry_rate_conflict")
        );
        assert_eq!(
            state.adjustment_bps(strict_policy, 0, HedgeSide::Buy, HOUR),
            Err("carry_rate_conflict")
        );
        assert_eq!(state.report.conflicting_rate_events, 2);

        // Strictly newer unambiguous source_ms supersedes conflict cleanly.
        state
            .observe_rate(0, HOUR + 1, HOUR + 1, d("0.0002"), HOUR)
            .unwrap();
        assert_eq!(
            state.adjustment_bps(policy, 0, HedgeSide::Buy, HOUR + 1),
            Ok(d("-2"))
        );
    }

    // 2. Duplicate identical observation at same source_ms is idempotent.
    let mut idem = CarryDriftState::default();
    idem.observe_rate(0, HOUR, HOUR, d("0.0005"), HOUR).unwrap();
    idem.observe_rate(0, HOUR, HOUR, d("0.0005"), HOUR).unwrap();
    idem.observe_rate(1, HOUR, HOUR, Decimal::ZERO, HOUR)
        .unwrap();
    assert_eq!(idem.report.rate_updates_by_leg, [1, 1]);
    assert_eq!(idem.report.conflicting_rate_events, 0);
    assert_eq!(
        idem.adjustment_bps(policy, 0, HedgeSide::Buy, HOUR),
        Ok(d("-5"))
    );

    // 3. End-to-end replay suspends quoting on conflicting same-source_ms rate updates.
    let mut cfg = config();
    cfg.maker_fee_bps = Decimal::ZERO;
    cfg.pair.legs[0].taker_fee_bps = Decimal::ZERO;
    cfg.pair.legs[1].taker_fee_bps = Decimal::ZERO;
    cfg.opening_max_levels = 1;
    cfg.entry_side = Some(HedgeSide::Buy);
    cfg.carry_drift_adjustment = Some(policy);
    let t0 = cfg.start_ms;
    let blocked = run(
        &cfg,
        vec![
            MakerFirstEvent::TradeFeed {
                at_ms: t0,
                leg: 0,
                online: true,
            },
            MakerFirstEvent::FundingRate {
                at_ms: t0,
                leg: 0,
                source_ms: t0,
                rate_per_settlement: d("0.0005"),
            },
            MakerFirstEvent::FundingRate {
                at_ms: t0,
                leg: 0,
                source_ms: t0,
                rate_per_settlement: d("-0.0005"),
            },
            MakerFirstEvent::FundingRate {
                at_ms: t0,
                leg: 1,
                source_ms: t0,
                rate_per_settlement: Decimal::ZERO,
            },
            books(&cfg, t0, "1000"),
        ],
    );
    assert_eq!(blocked.quotes_submitted, 0);
    assert_eq!(blocked.entry_blocked.get("carry_rate_conflict"), Some(&1));
    let cd = blocked.carry_drift.unwrap();
    assert_eq!(cd.conflicting_rate_events, 1);
    assert_eq!(cd.conflicting_rate_blocks, 1);
}

#[test]
fn funding_lifecycle_multi_slot_and_concurrent_cross_cycle_attribution() {
    let mut cfg = settlement_config();
    cfg.start_ms = HOUR - 30_000;
    cfg.end_ms = 3 * HOUR + 30_000;
    cfg.pair.legs[1].funding.interval_ms = 8 * HOUR;
    cfg.validate(&VenueRegistry::default()).unwrap();

    let mut lc = FundingLifecycleState::new(&cfg).unwrap().unwrap();
    // Slot 1 (`HOUR`): Cycle 0 opens and closes flat before the slot -> flat slot.
    assert_eq!(
        lc.on_fill(0, Some(0), HOUR - 10_000, HOUR - 10_000, d("0.10")),
        None
    );
    assert_eq!(
        lc.on_fill(0, Some(0), HOUR - 5_000, HOUR - 5_000, d("-0.10")),
        None
    );
    let (p1, e1) = lc.resolve_ready(HOUR + 3_000, false);
    assert!(p1.is_empty() && e1.is_none());
    assert_eq!(lc.report.flat_slots_by_leg, [1, 0]);

    // Slot 2 (`2 * HOUR`): two concurrent cycles hold across the slot -> unattributed (`None`).
    assert_eq!(
        lc.on_fill(0, Some(1), HOUR + 20_000, HOUR + 20_000, d("0.05")),
        None
    );
    assert_eq!(
        lc.on_fill(0, Some(2), HOUR + 30_000, HOUR + 30_000, d("0.05")),
        None
    );
    assert_eq!(
        lc.on_witness(
            0,
            2 * HOUR + 5_000,
            witnessed(2 * HOUR, "0.0010", "-1.00", "-0.90")
        )
        .unwrap(),
        None
    );
    let (p2, e2) = lc.resolve_ready(2 * HOUR + 5_000, false);
    assert_eq!(e2, None);
    assert_eq!(p2.len(), 1);
    assert_eq!(p2[0].cycle_id, None);
    assert_eq!(p2[0].low, d("-0.10"));
    assert_eq!(p2[0].high, d("-0.09"));
    assert_eq!(lc.report.unattributed_cross_cycle_payments, 1);

    // Slot 3 (`3 * HOUR`): Cycles 1 and 2 close after `2 * HOUR`; Cycle 3 holds across `3 * HOUR` -> `Some(3)`.
    assert_eq!(
        lc.on_fill(0, Some(1), 2 * HOUR + 10_000, 2 * HOUR + 10_000, d("-0.05")),
        None
    );
    assert_eq!(
        lc.on_fill(0, Some(2), 2 * HOUR + 12_000, 2 * HOUR + 12_000, d("-0.05")),
        None
    );
    assert_eq!(
        lc.on_fill(0, Some(3), 2 * HOUR + 20_000, 2 * HOUR + 20_000, d("0.04")),
        None
    );
    assert_eq!(
        lc.on_witness(
            0,
            3 * HOUR + 5_000,
            witnessed(3 * HOUR, "0.0005", "-0.50", "-0.45")
        )
        .unwrap(),
        None
    );
    let (p3, e3) = lc.resolve_ready(3 * HOUR + 5_000, false);
    assert_eq!(e3, None);
    assert_eq!(p3.len(), 1);
    assert_eq!(p3[0].cycle_id, Some(3));
    assert_eq!(p3[0].low, d("-0.02"));
    assert_eq!(p3[0].high, d("-0.018"));
    assert_eq!(lc.report.unattributed_cross_cycle_payments, 1);
}

#[test]
fn carry_drift_conflict_cancels_resting_opening_quote_preserving_inflight_fill() {
    let mut cfg = config();
    cfg.maker_fee_bps = Decimal::ZERO;
    cfg.pair.legs[0].taker_fee_bps = Decimal::ZERO;
    cfg.pair.legs[1].taker_fee_bps = Decimal::ZERO;
    cfg.entry_side = Some(HedgeSide::Buy);
    cfg.carry_drift_adjustment = Some(CarryDriftAdjustment {
        expected_hold_ms: HOUR,
        max_rate_age_ms: 60_000,
        favorable_credit_fraction: Decimal::ONE,
        max_favorable_credit_bps: d("100"),
        max_adverse_penalty_bps: d("100"),
        require_fresh_rates: true,
    });
    let t0 = cfg.start_ms;
    let rate = |receipt_ms, leg, value| MakerFirstEvent::FundingRate {
        at_ms: receipt_ms,
        source_ms: t0,
        leg,
        rate_per_settlement: d(value),
    };
    let cancel_bound_ms = cfg.transport_ms + cfg.maker_cancel_delay_ms;
    assert!(t0 + 200 + cancel_bound_ms < t0 + 500);

    for (conflict_at, trade_at, expected_fills, expected_cancels) in [
        (t0, t0 + 500, 0, 0),
        (t0 + 200, t0 + 500, 0, 1),
        (t0 + 200, t0 + 350, 1, 1),
    ] {
        let rows = vec![
            MakerFirstEvent::TradeFeed {
                at_ms: t0,
                leg: 0,
                online: true,
            },
            rate(t0, 0, "-0.0005"),
            rate(t0, 1, "0"),
            rate(conflict_at, 0, "0.0005"),
            books(&cfg, t0, "1000"),
            books(&cfg, t0 + 150, "1000"),
            partial(trade_at, trade_at, "0.14"),
            books(&cfg, t0 + 550, "1000"),
            books(&cfg, t0 + 650, "1000"),
            books(&cfg, t0 + 750, "1000"),
            books(&cfg, t0 + 950, "1000"),
            books(&cfg, t0 + 2_500, "1000"),
            books(&cfg, t0 + 2_700, "1000"),
            books(&cfg, t0 + 2_900, "1000"),
        ];
        let report = run(&cfg, rows);
        let drift = report.carry_drift.as_ref().unwrap();
        assert_eq!(drift.conflicting_rate_events, 1);
        assert_eq!(report.maker_fills, expected_fills);
        assert_eq!(report.maker_fills_after_cancel_request, expected_fills);
        assert_eq!(
            report
                .opening_cancel_requested
                .get("carry_rate_conflict")
                .copied()
                .unwrap_or(0),
            expected_cancels
        );
    }
}

#[test]
fn carry_drift_require_fresh_rates_cancels_resting_opening_quote_on_stale_rate() {
    let make_cfg = |require_fresh_rates: bool| {
        let mut cfg = config();
        cfg.maker_fee_bps = Decimal::ZERO;
        cfg.pair.legs[0].taker_fee_bps = Decimal::ZERO;
        cfg.pair.legs[1].taker_fee_bps = Decimal::ZERO;
        cfg.entry_side = Some(HedgeSide::Buy);
        cfg.carry_drift_adjustment = Some(CarryDriftAdjustment {
            expected_hold_ms: HOUR,
            max_rate_age_ms: 200,
            favorable_credit_fraction: Decimal::ONE,
            max_favorable_credit_bps: d("100"),
            max_adverse_penalty_bps: d("100"),
            require_fresh_rates,
        });
        cfg
    };

    let build_rows = |cfg: &MakerFirstConfig, trade_at: u64| {
        let t0 = cfg.start_ms;
        vec![
            MakerFirstEvent::TradeFeed {
                at_ms: t0,
                leg: 0,
                online: true,
            },
            MakerFirstEvent::FundingRate {
                at_ms: t0,
                leg: 0,
                source_ms: t0,
                rate_per_settlement: d("-0.0005"),
            },
            MakerFirstEvent::FundingRate {
                at_ms: t0,
                leg: 1,
                source_ms: t0,
                rate_per_settlement: Decimal::ZERO,
            },
            books(cfg, t0, "1000"),
            books(cfg, t0 + 150, "1000"),
            books(cfg, t0 + 250, "1000"),
            partial(trade_at, trade_at, "0.14"),
            books(cfg, t0 + 550, "1000"),
            books(cfg, t0 + 650, "1000"),
            books(cfg, t0 + 750, "1000"),
            books(cfg, t0 + 950, "1000"),
            books(cfg, t0 + 2_500, "1000"),
            books(cfg, t0 + 2_700, "1000"),
            books(cfg, t0 + 2_900, "1000"),
        ]
    };

    // Strict freshness cancels via exact timer at +201 ms (`age = 201 > 200`, `cancelled_ms = +401 ms`),
    // blocking +500 ms fill while preserving in-flight fill at +350 ms (`< 201 + 200 ms`).
    let strict_cfg = make_cfg(true);
    let t0 = strict_cfg.start_ms;
    let strict_blocked = run(&strict_cfg, build_rows(&strict_cfg, t0 + 500));
    assert_eq!(strict_blocked.maker_fills, 0);
    assert_eq!(
        strict_blocked
            .opening_cancel_requested
            .get("carry_rate_missing_or_stale"),
        Some(&1)
    );

    let strict_inflight = run(&strict_cfg, build_rows(&strict_cfg, t0 + 350));
    assert_eq!(strict_inflight.maker_fills, 1);
    assert_eq!(strict_inflight.maker_fills_after_cancel_request, 1);
    assert_eq!(
        strict_inflight
            .opening_cancel_requested
            .get("carry_rate_missing_or_stale"),
        Some(&1)
    );

    // Permissive fallback (`require_fresh_rates = false`) does not cancel resting opening quote on staleness.
    let relaxed_cfg = make_cfg(false);
    let relaxed = run(&relaxed_cfg, build_rows(&relaxed_cfg, relaxed_cfg.start_ms + 500));
    assert_eq!(relaxed.maker_fills, 1);
    assert_eq!(
        relaxed
            .opening_cancel_requested
            .get("carry_rate_missing_or_stale"),
        None
    );
}

#[test]
fn carry_drift_conflict_and_stale_never_cancel_closing_quote() {
    let mut cfg = config();
    cfg.maker_fee_bps = Decimal::ZERO;
    cfg.pair.legs[0].taker_fee_bps = Decimal::ZERO;
    cfg.pair.legs[1].taker_fee_bps = Decimal::ZERO;
    cfg.hold_ms = 20_000;
    cfg.drain_ms = 10_000;
    cfg.entry_side = Some(HedgeSide::Buy);
    cfg.passive_offset = Some(PassiveOffset {
        max_net_notional: d("100"),
        max_wait_ms: 5_000,
        close_probability: None,
        min_passive_edge_bps: None,
        min_offset_edge_bps: None,
        max_adverse_move_bps: None,
        offset_max_levels: 1,
        offset_quote_lifetime_ms: None,
        passive_loop_only: false,
    });
    cfg.carry_drift_adjustment = Some(CarryDriftAdjustment {
        expected_hold_ms: HOUR,
        max_rate_age_ms: 200,
        favorable_credit_fraction: Decimal::ONE,
        max_favorable_credit_bps: d("100"),
        max_adverse_penalty_bps: d("100"),
        require_fresh_rates: true,
    });

    let t0 = cfg.start_ms;
    let mut r = new_replay(&cfg).unwrap();
    r.feed_online = true;
    r.ledger.positions = [d("0.04"), Decimal::ZERO];
    r.cycle = Some(cycle(t0));
    if let MakerFirstEvent::Book { books, .. } = books(&cfg, t0, "1000") {
        r.set_books(books, t0).unwrap();
    }
    r.quote = Some(Quote {
        posture: TurnoverPosture::Neutral,
        side: HedgeSide::Sell,
        price: d("1001"),
        left: d("0.04"),
        ahead: Decimal::ZERO,
        queue_depth: None,
        arrival_ms: t0,
        activated_ms: Some(t0),
        expires_ms: t0 + 10_000,
        cancelled_ms: None,
        hedge_limit: None,
        passive_entry: false,
        closing: true,
        closing_touched: false,
        closing_filled: false,
    });

    // 1. Resting closing quote stays intact when funding rate goes stale.
    r.carry_drift
        .observe_rate(0, t0, t0, d("-0.0005"), HOUR)
        .unwrap();
    r.carry_drift
        .observe_rate(1, t0, t0, Decimal::ZERO, HOUR)
        .unwrap();
    if let MakerFirstEvent::Book { books, .. } = books(&cfg, t0 + 250, "1000") {
        r.set_books(books, t0 + 250).unwrap();
    }
    r.advance_quote(t0 + 250, true);
    assert!(
        r.quote
            .as_ref()
            .is_some_and(|q| q.closing && q.cancelled_ms.is_none())
    );

    // 2. Resting closing quote stays intact when same-source_ms funding rate conflicts.
    r.carry_drift
        .observe_rate(0, t0, t0 + 300, d("0.0005"), HOUR)
        .unwrap();
    r.advance_quote(t0 + 300, false);
    assert!(
        r.quote
            .as_ref()
            .is_some_and(|q| q.closing && q.cancelled_ms.is_none())
    );
    assert!(r.report.cycle_attribution.closing_quotes.cancel_requested.is_empty());
}

#[test]
fn carry_drift_quiet_tape_cancels_at_exact_staleness_boundary_without_book_events() {
    let make_cfg = |require_fresh_rates: bool| {
        let mut cfg = config();
        cfg.maker_fee_bps = Decimal::ZERO;
        cfg.pair.legs[0].taker_fee_bps = Decimal::ZERO;
        cfg.pair.legs[1].taker_fee_bps = Decimal::ZERO;
        cfg.entry_side = Some(HedgeSide::Buy);
        // Ensure quote TTL / book staleness outlast the quiet gap so only carry staleness can fire.
        cfg.quote_lifetime_ms = 2_000;
        cfg.pair.planner.max_quote_age_ms = 2_000;
        cfg.carry_drift_adjustment = Some(CarryDriftAdjustment {
            expected_hold_ms: HOUR,
            max_rate_age_ms: 200,
            favorable_credit_fraction: Decimal::ONE,
            max_favorable_credit_bps: d("100"),
            max_adverse_penalty_bps: d("100"),
            require_fresh_rates,
        });
        cfg
    };

    let build_quiet_rows =
        |cfg: &MakerFirstConfig, trade_at: u64, refresh_leg0_at_150: bool, refresh_leg1_at_150: bool| {
            let t0 = cfg.start_ms;
            let mut rows = vec![
                MakerFirstEvent::TradeFeed {
                    at_ms: t0,
                    leg: 0,
                    online: true,
                },
                MakerFirstEvent::FundingRate {
                    at_ms: t0,
                    leg: 0,
                    source_ms: t0,
                    rate_per_settlement: d("-0.0005"),
                },
                MakerFirstEvent::FundingRate {
                    at_ms: t0,
                    leg: 1,
                    source_ms: t0,
                    rate_per_settlement: Decimal::ZERO,
                },
                books(cfg, t0, "1000"),
                books(cfg, t0 + 150, "1000"),
            ];
            if refresh_leg0_at_150 {
                rows.push(MakerFirstEvent::FundingRate {
                    at_ms: t0 + 150,
                    leg: 0,
                    source_ms: t0 + 150,
                    rate_per_settlement: d("-0.0005"),
                });
            }
            if refresh_leg1_at_150 {
                rows.push(MakerFirstEvent::FundingRate {
                    at_ms: t0 + 150,
                    leg: 1,
                    source_ms: t0 + 150,
                    rate_per_settlement: Decimal::ZERO,
                });
            }
            rows.extend([
                // Quiet tape between +150 and `trade_at`: NO intermediate book events.
                partial(trade_at, trade_at, "0.14"),
                books(cfg, t0 + 700, "1000"),
                books(cfg, t0 + 950, "1000"),
                books(cfg, t0 + 2_500, "1000"),
                books(cfg, t0 + 2_700, "1000"),
                books(cfg, t0 + 2_900, "1000"),
            ]);
            rows
        };

    // Rates sampled at t0 with max_rate_age_ms=200 expire at +201 ms; cancel arrives at +401 ms.
    let strict_cfg = make_cfg(true);
    let t0 = strict_cfg.start_ms;
    let blocked_boundary = run(&strict_cfg, build_quiet_rows(&strict_cfg, t0 + 401, false, false));
    assert_eq!(blocked_boundary.maker_fills, 0);
    assert_eq!(
        blocked_boundary
            .opening_cancel_requested
            .get("carry_rate_missing_or_stale"),
        Some(&1)
    );

    let inflight_before_cancel =
        run(&strict_cfg, build_quiet_rows(&strict_cfg, t0 + 400, false, false));
    assert_eq!(inflight_before_cancel.maker_fills, 1);
    assert_eq!(inflight_before_cancel.maker_fills_after_cancel_request, 1);
    assert_eq!(
        inflight_before_cancel
            .opening_cancel_requested
            .get("carry_rate_missing_or_stale"),
        Some(&1)
    );

    // Permissive fallback (`require_fresh_rates = false`) must not arm a quiet-tape staleness cancel.
    let relaxed_cfg = make_cfg(false);
    let relaxed = run(&relaxed_cfg, build_quiet_rows(&relaxed_cfg, t0 + 500, false, false));
    assert_eq!(relaxed.maker_fills, 1);
    assert_eq!(
        relaxed
            .opening_cancel_requested
            .get("carry_rate_missing_or_stale"),
        None
    );

    // Refreshing ONLY leg 0 at +150 still leaves leg 1 at t0 -> expires at +201 ms -> +401 blocked.
    let one_leg_only = run(&strict_cfg, build_quiet_rows(&strict_cfg, t0 + 401, true, false));
    assert_eq!(one_leg_only.maker_fills, 0);
    assert_eq!(
        one_leg_only
            .opening_cancel_requested
            .get("carry_rate_missing_or_stale"),
        Some(&1)
    );

    // Refreshing BOTH legs at +150 advances expiry to +351 ms (`cancelled_ms = +551 ms`):
    // - before +351 ms (+350 ms): still valid, fills cleanly with zero carry cancel requested;
    // - in cancel flight [+351, +551) (+401 ms): cancel requested at +351, print arrives in-flight;
    // - at/after +551 ms (+551 ms): blocked after cancel completes.
    let both_legs_before_expiry =
        run(&strict_cfg, build_quiet_rows(&strict_cfg, t0 + 350, true, true));
    assert_eq!(both_legs_before_expiry.maker_fills, 1);
    assert_eq!(both_legs_before_expiry.maker_fills_after_cancel_request, 0);
    assert_eq!(
        both_legs_before_expiry
            .opening_cancel_requested
            .get("carry_rate_missing_or_stale"),
        None
    );
    let both_legs_inflight =
        run(&strict_cfg, build_quiet_rows(&strict_cfg, t0 + 401, true, true));
    assert_eq!(both_legs_inflight.maker_fills, 1);
    assert_eq!(both_legs_inflight.maker_fills_after_cancel_request, 1);
    assert_eq!(
        both_legs_inflight
            .opening_cancel_requested
            .get("carry_rate_missing_or_stale"),
        Some(&1)
    );
    let both_legs_later_blocked =
        run(&strict_cfg, build_quiet_rows(&strict_cfg, t0 + 551, true, true));
    assert_eq!(both_legs_later_blocked.maker_fills, 0);
    assert_eq!(
        both_legs_later_blocked
            .opening_cancel_requested
            .get("carry_rate_missing_or_stale"),
        Some(&1)
    );
}

#[test]
fn peer_0259_quiet_tape_uses_trade_source_and_cancels_before_equal_time_refresh() {
    let mut cfg = config();
    cfg.maker_fee_bps = Decimal::ZERO;
    cfg.pair.legs.iter_mut().for_each(|leg| leg.taker_fee_bps = Decimal::ZERO);
    cfg.entry_side = Some(HedgeSide::Buy);
    cfg.carry_drift_adjustment = Some(CarryDriftAdjustment {
        expected_hold_ms: HOUR,
        max_rate_age_ms: 200,
        favorable_credit_fraction: Decimal::ONE,
        max_favorable_credit_bps: d("100"),
        max_adverse_penalty_bps: d("100"),
        require_fresh_rates: true,
    });
    let t0 = cfg.start_ms;
    let update = |offset, leg, value| MakerFirstEvent::FundingRate {
        at_ms: t0 + offset,
        source_ms: t0 + offset,
        leg,
        rate_per_settlement: d(value),
    };
    let replay = |source_offset, refresh| {
        let mut rows = vec![
            MakerFirstEvent::TradeFeed {
                at_ms: t0,
                leg: 0,
                online: true,
            },
            update(0, 0, "-0.0005"),
            update(0, 1, "0"),
            books(&cfg, t0, "1000"),
            books(&cfg, t0 + 150, "1000"),
            partial(t0 + 500, t0 + source_offset, "0.14"),
            books(&cfg, t0 + 550, "1000"),
            books(&cfg, t0 + 650, "1000"),
            books(&cfg, t0 + 750, "1000"),
        ];
        if let Some(offset) = refresh {
            rows.extend([update(offset, 0, "-0.0005"), update(offset, 1, "0")]);
        }
        run(&cfg, rows)
    };
    for (source, refresh, expected) in [
        (400, None, 1),
        (401, None, 0),
        (450, None, 0),
        (500, Some(200), 1),
        (500, Some(201), 0),
    ] {
        let rep = replay(source, refresh);
        assert_eq!(rep.maker_fills, expected);
        assert_eq!(rep.maker_fills_after_cancel_request, expected);
        assert_eq!(
            rep.opening_cancel_requested.get("carry_rate_missing_or_stale"),
            Some(&1)
        );
    }
}
