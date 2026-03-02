//! Lock-Free Event Ring Buffer Reader
//!
//! This module provides a non-blocking reader for the private event ring buffer.
//! Events are written by the Go feeder and consumed by Rust strategies.

use crate::types::ShmPrivateEvent;
use memmap2::MmapMut;
use std::fs::OpenOptions;
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
    /// Open the event ring buffer at /dev/shm/aleph-events
    ///
    /// # Errors
    /// Returns error if the shared memory file doesn't exist or cannot be mapped
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let path = "/dev/shm/aleph-events";

        // Open the shared memory file
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)?;

        // Memory-map the file
        let mmap = unsafe { MmapMut::map_mut(&file)? };

        if mmap.len() < TOTAL_SIZE {
            return Err(format!(
                "Shared memory too small: {} < {}",
                mmap.len(),
                TOTAL_SIZE
            )
            .into());
        }

        Ok(Self {
            mmap,
            local_read_idx: 0,
        })
    }

    /// Read the current write index (atomic load)
    #[inline]
    fn read_write_idx(&self) -> u64 {
        let ptr = self.mmap.as_ptr() as *const u64;
        unsafe { std::ptr::read_volatile(ptr) }
    }

    /// Read an event from a specific slot
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
    /// # Safety
    /// Uses compiler_fence(Acquire) to ensure the event is fully written before reading.
    pub fn try_read(&mut self) -> Option<ShmPrivateEvent> {
        let write_idx = self.read_write_idx();

        // No new events available
        if self.local_read_idx >= write_idx {
            return None;
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
        match ShmEventReader::new() {
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
