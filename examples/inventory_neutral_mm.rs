//! Inventory-Neutral Market Maker Example
//!
//! Production-ready HFT strategy with asymmetric order sizing for inventory control.

use aleph_tx::account_stats_reader::AccountStatsReader;
use aleph_tx::config::AppConfig;
use aleph_tx::lighter_trading::LighterTrading;
use aleph_tx::shadow_ledger::ShadowLedgerManager;
use aleph_tx::shm_event_reader::ShmEventReader;
use aleph_tx::shm_reader::ShmReader;
use aleph_tx::strategy::inventory_neutral_mm::InventoryNeutralMM;
use std::sync::Arc;
use tokio::sync::watch;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter("info,aleph_tx=debug")
        .init();

    tracing::info!("🚀 Inventory-Neutral Market Maker");
    tracing::info!("==================================\n");

    // Step 1: Load configuration
    tracing::info!("📋 Loading configuration...");
    let config = AppConfig::load_default();
    let strategy_config = config
        .inventory_neutral_mm
        .ok_or("inventory_neutral_mm config not found in config.toml")?;
    tracing::info!("   Exchange ID: {}", strategy_config.exchange_id);
    tracing::info!("   Symbol ID: {}", strategy_config.symbol_id);
    tracing::info!("   Market ID: {}", strategy_config.market_id);
    tracing::info!("   Base order size: {}", strategy_config.base_order_size);
    tracing::info!("   Max position: {}", strategy_config.max_position);

    // Step 2: Initialize Shadow Ledger
    tracing::info!("📊 Initializing Shadow Ledger...");
    let ledger_manager = ShadowLedgerManager::new();
    let ledger_state = ledger_manager.state();

    // Step 3: Connect to Event Ring Buffer
    tracing::info!("🔗 Connecting to event ring buffer...");
    let event_reader = ShmEventReader::new_default()?;
    tracing::info!("   Write index: {}", event_reader.write_idx());
    tracing::info!("   Read index:  {}", event_reader.local_read_idx());

    // Step 4: Spawn Event Consumer
    tracing::info!("🔄 Spawning event consumer task...");
    let _consumer_handle = ledger_manager.spawn_consumer(event_reader);

    // Step 5: Connect to BBO Matrix
    tracing::info!("📡 Connecting to BBO matrix...");
    let shm_reader = ShmReader::open("/dev/shm/aleph-matrix", 2048)?;

    // Step 6: Connect to Account Stats
    tracing::info!("📈 Connecting to account stats...");
    let account_stats_reader = AccountStatsReader::open("/dev/shm/aleph-account-stats")?;

    // Step 7: Initialize Lighter Trading API
    tracing::info!("🎯 Initializing Lighter Trading API...");
    let trading = Arc::new(LighterTrading::new(strategy_config.market_id).await?);

    // Step 8: Initialize Strategy
    tracing::info!("🎯 Initializing Inventory-Neutral MM...");
    let mut strategy = InventoryNeutralMM::new(
        strategy_config,
        trading,
        ledger_state,
        shm_reader,
        account_stats_reader,
    );

    // Step 9: Setup graceful shutdown
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        tracing::info!("🛑 Ctrl+C received, initiating graceful shutdown...");
        let _ = shutdown_tx.send(true);
    });

    // Step 10: Run strategy
    tracing::info!("🚀 Starting strategy main loop...\n");
    strategy.run(Some(shutdown_rx)).await?;

    tracing::info!("✅ Strategy shutdown complete");
    Ok(())
}
