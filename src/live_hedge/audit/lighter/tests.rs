//! Synthetic controls through the real evidence parser and audit entry.

use super::super::{AuditStatus, Report, audit};
use crate::live_hedge::journal::{IntentRecord, JournalRecord, PrimaryObservation, StartRecord};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::str::FromStr;

const HL: &str = "0x00000000000000000000000000000000000000aa";
const CLOID: &str = "0x0123456789abcdef0123456789abcdef";

fn d(value: &str) -> Decimal {
    Decimal::from_str(value).unwrap()
}

fn start(schema: u32, market: Option<i64>) -> JournalRecord {
    JournalRecord::Start(StartRecord {
        session_id: "s-1".into(),
        network: "Testnet".into(),
        lighter_account_index: 7,
        hyperliquid_account: Some(HL.into()),
        symbol: "ETH".into(),
        journal_schema: Some(schema),
        lighter_market_id: market,
    })
}

/// A schema 2 IOC with no terminal: Hyperliquid stays pending.
fn pending_intent() -> JournalRecord {
    JournalRecord::Intent(IntentRecord {
        cloid: CLOID.into(),
        coin: "ETH".into(),
        submitted_at_ms: 1_000,
        is_buy: false,
        size: d("0.01"),
        limit_price: d("1998"),
        reduce_only: false,
        primary_position: d("0.01"),
        hedge_position: Decimal::ZERO,
        primary_observation: Some(PrimaryObservation {
            account_index: 7,
            market_id: 4095,
            position: d("0.01"),
            transaction_time: Some(30),
            request_us: 12_000,
            age_us: 15_000,
            max_age_us: 5_000_000,
        }),
    })
}

fn journal(records: &[JournalRecord]) -> Vec<u8> {
    records.iter().flat_map(|r| r.to_line().unwrap()).collect()
}

fn order(id: i64, status: &str, filled: &str, time: u64) -> Value {
    json!({"client_order_index": id, "market_index": 4095, "owner_account_index": 7,
        "status": status, "filled_base_amount": filled, "transaction_time": time,
        "order_index": id * 10, "price": "2000.00"})
}

fn account(time: u64, position: Option<(i64, &str)>) -> Value {
    let positions: Vec<Value> = position
        .map(|(sign, size)| {
            json!({"market_id": 4095, "symbol": "ETH", "sign": sign, "position": size,
                "avg_entry_price": "2000", "unrealized_pnl": "0", "liquidation_price": "0"})
        })
        .into_iter()
        .collect();
    json!({"code": 200, "accounts": [{"index": 7, "transaction_time": time, "positions": positions}]})
}

/// Evidence as the operator writes it: raw JSON through the real parser.
fn lighter(ids: Value, orders: Vec<Value>, account: Option<Value>) -> Value {
    let mut lighter = json!({"account_index": 7, "market_id": 4095, "symbol": "ETH",
        "client_order_ids": ids,
        "order_histories": [{"observed_at_ms": 2_000, "response": {"code": 200, "orders": orders}}]});
    if let Some(account) = account {
        lighter["account"] = json!({"observed_at_ms": 2_100, "response": account});
    }
    lighter
}

fn run(records: &[JournalRecord], lighter: Option<Value>) -> Report {
    let mut evidence = json!({"network": "Testnet", "hyperliquid_account": HL, "coin": "ETH"});
    if let Some(lighter) = lighter {
        evidence["lighter"] = lighter;
    }
    let evidence = serde_json::from_value(evidence).unwrap();
    audit(&journal(records), Some(&evidence))
}

fn v2(lighter: Value) -> Report {
    run(&[start(2, Some(4095)), pending_intent()], Some(lighter))
}

fn kinds(report: &Report) -> Vec<&'static str> {
    report.issues.iter().map(|i| i.kind).collect()
}

