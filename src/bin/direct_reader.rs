/// Direct market data reader - bypasses version polling
use aleph_tx::shm_reader::ShmReader;
use std::thread;
use std::time::Duration;

fn main() {
    println!("🔍 Direct shared memory reader (bypassing version check)...\n");

    let mut reader = ShmReader::open("/dev/shm/aleph-matrix", 2048)
        .expect("Failed to open shared memory");

    let target_symbols = vec![834u16, 835u16]; // BTC and ETH
    let mut count = 0;

    loop {
        for &symbol_id in &target_symbols {
            let exchanges = reader.read_all_exchanges(symbol_id);

            for (exch_idx, bbo) in exchanges.iter() {
                if bbo.bid_price > 0.0 && bbo.ask_price > 0.0 {
                    println!(
                        "[{}] Symbol {} Exch {} -> Bid: {:.2}@{:.3} Ask: {:.2}@{:.3}",
                        count,
                        symbol_id,
                        exch_idx,
                        bbo.bid_price,
                        bbo.bid_size,
                        bbo.ask_price,
                        bbo.ask_size
                    );
                }
            }
        }

        count += 1;
        thread::sleep(Duration::from_secs(2));

        if count >= 10 {
            break;
        }
    }
}
