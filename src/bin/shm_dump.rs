use aleph_tx::shm_reader::ShmReader;

fn main() -> anyhow::Result<()> {
    let mut reader = ShmReader::open("/dev/shm/aleph-matrix", 2048)?;
    println!("Dumping active SHM data...");

    let mut found = 0;
    for sym_id in 0..2048 {
        let exchanges = reader.read_all_exchanges(sym_id as u16);
        for (exch_idx, bbo) in exchanges.iter() {
            if bbo.bid_price > 0.0 || bbo.ask_price > 0.0 {
                found += 1;
                println!(
                    "Symbol: {}, Exch: {} -> Bid: {:.2}@{:.3}, Ask: {:.2}@{:.3} (TS: {})",
                    sym_id,
                    exch_idx,
                    bbo.bid_price,
                    bbo.bid_size,
                    bbo.ask_price,
                    bbo.ask_size,
                    bbo.timestamp_ns
                );
            }
        }
    }

    if found == 0 {
        println!("No active prices found in SHM!");
    }
    Ok(())
}
