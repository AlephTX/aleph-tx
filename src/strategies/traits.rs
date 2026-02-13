//! Strategy traits and runner

use async_trait::async_trait;
use crate::core::{Result, Signal, Ticker, Order, Position, Balance, Symbol};

/// Strategy configuration
#[derive(Debug, Clone, serde::Deserialize)]
pub struct StrategyConfig {
    /// Strategy name
    pub name: String,

    /// Trading symbols
    pub symbols: Vec<String>,

    /// Position size (as fraction of balance)
    pub position_size: f64,

    /// Max positions
    pub max_positions: usize,

    /// Custom parameters
    #[serde(flatten)]
    pub params: toml::Value,
}

/// Strategy runner - orchestrates strategy execution
pub struct StrategyRunner {
    strategies: Vec<Box<dyn Strategy>>,
}

impl StrategyRunner {
    pub fn new() -> Self {
        Self {
            strategies: vec![],
        }
    }

    pub fn add_strategy(&mut self, strategy: impl Strategy + 'static) {
        self.strategies.push(Box::new(strategy));
    }

    /// Process ticker and get signals from all strategies
    pub async fn process_tick(&self, ticker: &Ticker) -> Result<Vec<Signal>> {
        let mut signals = vec![];

        for strategy in &self.strategies {
            let strategy_signals = strategy.on_tick(ticker).await?;
            signals.extend(strategy_signals);
        }

        Ok(signals)
    }

    /// Check all strategies on order update
    pub async fn on_order_update(&self, order: &Order) -> Result<()> {
        for strategy in &self.strategies {
            strategy.on_order_update(order).await?;
        }
        Ok(())
    }
}

impl Default for StrategyRunner {
    fn default() -> Self {
        Self::new()
    }
}

/// Base strategy trait
#[async_trait]
pub trait Strategy: Send + Sync {
    /// Strategy name
    fn name(&self) -> &str;

    /// Initialize strategy
    async fn initialize(&self, config: &StrategyConfig) -> Result<()>;

    /// Process ticker and generate signals
    async fn on_tick(&self, ticker: &Ticker) -> Result<Vec<Signal>>;

    /// Handle order updates
    async fn on_order_update(&self, _order: &Order) -> Result<()>;
}
