//! Session journal records shared by the live writer and the offline audit.
//!
//! One JSON object per line, tagged by `event`. The live process appends a
//! record and fsyncs it before acting on it, so a line is the durable claim
//! of this process, not a venue confirmation.
//!
//! Compatibility: journals written before `journal_schema` existed carry no
//! schema field on `start` and no timestamps on `hedge_terminal`. Those
//! fields are optional here so legacy journals still parse; the audit reports
//! them as legacy instead of assuming values. Schema 1 has no Lighter market
//! on `start` and no primary observation on intents; schema 2 requires both.
//! Schema 3 adds `lighter_create_batch`: the client order IDs of one Lighter
//! batch, written and synced before the batch is sent. Only offline trials
//! write schema 3; the supervised launcher still writes schema 2.

use anyhow::{Context, Result};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

/// Schema written by this version on the `start` record.
pub const JOURNAL_SCHEMA: u32 = 2;

/// Schema whose journals may carry `lighter_create_batch` records.
pub const LIGHTER_BATCH_SCHEMA: u32 = 3;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartRecord {
    pub session_id: String,
    /// `Mainnet` or `Testnet`.
    pub network: String,
    pub lighter_account_index: i64,
    /// Hyperliquid account when the inventory hedge is configured.
    pub hyperliquid_account: Option<String>,
    pub symbol: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub journal_schema: Option<u32>,
    /// Configured Lighter market index. Schema 2 and later.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lighter_market_id: Option<i64>,
}

/// The Lighter position read an intent was planned against, as this process
/// observed it. A local claim: it does not prove the venue's state, its
/// generation time or the response's authenticity. Schema 2 and later.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrimaryObservation {
    pub account_index: i64,
    pub market_id: i64,
    /// Signed base position; equals the intent's `primary_position`.
    #[serde(with = "rust_decimal::serde::str")]
    pub position: Decimal,
    /// Account `transaction_time` trade watermark; None when the response
    /// carried none. Not a wall-clock time.
    pub transaction_time: Option<u64>,
    /// Local monotonic microseconds from request start to parsed response.
    pub request_us: u64,
    /// Local monotonic microseconds from request start to forming this
    /// intent (rounded up), before fsync, signing and dispatch.
    pub age_us: u64,
    /// The age budget the live check enforced, in microseconds.
    pub max_age_us: u64,
}

/// Largest Lighter observation age budget a schema 2 writer may enforce:
/// the live MAX_ACCOUNT_AGE_MS. A record claiming a larger budget is invalid.
pub const MAX_PRIMARY_AGE_US: u64 = super::MAX_ACCOUNT_AGE_MS * 1_000;

impl PrimaryObservation {
    /// The single freshness rule shared by the writer and the audit: the
    /// rounded-up age is strictly inside a positive budget no larger than
    /// MAX_PRIMARY_AGE_US. The writer journals only records that pass.
    pub fn within_budget(&self) -> bool {
        self.budget_allowed() && self.age_us < self.max_age_us
    }

    /// Whether the self-reported budget is one a writer could enforce.
    pub fn budget_allowed(&self) -> bool {
        self.max_age_us > 0 && self.max_age_us <= MAX_PRIMARY_AGE_US
    }
}

/// Written and fsynced before the IOC is signed and dispatched.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntentRecord {
    pub cloid: String,
    pub coin: String,
    /// Local time when the intent was formed, before fsync and signing.
    pub submitted_at_ms: u64,
    pub is_buy: bool,
    #[serde(with = "rust_decimal::serde::str")]
    pub size: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub limit_price: Decimal,
    pub reduce_only: bool,
    #[serde(with = "rust_decimal::serde::str")]
    pub primary_position: Decimal,
    /// Reconciled Hyperliquid position the IOC was planned against.
    #[serde(with = "rust_decimal::serde::str")]
    pub hedge_position: Decimal,
    /// Schema 2: the Lighter observation behind `primary_position`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_observation: Option<PrimaryObservation>,
}

/// Written once a terminal result and a later account snapshot agree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalRecord {
    pub cloid: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub filled: Decimal,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub average_price: Option<Decimal>,
    /// Hyperliquid position in the account snapshot that confirmed the fill.
    #[serde(with = "rust_decimal::serde::str")]
    pub position: Decimal,
    /// `response` (synchronous order reply, local dispatch time) or `lookup`
    /// (venue order status, venue status timestamp). Absent in legacy files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_at_ms: Option<u64>,
    /// Venue time of the confirming account snapshot. Absent in legacy files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_time_ms: Option<u64>,
}

