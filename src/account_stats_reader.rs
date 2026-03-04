//! Account Statistics Shared Memory Reader
//!
//! Reads account statistics from shared memory written by Go feeder.
//! Used for dynamic position sizing and risk management.

use memmap2::MmapMut;
use std::fs::OpenOptions;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};

/// Account statistics from Lighter WebSocket
/// Layout must match Go's ShmAccountStats exactly
#[repr(C, align(128))]
pub struct ShmAccountStats {
    pub version: AtomicU64,      // 0..8
    pub collateral: f64,         // 8..16
    pub portfolio_value: f64,    // 16..24
    pub leverage: f64,           // 24..32
    pub available_balance: f64,  // 32..40
    pub margin_usage: f64,       // 40..48
    pub buying_power: f64,       // 48..56
    pub updated_at: u64,         // 56..64
    _reserved: [u8; 64],         // 64..128
}

const ACCOUNT_STATS_SIZE: usize = 128;

pub struct AccountStatsReader {
    _mmap: MmapMut,
    stats: &'static ShmAccountStats,
    local_version: u64,
}

impl AccountStatsReader {
    pub fn open(path: &str) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)?;

        file.set_len(ACCOUNT_STATS_SIZE as u64)?;

        let mmap = unsafe { MmapMut::map_mut(&file)? };

        if mmap.len() != ACCOUNT_STATS_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Expected {} bytes, got {}", ACCOUNT_STATS_SIZE, mmap.len()),
            ));
        }

        let stats = unsafe { &*(mmap.as_ptr() as *const ShmAccountStats) };

        Ok(Self {
            _mmap: mmap,
            stats,
            local_version: 0,
        })
    }

    /// Read account stats if updated
    /// Returns Some(stats) if new data is available, None otherwise
    pub fn read_if_updated(&mut self) -> Option<AccountStatsSnapshot> {
        let remote_version = self.stats.version.load(Ordering::Acquire);

        // Check if version is even (write complete) and newer than local
        if remote_version.is_multiple_of(2) && remote_version > self.local_version {
            // Read all fields
            let snapshot = AccountStatsSnapshot {
                collateral: self.stats.collateral,
                portfolio_value: self.stats.portfolio_value,
                leverage: self.stats.leverage,
                available_balance: self.stats.available_balance,
                margin_usage: self.stats.margin_usage,
                buying_power: self.stats.buying_power,
                updated_at: self.stats.updated_at,
            };

            // Verify version didn't change during read
            let verify_version = self.stats.version.load(Ordering::Acquire);
            if verify_version == remote_version {
                self.local_version = remote_version;
                return Some(snapshot);
            }
        }

        None
    }

    /// Force read current stats (blocking until write completes)
    pub fn read(&mut self) -> AccountStatsSnapshot {
        loop {
            let version_before = self.stats.version.load(Ordering::Acquire);

            // Wait for even version (write complete)
            if !version_before.is_multiple_of(2) {
                std::hint::spin_loop();
                continue;
            }

            let snapshot = AccountStatsSnapshot {
                collateral: self.stats.collateral,
                portfolio_value: self.stats.portfolio_value,
                leverage: self.stats.leverage,
                available_balance: self.stats.available_balance,
                margin_usage: self.stats.margin_usage,
                buying_power: self.stats.buying_power,
                updated_at: self.stats.updated_at,
            };

            let version_after = self.stats.version.load(Ordering::Acquire);

            if version_before == version_after {
                self.local_version = version_after;
                return snapshot;
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct AccountStatsSnapshot {
    pub collateral: f64,
    pub portfolio_value: f64,
    pub leverage: f64,
    pub available_balance: f64,
    pub margin_usage: f64,
    pub buying_power: f64,
    pub updated_at: u64,
}

impl Default for AccountStatsSnapshot {
    fn default() -> Self {
        Self {
            collateral: 0.0,
            portfolio_value: 0.0,
            leverage: 0.0,
            available_balance: 0.0,
            margin_usage: 0.0,
            buying_power: 0.0,
            updated_at: 0,
        }
    }
}
