//! Lighter's declared fee rate for one terminal order, replayed from one
//! frozen trade.
//!
//! A trade binds to an order only through the venue `order_index` in that
//! order's own history entry; time, price, size or watermarks never bind.
//! The account's maker or taker role follows from the side the trade lists
//! the order on and `is_maker_ask`. The rate for that role must be an
//! explicit integer: Lighter omits zero or empty fields, so an absent rate
//! is unknown, never zero. The estimate is `rate / 1e6 * price * size`
//! (lighter-go `FeeTick` = 1_000_000, the feeder's `lighterFeePaid`),
//! computed exactly. It is the venue's declared rate applied to the trade's
//! notional, not the charged amount, and no settlement asset is named
//! because the trade declares none. Integrator fees are not included; this
//! process signs orders without an integrator.

use super::super::fees::{ExactSum, exact, parse_exact};
use super::{Finding, LighterEvidence, LighterOrderReport, RawResponse, finding};
use crate::exchanges::lighter::trading::recovery::order_history;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Fee rate denominator: parts per million of notional (lighter-go
/// `txtypes.FeeTick`). A rate beyond it would exceed the whole notional.
const FEE_TICK: i64 = 1_000_000;
const FEE_TICK_DIGITS: u32 = 6;

/// The fee view of one Lighter order.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LighterFeeReport {
    /// `not_provided` (no trades evidence), `no_fill` (terminal without a
    /// fill), `rate_derived`, `unknown` (not terminal, no bound trade that
    /// alone covers the fill, or no explicit rate for the account's role)
    /// or `invalid` (contradictory or unrepresentable evidence).
    pub status: &'static str,
    /// `frozen_trades` when trades evidence was used, otherwise `none`.
    pub source: &'static str,
    /// Venue order index from the order's own history entry.
    pub order_index: Option<i64>,
    pub trade_id: Option<u64>,
    /// `ask` or `bid`: where the trade lists the order.
    pub side: Option<&'static str>,
    /// `maker` or `taker` for this account.
    pub role: Option<&'static str>,
    /// `maker_fee` or `taker_fee`: the trade field the role selects.
    pub rate_field: Option<&'static str>,
    /// `explicit`, `absent` (omitted: Lighter drops zero or empty fields,
    /// so this is not a zero), `null` or `invalid`.
    pub rate_state: Option<&'static str>,
    /// Declared rate in parts per million of notional.
    pub rate_ppm: Option<i64>,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub price: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub size: Option<Decimal>,
    /// `rate_ppm / 1e6 * price * size`, exact, in the market's price units;
    /// negative is a rebate. An estimate from the declared rate, not the
    /// charged amount.
    #[serde(with = "rust_decimal::serde::str_option")]
    pub estimated_fee: Option<Decimal>,
    pub reasons: Vec<String>,
}

impl LighterFeeReport {
    pub(super) fn not_provided() -> Self {
        Self {
            status: "not_provided",
            source: "none",
            order_index: None,
            trade_id: None,
            side: None,
            role: None,
            rate_field: None,
            rate_state: None,
            rate_ppm: None,
            price: None,
            size: None,
            estimated_fee: None,
            reasons: Vec::new(),
        }
    }

    fn with(status: &'static str, order_index: Option<i64>, reason: String) -> Self {
        Self {
            status,
            source: "frozen_trades",
            order_index,
            reasons: vec![reason],
            ..Self::not_provided()
        }
    }
}

/// The fields this audit reads from one trade; others are ignored.
#[derive(Debug, Clone, PartialEq, Deserialize)]
struct Trade {
    trade_id: u64,
    #[serde(rename = "type", default)]
    kind: Option<String>,
    market_id: i64,
    #[serde(deserialize_with = "exact")]
    size: Decimal,
    #[serde(deserialize_with = "exact")]
    price: Decimal,
    ask_id: i64,
    bid_id: i64,
    #[serde(default)]
    ask_account_id: Option<i64>,
    #[serde(default)]
    bid_account_id: Option<i64>,
    /// Client order indexes per side; 0 or absent carries no identity.
    #[serde(default)]
    ask_client_id: Option<i64>,
    #[serde(default)]
    bid_client_id: Option<i64>,
    #[serde(default)]
    is_maker_ask: Option<bool>,
    /// Raw: absent (None) and `null` stay distinct from an explicit 0.
    #[serde(default, deserialize_with = "present")]
    maker_fee: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    taker_fee: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    integrator_maker_fee: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    integrator_taker_fee: Option<Value>,
}

