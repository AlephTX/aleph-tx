//! Venue-agnostic shared-memory book & depth normalization layer.
//!
//! Wraps [`ShmReader`] (`/dev/shm/aleph-matrix`) and [`ShmDepthReader`]
//! (`/dev/shm/aleph-depth`) into neutral [`NormalizedBookSnapshot`] structs with
//! zero dependencies on any specific exchange client (`crate::exchanges::*`).

use crate::shm_depth_reader::ShmDepthReader;
use crate::shm_reader::{NUM_EXCHANGES, NUM_SYMBOLS, ShmReader};
use anyhow::{Result, ensure};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::ops::Deref;

/// Maximum L2 depth levels decoded per side on the stack (matches [`ShmDepthSnapshot`]).
pub const MAX_SHM_DEPTH_LEVELS: usize = 5;

/// Fixed-capacity stack-allocated `(price, size)` ladder eliminating heap allocations on SHM reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FixedPriceLevels {
    levels: [(Decimal, Decimal); MAX_SHM_DEPTH_LEVELS],
    len: u8,
}

impl Default for FixedPriceLevels {
    fn default() -> Self {
        Self {
            levels: [(Decimal::ZERO, Decimal::ZERO); MAX_SHM_DEPTH_LEVELS],
            len: 0,
        }
    }
}

impl FixedPriceLevels {
    #[inline]
    pub fn from_l1(px: Decimal, sz: Decimal) -> Self {
        let mut levels = [(Decimal::ZERO, Decimal::ZERO); MAX_SHM_DEPTH_LEVELS];
        levels[0] = (px, sz);
        Self { levels, len: 1 }
    }

    pub fn from_slice(src: &[(Decimal, Decimal)]) -> Option<Self> {
        if src.is_empty() || src.len() > MAX_SHM_DEPTH_LEVELS {
            return None;
        }
        let mut out = Self::default();
        for &(px, sz) in src {
            out.push(px, sz)?;
        }
        Some(out)
    }

    #[inline]
    pub fn push(&mut self, px: Decimal, sz: Decimal) -> Option<()> {
        let idx = usize::from(self.len);
        if idx >= MAX_SHM_DEPTH_LEVELS {
            return None;
        }
        self.levels[idx] = (px, sz);
        self.len += 1;
        Some(())
    }

    #[inline]
    pub fn as_slice(&self) -> &[(Decimal, Decimal)] {
        &self.levels[..usize::from(self.len)]
    }

    #[inline]
    pub fn len(&self) -> usize {
        usize::from(self.len)
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn to_vec(&self) -> Vec<(Decimal, Decimal)> {
        self.as_slice().to_vec()
    }
}

impl Deref for FixedPriceLevels {
    type Target = [(Decimal, Decimal)];

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.as_slice()
    }
}

/// Zero-allocation floating-point L2 price level for low-latency `f64` strategy pipelines.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct FastPriceLevel {
    pub price: f64,
    pub size: f64,
}

/// Stack-allocated `f64` L2 price ladder with ZERO `Vec` heap allocation and ZERO `Decimal` conversion.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FastPriceLevels {
    levels: [FastPriceLevel; MAX_SHM_DEPTH_LEVELS],
    len: u8,
}

impl Default for FastPriceLevels {
    fn default() -> Self {
        Self {
            levels: [FastPriceLevel {
                price: 0.0,
                size: 0.0,
            }; MAX_SHM_DEPTH_LEVELS],
            len: 0,
        }
    }
}

impl FastPriceLevels {
    #[inline]
    pub fn from_l1(price: f64, size: f64) -> Self {
        let mut levels = [FastPriceLevel {
            price: 0.0,
            size: 0.0,
        }; MAX_SHM_DEPTH_LEVELS];
        levels[0] = FastPriceLevel { price, size };
        Self { levels, len: 1 }
    }

    #[inline]
    fn push(&mut self, price: f64, size: f64) -> Option<()> {
        let idx = usize::from(self.len);
        if idx >= MAX_SHM_DEPTH_LEVELS {
            return None;
        }
        self.levels[idx] = FastPriceLevel { price, size };
        self.len += 1;
        Some(())
    }

    #[inline]
    pub fn as_slice(&self) -> &[FastPriceLevel] {
        &self.levels[..usize::from(self.len)]
    }

    #[inline]
    pub fn len(&self) -> usize {
        usize::from(self.len)
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Deref for FastPriceLevels {
    type Target = [FastPriceLevel];

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.as_slice()
    }
}

/// Zero-allocation, `Decimal`-free L1/L2 book snapshot for hot `f64` strategy evaluation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FastBookSnapshot {
    pub source_ms: u64,
    pub feeder_receipt_ms: Option<u64>,
    pub bid_px: f64,
    pub bid_sz: f64,
    pub ask_px: f64,
    pub ask_sz: f64,
    pub bids: FastPriceLevels,
    pub asks: FastPriceLevels,
}

