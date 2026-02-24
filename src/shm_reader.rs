//! Seqlock shared memory reader — zero-copy, cache-line-aligned BBO messages.
//!
//! Uses strict Acquire/Release ordering on the seqlock to prevent
//! instruction reordering across the read barrier.

use std::sync::atomic::{AtomicU32, Ordering};
use std::ptr;

/// 64-byte cache-line-aligned BBO message.
/// Layout matches Go's ShmBboMessage exactly (C ABI).
#[repr(C, align(64))]
#[derive(Clone, Copy, Debug)]
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

// Compile-time assertion: ShmBboMessage must be exactly 64 bytes.
const _: () = assert!(size_of::<ShmBboMessage>() == SLOT_SIZE);
const _: () = assert!(align_of::<ShmBboMessage>() == 64);

/// Memory-mapped seqlock ring buffer reader.
///
/// Single-consumer, designed for the Rust strategy engine hot loop.
/// All reads are zero-allocation — the message is copied to a stack local.
pub struct ShmRingReader {
    mmap: memmap2::Mmap,
    slots: usize,
    read_idx: u64,
}

impl ShmRingReader {
    /// Open an existing shared memory ring buffer.
    /// `slots` must match the writer's slot count (power of 2).
    pub fn open(name: &str, slots: usize) -> anyhow::Result<Self> {
        assert!(slots.is_power_of_two(), "slots must be power of 2");
        let path = format!("/dev/shm/{name}");
        let file = std::fs::File::open(&path)?;
        let mmap = unsafe { memmap2::Mmap::map(&file)? };
        assert!(
            mmap.len() >= slots * SLOT_SIZE,
            "mmap too small: {} < {}",
            mmap.len(),
            slots * SLOT_SIZE
        );
        Ok(Self {
            mmap,
            slots,
            read_idx: 0,
        })
    }

    /// Slot base pointer for a given index (mod slots).
    #[inline(always)]
    fn slot_ptr(&self, idx: u64) -> *const u8 {
        let offset = (idx as usize & (self.slots - 1)) * SLOT_SIZE;
        self.mmap.as_ptr().wrapping_add(offset)
    }

    /// Try to read the next slot using the seqlock protocol.
    ///
    /// Returns `Some(msg)` on a consistent read, `None` if the slot is
    /// currently being written or has never been written.
    ///
    /// Memory ordering:
    /// - `Acquire` on the first seqlock load establishes a happens-before
    ///   relationship with the writer's `Release` store, ensuring all
    ///   payload stores are visible before we copy.
    /// - An explicit `Acquire` fence after the copy prevents the compiler
    ///   and CPU from reordering the second seqlock load before the copy.
    /// - `Acquire` on the second seqlock load ensures we see the writer's
    ///   final `Release` store if it completed during our copy window.
    #[inline(always)]
    pub fn try_read(&mut self) -> Option<ShmBboMessage> {
        let slot_ptr = self.slot_ptr(self.read_idx);

        // SAFETY: slot_ptr is within the mmap region, aligned to 64 bytes,
        // and we only read through atomic + copy_nonoverlapping.
        unsafe {
            let seq_ptr = slot_ptr.cast::<AtomicU32>();

            // Phase 1: load seqlock with Acquire ordering.
            // If odd, a write is in progress — bail.
            let seq1 = (*seq_ptr).load(Ordering::Acquire);
            if seq1 & 1 != 0 || seq1 == 0 {
                return None;
            }

            // Phase 2: copy the entire 64-byte slot to a stack-local.
            // This is the "read window" — must be bounded by barriers.
            let mut local = std::mem::MaybeUninit::<ShmBboMessage>::uninit();
            ptr::copy_nonoverlapping(slot_ptr, local.as_mut_ptr().cast::<u8>(), SLOT_SIZE);

            // Phase 3: acquire fence to prevent reordering of the copy
            // past the second seqlock load.
            std::sync::atomic::fence(Ordering::Acquire);

            // Phase 4: re-read seqlock. If it changed, the writer overwrote
            // during our copy — discard the torn read.
            let seq2 = (*seq_ptr).load(Ordering::Acquire);
            if seq1 != seq2 {
                return None;
            }

            self.read_idx += 1;
            Some(local.assume_init())
        }
    }

    /// Blocking spin-read. Use only on latency-critical dedicated cores.
    ///
    /// Spins with `std::hint::spin_loop()` (emits PAUSE on x86) to
    /// reduce power consumption and avoid starving the writer's
    /// hyper-thread.
    #[inline(always)]
    pub fn read_spin(&mut self) -> ShmBboMessage {
        loop {
            if let Some(msg) = self.try_read() {
                return msg;
            }
            std::hint::spin_loop();
        }
    }

    /// Current read position (for diagnostics).
    pub fn read_idx(&self) -> u64 {
        self.read_idx
    }

    /// Reset read cursor to a specific position.
    pub fn seek(&mut self, idx: u64) {
        self.read_idx = idx;
    }
}
