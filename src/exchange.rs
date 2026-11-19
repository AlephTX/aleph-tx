//! 交易所抽象层 - 统一的订单执行接口
//!
//! 定义交易所无关的 trait，使策略可以跨交易所复用。

pub use crate::exchanges::lighter::risk::{AccountMarginHealth, MarginRegime};
use anyhow::Result;
use async_trait::async_trait;
use rust_decimal::Decimal;

// ─── 通用类型定义 ────────────────────────────────────────────────────────────

/// 订单方向
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum OrderType {
    Limit,
    Market,
    PostOnly,
    Ioc,
}

impl std::fmt::Display for Side {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Side::Buy => write!(f, "buy"),
            Side::Sell => write!(f, "sell"),
        }
    }
}

/// 批量订单参数（不对称大小版本）
#[derive(Debug, Clone)]
pub struct BatchOrderParams {
    pub bid_price: f64,
    pub ask_price: f64,
    pub bid_size: f64,
    pub ask_size: f64,
}

/// 下单结果
#[derive(Debug, Clone)]
pub struct OrderResult {
    pub tx_hash: String,
    pub client_order_index: i64,
}

/// 批量下单结果
#[derive(Debug, Clone)]
pub struct BatchOrderResult {
    pub tx_hashes: Vec<String>,
    pub bid_client_order_index: i64,
    pub ask_client_order_index: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrderParams {
    pub side: Side,
    pub size: f64,
    pub price: f64,
    pub order_type: OrderType,
    pub reduce_only: bool,
}

#[derive(Debug, Clone)]
pub enum BatchAction {
    Place(OrderParams),
    Cancel(i64),
}

#[derive(Debug, Clone)]
pub struct PlaceResult {
    pub client_order_index: i64,
    pub side: Side,
    pub price: f64,
    pub size: f64,
}

#[derive(Debug, Clone)]
pub struct BatchResult {
    pub tx_hashes: Vec<String>,
    pub place_results: Vec<PlaceResult>,
}

/// 订单信息（查询用）
#[derive(Debug, Clone)]
pub struct OrderInfo {
    pub order_id: String,
    pub client_order_index: i64,
    pub order_index: Option<i64>,
    pub side: Side,
    pub price: f64,
    pub size: f64,
    pub filled: f64,
}

/// Observable venue transaction-quota snapshot (`OPT-05`).
/// `remaining == None` means the venue/tier did not report a quota field; never coerce unknown to `0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VolumeQuotaSnapshot {
    pub remaining: Option<i64>,
    pub observed_at_ns: u64,
}

/// Venue-agnostic inputs for perpetual Fair-Mark & pre-liquidation margin evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarginCheckInputs {
    pub index_px: Decimal,
    pub impact_bid: Decimal,
    pub impact_ask: Decimal,
    pub ema_px: Decimal,
    pub anchor_mark_px: Option<Decimal>,
    pub signed_position: Decimal,
    pub snapshot_equity: Decimal,
    pub maintenance_margin_fraction: Decimal,
    pub safety_multiple: Decimal,
}

/// Venue-agnostic account & BBO state returned by a plug-in `HedgeVenue` adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HedgeVenueSnapshot {
    pub position: Decimal,
    pub available_usd: Decimal,
    pub account_time_ms: u64,
    pub bid_px: Decimal,
    pub ask_px: Decimal,
    pub book_time_ms: u64,
}

/// Venue-agnostic outcome of an immediate IOC hedge submission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HedgeIocOutcome {
    Filled {
        filled_size: Decimal,
        avg_fill_px: Option<Decimal>,
        order_id: Option<u64>,
    },
    RestingOrPending {
        order_id: Option<u64>,
    },
    Rejected {
        reason: String,
    },
}

/// Plug-and-play interface for any secondary IOC hedge venue (`Hyperliquid`,
/// `Backpack`, `Arcus`, `Bulk`, `Binance`, etc.) used by delta-neutral strategies.
#[async_trait]
pub trait HedgeVenue: Send + Sync {
    fn venue_id(&self) -> &'static str;
    fn min_order_notional_usd(&self) -> Decimal;
    fn lot_size(&self) -> Decimal;
    async fn account_and_bbo(&self) -> Result<HedgeVenueSnapshot>;
    async fn submit_ioc(
        &self,
        client_order_id: &str,
        side: Side,
        size: Decimal,
        limit_price: Decimal,
        reduce_only: bool,
    ) -> Result<HedgeIocOutcome>;
    async fn cancel_all_open(&self) -> Result<u32>;
}

