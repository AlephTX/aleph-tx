//! The mixed-batch durability boundary: the real `submit_prepared` with the
//! offline batch journal and a fake send. No signer, network or account.

use super::*;
use crate::live_hedge::journal::{LIGHTER_BATCH_SCHEMA, LighterBatchJournal, StartRecord};
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicUsize;

/// Two creates and one cancel, as `execute_batch` hands them over after
/// signing: (tx_type, tx_info) per action, PlaceResult per create.
fn prepared() -> (Vec<(u8, String)>, Vec<PlaceResult>) {
    let txs = vec![
        (14, "create-101".into()),
        (15, "cancel-9".into()),
        (14, "create-102".into()),
    ];
    let places = vec![
        PlaceResult {
            client_order_index: 101,
            side: Side::Buy,
            price: 2000.0,
            size: 0.01,
        },
        PlaceResult {
            client_order_index: 102,
            side: Side::Sell,
            price: 2001.0,
            size: 0.02,
        },
    ];
    (txs, places)
}

fn trading() -> (LighterTrading, Arc<OrderTracker>) {
    let tracker = Arc::new(OrderTracker::new());
    let mut client = LighterTrading::offline_test_client("http://127.0.0.1:9").with_test_account(7);
    client.set_order_tracker(Arc::clone(&tracker));
    (client, tracker)
}

fn start() -> StartRecord {
    StartRecord {
        session_id: "batch-trial".into(),
        network: "Testnet".into(),
        lighter_account_index: 7,
        hyperliquid_account: None,
        symbol: "ETH".into(),
        journal_schema: Some(LIGHTER_BATCH_SCHEMA),
        lighter_market_id: Some(4095),
    }
}

