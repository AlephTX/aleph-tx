//! Offline explanation of caller-declared Lighter (primary leg) orders.
//!
//! Replays the production REST recovery judgement (`recover_flat`) against
//! frozen responses: each declared client order must be terminal for the
//! exact account and market, any fill needs a trade watermark, and the
//! account snapshot must cover the highest fill watermark before its
//! position counts. The declared IDs come from the caller and are never
//! proven to be every order the account placed.

use super::super::journal::StartRecord;
use crate::exchanges::lighter::trading::account_response_position;
use crate::exchanges::lighter::trading::recovery::{
    HistoricalOrder, OrderState, order_history, order_state,
};
use crate::lighter_trading::watermark_covers;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Frozen Lighter responses for the session's account and market.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LighterEvidence {
    pub account_index: i64,
    pub market_id: i64,
    pub symbol: String,
    /// Client order IDs the caller asserts the session used. A subset is
    /// possible; the audit cannot prove the set complete.
    pub client_order_ids: Vec<i64>,
    /// Raw `/api/v1/accountOrders` responses for those IDs. An ID listed in
    /// more than one entry is a contradiction, as within one response.
    #[serde(default)]
    pub order_histories: Vec<RawResponse>,
    /// Raw `/api/v1/account?by=index` response captured after the histories.
    #[serde(default)]
    pub account: Option<RawResponse>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawResponse {
    pub observed_at_ms: u64,
    pub response: serde_json::Value,
}

/// One declared client order as the frozen history shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LighterOrderReport {
    pub client_order_id: i64,
    /// `terminal`, `live`, `missing` or `invalid`.
    pub state: &'static str,
    pub venue_status: Option<String>,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub filled: Option<Decimal>,
    /// Trade watermark the account must cover; 0 when nothing filled.
    pub fill_watermark: Option<u64>,
    pub reason: Option<String>,
}

/// The primary leg as far as the declared orders and one account sample
/// explain it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PrimaryLegReport {
    /// `not_provided` (no Lighter section), `missing_evidence`, `invalid`,
    /// `unresolved` or `explained` (all declared orders terminal, the account
    /// covers their fills and is flat).
    pub status: &'static str,
    /// Always `caller_declared_order_ids`: not proven to be every order.
    pub scope: &'static str,
    pub account_index: Option<i64>,
    pub market_id: Option<i64>,
    pub orders: Vec<LighterOrderReport>,
    /// Highest fill watermark of the declared orders; null until all are
    /// terminal.
    pub required_watermark: Option<u64>,
    pub account_watermark: Option<u64>,
    /// `covered`, `stale`, `unknown` (no account watermark),
    /// `orders_unresolved` or `no_account_sample`.
    pub watermark: &'static str,
    /// Signed position in the account sample, whether or not it counts.
    #[serde(with = "rust_decimal::serde::str_option")]
    pub position: Option<Decimal>,
    /// `flat` or `nonzero` once the sample covers every declared fill;
    /// otherwise `unproven`.
    pub residual: &'static str,
}

/// A traceable finding; the caller turns it into a report issue.
pub(super) struct Finding {
    pub invalid: bool,
    pub kind: &'static str,
    pub detail: String,
}

fn finding(invalid: bool, kind: &'static str, detail: String) -> Finding {
    Finding {
        invalid,
        kind,
        detail,
    }
}

impl PrimaryLegReport {
    pub(super) fn not_provided() -> Self {
        Self::empty("not_provided")
    }

    fn empty(status: &'static str) -> Self {
        Self {
            status,
            scope: "caller_declared_order_ids",
            account_index: None,
            market_id: None,
            orders: Vec::new(),
            required_watermark: None,
            account_watermark: None,
            watermark: "no_account_sample",
            position: None,
            residual: "unproven",
        }
    }
}

/// Explain the declared orders and account sample against the session.
/// `start` is the journal's start record when its schema binds a Lighter
/// market; `usable` is false when the evidence names another session.
pub(super) fn evaluate(
    start: Option<&StartRecord>,
    usable: bool,
    evidence: &LighterEvidence,
) -> (PrimaryLegReport, Vec<Finding>) {
    let mut findings = Vec::new();
    let Some((start, market_id)) = start.and_then(|start| Some((start, start.lighter_market_id?)))
    else {
        findings.push(finding(
            false,
            "lighter_unbound_market",
            "the journal start binds no Lighter market (schema before 2); \
             Lighter evidence cannot be matched to the session"
                .into(),
        ));
        return (PrimaryLegReport::empty("missing_evidence"), findings);
    };
    if !usable {
        // check_evidence_identity already reported the mismatch.
        return (PrimaryLegReport::empty("invalid"), findings);
    }
    let mut report = PrimaryLegReport {
        account_index: Some(evidence.account_index),
        market_id: Some(evidence.market_id),
        ..PrimaryLegReport::empty("unresolved")
    };
    let mut identity = Vec::new();
    if evidence.account_index != start.lighter_account_index {
        identity.push(format!(
            "account {} vs journal {}",
            evidence.account_index, start.lighter_account_index
        ));
    }
    if evidence.market_id != market_id {
        identity.push(format!(
            "market {} vs journal {market_id}",
            evidence.market_id
        ));
    }
    if evidence.symbol != start.symbol {
        identity.push(format!(
            "symbol {} vs journal {}",
            evidence.symbol, start.symbol
        ));
    }
    if !identity.is_empty() {
        findings.extend(
            identity
                .into_iter()
                .map(|detail| finding(true, "lighter_identity", detail)),
        );
        report.status = "invalid";
        return (report, findings);
    }
    evaluate_orders(evidence, &mut report, &mut findings);
    evaluate_account(evidence, &mut report, &mut findings);
    report.status = if findings.iter().any(|f| f.invalid) {
        "invalid"
    } else if findings.iter().any(|f| MISSING.contains(&f.kind)) {
        "missing_evidence"
    } else if findings.is_empty() {
        "explained"
    } else {
        "unresolved"
    };
    (report, findings)
}

