use super::super::journal::{IntentRecord, PrimaryObservation, StartRecord, TerminalRecord};
use super::*;
use serde_json::json;
use std::str::FromStr;

const CLOID: &str = "0x0123456789abcdef0123456789abcdef";
const OTHER: &str = "0xfedcba9876543210fedcba9876543210";
const ACCOUNT: &str = "0x00000000000000000000000000000000000000aa";
const SESSION: &str = "3f0c6a52-6a4b-4f53-9d7c-2b1e7f6f9a10";

fn d(value: &str) -> Decimal {
    Decimal::from_str(value).unwrap()
}

/// The supervised launcher's schema 2 start: Lighter account 7, market 0.
fn start() -> StartRecord {
    StartRecord {
        session_id: SESSION.into(),
        network: "Testnet".into(),
        lighter_account_index: 7,
        hyperliquid_account: Some(ACCOUNT.into()),
        symbol: "ETH".into(),
        journal_schema: Some(2),
        lighter_market_id: Some(0),
    }
}

/// Sell 0.01 ETH IOC, limit 1998, planned against a flat hedge.
fn intent(cloid: &str, before: &str) -> IntentRecord {
    IntentRecord {
        cloid: cloid.into(),
        coin: "ETH".into(),
        submitted_at_ms: 1_000,
        is_buy: false,
        size: d("0.01"),
        limit_price: d("1998"),
        reduce_only: false,
        primary_position: d("0.01"),
        hedge_position: d(before),
        primary_observation: Some(PrimaryObservation {
            account_index: 7,
            market_id: 0,
            position: d("0.01"),
            transaction_time: Some(900),
            request_us: 12_000,
            age_us: 15_000,
            max_age_us: 5_000_000,
        }),
    }
}

/// Partial fill 0.004 confirmed by a later account snapshot.
fn terminal(cloid: &str, average: Option<&str>) -> TerminalRecord {
    TerminalRecord {
        cloid: cloid.into(),
        filled: d("0.004"),
        average_price: average.map(d),
        position: d("-0.004"),
        terminal_source: Some(
            if average.is_some() {
                "response"
            } else {
                "lookup"
            }
            .into(),
        ),
        terminal_at_ms: Some(if average.is_some() { 1_000 } else { 1_500 }),
        account_time_ms: Some(1_600),
    }
}

fn journal(records: Vec<JournalRecord>) -> Vec<u8> {
    records.iter().flat_map(|r| r.to_line().unwrap()).collect()
}

fn reconciled(average: Option<&str>) -> Vec<u8> {
    journal(vec![
        JournalRecord::Start(start()),
        JournalRecord::Intent(intent(CLOID, "0")),
        JournalRecord::Terminal(terminal(CLOID, average)),
    ])
}

fn lookup() -> audit::LookupEvidence {
    audit::LookupEvidence {
        cloid: CLOID.into(),
        observed_at_ms: 1_510,
        response: json!({"status": "order", "order": {"status": "canceled", "statusTimestamp": 1_500,
            "order": {"coin": "ETH", "cloid": CLOID, "side": "A", "limitPx": "1998",
                "origSz": "0.01", "sz": "0.006", "oid": 8, "timestamp": 1_000}}}),
    }
}

fn venue_fill(tid: u64, sz: &str, fee: &str, token: &str) -> serde_json::Value {
    json!({"coin": "ETH", "px": "1999", "sz": sz, "side": "A", "time": 1_400,
        "startPosition": "0", "dir": "Open Short", "closedPnl": "0", "hash": "0xabc",
        "oid": 8, "crossed": true, "fee": fee, "tid": tid, "feeToken": token})
}

fn evidence(fills: Vec<serde_json::Value>) -> Evidence {
    Evidence {
        network: "Testnet".into(),
        hyperliquid_account: ACCOUNT.into(),
        coin: "ETH".into(),
        lookups: vec![lookup()],
        account_snapshot: None,
        lighter: None,
        fills: if fills.is_empty() {
            Vec::new()
        } else {
            vec![audit::FillsEvidence {
                observed_at_ms: 2_000,
                response: serde_json::Value::Array(fills),
            }]
        },
    }
}

