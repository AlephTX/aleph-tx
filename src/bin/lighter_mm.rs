//! Lighter Market Maker - Main Entry Point
//!
//! Production-grade HFT market maker for Lighter DEX
//! - FFI-based order execution (<10μs latency)
//! - Shadow ledger for instant position queries
//! - Optimistic accounting with WebSocket reconciliation

use aleph_tx::error::Result;
use aleph_tx::shadow_ledger::ShadowLedger;
use aleph_tx::shm_reader::ShmReader;
use aleph_tx::strategy::lighter_mm::LighterMarketMaker;
use parking_lot::RwLock;
use std::env;
use std::sync::Arc;
use tokio::signal;
use tracing_subscriber::{fmt, EnvFilter};

#[tokio::main]
async fn main() -> Result<()> {
    // 1. Initialize logger
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,aleph_tx=debug,lighter_mm=debug"));
    fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_thread_ids(false)
        .with_level(true)
        .init();

    tracing::info!("🚀 Lighter Market Maker v3.2.0 - Tier-1 HFT Edition");
    tracing::info!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");

    // 2. Load credentials from environment
    let private_key = env::var("API_KEY_PRIVATE_KEY")
        .expect("API_KEY_PRIVATE_KEY not set");
    let account_index: i64 = env::var("LIGHTER_ACCOUNT_INDEX")
        .expect("LIGHTER_ACCOUNT_INDEX not set")
        .parse()
        .expect("Invalid LIGHTER_ACCOUNT_INDEX");
    let api_key_index: u8 = env::var("LIGHTER_API_KEY_INDEX")
        .expect("LIGHTER_API_KEY_INDEX not set")
        .parse()
        .expect("Invalid LIGHTER_API_KEY_INDEX");

    tracing::info!("🔑 Account: {} | API Key: {}", account_index, api_key_index);

    // 3. Open shared memory for market data
    let shm_path = "/dev/shm/aleph-matrix";
    let shm_reader = match ShmReader::open(shm_path, 2048) {
        Ok(r) => {
            tracing::info!("📡 Connected to {}", shm_path);
            r
        }
        Err(e) => {
            tracing::error!("❌ Failed to open shared memory: {}", e);
            tracing::error!("   Make sure Go feeder is running!");
            std::process::exit(1);
        }
    };

    // 4. Initialize shadow ledger
    let ledger = Arc::new(RwLock::new(ShadowLedger::default()));
    tracing::info!("📒 Shadow ledger initialized");

    // 5. Create market maker strategy
    // WETH-USDC on Lighter: market_id=0, symbol_id=1002 (SymbolETHPERP)
    let symbol_id = 1002;
    let market_id = 0;

    let mut strategy = LighterMarketMaker::new(
        symbol_id,
        market_id,
        private_key,
        account_index,
        api_key_index,
        Arc::clone(&ledger),
        shm_reader,
    )?;

    tracing::info!("✅ Strategy initialized: WETH-USDC (market_id={})", market_id);
    tracing::info!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");

    // 6. Setup graceful shutdown
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    tokio::spawn(async move {
        signal::ctrl_c().await.expect("Failed to listen for Ctrl+C");
        tracing::warn!("🛑 Shutdown signal received, canceling orders...");
        let _ = shutdown_tx.send(true);
    });

    // 7. Run strategy
    tracing::info!("🎯 Starting market making...");
    strategy.run(Some(shutdown_rx)).await?;

    tracing::info!("👋 Shutdown complete");
    Ok(())
}
