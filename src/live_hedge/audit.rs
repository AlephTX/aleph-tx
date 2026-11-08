//! Offline, read-only audit of an interrupted session journal.
//!
//! Rebuilds every Hyperliquid IOC from the durable journal alone and explains
//! what is proven at three layers:
//! 1. the journal's own claims (`hedge_terminal` written by this process),
//! 2. frozen venue evidence supplied by the operator (raw `orderStatus` and
//!    `clearinghouseState` responses), checked with the same identity,
//!    terminal and account-delta rules the live hedge uses,
//! 3. what remains unproven and which evidence would resolve it.
//!
//! The audit never contacts a venue, never reads credentials, never cancels
//! or resubmits, and never touches the session file or lock. A matched
//! result is only "offline evidence matches"; it does not permit restart.
//! Lighter (primary leg) orders and inventory are not journaled. Schema 2
//! intents carry the single Lighter position read each IOC was planned
//! against; the report checks it for identity and internal consistency only,
//! as a local claim. An optional `lighter` evidence section replays the
//! production recovery judgement for caller-declared client orders and one
//! account sample (see `lighter`); it never changes a Hyperliquid result.

use super::journal::{
    IntentRecord, JournalRecord, LighterBatchRecord, MAX_PRIMARY_AGE_US, ParseError,
    PrimaryObservation, StartRecord, TerminalRecord,
};
use super::{
    CLOCK_TOLERANCE_MS, IocIdentity, LOOKUP_SKEW_MS, LookupOutcome, account_confirmed_fill,
    classify_lookup, dedicated_position,
};
use crate::exchanges::hyperliquid::{AccountState, OrderLookup, validate_hex};
use anyhow::{Context, Result};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::Path;

mod lighter;
pub use lighter::{LighterEvidence, LighterOrderReport, PrimaryLegReport, RawResponse};

/// Operator-supplied, frozen venue observations for one hedge account.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Evidence {
    /// `Mainnet` or `Testnet`; must match the journal's `start`.
    pub network: String,
    pub hyperliquid_account: String,
    pub coin: String,
    #[serde(default)]
    pub lookups: Vec<LookupEvidence>,
    #[serde(default)]
    pub account_snapshot: Option<SnapshotEvidence>,
    /// Frozen Lighter order histories and account sample; optional.
    #[serde(default)]
    pub lighter: Option<LighterEvidence>,
}

/// One raw `{"type":"orderStatus"}` response and when it was captured.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LookupEvidence {
    pub cloid: String,
    pub observed_at_ms: u64,
    pub response: serde_json::Value,
}

/// One raw `{"type":"clearinghouseState"}` response and when it was captured.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotEvidence {
    pub observed_at_ms: u64,
    pub state: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditStatus {
    /// Every IOC has a consistent journaled terminal; no venue evidence used.
    JournalRecorded,
    /// No unresolved IOC remains and at least one was resolved by evidence.
    EvidenceMatched,
    /// Some IOC or record cannot be proven either way yet.
    Unresolved,
    /// The journal or evidence contradicts itself or the live contract.
    Invalid,
}

