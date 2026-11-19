// src/shm_depth_reader.rs - Order Book Depth Reader for OBI+VWMicro Pricing
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering, fence};

const NUM_SYMBOLS: usize = 2048;
const NUM_EXCHANGES: usize = 12;
const DEPTH_LEVELS: usize = 5;
const HEADER_SIZE: usize = NUM_SYMBOLS * std::mem::size_of::<u64>();
const SLOT_SIZE: usize = 256; // 256 bytes per snapshot
pub const DEPTH_MAGIC_V12: [u8; 16] = *b"ALEPHTX_DEP_V12\0";

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct PriceLevel {
    pub price: f64,
    pub size: f64,
}

#[repr(C, align(64))]
#[derive(Clone, Copy, Debug)]
pub struct ShmDepthSnapshot {
    pub seqlock: u32,                     // 0..4
    pub exchange_id: u8,                  // 4..5 (1 implicit padding byte `_padding0` at 5..6)
    pub symbol_id: u16,                   // 6..8
    pub _padding1: u8,                    // 8..9 (7 implicit padding bytes at 9..16 to align u64)
    pub timestamp_ns: u64,                // 16..24
    pub bids: [PriceLevel; DEPTH_LEVELS], // 24..104 (5 * 16 = 80 bytes)
    pub asks: [PriceLevel; DEPTH_LEVELS], // 104..184 (5 * 16 = 80 bytes)
    pub _reserved: [u8; 72],              // 184..256 (72 bytes; total 256 bytes)
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
    assert!(std::mem::size_of::<PriceLevel>() == 16);
    assert!(std::mem::align_of::<PriceLevel>() == 8);
    assert!(std::mem::size_of::<ShmDepthSnapshot>() == SLOT_SIZE);
    assert!(std::mem::align_of::<ShmDepthSnapshot>() == 64);
    assert!(std::mem::offset_of!(ShmDepthSnapshot, seqlock) == 0);
    assert!(std::mem::offset_of!(ShmDepthSnapshot, exchange_id) == 4);
    assert!(std::mem::offset_of!(ShmDepthSnapshot, symbol_id) == 6);
    assert!(std::mem::offset_of!(ShmDepthSnapshot, _padding1) == 8);
    assert!(std::mem::offset_of!(ShmDepthSnapshot, timestamp_ns) == 16);
    assert!(std::mem::offset_of!(ShmDepthSnapshot, bids) == 24);
    assert!(std::mem::offset_of!(ShmDepthSnapshot, asks) == 104);
    assert!(std::mem::offset_of!(ShmDepthSnapshot, _reserved) == 184);
};

#[inline(always)]
#[deny(unsafe_op_in_unsafe_fn)]
unsafe fn load_depth_payload_relaxed(
    slot_ptr: *const ShmDepthSnapshot,
    seqlock: u32,
) -> ShmDepthSnapshot {
    // SAFETY: `slot_ptr` points to a 64-byte-aligned `ShmDepthSnapshot` within the validated
    // shared mapping. Offset 4 is u32-aligned, and offsets 16..184 are u64-aligned. Per-word
    // Relaxed atomic loads avoid Rust/LLVM data-race UB when racing with the Go writer.
    unsafe {
        let header = (*slot_ptr.byte_add(4).cast::<AtomicU32>())
            .load(Ordering::Relaxed)
            .to_le();
        let timestamp_ns = (*slot_ptr.byte_add(16).cast::<AtomicU64>()).load(Ordering::Relaxed);

        let mut bids = [PriceLevel::default(); DEPTH_LEVELS];
        for (i, lvl) in bids.iter_mut().enumerate() {
            let lvl_offset = 24 + i * 16;
            let price = f64::from_bits(
                (*slot_ptr.byte_add(lvl_offset).cast::<AtomicU64>()).load(Ordering::Relaxed),
            );
            let size = f64::from_bits(
                (*slot_ptr.byte_add(lvl_offset + 8).cast::<AtomicU64>()).load(Ordering::Relaxed),
            );
            *lvl = PriceLevel { price, size };
        }

        let mut asks = [PriceLevel::default(); DEPTH_LEVELS];
        for (i, lvl) in asks.iter_mut().enumerate() {
            let lvl_offset = 104 + i * 16;
            let price = f64::from_bits(
                (*slot_ptr.byte_add(lvl_offset).cast::<AtomicU64>()).load(Ordering::Relaxed),
            );
            let size = f64::from_bits(
                (*slot_ptr.byte_add(lvl_offset + 8).cast::<AtomicU64>()).load(Ordering::Relaxed),
            );
            *lvl = PriceLevel { price, size };
        }

        ShmDepthSnapshot {
            seqlock,
            exchange_id: (header & 0xff) as u8,
            symbol_id: (header >> 16) as u16,
            _padding1: 0,
            timestamp_ns,
            bids,
            asks,
            _reserved: [0; 72],
        }
    }
}

