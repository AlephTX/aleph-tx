//! Structured telemetry export for remote monitoring.
//!
//! The strategy hands `Copy` records to a bounded channel with `try_send`, so
//! the trading path never blocks or allocates. A background thread serializes
//! records as JSON lines into `<dir>/<session_id>.jsonl`. When the channel is
//! full, records are dropped and counted; the count is reported in snapshots.

use crate::types::events::{EventType, ShmPrivateEventV2};
use serde::Serialize;
use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::time::{SystemTime, UNIX_EPOCH};

/// Environment variable that enables telemetry export.
pub const TELEMETRY_DIR_ENV: &str = "ALEPH_TELEMETRY_DIR";
const CHANNEL_CAPACITY: usize = 8192;

/// Session metadata written once when the sink opens.
#[derive(Debug, Clone, Serialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub network: String,
    pub exchange: String,
    pub symbol: String,
    pub market_id: u64,
    pub account: i64,
    pub hedge_account: Option<String>,
}

/// Periodic account and strategy state.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct StrategySnapshot {
    pub equity: f64,
    pub session_start_equity: f64,
    pub available_balance: f64,
    pub margin_usage: f64,
    pub exchange_position: f64,
    pub quote_position: f64,
    pub confirmed_position: f64,
    pub pending_exposure: f64,
    pub worst_case_long: f64,
    pub worst_case_short: f64,
    pub mid: f64,
    pub spread_bps: f64,
    pub adverse_selection: f64,
    /// Signed trade cash flow less fees since session start.
    pub trade_cash_flow: f64,
    pub total_fees: f64,
    pub fill_count: u64,
    pub orders_placed: u64,
    pub orders_rejected: u64,
    pub active_orders: u32,
}

/// One private order or fill event, copied from the V2 ring.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct OrderEventRecord {
    pub kind: &'static str,
    pub sequence: u64,
    pub exchange_ts_ns: u64,
    pub client_order_id: i64,
    pub order_index: i64,
    pub side: &'static str,
    pub price: f64,
    pub size: f64,
    pub remaining: f64,
    pub fill_price: f64,
    pub fill_size: f64,
    pub fee: f64,
    pub trade_id: u64,
}