/// Complete control: terminal orders, the account covers the highest fill
/// watermark and is flat. The Hyperliquid IOC result is not rewritten.
#[test]
fn terminal_orders_with_a_covering_flat_account_are_explained() {
    let report = v2(lighter(
        json!([11, 12]),
        vec![
            order(11, "filled", "0.01", 40),
            order(12, "canceled", "0", 0),
        ],
        Some(account(41, None)),
    ));
    let leg = &report.primary_leg;
    assert_eq!(leg.status, "explained", "{:?}", report.issues);
    assert_eq!(leg.scope, "caller_declared_order_ids");
    assert_eq!(
        (leg.required_watermark, leg.account_watermark),
        (Some(40), Some(41))
    );
    assert_eq!((leg.watermark, leg.residual), ("covered", "flat"));
    assert_eq!(leg.position, Some(Decimal::ZERO));
    let states: Vec<_> = leg
        .orders
        .iter()
        .map(|o| (o.client_order_id, o.state, o.fill_watermark))
        .collect();
    assert_eq!(
        states,
        [(11, "terminal", Some(40)), (12, "terminal", Some(0))]
    );
    // Hyperliquid IOC still pending; the overall report stays unresolved.
    assert_eq!(report.orders[0].status, super::super::OrderStatus::Pending);
    assert_eq!(report.status, AuditStatus::Unresolved);
    assert!(!report.restart_permitted);
    // Equal watermarks cover (production `>=`).
    let equal = v2(lighter(
        json!([11]),
        vec![order(11, "filled", "0.01", 40)],
        Some(account(40, None)),
    ));
    assert_eq!(equal.primary_leg.watermark, "covered");
}

/// Partial maker fill then cancel, as recovery sees it: the fill sets the
/// watermark and a covering sample with a position reports the residual.
#[test]
fn partial_fill_then_cancel_reports_the_signed_residual() {
    let report = v2(lighter(
        json!([21]),
        vec![order(21, "canceled-post-only", "0.004", 55)],
        Some(account(60, Some((-1, "0.004")))),
    ));
    let leg = &report.primary_leg;
    assert_eq!(leg.status, "unresolved");
    assert_eq!(leg.orders[0].filled, Some(d("0.004")));
    assert_eq!(
        (leg.required_watermark, leg.watermark),
        (Some(55), "covered")
    );
    assert_eq!((leg.position, leg.residual), (Some(d("-0.004")), "nonzero"));
    assert!(kinds(&report).contains(&"lighter_residual"));
}

/// Live orders, unlisted IDs, and stale or unknown account watermarks keep
/// the residual unproven, each with its own kind.
#[test]
fn open_missing_and_uncovered_orders_stay_unresolved() {
    let live = v2(lighter(
        json!([31]),
        vec![order(31, "open", "0.002", 70)],
        Some(account(80, None)),
    ));
    assert_eq!(live.primary_leg.orders[0].state, "live");
    assert_eq!(
        (live.primary_leg.watermark, live.primary_leg.residual),
        ("orders_unresolved", "unproven")
    );
    assert!(kinds(&live).contains(&"lighter_order_live"));
    // An ID the history omits, even when the account is flat and recent:
    // absence never proves the order terminal.
    let missing = v2(lighter(
        json!([31, 32]),
        vec![order(31, "filled", "0.01", 70)],
        Some(account(90, None)),
    ));
    assert_eq!(missing.primary_leg.status, "unresolved");
    assert_eq!(missing.primary_leg.orders[1].state, "missing");
    assert_eq!(missing.primary_leg.required_watermark, None);
    assert_eq!(missing.primary_leg.residual, "unproven");
    assert!(kinds(&missing).contains(&"lighter_order_missing"));
    let stale = v2(lighter(
        json!([31]),
        vec![order(31, "filled", "0.01", 70)],
        Some(account(69, None)),
    ));
    assert_eq!(
        (stale.primary_leg.watermark, stale.primary_leg.residual),
        ("stale", "unproven")
    );
    assert_eq!(
        stale.primary_leg.position,
        Some(Decimal::ZERO),
        "shown, not counted"
    );
    let unknown = v2(lighter(
        json!([31]),
        vec![order(31, "filled", "0.01", 70)],
        Some(account(0, None)),
    ));
    assert_eq!(unknown.primary_leg.watermark, "unknown");
    assert_eq!(unknown.primary_leg.account_watermark, None);
    for report in [&stale, &unknown] {
        assert_eq!(report.primary_leg.status, "unresolved");
        assert!(kinds(report).contains(&"lighter_watermark"));
    }
    // Nothing filled: an unknown account watermark still covers "no fills".
    let unfilled = v2(lighter(
        json!([33]),
        vec![order(33, "canceled", "0", 0)],
        Some(account(0, None)),
    ));
    assert_eq!(unfilled.primary_leg.status, "explained");
}

