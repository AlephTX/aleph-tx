//! Offline, read-only cost evidence for one reconciled Hyperliquid IOC.
//!
//! Sets two records of the same IOC side by side without merging them: the
//! strategy telemetry `hedge_fill` line (a configured-rate fee estimate,
//! priced at the venue average or the IOC limit) and the hedge-journal
//! audit's attribution of frozen venue fills (an exact fee per fee asset).
//!
//! Records are bound by identity only. The telemetry `session` record must
//! name the journal's session, network, both accounts, market and symbol,
//! and the IOC is selected by its exact CLOID. The live writer exports a
//! `hedge_fill` only after the journal durably records that IOC's positive
//! terminal (`LiveInventoryHedge::observe`), so a telemetry fill without
//! one, a second session record or a record after `stopped` means the files
//! do not belong together. File names, times and sizes never link records.
//! The two fees are never subtracted: the estimate names no asset.

use super::MAX_TAKER_FEE_BPS;
use super::audit::{self, AuditStatus, Evidence, FeeReport, OrderReport, OrderStatus, Severity};
use super::journal::{JournalRecord, StartRecord, TerminalRecord};
use crate::exchanges::hyperliquid::validate_hex;
use anyhow::{Context, Result};
use rust_decimal::{Decimal, prelude::ToPrimitive};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewStatus {
    /// Both records are bound to this IOC and agree with the journal. This
    /// does not say the fees agree; see `comparison`.
    Bound,
    /// A record is missing, incomplete or not reconciled yet.
    Unresolved,
    /// The inputs contradict each other or name another session or order.
    Invalid,
}

impl ViewStatus {
    /// Same convention as the hedge-journal audit: 0, 2 or 3.
    pub fn exit_code(self) -> i32 {
        match self {
            Self::Bound => 0,
            Self::Unresolved => 2,
            Self::Invalid => 3,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InputDigest {
    pub path: String,
    pub sha256: String,
    pub bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Inputs {
    pub journal: InputDigest,
    pub evidence: InputDigest,
    pub telemetry: InputDigest,
}

/// The journaled IOC as the audit reconstructed it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct IocView {
    /// The audit's verdict for this IOC.
    pub status: OrderStatus,
    pub intent_line: usize,
    pub terminal_lines: Vec<usize>,
    pub side: &'static str,
    #[serde(with = "rust_decimal::serde::str")]
    pub size: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub limit_price: Decimal,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub filled: Option<Decimal>,
    /// Venue average price on the journaled terminal; null when the
    /// terminal came from an order lookup, which reports none.
    #[serde(with = "rust_decimal::serde::str_option")]
    pub average_price: Option<Decimal>,
    pub terminal_source: Option<String>,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub position_after: Option<Decimal>,
    pub reasons: Vec<String>,
}

/// The telemetry `hedge_fill` for this IOC. Numbers are the JSON tokens the
/// writer emitted, unchanged.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TelemetryView {
    /// Whether the telemetry side binds; its reasons are in `FeeView::reasons`.
    pub status: ViewStatus,
    pub session_line: Option<usize>,
    pub stopped_line: Option<usize>,
    /// Lines holding this CLOID's `hedge_fill`.
    pub lines: Vec<usize>,
    pub side: Option<String>,
    pub size: Option<String>,
    pub price: Option<String>,
    /// `fill` (venue average price) or `limit` (the IOC limit price).
    pub price_source: Option<String>,
    pub fee: Option<String>,
    /// `configured_rate`, or `unknown` when the writer recorded none.
    pub fee_source: Option<String>,
    pub position_after: Option<String>,
    /// Facts that do not block binding, such as a byte-identical repeat.
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Comparison {
    /// Always `not_comparable`: no difference is computed. See `reasons`.
    pub status: &'static str,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FeeView {
    pub mode: &'static str,
    pub status: ViewStatus,
    pub cloid: String,
    /// Paths and hashes of the files read; null for in-memory inputs.
    pub inputs: Option<Inputs>,
    pub session: Option<StartRecord>,
    /// The hedge-journal audit's verdict over the whole journal and evidence.
    pub audit_status: AuditStatus,
    pub ioc: Option<IocView>,
    pub telemetry: TelemetryView,
    /// The audit's attribution of frozen venue fills to this IOC.
    pub venue_fee: Option<FeeReport>,
    pub comparison: Comparison,
    /// Why the view is not `bound`.
    pub reasons: Vec<String>,
    pub boundaries: Vec<&'static str>,
}

const BOUNDARIES: &[&str] = &[
    "Offline and read-only: only the three files are read; no venue request, credential, signature or order was made.",
    "Records are bound by session identity and the exact CLOID, never by file name, time or size.",
    "The telemetry fee is size times price times a configured rate. It is an estimate, names no asset and is not the amount charged.",
    "The venue fee is the frozen fills' declaration per fee asset as the hedge-journal audit attributes it; its authenticity is outside this view.",
    "The fees are never subtracted or converted, and a missing fee is never read as zero.",
    "One Hyperliquid IOC only: Lighter fills and cross-leg causality are not covered.",
];

// ─── Telemetry ───────────────────────────────────────────────────────────

/// The record type of one line; other fields are not read here.
#[derive(Deserialize)]
struct Kind<'a> {
    #[serde(rename = "type", borrow)]
    kind: Option<&'a str>,
}

/// The `session` record exactly as `TelemetrySink::open` writes it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionLine {
    #[serde(rename = "type")]
    _kind: String,
    #[serde(rename = "ts_ms")]
    _ts_ms: u64,
    session_id: String,
    network: String,
    exchange: String,
    symbol: String,
    market_id: u64,
    account: i64,
    hedge_account: Option<String>,
}

/// A `hedge_fill` record. Numbers stay raw tokens so the view reports what
/// the writer emitted and compares it exactly.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HedgeFillLine<'a> {
    #[serde(rename = "type")]
    _kind: &'a str,
    ts_ms: u64,
    venue: String,
    client_order_id: String,
    side: String,
    #[serde(borrow)]
    size: &'a RawValue,
    #[serde(borrow)]
    price: &'a RawValue,
    price_source: String,
    #[serde(borrow)]
    fee: &'a RawValue,
    /// Absent from writers older than the label.
    #[serde(default)]
    fee_source: Option<String>,
    #[serde(borrow)]
    position_after: &'a RawValue,
}

