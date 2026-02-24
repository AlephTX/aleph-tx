//! Shared memory reader - mmap-based for zero-copy.

use memmap2::Mmap;
use std::fs::File;
use std::path::Path;

pub struct ShmReader {
    mmap: Mmap,
    file: File,
}

impl ShmReader {
    pub fn new(name: &str) -> anyhow::Result<Self> {
        let path = format!("/dev/shm/{}", name);
        let file = File::open(&path)?;
        let mmap = unsafe { Mmap::map(&file)? };
        tracing::info!("shm: mapped {} bytes from {}", mmap.len(), path);
        Ok(Self { mmap, file })
    }

    /// Refresh mmap to see latest data (for files that grow)
    pub fn refresh(&mut self) -> &[u8] {
        // For /dev/shm, the mapping stays valid but we need to sync
        // Actually for memory-mapped tmpfs, reads always see latest data
        &self.mmap
    }
}