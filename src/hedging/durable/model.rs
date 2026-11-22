use super::super::{HedgeSide, Leg, OrderIntent, OrderStatus};
use crate::exchanges::arcus::{self, TickTier};
use anyhow::{Result, ensure};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

pub(super) const MAX_FEE_RATE_NUMERATOR: i64 = 1;
pub(super) const MAX_FEE_RATE_SCALE: u32 = 2; // 0.01 = 100 bp

pub(super) fn index(leg: Leg) -> usize {
    match leg {
        Leg::Primary => 0,
        Leg::Hedge => 1,
    }
}

pub(super) fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_:./".contains(&c))
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "venue", rename_all = "snake_case", deny_unknown_fields)]
pub enum VenuePriceRules {
    Arcus {
        #[serde(with = "rust_decimal::serde::str")]
        tick_size: Decimal,
        #[serde(default)]
        tick_tiers: Vec<TickTier>,
    },
    TradeXyz {
        sz_decimals: u32,
    },
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegIdentity {
    pub venue: String,
    pub network: String,
    pub account: String,
    pub subaccount: Option<u8>,
    pub market: String,
    pub market_id: u32,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegLimits {
    pub identity: LegIdentity,
    pub quote_asset: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub quantity_step: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub max_position_base: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub max_order_notional: Decimal,
    pub price_rules: VenuePriceRules,
    #[serde(with = "rust_decimal::serde::str")]
    pub quote_budget: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub max_fee_rate: Decimal,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSpec {
    /// Fresh UUID for one independently budgeted execution session.
    pub session_id: String,
    pub legs: [LegLimits; 2],
    #[serde(with = "rust_decimal::serde::str")]
    pub max_unhedged_base: Decimal,
    pub max_observation_age_us: u64,
}

impl SessionSpec {
    pub(super) fn validate(&self) -> Result<()> {
        ensure!(
            uuid::Uuid::parse_str(&self.session_id)?.to_string() == self.session_id,
            "Session UUID must be canonical"
        );
        ensure!(
            self.max_unhedged_base > Decimal::ZERO
                && self.max_observation_age_us > 0
                && self.max_observation_age_us <= 5_000_000,
            "Invalid execution risk or freshness limits"
        );
        let max_fee = Decimal::new(MAX_FEE_RATE_NUMERATOR, MAX_FEE_RATE_SCALE);
        for (i, leg) in self.legs.iter().enumerate() {
            let id = &leg.identity;
            crate::exchanges::hyperliquid::validate_hex(&id.account, 20)?;
            ensure!(
                id.account == id.account.to_ascii_lowercase(),
                "Account identity must be canonical"
            );
            ensure!(
                matches!(id.network.as_str(), "mainnet" | "testnet"),
                "Unknown execution network"
            );
            ensure!(valid_id(&leg.quote_asset), "Invalid leg quote asset");
            ensure!(
                leg.quantity_step > Decimal::ZERO
                    && leg.max_position_base > Decimal::ZERO
                    && leg.max_order_notional > Decimal::ZERO
                    && leg.quote_budget > Decimal::ZERO
                    && leg.max_fee_rate >= Decimal::ZERO
                    && leg.max_fee_rate <= max_fee,
                "Invalid leg limits"
            );
            if i == 0 {
                ensure!(
                    id.venue == "arcus"
                        && id.market == "ANT-USD"
                        && id.market_id <= u16::MAX as u32
                        && id.subaccount.is_some_and(|n| n < 10),
                    "Expected explicit Arcus ANT identity"
                );
                match &leg.price_rules {
                    VenuePriceRules::Arcus {
                        tick_size,
                        tick_tiers,
                    } => arcus::validate_tick_rules(*tick_size, tick_tiers)?,
                    VenuePriceRules::TradeXyz { .. } => {
                        anyhow::bail!("Expected explicit Arcus price rules");
                    }
                }
            } else {
                ensure!(
                    id.venue == "trade_xyz" && id.market == "xyz:ANT" && id.subaccount.is_none(),
                    "Expected explicit TradeXYZ ANT identity"
                );
                match &leg.price_rules {
                    VenuePriceRules::TradeXyz { sz_decimals } => {
                        ensure!(
                            *sz_decimals <= 6
                                && leg.quantity_step.normalize().scale() <= *sz_decimals,
                            "Expected valid TradeXYZ price/size precision"
                        );
                    }
                    VenuePriceRules::Arcus { .. } => {
                        anyhow::bail!("Expected explicit TradeXYZ price rules");
                    }
                }
            }
        }
        ensure!(
            self.legs[0].identity.network == self.legs[1].identity.network,
            "Mixed execution networks"
        );
        Ok(())
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct LegacyLegLimits {
    pub identity: LegIdentity,
    #[serde(with = "rust_decimal::serde::str")]
    pub quantity_step: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub max_position_base: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub max_order_notional: Decimal,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct LegacySessionSpec {
    pub session_id: String,
    pub legs: [LegacyLegLimits; 2],
    #[serde(with = "rust_decimal::serde::str")]
    pub max_unhedged_base: Decimal,
    pub max_observation_age_us: u64,
}

impl LegacySessionSpec {
    pub(super) fn matches(&self, expected: &SessionSpec) -> bool {
        self.session_id == expected.session_id
            && self.max_unhedged_base == expected.max_unhedged_base
            && self.max_observation_age_us == expected.max_observation_age_us
            && self
                .legs
                .iter()
                .zip(expected.legs.iter())
                .all(|(legacy, current)| {
                    legacy.identity == current.identity
                        && legacy.quantity_step == current.quantity_step
                        && legacy.max_position_base == current.max_position_base
                        && legacy.max_order_notional == current.max_order_notional
                })
    }
}

/// A durable logical-to-wire mapping. Returned only after append and fsync.
/// It is not authorization to send; the caller still owns the dispatch gates.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedOrder {
    pub intent: OrderIntent,
    pub wire_client_id: String,
    pub reduce_only: bool,
    /// Proven upper execution price bound for worst-case notional & fee reserve.
    /// For Buy orders this must equal `intent.limit_price`; for Sell orders it
    /// must be explicitly provided (>= `intent.limit_price`) from a proven outer
    /// execution bound rather than inferred from the lower sell limit.
    #[serde(with = "rust_decimal::serde::str")]
    pub max_fill_price: Decimal,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrderAcknowledgement {
    pub identity: LegIdentity,
    pub logical_id: String,
    pub exchange_order_id: String,
}

/// Normalized authoritative fill. Adapters must verify source account/market,
/// client mapping and side before constructing it. Fees remain in their asset.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionFill {
    pub identity: LegIdentity,
    pub logical_id: String,
    pub exchange_order_id: String,
    pub trade_id: String,
    pub venue_at_us: u64,
    #[serde(with = "rust_decimal::serde::str")]
    pub quantity: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub price: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub fee: Decimal,
    pub fee_asset: String,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrderTerminal {
    pub identity: LegIdentity,
    pub logical_id: String,
    pub exchange_order_id: Option<String>,
    pub status: OrderStatus,
    #[serde(with = "rust_decimal::serde::str")]
    pub cumulative_filled: Decimal,
    pub venue_at_us: u64,
}

/// All fields are evidence requirements, not claims inferred from an empty
/// response. This module does not authenticate or fetch a venue response.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PositionEvidence {
    pub identity: LegIdentity,
    pub request_started_us: u64,
    pub received_at_us: u64,
    pub position_venue_at_us: u64,
    pub fills_complete_through_venue_us: u64,
    /// Exact trade IDs for this session; missing/extra coverage is rejected.
    pub covered_trade_ids: Vec<String>,
    pub open_order_ids: Vec<String>,
    #[serde(with = "rust_decimal::serde::str")]
    pub position_base: Decimal,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reconciliation {
    /// Journal sequence captured before the evidence queries began.
    pub observed_sequence: u64,
    pub legs: [PositionEvidence; 2],
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettledFeeCharge {
    pub asset: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub amount: Decimal,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinancialEvidence {
    pub identity: LegIdentity,
    pub request_started_us: u64,
    pub received_at_us: u64,
    pub venue_at_us: u64,
    /// Unconverted positive non-quote fees resolved by independent settlement proof.
    pub settled_fee_assets: Vec<SettledFeeCharge>,
    /// Independent converted quote-currency charge (>= 0) booked into realized fees.
    #[serde(with = "rust_decimal::serde::str")]
    pub converted_quote_cost: Decimal,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinancialReconciliation {
    /// Journal sequence captured before the financial evidence queries began.
    pub observed_sequence: u64,
    pub legs: [FinancialEvidence; 2],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExposureRange {
    pub minimum_base: Decimal,
    pub maximum_base: Decimal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FillEffect {
    pub before_base: Decimal,
    pub after_base: Decimal,
    /// Provisional source-ordered attribution. Unknown for a late or tied
    /// source timestamp; positions/fees are still recorded exactly.
    pub closed_base: Option<Decimal>,
    pub opened_base: Option<Decimal>,
    pub crossed_zero: Option<bool>,
}

pub(super) fn signed(side: HedgeSide, amount: Decimal) -> Decimal {
    if side == HedgeSide::Buy {
        amount
    } else {
        -amount
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(
    tag = "event",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(super) enum Event {
    Start(SessionSpec),
    Prepared(PreparedOrder),
    Acknowledged(OrderAcknowledgement),
    Fill(ExecutionFill),
    Terminal(OrderTerminal),
    CancelRequested { logical_id: String },
    Uncertain { logical_id: String },
    Reconciled(Reconciliation),
    FinancialReconciled(FinancialReconciliation),
    Closed,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct LegacyPreparedOrder {
    pub intent: OrderIntent,
    pub wire_client_id: String,
    pub reduce_only: bool,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(
    tag = "event",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(super) enum LegacyEvent {
    Start(LegacySessionSpec),
    Prepared(LegacyPreparedOrder),
    Acknowledged(OrderAcknowledgement),
    Fill(ExecutionFill),
    Terminal(OrderTerminal),
    CancelRequested { logical_id: String },
    Uncertain { logical_id: String },
    Reconciled(Reconciliation),
    Closed,
}

impl LegacyEvent {
    pub(super) fn into_non_start(self, expected: &SessionSpec) -> Result<Event> {
        Ok(match self {
            Self::Start(legacy) => {
                ensure!(
                    legacy.matches(expected),
                    "Execution header identity mismatch"
                );
                Event::Start(expected.clone())
            }
            Self::Prepared(v) => Event::Prepared(PreparedOrder {
                max_fill_price: v.intent.limit_price,
                intent: v.intent,
                wire_client_id: v.wire_client_id,
                reduce_only: v.reduce_only,
            }),
            Self::Acknowledged(v) => Event::Acknowledged(v),
            Self::Fill(v) => Event::Fill(v),
            Self::Terminal(v) => Event::Terminal(v),
            Self::CancelRequested { logical_id } => Event::CancelRequested { logical_id },
            Self::Uncertain { logical_id } => Event::Uncertain { logical_id },
            Self::Reconciled(v) => Event::Reconciled(v),
            Self::Closed => Event::Closed,
        })
    }

    pub(super) fn from_non_start(event: &Event) -> Result<Self> {
        Ok(match event {
            Event::Start(_) => anyhow::bail!("Legacy journal cannot write new Start header"),
            Event::Prepared(v) => Self::Prepared(LegacyPreparedOrder {
                intent: v.intent.clone(),
                wire_client_id: v.wire_client_id.clone(),
                reduce_only: v.reduce_only,
            }),
            Event::Acknowledged(v) => Self::Acknowledged(v.clone()),
            Event::Fill(v) => Self::Fill(v.clone()),
            Event::Terminal(v) => Self::Terminal(v.clone()),
            Event::CancelRequested { logical_id } => Self::CancelRequested {
                logical_id: logical_id.clone(),
            },
            Event::Uncertain { logical_id } => Self::Uncertain {
                logical_id: logical_id.clone(),
            },
            Event::Reconciled(v) => Self::Reconciled(v.clone()),
            Event::FinancialReconciled(_) => {
                anyhow::bail!("Legacy journal does not support financial reconciliation")
            }
            Event::Closed => Self::Closed,
        })
    }
}
