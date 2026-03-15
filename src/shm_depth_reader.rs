// src/shm_depth_reader.rs - Order Book Depth Reader for OBI+VWMicro Pricing
use std::sync::atomic::{Ordering, compiler_fence};

const NUM_SYMBOLS: usize = 2048;
const NUM_EXCHANGES: usize = 6;
const DEPTH_LEVELS: usize = 5;
const SLOT_SIZE: usize = 256; // 256 bytes per snapshot

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct PriceLevel {
    pub price: f64,
    pub size: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ShmDepthSnapshot {
    pub seqlock: u32,                     // 4 bytes
    pub exchange_id: u8,                  // 1 byte
    pub symbol_id: u16,                   // 2 bytes
    pub _padding1: u8,                    // 1 byte
    pub timestamp_ns: u64,                // 8 bytes (total: 16 bytes)
    pub bids: [PriceLevel; DEPTH_LEVELS], // 5 * 16 = 80 bytes (total: 96 bytes)
    pub asks: [PriceLevel; DEPTH_LEVELS], // 5 * 16 = 80 bytes (total: 176 bytes)
    pub _reserved: [u8; 72],              // 72 bytes padding (total: 248 bytes + 8 alignment = 256)
}

impl Default for ShmDepthSnapshot {
    fn default() -> Self {
        Self {
            seqlock: 0,
            exchange_id: 0,
            symbol_id: 0,
            _padding1: 0,
            timestamp_ns: 0,
            bids: [PriceLevel::default(); DEPTH_LEVELS],
            asks: [PriceLevel::default(); DEPTH_LEVELS],
            _reserved: [0; 72],
        }
    }
}

// Size check moved to test
// const _: () = assert!(std::mem::size_of::<ShmDepthSnapshot>() == SLOT_SIZE);

pub struct ShmDepthReader {
    _mmap: memmap2::Mmap,
    data: *const u8,
    #[allow(dead_code)]
    local_versions: [u64; NUM_SYMBOLS],
}

impl ShmDepthReader {
    pub fn open(path: &str, num_symbols: usize) -> Result<Self, std::io::Error> {
        let file = std::fs::OpenOptions::new().read(true).open(path)?;

        let expected_size = 8 + num_symbols * NUM_EXCHANGES * SLOT_SIZE;

        let mmap = unsafe { memmap2::MmapOptions::new().map(&file)? };

        if mmap.len() < expected_size {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("SHM too small: {} < {}", mmap.len(), expected_size),
            ));
        }

        Ok(Self {
            data: mmap.as_ptr(),
            _mmap: mmap,
            local_versions: [0; NUM_SYMBOLS],
        })
    }

    #[inline]
    fn slot_offset(&self, symbol_id: u16, exchange_id: u8) -> usize {
        8 + (symbol_id as usize * NUM_EXCHANGES + exchange_id as usize) * SLOT_SIZE
    }

    pub fn read_depth(&self, symbol_id: u16, exchange_id: u8) -> Option<ShmDepthSnapshot> {
        let offset = self.slot_offset(symbol_id, exchange_id);
        let slot_ptr = unsafe { self.data.add(offset) as *const ShmDepthSnapshot };

        unsafe {
            let snapshot = &*slot_ptr;

            let mut spin_count: u32 = 0;
            const MAX_SPINS: u32 = 10_000;

            // Seqlock read protocol
            loop {
                let seq_before = snapshot.seqlock;
                compiler_fence(Ordering::Acquire);

                if seq_before & 1 != 0 {
                    spin_count += 1;
                    if spin_count > MAX_SPINS {
                        tracing::error!("Seqlock stuck in depth_reader: seq={} after {} spins", seq_before, spin_count);
                        return None;
                    }
                    std::hint::spin_loop();
                    continue;
                }

                let data = *snapshot;
                compiler_fence(Ordering::Acquire);
                let seq_after = snapshot.seqlock;

                if seq_before == seq_after {
                    return if data.timestamp_ns > 0 {
                        Some(data)
                    } else {
                        None
                    };
                }

                spin_count += 1;
                if spin_count > MAX_SPINS {
                    tracing::error!("Seqlock torn read in depth_reader: before={} after={} after {} spins", seq_before, seq_after, spin_count);
                    return None;
                }
            }
        }
    }

    pub fn read_all_exchanges(&self, symbol_id: u16) -> Vec<(u8, ShmDepthSnapshot)> {
        (0..NUM_EXCHANGES as u8)
            .filter_map(|exch_id| {
                self.read_depth(symbol_id, exch_id)
                    .map(|snapshot| (exch_id, snapshot))
            })
            .collect()
    }
}

unsafe impl Send for ShmDepthReader {}
unsafe impl Sync for ShmDepthReader {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_depth_snapshot_size() {
        let actual_size = std::mem::size_of::<ShmDepthSnapshot>();
        println!("ShmDepthSnapshot size: {} bytes", actual_size);
        println!(
            "PriceLevel size: {} bytes",
            std::mem::size_of::<PriceLevel>()
        );

        // With align(256), the struct will be padded to 256 bytes
        assert_eq!(
            actual_size, 256,
            "ShmDepthSnapshot must be exactly 256 bytes"
        );
        assert_eq!(std::mem::size_of::<PriceLevel>(), 16);
    }

    #[test]
    fn test_slot_offset_calculation() {
        let reader = ShmDepthReader {
            _mmap: unsafe { std::mem::zeroed() },
            data: std::ptr::null(),
            local_versions: [0; NUM_SYMBOLS],
        };

        assert_eq!(reader.slot_offset(0, 0), 8);
        assert_eq!(reader.slot_offset(0, 1), 8 + 256);
        assert_eq!(reader.slot_offset(1, 0), 8 + 6 * 256);
    }
}
