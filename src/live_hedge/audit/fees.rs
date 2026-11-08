//! Venue-declared Hyperliquid fees for reconciled hedge IOCs.
//!
//! Frozen `userFills` / `userFillsByTime` entries are bound to an IOC only
//! through the OID its own `orderStatus` lookups name for its CLOID. The
//! fee is summed per `feeToken`, never converted or added across assets,
//! and only when the bound fills cover exactly the reconciled quantity.
//! `fee` already includes any `builderFee`, which is reported, not added.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One raw fills response and when it was captured.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FillsEvidence {
    pub observed_at_ms: u64,
    /// The array returned by `userFills` or `userFillsByTime`.
    pub response: serde_json::Value,
}

/// The fields this audit reads from one fill; others are ignored.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Fill {
    coin: String,
    side: String,
    #[serde(with = "rust_decimal::serde::str")]
    sz: Decimal,
    time: u64,
    oid: u64,
    tid: u64,
    #[serde(with = "rust_decimal::serde::str")]
    fee: Decimal,
    fee_token: String,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    builder_fee: Option<Decimal>,
}

/// The venue's fee claim for one IOC.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FeeReport {
    /// `not_provided` (no fills evidence), `attributed`, `unattributed`
    /// (evidence present but this IOC cannot be bound or covered) or
    /// `invalid` (contradictory fills).
    pub status: &'static str,
    /// Always `frozen_venue_fills` when evidence was used: the venue's
    /// declaration as captured, not proof of authenticity or completeness.
    pub source: &'static str,
    pub oid: Option<u64>,
    /// Trade IDs counted, ascending.
    pub tids: Vec<u64>,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub matched_size: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub reconciled_size: Option<Decimal>,
    /// Signed fee per fee asset (negative is a rebate); empty unless
    /// attributed.
    pub fees: Vec<AssetFee>,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AssetFee {
    pub token: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub fee: Decimal,
    /// Builder fee included in `fee`, when the venue reported one.
    #[serde(with = "rust_decimal::serde::str_option")]
    pub builder_fee: Option<Decimal>,
}

impl FeeReport {
    pub(super) fn not_provided() -> Self {
        Self {
            status: "not_provided",
            source: "none",
            oid: None,
            tids: Vec::new(),
            matched_size: None,
            reconciled_size: None,
            fees: Vec::new(),
            reasons: Vec::new(),
        }
    }

    fn with(status: &'static str, oid: Option<u64>, reason: String) -> Self {
        Self {
            status,
            source: "frozen_venue_fills",
            oid,
            reasons: vec![reason],
            ..Self::not_provided()
        }
    }
}

/// Fills keyed by trade ID. An identical repeat (the same fill in two
/// captures or overlapping windows) is kept once; the same `tid` with any
/// differing field is a contradiction.
pub(super) struct FillSet {
    by_tid: BTreeMap<u64, Fill>,
}

/// Parse every capture. Returns the set or the reasons it is unusable.
pub(super) fn load(captures: &[FillsEvidence], coin: &str) -> Result<FillSet, Vec<String>> {
    let mut by_tid: BTreeMap<u64, Fill> = BTreeMap::new();
    let mut errors = Vec::new();
    for (index, capture) in captures.iter().enumerate() {
        let fills: Vec<Fill> = match serde_json::from_value(capture.response.clone()) {
            Ok(fills) => fills,
            Err(error) => {
                errors.push(format!("fills[{index}] is not a fills array: {error}"));
                continue;
            }
        };
        for fill in fills {
            // Other coins in an account-wide window are not this session's.
            if fill.coin != coin {
                continue;
            }
            if fill.time > capture.observed_at_ms {
                errors.push(format!(
                    "fill tid {} at {} is after its capture time {}",
                    fill.tid, fill.time, capture.observed_at_ms
                ));
            }
            if fill.sz <= Decimal::ZERO || fill.fee_token.trim().is_empty() {
                errors.push(format!("fill tid {} has no size or fee token", fill.tid));
            }
            match by_tid.get(&fill.tid) {
                Some(seen) if *seen != fill => {
                    errors.push(format!(
                        "tid {} is reported with conflicting fields",
                        fill.tid
                    ));
                }
                Some(_) => {}
                None => {
                    by_tid.insert(fill.tid, fill);
                }
            }
        }
    }
    if errors.is_empty() {
        Ok(FillSet { by_tid })
    } else {
        Err(errors)
    }
}

