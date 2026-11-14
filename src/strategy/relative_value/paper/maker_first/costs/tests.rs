use super::*;
fn d(s: &str) -> Decimal {
    s.parse().unwrap()
}
fn fill(
    now: u64,
    leg: usize,
    phase: Phase,
    side: HedgeSide,
    price: &str,
    mid: Option<&str>,
    fee: &str,
) -> CostFill {
    CostFill {
        now,
        leg,
        maker_leg: 0,
        phase,
        side,
        base: Decimal::ONE,
        notional: d(price),
        fee: d(fee),
        mid: mid.map(d),
        submit_touch: Some(d("100.05")),
    }
}

#[test]
fn flat_cycle_components_reconcile_and_labels_do_not_add_to_cash() {
    let mut costs = Costs::new();
    costs.begin(10_000, [Some(d("100")); 2]);
    for f in [
        fill(
            10_000,
            0,
            Phase::EntryMaker,
            HedgeSide::Buy,
            "100.1",
            Some("100"),
            "0.01",
        ),
        fill(
            10_100,
            1,
            Phase::EntryHedge,
            HedgeSide::Sell,
            "100",
            Some("100.1"),
            "0.02",
        ),
        fill(
            10_300,
            0,
            Phase::CloseMaker,
            HedgeSide::Sell,
            "99.8",
            Some("99.9"),
            "0",
        ),
        fill(
            10_400,
            1,
            Phase::CloseHedge,
            HedgeSide::Buy,
            "99.9",
            Some("100"),
            "0.01",
        ),
    ] {
        costs.fill(f);
    }
    costs.close(10_500, d("-0.24"), d("0.05"));
    // Labels can mature after the complete cycle has already closed.
    costs.book(11_000, Some((11_000, d("99.8"))));
    costs.book(15_000, Some((15_000, d("101"))));
    costs.finish(15_000);
    let row = &costs.cycles[0];
    assert!(row.cash_reconciled && row.price_decomposition_reconciled);
    assert_eq!(row.price_loss_quote, Some(d("0.2")));
    assert_eq!(row.price_drift_cost_quote, Some(Decimal::ZERO));
    assert_eq!(row.opening_hedge_slippage_vs_submit_touch, d("0.05"));
    assert_eq!(row.net_after_overhead, Some(d("-0.29")));
    assert_eq!(row.target_turnover, d("199.9"));
    assert_eq!(row.entry_markouts[0].mid_move_cost_quote, d("0.2"));
    assert_eq!(row.entry_markouts[0].markout_quote, d("-0.3"));
    assert_eq!(row.entry_markouts[1].mid_move_cost_quote, d("-1"));
    assert_eq!(row.entry_markouts[1].markout_quote, d("0.9"));
    let json = serde_json::to_value(row).unwrap();
    assert_eq!(json["net_before_overhead"], "-0.24");
    assert_eq!(json["stages"][0]["execution_cost_quote"], "0.1");
    assert_eq!(json["entry_markouts"][0]["mid_move_cost_quote"], "0.2");
    costs.reconcile_ledger(Some(d("-0.24")), Some(d("-0.29")));
    assert!(costs.whole_ledger_cash_reconciled);
    let json = serde_json::to_value(&costs).unwrap();
    assert_eq!(json["whole_ledger_net_before_overhead"], "-0.24");
    costs.reconcile_ledger(Some(d("-0.24")), Some(d("-0.28")));
    assert!(!costs.whole_ledger_cash_reconciled);
    costs.reconcile_ledger(None, None);
    assert!(!costs.whole_ledger_cash_reconciled);
}

#[test]
fn stale_future_and_absent_labels_remain_unobserved() {
    let mut c = Costs::new();
    c.begin(10_000, [Some(d("100")); 2]);
    c.fill(fill(
        10_000,
        0,
        Phase::EntryMaker,
        HedgeSide::Sell,
        "100",
        Some("100"),
        "0",
    ));
    c.book(11_000, Some((10_999, d("101"))));
    c.book(11_000, Some((11_001, d("101"))));
    c.book(12_001, Some((12_001, d("101"))));
    c.finish(14_000);
    let row = &c.cycles[0];
    assert_eq!(row.entry_markouts[0].marked_fills, 0);
    assert_eq!(row.entry_markouts[0].expired_fills, 1);
    assert_eq!(row.entry_markouts[1].pending_at_cutoff, 1);
    assert!(!row.completed);
    assert!(row.net_after_overhead.is_none());
}

#[test]
fn missing_mids_cannot_be_reported_as_zero_price_drift() {
    let mut c = Costs::new();
    c.begin(10_000, [Some(d("100")); 2]);
    c.fill(fill(
        10_000,
        0,
        Phase::EntryMaker,
        HedgeSide::Buy,
        "100",
        None,
        "0",
    ));
    c.fill(fill(
        10_100,
        0,
        Phase::ForcedClose,
        HedgeSide::Sell,
        "99",
        Some("99"),
        "0",
    ));
    c.close(10_100, d("-1"), Decimal::ZERO);
    let row = &c.cycles[0];
    assert!(row.cash_reconciled);
    assert!(!row.price_decomposition_reconciled);
    assert!(row.price_drift_cost_quote.is_none());
    assert_eq!(row.entry_markouts[0].missing_entry_mid, 1);
}
