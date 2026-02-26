use tracing_subscriber::{fmt, EnvFilter};
use std::time::Duration;

use aleph_tx::shm_reader::ShmReader;
use aleph_tx::strategy::{Strategy, arbitrage::ArbitrageEngine, market_maker::MarketMakerStrategy};

fn main() -> anyhow::Result<()> {
    // 1. Initialize high-performance logger
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,aleph_tx=debug"));

    fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_thread_ids(false)
        .with_level(true)
        .init();

    tracing::info!("🦀 AlephTX Core starting (Zero-copy IPC Strategy Engine)...");

    // Initialize async runtime for non-blocking HTTP APIs
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();

    // 2. Open lock-free shared memory matrix
    let shm_path = "/dev/shm/aleph-matrix";
    let mut reader = match ShmReader::open(shm_path, 2048) {
        Ok(r) => {
            tracing::info!("📡 Opened {} (scanning 2048 symbols)", shm_path);
            r
        }
        Err(e) => {
            tracing::error!("Failed to open shared memory: {}", e);
            tracing::error!("Make sure the Go feeder is running first.");
            std::process::exit(1);
        }
    };

    // 3. Initialize Strategy Multiplexer
    let mut strategies: Vec<Box<dyn Strategy>> =vec![
        Box::new(ArbitrageEngine::new(5.0)), // > 5.0 bps trigger
        Box::new(MarketMakerStrategy::new(3, 1002, 2.5)), // Exchange 3 (EdgeX), Symbol 1002 (ETH)
    ];

    tracing::info!("⏳ Booted {} strategies. Waiting for market data...", strategies.len());

    // 4. Main ultra-low latency spin loop
    let mut loop_count: u64 = 0;
    
    loop {
        match reader.try_poll() {
            Some(symbol_id) => {
                // Read all exchanges atomically for this symbol
                let exchanges = reader.read_all_exchanges(symbol_id);
                
                // Multiplex the updates to all active strategies
                for (exch_idx, bbo) in exchanges.iter() {
                    // Only pass valid BBOs
                    if bbo.bid_price > 0.0 && bbo.ask_price > 0.0 {
                        for strategy in strategies.iter_mut() {
                            strategy.on_bbo_update(symbol_id, *exch_idx, bbo);
                        }
                    }
                }
            }
            None => {
                loop_count += 1;
                
                for strategy in strategies.iter_mut() {
                    strategy.on_idle();
                }

                if loop_count % 1_000_000 == 0 {
                    std::thread::sleep(Duration::from_micros(1));
                } else {
                    std::hint::spin_loop();
                }
            }
        }
    }
}