#[derive(Debug, Clone, PartialEq)]
struct Fill {
    line: usize,
    /// Writer time; only used to tell a repeated line from a second write.
    ts_ms: u64,
    venue: String,
    side: String,
    size: String,
    price: String,
    price_source: String,
    fee: String,
    fee_source: Option<String>,
    position_after: String,
}

/// One CLOID's `hedge_fill` records.
#[derive(Default)]
struct Records {
    /// Well-formed records, in file order.
    fills: Vec<Fill>,
    /// This CLOID's records with a bad number: line and reason.
    faults: Vec<(usize, String)>,
}

/// What one telemetry file says about its session and every CLOID.
struct Telemetry {
    session: Option<SessionLine>,
    stopped: Option<usize>,
    /// Contradictions in the file as a whole: line (0 for the file) and
    /// reason, in file order.
    faults: Vec<(usize, String)>,
    /// Records by exact CLOID.
    by_cloid: BTreeMap<String, Records>,
    /// Line number of a final line without its newline.
    torn: Option<usize>,
}

impl Telemetry {
    fn fault(&mut self, line: usize, reason: String) {
        self.faults.push((line, reason));
    }

    /// File one parsed `hedge_fill` under its exact CLOID. A bad number is
    /// that CLOID's fault only, as it was when a view read one CLOID.
    fn record(&mut self, line: usize, fill: &HedgeFillLine<'_>) {
        let records = self
            .by_cloid
            .entry(fill.client_order_id.clone())
            .or_default();
        match own_fill(line, fill) {
            Ok(own) => records.fills.push(own),
            Err(reason) => records
                .faults
                .push((line, format!("line {line}: {reason}"))),
        }
    }
}

/// A finite JSON number token, unchanged; strings, null and the like fail.
fn number(raw: &RawValue) -> Option<String> {
    let text = raw.get();
    let numeric = text
        .bytes()
        .all(|b| b.is_ascii_digit() || matches!(b, b'-' | b'+' | b'.' | b'e' | b'E'));
    (numeric && text.parse::<f64>().is_ok_and(f64::is_finite)).then(|| text.to_owned())
}

fn read_telemetry(bytes: &[u8]) -> Telemetry {
    let mut out = Telemetry {
        session: None,
        stopped: None,
        faults: Vec::new(),
        by_cloid: BTreeMap::new(),
        torn: None,
    };
    let mut rest = bytes;
    let mut line_no = 0;
    while !rest.is_empty() {
        line_no += 1;
        let Some(end) = rest.iter().position(|b| *b == b'\n') else {
            // Cut mid-record: it may have been any record, including any
            // CLOID's, so the file cannot show a complete set.
            out.torn = Some(line_no);
            break;
        };
        let line = &rest[..end];
        rest = &rest[end + 1..];
        read_line(&mut out, line_no, line);
    }
    if line_no == 0 {
        out.fault(0, "telemetry file is empty".into());
    }
    out
}

