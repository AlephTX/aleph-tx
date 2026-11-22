use super::*;
use crate::live_hedge::journal::JournalSink;
use std::{
    fs, io,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

fn d(value: &str) -> Decimal {
    value.parse().unwrap()
}
fn spec() -> SessionSpec {
    SessionSpec {
        session_id: uuid::Uuid::new_v4().to_string(),
        legs: [
            LegLimits {
                identity: LegIdentity {
                    venue: "arcus".into(),
                    network: "testnet".into(),
                    account: "0x1111111111111111111111111111111111111111".into(),
                    subaccount: Some(2),
                    market: "ANT-USD".into(),
                    market_id: 88,
                },
                quote_asset: "USDG".into(),
                quantity_step: d("0.001"),
                max_position_base: d("5"),
                max_order_notional: d("1000"),
                price_rules: VenuePriceRules::Arcus {
                    tick_size: d("0.01"),
                    tick_tiers: vec![],
                },
                quote_budget: d("2000"),
                max_fee_rate: d("0.001"),
            },
            LegLimits {
                identity: LegIdentity {
                    venue: "trade_xyz".into(),
                    network: "testnet".into(),
                    account: "0x2222222222222222222222222222222222222222".into(),
                    subaccount: None,
                    market: "xyz:ANT".into(),
                    market_id: 100088,
                },
                quote_asset: "USDC".into(),
                quantity_step: d("0.01"),
                max_position_base: d("5"),
                max_order_notional: d("1000"),
                price_rules: VenuePriceRules::TradeXyz { sz_decimals: 2 },
                quote_budget: d("2000"),
                max_fee_rate: d("0.001"),
            },
        ],
        max_unhedged_base: d("2"),
        max_observation_age_us: 1_000_000,
    }
}
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!("aleph-ant-synthetic-{}", uuid::Uuid::new_v4())))
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn journal() -> (Temp, ExecutionJournal) {
    let temp = Temp::new();
    let mut j = ExecutionJournal::create(&temp.0, spec(), 100).unwrap();
    reconcile(&mut j);
    (temp, j)
}
fn at(j: &ExecutionJournal) -> u64 {
    j.ledger.written_at_us.max(j.recovery_after_us) + 10
}
fn intent(id: &str, leg: Leg, side: HedgeSide, quantity: &str) -> OrderIntent {
    OrderIntent {
        client_order_id: id.into(),
        venue: if leg == Leg::Primary {
            "arcus"
        } else {
            "trade_xyz"
        }
        .into(),
        instrument: "ANT".into(),
        leg,
        side,
        base_quantity: d(quantity),
        limit_price: d("100"),
        immediate_or_cancel: leg == Leg::Hedge,
    }
}
fn prepare(
    j: &mut ExecutionJournal,
    id: &str,
    leg: Leg,
    side: HedgeSide,
    quantity: &str,
) -> PreparedOrder {
    let order = intent(id, leg, side, quantity);
    let bound = (side == HedgeSide::Sell).then_some(order.limit_price);
    j.prepare_intent(order, false, bound, at(j)).unwrap()
}
fn ack(j: &mut ExecutionJournal, id: &str) {
    let i = index(j.ledger.orders[id].prepared.intent.leg);
    j.acknowledge(
        OrderAcknowledgement {
            identity: j.ledger.spec.legs[i].identity.clone(),
            logical_id: id.into(),
            exchange_order_id: format!("order-{id}"),
        },
        at(j),
    )
    .unwrap();
}
fn fill(
    j: &ExecutionJournal,
    id: &str,
    trade: &str,
    quantity: &str,
    venue_at_us: u64,
) -> ExecutionFill {
    let i = index(j.ledger.orders[id].prepared.intent.leg);
    ExecutionFill {
        identity: j.ledger.spec.legs[i].identity.clone(),
        logical_id: id.into(),
        exchange_order_id: format!("order-{id}"),
        trade_id: trade.into(),
        venue_at_us,
        quantity: d(quantity),
        price: d("100"),
        fee: d("0.001"),
        fee_asset: if i == 0 { "USDG" } else { "USDC" }.into(),
    }
}
fn terminal(
    j: &mut ExecutionJournal,
    id: &str,
    quantity: &str,
    status: OrderStatus,
    source_us: u64,
) {
    let order = &j.ledger.orders[id];
    let i = index(order.prepared.intent.leg);
    j.record_terminal(
        OrderTerminal {
            identity: j.ledger.spec.legs[i].identity.clone(),
            logical_id: id.into(),
            exchange_order_id: order.exchange_id.clone(),
            status,
            cumulative_filled: d(quantity),
            venue_at_us: source_us,
        },
        at(j),
    )
    .unwrap();
}
fn proof(j: &ExecutionJournal) -> Reconciliation {
    Reconciliation {
        observed_sequence: j.sequence(),
        legs: std::array::from_fn(|i| {
            let terminal = j
                .ledger
                .orders
                .values()
                .filter(|o| index(o.prepared.intent.leg) == i)
                .filter_map(|o| o.terminal.as_ref().map(|t| t.venue_at_us))
                .max()
                .unwrap_or(0);
            let fill = j
                .ledger
                .fills
                .iter()
                .filter(|((leg, _), _)| *leg == i)
                .map(|(_, f)| f.venue_at_us)
                .max()
                .unwrap_or(0);
            PositionEvidence {
                identity: j.ledger.spec.legs[i].identity.clone(),
                request_started_us: at(j),
                received_at_us: at(j) + 1,
                position_venue_at_us: terminal.max(fill) + 10,
                fills_complete_through_venue_us: terminal.max(fill) + 10,
                covered_trade_ids: j
                    .ledger
                    .fills
                    .keys()
                    .filter(|(leg, _)| *leg == i)
                    .map(|(_, id)| id.clone())
                    .collect(),
                open_order_ids: vec![],
                position_base: j.positions()[i],
            }
        }),
    }
}
fn reconcile(j: &mut ExecutionJournal) {
    j.reconcile(proof(j), at(j) + 2).unwrap();
}
fn file_path(temp: &Temp, spec: &SessionSpec) -> PathBuf {
    temp.0.join(format!(
        "{}.ant-journal.jsonl",
        uuid::Uuid::parse_str(&spec.session_id).unwrap().simple()
    ))
}

#[test]
fn intents_are_durable_unique_and_acks_are_not_fills() {
    let (temp, mut j) = journal();
    let p = prepare(&mut j, "p1", Leg::Primary, HedgeSide::Buy, "1");
    let raw = fs::read_to_string(file_path(&temp, &j.ledger.spec)).unwrap();
    assert!(raw.contains(&p.wire_client_id)); // only synthetic records
    assert_eq!(p.wire_client_id.len(), 33);
    assert_eq!(j.exposure().unwrap().1.maximum_base, d("1"));
    ack(&mut j, "p1");
    assert_eq!(j.positions(), [Decimal::ZERO; 2]);
    assert_eq!(j.exposure().unwrap().1.maximum_base, d("1"));
    let sequence = j.sequence();
    assert!(j.prepare_intent(p.intent, false, None, at(&j)).is_err());
    assert_eq!(j.sequence(), sequence);
    assert!(j.requires_reconciliation());
}

#[test]
fn pending_orders_on_both_venues_do_not_net_away_worst_case_risk() {
    let (_temp, mut j) = journal();
    prepare(&mut j, "p1", Leg::Primary, HedgeSide::Buy, "2");
    ack(&mut j, "p1");
    let hedge = prepare(&mut j, "h1", Leg::Hedge, HedgeSide::Sell, "2");
    ack(&mut j, "h1");
    assert_eq!(hedge.wire_client_id.len(), 34);
    assert_eq!(
        j.exposure().unwrap().1,
        ExposureRange {
            minimum_base: d("-2"),
            maximum_base: d("2")
        }
    );
    assert!(
        j.prepare_intent(
            intent("p2", Leg::Primary, HedgeSide::Buy, "0.1"),
            false,
            None,
            at(&j)
        )
        .is_err()
    );
}

#[test]
fn maker_partial_fill_can_be_hedged_while_primary_order_is_still_live() {
    let (_temp, mut j) = journal();
    let maker = prepare(&mut j, "p1", Leg::Primary, HedgeSide::Buy, "2");
    assert!(!maker.intent.immediate_or_cancel);
    let f = fill(&j, "p1", "t1", "0.5", 1000);
    j.record_fill(f, at(&j)).unwrap();
    prepare(&mut j, "h1", Leg::Hedge, HedgeSide::Sell, "0.5");
    let f = fill(&j, "h1", "t2", "0.5", 1000);
    j.record_fill(f, at(&j)).unwrap();
    assert_eq!(j.positions(), [d("0.5"), d("-0.5")]);
    assert_eq!(j.exposure().unwrap().1.maximum_base, d("1.5"));
    assert!(j.ledger.orders["p1"].terminal.is_none());
}

#[test]
fn cancel_and_terminal_never_release_until_exact_fills_and_both_positions_match() {
    let (_temp, mut j) = journal();
    prepare(&mut j, "p1", Leg::Primary, HedgeSide::Buy, "2");
    ack(&mut j, "p1");
    let f = fill(&j, "p1", "t1", "0.5", 1000);
    j.record_fill(f, at(&j)).unwrap();
    j.cancel_requested("p1", at(&j)).unwrap();
    terminal(&mut j, "p1", "0.5", OrderStatus::Canceled, 1001);
    assert_eq!(j.exposure().unwrap().1.maximum_base, d("2"));
    let mut missing = proof(&j);
    missing.legs[0].covered_trade_ids.clear();
    assert!(j.reconcile(missing, at(&j) + 2).is_err());
    assert_eq!(j.exposure().unwrap().1.maximum_base, d("2"));
    reconcile(&mut j);
    assert_eq!(j.exposure().unwrap().1.maximum_base, d("0.5"));
}

#[test]
fn duplicate_fills_are_idempotent_but_changed_fees_are_not() {
    let (_temp, mut j) = journal();
    prepare(&mut j, "p1", Leg::Primary, HedgeSide::Buy, "1");
    let mut f = fill(&j, "p1", "trade1", "0.5", 1000);
    assert!(j.record_fill(f.clone(), at(&j)).unwrap().is_some());
    let seq = j.sequence();
    assert!(j.record_fill(f.clone(), at(&j)).unwrap().is_none());
    assert_eq!(j.sequence(), seq);
    f.fee = d("0.002");
    assert!(j.record_fill(f, at(&j)).is_err());
    assert!(j.requires_reconciliation());
    assert_eq!(j.positions()[0], d("0.5"));
}

#[test]
fn actual_closes_and_cross_zero_are_separate_from_new_inventory() {
    let (_temp, mut j) = journal();
    prepare(&mut j, "buy", Leg::Primary, HedgeSide::Buy, "1");
    let f = fill(&j, "buy", "b1", "1", 1000);
    let e = j.record_fill(f, at(&j)).unwrap().unwrap();
    assert_eq!(e.opened_base, Some(d("1")));
    terminal(&mut j, "buy", "1", OrderStatus::Filled, 1001);
    reconcile(&mut j);
    prepare(&mut j, "sell", Leg::Primary, HedgeSide::Sell, "1.5");
    let f = fill(&j, "sell", "s1", "1.5", 1002);
    let e = j.record_fill(f, at(&j)).unwrap().unwrap();
    assert_eq!(e.closed_base, Some(d("1")));
    assert_eq!(e.opened_base, Some(d("0.5")));
    assert_eq!(e.crossed_zero, Some(true));
    assert_eq!(e.after_base, d("-0.5"));
}

#[test]
fn late_fill_is_preserved_and_reopens_recovery_and_reservations() {
    let (_temp, mut j) = journal();
    prepare(&mut j, "p1", Leg::Primary, HedgeSide::Buy, "2");
    ack(&mut j, "p1");
    terminal(&mut j, "p1", "0", OrderStatus::Canceled, 1000);
    reconcile(&mut j);
    assert_eq!(j.exposure().unwrap().1.maximum_base, Decimal::ZERO);
    let f = fill(&j, "p1", "late", "0.3", 999);
    j.record_fill(f, at(&j)).unwrap();
    assert_eq!(j.positions()[0], d("0.3"));
    assert_eq!(j.exposure().unwrap().1.maximum_base, d("2"));
    assert!(j.requires_reconciliation());
    assert!(j.reconcile(proof(&j), at(&j) + 2).is_err());
    terminal(&mut j, "p1", "0.3", OrderStatus::Canceled, 1001);
    reconcile(&mut j);
    assert_eq!(j.exposure().unwrap().1.maximum_base, d("0.3"));
}

