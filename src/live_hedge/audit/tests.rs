use super::*;
use serde_json::json;
use std::str::FromStr;

const CLOID: &str = "0x0123456789abcdef0123456789abcdef";
const OTHER: &str = "0xfedcba9876543210fedcba9876543210";
const ACCOUNT: &str = "0x00000000000000000000000000000000000000aa";

fn d(value: &str) -> Decimal {
    Decimal::from_str(value).unwrap()
}

fn start(schema: Option<u32>) -> JournalRecord {
    JournalRecord::Start(StartRecord {
        session_id: "s-1".into(),
        network: "Testnet".into(),
        lighter_account_index: 7,
        hyperliquid_account: Some(ACCOUNT.into()),
        symbol: "ETH".into(),
        journal_schema: schema,
        lighter_market_id: None,
    })
}

/// Sell 0.01 ETH IOC planned against a flat hedge.
fn intent(cloid: &str, before: &str) -> JournalRecord {
    JournalRecord::Intent(IntentRecord {
        cloid: cloid.into(),
        coin: "ETH".into(),
        submitted_at_ms: 1_000,
        is_buy: false,
        size: d("0.01"),
        limit_price: d("1998"),
        reduce_only: false,
        primary_position: d("0.01"),
        hedge_position: d(before),
        primary_observation: None,
    })
}

fn terminal(cloid: &str, filled: &str, position: &str) -> JournalRecord {
    JournalRecord::Terminal(TerminalRecord {
        cloid: cloid.into(),
        filled: d(filled),
        average_price: Some(d("1999")),
        position: d(position),
        terminal_source: Some("lookup".into()),
        terminal_at_ms: Some(1_500),
        account_time_ms: Some(1_600),
    })
}

fn journal(records: &[JournalRecord]) -> Vec<u8> {
    records.iter().flat_map(|r| r.to_line().unwrap()).collect()
}

fn lookup_response(status: &str, remaining: &str, status_at: u64) -> serde_json::Value {
    json!({"status": "order", "order": {"status": status, "statusTimestamp": status_at,
        "order": {"coin": "ETH", "cloid": CLOID, "side": "A", "limitPx": "1998",
            "origSz": "0.01", "sz": remaining, "oid": 8, "timestamp": 1_000}}})
}

fn lookup(response: serde_json::Value, observed_at_ms: u64) -> LookupEvidence {
    LookupEvidence {
        cloid: CLOID.into(),
        observed_at_ms,
        response,
    }
}

fn snapshot(position: &str, time: u64) -> SnapshotEvidence {
    SnapshotEvidence {
        observed_at_ms: time + 10,
        state: json!({"time": time, "withdrawable": "100",
            "marginSummary": {"accountValue": "100", "totalNtlPos": "0", "totalMarginUsed": "0"},
            "assetPositions": [{"position": {"coin": "ETH", "szi": position, "unrealizedPnl": "0"}}]}),
    }
}

fn evidence(lookups: Vec<LookupEvidence>, account: Option<SnapshotEvidence>) -> Evidence {
    Evidence {
        network: "Testnet".into(),
        hyperliquid_account: ACCOUNT.into(),
        coin: "ETH".into(),
        lookups,
        account_snapshot: account,
        lighter: None,
        fills: Vec::new(),
    }
}

fn kinds(report: &Report) -> Vec<&'static str> {
    report.issues.iter().map(|i| i.kind).collect()
}

#[test]
fn intent_without_terminal_keeps_the_full_ioc_as_possible_exposure() {
    let report = audit(&journal(&[start(Some(1)), intent(CLOID, "0")]), None);
    assert_eq!(report.status, AuditStatus::Unresolved);
    assert!(!report.restart_permitted);
    let order = &report.orders[0];
    assert_eq!(
        (order.status, order.journal, order.evidence),
        (OrderStatus::Pending, "intent_only", "none")
    );
    assert_eq!(order.filled, None);
    // A sell of up to 0.01 from flat: anywhere in [-0.01, 0].
    assert_eq!(
        (order.possible_position_min, order.possible_position_max),
        (Some(d("-0.01")), Some(d("0")))
    );
    assert_eq!(report.hedge_position.possible_min, Some(d("-0.01")));
    assert_eq!(report.journal.session_cleanup, "unverified");
    assert!(kinds(&report).contains(&"primary_leg_unverified"));
}

#[test]
fn evidence_releases_an_ioc_only_with_a_terminal_and_a_later_matching_account() {
    let crashed = journal(&[start(Some(1)), intent(CLOID, "0")]);
    let terminal_lookup = lookup(lookup_response("canceled", "0.006", 1_500), 1_510);
    let step = |lookups, account| {
        let report = audit(&crashed, Some(&evidence(lookups, account)));
        let order = report.orders[0].clone();
        (report.status, order.status, order.evidence)
    };
    // Unknown lookup: never proof of a zero fill.
    let unknown = lookup(json!({"status": "unknownOid"}), 1_200);
    assert_eq!(
        step(vec![unknown.clone()], None),
        (
            AuditStatus::Unresolved,
            OrderStatus::Pending,
            "lookup_unknown"
        )
    );
    // Still resting at the venue.
    assert_eq!(
        step(
            vec![lookup(lookup_response("open", "0.01", 1_100), 1_150)],
            None
        ),
        (AuditStatus::Unresolved, OrderStatus::Pending, "lookup_open")
    );
    // Terminal without an account snapshot.
    assert_eq!(
        step(vec![terminal_lookup.clone()], None),
        (
            AuditStatus::Unresolved,
            OrderStatus::Pending,
            "lookup_terminal"
        )
    );
    // Snapshot older than the terminal does not release it.
    assert_eq!(
        step(
            vec![terminal_lookup.clone()],
            Some(snapshot("-0.004", 1_400))
        ),
        (
            AuditStatus::Unresolved,
            OrderStatus::Pending,
            "lookup_terminal"
        )
    );
    // Later snapshot with exactly the terminal's fill: matched once.
    let report = audit(
        &crashed,
        Some(&evidence(
            vec![unknown, terminal_lookup.clone()],
            Some(snapshot("-0.004", 1_600)),
        )),
    );
    assert_eq!(report.status, AuditStatus::EvidenceMatched);
    assert_eq!(report.status.exit_code(), 0);
    assert!(!report.restart_permitted);
    let order = &report.orders[0];
    assert_eq!(
        (order.status, order.evidence),
        (OrderStatus::Matched, "matched")
    );
    assert_eq!(
        (order.filled, order.position_after),
        (Some(d("0.004")), Some(d("-0.004")))
    );
    assert_eq!(report.hedge_position.possible_min, Some(d("-0.004")));
    assert_eq!(report.hedge_position.possible_max, Some(d("-0.004")));
    // Later snapshot still at the planned position: account not caught up.
    assert_eq!(
        step(
            vec![lookup(lookup_response("canceled", "0.006", 1_500), 1_510)],
            Some(snapshot("0", 1_600))
        ),
        (
            AuditStatus::Unresolved,
            OrderStatus::Pending,
            "lookup_terminal"
        )
    );
    // Later snapshot that moved by a different amount is a conflict.
    assert_eq!(
        step(vec![terminal_lookup], Some(snapshot("-0.005", 1_600))),
        (AuditStatus::Invalid, OrderStatus::Invalid, "conflict")
    );
}

