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

use anyhow::{Context, Result};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

/// Schema written by this version on the `start` record.
pub const JOURNAL_SCHEMA: u32 = 2;

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

#[derive(Debug, Clone, PartialEq)]
pub enum JournalRecord {
    Start(StartRecord),
    Intent(IntentRecord),
    Terminal(TerminalRecord),
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
            Self::VerifiedFlat => "verified_flat",
        }
    }

    /// One journal line including the trailing newline.
    pub fn to_line(&self) -> Result<Vec<u8>> {
        let mut value = match self {
            Self::Start(record) => serde_json::to_value(record)?,
            Self::Intent(record) => serde_json::to_value(record)?,
            Self::Terminal(record) => serde_json::to_value(record)?,
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