#[test]
fn terminal_report_ahead_of_received_fills_cannot_release_reserve() {
    let (_temp, mut j) = journal();
    prepare(&mut j, "p1", Leg::Primary, HedgeSide::Buy, "1");
    ack(&mut j, "p1");
    terminal(&mut j, "p1", "1", OrderStatus::Filled, 1000);
    assert!(j.reconcile(proof(&j), at(&j) + 2).is_err());
    assert_eq!(j.positions()[0], Decimal::ZERO);
    let f = fill(&j, "p1", "t1", "1", 999);
    j.record_fill(f, at(&j)).unwrap();
    reconcile(&mut j);
    assert_eq!(j.positions()[0], d("1"));
}

#[test]
fn recovery_never_resubmits_and_old_reconciliation_cannot_unlock_it() {
    let (temp, mut j) = journal();
    let p = prepare(&mut j, "p1", Leg::Primary, HedgeSide::Buy, "1");
    ack(&mut j, "p1");
    terminal(&mut j, "p1", "0", OrderStatus::Canceled, 1000);
    reconcile(&mut j);
    let old = proof(&j);
    let spec = j.ledger.spec.clone();
    drop(j);
    let mut recovered = ExecutionJournal::recover(&temp.0, &spec, 10_000).unwrap();
    assert!(recovered.requires_reconciliation());
    assert_eq!(
        recovered.prepared_orders()[0].wire_client_id,
        p.wire_client_id
    );
    assert!(
        recovered
            .prepare_intent(
                intent("p2", Leg::Primary, HedgeSide::Buy, "1"),
                false,
                None,
                10_010
            )
            .is_err()
    );
    assert!(recovered.reconcile(old, 10_020).is_err());
    let mut fresh = proof(&recovered);
    for leg in &mut fresh.legs {
        leg.request_started_us = 10_021;
        leg.received_at_us = 10_022;
    }
    recovered.reconcile(fresh, 10_023).unwrap();
    assert!(!recovered.requires_reconciliation());
    drop(recovered);
    let audit = audit_journal(&temp.0, &spec).unwrap();
    assert!(!audit.restart_allowed);
    assert_eq!(audit.unverified_orders, 0);
}

#[test]
fn ack_loss_and_empty_open_order_lists_do_not_authorize_resume() {
    let (temp, mut j) = journal();
    prepare(&mut j, "p1", Leg::Primary, HedgeSide::Buy, "1");
    j.mark_uncertain("p1", at(&j)).unwrap();
    let spec = j.ledger.spec.clone();
    drop(j);
    let mut recovered = ExecutionJournal::recover(&temp.0, &spec, 1000).unwrap();
    let mut p = proof(&recovered);
    for leg in &mut p.legs {
        leg.request_started_us = 1001;
        leg.received_at_us = 1002;
    }
    assert!(recovered.reconcile(p, 1003).is_err());
    assert!(recovered.requires_reconciliation());
    assert_eq!(recovered.exposure().unwrap().1.maximum_base, d("1"));
}

#[test]
fn identity_stale_clock_and_incomplete_watermark_fail_closed() {
    for case in 0..6 {
        let (_temp, mut j) = journal();
        prepare(&mut j, "p1", Leg::Primary, HedgeSide::Buy, "1");
        ack(&mut j, "p1");
        terminal(&mut j, "p1", "0", OrderStatus::Canceled, 1000);
        let mut p = proof(&j);
        match case {
            0 => p.legs[0].identity.subaccount = Some(3),
            1 => p.legs[1].position_base = d("0.1"),
            2 => p.legs[0].fills_complete_through_venue_us = 999,
            3 => p.legs[1].request_started_us = 1,
            4 => p.observed_sequence -= 1,
            _ => p.legs[0].open_order_ids.push("unknown".into()),
        }
        assert!(j.reconcile(p, at(&j) + 2).is_err());
        assert!(j.requires_reconciliation());
        assert_eq!(j.exposure().unwrap().1.maximum_base, d("1"));
    }
}

#[test]
fn first_checkpoint_requires_positive_coverage_on_each_venue() {
    for leg in 0..2 {
        let temp = Temp::new();
        let mut j = ExecutionJournal::create(&temp.0, spec(), 100).unwrap();
        let mut p = proof(&j);
        p.legs[leg].fills_complete_through_venue_us = 0;
        let before = j.sequence();
        assert!(j.reconcile(p, at(&j) + 2).is_err());
        assert!(j.requires_reconciliation());
        assert_eq!(j.sequence(), before);
        reconcile(&mut j);
    }
}

#[test]
fn recovered_watermarks_are_monotone_by_venue_and_by_source() {
    for leg in 0..2 {
        let (temp, mut j) = journal();
        let mut anchor = proof(&j);
        anchor.legs[leg].position_venue_at_us = 1000;
        anchor.legs[leg].fills_complete_through_venue_us = 900;
        j.reconcile(anchor, at(&j) + 2).unwrap();
        let spec = j.ledger.spec.clone();
        drop(j);
        let mut j = ExecutionJournal::recover(&temp.0, &spec, 2000).unwrap();
        let before = j.sequence();
        for (position, fills) in [(999, 900), (1000, 899), (1001, 0)] {
            let mut p = proof(&j);
            p.legs[leg].position_venue_at_us = position;
            p.legs[leg].fills_complete_through_venue_us = fills;
            assert!(j.reconcile(p, at(&j) + 2).is_err());
            assert!(j.requires_reconciliation());
            assert_eq!(j.sequence(), before);
        }
        // The other venue can have an independent, much smaller clock.
        let mut p = proof(&j);
        p.legs[leg].position_venue_at_us = 1000;
        p.legs[leg].fills_complete_through_venue_us = 900;
        j.reconcile(p, at(&j) + 2).unwrap();
        assert!(!j.requires_reconciliation());
    }
}

#[test]
fn legacy_checkpoints_remain_auditable_and_cannot_lower_recovery_watermarks() {
    let (temp, mut j) = journal();
    let mut p = proof(&j);
    p.legs[0].position_venue_at_us = 1000;
    p.legs[0].fills_complete_through_venue_us = 900;
    j.reconcile(p, at(&j) + 2).unwrap();
    // Recreate a checksum-valid schema 1 checkpoint from the older writer.
    let mut legacy = proof(&j);
    legacy.legs[0].position_venue_at_us = 1100;
    legacy.legs[0].fills_complete_through_venue_us = 0;
    j.commit(Event::Reconciled(legacy), at(&j) + 2).unwrap();
    let spec = j.ledger.spec.clone();
    drop(j);
    assert!(!audit_journal(&temp.0, &spec).unwrap().restart_allowed);
    let mut j = ExecutionJournal::recover(&temp.0, &spec, 2000).unwrap();
    for (position, fills) in [(1099, 900), (1100, 899)] {
        let mut p = proof(&j);
        p.legs[0].position_venue_at_us = position;
        p.legs[0].fills_complete_through_venue_us = fills;
        assert!(j.reconcile(p, at(&j) + 2).is_err());
        assert!(j.requires_reconciliation());
    }
    let mut p = proof(&j);
    p.legs[0].position_venue_at_us = 1100;
    p.legs[0].fills_complete_through_venue_us = 900;
    j.reconcile(p, at(&j) + 2).unwrap();
}

#[test]
fn fees_keep_venue_and_asset_separate() {
    let (_temp, mut j) = journal();
    prepare(&mut j, "p1", Leg::Primary, HedgeSide::Buy, "1");
    let f = fill(&j, "p1", "t1", "1", 1000);
    j.record_fill(f, at(&j)).unwrap();
    prepare(&mut j, "h1", Leg::Hedge, HedgeSide::Sell, "1");
    let mut f = fill(&j, "h1", "t1", "1", 1000);
    f.fee = d("-0.001");
    j.record_fill(f, at(&j)).unwrap();
    let fees = j.fees_by_asset().unwrap();
    assert_eq!(fees[&(0, "USDG".into())], d("0.001"));
    assert_eq!(fees[&(1, "USDC".into())], d("-0.001"));
}

#[test]
fn account_writer_lock_covers_other_sessions_and_reader_is_read_only() {
    let (temp, j) = journal();
    let mut another = j.ledger.spec.clone();
    another.session_id = uuid::Uuid::new_v4().to_string();
    assert!(ExecutionJournal::create(&temp.0, another, 1000).is_err());
    assert!(audit_journal(&temp.0, &j.ledger.spec).is_err());
    let spec = j.ledger.spec.clone();
    let path = file_path(&temp, &spec);
    drop(j);
    let before = fs::read(&path).unwrap();
    assert!(!audit_journal(&temp.0, &spec).unwrap().restart_allowed);
    assert_eq!(before, fs::read(path).unwrap());
}

#[test]
fn torn_checksum_or_reordered_records_are_rejected_without_truncation() {
    for case in 0..3 {
        let (temp, j) = journal();
        let spec = j.ledger.spec.clone();
        let path = file_path(&temp, &spec);
        drop(j);
        let bytes = fs::read(&path).unwrap();
        let mut bad = bytes.clone();
        if case == 0 {
            bad.pop();
        } else if case == 1 {
            let i = bad.iter().position(|b| *b == b'1').unwrap();
            bad[i] = b'2';
        } else {
            let lines: Vec<_> = bytes.split_inclusive(|b| *b == b'\n').collect();
            bad = [lines[1], lines[0]].concat();
        }
        fs::write(&path, &bad).unwrap();
        assert!(ExecutionJournal::recover(&temp.0, &spec, 1000).is_err());
        assert_eq!(fs::read(&path).unwrap(), bad);
    }
}

#[test]
fn committed_markers_detect_loss_of_a_complete_last_record() {
    let (temp, mut j) = journal();
    prepare(&mut j, "p1", Leg::Primary, HedgeSide::Buy, "1");
    let spec = j.ledger.spec.clone();
    let path = file_path(&temp, &spec);
    drop(j);
    let bytes = fs::read(&path).unwrap();
    let previous_end = bytes[..bytes.len() - 1]
        .iter()
        .rposition(|b| *b == b'\n')
        .unwrap()
        + 1;
    let valid_prefix = &bytes[..previous_end];
    fs::write(&path, valid_prefix).unwrap();
    assert!(ExecutionJournal::recover(&temp.0, &spec, 1000).is_err());
    assert!(audit_journal(&temp.0, &spec).is_err());
    assert_eq!(fs::read(path).unwrap(), valid_prefix);
}

#[test]
fn crash_between_marker_updates_replays_the_full_wal_behind_a_barrier() {
    let (temp, mut j) = journal();
    let marker = fs::read_dir(&temp.0)
        .unwrap()
        .map(|x| x.unwrap().path())
        .find(|p| p.to_string_lossy().ends_with(".active.json"))
        .unwrap();
    let before = fs::read(&marker).unwrap();
    let prepared = prepare(&mut j, "p1", Leg::Primary, HedgeSide::Buy, "1");
    let spec = j.ledger.spec.clone();
    drop(j);
    // One account published the new tip; the other still has the old tip.
    fs::write(&marker, before).unwrap();
    let recovered = ExecutionJournal::recover(&temp.0, &spec, 1000).unwrap();
    assert!(recovered.requires_reconciliation());
    assert!(recovered.prepared_orders() == vec![prepared]);
    assert_eq!(recovered.exposure().unwrap().1.maximum_base, d("1"));
}

