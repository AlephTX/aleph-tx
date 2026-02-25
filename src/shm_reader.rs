// src/shm_reader.rs
//! Lock-free Shared Matrix Architecture for AlephTX HFT
//!
//! Instead of a seqlock ring buffer (which queues updates), we use a
//! version-based "latest-state-only" approach. The Go feeder writes directly
//! to a flat shared memory struct. The Rust core spins on version numbers
//! and only reads when a symbol actually changed.
//!
//! Memory layout (single mmap, cache-line friendly):
//!   - symbol_versions[2048]: AtomicU64 per symbol (16 KB, fits in L1d)
//bo_matrix[204!   - b8][5]: ShmBboMessage payload (64B × 5 × 2048 = 640 KB)

use std::sync::atomic::{compiler_fence, AtomicU32, AtomicU64, Ordering};
use std::ptr;

/// 64-byte BBO message (matches Go exactly).
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

const SLOT_SIZE: usize = 64;
const NUM_SYMBOLS: usize = 2048;
const NUM_EXCHANGES: usize = 5;

// Compile-time assertions
const _: () = assert!(std::mem::size_of::<ShmBboMessage>() == SLOT_SIZE);
const _: () = assert!(std::mem::align_of::<ShmBboMessage>() == 64);

/// The single flat shared memory structure.
/// Layout matches Go's ShmMarketState exactly.
#[repr(C)]
pub struct ShmMarketState {
    /// Version counter per symbol. Incremented by Go feeder on each write.
    /// Rust spins on these; if versions[sym] > local_versions[sym],
    /// the symbol has new data. 16 KB (2048 × 8 bytes), fits in L1d.
    pub symbol_versions: [AtomicU64; NUM_SYMBOLS],
    /// BBO matrix: [symbol_id][exchange_id] → ShmBboMessage
    /// Total: 640 KB (2048 × 5 × 64 bytes)
    pub bbo_matrix: [[ShmBboMessage; NUM_EXCHANGES]; NUM_SYMBOLS],
}

/// Local version tracker — one u64 per symbol.
#[repr(align(64))]
#[derive(Clone, Debug)]
pub struct LocalVersions(pub [u64; NUM_SYMBOLS]);

impl Default for LocalVersions {
    fn default() -> Self {
        Self([0u64; NUM_SYMBOLS])
    }
}

/// Shared memory reader with version-based notification.
///
/// Hot path: spin on symbol_versions, O(1) array lookup, only reads
/// when version increased. Zero allocations, zero queueing.
pub struct ShmReader {
    /// The memory-mapped shared state (written by Go feeder).
    shm: *const ShmMarketState,
    /// Local copy of versions — updated after reading each symbol.
    local_versions: LocalVersions,
    /// Number of symbols to scan (for diagnostics).
    max_symbols: usize,
}

impl ShmReader {
    /// Memory-map the shared state file.
    pub fn open(path: &str, num_symbols: usize) -> anyhow::Result<Self> {
        let file = std::fs::File::open(path)?;
        let mmap = unsafe { memmap2::Mmap::map(&file)? };
        
        let expected_size = std::mem::size_of::<ShmMarketState>();
        assert!(
            mmap.len() >= expected_size,
            "mmap too small: {} < {}",
            mmap.len(),
            expected_size
        );

        let shm = mmap.as_ptr() as *const ShmMarketState;
        
        Ok(Self {
            shm,
            local_versions: LocalVersions::default(),
            max_symbols: num_symbols.min(NUM_SYMBOLS),
        })
    }

    /// Load version for a symbol directly from shared memory.
    #[inline(always)]
    fn load_version(&self, symbol_id: u16) -> u64 {
        let sym = symbol_id as usize;
        // Direct array access — bounds check at runtime
        unsafe {
            (*self.shm).symbol_versions[sym].load(Ordering::Acquire)
        }
    }

