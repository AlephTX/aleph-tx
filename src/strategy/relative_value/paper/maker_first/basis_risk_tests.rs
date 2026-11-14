use super::*;

fn risk_config() -> MakerFirstConfig {
    let mut c = posture_config(TurnoverPosture::Neutral);
    c.pair.limit_buffer_bps = 0.0;
    c.latency_reserve_bps = d("0.5");
    c.pair.mode = PaperMode::Turnover {
        max_turnover_quote: d("100000"), max_fees_quote: d("100"),
        max_realized_loss_quote: Some(d("100")),
        incentive: Some(IncentiveTurnover { eligible_leg: 0,
            eligible_from_ms: c.start_ms, eligible_until_ms: c.end_ms,
            max_cost_bps: d("5") }),
    };
    c.turnover_feedback = Some(TurnoverFeedback { alpha: Decimal::ZERO,
        min_reserve_bps: d("0.25"), max_reserve_bps: d("0.25"),
        overhead_bps: Decimal::ZERO, max_quote_distance_bps: Decimal::ZERO });
    c.basis_risk_reserve = Some(BasisRiskReserve { lookback_ms: 1000, sample_ms: 100,
        max_gap_ms: 250, min_reserve_bps: d("0.5"), range_multiplier: d("0.5"), max_reserve_bps: d("10") });
    c
}

fn set(r: &mut Replay<'_>, at: u64) {
    if let MakerFirstEvent::Book { books, .. } = books(r.config, at, "999.8") {
        r.set_books(books, at).unwrap();
    }
}

#[test]
fn warmup_blocks_new_risk_and_resting_quote_cancels_with_original_delay() {
    let c = risk_config();
    c.preflight(&VenueRegistry::default()).unwrap();
    let t = c.start_ms;
    let mut r = new_replay(&c).unwrap();
    r.feed_online = true;
    set(&mut r, t);
    r.quote(t).unwrap();
    assert!(r.quote.is_none());
    assert_eq!(r.report.entry_blocked.get("basis_risk_warmup_or_gap"), Some(&1));
    for at in (t + 100..=t + 1000).step_by(100) { set(&mut r, at); }
    r.quote(t + 1000).unwrap();
    assert!(r.quote.is_some());
    set(&mut r, t + 1100);
    r.advance_quote(t + 1100, true);
    assert!(r.quote.as_ref().unwrap().activated_ms.is_some());
    // A discontinuity invalidates the risk history. The quote remains exposed
    // until the existing cancel boundary; it is never erased by the filter.
    r.basis_risk.reset();
    r.advance_quote(t + 1101, true);
    let q = r.quote.as_ref().unwrap();
    assert_eq!(q.cancelled_ms, Some(t + 1101 + c.transport_ms + c.maker_cancel_delay_ms));
    assert!(q.left > Decimal::ZERO);
    assert_eq!(r.report.quote_cancels, 1);
}

#[test]
fn opening_cost_gate_tightens_but_paired_close_ignores_missing_risk_history() {
    let c = risk_config();
    let mut r = new_replay(&c).unwrap();
    r.feed_online = true;
    let t = c.start_ms;
    for at in (t..=t + 1000).step_by(100) { set(&mut r, at); }
    let b = r.pair_books(t + 1000).unwrap();
    let opening = r.candidate(b, HedgeSide::Buy, false, &b[0].bids[0]).unwrap().unwrap();
    let budget = c.pair.mode.remaining_budget(Decimal::ZERO, Decimal::ZERO, Decimal::ZERO);
    assert!(r.turnover_rank(&opening, budget, &mut None, t + 1000).is_ok());
    r.basis_risk.observe(c.basis_risk_reserve.unwrap(), c.latency_reserve_bps,
        t + 1100, Some((d("100"), [t + 1100; 2])));
    assert_eq!(r.turnover_rank(&opening, budget, &mut None, t + 1100), Err("turnover_cost"));
    r.ledger.positions = [d("0.1"), d("-0.1")];
    let b = r.pair_books(t + 1100).unwrap();
    let closing = r.candidate(b, HedgeSide::Sell, true, &b[0].asks[0]).unwrap().unwrap();
    assert!(closing.turnover.is_none());
    r.basis_risk.reset();
    assert!(r.turnover_rank(&closing, budget, &mut None, t + 1100).is_ok());
}

#[test]
fn reserve_cannot_lower_original_floor_or_enable_unrelated_strategy() {
    let mut c = risk_config();
    c.basis_risk_reserve.as_mut().unwrap().max_reserve_bps = d("0.1");
    assert!(c.preflight(&VenueRegistry::default()).is_err());
    c.basis_risk_reserve.as_mut().unwrap().max_reserve_bps = d("2");
    c.turnover_feedback = None;
    assert!(c.preflight(&VenueRegistry::default()).is_err());
}
