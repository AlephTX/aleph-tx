// src/strategy/arbitrage.rs
//! O(1) Scalable Arbitrage Engine
//!
//! Scans all exchanges to find the Global Best Bid (GBB) and Global Best Ask (GBA) per symbol.

use crate::shm_reader::ShmBboMessage;
use crate::strategy::Strategy;

pub const NUM_EXCHANGES: usize = 5;

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

pub struct ArbitrageEngine {
    min_spread_bps: f64,
    min_spread_ratio: f64,
    
    // symbol_id -> [ShmBboMessage; 5 exchanges]
    bbo_state: std::collections::HashMap<u16, [ShmBboMessage; NUM_EXCHANGES]>,
}

impl ArbitrageEngine {
    pub fn new(min_spread_bps: f64) -> Self {
        Self {
            min_spread_bps,
            min_spread_ratio: min_spread_bps / 10_000.0,
            bbo_state: std::collections::HashMap::new(),
        }
    }
    
    fn sym_name(&self, symbol_id: u16) -> &'static str {
        match symbol_id {
            1001 => "BTC",
            1002 => "ETH",
            _ => "UNK",
        }
    }
}

impl Strategy for ArbitrageEngine {
    fn name(&self) -> &str {
        "Cross-Exchange Arbitrage"
    }

    fn on_bbo_update(&mut self, symbol_id: u16, exchange_id: u8, bbo: &ShmBboMessage) {
        let exchange_bbos = self.bbo_state.entry(symbol_id).or_insert_with(|| [ShmBboMessage::default(); NUM_EXCHANGES]);
        
        if (exchange_id as usize) < NUM_EXCHANGES {
            exchange_bbos[exchange_id as usize] = *bbo;
            
            // Re-evaluate global best
            let mut best_bid_price = 0.0_f64;
            let mut best_bid_size = 0.0_f64;
            let mut best_bid_exchange = 0u8;
            let mut best_ask_price = f64::MAX;
            let mut best_ask_size = 0.0_f64;
            let mut best_ask_exchange = 0u8;

            for (exch_idx, msg) in exchange_bbos.iter().enumerate() {
                let snap = BboSnapshot::from_shm(msg);
                if !snap.is_valid() { continue; }

                if snap.bid_price > best_bid_price {
                    best_bid_price = snap.bid_price;
                    best_bid_size = snap.bid_size;
                    best_bid_exchange = exch_idx as u8;
                }

                if snap.ask_price < best_ask_price {
                    best_ask_price = snap.ask_price;
                    best_ask_size = snap.ask_size;
                    best_ask_exchange = exch_idx as u8;
                }
            }

            if best_bid_price > 0.0 && best_ask_price < f64::MAX && best_bid_exchange != best_ask_exchange && best_bid_price > best_ask_price {
                let spread = best_bid_price - best_ask_price;
                let mid = (best_bid_price + best_ask_price) * 0.5;

                let spread_bps = (spread / mid) * 10_000.0;
                
                tracing::info!(
                    "📊 {} GBB={:.2}@x{} GBA={:.2}@x{} spread={:.2}bps",
                    self.sym_name(symbol_id),
                    best_bid_price, best_bid_exchange,
                    best_ask_price, best_ask_exchange,
                    spread_bps
                );

                if spread > mid * self.min_spread_ratio {
                    let exec_size = f64::min(best_bid_size, best_ask_size);
                    tracing::warn!(
                        "🚨 ARB sym={} buy_exch={} sell_exch={} buy@{:.2} sell@{:.2} size={:.4} spread={:.1}bps",
                        symbol_id, best_ask_exchange, best_bid_exchange, best_ask_price, best_bid_price, exec_size, spread_bps
                    );
                }
            }
        }
    }

    fn on_idle(&mut self) {
        // No-op
    }
}