/// Duplicate, foreign-account and foreign-market entries and malformed
/// responses contradict the production rules: invalid, never pending.
#[test]
fn contradictory_history_and_account_are_invalid() {
    let mut foreign_account = order(41, "filled", "0.01", 70);
    foreign_account["owner_account_index"] = json!(8);
    let mut foreign_market = order(41, "filled", "0.01", 70);
    foreign_market["market_index"] = json!(4096);
    let cases = [
        (
            vec![
                order(41, "filled", "0.01", 70),
                order(41, "filled", "0.01", 70),
            ],
            "duplicate",
        ),
        (vec![foreign_account], "foreign_account"),
        (vec![foreign_market], "foreign_market"),
        (
            vec![order(41, "filled", "0.01", 0)],
            "fill_without_watermark",
        ),
        (vec![order(41, "filled", "0", 0)], "filled_without_quantity"),
        (vec![order(41, "canceled", "-0.01", 70)], "negative_fill"),
    ];
    for (orders, kind) in cases {
        let report = v2(lighter(json!([41]), orders, Some(account(80, None))));
        assert_eq!(report.primary_leg.status, "invalid", "{kind}");
        assert_eq!(report.primary_leg.orders[0].state, "invalid", "{kind}");
        let reason = report.primary_leg.orders[0].reason.as_deref().unwrap();
        assert!(reason.contains(kind), "{kind}: {reason}");
        assert_eq!(report.status, AuditStatus::Invalid, "{kind}");
        assert!(!report.restart_permitted);
        // The Hyperliquid IOC result is not rewritten by Lighter findings.
        assert_eq!(report.orders[0].status, super::super::OrderStatus::Pending);
        assert_eq!(report.orders[0].primary.status, "journal_claim");
    }
    // Duplicate across two history responses is the same contradiction.
    let mut split = lighter(
        json!([41]),
        vec![order(41, "filled", "0.01", 70)],
        Some(account(80, None)),
    );
    split["order_histories"].as_array_mut().unwrap().push(
        json!({"observed_at_ms": 2_050, "response": {"code": 200, "orders": [order(41, "filled", "0.01", 70)]}}),
    );
    assert_eq!(v2(split).primary_leg.orders[0].state, "invalid");
    // Declared IDs must be positive and unique.
    for ids in [json!([0]), json!([41, 41])] {
        let report = v2(lighter(
            ids,
            vec![order(41, "filled", "0.01", 70)],
            Some(account(80, None)),
        ));
        assert!(kinds(&report).contains(&"lighter_declared_id"));
        assert_eq!(report.primary_leg.status, "invalid");
    }
    // Account sample: duplicate account, wrong symbol, bad sign, error code.
    let mut twice = account(80, None);
    let entry = twice["accounts"][0].clone();
    twice["accounts"].as_array_mut().unwrap().push(entry);
    let mut symbol = account(80, Some((1, "0.01")));
    symbol["accounts"][0]["positions"][0]["symbol"] = json!("BTC");
    let mut sign = account(80, Some((1, "0.01")));
    sign["accounts"][0]["positions"][0]["sign"] = json!(2);
    let failed = json!({"code": 500, "accounts": []});
    for sample in [twice, symbol, sign, failed] {
        let report = v2(lighter(
            json!([41]),
            vec![order(41, "filled", "0.01", 70)],
            Some(sample),
        ));
        assert!(
            kinds(&report).contains(&"lighter_invalid_account"),
            "{:?}",
            report.issues
        );
        assert_eq!(report.primary_leg.status, "invalid");
        assert_eq!(report.primary_leg.residual, "unproven");
    }
    let bad_history = json!({"account_index": 7, "market_id": 4095, "symbol": "ETH",
        "client_order_ids": [41], "order_histories": [{"observed_at_ms": 1, "response": {"code": 429, "orders": []}}]});
    assert!(kinds(&v2(bad_history)).contains(&"lighter_invalid_history"));
}

