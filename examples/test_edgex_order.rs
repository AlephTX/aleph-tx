use aleph_tx::exchanges::edgex::client::EdgeXClient;
use aleph_tx::exchanges::edgex::gateway::{EdgeXGateway, EdgeXConfig};
use aleph_tx::exchange::Exchange;
use std::sync::Arc;
use std::env;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .init();

    println!("=== EdgeX Order Placement Test ===\n");

    // Load environment variables from .env.edgex
    dotenv::from_filename(".env.edgex").ok();

    let stark_private_key = env::var("EDGEX_STARK_PRIVATE_KEY")?;

    // Create EdgeX client
    let client = EdgeXClient::new(&stark_private_key, None)?;
    let client = Arc::new(client);

    // Load gateway config from environment
    let config = EdgeXConfig::from_env()?;

    println!("Config:");
    println!("  Account ID: {}", config.account_id);
    println!("  Contract ID: {}", config.contract_id);
    println!("  Fee rate: {}%\n", config.fee_rate * 100.0);

    // Create gateway
    let gateway = EdgeXGateway::new(client, config);

    println!("Testing order placement...\n");

    // Place a buy order
    let result = gateway.buy(
        0.01,  // size: 0.01 ETH
        1500.0, // price: $1500
    ).await;

    match result {
        Ok(order_result) => {
            println!("✅ Order placed successfully!");
            println!("   TX Hash: {}", order_result.tx_hash);
        }
        Err(e) => {
            println!("❌ Order placement failed:");
            println!("   Error: {}", e);
        }
    }

    Ok(())
}
