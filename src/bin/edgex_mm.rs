// EdgeX Market Maker Example
//
// Demonstrates EdgeXGateway usage with full L2 signature support.

use aleph_tx::config::AppConfig;
use aleph_tx::exchange::Exchange;
use aleph_tx::exchanges::edgex::client::EdgeXClient;
use aleph_tx::exchanges::edgex::gateway::{EdgeXConfig, EdgeXGateway};
use aleph_tx::shm_reader::ShmReader;
use std::sync::Arc;
use tokio::sync::watch;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter("info,aleph_tx=debug")
        .init();

    tracing::info!("🚀 EdgeX Market Maker (Full L2 Signature Support)");
    tracing::info!("==================================================\n");

    // Step 1: Load configuration
    tracing::info!("📋 Loading configuration...");
    let config = AppConfig::load_default();
    let edgex_config = config.edgex;
    tracing::info!(
        "   Risk fraction: {:.1}%",
        edgex_config.risk_fraction * 100.0
    );
    tracing::info!("   Min spread: {} bps", edgex_config.min_spread_bps);

    // Step 2: Load EdgeX credentials from .env.edgex
    tracing::info!("🔑 Loading EdgeX credentials...");
    let env_path = std::env::var("EDGEX_ENV_PATH").unwrap_or_else(|_| ".env.edgex".to_string());

    // Load environment variables
    dotenv::from_filename(&env_path).ok();

    let stark_private_key = std::env::var("EDGEX_STARK_PRIVATE_KEY")
        .map_err(|_| "Missing EDGEX_STARK_PRIVATE_KEY in .env.edgex")?;

    // Step 3: Initialize EdgeX client
    tracing::info!("🎯 Initializing EdgeX client...");
    let client = EdgeXClient::new(&stark_private_key, None)?;
    let client = Arc::new(client);

    // Step 4: Load EdgeX gateway configuration
    tracing::info!("⚙️  Loading EdgeX gateway configuration...");
    let gateway_config = EdgeXConfig::from_env()?;
    tracing::info!("   Account ID: {}", gateway_config.account_id);
    tracing::info!("   Contract ID: {}", gateway_config.contract_id);
    tracing::info!("   Price decimals: {}", gateway_config.price_decimals);
    tracing::info!("   Size decimals: {}", gateway_config.size_decimals);
    tracing::info!("   Fee rate: {:.2}%", gateway_config.fee_rate * 100.0);

    // Step 5: Create EdgeXGateway (Exchange trait implementation)
    tracing::info!("🌉 Creating EdgeX gateway...");
    let gateway = Arc::new(EdgeXGateway::new(client.clone(), gateway_config));
    tracing::info!("✅ EdgeX gateway initialized with L2 signature support");

    // Step 6: Connect to BBO Matrix
    tracing::info!("📡 Connecting to BBO matrix...");
    let mut shm_reader = ShmReader::open("/dev/shm/aleph-matrix", 2048)?;

    // Step 7: Setup graceful shutdown
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        tracing::info!("🛑 Ctrl+C received, initiating graceful shutdown...");
        let _ = shutdown_tx.send(true);
    });

    // Step 8: Simple market making loop
    tracing::info!("🚀 Starting market making loop...\n");

    let symbol_id = 1002; // ETH
    let mut shutdown_rx_clone = shutdown_rx.clone();
    let min_spread_bps = edgex_config.min_spread_bps;
    let order_size = 0.01; // 0.01 ETH

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
                if let Some((_, bbo)) = exchanges.iter().find(|(exch_id, _)| *exch_id == 3)
                    && bbo.bid_price > 0.0 && bbo.ask_price > 0.0 {
                        let mid = (bbo.bid_price + bbo.ask_price) / 2.0;
                        let spread_bps = ((bbo.ask_price - bbo.bid_price) / mid * 10000.0) as u32;

                        tracing::info!(
                            "📊 EdgeX BBO: bid={:.2} ask={:.2} mid={:.2} spread={}bps",
                            bbo.bid_price, bbo.ask_price, mid, spread_bps
                        );

                        // Place orders if spread is sufficient
                        if spread_bps >= min_spread_bps as u32 {
                            let our_bid = bbo.bid_price + 0.01;
                            let our_ask = bbo.ask_price - 0.01;

                            tracing::info!(
                                "📤 Placing orders: bid={:.2} ask={:.2} size={}",
                                our_bid, our_ask, order_size
                            );

                            // Place buy order
                            match gateway.buy(order_size, our_bid).await {
                                Ok(result) => {
                                    tracing::info!("✅ Buy order placed: tx_hash={}", result.tx_hash);
                                }
                                Err(e) => {
                                    tracing::warn!("⚠️  Buy order failed: {}", e);
                                }
                            }

                            // Place sell order
                            match gateway.sell(order_size, our_ask).await {
                                Ok(result) => {
                                    tracing::info!("✅ Sell order placed: tx_hash={}", result.tx_hash);
                                }
                                Err(e) => {
                                    tracing::warn!("⚠️  Sell order failed: {}", e);
                                }
                            }
                        } else {
                            tracing::debug!("⏸️  Spread {}bps < min {}bps, skipping", spread_bps, min_spread_bps);
                        }
                }
            }
        }
    }

    // Step 9: Graceful shutdown
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
