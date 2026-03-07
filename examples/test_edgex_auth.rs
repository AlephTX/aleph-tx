use aleph_tx::edgex_api::client::EdgeXClient;
use std::fs;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    // Load credentials from .env.edgex
    let ex_env = fs::read_to_string(".env.edgex").unwrap_or_default();
    let mut ex_key = String::new();
    let mut ex_account: u64 = 0;

    for line in ex_env.lines() {
        if let Some(rest) = line.strip_prefix("EDGEX_ACCOUNT_ID=") {
            ex_account = rest.trim().parse().unwrap_or(0);
        }
        if let Some(rest) = line.strip_prefix("EDGEX_STARK_PRIVATE_KEY=") {
            ex_key = rest.trim().to_string();
        }
    }

    println!("Account ID: {}", ex_account);
    println!("Private Key: {}...", &ex_key[..16]);

    // Create client
    let client = EdgeXClient::new(&ex_key, None)?;

    // Try to get positions
    println!("\nTesting GET /api/v1/private/position/getPositionByAccountId");
    match client.get_positions(ex_account).await {
        Ok(positions) => {
            println!("✅ Authentication successful!");
            println!("Positions: {:?}", positions);
        }
        Err(e) => {
            println!("❌ Authentication failed: {}", e);
        }
    }

    Ok(())
}