/// A present field as its raw value, `null` included.
fn present<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(d).map(Some)
}

/// Trades keyed by trade ID. An identical repeat (the same raw trade in two
/// captures) is kept once and adds no coverage; the same ID with any
/// differing raw field, read here or not, is a contradiction.
struct TradeSet {
    by_id: BTreeMap<u64, Trade>,
}

fn load(captures: &[RawResponse]) -> Result<TradeSet, Vec<String>> {
    let mut by_id: BTreeMap<u64, Trade> = BTreeMap::new();
    let mut raw_by_id: BTreeMap<u64, &Value> = BTreeMap::new();
    let mut errors = Vec::new();
    for (index, capture) in captures.iter().enumerate() {
        let response = &capture.response;
        let entries = match (
            response.get("code").and_then(Value::as_i64),
            response.get("trades").and_then(Value::as_array),
        ) {
            (Some(200), Some(entries)) => entries,
            (Some(code), _) if code != 200 => {
                errors.push(format!("trades[{index}] has code {code}"));
                continue;
            }
            _ => {
                errors.push(format!(
                    "trades[{index}] is not a trades response with `code` and a `trades` array"
                ));
                continue;
            }
        };
        for (position, entry) in entries.iter().enumerate() {
            let trade = match Trade::deserialize(entry) {
                Ok(trade) => trade,
                Err(error) => {
                    // Located: a bad amount must not hide inside the array.
                    let id = entry.get("trade_id").map(ToString::to_string);
                    errors.push(format!(
                        "trades[{index}][{position}] (trade {}): {error}",
                        id.as_deref().unwrap_or("?")
                    ));
                    continue;
                }
            };
            if trade.trade_id == 0 || trade.size <= Decimal::ZERO || trade.price <= Decimal::ZERO {
                errors.push(format!(
                    "trade {} needs a positive ID, size and price",
                    trade.trade_id
                ));
                continue;
            }
            match raw_by_id.get(&trade.trade_id) {
                Some(seen) if *seen != entry => errors.push(format!(
                    "trade {} is reported with conflicting fields",
                    trade.trade_id
                )),
                Some(_) => {}
                None => {
                    raw_by_id.insert(trade.trade_id, entry);
                    by_id.insert(trade.trade_id, trade);
                }
            }
        }
    }
    if errors.is_empty() {
        Ok(TradeSet { by_id })
    } else {
        Err(errors)
    }
}

/// Identity and quantity read exactly from an order's own raw history
/// entry. Recovery does not read these fields; `HistoricalOrder` rounds
/// `filled_base_amount` past 28 fractional digits, so the fill is reread.
struct EntryFacts {
    order_index: Option<i64>,
    filled: Decimal,
    is_ask: Option<bool>,
    /// `post-only` can only make; `immediate-or-cancel` can only take.
    time_in_force: Option<String>,
}

/// Raw entries of every history that parses as recovery reads it.
fn history_entries(evidence: &LighterEvidence) -> Vec<&serde_json::Map<String, Value>> {
    evidence
        .order_histories
        .iter()
        .filter(|history| order_history(&history.response).is_ok())
        .filter_map(|history| history.response.get("orders")?.as_array())
        .flatten()
        .filter_map(Value::as_object)
        .collect()
}