/// The creates of one Lighter batch, written and synced before the batch is
/// sent. It proves only that this process prepared to try them: a crash can
/// fall between sync and send, and a send can be partly accepted or lost.
/// Cancel-only batches and batches that fail before this point write none.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LighterBatchRecord {
    pub session_id: String,
    pub network: String,
    pub lighter_account_index: i64,
    pub lighter_market_id: i64,
    /// Client order IDs of every create in the batch, in batch order.
    pub client_order_ids: Vec<i64>,
    /// Number of cancels in the same batch (diagnostic only).
    pub cancels: usize,
    /// Local wall-clock time when the record was formed (diagnostic only).
    pub prepared_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum JournalRecord {
    Start(StartRecord),
    Intent(IntentRecord),
    Terminal(TerminalRecord),
    LighterBatch(LighterBatchRecord),
    VerifiedFlat,
}

/// Why a journal line could not be read as a record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    Malformed(String),
    MissingEvent,
    UnknownEvent(String),
    Fields { event: String, detail: String },
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(detail) => write!(f, "not a JSON object: {detail}"),
            Self::MissingEvent => write!(f, "missing string `event` field"),
            Self::UnknownEvent(event) => write!(f, "unknown event `{event}`"),
            Self::Fields { event, detail } => write!(f, "invalid `{event}` fields: {detail}"),
        }
    }
}

impl JournalRecord {
    pub fn event(&self) -> &'static str {
        match self {
            Self::Start(_) => "start",
            Self::Intent(_) => "hedge_intent",
            Self::Terminal(_) => "hedge_terminal",
            Self::LighterBatch(_) => "lighter_create_batch",
            Self::VerifiedFlat => "verified_flat",
        }
    }

    /// One journal line including the trailing newline.
    pub fn to_line(&self) -> Result<Vec<u8>> {
        let mut value = match self {
            Self::Start(record) => serde_json::to_value(record)?,
            Self::Intent(record) => serde_json::to_value(record)?,
            Self::Terminal(record) => serde_json::to_value(record)?,
            Self::LighterBatch(record) => serde_json::to_value(record)?,
            Self::VerifiedFlat => serde_json::Value::Object(Default::default()),
        };
        value
            .as_object_mut()
            .context("Journal record must serialize to an object")?
            .insert("event".into(), self.event().into());
        let mut line = serde_json::to_vec(&value)?;
        line.push(b'\n');
        Ok(line)
    }

    /// Parse one line without its newline. Unknown events and unknown or
    /// missing fields are errors, never defaults.
    pub fn parse(line: &[u8]) -> std::result::Result<Self, ParseError> {
        let value: serde_json::Value =
            serde_json::from_slice(line).map_err(|e| ParseError::Malformed(e.to_string()))?;
        let serde_json::Value::Object(mut object) = value else {
            return Err(ParseError::Malformed(
                "top-level value is not an object".into(),
            ));
        };
        let event = match object.remove("event") {
            Some(serde_json::Value::String(event)) => event,
            _ => return Err(ParseError::MissingEvent),
        };
        let body = serde_json::Value::Object(object);
        fn fields<T: DeserializeOwned>(
            event: &str,
            body: serde_json::Value,
        ) -> std::result::Result<T, ParseError> {
            serde_json::from_value(body).map_err(|e| ParseError::Fields {
                event: event.into(),
                detail: e.to_string(),
            })
        }
        match event.as_str() {
            "start" => fields(&event, body).map(Self::Start),
            "hedge_intent" => fields(&event, body).map(Self::Intent),
            "hedge_terminal" => fields(&event, body).map(Self::Terminal),
            "lighter_create_batch" => fields(&event, body).map(Self::LighterBatch),
            "verified_flat" => {
                if body.as_object().is_some_and(|o| o.is_empty()) {
                    Ok(Self::VerifiedFlat)
                } else {
                    Err(ParseError::Fields {
                        event,
                        detail: "verified_flat carries no fields".into(),
                    })
                }
            }
            _ => Err(ParseError::UnknownEvent(event)),
        }
    }
}

/// Append-and-sync writer for `lighter_create_batch` records in a schema 3
/// journal. Offline trials only; the supervised launcher does not create it.
pub struct LighterBatchJournal {
    file: std::fs::File,
    session_id: String,
    network: String,
    account_index: i64,
    market_id: i64,
}

