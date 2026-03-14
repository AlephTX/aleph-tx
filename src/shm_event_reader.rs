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
use memmap2::MmapMut;
use std::fs::OpenOptions;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering, compiler_fence};

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
    mmap: MmapMut,
    local_read_idx: u64,
}

impl ShmEventReader {
    /// Open the event ring buffer at the specified path
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path.as_ref())
            .map_err(|e| {
                TradingError::SharedMemory(format!(
                    "Failed to open {}: {}",
                    path.as_ref().display(),
                    e
                ))
            })?;

        let mmap = unsafe { MmapMut::map_mut(&file) }
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
        compiler_fence(Ordering::Acquire);

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
        let write_idx = self.read_write_idx();

        if self.local_read_idx >= write_idx {
            return None;
        }

        // Detect gaps (writer wrapped around and overwrote unread events)
        let unread = write_idx.saturating_sub(self.local_read_idx);
        if unread > V1_RING_BUFFER_SLOTS {
            let gap_size = unread - V1_RING_BUFFER_SLOTS;
            tracing::error!(
                "⚠️  Event gap detected: {} events lost (buffer overflow)",
                gap_size
            );
            self.local_read_idx = write_idx.saturating_sub(V1_RING_BUFFER_SLOTS);
        }

        let slot = (self.local_read_idx % V1_RING_BUFFER_SLOTS) as usize;
        let event = self.read_slot(slot);
        self.local_read_idx += 1;

        Some(event)
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
    mmap: MmapMut,
    local_read_idx: u64,
}

impl ShmEventReaderV2 {
    /// Open the V2 event ring buffer at the specified path
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path.as_ref())
            .map_err(|e| {
                TradingError::SharedMemory(format!(
                    "Failed to open {}: {}",
                    path.as_ref().display(),
                    e
                ))
            })?;

        let mmap = unsafe { MmapMut::map_mut(&file) }
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
        compiler_fence(Ordering::Acquire);
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
        let write_idx = self.read_write_idx();

        if self.local_read_idx >= write_idx {
            return None;
        }

        // Detect gaps
        let unread = write_idx.saturating_sub(self.local_read_idx);
        if unread > V2_RING_BUFFER_SLOTS {
            let gap_size = unread - V2_RING_BUFFER_SLOTS;
            tracing::error!(
                "⚠️  V2 Event gap detected: {} events lost (buffer overflow)",
                gap_size
            );
            self.local_read_idx = write_idx.saturating_sub(V2_RING_BUFFER_SLOTS);
        }

        let slot = (self.local_read_idx % V2_RING_BUFFER_SLOTS) as usize;
        let event = self.read_slot(slot);
        self.local_read_idx += 1;

        Some(event)
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

    #[test]
    fn test_v1_reader_creation() {
        match ShmEventReader::new_default() {
            Ok(reader) => {
                assert_eq!(reader.local_read_idx(), 0);
                println!("V1 Reader created successfully");
            }
            Err(e) => {
                println!(
                    "Expected: shared memory not yet created by Go feeder: {}",
                    e
                );
            }
        }
    }

    #[test]
    fn test_v2_reader_creation() {
        match ShmEventReaderV2::new_default() {
            Ok(reader) => {
                assert_eq!(reader.local_read_idx(), 0);
                println!("V2 Reader created successfully");
            }
            Err(e) => {
                println!(
                    "Expected: V2 shared memory not yet created by Go feeder: {}",
                    e
                );
            }
        }
    }

    #[test]
    fn test_v2_constants() {
        assert_eq!(V2_EVENT_SIZE, 128);
        assert_eq!(V2_TOTAL_SIZE, 64 + 1024 * 128);
    }
}
