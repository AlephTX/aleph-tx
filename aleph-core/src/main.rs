//! AlephTX Core - Binary entry point

use aleph_core::{
    adapter::{BinanceAdapter, ExchangeAdapter},
    signer::HmacSigner,
    engine::{StateMachine, OrderManager, RiskEngine},
    types::Symbol,
};
use std::sync::Arc;
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    tracing::info!("🦀 AlephTX Core Starting...");

    // Initialize components
    let state = Arc::new(StateMachine::new());
    let order_manager = Arc::new(OrderManager::new());
    let risk_engine = Arc::new(RiskEngine::new(Default::default()));

    // Create exchange adapter (Binance testnet)
    let signer = Arc::new(HmacSigner::new(
        std::env::var("BINANCE_API_KEY").unwrap_or_default(),
        std::env::var("BINANCE_SECRET").unwrap_or_default(),
    ));
    let binance = Arc::new(BinanceAdapter::with_credentials(
        std::env::var("BINANCE_API_KEY").unwrap_or_default(),
        std::env::var("BINANCE_SECRET").unwrap_or_default(),
        true, // testnet
    ));

    tracing::info!("Connected to: {}", binance.name());

    // Fetch ticker
    let btc_ticker = binance.fetch_ticker(&Symbol::new("BTCUSDT")).await?;
    tracing::info!("BTC/USDT: bid={}, ask={}", btc_ticker.bid, btc_ticker.ask);

    // Update state
    state.update_ticker(btc_ticker);

    // Keep running
    tracing::info!("✅ AlephTX Core is running");
    
    tokio::signal::ctrl_c().await?;
    tracing::info!("🛑 Shutting down...");

    Ok(())
}
