//! Cold audit of real schema 3 batch writes with frozen venue responses.

use super::*;
use crate::live_hedge::audit::{AuditStatus, Report, audit};
use serde_json::{Value, json};
use std::path::Path;

fn order(id: i64, status: &str, filled: &str, watermark: u64) -> Value {
    json!({"client_order_index": id, "market_index": 4095, "owner_account_index": 7,
        "order_index": id * 10, "status": status, "filled_base_amount": filled,
        "transaction_time": watermark, "price": "2000"})
}

fn audit_unresolved_batch(
    path: &Path,
    ids: [i64; 2],
    orders: Vec<Value>,
    watermark: u64,
    position: Option<(i64, &str)>,
) -> Report {
    let positions: Vec<_> = position
        .map(|(sign, size)| {
            json!({"market_id": 4095, "symbol": "ETH", "sign": sign, "position": size,
                "avg_entry_price": "2000", "unrealized_pnl": "0", "liquidation_price": "0"})
        })
        .into_iter()
        .collect();
    let evidence = serde_json::from_value(json!({
        "network": "Testnet", "hyperliquid_account": HL, "coin": "ETH",
        "lighter": {
            "account_index": 7, "market_id": 4095, "symbol": "ETH", "client_order_ids": ids,
            "order_histories": [{"observed_at_ms": 2_000, "response": {"code": 200, "orders": orders}}],
            "account": {"observed_at_ms": 2_100, "response": {"code": 200, "accounts": [
                {"index": 7, "transaction_time": watermark, "positions": positions}
            ]}}
        }
    }))
    .unwrap();
    let report = audit(&std::fs::read(path).unwrap(), Some(&evidence));
    assert_eq!(report.journal.schema, "v3");
    assert_eq!(report.journal.lines, 2);
    assert_eq!(report.primary_leg.journal_coverage, "complete");
    assert_eq!(report.primary_leg.journal_order_ids, ids);
    assert_eq!(
        report.primary_leg.status, "unresolved",
        "{:?}",
        report.issues
    );
    assert_eq!(
        report.status,
        AuditStatus::Unresolved,
        "{:?}",
        report.issues
    );
    assert!(!report.restart_permitted);
    report
}

#[test]
fn crash_after_sync_before_send_keeps_both_creates_unresolved() {
    const CHILD_PATH: &str = "ALEPH_TEST_SERIAL_JOURNAL_CRASH_PATH";
    const CRASH_EXIT: i32 = 73;
    let ids = [600, 601];

    // Re-execute this test in a child so abrupt termination bypasses all
    // destructors, including the submission guard and journal handle.
    if let Some(path) = std::env::var_os(CHILD_PATH) {
        let file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        let journal = SerialJournal::new(Box::new(file));
        let (client, _) = maker();
        let (txs, places) = prepared(ids[0], ids.len());
        runtime()
            .block_on(client.submit_prepared(
                txs,
                places,
                1,
                true,
                |places, cancels| {
                    std::future::ready(journal.commit(&batch_record(places, cancels)).map(|_| ()))
                },
                |_| -> std::future::Ready<Result<SendTxBatchResponse>> {
                    // The send callback has started but has had no venue effect.
                    std::process::exit(CRASH_EXIT);
                },
            ))
            .unwrap();
        panic!("child returned without reaching the crash boundary");
    }

    let (path, file) = temp_journal("crash-before-send");
    drop(file);
    let (_, module) = module_path!().split_once("::").unwrap();
    let test = format!("{module}::crash_after_sync_before_send_keeps_both_creates_unresolved");
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &test, "--nocapture"])
        .env(CHILD_PATH, &path)
        .output()
        .unwrap();
    assert_eq!(
        child.status.code(),
        Some(CRASH_EXIT),
        "{}\n{}",
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr)
    );

    let report = audit_unresolved_batch(&path, ids, vec![], 80, None);
    let leg = report.primary_leg;
    let orders: Vec<_> = leg
        .orders
        .iter()
        .map(|order| (order.client_order_id, order.journal_line, order.state))
        .collect();
    assert_eq!(
        orders,
        [(600, Some(2), "missing"), (601, Some(2), "missing")]
    );
    assert_eq!(leg.position, Some(Decimal::ZERO));
    assert_eq!(leg.required_watermark, None);
    assert_eq!(leg.residual, "unproven");
    std::fs::remove_file(path).unwrap();
}

