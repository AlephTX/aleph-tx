use crate::shm_reader::ShmBboMessage;
use crate::strategy::Strategy;

/// MarketMakerStrategy executes a statistical grid or market-making algorithm
/// exclusively on a single exchange to provide liquidity and capture spread.
pub struct MarketMakerStrategy {
    target_exchange_id: u8,
    symbol_id: u16,
    half_spread_bps: f64,
}

impl MarketMakerStrategy {
    pub fn new(target_exchange_id: u8, symbol_id: u16, half_spread_bps: f64) -> Self {
        Self {
            target_exchange_id,
            symbol_id,
            half_spread_bps,
        }
    }
}

impl Strategy for MarketMakerStrategy {
    fn name(&self) -> &str {
        "Single-Exchange Market Maker"
    }

    fn on_bbo_update(&mut self, symbol_id: u16, exchange_id: u8, bbo: &ShmBboMessage) {
        if symbol_id != self.symbol_id || exchange_id != self.target_exchange_id {
            return;
        }

        if bbo.bid_price > 0.0 && bbo.ask_price > 0.0 {
            let mid_price = (bbo.bid_price + bbo.ask_price) / 2.0;

            let my_bid = mid_price * (1.0 - (self.half_spread_bps / 10000.0));
            let my_ask = mid_price * (1.0 + (self.half_spread_bps / 10000.0));

            // Log purely to demonstrate strategy logic evaluation
            tracing::debug!(
                "📈 [MM] exchange={} sym={} mid={:.2} quoting bid={:.2} ask={:.2}",
                self.target_exchange_id, self.symbol_id, mid_price, my_bid, my_ask
            );
            
            // TODO: dispatch local grid update logic
        }
    }

    fn on_idle(&mut self) {
        // e.g. Evaluate stop losses or risk bounds on idle loop
    }
}
