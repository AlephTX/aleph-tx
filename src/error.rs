//! Error types for the AlephTX trading system

use thiserror::Error;

#[derive(Error, Debug)]
pub enum TradingError {
    #[error("Order placement failed: {0}")]
    OrderFailed(String),

    #[error("Order placement failed after {retries} retries: {reason}")]
    OrderFailedAfterRetries { retries: u32, reason: String },

    #[error("Shadow ledger desync: expected {expected}, got {actual}")]
    LedgerDesync { expected: f64, actual: f64 },

    #[error("Event gap detected: {0} events lost")]
    EventGap(u64),

    #[error("Network error: {0}")]
    Network(#[from] reqwest::Error),

    #[error("Shared memory error: {0}")]
    SharedMemory(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Invalid event type: {0}")]
    InvalidEventType(u8),

    #[error("Out of order event: expected sequence {expected}, got {actual}")]
    OutOfOrderEvent { expected: u64, actual: u64 },

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Authentication error: {0}")]
    Authentication(String),

    #[error("Market data unavailable for symbol {symbol_id} on exchange {exchange_id}")]
    MarketDataUnavailable { symbol_id: u16, exchange_id: u8 },
}

pub type Result<T> = std::result::Result<T, TradingError>;
