//! Data Plane Thread - Dedicated SHM polling thread decoupled from Tokio runtime
//!
//! Solves the async starvation problem where SHM spin-loop monopolizes Tokio workers.
//! Uses a dedicated OS thread with optional CPU pinning + flume channel for async bridge.

use crate::shm_reader::{ShmBboMessage, ShmReader};
use flume::{bounded, Receiver, Sender};
use std::thread;
use tracing::{error, info};

/// BBO update message sent from data plane to strategy loop
#[derive(Debug, Clone)]
pub struct BboUpdate {
    pub symbol_id: u16,
    pub exchange_id: u8,
    pub bbo: ShmBboMessage,
}

/// Spawn a dedicated data plane thread for SHM polling
///
/// # Arguments
/// * `shm_path` - Path to shared memory file (e.g., "/dev/shm/aleph-matrix")
/// * `max_symbols` - Maximum number of symbols in SHM matrix
/// * `cpu_core` - Optional CPU core ID for thread pinning (e.g., Some(2))
///
/// # Returns
/// Receiver channel for async consumption in Tokio runtime
pub fn spawn_data_plane_thread(
    shm_path: &str,
    max_symbols: usize,
    cpu_core: Option<usize>,
) -> Receiver<BboUpdate> {
    let (tx, rx) = bounded(1024);
    let shm_path = shm_path.to_string();

    thread::Builder::new()
        .name("data-plane".to_string())
        .spawn(move || {
            data_plane_loop(shm_path, max_symbols, cpu_core, tx);
        })
        .expect("Failed to spawn data plane thread");

    rx
}

/// Main data plane loop (runs in dedicated OS thread)
fn data_plane_loop(
    shm_path: String,
    max_symbols: usize,
    cpu_core: Option<usize>,
    tx: Sender<BboUpdate>,
) {
    // Pin to CPU core if specified
    if let Some(core) = cpu_core {
        if let Some(core_id) = (core_affinity::CoreId { id: core }).into() {
            if core_affinity::set_for_current(core_id) {
                info!("📌 Data plane pinned to CPU core {}", core);
            } else {
                error!("⚠️ Failed to pin data plane to CPU core {}", core);
            }
        }
    }

    // Open SHM reader
    let mut reader = match ShmReader::open(&shm_path, max_symbols) {
        Ok(r) => {
            info!("✅ Data plane SHM reader opened: {}", shm_path);
            r
        }
        Err(e) => {
            error!("❌ Failed to open SHM reader: {}", e);
            return;
        }
    };

    info!("🚀 Data plane thread started (spin-loop mode)");

    // Spin-loop: poll SHM and send updates via channel
    loop {
        if let Some(symbol_id) = reader.try_poll() {
            // Read all exchanges for this symbol
            let exchanges = reader.read_all_exchanges(symbol_id);
            for (exch_idx, bbo) in exchanges.iter() {
                if bbo.bid_price > 0.0 && bbo.ask_price > 0.0 {
                    let update = BboUpdate {
                        symbol_id,
                        exchange_id: *exch_idx,
                        bbo: *bbo,
                    };

                    // Non-blocking send (drop if channel full to avoid backpressure)
                    if tx.try_send(update).is_err() {
                        // Channel full or disconnected - strategy loop is slow or dead
                        // In production, consider metrics here
                    }
                }
            }
        } else {
            // No updates available - yield CPU briefly
            std::hint::spin_loop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bbo_update_clone() {
        let bbo = ShmBboMessage {
            seqlock: 0,
            msg_type: 1,
            exchange_id: 2,
            symbol_id: 1002,
            timestamp_ns: 1234567890,
            bid_price: 3000.0,
            bid_size: 1.5,
            ask_price: 3001.0,
            ask_size: 2.0,
            _reserved: [0; 16],
        };

        let update = BboUpdate {
            symbol_id: 1002,
            exchange_id: 2,
            bbo: bbo.clone(),
        };

        let cloned = update.clone();
        assert_eq!(cloned.symbol_id, 1002);
        assert_eq!(cloned.exchange_id, 2);
        assert_eq!(cloned.bbo.bid_price, 3000.0);
    }
}
