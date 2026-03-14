//! Inventory-Neutral Market Maker Example
//!
//! Production-ready HFT strategy with asymmetric order sizing for inventory control.
//! v5.0.0: Uses OrderTracker (per-order state machine) instead of ShadowLedger.

use aleph_tx::account_stats_reader::AccountStatsReader;
use aleph_tx::config::AppConfig;
use aleph_tx::lighter_trading::LighterTrading;
use aleph_tx::order_tracker::OrderTracker;
use aleph_tx::shm_reader::ShmReader;
use aleph_tx::strategy::inventory_neutral_mm::InventoryNeutralMM;
use std::sync::Arc;
use tokio::sync::watch;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter("info,aleph_tx=debug")
        .init();

    tracing::info!("🚀 Inventory-Neutral Market Maker (v5.0.0)");

    // Load configuration
    let config = AppConfig::load_default();
    let strategy_config = config
        .inventory_neutral_mm
        .ok_or("inventory_neutral_mm config not found in config.toml")?;
    tracing::info!(
        "Exchange ID: {}, Symbol ID: {}, Market ID: {}",
        strategy_config.exchange_id,
        strategy_config.symbol_id,
        strategy_config.market_id
    );

    // Initialize OrderTracker (v5.0.0 per-order state machine)
    let order_tracker = Arc::new(OrderTracker::new());

    // Connect to SHM
    let shm_reader = ShmReader::open("/dev/shm/aleph-matrix", 2048)?;
    let account_stats_reader = AccountStatsReader::open("/dev/shm/aleph-account-stats")?;

    // Initialize Lighter Trading API
    let mut trading = LighterTrading::new(strategy_config.market_id).await?;
    trading.set_order_tracker(Arc::clone(&order_tracker));
    let trading = Arc::new(trading);

    // Initialize Strategy
    let mut strategy = InventoryNeutralMM::new(
        strategy_config,
        trading,
        order_tracker,
        shm_reader,
        account_stats_reader,
    );

    // Graceful shutdown
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        tracing::info!("🛑 Ctrl+C received, shutting down...");
        let _ = shutdown_tx.send(true);
    });

    strategy.run(Some(shutdown_rx)).await?;

    tracing::info!("✅ Strategy shutdown complete");
    Ok(())
}
