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
    #[serde(deserialize_with = "exact")]
    sz: Decimal,
    time: u64,
    oid: u64,
    tid: u64,
    #[serde(deserialize_with = "exact")]
    fee: Decimal,
    fee_token: String,
    #[serde(default, deserialize_with = "exact_option")]
    builder_fee: Option<Decimal>,
}

/// A venue decimal string parsed only if `Decimal` holds it exactly: the
/// value must print back to the same digits (trailing fractional zeros
/// aside). `Decimal`'s parser rounds past 28 fractional digits, which
/// would let a slightly short or long amount pass an exact comparison.
fn parse_exact(raw: &str) -> Result<Decimal, String> {
    let value: Decimal = raw
        .parse()
        .map_err(|e| format!("`{raw}` is not a decimal: {e}"))?;
    let canonical = |text: &str| {
        let text = text.strip_prefix('+').unwrap_or(text);
        match text.split_once('.') {
            Some((int, frac)) => {
                let frac = frac.trim_end_matches('0');
                if frac.is_empty() {
                    int.to_owned()
                } else {
                    format!("{int}.{frac}")
                }
            }
            None => text.to_owned(),
        }
    };
    let (raw_c, value_c) = (canonical(raw), canonical(&value.to_string()));
    // "-0" and "0" are the same amount.
    let zero = |t: &str| t.trim_start_matches('-').chars().all(|c| c == '0');
    if raw_c == value_c || (zero(&raw_c) && value.is_zero()) {
        Ok(value)
    } else {
        Err(format!(
            "`{raw}` is not exactly representable (would be {value})"
        ))
    }
}

fn exact<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Decimal, D::Error> {
    let raw = String::deserialize(d)?;
    parse_exact(&raw).map_err(serde::de::Error::custom)
}

fn exact_option<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Decimal>, D::Error> {
    Option::<String>::deserialize(d)?
        .map(|raw| parse_exact(&raw).map_err(serde::de::Error::custom))
        .transpose()
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
    /// Fills for other coins, kept only to catch a bound oid claimed by
    /// another coin.
    foreign: BTreeMap<u64, Fill>,
}

/// Parse every capture. Returns the set or the reasons it is unusable.
pub(super) fn load(captures: &[FillsEvidence], coin: &str) -> Result<FillSet, Vec<String>> {
    let mut by_tid: BTreeMap<u64, Fill> = BTreeMap::new();
    let mut foreign: BTreeMap<u64, Fill> = BTreeMap::new();
    let mut errors = Vec::new();
    for (index, capture) in captures.iter().enumerate() {
        let Some(entries) = capture.response.as_array() else {
            errors.push(format!("fills[{index}] is not a fills array"));
            continue;
        };
        for (position, entry) in entries.iter().enumerate() {
            let fill: Fill = match serde_json::from_value(entry.clone()) {
                Ok(fill) => fill,
                Err(error) => {
                    // Located: a bad amount must not hide inside the array.
                    let tid = entry.get("tid").map(ToString::to_string);
                    errors.push(format!(
                        "fills[{index}][{position}] (tid {}): {error}",
                        tid.as_deref().unwrap_or("?")
                    ));
                    continue;
                }
            };
            // Other coins in an account-wide window are kept by tid but only
            // matter if they claim an oid an IOC is bound to (see attribute).
            if fill.coin != coin {
                let tid = fill.tid;
                if let Some(seen) = foreign.insert(tid, fill.clone())
                    && seen != fill
                {
                    errors.push(format!("tid {tid} is reported with conflicting fields"));
                }
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
    for tid in foreign.keys() {
        if by_tid.contains_key(tid) {
            errors.push(format!("tid {tid} is reported for two coins"));
        }
    }
    if errors.is_empty() {
        Ok(FillSet { by_tid, foreign })
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
    /// Venue time of the order's terminal status from a terminal
    /// `orderStatus` lookup. A fill after it cannot belong to the IOC. None
    /// when no terminal lookup exists: fees then stay unattributed rather
    /// than trusting an unbounded window.
    pub terminal_at_ms: Option<u64>,
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
    // Another coin claiming the bound oid contradicts the binding itself.
    let foreign: Vec<String> = fills
        .foreign
        .values()
        .filter(|f| f.oid == oid)
        .map(|f| {
            format!(
                "tid {} is {} {} on oid {oid}, the IOC is {}",
                f.tid, f.side, f.coin, ioc.coin
            )
        })
        .collect();
    if !foreign.is_empty() {
        return FeeReport {
            reasons: foreign,
            ..FeeReport::with("invalid", Some(oid), String::new())
        };
    }
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
        // Venue times on both sides: equal is the same instant, later is
        // after the order was already terminal.
        if let Some(terminal) = ioc.terminal_at_ms
            && fill.time > terminal
        {
            reasons.push(format!(
                "tid {} at {} is after the order's venue terminal time {terminal}",
                fill.tid, fill.time
            ));
        }
    }
    if reasons.is_empty() && ioc.terminal_at_ms.is_none() && !bound.is_empty() {
        // Contradictions above win; otherwise an unbounded window is not
        // trusted with a fee.
        return FeeReport {
            tids: bound.iter().map(|f| f.tid).collect(),
            reconciled_size: Some(filled),
            ..FeeReport::with(
                "unattributed",
                Some(oid),
                "no venue terminal time bounds this order's fills; supply its terminal orderStatus"
                    .into(),
            )
        };
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
