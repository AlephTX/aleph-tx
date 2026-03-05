//! 测试 Lighter 交易功能
//!
//! 用法：
//!   make build && cargo run --bin test_trading -- buy 0.001 2100.50
//!   cargo run --bin test_trading -- sell 0.001 2150.00
//!   cargo run --bin test_trading -- batch 0.001 2100.50 2150.00
//!   cargo run --bin test_trading -- query <order_index>
//!   cargo run --bin test_trading -- verify <order_index> buy 2100.50 0.001
//!   cargo run --bin test_trading -- cancel <order_index>
//!   cargo run --bin test_trading -- cancel-all
//!   cargo run --bin test_trading -- list

use aleph_tx::lighter_trading::{BatchOrderParams, LighterTrading, Side};
use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("aleph_tx=info".parse().unwrap()),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        print_usage();
        return Ok(());
    }

    // market_id=0 即 ETH perps
    let trading = LighterTrading::new(0).await?;

    match args[1].as_str() {
        "buy" => {
            if args.len() != 4 {
                eprintln!("用法: test_trading buy <size> <price>");
                return Ok(());
            }
            let size: f64 = args[2].parse()?;
            let price: f64 = args[3].parse()?;
            println!("下买单: {:.4} @ ${:.2}", size, price);
            let result = trading.buy(size, price).await?;
            println!("买单已提交: tx_hash={} coi={}", result.tx_hash, result.client_order_index);
        }

        "sell" => {
            if args.len() != 4 {
                eprintln!("用法: test_trading sell <size> <price>");
                return Ok(());
            }
            let size: f64 = args[2].parse()?;
            let price: f64 = args[3].parse()?;

            println!("下卖单: {:.4} @ ${:.2}", size, price);
            let result = trading.sell(size, price).await?;
            println!("卖单已提交: tx_hash={} coi={}", result.tx_hash, result.client_order_index);
        }

        "batch" => {
            if args.len() != 5 {
                eprintln!("用法: test_trading batch <size> <bid_price> <ask_price>");
                return Ok(());
            }
            let size: f64 = args[2].parse()?;
            let bid_price: f64 = args[3].parse()?;
            let ask_price: f64 = args[4].parse()?;

            println!("批量下单: {:.4}, Bid=${:.2} Ask=${:.2}", size, bid_price, ask_price);
            let result = trading.place_batch(BatchOrderParams { bid_price, ask_price, size }).await?;
            println!("批量已提交: tx_hashes={:?}", result.tx_hashes);
            println!("  bid_coi={} ask_coi={}", result.bid_client_order_index, result.ask_client_order_index);
        }

        "list" => {
            println!("查询活跃订单...");
            let orders = trading.get_active_orders().await?;
            println!("共 {} 笔活跃订单:\n", orders.len());
            for o in &orders {
                println!(
                    "  [{}] {} {} @ ${} size={} remaining={} status={}",
                    o.order_index,
                    if o.is_ask { "SELL" } else { "BUY " },
                    o.order_type,
                    o.price,
                    o.initial_base_amount,
                    o.remaining_base_amount,
                    o.status,
                );
            }
        }
        "query" => {
            if args.len() != 3 {
                eprintln!("用法: test_trading query <order_index>");
                return Ok(());
            }
            let order_index: i64 = args[2].parse()?;

            println!("查询订单 {}...", order_index);
            let o = trading.get_order(order_index).await?;
            println!("订单详情:");
            println!("  order_index:    {}", o.order_index);
            println!("  client_order:   {}", o.client_order_index);
            println!("  side:           {}", if o.is_ask { "SELL" } else { "BUY" });
            println!("  price:          ${}", o.price);
            println!("  initial_size:   {}", o.initial_base_amount);
            println!("  remaining_size: {}", o.remaining_base_amount);
            println!("  filled_base:    {}", o.filled_base_amount);
            println!("  filled_quote:   {}", o.filled_quote_amount);
            println!("  status:         {}", o.status);
            println!("  type:           {}", o.order_type);
            println!("  time_in_force:  {}", o.time_in_force);
            println!("  reduce_only:    {}", o.reduce_only);
        }

        "verify" => {
            if args.len() != 6 {
                eprintln!("用法: test_trading verify <order_index> <buy|sell> <price> <size>");
                return Ok(());
            }
            let order_index: i64 = args[2].parse()?;
            let side = match args[3].as_str() {
                "buy" => Side::Buy,
                "sell" => Side::Sell,
                _ => { eprintln!("方向必须是 buy 或 sell"); return Ok(()); }
            };
            let price: f64 = args[4].parse()?;
            let size: f64 = args[5].parse()?;

            println!("验证订单 {}...", order_index);
            let ok = trading.verify_order(order_index, side, price, size).await?;
            println!("{}", if ok { "验证通过" } else { "验证失败" });
        }

        "cancel" => {
            if args.len() != 3 {
                eprintln!("用法: test_trading cancel <order_index>");
                return Ok(());
            }
            let order_index: i64 = args[2].parse()?;
            trading.cancel_order(order_index).await?;
            println!("已撤销订单 {}", order_index);
        }

        "cancel-all" => {
            let count = trading.cancel_all().await?;
            println!("已撤销 {} 笔订单", count);
        }

        "position" => {
            println!("查询仓位...");
            match trading.get_position().await? {
                Some(pos) => {
                    let size: f64 = pos.position.parse().unwrap_or(0.0);
                    let side = if pos.sign > 0 { "LONG" } else if pos.sign < 0 { "SHORT" } else { "FLAT" };
                    println!("  {} {} {} @ avg ${}", pos.symbol, side, size.abs(), pos.avg_entry_price);
                    println!("  unrealized_pnl: ${}", pos.unrealized_pnl);
                    println!("  liquidation:    ${}", pos.liquidation_price);
                }
                None => println!("  无仓位"),
            }
        }

        "close-all" => {
            if args.len() != 3 {
                eprintln!("用法: test_trading close-all <current_price>");
                return Ok(());
            }
            let current_price: f64 = args[2].parse()?;
            println!("关闭所有仓位 (参考价 ${:.2})...", current_price);
            trading.close_all_positions(current_price).await?;
            println!("平仓完成");
        }

        _ => print_usage(),
    }

    Ok(())
}

fn print_usage() {
    println!("Lighter 交易测试工具\n");
    println!("用法:");
    println!("  test_trading buy <size> <price>");
    println!("  test_trading sell <size> <price>");
    println!("  test_trading batch <size> <bid_price> <ask_price>");
    println!("  test_trading list");
    println!("  test_trading query <order_index>");
    println!("  test_trading verify <order_index> <buy|sell> <price> <size>");
    println!("  test_trading cancel <order_index>");
    println!("  test_trading cancel-all");
    println!("  test_trading position");
    println!("  test_trading close-all <current_price>");
    println!("\n示例:");
    println!("  test_trading buy 0.001 2100.50");
    println!("  test_trading batch 0.001 2100.50 2150.00");
    println!("  test_trading list");
    println!("  test_trading close-all 2100.00");
}