#[test]
fn single_field_evidence_mismatches_are_rejected_not_matched() {
    let crashed = journal(&[start(Some(1)), intent(CLOID, "0")]);
    let good = || {
        evidence(
            vec![lookup(lookup_response("canceled", "0.006", 1_500), 1_510)],
            Some(snapshot("-0.004", 1_600)),
        )
    };
    assert_eq!(
        audit(&crashed, Some(&good())).status,
        AuditStatus::EvidenceMatched
    );

    let mut wrong = Vec::new();
    let mut e = good();
    e.network = "Mainnet".into();
    wrong.push(("network", e));
    let mut e = good();
    e.hyperliquid_account = "0x00000000000000000000000000000000000000bb".into();
    wrong.push(("account", e));
    let mut e = good();
    e.coin = "BTC".into();
    wrong.push(("coin", e));
    let mut e = good();
    e.lookups[0].response["order"]["order"]["cloid"] = OTHER.into();
    wrong.push(("response cloid", e));
    let mut e = good();
    e.lookups[0].response["order"]["order"]["origSz"] = "0.02".into();
    wrong.push(("size", e));
    let mut e = good();
    e.lookups[0].response["order"]["order"]["side"] = "B".into();
    wrong.push(("side", e));
    let mut e = good();
    // Captured more than CLOCK_TOLERANCE_MS before its own status time.
    e.lookups[0].response["order"]["statusTimestamp"] = 5_000.into();
    e.lookups[0].observed_at_ms = 2_999;
    wrong.push(("capture time", e));
    let mut e = good();
    e.lookups[0].cloid = OTHER.into();
    wrong.push(("evidence cloid", e));
    let mut e = good();
    e.account_snapshot.as_mut().unwrap().state["assetPositions"][0]["position"]["coin"] =
        "BTC".into();
    wrong.push(("snapshot instrument", e));
    // Live tolerance: a status up to 1 s before the intent time still binds
    // (intent time precedes fsync and signing); earlier than that does not.
    let late_intent = journal(&[
        start(Some(1)),
        JournalRecord::Intent(IntentRecord {
            submitted_at_ms: 2_501,
            ..match intent(CLOID, "0") {
                JournalRecord::Intent(i) => i,
                _ => unreachable!(),
            }
        }),
    ]);
    let edge = JournalRecord::Intent(IntentRecord {
        submitted_at_ms: 2_500,
        ..match intent(CLOID, "0") {
            JournalRecord::Intent(i) => i,
            _ => unreachable!(),
        }
    });
    assert_eq!(
        audit(&journal(&[start(Some(1)), edge]), Some(&good())).status,
        AuditStatus::EvidenceMatched
    );
    let report = audit(&late_intent, Some(&good()));
    assert_eq!(report.status, AuditStatus::Invalid, "status before intent");
    for (field, evidence) in wrong {
        let report = audit(&crashed, Some(&evidence));
        assert_ne!(
            report.status,
            AuditStatus::EvidenceMatched,
            "{field} still matched"
        );
        assert!(
            report.orders[0].status != OrderStatus::Matched,
            "{field}: order released"
        );
    }
}

#[test]
fn journaled_terminals_are_recorded_and_contradicting_evidence_is_flagged() {
    let complete = journal(&[
        start(Some(1)),
        intent(CLOID, "0"),
        terminal(CLOID, "0.004", "-0.004"),
    ]);
    let report = audit(&complete, None);
    assert_eq!(report.status, AuditStatus::JournalRecorded);
    assert_eq!(report.orders[0].journal, "recorded");
    assert_eq!(report.hedge_position.journal_confirmed, d("-0.004"));
    let agrees = evidence(
        vec![lookup(lookup_response("canceled", "0.006", 1_500), 1_510)],
        None,
    );
    assert_eq!(
        audit(&complete, Some(&agrees)).orders[0].evidence,
        "lookup_agrees"
    );
    let disagrees = evidence(
        vec![lookup(lookup_response("canceled", "0.005", 1_500), 1_510)],
        None,
    );
    assert_eq!(
        audit(&complete, Some(&disagrees)).status,
        AuditStatus::Invalid
    );
    // A later account snapshot that moved without a journaled IOC.
    let moved = evidence(vec![], Some(snapshot("-0.009", 2_000)));
    let report = audit(&complete, Some(&moved));
    assert_eq!(report.status, AuditStatus::Invalid);
    assert!(kinds(&report).contains(&"unexplained_position"));
}

#[test]
fn duplicates_are_idempotent_but_conflicts_and_orphans_are_invalid() {
    let base = [
        start(Some(1)),
        intent(CLOID, "0"),
        terminal(CLOID, "0.004", "-0.004"),
    ];
    let mut exact = base.to_vec();
    exact.push(terminal(CLOID, "0.004", "-0.004"));
    exact.insert(2, intent(CLOID, "0"));
    let report = audit(&journal(&exact), None);
    assert_eq!(report.status, AuditStatus::JournalRecorded);
    assert_eq!(report.orders.len(), 1);
    assert_eq!(report.orders[0].terminal_lines, vec![4, 5]);

    let cases: Vec<(&str, Vec<JournalRecord>)> = vec![
        ("conflicting_terminal", {
            let mut r = base.to_vec();
            r.push(terminal(CLOID, "0.005", "-0.005"));
            r
        }),
        (
            "orphan_terminal",
            vec![start(Some(1)), terminal(OTHER, "0.004", "-0.004")],
        ),
        (
            "invalid_terminal",
            vec![
                start(Some(1)),
                intent(CLOID, "0"),
                terminal(CLOID, "0.02", "-0.02"),
            ],
        ),
        (
            "invalid_terminal",
            vec![
                start(Some(1)),
                intent(CLOID, "0"),
                terminal(CLOID, "0.004", "-0.003"),
            ],
        ),
        (
            "invalid_intent",
            vec![start(Some(1)), intent(CLOID, "0"), intent(OTHER, "0")],
        ),
        (
            "invalid_intent",
            vec![start(Some(1)), intent(CLOID, "-0.001")],
        ),
        ("record_before_start", vec![intent(CLOID, "0")]),
        ("misplaced_start", vec![start(Some(1)), start(Some(1))]),
        (
            "flat_with_pending_ioc",
            vec![
                start(Some(1)),
                intent(CLOID, "0"),
                JournalRecord::VerifiedFlat,
            ],
        ),
        ("flat_with_position", {
            let mut r = base.to_vec();
            r.push(JournalRecord::VerifiedFlat);
            r
        }),
    ];
    for (kind, records) in cases {
        let report = audit(&journal(&records), None);
        assert_eq!(report.status, AuditStatus::Invalid, "{kind}");
        assert!(
            kinds(&report).contains(&kind),
            "{kind}: {:?}",
            kinds(&report)
        );
    }
}

