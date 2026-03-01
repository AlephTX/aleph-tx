use aleph_tx::backpack_api::client::BackpackClient;
use aleph_tx::edgex_api::client::EdgeXClient;
use chrono::NaiveDateTime;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("==================================================");
    println!("📈 AlephTX Historical PnL Analyzer 📉");
    println!("==================================================\n");

    analyze_backpack().await;
    println!("--------------------------------------------------");
    analyze_edgex().await;

    println!("==================================================");
    Ok(())
}

async fn analyze_backpack() {
    println!("🎒 [BACKPACK EXCHANGE]");
    let env_str =
        std::fs::read_to_string("/home/metaverse/.openclaw/workspace/aleph-tx/.env.backpack")
            .unwrap_or_default();
    let mut api_key = String::new();
    let mut api_secret = String::new();

    for line in env_str.lines() {
        if let Some((k, v)) = line.split_once('=') {
            if k.trim() == "BACKPACK_PUBLIC_KEY" {
                api_key = v.trim().to_string();
            }
            if k.trim() == "BACKPACK_SECRET_KEY" {
                api_secret = v.trim().to_string();
            }
        }
    }

    if api_key.is_empty() || api_secret.is_empty() {
        return;
    }
    let client = match BackpackClient::new(&api_key, &api_secret, "https://api.backpack.exchange") {
        Ok(c) => c,
        Err(_) => return,
    };

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let cutoff = now_ms.saturating_sub(24 * 60 * 60 * 1000);

    let mut all_fills = Vec::new();
    let mut offset = 0;
    loop {
        let page_fills = match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.get_recent_fills("ETH_USDC_PERP", 100, offset),
        )
        .await
        {
            Ok(Ok(f)) => f,
            _ => {
                println!(
                    "    ❌ Timeout or error getting Backpack fills. (offset={})",
                    offset
                );
                break;
            }
        };

        if page_fills.is_empty() {
            break;
        }
        let len = page_fills.len() as u32;
        eprintln!(
            "    [DEBUG] Backpack page offset={} got {} fills",
            offset, len
        );
        let mut stop = false;

        for fill in page_fills {
            let mut ts: u64 = 0;
            if let Some(t) = &fill.timestamp {
                if let Some(s) = t.as_str() {
                    // Try epoch ms first
                    if let Ok(n) = s.parse::<u64>() {
                        ts = n;
                    } else {
                        // ISO 8601: "2026-03-01T04:24:18.646"
                        if let Ok(dt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f") {
                            ts = dt.and_utc().timestamp_millis() as u64;
                        } else if let Ok(dt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S")
                        {
                            ts = dt.and_utc().timestamp_millis() as u64;
                        }
                    }
                } else if let Some(n) = t.as_u64() {
                    ts = n;
                } else if let Some(n) = t.as_i64() {
                    ts = n as u64;
                }
            }

            if ts > 0 && ts < cutoff {
                stop = true;
                continue;
            }
            all_fills.push(fill);
        }

        offset += len;
        if stop || len < 100 || all_fills.len() > 50000 {
            break;
        }
    }

    println!("    Fetched {} recent fills (Last 24h).", all_fills.len());

    // Sort fills by timestamp (oldest first)
    all_fills.sort_by_key(|f| {
        if let Some(t) = &f.timestamp {
            if let Some(s) = t.as_str() {
                if let Ok(n) = s.parse::<u64>() {
                    return n;
                }
                if let Ok(dt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f") {
                    return dt.and_utc().timestamp_millis() as u64;
                }
                if let Ok(dt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
                    return dt.and_utc().timestamp_millis() as u64;
                }
            }
            if let Some(n) = t.as_u64() {
                return n;
            }
        }
        0
    });
    let fills = all_fills;

    let mut position = 0.0;
    let mut realized_pnl = 0.0;
    let mut fees_paid = 0.0;
    let mut volume = 0.0;
    let mut trades = 0;
    let mut maker_trades = 0;
    let mut taker_trades = 0;

    let mut entry_price = 0.0;

    for fill in fills {
        let price: f64 = fill.price.parse().unwrap_or(0.0);
        let qty: f64 = fill.quantity.parse().unwrap_or(0.0);
        let fee: f64 = fill.fee.parse().unwrap_or(0.0);

        let direction =
            if fill.side.eq_ignore_ascii_case("Bid") || fill.side.eq_ignore_ascii_case("Buy") {
                1.0
            } else {
                -1.0
            };
        fees_paid += fee;

        if fill.is_maker {
            maker_trades += 1;
        } else {
            taker_trades += 1;
        }

        if position == 0.0 {
            entry_price = price;
            position += qty * direction;
        } else if position * direction > 0.0 {
            let new_pos = position + qty * direction;
            entry_price = (entry_price * position.abs() + price * qty) / new_pos.abs();
            position = new_pos;
        } else {
            let reduce_qty = f64::min(qty, position.abs());
            let pnl = (price - entry_price) * reduce_qty * position.signum();
            realized_pnl += pnl;
            position += qty * direction;

            if position.signum() != (position - qty * direction).signum() && position != 0.0 {
                entry_price = price;
            }
        }
        volume += qty * price;
        trades += 1;
    }

    println!(
        "    Total Trades: {} (Maker: {}, Taker: {})",
        trades, maker_trades, taker_trades
    );
    println!("    Total Volume: ${:.2}", volume);
    println!("    Fees Paid:    ${:.4}", fees_paid);
    println!("    Realized PnL: ${:.4} (Gross)", realized_pnl);
    println!("    Net PnL:      ${:.4}", realized_pnl - fees_paid);
    if position.abs() > 0.0001 {
        println!(
            "    Open Position: {:.4} ETH @ {:.2}",
            position, entry_price
        );
    }
}

