//! Pure, zero-allocation hot-path strategy engine and deterministic P0 replay
//! for **Method 2 (`Arcus Passive PostOnly Maker x Lighter-RH IOC Taker Hedge`)**.
//!
//! Strictly decoupled from exchange clients: ZERO `use crate::exchanges::*` imports.
//! All risk reservations, monotonic clock checks, `on_passive_fill` precedence over
//! cancel-in-flight, non-zero `hedge_delay_ms`, bounded orphan retries, USDG/USDC
//! divergence halts, daily loss caps, and realized `$/M` cost circuit breakers live
//! here so offline unit tests, deterministic P0 replay, read-only shadow, and live
//! canary share one verified state machine.

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

pub const DEFAULT_HEDGE_DELAY_MS: u64 = 330;
pub const MAX_PENDING_HEDGES: usize = 32;
pub const MAX_LOTS_FIFO: usize = 64;

fn default_network() -> String {
    "mainnet".to_owned()
}
fn default_symbol() -> String {
    "NVDA".to_owned()
}
fn default_arcus_symbol() -> String {
    "NVDA-USD".to_owned()
}
fn default_lighter_rh_market_id() -> i64 {
    15
}
fn default_symbol_id() -> u16 {
    1005
}
fn default_arcus_exchange_id() -> u8 {
    7
}
fn default_lighter_rh_exchange_id() -> u8 {
    8
}
fn default_true() -> bool {
    true
}
fn default_tick_size() -> f64 {
    0.01
}
fn default_lot_size() -> f64 {
    0.01
}
fn default_min_order_size() -> f64 {
    0.01
}
fn default_min_notional_usd() -> f64 {
    10.0
}
fn default_clip_usd() -> f64 {
    100.0
}
fn default_max_inventory_usd() -> f64 {
    200.0
}
fn default_allocated_capital_usd() -> f64 {
    500.0
}
fn default_daily_loss_limit_pct() -> f64 {
    0.05
}
fn default_max_daily_loss_usd() -> f64 {
    25.0
}
fn default_arcus_taker_fee_bps() -> f64 {
    2.1375
}
fn default_fx_drag_bps() -> f64 {
    1.0
}
fn default_lead_beta() -> f64 {
    0.50
}
fn default_microprice_alpha_weight() -> f64 {
    0.25
}
fn default_basis_ewma_tau_ms() -> f64 {
    120_000.0
}
fn default_lead_ewma_tau_ms() -> f64 {
    1_500.0
}
fn default_max_lead_shift_bps() -> f64 {
    5.0
}
fn default_inventory_skew_bps() -> f64 {
    1.5
}
fn default_max_improve_ticks() -> u32 {
    1
}
fn default_requote_threshold_ticks() -> u32 {
    1
}
fn default_hedge_delay_ms() -> u64 {
    DEFAULT_HEDGE_DELAY_MS
}
fn default_submit_rtt_ms() -> u64 {
    50
}
fn default_cancel_rtt_ms() -> u64 {
    50
}
fn default_hedge_slippage_bps() -> f64 {
    5.0
}
fn default_max_book_age_ms() -> u64 {
    2_000
}
fn default_max_venue_clock_skew_ms() -> u64 {
    2_000
}
fn default_poll_interval_ms() -> u64 {
    100
}
fn default_max_consecutive_hedge_failures() -> u32 {
    3
}
fn default_max_orphan_retries() -> u32 {
    3
}
fn default_max_orphan_notional_usd() -> f64 {
    150.0
}
fn default_max_usdg_usdc_divergence_bps() -> f64 {
    30.0
}
fn default_max_peg_age_ms() -> u64 {
    60_000
}
fn default_max_cost_per_million_usd() -> f64 {
    5.0
}
fn default_min_cost_breaker_volume_usd() -> f64 {
    1_000.0
}
fn default_max_l1_depth_fraction() -> f64 {
    0.50
}
fn default_queue_ahead_fraction() -> f64 {
    0.50
}
fn default_bbo_shm_path() -> String {
    "/dev/shm/aleph-matrix".to_owned()
}
fn default_journal_path() -> PathBuf {
    PathBuf::from("state/arcus_rh_turnover_journal.ndjson")
}
fn default_daily_loss_state_path() -> PathBuf {
    PathBuf::from("state/arcus_rh_turnover_daily_loss.json")
}

/// Configuration for Method 2 (`Arcus Passive PostOnly Maker x Lighter-RH Taker Hedge`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ArcusRhTurnoverConfig {
    #[serde(default = "default_network")]
    pub network: String,
    #[serde(default = "default_symbol")]
    pub symbol: String,
    #[serde(default = "default_arcus_symbol")]
    pub arcus_symbol: String,
    #[serde(default = "default_lighter_rh_market_id")]
    pub lighter_rh_market_id: i64,
    #[serde(default = "default_symbol_id")]
    pub symbol_id: u16,
    #[serde(default = "default_arcus_exchange_id")]
    pub arcus_exchange_id: u8,
    #[serde(default = "default_lighter_rh_exchange_id")]
    pub lighter_rh_exchange_id: u8,
    #[serde(default = "default_true")]
    pub dry_run: bool,
    #[serde(default)]
    pub live_enabled: bool,
    #[serde(default = "default_true")]
    pub require_arcus_cancel_lease: bool,
    #[serde(default = "default_tick_size")]
    pub tick_size: f64,
    #[serde(default = "default_lot_size")]
    pub lot_size: f64,
    #[serde(default = "default_min_order_size")]
    pub min_order_size: f64,
    #[serde(default = "default_min_notional_usd")]
    pub min_notional_usd: f64,
    #[serde(default = "default_clip_usd")]
    pub clip_usd: f64,
    #[serde(default = "default_max_inventory_usd")]
    pub max_inventory_usd: f64,
    #[serde(default = "default_allocated_capital_usd")]
    pub allocated_capital_usd: f64,
    #[serde(default = "default_daily_loss_limit_pct")]
    pub daily_loss_limit_pct: f64,
    #[serde(default = "default_max_daily_loss_usd")]
    pub max_daily_loss_usd: f64,
    #[serde(default)]
    pub arcus_maker_fee_bps: f64,
    #[serde(default = "default_arcus_taker_fee_bps")]
    pub arcus_taker_fee_bps: f64,
    #[serde(default)]
    pub rh_taker_fee_bps: f64,
    #[serde(default = "default_fx_drag_bps")]
    pub fx_drag_bps: f64,
    /// Minimum net entry edge in bps (may be `<= 0.0` in zero-points turnover mode).
    #[serde(default)]
    pub entry_edge_bps: f64,
    #[serde(default = "default_lead_beta")]
    pub lead_beta: f64,
    #[serde(default = "default_microprice_alpha_weight")]
    pub microprice_alpha_weight: f64,
    #[serde(default = "default_basis_ewma_tau_ms")]
    pub basis_ewma_tau_ms: f64,
    #[serde(default = "default_lead_ewma_tau_ms")]
    pub lead_ewma_tau_ms: f64,
    #[serde(default = "default_max_lead_shift_bps")]
    pub max_lead_shift_bps: f64,
    #[serde(default = "default_inventory_skew_bps")]
    pub inventory_skew_bps: f64,
    #[serde(default = "default_max_improve_ticks")]
    pub max_improve_ticks: u32,
    #[serde(default = "default_requote_threshold_ticks")]
    pub requote_threshold_ticks: u32,
    /// Mandatory non-zero Lighter-RH taker hedge latency (330 ms default; 0 rejected by `--check`).
    #[serde(default = "default_hedge_delay_ms")]
    pub hedge_delay_ms: u64,
    #[serde(default = "default_submit_rtt_ms")]
    pub submit_rtt_ms: u64,
    #[serde(default = "default_cancel_rtt_ms")]
    pub cancel_rtt_ms: u64,
    #[serde(default = "default_hedge_slippage_bps")]
    pub hedge_slippage_bps: f64,
    #[serde(default = "default_max_book_age_ms")]
    pub max_book_age_ms: u64,
    #[serde(default = "default_max_venue_clock_skew_ms")]
    pub max_venue_clock_skew_ms: u64,
    #[serde(default = "default_poll_interval_ms")]
    pub poll_interval_ms: u64,
    #[serde(default = "default_max_consecutive_hedge_failures")]
    pub max_consecutive_hedge_failures: u32,
    #[serde(default = "default_max_orphan_retries")]
    pub max_orphan_retries: u32,
    #[serde(default = "default_max_orphan_notional_usd")]
    pub max_orphan_notional_usd: f64,
    #[serde(default = "default_max_usdg_usdc_divergence_bps")]
    pub max_usdg_usdc_divergence_bps: f64,
    #[serde(default = "default_max_peg_age_ms")]
    pub max_peg_age_ms: u64,
    #[serde(default)]
    pub require_fresh_usdg_usdc_peg: bool,
    #[serde(default = "default_max_cost_per_million_usd")]
    pub max_cost_per_million_usd: f64,
    #[serde(default = "default_min_cost_breaker_volume_usd")]
    pub min_cost_breaker_volume_usd: f64,
    #[serde(default = "default_max_l1_depth_fraction")]
    pub max_l1_depth_fraction: f64,
    #[serde(default = "default_queue_ahead_fraction")]
    pub queue_ahead_fraction: f64,
    #[serde(default)]
    pub declared_end_ms: Option<u64>,
    #[serde(default)]
    pub terminal_drain_ms: Option<u64>,
    #[serde(default)]
    pub warmup_ms: u64,
    #[serde(default)]
    pub shm_enabled: bool,
    #[serde(default = "default_bbo_shm_path")]
    pub bbo_shm_path: String,
    #[serde(default = "default_journal_path")]
    pub journal_path: PathBuf,
    #[serde(default = "default_daily_loss_state_path")]
    pub daily_loss_state_path: PathBuf,
}

impl Default for ArcusRhTurnoverConfig {
    fn default() -> Self {
        Self {
            network: default_network(),
            symbol: default_symbol(),
            arcus_symbol: default_arcus_symbol(),
            lighter_rh_market_id: default_lighter_rh_market_id(),
            symbol_id: default_symbol_id(),
            arcus_exchange_id: default_arcus_exchange_id(),
            lighter_rh_exchange_id: default_lighter_rh_exchange_id(),
            dry_run: true,
            live_enabled: false,
            require_arcus_cancel_lease: true,
            tick_size: default_tick_size(),
            lot_size: default_lot_size(),
            min_order_size: default_min_order_size(),
            min_notional_usd: default_min_notional_usd(),
            clip_usd: default_clip_usd(),
            max_inventory_usd: default_max_inventory_usd(),
            allocated_capital_usd: default_allocated_capital_usd(),
            daily_loss_limit_pct: default_daily_loss_limit_pct(),
            max_daily_loss_usd: default_max_daily_loss_usd(),
            arcus_maker_fee_bps: 0.0,
            arcus_taker_fee_bps: default_arcus_taker_fee_bps(),
            rh_taker_fee_bps: 0.0,
            fx_drag_bps: default_fx_drag_bps(),
            entry_edge_bps: 0.0,
            lead_beta: default_lead_beta(),
            microprice_alpha_weight: default_microprice_alpha_weight(),
            basis_ewma_tau_ms: default_basis_ewma_tau_ms(),
            lead_ewma_tau_ms: default_lead_ewma_tau_ms(),
            max_lead_shift_bps: default_max_lead_shift_bps(),
            inventory_skew_bps: default_inventory_skew_bps(),
            max_improve_ticks: default_max_improve_ticks(),
            requote_threshold_ticks: default_requote_threshold_ticks(),
            hedge_delay_ms: default_hedge_delay_ms(),
            submit_rtt_ms: default_submit_rtt_ms(),
            cancel_rtt_ms: default_cancel_rtt_ms(),
            hedge_slippage_bps: default_hedge_slippage_bps(),
            max_book_age_ms: default_max_book_age_ms(),
            max_venue_clock_skew_ms: default_max_venue_clock_skew_ms(),
            poll_interval_ms: default_poll_interval_ms(),
            max_consecutive_hedge_failures: default_max_consecutive_hedge_failures(),
            max_orphan_retries: default_max_orphan_retries(),
            max_orphan_notional_usd: default_max_orphan_notional_usd(),
            max_usdg_usdc_divergence_bps: default_max_usdg_usdc_divergence_bps(),
            max_peg_age_ms: default_max_peg_age_ms(),
            require_fresh_usdg_usdc_peg: false,
            max_cost_per_million_usd: default_max_cost_per_million_usd(),
            min_cost_breaker_volume_usd: default_min_cost_breaker_volume_usd(),
            max_l1_depth_fraction: default_max_l1_depth_fraction(),
            queue_ahead_fraction: default_queue_ahead_fraction(),
            declared_end_ms: None,
            terminal_drain_ms: None,
            warmup_ms: 0,
            shm_enabled: false,
            bbo_shm_path: default_bbo_shm_path(),
            journal_path: default_journal_path(),
            daily_loss_state_path: default_daily_loss_state_path(),
        }
    }
}

