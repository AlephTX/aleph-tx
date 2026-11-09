//! Session coverage: the audit's order set is the universe, each IOC is
//! judged as the single view judges it, and nothing raises the ratio that
//! is not a bound positive fill.

use super::*;
use crate::live_hedge::fee_view::coverage::{self, Class, CoverageReport, CoverageStatus};

/// The second IOC: same session, sells 0.01 after the first left -0.004.
const SECOND: &str = "0x1123456789abcdef0123456789abcdef";

fn second_intent() -> IntentRecord {
    IntentRecord {
        submitted_at_ms: 2_000,
        ..intent(SECOND, "-0.004")
    }
}

/// Fills 0.004 more, to -0.008, per its own order response.
fn second_terminal() -> TerminalRecord {
    TerminalRecord {
        cloid: SECOND.into(),
        filled: d("0.004"),
        average_price: Some(d("1999")),
        position: d("-0.008"),
        terminal_source: Some("response".into()),
        terminal_at_ms: Some(2_000),
        account_time_ms: Some(2_600),
    }
}

/// Two reconciled positive IOCs, in journal order.
fn two_filled() -> Vec<u8> {
    journal(vec![
        JournalRecord::Start(start()),
        JournalRecord::Intent(intent(CLOID, "0")),
        JournalRecord::Terminal(terminal(CLOID, Some("1999"))),
        JournalRecord::Intent(second_intent()),
        JournalRecord::Terminal(second_terminal()),
    ])
}

/// The second IOC's telemetry record, as the writer exports it.
fn second_record() -> crate::telemetry_sink::HedgeFillRecord {
    crate::telemetry_sink::HedgeFillRecord {
        client_order_id: SECOND.into(),
        position_after: -0.008,
        ..record(1999.0, "fill")
    }
}

/// Evidence for both IOCs: each one's terminal lookup (oids 8 and 9) and
/// fills that cover exactly its 0.004.
fn both_paid() -> Evidence {
    let mut both = paid();
    both.lookups.push(audit::LookupEvidence {
        cloid: SECOND.into(),
        observed_at_ms: 2_510,
        response: json!({"status": "order", "order": {"status": "canceled", "statusTimestamp": 2_000,
            "order": {"coin": "ETH", "cloid": SECOND, "side": "A", "limitPx": "1998",
                "origSz": "0.01", "sz": "0.006", "oid": 9, "timestamp": 2_000}}}),
    });
    let mut second_fill = venue_fill(2, "0.004", "0.0035982", "USDC");
    second_fill["oid"] = json!(9);
    second_fill["time"] = json!(2_000);
    both.fills[0]
        .response
        .as_array_mut()
        .unwrap()
        .push(second_fill);
    both
}

fn row<'r>(report: &'r CoverageReport, cloid: &str) -> &'r coverage::OrderCoverage {
    report.orders.iter().find(|o| o.cloid == cloid).unwrap()
}

