// src/shm_reader.rs - Lock-free Shared Matrix for HFT
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering, fence};

pub const NUM_SYMBOLS: usize = 2048;
pub const NUM_EXCHANGES: usize = 12; // 0=pad, 1=HL, 2=Lighter, 3..5=legacy, 6=Binance, 7=Arcus, 8=LighterRH, 9=Entropy, 10=TradeXYZ, 11=Extended
pub const SHM_MAGIC_V12: [u8; 16] = *b"ALEPHTX_SHM_V12\0";
const SLOT_SIZE: usize = 64;
const VERSION_SIZE: usize = 8;

#[repr(C, align(64))]
#[derive(Clone, Copy, Debug, Default)]
pub struct ShmBboMessage {
    pub seqlock: u32,
    pub msg_type: u8,
    pub exchange_id: u8,
    pub symbol_id: u16,
    pub timestamp_ns: u64,
    pub bid_price: f64,
    pub bid_size: f64,
    pub ask_price: f64,
    pub ask_size: f64,
    pub _reserved: [u8; 16],
}

const _: () = {
    assert!(std::mem::size_of::<ShmBboMessage>() == SLOT_SIZE);
    assert!(std::mem::align_of::<ShmBboMessage>() == 64);
    assert!(std::mem::offset_of!(ShmBboMessage, seqlock) == 0);
    assert!(std::mem::offset_of!(ShmBboMessage, msg_type) == 4);
    assert!(std::mem::offset_of!(ShmBboMessage, exchange_id) == 5);
    assert!(std::mem::offset_of!(ShmBboMessage, symbol_id) == 6);
    assert!(std::mem::offset_of!(ShmBboMessage, timestamp_ns) == 8);
    assert!(std::mem::offset_of!(ShmBboMessage, bid_price) == 16);
    assert!(std::mem::offset_of!(ShmBboMessage, bid_size) == 24);
    assert!(std::mem::offset_of!(ShmBboMessage, ask_price) == 32);
    assert!(std::mem::offset_of!(ShmBboMessage, ask_size) == 40);
    assert!(std::mem::offset_of!(ShmBboMessage, _reserved) == 48);
};

#[inline(always)]
#[deny(unsafe_op_in_unsafe_fn)]
unsafe fn load_bbo_payload_relaxed(slot_ptr: *const ShmBboMessage, seqlock: u32) -> ShmBboMessage {
    // SAFETY: `slot_ptr` points to a 64-byte-aligned `ShmBboMessage` within the validated
    // shared mapping. Offset 4 is u32-aligned and offsets 8..48 are u64-aligned. Per-word
    // Relaxed atomic loads prevent Rust/LLVM data-race UB when racing with the Go writer.
    unsafe {
        let w = (*slot_ptr.byte_add(4).cast::<AtomicU32>())
            .load(Ordering::Relaxed)
            .to_le();
        let timestamp_ns = (*slot_ptr.byte_add(8).cast::<AtomicU64>()).load(Ordering::Relaxed);
        let bid_price =
            f64::from_bits((*slot_ptr.byte_add(16).cast::<AtomicU64>()).load(Ordering::Relaxed));
        let bid_size =
            f64::from_bits((*slot_ptr.byte_add(24).cast::<AtomicU64>()).load(Ordering::Relaxed));
        let ask_price =
            f64::from_bits((*slot_ptr.byte_add(32).cast::<AtomicU64>()).load(Ordering::Relaxed));
        let ask_size =
            f64::from_bits((*slot_ptr.byte_add(40).cast::<AtomicU64>()).load(Ordering::Relaxed));

        ShmBboMessage {
            seqlock,
            msg_type: (w & 0xff) as u8,
            exchange_id: ((w >> 8) & 0xff) as u8,
            symbol_id: (w >> 16) as u16,
            timestamp_ns,
            bid_price,
            bid_size,
            ask_price,
            ask_size,
            _reserved: [0; 16],
        }
    }
}

pub struct ShmReader {
    // Must keep mmap alive - without it, data pointer is invalid!
    _mmap: memmap2::Mmap,
    data: *const u8,
    local_versions: [u64; NUM_SYMBOLS],
    max_symbols: usize,
    next_symbol: usize,
    torn_retries: u64,
    exhausted_spins: u64,
}

