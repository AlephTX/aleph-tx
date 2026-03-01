use aleph_tx::edgex_api::client::EdgeXClient;

#[tokio::main]
async fn main() {
    let env_str = std::fs::read_to_string(
        "/home/metaverse/.openclaw/workspace/aleph-tx/.env.edgex"
    ).unwrap();

    let mut key = String::new();
    let mut account_id = 0u64;

    for line in env_str.lines() {
        if let Some(rest) = line.strip_prefix("EDGEX_STARK_PRIVATE_KEY=") {
            key = rest.trim().to_string();
        }
        if let Some(rest) = line.strip_prefix("EDGEX_ACCOUNT_ID=") {
            account_id = rest.trim().parse().unwrap_or(0);
        }
    }

    let client = EdgeXClient::new(&key, None).unwrap();

    println!("=== EdgeX Account {} ===\n", account_id);

    // Get balances
    match client.get_balances(account_id).await {
        Ok(balances) => {
            println!("Balances:");
            let mut total_usd = 0.0;
            for b in &balances {
                let bal: f64 = b.balance.parse().unwrap_or(0.0);
                if bal > 0.01 {
                    println!("  Asset: {} | Balance: ${:.2}", b.asset_id, bal);
                    total_usd += bal;
                }
            }
            println!("\n  Total: ${:.2}", total_usd);
        }
        Err(e) => println!("Balance error: {:?}", e),
    }

    // Get positions
    match client.get_positions(account_id).await {
        Ok(positions) => {
            println!("\nPositions:");
            if positions.is_empty() {
                println!("  No open positions");
            }
            for p in &positions {
                println!("  Contract: {} | Size: {}",
                    p.contract_id, p.open_size);
            }
        }
        Err(e) => println!("Position error: {:?}", e),
    }

    // Get open orders
    match client.get_open_orders(account_id).await {
        Ok(orders) => {
            println!("\nOpen Orders:");
            if orders.is_empty() {
                println!("  No open orders");
            }
            for o in orders.iter().take(5) {
                println!("  Order {} | {:?} {} @ {}",
                    o.order_id, o.side, o.size, o.price);
            }
        }
        Err(e) => println!("Orders error: {:?}", e),
    }
}
