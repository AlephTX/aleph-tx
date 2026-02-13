//! Risk Engine - Position limits and risk controls

use rust_decimal::Decimal;

use crate::types::{Order, Position, Balance, Symbol, Signal, SignalType};

/// Risk configuration
#[derive(Debug, Clone)]
pub struct RiskConfig {
    /// Max portfolio risk (0.0-1.0)
    pub max_portfolio_risk: Decimal,
    /// Max position risk (0.0-1.0) 
    pub max_position_risk: Decimal,
    /// Max drawdown % before stopping
    pub max_drawdown: Decimal,
    /// Emergency stop enabled
    pub emergency_stop: bool,
    /// Max positions per symbol
    pub max_positions_per_symbol: usize,
    /// Max total positions
    pub max_total_positions: usize,
}

impl Default for RiskConfig {
    fn default() -> Self {
        Self {
            max_portfolio_risk: Decimal::from(20) / Decimal::from(100),
            max_position_risk: Decimal::from(10) / Decimal::from(100),
            max_drawdown: Decimal::from(15) / Decimal::from(100),
            emergency_stop: true,
            max_positions_per_symbol: 1,
            max_total_positions: 5,
        }
    }
}

/// Risk Engine - Evaluates and enforces risk rules
pub struct RiskEngine {
    config: RiskConfig,
}

impl RiskEngine {
    pub fn new(config: RiskConfig) -> Self {
        Self { config }
    }

    /// Check if signal passes risk rules
    pub fn check_signal(&self, signal: &Signal, positions: &[Position], balances: &[Balance]) -> Result<bool, String> {
        // Emergency stop check
        if self.config.emergency_stop {
            let drawdown = self.calculate_drawdown(positions, balances);
            if drawdown > self.config.max_drawdown {
                return Err(format!("Emergency stop: drawdown {}% exceeds max {}%", 
                    drawdown * Decimal::from(100), 
                    self.config.max_drawdown * Decimal::from(100)));
            }
        }

        // Position limit check
        let total_positions = positions.len();
        if total_positions >= self.config.max_total_positions {
            return Err("Max total positions reached".to_string());
        }

        // Symbol position check
        let symbol_positions = positions.iter()
            .filter(|p| p.symbol == signal.symbol)
            .count();
        if symbol_positions >= self.config.max_positions_per_symbol {
            return Err(format!("Max positions for {} reached", signal.symbol));
        }

        Ok(true)
    }

    /// Check if order passes risk rules
    pub fn check_order(&self, order: &Order, positions: &[Position], balances: &[Balance]) -> Result<bool, String> {
        // Convert order to signal for unified checking
        let signal = Signal {
            id: order.id.clone(),
            symbol: order.symbol.clone(),
            signal_type: match order.side {
                Side::Buy => SignalType::EntryLong,
                Side::Sell => SignalType::ExitLong,
            },
            price: order.price.unwrap_or(crate::types::Price::new(Decimal::ZERO)),
            reason: "order_check".to_string(),
            timestamp: order.created_at,
        };

        self.check_signal(&signal, positions, balances)
    }

    /// Calculate current drawdown
    fn calculate_drawdown(&self, positions: &[Position], _balances: &[Balance]) -> Decimal {
        // Simplified: sum of unrealized PnL
        // In production, would track peak equity
        let total_pnl: Decimal = positions.iter()
            .map(|p| p.unrealized_pnl)
            .sum();

        // For now, return 0 if profitable, else drawdown
        if total_pnl > Decimal::ZERO {
            Decimal::ZERO
        } else {
            total_pnl.abs()
        }
    }

    /// Calculate max order size
    pub fn max_order_size(&self, symbol: &Symbol, balance: &[Balance]) -> Decimal {
        // Find base currency (USDT)
        let usdt = balance.iter()
            .find(|b| b.asset == "USDT")
            .map(|b| b.total())
            .unwrap_or(Decimal::ZERO);

        usdt * self.config.max_position_risk
    }
}
