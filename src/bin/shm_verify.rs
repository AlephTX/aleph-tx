//! Shared Memory IPC Verification Tool
//!
//! Verifies that Go feeder → Rust reader shared memory exchange works correctly.
//! Tests: BBO Matrix, Event Ring Buffer, Account Stats
//!
//! Usage: cargo run --bin shm_verify (requires Go feeder running)

use aleph_tx::account_stats_reader::AccountStatsReader;
use aleph_tx::shm_event_reader::ShmEventReader;
use aleph_tx::shm_reader::ShmReader;
use std::time::{Duration, Instant};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter("info")
        .init();

    println!("=== AlephTX Shared Memory IPC Verification ===\n");

    let mut passed = 0u32;
    let mut failed = 0u32;

    // Test 1: BBO Matrix
    print!("[1/3] BBO Matrix (/dev/shm/aleph-matrix)... ");
    match ShmReader::open("/dev/shm/aleph-matrix", 2048) {
        Ok(mut reader) => {
            // Wait up to 5s for at least one BBO update
            let start = Instant::now();
            let mut got_data = false;
            while start.elapsed() < Duration::from_secs(5) {
                if let Some(sym_id) = reader.try_poll() {
                    let exchanges = reader.read_all_exchanges(sym_id);
                    for (exch_id, bbo) in &exchanges {
                        if bbo.bid_price > 0.0 && bbo.ask_price > 0.0 {
                            println!("OK");
                            println!("  sym={} exch={} bid={:.2} ask={:.2} spread={:.4}bps",
                                sym_id, exch_id, bbo.bid_price, bbo.ask_price,
                                (bbo.ask_price - bbo.bid_price) / bbo.bid_price * 10000.0);
                            println!("  version={}", reader.shared_version(sym_id));
                            got_data = true;
                            break;
                        }
                    }
                    if got_data { break; }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            if got_data {
                passed += 1;
            } else {
                println!("FAIL (no BBO data within 5s)");
                failed += 1;
            }
        }
        Err(e) => {
            println!("FAIL ({})", e);
            failed += 1;
        }
    }

    // Test 2: Event Ring Buffer
    print!("[2/3] Event Ring Buffer (/dev/shm/aleph-events)... ");
    match ShmEventReader::new_default() {
        Ok(reader) => {
            let write_idx = reader.write_idx();
            let read_idx = reader.local_read_idx();
            println!("OK");
            println!("  write_idx={} read_idx={} unread={}", write_idx, read_idx, write_idx.saturating_sub(read_idx));
            passed += 1;
        }
        Err(e) => {
            println!("FAIL ({})", e);
            failed += 1;
        }
    }

    // Test 3: Account Stats
    print!("[3/3] Account Stats (/dev/shm/aleph-account-stats)... ");
    match AccountStatsReader::open("/dev/shm/aleph-account-stats") {
        Ok(mut reader) => {
            // Try to read stats
            let start = Instant::now();
            let mut got_stats = false;
            while start.elapsed() < Duration::from_secs(5) {
                if let Some(stats) = reader.read_if_updated() {
                    println!("OK");
                    println!("  collateral=${:.2} portfolio=${:.2} leverage={:.2}x",
                        stats.collateral, stats.portfolio_value, stats.leverage);
                    println!("  balance=${:.2} margin={:.1}% position={:.4}",
                        stats.available_balance, stats.margin_usage * 100.0, stats.position);
                    got_stats = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            if !got_stats {
                // Force read (blocking)
                let stats = reader.read();
                if stats.updated_at > 0 {
                    println!("OK (force read)");
                    println!("  collateral=${:.2} leverage={:.2}x position={:.4}",
                        stats.collateral, stats.leverage, stats.position);
                    got_stats = true;
                }
            }
            if got_stats { passed += 1; } else { println!("FAIL (no data)"); failed += 1; }
        }
        Err(e) => {
            println!("FAIL ({})", e);
            failed += 1;
        }
    }

    // Summary
    println!("\n=== Results: {}/{} passed ===", passed, passed + failed);
    if failed > 0 {
        println!("Make sure Go feeder is running: cd feeder && ./feeder-app");
        std::process::exit(1);
    }
    Ok(())
}
