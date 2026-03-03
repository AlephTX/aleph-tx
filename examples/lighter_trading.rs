//! Lighter Trading Example
//!
//! Demonstrates the complete Dual-Track IPC Architecture:
//! 1. Go Feeder: Public BBO Matrix + Private Event RingBuffer
//! 2. Rust Core: Shadow Ledger + HTTP Order Execution
//! 3. Optimistic Accounting: in_flight_pos updated before API responds

use aleph_tx::shadow_ledger::ShadowLedgerManager;
use aleph_tx::shm_event_reader::ShmEventReader;
use aleph_tx::shm_reader::ShmReader;
use aleph_tx::strategy::lighter_mm::LighterMarketMaker;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter("info,aleph_tx=debug")
        .init();

    tracing::info!("🚀 AlephTX Lighter Trading System");
    tracing::info!("==================================\n");

    // Step 1: Initialize Shadow Ledger
    tracing::info!("📊 Initializing Shadow Ledger...");
    let ledger_manager = ShadowLedgerManager::new();
    let ledger_state = ledger_manager.state();

    // Step 2: Connect to Event Ring Buffer
    tracing::info!("🔗 Connecting to event ring buffer (/dev/shm/aleph-events)...");
    let event_reader = ShmEventReader::new_default()?;
    tracing::info!("   Write index: {}", event_reader.write_idx());
    tracing::info!("   Read index:  {}", event_reader.local_read_idx());

    // Step 3: Spawn Event Consumer (background reconciliation)
    tracing::info!("🔄 Spawning event consumer task...");
    let _consumer_handle = ledger_manager.spawn_consumer(event_reader);

    // Step 4: Connect to BBO Matrix
    tracing::info!("📡 Connecting to BBO matrix (/dev/shm/aleph-matrix)...");
    let shm_reader = ShmReader::open("/dev/shm/aleph-matrix", 2048)?;

    // Step 5: Initialize Strategy
    tracing::info!("🎯 Initializing Lighter Market Maker...");
    let api_key = std::env::var("LIGHTER_API_KEY")
        .expect("LIGHTER_API_KEY not set");
    let private_key = std::env::var("LIGHTER_PRIVATE_KEY")
        .expect("LIGHTER_PRIVATE_KEY not set");

    let mut strategy = LighterMarketMaker::new(
        0,  // BTC symbol_id
        0,  // BTC market_id on Lighter
        api_key,
        private_key,
        ledger_state,
        shm_reader,
    )?;

    tracing::info!("\n✅ System initialized successfully");
    tracing::info!("🏁 Starting trading loop...\n");

    // Step 6: Run Strategy with graceful shutdown
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    // Spawn shutdown handler
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.expect("Failed to listen for Ctrl+C");
        tracing::info!("Ctrl+C received, initiating graceful shutdown...");
        let _ = shutdown_tx.send(true);
    });

    strategy.run(Some(shutdown_rx)).await?;

    Ok(())
}