fn paid() -> Evidence {
    evidence(vec![venue_fill(1, "0.004", "0.0035982", "USDC")])
}

fn info() -> crate::telemetry_sink::SessionInfo {
    crate::telemetry_sink::SessionInfo {
        session_id: SESSION.into(),
        network: "Testnet".into(),
        exchange: "lighter".into(),
        symbol: "ETH".into(),
        market_id: 0,
        account: 7,
        hedge_account: Some(ACCOUNT.into()),
    }
}

/// What `hedge_fill_record` exports for the reconciled IOC above at 4.5 bps.
fn record(price: f64, price_source: &'static str) -> crate::telemetry_sink::HedgeFillRecord {
    crate::telemetry_sink::HedgeFillRecord {
        venue: "hyperliquid",
        client_order_id: CLOID.into(),
        side: "sell",
        size: 0.004,
        price,
        price_source,
        fee: 0.004 * price * 4.5 / 10_000.0,
        fee_source: "configured_rate",
        position_after: -0.004,
    }
}

/// A telemetry file written by the real sink.
async fn written(
    session: crate::telemetry_sink::SessionInfo,
    fills: Vec<crate::telemetry_sink::HedgeFillRecord>,
) -> Vec<u8> {
    let dir = std::env::temp_dir().join(format!("aleph-fee-view-{}", uuid::Uuid::new_v4()));
    let sink = crate::telemetry_sink::TelemetrySink::open(&dir, &session).unwrap();
    for fill in fills {
        sink.hedge_fill(fill);
    }
    sink.stopped().await.unwrap();
    let bytes = std::fs::read(dir.join(format!("{}.jsonl", session.session_id))).unwrap();
    std::fs::remove_dir_all(dir).unwrap();
    bytes
}

fn has(view: &FeeView, text: &str) -> bool {
    view.reasons.iter().any(|r| r.contains(text))
}

#[tokio::test]
async fn one_reconciled_ioc_binds_both_costs_without_subtracting_them() {
    let telemetry = written(info(), vec![record(1999.0, "fill")]).await;
    let view = build(&reconciled(Some("1999")), &paid(), &telemetry, CLOID);
    assert_eq!(view.status, ViewStatus::Bound, "{:?}", view.reasons);
    assert_eq!(view.status.exit_code(), 0);
    assert_eq!(view.audit_status, AuditStatus::JournalRecorded);
    let ioc = view.ioc.as_ref().unwrap();
    assert_eq!(
        (ioc.status, ioc.filled, ioc.average_price),
        (OrderStatus::Recorded, Some(d("0.004")), Some(d("1999")))
    );
    // The writer's own tokens, unchanged.
    let t = &view.telemetry;
    assert_eq!(t.status, ViewStatus::Bound);
    assert_eq!(t.lines, [2]);
    assert_eq!(t.stopped_line, Some(3));
    assert_eq!(
        (
            t.size.as_deref(),
            t.price.as_deref(),
            t.price_source.as_deref()
        ),
        (Some("0.004"), Some("1999.0"), Some("fill"))
    );
    assert_eq!(
        t.fee.as_deref().map(|f| f.parse::<f64>().unwrap()),
        Some(0.004 * 1999.0 * 4.5 / 10_000.0)
    );
    assert_eq!(t.fee_source.as_deref(), Some("configured_rate"));
    let venue = view.venue_fee.as_ref().unwrap();
    assert_eq!(venue.status, "attributed");
    assert_eq!(
        venue
            .fees
            .iter()
            .map(|f| (f.token.as_str(), f.fee))
            .collect::<Vec<_>>(),
        [("USDC", d("0.0035982"))]
    );
    // Shown side by side, never subtracted.
    assert_eq!(view.comparison.status, "not_comparable");
    assert!(view.comparison.reasons[0].contains("names no asset"));
    assert!(
        view.boundaries
            .iter()
            .any(|b| b.contains("never subtracted"))
    );
    // Deterministic for the same inputs.
    assert_eq!(
        view,
        build(&reconciled(Some("1999")), &paid(), &telemetry, CLOID)
    );
}

