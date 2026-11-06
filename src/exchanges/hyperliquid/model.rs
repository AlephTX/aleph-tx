use anyhow::{Result, ensure};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Network {
    Mainnet,
    Testnet,
}

impl Network {
    pub fn base_url(self) -> &'static str {
        match self {
            Self::Mainnet => "https://api.hyperliquid.xyz",
            Self::Testnet => "https://api.hyperliquid-testnet.xyz",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PerpAsset {
    pub name: String,
    pub sz_decimals: u32,
    pub max_leverage: u32,
    #[serde(default)]
    pub is_delisted: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PerpMeta {
    pub universe: Vec<PerpAsset>,
    #[serde(rename = "collateralToken", default)]
    pub collateral_token: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PerpDex {
    pub name: String,
}

/// Resolve the deployer's current index from perpDexs; never hardcode Entropy's
/// or any other builder's asset base. Entry zero represents the native DEX.
pub fn resolve_perp_offset(dex: &str, dexs: &[Option<PerpDex>]) -> Result<u32> {
    if dex.is_empty() {
        return Ok(0);
    }
    ensure!(
        matches!(dexs.first(), Some(None)),
        "Invalid perpDexs native entry"
    );
    let indices: Vec<_> = dexs
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.as_ref().is_some_and(|item| item.name == dex))
        .map(|(index, _)| index)
        .collect();
    ensure!(indices.len() == 1, "DEX missing or duplicated in perpDexs");
    let index = u32::try_from(indices[0])?;
    index
        .checked_mul(10_000)
        .and_then(|base| base.checked_add(100_000))
        .ok_or_else(|| anyhow::anyhow!("Perpetual asset offset overflow"))
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetContext {
    pub funding: String,
    pub mark_px: String,
    pub oracle_px: String,
    pub open_interest: String,
}

#[derive(Debug, Clone)]
pub struct PerpMarket {
    /// Exchange ID resolved from DEX index and universe position, not a stable cross-venue symbol ID.
    pub asset_id: u32,
    pub asset: PerpAsset,
    /// Preserve native collateral identity; a generic router must resolve its
    /// token metadata before claiming two venues use equivalent settlement.
    pub collateral_token: Option<u32>,
    pub mark_price: Decimal,
    pub oracle_price: Decimal,
    /// Signed fraction payable by a long over one hour, not annualized APY.
    pub hourly_funding: Decimal,
    pub open_interest: Decimal,
}

pub fn parse_markets(meta: PerpMeta, contexts: Vec<AssetContext>) -> Result<Vec<PerpMarket>> {
    parse_markets_for_dex(meta, contexts, "", 0)
}

pub fn parse_markets_for_dex(
    meta: PerpMeta,
    contexts: Vec<AssetContext>,
    dex: &str,
    offset: u32,
) -> Result<Vec<PerpMarket>> {
    ensure!(
        (dex.is_empty() && offset == 0)
            || (!dex.is_empty() && offset >= 110_000 && offset.is_multiple_of(10_000)),
        "Invalid dex/asset offset combination"
    );
    ensure!(
        meta.universe.len() <= 10_000,
        "Too many assets in a perpetual dex"
    );
    let collateral_token = meta.collateral_token;
    ensure!(
        meta.universe.len() == contexts.len(),
        "Metadata/context length mismatch"
    );
    meta.universe
        .into_iter()
        .zip(contexts)
        .enumerate()
        .map(|(index, (asset, ctx))| {
            ensure!(
                !asset.name.is_empty() && asset.sz_decimals <= 6 && asset.max_leverage > 0,
                "Invalid native perpetual metadata"
            );
            ensure!(
                if dex.is_empty() {
                    !asset.name.contains(':')
                } else {
                    asset
                        .name
                        .strip_prefix(dex)
                        .is_some_and(|suffix| suffix.starts_with(':') && suffix.len() > 1)
                },
                "Asset name does not match requested dex namespace"
            );
            let mark_price = parse_decimal(&ctx.mark_px)?;
            let oracle_price = parse_decimal(&ctx.oracle_px)?;
            let open_interest = parse_decimal(&ctx.open_interest)?;
            let hourly_funding = parse_decimal(&ctx.funding)?;
            ensure!(
                mark_price > Decimal::ZERO
                    && oracle_price > Decimal::ZERO
                    && open_interest >= Decimal::ZERO,
                "Invalid asset context"
            );
            Ok(PerpMarket {
                asset_id: offset
                    .checked_add(u32::try_from(index)?)
                    .ok_or_else(|| anyhow::anyhow!("Asset ID overflow"))?,
                asset,
                collateral_token,
                mark_price,
                oracle_price,
                hourly_funding,
                open_interest,
            })
        })
        .collect()
}

pub(super) fn parse_decimal(value: &str) -> Result<Decimal> {
    Ok(Decimal::from_str(value)?)
}

#[derive(Debug, Clone, Deserialize)]
pub struct BookLevel {
    pub px: String,
    pub sz: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct L2Book {
    pub coin: String,
    pub time: u64,
    pub levels: [Vec<BookLevel>; 2],
}

#[derive(Debug, Clone, PartialEq)]
pub struct BestBidAsk {
    pub coin: String,
    pub timestamp_ms: u64,
    pub bid: Decimal,
    pub bid_size: Decimal,
    pub ask: Decimal,
    pub ask_size: Decimal,
}

impl L2Book {
    pub fn best_bid_ask(&self) -> Result<BestBidAsk> {
        let bid = self.levels[0]
            .first()
            .ok_or_else(|| anyhow::anyhow!("Empty bid side"))?;
        let ask = self.levels[1]
            .first()
            .ok_or_else(|| anyhow::anyhow!("Empty ask side"))?;
        let result = BestBidAsk {
            coin: self.coin.clone(),
            timestamp_ms: self.time,
            bid: parse_decimal(&bid.px)?,
            bid_size: parse_decimal(&bid.sz)?,
            ask: parse_decimal(&ask.px)?,
            ask_size: parse_decimal(&ask.sz)?,
        };
        ensure!(
            self.time > 0
                && !self.coin.is_empty()
                && result.bid > Decimal::ZERO
                && result.ask > result.bid
                && result.bid_size > Decimal::ZERO
                && result.ask_size > Decimal::ZERO,
            "Invalid BBO"
        );
        Ok(result)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum TimeInForce {
    Alo,
    Ioc,
    Gtc,
}

#[derive(Debug, Clone)]
pub struct PerpOrder {
    pub is_buy: bool,
    pub limit_price: Decimal,
    pub size: Decimal,
    pub reduce_only: bool,
    pub time_in_force: TimeInForce,
    /// 128-bit hex ID. Caller must persist this before dispatch for reconciliation.
    pub client_order_id: Option<String>,
}

/// Wire field ordering matches the official SDK's insertion-ordered MessagePack.
#[derive(Debug, Clone, Serialize)]
pub struct OrderWire {
    pub(super) a: u32,
    pub(super) b: bool,
    pub(super) p: String,
    pub(super) s: String,
    pub(super) r: bool,
    pub(super) t: LimitWire,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) c: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct LimitWire {
    limit: LimitTif,
}
#[derive(Debug, Clone, Serialize)]
struct LimitTif {
    tif: TimeInForce,
}

impl PerpAsset {
    pub fn validate_order(&self, order: &PerpOrder) -> Result<()> {
        ensure!(
            !self.is_delisted && self.sz_decimals <= 6,
            "Asset delisted or invalid size precision"
        );
        ensure!(
            order.limit_price > Decimal::ZERO && order.size > Decimal::ZERO,
            "Order price/size must be positive"
        );
        let price = order.limit_price.normalize();
        let size = order.size.normalize();
        ensure!(
            size.scale() <= self.sz_decimals,
            "Size exceeds venue lot precision"
        );
        ensure!(
            price.scale() <= 6 - self.sz_decimals,
            "Price exceeds venue decimal precision"
        );
        // Integer prices are explicitly exempt from the five-significant-digit rule.
        if price.fract() != Decimal::ZERO {
            let digits = price.mantissa().unsigned_abs().to_string().len();
            ensure!(digits <= 5, "Price exceeds five significant digits");
        }
        if let Some(id) = &order.client_order_id {
            validate_hex(id, 16)?;
        }
        Ok(())
    }

    /// Conservatively quantize: never increase size or cross the caller's limit.
    pub fn normalize_order(&self, mut order: PerpOrder) -> Result<PerpOrder> {
        ensure!(
            self.sz_decimals <= 6
                && order.limit_price > Decimal::ZERO
                && order.size > Decimal::ZERO,
            "Invalid order or metadata"
        );
        order.size = order
            .size
            .round_dp_with_strategy(self.sz_decimals, rust_decimal::RoundingStrategy::ToZero);
        let allowed_decimals = 6 - self.sz_decimals;
        let integer_digits = order
            .limit_price
            .trunc()
            .normalize()
            .mantissa()
            .unsigned_abs()
            .to_string()
            .len();
        let significant_decimals = if order.limit_price >= Decimal::ONE {
            5_u32.saturating_sub(integer_digits as u32)
        } else {
            // Leading fractional zeros do not count as significant figures.
            let mut scaled = order.limit_price;
            let mut zeros = 0;
            while scaled < Decimal::ONE && zeros < 28 {
                scaled *= Decimal::TEN;
                zeros += 1;
            }
            zeros + 4
        };
        let decimals = allowed_decimals.min(significant_decimals);
        let direction = if order.is_buy {
            rust_decimal::RoundingStrategy::ToNegativeInfinity
        } else {
            rust_decimal::RoundingStrategy::ToPositiveInfinity
        };
        order.limit_price = order
            .limit_price
            .round_dp_with_strategy(decimals, direction)
            .normalize();
        self.validate_order(&order)?;
        Ok(order)
    }
}

impl OrderWire {
    pub fn new(asset_id: u32, asset: &PerpAsset, order: &PerpOrder) -> Result<Self> {
        asset.validate_order(order)?;
        Ok(Self {
            a: asset_id,
            b: order.is_buy,
            p: order.limit_price.normalize().to_string(),
            s: order.size.normalize().to_string(),
            r: order.reduce_only,
            t: LimitWire {
                limit: LimitTif {
                    tif: order.time_in_force,
                },
            },
            c: order.client_order_id.clone(),
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct OrderAction {
    #[serde(rename = "type")]
    kind: &'static str,
    pub orders: Vec<OrderWire>,
    grouping: &'static str,
}
impl OrderAction {
    pub fn new(orders: Vec<OrderWire>) -> Result<Self> {
        ensure!(
            !orders.is_empty() && orders.len() <= 100,
            "Order batch must contain 1..=100 orders"
        );
        Ok(Self {
            kind: "order",
            orders,
            grouping: "na",
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CancelWire {
    pub a: u32,
    pub o: u64,
}
#[derive(Debug, Clone, Serialize)]
pub struct CancelAction {
    #[serde(rename = "type")]
    kind: &'static str,
    pub cancels: Vec<CancelWire>,
}
impl CancelAction {
    pub fn new(cancels: Vec<CancelWire>) -> Result<Self> {
        ensure!(
            !cancels.is_empty() && cancels.len() <= 100,
            "Cancel batch must contain 1..=100 orders"
        );
        Ok(Self {
            kind: "cancel",
            cancels,
        })
    }
}

pub fn validate_hex(value: &str, bytes: usize) -> Result<()> {
    let raw = value
        .strip_prefix("0x")
        .ok_or_else(|| anyhow::anyhow!("Expected 0x-prefixed hex"))?;
    ensure!(
        raw.len() == bytes * 2 && raw.bytes().all(|c| c.is_ascii_hexdigit()),
        "Invalid hex length or encoding"
    );
    Ok(())
}

#[derive(Debug, Clone, PartialEq)]
pub enum ExecutionStatus {
    Resting {
        order_id: u64,
    },
    Filled {
        order_id: u64,
        total_size: Decimal,
        average_price: Decimal,
    },
    Rejected {
        reason: String,
    },
    Canceled,
}

#[derive(Debug, thiserror::Error)]
pub enum SubmissionError {
    #[error("Exchange rejected submission: {0}")]
    Rejected(String),
    #[error("Submission outcome unknown; reconcile by client/order ID before retrying: {0}")]
    Uncertain(String),
}

pub fn parse_execution_response(
    body: &str,
    response_type: &str,
    expected: usize,
) -> Result<Vec<ExecutionStatus>, SubmissionError> {
    let uncertain = |reason: &str| SubmissionError::Uncertain(reason.to_owned());
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|_| uncertain("Invalid JSON response"))?;
    if value["status"] == "err" {
        return Err(SubmissionError::Rejected(
            value["response"]
                .as_str()
                .unwrap_or("Unspecified error")
                .to_owned(),
        ));
    }
    if value["status"] != "ok" || value["response"]["type"] != response_type {
        return Err(uncertain("Unexpected response envelope"));
    }
    let statuses = value["response"]["data"]["statuses"]
        .as_array()
        .ok_or_else(|| uncertain("Missing statuses"))?;
    if statuses.len() != expected {
        return Err(uncertain("Incomplete batch response"));
    }
    statuses
        .iter()
        .map(|status| {
            if let Some(reason) = status.get("error").and_then(|v| v.as_str()) {
                return Ok(ExecutionStatus::Rejected {
                    reason: reason.to_owned(),
                });
            }
            if response_type == "cancel" && status == "success" {
                return Ok(ExecutionStatus::Canceled);
            }
            if response_type == "order" {
                if let Some(order_id) = status
                    .get("resting")
                    .and_then(|s| s.get("oid"))
                    .and_then(|v| v.as_u64())
                {
                    return Ok(ExecutionStatus::Resting { order_id });
                }
                if let Some(fill) = status.get("filled") {
                    let order_id = fill["oid"]
                        .as_u64()
                        .ok_or_else(|| uncertain("Invalid filled order ID"))?;
                    let total_size = fill["totalSz"]
                        .as_str()
                        .and_then(|s| parse_decimal(s).ok())
                        .filter(|v| *v > Decimal::ZERO)
                        .ok_or_else(|| uncertain("Invalid fill size"))?;
                    let average_price = fill["avgPx"]
                        .as_str()
                        .and_then(|s| parse_decimal(s).ok())
                        .filter(|v| *v > Decimal::ZERO)
                        .ok_or_else(|| uncertain("Invalid fill price"))?;
                    return Ok(ExecutionStatus::Filled {
                        order_id,
                        total_size,
                        average_price,
                    });
                }
            }
            Err(uncertain("Unrecognized per-order status"))
        })
        .collect()
}
