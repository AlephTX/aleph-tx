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
    let mut size_sum = ExactSum::default();
    for fill in &bound {
        size_sum.add(fill.sz);
    }
    // Compared exactly, before any conversion back to Decimal.
    let coverage = size_sum.cmp_to(filled);
    let exact_size = size_sum.exact();
    let tids: Vec<u64> = bound.iter().map(|f| f.tid).collect();
    let mut report = FeeReport {
        tids,
        matched_size: exact_size.as_ref().ok().copied(),
        reconciled_size: Some(filled),
        ..FeeReport::with("unattributed", Some(oid), String::new())
    };
    report.reasons.clear();
    let matched = exact_size.as_ref().ok().copied();
    match matched {
        _ if coverage == std::cmp::Ordering::Greater => {
            report.status = "invalid";
            report.reasons.push(format!(
                "fills for oid {oid} total {}, more than the reconciled {filled}",
                exact_size
                    .as_ref()
                    .map_or_else(Clone::clone, ToString::to_string)
            ));
        }
        None if coverage == std::cmp::Ordering::Less => {
            report.reasons.push(format!(
                "fills for oid {oid} cover less than the reconciled {filled}"
            ));
        }
        None => {
            report.status = "invalid";
            report
                .reasons
                .push("matched size is not exactly representable".into());
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
        Some(matched) if coverage == std::cmp::Ordering::Less => report.reasons.push(format!(
            "fills for oid {oid} cover {matched} of the reconciled {filled}"
        )),
        // Exactly equal: only now are fees summed.
        Some(_) => match per_token(&bound) {
            Ok(fees) => {
                report.status = "attributed";
                report.fees = fees;
            }
            Err(reason) => {
                report.status = "invalid";
                report.reasons.push(reason);
            }
        },
    }
    report
}

/// Exact signed fee per token; builder fees are summed separately and
/// reported, not added. A token whose total `Decimal` cannot hold exactly
/// rejects the whole attribution, naming the token and field.
fn per_token(fills: &[&Fill]) -> Result<Vec<AssetFee>, String> {
    let mut by_token: BTreeMap<&str, (ExactSum, Option<ExactSum>)> = BTreeMap::new();
    for fill in fills {
        let entry = by_token
            .entry(fill.fee_token.as_str())
            .or_insert_with(|| (ExactSum::default(), None));
        entry.0.add(fill.fee);
        if let Some(builder) = fill.builder_fee {
            entry.1.get_or_insert_with(ExactSum::default).add(builder);
        }
    }
    by_token
        .into_iter()
        .map(|(token, (fee, builder))| {
            let fee = fee.exact().map_err(|exact| {
                format!("{token} fee total {exact} is not exactly representable")
            })?;
            let builder_fee = builder
                .map(|b| {
                    b.exact().map_err(|exact| {
                        format!("{token} builderFee total {exact} is not exactly representable")
                    })
                })
                .transpose()?;
            Ok(AssetFee {
                token: token.to_owned(),
                fee,
                builder_fee,
            })
        })
        .collect()
}

/// Exact sum of decimals: every term is aligned to the largest scale and
/// added as an arbitrary-precision integer, so no intermediate step rounds
/// and the order of signed terms does not matter. `Decimal::checked_add`
/// rounds once a sum needs more than 96 bits of mantissa at its scale.
#[derive(Debug, Clone, Default)]
struct ExactSum {
    /// (mantissa, scale) of every term.
    terms: Vec<(num_bigint::BigInt, u32)>,
}

impl ExactSum {
    fn add(&mut self, value: Decimal) {
        self.terms
            .push((num_bigint::BigInt::from(value.mantissa()), value.scale()));
    }

    /// The exact total as (mantissa, scale) at the largest term scale.
    fn total(&self) -> (num_bigint::BigInt, u32) {
        let scale = self.terms.iter().map(|(_, s)| *s).max().unwrap_or(0);
        let ten = num_bigint::BigInt::from(10);
        let sum = self
            .terms
            .iter()
            .map(|(m, s)| m * num_traits::Pow::pow(&ten, scale - s))
            .sum();
        (sum, scale)
    }