fn read_line(out: &mut Telemetry, line_no: usize, line: &[u8]) {
    let kind = match serde_json::from_slice::<Kind<'_>>(line) {
        Ok(Kind { kind: Some(kind) }) => kind.to_owned(),
        Ok(Kind { kind: None }) => {
            out.fault(line_no, format!("line {line_no}: no record type"));
            return;
        }
        Err(error) => {
            out.fault(
                line_no,
                format!("line {line_no}: not a JSON record ({error})"),
            );
            return;
        }
    };
    if let Some(stopped) = out.stopped {
        out.fault(
            line_no,
            format!("line {line_no}: `{kind}` follows the stopped marker on line {stopped}"),
        );
        return;
    }
    if kind == "session" {
        if line_no != 1 {
            out.fault(
                line_no,
                format!("line {line_no}: a second session record; one file holds one session"),
            );
            return;
        }
        match serde_json::from_slice::<SessionLine>(line) {
            Ok(session) => out.session = Some(session),
            Err(error) => out.fault(1, format!("line 1: malformed session record ({error})")),
        }
        return;
    }
    if line_no == 1 {
        out.fault(1, format!("line 1 is `{kind}`, not the session record"));
        return;
    }
    match kind.as_str() {
        "stopped" => out.stopped = Some(line_no),
        "hedge_fill" => match serde_json::from_slice::<HedgeFillLine<'_>>(line) {
            Ok(fill) => out.record(line_no, &fill),
            // Unparsed, so its CLOID is unknown: a fault for every view.
            Err(error) => out.fault(
                line_no,
                format!("line {line_no}: malformed hedge_fill ({error})"),
            ),
        },
        _ => {}
    }
}

fn own_fill(line: usize, f: &HedgeFillLine<'_>) -> Result<Fill, String> {
    let field = |name: &str, value: &RawValue| {
        number(value).ok_or_else(|| format!("{name} `{}` is not a finite number", value.get()))
    };
    Ok(Fill {
        line,
        ts_ms: f.ts_ms,
        venue: f.venue.clone(),
        side: f.side.clone(),
        size: field("size", f.size)?,
        price: field("price", f.price)?,
        price_source: f.price_source.clone(),
        fee: field("fee", f.fee)?,
        fee_source: f.fee_source.clone(),
        position_after: field("position_after", f.position_after)?,
    })
}

// ─── Binding ─────────────────────────────────────────────────────────────

/// The telemetry session must be the journal's: the supervised launcher
/// writes both from the same values (`session.rs`).
fn check_session(start: &StartRecord, session: &SessionLine, reasons: &mut Vec<String>) {
    let mut differ = |field: &str, journal: String, telemetry: String| {
        if journal != telemetry {
            reasons.push(format!(
                "telemetry {field} `{telemetry}` differs from the journal's `{journal}`"
            ));
        }
    };
    differ(
        "session_id",
        start.session_id.clone(),
        session.session_id.clone(),
    );
    differ("network", start.network.clone(), session.network.clone());
    differ("exchange", "lighter".into(), session.exchange.clone());
    differ("symbol", start.symbol.clone(), session.symbol.clone());
    differ(
        "account",
        start.lighter_account_index.to_string(),
        session.account.to_string(),
    );
    // Schema 1 and legacy journals never recorded the market; the caller
    // reports that as unverifiable, not as a contradiction.
    if let Some(market) = start.lighter_market_id {
        differ(
            "market_id",
            market.to_string(),
            session.market_id.to_string(),
        );
    }
    match (&start.hyperliquid_account, &session.hedge_account) {
        (Some(journal), Some(telemetry)) if journal.eq_ignore_ascii_case(telemetry) => {}
        (journal, telemetry) => reasons.push(format!(
            "telemetry hedge_account {telemetry:?} differs from the journal's {journal:?}"
        )),
    }
}

/// The journal split once at its newlines, so each IOC's terminal is read
/// back without rescanning the file.
fn journal_lines(journal: &[u8]) -> Vec<&[u8]> {
    journal.split(|b| *b == b'\n').collect()
}

