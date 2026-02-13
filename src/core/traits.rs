//! Core traits - Zero-cost abstractions for extensibility

use async_trait::async_trait;
use crate::core::{Error, Result, types::*};

/// Market data feed trait - implemented by exchanges
#[async_trait]
pub trait MarketFeed: Send + Sync {
    /// Subscribe to ticker updates for a symbol
    async fn subscribe_ticker(&self, symbol: &Symbol) -> Result<()>;

    /// Unsubscribe from ticker updates
    async fn unsubscribe_ticker(&self, symbol: &Symbol) -> Result<()>;

    /// Fetch current ticker
    async fn fetch_ticker(&self, symbol: &Symbol) -> Result<Ticker>;

    /// Fetch order book depth
    async fn fetch_orderbook(&self, symbol: &Symbol, depth: usize) -> Result<OrderBook>;

    /// Get the exchange name
    fn name(&self) -> &str;
}

/// Order book
#[derive(Debug, Clone)]
pub struct OrderBook {
    pub bids: Vec<(Price, Quantity)>,
    pub asks: Vec<(Price, Quantity)>,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

/// Exchange trait - Core trading operations
#[async_trait]
pub trait Exchange: Send + Sync {
    /// Place an order
    async fn place_order(&self, order: &Order) -> Result<Order>;

    /// Cancel an order
    async fn cancel_order(&self, order_id: &Uuid) -> Result<()>;

    /// Get order status
    async fn get_order(&self, order_id: &Uuid) -> Result<Order>;

    /// Get open orders
    async fn get_open_orders(&self, symbol: Option<&Symbol>) -> Result<Vec<Order>>;

    /// Get positions
    async fn get_positions(&self) -> Result<Vec<Position>>;

    /// Get account balance
    async fn get_balance(&self) -> Result<Vec<Balance>>;

    /// Set leverage
    async fn set_leverage(&self, symbol: &Symbol, leverage: u32) -> Result<()>;

    /// Exchange name
    fn name(&self) -> &str;

    /// Supported symbols
    fn supported_symbols(&self) -> Vec<Symbol>;
}

/// Trading strategy trait
#[async_trait]
pub trait Strategy: Send + Sync {
    /// Strategy name
    fn name(&self) -> &str;

    /// Initialize strategy with config
    async fn initialize(&self, config: &toml::Value) -> Result<()>;

    /// Called on each ticker update
    async fn on_tick(&self, ticker: &Ticker) -> Result<Vec<Signal>>;

    /// Called on each order update
    async fn on_order_update(&self, order: &Order) -> Result<()>;
}

/// Risk manager trait
pub trait RiskManager: Send + Sync {
    /// Check if a signal passes risk checks
    fn check_signal(&self, signal: &Signal, positions: &[Position], balance: &[Balance]) -> Result<bool>;

    /// Check if order passes risk checks
    fn check_order(&self, order: &Order, positions: &[Position], balance: &[Balance]) -> Result<bool>;

    /// Get max position size allowed
    fn max_position_size(&self, symbol: &Symbol, balance: &[Balance]) -> Result<Quantity>;
}
