//! Shared memory reader for IPC with Go feeder - uses mmap for zero-copy reads.

use std::fs::File;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Memory-mapped reader - sees Go's writes in real-time.
pub struct ShmReader {
    data: memmap2::Mmap,
    roff: AtomicUsize,
}

impl ShmReader {
    pub fn new(name: &str, capacity: usize) -> anyhow::Result<Self> {
        let path = format!("/dev/shm/{}", name);
        let file = File::open(&path)?;
        let mmap = unsafe { memmap2::Mmap::map(&file)? };
        tracing::info!("shm: mapped {} bytes from {}", mmap.len(), path);
        Ok(Self { 
            data: mmap, 
            roff: AtomicUsize::new(0),
        })
    }

    /// Try to read one message. Returns (msg_type, payload) or None if empty.
    pub fn try_read(&mut self) -> Option<(u8, &[u8])> {
        let roff = self.roff.load(Ordering::Acquire);
        
        if roff >= self.data.len() {
            // Reset for simplicity (not a true ring, but works for now)
            self.roff.store(0, Ordering::Release);
            return None;
        }

        let pos = roff;
        if pos >= self.data.len() {
            return None;
        }

        let msg_type = self.data[pos];
        if msg_type == 0 {
            // Empty slot, try again later
            std::thread::sleep(std::time::Duration::from_micros(100));
            return None;
        }

        if pos + 3 > self.data.len() {
            self.roff.store(0, Ordering::Release);
            return None;
        }

        let msg_len = u16::from_le_bytes([self.data[pos + 1], self.data[pos + 2]]) as usize;
        if msg_len == 0 || pos + 3 + msg_len > self.data.len() {
            self.roff.store(0, Ordering::Release);
            return None;
        }

        let payload_start = pos + 3;
        let payload_end = payload_start + msg_len;
        let payload = &self.data[payload_start..payload_end];

        // Clear slot and advance
        // Note: in a real ring we'd mark this as read, but for simplicity just advance
        self.roff.store(payload_end, Ordering::Release);

        Some((msg_type, payload))
    }
}
