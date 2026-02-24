//! Risk Gate — hard limits to prevent catastrophic losses.

use crate::orderbook::LocalOrderbook;
use crate::types::{OrderRequest, Side};
use rust_decimal::Decimal;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum RiskError {
    #[error("order amount {0} exceeds max {1}")]
    OrderTooLarge(Decimal, Decimal),
    
    #[error("position {0} + order {1} exceeds max position {2}")]
    PositionOverflow(Decimal, Decimal, Decimal),
    
    #[error("spread {0} deviates too much from mid price {1} (threshold: {2})")]
    SpreadAnomaly(Decimal, Decimal, Decimal),
    
    #[error("insufficient balance: available {0}, required {1}")]
    InsufficientBalance(Decimal, Decimal),
    
    #[error("trading paused: {0}")]
    TradingPaused(String),
}

/// Risk configuration limits.
#[derive(Debug, Clone)]
pub struct RiskConfig {
    pub max_position_usd: Decimal,
    pub max_order_usd: Decimal,
    pub max_spread_pct: Decimal,      // e.g., 0.5% = 0.005
    pub min_order_usd: Decimal,       // minimum order to avoid dust
}

impl Default for RiskConfig {
    fn default() -> Self {
        Self {
            max_position_usd: Decimal::from(100_000),  // $100k per symbol
            max_order_usd: Decimal::from(50_000),      // $50k per order
            max_spread_pct: Decimal::from(5) / Decimal::from(1000), // 0.5%
            min_order_usd: Decimal::from(10),           // $10 min
        }
    }
}

/// Risk Gate — enforces trading limits before order submission.
pub struct RiskGate {
    config: RiskConfig,
    positions: std::collections::HashMap<String, Decimal>, // symbol → position in USD
    paused: bool,
    pause_reason: Option<String>,
}

impl RiskGate {
    pub fn new(config: RiskConfig) -> Self {
        Self {
            config,
            positions: std::collections::HashMap::new(),
            paused: false,
            pause_reason: None,
        }
    }

    /// Check if an order passes risk controls.
    pub fn check_order(&self, order: &OrderRequest, orderbook: Option<&LocalOrderbook>) -> Result<(), RiskError> {
        // 1. Trading paused?
        if self.paused {
            return Err(RiskError::TradingPaused(
                self.pause_reason.clone().unwrap_or_else(|| "unknown".into())
            ));
        }

        // 2. Order size limits
        let order_value = order.quantity * order.price.unwrap_or(Decimal::ZERO);
        if order_value > self.config.max_order_usd {
            return Err(RiskError::OrderTooLarge(order_value, self.config.max_order_usd));
        }
        if order_value < self.config.min_order_usd {
            return Err(RiskError::OrderTooLarge(order_value, -self.config.min_order_usd)); // reusing error type
        }

        // 3. Position limits
        let current_position = self.positions.get(order.symbol.as_str()).cloned().unwrap_or(Decimal::ZERO);
        let position_delta = match order.side {
            Side::Buy => order_value,
            Side::Sell => -order_value,
        };
        let new_position = current_position + position_delta;
        if new_position.abs() > self.config.max_position_usd {
            return Err(RiskError::PositionOverflow(current_position, position_delta, self.config.max_position_usd));
        }

        // 4. Spread sanity check (if orderbook provided)
        if let Some(ob) = orderbook {
            if let (Some(best_bid), Some(best_ask)) = (ob.best_bid(), ob.best_ask()) {
                let mid = (best_bid.price + best_ask.price) / Decimal::from(2);
                let spread = best_ask.price - best_bid.price;
                let spread_pct = spread / mid;
                if spread_pct > self.config.max_spread_pct {
                    return Err(RiskError::SpreadAnomaly(spread, mid, self.config.max_spread_pct * mid));
                }
            }
        }

        Ok(())
    }

    /// Update position after trade fills.
    pub fn update_position(&mut self, symbol: &str, side: Side, value: Decimal) {
        let entry = self.positions.entry(symbol.to_string()).or_insert(Decimal::ZERO);
        match side {
            Side::Buy => *entry += value,
            Side::Sell => *entry -= value,
        }
        // Clean up zero positions
        if *entry == Decimal::ZERO {
            self.positions.remove(symbol);
        }
    }

    /// Pause all trading.
    pub fn pause(&mut self, reason: impl Into<String>) {
        self.paused = true;
        self.pause_reason = Some(reason.into());
    }

    /// Resume trading.
    pub fn resume(&mut self) {
        self.paused = false;
        self.pause_reason = None;
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{OrderType, Symbol};

    #[test]
    fn test_order_too_large() {
        let config = RiskConfig::default();
        let gate = RiskGate::new(config);
        
        let order = OrderRequest {
            symbol: Symbol::new("BTCUSDT"),
            side: Side::Buy,
            order_type: OrderType::Market,
            quantity: Decimal::from(10),
            price: Some(Decimal::from(50000)),
            reduce_only: false,
            post_only: false,
        }; // 10 * 50000 = $500k > $50k limit
        
        let result = gate.check_order(&order, None);
        assert!(result.is_err());
    }
}
