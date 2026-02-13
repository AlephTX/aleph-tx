//! AlephTX - Main binary entry point

use aleph_tx::{
    Config,
    Binance,
};
use aleph_tx::strategies::{StrategyRunner, GridStrategy, TrendStrategy, StrategyConfig};
use aleph_tx::core::{Symbol, RunMode};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    tracing::info!("🚀 AlephTX Starting...");

    // Load configuration
    let config = match std::path::PathBuf::from("config.toml").exists() {
        true => Config::load(&std::path::PathBuf::from("config.toml"))?,
        false => {
            tracing::warn!("No config.toml found, using defaults");
            Config::default()
        }
    };

    tracing::info!("Mode: {:?}", config.app.mode);

    // Initialize exchange (Binance testnet)
    let exchange = std::sync::Arc::new(Binance::new(config.app.mode == RunMode::Paper));

    // Initialize strategies
    let mut runner = StrategyRunner::new();

    // Add grid strategy
    let grid_config = StrategyConfig {
        name: "grid".to_string(),
        symbols: config.trading.symbols.clone(),
        position_size: config.trading.order_size_fraction,
        max_positions: config.trading.max_positions,
        params: toml::from_str(r#"
            grid_levels = 10
            grid_spacing = 0.01
            position_size = 0.01
            upper_price = 55000.0
            lower_price = 45000.0
        "#)?,
    };
    let grid_params = aleph_tx::strategies::grid::GridParams::from_config(&grid_config)?;
    runner.add_strategy(GridStrategy::new(grid_params));

    // Add trend strategy
    let trend_config = StrategyConfig {
        name: "trend".to_string(),
        symbols: config.trading.symbols.clone(),
        position_size: config.trading.order_size_fraction,
        max_positions: config.trading.max_positions,
        params: toml::from_str(r#"
            rsi_period = 14
            rsi_oversold = 30.0
            rsi_overbought = 70.0
            ma_period = 50
        "#)?,
    };
    let trend_params = aleph_tx::strategies::trend::TrendParams::from_config(&trend_config)?;
    runner.add_strategy(TrendStrategy::new(trend_params));

    tracing::info!("📈 Strategies loaded: grid, trend");

    // Main trading loop (placeholder)
    if config.app.mode == RunMode::Paper {
        tracing::info!("📝 Running in paper trading mode");
    }

    // Keep running
    tokio::signal::ctrl_c().await?;
    tracing::info!("🛑 Shutting down...");

    Ok(())
}