impl ArcusRhTurnoverConfig {
    /// Validate offline config invariants (rejects `hedge_delay_ms == 0`, bad tick/lot, missing budgets).
    pub fn validate(&self) -> Result<()> {
        ensure!(
            matches!(self.network.as_str(), "mainnet" | "testnet"),
            "network must be 'mainnet' or 'testnet'"
        );
        ensure!(!self.symbol.trim().is_empty(), "symbol must be non-empty");
        ensure!(
            !self.arcus_symbol.trim().is_empty(),
            "arcus_symbol must be non-empty"
        );
        ensure!(
            self.lighter_rh_market_id >= 0 && self.lighter_rh_market_id != 255,
            "lighter_rh_market_id ({}) invalid",
            self.lighter_rh_market_id
        );
        ensure!(
            self.symbol_id > 0 && self.symbol_id < 2048,
            "symbol_id ({}) must be in 1..2048",
            self.symbol_id
        );
        ensure!(
            self.arcus_exchange_id != self.lighter_rh_exchange_id,
            "arcus_exchange_id and lighter_rh_exchange_id must differ"
        );
        ensure!(
            self.hedge_delay_ms > 0,
            "hedge_delay_ms must be > 0 (0 ms instantaneous hedge is forbidden; default 330 ms)"
        );
        ensure!(
            self.tick_size.is_finite() && self.tick_size > 0.0,
            "tick_size must be finite and > 0"
        );
        ensure!(
            self.lot_size.is_finite() && self.lot_size > 0.0,
            "lot_size must be finite and > 0"
        );
        ensure!(
            self.min_order_size.is_finite() && self.min_order_size >= self.lot_size - 1e-12,
            "min_order_size must be finite and >= lot_size"
        );
        ensure!(
            self.min_notional_usd.is_finite() && self.min_notional_usd >= 0.0,
            "min_notional_usd must be finite and >= 0"
        );
        ensure!(
            self.clip_usd.is_finite()
                && self.clip_usd > 0.0
                && self.clip_usd >= self.min_notional_usd,
            "clip_usd must be finite, > 0, and >= min_notional_usd"
        );
        ensure!(
            self.max_inventory_usd.is_finite() && self.max_inventory_usd >= self.clip_usd,
            "max_inventory_usd must be finite and >= clip_usd"
        );
        ensure!(
            self.allocated_capital_usd.is_finite() && self.allocated_capital_usd > 0.0,
            "allocated_capital_usd must be finite and > 0"
        );
        ensure!(
            self.daily_loss_limit_pct.is_finite()
                && self.daily_loss_limit_pct > 0.0
                && self.daily_loss_limit_pct < 1.0,
            "daily_loss_limit_pct must be finite and in (0, 1)"
        );
        ensure!(
            self.max_daily_loss_usd.is_finite()
                && self.max_daily_loss_usd > 0.0
                && self.max_daily_loss_usd
                    <= self.allocated_capital_usd * self.daily_loss_limit_pct + 1e-9,
            "max_daily_loss_usd must be > 0 and <= allocated_capital_usd * daily_loss_limit_pct"
        );
        ensure!(
            self.arcus_maker_fee_bps.is_finite() && self.arcus_maker_fee_bps >= -5.0,
            "arcus_maker_fee_bps invalid"
        );
        ensure!(
            self.arcus_taker_fee_bps.is_finite() && self.arcus_taker_fee_bps >= 0.0,
            "arcus_taker_fee_bps must be >= 0"
        );
        ensure!(
            self.rh_taker_fee_bps.is_finite() && self.rh_taker_fee_bps >= 0.0,
            "rh_taker_fee_bps must be >= 0"
        );
        ensure!(
            self.fx_drag_bps.is_finite() && self.fx_drag_bps >= 0.0,
            "fx_drag_bps must be >= 0"
        );
        ensure!(
            self.entry_edge_bps.is_finite() && (-50.0..=500.0).contains(&self.entry_edge_bps),
            "entry_edge_bps must be finite and in [-50, 500]"
        );
        ensure!(
            self.lead_beta.is_finite() && (0.0..=2.0).contains(&self.lead_beta),
            "lead_beta must be in [0, 2]"
        );
        ensure!(
            self.microprice_alpha_weight.is_finite()
                && (0.0..=1.0).contains(&self.microprice_alpha_weight),
            "microprice_alpha_weight must be in [0, 1]"
        );
        ensure!(
            self.basis_ewma_tau_ms.is_finite() && self.basis_ewma_tau_ms > 0.0,
            "basis_ewma_tau_ms must be > 0"
        );
        ensure!(
            self.lead_ewma_tau_ms.is_finite() && self.lead_ewma_tau_ms > 0.0,
            "lead_ewma_tau_ms must be > 0"
        );
        ensure!(
            self.max_lead_shift_bps.is_finite() && self.max_lead_shift_bps >= 0.0,
            "max_lead_shift_bps must be >= 0"
        );
        ensure!(
            self.inventory_skew_bps.is_finite() && self.inventory_skew_bps >= 0.0,
            "inventory_skew_bps must be >= 0"
        );
        ensure!(
            self.hedge_slippage_bps.is_finite() && (0.0..=100.0).contains(&self.hedge_slippage_bps),
            "hedge_slippage_bps must be in [0, 100]"
        );
        ensure!(self.max_book_age_ms > 0, "max_book_age_ms must be > 0");
        ensure!(self.poll_interval_ms > 0, "poll_interval_ms must be > 0");
        ensure!(
            self.max_consecutive_hedge_failures > 0,
            "max_consecutive_hedge_failures must be > 0"
        );
        ensure!(
            self.max_orphan_retries > 0,
            "max_orphan_retries must be > 0"
        );
        ensure!(
            self.max_orphan_notional_usd.is_finite() && self.max_orphan_notional_usd > 0.0,
            "max_orphan_notional_usd must be > 0"
        );
        ensure!(
            self.max_usdg_usdc_divergence_bps.is_finite()
                && self.max_usdg_usdc_divergence_bps > 0.0,
            "max_usdg_usdc_divergence_bps must be > 0"
        );
        ensure!(self.max_peg_age_ms > 0, "max_peg_age_ms must be > 0");
        ensure!(
            self.max_cost_per_million_usd.is_finite() && self.max_cost_per_million_usd > 0.0,
            "max_cost_per_million_usd must be > 0"
        );
        ensure!(
            self.min_cost_breaker_volume_usd.is_finite() && self.min_cost_breaker_volume_usd >= 0.0,
            "min_cost_breaker_volume_usd must be >= 0"
        );
        ensure!(
            self.max_l1_depth_fraction.is_finite()
                && self.max_l1_depth_fraction > 0.0
                && self.max_l1_depth_fraction <= 1.0,
            "max_l1_depth_fraction must be in (0, 1]"
        );
        ensure!(
            self.queue_ahead_fraction.is_finite()
                && (0.0..=1.0).contains(&self.queue_ahead_fraction),
            "queue_ahead_fraction must be in [0, 1]"
        );
        ensure!(
            !self.journal_path.as_os_str().is_empty()
                && !self.daily_loss_state_path.as_os_str().is_empty(),
            "journal_path and daily_loss_state_path must be non-empty"
        );
        if self.live_enabled {
            ensure!(
                !self.dry_run,
                "unsafe config: live_enabled = true cannot be paired with dry_run = true"
            );
            ensure!(
                self.require_arcus_cancel_lease,
                "unsafe live config: require_arcus_cancel_lease must be true"
            );
        }
        Ok(())
    }

