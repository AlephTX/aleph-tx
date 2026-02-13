//! Grid trading strategy

use async_trait::async_trait;
use rust_decimal::Decimal;
use crate::core::{Result, Signal, SignalType, Ticker, Price, Symbol, Order};

use super::{Strategy, StrategyConfig};

/// Grid trading parameters
#[derive(Debug, Clone)]
pub struct GridParams {
    /// Grid levels
    pub grid_levels: usize,
    /// Grid spacing (as fraction, e.g., 0.01 = 1%)
    pub grid_spacing: Decimal,
    /// Position size per grid
    pub position_size: Decimal,
    /// Upper price bound
    pub upper_price: Price,
    /// Lower price bound
    pub lower_price: Price,
}

impl GridParams {
    pub fn from_config(config: &StrategyConfig) -> Result<Self> {
        let params = &config.params;
        Ok(Self {
            grid_levels: params["grid_levels"].as_integer().unwrap_or(10) as usize,
            grid_spacing: params["grid_spacing"]
                .as_float()
                .map(Decimal::try_from)
                .unwrap_or(Ok(Decimal::from(10)))?
            .with_scale(4),
            position_size: Decimal::try_from(
                params["position_size"]
                    .as_float()
                    .unwrap_or(0.01)
            )?.with_scale(4),
            upper_price: Price::from_f64(
                params["upper_price"].as_float().unwrap_or(55000.0)
            ),
            lower_price: Price::from_f64(
                params["lower_price"].as_float().unwrap_or(45000.0)
            ),
        })
    }
}

/// Grid trading strategy
pub struct GridStrategy {
    params: GridParams,
    grid_prices: Vec<Price>,
}

impl GridStrategy {
    pub fn new(params: GridParams) -> Self {
        // Generate grid levels
        let range = params.upper_price.as_f64() - params.lower_price.as_f64();
        let step = range / params.grid_levels as f64;

        let grid_prices = (0..params.grid_levels)
            .map(|i| Price::from_f64(params.lower_price.as_f64() + step * i as f64))
            .collect();

        Self { params, grid_prices }
    }
}

#[async_trait]
impl Strategy for GridStrategy {
    fn name(&self) -> &str {
        "grid"
    }

    async fn initialize(&self, _config: &StrategyConfig) -> Result<()> {
        tracing::info!(
            "Initializing grid strategy: {} levels, spacing: {}",
            self.params.grid_levels,
            self.params.grid_spacing
        );
        Ok(())
    }

    async fn on_tick(&self, ticker: &Ticker) -> Result<Vec<Signal>> {
        let price = ticker.last.as_f64();
        let lower = self.params.lower_price.as_f64();
        let upper = self.params.upper_price.as_f64();

        // Check if price is within grid bounds
        if price < lower || price > upper {
            return Ok(vec![]);
        }

        // Find nearest grid level
        let range = upper - lower;
        let step = range / self.params.grid_levels as f64;
        let level = ((price - lower) / step).floor() as usize;

        // Generate signals
        let mut signals = vec![];

        // Check if near a grid line (within 10% of step)
        let grid_price = lower + step * level as f64;
        if (price - grid_price).abs() / step < 0.1 {
            signals.push(Signal {
                id: uuid::Uuid::new_v4(),
                symbol: ticker.symbol.clone(),
                signal_type: SignalType::EntryLong, // Simplified
                price: ticker.last,
                reason: format!("Grid level {} triggered", level),
                timestamp: chrono::Utc::now(),
            });
        }

        Ok(signals)
    }
}