/// Evidence for another account, market or symbol is not applied.
#[test]
fn lighter_evidence_must_match_the_journaled_session() {
    for (field, value) in [
        ("account_index", json!(8)),
        ("market_id", json!(4096)),
        ("symbol", json!("BTC")),
    ] {
        let mut evidence = lighter(
            json!([11]),
            vec![order(11, "canceled", "0", 0)],
            Some(account(41, None)),
        );
        evidence[field] = value;
        let report = v2(evidence);
        assert_eq!(report.primary_leg.status, "invalid", "{field}");
        assert!(report.primary_leg.orders.is_empty(), "{field}");
        assert!(kinds(&report).contains(&"lighter_identity"));
        assert_eq!(report.status, AuditStatus::Invalid);
    }
}

/// Absent sections, an empty ID set and journals without a bound market are
/// reported as missing evidence; old evidence files still parse.
#[test]
fn missing_lighter_evidence_is_explicit_and_old_files_still_parse() {
    // Old evidence (no `lighter` section): unchanged report, not_provided.
    let old = run(&[start(2, Some(4095)), pending_intent()], None);
    assert_eq!(old.primary_leg.status, "not_provided");
    assert!(!kinds(&old).iter().any(|k| k.starts_with("lighter_")));
    assert_eq!(old.status, AuditStatus::Unresolved);
    let empty = v2(lighter(json!([]), vec![], Some(account(41, None))));
    assert_eq!(empty.primary_leg.status, "missing_evidence");
    assert!(kinds(&empty).contains(&"lighter_missing_order_ids"));
    assert_eq!(
        empty.primary_leg.residual, "unproven",
        "no IDs never proves flat"
    );
    let no_account = v2(lighter(
        json!([11]),
        vec![order(11, "canceled", "0", 0)],
        None,
    ));
    assert_eq!(no_account.primary_leg.status, "missing_evidence");
    assert!(kinds(&no_account).contains(&"lighter_missing_account"));
    assert_eq!(no_account.primary_leg.orders[0].state, "terminal");
    // Schema 1 start binds no market: the section cannot be matched.
    let v1 = run(
        &[start(1, None)],
        Some(lighter(
            json!([11]),
            vec![order(11, "canceled", "0", 0)],
            Some(account(41, None)),
        )),
    );
    assert_eq!(v1.primary_leg.status, "missing_evidence");
    assert!(kinds(&v1).contains(&"lighter_unbound_market"));
    assert!(v1.primary_leg.orders.is_empty());
    // Unknown fields in the Lighter section are rejected by the parser.
    let mut evidence = json!({"network": "Testnet", "hyperliquid_account": HL, "coin": "ETH",
        "lighter": lighter(json!([11]), vec![], None)});
    evidence["lighter"]["complete"] = json!(true);
    assert!(serde_json::from_value::<super::super::Evidence>(evidence).is_err());
}

fn v3_start() -> JournalRecord {
    start(3, Some(4095))
}

fn batch(ids: &[i64]) -> JournalRecord {
    JournalRecord::LighterBatch(crate::live_hedge::journal::LighterBatchRecord {
        session_id: "s-1".into(),
        network: "Testnet".into(),
        lighter_account_index: 7,
        lighter_market_id: 4095,
        client_order_ids: ids.to_vec(),
        cancels: 1,
        prepared_at_ms: 1_000,
    })
}

