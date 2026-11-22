//! Offline normalization of account-scoped REST evidence. No requests or sends.
//! A trusted collector must bind RequestContext to the actual request. A short
//! history page is not a matching-engine watermark or a retention guarantee.
mod wire;
use super::super::{HedgeSide, Leg, OrderStatus};
use super::model::{index, valid_id};
use super::{
    ExecutionFill, LegIdentity, OrderAcknowledgement, OrderTerminal, PreparedOrder, SessionSpec,
};
use anyhow::{Result, ensure};
use rust_decimal::Decimal;
use serde::de::DeserializeOwned;
use std::collections::BTreeMap;
use wire::*;

const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// Captured by the transport, never inferred from response contents. Scope must
/// name the actual account/subaccount, network and HIP-3 dex, not an agent key.
#[derive(Clone)]
pub struct RequestContext {
    pub identity: LegIdentity,
    pub request_started_us: u64,
    pub received_at_us: u64,
}

pub struct NativeEvidence {
    leg: Leg,
    context: RequestContext,
}

pub struct OrderObservation {
    pub acknowledgement: OrderAcknowledgement,
    pub terminal: Option<OrderTerminal>,
    /// HL cancellation status does not establish exact cumulative fill coverage.
    pub canceled_quantity_unproven: bool,
}

#[derive(Clone, PartialEq, Eq)]
pub struct NativeFill {
    pub identity: LegIdentity,
    pub trade_id: String,
    pub exchange_order_id: String,
    pub wire_client_id: Option<String>,
    pub side: HedgeSide,
    pub quantity: Decimal,
    pub original_quantity: Option<Decimal>,
    pub price: Decimal,
    pub fee: Decimal,
    /// Arcus REST does not name this asset; never infer it from quoteAsset.
    pub fee_asset: Option<String>,
    pub venue_at_us: u64,
}

impl NativeFill {
    /// Requires a prior client-ID/order-ID binding and a native fee asset.
    /// Unknown Arcus fee asset remains an explicit integration blocker.
    pub fn execution_fill(
        &self,
        prepared: &PreparedOrder,
        binding: &OrderAcknowledgement,
    ) -> Result<ExecutionFill> {
        ensure!(
            binding.identity == self.identity
                && binding.logical_id == prepared.intent.client_order_id
                && binding.exchange_order_id == self.exchange_order_id
                && prepared.intent.venue == self.identity.venue
                && prepared.intent.instrument == "ANT"
                && prepared.intent.side == self.side
                && self
                    .wire_client_id
                    .as_ref()
                    .is_none_or(|v| v == &prepared.wire_client_id)
                && self
                    .original_quantity
                    .is_none_or(|v| v == prepared.intent.base_quantity)
                && matches!(
                    (prepared.intent.leg, self.identity.venue.as_str()),
                    (Leg::Primary, "arcus") | (Leg::Hedge, "trade_xyz")
                )
                && self.venue_at_us > 0
                && self.price > Decimal::ZERO
                && valid_id(&self.trade_id)
                && valid_id(&self.exchange_order_id)
                && self.quantity > Decimal::ZERO
                && self.quantity <= prepared.intent.base_quantity,
            "Fill does not match durable order binding"
        );
        ensure!(
            match self.side {
                HedgeSide::Buy => self.price <= prepared.intent.limit_price,
                HedgeSide::Sell => self.price >= prepared.intent.limit_price,
            },
            "Fill violates original limit"
        );
        let asset = self
            .fee_asset
            .as_ref()
            .filter(|v| valid_id(v))
            .ok_or_else(|| anyhow::anyhow!("Native fee asset is unknown"))?;
        Ok(ExecutionFill {
            identity: self.identity.clone(),
            logical_id: binding.logical_id.clone(),
            exchange_order_id: self.exchange_order_id.clone(),
            trade_id: self.trade_id.clone(),
            venue_at_us: self.venue_at_us,
            quantity: self.quantity,
            price: self.price,
            fee: self.fee,
            fee_asset: asset.clone(),
        })
    }
}

