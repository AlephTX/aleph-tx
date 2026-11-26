//! Venue-agnostic queue-priority market-making state machine with multi-venue
//! lead-lag fair value, asymmetric cancel shields, and fast trade-print sweep pulls.
//!
//! Zero exchange-client coupling (`crate::exchanges::*` is intentionally absent).

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuoteSide {
    Bid,
    Ask,
}

impl QuoteSide {
    pub fn opposite(self) -> Self {
        match self {
            Self::Bid => Self::Ask,
            Self::Ask => Self::Bid,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum HedgeMode {
    PassiveInventoryCycle,
    CrossVenueMakerNet,
    ExternalIocHedge { band_usd: f64 },
}

/// Venue-agnostic physical execution profile describing latency, fees, and order
/// channel capabilities without coupling to any exchange module.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct VenueExecutionProfile {
    pub profile_name: &'static str,
    pub submit_rtt_ms: f64,
    pub cancel_rtt_ms: f64,
    pub maker_fee_bps: f64,
    pub taker_fee_bps: f64,
    pub supports_alo_skip_speedbump: bool,
    pub supports_priority_cancel: bool,
    pub supports_batch_modify: bool,
    pub supports_dms: bool,
}

impl VenueExecutionProfile {
    /// Low-latency (`~35 ms`) zero-maker-fee CLOB profile with ALO speed-bump bypass and DMS.
    pub const fn fast_zero_fee_alo() -> Self {
        Self {
            profile_name: "fast_zero_fee_alo",
            submit_rtt_ms: 36.0,
            cancel_rtt_ms: 33.0,
            maker_fee_bps: 0.0,
            taker_fee_bps: 2.25,
            supports_alo_skip_speedbump: true,
            supports_priority_cancel: true,
            supports_batch_modify: false,
            supports_dms: true,
        }
    }

    /// Higher-latency (`~670 ms` L1 write consensus) wide-spread builder-DEX profile.
    pub const fn slow_l1_wide_spread() -> Self {
        Self {
            profile_name: "slow_l1_wide_spread",
            submit_rtt_ms: 670.0,
            cancel_rtt_ms: 695.0,
            maker_fee_bps: 0.30,
            taker_fee_bps: 0.90,
            supports_alo_skip_speedbump: false,
            supports_priority_cancel: false,
            supports_batch_modify: true,
            supports_dms: false,
        }
    }

    /// Zero-fee post-only venue with symmetric speed bump (`~200 ms`, e.g. Robinhood / RWA speed-bump books).
    pub const fn speedbump_zero_fee_wide() -> Self {
        Self {
            profile_name: "speedbump_zero_fee_wide",
            submit_rtt_ms: 200.0,
            cancel_rtt_ms: 200.0,
            maker_fee_bps: 0.0,
            taker_fee_bps: 0.0,
            supports_alo_skip_speedbump: false,
            supports_priority_cancel: false,
            supports_batch_modify: true,
            supports_dms: false,
        }
    }

    /// Low-latency (`~40 ms`) CLOB with maker-fee / taker-rebate tiers (e.g. Extended / Bulk / Backpack MM tiers).
    pub const fn fast_fee_rebate_clob() -> Self {
        Self {
            profile_name: "fast_fee_rebate_clob",
            submit_rtt_ms: 40.0,
            cancel_rtt_ms: 40.0,
            maker_fee_bps: 0.0,
            taker_fee_bps: 2.00,
            supports_alo_skip_speedbump: true,
            supports_priority_cancel: true,
            supports_batch_modify: false,
            supports_dms: false,
        }
    }
}

fn default_lead_validation_horizon_ms() -> u64 {
    2_000
}
fn default_lead_validation_min_move_bps() -> f64 {
    0.50
}
fn default_lead_validation_min_samples() -> u32 {
    12
}
fn default_lead_validation_min_score_bps() -> f64 {
    0.02
}
fn default_alo_reject_cooldown_ms() -> u64 {
    350
}
fn default_alo_reject_stepback_streak() -> u32 {
    2
}
fn default_markout_shield_horizon_ms() -> u64 {
    1_000
}
fn default_markout_shield_adverse_threshold_bps() -> f64 {
    1.50
}
fn default_markout_shield_cooldown_ms() -> u64 {
    1_200
}
fn default_markout_shield_size_scale() -> f64 {
    0.50
}
fn default_submit_rtt_ms() -> f64 {
    0.0
}
fn default_alo_flight_guard_sigma_bps() -> f64 {
    0.0
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueuePriorityMmConfig {
    pub tick_size: f64,
    pub lot_size: f64,
    pub min_notional_usd: f64,
    /// Maker fee in bps (e.g. `0.0` on fast zero-fee ALO venues, `0.30` on Tier 0 growthMode).
    pub maker_fee_bps: f64,
    pub quote_notional_usd: f64,
    pub max_position_notional_usd: f64,
    pub ladder_levels: usize,
    /// Step 1 tick inside touch when spread >= `min_penny_spread_ticks` (and edge permits).
    pub min_penny_spread_ticks: u32,
    /// Minimum `(edge_vs_fair_value_bps - maker_fee_bps)` before placing/keeping an opening quote.
    pub min_entry_edge_bps: f64,
    /// Pull ONLY the endangered side when aligned external lead gap >= threshold (bps).
    pub lead_cancel_threshold_bps: f64,
    /// Spread multiplier for dynamic lead-pull threshold: `max(lead_cancel_threshold_bps, mult * spread_bps)`.
    #[serde(default)]
    pub lead_cancel_spread_mult: f64,
    /// Require a reference feed to earn causal forward-prediction validation before driving lead pulls.
    #[serde(default)]
    pub require_validated_lead_ref: bool,
    #[serde(default = "default_lead_validation_horizon_ms")]
    pub lead_validation_horizon_ms: u64,
    #[serde(default = "default_lead_validation_min_move_bps")]
    pub lead_validation_min_move_bps: f64,
    #[serde(default = "default_lead_validation_min_samples")]
    pub lead_validation_min_samples: u32,
    #[serde(default = "default_lead_validation_min_score_bps")]
    pub lead_validation_min_score_bps: f64,
    /// Reject external references whose relative spread exceeds `ratio * max(target_spread_bps, 0.5)` (`0.0` disables).
    #[serde(default)]
    pub max_ref_spread_to_target_ratio: f64,
    /// Causal EWMA half-life / tau (ms) for cross-venue basis `ln(ref_mid / target_mid)`.
    pub basis_ewma_tau_ms: f64,
    /// Skew applied to reservation price (bps) at 100% of `max_position_notional_usd`.
    pub inventory_skew_bps_per_full_limit: f64,
    /// Retain existing resting quote without resetting FIFO queue if within N ticks of target.
    pub max_queue_keep_ticks: u32,
    /// Pull side immediately if a fast trade print consumes >= ratio of L1 (above `sweep_min_notional_usd`) or trades strictly through touch.
    pub sweep_print_pull_ratio: f64,
    /// Minimum notional USD required before L1 size-ratio trade-print sweep protection triggers (`0.0` disables notional floor).
    #[serde(default)]
    pub sweep_min_notional_usd: f64,
    /// Cooldown (ms) after a sweep print before re-quoting that side.
    pub sweep_cooldown_ms: u64,
    /// Per-side opening cooldown (ms) after a post-only ALO rejection.
    #[serde(default = "default_alo_reject_cooldown_ms")]
    pub alo_reject_cooldown_ms: u64,
    /// Consecutive ALO rejections on a side before stepping back 1 extra tick from touch.
    #[serde(default = "default_alo_reject_stepback_streak")]
    pub alo_reject_stepback_streak: u32,
    #[serde(default = "default_markout_shield_horizon_ms")]
    pub markout_shield_horizon_ms: u64,
    #[serde(default = "default_markout_shield_adverse_threshold_bps")]
    pub markout_shield_adverse_threshold_bps: f64,
    #[serde(default = "default_markout_shield_cooldown_ms")]
    pub markout_shield_cooldown_ms: u64,
    #[serde(default = "default_markout_shield_size_scale")]
    pub markout_shield_size_scale: f64,
    /// Optional 3-level weighted OBI filter (`(0, 1]`) that blocks new opening back-of-queue orders under adverse imbalance.
    #[serde(default)]
    pub obi_entry_veto_threshold: Option<f64>,
    /// Expected order submission RTT (`ms >= 0`) used by the proactive ALO flight-collision guard.
    #[serde(default = "default_submit_rtt_ms")]
    pub submit_rtt_ms: f64,
    /// Skip fresh opening L1 ALO placements on narrow (`<= 2` tick) spreads when flight volatility
    /// `sigma_bps_per_sqrt_s * sqrt(submit_rtt_ms / 1000)` reaches this threshold (`0.0` disables).
    #[serde(default = "default_alo_flight_guard_sigma_bps")]
    pub alo_flight_guard_sigma_bps: f64,
    /// Optional 3-level weighted OBI threshold (`(0, 1]`) that skips fresh opening ALO chases on
    /// narrow (`<= 2` tick) books where touch is likely to move before wire arrival.
    #[serde(default)]
    pub alo_flight_guard_obi_threshold: Option<f64>,
    pub max_book_age_ms: u64,
    pub max_session_loss_usd: f64,
    pub max_inventory_hold_ms: u64,
    /// If true, refuse opening quotes unless at least one fresh external reference is available.
    pub require_fresh_reference: bool,
    pub hedge_mode: HedgeMode,
}

impl Default for QueuePriorityMmConfig {
    fn default() -> Self {
        Self {
            tick_size: 0.01,
            lot_size: 0.01,
            min_notional_usd: 10.0,
            maker_fee_bps: 0.0,
            quote_notional_usd: 30.0,
            max_position_notional_usd: 50.0,
            ladder_levels: 1,
            min_penny_spread_ticks: 2,
            min_entry_edge_bps: 0.10,
            lead_cancel_threshold_bps: 0.30,
            lead_cancel_spread_mult: 0.0,
            require_validated_lead_ref: false,
            lead_validation_horizon_ms: default_lead_validation_horizon_ms(),
            lead_validation_min_move_bps: default_lead_validation_min_move_bps(),
            lead_validation_min_samples: default_lead_validation_min_samples(),
            lead_validation_min_score_bps: default_lead_validation_min_score_bps(),
            max_ref_spread_to_target_ratio: 0.0,
            basis_ewma_tau_ms: 120_000.0,
            inventory_skew_bps_per_full_limit: 1.5,
            max_queue_keep_ticks: 2,
            sweep_print_pull_ratio: 0.60,
            sweep_min_notional_usd: 0.0,
            sweep_cooldown_ms: 1_500,
            alo_reject_cooldown_ms: default_alo_reject_cooldown_ms(),
            alo_reject_stepback_streak: default_alo_reject_stepback_streak(),
            markout_shield_horizon_ms: default_markout_shield_horizon_ms(),
            markout_shield_adverse_threshold_bps: default_markout_shield_adverse_threshold_bps(),
            markout_shield_cooldown_ms: default_markout_shield_cooldown_ms(),
            markout_shield_size_scale: default_markout_shield_size_scale(),
            obi_entry_veto_threshold: None,
            submit_rtt_ms: default_submit_rtt_ms(),
            alo_flight_guard_sigma_bps: default_alo_flight_guard_sigma_bps(),
            alo_flight_guard_obi_threshold: None,
            max_book_age_ms: 5_000,
            max_session_loss_usd: 3.0,
            max_inventory_hold_ms: 90_000,
            require_fresh_reference: false,
            hedge_mode: HedgeMode::PassiveInventoryCycle,
        }
    }
}

impl QueuePriorityMmConfig {
    /// Construct a venue-agnostic configuration calibrated from a physical [`VenueExecutionProfile`].
    pub fn for_profile(
        profile: &VenueExecutionProfile,
        tick_size: f64,
        lot_size: f64,
        min_notional_usd: f64,
        quote_usd: f64,
        max_pos_usd: f64,
        max_loss_usd: f64,
    ) -> Self {
        if profile.cancel_rtt_ms <= 60.0 {
            let entry_edge = if profile.maker_fee_bps > 0.0 {
                0.20
            } else {
                0.10
            };
            Self {
                tick_size,
                lot_size,
                min_notional_usd,
                maker_fee_bps: profile.maker_fee_bps,
                quote_notional_usd: quote_usd,
                max_position_notional_usd: max_pos_usd,
                ladder_levels: 1,
                min_penny_spread_ticks: 4,
                min_entry_edge_bps: entry_edge,
                lead_cancel_threshold_bps: 0.80,
                lead_cancel_spread_mult: 1.5,
                require_validated_lead_ref: true,
                lead_validation_horizon_ms: 1_500,
                lead_validation_min_move_bps: 0.35,
                lead_validation_min_samples: 5,
                max_ref_spread_to_target_ratio: 3.0,
                inventory_skew_bps_per_full_limit: 1.8,
                max_queue_keep_ticks: 4,
                sweep_print_pull_ratio: 0.75,
                sweep_min_notional_usd: 200.0,
                sweep_cooldown_ms: 1_000,
                alo_reject_cooldown_ms: 350,
                submit_rtt_ms: profile.submit_rtt_ms,
                alo_flight_guard_sigma_bps: 0.25,
                alo_flight_guard_obi_threshold: Some(0.45),
                max_book_age_ms: 5_000,
                max_session_loss_usd: max_loss_usd,
                max_inventory_hold_ms: 90_000,
                hedge_mode: HedgeMode::PassiveInventoryCycle,
                ..Default::default()
            }
        } else if profile.cancel_rtt_ms <= 300.0 {
            Self {
                tick_size,
                lot_size,
                min_notional_usd,
                maker_fee_bps: profile.maker_fee_bps,
                quote_notional_usd: quote_usd,
                max_position_notional_usd: max_pos_usd,
                ladder_levels: 1,
                min_penny_spread_ticks: 4,
                min_entry_edge_bps: 0.25,
                lead_cancel_threshold_bps: 1.10,
                lead_cancel_spread_mult: 1.8,
                require_validated_lead_ref: true,
                lead_validation_horizon_ms: 1_500,
                lead_validation_min_move_bps: 0.40,
                lead_validation_min_samples: 5,
                max_ref_spread_to_target_ratio: 3.0,
                inventory_skew_bps_per_full_limit: 2.2,
                max_queue_keep_ticks: 4,
                sweep_print_pull_ratio: 0.75,
                sweep_min_notional_usd: 200.0,
                sweep_cooldown_ms: 1_200,
                alo_reject_cooldown_ms: 600,
                submit_rtt_ms: profile.submit_rtt_ms,
                alo_flight_guard_sigma_bps: 0.35,
                alo_flight_guard_obi_threshold: Some(0.55),
                max_book_age_ms: 5_000,
                max_session_loss_usd: max_loss_usd,
                max_inventory_hold_ms: 120_000,
                hedge_mode: HedgeMode::PassiveInventoryCycle,
                ..Default::default()
            }
        } else {
            Self {
                tick_size,
                lot_size,
                min_notional_usd,
                maker_fee_bps: profile.maker_fee_bps,
                quote_notional_usd: quote_usd,
                max_position_notional_usd: max_pos_usd,
                ladder_levels: 1,
                min_penny_spread_ticks: 4,
                min_entry_edge_bps: 0.50,
                lead_cancel_threshold_bps: 1.50,
                lead_cancel_spread_mult: 2.0,
                require_validated_lead_ref: true,
                lead_validation_horizon_ms: 2_000,
                lead_validation_min_move_bps: 0.50,
                lead_validation_min_samples: 6,
                max_ref_spread_to_target_ratio: 3.0,
                inventory_skew_bps_per_full_limit: 2.5,
                max_queue_keep_ticks: 4,
                sweep_print_pull_ratio: 0.75,
                sweep_min_notional_usd: 200.0,
                sweep_cooldown_ms: 1_500,
                alo_reject_cooldown_ms: 1_000,
                submit_rtt_ms: profile.submit_rtt_ms,
                alo_flight_guard_sigma_bps: 0.0,
                alo_flight_guard_obi_threshold: None,
                max_book_age_ms: 5_000,
                max_session_loss_usd: max_loss_usd,
                max_inventory_hold_ms: 180_000,
                hedge_mode: HedgeMode::PassiveInventoryCycle,
                ..Default::default()
            }
        }
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.tick_size.is_finite() && self.tick_size > 0.0,
            "tick_size must be positive and finite"
        );
        ensure!(
            self.lot_size.is_finite() && self.lot_size > 0.0,
            "lot_size must be positive and finite"
        );
        ensure!(
            self.min_notional_usd.is_finite() && self.min_notional_usd >= 0.0,
            "min_notional_usd must be non-negative"
        );
        ensure!(
            self.maker_fee_bps.is_finite() && self.maker_fee_bps >= -5.0,
            "maker_fee_bps invalid"
        );
        ensure!(
            self.quote_notional_usd.is_finite() && self.quote_notional_usd > 0.0,
            "quote_notional_usd must be positive"
        );
        ensure!(
            self.max_position_notional_usd.is_finite()
                && self.max_position_notional_usd >= self.quote_notional_usd,
            "max_position_notional_usd must be >= quote_notional_usd"
        );
        ensure!(
            (1..=3).contains(&self.ladder_levels),
            "ladder_levels must be in 1..=3"
        );
        ensure!(
            self.lead_cancel_threshold_bps.is_finite() && self.lead_cancel_threshold_bps > 0.0,
            "lead_cancel_threshold_bps must be positive"
        );
        ensure!(
            self.lead_cancel_spread_mult.is_finite() && self.lead_cancel_spread_mult >= 0.0,
            "lead_cancel_spread_mult must be finite and >= 0"
        );
        ensure!(
            self.lead_validation_horizon_ms > 0
                && self.lead_validation_min_move_bps.is_finite()
                && self.lead_validation_min_move_bps > 0.0
                && self.lead_validation_min_score_bps.is_finite(),
            "lead validation parameters invalid"
        );
        ensure!(
            self.max_ref_spread_to_target_ratio.is_finite()
                && self.max_ref_spread_to_target_ratio >= 0.0,
            "max_ref_spread_to_target_ratio must be finite and >= 0"
        );
        ensure!(
            self.basis_ewma_tau_ms.is_finite() && self.basis_ewma_tau_ms > 0.0,
            "basis_ewma_tau_ms must be positive"
        );
        ensure!(
            self.sweep_print_pull_ratio.is_finite() && self.sweep_print_pull_ratio > 0.0,
            "sweep_print_pull_ratio must be positive"
        );
        ensure!(
            self.sweep_min_notional_usd.is_finite() && self.sweep_min_notional_usd >= 0.0,
            "sweep_min_notional_usd must be non-negative"
        );
        ensure!(
            self.markout_shield_adverse_threshold_bps.is_finite()
                && self.markout_shield_adverse_threshold_bps >= 0.0
                && self.markout_shield_size_scale.is_finite()
                && self.markout_shield_size_scale > 0.0
                && self.markout_shield_size_scale <= 1.0,
            "markout shield parameters invalid"
        );
        if let Some(obi_thresh) = self.obi_entry_veto_threshold {
            ensure!(
                obi_thresh.is_finite() && obi_thresh > 0.0 && obi_thresh <= 1.0,
                "obi_entry_veto_threshold must be finite and in (0, 1]"
            );
        }
        ensure!(
            self.submit_rtt_ms.is_finite() && self.submit_rtt_ms >= 0.0,
            "submit_rtt_ms must be finite and >= 0"
        );
        ensure!(
            self.alo_flight_guard_sigma_bps.is_finite() && self.alo_flight_guard_sigma_bps >= 0.0,
            "alo_flight_guard_sigma_bps must be finite and >= 0"
        );
        if let Some(obi_flight) = self.alo_flight_guard_obi_threshold {
            ensure!(
                obi_flight.is_finite() && obi_flight > 0.0 && obi_flight <= 1.0,
                "alo_flight_guard_obi_threshold must be finite and in (0, 1]"
            );
        }
        ensure!(self.max_book_age_ms > 0, "max_book_age_ms must be positive");
        ensure!(
            self.max_session_loss_usd.is_finite() && self.max_session_loss_usd > 0.0,
            "max_session_loss_usd must be positive"
        );
        if let HedgeMode::ExternalIocHedge { band_usd } = self.hedge_mode {
            ensure!(
                band_usd.is_finite() && band_usd >= 0.0,
                "ExternalIocHedge band_usd must be non-negative"
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct VenueBookUpdate {
    /// Descending `(price, size)`.
    pub bids: Vec<(f64, f64)>,
    /// Ascending `(price, size)`.
    pub asks: Vec<(f64, f64)>,
    pub received_at_ms: u64,
    pub source_ms: Option<u64>,
}

impl VenueBookUpdate {
    pub fn best_bid(&self) -> Option<(f64, f64)> {
        self.bids
            .first()
            .copied()
            .filter(|(p, s)| p.is_finite() && s.is_finite() && *p > 0.0 && *s > 0.0)
    }

    pub fn best_ask(&self) -> Option<(f64, f64)> {
        self.asks
            .first()
            .copied()
            .filter(|(p, s)| p.is_finite() && s.is_finite() && *p > 0.0 && *s > 0.0)
    }

    pub fn is_valid(&self) -> bool {
        matches!((self.best_bid(), self.best_ask()), (Some((b, _)), Some((a, _))) if b < a)
    }

    pub fn mid(&self) -> Option<f64> {
        let (b, _) = self.best_bid()?;
        let (a, _) = self.best_ask()?;
        if b < a { Some(0.5 * (b + a)) } else { None }
    }

    pub fn spread_bps(&self) -> Option<f64> {
        let (b, _) = self.best_bid()?;
        let (a, _) = self.best_ask()?;
        let mid = 0.5 * (b + a);
        if b < a && mid > 0.0 {
            Some((a - b) / mid * 10_000.0)
        } else {
            None
        }
    }

    /// 3-level weighted order-book imbalance `(wb - wa) / (wb + wa)` in `[-1.0, 1.0]`.
    pub fn weighted_obi(&self) -> f64 {
        const WEIGHTS: [f64; 3] = [1.0, 0.5, 0.25];
        let mut wb = 0.0;
        let mut wa = 0.0;
        for (i, w) in WEIGHTS.iter().enumerate() {
            if let Some(&(p, s)) = self.bids.get(i)
                && p.is_finite()
                && s.is_finite()
                && p > 0.0
                && s > 0.0
            {
                wb += s * w;
            }
            if let Some(&(p, s)) = self.asks.get(i)
                && p.is_finite()
                && s.is_finite()
                && p > 0.0
                && s > 0.0
            {
                wa += s * w;
            }
        }
        let denom = wb + wa;
        if denom > 0.0 {
            ((wb - wa) / denom).clamp(-1.0, 1.0)
        } else {
            0.0
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReferenceBookUpdate {
    pub venue_id: String,
    pub bid: f64,
    pub ask: f64,
    pub received_at_ms: u64,
}

impl ReferenceBookUpdate {
    pub fn mid(&self) -> Option<f64> {
        if self.bid.is_finite() && self.ask.is_finite() && self.bid > 0.0 && self.ask > self.bid {
            Some(0.5 * (self.bid + self.ask))
        } else {
            None
        }
    }

    pub fn spread_bps(&self) -> Option<f64> {
        let mid = self.mid()?;
        Some((self.ask - self.bid) / mid * 10_000.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FastTradePrint {
    /// Taker aggressor side: `Bid` means taker bought into resting asks; `Ask` means taker sold into bids.
    pub taker_side: QuoteSide,
    pub price: f64,
    pub size: f64,
    pub received_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RestingQuote {
    pub client_id: String,
    pub side: QuoteSide,
    pub price: f64,
    pub qty: f64,
    pub level_idx: usize,
    pub placed_at_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct InventoryState {
    pub target_qty: f64,
    pub external_net_qty: f64,
    pub inventory_since_ms: Option<u64>,
    pub realized_pnl_usd: f64,
    pub unrealized_pnl_usd: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MmAction {
    Keep {
        client_id: String,
        side: QuoteSide,
        level_idx: usize,
    },
    PlaceAlo {
        side: QuoteSide,
        price: f64,
        qty: f64,
        level_idx: usize,
        reduce_only: bool,
    },
    Cancel {
        client_id: String,
        side: QuoteSide,
        reason: &'static str,
    },
    ReduceOnlyIoc {
        side: QuoteSide,
        limit_price: f64,
        qty: f64,
        reason: &'static str,
    },
    HedgeIoc {
        side: QuoteSide,
        qty: f64,
        reason: &'static str,
    },
    Halt {
        reason: &'static str,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct EngineTelemetrySnapshot {
    pub fair_value: f64,
    pub lead_gap_bps: f64,
    pub obi: f64,
    pub sigma_bps_per_sqrt_s: f64,
    pub validated_lead_ref_count: usize,
    pub max_validated_lead_score_bps: f64,
    pub bid_markout_ewma_bps: f64,
    pub ask_markout_ewma_bps: f64,
    pub bid_alo_reject_streak: u32,
    pub ask_alo_reject_streak: u32,
}

#[derive(Debug, Clone, Default)]
struct RefLeadState {
    history: VecDeque<(u64, f64)>,
    pending: VecDeque<(u64, f64, f64)>, // (eval_ts_ms, dir, target_mid_at_signal)
    samples: u32,
    lead_score_ewma_bps: f64,
    last_signal_ms: Option<u64>,
}

impl RefLeadState {
    fn observe_and_mature(
        &mut self,
        now_ms: u64,
        ref_mid: f64,
        target_mid: f64,
        horizon_ms: u64,
        min_move_bps: f64,
    ) {
        while let Some(&(eval_ms, dir, tgt0)) = self.pending.front() {
            if now_ms < eval_ms {
                break;
            }
            self.pending.pop_front();
            if tgt0 > 0.0 && target_mid > 0.0 {
                let target_move_bps = (target_mid / tgt0).ln() * 10_000.0;
                let realized_bps = dir * target_move_bps;
                if realized_bps.is_finite() {
                    if self.samples == 0 {
                        self.lead_score_ewma_bps = realized_bps;
                    } else {
                        self.lead_score_ewma_bps =
                            0.85 * self.lead_score_ewma_bps + 0.15 * realized_bps;
                    }
                    self.samples = self.samples.saturating_add(1);
                }
            }
        }

        if self.history.back().is_none_or(|&(ts, _)| now_ms > ts) {
            if self.history.len() >= 64 {
                self.history.pop_front();
            }
            self.history.push_back((now_ms, ref_mid));
        }

        let cutoff = now_ms.saturating_sub(horizon_ms);
        let baseline = self
            .history
            .iter()
            .rev()
            .find(|&&(ts, _)| ts <= cutoff)
            .map(|&(_, px)| px);
        if let Some(ref0) = baseline
            && ref0 > 0.0
            && self
                .last_signal_ms
                .is_none_or(|prev| now_ms.saturating_sub(prev) >= 250)
        {
            let move_bps = (ref_mid / ref0).ln() * 10_000.0;
            if move_bps.abs() >= min_move_bps {
                let dir = if move_bps > 0.0 { 1.0 } else { -1.0 };
                if self.pending.len() >= 64 {
                    self.pending.pop_front();
                }
                self.pending
                    .push_back((now_ms.saturating_add(horizon_ms), dir, target_mid));
                self.last_signal_ms = Some(now_ms);
            }
        }
    }

    fn is_validated(&self, min_samples: u32, min_score_bps: f64) -> bool {
        self.samples >= min_samples && self.lead_score_ewma_bps >= min_score_bps
    }
}

#[derive(Debug, Clone)]
struct PendingFillMarkout {
    eval_ts_ms: u64,
    side: QuoteSide,
    fill_price: f64,
}

#[derive(Debug, Clone)]
pub struct QueuePriorityMmEngine {
    cfg: QueuePriorityMmConfig,
    basis_ewma_bps: Option<f64>,
    last_basis_update_ms: Option<u64>,
    bid_sweep_until_ms: u64,
    ask_sweep_until_ms: u64,
    bid_alo_cooldown_until_ms: u64,
    ask_alo_cooldown_until_ms: u64,
    bid_alo_reject_streak: u32,
    ask_alo_reject_streak: u32,
    pending_fills: VecDeque<PendingFillMarkout>,
    bid_markout_ewma_bps: f64,
    ask_markout_ewma_bps: f64,
    bid_markout_cooldown_until_ms: u64,
    ask_markout_cooldown_until_ms: u64,
    ref_lead_states: HashMap<String, RefLeadState>,
    last_mid_for_vol: Option<(u64, f64)>,
    var_bps2_per_s: f64,
    last_telemetry: EngineTelemetrySnapshot,
    halted_reason: Option<&'static str>,
}

impl QueuePriorityMmEngine {
    pub fn new(cfg: QueuePriorityMmConfig) -> Result<Self> {
        cfg.validate()?;
        Ok(Self {
            cfg,
            basis_ewma_bps: None,
            last_basis_update_ms: None,
            bid_sweep_until_ms: 0,
            ask_sweep_until_ms: 0,
            bid_alo_cooldown_until_ms: 0,
            ask_alo_cooldown_until_ms: 0,
            bid_alo_reject_streak: 0,
            ask_alo_reject_streak: 0,
            pending_fills: VecDeque::new(),
            bid_markout_ewma_bps: 0.0,
            ask_markout_ewma_bps: 0.0,
            bid_markout_cooldown_until_ms: 0,
            ask_markout_cooldown_until_ms: 0,
            ref_lead_states: HashMap::new(),
            last_mid_for_vol: None,
            var_bps2_per_s: 0.0,
            last_telemetry: EngineTelemetrySnapshot::default(),
            halted_reason: None,
        })
    }

    pub fn config(&self) -> &QueuePriorityMmConfig {
        &self.cfg
    }

    pub fn basis_ewma_bps(&self) -> Option<f64> {
        self.basis_ewma_bps
    }

    pub fn is_halted(&self) -> bool {
        self.halted_reason.is_some()
    }

    pub fn telemetry(&self) -> EngineTelemetrySnapshot {
        self.last_telemetry
    }

    /// Record an ALO post-only rejection on `side` to trigger cooldown and optional safety stepback.
    pub fn on_alo_reject(&mut self, side: QuoteSide, now_ms: u64) {
        let until = now_ms.saturating_add(self.cfg.alo_reject_cooldown_ms);
        match side {
            QuoteSide::Bid => {
                self.bid_alo_reject_streak = self.bid_alo_reject_streak.saturating_add(1);
                self.bid_alo_cooldown_until_ms = self.bid_alo_cooldown_until_ms.max(until);
            }
            QuoteSide::Ask => {
                self.ask_alo_reject_streak = self.ask_alo_reject_streak.saturating_add(1);
                self.ask_alo_cooldown_until_ms = self.ask_alo_cooldown_until_ms.max(until);
            }
        }
    }

    /// Clear consecutive ALO rejection state on `side` once an order rests successfully.
    pub fn on_alo_accepted(&mut self, side: QuoteSide) {
        match side {
            QuoteSide::Bid => self.bid_alo_reject_streak = 0,
            QuoteSide::Ask => self.ask_alo_reject_streak = 0,
        }
    }

    /// Record a fill for causal post-fill adverse-markout feedback (`reduce_only` unwinds remain exempt).
    pub fn on_fill(&mut self, side: QuoteSide, fill_price: f64, qty: f64, fill_ts_ms: u64) {
        if !fill_price.is_finite() || fill_price <= 0.0 || !qty.is_finite() || qty <= 0.0 {
            return;
        }
        if self.pending_fills.len() >= 128 {
            self.pending_fills.pop_front();
        }
        self.pending_fills.push_back(PendingFillMarkout {
            eval_ts_ms: fill_ts_ms.saturating_add(self.cfg.markout_shield_horizon_ms),
            side,
            fill_price,
        });
    }

    /// Notify engine of an incoming fast trade print to arm side-specific sweep protection.
    pub fn on_trade_print(&mut self, print: FastTradePrint, target_book: Option<&VenueBookUpdate>) {
        if !print.price.is_finite()
            || !print.size.is_finite()
            || print.price <= 0.0
            || print.size <= 0.0
        {
            return;
        }
        let Some(book) = target_book else {
            return;
        };
        if !book.is_valid() {
            return;
        }
        let trade_notional = print.price * print.size;
        let tol = self.cfg.tick_size * 0.1;
        match print.taker_side {
            QuoteSide::Bid => {
                // Taker bought strictly through ask or swept large size above notional floor.
                if let Some((ask_px, ask_sz)) = book.best_ask()
                    && (print.price > ask_px + tol
                        || (print.size >= ask_sz * self.cfg.sweep_print_pull_ratio
                            && trade_notional >= self.cfg.sweep_min_notional_usd))
                {
                    self.ask_sweep_until_ms = self.ask_sweep_until_ms.max(
                        print
                            .received_at_ms
                            .saturating_add(self.cfg.sweep_cooldown_ms),
                    );
                }
            }
            QuoteSide::Ask => {
                // Taker sold strictly through bid or swept large size above notional floor.
                if let Some((bid_px, bid_sz)) = book.best_bid()
                    && (print.price < bid_px - tol
                        || (print.size >= bid_sz * self.cfg.sweep_print_pull_ratio
                            && trade_notional >= self.cfg.sweep_min_notional_usd))
                {
                    self.bid_sweep_until_ms = self.bid_sweep_until_ms.max(
                        print
                            .received_at_ms
                            .saturating_add(self.cfg.sweep_cooldown_ms),
                    );
                }
            }
        }
    }

    fn update_realized_vol(&mut self, now_ms: u64, mid: f64) {
        if let Some((prev_ms, prev_mid)) = self.last_mid_for_vol
            && now_ms > prev_ms
            && prev_mid > 0.0
            && mid > 0.0
        {
            let dt_s = (now_ms - prev_ms) as f64 / 1000.0;
            if dt_s > 0.0 && dt_s <= 10.0 {
                let ret_bps = (mid / prev_mid).ln() * 10_000.0;
                let inst_var = ((ret_bps * ret_bps) / dt_s).min(400.0);
                let alpha = 1.0 - (-dt_s / 15.0).exp();
                self.var_bps2_per_s = (1.0 - alpha) * self.var_bps2_per_s + alpha * inst_var;
            }
        }
        self.last_mid_for_vol = Some((now_ms, mid));
    }

    fn mature_fill_markouts(&mut self, now_ms: u64, target_mid: f64) {
        if target_mid <= 0.0 {
            return;
        }
        while let Some(pf) = self.pending_fills.front() {
            if now_ms < pf.eval_ts_ms {
                break;
            }
            let pf = self.pending_fills.pop_front().expect("checked front");
            if pf.fill_price <= 0.0 {
                continue;
            }
            let sgn = match pf.side {
                QuoteSide::Bid => 1.0,
                QuoteSide::Ask => -1.0,
            };
            let mk_bps = sgn * (target_mid - pf.fill_price) / pf.fill_price * 10_000.0;
            if !mk_bps.is_finite() {
                continue;
            }
            let threshold = self.cfg.markout_shield_adverse_threshold_bps;
            match pf.side {
                QuoteSide::Bid => {
                    self.bid_markout_ewma_bps = 0.75 * self.bid_markout_ewma_bps + 0.25 * mk_bps;
                    if threshold > 0.0 && self.bid_markout_ewma_bps < -2.0 * threshold {
                        self.bid_markout_cooldown_until_ms = self
                            .bid_markout_cooldown_until_ms
                            .max(now_ms.saturating_add(self.cfg.markout_shield_cooldown_ms));
                    }
                }
                QuoteSide::Ask => {
                    self.ask_markout_ewma_bps = 0.75 * self.ask_markout_ewma_bps + 0.25 * mk_bps;
                    if threshold > 0.0 && self.ask_markout_ewma_bps < -2.0 * threshold {
                        self.ask_markout_cooldown_until_ms = self
                            .ask_markout_cooldown_until_ms
                            .max(now_ms.saturating_add(self.cfg.markout_shield_cooldown_ms));
                    }
                }
            }
        }
    }

    /// Evaluate target & reference books, inventory, and resting orders to produce deterministic actions.
    pub fn evaluate(
        &mut self,
        target: Option<&VenueBookUpdate>,
        references: &[ReferenceBookUpdate],
        resting: &[RestingQuote],
        inventory: InventoryState,
        now_ms: u64,
    ) -> Vec<MmAction> {
        let mut actions = Vec::new();

        // 1. Check session drawdown halt invariant.
        let total_pnl = inventory.realized_pnl_usd + inventory.unrealized_pnl_usd;
        if let Some(reason) = self.halted_reason {
            return self.fail_closed_with_flatten(target, resting, inventory, reason);
        }
        if total_pnl <= -self.cfg.max_session_loss_usd {
            self.halted_reason = Some("session_loss_breaker");
            return self.fail_closed_with_flatten(
                target,
                resting,
                inventory,
                "session_loss_breaker",
            );
        }

        // 2. Validate target book freshness & integrity (fail-closed cancel).
        let Some(book) = target else {
            return cancel_all_resting(resting, "missing_target_book");
        };
        if !book.is_valid() || now_ms.saturating_sub(book.received_at_ms) > self.cfg.max_book_age_ms
        {
            return cancel_all_resting(resting, "stale_or_invalid_target_book");
        }
        let (best_bid, _) = book.best_bid().expect("validated");
        let (best_ask, _) = book.best_ask().expect("validated");
        let target_mid = 0.5 * (best_bid + best_ask);
        let target_spread_bps = (best_ask - best_bid) / target_mid * 10_000.0;
        let obi = book.weighted_obi();
        self.update_realized_vol(now_ms, target_mid);
        self.mature_fill_markouts(now_ms, target_mid);

        // 3. Evaluate external reference anchor(s), causal lead validation, & basis EWMA.
        let mut fresh_valid_count = 0usize;
        let mut lead_validated_mids = Vec::new();
        let mut max_validated_lead_score_bps = 0.0_f64;
        for r in references {
            if now_ms.saturating_sub(r.received_at_ms) > self.cfg.max_book_age_ms {
                continue;
            }
            let Some(r_mid) = r.mid() else {
                continue;
            };
            let spread_ok = if self.cfg.max_ref_spread_to_target_ratio > 0.0 {
                r.spread_bps().is_some_and(|rsp| {
                    rsp <= self.cfg.max_ref_spread_to_target_ratio * target_spread_bps.max(0.5)
                })
            } else {
                true
            };
            if !spread_ok {
                continue;
            }
            fresh_valid_count += 1;
            let state = self.ref_lead_states.entry(r.venue_id.clone()).or_default();
            state.observe_and_mature(
                now_ms,
                r_mid,
                target_mid,
                self.cfg.lead_validation_horizon_ms,
                self.cfg.lead_validation_min_move_bps,
            );
            let validated_by_samples = state.is_validated(
                self.cfg.lead_validation_min_samples,
                self.cfg.lead_validation_min_score_bps,
            );
            if validated_by_samples {
                max_validated_lead_score_bps =
                    max_validated_lead_score_bps.max(state.lead_score_ewma_bps);
            }
            let lead_ok = !self.cfg.require_validated_lead_ref || validated_by_samples;
            if lead_ok {
                lead_validated_mids.push(r_mid);
            }
        }

        if (!references.is_empty() || self.cfg.require_fresh_reference) && fresh_valid_count == 0 {
            self.reset_basis_if_expired(now_ms);
            return cancel_all_resting(resting, "stale_reference_book");
        }

        let validated_lead_ref_count = lead_validated_mids.len();
        let (fair_value, lead_gap_bps) = if !lead_validated_mids.is_empty() {
            let ref_mid = median_f64(&lead_validated_mids);
            self.update_basis_ewma(ref_mid, target_mid, now_ms);
            let raw_basis_bps = (ref_mid / target_mid).ln() * 10_000.0;
            let ewma = self.basis_ewma_bps.unwrap_or(raw_basis_bps);
            let gap_bps = raw_basis_bps - ewma;
            let fv = ref_mid * (-ewma / 10_000.0).exp();
            (fv, gap_bps)
        } else {
            self.reset_basis_if_expired(now_ms);
            (target_mid, 0.0)
        };

        let sigma_bps_per_sqrt_s = self.var_bps2_per_s.sqrt();
        self.last_telemetry = EngineTelemetrySnapshot {
            fair_value,
            lead_gap_bps,
            obi,
            sigma_bps_per_sqrt_s,
            validated_lead_ref_count,
            max_validated_lead_score_bps,
            bid_markout_ewma_bps: self.bid_markout_ewma_bps,
            ask_markout_ewma_bps: self.ask_markout_ewma_bps,
            bid_alo_reject_streak: self.bid_alo_reject_streak,
            ask_alo_reject_streak: self.ask_alo_reject_streak,
        };

        // 4. Optional external IOC hedge (when net combined exposure exceeds band).
        if let HedgeMode::ExternalIocHedge { band_usd } = self.cfg.hedge_mode {
            let net_qty = inventory.target_qty + inventory.external_net_qty;
            let net_usd = net_qty * target_mid;
            if net_usd.abs() > band_usd {
                let excess_qty =
                    quantize_down((net_usd.abs() - band_usd) / target_mid, self.cfg.lot_size);
                if excess_qty * target_mid >= self.cfg.min_notional_usd && excess_qty > 0.0 {
                    actions.push(MmAction::HedgeIoc {
                        side: if net_qty > 0.0 {
                            QuoteSide::Ask
                        } else {
                            QuoteSide::Bid
                        },
                        qty: excess_qty,
                        reason: "external_hedge_band_breach",
                    });
                }
            }
        }

        // 5. Inventory hold timeout reduce-only flatten.
        if let Some(since_ms) = inventory.inventory_since_ms
            && now_ms.saturating_sub(since_ms) >= self.cfg.max_inventory_hold_ms
            && inventory.target_qty.abs() * target_mid >= self.cfg.min_notional_usd
        {
            let qty = quantize_down(inventory.target_qty.abs(), self.cfg.lot_size);
            if qty > 0.0 {
                let close_side = if inventory.target_qty > 0.0 {
                    QuoteSide::Ask
                } else {
                    QuoteSide::Bid
                };
                let limit_price = match close_side {
                    QuoteSide::Bid => round_tick_up(best_ask * 1.001, self.cfg.tick_size),
                    QuoteSide::Ask => round_tick_down(best_bid * 0.999, self.cfg.tick_size),
                };
                actions.extend(cancel_all_resting(resting, "inventory_hold_timeout"));
                actions.push(MmAction::ReduceOnlyIoc {
                    side: close_side,
                    limit_price,
                    qty,
                    reason: "inventory_hold_timeout",
                });
                return actions;
            }
        }

        // 6. Side-specific adverse selection shields (lead-lag + fast print sweep).
        // lead_gap_bps > 0 => ref moved UP relative to target => ASK is endangered.
        // lead_gap_bps < 0 => ref moved DOWN relative to target => BID is endangered.
        let effective_lead_thresh_bps = self
            .cfg
            .lead_cancel_threshold_bps
            .max(self.cfg.lead_cancel_spread_mult * target_spread_bps);
        let bid_lead_danger = (-lead_gap_bps) >= effective_lead_thresh_bps;
        let ask_lead_danger = lead_gap_bps >= effective_lead_thresh_bps;
        let bid_sweep_danger = now_ms < self.bid_sweep_until_ms;
        let ask_sweep_danger = now_ms < self.ask_sweep_until_ms;

        let bid_danger = bid_lead_danger || bid_sweep_danger;
        let ask_danger = ask_lead_danger || ask_sweep_danger;

        // 7. Compute skewed reservation price.
        let pos_notional = inventory.target_qty * target_mid;
        let inv_ratio = (pos_notional / self.cfg.max_position_notional_usd).clamp(-1.5, 1.5);
        // Smooth linear + quadratic skew: gentle near zero, firm near cap.
        let skew_factor = 0.5 * inv_ratio + 0.5 * inv_ratio * inv_ratio.abs();
        let skew_bps = skew_factor * self.cfg.inventory_skew_bps_per_full_limit;
        let reservation_px = fair_value * (-skew_bps / 10_000.0).exp();

        let spread_ticks = ((best_ask - best_bid) / self.cfg.tick_size)
            .round()
            .max(1.0) as u32;
        // Require >= 3 ticks before pennying BOTH opening quotes so Bid & Ask never collide at midpoint.
        let can_penny_open = spread_ticks >= self.cfg.min_penny_spread_ticks.max(3);

        // Evaluate Bid side and Ask side independently (asymmetric cancel / keep!).
        for side in [QuoteSide::Bid, QuoteSide::Ask] {
            let danger = match side {
                QuoteSide::Bid => bid_danger,
                QuoteSide::Ask => ask_danger,
            };
            let danger_reason = match (side, bid_lead_danger, ask_lead_danger) {
                (QuoteSide::Bid, true, _) => "lead_lag_pull_bid",
                (QuoteSide::Ask, _, true) => "lead_lag_pull_ask",
                (QuoteSide::Bid, false, _) => "sweep_print_pull_bid",
                (QuoteSide::Ask, _, false) => "sweep_print_pull_ask",
            };

            let side_resting: Vec<&RestingQuote> =
                resting.iter().filter(|r| r.side == side).collect();

            if danger {
                for r in side_resting {
                    actions.push(MmAction::Cancel {
                        client_id: r.client_id.clone(),
                        side,
                        reason: danger_reason,
                    });
                }
                continue;
            }

            let min_edge_total_bps = self.cfg.min_entry_edge_bps + self.cfg.maker_fee_bps;
            let reducing_edge_floor_bps =
                self.cfg.maker_fee_bps - self.cfg.inventory_skew_bps_per_full_limit;
            let max_pos_qty = self.cfg.max_position_notional_usd / target_mid;

            // Track committed exposure across levels on this side so total never breaches max_pos.
            let mut committed_qty = match side {
                QuoteSide::Bid => inventory.target_qty,
                QuoteSide::Ask => -inventory.target_qty,
            };
            let (alo_cooldown_until, alo_streak, side_markout_ewma, markout_cooldown_until) =
                match side {
                    QuoteSide::Bid => (
                        self.bid_alo_cooldown_until_ms,
                        self.bid_alo_reject_streak,
                        self.bid_markout_ewma_bps,
                        self.bid_markout_cooldown_until_ms,
                    ),
                    QuoteSide::Ask => (
                        self.ask_alo_cooldown_until_ms,
                        self.ask_alo_reject_streak,
                        self.ask_markout_ewma_bps,
                        self.ask_markout_cooldown_until_ms,
                    ),
                };

            for lvl in 0..self.cfg.ladder_levels {
                let remaining_to_flat = (-committed_qty).max(0.0);
                let is_reducing = remaining_to_flat > self.cfg.lot_size * 0.5;
                let can_penny_side = can_penny_open || (is_reducing && spread_ticks >= 2);
                let stepback_ticks = if !is_reducing
                    && self.cfg.alo_reject_stepback_streak > 0
                    && alo_streak >= self.cfg.alo_reject_stepback_streak
                {
                    1.0
                } else {
                    0.0
                };
                let touch_l1 = match side {
                    QuoteSide::Bid => {
                        let base = if can_penny_side {
                            (best_bid + self.cfg.tick_size).min(best_ask - self.cfg.tick_size)
                        } else {
                            best_bid
                        };
                        base - stepback_ticks * self.cfg.tick_size
                    }
                    QuoteSide::Ask => {
                        let base = if can_penny_side {
                            (best_ask - self.cfg.tick_size).max(best_bid + self.cfg.tick_size)
                        } else {
                            best_ask
                        };
                        base + stepback_ticks * self.cfg.tick_size
                    }
                };
                let desired_px_raw = match side {
                    QuoteSide::Bid => touch_l1 - (lvl as f64) * self.cfg.tick_size,
                    QuoteSide::Ask => touch_l1 + (lvl as f64) * self.cfg.tick_size,
                };
                let desired_px = match side {
                    QuoteSide::Bid => round_tick_down(desired_px_raw, self.cfg.tick_size),
                    QuoteSide::Ask => round_tick_up(desired_px_raw, self.cfg.tick_size),
                };

                // Edge check vs reservation price (strictly bound reducing tolerance to remaining unwind qty).
                let edge_bps = |px: f64| match side {
                    QuoteSide::Bid => (reservation_px - px) / reservation_px * 10_000.0,
                    QuoteSide::Ask => (px - reservation_px) / reservation_px * 10_000.0,
                };
                let desired_edge_bps = edge_bps(desired_px);
                let edge_ok = if is_reducing {
                    desired_edge_bps >= reducing_edge_floor_bps
                } else {
                    desired_edge_bps >= min_edge_total_bps
                };

                // Check post-only crossing guard against current opposing touch.
                let crosses_touch = match side {
                    QuoteSide::Bid => desired_px >= best_ask,
                    QuoteSide::Ask => desired_px <= best_bid,
                };

                let existing = side_resting.iter().find(|r| r.level_idx == lvl);

                if !edge_ok || crosses_touch || desired_px <= 0.0 {
                    if let Some(r) = existing {
                        actions.push(MmAction::Cancel {
                            client_id: r.client_id.clone(),
                            side,
                            reason: "edge_or_cross_invalid",
                        });
                    }
                    continue;
                }

                // Check if existing resting order can KEEP its queue priority.
                if let Some(r) = existing {
                    let r_crosses = match side {
                        QuoteSide::Bid => r.price >= best_ask,
                        QuoteSide::Ask => r.price <= best_bid,
                    };
                    let tick_diff =
                        ((r.price - desired_px).abs() / self.cfg.tick_size).round() as u32;
                    let r_edge_bps = edge_bps(r.price);
                    let r_opens_past_flat = !is_reducing
                        || r.qty > remaining_to_flat.min(inventory.target_qty.abs()) + 1e-12;
                    let r_edge_ok = if r_opens_past_flat {
                        r_edge_bps >= min_edge_total_bps
                    } else {
                        r_edge_bps >= reducing_edge_floor_bps
                    };
                    // Keep resting queue priority while order stays non-crossing, clears edge,
                    // sits within `max_queue_keep_ticks` of desired_px, and stays within position cap.
                    let within_keep = tick_diff <= self.cfg.max_queue_keep_ticks;
                    let would_exceed_cap = committed_qty + r.qty > max_pos_qty + 1e-12;
                    if !r_crosses && r_edge_ok && within_keep && !would_exceed_cap {
                        committed_qty += r.qty;
                        actions.push(MmAction::Keep {
                            client_id: r.client_id.clone(),
                            side,
                            level_idx: lvl,
                        });
                        continue;
                    }
                    actions.push(MmAction::Cancel {
                        client_id: r.client_id.clone(),
                        side,
                        reason: if would_exceed_cap {
                            "position_cap_trim"
                        } else {
                            "reprice_out_of_band"
                        },
                    });
                }

                // Opening-only entry gates (ALO cooldown, severe markout shield cooldown, OBI veto,
                // and proactive narrow-spread ALO flight-collision guard). Never blocks Keep or reduce_only.
                if !is_reducing {
                    if now_ms < alo_cooldown_until || now_ms < markout_cooldown_until {
                        continue;
                    }
                    let aligned_adverse_obi = match side {
                        QuoteSide::Bid => -obi,
                        QuoteSide::Ask => obi,
                    };
                    if let Some(obi_thresh) = self.cfg.obi_entry_veto_threshold
                        && aligned_adverse_obi >= obi_thresh
                    {
                        continue;
                    }
                    if spread_ticks <= 2 {
                        let flight_sigma_bps =
                            sigma_bps_per_sqrt_s * (self.cfg.submit_rtt_ms / 1_000.0).sqrt();
                        if self.cfg.alo_flight_guard_sigma_bps > 0.0
                            && flight_sigma_bps >= self.cfg.alo_flight_guard_sigma_bps
                        {
                            continue;
                        }
                        if let Some(obi_flight) = self.cfg.alo_flight_guard_obi_threshold
                            && aligned_adverse_obi >= obi_flight
                        {
                            continue;
                        }
                    }
                }

                let size_scale = if !is_reducing
                    && self.cfg.markout_shield_adverse_threshold_bps > 0.0
                    && side_markout_ewma < -self.cfg.markout_shield_adverse_threshold_bps
                {
                    self.cfg.markout_shield_size_scale
                } else {
                    1.0
                };
                let raw_quote_qty = quantize_down(
                    (self.cfg.quote_notional_usd * size_scale) / desired_px,
                    self.cfg.lot_size,
                );

                // Compute allowable quantity without breaching max_position_notional_usd or flipping
                // inventory across zero when quoting under relaxed reducing edge tolerance.
                let headroom = (max_pos_qty - committed_qty).max(0.0);
                let reduce_cap = if is_reducing {
                    remaining_to_flat.min(inventory.target_qty.abs())
                } else {
                    f64::INFINITY
                };
                let qty = quantize_down(
                    raw_quote_qty.min(headroom).min(reduce_cap),
                    self.cfg.lot_size,
                );
                if qty <= 0.0 || (!is_reducing && qty * desired_px < self.cfg.min_notional_usd) {
                    continue;
                }
                let reduce_only = is_reducing && qty <= inventory.target_qty.abs() + 1e-12;
                committed_qty += qty;
                actions.push(MmAction::PlaceAlo {
                    side,
                    price: desired_px,
                    qty,
                    level_idx: lvl,
                    reduce_only,
                });
            }

            // Cancel any leftover resting orders at levels >= ladder_levels.
            for r in side_resting {
                if r.level_idx >= self.cfg.ladder_levels {
                    actions.push(MmAction::Cancel {
                        client_id: r.client_id.clone(),
                        side,
                        reason: "excess_ladder_level",
                    });
                }
            }
        }

        actions
    }

    fn reset_basis_if_expired(&mut self, now_ms: u64) {
        if let Some(last) = self.last_basis_update_ms
            && now_ms.saturating_sub(last) > self.cfg.max_book_age_ms
        {
            self.basis_ewma_bps = None;
            self.last_basis_update_ms = None;
        }
    }

    fn update_basis_ewma(&mut self, ref_mid: f64, target_mid: f64, now_ms: u64) {
        let sample_bps = (ref_mid / target_mid).ln() * 10_000.0;
        if !sample_bps.is_finite() {
            return;
        }
        match (self.basis_ewma_bps, self.last_basis_update_ms) {
            (Some(prev), Some(last_ms))
                if now_ms >= last_ms
                    && now_ms.saturating_sub(last_ms) <= self.cfg.max_book_age_ms =>
            {
                let dt = (now_ms - last_ms) as f64;
                let alpha = 1.0 - (-dt / self.cfg.basis_ewma_tau_ms).exp();
                self.basis_ewma_bps = Some(prev + alpha * (sample_bps - prev));
            }
            _ => {
                // Gap > max_book_age_ms or initial sample: reset EWMA cleanly.
                self.basis_ewma_bps = Some(sample_bps);
            }
        }
        self.last_basis_update_ms = Some(now_ms);
    }

    fn fail_closed_with_flatten(
        &self,
        target: Option<&VenueBookUpdate>,
        resting: &[RestingQuote],
        inventory: InventoryState,
        reason: &'static str,
    ) -> Vec<MmAction> {
        let mut actions = cancel_all_resting(resting, reason);
        if let Some(book) = target
            && let (Some((bid, _)), Some((ask, _))) = (book.best_bid(), book.best_ask())
            && bid < ask
        {
            let qty = quantize_down(inventory.target_qty.abs(), self.cfg.lot_size);
            if qty > 0.0 {
                let side = if inventory.target_qty > 0.0 {
                    QuoteSide::Ask
                } else {
                    QuoteSide::Bid
                };
                let limit_price = match side {
                    QuoteSide::Bid => round_tick_up(ask * 1.002, self.cfg.tick_size),
                    QuoteSide::Ask => round_tick_down(bid * 0.998, self.cfg.tick_size),
                };
                actions.push(MmAction::ReduceOnlyIoc {
                    side,
                    limit_price,
                    qty,
                    reason,
                });
            }
        }
        actions.push(MmAction::Halt { reason });
        actions
    }
}

fn cancel_all_resting(resting: &[RestingQuote], reason: &'static str) -> Vec<MmAction> {
    resting
        .iter()
        .map(|r| MmAction::Cancel {
            client_id: r.client_id.clone(),
            side: r.side,
            reason,
        })
        .collect()
}

fn median_f64(values: &[f64]) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let n = sorted.len();
    if n.is_multiple_of(2) {
        0.5 * (sorted[n / 2 - 1] + sorted[n / 2])
    } else {
        sorted[n / 2]
    }
}

pub fn round_tick_down(price: f64, tick: f64) -> f64 {
    ((price / tick) + 1e-9).floor() * tick
}

pub fn round_tick_up(price: f64, tick: f64) -> f64 {
    ((price / tick) - 1e-9).ceil() * tick
}

pub fn quantize_down(qty: f64, lot: f64) -> f64 {
    if qty <= 0.0 {
        return 0.0;
    }
    ((qty / lot) + 1e-9).floor() * lot
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book(bid: f64, bsz: f64, ask: f64, asz: f64, ts: u64) -> VenueBookUpdate {
        VenueBookUpdate {
            bids: vec![(bid, bsz)],
            asks: vec![(ask, asz)],
            received_at_ms: ts,
            source_ms: Some(ts),
        }
    }

    fn ref_book(mid: f64, ts: u64) -> ReferenceBookUpdate {
        ReferenceBookUpdate {
            venue_id: "lighter_core".into(),
            bid: mid - 0.01,
            ask: mid + 0.01,
            received_at_ms: ts,
        }
    }

    fn base_cfg() -> QueuePriorityMmConfig {
        QueuePriorityMmConfig {
            tick_size: 0.01,
            lot_size: 0.01,
            min_notional_usd: 10.0,
            maker_fee_bps: 0.0,
            quote_notional_usd: 30.0,
            max_position_notional_usd: 60.0,
            ladder_levels: 1,
            min_penny_spread_ticks: 2,
            min_entry_edge_bps: 0.2,
            lead_cancel_threshold_bps: 0.30,
            basis_ewma_tau_ms: 120_000.0,
            inventory_skew_bps_per_full_limit: 2.0,
            max_queue_keep_ticks: 2,
            sweep_print_pull_ratio: 0.60,
            sweep_cooldown_ms: 1_500,
            max_book_age_ms: 5_000,
            max_session_loss_usd: 3.0,
            max_inventory_hold_ms: 60_000,
            require_fresh_reference: false,
            hedge_mode: HedgeMode::PassiveInventoryCycle,
            ..Default::default()
        }
    }

    #[test]
    fn pennies_inside_multi_tick_spread_when_edge_permits() {
        let mut eng = QueuePriorityMmEngine::new(base_cfg()).unwrap();
        // 4 ticks wide at ~$100 -> 4 bp spread; penny 1 tick inside gives +1 bp edge > 0.2 bp min.
        let b = book(100.00, 10.0, 100.04, 10.0, 1_000);
        let r = [ref_book(100.02, 1_000)];
        let acts = eng.evaluate(Some(&b), &r, &[], InventoryState::default(), 1_000);
        assert!(acts.iter().any(|a| matches!(
            a,
            MmAction::PlaceAlo { side: QuoteSide::Bid, price, .. } if (*price - 100.01).abs() < 1e-6
        )));
        assert!(acts.iter().any(|a| matches!(
            a,
            MmAction::PlaceAlo { side: QuoteSide::Ask, price, .. } if (*price - 100.03).abs() < 1e-6
        )));
    }

    #[test]
    fn min_entry_edge_blocks_one_tick_zero_edge_quotes() {
        let mut cfg = base_cfg();
        cfg.maker_fee_bps = 0.30; // Entropy growthMode
        cfg.min_entry_edge_bps = 0.50; // requires >= 0.80 bp half-spread
        let mut eng = QueuePriorityMmEngine::new(cfg).unwrap();
        // 1-tick spread at 100.00/100.01 -> half spread = 0.50 bp < 0.80 bp
        let b = book(100.00, 10.0, 100.01, 10.0, 1_000);
        let r = [ref_book(100.005, 1_000)];
        let acts = eng.evaluate(Some(&b), &r, &[], InventoryState::default(), 1_000);
        assert!(
            !acts.iter().any(|a| matches!(a, MmAction::PlaceAlo { .. })),
            "expected no opening quotes on insufficient edge, got {acts:?}"
        );
    }

    #[test]
    fn keeps_resting_order_within_band_to_preserve_queue_priority() {
        let mut eng = QueuePriorityMmEngine::new(base_cfg()).unwrap();
        let b1 = book(100.00, 10.0, 100.04, 10.0, 1_000);
        let r1 = [ref_book(100.02, 1_000)];
        let _ = eng.evaluate(Some(&b1), &r1, &[], InventoryState::default(), 1_000);

        // Resting at 100.01 / 100.03, BBO moves 1 tick away (100.00->99.99 / 100.04->100.05)
        let resting = vec![
            RestingQuote {
                client_id: "b1".into(),
                side: QuoteSide::Bid,
                price: 100.01,
                qty: 0.3,
                level_idx: 0,
                placed_at_ms: 1_000,
            },
            RestingQuote {
                client_id: "a1".into(),
                side: QuoteSide::Ask,
                price: 100.03,
                qty: 0.3,
                level_idx: 0,
                placed_at_ms: 1_000,
            },
        ];
        let b2 = book(99.99, 10.0, 100.05, 10.0, 1_200);
        let r2 = [ref_book(100.02, 1_200)];
        let acts = eng.evaluate(Some(&b2), &r2, &resting, InventoryState::default(), 1_200);
        assert!(acts.contains(&MmAction::Keep {
            client_id: "b1".into(),
            side: QuoteSide::Bid,
            level_idx: 0
        }));
        assert!(acts.contains(&MmAction::Keep {
            client_id: "a1".into(),
            side: QuoteSide::Ask,
            level_idx: 0
        }));
    }

    #[test]
    fn asymmetric_lead_lag_cancels_only_endangered_side() {
        let mut eng = QueuePriorityMmEngine::new(base_cfg()).unwrap();
        let b = book(100.00, 10.0, 100.04, 10.0, 1_000);
        let _ = eng.evaluate(
            Some(&b),
            &[ref_book(100.02, 1_000)],
            &[],
            InventoryState::default(),
            1_000,
        );

        // External reference jumps UP by +1.0 bp (> 0.30 bp threshold): Ask is endangered, Bid is safe!
        let resting = vec![
            RestingQuote {
                client_id: "b1".into(),
                side: QuoteSide::Bid,
                price: 100.01,
                qty: 0.3,
                level_idx: 0,
                placed_at_ms: 1_000,
            },
            RestingQuote {
                client_id: "a1".into(),
                side: QuoteSide::Ask,
                price: 100.03,
                qty: 0.3,
                level_idx: 0,
                placed_at_ms: 1_000,
            },
        ];
        let acts = eng.evaluate(
            Some(&b),
            &[ref_book(100.03, 1_100)],
            &resting,
            InventoryState::default(),
            1_100,
        );
        assert!(acts.iter().any(|a| matches!(
            a,
            MmAction::Cancel { client_id, side: QuoteSide::Ask, reason: "lead_lag_pull_ask" } if client_id == "a1"
        )));
        assert!(acts.iter().any(|a| matches!(
            a,
            MmAction::Keep { client_id, side: QuoteSide::Bid, .. } if client_id == "b1"
        )));
        assert!(!acts.iter().any(|a| matches!(
            a,
            MmAction::PlaceAlo {
                side: QuoteSide::Ask,
                ..
            }
        )));
    }

    #[test]
    fn fast_sweep_trade_print_pulls_hit_side_for_cooldown() {
        let mut eng = QueuePriorityMmEngine::new(base_cfg()).unwrap();
        let b = book(100.00, 10.0, 100.04, 10.0, 1_000);
        let _ = eng.evaluate(
            Some(&b),
            &[ref_book(100.02, 1_000)],
            &[],
            InventoryState::default(),
            1_000,
        );

        // Taker sells 7.0 (>= 60% of 10.0 bid L1) -> pulls Bid only.
        eng.on_trade_print(
            FastTradePrint {
                taker_side: QuoteSide::Ask,
                price: 100.00,
                size: 7.0,
                received_at_ms: 1_050,
            },
            Some(&b),
        );
        let resting = vec![
            RestingQuote {
                client_id: "b1".into(),
                side: QuoteSide::Bid,
                price: 100.00,
                qty: 0.3,
                level_idx: 0,
                placed_at_ms: 1_000,
            },
            RestingQuote {
                client_id: "a1".into(),
                side: QuoteSide::Ask,
                price: 100.04,
                qty: 0.3,
                level_idx: 0,
                placed_at_ms: 1_000,
            },
        ];
        let acts = eng.evaluate(
            Some(&b),
            &[ref_book(100.02, 1_100)],
            &resting,
            InventoryState::default(),
            1_100,
        );
        assert!(acts.iter().any(|a| matches!(
            a,
            MmAction::Cancel { client_id, side: QuoteSide::Bid, reason: "sweep_print_pull_bid" } if client_id == "b1"
        )));
        assert!(acts.iter().any(|a| matches!(
            a,
            MmAction::Keep { client_id, side: QuoteSide::Ask, .. } if client_id == "a1"
        )));
    }

    #[test]
    fn stale_book_or_reference_fails_closed_by_canceling_all() {
        let mut eng = QueuePriorityMmEngine::new(base_cfg()).unwrap();
        let resting = vec![RestingQuote {
            client_id: "b1".into(),
            side: QuoteSide::Bid,
            price: 100.00,
            qty: 0.3,
            level_idx: 0,
            placed_at_ms: 1_000,
        }];
        let stale_b = book(100.00, 10.0, 100.04, 10.0, 1_000);
        let acts = eng.evaluate(
            Some(&stale_b),
            &[],
            &resting,
            InventoryState::default(),
            7_000,
        );
        assert_eq!(
            acts,
            vec![MmAction::Cancel {
                client_id: "b1".into(),
                side: QuoteSide::Bid,
                reason: "stale_or_invalid_target_book"
            }]
        );

        let fresh_b = book(100.00, 10.0, 100.04, 10.0, 7_000);
        let stale_r = [ref_book(100.02, 1_000)];
        let acts2 = eng.evaluate(
            Some(&fresh_b),
            &stale_r,
            &resting,
            InventoryState::default(),
            7_000,
        );
        assert_eq!(
            acts2,
            vec![MmAction::Cancel {
                client_id: "b1".into(),
                side: QuoteSide::Bid,
                reason: "stale_reference_book"
            }]
        );
    }

    #[test]
    fn enforces_max_position_cap_including_resting_orders() {
        let mut eng = QueuePriorityMmEngine::new(base_cfg()).unwrap();
        let b = book(100.00, 10.0, 100.04, 10.0, 1_000);
        // Max pos $60 = 0.60 units; already long 0.50 units -> only 0.10 bid headroom (< $10 min notional -> no bid!).
        let inv = InventoryState {
            target_qty: 0.50,
            ..Default::default()
        };
        let acts = eng.evaluate(Some(&b), &[ref_book(100.02, 1_000)], &[], inv, 1_000);
        assert!(!acts.iter().any(|a| matches!(
            a,
            MmAction::PlaceAlo {
                side: QuoteSide::Bid,
                ..
            }
        )));
        assert!(acts.iter().any(|a| matches!(
            a,
            MmAction::PlaceAlo {
                side: QuoteSide::Ask,
                reduce_only: true,
                ..
            }
        )));
    }

    #[test]
    fn basis_ewma_resets_after_gap_exceeding_max_book_age() {
        let mut eng = QueuePriorityMmEngine::new(base_cfg()).unwrap();
        let b1 = book(100.00, 10.0, 100.04, 10.0, 1_000);
        let _ = eng.evaluate(
            Some(&b1),
            &[ref_book(100.02, 1_000)],
            &[],
            InventoryState::default(),
            1_000,
        );
        assert!(eng.basis_ewma_bps().unwrap().abs() < 1e-6);

        // Gap of 10s (> 5s max_book_age_ms): EWMA must reset cleanly to new basis (~+10 bp) rather than reporting a false 10 bp lead gap!
        let b2 = book(100.00, 10.0, 100.04, 10.0, 11_000);
        let r2 = [ref_book(100.12, 11_000)];
        let acts = eng.evaluate(Some(&b2), &r2, &[], InventoryState::default(), 11_000);
        assert!((eng.basis_ewma_bps().unwrap() - 9.99).abs() < 0.2);
        assert!(acts.iter().any(|a| matches!(
            a,
            MmAction::PlaceAlo {
                side: QuoteSide::Ask,
                ..
            }
        )));
    }

    #[test]
    fn session_loss_breaker_cancels_flattens_and_halts() {
        let mut eng = QueuePriorityMmEngine::new(base_cfg()).unwrap();
        let b = book(100.00, 10.0, 100.04, 10.0, 1_000);
        let resting = vec![RestingQuote {
            client_id: "b1".into(),
            side: QuoteSide::Bid,
            price: 100.01,
            qty: 0.3,
            level_idx: 0,
            placed_at_ms: 1_000,
        }];
        let inv = InventoryState {
            target_qty: 0.2,
            realized_pnl_usd: -2.5,
            unrealized_pnl_usd: -0.6,
            ..Default::default()
        };
        let acts = eng.evaluate(Some(&b), &[], &resting, inv, 1_000);
        assert!(eng.is_halted());
        assert!(acts.iter().any(|a| matches!(a, MmAction::Cancel { .. })));
        assert!(acts.iter().any(|a| matches!(
            a,
            MmAction::ReduceOnlyIoc {
                side: QuoteSide::Ask,
                ..
            }
        )));
        assert!(acts.iter().any(|a| matches!(
            a,
            MmAction::Halt {
                reason: "session_loss_breaker"
            }
        )));
    }

    #[test]
    fn inventory_hold_timeout_and_external_hedge_band_trigger_cleanly() {
        let mut cfg = base_cfg();
        cfg.hedge_mode = HedgeMode::ExternalIocHedge { band_usd: 20.0 };
        let mut eng = QueuePriorityMmEngine::new(cfg).unwrap();
        let b = book(100.00, 10.0, 100.04, 10.0, 70_000);
        let inv = InventoryState {
            target_qty: 0.35, // $35 > $20 band -> hedge $15 (0.15)
            external_net_qty: 0.0,
            inventory_since_ms: Some(1_000), // held 69s >= 60s
            ..Default::default()
        };
        let acts = eng.evaluate(Some(&b), &[ref_book(100.02, 70_000)], &[], inv, 70_000);
        assert!(acts.iter().any(|a| matches!(
            a,
            MmAction::HedgeIoc { side: QuoteSide::Ask, qty, .. } if (*qty - 0.15).abs() < 1e-6
        )));
        assert!(acts.iter().any(|a| matches!(
            a,
            MmAction::ReduceOnlyIoc {
                side: QuoteSide::Ask,
                ..
            }
        )));
    }

    #[test]
    fn reducing_quote_caps_at_inventory_and_stays_reduce_only() {
        let mut cfg = base_cfg();
        cfg.min_entry_edge_bps = 1.5;
        cfg.min_penny_spread_ticks = 99;
        let mut eng = QueuePriorityMmEngine::new(cfg).unwrap();
        // 2-tick spread at 100.00/100.02 gives 1.0 bp half-spread (< 1.5 bp opening edge),
        // while positive inventory adds skew that allows reduce-only Ask unwind.
        let b = book(100.00, 10.0, 100.02, 10.0, 1_000);
        let inv = InventoryState {
            target_qty: 0.20, // < raw quote qty 0.30 ($30)
            ..Default::default()
        };
        let acts = eng.evaluate(Some(&b), &[ref_book(100.01, 1_000)], &[], inv, 1_000);
        let ask_place = acts
            .iter()
            .find_map(|a| match a {
                MmAction::PlaceAlo {
                    side: QuoteSide::Ask,
                    qty,
                    reduce_only,
                    ..
                } => Some((*qty, *reduce_only)),
                _ => None,
            })
            .expect("expected reducing ask quote");
        assert!((ask_place.0 - 0.20).abs() <= 1e-9);
        assert!(
            ask_place.1,
            "reducing quote must remain strictly reduce_only"
        );
    }

    #[test]
    fn strict_through_sweep_ignores_normal_touch_print_and_triggers_on_through_or_large_sweep() {
        let mut cfg = base_cfg();
        cfg.sweep_print_pull_ratio = 0.75;
        cfg.sweep_min_notional_usd = 200.0;
        let mut eng = QueuePriorityMmEngine::new(cfg).unwrap();
        let b = book(100.00, 10.0, 100.04, 10.0, 1_000);
        let resting = vec![
            RestingQuote {
                client_id: "b1".into(),
                side: QuoteSide::Bid,
                price: 100.00,
                qty: 0.3,
                level_idx: 0,
                placed_at_ms: 1_000,
            },
            RestingQuote {
                client_id: "a1".into(),
                side: QuoteSide::Ask,
                price: 100.04,
                qty: 0.3,
                level_idx: 0,
                placed_at_ms: 1_000,
            },
        ];

        // Normal small touch print at best_ask (100.04, size=1.0) must NOT pull Ask.
        eng.on_trade_print(
            FastTradePrint {
                taker_side: QuoteSide::Bid,
                price: 100.04,
                size: 1.0,
                received_at_ms: 1_050,
            },
            Some(&b),
        );
        let acts1 = eng.evaluate(
            Some(&b),
            &[ref_book(100.02, 1_060)],
            &resting,
            InventoryState::default(),
            1_060,
        );
        assert!(acts1.iter().any(|a| matches!(
            a,
            MmAction::Keep {
                client_id,
                side: QuoteSide::Ask,
                ..
            } if client_id == "a1"
        )));

        // Strict-through print above best_ask (100.05) MUST arm Ask sweep pull.
        eng.on_trade_print(
            FastTradePrint {
                taker_side: QuoteSide::Bid,
                price: 100.05,
                size: 0.2,
                received_at_ms: 1_100,
            },
            Some(&b),
        );
        let acts2 = eng.evaluate(
            Some(&b),
            &[ref_book(100.02, 1_120)],
            &resting,
            InventoryState::default(),
            1_120,
        );
        assert!(acts2.iter().any(|a| matches!(
            a,
            MmAction::Cancel {
                client_id,
                side: QuoteSide::Ask,
                reason: "sweep_print_pull_ask"
            } if client_id == "a1"
        )));
    }

    #[test]
    fn causal_reference_lead_validator_enables_leading_ref_and_mutes_lagging_ref() {
        let mut cfg = base_cfg();
        cfg.require_validated_lead_ref = true;
        cfg.lead_validation_horizon_ms = 500;
        cfg.lead_validation_min_move_bps = 1.0;
        cfg.lead_validation_min_samples = 3;
        cfg.lead_validation_min_score_bps = 0.20;
        cfg.lead_cancel_threshold_bps = 0.50;
        cfg.max_book_age_ms = 5_000;

        // Case 1: Ref jumps FIRST, then target follows 500ms later -> Ref earns positive lead validation.
        let mut eng_lead = QueuePriorityMmEngine::new(cfg.clone()).unwrap();
        let mut t = 1_000_u64;
        for step in 0..5 {
            let tgt_px = 100.00 + (step as f64) * 0.05;
            let ref_px = 100.05 + (step as f64) * 0.05; // already moved ahead
            let b = book(tgt_px - 0.02, 10.0, tgt_px + 0.02, 10.0, t);
            let r = [ReferenceBookUpdate {
                venue_id: "fast_leader".into(),
                bid: ref_px - 0.01,
                ask: ref_px + 0.01,
                received_at_ms: t,
            }];
            let _ = eng_lead.evaluate(Some(&b), &r, &[], InventoryState::default(), t);
            t += 500;
        }
        assert!(
            eng_lead.telemetry().validated_lead_ref_count >= 1,
            "expected leading ref to validate cleanly"
        );

        // Case 2: Ref moves in OPPOSITE/lagging direction -> stays muted and never triggers lead_lag_pull.
        let mut eng_lag = QueuePriorityMmEngine::new(cfg).unwrap();
        let mut t2 = 1_000_u64;
        for step in 0..5 {
            let tgt_px = 100.00 - (step as f64) * 0.05; // target moves down
            let ref_px = 100.00 + (step as f64) * 0.05; // ref moves opposite
            let b = book(tgt_px - 0.02, 10.0, tgt_px + 0.02, 10.0, t2);
            let r = [ReferenceBookUpdate {
                venue_id: "lagging_ref".into(),
                bid: ref_px - 0.01,
                ask: ref_px + 0.01,
                received_at_ms: t2,
            }];
            let _ = eng_lag.evaluate(Some(&b), &r, &[], InventoryState::default(), t2);
            t2 += 500;
        }
        assert_eq!(eng_lag.telemetry().validated_lead_ref_count, 0);
        let resting_ask = vec![RestingQuote {
            client_id: "a1".into(),
            side: QuoteSide::Ask,
            price: 99.82,
            qty: 0.3,
            level_idx: 0,
            placed_at_ms: t2,
        }];
        let b_last = book(99.78, 10.0, 99.82, 10.0, t2);
        let r_spike = [ReferenceBookUpdate {
            venue_id: "lagging_ref".into(),
            bid: 101.00,
            ask: 101.02,
            received_at_ms: t2,
        }];
        let acts = eng_lag.evaluate(
            Some(&b_last),
            &r_spike,
            &resting_ask,
            InventoryState::default(),
            t2,
        );
        assert!(!acts.iter().any(|a| matches!(
            a,
            MmAction::Cancel {
                reason: "lead_lag_pull_ask" | "lead_lag_pull_bid",
                ..
            }
        )));
    }

    #[test]
    fn non_colliding_pennying_prevents_two_tick_midpoint_collision_for_opening_quotes() {
        let mut eng = QueuePriorityMmEngine::new(base_cfg()).unwrap();
        // 2-tick spread: 100.00 / 100.02. Opening quotes MUST NOT both penny to 100.01!
        let b = book(100.00, 10.0, 100.02, 10.0, 1_000);
        let acts = eng.evaluate(
            Some(&b),
            &[ref_book(100.01, 1_000)],
            &[],
            InventoryState::default(),
            1_000,
        );
        let bid_px = acts.iter().find_map(|a| match a {
            MmAction::PlaceAlo {
                side: QuoteSide::Bid,
                price,
                ..
            } => Some(*price),
            _ => None,
        });
        let ask_px = acts.iter().find_map(|a| match a {
            MmAction::PlaceAlo {
                side: QuoteSide::Ask,
                price,
                ..
            } => Some(*price),
            _ => None,
        });
        assert!((bid_px.unwrap() - 100.00).abs() < 1e-6);
        assert!((ask_px.unwrap() - 100.02).abs() < 1e-6);
    }

    #[test]
    fn alo_reject_feedback_arms_cooldown_and_clears_on_accept() {
        let mut cfg = base_cfg();
        cfg.alo_reject_cooldown_ms = 500;
        cfg.alo_reject_stepback_streak = 2;
        let mut eng = QueuePriorityMmEngine::new(cfg).unwrap();
        let b = book(100.00, 10.0, 100.04, 10.0, 1_000);

        eng.on_alo_reject(QuoteSide::Bid, 1_000);
        eng.on_alo_reject(QuoteSide::Bid, 1_100);
        let acts_cd = eng.evaluate(Some(&b), &[], &[], InventoryState::default(), 1_200);
        assert!(!acts_cd.iter().any(|a| matches!(
            a,
            MmAction::PlaceAlo {
                side: QuoteSide::Bid,
                ..
            }
        )));

        // After cooldown expires with streak == 2, Bid steps back 1 tick (100.01 -> 100.00).
        let acts_step = eng.evaluate(Some(&b), &[], &[], InventoryState::default(), 1_700);
        let bid_px = acts_step.iter().find_map(|a| match a {
            MmAction::PlaceAlo {
                side: QuoteSide::Bid,
                price,
                ..
            } => Some(*price),
            _ => None,
        });
        assert!((bid_px.unwrap() - 100.00).abs() < 1e-6);

        eng.on_alo_accepted(QuoteSide::Bid);
        let acts_norm = eng.evaluate(Some(&b), &[], &[], InventoryState::default(), 1_800);
        let bid_norm = acts_norm.iter().find_map(|a| match a {
            MmAction::PlaceAlo {
                side: QuoteSide::Bid,
                price,
                ..
            } => Some(*price),
            _ => None,
        });
        assert!((bid_norm.unwrap() - 100.01).abs() < 1e-6);
    }

    #[test]
    fn post_fill_adverse_markout_shield_decays_opening_size_without_blocking_reduce_only() {
        let mut cfg = base_cfg();
        cfg.markout_shield_horizon_ms = 1_000;
        cfg.markout_shield_adverse_threshold_bps = 1.5;
        cfg.markout_shield_size_scale = 0.5;
        let mut eng = QueuePriorityMmEngine::new(cfg).unwrap();

        // 1 fill at -10 bp puts EWMA at -2.5 bp (< -1.5 threshold, > -3.0 severe cooldown) -> 50% size decay.
        eng.on_fill(QuoteSide::Bid, 100.10, 0.3, 1_000);
        let b_drop = book(99.98, 10.0, 100.02, 10.0, 2_500);
        let acts = eng.evaluate(Some(&b_drop), &[], &[], InventoryState::default(), 2_500);
        let open_bid_qty = acts
            .iter()
            .find_map(|a| match a {
                MmAction::PlaceAlo {
                    side: QuoteSide::Bid,
                    qty,
                    reduce_only: false,
                    ..
                } => Some(*qty),
                _ => None,
            })
            .expect("expected scaled opening bid");
        assert!(
            open_bid_qty <= 0.16,
            "expected ~50% decay, got {open_bid_qty}"
        );

        // 5 more severe fills at -10 bp push EWMA below -3.0 bp -> severe cooldown blocks opening Bid.
        for i in 1..6 {
            eng.on_fill(QuoteSide::Bid, 100.10, 0.3, 1_000 + i * 50);
        }
        let acts_cd = eng.evaluate(Some(&b_drop), &[], &[], InventoryState::default(), 2_525);
        assert!(!acts_cd.iter().any(|a| matches!(
            a,
            MmAction::PlaceAlo {
                side: QuoteSide::Bid,
                reduce_only: false,
                ..
            }
        )));

        // With short inventory, Bid is reduce_only and must NOT be blocked/decayed by opening markout shield.
        let inv_short = InventoryState {
            target_qty: -0.30,
            ..Default::default()
        };
        let acts_red = eng.evaluate(Some(&b_drop), &[], &[], inv_short, 2_550);
        assert!(acts_red.iter().any(|a| matches!(
            a,
            MmAction::PlaceAlo {
                side: QuoteSide::Bid,
                reduce_only: true,
                qty,
                ..
            } if (*qty - 0.30).abs() <= 0.01
        )));
    }

    #[test]
    fn execution_profile_presets_differentiate_fast_and_slow_venues_without_coupling() {
        let fast = VenueExecutionProfile::fast_zero_fee_alo();
        let slow = VenueExecutionProfile::slow_l1_wide_spread();
        let speedbump = VenueExecutionProfile::speedbump_zero_fee_wide();
        let rebate_clob = VenueExecutionProfile::fast_fee_rebate_clob();
        let cfg_fast = QueuePriorityMmConfig::for_profile(&fast, 0.01, 0.001, 5.0, 30.0, 50.0, 3.0);
        let cfg_slow =
            QueuePriorityMmConfig::for_profile(&slow, 0.01, 0.001, 10.0, 30.0, 50.0, 3.0);
        let cfg_sb =
            QueuePriorityMmConfig::for_profile(&speedbump, 0.01, 0.001, 10.0, 30.0, 50.0, 3.0);
        let cfg_clob =
            QueuePriorityMmConfig::for_profile(&rebate_clob, 0.01, 0.001, 10.0, 30.0, 50.0, 3.0);
        cfg_fast.validate().unwrap();
        cfg_slow.validate().unwrap();
        cfg_sb.validate().unwrap();
        cfg_clob.validate().unwrap();
        assert!(cfg_fast.lead_cancel_threshold_bps < cfg_sb.lead_cancel_threshold_bps);
        assert!(cfg_sb.lead_cancel_threshold_bps < cfg_slow.lead_cancel_threshold_bps);
        assert!(cfg_fast.min_entry_edge_bps < cfg_slow.min_entry_edge_bps);
        assert!(cfg_fast.alo_reject_cooldown_ms < cfg_slow.alo_reject_cooldown_ms);
        assert_eq!(cfg_fast.lead_validation_min_samples, 5);
        assert_eq!(cfg_sb.lead_validation_min_samples, 5);
        assert_eq!(cfg_clob.lead_validation_min_samples, 5);
    }

    #[test]
    fn alo_flight_guard_skips_narrow_opening_chase_without_blocking_keep_or_reduce_only() {
        let mut cfg = base_cfg();
        cfg.min_penny_spread_ticks = 99;
        cfg.submit_rtt_ms = 40.0;
        cfg.alo_flight_guard_sigma_bps = 0.0;
        cfg.alo_flight_guard_obi_threshold = Some(0.50);
        let mut eng = QueuePriorityMmEngine::new(cfg).unwrap();

        // 1-tick book (100.00/100.01) with heavy ask pressure (obi = (1 - 9)/10 = -0.80):
        // Bid aligned_adverse_obi = +0.80 >= 0.50 -> fresh opening Bid skipped, Ask stays open.
        let skewed = book(100.00, 1.0, 100.01, 9.0, 1_000);
        let acts = eng.evaluate(Some(&skewed), &[], &[], InventoryState::default(), 1_000);
        assert!(!acts.iter().any(|a| matches!(
            a,
            MmAction::PlaceAlo {
                side: QuoteSide::Bid,
                reduce_only: false,
                ..
            }
        )));
        assert!(acts.iter().any(|a| matches!(
            a,
            MmAction::PlaceAlo {
                side: QuoteSide::Ask,
                reduce_only: false,
                ..
            }
        )));

        // Already-resting Bid must Keep its queue priority; reduce_only Bid must never be blocked.
        let resting = vec![RestingQuote {
            client_id: "b1".into(),
            side: QuoteSide::Bid,
            price: 100.00,
            qty: 0.3,
            level_idx: 0,
            placed_at_ms: 900,
        }];
        let acts_keep = eng.evaluate(
            Some(&skewed),
            &[],
            &resting,
            InventoryState::default(),
            1_100,
        );
        assert!(acts_keep.iter().any(|a| matches!(
            a,
            MmAction::Keep {
                side: QuoteSide::Bid,
                ..
            }
        )));

        let inv_short = InventoryState {
            target_qty: -0.25,
            ..Default::default()
        };
        let acts_red = eng.evaluate(Some(&skewed), &[], &[], inv_short, 1_200);
        assert!(acts_red.iter().any(|a| matches!(
            a,
            MmAction::PlaceAlo {
                side: QuoteSide::Bid,
                reduce_only: true,
                ..
            }
        )));

        // High flight sigma check: prime realized vol via rapid 50ms mid jump (>= 4 bps over 50ms => high sigma).
        let mut sig_cfg = base_cfg();
        sig_cfg.min_penny_spread_ticks = 99;
        sig_cfg.submit_rtt_ms = 40.0;
        sig_cfg.alo_flight_guard_sigma_bps = 0.25;
        let mut sig_eng = QueuePriorityMmEngine::new(sig_cfg).unwrap();
        let _ = sig_eng.evaluate(
            Some(&book(100.00, 10.0, 100.01, 10.0, 1_000)),
            &[],
            &[],
            InventoryState::default(),
            1_000,
        );
        // +20 bp jump in 200ms -> inst_var capped at 400 -> ewma > 0 -> sqrt(ewma) * sqrt(0.04) > 0.25
        let storm = book(100.20, 10.0, 100.21, 10.0, 1_200);
        let acts_storm = sig_eng.evaluate(Some(&storm), &[], &[], InventoryState::default(), 1_200);
        assert!(!acts_storm.iter().any(|a| matches!(
            a,
            MmAction::PlaceAlo {
                reduce_only: false,
                ..
            }
        )));
    }

    #[test]
    fn fast_five_sample_lead_validator_warms_up_five_moves_on_profile_preset() {
        let fast = VenueExecutionProfile::fast_zero_fee_alo();
        let mut cfg = QueuePriorityMmConfig::for_profile(&fast, 0.01, 0.001, 5.0, 30.0, 50.0, 3.0);
        cfg.lead_validation_horizon_ms = 500;
        let mut eng = QueuePriorityMmEngine::new(cfg).unwrap();
        let mut t = 1_000_u64;
        for step in 0..8 {
            let tgt_px = 100.00 + (step as f64) * 0.02;
            let ref_px = 100.04 + (step as f64) * 0.02;
            let b = book(tgt_px - 0.02, 10.0, tgt_px + 0.02, 10.0, t);
            let r = [ReferenceBookUpdate {
                venue_id: "leader".into(),
                bid: ref_px - 0.01,
                ask: ref_px + 0.01,
                received_at_ms: t,
            }];
            let _ = eng.evaluate(Some(&b), &r, &[], InventoryState::default(), t);
            t += 500;
        }
        assert_eq!(eng.telemetry().validated_lead_ref_count, 1);
        assert!(eng.telemetry().max_validated_lead_score_bps > 0.02);
    }
}
