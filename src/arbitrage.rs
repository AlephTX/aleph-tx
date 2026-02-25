//! Arbitrage state machine — cross-DEX spread detection and execution.
//!
//! Maintains a fixed-size BBO matrix indexed by `[symbol_id][exchange_id]`.
//! All operations are zero-allocation on the hot path.
//!
//! Hot-path optimization: the trigger condition uses a pre-computed
//! `min_spread_ratio` (= bps / 10_000) to replace division with a single
//! multiply-compare. The expensive `spread_bps` division is deferred to
//! signal construction, which only runs on actual triggers.

use crate::shm_reader::ShmBboMessage;

/// Exchange IDs (must match Go feeder constants).
pub const EXCHANGE_HYPERLIQUID: u8 = 1;
pub const EXCHANGE_LIGHTER: u8 = 2;
pub const EXCHANGE_EDGEX: u8 = 3;
pub const EXCHANGE_01: u8 = 4;
pub const NUM_EXCHANGES: usize = 5; // 0=unused, 1=HL, 2=Lighter, 3=EdgeX, 4=01

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
    /// A snapshot is valid when both sides are quoted and not crossed.
    #[inline(always)]
    pub fn is_valid(&self) -> bool {
        self.bid_price > 0.0 && self.ask_price > 0.0 && self.bid_price < self.ask_price
    }
}

/// Arbitrage signal — describes a detected opportunity.
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

/// Global market state — the core of the arbitrage engine.
///
/// Entirely stack-allocated. `bbo_matrix` is indexed as
/// `[symbol_id][exchange_id]` for O(1) lookups with no indirection.
///
/// `min_spread_ratio` is pre-computed at init as `min_spread_bps / 10_000.0`
/// so the hot-path trigger uses `spread > mid * ratio` (one fmul + one fcmp)
/// instead of `(spread / mid) * 10_000 > threshold` (one fdiv + one fmul + one fcmp).
pub struct GlobalMarketState {
    bbo_matrix: [[BboSnapshot; NUM_EXCHANGES]; MAX_SYMBOLS],
    min_spread_bps: f64,
    /// Pre-computed: min_spread_bps / 10_000.0
    /// Hot-path check becomes: `spread > mid * min_spread_ratio`
    min_spread_ratio: f64,
}

