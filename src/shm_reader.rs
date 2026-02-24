//! Seqlock shared memory reader — zero-copy, cache-line-aligned BBO messages.

use std::sync::atomic::{AtomicU32, Ordering};
use std::{fs::File, ptr};

/// 64-byte cache-line-aligned BBO message.
/// Layout matches Go's ShmBboMessage exactly.
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
const _: () = assert!(std::mem::size_of::<ShmBboMessage>() == SLOT_SIZE);

/// Memory-mapped seqlock ring buffer reader.
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
        let path = format!("/dev/shm/{}", name);
        let file = File::open(&path)?;
        let mmap = unsafe { memmap2::Mmap::map(&file)? };
        assert!(mmap.len() >= slots * SLOT_SIZE);
        Ok(Self { mmap, slots, read_idx: 0 })
    }

    /// Try to read the next slot using the seqlock protocol.
    /// Returns `Some(msg)` on success, `None` if the slot is being written.
    /// Zero heap allocations.
    #[inline(always)]
    pub fn try_read(&mut self) -> Option<ShmBboMessage> {
        let offset = (self.read_idx as usize & (self.slots - 1)) * SLOT_SIZE;
        let slot_ptr = self.mmap.as_ptr().wrapping_add(offset);

        // Safety: slot_ptr is within the mmap region and aligned to 64 bytes.
        unsafe {
            let seq_ptr = slot_ptr as *const AtomicU32;

            // Phase 1: read seqlock — must be even (no write in progress)
            let seq1 = (*seq_ptr).load(Ordering::Acquire);
            if seq1 & 1 != 0 {
                // Writer is mid-write, spin
                return None;
            }
            if seq1 == 0 {
                // Slot never written
                return None;
            }

            // Phase 2: copy 64 bytes to stack (zero-copy from mmap perspective)
            let mut local = std::mem::MaybeUninit::<ShmBboMessage>::uninit();
            ptr::copy_nonoverlapping(
                slot_ptr,
                local.as_mut_ptr() as *mut u8,
                SLOT_SIZE,
            );

            // Phase 3: re-read seqlock — must match seq1
            std::sync::atomic::fence(Ordering::Acquire);
            let seq2 = (*seq_ptr).load(Ordering::Acquire);
            if seq1 != seq2 {
                // Writer overwrote during our read, discard
                return None;
            }

            self.read_idx += 1;
            Some(local.assume_init())
        }
    }

    /// Blocking read with spin-loop. Use for latency-critical paths.
    #[inline(always)]
    pub fn read_spin(&mut self) -> ShmBboMessage {
        loop {
            if let Some(msg) = self.try_read() {
                return msg;
            }
            std::hint::spin_loop();
        }
    }
}