/// Kinds that mean evidence was not supplied, as opposed to supplied and
/// still open.
const MISSING: &[&str] = &["lighter_missing_order_ids", "lighter_missing_account"];

fn evaluate_orders(
    evidence: &LighterEvidence,
    report: &mut PrimaryLegReport,
    findings: &mut Vec<Finding>,
) {
    if evidence.client_order_ids.is_empty() {
        findings.push(finding(
            false,
            "lighter_missing_order_ids",
            "no client order IDs declared; an empty set proves no order was placed".into(),
        ));
        return;
    }
    let mut records: Vec<HistoricalOrder> = Vec::new();
    for (index, history) in evidence.order_histories.iter().enumerate() {
        match order_history(&history.response) {
            Ok(orders) => records.extend(orders),
            Err(error) => findings.push(finding(
                true,
                "lighter_invalid_history",
                format!("order_histories[{index}]: {error:#}"),
            )),
        }
    }
    let mut seen = BTreeSet::new();
    let mut required = Some(0);
    for &id in &evidence.client_order_ids {
        if id <= 0 || !seen.insert(id) {
            findings.push(finding(
                true,
                "lighter_declared_id",
                format!("client order ID {id} is not positive or is declared twice"),
            ));
            required = None;
            continue;
        }
        let (order, reason) =
            match order_state(&records, id, evidence.account_index, evidence.market_id) {
                Ok(OrderState::Terminal {
                    order,
                    fill_watermark,
                }) => {
                    required = required.map(|r: u64| r.max(fill_watermark));
                    (
                        lighter_order(id, "terminal", Some(order), Some(fill_watermark)),
                        None,
                    )
                }
                Ok(OrderState::Live(order)) => {
                    required = None;
                    (
                        lighter_order(id, "live", Some(order), None),
                        Some((
                            false,
                            "lighter_order_live",
                            format!("{id} is {}", order.status),
                        )),
                    )
                }
                Ok(OrderState::Missing) => {
                    required = None;
                    (
                        lighter_order(id, "missing", None, None),
                        Some((
                            false,
                            "lighter_order_missing",
                            format!("{id} is in no order history; absence proves nothing"),
                        )),
                    )
                }
                Err(contradiction) => {
                    required = None;
                    let entry = records.iter().find(|o| o.client_order_index == id);
                    (
                        lighter_order(id, "invalid", entry, None),
                        Some((
                            true,
                            "lighter_order_contradiction",
                            format!("{id}: {contradiction} ({})", contradiction.kind()),
                        )),
                    )
                }
            };
        let mut order = order;
        if let Some((invalid, kind, detail)) = reason {
            order.reason = Some(detail.clone());
            findings.push(finding(invalid, kind, detail));
        }
        report.orders.push(order);
    }
    report.required_watermark = required;
}

fn lighter_order(
    id: i64,
    state: &'static str,
    order: Option<&HistoricalOrder>,
    fill_watermark: Option<u64>,
) -> LighterOrderReport {
    LighterOrderReport {
        client_order_id: id,
        state,
        venue_status: order.map(|o| o.status.clone()),
        filled: order.map(|o| o.filled_base_amount),
        fill_watermark,
        reason: None,
    }
}

fn evaluate_account(
    evidence: &LighterEvidence,
    report: &mut PrimaryLegReport,
    findings: &mut Vec<Finding>,
) {
    let Some(account) = &evidence.account else {
        findings.push(finding(
            false,
            "lighter_missing_account",
            "no Lighter account sample; the residual position is unproven".into(),
        ));
        return;
    };
    let (position, account_watermark) = match account_response_position(
        &account.response,
        evidence.account_index,
        evidence.market_id,
        &evidence.symbol,
    ) {
        Ok(sample) => sample,
        Err(error) => {
            findings.push(finding(
                true,
                "lighter_invalid_account",
                format!("{error:#}"),
            ));
            report.watermark = "unknown";
            return;
        }
    };
    report.position = Some(position);
    report.account_watermark = account_watermark;
    let Some(required) = report.required_watermark else {
        report.watermark = "orders_unresolved";
        return;
    };
    if !watermark_covers(account_watermark, required) {
        report.watermark = if account_watermark.is_some() {
            "stale"
        } else {
            "unknown"
        };
        findings.push(finding(
            false,
            "lighter_watermark",
            format!(
                "account watermark {account_watermark:?} does not cover fill watermark {required}"
            ),
        ));
        return;
    }
    report.watermark = "covered";
    if position.is_zero() {
        report.residual = "flat";
    } else {
        report.residual = "nonzero";
        findings.push(finding(
            false,
            "lighter_residual",
            format!("covered account sample holds Lighter position {position}"),
        ));
    }
}

#[cfg(test)]
mod tests;
