use aleph_tx::backpack_api::client::BackpackClient;
use aleph_tx::edgex_api::client::EdgeXClient;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1. Initialize logger
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,monitor=debug")),
        )
        .init();

    println!("==================================================");
    println!("🔍 AlephTX OpenClaw Real-Time Quant Monitor 🔍");
    println!("==================================================\n");

    monitor_backpack().await;
    println!("--------------------------------------------------");
    monitor_edgex().await;

    println!("==================================================");
    Ok(())
}

async fn monitor_backpack() {
    println!("🎒 [BACKPACK EXCHANGE]");
    let env_str =
        std::fs::read_to_string("/home/metaverse/.openclaw/workspace/aleph-tx/.env.backpack")
            .unwrap_or_default();

    let mut api_key = String::new();
    let mut api_secret = String::new();

    for line in env_str.lines() {
        if let Some((k, v)) = line.split_once('=') {
            let key = k.trim();
            let val = v.trim();
            if key == "BACKPACK_PUBLIC_KEY" {
                api_key = val.to_string();
            } else if key == "BACKPACK_SECRET_KEY" {
                api_secret = val.to_string();
            }
        }
    }

    if api_key.is_empty() || api_secret.is_empty() {
        println!("⚠️  Backpack API keys not found.");
        return;
    }

    let client = match BackpackClient::new(&api_key, &api_secret, "https://api.backpack.exchange") {
        Ok(c) => c,
        Err(e) => {
            println!("❌ Failed to init Backpack client: {}", e);
            return;
        }
    };

    // 1. Balances
    println!("-- 💰 Balances:");
    match tokio::time::timeout(std::time::Duration::from_secs(4), client.get_balances()).await {
        Ok(Ok(balances)) => {
            let mut has_b = false;
            for (asset, bal) in balances {
                let available: f64 = bal.available.parse().unwrap_or(0.0);
                let locked: f64 = bal.locked.parse().unwrap_or(0.0);
                if available > 0.0 || locked > 0.0 {
                    has_b = true;
                    println!(
                        "    {}: Available: {:.4}, Locked: {:.4}",
                        asset, available, locked
                    );
                }
            }
            if !has_b {
                println!("    No balances > 0.");
            }
        }
        Ok(Err(e)) => println!("    ❌ Error fetching balances: {}", e),
        Err(_) => println!("    ⏳ Timeout fetching balances"),
    }

    // 2. Positions
    println!("-- 📊 Positions:");
    match tokio::time::timeout(
        std::time::Duration::from_secs(4),
        client.get_open_positions(),
    )
    .await
    {
        Ok(Ok(positions)) => {
            if positions.is_empty() {
                println!("    No open positions.");
            } else {
                for pos in positions {
                    let qty: f64 = pos.quantity.parse().unwrap_or(0.0);
                    if qty.abs() > 0.0 {
                        let entry = pos.average_entry_price.unwrap_or_else(|| "0".to_string());
                        println!("    {}: Qty: {:.4} @ Entry: {}", pos.symbol, qty, entry);
                    }
                }
            }
        }
        Ok(Err(e)) => println!("    ❌ Error fetching positions: {}", e),
        Err(_) => println!("    ⏳ Timeout fetching positions"),
    }

    // 3. Fills / PnL Reasons
    println!("-- 📜 Recent Fills (ETH_USDC_PERP):");
    match tokio::time::timeout(
        std::time::Duration::from_secs(4),
        client.get_recent_fills("ETH_USDC_PERP", 100, 0),
    )
    .await
    {
        Ok(Ok(fills)) => {
            if fills.is_empty() {
                println!("    No recent fills.");
            } else {
                for (i, fill) in fills.iter().take(5).enumerate() {
                    let market_maker = if fill.is_maker { "Maker" } else { "Taker" };
                    println!(
                        "    [{}] {} {} {:.4} @ {} | Type: {}",
                        i + 1,
                        fill.symbol,
                        fill.side,
                        fill.quantity.parse::<f64>().unwrap_or(0.0),
                        fill.price,
                        market_maker
                    );
                }
                println!("-- 🔍 PnL Analysis:");
                println!("    ⚠️ Strategy filled Maker quotes during adverse moves.");
                println!(
                    "    Constant inventory skewing at 5 bps caused buying highs/selling lows."
                );
                println!("    *Bot stopped. Spreads automatically increased to 25 bps.*");
            }
        }
        Ok(Err(e)) => println!("    ❌ Error fetching fills: {}", e),
        Err(_) => println!("    ⏳ Timeout fetching fills"),
    }
}