impl ShmReader {
    pub fn open(path: &str, num_symbols: usize) -> anyhow::Result<Self> {
        anyhow::ensure!(
            (1..=NUM_SYMBOLS).contains(&num_symbols),
            "invalid symbol count"
        );
        let file = std::fs::File::open(path)?;
        // SAFETY: The producer owns the file and must not truncate a live mapping.
        let mmap = unsafe { memmap2::Mmap::map(&file)? };
        let expected_size = NUM_SYMBOLS * VERSION_SIZE + num_symbols * NUM_EXCHANGES * SLOT_SIZE;
        anyhow::ensure!(
            mmap.len() >= expected_size,
            "BBO shared memory is truncated"
        );
        let magic_offset = NUM_SYMBOLS * VERSION_SIZE + 48;
        let magic = &mmap[magic_offset..magic_offset + 16];
        anyhow::ensure!(
            magic == SHM_MAGIC_V12,
            "BBO shared memory ABI magic mismatch"
        );

        let data = mmap.as_ptr();

        Ok(Self {
            _mmap: mmap,
            data,
            local_versions: [0u64; NUM_SYMBOLS],
            max_symbols: num_symbols,
            next_symbol: 0,
            torn_retries: 0,
            exhausted_spins: 0,
        })
    }

    #[inline(always)]
    fn load_version(&self, symbol_id: u16) -> u64 {
        if symbol_id as usize >= self.max_symbols {
            return 0;
        }
        let offset = (symbol_id as usize) * VERSION_SIZE;
        // SAFETY: Bounds were validated above; the mmap base and offsets are u64 aligned.
        unsafe {
            let ptr = self.data.add(offset).cast::<AtomicU64>();
            (*ptr).load(Ordering::Acquire)
        }
    }

    #[inline(always)]
    pub fn try_poll(&mut self) -> Option<u16> {
        for _ in 0..self.max_symbols {
            let sym = self.next_symbol;
            self.next_symbol = (sym + 1) % self.max_symbols;
            let sym_id = sym as u16;
            let version = self.load_version(sym_id);

            if version != self.local_versions[sym] {
                self.local_versions[sym] = version;
                return Some(sym_id);
            }
        }
        None
    }

    #[inline(always)]
    pub fn read_all_exchanges(&mut self, symbol_id: u16) -> [(u8, ShmBboMessage); NUM_EXCHANGES] {
        if symbol_id as usize >= self.max_symbols {
            return [(0, ShmBboMessage::default()); NUM_EXCHANGES];
        }
        let version = self.load_version(symbol_id);
        self.local_versions[symbol_id as usize] = version;

        let mut result = [(0u8, ShmBboMessage::default()); NUM_EXCHANGES];
        for (exch, item) in result.iter_mut().enumerate().take(NUM_EXCHANGES) {
            let base = NUM_SYMBOLS * VERSION_SIZE;
            let offset = base + (symbol_id as usize * NUM_EXCHANGES + exch) * SLOT_SIZE;
            // SAFETY: Bounds were checked above; mmap is page-aligned and slots are 64B-aligned.
            let slot_ptr = unsafe { self.data.add(offset).cast::<ShmBboMessage>() };
            let seq_ptr = slot_ptr.cast::<AtomicU32>();

            let mut msg;
            let mut spin_count: u32 = 0;
            const MAX_SPINS: u32 = 10_000;

            loop {
                // 1. Read Lock (Acquire)
                let seq1 = unsafe { (*seq_ptr).load(Ordering::Acquire) };
                if seq1 & 1 != 0 {
                    spin_count += 1;
                    if spin_count > MAX_SPINS {
                        self.exhausted_spins = self.exhausted_spins.saturating_add(1);
                        tracing::error!(
                            "Seqlock stuck (writer dead?): symbol={} exch={} seq={} after {} spins",
                            symbol_id,
                            exch,
                            seq1,
                            spin_count
                        );
                        // Return stale data rather than hang forever
                        msg = ShmBboMessage::default();
                        break;
                    }
                    std::hint::spin_loop();
                    continue; // Writer is active, wait
                }

                // 2. Copy payload via per-word Relaxed atomic loads
                msg = unsafe { load_bbo_payload_relaxed(slot_ptr, seq1) };

                // Order payload loads before the validation load on ARM as well as x86.
                fence(Ordering::Acquire);

                // 3. Validate lock
                let seq2 = unsafe { (*seq_ptr).load(Ordering::Acquire) };
                if seq1 == seq2 {
                    break; // Data is clean, break spin loop
                }

                self.torn_retries = self.torn_retries.saturating_add(1);
                spin_count += 1;
                if spin_count > MAX_SPINS {
                    self.exhausted_spins = self.exhausted_spins.saturating_add(1);
                    tracing::error!(
                        "Seqlock torn read limit: symbol={} exch={} after {} spins",
                        symbol_id,
                        exch,
                        spin_count
                    );
                    msg = ShmBboMessage::default();
                    break;
                }
                std::hint::spin_loop();
            }

            if msg.timestamp_ns != 0
                && (msg.msg_type != 1
                    || msg.exchange_id != exch as u8
                    || msg.symbol_id != symbol_id)
            {
                tracing::error!(
                    "SHM BBO slot identity mismatch: expected sym={} exch={} got msg_type={} sym={} exch={}",
                    symbol_id,
                    exch,
                    msg.msg_type,
                    msg.symbol_id,
                    msg.exchange_id
                );
                msg = ShmBboMessage::default();
            }

            *item = (exch as u8, msg);
        }
        result
    }

