//! Dedicated `Arcus <-> Hyperliquid HIP-3 (xyz)` cross-venue basis arbitrage engine (`Step 3`).
//!
//! Translates the OOS-candidate `EXP-ARB-A2` configuration into a standalone state machine
//! with durable intent journaling (`BasisJournal`), flight locks, residual-first hedging,
//! causal persistence, Leung-Li upper dislocation bounds, asymmetric adding-only inventory
//! laddering, and progressive intraday drawdown scaling (`DailyLossGuard::risk_scale`).

use crate::daily_loss::DailyLossGuard;
use crate::exchanges::arcus::{
    ExecutionError as ArcusExecutionError, Fill as ArcusFill, Market as ArcusMarket,
    Order as ArcusOrder, OrderReceipt as ArcusOrderReceipt, Side as ArcusSide,
};
use crate::exchanges::hyperliquid::{
    AccountState as HlAccountState, ExecutionStatus as HlExecutionStatus, OpenOrder as HlOpenOrder,
    OrderLookup as HlOrderLookup, PerpAsset as HlPerpAsset, SubmissionError as HlSubmissionError,
    ValidatedBook,
};
use crate::live_hedge::{
    IocIdentity, LOOKUP_SKEW_MS, LookupOutcome, account_confirmed_fill, classify_lookup,
};
use anyhow::{Context, Result, bail, ensure};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::{Decimal, RoundingStrategy};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

fn default_entry_bp() -> f64 {
    10.0
}
fn default_max_entry_bp() -> f64 {
    35.0
}
fn default_persist_ms() -> u64 {
    100
}
fn default_true() -> bool {
    true
}
fn default_ladder_floor_frac() -> f64 {
    0.25
}
fn default_ladder_scale_bps() -> f64 {
    2.0
}
fn default_ladder_convex_power() -> f64 {
    1.0
}
fn default_max_hold_ms() -> u64 {
    7_200_000
}
fn default_max_arcus_age_ms() -> u64 {
    250
}
pub const HL_MIN_ORDER_NOTIONAL_USD: f64 = 10.0;

fn default_max_hl_age_ms() -> u64 {
    5_000
}
fn default_max_venue_clock_skew_ms() -> u64 {
    2_000
}
fn default_slippage_bps() -> f64 {
    8.0
}
fn default_poll_interval_ms() -> u64 {
    250
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ArcusXyzBasisConfig {
    pub network: String,
    pub arcus_symbol: String,
    pub hl_dex: String,
    pub hl_coin: String,
    #[serde(default = "default_entry_bp")]
    pub entry_bp: f64,
    #[serde(default)]
    pub exit_bp: f64,
    #[serde(default = "default_max_entry_bp")]
    pub max_entry_bp: f64,
    #[serde(default = "default_persist_ms")]
    pub persist_ms: u64,
    #[serde(default = "default_true")]
    pub post_trade_refresh_lock: bool,
    pub clip_usd: f64,
    pub max_notional_usd: f64,
    #[serde(default = "default_ladder_floor_frac")]
    pub ladder_floor_frac: f64,
    #[serde(default = "default_ladder_scale_bps")]
    pub ladder_scale_bps: f64,
    #[serde(default = "default_ladder_convex_power")]
    pub ladder_convex_power: f64,
    #[serde(default = "default_max_hold_ms")]
    pub max_hold_ms: u64,
    #[serde(default = "default_max_arcus_age_ms")]
    pub max_arcus_book_age_ms: u64,
    #[serde(default = "default_max_hl_age_ms")]
    pub max_hl_book_age_ms: u64,
    #[serde(default = "default_max_venue_clock_skew_ms")]
    pub max_venue_clock_skew_ms: u64,
    #[serde(default = "default_slippage_bps")]
    pub slippage_bps: f64,
    pub allocated_capital_usd: f64,
    pub daily_loss_limit_pct: f64,
    pub canary_max_order_usd: f64,
    pub canary_max_total_notional_usd: f64,
    #[serde(default = "default_true")]
    pub require_arcus_cancel_lease: bool,
    #[serde(default = "default_true")]
    pub dry_run: bool,
    #[serde(default = "default_poll_interval_ms")]
    pub poll_interval_ms: u64,
    pub journal_path: PathBuf,
    pub daily_loss_state_path: PathBuf,
}

impl ArcusXyzBasisConfig {
    pub fn book_problem(
        &self,
        now_ms: u64,
        arc: &ArcusBookSnapshot,
        xyz: &ValidatedBook,
        xyz_rx_ms: u64,
    ) -> Option<&'static str> {
        if arc.bid_px <= Decimal::ZERO
            || arc.ask_px <= arc.bid_px
            || arc.bid_sz <= Decimal::ZERO
            || arc.ask_sz <= Decimal::ZERO
        {
            return Some("invalid_arcus_book");
        }
        if now_ms.saturating_sub(arc.receipt_ms) > self.max_arcus_book_age_ms {
            return Some("stale_arcus_book");
        }
        if now_ms.saturating_sub(xyz_rx_ms) > self.max_hl_book_age_ms {
            return Some("stale_xyz_book");
        }
        let skew = self.max_venue_clock_skew_ms;
        if arc.source_ms == 0 || arc.source_ms > now_ms.saturating_add(skew) {
            return Some("invalid_arcus_source_time");
        }
        if now_ms.saturating_sub(arc.source_ms) > self.max_arcus_book_age_ms.saturating_add(skew) {
            return Some("stale_arcus_source_book");
        }
        if xyz.bbo.timestamp_ms == 0 || xyz.bbo.timestamp_ms > now_ms.saturating_add(skew) {
            return Some("invalid_xyz_source_time");
        }
        if now_ms.saturating_sub(xyz.bbo.timestamp_ms)
            > self.max_hl_book_age_ms.saturating_add(skew)
        {
            return Some("stale_xyz_source_book");
        }
        None
    }

    /// Largest additional quantity permitted at this leg's protected execution value.
    pub fn leg_capacity(
        &self,
        price: Decimal,
        position: Decimal,
        buy: bool,
        reduce_only: bool,
    ) -> Decimal {
        if price <= Decimal::ZERO {
            return Decimal::ZERO;
        }
        let order = decimal_from_f64(self.canary_max_order_usd.min(self.clip_usd))
            .unwrap_or_default()
            / price;
        let along = if buy { position } else { -position };
        let room = if reduce_only {
            -along
        } else {
            decimal_from_f64(
                self.max_notional_usd
                    .min(self.canary_max_total_notional_usd),
            )
            .unwrap_or_default()
                / price
                - along
        };
        order.min(room).max(Decimal::ZERO)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            matches!(self.network.as_str(), "mainnet" | "testnet"),
            "network must be mainnet or testnet"
        );
        ensure!(
            !self.arcus_symbol.trim().is_empty() && !self.hl_coin.trim().is_empty(),
            "arcus_symbol and hl_coin must be non-empty"
        );
        if !self.hl_dex.is_empty() {
            let expected_prefix = format!("{}:", self.hl_dex);
            ensure!(
                self.hl_coin.starts_with(&expected_prefix)
                    && self.hl_coin.len() > expected_prefix.len(),
                "hl_coin '{}' must match hl_dex '{}' prefix",
                self.hl_coin,
                self.hl_dex
            );
        }
        ensure!(
            self.entry_bp.is_finite()
                && self.exit_bp.is_finite()
                && self.max_entry_bp.is_finite()
                && self.entry_bp > 0.0
                && self.max_entry_bp > self.entry_bp,
            "require finite 0 < entry_bp < max_entry_bp"
        );
        ensure!(
            self.clip_usd.is_finite()
                && self.max_notional_usd.is_finite()
                && self.clip_usd >= HL_MIN_ORDER_NOTIONAL_USD
                && self.max_notional_usd >= self.clip_usd,
            "require {HL_MIN_ORDER_NOTIONAL_USD:.1} <= clip_usd <= max_notional_usd"
        );
        ensure!(
            self.ladder_floor_frac.is_finite()
                && (0.0..1.0).contains(&self.ladder_floor_frac)
                && self.ladder_scale_bps.is_finite()
                && self.ladder_scale_bps >= 0.0
                && self.ladder_convex_power.is_finite()
                && self.ladder_convex_power >= 1.0,
            "invalid asymmetric ladder parameters"
        );
        ensure!(
            self.max_hold_ms >= 60_000
                && self.max_arcus_book_age_ms > 0
                && self.max_hl_book_age_ms > 0
                && self.max_venue_clock_skew_ms <= 30_000
                && self.poll_interval_ms >= 50,
            "invalid hold, freshness, clock-skew, or polling timing bounds"
        );
        ensure!(
            self.slippage_bps.is_finite() && self.slippage_bps > 0.0 && self.slippage_bps <= 100.0,
            "slippage_bps must be in (0, 100]"
        );
        ensure!(
            self.allocated_capital_usd.is_finite()
                && self.allocated_capital_usd > 0.0
                && self.daily_loss_limit_pct.is_finite()
                && self.daily_loss_limit_pct > 0.0
                && self.daily_loss_limit_pct < 1.0,
            "invalid allocated capital or daily loss limit"
        );
        ensure!(
            self.canary_max_order_usd.is_finite()
                && self.canary_max_total_notional_usd.is_finite()
                && self.canary_max_order_usd >= HL_MIN_ORDER_NOTIONAL_USD
                && self.canary_max_total_notional_usd >= self.canary_max_order_usd,
            "invalid canary order/notional caps"
        );
        ensure!(
            !self.journal_path.as_os_str().is_empty()
                && !self.daily_loss_state_path.as_os_str().is_empty(),
            "journal_path and daily_loss_state_path must be non-empty"
        );
        Ok(())
    }

    /// Live canary guard: requires explicit opt-out from `dry_run` and tight notional bounds.
    pub fn validate_canary_caps(&self) -> Result<()> {
        self.validate()?;
        ensure!(
            !self.dry_run,
            "refusing live canary while dry_run = true in config"
        );
        ensure!(
            self.clip_usd <= self.canary_max_order_usd,
            "clip_usd (${:.2}) exceeds canary_max_order_usd (${:.2})",
            self.clip_usd,
            self.canary_max_order_usd
        );
        ensure!(
            self.max_notional_usd <= self.canary_max_total_notional_usd,
            "max_notional_usd (${:.2}) exceeds canary_max_total_notional_usd (${:.2})",
            self.max_notional_usd,
            self.canary_max_total_notional_usd
        );
        Ok(())
    }
}

/// Top-of-book Arcus snapshot keeping venue source time separate from local receipt time.
#[derive(Debug, Clone)]
pub struct ArcusBookSnapshot {
    pub source_ms: u64,
    pub receipt_ms: u64,
    pub bid_px: Decimal,
    pub bid_sz: Decimal,
    pub ask_px: Decimal,
    pub ask_sz: Decimal,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BasisAction {
    Hold {
        reason: &'static str,
    },
    HedgeResidualXyz {
        is_buy: bool,
        qty: Decimal,
        limit_price: Decimal,
        reduce_only: bool,
    },
    ExitPairIoc {
        direction_arcus_buy: bool,
        qty: Decimal,
        arcus_limit_px: Decimal,
        xyz_limit_px: Decimal,
        timed_out: bool,
    },
    EnterPairIoc {
        direction_arcus_buy: bool,
        qty: Decimal,
        arcus_limit_px: Decimal,
        xyz_limit_px: Decimal,
        raw_edge_bp: f64,
        req_edge_bp: f64,
        brake_scale: f64,
    },
}

fn gcd_u128(mut a: u128, mut b: u128) -> u128 {
    while b != 0 {
        let r = a % b;
        a = b;
        b = r;
    }
    a
}

/// Quantize base size down conservatively to HL's lot precision (`sz_decimals`).
pub fn quantize_hl_base_qty(raw_qty: Decimal, hl_sz_decimals: u32) -> Result<Decimal> {
    ensure!(
        raw_qty > Decimal::ZERO && hl_sz_decimals <= 6,
        "invalid HL quantization parameters"
    );
    Ok(raw_qty
        .round_dp_with_strategy(hl_sz_decimals, RoundingStrategy::ToZero)
        .normalize())
}

/// Quantize base size down onto the exact common LCM grid of Arcus `step_size` and HL `sz_decimals`.
pub fn quantize_shared_base_qty(
    raw_qty: Decimal,
    arcus_step: Decimal,
    hl_sz_decimals: u32,
) -> Result<Decimal> {
    ensure!(
        raw_qty > Decimal::ZERO && arcus_step > Decimal::ZERO && hl_sz_decimals <= 6,
        "invalid quantization parameters"
    );
    let arc_norm = arcus_step.normalize();
    let hl_step = Decimal::new(1, hl_sz_decimals).normalize();
    let scale = arc_norm.scale().max(hl_step.scale());
    ensure!(scale <= 12, "step size precision exceeds 12 decimal places");
    let pow10 = 10_i128.checked_pow(scale).context("step scale overflow")?;
    let pow10_dec = Decimal::from_i128_with_scale(pow10, 0);
    let arc_scaled = (arc_norm * pow10_dec).normalize();
    ensure!(
        arc_scaled.fract().is_zero() && arc_scaled > Decimal::ZERO,
        "Arcus step_size {arcus_step} cannot be represented cleanly at scale {scale}"
    );
    let arc_ticks = u128::try_from(arc_scaled.mantissa()).context("Arcus step ticks overflow")?;
    let hl_scaled = (hl_step * pow10_dec).normalize();
    let hl_ticks = u128::try_from(hl_scaled.mantissa()).context("HL step ticks overflow")?;
    let g = gcd_u128(arc_ticks, hl_ticks);
    let lcm_ticks = (arc_ticks / g)
        .checked_mul(hl_ticks)
        .context("shared step LCM overflow")?;
    let lcm_i128 = i128::try_from(lcm_ticks).context("shared step exceeds Decimal range")?;
    let shared_step = Decimal::from_i128_with_scale(lcm_i128, scale).normalize();

    let units = (raw_qty / shared_step)
        .round_dp_with_strategy(0, RoundingStrategy::ToZero)
        .max(Decimal::ZERO);
    let q = (units * shared_step).normalize();
    if q > Decimal::ZERO {
        ensure!(
            (q % arcus_step).is_zero(),
            "quantized qty {q} is not a multiple of arcus_step {arcus_step}"
        );
        ensure!(
            q.scale() <= hl_sz_decimals,
            "quantized qty {q} exceeds hl_sz_decimals {hl_sz_decimals}"
        );
    }
    Ok(q)
}

#[derive(Debug, Clone, Copy)]
pub struct VenueOrderConstraints {
    pub arcus_step_size: Decimal,
    pub arcus_min_order_size: Decimal,
    pub arcus_min_order_notional_usd: f64,
    pub hl_sz_decimals: u32,
    pub hl_min_order_notional_usd: f64,
}

impl Default for VenueOrderConstraints {
    fn default() -> Self {
        Self {
            arcus_step_size: Decimal::new(1, 6),
            arcus_min_order_size: Decimal::new(1, 6),
            arcus_min_order_notional_usd: 5.0,
            hl_sz_decimals: 2,
            hl_min_order_notional_usd: HL_MIN_ORDER_NOTIONAL_USD,
        }
    }
}

impl VenueOrderConstraints {
    pub fn from_markets(arc: &ArcusMarket, hl: &HlPerpAsset) -> Result<Self> {
        ensure!(
            arc.step_size > Decimal::ZERO
                && arc.min_order_size >= Decimal::ZERO
                && arc.min_order_notional >= Decimal::ZERO
                && hl.sz_decimals <= 6,
            "invalid venue market constraints"
        );
        let arc_min_ntl = arc
            .min_order_notional
            .to_f64()
            .filter(|v| v.is_finite() && *v >= 0.0)
            .context("invalid Arcus min_order_notional")?;
        Ok(Self {
            arcus_step_size: arc.step_size,
            arcus_min_order_size: arc.min_order_size.max(arc.step_size),
            arcus_min_order_notional_usd: arc_min_ntl,
            hl_sz_decimals: hl.sz_decimals,
            hl_min_order_notional_usd: HL_MIN_ORDER_NOTIONAL_USD,
        })
    }

