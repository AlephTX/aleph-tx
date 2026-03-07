// EdgeX Market Maker Example
//
// Demonstrates EdgeXGateway usage with simple market making loop.
// NOTE: EdgeX gateway currently has stub implementation - full L2 signature integration pending.

use aleph_tx::config::AppConfig;
use aleph_tx::exchange::Exchange;
use aleph_tx::exchanges::edgex::client::EdgeXClient;
use aleph_tx::exchanges::edgex::gateway::EdgeXGateway;
use aleph_tx::shm_reader::ShmReader;
use std::sync::Arc;
use tokio::sync::watch;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter("info,aleph_tx=debug")
        .init();

    tracing::info!("🚀 EdgeX Market Maker (Exchange Trait Demo)");
    tracing::info!("==============================================\n");

    // Step 1: Load configuration
    tracing::info!("📋 Loading configuration...");
    let config = AppConfig::load_default();
    let edgex_config = config.edgex;
    tracing::info!("   Risk fraction: {:.1}%", edgex_config.risk_fraction * 100.0);
    tracing::info!("   Min spread: {} bps", edgex_config.min_spread_bps);

    // Step 2: Load EdgeX credentials from .env.edgex
    tracing::info!("🔑 Loading EdgeX credentials...");
    let env_path = std::env::var("EDGEX_ENV_PATH")
        .unwrap_or_else(|_| ".env.edgex".to_string());
    let env_content = std::fs::read_to_string(&env_path)?;

    let mut account_id = 0u64;
    let mut stark_private_key = String::new();
    for line in env_content.lines() {
        if let Some(rest) = line.strip_prefix("EDGEX_ACCOUNT_ID=") {
            account_id = rest.trim().parse()?;
        }
        if let Some(rest) = line.strip_prefix("EDGEX_STARK_PRIVATE_KEY=") {
            stark_private_key = rest.trim().to_string();
        }
    }

    if account_id == 0 || stark_private_key.is_empty() {
        return Err("Missing EDGEX_ACCOUNT_ID or EDGEX_STARK_PRIVATE_KEY in .env.edgex".into());
    }

    // Step 3: Initialize EdgeX client
    tracing::info!("🎯 Initializing EdgeX client...");
    let client = EdgeXClient::new(&stark_private_key, None)?;
    let client = Arc::new(client);

    // Step 4: Create EdgeXGateway (Exchange trait implementation)
    tracing::info!("🌉 Creating EdgeX gateway...");
    let contract_id = 1; // ETH-USDC perp
    let gateway = Arc::new(EdgeXGateway::new(
        client.clone(),
        account_id,
        contract_id,
    ));
    tracing::info!("⚠️  EdgeX gateway initialized (stub - L2 signature pending)");

    // Step 5: Connect to BBO Matrix
    tracing::info!("📡 Connecting to BBO matrix...");
    let mut shm_reader = ShmReader::open("/dev/shm/aleph-matrix", 2048)?;

    // Step 6: Setup graceful shutdown
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        tracing::info!("🛑 Ctrl+C received, initiating graceful shutdown...");
        let _ = shutdown_tx.send(true);
    });

    // Step 7: Simple market making loop (demo only)
    tracing::info!("🚀 Starting market making loop...\n");

    let symbol_id = 1002; // ETH
    let mut shutdown_rx_clone = shutdown_rx.clone();

    loop {
        tokio::select! {
            _ = shutdown_rx_clone.changed() => {
                if *shutdown_rx_clone.borrow() {
                    break;
                }
            }
            _ = tokio::time::sleep(tokio::time::Duration::from_secs(5)) => {
                // Read BBO from shared memory
                let exchanges = shm_reader.read_all_exchanges(symbol_id);

                // Find EdgeX exchange (exchange_id = 3)
                if let Some((_, bbo)) = exchanges.iter().find(|(exch_id, _)| *exch_id == 3) {
                    if bbo.bid_price > 0.0 && bbo.ask_price > 0.0 {
                        let mid = (bbo.bid_price + bbo.ask_price) / 2.0;
                        let spread_bps = ((bbo.ask_price - bbo.bid_price) / mid * 10000.0) as u32;

                        tracing::info!(
                            "📊 EdgeX BBO: bid={:.2} ask={:.2} mid={:.2} spread={}bps",
                            bbo.bid_price, bbo.ask_price, mid, spread_bps
                        );

                        // Demo: Would place orders here if L2 signature was implemented
                        tracing::debug!("⚠️  Order placement skipped (L2 signature not implemented)");
                    }
                }
            }
        }
    }

    // Step 8: Graceful shutdown
    tracing::info!("\n🛑 Shutting down...");
    tracing::info!("📤 Canceling all orders...");
    if let Err(e) = gateway.cancel_all().await {
        tracing::warn!("⚠️  Failed to cancel orders: {}", e);
    }

    tracing::info!("📤 Closing all positions...");
    if let Err(e) = gateway.close_all_positions(0.0).await {
        tracing::warn!("⚠️  Failed to close positions: {}", e);
    }

    tracing::info!("✅ EdgeX MM stopped");
    Ok(())
}