/// A fresh schema 3 journal in a temp dir with its start line.
fn journal_file(name: &str) -> (std::path::PathBuf, std::fs::File) {
    use std::io::Write;
    let path = std::env::temp_dir().join(format!(
        "aleph-batch-{name}-{}-{}.jsonl",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .append(true)
        .open(&path)
        .unwrap();
    file.write_all(
        &crate::live_hedge::journal::JournalRecord::Start(start())
            .to_line()
            .unwrap(),
    )
    .unwrap();
    file.sync_data().unwrap();
    (path, file)
}

fn ok_send(n: usize) -> Result<SendTxBatchResponse> {
    Ok(SendTxBatchResponse {
        tx_hash: Some((0..n).map(|i| format!("hash-{i}")).collect()),
    })
}

fn journaled_lines(path: &std::path::Path) -> usize {
    std::fs::read_to_string(path)
        .unwrap()
        .matches("\"lighter_create_batch\"")
        .count()
}

/// Synced record, then send: the fake send sees the record on disk, and
/// the record lists every create of the mixed batch.
#[tokio::test]
async fn batch_creates_are_durable_before_send() {
    let (client, tracker) = trading();
    let (path, file) = journal_file("ok");
    let mut journal = LighterBatchJournal::new(file, &start()).unwrap();
    let order = StdMutex::new(Vec::new());
    let (txs, places) = prepared();
    let result = client
        .submit_prepared(
            txs,
            places,
            1,
            true,
            |places, cancels| {
                let ids = places.iter().map(|p| p.client_order_index).collect();
                let written = journal
                    .record(ids, cancels)
                    .and_then(|record| journal.append(&record));
                order.lock().unwrap().push("recorded");
                std::future::ready(written)
            },
            |txs| {
                order.lock().unwrap().push("sent");
                // Durable before the send: the line is already on disk.
                assert_eq!(journaled_lines(&path), 1);
                assert_eq!(txs.len(), 3);
                std::future::ready(ok_send(txs.len()))
            },
        )
        .await
        .unwrap();
    assert_eq!(*order.lock().unwrap(), ["recorded", "sent"]);
    assert_eq!(result.place_results.len(), 2);
    let mut active = tracker.active_cois();
    active.sort_unstable();
    assert_eq!(active, [101, 102]);
    assert!(!tracker.needs_reconciliation());
    let text = std::fs::read_to_string(&path).unwrap();
    let record: serde_json::Value = serde_json::from_str(text.lines().nth(1).unwrap()).unwrap();
    assert_eq!(record["client_order_ids"], serde_json::json!([101, 102]));
    assert_eq!(record["cancels"], 1);
    assert_eq!(record["lighter_account_index"], 7);
    assert_eq!(record["lighter_market_id"], 4095);
    std::fs::remove_file(path).unwrap();
}

/// A record that cannot be written (read-only handle) or synced (injected
/// error after the write) sends nothing and releases the reservations.
#[tokio::test]
async fn failed_record_sends_nothing() {
    for case in ["write", "sync"] {
        let (client, tracker) = trading();
        let (path, file) = journal_file(case);
        drop(file);
        let handle = if case == "write" {
            std::fs::File::open(&path).unwrap() // read-only: write_all fails
        } else {
            std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap()
        };
        let mut journal = LighterBatchJournal::new(handle, &start()).unwrap();
        let sends = AtomicUsize::new(0);
        let (txs, places) = prepared();
        let error = client
            .submit_prepared(
                txs,
                places,
                1,
                true,
                |places, cancels| {
                    let ids = places.iter().map(|p| p.client_order_index).collect();
                    let mut written = journal
                        .record(ids, cancels)
                        .and_then(|record| journal.append(&record));
                    if case == "sync" {
                        written = written.and(Err(anyhow::anyhow!("injected sync failure")));
                    }
                    std::future::ready(written)
                },
                |txs| {
                    sends.fetch_add(1, Ordering::SeqCst);
                    std::future::ready(ok_send(txs.len()))
                },
            )
            .await
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("batch not sent"),
            "{case}: {error:#}"
        );
        assert_eq!(sends.load(Ordering::SeqCst), 0, "{case}");
        assert!(
            tracker.active_cois().is_empty(),
            "{case}: reservations released"
        );
        assert!(!tracker.needs_reconciliation(), "{case}");
        // A synced-then-failed record stays on disk: conservative, audited.
        assert_eq!(
            journaled_lines(&path),
            usize::from(case == "sync"),
            "{case}"
        );
        std::fs::remove_file(path).unwrap();
    }
}

/// After a durable record, a lost or incomplete send keeps every create
/// reserved and requires reconciliation; the journal names all of them.
/// A dropped future during the record also keeps them reserved.
#[tokio::test]
async fn uncertain_send_keeps_every_recorded_create_open() {
    let (client, tracker) = trading();
    let (path, file) = journal_file("lost");
    let mut journal = LighterBatchJournal::new(file, &start()).unwrap();
    let (txs, places) = prepared();
    let error = client
        .submit_prepared(
            txs,
            places,
            1,
            true,
            |places, cancels| {
                let ids = places.iter().map(|p| p.client_order_index).collect();
                std::future::ready(
                    journal
                        .record(ids, cancels)
                        .and_then(|record| journal.append(&record)),
                )
            },
            |_| {
                std::future::ready(Err(TradingError::SubmissionUncertain(
                    "response lost".into(),
                )
                .into()))
            },
        )
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("response lost"));
    let mut active = tracker.active_cois();
    active.sort_unstable();
    assert_eq!(active, [101, 102], "ACK never seen: both stay reserved");
    assert!(tracker.needs_reconciliation());
    assert_eq!(journaled_lines(&path), 1);
    // The real audit reads the trial journal: both IDs need venue evidence.
    let report = crate::live_hedge::audit::audit_files(&path, None).unwrap();
    assert_eq!(report.journal.schema, "v3");
    assert_eq!(report.primary_leg.journal_order_ids, [101, 102]);
    assert_eq!(report.primary_leg.status, "missing_evidence");
    assert!(!report.restart_permitted);
    // Keep the trial journal for the offline CLI when asked to.
    if let Some(out) = std::env::var_os("ALEPH_BATCH_TRIAL_OUT") {
        std::fs::copy(&path, out).unwrap();
    }
    std::fs::remove_file(&path).unwrap();

    // Dropped while recording: nothing sent, reservations kept, gated.
    let (client, tracker) = trading();
    let (txs, places) = prepared();
    let sends = AtomicUsize::new(0);
    let pending = client.submit_prepared(
        txs,
        places,
        1,
        true,
        |_, _| std::future::pending::<Result<()>>(),
        |txs| {
            sends.fetch_add(1, Ordering::SeqCst);
            std::future::ready(ok_send(txs.len()))
        },
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(20), pending)
            .await
            .is_err()
    );
    assert_eq!(sends.load(Ordering::SeqCst), 0);
    assert_eq!(tracker.active_cois().len(), 2);
    assert!(tracker.needs_reconciliation());
}

/// Cancel-only batches write no record and still send.
#[tokio::test]
async fn cancel_only_batch_writes_no_record() {
    let (client, _tracker) = trading();
    let records = AtomicUsize::new(0);
    client
        .submit_prepared(
            vec![(15, "cancel-9".into())],
            Vec::new(),
            1,
            false,
            |_, _| {
                records.fetch_add(1, Ordering::SeqCst);
                std::future::ready(Ok(()))
            },
            |txs| std::future::ready(ok_send(txs.len())),
        )
        .await
        .unwrap();
    assert_eq!(records.load(Ordering::SeqCst), 0);
}