#[test]
fn truncated_and_malformed_lines_are_reported_never_dropped() {
    let mut bytes = journal(&[start(Some(1)), intent(CLOID, "0")]);
    let complete_terminal = terminal(CLOID, "0.004", "-0.004").to_line().unwrap();
    // Crash mid-write of the terminal: the IOC stays pending.
    bytes.extend_from_slice(&complete_terminal[..complete_terminal.len() / 2]);
    let report = audit(&bytes, None);
    assert_eq!(report.status, AuditStatus::Unresolved);
    assert_eq!(report.orders[0].status, OrderStatus::Pending);
    assert_eq!(
        report
            .issues
            .iter()
            .find(|i| i.kind == "truncated_tail")
            .unwrap()
            .line,
        Some(3)
    );
    // A complete terminal JSON missing only its newline is also not durable.
    let mut bytes = journal(&[start(Some(1)), intent(CLOID, "0")]);
    bytes.extend_from_slice(&complete_terminal[..complete_terminal.len() - 1]);
    assert_eq!(audit(&bytes, None).orders[0].status, OrderStatus::Pending);
    // An intent cut before its newline is still possible exposure.
    let mut bytes = journal(&[start(Some(1))]);
    let line = intent(CLOID, "0").to_line().unwrap();
    bytes.extend_from_slice(&line[..line.len() - 1]);
    let report = audit(&bytes, None);
    assert_eq!(report.orders.len(), 1);
    assert_eq!(report.orders[0].status, OrderStatus::Pending);
    // Mid-file garbage, unknown events and unknown fields are invalid.
    for bad in [
        &b"{not json"[..],
        br#"{"event":"hedge_cancel","cloid":"x"}"#,
        br#"{"cloid":"x"}"#,
        br#"{"event":"verified_flat","extra":1}"#,
    ] {
        let mut bytes = journal(&[start(Some(1))]);
        bytes.extend_from_slice(bad);
        bytes.push(b'\n');
        bytes.extend(journal(&[intent(CLOID, "0")]));
        let report = audit(&bytes, None);
        assert_eq!(report.status, AuditStatus::Invalid);
        assert_eq!(
            report
                .issues
                .iter()
                .find(|i| i.severity == Severity::Invalid)
                .unwrap()
                .line,
            Some(2)
        );
        assert_eq!(report.orders.len(), 1, "later records are still audited");
    }
    let mut fields = serde_json::to_value(
        intent(CLOID, "0")
            .to_line()
            .map(|l| serde_json::from_slice::<serde_json::Value>(&l).unwrap())
            .unwrap(),
    )
    .unwrap();
    fields["unexpected"] = 1.into();
    let mut bytes = journal(&[start(Some(1))]);
    bytes.extend(serde_json::to_vec(&fields).unwrap());
    bytes.push(b'\n');
    assert!(kinds(&audit(&bytes, None)).contains(&"malformed_line"));
}

#[test]
fn legacy_journals_are_labelled_and_never_given_assumed_times() {
    let legacy_terminal = JournalRecord::Terminal(TerminalRecord {
        cloid: CLOID.into(),
        filled: d("0.004"),
        average_price: None,
        position: d("-0.004"),
        terminal_source: None,
        terminal_at_ms: None,
        account_time_ms: None,
    });
    // Exactly what the pre-schema writer produced.
    let legacy = format!(
        "{}\n{}\n",
        json!({"event":"start","session_id":"s-1","network":"Testnet","lighter_account_index":7,
            "hyperliquid_account":ACCOUNT,"symbol":"ETH"}),
        json!({"event":"hedge_terminal","cloid":CLOID,"filled":"0.004","average_price":null,"position":"-0.004"}),
    );
    let mut bytes = legacy.lines().next().unwrap().as_bytes().to_vec();
    bytes.push(b'\n');
    bytes.extend(journal(&[intent(CLOID, "0")]));
    bytes.extend_from_slice(legacy.lines().nth(1).unwrap().as_bytes());
    bytes.push(b'\n');
    let report = audit(&bytes, None);
    assert_eq!(report.journal.schema, "legacy");
    assert_eq!(report.status, AuditStatus::JournalRecorded);
    assert_eq!(report.orders[0].journal, "recorded_legacy");
    assert!(kinds(&report).contains(&"legacy_journal"));
    // The same timestamp-less terminal in a v1 journal is invalid.
    let v1 = journal(&[start(Some(1)), intent(CLOID, "0"), legacy_terminal]);
    assert_eq!(audit(&v1, None).status, AuditStatus::Invalid);
    assert_eq!(
        audit(&journal(&[start(Some(9))]), None).status,
        AuditStatus::Invalid
    );
}