/// Journaled IDs are checked even when the caller leaves them out; extra
/// declared IDs are flagged; each order carries its source and line.
#[test]
fn journaled_ids_are_checked_and_sources_kept_apart() {
    let flat = || Some(account(80, None));
    let orders = || {
        vec![
            order(11, "filled", "0.01", 40),
            order(12, "canceled", "0", 0),
        ]
    };
    let records = [v3_start(), batch(&[11, 12])];
    // Caller declares both: explained, both journal_and_caller.
    let both = run(&records, Some(lighter(json!([11, 12]), orders(), flat())));
    assert_eq!(both.journal.schema, "v3");
    assert_eq!(both.primary_leg.status, "explained", "{:?}", both.issues);
    assert_eq!(both.primary_leg.scope, "journal_and_caller_order_ids");
    let src: Vec<_> = both
        .primary_leg
        .orders
        .iter()
        .map(|o| (o.client_order_id, o.source, o.journal_line))
        .collect();
    assert_eq!(
        src,
        [
            (11, "journal_and_caller", Some(2)),
            (12, "journal_and_caller", Some(2))
        ]
    );
    // Caller omits 12: it is still checked, flagged journal_only.
    let omitted = run(&records, Some(lighter(json!([11]), orders(), flat())));
    assert_eq!(omitted.primary_leg.orders.len(), 2);
    assert_eq!(omitted.primary_leg.orders[1].source, "journal_only");
    assert!(kinds(&omitted).contains(&"lighter_undeclared_journal_id"));
    assert_eq!(omitted.primary_leg.status, "unresolved");
    // Caller omits 12 and the history lacks it: missing, not covered.
    let lost = run(
        &records,
        Some(lighter(
            json!([11]),
            vec![order(11, "filled", "0.01", 40)],
            flat(),
        )),
    );
    assert_eq!(lost.primary_leg.orders[1].state, "missing");
    assert_eq!(lost.primary_leg.residual, "unproven");
    // Empty declaration still checks the journaled IDs.
    let empty = run(&records, Some(lighter(json!([]), orders(), flat())));
    assert!(!kinds(&empty).contains(&"lighter_missing_order_ids"));
    assert_eq!(empty.primary_leg.orders.len(), 2);
    // An extra declared ID is caller_only and flagged.
    let extra = run(
        &records,
        Some(lighter(json!([11, 12, 13]), orders(), flat())),
    );
    assert_eq!(extra.primary_leg.orders[2].source, "caller_only");
    assert!(kinds(&extra).contains(&"lighter_unjournaled_declared_id"));
    for report in [&both, &omitted, &lost, &empty, &extra] {
        assert!(!report.restart_permitted);
        assert_eq!(
            report.orders.len(),
            0,
            "no Hyperliquid IOC in these journals"
        );
    }
}

/// Bad batch records are invalid and never contribute IDs; a torn record
/// is not written; batch records in a v2 journal are rejected.
#[test]
fn bad_batch_records_never_count_as_recorded() {
    let JournalRecord::LighterBatch(base) = batch(&[11]) else {
        unreachable!()
    };
    let mut cases = Vec::new();
    for (name, edit) in [
        (
            "session",
            Box::new(|b: &mut crate::live_hedge::journal::LighterBatchRecord| {
                b.session_id = "other".into()
            }) as Box<dyn Fn(&mut _)>,
        ),
        (
            "network",
            Box::new(|b: &mut crate::live_hedge::journal::LighterBatchRecord| {
                b.network = "Mainnet".into()
            }),
        ),
        (
            "account",
            Box::new(|b: &mut crate::live_hedge::journal::LighterBatchRecord| {
                b.lighter_account_index = 8
            }),
        ),
        (
            "market",
            Box::new(|b: &mut crate::live_hedge::journal::LighterBatchRecord| {
                b.lighter_market_id = 4096
            }),
        ),
        (
            "empty",
            Box::new(|b: &mut crate::live_hedge::journal::LighterBatchRecord| {
                b.client_order_ids.clear()
            }),
        ),
        (
            "non-positive",
            Box::new(|b: &mut crate::live_hedge::journal::LighterBatchRecord| {
                b.client_order_ids = vec![0]
            }),
        ),
        (
            "duplicate",
            Box::new(|b: &mut crate::live_hedge::journal::LighterBatchRecord| {
                b.client_order_ids = vec![11, 11]
            }),
        ),
        (
            "oversize",
            Box::new(|b: &mut crate::live_hedge::journal::LighterBatchRecord| b.cancels = 50),
        ),
    ] {
        let mut record = base.clone();
        edit(&mut record);
        cases.push((name, vec![v3_start(), JournalRecord::LighterBatch(record)]));
    }
    cases.push((
        "repeat across records",
        vec![v3_start(), batch(&[11]), batch(&[11])],
    ));
    cases.push(("v2 journal", vec![start(2, Some(4095)), batch(&[11])]));
    for (name, records) in cases {
        let report = audit(&journal(&records), None);
        assert_eq!(report.status, AuditStatus::Invalid, "{name}");
        assert!(kinds(&report).contains(&"invalid_lighter_batch"), "{name}");
        let expected: &[i64] = if name == "repeat across records" {
            &[11]
        } else {
            &[]
        };
        assert_eq!(report.primary_leg.journal_order_ids, expected, "{name}");
        assert!(!report.restart_permitted);
    }
    // Torn tail: a batch record without its newline was never synced.
    let mut bytes = journal(&[v3_start()]);
    let line = batch(&[11]).to_line().unwrap();
    bytes.extend_from_slice(&line[..line.len() - 1]);
    let torn = audit(&bytes, None);
    assert!(kinds(&torn).contains(&"truncated_tail"));
    assert!(torn.primary_leg.journal_order_ids.is_empty());
    // The tail may have been a batch record: a prefix, never complete.
    assert_eq!(torn.primary_leg.status, "unresolved");
    assert_eq!(torn.primary_leg.journal_coverage, "prefix");
    assert_eq!(torn.primary_leg.journal_gaps, [2]);
    // Unknown fields in a batch record are a parse error, not ignored.
    let mut value: Value = serde_json::from_slice(&batch(&[11]).to_line().unwrap()).unwrap();
    value["acked"] = json!(true);
    let mut tampered = journal(&[v3_start()]);
    tampered.extend(serde_json::to_vec(&value).unwrap());
    tampered.push(b'\n');
    assert_eq!(audit(&tampered, None).status, AuditStatus::Invalid);
}