/// The unique raw entry for `id`, read exactly. `Ok(None)` when no single
/// entry lists it.
fn entry_facts(
    entries: &[&serde_json::Map<String, Value>],
    id: i64,
) -> Result<Option<EntryFacts>, String> {
    let mut matched = entries
        .iter()
        .filter(|entry| entry.get("client_order_index").and_then(Value::as_i64) == Some(id));
    let (Some(entry), None) = (matched.next(), matched.next()) else {
        return Ok(None);
    };
    // 0 is lighter-go's nil order index: no venue order to bind.
    let order_index = match entry.get("order_index") {
        None | Some(Value::Null) => None,
        Some(value) => match value.as_i64() {
            Some(0) => None,
            Some(index) if index > 0 => Some(index),
            _ => {
                return Err(format!(
                    "history order_index {value} is not a non-negative integer"
                ));
            }
        },
    };
    let filled = entry
        .get("filled_base_amount")
        .and_then(Value::as_str)
        .ok_or("history filled_base_amount is not a decimal string")
        .map_err(str::to_owned)
        .and_then(|raw| parse_exact(raw).map_err(|e| format!("history filled_base_amount {e}")))?;
    let is_ask = match entry.get("is_ask") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_bool()
                .ok_or_else(|| format!("history is_ask {value} is not a boolean"))?,
        ),
    };
    let time_in_force = match entry.get("time_in_force") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_str()
                .ok_or_else(|| format!("history time_in_force {value} is not a string"))?
                .to_owned(),
        ),
    };
    Ok(Some(EntryFacts {
        order_index,
        filled,
        is_ask,
        time_in_force,
    }))
}

/// `rate_ppm / 1e6 * price * size`, exactly, or the exact digits that no
/// `Decimal` can hold.
fn estimate(rate_ppm: i64, price: Decimal, size: Decimal) -> Result<Decimal, String> {
    use num_bigint::BigInt;
    let mut product = ExactSum::default();
    product.add_scaled(
        BigInt::from(rate_ppm) * BigInt::from(price.mantissa()) * BigInt::from(size.mantissa()),
        price.scale() + size.scale() + FEE_TICK_DIGITS,
    );
    product
        .exact()
        .map_err(|exact| format!("estimated fee {exact} is not exactly representable"))
}

const _: () = assert!(10_i64.pow(FEE_TICK_DIGITS) == FEE_TICK);

/// Fill each checked order's fee view from frozen trades and report
/// contradictions. Fee findings never change the order or leg verdict; an
/// invalid one makes the whole report invalid, as Hyperliquid fees do.
pub(super) fn attribute(
    evidence: &LighterEvidence,
    orders: &mut [LighterOrderReport],
    findings: &mut Vec<Finding>,
) {
    if evidence.trades.is_empty() {
        return;
    }
    let trades = match load(&evidence.trades) {
        Ok(trades) => trades,
        Err(errors) => {
            for error in &errors {
                findings.push(finding(true, "lighter_invalid_trades", error.clone()));
            }
            for order in orders.iter_mut() {
                order.fee = LighterFeeReport {
                    status: "invalid",
                    source: "frozen_trades",
                    reasons: errors.clone(),
                    ..LighterFeeReport::not_provided()
                };
            }
            return;
        }
    };
    let entries = history_entries(evidence);
    let facts: Vec<Option<Result<EntryFacts, String>>> = orders
        .iter()
        .map(|order| {
            (order.state == "terminal").then(|| {
                entry_facts(&entries, order.client_order_id)?
                    .ok_or_else(|| "the terminal order has no unique raw history entry".to_owned())
            })
        })
        .collect();
    let index_of = |facts: &Option<Result<EntryFacts, String>>| match facts {
        Some(Ok(facts)) => facts.order_index,
        _ => None,
    };
    for (position, order) in orders.iter_mut().enumerate() {
        order.fee = match &facts[position] {
            None => LighterFeeReport::with(
                "unknown",
                None,
                format!(
                    "order is {}; only a terminal order's fill is replayed",
                    order.state
                ),
            ),
            Some(Err(reason)) => LighterFeeReport::with("invalid", None, reason.clone()),
            Some(Ok(entry)) => {
                let shared = entry.order_index.is_some()
                    && facts
                        .iter()
                        .enumerate()
                        .any(|(other, f)| other != position && index_of(f) == entry.order_index);
                attribute_one(&trades, evidence, order.client_order_id, entry, shared)
            }
        };
        if order.fee.status == "invalid" {
            for reason in &order.fee.reasons {
                findings.push(finding(
                    true,
                    "lighter_invalid_fee_evidence",
                    format!("{}: {reason}", order.client_order_id),
                ));
            }
        }
    }
}