/// What the audit knows about one IOC when attributing its fee.
pub(super) struct IocFacts<'a> {
    pub coin: &'a str,
    pub is_buy: bool,
    pub submitted_at_ms: u64,
    /// The OID every located lookup for this CLOID named.
    pub oid: Option<u64>,
    /// Whether another IOC's lookups name the same OID.
    pub oid_shared: bool,
    /// Reconciled fill (journal terminal or evidence match); None if open.
    pub filled: Option<Decimal>,
}

/// Attribute venue-declared fees to one IOC, or explain why not.
pub(super) fn attribute(fills: &FillSet, ioc: &IocFacts<'_>, skew_ms: u64) -> FeeReport {
    let Some(oid) = ioc.oid else {
        return FeeReport::with(
            "unattributed",
            None,
            "no orderStatus lookup binds this cloid to an oid".into(),
        );
    };
    if ioc.oid_shared {
        return FeeReport::with(
            "invalid",
            Some(oid),
            format!("oid {oid} is bound to more than one journaled IOC"),
        );
    }
    let bound: Vec<&Fill> = fills.by_tid.values().filter(|f| f.oid == oid).collect();
    let Some(filled) = ioc.filled else {
        return FeeReport::with(
            "unattributed",
            Some(oid),
            "the IOC fill is not reconciled; its fee stays unknown".into(),
        );
    };
    let side = if ioc.is_buy { "B" } else { "A" };
    let mut reasons = Vec::new();
    for fill in &bound {
        if fill.coin != ioc.coin || fill.side != side {
            reasons.push(format!(
                "tid {} is {} {}, the IOC is {} {}",
                fill.tid, fill.side, fill.coin, side, ioc.coin
            ));
        }
        if fill.time < ioc.submitted_at_ms.saturating_sub(skew_ms) {
            reasons.push(format!(
                "tid {} at {} precedes the intent time {}",
                fill.tid, fill.time, ioc.submitted_at_ms
            ));
        }
    }
    if !reasons.is_empty() {
        return FeeReport {
            reasons,
            ..FeeReport::with("invalid", Some(oid), String::new())
        };
    }
    let matched = bound
        .iter()
        .try_fold(Decimal::ZERO, |sum, f| sum.checked_add(f.sz));
    let tids: Vec<u64> = bound.iter().map(|f| f.tid).collect();
    let mut report = FeeReport {
        tids,
        matched_size: matched,
        reconciled_size: Some(filled),
        ..FeeReport::with("unattributed", Some(oid), String::new())
    };
    report.reasons.clear();
    match matched {
        None => {
            report.status = "invalid";
            report
                .reasons
                .push("fill sizes overflow the decimal range".into());
        }
        Some(_) if filled <= Decimal::ZERO => {
            if bound.is_empty() {
                report
                    .reasons
                    .push("no positive reconciled fill; nothing to attribute".into());
            } else {
                report.status = "invalid";
                report.reasons.push(format!(
                    "fills exist for oid {oid} but the reconciled fill is zero"
                ));
            }
        }
        Some(matched) if matched < filled => report.reasons.push(format!(
            "fills for oid {oid} cover {matched} of the reconciled {filled}"
        )),
        Some(matched) if matched > filled => {
            report.status = "invalid";
            report.reasons.push(format!(
                "fills for oid {oid} total {matched}, more than the reconciled {filled}"
            ));
        }
        Some(_) => match per_token(&bound) {
            Some(fees) => {
                report.status = "attributed";
                report.fees = fees;
            }
            None => {
                report.status = "invalid";
                report
                    .reasons
                    .push("fees overflow the decimal range".into());
            }
        },
    }
    report
}

/// Signed fee per token; builder fees are reported per token, not added.
fn per_token(fills: &[&Fill]) -> Option<Vec<AssetFee>> {
    let mut by_token: BTreeMap<&str, (Decimal, Option<Decimal>)> = BTreeMap::new();
    for fill in fills {
        let entry = by_token
            .entry(fill.fee_token.as_str())
            .or_insert((Decimal::ZERO, None));
        entry.0 = entry.0.checked_add(fill.fee)?;
        if let Some(builder) = fill.builder_fee {
            entry.1 = Some(entry.1.unwrap_or_default().checked_add(builder)?);
        }
    }
    Some(
        by_token
            .into_iter()
            .map(|(token, (fee, builder_fee))| AssetFee {
                token: token.to_owned(),
                fee,
                builder_fee,
            })
            .collect(),
    )
}
