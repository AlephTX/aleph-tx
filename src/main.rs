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

            match msg_type {
                MSG_TYPE_TICKER => {
                    // symbol(12) + bid(16) + ask(16) + ts(8) = 52 bytes
                    if payload.len() >= 52 {
                        let symbol = std::str::from_utf8(&payload[0..12]).unwrap_or("").trim();
                        // bid/ask are 16-byte fixed strings, need to parse as string
                        let bid_slice = std::str::from_utf8(&payload[12..28]).unwrap_or("").trim_end_matches('\0');
                        let ask_slice = std::str::from_utf8(&payload[28..44]).unwrap_or("").trim_end_matches('\0');

                        let ticker = aleph_tx::types::Ticker {
                            symbol: Symbol::new(symbol),
                            bid: Decimal::from_str(bid_slice).unwrap_or(Decimal::ZERO),
                            ask: Decimal::from_str(ask_slice).unwrap_or(Decimal::ZERO),
                            last: Decimal::ZERO,
                            volume_24h: Decimal::ZERO,
                            timestamp: 0,
                        };
                        state.update_ticker(ticker);
                    }
                }
                MSG_TYPE_DEPTH => {
                    // symbol(12) + 6 bids(96) + 6 asks(96) + ts(8) = 212 bytes
                    if payload.len() >= 212 {
                        let symbol = std::str::from_utf8(&payload[0..12]).unwrap_or("").trim();

                        let mut bids = Vec::new();
                        let mut asks = Vec::new();
                        let mut off = 12;

                        // 6 bids
                        for _ in 0..6 {
                            if off + 16 > payload.len() { break; }
                            let price_slice = std::str::from_utf8(&payload[off..off+8]).unwrap_or("").trim_end_matches('\0');
                            let qty_slice = std::str::from_utf8(&payload[off+8..off+16]).unwrap_or("").trim_end_matches('\0');
                            if !price_slice.is_empty() && !price_slice.starts_with('\0') {
                                bids.push([price_slice.to_string(), qty_slice.to_string()]);
                            }
                            off += 16;
                        }
                        // 6 asks
                        for _ in 0..6 {
                            if off + 16 > payload.len() { break; }
                            let price_slice = std::str::from_utf8(&payload[off..off+8]).unwrap_or("").trim_end_matches('\0');
                            let qty_slice = std::str::from_utf8(&payload[off+8..off+16]).unwrap_or("").trim_end_matches('\0');
                            if !price_slice.is_empty() && !price_slice.starts_with('\0') {
                                asks.push([price_slice.to_string(), qty_slice.to_string()]);
                            }
                            off += 16;
                        }

                        if !bids.is_empty() && !asks.is_empty() {
                            let ob = orderbooks
                                .entry(symbol.to_string())
                                .or_insert_with(|| LocalOrderbook::new(Symbol::new(symbol)));
                            ob.apply(&bids, &asks, 0);
                            if let (Some(bid), Some(ask)) = (ob.best_bid(), ob.best_ask()) {
                                tracing::info!(
                                    "[OB {}] bid={} ask={} spread={}",
                                    symbol,
                                    bid.price,
                                    ask.price,
                                    ob.spread().unwrap_or_default()
                                );
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