#[test]
fn lost_batch_ack_gates_new_exposure_and_audit_requires_every_create() {
    let ids = [700, 701];
    let (path, file) = temp_journal("lost-batch-ack");
    let journal = SerialJournal::new(Box::new(file));
    let (client, tracker) = maker();
    let sends = AtomicUsize::new(0);
    let mut history = Vec::new();
    let (txs, places) = prepared(ids[0], ids.len());
    let rt = runtime();
    let outcome = rt.block_on(client.submit_prepared(
        txs,
        places,
        1,
        true,
        |places, cancels| {
            std::future::ready(journal.commit(&batch_record(places, cancels)).map(|_| ()))
        },
        |txs| {
            assert_eq!(txs.len(), 3);
            assert_eq!(journal.committed(), 1);
            sends.fetch_add(1, Ordering::AcqRel);
            // One create reached the venue and is terminal. There is no
            // acknowledgment for the batch or evidence for the other create.
            history.push(order(ids[0], "canceled", "0", 0));
            std::future::ready(Err(anyhow::anyhow!("injected lost batch acknowledgment")))
        },
    ));
    assert_eq!(
        outcome.unwrap_err().to_string(),
        "injected lost batch acknowledgment"
    );
    let mut active = tracker.active_cois();
    active.sort_unstable();
    assert_eq!(active, ids);
    assert!(tracker.needs_reconciliation());
    let error = rt
        .block_on(maker_batch(&client, Some(&journal), 710, ids.len(), &sends))
        .unwrap_err();
    assert!(
        error.to_string().contains("requires reconciliation"),
        "{error}"
    );
    assert_eq!(sends.load(Ordering::Acquire), 1);
    assert_eq!(journal.committed(), 1);
    drop((client, tracker, journal));

    let report = audit_unresolved_batch(&path, ids, history, 80, None);
    let leg = report.primary_leg;
    assert_eq!(leg.orders[0].state, "terminal");
    assert_eq!(leg.orders[1].state, "missing");
    assert_eq!(leg.position, Some(Decimal::ZERO));
    assert_eq!(
        (leg.watermark, leg.residual),
        ("orders_unresolved", "unproven")
    );
    std::fs::remove_file(path).unwrap();
}

#[test]
fn partial_fill_requires_terminal_orders_and_a_covering_account_sample() {
    let ids = [800, 801];
    let (path, file) = temp_journal("partial-fill");
    let journal = SerialJournal::new(Box::new(file));
    let (client, tracker) = maker();
    let sends = AtomicUsize::new(0);
    runtime()
        .block_on(maker_batch(
            &client,
            Some(&journal),
            ids[0],
            ids.len(),
            &sends,
        ))
        .unwrap();
    assert_eq!(sends.load(Ordering::Acquire), 1);
    drop((client, tracker, journal));

    // The 0.001 sell filled 0.0004. Even a covering flat account is
    // inconclusive while the remainder can still trade.
    let history = |status| {
        vec![
            order(ids[0], "canceled", "0", 0),
            order(ids[1], status, "0.0004", 55),
        ]
    };
    let live = audit_unresolved_batch(&path, ids, history("open"), 55, None).primary_leg;
    assert_eq!(live.orders[1].state, "live");
    assert_eq!(live.required_watermark, None);
    assert_eq!(
        (live.watermark, live.residual),
        ("orders_unresolved", "unproven")
    );

    let stale = audit_unresolved_batch(&path, ids, history("canceled"), 54, None).primary_leg;
    assert_eq!(stale.orders[1].state, "terminal");
    assert_eq!(stale.orders[1].filled, Some(Decimal::new(4, 4)));
    assert_eq!(stale.required_watermark, Some(55));
    assert_eq!((stale.watermark, stale.residual), ("stale", "unproven"));

    let fresh = audit_unresolved_batch(&path, ids, history("canceled"), 55, Some((-1, "0.0004")));
    assert!(
        fresh
            .issues
            .iter()
            .any(|issue| issue.kind == "lighter_residual")
    );
    let leg = fresh.primary_leg;
    assert_eq!(leg.position, Some(Decimal::new(-4, 4)));
    assert_eq!((leg.watermark, leg.residual), ("covered", "nonzero"));
    std::fs::remove_file(path).unwrap();
}
