//! AlephTX - Core Library
//! High-performance quantitative trading system

// Public modules
pub mod core;
pub mod feeds;
pub mod exchanges;
pub mod strategies;
pub mod execution;
pub mod risk;
pub mod telegram;

// Re-exports
pub use core::{Config, Error, Result};