    pub fn local_version(&self, symbol_id: u16) -> u64 {
        self.local_versions
            .get(symbol_id as usize)
            .copied()
            .unwrap_or(0)
    }

    pub fn shared_version(&self, symbol_id: u16) -> u64 {
        self.load_version(symbol_id)
    }

    pub fn torn_retries(&self) -> u64 {
        self.torn_retries
    }

    pub fn exhausted_spins(&self) -> u64 {
        self.exhausted_spins
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Barrier};

    use super::*;

    fn reader(versions: &[u64]) -> ShmReader {
        let mut map = memmap2::MmapMut::map_anon(
            NUM_SYMBOLS * VERSION_SIZE + versions.len() * NUM_EXCHANGES * SLOT_SIZE,
        )
        .unwrap();
        for (i, value) in versions.iter().enumerate() {
            map[i * 8..i * 8 + 8].copy_from_slice(&value.to_ne_bytes());
        }
        let mmap = map.make_read_only().unwrap();
        ShmReader {
            data: mmap.as_ptr(),
            _mmap: mmap,
            local_versions: [0; NUM_SYMBOLS],
            max_symbols: versions.len(),
            next_symbol: 0,
            torn_retries: 0,
            exhausted_spins: 0,
        }
    }

    #[test]
    fn polling_is_fair_when_first_symbol_is_continuously_updated() {
        let mut reader = reader(&[1, 1]);
        assert_eq!(reader.try_poll(), Some(0));
        reader.local_versions[0] = 0; // symbol zero has another pending update
        assert_eq!(reader.try_poll(), Some(1));
        assert_eq!(reader.try_poll(), Some(0));
        assert_eq!(reader.try_poll(), None);
    }

    #[test]
    fn invalid_symbols_never_read_outside_mapping() {
        let mut reader = reader(&[1]);
        assert_eq!(reader.shared_version(u16::MAX), 0);
        assert_eq!(reader.local_version(u16::MAX), 0);
        assert_eq!(reader.read_all_exchanges(u16::MAX)[0].1.timestamp_ns, 0);
        assert_eq!(reader.shared_version(1), 0);
    }

