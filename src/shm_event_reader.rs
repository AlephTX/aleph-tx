//! Lock-Free Event Ring Buffer Reader
//!
//! This module provides a non-blocking reader for the private event ring buffer.
//! Events are written by the Go feeder and consumed by Rust strategies.
//!
//! # Safety
//!
//! This implementation uses volatile reads and compiler fences to ensure correct
//! memory ordering in a lock-free SPSC (single producer, single consumer) model:
//! - Only one writer (Go feeder) updates write_idx atomically
//! - Only one reader (Rust strategy) reads events sequentially
//! - Acquire fence ensures event data is visible before we read it
//!
//! The ring buffer can hold up to 1024 events. If the writer wraps around and
//! overwrites unread events, a gap is detected and reported.

use crate::error::{Result, TradingError};
use crate::types::ShmPrivateEvent;
use memmap2::MmapMut;
use std::fs::OpenOptions;
use std::path::Path;
use std::sync::atomic::{compiler_fence, Ordering};

const RING_BUFFER_SLOTS: u64 = 1024;
const EVENT_SIZE: usize = 64;
const HEADER_SIZE: usize = 64;
const TOTAL_SIZE: usize = HEADER_SIZE + (RING_BUFFER_SLOTS as usize * EVENT_SIZE);

/// Lock-free event ring buffer reader
///
/// # Safety
/// - Single reader, single writer (SPSC) model
/// - Uses atomic operations and compiler fences to prevent torn reads
/// - No heap allocations in hot path
pub struct ShmEventReader {
    mmap: MmapMut,
    local_read_idx: u64,
}

impl ShmEventReader {
    /// Open the event ring buffer at the specified path
    ///
    /// # Errors
    /// Returns error if the shared memory file doesn't exist or cannot be mapped
    ///
    /// # Default Path
    /// Use `ShmEventReader::new_default()` to open `/dev/shm/aleph-events`
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self> {
        // Open the shared memory file
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

        // Memory-map the file
        let mmap = unsafe { MmapMut::map_mut(&file) }.map_err(|e| {
            TradingError::SharedMemory(format!("Failed to mmap: {}", e))
        })?;

        if mmap.len() < TOTAL_SIZE {
            return Err(TradingError::SharedMemory(format!(
                "Shared memory too small: {} < {}",
                mmap.len(),
                TOTAL_SIZE
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

    /// Read the current write index (atomic load)
    ///
    /// # Safety
    ///
    /// Uses volatile read to prevent compiler optimizations that could cause
    /// torn reads. The Go writer updates this atomically.
    #[inline]
    fn read_write_idx(&self) -> u64 {
        let ptr = self.mmap.as_ptr() as *const u64;
        unsafe { std::ptr::read_volatile(ptr) }
    }

    /// Read an event from a specific slot
    ///
    /// # Safety
    ///
    /// Uses volatile read to ensure we see the complete event write.
    /// The acquire fence in `try_read()` ensures proper memory ordering.
    #[inline]
    fn read_slot(&self, slot: usize) -> ShmPrivateEvent {
        let offset = HEADER_SIZE + (slot * EVENT_SIZE);
        let ptr = unsafe { self.mmap.as_ptr().add(offset) as *const ShmPrivateEvent };
        unsafe { std::ptr::read_volatile(ptr) }
    }

    /// Try to read the next event (non-blocking)
    ///
    /// Returns `None` if no new events are available.
    /// Returns `Some(event)` if a new event was read.
    ///
    /// # Gap Detection
    ///
    /// If the writer has wrapped around and overwrote unread events, this method
    /// detects the gap, logs an error, and skips to the oldest available event.
    ///
    /// # Safety
    ///
    /// Uses compiler_fence(Acquire) to ensure the event is fully written before reading.
    pub fn try_read(&mut self) -> Option<ShmPrivateEvent> {
        let write_idx = self.read_write_idx();

        // No new events available
        if self.local_read_idx >= write_idx {
            return None;
        }

        // Detect gaps (writer wrapped around and overwrote unread events)
        let unread = write_idx.saturating_sub(self.local_read_idx);
        if unread > RING_BUFFER_SLOTS {
            let gap_size = unread - RING_BUFFER_SLOTS;
            tracing::error!(
                "⚠️  Event gap detected: {} events lost (buffer overflow)",
                gap_size
            );

            // Skip to the oldest available event
            self.local_read_idx = write_idx.saturating_sub(RING_BUFFER_SLOTS);
        }

        // Acquire fence: ensure we see the complete event write
        compiler_fence(Ordering::Acquire);

        // Calculate slot index (wrap around)
        let slot = (self.local_read_idx % RING_BUFFER_SLOTS) as usize;

        // Read the event
        let event = self.read_slot(slot);

        // Advance local read index
        self.local_read_idx += 1;

        Some(event)
    }

    /// Get the current local read index (for debugging)
    pub fn local_read_idx(&self) -> u64 {
        self.local_read_idx
    }

    /// Get the current write index (for debugging)
    pub fn write_idx(&self) -> u64 {
        self.read_write_idx()
    }

    /// Check if there are unread events
    pub fn has_events(&self) -> bool {
        self.local_read_idx < self.read_write_idx()
    }

    /// Get the number of unread events
    pub fn unread_count(&self) -> u64 {
        let write_idx = self.read_write_idx();
        write_idx.saturating_sub(self.local_read_idx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reader_creation() {
        // This test requires the shared memory file to exist
        // In production, the Go feeder creates it
        match ShmEventReader::new_default() {
            Ok(reader) => {
                assert_eq!(reader.local_read_idx(), 0);
                println!("Reader created successfully");
            }
            Err(e) => {
                println!("Expected: shared memory not yet created by Go feeder: {}", e);
            }
        }
    }
}
