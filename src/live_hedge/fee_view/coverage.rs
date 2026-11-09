//! Offline, read-only fee evidence coverage for one frozen hedge session.
//!
//! The universe is the order set of one hedge-journal audit over the given
//! journal and evidence. Each IOC is judged exactly as `hedge-fee-view`
//! judges its CLOID on the same files, from one archive read once: each
//! file is read and hashed once, the journal audited once and the
//! telemetry indexed once. Findings that hold for every IOC (a torn file,
//! another session, a journal-wide contradiction) are listed once and hold
//! the ratio back; the rows stay as diagnostics.
//!
//! The denominator is every IOC with a positive fill and a known terminal,
//! never the records that happen to be visible. Fees are listed per IOC and
//! per asset; nothing is summed, converted or netted.

use super::{Archive, Inputs, Records, ViewStatus, read_files};
use crate::live_hedge::audit::{AuditStatus, Evidence, FeeReport, OrderReport, OrderStatus};
use crate::live_hedge::journal::StartRecord;
use anyhow::Result;
use rust_decimal::Decimal;
use serde::Serialize;
use std::collections::BTreeSet;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CoverageStatus {
    /// Every positive fill in the supplied files binds both cost records
    /// and no finding holds any view back. It covers these files only and
    /// does not show the session ended.
    Complete,
    /// A record is missing, an outcome is unknown, a file is torn or no IOC
    /// has a fill to cover.
    Incomplete,
    /// The inputs contradict each other or name another session.
    Invalid,
}

impl CoverageStatus {
    /// Same convention as the audit and the single view: 0, 2 or 3.
    pub fn exit_code(self) -> i32 {
        match self {
            Self::Complete => 0,
            Self::Incomplete => 2,
            Self::Invalid => 3,
        }
    }
}

/// One IOC by its own records; archive-wide findings are separate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Class {
    /// A positive fill whose telemetry estimate and venue fee both bind.
    Bound,
    /// A positive fill with no bound estimate or no attributed venue fee.
    MissingEvidence,
    /// Not reconciled: it may or may not have filled.
    UnknownOutcome,
    /// Reconciled with no fill, so there is no fee to cover.
    NoFill,
    /// Its own records contradict each other or the journal.
    Contradiction,
}

/// The telemetry `hedge_fill` for one IOC, as the writer emitted it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Estimate {
    /// Lines holding this CLOID's well-formed records.
    pub lines: Vec<usize>,
    /// The configured-rate estimate; names no asset. Null when absent.
    pub fee: Option<String>,
    /// `configured_rate`, or `unknown` when the writer recorded none.
    pub fee_source: Option<String>,
    /// `fill` (venue average price) or `limit` (the IOC limit price).
    pub price_source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OrderCoverage {
    pub cloid: String,
    pub intent_line: usize,
    pub terminal_lines: Vec<usize>,
    /// The audit's verdict for this IOC.
    pub audit_status: OrderStatus,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub filled: Option<Decimal>,
    pub class: Class,
    pub in_denominator: bool,
    /// What `hedge-fee-view` reports for this CLOID over the same files.
    pub view_status: ViewStatus,
    /// This IOC's own reasons, in view order. Archive-wide reasons are
    /// listed once in `archive`.
    pub reasons: Vec<String>,
    /// Facts that do not block binding, such as an exact repeat.
    pub notes: Vec<String>,
    pub estimate: Estimate,
    /// The audit's attribution of frozen venue fills, per fee asset.
    pub venue_fee: FeeReport,
}