#[test]
fn marker_publication_failure_never_returns_a_dispatchable_intent() {
    let (temp, mut j) = journal();
    let marker = fs::read_dir(&temp.0)
        .unwrap()
        .map(|x| x.unwrap().path())
        .find(|p| p.to_string_lossy().ends_with(".active.json"))
        .unwrap();
    let before = fs::read(&marker).unwrap();
    fs::remove_file(&marker).unwrap();
    fs::create_dir(&marker).unwrap(); // inject failure in atomic marker rename
    assert!(
        j.prepare_intent(
            intent("p1", Leg::Primary, HedgeSide::Buy, "1"),
            false,
            None,
            at(&j)
        )
        .is_err()
    );
    assert!(j.requires_reconciliation());
    assert!(j.prepared_orders().is_empty());
    let spec = j.ledger.spec.clone();
    drop(j);
    fs::remove_dir(&marker).unwrap();
    fs::write(&marker, before).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&marker, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let recovered = ExecutionJournal::recover(&temp.0, &spec, 1000).unwrap();
    assert!(recovered.requires_reconciliation());
    // It might have been sent by a crashed caller: recovery reserves it even
    // though this particular caller never received a successful prepare.
    assert_eq!(recovered.prepared_orders().len(), 1);
    assert_eq!(recovered.exposure().unwrap().1.maximum_base, d("1"));
}

#[test]
fn wrong_session_identity_cannot_recover_a_valid_file() {
    let (temp, j) = journal();
    let mut expected = j.ledger.spec.clone();
    drop(j);
    expected.legs[0].identity.market_id = 89;
    assert!(ExecutionJournal::recover(&temp.0, &expected, 1000).is_err());
}

struct FailingSink {
    data: Arc<Mutex<Vec<u8>>>,
    fail_write: Arc<AtomicBool>,
    fail_sync: Arc<AtomicBool>,
    calls: Arc<AtomicUsize>,
}
impl JournalSink for FailingSink {
    fn write_line(&mut self, line: &[u8]) -> io::Result<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_write.load(Ordering::SeqCst) {
            self.data.lock().unwrap().extend_from_slice(&line[..5]);
            return Err(io::Error::other("synthetic write failure"));
        }
        self.data.lock().unwrap().extend_from_slice(line);
        Ok(())
    }
    fn sync(&mut self) -> io::Result<()> {
        if self.fail_sync.load(Ordering::SeqCst) {
            Err(io::Error::other("synthetic sync failure"))
        } else {
            Ok(())
        }
    }
}

#[test]
fn write_or_sync_failure_never_returns_an_intent_and_poison_stops_all_later_writes() {
    for write_failure in [true, false] {
        let data = Arc::new(Mutex::new(vec![]));
        let fail_write = Arc::new(AtomicBool::new(false));
        let fail_sync = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let spec = spec();
        let writer = storage::Writer::with_sink(Box::new(FailingSink {
            data: data.clone(),
            fail_write: fail_write.clone(),
            fail_sync: fail_sync.clone(),
            calls: calls.clone(),
        }));
        let mut j = ExecutionJournal {
            ledger: Ledger::new(spec.clone()).unwrap(),
            writer,
            recovery_required: true,
            recovery_after_us: 100,
            financial_recovery_after_us: 100,
        };
        j.commit(Event::Start(spec), 100).unwrap();
        reconcile(&mut j);
        if write_failure {
            fail_write.store(true, Ordering::SeqCst);
        } else {
            fail_sync.store(true, Ordering::SeqCst);
        }
        let mut fake_sends = 0;
        if j.prepare_intent(
            intent("p1", Leg::Primary, HedgeSide::Buy, "1"),
            false,
            None,
            at(&j),
        )
        .is_ok()
        {
            fake_sends += 1;
        }
        assert_eq!(fake_sends, 0);
        assert!(j.requires_reconciliation());
        let count = calls.load(Ordering::SeqCst);
        fail_write.store(false, Ordering::SeqCst);
        fail_sync.store(false, Ordering::SeqCst);
        assert!(
            j.prepare_intent(
                intent("p2", Leg::Primary, HedgeSide::Buy, "1"),
                false,
                None,
                at(&j)
            )
            .is_err()
        );
        assert!(j.reconcile(proof(&j), at(&j) + 2).is_err());
        assert_eq!(calls.load(Ordering::SeqCst), count);
    }
}

#[cfg(unix)]
#[test]
fn journal_permissions_symlinks_and_hardlinks_are_checked() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let (temp, j) = journal();
    let spec = j.ledger.spec.clone();
    let path = file_path(&temp, &spec);
    drop(j);
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let copy = temp.0.join("copy");
    fs::hard_link(&path, &copy).unwrap();
    assert!(audit_journal(&temp.0, &spec).is_err());
    fs::remove_file(&copy).unwrap();
    fs::rename(&path, &copy).unwrap();
    symlink(&copy, &path).unwrap();
    assert!(audit_journal(&temp.0, &spec).is_err());
    fs::remove_file(&path).unwrap();
    fs::rename(&copy, &path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(audit_journal(&temp.0, &spec).is_err());
}

#[test]
fn crash_cannot_be_bypassed_by_creating_a_new_session_uuid() {
    let (temp, mut j) = journal();
    prepare(&mut j, "p1", Leg::Primary, HedgeSide::Buy, "1");
    let old = j.ledger.spec.clone();
    let mut new = old.clone();
    new.session_id = uuid::Uuid::new_v4().to_string();
    drop(j);
    assert!(ExecutionJournal::create(&temp.0, new, 1000).is_err());
    let recovered = ExecutionJournal::recover(&temp.0, &old, 1000).unwrap();
    assert!(recovered.requires_reconciliation());
    assert_eq!(recovered.prepared_orders().len(), 1);
}

#[test]
fn only_a_durable_fully_flat_close_releases_account_markers() {
    let (temp, mut j) = journal();
    let original = j.ledger.spec.clone();
    j.close_session(at(&j)).unwrap();
    assert!(j.requires_reconciliation());
    assert!(
        j.prepare_intent(
            intent("late", Leg::Primary, HedgeSide::Buy, "1"),
            false,
            None,
            at(&j)
        )
        .is_err()
    );
    drop(j);
    let bytes = fs::read(file_path(&temp, &original)).unwrap();
    let mut next = original.clone();
    next.session_id = uuid::Uuid::new_v4().to_string();
    let next_writer = ExecutionJournal::create(&temp.0, next, 1000).unwrap();
    assert!(next_writer.requires_reconciliation());
    assert_eq!(fs::read(file_path(&temp, &original)).unwrap(), bytes);
}

#[test]
fn paired_net_flat_positions_are_not_closed_accounts() {
    let (_temp, mut j) = journal();
    prepare(&mut j, "p1", Leg::Primary, HedgeSide::Buy, "1");
    let f = fill(&j, "p1", "t1", "1", 1000);
    j.record_fill(f, at(&j)).unwrap();
    terminal(&mut j, "p1", "1", OrderStatus::Filled, 1001);
    prepare(&mut j, "h1", Leg::Hedge, HedgeSide::Sell, "1");
    let f = fill(&j, "h1", "t2", "1", 1000);
    j.record_fill(f, at(&j)).unwrap();
    terminal(&mut j, "h1", "1", OrderStatus::Filled, 1001);
    reconcile(&mut j);
    assert_eq!(j.positions(), [d("1"), d("-1")]);
    assert!(j.close_session(at(&j)).is_err());
}

#[test]
fn crash_between_closed_record_and_marker_cleanup_can_finish_without_resuming() {
    let (temp, mut j) = journal();
    let spec = j.ledger.spec.clone();
    j.commit(Event::Closed, at(&j)).unwrap();
    drop(j);
    let lock = fs::read_dir(&temp.0)
        .unwrap()
        .map(|x| x.unwrap().path())
        .find(|p| p.to_string_lossy().ends_with(".active.json"))
        .unwrap();
    fs::remove_file(lock).unwrap(); // synthetic crash after clearing only one marker
    let mut recovered = ExecutionJournal::recover(&temp.0, &spec, 1000).unwrap();
    assert!(recovered.requires_reconciliation());
    recovered.close_session(1001).unwrap();
    drop(recovered);
    let mut next = spec;
    next.session_id = uuid::Uuid::new_v4().to_string();
    assert!(ExecutionJournal::create(&temp.0, next, 1100).is_ok());
}

#[test]
fn reduce_only_reserves_direction_against_other_pending_closes() {
    let (_temp, mut j) = journal();
    prepare(&mut j, "buy", Leg::Primary, HedgeSide::Buy, "1");
    let f = fill(&j, "buy", "b1", "1", 1000);
    j.record_fill(f, at(&j)).unwrap();
    terminal(&mut j, "buy", "1", OrderStatus::Filled, 1001);
    reconcile(&mut j);
    j.prepare_intent(
        intent("close1", Leg::Primary, HedgeSide::Sell, "0.7"),
        true,
        Some(d("100")),
        at(&j),
    )
    .unwrap();
    ack(&mut j, "close1");
    assert!(
        j.prepare_intent(
            intent("close2", Leg::Primary, HedgeSide::Sell, "0.7"),
            true,
            Some(d("100")),
            at(&j)
        )
        .is_err()
    );
    assert_eq!(j.exposure().unwrap().0[0].minimum_base, d("0.3"));
}

#[test]
fn out_of_order_fills_preserve_position_but_withhold_cycle_attribution() {
    let (_temp, mut j) = journal();
    prepare(&mut j, "p1", Leg::Primary, HedgeSide::Buy, "1");
    let first = fill(&j, "p1", "t1", "0.4", 1000);
    j.record_fill(first, at(&j)).unwrap();
    let older = fill(&j, "p1", "t2", "0.3", 999);
    let effect = j.record_fill(older, at(&j)).unwrap().unwrap();
    assert_eq!(j.positions()[0], d("0.7"));
    assert_eq!(effect.opened_base, None);
    assert_eq!(effect.closed_base, None);
    assert!(j.requires_reconciliation());
    assert_eq!(j.fees_by_asset().unwrap()[&(0, "USDG".into())], d("0.002"));
}

#[test]
fn reused_exchange_order_id_and_wrong_account_do_not_change_mapping() {
    let (_temp, mut j) = journal();
    prepare(&mut j, "p1", Leg::Primary, HedgeSide::Buy, "1");
    ack(&mut j, "p1");
    prepare(&mut j, "p2", Leg::Primary, HedgeSide::Buy, "1");
    let ack = OrderAcknowledgement {
        identity: j.ledger.spec.legs[0].identity.clone(),
        logical_id: "p2".into(),
        exchange_order_id: "order-p1".into(),
    };
    assert!(j.acknowledge(ack, at(&j)).is_err());
    assert!(j.ledger.orders["p2"].exchange_id.is_none());
    let mut f = fill(&j, "p1", "t1", "1", 1000);
    f.identity.subaccount = Some(3);
    assert!(j.record_fill(f, at(&j)).is_err());
    assert_eq!(j.positions(), [Decimal::ZERO; 2]);
    assert!(j.requires_reconciliation());
}

#[test]
fn r2_equal_timestamp_multi_fill_preserves_state_without_halting_while_reversed_or_late_halts() {
    let (_temp, mut j) = journal();
    prepare(&mut j, "h1", Leg::Hedge, HedgeSide::Buy, "2");
    let e1 = j
        .record_fill(fill(&j, "h1", "t1", "0.4", 1000), at(&j))
        .unwrap()
        .unwrap();
    assert_eq!(e1.opened_base, Some(d("0.4")));
    assert_eq!(e1.closed_base, Some(Decimal::ZERO));
    assert_eq!(e1.crossed_zero, Some(false));
    assert!(!j.requires_reconciliation());

    // Second fill in the exact same millisecond leaves attribution None without halting.
    let mut f2 = fill(&j, "h1", "t2", "0.6", 1000);
    f2.fee = d("0.002");
    let e2 = j.record_fill(f2, at(&j)).unwrap().unwrap();
    assert_eq!(e2.before_base, d("0.4"));
    assert_eq!(e2.after_base, d("1.0"));
    assert_eq!(e2.opened_base, None);
    assert_eq!(e2.closed_base, None);
    assert_eq!(e2.crossed_zero, None);
    assert!(!j.requires_reconciliation());
    assert_eq!(j.fees_by_asset().unwrap()[&(1, "USDC".into())], d("0.003"));

    // Can still prepare and settle further orders without hitting recovery.
    prepare(&mut j, "p1", Leg::Primary, HedgeSide::Sell, "1");
    j.record_fill(fill(&j, "p1", "a1", "1", 1000), at(&j))
        .unwrap();
    terminal(&mut j, "h1", "1", OrderStatus::Canceled, 1001);
    terminal(&mut j, "p1", "1", OrderStatus::Filled, 1001);
    reconcile(&mut j);

    // Late fill at the same terminal timestamp MUST still trigger recovery.
    let e_late = j
        .record_fill(fill(&j, "h1", "t3", "0.2", 1001), at(&j))
        .unwrap()
        .unwrap();
    assert_eq!(e_late.opened_base, None);
    assert!(j.requires_reconciliation());
    terminal(&mut j, "h1", "1.2", OrderStatus::Canceled, 1002);
    reconcile(&mut j);

    // Reversed fill timestamp strictly below a prior fill MUST trigger recovery.
    prepare(&mut j, "h2", Leg::Hedge, HedgeSide::Buy, "0.5");
    let e_rev = j
        .record_fill(fill(&j, "h2", "t4", "0.5", 999), at(&j))
        .unwrap()
        .unwrap();
    assert_eq!(e_rev.opened_base, None);
    assert!(j.requires_reconciliation());
}

