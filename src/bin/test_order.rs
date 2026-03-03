use reqwest;
use serde::{Deserialize, Serialize};
use std::env;

#[derive(Debug, Serialize)]
struct CreateOrderRequest {
    market_index: i16,
    client_order_index: i64,
    base_amount: i64,
    price: u32,
    is_ask: u8,
    #[serde(rename = "type")]
    order_type: u8,
    time_in_force: u8,
    reduce_only: u8,
    trigger_price: u32,
    order_expiry: i64,
    account_index: i64,
    api_key_index: u8,
    expired_at: i64,
    nonce: i64,
    signature: String,
}

#[derive(Debug, Deserialize)]
struct OrderResponse {
    code: i32,
    message: String,
    data: Option<OrderData>,
}

#[derive(Debug, Deserialize)]
struct OrderData {
    order_id: String,
    tx_hash: String,
}

#[derive(Debug, Deserialize)]
struct TickerResponse {
    code: i32,
    data: Option<TickerData>,
}

#[derive(Debug, Deserialize)]
struct TickerData {
    best_bid: String,
    best_ask: String,
    last_price: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("🧪 Lighter Test Order - Simple Market Maker");

    // Load environment variables
    let account_index: i64 = env::var("LIGHTER_ACCOUNT_INDEX")?
        .parse()?;
    let api_key_index: u8 = env::var("LIGHTER_API_KEY_INDEX")?
        .parse()?;

    println!("✓ Loaded credentials");
    println!("  Account: {}", account_index);
    println!("  API Key: {}", api_key_index);

    // Get current market data
    let client = reqwest::Client::new();
    let ticker_url = "https://api.lighter.xyz/api/v1/ticker?market_index=0";

    println!("\n📊 Fetching BTC-USDC market data...");
    let ticker_resp: TickerResponse = client
        .get(ticker_url)
        .send()
        .await?
        .json()
        .await?;

    if let Some(ticker) = ticker_resp.data {
        let best_bid: f64 = ticker.best_bid.parse()?;
        let best_ask: f64 = ticker.best_ask.parse()?;
        let mid_price = (best_bid + best_ask) / 2.0;

        println!("  Best Bid: ${:.2}", best_bid);
        println!("  Best Ask: ${:.2}", best_ask);
        println!("  Mid Price: ${:.2}", mid_price);

        // Calculate our quotes (0.1% spread)
        let spread = mid_price * 0.001;
        let our_bid = mid_price - spread / 2.0;
        let our_ask = mid_price + spread / 2.0;

        println!("\n📝 Our Quotes");
        println!("  Bid: ${:.2} (size: 0.001 BTC)", our_bid);
        println!("  Ask: ${:.2} (size: 0.001 BTC)", our_ask);

        println!("\n💡 To place orders, we need to:");
        println!("  1. Generate signature using Lighter SDK (Poseidon2 + Schnorr)");
        println!("  2. Submit order via REST API");
        println!("  3. Monitor fills via WebSocket private stream");

        println!("\n⚠️  For now, run the Go test order script:");
        println!("  cd feeder && go run test/order/main.go");
    }

    Ok(())
}
