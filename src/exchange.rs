//! 交易所抽象层 - 统一的订单执行接口
//!
//! 定义交易所无关的 trait，使策略可以跨交易所复用。

use anyhow::Result;
use async_trait::async_trait;

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

#[derive(Debug, Clone)]
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
    pub side: Side,
    pub price: f64,
    pub size: f64,
    pub filled: f64,
}

// ─── Exchange Trait ──────────────────────────────────────────────────────────

/// 交易所通用接口
#[async_trait]
pub trait Exchange: Send + Sync {
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

    /// 通用批量执行（支持混合 挂单/撤单）
    async fn execute_batch(&self, actions: Vec<BatchAction>) -> Result<BatchResult>;

    /// Get account stats (balance, position, etc.)
    async fn get_account_stats(&self) -> Result<crate::strategy::inventory_neutral_mm::AccountStats>;

    /// 获取限价单类型（PostOnly 或 Limit）
    fn limit_order_type(&self) -> OrderType;
}
