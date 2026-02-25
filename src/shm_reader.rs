// src/shm_reader.rs - Lock-free Shared Matrix for HFT
use std::sync::atomic::{compiler_fence, Ordering};
use std::ptr;

const NUM_SYMBOLS: usize = 2048;
const NUM_EXCHANGES: usize = 5;
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
}

impl ShmReader {
    pub fn open(path: &str, num_symbols: usize) -> anyhow::Result<Self> {
        let file = std::fs::File::open(path)?;
        let mmap = unsafe { memmap2::Mmap::map(&file)? };
        
        
        let data = mmap.as_ptr();
        
        Ok(Self {
            _mmap: mmap,
            data,
            local_versions: [0u64; NUM_SYMBOLS],
            max_symbols: num_symbols.min(NUM_SYMBOLS),
        })
    }

    #[inline(always)]
    fn load_version(&self, symbol_id: u16) -> u64 {
        let offset = (symbol_id as usize) * VERSION_SIZE;
        unsafe {
            let ptr = self.data.add(offset);
            ptr::read_unaligned(ptr as *const u64)
        }
    }

    #[inline(always)]
    pub fn try_poll(&mut self) -> Option<u16> {
        for sym in 0..self.max_symbols {
            let version = self.load_version(sym as u16);
            
            if version > self.local_versions[sym] {
                self.local_versions[sym] = version;
                return Some(sym as u16);
            }
        }
        None
    }

    #[inline(always)]
    pub fn read_all_exchanges(&mut self, symbol_id: u16) -> [(u8, ShmBboMessage); NUM_EXCHANGES] {
        let version = self.load_version(symbol_id);
        self.local_versions[symbol_id as usize] = version;
        
        let mut result = [(0u8, ShmBboMessage::default()); NUM_EXCHANGES];
        for exch in 0..NUM_EXCHANGES {
            let base = NUM_SYMBOLS * VERSION_SIZE;
            let offset = base + (symbol_id as usize * NUM_EXCHANGES + exch) * SLOT_SIZE;
            let ptr = unsafe { self.data.add(offset) };
            let msg = unsafe { ptr::read_unaligned(ptr as *const ShmBboMessage) };
            result[exch] = (exch as u8, msg);
        }
        result
    }

    pub fn local_version(&self, symbol_id: u16) -> u64 {
        self.local_versions[symbol_id as usize]
    }

    pub fn shared_version(&self, symbol_id: u16) -> u64 {
        self.load_version(symbol_id)
    }
}
