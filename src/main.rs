use aleph_tx::config::{AppConfig, EXCH_BACKPACK, EXCH_EDGEX, SYM_ETH};
use aleph_tx::data_plane;
use aleph_tx::strategy::{
    Strategy, arbitrage::ArbitrageEngine, backpack_mm::BackpackMMStrategy,
    edgex_mm::MarketMakerStrategy,
};
use tokio::signal;
use tracing_subscriber::{EnvFilter, fmt};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1. Initialize logger
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,aleph_tx=debug"));
    fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_thread_ids(false)
        .with_level(true)
        .init();

    tracing::info!("🦀 AlephTX Core v4 starting (Institutional Pipeline)...");

    // 2. Load configuration
    let config = AppConfig::load_default();
    
    // 3. Initialize strategies
    let mut strategies: Vec<Box<dyn Strategy>> = vec![
        Box::new(ArbitrageEngine::new(25.0)),
        Box::new(MarketMakerStrategy::new(
            EXCH_EDGEX, 
            SYM_ETH, 
            25.0,
            config.edgex.clone(),
        )),
        Box::new(BackpackMMStrategy::new(
            EXCH_BACKPACK,
            SYM_ETH,
            25.0,
            config.backpack.clone(),
        )),
    ];

    tracing::info!(
        "⏳ Booted {} strategies. Waiting for market data...",
        strategies.len()
    );

    // 4. Spawn dedicated data plane thread (decoupled from Tokio)
    let bbo_rx = data_plane::spawn_data_plane_thread(
        "/dev/shm/aleph-matrix",
        2048,
        Some(2), // Pin to CPU core 2
    );

    // 5. Main loop with graceful shutdown
    let sigint = signal::ctrl_c();
    tokio::pin!(sigint);
    
    loop {
        // Async select: receive BBO updates from data plane, idle timeout, or shutdown signal
        tokio::select! {
             _ = &mut sigint => {
                tracing::warn!("🛑 Ctrl+C received — shutting down gracefully...");
                break;
            }
            Ok(update) = bbo_rx.recv_async() => {
                // Process BBO update from data plane thread
                if update.bbo.bid_price > 0.0 && update.bbo.ask_price > 0.0 {
                    for strategy in strategies.iter_mut() {
                        strategy.on_bbo_update(update.symbol_id, update.exchange_id, &update.bbo);
                    }
                }
            }
            _ = tokio::time::sleep(tokio::time::Duration::from_millis(1)) => {
                // Idle timeout - call on_idle() for all strategies
                for strategy in strategies.iter_mut() {
                    strategy.on_idle();
                }
            }
        }
    }

    // 6. Graceful Shutdown: Strategy hooks handle order cancellation
    tracing::info!("♻️ Executing strategy shutdown hooks...");
    for strategy in strategies.iter_mut() {
        strategy.on_shutdown().await;
    }

    tracing::info!("🏁 AlephTX shutdown complete.");
    Ok(())
}