/// One terminal order: bind trades by its own `order_index`, require one
/// trade that alone covers its exact fill, then read the rate its role
/// selects.
fn attribute_one(
    trades: &TradeSet,
    evidence: &LighterEvidence,
    id: i64,
    entry: &EntryFacts,
    shared: bool,
) -> LighterFeeReport {
    use std::cmp::Ordering;
    let index = entry.order_index;
    let names = |trade: &&Trade, oi: i64| trade.ask_id == oi || trade.bid_id == oi;
    if shared && let Some(oi) = index {
        return LighterFeeReport::with(
            "invalid",
            index,
            format!("order_index {oi} is listed for another client order"),
        );
    }
    if entry.filled.is_zero() {
        if let Some(oi) = index
            && let Some(trade) = trades.by_id.values().find(|t| names(t, oi))
        {
            return LighterFeeReport::with(
                "invalid",
                index,
                format!(
                    "trade {} names order_index {oi}, but the order filled nothing",
                    trade.trade_id
                ),
            );
        }
        return LighterFeeReport {
            status: "no_fill",
            source: "frozen_trades",
            order_index: index,
            ..LighterFeeReport::not_provided()
        };
    }
    let Some(oi) = index else {
        return LighterFeeReport::with(
            "unknown",
            None,
            "the history entry has no order_index; no trade can be bound to the order".into(),
        );
    };
    let naming: Vec<&Trade> = trades.by_id.values().filter(|t| names(t, oi)).collect();
    for trade in &naming {
        let tid = trade.trade_id;
        let is_ask = trade.ask_id == oi;
        let (account, client) = if is_ask {
            (trade.ask_account_id, trade.ask_client_id)
        } else {
            (trade.bid_account_id, trade.bid_client_id)
        };
        let side = |ask: bool| if ask { "ask" } else { "bid" };
        let contradiction = if trade.market_id != evidence.market_id {
            Some(format!(
                "trade {tid} on market {} names order_index {oi} of an order on market {}",
                trade.market_id, evidence.market_id
            ))
        } else if trade.ask_id == trade.bid_id {
            Some(format!("trade {tid} names order_index {oi} on both sides"))
        } else if let Some(account) = account
            && account != evidence.account_index
        {
            Some(format!(
                "trade {tid} lists order_index {oi} for account {account}, not {}",
                evidence.account_index
            ))
        } else if let Some(client) = client.filter(|c| *c != 0)
            && client != id
        {
            Some(format!(
                "trade {tid} lists order_index {oi} with client order {client}, not {id}"
            ))
        } else if let Some(history_ask) = entry.is_ask
            && history_ask != is_ask
        {
            Some(format!(
                "trade {tid} lists order_index {oi} as {}, its history entry as {}",
                side(is_ask),
                side(history_ask)
            ))
        } else {
            None
        };
        if let Some(reason) = contradiction {
            return LighterFeeReport::with("invalid", index, reason);
        }
    }
    let mut covered = ExactSum::default();
    for trade in &naming {
        covered.add(trade.size);
    }
    let covered_text = covered
        .exact()
        .map_or_else(|exact| exact, |d| d.to_string());
    let filled = entry.filled;
    let trade = match (covered.cmp_to(filled), naming.as_slice()) {
        (Ordering::Greater, _) => {
            return LighterFeeReport::with(
                "invalid",
                index,
                format!(
                    "trades for order_index {oi} total {covered_text}, more than the order's filled {filled}"
                ),
            );
        }
        (Ordering::Equal, &[trade]) => trade,
        (Ordering::Equal, _) => {
            return LighterFeeReport::with(
                "unknown",
                index,
                format!(
                    "the fill of {filled} is split over {} trades; only a single covering trade is replayed",
                    naming.len()
                ),
            );
        }
        (Ordering::Less, []) => {
            return LighterFeeReport::with(
                "unknown",
                index,
                format!("no trade names order_index {oi}; the fill of {filled} is not covered"),
            );
        }
        (Ordering::Less, _) => {
            return LighterFeeReport::with(
                "unknown",
                index,
                format!(
                    "trades for order_index {oi} cover {covered_text} of the order's filled {filled}"
                ),
            );
        }
    };
    rate_view(trade, oi, entry.time_in_force.as_deref())
}

