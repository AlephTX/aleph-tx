//! Same-underlying linear perp strategies over synchronized, normalized L2 data.
//! The planner emits bounded IOC intents for research. No transport runs here.

mod paper;
pub use paper::{
    ExitReason, PaperEntryDiagnostics, PaperIocGate, PaperLegExecution, PaperMarketCosts,
    PaperReport, PaperScenario, PaperTrade, run_paper, run_paper_frames,
};

use crate::{
    hedging::{HedgePlan, HedgeSide, LegSpec, common_quantity_step, floor_step},
    strategy::cross_venue::PlannerConfig,
    venues::{OrderSupport, VenueRegistry},
};
use anyhow::{Context, Result, ensure};
use rust_decimal::{Decimal, prelude::ToPrimitive};
use serde::{Deserialize, Serialize};

const BPS: Decimal = Decimal::from_parts(10_000, 0, 0, false, 0);
const MAX_VALUE: Decimal = Decimal::from_parts(1_000_000_000, 0, 0, false, 0);
const DAY_MS: u64 = 86_400_000;
pub(crate) const EQUITY_MINUTE_MS: u64 = 60_000;
const MAX_LEVELS: usize = 20;
const MAX_FUNDING_POINTS: usize = 1024;
const MIN_PRICE: Decimal = Decimal::from_parts(1, 0, 0, false, 12);
const MAX_MULTIPLIER: Decimal = Decimal::from_parts(1_000_000, 0, 0, false, 0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssetClass {
    Crypto,
    Equity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    FundingCarry,
    BasisReversion,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairConfig {
    pub asset_class: AssetClass,
    pub strategy: Strategy,
    pub legs: [MarketTerms; 2],
    #[serde(default)]
    pub planner: PlannerConfig,
    #[serde(default)]
    pub basis: BasisConfig,
    #[serde(default)]
    pub allow_shared_risk_domain: bool,
    /// Time allowed for both legs to fill. Also reserved for the close.
    pub execution_window_ms: u64,
    pub oracle_max_age_ms: u64,
    /// Explicit paper research policy for completed underlying equity bars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub equity_minute_reference_max_age_ms: Option<u64>,
    pub max_oracle_deviation_bps: f64,
    pub equity_close_buffer_ms: u64,
    /// Max distance from the touch, including the protective limit buffer.
    pub max_depth_impact_bps: f64,
    pub limit_buffer_bps: f64,
    /// Charge on gross notional for the full planned hold plus unwind time.
    #[serde(default)]
    pub capital_charge_bps_per_day: f64,
    /// For funding carry, consider exiting after each reachable settlement.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub optimize_funding_horizon: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarketTerms {
    pub venue: String,
    pub market: String,
    /// Canonical asset identity including the share class for equities.
    pub underlying_id: String,
    /// Reviewed cashflow and adjustment convention, including corporate actions.
    pub payoff_id: String,
    pub quote: String,
    pub settlement: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub base_per_contract: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub tick: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub lot: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub min_contracts: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub min_notional: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub taker_fee_bps: Decimal,
    pub funding: FundingSchedule,
}

impl MarketTerms {
    fn base_step(&self) -> Decimal {
        self.lot * self.base_per_contract
    }

    fn min_base(&self, limit: Decimal) -> Decimal {
        let step = self.base_step();
        // Align both exchange minimums to the native size lattice.
        let minimum = (self.min_contracts * self.base_per_contract).max(self.min_notional / limit);
        let remainder = minimum % step;
        if remainder == Decimal::ZERO {
            minimum
        } else {
            minimum - remainder + step
        }
    }
}

/// A verified fixed UTC cadence. Equity markets with irregular intraday
/// funding schedules require a separate adapter/calendar before use here.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FundingSchedule {
    pub interval_ms: u64,
    pub offset_ms: u64,
}

impl FundingSchedule {
    /// Number of payments in (start, end], and the first one if present.
    fn window(self, start: u64, end: u64) -> Option<(u64, u64)> {
        let remainder = start % self.interval_ms;
        let delay = if remainder < self.offset_ms {
            self.offset_ms - remainder
        } else {
            self.interval_ms - (remainder - self.offset_ms)
        };
        let first = start.checked_add(delay)?;
        if first > end {
            return Some((first, 0));
        }
        Some((first, 1 + (end - first) / self.interval_ms))
    }

    pub(crate) fn matches(self, time: u64) -> bool {
        time % self.interval_ms == self.offset_ms
    }

    fn near_payment(self, time: u64, guard_ms: u64) -> bool {
        let distance = (time % self.interval_ms).abs_diff(self.offset_ms);
        guard_ms > 0 && distance.min(self.interval_ms - distance) <= guard_ms
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BasisConfig {
    pub min_samples: u32,
    pub min_sample_ms: u64,
    pub max_sample_gap_ms: u64,
    pub half_life_ms: u64,
    pub volatility_floor_bps: f64,
    pub stress_sigma: f64,
    pub entry_z: f64,
    pub exit_z: f64,
    pub stop_z: f64,
    /// Fraction of the distance to the trailing mean credited on entry.
    pub reversion_fraction: f64,
}

impl Default for BasisConfig {
    fn default() -> Self {
        Self {
            min_samples: 32,
            min_sample_ms: 1_000,
            max_sample_gap_ms: 60_000,
            half_life_ms: 120_000,
            volatility_floor_bps: 1.0,
            stress_sigma: 1.0,
            entry_z: 2.5,
            exit_z: 0.5,
            stop_z: 5.0,
            reversion_fraction: 0.5,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OracleMode {
    LiveExternal,
    Internal,
    LastClose,
    EquityMinuteClose,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Oracle {
    #[serde(with = "rust_decimal::serde::str")]
    pub price: Decimal,
    pub timestamp_ms: u64,
    pub mode: OracleMode,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EquitySession {
    pub observed_at_ms: u64,
    pub opens_at_ms: u64,
    pub closes_at_ms: u64,
    /// Verified coverage of the corporate action calendar and contract terms.
    pub clear_until_ms: u64,
    pub halted: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Level {
    #[serde(with = "rust_decimal::serde::str")]
    pub price: Decimal,
    /// Displayed size in venue-native contract units.
    #[serde(with = "rust_decimal::serde::str")]
    pub contracts: Decimal,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PriceBounds {
    #[serde(with = "rust_decimal::serde::str")]
    pub lower: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub upper: Decimal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Book {
    pub market: String,
    pub timestamp_ms: u64,
    pub tradable: bool,
    pub bids: Vec<Level>,
    pub asks: Vec<Level>,
    pub bounds: Option<PriceBounds>,
    #[serde(with = "rust_decimal::serde::str")]
    pub available_notional: Decimal,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FundingPoint {
    pub at_ms: u64,
    /// Fractional per-payment rate. Positive means longs pay.
    #[serde(with = "rust_decimal::serde::str")]
    pub rate: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub uncertainty_bps: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    /// Funding notional price. This is the mark on venues that settle on mark.
    pub oracle_price: Decimal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FundingForecast {
    pub observed_at_ms: u64,
    pub payments: Vec<FundingPoint>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FundingSettlement {
    pub at_ms: u64,
    #[serde(with = "rust_decimal::serde::str")]
    pub rate: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    /// Verified price actually used to value the position for this payment.
    pub oracle_price: Decimal,
}

impl FundingSettlement {
    pub(crate) fn valid(&self, schedule: FundingSchedule) -> bool {
        self.at_ms > 0
            && schedule.matches(self.at_ms)
            && self.rate.abs() <= Decimal::ONE
            && price(self.oracle_price)
    }
}

/// Cash per long base unit bounded by observed venue ledger entries and an
/// explicit cash reporting tolerance. The short cashflow has opposite sign.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WitnessedSettlement {
    pub at_ms: u64,
    pub exchange_time_ms: u64,
    #[serde(with = "rust_decimal::serde::str")]
    pub rate: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub long_cash_low_per_base: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub long_cash_high_per_base: Decimal,
}

impl WitnessedSettlement {
    pub(crate) fn valid(&self, schedule: FundingSchedule, guard_ms: u64) -> bool {
        let low = self.long_cash_low_per_base;
        let high = self.long_cash_high_per_base;
        guard_ms > 0
            && self.at_ms > 0
            && schedule.matches(self.at_ms)
            && self
                .exchange_time_ms
                .checked_sub(self.at_ms)
                .is_some_and(|lag| lag <= guard_ms)
            && self.rate.abs() <= Decimal::ONE
            && low <= high
            && bounded(low.abs())
            && bounded(high.abs())
            && match self.rate.cmp(&Decimal::ZERO) {
                std::cmp::Ordering::Greater => high <= Decimal::ZERO && low < Decimal::ZERO,
                std::cmp::Ordering::Less => low >= Decimal::ZERO && high > Decimal::ZERO,
                std::cmp::Ordering::Equal => low.is_zero() && high.is_zero(),
            }
    }

    fn cash_bounds(&self, position: Decimal) -> (Decimal, Decimal) {
        let a = position * self.long_cash_low_per_base;
        let b = position * self.long_cash_high_per_base;
        (a.min(b), a.max(b))
    }
}

pub(crate) fn validate_funding_guard(guard_ms: u64, legs: &[MarketTerms; 2]) -> Result<()> {
    ensure!(
        guard_ms <= 60_000
            && legs
                .iter()
                .all(|leg| guard_ms <= leg.funding.interval_ms / 4),
        "funding guard exceeds the allowable settlement window"
    );
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarketFrame {
    pub book: Book,
    pub oracle: Oracle,
    pub forecast: Option<FundingForecast>,
    #[serde(default)]
    pub settled: Vec<FundingSettlement>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub witnessed_settlements: Vec<WitnessedSettlement>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Frame {
    /// Local availability time. Source observations must be at or before it.
    pub now_ms: u64,
    pub reference: Oracle,
    pub equity_session: Option<EquitySession>,
    pub legs: [MarketFrame; 2],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, thiserror::Error)]
#[serde(rename_all = "snake_case")]
pub enum Rejection {
    #[error("book is malformed, stale, or unsynchronized")]
    Book,
    #[error("equity session is closed, halted, or too short for the hold and unwind")]
    Session,
    #[error("oracle is invalid, non-external, or diverges from the common reference")]
    Oracle,
    #[error("an external oracle is temporarily unavailable or stale")]
    OracleStale,
    #[error("paired sample did not advance")]
    SampleCadence,
    #[error("basis estimator has insufficient prior observations")]
    Warmup,
    #[error("basis is outside the entry band")]
    BasisBand,
    #[error("missing, stale, or inconsistent per-payment funding forecast")]
    Funding,
    #[error("no shared lot fits the depth and risk budget")]
    Capacity,
    #[error("conservative net edge is below threshold")]
    Edge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryGate {
    Session,
    BookMissing,
    BookStale,
    BookClock,
    BookInvalid,
    QuoteSkew,
    ReferenceStale,
    ReferenceInvalid,
    VenueOracleStale,
    VenueOracleInvalid,
    VenueOracleDeviation,
    BookReferenceDeviation,
    SampleCadence,
    Warmup,
    BasisBelowEntry,
    BasisBeyondStop,
    EntryWindow,
    FundingForecast,
    Capacity,
    NetEdge,
    LatencyWindow,
    FundingGuard,
}

impl EntryGate {
    fn rejection(self) -> Rejection {
        match self {
            Self::Session => Rejection::Session,
            Self::BookMissing
            | Self::BookStale
            | Self::BookClock
            | Self::BookInvalid
            | Self::QuoteSkew
            | Self::EntryWindow
            | Self::LatencyWindow => Rejection::Book,
            Self::ReferenceStale | Self::VenueOracleStale => Rejection::OracleStale,
            Self::ReferenceInvalid
            | Self::VenueOracleInvalid
            | Self::VenueOracleDeviation
            | Self::BookReferenceDeviation => Rejection::Oracle,
            Self::SampleCadence => Rejection::SampleCadence,
            Self::Warmup => Rejection::Warmup,
            Self::BasisBelowEntry | Self::BasisBeyondStop => Rejection::BasisBand,
            Self::FundingForecast | Self::FundingGuard => Rejection::Funding,
            Self::Capacity => Rejection::Capacity,
            Self::NetEdge => Rejection::Edge,
        }
    }

    fn on_leg(self, leg: usize) -> BlockedEntry {
        BlockedEntry {
            gate: self,
            leg: Some(leg),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct BlockedEntry {
    gate: EntryGate,
    leg: Option<usize>,
}

impl From<EntryGate> for BlockedEntry {
    fn from(gate: EntryGate) -> Self {
        Self { gate, leg: None }
    }
}

type EntryDecision<T> = std::result::Result<T, BlockedEntry>;

#[derive(Debug, Clone, Copy, Serialize)]
pub struct BasisSignal {
    pub observed_bps: f64,
    pub mean_bps: f64,
    pub sigma_bps: f64,
    pub z: f64,
    pub prior_samples: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    pub strategy: Strategy,
    pub buy_leg: usize,
    pub basis: BasisSignal,
    /// Conservative change in the entry basis, in settlement units.
    #[serde(with = "rust_decimal::serde::str")]
    pub modeled_convergence: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub entry_cost: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub exit_cost: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub basis_reserve: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub funding_lower_bound: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub capital_charge: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub net_edge: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub net_edge_bps: Decimal,
    pub hold_until_ms: u64,
    pub unwind_until_ms: u64,
    pub hedge: HedgePlan,
}

type Decision<T> = std::result::Result<T, Rejection>;

fn dec(value: f64) -> Result<Decimal> {
    // Config values use their decimal spelling instead of binary rounding.
    value
        .to_string()
        .parse()
        .context("invalid decimal configuration")
}

fn finite_between(value: f64, low: f64, high: f64) -> bool {
    (low..=high).contains(&value)
}

fn bounded(value: Decimal) -> bool {
    (Decimal::ZERO..=MAX_VALUE).contains(&value)
}

fn price(value: Decimal) -> bool {
    (MIN_PRICE..=MAX_VALUE).contains(&value)
}

fn fresh(timestamp: u64, now: u64, age: u64) -> bool {
    timestamp > 0 && now.checked_sub(timestamp).is_some_and(|delta| delta <= age)
}

fn invalid_oracle_mode(oracle: Oracle) -> Rejection {
    if oracle.timestamp_ms == 0 && oracle.price.is_zero() {
        Rejection::OracleStale
    } else {
        Rejection::Oracle
    }
}

fn oracle_deadline_ms(oracle: Oracle, now: u64, earliest: u64, max_age: u64) -> Decision<u64> {
    if !price(oracle.price) || oracle.timestamp_ms == 0 || oracle.timestamp_ms > now {
        return Err(invalid_oracle_mode(oracle));
    }
    let expiry = oracle.timestamp_ms.saturating_add(max_age);
    if oracle.timestamp_ms < earliest || now > expiry {
        return Err(Rejection::OracleStale);
    }
    Ok(expiry)
}

impl PairConfig {
    pub(crate) fn usable_reference(
        &self,
        reference: Oracle,
        session: Option<EquitySession>,
        now: u64,
    ) -> bool {
        self.reference_deadline_ms(reference, session, now).is_ok()
    }

    fn reference_deadline_ms(
        &self,
        reference: Oracle,
        session: Option<EquitySession>,
        now: u64,
    ) -> Decision<u64> {
        let (max_age, earliest) = match reference.mode {
            OracleMode::LiveExternal => (
                self.oracle_max_age_ms,
                session.map_or(0, |session| session.opens_at_ms),
            ),
            OracleMode::EquityMinuteClose => {
                let (Some(age), Some(session)) = (self.equity_minute_reference_max_age_ms, session)
                else {
                    return Err(Rejection::Oracle);
                };
                if !reference.timestamp_ms.is_multiple_of(EQUITY_MINUTE_MS) {
                    return Err(Rejection::Oracle);
                }
                (age, session.opens_at_ms.saturating_add(EQUITY_MINUTE_MS))
            }
            _ => return Err(invalid_oracle_mode(reference)),
        };
        oracle_deadline_ms(reference, now, earliest, max_age)
    }

    fn validate(&self, registry: &VenueRegistry) -> Result<()> {
        self.planner.validate()?;
        ensure!(
            self.planner.max_base_quantity <= 1e9
                && self.planner.max_leg_notional_usd <= 1e9
                && self.planner.max_unhedged_notional_usd <= 1e9
                && self.planner.exit_basis_reserve_bps <= 10_000.0
                && self.planner.min_net_edge_bps <= 10_000.0,
            "relative-value risk limits out of range"
        );
        ensure!(
            (1..=DAY_MS).contains(&self.execution_window_ms)
                && self.execution_window_ms < self.planner.funding_horizon_ms
                && (1..=DAY_MS).contains(&self.oracle_max_age_ms)
                && self.equity_minute_reference_max_age_ms.is_none_or(|age| {
                    self.asset_class == AssetClass::Equity
                        && (1..=3 * EQUITY_MINUTE_MS).contains(&age)
                })
                && self.equity_close_buffer_ms <= DAY_MS
                && (self.asset_class == AssetClass::Crypto || self.equity_close_buffer_ms > 0)
                && finite_between(self.max_oracle_deviation_bps, 0.01, 1_000.0)
                && finite_between(self.limit_buffer_bps, 0.0, self.max_depth_impact_bps)
                && finite_between(self.max_depth_impact_bps, 0.01, 1_000.0)
                && finite_between(self.capital_charge_bps_per_day, 0.0, 10_000.0),
            "invalid oracle, execution or cost policy"
        );
        let basis = &self.basis;
        ensure!(
            (3..=100_000).contains(&basis.min_samples)
                && basis.min_sample_ms > 0
                && basis.max_sample_gap_ms >= basis.min_sample_ms
                && basis.max_sample_gap_ms <= DAY_MS
                && (1..=30 * DAY_MS).contains(&basis.half_life_ms)
                && finite_between(basis.volatility_floor_bps, 0.01, 1_000.0)
                && finite_between(basis.stress_sigma, 0.0, 20.0)
                && finite_between(basis.exit_z, 0.0, 10.0)
                && finite_between(basis.entry_z, 0.01, 10.0)
                && finite_between(basis.stop_z, 0.01, 20.0)
                && basis.exit_z < basis.entry_z
                && basis.entry_z < basis.stop_z
                && finite_between(basis.reversion_fraction, 0.01, 1.0),
            "invalid basis estimator policy"
        );
        for leg in &self.legs {
            let venue = registry.get(&leg.venue)?;
            ensure!(
                !matches!(venue.order_support, OrderSupport::ReadOnly),
                "{} has no executable order book adapter",
                leg.venue
            );
            if let Some(dex) = &venue.hyperliquid_dex {
                let valid_market = if dex.is_empty() {
                    !leg.market.contains(':')
                } else {
                    leg.market.split_once(':').is_some_and(|(prefix, symbol)| {
                        prefix == dex && !symbol.is_empty() && !symbol.contains(':')
                    })
                };
                ensure!(valid_market, "market does not belong to {}", leg.venue);
            }
            ensure!(
                [&leg.market, &leg.underlying_id, &leg.payoff_id, &leg.quote]
                    .iter()
                    .all(|s| !s.trim().is_empty())
                    && leg.quote == leg.settlement
                    && price(leg.tick)
                    && price(leg.lot)
                    && (MIN_PRICE..=MAX_MULTIPLIER).contains(&leg.base_per_contract)
                    && bounded(leg.min_contracts)
                    && bounded(leg.min_notional)
                    && (Decimal::ZERO..=Decimal::from(1_000)).contains(&leg.taker_fee_bps)
                    && leg.base_step() >= MIN_PRICE
                    && (1..=DAY_MS).contains(&leg.funding.interval_ms)
                    && leg.funding.offset_ms < leg.funding.interval_ms,
                "invalid or unsupported linear contract economics for {}",
                leg.venue
            );
        }
        let [a, b] = &self.legs;
        let (venue_a, venue_b) = (registry.get(&a.venue)?, registry.get(&b.venue)?);
        ensure!(
            a.underlying_id == b.underlying_id
                && a.payoff_id == b.payoff_id
                && a.settlement == b.settlement,
            "different underlying, payoff or settlement conventions"
        );
        ensure!(
            venue_a.liquidity_domain != venue_b.liquidity_domain,
            "legs share a matching book"
        );
        ensure!(
            self.allow_shared_risk_domain || venue_a.risk_domain != venue_b.risk_domain,
            "legs share an infrastructure risk domain"
        );
        Ok(())
    }
}

impl Book {
    fn valid_depth(&self, terms: &MarketTerms) -> bool {
        self.market == terms.market
            && self.tradable
            && bounded(self.available_notional)
            && self
                .bounds
                .is_none_or(|b| price(b.lower) && price(b.upper) && b.lower < b.upper)
            && [&self.bids, &self.asks]
                .into_iter()
                .enumerate()
                .all(|(i, levels)| {
                    (1..=MAX_LEVELS).contains(&levels.len())
                        && levels.iter().all(|l| {
                            price(l.price)
                                && l.price % terms.tick == Decimal::ZERO
                                && bounded(l.contracts)
                                && l.contracts > Decimal::ZERO
                        })
                        && levels.windows(2).all(|w| {
                            if i == 0 {
                                w[0].price > w[1].price
                            } else {
                                w[0].price < w[1].price
                            }
                        })
                })
            && self.bids[0].price < self.asks[0].price
    }

    fn side(&self, side: HedgeSide) -> &[Level] {
        match side {
            HedgeSide::Buy => &self.asks,
            HedgeSide::Sell => &self.bids,
        }
    }

    fn mid(&self) -> Decimal {
        (self.bids[0].price + self.asks[0].price) / Decimal::TWO
    }

    fn in_bounds(&self, limit: Decimal) -> bool {
        price(limit)
            && self
                .bounds
                .is_none_or(|b| b.lower <= limit && limit <= b.upper)
    }
}

#[derive(Debug, Default)]
struct BasisModel {
    samples: u32,
    last_ms: u64,
    last_books: [u64; 2],
    session_open: Option<u64>,
    mean: f64,
    variance: f64,
}

impl BasisModel {
    fn update(
        &mut self,
        config: &BasisConfig,
        now: u64,
        books: [u64; 2],
        session_open: Option<u64>,
        basis: f64,
    ) -> Decision<BasisSignal> {
        let elapsed = now - self.last_ms;
        if elapsed > config.max_sample_gap_ms || session_open != self.session_open {
            *self = Self::default();
        }
        if self.samples > 0
            && (elapsed < config.min_sample_ms
                || books[0] <= self.last_books[0]
                || books[1] <= self.last_books[1])
        {
            return Err(Rejection::SampleCadence);
        }
        let sigma = self.variance.sqrt().max(config.volatility_floor_bps);
        let innovation = basis - self.mean;
        // Score against the past, then update. Winsorize tail innovations once
        // warmed up to keep a single dislocation from resetting the baseline.
        let ready = self.samples >= config.min_samples;
        let signal = BasisSignal {
            observed_bps: basis,
            mean_bps: self.mean,
            sigma_bps: sigma,
            z: innovation / sigma,
            prior_samples: self.samples,
        };
        if self.samples == 0 {
            self.mean = basis;
        } else {
            let alpha =
                -(-std::f64::consts::LN_2 * elapsed as f64 / config.half_life_ms as f64).exp_m1();
            let delta = if ready {
                innovation.clamp(-6.0 * sigma, 6.0 * sigma)
            } else {
                innovation
            };
            self.mean += alpha * delta;
            self.variance = (1.0 - alpha) * (self.variance + alpha * delta * delta);
        }
        self.samples = self.samples.saturating_add(1);
        self.last_ms = now;
        self.last_books = books;
        self.session_open = session_open;
        if ready {
            Ok(signal)
        } else {
            Err(Rejection::Warmup)
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct DepthBand {
    base: Decimal,
    limit: Decimal,
}

#[derive(Debug)]
struct Depth {
    levels: [DepthBand; MAX_LEVELS],
    len: usize,
}

impl Depth {
    fn build(book: &Book, terms: &MarketTerms, side: HedgeSide, rules: Rules) -> Self {
        let buying = side == HedgeSide::Buy;
        let levels = book.side(side);
        let touch = levels[0].price;
        let max_impact = touch * rules.max_impact;
        let factor = Decimal::ONE
            + if buying {
                rules.limit_buffer
            } else {
                -rules.limit_buffer
            };
        let mut base = Decimal::ZERO;
        let mut depth = Self {
            levels: [DepthBand::default(); MAX_LEVELS],
            len: 0,
        };
        for level in levels {
            let padded = level.price * factor;
            let remainder = padded % terms.tick;
            let aligned = padded - remainder;
            let limit = if buying && remainder != Decimal::ZERO {
                aligned + terms.tick
            } else {
                aligned
            };
            if (limit - touch).abs() > max_impact || !book.in_bounds(limit) {
                break;
            }
            base += level.contracts * terms.base_per_contract * rules.participation;
            depth.levels[depth.len] = DepthBand { base, limit };
            depth.len += 1;
        }
        depth
    }

    fn bands(&self) -> &[DepthBand] {
        &self.levels[..self.len]
    }

    fn last(&self) -> Decision<DepthBand> {
        self.bands().last().copied().ok_or(Rejection::Capacity)
    }

    fn limit(&self, base: Decimal) -> Decision<Decimal> {
        self.bands()
            .iter()
            .find(|level| level.base >= base)
            .map(|level| level.limit)
            .ok_or(Rejection::Capacity)
    }
}

#[derive(Debug, Clone, Copy)]
struct Rules {
    participation: Decimal,
    max_impact: Decimal,
    limit_buffer: Decimal,
    max_base: Decimal,
    max_single_leg: Decimal,
    min_edge: Decimal,
    capital_charge: Decimal,
}

impl Rules {
    fn new(config: &PairConfig) -> Result<Self> {
        let planner = &config.planner;
        Ok(Self {
            participation: dec(planner.depth_fraction)?,
            max_impact: dec(config.max_depth_impact_bps)? / BPS,
            limit_buffer: dec(config.limit_buffer_bps)? / BPS,
            max_base: dec(planner.max_base_quantity)?,
            max_single_leg: dec(planner
                .max_leg_notional_usd
                .min(planner.max_unhedged_notional_usd))?,
            min_edge: dec(planner.min_net_edge_bps)?,
            capital_charge: dec(config.capital_charge_bps_per_day)? / BPS,
        })
    }
}

pub struct RelativeValue {
    config: PairConfig,
    rules: Rules,
    step: Decimal,
    basis: BasisModel,
    last_frame_ms: u64,
    last_book_ms: [u64; 2],
}

impl RelativeValue {
    pub fn new(config: PairConfig, registry: &VenueRegistry) -> Result<Self> {
        config.validate(registry)?;
        let rules = Rules::new(&config)?;
        let step = common_quantity_step(config.legs[0].base_step(), config.legs[1].base_step())?;
        Ok(Self {
            config,
            rules,
            step,
            basis: BasisModel::default(),
            last_frame_ms: 0,
            last_book_ms: [0; 2],
        })
    }

    fn book_times(&self, frame: &Frame) -> [u64; 2] {
        frame.legs.each_ref().map(|leg| leg.book.timestamp_ms)
    }

    fn usable_books(&self, frame: &Frame) -> bool {
        self.check_quote_skew(self.book_times(frame)).is_ok()
            && (0..2).all(|i| self.usable_book(frame, i))
    }

    fn usable_book(&self, frame: &Frame, i: usize) -> bool {
        self.check_book(frame, i).is_ok()
    }

    fn check_book(&self, frame: &Frame, i: usize) -> EntryDecision<()> {
        let book = &frame.legs[i].book;
        let gate = if book.timestamp_ms == 0 {
            EntryGate::BookMissing
        } else if book.timestamp_ms < self.last_book_ms[i] || book.timestamp_ms > frame.now_ms {
            EntryGate::BookClock
        } else if frame.now_ms - book.timestamp_ms > self.config.planner.max_quote_age_ms {
            EntryGate::BookStale
        } else if !book.valid_depth(&self.config.legs[i]) {
            EntryGate::BookInvalid
        } else {
            return Ok(());
        };
        Err(gate.on_leg(i))
    }

    fn check_quote_skew(&self, times: [u64; 2]) -> EntryDecision<()> {
        if times[0].abs_diff(times[1]) > self.config.planner.max_quote_skew_ms {
            return Err(EntryGate::QuoteSkew.into());
        }
        Ok(())
    }

    fn session_end(&self, frame: &Frame) -> Decision<u64> {
        match (self.config.asset_class, frame.equity_session) {
            (AssetClass::Crypto, None) => Ok(u64::MAX),
            (AssetClass::Equity, Some(session))
                if session.observed_at_ms > 0
                    && session.observed_at_ms <= frame.now_ms
                    && session.opens_at_ms > 0
                    && session.opens_at_ms <= frame.now_ms
                    && !session.halted =>
            {
                let end = session.closes_at_ms.min(session.clear_until_ms);
                if frame.now_ms < end {
                    Ok(end)
                } else {
                    Err(Rejection::Session)
                }
            }
            _ => Err(Rejection::Session),
        }
    }

    fn cutoff(&self, frame: &Frame) -> Decision<u64> {
        let cutoff = self
            .session_end(frame)?
            .saturating_sub(self.config.equity_close_buffer_ms);
        if frame.now_ms < cutoff {
            Ok(cutoff)
        } else {
            Err(Rejection::Session)
        }
    }

    fn basis_bps(&self, frame: &Frame) -> EntryDecision<f64> {
        self.config
            .reference_deadline_ms(frame.reference, frame.equity_session, frame.now_ms)
            .map_err(|reason| match reason {
                Rejection::OracleStale => EntryGate::ReferenceStale,
                _ => EntryGate::ReferenceInvalid,
            })?;
        let earliest = frame
            .equity_session
            .map_or(0, |session| session.opens_at_ms);
        for (i, leg) in frame.legs.iter().enumerate() {
            let validity = if leg.oracle.mode == OracleMode::LiveExternal {
                oracle_deadline_ms(
                    leg.oracle,
                    frame.now_ms,
                    earliest,
                    self.config.oracle_max_age_ms,
                )
            } else {
                Err(invalid_oracle_mode(leg.oracle))
            };
            validity.map_err(|reason| match reason {
                Rejection::OracleStale => EntryGate::VenueOracleStale.on_leg(i),
                _ => EntryGate::VenueOracleInvalid.on_leg(i),
            })?;
            if !self.within_reference(leg.oracle.price, frame.reference.price) {
                return Err(EntryGate::VenueOracleDeviation.on_leg(i));
            }
        }
        // Reject incoherent books instead of training the model on dislocations
        // caused by unit, contract or oracle mismatches.
        for (i, leg) in frame.legs.iter().enumerate() {
            if !self.within_reference(leg.book.mid(), frame.reference.price) {
                return Err(EntryGate::BookReferenceDeviation.on_leg(i));
            }
        }
        ((frame.legs[1].book.mid() - frame.legs[0].book.mid()) / frame.reference.price * BPS)
            .to_f64()
            .ok_or_else(|| EntryGate::ReferenceInvalid.into())
    }

    fn within_reference(&self, observed: Decimal, reference: Decimal) -> bool {
        ((observed - reference).abs() / reference * BPS)
            .to_f64()
            .is_some_and(|bps| bps <= self.config.max_oracle_deviation_bps)
    }

    /// Ingest a synchronized observation and possibly plan the next entry.
    /// Feed every frame in availability order, including frames during holds,
    /// so the next trade's baseline is fit on observations available then.
    pub fn observe(&mut self, frame: &Frame) -> Result<Decision<Plan>> {
        Ok(self
            .sample(frame)?
            .and_then(|(signal, cutoff)| self.plan(frame, signal, cutoff, 0, None))
            .map_err(|blocked| blocked.gate.rejection()))
    }

    fn sample(&mut self, frame: &Frame) -> Result<EntryDecision<(BasisSignal, u64)>> {
        ensure!(
            frame.now_ms > self.last_frame_ms,
            "non-increasing replay availability time"
        );
        let times = self.book_times(frame);
        let regressed = times
            .iter()
            .enumerate()
            .any(|(i, time)| *time > 0 && *time < self.last_book_ms[i]);
        // Track each venue even if the other venue is stale. The simulator can
        // fill one leg independently and must never accept a regressed book.
        let books = [0, 1].map(|i| self.check_book(frame, i));
        for (i, book) in books.iter().enumerate() {
            if book.is_ok() {
                self.last_book_ms[i] = times[i];
            }
        }
        self.last_frame_ms = frame.now_ms;
        let result = self.observe_inner(frame, times, books);
        // Short gaps in valid books or external updates do not train the model.
        // The next usable sample enforces the configured maximum gap.
        if regressed
            || result.is_err_and(|blocked| {
                matches!(
                    blocked.gate.rejection(),
                    Rejection::Session | Rejection::Oracle
                )
            })
        {
            self.basis = BasisModel::default();
        }
        Ok(result)
    }

    fn observe_inner(
        &mut self,
        frame: &Frame,
        times: [u64; 2],
        books: [EntryDecision<()>; 2],
    ) -> EntryDecision<(BasisSignal, u64)> {
        let cutoff = self.cutoff(frame).map_err(|_| EntryGate::Session)?;
        books[0]?;
        books[1]?;
        self.check_quote_skew(times)?;
        let basis = self.basis_bps(frame)?;
        let signal = self
            .basis
            .update(
                &self.config.basis,
                frame.now_ms,
                times,
                frame.equity_session.map(|s| s.opens_at_ms),
                basis,
            )
            .map_err(|reason| match reason {
                Rejection::Warmup => EntryGate::Warmup,
                _ => EntryGate::SampleCadence,
            })?;
        Ok((signal, cutoff))
    }

    fn plan(
        &self,
        frame: &Frame,
        signal: BasisSignal,
        cutoff: u64,
        funding_guard_ms: u64,
        mut best_edge_bps: Option<&mut Option<Decimal>>,
    ) -> EntryDecision<Plan> {
        let now = frame.now_ms;
        let max_hold = now
            .checked_add(self.config.planner.funding_horizon_ms)
            .ok_or(EntryGate::Session)?;
        let mut depth = None;
        if self.config.strategy != Strategy::FundingCarry || !self.config.optimize_funding_horizon {
            return self.plan_for_hold(frame, signal, max_hold, cutoff, &mut depth, best_edge_bps);
        }
        let max_hold = max_hold.min(
            cutoff
                .checked_sub(self.config.execution_window_ms)
                .filter(|latest| *latest > now)
                .ok_or(EntryGate::Session)?,
        );
        // The carry bound credits payments strictly before the hold deadline.
        // Leave the full execution window after the settlement guard.
        let after_payment = funding_guard_ms.saturating_add(1);
        let Some(last_payment) = max_hold
            .checked_sub(after_payment)
            .filter(|last| *last > now)
        else {
            return Err(EntryGate::NetEdge.into());
        };
        let mut holds = Vec::new();
        for (i, leg) in self.config.legs.iter().enumerate() {
            let (first, count) = leg
                .funding
                .window(now, last_payment)
                .ok_or(EntryGate::FundingForecast.on_leg(i))?;
            holds.extend(
                (0..count.min(MAX_FUNDING_POINTS as u64))
                    .map(|index| first + index * leg.funding.interval_ms + after_payment),
            );
        }
        holds.sort_unstable();
        holds.dedup();
        let mut best: Option<Plan> = None;
        let mut rejection: Option<BlockedEntry> = None;
        for hold in holds {
            match self.plan_for_hold(
                frame,
                signal,
                hold,
                cutoff,
                &mut depth,
                best_edge_bps.as_deref_mut(),
            ) {
                Ok(plan)
                    if best
                        .as_ref()
                        .is_none_or(|previous| plan.net_edge > previous.net_edge) =>
                {
                    best = Some(plan);
                }
                Ok(_) => {}
                Err(blocked) => {
                    if rejection.is_none_or(|previous| previous.gate < blocked.gate) {
                        rejection = Some(blocked);
                    }
                    // Later unwinds would require the same missing evidence.
                    if blocked.gate == EntryGate::FundingForecast {
                        break;
                    }
                }
            }
        }
        best.ok_or_else(|| rejection.unwrap_or_else(|| EntryGate::NetEdge.into()))
    }

    fn plan_for_hold(
        &self,
        frame: &Frame,
        signal: BasisSignal,
        hold_until: u64,
        cutoff: u64,
        depth: &mut Option<[[Depth; 2]; 2]>,
        mut best_edge_bps: Option<&mut Option<Decimal>>,
    ) -> EntryDecision<Plan> {
        let now = frame.now_ms;
        let unwind_until = hold_until
            .checked_add(self.config.execution_window_ms)
            .ok_or(EntryGate::Session)?;
        if unwind_until > cutoff {
            return Err(EntryGate::Session.into());
        }
        let inputs_expire = frame.legs.iter().fold(
            self.config
                .reference_deadline_ms(frame.reference, frame.equity_session, now)
                .map_err(|reason| match reason {
                    Rejection::OracleStale => EntryGate::ReferenceStale,
                    _ => EntryGate::ReferenceInvalid,
                })?,
            |time, leg| {
                time.min(
                    leg.book
                        .timestamp_ms
                        .saturating_add(self.config.planner.max_quote_age_ms),
                )
                .min(
                    leg.oracle
                        .timestamp_ms
                        .saturating_add(self.config.oracle_max_age_ms),
                )
            },
        );
        let mut entry_until = now
            .saturating_add(self.config.execution_window_ms)
            .min(inputs_expire);
        // The planned hold must start after the last permitted entry fill.
        if entry_until <= now || entry_until >= hold_until {
            return Err(EntryGate::EntryWindow.into());
        }
        for (i, leg) in frame.legs.iter().enumerate() {
            let (_, count) = self.config.legs[i]
                .funding
                .window(now, unwind_until)
                .ok_or(EntryGate::FundingForecast)?;
            if count > 0 {
                let forecast = leg
                    .forecast
                    .as_ref()
                    .ok_or(EntryGate::FundingForecast.on_leg(i))?;
                entry_until = entry_until.min(
                    forecast
                        .observed_at_ms
                        .saturating_add(self.config.planner.max_funding_age_ms),
                );
            }
        }
        if entry_until <= now {
            return Err(EntryGate::FundingForecast.into());
        }
        let bps = &self.config.basis;
        if signal.z.abs() >= bps.stop_z {
            return Err(EntryGate::BasisBeyondStop.into());
        }
        let mean_reversion = match self.config.strategy {
            Strategy::FundingCarry => Decimal::ZERO,
            Strategy::BasisReversion if signal.z.abs() >= bps.entry_z => {
                dec((signal.observed_bps - signal.mean_bps).abs() * bps.reversion_fraction)
                    .map_err(|_| EntryGate::NetEdge)?
                    / BPS
                    * frame.reference.price
            }
            Strategy::BasisReversion => return Err(EntryGate::BasisBelowEntry.into()),
        };
        let reserve = dec(self
            .config
            .planner
            .exit_basis_reserve_bps
            .max(signal.sigma_bps * bps.stress_sigma))
        .map_err(|_| EntryGate::NetEdge)?
            / BPS
            * frame.reference.price;
        let depth = depth.get_or_insert_with(|| {
            [0, 1].map(|i| {
                [HedgeSide::Buy, HedgeSide::Sell].map(|side| {
                    Depth::build(&frame.legs[i].book, &self.config.legs[i], side, self.rules)
                })
            })
        });
        let mut best: Option<Plan> = None;
        let mut rejected = Rejection::Capacity;
        for buy_leg in 0..2 {
            if self.config.strategy == Strategy::BasisReversion
                && (buy_leg == 0) != (signal.z > 0.0)
            {
                continue;
            }
            let window = [entry_until, hold_until, unwind_until];
            let funding = self
                .funding_bound(frame, buy_leg, window)
                .map_err(|_| EntryGate::FundingForecast)?;
            if self.config.strategy == Strategy::FundingCarry && funding <= Decimal::ZERO {
                rejected = Rejection::Edge;
                continue;
            }
            let score = Score {
                signal,
                buy_leg,
                funding,
                mean_reversion,
                reserve,
                window,
            };
            match self.size_plan(frame, depth, score, best_edge_bps.as_deref_mut()) {
                Ok(plan)
                    if best
                        .as_ref()
                        .is_none_or(|current| plan.net_edge > current.net_edge) =>
                {
                    best = Some(plan);
                }
                Ok(_) => {}
                Err(reason) => rejected = rejected.max(reason),
            }
        }
        best.ok_or_else(|| match rejected {
            Rejection::Edge => EntryGate::NetEdge.into(),
            _ => EntryGate::Capacity.into(),
        })
    }

    fn funding_bound(&self, frame: &Frame, buy_leg: usize, window: [u64; 3]) -> Decision<Decimal> {
        let [entry_until, hold_until, unwind_until] = window;
        let now = frame.now_ms;
        let mut bound = Decimal::ZERO;
        for i in 0..2 {
            let schedule = self.config.legs[i].funding;
            let (first, count) = schedule
                .window(now, unwind_until)
                .ok_or(Rejection::Funding)?;
            if count == 0 {
                continue;
            }
            let forecast = frame.legs[i].forecast.as_ref().ok_or(Rejection::Funding)?;
            if !fresh(
                forecast.observed_at_ms,
                now,
                self.config.planner.max_funding_age_ms,
            ) || forecast.payments.len() > MAX_FUNDING_POINTS
                || forecast
                    .payments
                    .windows(2)
                    .any(|w| w[0].at_ms >= w[1].at_ms)
                || count > MAX_FUNDING_POINTS as u64
            {
                return Err(Rejection::Funding);
            }
            let mut points = forecast.payments.iter().filter(|p| p.at_ms > now);
            for index in 0..count {
                let time = first + index * schedule.interval_ms;
                let point = points.next().ok_or(Rejection::Funding)?;
                if point.at_ms != time
                    || point.rate.abs() > Decimal::ONE
                    || !(Decimal::ZERO..=BPS).contains(&point.uncertainty_bps)
                    || !price(point.oracle_price)
                    || !self.within_reference(point.oracle_price, frame.reference.price)
                {
                    return Err(Rejection::Funding);
                }
                let signed_rate = if i == buy_leg {
                    -point.rate
                } else {
                    point.rate
                };
                let lower = (signed_rate - point.uncertainty_bps / BPS) * point.oracle_price;
                // Avoid credit near entry/exit, or when basis reversion may exit
                // early. Always reserve plausible debits while a leg may be open.
                let credit = self.config.strategy == Strategy::FundingCarry
                    && time > entry_until
                    && time < hold_until;
                bound += if credit {
                    lower
                } else {
                    lower.min(Decimal::ZERO)
                };
            }
        }
        Ok(bound)
    }

    fn size_plan(
        &self,
        frame: &Frame,
        depth: &[[Depth; 2]; 2],
        score: Score,
        mut best_edge_bps: Option<&mut Option<Decimal>>,
    ) -> Decision<Plan> {
        let mut cap = self.rules.max_base;
        for curve in depth.iter().flatten() {
            cap = cap.min(curve.last()?.base);
        }
        let cap = floor_step(cap, self.step).map_err(|_| Rejection::Capacity)?;
        if cap <= Decimal::ZERO {
            return Err(Rejection::Capacity);
        }

        // Using each marginal limit for the full order bounds taker cost
        // conservatively. Per-base economics are constant between level
        // boundaries, so the rightmost common lot in each band suffices.
        let candidates = std::iter::once(cap).chain(
            depth
                .iter()
                .flatten()
                .flat_map(|curve| curve.bands().iter().map(|b| b.base))
                .filter(|size| *size < cap),
        );
        let mut best: Option<(Decimal, Economics)> = None;
        let mut sized = false;
        let mids = frame.legs.each_ref().map(|leg| leg.book.mid());
        let days = Decimal::from(score.window[2] - frame.now_ms) / Decimal::from(DAY_MS);
        let budget = frame
            .legs
            .each_ref()
            .map(|leg| leg.book.available_notional.min(self.rules.max_single_leg));
        let risk_price = |i: usize, e: &Economics| {
            e.entry_limits[i]
                .max(e.exit_limits[i])
                .max(frame.reference.price)
                .max(frame.legs[i].oracle.price)
        };
        for candidate in candidates {
            let mut qty = floor_step(candidate, self.step).map_err(|_| Rejection::Capacity)?;
            if qty <= Decimal::ZERO {
                continue;
            }
            let mut econ = self.economics(depth, score, qty, mids, days)?;
            // Size at the risk prices of this band, so unrelated deep levels
            // cannot consume the notional budget of a smaller executable lot.
            let affordable = (0..2).fold(qty, |q, i| q.min(budget[i] / risk_price(i, &econ)));
            if affordable < qty {
                qty = floor_step(affordable, self.step).map_err(|_| Rejection::Capacity)?;
                if qty <= Decimal::ZERO {
                    continue;
                }
                econ = self.economics(depth, score, qty, mids, days)?;
            }
            if (0..2).any(|i| {
                qty * risk_price(i, &econ) > budget[i]
                    || qty < self.config.legs[i].min_base(econ.entry_limits[i])
                    || qty < self.config.legs[i].min_base(econ.exit_limits[i])
            }) {
                continue;
            }
            sized = true;
            let net = econ.net(score);
            let net_bps = if net > Decimal::ZERO || best_edge_bps.is_some() {
                net / frame.reference.price * BPS
            } else {
                Decimal::ZERO
            };
            if let Some(best) = best_edge_bps.as_deref_mut() {
                *best = Some(best.map_or(net_bps, |previous| previous.max(net_bps)));
            }
            if net > Decimal::ZERO
                && net_bps >= self.rules.min_edge
                && best
                    .as_ref()
                    .is_none_or(|(previous, old)| net * qty > old.net(score) * previous)
            {
                best = Some((qty, econ));
            }
        }
        let (qty, econ) = best.ok_or(if sized {
            Rejection::Edge
        } else {
            Rejection::Capacity
        })?;
        let sides = sides(score.buy_leg);
        let hedge = self
            .hedge_plan(
                sides,
                econ.entry_limits,
                qty,
                score.window[0],
                format!("rv-entry-{}", frame.now_ms),
                depth,
            )
            .map_err(|_| Rejection::Capacity)?;
        let net = econ.net(score);
        Ok(Plan {
            strategy: self.config.strategy,
            buy_leg: score.buy_leg,
            basis: score.signal,
            modeled_convergence: score.mean_reversion * qty,
            entry_cost: econ.entry * qty,
            exit_cost: econ.exit * qty,
            basis_reserve: score.reserve * qty,
            funding_lower_bound: score.funding * qty,
            capital_charge: econ.capital * qty,
            net_edge: net * qty,
            net_edge_bps: net / frame.reference.price * BPS,
            hold_until_ms: score.window[1],
            unwind_until_ms: score.window[2],
            hedge,
        })
    }

    fn economics(
        &self,
        depth: &[[Depth; 2]; 2],
        score: Score,
        qty: Decimal,
        mid: [Decimal; 2],
        days: Decimal,
    ) -> Decision<Economics> {
        let mut econ = Economics::default();
        for i in 0..2 {
            let is_buy = i == score.buy_leg;
            let entry_limit = depth[i][usize::from(!is_buy)].limit(qty)?;
            let exit_limit = depth[i][usize::from(is_buy)].limit(qty)?;
            let fee = self.config.legs[i].taker_fee_bps / BPS;
            let sign = if is_buy { Decimal::ONE } else { -Decimal::ONE };
            econ.entry_limits[i] = entry_limit;
            econ.exit_limits[i] = exit_limit;
            econ.entry += sign * (entry_limit - mid[i]) + entry_limit * fee;
            econ.exit += sign * (mid[i] - exit_limit) + exit_limit * fee;
            econ.capital += entry_limit.max(exit_limit) * self.rules.capital_charge * days;
        }
        Ok(econ)
    }

    fn hedge_plan(
        &self,
        sides: [HedgeSide; 2],
        limits: [Decimal; 2],
        qty: Decimal,
        deadline: u64,
        execution_id: String,
        depth: &[[Depth; 2]; 2],
    ) -> Result<HedgePlan> {
        let specs = [0, 1].map(|i| {
            let leg = &self.config.legs[i];
            LegSpec {
                venue: leg.venue.clone(),
                side: sides[i],
                quantity_step: leg.base_step(),
                min_base: leg.min_base(limits[i]).max(leg.base_step()),
                limit_price: limits[i],
                max_base: qty,
                max_notional: qty * limits[i],
            }
        });
        let cap = |i: usize| {
            depth[i][usize::from(sides[i] == HedgeSide::Sell)]
                .last()
                .map(|band| band.base)
        };
        // Lead with the thinner book, leaving the larger displayed capacity as
        // the hedge. The whole requested size fits the single-leg risk budget.
        let primary = usize::from(cap(1)? < cap(0)?);
        let [a, b] = specs;
        let (primary, hedge) = if primary == 0 { (a, b) } else { (b, a) };
        let plan = HedgePlan {
            execution_id,
            instrument: self.config.legs[0].underlying_id.clone(),
            primary,
            hedge,
            target_base: qty,
            max_unhedged_base: qty,
            deadline_ms: deadline,
            max_order_attempts: 8,
        };
        plan.validate()?;
        Ok(plan)
    }
}

#[derive(Debug, Clone, Copy)]
struct Score {
    signal: BasisSignal,
    buy_leg: usize,
    funding: Decimal,
    mean_reversion: Decimal,
    reserve: Decimal,
    window: [u64; 3],
}

#[derive(Debug, Default)]
struct Economics {
    entry_limits: [Decimal; 2],
    exit_limits: [Decimal; 2],
    entry: Decimal,
    exit: Decimal,
    capital: Decimal,
}

impl Economics {
    fn net(&self, score: Score) -> Decimal {
        score.mean_reversion + score.funding - self.entry - self.exit - score.reserve - self.capital
    }
}

fn sides(buy_leg: usize) -> [HedgeSide; 2] {
    if buy_leg == 0 {
        [HedgeSide::Buy, HedgeSide::Sell]
    } else {
        [HedgeSide::Sell, HedgeSide::Buy]
    }
}

#[cfg(test)]
mod tests;
