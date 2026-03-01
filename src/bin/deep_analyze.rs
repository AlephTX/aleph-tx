use aleph_tx::backpack_api::client::BackpackClient;
use aleph_tx::edgex_api::client::EdgeXClient;
use chrono::NaiveDateTime;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("==============================================================");
    println!("📊 AlephTX Deep Per-Trade PnL Analyzer (24h)");
    println!("==============================================================\n");

    deep_analyze_backpack().await;
    println!("\n--------------------------------------------------------------\n");
    deep_analyze_edgex().await;

    println!("\n==============================================================");
    Ok(())
}

fn parse_bp_ts(fill: &aleph_tx::backpack_api::model::BackpackFill) -> u64 {
    if let Some(t) = &fill.timestamp {
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
}

async fn deep_analyze_backpack() {
    println!("🎒 [BACKPACK EXCHANGE — Per-Trade Breakdown]");
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
        let page = match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.get_recent_fills("ETH_USDC_PERP", 100, offset),
        )
        .await
        {
            Ok(Ok(f)) => f,
            _ => break,
        };
        if page.is_empty() {
            break;
        }
        let len = page.len() as u32;
        let mut stop = false;
        for f in page {
            let ts = parse_bp_ts(&f);
            if ts > 0 && ts < cutoff {
                stop = true;
                continue;
            }
            all_fills.push(f);
        }
        offset += len;
        if stop || len < 100 {
            break;
        }
    }

    all_fills.sort_by_key(|f| parse_bp_ts(f));
    println!("  Total fills (24h): {}\n", all_fills.len());

    // Track round-trips
    let mut position: f64 = 0.0;
    let mut entry_price: f64 = 0.0;
    let mut round_trips: Vec<(f64, f64, f64, String)> = vec![]; // (pnl, fee, size, description)
    let mut fees_total: f64 = 0.0;
    let mut gross_pnl: f64 = 0.0;
    let mut win_count = 0;
    let mut loss_count = 0;
    let mut biggest_win: f64 = f64::MIN;
    let mut biggest_loss: f64 = f64::MAX;

    for fill in &all_fills {
        let price: f64 = fill.price.parse().unwrap_or(0.0);
        let qty: f64 = fill.quantity.parse().unwrap_or(0.0);
        let fee: f64 = fill.fee.parse().unwrap_or(0.0);
        let dir: f64 = if fill.side.eq_ignore_ascii_case("Bid") {
            1.0
        } else {
            -1.0
        };
        fees_total += fee;

        if position == 0.0 {
            entry_price = price;
            position = qty * dir;
        } else if position * dir > 0.0 {
            entry_price = (entry_price * position.abs() + price * qty) / (position.abs() + qty);
            position += qty * dir;
        } else {
            let reduce = f64::min(qty, position.abs());
            let pnl = (price - entry_price) * reduce * position.signum();
            gross_pnl += pnl;
            if pnl >= 0.0 {
                win_count += 1;
            } else {
                loss_count += 1;
            }
            if pnl > biggest_win {
                biggest_win = pnl;
            }
            if pnl < biggest_loss {
                biggest_loss = pnl;
            }
            let desc = format!(
                "{} {:.4}@{:.2} → close@{:.2}",
                if position > 0.0 { "Long" } else { "Short" },
                reduce,
                entry_price,
                price
            );
            round_trips.push((pnl, fee, reduce, desc));
            position += qty * dir;
            if (position.abs() > 0.001) && (position.signum() != (position - qty * dir).signum()) {
                entry_price = price;
            }
        }
    }

    // Print round trip sample
    println!("  === Sample Round-Trip Trades (first 20) ===");
    for (i, (pnl, fee, size, desc)) in round_trips.iter().take(20).enumerate() {
        let icon = if *pnl >= 0.0 { "✅" } else { "❌" };
        println!(
            "  {:>3}. {} PnL: {:>+8.4} Fee: {:>6.4} Size: {:.4} | {}",
            i + 1,
            icon,
            pnl,
            fee,
            size,
            desc
        );
    }
    if round_trips.len() > 20 {
        println!("  ... ({} more round-trips)", round_trips.len() - 20);
    }

    // Statistics
    let total_rt = round_trips.len();
    let avg_win = if win_count > 0 {
        round_trips
            .iter()
            .filter(|r| r.0 >= 0.0)
            .map(|r| r.0)
            .sum::<f64>()
            / win_count as f64
    } else {
        0.0
    };
    let avg_loss = if loss_count > 0 {
        round_trips
            .iter()
            .filter(|r| r.0 < 0.0)
            .map(|r| r.0)
            .sum::<f64>()
            / loss_count as f64
    } else {
        0.0
    };

    println!("\n  === Statistics ===");
    println!(
        "  Round-trips: {} (Win: {} / Loss: {})",
        total_rt, win_count, loss_count
    );
    println!(
        "  Win Rate: {:.1}%",
        if total_rt > 0 {
            win_count as f64 / total_rt as f64 * 100.0
        } else {
            0.0
        }
    );
    println!("  Avg Win:  ${:.4} | Avg Loss: ${:.4}", avg_win, avg_loss);
    println!(
        "  Biggest Win: ${:.4} | Biggest Loss: ${:.4}",
        if biggest_win > f64::MIN {
            biggest_win
        } else {
            0.0
        },
        if biggest_loss < f64::MAX {
            biggest_loss
        } else {
            0.0
        }
    );
    println!("  Gross PnL: ${:.4}", gross_pnl);
    println!("  Fees Paid: ${:.4}", fees_total);
    println!("  Net PnL:   ${:.4}", gross_pnl - fees_total);
    if position.abs() > 0.001 {
        println!("  Open Pos:  {:.4} ETH @ ${:.2}", position, entry_price);
    }
}