/// The coverage row of each IOC says what the single view says.
fn assert_rows_match_views(journal: &[u8], evidence: &Evidence, telemetry: &[u8]) {
    let report = coverage::build(journal, evidence, telemetry);
    for order in &report.orders {
        let view = build(journal, evidence, telemetry, &order.cloid);
        assert_eq!(order.view_status, view.status, "{}", order.cloid);
        // Own reasons plus the archive's, in the view's order, are the
        // view's reasons exactly.
        let mut expected = view.reasons.clone();
        expected.retain(|r| !report.archive.reasons.contains(r));
        assert_eq!(order.reasons, expected, "{}", order.cloid);
        assert!(
            report
                .archive
                .reasons
                .iter()
                .all(|r| view.reasons.contains(r)),
            "{}",
            order.cloid
        );
        assert_eq!(order.notes, view.telemetry.notes, "{}", order.cloid);
        assert_eq!(Some(&order.venue_fee), view.venue_fee.as_ref());
        // A row binds exactly when its view would, once the archive does.
        if report.archive.status == ViewStatus::Bound {
            assert_eq!(
                order.class == Class::Bound,
                view.status == ViewStatus::Bound,
                "{}",
                order.cloid
            );
        }
        assert!(order.class != Class::Bound || order.in_denominator);
    }
    // The counts and the denominator are the rows, nothing else.
    let n = &report.counts;
    let class = |c: Class| report.orders.iter().filter(|o| o.class == c).count();
    assert_eq!(n.intents, report.orders.len());
    assert_eq!(
        (
            n.bound,
            n.missing_evidence,
            n.evidence_only,
            n.unknown_outcome,
            n.no_fill,
            n.contradiction
        ),
        (
            class(Class::Bound),
            class(Class::MissingEvidence),
            class(Class::EvidenceOnly),
            class(Class::UnknownOutcome),
            class(Class::NoFill),
            class(Class::Contradiction)
        )
    );
    // The denominator is the positive journaled fills; evidence-only
    // positive fills are counted apart, never in it.
    let positive = |o: &&coverage::OrderCoverage| o.filled.is_some_and(|f| f > Decimal::ZERO);
    let cohort = |status: OrderStatus| {
        report
            .orders
            .iter()
            .filter(positive)
            .filter(|o| o.audit_status == status)
            .count()
    };
    let c = &report.coverage;
    assert_eq!(
        c.denominator,
        report.orders.iter().filter(|o| o.in_denominator).count()
    );
    assert_eq!(c.denominator, cohort(OrderStatus::Recorded));
    assert_eq!(c.evidence_only, cohort(OrderStatus::Matched));
    for order in &report.orders {
        assert_eq!(
            order.in_denominator,
            order.audit_status == OrderStatus::Recorded && positive(&order),
            "{}",
            order.cloid
        );
    }
    assert!(c.evidence_only == 0 || c.ratio.is_none());
    assert_eq!(c.bound, n.bound);
    assert_eq!(c.ratio.is_none(), !c.withheld.is_empty());
    assert_eq!(n.orphan_telemetry, report.orphan_telemetry.len());
}

/// The acceptance case: two positive fills, the second with neither cost
/// record, is 1 of 2 bound, with the gap named on the second row.
#[tokio::test]
async fn one_of_two_positive_fills_is_bound_and_the_other_names_its_gap() {
    let telemetry = written(info(), vec![record(1999.0, "fill")]).await;
    let journal = two_filled();
    let report = coverage::build(&journal, &paid(), &telemetry);
    assert_eq!(report.status, CoverageStatus::Incomplete, "{report:#?}");
    assert_eq!(report.status.exit_code(), 2);
    assert_eq!(
        report.archive.status,
        ViewStatus::Bound,
        "{:?}",
        report.archive.reasons
    );
    let c = &report.coverage;
    assert_eq!((c.denominator, c.evidence_only, c.bound), (2, 0, 1));
    assert_eq!(c.ratio, Some(0.5));
    assert!(c.withheld.is_empty(), "{:?}", c.withheld);
    let n = &report.counts;
    assert_eq!((n.intents, n.bound, n.missing_evidence), (2, 1, 1));
    assert_eq!((n.missing_telemetry, n.missing_venue_fee), (1, 1));
    assert_eq!(
        report
            .orders
            .iter()
            .map(|o| o.cloid.as_str())
            .collect::<Vec<_>>(),
        [CLOID, SECOND]
    );
    let first = row(&report, CLOID);
    assert_eq!(
        (first.class, first.view_status),
        (Class::Bound, ViewStatus::Bound)
    );
    assert_eq!(first.venue_fee.status, "attributed");
    assert!(first.estimate.fee.is_some());
    let second = row(&report, SECOND);
    assert_eq!(second.class, Class::MissingEvidence);
    assert_eq!(second.view_status, ViewStatus::Unresolved);
    assert!(second.in_denominator);
    assert!(
        second
            .reasons
            .iter()
            .any(|r| r.contains("no hedge_fill for this CLOID"))
    );
    assert!(
        second
            .reasons
            .iter()
            .any(|r| r.contains("venue fee is unknown"))
    );
    // A missing fee stays unknown: no amount is invented for it.
    assert_eq!(second.estimate.fee, None);
    assert!(second.venue_fee.fees.is_empty());
    assert_rows_match_views(&journal, &paid(), &telemetry);
    // Deterministic for the same inputs.
    assert_eq!(report, coverage::build(&journal, &paid(), &telemetry));
}