/// Bounds use the native endpoint's integer unit: Arcus us, Hyperliquid ms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryRange {
    pub from: u64,
    pub to: u64,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pagination {
    /// Only this response is below its cap. Retention and indexing lag unknown.
    BelowCap,
    Split([HistoryRange; 2]),
    /// More fills may exist at this same timestamp; never advance past it.
    SaturatedTimestamp,
}

/// Capture actual query settings. Arcus limit None means its default 1000;
/// Hyperliquid has no limit parameter and requires None.
#[derive(Clone, Copy)]
pub struct HistoryQuery {
    pub range: HistoryRange,
    pub limit: Option<usize>,
    pub aggregate_by_time: bool,
}

pub struct FillPage {
    pub fills: Vec<NativeFill>,
    pub pagination: Pagination,
    /// REST count, bounds and max(fill time) cannot establish this field.
    pub complete_through_venue_us: Option<u64>,
}

pub struct PositionObservation {
    pub context: RequestContext,
    pub position_base: Decimal,
    /// Arcus REST has no source timestamp. HL clearinghouseState.time is ms.
    pub position_venue_at_us: Option<u64>,
    pub fills_complete_through_venue_us: Option<u64>,
    pub other_nonzero_positions: usize,
}

fn decode<T: DeserializeOwned>(raw: &[u8]) -> Result<T> {
    ensure!(
        !raw.is_empty() && raw.len() <= MAX_RESPONSE_BYTES,
        "Invalid native response size"
    );
    serde_json::from_slice(raw).map_err(|_| anyhow::anyhow!("Malformed native response"))
}
fn ms_to_us(value: u64) -> Result<u64> {
    ensure!(value > 0, "Missing source time");
    value
        .checked_mul(1000)
        .ok_or_else(|| anyhow::anyhow!("Source time overflow"))
}
fn side(value: &str, arcus: bool) -> Result<HedgeSide> {
    match (arcus, value) {
        (true, "BUY") | (false, "B") => Ok(HedgeSide::Buy),
        (true, "SELL") | (false, "A") => Ok(HedgeSide::Sell),
        _ => anyhow::bail!("Unknown native side"),
    }
}
fn checked_id(value: &str) -> Result<()> {
    ensure!(valid_id(value), "Invalid native identifier");
    Ok(())
}