    /// The total as a `Decimal` if it fits exactly; otherwise None (the
    /// exact digits are kept in the error text by the caller).
    fn exact(&self) -> Result<Decimal, String> {
        let (mut mantissa, mut scale) = self.total();
        let ten = num_bigint::BigInt::from(10);
        // Drop trailing zero digits the scale does not need.
        while scale > 0 && (&mantissa % &ten) == num_bigint::BigInt::from(0) {
            mantissa /= &ten;
            scale -= 1;
        }
        i128::try_from(&mantissa)
            .ok()
            .and_then(|m| Decimal::try_from_i128_with_scale(m, scale).ok())
            .ok_or_else(|| decimal_text(&mantissa, scale))
    }

    /// Compare the exact total with `other` exactly.
    fn cmp_to(&self, other: Decimal) -> std::cmp::Ordering {
        let mut both = self.clone();
        both.add(-other);
        both.total().0.cmp(&num_bigint::BigInt::from(0))
    }
}

/// Exact decimal text for an integer mantissa and scale.
fn decimal_text(mantissa: &num_bigint::BigInt, scale: u32) -> String {
    let negative = mantissa.sign() == num_bigint::Sign::Minus;
    let digits = mantissa.magnitude().to_string();
    let scale = scale as usize;
    let body = if scale == 0 {
        digits
    } else if digits.len() > scale {
        let (int, frac) = digits.split_at(digits.len() - scale);
        format!("{int}.{frac}")
    } else {
        format!("0.{}{digits}", "0".repeat(scale - digits.len()))
    };
    if negative { format!("-{body}") } else { body }
}

#[cfg(test)]
mod exact_sum_tests {
    use super::*;
    use std::str::FromStr;

    fn d(value: &str) -> Decimal {
        Decimal::from_str(value).unwrap()
    }

    fn sum(values: &[&str]) -> ExactSum {
        let mut total = ExactSum::default();
        for value in values {
            total.add(d(value));
        }
        total
    }

    /// The review reproducer: Decimal::checked_add rounds 10 + 1e-28 to 10.
    #[test]
    fn exact_sum_never_rounds_where_decimal_does() {
        let tiny = "0.0000000000000000000000000001";
        assert_eq!(
            d("10").checked_add(d(tiny)),
            Some(d("10.000000000000000000000000000"))
        );
        let total = sum(&["10", tiny]);
        assert_eq!(total.cmp_to(d("10")), std::cmp::Ordering::Greater);
        assert_eq!(
            total.exact().unwrap_err(),
            "10.0000000000000000000000000001"
        );
        // Representable at the highest precision: kept exactly.
        assert_eq!(
            sum(&["0.1", tiny]).exact().unwrap(),
            d("0.1000000000000000000000000001")
        );
    }

    /// Signed terms cancel exactly in any order; trailing zeros normalise.
    #[test]
    fn exact_sum_is_order_independent_and_signed() {
        let big = "79228162514264337593543950335"; // Decimal::MAX
        for order in [
            [big, "-1", "-79228162514264337593543950334"],
            ["-1", "-79228162514264337593543950334", big],
        ] {
            let total = sum(&order);
            assert_eq!(total.exact().unwrap(), Decimal::ZERO, "{order:?}");
            assert_eq!(total.cmp_to(Decimal::ZERO), std::cmp::Ordering::Equal);
        }
        // An intermediate beyond Decimal::MAX that cancels back is fine.
        assert_eq!(
            sum(&[big, "1", "-2"]).exact().unwrap(),
            d("79228162514264337593543950334")
        );
        assert_eq!(
            sum(&["0.0010", "0.0030"]).exact().unwrap().to_string(),
            "0.004"
        );
        assert_eq!(sum(&["0.0009", "-0.0002"]).exact().unwrap(), d("0.0007"));
        assert_eq!(sum(&[]).exact().unwrap(), Decimal::ZERO);
        assert_eq!(
            sum(&[big, "1"]).exact().unwrap_err(),
            "79228162514264337593543950336"
        );
        assert_eq!(
            sum(&[
                "-0.5",
                "-0.0000000000000000000000000001",
                "-79228162514264337593543950"
            ])
            .exact()
            .unwrap_err(),
            "-79228162514264337593543950.5000000000000000000000000001"
        );
    }
}