/// Both cost records for both IOCs: complete, still only for these files.
#[tokio::test]
async fn every_positive_fill_bound_is_complete_for_these_files_only() {
    let telemetry = written(info(), vec![record(1999.0, "fill"), second_record()]).await;
    let both = both_paid();
    let journal = two_filled();
    let report = coverage::build(&journal, &both, &telemetry);
    assert_eq!(report.status, CoverageStatus::Complete, "{report:#?}");
    assert_eq!(report.status.exit_code(), 0);
    assert_eq!(report.coverage.ratio, Some(1.0));
    assert_eq!(report.counts.bound, 2);
    // Never summed: each IOC keeps its own fee per asset.
    assert!(report.orders.iter().all(|o| o.venue_fee.fees.len() == 1));
    // Whole files are not a closed session.
    assert!(
        report
            .archive
            .notes
            .iter()
            .any(|n| n.contains("verified_flat"))
    );
    assert!(
        report
            .boundaries
            .iter()
            .any(|b| b.contains("supplied files only"))
    );
    assert_rows_match_views(&journal, &both, &telemetry);
}

/// An unknown outcome, a zero fill and an empty denominator never raise
/// the ratio; each withholds it with its reason.
#[tokio::test]
async fn unknown_outcomes_zero_fills_and_an_empty_denominator_withhold_the_ratio() {
    // One bound fill and one open IOC: 1 of 1 known, but the open one may
    // have filled, so there is no ratio.
    let telemetry = written(info(), vec![record(1999.0, "fill")]).await;
    let open = journal(vec![
        JournalRecord::Start(start()),
        JournalRecord::Intent(intent(CLOID, "0")),
        JournalRecord::Terminal(terminal(CLOID, Some("1999"))),
        JournalRecord::Intent(second_intent()),
    ]);
    let report = coverage::build(&open, &paid(), &telemetry);
    assert_eq!(report.status, CoverageStatus::Incomplete);
    let c = &report.coverage;
    assert_eq!((c.denominator, c.bound, c.ratio), (1, 1, None));
    assert!(
        c.withheld.iter().any(|w| w.contains("not reconciled")),
        "{:?}",
        c.withheld
    );
    let second = row(&report, SECOND);
    assert_eq!(second.class, Class::UnknownOutcome);
    assert!(!second.in_denominator);
    assert_rows_match_views(&open, &paid(), &telemetry);

    // Only an open IOC: nothing known to cover.
    let empty = written(info(), vec![]).await;
    let report = coverage::build(&journal_with_open_intent(), &paid(), &empty);
    assert_eq!(report.status, CoverageStatus::Incomplete);
    assert_eq!(
        (report.coverage.denominator, report.coverage.ratio),
        (0, None)
    );
    assert_eq!(report.counts.unknown_outcome, 1);
    assert!(
        report
            .coverage
            .withheld
            .iter()
            .any(|w| w.contains("the denominator is empty"))
    );

    // A zero fill owes no fee and is not in the denominator.
    let mut zero = terminal(CLOID, None);
    zero.filled = Decimal::ZERO;
    zero.position = Decimal::ZERO;
    let unfilled = journal(vec![
        JournalRecord::Start(start()),
        JournalRecord::Intent(intent(CLOID, "0")),
        JournalRecord::Terminal(zero),
    ]);
    let no_lookup = Evidence {
        lookups: Vec::new(),
        ..evidence(vec![])
    };
    let report = coverage::build(&unfilled, &no_lookup, &empty);
    assert_eq!(report.counts.no_fill, 1);
    assert_eq!(row(&report, CLOID).class, Class::NoFill);
    assert!(!row(&report, CLOID).in_denominator);
    assert_eq!(
        (report.coverage.denominator, report.coverage.ratio),
        (0, None)
    );
    assert_eq!(report.status, CoverageStatus::Incomplete);
    assert_rows_match_views(&unfilled, &no_lookup, &empty);
}

