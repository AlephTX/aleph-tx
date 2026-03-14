//! Backpack Market Maker Example
//!
//! Demonstrates using BackpackGateway with the unified Exchange trait.

use aleph_tx::config::AppConfig;
use aleph_tx::exchanges::backpack::client::BackpackClient;
use aleph_tx::exchanges::backpack::gateway::BackpackGateway;
use aleph_tx::shm_reader::ShmReader;
use std::sync::Arc;
use tokio::sync::watch;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter("info,aleph_tx=debug")
        .init();

    tracing::info!("🚀 Backpack Market Maker (Exchange Trait Demo)");
    tracing::info!("==============================================\n");

    // Step 1: Load configuration
    tracing::info!("📋 Loading configuration...");
    let config = AppConfig::load_default();
    let backpack_config = config.backpack;
    tracing::info!(
        "   Risk fraction: {:.1}%",
        backpack_config.risk_fraction * 100.0
    );
    tracing::info!("   Min spread: {} bps", backpack_config.min_spread_bps);

    // Step 2: Load Backpack credentials from .env.backpack
    tracing::info!("🔑 Loading Backpack credentials...");
    let env_path =
        std::env::var("BACKPACK_ENV_PATH").unwrap_or_else(|_| ".env.backpack".to_string());
    let env_content = std::fs::read_to_string(&env_path)?;

    let mut api_key = String::new();
    let mut api_secret = String::new();
    for line in env_content.lines() {
        if let Some(rest) = line.strip_prefix("BACKPACK_PUBLIC_KEY=") {
            api_key = rest.trim().to_string();
        }
        if let Some(rest) = line.strip_prefix("BACKPACK_SECRET_KEY=") {
            api_secret = rest.trim().to_string();
        }
    }

    if api_key.is_empty() || api_secret.is_empty() {
        return Err("Missing BACKPACK_PUBLIC_KEY or BACKPACK_SECRET_KEY in .env.backpack".into());
    }

    // Step 3: Initialize Backpack client
    tracing::info!("🎯 Initializing Backpack client...");
    let client = BackpackClient::new(&api_key, &api_secret, "https://api.backpack.exchange")?;
    let client = Arc::new(client);

    // Step 4: Create BackpackGateway (Exchange trait implementation)
    tracing::info!("🌉 Creating Backpack gateway...");
    let gateway = Arc::new(BackpackGateway::new(
        client.clone(),
        "ETH_USDC_PERP".to_string(),
    ));

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
    let exchange_id = 5; // Backpack
    let mut last_quote_time = std::time::Instant::now();
    let requote_interval = std::time::Duration::from_millis(backpack_config.requote_interval_ms);

    loop {
        // Check shutdown signal
        if *shutdown_rx.borrow() {
            break;
        }

        // Poll BBO updates
        if let Some(updated_symbol) = shm_reader.try_poll()
            && updated_symbol == symbol_id
        {
            let exchanges = shm_reader.read_all_exchanges(symbol_id);
            if let Some((_exch_idx, bbo)) = exchanges.iter().find(|(idx, _)| *idx == exchange_id)
                && bbo.bid_price > 0.0
                && bbo.ask_price > 0.0
            {
                let mid = (bbo.bid_price + bbo.ask_price) / 2.0;
                tracing::debug!(
                    "BBO Update: bid={:.2} ask={:.2} mid={:.2}",
                    bbo.bid_price,
                    bbo.ask_price,
                    mid
                );

                // Simple quoting logic (demo only - not production ready)
                if last_quote_time.elapsed() >= requote_interval {
                    let spread_bps = backpack_config.min_spread_bps;
                    let half_spread = mid * spread_bps / 10000.0;

                    let our_bid = mid - half_spread;
                    let our_ask = mid + half_spread;
                    let size = 0.01; // Small demo size

                    tracing::info!(
                        "💡 Quote: bid={:.2} ask={:.2} size={}",
                        our_bid,
                        our_ask,
                        size
                    );

                    last_quote_time = std::time::Instant::now();
                }
            }
        }

        // Yield to avoid busy loop
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    // Step 8: Graceful shutdown
    tracing::info!("♻️ Cancelling all orders...");
    use aleph_tx::exchange::Exchange;
    match gateway.cancel_all().await {
        Ok(_) => tracing::info!("✅ All orders cancelled"),
        Err(e) => tracing::warn!("⚠️ Cancel failed: {}", e),
    }

    tracing::info!("✅ Backpack MM shutdown complete");
    Ok(())
}
