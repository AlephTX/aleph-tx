// src/arbitrage.rs
//! O(1) Scalable Arbitrage Engine
//!
//! Instead of hardcoded pairs (HL vs Lighter), we scan all 5 exchanges
//! to find the Global Best Bid (GBB) and Global Best Ask (GBA) per symbol.
//! This is fully scalable — adding new exchanges requires zero code changes.
//!
//! Hot-path optimization:
//!   - Version-based notification: only check symbol when Go feeder signaled update
//!   - Pre-computed min_spread_ratio: trigger uses fmul, not fdiv
//!   - Zero heap allocations on hot path

use crate::shm_reader::{ShmBboMessage, ShmReader};

pub const EXCHANGE_HYPERLIQUID: u8 = 1;
pub const EXCHANGE_LIGHTER: u8 = 2;
pub const EXCHANGE_EDGEX: u8 = 3;
pub const EXCHANGE_01: u8 = 4;
pub const NUM_EXCHANGES: usize = 6;

pub const MAX_SYMBOLS: usize = 2048;

#[derive(Clone, Copy, Debug, Default)]
pub struct BboSnapshot {
    pub bid_price: f64,
    pub bid_size: f64,
    pub ask_price: f64,
    pub ask_size: f64,
    pub timestamp_ns: u64,
}

impl BboSnapshot {
    #[inline(always)]
    pub fn is_valid(&self) -> bool {
        self.bid_price > 0.0 && self.ask_price > 0.0 && self.bid_price < self.ask_price
    }

    /// Construct from shared memory message (copies to stack).
    #[inline(always)]
    pub fn from_shm(msg: &ShmBboMessage) -> Self {
        Self {
            bid_price: msg.bid_price,
            bid_size: msg.bid_size,
            ask_price: msg.ask_price,
            ask_size: msg.ask_size,
            timestamp_ns: msg.timestamp_ns,
        }
    }
}

/// Best bid/ask across all exchanges for a symbol.
#[derive(Clone, Copy, Debug)]
pub struct GlobalBest {
    pub bid_price: f64,
    pub bid_size: f64,
    pub bid_exchange: u8,
    pub ask_price: f64,
    pub ask_size: f64,
    pub ask_exchange: u8,
    pub timestamp_ns: u64,
}

impl GlobalBest {
    /// Check if a valid cross-exchange spread exists.
    #[inline(always)]
    pub fn has_arb(&self) -> bool {
        self.bid_price > 0.0 
        && self.ask_price > 0.0 
        && self.bid_exchange != self.ask_exchange
        && self.bid_price > self.ask_price
    }

    /// Calculate the raw spread (not bps).
    #[inline(always)]
    pub fn spread(&self) -> f64 {
        self.bid_price - self.ask_price
    }

    /// Calculate mid price.
    #[inline(always)]
    pub fn mid(&self) -> f64 {
        (self.bid_price + self.ask_price) * 0.5
    }
}

/// Arbitrage signal — ready for execution.
#[derive(Clone, Debug)]
pub struct ArbSignal {
    pub symbol_id: u16,
    pub buy_exchange: u8,
    pub sell_exchange: u8,
    pub buy_price: f64,
    pub sell_price: f64,
    pub size: f64,
    pub spread_bps: f64,
    pub timestamp_ns: u64,
}

/// O(1) scalable arbitrage engine.
///
/// Maintains no internal state — purely functional on each call.
/// Scans all NUM_EXCHANGES to find GBB/GBA, triggers on cross-exchange spread.
pub struct ArbitrageEngine {
    min_spread_bps: f64,
    /// Pre-computed: min_spread_bps / 10_000.0
    /// Trigger: spread > mid * min_spread_ratio (one fmul instead of fdiv)
    min_spread_ratio: f64,
}

impl ArbitrageEngine {
    pub fn new(min_spread_bps: f64) -> Self {
        Self {
            min_spread_bps,
            min_spread_ratio: min_spread_bps / 10_000.0,
        }
    }

    /// Scan all 5 exchanges to find global best bid/ask for a symbol.
    /// O(5) = O(1) constant time.
    #[inline(always)]
    pub fn find_global_best(&self, reader: &mut ShmReader, symbol_id: u16) -> Option<GlobalBest> {
        let exchanges = reader.read_all_exchanges(symbol_id);
        
        let mut best_bid_price = 0.0_f64;
        let mut best_bid_size = 0.0_f64;
        let mut best_bid_exchange = 0u8;
        let mut best_ask_price = f64::MAX;
        let mut best_ask_size = 0.0_f64;
        let mut best_ask_exchange = 0u8;
        let mut latest_ts = 0u64;

        for (exchange_id, msg) in exchanges.iter() {
            let bbo = BboSnapshot::from_shm(msg);
            
            if !bbo.is_valid() {
                continue;
            }

            // Track highest bid (buy side — we want to sell to whoever pays most)
            if bbo.bid_price > best_bid_price {
                best_bid_price = bbo.bid_price;
                best_bid_size = bbo.bid_size;
                best_bid_exchange = *exchange_id;
            }

            // Track lowest ask (sell side — we want to buy from whoever asks least)
            if bbo.ask_price < best_ask_price {
                best_ask_price = bbo.ask_price;
                best_ask_size = bbo.ask_size;
                best_ask_exchange = *exchange_id;
            }

            // Track latest timestamp
            if bbo.timestamp_ns > latest_ts {
                latest_ts = bbo.timestamp_ns;
            }
        }

        // Check if we have valid quotes on both sides
        if best_bid_price > 0.0 && best_ask_price < f64::MAX {
            Some(GlobalBest {
                bid_price: best_bid_price,
                bid_size: best_bid_size,
                bid_exchange: best_bid_exchange,
                ask_price: best_ask_price,
                ask_size: best_ask_size,
                ask_exchange: best_ask_exchange,
                timestamp_ns: latest_ts,
            })
        } else {
            None
        }
    }