/// Repeats count once. A conflicting repeat, or telemetry naming no
/// journaled IOC, makes the files contradictory.
#[tokio::test]
async fn repeats_count_once_and_orphan_or_conflicting_records_are_invalid() {
    let base = written(info(), vec![record(1999.0, "fill")]).await;
    let text = String::from_utf8(base).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    let (session, fill, stopped) = (lines[0], lines[1], lines[2]);
    let file = |lines: &[&str]| {
        lines
            .iter()
            .map(|l| format!("{l}\n"))
            .collect::<String>()
            .into_bytes()
    };
    // An exact telemetry repeat and a repeated journal terminal: one IOC,
    // bound once.
    let repeated = journal(vec![
        JournalRecord::Start(start()),
        JournalRecord::Intent(intent(CLOID, "0")),
        JournalRecord::Terminal(terminal(CLOID, Some("1999"))),
        JournalRecord::Terminal(terminal(CLOID, Some("1999"))),
    ]);
    let telemetry = file(&[session, fill, fill, stopped]);
    let report = coverage::build(&repeated, &paid(), &telemetry);
    assert_eq!(report.status, CoverageStatus::Complete, "{report:#?}");
    let c = &report.coverage;
    assert_eq!((report.counts.intents, c.denominator, c.bound), (1, 1, 1));
    assert_eq!(c.ratio, Some(1.0));
    let one = row(&report, CLOID);
    assert_eq!(one.terminal_lines, [3, 4]);
    assert_eq!(one.estimate.lines, [2, 3]);
    assert!(one.notes[0].contains("repeats line 2"), "{:?}", one.notes);
    assert_rows_match_views(&repeated, &paid(), &telemetry);

    // A second, different record for the same IOC is a contradiction.
    let mut value: serde_json::Value = serde_json::from_str(fill).unwrap();
    value["fee"] = json!(0.0001);
    let second_write = value.to_string();
    let conflicting = file(&[session, fill, &second_write, stopped]);
    let report = coverage::build(&reconciled(Some("1999")), &paid(), &conflicting);
    assert_eq!(report.status, CoverageStatus::Invalid);
    assert_eq!(report.status.exit_code(), 3);
    assert_eq!(row(&report, CLOID).class, Class::Contradiction);
    assert_eq!((report.coverage.bound, report.coverage.ratio), (0, None));

    // A record for an IOC the journal never had. The single view of CLOID
    // still binds, but the files do not belong together.
    let mut stray = record(1999.0, "fill");
    stray.client_order_id = OTHER.into();
    let orphaned = written(info(), vec![record(1999.0, "fill"), stray]).await;
    let one = reconciled(Some("1999"));
    assert_eq!(
        build(&one, &paid(), &orphaned, CLOID).status,
        ViewStatus::Bound
    );
    let report = coverage::build(&one, &paid(), &orphaned);
    assert_eq!(report.status, CoverageStatus::Invalid);
    assert_eq!(report.counts.orphan_telemetry, 1);
    assert_eq!(
        (
            report.orphan_telemetry[0].cloid.as_str(),
            report.orphan_telemetry[0].lines.as_slice()
        ),
        (OTHER, [3].as_slice())
    );
    assert_eq!((report.coverage.bound, report.coverage.ratio), (1, None));
    assert!(
        report
            .coverage
            .withheld
            .iter()
            .any(|w| w.contains("name no journaled IOC"))
    );
}

