//! Error handling - Zero-cost, hierarchical errors

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

/// AlephTX error hierarchy
#[derive(Debug, Error)]
pub enum Error {
    /// Configuration errors
    #[error("Configuration error: {0}")]
    Config(String),

    /// Network/IO errors
    #[error("Network error: {0}")]
    Network(#[from] reqwest::Error),

    /// WebSocket errors
    #[error("WebSocket error: {0}")]
    WebSocket(String),

    /// Exchange API errors
    #[error("Exchange error: {0}")]
    Exchange(String),

    /// Trading errors (insufficient balance, etc.)
    #[error("Trading error: {0}")]
    Trading(String),

    /// Risk management errors
    #[error("Risk error: {0}")]
    Risk(String),

    /// Serialization errors
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// Invalid state
    #[error("Invalid state: {0}")]
    InvalidState(String),

    /// Authentication errors
    #[error("Authentication error: {0}")]
    Auth(String),

    /// Not implemented
    #[error("Not implemented: {0}")]
    NotImplemented(String),
}