/// The role and declared rate of one bound, covering trade. Only a plain
/// `trade` with a known maker side consistent with the order's time in
/// force, no integrator fee and an explicit integer rate yields an estimate.
fn rate_view(trade: &Trade, oi: i64, time_in_force: Option<&str>) -> LighterFeeReport {
    let is_ask = trade.ask_id == oi;
    let mut report = LighterFeeReport {
        status: "unknown",
        source: "frozen_trades",
        order_index: Some(oi),
        trade_id: Some(trade.trade_id),
        side: Some(if is_ask { "ask" } else { "bid" }),
        price: Some(trade.price),
        size: Some(trade.size),
        ..LighterFeeReport::not_provided()
    };
    match trade.kind.as_deref() {
        Some("trade") => {}
        kind => {
            // Liquidation and deleverage fees follow other rules.
            report.reasons.push(format!(
                "trade type {} is not `trade`; its fee is not rate-derived",
                kind.unwrap_or("(absent)")
            ));
            return report;
        }
    }
    // A mismatching account was already rejected; an absent one leaves the
    // side unconfirmed by the trade itself.
    let account_field = if is_ask {
        ("ask_account_id", trade.ask_account_id)
    } else {
        ("bid_account_id", trade.bid_account_id)
    };
    if account_field.1.is_none() {
        report.reasons.push(format!(
            "{} is absent; the trade does not confirm the order's side is this account",
            account_field.0
        ));
        return report;
    }
    let Some(maker_ask) = trade.is_maker_ask else {
        report
            .reasons
            .push("is_maker_ask is absent; the account's role is unknown".into());
        return report;
    };
    let maker = is_ask == maker_ask;
    let role = if maker { "maker" } else { "taker" };
    report.role = Some(role);
    // The order's own time in force is the only independent check of the
    // role the trade implies.
    let forbidden = match time_in_force {
        Some("post-only") => Some("taker"),
        Some("immediate-or-cancel") => Some("maker"),
        _ => None,
    };
    if forbidden == Some(role) {
        report.status = "invalid";
        report.reasons.push(format!(
            "trade makes the account the {role}, but the order is {}",
            time_in_force.unwrap_or_default()
        ));
        return report;
    }
    let (field, raw, integrator) = if maker {
        ("maker_fee", &trade.maker_fee, &trade.integrator_maker_fee)
    } else {
        ("taker_fee", &trade.taker_fee, &trade.integrator_taker_fee)
    };
    report.rate_field = Some(field);
    // Absent, null or 0 means no integrator fee; anything else is a charge
    // this estimate does not model.
    if !matches!(integrator, None | Some(Value::Null))
        && integrator.as_ref().and_then(Value::as_i64) != Some(0)
    {
        report.reasons.push(format!(
            "integrator_{field} {} is declared; the estimate covers only {field}",
            integrator
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default()
        ));
        return report;
    }
    let rate = match raw {
        None => {
            report.rate_state = Some("absent");
            report.reasons.push(format!(
                "{field} is absent; Lighter omits zero or empty fields, so the rate is unknown"
            ));
            return report;
        }
        Some(Value::Null) => {
            report.rate_state = Some("null");
            report.reasons.push(format!("{field} is null"));
            return report;
        }
        // A range, not `abs()`: `i64::MIN.abs()` overflows.
        Some(value) => match value
            .as_i64()
            .filter(|r| (-FEE_TICK..=FEE_TICK).contains(r))
        {
            Some(rate) => rate,
            None => {
                report.status = "invalid";
                report.rate_state = Some("invalid");
                report.reasons.push(format!(
                    "{field} {value} is not an integer rate within ±{FEE_TICK} ppm"
                ));
                return report;
            }
        },
    };
    report.rate_state = Some("explicit");
    report.rate_ppm = Some(rate);
    match estimate(rate, trade.price, trade.size) {
        Ok(fee) => {
            report.status = "rate_derived";
            report.estimated_fee = Some(fee);
        }
        Err(reason) => {
            report.status = "invalid";
            report.reasons.push(reason);
        }
    }
    report
}
