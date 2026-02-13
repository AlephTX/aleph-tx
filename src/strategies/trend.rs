//! Trend following strategy

use async_trait::async_trait;
use rust_decimal::Decimal;
use std::collections::VecDeque;
use crate::core::{Result, Signal, SignalType, Ticker, Price};

use super::{Strategy, StrategyConfig};

/// Trend following parameters
#[derive(Debug, Clone)]
pub struct TrendParams {
    /// RSI period
    pub rsi_period: usize,
    /// RSI oversold threshold
    pub rsi_oversold: f64,
    /// RSI overbought threshold
    pub rsi_overbought: f64,
    /// Moving average period
    pub ma_period: usize,
}

impl TrendParams {
    pub fn from_config(config: &StrategyConfig) -> Result<Self> {
        let params = &config.params;
        Ok(Self {
            rsi_period: params["rsi_period"].as_integer().unwrap_or(14) as usize,
            rsi_oversold: params["rsi_oversold"].as_float().unwrap_or(30.0),
            rsi_overbought: params["rsi_overbought"].as_float().unwrap_or(70.0),
            ma_period: params["ma_period"].unwrap_or(50).as_integer().unwrap_or(50) as usize,
        })
    }
}

/// Trend following strategy
pub struct TrendStrategy {
    params: TrendParams,
    price_history: VecDeque<f64>,
}

impl TrendStrategy {
    pub fn new(params: TrendParams) -> Self {
        Self {
            params,
            price_history: VecDeque::with_capacity(200),
        }
    }

    /// Calculate RSI
    fn calculate_rsi(&self) -> Option<f64> {
        if self.price_history.len() < self.params.rsi_period + 1 {
            return None;
        }

        let mut gains = 0.0;
        let mut losses = 0.0;

        let prices: Vec<f64> = self.price_history.iter().rev().take(self.params.rsi_period + 1).cloned().collect();

        for i in 1..prices.len() {
            let change = prices[i] - prices[i - 1];
            if change > 0 {
                gains += change;
            } else {
                losses -= change;
            }
        }

        let avg_gain = gains / self.params.rsi_period as f64;
        let avg_loss = losses / self.params.rsi_period as f64;

        if avg_loss == 0.0 {
            return Some(100.0);
        }

        let rs = avg_gain / avg_loss;
        let rsi = 100.0 - (100.0 / (1.0 + rs));
        Some(rsi)
    }

    /// Calculate Simple Moving Average
    fn calculate_sma(&self) -> Option<f64> {
        if self.price_history.len() < self.params.ma_period {
            return None;
        }

        let sum: f64 = self.price_history.iter().rev().take(self.params.ma_period).sum();
        Some(sum / self.params.ma_period as f64)
    }
}

#[async_trait]
impl Strategy for TrendStrategy {
    fn name(&self) -> &str {
        "trend"
    }

    async fn initialize(&self, config: &StrategyConfig) -> Result<()> {
        tracing::info!(
            "Initializing trend strategy: RSI({}, {}/{}), MA({})",
            self.params.rsi_period,
            self.params.rsi_oversold,
            self.params.rsi_overbought,
            self.params.ma_period
        );
        Ok(())
    }

    async fn on_tick(&self, ticker: &Ticker) -> Result<Vec<Signal>> {
        let price = ticker.last.as_f64();
        self.price_history.push_back(price);

        // Keep history bounded
        if self.price_history.len() > 200 {
            self.price_history.pop_front();
        }

        let rsi = match self.calculate_rsi() {
            Some(r) => r,
            None => return Ok(vec![]),
        };

        let ma = match self.calculate_sma() {
            Some(m) => m,
            None => return Ok(vec![]),
        };

        // Generate signals
        let mut signals = vec![];

        if rsi < self.params.rsi_oversold && price > ma {
            signals.push(Signal {
                id: uuid::Uuid::new_v4(),
                symbol: ticker.symbol.clone(),
                signal_type: SignalType::EntryLong,
                price: ticker.last,
                reason: format!("RSI oversold ({:.1}) + price above MA({:.2})", rsi, ma),
                timestamp: chrono::Utc::now(),
            });
        } else if rsi > self.params.rsi_overbought && price < ma {
            signals.push(Signal {
                id: uuid::Uuid::new_v4(),
                symbol: ticker.symbol.clone(),
                signal_type: SignalType::EntryShort,
                price: ticker.last,
                reason: format!("RSI overbought ({:.1}) + price below MA({:.2})", rsi, ma),
                timestamp: chrono::Utc::now(),
            });
        }

        Ok(signals)
    }
}
