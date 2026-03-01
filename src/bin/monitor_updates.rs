/// Debug tool to monitor shared memory version updates in real-time
use aleph_tx::shm_reader::ShmReader;
use std::thread;
use std::time::Duration;

fn main() {
    println!("🔍 Monitoring shared memory version updates...\n");

    let mut reader = ShmReader::open("/dev/shm/aleph-matrix", 2048)
        .expect("Failed to open shared memory");

    let mut last_versions = vec![0u64; 2048];
    let mut update_count = 0;

    loop {
        // Poll for updates
        if let Some(symbol_id) = reader.try_poll() {
            update_count += 1;
            let exchanges = reader.read_all_exchanges(symbol_id);

            for (exch_idx, bbo) in exchanges.iter() {
                if bbo.bid_price > 0.0 || bbo.ask_price > 0.0 {
                    println!(
                        "[{}] Symbol {} Exch {} -> Bid: {:.2}@{:.3} Ask: {:.2}@{:.3}",
                        update_count,
                        symbol_id,
                        exch_idx,
                        bbo.bid_price,
                        bbo.bid_size,
                        bbo.ask_price,
                        bbo.ask_size
                    );
                }
            }
        } else {
            // No updates, sleep briefly
            thread::sleep(Duration::from_millis(10));
        }

        // Print status every 5 seconds
        if update_count % 100 == 0 && update_count > 0 {
            println!("--- {} updates received ---", update_count);
        }
    }
}