async fn deep_analyze_edgex() {
    println!("🔌 [EDGEX EXCHANGE — Per-Trade Breakdown]");
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

    // First dump a sample to check timestamp format
    let sample = match tokio::time::timeout(
        std::time::Duration::from_secs(5),
        client.get_fills(account_id, 1, 3),
    )
    .await
    {
        Ok(Ok(f)) => f,
        _ => vec![],
    };
    if !sample.is_empty() {
        println!(
            "  [DEBUG] EdgeX sample fill matchTime: {}",
            sample[0].match_time
        );
        let ts: u64 = sample[0].match_time.parse().unwrap_or(0);
        println!(
            "  [DEBUG] Parsed as epoch ms: {} | cutoff: {} | now: {}",
            ts, cutoff, now_ms
        );
        println!(
            "  [DEBUG] Age: {:.1} hours",
            (now_ms - ts) as f64 / 3600000.0
        );
    }

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
            _ => break,
        };
        if page_fills.is_empty() {
            break;
        }
        let len = page_fills.len();
        let mut stop = false;
        for f in page_fills {
            let ts: u64 = f.match_time.parse().unwrap_or(0);
            if ts > 0 && ts < cutoff {
                stop = true;
                continue;
            }
            all_fills.push(f);
        }
        page += 1;
        if stop || len < 100 || page > 50 {
            break;
        }
    }

    all_fills.sort_by_key(|f| f.match_time.parse::<u64>().unwrap_or(0));
    println!("  Total fills (24h): {}\n", all_fills.len());

    let mut position: f64 = 0.0;
    let mut entry_price: f64 = 0.0;
    let mut round_trips: Vec<(f64, f64, f64, String)> = vec![];
    let mut fees_total: f64 = 0.0;
    let mut gross_pnl: f64 = 0.0;
    let mut win_count = 0;
    let mut loss_count = 0;
    let mut biggest_win: f64 = f64::MIN;
    let mut biggest_loss: f64 = f64::MAX;

    for fill in &all_fills {
        let price: f64 = fill.fill_price.parse().unwrap_or(0.0);
        let qty: f64 = fill.fill_size.parse().unwrap_or(0.0);
        let fee: f64 = fill.fill_fee.parse().unwrap_or(0.0);
        let dir: f64 = if format!("{:?}", fill.order_side).eq_ignore_ascii_case("Buy") {
            1.0
        } else {
            -1.0
        };
        fees_total += fee;

        if position == 0.0 {
            entry_price = price;
            position = qty * dir;
        } else if position * dir > 0.0 {
            entry_price = (entry_price * position.abs() + price * qty) / (position.abs() + qty);
            position += qty * dir;
        } else {
            let reduce = f64::min(qty, position.abs());
            let pnl = (price - entry_price) * reduce * position.signum();
            gross_pnl += pnl;
            if pnl >= 0.0 {
                win_count += 1;
            } else {
                loss_count += 1;
            }
            if pnl > biggest_win {
                biggest_win = pnl;
            }
            if pnl < biggest_loss {
                biggest_loss = pnl;
            }
            let desc = format!(
                "{} {:.4}@{:.2} → close@{:.2}",
                if position > 0.0 { "Long" } else { "Short" },
                reduce,
                entry_price,
                price
            );
            round_trips.push((pnl, fee, reduce, desc));
            position += qty * dir;
            if (position.abs() > 0.001) && (position.signum() != (position - qty * dir).signum()) {
                entry_price = price;
            }
        }
    }

    println!("  === Sample Round-Trip Trades (first 20) ===");
    for (i, (pnl, fee, size, desc)) in round_trips.iter().take(20).enumerate() {
        let icon = if *pnl >= 0.0 { "✅" } else { "❌" };
        println!(
            "  {:>3}. {} PnL: {:>+8.4} Fee: {:>6.4} Size: {:.4} | {}",
            i + 1,
            icon,
            pnl,
            fee,
            size,
            desc
        );
    }
    if round_trips.len() > 20 {
        println!("  ... ({} more round-trips)", round_trips.len() - 20);
    }

    let total_rt = round_trips.len();
    let avg_win = if win_count > 0 {
        round_trips
            .iter()
            .filter(|r| r.0 >= 0.0)
            .map(|r| r.0)
            .sum::<f64>()
            / win_count as f64
    } else {
        0.0
    };
    let avg_loss = if loss_count > 0 {
        round_trips
            .iter()
            .filter(|r| r.0 < 0.0)
            .map(|r| r.0)
            .sum::<f64>()
            / loss_count as f64
    } else {
        0.0
    };

    println!("\n  === Statistics ===");
    println!(
        "  Round-trips: {} (Win: {} / Loss: {})",
        total_rt, win_count, loss_count
    );
    println!(
        "  Win Rate: {:.1}%",
        if total_rt > 0 {
            win_count as f64 / total_rt as f64 * 100.0
        } else {
            0.0
        }
    );
    println!("  Avg Win:  ${:.4} | Avg Loss: ${:.4}", avg_win, avg_loss);
    println!(
        "  Biggest Win: ${:.4} | Biggest Loss: ${:.4}",
        if biggest_win > f64::MIN {
            biggest_win
        } else {
            0.0
        },
        if biggest_loss < f64::MAX {
            biggest_loss
        } else {
            0.0
        }
    );
    println!("  Gross PnL: ${:.4}", gross_pnl);
    println!("  Fees Paid: ${:.4}", fees_total);
    println!("  Net PnL:   ${:.4}", gross_pnl - fees_total);
    if position.abs() > 0.001 {
        println!("  Open Pos:  {:.4} ETH @ ${:.2}", position, entry_price);
    }
}
