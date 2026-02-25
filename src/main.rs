use aleph_tx::{
    arbitrage::{self, ArbitrageEngine},
    shm_reader::ShmReader,
};
use std::time::Instant;

const SYMBOL_BTC: u16 = 1001;
const SYMBOL_ETH: u16 = 1002;

fn symbol_name(id: u16) -> &'static str {
    match id {
        1001 => "BTC",
        1002 => "ETH",
        _ => "UNK",
    }
}

fn main() {
    tracing_subscriber::fmt::init();
    tracing::info!("🦀 AlephTX Core starting (Lock-free Shared Matrix)...");

    let shm_path = std::env::var("ALEPH_SHM")
        .unwrap_or_else(|_| "/dev/shm/aleph-matrix".to_string());
    
    let num_symbols = std::env::var("ALEPH_SYMBOLS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(32);

    let mut reader = ShmReader::open(&shm_path, num_symbols)
        .expect("Failed to open shared memory");
    tracing::info!("📡 Opened {} (scanning {} symbols)", shm_path, num_symbols);

    let engine = ArbitrageEngine::new(3.0);
    
    let mut poll_count: u64 = 0;
    let mut last_log = Instant::now();
    let mut last_stats = Instant::now();

    tracing::info!("⏳ Waiting for market data...");

    let mut loop_count: u64 = 0;
    loop {
        loop_count += 1;
        
        if let Some(sym) = reader.try_poll() {
            poll_count += 1;

            if let Some(signal) = engine.check(&mut reader, sym) {
                arbitrage::execute_arbitrage(&signal);
            }

            if sym == SYMBOL_BTC || sym == SYMBOL_ETH {
                if let Some(gb) = engine.find_global_best(&mut reader, sym) {
                    if gb.has_arb() {
                        tracing::info!(
                            "📊 {} GBB={:.2}@x{} GBA={:.2}@x{} spread={:.2}bps",
                            symbol_name(sym),
                            gb.bid_price,
                            gb.bid_exchange,
                            gb.ask_price,
                            gb.ask_exchange,
                            (gb.spread() / gb.mid()) * 10_000.0
                        );
                    }
                }
            }
        } else {
            std::hint::spin_loop();
        }

        if last_log.elapsed().as_millis() >= 1000 {
            let elapsed = last_stats.elapsed().as_secs_f64();
            if poll_count > 0 {
                tracing::info!(
                    "📈 poll/s: {:.0}k | BTC v{} ETH v{}",
                    poll_count as f64 / elapsed,
                    reader.local_version(SYMBOL_BTC),
                    reader.local_version(SYMBOL_ETH)
                );
            }
            poll_count = 0;
            last_stats = Instant::now();
        }
        last_log = Instant::now();
        
        if loop_count > 10_000_000 {
            tracing::info!("Exiting after 10M iterations");
            break;
        }
    }
}
