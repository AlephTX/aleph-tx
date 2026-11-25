//! Privacy-preserving public account toxicity shield (`S1-4`, arXiv:2611.05218).
//!
//! Stores only keyed 64-bit salted hashes of public exchange account IDs inside an
//! in-memory bounded table (leaving the fixed 128-byte `ShmAccountStats` ABI untouched
//! and never logging raw wallet identifiers). Fades the threatened opening/entry quote side
//! for 1 second upon D9/D10 toxic sweeps while never throttling `reduce_only` exits or
//! benign `twap-sub` flow.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};

/// Deterministic keyed 64-bit hash (`FNV-1a` mixed with splitmix64 finalizer) so raw
/// exchange account IDs never persist in telemetry, logs, or strategy tables.
pub fn salted_wallet_key(salt: u64, account_id: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325 ^ salt.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    for &b in account_id.trim().as_bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h ^= h >> 30;
    h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94d0_49bb_1331_11eb);
    h ^ (h >> 31)
}

/// Offline-calibrated toxicity classification assigned to a hashed wallet key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WalletToxicityTier {
    Unknown,
    BenignTwapSub,
    /// Decile `1..=10` ranked by trailing 10-day volume-weighted 10s markout (`D10` = worst).
    Decile(u8),
}

impl WalletToxicityTier {
    pub fn validate(self) -> Result<()> {
        if let Self::Decile(d) = self {
            ensure!(
                (1..=10).contains(&d),
                "toxicity decile must be in 1..=10, got {d}"
            );
        }
        Ok(())
    }
}

/// Quote side (`0 = Bid`, `1 = Ask`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum QuoteSide {
    Bid = 0,
    Ask = 1,
}

/// Configuration for [`WalletToxicityShield`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WalletToxicityConfig {
    /// Duration (ms) after a D9/D10 taker sweep to fade the threatened side (`default 1_000`).
    pub fade_window_ms: u64,
    /// Half-spread multiplier when active sweep came from a D9 wallet (`default 2.0`).
    pub d9_spread_mult: f64,
    /// Half-spread multiplier when active sweep came from a D10 wallet (`default 3.0`).
    pub d10_spread_mult: f64,
    /// Opening quote size scale while faded (`default 0.25`).
    pub toxic_size_scale: f64,
    /// Bounded LRU/FIFO capacity for hashed wallet entries (`default 4_096`).
    pub max_tracked_wallets: usize,
    /// Maximum forward jump (ms) a single sweep timestamp may advance the watermark
    /// (`default 5_000`). A sweep dated further ahead than `last_observed_ms + this` is
    /// treated as a clock glitch / spoofed print and ignored, so one absurd future
    /// timestamp can never permanently latch the watermark and disable the shield.
    pub max_future_skew_ms: u64,
}

impl Default for WalletToxicityConfig {
    fn default() -> Self {
        Self {
            fade_window_ms: 1_000,
            d9_spread_mult: 2.0,
            d10_spread_mult: 3.0,
            toxic_size_scale: 0.25,
            max_tracked_wallets: 4_096,
            max_future_skew_ms: 5_000,
        }
    }
}

impl WalletToxicityConfig {
    /// Fail closed on invalid multipliers, scales, window durations, or capacity.
    pub fn validate(&self) -> Result<()> {
        ensure!(self.fade_window_ms > 0, "fade_window_ms must be > 0");
        ensure!(
            self.d9_spread_mult.is_finite()
                && self.d10_spread_mult.is_finite()
                && self.d9_spread_mult >= 1.0
                && self.d10_spread_mult >= self.d9_spread_mult,
            "spread multipliers must be finite with 1.0 <= d9_spread_mult <= d10_spread_mult"
        );
        ensure!(
            self.toxic_size_scale.is_finite()
                && self.toxic_size_scale > 0.0
                && self.toxic_size_scale <= 1.0,
            "toxic_size_scale must be finite and in (0.0, 1.0]"
        );
        ensure!(
            self.max_tracked_wallets >= 1,
            "max_tracked_wallets must be >= 1"
        );
        ensure!(
            self.max_future_skew_ms > 0,
            "max_future_skew_ms must be > 0"
        );
        Ok(())
    }
}