// ─── Exchange Trait ──────────────────────────────────────────────────────────

/// 交易所通用接口
#[async_trait]
pub trait Exchange: Send + Sync {
    /// Venue identifier for telemetry and diagnostics.
    fn venue_name(&self) -> &'static str {
        "generic"
    }

    /// 买入（市价或限价）
    async fn buy(&self, size: f64, price: f64) -> Result<OrderResult>;

    /// 卖出（市价或限价）
    async fn sell(&self, size: f64, price: f64) -> Result<OrderResult>;

    /// 批量下单（一买一卖，不对称大小）
    async fn place_batch(&self, params: BatchOrderParams) -> Result<BatchOrderResult>;

    /// 撤销单个订单
    async fn cancel_order(&self, order_id: i64) -> Result<()>;

    /// 撤销所有订单
    async fn cancel_all(&self) -> Result<u32>;

    /// 获取活跃订单列表
    async fn get_active_orders(&self) -> Result<Vec<OrderInfo>>;

    /// 平仓（紧急风控用）
    async fn close_all_positions(&self, current_price: f64) -> Result<()>;

    /// Prove terminal orders and a flat position using independent venue evidence.
    async fn recover_flat_without_feed(&self) -> Result<()> {
        anyhow::bail!("Independent shutdown recovery is unavailable for this venue")
    }

    /// Prove terminal orders and reconcile authoritative position without forcing flatten.
    async fn reconcile_non_flat_without_feed(&self) -> Result<f64> {
        anyhow::bail!("Venue cannot reconcile reserved orders without private events")
    }

    /// 通用批量执行（支持混合 挂单/撤单）
    async fn execute_batch(&self, actions: Vec<BatchAction>) -> Result<BatchResult>;

    /// Get account stats (balance, position, etc.)
    async fn get_account_stats(
        &self,
    ) -> Result<crate::strategy::inventory_neutral_mm::AccountStats>;

    /// 获取限价单类型（PostOnly 或 Limit）
    fn limit_order_type(&self) -> OrderType;

    /// Update the execution-boundary ceiling on worst-case single-leg position
    /// (base units) so USD-notional profiles refresh the hard cap dynamically.
    fn set_max_worst_case_position(&self, _max_position: f64) {}

    /// Latest transaction/volume quota reported by the venue (`None` if unsupported/unobserved).
    fn volume_quota_snapshot(&self) -> Option<VolumeQuotaSnapshot> {
        None
    }

    /// Update the execution-boundary reference mid and max allowed entry price
    /// deviation (`bps`) enforced pre-nonce across single and batch non-reducing orders.
    fn set_reference_price_band(&self, _reference_mid: f64, _max_deviation_bps: f64) {}

    /// Evaluate perpetual Fair-Mark and pre-liquidation margin health using venue rules.
    /// Defaults to the universal clamped 3-way median Fair-Mark + safety-multiple model.
    fn evaluate_venue_margin_health(
        &self,
        inputs: &MarginCheckInputs,
    ) -> Option<(Decimal, AccountMarginHealth)> {
        use crate::exchanges::lighter::risk::{
            DEFAULT_IMPACT_CLAMP_RATIO, evaluate_margin_health, fair_mark_price,
        };
        let fair_mark = fair_mark_price(
            inputs.index_px,
            inputs.impact_bid,
            inputs.impact_ask,
            inputs.ema_px,
            DEFAULT_IMPACT_CLAMP_RATIO,
        )
        .ok()?;
        let anchor_mark = inputs.anchor_mark_px.unwrap_or(fair_mark);
        let close_out = inputs.maintenance_margin_fraction / Decimal::TWO;
        let health = evaluate_margin_health(
            inputs.snapshot_equity,
            inputs.signed_position,
            anchor_mark,
            fair_mark,
            inputs.maintenance_margin_fraction,
            close_out,
            inputs.safety_multiple,
        )
        .ok()?;
        Some((fair_mark, health))
    }
}

/// Telemetry counters observed on a read-only zero-credential [`ShadowExchange`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ShadowStats {
    pub batches_executed: u64,
    pub orders_placed: u64,
    pub orders_canceled: u64,
    pub price_band_rejections: u64,
    pub close_requests: u64,
}