/// Any identity the launcher writes into both files must match; one wrong
/// field makes the files foreign to each other.
#[tokio::test]
async fn telemetry_from_another_session_account_or_market_is_invalid() {
    type Edit = fn(&mut crate::telemetry_sink::SessionInfo);
    let cases: [(&str, Edit, &str); 6] = [
        (
            "session",
            |s| s.session_id = "other-session".into(),
            "session_id",
        ),
        ("network", |s| s.network = "Mainnet".into(), "network"),
        ("account", |s| s.account = 8, "account"),
        ("market", |s| s.market_id = 1, "market_id"),
        ("symbol", |s| s.symbol = "BTC".into(), "symbol"),
        ("hedge account", |s| s.hedge_account = None, "hedge_account"),
    ];
    for (name, edit, field) in cases {
        let mut session = info();
        edit(&mut session);
        let telemetry = written(session, vec![record(1999.0, "fill")]).await;
        let view = build(&reconciled(Some("1999")), &paid(), &telemetry, CLOID);
        assert_eq!(view.status, ViewStatus::Invalid, "{name}");
        assert_eq!(view.status.exit_code(), 3, "{name}");
        assert!(has(&view, field), "{name}: {:?}", view.reasons);
    }
    // The hedge account is compared case-insensitively, as the audit does.
    let mut upper = info();
    upper.hedge_account = Some(ACCOUNT.to_uppercase().replacen("0X", "0x", 1));
    let telemetry = written(upper, vec![record(1999.0, "fill")]).await;
    let view = build(&reconciled(Some("1999")), &paid(), &telemetry, CLOID);
    assert_eq!(view.status, ViewStatus::Bound, "{:?}", view.reasons);
}

/// Mixed sessions, records after `stopped`, conflicting repeats and a fill
/// the journal never confirmed are contradictions; an exact repeat is not.
#[tokio::test]
async fn mixed_conflicting_or_unjournaled_records_are_invalid() {
    let base = written(info(), vec![record(1999.0, "fill")]).await;
    let text = String::from_utf8(base.clone()).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    let (session, fill, stopped) = (lines[0], lines[1], lines[2]);
    let file = |lines: &[&str]| {
        lines
            .iter()
            .map(|l| format!("{l}\n"))
            .collect::<String>()
            .into_bytes()
    };
    let other = written(
        crate::telemetry_sink::SessionInfo {
            session_id: "s-2".into(),
            ..info()
        },
        vec![],
    )
    .await;
    let foreign_session = String::from_utf8(other)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .to_owned();
    let altered = fill
        .replace("\"fee\":", "\"fee\":1e-9,\"x\":0,\"dummy\":")
        .to_owned();
    let second_write = {
        let mut value: serde_json::Value = serde_json::from_str(fill).unwrap();
        value["fee"] = json!(0.0001);
        value.to_string()
    };
    let journal = reconciled(Some("1999"));
    let cases: Vec<(&str, Vec<u8>, &str)> = vec![
        (
            "two sessions",
            file(&[session, &foreign_session, fill, stopped]),
            "second session",
        ),
        (
            "after stopped",
            file(&[session, stopped, fill]),
            "follows the stopped marker",
        ),
        (
            "conflicting repeat",
            file(&[session, fill, &second_write, stopped]),
            "disagree",
        ),
        (
            "unknown field",
            file(&[session, &altered, stopped]),
            "malformed hedge_fill",
        ),
        (
            "session not first",
            file(&[fill, session, stopped]),
            "not the session record",
        ),
        ("empty", Vec::new(), "empty"),
    ];
    for (name, telemetry, reason) in cases {
        let view = build(&journal, &paid(), &telemetry, CLOID);
        assert_eq!(view.status, ViewStatus::Invalid, "{name}");
        assert!(has(&view, reason), "{name}: {:?}", view.reasons);
    }
    // A byte-identical copy of the line is the same claim, noted only.
    let view = build(
        &journal,
        &paid(),
        &file(&[session, fill, fill, stopped]),
        CLOID,
    );
    assert_eq!(view.status, ViewStatus::Bound, "{:?}", view.reasons);
    assert_eq!(view.telemetry.lines, [2, 3]);
    assert!(view.telemetry.notes[0].contains("repeats line 2"));
    // The writer exports only after the journal's terminal: a fill for an
    // IOC that is still open did not come from this journal's session.
    let open = journal_with_open_intent();
    let view = build(&open, &paid(), &base, CLOID);
    assert_eq!(view.status, ViewStatus::Invalid);
    assert!(
        has(&view, "never recorded as terminal"),
        "{:?}",
        view.reasons
    );
}

