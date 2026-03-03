// Lighter order submission via WebSocket
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct LighterOrderRequest {
    #[serde(rename = "type")]
    pub msg_type: String, // "create_order"
    pub market_index: i16,
    pub client_order_index: i64,
    pub base_amount: i64,
    pub price: u32,
    pub is_ask: u8, // 0 = BUY, 1 = SELL
    #[serde(rename = "type")]
    pub order_type: u8, // 0 = LIMIT
    pub time_in_force: u8, // 0 = GTC, 3 = IOC
    pub reduce_only: u8,
    pub trigger_price: u32,
    pub order_expiry: i64,
    pub account_index: i64,
    pub api_key_index: u8,
    pub expired_at: i64,
    pub nonce: i64,
    pub signature: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LighterCancelRequest {
    #[serde(rename = "type")]
    pub msg_type: String, // "cancel_order"
    pub market_index: i16,
    pub order_index: i64,
    pub account_index: i64,
    pub api_key_index: u8,
    pub expired_at: i64,
    pub nonce: i64,
    pub signature: String,
}

// Simple market maker strategy
pub struct SimpleMarketMaker {
    market_index: i16,
    symbol_id: u16,
    spread_bps: u32, // Spread in basis points (e.g., 10 = 0.1%)
    order_size: f64, // Order size in BTC
    max_position: f64, // Max position in BTC
}

impl SimpleMarketMaker {
    pub fn new(market_index: i16, symbol_id: u16) -> Self {
        Self {
            market_index,
            symbol_id,
            spread_bps: 10, // 0.1% spread
            order_size: 0.001, // 0.001 BTC per order
            max_position: 0.01, // Max 0.01 BTC position
        }
    }

    pub fn calculate_quotes(&self, mid_price: f64, current_position: f64) -> Option<(f64, f64)> {
        // Don't quote if position is too large
        if current_position.abs() >= self.max_position {
            return None;
        }

        let spread = mid_price * (self.spread_bps as f64) / 10000.0;
        let bid = mid_price - spread / 2.0;
        let ask = mid_price + spread / 2.0;

        Some((bid, ask))
    }

    pub fn should_place_orders(&self, current_position: f64) -> bool {
        current_position.abs() < self.max_position
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_market_maker_quotes() {
        let mm = SimpleMarketMaker::new(0, 1);
        let mid_price = 95000.0;
        let position = 0.0;

        let quotes = mm.calculate_quotes(mid_price, position);
        assert!(quotes.is_some());

        let (bid, ask) = quotes.unwrap();
        assert!(bid < mid_price);
        assert!(ask > mid_price);
        assert!((ask - bid) / mid_price > 0.0009); // At least 0.09% spread
    }

    #[test]
    fn test_market_maker_max_position() {
        let mm = SimpleMarketMaker::new(0, 1);
        assert!(mm.should_place_orders(0.005));
        assert!(!mm.should_place_orders(0.01));
        assert!(!mm.should_place_orders(-0.01));
    }
}