pub struct ShmDepthReader {
    _mmap: memmap2::Mmap,
    data: *const u8,
    max_symbols: usize,
    torn_retries: AtomicU64,
    exhausted_spins: AtomicU64,
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
        let magic_offset = HEADER_SIZE + std::mem::offset_of!(ShmDepthSnapshot, _reserved);
        let magic = &mmap[magic_offset..magic_offset + 16];
        if magic != DEPTH_MAGIC_V12 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Depth shared memory ABI magic mismatch",
            ));
        }

        Ok(Self {
            data: mmap.as_ptr(),
            _mmap: mmap,
            max_symbols: num_symbols,
            torn_retries: AtomicU64::new(0),
            exhausted_spins: AtomicU64::new(0),
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
        let slot_ptr = unsafe { self.data.add(offset).cast::<ShmDepthSnapshot>() };
        let seq_ptr = slot_ptr.cast::<AtomicU32>();
        const MAX_SPINS: u32 = 10_000;
        for _ in 0..MAX_SPINS {
            // SAFETY: Seqlock is the aligned first field, published atomically by Go.
            let seq_before = unsafe { (*seq_ptr).load(Ordering::Acquire) };
            if seq_before & 1 != 0 {
                std::hint::spin_loop();
                continue;
            }
            // SAFETY: Per-word Relaxed atomic loads copy out of shared memory without data-race UB.
            let data = unsafe { load_depth_payload_relaxed(slot_ptr, seq_before) };
            fence(Ordering::Acquire);
            // SAFETY: Same valid seqlock pointer as above.
            let seq_after = unsafe { (*seq_ptr).load(Ordering::Acquire) };
            if seq_before == seq_after {
                if data.timestamp_ns == 0
                    || data.exchange_id != exchange_id
                    || data.symbol_id != symbol_id
                {
                    return None;
                }
                return Some(data);
            }
            self.torn_retries.fetch_add(1, Ordering::Relaxed);
            std::hint::spin_loop();
        }
        self.exhausted_spins.fetch_add(1, Ordering::Relaxed);
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

    pub fn torn_retries(&self) -> u64 {
        self.torn_retries.load(Ordering::Relaxed)
    }

    pub fn exhausted_spins(&self) -> u64 {
        self.exhausted_spins.load(Ordering::Relaxed)
    }
}

// SAFETY: mmap owns the backing storage; reads copy snapshots under the seqlock.
unsafe impl Send for ShmDepthReader {}
// SAFETY: Concurrent readers only perform atomic loads on mmap and Relaxed AtomicU64 updates.
unsafe impl Sync for ShmDepthReader {}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Barrier};

    use super::*;

    #[test]
    fn test_depth_snapshot_size() {
        let actual_size = std::mem::size_of::<ShmDepthSnapshot>();
        println!("ShmDepthSnapshot size: {} bytes", actual_size);
        println!(
            "PriceLevel size: {} bytes",
            std::mem::size_of::<PriceLevel>()
        );

        // 256-byte snapshot (4 * 64-byte cache lines)
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
        assert_eq!(
            ShmDepthReader::slot_offset(1, 0),
            16_384 + NUM_EXCHANGES * 256
        );
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
            torn_retries: AtomicU64::new(0),
            exhausted_spins: AtomicU64::new(0),
        };
        assert_eq!(reader.read_depth(0, 2).unwrap().bids[0].price, 3000.0);
        assert!(reader.read_depth(1, 2).is_none());
        assert!(reader.read_depth(0, NUM_EXCHANGES as u8).is_none());
        assert!(reader.read_depth(u16::MAX, u8::MAX).is_none());
    }

    #[test]
    fn open_requires_v12_depth_magic() {
        let path = std::env::temp_dir().join(format!("aleph-dep-magic-{}", uuid::Uuid::new_v4()));
        let mut buf = vec![0u8; HEADER_SIZE + NUM_EXCHANGES * SLOT_SIZE];
        std::fs::write(&path, &buf).unwrap();
        assert!(ShmDepthReader::open(path.to_str().unwrap(), 1).is_err());

        let magic_offset = HEADER_SIZE + std::mem::offset_of!(ShmDepthSnapshot, _reserved);
        buf[magic_offset..magic_offset + 16].copy_from_slice(b"ALEPHTX_DEP_V07\0");
        std::fs::write(&path, &buf).unwrap();
        assert!(ShmDepthReader::open(path.to_str().unwrap(), 1).is_err());

        buf[magic_offset..magic_offset + 16].copy_from_slice(&DEPTH_MAGIC_V12);
        std::fs::write(&path, &buf).unwrap();
        assert!(ShmDepthReader::open(path.to_str().unwrap(), 1).is_ok());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn concurrent_atomic_seqlock_writer_never_yields_torn_depth_levels() {
        const ITERATIONS: u64 = 20_000;
        const SYM: u16 = 1;
        const EXCH: u8 = 2;
        const NUM_READERS: usize = 2;

        let path = std::env::temp_dir().join(format!("aleph-dep-stress-{}", uuid::Uuid::new_v4()));
        let total_len = HEADER_SIZE + 2 * NUM_EXCHANGES * SLOT_SIZE;
        let mut init_buf = vec![0u8; total_len];
        let magic_offset = HEADER_SIZE + std::mem::offset_of!(ShmDepthSnapshot, _reserved);
        init_buf[magic_offset..magic_offset + 16].copy_from_slice(&DEPTH_MAGIC_V12);
        std::fs::write(&path, &init_buf).unwrap();

        let reader = Arc::new(ShmDepthReader::open(path.to_str().unwrap(), 2).unwrap());
        let barrier = Arc::new(Barrier::new(NUM_READERS + 1));
        let done = Arc::new(AtomicBool::new(false));

        let writer_path = path.clone();
        let writer_barrier = Arc::clone(&barrier);
        let writer_done = Arc::clone(&done);
        let writer = std::thread::spawn(move || {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&writer_path)
                .unwrap();
            // SAFETY: Pre-sized backing file is mapped exclusively by this writer thread.
            let mut mmap = unsafe { memmap2::MmapMut::map_mut(&file).unwrap() };
            let base_ptr = mmap.as_mut_ptr();
            let slot_offset = ShmDepthReader::slot_offset(SYM, EXCH);

            writer_barrier.wait();
            for step in 1..=ITERATIONS {
                let active = if step.is_multiple_of(4) {
                    ((step as usize / 4) % DEPTH_LEVELS) + 1
                } else {
                    DEPTH_LEVELS
                };
                let base = 3000.0 + (step % 500) as f64;
                let ts_ns = step * 10;
                let header = (EXCH as u32) | ((SYM as u32) << 16);

                // SAFETY: `base_ptr` is page-aligned and offsets match Go's `ShmDepthSnapshot` ABI.
                unsafe {
                    let slot_ptr = base_ptr.add(slot_offset).cast::<ShmDepthSnapshot>();
                    let seq_ptr = slot_ptr.cast::<AtomicU32>();
                    let prev = (*seq_ptr).fetch_add(1, Ordering::AcqRel);
                    fence(Ordering::Release);

                    (*slot_ptr.byte_add(4).cast::<AtomicU32>())
                        .store(header.to_le(), Ordering::Release);
                    (*slot_ptr.byte_add(8).cast::<AtomicU64>()).store(0, Ordering::Release);
                    (*slot_ptr.byte_add(16).cast::<AtomicU64>()).store(ts_ns, Ordering::Release);

                    for lvl in 0..DEPTH_LEVELS {
                        let (bid_px, bid_sz, ask_px, ask_sz) = if lvl < active {
                            (
                                base - lvl as f64,
                                base * 2.0 + (lvl + 1) as f64,
                                base + 1.0 + lvl as f64,
                                base * 3.0 + (lvl + 1) as f64,
                            )
                        } else {
                            (0.0, 0.0, 0.0, 0.0)
                        };
                        let bid_off = 24 + lvl * 16;
                        let ask_off = 104 + lvl * 16;
                        (*slot_ptr.byte_add(bid_off).cast::<AtomicU64>())
                            .store(bid_px.to_bits(), Ordering::Release);
                        (*slot_ptr.byte_add(bid_off + 8).cast::<AtomicU64>())
                            .store(bid_sz.to_bits(), Ordering::Release);
                        (*slot_ptr.byte_add(ask_off).cast::<AtomicU64>())
                            .store(ask_px.to_bits(), Ordering::Release);
                        (*slot_ptr.byte_add(ask_off + 8).cast::<AtomicU64>())
                            .store(ask_sz.to_bits(), Ordering::Release);
                    }

                    (*seq_ptr).store(prev.wrapping_add(2), Ordering::Release);
                    let ver_ptr = base_ptr.add(SYM as usize * 8).cast::<AtomicU64>();
                    (*ver_ptr).fetch_add(1, Ordering::Release);
                }
            }
            writer_done.store(true, Ordering::Release);
            drop(mmap);
        });

        let validate = |snap: &ShmDepthSnapshot| {
            assert_eq!(snap.exchange_id, EXCH);
            assert_eq!(snap.symbol_id, SYM);
            assert_ne!(snap.timestamp_ns, 0);
            assert_eq!(snap.timestamp_ns % 10, 0);
            let step = snap.timestamp_ns / 10;
            let active = if step.is_multiple_of(4) {
                ((step as usize / 4) % DEPTH_LEVELS) + 1
            } else {
                DEPTH_LEVELS
            };
            let base = 3000.0 + (step % 500) as f64;
            for lvl in 0..DEPTH_LEVELS {
                let (want_bid_px, want_bid_sz, want_ask_px, want_ask_sz) = if lvl < active {
                    (
                        base - lvl as f64,
                        base * 2.0 + (lvl + 1) as f64,
                        base + 1.0 + lvl as f64,
                        base * 3.0 + (lvl + 1) as f64,
                    )
                } else {
                    (0.0, 0.0, 0.0, 0.0)
                };
                assert_eq!(snap.bids[lvl].price, want_bid_px, "torn bid price L{}", lvl);
                assert_eq!(snap.bids[lvl].size, want_bid_sz, "torn bid size L{}", lvl);
                assert_eq!(snap.asks[lvl].price, want_ask_px, "torn ask price L{}", lvl);
                assert_eq!(snap.asks[lvl].size, want_ask_sz, "torn ask size L{}", lvl);
            }
        };

        let mut reader_handles = Vec::with_capacity(NUM_READERS);
        for _ in 0..NUM_READERS {
            let r = Arc::clone(&reader);
            let b = Arc::clone(&barrier);
            let d = Arc::clone(&done);
            reader_handles.push(std::thread::spawn(move || {
                b.wait();
                let mut valid_reads = 0u64;
                while !d.load(Ordering::Acquire) {
                    if let Some(snap) = r.read_depth(SYM, EXCH) {
                        validate(&snap);
                        valid_reads += 1;
                    }
                }
                valid_reads
            }));
        }

        writer.join().unwrap();
        let mut total_valid_reads = 0u64;
        for handle in reader_handles {
            total_valid_reads += handle.join().unwrap();
        }

        let spins_before_quiescent = reader.exhausted_spins();
        let final_snap = reader.read_depth(SYM, EXCH).unwrap();
        validate(&final_snap);
        assert_eq!(final_snap.timestamp_ns, ITERATIONS * 10);
        assert_eq!(reader.exhausted_spins(), spins_before_quiescent);
        let _ = (total_valid_reads, reader.torn_retries());

        std::fs::remove_file(path).unwrap();
    }
}