    pub fn min_entry_notional_usd(&self) -> f64 {
        HL_MIN_ORDER_NOTIONAL_USD
            .max(self.hl_min_order_notional_usd)
            .max(self.arcus_min_order_notional_usd)
    }
}

/// Average-cost Arcus leg PnL estimator from verified fills and local mid price.
/// Kept distinct from reported Hyperliquid equity in diagnostics/alerts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArcusLegPnlTracker {
    #[serde(with = "rust_decimal::serde::str")]
    position: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    avg_cost: Decimal,
    realized_pnl_usd: f64,
    estimated_fees_usd: f64,
    resynced: bool,
}

impl ArcusLegPnlTracker {
    pub fn new(initial_pos: Decimal, initial_mid: Option<Decimal>) -> Self {
        let avg_cost = if initial_pos == Decimal::ZERO {
            Decimal::ZERO
        } else {
            initial_mid
                .filter(|m| *m > Decimal::ZERO)
                .unwrap_or(Decimal::ZERO)
        };
        Self {
            position: initial_pos,
            avg_cost,
            realized_pnl_usd: 0.0,
            estimated_fees_usd: 0.0,
            resynced: false,
        }
    }

    pub fn position(&self) -> Decimal {
        self.position
    }

    pub fn avg_cost(&self) -> Decimal {
        self.avg_cost
    }

    pub fn realized_pnl_usd(&self) -> f64 {
        self.realized_pnl_usd
    }

    pub fn estimated_fees_usd(&self) -> f64 {
        self.estimated_fees_usd
    }

    pub fn resynced(&self) -> bool {
        self.resynced
    }

    /// Anchor cost basis on first live mid observation when session resumed with existing inventory.
    pub fn anchor_initial_if_unpriced(&mut self, mark_mid: Decimal) {
        if self.position != Decimal::ZERO
            && self.avg_cost == Decimal::ZERO
            && mark_mid > Decimal::ZERO
        {
            self.avg_cost = mark_mid;
        }
    }

    pub fn record_fill(
        &mut self,
        is_buy: bool,
        filled_qty: Decimal,
        avg_price: Decimal,
        fee_rate: f64,
    ) {
        if filled_qty <= Decimal::ZERO || avg_price <= Decimal::ZERO {
            return;
        }
        if fee_rate.is_finite()
            && fee_rate > 0.0
            && let Some(notional) = (filled_qty * avg_price).to_f64()
        {
            self.estimated_fees_usd += notional * fee_rate;
        }
        let sign = if is_buy { Decimal::ONE } else { -Decimal::ONE };
        if self.position == Decimal::ZERO || self.position.is_sign_positive() == is_buy {
            let new_abs = self.position.abs() + filled_qty;
            if self.avg_cost > Decimal::ZERO || self.position == Decimal::ZERO {
                self.avg_cost =
                    (self.position.abs() * self.avg_cost + filled_qty * avg_price) / new_abs;
            }
            self.position += sign * filled_qty;
            return;
        }
        let close_qty = filled_qty.min(self.position.abs());
        if self.avg_cost > Decimal::ZERO {
            let close_sign = if self.position > Decimal::ZERO {
                Decimal::ONE
            } else {
                -Decimal::ONE
            };
            let realized = close_qty * (avg_price - self.avg_cost) * close_sign;
            if let Some(r_f64) = realized.to_f64() {
                self.realized_pnl_usd += r_f64;
            }
        }
        self.position += sign * close_qty;
        let remainder = filled_qty - close_qty;
        if self.position == Decimal::ZERO {
            self.avg_cost = Decimal::ZERO;
        }
        if remainder > Decimal::ZERO {
            self.position = sign * remainder;
            self.avg_cost = avg_price;
        }
    }

    pub fn sync_position(&mut self, venue_pos: Decimal, mark_mid: Option<Decimal>) {
        if venue_pos == self.position {
            return;
        }
        self.resynced = true;
        if venue_pos == Decimal::ZERO {
            self.position = Decimal::ZERO;
            self.avg_cost = Decimal::ZERO;
            return;
        }
        let mark = mark_mid.filter(|m| *m > Decimal::ZERO);
        if self.position == Decimal::ZERO
            || self.position.is_sign_positive() != venue_pos.is_sign_positive()
        {
            self.position = venue_pos;
            self.avg_cost = mark.unwrap_or(Decimal::ZERO);
        } else if venue_pos.abs() > self.position.abs() {
            let extra = venue_pos.abs() - self.position.abs();
            if self.avg_cost > Decimal::ZERO
                && let Some(m) = mark
            {
                self.avg_cost = (self.position.abs() * self.avg_cost + extra * m) / venue_pos.abs();
            }
            self.position = venue_pos;
        } else {
            self.position = venue_pos;
        }
    }

    pub fn estimated_total_pnl_usd(&self, current_mid: f64) -> Option<f64> {
        let base = self.realized_pnl_usd - self.estimated_fees_usd;
        if self.position == Decimal::ZERO {
            return Some(base);
        }
        if !current_mid.is_finite() || current_mid <= 0.0 || self.avg_cost <= Decimal::ZERO {
            return None;
        }
        let pos = self.position.to_f64()?;
        let cost = self.avg_cost.to_f64()?;
        Some(base + pos * (current_mid - cost))
    }
}

/// Combine reported Hyperliquid equity delta with estimated Arcus fill/mid PnL for loss guarding.
pub fn combined_guard_equity_usd(
    allocated_capital_usd: f64,
    hl_equity_delta_usd: f64,
    arcus_estimated_pnl_usd: Option<f64>,
) -> f64 {
    let hl = if hl_equity_delta_usd.is_finite() {
        hl_equity_delta_usd
    } else {
        0.0
    };
    let arc = arcus_estimated_pnl_usd
        .filter(|v| v.is_finite())
        .unwrap_or(0.0);
    allocated_capital_usd + hl + arc
}

/// Conservatively align an IOC price bound inside the active Arcus tick grid.
pub fn quantize_arcus_limit_price(
    raw_price: Decimal,
    market: &ArcusMarket,
    is_buy: bool,
) -> Result<Decimal> {
    ensure!(raw_price > Decimal::ZERO, "price must be positive");
    let mut tick = market.tick_size;
    for tier in &market.tick_tiers {
        if tier.up_to_price.is_none_or(|upper| raw_price <= upper) {
            tick = tick.max(tier.tick);
            break;
        }
    }
    ensure!(tick > Decimal::ZERO, "Arcus tick size must be positive");
    let strategy = if is_buy {
        RoundingStrategy::ToNegativeInfinity
    } else {
        RoundingStrategy::ToPositiveInfinity
    };
    let ticks = (raw_price / tick).round_dp_with_strategy(0, strategy);
    ensure!(ticks > Decimal::ZERO, "quantized Arcus price non-positive");
    Ok((ticks * tick).normalize())
}

fn decimal_from_f64(value: f64) -> Option<Decimal> {
    if !value.is_finite() {
        return None;
    }
    Decimal::from_f64_retain(value)
}

fn protective_limit(touch_px: Decimal, is_buy: bool, slippage_bps: f64) -> Option<Decimal> {
    let slip = decimal_from_f64(slippage_bps / 10_000.0)?;
    let mult = if is_buy {
        Decimal::ONE + slip
    } else {
        Decimal::ONE - slip
    };
    let px = touch_px * mult;
    (px > Decimal::ZERO).then_some(px)
}

#[derive(Debug, Default)]
pub struct BasisEngineCounters {
    pub entries_emitted: u64,
    pub exits_emitted: u64,
    pub residual_hedges_emitted: u64,
    pub residual_blocked: u64,
    pub leung_li_blocked: u64,
    pub persist_reset_count: u64,
    pub brake_scaled_count: u64,
}

#[derive(Debug)]
pub struct ArcusXyzBasisEngine {
    config: ArcusXyzBasisConfig,
    constraints: VenueOrderConstraints,
    inv_arc: Decimal,
    inv_xyz: Decimal,
    in_flight: bool,
    residual_blocked_reason: Option<&'static str>,
    entry_open_ms: Option<u64>,
    last_trade_arc_rx_ms: u64,
    last_trade_xyz_rx_ms: u64,
    armed_dir: Option<i8>,
    armed_since_ms: u64,
    dust_qty: Decimal,
    pub counters: BasisEngineCounters,
}

