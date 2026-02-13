use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Symbol(pub String);

impl Symbol {
    pub fn new(s: impl Into<String>) -> Self { Self(s.into().to_uppercase()) }
    pub fn as_str(&self) -> &str { &self.0 }
}

impl std::fmt::Display for Symbol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, "{}", self.0) }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side { Buy, Sell }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OrderType { Market, Limit, StopLoss, TakeProfit }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OrderStatus { Pending, Open, PartiallyFilled, Filled, Cancelled, Rejected, Expired }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Market { Spot, Futures, Perp }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalType { EntryLong, EntryShort, ExitLong, ExitShort }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Order {
    pub id: String,
    pub symbol: Symbol,
    pub side: Side,
    pub order_type: OrderType,
    pub quantity: Decimal,
    pub price: Option<Decimal>,
    pub status: OrderStatus,
    pub filled_quantity: Decimal,
    pub filled_price: Option<Decimal>,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    pub symbol: Symbol,
    pub side: Side,
    pub quantity: Decimal,
    pub entry_price: Decimal,
    pub unrealized_pnl: Decimal,
    pub opened_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Balance {
    pub asset: String,
    pub free: Decimal,
    pub locked: Decimal,
}

impl Balance {
    pub fn total(&self) -> Decimal { self.free + self.locked }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceLevel {
    pub price: Decimal,
    pub quantity: Decimal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Orderbook {
    pub symbol: Symbol,
    pub bids: Vec<PriceLevel>,
    pub asks: Vec<PriceLevel>,
    pub timestamp: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ticker {
    pub symbol: Symbol,
    pub bid: Decimal,
    pub ask: Decimal,
    pub last: Decimal,
    pub volume_24h: Decimal,
    pub timestamp: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Signal {
    pub id: String,
    pub symbol: Symbol,
    pub signal_type: SignalType,
    pub price: Decimal,
    pub reason: String,
    pub timestamp: u64,
}

/// Orderbook update event
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderbookUpdate {
    pub symbol: Symbol,
    pub bids: Vec<PriceLevel>,
    pub asks: Vec<PriceLevel>,
    pub timestamp: u64,
}

/// Order request
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

/// Order response
#[derive(Debug, Clone)]
pub struct OrderResponse {
    pub order_id: String,
    pub status: OrderStatus,
    pub filled_quantity: Decimal,
    pub filled_price: Option<Decimal>,
    pub created_at: u64,
}
