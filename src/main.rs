use aleph_tx::{
    arbitrage::{self, ArbitrageEngine, EXCHANGE_HYPERLIQUID, EXCHANGE_LIGHTER, EXCHANGE_EDGEX, EXCHANGE_01},
    shm_reader::ShmReader,
};
use std::time::Instant;

const SYMBOL_BTC: u16 = 1001;
const SYMBOL_ETH: u16 = 1002;

/// Symbol ID to name (for logging).
fn symbol_name(id: u16) -> &'static str {
    match id {
        1001 => "BTC",
        1002 => "ETH",
        _ => "UNK",
    }
}

/// Exchange ID to name.
fn exchange_name(id: u8) -> &'static str {
    match id {
        1 => "Hyperliquid",
        2 => "Lighter",
        3 => "EdgeX",
        4 => "01",
        _ => "Unknown",
    }
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    tracing::info!("🦀 AlephTX Core starting (Lock-free Shared Matrix)...");

    // Open shared memory (version-based architecture)
    let shm_path = std::env::var("ALEPH_SHM")
        .unwrap_or_else(|_| "/dev/shm/aleph-matrix".to_string());
    
    let num_symbols = std::env::var("ALEPH_SYMBOLS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(32);

    let mut reader = ShmReader::open(&shm_path, num_symbols)?;
    tracing::info!("📡 Opened {} (scanning {} symbols)", shm_path, num_symbols);

    // O(1) scalable arbitrage engine
    let engine = ArbitrageEngine::new(3.0); // 3 bps min spread
    
    let mut poll_count: u64 = 0;
    let mut last_log = Instant::now();
    let mut last_stats = Instant::now();

    tracing::info!("⏳ Waiting for market data...");

    // Version-based spin loop: only react to latest state
    loop {
        // Poll for updated symbols (O(max_symbols) scan, ~16KB, cache-friendly)
        if let Some(sym) = reader.try_poll() {
            poll_count += 1;

            // Check for arbitrage on this specific symbol
            if let Some(signal) = engine.check(&mut reader, sym) {
                arbitrage::execute_arbitrage(&signal);
            }

            // Log BBO for tracked symbols
            if sym == SYMBOL_BTC || sym == SYMBOL_ETH {
                let name = symbol_name(sym);
                let global = engine.find_global_best(&mut reader, sym);
                
                if let Some(gb) = global {
                    if gb.has_arb() {
                        tracing::info!(
                            "📊 {} GBB={:.2}@{} GBA={:.2}@{} spread={:.2}bps",
                            name,
                            gb.bid_price,
                            exchange_name(gb.bid_exchange),
                            gb.ask_price,
                            exchange_name(gb.ask_exchange),
                            (gb.spread() / gb.mid()) * 10_000.0
                        );
                    }
                }
            }
        } else {
            // No updates — PAUSE to save power
            std::hint::spin_loop();
        }

        // Periodic stats logging
        if last_log.elapsed().as_millis() >= 1000 {
            let elapsed = last_stats.elapsed().as_secs_f64();
            tracing::info!(
                "📈 poll/s: {:.0}k | total polls: {} | sym versions: BTC={} ETH={}",
                poll_count as f64 / elapsed,
                poll_count,
                reader.local_version(SYMBOL_BTC),
                reader.local_version(SYMBOL_ETH)
            );
            poll_count = 0;
            last_stats = Instant::now();
        }
        last_log = Instant::now();
    }
}
