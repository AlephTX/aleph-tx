use aleph_tx::{adapter::*, engine::StateMachine, signer::HmacSigner, types::Symbol};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();
    tracing::info!("🦀 AlephTX Starting...");

    let state = StateMachine::new();
    let signer = Arc::new(HmacSigner::new("", ""));
    let exchange = Arc::new(BinanceAdapter::new(true, signer));

    let ticker = exchange.fetch_ticker(&Symbol::new("BTCUSDT")).await?;
    tracing::info!("BTC/USDT: {} @ {}", ticker.bid, ticker.ask);

    state.update_ticker(ticker);

    tracing::info!("✅ AlephTX Core running");
    Ok(())
}
