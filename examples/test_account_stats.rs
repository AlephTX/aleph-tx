use aleph_tx::account_stats_reader::AccountStatsReader;

fn main() {
    let mut reader = AccountStatsReader::open("/dev/shm/aleph-account-stats")
        .expect("Failed to open account stats");

    println!("Reading account stats...");
    let stats = reader.read();

    println!("Collateral: ${:.2}", stats.collateral);
    println!("Portfolio Value: ${:.2}", stats.portfolio_value);
    println!("Leverage: {:.2}x", stats.leverage);
    println!("Available Balance: ${:.2}", stats.available_balance);
    println!("Margin Usage: {:.2}%", stats.margin_usage);
    println!("Buying Power: ${:.2}", stats.buying_power);
}
