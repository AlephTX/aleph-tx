//! Lock-Free Event Ring Buffer Reader (V2: 128-byte events + proper memory barriers)
//!
//! This module provides a non-blocking reader for the private event ring buffer.
//! Events are written by the Go feeder and consumed by Rust strategies.
//!
//! # Safety
//!
//! V2 changes:
//! - Replaced `read_volatile` with `AtomicU64::load(Acquire)` for correct memory ordering
//!   on ARM/Apple Silicon (read_volatile does NOT provide hardware memory barriers)
//! - Added `compiler_fence(Acquire)` after index load to prevent instruction reordering
//! - Supports both V1 (64-byte) and V2 (128-byte) event formats
//!
//! Memory ordering guarantee:
//! - Go writer: `atomic.StoreUint64(&write_idx, val)` (Release semantics)
//! - Rust reader: `AtomicU64::load(Acquire)` (Acquire semantics)
//! - This Release-Acquire pair ensures all event slot writes are visible before
//!   the reader sees the updated write_idx.

use crate::error::{Result, TradingError};
use crate::types::{ShmPrivateEvent, ShmPrivateEventV2};
use memmap2::Mmap;
use std::fs::OpenOptions;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering, fence};

// ─── V1 Constants (64-byte events) ──────────────────────────────────────────

const V1_RING_BUFFER_SLOTS: u64 = 1024;
const V1_EVENT_SIZE: usize = 64;
const V1_HEADER_SIZE: usize = 64;
const V1_TOTAL_SIZE: usize = V1_HEADER_SIZE + (V1_RING_BUFFER_SLOTS as usize * V1_EVENT_SIZE);

// ─── V2 Constants (128-byte events) ─────────────────────────────────────────

const V2_RING_BUFFER_SLOTS: u64 = 1024;
const V2_EVENT_SIZE: usize = 128;
const V2_HEADER_SIZE: usize = 64;
const V2_TOTAL_SIZE: usize = V2_HEADER_SIZE + (V2_RING_BUFFER_SLOTS as usize * V2_EVENT_SIZE);

// Header bytes 8..16 form a producer generation: odd while any slot is
// being overwritten, even after write_idx is published. The old write_idx-only
// protocol cannot protect a slow reader when a full ring wraps over its slot.
#[inline]
fn generation(mmap: &Mmap) -> u64 {
    // SAFETY: Both constructors validate the complete 64-byte aligned header.
    unsafe { (*mmap.as_ptr().add(8).cast::<AtomicU64>()).load(Ordering::Acquire) }
}

// ─── V1 Reader (backward compatible) ────────────────────────────────────────

/// Lock-free event ring buffer reader (V1: 64-byte events)
///
/// DEPRECATED: Use ShmEventReaderV2 for new code.
///
/// # Safety
/// - Single reader, single writer (SPSC) model
/// - Uses AtomicU64::load(Acquire) for correct memory ordering on all architectures
/// - No heap allocations in hot path
pub struct ShmEventReader {
    mmap: Mmap,
    local_read_idx: u64,
    integrity_lost: bool,
}

impl ShmEventReader {
    /// Open the event ring buffer at the specified path
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .open(path.as_ref())
            .map_err(|e| {
                TradingError::SharedMemory(format!(
                    "Failed to open {}: {}",
                    path.as_ref().display(),
                    e
                ))
            })?;

        let mmap = unsafe { Mmap::map(&file) }
            .map_err(|e| TradingError::SharedMemory(format!("Failed to mmap: {}", e)))?;

        if mmap.len() < V1_TOTAL_SIZE {
            return Err(TradingError::SharedMemory(format!(
                "Shared memory too small: {} < {}",
                mmap.len(),
                V1_TOTAL_SIZE
            )));
        }