/// The terminal the audit accepted for this IOC, read from its first
/// terminal line (the report omits the average price and source). Lines are
/// numbered as the audit numbers them, and it lists only complete lines.
fn terminal_at(lines: &[&[u8]], line: usize) -> Option<TerminalRecord> {
    let raw = lines.get(line.checked_sub(1)?)?;
    match JournalRecord::parse(raw).ok()? {
        JournalRecord::Terminal(terminal) => Some(terminal),
        _ => None,
    }
}

fn exact(raw: &str) -> Option<f64> {
    raw.parse::<f64>().ok()
}

/// Check one telemetry fill against the journal, as `hedge_fill_record`
/// derives it from the terminal: size is the fill, price the venue average
/// (`fill`) or the IOC limit (`limit`), position the terminal's.
fn check_fill(
    fill: &Fill,
    order: &OrderReport,
    terminal: &TerminalRecord,
    reasons: &mut Vec<String>,
) {
    let mut wrong = |what: String| reasons.push(format!("line {}: {what}", fill.line));
    if fill.venue != "hyperliquid" {
        wrong(format!("venue `{}` is not hyperliquid", fill.venue));
    }
    if fill.side != order.side {
        wrong(format!(
            "side {} but the IOC is a {}",
            fill.side, order.side
        ));
    }
    let journal_f64 = |d: Decimal| d.to_f64();
    let same = |raw: &str, d: Decimal| exact(raw).is_some() && exact(raw) == journal_f64(d);
    if !same(&fill.size, terminal.filled) {
        wrong(format!(
            "size {} is not the journaled fill {}",
            fill.size, terminal.filled
        ));
    }
    if !same(&fill.position_after, terminal.position) {
        wrong(format!(
            "position_after {} is not the journaled position {}",
            fill.position_after, terminal.position
        ));
    }
    let (expected_source, expected_price) = match terminal.average_price {
        Some(price) => ("fill", price),
        None => ("limit", order.limit_price),
    };
    if fill.price_source != expected_source {
        wrong(format!(
            "price_source `{}` but the journaled terminal implies `{expected_source}`",
            fill.price_source
        ));
    } else if !same(&fill.price, expected_price) {
        wrong(format!(
            "price {} is not the journaled {expected_source} price {expected_price}",
            fill.price
        ));
    }
    match fill.fee_source.as_deref() {
        Some("configured_rate") | None => {}
        Some(other) => wrong(format!("unknown fee_source `{other}`")),
    }
    // The configured rate is not journaled, so only its configurable range
    // bounds the estimate. The writer computes size * price * bps / 1e4 in
    // that order; f64 rounding is monotonic, so the bound is exact.
    if let (Some(fee), Some(size), Some(price)) =
        (exact(&fill.fee), exact(&fill.size), exact(&fill.price))
    {
        let ceiling = size * price * MAX_TAKER_FEE_BPS / 10_000.0;
        if !(0.0..=ceiling).contains(&fee) {
            wrong(format!(
                "fee {} is outside 0..={ceiling} for a configured rate up to {MAX_TAKER_FEE_BPS} bps",
                fill.fee
            ));
        }
    }
}

// ─── View ────────────────────────────────────────────────────────────────

/// One frozen archive read once: a single audit of the journal and
/// evidence, the telemetry indexed by CLOID, the journal split at its
/// newlines, and the findings that hold for every IOC. Views over it read
/// nothing again and repeat no archive-wide work.
struct Archive<'a> {
    report: audit::Report,
    lines: Vec<&'a [u8]>,
    telemetry: Telemetry,
    /// Findings about the journal and evidence as a whole, in view order.
    journal: Vec<(ViewStatus, String)>,
    /// Findings about the telemetry session and file end, in view order,
    /// already labeled as telemetry findings.
    file: Vec<(ViewStatus, String)>,
}

/// The worst status among `findings`; `Bound` when there is none.
fn worst<'f>(findings: impl IntoIterator<Item = &'f (ViewStatus, String)>) -> ViewStatus {
    findings
        .into_iter()
        .map(|(status, _)| *status)
        .max()
        .unwrap_or(ViewStatus::Bound)
}

