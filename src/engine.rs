//! Legacy state machine — DEPRECATED.
//!
//! This module used `Arc<RwLock<HashMap<String, Ticker>>>` for market state,
//! which introduces:
//!   - Heap allocations on every update (String keys, Ticker clones)
//!   - OS-level lock contention (RwLock syscalls under contention)
//!   - HashMap hashing overhead (~20-50ns per lookup)
//!
//! All L0/L1 hot-path routing now uses `arbitrage::GlobalMarketState`, which
//! is entirely stack-allocated with O(1) array indexing and zero locks.
//!
//! This module is retained only for cold-path diagnostics and REST API
//! queries where allocation cost is irrelevant. Do NOT use on the hot path.

#![deprecated(
    since = "0.2.0",
    note = "Use arbitrage::GlobalMarketState for hot-path routing. \
            StateMachine is retained for cold-path diagnostics only."
)]

use crate::types::Ticker;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;

/// Legacy ticker state — cold-path only.
///
/// Suitable for REST API responses, logging, and diagnostics where
/// microsecond latency is not a concern.
pub struct StateMachine {
    tickers: Arc<RwLock<HashMap<String, Ticker>>>,
}

#[allow(deprecated)]
impl StateMachine {
    pub fn new() -> Self {
        Self {
            tickers: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl Default for StateMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl StateMachine {
    /// Update a ticker. Allocates on insert (String key + Ticker clone).
    /// Cold-path only — do NOT call from the spin-loop.
    pub fn update_ticker(&self, ticker: Ticker) {
        self.tickers
            .write()
            .insert(ticker.symbol.to_string(), ticker);
    }

    /// Get a ticker by symbol name. Clones the Ticker on return.
    /// Cold-path only.
    pub fn get_ticker(&self, symbol: &str) -> Option<Ticker> {
        self.tickers.read().get(symbol).cloned()
    }

    /// Snapshot all tickers (for diagnostics / REST).
    pub fn snapshot(&self) -> HashMap<String, Ticker> {
        self.tickers.read().clone()
    }
}
