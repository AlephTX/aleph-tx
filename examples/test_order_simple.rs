//! Simple EdgeX Order Test
//!
//! Tests basic order placement via EdgeXGateway.

use aleph_tx::exchange::Exchange;
use aleph_tx::exchanges::edgex::{
    client::EdgeXClient,
    gateway::{EdgeXConfig, EdgeXGateway},
};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .init();

    println!("Creating EdgeX client...");
    let private_key = std::env::var("EDGEX_L2_PRIVATE_KEY")
        .expect("EDGEX_L2_PRIVATE_KEY not set");
    let client = Arc::new(EdgeXClient::new(&private_key, None)?);
    let config = EdgeXConfig::from_env()?;
    let gateway = Arc::new(EdgeXGateway::new(client, config));

    println!("Placing buy order...");
    let result = gateway.buy(0.01, 1500.0).await;

    match result {
        Ok(order) => {
            println!("Order placed: tx_hash={}", order.tx_hash);
        }
        Err(e) => {
            println!("Order failed: {}", e);
        }
    }

    Ok(())
}