impl OrderEventRecord {
    pub fn from_event(event: &ShmPrivateEventV2) -> Self {
        let kind = match event.event_type() {
            Some(EventType::OrderCreated) => "created",
            Some(EventType::OrderFilled) => "filled",
            Some(EventType::OrderCanceled) => "canceled",
            Some(EventType::OrderRejected) => "rejected",
            Some(EventType::PrivateStreamLost) => "stream_lost",
            None => "unknown",
        };
        Self {
            kind,
            sequence: event.sequence,
            exchange_ts_ns: event.timestamp_ns,
            client_order_id: event.client_order_id,
            order_index: event.order_index,
            side: if event.is_ask == 1 { "sell" } else { "buy" },
            price: event.order_price,
            size: event.original_size,
            remaining: event.remaining_size,
            fill_price: event.fill_price,
            fill_size: event.fill_size,
            fee: event.fee_paid,
            trade_id: event.trade_id,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Record {
    Order(OrderEventRecord),
    Snapshot(StrategySnapshot),
    Stopped,
}

#[derive(Serialize)]
struct Line<'a, T: Serialize> {
    #[serde(rename = "type")]
    kind: &'a str,
    ts_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    dropped: Option<u64>,
    #[serde(flatten)]
    body: T,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Cloneable handle; sending never blocks.
#[derive(Clone)]
pub struct TelemetrySink {
    tx: SyncSender<Record>,
    dropped: Arc<AtomicU64>,
}

impl TelemetrySink {
    /// Open `<dir>/<session_id>.jsonl` (mode 0640) and start the writer thread.
    pub fn open(dir: &Path, session: &SessionInfo) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(format!("{}.jsonl", session.session_id));
        let file = OpenOptions::new()
            .create_new(true)
            .append(true)
            .mode(0o640)
            .open(&path)?;
        let mut out = BufWriter::new(file);
        write_line(&mut out, "session", None, session)?;
        out.flush()?;
        let (tx, rx) = sync_channel(CHANNEL_CAPACITY);
        let dropped = Arc::new(AtomicU64::new(0));
        let writer_dropped = Arc::clone(&dropped);
        std::thread::Builder::new()
            .name("telemetry-sink".into())
            .spawn(move || writer_loop(rx, out, path, writer_dropped))?;
        Ok(Self { tx, dropped })
    }

    /// Open from `ALEPH_TELEMETRY_DIR`; returns `None` when unset or on error.
    pub fn from_env(session: &SessionInfo) -> Option<Self> {
        let dir = std::env::var_os(TELEMETRY_DIR_ENV)?;
        match Self::open(Path::new(&dir), session) {
            Ok(sink) => Some(sink),
            Err(error) => {
                tracing::warn!(%error, "Telemetry export disabled");
                None
            }
        }
    }

    fn send(&self, record: Record) {
        match self.tx.try_send(record) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    pub fn order_event(&self, event: &ShmPrivateEventV2) {
        self.send(Record::Order(OrderEventRecord::from_event(event)));
    }

    pub fn snapshot(&self, snapshot: StrategySnapshot) {
        self.send(Record::Snapshot(snapshot));
    }

    /// Write a final marker; the writer flushes and exits after it.
    pub fn stopped(&self) {
        self.send(Record::Stopped);
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

fn write_line<T: Serialize>(
    out: &mut impl Write,
    kind: &str,
    dropped: Option<u64>,
    body: T,
) -> std::io::Result<()> {
    serde_json::to_writer(
        &mut *out,
        &Line {
            kind,
            ts_ms: now_ms(),
            dropped,
            body,
        },
    )?;
    out.write_all(b"\n")
}

fn writer_loop(
    rx: Receiver<Record>,
    mut out: BufWriter<std::fs::File>,
    path: PathBuf,
    dropped: Arc<AtomicU64>,
) {
    // Fills and lifecycle events are flushed immediately so a remote reader
    // sees them within one poll; snapshots ride along with the next flush.
    while let Ok(record) = rx.recv() {
        let result = match record {
            Record::Order(event) => {
                write_line(&mut out, "order", None, event).and_then(|()| out.flush())
            }
            Record::Snapshot(snapshot) => write_line(
                &mut out,
                "snapshot",
                Some(dropped.load(Ordering::Relaxed)),
                snapshot,
            )
            .and_then(|()| out.flush()),
            Record::Stopped => {
                let _ = write_line(&mut out, "stopped", None, serde_json::json!({}))
                    .and_then(|()| out.flush());
                break;
            }
        };
        if let Err(error) = result {
            tracing::warn!(path=%path.display(), %error, "Telemetry write failed; export stopped");
            break;
        }
    }
    let _ = out.get_ref().sync_data();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str) -> SessionInfo {
        SessionInfo {
            session_id: id.into(),
            network: "Testnet".into(),
            exchange: "lighter".into(),
            symbol: "ETH".into(),
            market_id: 0,
            account: 7,
            hedge_account: None,
        }
    }

    fn wait_for_lines(path: &Path, n: usize) -> Vec<serde_json::Value> {
        for _ in 0..200 {
            let text = std::fs::read_to_string(path).unwrap_or_default();
            let lines: Vec<_> = text
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect();
            if lines.len() >= n {
                return lines;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("timed out waiting for {n} telemetry lines");
    }

    #[test]
    fn writes_session_order_snapshot_and_stop_lines() {
        let dir = std::env::temp_dir().join(format!("aleph-telemetry-{}", uuid::Uuid::new_v4()));
        let sink = TelemetrySink::open(&dir, &session("s1")).unwrap();
        let fill = ShmPrivateEventV2::order_filled(
            9, 2, 1, 55, 1001, 77, 2500.5, 0.02, 0.0, -0.001, true, 123, 4242,
        );
        sink.order_event(&fill);
        sink.snapshot(StrategySnapshot {
            equity: 1000.0,
            fill_count: 1,
            ..Default::default()
        });
        sink.stopped();
        let lines = wait_for_lines(&dir.join("s1.jsonl"), 4);
        assert_eq!(lines[0]["type"], "session");
        assert_eq!(lines[0]["symbol"], "ETH");
        assert_eq!(lines[1]["type"], "order");
        assert_eq!(lines[1]["kind"], "filled");
        assert_eq!(lines[1]["side"], "sell");
        assert_eq!(lines[1]["trade_id"], 4242);
        assert_eq!(lines[1]["fill_price"], 2500.5);
        assert_eq!(lines[2]["type"], "snapshot");
        assert_eq!(lines[2]["equity"], 1000.0);
        assert_eq!(lines[2]["dropped"], 0);
        assert_eq!(lines[3]["type"], "stopped");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn refuses_to_overwrite_existing_session_file() {
        let dir = std::env::temp_dir().join(format!("aleph-telemetry-{}", uuid::Uuid::new_v4()));
        let _first = TelemetrySink::open(&dir, &session("dup")).unwrap();
        assert!(TelemetrySink::open(&dir, &session("dup")).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
