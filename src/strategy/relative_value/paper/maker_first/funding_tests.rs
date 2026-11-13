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
        posture: TurnoverPosture::Neutral,
        started_ms: at,
        initial_cash: Decimal::ZERO,
        initial_turnover: Decimal::ZERO,
        draining: false,
        externalizing: false,
        maker_turnover: Decimal::ZERO,
        passive_side: None,
        paired_at_ms: None,
        hedge_fill_at_ms: None,
        paired_basis_bps: None,
        closing_maker_filled: false,
        forced_ioc_filled: false,
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