impl ArcusXyzBasisEngine {
    pub fn new(config: ArcusXyzBasisConfig) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            config,
            constraints: VenueOrderConstraints::default(),
            inv_arc: Decimal::ZERO,
            inv_xyz: Decimal::ZERO,
            in_flight: false,
            residual_blocked_reason: None,
            entry_open_ms: None,
            last_trade_arc_rx_ms: 0,
            last_trade_xyz_rx_ms: 0,
            armed_dir: None,
            armed_since_ms: 0,
            dust_qty: Decimal::new(1, 6),
            counters: BasisEngineCounters::default(),
        })
    }

    pub fn set_constraints(&mut self, constraints: VenueOrderConstraints) {
        self.constraints = constraints;
    }

    pub fn constraints(&self) -> VenueOrderConstraints {
        self.constraints
    }

    pub fn residual_blocked(&self) -> Option<&'static str> {
        self.residual_blocked_reason
    }

    pub fn inv_arc(&self) -> Decimal {
        self.inv_arc
    }

    pub fn inv_xyz(&self) -> Decimal {
        self.inv_xyz
    }

    pub fn in_flight(&self) -> bool {
        self.in_flight
    }

    /// Reconcile venue positions cleanly after a dispatch settles or on startup.
    pub fn reconcile_positions(
        &mut self,
        inv_arc: Decimal,
        inv_xyz: Decimal,
        arc_rx_ms: u64,
        xyz_rx_ms: u64,
        now_ms: u64,
    ) {
        self.inv_arc = inv_arc;
        self.inv_xyz = inv_xyz;
        self.in_flight = false;
        if (self.inv_arc + self.inv_xyz).abs() <= self.dust_qty {
            self.residual_blocked_reason = None;
        }
        self.last_trade_arc_rx_ms = self.last_trade_arc_rx_ms.max(arc_rx_ms);
        self.last_trade_xyz_rx_ms = self.last_trade_xyz_rx_ms.max(xyz_rx_ms);
        if self.inv_arc.abs() <= self.dust_qty && self.inv_xyz.abs() <= self.dust_qty {
            self.entry_open_ms = None;
        } else if self.entry_open_ms.is_none() {
            self.entry_open_ms = Some(now_ms);
        }
    }

    fn reset_persist(&mut self) {
        if self.armed_dir.take().is_some() {
            self.armed_since_ms = 0;
            self.counters.persist_reset_count += 1;
        }
    }

    fn dual_refresh_ok(&self, arc_rx_ms: u64, xyz_rx_ms: u64) -> bool {
        if !self.config.post_trade_refresh_lock
            || (self.last_trade_arc_rx_ms == 0 && self.last_trade_xyz_rx_ms == 0)
        {
            return true;
        }
        arc_rx_ms > self.last_trade_arc_rx_ms && xyz_rx_ms > self.last_trade_xyz_rx_ms
    }

    fn ladder_add_penalty_bp(&self, abs_pos_usd: f64) -> f64 {
        if self.config.ladder_scale_bps <= 0.0 || self.config.max_notional_usd <= 0.0 {
            return 0.0;
        }
        let u = (abs_pos_usd / self.config.max_notional_usd).clamp(0.0, 1.0);
        if u <= self.config.ladder_floor_frac {
            return 0.0;
        }
        let frac =
            (u - self.config.ladder_floor_frac) / (1.0 - self.config.ladder_floor_frac).max(1e-9);
        self.config.ladder_scale_bps * frac.powf(self.config.ladder_convex_power)
    }

    /// Evaluate the latest cross-venue snapshots and emit at most one guarded action.
    pub fn on_books(
        &mut self,
        now_ms: u64,
        arc: &ArcusBookSnapshot,
        xyz: &ValidatedBook,
        xyz_rx_ms: u64,
        daily_loss: &DailyLossGuard,
        cancel_lease_ok: bool,
    ) -> BasisAction {
        if self.in_flight {
            return BasisAction::Hold {
                reason: "in_flight",
            };
        }
        if let Some(reason) = self.config.book_problem(now_ms, arc, xyz, xyz_rx_ms) {
            self.reset_persist();
            return BasisAction::Hold { reason };
        }

        // Priority 0: Hedge unhedged single-leg residual before any exit or new entry.
        let unhedged = self.inv_arc + self.inv_xyz;
        if unhedged.abs() > self.dust_qty {
            let is_buy = unhedged < Decimal::ZERO;
            let touch = if is_buy { xyz.bbo.ask } else { xyz.bbo.bid };
            let Some(limit_price) = protective_limit(touch, is_buy, self.config.slippage_bps)
            else {
                return BasisAction::Hold {
                    reason: "invalid_hedge_limit",
                };
            };
            let cons_px = touch.min(limit_price);
            let q_unhedged = quantize_hl_base_qty(unhedged.abs(), self.constraints.hl_sz_decimals)
                .unwrap_or(Decimal::ZERO);
            if q_unhedged <= Decimal::ZERO {
                self.reset_persist();
                self.residual_blocked_reason = Some("residual_below_hl_min_lot");
                self.counters.residual_blocked += 1;
                return BasisAction::Hold {
                    reason: "residual_below_hl_min_lot",
                };
            }
            let reduce_only_full =
                (is_buy && self.inv_xyz < Decimal::ZERO && self.inv_xyz.abs() >= q_unhedged)
                    || (!is_buy && self.inv_xyz > Decimal::ZERO && self.inv_xyz >= q_unhedged);
            let unhedged_notional = (q_unhedged * cons_px).to_f64().unwrap_or(0.0);
            if !reduce_only_full && unhedged_notional < self.constraints.hl_min_order_notional_usd {
                self.reset_persist();
                self.residual_blocked_reason = Some("residual_below_hl_min_notional");
                self.counters.residual_blocked += 1;
                return BasisAction::Hold {
                    reason: "residual_below_hl_min_notional",
                };
            }

            let visible = xyz.visible_size_within_limit(is_buy, limit_price);
            let raw_qty = unhedged.abs().min(visible).min(self.config.leg_capacity(
                touch.max(limit_price),
                self.inv_xyz,
                is_buy,
                reduce_only_full,
            ));
            let qty = if raw_qty > Decimal::ZERO {
                quantize_hl_base_qty(raw_qty, self.constraints.hl_sz_decimals)
                    .unwrap_or(Decimal::ZERO)
            } else {
                Decimal::ZERO
            };
            let reduce_only = (is_buy && self.inv_xyz < Decimal::ZERO && self.inv_xyz.abs() >= qty)
                || (!is_buy && self.inv_xyz > Decimal::ZERO && self.inv_xyz >= qty);
            let visible_notional = (qty * cons_px).to_f64().unwrap_or(0.0);
            if qty <= Decimal::ZERO
                || (!reduce_only && visible_notional < self.constraints.hl_min_order_notional_usd)
            {
                return BasisAction::Hold {
                    reason: "thin_xyz_hedge_book",
                };
            }
            self.residual_blocked_reason = None;
            self.reset_persist();
            self.in_flight = true;
            self.counters.residual_hedges_emitted += 1;
            return BasisAction::HedgeResidualXyz {
                is_buy,
                qty,
                limit_price,
                reduce_only,
            };
        }

        let Some(ab) = arc.bid_px.to_f64() else {
            return BasisAction::Hold {
                reason: "arc_px_overflow",
            };
        };
        let Some(aa) = arc.ask_px.to_f64() else {
            return BasisAction::Hold {
                reason: "arc_px_overflow",
            };
        };
        let Some(xb) = xyz.bbo.bid.to_f64() else {
            return BasisAction::Hold {
                reason: "xyz_px_overflow",
            };
        };
        let Some(xa) = xyz.bbo.ask.to_f64() else {
            return BasisAction::Hold {
                reason: "xyz_px_overflow",
            };
        };
        let am = 0.5 * (ab + aa);
        if am <= 0.0 || aa <= 0.0 || xa <= 0.0 {
            return BasisAction::Hold {
                reason: "invalid_prices",
            };
        }
        // Raw executable basis edges (bps): +1 buy Arcus / sell xyz; -1 sell Arcus / buy xyz.
        let edge_pos_bp = (xb / aa - 1.0) * 1e4;
        let edge_neg_bp = (ab / xa - 1.0) * 1e4;
        let refresh_ok = self.dual_refresh_ok(arc.receipt_ms, xyz_rx_ms);

        // Priority 1: Check exit while carrying paired delta-neutral inventory.
        if self.inv_arc.abs() > self.dust_qty {
            let pos_sign: i8 = if self.inv_arc > Decimal::ZERO { 1 } else { -1 };
            let hold_ms = now_ms.saturating_sub(self.entry_open_ms.unwrap_or(now_ms));
            let timed_out = hold_ms >= self.config.max_hold_ms;
            let close_edge_bp = if pos_sign > 0 {
                edge_neg_bp
            } else {
                edge_pos_bp
            };
            if timed_out || (close_edge_bp >= self.config.exit_bp && refresh_ok) {
                let direction_arcus_buy = pos_sign < 0;
                let arc_touch = if direction_arcus_buy {
                    arc.ask_px
                } else {
                    arc.bid_px
                };
                let xyz_touch = if direction_arcus_buy {
                    xyz.bbo.bid
                } else {
                    xyz.bbo.ask
                };
                let Some(arcus_limit_px) =
                    protective_limit(arc_touch, direction_arcus_buy, self.config.slippage_bps)
                else {
                    return BasisAction::Hold {
                        reason: "invalid_exit_limit",
                    };
                };
                let Some(xyz_limit_px) =
                    protective_limit(xyz_touch, !direction_arcus_buy, self.config.slippage_bps)
                else {
                    return BasisAction::Hold {
                        reason: "invalid_exit_limit",
                    };
                };
                let arc_vis = if direction_arcus_buy {
                    arc.ask_sz
                } else {
                    arc.bid_sz
                };
                let xyz_vis = xyz.visible_size_within_limit(!direction_arcus_buy, xyz_limit_px);
                let raw_close = self
                    .inv_arc
                    .abs()
                    .min(arc_vis)
                    .min(xyz_vis)
                    .min(self.config.leg_capacity(
                        arc_touch.max(arcus_limit_px),
                        self.inv_arc,
                        direction_arcus_buy,
                        true,
                    ))
                    .min(self.config.leg_capacity(
                        xyz_touch.max(xyz_limit_px),
                        self.inv_xyz,
                        !direction_arcus_buy,
                        true,
                    ));
                let max_close = if raw_close > Decimal::ZERO {
                    quantize_shared_base_qty(
                        raw_close,
                        self.constraints.arcus_step_size,
                        self.constraints.hl_sz_decimals,
                    )
                    .unwrap_or(Decimal::ZERO)
                } else {
                    Decimal::ZERO
                };
                if max_close <= self.dust_qty || max_close < self.constraints.arcus_min_order_size {
                    return BasisAction::Hold {
                        reason: "thin_exit_book",
                    };
                }
                self.reset_persist();
                self.in_flight = true;
                self.counters.exits_emitted += 1;
                return BasisAction::ExitPairIoc {
                    direction_arcus_buy,
                    qty: max_close,
                    arcus_limit_px,
                    xyz_limit_px,
                    timed_out,
                };
            }
        }

        // Priority 2: Evaluate opening or adding-side scaling under strict risk gates.
        let brake_scale = daily_loss.risk_scale();
        if brake_scale <= 0.0 {
            self.reset_persist();
            return BasisAction::Hold {
                reason: "daily_loss_halted",
            };
        }
        if self.config.require_arcus_cancel_lease && !cancel_lease_ok {
            self.reset_persist();
            return BasisAction::Hold {
                reason: "arcus_cancel_lease_required",
            };
        }
        if !refresh_ok {
            return BasisAction::Hold {
                reason: "awaiting_post_trade_refresh",
            };
        }

        let min_entry_usd = self.constraints.min_entry_notional_usd();
        let pos_usd = self.inv_arc.abs().to_f64().unwrap_or(f64::MAX) * am;
        let remaining_usd = self.config.max_notional_usd - pos_usd;
        if remaining_usd < min_entry_usd {
            self.reset_persist();
            return BasisAction::Hold {
                reason: "max_notional_reached",
            };
        }

        let allowed_dirs: [i8; 2] = if self.inv_arc.abs() > self.dust_qty {
            let s = if self.inv_arc > Decimal::ZERO { 1 } else { -1 };
            [s, 0]
        } else {
            [1, -1]
        };

        let penalty_bp = self.ladder_add_penalty_bp(pos_usd);
        let req_edge_bp = self.config.entry_bp + penalty_bp;
        let mut best_dir = 0_i8;
        let mut best_raw_bp = f64::NEG_INFINITY;
        let mut leung_li_hit = false;

        for &dir in &allowed_dirs {
            if dir == 0 {
                continue;
            }
            let raw_bp = if dir > 0 { edge_pos_bp } else { edge_neg_bp };
            if raw_bp > self.config.max_entry_bp {
                leung_li_hit = true;
                continue;
            }
            if raw_bp >= req_edge_bp && raw_bp > best_raw_bp {
                best_raw_bp = raw_bp;
                best_dir = dir;
            }
        }

        if best_dir == 0 {
            if leung_li_hit {
                self.counters.leung_li_blocked += 1;
            }
            self.reset_persist();
            return BasisAction::Hold {
                reason: if leung_li_hit {
                    "leung_li_structural_cap"
                } else {
                    "below_entry_threshold"
                },
            };
        }

        if self.config.persist_ms > 0 {
            if self.armed_dir != Some(best_dir) {
                if self.armed_dir.is_some() {
                    self.counters.persist_reset_count += 1;
                }
                self.armed_dir = Some(best_dir);
                self.armed_since_ms = now_ms;
                return BasisAction::Hold {
                    reason: "persistence_arming",
                };
            }
            if now_ms.saturating_sub(self.armed_since_ms) < self.config.persist_ms {
                return BasisAction::Hold {
                    reason: "persistence_waiting",
                };
            }
        }

        let eff_clip_usd = (self.config.clip_usd * brake_scale).min(remaining_usd);
        if eff_clip_usd < min_entry_usd {
            self.reset_persist();
            return BasisAction::Hold {
                reason: "braked_below_min_notional",
            };
        }
        let direction_arcus_buy = best_dir > 0;
        let arc_touch = if direction_arcus_buy {
            arc.ask_px
        } else {
            arc.bid_px
        };
        let xyz_touch = if direction_arcus_buy {
            xyz.bbo.bid
        } else {
            xyz.bbo.ask
        };
        let Some(arcus_limit_px) =
            protective_limit(arc_touch, direction_arcus_buy, self.config.slippage_bps)
        else {
            return BasisAction::Hold {
                reason: "invalid_entry_limit",
            };
        };
        let Some(xyz_limit_px) =
            protective_limit(xyz_touch, !direction_arcus_buy, self.config.slippage_bps)
        else {
            return BasisAction::Hold {
                reason: "invalid_entry_limit",
            };
        };
        let arc_vis = if direction_arcus_buy {
            arc.ask_sz
        } else {
            arc.bid_sz
        };
        let xyz_vis = xyz.visible_size_within_limit(!direction_arcus_buy, xyz_limit_px);
        let arc_bound = arc_touch.max(arcus_limit_px);
        let xyz_bound = xyz_touch.max(xyz_limit_px);
        let clip = decimal_from_f64(eff_clip_usd).unwrap_or_default() / arc_bound.max(xyz_bound);
        let raw_capped_qty = clip
            .min(arc_vis)
            .min(xyz_vis)
            .min(
                self.config
                    .leg_capacity(arc_bound, self.inv_arc, direction_arcus_buy, false),
            )
            .min(
                self.config
                    .leg_capacity(xyz_bound, self.inv_xyz, !direction_arcus_buy, false),
            );
        let qty = if raw_capped_qty > Decimal::ZERO {
            quantize_shared_base_qty(
                raw_capped_qty,
                self.constraints.arcus_step_size,
                self.constraints.hl_sz_decimals,
            )
            .unwrap_or(Decimal::ZERO)
        } else {
            Decimal::ZERO
        };
        if qty <= self.dust_qty || qty < self.constraints.arcus_min_order_size {
            self.reset_persist();
            return BasisAction::Hold {
                reason: "thin_entry_book",
            };
        }
        let arc_notional_usd = (qty * arc_touch.min(arcus_limit_px))
            .to_f64()
            .unwrap_or(0.0);
        let hl_notional_usd = (qty * xyz_touch.min(xyz_limit_px)).to_f64().unwrap_or(0.0);
        if arc_notional_usd < self.constraints.arcus_min_order_notional_usd
            || hl_notional_usd < self.constraints.hl_min_order_notional_usd
        {
            self.reset_persist();
            return BasisAction::Hold {
                reason: "entry_below_venue_min",
            };
        }

        self.armed_dir = None;
        self.armed_since_ms = 0;
        self.in_flight = true;
        self.counters.entries_emitted += 1;
        if brake_scale < 1.0 - 1e-9 {
            self.counters.brake_scaled_count += 1;
        }
        BasisAction::EnterPairIoc {
            direction_arcus_buy,
            qty,
            arcus_limit_px,
            xyz_limit_px,
            raw_edge_bp: best_raw_bp,
            req_edge_bp,
            brake_scale,
        }
    }
}

const ARCUS_CLOCK_SKEW_US: u64 = 5_000_000;

fn arcus_sides_match(a: ArcusSide, b: ArcusSide) -> bool {
    matches!(
        (a, b),
        (ArcusSide::Buy, ArcusSide::Buy) | (ArcusSide::Sell, ArcusSide::Sell)
    )
}