#[test]
fn r3_arcus_tiered_ticks_and_hyperliquid_price_precision_are_enforced() {
    use crate::exchanges::arcus::TickTier;
    let mut s = spec();
    s.legs[0].price_rules = VenuePriceRules::Arcus {
        tick_size: d("0.01"),
        tick_tiers: vec![
            TickTier {
                tick: d("0.01"),
                up_to_price: Some(d("100")),
            },
            TickTier {
                tick: d("0.1"),
                up_to_price: None,
            },
        ],
    };
    let temp = Temp::new();
    let mut j = ExecutionJournal::create(&temp.0, s, 100).unwrap();
    reconcile(&mut j);

    // Invalid price attempts fail without advancing sequence; ledger is flat so reconcile clears.
    let mut bad_high = intent("a_bad_high", Leg::Primary, HedgeSide::Buy, "1");
    bad_high.limit_price = d("100.05");
    assert!(j.prepare_intent(bad_high, false, None, at(&j)).is_err());
    reconcile(&mut j);

    let mut hl_sigfigs = intent("h_sig", Leg::Hedge, HedgeSide::Sell, "1");
    hl_sigfigs.limit_price = d("100.123"); // 6 sig figs -> invalid on HL
    assert!(
        j.prepare_intent(hl_sigfigs, false, Some(d("100.123")), at(&j))
            .is_err()
    );
    reconcile(&mut j);

    let mut hl_decimals = intent("h_dec", Leg::Hedge, HedgeSide::Sell, "1");
    hl_decimals.limit_price = d("0.00123"); // scale 5 > 6 - 2 -> invalid on HL
    assert!(
        j.prepare_intent(hl_decimals, false, Some(d("0.00123")), at(&j))
            .is_err()
    );
    reconcile(&mut j);

    // Arcus <= 100 allows 0.01 tick; > 100 requires 0.1 tick.
    let mut ok_low = intent("a_ok_low", Leg::Primary, HedgeSide::Buy, "1");
    ok_low.limit_price = d("99.99");
    j.prepare_intent(ok_low, false, None, at(&j)).unwrap();
    ack(&mut j, "a_ok_low");

    let mut ok_high = intent("a_ok_high", Leg::Primary, HedgeSide::Buy, "1");
    ok_high.limit_price = d("100.1");
    j.prepare_intent(ok_high, false, None, at(&j)).unwrap();
    ack(&mut j, "a_ok_high");

    // TradeXYZ sz_decimals = 2 allows scale <= 4 and <= 5 sig figs if fractional; integers exempt.
    let mut hl_ok = intent("h_ok", Leg::Hedge, HedgeSide::Sell, "1");
    hl_ok.limit_price = d("100.12");
    j.prepare_intent(hl_ok, false, Some(d("100.12")), at(&j))
        .unwrap();
    ack(&mut j, "h_ok");

    let mut hl_int = intent("h_int", Leg::Hedge, HedgeSide::Sell, "0.01");
    hl_int.limit_price = d("99999");
    j.prepare_intent(hl_int, false, Some(d("99999")), at(&j))
        .unwrap();
}

#[test]
fn r3_quote_budget_reserves_worst_limit_plus_fee_and_quarantines_unsettled_gains() {
    let mut s = spec();
    s.max_unhedged_base = d("10");
    s.legs[1].max_position_base = d("10");
    s.legs[1].max_order_notional = d("600");
    s.legs[1].quote_budget = d("501.224100");
    s.legs[1].max_fee_rate = d("0.0005");
    let temp = Temp::new();
    let mut j = ExecutionJournal::create(&temp.0, s, 100).unwrap();
    reconcile(&mut j);

    // 5 * 100.23 * 1.0005 = 501.400575 > 501.224100 -> rejected.
    let mut too_high = intent("h_high", Leg::Hedge, HedgeSide::Buy, "5");
    too_high.limit_price = d("100.23");
    assert!(j.prepare_intent(too_high, false, None, at(&j)).is_err());
    assert_eq!(j.available_quote().unwrap()[1], d("501.224100"));
    reconcile(&mut j);

    // 5 * 100.10 * 1.0005 = 500.75025 <= 501.224100 -> admitted.
    let mut ok = intent("h_ok", Leg::Hedge, HedgeSide::Buy, "5");
    ok.limit_price = d("100.10");
    j.prepare_intent(ok, false, None, at(&j)).unwrap();
    assert_eq!(j.available_quote().unwrap()[1], d("0.473850"));

    // Partial fill 2 @ 100.00 + 0.10 fee; remaining 3 reserved @ 100.10 * 1.0005 = 300.45015.
    let mut part = fill(&j, "h_ok", "hf1", "2", 1000);
    part.fee = d("0.10");
    j.record_fill(part, at(&j)).unwrap();
    assert_eq!(j.available_quote().unwrap()[1], d("0.673950"));

    // Cannot open another non-reduce order while residual budget is 0.67395.
    assert!(
        j.prepare_intent(
            intent("h_extra", Leg::Hedge, HedgeSide::Buy, "0.01"),
            false,
            None,
            at(&j)
        )
        .is_err()
    );
    terminal(&mut j, "h_ok", "2", OrderStatus::Canceled, 1001);
    reconcile(&mut j);

    // Finish remaining 3 @ 100.00 + 0.15 fee, reconcile settled +5 ANT.
    prepare(&mut j, "h_rest", Leg::Hedge, HedgeSide::Buy, "3");
    let mut f_rest = fill(&j, "h_rest", "hf2", "3", 1002);
    f_rest.fee = d("0.15");
    j.record_fill(f_rest, at(&j)).unwrap();
    terminal(&mut j, "h_rest", "3", OrderStatus::Filled, 1003);
    reconcile(&mut j);
    assert_eq!(j.available_quote().unwrap()[1], d("0.974100"));

    // reduce_only Sell 5 @ 99.50 (loss 2.50 + fee 0.24875) succeeds without extra full notional,
    // leaving positive equity (498.22535), while still blocking new opens until settled!
    let mut close_it = intent("h_close", Leg::Hedge, HedgeSide::Sell, "5");
    close_it.limit_price = d("99.50");
    j.prepare_intent(close_it, true, Some(d("99.50")), at(&j))
        .unwrap();
    let mut h_close_fill = fill(&j, "h_close", "hf3", "5", 1004);
    h_close_fill.price = d("99.50");
    h_close_fill.fee = d("0.24875");
    j.record_fill(h_close_fill, at(&j)).unwrap();
    terminal(&mut j, "h_close", "5", OrderStatus::Filled, 1004);
    assert_eq!(j.available_quote().unwrap()[1], d("0.725350"));

    // Unsettled gain quarantine test on Leg 0: buy 1 @ 100, sell 1 @ 105 -> +5 waits for reconcile!
    let avail_before = j.available_quote().unwrap()[0];
    prepare(&mut j, "p_buy", Leg::Primary, HedgeSide::Buy, "1");
    let mut pb = fill(&j, "p_buy", "pf1", "1", 1000);
    pb.fee = Decimal::ZERO;
    j.record_fill(pb, at(&j)).unwrap();
    terminal(&mut j, "p_buy", "1", OrderStatus::Filled, 1001);
    let mut p_sell = intent("p_sell", Leg::Primary, HedgeSide::Sell, "1");
    p_sell.limit_price = d("105");
    j.prepare_intent(p_sell, true, Some(d("105")), at(&j))
        .unwrap();
    let mut ps = fill(&j, "p_sell", "pf2", "1", 1002);
    ps.price = d("105");
    ps.fee = Decimal::ZERO;
    j.record_fill(ps, at(&j)).unwrap();
    terminal(&mut j, "p_sell", "1", OrderStatus::Filled, 1003);
    assert_eq!(j.available_quote().unwrap()[0], avail_before - d("100"));
    reconcile(&mut j);
    assert_eq!(j.available_quote().unwrap()[0], avail_before + d("5"));
    assert_eq!(j.available_quote().unwrap()[1], d("498.225350"));
}