/// Findings about the journal and evidence that hold for every IOC.
fn journal_findings(report: &audit::Report) -> Vec<(ViewStatus, String)> {
    let mut out = Vec::new();
    if report.journal.session.is_none() {
        out.push((
            ViewStatus::Invalid,
            "the journal has no readable session start".into(),
        ));
    }
    // Journal-wide contradictions make every binding suspect; unresolved
    // issues elsewhere (another IOC, the Lighter leg) do not.
    for issue in report
        .issues
        .iter()
        .filter(|i| i.severity == Severity::Invalid)
    {
        out.push((
            ViewStatus::Invalid,
            format!("audit {}: {}", issue.kind, issue.detail),
        ));
    }
    // A torn final line was cut mid-write and may have been any record,
    // including a later terminal or intent for this CLOID. The complete
    // prefix cannot show the selected IOC is unaffected, so the view is at
    // least unresolved; a contradiction above still wins.
    for issue in report.issues.iter().filter(|i| i.kind == "truncated_tail") {
        let at = issue
            .line
            .map_or_else(String::new, |line| format!(" on line {line}"));
        out.push((
            ViewStatus::Unresolved,
            format!(
                "the journal is truncated{at}; its complete prefix cannot show this IOC is unaffected ({})",
                issue.detail
            ),
        ));
    }
    if report
        .evidence
        .as_ref()
        .is_some_and(|e| !e.identity_matches)
    {
        out.push((
            ViewStatus::Invalid,
            "the evidence names another network, account or coin".into(),
        ));
    }
    out
}

/// Findings about the telemetry session and file end that hold for every
/// IOC. Line faults stay apart so each view lists them in file order.
fn file_findings(start: Option<&StartRecord>, telemetry: &Telemetry) -> Vec<(ViewStatus, String)> {
    let mut out = Vec::new();
    match (&telemetry.session, start) {
        (Some(session), Some(start)) => {
            let mut mismatches = Vec::new();
            check_session(start, session, &mut mismatches);
            out.extend(mismatches.into_iter().map(|r| (ViewStatus::Invalid, r)));
            if start.lighter_market_id.is_none() {
                out.push((
                    ViewStatus::Unresolved,
                    "the journal records no Lighter market (schema 1 or legacy), so the market cannot be matched".into(),
                ));
            }
        }
        // A complete first line leaves a session or a fault, so neither
        // means the file is empty of complete lines.
        (None, _) if telemetry.faults.is_empty() => {
            out.push((ViewStatus::Invalid, "no session record on line 1".into()))
        }
        _ => {}
    }
    if let Some(line) = telemetry.torn {
        out.push((
            ViewStatus::Unresolved,
            format!("line {line} lacks its newline; the file may be incomplete"),
        ));
    }
    for finding in &mut out {
        finding.1.insert_str(0, "telemetry: ");
    }
    out
}

/// Two fault lists, each in file order, merged back into file order. A
/// line holds at most one fault, so lines never tie.
fn in_file_order<'f>(a: &'f [(usize, String)], b: &'f [(usize, String)]) -> Vec<&'f str> {
    let mut merged: Vec<&(usize, String)> = a.iter().chain(b).collect();
    merged.sort_by_key(|fault| fault.0);
    merged.into_iter().map(|fault| fault.1.as_str()).collect()
}

/// The audit's verdict on the IOC, as the view states it.
fn order_finding(
    cloid: &str,
    order: Option<&OrderReport>,
    terminal: Option<&TerminalRecord>,
) -> Option<(ViewStatus, String)> {
    match order {
        None => Some((
            ViewStatus::Invalid,
            format!("no journaled IOC has CLOID {cloid}"),
        )),
        Some(o) if o.status == OrderStatus::Invalid => Some((
            ViewStatus::Invalid,
            format!("the audit finds this IOC invalid: {:?}", o.reasons),
        )),
        Some(o) if o.status == OrderStatus::Pending => Some((
            ViewStatus::Unresolved,
            "the IOC is not reconciled; neither fee can be bound to a fill".into(),
        )),
        // Telemetry is exported only after a journaled terminal; an
        // evidence-matched IOC has none, so the writer emitted no record.
        Some(o) if o.status == OrderStatus::Matched => Some((
            ViewStatus::Unresolved,
            "reconciled by evidence only: the journal has no terminal, so the writer exported no estimate".into(),
        )),
        Some(o) if o.filled.is_none_or(|f| f <= Decimal::ZERO) => Some((
            ViewStatus::Unresolved,
            "the IOC did not fill; there is no fee to compare".into(),
        )),
        Some(_) if terminal.is_none() => Some((
            ViewStatus::Invalid,
            "the journaled terminal cannot be read back".into(),
        )),
        Some(_) => None,
    }
}