/// A torn file, another session or foreign evidence holds every row back.
/// The rows stay as diagnostics; the ratio is withheld.
#[tokio::test]
async fn truncation_or_a_foreign_session_withholds_the_ratio() {
    let paid = paid();
    let telemetry = written(info(), vec![record(1999.0, "fill"), second_record()]).await;
    let journal = two_filled();
    let prefix = format!(r#"{{"event":"hedge_terminal","cloid":"{SECOND}","fil"#);
    let torn_journal = [journal.as_slice(), prefix.as_bytes()].concat();
    let mut torn_telemetry = telemetry.clone();
    torn_telemetry.extend_from_slice(br#"{"type":"hedge_fill","ts_ms":5,"client"#);
    let mut foreign = info();
    foreign.session_id = "other-session".into();
    let foreign_telemetry = written(foreign, vec![record(1999.0, "fill"), second_record()]).await;
    let mut foreign_evidence = paid.clone();
    foreign_evidence.hyperliquid_account = format!("0x{:040x}", 0xbb);
    /// Name, journal, evidence, telemetry, expected status and reason.
    type Case<'c> = (
        &'c str,
        &'c [u8],
        &'c Evidence,
        &'c [u8],
        CoverageStatus,
        &'c str,
    );
    let cases: [Case<'_>; 4] = [
        (
            "torn journal",
            &torn_journal,
            &paid,
            &telemetry,
            CoverageStatus::Incomplete,
            "journal is truncated on line 6",
        ),
        (
            "torn telemetry",
            &journal,
            &paid,
            &torn_telemetry,
            CoverageStatus::Incomplete,
            "line 5 lacks its newline",
        ),
        (
            "foreign session",
            &journal,
            &paid,
            &foreign_telemetry,
            CoverageStatus::Invalid,
            "session_id",
        ),
        (
            "foreign evidence",
            &journal,
            &foreign_evidence,
            &telemetry,
            CoverageStatus::Invalid,
            "another network, account or coin",
        ),
    ];
    for (name, journal, evidence, telemetry, status, reason) in cases {
        let report = coverage::build(journal, evidence, telemetry);
        assert_eq!(report.status, status, "{name}");
        assert_eq!(report.coverage.ratio, None, "{name}");
        assert!(
            report.coverage.withheld[0].starts_with("the archive is"),
            "{name}: {:?}",
            report.coverage.withheld
        );
        assert!(
            report.archive.reasons.iter().any(|r| r.contains(reason)),
            "{name}: {:?}",
            report.archive.reasons
        );
        // Every IOC is still listed, held back by the archive.
        assert_eq!(report.orders.len(), 2, "{name}");
        assert!(
            report
                .orders
                .iter()
                .all(|o| o.view_status >= ViewStatus::Unresolved),
            "{name}"
        );
        assert_rows_match_views(journal, evidence, telemetry);
    }
    // The complete prefix keeps its diagnostic: the first IOC's own
    // records bind, and the ratio is still withheld.
    let report = coverage::build(&torn_journal, &paid, &telemetry);
    let first = row(&report, CLOID);
    assert_eq!(
        (first.class, first.view_status),
        (Class::Bound, ViewStatus::Unresolved)
    );
}

/// An account sample at `time` with the hedge at `position`.
fn snapshot(time: u64, position: &str) -> audit::SnapshotEvidence {
    audit::SnapshotEvidence {
        observed_at_ms: time + 50,
        state: json!({"time": time, "withdrawable": "100",
            "marginSummary": {"accountValue": "100", "totalNtlPos": "0", "totalMarginUsed": "0"},
            "assetPositions": [{"position": {"coin": "ETH", "szi": position, "unrealizedPnl": "0"}}]}),
    }
}

/// An IOC reconciled only by venue evidence has filled, but the journal has
/// no terminal for it, so the writer exported no estimate. It is listed
/// apart with its venue fee and reasons, outside the denominator, and it
/// withholds the ratio. It is never called an unknown outcome.
#[tokio::test]
async fn an_evidence_only_fill_is_listed_apart_and_withholds_the_ratio() {
    let empty = written(info(), vec![]).await;
    let matched = Evidence {
        account_snapshot: Some(snapshot(1_650, "-0.004")),
        ..paid()
    };
    let open = journal_with_open_intent();
    let report = coverage::build(&open, &matched, &empty);
    let one = row(&report, CLOID);
    assert_eq!(one.audit_status, OrderStatus::Matched, "{:?}", one.reasons);
    assert_eq!(one.class, Class::EvidenceOnly);
    assert!(!one.in_denominator);
    assert_eq!(one.filled, Some(d("0.004")));
    assert_eq!(one.venue_fee.status, "attributed");
    assert!(
        one.reasons
            .iter()
            .any(|r| r.contains("reconciled by evidence only")),
        "{:?}",
        one.reasons
    );
    let c = &report.coverage;
    assert_eq!((c.denominator, c.evidence_only, c.bound), (0, 1, 0));
    assert_eq!(c.ratio, None);
    for expected in ["no journaled terminal", "the denominator is empty"] {
        assert!(
            c.withheld.iter().any(|w| w.contains(expected)),
            "{expected}: {:?}",
            c.withheld
        );
    }
    assert!(!c.withheld.iter().any(|w| w.contains("not reconciled")));
    let n = &report.counts;
    assert_eq!(
        (n.evidence_only, n.unknown_outcome, n.missing_evidence),
        (1, 0, 0)
    );
    assert_eq!(report.status, CoverageStatus::Incomplete);
    assert_eq!(report.status.exit_code(), 2);
    assert_rows_match_views(&open, &matched, &empty);
}

/// The review's case: a bound journaled fill and an evidence-only fill are
/// 1 of 1 journaled with 1 apart, and no ratio. The evidence-only row keeps
/// its venue fee and reasons.
#[tokio::test]
async fn a_bound_journaled_fill_beside_an_evidence_only_fill_has_no_ratio() {
    let telemetry = written(info(), vec![record(1999.0, "fill")]).await;
    let journal = journal(vec![
        JournalRecord::Start(start()),
        JournalRecord::Intent(intent(CLOID, "0")),
        JournalRecord::Terminal(terminal(CLOID, Some("1999"))),
        JournalRecord::Intent(second_intent()),
    ]);
    let evidence = Evidence {
        account_snapshot: Some(snapshot(2_650, "-0.008")),
        ..both_paid()
    };
    let report = coverage::build(&journal, &evidence, &telemetry);
    let second = row(&report, SECOND);
    assert_eq!(
        second.audit_status,
        OrderStatus::Matched,
        "{:?}",
        second.reasons
    );
    assert_eq!(second.class, Class::EvidenceOnly);
    assert!(!second.in_denominator);
    assert_eq!(second.view_status, ViewStatus::Unresolved);
    assert_eq!(second.venue_fee.status, "attributed");
    assert_eq!(second.venue_fee.fees.len(), 1);
    assert!(second.estimate.fee.is_none());
    let first = row(&report, CLOID);
    assert_eq!((first.class, first.in_denominator), (Class::Bound, true));
    let c = &report.coverage;
    assert_eq!((c.denominator, c.evidence_only, c.bound), (1, 1, 1));
    assert_eq!(c.ratio, None);
    assert_eq!(c.withheld.len(), 1, "{:?}", c.withheld);
    assert!(c.withheld[0].contains("no journaled terminal"));
    assert_eq!(report.status, CoverageStatus::Incomplete);
    assert_rows_match_views(&journal, &evidence, &telemetry);

    // A zero fill the venue confirms is a known zero fill, not an
    // evidence-only one: it owes no fee and does not hold the ratio back.
    let canceled = json!({"status": "order", "order": {"status": "canceled", "statusTimestamp": 2_000,
        "order": {"coin": "ETH", "cloid": SECOND, "side": "A", "limitPx": "1998",
            "origSz": "0.01", "sz": "0.01", "oid": 9, "timestamp": 2_000}}});
    let mut unfilled = paid();
    unfilled.lookups.push(audit::LookupEvidence {
        cloid: SECOND.into(),
        observed_at_ms: 2_510,
        response: canceled,
    });
    unfilled.account_snapshot = Some(snapshot(2_650, "-0.004"));
    let report = coverage::build(&journal, &unfilled, &telemetry);
    let second = row(&report, SECOND);
    assert_eq!(
        (second.audit_status, second.filled),
        (OrderStatus::Matched, Some(Decimal::ZERO)),
        "{:?}",
        second.reasons
    );
    assert_eq!(second.class, Class::NoFill);
    assert!(!second.in_denominator);
    let c = &report.coverage;
    assert_eq!((c.denominator, c.evidence_only, c.bound), (1, 0, 1));
    assert_eq!(c.ratio, Some(1.0), "{:?}", c.withheld);
    assert_eq!(report.status, CoverageStatus::Complete);
    assert_rows_match_views(&journal, &unfilled, &telemetry);
}

/// The file entry point reads each input once, reports its hash, leaves it
/// unchanged and gives the same report twice. Each row agrees with the
/// single view over the same files. The committed single-IOC example is
/// complete, 1 of 1.
#[tokio::test]
async fn coverage_files_hash_inputs_and_leave_them_unchanged() {
    let dir = std::env::temp_dir().join(format!("aleph-fee-coverage-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&dir).unwrap();
    let paths = (
        dir.join("journal.jsonl"),
        dir.join("evidence.json"),
        dir.join("telemetry.jsonl"),
    );
    let journal_bytes = two_filled();
    let telemetry_bytes = written(info(), vec![record(1999.0, "fill")]).await;
    let evidence_bytes = serde_json::to_vec_pretty(&json!({
        "network": "Testnet", "hyperliquid_account": ACCOUNT, "coin": "ETH",
        "lookups": [{"cloid": CLOID, "observed_at_ms": 1_510, "response": lookup().response}],
        "fills": [{"observed_at_ms": 2_000,
            "response": [venue_fill(1, "0.004", "0.0035982", "USDC")]}]}))
    .unwrap();
    std::fs::write(&paths.0, &journal_bytes).unwrap();
    std::fs::write(&paths.1, &evidence_bytes).unwrap();
    std::fs::write(&paths.2, &telemetry_bytes).unwrap();
    let first = coverage::coverage_files(&paths.0, &paths.1, &paths.2).unwrap();
    let second = coverage::coverage_files(&paths.0, &paths.1, &paths.2).unwrap();
    assert_eq!(first, second);
    assert_eq!(first.coverage.ratio, Some(0.5), "{first:#?}");
    let inputs = first.inputs.as_ref().unwrap();
    for (digest, bytes) in [
        (&inputs.journal, &journal_bytes),
        (&inputs.evidence, &evidence_bytes),
        (&inputs.telemetry, &telemetry_bytes),
    ] {
        assert_eq!(digest.sha256, hex::encode(Sha256::digest(bytes)));
        assert_eq!(digest.bytes, bytes.len());
    }
    assert_eq!(std::fs::read(&paths.0).unwrap(), journal_bytes);
    assert_eq!(std::fs::read(&paths.1).unwrap(), evidence_bytes);
    assert_eq!(std::fs::read(&paths.2).unwrap(), telemetry_bytes);
    for order in &first.orders {
        let view = view_files(&paths.0, &paths.1, &paths.2, &order.cloid).unwrap();
        assert_eq!(order.view_status, view.status, "{}", order.cloid);
        assert_eq!(view.inputs, first.inputs);
    }
    // The audit of the same files is unchanged by the report.
    let audit_report = audit::audit_files(&paths.0, Some(&paths.1)).unwrap();
    assert_eq!(audit_report.status, first.audit_status);
    std::fs::remove_dir_all(dir).unwrap();

    let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/hedge_fee_view");
    let report = coverage::coverage_files(
        &example.join("journal.jsonl"),
        &example.join("evidence.json"),
        &example.join("telemetry.jsonl"),
    )
    .unwrap();
    assert_eq!(report.status, CoverageStatus::Complete, "{report:#?}");
    assert_eq!(report.coverage.ratio, Some(1.0));
}

/// Each view lists archive-wide and own reasons in the order the single
/// view always has: the journal, then telemetry line faults in file order
/// (its own malformed records among them), then the session, then its own
/// record and the venue fee. Another IOC's malformed record is not listed.
#[tokio::test]
async fn single_views_keep_their_reason_order_across_archive_and_own_findings() {
    let mut foreign = info();
    foreign.session_id = "other-session".into();
    let base = String::from_utf8(written(foreign, vec![record(1999.0, "fill")]).await).unwrap();
    let lines: Vec<&str> = base.lines().collect();
    let mut bad: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    bad["client_order_id"] = json!(SECOND);
    bad["position_after"] = json!(-0.008);
    bad["fee"] = json!(false);
    let telemetry = [
        lines[0],
        r#"{"type":"hedge_fill","ts_ms":1}"#,
        lines[1],
        &bad.to_string(),
        r#"{"x":1}"#,
        lines[2],
    ]
    .iter()
    .map(|l| format!("{l}\n"))
    .collect::<String>()
    .into_bytes();
    let torn = [two_filled().as_slice(), b"{\"ev"].concat();
    let starts = |reasons: &[String], prefixes: &[&str]| {
        assert_eq!(reasons.len(), prefixes.len(), "{reasons:#?}");
        for (reason, prefix) in reasons.iter().zip(prefixes) {
            assert!(reason.starts_with(prefix), "{reason} / {prefix}");
        }
    };
    let second = build(&torn, &paid(), &telemetry, SECOND);
    starts(
        &second.reasons,
        &[
            "the journal is truncated on line 6",
            "telemetry: line 2: malformed hedge_fill",
            "telemetry: line 4: fee `false` is not a finite number",
            "telemetry: line 5: no record type",
            "telemetry: telemetry session_id `other-session`",
            "telemetry: no hedge_fill for this CLOID",
            "venue fills do not cover this IOC",
        ],
    );
    assert_eq!(second.telemetry.status, ViewStatus::Invalid);
    let first = build(&torn, &paid(), &telemetry, CLOID);
    starts(
        &first.reasons,
        &[
            "the journal is truncated on line 6",
            "telemetry: line 2: malformed hedge_fill",
            "telemetry: line 5: no record type",
            "telemetry: telemetry session_id `other-session`",
        ],
    );
    assert_eq!(first.telemetry.lines, [3]);
    // The report lists the archive's reasons once and each IOC's own.
    let report = coverage::build(&torn, &paid(), &telemetry);
    assert_eq!(report.archive.reasons, first.reasons);
    let own = row(&report, SECOND);
    assert_eq!(own.class, Class::Contradiction);
    starts(
        &own.reasons,
        &[
            "telemetry: line 4: fee `false` is not a finite number",
            "telemetry: no hedge_fill for this CLOID",
            "venue fills do not cover this IOC",
        ],
    );
    assert!(row(&report, CLOID).reasons.is_empty());
    assert_rows_match_views(&torn, &paid(), &telemetry);
}

/// An orphan with a malformed number stays visible in the report; the
/// single view of a journaled CLOID still ignores it.
#[tokio::test]
async fn a_malformed_orphan_is_reported_and_the_single_view_ignores_it() {
    let base = String::from_utf8(written(info(), vec![record(1999.0, "fill")]).await).unwrap();
    let lines: Vec<&str> = base.lines().collect();
    let mut stray: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    stray["client_order_id"] = json!(OTHER);
    stray["fee"] = json!(false);
    let telemetry = [lines[0], lines[1], &stray.to_string(), lines[2]]
        .iter()
        .map(|l| format!("{l}\n"))
        .collect::<String>()
        .into_bytes();
    let one = reconciled(Some("1999"));
    let view = build(&one, &paid(), &telemetry, CLOID);
    assert_eq!(view.status, ViewStatus::Bound, "{:?}", view.reasons);
    let report = coverage::build(&one, &paid(), &telemetry);
    assert_eq!(report.status, CoverageStatus::Invalid);
    assert_eq!(report.coverage.ratio, None);
    let orphan = &report.orphan_telemetry[0];
    assert_eq!(
        (orphan.cloid.as_str(), orphan.lines.as_slice()),
        (OTHER, [3].as_slice())
    );
    assert!(
        orphan.reasons[0].contains("fee `false`"),
        "{:?}",
        orphan.reasons
    );
    assert_rows_match_views(&one, &paid(), &telemetry);
}