fn batch_counts(ids: &[i64], cancels: usize) -> JournalRecord {
    let JournalRecord::LighterBatch(mut record) = batch(ids) else {
        unreachable!()
    };
    record.cancels = cancels;
    JournalRecord::LighterBatch(record)
}

/// The action count is checked arithmetic: 50 is accepted, 51 and any
/// wrapping count are rejected in every build, and a rejected batch
/// contributes no ID and leaves a prefix.
#[test]
fn batch_action_count_is_checked_and_never_wraps() {
    let evidence = || {
        Some(lighter(
            json!([101]),
            vec![order(101, "canceled", "0", 0)],
            Some(account(80, None)),
        ))
    };
    let ok = run(&[v3_start(), batch_counts(&[101], 49)], evidence());
    assert_eq!(ok.status, AuditStatus::JournalRecorded, "{:?}", ok.issues);
    assert_eq!(ok.primary_leg.journal_order_ids, [101]);
    assert_eq!(
        (ok.primary_leg.status, ok.primary_leg.journal_coverage),
        ("explained", "complete")
    );
    assert_eq!(ok.status.exit_code(), 0);
    let fifty: Vec<i64> = (1..=50).collect();
    let full = audit(&journal(&[v3_start(), batch_counts(&fifty, 0)]), None);
    assert_eq!(full.primary_leg.journal_order_ids.len(), 50);
    assert!(!kinds(&full).contains(&"invalid_lighter_batch"));
    for cancels in [50, usize::MAX, usize::MAX - 1] {
        let report = run(&[v3_start(), batch_counts(&[101], cancels)], evidence());
        assert_eq!(report.status, AuditStatus::Invalid, "{cancels}");
        assert_eq!(report.status.exit_code(), 3);
        assert!(kinds(&report).contains(&"invalid_lighter_batch"));
        assert!(report.primary_leg.journal_order_ids.is_empty(), "{cancels}");
        assert_eq!(report.primary_leg.status, "invalid", "{cancels}");
        assert_eq!(report.primary_leg.journal_coverage, "prefix");
        assert_eq!(report.primary_leg.journal_gaps, [2]);
        assert_eq!(report.primary_leg.residual, "unproven");
        // The declared order is still diagnosed, as caller_only.
        assert_eq!(report.primary_leg.orders[0].source, "caller_only");
        assert_eq!(report.primary_leg.orders[0].state, "terminal");
    }
}