#[test]
fn r3_schema_1_legacy_audit_and_closed_cleanup_remain_compatible() {
    use super::model::{LegacyEvent, LegacyLegLimits, LegacyPreparedOrder, LegacySessionSpec};
    use serde::Serialize;
    use sha2::{Digest, Sha256};

    #[derive(Serialize)]
    struct LegacyPayload<'a> {
        schema: u8,
        sequence: u64,
        previous_checksum: &'a str,
        written_at_us: u64,
        record: &'a LegacyEvent,
    }
    #[derive(Serialize)]
    struct LegacyLine<'a> {
        payload: LegacyPayload<'a>,
        checksum: String,
    }

    let s = spec();
    let v1_spec = LegacySessionSpec {
        session_id: s.session_id.clone(),
        legs: [
            LegacyLegLimits {
                identity: s.legs[0].identity.clone(),
                quantity_step: s.legs[0].quantity_step,
                max_position_base: s.legs[0].max_position_base,
                max_order_notional: s.legs[0].max_order_notional,
            },
            LegacyLegLimits {
                identity: s.legs[1].identity.clone(),
                quantity_step: s.legs[1].quantity_step,
                max_position_base: s.legs[1].max_position_base,
                max_order_notional: s.legs[1].max_order_notional,
            },
        ],
        max_unhedged_base: s.max_unhedged_base,
        max_observation_age_us: 1_000,
    };
    let mut s_compat = s.clone();
    s_compat.max_observation_age_us = 1_000;

    let mut p_in = intent("legacy-p1", Leg::Primary, HedgeSide::Buy, "0.5");
    // Unrounded 6-decimal price that schema 2 would reject, valid in schema 1 audit.
    p_in.limit_price = d("100.123456");
    let rec1 = LegacyEvent::Start(v1_spec);
    let rec2 = LegacyEvent::Reconciled(Reconciliation {
        observed_sequence: 1,
        legs: std::array::from_fn(|i| PositionEvidence {
            identity: s.legs[i].identity.clone(),
            request_started_us: 100,
            received_at_us: 100,
            position_venue_at_us: 90,
            fills_complete_through_venue_us: 90,
            covered_trade_ids: vec![],
            open_order_ids: vec![],
            position_base: Decimal::ZERO,
        }),
    });
    let rec3 = LegacyEvent::Prepared(LegacyPreparedOrder {
        intent: p_in,
        wire_client_id: format!("a{}", "b".repeat(32)),
        reduce_only: false,
    });
    let rec4 = LegacyEvent::Terminal(OrderTerminal {
        identity: s.legs[0].identity.clone(),
        logical_id: "legacy-p1".into(),
        exchange_order_id: None,
        status: OrderStatus::Rejected,
        cumulative_filled: Decimal::ZERO,
        venue_at_us: 150,
    });
    // Reconcile at t=1400 with receipt at t=500 (after rec4's 400), then Close at t=2000:
    // written_at gap 600 <= 1000, actual receipt gap 1500 > 1000 -> valid legacy schema 1 Closed record!
    let rec5 = LegacyEvent::Reconciled(Reconciliation {
        observed_sequence: 4,
        legs: std::array::from_fn(|i| PositionEvidence {
            identity: s.legs[i].identity.clone(),
            request_started_us: 450,
            received_at_us: 500,
            position_venue_at_us: 200,
            fills_complete_through_venue_us: 200,
            covered_trade_ids: vec![],
            open_order_ids: vec![],
            position_base: Decimal::ZERO,
        }),
    });
    let rec6 = LegacyEvent::Closed;

    let temp = Temp::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let mut dir = fs::DirBuilder::new();
        dir.mode(0o700);
        dir.create(&temp.0).unwrap();
        let mut out = String::new();
        let mut prev = String::new();
        for (idx, (ts, rec)) in [
            (100_u64, &rec1),
            (200, &rec2),
            (300, &rec3),
            (400, &rec4),
            (1400, &rec5),
            (2000, &rec6),
        ]
        .into_iter()
        .enumerate()
        {
            let payload = LegacyPayload {
                schema: 1,
                sequence: (idx + 1) as u64,
                previous_checksum: &prev,
                written_at_us: ts,
                record: rec,
            };
            let checksum = hex::encode(Sha256::digest(serde_json::to_vec(&payload).unwrap()));
            let line = serde_json::to_string(&LegacyLine {
                payload,
                checksum: checksum.clone(),
            })
            .unwrap();
            out.push_str(&line);
            out.push('\n');
            prev = checksum;
        }
        let jpath = file_path(&temp, &s_compat);
        fs::write(&jpath, out).unwrap();
        fs::set_permissions(&jpath, fs::Permissions::from_mode(0o600)).unwrap();

        // Simulate crash before .active.json marker cleanup on the already-closed schema 1 journal.
        let mut keys = Vec::new();
        for leg in &s_compat.legs {
            let id = &leg.identity;
            keys.push(hex::encode(Sha256::digest(
                serde_json::to_vec(&(&id.venue, &id.network, &id.account, id.subaccount)).unwrap(),
            )));
        }
        for key in keys {
            let marker = temp.0.join(format!("{key}.active.json"));
            fs::write(
                &marker,
                format!(
                    "{{\"schema\":1,\"session_id\":\"{}\",\"sequence\":6,\"checksum\":\"{}\"}}\n",
                    s_compat.session_id, prev
                ),
            )
            .unwrap();
            fs::set_permissions(&marker, fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    let audited = audit_journal(&temp.0, &s_compat).unwrap();
    assert!(!audited.restart_allowed);
    assert_eq!(audited.records, 6);
    assert_eq!(audited.unverified_orders, 0);

    let mut recovered = ExecutionJournal::recover(&temp.0, &s_compat, 2100).unwrap();
    assert!(recovered.requires_reconciliation());
    recovered.close_session(2101).unwrap();
    drop(recovered);

    let mut next = s_compat;
    next.session_id = uuid::Uuid::new_v4().to_string();
    assert!(ExecutionJournal::create(&temp.0, next, 2200).is_ok());
}

#[test]
fn r4_new_close_checks_earliest_reconciliation_receipt_timestamp() {
    let mut s = spec();
    s.max_observation_age_us = 1_000_000;
    let temp = Temp::new();
    let mut j = ExecutionJournal::create(&temp.0, s, 1_000_000).unwrap();

    // Leg 0 receipt at 1_100_000, Leg 1 receipt at 1_900_000, reconcile written at 2_000_000:
    // Reconcile passes (2_000_000 - 1_100_000 = 900_000 <= 1_000_000).
    let mut p = proof(&j);
    p.legs[0].request_started_us = 1_050_000;
    p.legs[0].received_at_us = 1_100_000;
    p.legs[1].request_started_us = 1_850_000;
    p.legs[1].received_at_us = 1_900_000;
    j.reconcile(p, 2_000_000).unwrap();

    // Close at 2_200_000: written_at gap = 200_000 <= 1_000_000,
    // BUT earliest receipt gap = 2_200_000 - 1_100_000 = 1_100_000 > 1_000_000 -> MUST fail!
    assert!(j.close_session(2_200_000).is_err());
    assert!(j.requires_reconciliation());

    // Re-reconcile with fresh receipts on both legs, then close within 1s -> succeeds!
    let mut fresh = proof(&j);
    for leg in &mut fresh.legs {
        leg.request_started_us = 2_210_000;
        leg.received_at_us = 2_250_000;
    }
    j.reconcile(fresh, 2_260_000).unwrap();
    j.close_session(2_300_000).unwrap();
}

#[test]
fn r5_concurrent_account_lock_creation_tolerates_already_exists_and_rejects_symlinks() {
    use std::sync::Barrier;

    let temp = Temp::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt, symlink};
        let mut dir = fs::DirBuilder::new();
        dir.mode(0o700);
        dir.create(&temp.0).unwrap();

        let lock_path = temp.0.join("shared.account.lock");
        let barrier = Arc::new(Barrier::new(4));
        let mut handles = Vec::new();
        for _ in 0..4 {
            let b = Arc::clone(&barrier);
            let p = lock_path.clone();
            handles.push(std::thread::spawn(move || {
                b.wait();
                let f = storage::lock_file(&p)?;
                Ok::<_, anyhow::Error>(f.try_lock().is_ok())
            }));
        }
        let mut locked_count = 0;
        for h in handles {
            let ok = h.join().unwrap().unwrap();
            if ok {
                locked_count += 1;
            }
        }
        // Only one live flock holder wins while locks are held concurrently (here sequential drop
        // may allow later threads after prior thread exits, so all threads succeeded opening safely
        // with zero AlreadyExists errors and at least 1 locked).
        assert!(locked_count >= 1);

        // Now verify holding lock prevents second writer cleanly across concurrent sessions.
        let barrier2 = Arc::new(Barrier::new(2));
        let s1 = spec();
        let mut s2 = s1.clone();
        s2.session_id = uuid::Uuid::new_v4().to_string();
        let race_dir = Temp::new();
        let dir1 = race_dir.0.clone();
        let dir2 = race_dir.0.clone();
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&race_dir.0)
            .unwrap();
        let b1 = Arc::clone(&barrier2);
        let b2 = Arc::clone(&barrier2);
        let h1 = std::thread::spawn(move || {
            b1.wait();
            ExecutionJournal::create(&dir1, s1, 100)
        });
        let h2 = std::thread::spawn(move || {
            b2.wait();
            ExecutionJournal::create(&dir2, s2, 100)
        });
        let r1 = h1.join().unwrap();
        let r2 = h2.join().unwrap();
        assert_eq!(usize::from(r1.is_ok()) + usize::from(r2.is_ok()), 1);

        // Symlink or permissive existing lock_path hits AlreadyExists -> private_file rejects it!
        let target = temp.0.join("real");
        fs::write(&target, b"").unwrap();
        let sym = temp.0.join("sym.account.lock");
        symlink(&target, &sym).unwrap();
        assert!(storage::lock_file(&sym).is_err());

        let perm = temp.0.join("perm.account.lock");
        fs::write(&perm, b"").unwrap();
        fs::set_permissions(&perm, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(storage::lock_file(&perm).is_err());
    }
}

#[test]
fn r3a_sell_requires_outer_price_bound_and_recovers_on_price_improvement_breach() {
    let mut s = spec();
    s.legs[1].max_order_notional = d("500");
    s.legs[1].quote_budget = d("100.05");
    s.legs[1].max_fee_rate = d("0.0005");

    // 1. Sell validation rejects missing/invalid outer price bounds and budget/notional breaches.
    {
        let (_temp, mut j) = journal_with(s.clone());
        let sell = intent("s_none", Leg::Hedge, HedgeSide::Sell, "1");
        assert!(j.prepare_intent(sell.clone(), false, None, at(&j)).is_err());
        reconcile(&mut j);
        assert!(
            j.prepare_intent(sell.clone(), false, Some(d("99")), at(&j))
                .is_err()
        );
        reconcile(&mut j);
        assert!(
            j.prepare_intent(sell.clone(), false, Some(d("101")), at(&j))
                .is_err()
        );
        assert_eq!(j.available_quote().unwrap()[1], d("100.05"));
        reconcile(&mut j);
        assert!(
            j.prepare_intent(sell, false, Some(d("501")), at(&j))
                .is_err()
        );
    }

    // 2. Proven outer bound = 101 with sufficient budget reserves worst notional + fee and clears cleanly.
    {
        let mut s_ok = s.clone();
        s_ok.legs[1].quote_budget = d("101.0505");
        let (_temp, mut j) = journal_with(s_ok);
        let sell = intent("s_ok", Leg::Hedge, HedgeSide::Sell, "1");
        j.prepare_intent(sell, false, Some(d("101")), at(&j))
            .unwrap();
        assert!(j.available_quote().unwrap()[1].is_zero());
        let mut f = fill(&j, "s_ok", "f_ok", "1", 1000);
        f.price = d("101");
        f.fee = d("0.05");
        j.record_fill(f, at(&j)).unwrap();
        assert_eq!(j.available_quote().unwrap()[1], d("0.0005"));
        assert!(!j.requires_reconciliation());
    }

    // 3. Codex counterexample: tight outer bound (100) fills at 101 (+0.05 USDC).
    // The real fill MUST be preserved cleanly AND force recovery_required = true.
    {
        let (_temp, mut j) = journal_with(s.clone());
        let sell = intent("peer", Leg::Hedge, HedgeSide::Sell, "1");
        j.prepare_intent(sell, false, Some(d("100")), at(&j))
            .unwrap();
        assert!(j.available_quote().unwrap()[1].is_zero());

        let mut f = fill(&j, "peer", "fill", "1", 1000);
        f.price = d("101");
        f.fee = d("0.05");
        j.record_fill(f, at(&j)).unwrap();
        assert_eq!(j.positions()[1], d("-1"));
        assert_eq!(j.fees_by_asset().unwrap()[&(1, "USDC".into())], d("0.05"));
        assert_eq!(j.available_quote().unwrap()[1], d("-1"));
        assert!(j.requires_reconciliation());

        terminal(&mut j, "peer", "1", OrderStatus::Filled, 1001);
        reconcile(&mut j);
        // Even after reconcile, negative available quote still blocks new non-reduce opens.
        assert!(
            j.prepare_intent(
                intent("blocked_open", Leg::Hedge, HedgeSide::Sell, "0.1"),
                false,
                Some(d("100")),
                at(&j)
            )
            .is_err()
        );
        reconcile(&mut j);

        // reduce_only Buy flattens the -1 short and allows clean close.
        j.prepare_intent(
            intent("close_short", Leg::Hedge, HedgeSide::Buy, "1"),
            true,
            None,
            at(&j),
        )
        .unwrap();
        let mut fc = fill(&j, "close_short", "f_close", "1", 1002);
        fc.price = d("100");
        fc.fee = Decimal::ZERO;
        j.record_fill(fc, at(&j)).unwrap();
        terminal(&mut j, "close_short", "1", OrderStatus::Filled, 1003);
        reconcile(&mut j);
        j.close_session(at(&j)).unwrap();
    }

    // 4. reduce_only Sell reserves worst_close_loss from limit_price (99 -> loss 1)
    // AND close_fees from proven max_fill_price (110 * 0.01 = 1.10).
    {
        let mut s_ro = s;
        s_ro.legs[1].quote_budget = d("102.10");
        s_ro.legs[1].max_fee_rate = d("0.01");
        let (_temp, mut j) = journal_with(s_ro);
        prepare(&mut j, "open_long", Leg::Hedge, HedgeSide::Buy, "1");
        let mut fo = fill(&j, "open_long", "fo", "1", 1000);
        fo.fee = Decimal::ZERO;
        j.record_fill(fo, at(&j)).unwrap();
        terminal(&mut j, "open_long", "1", OrderStatus::Filled, 1001);
        reconcile(&mut j);
        assert_eq!(j.available_quote().unwrap()[1], d("2.10"));

        let mut close_long = intent("close_long", Leg::Hedge, HedgeSide::Sell, "1");
        close_long.limit_price = d("99");
        j.prepare_intent(close_long, true, Some(d("110")), at(&j))
            .unwrap();
        assert_eq!(j.available_quote().unwrap()[1], d("0.00"));
    }
}

