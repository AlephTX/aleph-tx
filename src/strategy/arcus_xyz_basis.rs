//! Dedicated `Arcus <-> Hyperliquid HIP-3 (xyz)` cross-venue basis arbitrage engine (`Step 3`).
//!
//! Translates the OOS-candidate `EXP-ARB-A2` configuration into a standalone state machine
//! with durable intent journaling (`BasisJournal`), flight locks, residual-first hedging,
//! causal persistence, Leung-Li upper dislocation bounds, asymmetric adding-only inventory
//! laddering, and progressive intraday drawdown scaling (`DailyLossGuard::risk_scale`).

use crate::daily_loss::DailyLossGuard;
use crate::exchanges::arcus::Market as ArcusMarket;
use crate::exchanges::hyperliquid::ValidatedBook;
use anyhow::{Context, Result, ensure};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::{Decimal, RoundingStrategy};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
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
fn default_max_hl_age_ms() -> u64 {
    30_000
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
                && self.clip_usd >= 5.0
                && self.max_notional_usd >= self.clip_usd,
            "require 5.0 <= clip_usd <= max_notional_usd"
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
                && self.poll_interval_ms >= 50,
            "invalid hold, freshness, or polling timing bounds"
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
                && self.canary_max_order_usd >= 5.0
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

/// Quantize base size down conservatively to satisfy both Arcus `step_size` and HL `sz_decimals`.
pub fn quantize_shared_base_qty(
    raw_qty: Decimal,
    arcus_step: Decimal,
    hl_sz_decimals: u32,
) -> Result<Decimal> {
    ensure!(
        raw_qty > Decimal::ZERO && arcus_step > Decimal::ZERO && hl_sz_decimals <= 6,
        "invalid quantization parameters"
    );
    let hl_step = Decimal::new(1, hl_sz_decimals);
    let coarse_step = arcus_step.max(hl_step);
    let units = (raw_qty / coarse_step)
        .round_dp_with_strategy(0, RoundingStrategy::ToZero)
        .max(Decimal::ZERO);
    let q_arc = (units * coarse_step)
        .round_dp_with_strategy(hl_sz_decimals, RoundingStrategy::ToZero)
        .normalize();
    // Ensure remaining integer multiples of arcus_step if hl_step < arcus_step.
    let arc_units = (q_arc / arcus_step).round_dp_with_strategy(0, RoundingStrategy::ToZero);
    Ok((arc_units * arcus_step).normalize())
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
    pub leung_li_blocked: u64,
    pub persist_reset_count: u64,
    pub brake_scaled_count: u64,
}

#[derive(Debug)]
pub struct ArcusXyzBasisEngine {
    config: ArcusXyzBasisConfig,
    inv_arc: Decimal,
    inv_xyz: Decimal,
    in_flight: bool,
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
            inv_arc: Decimal::ZERO,
            inv_xyz: Decimal::ZERO,
            in_flight: false,
            entry_open_ms: None,
            last_trade_arc_rx_ms: 0,
            last_trade_xyz_rx_ms: 0,
            armed_dir: None,
            armed_since_ms: 0,
            dust_qty: Decimal::new(1, 6),
            counters: BasisEngineCounters::default(),
        })
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
        if arc.bid_px <= Decimal::ZERO
            || arc.ask_px <= arc.bid_px
            || arc.bid_sz <= Decimal::ZERO
            || arc.ask_sz <= Decimal::ZERO
        {
            self.reset_persist();
            return BasisAction::Hold {
                reason: "invalid_arcus_book",
            };
        }
        if now_ms.saturating_sub(arc.receipt_ms) > self.config.max_arcus_book_age_ms {
            self.reset_persist();
            return BasisAction::Hold {
                reason: "stale_arcus_book",
            };
        }
        if now_ms.saturating_sub(xyz_rx_ms) > self.config.max_hl_book_age_ms {
            self.reset_persist();
            return BasisAction::Hold {
                reason: "stale_xyz_book",
            };
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
            let visible = xyz.visible_size_within_limit(is_buy, limit_price);
            let qty = unhedged.abs().min(visible);
            if qty <= self.dust_qty {
                return BasisAction::Hold {
                    reason: "thin_xyz_hedge_book",
                };
            }
            let reduce_only = (is_buy && self.inv_xyz < Decimal::ZERO && self.inv_xyz.abs() >= qty)
                || (!is_buy && self.inv_xyz > Decimal::ZERO && self.inv_xyz >= qty);
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
                let max_close = self.inv_arc.abs().min(arc_vis).min(xyz_vis);
                if max_close <= self.dust_qty {
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

        let pos_usd = self.inv_arc.abs().to_f64().unwrap_or(f64::MAX) * am;
        let remaining_usd = self.config.max_notional_usd - pos_usd;
        if remaining_usd < 5.0 {
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
        if eff_clip_usd < 5.0 {
            self.reset_persist();
            return BasisAction::Hold {
                reason: "braked_below_min_notional",
            };
        }
        let Some(raw_qty) = decimal_from_f64(eff_clip_usd / am) else {
            return BasisAction::Hold {
                reason: "qty_overflow",
            };
        };

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
        let qty = raw_qty.min(arc_vis).min(xyz_vis);
        let notional_usd = qty.to_f64().unwrap_or(0.0) * am;
        if qty <= self.dust_qty || notional_usd < 5.0 {
            return BasisAction::Hold {
                reason: "thin_entry_book",
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
    IntentTerminal {
        intent_id: String,
        status: String,
        #[serde(with = "rust_decimal::serde::str")]
        inv_arc: Decimal,
        #[serde(with = "rust_decimal::serde::str")]
        inv_xyz: Decimal,
        journal_ms: u64,
    },
    CleanShutdown {
        journal_ms: u64,
    },
}

/// Fail-closed intent durability log: every dispatch writes & fsyncs `IntentOpen` first.
#[derive(Debug, Clone)]
pub struct BasisJournal {
    path: PathBuf,
}

impl BasisJournal {
    pub fn open_and_check_restart(path: PathBuf) -> Result<Self> {
        Self::check_clean_restart(&path)?;
        Ok(Self { path })
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
                BasisJournalEvent::IntentTerminal { intent_id, .. } => {
                    pending.remove(&intent_id);
                }
                BasisJournalEvent::SessionStart { .. }
                | BasisJournalEvent::CleanShutdown { .. } => {}
            }
        }
        ensure!(
            pending.is_empty(),
            "unresolved intents in {} ({:?}); reconcile both venues and clear journal before restart",
            path.display(),
            pending
        );
        Ok(())
    }

    pub fn append(&self, event: &BasisJournalEvent) -> Result<()> {
        if let Some(parent) = self.path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }
        let mut opts = OpenOptions::new();
        opts.create(true).append(true);
        #[cfg(unix)]
        opts.mode(0o600);
        let mut file = opts
            .open(&self.path)
            .with_context(|| format!("open journal {} for append", self.path.display()))?;
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
            max_hl_book_age_ms: 30_000,
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
        assert!(BasisJournal::open_and_check_restart(path.clone()).is_err());

        journal
            .append(&BasisJournalEvent::IntentTerminal {
                intent_id: "i-1".into(),
                status: "reconciled".into(),
                inv_arc: d("0.5"),
                inv_xyz: d("-0.5"),
                journal_ms: 1_200,
            })
            .unwrap();
        assert!(BasisJournal::open_and_check_restart(path.clone()).is_ok());

        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
