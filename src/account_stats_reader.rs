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

const _: () = {
    assert!(std::mem::size_of::<ShmAccountStats>() == ACCOUNT_STATS_SIZE);
    assert!(std::mem::offset_of!(ShmAccountStats, version) == 0);
    assert!(std::mem::offset_of!(ShmAccountStats, collateral) == 8);
    assert!(std::mem::offset_of!(ShmAccountStats, portfolio_value) == 16);
    assert!(std::mem::offset_of!(ShmAccountStats, leverage) == 24);
    assert!(std::mem::offset_of!(ShmAccountStats, available_balance) == 32);
    assert!(std::mem::offset_of!(ShmAccountStats, margin_usage) == 40);
    assert!(std::mem::offset_of!(ShmAccountStats, buying_power) == 48);
    assert!(std::mem::offset_of!(ShmAccountStats, updated_at) == 56);
    assert!(std::mem::offset_of!(ShmAccountStats, position) == 64);
    assert!(std::mem::offset_of!(ShmAccountStats, position_updated_at) == 72);
};

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
        let base = self.mmap.as_ptr();
        // SAFETY: open() validates mmap.len() == ACCOUNT_STATS_SIZE (128), mmap is page-aligned,
        // and compile-time offset_of! assertions confirm every field is 8-byte aligned.
        // Relaxed atomic loads avoid C11/Rust data-race UB when racing against a live writer
        // before fence(Ordering::Acquire) + version seqlock verification.
        let load_u64 = |offset: usize| -> u64 {
            debug_assert!(offset + 8 <= ACCOUNT_STATS_SIZE && offset.is_multiple_of(8));
            unsafe { (*base.byte_add(offset).cast::<AtomicU64>()).load(Ordering::Relaxed) }
        };
        AccountStatsSnapshot {
            collateral: f64::from_bits(load_u64(8)),
            portfolio_value: f64::from_bits(load_u64(16)),
            leverage: f64::from_bits(load_u64(24)),
            available_balance: f64::from_bits(load_u64(32)),
            margin_usage: f64::from_bits(load_u64(40)),
            buying_power: f64::from_bits(load_u64(48)),
            updated_at: load_u64(56),
            position: f64::from_bits(load_u64(64)),
            position_updated_at: load_u64(72),
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