/// The CLOID's well-formed telemetry records against its journaled
/// terminal. Labeled as telemetry findings.
fn fill_findings(
    order: Option<&OrderReport>,
    terminal: Option<&TerminalRecord>,
    fills: &[Fill],
    notes: &mut Vec<String>,
) -> Vec<(ViewStatus, String)> {
    let mut out = Vec::new();
    let mut tel = |status, reason: String| out.push((status, format!("telemetry: {reason}")));
    if let Some((first, repeats)) = fills.split_first() {
        for repeat in repeats {
            // The writer emits one record per reconciled IOC. An identical
            // line is a copy; anything else is a second, different claim.
            let same = Fill {
                line: first.line,
                ..repeat.clone()
            } == *first;
            if same {
                notes.push(format!(
                    "line {} repeats line {} exactly",
                    repeat.line, first.line
                ));
            } else {
                tel(
                    ViewStatus::Invalid,
                    format!(
                        "lines {} and {} disagree for this CLOID",
                        first.line, repeat.line
                    ),
                );
            }
        }
        match (order, terminal) {
            (Some(order), Some(terminal)) => {
                let mut wrong = Vec::new();
                check_fill(first, order, terminal, &mut wrong);
                for reason in wrong {
                    tel(ViewStatus::Invalid, reason);
                }
            }
            // Exported only after a journaled terminal for this CLOID.
            _ => tel(
                ViewStatus::Invalid,
                format!(
                    "line {} exports a fill the journal never recorded as terminal",
                    first.line
                ),
            ),
        }
    } else if terminal.is_some()
        && order.is_some_and(|o| o.filled.is_some_and(|f| f > Decimal::ZERO))
    {
        tel(
            ViewStatus::Unresolved,
            "no hedge_fill for this CLOID (telemetry disabled, dropped or written elsewhere)"
                .into(),
        );
    }
    out
}

/// The audit's venue fee attribution, as the view states it.
fn venue_finding(fee: Option<&FeeReport>) -> Option<(ViewStatus, String)> {
    match fee.map(|f| f.status)? {
        "attributed" => None,
        "invalid" => Some((
            ViewStatus::Invalid,
            "venue fills contradict this IOC; see venue_fee".into(),
        )),
        "not_provided" => Some((
            ViewStatus::Unresolved,
            "the evidence has no fills; the venue fee is unknown".into(),
        )),
        _ => Some((
            ViewStatus::Unresolved,
            "venue fills do not cover this IOC; the venue fee is unknown".into(),
        )),
    }
}

/// A CLOID no telemetry line names.
static NO_RECORDS: Records = Records {
    fills: Vec::new(),
    faults: Vec::new(),
};

/// One IOC's own findings, grouped by where a view lists them among the
/// archive-wide ones.
struct Judged<'r> {
    terminal: Option<TerminalRecord>,
    records: &'r Records,
    /// Listed first: the CLOID itself.
    cloid: Option<(ViewStatus, String)>,
    /// After the journal findings: the audit's verdict on the IOC.
    order_finding: Option<(ViewStatus, String)>,
    /// After the file findings: its telemetry record against the journal.
    telemetry: Vec<(ViewStatus, String)>,
    /// Last: the venue fee.
    venue: Option<(ViewStatus, String)>,
    /// Facts that do not block binding, such as an exact repeat.
    notes: Vec<String>,
}

impl Judged<'_> {
    /// This CLOID's own malformed `hedge_fill` lines contradict its view.
    fn faulted(&self) -> ViewStatus {
        if self.records.faults.is_empty() {
            ViewStatus::Bound
        } else {
            ViewStatus::Invalid
        }
    }

    /// The worst of its own findings; `Bound` when archive findings alone
    /// can hold the view back.
    fn status(&self) -> ViewStatus {
        worst(
            self.cloid
                .iter()
                .chain(&self.order_finding)
                .chain(&self.telemetry)
                .chain(&self.venue),
        )
        .max(self.faulted())
    }

    /// The same for its telemetry side alone.
    fn telemetry_status(&self) -> ViewStatus {
        worst(&self.telemetry).max(self.faulted())
    }

    /// Its own reasons in view order, without the archive-wide ones.
    fn reasons(&self) -> Vec<String> {
        let text = |finding: &(ViewStatus, String)| finding.1.clone();
        let faults = self
            .records
            .faults
            .iter()
            .map(|(_, r)| format!("telemetry: {r}"));
        self.cloid
            .iter()
            .chain(&self.order_finding)
            .map(text)
            .chain(faults)
            .chain(self.telemetry.iter().chain(&self.venue).map(text))
            .collect()
    }
}

