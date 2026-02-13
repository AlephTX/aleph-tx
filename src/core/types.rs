//! Core types - Strong typing for safety

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

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

/// Price with arbitrary precision
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Price(Decimal);

impl Price {
    pub fn new(value: impl Into<Decimal>) -> Self {
        Self(value.into())
    }

    pub fn from_f64(value: f64) -> Self {
        Self(Decimal::try_from(value).unwrap())
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

/// Quantity/Size
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Quantity(Decimal);

impl Quantity {
    pub fn new(value: impl Into<Decimal>) -> Self {
        Self(value.into())
    }

    pub fn from_f64(value: f64) -> Self {
        Self(Decimal::try_from(value).unwrap())
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

impl std::fmt::Display for OrderType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OrderType::Market => write!(f, "MARKET"),
            OrderType::Limit => write!(f, "LIMIT"),
            OrderType::StopLoss => write!(f, "STOP_LOSS"),
            OrderType::TakeProfit => write!(f, "TAKE_PROFIT"),
        }
    }
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
    pub id: Uuid,
    pub symbol: Symbol,
    pub side: Side,
    pub order_type: OrderType,
    pub quantity: Quantity,
    pub price: Option<Price>,
    pub status: OrderStatus,
    pub filled_quantity: Quantity,
    pub filled_price: Option<Price>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Order {
    pub fn new_market(symbol: Symbol, side: Side, quantity: Quantity) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            symbol,
            side,
            order_type: OrderType::Market,
            quantity,
            price: None,
            status: OrderStatus::Pending,
            filled_quantity: Quantity::new(0),
            filled_price: None,
            created_at: now,
            updated_at: now,
        }
    }

    pub fn new_limit(symbol: Symbol, side: Side, quantity: Quantity, price: Price) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            symbol,
            side,
            order_type: OrderType::Limit,
            quantity,
            price: Some(price),
            status: OrderStatus::Pending,
            filled_quantity: Quantity::new(0),
            filled_price: None,
            created_at: now,
            updated_at: now,
        }
    }
}

/// Position
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    pub symbol: Symbol,
    pub side: Side,
    pub quantity: Quantity,
    pub entry_price: Price,
    pub unrealized_pnl: Decimal,
    pub opened_at: DateTime<Utc>,
}

/// Ticker/Market data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ticker {
    pub symbol: Symbol,
    pub bid: Price,
    pub ask: Price,
    pub last: Price,
    pub volume_24h: Decimal,
    pub timestamp: DateTime<Utc>,
}

/// Trade signal
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Signal {
    pub id: Uuid,
    pub symbol: Symbol,
    pub signal_type: SignalType,
    pub price: Price,
    pub reason: String,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalType {
    EntryLong,
    EntryShort,
    ExitLong,
    ExitShort,
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
