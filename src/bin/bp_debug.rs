use aleph_tx::backpack_api::client::BackpackClient;

#[tokio::main]
async fn main() {
    let env_str = std::fs::read_to_string(
        "/home/metaverse/.openclaw/workspace/aleph-tx/.env.backpack",
    )
    .unwrap();
    let mut api_key = String::new();
    let mut api_secret = String::new();
    for line in env_str.lines() {
        if let Some(rest) = line.strip_prefix("BACKPACK_PUBLIC_KEY=") {
            api_key = rest.trim().to_string();
        }
        if let Some(rest) = line.strip_prefix("BACKPACK_SECRET_KEY=") {
            api_secret = rest.trim().to_string();
        }
    }

    let client =
        BackpackClient::new(&api_key, &api_secret, "https://api.backpack.exchange").unwrap();

    // Test get_balances for the complete picture
    eprintln!("=== Spot Balances (ALL) ===");
    match client.get_balances().await {
        Ok(balances) => {
            eprintln!("Total assets: {}", balances.len());
            for (k, v) in &balances {
                let _avail: f64 = v.available.parse().unwrap_or(0.0);
                let _locked: f64 = v.locked.parse().unwrap_or(0.0);
                eprintln!("  {} => available={}, locked={}", k, v.available, v.locked);
            }
        }
        Err(e) => eprintln!("Balances error: {:?}", e),
    }

    // Test positions
    eprintln!("\n=== Open Positions ===");
    match client.get_open_positions().await {
        Ok(positions) => {
            for pos in &positions {
                eprintln!("  {:?}", pos);
            }
            if positions.is_empty() {
                eprintln!("  No open positions");
            }
        }
        Err(e) => eprintln!("Positions error: {:?}", e),
    }

    // Test total equity calculation
    eprintln!("\n=== Total Equity Calculation ===");
    match client.get_total_equity().await {
        Ok(equity) => eprintln!("  Total Equity: ${:.2}", equity),
        Err(e) => eprintln!("  Equity calculation error: {:?}", e),
    }
}