fn default_bbo_shm_path() -> String {
    "/dev/shm/aleph-matrix".to_owned()
}
fn default_depth_shm_path() -> String {
    "/dev/shm/aleph-depth".to_owned()
}
fn default_primary_exchange_id() -> u8 {
    7
}
fn default_hedge_exchange_id() -> u8 {
    10
}

/// Venue-agnostic normalized L1/L2 book snapshot decoded from shared memory.
///
/// 64B `ShmBboMessage` V12 carries venue source timestamps (`source_ms`) only without an
/// independent feeder local receipt clock, so `feeder_receipt_ms` is `None` for V12 and must
/// never alias `source_ms` as local receipt time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NormalizedBookSnapshot {
    pub source_ms: u64,
    pub feeder_receipt_ms: Option<u64>,
    pub bid_px: Decimal,
    pub bid_sz: Decimal,
    pub ask_px: Decimal,
    pub ask_sz: Decimal,
    pub bids: FixedPriceLevels,
    pub asks: FixedPriceLevels,
}

/// Configuration for [`GenericShmBookFeed`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShmPairFeedConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_bbo_shm_path")]
    pub bbo_shm_path: String,
    #[serde(default = "default_depth_shm_path")]
    pub depth_shm_path: String,
    #[serde(default)]
    pub symbol_id: usize,
    #[serde(default = "default_primary_exchange_id")]
    pub primary_exchange_id: u8,
    #[serde(default = "default_hedge_exchange_id")]
    pub hedge_exchange_id: u8,
}

impl Default for ShmPairFeedConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bbo_shm_path: default_bbo_shm_path(),
            depth_shm_path: default_depth_shm_path(),
            symbol_id: 0,
            primary_exchange_id: default_primary_exchange_id(),
            hedge_exchange_id: default_hedge_exchange_id(),
        }
    }
}

impl ShmPairFeedConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.symbol_id < NUM_SYMBOLS,
            "shm_book_feed.symbol_id ({}) must be < {NUM_SYMBOLS}",
            self.symbol_id
        );
        ensure!(
            usize::from(self.primary_exchange_id) < NUM_EXCHANGES
                && usize::from(self.hedge_exchange_id) < NUM_EXCHANGES
                && self.primary_exchange_id != self.hedge_exchange_id,
            "shm_book_feed exchange IDs must be distinct and < {NUM_EXCHANGES}"
        );
        if self.enabled {
            ensure!(
                !self.bbo_shm_path.trim().is_empty() && !self.depth_shm_path.trim().is_empty(),
                "shm_book_feed paths must be non-empty when enabled"
            );
        }
        Ok(())
    }
}

/// Observable outcome of attempting to attach the optional L2 depth reader.
///
/// Distinguishes a depth stream that is live from one that is present-but-unreadable
/// (bad magic, short mmap, wrong size) so operators can tell WHY the feed degraded to
/// L1-only instead of a silently-dropped error. A depth failure never kills the BBO/L1
/// path; it only records why L2 is dark.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DepthStatus {
    /// L2 depth reader attached and will be consulted on each read.
    Attached,
    /// Depth mmap open failed (e.g. wrong magic or short/corrupt file); L1 BBO still serves.
    /// The string is the underlying open error for logging; never reproduces payload bytes.
    Unavailable(String),
}

/// Non-`Send` shared-memory BBO + optional L2 depth reader shared across venue adapters.
pub struct GenericShmBookFeed {
    bbo: ShmReader,
    depth: Option<ShmDepthReader>,
    depth_status: DepthStatus,
    symbol_id: u16,
    primary_exchange_id: u8,
    hedge_exchange_id: u8,
}

impl GenericShmBookFeed {
    /// Open BBO shared memory (and optional L2 depth) when configured; returns `Ok(None)`
    /// when disabled or returns `Err` on validation/mmap failure so callers can log and fall back to REST.
    ///
    /// A failure to attach the L2 depth reader is NOT fatal: the BBO/L1 path still opens and
    /// the reason is recorded in [`GenericShmBookFeed::depth_status`] so the degradation to
    /// L1-only is observable rather than silently swallowed.
    pub fn open(cfg: &ShmPairFeedConfig) -> Result<Option<Self>> {
        cfg.validate()?;
        if !cfg.enabled {
            return Ok(None);
        }
        let symbol_id = u16::try_from(cfg.symbol_id)?;
        let bbo = ShmReader::open(&cfg.bbo_shm_path, NUM_SYMBOLS)?;
        let (depth, depth_status) = match ShmDepthReader::open(&cfg.depth_shm_path, NUM_SYMBOLS) {
            Ok(reader) => (Some(reader), DepthStatus::Attached),
            Err(err) => (None, DepthStatus::Unavailable(err.to_string())),
        };
        Ok(Some(Self {
            bbo,
            depth,
            depth_status,
            symbol_id,
            primary_exchange_id: cfg.primary_exchange_id,
            hedge_exchange_id: cfg.hedge_exchange_id,
        }))
    }

