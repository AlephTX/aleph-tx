//! AlephTX Core Library
//! Institutional-grade quantitative trading system

// Public modules
pub mod adapter;
pub mod engine;
pub mod signer;
pub mod types;
pub mod error;
pub mod messaging;

// Re-exports
pub use error::{Error, Result};
pub use types::*;
