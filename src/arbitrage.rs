//! Arbitrage state machine — cross-DEX spread detection and execution.

use crate::shm_reader::ShmBboMessage;

/// Exchange IDs (must match Go feeder constants).
pub const EXCHANGE_HYPERLIQUID: u8 = 1;
pub const EXCHANGE_LIGHTER: u8 = 2;
pub const NUM_EXCHANGES: usize = 3; // 0=unused, 1=HL, 2=Lighter

/// Maximum number of tracked symbols.
pub const MAX_SYMBOLS: usize = 2048;

/// Best Bid/Offer snapshot for one exchange+symbol.
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
}

/// Arbitrage signal — what to do.
#[derive(Debug, Clone)]
pub struct ArbSignal {
    pub symbol_id: u16,
    pub buy_exchange: u8,
    pub sell_exchange: u8,
    pub buy_price: f64,
    pub sell_price: f64,
    pub size: f64,
    pub spread_bps: f64,
}

/// Global market state — the "brain" of the arbitrage engine.
/// Stack-allocated, no heap. Fixed-size arrays indexed by [symbol_id][exchange_id].
pub struct GlobalMarketState {
    bbo_matrix: [[BboSnapshot; NUM_EXCHANGES]; MAX_SYMBOLS],
    min_spread_bps: f64,
}

impl GlobalMarketState {
    pub fn new(min_spread_bps: f64) -> Self {
        Self {
            bbo_matrix: [[BboSnapshot::default(); NUM_EXCHANGES]; MAX_SYMBOLS],
            min_spread_bps,
        }
    }

    /// Update BBO from a shared memory message. Zero allocations.
    #[inline(always)]
    pub fn update(&mut self, msg: &ShmBboMessage) {
        let sym = msg.symbol_id as usize;
        let exch = msg.exchange_id as usize;
        if sym >= MAX_SYMBOLS || exch >= NUM_EXCHANGES {
            return;
        }
        let slot = &mut self.bbo_matrix[sym][exch];
        slot.bid_price = msg.bid_price;
        slot.bid_size = msg.bid_size;
        slot.ask_price = msg.ask_price;
        slot.ask_size = msg.ask_size;
        slot.timestamp_ns = msg.timestamp_ns;
    }

    /// Check for arbitrage between Hyperliquid and Lighter for a given symbol.
    /// Returns `Some(ArbSignal)` if spread exceeds threshold.
    ///
    /// Arb exists when:
    ///   - HL bid > Lighter ask  → buy Lighter, sell HL
    ///   - Lighter bid > HL ask  → buy HL, sell Lighter
    #[inline(always)]
    pub fn check_arbitrage(&self, symbol_id: u16) -> Option<ArbSignal> {
        let sym = symbol_id as usize;
        if sym >= MAX_SYMBOLS {
            return None;
        }

        let hl = &self.bbo_matrix[sym][EXCHANGE_HYPERLIQUID as usize];
        let lt = &self.bbo_matrix[sym][EXCHANGE_LIGHTER as usize];

        if !hl.is_valid() || !lt.is_valid() {
            return None;
        }

        // Direction 1: buy on Lighter (at ask), sell on Hyperliquid (at bid)
        let spread_1 = hl.bid_price - lt.ask_price;
        let mid_1 = (hl.bid_price + lt.ask_price) * 0.5;
        let spread_bps_1 = (spread_1 / mid_1) * 10_000.0;

        if spread_bps_1 > self.min_spread_bps {
            let size = f64::min(hl.bid_size, lt.ask_size);
            return Some(ArbSignal {
                symbol_id,
                buy_exchange: EXCHANGE_LIGHTER,
                sell_exchange: EXCHANGE_HYPERLIQUID,
                buy_price: lt.ask_price,
                sell_price: hl.bid_price,
                size,
                spread_bps: spread_bps_1,
            });
        }

        // Direction 2: buy on Hyperliquid (at ask), sell on Lighter (at bid)
        let spread_2 = lt.bid_price - hl.ask_price;
        let mid_2 = (lt.bid_price + hl.ask_price) * 0.5;
        let spread_bps_2 = (spread_2 / mid_2) * 10_000.0;

        if spread_bps_2 > self.min_spread_bps {
            let size = f64::min(lt.bid_size, hl.ask_size);
            return Some(ArbSignal {
                symbol_id,
                buy_exchange: EXCHANGE_HYPERLIQUID,
                sell_exchange: EXCHANGE_LIGHTER,
                buy_price: hl.ask_price,
                sell_price: lt.bid_price,
                size,
                spread_bps: spread_bps_2,
            });
        }

        None
    }
}

/// Placeholder execution function — will be replaced with real order submission.
pub fn execute_arbitrage(signal: &ArbSignal) {
    tracing::warn!(
        "🚨 ARB DETECTED sym={} buy_exch={} sell_exch={} buy@{:.2} sell@{:.2} size={:.4} spread={:.1}bps",
        signal.symbol_id,
        signal.buy_exchange,
        signal.sell_exchange,
        signal.buy_price,
        signal.sell_price,
        signal.size,
        signal.spread_bps,
    );
    // TODO: submit orders via exchange adapters
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_arb_detection() {
        let mut state = GlobalMarketState::new(5.0); // 5 bps threshold

        // Hyperliquid: BTC-PERP bid=63100 ask=63105
        let hl_msg = ShmBboMessage {
            seqlock: 2,
            msg_type: 1,
            exchange_id: EXCHANGE_HYPERLIQUID,
            symbol_id: 1001,
            timestamp_ns: 1000,
            bid_price: 63100.0,
            bid_size: 1.0,
            ask_price: 63105.0,
            ask_size: 1.0,
            _reserved: [0; 16],
        };
        state.update(&hl_msg);

        // Lighter: BTC-PERP bid=63050 ask=63060 → HL bid > LT ask = arb
        let lt_msg = ShmBboMessage {
            seqlock: 2,
            msg_type: 1,
            exchange_id: EXCHANGE_LIGHTER,
            symbol_id: 1001,
            timestamp_ns: 1001,
            bid_price: 63050.0,
            bid_size: 0.5,
            ask_price: 63060.0,
            ask_size: 0.8,
            _reserved: [0; 16],
        };
        state.update(&lt_msg);

        let signal = state.check_arbitrage(1001);
        assert!(signal.is_some());
        let s = signal.unwrap();
        assert_eq!(s.buy_exchange, EXCHANGE_LIGHTER);
        assert_eq!(s.sell_exchange, EXCHANGE_HYPERLIQUID);
        assert!(s.spread_bps > 5.0);
    }

    #[test]
    fn test_no_arb() {
        let mut state = GlobalMarketState::new(5.0);

        // Both exchanges at same price → no arb
        for exch in [EXCHANGE_HYPERLIQUID, EXCHANGE_LIGHTER] {
            let msg = ShmBboMessage {
                seqlock: 2,
                msg_type: 1,
                exchange_id: exch,
                symbol_id: 1001,
                timestamp_ns: 1000,
                bid_price: 63100.0,
                bid_size: 1.0,
                ask_price: 63105.0,
                ask_size: 1.0,
                _reserved: [0; 16],
            };
            state.update(&msg);
        }

        assert!(state.check_arbitrage(1001).is_none());
    }
}
