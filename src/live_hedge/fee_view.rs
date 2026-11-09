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

/// What one telemetry file says about its session and one CLOID.
struct Telemetry {
    session: Option<SessionLine>,
    stopped: Option<usize>,
    /// Every `hedge_fill` for the CLOID, in file order.
    fills: Vec<Fill>,
    /// Contradictions anywhere in the file.
    invalid: Vec<String>,
    /// Line number of a final line without its newline.
    torn: Option<usize>,
}

/// A finite JSON number token, unchanged; strings, null and the like fail.
fn number(raw: &RawValue) -> Option<String> {
    let text = raw.get();
    let numeric = text
        .bytes()
        .all(|b| b.is_ascii_digit() || matches!(b, b'-' | b'+' | b'.' | b'e' | b'E'));
    (numeric && text.parse::<f64>().is_ok_and(f64::is_finite)).then(|| text.to_owned())
}

fn read_telemetry(bytes: &[u8], cloid: &str) -> Telemetry {
    let mut out = Telemetry {
        session: None,
        stopped: None,
        fills: Vec::new(),
        invalid: Vec::new(),
        torn: None,
    };
    let mut rest = bytes;
    let mut line_no = 0;
    while !rest.is_empty() {
        line_no += 1;
        let Some(end) = rest.iter().position(|b| *b == b'\n') else {
            // Cut mid-record: it may have been any record, including this
            // CLOID's, so the file cannot show a complete set.
            out.torn = Some(line_no);
            break;
        };
        let line = &rest[..end];
        rest = &rest[end + 1..];
        read_line(&mut out, line_no, line, cloid);
    }
    if line_no == 0 {
        out.invalid.push("telemetry file is empty".into());
    }
    out
}