        Ok(Self {
            mmap,
            local_read_idx: 0,
            integrity_lost: false,
        })
    }

    /// Open the event ring buffer at the default path `/dev/shm/aleph-events`
    pub fn new_default() -> Result<Self> {
        Self::new("/dev/shm/aleph-events")
    }

    /// Read the current write index using proper atomic load (Acquire ordering)
    ///
    /// # Safety
    ///
    /// Uses AtomicU64::load(Acquire) instead of read_volatile.
    /// read_volatile only prevents compiler reordering, NOT hardware reordering.
    /// On ARM/Apple Silicon, this distinction is critical — without a hardware
    /// memory barrier, we could read stale event data even after seeing an
    /// updated write_idx.
    #[inline]
    fn read_write_idx(&self) -> u64 {
        // SAFETY: The first 8 bytes of the mmap are the write_idx, written
        // atomically by the Go feeder using atomic.StoreUint64 (Release).
        // We read with Acquire to form a Release-Acquire pair.
        let ptr = self.mmap.as_ptr() as *const AtomicU64;
        let val = unsafe { (*ptr).load(Ordering::Acquire) };

        // Belt-and-suspenders: compiler fence to prevent any subsequent reads
        // from being reordered before the atomic load.
        fence(Ordering::Acquire);

        val
    }

    /// Read an event from a specific slot using atomic-safe copy
    #[inline]
    fn read_slot(&self, slot: usize) -> ShmPrivateEvent {
        let offset = V1_HEADER_SIZE + (slot * V1_EVENT_SIZE);
        let ptr = unsafe { self.mmap.as_ptr().add(offset) as *const ShmPrivateEvent };
        // SAFETY: The Acquire load on write_idx guarantees this data is fully written.
        // We use read_volatile as an additional safety measure to prevent the compiler
        // from caching or eliding this read.
        unsafe { std::ptr::read_volatile(ptr) }
    }

    /// Try to read the next event (non-blocking)
    ///
    /// Returns `None` if no new events are available.
    pub fn try_read(&mut self) -> Option<ShmPrivateEvent> {
        let before = generation(&self.mmap);
        if before & 1 != 0 {
            return None;
        }
        let write_idx = self.read_write_idx();
        if write_idx < self.local_read_idx {
            self.integrity_lost = true;
            return None;
        }
        if self.local_read_idx == write_idx {
            return None;
        }
        let read_idx = self
            .local_read_idx
            .max(write_idx.saturating_sub(V1_RING_BUFFER_SLOTS));
        let slot = (read_idx % V1_RING_BUFFER_SLOTS) as usize;
        let event = self.read_slot(slot);
        fence(Ordering::Acquire);
        // If the writer touched the ring during our copy, retry without consuming.
        if generation(&self.mmap) != before {
            return None;
        }
        if event.sequence != read_idx {
            self.integrity_lost = true;
            return None;
        }
        if read_idx > self.local_read_idx {
            self.integrity_lost = true;
            tracing::error!(
                "V1 event ring overflow: {} events lost",
                read_idx - self.local_read_idx
            );
        }
        self.local_read_idx = read_idx + 1;
        Some(event)
    }

    /// Consume an integrity alarm (overflow, corrupt slot, or producer index reset).
    /// The caller must stop new orders and reconcile authoritative account state.
    pub fn take_integrity_loss(&mut self) -> bool {
        std::mem::take(&mut self.integrity_lost)
    }

    pub fn local_read_idx(&self) -> u64 {
        self.local_read_idx
    }

    pub fn write_idx(&self) -> u64 {
        self.read_write_idx()
    }

    pub fn has_events(&self) -> bool {
        self.local_read_idx < self.read_write_idx()
    }

    pub fn unread_count(&self) -> u64 {
        let write_idx = self.read_write_idx();
        write_idx.saturating_sub(self.local_read_idx)
    }
}

// ─── V2 Reader (128-byte events, world-class) ──────────────────────────────

/// Lock-free event ring buffer reader (V2: 128-byte events)
///
/// # Safety
/// - Single reader, single writer (SPSC) model
/// - Uses AtomicU64::load(Acquire) + compiler_fence for correct memory ordering
///   on x86, ARM, and Apple Silicon
/// - No heap allocations in hot path
pub struct ShmEventReaderV2 {
    mmap: Mmap,
    local_read_idx: u64,
    integrity_lost: bool,
}