async fn analyze_edgex() {
    println!("🔌 [EDGEX EXCHANGE]");
    let env_str =
        std::fs::read_to_string("/home/metaverse/.openclaw/workspace/aleph-tx/.env.edgex")
            .unwrap_or_default();
    let mut account_id_str = String::new();
    let mut private_key = String::new();

    for line in env_str.lines() {
        if let Some((k, v)) = line.split_once('=') {
            if k.trim() == "EDGEX_ACCOUNT_ID" {
                account_id_str = v.trim().to_string();
            }
            if k.trim() == "EDGEX_STARK_PRIVATE_KEY" {
                private_key = v.trim().to_string();
            }
        }
    }

    if account_id_str.is_empty() {
        return;
    }
    let account_id: u64 = account_id_str.parse().unwrap_or(0);
    let client = match EdgeXClient::new(&private_key, None) {
        Ok(c) => c,
        Err(_) => return,
    };

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let cutoff = now_ms.saturating_sub(24 * 60 * 60 * 1000);

    let mut all_fills = Vec::new();
    let mut page = 1;

    loop {
        let page_fills = match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.get_fills(account_id, page, 100),
        )
        .await
        {
            Ok(Ok(f)) => f,
            _ => {
                println!(
                    "    ❌ Timeout or error getting EdgeX fills. (page={})",
                    page
                );
                break;
            }
        };

        if page_fills.is_empty() {
            break;
        }
        let len = page_fills.len();
        let mut stop = false;

        for fill in page_fills {
            let ts: u64 = fill.match_time.parse().unwrap_or(0);
            if ts > 0 && ts < cutoff {
                stop = true;
                continue;
            }
            if ts == 0 || ts >= cutoff {
                all_fills.push(fill);
            }
        }

        page += 1;
        if stop || len < 100 || all_fills.len() > 50000 || page > 30 {
            break;
        }
    }

    println!("    Fetched {} recent fills (Last 24h).", all_fills.len());
    let mut fills = all_fills;
    // Exactly sort chronologically
    fills.sort_by_key(|f| f.match_time.parse::<u64>().unwrap_or(0));

    let mut position = 0.0;
    let mut realized_pnl = 0.0;
    let mut fees_paid = 0.0;
    let mut volume = 0.0;
    let mut trades = 0;
    let mut entry_price = 0.0;

    for fill in fills {
        let price: f64 = fill.fill_price.parse().unwrap_or(0.0);
        let qty: f64 = fill.fill_size.parse().unwrap_or(0.0);
        let fee: f64 = fill.fill_fee.parse().unwrap_or(0.0);

        let direction = if format!("{:?}", fill.order_side).eq_ignore_ascii_case("Buy") {
            1.0
        } else {
            -1.0
        };
        fees_paid += fee;

        if position == 0.0 {
            entry_price = price;
            position += qty * direction;
        } else if position * direction > 0.0 {
            let new_pos = position + qty * direction;
            entry_price = (entry_price * position.abs() + price * qty) / new_pos.abs();
            position = new_pos;
        } else {
            let reduce_qty = f64::min(qty, position.abs());
            let pnl = (price - entry_price) * reduce_qty * position.signum();
            realized_pnl += pnl;
            position += qty * direction;

            if position.signum() != (position - qty * direction).signum() && position != 0.0 {
                entry_price = price;
            }
        }
        volume += qty * price;
        trades += 1;
    }

    println!("    Total Trades: {}", trades);
    println!("    Total Volume: ${:.2}", volume);
    println!("    Fees Paid:    ${:.4}", fees_paid);
    println!("    Realized PnL: ${:.4} (Gross)", realized_pnl);
    println!("    Net PnL:      ${:.4}", realized_pnl - fees_paid);
    if position.abs() > 0.0001 {
        println!(
            "    Open Position: {:.4} ETH @ {:.2}",
            position, entry_price
        );
    }
}