fn journal_with_open_intent() -> Vec<u8> {
    journal(vec![
        JournalRecord::Start(start()),
        JournalRecord::Intent(intent(CLOID, "0")),
    ])
}

/// The view selects the exact CLOID; another IOC's record never stands in.
#[tokio::test]
async fn the_ioc_is_chosen_by_exact_cloid_only() {
    let telemetry = written(info(), vec![record(1999.0, "fill")]).await;
    let one = reconciled(Some("1999"));
    for (cloid, reason) in [
        (OTHER, "no journaled IOC"),
        ("0x0123", "16-byte hex"),
        (
            &CLOID.to_uppercase().replacen("0X", "0x", 1),
            "no journaled IOC",
        ),
    ] {
        let view = build(&one, &paid(), &telemetry, cloid);
        assert_eq!(view.status, ViewStatus::Invalid, "{cloid}");
        assert!(has(&view, reason), "{cloid}: {:?}", view.reasons);
        assert!(view.telemetry.lines.is_empty(), "{cloid}");
    }
    // A second reconciled IOC in the same files does not leak into this one.
    let two = journal(vec![
        JournalRecord::Start(start()),
        JournalRecord::Intent(intent(CLOID, "0")),
        JournalRecord::Terminal(terminal(CLOID, Some("1999"))),
        JournalRecord::Intent(IntentRecord {
            cloid: OTHER.into(),
            submitted_at_ms: 2_000,
            ..intent(OTHER, "-0.004")
        }),
    ]);
    let mut other = record(1999.0, "fill");
    other.client_order_id = OTHER.into();
    other.fee = 9.0;
    let telemetry = written(info(), vec![record(1999.0, "fill"), other]).await;
    let view = build(&two, &paid(), &telemetry, CLOID);
    assert_eq!(view.telemetry.lines, [2]);
    assert_eq!(view.ioc.as_ref().unwrap().intent_line, 2);
}

/// A lookup terminal has no average price, so the writer prices the
/// estimate at the IOC limit. The view keeps that label and says why the
/// numbers are not comparable; several fee assets are never added.
#[tokio::test]
async fn limit_priced_estimates_and_multi_asset_fees_stay_labeled_apart() {
    let telemetry = written(info(), vec![record(1998.0, "limit")]).await;
    let mut built = venue_fill(2, "0.001", "0.05", "HYPE");
    built["builderFee"] = json!("0.001");
    let fees = evidence(vec![venue_fill(1, "0.003", "-0.0002", "USDC"), built]);
    let view = build(&reconciled(None), &fees, &telemetry, CLOID);
    assert_eq!(view.status, ViewStatus::Bound, "{:?}", view.reasons);
    let ioc = view.ioc.as_ref().unwrap();
    assert_eq!(
        (ioc.average_price, ioc.terminal_source.as_deref()),
        (None, Some("lookup"))
    );
    assert_eq!(view.telemetry.price_source.as_deref(), Some("limit"));
    assert_eq!(view.telemetry.price.as_deref(), Some("1998.0"));
    let venue = view.venue_fee.as_ref().unwrap();
    assert_eq!(
        venue
            .fees
            .iter()
            .map(|f| (f.token.as_str(), f.fee))
            .collect::<Vec<_>>(),
        [("HYPE", d("0.05")), ("USDC", d("-0.0002"))]
    );
    let why = view.comparison.reasons.join(" | ");
    for expected in [
        "names no asset",
        "IOC limit",
        "2 fee assets",
        "builder fee",
        "rebate",
    ] {
        assert!(why.contains(expected), "{expected}: {why}");
    }
    assert_eq!(view.comparison.status, "not_comparable");
    // Telemetry claiming a fill price where the journal has none is wrong.
    let claimed = written(info(), vec![record(1998.0, "fill")]).await;
    let view = build(&reconciled(None), &fees, &claimed, CLOID);
    assert_eq!(view.status, ViewStatus::Invalid);
    assert!(has(&view, "implies `limit`"), "{:?}", view.reasons);
}

