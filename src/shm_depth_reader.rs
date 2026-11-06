// src/shm_depth_reader.rs - Order Book Depth Reader for OBI+VWMicro Pricing
use std::sync::atomic::{AtomicU32, Ordering, fence};

const NUM_SYMBOLS: usize = 2048;
const NUM_EXCHANGES: usize = 7;
const DEPTH_LEVELS: usize = 5;
const HEADER_SIZE: usize = NUM_SYMBOLS * std::mem::size_of::<u64>();
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

const _: () = {
    assert!(std::mem::size_of::<ShmDepthSnapshot>() == SLOT_SIZE);
    assert!(std::mem::offset_of!(ShmDepthSnapshot, timestamp_ns) == 16);
    assert!(std::mem::offset_of!(ShmDepthSnapshot, bids) == 24);
    assert!(std::mem::offset_of!(ShmDepthSnapshot, asks) == 104);
};

pub struct ShmDepthReader {
    _mmap: memmap2::Mmap,
    data: *const u8,
    max_symbols: usize,
}

impl ShmDepthReader {
    pub fn open(path: &str, num_symbols: usize) -> Result<Self, std::io::Error> {
        if !(1..=NUM_SYMBOLS).contains(&num_symbols) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid symbol count",
            ));
        }
        let file = std::fs::OpenOptions::new().read(true).open(path)?;

        let expected_size = HEADER_SIZE + num_symbols * NUM_EXCHANGES * SLOT_SIZE;

        // SAFETY: The producer must not truncate the file while readers are mapped.
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
            max_symbols: num_symbols,
        })
    }

    #[inline]
    fn slot_offset(symbol_id: u16, exchange_id: u8) -> usize {
        HEADER_SIZE + (symbol_id as usize * NUM_EXCHANGES + exchange_id as usize) * SLOT_SIZE
    }

    pub fn read_depth(&self, symbol_id: u16, exchange_id: u8) -> Option<ShmDepthSnapshot> {
        if symbol_id as usize >= self.max_symbols || exchange_id as usize >= NUM_EXCHANGES {
            return None;
        }
        let offset = Self::slot_offset(symbol_id, exchange_id);
        // SAFETY: open() validated the mapped extent; the bounds above keep this
        // slot inside it. mmap is page-aligned and slots are naturally aligned.
        let slot_ptr = unsafe { self.data.add(offset) as *const ShmDepthSnapshot };
        let seq_ptr = slot_ptr.cast::<AtomicU32>();
        for _ in 0..10_000 {
            // SAFETY: Seqlock is the aligned first field, published atomically by Go.
            let seq_before = unsafe { (*seq_ptr).load(Ordering::Acquire) };
            if seq_before & 1 != 0 {
                std::hint::spin_loop();
                continue;
            }
            // SAFETY: Copy out of external shared memory without caching its fields.
            let data = unsafe { std::ptr::read_volatile(slot_ptr) };
            fence(Ordering::Acquire);
            // SAFETY: Same valid seqlock pointer as above.
            let seq_after = unsafe { (*seq_ptr).load(Ordering::Acquire) };
            if seq_before == seq_after {
                return (data.timestamp_ns > 0).then_some(data);
            }
        }
        None
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

// SAFETY: mmap owns the backing storage; reads copy snapshots under the seqlock.
unsafe impl Send for ShmDepthReader {}
// SAFETY: Concurrent readers have no mutable local state or shared writes.
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
        assert_eq!(ShmDepthReader::slot_offset(0, 0), 16_384);
        assert_eq!(ShmDepthReader::slot_offset(0, 1), 16_384 + 256);
        assert_eq!(ShmDepthReader::slot_offset(1, 0), 16_384 + 7 * 256);
    }

    #[test]
    fn reads_producer_layout_and_rejects_out_of_bounds() {
        let mut mmap = memmap2::MmapMut::map_anon(HEADER_SIZE + NUM_EXCHANGES * SLOT_SIZE).unwrap();
        let snapshot = ShmDepthSnapshot {
            timestamp_ns: 123,
            exchange_id: 2,
            bids: [PriceLevel {
                price: 3000.0,
                size: 1.0,
            }; DEPTH_LEVELS],
            ..Default::default()
        };
        // SAFETY: The anonymous map has an aligned, fully allocated slot.
        unsafe {
            std::ptr::write(
                mmap.as_mut_ptr()
                    .add(ShmDepthReader::slot_offset(0, 2))
                    .cast(),
                snapshot,
            );
        }
        let mmap = mmap.make_read_only().unwrap();
        let reader = ShmDepthReader {
            data: mmap.as_ptr(),
            _mmap: mmap,
            max_symbols: 1,
        };
        assert_eq!(reader.read_depth(0, 2).unwrap().bids[0].price, 3000.0);
        assert!(reader.read_depth(1, 2).is_none());
        assert!(reader.read_depth(0, NUM_EXCHANGES as u8).is_none());
        assert!(reader.read_depth(u16::MAX, u8::MAX).is_none());
    }
}