/// Output quote adjustment for one side from [`WalletToxicityShield::quote_adjustment`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct QuoteToxicityAdjustment {
    pub spread_multiplier: f64,
    pub size_scale: f64,
    pub active_decile: Option<u8>,
    pub faded: bool,
}

#[derive(Debug, Clone, Copy, Default)]
struct SideLatch {
    triggered_at_ms: Option<u64>,
    decile: u8,
}

/// Bounded privacy-preserving wallet toxicity supervisor.
#[derive(Debug, Clone)]
pub struct WalletToxicityShield {
    config: WalletToxicityConfig,
    tiers: HashMap<u64, WalletToxicityTier>,
    insertion_order: VecDeque<u64>,
    last_observed_ms: u64,
    side_latches: [SideLatch; 2],
}

impl WalletToxicityShield {
    pub fn new(config: WalletToxicityConfig) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            config,
            tiers: HashMap::with_capacity(config.max_tracked_wallets.min(1024)),
            insertion_order: VecDeque::with_capacity(config.max_tracked_wallets.min(1024)),
            last_observed_ms: 0,
            side_latches: [SideLatch::default(); 2],
        })
    }

    pub fn tracked_wallet_count(&self) -> usize {
        self.tiers.len()
    }

    /// Assign or update a wallet tier under a bounded FIFO cap.
    pub fn upsert_wallet_tier(&mut self, wallet_key: u64, tier: WalletToxicityTier) -> Result<()> {
        tier.validate()?;
        if !self.tiers.contains_key(&wallet_key) {
            while self.tiers.len() >= self.config.max_tracked_wallets {
                if let Some(oldest) = self.insertion_order.pop_front() {
                    self.tiers.remove(&oldest);
                } else {
                    break;
                }
            }
            self.insertion_order.push_back(wallet_key);
        }
        self.tiers.insert(wallet_key, tier);
        Ok(())
    }

    /// Process a public taker trade event causally and trigger quote fading when
    /// a tracked D9/D10 wallet sweeps liquidity. Returns the threatened [`QuoteSide`]
    /// if a fade latch was updated.
    ///
    /// Clock handling is fail-open for liveness, fail-safe for the watermark: a public
    /// taker feed can deliver out-of-order or future-dated prints (multi-lane/sharded feeds,
    /// or spoofed timestamps). Such events are DROPPED (benign `Ok(None)`) rather than
    /// erroring, so a single bad print never disrupts the quote cycle. Critically, the
    /// `last_observed_ms` watermark advances ONLY for events that pass the twap/tier gating
    /// and are actually processed — so a far-future print from an ignored cohort (benign
    /// twap-sub, non-D9/D10, or beyond `max_future_skew_ms`) can never latch the watermark
    /// forward and permanently disable detection of later genuine sweeps.
    pub fn observe_taker_sweep(
        &mut self,
        now_ms: u64,
        wallet_key: u64,
        taker_is_buy: bool,
        is_twap_sub: bool,
    ) -> Result<Option<QuoteSide>> {
        ensure!(now_ms > 0, "taker sweep timestamp must be > 0");
        // Once a baseline watermark exists, reject regressed and implausibly-far-future prints.
        // Before the first accepted event (watermark == 0) there is no baseline to judge skew
        // against, so the first in-cohort sweep bootstraps the clock unconditionally.
        if self.last_observed_ms > 0 {
            // Regressed print: ignore for cooldown/latch, do not error, do not advance watermark.
            if now_ms < self.last_observed_ms {
                return Ok(None);
            }
            // Future print beyond the bounded skew: treat as a clock glitch / spoof. Ignore it
            // and do NOT advance the watermark, so it cannot lock out later real-time events.
            if now_ms
                > self
                    .last_observed_ms
                    .saturating_add(self.config.max_future_skew_ms)
            {
                return Ok(None);
            }
        }

        if is_twap_sub {
            return Ok(None);
        }
        let tier = self
            .tiers
            .get(&wallet_key)
            .copied()
            .unwrap_or(WalletToxicityTier::Unknown);
        let WalletToxicityTier::Decile(decile) = tier else {
            return Ok(None);
        };
        if decile < 9 {
            return Ok(None);
        }

        // Accepted, in-cohort D9/D10 sweep: now it is safe to advance the watermark.
        self.last_observed_ms = now_ms;

        // An informed buyer sweep threatens resting asks; an informed seller sweep threatens bids.
        let threatened_side = if taker_is_buy {
            QuoteSide::Ask
        } else {
            QuoteSide::Bid
        };
        let slot = &mut self.side_latches[threatened_side as usize];
        let keep_higher = slot
            .triggered_at_ms
            .is_some_and(|prev| now_ms.saturating_sub(prev) <= self.config.fade_window_ms)
            && slot.decile > decile;
        if !keep_higher {
            slot.decile = decile;
        }
        slot.triggered_at_ms = Some(now_ms);
        Ok(Some(threatened_side))
    }

    /// Compute quote spread multiplier & size scale for `side`. Never throttles
    /// `is_reducing_side = true` so unwind priority is always preserved.
    #[inline]
    pub fn quote_adjustment(
        &self,
        now_ms: u64,
        side: QuoteSide,
        is_reducing_side: bool,
    ) -> QuoteToxicityAdjustment {
        if is_reducing_side {
            return QuoteToxicityAdjustment {
                spread_multiplier: 1.0,
                size_scale: 1.0,
                active_decile: None,
                faded: false,
            };
        }
        let latch = self.side_latches[side as usize];
        if let Some(triggered_at) = latch.triggered_at_ms
            && now_ms >= triggered_at
            && now_ms - triggered_at <= self.config.fade_window_ms
        {
            let mult = if latch.decile >= 10 {
                self.config.d10_spread_mult
            } else {
                self.config.d9_spread_mult
            };
            return QuoteToxicityAdjustment {
                spread_multiplier: mult,
                size_scale: self.config.toxic_size_scale,
                active_decile: Some(latch.decile),
                faded: true,
            };
        }
        QuoteToxicityAdjustment {
            spread_multiplier: 1.0,
            size_scale: 1.0,
            active_decile: None,
            faded: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn salted_key_is_deterministic_and_opaque() {
        let k1 = salted_wallet_key(0x5555_aaaa, "12345678");
        let k2 = salted_wallet_key(0x5555_aaaa, " 12345678 ");
        let k3 = salted_wallet_key(0x9999_1111, "12345678");
        assert_eq!(k1, k2);
        assert_ne!(k1, k3);
        assert!(!format!("{k1:x}").contains("12345678"));
    }

    #[test]
    fn shield_fades_threatened_entry_side_on_d9_d10_and_never_throttles_reducing_or_twap() {
        let mut shield = WalletToxicityShield::new(WalletToxicityConfig {
            max_tracked_wallets: 2,
            ..WalletToxicityConfig::default()
        })
        .unwrap();

        let d9 = salted_wallet_key(1, "acct_d9");
        let d10 = salted_wallet_key(1, "acct_d10");
        let benign = salted_wallet_key(1, "acct_twap");

        shield
            .upsert_wallet_tier(benign, WalletToxicityTier::BenignTwapSub)
            .unwrap();
        shield
            .upsert_wallet_tier(d9, WalletToxicityTier::Decile(9))
            .unwrap();
        // Capacity cap = 2 evicts `benign`.
        shield
            .upsert_wallet_tier(d10, WalletToxicityTier::Decile(10))
            .unwrap();
        assert_eq!(shield.tracked_wallet_count(), 2);

        // Benign twap-sub flagged event does not fade even if wallet key were risky.
        assert_eq!(
            shield.observe_taker_sweep(1_000, d10, true, true).unwrap(),
            None
        );

        // D10 taker buy sweep threatens Ask side -> Ask opening quotes fade 3x & 25% size for 1s.
        assert_eq!(
            shield.observe_taker_sweep(2_000, d10, true, false).unwrap(),
            Some(QuoteSide::Ask)
        );
        let ask_open = shield.quote_adjustment(2_500, QuoteSide::Ask, false);
        assert!(ask_open.faded);
        assert_eq!(ask_open.spread_multiplier, 3.0);
        assert_eq!(ask_open.size_scale, 0.25);
        assert_eq!(ask_open.active_decile, Some(10));

        // Bid side unaffected, and Ask REDUCING side never fades!
        assert!(!shield.quote_adjustment(2_500, QuoteSide::Bid, false).faded);
        let ask_reduce = shield.quote_adjustment(2_500, QuoteSide::Ask, true);
        assert!(!ask_reduce.faded);
        assert_eq!(ask_reduce.spread_multiplier, 1.0);
        assert_eq!(ask_reduce.size_scale, 1.0);

        // Expires cleanly after fade_window_ms (1_000 ms).
        assert!(!shield.quote_adjustment(3_001, QuoteSide::Ask, false).faded);

        // D9 taker sell sweep threatens Bid side at 2x spread.
        assert_eq!(
            shield.observe_taker_sweep(4_000, d9, false, false).unwrap(),
            Some(QuoteSide::Bid)
        );
        let bid_open = shield.quote_adjustment(4_100, QuoteSide::Bid, false);
        assert!(bid_open.faded && bid_open.spread_multiplier == 2.0);

        // Regressed timestamp is ignored (benign None), NOT an error, and does not disrupt state.
        assert_eq!(
            shield.observe_taker_sweep(3_999, d9, false, false).unwrap(),
            None
        );
        // The prior genuine bid latch is still intact after the ignored regressed print.
        assert!(shield.quote_adjustment(4_200, QuoteSide::Bid, false).faded);
    }

    #[test]
    fn out_of_order_print_does_not_error_or_disable_subsequent_detection() {
        let mut shield = WalletToxicityShield::new(WalletToxicityConfig::default()).unwrap();
        let d10 = salted_wallet_key(7, "acct_d10");
        shield
            .upsert_wallet_tier(d10, WalletToxicityTier::Decile(10))
            .unwrap();

        // Accept a genuine sweep at t=10_000 (watermark -> 10_000).
        assert_eq!(
            shield
                .observe_taker_sweep(10_000, d10, true, false)
                .unwrap(),
            Some(QuoteSide::Ask)
        );

        // An out-of-order (regressed) print at t=9_000 must NOT error and must NOT disrupt.
        assert_eq!(
            shield.observe_taker_sweep(9_000, d10, true, false).unwrap(),
            None
        );

        // Real-time detection still works for a later in-order genuine sweep.
        assert_eq!(
            shield
                .observe_taker_sweep(10_500, d10, true, false)
                .unwrap(),
            Some(QuoteSide::Ask)
        );
        assert!(shield.quote_adjustment(10_800, QuoteSide::Ask, false).faded);
    }

    #[test]
    fn far_future_spoof_does_not_latch_watermark_or_block_later_genuine_sweep() {
        let cfg = WalletToxicityConfig {
            max_future_skew_ms: 5_000,
            ..WalletToxicityConfig::default()
        };
        let mut shield = WalletToxicityShield::new(cfg).unwrap();
        let d10 = salted_wallet_key(3, "acct_d10");
        let benign = salted_wallet_key(3, "acct_twap");
        shield
            .upsert_wallet_tier(d10, WalletToxicityTier::Decile(10))
            .unwrap();
        shield
            .upsert_wallet_tier(benign, WalletToxicityTier::BenignTwapSub)
            .unwrap();

        // Baseline accepted sweep sets watermark to 1_000.
        assert_eq!(
            shield.observe_taker_sweep(1_000, d10, true, false).unwrap(),
            Some(QuoteSide::Ask)
        );

        // A spoofed far-future print (billions of ms ahead) from a benign cohort is ignored
        // and must NOT advance the watermark.
        assert_eq!(
            shield
                .observe_taker_sweep(9_000_000_000, benign, true, true)
                .unwrap(),
            None
        );
        // Even a far-future D10 print beyond max_future_skew_ms is dropped without latching.
        assert_eq!(
            shield
                .observe_taker_sweep(9_000_000_000, d10, true, false)
                .unwrap(),
            None
        );

        // A later genuine sweep at a normal time is still detected (watermark was not latched).
        assert_eq!(
            shield
                .observe_taker_sweep(2_000, d10, false, false)
                .unwrap(),
            Some(QuoteSide::Bid)
        );
        assert!(shield.quote_adjustment(2_300, QuoteSide::Bid, false).faded);
    }
}