impl<'a> Archive<'a> {
    /// Audit once, index the telemetry once and split the journal once.
    fn new(journal: &'a [u8], evidence: &Evidence, telemetry: &[u8]) -> Self {
        let report = audit::audit(journal, Some(evidence));
        let telemetry = read_telemetry(telemetry);
        let journal_wide = journal_findings(&report);
        let file = file_findings(report.journal.session.as_ref(), &telemetry);
        Archive {
            report,
            lines: journal_lines(journal),
            telemetry,
            journal: journal_wide,
            file,
        }
    }

    /// A malformed line no CLOID owns contradicts every view.
    fn faulted(&self) -> ViewStatus {
        if self.telemetry.faults.is_empty() {
            ViewStatus::Bound
        } else {
            ViewStatus::Invalid
        }
    }

    /// The worst finding that holds for every IOC; `Bound` when none does.
    fn status(&self) -> ViewStatus {
        worst(self.journal.iter().chain(&self.file)).max(self.faulted())
    }

    /// The same for the telemetry side alone.
    fn telemetry_status(&self) -> ViewStatus {
        worst(&self.file).max(self.faulted())
    }

    /// Reasons that hold for every IOC, in view order.
    fn reasons(&self) -> Vec<String> {
        let faults = self
            .telemetry
            .faults
            .iter()
            .map(|(_, r)| format!("telemetry: {r}"));
        self.journal
            .iter()
            .map(|(_, r)| r.clone())
            .chain(faults)
            .chain(self.file.iter().map(|(_, r)| r.clone()))
            .collect()
    }

    /// One IOC's own findings, as `hedge-fee-view` finds them. `order` is
    /// the audit's IOC with this exact CLOID, if any.
    fn judge<'r>(&'r self, cloid: &str, order: Option<&'r OrderReport>) -> Judged<'r> {
        let terminal = order
            .and_then(|o| o.terminal_lines.first())
            .and_then(|&line| terminal_at(&self.lines, line));
        let records = self.telemetry.by_cloid.get(cloid).unwrap_or(&NO_RECORDS);
        let mut notes = Vec::new();
        let telemetry = fill_findings(order, terminal.as_ref(), &records.fills, &mut notes);
        Judged {
            cloid: validate_hex(cloid, 16).is_err().then(|| {
                (
                    ViewStatus::Invalid,
                    format!("`{cloid}` is not a 16-byte hex CLOID"),
                )
            }),
            order_finding: order_finding(cloid, order, terminal.as_ref()),
            telemetry,
            venue: venue_finding(order.map(|o| &o.fee)),
            notes,
            terminal,
            records,
        }
    }

    /// The `hedge-fee-view` of one CLOID.
    fn view(&self, cloid: &str) -> FeeView {
        let order = self.report.orders.iter().find(|o| o.cloid == cloid);
        let judged = self.judge(cloid, order);
        let text = |finding: &(ViewStatus, String)| finding.1.clone();
        // Reasons in the order the view has always listed them.
        let faults = in_file_order(&self.telemetry.faults, &judged.records.faults);
        let reasons: Vec<String> = judged
            .cloid
            .iter()
            .chain(&self.journal)
            .chain(&judged.order_finding)
            .map(text)
            .chain(faults.into_iter().map(|r| format!("telemetry: {r}")))
            .chain(
                self.file
                    .iter()
                    .chain(&judged.telemetry)
                    .chain(&judged.venue)
                    .map(text),
            )
            .collect();
        let fills = &judged.records.fills;
        let fill = fills.first();
        let telemetry = TelemetryView {
            status: judged.telemetry_status().max(self.telemetry_status()),
            session_line: self.telemetry.session.as_ref().map(|_| 1),
            stopped_line: self.telemetry.stopped,
            lines: fills.iter().map(|f| f.line).collect(),
            side: fill.map(|f| f.side.clone()),
            size: fill.map(|f| f.size.clone()),
            price: fill.map(|f| f.price.clone()),
            price_source: fill.map(|f| f.price_source.clone()),
            fee: fill.map(|f| f.fee.clone()),
            fee_source: fill.map(|f| f.fee_source.clone().unwrap_or_else(|| "unknown".into())),
            position_after: fill.map(|f| f.position_after.clone()),
            notes: judged.notes.clone(),
        };
        let venue_fee = order.map(|o| o.fee.clone());
        let comparison = compare(&telemetry, venue_fee.as_ref());
        let terminal = judged.terminal.as_ref();
        FeeView {
            mode: "offline-read-only",
            status: judged.status().max(self.status()),
            cloid: cloid.to_owned(),
            inputs: None,
            session: self.report.journal.session.clone(),
            audit_status: self.report.status,
            ioc: order.map(|o| IocView {
                status: o.status,
                intent_line: o.intent_line,
                terminal_lines: o.terminal_lines.clone(),
                side: o.side,
                size: o.size,
                limit_price: o.limit_price,
                filled: o.filled,
                average_price: terminal.and_then(|t| t.average_price),
                terminal_source: terminal.and_then(|t| t.terminal_source.clone()),
                position_after: o.position_after,
                reasons: o.reasons.clone(),
            }),
            telemetry,
            venue_fee,
            comparison,
            reasons,
            boundaries: BOUNDARIES.to_vec(),
        }
    }
}