fn read_line(out: &mut Telemetry, line_no: usize, line: &[u8], cloid: &str) {
    let kind = match serde_json::from_slice::<Kind<'_>>(line) {
        Ok(Kind { kind: Some(kind) }) => kind.to_owned(),
        Ok(Kind { kind: None }) => {
            out.invalid.push(format!("line {line_no}: no record type"));
            return;
        }
        Err(error) => {
            out.invalid
                .push(format!("line {line_no}: not a JSON record ({error})"));
            return;
        }
    };
    if let Some(stopped) = out.stopped {
        out.invalid.push(format!(
            "line {line_no}: `{kind}` follows the stopped marker on line {stopped}"
        ));
        return;
    }
    if kind == "session" {
        if line_no != 1 {
            out.invalid.push(format!(
                "line {line_no}: a second session record; one file holds one session"
            ));
            return;
        }
        match serde_json::from_slice::<SessionLine>(line) {
            Ok(session) => out.session = Some(session),
            Err(error) => out
                .invalid
                .push(format!("line 1: malformed session record ({error})")),
        }
        return;
    }
    if line_no == 1 {
        out.invalid
            .push(format!("line 1 is `{kind}`, not the session record"));
        return;
    }
    match kind.as_str() {
        "stopped" => out.stopped = Some(line_no),
        "hedge_fill" => match serde_json::from_slice::<HedgeFillLine<'_>>(line) {
            Ok(fill) if fill.client_order_id == cloid => match own_fill(line_no, &fill) {
                Ok(fill) => out.fills.push(fill),
                Err(reason) => out.invalid.push(format!("line {line_no}: {reason}")),
            },
            Ok(_) => {}
            Err(error) => out
                .invalid
                .push(format!("line {line_no}: malformed hedge_fill ({error})")),
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

/// The terminal the audit accepted for this IOC, read from its first
/// terminal line (the report omits the average price and source). Lines are
/// numbered as the audit numbers them, and it lists only complete lines.
fn terminal_at(journal: &[u8], line: usize) -> Option<TerminalRecord> {
    let raw = journal.split(|b| *b == b'\n').nth(line.checked_sub(1)?)?;
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

fn worse(status: &mut ViewStatus, to: ViewStatus) {
    *status = (*status).max(to);
}

/// Raise `status` to at least `to` and record why.
fn flag(status: &mut ViewStatus, reasons: &mut Vec<String>, to: ViewStatus, reason: String) {
    worse(status, to);
    reasons.push(reason);
}

/// A telemetry-side finding: raises the telemetry status and the view's.
fn tel(reasons: &mut Vec<String>, to: ViewStatus, reason: String, status: &mut ViewStatus) {
    flag(status, reasons, to, format!("telemetry: {reason}"));
}

/// Build the view from in-memory inputs. Pure: no I/O.
pub fn build(journal: &[u8], evidence: &Evidence, telemetry: &[u8], cloid: &str) -> FeeView {
    let report = audit::audit(journal, Some(evidence));
    let mut status = ViewStatus::Bound;
    let mut reasons = Vec::new();
    if validate_hex(cloid, 16).is_err() {
        flag(
            &mut status,
            &mut reasons,
            ViewStatus::Invalid,
            format!("`{cloid}` is not a 16-byte hex CLOID"),
        );
    }
    let start = report.journal.session.clone();
    if start.is_none() {
        flag(
            &mut status,
            &mut reasons,
            ViewStatus::Invalid,
            "the journal has no readable session start".into(),
        );
    }
    // Journal-wide contradictions make every binding suspect; unresolved
    // issues elsewhere (another IOC, the Lighter leg) do not.
    for issue in report
        .issues
        .iter()
        .filter(|i| i.severity == Severity::Invalid)
    {
        flag(
            &mut status,
            &mut reasons,
            ViewStatus::Invalid,
            format!("audit {}: {}", issue.kind, issue.detail),
        );
    }
    // A torn final line was cut mid-write and may have been any record,
    // including a later terminal or intent for this CLOID. The complete
    // prefix cannot show the selected IOC is unaffected, so the view is at
    // least unresolved; a contradiction above still wins.
    for issue in report.issues.iter().filter(|i| i.kind == "truncated_tail") {
        let at = issue
            .line
            .map_or_else(String::new, |line| format!(" on line {line}"));
        flag(
            &mut status,
            &mut reasons,
            ViewStatus::Unresolved,
            format!(
                "the journal is truncated{at}; its complete prefix cannot show this IOC is unaffected ({})",
                issue.detail
            ),
        );
    }
    if report
        .evidence
        .as_ref()
        .is_some_and(|e| !e.identity_matches)
    {
        flag(
            &mut status,
            &mut reasons,
            ViewStatus::Invalid,
            "the evidence names another network, account or coin".into(),
        );
    }

    let order = report.orders.iter().find(|o| o.cloid == cloid);
    let terminal = order
        .and_then(|o| o.terminal_lines.first())
        .and_then(|&line| terminal_at(journal, line));
    match order {
        None => flag(&mut status, &mut reasons,
            ViewStatus::Invalid,
            format!("no journaled IOC has CLOID {cloid}"),
        ),
        Some(o) if o.status == OrderStatus::Invalid => {
            flag(&mut status, &mut reasons, ViewStatus::Invalid, format!("the audit finds this IOC invalid: {:?}", o.reasons))
        }
        Some(o) if o.status == OrderStatus::Pending => flag(&mut status, &mut reasons,
            ViewStatus::Unresolved,
            "the IOC is not reconciled; neither fee can be bound to a fill".into(),
        ),
        // Telemetry is exported only after a journaled terminal; an
        // evidence-matched IOC has none, so the writer emitted no record.
        Some(o) if o.status == OrderStatus::Matched => flag(&mut status, &mut reasons,
            ViewStatus::Unresolved,
            "reconciled by evidence only: the journal has no terminal, so the writer exported no estimate".into(),
        ),
        Some(o) if o.filled.is_none_or(|f| f <= Decimal::ZERO) => flag(&mut status, &mut reasons,
            ViewStatus::Unresolved,
            "the IOC did not fill; there is no fee to compare".into(),
        ),
        Some(_) if terminal.is_none() => flag(&mut status, &mut reasons,
            ViewStatus::Invalid,
            "the journaled terminal cannot be read back".into(),
        ),
        Some(_) => {}
    }

    let parsed = read_telemetry(telemetry, cloid);
    let mut telemetry_status = ViewStatus::Bound;
    for reason in &parsed.invalid {
        tel(
            &mut reasons,
            ViewStatus::Invalid,
            reason.clone(),
            &mut telemetry_status,
        );
    }
    match (&parsed.session, &start) {
        (Some(session), Some(start)) => {
            let mut mismatches = Vec::new();
            check_session(start, session, &mut mismatches);
            for reason in mismatches {
                tel(
                    &mut reasons,
                    ViewStatus::Invalid,
                    reason,
                    &mut telemetry_status,
                );
            }
            if start.lighter_market_id.is_none() {
                tel(&mut reasons,
                    ViewStatus::Unresolved,
                    "the journal records no Lighter market (schema 1 or legacy), so the market cannot be matched".into(),
                    &mut telemetry_status,
                );
            }
        }
        (None, _) if parsed.invalid.is_empty() => tel(
            &mut reasons,
            ViewStatus::Invalid,
            "no session record on line 1".into(),
            &mut telemetry_status,
        ),
        _ => {}
    }
    if let Some(line) = parsed.torn {
        tel(
            &mut reasons,
            ViewStatus::Unresolved,
            format!("line {line} lacks its newline; the file may be incomplete"),
            &mut telemetry_status,
        );
    }
    let mut notes = Vec::new();
    let fill = parsed.fills.first();
    if let Some(first) = fill {
        for repeat in &parsed.fills[1..] {
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
                    &mut reasons,
                    ViewStatus::Invalid,
                    format!(
                        "lines {} and {} disagree for this CLOID",
                        first.line, repeat.line
                    ),
                    &mut telemetry_status,
                );
            }
        }
        match (order, &terminal) {
            (Some(order), Some(terminal)) => {
                let mut wrong = Vec::new();
                check_fill(first, order, terminal, &mut wrong);
                for reason in wrong {
                    tel(
                        &mut reasons,
                        ViewStatus::Invalid,
                        reason,
                        &mut telemetry_status,
                    );
                }
            }
            // Exported only after a journaled terminal for this CLOID.
            _ => tel(
                &mut reasons,
                ViewStatus::Invalid,
                format!(
                    "line {} exports a fill the journal never recorded as terminal",
                    first.line
                ),
                &mut telemetry_status,
            ),
        }
    } else if terminal.is_some()
        && order.is_some_and(|o| o.filled.is_some_and(|f| f > Decimal::ZERO))
    {
        tel(
            &mut reasons,
            ViewStatus::Unresolved,
            "no hedge_fill for this CLOID (telemetry disabled, dropped or written elsewhere)"
                .into(),
            &mut telemetry_status,
        );
    }
    worse(&mut status, telemetry_status);

    let venue_fee = order.map(|o| o.fee.clone());
    match venue_fee.as_ref().map(|f| f.status) {
        Some("attributed") => {}
        Some("invalid") => flag(
            &mut status,
            &mut reasons,
            ViewStatus::Invalid,
            "venue fills contradict this IOC; see venue_fee".into(),
        ),
        Some("not_provided") => flag(
            &mut status,
            &mut reasons,
            ViewStatus::Unresolved,
            "the evidence has no fills; the venue fee is unknown".into(),
        ),
        Some(_) => flag(
            &mut status,
            &mut reasons,
            ViewStatus::Unresolved,
            "venue fills do not cover this IOC; the venue fee is unknown".into(),
        ),
        None => {}
    }

    let telemetry_view = TelemetryView {
        status: telemetry_status,
        session_line: parsed.session.as_ref().map(|_| 1),
        stopped_line: parsed.stopped,
        lines: parsed.fills.iter().map(|f| f.line).collect(),
        side: fill.map(|f| f.side.clone()),
        size: fill.map(|f| f.size.clone()),
        price: fill.map(|f| f.price.clone()),
        price_source: fill.map(|f| f.price_source.clone()),
        fee: fill.map(|f| f.fee.clone()),
        fee_source: fill.map(|f| f.fee_source.clone().unwrap_or_else(|| "unknown".into())),
        position_after: fill.map(|f| f.position_after.clone()),
        notes,
    };
    let comparison = compare(&telemetry_view, venue_fee.as_ref());
    FeeView {
        mode: "offline-read-only",
        status,
        cloid: cloid.to_owned(),
        inputs: None,
        session: start,
        audit_status: report.status,
        ioc: order.map(|o| IocView {
            status: o.status,
            intent_line: o.intent_line,
            terminal_lines: o.terminal_lines.clone(),
            side: o.side,
            size: o.size,
            limit_price: o.limit_price,
            filled: o.filled,
            average_price: terminal.as_ref().and_then(|t| t.average_price),
            terminal_source: terminal.as_ref().and_then(|t| t.terminal_source.clone()),
            position_after: o.position_after,
            reasons: o.reasons.clone(),
        }),
        telemetry: telemetry_view,
        venue_fee,
        comparison,
        reasons,
        boundaries: BOUNDARIES.to_vec(),
    }
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

/// Read-only view over files on disk; the inputs are only read.
pub fn view_files(
    journal: &Path,
    evidence: &Path,
    telemetry: &Path,
    cloid: &str,
) -> Result<FeeView> {
    let read =
        |path: &Path| std::fs::read(path).with_context(|| format!("Read {}", path.display()));
    let (journal_bytes, evidence_bytes, telemetry_bytes) =
        (read(journal)?, read(evidence)?, read(telemetry)?);
    let parsed: Evidence = serde_json::from_slice(&evidence_bytes)
        .with_context(|| format!("Parse evidence {}", evidence.display()))?;
    let mut view = build(&journal_bytes, &parsed, &telemetry_bytes, cloid);
    view.inputs = Some(Inputs {
        journal: digest(journal, &journal_bytes),
        evidence: digest(evidence, &evidence_bytes),
        telemetry: digest(telemetry, &telemetry_bytes),
    });
    Ok(view)
}

#[cfg(test)]
mod tests;
