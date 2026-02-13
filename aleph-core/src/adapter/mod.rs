//! Universal Exchange Adapter - The core abstraction
//! All exchanges (CEX/DEX) implement this same interface

use async_trait::async_trait;
use std::sync::Arc;

use crate::types::*;
use crate::error::{Error, Result};

/// Universal Exchange Adapter Trait
/// This is the foundation of AlephTX's extensibility
/// Every exchange (Binance, OKX, EdgeX, Hyperliquid, etc.) implements this
#[async_trait]
pub trait ExchangeAdapter: Send + Sync {
    // ─────────────────────────────────────────────────────────────
    // Identity
    // ─────────────────────────────────────────────────────────────
    
    /// Exchange name (e.g., "binance", "hyperliquid")
    fn name(&self) -> &str;
    
    /// Supported markets (Spot, Futures, Perp)
    fn markets(&self) -> &[Market];
    
    // ─────────────────────────────────────────────────────────────
    // Market Data
    // ─────────────────────────────────────────────────────────────
    
    /// Subscribe to orderbook updates (WebSocket or Chain Event)
    async fn subscribe_orderbook(
        &self, 
        symbols: &[Symbol],
        tx: flume::Sender<OrderbookUpdate>,
    ) -> Result<()>;
    
    /// Subscribe to ticker/trade updates
    async fn subscribe_ticker(
        &self,
        symbols: &[Symbol],
        tx: flume::Sender<Ticker>,
    ) -> Result<()>;
    
    /// Fetch current orderbook snapshot
    async fn fetch_orderbook(&self, symbol: &Symbol, depth: usize) -> Result<Orderbook>;
    
    /// Fetch current ticker
    async fn fetch_ticker(&self, symbol: &Symbol) -> Result<Ticker>;
    
    // ─────────────────────────────────────────────────────────────
    // Trading
    // ─────────────────────────────────────────────────────────────
    
    /// Place an order
    async fn place_order(&self, order: OrderRequest) -> Result<OrderResponse>;
    
    /// Cancel an order
    async fn cancel_order(&self, order_id: &str) -> Result<()>;
    
    /// Get order status
    async fn get_order(&self, order_id: &str) -> Result<Order>;
    
    /// Get all open orders
    async fn get_open_orders(&self, symbol: Option<&Symbol>) -> Result<Vec<Order>>;
    
    // ─────────────────────────────────────────────────────────────
    // Positions & Balance
    // ─────────────────────────────────────────────────────────────
    
    /// Get current positions
    async fn get_positions(&self) -> Result<Vec<Position>>;
    
    /// Get account balance
    async fn get_balance(&self) -> Result<Vec<Balance>>;
    
    // ─────────────────────────────────────────────────────────────
    // Configuration
    // ─────────────────────────────────────────────────────────────
    
    /// Set leverage (for perpetual contracts)
    async fn set_leverage(&self, symbol: &Symbol, leverage: u32) -> Result<()>;
    
    /// Get signer (for signing requests)
    fn signer(&self) -> Arc<dyn Signer>;
}

/// Market types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Market {
    Spot,
    Futures,
    Perp,
}

/// Orderbook update event
#[derive(Debug, Clone)]
pub struct OrderbookUpdate {
    pub symbol: Symbol,
    pub bids: Vec<PriceLevel>,
    pub asks: Vec<PriceLevel>,
    pub timestamp: Timestamp,
}

/// Price level in orderbook
#[derive(Debug, Clone)]
pub struct PriceLevel {
    pub price: Decimal,
    pub quantity: Decimal,
}

/// Order request from strategy/agent
#[derive(Debug, Clone)]
pub struct OrderRequest {
    pub symbol: Symbol,
    pub side: Side,
    pub order_type: OrderType,
    pub quantity: Decimal,
    pub price: Option<Decimal>,
    pub reduce_only: bool,
    pub post_only: bool,
}

/// Order response from exchange
#[derive(Debug, Clone)]
pub struct OrderResponse {
    pub order_id: String,
    pub status: OrderStatus,
    pub filled_quantity: Decimal,
    pub filled_price: Option<Decimal>,
    pub created_at: Timestamp,
}

/// Signer trait - different exchanges use different signing methods
pub trait Signer: Send + Sync {
    /// Sign a request
    fn sign(&self, payload: &[u8]) -> Vec<u8>;
    
    /// Get public address
    fn address(&self) -> &str;
    
    /// Signer type
    fn signer_type(&self) -> SignerType;
}

#[derive(Debug, Clone, Copy)]
pub enum SignerType {
    /// HMAC-SHA256 (CEX like Binance, OKX)
    Hmac,
    /// Ethereum ECDSA (k256)
    Evm,
    /// StarkNet (StarkEx)
    StarkNet,
    /// EdDSA (Solana, Near)
    EdDSA,
}