    /// Why L2 depth is (or is not) available, so a caller can log the degradation reason.
    pub fn depth_status(&self) -> &DepthStatus {
        &self.depth_status
    }

    /// Synchronously read a paired `[primary, hedge]` book snapshot without holding `!Send` across `.await`.
    pub fn read_pair(&mut self) -> Option<(NormalizedBookSnapshot, NormalizedBookSnapshot)> {
        let slots = self.bbo.read_all_exchanges(self.symbol_id);
        let primary = self.decode_slot(&slots, self.primary_exchange_id)?;
        let hedge = self.decode_slot(&slots, self.hedge_exchange_id)?;
        Some((primary, hedge))
    }

    /// Synchronously read a single exchange slot as a normalized L1/L2 book snapshot.
    pub fn read_exchange(&mut self, exchange_id: u8) -> Option<NormalizedBookSnapshot> {
        if usize::from(exchange_id) >= NUM_EXCHANGES {
            return None;
        }
        let slots = self.bbo.read_all_exchanges(self.symbol_id);
        self.decode_slot(&slots, exchange_id)
    }

    /// Synchronously read a paired `[primary, hedge]` zero-allocation `f64` snapshot.
    pub fn read_pair_fast(&mut self) -> Option<(FastBookSnapshot, FastBookSnapshot)> {
        let slots = self.bbo.read_all_exchanges(self.symbol_id);
        let primary = self.decode_slot_fast(&slots, self.primary_exchange_id)?;
        let hedge = self.decode_slot_fast(&slots, self.hedge_exchange_id)?;
        Some((primary, hedge))
    }

    /// Synchronously read a single exchange zero-allocation `f64` snapshot.
    pub fn read_exchange_fast(&mut self, exchange_id: u8) -> Option<FastBookSnapshot> {
        if usize::from(exchange_id) >= NUM_EXCHANGES {
            return None;
        }
        let slots = self.bbo.read_all_exchanges(self.symbol_id);
        self.decode_slot_fast(&slots, exchange_id)
    }

    fn decode_slot(
        &self,
        slots: &[(u8, crate::shm_reader::ShmBboMessage); NUM_EXCHANGES],
        exchange_id: u8,
    ) -> Option<NormalizedBookSnapshot> {
        let fast = self.decode_slot_fast(slots, exchange_id)?;
        let bid_px = decimal_from_pos_f64(fast.bid_px)?;
        let bid_sz = decimal_from_pos_f64(fast.bid_sz)?;
        let ask_px = decimal_from_pos_f64(fast.ask_px)?;
        let ask_sz = decimal_from_pos_f64(fast.ask_sz)?;
        if ask_px <= bid_px {
            return None;
        }

        let mut bids = FixedPriceLevels::default();
        let mut asks = FixedPriceLevels::default();
        let mut depth_valid = true;
        for lvl in fast.bids.as_slice() {
            let (Some(px), Some(sz)) = (
                decimal_from_pos_f64(lvl.price),
                decimal_from_pos_f64(lvl.size),
            ) else {
                depth_valid = false;
                break;
            };
            if let Some(&(prev_px, _)) = bids.last()
                && prev_px <= px
            {
                depth_valid = false;
                break;
            }
            if bids.push(px, sz).is_none() {
                depth_valid = false;
                break;
            }
        }
        if depth_valid {
            for lvl in fast.asks.as_slice() {
                let (Some(px), Some(sz)) = (
                    decimal_from_pos_f64(lvl.price),
                    decimal_from_pos_f64(lvl.size),
                ) else {
                    depth_valid = false;
                    break;
                };
                if let Some(&(prev_px, _)) = asks.last()
                    && prev_px >= px
                {
                    depth_valid = false;
                    break;
                }
                if asks.push(px, sz).is_none() {
                    depth_valid = false;
                    break;
                }
            }
        }
        if !depth_valid
            || bids.is_empty()
            || asks.is_empty()
            || bids[0].0 != bid_px
            || asks[0].0 != ask_px
            || bids[0].0 >= asks[0].0
        {
            bids = FixedPriceLevels::from_l1(bid_px, bid_sz);
            asks = FixedPriceLevels::from_l1(ask_px, ask_sz);
        }

        Some(NormalizedBookSnapshot {
            source_ms: fast.source_ms,
            feeder_receipt_ms: fast.feeder_receipt_ms,
            bid_px,
            bid_sz,
            ask_px,
            ask_sz,
            bids,
            asks,
        })
    }