impl AuditStatus {
    /// 0 means every journaled IOC is explained. It is never permission to
    /// restart: the session lock and the primary leg are outside the audit.
    pub fn exit_code(self) -> i32 {
        match self {
            Self::JournalRecorded | Self::EvidenceMatched => 0,
            Self::Unresolved => 2,
            Self::Invalid => 3,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Note,
    Unresolved,
    Invalid,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Issue {
    /// 1-based journal line, when the issue comes from the journal.
    pub line: Option<usize>,
    pub severity: Severity,
    pub kind: &'static str,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderStatus {
    /// Journal holds a consistent terminal record.
    Recorded,
    /// No journaled terminal, but venue lookup and account snapshot agree.
    Matched,
    /// Outcome unknown; exposure must still be assumed.
    Pending,
    /// Records or evidence for this IOC contradict each other.
    Invalid,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OrderReport {
    pub cloid: String,
    pub intent_line: usize,
    pub terminal_lines: Vec<usize>,
    pub side: &'static str,
    #[serde(with = "rust_decimal::serde::str")]
    pub size: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub limit_price: Decimal,
    pub reduce_only: bool,
    pub submitted_at_ms: u64,
    /// Hyperliquid position the IOC was planned against.
    #[serde(with = "rust_decimal::serde::str")]
    pub before_position: Decimal,
    pub status: OrderStatus,
    /// `recorded`, `recorded_legacy` (no terminal/account times) or `intent_only`.
    pub journal: &'static str,
    /// `none`, `lookup_unknown`, `lookup_open`, `lookup_terminal`,
    /// `lookup_agrees`, `matched` or `conflict`.
    pub evidence: &'static str,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub filled: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub position_after: Option<Decimal>,
    /// Bounds on the Hyperliquid position this IOC can have produced;
    /// null when the bound is not representable (the order is then invalid).
    #[serde(with = "rust_decimal::serde::str_option")]
    pub possible_position_min: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub possible_position_max: Option<Decimal>,
    pub primary: PrimaryReport,
    pub reasons: Vec<String>,
}

/// The Lighter observation an IOC was planned against, as journaled.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PrimaryReport {
    /// `journal_claim` (schema 2, consistent), `unavailable` (schema 1 or
    /// legacy: never recorded) or `invalid` (missing or contradictory).
    pub status: &'static str,
    pub observation: Option<PrimaryObservation>,
    /// `known` or `unknown`; an unknown watermark proves nothing either way.
    pub watermark: &'static str,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct JournalSummary {
    pub path: Option<String>,
    pub sha256: Option<String>,
    pub lines: usize,
    /// `v2`, `v1`, `legacy` (no schema on `start`) or `unknown` (no
    /// readable start or an unsupported schema).
    pub schema: &'static str,
    pub session: Option<StartRecord>,
    /// `verified_flat` or `unverified`.
    pub session_cleanup: &'static str,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EvidenceSummary {
    pub path: Option<String>,
    pub sha256: Option<String>,
    pub lookups: usize,
    pub account_snapshot_time_ms: Option<u64>,
    /// False when the evidence names another network, account or coin.
    pub identity_matches: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PositionSummary {
    /// Last position confirmed by a consistent journaled terminal.
    #[serde(with = "rust_decimal::serde::str")]
    pub journal_confirmed: Decimal,
    /// Null when an order's bound is not representable.
    #[serde(with = "rust_decimal::serde::str_option")]
    pub possible_min: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub possible_max: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub snapshot: Option<Decimal>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Report {
    pub mode: &'static str,
    pub status: AuditStatus,
    /// Always false: the audit never releases the session lock.
    pub restart_permitted: bool,
    pub journal: JournalSummary,
    pub evidence: Option<EvidenceSummary>,
    pub hedge_position: PositionSummary,
    pub orders: Vec<OrderReport>,
    /// Caller-declared Lighter orders and account sample, when supplied.
    pub primary_leg: PrimaryLegReport,
    pub issues: Vec<Issue>,
    pub boundaries: Vec<&'static str>,
}

const BOUNDARIES: &[&str] = &[
    "Offline and read-only: no venue request, credential, signature, cancel or order was made.",
    "Journal records are this process's durable claims, not venue confirmations.",
    "Evidence authenticity is outside this audit; it checks consistency only.",
    "Lighter (primary leg) orders and inventory are not journaled; check them on the venue.",
    "Lighter evidence covers only the client order IDs the caller declared, not every order on the account; an absent order proves nothing.",
    "A primary observation is this process's local record of one Lighter read: not venue-signed, not proof of venue state time, and not an order or account ledger.",
    "An unknown or absent lookup never proves an IOC was not accepted or filled.",
    "The active journal and account lock are untouched; this report never permits restart.",
];

/// Read-only audit of files on disk; the inputs are only read.
pub fn audit_files(journal: &Path, evidence: Option<&Path>) -> Result<Report> {
    let journal_bytes =
        std::fs::read(journal).with_context(|| format!("Read {}", journal.display()))?;
    let evidence_input = evidence
        .map(|path| -> Result<(Vec<u8>, Evidence)> {
            let bytes = std::fs::read(path).with_context(|| format!("Read {}", path.display()))?;
            let parsed = serde_json::from_slice(&bytes)
                .with_context(|| format!("Parse evidence {}", path.display()))?;
            Ok((bytes, parsed))
        })
        .transpose()?;
    let mut report = audit(&journal_bytes, evidence_input.as_ref().map(|(_, e)| e));
    report.journal.path = Some(journal.display().to_string());
    report.journal.sha256 = Some(sha256_hex(&journal_bytes));
    if let (Some(summary), Some(path), Some((bytes, _))) =
        (report.evidence.as_mut(), evidence, evidence_input.as_ref())
    {
        summary.path = Some(path.display().to_string());
        summary.sha256 = Some(sha256_hex(bytes));
    }
    Ok(report)
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

// ─── Journal reconstruction ──────────────────────────────────────────────

struct Ioc {
    intent: IntentRecord,
    intent_line: usize,
    primary: PrimaryReport,
    terminal: Option<TerminalRecord>,
    terminal_lines: Vec<usize>,
    invalid: Vec<String>,
}

struct State {
    issues: Vec<Issue>,
    start: Option<StartRecord>,
    schema: &'static str,
    iocs: Vec<Ioc>,
    by_cloid: BTreeMap<String, usize>,
    confirmed: Decimal,
    verified_flat: Option<usize>,
    /// Client order IDs from consistent `lighter_create_batch` records, with
    /// their journal line, in journal order.
    lighter_batches: Vec<(usize, i64)>,
    /// Lines whose `lighter_create_batch` was rejected: its creates are not
    /// journal evidence and the journaled set is incomplete.
    lighter_batch_gaps: Vec<usize>,
    /// Unreadable or torn lines: any of them may have been a batch record,
    /// so the journaled set cannot be shown complete.
    unreadable_lines: Vec<(usize, &'static str)>,
}

impl State {
    fn issue(
        &mut self,
        line: Option<usize>,
        severity: Severity,
        kind: &'static str,
        detail: String,
    ) {
        self.issues.push(Issue {
            line,
            severity,
            kind,
            detail,
        });
    }

    fn pending(&self) -> Option<usize> {
        self.iocs.iter().position(|ioc| ioc.terminal.is_none())
    }

    fn apply(&mut self, line: usize, record: JournalRecord) {
        if matches!(record, JournalRecord::LighterBatch(_))
            && (self.verified_flat.is_some() || self.start.is_none())
        {
            // Rejected below; its creates are not journal evidence.
            self.lighter_batch_gaps.push(line);
        }
        if let Some(flat) = self.verified_flat {
            self.issue(
                Some(line),
                Severity::Invalid,
                "record_after_verified_flat",
                format!("`{}` follows verified_flat on line {flat}", record.event()),
            );
            return;
        }
        match record {
            JournalRecord::Start(start) => self.apply_start(line, start),
            _ if self.start.is_none() => self.issue(
                Some(line),
                Severity::Invalid,
                "record_before_start",
                format!("`{}` precedes the session start record", record.event()),
            ),
            JournalRecord::Intent(intent) => self.apply_intent(line, intent),
            JournalRecord::Terminal(terminal) => self.apply_terminal(line, terminal),
            JournalRecord::LighterBatch(batch) => self.apply_lighter_batch(line, batch),
            JournalRecord::VerifiedFlat => self.apply_verified_flat(line),
        }
    }

    fn apply_start(&mut self, line: usize, start: StartRecord) {
        if self.start.is_some() || line != 1 {
            self.issue(
                Some(line),
                Severity::Invalid,
                "misplaced_start",
                "a session journal has exactly one start record, on line 1".into(),
            );
            return;
        }
        self.schema = match start.journal_schema {
            None => {
                self.issue(
                    Some(line),
                    Severity::Note,
                    "legacy_journal",
                    "start has no journal_schema; terminal and account times are not recorded"
                        .into(),
                );
                "legacy"
            }
            Some(1) => "v1",
            Some(2) => "v2",
            Some(3) => "v3",
            Some(other) => {
                self.issue(
                    Some(line),
                    Severity::Invalid,
                    "unknown_schema",
                    format!("journal_schema {other} is not supported by this audit"),
                );
                "unknown"
            }
        };
        // Identity the supervised writer always emits (session.rs); a legacy
        // session_id is only required to be present, not a UUID.
        let mut invalid = Vec::new();
        if !matches!(start.network.as_str(), "Mainnet" | "Testnet") {
            invalid.push(format!("unknown network `{}`", start.network));
        }
        if start.session_id.trim().is_empty() {
            invalid.push("empty session_id".into());
        }
        if start.lighter_account_index <= 0 {
            // LighterCredentials rejects non-positive account indices.
            invalid.push(format!(
                "lighter_account_index {} is not a valid Lighter account",
                start.lighter_account_index
            ));
        }
        if start.symbol.trim().is_empty() {
            invalid.push("empty symbol".into());
        }
        match (start.journal_schema, start.lighter_market_id) {
            (Some(2 | 3), None) => {
                invalid.push("schema 2 or later start lacks lighter_market_id".into())
            }
            (Some(2 | 3), Some(id)) if id < 0 => {
                invalid.push(format!("lighter_market_id {id} is negative"))
            }
            (Some(2 | 3), Some(_)) | (_, None) => {}
            (_, Some(_)) => invalid.push("lighter_market_id before schema 2".into()),
        }
        for reason in invalid {
            self.issue(Some(line), Severity::Invalid, "invalid_start", reason);
        }
        if let Some(account) = &start.hyperliquid_account
            && validate_hex(account, 20).is_err()
        {
            self.issue(
                Some(line),
                Severity::Invalid,
                "invalid_start",
                "hyperliquid_account is not a 20-byte hex address".into(),
            );
        }
        self.start = Some(start);
    }

    /// Schema 2 intents must carry a primary observation that matches the
    /// session identity and the planned position, with ordered local times
    /// inside the enforced budget. Earlier schemas never carry one.
    fn primary_contract(&self, intent: &IntentRecord) -> (&'static str, Vec<String>) {
        let mut invalid = Vec::new();
        let Some(observation) = &intent.primary_observation else {
            return if binds_lighter(self.schema) {
                (
                    "invalid",
                    vec!["schema 2 intent lacks primary_observation".into()],
                )
            } else {
                ("unavailable", invalid)
            };
        };
        if !binds_lighter(self.schema) {
            invalid.push(format!("primary_observation in a {} journal", self.schema));
        }
        if let Some(start) = &self.start {
            if observation.account_index != start.lighter_account_index {
                invalid.push(format!(
                    "account {} differs from session account {}",
                    observation.account_index, start.lighter_account_index
                ));
            }
            if start
                .lighter_market_id
                .is_some_and(|id| id != observation.market_id)
            {
                invalid.push(format!(
                    "market {} differs from session market {:?}",
                    observation.market_id, start.lighter_market_id
                ));
            }
        }
        if observation.position != intent.primary_position {
            invalid.push(format!(
                "position {} differs from intent primary_position {}",
                observation.position, intent.primary_position
            ));
        }
        if observation.transaction_time == Some(0) {
            invalid.push("transaction_time 0 must be recorded as unknown".into());
        }
        if observation.request_us > observation.age_us {
            invalid.push(format!(
                "request took {} us but the intent formed {} us after its start",
                observation.request_us, observation.age_us
            ));
        }
        if !observation.budget_allowed() {
            invalid.push(format!(
                "budget {} us is not in (0, {MAX_PRIMARY_AGE_US}] us, the live limit",
                observation.max_age_us
            ));
        } else if !observation.within_budget() {
            invalid.push(format!(
                "age {} us is not within the enforced budget {} us",
                observation.age_us, observation.max_age_us
            ));
        }
        if invalid.is_empty() {
            ("journal_claim", invalid)
        } else {
            ("invalid", invalid)
        }
    }

    fn apply_intent(&mut self, line: usize, intent: IntentRecord) {
        if let Some(&index) = self.by_cloid.get(&intent.cloid) {
            if self.iocs[index].intent == intent {
                self.issue(
                    Some(line),
                    Severity::Note,
                    "duplicate_record",
                    format!("identical intent for {} repeated", intent.cloid),
                );
            } else {
                self.iocs[index].invalid.push(format!(
                    "line {line}: conflicting intent for the same cloid"
                ));
                self.issue(
                    Some(line),
                    Severity::Invalid,
                    "conflicting_intent",
                    format!(
                        "intent for {} differs from line {}",
                        intent.cloid, self.iocs[index].intent_line
                    ),
                );
            }
            return;
        }
        let mut invalid = Vec::new();
        if validate_hex(&intent.cloid, 16).is_err() {
            invalid.push("cloid is not a 16-byte hex id".into());
        }
        if let Some(start) = &self.start {
            if intent.coin != start.symbol {
                invalid.push(format!(
                    "coin {} differs from session symbol {}",
                    intent.coin, start.symbol
                ));
            }
            if start.hyperliquid_account.is_none() {
                invalid.push("session start names no Hyperliquid account".into());
            }
        }
        if intent.size <= Decimal::ZERO || intent.limit_price <= Decimal::ZERO {
            invalid.push("non-positive size or limit price".into());
        }
        let (primary_status, primary_reasons) = self.primary_contract(&intent);
        if primary_status == "invalid" {
            invalid.extend(
                primary_reasons
                    .iter()
                    .map(|r| format!("primary observation: {r}")),
            );
        }
        let primary = PrimaryReport {
            status: primary_status,
            observation: intent.primary_observation.clone(),
            watermark: match &intent.primary_observation {
                Some(o) if o.transaction_time.is_some() => "known",
                _ => "unknown",
            },
            reasons: if primary_status == "unavailable" {
                vec![format!(
                    "{} journal: the Lighter observation behind this IOC was not recorded",
                    self.schema
                )]
            } else {
                primary_reasons
            },
        };
        if let Some(open) = self.pending() {
            // The live hedge refuses a second IOC while one is unresolved.
            invalid.push(format!(
                "dispatched while {} (line {}) had no terminal",
                self.iocs[open].intent.cloid, self.iocs[open].intent_line
            ));
        } else if intent.hedge_position != self.confirmed {
            invalid.push(format!(
                "planned against position {} but the journal had confirmed {}",
                intent.hedge_position, self.confirmed
            ));
        }
        for reason in &invalid {
            self.issue(
                Some(line),
                Severity::Invalid,
                "invalid_intent",
                format!("{}: {reason}", intent.cloid),
            );
        }
        self.by_cloid.insert(intent.cloid.clone(), self.iocs.len());
        self.iocs.push(Ioc {
            intent,
            intent_line: line,
            primary,
            terminal: None,
            terminal_lines: Vec::new(),
            invalid,
        });
    }

    fn apply_terminal(&mut self, line: usize, terminal: TerminalRecord) {
        let Some(&index) = self.by_cloid.get(&terminal.cloid) else {
            self.issue(
                Some(line),
                Severity::Invalid,
                "orphan_terminal",
                format!("terminal for {} has no journaled intent", terminal.cloid),
            );
            return;
        };
        self.iocs[index].terminal_lines.push(line);
        if let Some(existing) = &self.iocs[index].terminal {
            if *existing == terminal {
                self.issue(
                    Some(line),
                    Severity::Note,
                    "duplicate_record",
                    format!("identical terminal for {} repeated", terminal.cloid),
                );
            } else {
                self.iocs[index]
                    .invalid
                    .push(format!("line {line}: conflicting terminal"));
                self.issue(
                    Some(line),
                    Severity::Invalid,
                    "conflicting_terminal",
                    format!(
                        "second terminal for {} disagrees with the first",
                        terminal.cloid
                    ),
                );
            }
            return;
        }
        let intent = &self.iocs[index].intent;
        let mut invalid = Vec::new();
        if terminal.filled < Decimal::ZERO || terminal.filled > intent.size {
            invalid.push(format!(
                "filled {} outside 0..={}",
                terminal.filled, intent.size
            ));
        }
        let expected = if intent.is_buy {
            intent.hedge_position.checked_add(terminal.filled)
        } else {
            intent.hedge_position.checked_sub(terminal.filled)
        };
        if expected != Some(terminal.position) {
            invalid.push(format!(
                "position {} is not {} {} {}",
                terminal.position,
                intent.hedge_position,
                if intent.is_buy { "+" } else { "-" },
                terminal.filled
            ));
        }
        if let Some(price) = terminal.average_price {
            let within = if intent.is_buy {
                price <= intent.limit_price
            } else {
                price >= intent.limit_price
            };
            if !within || price <= Decimal::ZERO {
                invalid.push(format!(
                    "average price {price} violates limit {}",
                    intent.limit_price
                ));
            }
        }
        invalid.extend(terminal_time_contract(self.schema, intent, &terminal));
        for reason in &invalid {
            self.issue(
                Some(line),
                Severity::Invalid,
                "invalid_terminal",
                format!("{}: {reason}", terminal.cloid),
            );
        }
        self.confirmed = terminal.position;
        let ioc = &mut self.iocs[index];
        ioc.invalid.extend(invalid);
        ioc.terminal = Some(terminal);
    }

    /// A schema 3 batch record must name this session, its account and
    /// market, and at least one positive client order ID not journaled
    /// before. Any violation drops the whole record from the recorded set.
    fn apply_lighter_batch(&mut self, line: usize, batch: LighterBatchRecord) {
        let mut invalid = Vec::new();
        if self.schema != "v3" {
            invalid.push(format!("lighter_create_batch in a {} journal", self.schema));
        }
        if let Some(start) = &self.start {
            if batch.session_id != start.session_id {
                invalid.push("session_id differs from the journal start".into());
            }
            if batch.network != start.network {
                invalid.push(format!(
                    "network {} vs journal {}",
                    batch.network, start.network
                ));
            }
            if batch.lighter_account_index != start.lighter_account_index {
                invalid.push(format!(
                    "account {} vs journal {}",
                    batch.lighter_account_index, start.lighter_account_index
                ));
            }
            if start.lighter_market_id != Some(batch.lighter_market_id) {
                invalid.push(format!(
                    "market {} vs journal {:?}",
                    batch.lighter_market_id, start.lighter_market_id
                ));
            }
        }
        if batch.client_order_ids.is_empty() {
            invalid.push("a create batch lists no client order ID".into());
        }
        // Journal-supplied counts: checked, so no build can wrap them.
        if batch
            .client_order_ids
            .len()
            .checked_add(batch.cancels)
            .is_none_or(|actions| actions > MAX_BATCH_ACTIONS)
        {
            invalid.push(format!(
                "{} creates and {} cancels exceed the {MAX_BATCH_ACTIONS}-action Lighter batch",
                batch.client_order_ids.len(),
                batch.cancels
            ));
        }
        let mut seen = std::collections::BTreeSet::new();
        for &id in &batch.client_order_ids {
            if id <= 0 {
                invalid.push(format!("client order ID {id} is not positive"));
            } else if !seen.insert(id) || self.lighter_batches.iter().any(|(_, j)| *j == id) {
                invalid.push(format!("client order ID {id} is journaled more than once"));
            }
        }
        if invalid.is_empty() {
            self.lighter_batches
                .extend(batch.client_order_ids.iter().map(|&id| (line, id)));
        } else {
            self.lighter_batch_gaps.push(line);
        }
        for reason in invalid {
            self.issue(
                Some(line),
                Severity::Invalid,
                "invalid_lighter_batch",
                reason,
            );
        }
    }

    fn apply_verified_flat(&mut self, line: usize) {
        if let Some(open) = self.pending() {
            self.issue(
                Some(line),
                Severity::Invalid,
                "flat_with_pending_ioc",
                format!(
                    "verified_flat while {} has no terminal",
                    self.iocs[open].intent.cloid
                ),
            );
        }
        if !self.confirmed.is_zero() {
            self.issue(
                Some(line),
                Severity::Invalid,
                "flat_with_position",
                format!(
                    "verified_flat while the journaled hedge position is {}",
                    self.confirmed
                ),
            );
        }
        self.verified_flat = Some(line);
    }
}

/// Version- and source-specific timing of a journaled terminal, mirroring
/// what the live writer can produce:
/// - `response`: the synchronous order reply carries no venue time; live
///   stamps the terminal with the intent's local `submitted_at_ms`.
/// - `lookup`: venue status time, accepted live when no earlier than
///   `submitted_at_ms - LOOKUP_SKEW_MS`.
///
/// In both cases the confirming account time is at or after the terminal.
/// v1 must carry all three fields; legacy may carry none and is reported as
/// unproven timing elsewhere, but fields it does carry are still checked.
/// Actions one Lighter `sendTxBatch` carries (`execute_batch` enforces it).
const MAX_BATCH_ACTIONS: usize = 50;

/// Schema 2 and later bind the Lighter account and market on `start` and a
/// primary observation on every intent.
fn binds_lighter(schema: &str) -> bool {
    matches!(schema, "v2" | "v3")
}

fn terminal_time_contract(
    schema: &str,
    intent: &IntentRecord,
    terminal: &TerminalRecord,
) -> Vec<String> {
    let mut invalid = Vec::new();
    let fields = (
        terminal.terminal_source.as_deref(),
        terminal.terminal_at_ms,
        terminal.account_time_ms,
    );
    if matches!(schema, "v1" | "v2" | "v3") {
        for (name, missing) in [
            ("terminal_source", fields.0.is_none()),
            ("terminal_at_ms", fields.1.is_none()),
            ("account_time_ms", fields.2.is_none()),
        ] {
            if missing {
                invalid.push(format!("v1 terminal lacks {name}"));
            }
        }
    }
    match (fields.0, fields.1) {
        (Some("response"), Some(at)) if at != intent.submitted_at_ms => invalid.push(format!(
            "response terminal time {at} is not the intent time {}",
            intent.submitted_at_ms
        )),
        (Some("lookup"), Some(at))
            if at < intent.submitted_at_ms.saturating_sub(LOOKUP_SKEW_MS) =>
        {
            invalid.push(format!(
                "lookup terminal time {at} is more than {LOOKUP_SKEW_MS} ms before the intent time {}",
                intent.submitted_at_ms
            ))
        }
        (Some("response" | "lookup"), _) => {}
        (Some(source), _) => invalid.push(format!("unknown terminal_source `{source}`")),
        (None, Some(_)) => invalid.push("terminal_at_ms without terminal_source".into()),
        (None, None) => {}
    }
    if let (Some(at), Some(account)) = (fields.1, fields.2)
        && account < at
    {
        invalid.push(format!(
            "account time {account} precedes terminal time {at}"
        ));
    }
    if fields.2.is_some() && fields.1.is_none() {
        invalid.push("account_time_ms without terminal_at_ms".into());
    }
    invalid
}

// ─── Frozen evidence ─────────────────────────────────────────────────────

struct Snapshot {
    time_ms: u64,
    position: Decimal,
}

fn load_snapshot(evidence: &SnapshotEvidence, coin: &str) -> Result<Snapshot> {
    let state: AccountState = serde_json::from_value(evidence.state.clone())
        .context("account_snapshot.state is not a clearinghouseState response")?;
    anyhow::ensure!(
        state.time > 0 && state.time <= evidence.observed_at_ms.saturating_add(CLOCK_TOLERANCE_MS),
        "account snapshot time {} is after its capture time {}",
        state.time,
        evidence.observed_at_ms
    );
    Ok(Snapshot {
        time_ms: state.time,
        position: dedicated_position(&state, coin)?,
    })
}

/// What the frozen lookups for one IOC show, by the live identity rules.
#[derive(Debug, Clone, Copy, PartialEq)]
enum LookupView {
    None,
    /// Only `unknownOid` responses: proves nothing about acceptance.
    Unknown,
    /// Latest non-terminal view is resting at the venue.
    Open,
    Terminal {
        at_ms: u64,
        filled: Decimal,
    },
}

/// Classify every frozen lookup for `intent`. Identity mismatches,
/// disagreeing terminals or an `open` captured after a terminal are errors.
fn evaluate_lookups(
    intent: &IntentRecord,
    lookups: &[&LookupEvidence],
) -> std::result::Result<LookupView, String> {
    let identity = IocIdentity {
        cloid: &intent.cloid,
        coin: &intent.coin,
        is_buy: intent.is_buy,
        size: intent.size,
        limit_price: intent.limit_price,
        sent_at_ms: intent.submitted_at_ms,
    };
    let mut terminal: Option<(u64, u64, Decimal)> = None; // (captured, status at, filled)
    let mut last_open: Option<u64> = None;
    let mut unknown = false;
    for lookup in lookups {
        let parsed = OrderLookup::from_response(lookup.response.clone())
            .map_err(|e| format!("lookup at {}: {e:#}", lookup.observed_at_ms))?;
        let located = match parsed {
            OrderLookup::Unknown => {
                unknown = true;
                continue;
            }
            OrderLookup::Found(located) => located,
        };
        if located.status_timestamp > lookup.observed_at_ms.saturating_add(CLOCK_TOLERANCE_MS) {
            return Err(format!(
                "lookup status time {} is after its capture time {}",
                located.status_timestamp, lookup.observed_at_ms
            ));
        }
        match classify_lookup(&identity, &located)
            .map_err(|e| format!("lookup at {}: {e:#}", lookup.observed_at_ms))?
        {
            LookupOutcome::Terminal { at_ms, filled } => {
                if let Some((_, _, seen)) = terminal
                    && seen != filled
                {
                    return Err(format!(
                        "terminal lookups disagree: filled {seen} vs {filled}"
                    ));
                }
                if terminal.is_none_or(|(captured, _, _)| lookup.observed_at_ms > captured) {
                    terminal = Some((lookup.observed_at_ms, at_ms, filled));
                }
            }
            LookupOutcome::Open { .. } => {
                last_open = last_open.max(Some(lookup.observed_at_ms));
            }
        }
    }
    if let (Some(open_at), Some((terminal_captured, _, _))) = (last_open, terminal)
        && open_at > terminal_captured
    {
        return Err("an open lookup was captured after a terminal one".into());
    }
    Ok(match (terminal, last_open, unknown) {
        (Some((_, at_ms, filled)), _, _) => LookupView::Terminal { at_ms, filled },
        (None, Some(_), _) => LookupView::Open,
        (None, None, true) => LookupView::Unknown,
        (None, None, false) => LookupView::None,
    })
}

// ─── Report ──────────────────────────────────────────────────────────────

/// Audit journal bytes against optional frozen evidence. Pure: no I/O.
pub fn audit(journal: &[u8], evidence: Option<&Evidence>) -> Report {
    let mut state = State {
        issues: Vec::new(),
        start: None,
        schema: "unknown",
        iocs: Vec::new(),
        by_cloid: BTreeMap::new(),
        confirmed: Decimal::ZERO,
        verified_flat: None,
        lighter_batches: Vec::new(),
        lighter_batch_gaps: Vec::new(),
        unreadable_lines: Vec::new(),
    };
    let lines = read_lines(journal, &mut state);
    if state.start.is_none() {
        state.issue(
            None,
            Severity::Invalid,
            "missing_start",
            "no readable session start record on line 1".into(),
        );
    }
    if state.verified_flat.is_none() {
        // Reported, not folded into `status`: the journal cannot prove the
        // primary leg either way, and `restart_permitted` stays false.
        state.issue(
            None,
            Severity::Note,
            "primary_leg_unverified",
            "session ended without verified_flat; Lighter orders and inventory are unproven".into(),
        );
    }
    let evidence =
        evidence.map(|evidence| (evidence, check_evidence_identity(&mut state, evidence)));
    let snapshot = evidence.and_then(|(evidence, usable)| {
        let raw = evidence.account_snapshot.as_ref().filter(|_| usable)?;
        match load_snapshot(raw, &evidence.coin) {
            Ok(snapshot) => Some(snapshot),
            Err(error) => {
                state.issue(
                    None,
                    Severity::Invalid,
                    "invalid_snapshot",
                    format!("{error:#}"),
                );
                None
            }
        }
    });
    let usable_lookups: Vec<&LookupEvidence> = evidence
        .filter(|(_, usable)| *usable)
        .map(|(evidence, _)| evidence.lookups.iter().collect())
        .unwrap_or_default();
    for lookup in &usable_lookups {
        if !state.by_cloid.contains_key(&lookup.cloid) {
            state.issue(
                None,
                Severity::Invalid,
                "evidence_unknown_cloid",
                format!("lookup for {} matches no journaled intent", lookup.cloid),
            );
        }
    }

    let last_index = state.iocs.len().checked_sub(1);
    let mut orders = Vec::with_capacity(state.iocs.len());
    let mut position = PositionSummary {
        journal_confirmed: state.confirmed,
        possible_min: Some(state.confirmed),
        possible_max: Some(state.confirmed),
        snapshot: snapshot.as_ref().map(|s| s.position),
    };
    for (index, ioc) in state.iocs.iter().enumerate() {
        let lookups: Vec<&LookupEvidence> = usable_lookups
            .iter()
            .copied()
            .filter(|l| l.cloid == ioc.intent.cloid)
            .collect();
        let view = evaluate_lookups(&ioc.intent, &lookups);
        let order = order_report(
            ioc,
            view,
            snapshot.as_ref().filter(|_| Some(index) == last_index),
        );
        let unbounded = order.possible_position_min.is_none();
        if unbounded || order.status == OrderStatus::Pending || order.status == OrderStatus::Matched
        {
            position.possible_min = order.possible_position_min;
            position.possible_max = order.possible_position_max;
        }
        orders.push(order);
    }
    if let Some(snapshot) = &snapshot
        && orders.iter().all(|o| o.status == OrderStatus::Recorded)
    {
        check_snapshot_against_journal(&mut state, snapshot);
    }
    // Lighter findings only add issues; Hyperliquid order results stay as is.
    let journaled = std::mem::take(&mut state.lighter_batches);
    let gaps: Vec<lighter::JournalGap> = state
        .lighter_batch_gaps
        .iter()
        .map(|&line| lighter::JournalGap {
            line,
            kind: "invalid_lighter_batch",
        })
        .chain(
            state
                .unreadable_lines
                .iter()
                .map(|&(line, kind)| lighter::JournalGap { line, kind }),
        )
        .collect();
    let primary_leg = match evidence.and_then(|(e, usable)| Some((e.lighter.as_ref()?, usable))) {
        None if journaled.is_empty() => PrimaryLegReport::not_provided(),
        None => {
            // Journaled creates with no venue evidence: each one is open.
            state.issue(
                None,
                Severity::Unresolved,
                "lighter_missing_evidence",
                format!(
                    "{} journaled Lighter create(s) have no order history or account sample",
                    journaled.len()
                ),
            );
            PrimaryLegReport::journal_only(&journaled)
        }
        Some((lighter, usable)) => {
            let bound = state.start.as_ref().filter(|_| binds_lighter(state.schema));
            let (report, findings) = lighter::evaluate(bound, usable, lighter, &journaled);
            for f in findings {
                let severity = if f.invalid {
                    Severity::Invalid
                } else {
                    Severity::Unresolved
                };
                state.issue(None, severity, f.kind, f.detail);
            }
            report
        }
    };
    let mut primary_leg = primary_leg;
    // Only schema 3 records batches; older journals keep their verdict.
    let v3 = state.schema == "v3";
    let gaps = if v3 { gaps } else { Vec::new() };
    if lighter::apply_journal_gaps(&mut primary_leg, v3, &gaps) {
        // Rejected or unreadable lines already carry their own issue.
        debug_assert!(state.issues.iter().any(|i| i.severity == Severity::Invalid));
    }
    if !gaps.is_empty() {
        state.issue(
            None,
            Severity::Unresolved,
            "lighter_journal_prefix",
            format!(
                "journal lines {:?} may hold Lighter creates not listed; primary_leg covers a prefix only",
                primary_leg.journal_gaps
            ),
        );
    }

    let worst = state.issues.iter().map(|i| i.severity).max();
    let status = if worst == Some(Severity::Invalid)
        || orders.iter().any(|o| o.status == OrderStatus::Invalid)
    {
        AuditStatus::Invalid
    } else if worst == Some(Severity::Unresolved)
        || orders.iter().any(|o| o.status == OrderStatus::Pending)
    {
        AuditStatus::Unresolved
    } else if orders.iter().any(|o| o.status == OrderStatus::Matched) {
        AuditStatus::EvidenceMatched
    } else {
        AuditStatus::JournalRecorded
    };
    Report {
        mode: "offline-read-only",
        status,
        restart_permitted: false,
        journal: JournalSummary {
            path: None,
            sha256: None,
            lines,
            schema: state.schema,
            session: state.start.clone(),
            session_cleanup: if state.verified_flat.is_some() {
                "verified_flat"
            } else {
                "unverified"
            },
        },
        evidence: evidence.map(|(evidence, usable)| EvidenceSummary {
            path: None,
            sha256: None,
            lookups: evidence.lookups.len(),
            account_snapshot_time_ms: snapshot.as_ref().map(|s| s.time_ms),
            identity_matches: usable,
        }),
        hedge_position: position,
        orders,
        primary_leg,
        issues: state.issues,
        boundaries: BOUNDARIES.to_vec(),
    }
}

/// Split into lines, keeping 1-based numbers. A final line without a newline
/// was cut mid-write: its `write_all` never returned, so the live process did
/// not act on it. An intent there is kept as possible exposure (conservative);
/// any other partial record is ignored, which keeps the IOC pending.
fn read_lines(journal: &[u8], state: &mut State) -> usize {
    let mut count = 0;
    let mut rest = journal;
    while !rest.is_empty() {
        count += 1;
        let (line, complete) = match rest.iter().position(|b| *b == b'\n') {
            Some(end) => {
                let line = &rest[..end];
                rest = &rest[end + 1..];
                (line, true)
            }
            None => {
                let line = rest;
                rest = &[];
                (line, false)
            }
        };
        match (JournalRecord::parse(line), complete) {
            (Ok(record), true) => state.apply(count, record),
            (Ok(JournalRecord::Intent(intent)), false) => {
                state.issue(
                    Some(count),
                    Severity::Unresolved,
                    "truncated_tail",
                    format!(
                        "intent for {} lacks its newline; not durably written, kept as possible exposure",
                        intent.cloid
                    ),
                );
                state.apply(count, JournalRecord::Intent(intent));
            }
            (parsed, false) => {
                state.unreadable_lines.push((count, "truncated_tail"));
                state.issue(
                    Some(count),
                    Severity::Unresolved,
                    "truncated_tail",
                    match parsed {
                        Ok(record) => format!(
                            "`{}` lacks its newline; treated as not written",
                            record.event()
                        ),
                        Err(error) => format!("partial final line ignored ({error})"),
                    },
                )
            }
            (Err(error), true) => {
                let kind = match error {
                    ParseError::UnknownEvent(_) => "unknown_event",
                    _ => "malformed_line",
                };
                state.unreadable_lines.push((count, kind));
                state.issue(Some(count), Severity::Invalid, kind, error.to_string());
            }
        }
    }
    count
}

fn check_evidence_identity(state: &mut State, evidence: &Evidence) -> bool {
    let Some(start) = &state.start else {
        return false;
    };
    let mut mismatches = Vec::new();
    if evidence.network != start.network {
        mismatches.push(format!(
            "network {} vs journal {}",
            evidence.network, start.network
        ));
    }
    let account = start.hyperliquid_account.as_deref().unwrap_or_default();
    if !evidence.hyperliquid_account.eq_ignore_ascii_case(account) {
        mismatches.push("hyperliquid_account differs from the journal".into());
    }
    if evidence.coin != start.symbol {
        mismatches.push(format!(
            "coin {} vs journal symbol {}",
            evidence.coin, start.symbol
        ));
    }
    for mismatch in &mismatches {
        state.issue(
            None,
            Severity::Invalid,
            "evidence_identity",
            mismatch.clone(),
        );
    }
    mismatches.is_empty()
}

/// With every IOC recorded, a later snapshot must still show the journaled
/// position; otherwise the account changed outside this journal.
fn check_snapshot_against_journal(state: &mut State, snapshot: &Snapshot) {
    let last_terminal_ms = state
        .iocs
        .iter()
        .filter_map(|ioc| ioc.terminal.as_ref()?.account_time_ms)
        .max();
    if last_terminal_ms.is_some_and(|at| snapshot.time_ms < at) {
        state.issue(
            None,
            Severity::Unresolved,
            "stale_snapshot",
            format!(
                "snapshot time {} precedes the last journaled confirmation",
                snapshot.time_ms
            ),
        );
    } else if snapshot.position != state.confirmed {
        state.issue(
            None,
            Severity::Invalid,
            "unexplained_position",
            format!(
                "snapshot position {} differs from journaled {}",
                snapshot.position, state.confirmed
            ),
        );
    }
}

/// Position after `filled` from `before`; None when not representable.
fn signed(is_buy: bool, before: Decimal, filled: Decimal) -> Option<Decimal> {
    if is_buy {
        before.checked_add(filled)
    } else {
        before.checked_sub(filled)
    }
}

fn order_report(
    ioc: &Ioc,
    view: std::result::Result<LookupView, String>,
    snapshot: Option<&Snapshot>,
) -> OrderReport {
    let intent = &ioc.intent;
    // Known fill: a point. Unknown: anywhere between no fill and full size.
    let range = |filled: Option<Decimal>| -> Option<(Decimal, Decimal)> {
        let end = signed(
            intent.is_buy,
            intent.hedge_position,
            filled.unwrap_or(intent.size),
        )?;
        let start = if filled.is_some() {
            end
        } else {
            intent.hedge_position
        };
        Some((start.min(end), start.max(end)))
    };
    let overflow = || {
        format!(
            "position range from {} {} {} exceeds the representable decimal range",
            intent.hedge_position,
            if intent.is_buy { "+" } else { "-" },
            intent.size
        )
    };
    let mut reasons = ioc.invalid.clone();
    let mut report = OrderReport {
        cloid: intent.cloid.clone(),
        intent_line: ioc.intent_line,
        terminal_lines: ioc.terminal_lines.clone(),
        side: if intent.is_buy { "buy" } else { "sell" },
        size: intent.size,
        limit_price: intent.limit_price,
        reduce_only: intent.reduce_only,
        submitted_at_ms: intent.submitted_at_ms,
        before_position: intent.hedge_position,
        status: OrderStatus::Pending,
        journal: "intent_only",
        evidence: "none",
        filled: None,
        position_after: None,
        possible_position_min: None,
        possible_position_max: None,
        primary: ioc.primary.clone(),
        reasons: Vec::new(),
    };
    let view = match view {
        Ok(view) => view,
        Err(error) => {
            reasons.push(error);
            report.evidence = "conflict";
            LookupView::None
        }
    };
    if let Some(terminal) = &ioc.terminal {
        report.journal = if terminal.account_time_ms.is_some() {
            "recorded"
        } else {
            "recorded_legacy"
        };
        report.filled = Some(terminal.filled);
        report.position_after = Some(terminal.position);
        // Evidence about a recorded IOC may corroborate or contradict it.
        if let LookupView::Terminal { filled, .. } = view {
            if filled == terminal.filled {
                report.evidence = "lookup_agrees";
            } else {
                report.evidence = "conflict";
                reasons.push(format!(
                    "venue lookup shows filled {filled}, journal {}",
                    terminal.filled
                ));
            }
        } else if view == LookupView::Open {
            report.evidence = "conflict";
            reasons.push("journal records a terminal but the venue shows the IOC open".into());
        }
        let bounds = range(Some(terminal.filled));
        if bounds.is_none() {
            reasons.push(overflow());
        }
        report.status = if reasons.is_empty() {
            OrderStatus::Recorded
        } else {
            OrderStatus::Invalid
        };
        report.possible_position_min = bounds.map(|b| b.0);
        report.possible_position_max = bounds.map(|b| b.1);
        report.reasons = reasons;
        return report;
    }

    // No journaled terminal: the IOC may have reached the venue.
    report.evidence = match (report.evidence, view) {
        ("conflict", _) => "conflict",
        (_, LookupView::None) => "none",
        (_, LookupView::Unknown) => "lookup_unknown",
        (_, LookupView::Open) => "lookup_open",
        (_, LookupView::Terminal { .. }) => "lookup_terminal",
    };
    match view {
        LookupView::None => reasons.push("no venue lookup supplied for this cloid".into()),
        LookupView::Unknown => reasons.push(
            "lookup returned unknownOid; this does not prove the IOC was never accepted".into(),
        ),
        LookupView::Open => reasons.push(
            "venue shows the IOC open; it must reach a terminal state at the venue".into(),
        ),
        LookupView::Terminal { at_ms, filled } => match snapshot {
            None => reasons.push(format!(
                "venue terminal filled {filled}; supply an account snapshot taken at or after {at_ms}"
            )),
            Some(snapshot) => match account_confirmed_fill(
                intent.is_buy,
                intent.hedge_position,
                intent.size,
                at_ms,
                filled,
                snapshot.position,
                snapshot.time_ms,
            ) {
                Err(error) => {
                    report.evidence = "conflict";
                    reasons.push(format!("{error:#}"));
                }
                Ok(None) if snapshot.time_ms < at_ms => reasons.push(format!(
                    "account snapshot at {} precedes the terminal at {at_ms}",
                    snapshot.time_ms
                )),
                // Same rule as live `observe`: an unchanged position keeps
                // waiting for the account to catch up; a changed position
                // that is not the terminal fill is unexplained.
                Ok(None) if snapshot.position == intent.hedge_position => reasons.push(format!(
                    "account snapshot at {} does not yet reflect the terminal fill {filled}",
                    snapshot.time_ms
                )),
                Ok(None) => {
                    report.evidence = "conflict";
                    reasons.push(format!(
                        "snapshot position {} does not equal {} {} {filled}",
                        snapshot.position,
                        intent.hedge_position,
                        if intent.is_buy { "+" } else { "-" }
                    ));
                }
                Ok(Some(fill)) => {
                    report.evidence = "matched";
                    report.filled = Some(fill);
                    report.position_after = Some(snapshot.position);
                }
            },
        },
    }
    let bounds = range(if report.evidence == "matched" {
        report.filled
    } else {
        None
    });
    if bounds.is_none() {
        reasons.push(overflow());
    }
    let invalid = !ioc.invalid.is_empty() || report.evidence == "conflict" || bounds.is_none();
    report.status = if invalid {
        OrderStatus::Invalid
    } else if report.evidence == "matched" {
        OrderStatus::Matched
    } else {
        OrderStatus::Pending
    };
    report.possible_position_min = bounds.map(|b| b.0);
    report.possible_position_max = bounds.map(|b| b.1);
    report.reasons = reasons;
    report
}

#[cfg(test)]
mod tests;
