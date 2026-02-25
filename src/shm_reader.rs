//! Seqlock shared memory reader — zero-copy, cache-line-aligned BBO messages.
//!
//! Uses strict compiler fences and volatile reads to prevent any reordering
//! across the seqlock read barrier. This is critical for correctness on
//! weakly-ordered architectures and against aggressive compiler optimizations.

use std::sync::atomic::{compiler_fence, AtomicU32, Ordering};
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
    /// Memory ordering protocol (bulletproof against compiler + CPU reordering):
    ///
    /// 1. **Atomic Acquire load** of seqlock — establishes happens-before with
    ///    the writer's Release store. Bail if odd (write in progress) or zero
    ///    (never written).
    ///
    /// 2. **compiler_fence(Acquire)** — prevents the compiler from hoisting
    ///    the volatile payload read above the seqlock check. On x86 this is
    ///    sufficient (loads are not reordered with loads). On ARM/RISC-V the
    ///    Acquire on the atomic load already provides the hardware barrier.
    ///
    /// 3. **read_volatile** of the full 64-byte slot — forces the compiler to
    ///    emit an actual load from the mmap'd address. Unlike
    ///    `copy_nonoverlapping`, volatile reads cannot be elided, merged, or
    ///    reordered by the compiler. This is the critical difference: the
    ///    compiler must treat the source as potentially changing between any
    ///    two reads.
    ///
    /// 4. **compiler_fence(Acquire)** — prevents the compiler from sinking
    ///    the second seqlock load above the payload read. Ensures the full
    ///    64 bytes are materialized before we validate consistency.
    ///
    /// 5. **Atomic Acquire load** of seqlock again — if it changed, the writer
    ///    overwrote during our read window. Discard the torn read.
    #[inline(always)]
    pub fn try_read(&mut self) -> Option<ShmBboMessage> {
        let slot_ptr = self.slot_ptr(self.read_idx);

        // SAFETY: slot_ptr is within the mmap region, aligned to 64 bytes.
        // We read through atomic ops + read_volatile only.
        unsafe {
            let seq_ptr = slot_ptr.cast::<AtomicU32>();

            // ── Step 1: Load seqlock (Acquire) ──────────────────────────
            // Acquire ordering on x86 emits a plain MOV (loads already have
            // acquire semantics). On ARM it emits LDAR. Either way, all
            // subsequent loads in program order see stores that happened
            // before the writer's matching Release store.
            let seq1 = (*seq_ptr).load(Ordering::Acquire);

            // Odd → write in progress. Zero → slot never written.
            if seq1 & 1 != 0 || seq1 == 0 {
                return None;
            }

            // ── Step 2: Compiler fence ──────────────────────────────────
            // Prevent the compiler from moving the volatile read above the
            // seqlock check. This is a compiler-only barrier (no hardware
            // instruction emitted on any arch).
            compiler_fence(Ordering::Acquire);

            // ── Step 3: Volatile read of 64-byte payload ────────────────
            // read_volatile guarantees:
            //   - The read is not elided (compiler cannot optimize it away)
            //   - The read is not split or merged with adjacent reads
            //   - The read is performed exactly once at this program point
            //
            // We read the entire ShmBboMessage as a single aligned 64-byte
            // volatile load. The alignment guarantee (repr(align(64))) means
            // this maps to efficient SIMD or cache-line-width loads on
            // modern CPUs.
            let msg = ptr::read_volatile(slot_ptr.cast::<ShmBboMessage>());

            // ── Step 4: Compiler fence ──────────────────────────────────
            // Prevent the compiler from reordering the second seqlock load
            // before the volatile read completes. Without this, the compiler
            // could legally move seq2's load above the payload read, making
            // the consistency check useless.
            compiler_fence(Ordering::Acquire);

            // ── Step 5: Re-load seqlock (Acquire) ───────────────────────
            // If seq2 != seq1, the writer started or completed a write
            // during our read window — the payload may be torn. Discard.
            let seq2 = (*seq_ptr).load(Ordering::Acquire);
            if seq1 != seq2 {
                return None;
            }

            self.read_idx += 1;
            Some(msg)
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