impl LighterBatchJournal {
    /// Wrap a journal whose `start` declares schema 3 with this identity.
    pub fn new(file: std::fs::File, start: &StartRecord) -> Result<Self> {
        anyhow::ensure!(
            start.journal_schema == Some(LIGHTER_BATCH_SCHEMA),
            "Lighter batch records need a schema {LIGHTER_BATCH_SCHEMA} journal"
        );
        Ok(Self {
            file,
            session_id: start.session_id.clone(),
            network: start.network.clone(),
            account_index: start.lighter_account_index,
            market_id: start
                .lighter_market_id
                .context("Schema 3 start lacks lighter_market_id")?,
        })
    }

    /// The record for one batch's creates.
    pub fn record(&self, client_order_ids: Vec<i64>, cancels: usize) -> Result<JournalRecord> {
        let prepared_at_ms = u64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_millis(),
        )?;
        Ok(JournalRecord::LighterBatch(LighterBatchRecord {
            session_id: self.session_id.clone(),
            network: self.network.clone(),
            lighter_account_index: self.account_index,
            lighter_market_id: self.market_id,
            client_order_ids,
            cancels,
            prepared_at_ms,
        }))
    }

    /// One write, then `sync_data`; returns only once the line is durable.
    pub fn append(&mut self, record: &JournalRecord) -> Result<()> {
        use std::io::Write;
        self.file.write_all(&record.to_line()?)?;
        self.file.sync_data()?;
        Ok(())
    }
}

/// Serial, fail-stop committer shared by every producer of one journal
/// (offline trials only). Each `commit` writes one line and syncs it under
/// a single lock; it returns Ok only when that line is durable. The first
/// write or sync error poisons the journal for every producer: no later
/// record is appended, so a failure leaves at most one damaged tail and
/// never a damaged middle line. Records written before the failure stay.
pub struct SerialJournal {
    inner: parking_lot::Mutex<SerialInner>,
}

struct SerialInner {
    file: Box<dyn JournalSink>,
    poisoned: Option<String>,
    /// Records that returned Ok, in commit order.
    committed: u64,
}

/// The byte sink behind a `SerialJournal`; a file, or a fault injector.
pub trait JournalSink: Send {
    fn write_line(&mut self, line: &[u8]) -> std::io::Result<()>;
    fn sync(&mut self) -> std::io::Result<()>;
}

impl JournalSink for std::fs::File {
    fn write_line(&mut self, line: &[u8]) -> std::io::Result<()> {
        std::io::Write::write_all(self, line)
    }
    fn sync(&mut self) -> std::io::Result<()> {
        self.sync_data()
    }
}

/// Time spent in one commit, split at the lock.
#[derive(Debug, Clone, Copy)]
pub struct CommitTiming {
    /// Waiting for another producer's commit.
    pub queued: std::time::Duration,
    /// Encode, write and sync of this record.
    pub durable: std::time::Duration,
}

impl SerialJournal {
    pub fn new(sink: Box<dyn JournalSink>) -> Self {
        Self {
            inner: parking_lot::Mutex::new(SerialInner {
                file: sink,
                poisoned: None,
                committed: 0,
            }),
        }
    }

    /// Append `record` and sync it; Ok only once it is durable.
    pub fn commit(&self, record: &JournalRecord) -> Result<CommitTiming> {
        let line = record.to_line()?;
        let waited = std::time::Instant::now();
        let mut inner = self.inner.lock();
        let queued = waited.elapsed();
        if let Some(reason) = &inner.poisoned {
            anyhow::bail!("journal stopped after an earlier failure: {reason}");
        }
        let started = std::time::Instant::now();
        let result = inner
            .file
            .write_line(&line)
            .context("journal write failed")
            .and_then(|()| inner.file.sync().context("journal sync failed"));
        if let Err(error) = result {
            inner.poisoned = Some(format!("{error:#}"));
            return Err(error);
        }
        inner.committed += 1;
        Ok(CommitTiming {
            queued,
            durable: started.elapsed(),
        })
    }

    /// The failure that stopped this journal, if any.
    pub fn poisoned(&self) -> Option<String> {
        self.inner.lock().poisoned.clone()
    }

    pub fn committed(&self) -> u64 {
        self.inner.lock().committed
    }

    /// Test builds only: hold the commit lock, as a slow producer would.
    #[cfg(test)]
    pub(crate) fn hold(&self) -> impl Drop + '_ {
        self.inner.lock()
    }
}
