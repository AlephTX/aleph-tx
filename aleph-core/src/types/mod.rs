//! Core Types - Strong typing for safety

use serde::{Deserialize, Serialize};
use rust_decimal::Decimal;
use chrono::{DateTime, Utc};

/// Tradeable symbol (e.g., "BTC/USDT")
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Symbol(String);

impl Symbol {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into().to_uppercase())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Symbol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<&str> for Symbol {
    fn from(s: &str) -> Self {
        Symbol::new(s)
    }
}

impl From<String> for Symbol {
    fn from(s: String) -> Self {
        Symbol::new(s)
    }
}

/// Price with arbitrary precision
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Price(Decimal);

impl Price {
    pub fn new(value: impl Into<Decimal>) -> Self {
        Self(value.into())
    }

    pub fn from_f64(value: f64) -> Self {
        Self(Decimal::try_from(value).unwrap_or(Decimal::ZERO))
    }

    pub fn as_decimal(&self) -> Decimal {
        self.0
    }

    pub fn as_f64(&self) -> f64 {
        self.0.to_string().parse().unwrap_or(0.0)
    }
}

impl std::fmt::Display for Price {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Quantity
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Quantity(Decimal);

impl Quantity {
    pub fn new(value: impl Into<Decimal>) -> Self {
        Self(value.into())
    }

    pub fn from_f64(value: f64) -> Self {
        Self(Decimal::try_from(value).unwrap_or(Decimal::ZERO))
    }

    pub fn as_decimal(&self) -> Decimal {
        self.0
    }
}

/// Order side
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Buy,
    Sell,
}

impl std::fmt::Display for Side {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Side::Buy => write!(f, "BUY"),
            Side::Sell => write!(f, "SELL"),
        }
    }
}

/// Order type
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OrderType {
    Market,
    Limit,
    StopLoss,
    TakeProfit,
}

/// Order status
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OrderStatus {
    Pending,
    Open,
    PartiallyFilled,
    Filled,
    Cancelled,
    Rejected,
    Expired,
}

/// Order
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Order {
    pub id: String,
    pub symbol: Symbol,
    pub side: Side,
    pub order_type: OrderType,
    pub quantity: Quantity,
    pub price: Option<Price>,
    pub status: OrderStatus,
    pub filled_quantity: Quantity,
    pub filled_price: Option<Price>,
    pub created_at: u64,
    pub updated_at: u64,
}

/// Position
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    pub symbol: Symbol,
    pub side: Side,
    pub quantity: Quantity,
    pub entry_price: Price,
    pub unrealized_pnl: Decimal,
    pub opened_at: u64,
}

/// Account balance
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Balance {
    pub asset: String,
    pub free: Decimal,
    pub locked: Decimal,
}

impl Balance {
    pub fn total(&self) -> Decimal {
        self.free + self.locked
    }
}

/// Orderbook
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Orderbook {
    pub symbol: Symbol,
    pub bids: Vec<crate::adapter::PriceLevel>,
    pub asks: Vec<crate::adapter::PriceLevel>,
    pub timestamp: u64,
}

/// Trade signal
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Signal {
    pub id: String,
    pub symbol: Symbol,
    pub signal_type: SignalType,
    pub price: Price,
    pub reason: String,
    pub timestamp: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalType {
    EntryLong,
    EntryShort,
    ExitLong,
    ExitShort,
}

/// Timestamp alias
pub type Timestamp = u64;