impl ShmEventReaderV2 {
    /// Open the V2 event ring buffer at the specified path
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .open(path.as_ref())
            .map_err(|e| {
                TradingError::SharedMemory(format!(
                    "Failed to open {}: {}",
                    path.as_ref().display(),
                    e
                ))
            })?;

        let mmap = unsafe { Mmap::map(&file) }
            .map_err(|e| TradingError::SharedMemory(format!("Failed to mmap: {}", e)))?;

        if mmap.len() < V2_TOTAL_SIZE {
            return Err(TradingError::SharedMemory(format!(
                "V2 shared memory too small: {} < {}",
                mmap.len(),
                V2_TOTAL_SIZE
            )));
        }

        Ok(Self {
            mmap,
            local_read_idx: 0,
            integrity_lost: false,
        })
    }

    /// Open the V2 event ring buffer at the default path
    pub fn new_default() -> Result<Self> {
        Self::new("/dev/shm/aleph-events-v2")
    }

    /// Skip all existing events, only consume events written after this call
    pub fn skip_to_end(&mut self) {
        self.local_read_idx = self.read_write_idx();
    }

    /// Read the current write index using proper atomic load (Acquire ordering)
    #[inline]
    fn read_write_idx(&self) -> u64 {
        let ptr = self.mmap.as_ptr() as *const AtomicU64;
        let val = unsafe { (*ptr).load(Ordering::Acquire) };
        fence(Ordering::Acquire);
        val
    }

    /// Read a V2 event from a specific slot
    #[inline]
    fn read_slot(&self, slot: usize) -> ShmPrivateEventV2 {
        let offset = V2_HEADER_SIZE + (slot * V2_EVENT_SIZE);
        let ptr = unsafe { self.mmap.as_ptr().add(offset) as *const ShmPrivateEventV2 };
        unsafe { std::ptr::read_volatile(ptr) }
    }

    /// Try to read the next V2 event (non-blocking)
    ///
    /// Returns `None` if no new events are available.
    pub fn try_read(&mut self) -> Option<ShmPrivateEventV2> {
        let before = generation(&self.mmap);
        if before & 1 != 0 {
            return None;
        }
        let write_idx = self.read_write_idx();
        if write_idx < self.local_read_idx {
            self.integrity_lost = true;
            return None;
        }
        if self.local_read_idx == write_idx {
            return None;
        }
        let read_idx = self
            .local_read_idx
            .max(write_idx.saturating_sub(V2_RING_BUFFER_SLOTS));
        let slot = (read_idx % V2_RING_BUFFER_SLOTS) as usize;
        let event = self.read_slot(slot);
        fence(Ordering::Acquire);
        // If the writer touched the ring during our copy, retry without consuming.
        if generation(&self.mmap) != before {
            return None;
        }
        if event.sequence != read_idx {
            self.integrity_lost = true;
            return None;
        }
        if read_idx > self.local_read_idx {
            self.integrity_lost = true;
            tracing::error!(
                "V2 event ring overflow: {} events lost",
                read_idx - self.local_read_idx
            );
        }
        self.local_read_idx = read_idx + 1;
        Some(event)
    }

    /// Consume an integrity alarm (overflow, corrupt slot, or producer index reset).
    /// The caller must stop new orders and reconcile authoritative account state.
    pub fn take_integrity_loss(&mut self) -> bool {
        std::mem::take(&mut self.integrity_lost)
    }

    pub fn local_read_idx(&self) -> u64 {
        self.local_read_idx
    }

    pub fn write_idx(&self) -> u64 {
        self.read_write_idx()
    }

    pub fn has_events(&self) -> bool {
        self.local_read_idx < self.read_write_idx()
    }

    pub fn unread_count(&self) -> u64 {
        self.read_write_idx().saturating_sub(self.local_read_idx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring_v2(index: u64, generation: u64, event: ShmPrivateEventV2) -> ShmEventReaderV2 {
        let mut map = memmap2::MmapMut::map_anon(V2_TOTAL_SIZE).unwrap();
        map[0..8].copy_from_slice(&index.to_ne_bytes());
        map[8..16].copy_from_slice(&generation.to_ne_bytes());
        let slot = event.sequence as usize % V2_RING_BUFFER_SLOTS as usize;
        // SAFETY: anonymous mmap is page aligned; slot is within allocated extent.
        unsafe {
            std::ptr::write(
                map.as_mut_ptr()
                    .add(V2_HEADER_SIZE + slot * V2_EVENT_SIZE)
                    .cast(),
                event,
            );
        }
        ShmEventReaderV2 {
            mmap: map.make_read_only().unwrap(),
            local_read_idx: 0,
            integrity_lost: false,
        }
    }

    #[test]
    fn v2_reads_zero_based_first_event_once() {
        let event = ShmPrivateEventV2::order_created(0, 2, 1, 5, 7, 5, 100.0, 0.1, false, 0);
        let mut reader = ring_v2(1, 2, event);
        assert_eq!(reader.try_read().unwrap().client_order_id, 7);
        assert_eq!(reader.local_read_idx(), 1);
        assert!(reader.try_read().is_none());
    }

    #[test]
    fn v2_does_not_consume_while_writer_is_overwriting() {
        let event = ShmPrivateEventV2::order_created(0, 2, 1, 5, 7, 5, 100.0, 0.1, false, 0);
        let mut reader = ring_v2(1, 3, event);
        assert!(reader.try_read().is_none());
        assert_eq!(reader.local_read_idx(), 0);
    }

    #[test]
    fn v2_overflow_reads_oldest_surviving_slot() {
        let event = ShmPrivateEventV2::order_created(5, 2, 1, 5, 7, 5, 100.0, 0.1, false, 0);
        let mut reader = ring_v2(V2_RING_BUFFER_SLOTS + 5, 10, event);
        assert_eq!(reader.try_read().unwrap().sequence, 5);
        assert_eq!(reader.local_read_idx(), 6);
        assert!(reader.take_integrity_loss());
        assert!(!reader.take_integrity_loss());
    }

    #[test]
    fn v2_rejects_mismatched_slot_sequence() {
        let event = ShmPrivateEventV2::order_created(
            V2_RING_BUFFER_SLOTS,
            2,
            1,
            5,
            7,
            5,
            100.0,
            0.1,
            false,
            0,
        );
        let mut reader = ring_v2(1, 2, event);
        assert!(reader.try_read().is_none());
        assert_eq!(reader.local_read_idx(), 0);
    }

    #[test]
    fn producer_index_regression_is_reported() {
        let mut reader = ring_v2(1, 2, ShmPrivateEventV2::default());
        reader.local_read_idx = 100;
        assert!(reader.try_read().is_none());
        assert!(reader.take_integrity_loss());
    }

    #[test]
    fn v1_uses_the_same_generation_protocol() {
        let mut map = memmap2::MmapMut::map_anon(V1_TOTAL_SIZE).unwrap();
        map[0..8].copy_from_slice(&1u64.to_ne_bytes());
        map[8..16].copy_from_slice(&2u64.to_ne_bytes());
        let event = ShmPrivateEvent::order_created(0, 2, 1, 5, 0.1, false);
        // SAFETY: anonymous map has a fully allocated aligned first slot.
        unsafe {
            std::ptr::write(map.as_mut_ptr().add(V1_HEADER_SIZE).cast(), event);
        }
        let mut reader = ShmEventReader {
            mmap: map.make_read_only().unwrap(),
            local_read_idx: 0,
            integrity_lost: false,
        };
        assert_eq!(reader.try_read().unwrap().order_id, 5);
        assert!(reader.try_read().is_none());
    }
}
