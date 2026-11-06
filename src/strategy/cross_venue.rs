//! Cost-aware cross-venue planning, deliberately separate from order submission.
//!
//! A pair of derivatives is a basis/convergence position, not guaranteed profit.
//! Plans reserve both entry and exit costs, a stressed exit basis, and funding
//! uncertainty. Monetary values use the common quote/settlement currency; `_usd`
//! field names assume that currency is USD-denominated and do not perform FX.
//!
//! Funding rates are fractional PER-PAYMENT rates, positive when longs pay.
//! Annualized statistics and asymmetric swap financing must be normalized by a
//! venue-specific adapter, or rejected, rather than silently treated as perps.

use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde::{Deserialize, Serialize};
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContractKind {
    Spot,
    LinearPerpetual,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstrumentSpec {
    pub base: String,
    pub quote: String,
    pub settlement: String,
    /// Canonical underlying/index identity verified by the adapter.
    pub reference_index: String,
    pub kind: ContractKind,
    /// Base units per native venue quantity unit. Inverse/quanto contracts are unsupported.
    pub contract_multiplier: f64,
    pub price_tick: f64,
    pub quantity_step: f64,
    pub min_quantity: f64,
    pub min_notional_usd: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionCosts {
    pub entry_fee_bps: f64,
    pub exit_fee_bps: f64,
    pub entry_slippage_bps: f64,
    pub exit_slippage_bps: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum QuoteKind {
    OrderBook,
    /// A size-specific executable RFQ; resizing it requires a new quote.
    FirmRfq {
        quote_id: String,
        expires_at_ms: u64,
        quantity: f64,
    },
    Indicative,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FundingEstimate {
    /// Fractional rate for ONE interval, never APR. Positive means longs pay.
    pub rate: f64,
    pub interval_ms: u64,
    pub next_payment_ms: u64,
    pub observed_at_ms: u64,
    /// Worst-case uncertainty in the rate for each projected payment, in bps.
    pub uncertainty_bps: f64,
    /// Price used by the venue to calculate funding notional (often oracle price).
    pub oracle_price: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VenueSnapshot {
    pub venue: String,
    /// Matching-book domain, not frontend name (e.g. hyperliquid:core vs hyperliquid:io).
    pub liquidity_domain: String,
    pub instrument: InstrumentSpec,
    pub quote_kind: QuoteKind,
    pub bid: f64,
    pub ask: f64,
    /// Quantity in venue-native units; contract_multiplier converts to base.
    pub bid_size: f64,
    pub ask_size: f64,
    pub timestamp_ms: u64,
    /// Already-approved risk capacity, including collateral and existing exposure.
    /// The planner does not infer borrowing capacity from an account balance.
    pub available_notional_usd: f64,
    pub costs: ExecutionCosts,
    pub funding: Option<FundingEstimate>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PlannerConfig {
    pub max_quote_age_ms: u64,
    pub max_quote_skew_ms: u64,
    pub max_funding_age_ms: u64,
    pub min_quote_validity_ms: u64,
    pub max_leg_notional_usd: f64,
    pub max_base_quantity: f64,
    pub depth_fraction: f64,
    pub min_net_edge_bps: f64,
    /// Stressed closing basis, separate from fees/slippage. Not a guarantee.
    pub exit_basis_reserve_bps: f64,
    pub funding_horizon_ms: u64,
    /// Size each plan for the possibility that ONLY ONE leg fills.
    pub max_unhedged_notional_usd: f64,
}

impl Default for PlannerConfig {
    fn default() -> Self {
        Self {
            max_quote_age_ms: 1_000,
            max_quote_skew_ms: 250,
            max_funding_age_ms: 60_000,
            min_quote_validity_ms: 1_000,
            max_leg_notional_usd: 100.0,
            max_base_quantity: 1.0,
            depth_fraction: 0.25,
            min_net_edge_bps: 5.0,
            exit_basis_reserve_bps: 10.0,
            funding_horizon_ms: 8 * 60 * 60 * 1_000,
            max_unhedged_notional_usd: 25.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanKind {
    SpreadCapture,
    FundingCarry,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlannedLeg {
    pub venue: String,
    pub liquidity_domain: String,
    pub native_quantity: f64,
    pub limit_price: f64,
    pub quote_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TradePlan {
    pub kind: PlanKind,
    pub base: String,
    pub settlement: String,
    pub buy: PlannedLeg,
    pub sell: PlannedLeg,
    pub base_quantity: f64,
    pub reference_notional_usd: f64,
    pub gross_entry_edge_usd: f64,
    pub entry_cost_usd: f64,
    pub exit_cost_usd: f64,
    pub exit_basis_reserve_usd: f64,
    pub expected_funding_usd: f64,
    pub funding_uncertainty_usd: f64,
    pub net_edge_usd: f64,
    pub net_edge_bps: f64,
    pub buy_funding_payments: u64,
    pub sell_funding_payments: u64,
    /// Time at which inputs must be revalidated; never an authorization to trade.
    pub valid_until_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(rename_all = "snake_case")]
pub enum PlanRejection {
    #[error("invalid planner configuration")]
    InvalidConfig,
    #[error("invalid numeric value or market metadata")]
    InvalidMarket,
    #[error("snapshots are stale, future-dated, or too far apart")]
    StaleQuotes,
    #[error("same venue or shared liquidity domain")]
    SharedLiquidity,
    #[error("incompatible contract, base, quote or settlement currency")]
    IncompatibleContracts,
    #[error("indicative prices are not executable")]
    IndicativeQuote,
    #[error("firm RFQ expired, too short-lived, or incompatible with planned size")]
    InvalidRfq,
    #[error("funding input is missing, stale or not a future interval rate")]
    InvalidFunding,
    #[error("insufficient depth, approved capacity or common tradable quantity")]
    InsufficientCapacity,
    #[error("net edge does not cover the configured cost/risk threshold")]
    InsufficientEdge,
    #[error("decimal sizing or cost arithmetic overflow")]
    Arithmetic,
}

type Result<T> = std::result::Result<T, PlanRejection>;

impl PlannerConfig {
    pub fn validate(&self) -> Result<()> {
        if self.max_quote_age_ms == 0
            || self.max_quote_skew_ms > self.max_quote_age_ms
            || self.max_funding_age_ms == 0
            || self.min_quote_validity_ms == 0
            || self.funding_horizon_ms == 0
            || self.funding_horizon_ms > 30 * 24 * 60 * 60 * 1_000
            || !positive(&[
                self.max_leg_notional_usd,
                self.max_base_quantity,
                self.depth_fraction,
                self.max_unhedged_notional_usd,
            ])
            || self.depth_fraction > 1.0
            || !nonnegative(&[self.min_net_edge_bps, self.exit_basis_reserve_bps])
        {
            return Err(PlanRejection::InvalidConfig);
        }
        Ok(())
    }
}

fn positive(values: &[f64]) -> bool {
    values.iter().all(|v| v.is_finite() && *v > 0.0)
}
fn nonnegative(values: &[f64]) -> bool {
    values.iter().all(|v| v.is_finite() && *v >= 0.0)
}
fn recent(timestamp: u64, now: u64, max_age: u64) -> bool {
    timestamp > 0 && timestamp <= now && now - timestamp <= max_age
}

impl VenueSnapshot {
    /// Validate a source snapshot, without making a trading decision.
    pub fn validate(&self, config: &PlannerConfig, now_ms: u64) -> Result<()> {
        let i = &self.instrument;
        if self.venue.trim().is_empty()
            || self.liquidity_domain.trim().is_empty()
            || i.base.trim().is_empty()
            || i.quote.trim().is_empty()
            || i.reference_index.trim().is_empty()
            || i.quote != i.settlement
            || !positive(&[
                self.bid,
                self.ask,
                self.bid_size,
                self.ask_size,
                i.contract_multiplier,
                i.price_tick,
                i.quantity_step,
            ])
            || !nonnegative(&[
                i.min_quantity,
                i.min_notional_usd,
                self.available_notional_usd,
            ])
            || self.bid >= self.ask
            || !nonnegative(&[
                self.costs.entry_fee_bps,
                self.costs.exit_fee_bps,
                self.costs.entry_slippage_bps,
                self.costs.exit_slippage_bps,
            ])
            || self.costs.entry_slippage_bps >= 10_000.0
            || self.costs.exit_slippage_bps >= 10_000.0
        {
            return Err(PlanRejection::InvalidMarket);
        }
        if !recent(self.timestamp_ms, now_ms, config.max_quote_age_ms) {
            return Err(PlanRejection::StaleQuotes);
        }
        match &self.quote_kind {
            QuoteKind::Indicative => return Err(PlanRejection::IndicativeQuote),
            QuoteKind::FirmRfq {
                quote_id,
                expires_at_ms,
                quantity,
            } => {
                if quote_id.trim().is_empty()
                    || !positive(&[*quantity])
                    || expires_at_ms.saturating_sub(now_ms) < config.min_quote_validity_ms
                {
                    return Err(PlanRejection::InvalidRfq);
                }
            }
            QuoteKind::OrderBook => {}
        }
        Ok(())
    }
}

fn decimal(value: f64) -> Result<Decimal> {
    Decimal::from_str(&value.to_string()).map_err(|_| PlanRejection::Arithmetic)
}
fn float(value: Decimal) -> Result<f64> {
    value
        .to_f64()
        .filter(|v| v.is_finite())
        .ok_or(PlanRejection::Arithmetic)
}
fn mul(a: Decimal, b: Decimal) -> Result<Decimal> {
    a.checked_mul(b).ok_or(PlanRejection::Arithmetic)
}
fn div(a: Decimal, b: Decimal) -> Result<Decimal> {
    a.checked_div(b).ok_or(PlanRejection::Arithmetic)
}
fn gcd(mut a: i128, mut b: i128) -> i128 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// Exact least common multiple of base-unit lot sizes, including nondecimal
/// contract multipliers and steps such as 0.002 versus 0.003.
fn common_base_step(a: &InstrumentSpec, b: &InstrumentSpec) -> Result<Decimal> {
    let a = mul(decimal(a.quantity_step)?, decimal(a.contract_multiplier)?)?.normalize();
    let b = mul(decimal(b.quantity_step)?, decimal(b.contract_multiplier)?)?.normalize();
    let scale = a.scale().max(b.scale());
    // Keep risk sizing bounded and auditable; sub-picounit contracts need an adapter.
    if scale > 12 {
        return Err(PlanRejection::Arithmetic);
    }
    let scaled = |value: Decimal| {
        value
            .mantissa()
            .checked_mul(10_i128.pow(scale - value.scale()))
            .ok_or(PlanRejection::Arithmetic)
    };
    let (a, b) = (scaled(a)?, scaled(b)?);
    if a <= 0 || b <= 0 {
        return Err(PlanRejection::Arithmetic);
    }
    let lcm = (a / gcd(a, b))
        .checked_mul(b)
        .ok_or(PlanRejection::Arithmetic)?;
    Decimal::try_from_i128_with_scale(lcm, scale).map_err(|_| PlanRejection::Arithmetic)
}

fn execution_price(snapshot: &VenueSnapshot, buy: bool) -> Result<f64> {
    let price = if buy { snapshot.ask } else { snapshot.bid };
    if matches!(snapshot.quote_kind, QuoteKind::FirmRfq { .. }) {
        // A firm RFQ is already size-specific and is accepted by quote ID.
        return Ok(price);
    }
    let direction = if buy { 1.0 } else { -1.0 };
    let padded = mul(
        decimal(price)?,
        decimal(1.0 + direction * snapshot.costs.entry_slippage_bps / 10_000.0)?,
    )?;
    let tick = decimal(snapshot.instrument.price_tick)?;
    let remainder = padded.checked_rem(tick).ok_or(PlanRejection::Arithmetic)?;
    let mut aligned = padded
        .checked_sub(remainder)
        .ok_or(PlanRejection::Arithmetic)?;
    if buy && remainder > Decimal::ZERO {
        aligned = aligned.checked_add(tick).ok_or(PlanRejection::Arithmetic)?;
    }
    let result = float(aligned)?;
    if result <= 0.0 {
        return Err(PlanRejection::InsufficientCapacity);
    }
    Ok(result)
}

fn executable_capacity(
    snapshot: &VenueSnapshot,
    buy: bool,
    depth_fraction: f64,
) -> Result<Decimal> {
    let displayed = if buy {
        snapshot.ask_size
    } else {
        snapshot.bid_size
    };
    let native = match snapshot.quote_kind {
        QuoteKind::FirmRfq { quantity, .. } => {
            if quantity > displayed {
                return Err(PlanRejection::InvalidRfq);
            }
            decimal(quantity)?
        }
        _ => mul(decimal(displayed)?, decimal(depth_fraction)?)?,
    };
    mul(native, decimal(snapshot.instrument.contract_multiplier)?)
}

fn quote_id(snapshot: &VenueSnapshot) -> Option<String> {
    match &snapshot.quote_kind {
        QuoteKind::FirmRfq { quote_id, .. } => Some(quote_id.clone()),
        _ => None,
    }
}

fn valid_until(snapshot: &VenueSnapshot, max_age: u64) -> u64 {
    let stale_at = snapshot.timestamp_ms.saturating_add(max_age);
    match snapshot.quote_kind {
        QuoteKind::FirmRfq { expires_at_ms, .. } => stale_at.min(expires_at_ms),
        _ => stale_at,
    }
}

fn funding_payments(funding: &FundingEstimate, config: &PlannerConfig, now: u64) -> Result<u64> {
    if !funding.rate.is_finite()
        || funding.rate.abs() > 1.0
        || !positive(&[funding.oracle_price])
        || !nonnegative(&[funding.uncertainty_bps])
        || funding.interval_ms == 0
        || funding.next_payment_ms <= now
        || funding.next_payment_ms - now > funding.interval_ms
        || !recent(funding.observed_at_ms, now, config.max_funding_age_ms)
    {
        return Err(PlanRejection::InvalidFunding);
    }
    let end = now
        .checked_add(config.funding_horizon_ms)
        .ok_or(PlanRejection::Arithmetic)?;
    Ok(if funding.next_payment_ms > end {
        0
    } else {
        1 + (end - funding.next_payment_ms) / funding.interval_ms
    })
}

/// Evaluate one long/buy versus short/sell orientation. A plan is a conservative
/// scenario, not an order or an assertion that two fills will be atomic.
/// For derivatives both strategies model funding throughout funding_horizon_ms.
/// Spot sell capacity must represent inventory or explicitly approved borrow.
pub fn evaluate_pair(
    buy: &VenueSnapshot,
    sell: &VenueSnapshot,
    config: &PlannerConfig,
    now_ms: u64,
    kind: PlanKind,
) -> Result<TradePlan> {
    config.validate()?;
    buy.validate(config, now_ms)?;
    sell.validate(config, now_ms)?;
    if buy.venue == sell.venue || buy.liquidity_domain == sell.liquidity_domain {
        return Err(PlanRejection::SharedLiquidity);
    }
    if buy.instrument.base != sell.instrument.base
        || buy.instrument.quote != sell.instrument.quote
        || buy.instrument.settlement != sell.instrument.settlement
        || buy.instrument.reference_index != sell.instrument.reference_index
        || buy.instrument.kind != sell.instrument.kind
        || (kind == PlanKind::FundingCarry && buy.instrument.kind != ContractKind::LinearPerpetual)
    {
        return Err(PlanRejection::IncompatibleContracts);
    }
    if buy.timestamp_ms.abs_diff(sell.timestamp_ms) > config.max_quote_skew_ms {
        return Err(PlanRejection::StaleQuotes);
    }
    let buy_limit = execution_price(buy, true)?;
    let sell_limit = execution_price(sell, false)?;
    let max_price = buy_limit.max(sell_limit);
    let buy_multiplier = decimal(buy.instrument.contract_multiplier)?;
    let sell_multiplier = decimal(sell.instrument.contract_multiplier)?;
    let mut capacity = decimal(config.max_base_quantity)?;
    for limit in [
        div(
            decimal(
                config
                    .max_leg_notional_usd
                    .min(config.max_unhedged_notional_usd),
            )?,
            decimal(max_price)?,
        )?,
        div(decimal(buy.available_notional_usd)?, decimal(buy_limit)?)?,
        div(decimal(sell.available_notional_usd)?, decimal(sell_limit)?)?,
        executable_capacity(buy, true, config.depth_fraction)?,
        executable_capacity(sell, false, config.depth_fraction)?,
    ] {
        capacity = capacity.min(limit);
    }
    let step = common_base_step(&buy.instrument, &sell.instrument)?;
    let remainder = capacity
        .checked_rem(step)
        .ok_or(PlanRejection::Arithmetic)?;
    let base_quantity = capacity
        .checked_sub(remainder)
        .ok_or(PlanRejection::Arithmetic)?;
    if base_quantity <= Decimal::ZERO {
        return Err(PlanRejection::InsufficientCapacity);
    }
    let buy_quantity = div(base_quantity, buy_multiplier)?;
    let sell_quantity = div(base_quantity, sell_multiplier)?;
    for (snapshot, native, price) in [
        (buy, buy_quantity, buy_limit),
        (sell, sell_quantity, sell_limit),
    ] {
        if native < decimal(snapshot.instrument.min_quantity)?
            || mul(base_quantity, decimal(price)?)? < decimal(snapshot.instrument.min_notional_usd)?
        {
            return Err(PlanRejection::InsufficientCapacity);
        }
        if let QuoteKind::FirmRfq { quantity, .. } = snapshot.quote_kind
            && native != decimal(quantity)?
        {
            return Err(PlanRejection::InvalidRfq);
        }
    }
    let size = float(base_quantity)?;
    let reference = buy.ask * 0.5 + sell.bid * 0.5;
    let notional = size * reference;
    let gross_entry = size * (sell.bid - buy.ask);
    let entry_cost = size * (buy_limit - buy.ask + sell.bid - sell_limit)
        + size * (buy_limit * buy.costs.entry_fee_bps + sell_limit * sell.costs.entry_fee_bps)
            / 10_000.0;
    let exit_cost = size
        * (buy.ask * (buy.costs.exit_fee_bps + buy.costs.exit_slippage_bps)
            + sell.bid * (sell.costs.exit_fee_bps + sell.costs.exit_slippage_bps))
        / 10_000.0;
    let basis_reserve = notional * config.exit_basis_reserve_bps / 10_000.0;
    let mut expires_at =
        valid_until(buy, config.max_quote_age_ms).min(valid_until(sell, config.max_quote_age_ms));
    let (expected_funding, uncertainty, buy_payments, sell_payments) =
        if buy.instrument.kind == ContractKind::LinearPerpetual {
            let buy_funding = buy.funding.as_ref().ok_or(PlanRejection::InvalidFunding)?;
            let sell_funding = sell.funding.as_ref().ok_or(PlanRejection::InvalidFunding)?;
            let bp = funding_payments(buy_funding, config, now_ms)?;
            let sp = funding_payments(sell_funding, config, now_ms)?;
            for funding in [buy_funding, sell_funding] {
                expires_at = expires_at.min(funding.next_payment_ms).min(
                    funding
                        .observed_at_ms
                        .saturating_add(config.max_funding_age_ms),
                );
            }
            let expected = size
                * (sell_funding.oracle_price * sell_funding.rate * sp as f64
                    - buy_funding.oracle_price * buy_funding.rate * bp as f64);
            let uncertainty = size
                * (sell_funding.oracle_price * sell_funding.uncertainty_bps * sp as f64
                    + buy_funding.oracle_price * buy_funding.uncertainty_bps * bp as f64)
                / 10_000.0;
            (expected, uncertainty, bp, sp)
        } else {
            (0.0, 0.0, 0, 0)
        };
    let net = gross_entry - entry_cost - exit_cost - basis_reserve + expected_funding - uncertainty;
    let net_bps = net / notional * 10_000.0;
    if ![
        size,
        notional,
        gross_entry,
        entry_cost,
        exit_cost,
        basis_reserve,
        expected_funding,
        uncertainty,
        net,
        net_bps,
    ]
    .iter()
    .all(|v| v.is_finite())
        || notional <= 0.0
    {
        return Err(PlanRejection::Arithmetic);
    }
    if net <= 0.0
        || net_bps < config.min_net_edge_bps
        || (kind == PlanKind::FundingCarry && expected_funding <= uncertainty)
        || (kind == PlanKind::SpreadCapture && gross_entry <= 0.0)
    {
        return Err(PlanRejection::InsufficientEdge);
    }
    Ok(TradePlan {
        kind,
        base: buy.instrument.base.clone(),
        settlement: buy.instrument.settlement.clone(),
        buy: PlannedLeg {
            venue: buy.venue.clone(),
            liquidity_domain: buy.liquidity_domain.clone(),
            native_quantity: float(buy_quantity)?,
            limit_price: buy_limit,
            quote_id: quote_id(buy),
        },
        sell: PlannedLeg {
            venue: sell.venue.clone(),
            liquidity_domain: sell.liquidity_domain.clone(),
            native_quantity: float(sell_quantity)?,
            limit_price: sell_limit,
            quote_id: quote_id(sell),
        },
        base_quantity: size,
        reference_notional_usd: notional,
        gross_entry_edge_usd: gross_entry,
        entry_cost_usd: entry_cost,
        exit_cost_usd: exit_cost,
        exit_basis_reserve_usd: basis_reserve,
        expected_funding_usd: expected_funding,
        funding_uncertainty_usd: uncertainty,
        net_edge_usd: net,
        net_edge_bps: net_bps,
        buy_funding_payments: buy_payments,
        sell_funding_payments: sell_payments,
        valid_until_ms: expires_at,
    })
}

fn scan(
    snapshots: &[VenueSnapshot],
    config: &PlannerConfig,
    now_ms: u64,
    kind: PlanKind,
) -> Result<Vec<TradePlan>> {
    config.validate()?;
    // Multiple snapshots from one venue/instrument could otherwise be selected
    // opportunistically. Callers must keep one current snapshot per source.
    for (index, snapshot) in snapshots.iter().enumerate() {
        if snapshots[..index].iter().any(|other| {
            other.venue == snapshot.venue && other.instrument.base == snapshot.instrument.base
        }) {
            return Err(PlanRejection::InvalidMarket);
        }
    }
    let mut plans = Vec::new();
    for (buy_index, buy) in snapshots.iter().enumerate() {
        for (sell_index, sell) in snapshots.iter().enumerate() {
            if buy_index != sell_index
                && let Ok(plan) = evaluate_pair(buy, sell, config, now_ms, kind)
            {
                plans.push(plan);
            }
        }
    }
    plans.sort_by(|a, b| b.net_edge_usd.total_cmp(&a.net_edge_usd));
    Ok(plans)
}

pub fn plan_spread(
    snapshots: &[VenueSnapshot],
    config: &PlannerConfig,
    now_ms: u64,
) -> Result<Vec<TradePlan>> {
    scan(snapshots, config, now_ms, PlanKind::SpreadCapture)
}

pub fn plan_funding(
    snapshots: &[VenueSnapshot],
    config: &PlannerConfig,
    now_ms: u64,
) -> Result<Vec<TradePlan>> {
    scan(snapshots, config, now_ms, PlanKind::FundingCarry)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    const NOW: u64 = 1_000_000;

    pub(crate) fn market(venue: &str, bid: f64, ask: f64) -> VenueSnapshot {
        VenueSnapshot {
            venue: venue.into(),
            liquidity_domain: venue.into(),
            instrument: InstrumentSpec {
                base: "ETH".into(),
                quote: "USDC".into(),
                settlement: "USDC".into(),
                reference_index: "ETH/USD".into(),
                kind: ContractKind::Spot,
                contract_multiplier: 1.0,
                price_tick: 0.01,
                quantity_step: 0.001,
                min_quantity: 0.001,
                min_notional_usd: 1.0,
            },
            quote_kind: QuoteKind::OrderBook,
            bid,
            ask,
            bid_size: 10.0,
            ask_size: 10.0,
            timestamp_ms: NOW,
            available_notional_usd: 1_000.0,
            costs: ExecutionCosts {
                entry_fee_bps: 1.0,
                exit_fee_bps: 1.0,
                entry_slippage_bps: 1.0,
                exit_slippage_bps: 1.0,
            },
            funding: None,
        }
    }

    fn config() -> PlannerConfig {
        PlannerConfig {
            max_leg_notional_usd: 1_000.0,
            max_unhedged_notional_usd: 1_000.0,
            max_base_quantity: 1.0,
            depth_fraction: 1.0,
            exit_basis_reserve_bps: 2.0,
            ..Default::default()
        }
    }

    fn funding(rate: f64, interval_ms: u64) -> FundingEstimate {
        FundingEstimate {
            rate,
            interval_ms,
            next_payment_ms: NOW + interval_ms,
            observed_at_ms: NOW,
            uncertainty_bps: 0.1,
            oracle_price: 100.0,
        }
    }

    #[test]
    fn gross_discrepancy_must_cover_four_fees_and_both_slippage_budgets() {
        let buy = market("lighter", 99.98, 100.0);
        let sell = market("hyperliquid", 100.05, 100.07);
        assert_eq!(
            evaluate_pair(&buy, &sell, &config(), NOW, PlanKind::SpreadCapture),
            Err(PlanRejection::InsufficientEdge)
        );
        let profitable = market("hyperliquid", 101.0, 101.02);
        let plan =
            evaluate_pair(&buy, &profitable, &config(), NOW, PlanKind::SpreadCapture).unwrap();
        assert!(plan.net_edge_usd < plan.gross_entry_edge_usd);
        assert!(
            plan.entry_cost_usd > 0.0
                && plan.exit_cost_usd > 0.0
                && plan.exit_basis_reserve_usd > 0.0
        );
        assert!(plan.buy.limit_price >= buy.ask && plan.sell.limit_price <= profitable.bid);
    }

    #[test]
    fn liquidity_domains_prevent_frontend_alias_arbitrage() {
        let buy = market("hyperliquid", 99.0, 100.0);
        let mut sell = market("entropy", 101.0, 102.0);
        sell.liquidity_domain = buy.liquidity_domain.clone();
        assert_eq!(
            evaluate_pair(&buy, &sell, &config(), NOW, PlanKind::SpreadCapture),
            Err(PlanRejection::SharedLiquidity)
        );
        sell.liquidity_domain = "hyperliquid:io".into();
        assert!(evaluate_pair(&buy, &sell, &config(), NOW, PlanKind::SpreadCapture).is_ok());
    }

    #[test]
    fn indicative_and_expired_quotes_cannot_generate_plans() {
        let buy = market("lighter", 99.0, 100.0);
        let mut sell = market("variational", 101.0, 102.0);
        sell.quote_kind = QuoteKind::Indicative;
        assert_eq!(
            evaluate_pair(&buy, &sell, &config(), NOW, PlanKind::SpreadCapture),
            Err(PlanRejection::IndicativeQuote)
        );
        sell.quote_kind = QuoteKind::FirmRfq {
            quote_id: "q1".into(),
            expires_at_ms: NOW + 500,
            quantity: 1.0,
        };
        assert_eq!(
            evaluate_pair(&buy, &sell, &config(), NOW, PlanKind::SpreadCapture),
            Err(PlanRejection::InvalidRfq)
        );
        sell.quote_kind = QuoteKind::FirmRfq {
            quote_id: "q1".into(),
            expires_at_ms: NOW + 5_000,
            quantity: 0.5,
        };
        let plan = evaluate_pair(&buy, &sell, &config(), NOW, PlanKind::SpreadCapture).unwrap();
        assert_eq!(plan.sell.native_quantity, 0.5);
        assert_eq!(plan.sell.quote_id.as_deref(), Some("q1"));
        let small = PlannerConfig {
            max_base_quantity: 0.25,
            ..config()
        };
        assert_eq!(
            evaluate_pair(&buy, &sell, &small, NOW, PlanKind::SpreadCapture),
            Err(PlanRejection::InvalidRfq)
        );
    }

    #[test]
    fn quotes_must_be_finite_fresh_and_synchronized() {
        let buy = market("lighter", 99.0, 100.0);
        let mut sell = market("hyperliquid", 101.0, 102.0);
        for timestamp in [0, NOW + 1, NOW - 1_001, NOW - 300] {
            sell.timestamp_ms = timestamp;
            assert_eq!(
                evaluate_pair(&buy, &sell, &config(), NOW, PlanKind::SpreadCapture),
                Err(PlanRejection::StaleQuotes)
            );
        }
        sell.timestamp_ms = NOW;
        for bad in [f64::NAN, f64::INFINITY, -1.0, 0.0] {
            sell.bid_size = bad;
            assert_eq!(
                evaluate_pair(&buy, &sell, &config(), NOW, PlanKind::SpreadCapture),
                Err(PlanRejection::InvalidMarket)
            );
        }
    }

    #[test]
    fn reference_index_and_settlement_must_match() {
        let buy = market("lighter", 99.0, 100.0);
        let mut sell = market("hyperliquid", 101.0, 102.0);
        sell.instrument.reference_index = "different ETH index".into();
        assert_eq!(
            evaluate_pair(&buy, &sell, &config(), NOW, PlanKind::SpreadCapture),
            Err(PlanRejection::IncompatibleContracts)
        );
        sell.instrument.reference_index = buy.instrument.reference_index.clone();
        sell.instrument.quote = "USDT".into();
        sell.instrument.settlement = "USDT".into();
        assert_eq!(
            evaluate_pair(&buy, &sell, &config(), NOW, PlanKind::SpreadCapture),
            Err(PlanRejection::IncompatibleContracts)
        );
        sell.instrument.quote = "USDC".into();
        assert_eq!(
            evaluate_pair(&buy, &sell, &config(), NOW, PlanKind::SpreadCapture),
            Err(PlanRejection::InvalidMarket)
        );
    }

    #[test]
    fn common_lot_uses_lcm_not_larger_step_and_preserves_base_delta() {
        let mut buy = market("lighter", 99.0, 100.0);
        let mut sell = market("hyperliquid", 101.0, 102.0);
        buy.instrument.quantity_step = 0.002;
        sell.instrument.quantity_step = 0.003;
        let small = PlannerConfig {
            max_base_quantity: 0.017,
            ..config()
        };
        let plan = evaluate_pair(&buy, &sell, &small, NOW, PlanKind::SpreadCapture).unwrap();
        assert_eq!(plan.base_quantity, 0.012);
        sell.instrument.contract_multiplier = 10.0;
        let plan = evaluate_pair(&buy, &sell, &config(), NOW, PlanKind::SpreadCapture).unwrap();
        assert!((plan.buy.native_quantity - plan.sell.native_quantity * 10.0).abs() < 1e-12);
        assert!((plan.base_quantity / 0.03 - (plan.base_quantity / 0.03).round()).abs() < 1e-9);
    }

    #[test]
    fn capacity_and_single_leg_risk_caps_apply_after_price_padding() {
        let buy = market("lighter", 99.0, 100.0);
        let mut sell = market("hyperliquid", 101.0, 102.0);
        sell.available_notional_usd = 15.0;
        let limited = PlannerConfig {
            max_unhedged_notional_usd: 20.0,
            ..config()
        };
        let plan = evaluate_pair(&buy, &sell, &limited, NOW, PlanKind::SpreadCapture).unwrap();
        assert!(plan.base_quantity * plan.sell.limit_price <= 15.0 + 1e-12);
        assert!(plan.base_quantity * plan.buy.limit_price <= 20.0 + 1e-12);
        sell.instrument.min_notional_usd = 16.0;
        assert_eq!(
            evaluate_pair(&buy, &sell, &limited, NOW, PlanKind::SpreadCapture),
            Err(PlanRejection::InsufficientCapacity)
        );
    }

    #[test]
    fn funding_uses_discrete_payments_oracle_notional_and_correct_side() {
        let mut buy = market("lighter", 99.9, 100.0);
        let mut sell = market("hyperliquid", 99.9, 100.0);
        buy.instrument.kind = ContractKind::LinearPerpetual;
        sell.instrument.kind = ContractKind::LinearPerpetual;
        buy.funding = Some(funding(0.0001, 8 * 3_600_000));
        sell.funding = Some(funding(0.001, 3_600_000));
        let plan = evaluate_pair(&buy, &sell, &config(), NOW, PlanKind::FundingCarry).unwrap();
        assert_eq!(
            (plan.buy_funding_payments, plan.sell_funding_payments),
            (1, 8)
        );
        assert!((plan.expected_funding_usd - 0.79).abs() < 1e-9);
        assert_eq!(
            evaluate_pair(&sell, &buy, &config(), NOW, PlanKind::FundingCarry),
            Err(PlanRejection::InsufficientEdge)
        );
        sell.funding.as_mut().unwrap().oracle_price = 50.0;
        let changed = evaluate_pair(&buy, &sell, &config(), NOW, PlanKind::FundingCarry).unwrap();
        assert!((changed.expected_funding_usd - 0.39).abs() < 1e-9);
    }

    #[test]
    fn forecasts_and_holding_interval_risks_are_not_free_profits() {
        let mut buy = market("lighter", 99.9, 100.0);
        let mut sell = market("hyperliquid", 99.9, 100.0);
        buy.instrument.kind = ContractKind::LinearPerpetual;
        sell.instrument.kind = ContractKind::LinearPerpetual;
        buy.funding = Some(funding(0.0001, 3_600_000));
        sell.funding = Some(funding(0.001, 3_600_000));
        sell.funding.as_mut().unwrap().uncertainty_bps = 10.0;
        assert_eq!(
            evaluate_pair(&buy, &sell, &config(), NOW, PlanKind::FundingCarry),
            Err(PlanRejection::InsufficientEdge)
        );
        sell.funding.as_mut().unwrap().next_payment_ms = NOW;
        assert_eq!(
            evaluate_pair(&buy, &sell, &config(), NOW, PlanKind::FundingCarry),
            Err(PlanRejection::InvalidFunding)
        );
        sell.funding = Some(funding(0.001, 3_600_000));
        let short_horizon = PlannerConfig {
            funding_horizon_ms: 1_000,
            ..config()
        };
        assert_eq!(
            evaluate_pair(&buy, &sell, &short_horizon, NOW, PlanKind::FundingCarry),
            Err(PlanRejection::InsufficientEdge)
        );
        sell.funding.as_mut().unwrap().observed_at_ms = NOW - 60_001;
        assert_eq!(
            evaluate_pair(&buy, &sell, &config(), NOW, PlanKind::FundingCarry),
            Err(PlanRejection::InvalidFunding)
        );
    }

    #[test]
    fn derivative_spread_cannot_ignore_unfavorable_funding() {
        let mut buy = market("lighter", 99.0, 100.0);
        let mut sell = market("hyperliquid", 101.0, 102.0);
        buy.instrument.kind = ContractKind::LinearPerpetual;
        sell.instrument.kind = ContractKind::LinearPerpetual;
        assert_eq!(
            evaluate_pair(&buy, &sell, &config(), NOW, PlanKind::SpreadCapture),
            Err(PlanRejection::InvalidFunding)
        );
        buy.funding = Some(funding(0.01, 3_600_000));
        sell.funding = Some(funding(0.0, 3_600_000));
        assert_eq!(
            evaluate_pair(&buy, &sell, &config(), NOW, PlanKind::SpreadCapture),
            Err(PlanRejection::InsufficientEdge)
        );
    }

    #[test]
    fn duplicate_sources_and_unknown_json_fields_are_rejected() {
        let buy = market("lighter", 99.0, 100.0);
        assert_eq!(
            plan_spread(&[buy.clone(), buy], &config(), NOW),
            Err(PlanRejection::InvalidMarket)
        );
        assert!(
            serde_json::from_str::<PlannerConfig>(r#"{"max_leg_notional_typo":5000}"#).is_err()
        );
        let market = market("lighter", 99.0, 100.0);
        let serialized = serde_json::to_string(&market).unwrap();
        assert_eq!(
            serde_json::from_str::<VenueSnapshot>(&serialized).unwrap(),
            market
        );
    }
}

#[cfg(test)]
mod benchmark {
    use super::*;
    use std::{hint::black_box, time::Instant};

    #[test]
    #[ignore = "release microbenchmark"]
    fn benchmark_cross_venue_pair() {
        let mut buy = tests::market("lighter", 1999.0, 2000.0);
        let mut sell = tests::market("hyperliquid", 2010.0, 2011.0);
        for snapshot in [&mut buy, &mut sell] {
            snapshot.instrument.kind = ContractKind::LinearPerpetual;
            snapshot.funding = Some(FundingEstimate {
                rate: 0.00003,
                interval_ms: 3_600_000,
                next_payment_ms: snapshot.timestamp_ms + 3_600_000,
                observed_at_ms: snapshot.timestamp_ms,
                uncertainty_bps: 0.1,
                oracle_price: 2000.0,
            });
        }
        let config = PlannerConfig::default();
        let now = buy.timestamp_ms;
        let trials = 100_000;
        let start = Instant::now();
        for _ in 0..trials {
            let plan = evaluate_pair(
                black_box(&buy),
                black_box(&sell),
                black_box(&config),
                black_box(now),
                PlanKind::SpreadCapture,
            )
            .expect("tradable benchmark pair");
            black_box(plan);
        }
        eprintln!(
            "cross-venue perpetual pair: {:.0} ns/evaluation over {trials} evaluations",
            start.elapsed().as_nanos() as f64 / f64::from(trials)
        );
    }
}