/// Zero-credential read-only shadow venue adapter (`MODE=shadow`).
///
/// Contains **no HTTP/WebSocket clients and no signing keys**: all order intents
/// and cancellations are validated against `set_reference_price_band` and fed locally
/// into [`OrderTracker`] via synthetic `ShmPrivateEventV2` updates so Phase-3 shadow
/// runs exercise the full strategy state machine without network dispatch.
pub struct ShadowExchange {
    venue: &'static str,
    tracker: std::sync::Arc<crate::order_tracker::OrderTracker>,
    exchange_id: u8,
    symbol_id: u16,
    post_only: bool,
    initial_equity_usd: f64,
    next_coid: std::sync::atomic::AtomicI64,
    next_oid: std::sync::atomic::AtomicI64,
    next_seq: std::sync::atomic::AtomicU64,
    ref_mid_bits: std::sync::atomic::AtomicU64,
    max_price_band_bps_bits: std::sync::atomic::AtomicU64,
    batches_executed: std::sync::atomic::AtomicU64,
    orders_placed: std::sync::atomic::AtomicU64,
    orders_canceled: std::sync::atomic::AtomicU64,
    price_band_rejections: std::sync::atomic::AtomicU64,
    close_requests: std::sync::atomic::AtomicU64,
}

impl ShadowExchange {
    pub fn new(
        venue: &'static str,
        tracker: std::sync::Arc<crate::order_tracker::OrderTracker>,
        exchange_id: u8,
        symbol_id: u16,
        post_only: bool,
        initial_equity_usd: f64,
    ) -> Self {
        Self {
            venue,
            tracker,
            exchange_id,
            symbol_id,
            post_only,
            initial_equity_usd: if initial_equity_usd.is_finite() && initial_equity_usd > 0.0 {
                initial_equity_usd
            } else {
                50_000.0
            },
            next_coid: std::sync::atomic::AtomicI64::new(1),
            next_oid: std::sync::atomic::AtomicI64::new(100_000),
            next_seq: std::sync::atomic::AtomicU64::new(1),
            ref_mid_bits: std::sync::atomic::AtomicU64::new(0),
            max_price_band_bps_bits: std::sync::atomic::AtomicU64::new(0),
            batches_executed: std::sync::atomic::AtomicU64::new(0),
            orders_placed: std::sync::atomic::AtomicU64::new(0),
            orders_canceled: std::sync::atomic::AtomicU64::new(0),
            price_band_rejections: std::sync::atomic::AtomicU64::new(0),
            close_requests: std::sync::atomic::AtomicU64::new(0),
        }
    }

    pub fn stats(&self) -> ShadowStats {
        use std::sync::atomic::Ordering::Acquire;
        ShadowStats {
            batches_executed: self.batches_executed.load(Acquire),
            orders_placed: self.orders_placed.load(Acquire),
            orders_canceled: self.orders_canceled.load(Acquire),
            price_band_rejections: self.price_band_rejections.load(Acquire),
            close_requests: self.close_requests.load(Acquire),
        }
    }