    fn decode_slot_fast(
        &self,
        slots: &[(u8, crate::shm_reader::ShmBboMessage); NUM_EXCHANGES],
        exchange_id: u8,
    ) -> Option<FastBookSnapshot> {
        let (_, msg) = slots[usize::from(exchange_id)];
        if msg.timestamp_ns == 0
            || msg.msg_type != 1
            || msg.exchange_id != exchange_id
            || msg.symbol_id != self.symbol_id
        {
            return None;
        }
        let slot_ms = msg.timestamp_ns / 1_000_000;
        if slot_ms == 0 {
            return None;
        }
        if !msg.bid_price.is_finite()
            || !msg.bid_size.is_finite()
            || !msg.ask_price.is_finite()
            || !msg.ask_size.is_finite()
            || msg.bid_price <= 0.0
            || msg.bid_size <= 0.0
            || msg.ask_price <= msg.bid_price
            || msg.ask_size <= 0.0
        {
            return None;
        }

        let (bids, asks) = self
            .depth
            .as_ref()
            .and_then(|depth_reader| depth_reader.read_depth(self.symbol_id, exchange_id))
            .and_then(|depth_snap| {
                validated_fast_depth_levels(
                    &depth_snap,
                    self.symbol_id,
                    exchange_id,
                    msg.timestamp_ns,
                    msg.bid_price,
                    msg.ask_price,
                )
            })
            .unwrap_or_else(|| {
                (
                    FastPriceLevels::from_l1(msg.bid_price, msg.bid_size),
                    FastPriceLevels::from_l1(msg.ask_price, msg.ask_size),
                )
            });

        Some(FastBookSnapshot {
            source_ms: slot_ms,
            feeder_receipt_ms: None,
            bid_px: msg.bid_price,
            bid_sz: msg.bid_size,
            ask_px: msg.ask_price,
            ask_sz: msg.ask_size,
            bids,
            asks,
        })
    }
}

fn parse_fast_depth_side(
    levels: &[crate::shm_depth_reader::PriceLevel],
    descending: bool,
) -> Option<FastPriceLevels> {
    let mut out = FastPriceLevels::default();
    let mut saw_zero_tail = false;
    for lvl in levels {
        if lvl.price == 0.0 && lvl.size == 0.0 {
            saw_zero_tail = true;
            continue;
        }
        if saw_zero_tail {
            return None;
        }
        if !lvl.price.is_finite() || !lvl.size.is_finite() || lvl.price <= 0.0 || lvl.size <= 0.0 {
            return None;
        }
        if let Some(prev) = out.last() {
            let monotonic = if descending {
                prev.price > lvl.price
            } else {
                prev.price < lvl.price
            };
            if !monotonic {
                return None;
            }
        }
        out.push(lvl.price, lvl.size)?;
    }
    (!out.is_empty()).then_some(out)
}

fn validated_fast_depth_levels(
    snap: &crate::shm_depth_reader::ShmDepthSnapshot,
    symbol_id: u16,
    exchange_id: u8,
    bbo_ts_ns: u64,
    bid_px: f64,
    ask_px: f64,
) -> Option<(FastPriceLevels, FastPriceLevels)> {
    if snap.symbol_id != symbol_id
        || snap.exchange_id != exchange_id
        || snap.timestamp_ns != bbo_ts_ns
    {
        return None;
    }
    let bids = parse_fast_depth_side(&snap.bids, true)?;
    let asks = parse_fast_depth_side(&snap.asks, false)?;
    if bids[0].price >= asks[0].price || bids[0].price != bid_px || asks[0].price != ask_px {
        return None;
    }
    Some((bids, asks))
}