fn journal_with(spec: SessionSpec) -> (Temp, ExecutionJournal) {
    let temp = Temp::new();
    let mut j = ExecutionJournal::create(&temp.0, spec, 100).unwrap();
    reconcile(&mut j);
    (temp, j)
}

#[test]
fn r3b_only_matching_quote_asset_fees_enter_quote_budget() {
    let mut s = spec();
    s.legs[1].quote_budget = d("200");
    s.legs[1].max_fee_rate = d("0.0005");

    // 1. Codex counterexample: -1 OTHER rebate never inflates USDC quote budget after reconcile.
    {
        let (_temp, mut j) = journal_with(s.clone());
        prepare(&mut j, "peer", Leg::Hedge, HedgeSide::Buy, "1");
        let mut f = fill(&j, "peer", "fill", "1", 1000);
        f.fee = d("-1");
        f.fee_asset = "OTHER".into();
        j.record_fill(f, at(&j)).unwrap();
        terminal(&mut j, "peer", "1", OrderStatus::Filled, 1001);
        assert!(!j.requires_reconciliation());
        assert_eq!(j.available_quote().unwrap()[1], d("100"));
        reconcile(&mut j);
        assert_eq!(j.available_quote().unwrap()[1], d("100"));
        assert_eq!(j.fees_by_asset().unwrap()[&(1, "OTHER".into())], d("-1"));
    }

    // 2. Control: matching quote-asset rebate (-1 USDC) stays quarantined pre-reconcile and settles +1 after reconcile.
    {
        let (_temp, mut j) = journal_with(s.clone());
        prepare(&mut j, "same_rebate", Leg::Hedge, HedgeSide::Buy, "1");
        let mut f = fill(&j, "same_rebate", "f_usdc", "1", 1000);
        f.fee = d("-1");
        f.fee_asset = "USDC".into();
        j.record_fill(f, at(&j)).unwrap();
        terminal(&mut j, "same_rebate", "1", OrderStatus::Filled, 1001);
        assert!(!j.requires_reconciliation());
        assert_eq!(j.available_quote().unwrap()[1], d("100"));
        reconcile(&mut j);
        assert_eq!(j.available_quote().unwrap()[1], d("101"));
    }

    // 3. Unconverted positive non-quote fee (+1 OTHER): fill/fee preserved, quote budget untouched, recovery forced.
    {
        let (_temp, mut j) = journal_with(s.clone());
        prepare(&mut j, "pos_other", Leg::Hedge, HedgeSide::Buy, "1");
        let mut f = fill(&j, "pos_other", "f_pos", "1", 1000);
        f.fee = d("1");
        f.fee_asset = "OTHER".into();
        j.record_fill(f, at(&j)).unwrap();
        assert!(j.requires_reconciliation());
        assert_eq!(j.positions()[1], d("1"));
        assert_eq!(j.fees_by_asset().unwrap()[&(1, "OTHER".into())], d("1"));
        assert_eq!(j.available_quote().unwrap()[1], d("100"));
    }

    // 4. Zero non-quote fee (0 OTHER): clean preservation without false recovery.
    {
        let (_temp, mut j) = journal_with(s);
        prepare(&mut j, "zero_other", Leg::Hedge, HedgeSide::Buy, "1");
        let mut f = fill(&j, "zero_other", "f_zero", "1", 1000);
        f.fee = Decimal::ZERO;
        f.fee_asset = "OTHER".into();
        j.record_fill(f, at(&j)).unwrap();
        assert!(!j.requires_reconciliation());
        assert_eq!(j.positions()[1], d("1"));
        assert_eq!(j.available_quote().unwrap()[1], d("100"));
    }
}

#[test]
fn r4_schema_2_replay_and_audit_enforce_close_receipt_freshness() {
    use serde::Serialize;
    use sha2::{Digest, Sha256};

    #[derive(Serialize)]
    struct CurrentPayload<'a> {
        schema: u8,
        sequence: u64,
        previous_checksum: &'a str,
        written_at_us: u64,
        record: &'a Event,
    }
    #[derive(Serialize)]
    struct CurrentLine<'a> {
        payload: CurrentPayload<'a>,
        checksum: String,
    }

    fn write_schema_2(spec: &SessionSpec, reconcile_received_us: u64) -> Temp {
        let temp = Temp::new();
        let events = [
            (100_u64, Event::Start(spec.clone())),
            (
                1400,
                Event::Reconciled(Reconciliation {
                    observed_sequence: 1,
                    legs: std::array::from_fn(|i| PositionEvidence {
                        identity: spec.legs[i].identity.clone(),
                        request_started_us: reconcile_received_us - 50,
                        received_at_us: reconcile_received_us,
                        position_venue_at_us: 200,
                        fills_complete_through_venue_us: 200,
                        covered_trade_ids: vec![],
                        open_order_ids: vec![],
                        position_base: Decimal::ZERO,
                    }),
                }),
            ),
            (2000, Event::Closed),
        ];
        #[cfg(unix)]
        {
            use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
            let mut dir = fs::DirBuilder::new();
            dir.mode(0o700);
            dir.create(&temp.0).unwrap();
            let mut out = String::new();
            let mut prev = String::new();
            for (idx, (ts, rec)) in events.iter().enumerate() {
                let payload = CurrentPayload {
                    schema: 2,
                    sequence: (idx + 1) as u64,
                    previous_checksum: &prev,
                    written_at_us: *ts,
                    record: rec,
                };
                let checksum = hex::encode(Sha256::digest(serde_json::to_vec(&payload).unwrap()));
                let line = serde_json::to_string(&CurrentLine {
                    payload,
                    checksum: checksum.clone(),
                })
                .unwrap();
                out.push_str(&line);
                out.push('\n');
                prev = checksum;
            }
            let jpath = file_path(&temp, spec);
            fs::write(&jpath, out).unwrap();
            fs::set_permissions(&jpath, fs::Permissions::from_mode(0o600)).unwrap();
        }
        temp
    }

    let mut s = spec();
    s.max_observation_age_us = 1_000;

    // Stale close in schema 2 (written gap 600 <= 1000, earliest receipt gap 1500 > 1000):
    // both audit and recover MUST reject!
    let stale_temp = write_schema_2(&s, 500);
    #[cfg(unix)]
    {
        assert!(audit_journal(&stale_temp.0, &s).is_err());
        assert!(ExecutionJournal::recover(&stale_temp.0, &s, 2100).is_err());
    }

    // Fresh close in schema 2 (earliest receipt gap 800 <= 1000): both audit and recover accept.
    let fresh_temp = write_schema_2(&s, 1200);
    #[cfg(unix)]
    {
        let audited = audit_journal(&fresh_temp.0, &s).unwrap();
        assert!(!audited.restart_allowed);
        assert_eq!(audited.records, 3);
        let recovered = ExecutionJournal::recover(&fresh_temp.0, &s, 2100).unwrap();
        assert!(recovered.requires_reconciliation());
    }
}

fn fin_at(j: &ExecutionJournal) -> u64 {
    j.ledger
        .written_at_us
        .max(j.recovery_after_us)
        .max(j.financial_recovery_after_us)
        + 10
}

fn financial_proof(
    j: &ExecutionJournal,
    leg0_cost: &str,
    leg1_cost: &str,
) -> FinancialReconciliation {
    let t = fin_at(j);
    let costs = [d(leg0_cost), d(leg1_cost)];
    FinancialReconciliation {
        observed_sequence: j.sequence(),
        legs: std::array::from_fn(|i| {
            let settled_fee_assets = j.ledger.budgets[i]
                .unconverted_charges
                .iter()
                .map(|(asset, amount)| SettledFeeCharge {
                    asset: asset.clone(),
                    amount: *amount,
                })
                .collect();
            let last_fill_venue = j
                .ledger
                .fills
                .iter()
                .filter(|((leg, _), _)| *leg == i)
                .map(|(_, f)| f.venue_at_us)
                .max()
                .unwrap_or(0);
            FinancialEvidence {
                identity: j.ledger.spec.legs[i].identity.clone(),
                request_started_us: t,
                received_at_us: t + 1,
                venue_at_us: last_fill_venue.max(t) + 10,
                settled_fee_assets,
                converted_quote_cost: costs[i],
            }
        }),
    }
}

#[test]
fn r3_unconverted_non_quote_fee_keeps_financial_barrier_across_position_reconcile_restart_and_audit()
 {
    let temp = Temp::new();
    let mut s = spec();
    s.legs[0].quote_budget = d("200");
    let mut j = ExecutionJournal::create(&temp.0, s.clone(), 100).unwrap();
    reconcile(&mut j);

    prepare(&mut j, "p-open", Leg::Primary, HedgeSide::Buy, "1");
    ack(&mut j, "p-open");
    let mut non_quote_fill = fill(&j, "p-open", "t-open", "1", 500);
    non_quote_fill.fee = d("1");
    non_quote_fill.fee_asset = "OTHER".into();
    j.record_fill(non_quote_fill, at(&j)).unwrap();
    terminal(&mut j, "p-open", "1", OrderStatus::Filled, 501);

    assert!(j.requires_reconciliation());
    assert!(j.requires_financial_reconciliation());
    assert_eq!(
        j.unconverted_fees_by_asset(),
        BTreeMap::from([((0, "OTHER".into()), d("1"))])
    );
    assert_eq!(
        j.fees_by_asset().unwrap(),
        BTreeMap::from([((0, "OTHER".into()), d("1"))])
    );
    assert_eq!(j.positions(), [d("1"), d("0")]);
    assert_eq!(j.available_quote().unwrap()[0], d("100"));

    // Position-only reconciliation verifies positions/open orders, NOT non-quote financial fees.
    reconcile(&mut j);
    assert!(!j.requires_reconciliation());
    assert!(j.requires_financial_reconciliation());

    // Risk-increasing exposure MUST remain blocked until independent financial proof,
    // without poisoning position recovery state.
    assert!(
        j.prepare_intent(
            intent("p-more", Leg::Primary, HedgeSide::Buy, "0.5"),
            false,
            None,
            at(&j),
        )
        .is_err()
    );
    assert!(!j.requires_reconciliation());
    assert!(j.requires_financial_reconciliation());

    // Controlled reduce-only close MUST remain permitted once positions are reconciled.
    let close = intent("p-close", Leg::Primary, HedgeSide::Sell, "1");
    let close_bound = Some(close.limit_price);
    j.prepare_intent(close, true, close_bound, at(&j)).unwrap();
    ack(&mut j, "p-close");
    let mut close_fill = fill(&j, "p-close", "t-close", "1", 600);
    close_fill.fee = d("0.01");
    close_fill.fee_asset = "USDG".into();
    j.record_fill(close_fill, at(&j)).unwrap();
    terminal(&mut j, "p-close", "1", OrderStatus::Filled, 601);
    reconcile(&mut j);

    assert_eq!(j.positions(), [d("0"), d("0")]);
    assert!(!j.requires_reconciliation());
    assert!(j.requires_financial_reconciliation());
    assert!(j.close_session(at(&j) + 2).is_err());

    let reopen_ts = fin_at(&j) + 100;
    drop(j);

    let audited = audit_journal(&temp.0, &s).unwrap();
    assert!(!audited.restart_allowed);
    assert!(audited.financial_reconciliation_required);
    assert_eq!(
        audited.unconverted_fees_by_asset,
        BTreeMap::from([((0, "OTHER".into()), d("1"))])
    );

    let mut recovered = ExecutionJournal::recover(&temp.0, &s, reopen_ts).unwrap();
    assert!(recovered.requires_reconciliation());
    assert!(recovered.requires_financial_reconciliation());
    assert_eq!(
        recovered.unconverted_fees_by_asset(),
        BTreeMap::from([((0, "OTHER".into()), d("1"))])
    );
    reconcile(&mut recovered);
    assert!(!recovered.requires_reconciliation());
    assert!(recovered.requires_financial_reconciliation());
    assert!(
        recovered
            .prepare_intent(
                intent("p-after-restart", Leg::Primary, HedgeSide::Buy, "0.5"),
                false,
                None,
                at(&recovered),
            )
            .is_err()
    );
}