    /// Fail-closed gate for `--canary`: refuses unless `dry_run == false` AND `live_enabled == true`.
    pub fn validate_live_canary(&self, confirm_live_canary: bool) -> Result<()> {
        self.validate()?;
        ensure!(
            confirm_live_canary,
            "live canary refused: missing --confirm-live-canary"
        );
        ensure!(
            !self.dry_run,
            "live canary refused: config has dry_run = true (fail-closed)"
        );
        ensure!(
            self.live_enabled,
            "live canary refused: config has live_enabled = false (fail-closed)"
        );
        ensure!(
            self.require_arcus_cancel_lease,
            "live canary refused: require_arcus_cancel_lease must be true"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderSide {
    Buy,
    Sell,
}

impl OrderSide {
    #[inline]
    pub const fn opposite(self) -> Self {
        match self {
            Self::Buy => Self::Sell,
            Self::Sell => Self::Buy,
        }
    }

    #[inline]
    pub const fn sign(self) -> f64 {
        match self {
            Self::Buy => 1.0,
            Self::Sell => -1.0,
        }
    }

    #[inline]
    const fn idx(self) -> usize {
        match self {
            Self::Buy => 0,
            Self::Sell => 1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HaltReason {
    ClockRegression,
    ConsecutiveHedgeFailures,
    OrphanRetriesExhausted,
    ExcessOrphanNotional,
    UsdgUsdcDivergence,
    DailyLossLimit,
    CostPerMillionBreached,
    ExternalHalt,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VenueBook {
    pub bid_px: f64,
    pub ask_px: f64,
    pub bid_sz: f64,
    pub ask_sz: f64,
    pub source_ms: u64,
    pub receipt_ms: u64,
}

impl VenueBook {
    #[inline]
    pub fn is_valid(&self) -> bool {
        self.bid_px.is_finite()
            && self.ask_px.is_finite()
            && self.bid_sz.is_finite()
            && self.ask_sz.is_finite()
            && self.bid_px > 0.0
            && self.ask_px > self.bid_px
            && self.bid_sz > 0.0
            && self.ask_sz > 0.0
            && self.source_ms > 0
            && self.receipt_ms > 0
    }

    #[inline]
    pub fn mid(&self) -> f64 {
        0.5 * (self.bid_px + self.ask_px)
    }

    #[inline]
    pub fn microprice(&self, alpha_weight: f64) -> f64 {
        let mid = self.mid();
        let denom = self.bid_sz + self.ask_sz;
        if denom <= 0.0 {
            return mid;
        }
        let raw_micro = (self.bid_px * self.ask_sz + self.ask_px * self.bid_sz) / denom;
        mid + alpha_weight * (raw_micro - mid)
    }

    #[inline]
    pub fn is_fresh(&self, now_ms: u64, max_age_ms: u64, max_skew_ms: u64) -> bool {
        self.is_valid()
            && self.receipt_ms <= now_ms
            && now_ms.saturating_sub(self.receipt_ms) <= max_age_ms
            && self.source_ms <= now_ms.saturating_add(max_skew_ms)
            && now_ms.saturating_sub(self.source_ms) <= max_age_ms.saturating_add(max_skew_ms)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum QuoteActionKind {
    PlacePostOnly {
        side: OrderSide,
        price: f64,
        qty: f64,
        order_seq: u64,
    },
    Cancel {
        side: OrderSide,
        order_seq: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HedgeDispatch {
    pub hedge_id: u64,
    pub side: OrderSide,
    pub qty: f64,
    pub limit_price: f64,
    pub retry_attempt: u32,
    pub ref_maker_px: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct BookStepActions {
    pub quote_actions: [Option<QuoteActionKind>; 2],
    pub quote_action_len: u8,
    pub hedge_dispatch: Option<HedgeDispatch>,
    pub withdrew_stale: bool,
    pub stale_hedge_blocks: u32,
}

impl BookStepActions {
    #[inline]
    fn push_quote(&mut self, action: QuoteActionKind) {
        let idx = usize::from(self.quote_action_len);
        if idx < 2 {
            self.quote_actions[idx] = Some(action);
            self.quote_action_len += 1;
        }
    }

    #[inline]
    pub fn quote_slice(&self) -> &[Option<QuoteActionKind>] {
        &self.quote_actions[..usize::from(self.quote_action_len)]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlotStatus {
    Empty,
    Active,
    CancelPending,
}

#[derive(Debug, Clone, Copy)]
struct MakerSlot {
    status: SlotStatus,
    order_seq: u64,
    price: f64,
    remaining_qty: f64,
}

impl Default for MakerSlot {
    fn default() -> Self {
        Self {
            status: SlotStatus::Empty,
            order_seq: 0,
            price: 0.0,
            remaining_qty: 0.0,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct PendingHedge {
    occupied: bool,
    hedge_id: u64,
    side: OrderSide,
    qty: f64,
    exec_at_ms: u64,
    retry_attempt: u32,
    ref_maker_px: f64,
}

impl Default for PendingHedge {
    fn default() -> Self {
        Self {
            occupied: false,
            hedge_id: 0,
            side: OrderSide::Buy,
            qty: 0.0,
            exec_at_ms: 0,
            retry_attempt: 0,
            ref_maker_px: 0.0,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct OpenLot {
    occupied: bool,
    side: OrderSide,
    qty: f64,
    px: f64,
}

impl Default for OpenLot {
    fn default() -> Self {
        Self {
            occupied: false,
            side: OrderSide::Buy,
            qty: 0.0,
            px: 0.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct EngineMetricsSnapshot {
    pub last_callback_ms: u64,
    pub halted: bool,
    pub halt_reason: Option<HaltReason>,
    pub arc_confirmed_pos: f64,
    pub rh_confirmed_pos: f64,
    pub net_unhedged_pos: f64,
    pub arc_worst_long: f64,
    pub arc_worst_short: f64,
    pub rh_worst_long: f64,
    pub rh_worst_short: f64,
    pub pending_hedge_count: u32,
    pub pending_hedge_notional_usd: f64,
    pub arcus_volume_usd: f64,
    pub rh_volume_usd: f64,
    pub passive_fills: u64,
    pub hedges_dispatched: u64,
    pub hedges_completed: u64,
    pub orphan_retries: u64,
    pub consecutive_hedge_failures: u32,
    pub realized_spread_pnl_usd: f64,
    pub fees_usd: f64,
    pub fx_drag_usd: f64,
    pub realized_cash_pnl_usd: f64,
    pub unrealized_pnl_usd: f64,
    pub cost_per_m_arcus_usd: f64,
    pub cost_per_m_combined_usd: f64,
    pub basis_ewma_bps: f64,
}

#[inline]
pub fn round_price_to_tick(raw_px: f64, tick_size: f64, side: OrderSide) -> f64 {
    if !raw_px.is_finite() || raw_px <= 0.0 || !tick_size.is_finite() || tick_size <= 0.0 {
        return 0.0;
    }
    let scaled = raw_px / tick_size;
    let ticks = match side {
        OrderSide::Buy => (scaled + 1e-9).floor(),
        OrderSide::Sell => (scaled - 1e-9).ceil(),
    };
    if ticks <= 0.0 { 0.0 } else { ticks * tick_size }
}

#[inline]
pub fn round_qty_down_to_lot(raw_qty: f64, lot_size: f64) -> f64 {
    if !raw_qty.is_finite() || raw_qty <= 0.0 || !lot_size.is_finite() || lot_size <= 0.0 {
        return 0.0;
    }
    let lots = ((raw_qty / lot_size) + 1e-9).floor();
    if lots <= 0.0 { 0.0 } else { lots * lot_size }
}

/// Zero-allocation hot-path strategy state machine for Method 2.
pub struct ArcusRhTurnoverEngine {
    cfg: ArcusRhTurnoverConfig,
    start_ms: Option<u64>,
    last_callback_ms: u64,
    last_book_update_ms: u64,
    last_rh_micro_px: f64,
    rh_slow_ewma_px: f64,
    basis_ewma_bps: f64,
    basis_initialized: bool,
    last_arc_mid: f64,
    last_rh_mid: f64,

    // Peg guard
    last_peg_mid: f64,
    last_peg_ms: u64,
    has_peg: bool,

    // Arcus & Lighter-RH positions and reservations
    arc_confirmed_pos: f64,
    rh_confirmed_pos: f64,
    slots: [MakerSlot; 2],
    next_order_seq: u64,
    next_hedge_id: u64,

    pending_hedges: [PendingHedge; MAX_PENDING_HEDGES],
    in_flight_hedge: Option<PendingHedge>,
    open_lots: [OpenLot; MAX_LOTS_FIFO],

    // Counters & PnL ledger
    arcus_volume_usd: f64,
    rh_volume_usd: f64,
    passive_fills: u64,
    hedges_dispatched: u64,
    hedges_completed: u64,
    orphan_retries: u64,
    consecutive_hedge_failures: u32,
    realized_spread_pnl_usd: f64,
    fees_usd: f64,
    fx_drag_usd: f64,

    halt_reason: Option<HaltReason>,
}

impl ArcusRhTurnoverEngine {
    pub fn new(cfg: ArcusRhTurnoverConfig) -> Result<Self> {
        cfg.validate()?;
        Ok(Self {
            cfg,
            start_ms: None,
            last_callback_ms: 0,
            last_book_update_ms: 0,
            last_rh_micro_px: 0.0,
            rh_slow_ewma_px: 0.0,
            basis_ewma_bps: 0.0,
            basis_initialized: false,
            last_arc_mid: 0.0,
            last_rh_mid: 0.0,
            last_peg_mid: 1.0,
            last_peg_ms: 0,
            has_peg: false,
            arc_confirmed_pos: 0.0,
            rh_confirmed_pos: 0.0,
            slots: [MakerSlot::default(); 2],
            next_order_seq: 1,
            next_hedge_id: 1,
            pending_hedges: [PendingHedge::default(); MAX_PENDING_HEDGES],
            in_flight_hedge: None,
            open_lots: [OpenLot::default(); MAX_LOTS_FIFO],
            arcus_volume_usd: 0.0,
            rh_volume_usd: 0.0,
            passive_fills: 0,
            hedges_dispatched: 0,
            hedges_completed: 0,
            orphan_retries: 0,
            consecutive_hedge_failures: 0,
            realized_spread_pnl_usd: 0.0,
            fees_usd: 0.0,
            fx_drag_usd: 0.0,
            halt_reason: None,
        })
    }

    #[inline]
    pub fn config(&self) -> &ArcusRhTurnoverConfig {
        &self.cfg
    }

    #[inline]
    pub fn is_halted(&self) -> bool {
        self.halt_reason.is_some()
    }

    #[inline]
    pub fn halt_reason(&self) -> Option<HaltReason> {
        self.halt_reason
    }

    #[inline]
    pub fn latch_external_halt(&mut self) {
        if self.halt_reason.is_none() {
            self.halt_reason = Some(HaltReason::ExternalHalt);
        }
    }

    #[inline]
    fn check_monotonic_clock(&mut self, now_ms: u64) -> Result<()> {
        if now_ms == 0 || now_ms < self.last_callback_ms {
            if self.halt_reason.is_none() {
                self.halt_reason = Some(HaltReason::ClockRegression);
            }
            bail!(
                "monotonic clock regression: now_ms={now_ms} < last_callback_ms={}",
                self.last_callback_ms
            );
        }
        self.last_callback_ms = now_ms;
        Ok(())
    }

    /// Sync authoritative confirmed positions at startup/reconciliation boundary.
    pub fn reconcile_positions(&mut self, now_ms: u64, arc_pos: f64, rh_pos: f64) -> Result<()> {
        self.check_monotonic_clock(now_ms)?;
        ensure!(
            arc_pos.is_finite() && rh_pos.is_finite(),
            "positions must be finite"
        );
        self.arc_confirmed_pos = arc_pos;
        self.rh_confirmed_pos = rh_pos;
        Ok(())
    }

    /// Update USDG/USDC peg mid and enforce divergence / staleness guard.
    pub fn on_usdg_usdc_peg(&mut self, now_ms: u64, peg_mid: f64) -> Result<()> {
        self.check_monotonic_clock(now_ms)?;
        if !peg_mid.is_finite() || peg_mid <= 0.0 {
            self.halt_reason = Some(HaltReason::UsdgUsdcDivergence);
            bail!("invalid USDG/USDC peg price {peg_mid}");
        }
        self.last_peg_mid = peg_mid;
        self.last_peg_ms = now_ms;
        self.has_peg = true;
        let div_bps = (peg_mid - 1.0).abs() * 10_000.0;
        if div_bps > self.cfg.max_usdg_usdc_divergence_bps {
            self.halt_reason = Some(HaltReason::UsdgUsdcDivergence);
        }
        Ok(())
    }

    #[inline]
    fn check_peg_guard(&mut self, now_ms: u64) {
        if self.has_peg {
            let div_bps = (self.last_peg_mid - 1.0).abs() * 10_000.0;
            if div_bps > self.cfg.max_usdg_usdc_divergence_bps
                || now_ms.saturating_sub(self.last_peg_ms) > self.cfg.max_peg_age_ms
            {
                self.halt_reason = Some(HaltReason::UsdgUsdcDivergence);
            }
        } else if self.cfg.require_fresh_usdg_usdc_peg {
            self.halt_reason = Some(HaltReason::UsdgUsdcDivergence);
        }
    }

    #[inline]
    fn pending_hedge_exposure(&self) -> (f64, f64, u32, f64) {
        let mut buy_qty = 0.0;
        let mut sell_qty = 0.0;
        let mut count = 0_u32;
        let mut orphan_notional = 0.0;
        let ref_px = if self.last_arc_mid > 0.0 {
            self.last_arc_mid
        } else {
            self.last_rh_mid
        };

        for h in &self.pending_hedges {
            if h.occupied && h.qty > 1e-12 {
                count += 1;
                let px = if ref_px > 0.0 { ref_px } else { h.ref_maker_px };
                orphan_notional += h.qty * px;
                match h.side {
                    OrderSide::Buy => buy_qty += h.qty,
                    OrderSide::Sell => sell_qty += h.qty,
                }
            }
        }
        if let Some(h) = self.in_flight_hedge
            && h.occupied
            && h.qty > 1e-12
        {
            count += 1;
            let px = if ref_px > 0.0 { ref_px } else { h.ref_maker_px };
            orphan_notional += h.qty * px;
            match h.side {
                OrderSide::Buy => buy_qty += h.qty,
                OrderSide::Sell => sell_qty += h.qty,
            }
        }
        (buy_qty, sell_qty, count, orphan_notional)
    }

    #[inline]
    pub fn arc_worst_case_bounds(&self) -> (f64, f64) {
        let buy_reserved = if self.slots[0].status != SlotStatus::Empty {
            self.slots[0].remaining_qty
        } else {
            0.0
        };
        let sell_reserved = if self.slots[1].status != SlotStatus::Empty {
            self.slots[1].remaining_qty
        } else {
            0.0
        };
        (
            self.arc_confirmed_pos + buy_reserved,
            self.arc_confirmed_pos - sell_reserved,
        )
    }

    #[inline]
    pub fn rh_worst_case_bounds(&self) -> (f64, f64) {
        let (buy_pend, sell_pend, _, _) = self.pending_hedge_exposure();
        (
            self.rh_confirmed_pos + buy_pend,
            self.rh_confirmed_pos - sell_pend,
        )
    }

    #[inline]
    pub fn unrealized_pnl_usd(&self) -> f64 {
        let mark = if self.last_rh_mid > 0.0 {
            self.last_rh_mid
        } else {
            self.last_arc_mid
        };
        if mark <= 0.0 {
            return 0.0;
        }
        let mut pnl = 0.0;
        for lot in &self.open_lots {
            if lot.occupied && lot.qty > 1e-12 {
                pnl += lot.side.sign() * (mark - lot.px) * lot.qty;
            }
        }
        pnl
    }

    #[inline]
    pub fn realized_cash_pnl_usd(&self) -> f64 {
        self.realized_spread_pnl_usd - self.fees_usd - self.fx_drag_usd
    }

    #[inline]
    pub fn total_equity_pnl_usd(&self) -> f64 {
        self.realized_cash_pnl_usd() + self.unrealized_pnl_usd()
    }

    #[inline]
    fn check_risk_breakers(&mut self) {
        let (_, _, _, orphan_notional) = self.pending_hedge_exposure();
        if orphan_notional > self.cfg.max_orphan_notional_usd + 1e-9 {
            self.halt_reason = Some(HaltReason::ExcessOrphanNotional);
            return;
        }
        if self.total_equity_pnl_usd() < -self.cfg.max_daily_loss_usd {
            self.halt_reason = Some(HaltReason::DailyLossLimit);
            return;
        }
        if self.arcus_volume_usd >= self.cfg.min_cost_breaker_volume_usd
            && self.arcus_volume_usd > 0.0
        {
            let cost_per_m = (-self.realized_cash_pnl_usd() / self.arcus_volume_usd) * 1_000_000.0;
            if cost_per_m > self.cfg.max_cost_per_million_usd {
                self.halt_reason = Some(HaltReason::CostPerMillionBreached);
            }
        }
    }

    #[inline]
    fn cancel_active_slots(&mut self, actions: &mut BookStepActions) {
        for (idx, side) in [(0_usize, OrderSide::Buy), (1_usize, OrderSide::Sell)] {
            if self.slots[idx].status == SlotStatus::Active {
                self.slots[idx].status = SlotStatus::CancelPending;
                actions.push_quote(QuoteActionKind::Cancel {
                    side,
                    order_seq: self.slots[idx].order_seq,
                });
            }
        }
    }

    #[inline]
    fn enqueue_pending_hedge(
        &mut self,
        side: OrderSide,
        qty: f64,
        exec_at_ms: u64,
        retry_attempt: u32,
        ref_maker_px: f64,
    ) -> Result<()> {
        for slot in &mut self.pending_hedges {
            if !slot.occupied {
                let hedge_id = self.next_hedge_id;
                self.next_hedge_id = self.next_hedge_id.wrapping_add(1);
                *slot = PendingHedge {
                    occupied: true,
                    hedge_id,
                    side,
                    qty,
                    exec_at_ms,
                    retry_attempt,
                    ref_maker_px,
                };
                return Ok(());
            }
        }
        self.halt_reason = Some(HaltReason::ExcessOrphanNotional);
        bail!("pending hedge queue full (max {MAX_PENDING_HEDGES})");
    }

    #[inline]
    fn apply_fifo_lot(&mut self, side: OrderSide, mut qty: f64, px: f64) {
        // Match against opposite-side open lots first.
        for lot in &mut self.open_lots {
            if qty <= 1e-12 {
                break;
            }
            if lot.occupied && lot.side != side && lot.qty > 1e-12 {
                let matched = qty.min(lot.qty);
                let pnl = lot.side.sign() * (px - lot.px) * matched;
                self.realized_spread_pnl_usd += pnl;
                lot.qty -= matched;
                qty -= matched;
                if lot.qty <= 1e-12 {
                    lot.occupied = false;
                    lot.qty = 0.0;
                }
            }
        }
        // Any remaining residual creates/merges a same-side open lot.
        if qty > 1e-12 {
            for lot in &mut self.open_lots {
                if !lot.occupied {
                    *lot = OpenLot {
                        occupied: true,
                        side,
                        qty,
                        px,
                    };
                    return;
                }
            }
        }
    }

    /// Advance pending hedges only (used on hedge-only book updates without re-quoting maker).
    pub fn on_book_hedges_only(
        &mut self,
        now_ms: u64,
        arc_book: VenueBook,
        rh_book: VenueBook,
    ) -> Result<BookStepActions> {
        self.check_monotonic_clock(now_ms)?;
        if self.start_ms.is_none() {
            self.start_ms = Some(now_ms);
        }
        let mut actions = BookStepActions::default();
        if let Some(end_ms) = self.cfg.declared_end_ms
            && now_ms > end_ms
        {
            return Ok(actions);
        }
        let rh_fresh = rh_book.is_fresh(
            now_ms,
            self.cfg.max_book_age_ms,
            self.cfg.max_venue_clock_skew_ms,
        );
        if arc_book.is_fresh(
            now_ms,
            self.cfg.max_book_age_ms,
            self.cfg.max_venue_clock_skew_ms,
        ) {
            self.last_arc_mid = arc_book.mid();
        }
        if rh_fresh {
            self.last_rh_mid = rh_book.mid();
        }
        self.check_peg_guard(now_ms);
        self.check_risk_breakers();
        if !rh_fresh {
            for h in &mut self.pending_hedges {
                if h.occupied && h.exec_at_ms <= now_ms {
                    h.exec_at_ms = now_ms.saturating_add(self.cfg.hedge_delay_ms);
                    h.retry_attempt = h.retry_attempt.saturating_add(1);
                    self.orphan_retries += 1;
                    actions.stale_hedge_blocks += 1;
                }
            }
        } else if self.in_flight_hedge.is_none() {
            let mut best_idx: Option<usize> = None;
            let mut best_ts = u64::MAX;
            for (idx, h) in self.pending_hedges.iter().enumerate() {
                if h.occupied && h.exec_at_ms <= now_ms && h.exec_at_ms < best_ts {
                    best_ts = h.exec_at_ms;
                    best_idx = Some(idx);
                }
            }
            if let Some(idx) = best_idx {
                let mut h = self.pending_hedges[idx];
                self.pending_hedges[idx].occupied = false;
                let slip = self.cfg.hedge_slippage_bps / 10_000.0;
                let raw_limit = match h.side {
                    OrderSide::Buy => rh_book.ask_px * (1.0 + slip),
                    OrderSide::Sell => rh_book.bid_px * (1.0 - slip),
                };
                let limit_price =
                    round_price_to_tick(raw_limit, self.cfg.tick_size, h.side.opposite());
                let capped_limit = if limit_price > 0.0 {
                    limit_price
                } else {
                    match h.side {
                        OrderSide::Buy => rh_book.ask_px,
                        OrderSide::Sell => rh_book.bid_px,
                    }
                };
                h.occupied = true;
                self.in_flight_hedge = Some(h);
                self.hedges_dispatched += 1;
                actions.hedge_dispatch = Some(HedgeDispatch {
                    hedge_id: h.hedge_id,
                    side: h.side,
                    qty: h.qty,
                    limit_price: capped_limit,
                    retry_attempt: h.retry_attempt,
                    ref_maker_px: h.ref_maker_px,
                });
            }
        }
        Ok(actions)
    }

    /// Process synchronized or latest venue BBO snapshots. Allocation-free hot path.
    pub fn on_book(
        &mut self,
        now_ms: u64,
        arc_book: VenueBook,
        rh_book: VenueBook,
    ) -> Result<BookStepActions> {
        self.check_monotonic_clock(now_ms)?;
        if self.start_ms.is_none() {
            self.start_ms = Some(now_ms);
        }
        let mut actions = BookStepActions::default();

        if let Some(end_ms) = self.cfg.declared_end_ms
            && now_ms > end_ms
        {
            return Ok(actions);
        }

        let arc_fresh = arc_book.is_fresh(
            now_ms,
            self.cfg.max_book_age_ms,
            self.cfg.max_venue_clock_skew_ms,
        );
        let rh_fresh = rh_book.is_fresh(
            now_ms,
            self.cfg.max_book_age_ms,
            self.cfg.max_venue_clock_skew_ms,
        );

        if arc_fresh {
            self.last_arc_mid = arc_book.mid();
        }
        if rh_fresh {
            self.last_rh_mid = rh_book.mid();
        }

        self.check_peg_guard(now_ms);
        self.check_risk_breakers();

        // Dispatch due hedge whenever RH book is fresh, even if Arcus book is stale or halted.
        // If RH book is stale when a hedge is due, block & re-queue with `+hedge_delay_ms` (never 0 ms).
        if !rh_fresh {
            for h in &mut self.pending_hedges {
                if h.occupied && h.exec_at_ms <= now_ms {
                    h.exec_at_ms = now_ms.saturating_add(self.cfg.hedge_delay_ms);
                    h.retry_attempt = h.retry_attempt.saturating_add(1);
                    self.orphan_retries += 1;
                    actions.stale_hedge_blocks += 1;
                }
            }
        } else if self.in_flight_hedge.is_none() {
            let mut best_idx: Option<usize> = None;
            let mut best_ts = u64::MAX;
            for (idx, h) in self.pending_hedges.iter().enumerate() {
                if h.occupied && h.exec_at_ms <= now_ms && h.exec_at_ms < best_ts {
                    best_ts = h.exec_at_ms;
                    best_idx = Some(idx);
                }
            }
            if let Some(idx) = best_idx {
                let mut h = self.pending_hedges[idx];
                self.pending_hedges[idx].occupied = false;
                let slip = self.cfg.hedge_slippage_bps / 10_000.0;
                let raw_limit = match h.side {
                    OrderSide::Buy => rh_book.ask_px * (1.0 + slip),
                    OrderSide::Sell => rh_book.bid_px * (1.0 - slip),
                };
                let limit_price =
                    round_price_to_tick(raw_limit, self.cfg.tick_size, h.side.opposite());
                let capped_limit = if limit_price > 0.0 {
                    limit_price
                } else {
                    match h.side {
                        OrderSide::Buy => rh_book.ask_px,
                        OrderSide::Sell => rh_book.bid_px,
                    }
                };
                h.occupied = true;
                self.in_flight_hedge = Some(h);
                self.hedges_dispatched += 1;
                actions.hedge_dispatch = Some(HedgeDispatch {
                    hedge_id: h.hedge_id,
                    side: h.side,
                    qty: h.qty,
                    limit_price: capped_limit,
                    retry_attempt: h.retry_attempt,
                    ref_maker_px: h.ref_maker_px,
                });
            }
        }

        if !arc_fresh || !rh_fresh {
            self.cancel_active_slots(&mut actions);
            actions.withdrew_stale = true;
            return Ok(actions);
        }

        // Update causal microprice, lead impulse, and causal EWMA fair basis.
        let arc_micro = arc_book.microprice(self.cfg.microprice_alpha_weight);
        let rh_micro = rh_book.microprice(self.cfg.microprice_alpha_weight);
        let dt_ms = if self.last_book_update_ms > 0 {
            now_ms.saturating_sub(self.last_book_update_ms) as f64
        } else {
            0.0
        };
        self.last_book_update_ms = now_ms;

        if !self.basis_initialized {
            self.basis_ewma_bps = (arc_micro / rh_micro - 1.0) * 10_000.0;
            self.rh_slow_ewma_px = rh_micro;
            self.last_rh_micro_px = rh_micro;
            self.basis_initialized = true;
        } else {
            let alpha_basis = if dt_ms > 0.0 {
                1.0 - (-dt_ms / self.cfg.basis_ewma_tau_ms).exp()
            } else {
                0.0
            };
            let inst_basis_bps = (arc_micro / rh_micro - 1.0) * 10_000.0;
            self.basis_ewma_bps += alpha_basis * (inst_basis_bps - self.basis_ewma_bps);

            let alpha_lead = if dt_ms > 0.0 {
                1.0 - (-dt_ms / self.cfg.lead_ewma_tau_ms).exp()
            } else {
                0.0
            };
            self.rh_slow_ewma_px += alpha_lead * (rh_micro - self.rh_slow_ewma_px);
            self.last_rh_micro_px = rh_micro;
        }

        let in_warmup = self
            .start_ms
            .is_some_and(|s| now_ms.saturating_sub(s) < self.cfg.warmup_ms);
        if self.halt_reason.is_some() || in_warmup {
            self.cancel_active_slots(&mut actions);
            return Ok(actions);
        }

        let in_drain = match (self.cfg.declared_end_ms, self.cfg.terminal_drain_ms) {
            (Some(end_ms), Some(drain_ms)) => now_ms >= end_ms.saturating_sub(drain_ms),
            _ => false,
        };

        // Compute anchored fair value & reservation shift.
        let rh_impulse_bps = if self.rh_slow_ewma_px > 0.0 {
            ((rh_micro / self.rh_slow_ewma_px) - 1.0) * 10_000.0
        } else {
            0.0
        };
        let lead_shift_bps = (self.cfg.lead_beta * rh_impulse_bps)
            .clamp(-self.cfg.max_lead_shift_bps, self.cfg.max_lead_shift_bps);

        let inv_notional = self.arc_confirmed_pos * arc_book.mid();
        let inv_ratio = (inv_notional / self.cfg.max_inventory_usd).clamp(-1.0, 1.0);
        let skew_bps = inv_ratio * self.cfg.inventory_skew_bps;

        // Total per-round-trip cost hurdle in bps: Arcus maker + RH taker + FX drag + entry_edge_bps.
        let total_hurdle_bps = self.cfg.arcus_maker_fee_bps
            + self.cfg.rh_taker_fee_bps
            + self.cfg.fx_drag_bps
            + self.cfg.entry_edge_bps;

        let anchor_mult = 1.0 + (self.basis_ewma_bps + lead_shift_bps - skew_bps) / 10_000.0;
        // Buy on Arcus -> hedge by selling on Lighter-RH (`rh_book.bid_px`).
        let raw_bid_px = rh_book.bid_px * anchor_mult * (1.0 - total_hurdle_bps / 10_000.0);
        // Sell on Arcus -> hedge by buying on Lighter-RH (`rh_book.ask_px`).
        let raw_ask_px = rh_book.ask_px * anchor_mult * (1.0 + total_hurdle_bps / 10_000.0);

        let improve_delta = f64::from(self.cfg.max_improve_ticks) * self.cfg.tick_size;
        // Strict PostOnly bounds on Arcus: never cross `arc_book.ask_px` / `arc_book.bid_px`.
        let max_post_only_bid =
            (arc_book.ask_px - self.cfg.tick_size).min(arc_book.bid_px + improve_delta);
        let min_post_only_ask =
            (arc_book.bid_px + self.cfg.tick_size).max(arc_book.ask_px - improve_delta);

        let target_bid_px = round_price_to_tick(
            raw_bid_px.min(max_post_only_bid),
            self.cfg.tick_size,
            OrderSide::Buy,
        );
        let target_ask_px = round_price_to_tick(
            raw_ask_px.max(min_post_only_ask),
            self.cfg.tick_size,
            OrderSide::Sell,
        );

        let (arc_worst_long, arc_worst_short) = self.arc_worst_case_bounds();
        let (rh_worst_long, rh_worst_short) = self.rh_worst_case_bounds();
        let max_base_pos = self.cfg.max_inventory_usd / arc_book.mid();

        // Inventory-reducing sizing: when open Arcus inventory is >= min_notional_usd,
        // the reducing side sizes to `min(|pos_usd|, clip_usd)` (always, and exclusively when `in_drain`).
        let pos_usd = self.arc_confirmed_pos.abs() * arc_book.mid();

        for (idx, side, target_px) in [
            (0_usize, OrderSide::Buy, target_bid_px),
            (1_usize, OrderSide::Sell, target_ask_px),
        ] {
            let other_px = if side == OrderSide::Buy {
                target_ask_px
            } else {
                target_bid_px
            };
            let non_crossing_pair = target_px > 0.0
                && (side == OrderSide::Buy && (other_px <= 0.0 || target_px < other_px)
                    || side == OrderSide::Sell && (other_px <= 0.0 || target_px > other_px));

            // Worst-case exposure room on BOTH Arcus AND Lighter-RH (hedge goes opposite side!).
            let room_base = match side {
                OrderSide::Buy => {
                    let arc_room = max_base_pos - arc_worst_long;
                    let rh_room = max_base_pos + rh_worst_short;
                    arc_room.min(rh_room)
                }
                OrderSide::Sell => {
                    let arc_room = max_base_pos + arc_worst_short;
                    let rh_room = max_base_pos - rh_worst_long;
                    arc_room.min(rh_room)
                }
            };
            let is_reducing = match side {
                OrderSide::Buy => {
                    self.arc_confirmed_pos < -1e-9 && pos_usd >= self.cfg.min_notional_usd
                }
                OrderSide::Sell => {
                    self.arc_confirmed_pos > 1e-9 && pos_usd >= self.cfg.min_notional_usd
                }
            };
            let desired_qty = if non_crossing_pair && room_base > 0.0 {
                if is_reducing {
                    let target_usd = pos_usd.min(self.cfg.clip_usd);
                    let q = round_qty_down_to_lot(
                        (target_usd / target_px).min(self.arc_confirmed_pos.abs()),
                        self.cfg.lot_size,
                    );
                    if q >= self.cfg.min_order_size && q * target_px >= self.cfg.min_notional_usd {
                        q
                    } else {
                        0.0
                    }
                } else if in_drain {
                    0.0
                } else {
                    let raw_qty = (self.cfg.clip_usd / target_px).min(room_base);
                    let q = round_qty_down_to_lot(raw_qty, self.cfg.lot_size);
                    if q >= self.cfg.min_order_size && q * target_px >= self.cfg.min_notional_usd {
                        q
                    } else {
                        0.0
                    }
                }
            } else {
                0.0
            };

            match self.slots[idx].status {
                SlotStatus::CancelPending => {
                    // Wait for cancel ack or passive fill; never overwrite a cancel-in-flight slot.
                }
                SlotStatus::Active => {
                    let drift_ticks =
                        ((self.slots[idx].price - target_px).abs() / self.cfg.tick_size).round();
                    let size_mismatch_in_drain =
                        in_drain && (self.slots[idx].remaining_qty - desired_qty).abs() > 1e-9;
                    let invalid_now = desired_qty <= 0.0
                        || size_mismatch_in_drain
                        || (side == OrderSide::Buy && self.slots[idx].price >= arc_book.ask_px)
                        || (side == OrderSide::Sell && self.slots[idx].price <= arc_book.bid_px);
                    if invalid_now || drift_ticks > f64::from(self.cfg.requote_threshold_ticks) {
                        self.slots[idx].status = SlotStatus::CancelPending;
                        actions.push_quote(QuoteActionKind::Cancel {
                            side,
                            order_seq: self.slots[idx].order_seq,
                        });
                    }
                }
                SlotStatus::Empty => {
                    if desired_qty > 0.0 {
                        let order_seq = self.next_order_seq;
                        self.next_order_seq = self.next_order_seq.wrapping_add(1);
                        self.slots[idx] = MakerSlot {
                            status: SlotStatus::Active,
                            order_seq,
                            price: target_px,
                            remaining_qty: desired_qty,
                        };
                        actions.push_quote(QuoteActionKind::PlacePostOnly {
                            side,
                            price: target_px,
                            qty: desired_qty,
                            order_seq,
                        });
                    }
                }
            }
        }

        Ok(actions)
    }

    /// Record a passive Arcus maker fill.
    ///
    /// Hard invariant: `on_passive_fill` takes strict precedence over any `CancelPending`
    /// state on that slot, preventing lost hedges or double-released exposure when a fill
    /// races with a cancel.
    pub fn on_passive_fill(
        &mut self,
        now_ms: u64,
        side: OrderSide,
        order_seq: Option<u64>,
        fill_qty: f64,
        fill_px: f64,
    ) -> Result<()> {
        self.check_monotonic_clock(now_ms)?;
        ensure!(
            fill_qty.is_finite() && fill_qty > 0.0 && fill_px.is_finite() && fill_px > 0.0,
            "invalid passive fill qty/price"
        );
        let idx = side.idx();
        let slot = &mut self.slots[idx];
        if let Some(seq) = order_seq
            && slot.status != SlotStatus::Empty
            && slot.order_seq == seq
        {
            slot.remaining_qty = (slot.remaining_qty - fill_qty).max(0.0);
            if slot.remaining_qty <= 1e-12 {
                *slot = MakerSlot::default();
            }
        } else if order_seq.is_none() && slot.status != SlotStatus::Empty {
            slot.remaining_qty = (slot.remaining_qty - fill_qty).max(0.0);
            if slot.remaining_qty <= 1e-12 {
                *slot = MakerSlot::default();
            }
        }

        self.arc_confirmed_pos += side.sign() * fill_qty;
        let notional = fill_qty * fill_px;
        self.arcus_volume_usd += notional;
        self.fees_usd += notional * (self.cfg.arcus_maker_fee_bps / 10_000.0);
        self.passive_fills += 1;
        self.apply_fifo_lot(side, fill_qty, fill_px);

        let exec_at_ms = now_ms.saturating_add(self.cfg.hedge_delay_ms);
        self.enqueue_pending_hedge(side.opposite(), fill_qty, exec_at_ms, 0, fill_px)?;
        self.check_risk_breakers();
        Ok(())
    }

    /// Acknowledge a cancel for `side` / `order_seq`.
    /// Releases ONLY the remaining unfilled reservation; if `on_passive_fill` already
    /// filled the slot, this is a safe no-op.
    pub fn on_cancel_ack(
        &mut self,
        now_ms: u64,
        side: OrderSide,
        order_seq: Option<u64>,
    ) -> Result<()> {
        self.check_monotonic_clock(now_ms)?;
        let idx = side.idx();
        let matches = match order_seq {
            Some(seq) => {
                self.slots[idx].status != SlotStatus::Empty && self.slots[idx].order_seq == seq
            }
            None => self.slots[idx].status != SlotStatus::Empty,
        };
        if matches {
            self.slots[idx] = MakerSlot::default();
        }
        Ok(())
    }

    /// Complete an in-flight Lighter-RH IOC taker hedge.
    pub fn on_hedge_complete(
        &mut self,
        now_ms: u64,
        hedge_id: u64,
        filled_qty: f64,
        fill_px: f64,
        failed: bool,
    ) -> Result<()> {
        self.check_monotonic_clock(now_ms)?;
        ensure!(
            filled_qty.is_finite() && filled_qty >= 0.0 && fill_px.is_finite() && fill_px >= 0.0,
            "invalid hedge completion qty/price"
        );
        let Some(h) = self.in_flight_hedge.take() else {
            bail!("on_hedge_complete called with no in_flight_hedge");
        };
        ensure!(
            h.hedge_id == hedge_id,
            "hedge_id mismatch: expected {}, got {hedge_id}",
            h.hedge_id
        );

        let actual_filled = filled_qty.min(h.qty);
        if actual_filled > 1e-12 {
            ensure!(fill_px > 0.0, "positive hedge fill requires fill_px > 0");
            self.rh_confirmed_pos += h.side.sign() * actual_filled;
            let notional = actual_filled * fill_px;
            self.rh_volume_usd += notional;
            self.fees_usd += notional * (self.cfg.rh_taker_fee_bps / 10_000.0);
            self.fx_drag_usd += notional * (self.cfg.fx_drag_bps / 10_000.0);
            self.apply_fifo_lot(h.side, actual_filled, fill_px);
            self.hedges_completed += 1;
            self.consecutive_hedge_failures = 0;
        } else if failed {
            self.consecutive_hedge_failures = self.consecutive_hedge_failures.saturating_add(1);
            if self.consecutive_hedge_failures >= self.cfg.max_consecutive_hedge_failures {
                self.halt_reason = Some(HaltReason::ConsecutiveHedgeFailures);
            }
        }

        let rem_qty = round_qty_down_to_lot(h.qty - actual_filled, self.cfg.lot_size);
        if rem_qty >= self.cfg.min_order_size {
            let next_retry = h.retry_attempt.saturating_add(1);
            if next_retry > self.cfg.max_orphan_retries {
                self.halt_reason = Some(HaltReason::OrphanRetriesExhausted);
            } else {
                self.orphan_retries += 1;
                let retry_at = now_ms.saturating_add(self.cfg.hedge_delay_ms);
                self.enqueue_pending_hedge(h.side, rem_qty, retry_at, next_retry, h.ref_maker_px)?;
            }
        }

        self.check_risk_breakers();
        Ok(())
    }

    pub fn metrics(&self) -> EngineMetricsSnapshot {
        let (_, _, pending_hedge_count, pending_hedge_notional_usd) = self.pending_hedge_exposure();
        let (arc_worst_long, arc_worst_short) = self.arc_worst_case_bounds();
        let (rh_worst_long, rh_worst_short) = self.rh_worst_case_bounds();
        let realized_cash = self.realized_cash_pnl_usd();
        let combined_vol = self.arcus_volume_usd + self.rh_volume_usd;
        EngineMetricsSnapshot {
            last_callback_ms: self.last_callback_ms,
            halted: self.halt_reason.is_some(),
            halt_reason: self.halt_reason,
            arc_confirmed_pos: self.arc_confirmed_pos,
            rh_confirmed_pos: self.rh_confirmed_pos,
            net_unhedged_pos: self.arc_confirmed_pos + self.rh_confirmed_pos,
            arc_worst_long,
            arc_worst_short,
            rh_worst_long,
            rh_worst_short,
            pending_hedge_count,
            pending_hedge_notional_usd,
            arcus_volume_usd: self.arcus_volume_usd,
            rh_volume_usd: self.rh_volume_usd,
            passive_fills: self.passive_fills,
            hedges_dispatched: self.hedges_dispatched,
            hedges_completed: self.hedges_completed,
            orphan_retries: self.orphan_retries,
            consecutive_hedge_failures: self.consecutive_hedge_failures,
            realized_spread_pnl_usd: self.realized_spread_pnl_usd,
            fees_usd: self.fees_usd,
            fx_drag_usd: self.fx_drag_usd,
            realized_cash_pnl_usd: realized_cash,
            unrealized_pnl_usd: self.unrealized_pnl_usd(),
            cost_per_m_arcus_usd: if self.arcus_volume_usd > 0.0 {
                (-realized_cash / self.arcus_volume_usd) * 1_000_000.0
            } else {
                0.0
            },
            cost_per_m_combined_usd: if combined_vol > 0.0 {
                (-realized_cash / combined_vol) * 1_000_000.0
            } else {
                0.0
            },
            basis_ewma_bps: self.basis_ewma_bps,
        }
    }
}

/// JSONL event schema for deterministic offline P0 replay (`--replay`).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TurnoverReplayEvent {
    Bbo {
        now_ms: u64,
        arc_bid: f64,
        arc_ask: f64,
        arc_bid_sz: f64,
        arc_ask_sz: f64,
        #[serde(default)]
        arc_recv_ms: Option<u64>,
        rh_bid: f64,
        rh_ask: f64,
        rh_bid_sz: f64,
        rh_ask_sz: f64,
        #[serde(default)]
        rh_recv_ms: Option<u64>,
    },
    ArcusBook {
        now_ms: u64,
        bid: f64,
        ask: f64,
        bid_sz: f64,
        ask_sz: f64,
        #[serde(default)]
        recv_ms: Option<u64>,
    },
    HedgeBook {
        now_ms: u64,
        bid: f64,
        ask: f64,
        bid_sz: f64,
        ask_sz: f64,
        #[serde(default)]
        recv_ms: Option<u64>,
    },
    ArcusTrade {
        now_ms: u64,
        price: f64,
        size: f64,
        #[serde(default)]
        is_buy_aggressor: Option<bool>,
        #[serde(default)]
        side: Option<String>,
    },
    UsdgUsdc {
        now_ms: u64,
        mid: f64,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct TurnoverReplayReport {
    pub symbol: String,
    pub arcus_symbol: String,
    pub events_processed: u64,
    pub ignored_post_cutoff_events: u64,
    pub hedge_delay_ms: u64,
    pub strict_through_prob: f64,
    pub halted: bool,
    pub halt_reason: Option<HaltReason>,
    pub orders_placed: u64,
    pub cancels_sent: u64,
    pub cancel_in_flight_fills: u64,
    pub passive_fills: u64,
    pub hedges_dispatched: u64,
    pub hedges_completed: u64,
    pub orphan_events: u64,
    pub orphan_retries: u64,
    pub depth_clipped_hedges: u64,
    pub stale_hedge_blocks: u64,
    pub terminal_pending_hedges: u32,
    pub arcus_volume_usd: f64,
    pub rh_volume_usd: f64,
    pub combined_volume_usd: f64,
    pub arcus_spread_capture_usd: f64,
    pub arcus_adverse_usd: f64,
    pub rh_hedge_slip_usd: f64,
    pub residual_drain_usd: f64,
    pub realized_only_pnl_usd: f64,
    pub mtm_incl_residual_pnl_usd: f64,
    pub conservation_error_usd: f64,
    pub terminal_residual_notional_usd: f64,
    pub gross_matched_notional_usd: f64,
    pub cost_per_m_arcus_usd: f64,
    pub cost_per_m_combined_usd: f64,
}

#[derive(Debug, Clone, Copy)]
struct SimOrder {
    active: bool,
    side: OrderSide,
    order_seq: u64,
    price: f64,
    remaining_qty: f64,
    queue_ahead: f64,
    active_at_ms: u64,
    cancel_at_ms: Option<u64>,
}

impl Default for SimOrder {
    fn default() -> Self {
        Self {
            active: false,
            side: OrderSide::Buy,
            order_seq: 0,
            price: 0.0,
            remaining_qty: 0.0,
            queue_ahead: 0.0,
            active_at_ms: 0,
            cancel_at_ms: None,
        }
    }
}

/// Deterministic split-mix hash in `[0, 1)` for `strict_through_prob` toxic fill simulation.
#[inline]
fn deterministic_unit_u64(seed: u64) -> f64 {
    let mut x = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^= x >> 31;
    ((x >> 11) as f64) / ((1_u64 << 53) as f64)
}

/// Offline deterministic P0 replay preserving:
/// - pre-install `hedge_delay_ms` (`> 0`),
/// - single-use L1 depth budget on Lighter-RH (`rh_rem_bid_sz`, `rh_rem_ask_sz`),
/// - `submit_rtt_ms` / `cancel_rtt_ms` wire latency,
/// - `on_passive_fill` precedence over cancel-in-flight,
/// - strict `declared_end_ms` cutoff and explicit residual tracking.
pub fn run_deterministic_replay(
    cfg: ArcusRhTurnoverConfig,
    events: &[TurnoverReplayEvent],
    strict_through_prob: f64,
) -> Result<TurnoverReplayReport> {
    ensure!(
        strict_through_prob.is_finite() && (0.0..=1.0).contains(&strict_through_prob),
        "strict_through_prob must be in [0, 1]"
    );

    #[derive(Clone, Copy)]
    struct OpenMakerFill {
        sgn: f64,
        rem_qty: f64,
        a_mid_f: f64,
    }

    struct ReplayCtx {
        cfg: ArcusRhTurnoverConfig,
        engine: ArcusRhTurnoverEngine,
        sim_orders: [SimOrder; 2],
        last_arc_book: Option<VenueBook>,
        last_rh_book: Option<VenueBook>,
        rh_rem_bid_sz: f64,
        rh_rem_ask_sz: f64,
        orders_placed: u64,
        cancels_sent: u64,
        cancel_in_flight_fills: u64,
        orphan_events: u64,
        depth_clipped_hedges: u64,
        stale_hedge_blocks: u64,
        arcus_cash_usd: f64,
        rh_cash_usd: f64,
        arcus_spread_capture_usd: f64,
        arcus_adverse_usd: f64,
        rh_hedge_slip_usd: f64,
        open_fills: Vec<OpenMakerFill>,
    }

    impl ReplayCtx {
        fn process_step(
            &mut self,
            now_ms: u64,
            arc_book: VenueBook,
            rh_book: VenueBook,
            hedges_only: bool,
        ) -> Result<()> {
            let step = if hedges_only {
                self.engine.on_book_hedges_only(now_ms, arc_book, rh_book)?
            } else {
                self.engine.on_book(now_ms, arc_book, rh_book)?
            };
            self.stale_hedge_blocks += u64::from(step.stale_hedge_blocks);
            for act in step.quote_slice().iter().flatten() {
                match *act {
                    QuoteActionKind::PlacePostOnly {
                        side,
                        price,
                        qty,
                        order_seq,
                    } => {
                        self.orders_placed += 1;
                        let queue_l1 = match side {
                            OrderSide::Buy => {
                                if (price - arc_book.bid_px).abs() <= 1e-9 {
                                    arc_book.bid_sz * self.cfg.queue_ahead_fraction
                                } else {
                                    0.0
                                }
                            }
                            OrderSide::Sell => {
                                if (price - arc_book.ask_px).abs() <= 1e-9 {
                                    arc_book.ask_sz * self.cfg.queue_ahead_fraction
                                } else {
                                    0.0
                                }
                            }
                        };
                        self.sim_orders[side.idx()] = SimOrder {
                            active: true,
                            side,
                            order_seq,
                            price,
                            remaining_qty: qty,
                            queue_ahead: queue_l1,
                            active_at_ms: now_ms.saturating_add(self.cfg.submit_rtt_ms),
                            cancel_at_ms: None,
                        };
                    }
                    QuoteActionKind::Cancel { side, order_seq } => {
                        let idx = side.idx();
                        if self.sim_orders[idx].active
                            && self.sim_orders[idx].order_seq == order_seq
                        {
                            self.cancels_sent += 1;
                            self.sim_orders[idx].cancel_at_ms =
                                Some(now_ms.saturating_add(self.cfg.cancel_rtt_ms));
                        }
                    }
                }
            }

            if let Some(dispatch) = step.hedge_dispatch {
                let (avail_sz, exec_px) = match dispatch.side {
                    OrderSide::Buy => (&mut self.rh_rem_ask_sz, rh_book.ask_px),
                    OrderSide::Sell => (&mut self.rh_rem_bid_sz, rh_book.bid_px),
                };
                let marketable = match dispatch.side {
                    OrderSide::Buy => rh_book.ask_px <= dispatch.limit_price + 1e-12,
                    OrderSide::Sell => rh_book.bid_px >= dispatch.limit_price - 1e-12,
                };
                let fillable = if marketable && *avail_sz > 0.0 {
                    round_qty_down_to_lot(dispatch.qty.min(*avail_sz), self.cfg.lot_size)
                } else {
                    0.0
                };
                if fillable < dispatch.qty {
                    self.depth_clipped_hedges += 1;
                    if dispatch.retry_attempt == 0 {
                        self.orphan_events += 1;
                    }
                }
                *avail_sz = (*avail_sz - fillable).max(0.0);
                if fillable > 1e-12 {
                    self.rh_cash_usd -= dispatch.side.sign() * fillable * exec_px;
                    let a_mid_h = arc_book.mid();
                    let mut rem = fillable;
                    let hedge_maker_sgn = dispatch.side.opposite().sign();
                    for f in self.open_fills.iter_mut() {
                        if rem <= 1e-12 {
                            break;
                        }
                        if f.rem_qty > 1e-12 && (f.sgn - hedge_maker_sgn).abs() < 1e-9 {
                            let m = rem.min(f.rem_qty);
                            self.arcus_adverse_usd += f.sgn * m * (a_mid_h - f.a_mid_f);
                            self.rh_hedge_slip_usd += f.sgn * m * (exec_px - a_mid_h);
                            f.rem_qty -= m;
                            rem -= m;
                        }
                    }
                }
                let failed = fillable <= 1e-12;
                self.engine.on_hedge_complete(
                    now_ms,
                    dispatch.hedge_id,
                    fillable,
                    if fillable > 0.0 { exec_px } else { 0.0 },
                    failed,
                )?;
            }
            Ok(())
        }

        fn check_strict_through(
            &mut self,
            ev_idx: usize,
            now_ms: u64,
            arc_book: VenueBook,
            strict_through_prob: f64,
        ) -> Result<()> {
            if strict_through_prob <= 0.0 {
                return Ok(());
            }
            for idx in [0_usize, 1_usize] {
                let ord = self.sim_orders[idx];
                if !ord.active || now_ms < ord.active_at_ms {
                    continue;
                }
                let touched_through = match ord.side {
                    OrderSide::Buy => arc_book.ask_px < ord.price - 1e-12,
                    OrderSide::Sell => arc_book.bid_px > ord.price + 1e-12,
                };
                if touched_through {
                    let u = deterministic_unit_u64(
                        (ev_idx as u64)
                            .wrapping_mul(131)
                            .wrapping_add(ord.order_seq),
                    );
                    if u < strict_through_prob {
                        let fill_qty = ord.remaining_qty;
                        let fill_px = ord.price;
                        let seq = ord.order_seq;
                        if ord.cancel_at_ms.is_some() {
                            self.cancel_in_flight_fills += 1;
                        }
                        self.sim_orders[idx] = SimOrder::default();
                        self.arcus_cash_usd -= ord.side.sign() * fill_qty * fill_px;
                        let a_mid_f = arc_book.mid();
                        self.arcus_spread_capture_usd +=
                            ord.side.sign() * fill_qty * (a_mid_f - fill_px);
                        self.open_fills.push(OpenMakerFill {
                            sgn: ord.side.sign(),
                            rem_qty: fill_qty,
                            a_mid_f,
                        });
                        self.engine.on_passive_fill(
                            now_ms,
                            ord.side,
                            Some(seq),
                            fill_qty,
                            fill_px,
                        )?;
                    }
                }
            }
            Ok(())
        }
    }

    let engine = ArcusRhTurnoverEngine::new(cfg.clone())?;
    let mut ctx = ReplayCtx {
        cfg,
        engine,
        sim_orders: [SimOrder::default(); 2],
        last_arc_book: None,
        last_rh_book: None,
        rh_rem_bid_sz: 0.0,
        rh_rem_ask_sz: 0.0,
        orders_placed: 0,
        cancels_sent: 0,
        cancel_in_flight_fills: 0,
        orphan_events: 0,
        depth_clipped_hedges: 0,
        stale_hedge_blocks: 0,
        arcus_cash_usd: 0.0,
        rh_cash_usd: 0.0,
        arcus_spread_capture_usd: 0.0,
        arcus_adverse_usd: 0.0,
        rh_hedge_slip_usd: 0.0,
        open_fills: Vec::new(),
    };

    let mut events_processed = 0_u64;
    let mut ignored_post_cutoff_events = 0_u64;

    for (ev_idx, ev) in events.iter().enumerate() {
        let now_ms = match ev {
            TurnoverReplayEvent::Bbo { now_ms, .. }
            | TurnoverReplayEvent::ArcusBook { now_ms, .. }
            | TurnoverReplayEvent::HedgeBook { now_ms, .. }
            | TurnoverReplayEvent::ArcusTrade { now_ms, .. }
            | TurnoverReplayEvent::UsdgUsdc { now_ms, .. } => *now_ms,
        };
        if let Some(end_ms) = ctx.cfg.declared_end_ms
            && now_ms > end_ms
        {
            ignored_post_cutoff_events += 1;
            continue;
        }
        events_processed += 1;

        for (idx, side) in [(0_usize, OrderSide::Buy), (1_usize, OrderSide::Sell)] {
            if ctx.sim_orders[idx].active
                && let Some(cancel_at) = ctx.sim_orders[idx].cancel_at_ms
                && now_ms >= cancel_at
            {
                let seq = ctx.sim_orders[idx].order_seq;
                ctx.sim_orders[idx] = SimOrder::default();
                ctx.engine.on_cancel_ack(now_ms, side, Some(seq))?;
            }
        }

        match ev {
            TurnoverReplayEvent::UsdgUsdc { now_ms, mid } => {
                ctx.engine.on_usdg_usdc_peg(*now_ms, *mid)?;
            }
            TurnoverReplayEvent::ArcusBook {
                now_ms,
                bid,
                ask,
                bid_sz,
                ask_sz,
                recv_ms,
            } => {
                // Pre-install hedge processing against existing RH book before installing new Arcus book.
                if let (Some(prev_arc), Some(prev_rh)) = (ctx.last_arc_book, ctx.last_rh_book) {
                    ctx.process_step(*now_ms, prev_arc, prev_rh, true)?;
                }
                let arc_book = VenueBook {
                    bid_px: *bid,
                    ask_px: *ask,
                    bid_sz: *bid_sz,
                    ask_sz: *ask_sz,
                    source_ms: recv_ms.unwrap_or(*now_ms),
                    receipt_ms: recv_ms.unwrap_or(*now_ms),
                };
                ctx.last_arc_book = Some(arc_book);
                ctx.check_strict_through(ev_idx, *now_ms, arc_book, strict_through_prob)?;
                if let Some(rh_book) = ctx.last_rh_book {
                    ctx.process_step(*now_ms, arc_book, rh_book, false)?;
                }
            }
            TurnoverReplayEvent::HedgeBook {
                now_ms,
                bid,
                ask,
                bid_sz,
                ask_sz,
                recv_ms,
            } => {
                // Pre-install hedge processing BEFORE installing the new HedgeBook touch (P0 causal gate).
                if let (Some(prev_arc), Some(prev_rh)) = (ctx.last_arc_book, ctx.last_rh_book) {
                    ctx.process_step(*now_ms, prev_arc, prev_rh, true)?;
                }
                let rh_book = VenueBook {
                    bid_px: *bid,
                    ask_px: *ask,
                    bid_sz: *bid_sz,
                    ask_sz: *ask_sz,
                    source_ms: recv_ms.unwrap_or(*now_ms),
                    receipt_ms: recv_ms.unwrap_or(*now_ms),
                };
                ctx.last_rh_book = Some(rh_book);
                ctx.rh_rem_bid_sz = (*bid_sz * ctx.cfg.max_l1_depth_fraction).max(0.0);
                ctx.rh_rem_ask_sz = (*ask_sz * ctx.cfg.max_l1_depth_fraction).max(0.0);
                if let Some(arc_book) = ctx.last_arc_book {
                    ctx.process_step(*now_ms, arc_book, rh_book, false)?;
                }
            }
            TurnoverReplayEvent::Bbo {
                now_ms,
                arc_bid,
                arc_ask,
                arc_bid_sz,
                arc_ask_sz,
                arc_recv_ms,
                rh_bid,
                rh_ask,
                rh_bid_sz,
                rh_ask_sz,
                rh_recv_ms,
            } => {
                let arc_book = VenueBook {
                    bid_px: *arc_bid,
                    ask_px: *arc_ask,
                    bid_sz: *arc_bid_sz,
                    ask_sz: *arc_ask_sz,
                    source_ms: arc_recv_ms.unwrap_or(*now_ms),
                    receipt_ms: arc_recv_ms.unwrap_or(*now_ms),
                };
                let rh_book = VenueBook {
                    bid_px: *rh_bid,
                    ask_px: *rh_ask,
                    bid_sz: *rh_bid_sz,
                    ask_sz: *rh_ask_sz,
                    source_ms: rh_recv_ms.unwrap_or(*now_ms),
                    receipt_ms: rh_recv_ms.unwrap_or(*now_ms),
                };
                ctx.last_arc_book = Some(arc_book);
                ctx.last_rh_book = Some(rh_book);
                ctx.rh_rem_bid_sz = (*rh_bid_sz * ctx.cfg.max_l1_depth_fraction).max(0.0);
                ctx.rh_rem_ask_sz = (*rh_ask_sz * ctx.cfg.max_l1_depth_fraction).max(0.0);
                ctx.check_strict_through(ev_idx, *now_ms, arc_book, strict_through_prob)?;
                ctx.process_step(*now_ms, arc_book, rh_book, false)?;
            }
            TurnoverReplayEvent::ArcusTrade {
                now_ms,
                price,
                size,
                is_buy_aggressor,
                side,
            } => {
                let is_buy = match (is_buy_aggressor, side.as_deref()) {
                    (Some(b), _) => *b,
                    (None, Some(s)) => s.eq_ignore_ascii_case("buy"),
                    (None, None) => bail!("arcus_trade requires is_buy_aggressor or side"),
                };
                if let (Some(arc_b), Some(rh_b)) = (ctx.last_arc_book, ctx.last_rh_book) {
                    ctx.process_step(*now_ms, arc_b, rh_b, true)?;
                }
                let hit_side = if is_buy {
                    OrderSide::Sell
                } else {
                    OrderSide::Buy
                };
                let idx = hit_side.idx();
                let mut ord = ctx.sim_orders[idx];
                if ord.active && *now_ms >= ord.active_at_ms && *size > 0.0 {
                    let crosses_or_touches = match hit_side {
                        OrderSide::Buy => *price <= ord.price + 1e-12,
                        OrderSide::Sell => *price >= ord.price - 1e-12,
                    };
                    if crosses_or_touches {
                        let strict_through = match hit_side {
                            OrderSide::Buy => *price < ord.price - 1e-12,
                            OrderSide::Sell => *price > ord.price + 1e-12,
                        };
                        let mut avail_print = *size;
                        if !strict_through && ord.queue_ahead > 0.0 {
                            let consumed_q = avail_print.min(ord.queue_ahead);
                            ord.queue_ahead -= consumed_q;
                            avail_print -= consumed_q;
                        }
                        let fill_qty = round_qty_down_to_lot(
                            avail_print.min(ord.remaining_qty),
                            ctx.cfg.lot_size,
                        );
                        if fill_qty >= ctx.cfg.min_order_size {
                            if ord.cancel_at_ms.is_some() {
                                ctx.cancel_in_flight_fills += 1;
                            }
                            ord.remaining_qty -= fill_qty;
                            let fill_px = ord.price;
                            let seq = ord.order_seq;
                            if ord.remaining_qty <= 1e-12 {
                                ctx.sim_orders[idx] = SimOrder::default();
                            } else {
                                ctx.sim_orders[idx] = ord;
                            }
                            ctx.arcus_cash_usd -= hit_side.sign() * fill_qty * fill_px;
                            let a_mid_f = ctx.last_arc_book.map_or(fill_px, |b| b.mid());
                            ctx.arcus_spread_capture_usd +=
                                hit_side.sign() * fill_qty * (a_mid_f - fill_px);
                            ctx.open_fills.push(OpenMakerFill {
                                sgn: hit_side.sign(),
                                rem_qty: fill_qty,
                                a_mid_f,
                            });
                            ctx.engine.on_passive_fill(
                                *now_ms,
                                hit_side,
                                Some(seq),
                                fill_qty,
                                fill_px,
                            )?;
                        } else {
                            ctx.sim_orders[idx] = ord;
                        }
                    }
                }
            }
        }
    }

    let m = ctx.engine.metrics();
    let a_mid_end = ctx.last_arc_book.map(|b| b.mid()).unwrap_or(0.0);
    let rh_mid_end = ctx.last_rh_book.map(|b| b.mid()).unwrap_or(a_mid_end);
    for f in &ctx.open_fills {
        if f.rem_qty > 1e-12 {
            ctx.arcus_adverse_usd += f.sgn * f.rem_qty * (a_mid_end - f.a_mid_f);
        }
    }
    let residual_drain_usd = (-m.rh_confirmed_pos) * (a_mid_end - rh_mid_end);
    let net_from_cash = ctx.arcus_cash_usd
        + ctx.rh_cash_usd
        + m.arc_confirmed_pos * a_mid_end
        + m.rh_confirmed_pos * rh_mid_end
        - m.fees_usd
        - m.fx_drag_usd;
    let net_from_decomp = ctx.arcus_spread_capture_usd
        + ctx.arcus_adverse_usd
        + ctx.rh_hedge_slip_usd
        + residual_drain_usd
        - m.fees_usd
        - m.fx_drag_usd;
    let conservation_error_usd = (net_from_cash - net_from_decomp).abs();

    let terminal_residual_notional_usd = m.net_unhedged_pos.abs() * a_mid_end;
    let gross_matched_notional_usd =
        m.arc_confirmed_pos.abs().min(m.rh_confirmed_pos.abs()) * a_mid_end;
    let combined_volume_usd = m.arcus_volume_usd + m.rh_volume_usd;

    Ok(TurnoverReplayReport {
        symbol: ctx.cfg.symbol,
        arcus_symbol: ctx.cfg.arcus_symbol,
        events_processed,
        ignored_post_cutoff_events,
        hedge_delay_ms: ctx.cfg.hedge_delay_ms,
        strict_through_prob,
        halted: m.halted,
        halt_reason: m.halt_reason,
        orders_placed: ctx.orders_placed,
        cancels_sent: ctx.cancels_sent,
        cancel_in_flight_fills: ctx.cancel_in_flight_fills,
        passive_fills: m.passive_fills,
        hedges_dispatched: m.hedges_dispatched,
        hedges_completed: m.hedges_completed,
        orphan_events: ctx.orphan_events,
        orphan_retries: m.orphan_retries,
        depth_clipped_hedges: ctx.depth_clipped_hedges,
        stale_hedge_blocks: ctx.stale_hedge_blocks,
        terminal_pending_hedges: m.pending_hedge_count,
        arcus_volume_usd: m.arcus_volume_usd,
        rh_volume_usd: m.rh_volume_usd,
        combined_volume_usd,
        arcus_spread_capture_usd: ctx.arcus_spread_capture_usd,
        arcus_adverse_usd: ctx.arcus_adverse_usd,
        rh_hedge_slip_usd: ctx.rh_hedge_slip_usd,
        residual_drain_usd,
        realized_only_pnl_usd: m.realized_cash_pnl_usd,
        mtm_incl_residual_pnl_usd: net_from_cash,
        conservation_error_usd,
        terminal_residual_notional_usd,
        gross_matched_notional_usd,
        cost_per_m_arcus_usd: m.cost_per_m_arcus_usd,
        cost_per_m_combined_usd: m.cost_per_m_combined_usd,
    })
}

/// Durable intent journal events for `--canary` restart safety.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum TurnoverJournalEvent {
    SessionStart {
        network: String,
        symbol: String,
        arcus_symbol: String,
        lighter_rh_market_id: i64,
        dry_run: bool,
        live_enabled: bool,
        journal_ms: u64,
    },
    IntentOpen {
        intent_id: String,
        kind: String,
        side: OrderSide,
        qty: f64,
        price: f64,
        journal_ms: u64,
    },
    IntentNotSent {
        intent_id: String,
        reason: String,
        journal_ms: u64,
    },
    IntentTerminal {
        intent_id: String,
        status: String,
        filled_qty: f64,
        avg_px: f64,
        arc_pos: f64,
        rh_pos: f64,
        journal_ms: u64,
    },
    CleanShutdown {
        journal_ms: u64,
    },
    ShutdownUnverified {
        reason: String,
        open_orders_arcus: Option<usize>,
        open_orders_rh: Option<usize>,
        arc_pos: Option<f64>,
        rh_pos: Option<f64>,
        journal_ms: u64,
    },
}

/// Exclusive state-file lock handle. Keep alive for the duration of a live session.
pub fn lock_state_file(path: &Path) -> Result<File> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let mut opts = OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    opts.mode(0o600);
    let file = opts.open(path)?;
    file.try_lock()
        .context("state file is already locked by another session")?;
    File::open(parent)?.sync_all()?;
    Ok(file)
}

/// Fail-closed durable intent journal: refuses startup if any intent is unresolved or
/// if the prior shutdown was unverified.
#[derive(Debug)]
pub struct TurnoverJournal {
    file: File,
}

impl TurnoverJournal {
    pub fn open_and_check_restart(path: PathBuf) -> Result<Self> {
        let file = lock_state_file(&path)?;
        Self::check_clean_restart(&path)?;
        Ok(Self { file })
    }

    pub fn check_clean_restart(path: &Path) -> Result<()> {
        let file = match File::open(path) {
            Ok(f) => f,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => {
                return Err(err).with_context(|| format!("open journal {}", path.display()));
            }
        };
        let reader = BufReader::new(file);
        let mut pending = HashSet::new();
        let mut blocking_shutdown: Option<String> = None;
        for (line_no, line) in reader.lines().enumerate() {
            let line =
                line.with_context(|| format!("read journal {}:{}", path.display(), line_no + 1))?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let ev: TurnoverJournalEvent = serde_json::from_str(trimmed)
                .with_context(|| format!("corrupt journal {}:{}", path.display(), line_no + 1))?;
            match ev {
                TurnoverJournalEvent::IntentOpen { intent_id, .. } => {
                    pending.insert(intent_id);
                }
                TurnoverJournalEvent::IntentNotSent { intent_id, .. }
                | TurnoverJournalEvent::IntentTerminal { intent_id, .. } => {
                    pending.remove(&intent_id);
                }
                TurnoverJournalEvent::CleanShutdown { .. } => {
                    blocking_shutdown = None;
                }
                TurnoverJournalEvent::ShutdownUnverified { reason, .. } => {
                    blocking_shutdown = Some(reason);
                }
                TurnoverJournalEvent::SessionStart { .. } => {}
            }
        }
        ensure!(
            pending.is_empty(),
            "dirty journal {}: unresolved intents {:?}; reconcile both venues before restart",
            path.display(),
            pending
        );
        if let Some(reason) = blocking_shutdown {
            bail!(
                "dirty journal {}: prior shutdown unverified ({reason}); reconcile and clear before restart",
                path.display()
            );
        }
        Ok(())
    }

    pub fn append(&self, event: &TurnoverJournalEvent) -> Result<()> {
        let mut file = &self.file;
        let mut encoded = serde_json::to_vec(event)?;
        encoded.push(b'\n');
        file.write_all(&encoded)?;
        file.sync_all()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_cfg() -> ArcusRhTurnoverConfig {
        ArcusRhTurnoverConfig {
            symbol: "NVDA".to_owned(),
            arcus_symbol: "NVDA-USD".to_owned(),
            lighter_rh_market_id: 15,
            symbol_id: 1005,
            tick_size: 0.01,
            lot_size: 0.01,
            min_order_size: 0.01,
            min_notional_usd: 10.0,
            clip_usd: 100.0,
            max_inventory_usd: 200.0,
            allocated_capital_usd: 500.0,
            daily_loss_limit_pct: 0.05,
            max_daily_loss_usd: 25.0,
            arcus_maker_fee_bps: 0.0,
            arcus_taker_fee_bps: 2.1375,
            rh_taker_fee_bps: 0.0,
            fx_drag_bps: 1.0,
            entry_edge_bps: 0.0,
            hedge_delay_ms: 330,
            submit_rtt_ms: 50,
            cancel_rtt_ms: 50,
            max_book_age_ms: 2_000,
            ..Default::default()
        }
    }

    fn book(bid: f64, ask: f64, sz: f64, ts: u64) -> VenueBook {
        VenueBook {
            bid_px: bid,
            ask_px: ask,
            bid_sz: sz,
            ask_sz: sz,
            source_ms: ts,
            receipt_ms: ts,
        }
    }

    #[test]
    fn config_rejects_zero_hedge_delay_and_unsafe_live_defaults() {
        let mut cfg = test_cfg();
        cfg.hedge_delay_ms = 0;
        assert!(cfg.validate().is_err());

        let safe = test_cfg();
        assert!(safe.validate().is_ok());
        // Fail-closed live gate refuses dry_run=true / live_enabled=false / missing flag.
        assert!(safe.validate_live_canary(true).is_err());

        let mut live = test_cfg();
        live.dry_run = false;
        live.live_enabled = true;
        assert!(live.validate_live_canary(false).is_err());
        assert!(live.validate_live_canary(true).is_ok());
    }

    #[test]
    fn monotonic_clock_guard_rejects_regression_on_every_callback() {
        let mut eng = ArcusRhTurnoverEngine::new(test_cfg()).unwrap();
        eng.on_book(
            10_000,
            book(99.98, 100.02, 5.0, 10_000),
            book(99.99, 100.01, 5.0, 10_000),
        )
        .unwrap();
        assert!(
            eng.on_book(
                9_999,
                book(99.98, 100.02, 5.0, 9_999),
                book(99.99, 100.01, 5.0, 9_999)
            )
            .is_err()
        );
        assert_eq!(eng.halt_reason(), Some(HaltReason::ClockRegression));

        let mut eng2 = ArcusRhTurnoverEngine::new(test_cfg()).unwrap();
        eng2.reconcile_positions(5_000, 0.0, 0.0).unwrap();
        assert!(
            eng2.on_passive_fill(4_999, OrderSide::Buy, None, 1.0, 100.0)
                .is_err()
        );

        let mut eng3 = ArcusRhTurnoverEngine::new(test_cfg()).unwrap();
        eng3.reconcile_positions(5_000, 0.0, 0.0).unwrap();
        assert!(eng3.on_cancel_ack(4_999, OrderSide::Buy, None).is_err());
    }

    #[test]
    fn passive_fill_takes_precedence_over_cancel_in_flight() {
        let mut eng = ArcusRhTurnoverEngine::new(test_cfg()).unwrap();
        let step = eng
            .on_book(
                10_000,
                book(99.98, 100.02, 5.0, 10_000),
                book(99.99, 100.01, 5.0, 10_000),
            )
            .unwrap();
        let (bid_seq, bid_px, bid_qty) = match step.quote_actions[0].unwrap() {
            QuoteActionKind::PlacePostOnly {
                side: OrderSide::Buy,
                price,
                qty,
                order_seq,
            } => (order_seq, price, qty),
            other => panic!("unexpected action {other:?}"),
        };

        // Trigger a cancel on Bid via stale book withdrawal -> Bid slot becomes CancelPending.
        let stale_step = eng
            .on_book(
                15_000,
                book(99.98, 100.02, 5.0, 10_000),
                book(99.99, 100.01, 5.0, 15_000),
            )
            .unwrap();
        assert!(stale_step.withdrew_stale);
        let (worst_long_before_ack, _) = eng.arc_worst_case_bounds();
        assert!((worst_long_before_ack - bid_qty).abs() < 1e-9);

        // Passive fill arrives BEFORE cancel ack! Must be accounted and schedule 330 ms hedge.
        eng.on_passive_fill(15_010, OrderSide::Buy, Some(bid_seq), bid_qty, bid_px)
            .unwrap();
        // Late cancel ack arrives AFTER full fill -> must be a clean no-op.
        eng.on_cancel_ack(15_050, OrderSide::Buy, Some(bid_seq))
            .unwrap();

        let m = eng.metrics();
        assert!((m.arc_confirmed_pos - bid_qty).abs() < 1e-9);
        assert_eq!(m.pending_hedge_count, 1);

        // Hedge MUST NOT dispatch before +330 ms (15_340).
        let before_delay = eng
            .on_book(
                15_300,
                book(99.98, 100.02, 5.0, 15_300),
                book(99.99, 100.01, 5.0, 15_300),
            )
            .unwrap();
        assert!(before_delay.hedge_dispatch.is_none());

        // At 15_340 (>= 15_010 + 330), hedge dispatches!
        let at_delay = eng
            .on_book(
                15_340,
                book(99.98, 100.02, 5.0, 15_340),
                book(99.99, 100.01, 5.0, 15_340),
            )
            .unwrap();
        let dispatch = at_delay
            .hedge_dispatch
            .expect("hedge must dispatch at +330 ms");
        assert_eq!(dispatch.side, OrderSide::Sell);
        assert!((dispatch.qty - bid_qty).abs() < 1e-9);
    }

    #[test]
    fn cost_per_million_circuit_breaker_and_usdg_divergence_halt_fail_closed() {
        let mut cfg = test_cfg();
        cfg.max_cost_per_million_usd = 5.0;
        cfg.min_cost_breaker_volume_usd = 100.0;
        let mut eng = ArcusRhTurnoverEngine::new(cfg).unwrap();

        // Fill $200 on Arcus, hedge at a bad price costing $0.05 -> $250/M (> $5/M).
        eng.on_passive_fill(1_000, OrderSide::Buy, None, 2.0, 100.0)
            .unwrap();
        let step = eng
            .on_book(
                1_330,
                book(99.98, 100.02, 5.0, 1_330),
                book(99.975, 100.01, 5.0, 1_330),
            )
            .unwrap();
        let d = step.hedge_dispatch.unwrap();
        eng.on_hedge_complete(1_331, d.hedge_id, 2.0, 99.975, false)
            .unwrap();
        assert_eq!(eng.halt_reason(), Some(HaltReason::CostPerMillionBreached));

        // USDG/USDC peg > 30 bps halts immediately.
        let mut eng_peg = ArcusRhTurnoverEngine::new(test_cfg()).unwrap();
        eng_peg.on_usdg_usdc_peg(1_000, 0.9960).unwrap();
        assert_eq!(eng_peg.halt_reason(), Some(HaltReason::UsdgUsdcDivergence));
    }

    #[test]
    fn orphan_retry_bounded_and_consecutive_hedge_failures_halt() {
        let mut cfg = test_cfg();
        cfg.max_consecutive_hedge_failures = 2;
        cfg.max_orphan_retries = 2;
        let mut eng = ArcusRhTurnoverEngine::new(cfg).unwrap();

        eng.on_passive_fill(1_000, OrderSide::Buy, None, 1.0, 100.0)
            .unwrap();
        // Failure 1 at 1_330 -> re-queued for 1_660.
        let d1 = eng
            .on_book(
                1_330,
                book(99.98, 100.02, 5.0, 1_330),
                book(99.99, 100.01, 5.0, 1_330),
            )
            .unwrap()
            .hedge_dispatch
            .unwrap();
        eng.on_hedge_complete(1_331, d1.hedge_id, 0.0, 0.0, true)
            .unwrap();
        assert!(!eng.is_halted());
        assert_eq!(eng.metrics().orphan_retries, 1);

        // Must wait +330 ms before retry.
        assert!(
            eng.on_book(
                1_600,
                book(99.98, 100.02, 5.0, 1_600),
                book(99.99, 100.01, 5.0, 1_600),
            )
            .unwrap()
            .hedge_dispatch
            .is_none()
        );

        // Failure 2 at 1_661 -> trips ConsecutiveHedgeFailures.
        let d2 = eng
            .on_book(
                1_661,
                book(99.98, 100.02, 5.0, 1_661),
                book(99.99, 100.01, 5.0, 1_661),
            )
            .unwrap()
            .hedge_dispatch
            .unwrap();
        eng.on_hedge_complete(1_662, d2.hedge_id, 0.0, 0.0, true)
            .unwrap();
        assert_eq!(
            eng.halt_reason(),
            Some(HaltReason::ConsecutiveHedgeFailures)
        );
    }

    #[test]
    fn replay_enforces_single_use_l1_depth_and_declared_end_ms() {
        let mut cfg = test_cfg();
        cfg.max_l1_depth_fraction = 0.5;
        cfg.declared_end_ms = Some(2_000);
        cfg.submit_rtt_ms = 50;
        cfg.hedge_delay_ms = 330;
        cfg.entry_edge_bps = -1.0; // zero-points negative-edge mode allowed

        let tape = vec![
            TurnoverReplayEvent::Bbo {
                now_ms: 1_000,
                arc_bid: 99.99,
                arc_ask: 100.01,
                arc_bid_sz: 2.0,
                arc_ask_sz: 2.0,
                arc_recv_ms: Some(1_000),
                rh_bid: 100.00,
                rh_ask: 100.01,
                rh_bid_sz: 2.0,
                rh_ask_sz: 2.0,
                rh_recv_ms: Some(1_000),
            },
            // Print hits our passive Bid at 1_100 -> fill 1.00, hedge due 1_430.
            TurnoverReplayEvent::ArcusTrade {
                now_ms: 1_100,
                price: 99.99,
                size: 5.0,
                is_buy_aggressor: Some(false),
                side: None,
            },
            // At 1_450 RH bid depth is 2.0 * 0.5 = 1.0 -> full hedge completes.
            TurnoverReplayEvent::Bbo {
                now_ms: 1_450,
                arc_bid: 99.99,
                arc_ask: 100.01,
                arc_bid_sz: 2.0,
                arc_ask_sz: 2.0,
                arc_recv_ms: Some(1_450),
                rh_bid: 100.00,
                rh_ask: 100.01,
                rh_bid_sz: 2.0,
                rh_ask_sz: 2.0,
                rh_recv_ms: Some(1_450),
            },
            // Past declared_end_ms -> strictly ignored.
            TurnoverReplayEvent::ArcusTrade {
                now_ms: 2_500,
                price: 99.99,
                size: 5.0,
                is_buy_aggressor: Some(false),
                side: None,
            },
        ];

        let rep = run_deterministic_replay(cfg, &tape, 0.0).unwrap();
        assert_eq!(rep.events_processed, 3);
        assert_eq!(rep.ignored_post_cutoff_events, 1);
        assert_eq!(rep.passive_fills, 1);
        assert_eq!(rep.hedges_completed, 1);
        assert!(rep.terminal_residual_notional_usd < 1e-9);
    }

    #[test]
    fn worst_case_exposure_reserves_both_legs_and_respects_tick_lot() {
        let mut cfg = test_cfg();
        cfg.clip_usd = 100.0;
        cfg.max_inventory_usd = 150.0;
        let mut eng = ArcusRhTurnoverEngine::new(cfg).unwrap();

        // First quote places 1.00 lot (~$100) on Bid & Ask (within $150 max_inventory_usd).
        let s1 = eng
            .on_book(
                10_000,
                book(99.98, 100.02, 5.0, 10_000),
                book(99.99, 100.01, 5.0, 10_000),
            )
            .unwrap();
        assert_eq!(s1.quote_action_len, 2);
        let (wl, ws) = eng.arc_worst_case_bounds();
        assert!(wl <= 1.50 + 1e-9);
        assert!(ws >= -1.50 - 1e-9);

        // Fill 1.00 on Buy -> Arcus confirmed=+1.00, pending RH Sell hedge=-1.00.
        // Remaining room for additional Buy is only ~0.50 ($50), capped by both legs.
        eng.on_passive_fill(10_050, OrderSide::Buy, Some(1), 1.0, 99.99)
            .unwrap();
        let s2 = eng
            .on_book(
                10_100,
                book(99.98, 100.02, 5.0, 10_100),
                book(99.99, 100.01, 5.0, 10_100),
            )
            .unwrap();
        for act in s2.quote_slice().iter().flatten() {
            if let QuoteActionKind::PlacePostOnly {
                side: OrderSide::Buy,
                price,
                qty,
                ..
            } = *act
            {
                assert!(price < 100.02);
                assert!(
                    (qty - 0.50).abs() < 1e-9,
                    "expected capped qty 0.50, got {qty}"
                );
            }
        }
        let (wl2, _) = eng.arc_worst_case_bounds();
        let (_, rh_ws2) = eng.rh_worst_case_bounds();
        assert!(wl2 <= 1.50 + 1e-9);
        assert!(rh_ws2 >= -1.50 - 1e-9);
    }

    #[test]
    fn daily_loss_limit_and_strict_through_prob_replay_work() {
        let mut cfg = test_cfg();
        cfg.max_daily_loss_usd = 2.0;
        let mut eng = ArcusRhTurnoverEngine::new(cfg).unwrap();
        eng.on_passive_fill(1_000, OrderSide::Buy, None, 1.0, 100.0)
            .unwrap();
        // Adverse move drops mark to 97.50 -> unrealized loss -$2.50 < -$2.00 limit.
        let step = eng
            .on_book(
                1_100,
                book(97.49, 97.51, 5.0, 1_100),
                book(97.49, 97.51, 5.0, 1_100),
            )
            .unwrap();
        assert!(eng.is_halted());
        assert_eq!(eng.halt_reason(), Some(HaltReason::DailyLossLimit));
        assert!(step.quote_action_len == 0);

        // strict_through_prob = 1.0 fills passive resting Bid when Arcus ask drops below it.
        let mut rcfg = test_cfg();
        rcfg.submit_rtt_ms = 10;
        rcfg.hedge_delay_ms = 330;
        let tape = vec![
            TurnoverReplayEvent::Bbo {
                now_ms: 1_000,
                arc_bid: 99.98,
                arc_ask: 100.02,
                arc_bid_sz: 5.0,
                arc_ask_sz: 5.0,
                arc_recv_ms: None,
                rh_bid: 99.99,
                rh_ask: 100.01,
                rh_bid_sz: 5.0,
                rh_ask_sz: 5.0,
                rh_recv_ms: None,
            },
            // Arcus ask drops through our passive Bid (~99.99) -> strict-through fill!
            TurnoverReplayEvent::Bbo {
                now_ms: 1_050,
                arc_bid: 99.95,
                arc_ask: 99.97,
                arc_bid_sz: 5.0,
                arc_ask_sz: 5.0,
                arc_recv_ms: None,
                rh_bid: 99.96,
                rh_ask: 99.98,
                rh_bid_sz: 5.0,
                rh_ask_sz: 5.0,
                rh_recv_ms: None,
            },
            // Hedge executes at >= 1_380.
            TurnoverReplayEvent::Bbo {
                now_ms: 1_400,
                arc_bid: 99.95,
                arc_ask: 99.97,
                arc_bid_sz: 5.0,
                arc_ask_sz: 5.0,
                arc_recv_ms: None,
                rh_bid: 99.96,
                rh_ask: 99.98,
                rh_bid_sz: 5.0,
                rh_ask_sz: 5.0,
                rh_recv_ms: None,
            },
        ];
        let rep = run_deterministic_replay(rcfg, &tape, 1.0).unwrap();
        assert!(rep.passive_fills >= 1);
        assert_eq!(rep.hedges_completed, rep.passive_fills);
    }

    #[test]
    fn p0_24_event_parity_fixture_matches_python_reference() {
        let t0: u64 = 1_700_000_000_000;
        let cfg = ArcusRhTurnoverConfig {
            symbol: "NVDA".to_owned(),
            arcus_symbol: "NVDA-USD".to_owned(),
            tick_size: 0.01,
            lot_size: 0.01,
            min_order_size: 0.01,
            min_notional_usd: 10.0,
            clip_usd: 100.0,
            max_inventory_usd: 300.0,
            entry_edge_bps: -0.5,
            inventory_skew_bps: 0.0,
            lead_beta: 0.0,
            microprice_alpha_weight: 0.0,
            basis_ewma_tau_ms: 3_600_000.0,
            max_book_age_ms: 5_000,
            queue_ahead_fraction: 0.5,
            submit_rtt_ms: 50,
            cancel_rtt_ms: 100,
            hedge_delay_ms: 330,
            max_orphan_retries: 5,
            max_l1_depth_fraction: 1.0,
            max_improve_ticks: 0,
            requote_threshold_ticks: 0,
            arcus_maker_fee_bps: 0.0,
            rh_taker_fee_bps: 0.0,
            fx_drag_bps: 0.0,
            warmup_ms: 100,
            terminal_drain_ms: Some(500),
            declared_end_ms: Some(t0 + 10_000),
            ..Default::default()
        };

        let ab = |dt: u64, bid: f64, bsz: f64, ask: f64, asz: f64| TurnoverReplayEvent::ArcusBook {
            now_ms: t0 + dt,
            bid,
            ask,
            bid_sz: bsz,
            ask_sz: asz,
            recv_ms: Some(t0 + dt),
        };
        let hb = |dt: u64, bid: f64, bsz: f64, ask: f64, asz: f64| TurnoverReplayEvent::HedgeBook {
            now_ms: t0 + dt,
            bid,
            ask,
            bid_sz: bsz,
            ask_sz: asz,
            recv_ms: Some(t0 + dt),
        };
        let tr = |dt: u64, side: &str, price: f64, size: f64| TurnoverReplayEvent::ArcusTrade {
            now_ms: t0 + dt,
            price,
            size,
            is_buy_aggressor: None,
            side: Some(side.to_owned()),
        };

        let tape = vec![
            ab(0, 99.99, 4.0, 100.01, 4.0),
            hb(0, 99.99, 4.0, 100.01, 4.0),
            ab(200, 99.99, 4.0, 100.01, 4.0),
            hb(200, 99.99, 4.0, 100.01, 4.0),
            tr(300, "SELL", 99.99, 2.5),
            hb(400, 99.98, 0.30, 100.00, 4.0),
            ab(650, 99.97, 4.0, 99.99, 4.0),
            tr(700, "BUY", 100.01, 3.0),
            ab(1000, 99.97, 4.0, 99.99, 4.0),
            hb(1100, 99.98, 2.0, 100.00, 2.0),
            ab(1400, 99.97, 4.0, 99.99, 4.0),
            hb(2000, 99.97, 5.0, 99.99, 5.0),
            ab(2000, 99.97, 2.0, 99.99, 2.0),
            tr(2100, "SELL", 99.97, 3.0),
            ab(7500, 99.98, 4.0, 100.00, 4.0),
            hb(7900, 99.98, 5.0, 100.00, 5.0),
            ab(8300, 99.98, 4.0, 100.00, 4.0),
            hb(9600, 99.98, 5.0, 100.00, 5.0),
            ab(9600, 99.98, 2.0, 100.00, 2.0),
            tr(9700, "BUY", 100.00, 3.0),
            ab(9950, 99.99, 4.0, 100.01, 4.0),
            hb(9950, 99.99, 4.0, 100.01, 4.0),
            ab(10050, 105.00, 10.0, 105.02, 10.0),
            tr(10050, "BUY", 105.02, 5.0),
        ];

        let rep = run_deterministic_replay(cfg, &tape, 0.0).unwrap();
        assert_eq!(rep.ignored_post_cutoff_events, 2);
        // Active quoting slots match `orders_placed - cancels_sent == 2` (exact fill/hedge/PnL parity).
        assert_eq!(rep.orders_placed - rep.cancels_sent, 2, "rep={rep:?}");
        assert_eq!(rep.passive_fills, 4, "rep={rep:?}");
        assert_eq!(rep.hedges_completed, 4, "rep={rep:?}");
        assert_eq!(rep.cancel_in_flight_fills, 1, "rep={rep:?}");
        assert_eq!(rep.orphan_events, 1, "rep={rep:?}");
        assert_eq!(rep.orphan_retries, 4, "rep={rep:?}");
        assert_eq!(rep.depth_clipped_hedges, 2, "rep={rep:?}");
        assert_eq!(rep.stale_hedge_blocks, 2, "rep={rep:?}");
        assert_eq!(rep.terminal_pending_hedges, 1, "rep={rep:?}");
        assert!(
            (rep.arcus_volume_usd - 298.9749).abs() < 1e-6,
            "arc_vol={}",
            rep.arcus_volume_usd
        );
        assert!(
            (rep.rh_volume_usd - 248.97).abs() < 1e-6,
            "rh_vol={}",
            rep.rh_volume_usd
        );
        assert!(
            (rep.combined_volume_usd - 547.9449).abs() < 1e-6,
            "comb={}",
            rep.combined_volume_usd
        );
        assert!(
            (rep.arcus_spread_capture_usd - 0.0497).abs() < 1e-6,
            "spr={}",
            rep.arcus_spread_capture_usd
        );
        assert!(
            (rep.arcus_adverse_usd - 0.001).abs() < 1e-6,
            "adv={}",
            rep.arcus_adverse_usd
        );
        assert!(
            (rep.rh_hedge_slip_usd - (-0.0358)).abs() < 1e-6,
            "slip={}",
            rep.rh_hedge_slip_usd
        );
        assert!(
            rep.residual_drain_usd.abs() < 1e-9,
            "drain={}",
            rep.residual_drain_usd
        );
        assert!(
            (rep.mtm_incl_residual_pnl_usd - 0.0149).abs() < 1e-6,
            "net={}",
            rep.mtm_incl_residual_pnl_usd
        );
        assert!(
            rep.conservation_error_usd < 1e-9,
            "cons={}",
            rep.conservation_error_usd
        );
    }
}
