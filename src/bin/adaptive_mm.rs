//! Adaptive Market Maker Example
//!
//! Production-grade market making with:
//! - Real-time account stats from shared memory
//! - Dynamic position sizing based on balance
//! - Inventory skew for risk management
//! - Adaptive spreads based on volatility

use aleph_tx::account_stats_reader::AccountStatsReader;
use aleph_tx::lighter_trading::LighterTrading;
use aleph_tx::shadow_ledger::ShadowLedgerManager;
use aleph_tx::shm_event_reader::ShmEventReader;
use aleph_tx::shm_reader::ShmReader;
use aleph_tx::strategy::adaptive_mm::AdaptiveMarketMaker;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter("info,aleph_tx=debug")
        .init();

    tracing::info!("AlephTX Adaptive Market Maker");

    // Initialize Shadow Ledger (adaptive_mm still uses V1 ShadowLedger)
    let ledger_manager = ShadowLedgerManager::new();
    let ledger_state = ledger_manager.state();

    // Connect to Event Ring Buffer
    let event_reader = ShmEventReader::new_default()?;
    let _consumer_handle = ledger_manager.spawn_consumer(event_reader);

    // Connect to shared memory
    let shm_reader = ShmReader::open("/dev/shm/aleph-matrix", 2048)?;
    let account_stats_reader = AccountStatsReader::open("/dev/shm/aleph-account-stats")?;

    // Initialize LighterTrading (market_id=0 = ETH perps)
    let trading = Arc::new(LighterTrading::new(0).await?);

    // symbol_id=1002 is ETH, market_id=0 is ETH-USDC
    let mut strategy = AdaptiveMarketMaker::new(
        1002,
        0,
        trading,
        ledger_state,
        shm_reader,
        account_stats_reader,
    );

    // Graceful shutdown
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to listen for Ctrl+C");
        tracing::info!("Ctrl+C received, shutting down...");
        let _ = shutdown_tx.send(true);
    });

    strategy.run(Some(shutdown_rx)).await?;

    Ok(())
}