    fn now_ns() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64
    }

    fn check_price_band(&self, price: f64, reduce_only: bool) -> Result<()> {
        use std::sync::atomic::Ordering::{Acquire, Release};
        anyhow::ensure!(
            price.is_finite() && price > 0.0,
            "Invalid shadow order price {price}"
        );
        if reduce_only {
            return Ok(());
        }
        let ref_mid = f64::from_bits(self.ref_mid_bits.load(Acquire));
        let band_bps = f64::from_bits(self.max_price_band_bps_bits.load(Acquire));
        if ref_mid.is_finite() && ref_mid > 0.0 && band_bps.is_finite() && band_bps > 0.0 {
            let dev_bps = ((price - ref_mid).abs() / ref_mid) * 10_000.0;
            if dev_bps > band_bps + 1e-9 {
                self.price_band_rejections.fetch_add(1, Release);
                anyhow::bail!(
                    "Shadow price-band guard rejected price={price} vs ref_mid={ref_mid} (dev={dev_bps:.2} bps > max={band_bps:.2} bps)"
                );
            }
        }
        Ok(())
    }

    fn check_reduce_only_total(&self, side: Side, total_size: f64, is_passive: bool) -> Result<()> {
        use crate::order_tracker::OrderSide;

        anyhow::ensure!(
            !self.tracker.needs_reconciliation(),
            "Shadow order state requires reconciliation before reduce-only order"
        );
        let (confirmed, pending_same_side) = if is_passive {
            let (confirmed, short, long) = self.tracker.exposure_bounds();
            let pending = match side {
                Side::Buy => (long - confirmed).max(0.0),
                Side::Sell => (confirmed - short).max(0.0),
            };
            (confirmed, pending)
        } else {
            let tracker_side = match side {
                Side::Buy => OrderSide::Buy,
                Side::Sell => OrderSide::Sell,
            };
            (
                self.tracker.confirmed_position(),
                self.tracker.reduce_only_pending_exposure(tracker_side),
            )
        };
        let valid_direction = confirmed.is_finite()
            && match side {
                Side::Sell => confirmed > 1e-9,
                Side::Buy => confirmed < -1e-9,
            };
        anyhow::ensure!(
            valid_direction
                && total_size.is_finite()
                && total_size > 0.0
                && pending_same_side + total_size <= confirmed.abs() * 1.000_001 + 1e-6,
            "Shadow reduce-only {:?} size {:.6} (pending={:.6}) incompatible with confirmed position {:.6}",
            side,
            total_size,
            pending_same_side,
            confirmed
        );
        Ok(())
    }

    fn validate_place(&self, params: &OrderParams) -> Result<()> {
        anyhow::ensure!(
            params.size.is_finite() && params.size > 0.0,
            "Invalid shadow order size {}",
            params.size
        );
        if params.reduce_only {
            self.check_reduce_only_total(
                params.side,
                params.size,
                params.order_type != OrderType::Ioc,
            )?;
        }
        self.check_price_band(params.price, params.reduce_only)
    }

    fn place_one_unchecked(&self, params: OrderParams) -> Result<PlaceResult> {
        use crate::order_tracker::OrderSide;
        use crate::types::ShmPrivateEventV2;
        use std::sync::atomic::Ordering::AcqRel;

        let coid = self.next_coid.fetch_add(1, AcqRel);
        if params.order_type == OrderType::Ioc {
            self.orders_placed.fetch_add(1, AcqRel);
            return Ok(PlaceResult {
                client_order_index: coid,
                side: params.side,
                price: params.price,
                size: params.size,
            });
        }
        let oid = self.next_oid.fetch_add(1, AcqRel);
        let seq = self.next_seq.fetch_add(1, AcqRel);
        let order_side = match params.side {
            Side::Buy => OrderSide::Buy,
            Side::Sell => OrderSide::Sell,
        };
        anyhow::ensure!(
            self.tracker.start_tracking_order(
                coid,
                order_side,
                params.price,
                params.size,
                params.reduce_only
            ),
            "Shadow failed to start_tracking coid={coid}"
        );
        let ev = ShmPrivateEventV2::order_created(
            seq,
            self.exchange_id,
            self.symbol_id,
            oid as u64,
            coid,
            oid,
            params.price,
            params.size,
            params.side == Side::Sell,
            Self::now_ns(),
        );
        self.tracker.apply_event(&ev)?;
        self.orders_placed.fetch_add(1, AcqRel);
        Ok(PlaceResult {
            client_order_index: coid,
            side: params.side,
            price: params.price,
            size: params.size,
        })
    }

    fn cancel_by_order_index(&self, order_index: i64) -> Result<bool> {
        use crate::types::ShmPrivateEventV2;
        use std::sync::atomic::Ordering::AcqRel;

        let Some(order) = self
            .tracker
            .active_orders_snapshot()
            .into_iter()
            .find(|o| o.order_index == Some(order_index))
        else {
            return Ok(false);
        };
        let seq = self.next_seq.fetch_add(1, AcqRel);
        let ev = ShmPrivateEventV2::order_canceled(
            seq,
            self.exchange_id,
            self.symbol_id,
            order.exchange_order_id.unwrap_or(order_index as u64),
            order.client_order_id,
            order_index,
            order.remaining_size(),
            Self::now_ns(),
        );
        self.tracker.apply_event(&ev)?;
        self.orders_canceled.fetch_add(1, AcqRel);
        Ok(true)
    }
}

