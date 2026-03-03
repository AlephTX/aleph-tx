//! Event Buffer Monitor - Debug tool for inspecting the event ring buffer

use aleph_tx::shm_event_reader::ShmEventReader;
use aleph_tx::types::EventType;
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("🔍 AlephTX Event Buffer Monitor");
    println!("================================\n");

    let mut reader = match ShmEventReader::new_default() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("❌ Failed to open event buffer: {}", e);
            eprintln!("   Make sure the Go feeder is running.");
            eprintln!("   Expected file: /dev/shm/aleph-events");
            return Err(Box::new(e));
        }
    };

    println!("✅ Event buffer opened successfully");
    println!("   Write index: {}", reader.write_idx());
    println!("   Read index:  {}", reader.local_read_idx());
    println!("   Unread:      {}\n", reader.unread_count());

    println!("Monitoring events (Ctrl+C to exit)...\n");

    let mut event_count = 0;
    let mut last_sequence = 0u64;

    loop {
        if let Some(event) = reader.try_read() {
            event_count += 1;

            // Check for sequence gaps
            if event.sequence != last_sequence + 1 && last_sequence > 0 {
                println!(
                    "⚠️  Sequence gap detected: {} -> {}",
                    last_sequence, event.sequence
                );
            }
            last_sequence = event.sequence;

            // Print event details
            let event_type_str = match event.event_type() {
                Some(EventType::OrderCreated) => "Created",
                Some(EventType::OrderFilled) => "Filled",
                Some(EventType::OrderCanceled) => "Canceled",
                Some(EventType::OrderRejected) => "Rejected",
                None => "Unknown",
            };

            println!(
                "[{}] {} | Exchange: {} | Symbol: {} | Order: {}",
                event.sequence, event_type_str, event.exchange_id, event.symbol_id, event.order_id
            );

            if event.event_type() == Some(EventType::OrderFilled) {
                println!(
                    "       Fill: {:.4} @ {:.2} | Fee: {:.4} | Remaining: {:.4}",
                    event.fill_size, event.fill_price, event.fee_paid, event.remaining_size
                );
            }
        } else {
            // No events, sleep briefly
            std::thread::sleep(Duration::from_millis(100));
        }

        // Print stats every 100 events
        if event_count > 0 && event_count % 100 == 0 {
            println!(
                "\n📊 Stats: {} events processed | Unread: {}\n",
                event_count,
                reader.unread_count()
            );
        }
    }
}