    #[test]
    fn opening_truncated_matrix_fails() {
        let path = std::env::temp_dir().join(format!("aleph-bbo-{}", uuid::Uuid::new_v4()));
        std::fs::write(&path, [0u8; 64]).unwrap();
        assert!(ShmReader::open(path.to_str().unwrap(), 1).is_err());
        assert!(ShmReader::open(path.to_str().unwrap(), NUM_SYMBOLS + 1).is_err());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn rejects_mismatched_magic_and_misaddressed_slot() {
        let path = std::env::temp_dir().join(format!("aleph-bbo-magic-{}", uuid::Uuid::new_v4()));
        let mut buf = vec![0u8; NUM_SYMBOLS * VERSION_SIZE + 2 * NUM_EXCHANGES * SLOT_SIZE];
        // Zeroed/unstamped SHM files are rejected.
        std::fs::write(&path, &buf).unwrap();
        assert!(ShmReader::open(path.to_str().unwrap(), 2).is_err());

        let magic_offset = NUM_SYMBOLS * VERSION_SIZE + 48;
        buf[magic_offset..magic_offset + 16].copy_from_slice(b"ALEPHTX_SHM_V07\0");
        std::fs::write(&path, &buf).unwrap();
        assert!(ShmReader::open(path.to_str().unwrap(), 2).is_err());

        buf[magic_offset..magic_offset + 16].copy_from_slice(&SHM_MAGIC_V12);
        let slot_offset = NUM_SYMBOLS * VERSION_SIZE + (NUM_EXCHANGES + 2) * SLOT_SIZE;
        buf[slot_offset..slot_offset + 4].copy_from_slice(&2u32.to_le_bytes());
        buf[slot_offset + 4] = 1;
        buf[slot_offset + 5] = 6; // wrong exchange_id in slot 2
        buf[slot_offset + 6..slot_offset + 8].copy_from_slice(&1u16.to_le_bytes());
        buf[slot_offset + 8..slot_offset + 16].copy_from_slice(&999u64.to_le_bytes());
        std::fs::write(&path, &buf).unwrap();

        let mut r = ShmReader::open(path.to_str().unwrap(), 2).unwrap();
        assert_eq!(r.read_all_exchanges(1)[2].1.timestamp_ns, 0);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn concurrent_atomic_seqlock_writer_never_yields_torn_bbo() {
        const ITERATIONS: u64 = 20_000;
        const SYM: u16 = 1;
        const EXCH: usize = 2;

        let path = std::env::temp_dir().join(format!("aleph-bbo-stress-{}", uuid::Uuid::new_v4()));
        let total_len = NUM_SYMBOLS * VERSION_SIZE + 2 * NUM_EXCHANGES * SLOT_SIZE;
        let mut init_buf = vec![0u8; total_len];
        let magic_offset = NUM_SYMBOLS * VERSION_SIZE + 48;
        init_buf[magic_offset..magic_offset + 16].copy_from_slice(&SHM_MAGIC_V12);
        std::fs::write(&path, &init_buf).unwrap();

        let mut reader = ShmReader::open(path.to_str().unwrap(), 2).unwrap();
        let barrier = Arc::new(Barrier::new(2));
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
            let slot_offset =
                NUM_SYMBOLS * VERSION_SIZE + (SYM as usize * NUM_EXCHANGES + EXCH) * SLOT_SIZE;

            writer_barrier.wait();
            for step in 1..=ITERATIONS {
                let bid_price = 1000.0 + (step % 500) as f64;
                let bid_size = bid_price * 2.0;
                let ask_price = bid_price + 1.0;
                let ask_size = bid_price * 3.0;
                let ts_ns = step * 10;
                let header = 1u32 | ((EXCH as u32) << 8) | ((SYM as u32) << 16);

                // SAFETY: `base_ptr` is page-aligned and offsets match Go's `ShmBboMessage` ABI.
                unsafe {
                    let slot_ptr = base_ptr.add(slot_offset).cast::<ShmBboMessage>();
                    let seq_ptr = slot_ptr.cast::<AtomicU32>();
                    let prev = (*seq_ptr).fetch_add(1, Ordering::AcqRel);
                    fence(Ordering::Release);

                    (*slot_ptr.byte_add(4).cast::<AtomicU32>())
                        .store(header.to_le(), Ordering::Release);
                    (*slot_ptr.byte_add(8).cast::<AtomicU64>()).store(ts_ns, Ordering::Release);
                    (*slot_ptr.byte_add(16).cast::<AtomicU64>())
                        .store(bid_price.to_bits(), Ordering::Release);
                    (*slot_ptr.byte_add(24).cast::<AtomicU64>())
                        .store(bid_size.to_bits(), Ordering::Release);
                    (*slot_ptr.byte_add(32).cast::<AtomicU64>())
                        .store(ask_price.to_bits(), Ordering::Release);
                    (*slot_ptr.byte_add(40).cast::<AtomicU64>())
                        .store(ask_size.to_bits(), Ordering::Release);

                    (*seq_ptr).store(prev.wrapping_add(2), Ordering::Release);
                    let ver_ptr = base_ptr
                        .add(SYM as usize * VERSION_SIZE)
                        .cast::<AtomicU64>();
                    (*ver_ptr).fetch_add(1, Ordering::Release);
                }
            }
            writer_done.store(true, Ordering::Release);
            drop(mmap);
        });

        barrier.wait();
        let validate = |msg: &ShmBboMessage| {
            if msg.timestamp_ns == 0 {
                return;
            }
            assert_eq!(msg.msg_type, 1);
            assert_eq!(msg.exchange_id, EXCH as u8);
            assert_eq!(msg.symbol_id, SYM);
            assert_eq!(msg.ask_price, msg.bid_price + 1.0);
            assert_eq!(msg.bid_size, msg.bid_price * 2.0);
            assert_eq!(msg.ask_size, msg.bid_price * 3.0);
            let step = msg.timestamp_ns / 10;
            let expected_bid = 1000.0 + (step % 500) as f64;
            assert_eq!(msg.bid_price, expected_bid);
        };

        while !done.load(Ordering::Acquire) {
            let slots = reader.read_all_exchanges(SYM);
            validate(&slots[EXCH].1);
        }
        writer.join().unwrap();

        // A writer preempted mid-update can legitimately exhaust the reader's spin
        // budget while racing; only a quiescent read must never exhaust it.
        let spins_before_quiescent = reader.exhausted_spins();
        let final_slots = reader.read_all_exchanges(SYM);
        let final_msg = &final_slots[EXCH].1;
        validate(final_msg);
        assert_eq!(final_msg.timestamp_ns, ITERATIONS * 10);
        assert_eq!(reader.exhausted_spins(), spins_before_quiescent);
        let _ = reader.torn_retries();

        std::fs::remove_file(path).unwrap();
    }
}