fn decimal_from_pos_f64(val: f64) -> Option<Decimal> {
    if !val.is_finite() || val <= 0.0 {
        return None;
    }
    Decimal::from_f64_retain(val).filter(|d| *d > Decimal::ZERO)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shm_depth_reader::DEPTH_MAGIC_V12;
    use crate::shm_reader::SHM_MAGIC_V12;
    use std::fs;
    use std::path::PathBuf;

    struct TempFileGuard(PathBuf);
    impl Drop for TempFileGuard {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    #[test]
    fn generic_shm_book_feed_decodes_bbo_depth_and_rejects_crossed_or_zero_slots() {
        let dir = std::env::temp_dir();
        let uid = uuid::Uuid::new_v4();
        let bbo_path = dir.join(format!("generic_bbo_{uid}.shm"));
        let depth_path = dir.join(format!("generic_depth_{uid}.shm"));
        let _bbo_guard = TempFileGuard(bbo_path.clone());
        let _depth_guard = TempFileGuard(depth_path.clone());

        let version_hdr = NUM_SYMBOLS * 8;
        let bbo_total = version_hdr + NUM_SYMBOLS * NUM_EXCHANGES * 64;
        let mut bbo_buf = vec![0_u8; bbo_total];
        bbo_buf[version_hdr + 48..version_hdr + 64].copy_from_slice(&SHM_MAGIC_V12);

        let now_ms: u64 = 1_700_000_000_000;
        let write_bbo = |buf: &mut [u8], exch: u8, ts_ms: u64, bid: f64, ask: f64| {
            let off = version_hdr + usize::from(exch) * 64;
            buf[off..off + 4].copy_from_slice(&2_u32.to_le_bytes());
            buf[off + 4] = 1;
            buf[off + 5] = exch;
            buf[off + 6..off + 8].copy_from_slice(&0_u16.to_le_bytes());
            buf[off + 8..off + 16].copy_from_slice(&(ts_ms * 1_000_000).to_le_bytes());
            buf[off + 16..off + 24].copy_from_slice(&bid.to_le_bytes());
            buf[off + 24..off + 32].copy_from_slice(&5.0_f64.to_le_bytes());
            buf[off + 32..off + 40].copy_from_slice(&ask.to_le_bytes());
            buf[off + 40..off + 48].copy_from_slice(&4.0_f64.to_le_bytes());
        };
        write_bbo(&mut bbo_buf, 7, now_ms, 100.0, 100.1);
        write_bbo(&mut bbo_buf, 10, now_ms, 100.2, 100.3);
        fs::write(&bbo_path, &bbo_buf).unwrap();

        let depth_total = version_hdr + NUM_SYMBOLS * NUM_EXCHANGES * 256;
        let mut depth_buf = vec![0_u8; depth_total];
        depth_buf[version_hdr + 184..version_hdr + 200].copy_from_slice(&DEPTH_MAGIC_V12);
        let d_off = version_hdr + 10 * 256;
        depth_buf[d_off..d_off + 4].copy_from_slice(&2_u32.to_le_bytes());
        depth_buf[d_off + 4] = 10;
        depth_buf[d_off + 6..d_off + 8].copy_from_slice(&0_u16.to_le_bytes());
        depth_buf[d_off + 16..d_off + 24].copy_from_slice(&(now_ms * 1_000_000).to_le_bytes());
        for (i, (px, sz)) in [(100.2_f64, 5.0_f64), (100.1_f64, 10.0_f64)]
            .into_iter()
            .enumerate()
        {
            let p_off = d_off + 24 + i * 16;
            depth_buf[p_off..p_off + 8].copy_from_slice(&px.to_le_bytes());
            depth_buf[p_off + 8..p_off + 16].copy_from_slice(&sz.to_le_bytes());
        }
        for (i, (px, sz)) in [(100.3_f64, 4.0_f64), (100.4_f64, 8.0_f64)]
            .into_iter()
            .enumerate()
        {
            let p_off = d_off + 104 + i * 16;
            depth_buf[p_off..p_off + 8].copy_from_slice(&px.to_le_bytes());
            depth_buf[p_off + 8..p_off + 16].copy_from_slice(&sz.to_le_bytes());
        }
        fs::write(&depth_path, &depth_buf).unwrap();

        let cfg = ShmPairFeedConfig {
            enabled: true,
            bbo_shm_path: bbo_path.display().to_string(),
            depth_shm_path: depth_path.display().to_string(),
            symbol_id: 0,
            primary_exchange_id: 7,
            hedge_exchange_id: 10,
        };
        let mut feed = GenericShmBookFeed::open(&cfg).unwrap().unwrap();
        let (p, h) = feed.read_pair().expect("pair read should succeed");
        assert_eq!(p.source_ms, now_ms);
        assert!(p.feeder_receipt_ms.is_none());
        assert!(h.feeder_receipt_ms.is_none());
        assert_eq!(h.bids.len(), 2);
        assert_eq!(h.asks.len(), 2);

        // Crossed slot (bid >= ask) must return None fail-closed.
        write_bbo(&mut bbo_buf, 7, now_ms + 1, 100.5, 100.1);
        fs::write(&bbo_path, &bbo_buf).unwrap();
        let mut feed2 = GenericShmBookFeed::open(&cfg).unwrap().unwrap();
        assert!(feed2.read_pair().is_none());
    }

    #[test]
    fn generic_shm_book_feed_rejects_corrupted_crossed_or_mismatched_depth_falling_back_to_bbo() {
        let dir = std::env::temp_dir();
        let uid = uuid::Uuid::new_v4();
        let bbo_path = dir.join(format!("generic_bbo_depth_fb_{uid}.shm"));
        let depth_path = dir.join(format!("generic_depth_fb_{uid}.shm"));
        let _bbo_guard = TempFileGuard(bbo_path.clone());
        let _depth_guard = TempFileGuard(depth_path.clone());

        let version_hdr = NUM_SYMBOLS * 8;
        let bbo_total = version_hdr + NUM_SYMBOLS * NUM_EXCHANGES * 64;
        let mut bbo_buf = vec![0_u8; bbo_total];
        bbo_buf[version_hdr + 48..version_hdr + 64].copy_from_slice(&SHM_MAGIC_V12);

        let now_ms: u64 = 1_700_000_000_000;
        for (exch, bid, ask) in [(7_u8, 100.0_f64, 100.1_f64), (10_u8, 100.2_f64, 100.3_f64)] {
            let off = version_hdr + usize::from(exch) * 64;
            bbo_buf[off..off + 4].copy_from_slice(&2_u32.to_le_bytes());
            bbo_buf[off + 4] = 1;
            bbo_buf[off + 5] = exch;
            bbo_buf[off + 6..off + 8].copy_from_slice(&0_u16.to_le_bytes());
            bbo_buf[off + 8..off + 16].copy_from_slice(&(now_ms * 1_000_000).to_le_bytes());
            bbo_buf[off + 16..off + 24].copy_from_slice(&bid.to_le_bytes());
            bbo_buf[off + 24..off + 32].copy_from_slice(&5.0_f64.to_le_bytes());
            bbo_buf[off + 32..off + 40].copy_from_slice(&ask.to_le_bytes());
            bbo_buf[off + 40..off + 48].copy_from_slice(&4.0_f64.to_le_bytes());
        }
        fs::write(&bbo_path, &bbo_buf).unwrap();

        let depth_total = version_hdr + NUM_SYMBOLS * NUM_EXCHANGES * 256;
        let mut depth_buf = vec![0_u8; depth_total];
        depth_buf[version_hdr + 184..version_hdr + 200].copy_from_slice(&DEPTH_MAGIC_V12);
        let d_off = version_hdr + 10 * 256;
        depth_buf[d_off..d_off + 4].copy_from_slice(&2_u32.to_le_bytes());
        depth_buf[d_off + 4] = 10;
        depth_buf[d_off + 6..d_off + 8].copy_from_slice(&0_u16.to_le_bytes());
        depth_buf[d_off + 16..d_off + 24].copy_from_slice(&(now_ms * 1_000_000).to_le_bytes());

        // Case 1: unsorted bid ladder ([100.1, 100.2]) -> falls back safely to L1 BBO [(100.2, 5.0)].
        for (i, (px, sz)) in [(100.1_f64, 5.0_f64), (100.2_f64, 10.0_f64)]
            .into_iter()
            .enumerate()
        {
            let p_off = d_off + 24 + i * 16;
            depth_buf[p_off..p_off + 8].copy_from_slice(&px.to_le_bytes());
            depth_buf[p_off + 8..p_off + 16].copy_from_slice(&sz.to_le_bytes());
        }
        for (i, (px, sz)) in [(100.3_f64, 4.0_f64), (100.4_f64, 8.0_f64)]
            .into_iter()
            .enumerate()
        {
            let p_off = d_off + 104 + i * 16;
            depth_buf[p_off..p_off + 8].copy_from_slice(&px.to_le_bytes());
            depth_buf[p_off + 8..p_off + 16].copy_from_slice(&sz.to_le_bytes());
        }
        fs::write(&depth_path, &depth_buf).unwrap();

        let cfg = ShmPairFeedConfig {
            enabled: true,
            bbo_shm_path: bbo_path.display().to_string(),
            depth_shm_path: depth_path.display().to_string(),
            symbol_id: 0,
            primary_exchange_id: 7,
            hedge_exchange_id: 10,
        };
        let mut feed = GenericShmBookFeed::open(&cfg).unwrap().unwrap();
        let (_, h) = feed.read_pair().expect("pair should fall back to valid L1");
        assert_eq!(h.bids.len(), 1);
        assert_eq!(h.asks.len(), 1);
        assert_eq!(h.bids[0].0, Decimal::from_f64_retain(100.2).unwrap());

        // Case 2: NaN at bid level 0 must NOT silently promote level 1 into bids[0].
        depth_buf[d_off + 24..d_off + 32].copy_from_slice(&f64::NAN.to_le_bytes());
        depth_buf[d_off + 40..d_off + 48].copy_from_slice(&100.2_f64.to_le_bytes());
        fs::write(&depth_path, &depth_buf).unwrap();
        let (_, h2) = feed.read_pair().expect("NaN L0 depth falls back to L1");
        assert_eq!(h2.bids.len(), 1);
        assert_eq!(h2.bids[0].0, Decimal::from_f64_retain(100.2).unwrap());

        // Case 3: Depth timestamp mismatch vs L1 timestamp falls back cleanly to L1.
        for (i, (px, sz)) in [(100.2_f64, 5.0_f64), (100.1_f64, 10.0_f64)]
            .into_iter()
            .enumerate()
        {
            let p_off = d_off + 24 + i * 16;
            depth_buf[p_off..p_off + 8].copy_from_slice(&px.to_le_bytes());
            depth_buf[p_off + 8..p_off + 16].copy_from_slice(&sz.to_le_bytes());
        }
        depth_buf[d_off + 16..d_off + 24]
            .copy_from_slice(&((now_ms + 5) * 1_000_000).to_le_bytes());
        fs::write(&depth_path, &depth_buf).unwrap();
        let (_, h3) = feed
            .read_pair()
            .expect("mismatched-clock L2 falls back to L1");
        assert_eq!(h3.bids.len(), 1);
        assert_eq!(h3.asks.len(), 1);
    }

    #[test]
    fn corrupt_depth_magic_surfaces_unavailable_status_but_keeps_l1_bbo() {
        let dir = std::env::temp_dir();
        let uid = uuid::Uuid::new_v4();
        let bbo_path = dir.join(format!("generic_bbo_badmagic_{uid}.shm"));
        let depth_path = dir.join(format!("generic_depth_badmagic_{uid}.shm"));
        let _bbo_guard = TempFileGuard(bbo_path.clone());
        let _depth_guard = TempFileGuard(depth_path.clone());

        let version_hdr = NUM_SYMBOLS * 8;
        let bbo_total = version_hdr + NUM_SYMBOLS * NUM_EXCHANGES * 64;
        let mut bbo_buf = vec![0_u8; bbo_total];
        bbo_buf[version_hdr + 48..version_hdr + 64].copy_from_slice(&SHM_MAGIC_V12);

        let now_ms: u64 = 1_700_000_000_000;
        for (exch, bid, ask) in [(7_u8, 100.0_f64, 100.1_f64), (10_u8, 100.2_f64, 100.3_f64)] {
            let off = version_hdr + usize::from(exch) * 64;
            bbo_buf[off..off + 4].copy_from_slice(&2_u32.to_le_bytes());
            bbo_buf[off + 4] = 1;
            bbo_buf[off + 5] = exch;
            bbo_buf[off + 6..off + 8].copy_from_slice(&0_u16.to_le_bytes());
            bbo_buf[off + 8..off + 16].copy_from_slice(&(now_ms * 1_000_000).to_le_bytes());
            bbo_buf[off + 16..off + 24].copy_from_slice(&bid.to_le_bytes());
            bbo_buf[off + 24..off + 32].copy_from_slice(&5.0_f64.to_le_bytes());
            bbo_buf[off + 32..off + 40].copy_from_slice(&ask.to_le_bytes());
            bbo_buf[off + 40..off + 48].copy_from_slice(&4.0_f64.to_le_bytes());
        }
        fs::write(&bbo_path, &bbo_buf).unwrap();

        // Correct size depth file, but WRONG magic -> ShmDepthReader::open returns Err.
        let depth_total = version_hdr + NUM_SYMBOLS * NUM_EXCHANGES * 256;
        let mut depth_buf = vec![0_u8; depth_total];
        depth_buf[version_hdr + 184..version_hdr + 200].copy_from_slice(b"ALEPHTX_DEP_BAD\0");
        fs::write(&depth_path, &depth_buf).unwrap();

        let cfg = ShmPairFeedConfig {
            enabled: true,
            bbo_shm_path: bbo_path.display().to_string(),
            depth_shm_path: depth_path.display().to_string(),
            symbol_id: 0,
            primary_exchange_id: 7,
            hedge_exchange_id: 10,
        };
        let mut feed = GenericShmBookFeed::open(&cfg).unwrap().unwrap();

        // Degradation is observable with a reason rather than a silent None.
        match feed.depth_status() {
            DepthStatus::Unavailable(reason) => assert!(!reason.is_empty()),
            DepthStatus::Attached => panic!("corrupt-magic depth must not report Attached"),
        }

        // BBO/L1 path still serves; snapshot degrades to a single L1 level per side.
        let (p, h) = feed
            .read_pair()
            .expect("L1 BBO still readable with dark L2");
        assert_eq!(p.source_ms, now_ms);
        assert!(p.feeder_receipt_ms.is_none());
        assert_eq!(h.bids.len(), 1);
        assert_eq!(h.asks.len(), 1);
    }

    #[test]
    fn fast_pair_reader_decodes_stack_f64_levels_and_falls_back_cleanly() {
        let dir = std::env::temp_dir();
        let uid = uuid::Uuid::new_v4();
        let bbo_path = dir.join(format!("generic_bbo_fast_{uid}.shm"));
        let depth_path = dir.join(format!("generic_depth_fast_{uid}.shm"));
        let _bbo_guard = TempFileGuard(bbo_path.clone());
        let _depth_guard = TempFileGuard(depth_path.clone());

        let version_hdr = NUM_SYMBOLS * 8;
        let bbo_total = version_hdr + NUM_SYMBOLS * NUM_EXCHANGES * 64;
        let mut bbo_buf = vec![0_u8; bbo_total];
        bbo_buf[version_hdr + 48..version_hdr + 64].copy_from_slice(&SHM_MAGIC_V12);

        let now_ms: u64 = 1_700_000_000_000;
        for (exch, bid, ask) in [(7_u8, 100.0_f64, 100.1_f64), (10_u8, 100.2_f64, 100.3_f64)] {
            let off = version_hdr + usize::from(exch) * 64;
            bbo_buf[off..off + 4].copy_from_slice(&2_u32.to_le_bytes());
            bbo_buf[off + 4] = 1;
            bbo_buf[off + 5] = exch;
            bbo_buf[off + 6..off + 8].copy_from_slice(&0_u16.to_le_bytes());
            bbo_buf[off + 8..off + 16].copy_from_slice(&(now_ms * 1_000_000).to_le_bytes());
            bbo_buf[off + 16..off + 24].copy_from_slice(&bid.to_le_bytes());
            bbo_buf[off + 24..off + 32].copy_from_slice(&5.0_f64.to_le_bytes());
            bbo_buf[off + 32..off + 40].copy_from_slice(&ask.to_le_bytes());
            bbo_buf[off + 40..off + 48].copy_from_slice(&4.0_f64.to_le_bytes());
        }
        fs::write(&bbo_path, &bbo_buf).unwrap();

        let depth_total = version_hdr + NUM_SYMBOLS * NUM_EXCHANGES * 256;
        let mut depth_buf = vec![0_u8; depth_total];
        depth_buf[version_hdr + 184..version_hdr + 200].copy_from_slice(&DEPTH_MAGIC_V12);
        let d_off = version_hdr + 10 * 256;
        depth_buf[d_off..d_off + 4].copy_from_slice(&2_u32.to_le_bytes());
        depth_buf[d_off + 4] = 10;
        depth_buf[d_off + 6..d_off + 8].copy_from_slice(&0_u16.to_le_bytes());
        depth_buf[d_off + 16..d_off + 24].copy_from_slice(&(now_ms * 1_000_000).to_le_bytes());
        for (i, (px, sz)) in [(100.2_f64, 5.0_f64), (100.1_f64, 9.0_f64)]
            .into_iter()
            .enumerate()
        {
            let p_off = d_off + 24 + i * 16;
            depth_buf[p_off..p_off + 8].copy_from_slice(&px.to_le_bytes());
            depth_buf[p_off + 8..p_off + 16].copy_from_slice(&sz.to_le_bytes());
        }
        for (i, (px, sz)) in [(100.3_f64, 4.0_f64), (100.4_f64, 8.0_f64)]
            .into_iter()
            .enumerate()
        {
            let p_off = d_off + 104 + i * 16;
            depth_buf[p_off..p_off + 8].copy_from_slice(&px.to_le_bytes());
            depth_buf[p_off + 8..p_off + 16].copy_from_slice(&sz.to_le_bytes());
        }
        fs::write(&depth_path, &depth_buf).unwrap();

        let cfg = ShmPairFeedConfig {
            enabled: true,
            bbo_shm_path: bbo_path.display().to_string(),
            depth_shm_path: depth_path.display().to_string(),
            symbol_id: 0,
            primary_exchange_id: 7,
            hedge_exchange_id: 10,
        };
        let mut feed = GenericShmBookFeed::open(&cfg).unwrap().unwrap();
        let (p_fast, h_fast) = feed.read_pair_fast().expect("fast read should succeed");
        let (p_dec, h_dec) = feed.read_pair().expect("decimal read should succeed");
        assert_eq!(p_fast.source_ms, p_dec.source_ms);
        assert!(p_fast.feeder_receipt_ms.is_none());
        assert_eq!(h_fast.bids.len(), 2);
        assert_eq!(h_fast.asks.len(), 2);
        assert_eq!(h_dec.bids.len(), 2);
        assert_eq!(h_fast.bids[1].price, 100.1);
        assert_eq!(h_fast.asks[1].size, 8.0);
    }
}
