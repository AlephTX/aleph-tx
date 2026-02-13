//! Error handling - Hierarchical, zero-cost errors

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

/// AlephTX error hierarchy
#[derive(Debug, Error)]
pub enum Error {
    /// Configuration errors
    #[error("Config: {0}")]
    Config(String),

    /// Network/IO errors
    #[error("Network: {0}")]
    Network(#[from] reqwest::Error),

    /// WebSocket errors  
    #[error("WebSocket: {0}")]
    WebSocket(String),

    /// Exchange API errors
    #[error("Exchange: {0}")]
    Exchange(String),

    /// Trading errors
    #[error("Trading: {0}")]
    Trading(String),

    /// Risk management
    #[error("Risk: {0}")]
    Risk(String),

    /// Serialization
    #[error("Serialization: {0}")]
    Serialization(#[from] serde_json::Error),

    /// Invalid state
    #[error("Invalid state: {0}")]
    InvalidState(String),

    /// Authentication
    #[error("Auth: {0}")]
    Auth(String),

    /// Not implemented
    #[error("Not implemented: {0}")]
    NotImplemented(String),

    /// Signature error
    #[error("Signature: {0}")]
    Signature(String),
}
