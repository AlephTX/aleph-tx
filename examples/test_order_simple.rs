use aleph_tx::exchanges::edgex::{
    client::EdgeXClient,
    gateway::EdgeXGateway,
};
use std::sync::Arc;
use tokio;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .init();

    println!("Creating EdgeX client...");
    let client = EdgeXClient::new()?;
    let gateway = Arc::new(EdgeXGateway::new(client));

    println!("Placing order...");
    let result = gateway.place_order(
        "buy",
        1500.0,
        0.01,
    ).await;

    match result {
        Ok(order_id) => {
            println!("✅ Order placed successfully!");
            println!("Order ID: {}", order_id);
        }
        Err(e) => {
            println!("❌ Order failed: {}", e);
        }
    }

    Ok(())
}