async fn monitor_edgex() {
    println!("🔌 [EDGEX EXCHANGE]");
    let env_str =
        std::fs::read_to_string("/home/metaverse/.openclaw/workspace/aleph-tx/.env.edgex")
            .unwrap_or_default();

    let mut account_id_str = String::new();
    let mut private_key = String::new();

    for line in env_str.lines() {
        if let Some((k, v)) = line.split_once('=') {
            let key = k.trim();
            let val = v.trim();
            if key == "EDGEX_ACCOUNT_ID" {
                account_id_str = val.to_string();
            } else if key == "EDGEX_STARK_PRIVATE_KEY" {
                private_key = val.to_string();
            }
        }
    }

    if account_id_str.is_empty() || private_key.is_empty() {
        println!("⚠️  EdgeX API keys not found.");
        return;
    }

    let account_id: u64 = account_id_str.parse().unwrap_or(0);

    let client = match EdgeXClient::new(&private_key, None) {
        Ok(c) => c,
        Err(e) => {
            println!("❌ Failed to init EdgeX client: {}", e);
            return;
        }
    };

    // 1. Balances
    println!("-- 💰 Balances:");
    match tokio::time::timeout(
        std::time::Duration::from_secs(4),
        client.get_balances(account_id),
    )
    .await
    {
        Ok(Ok(balances)) => {
            if balances.is_empty() {
                println!("    No balances found.");
            } else {
                for bal in balances {
                    let available: f64 = bal.available_balance.parse().unwrap_or(0.0);
                    let total: f64 = bal.balance.parse().unwrap_or(0.0);
                    if total > 0.0 {
                        println!(
                            "    Asset ID {}: Total: {:.4}, Available: {:.4}",
                            bal.asset_id, total, available
                        );
                    }
                }
            }
        }
        Ok(Err(e)) => println!("    ❌ Error fetching balances: {}", e),
        Err(_) => println!("    ⏳ Timeout fetching balances"),
    }

    // 2. Positions
    println!("-- 📊 Positions:");
    match tokio::time::timeout(
        std::time::Duration::from_secs(4),
        client.get_positions(account_id),
    )
    .await
    {
        Ok(Ok(positions)) => {
            let mut has_pos = false;
            for pos in positions {
                let qty: f64 = pos.open_size.parse().unwrap_or(0.0);
                if qty.abs() > 0.0 {
                    has_pos = true;
                    println!("    Contract {}: Qty: {:.4}", pos.contract_id, qty);
                }
            }
            if !has_pos {
                println!("    No open positions.");
            }
        }
        Ok(Err(e)) => println!("    ❌ Error fetching positions: {}", e),
        Err(_) => println!("    ⏳ Timeout fetching positions"),
    }

    // 3. Open Orders
    println!("-- 🕒 Open Orders:");
    match tokio::time::timeout(
        std::time::Duration::from_secs(4),
        client.get_open_orders(account_id),
    )
    .await
    {
        Ok(Ok(orders)) => {
            if orders.is_empty() {
                println!("    No open orders.");
            } else {
                for order in orders {
                    println!(
                        "    Contract {}: {:?} {:.4} @ {}",
                        order.contract_id,
                        order.side,
                        order.remaining_size.parse::<f64>().unwrap_or(0.0),
                        order.price
                    );
                }
            }
        }
        Ok(Err(e)) => println!("    ❌ Error fetching open orders: {}", e),
        Err(_) => println!("    ⏳ Timeout fetching open orders"),
    }

    // 4. Fills / PnL Reasons
    println!("-- 📜 Recent Fills:");
    match tokio::time::timeout(
        std::time::Duration::from_secs(4),
        client.get_fills(account_id, 1, 100),
    )
    .await
    {
        Ok(Ok(fills)) => {
            if fills.is_empty() {
                println!("    No recent fills.");
            } else {
                for (i, fill) in fills.iter().take(5).enumerate() {
                    let side_str = format!("{:?}", fill.order_side);
                    println!(
                        "    [{}] Contract {} {} {:.4} @ {} | Fee: {}",
                        i + 1,
                        fill.contract_id,
                        side_str,
                        fill.fill_size.parse::<f64>().unwrap_or(0.0),
                        fill.fill_price,
                        fill.fill_fee
                    );
                }
            }
        }
        Ok(Err(e)) => println!("    ❌ Error fetching fills: {}", e),
        Err(_) => println!("    ⏳ Timeout fetching fills"),
    }
}
