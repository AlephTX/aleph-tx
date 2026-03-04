/// Performance monitoring tool for AlephTX
/// Tracks latency, throughput, and strategy performance metrics
use aleph_tx::backpack_api::client::BackpackClient;
use aleph_tx::edgex_api::client::EdgeXClient;
use std::time::{Duration, Instant};
use tokio::time::sleep;

#[derive(Default)]
struct PerformanceMetrics {
    // Latency metrics
    avg_tick_to_quote_ms: f64,
    p50_latency_ms: f64,
    p95_latency_ms: f64,
    p99_latency_ms: f64,

    // Throughput
    quotes_per_second: f64,
    fills_per_hour: u32,

    // Strategy performance
    total_pnl_usd: f64,
    sharpe_ratio: f64,
    max_drawdown_pct: f64,
    win_rate_pct: f64,

    // Risk metrics
    current_position: f64,
    max_position_seen: f64,
    adverse_selection_rate: f64,
}

impl PerformanceMetrics {
    fn print_report(&self) {
        println!("\n╔══════════════════════════════════════════════════════════╗");
        println!("║          AlephTX Performance Report                     ║");
        println!("╠══════════════════════════════════════════════════════════╣");
        println!("║ LATENCY METRICS                                          ║");
        println!("║  Avg Tick-to-Quote: {:.2} ms                          ║", self.avg_tick_to_quote_ms);
        println!("║  P50 Latency:       {:.2} ms                          ║", self.p50_latency_ms);
        println!("║  P95 Latency:       {:.2} ms                          ║", self.p95_latency_ms);
        println!("║  P99 Latency:       {:.2} ms                          ║", self.p99_latency_ms);
        println!("╠══════════════════════════════════════════════════════════╣");
        println!("║ THROUGHPUT                                               ║");
        println!("║  Quotes/sec:        {:.1}                              ║", self.quotes_per_second);
        println!("║  Fills/hour:        {}                                  ║", self.fills_per_hour);
        println!("╠══════════════════════════════════════════════════════════╣");
        println!("║ STRATEGY PERFORMANCE                                     ║");
        println!("║  Total PnL:         ${:.2}                            ║", self.total_pnl_usd);
        println!("║  Sharpe Ratio:      {:.2}                              ║", self.sharpe_ratio);
        println!("║  Max Drawdown:      {:.2}%                             ║", self.max_drawdown_pct);
        println!("║  Win Rate:          {:.1}%                             ║", self.win_rate_pct);
        println!("╠══════════════════════════════════════════════════════════╣");
        println!("║ RISK METRICS                                             ║");
        println!("║  Current Position:  {:.4} ETH                          ║", self.current_position);
        println!("║  Max Position:      {:.4} ETH                          ║", self.max_position_seen);
        println!("║  Adverse Sel Rate:  {:.2}%                             ║", self.adverse_selection_rate);
        println!("╚══════════════════════════════════════════════════════════╝\n");
    }
}

async fn fetch_backpack_metrics() -> Result<(f64, f64), Box<dyn std::error::Error>> {
    let env_str = std::fs::read_to_string(
        "/home/metaverse/.openclaw/workspace/aleph-tx/.env.backpack"
    )?;
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

    let client = BackpackClient::new(&api_key, &api_secret, "https://api.backpack.exchange")?;

    // Get position
    let mut position = 0.0;
    if let Ok(positions) = client.get_open_positions().await {
        for pos in positions {
            if pos.symbol == "ETH_USDC_PERP" {
                position = pos.quantity.parse().unwrap_or(0.0);
            }
        }
    }

    // Get equity
    let equity = client.get_total_equity().await.unwrap_or(0.0);

    Ok((position, equity))
}

async fn fetch_edgex_metrics() -> Result<(f64, f64), Box<dyn std::error::Error>> {
    let env_str = std::fs::read_to_string(
        "/home/metaverse/.openclaw/workspace/aleph-tx/.env.edgex"
    )?;
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

    let client = EdgeXClient::new(&key, None)?;

    let mut position = 0.0;
    if let Ok(positions) = client.get_positions(account_id).await {
        for p in positions {
            if p.contract_id == "10000002" {
                position += p.open_size.parse::<f64>().unwrap_or(0.0);
            }
        }
    }

    let mut equity = 0.0;
    if let Ok(balances) = client.get_balances(account_id).await {
        for b in balances {
            let bal: f64 = b.balance.parse().unwrap_or(0.0);
            if bal > equity {
                equity = bal;
            }
        }
    }

    Ok((position, equity))
}

#[tokio::main]
async fn main() {
    println!("🔍 AlephTX Performance Monitor");
    println!("Collecting metrics every 10 seconds...\n");

    let mut metrics = PerformanceMetrics::default();
    let start_time = Instant::now();
    let mut last_equity_bp = 0.0;
    let mut last_equity_ex = 0.0;

    loop {
        // Fetch Backpack metrics
        if let Ok((pos_bp, equity_bp)) = fetch_backpack_metrics().await {
            println!("📊 [Backpack] Position: {:.4} ETH | Equity: ${:.2}", pos_bp, equity_bp);
            metrics.current_position = pos_bp;
            if pos_bp.abs() > metrics.max_position_seen {
                metrics.max_position_seen = pos_bp.abs();
            }

            if last_equity_bp > 0.0 {
                let pnl_change = equity_bp - last_equity_bp;
                metrics.total_pnl_usd += pnl_change;
            }
            last_equity_bp = equity_bp;
        }

        // Fetch EdgeX metrics
        if let Ok((pos_ex, equity_ex)) = fetch_edgex_metrics().await {
            println!("📊 [EdgeX] Position: {:.4} ETH | Equity: ${:.2}", pos_ex, equity_ex);

            if last_equity_ex > 0.0 {
                let pnl_change = equity_ex - last_equity_ex;
                metrics.total_pnl_usd += pnl_change;
            }
            last_equity_ex = equity_ex;
        }

        // Print report every minute
        if start_time.elapsed().as_secs().is_multiple_of(60) {
            metrics.print_report();
        }

        sleep(Duration::from_secs(10)).await;
    }
}
