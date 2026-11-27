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
    /// Avellaneda-Stoikov inventory skew coefficient (`gamma >= 0.0`, `0.0` disables).
    /// Scales reservation-price skew with normalized inventory and `(1.0 + sigma_1s^2)`.
    #[serde(default)]
    pub as_inventory_skew_gamma: f64,
    /// Causal volatility-adaptive half-spread multiplier (`>= 0.0`, `0.0` disables):
    /// adds `vol_spread_mult * sigma_1s_bps` bps to opening required half-spread.
    #[serde(default)]
    pub vol_spread_mult: f64,
    /// Asymmetric directional negative-markout skew multiplier (`>= 0.0`, `0.0` disables):
    /// widens/skews ONLY the side with `markout_ewma_bps < 0` by `(-markout_ewma_bps) * mult` bps.
    #[serde(default)]
    pub directional_markout_skew_mult: f64,
    /// Quiet-regime opening admission gate (`>= 0.0`, `0.0` disables):
    /// suppresses NEW opening quotes when `spread_bps / sigma_1s.max(0.05) < min_ratio`.
    #[serde(default)]
    pub quiet_regime_min_spread_to_vol: f64,
    /// Quiet-regime max entry volatility gate (`bps / sqrt(s)`): suppresses NEW opening quotes
    /// when `sigma_1s > max_vol`, while ALWAYS keeping inventory-reducing exits active.
    #[serde(default)]
    pub quiet_regime_max_entry_vol_bps: Option<f64>,
    /// Weight in `[0.0, 1.0]` on L1 size-weighted microprice offset relative to mid (`0.0` disables).
    #[serde(default)]
    pub microprice_alpha_weight: f64,
    /// Causal EWMA time constant (ms) for signed trade-flow imbalance (`0.0` disables TFI).
    #[serde(default)]
    pub tfi_ewma_tau_ms: f64,
    /// Directional reservation-price skew (bps) at full `|TFI| == 1.0` (`0.0` disables).
    #[serde(default)]
    pub tfi_skew_bps: f64,
    /// Normalized signed TFI magnitude threshold in `(0, 1]` that pulls/blocks endangered opening quotes (`0.0` disables).
    #[serde(default)]
    pub tfi_veto_threshold: f64,
    /// Immediate same-side opening stepback (bps) added after a passive fill (`0.0` disables).
    #[serde(default)]
    pub self_fill_stepback_bps: f64,
    /// Exponential decay time constant (ms) for post-fill self-adverse-selection stepback (`0` disables).
    #[serde(default)]
    pub self_fill_decay_ms: u64,
    /// Max ticks a `reduce_only` exit may step inside wide spreads (strictly post-only non-crossing; `0` preserves 1-tick default).
    #[serde(default)]
    pub reduce_only_penny_ticks: u32,
    /// Speed-bump cancel RTT (ms) used to scale the latency-adjusted opening spread floor (`0.0` disables).
    #[serde(default)]
    pub speed_bump_cancel_ms: f64,
    /// Multiplier on `sigma_1s * sqrt(speed_bump_cancel_ms / 1000)` added to opening required half-spread (`0.0` disables).
    #[serde(default)]
    pub latency_vol_spread_mult: f64,
    /// Weight in `[0.0, 1.0]` applied to validated cross-venue basis-adjusted fair shift (`0.0` disables).
    #[serde(default)]
    pub cross_venue_fair_weight: f64,
    /// Maximum absolute cross-venue fair-value shift in bps (`0.0` disables the cap).
    #[serde(default)]
    pub cross_venue_fair_cap_bps: f64,
    /// Adverse symbol markout EWMA threshold (bps > 0) triggering the rolling toxicity circuit breaker (`0.0` disables).
    #[serde(default)]
    pub toxicity_pause_ewma_bps: f64,
    /// Duration (ms) opening quotes remain paused after the toxicity circuit breaker triggers (`0` disables).
    #[serde(default)]
    pub toxicity_pause_ms: u64,
    /// Minimum matured fills required before the rolling symbol toxicity circuit breaker can arm (`0` allows immediate trigger).
    #[serde(default)]
    pub toxicity_min_fills: u32,
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
            as_inventory_skew_gamma: 0.0,
            vol_spread_mult: 0.0,
            directional_markout_skew_mult: 0.0,
            quiet_regime_min_spread_to_vol: 0.0,
            quiet_regime_max_entry_vol_bps: None,
            microprice_alpha_weight: 0.0,
            tfi_ewma_tau_ms: 0.0,
            tfi_skew_bps: 0.0,
            tfi_veto_threshold: 0.0,
            self_fill_stepback_bps: 0.0,
            self_fill_decay_ms: 0,
            reduce_only_penny_ticks: 0,
            speed_bump_cancel_ms: 0.0,
            latency_vol_spread_mult: 0.0,
            cross_venue_fair_weight: 0.0,
            cross_venue_fair_cap_bps: 0.0,
            toxicity_pause_ewma_bps: 0.0,
            toxicity_pause_ms: 0,
            toxicity_min_fills: 0,
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
        ensure!(
            self.as_inventory_skew_gamma.is_finite() && self.as_inventory_skew_gamma >= 0.0,
            "as_inventory_skew_gamma must be finite and >= 0"
        );
        ensure!(
            self.vol_spread_mult.is_finite() && self.vol_spread_mult >= 0.0,
            "vol_spread_mult must be finite and >= 0"
        );
        ensure!(
            self.directional_markout_skew_mult.is_finite()
                && self.directional_markout_skew_mult >= 0.0,
            "directional_markout_skew_mult must be finite and >= 0"
        );
        ensure!(
            self.quiet_regime_min_spread_to_vol.is_finite()
                && self.quiet_regime_min_spread_to_vol >= 0.0,
            "quiet_regime_min_spread_to_vol must be finite and >= 0"
        );
        if let Some(max_vol) = self.quiet_regime_max_entry_vol_bps {
            ensure!(
                max_vol.is_finite() && max_vol > 0.0,
                "quiet_regime_max_entry_vol_bps must be finite and > 0"
            );
        }
        ensure!(
            self.microprice_alpha_weight.is_finite()
                && self.microprice_alpha_weight >= 0.0
                && self.microprice_alpha_weight <= 1.0,
            "microprice_alpha_weight must be finite and in [0, 1]"
        );
        ensure!(
            self.tfi_ewma_tau_ms.is_finite() && self.tfi_ewma_tau_ms >= 0.0,
            "tfi_ewma_tau_ms must be finite and >= 0"
        );
        ensure!(
            self.tfi_skew_bps.is_finite() && self.tfi_skew_bps >= 0.0,
            "tfi_skew_bps must be finite and >= 0"
        );
        ensure!(
            self.tfi_veto_threshold.is_finite()
                && self.tfi_veto_threshold >= 0.0
                && self.tfi_veto_threshold <= 1.0,
            "tfi_veto_threshold must be finite and in [0, 1]"
        );
        ensure!(
            self.self_fill_stepback_bps.is_finite() && self.self_fill_stepback_bps >= 0.0,
            "self_fill_stepback_bps must be finite and >= 0"
        );
        ensure!(
            self.speed_bump_cancel_ms.is_finite() && self.speed_bump_cancel_ms >= 0.0,
            "speed_bump_cancel_ms must be finite and >= 0"
        );
        ensure!(
            self.latency_vol_spread_mult.is_finite() && self.latency_vol_spread_mult >= 0.0,
            "latency_vol_spread_mult must be finite and >= 0"
        );
        ensure!(
            self.cross_venue_fair_weight.is_finite()
                && self.cross_venue_fair_weight >= 0.0
                && self.cross_venue_fair_weight <= 1.0,
            "cross_venue_fair_weight must be finite and in [0, 1]"
        );
        ensure!(
            self.cross_venue_fair_cap_bps.is_finite() && self.cross_venue_fair_cap_bps >= 0.0,
            "cross_venue_fair_cap_bps must be finite and >= 0"
        );
        ensure!(
            self.toxicity_pause_ewma_bps.is_finite() && self.toxicity_pause_ewma_bps >= 0.0,
            "toxicity_pause_ewma_bps must be finite and >= 0"
        );
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
    pub microprice_offset_bps: f64,
    pub tfi_norm: f64,
    pub symbol_toxicity_ewma_bps: f64,
    pub toxicity_paused: bool,
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
    tfi_signed_vol_ewma: f64,
    tfi_total_vol_ewma: f64,
    last_tfi_ts_ms: Option<u64>,
    bid_self_fill_stepback_bps: f64,
    ask_self_fill_stepback_bps: f64,
    bid_self_fill_last_ms: Option<u64>,
    ask_self_fill_last_ms: Option<u64>,
    symbol_toxicity_ewma_bps: f64,
    symbol_toxicity_fills: u32,
    toxicity_pause_until_ms: u64,
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
            tfi_signed_vol_ewma: 0.0,
            tfi_total_vol_ewma: 0.0,
            last_tfi_ts_ms: None,
            bid_self_fill_stepback_bps: 0.0,
            ask_self_fill_stepback_bps: 0.0,
            bid_self_fill_last_ms: None,
            ask_self_fill_last_ms: None,
            symbol_toxicity_ewma_bps: 0.0,
            symbol_toxicity_fills: 0,
            toxicity_pause_until_ms: 0,
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

    /// Record a fill for causal post-fill adverse-markout feedback and optional self-fill stepback (`reduce_only` unwinds remain exempt).
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
        if self.cfg.self_fill_stepback_bps > 0.0 && self.cfg.self_fill_decay_ms > 0 {
            let current = self.decayed_self_fill_stepback(side, fill_ts_ms);
            let updated = (current + self.cfg.self_fill_stepback_bps)
                .min(3.0 * self.cfg.self_fill_stepback_bps);
            match side {
                QuoteSide::Bid => {
                    self.bid_self_fill_stepback_bps = updated;
                    self.bid_self_fill_last_ms = Some(
                        self.bid_self_fill_last_ms
                            .map_or(fill_ts_ms, |prev| prev.max(fill_ts_ms)),
                    );
                }
                QuoteSide::Ask => {
                    self.ask_self_fill_stepback_bps = updated;
                    self.ask_self_fill_last_ms = Some(
                        self.ask_self_fill_last_ms
                            .map_or(fill_ts_ms, |prev| prev.max(fill_ts_ms)),
                    );
                }
            }
        }
    }

    /// Alias for [`Self::on_trade_print`] updating signed Trade-Flow Imbalance (TFI) and sweep shields.
    pub fn on_market_trade(
        &mut self,
        print: FastTradePrint,
        target_book: Option<&VenueBookUpdate>,
    ) {
        self.on_trade_print(print, target_book);
    }

    /// Notify engine of an incoming fast trade print to update signed TFI EWMA and arm side-specific sweep protection.
    pub fn on_trade_print(&mut self, print: FastTradePrint, target_book: Option<&VenueBookUpdate>) {
        if !print.price.is_finite()
            || !print.size.is_finite()
            || print.price <= 0.0
            || print.size <= 0.0
        {
            return;
        }
        let trade_notional = print.price * print.size;
        if self.cfg.tfi_ewma_tau_ms > 0.0 {
            self.decay_tfi_to(print.received_at_ms);
            let is_stale_order = self.last_tfi_ts_ms.is_some_and(|prev| {
                prev.saturating_sub(print.received_at_ms) > self.cfg.max_book_age_ms
            });
            if !is_stale_order {
                let sgn = match print.taker_side {
                    QuoteSide::Bid => 1.0,
                    QuoteSide::Ask => -1.0,
                };
                self.tfi_signed_vol_ewma += sgn * trade_notional;
                self.tfi_total_vol_ewma += trade_notional;
                self.last_tfi_ts_ms = Some(
                    self.last_tfi_ts_ms
                        .map_or(print.received_at_ms, |prev| prev.max(print.received_at_ms)),
                );
            }
        }
        let Some(book) = target_book else {
            return;
        };
        if !book.is_valid() {
            return;
        }
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
            if self.symbol_toxicity_fills == 0 {
                self.symbol_toxicity_ewma_bps = mk_bps;
            } else {
                self.symbol_toxicity_ewma_bps =
                    0.80 * self.symbol_toxicity_ewma_bps + 0.20 * mk_bps;
            }
            self.symbol_toxicity_fills = self.symbol_toxicity_fills.saturating_add(1);
            if self.cfg.toxicity_pause_ewma_bps > 0.0
                && self.cfg.toxicity_pause_ms > 0
                && self.symbol_toxicity_fills >= self.cfg.toxicity_min_fills
                && self.symbol_toxicity_ewma_bps < -self.cfg.toxicity_pause_ewma_bps
            {
                self.toxicity_pause_until_ms = self
                    .toxicity_pause_until_ms
                    .max(now_ms.saturating_add(self.cfg.toxicity_pause_ms));
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

        // 1. Check inventory finiteness & session drawdown halt invariant (fail-closed on NaN/Inf).
        let total_pnl = inventory.realized_pnl_usd + inventory.unrealized_pnl_usd;
        if !inventory.target_qty.is_finite()
            || !inventory.external_net_qty.is_finite()
            || !total_pnl.is_finite()
        {
            return cancel_all_resting(resting, "invalid_inventory_state");
        }
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
        let (best_bid, best_bid_sz) = book.best_bid().expect("validated");
        let (best_ask, best_ask_sz) = book.best_ask().expect("validated");
        let target_mid = 0.5 * (best_bid + best_ask);
        let target_spread_bps = (best_ask - best_bid) / target_mid * 10_000.0;
        let l1_sz_sum = best_bid_sz + best_ask_sz;
        let microprice_offset_bps = if l1_sz_sum > 0.0 && target_mid > 0.0 {
            let microprice = (best_bid * best_ask_sz + best_ask * best_bid_sz) / l1_sz_sum;
            ((microprice / target_mid) - 1.0) * 10_000.0
        } else {
            0.0
        };
        let tfi_norm = self.current_tfi_norm(now_ms);
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
            let gap_bps = if raw_basis_bps.is_finite() && ewma.is_finite() {
                raw_basis_bps - ewma
            } else {
                0.0
            };
            let fv = if self.cfg.cross_venue_fair_weight > 0.0 {
                let weighted_shift = self.cfg.cross_venue_fair_weight * gap_bps;
                let clamped_shift = if self.cfg.cross_venue_fair_cap_bps > 0.0 {
                    weighted_shift.clamp(
                        -self.cfg.cross_venue_fair_cap_bps,
                        self.cfg.cross_venue_fair_cap_bps,
                    )
                } else {
                    weighted_shift
                };
                let shifted = target_mid * (clamped_shift / 10_000.0).exp();
                if shifted.is_finite() && shifted > 0.0 {
                    shifted
                } else {
                    target_mid
                }
            } else {
                let raw_fv = ref_mid * (-ewma / 10_000.0).exp();
                if raw_fv.is_finite() && raw_fv > 0.0 {
                    raw_fv
                } else {
                    target_mid
                }
            };
            (fv, gap_bps)
        } else {
            self.reset_basis_if_expired(now_ms);
            (target_mid, 0.0)
        };

        let toxicity_paused = self.cfg.toxicity_pause_ewma_bps > 0.0
            && self.cfg.toxicity_pause_ms > 0
            && now_ms < self.toxicity_pause_until_ms;
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
            microprice_offset_bps,
            tfi_norm,
            symbol_toxicity_ewma_bps: self.symbol_toxicity_ewma_bps,
            toxicity_paused,
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

        // 7. Compute skewed reservation price (with optional Avellaneda-Stoikov inventory gamma).
        let pos_notional = inventory.target_qty * target_mid;
        let inv_ratio = (pos_notional / self.cfg.max_position_notional_usd).clamp(-1.5, 1.5);
        // Smooth linear + quadratic skew: gentle near zero, firm near cap.
        let skew_factor = 0.5 * inv_ratio + 0.5 * inv_ratio * inv_ratio.abs();
        let as_skew_bps = if self.cfg.as_inventory_skew_gamma > 0.0 {
            inv_ratio * self.cfg.as_inventory_skew_gamma * (1.0 + self.var_bps2_per_s)
        } else {
            0.0
        };
        let skew_bps = skew_factor * self.cfg.inventory_skew_bps_per_full_limit + as_skew_bps;
        let pre_move_alpha_bps = self.cfg.microprice_alpha_weight * microprice_offset_bps
            + self.cfg.tfi_skew_bps * tfi_norm;
        let directional_fair_px = if pre_move_alpha_bps != 0.0 {
            fair_value * (pre_move_alpha_bps / 10_000.0).exp()
        } else {
            fair_value
        };
        let reservation_px = directional_fair_px * (-skew_bps / 10_000.0).exp();

        let tfi_bid_vetoed =
            self.cfg.tfi_veto_threshold > 0.0 && (-tfi_norm) >= self.cfg.tfi_veto_threshold;
        let tfi_ask_vetoed =
            self.cfg.tfi_veto_threshold > 0.0 && tfi_norm >= self.cfg.tfi_veto_threshold;

        let quiet_ratio_blocked = self.cfg.quiet_regime_min_spread_to_vol > 0.0
            && (target_spread_bps / sigma_bps_per_sqrt_s.max(0.05))
                < self.cfg.quiet_regime_min_spread_to_vol;
        let quiet_vol_blocked = self
            .cfg
            .quiet_regime_max_entry_vol_bps
            .is_some_and(|max_vol| sigma_bps_per_sqrt_s > max_vol);
        let quiet_entry_blocked = quiet_ratio_blocked || quiet_vol_blocked;
        let vol_half_spread_bps = self.cfg.vol_spread_mult * sigma_bps_per_sqrt_s;
        let latency_vol_half_spread_bps =
            if self.cfg.speed_bump_cancel_ms > 0.0 && self.cfg.latency_vol_spread_mult > 0.0 {
                self.cfg.latency_vol_spread_mult
                    * sigma_bps_per_sqrt_s
                    * (self.cfg.speed_bump_cancel_ms / 1_000.0).sqrt()
            } else {
                0.0
            };
        let as_max_skew_bps = if self.cfg.as_inventory_skew_gamma > 0.0 {
            1.5 * self.cfg.as_inventory_skew_gamma * (1.0 + self.var_bps2_per_s)
        } else {
            0.0
        };
        let max_microprice_skew_bps = if self.cfg.microprice_alpha_weight > 0.0 {
            self.cfg.microprice_alpha_weight * (target_spread_bps * 0.5)
        } else {
            0.0
        };

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
            let reducing_edge_floor_bps = self.cfg.maker_fee_bps
                - self.cfg.inventory_skew_bps_per_full_limit
                - as_max_skew_bps
                - self.cfg.tfi_skew_bps
                - max_microprice_skew_bps;
            let max_pos_qty = self.cfg.max_position_notional_usd / target_mid;
            let tfi_vetoed = match side {
                QuoteSide::Bid => tfi_bid_vetoed,
                QuoteSide::Ask => tfi_ask_vetoed,
            };
            let side_self_fill_stepback = self.decayed_self_fill_stepback(side, now_ms);

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
            let asym_markout_skew_bps =
                if self.cfg.directional_markout_skew_mult > 0.0 && side_markout_ewma < 0.0 {
                    (-side_markout_ewma) * self.cfg.directional_markout_skew_mult
                } else {
                    0.0
                };
            let opening_req_half_bps = min_edge_total_bps
                + vol_half_spread_bps
                + latency_vol_half_spread_bps
                + asym_markout_skew_bps
                + side_self_fill_stepback;
            let use_g4_quote_skew = self.cfg.as_inventory_skew_gamma > 0.0
                || self.cfg.vol_spread_mult > 0.0
                || latency_vol_half_spread_bps > 0.0
                || self.cfg.cross_venue_fair_weight > 0.0
                || self.cfg.directional_markout_skew_mult > 0.0
                || self.cfg.microprice_alpha_weight > 0.0
                || self.cfg.tfi_skew_bps > 0.0
                || side_self_fill_stepback > 0.0;

            for lvl in 0..self.cfg.ladder_levels {
                let remaining_to_flat = (-committed_qty).max(0.0);
                let is_reducing = remaining_to_flat > self.cfg.lot_size * 0.5;
                let opening_penny_ticks = u32::from(can_penny_open);
                let reducing_penny_ticks = if is_reducing && spread_ticks >= 2 {
                    self.cfg.reduce_only_penny_ticks.max(1)
                } else {
                    0
                };
                let penny_ticks = if is_reducing {
                    reducing_penny_ticks.max(opening_penny_ticks)
                } else {
                    opening_penny_ticks
                };
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
                        let base = if penny_ticks > 0 {
                            (best_bid + (penny_ticks as f64) * self.cfg.tick_size)
                                .min(best_ask - self.cfg.tick_size)
                        } else {
                            best_bid
                        };
                        base - stepback_ticks * self.cfg.tick_size
                    }
                    QuoteSide::Ask => {
                        let base = if penny_ticks > 0 {
                            (best_ask - (penny_ticks as f64) * self.cfg.tick_size)
                                .max(best_bid + self.cfg.tick_size)
                        } else {
                            best_ask
                        };
                        base + stepback_ticks * self.cfg.tick_size
                    }
                };
                let level_anchor = if !is_reducing && use_g4_quote_skew {
                    let skewed_limit_px = match side {
                        QuoteSide::Bid => reservation_px * (-opening_req_half_bps / 10_000.0).exp(),
                        QuoteSide::Ask => reservation_px * (opening_req_half_bps / 10_000.0).exp(),
                    };
                    match side {
                        QuoteSide::Bid => touch_l1.min(skewed_limit_px),
                        QuoteSide::Ask => touch_l1.max(skewed_limit_px),
                    }
                } else {
                    touch_l1
                };
                let desired_px_raw = match side {
                    QuoteSide::Bid => level_anchor - (lvl as f64) * self.cfg.tick_size,
                    QuoteSide::Ask => level_anchor + (lvl as f64) * self.cfg.tick_size,
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
                    desired_edge_bps >= opening_req_half_bps
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
                        r_edge_bps >= opening_req_half_bps
                    } else {
                        r_edge_bps >= reducing_edge_floor_bps
                    };
                    // Keep resting queue priority while order stays non-crossing, clears edge,
                    // sits within `max_queue_keep_ticks` of desired_px, stays within position cap,
                    // and is not an opening order blocked by quiet-regime or TFI veto gates.
                    let within_keep = tick_diff <= self.cfg.max_queue_keep_ticks;
                    let better_reducing_penny = is_reducing
                        && self.cfg.reduce_only_penny_ticks > 0
                        && match side {
                            QuoteSide::Bid => desired_px > r.price + self.cfg.tick_size * 0.1,
                            QuoteSide::Ask => desired_px < r.price - self.cfg.tick_size * 0.1,
                        };
                    let would_exceed_cap = committed_qty + r.qty > max_pos_qty + 1e-12;
                    let quiet_blocked_resting = quiet_entry_blocked && r_opens_past_flat;
                    let tfi_blocked_resting = tfi_vetoed && r_opens_past_flat;
                    let toxicity_blocked_resting = toxicity_paused && r_opens_past_flat;
                    if !r_crosses
                        && r_edge_ok
                        && within_keep
                        && !better_reducing_penny
                        && !would_exceed_cap
                        && !quiet_blocked_resting
                        && !tfi_blocked_resting
                        && !toxicity_blocked_resting
                    {
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
                        reason: if toxicity_blocked_resting {
                            "toxicity_circuit_breaker"
                        } else if quiet_blocked_resting {
                            "quiet_regime_gate"
                        } else if tfi_blocked_resting {
                            "tfi_adverse_veto"
                        } else if would_exceed_cap {
                            "position_cap_trim"
                        } else {
                            "reprice_out_of_band"
                        },
                    });
                }

                // Opening-only entry gates (quiet-regime gate, TFI veto, toxicity breaker, ALO cooldown,
                // severe markout shield cooldown, OBI veto, and narrow-spread ALO flight-collision guard).
                // Quiet-regime gate, TFI veto, and toxicity breaker cancel resting opening orders above while
                // preserving pure `reduce_only` exits (`!r_opens_past_flat`); none of these gates block exits.
                if !is_reducing {
                    if quiet_entry_blocked || tfi_vetoed || toxicity_paused {
                        continue;
                    }
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

    fn decay_tfi_to(&mut self, now_ms: u64) {
        if self.cfg.tfi_ewma_tau_ms <= 0.0 {
            return;
        }
        if let Some(prev_ms) = self.last_tfi_ts_ms
            && now_ms > prev_ms
        {
            let dt = now_ms - prev_ms;
            if dt > self.cfg.max_book_age_ms {
                self.tfi_signed_vol_ewma = 0.0;
                self.tfi_total_vol_ewma = 0.0;
            } else {
                let decay = (-(dt as f64) / self.cfg.tfi_ewma_tau_ms).exp();
                self.tfi_signed_vol_ewma *= decay;
                self.tfi_total_vol_ewma *= decay;
            }
            self.last_tfi_ts_ms = Some(now_ms);
        }
    }

    fn current_tfi_norm(&mut self, now_ms: u64) -> f64 {
        if self.cfg.tfi_ewma_tau_ms <= 0.0 {
            return 0.0;
        }
        self.decay_tfi_to(now_ms);
        if self.tfi_total_vol_ewma > 1e-12 {
            (self.tfi_signed_vol_ewma / self.tfi_total_vol_ewma).clamp(-1.0, 1.0)
        } else {
            0.0
        }
    }

    fn decayed_self_fill_stepback(&self, side: QuoteSide, now_ms: u64) -> f64 {
        if self.cfg.self_fill_stepback_bps <= 0.0 || self.cfg.self_fill_decay_ms == 0 {
            return 0.0;
        }
        let (val, last_ms) = match side {
            QuoteSide::Bid => (self.bid_self_fill_stepback_bps, self.bid_self_fill_last_ms),
            QuoteSide::Ask => (self.ask_self_fill_stepback_bps, self.ask_self_fill_last_ms),
        };
        match last_ms {
            Some(prev) if now_ms >= prev => {
                val * (-((now_ms - prev) as f64) / (self.cfg.self_fill_decay_ms as f64)).exp()
            }
            Some(_) => val,
            None => 0.0,
        }
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

    #[test]
    fn g4_defaults_remain_backward_compatible_off() {
        let cfg = QueuePriorityMmConfig::default();
        assert_eq!(cfg.as_inventory_skew_gamma, 0.0);
        assert_eq!(cfg.vol_spread_mult, 0.0);
        assert_eq!(cfg.directional_markout_skew_mult, 0.0);
        assert_eq!(cfg.quiet_regime_min_spread_to_vol, 0.0);
        assert_eq!(cfg.quiet_regime_max_entry_vol_bps, None);

        // Deserializing legacy TOML/JSON without G4 fields keeps all new gates disabled.
        let legacy_json = serde_json::to_value(&cfg).unwrap();
        let mut obj = legacy_json.as_object().unwrap().clone();
        obj.remove("as_inventory_skew_gamma");
        obj.remove("vol_spread_mult");
        obj.remove("directional_markout_skew_mult");
        obj.remove("quiet_regime_min_spread_to_vol");
        obj.remove("quiet_regime_max_entry_vol_bps");
        let parsed: QueuePriorityMmConfig =
            serde_json::from_value(serde_json::Value::Object(obj)).unwrap();
        assert_eq!(parsed, cfg);

        let mut bad_cfg = base_cfg();
        bad_cfg.quiet_regime_max_entry_vol_bps = Some(0.0);
        assert!(bad_cfg.validate().is_err());
        bad_cfg.quiet_regime_max_entry_vol_bps = None;
        bad_cfg.as_inventory_skew_gamma = -0.1;
        assert!(bad_cfg.validate().is_err());
    }

    #[test]
    fn g4_as_inventory_skew_and_vol_adaptive_spread_widen_opening_quotes() {
        let mut cfg = base_cfg();
        cfg.min_penny_spread_ticks = 99;
        cfg.as_inventory_skew_gamma = 2.0;
        cfg.vol_spread_mult = 1.5;
        let mut eng = QueuePriorityMmEngine::new(cfg).unwrap();

        // Warm up realized vol (~2.54 bps/sqrt(s) from 10 bps move over 1s).
        let _ = eng.evaluate(
            Some(&book(100.00, 10.0, 100.04, 10.0, 1_000)),
            &[],
            &[],
            InventoryState::default(),
            1_000,
        );
        let acts_vol = eng.evaluate(
            Some(&book(100.10, 10.0, 100.14, 10.0, 2_000)),
            &[],
            &[],
            InventoryState::default(),
            2_000,
        );
        let bid_px = acts_vol
            .iter()
            .find_map(|a| match a {
                MmAction::PlaceAlo {
                    side: QuoteSide::Bid,
                    price,
                    ..
                } => Some(*price),
                _ => None,
            })
            .expect("expected widened bid");
        let ask_px = acts_vol
            .iter()
            .find_map(|a| match a {
                MmAction::PlaceAlo {
                    side: QuoteSide::Ask,
                    price,
                    ..
                } => Some(*price),
                _ => None,
            })
            .expect("expected widened ask");
        // With vol_spread_mult * sigma (~3.81 bps) > 2.0 bps book half-spread, both quotes widen strictly beyond touch.
        assert!(bid_px < 100.10, "expected bid < 100.10, got {bid_px}");
        assert!(ask_px > 100.14, "expected ask > 100.14, got {ask_px}");

        // Now add LONG inventory (+0.30): AS gamma skew pushes Bid even lower while Ask reduces at touch!
        let inv_long = InventoryState {
            target_qty: 0.30,
            ..Default::default()
        };
        let acts_inv = eng.evaluate(
            Some(&book(100.10, 10.0, 100.14, 10.0, 2_250)),
            &[],
            &[],
            inv_long,
            2_250,
        );
        let bid_skewed = acts_inv
            .iter()
            .find_map(|a| match a {
                MmAction::PlaceAlo {
                    side: QuoteSide::Bid,
                    price,
                    reduce_only: false,
                    ..
                } => Some(*price),
                _ => None,
            })
            .expect("expected skewed opening bid");
        let ask_reduce = acts_inv
            .iter()
            .find_map(|a| match a {
                MmAction::PlaceAlo {
                    side: QuoteSide::Ask,
                    price,
                    reduce_only: true,
                    ..
                } => Some(*price),
                _ => None,
            })
            .expect("expected reduce-only ask");
        assert!(
            bid_skewed < bid_px,
            "AS skew should push opening bid lower when long: {bid_skewed} vs {bid_px}"
        );
        assert!(
            ask_reduce <= 100.14,
            "reducing ask should stay at touch or better: {ask_reduce}"
        );
    }

    #[test]
    fn g4_directional_markout_skew_widens_only_negative_markout_side() {
        let mut cfg = base_cfg();
        cfg.min_penny_spread_ticks = 99;
        cfg.markout_shield_horizon_ms = 500;
        cfg.markout_shield_adverse_threshold_bps = 100.0; // disable symmetric cooldown so skew is isolated
        cfg.directional_markout_skew_mult = 2.0;
        let mut eng = QueuePriorityMmEngine::new(cfg).unwrap();

        // Adverse BID fill: filled Bid at 100.10, mid drops to 100.02 (-8 bps) -> bid_markout_ewma = -2.0 bps.
        // Ask has 0.0 markout EWMA, so ONLY Bid should widen (> 4 bps skew); Ask stays at touch 100.04!
        eng.on_fill(QuoteSide::Bid, 100.10, 0.3, 1_000);
        let b = book(100.00, 10.0, 100.04, 10.0, 1_600);
        let acts = eng.evaluate(Some(&b), &[], &[], InventoryState::default(), 1_600);
        assert!(eng.telemetry().bid_markout_ewma_bps < -1.0);
        assert_eq!(eng.telemetry().ask_markout_ewma_bps, 0.0);

        let bid_px = acts
            .iter()
            .find_map(|a| match a {
                MmAction::PlaceAlo {
                    side: QuoteSide::Bid,
                    price,
                    ..
                } => Some(*price),
                _ => None,
            })
            .expect("expected skewed bid");
        let ask_px = acts
            .iter()
            .find_map(|a| match a {
                MmAction::PlaceAlo {
                    side: QuoteSide::Ask,
                    price,
                    ..
                } => Some(*price),
                _ => None,
            })
            .expect("expected unskewed ask");
        assert!(
            bid_px < 100.00,
            "negative bid markout should widen bid below 100.00, got {bid_px}"
        );
        assert!(
            (ask_px - 100.04).abs() < 1e-6,
            "non-negative ask markout should leave ask at touch 100.04, got {ask_px}"
        );
    }

    #[test]
    fn g4_quiet_regime_gates_suppress_opening_but_preserve_reducing_exits() {
        // 1. Spread-to-vol ratio gate (`spread_bps / max(sigma_1s, 0.05) < min_ratio`).
        let mut cfg_tight = base_cfg();
        cfg_tight.tick_size = 0.005;
        cfg_tight.min_penny_spread_ticks = 99;
        cfg_tight.min_entry_edge_bps = 0.05;
        cfg_tight.quiet_regime_min_spread_to_vol = 20.0; // requires spread >= 20 * max(sigma, 0.05) = 1.0 bp at sigma=0
        let mut eng_tight = QueuePriorityMmEngine::new(cfg_tight).unwrap();
        // 0.50 bp spread (100.00 / 100.005, mid=100.0025) at sigma=0 => ratio=10 < 20 -> blocked!
        let tight_b = VenueBookUpdate {
            bids: vec![(100.00, 10.0)],
            asks: vec![(100.005, 10.0)],
            received_at_ms: 1_000,
            source_ms: Some(1_000),
        };
        let acts_blocked =
            eng_tight.evaluate(Some(&tight_b), &[], &[], InventoryState::default(), 1_000);
        assert!(
            !acts_blocked
                .iter()
                .any(|a| matches!(a, MmAction::PlaceAlo { .. })),
            "expected opening quotes suppressed when spread/vol < min_ratio, got {acts_blocked:?}"
        );

        // Same mid (100.0025) with wider 4.0 bp spread (99.9825 / 100.0225) keeps sigma=0 -> ratio = 80 >= 20 -> allowed!
        let wide_b = VenueBookUpdate {
            bids: vec![(99.9825, 10.0)],
            asks: vec![(100.0225, 10.0)],
            received_at_ms: 1_500,
            source_ms: Some(1_500),
        };
        let acts_ok = eng_tight.evaluate(Some(&wide_b), &[], &[], InventoryState::default(), 1_500);
        assert_eq!(
            acts_ok
                .iter()
                .filter(|a| matches!(
                    a,
                    MmAction::PlaceAlo {
                        reduce_only: false,
                        ..
                    }
                ))
                .count(),
            2
        );

        // 2. Max entry volatility gate (`sigma_1s > max_entry_vol_bps`): suppresses opening quotes
        // under volatile regime while ALWAYS keeping reduce_only exits active when position != 0.
        let mut cfg_vol = base_cfg();
        cfg_vol.quiet_regime_min_spread_to_vol = 0.80;
        cfg_vol.quiet_regime_max_entry_vol_bps = Some(1.2);
        let mut eng_vol = QueuePriorityMmEngine::new(cfg_vol).unwrap();

        let _ = eng_vol.evaluate(
            Some(&book(100.00, 10.0, 100.04, 10.0, 1_000)),
            &[],
            &[],
            InventoryState::default(),
            1_000,
        );
        // Trigger volatile jump (+15 bps in 500ms -> sigma_1s > 1.2 bps/sqrt(s)).
        let storm_b = book(100.15, 10.0, 100.19, 10.0, 1_500);
        let acts_storm_flat =
            eng_vol.evaluate(Some(&storm_b), &[], &[], InventoryState::default(), 1_500);
        assert!(eng_vol.telemetry().sigma_bps_per_sqrt_s > 1.2);
        assert!(
            !acts_storm_flat
                .iter()
                .any(|a| matches!(a, MmAction::PlaceAlo { .. })),
            "expected all new opening quotes suppressed when sigma > max_entry_vol_bps"
        );

        // Now hold LONG inventory (+0.30): Ask exit MUST remain active (reduce_only=true), while Bid stays suppressed!
        let inv_long = InventoryState {
            target_qty: 0.30,
            ..Default::default()
        };
        let acts_storm_inv = eng_vol.evaluate(Some(&storm_b), &[], &[], inv_long, 1_750);
        assert!(
            !acts_storm_inv.iter().any(|a| matches!(
                a,
                MmAction::PlaceAlo {
                    side: QuoteSide::Bid,
                    ..
                }
            )),
            "opening bid must remain suppressed in volatile regime"
        );
        assert!(
            acts_storm_inv.iter().any(|a| matches!(
                a,
                MmAction::PlaceAlo {
                    side: QuoteSide::Ask,
                    reduce_only: true,
                    qty,
                    ..
                } if *qty > 0.0 && *qty <= 0.30 + 1e-6
            )),
            "reducing ask exit MUST stay active under volatile regime, got {acts_storm_inv:?}"
        );
    }

    #[test]
    fn g4_quiet_regime_gate_cancels_resting_opening_orders_but_keeps_reducing_exits() {
        let mut cfg = base_cfg();
        cfg.min_penny_spread_ticks = 99;
        cfg.vol_spread_mult = 0.0;
        cfg.quiet_regime_max_entry_vol_bps = Some(1.2);
        let mut eng = QueuePriorityMmEngine::new(cfg).unwrap();

        // Seed calm book then trigger a volatile jump so `quiet_entry_blocked == true`.
        let _ = eng.evaluate(
            Some(&book(100.00, 10.0, 100.04, 10.0, 1_000)),
            &[],
            &[],
            InventoryState::default(),
            1_000,
        );
        let storm_b = book(100.15, 10.0, 100.19, 10.0, 1_500);
        let resting_pair = vec![
            RestingQuote {
                client_id: "b_open".into(),
                side: QuoteSide::Bid,
                price: 100.15,
                qty: 0.30,
                level_idx: 0,
                placed_at_ms: 1_200,
            },
            RestingQuote {
                client_id: "a_open".into(),
                side: QuoteSide::Ask,
                price: 100.19,
                qty: 0.30,
                level_idx: 0,
                placed_at_ms: 1_200,
            },
        ];

        // Flat inventory: BOTH resting opening orders must cancel with reason "quiet_regime_gate"
        // (and neither Keep nor PlaceAlo may be emitted).
        let acts_flat = eng.evaluate(
            Some(&storm_b),
            &[],
            &resting_pair,
            InventoryState::default(),
            1_500,
        );
        assert!(eng.telemetry().sigma_bps_per_sqrt_s > 1.2);
        assert!(
            acts_flat.iter().any(|a| matches!(
                a,
                MmAction::Cancel {
                    client_id,
                    side: QuoteSide::Bid,
                    reason: "quiet_regime_gate",
                } if client_id == "b_open"
            )),
            "expected resting opening bid cancelled under quiet_regime_gate, got {acts_flat:?}"
        );
        assert!(
            acts_flat.iter().any(|a| matches!(
                a,
                MmAction::Cancel {
                    client_id,
                    side: QuoteSide::Ask,
                    reason: "quiet_regime_gate",
                } if client_id == "a_open"
            )),
            "expected resting opening ask cancelled under quiet_regime_gate, got {acts_flat:?}"
        );
        assert!(
            !acts_flat
                .iter()
                .any(|a| matches!(a, MmAction::Keep { .. } | MmAction::PlaceAlo { .. })),
            "flat quiet-blocked state must not keep or place opening quotes, got {acts_flat:?}"
        );

        // Long inventory (+0.30): resting opening Bid cancels, while pure reducing Ask keeps queue priority!
        let inv_long = InventoryState {
            target_qty: 0.30,
            ..Default::default()
        };
        let acts_long = eng.evaluate(Some(&storm_b), &[], &resting_pair, inv_long, 1_600);
        assert!(
            acts_long.iter().any(|a| matches!(
                a,
                MmAction::Cancel {
                    client_id,
                    side: QuoteSide::Bid,
                    reason: "quiet_regime_gate",
                } if client_id == "b_open"
            )),
            "long inventory must cancel opening bid under quiet_regime_gate, got {acts_long:?}"
        );
        assert!(
            acts_long.iter().any(|a| matches!(
                a,
                MmAction::Keep {
                    client_id,
                    side: QuoteSide::Ask,
                    ..
                } if client_id == "a_open"
            )),
            "long inventory must KEEP pure reducing ask exit, got {acts_long:?}"
        );
    }

    #[test]
    fn microstructure_tfi_self_fill_and_reduce_penny_defaults_remain_off_and_validate() {
        let cfg = QueuePriorityMmConfig::default();
        assert_eq!(cfg.microprice_alpha_weight, 0.0);
        assert_eq!(cfg.tfi_ewma_tau_ms, 0.0);
        assert_eq!(cfg.tfi_skew_bps, 0.0);
        assert_eq!(cfg.tfi_veto_threshold, 0.0);
        assert_eq!(cfg.self_fill_stepback_bps, 0.0);
        assert_eq!(cfg.self_fill_decay_ms, 0);
        assert_eq!(cfg.reduce_only_penny_ticks, 0);

        let legacy_json = serde_json::to_value(&cfg).unwrap();
        let mut obj = legacy_json.as_object().unwrap().clone();
        for key in [
            "microprice_alpha_weight",
            "tfi_ewma_tau_ms",
            "tfi_skew_bps",
            "tfi_veto_threshold",
            "self_fill_stepback_bps",
            "self_fill_decay_ms",
            "reduce_only_penny_ticks",
        ] {
            obj.remove(key);
        }
        let parsed: QueuePriorityMmConfig =
            serde_json::from_value(serde_json::Value::Object(obj)).unwrap();
        assert_eq!(parsed, cfg);

        let mut bad = base_cfg();
        bad.microprice_alpha_weight = 1.2;
        assert!(bad.validate().is_err());
        bad.microprice_alpha_weight = 0.5;
        bad.tfi_veto_threshold = 1.1;
        assert!(bad.validate().is_err());
        bad.tfi_veto_threshold = 0.5;
        bad.self_fill_stepback_bps = -0.1;
        assert!(bad.validate().is_err());
    }

    #[test]
    fn microprice_and_tfi_skew_and_veto_pull_endangered_openings_before_mid_jumps() {
        let mut cfg = base_cfg();
        cfg.min_penny_spread_ticks = 99;
        cfg.min_entry_edge_bps = 0.2;
        cfg.microprice_alpha_weight = 1.0;
        cfg.tfi_ewma_tau_ms = 2_000.0;
        cfg.tfi_skew_bps = 3.0;
        cfg.tfi_veto_threshold = 0.65;
        let mut eng = QueuePriorityMmEngine::new(cfg).unwrap();

        // Book has 4-tick spread (100.00 / 100.04, mid=100.02, half-spread=2 bps) with heavy BID L1
        // (bid_sz=190, ask_sz=10 => microprice=100.038, offset=+1.8 bps) + aggressive BUY trade flow
        // sized below sweep_print_pull_ratio (5.0 < 0.6 * 10.0) to isolate TFI veto.
        let b = book(100.00, 190.0, 100.04, 10.0, 1_000);
        eng.on_market_trade(
            FastTradePrint {
                taker_side: QuoteSide::Bid,
                price: 100.04,
                size: 5.0,
                received_at_ms: 1_000,
            },
            Some(&b),
        );
        // Existing resting opening Ask at 100.04 and Bid at 100.00.
        let resting = vec![
            RestingQuote {
                client_id: "b_open".into(),
                side: QuoteSide::Bid,
                price: 100.00,
                qty: 0.30,
                level_idx: 0,
                placed_at_ms: 900,
            },
            RestingQuote {
                client_id: "a_open".into(),
                side: QuoteSide::Ask,
                price: 100.04,
                qty: 0.30,
                level_idx: 0,
                placed_at_ms: 900,
            },
        ];
        let acts = eng.evaluate(Some(&b), &[], &resting, InventoryState::default(), 1_050);
        assert!(eng.telemetry().microprice_offset_bps > 1.5);
        assert!(eng.telemetry().tfi_norm > 0.9);
        assert!(
            acts.iter().any(|a| matches!(
                a,
                MmAction::Cancel {
                    client_id,
                    side: QuoteSide::Ask,
                    ..
                } if client_id == "a_open"
            )),
            "expected endangered opening Ask pulled on strong buy TFI + microprice, got {acts:?}"
        );
        assert!(
            !acts.iter().any(|a| matches!(
                a,
                MmAction::PlaceAlo {
                    side: QuoteSide::Ask,
                    reduce_only: false,
                    ..
                }
            )),
            "TFI veto must block new opening Ask during buy surge"
        );
        assert!(
            acts.iter().any(|a| matches!(
                a,
                MmAction::Keep {
                    client_id,
                    side: QuoteSide::Bid,
                    ..
                } if client_id == "b_open"
            )),
            "safe opening Bid should keep queue priority"
        );

        // With LONG inventory, Ask is reduce_only and MUST NOT be blocked by TFI veto or skew!
        let inv_long = InventoryState {
            target_qty: 0.30,
            ..Default::default()
        };
        let acts_red = eng.evaluate(Some(&b), &[], &[], inv_long, 1_100);
        assert!(
            acts_red.iter().any(|a| matches!(
                a,
                MmAction::PlaceAlo {
                    side: QuoteSide::Ask,
                    reduce_only: true,
                    qty,
                    ..
                } if *qty > 0.0 && *qty <= 0.30 + 1e-6
            )),
            "TFI veto must NEVER block reduce_only exits, got {acts_red:?}"
        );
    }

    #[test]
    fn self_fill_stepback_widens_opening_requotes_and_decays_without_blocking_exits() {
        let mut cfg = base_cfg();
        cfg.min_penny_spread_ticks = 99;
        cfg.self_fill_stepback_bps = 3.0;
        cfg.self_fill_decay_ms = 1_000;
        let mut eng = QueuePriorityMmEngine::new(cfg).unwrap();

        let b = book(100.00, 10.0, 100.04, 10.0, 1_000);
        // Passive Bid fill at t=1_000 adds 3.0 bps stepback -> opening_req_half_bps = 3.2 bps
        // (mid=100.02 -> skewed bid ~ 99.988 -> rounded down to 99.98 < best_bid 100.00).
        eng.on_fill(QuoteSide::Bid, 100.00, 0.30, 1_000);
        let acts_immediate = eng.evaluate(Some(&b), &[], &[], InventoryState::default(), 1_050);
        let stepped_bid = acts_immediate
            .iter()
            .find_map(|a| match a {
                MmAction::PlaceAlo {
                    side: QuoteSide::Bid,
                    price,
                    reduce_only: false,
                    ..
                } => Some(*price),
                _ => None,
            })
            .expect("expected stepped-back opening bid");
        assert!(
            stepped_bid < 100.00,
            "self-fill stepback should push opening bid below touch 100.00, got {stepped_bid}"
        );

        // Consecutive same-side fill compounds stepback further!
        eng.on_fill(QuoteSide::Bid, 99.98, 0.30, 1_100);
        let acts_compound = eng.evaluate(Some(&b), &[], &[], InventoryState::default(), 1_150);
        let compounded_bid = acts_compound
            .iter()
            .find_map(|a| match a {
                MmAction::PlaceAlo {
                    side: QuoteSide::Bid,
                    price,
                    reduce_only: false,
                    ..
                } => Some(*price),
                _ => None,
            })
            .expect("expected compounded stepped-back bid");
        assert!(
            compounded_bid < stepped_bid,
            "second same-side fill should compound stepback: {compounded_bid} vs {stepped_bid}"
        );

        // Short inventory -> Bid is reduce_only and stays at touch/pennied (NOT stepped back!).
        let inv_short = InventoryState {
            target_qty: -0.30,
            ..Default::default()
        };
        let acts_red = eng.evaluate(Some(&b), &[], &[], inv_short, 1_150);
        let red_bid = acts_red
            .iter()
            .find_map(|a| match a {
                MmAction::PlaceAlo {
                    side: QuoteSide::Bid,
                    price,
                    reduce_only: true,
                    ..
                } => Some(*price),
                _ => None,
            })
            .expect("expected reduce_only bid");
        assert!(
            red_bid >= 100.00,
            "reduce_only bid must not be stepped back below touch, got {red_bid}"
        );

        // After 5s (5x decay_ms), stepback decays back to touch 100.00 (fresh book at t=6_500).
        let b_decayed = book(100.00, 10.0, 100.04, 10.0, 6_500);
        let acts_decayed =
            eng.evaluate(Some(&b_decayed), &[], &[], InventoryState::default(), 6_500);
        let decayed_bid = acts_decayed
            .iter()
            .find_map(|a| match a {
                MmAction::PlaceAlo {
                    side: QuoteSide::Bid,
                    price,
                    reduce_only: false,
                    ..
                } => Some(*price),
                _ => None,
            })
            .expect("expected decayed opening bid");
        assert!(
            (decayed_bid - 100.00).abs() < 1e-6,
            "stepback should decay back to touch 100.00, got {decayed_bid}"
        );
    }

    #[test]
    fn patient_passive_pennying_unwind_steps_inside_wide_spread_non_crossing() {
        let mut cfg = base_cfg();
        cfg.min_penny_spread_ticks = 99; // keep opening quotes strictly at touch
        cfg.reduce_only_penny_ticks = 2;
        let mut eng = QueuePriorityMmEngine::new(cfg).unwrap();

        // 6-tick wide spread: 100.00 / 100.06, long +0.30 -> Ask reduce_only steps 2 ticks inside to 100.04,
        // while opening Bid stays at touch 100.00!
        let b_wide = book(100.00, 10.0, 100.06, 10.0, 1_000);
        let inv_long = InventoryState {
            target_qty: 0.30,
            ..Default::default()
        };
        let acts_wide = eng.evaluate(Some(&b_wide), &[], &[], inv_long, 1_000);
        let ask_wide = acts_wide
            .iter()
            .find_map(|a| match a {
                MmAction::PlaceAlo {
                    side: QuoteSide::Ask,
                    price,
                    reduce_only: true,
                    ..
                } => Some(*price),
                _ => None,
            })
            .expect("expected reduce_only ask");
        let bid_wide = acts_wide
            .iter()
            .find_map(|a| match a {
                MmAction::PlaceAlo {
                    side: QuoteSide::Bid,
                    price,
                    reduce_only: false,
                    ..
                } => Some(*price),
                _ => None,
            })
            .expect("expected opening bid");
        assert!(
            (ask_wide - 100.04).abs() < 1e-6,
            "expected 2-tick patient passive-pennying unwind at 100.04, got {ask_wide}"
        );
        assert!(
            (bid_wide - 100.00).abs() < 1e-6,
            "opening bid must remain at touch 100.00, got {bid_wide}"
        );

        // 2-tick spread (100.00 / 100.02): reduce_only_penny_ticks=2 clamps strictly non-crossing at 100.01!
        let b_two = book(100.00, 10.0, 100.02, 10.0, 1_200);
        let acts_two = eng.evaluate(Some(&b_two), &[], &[], inv_long, 1_200);
        let ask_two = acts_two
            .iter()
            .find_map(|a| match a {
                MmAction::PlaceAlo {
                    side: QuoteSide::Ask,
                    price,
                    reduce_only: true,
                    ..
                } => Some(*price),
                _ => None,
            })
            .expect("expected clamped reduce_only ask");
        assert!(
            (ask_two - 100.01).abs() < 1e-6 && ask_two > 100.00,
            "2-tick clamp must keep reduce_only ask strictly post-only at 100.01, got {ask_two}"
        );

        // 1-tick spread (100.00 / 100.01): never crosses best_bid!
        let b_one = book(100.00, 10.0, 100.01, 10.0, 1_400);
        let acts_one = eng.evaluate(Some(&b_one), &[], &[], inv_long, 1_400);
        let ask_one = acts_one
            .iter()
            .find_map(|a| match a {
                MmAction::PlaceAlo {
                    side: QuoteSide::Ask,
                    price,
                    reduce_only: true,
                    ..
                } => Some(*price),
                _ => None,
            })
            .expect("expected 1-tick touch reduce_only ask");
        assert!(
            (ask_one - 100.01).abs() < 1e-6,
            "1-tick book must stay at best_ask 100.01, got {ask_one}"
        );
    }

    #[test]
    fn speedbump_defaults_remain_backward_compatible_off_and_validate() {
        let cfg = QueuePriorityMmConfig::default();
        assert_eq!(cfg.speed_bump_cancel_ms, 0.0);
        assert_eq!(cfg.latency_vol_spread_mult, 0.0);
        assert_eq!(cfg.cross_venue_fair_weight, 0.0);
        assert_eq!(cfg.cross_venue_fair_cap_bps, 0.0);
        assert_eq!(cfg.toxicity_pause_ewma_bps, 0.0);
        assert_eq!(cfg.toxicity_pause_ms, 0);
        assert_eq!(cfg.toxicity_min_fills, 0);
        assert_eq!(cfg.reduce_only_penny_ticks, 0);

        let legacy_json = serde_json::to_value(&cfg).unwrap();
        let mut obj = legacy_json.as_object().unwrap().clone();
        for key in [
            "speed_bump_cancel_ms",
            "latency_vol_spread_mult",
            "cross_venue_fair_weight",
            "cross_venue_fair_cap_bps",
            "toxicity_pause_ewma_bps",
            "toxicity_pause_ms",
            "toxicity_min_fills",
        ] {
            obj.remove(key);
        }
        let parsed: QueuePriorityMmConfig =
            serde_json::from_value(serde_json::Value::Object(obj)).unwrap();
        assert_eq!(parsed, cfg);

        let mut bad = base_cfg();
        bad.speed_bump_cancel_ms = -1.0;
        assert!(bad.validate().is_err());
        bad.speed_bump_cancel_ms = 300.0;
        bad.latency_vol_spread_mult = f64::NAN;
        assert!(bad.validate().is_err());
        bad.latency_vol_spread_mult = 1.0;
        bad.cross_venue_fair_weight = 1.25;
        assert!(bad.validate().is_err());
        bad.cross_venue_fair_weight = 0.5;
        bad.cross_venue_fair_cap_bps = -0.1;
        assert!(bad.validate().is_err());
        bad.cross_venue_fair_cap_bps = 3.0;
        bad.toxicity_pause_ewma_bps = f64::INFINITY;
        assert!(bad.validate().is_err());
    }

    #[test]
    fn latency_adjusted_spread_floor_widens_opening_quotes_without_blocking_reduce_only() {
        let mut cfg = base_cfg();
        cfg.min_penny_spread_ticks = 99;
        cfg.speed_bump_cancel_ms = 300.0;
        cfg.latency_vol_spread_mult = 4.0;
        let mut eng = QueuePriorityMmEngine::new(cfg).unwrap();

        // Warm up realized vol (~2.54 bps/sqrt(s) from 10 bps move over 1s).
        // Latency floor = 4.0 * 2.54 * sqrt(0.3) ~ 5.56 bps > 2.0 bps book half-spread.
        let _ = eng.evaluate(
            Some(&book(100.00, 10.0, 100.04, 10.0, 1_000)),
            &[],
            &[],
            InventoryState::default(),
            1_000,
        );
        let acts_open = eng.evaluate(
            Some(&book(100.10, 10.0, 100.14, 10.0, 2_000)),
            &[],
            &[],
            InventoryState::default(),
            2_000,
        );
        let bid_open = acts_open
            .iter()
            .find_map(|a| match a {
                MmAction::PlaceAlo {
                    side: QuoteSide::Bid,
                    price,
                    reduce_only: false,
                    ..
                } => Some(*price),
                _ => None,
            })
            .expect("expected widened opening bid");
        let ask_open = acts_open
            .iter()
            .find_map(|a| match a {
                MmAction::PlaceAlo {
                    side: QuoteSide::Ask,
                    price,
                    reduce_only: false,
                    ..
                } => Some(*price),
                _ => None,
            })
            .expect("expected widened opening ask");
        assert!(
            bid_open < 100.10,
            "latency spread floor must push opening bid below touch 100.10, got {bid_open}"
        );
        assert!(
            ask_open > 100.14,
            "latency spread floor must push opening ask above touch 100.14, got {ask_open}"
        );

        // Long inventory (+0.30): reduce_only Ask must remain exempt (stays at touch <= 100.14).
        let inv_long = InventoryState {
            target_qty: 0.30,
            ..Default::default()
        };
        let acts_red = eng.evaluate(
            Some(&book(100.10, 10.0, 100.14, 10.0, 2_250)),
            &[],
            &[],
            inv_long,
            2_250,
        );
        let ask_red = acts_red
            .iter()
            .find_map(|a| match a {
                MmAction::PlaceAlo {
                    side: QuoteSide::Ask,
                    price,
                    reduce_only: true,
                    ..
                } => Some(*price),
                _ => None,
            })
            .expect("expected exempt reduce_only ask");
        assert!(
            ask_red <= 100.14,
            "reduce_only exit must remain exempt from latency spread widening, got {ask_red}"
        );
    }

    #[test]
    fn cross_venue_fair_shift_clamps_causally_and_skews_reservation_price() {
        let mut cfg = base_cfg();
        cfg.min_penny_spread_ticks = 99;
        cfg.require_validated_lead_ref = true;
        cfg.lead_validation_horizon_ms = 500;
        cfg.lead_validation_min_move_bps = 1.0;
        cfg.lead_validation_min_samples = 2;
        cfg.lead_validation_min_score_bps = 0.20;
        cfg.lead_cancel_threshold_bps = 50.0; // isolate fair-shift quoting without lead-lag cancel
        cfg.cross_venue_fair_weight = 0.50;
        cfg.cross_venue_fair_cap_bps = 3.0;
        let mut eng = QueuePriorityMmEngine::new(cfg).unwrap();

        // Warm up causal lead validation across two reference venues (arcus & trade_xyz).
        let mut t = 1_000_u64;
        for step in 0..4 {
            let tgt_px = 100.00 + (step as f64) * 0.03;
            let ref_px = 100.03 + (step as f64) * 0.03;
            let b = book(tgt_px - 0.02, 10.0, tgt_px + 0.02, 10.0, t);
            let refs = [
                ReferenceBookUpdate {
                    venue_id: "arcus".into(),
                    bid: ref_px - 0.01,
                    ask: ref_px + 0.01,
                    received_at_ms: t,
                },
                ReferenceBookUpdate {
                    venue_id: "trade_xyz".into(),
                    bid: ref_px - 0.01,
                    ask: ref_px + 0.01,
                    received_at_ms: t,
                },
            ];
            let _ = eng.evaluate(Some(&b), &refs, &[], InventoryState::default(), t);
            t += 500;
        }
        assert_eq!(eng.telemetry().validated_lead_ref_count, 2);

        // External consensus jumps +20 bps (100.20 -> 100.40) while slow target stays at 100.09/100.13 (mid 100.11).
        // Unclamped 0.5 * ~20 bps = ~10 bps must be clamped strictly to +3.0 bps (~100.1400)!
        let slow_b = book(100.09, 10.0, 100.13, 10.0, t);
        let consensus_jump = [
            ReferenceBookUpdate {
                venue_id: "arcus".into(),
                bid: 100.39,
                ask: 100.41,
                received_at_ms: t,
            },
            ReferenceBookUpdate {
                venue_id: "trade_xyz".into(),
                bid: 100.39,
                ask: 100.41,
                received_at_ms: t,
            },
        ];
        let acts = eng.evaluate(
            Some(&slow_b),
            &consensus_jump,
            &[],
            InventoryState::default(),
            t,
        );
        let fv = eng.telemetry().fair_value;
        let shift_bps = (fv / 100.11).ln() * 10_000.0;
        assert!(
            (shift_bps - 3.0).abs() < 0.05,
            "expected fair shift clamped at +3.0 bps, got {shift_bps:.4} bps"
        );
        // Shifted reservation price pushes opening Ask strictly above touch 100.13 while Bid stays <= 100.09.
        let ask_px = acts
            .iter()
            .find_map(|a| match a {
                MmAction::PlaceAlo {
                    side: QuoteSide::Ask,
                    price,
                    reduce_only: false,
                    ..
                } => Some(*price),
                _ => None,
            })
            .expect("expected shifted opening ask");
        assert!(
            ask_px > 100.13,
            "positive cross-venue fair shift must raise opening ask above 100.13, got {ask_px}"
        );
    }

    #[test]
    fn toxicity_circuit_breaker_suppresses_opening_quotes_while_keeping_reduce_only_exits() {
        let mut cfg = base_cfg();
        cfg.min_penny_spread_ticks = 99;
        cfg.markout_shield_horizon_ms = 500;
        cfg.markout_shield_adverse_threshold_bps = 100.0; // isolate rolling symbol toxicity breaker
        cfg.toxicity_pause_ewma_bps = 2.0;
        cfg.toxicity_pause_ms = 3_000;
        cfg.toxicity_min_fills = 3;
        let mut eng = QueuePriorityMmEngine::new(cfg).unwrap();

        // 2 toxic fills (< min_fills=3): breaker must NOT arm yet.
        eng.on_fill(QuoteSide::Bid, 100.10, 0.3, 1_000);
        eng.on_fill(QuoteSide::Ask, 99.90, 0.3, 1_050);
        let b_mid = book(99.98, 10.0, 100.02, 10.0, 1_600);
        let acts_pre = eng.evaluate(Some(&b_mid), &[], &[], InventoryState::default(), 1_600);
        assert!(!eng.telemetry().toxicity_paused);
        assert!(acts_pre.iter().any(|a| matches!(
            a,
            MmAction::PlaceAlo {
                reduce_only: false,
                ..
            }
        )));

        // 3rd toxic fill matures -> symbol_toxicity_ewma < -2.0 bps -> circuit breaker arms for 3_000 ms!
        eng.on_fill(QuoteSide::Bid, 100.10, 0.3, 1_200);
        let resting = vec![
            RestingQuote {
                client_id: "b_open".into(),
                side: QuoteSide::Bid,
                price: 99.98,
                qty: 0.30,
                level_idx: 0,
                placed_at_ms: 1_600,
            },
            RestingQuote {
                client_id: "a_open".into(),
                side: QuoteSide::Ask,
                price: 100.02,
                qty: 0.30,
                level_idx: 0,
                placed_at_ms: 1_600,
            },
        ];
        let b_trig = book(99.98, 10.0, 100.02, 10.0, 1_800);
        let acts_paused = eng.evaluate(
            Some(&b_trig),
            &[],
            &resting,
            InventoryState::default(),
            1_800,
        );
        assert!(eng.telemetry().toxicity_paused);
        assert!(eng.telemetry().symbol_toxicity_ewma_bps < -2.0);
        assert!(acts_paused.iter().any(|a| matches!(
            a,
            MmAction::Cancel {
                client_id,
                reason: "toxicity_circuit_breaker",
                ..
            } if client_id == "b_open"
        )));
        assert!(acts_paused.iter().any(|a| matches!(
            a,
            MmAction::Cancel {
                client_id,
                reason: "toxicity_circuit_breaker",
                ..
            } if client_id == "a_open"
        )));
        assert!(
            !acts_paused
                .iter()
                .any(|a| matches!(a, MmAction::PlaceAlo { .. })),
            "flat paused symbol must not place any opening orders"
        );

        // Long inventory (+0.30) while paused: pure reducing Ask keeps queue priority & places reduce_only=true,
        // while opening Bid remains suppressed!
        let inv_long = InventoryState {
            target_qty: 0.30,
            ..Default::default()
        };
        let acts_red = eng.evaluate(Some(&b_trig), &[], &resting, inv_long, 2_000);
        assert!(acts_red.iter().any(|a| matches!(
            a,
            MmAction::Keep {
                client_id,
                side: QuoteSide::Ask,
                ..
            } if client_id == "a_open"
        )));
        assert!(acts_red.iter().any(|a| matches!(
            a,
            MmAction::Cancel {
                client_id,
                side: QuoteSide::Bid,
                reason: "toxicity_circuit_breaker",
            } if client_id == "b_open"
        )));

        let acts_red_place = eng.evaluate(Some(&b_trig), &[], &[], inv_long, 2_100);
        assert!(acts_red_place.iter().any(|a| matches!(
            a,
            MmAction::PlaceAlo {
                side: QuoteSide::Ask,
                reduce_only: true,
                ..
            }
        )));
        assert!(!acts_red_place.iter().any(|a| matches!(
            a,
            MmAction::PlaceAlo {
                side: QuoteSide::Bid,
                ..
            }
        )));

        // After 3_000 ms pause expires (1_800 + 3_000 = 4_800), quoting resumes cleanly.
        let b_resumed = book(99.98, 10.0, 100.02, 10.0, 5_000);
        let acts_resumed =
            eng.evaluate(Some(&b_resumed), &[], &[], InventoryState::default(), 5_000);
        assert!(!eng.telemetry().toxicity_paused);
        assert_eq!(
            acts_resumed
                .iter()
                .filter(|a| matches!(
                    a,
                    MmAction::PlaceAlo {
                        reduce_only: false,
                        ..
                    }
                ))
                .count(),
            2
        );
    }

    #[test]
    fn nan_or_inf_inputs_fail_closed() {
        let mut eng = QueuePriorityMmEngine::new(base_cfg()).unwrap();
        let resting = vec![RestingQuote {
            client_id: "b1".into(),
            side: QuoteSide::Bid,
            price: 100.00,
            qty: 0.30,
            level_idx: 0,
            placed_at_ms: 1_000,
        }];

        // NaN in target book -> stale_or_invalid_target_book cancel all.
        let nan_book = VenueBookUpdate {
            bids: vec![(f64::NAN, 10.0)],
            asks: vec![(100.04, 10.0)],
            received_at_ms: 1_000,
            source_ms: Some(1_000),
        };
        assert_eq!(
            eng.evaluate(
                Some(&nan_book),
                &[],
                &resting,
                InventoryState::default(),
                1_000
            ),
            vec![MmAction::Cancel {
                client_id: "b1".into(),
                side: QuoteSide::Bid,
                reason: "stale_or_invalid_target_book",
            }]
        );

        // Inf in inventory -> invalid_inventory_state cancel all.
        let good_book = book(100.00, 10.0, 100.04, 10.0, 1_000);
        let bad_inv = InventoryState {
            target_qty: f64::INFINITY,
            ..Default::default()
        };
        assert_eq!(
            eng.evaluate(Some(&good_book), &[], &resting, bad_inv, 1_000),
            vec![MmAction::Cancel {
                client_id: "b1".into(),
                side: QuoteSide::Bid,
                reason: "invalid_inventory_state",
            }]
        );

        // NaN in reference book -> stale_reference_book cancel all.
        let bad_ref = [ReferenceBookUpdate {
            venue_id: "arcus".into(),
            bid: f64::NAN,
            ask: 100.02,
            received_at_ms: 1_000,
        }];
        assert_eq!(
            eng.evaluate(
                Some(&good_book),
                &bad_ref,
                &resting,
                InventoryState::default(),
                1_000
            ),
            vec![MmAction::Cancel {
                client_id: "b1".into(),
                side: QuoteSide::Bid,
                reason: "stale_reference_book",
            }]
        );
    }
}
