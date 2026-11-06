// src/shm_reader.rs - Lock-free Shared Matrix for HFT
use std::sync::atomic::{Ordering, fence};

pub const NUM_SYMBOLS: usize = 2048;
pub const NUM_EXCHANGES: usize = 7; // Padding, HL, Lighter, EdgeX, 01, Backpack, Binance
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

const _: () = assert!(std::mem::size_of::<ShmBboMessage>() == SLOT_SIZE);

pub struct ShmReader {
    // Must keep mmap alive - without it, data pointer is invalid!
    _mmap: memmap2::Mmap,
    data: *const u8,
    local_versions: [u64; NUM_SYMBOLS],
    max_symbols: usize,
    next_symbol: usize,
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

        let data = mmap.as_ptr();

        Ok(Self {
            _mmap: mmap,
            data,
            local_versions: [0u64; NUM_SYMBOLS],
            max_symbols: num_symbols,
            next_symbol: 0,
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
            let ptr = self.data.add(offset) as *const std::sync::atomic::AtomicU64;
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
            let ptr = unsafe { self.data.add(offset) };
            let seq_ptr = ptr as *const std::sync::atomic::AtomicU32;

            let mut msg;
            let mut spin_count: u32 = 0;
            const MAX_SPINS: u32 = 10_000;

            loop {
                // 1. Read Lock (Acquire)
                let seq1 = unsafe { (*seq_ptr).load(Ordering::Acquire) };
                if seq1 & 1 != 0 {
                    spin_count += 1;
                    if spin_count > MAX_SPINS {
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

                // 2. Copy payload
                msg = unsafe { core::ptr::read_volatile(ptr as *const ShmBboMessage) };

                // Order payload loads before the validation load on ARM as well as x86.
                fence(Ordering::Acquire);

                // 3. Validate lock
                let seq2 = unsafe { (*seq_ptr).load(Ordering::Acquire) };
                if seq1 == seq2 {
                    break; // Data is clean, break spin loop
                }

                spin_count += 1;
                if spin_count > MAX_SPINS {
                    tracing::error!(
                        "Seqlock torn read limit: symbol={} exch={} after {} spins",
                        symbol_id,
                        exch,
                        spin_count
                    );
                    msg = ShmBboMessage::default();
                    break;
                }
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
}

#[cfg(test)]
mod tests {
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
}
