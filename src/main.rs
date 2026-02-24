use aleph_tx::{
    engine::StateMachine,
    orderbook::LocalOrderbook,
    types::Symbol,
};
use rust_decimal::Decimal;
use std::{collections::HashMap, str::FromStr, sync::Arc};

const MSG_TYPE_TICKER: u8 = 1;
const MSG_TYPE_DEPTH: u8 = 2;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    tracing::info!("🦀 AlephTX Core starting...");

    let ring_name = std::env::var("ALEPH_RING").unwrap_or_else(|_| "aleph-ring".into());
    let path = format!("/dev/shm/{}", ring_name);

    let state = Arc::new(StateMachine::new());
    let mut orderbooks: HashMap<String, LocalOrderbook> = HashMap::new();
    
    let mut pos = 0usize;

    tracing::info!("⏳ Waiting for data from {}...", path);

    loop {
        // Read entire file fresh each time (simple, avoids caching issues)
        let data = match std::fs::read(&path) {
            Ok(d) => d,
            Err(_) => {
                std::thread::sleep(std::time::Duration::from_millis(10));
                continue;
            }
        };

        if data.len() <= pos {
            std::thread::sleep(std::time::Duration::from_millis(1));
            continue;
        }

        while pos + 3 <= data.len() {
            let msg_type = data[pos];
            let msg_len = u16::from_le_bytes([data[pos + 1], data[pos + 2]]) as usize;

            if msg_len == 0 || pos + 3 + msg_len > data.len() {
                break;
            }

            let payload = &data[pos + 3..pos + 3 + msg_len];
            pos += 3 + msg_len;

            // Parse as string
            let payload_str = match std::str::from_utf8(payload) {
                Ok(s) => s,
                Err(_) => continue,
            };

            match msg_type {
                MSG_TYPE_TICKER => {
                    // Format: symbol|bid|ask
                    let parts: Vec<&str> = payload_str.split('|').collect();
                    if parts.len() >= 3 {
                        let symbol = parts[0].trim();
                        let bid = Decimal::from_str(parts[1].trim()).unwrap_or(Decimal::ZERO);
                        let ask = Decimal::from_str(parts[2].trim()).unwrap_or(Decimal::ZERO);

                        if !symbol.is_empty() && symbol.len() <= 12 && bid > Decimal::ZERO && ask > Decimal::ZERO {
                            let ticker = aleph_tx::types::Ticker {
                                symbol: Symbol::new(symbol),
                                bid,
                                ask,
                                last: Decimal::ZERO,
                                volume_24h: Decimal::ZERO,
                                timestamp: 0,
                            };
                            state.update_ticker(ticker);
                        }
                    }
                }
                MSG_TYPE_DEPTH => {
                    // Format: symbol|bids|asks (price,qty;price,qty)
                    let parts: Vec<&str> = payload_str.split('|').collect();
                    if parts.len() >= 3 {
                        let symbol = parts[0].trim();
                        
                        // Validate symbol
                        if symbol.is_empty() || symbol.len() > 12 || symbol.chars().any(|c| !c.is_alphanumeric()) {
                            continue;
                        }
                        
                        let mut bids = Vec::new();
                        for b in parts[1].split(';') {
                            let p: Vec<&str> = b.split(',').collect();
                            if p.len() >= 2 {
                                let price = Decimal::from_str(p[0].trim()).unwrap_or(Decimal::ZERO);
                                let qty = Decimal::from_str(p[1].trim()).unwrap_or(Decimal::ZERO);
                                if price > Decimal::ZERO && qty > Decimal::ZERO {
                                    bids.push([price.to_string(), qty.to_string()]);
                                }
                            }
                        }
                        
                        let mut asks = Vec::new();
                        for a in parts[2].split(';') {
                            let p: Vec<&str> = a.split(',').collect();
                            if p.len() >= 2 {
                                let price = Decimal::from_str(p[0].trim()).unwrap_or(Decimal::ZERO);
                                let qty = Decimal::from_str(p[1].trim()).unwrap_or(Decimal::ZERO);
                                if price > Decimal::ZERO && qty > Decimal::ZERO {
                                    asks.push([price.to_string(), qty.to_string()]);
                                }
                            }
                        }

                        if !bids.is_empty() && !asks.is_empty() {
                            let ob = orderbooks
                                .entry(symbol.to_string())
                                .or_insert_with(|| LocalOrderbook::new(Symbol::new(symbol)));
                            ob.apply(&bids, &asks, 0);
                            if let (Some(best_bid), Some(best_ask)) = (ob.best_bid(), ob.best_ask()) {
                                let spread = ob.spread().unwrap_or(Decimal::ZERO);
                                let mid = (best_bid.price + best_ask.price) / Decimal::from(2);
                                let spread_pct = spread / mid * Decimal::from(100);
                                if spread_pct < Decimal::from(2) && spread_pct > Decimal::ZERO {
                                    tracing::info!(
                                        "[OB {}] bid={:.2} ask={:.2} spread={:.4}%",
                                        symbol,
                                        best_bid.price,
                                        best_ask.price,
                                        spread_pct
                                    );
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        if data.len() == pos {
            pos = 0;
        }
    }
}