/// Findings that hold for every IOC in the archive.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ArchiveView {
    /// The worst such finding; `bound` when there is none.
    pub status: ViewStatus,
    pub reasons: Vec<String>,
    /// `verified_flat` or `unverified`, as the audit reads the journal.
    pub journal_cleanup: &'static str,
    pub telemetry_stopped_line: Option<usize>,
    /// Scope facts that hold no view back.
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Counts {
    /// Every IOC intent the audit identified; the classes below add up to it.
    pub intents: usize,
    pub bound: usize,
    pub missing_evidence: usize,
    pub unknown_outcome: usize,
    pub no_fill: usize,
    pub contradiction: usize,
    /// Of `missing_evidence`: no bound estimate, and no attributed venue
    /// fee. One IOC can lack both.
    pub missing_telemetry: usize,
    pub missing_venue_fee: usize,
    /// Telemetry CLOIDs that name no journaled IOC.
    pub orphan_telemetry: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Coverage {
    /// IOCs with a positive fill and a known terminal: `journaled`
    /// (recorded) plus `evidence_only` (reconciled by venue evidence, so
    /// the writer exported no estimate).
    pub denominator: usize,
    pub journaled: usize,
    pub evidence_only: usize,
    pub bound: usize,
    /// `bound / denominator`; null when any reason in `withheld` applies.
    pub ratio: Option<f64>,
    pub withheld: Vec<String>,
}

/// A telemetry CLOID that names no journaled IOC.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Orphan {
    pub cloid: String,
    /// Every line naming it, well-formed or not.
    pub lines: Vec<usize>,
    /// Its malformed records, if any.
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CoverageReport {
    pub mode: &'static str,
    pub status: CoverageStatus,
    /// Paths and hashes of the files read; null for in-memory inputs.
    pub inputs: Option<Inputs>,
    pub session: Option<StartRecord>,
    /// The hedge-journal audit's verdict over the whole journal and evidence.
    pub audit_status: AuditStatus,
    pub archive: ArchiveView,
    pub coverage: Coverage,
    pub counts: Counts,
    /// Every journaled IOC, in journal order.
    pub orders: Vec<OrderCoverage>,
    pub orphan_telemetry: Vec<Orphan>,
    pub boundaries: Vec<&'static str>,
}

const BOUNDARIES: &[&str] = &[
    "Offline and read-only: only the three files are read, each once; no venue request, credential, signature or order was made.",
    "Each IOC is judged as hedge-fee-view judges its CLOID on the same files. Findings that hold for every IOC are listed once in `archive`.",
    "The denominator is every IOC with a positive fill and a known terminal, journaled or reconciled by evidence, never the records that happen to be visible.",
    "`complete` covers the supplied files only. A whole, parseable archive does not show the session ended or that no other record exists.",
    "Fees are listed per IOC and per asset as recorded. None is summed, converted or netted, and a missing fee is unknown, never zero.",
    "One Hyperliquid session only: no Lighter fee attribution, cross-leg PnL, amount charged, return or restart permission.",
];

fn positive(filled: Option<Decimal>) -> bool {
    filled.is_some_and(|f| f > Decimal::ZERO)
}

/// One IOC by the audit's verdict and its own findings only.
fn classify(order: &OrderReport, own: ViewStatus) -> Class {
    if own == ViewStatus::Invalid {
        return Class::Contradiction;
    }
    match order.status {
        OrderStatus::Invalid => Class::Contradiction,
        OrderStatus::Pending => Class::UnknownOutcome,
        _ if !positive(order.filled) => Class::NoFill,
        _ if own == ViewStatus::Bound => Class::Bound,
        _ => Class::MissingEvidence,
    }
}

/// A telemetry CLOID no journaled IOC has, with every line naming it.
fn orphan(cloid: &str, records: &Records) -> Orphan {
    let mut lines: Vec<usize> = records
        .fills
        .iter()
        .map(|f| f.line)
        .chain(records.faults.iter().map(|(line, _)| *line))
        .collect();
    lines.sort_unstable();
    Orphan {
        cloid: cloid.to_owned(),
        lines,
        reasons: records.faults.iter().map(|(_, r)| r.clone()).collect(),
    }
}

/// Build the report from in-memory inputs. Pure: no I/O.
pub fn build(journal: &[u8], evidence: &Evidence, telemetry: &[u8]) -> CoverageReport {
    let archive = Archive::new(journal, evidence, telemetry);
    let archive_status = archive.status();
    let mut counts = Counts::default();
    let mut orders = Vec::with_capacity(archive.report.orders.len());
    for order in &archive.report.orders {
        let judged = archive.judge(&order.cloid, Some(order));
        let own = judged.status();
        let class = classify(order, own);
        let fills = &judged.records.fills;
        counts.intents += 1;
        match class {
            Class::Bound => counts.bound += 1,
            Class::MissingEvidence => {
                counts.missing_evidence += 1;
                // No record at all: a malformed one is a contradiction.
                counts.missing_telemetry += usize::from(fills.is_empty());
                counts.missing_venue_fee += usize::from(order.fee.status != "attributed");
            }
            Class::UnknownOutcome => counts.unknown_outcome += 1,
            Class::NoFill => counts.no_fill += 1,
            Class::Contradiction => counts.contradiction += 1,
        }
        let first = fills.first();
        let estimate = Estimate {
            lines: fills.iter().map(|f| f.line).collect(),
            fee: first.map(|f| f.fee.clone()),
            fee_source: first.map(|f| f.fee_source.clone().unwrap_or_else(|| "unknown".into())),
            price_source: first.map(|f| f.price_source.clone()),
        };
        let reasons = judged.reasons();
        orders.push(OrderCoverage {
            cloid: order.cloid.clone(),
            intent_line: order.intent_line,
            terminal_lines: order.terminal_lines.clone(),
            audit_status: order.status,
            filled: order.filled,
            class,
            // A known positive fill owes a fee whatever records survive.
            in_denominator: positive(order.filled)
                && matches!(order.status, OrderStatus::Recorded | OrderStatus::Matched),
            view_status: own.max(archive_status),
            reasons,
            notes: judged.notes,
            estimate,
            venue_fee: order.fee.clone(),
        });
    }

    let journaled: BTreeSet<&str> = archive
        .report
        .orders
        .iter()
        .map(|o| o.cloid.as_str())
        .collect();
    let orphan_telemetry: Vec<Orphan> = archive
        .telemetry
        .by_cloid
        .iter()
        .filter(|(cloid, _)| !journaled.contains(cloid.as_str()))
        .map(|(cloid, records)| orphan(cloid, records))
        .collect();
    counts.orphan_telemetry = orphan_telemetry.len();

    let in_denominator = |status: OrderStatus| {
        orders
            .iter()
            .filter(|o| o.in_denominator && o.audit_status == status)
            .count()
    };
    let (journaled_fills, evidence_only) = (
        in_denominator(OrderStatus::Recorded),
        in_denominator(OrderStatus::Matched),
    );
    let denominator = journaled_fills + evidence_only;
    let mut withheld = Vec::new();
    if archive_status != ViewStatus::Bound {
        let what = if archive_status == ViewStatus::Invalid {
            "contradictory"
        } else {
            "incomplete"
        };
        withheld.push(format!(
            "the archive is {what} for every IOC (see archive.reasons); the rows are diagnostics only"
        ));
    }
    if counts.contradiction > 0 {
        withheld.push(format!(
            "{} IOC(s) have records that contradict each other or the journal",
            counts.contradiction
        ));
    }
    if counts.orphan_telemetry > 0 {
        withheld.push(format!(
            "{} telemetry CLOID(s) name no journaled IOC; the files do not belong together",
            counts.orphan_telemetry
        ));
    }
    if counts.unknown_outcome > 0 {
        withheld.push(format!(
            "{} IOC(s) are not reconciled; whether they filled, and so the denominator, is unknown",
            counts.unknown_outcome
        ));
    }
    if denominator == 0 {
        withheld.push("no IOC has a positive fill with a known terminal; nothing to cover".into());
    }
    let ratio = withheld
        .is_empty()
        .then(|| counts.bound as f64 / denominator as f64);
    let status = if archive_status == ViewStatus::Invalid
        || counts.contradiction > 0
        || counts.orphan_telemetry > 0
    {
        CoverageStatus::Invalid
    } else if withheld.is_empty() && counts.bound == denominator {
        CoverageStatus::Complete
    } else {
        CoverageStatus::Incomplete
    };

    let mut notes = Vec::new();
    if archive.report.journal.session_cleanup != "verified_flat" {
        notes.push(
            "the journal has no verified_flat record, so it does not show the session ended; coverage is of the IOCs these files hold".into(),
        );
    }
    if archive.telemetry.stopped.is_none() {
        notes.push("the telemetry has no stopped marker; the writer may not have finished".into());
    }
    CoverageReport {
        mode: "offline-read-only",
        status,
        inputs: None,
        session: archive.report.journal.session.clone(),
        audit_status: archive.report.status,
        archive: ArchiveView {
            status: archive_status,
            reasons: archive.reasons(),
            journal_cleanup: archive.report.journal.session_cleanup,
            telemetry_stopped_line: archive.telemetry.stopped,
            notes,
        },
        coverage: Coverage {
            denominator,
            journaled: journaled_fills,
            evidence_only,
            bound: counts.bound,
            ratio,
            withheld,
        },
        counts,
        orders,
        orphan_telemetry,
        boundaries: BOUNDARIES.to_vec(),
    }
}

/// Read-only report over files on disk; each is read once and only read.
pub fn coverage_files(journal: &Path, evidence: &Path, telemetry: &Path) -> Result<CoverageReport> {
    let files = read_files(journal, evidence, telemetry)?;
    let mut report = build(&files.journal, &files.evidence, &files.telemetry);
    report.inputs = Some(files.inputs);
    Ok(report)
}