/// Each side can be missing on its own. A missing fee is unknown, never 0.
#[tokio::test]
async fn one_missing_side_is_unresolved_and_never_read_as_zero() {
    let journal = reconciled(Some("1999"));
    let telemetry = written(info(), vec![record(1999.0, "fill")]).await;
    // No fills in the evidence: the venue fee is unknown.
    let view = build(&journal, &evidence(vec![]), &telemetry, CLOID);
    assert_eq!(view.status, ViewStatus::Unresolved);
    assert_eq!(view.status.exit_code(), 2);
    assert_eq!(view.venue_fee.as_ref().unwrap().status, "not_provided");
    assert!(
        has(&view, "the evidence has no fills"),
        "{:?}",
        view.reasons
    );
    assert!(view.venue_fee.as_ref().unwrap().fees.is_empty());
    assert!(
        view.comparison
            .reasons
            .iter()
            .any(|r| r.contains("not_provided"))
    );
    // Fills short of the reconciled size leave the fee unattributed.
    let short = evidence(vec![venue_fill(1, "0.003", "0.0027", "USDC")]);
    let view = build(&journal, &short, &telemetry, CLOID);
    assert_eq!(view.status, ViewStatus::Unresolved);
    assert_eq!(view.venue_fee.as_ref().unwrap().status, "unattributed");
    // Fills above the reconciled size contradict it.
    let excess = evidence(vec![venue_fill(1, "0.005", "0.0045", "USDC")]);
    assert_eq!(
        build(&journal, &excess, &telemetry, CLOID).status,
        ViewStatus::Invalid
    );
    // No telemetry record for a reconciled fill: the estimate is unknown.
    let none = written(info(), vec![]).await;
    let view = build(&journal, &paid(), &none, CLOID);
    assert_eq!(view.status, ViewStatus::Unresolved);
    assert!(view.telemetry.fee.is_none());
    assert!(
        has(&view, "no hedge_fill for this CLOID"),
        "{:?}",
        view.reasons
    );
    // A torn tail may hide the record: unresolved, not missing.
    let mut torn = none.clone();
    torn.extend_from_slice(br#"{"type":"hedge_fill","ts_ms":5,"client"#);
    let view = build(&journal, &paid(), &torn, CLOID);
    assert_eq!(view.status, ViewStatus::Unresolved);
    assert!(has(&view, "lacks its newline"), "{:?}", view.reasons);
    // An older writer without fee_source is unknown provenance, still bound.
    let text = String::from_utf8(telemetry.clone()).unwrap();
    let legacy = text.replace(",\"fee_source\":\"configured_rate\"", "");
    assert_ne!(legacy, text);
    let view = build(&journal, &paid(), legacy.as_bytes(), CLOID);
    assert_eq!(view.status, ViewStatus::Bound, "{:?}", view.reasons);
    assert_eq!(view.telemetry.fee_source.as_deref(), Some("unknown"));
    assert!(
        view.comparison
            .reasons
            .iter()
            .any(|r| r.contains("how its fee was derived"))
    );
}

/// An IOC that is not reconciled has no fill for either fee to describe.
#[tokio::test]
async fn unreconciled_or_unfilled_iocs_bind_no_cost() {
    let empty = written(info(), vec![]).await;
    let view = build(&journal_with_open_intent(), &paid(), &empty, CLOID);
    assert_eq!(view.status, ViewStatus::Unresolved, "{:?}", view.reasons);
    assert_eq!(view.ioc.as_ref().unwrap().status, OrderStatus::Pending);
    assert!(has(&view, "not reconciled"), "{:?}", view.reasons);
    assert_eq!(view.venue_fee.as_ref().unwrap().status, "unattributed");
    // A zero fill exports no telemetry and has no fee to compare.
    let mut zero = terminal(CLOID, None);
    zero.filled = Decimal::ZERO;
    zero.position = Decimal::ZERO;
    let unfilled = journal(vec![
        JournalRecord::Start(start()),
        JournalRecord::Intent(intent(CLOID, "0")),
        JournalRecord::Terminal(zero),
    ]);
    // No lookup: the shared one shows 0.004 filled, which would contradict.
    let no_lookup = Evidence {
        lookups: Vec::new(),
        ..evidence(vec![])
    };
    let view = build(&unfilled, &no_lookup, &empty, CLOID);
    assert_eq!(view.status, ViewStatus::Unresolved, "{:?}", view.reasons);
    assert!(has(&view, "did not fill"), "{:?}", view.reasons);
}

/// The telemetry record must be the one `hedge_fill_record` derives from
/// the journaled terminal; any field that disagrees is a contradiction.
#[tokio::test]
async fn a_record_that_disagrees_with_the_journal_is_invalid() {
    type Edit = fn(&mut crate::telemetry_sink::HedgeFillRecord);
    let cases: [(&str, Edit, &str); 6] = [
        ("size", |r| r.size = 0.005, "journaled fill"),
        ("side", |r| r.side = "buy", "side"),
        ("price", |r| r.price = 2000.0, "journaled fill price"),
        (
            "position",
            |r| r.position_after = -0.005,
            "journaled position",
        ),
        (
            "fee above any rate",
            |r| r.fee = 1.0,
            "configured rate up to",
        ),
        ("negative fee", |r| r.fee = -0.0001, "outside"),
    ];
    for (name, edit, reason) in cases {
        let mut fill = record(1999.0, "fill");
        edit(&mut fill);
        let telemetry = written(info(), vec![fill]).await;
        let view = build(&reconciled(Some("1999")), &paid(), &telemetry, CLOID);
        assert_eq!(view.status, ViewStatus::Invalid, "{name}");
        assert!(has(&view, reason), "{name}: {:?}", view.reasons);
    }
    // The configurable maximum rate itself is accepted.
    let mut max = record(1999.0, "fill");
    max.fee = 0.004 * 1999.0 * MAX_TAKER_FEE_BPS / 10_000.0;
    let telemetry = written(info(), vec![max]).await;
    let view = build(&reconciled(Some("1999")), &paid(), &telemetry, CLOID);
    assert_eq!(view.status, ViewStatus::Bound, "{:?}", view.reasons);
}

/// Evidence for another account invalidates the view, as in the audit.
#[tokio::test]
async fn evidence_for_another_account_is_invalid() {
    let telemetry = written(info(), vec![record(1999.0, "fill")]).await;
    let mut foreign = paid();
    foreign.hyperliquid_account = format!("0x{:040x}", 0xbb);
    let view = build(&reconciled(Some("1999")), &foreign, &telemetry, CLOID);
    assert_eq!(view.status, ViewStatus::Invalid);
    assert!(
        has(&view, "another network, account or coin"),
        "{:?}",
        view.reasons
    );
    assert_eq!(view.venue_fee.as_ref().unwrap().status, "invalid");
}

/// The file entry point reports each input's hash and size, reads only, and
/// gives the same view twice.
#[tokio::test]
async fn file_view_hashes_inputs_and_leaves_them_unchanged() {
    let dir = std::env::temp_dir().join(format!("aleph-fee-files-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&dir).unwrap();
    let (journal_path, evidence_path, telemetry_path) = (
        dir.join("journal.jsonl"),
        dir.join("evidence.json"),
        dir.join("telemetry.jsonl"),
    );
    let journal_bytes = reconciled(Some("1999"));
    let telemetry_bytes = written(info(), vec![record(1999.0, "fill")]).await;
    let evidence_bytes = serde_json::to_vec_pretty(&json!({
        "network": "Testnet", "hyperliquid_account": ACCOUNT, "coin": "ETH",
        "lookups": [{"cloid": CLOID, "observed_at_ms": 1_510, "response": lookup().response}],
        "fills": [{"observed_at_ms": 2_000,
            "response": [venue_fill(1, "0.004", "0.0035982", "USDC")]}]}))
    .unwrap();
    std::fs::write(&journal_path, &journal_bytes).unwrap();
    std::fs::write(&evidence_path, &evidence_bytes).unwrap();
    std::fs::write(&telemetry_path, &telemetry_bytes).unwrap();
    let first = view_files(&journal_path, &evidence_path, &telemetry_path, CLOID).unwrap();
    let second = view_files(&journal_path, &evidence_path, &telemetry_path, CLOID).unwrap();
    assert_eq!(first, second);
    assert_eq!(first.status, ViewStatus::Bound, "{:?}", first.reasons);
    let inputs = first.inputs.as_ref().unwrap();
    assert_eq!(
        inputs.journal.sha256,
        hex::encode(Sha256::digest(&journal_bytes))
    );
    assert_eq!(inputs.telemetry.bytes, telemetry_bytes.len());
    assert_eq!(std::fs::read(&journal_path).unwrap(), journal_bytes);
    assert_eq!(std::fs::read(&evidence_path).unwrap(), evidence_bytes);
    assert_eq!(std::fs::read(&telemetry_path).unwrap(), telemetry_bytes);
    // The audit of the same files is unchanged by the view.
    let audit_report = audit::audit_files(&journal_path, Some(&evidence_path)).unwrap();
    assert_eq!(audit_report.status, first.audit_status);
    std::fs::remove_dir_all(dir).unwrap();
}

/// examples/hedge_fee_view/: one reconciled IOC from the real journal and
/// telemetry writers plus raw-format venue evidence, for the CLI. A fixed
/// `ts_ms` keeps the telemetry stable. ALEPH_WRITE_FIXTURES=1 rewrites it.
#[tokio::test]
async fn cli_example_matches_the_writers() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/hedge_fee_view");
    let telemetry = written(info(), vec![record(1999.0, "fill")]).await;
    let stable: String = String::from_utf8(telemetry)
        .unwrap()
        .lines()
        .enumerate()
        .map(|(i, line)| {
            let mut value: serde_json::Value = serde_json::from_str(line).unwrap();
            value["ts_ms"] = (1_000 * (i as u64 + 1)).into();
            serde_json::to_string(&value).unwrap() + "\n"
        })
        .collect();
    let evidence = serde_json::to_string_pretty(&json!({
        "network": "Testnet", "hyperliquid_account": ACCOUNT, "coin": "ETH",
        "lookups": [{"cloid": CLOID, "observed_at_ms": 1_510, "response": lookup().response}],
        "fills": [{"observed_at_ms": 2_000,
            "response": [venue_fill(1, "0.004", "0.0035982", "USDC")]}]}))
    .unwrap()
        + "\n";
    let files = [
        ("journal.jsonl", reconciled(Some("1999"))),
        ("evidence.json", evidence.into_bytes()),
        ("telemetry.jsonl", stable.into_bytes()),
    ];
    if std::env::var_os("ALEPH_WRITE_FIXTURES").is_some() {
        std::fs::create_dir_all(&dir).unwrap();
        for (name, bytes) in &files {
            std::fs::write(dir.join(name), bytes).unwrap();
        }
    }
    for (name, bytes) in &files {
        let path = dir.join(name);
        let committed = std::fs::read(&path).expect("example missing (ALEPH_WRITE_FIXTURES=1)");
        assert_eq!(&committed, bytes, "{name} differs from the writers");
    }
    let view = view_files(
        &dir.join("journal.jsonl"),
        &dir.join("evidence.json"),
        &dir.join("telemetry.jsonl"),
        CLOID,
    )
    .unwrap();
    assert_eq!(view.status, ViewStatus::Bound, "{:?}", view.reasons);
}
