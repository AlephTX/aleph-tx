//! Order Request Ring Buffer
//!
//! This module implements a lock-free ring buffer for sending order requests
//! from Rust to Go. Go will execute orders using the lighter-go SDK with
//! proper Poseidon2 + Schnorr authentication.
//!
//! # Architecture
//!
//! ```text
//! Rust Strategy --> Order Request Buffer --> Go Feeder --> Lighter API
//!                                                ^
//!                                                |
//!                                         (Poseidon2 + Schnorr)
//! ```

use std::sync::atomic::{compiler_fence, Ordering};

const RING_BUFFER_SLOTS: u64 = 256;
const REQUEST_SIZE: usize = 64;
const HEADER_SIZE: usize = 64;

/// Order request types
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderRequestType {
    PlaceLimit = 1,
    Cancel = 2,
}

/// Order side
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderRequestSide {
    Buy = 1,
    Sell = 2,
}

/// Order request structure (64 bytes, cache-line aligned)
#[repr(C, align(64))]
#[derive(Debug, Clone, Copy)]
pub struct OrderRequest {
    pub sequence: u64,        // Request sequence number
    pub request_type: u8,     // OrderRequestType
    pub side: u8,             // OrderRequestSide (for PlaceLimit)
    pub market_id: u16,       // Market ID
    pub symbol_id: u16,       // Symbol ID
    pub _padding1: u16,
    pub price: f64,           // Limit price (for PlaceLimit)
    pub size: f64,            // Order size (for PlaceLimit)
    pub order_id: u64,        // Order ID (for Cancel)
    pub _padding2: [u8; 16],
}

impl OrderRequest {
    /// Create a new place limit order request
    pub fn place_limit(
        sequence: u64,
        market_id: u16,
        symbol_id: u16,
        side: OrderRequestSide,
        price: f64,
        size: f64,
    ) -> Self {
        Self {
            sequence,
            request_type: OrderRequestType::PlaceLimit as u8,
            side: side as u8,
            market_id,
            symbol_id,
            _padding1: 0,
            price,
            size,
            order_id: 0,
            _padding2: [0; 16],
        }
    }

    /// Create a new cancel order request
    pub fn cancel(sequence: u64, market_id: u16, symbol_id: u16, order_id: u64) -> Self {
        Self {
            sequence,
            request_type: OrderRequestType::Cancel as u8,
            side: 0,
            market_id,
            symbol_id,
            _padding1: 0,
            price: 0.0,
            size: 0.0,
            order_id,
            _padding2: [0; 16],
        }
    }
}

/// Order request ring buffer writer (Rust side)
pub struct OrderRequestWriter {
    mmap: memmap2::MmapMut,
    local_write_idx: u64,
}

impl OrderRequestWriter {
    /// Create a new order request writer
    pub fn new(path: &str) -> std::io::Result<Self> {
        use std::fs::OpenOptions;

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;

        let total_size = HEADER_SIZE + (RING_BUFFER_SLOTS as usize * REQUEST_SIZE);
        file.set_len(total_size as u64)?;

        let mmap = unsafe { memmap2::MmapMut::map_mut(&file)? };

        Ok(Self {
            mmap,
            local_write_idx: 0,
        })
    }

    /// Push an order request to the ring buffer
    ///
    /// # Safety
    ///
    /// Uses volatile write and release fence to ensure the request is visible
    /// to the Go reader before updating the write index.
    pub fn push(&mut self, request: OrderRequest) {
        // Calculate slot index
        let slot = (self.local_write_idx % RING_BUFFER_SLOTS) as usize;
        let offset = HEADER_SIZE + (slot * REQUEST_SIZE);

        // Write request to slot (volatile)
        let ptr = unsafe { self.mmap.as_mut_ptr().add(offset) as *mut OrderRequest };
        unsafe {
            std::ptr::write_volatile(ptr, request);
        }

        // Release fence: ensure request is written before updating write_idx
        compiler_fence(Ordering::Release);

        // Update write index (atomic)
        self.local_write_idx += 1;
        let write_idx_ptr = self.mmap.as_mut_ptr() as *mut u64;
        unsafe {
            std::ptr::write_volatile(write_idx_ptr, self.local_write_idx);
        }
    }

    /// Get current write index
    pub fn write_idx(&self) -> u64 {
        self.local_write_idx
    }
}
