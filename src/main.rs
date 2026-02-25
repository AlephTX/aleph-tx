use aleph_tx::{
    arbitrage::{self, GlobalMarketState, EXCHANGE_HYPERLIQUID, EXCHANGE_LIGHTER, EXCHANGE_EDGEX, EXCHANGE_01},
    shm_reader::ShmRingReader,
};
use std::time::Instant;

const SYMBOL_BTC: u16 = 1001;
const SYMBOL_ETH: u16 = 1002;

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
    tracing::info!("🦀 AlephTX Core starting...");

    let ring_name = std::env::var("ALEPH_RING").unwrap_or_else(|_| "aleph-bbo".into());

    let mut reader = ShmRingReader::open(&ring_name, 1024)?;
    tracing::info!("📡 Reading from /dev/shm/{} (1024 slots)", ring_name);

    let mut state = GlobalMarketState::new(3.0); // 3 bps min spread
    let mut msg_count: u64 = 0;
    let mut last_log = Instant::now();

    tracing::info!("⏳ Waiting for data...");

    loop {
        if let Some(msg) = reader.try_read() {
            state.update(&msg);
            msg_count += 1;

            // Log every second
            if last_log.elapsed().as_millis() >= 1000 {
                last_log = Instant::now();

                // Print BBO for all exchanges
                for &sym in &[SYMBOL_BTC, SYMBOL_ETH] {
                    let sym_name = if sym == SYMBOL_BTC { "BTC" } else { "ETH" };
                    for &exch in &[EXCHANGE_HYPERLIQUID, EXCHANGE_LIGHTER, EXCHANGE_EDGEX, EXCHANGE_01] {
                        if let Some(bbo) = state.get_bbo(sym, exch) {
                            if bbo.is_valid() {
                                tracing::info!(
                                    "[{:>12}] {} bid={:.2} ask={:.2} spread={:.4}",
                                    exchange_name(exch),
                                    sym_name,
                                    bbo.bid_price,
                                    bbo.ask_price,
                                    bbo.ask_price - bbo.bid_price,
                                );
                            }
                        }
                    }

                    // Check arbitrage across all exchange pairs
                    if let Some(signal) = state.check_arbitrage(sym) {
                        arbitrage::execute_arbitrage(&signal);
                    }
                }

                tracing::info!("--- msgs/s: {} total: {} ---", msg_count, reader.read_idx());
                msg_count = 0;
            }
        } else {
            std::hint::spin_loop();
        }
    }
}
