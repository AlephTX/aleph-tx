//! Offline trial: one serial, fail-stop journal shared by a fake Lighter
//! maker (real `submit_prepared`, fake send) and a fake Hyperliquid hedge.
//! Temp files and fake sends only; no signer, network, account or key.
//!
//! Run the latency comparison with
//! `cargo test --release --lib benchmark_serial_journal -- --ignored --nocapture`
//! (also part of `make benchmark`).

use super::*;
use crate::live_hedge::journal::{
    IntentRecord, JournalRecord, JournalSink, LIGHTER_BATCH_SCHEMA, LighterBatchRecord,
    SerialJournal, StartRecord, TerminalRecord,
};
use anyhow::Context;
use rust_decimal::Decimal;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::time::Duration;

mod recovery_tests;

const HL: &str = "0x00000000000000000000000000000000000000aa";

fn start() -> StartRecord {
    StartRecord {
        session_id: "serial-trial".into(),
        network: "Testnet".into(),
        lighter_account_index: 7,
        hyperliquid_account: Some(HL.into()),
        symbol: "ETH".into(),
        journal_schema: Some(LIGHTER_BATCH_SCHEMA),
        lighter_market_id: Some(4095),
    }
}

fn temp_journal(name: &str) -> (std::path::PathBuf, std::fs::File) {
    use std::io::Write;
    let path = std::env::temp_dir().join(format!(
        "aleph-serial-{name}-{}-{}.jsonl",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .append(true)
        .open(&path)
        .unwrap();
    file.write_all(&JournalRecord::Start(start()).to_line().unwrap())
        .unwrap();
    file.sync_data().unwrap();
    (path, file)
}

/// `n` creates plus one cancel, signed-shape only.
fn prepared(first_coi: i64, creates: usize) -> (Vec<(u8, String)>, Vec<PlaceResult>) {
    let mut txs = vec![(15, "cancel".to_string())];
    let places = (0..creates)
        .map(|i| {
            txs.push((14, format!("create-{i}")));
            PlaceResult {
                client_order_index: first_coi + i as i64,
                side: if i % 2 == 0 { Side::Buy } else { Side::Sell },
                price: 2000.0 + i as f64,
                size: 0.001,
            }
        })
        .collect();
    (txs, places)
}

fn batch_record(places: &[PlaceResult], cancels: usize) -> JournalRecord {
    JournalRecord::LighterBatch(LighterBatchRecord {
        session_id: "serial-trial".into(),
        network: "Testnet".into(),
        lighter_account_index: 7,
        lighter_market_id: 4095,
        client_order_ids: places.iter().map(|p| p.client_order_index).collect(),
        cancels,
        prepared_at_ms: 1_000,
    })
}

fn ok_send(n: usize) -> Result<SendTxBatchResponse> {
    Ok(SendTxBatchResponse {
        tx_hash: Some((0..n).map(|i| format!("hash-{i}")).collect()),
    })
}

fn maker() -> (LighterTrading, Arc<OrderTracker>) {
    let tracker = Arc::new(OrderTracker::new());
    let mut client = LighterTrading::offline_test_client("http://127.0.0.1:9").with_test_account(7);
    client.set_order_tracker(Arc::clone(&tracker));
    (client, tracker)
}

/// One fake hedge IOC: intent then a response terminal that fills it.
/// `before` is the confirmed hedge position; returns the position after.
fn hedge_pair(round: u64, before: Decimal) -> (JournalRecord, JournalRecord, Decimal) {
    let size = Decimal::new(1, 3);
    let after = before - size;
    let cloid = format!("0x{round:032x}");
    let at = 2_000_000_000_000 + round;
    let intent = JournalRecord::Intent(IntentRecord {
        cloid: cloid.clone(),
        coin: "ETH".into(),
        submitted_at_ms: at,
        is_buy: false,
        size,
        limit_price: Decimal::new(1998, 0),
        reduce_only: false,
        primary_position: -after,
        hedge_position: before,
        primary_observation: Some(crate::live_hedge::journal::PrimaryObservation {
            account_index: 7,
            market_id: 4095,
            position: -after,
            transaction_time: Some(30 + round),
            request_us: 1_000,
            age_us: 2_000,
            max_age_us: 5_000_000,
        }),
    });
    let terminal = JournalRecord::Terminal(TerminalRecord {
        cloid,
        filled: size,
        average_price: Some(Decimal::new(1999, 0)),
        position: after,
        terminal_source: Some("response".into()),
        terminal_at_ms: Some(at),
        account_time_ms: Some(at + 5),
    });
    (intent, terminal, after)
}

/// Fake hedge loop: each IOC is "dispatched" (counted) only after its
/// intent is durable, and confirmed only after its terminal is durable.
/// Stops at the first commit error, as the live hedge stops on `record`.
fn run_hedge(
    journal: &SerialJournal,
    rounds: u64,
    sends: &AtomicUsize,
    stop: &AtomicBool,
) -> Vec<Duration> {
    let mut position = Decimal::ZERO;
    let mut durable = Vec::new();
    for round in 1..=rounds {
        if stop.load(Ordering::Acquire) {
            break;
        }
        let (intent, terminal, after) = hedge_pair(round, position);
        let Ok(timing) = journal.commit(&intent) else {
            break;
        };
        durable.push(timing.queued + timing.durable);
        sends.fetch_add(1, Ordering::AcqRel);
        if journal.commit(&terminal).is_err() {
            break;
        }
        position = after;
    }
    durable
}

/// One maker batch's timing. `prepare_to_send` runs from `submit_prepared`
/// entry (after signing) to the fake send: tracker reservation, queueing
/// for the journal and the durable record. It is not venue latency.
#[derive(Debug, Clone, Copy)]
struct Sample {
    prepare_to_send: Duration,
    queued: Duration,
    durable: Duration,
}

/// One mixed batch through the real `submit_prepared`. `journal` None is
/// the production no-op record.
async fn maker_batch(
    client: &LighterTrading,
    journal: Option<&SerialJournal>,
    first_coi: i64,
    creates: usize,
    sends: &AtomicUsize,
) -> Result<Sample> {
    use std::cell::Cell;
    let (txs, places) = prepared(first_coi, creates);
    let timing = Cell::new(None);
    let sent_after = Cell::new(None);
    let started = Instant::now();
    client
        .submit_prepared(
            txs,
            places,
            1,
            true,
            |places, cancels| {
                std::future::ready(match journal {
                    None => Ok(()),
                    Some(journal) => journal
                        .commit(&batch_record(places, cancels))
                        .map(|t| timing.set(Some(t))),
                })
            },
            |txs| {
                sent_after.set(Some(started.elapsed()));
                sends.fetch_add(1, Ordering::AcqRel);
                std::future::ready(ok_send(txs.len()))
            },
        )
        .await?;
    let timing = timing.get();
    Ok(Sample {
        prepare_to_send: sent_after.get().context("batch was not sent")?,
        queued: timing.map_or(Duration::ZERO, |t| t.queued),
        durable: timing.map_or(Duration::ZERO, |t| t.durable),
    })
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
}

/// Maker and hedge commit concurrently through one serial journal; every
/// fake send follows a durable record, and the v3 audit reads it back.
#[test]
fn maker_and_hedge_share_one_serial_journal() {
    let (path, file) = temp_journal("shared");
    let journal = SerialJournal::new(Box::new(file));
    let (client, tracker) = maker();
    let (maker_sends, hedge_sends, stop) = (
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicBool::new(false),
    );
    let (batches, creates, rounds) = (20, 3, 20);
    std::thread::scope(|scope| {
        let hedge = scope.spawn(|| run_hedge(&journal, rounds, &hedge_sends, &stop));
        runtime().block_on(async {
            for batch in 0..batches {
                let first = 1_000 + (batch * creates) as i64;
                maker_batch(&client, Some(&journal), first, creates, &maker_sends)
                    .await
                    .unwrap();
                // The batch line is on disk by the time its send returns.
                let text = std::fs::read_to_string(&path).unwrap();
                assert!(text.contains(&format!("[{first},")), "batch {batch}");
            }
        });
        assert_eq!(hedge.join().unwrap().len(), rounds as usize);
    });
    assert_eq!(maker_sends.load(Ordering::Acquire), batches);
    assert_eq!(hedge_sends.load(Ordering::Acquire), rounds as usize);
    assert_eq!(journal.committed(), (batches + 2 * rounds as usize) as u64);
    assert!(journal.poisoned().is_none());
    assert_eq!(tracker.active_cois().len(), batches * creates);
    let report = crate::live_hedge::audit::audit_files(&path, None).unwrap();
    assert_eq!(report.journal.schema, "v3");
    assert_eq!(report.journal.lines, 1 + batches + 2 * rounds as usize);
    assert!(
        !report
            .issues
            .iter()
            .any(|i| i.severity == crate::live_hedge::audit::Severity::Invalid),
        "{:?}",
        report.issues
    );
    assert!(
        report
            .orders
            .iter()
            .all(|o| o.status == crate::live_hedge::audit::OrderStatus::Recorded)
    );
    assert_eq!(report.orders.len(), rounds as usize);
    assert_eq!(report.primary_leg.journal_coverage, "complete");
    assert_eq!(
        report.primary_leg.journal_order_ids.len(),
        batches * creates
    );
    // Creates are journaled but have no venue evidence: still open.
    assert_eq!(report.primary_leg.status, "missing_evidence");
    assert_eq!(
        report.status,
        crate::live_hedge::audit::AuditStatus::Unresolved
    );
    assert!(!report.restart_permitted);
    if let Some(out) = std::env::var_os("ALEPH_SERIAL_TRIAL_OUT") {
        std::fs::copy(&path, out).unwrap();
    }
    std::fs::remove_file(path).unwrap();
}

/// Fault injector over a real file: after `ok_commits` good commits, the
/// next one writes only half its line (`Fault::PartialWrite`) or writes it
/// fully and then fails the sync (`Fault::SyncError`).
#[derive(Clone, Copy, PartialEq, Debug)]
enum Fault {
    PartialWrite,
    SyncError,
}

struct FaultySink {
    file: std::fs::File,
    ok_commits: usize,
    fault: Fault,
    writes: usize,
}

impl JournalSink for FaultySink {
    fn write_line(&mut self, line: &[u8]) -> std::io::Result<()> {
        self.writes += 1;
        if self.writes > self.ok_commits && self.fault == Fault::PartialWrite {
            std::io::Write::write_all(&mut self.file, &line[..line.len() / 2])?;
            return Err(std::io::Error::other("injected partial write"));
        }
        std::io::Write::write_all(&mut self.file, line)
    }
    fn sync(&mut self) -> std::io::Result<()> {
        if self.writes > self.ok_commits && self.fault == Fault::SyncError {
            return Err(std::io::Error::other("injected sync failure"));
        }
        self.file.sync_data()
    }
}

/// The first failure stops every producer: the failing batch is not sent
/// and releases its reservations, no later maker or hedge record is
/// appended, and the audit reports the damaged tail or the unsent
/// candidate, never a complete journal.
#[test]
fn first_journal_failure_stops_every_producer() {
    for fault in [Fault::PartialWrite, Fault::SyncError] {
        let (path, file) = temp_journal("fault");
        // Commits 1-2: maker batch, hedge intent. Commit 3: maker batch fails.
        let journal = SerialJournal::new(Box::new(FaultySink {
            file,
            ok_commits: 2,
            fault,
            writes: 0,
        }));
        let (client, tracker) = maker();
        let (maker_sends, hedge_sends, stop) = (
            AtomicUsize::new(0),
            AtomicUsize::new(0),
            AtomicBool::new(false),
        );
        let rt = runtime();
        rt.block_on(maker_batch(&client, Some(&journal), 100, 2, &maker_sends))
            .unwrap();
        let (intent, _, _) = hedge_pair(1, Decimal::ZERO);
        journal.commit(&intent).unwrap();
        hedge_sends.fetch_add(1, Ordering::AcqRel);
        let error = rt
            .block_on(maker_batch(&client, Some(&journal), 200, 2, &maker_sends))
            .unwrap_err();
        assert!(format!("{error:#}").contains("batch not sent"), "{fault:?}");
        assert_eq!(maker_sends.load(Ordering::Acquire), 1, "{fault:?}");
        assert!(journal.poisoned().is_some(), "{fault:?}");
        // The failed batch's reservations are released; the sent one stays.
        let mut active = tracker.active_cois();
        active.sort_unstable();
        assert_eq!(active, [100, 101], "{fault:?}");
        // Every later producer is refused without touching the file.
        let before = std::fs::read(&path).unwrap();
        assert!(
            run_hedge(&journal, 3, &hedge_sends, &stop).is_empty(),
            "{fault:?}"
        );
        assert!(
            rt.block_on(maker_batch(&client, Some(&journal), 300, 2, &maker_sends))
                .is_err()
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "{fault:?}: no append after failure"
        );
        assert_eq!(
            (
                maker_sends.load(Ordering::Acquire),
                hedge_sends.load(Ordering::Acquire)
            ),
            (1, 1)
        );
        let report = crate::live_hedge::audit::audit_files(&path, None).unwrap();
        let kinds: Vec<_> = report.issues.iter().map(|i| i.kind).collect();
        assert!(!report.restart_permitted);
        assert_eq!(
            report.orders.len(),
            1,
            "{fault:?}: the durable intent stays pending"
        );
        assert_eq!(
            report.orders[0].status,
            crate::live_hedge::audit::OrderStatus::Pending
        );
        match fault {
            Fault::PartialWrite => {
                // Damaged tail only, never a middle line: a prefix.
                assert!(kinds.contains(&"truncated_tail"), "{kinds:?}");
                assert_eq!(report.primary_leg.journal_coverage, "prefix");
                assert_eq!(report.primary_leg.journal_gaps, [4]);
                assert_eq!(report.primary_leg.journal_order_ids, [100, 101]);
            }
            Fault::SyncError => {
                // The written-but-unsynced candidate is kept and audited as
                // a recorded batch that was never sent: still open.
                assert_eq!(report.primary_leg.journal_order_ids, [100, 101, 200, 201]);
                assert_eq!(report.primary_leg.journal_coverage, "complete");
            }
        }
        assert_ne!(report.primary_leg.status, "explained");
        assert_ne!(
            report.status,
            crate::live_hedge::audit::AuditStatus::JournalRecorded
        );
        std::fs::remove_file(path).unwrap();
    }
}

/// A batch dropped while queued behind another producer's commit sends
/// nothing, keeps its reservations and gates new risk; the journal is not
/// poisoned and the holder's later commits proceed.
#[test]
fn batch_dropped_while_queued_sends_nothing() {
    let (path, file) = temp_journal("queued");
    let journal = Arc::new(SerialJournal::new(Box::new(file)));
    let (client, tracker) = maker();
    let sends = AtomicUsize::new(0);
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let holder = {
        let journal = Arc::clone(&journal);
        std::thread::spawn(move || {
            let guard = journal.hold();
            held_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            drop(guard);
        })
    };
    held_rx.recv().unwrap();
    let rt = runtime();
    let (txs, places) = prepared(500, 2);
    let queued = Arc::clone(&journal);
    let pending = client.submit_prepared(
        txs,
        places,
        1,
        true,
        |places, cancels| {
            let record = batch_record(places, cancels);
            // Blocks on the held lock off the runtime thread.
            async move {
                tokio::task::spawn_blocking(move || queued.commit(&record).map(|_| ()))
                    .await
                    .map_err(anyhow::Error::from)?
            }
        },
        |txs| {
            sends.fetch_add(1, Ordering::AcqRel);
            std::future::ready(ok_send(txs.len()))
        },
    );
    let outcome =
        rt.block_on(async { tokio::time::timeout(Duration::from_millis(50), pending).await });
    assert!(outcome.is_err(), "still queued when dropped");
    assert_eq!(sends.load(Ordering::Acquire), 0);
    assert_eq!(tracker.active_cois().len(), 2);
    assert!(tracker.needs_reconciliation());
    release_tx.send(()).unwrap();
    holder.join().unwrap();
    // The detached commit may still land after the drop: a durable
    // candidate that was never sent, which the audit keeps open.
    drop(rt);
    assert!(journal.poisoned().is_none());
    let (intent, _, _) = hedge_pair(1, Decimal::ZERO);
    journal.commit(&intent).unwrap();
    let report = crate::live_hedge::audit::audit_files(&path, None).unwrap();
    assert_ne!(report.primary_leg.status, "explained");
    assert!(
        report
            .primary_leg
            .journal_order_ids
            .iter()
            .all(|id| [500, 501].contains(id))
    );
    std::fs::remove_file(path).unwrap();
}

fn quantiles(mut values: Vec<Duration>) -> String {
    if values.is_empty() {
        return "n=0".into();
    }
    values.sort_unstable();
    let at = |q: f64| {
        let index = ((values.len() as f64 * q).ceil() as usize).clamp(1, values.len()) - 1;
        values[index].as_secs_f64() * 1e6
    };
    format!(
        "n={} p50={:.1}us p95={:.1}us p99={:.1}us max={:.1}us",
        values.len(),
        at(0.50),
        at(0.95),
        at(0.99),
        at(1.0)
    )
}

/// One scenario: `batches` maker batches of `creates` creates, optionally
/// through the serial journal, optionally against a concurrent fake hedge
/// committing intent/terminal pairs as fast as it can.
fn scenario(name: &str, batches: usize, creates: usize, journaled: bool, contended: bool) {
    let (path, file) = temp_journal(name);
    let journal = SerialJournal::new(Box::new(file));
    let (client, _tracker) = maker();
    let (maker_sends, hedge_sends, stop) = (
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicBool::new(false),
    );
    let samples = std::thread::scope(|scope| {
        let hedge =
            contended.then(|| scope.spawn(|| run_hedge(&journal, u64::MAX, &hedge_sends, &stop)));
        let rt = runtime();
        let samples: Vec<Sample> = (0..batches)
            .map(|batch| {
                let first = 1 + (batch * creates) as i64;
                rt.block_on(maker_batch(
                    &client,
                    journaled.then_some(&journal),
                    first,
                    creates,
                    &maker_sends,
                ))
                .unwrap()
            })
            .collect();
        stop.store(true, Ordering::Release);
        if let Some(hedge) = hedge {
            hedge.join().unwrap();
        }
        samples
    });
    println!(
        "serial_journal {name}: batches={batches} creates/batch={creates} hedge_sends={} \
         prepare_to_send[{}] queued[{}] durable[{}]",
        hedge_sends.load(Ordering::Acquire),
        quantiles(samples.iter().map(|s| s.prepare_to_send).collect()),
        quantiles(samples.iter().map(|s| s.queued).collect()),
        quantiles(samples.iter().map(|s| s.durable).collect()),
    );
    assert_eq!(maker_sends.load(Ordering::Acquire), batches);
    std::fs::remove_file(path).unwrap();
}

/// Prepare-to-send boundary cost of the serial journal versus the no-op
/// production record, on the machine running it. Local file system only;
/// numbers are not venue latency and not a production p99.
#[test]
#[ignore]
fn benchmark_serial_journal_prepare_to_send() {
    let batches = std::env::var("ALEPH_SERIAL_BATCHES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2_000);
    println!(
        "serial_journal host: os={} arch={} temp_dir={}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::env::temp_dir().display()
    );
    for creates in [2, 10, 49] {
        scenario(
            &format!("noop_baseline_c{creates}"),
            batches,
            creates,
            false,
            false,
        );
        scenario(
            &format!("serial_idle_c{creates}"),
            batches,
            creates,
            true,
            false,
        );
        scenario(
            &format!("serial_contended_c{creates}"),
            batches,
            creates,
            true,
            true,
        );
    }
}