#[test]
fn r3_financial_reconciliation_validates_bounds_is_independent_and_persists_quote_cost() {
    let temp = Temp::new();
    let mut s = spec();
    s.legs[0].quote_budget = d("150");
    let mut j = ExecutionJournal::create(&temp.0, s.clone(), 100).unwrap();
    reconcile(&mut j);

    prepare(&mut j, "p1", Leg::Primary, HedgeSide::Buy, "1");
    ack(&mut j, "p1");
    let mut f1 = fill(&j, "p1", "t1", "1", 500);
    f1.fee = d("2");
    f1.fee_asset = "OTHER".into();
    j.record_fill(f1, at(&j)).unwrap();
    terminal(&mut j, "p1", "1", OrderStatus::Filled, 501);

    // Zero converted quote cost for positive unconverted fee MUST fail closed.
    let zero_cost = financial_proof(&j, "0", "0");
    assert!(j.reconcile_financial(zero_cost, fin_at(&j) + 2).is_err());

    // Pre-failure timestamp request MUST fail closed after financial failure advances watermark.
    let mut stale_req = financial_proof(&j, "15", "0");
    stale_req.legs[0].request_started_us = 100;
    stale_req.legs[0].received_at_us = fin_at(&j);
    assert!(j.reconcile_financial(stale_req, fin_at(&j) + 2).is_err());

    // Wrong sequence MUST fail closed.
    let mut bad_seq = financial_proof(&j, "15", "0");
    bad_seq.observed_sequence -= 1;
    assert!(j.reconcile_financial(bad_seq, fin_at(&j) + 2).is_err());

    // Venue timestamp before unconverted fill MUST fail closed.
    let mut early_venue = financial_proof(&j, "15", "0");
    early_venue.legs[0].venue_at_us = 499;
    assert!(j.reconcile_financial(early_venue, fin_at(&j) + 2).is_err());

    // Missing/wrong/duplicate asset MUST fail closed.
    let mut wrong_amt = financial_proof(&j, "15", "0");
    wrong_amt.legs[0].settled_fee_assets[0].amount = d("1");
    assert!(j.reconcile_financial(wrong_amt, fin_at(&j) + 2).is_err());

    let mut dup_asset = financial_proof(&j, "15", "0");
    dup_asset.legs[0].settled_fee_assets.push(SettledFeeCharge {
        asset: "OTHER".into(),
        amount: d("2"),
    });
    assert!(j.reconcile_financial(dup_asset, fin_at(&j) + 2).is_err());

    // Clean leg (leg 1) cannot inject spurious converted fee cost.
    let mut spurious_leg1 = financial_proof(&j, "15", "1");
    assert!(
        j.reconcile_financial(spurious_leg1.clone(), fin_at(&j) + 2)
            .is_err()
    );
    spurious_leg1.legs[1].converted_quote_cost = Decimal::ZERO;
    spurious_leg1.legs[1]
        .settled_fee_assets
        .push(SettledFeeCharge {
            asset: "EXTRA".into(),
            amount: d("1"),
        });
    assert!(
        j.reconcile_financial(spurious_leg1, fin_at(&j) + 2)
            .is_err()
    );

    // Financial proof BEFORE position reconcile clears financial barrier ONLY,
    // keeping position recovery required until reconcile().
    let valid_p1 = financial_proof(&j, "15", "0");
    j.reconcile_financial(valid_p1, fin_at(&j) + 2).unwrap();
    assert!(!j.requires_financial_reconciliation());
    assert!(j.unconverted_fees_by_asset().is_empty());
    assert!(j.requires_reconciliation());
    assert!(
        j.prepare_intent(
            intent("blocked-pos", Leg::Primary, HedgeSide::Buy, "0.1"),
            false,
            None,
            fin_at(&j),
        )
        .is_err()
    );

    // After position reconcile, both barriers are clear and converted $15 fee deducted ($150 - $100 - $15 = $35).
    reconcile(&mut j);
    assert!(!j.requires_reconciliation());
    assert!(!j.requires_financial_reconciliation());
    assert_eq!(j.available_quote().unwrap()[0], d("35"));

    // Second cycle consumes $30 notional with another +1 FUEL unconverted charge.
    prepare(&mut j, "p2", Leg::Primary, HedgeSide::Buy, "0.3");
    ack(&mut j, "p2");
    let mut f2 = fill(&j, "p2", "t2", "0.3", 700);
    f2.fee = d("1");
    f2.fee_asset = "FUEL".into();
    j.record_fill(f2, at(&j)).unwrap();
    terminal(&mut j, "p2", "0.3", OrderStatus::Filled, 701);

    assert!(j.requires_financial_reconciliation());
    reconcile(&mut j);
    let exhaust_proof = financial_proof(&j, "10", "0");
    j.reconcile_financial(exhaust_proof, fin_at(&j) + 2)
        .unwrap();
    assert!(!j.requires_financial_reconciliation());
    // Remaining available quote is $150 - $130 notional - ($15 + $10) converted fees = -$5.
    assert_eq!(j.available_quote().unwrap()[0], d("-5"));
    // Risk-increasing order MUST still be blocked by exhausted quote budget,
    // while reduce-only close succeeds (equity $125 >= 0).
    assert!(
        j.prepare_intent(
            intent("p-over-budget", Leg::Primary, HedgeSide::Buy, "0.1"),
            false,
            None,
            at(&j),
        )
        .is_err()
    );
    assert!(j.requires_reconciliation());
    reconcile(&mut j);

    let close_all = intent("p-close-all", Leg::Primary, HedgeSide::Sell, "1.3");
    let close_bound = Some(close_all.limit_price);
    j.prepare_intent(close_all, true, close_bound, at(&j))
        .unwrap();
    ack(&mut j, "p-close-all");
    let mut f3 = fill(&j, "p-close-all", "t3", "1.3", 800);
    f3.fee = d("0.5");
    f3.fee_asset = "GAS".into();
    j.record_fill(f3, at(&j)).unwrap();
    terminal(&mut j, "p-close-all", "1.3", OrderStatus::Filled, 801);

    // Position reconcile FIRST while flat, then financial reconcile AFTER, then close_session.
    reconcile(&mut j);
    assert_eq!(j.positions(), [d("0"), d("0")]);
    assert!(j.requires_financial_reconciliation());
    let final_fin = financial_proof(&j, "5", "0");
    let fin_now = fin_at(&j) + 2;
    j.reconcile_financial(final_fin, fin_now).unwrap();
    assert!(!j.requires_financial_reconciliation());
    assert_eq!(j.available_quote().unwrap()[0], d("120"));

    let check_recover_ts = fin_at(&j) + 50;
    j.close_session(fin_now + 1).unwrap();
    drop(j);

    let audited = audit_journal(&temp.0, &s).unwrap();
    assert!(!audited.financial_reconciliation_required);
    assert!(audited.unconverted_fees_by_asset.is_empty());

    let recovered = ExecutionJournal::recover(&temp.0, &s, check_recover_ts).unwrap();
    assert!(!recovered.requires_financial_reconciliation());
    assert_eq!(recovered.available_quote().unwrap()[0], d("120"));
}

#[cfg(unix)]
fn write_signed_events(
    s: &SessionSpec,
    schema: u8,
    events: &[(u64, Event)],
    write_active_tips: bool,
) -> Temp {
    use serde::Serialize;
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    #[derive(Serialize)]
    struct PayloadRef<'a> {
        schema: u8,
        sequence: u64,
        previous_checksum: &'a str,
        written_at_us: u64,
        record: &'a Event,
    }
    #[derive(Serialize)]
    struct LineRef<'a> {
        payload: PayloadRef<'a>,
        checksum: String,
    }

    let temp = Temp::new();
    let mut dir = fs::DirBuilder::new();
    dir.mode(0o700);
    dir.create(&temp.0).unwrap();
    let mut out = String::new();
    let mut prev = String::new();
    for (idx, (ts, ev)) in events.iter().enumerate() {
        let payload = PayloadRef {
            schema,
            sequence: (idx + 1) as u64,
            previous_checksum: &prev,
            written_at_us: *ts,
            record: ev,
        };
        let checksum = hex::encode(Sha256::digest(serde_json::to_vec(&payload).unwrap()));
        let line = serde_json::to_string(&LineRef {
            payload,
            checksum: checksum.clone(),
        })
        .unwrap();
        out.push_str(&line);
        out.push('\n');
        prev = checksum;
    }
    let path = file_path(&temp, s);
    fs::write(&path, out).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

    if write_active_tips {
        for leg in &s.legs {
            let id = &leg.identity;
            let key = hex::encode(Sha256::digest(
                serde_json::to_vec(&(&id.venue, &id.network, &id.account, id.subaccount)).unwrap(),
            ));
            let marker = temp.0.join(format!("{key}.active.json"));
            fs::write(
                &marker,
                format!(
                    "{{\"schema\":1,\"session_id\":\"{}\",\"sequence\":{},\"checksum\":\"{}\"}}\n",
                    s.session_id,
                    events.len(),
                    prev
                ),
            )
            .unwrap();
            fs::set_permissions(&marker, fs::Permissions::from_mode(0o600)).unwrap();
        }
    }
    temp
}

#[cfg(unix)]
#[test]
fn r3_replay_and_audit_reject_unconverted_risk_increase_and_tampered_close() {
    let (_create_dir, j_new) = journal();
    assert_eq!(j_new.writer.schema(), storage::SCHEMA_CURRENT);
    drop(j_new);

    let s = spec();
    let base_pos = |ts: u64, covered: Vec<String>, pos: Decimal| {
        std::array::from_fn(|i| PositionEvidence {
            identity: s.legs[i].identity.clone(),
            request_started_us: ts,
            received_at_us: ts + 1,
            position_venue_at_us: if i == 0 { ts } else { 10 },
            fills_complete_through_venue_us: if i == 0 { ts } else { 10 },
            covered_trade_ids: if i == 0 { covered.clone() } else { vec![] },
            open_order_ids: vec![],
            position_base: if i == 0 { pos } else { Decimal::ZERO },
        })
    };

    let open_order = PreparedOrder {
        intent: intent("p1", Leg::Primary, HedgeSide::Buy, "1"),
        wire_client_id: format!("a{}", "1".repeat(32)),
        reduce_only: false,
        max_fill_price: d("100"),
    };
    let fill_other = ExecutionFill {
        identity: s.legs[0].identity.clone(),
        logical_id: "p1".into(),
        exchange_order_id: "oid-1".into(),
        trade_id: "t1".into(),
        venue_at_us: 200,
        quantity: d("1"),
        price: d("100"),
        fee: d("1"),
        fee_asset: "OTHER".into(),
    };
    let term_open = OrderTerminal {
        identity: s.legs[0].identity.clone(),
        logical_id: "p1".into(),
        exchange_order_id: Some("oid-1".into()),
        status: OrderStatus::Filled,
        cumulative_filled: d("1"),
        venue_at_us: 201,
    };

    // Case 1: Tampered schema 3 WAL admits !reduce_only order while +1 OTHER is unsettled.
    let bad_risk_increase = write_signed_events(
        &s,
        storage::SCHEMA_CURRENT,
        &[
            (100, Event::Start(s.clone())),
            (
                202,
                Event::Reconciled(Reconciliation {
                    observed_sequence: 1,
                    legs: base_pos(200, vec![], Decimal::ZERO),
                }),
            ),
            (300, Event::Prepared(open_order.clone())),
            (350, Event::Fill(fill_other.clone())),
            (400, Event::Terminal(term_open.clone())),
            (
                502,
                Event::Reconciled(Reconciliation {
                    observed_sequence: 5,
                    legs: base_pos(500, vec!["t1".into()], d("1")),
                }),
            ),
            (
                600,
                Event::Prepared(PreparedOrder {
                    intent: intent("p2-illegal", Leg::Primary, HedgeSide::Buy, "0.5"),
                    wire_client_id: format!("a{}", "2".repeat(32)),
                    reduce_only: false,
                    max_fill_price: d("100"),
                }),
            ),
        ],
        true,
    );
    let err1 = audit_journal(&bad_risk_increase.0, &s)
        .err()
        .expect("tampered risk increase must fail audit")
        .to_string();
    assert!(
        err1.contains("Financial fee reconciliation required before increasing exposure"),
        "unexpected audit error: {err1}"
    );
    assert!(ExecutionJournal::recover(&bad_risk_increase.0, &s, 700).is_err());

    // Case 2: Tampered schema 3 WAL closes session flat without reconciling +1 OTHER financial charge.
    let close_order = PreparedOrder {
        intent: intent("p-close", Leg::Primary, HedgeSide::Sell, "1"),
        wire_client_id: format!("a{}", "3".repeat(32)),
        reduce_only: true,
        max_fill_price: d("100"),
    };
    let fill_close = ExecutionFill {
        identity: s.legs[0].identity.clone(),
        logical_id: "p-close".into(),
        exchange_order_id: "oid-2".into(),
        trade_id: "t2".into(),
        venue_at_us: 550,
        quantity: d("1"),
        price: d("100"),
        fee: Decimal::ZERO,
        fee_asset: "USDG".into(),
    };
    let term_close = OrderTerminal {
        identity: s.legs[0].identity.clone(),
        logical_id: "p-close".into(),
        exchange_order_id: Some("oid-2".into()),
        status: OrderStatus::Filled,
        cumulative_filled: d("1"),
        venue_at_us: 551,
    };
    let bad_close = write_signed_events(
        &s,
        storage::SCHEMA_CURRENT,
        &[
            (100, Event::Start(s.clone())),
            (
                202,
                Event::Reconciled(Reconciliation {
                    observed_sequence: 1,
                    legs: base_pos(200, vec![], Decimal::ZERO),
                }),
            ),
            (300, Event::Prepared(open_order)),
            (350, Event::Fill(fill_other)),
            (400, Event::Terminal(term_open)),
            (500, Event::Prepared(close_order)),
            (550, Event::Fill(fill_close)),
            (560, Event::Terminal(term_close)),
            (
                602,
                Event::Reconciled(Reconciliation {
                    observed_sequence: 8,
                    legs: base_pos(600, vec!["t1".into(), "t2".into()], Decimal::ZERO),
                }),
            ),
            (700, Event::Closed),
        ],
        false,
    );
    let err2 = audit_journal(&bad_close.0, &s)
        .err()
        .expect("tampered close must fail audit")
        .to_string();
    assert!(
        err2.contains("Session close requires resolved financial fee charges"),
        "unexpected close audit error: {err2}"
    );
    assert!(ExecutionJournal::recover(&bad_close.0, &s, 800).is_err());
}