#[async_trait]
impl Exchange for ShadowExchange {
    fn venue_name(&self) -> &'static str {
        self.venue
    }

    async fn buy(&self, size: f64, price: f64) -> Result<OrderResult> {
        let params = OrderParams {
            side: Side::Buy,
            size,
            price,
            order_type: self.limit_order_type(),
            reduce_only: false,
        };
        self.validate_place(&params)?;
        let placed = self.place_one_unchecked(params)?;
        Ok(OrderResult {
            tx_hash: format!("shadow-tx-{}", placed.client_order_index),
            client_order_index: placed.client_order_index,
        })
    }

    async fn sell(&self, size: f64, price: f64) -> Result<OrderResult> {
        let params = OrderParams {
            side: Side::Sell,
            size,
            price,
            order_type: self.limit_order_type(),
            reduce_only: false,
        };
        self.validate_place(&params)?;
        let placed = self.place_one_unchecked(params)?;
        Ok(OrderResult {
            tx_hash: format!("shadow-tx-{}", placed.client_order_index),
            client_order_index: placed.client_order_index,
        })
    }

    async fn place_batch(&self, params: BatchOrderParams) -> Result<BatchOrderResult> {
        let bid = OrderParams {
            side: Side::Buy,
            size: params.bid_size,
            price: params.bid_price,
            order_type: self.limit_order_type(),
            reduce_only: false,
        };
        let ask = OrderParams {
            side: Side::Sell,
            size: params.ask_size,
            price: params.ask_price,
            order_type: self.limit_order_type(),
            reduce_only: false,
        };
        self.validate_place(&bid)?;
        self.validate_place(&ask)?;
        let bid_res = self.place_one_unchecked(bid)?;
        let ask_res = self.place_one_unchecked(ask)?;
        let batch_n = self
            .batches_executed
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel)
            + 1;
        Ok(BatchOrderResult {
            tx_hashes: vec![format!("shadow-batch-{batch_n}")],
            bid_client_order_index: bid_res.client_order_index,
            ask_client_order_index: ask_res.client_order_index,
        })
    }

    async fn cancel_order(&self, order_id: i64) -> Result<()> {
        let _ = self.cancel_by_order_index(order_id)?;
        Ok(())
    }

    async fn cancel_all(&self) -> Result<u32> {
        let active = self.tracker.active_orders_snapshot();
        let mut canceled = 0u32;
        for order in active {
            if let Some(oid) = order.order_index
                && self.cancel_by_order_index(oid)?
            {
                canceled = canceled.saturating_add(1);
            }
        }
        Ok(canceled)
    }

    async fn get_active_orders(&self) -> Result<Vec<OrderInfo>> {
        use crate::order_tracker::OrderSide;
        Ok(self
            .tracker
            .active_orders_snapshot()
            .into_iter()
            .map(|order| OrderInfo {
                order_id: order.order_index.unwrap_or(0).to_string(),
                client_order_index: order.client_order_id,
                order_index: order.order_index,
                side: match order.side {
                    OrderSide::Buy => Side::Buy,
                    OrderSide::Sell => Side::Sell,
                },
                price: order.price,
                size: order.original_size,
                filled: order.filled_size,
            })
            .collect())
    }

    async fn close_all_positions(&self, _current_price: f64) -> Result<()> {
        self.close_requests
            .fetch_add(1, std::sync::atomic::Ordering::Release);
        let _ = self.cancel_all().await?;
        Ok(())
    }

    async fn recover_flat_without_feed(&self) -> Result<()> {
        let _ = self.cancel_all().await?;
        let epoch = self.tracker.reconciliation_epoch();
        anyhow::ensure!(
            self.tracker.confirm_cleanup(epoch, 0.0),
            "Shadow flat cleanup failed with residual orders"
        );
        Ok(())
    }

    async fn reconcile_non_flat_without_feed(&self) -> Result<f64> {
        let _ = self.cancel_all().await?;
        let epoch = self.tracker.reconciliation_epoch();
        anyhow::ensure!(
            self.tracker.confirm_cleanup(epoch, 0.0),
            "Shadow reconciliation failed with residual orders"
        );
        Ok(0.0)
    }

    async fn execute_batch(&self, actions: Vec<BatchAction>) -> Result<BatchResult> {
        let (mut ro_buy_total, mut ro_sell_total) = (0.0_f64, 0.0_f64);
        let (mut ro_buy_passive, mut ro_sell_passive) = (false, false);
        for action in &actions {
            if let BatchAction::Place(params) = action {
                self.validate_place(params)?;
                if params.reduce_only {
                    let is_passive = params.order_type != OrderType::Ioc;
                    match params.side {
                        Side::Buy => {
                            ro_buy_total += params.size;
                            ro_buy_passive |= is_passive;
                        }
                        Side::Sell => {
                            ro_sell_total += params.size;
                            ro_sell_passive |= is_passive;
                        }
                    }
                }
            }
        }
        if ro_buy_total > 0.0 {
            self.check_reduce_only_total(Side::Buy, ro_buy_total, ro_buy_passive)?;
        }
        if ro_sell_total > 0.0 {
            self.check_reduce_only_total(Side::Sell, ro_sell_total, ro_sell_passive)?;
        }
        let mut place_results = Vec::new();
        for action in actions {
            match action {
                BatchAction::Cancel(oid) => {
                    let _ = self.cancel_by_order_index(oid)?;
                }
                BatchAction::Place(params) => {
                    place_results.push(self.place_one_unchecked(params)?);
                }
            }
        }
        let batch_n = self
            .batches_executed
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel)
            + 1;
        Ok(BatchResult {
            tx_hashes: vec![format!("shadow-batch-{batch_n}")],
            place_results,
        })
    }

    async fn get_account_stats(
        &self,
    ) -> Result<crate::strategy::inventory_neutral_mm::AccountStats> {
        Ok(crate::strategy::inventory_neutral_mm::AccountStats {
            available_balance: self.initial_equity_usd,
            portfolio_value: self.initial_equity_usd,
            position: 0.0,
            leverage: 0.0,
            margin_usage: 0.0,
            last_update: tokio::time::Instant::now(),
        })
    }

    fn limit_order_type(&self) -> OrderType {
        if self.post_only {
            OrderType::PostOnly
        } else {
            OrderType::Limit
        }
    }

    fn set_reference_price_band(&self, reference_mid: f64, max_deviation_bps: f64) {
        use std::sync::atomic::Ordering::Release;
        self.ref_mid_bits.store(reference_mid.to_bits(), Release);
        self.max_price_band_bps_bits
            .store(max_deviation_bps.to_bits(), Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::order_tracker::OrderTracker;
    use std::sync::Arc;

    #[tokio::test]
    async fn shadow_exchange_enforces_price_band_and_drives_order_tracker_offline() {
        let tracker = Arc::new(OrderTracker::new());
        let shadow = ShadowExchange::new(
            "lighter_core",
            Arc::clone(&tracker),
            2,
            1002,
            true,
            50_000.0,
        );
        assert_eq!(shadow.venue_name(), "lighter_core");
        shadow.set_reference_price_band(2000.0, 25.0);

        // Non-reducing fat-finger quote outside +-25 bps fails atomically without mutating tracker.
        let rejected = shadow
            .execute_batch(vec![BatchAction::Place(OrderParams {
                side: Side::Buy,
                size: 0.05,
                price: 2010.0, // +50 bps > 25 bps
                order_type: OrderType::PostOnly,
                reduce_only: false,
            })])
            .await;
        assert!(rejected.is_err());
        assert_eq!(tracker.active_order_count(), 0);
        assert_eq!(shadow.stats().price_band_rejections, 1);

        // Valid bilateral PostOnly batch creates resting orders in OrderTracker, then cancel cleans up.
        let placed = shadow
            .execute_batch(vec![
                BatchAction::Place(OrderParams {
                    side: Side::Buy,
                    size: 0.05,
                    price: 1998.0,
                    order_type: OrderType::PostOnly,
                    reduce_only: false,
                }),
                BatchAction::Place(OrderParams {
                    side: Side::Sell,
                    size: 0.05,
                    price: 2002.0,
                    order_type: OrderType::PostOnly,
                    reduce_only: false,
                }),
            ])
            .await
            .unwrap();
        assert_eq!(placed.place_results.len(), 2);
        assert_eq!(tracker.active_order_count(), 2);
        assert_eq!(shadow.get_active_orders().await.unwrap().len(), 2);

        shadow.recover_flat_without_feed().await.unwrap();
        assert_eq!(tracker.active_order_count(), 0);
        assert_eq!(
            shadow.stats(),
            ShadowStats {
                batches_executed: 1,
                orders_placed: 2,
                orders_canceled: 2,
                price_band_rejections: 1,
                close_requests: 0,
            }
        );
    }
}