/// A good batch followed by a rejected, unreadable or torn line: the good
/// batch is still diagnosed, the account fields stay visible, but the
/// primary leg is never explained or proven flat.
#[test]
fn a_damaged_journal_reports_a_prefix_never_a_complete_leg() {
    let JournalRecord::LighterBatch(mut foreign) = batch(&[102]) else {
        unreachable!()
    };
    foreign.lighter_account_index = 8;
    let good = batch(&[101]);
    let torn = {
        let line = batch(&[102]).to_line().unwrap();
        line[..line.len() - 1].to_vec()
    };
    let half_json = b"{\"event\":\"lighter_create_batch\",\"client_ord".to_vec();
    let cases: Vec<(&str, Vec<u8>, &str, AuditStatus)> = vec![
        (
            "duplicate",
            journal(&[v3_start(), good.clone(), batch(&[101])]),
            "invalid",
            AuditStatus::Invalid,
        ),
        (
            "foreign",
            journal(&[
                v3_start(),
                good.clone(),
                JournalRecord::LighterBatch(foreign),
            ]),
            "invalid",
            AuditStatus::Invalid,
        ),
        (
            "torn",
            [journal(&[v3_start(), good.clone()]), torn].concat(),
            "unresolved",
            AuditStatus::Unresolved,
        ),
        (
            "half json",
            [journal(&[v3_start(), good.clone()]), half_json.clone()].concat(),
            "unresolved",
            AuditStatus::Unresolved,
        ),
        (
            "half json line",
            [
                journal(&[v3_start(), good.clone()]),
                half_json,
                b"\n".to_vec(),
            ]
            .concat(),
            "invalid",
            AuditStatus::Invalid,
        ),
    ];
    for (name, bytes, leg, overall) in cases {
        let evidence: super::super::Evidence = serde_json::from_value(json!({
            "network": "Testnet", "hyperliquid_account": HL, "coin": "ETH",
            "lighter": lighter(json!([101]), vec![order(101, "filled", "0.01", 40)], Some(account(41, None)))}))
        .unwrap();
        for with_evidence in [true, false] {
            let report = audit(&bytes, with_evidence.then_some(&evidence));
            let leg_report = &report.primary_leg;
            assert_eq!(
                report.status, overall,
                "{name} {with_evidence}: {:?}",
                report.issues
            );
            assert_eq!(leg_report.journal_coverage, "prefix", "{name}");
            assert_eq!(leg_report.journal_gaps, [3], "{name}");
            assert_eq!(leg_report.journal_order_ids, [101], "{name}");
            assert_ne!(leg_report.status, "explained", "{name}");
            assert_eq!(leg_report.residual, "unproven", "{name}");
            assert!(kinds(&report).contains(&"lighter_journal_prefix"), "{name}");
            assert!(!report.restart_permitted);
            if with_evidence {
                assert_eq!(leg_report.status, leg, "{name}");
                // Per-order diagnosis and the raw sample stay visible.
                assert_eq!(leg_report.orders[0].state, "terminal", "{name}");
                assert_eq!(leg_report.watermark, "prefix_covered", "{name}");
                assert_eq!(leg_report.position, Some(Decimal::ZERO), "{name}");
            } else {
                assert_ne!(leg_report.status, "not_provided", "{name}");
            }
        }
    }
}

/// Without batch records (v1, v2) coverage is `not_recorded`, and a
/// damaged older journal does not invent a Lighter prefix.
#[test]
fn older_schemas_never_claim_batch_coverage() {
    let evidence = || {
        Some(lighter(
            json!([11]),
            vec![order(11, "canceled", "0", 0)],
            Some(account(41, None)),
        ))
    };
    let v2 = run(&[start(2, Some(4095)), pending_intent()], evidence());
    assert_eq!(v2.primary_leg.journal_coverage, "not_recorded");
    assert_eq!(v2.primary_leg.status, "explained");
    let mut torn = journal(&[start(2, Some(4095))]);
    torn.extend_from_slice(b"{\"event\":\"hedge_int");
    let torn_v2 = audit(&torn, None);
    assert_eq!(torn_v2.primary_leg.journal_coverage, "not_recorded");
    assert_eq!(torn_v2.primary_leg.status, "not_provided");
    assert!(!kinds(&torn_v2).contains(&"lighter_journal_prefix"));
    let v1 = run(&[start(1, None)], evidence());
    assert_eq!(v1.primary_leg.journal_coverage, "not_recorded");
    assert_eq!(v1.primary_leg.status, "missing_evidence");
}