#[cfg(unix)]
#[test]
fn r3_schema_2_historical_journals_remain_auditable_and_enforce_dual_gates_after_recovery() {
    let s = spec();
    let base_pos = |ts: u64, covered: Vec<String>, pos: Decimal| {
        std::array::from_fn(|i| PositionEvidence {
            identity: s.legs[i].identity.clone(),
            request_started_us: ts,
            received_at_us: ts + 1,
            position_venue_at_us: if i == 0 { ts } else { 10 },
            fills_complete_through_venue_us: if i == 0 { ts } else { 10 },
            covered_trade_ids: if i == 0 { covered.clone() } else { vec![] },
            open_order_ids: vec![],
            position_base: if i == 0 { pos } else { Decimal::ZERO },
        })
    };

    let open_order = PreparedOrder {
        intent: intent("p1", Leg::Primary, HedgeSide::Buy, "1"),
        wire_client_id: format!("a{}", "1".repeat(32)),
        reduce_only: false,
        max_fill_price: d("100"),
    };
    let fill_other = ExecutionFill {
        identity: s.legs[0].identity.clone(),
        logical_id: "p1".into(),
        exchange_order_id: "oid-1".into(),
        trade_id: "t1".into(),
        venue_at_us: 200,
        quantity: d("1"),
        price: d("100"),
        fee: d("1"),
        fee_asset: "OTHER".into(),
    };
    let term_open = OrderTerminal {
        identity: s.legs[0].identity.clone(),
        logical_id: "p1".into(),
        exchange_order_id: Some("oid-1".into()),
        status: OrderStatus::Filled,
        cumulative_filled: d("1"),
        venue_at_us: 201,
    };

    // Historical schema 2 Case A (11 records): pre-gate closed journal with +1 OTHER fee.
    // Must audit cleanly, preserve unconverted fees read-only, and allow marker cleanup recovery.
    let close_order = PreparedOrder {
        intent: intent("p-close", Leg::Primary, HedgeSide::Sell, "1"),
        wire_client_id: format!("a{}", "2".repeat(32)),
        reduce_only: true,
        max_fill_price: d("100"),
    };
    let fill_close = ExecutionFill {
        identity: s.legs[0].identity.clone(),
        logical_id: "p-close".into(),
        exchange_order_id: "oid-2".into(),
        trade_id: "t2".into(),
        venue_at_us: 550,
        quantity: d("1"),
        price: d("100"),
        fee: Decimal::ZERO,
        fee_asset: "USDG".into(),
    };
    let term_close = OrderTerminal {
        identity: s.legs[0].identity.clone(),
        logical_id: "p-close".into(),
        exchange_order_id: Some("oid-2".into()),
        status: OrderStatus::Filled,
        cumulative_filled: d("1"),
        venue_at_us: 551,
    };
    let closed_v2 = write_signed_events(
        &s,
        storage::SCHEMA_V2,
        &[
            (100, Event::Start(s.clone())),
            (
                202,
                Event::Reconciled(Reconciliation {
                    observed_sequence: 1,
                    legs: base_pos(200, vec![], Decimal::ZERO),
                }),
            ),
            (300, Event::Prepared(open_order.clone())),
            (350, Event::Fill(fill_other.clone())),
            (400, Event::Terminal(term_open.clone())),
            (
                452,
                Event::Reconciled(Reconciliation {
                    observed_sequence: 5,
                    legs: base_pos(450, vec!["t1".into()], d("1")),
                }),
            ),
            (500, Event::Prepared(close_order)),
            (550, Event::Fill(fill_close)),
            (560, Event::Terminal(term_close)),
            (
                602,
                Event::Reconciled(Reconciliation {
                    observed_sequence: 9,
                    legs: base_pos(600, vec!["t1".into(), "t2".into()], Decimal::ZERO),
                }),
            ),
            (700, Event::Closed),
        ],
        true,
    );
    let closed_audit = audit_journal(&closed_v2.0, &s).unwrap();
    assert_eq!(closed_audit.records, 11);
    assert_eq!(closed_audit.orders, 2);
    assert_eq!(closed_audit.fills, 2);
    assert_eq!(closed_audit.unverified_orders, 0);
    assert!(!closed_audit.restart_allowed);
    assert!(closed_audit.financial_reconciliation_required);
    assert_eq!(
        closed_audit.unconverted_fees_by_asset,
        BTreeMap::from([((0, "OTHER".into()), d("1"))])
    );

    let mut recovered_closed = ExecutionJournal::recover(&closed_v2.0, &s, 800).unwrap();
    assert!(recovered_closed.requires_reconciliation());
    assert!(recovered_closed.requires_financial_reconciliation());
    assert_eq!(
        recovered_closed.unconverted_fees_by_asset(),
        BTreeMap::from([((0, "OTHER".into()), d("1"))])
    );
    assert!(
        recovered_closed
            .prepare_intent(
                intent("blocked-closed", Leg::Primary, HedgeSide::Buy, "0.5"),
                false,
                None,
                805,
            )
            .is_err()
    );
    recovered_closed.close_session(801).unwrap();
    drop(recovered_closed);

    // Historical schema 2 Case B (7 records): active journal with +1 OTHER, pos reconcile, and in-flight !reduce_only order.
    let active_v2 = write_signed_events(
        &s,
        storage::SCHEMA_V2,
        &[
            (100, Event::Start(s.clone())),
            (
                202,
                Event::Reconciled(Reconciliation {
                    observed_sequence: 1,
                    legs: base_pos(200, vec![], Decimal::ZERO),
                }),
            ),
            (300, Event::Prepared(open_order)),
            (350, Event::Fill(fill_other)),
            (400, Event::Terminal(term_open)),
            (
                502,
                Event::Reconciled(Reconciliation {
                    observed_sequence: 5,
                    legs: base_pos(500, vec!["t1".into()], d("1")),
                }),
            ),
            (
                600,
                Event::Prepared(PreparedOrder {
                    intent: intent("p2-historic-increase", Leg::Primary, HedgeSide::Buy, "0.5"),
                    wire_client_id: format!("a{}", "3".repeat(32)),
                    reduce_only: false,
                    max_fill_price: d("100"),
                }),
            ),
        ],
        true,
    );
    let active_audit = audit_journal(&active_v2.0, &s).unwrap();
    assert_eq!(active_audit.records, 7);
    assert_eq!(active_audit.orders, 2);
    assert_eq!(active_audit.fills, 1);
    assert_eq!(active_audit.unverified_orders, 1);
    assert!(!active_audit.restart_allowed);
    assert!(active_audit.financial_reconciliation_required);
    assert_eq!(
        active_audit.unconverted_fees_by_asset,
        BTreeMap::from([((0, "OTHER".into()), d("1"))])
    );

    let mut active = ExecutionJournal::recover(&active_v2.0, &s, 700).unwrap();
    assert!(active.requires_reconciliation());
    assert!(active.requires_financial_reconciliation());

    // Settle historic in-flight order + position-only reconcile: position clears, financial gate stays active.
    active
        .record_terminal(
            OrderTerminal {
                identity: s.legs[0].identity.clone(),
                logical_id: "p2-historic-increase".into(),
                exchange_order_id: None,
                status: OrderStatus::Rejected,
                cumulative_filled: Decimal::ZERO,
                venue_at_us: 710,
            },
            720,
        )
        .unwrap();
    reconcile(&mut active);
    assert!(!active.requires_reconciliation());
    assert!(active.requires_financial_reconciliation());

    // New risk-increasing writes on recovered schema 2 MUST be blocked under new rules.
    assert!(
        active
            .prepare_intent(
                intent(
                    "p3-new-increase-blocked",
                    Leg::Primary,
                    HedgeSide::Buy,
                    "0.5"
                ),
                false,
                None,
                at(&active),
            )
            .is_err()
    );

    // Controlled reduce-only close succeeds cleanly, but close_session stays blocked until financial proof.
    let close_now = intent("p4-reduce", Leg::Primary, HedgeSide::Sell, "1");
    let close_bound = Some(close_now.limit_price);
    active
        .prepare_intent(close_now, true, close_bound, at(&active))
        .unwrap();
    ack(&mut active, "p4-reduce");
    let mut reduce_fill = fill(&active, "p4-reduce", "t-reduce", "1", 800);
    reduce_fill.fee = Decimal::ZERO;
    reduce_fill.fee_asset = "USDG".into();
    active.record_fill(reduce_fill, at(&active)).unwrap();
    terminal(&mut active, "p4-reduce", "1", OrderStatus::Filled, 801);
    reconcile(&mut active);
    assert_eq!(active.positions(), [d("0"), d("0")]);
    assert!(active.requires_financial_reconciliation());
    assert!(active.close_session(at(&active) + 2).is_err());

    // Independent financial settlement resolves +1 OTHER, deducts quote budget, and allows clean close & audit.
    let fin = financial_proof(&active, "1", "0");
    let fin_ts = fin_at(&active) + 2;
    active.reconcile_financial(fin, fin_ts).unwrap();
    assert!(!active.requires_financial_reconciliation());
    assert_eq!(active.available_quote().unwrap()[0], d("1999"));
    active.close_session(fin_ts + 1).unwrap();
    let final_ts = fin_ts + 50;
    drop(active);

    let post_close_audit = audit_journal(&active_v2.0, &s).unwrap();
    assert!(!post_close_audit.restart_allowed);
    assert!(!post_close_audit.financial_reconciliation_required);
    assert!(post_close_audit.unconverted_fees_by_asset.is_empty());
    assert!(ExecutionJournal::recover(&active_v2.0, &s, final_ts).is_ok());
}
