//! Account Statistics Shared Memory Reader
//!
//! Reads account statistics from shared memory written by Go feeder.
//! Used for dynamic position sizing and risk management.

use memmap2::Mmap;
use std::fs::OpenOptions;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering, fence};

/// Account statistics from Lighter WebSocket
/// Layout must match Go's ShmAccountStats exactly
#[repr(C, align(128))]
pub struct ShmAccountStats {
    pub version: AtomicU64,       // 0..8
    pub collateral: f64,          // 8..16
    pub portfolio_value: f64,     // 16..24
    pub leverage: f64,            // 24..32
    pub available_balance: f64,   // 32..40
    pub margin_usage: f64,        // 40..48
    pub buying_power: f64,        // 48..56
    pub updated_at: u64,          // 56..64
    pub position: f64,            // 64..72 - Net position (positive=long, negative=short)
    pub position_updated_at: u64, // 72..80 - independent position stream timestamp
    _reserved: [u8; 48],          // 80..128
}

const ACCOUNT_STATS_SIZE: usize = 128;

pub struct AccountStatsReader {
    mmap: Mmap,
    local_version: u64,
}

impl AccountStatsReader {
    pub fn open(path: &str) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).open(path)?;
        // SAFETY: The producer owns the fixed-size file and must not truncate it live.
        let mmap = unsafe { Mmap::map(&file)? };

        if mmap.len() != ACCOUNT_STATS_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Expected {} bytes, got {}", ACCOUNT_STATS_SIZE, mmap.len()),
            ));
        }

        Ok(Self {
            mmap,
            local_version: 0,
        })
    }

    fn version(&self) -> u64 {
        // SAFETY: mmap is page-aligned and open() checks the complete wire layout.
        unsafe { (*self.mmap.as_ptr().cast::<AtomicU64>()).load(Ordering::Acquire) }
    }

    fn snapshot(&self) -> AccountStatsSnapshot {
        let ptr = self.mmap.as_ptr().cast::<ShmAccountStats>();
        // SAFETY: Each field is inside the validated mapping. Raw volatile reads
        // avoid creating a shared Rust reference to externally mutated payload.
        unsafe {
            AccountStatsSnapshot {
                collateral: std::ptr::addr_of!((*ptr).collateral).read_volatile(),
                portfolio_value: std::ptr::addr_of!((*ptr).portfolio_value).read_volatile(),
                leverage: std::ptr::addr_of!((*ptr).leverage).read_volatile(),
                available_balance: std::ptr::addr_of!((*ptr).available_balance).read_volatile(),
                margin_usage: std::ptr::addr_of!((*ptr).margin_usage).read_volatile(),
                buying_power: std::ptr::addr_of!((*ptr).buying_power).read_volatile(),
                position: std::ptr::addr_of!((*ptr).position).read_volatile(),
                updated_at: std::ptr::addr_of!((*ptr).updated_at).read_volatile(),
                position_updated_at: std::ptr::addr_of!((*ptr).position_updated_at).read_volatile(),
            }
        }
    }

    fn try_snapshot(&self) -> Option<(u64, AccountStatsSnapshot)> {
        let before = self.version();
        if before & 1 != 0 {
            return None;
        }
        let snapshot = self.snapshot();
        // Hardware load barrier: payload must precede the seqlock verification.
        fence(Ordering::Acquire);
        let after = self.version();
        (before == after && snapshot.is_finite()).then_some((after, snapshot))
    }

    /// Read a stable update, including a version reset after producer restart.
    pub fn read_if_updated(&mut self) -> Option<AccountStatsSnapshot> {
        let (version, snapshot) = self.try_snapshot()?;
        if version == self.local_version {
            return None;
        }
        self.local_version = version;
        Some(snapshot)
    }

    /// Read a coherent snapshot with a bounded retry count if the writer stalls.
    pub fn read(&mut self) -> AccountStatsSnapshot {
        for _ in 0..10_000 {
            if let Some((version, snapshot)) = self.try_snapshot() {
                self.local_version = version;
                return snapshot;
            }
            std::hint::spin_loop();
        }
        tracing::error!("AccountStats snapshot unavailable: writer stalled or invalid data");
        AccountStatsSnapshot::default()
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
    pub position: f64,
    pub updated_at: u64,
    pub position_updated_at: u64,
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
            position: 0.0,
            updated_at: 0,
            position_updated_at: 0,
        }
    }
}

impl AccountStatsSnapshot {
    fn is_finite(&self) -> bool {
        [
            self.collateral,
            self.portfolio_value,
            self.leverage,
            self.available_balance,
            self.margin_usage,
            self.buying_power,
            self.position,
        ]
        .iter()
        .all(|v| v.is_finite())
    }
}

const _: () = {
    assert!(std::mem::size_of::<ShmAccountStats>() == ACCOUNT_STATS_SIZE);
    assert!(std::mem::offset_of!(ShmAccountStats, position) == 64);
    assert!(std::mem::offset_of!(ShmAccountStats, position_updated_at) == 72);
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reader_never_resizes_input() {
        let path = std::env::temp_dir().join(format!("aleph-account-{}", uuid::Uuid::new_v4()));
        std::fs::write(&path, [0u8; 64]).unwrap();
        assert!(AccountStatsReader::open(path.to_str().unwrap()).is_err());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 64);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn reads_wire_fields_and_rejects_odd_or_invalid_snapshots() {
        let mut map = memmap2::MmapMut::map_anon(ACCOUNT_STATS_SIZE).unwrap();
        map[0..8].copy_from_slice(&2u64.to_ne_bytes());
        map[8..16].copy_from_slice(&100.0f64.to_ne_bytes());
        map[56..64].copy_from_slice(&123u64.to_ne_bytes());
        map[64..72].copy_from_slice(&(-0.25f64).to_ne_bytes());
        map[72..80].copy_from_slice(&456u64.to_ne_bytes());
        let mut reader = AccountStatsReader {
            mmap: map.make_read_only().unwrap(),
            local_version: 0,
        };
        let stats = reader.read_if_updated().unwrap();
        assert_eq!(stats.collateral, 100.0);
        assert_eq!(stats.position, -0.25);
        assert_eq!(stats.updated_at, 123);
        assert_eq!(stats.position_updated_at, 456);
        assert!(reader.read_if_updated().is_none());

        let mut map = memmap2::MmapMut::map_anon(ACCOUNT_STATS_SIZE).unwrap();
        map[0..8].copy_from_slice(&3u64.to_ne_bytes());
        reader.mmap = map.make_read_only().unwrap();
        assert!(reader.try_snapshot().is_none());
        let mut map = memmap2::MmapMut::map_anon(ACCOUNT_STATS_SIZE).unwrap();
        map[0..8].copy_from_slice(&4u64.to_ne_bytes());
        map[64..72].copy_from_slice(&f64::NAN.to_ne_bytes());
        reader.mmap = map.make_read_only().unwrap();
        assert!(reader.try_snapshot().is_none());
    }
}