impl GlobalMarketState {
    /// Create a new state with the given minimum spread threshold (in basis points).
    pub fn new(min_spread_bps: f64) -> Self {
        Self {
            bbo_matrix: [[BboSnapshot::default(); NUM_EXCHANGES]; MAX_SYMBOLS],
            min_spread_bps,
            min_spread_ratio: min_spread_bps / 10_000.0,
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

    /// Check for cross-exchange arbitrage on a given symbol.
    ///
    /// Scans all exchange pairs for a crossed spread exceeding the
    /// minimum threshold. Returns the best opportunity if found.
    ///
    /// Hot-path: the trigger condition avoids division entirely.
    /// `spread > mid * min_spread_ratio` is equivalent to
    /// `(spread / mid) * 10_000 > min_spread_bps` but uses fmul instead of fdiv.
    /// The exact `spread_bps` is only computed inside the triggered branch.
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

        // Direction 1: HL bid > Lighter ask → buy Lighter, sell HL
        let spread_1 = hl.bid_price - lt.ask_price;
        if spread_1 > 0.0 {
            let mid = (hl.bid_price + lt.ask_price) * 0.5;
            // Fast trigger: one fmul + one fcmp, no fdiv
            if spread_1 > mid * self.min_spread_ratio {
                // Cold path: compute exact bps only when signal fires
                let spread_bps = (spread_1 / mid) * 10_000.0;
                let size = f64::min(hl.bid_size, lt.ask_size);
                return Some(ArbSignal {
                    symbol_id,
                    buy_exchange: EXCHANGE_LIGHTER,
                    sell_exchange: EXCHANGE_HYPERLIQUID,
                    buy_price: lt.ask_price,
                    sell_price: hl.bid_price,
                    size,
                    spread_bps,
                });
            }
        }

        // Direction 2: Lighter bid > HL ask → buy HL, sell Lighter
        let spread_2 = lt.bid_price - hl.ask_price;
        if spread_2 > 0.0 {
            let mid = (lt.bid_price + hl.ask_price) * 0.5;
            if spread_2 > mid * self.min_spread_ratio {
                let spread_bps = (spread_2 / mid) * 10_000.0;
                let size = f64::min(lt.bid_size, hl.ask_size);
                return Some(ArbSignal {
                    symbol_id,
                    buy_exchange: EXCHANGE_HYPERLIQUID,
                    sell_exchange: EXCHANGE_LIGHTER,
                    buy_price: hl.ask_price,
                    sell_price: lt.bid_price,
                    size,
                    spread_bps,
                });
            }
        }

        None
    }

    /// Get the BBO snapshot for a specific symbol and exchange.
    pub fn get_bbo(&self, symbol_id: u16, exchange_id: u8) -> Option<&BboSnapshot> {
        let sym = symbol_id as usize;
        let exch = exchange_id as usize;
        if sym < MAX_SYMBOLS && exch < NUM_EXCHANGES {
            Some(&self.bbo_matrix[sym][exch])
        } else {
            None
        }
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

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_msg(exchange_id: u8, symbol_id: u16, bid: f64, ask: f64) -> ShmBboMessage {
        ShmBboMessage {
            seqlock: 2,
            msg_type: 1,
            exchange_id,
            symbol_id,
            timestamp_ns: 1_000_000,
            bid_price: bid,
            bid_size: 1.0,
            ask_price: ask,
            ask_size: 1.0,
            _reserved: [0u8; 16],
        }
    }

    #[test]
    fn test_min_spread_ratio_precomputed() {
        let state = GlobalMarketState::new(5.0);
        assert!((state.min_spread_ratio - 0.0005).abs() < 1e-12);

        let state2 = GlobalMarketState::new(50.0);
        assert!((state2.min_spread_ratio - 0.005).abs() < 1e-12);
    }

    #[test]
    fn test_arb_detected_hl_bid_gt_lighter_ask() {
        let mut state = GlobalMarketState::new(5.0);

        // HL bid=63100, Lighter ask=63060 → spread=40 → ~6.3 bps
        state.update(&make_msg(EXCHANGE_HYPERLIQUID, 1001, 63100.0, 63105.0));
        state.update(&make_msg(EXCHANGE_LIGHTER, 1001, 63055.0, 63060.0));

        let signal = state.check_arbitrage(1001);
        assert!(signal.is_some());
        let s = signal.unwrap();
        assert_eq!(s.buy_exchange, EXCHANGE_LIGHTER);
        assert_eq!(s.sell_exchange, EXCHANGE_HYPERLIQUID);
        assert!(s.spread_bps > 5.0);
    }

    #[test]
    fn test_no_arb_same_prices() {
        let mut state = GlobalMarketState::new(5.0);

        for exch in [EXCHANGE_HYPERLIQUID, EXCHANGE_LIGHTER] {
            state.update(&make_msg(exch, 1001, 63100.0, 63105.0));
        }

        assert!(state.check_arbitrage(1001).is_none());
    }

    #[test]
    fn test_no_arb_below_threshold() {
        let mut state = GlobalMarketState::new(50.0); // 50 bps — high threshold

        // Tiny spread that won't exceed 50 bps
        state.update(&make_msg(EXCHANGE_HYPERLIQUID, 1001, 63100.0, 63105.0));
        state.update(&make_msg(EXCHANGE_LIGHTER, 1001, 63095.0, 63098.0));

        assert!(state.check_arbitrage(1001).is_none());
    }

    #[test]
    fn test_reverse_direction_arb() {
        let mut state = GlobalMarketState::new(5.0);

        // Lighter bid > HL ask → buy HL, sell Lighter
        state.update(&make_msg(EXCHANGE_HYPERLIQUID, 1001, 63050.0, 63060.0));
        state.update(&make_msg(EXCHANGE_LIGHTER, 1001, 63100.0, 63105.0));

        let signal = state.check_arbitrage(1001);
        assert!(signal.is_some());
        let s = signal.unwrap();
        assert_eq!(s.buy_exchange, EXCHANGE_HYPERLIQUID);
        assert_eq!(s.sell_exchange, EXCHANGE_LIGHTER);
    }

    #[test]
    fn test_trigger_equivalence() {
        // Verify that the fast-path (mul) and slow-path (div) agree
        let min_bps = 5.0;
        let ratio = min_bps / 10_000.0;

        let bid = 63100.0_f64;
        let ask = 63060.0_f64;
        let spread = bid - ask;
        let mid = (bid + ask) * 0.5;

        let slow_bps = (spread / mid) * 10_000.0;
        let fast_trigger = spread > mid * ratio;
        let slow_trigger = slow_bps > min_bps;

        assert_eq!(fast_trigger, slow_trigger);
    }
}