impl NativeEvidence {
    pub fn new(spec: &SessionSpec, leg: Leg, context: RequestContext, now_us: u64) -> Result<Self> {
        spec.validate()?;
        ensure!(
            context.identity == spec.legs[index(leg)].identity,
            "Native request scope mismatch"
        );
        ensure!(
            context.request_started_us > 0
                && context.received_at_us >= context.request_started_us
                && now_us >= context.received_at_us
                && now_us - context.request_started_us <= spec.max_observation_age_us,
            "Stale or invalid native request clock"
        );
        Ok(Self { leg, context })
    }
    fn arcus(&self) -> bool {
        self.leg == Leg::Primary
    }
    fn prepared(&self, prepared: &PreparedOrder) -> Result<()> {
        ensure!(
            prepared.intent.leg == self.leg
                && prepared.intent.venue == self.context.identity.venue
                && prepared.intent.instrument == "ANT",
            "Prepared order scope mismatch"
        );
        checked_id(&prepared.intent.client_order_id)?;
        ensure!(
            prepared.intent.base_quantity > Decimal::ZERO
                && prepared.intent.limit_price > Decimal::ZERO,
            "Invalid prepared quantity or limit"
        );
        let wire = if self.arcus() {
            prepared.wire_client_id.strip_prefix('a')
        } else {
            prepared.wire_client_id.strip_prefix("0x")
        };
        ensure!(
            wire.is_some_and(|v| v.len() == 32
                && v.bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())),
            "Invalid prepared wire ID"
        );
        Ok(())
    }
    fn arcus_identity(
        &self,
        address: &str,
        account_index: u8,
        market_id: u16,
        market: &str,
    ) -> Result<()> {
        let id = &self.context.identity;
        ensure!(
            self.arcus()
                && address == id.account
                && Some(account_index) == id.subaccount
                && u32::from(market_id) == id.market_id
                && market == id.market,
            "Arcus response identity mismatch"
        );
        Ok(())
    }
    /// One Arcus order row or a Hyperliquid orderStatus envelope. unknownOid
    /// returns None; it is never transformed into a rejected/terminal order.
    pub fn order(&self, raw: &[u8], prepared: &PreparedOrder) -> Result<Option<OrderObservation>> {
        self.prepared(prepared)?;
        let (oid, status, original, remaining, time, client, price, native_side) = if self.arcus() {
            let v: ArcusOrder = decode(raw)?;
            ensure!(
                v.filled_size
                    .is_none_or(|q| v.original_size.checked_sub(v.remaining_size) == Some(q)),
                "Conflicting native cumulative fill quantity"
            );
            self.arcus_identity(
                &v.address,
                v.account_index,
                v.market_id,
                &v.market_display_name,
            )?;
            (
                v.order_id,
                v.status,
                v.original_size,
                Some(v.remaining_size),
                v.updated_at,
                v.client_id,
                v.price,
                side(&v.side, true)?,
            )
        } else {
            let envelope: HlOrderEnvelope = decode(raw)?;
            if envelope.status == "unknownOid" {
                ensure!(
                    envelope.order.is_none(),
                    "Conflicting unknown order envelope"
                );
                return Ok(None);
            }
            ensure!(envelope.status == "order", "Unknown order envelope");
            let v = envelope
                .order
                .ok_or_else(|| anyhow::anyhow!("Missing native order"))?;
            ensure!(
                v.order.coin == self.context.identity.market,
                "Wrong HIP-3 market"
            );
            (
                v.order.oid.to_string(),
                v.status,
                v.order.orig_sz,
                Some(v.order.sz),
                ms_to_us(v.status_timestamp)?,
                v.order.cloid,
                v.order.limit_px,
                side(&v.order.side, false)?,
            )
        };
        checked_id(&oid)?;
        ensure!(
            client.as_deref() == Some(prepared.wire_client_id.as_str())
                && original == prepared.intent.base_quantity
                && native_side == prepared.intent.side
                && price == prepared.intent.limit_price
                && time > 0,
            "Native order differs from durable intent"
        );
        let remaining = remaining.unwrap();
        ensure!(
            remaining >= Decimal::ZERO && remaining <= original,
            "Invalid native remaining size"
        );
        let terminal_status = match (self.arcus(), status.as_str()) {
            (true, "OPEN" | "PARTIALLY_FILLED" | "PENDING" | "ACK" | "CANCEL_PENDING")
            | (false, "open") => None,
            (true, "FILLED") | (false, "filled") => Some(OrderStatus::Filled),
            (true, "CANCELED" | "MARGIN_CANCELED")
            | (
                false,
                "canceled"
                | "marginCanceled"
                | "vaultWithdrawalCanceled"
                | "openInterestCapCanceled"
                | "selfTradeCanceled"
                | "reduceOnlyCanceled"
                | "siblingFilledCanceled"
                | "delistedCanceled"
                | "liquidatedCanceled"
                | "scheduledCancel",
            ) => Some(OrderStatus::Canceled),
            (true, "REJECTED")
            | (
                false,
                "rejected"
                | "tickRejected"
                | "minTradeNtlRejected"
                | "perpMarginRejected"
                | "reduceOnlyRejected"
                | "badAloPxRejected"
                | "iocCancelRejected"
                | "badTriggerPxRejected"
                | "marketOrderNoLiquidityRejected"
                | "positionIncreaseAtOpenInterestCapRejected"
                | "positionFlipAtOpenInterestCapRejected"
                | "tooAggressiveAtOpenInterestCapRejected"
                | "openInterestIncreaseRejected"
                | "insufficientSpotBalanceRejected"
                | "oracleRejected"
                | "perpMaxPositionRejected",
            ) => Some(OrderStatus::Rejected),
            _ => anyhow::bail!("Unsupported native order status; recovery required"),
        };
        let ack = OrderAcknowledgement {
            identity: self.context.identity.clone(),
            logical_id: prepared.intent.client_order_id.clone(),
            exchange_order_id: oid,
        };
        let canceled_quantity_unproven =
            !self.arcus() && terminal_status == Some(OrderStatus::Canceled);
        let terminal = if let Some(status) = terminal_status.filter(|_| !canceled_quantity_unproven)
        {
            let cumulative = match status {
                OrderStatus::Filled => {
                    ensure!(remaining.is_zero(), "Filled order still has remainder");
                    original
                }
                OrderStatus::Rejected => {
                    ensure!(remaining == original, "Rejected order reports execution");
                    Decimal::ZERO
                }
                _ => original
                    .checked_sub(remaining)
                    .ok_or_else(|| anyhow::anyhow!("Native quantity overflow"))?,
            };
            Some(OrderTerminal {
                identity: ack.identity.clone(),
                logical_id: ack.logical_id.clone(),
                exchange_order_id: Some(ack.exchange_order_id.clone()),
                status,
                cumulative_filled: cumulative,
                venue_at_us: time,
            })
        } else {
            None
        };
        Ok(Some(OrderObservation {
            acknowledgement: ack,
            terminal,
            canceled_quantity_unproven,
        }))
    }

    /// Parse one unaggregated, unfiltered account fill page. Query scope must
    /// retain all markets; unknown orders are not silently assigned to an intent.
    pub fn fills(&self, raw: &[u8], query: HistoryQuery) -> Result<FillPage> {
        let range = query.range;
        ensure!(
            !query.aggregate_by_time && range.from > 0 && range.from <= range.to,
            "Invalid or aggregated fill query"
        );
        let cap = if self.arcus() {
            ensure!(
                range.from >= 100_000_000_000_000,
                "Arcus history requires epoch microseconds"
            );
            let limit = query.limit.unwrap_or(1000);
            ensure!((1..=1000).contains(&limit), "Invalid Arcus page limit");
            limit
        } else {
            ensure!(
                query.limit.is_none(),
                "Hyperliquid does not accept a fill limit"
            );
            ms_to_us(range.to)?;
            2000
        };
        let (mut fills, total) = if self.arcus() {
            let page: ArcusFills = decode(raw)?;
            let total = page.total;
            let mut fills = Vec::with_capacity(page.fills.len());
            for v in page.fills {
                self.arcus_identity(
                    &v.address,
                    v.account_index,
                    v.market_id,
                    &v.market_display_name,
                )?;
                ensure!(
                    matches!(v.role.as_str(), "MAKER" | "TAKER"),
                    "Unknown native liquidity role"
                );
                fills.push(NativeFill {
                    identity: self.context.identity.clone(),
                    trade_id: v.trade_id,
                    exchange_order_id: v.order_id,
                    wire_client_id: v.client_id,
                    side: side(&v.side, true)?,
                    quantity: v.size,
                    original_quantity: Some(v.original_size),
                    price: v.price,
                    fee: v.fee,
                    fee_asset: None,
                    venue_at_us: v.created_at,
                });
            }
            (fills, total)
        } else {
            let page: Vec<HlFill> = decode(raw)?;
            let mut fills = Vec::with_capacity(page.len());
            for v in page {
                ensure!(
                    v.coin == self.context.identity.market,
                    "Unexpected fill market; reconcile other activity separately"
                );
                checked_id(&v.fee_token)?;
                // fee already includes builderFee. Parsing it still checks exactness.
                if let Some(builder) = v.builder_fee {
                    ensure!(builder >= Decimal::ZERO, "Invalid builder fee");
                }
                fills.push(NativeFill {
                    identity: self.context.identity.clone(),
                    trade_id: v.tid.to_string(),
                    exchange_order_id: v.oid.to_string(),
                    wire_client_id: None,
                    side: side(&v.side, false)?,
                    quantity: v.sz,
                    original_quantity: None,
                    price: v.px,
                    fee: v.fee,
                    fee_asset: Some(v.fee_token),
                    venue_at_us: ms_to_us(v.time)?,
                });
            }
            (fills, None)
        };
        let count = fills.len();
        ensure!(
            count <= cap && total.is_none_or(|n| n == count),
            "Invalid native page count"
        );
        let mut seen = BTreeMap::new();
        for fill in fills.drain(..) {
            checked_id(&fill.trade_id)?;
            checked_id(&fill.exchange_order_id)?;
            let native_time = if self.arcus() {
                fill.venue_at_us
            } else {
                fill.venue_at_us / 1000
            };
            ensure!(
                native_time >= range.from
                    && native_time <= range.to
                    && fill.quantity > Decimal::ZERO
                    && fill.price > Decimal::ZERO
                    && fill.original_quantity.is_none_or(|q| q >= fill.quantity),
                "Invalid or out-of-window native fill"
            );
            if let Some(old) = seen.get(&fill.trade_id) {
                ensure!(old == &fill, "Conflicting native duplicate fill");
            } else {
                seen.insert(fill.trade_id.clone(), fill);
            }
        }
        let pagination = if count < cap {
            Pagination::BelowCap
        } else if range.from == range.to {
            Pagination::SaturatedTimestamp
        } else {
            let mid = range.from + (range.to - range.from) / 2;
            Pagination::Split([
                HistoryRange {
                    from: range.from,
                    to: mid,
                },
                HistoryRange {
                    from: mid + 1,
                    to: range.to,
                },
            ])
        };
        let mut fills: Vec<_> = seen.into_values().collect();
        fills.sort_by(|a, b| (a.venue_at_us, &a.trade_id).cmp(&(b.venue_at_us, &b.trade_id)));
        Ok(FillPage {
            fills,
            pagination,
            complete_through_venue_us: None,
        })
    }

    pub fn positions(&self, raw: &[u8]) -> Result<PositionObservation> {
        let (position, source_time, other) = if self.arcus() {
            let page: ArcusPositions = decode(raw)?;
            ensure!(
                page.total.is_none_or(|n| n == page.positions.len()),
                "Position count mismatch"
            );
            let mut target = Decimal::ZERO;
            let mut other = 0;
            for (key, v) in page.positions {
                ensure!(
                    key == v.market_id.to_string()
                        && v.address == self.context.identity.account
                        && Some(v.account_index) == self.context.identity.subaccount,
                    "Position identity mismatch"
                );
                ensure!(
                    matches!(v.side.as_str(), "LONG" | "SHORT")
                        && ((v.size > Decimal::ZERO && v.side == "LONG")
                            || (v.size < Decimal::ZERO && v.side == "SHORT")),
                    "Position side disagrees with signed size"
                );
                if u32::from(v.market_id) == self.context.identity.market_id {
                    ensure!(
                        v.market_display_name == self.context.identity.market,
                        "Position market mismatch"
                    );
                    target = v.size;
                } else {
                    other += 1;
                }
            }
            (target, None, other)
        } else {
            let page: HlPositions = decode(raw)?;
            let mut seen = BTreeMap::new();
            let mut other = 0;
            let mut target = Decimal::ZERO;
            for v in page.asset_positions {
                let p = v.position;
                ensure!(
                    !p.coin.is_empty() && seen.insert(p.coin.clone(), ()).is_none(),
                    "Repeated or missing position market"
                );
                if p.coin == self.context.identity.market {
                    target = p.szi;
                } else if !p.szi.is_zero() {
                    other += 1;
                }
            }
            (target, Some(ms_to_us(page.time)?), other)
        };
        Ok(PositionObservation {
            context: self.context.clone(),
            position_base: position,
            position_venue_at_us: source_time,
            fills_complete_through_venue_us: None,
            other_nonzero_positions: other,
        })
    }
}
#[cfg(test)]
mod tests;
