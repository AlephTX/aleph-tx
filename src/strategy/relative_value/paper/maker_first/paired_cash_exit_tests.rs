use super::*;

fn setup() -> MakerFirstConfig {
    let mut cfg = passive_config(HedgeSide::Buy);
    cfg.passive_offset = None;
    cfg.hedged_exit = Some(HedgedExit {
        max_hold_ms: 10_000,
        min_saving_bps: Decimal::ZERO,
        max_adverse_basis_bps: d("20"),
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
    cfg.pair.legs.iter_mut().for_each(|t| t.taker_fee_bps = Decimal::ZERO);
    cfg.paired_cash_exit = Some(PairedCashExit {
        all_turnover_cost_bps: d("0.25"),
        extra_taker_cost_bps: Decimal::ONE,
    });
    cfg
}

fn seed(cfg: &MakerFirstConfig) -> Replay<'_> {
    let mut r = new_replay(cfg).unwrap();
    let start = cfg.start_ms;
    r.feed_online = true;
    r.ledger.positions = [d("0.04"), d("-0.04")];
    r.ledger.cash = Decimal::ONE;
    r.ledger.turnover = d("80");
    r.cycle = Some(Cycle {
        id: 0,
        posture: TurnoverPosture::Neutral,
        started_ms: start,
        initial_cash: Decimal::ZERO,
        initial_turnover: Decimal::ZERO,
        initial_fees: Decimal::ZERO,
        draining: false,
        externalizing: false,
        maker_turnover: d("40"),
        passive_side: None,
        entry_ref_mid: None,
        paired_mids: [Some(d("1000.1")), Some(d("1000.3"))],
        paired_at_ms: Some(start),
        hedge_fill_at_ms: Some(start),
        paired_basis_bps: None,
        closing_maker_filled: false,
        forced_ioc_filled: false,
        sub_min_top_up_attempts: 0,
        sub_min_recovery_pending_base: Decimal::ZERO,
        used_partner_step_deadband: false,
        cash_exit_triggered: false,
    });
    refresh(&mut r, start);
    r
}

fn refresh(r: &mut Replay<'_>, at: u64) {
    let MakerFirstEvent::Book { books, .. } = books(r.config, at, "1000") else { unreachable!() };
    r.set_books(books, at).unwrap();
}

#[test]
fn cash_exit_prices_full_depth_and_all_sunk_turnover_costs() {
    let cfg = setup();
    let r = seed(&cfg);
    let policy = cfg.paired_cash_exit.unwrap();
    let expected = d("0.984") - policy.cost(d("160.016"), d("120.016"));
    assert_eq!(r.paired_cash_exit_value(cfg.start_ms), Some(expected));
    let mut nearly = seed(&cfg);
    nearly.ledger.cash = d("0.02");
    // Gross displayed liquidation is positive, but original+exit costs exceed it.
    assert!(nearly.paired_cash_exit_value(cfg.start_ms).is_none());
}

#[test]
fn cash_exit_requires_confirmed_exact_pair_fresh_unconsumed_full_depth() {
    let cfg = setup();
    let now = cfg.start_ms;
    let mut r = seed(&cfg);
    r.notices.push_back((now + 100, d("0.01")));
    assert!(r.paired_cash_exit_value(now).is_none());
    r.notices.clear();
    r.ledger.positions[1] = d("-0.03");
    assert!(r.paired_cash_exit_value(now).is_none());
    r.ledger.positions[1] = d("-0.04");
    r.consumed_ms[0] = now;
    assert!(r.paired_cash_exit_value(now).is_none());
    r.consumed_ms[0] = 0;
    assert!(r.paired_cash_exit_value(now + 100_000).is_none());
    r.books.as_mut().unwrap()[0].bids[0].contracts = d("0.03");
    assert!(r.paired_cash_exit_value(now).is_none());
    refresh(&mut r, now);
    r.ledger.positions = [d("0.0405"), d("-0.0405")];
    assert!(r.paired_cash_exit_value(now).is_none(), "lot dust cannot be marked flat");
}

#[test]
fn cash_exit_supports_reverse_pair_and_charges_both_venue_fees() {
    let mut cfg = setup();
    let mut r = seed(&cfg);
    r.ledger.positions = [d("-0.04"), d("0.04")];
    assert!(r.paired_cash_exit_value(cfg.start_ms).is_some());
    cfg.pair.legs[1].taker_fee_bps = d("500");
    assert!(seed(&cfg).paired_cash_exit_value(cfg.start_ms).is_none());
}

#[test]
fn cash_exit_cancel_race_retains_notice_barrier_and_does_not_overclose() {
    let cfg = setup();
    let start = cfg.start_ms;
    let mut r = seed(&cfg);
    let mut q = cancellation_quote(start);
    q.side = HedgeSide::Sell;
    q.price = d("1000.2");
    q.left = d("0.04");
    q.closing = true;
    q.cancelled_ms = None;
    r.quote = Some(q);
    r.advance(start + 10).unwrap();
    assert!(r.cycle.as_ref().unwrap().draining);
    assert!(r.ioc.is_none(), "resting maker retains the close reservation");
    assert_eq!(r.quote.as_ref().unwrap().cancelled_ms, Some(start + 210));
    // A pre-cancel public print arrives after the cancellation's source cutoff.
    r.trade(start + 310, start + 200, HedgeSide::Buy, d("1000.2"), d("0.04")).unwrap();
    assert_eq!(r.ledger.positions, [Decimal::ZERO, d("-0.04")]);
    r.advance_quote(start + 361, false);
    r.advance(start + 361).unwrap();
    assert!(r.ioc.is_none(), "late-fill notice has not arrived");
    refresh(&mut r, start + 411);
    r.advance_quote(start + 411, true);
    r.advance(start + 411).unwrap();
    let ioc = r.ioc.as_ref().expect("close only the confirmed remaining leg");
    assert_eq!((ioc.leg, ioc.side, ioc.quantity), (1, HedgeSide::Buy, d("0.04")));
    refresh(&mut r, start + 511);
    r.advance(start + 511).unwrap();
    assert!(r.ledger.is_flat());
    let stats = r.report.paired_cash_exit.as_ref().unwrap();
    assert_eq!((stats.triggers, stats.with_pending_maker_quote, stats.settled_cycles), (1, 1, 1));
    let cycle_turnover = r.ledger.turnover;
    let maker_turnover = d("40") + d("40.008");
    assert_eq!(stats.realized_stress_cash, r.ledger.cash-cfg.paired_cash_exit.unwrap().cost(cycle_turnover,cycle_turnover-maker_turnover));
}

#[test]
fn cash_exit_disabled_is_absent_and_invalid_configuration_is_rejected() {
    let mut cfg = setup();
    cfg.paired_cash_exit = None;
    let r = seed(&cfg);
    assert!(r.paired_cash_exit_value(cfg.start_ms).is_none());
    assert!(serde_json::to_value(&r.report).unwrap().get("paired_cash_exit").is_none());
    cfg = setup();
    cfg.hedged_exit = None;
    assert!(cfg.validate(&VenueRegistry::default()).is_err());
    cfg = setup();
    cfg.paired_cash_exit.as_mut().unwrap().extra_taker_cost_bps = -Decimal::ONE;
    assert!(cfg.validate(&VenueRegistry::default()).is_err());
}