    /// Read a single BBO message with strict seqlock protocol.
    /// Called only when we know the version increased.
    #[inline(always)]
    unsafe fn read_bbo_strict(&self, symbol_id: u16, exchange_id: u8) -> ShmBboMessage {
        // Get reference to the BBO message
        let msg_ref = &(*self.shm).bbo_matrix[symbol_id as usize][exchange_id as usize];
        
        // Get pointer to seqlock as AtomicU32 for proper atomic loads
        let msg_bytes = msg_ref as *const ShmBboMessage as *const u8;
        let seqlock_ptr = msg_bytes.add(0) as *const AtomicU32;
        
        // Phase 1: load seqlock (Acquire)
        let seq1 = (*seqlock_ptr).load(Ordering::Acquire);
        
        // Bail if odd (write in progress) or zero (never written)
        if seq1 & 1 != 0 || seq1 == 0 {
            return ShmBboMessage::default();
        }
        
        // Phase 2: compiler fence
        compiler_fence(Ordering::Acquire);
        
        // Phase 3: volatile read of payload
        let msg = ptr::read_volatile(msg_ref);
        
        // Phase 4: compiler fence
        compiler_fence(Ordering::Acquire);
        
        // Phase 5: re-check seqlock
        let seq2 = (*seqlock_ptr).load(Ordering::Acquire);
        if seq1 != seq2 {
            return ShmBboMessage::default(); // torn read
        }
        
        msg
    }

    /// Check if any symbol has new data (O(max_symbols) scan).
    /// Returns the first symbol_id that updated, or None.
    #[inline(always)]
    pub fn try_poll(&mut self) -> Option<u16> {
        for sym in 0..self.max_symbols {
            let version = self.load_version(sym as u16);
            
            if version > self.local_versions.0[sym] {
                self.local_versions.0[sym] = version;
                return Some(sym as u16);
            }
        }
        None
    }

    /// Poll all symbols that have updates. Returns up to `max` symbol IDs.
    #[inline(always)]
    pub fn poll_all(&mut self, max: usize) -> Vec<u16> {
        let mut updated = Vec::with_capacity(max);
        
        for sym in 0..self.max_symbols {
            let version = self.load_version(sym as u16);
            
            if version > self.local_versions.0[sym] {
                self.local_versions.0[sym] = version;
                updated.push(sym as u16);
                
                if updated.len() >= max {
                    break;
                }
            }
        }
        
        updated
    }

    /// Read the latest BBO for a specific (symbol, exchange) pair.
    /// Only use after version check confirmed an update.
    #[inline(always)]
    pub fn read_bbo(&mut self, symbol_id: u16, exchange_id: u8) -> ShmBboMessage {
        // Update local version first
        let version = self.load_version(symbol_id);
        self.local_versions.0[symbol_id as usize] = version;
        
        // Then read with seqlock
        unsafe { self.read_bbo_strict(symbol_id, exchange_id) }
    }

    /// Bulk-read all 5 exchange BBO slots for a symbol.
    /// Returns array of (exchange_id, message).
    #[inline(always)]
    pub fn read_all_exchanges(&mut self, symbol_id: u16) -> [(u8, ShmBboMessage); NUM_EXCHANGES] {
        // Update version
        let version = self.load_version(symbol_id);
        self.local_versions.0[symbol_id as usize] = version;
        
        // Read all exchanges
        let mut result = [(0u8, ShmBboMessage::default()); NUM_EXCHANGES];
        for exch in 0..NUM_EXCHANGES {
            let msg = unsafe { self.read_bbo_strict(symbol_id, exch as u8) };
            result[exch] = (exch as u8, msg);
        }
        result
    }

    /// Get current local version for a symbol (for diagnostics).
    pub fn local_version(&self, symbol_id: u16) -> u64 {
        self.local_versions.0[symbol_id as usize]
    }

    /// Get shared version for a symbol (for diagnostics).
    pub fn shared_version(&self, symbol_id: u16) -> u64 {
        self.load_version(symbol_id)
    }
}