/// Strict Hyperliquid HIP-3/Core position parser: fails closed on stale timestamp,
/// duplicate coin entries, or malformed `szi` rather than defaulting to zero.
pub fn parse_hl_market_position(state: &HlAccountState, coin: &str) -> Result<Decimal> {
    ensure!(
        state.time > 0,
        "Hyperliquid clearinghouse state missing positive watermark time for {coin}"
    );
    let mut found: Option<Decimal> = None;
    for entry in &state.asset_positions {
        if entry.position.coin == coin {
            let size = Decimal::from_str_exact(entry.position.szi.trim()).with_context(|| {
                format!(
                    "malformed Hyperliquid position size {:?} for {coin}",
                    entry.position.szi
                )
            })?;
            ensure!(
                found.replace(size).is_none(),
                "duplicate Hyperliquid position entry for {coin}"
            );
        }
    }
    Ok(found.unwrap_or(Decimal::ZERO))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HlSubmitOutcome {
    FilledAck {
        order_id: u64,
        total_size: Decimal,
        average_price: Decimal,
    },
    CanceledAck,
    DefinitiveRejected {
        reason: String,
    },
    UnexpectedResting {
        order_id: u64,
    },
    Ambiguous {
        reason: String,
    },
}

pub fn classify_hl_submit(
    res: &Result<Vec<HlExecutionStatus>>,
    planned_qty: Decimal,
) -> HlSubmitOutcome {
    match res {
        Ok(statuses) => match statuses.as_slice() {
            [
                HlExecutionStatus::Filled {
                    order_id,
                    total_size,
                    average_price,
                },
            ] => {
                if *total_size > Decimal::ZERO
                    && *total_size <= planned_qty
                    && *average_price > Decimal::ZERO
                {
                    HlSubmitOutcome::FilledAck {
                        order_id: *order_id,
                        total_size: *total_size,
                        average_price: *average_price,
                    }
                } else {
                    HlSubmitOutcome::Ambiguous {
                        reason: format!(
                            "invalid filled ack size={total_size} avg_px={average_price} planned={planned_qty}"
                        ),
                    }
                }
            }
            [HlExecutionStatus::Canceled] => HlSubmitOutcome::CanceledAck,
            [HlExecutionStatus::Rejected { reason }] => HlSubmitOutcome::DefinitiveRejected {
                reason: reason.clone(),
            },
            [HlExecutionStatus::Resting { order_id }] => HlSubmitOutcome::UnexpectedResting {
                order_id: *order_id,
            },
            [] => HlSubmitOutcome::Ambiguous {
                reason: "empty Hyperliquid execution status list".into(),
            },
            many => HlSubmitOutcome::Ambiguous {
                reason: format!("unexpected {} Hyperliquid execution statuses", many.len()),
            },
        },
        Err(err) => {
            if let Some(HlSubmissionError::Rejected(reason)) =
                err.downcast_ref::<HlSubmissionError>()
            {
                HlSubmitOutcome::DefinitiveRejected {
                    reason: reason.clone(),
                }
            } else {
                HlSubmitOutcome::Ambiguous {
                    reason: err.to_string(),
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct HlLegIntent {
    pub cloid: String,
    pub coin: String,
    pub is_buy: bool,
    pub qty: Decimal,
    pub limit_price: Decimal,
    pub sent_at_ms: u64,
    pub prev_inv_xyz: Decimal,
}

#[derive(Debug, Clone)]
pub struct ArcusLegIntent {
    pub market_id: u16,
    pub client_id: String,
    pub side: ArcusSide,
    pub qty: Decimal,
    pub limit_price: Decimal,
    pub sent_at_us: u64,
    pub prev_inv_arc: Decimal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArcusSubmitOutcome {
    GatewayAccepted {
        http_status: u16,
        order_id: Option<String>,
    },
    GatewayRejected {
        order_id: Option<String>,
        reason: String,
    },
    HardRejected {
        reason: String,
    },
    Ambiguous {
        reason: String,
    },
}

pub fn classify_arcus_submit(
    res: &Result<ArcusOrderReceipt>,
    expected_client_id: &str,
) -> ArcusSubmitOutcome {
    match res {
        Ok(receipt) => {
            if let Some(cid) = receipt.client_id.as_deref()
                && cid != expected_client_id
            {
                return ArcusSubmitOutcome::Ambiguous {
                    reason: format!(
                        "Arcus receipt client_id {cid:?} mismatches expected {expected_client_id:?}"
                    ),
                };
            }
            if receipt.rejection_reason.is_some() || receipt.status.eq_ignore_ascii_case("rejected")
            {
                ArcusSubmitOutcome::GatewayRejected {
                    order_id: receipt.order_id.clone(),
                    reason: receipt
                        .rejection_reason
                        .clone()
                        .unwrap_or_else(|| receipt.status.clone()),
                }
            } else {
                ArcusSubmitOutcome::GatewayAccepted {
                    http_status: receipt.http_status,
                    order_id: receipt.order_id.clone(),
                }
            }
        }
        Err(err) => {
            if let Some(ArcusExecutionError::Rejected {
                status, message, ..
            }) = err.downcast_ref::<ArcusExecutionError>()
            {
                ArcusSubmitOutcome::HardRejected {
                    reason: format!("HTTP {status}: {message}"),
                }
            } else {
                ArcusSubmitOutcome::Ambiguous {
                    reason: err.to_string(),
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedLegTerminal {
    pub status: String,
    pub filled_qty: Decimal,
    pub avg_price: Option<Decimal>,
    pub terminal_at_ms: Option<u64>,
    pub account_time_ms: Option<u64>,
}

/// Authoritative Hyperliquid leg verifier: requires client ID status proof and
/// clearinghouse watermark coverage (`hl_account_time_ms >= terminal_at_ms`),
/// never trusting bare `Resting` or `submit_err` position diffs alone.
pub fn verify_hl_leg_terminal(
    intent: &HlLegIntent,
    submit: &HlSubmitOutcome,
    lookup: &HlOrderLookup,
    open_orders: &[HlOpenOrder],
    inv_xyz: Decimal,
    hl_account_time_ms: u64,
) -> Result<Option<VerifiedLegTerminal>> {
    if let HlSubmitOutcome::UnexpectedResting { order_id } = submit {
        bail!(
            "hyperliquid IOC cloid {} unexpectedly rested as order {order_id}",
            intent.cloid
        );
    }
    let expected_oid = match submit {
        HlSubmitOutcome::FilledAck { order_id, .. } => Some(*order_id),
        _ => None,
    };
    let lookup_oid = match lookup {
        HlOrderLookup::Found(located) => Some(located.order.oid),
        HlOrderLookup::Unknown => None,
    };
    if open_orders.iter().any(|o| {
        o.cloid.as_deref() == Some(intent.cloid.as_str())
            || expected_oid == Some(o.oid)
            || lookup_oid == Some(o.oid)
    }) {
        bail!(
            "hyperliquid IOC cloid {} is still open in open_orders",
            intent.cloid
        );
    }
    match lookup {
        HlOrderLookup::Found(located) => {
            let outcome = classify_lookup(
                &IocIdentity {
                    cloid: &intent.cloid,
                    coin: &intent.coin,
                    is_buy: intent.is_buy,
                    size: intent.qty,
                    limit_price: intent.limit_price,
                    sent_at_ms: intent.sent_at_ms,
                },
                located,
            )?;
            let (at_ms, filled) = match outcome {
                LookupOutcome::Open { oid } => {
                    bail!(
                        "hyperliquid IOC cloid {} returned non-terminal open status (oid={oid})",
                        intent.cloid
                    );
                }
                LookupOutcome::Terminal { at_ms, filled } => (at_ms, filled),
            };
            let avg_price = match submit {
                HlSubmitOutcome::FilledAck {
                    order_id,
                    total_size,
                    average_price,
                } => {
                    ensure!(
                        *order_id == located.order.oid,
                        "Hyperliquid filled acknowledgement order ID differs from terminal lookup"
                    );
                    ensure!(
                        filled == *total_size,
                        "hyperliquid cloid {} lookup fill {filled} disagrees with filled ack {total_size}",
                        intent.cloid
                    );
                    ensure!(
                        *average_price > Decimal::ZERO,
                        "hyperliquid cloid {} filled ack has non-positive avg_price {average_price}",
                        intent.cloid
                    );
                    let within_limit = if intent.is_buy {
                        *average_price <= intent.limit_price
                    } else {
                        *average_price >= intent.limit_price
                    };
                    ensure!(
                        within_limit,
                        "hyperliquid cloid {} filled ack avg_price {average_price} exceeded journaled IOC limit {}",
                        intent.cloid,
                        intent.limit_price
                    );
                    Some(*average_price)
                }
                HlSubmitOutcome::CanceledAck | HlSubmitOutcome::DefinitiveRejected { .. } => {
                    ensure!(
                        filled == Decimal::ZERO,
                        "hyperliquid cloid {} rejected/canceled ack conflicts with lookup fill {filled}",
                        intent.cloid
                    );
                    None
                }
                HlSubmitOutcome::Ambiguous { .. } => None,
                HlSubmitOutcome::UnexpectedResting { .. } => unreachable!(),
            };
            let Some(confirmed_fill) = account_confirmed_fill(
                intent.is_buy,
                intent.prev_inv_xyz,
                intent.qty,
                at_ms,
                filled,
                inv_xyz,
                hl_account_time_ms,
            )?
            else {
                return Ok(None);
            };
            Ok(Some(VerifiedLegTerminal {
                status: located.status.clone(),
                filled_qty: confirmed_fill,
                avg_price,
                terminal_at_ms: Some(at_ms),
                account_time_ms: Some(hl_account_time_ms),
            }))
        }
        HlOrderLookup::Unknown => match submit {
            HlSubmitOutcome::DefinitiveRejected { .. } => {
                if hl_account_time_ms < intent.sent_at_ms.saturating_sub(LOOKUP_SKEW_MS) {
                    return Ok(None);
                }
                ensure!(
                    inv_xyz == intent.prev_inv_xyz,
                    "hyperliquid definitive reject for {} has unexpected position shift ({} -> {inv_xyz})",
                    intent.cloid,
                    intent.prev_inv_xyz
                );
                Ok(Some(VerifiedLegTerminal {
                    status: "rejected".into(),
                    filled_qty: Decimal::ZERO,
                    avg_price: None,
                    terminal_at_ms: Some(intent.sent_at_ms),
                    account_time_ms: Some(hl_account_time_ms),
                }))
            }
            HlSubmitOutcome::FilledAck { .. }
            | HlSubmitOutcome::CanceledAck
            | HlSubmitOutcome::Ambiguous { .. } => Ok(None),
            HlSubmitOutcome::UnexpectedResting { .. } => unreachable!(),
        },
    }
}

fn is_arcus_order_open_status(norm_status: &str) -> bool {
    matches!(
        norm_status,
        "open" | "new" | "pending" | "accepted" | "partially_filled"
    )
}

enum ArcusOrderMatch<'a> {
    None,
    PendingPropagation,
    Matched(&'a ArcusOrder),
}

fn validate_matching_arcus_order_pair(
    intent: &ArcusLegIntent,
    prev: &ArcusOrder,
    hist: &ArcusOrder,
) -> Result<bool> {
    ensure!(
        prev.order_id == hist.order_id,
        "conflicting Arcus order_ids ({} vs {}) for client_id {}",
        prev.order_id,
        hist.order_id,
        intent.client_id
    );
    ensure!(
        arcus_sides_match(prev.side, hist.side)
            && prev.price == hist.price
            && prev.original_size == hist.original_size,
        "conflicting Arcus order attributes for {}",
        prev.order_id
    );
    let prev_status = prev.status.trim().to_ascii_lowercase();
    let hist_status = hist.status.trim().to_ascii_lowercase();
    if is_arcus_order_open_status(&prev_status) || is_arcus_order_open_status(&hist_status) {
        return Ok(true);
    }
    ensure!(
        prev_status == hist_status && prev.remaining_size == hist.remaining_size,
        "conflicting terminal Arcus order state for {}",
        prev.order_id
    );
    Ok(false)
}

fn arcus_order_matches_intent<'a>(
    intent: &ArcusLegIntent,
    expected_oid: Option<&str>,
    direct_order: Option<&'a ArcusOrder>,
    order_history: &'a [ArcusOrder],
) -> Result<ArcusOrderMatch<'a>> {
    let mut chosen: Option<&ArcusOrder> = None;
    let mut pending_propagation = false;
    if let Some(direct) = direct_order {
        ensure!(
            expected_oid.is_none_or(|eid| direct.order_id == eid)
                && direct.market_id == intent.market_id
                && direct
                    .client_id
                    .as_deref()
                    .is_none_or(|cid| cid == intent.client_id),
            "Arcus direct order {} disagrees with journaled intent",
            direct.order_id
        );
        chosen = Some(direct);
    }
    for hist in order_history {
        let matches = hist.client_id.as_deref() == Some(intent.client_id.as_str())
            || expected_oid.is_some_and(|eid| hist.order_id == eid)
            || chosen.is_some_and(|prev| hist.order_id == prev.order_id);
        if !matches {
            continue;
        }
        ensure!(
            hist.market_id == intent.market_id
                && hist
                    .client_id
                    .as_deref()
                    .is_none_or(|cid| cid == intent.client_id)
                && expected_oid.is_none_or(|eid| hist.order_id == eid),
            "Arcus history order {} disagrees with journaled intent",
            hist.order_id
        );
        if let Some(prev) = chosen {
            if validate_matching_arcus_order_pair(intent, prev, hist)? {
                pending_propagation = true;
            }
        } else {
            chosen = Some(hist);
        }
    }
    if pending_propagation {
        Ok(ArcusOrderMatch::PendingPropagation)
    } else if let Some(order) = chosen {
        Ok(ArcusOrderMatch::Matched(order))
    } else {
        Ok(ArcusOrderMatch::None)
    }
}

/// Authoritative Arcus leg verifier: HTTP 202 `GatewayAccepted` alone is never terminal.
/// Requires open-orders exclusion, terminal `Order` identity match, deduped fills parity,
/// and exact `inv_arc` inventory parity.
pub fn verify_arcus_leg_terminal(
    intent: &ArcusLegIntent,
    submit: &ArcusSubmitOutcome,
    direct_order: Option<&ArcusOrder>,
    open_orders: &[ArcusOrder],
    order_history: &[ArcusOrder],
    fills: &[ArcusFill],
    inv_arc: Decimal,
) -> Result<Option<VerifiedLegTerminal>> {
    let expected_oid = match submit {
        ArcusSubmitOutcome::GatewayAccepted { order_id, .. }
        | ArcusSubmitOutcome::GatewayRejected { order_id, .. } => order_id.as_deref(),
        ArcusSubmitOutcome::HardRejected { .. } | ArcusSubmitOutcome::Ambiguous { .. } => None,
    };
    let order_match =
        arcus_order_matches_intent(intent, expected_oid, direct_order, order_history)?;
    let known_oid = match &order_match {
        ArcusOrderMatch::Matched(order) => Some(order.order_id.as_str()),
        _ => expected_oid,
    };

    for o in open_orders {
        let matches = o.client_id.as_deref() == Some(intent.client_id.as_str())
            || known_oid.is_some_and(|id| o.order_id == id);
        if matches {
            ensure!(
                o.market_id == intent.market_id
                    && o.client_id
                        .as_deref()
                        .is_none_or(|cid| cid == intent.client_id)
                    && known_oid.is_none_or(|id| o.order_id == id),
                "Arcus open order {} disagrees with journaled intent",
                o.order_id
            );
            return Ok(None);
        }
    }

    let order = match order_match {
        ArcusOrderMatch::Matched(order) => order,
        ArcusOrderMatch::PendingPropagation => return Ok(None),
        ArcusOrderMatch::None => {
            return match submit {
                ArcusSubmitOutcome::HardRejected { .. }
                | ArcusSubmitOutcome::GatewayRejected { .. } => {
                    let any_fill = fills.iter().any(|f| {
                        f.client_id.as_deref() == Some(intent.client_id.as_str())
                            || expected_oid.is_some_and(|eid| f.order_id == eid)
                    });
                    ensure!(
                        !any_fill,
                        "Arcus rejected intent {} unexpectedly matched a fill",
                        intent.client_id
                    );
                    ensure!(
                        inv_arc == intent.prev_inv_arc,
                        "Arcus rejected intent {} shifted inventory ({} -> {inv_arc})",
                        intent.client_id,
                        intent.prev_inv_arc
                    );
                    Ok(Some(VerifiedLegTerminal {
                        status: "rejected".into(),
                        filled_qty: Decimal::ZERO,
                        avg_price: None,
                        terminal_at_ms: Some(intent.sent_at_us / 1_000),
                        account_time_ms: None,
                    }))
                }
                ArcusSubmitOutcome::GatewayAccepted { .. }
                | ArcusSubmitOutcome::Ambiguous { .. } => Ok(None),
            };
        }
    };

    ensure!(
        order.market_id == intent.market_id && arcus_sides_match(order.side, intent.side),
        "Arcus order {} disagrees with journaled market/side",
        order.order_id
    );
    ensure!(
        order.price == intent.limit_price && order.original_size == intent.qty,
        "Arcus order {} disagrees with journaled price/size",
        order.order_id
    );
    if let Some(cid) = order.client_id.as_deref() {
        ensure!(
            cid == intent.client_id,
            "Arcus order {} client_id {cid:?} disagrees with {}",
            order.order_id,
            intent.client_id
        );
    }
    if let Some(eid) = expected_oid {
        ensure!(
            order.order_id == eid,
            "Arcus order_id {} disagrees with receipt {eid}",
            order.order_id
        );
    }
    ensure!(
        order.updated_at >= intent.sent_at_us.saturating_sub(ARCUS_CLOCK_SKEW_US),
        "Arcus order {} updated_at {} predates intent {}",
        order.order_id,
        order.updated_at,
        intent.sent_at_us
    );
    ensure!(
        order.remaining_size >= Decimal::ZERO && order.remaining_size <= order.original_size,
        "Arcus order {} has invalid remaining_size {}",
        order.order_id,
        order.remaining_size
    );

    let norm_status = order.status.trim().to_ascii_lowercase();
    let expected_filled = match norm_status.as_str() {
        "open" | "new" | "pending" | "accepted" | "partially_filled" => return Ok(None),
        "filled" => {
            ensure!(
                order.remaining_size == Decimal::ZERO,
                "Arcus filled order {} still has remaining_size {}",
                order.order_id,
                order.remaining_size
            );
            order.original_size
        }
        "canceled" | "cancelled" | "expired" | "ioc_canceled" => {
            order.original_size - order.remaining_size
        }
        "rejected" => {
            ensure!(
                order.remaining_size == order.original_size,
                "Arcus rejected order {} has partial remaining_size {}",
                order.order_id,
                order.remaining_size
            );
            Decimal::ZERO
        }
        other => bail!(
            "unrecognized Arcus order {} status {other:?}",
            order.order_id
        ),
    };

    let mut seen_trade_ids: HashMap<&str, (Decimal, Decimal)> = HashMap::new();
    let mut fills_qty = Decimal::ZERO;
    let mut fills_notional = Decimal::ZERO;
    for fill in fills {
        let matches = fill.order_id == order.order_id
            || fill.client_id.as_deref() == Some(intent.client_id.as_str());
        if !matches {
            continue;
        }
        ensure!(
            !fill.trade_id.trim().is_empty(),
            "Arcus fill for order {} has empty trade_id",
            order.order_id
        );
        ensure!(
            fill.market_id == intent.market_id,
            "Arcus fill {} disagrees with market_id",
            fill.trade_id
        );
        ensure!(
            fill.order_id == order.order_id && arcus_sides_match(fill.side, intent.side),
            "Arcus fill {} disagrees with order/side",
            fill.trade_id
        );
        ensure!(
            fill.client_id
                .as_deref()
                .is_none_or(|client_id| client_id == intent.client_id),
            "Arcus fill client ID differs from the journaled intent"
        );
        ensure!(
            fill.size > Decimal::ZERO && fill.price > Decimal::ZERO,
            "Arcus fill {} has non-positive size/price",
            fill.trade_id
        );
        let within_limit = match intent.side {
            ArcusSide::Buy => fill.price <= intent.limit_price,
            ArcusSide::Sell => fill.price >= intent.limit_price,
        };
        ensure!(
            within_limit,
            "Arcus fill {} exceeded the journaled IOC limit",
            fill.trade_id
        );
        if let Some(&(prev_size, prev_price)) = seen_trade_ids.get(fill.trade_id.as_str()) {
            ensure!(
                prev_size == fill.size && prev_price == fill.price,
                "Arcus duplicate trade_id {} disagrees on size/price ({prev_size}@{prev_price} vs {}@{})",
                fill.trade_id,
                fill.size,
                fill.price
            );
            continue;
        }
        seen_trade_ids.insert(fill.trade_id.as_str(), (fill.size, fill.price));
        fills_qty += fill.size;
        fills_notional += fill.size * fill.price;
    }

    ensure!(
        fills_qty <= expected_filled,
        "Arcus fills sum {fills_qty} exceeds order {} filled {expected_filled}",
        order.order_id
    );
    if fills_qty < expected_filled {
        return Ok(None);
    }

    let pos_delta = match intent.side {
        ArcusSide::Buy => inv_arc - intent.prev_inv_arc,
        ArcusSide::Sell => intent.prev_inv_arc - inv_arc,
    };
    if pos_delta != expected_filled {
        return Ok(None);
    }

    let avg_price = if fills_qty > Decimal::ZERO {
        Some(fills_notional / fills_qty)
    } else {
        None
    };
    Ok(Some(VerifiedLegTerminal {
        status: norm_status,
        filled_qty: expected_filled,
        avg_price,
        terminal_at_ms: Some(order.updated_at / 1_000),
        account_time_ms: None,
    }))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShutdownDisposition {
    Clean,
    ResidualPosition {
        inv_arc: Decimal,
        inv_xyz: Decimal,
    },
    OpenOrdersRemaining {
        open_orders_arcus: usize,
        open_orders_hl: usize,
        inv_arc: Option<Decimal>,
        inv_xyz: Option<Decimal>,
    },
    CancelUnknown {
        cancel_ok: bool,
        open_orders_arcus: Option<usize>,
        open_orders_hl: Option<usize>,
        inv_arc: Option<Decimal>,
        inv_xyz: Option<Decimal>,
        detail: String,
    },
}

pub fn classify_shutdown(
    cancel_ok: bool,
    open_orders: Result<(usize, usize)>,
    reconciled: Result<(Decimal, Decimal)>,
) -> ShutdownDisposition {
    let (open_res, open_err) = match open_orders {
        Ok(counts) => (Some(counts), None),
        Err(err) => (None, Some(err.to_string())),
    };
    let (pos_res, pos_err) = match reconciled {
        Ok(pos) => (Some(pos), None),
        Err(err) => (None, Some(err.to_string())),
    };
    if !cancel_ok || open_res.is_none() || pos_res.is_none() {
        let mut details = Vec::new();
        if !cancel_ok {
            details.push("arcus_cancel_all_failed".to_owned());
        }
        if let Some(err) = open_err {
            details.push(format!("open_orders_err={err}"));
        }
        if let Some(err) = pos_err {
            details.push(format!("reconcile_err={err}"));
        }
        return ShutdownDisposition::CancelUnknown {
            cancel_ok,
            open_orders_arcus: open_res.map(|c| c.0),
            open_orders_hl: open_res.map(|c| c.1),
            inv_arc: pos_res.map(|p| p.0),
            inv_xyz: pos_res.map(|p| p.1),
            detail: details.join("; "),
        };
    }
    let (open_arc, open_hl) = open_res.unwrap();
    let (inv_a, inv_x) = pos_res.unwrap();
    if open_arc > 0 || open_hl > 0 {
        return ShutdownDisposition::OpenOrdersRemaining {
            open_orders_arcus: open_arc,
            open_orders_hl: open_hl,
            inv_arc: Some(inv_a),
            inv_xyz: Some(inv_x),
        };
    }
    if inv_a != Decimal::ZERO || inv_x != Decimal::ZERO {
        return ShutdownDisposition::ResidualPosition {
            inv_arc: inv_a,
            inv_xyz: inv_x,
        };
    }
    ShutdownDisposition::Clean
}

mod opt_decimal_str {
    use rust_decimal::Decimal;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(
        value: &Option<Decimal>,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match value {
            Some(dec) => serializer.serialize_some(&dec.to_string()),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> std::result::Result<Option<Decimal>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let opt = Option::<String>::deserialize(deserializer)?;
        opt.map(|s| Decimal::from_str_exact(s.trim()).map_err(serde::de::Error::custom))
            .transpose()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum BasisJournalEvent {
    SessionStart {
        network: String,
        arcus_symbol: String,
        hl_coin: String,
        dry_run: bool,
        journal_ms: u64,
    },
    IntentOpen {
        intent_id: String,
        kind: String,
        arcus_client_id: Option<String>,
        hl_cloid: Option<String>,
        #[serde(with = "rust_decimal::serde::str")]
        qty: Decimal,
        journal_ms: u64,
    },
    IntentNotSent {
        intent_id: String,
        journal_ms: u64,
    },
    IntentTerminal {
        intent_id: String,
        status: String,
        #[serde(with = "rust_decimal::serde::str")]
        inv_arc: Decimal,
        #[serde(with = "rust_decimal::serde::str")]
        inv_xyz: Decimal,
        journal_ms: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        arcus_status: Option<String>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            with = "opt_decimal_str"
        )]
        arcus_filled_qty: Option<Decimal>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hl_status: Option<String>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            with = "opt_decimal_str"
        )]
        hl_filled_qty: Option<Decimal>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hl_terminal_at_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hl_account_time_ms: Option<u64>,
    },
    CleanShutdown {
        journal_ms: u64,
    },
    ShutdownUnverified {
        reason: String,
        cancel_ok: bool,
        open_orders_arcus: Option<usize>,
        open_orders_hl: Option<usize>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            with = "opt_decimal_str"
        )]
        inv_arc: Option<Decimal>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            with = "opt_decimal_str"
        )]
        inv_xyz: Option<Decimal>,
        journal_ms: u64,
    },
}

/// Fail-closed intent durability log: every dispatch writes & fsyncs `IntentOpen` first.
#[derive(Debug)]
pub struct BasisJournal {
    file: File,
}

/// Keep the returned descriptor alive for the entire writer session.
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
        .context("state is already in use or cannot be locked")?;
    File::open(parent)?.sync_all()?;
    Ok(file)
}

impl BasisJournal {
    pub fn open_and_check_restart(path: PathBuf) -> Result<Self> {
        let file = lock_state_file(&path)?;
        Self::check_clean_restart(&path)?;
        Ok(Self { file })
    }

    pub fn check_clean_restart(path: &Path) -> Result<()> {
        let file = match File::open(path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => {
                return Err(err).with_context(|| format!("open journal {}", path.display()));
            }
        };
        let reader = BufReader::new(file);
        let mut pending = HashSet::new();
        let mut blocking_shutdown: Option<String> = None;
        for (line_no, line) in reader.lines().enumerate() {
            let line = line.with_context(|| {
                format!("read journal line {} in {}", line_no + 1, path.display())
            })?;
            if line.trim().is_empty() {
                continue;
            }
            let event: BasisJournalEvent = serde_json::from_str(&line).with_context(|| {
                format!("corrupt journal line {} in {}", line_no + 1, path.display())
            })?;
            match event {
                BasisJournalEvent::IntentOpen { intent_id, .. } => {
                    pending.insert(intent_id);
                }
                BasisJournalEvent::IntentNotSent { intent_id, .. }
                | BasisJournalEvent::IntentTerminal { intent_id, .. } => {
                    pending.remove(&intent_id);
                }
                BasisJournalEvent::CleanShutdown { .. } => {
                    blocking_shutdown = None;
                }
                BasisJournalEvent::ShutdownUnverified { reason, .. } => {
                    if reason != "residual_position" {
                        blocking_shutdown = Some(reason);
                    }
                }
                BasisJournalEvent::SessionStart { .. } => {}
            }
        }
        ensure!(
            pending.is_empty(),
            "unresolved intents in {} ({:?}); reconcile both venues and clear journal before restart",
            path.display(),
            pending
        );
        if let Some(reason) = blocking_shutdown {
            bail!(
                "prior shutdown in {} was unverified ({reason}); verify and clear open orders/cancels before restart",
                path.display()
            );
        }
        Ok(())
    }

    pub fn append(&self, event: &BasisJournalEvent) -> Result<()> {
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
    use crate::exchanges::hyperliquid::{BestBidAsk, ValidatedBook};

    fn test_config() -> ArcusXyzBasisConfig {
        ArcusXyzBasisConfig {
            network: "mainnet".into(),
            arcus_symbol: "ANT-USD".into(),
            hl_dex: "xyz".into(),
            hl_coin: "xyz:ANT".into(),
            entry_bp: 10.0,
            exit_bp: 0.0,
            max_entry_bp: 35.0,
            persist_ms: 100,
            post_trade_refresh_lock: true,
            clip_usd: 500.0,
            max_notional_usd: 2_000.0,
            ladder_floor_frac: 0.25,
            ladder_scale_bps: 2.0,
            ladder_convex_power: 1.0,
            max_hold_ms: 7_200_000,
            max_arcus_book_age_ms: 250,
            max_hl_book_age_ms: 5_000,
            max_venue_clock_skew_ms: 2_000,
            slippage_bps: 8.0,
            allocated_capital_usd: 1_000.0,
            daily_loss_limit_pct: 0.05,
            canary_max_order_usd: 500.0,
            canary_max_total_notional_usd: 2_000.0,
            require_arcus_cancel_lease: false,
            dry_run: true,
            poll_interval_ms: 250,
            journal_path: PathBuf::from("state/test-journal.ndjson"),
            daily_loss_state_path: PathBuf::from("state/test-daily-loss.json"),
        }
    }

    fn d(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    fn arc_book(ts: u64, bid: &str, ask: &str, sz: &str) -> ArcusBookSnapshot {
        ArcusBookSnapshot {
            source_ms: ts,
            receipt_ms: ts,
            bid_px: d(bid),
            bid_sz: d(sz),
            ask_px: d(ask),
            ask_sz: d(sz),
        }
    }

    fn xyz_book(ts: u64, bids: &[(&str, &str)], asks: &[(&str, &str)]) -> ValidatedBook {
        ValidatedBook {
            bbo: BestBidAsk {
                coin: "xyz:ANT".into(),
                timestamp_ms: ts,
                bid: d(bids[0].0),
                bid_size: d(bids[0].1),
                ask: d(asks[0].0),
                ask_size: d(asks[0].1),
            },
            bids: bids.iter().map(|(p, s)| (d(p), d(s))).collect(),
            asks: asks.iter().map(|(p, s)| (d(p), d(s))).collect(),
        }
    }

    #[test]
    fn cap_every_leg_and_allow_small_reduce_only_exit() {
        let mut cfg = test_config();
        cfg.persist_ms = 0;
        cfg.clip_usd = 50.0;
        cfg.canary_max_order_usd = 50.0;
        cfg.max_notional_usd = 100.0;
        let g = DailyLossGuard::open(None, 0.05, 500.0, 1).unwrap();
        let mut engine = ArcusXyzBasisEngine::new(cfg.clone()).unwrap();
        let now = cfg.max_hold_ms + 10;
        let a = arc_book(now, "99.99", "100.01", "10");
        let x = xyz_book(now, &[("100.20", "10")], &[("100.22", "10")]);
        let enter = engine.on_books(now, &a, &x, now, &g, true);
        let BasisAction::EnterPairIoc {
            qty,
            arcus_limit_px,
            xyz_limit_px,
            ..
        } = enter
        else {
            panic!("expected entry");
        };
        assert!(qty * arcus_limit_px <= d("50") && qty * xyz_limit_px.max(x.bbo.bid) <= d("50"));
        for pos in ["1", "0.03"] {
            let mut engine = ArcusXyzBasisEngine::new(cfg.clone()).unwrap();
            engine.reconcile_positions(d(pos), -d(pos), 1, 1, 1);
            let BasisAction::ExitPairIoc {
                qty,
                arcus_limit_px,
                xyz_limit_px,
                ..
            } = engine.on_books(now, &a, &x, now, &g, true)
            else {
                panic!("expected timed exit for {pos}");
            };
            assert!(qty * xyz_limit_px <= d("50") && qty * arcus_limit_px.max(a.bid_px) <= d("50"));
            if pos == "0.03" {
                assert_eq!(qty, d(pos));
            }
        }
        engine.reconcile_positions(d("1"), Decimal::ZERO, 1, 1, 1);
        let BasisAction::HedgeResidualXyz {
            qty, limit_price, ..
        } = engine.on_books(now, &a, &x, now, &g, true)
        else {
            panic!("expected residual");
        };
        assert!(qty * limit_price.max(x.bbo.bid) <= d("50"));
    }

    #[test]
    fn live_raw_edge_cap_matches_oos_boundaries() {
        let g = DailyLossGuard::open(None, 0.05, 500.0, 1).unwrap();
        for (bid, ask, admitted) in [
            ("100.34", "100.35", true),
            ("100.36", "100.37", false),
            ("100.48", "100.49", false),
        ] {
            let mut cfg = test_config();
            cfg.persist_ms = 0;
            let mut engine = ArcusXyzBasisEngine::new(cfg).unwrap();
            let action = engine.on_books(
                1000,
                &arc_book(1000, "99.99", "100", "10"),
                &xyz_book(1000, &[(bid, "10")], &[(ask, "10")]),
                1000,
                &g,
                true,
            );
            assert_eq!(matches!(action, BasisAction::EnterPairIoc { .. }), admitted);
        }
    }

    #[test]
    fn config_and_canary_caps_enforce_safety_bounds() {
        let mut cfg = test_config();
        assert!(cfg.validate().is_ok());
        // Cannot launch live canary while dry_run=true.
        assert!(cfg.validate_canary_caps().is_err());

        cfg.dry_run = false;
        assert!(cfg.validate_canary_caps().is_ok());

        cfg.clip_usd = 550.0;
        assert!(cfg.validate_canary_caps().is_err());

        cfg.clip_usd = 100.0;
        cfg.hl_coin = "ANT".into(); // missing xyz: prefix when hl_dex="xyz"
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn causal_persistence_blocks_spike_and_admits_sustained_edge() {
        let mut engine = ArcusXyzBasisEngine::new(test_config()).unwrap();
        let guard = DailyLossGuard::open(None, 0.05, 1_000.0, 100).unwrap();

        // +12.99 bp edge (buy Arcus @ 99.92, sell xyz @ 100.05) -> first tick only arms persistence!
        let a1 = arc_book(1_000, "99.90", "99.92", "50");
        let x1 = xyz_book(1_000, &[("100.05", "50")], &[("100.07", "50")]);
        assert_eq!(
            engine.on_books(1_000, &a1, &x1, 1_000, &guard, true),
            BasisAction::Hold {
                reason: "persistence_arming"
            }
        );

        // Spike collapses at +60ms (< 100ms): must reset persistence and never fire!
        let a2 = arc_book(1_060, "100.03", "100.05", "50");
        assert_eq!(
            engine.on_books(1_060, &a2, &x1, 1_000, &guard, true),
            BasisAction::Hold {
                reason: "below_entry_threshold"
            }
        );
        assert_eq!(engine.counters.persist_reset_count, 1);

        // Sustained dislocation >= 100ms fires EnterPairIoc.
        let a3 = arc_book(2_000, "99.90", "99.92", "50");
        assert!(matches!(
            engine.on_books(2_000, &a3, &x1, 2_000, &guard, true),
            BasisAction::Hold {
                reason: "persistence_arming"
            }
        ));
        let a4 = arc_book(2_110, "99.90", "99.92", "50");
        let act = engine.on_books(2_110, &a4, &x1, 2_000, &guard, true);
        match act {
            BasisAction::EnterPairIoc {
                direction_arcus_buy,
                qty,
                brake_scale,
                ..
            } => {
                assert!(direction_arcus_buy);
                assert!(qty > d("4.9") && qty < d("5.1"));
                assert_eq!(brake_scale, 1.0);
            }
            other => panic!("expected EnterPairIoc, got {other:?}"),
        }
        assert!(engine.in_flight());
    }

    #[test]
    fn leung_li_cap_and_asymmetric_ladder_and_progressive_brake_work_together() {
        let mut cfg = test_config();
        cfg.persist_ms = 0;
        let mut engine = ArcusXyzBasisEngine::new(cfg).unwrap();
        let mut guard = DailyLossGuard::open(None, 0.05, 1_000.0, 100).unwrap();

        // 50 bp dislocation exceeds max_entry_bp=35 -> blocked by Leung-Li!
        let a_brk = arc_book(1_000, "99.48", "99.50", "50");
        let x_brk = xyz_book(1_000, &[("100.00", "50")], &[("100.02", "50")]);
        assert_eq!(
            engine.on_books(1_000, &a_brk, &x_brk, 1_000, &guard, true),
            BasisAction::Hold {
                reason: "leung_li_structural_cap"
            }
        );
        assert_eq!(engine.counters.leung_li_blocked, 1);

        // Engage Tier 2 soft brake (r_dd = 0.50 -> $25 loss on $50 budget -> risk_scale = 0.50x).
        guard.check(975.0, 100).unwrap();
        assert_eq!(guard.risk_scale(), 0.50);

        // Also seed 10 ANT (~$1000 = 50% of $2000 max_notional) -> ladder penalty > 0 raises req > 10 bp.
        engine.reconcile_positions(d("10"), d("-10"), 1_000, 1_000, 1_000);
        // +10.2 bp edge is enough for flat 10.0 bp, NOT enough for ladder req (10 + 2/3 ~= 10.67 bp)!
        let a_low = arc_book(1_200, "99.90", "99.92", "50");
        let x_low = xyz_book(1_200, &[("100.022", "50")], &[("100.042", "50")]);
        assert_eq!(
            engine.on_books(1_200, &a_low, &x_low, 1_200, &guard, true),
            BasisAction::Hold {
                reason: "below_entry_threshold"
            }
        );

        // +13 bp clears ladder hurdle and scales clip $500 * 0.50 = $250 (~2.502 ANT).
        let x_ok = xyz_book(1_300, &[("100.05", "50")], &[("100.07", "50")]);
        match engine.on_books(1_300, &a_low, &x_ok, 1_200, &guard, true) {
            BasisAction::EnterPairIoc {
                qty, brake_scale, ..
            } => {
                assert_eq!(brake_scale, 0.50);
                assert!(qty > d("2.4") && qty < d("2.6"));
            }
            other => panic!("expected scaled add entry, got {other:?}"),
        }
    }

    #[test]
    fn unhedged_residual_prioritizes_visible_depth_xyz_hedge() {
        let mut cfg = test_config();
        cfg.persist_ms = 0;
        let mut engine = ArcusXyzBasisEngine::new(cfg).unwrap();
        let guard = DailyLossGuard::open(None, 0.05, 1_000.0, 100).unwrap();
        // Long 5 Arcus, short only 2 xyz -> unhedged +3 needs sell 3 on xyz, but only 1.8 within limit!
        engine.reconcile_positions(d("5"), d("-2"), 1_000, 1_000, 1_000);
        let a = arc_book(1_500, "99.90", "99.92", "50");
        let x = xyz_book(
            1_500,
            &[("100.05", "1.0"), ("99.98", "0.8"), ("99.00", "20.0")],
            &[("100.07", "50")],
        );
        match engine.on_books(1_500, &a, &x, 1_500, &guard, true) {
            BasisAction::HedgeResidualXyz {
                is_buy,
                qty,
                reduce_only,
                ..
            } => {
                assert!(!is_buy);
                assert_eq!(qty, d("1.8"));
                assert!(!reduce_only); // deepening short xyz from -2 to -3.8
            }
            other => panic!("expected HedgeResidualXyz, got {other:?}"),
        }
    }

    #[test]
    fn journal_enforces_restart_barrier_until_terminal_reconciliation() {
        let path = std::env::temp_dir()
            .join(format!("aleph-basis-journal-{}", uuid::Uuid::new_v4()))
            .join("basis.ndjson");
        let journal = BasisJournal::open_and_check_restart(path.clone()).unwrap();
        assert!(BasisJournal::open_and_check_restart(path.clone()).is_err());
        journal
            .append(&BasisJournalEvent::IntentOpen {
                intent_id: "i-1".into(),
                kind: "entry".into(),
                arcus_client_id: Some("arc-1".into()),
                hl_cloid: Some("0x01".into()),
                qty: d("0.5"),
                journal_ms: 1_000,
            })
            .unwrap();

        // Unresolved intent blocks restart!
        assert!(BasisJournal::check_clean_restart(&path).is_err());

        journal
            .append(&BasisJournalEvent::IntentTerminal {
                intent_id: "i-1".into(),
                status: "reconciled".into(),
                inv_arc: d("0.5"),
                inv_xyz: d("-0.5"),
                journal_ms: 1_200,
                arcus_status: None,
                arcus_filled_qty: None,
                hl_status: None,
                hl_filled_qty: None,
                hl_terminal_at_ms: None,
                hl_account_time_ms: None,
            })
            .unwrap();
        assert!(BasisJournal::check_clean_restart(&path).is_ok());

        // ShutdownUnverified(cancel_unknown / open_orders_remaining) keeps restart blocked even across subsequent residual_position until CleanShutdown.
        journal
            .append(&BasisJournalEvent::ShutdownUnverified {
                reason: "cancel_unknown".into(),
                cancel_ok: false,
                open_orders_arcus: Some(0),
                open_orders_hl: Some(0),
                inv_arc: Some(Decimal::ZERO),
                inv_xyz: Some(Decimal::ZERO),
                journal_ms: 1_300,
            })
            .unwrap();
        assert!(BasisJournal::check_clean_restart(&path).is_err());

        journal
            .append(&BasisJournalEvent::ShutdownUnverified {
                reason: "open_orders_remaining".into(),
                cancel_ok: true,
                open_orders_arcus: Some(1),
                open_orders_hl: Some(0),
                inv_arc: Some(Decimal::ZERO),
                inv_xyz: Some(Decimal::ZERO),
                journal_ms: 1_350,
            })
            .unwrap();
        assert!(BasisJournal::check_clean_restart(&path).is_err());

        journal
            .append(&BasisJournalEvent::ShutdownUnverified {
                reason: "residual_position".into(),
                cancel_ok: true,
                open_orders_arcus: Some(0),
                open_orders_hl: Some(0),
                inv_arc: Some(d("0.5")),
                inv_xyz: Some(d("-0.5")),
                journal_ms: 1_400,
            })
            .unwrap();
        assert!(BasisJournal::check_clean_restart(&path).is_err());

        journal
            .append(&BasisJournalEvent::CleanShutdown { journal_ms: 1_420 })
            .unwrap();
        assert!(BasisJournal::check_clean_restart(&path).is_ok());

        journal
            .append(&BasisJournalEvent::ShutdownUnverified {
                reason: "residual_position".into(),
                cancel_ok: true,
                open_orders_arcus: Some(0),
                open_orders_hl: Some(0),
                inv_arc: Some(d("0.5")),
                inv_xyz: Some(d("-0.5")),
                journal_ms: 1_450,
            })
            .unwrap();
        assert!(BasisJournal::check_clean_restart(&path).is_ok());

        // ShutdownUnverified must never clear an unresolved IntentOpen.
        journal
            .append(&BasisJournalEvent::IntentOpen {
                intent_id: "i-2".into(),
                kind: "entry".into(),
                arcus_client_id: Some("arc-2".into()),
                hl_cloid: Some("0x02".into()),
                qty: d("0.5"),
                journal_ms: 1_500,
            })
            .unwrap();
        journal
            .append(&BasisJournalEvent::ShutdownUnverified {
                reason: "residual_position".into(),
                cancel_ok: true,
                open_orders_arcus: Some(0),
                open_orders_hl: Some(0),
                inv_arc: Some(d("0.5")),
                inv_xyz: Some(Decimal::ZERO),
                journal_ms: 1_550,
            })
            .unwrap();
        assert!(BasisJournal::check_clean_restart(&path).is_err());

        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    fn hl_state(coin: &str, szi: &str, time: u64) -> HlAccountState {
        use crate::exchanges::hyperliquid::{AssetPosition, MarginSummary, Position as HlPos};
        HlAccountState {
            margin_summary: MarginSummary {
                account_value: "1000".into(),
                total_ntl_pos: "0".into(),
                total_margin_used: "0".into(),
            },
            asset_positions: if coin.is_empty() {
                vec![]
            } else {
                vec![AssetPosition {
                    position: HlPos {
                        coin: coin.into(),
                        szi: szi.into(),
                        entry_px: Some("100".into()),
                        unrealized_pnl: "0".into(),
                    },
                }]
            },
            withdrawable: "1000".into(),
            time,
        }
    }

    fn hl_located(
        cloid: &str,
        is_buy: bool,
        orig_sz: &str,
        rem_sz: &str,
        limit_px: &str,
        status: &str,
        status_ts: u64,
    ) -> crate::exchanges::hyperliquid::LocatedOrder {
        use crate::exchanges::hyperliquid::LocatedOrder;
        LocatedOrder {
            order: HlOpenOrder {
                coin: "xyz:ANT".into(),
                side: if is_buy { "B".into() } else { "A".into() },
                limit_px: limit_px.into(),
                sz: rem_sz.into(),
                oid: 77,
                timestamp: status_ts,
                cloid: Some(cloid.into()),
                original_size: Some(orig_sz.into()),
            },
            status: status.into(),
            status_timestamp: status_ts,
        }
    }

    #[test]
    fn parse_hl_market_position_fails_closed_on_malformed_duplicate_or_zero_time() {
        assert_eq!(
            parse_hl_market_position(&hl_state("xyz:ANT", "-1.25", 10_000), "xyz:ANT").unwrap(),
            d("-1.25")
        );
        assert_eq!(
            parse_hl_market_position(&hl_state("xyz:OTHER", "2", 10_000), "xyz:ANT").unwrap(),
            Decimal::ZERO
        );
        assert!(
            parse_hl_market_position(&hl_state("xyz:ANT", "bad_num", 10_000), "xyz:ANT").is_err()
        );
        assert!(parse_hl_market_position(&hl_state("xyz:ANT", "1.0", 0), "xyz:ANT").is_err());

        let mut dup = hl_state("xyz:ANT", "1.0", 10_000);
        dup.asset_positions.push(dup.asset_positions[0].clone());
        assert!(parse_hl_market_position(&dup, "xyz:ANT").is_err());
    }

    #[test]
    fn classify_hl_submit_rejects_resting_and_distinguishes_terminal_vs_ambiguous() {
        assert_eq!(
            classify_hl_submit(
                &Ok(vec![HlExecutionStatus::Resting { order_id: 9 }]),
                d("1")
            ),
            HlSubmitOutcome::UnexpectedResting { order_id: 9 }
        );
        assert_eq!(
            classify_hl_submit(
                &Ok(vec![HlExecutionStatus::Filled {
                    order_id: 10,
                    total_size: d("0.5"),
                    average_price: d("100.1"),
                }]),
                d("1")
            ),
            HlSubmitOutcome::FilledAck {
                order_id: 10,
                total_size: d("0.5"),
                average_price: d("100.1"),
            }
        );
        assert_eq!(
            classify_hl_submit(&Ok(vec![HlExecutionStatus::Canceled]), d("1")),
            HlSubmitOutcome::CanceledAck
        );
        assert_eq!(
            classify_hl_submit(
                &Ok(vec![HlExecutionStatus::Rejected {
                    reason: "min ntl".into()
                }]),
                d("1")
            ),
            HlSubmitOutcome::DefinitiveRejected {
                reason: "min ntl".into()
            }
        );
        assert_eq!(
            classify_hl_submit(
                &Err(anyhow::Error::new(HlSubmissionError::Rejected(
                    "bad".into()
                ))),
                d("1")
            ),
            HlSubmitOutcome::DefinitiveRejected {
                reason: "bad".into()
            }
        );
        assert!(matches!(
            classify_hl_submit(
                &Err(anyhow::Error::new(HlSubmissionError::Uncertain(
                    "timeout".into()
                ))),
                d("1")
            ),
            HlSubmitOutcome::Ambiguous { .. }
        ));
        assert!(matches!(
            classify_hl_submit(&Ok(vec![]), d("1")),
            HlSubmitOutcome::Ambiguous { .. }
        ));
    }

    #[test]
    fn verify_hl_leg_terminal_enforces_cloid_and_watermark() {
        let intent = HlLegIntent {
            cloid: "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            coin: "xyz:ANT".into(),
            is_buy: false,
            qty: d("1.0"),
            limit_price: d("99.5"),
            sent_at_ms: 5_000,
            prev_inv_xyz: Decimal::ZERO,
        };
        let filled_submit = HlSubmitOutcome::FilledAck {
            order_id: 77,
            total_size: d("1.0"),
            average_price: d("100.0"),
        };
        let ambig = HlSubmitOutcome::Ambiguous {
            reason: "http timeout".into(),
        };
        let loc_filled = hl_located(&intent.cloid, false, "1.0", "0.0", "99.5", "filled", 5_100);

        // Resting is always fatal.
        assert!(
            verify_hl_leg_terminal(
                &intent,
                &HlSubmitOutcome::UnexpectedResting { order_id: 1 },
                &HlOrderLookup::Unknown,
                &[],
                Decimal::ZERO,
                5_200,
            )
            .is_err()
        );
        // Open lookup is fatal.
        assert!(
            verify_hl_leg_terminal(
                &intent,
                &ambig,
                &HlOrderLookup::Found(hl_located(
                    &intent.cloid,
                    false,
                    "1.0",
                    "1.0",
                    "99.5",
                    "open",
                    5_100,
                )),
                &[],
                Decimal::ZERO,
                5_200,
            )
            .is_err()
        );
        // Ambiguous + Unknown stays pending (Ok(None)), never terminal!
        assert_eq!(
            verify_hl_leg_terminal(
                &intent,
                &ambig,
                &HlOrderLookup::Unknown,
                &[],
                Decimal::ZERO,
                5_500,
            )
            .unwrap(),
            None
        );
        // Stale account watermark < terminal_at_ms -> Ok(None) (retry).
        assert_eq!(
            verify_hl_leg_terminal(
                &intent,
                &filled_submit,
                &HlOrderLookup::Found(loc_filled.clone()),
                &[],
                d("-1.0"),
                5_050,
            )
            .unwrap(),
            None
        );
        // Fresh watermark + exact delta -> Ok(Some(VerifiedLegTerminal)).
        let ok = verify_hl_leg_terminal(
            &intent,
            &filled_submit,
            &HlOrderLookup::Found(loc_filled.clone()),
            &[],
            d("-1.0"),
            5_100,
        )
        .unwrap()
        .unwrap();
        assert_eq!(ok.filled_qty, d("1.0"));
        assert_eq!(ok.avg_price, Some(d("100.0")));

        let wrong_order_ack = HlSubmitOutcome::FilledAck {
            order_id: 78,
            total_size: d("1.0"),
            average_price: d("100.0"),
        };
        assert!(
            verify_hl_leg_terminal(
                &intent,
                &wrong_order_ack,
                &HlOrderLookup::Found(loc_filled),
                &[],
                d("-1.0"),
                5_100,
            )
            .is_err()
        );

        // Definitive reject with Unknown lookup + unchanged inv_xyz -> Ok(Some(rejected, 0)).
        let rej = verify_hl_leg_terminal(
            &intent,
            &HlSubmitOutcome::DefinitiveRejected {
                reason: "rejected".into(),
            },
            &HlOrderLookup::Unknown,
            &[],
            Decimal::ZERO,
            5_200,
        )
        .unwrap()
        .unwrap();
        assert_eq!(rej.status, "rejected");
        assert_eq!(rej.filled_qty, Decimal::ZERO);

        // Hyperliquid FilledAck average_price must be positive and within the journaled IOC limit.
        let sell_bad_px = HlSubmitOutcome::FilledAck {
            order_id: 77,
            total_size: d("1.0"),
            average_price: d("99.49"),
        };
        let loc_sell = hl_located(&intent.cloid, false, "1.0", "0.0", "99.5", "filled", 5_100);
        assert!(
            verify_hl_leg_terminal(
                &intent,
                &sell_bad_px,
                &HlOrderLookup::Found(loc_sell.clone()),
                &[],
                d("-1.0"),
                5_100,
            )
            .is_err()
        );
        let sell_zero_px = HlSubmitOutcome::FilledAck {
            order_id: 77,
            total_size: d("1.0"),
            average_price: Decimal::ZERO,
        };
        assert!(
            verify_hl_leg_terminal(
                &intent,
                &sell_zero_px,
                &HlOrderLookup::Found(loc_sell.clone()),
                &[],
                d("-1.0"),
                5_100,
            )
            .is_err()
        );
        let sell_exact_px = HlSubmitOutcome::FilledAck {
            order_id: 77,
            total_size: d("1.0"),
            average_price: intent.limit_price,
        };
        assert!(
            verify_hl_leg_terminal(
                &intent,
                &sell_exact_px,
                &HlOrderLookup::Found(loc_sell),
                &[],
                d("-1.0"),
                5_100,
            )
            .unwrap()
            .is_some()
        );

        let buy_intent = HlLegIntent {
            is_buy: true,
            ..intent.clone()
        };
        let loc_buy = hl_located(
            &buy_intent.cloid,
            true,
            "1.0",
            "0.0",
            "99.5",
            "filled",
            5_100,
        );
        let buy_bad_px = HlSubmitOutcome::FilledAck {
            order_id: 77,
            total_size: d("1.0"),
            average_price: d("99.51"),
        };
        assert!(
            verify_hl_leg_terminal(
                &buy_intent,
                &buy_bad_px,
                &HlOrderLookup::Found(loc_buy.clone()),
                &[],
                d("1.0"),
                5_100,
            )
            .is_err()
        );
        let buy_exact_px = HlSubmitOutcome::FilledAck {
            order_id: 77,
            total_size: d("1.0"),
            average_price: buy_intent.limit_price,
        };
        assert!(
            verify_hl_leg_terminal(
                &buy_intent,
                &buy_exact_px,
                &HlOrderLookup::Found(loc_buy.clone()),
                &[],
                d("1.0"),
                5_100,
            )
            .unwrap()
            .is_some()
        );
        let open_by_oid_no_cloid = HlOpenOrder {
            coin: "xyz:ANT".into(),
            side: "B".into(),
            limit_px: "99.5".into(),
            sz: "1.0".into(),
            oid: 77,
            timestamp: 5_100,
            cloid: None,
            original_size: Some("1.0".into()),
        };
        assert!(
            verify_hl_leg_terminal(
                &buy_intent,
                &ambig,
                &HlOrderLookup::Found(loc_buy.clone()),
                std::slice::from_ref(&open_by_oid_no_cloid),
                d("1.0"),
                5_100,
            )
            .is_err()
        );
        assert!(
            verify_hl_leg_terminal(
                &buy_intent,
                &buy_exact_px,
                &HlOrderLookup::Found(loc_buy),
                &[open_by_oid_no_cloid],
                d("1.0"),
                5_100,
            )
            .is_err()
        );
    }

    #[test]
    fn verify_arcus_leg_terminal_never_trusts_http_202_alone_and_checks_fills_and_position() {
        let mut intent = ArcusLegIntent {
            market_id: 7,
            client_id: "axb123".into(),
            side: ArcusSide::Buy,
            qty: d("1.0"),
            limit_price: d("100.0"),
            sent_at_us: 10_000_000,
            prev_inv_arc: Decimal::ZERO,
        };
        let receipt_202 = ArcusOrderReceipt {
            http_status: 202,
            status: "accepted".into(),
            order_id: Some("arc-ord-1".into()),
            client_id: Some("axb123".into()),
            rejection_reason: None,
        };
        let accepted = classify_arcus_submit(&Ok(receipt_202), "axb123");
        assert!(matches!(
            accepted,
            ArcusSubmitOutcome::GatewayAccepted { .. }
        ));

        // HTTP 202 + missing history stays pending (Ok(None)).
        assert_eq!(
            verify_arcus_leg_terminal(&intent, &accepted, None, &[], &[], &[], Decimal::ZERO)
                .unwrap(),
            None
        );

        let mut arc_order = ArcusOrder {
            order_id: "arc-ord-1".into(),
            client_id: Some("axb123".into()),
            market_id: 7,
            side: ArcusSide::Buy,
            status: "FILLED".into(),
            price: d("100.0"),
            original_size: d("1.0"),
            remaining_size: Decimal::ZERO,
            created_at: Some(10_000_100),
            updated_at: 10_050_000,
        };
        let mut arc_fill = ArcusFill {
            trade_id: "tr-1".into(),
            order_id: "arc-ord-1".into(),
            client_id: Some("axb123".into()),
            market_id: 7,
            side: ArcusSide::Buy,
            size: d("1.0"),
            price: d("99.95"),
            fee: d("0.01"),
            role: "TAKER".into(),
            created_at: 10_050_000,
        };

        // Still in open_orders -> Ok(None).
        assert_eq!(
            verify_arcus_leg_terminal(
                &intent,
                &accepted,
                Some(&arc_order),
                std::slice::from_ref(&arc_order),
                &[],
                std::slice::from_ref(&arc_fill),
                d("1.0"),
            )
            .unwrap(),
            None
        );
        // Terminal order without matching fills -> Ok(None).
        assert_eq!(
            verify_arcus_leg_terminal(
                &intent,
                &accepted,
                Some(&arc_order),
                &[],
                &[],
                &[],
                d("1.0")
            )
            .unwrap(),
            None
        );
        // Terminal order + matching fill + stale inventory -> Ok(None).
        assert_eq!(
            verify_arcus_leg_terminal(
                &intent,
                &accepted,
                Some(&arc_order),
                &[],
                &[],
                std::slice::from_ref(&arc_fill),
                Decimal::ZERO,
            )
            .unwrap(),
            None
        );
        // Terminal order + matching fills + matching inventory -> Ok(Some(...)).
        let ok = verify_arcus_leg_terminal(
            &intent,
            &accepted,
            Some(&arc_order),
            &[],
            &[],
            std::slice::from_ref(&arc_fill),
            d("1.0"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(ok.status, "filled");
        assert_eq!(ok.filled_qty, d("1.0"));
        assert_eq!(ok.avg_price, Some(d("99.95")));

        let ambiguous = ArcusSubmitOutcome::Ambiguous {
            reason: "http timeout".into(),
        };
        let history = std::slice::from_ref(&arc_order);
        let fills = std::slice::from_ref(&arc_fill);
        assert!(
            verify_arcus_leg_terminal(&intent, &ambiguous, None, &[], history, fills, d("1.0"))
                .unwrap()
                .is_some()
        );
        let open_without_client_id = ArcusOrder {
            order_id: arc_order.order_id.clone(),
            client_id: None,
            market_id: arc_order.market_id,
            side: arc_order.side,
            status: "OPEN".into(),
            price: arc_order.price,
            original_size: arc_order.original_size,
            remaining_size: arc_order.original_size,
            created_at: arc_order.created_at,
            updated_at: arc_order.updated_at,
        };
        assert_eq!(
            verify_arcus_leg_terminal(
                &intent,
                &ambiguous,
                None,
                &[open_without_client_id],
                history,
                fills,
                d("1.0"),
            )
            .unwrap(),
            None
        );

        let client_id = arc_fill.client_id.replace("different-client-id".into());
        assert!(
            verify_arcus_leg_terminal(
                &intent,
                &accepted,
                Some(&arc_order),
                &[],
                &[],
                std::slice::from_ref(&arc_fill),
                d("1.0"),
            )
            .is_err()
        );
        arc_fill.client_id = client_id;

        // Hard rejection with empty order_history/fills and unchanged position -> Ok(Some(rejected)).
        let hard_rej = classify_arcus_submit(
            &Err(anyhow::Error::new(ArcusExecutionError::Rejected {
                path: "/v1/order",
                status: reqwest::StatusCode::BAD_REQUEST,
                message: "min_notional".into(),
            })),
            "axb123",
        );
        let rej = verify_arcus_leg_terminal(&intent, &hard_rej, None, &[], &[], &[], Decimal::ZERO)
            .unwrap()
            .unwrap();
        assert_eq!(rej.status, "rejected");
        assert_eq!(rej.filled_qty, Decimal::ZERO);

        // An impossible fill beyond the signed price cannot clear the intent,
        // even when its quantity agrees with the order and account position.
        arc_fill.price = d("100.01");
        assert!(
            verify_arcus_leg_terminal(
                &intent,
                &accepted,
                Some(&arc_order),
                &[],
                &[],
                std::slice::from_ref(&arc_fill),
                d("1.0"),
            )
            .is_err()
        );
        intent.side = ArcusSide::Sell;
        arc_order.side = ArcusSide::Sell;
        arc_fill.side = ArcusSide::Sell;
        let verify_sale = |fill: &ArcusFill| {
            verify_arcus_leg_terminal(
                &intent,
                &accepted,
                Some(&arc_order),
                &[],
                &[],
                std::slice::from_ref(fill),
                d("-1.0"),
            )
        };
        arc_fill.price = d("99.99");
        assert!(verify_sale(&arc_fill).is_err());
        arc_fill.price = intent.limit_price;
        assert!(verify_sale(&arc_fill).unwrap().is_some());

        // Identical duplicate trade_id is idempotently deduped, while conflicting duplicate size/price fails closed.
        let mk_fill = |size: Decimal, price: Decimal| ArcusFill {
            trade_id: "tr-1".into(),
            order_id: "arc-ord-1".into(),
            client_id: Some("axb123".into()),
            market_id: 7,
            side: ArcusSide::Sell,
            size,
            price,
            fee: d("0.01"),
            role: "TAKER".into(),
            created_at: 10_050_000,
        };
        let dup_identical = [
            mk_fill(d("1.0"), intent.limit_price),
            mk_fill(d("1.0"), intent.limit_price),
        ];
        assert!(
            verify_arcus_leg_terminal(
                &intent,
                &accepted,
                Some(&arc_order),
                &[],
                &[],
                &dup_identical,
                d("-1.0"),
            )
            .unwrap()
            .is_some()
        );
        let conflicting_px = [
            mk_fill(d("1.0"), intent.limit_price),
            mk_fill(d("1.0"), d("100.05")),
        ];
        assert!(
            verify_arcus_leg_terminal(
                &intent,
                &accepted,
                Some(&arc_order),
                &[],
                &[],
                &conflicting_px,
                d("-1.0"),
            )
            .is_err()
        );
        let conflicting_sz = [
            mk_fill(d("1.0"), intent.limit_price),
            mk_fill(d("0.5"), intent.limit_price),
        ];
        assert!(
            verify_arcus_leg_terminal(
                &intent,
                &accepted,
                Some(&arc_order),
                &[],
                &[],
                &conflicting_sz,
                d("-1.0"),
            )
            .is_err()
        );

        let valid_sell_fill = mk_fill(d("1.0"), intent.limit_price);
        let mut wrong_mkt_direct = ArcusOrder {
            order_id: arc_order.order_id.clone(),
            client_id: arc_order.client_id.clone(),
            market_id: 99,
            side: arc_order.side,
            status: arc_order.status.clone(),
            price: arc_order.price,
            original_size: arc_order.original_size,
            remaining_size: arc_order.remaining_size,
            created_at: arc_order.created_at,
            updated_at: arc_order.updated_at,
        };
        let gw_rej = ArcusSubmitOutcome::GatewayRejected {
            order_id: Some("arc-ord-1".into()),
            reason: "rejected".into(),
        };
        assert!(
            verify_arcus_leg_terminal(
                &intent,
                &gw_rej,
                Some(&wrong_mkt_direct),
                &[],
                &[],
                &[],
                Decimal::ZERO,
            )
            .is_err()
        );
        wrong_mkt_direct.market_id = 7;

        let hist_open = ArcusOrder {
            status: "OPEN".into(),
            remaining_size: d("1.0"),
            ..wrong_mkt_direct
        };
        assert_eq!(
            verify_arcus_leg_terminal(
                &intent,
                &accepted,
                Some(&arc_order),
                &[],
                &[hist_open],
                std::slice::from_ref(&valid_sell_fill),
                d("-1.0"),
            )
            .unwrap(),
            None
        );

        let hist_canceled = ArcusOrder {
            order_id: arc_order.order_id.clone(),
            client_id: arc_order.client_id.clone(),
            market_id: 7,
            side: arc_order.side,
            status: "CANCELED".into(),
            price: arc_order.price,
            original_size: arc_order.original_size,
            remaining_size: d("1.0"),
            created_at: arc_order.created_at,
            updated_at: arc_order.updated_at,
        };
        assert!(
            verify_arcus_leg_terminal(
                &intent,
                &accepted,
                Some(&arc_order),
                &[],
                &[hist_canceled],
                std::slice::from_ref(&valid_sell_fill),
                d("-1.0"),
            )
            .is_err()
        );

        let wrong_mkt_fill = ArcusFill {
            market_id: 99,
            ..mk_fill(d("1.0"), intent.limit_price)
        };
        assert!(
            verify_arcus_leg_terminal(
                &intent,
                &accepted,
                Some(&arc_order),
                &[],
                &[],
                &[wrong_mkt_fill],
                d("-1.0"),
            )
            .is_err()
        );

        let empty_trade_id_fill = ArcusFill {
            trade_id: "   ".into(),
            ..mk_fill(d("1.0"), intent.limit_price)
        };
        assert!(
            verify_arcus_leg_terminal(
                &intent,
                &accepted,
                Some(&arc_order),
                &[],
                &[],
                &[empty_trade_id_fill],
                d("-1.0"),
            )
            .is_err()
        );
    }

    #[test]
    fn classify_shutdown_distinguishes_clean_open_orders_residual_and_cancel_unknown() {
        assert_eq!(
            classify_shutdown(true, Ok((0, 0)), Ok((Decimal::ZERO, Decimal::ZERO))),
            ShutdownDisposition::Clean
        );
        assert_eq!(
            classify_shutdown(true, Ok((0, 0)), Ok((d("0.5"), d("-0.5")))),
            ShutdownDisposition::ResidualPosition {
                inv_arc: d("0.5"),
                inv_xyz: d("-0.5"),
            }
        );
        assert!(matches!(
            classify_shutdown(true, Ok((1, 0)), Ok((Decimal::ZERO, Decimal::ZERO))),
            ShutdownDisposition::OpenOrdersRemaining {
                open_orders_arcus: 1,
                open_orders_hl: 0,
                ..
            }
        ));
        assert!(matches!(
            classify_shutdown(false, Ok((0, 0)), Ok((Decimal::ZERO, Decimal::ZERO))),
            ShutdownDisposition::CancelUnknown {
                cancel_ok: false,
                ..
            }
        ));
    }

    #[test]
    fn journal_keeps_prior_blocking_shutdown_across_subsequent_residual_position() {
        let dir = std::env::temp_dir().join(format!("aleph-basis-jtest-{}", uuid::Uuid::new_v4()));
        let path = dir.join("journal.ndjson");
        let journal = BasisJournal::open_and_check_restart(path.clone()).unwrap();
        journal
            .append(&BasisJournalEvent::ShutdownUnverified {
                reason: "cancel_unknown".into(),
                cancel_ok: false,
                open_orders_arcus: None,
                open_orders_hl: None,
                inv_arc: None,
                inv_xyz: None,
                journal_ms: 1_000,
            })
            .unwrap();
        journal
            .append(&BasisJournalEvent::ShutdownUnverified {
                reason: "residual_position".into(),
                cancel_ok: true,
                open_orders_arcus: Some(0),
                open_orders_hl: Some(0),
                inv_arc: Some(d("0.5")),
                inv_xyz: Some(d("-0.5")),
                journal_ms: 2_000,
            })
            .unwrap();

        let err = BasisJournal::check_clean_restart(&path).unwrap_err();
        assert!(err.to_string().contains("cancel_unknown"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn quantize_shared_base_qty_uses_exact_lcm_for_non_divisible_steps() {
        // 0.003 vs hl_sz_decimals=2 (0.01) has LCM = 0.03 -> 1.00 quantizes down to 0.99 (valid on HL!).
        let q = quantize_shared_base_qty(d("1.00"), d("0.003"), 2).unwrap();
        assert_eq!(q, d("0.99"));
        assert!((q % d("0.003")).is_zero());

        let hl_asset = HlPerpAsset {
            name: "xyz:TEST".into(),
            sz_decimals: 2,
            max_leverage: 10,
            is_delisted: false,
            growth_mode: None,
            deployer_fee_scale: None,
        };
        let order = crate::exchanges::hyperliquid::PerpOrder {
            is_buy: true,
            limit_price: d("100.0"),
            size: q,
            reduce_only: false,
            time_in_force: crate::exchanges::hyperliquid::TimeInForce::Ioc,
            client_order_id: None,
        };
        assert!(hl_asset.validate_order(&order).is_ok());

        // ANT-USD (step 0.000001) with sz_decimals=2 (0.01) quantizes onto 0.01.
        assert_eq!(
            quantize_shared_base_qty(d("1.239"), d("0.000001"), 2).unwrap(),
            d("1.23")
        );
    }

    #[test]
    fn engine_enforces_hl_min_notional_arcus_min_notional_and_unhedgeable_residual_halt() {
        let mut cfg = test_config();
        cfg.persist_ms = 0;
        let mut engine = ArcusXyzBasisEngine::new(cfg).unwrap();
        engine.set_constraints(VenueOrderConstraints {
            arcus_step_size: d("0.01"),
            arcus_min_order_size: d("0.1"),
            arcus_min_order_notional_usd: 5.0,
            hl_sz_decimals: 2,
            hl_min_order_notional_usd: 10.0,
        });
        let guard = DailyLossGuard::open(None, 0.05, 1_000.0, 100).unwrap();

        // Thin depth caps trade to 0.08 ANT (~$8): passes $5 old floor, MUST fail HL $10 / 0.1 min size!
        let a_thin = arc_book(1_000, "99.90", "99.92", "0.08");
        let x_thin = xyz_book(1_000, &[("100.10", "5.0")], &[("100.12", "5.0")]);
        assert_eq!(
            engine.on_books(1_000, &a_thin, &x_thin, 1_000, &guard, true),
            BasisAction::Hold {
                reason: "thin_entry_book"
            }
        );

        // 0.09 ANT @ ~$100 = $9 (< $10 HL min) passes min_order_size=0.05 or not; test $9 vs $10 specifically.
        engine.set_constraints(VenueOrderConstraints {
            arcus_step_size: d("0.01"),
            arcus_min_order_size: d("0.01"),
            arcus_min_order_notional_usd: 15.0,
            hl_sz_decimals: 2,
            hl_min_order_notional_usd: 10.0,
        });
        let a_sub_arc = arc_book(1_100, "99.90", "99.92", "0.12"); // $12 >= $10 HL, < $15 Arcus min!
        assert_eq!(
            engine.on_books(1_100, &a_sub_arc, &x_thin, 1_100, &guard, true),
            BasisAction::Hold {
                reason: "entry_below_venue_min"
            }
        );

        // Residual: unhedged +0.05 ANT (~$5 < $10) with inv_xyz=0 (non-reduce-only) -> permanent halt!
        engine.set_constraints(VenueOrderConstraints::default());
        engine.reconcile_positions(d("0.05"), Decimal::ZERO, 1_200, 1_200, 1_200);
        let a_ok = arc_book(1_300, "99.90", "99.92", "5.0");
        let x_ok = xyz_book(1_300, &[("100.10", "5.0")], &[("100.12", "5.0")]);
        assert_eq!(
            engine.on_books(1_300, &a_ok, &x_ok, 1_300, &guard, true),
            BasisAction::Hold {
                reason: "residual_below_hl_min_notional"
            }
        );
        assert_eq!(
            engine.residual_blocked(),
            Some("residual_below_hl_min_notional")
        );

        // Sub-lot residual (< 0.01 lot when sz_decimals=2) -> permanent halt!
        engine.reconcile_positions(d("0.004"), Decimal::ZERO, 1_400, 1_400, 1_400);
        assert_eq!(
            engine.on_books(1_500, &a_ok, &x_ok, 1_500, &guard, true),
            BasisAction::Hold {
                reason: "residual_below_hl_min_lot"
            }
        );
        assert_eq!(engine.residual_blocked(), Some("residual_below_hl_min_lot"));

        // Reduce-only residual < $10 (but >= 1 lot): HL closes existing long position from +0.05 -> 0!
        engine.reconcile_positions(Decimal::ZERO, d("0.05"), 1_600, 1_600, 1_600);
        let a_red = arc_book(1_700, "99.90", "99.92", "5.0");
        let x_red = xyz_book(1_700, &[("100.10", "5.0")], &[("100.12", "5.0")]);
        match engine.on_books(1_700, &a_red, &x_red, 1_700, &guard, true) {
            BasisAction::HedgeResidualXyz {
                is_buy,
                qty,
                reduce_only,
                ..
            } => {
                assert!(!is_buy);
                assert_eq!(qty, d("0.05"));
                assert!(reduce_only);
            }
            other => panic!("expected reduce_only HedgeResidualXyz, got {other:?}"),
        }
        assert_eq!(engine.residual_blocked(), None);
    }

    #[test]
    fn engine_checks_venue_source_timestamps_independently_of_receipt_time() {
        let mut cfg = test_config();
        cfg.persist_ms = 0;
        let mut engine = ArcusXyzBasisEngine::new(cfg).unwrap();
        let guard = DailyLossGuard::open(None, 0.05, 1_000.0, 100).unwrap();

        // Stale Arcus venue source_ms (> max_arcus_book_age_ms + max_venue_clock_skew_ms) even with fresh receipt_ms!
        let mut a_stale_src = arc_book(10_000, "99.90", "99.92", "10");
        a_stale_src.source_ms = 7_000; // 3_000ms behind now_ms=10_000 (> 250 + 2000)
        let x_fresh = xyz_book(10_000, &[("100.10", "10")], &[("100.12", "10")]);
        assert_eq!(
            engine.on_books(10_000, &a_stale_src, &x_fresh, 10_000, &guard, true),
            BasisAction::Hold {
                reason: "stale_arcus_source_book"
            }
        );

        // Stale HL bbo.timestamp_ms even when xyz_rx_ms == now_ms!
        let a_fresh = arc_book(10_000, "99.90", "99.92", "10");
        let x_stale_src = xyz_book(2_000, &[("100.10", "10")], &[("100.12", "10")]);
        assert_eq!(
            engine.on_books(10_000, &a_fresh, &x_stale_src, 10_000, &guard, true),
            BasisAction::Hold {
                reason: "stale_xyz_source_book"
            }
        );

        // Future timestamp beyond clock-skew tolerance fails closed.
        let mut a_future = arc_book(10_000, "99.90", "99.92", "10");
        a_future.source_ms = 12_500;
        assert_eq!(
            engine.on_books(10_000, &a_future, &x_fresh, 10_000, &guard, true),
            BasisAction::Hold {
                reason: "invalid_arcus_source_time"
            }
        );
    }

    #[test]
    fn arcus_leg_pnl_tracker_drives_combined_daily_loss_guard() {
        let mut tracker = ArcusLegPnlTracker::new(Decimal::ZERO, None);
        tracker.record_fill(true, d("2.0"), d("100.0"), 0.0005); // +2 @ 100, fee = $0.10
        assert_eq!(tracker.position(), d("2.0"));
        assert_eq!(tracker.avg_cost(), d("100.0"));

        // Arcus mid drops to $85 (-$30 unrealized - $0.10 fee = -$30.10) while HL equity delta is $0.
        let est = tracker.estimated_total_pnl_usd(85.0).unwrap();
        assert!((est - (-30.10)).abs() < 1e-9);
        let combined = combined_guard_equity_usd(500.0, 0.0, Some(est));
        assert!((combined - 469.90).abs() < 1e-9);

        let mut guard = DailyLossGuard::open(None, 0.05, 500.0, 100).unwrap(); // budget $25
        let err = guard.check(combined, 100).unwrap_err();
        assert!(err.to_string().contains("daily loss limit breached"));
        assert_eq!(guard.risk_scale(), 0.0); // halted!

        // Partial close realizes loss cleanly and flips when crossing zero.
        tracker.record_fill(false, d("3.0"), d("90.0"), 0.0005);
        assert_eq!(tracker.position(), d("-1.0"));
        assert_eq!(tracker.avg_cost(), d("90.0"));
        assert!((tracker.realized_pnl_usd() - (-20.0)).abs() < 1e-9);
    }
}