/// Build the view from in-memory inputs. Pure: no I/O.
pub fn build(journal: &[u8], evidence: &Evidence, telemetry: &[u8], cloid: &str) -> FeeView {
    Archive::new(journal, evidence, telemetry).view(cloid)
}

/// Never a difference: the estimate names no asset, and nothing here proves
/// what it would be denominated in. Every applicable reason is listed.
fn compare(telemetry: &TelemetryView, venue: Option<&FeeReport>) -> Comparison {
    let mut reasons = Vec::new();
    match telemetry.fee.as_deref() {
        None => reasons.push("no telemetry estimate is bound to this IOC".into()),
        Some(_) => reasons.push(
            "the telemetry fee is a configured-rate estimate that names no asset; no evidence fixes its unit".into(),
        ),
    }
    if telemetry.price_source.as_deref() == Some("limit") {
        reasons.push(
            "the estimate is priced at the IOC limit, a worst-case bound, not the fill price"
                .into(),
        );
    }
    if telemetry.fee_source.as_deref() == Some("unknown") {
        reasons.push("the telemetry record does not say how its fee was derived".into());
    }
    match venue {
        Some(fee) if fee.status == "attributed" => {
            if fee.fees.len() > 1 {
                reasons.push(format!(
                    "the venue charged {} fee assets; they are never added or converted",
                    fee.fees.len()
                ));
            }
            if fee.fees.iter().any(|f| f.builder_fee.is_some()) {
                reasons.push(
                    "the venue fee includes a builder fee the estimate does not model".into(),
                );
            }
            if fee.fees.iter().any(|f| f.fee < Decimal::ZERO) {
                reasons.push("the venue fee is a rebate; the estimate is always a cost".into());
            }
        }
        Some(fee) => reasons.push(format!("the venue fee is {}", fee.status)),
        None => reasons.push("no venue fee is bound to this IOC".into()),
    }
    Comparison {
        status: "not_comparable",
        reasons,
    }
}

fn digest(path: &Path, bytes: &[u8]) -> InputDigest {
    InputDigest {
        path: path.display().to_string(),
        sha256: hex::encode(Sha256::digest(bytes)),
        bytes: bytes.len(),
    }
}

/// The three files of one frozen archive, each read once and only read.
struct Files {
    journal: Vec<u8>,
    evidence: Evidence,
    telemetry: Vec<u8>,
    inputs: Inputs,
}

fn read_files(journal: &Path, evidence: &Path, telemetry: &Path) -> Result<Files> {
    let read =
        |path: &Path| std::fs::read(path).with_context(|| format!("Read {}", path.display()));
    let (journal_bytes, evidence_bytes, telemetry_bytes) =
        (read(journal)?, read(evidence)?, read(telemetry)?);
    let parsed: Evidence = serde_json::from_slice(&evidence_bytes)
        .with_context(|| format!("Parse evidence {}", evidence.display()))?;
    Ok(Files {
        inputs: Inputs {
            journal: digest(journal, &journal_bytes),
            evidence: digest(evidence, &evidence_bytes),
            telemetry: digest(telemetry, &telemetry_bytes),
        },
        journal: journal_bytes,
        evidence: parsed,
        telemetry: telemetry_bytes,
    })
}

/// Read-only view over files on disk; the inputs are only read.
pub fn view_files(
    journal: &Path,
    evidence: &Path,
    telemetry: &Path,
    cloid: &str,
) -> Result<FeeView> {
    let files = read_files(journal, evidence, telemetry)?;
    let mut view = build(&files.journal, &files.evidence, &files.telemetry, cloid);
    view.inputs = Some(files.inputs);
    Ok(view)
}

pub mod coverage;

#[cfg(test)]
mod tests;