#[test]
fn file_audit_is_deterministic_and_leaves_inputs_unchanged() {
    let dir = std::env::temp_dir().join(format!("aleph-audit-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&dir).unwrap();
    let journal_path = dir.join("aleph-trade.active.jsonl");
    let evidence_path = dir.join("evidence.json");
    let bytes = journal(&[start(Some(1)), intent(CLOID, "0")]);
    std::fs::write(&journal_path, &bytes).unwrap();
    let raw_evidence = json!({"network": "Testnet", "hyperliquid_account": ACCOUNT, "coin": "ETH",
        "lookups": [{"cloid": CLOID, "observed_at_ms": 1_510,
            "response": lookup_response("canceled", "0.006", 1_500)}],
        "account_snapshot": {"observed_at_ms": 1_610, "state": snapshot("-0.004", 1_600).state}});
    let evidence_bytes = serde_json::to_vec_pretty(&raw_evidence).unwrap();
    std::fs::write(&evidence_path, &evidence_bytes).unwrap();
    let first = audit_files(&journal_path, Some(&evidence_path)).unwrap();
    let second = audit_files(&journal_path, Some(&evidence_path)).unwrap();
    assert_eq!(first, second);
    assert_eq!(
        serde_json::to_string(&first).unwrap(),
        serde_json::to_string(&second).unwrap()
    );
    assert_eq!(first.status, AuditStatus::EvidenceMatched);
    assert_eq!(
        first.journal.sha256.as_deref(),
        Some(sha256_hex(&bytes).as_str())
    );
    assert_eq!(std::fs::read(&journal_path).unwrap(), bytes);
    assert_eq!(std::fs::read(&evidence_path).unwrap(), evidence_bytes);
    // Unknown evidence fields are rejected rather than ignored.
    std::fs::write(
        &evidence_path,
        br#"{"network":"Testnet","hyperliquid_account":"x","coin":"ETH","extra":1}"#,
    )
    .unwrap();
    assert!(audit_files(&journal_path, Some(&evidence_path)).is_err());
    std::fs::remove_dir_all(dir).unwrap();
}

fn with_terminal(edit: impl FnOnce(&mut TerminalRecord)) -> JournalRecord {
    let JournalRecord::Terminal(mut t) = terminal(CLOID, "0.004", "-0.004") else {
        unreachable!()
    };
    edit(&mut t);
    JournalRecord::Terminal(t)
}

fn with_start(edit: impl FnOnce(&mut StartRecord)) -> JournalRecord {
    let JournalRecord::Start(mut s) = start(Some(1)) else {
        unreachable!()
    };
    edit(&mut s);
    JournalRecord::Start(s)
}

fn recorded(records: &[JournalRecord]) -> AuditStatus {
    audit(&journal(records), None).status
}

#[test]
fn v1_terminal_timing_follows_its_source_and_legacy_stays_labelled() {
    let ok = |t: JournalRecord| recorded(&[start(Some(1)), intent(CLOID, "0"), t]);
    // Control: lookup at intent time, account later.
    assert_eq!(ok(with_terminal(|_| {})), AuditStatus::JournalRecorded);
    // Lookup: live accepts a status up to LOOKUP_SKEW_MS before the intent.
    let late = |terminal_at: u64| {
        let JournalRecord::Intent(mut i) = intent(CLOID, "0") else {
            unreachable!()
        };
        i.submitted_at_ms = 5_000;
        recorded(&[
            start(Some(1)),
            JournalRecord::Intent(i),
            with_terminal(|t| {
                t.terminal_at_ms = Some(terminal_at);
                t.account_time_ms = Some(9_000);
            }),
        ])
    };
    assert_eq!(late(5_000 - LOOKUP_SKEW_MS), AuditStatus::JournalRecorded);
    assert_eq!(late(5_000 - LOOKUP_SKEW_MS - 1), AuditStatus::Invalid);
    // The reviewed counterexample: terminal at 1 ms for an intent at 5 s.
    assert_eq!(late(1), AuditStatus::Invalid);
    // Response: live stamps the local intent time, nothing else.
    let response = |at: u64| {
        ok(with_terminal(|t| {
            t.terminal_source = Some("response".into());
            t.terminal_at_ms = Some(at);
            t.account_time_ms = Some(1_600);
        }))
    };
    assert_eq!(response(1_000), AuditStatus::JournalRecorded);
    assert_eq!(response(999), AuditStatus::Invalid);
    assert_eq!(response(1_001), AuditStatus::Invalid);
    // Account confirmation at the terminal time is allowed; earlier is not.
    assert_eq!(
        ok(with_terminal(|t| t.account_time_ms = Some(1_500))),
        AuditStatus::JournalRecorded
    );
    assert_eq!(
        ok(with_terminal(|t| t.account_time_ms = Some(1_499))),
        AuditStatus::Invalid
    );
    // Each production-written v1 field is required.
    for edit in [
        (|t: &mut TerminalRecord| t.terminal_source = None) as fn(&mut TerminalRecord),
        |t| t.terminal_at_ms = None,
        |t| t.account_time_ms = None,
        |t| t.terminal_source = Some("venue".into()),
    ] {
        let report = audit(
            &journal(&[start(Some(1)), intent(CLOID, "0"), with_terminal(edit)]),
            None,
        );
        assert_eq!(report.status, AuditStatus::Invalid);
        assert_eq!(report.status.exit_code(), 3);
        assert!(kinds(&report).contains(&"invalid_terminal"));
    }
    // Legacy: no times is honest legacy; partial fields are still checked.
    let legacy = |t: JournalRecord| recorded(&[start(None), intent(CLOID, "0"), t]);
    let bare = |t: &mut TerminalRecord| {
        t.terminal_source = None;
        t.terminal_at_ms = None;
        t.account_time_ms = None;
    };
    assert_eq!(legacy(with_terminal(bare)), AuditStatus::JournalRecorded);
    assert_eq!(
        legacy(with_terminal(|t| {
            bare(t);
            t.account_time_ms = Some(5);
        })),
        AuditStatus::Invalid
    );
    // Fields a legacy record does carry must still be consistent.
    assert_eq!(
        legacy(with_terminal(|t| t.terminal_at_ms = Some(1_700))),
        AuditStatus::Invalid
    );
}

#[test]
fn session_identity_must_match_what_the_supervised_writer_emits() {
    let run =
        |s: JournalRecord| recorded(&[s, intent(CLOID, "0"), terminal(CLOID, "0.004", "-0.004")]);
    assert_eq!(run(with_start(|_| {})), AuditStatus::JournalRecorded);
    // Legacy session ids need not be UUIDs.
    assert_eq!(
        run(with_start(|s| s.session_id = "pre-uuid-session".into())),
        AuditStatus::JournalRecorded
    );
    assert_eq!(
        run(with_start(|s| s.lighter_account_index = 1)),
        AuditStatus::JournalRecorded
    );
    for (name, edit) in [
        (
            "empty session",
            (|s: &mut StartRecord| s.session_id = String::new()) as fn(&mut StartRecord),
        ),
        ("blank session", |s| s.session_id = "  ".into()),
        ("zero account", |s| s.lighter_account_index = 0),
        ("negative account", |s| s.lighter_account_index = -1),
        ("empty symbol", |s| s.symbol = String::new()),
        ("network", |s| s.network = "Devnet".into()),
        ("hl account", |s| s.hyperliquid_account = Some("0x1".into())),
        ("no hl account", |s| s.hyperliquid_account = None),
    ] {
        let report = audit(
            &journal(&[
                with_start(edit),
                intent(CLOID, "0"),
                terminal(CLOID, "0.004", "-0.004"),
            ]),
            None,
        );
        assert_eq!(report.status, AuditStatus::Invalid, "{name}");
    }
    // A session without IOCs needs no Hyperliquid account.
    assert_eq!(
        recorded(&[with_start(|s| s.hyperliquid_account = None)]),
        AuditStatus::JournalRecorded
    );
}

#[test]
fn unrepresentable_position_ranges_are_rejected_without_panicking() {
    let max = Decimal::MAX.to_string();
    let edit_intent = |cloid: &str, is_buy: bool, before: &str, size: &str| {
        let JournalRecord::Intent(mut i) = intent(cloid, before) else {
            unreachable!()
        };
        i.is_buy = is_buy;
        i.size = d(size);
        i.limit_price = if is_buy { d("2002") } else { d("1998") };
        JournalRecord::Intent(i)
    };
    let fill = |cloid: &str, filled: &str, position: &str| {
        JournalRecord::Terminal(TerminalRecord {
            average_price: Some(d("2000")),
            ..match terminal(cloid, filled, position) {
                JournalRecord::Terminal(t) => t,
                _ => unreachable!(),
            }
        })
    };
    let neg_max = format!("-{max}");
    // Recorded to ±MAX, then an unknown IOC that would push past it.
    for (is_buy, edge) in [(true, max.as_str()), (false, neg_max.as_str())] {
        let records = [
            start(Some(1)),
            edit_intent(CLOID, is_buy, "0", &max),
            fill(CLOID, &max, edge),
            edit_intent(OTHER, is_buy, edge, "1"),
        ];
        let report = audit(&journal(&records), None);
        assert_eq!(
            report.orders[0].status,
            OrderStatus::Recorded,
            "edge reachable"
        );
        assert_eq!(report.orders[0].possible_position_max, Some(d(edge)));
        let open = &report.orders[1];
        assert_eq!(open.status, OrderStatus::Invalid);
        assert_eq!(
            (open.possible_position_min, open.possible_position_max),
            (None, None)
        );
        assert!(open.reasons.iter().any(|r| r.contains("representable")));
        assert_eq!(report.status, AuditStatus::Invalid);
        assert_eq!(
            (
                report.hedge_position.possible_min,
                report.hedge_position.possible_max
            ),
            (None, None)
        );
        assert!(serde_json::to_string(&report).is_ok());
        // Same edge, reducing direction: representable and still pending.
        let reducing = [
            start(Some(1)),
            edit_intent(CLOID, is_buy, "0", &max),
            fill(CLOID, &max, edge),
            edit_intent(OTHER, !is_buy, edge, "1"),
        ];
        let report = audit(&journal(&reducing), None);
        assert_eq!(report.orders[1].status, OrderStatus::Pending);
        assert!(report.orders[1].possible_position_min.is_some());
    }
}

fn v2_start(market: Option<i64>) -> JournalRecord {
    let JournalRecord::Start(mut s) = start(Some(2)) else {
        unreachable!()
    };
    s.lighter_market_id = market;
    JournalRecord::Start(s)
}

fn observed_intent(observation: Option<PrimaryObservation>) -> JournalRecord {
    let JournalRecord::Intent(mut i) = intent(CLOID, "0") else {
        unreachable!()
    };
    i.primary_observation = observation;
    JournalRecord::Intent(i)
}

fn observation() -> PrimaryObservation {
    PrimaryObservation {
        account_index: 7,
        market_id: 4095,
        position: d("0.01"),
        transaction_time: None,
        request_us: 12_000,
        age_us: 15_000,
        max_age_us: 5_000_000,
    }
}

/// Schema 1 and legacy journals stay auditable with the primary observation
/// labelled unavailable; schema 2 requires the market on start and an
/// observation on every intent; schema 1 may not carry either.
#[test]
fn primary_observation_is_required_in_v2_and_unavailable_before() {
    for (schema, label) in [(Some(1), "v1"), (None, "legacy")] {
        let report = audit(&journal(&[start(schema), intent(CLOID, "0")]), None);
        assert_eq!(report.journal.schema, label);
        assert_eq!(report.status, AuditStatus::Unresolved, "{label}");
        let primary = &report.orders[0].primary;
        assert_eq!(
            (primary.status, primary.watermark),
            ("unavailable", "unknown")
        );
        assert!(
            primary.reasons[0].contains("not recorded"),
            "{:?}",
            primary.reasons
        );
    }
    let ok = audit(
        &journal(&[v2_start(Some(4095)), observed_intent(Some(observation()))]),
        None,
    );
    assert_eq!(ok.journal.schema, "v2");
    assert_eq!(ok.status, AuditStatus::Unresolved, "{:?}", ok.issues);
    assert_eq!(ok.orders[0].primary.status, "journal_claim");
    assert_eq!(ok.orders[0].primary.watermark, "unknown");
    let known = PrimaryObservation {
        transaction_time: Some(30),
        ..observation()
    };
    let report = audit(
        &journal(&[v2_start(Some(4095)), observed_intent(Some(known))]),
        None,
    );
    assert_eq!(report.orders[0].primary.watermark, "known");
    // Contract violations: each makes the journal invalid.
    let JournalRecord::Start(mut v1_with_market) = start(Some(1)) else {
        unreachable!()
    };
    v1_with_market.lighter_market_id = Some(4095);
    let cases: Vec<(&str, Vec<JournalRecord>)> = vec![
        ("v2 start without market", vec![v2_start(None)]),
        ("v2 negative market", vec![v2_start(Some(-1))]),
        (
            "v1 start with market",
            vec![JournalRecord::Start(v1_with_market)],
        ),
        (
            "v2 intent without observation",
            vec![v2_start(Some(4095)), observed_intent(None)],
        ),
        (
            "v1 intent with observation",
            vec![start(Some(1)), observed_intent(Some(observation()))],
        ),
        (
            "max age zero",
            vec![
                v2_start(Some(4095)),
                observed_intent(Some(PrimaryObservation {
                    max_age_us: 0,
                    age_us: 0,
                    request_us: 0,
                    ..observation()
                })),
            ],
        ),
    ];
    for (name, records) in cases {
        let report = audit(&journal(&records), None);
        assert_eq!(report.status, AuditStatus::Invalid, "{name}");
        assert!(!report.restart_permitted);
    }
    // An unsupported future schema is reported, not parsed as v2.
    assert_eq!(
        audit(&journal(&[start(Some(4))]), None).journal.schema,
        "unknown"
    );
}

/// A self-reported budget above the live 5 s limit cannot make an old
/// observation look fresh; small synthetic budgets stay valid.
#[test]
fn inflated_primary_budget_is_invalid() {
    let case = |age_us, max_age_us| {
        let observation = PrimaryObservation {
            age_us,
            max_age_us,
            ..observation()
        };
        audit(
            &journal(&[v2_start(Some(4095)), observed_intent(Some(observation))]),
            None,
        )
    };
    for (age, max) in [(6_000_000, 10_000_000), (15_000, MAX_PRIMARY_AGE_US + 1)] {
        let report = case(age, max);
        assert_eq!(report.status, AuditStatus::Invalid, "{age}/{max}");
        assert_eq!(report.orders[0].primary.status, "invalid");
        assert!(report.orders[0].primary.reasons[0].contains("live limit"));
    }
    for (age, max) in [(15_000, MAX_PRIMARY_AGE_US), (15_000, 200_000)] {
        let report = case(age, max);
        assert_eq!(report.status, AuditStatus::Unresolved, "{age}/{max}");
        assert_eq!(report.orders[0].primary.status, "journal_claim");
    }
    assert_eq!(
        case(MAX_PRIMARY_AGE_US, MAX_PRIMARY_AGE_US).status,
        AuditStatus::Invalid
    );
}

// ─── Venue-declared fees ─────────────────────────────────────────────────

fn fill(tid: u64, oid: u64, sz: &str, fee: &str, token: &str, time: u64) -> serde_json::Value {
    json!({"coin": "ETH", "px": "1999", "sz": sz, "side": "A", "time": time,
        "startPosition": "0", "dir": "Open Short", "closedPnl": "0", "hash": "0xabc",
        "oid": oid, "crossed": true, "fee": fee, "tid": tid, "feeToken": token})
}

fn fills(entries: Vec<serde_json::Value>) -> FillsEvidence {
    FillsEvidence {
        observed_at_ms: 2_000,
        response: serde_json::Value::Array(entries),
    }
}

/// A recorded IOC (filled 0.004) with an agreeing lookup binding oid 8.
fn fee_report(captures: Vec<FillsEvidence>) -> Report {
    let recorded = journal(&[
        start(Some(1)),
        intent(CLOID, "0"),
        terminal(CLOID, "0.004", "-0.004"),
    ]);
    let mut evidence = evidence(
        vec![lookup(lookup_response("canceled", "0.006", 1_500), 1_510)],
        None,
    );
    evidence.fills = captures;
    audit(&recorded, Some(&evidence))
}

/// Several partial fills of the bound oid that exactly cover the
/// reconciled quantity are summed per fee token, with rebates kept signed.
#[test]
fn bound_fills_covering_the_reconciled_fill_attribute_fees_per_token() {
    let report = fee_report(vec![fills(vec![
        fill(3, 8, "0.001", "0.0009", "USDC", 1_400),
        fill(1, 8, "0.002", "-0.0002", "USDC", 1_400),
        fill(2, 8, "0.001", "0.05", "HYPE", 1_450),
        fill(9, 99, "0.5", "1", "USDC", 1_450), // another order: ignored
    ])]);
    let fee = &report.orders[0].fee;
    assert_eq!(report.orders[0].status, OrderStatus::Recorded);
    assert_eq!(fee.status, "attributed", "{:?}", fee.reasons);
    assert_eq!(fee.source, "frozen_venue_fills");
    assert_eq!((fee.oid, fee.tids.clone()), (Some(8), vec![1, 2, 3]));
    assert_eq!(
        (fee.matched_size, fee.reconciled_size),
        (Some(d("0.004")), Some(d("0.004")))
    );
    let per: Vec<_> = fee.fees.iter().map(|f| (f.token.as_str(), f.fee)).collect();
    assert_eq!(per, [("HYPE", d("0.05")), ("USDC", d("0.0007"))]);
    assert_eq!(
        report.status,
        AuditStatus::JournalRecorded,
        "{:?}",
        report.issues
    );
    assert!(!report.restart_permitted);
    // A zero fee is a legal, attributed zero.
    let zero = fee_report(vec![fills(vec![fill(1, 8, "0.004", "0", "USDC", 1_400)])]);
    assert_eq!(zero.orders[0].fee.status, "attributed");
    assert_eq!(zero.orders[0].fee.fees[0].fee, Decimal::ZERO);
    // builderFee is already inside fee: reported, never added.
    let mut built = fill(1, 8, "0.004", "0.003", "USDC", 1_400);
    built["builderFee"] = json!("0.001");
    let builder = fee_report(vec![fills(vec![built])]);
    let asset = &builder.orders[0].fee.fees[0];
    assert_eq!(
        (asset.fee, asset.builder_fee),
        (d("0.003"), Some(d("0.001")))
    );
    // Overlapping captures repeating the same fill do not double it.
    let twice = fee_report(vec![
        fills(vec![fill(1, 8, "0.004", "0.002", "USDC", 1_400)]),
        fills(vec![fill(1, 8, "0.004", "0.002", "USDC", 1_400)]),
    ]);
    assert_eq!(twice.orders[0].fee.fees[0].fee, d("0.002"));
    assert_eq!(twice.orders[0].fee.matched_size, Some(d("0.004")));
}

/// Short, excess, foreign or contradictory fills are never shown as an
/// attributed fee, and each case is distinct and traceable.
#[test]
fn incomplete_or_contradictory_fills_are_not_attributed() {
    let status = |captures| {
        let report = fee_report(captures);
        let fee = report.orders[0].fee.clone();
        (fee.status, fee.reasons.join("; "), report.status)
    };
    let short = status(vec![fills(vec![fill(
        1, 8, "0.003", "0.001", "USDC", 1_400,
    )])]);
    assert_eq!(short.0, "unattributed");
    assert!(
        short.1.contains("cover 0.003 of the reconciled 0.004"),
        "{}",
        short.1
    );
    assert_eq!(
        short.2,
        AuditStatus::JournalRecorded,
        "missing fills are not a contradiction"
    );
    let missing = status(vec![fills(vec![fill(
        9, 99, "0.004", "0.001", "USDC", 1_400,
    )])]);
    assert_eq!(missing.0, "unattributed");
    let excess = status(vec![fills(vec![
        fill(1, 8, "0.004", "0.001", "USDC", 1_400),
        fill(2, 8, "0.001", "0.001", "USDC", 1_400),
    ])]);
    assert_eq!((excess.0, excess.2), ("invalid", AuditStatus::Invalid));
    assert!(excess.1.contains("more than the reconciled"));
    let mut buy = fill(1, 8, "0.004", "0.001", "USDC", 1_400);
    buy["side"] = json!("B");
    let wrong_side = status(vec![fills(vec![buy])]);
    assert_eq!(wrong_side.0, "invalid");
    assert!(wrong_side.1.contains("the IOC is A ETH"));
    // Within LOOKUP_SKEW_MS of the 1000 ms intent is tolerated; earlier is not.
    let skewed = status(vec![fills(vec![fill(1, 8, "0.004", "0.001", "USDC", 0)])]);
    assert_eq!(skewed.0, "attributed");
    let recorded = journal(&[
        start(Some(1)),
        intent(CLOID, "0"),
        terminal(CLOID, "0.004", "-0.004"),
    ]);
    let mut late_intent = recorded.clone();
    let text = String::from_utf8(late_intent)
        .unwrap()
        .replace("\"submitted_at_ms\":1000", "\"submitted_at_ms\":1400");
    late_intent = text.into_bytes();
    let mut early_evidence = evidence(
        vec![lookup(lookup_response("canceled", "0.006", 1_500), 1_510)],
        None,
    );
    early_evidence.fills = vec![fills(vec![fill(1, 8, "0.004", "0.001", "USDC", 399)])];
    let early = audit(&late_intent, Some(&early_evidence));
    assert_eq!(
        early.orders[0].fee.status, "invalid",
        "{:?}",
        early.orders[0].fee
    );
    assert!(early.orders[0].fee.reasons[0].contains("precedes the intent time"));
    let conflicting = status(vec![
        fills(vec![fill(1, 8, "0.004", "0.001", "USDC", 1_400)]),
        fills(vec![fill(1, 8, "0.004", "0.002", "USDC", 1_400)]),
    ]);
    assert_eq!(
        (conflicting.0, conflicting.2),
        ("invalid", AuditStatus::Invalid)
    );
    assert!(conflicting.1.contains("conflicting fields"));
    let after_capture = status(vec![fills(vec![fill(
        1, 8, "0.004", "0.001", "USDC", 2_500,
    )])]);
    assert_eq!(after_capture.0, "invalid");
    let malformed = status(vec![FillsEvidence {
        observed_at_ms: 2_000,
        response: json!({"error": "rate limited"}),
    }]);
    assert_eq!(malformed.0, "invalid");
    // Another coin on an unrelated oid is not this session's: ignored.
    let mut btc = fill(5, 77, "0.004", "0.001", "USDC", 1_400);
    btc["coin"] = json!("BTC");
    let unrelated = status(vec![fills(vec![
        btc,
        fill(1, 8, "0.004", "0.002", "USDC", 1_400),
    ])]);
    assert_eq!(unrelated.0, "attributed");
}

/// Without a reconciled fill, a bound oid, or a unique binding, fees stay
/// unknown; without fills evidence the order report is unchanged.
#[test]
fn fees_need_a_reconciled_fill_and_a_unique_oid_binding() {
    let bound = || vec![fills(vec![fill(1, 8, "0.004", "0.001", "USDC", 1_400)])];
    // Pending IOC: an unknown lookup, no terminal, so the fill is unknown.
    let crashed = journal(&[start(Some(1)), intent(CLOID, "0")]);
    let mut pending = evidence(vec![lookup(json!({"status": "unknownOid"}), 1_200)], None);
    pending.fills = bound();
    let report = audit(&crashed, Some(&pending));
    assert_eq!(report.orders[0].status, OrderStatus::Pending);
    assert_eq!(report.orders[0].fee.status, "unattributed");
    assert!(report.orders[0].fee.reasons[0].contains("binds this cloid to an oid"));
    // ACK-like open lookup: oid known, fill not reconciled.
    let mut open = evidence(
        vec![lookup(lookup_response("open", "0.01", 1_100), 1_150)],
        None,
    );
    open.fills = bound();
    let report = audit(&crashed, Some(&open));
    assert_eq!(report.orders[0].fee.oid, Some(8));
    assert!(report.orders[0].fee.reasons[0].contains("not reconciled"));
    // Recorded IOC but no lookup supplied: no oid binding.
    let recorded = journal(&[
        start(Some(1)),
        intent(CLOID, "0"),
        terminal(CLOID, "0.004", "-0.004"),
    ]);
    let mut no_lookup = evidence(Vec::new(), None);
    no_lookup.fills = bound();
    assert_eq!(
        audit(&recorded, Some(&no_lookup)).orders[0].fee.status,
        "unattributed"
    );
    // Lookups for one cloid naming two oids are a lookup conflict.
    let mut other = lookup_response("canceled", "0.006", 1_500);
    other["order"]["order"]["oid"] = json!(9);
    let mut split = evidence(
        vec![
            lookup(lookup_response("canceled", "0.006", 1_500), 1_510),
            lookup(other, 1_520),
        ],
        None,
    );
    split.fills = bound();
    let report = audit(&recorded, Some(&split));
    assert_eq!(report.orders[0].status, OrderStatus::Invalid);
    assert_eq!(report.orders[0].fee.status, "unattributed");
    // Old evidence without fills: fee not_provided, report otherwise equal.
    let plain = evidence(
        vec![lookup(lookup_response("canceled", "0.006", 1_500), 1_510)],
        None,
    );
    let without = audit(&recorded, Some(&plain));
    assert_eq!(without.orders[0].fee, FeeReport::not_provided());
    let with = fee_report(bound());
    assert_eq!(without.status, with.status);
    assert_eq!(without.orders[0].status, with.orders[0].status);
    assert_eq!(without.hedge_position, with.hedge_position);
    assert!(!with.restart_permitted && !without.restart_permitted);
    // Evidence for another account: fees invalid, nothing attributed.
    let mut foreign = evidence(
        vec![lookup(lookup_response("canceled", "0.006", 1_500), 1_510)],
        None,
    );
    foreign.hyperliquid_account = format!("0x{}", "bb".repeat(20));
    foreign.fills = bound();
    let report = audit(&recorded, Some(&foreign));
    assert_eq!(report.orders[0].fee.status, "invalid");
    assert!(report.orders[0].fee.fees.is_empty());
}

/// Two IOCs whose lookups name the same oid cannot share fills.
#[test]
fn one_oid_never_binds_two_iocs() {
    let recorded = journal(&[
        start(Some(1)),
        intent(CLOID, "0"),
        terminal(CLOID, "0.004", "-0.004"),
        intent(OTHER, "-0.004"),
        terminal(OTHER, "0.004", "-0.008"),
    ]);
    let mut second = lookup_response("canceled", "0.006", 1_500);
    second["order"]["order"]["cloid"] = json!(OTHER);
    let mut evidence = evidence(
        vec![
            lookup(lookup_response("canceled", "0.006", 1_500), 1_510),
            LookupEvidence {
                cloid: OTHER.into(),
                observed_at_ms: 1_520,
                response: second,
            },
        ],
        None,
    );
    evidence.fills = vec![fills(vec![fill(1, 8, "0.004", "0.001", "USDC", 1_400)])];
    let report = audit(&recorded, Some(&evidence));
    for order in &report.orders {
        assert_eq!(order.fee.status, "invalid", "{}", order.cloid);
        assert!(order.fee.fees.is_empty());
    }
    assert!(kinds(&report).contains(&"invalid_fee_evidence"));
}

/// Fills are bounded by the order's venue terminal time: the same instant
/// is attributed, later is invalid; a local-only terminal time (response
/// source, no lookup) bounds nothing and leaves the fee unattributed.
#[test]
fn fills_after_the_venue_terminal_time_are_rejected() {
    // fee_report: lookup terminal at 1500, journal terminal (lookup) 1500,
    // account time 1600.
    let at = |time| {
        let report = fee_report(vec![fills(vec![fill(
            1, 8, "0.004", "0.002", "USDC", time,
        )])]);
        (report.orders[0].fee.clone(), report.status)
    };
    assert_eq!(at(1_500).0.status, "attributed", "same venue instant");
    for time in [1_501, 1_650] {
        let (fee, status) = at(time);
        assert_eq!(fee.status, "invalid", "{time}");
        assert!(
            fee.reasons[0].contains("after the order's venue terminal time 1500"),
            "{:?}",
            fee.reasons
        );
        assert!(fee.fees.is_empty());
        assert_eq!(status, AuditStatus::Invalid);
    }
    // A partial fill in two parts, the last exactly at the terminal time.
    let split = fee_report(vec![fills(vec![
        fill(1, 8, "0.001", "0.0005", "USDC", 1_100),
        fill(2, 8, "0.003", "0.0015", "USDC", 1_500),
    ])]);
    assert_eq!(split.orders[0].fee.status, "attributed");
    assert_eq!(split.orders[0].fee.fees[0].fee, d("0.002"));
    // Without a venue terminal time (only local times known) the fee is
    // never attributed. Through `audit` an oid always comes from a located
    // lookup, so this unit-checks the attribution rule directly.
    let set = fees::load(
        &[fills(vec![fill(1, 8, "0.004", "0.002", "USDC", 1_400)])],
        "ETH",
    )
    .unwrap_or_else(|e| panic!("{e:?}"));
    let facts = |terminal_at_ms| fees::IocFacts {
        coin: "ETH",
        is_buy: false,
        submitted_at_ms: 1_000,
        oid: Some(8),
        oid_shared: false,
        filled: Some(d("0.004")),
        terminal_at_ms,
    };
    let unbounded = fees::attribute(&set, &facts(None), 1_000);
    assert_eq!(unbounded.status, "unattributed");
    assert!(unbounded.reasons[0].contains("no venue terminal time"));
    assert!(unbounded.fees.is_empty());
    assert_eq!(
        fees::attribute(&set, &facts(Some(1_400)), 1_000).status,
        "attributed"
    );
}

/// A bound oid claimed by another coin invalidates that IOC's fee, even
/// beside a fill that alone would cover it; other coins on other oids do
/// not touch it, and a tid reported for two coins is a contradiction.
#[test]
fn a_bound_oid_claimed_by_another_coin_is_invalid() {
    let btc = |tid, oid| {
        let mut entry = fill(tid, oid, "0.004", "0.001", "USDC", 1_400);
        entry["coin"] = json!("BTC");
        entry
    };
    let good = || fill(1, 8, "0.004", "0.002", "USDC", 1_400);
    for (name, entries) in [
        ("alone", vec![btc(7, 8)]),
        ("beside a covering fill", vec![btc(7, 8), good()]),
    ] {
        let report = fee_report(vec![fills(entries)]);
        let fee = &report.orders[0].fee;
        assert_eq!(fee.status, "invalid", "{name}");
        assert!(
            fee.reasons[0].contains("tid 7 is A BTC on oid 8"),
            "{name}: {:?}",
            fee.reasons
        );
        assert!(fee.fees.is_empty(), "{name}");
        assert_eq!(report.status, AuditStatus::Invalid, "{name}");
        assert!(kinds(&report).contains(&"invalid_fee_evidence"));
        // The order's own reconciliation is untouched.
        assert_eq!(report.orders[0].status, OrderStatus::Recorded);
    }
    assert_eq!(
        fee_report(vec![fills(vec![btc(7, 77), good()])]).orders[0]
            .fee
            .status,
        "attributed"
    );
    let two_coins = fee_report(vec![fills(vec![btc(1, 77), good()])]);
    assert_eq!(two_coins.orders[0].fee.status, "invalid");
    assert!(two_coins.orders[0].fee.reasons[0].contains("tid 1 is reported for two coins"));
    // A conflicting repeat of another coin's tid is still a contradiction.
    let mut changed = btc(7, 77);
    changed["fee"] = json!("0.009");
    let conflict = fee_report(vec![fills(vec![btc(7, 77), good()]), fills(vec![changed])]);
    assert_eq!(conflict.orders[0].fee.status, "invalid");
}

/// Amounts that `Decimal` would round are rejected with their location;
/// exact values (trailing zeros, zero, negative rebate, 28 digits) pass.
#[test]
fn fill_amounts_must_be_exactly_representable() {
    let long = "0.00400000000000000000000000001"; // 29 fractional digits
    for (field, value) in [
        ("sz", long),
        ("fee", "0.00200000000000000000000000001"),
        ("builderFee", long),
    ] {
        let mut entry = fill(1, 8, "0.004", "0.002", "USDC", 1_400);
        entry[field] = json!(value);
        let report = fee_report(vec![fills(vec![entry])]);
        let fee = &report.orders[0].fee;
        assert_eq!(fee.status, "invalid", "{field}");
        let reason = fee.reasons.join("; ");
        assert!(
            reason.contains("fills[0][0] (tid 1)") && reason.contains("not exactly representable"),
            "{field}: {reason}"
        );
        assert!(kinds(&report).contains(&"invalid_fills"));
        assert_eq!(report.status, AuditStatus::Invalid);
    }
    for (sz, fee) in [
        ("0.0040", "0.00200"),
        ("0.004", "0"),
        ("0.004", "-0.0002"),
        ("0.004", "0.0000000000000000000000000001"),
    ] {
        let report = fee_report(vec![fills(vec![fill(1, 8, sz, fee, "USDC", 1_400)])]);
        assert_eq!(report.orders[0].fee.status, "attributed", "{sz} {fee}");
        assert_eq!(report.orders[0].fee.fees[0].fee, d(fee));
    }
    let mut not_number = fill(1, 8, "0.004", "0.002", "USDC", 1_400);
    not_number["fee"] = json!("1e-3");
    assert_eq!(
        fee_report(vec![fills(vec![not_number])]).orders[0]
            .fee
            .status,
        "invalid"
    );
}