    /// Check for arbitrage opportunity on a symbol.
    /// Returns Some(ArbSignal) if spread exceeds threshold.
    #[inline(always)]
    pub fn check(&self, reader: &mut ShmReader, symbol_id: u16) -> Option<ArbSignal> {
        let global = self.find_global_best(reader, symbol_id)?;

        // Must be cross-exchange (not same exchange)
        if global.bid_exchange == global.ask_exchange {
            return None;
        }

        let spread = global.spread();
        
        // Fast trigger: fmul instead of fdiv
        if spread > global.mid() * self.min_spread_ratio {
            // Cold path: compute exact bps only when signal triggers
            let spread_bps = (spread / global.mid()) * 10_000.0;

            // Size is limited by the smaller of bid/ask size
            let size = f64::min(global.bid_size, global.ask_size);

            // Direction: buy at ask (lower), sell at bid (higher)
            let (buy_exchange, buy_price, sell_exchange, sell_price) = 
                if global.bid_price > global.ask_price {
                    // Arbitrage: buy cheap, sell expensive
                    // bid > ask means we can buy on ask_exchange and sell on bid_exchange
                    (global.ask_exchange, global.ask_price, 
                     global.bid_exchange, global.bid_price)
                } else {
                    return None; // no positive spread
                };

            return Some(ArbSignal {
                symbol_id,
                buy_exchange,
                sell_exchange,
                buy_price,
                sell_price,
                size,
                spread_bps,
                timestamp_ns: global.timestamp_ns,
            });
        }

        None
    }

    /// Process multiple symbols efficiently.
    /// Takes a list of updated symbol IDs (from version polling).
    #[inline(always)]
    pub fn process_batch(
        &self, 
        reader: &mut ShmReader, 
        symbols: &[u16]
    ) -> Vec<ArbSignal> {
        let mut signals = Vec::with_capacity(symbols.len());
        
        for &sym in symbols {
            if let Some(signal) = self.check(reader, sym) {
                signals.push(signal);
            }
        }
        
        signals
    }
}

/// Execute an arbitrage signal (placeholder — wire to exchange adapters).
pub fn execute_arbitrage(signal: &ArbSignal) {
    tracing::warn!(
        "🚨 ARB sym={} buy_exch={} sell_exch={} buy@{:.2} sell@{:.2} size={:.4} spread={:.1}bps",
        signal.symbol_id,
        signal.buy_exchange,
        signal.sell_exchange,
        signal.buy_price,
        signal.sell_price,
        signal.size,
        signal.spread_bps,
    );
}

/// Exchange ID to name (for logging).
pub fn exchange_name(id: u8) -> &'static str {
    match id {
        1 => "Hyperliquid",
        2 => "Lighter",
        3 => "EdgeX",
        4 => "01",
        _ => "Unknown",
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, AtomicU32, Ordering};
    use std::ptr;

    /// Create a test BBO message.
    fn make_bbo(exchange: u8, bid: f64, ask: f64) -> ShmBboMessage {
        ShmBboMessage {
            seqlock: 2, // even = valid
            msg_type: 1,
            exchange_id: exchange,
            symbol_id: 1001,
            timestamp_ns: 1_000_000_000,
            bid_price: bid,
            bid_size: 1.0,
            ask_price: ask,
            ask_size: 1.0,
            _reserved: [0u8; 16],
        }
    }

    #[test]
    fn test_min_spread_ratio() {
        let engine = ArbitrageEngine::new(5.0);
        assert!((engine.min_spread_ratio - 0.0005).abs() < 1e-12);
    }

    #[test]
    fn test_global_best_single_exchange() {
        // Set up mock data for a single exchange (EdgeX) at reasonable prices
        // The engine should find the best bid/ask even from one exchange
        // But arb only triggers with cross-exchange
    }

    #[test]
    fn test_trigger_equivalence() {
        // Verify fmul trigger equals fdiv calculation
        let min_bps = 5.0;
        let ratio = min_bps / 10_000.0;
        
        let bid = 63100.0_f64;
        let ask = 63060.0_f64;
        let spread = bid - ask;
        let mid = (bid + ask) * 0.5;
        
        let slow_trigger = (spread / mid) * 10_000.0 > min_bps;
        let fast_trigger = spread > mid * ratio;
        
        assert_eq!(fast_trigger, slow_trigger);
    }

    #[test]
    fn test_no_arb_same_exchange() {
        // If both best bid and best ask are from the same exchange,
        // there is no arbitrage (we can't trade with ourselves)
        // This is handled by the cross-exchange check
    }
}
