//! Decoupled Track 01 (`01_quant_profit`) and Track 02 (`02_hedge_arbitrage`) shadow sessions.
//!
//! Strictly isolated from exchange execution adapters (`crate::exchanges::*` is forbidden here)
//! and strictly isolated from each other: separate configurations, runtime state,
//! [`DailyLossGuard`] circuit breakers, and reports (`AGENTS.md` non-cross-subsidy contract).

use crate::daily_loss::DailyLossGuard;
use crate::strategy::basis_maker_hedge::{
    FillSide, MakerHedgeQuoter, MakerHedgeQuoterConfig, MakerHedgeQuotes, ShieldedPairEconomics,
};
use crate::strategy::lead_lag_taker::{
    LeadLagAction, LeadLagSide, LeadLagTaker, LeadLagTakerConfig,
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

pub use crate::strategy::basis_maker_hedge::Touch as MakerTouch;

const UNIX_EPOCH_CE_DAY: i32 = 719_163;
const REF_RING_CAP: usize = 64;

/// Convert Unix epoch milliseconds to UTC days since CE (matching `chrono::Datelike::num_days_from_ce`).
pub fn utc_day_from_ms(now_ms: u64) -> i32 {
    let days_since_unix = (now_ms / 86_400_000).min((i32::MAX - UNIX_EPOCH_CE_DAY) as u64) as i32;
    UNIX_EPOCH_CE_DAY + days_since_unix
}

fn valid_bid_ask(bid: f64, ask: f64) -> bool {
    bid.is_finite() && ask.is_finite() && bid > 0.0 && ask > bid
}

fn default_max_l1_depth_fraction() -> f64 {
    0.50
}

fn default_max_hedge_stale_ms() -> u64 {
    5_000
}

fn default_true() -> bool {
    true
}

// ============================================================================
// Track 01: 01_quant_profit (Cross-Venue Lead-Lag Taker Shadow)
// ============================================================================

/// Top-of-book snapshot with local receipt time for Track 01 lead-lag shadow replay.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
pub struct LeadLagTouch {
    pub bid: f64,
    pub ask: f64,
    pub recv_ms: u64,
}

impl LeadLagTouch {
    pub fn valid(&self) -> bool {
        valid_bid_ask(self.bid, self.ask)
    }

    pub fn mid(&self) -> f64 {
        0.5 * (self.bid + self.ask)
    }
}

/// Shadow configuration for a single Track 01 lead-lag arm.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Track01ArmShadowConfig {
    pub symbol: String,
    pub symbol_id: u16,
    pub ref_exchange_id: u8,
    pub tgt_exchange_id: u8,
    pub clip_usd: f64,
    #[serde(default = "default_max_l1_depth_fraction")]
    pub max_l1_depth_fraction: f64,
    pub taker_fee_bps: f64,
    pub strategy: LeadLagTakerConfig,
}

impl Default for Track01ArmShadowConfig {
    fn default() -> Self {
        Self {
            symbol: String::new(),
            symbol_id: 0,
            ref_exchange_id: 6,
            tgt_exchange_id: 2,
            clip_usd: 250.0,
            max_l1_depth_fraction: default_max_l1_depth_fraction(),
            taker_fee_bps: 0.0,
            strategy: LeadLagTakerConfig::default(),
        }
    }
}

/// Isolated shadow session configuration for Track 01 (`01_quant_profit`).
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Track01ShadowConfig {
    pub capital_usd: f64,
    pub daily_loss_limit_pct: f64,
    pub daily_loss_state_path: Option<PathBuf>,
    pub allowlisted_symbols: Vec<String>,
    pub arms: Vec<Track01ArmShadowConfig>,
}

impl Default for Track01ShadowConfig {
    fn default() -> Self {
        Self {
            capital_usd: 10_000.0,
            daily_loss_limit_pct: 0.02,
            daily_loss_state_path: None,
            allowlisted_symbols: Vec::new(),
            arms: Vec::new(),
        }
    }
}

impl Track01ShadowConfig {
    /// Validate capital, loss limits, allowlist membership, unique symbol IDs, and arm strategies.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.capital_usd.is_finite() && self.capital_usd > 0.0,
            "capital_usd must be finite and > 0"
        );
        ensure!(
            self.daily_loss_limit_pct.is_finite()
                && self.daily_loss_limit_pct > 0.0
                && self.daily_loss_limit_pct < 1.0,
            "daily_loss_limit_pct must be finite and in (0, 1)"
        );
        ensure!(
            !self.allowlisted_symbols.is_empty(),
            "allowlisted_symbols must be non-empty"
        );
        for sym in &self.allowlisted_symbols {
            ensure!(
                !sym.trim().is_empty(),
                "allowlisted_symbols must not contain blank entries"
            );
        }
        ensure!(!self.arms.is_empty(), "arms must be non-empty");

        let mut seen_symbol_ids = HashSet::with_capacity(self.arms.len());
        for arm in &self.arms {
            ensure!(
                !arm.symbol.trim().is_empty(),
                "arm.symbol must be non-empty"
            );
            ensure!(
                self.allowlisted_symbols.iter().any(|s| s == &arm.symbol),
                "arm symbol '{}' is not in allowlisted_symbols",
                arm.symbol
            );
            ensure!(
                seen_symbol_ids.insert(arm.symbol_id),
                "duplicate arm symbol_id {}",
                arm.symbol_id
            );
            ensure!(
                arm.ref_exchange_id != arm.tgt_exchange_id,
                "arm '{}' ref_exchange_id and tgt_exchange_id must differ",
                arm.symbol
            );
            ensure!(
                arm.clip_usd.is_finite() && arm.clip_usd > 0.0,
                "arm '{}' clip_usd must be finite and > 0",
                arm.symbol
            );
            ensure!(
                arm.max_l1_depth_fraction.is_finite()
                    && arm.max_l1_depth_fraction > 0.0
                    && arm.max_l1_depth_fraction <= 1.0,
                "arm '{}' max_l1_depth_fraction must be in (0, 1]",
                arm.symbol
            );
            ensure!(
                arm.taker_fee_bps.is_finite() && arm.taker_fee_bps >= 0.0,
                "arm '{}' taker_fee_bps must be finite and >= 0",
                arm.symbol
            );
            arm.strategy.validate()?;
        }
        Ok(())
    }
}

/// Active shadow position held by a Track 01 arm.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OpenShadowPos {
    pub side: LeadLagSide,
    pub qty: f64,
    pub entry_px: f64,
    pub notional_usd: f64,
    entry_fee_usd: f64,
}

struct Track01ArmRuntime {
    cfg: Track01ArmShadowConfig,
    engine: LeadLagTaker,
    open_pos: Option<OpenShadowPos>,
    last_tgt_touch: Option<LeadLagTouch>,
    realized_pnl_usd: f64,
    total_fees_usd: f64,
    trades: u64,
    wins: u64,
    skips: u64,
    blocks: u64,
}

impl Track01ArmRuntime {
    fn open_mtm_usd(&self) -> f64 {
        let (Some(pos), Some(touch)) = (self.open_pos, self.last_tgt_touch) else {
            return 0.0;
        };
        let exit_px = match pos.side {
            LeadLagSide::Long => touch.bid,
            LeadLagSide::Short => touch.ask,
        };
        pos.side.sign() * (exit_px - pos.entry_px) * pos.qty
    }

    fn enter(&mut self, side: LeadLagSide, tgt_touch: LeadLagTouch, notional: f64) {
        let entry_px = match side {
            LeadLagSide::Long => tgt_touch.ask,
            LeadLagSide::Short => tgt_touch.bid,
        };
        let qty = notional / entry_px;
        let entry_fee = notional * self.cfg.taker_fee_bps * 1e-4;
        self.realized_pnl_usd -= entry_fee;
        self.total_fees_usd += entry_fee;
        self.open_pos = Some(OpenShadowPos {
            side,
            qty,
            entry_px,
            notional_usd: notional,
            entry_fee_usd: entry_fee,
        });
    }

    fn exit(&mut self, tgt_touch: LeadLagTouch) {
        let Some(pos) = self.open_pos.take() else {
            return;
        };
        let exit_px = match pos.side {
            LeadLagSide::Long => tgt_touch.bid,
            LeadLagSide::Short => tgt_touch.ask,
        };
        let exit_notional = pos.qty * exit_px;
        let exit_fee = exit_notional * self.cfg.taker_fee_bps * 1e-4;
        let gross_pnl = pos.side.sign() * (exit_px - pos.entry_px) * pos.qty;
        let net_trade_pnl = gross_pnl - pos.entry_fee_usd - exit_fee;
        self.realized_pnl_usd += gross_pnl - exit_fee;
        self.total_fees_usd += exit_fee;
        self.trades += 1;
        if net_trade_pnl > 0.0 {
            self.wins += 1;
        }
    }
}

/// Per-arm shadow summary for Track 01.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Track01ArmReport {
    pub symbol: String,
    pub symbol_id: u16,
    pub ref_exchange_id: u8,
    pub tgt_exchange_id: u8,
    pub realized_pnl_usd: f64,
    pub unrealized_pnl_usd: f64,
    pub total_fees_usd: f64,
    pub trades: u64,
    pub wins: u64,
    pub skips: u64,
    pub blocks: u64,
    pub open_side: Option<LeadLagSide>,
    pub open_notional_usd: f64,
}

/// Aggregate shadow summary for Track 01 (`01_quant_profit`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Track01ShadowReport {
    pub track: &'static str,
    pub capital_usd: f64,
    pub equity_usd: f64,
    pub realized_pnl_usd: f64,
    pub unrealized_pnl_usd: f64,
    pub total_fees_usd: f64,
    pub max_drawdown_usd: f64,
    pub trades: u64,
    pub wins: u64,
    pub skips: u64,
    pub blocks: u64,
    pub risk_scale: f64,
    pub halted: bool,
    pub arms: Vec<Track01ArmReport>,
}

/// Decoupled Track 01 lead-lag taker shadow session with isolated [`DailyLossGuard`].
pub struct Track01ShadowSession {
    cfg: Track01ShadowConfig,
    arms: Vec<Track01ArmRuntime>,
    arm_by_symbol_id: HashMap<u16, usize>,
    daily_loss: DailyLossGuard,
    halted: bool,
    peak_equity_usd: f64,
    max_drawdown_usd: f64,
}

impl Track01ShadowSession {
    pub fn new(cfg: Track01ShadowConfig, initial_utc_day: i32) -> Result<Self> {
        cfg.validate()?;
        let daily_loss = DailyLossGuard::open(
            cfg.daily_loss_state_path.clone(),
            cfg.daily_loss_limit_pct,
            cfg.capital_usd,
            initial_utc_day,
        )?;
        let mut arms = Vec::with_capacity(cfg.arms.len());
        let mut arm_by_symbol_id = HashMap::with_capacity(cfg.arms.len());
        for (idx, arm_cfg) in cfg.arms.iter().enumerate() {
            let engine = LeadLagTaker::new(arm_cfg.strategy)?;
            arm_by_symbol_id.insert(arm_cfg.symbol_id, idx);
            arms.push(Track01ArmRuntime {
                cfg: arm_cfg.clone(),
                engine,
                open_pos: None,
                last_tgt_touch: None,
                realized_pnl_usd: 0.0,
                total_fees_usd: 0.0,
                trades: 0,
                wins: 0,
                skips: 0,
                blocks: 0,
            });
        }
        Ok(Self {
            peak_equity_usd: cfg.capital_usd,
            cfg,
            arms,
            arm_by_symbol_id,
            daily_loss,
            halted: false,
            max_drawdown_usd: 0.0,
        })
    }

    pub fn config(&self) -> &Track01ShadowConfig {
        &self.cfg
    }

    pub fn is_halted(&self) -> bool {
        self.halted
    }

    pub fn arm_ids(&self) -> impl Iterator<Item = (u16, u8, u8)> + '_ {
        self.arms.iter().map(|a| {
            (
                a.cfg.symbol_id,
                a.cfg.ref_exchange_id,
                a.cfg.tgt_exchange_id,
            )
        })
    }

    fn equity_usd(&self) -> f64 {
        let pnl: f64 = self
            .arms
            .iter()
            .map(|a| a.realized_pnl_usd + a.open_mtm_usd())
            .sum();
        self.cfg.capital_usd + pnl
    }

    fn check_equity_and_drawdown(&mut self, utc_day: i32) {
        let equity = self.equity_usd();
        if equity.is_finite() {
            self.peak_equity_usd = self.peak_equity_usd.max(equity);
            self.max_drawdown_usd = self.max_drawdown_usd.max(self.peak_equity_usd - equity);
        }
        if !self.halted && self.daily_loss.check(equity, utc_day).is_err() {
            self.halted = true;
        }
    }

    /// Process one synchronized reference/target BBO pair for `symbol_id`.
    #[allow(clippy::too_many_arguments)]
    pub fn on_bbo_pair(
        &mut self,
        symbol_id: u16,
        now_ms: u64,
        ref_touch: LeadLagTouch,
        tgt_touch: LeadLagTouch,
        tgt_bid_sz: f64,
        tgt_ask_sz: f64,
        utc_day: i32,
    ) -> Result<Option<LeadLagAction>> {
        let Some(&arm_idx) = self.arm_by_symbol_id.get(&symbol_id) else {
            return Ok(None);
        };
        if tgt_touch.valid() {
            self.arms[arm_idx].last_tgt_touch = Some(tgt_touch);
        }
        // Check MTM equity & daily loss brake before sizing any new entry.
        self.check_equity_and_drawdown(utc_day);

        let ref_mid = if ref_touch.valid() {
            ref_touch.mid()
        } else {
            f64::NAN
        };
        let tgt_mid = if tgt_touch.valid() {
            tgt_touch.mid()
        } else {
            f64::NAN
        };
        let action = self.arms[arm_idx].engine.observe(
            now_ms,
            ref_mid,
            ref_touch.recv_ms,
            tgt_mid,
            tgt_touch.recv_ms,
        );

        let risk_scale = if self.halted {
            0.0
        } else {
            self.daily_loss.risk_scale()
        };
        let arm = &mut self.arms[arm_idx];
        let mut emitted = action;
        match action {
            LeadLagAction::Enter { side, .. } => {
                let entry_l1_usd = match side {
                    LeadLagSide::Long => tgt_touch.ask * tgt_ask_sz,
                    LeadLagSide::Short => tgt_touch.bid * tgt_bid_sz,
                };
                let capped = arm
                    .cfg
                    .clip_usd
                    .min(arm.cfg.max_l1_depth_fraction * entry_l1_usd);
                let notional = capped * risk_scale;
                if self.halted
                    || !tgt_touch.valid()
                    || !entry_l1_usd.is_finite()
                    || entry_l1_usd <= 0.0
                    || !notional.is_finite()
                    || notional <= 0.0
                {
                    // Synchronize decision state so skipped entries never trigger future phantom exits.
                    arm.engine.clear_position();
                    arm.skips += 1;
                    emitted = LeadLagAction::Idle;
                } else {
                    arm.enter(side, tgt_touch, notional);
                }
            }
            LeadLagAction::Exit { .. } => {
                if tgt_touch.valid() {
                    arm.exit(tgt_touch);
                    // Shadow execution immediately confirms flat
                    arm.engine.clear_position();
                } else if arm.open_pos.is_none() {
                    // Resynchronize cleanly if somehow out of sync with no shadow position open.
                    arm.engine.clear_position();
                }
            }
            LeadLagAction::Blocked(_) => {
                arm.blocks += 1;
            }
            LeadLagAction::Idle | LeadLagAction::QuotePassiveExit { .. } => {}
        }

        self.check_equity_and_drawdown(utc_day);
        Ok(Some(emitted))
    }

    pub fn report(&self) -> Track01ShadowReport {
        let mut realized = 0.0;
        let mut unrealized = 0.0;
        let mut fees = 0.0;
        let mut trades = 0;
        let mut wins = 0;
        let mut skips = 0;
        let mut blocks = 0;
        let arms = self
            .arms
            .iter()
            .map(|a| {
                let mtm = a.open_mtm_usd();
                realized += a.realized_pnl_usd;
                unrealized += mtm;
                fees += a.total_fees_usd;
                trades += a.trades;
                wins += a.wins;
                skips += a.skips;
                blocks += a.blocks;
                Track01ArmReport {
                    symbol: a.cfg.symbol.clone(),
                    symbol_id: a.cfg.symbol_id,
                    ref_exchange_id: a.cfg.ref_exchange_id,
                    tgt_exchange_id: a.cfg.tgt_exchange_id,
                    realized_pnl_usd: a.realized_pnl_usd,
                    unrealized_pnl_usd: mtm,
                    total_fees_usd: a.total_fees_usd,
                    trades: a.trades,
                    wins: a.wins,
                    skips: a.skips,
                    blocks: a.blocks,
                    open_side: a.open_pos.map(|p| p.side),
                    open_notional_usd: a.open_pos.map_or(0.0, |p| p.notional_usd),
                }
            })
            .collect();
        Track01ShadowReport {
            track: "01_quant_profit",
            capital_usd: self.cfg.capital_usd,
            equity_usd: self.cfg.capital_usd + realized + unrealized,
            realized_pnl_usd: realized,
            unrealized_pnl_usd: unrealized,
            total_fees_usd: fees,
            max_drawdown_usd: self.max_drawdown_usd,
            trades,
            wins,
            skips,
            blocks,
            risk_scale: if self.halted {
                0.0
            } else {
                self.daily_loss.risk_scale()
            },
            halted: self.halted,
            arms,
        }
    }
}

// ============================================================================
// Track 02: 02_hedge_arbitrage (Shielded Maker-Hedge Shadow)
// ============================================================================

/// Shadow configuration for a single Track 02 shielded maker-hedge pair.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Track02PairShadowConfig {
    pub label: String,
    pub symbol_id: u16,
    pub maker_exchange_id: u8,
    pub hedge_exchange_id: u8,
    pub ref_exchange_id: u8,
    pub clip_usd: f64,
    pub expected_gross_edge_bps: f64,
    pub min_net_edge_bps: f64,
    #[serde(default = "default_max_hedge_stale_ms")]
    pub max_hedge_stale_ms: u64,
    #[serde(default = "default_true")]
    pub require_fail_closed_shield: bool,
    pub economics: ShieldedPairEconomics,
    pub strategy: MakerHedgeQuoterConfig,
}

impl Default for Track02PairShadowConfig {
    fn default() -> Self {
        Self {
            label: String::new(),
            symbol_id: 0,
            maker_exchange_id: 2,
            hedge_exchange_id: 7,
            ref_exchange_id: 6,
            clip_usd: 500.0,
            expected_gross_edge_bps: 6.0,
            min_net_edge_bps: 0.5,
            max_hedge_stale_ms: default_max_hedge_stale_ms(),
            require_fail_closed_shield: default_true(),
            economics: ShieldedPairEconomics::default(),
            strategy: MakerHedgeQuoterConfig::default(),
        }
    }
}

/// Isolated shadow session configuration for Track 02 (`02_hedge_arbitrage`).
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Track02ShadowConfig {
    pub capital_usd: f64,
    pub daily_loss_limit_pct: f64,
    pub daily_loss_state_path: Option<PathBuf>,
    pub pairs: Vec<Track02PairShadowConfig>,
}

impl Default for Track02ShadowConfig {
    fn default() -> Self {
        Self {
            capital_usd: 20_000.0,
            daily_loss_limit_pct: 0.02,
            daily_loss_state_path: None,
            pairs: Vec::new(),
        }
    }
}

impl Track02ShadowConfig {
    /// Validate capital, loss limits, fail-closed shield enforcement, and pair economics.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.capital_usd.is_finite() && self.capital_usd > 0.0,
            "capital_usd must be finite and > 0"
        );
        ensure!(
            self.daily_loss_limit_pct.is_finite()
                && self.daily_loss_limit_pct > 0.0
                && self.daily_loss_limit_pct < 1.0,
            "daily_loss_limit_pct must be finite and in (0, 1)"
        );
        ensure!(!self.pairs.is_empty(), "pairs must be non-empty");

        let mut seen_symbol_ids = HashSet::with_capacity(self.pairs.len());
        for pair in &self.pairs {
            ensure!(
                !pair.label.trim().is_empty(),
                "pair.label must be non-empty"
            );
            ensure!(
                seen_symbol_ids.insert(pair.symbol_id),
                "duplicate pair symbol_id {}",
                pair.symbol_id
            );
            ensure!(
                pair.maker_exchange_id != pair.hedge_exchange_id,
                "pair '{}' maker_exchange_id and hedge_exchange_id must differ",
                pair.label
            );
            ensure!(
                pair.clip_usd.is_finite() && pair.clip_usd > 0.0,
                "pair '{}' clip_usd must be finite and > 0",
                pair.label
            );
            ensure!(
                pair.max_hedge_stale_ms > 0,
                "pair '{}' max_hedge_stale_ms must be > 0",
                pair.label
            );
            ensure!(
                pair.expected_gross_edge_bps.is_finite() && pair.expected_gross_edge_bps >= 0.0,
                "pair '{}' expected_gross_edge_bps must be finite and >= 0",
                pair.label
            );
            ensure!(
                pair.min_net_edge_bps.is_finite(),
                "pair '{}' min_net_edge_bps must be finite",
                pair.label
            );
            pair.strategy.validate()?;
            if pair.require_fail_closed_shield {
                ensure!(
                    pair.strategy.ref_shield.enabled
                        && pair.strategy.ref_shield.fail_closed_when_unready,
                    "pair '{}' requires ref_shield.enabled && ref_shield.fail_closed_when_unready",
                    pair.label
                );
            }
            ensure!(
                pair.economics
                    .is_economically_viable(pair.expected_gross_edge_bps, pair.min_net_edge_bps),
                "pair '{}' failed economic viability (same_risk_domain={} net_edge={:.3} bp < min {:.3} bp)",
                pair.label,
                pair.economics.same_risk_domain,
                pair.economics
                    .net_expected_edge_bps(pair.expected_gross_edge_bps),
                pair.min_net_edge_bps
            );
        }
        Ok(())
    }
}

/// Average-cost signed position tracker for a single leg (`+` = long, `-` = short).
#[derive(Debug, Clone, Copy, Default)]
struct LegPosition {
    qty: f64,
    avg_entry_px: f64,
}

impl LegPosition {
    /// Apply signed `delta_qty` at `fill_px` and return realized PnL from any closed portion.
    fn apply_fill(&mut self, delta_qty: f64, fill_px: f64) -> f64 {
        const EPS: f64 = 1e-12;
        if delta_qty.abs() <= EPS {
            return 0.0;
        }
        if self.qty.abs() <= EPS || self.qty.signum() == delta_qty.signum() {
            let new_qty = self.qty + delta_qty;
            self.avg_entry_px =
                (self.qty.abs() * self.avg_entry_px + delta_qty.abs() * fill_px) / new_qty.abs();
            self.qty = new_qty;
            return 0.0;
        }
        let closed_abs = self.qty.abs().min(delta_qty.abs());
        let realized = self.qty.signum() * closed_abs * (fill_px - self.avg_entry_px);
        let new_qty = self.qty + delta_qty;
        if new_qty.abs() <= EPS {
            self.qty = 0.0;
            self.avg_entry_px = 0.0;
        } else if new_qty.signum() != self.qty.signum() {
            self.qty = new_qty;
            self.avg_entry_px = fill_px;
        } else {
            self.qty = new_qty;
        }
        realized
    }

    fn unrealized_pnl(&self, mark_px: f64) -> f64 {
        if self.qty == 0.0 {
            0.0
        } else {
            self.qty * (mark_px - self.avg_entry_px)
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct RefSample {
    ts_ms: u64,
    mid: f64,
}

/// Stack-allocated mirror of [`MakerHedgeQuoter`] reference window for exact shield accounting.
#[derive(Debug, Clone)]
struct RefShieldTracker {
    ring: [RefSample; REF_RING_CAP],
    head: usize,
    len: usize,
}

impl Default for RefShieldTracker {
    fn default() -> Self {
        Self {
            ring: [RefSample::default(); REF_RING_CAP],
            head: 0,
            len: 0,
        }
    }
}

impl RefShieldTracker {
    fn observe(&mut self, now_ms: u64, ref_mid: f64, max_gap_ms: u64) {
        if !(ref_mid.is_finite() && ref_mid > 0.0) {
            return;
        }
        if self.len > 0 {
            let last_idx = (self.head + REF_RING_CAP - 1) % REF_RING_CAP;
            let last_ts = self.ring[last_idx].ts_ms;
            if now_ms < last_ts {
                return;
            }
            if now_ms.saturating_sub(last_ts) > max_gap_ms {
                self.head = 0;
                self.len = 0;
            }
        }
        self.ring[self.head] = RefSample {
            ts_ms: now_ms,
            mid: ref_mid,
        };
        self.head = (self.head + 1) % REF_RING_CAP;
        self.len = (self.len + 1).min(REF_RING_CAP);
    }

    fn return_bps(&self, now_ms: u64, cfg: &MakerHedgeQuoterConfig) -> Option<f64> {
        if !cfg.ref_shield.enabled || self.len < 2 {
            return None;
        }
        let newest_idx = (self.head + REF_RING_CAP - 1) % REF_RING_CAP;
        let newest = self.ring[newest_idx];
        if newest.ts_ms > now_ms || now_ms - newest.ts_ms > cfg.max_book_age_ms {
            return None;
        }
        let cutoff = now_ms.saturating_sub(cfg.ref_shield.lookback_ms);
        let mut base = None;
        for offset in 0..self.len {
            let idx = (self.head + REF_RING_CAP - 1 - offset) % REF_RING_CAP;
            let pt = self.ring[idx];
            if pt.ts_ms > now_ms || now_ms - pt.ts_ms > cfg.max_book_age_ms {
                break;
            }
            base = Some(pt);
            if pt.ts_ms <= cutoff {
                break;
            }
        }
        let base = base?;
        if newest.ts_ms <= base.ts_ms || !base.mid.is_finite() || base.mid <= 0.0 {
            return None;
        }
        Some((newest.mid / base.mid - 1.0) * 1e4)
    }

    fn is_blocking_any_side(&self, now_ms: u64, cfg: &MakerHedgeQuoterConfig, q: f64) -> bool {
        if !cfg.ref_shield.enabled {
            return false;
        }
        let (raw_bid, raw_ask) = match self.return_bps(now_ms, cfg) {
            Some(r) => (
                r <= -cfg.ref_shield.adverse_move_bps,
                r >= cfg.ref_shield.adverse_move_bps,
            ),
            None if cfg.ref_shield.fail_closed_when_unready => (true, true),
            None => (false, false),
        };
        let block_bid = raw_bid && !(cfg.ref_shield.allow_reduce_only_through_shield && q < 0.0);
        let block_ask = raw_ask && !(cfg.ref_shield.allow_reduce_only_through_shield && q > 0.0);
        block_bid || block_ask
    }
}

struct Track02PairRuntime {
    cfg: Track02PairShadowConfig,
    quoter: MakerHedgeQuoter,
    ref_tracker: RefShieldTracker,
    latest_quotes: Option<MakerHedgeQuotes>,
    last_maker_touch: Option<MakerTouch>,
    last_hedge_touch: Option<MakerTouch>,
    maker_leg: LegPosition,
    hedge_leg: LegPosition,
    inventory_clips: f64,
    realized_pnl_usd: f64,
    total_fees_usd: f64,
    total_modeled_hedge_slip_usd: f64,
    unhedged_maker_notional_usd: f64,
    fills: u64,
    shield_blocked_ticks: u64,
    unhedged_stale_halts: u64,
    halted: bool,
}

impl Track02PairRuntime {
    fn unrealized_pnl_usd(&self) -> f64 {
        let maker_mtm = self
            .last_maker_touch
            .map_or(0.0, |t| self.maker_leg.unrealized_pnl(t.mid()));
        let hedge_mtm = self
            .last_hedge_touch
            .map_or(0.0, |t| self.hedge_leg.unrealized_pnl(t.mid()));
        maker_mtm + hedge_mtm
    }

    fn total_pnl_usd(&self) -> f64 {
        self.realized_pnl_usd + self.unrealized_pnl_usd()
    }
}

/// Per-pair shadow summary for Track 02.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Track02PairReport {
    pub label: String,
    pub symbol_id: u16,
    pub maker_exchange_id: u8,
    pub hedge_exchange_id: u8,
    pub ref_exchange_id: u8,
    pub realized_pnl_usd: f64,
    pub unrealized_pnl_usd: f64,
    pub total_fees_usd: f64,
    pub total_modeled_hedge_slip_usd: f64,
    pub unhedged_maker_notional_usd: f64,
    pub maker_qty: f64,
    pub hedge_qty: f64,
    pub net_unhedged_base_qty: f64,
    pub inventory_clips: f64,
    pub fills: u64,
    pub shield_blocked_ticks: u64,
    pub unhedged_stale_halts: u64,
    pub halted: bool,
}

/// Aggregate shadow summary for Track 02 (`02_hedge_arbitrage`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Track02ShadowReport {
    pub track: &'static str,
    pub fill_evidence_mode: &'static str,
    pub fill_evidence_note: &'static str,
    pub hedge_slip_mode: &'static str,
    pub capital_usd: f64,
    pub equity_usd: f64,
    pub realized_pnl_usd: f64,
    pub unrealized_pnl_usd: f64,
    pub total_fees_usd: f64,
    pub total_modeled_hedge_slip_usd: f64,
    pub unhedged_maker_notional_usd: f64,
    pub max_drawdown_usd: f64,
    pub fills: u64,
    pub shield_blocked_ticks: u64,
    pub unhedged_stale_halts: u64,
    pub risk_scale: f64,
    pub halted: bool,
    pub pairs: Vec<Track02PairReport>,
}

/// Decoupled Track 02 shielded maker-hedge shadow session with isolated [`DailyLossGuard`].
pub struct Track02ShadowSession {
    cfg: Track02ShadowConfig,
    pairs: Vec<Track02PairRuntime>,
    pair_by_symbol_id: HashMap<u16, usize>,
    daily_loss: DailyLossGuard,
    fill_evidence_mode: &'static str,
    halted: bool,
    peak_equity_usd: f64,
    max_drawdown_usd: f64,
}

impl Track02ShadowSession {
    pub fn new(cfg: Track02ShadowConfig, initial_utc_day: i32) -> Result<Self> {
        cfg.validate()?;
        let daily_loss = DailyLossGuard::open(
            cfg.daily_loss_state_path.clone(),
            cfg.daily_loss_limit_pct,
            cfg.capital_usd,
            initial_utc_day,
        )?;
        let mut pairs = Vec::with_capacity(cfg.pairs.len());
        let mut pair_by_symbol_id = HashMap::with_capacity(cfg.pairs.len());
        for (idx, pair_cfg) in cfg.pairs.iter().enumerate() {
            let quoter = MakerHedgeQuoter::new(pair_cfg.strategy)?;
            pair_by_symbol_id.insert(pair_cfg.symbol_id, idx);
            pairs.push(Track02PairRuntime {
                cfg: pair_cfg.clone(),
                quoter,
                ref_tracker: RefShieldTracker::default(),
                latest_quotes: None,
                last_maker_touch: None,
                last_hedge_touch: None,
                maker_leg: LegPosition::default(),
                hedge_leg: LegPosition::default(),
                inventory_clips: 0.0,
                realized_pnl_usd: 0.0,
                total_fees_usd: 0.0,
                total_modeled_hedge_slip_usd: 0.0,
                unhedged_maker_notional_usd: 0.0,
                fills: 0,
                shield_blocked_ticks: 0,
                unhedged_stale_halts: 0,
                halted: false,
            });
        }
        Ok(Self {
            peak_equity_usd: cfg.capital_usd,
            cfg,
            pairs,
            pair_by_symbol_id,
            daily_loss,
            fill_evidence_mode: "public_trade_prints",
            halted: false,
            max_drawdown_usd: 0.0,
        })
    }

    pub fn set_fill_evidence_mode(&mut self, mode: &'static str) {
        self.fill_evidence_mode = mode;
    }

    pub fn fill_evidence_note(&self) -> &'static str {
        if self.fill_evidence_mode == "shm_bbo_cross_diagnostic_no_trades" {
            "BboMatrix SHM has no public trade prints; Track 02 fills are conservative BBO-cross sweep diagnostics only"
        } else {
            "trade_print_strict_trade_through"
        }
    }

    pub fn config(&self) -> &Track02ShadowConfig {
        &self.cfg
    }

    pub fn is_halted(&self) -> bool {
        self.halted
    }

    pub fn pair_ids(&self) -> impl Iterator<Item = (u16, u8, u8, u8)> + '_ {
        self.pairs.iter().map(|p| {
            (
                p.cfg.symbol_id,
                p.cfg.maker_exchange_id,
                p.cfg.hedge_exchange_id,
                p.cfg.ref_exchange_id,
            )
        })
    }

    fn equity_usd(&self) -> f64 {
        let pnl: f64 = self
            .pairs
            .iter()
            .map(Track02PairRuntime::total_pnl_usd)
            .sum();
        self.cfg.capital_usd + pnl
    }

    fn check_equity_and_drawdown(&mut self, utc_day: i32) {
        let equity = self.equity_usd();
        if equity.is_finite() {
            self.peak_equity_usd = self.peak_equity_usd.max(equity);
            self.max_drawdown_usd = self.max_drawdown_usd.max(self.peak_equity_usd - equity);
        }
        if !self.halted && self.daily_loss.check(equity, utc_day).is_err() {
            self.halted = true;
            for pair in &mut self.pairs {
                pair.latest_quotes = None;
            }
        }
    }

    /// Feed an external reference mid price into the pair's cancel shield.
    pub fn on_reference_mid(&mut self, symbol_id: u16, now_ms: u64, ref_mid: f64) {
        let Some(&idx) = self.pair_by_symbol_id.get(&symbol_id) else {
            return;
        };
        let pair = &mut self.pairs[idx];
        pair.quoter.observe_reference(now_ms, ref_mid);
        pair.ref_tracker
            .observe(now_ms, ref_mid, pair.cfg.strategy.max_gap_ms);
    }

    /// Update maker/hedge quotes from the latest books and enforce daily loss limits.
    pub fn on_pair_books(
        &mut self,
        symbol_id: u16,
        now_ms: u64,
        maker: MakerTouch,
        hedge: MakerTouch,
        utc_day: i32,
    ) -> Result<Option<MakerHedgeQuotes>> {
        let Some(&idx) = self.pair_by_symbol_id.get(&symbol_id) else {
            return Ok(None);
        };
        {
            let pair = &mut self.pairs[idx];
            if valid_bid_ask(maker.bid, maker.ask) {
                pair.last_maker_touch = Some(maker);
            }
            if valid_bid_ask(hedge.bid, hedge.ask) {
                pair.last_hedge_touch = Some(hedge);
            }
        }

        let step_res = {
            let pair = &mut self.pairs[idx];
            pair.quoter.step(now_ms, maker, hedge, pair.inventory_clips)
        };
        self.check_equity_and_drawdown(utc_day);

        let pair = &mut self.pairs[idx];
        let Ok(quotes) = step_res else {
            pair.latest_quotes = None;
            return Ok(None);
        };
        if pair
            .ref_tracker
            .is_blocking_any_side(now_ms, &pair.cfg.strategy, pair.inventory_clips)
        {
            pair.shield_blocked_ticks += 1;
        }
        if self.halted || pair.halted {
            pair.latest_quotes = None;
            return Ok(None);
        }
        pair.latest_quotes = Some(quotes);
        Ok(Some(quotes))
    }

    /// Simulate passive fill on strict maker trade-through and immediate hedge against `hedge`.
    #[allow(clippy::too_many_arguments)]
    pub fn on_maker_trade_print(
        &mut self,
        symbol_id: u16,
        now_ms: u64,
        trade_px: f64,
        trade_sz: f64,
        is_buy_aggressor: bool,
        hedge: MakerTouch,
        utc_day: i32,
    ) -> Result<bool> {
        let Some(&idx) = self.pair_by_symbol_id.get(&symbol_id) else {
            return Ok(false);
        };
        if !(trade_px.is_finite() && trade_px > 0.0 && trade_sz.is_finite() && trade_sz > 0.0) {
            return Ok(false);
        }
        if self.halted || self.pairs[idx].halted {
            self.pairs[idx].latest_quotes = None;
            return Ok(false);
        }
        let Some(quotes) = self.pairs[idx].latest_quotes else {
            return Ok(false);
        };
        let hit = if !is_buy_aggressor {
            quotes
                .bid
                .filter(|&bid| trade_px < bid)
                .map(|bid| (FillSide::MakerBought, bid))
        } else {
            quotes
                .ask
                .filter(|&ask| trade_px > ask)
                .map(|ask| (FillSide::MakerSold, ask))
        };
        let Some((fill_side, maker_px)) = hit else {
            return Ok(false);
        };

        let hedge_fresh = valid_bid_ask(hedge.bid, hedge.ask)
            && hedge.recv_ms <= now_ms
            && now_ms - hedge.recv_ms <= self.pairs[idx].cfg.max_hedge_stale_ms;
        if !hedge_fresh {
            let pair_clip = self.pairs[idx].cfg.clip_usd;
            let risk_scale = self.daily_loss.risk_scale();
            let fill_notional = pair_clip.min(trade_px * trade_sz) * risk_scale.max(0.0);
            let did_fill = {
                let pair = &mut self.pairs[idx];
                pair.unhedged_stale_halts += 1;
                pair.halted = true;
                pair.latest_quotes = None;

                if fill_notional.is_finite() && fill_notional > 0.0 {
                    let qty = fill_notional / maker_px;
                    let (signed_maker_qty, signed_clip_delta) = match fill_side {
                        FillSide::MakerBought => (qty, fill_notional / pair_clip),
                        FillSide::MakerSold => (-qty, -(fill_notional / pair_clip)),
                    };
                    let maker_realized = pair.maker_leg.apply_fill(signed_maker_qty, maker_px);
                    let maker_fee = fill_notional * pair.cfg.economics.maker_fee_bps * 1e-4;
                    pair.realized_pnl_usd += maker_realized - maker_fee;
                    pair.total_fees_usd += maker_fee;
                    pair.inventory_clips += signed_clip_delta;
                    pair.fills += 1;
                    pair.unhedged_maker_notional_usd += fill_notional;
                    true
                } else {
                    false
                }
            };
            if did_fill {
                self.check_equity_and_drawdown(utc_day);
                return Ok(true);
            }
            return Ok(false);
        }

        let risk_scale = self.daily_loss.risk_scale();
        if risk_scale <= 0.0 {
            return Ok(false);
        }
        let pair_clip = self.pairs[idx].cfg.clip_usd;
        let fill_notional = pair_clip.min(trade_px * trade_sz) * risk_scale;
        if !fill_notional.is_finite() || fill_notional <= 0.0 {
            return Ok(false);
        }

        let qty = fill_notional / maker_px;
        let econ = self.pairs[idx].cfg.economics;
        let slip_bps = econ.expected_hedge_slip_bps.max(0.0);
        let slip_frac = slip_bps * 1e-4;
        let (signed_maker_qty, hedge_px, raw_hedge_touch_px, signed_clip_delta) = match fill_side {
            FillSide::MakerBought => (
                qty,
                hedge.bid * (1.0 - slip_frac),
                hedge.bid,
                fill_notional / pair_clip,
            ),
            FillSide::MakerSold => (
                -qty,
                hedge.ask * (1.0 + slip_frac),
                hedge.ask,
                -(fill_notional / pair_clip),
            ),
        };

        {
            let pair = &mut self.pairs[idx];
            pair.last_hedge_touch = Some(hedge);
            let maker_realized = pair.maker_leg.apply_fill(signed_maker_qty, maker_px);
            let hedge_realized = pair.hedge_leg.apply_fill(-signed_maker_qty, hedge_px);
            let maker_fee = (qty * maker_px) * econ.maker_fee_bps * 1e-4;
            let hedge_fee_bps = econ.hedge_taker_fee_bps
                + if econ.cross_stablecoin {
                    econ.cross_stablecoin_drag_bps
                } else {
                    0.0
                };
            let hedge_fee = (qty * hedge_px) * hedge_fee_bps * 1e-4;
            let total_fee = maker_fee + hedge_fee;

            pair.realized_pnl_usd += maker_realized + hedge_realized - total_fee;
            pair.total_fees_usd += total_fee;
            pair.total_modeled_hedge_slip_usd += qty * (raw_hedge_touch_px - hedge_px).abs();
            pair.inventory_clips += signed_clip_delta;
            pair.fills += 1;
            if let Some(q) = pair.latest_quotes.as_mut() {
                match fill_side {
                    FillSide::MakerBought => q.bid = None,
                    FillSide::MakerSold => q.ask = None,
                }
            }
        }

        self.check_equity_and_drawdown(utc_day);
        Ok(true)
    }

    /// Diagnostic fill probe for SHM streams lacking public trade prints.
    ///
    /// Triggers a synthetic strict trade-through only when the incoming maker BBO
    /// strictly crosses an active resting quote (`maker.ask < bid` or `maker.bid > ask`).
    #[allow(clippy::too_many_arguments)]
    pub fn on_maker_bbo_cross_diagnostic(
        &mut self,
        symbol_id: u16,
        now_ms: u64,
        maker: MakerTouch,
        maker_bid_sz: f64,
        maker_ask_sz: f64,
        hedge: MakerTouch,
        utc_day: i32,
    ) -> Result<bool> {
        let Some(&idx) = self.pair_by_symbol_id.get(&symbol_id) else {
            return Ok(false);
        };
        if self.halted || self.pairs[idx].halted {
            self.pairs[idx].latest_quotes = None;
            return Ok(false);
        }
        if !valid_bid_ask(maker.bid, maker.ask) {
            return Ok(false);
        }
        let Some(quotes) = self.pairs[idx].latest_quotes else {
            return Ok(false);
        };

        let mut bid_hit = false;
        if let Some(bid) = quotes.bid
            && maker.ask < bid
        {
            bid_hit = self.on_maker_trade_print(
                symbol_id,
                now_ms,
                maker.ask,
                maker_ask_sz.max(0.0),
                false,
                hedge,
                utc_day,
            )?;
        }

        let mut ask_hit = false;
        if let Some(ask) = self.pairs[idx].latest_quotes.and_then(|q| q.ask)
            && maker.bid > ask
        {
            ask_hit = self.on_maker_trade_print(
                symbol_id,
                now_ms,
                maker.bid,
                maker_bid_sz.max(0.0),
                true,
                hedge,
                utc_day,
            )?;
        }

        Ok(bid_hit || ask_hit)
    }

    pub fn report(&self) -> Track02ShadowReport {
        let mut realized = 0.0;
        let mut unrealized = 0.0;
        let mut fees = 0.0;
        let mut total_modeled_hedge_slip_usd = 0.0;
        let mut unhedged_maker_notional_usd = 0.0;
        let mut fills = 0;
        let mut shield_blocked_ticks = 0;
        let mut unhedged_stale_halts = 0;
        let pairs = self
            .pairs
            .iter()
            .map(|p| {
                let mtm = p.unrealized_pnl_usd();
                realized += p.realized_pnl_usd;
                unrealized += mtm;
                fees += p.total_fees_usd;
                total_modeled_hedge_slip_usd += p.total_modeled_hedge_slip_usd;
                unhedged_maker_notional_usd += p.unhedged_maker_notional_usd;
                fills += p.fills;
                shield_blocked_ticks += p.shield_blocked_ticks;
                unhedged_stale_halts += p.unhedged_stale_halts;
                Track02PairReport {
                    label: p.cfg.label.clone(),
                    symbol_id: p.cfg.symbol_id,
                    maker_exchange_id: p.cfg.maker_exchange_id,
                    hedge_exchange_id: p.cfg.hedge_exchange_id,
                    ref_exchange_id: p.cfg.ref_exchange_id,
                    realized_pnl_usd: p.realized_pnl_usd,
                    unrealized_pnl_usd: mtm,
                    total_fees_usd: p.total_fees_usd,
                    total_modeled_hedge_slip_usd: p.total_modeled_hedge_slip_usd,
                    unhedged_maker_notional_usd: p.unhedged_maker_notional_usd,
                    maker_qty: p.maker_leg.qty,
                    hedge_qty: p.hedge_leg.qty,
                    net_unhedged_base_qty: p.maker_leg.qty + p.hedge_leg.qty,
                    inventory_clips: p.inventory_clips,
                    fills: p.fills,
                    shield_blocked_ticks: p.shield_blocked_ticks,
                    unhedged_stale_halts: p.unhedged_stale_halts,
                    halted: self.halted || p.halted,
                }
            })
            .collect();
        Track02ShadowReport {
            track: "02_hedge_arbitrage",
            fill_evidence_mode: self.fill_evidence_mode,
            fill_evidence_note: self.fill_evidence_note(),
            hedge_slip_mode: "expected_adverse_bps_applied",
            capital_usd: self.cfg.capital_usd,
            equity_usd: self.cfg.capital_usd + realized + unrealized,
            realized_pnl_usd: realized,
            unrealized_pnl_usd: unrealized,
            total_fees_usd: fees,
            total_modeled_hedge_slip_usd,
            unhedged_maker_notional_usd,
            max_drawdown_usd: self.max_drawdown_usd,
            fills,
            shield_blocked_ticks,
            unhedged_stale_halts,
            risk_scale: if self.halted {
                0.0
            } else {
                self.daily_loss.risk_scale()
            },
            halted: self.halted,
            pairs,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strategy::basis_maker_hedge::RefCancelShieldConfig;

    fn t1_cfg() -> Track01ShadowConfig {
        Track01ShadowConfig {
            capital_usd: 10_000.0,
            daily_loss_limit_pct: 0.02,
            daily_loss_state_path: None,
            allowlisted_symbols: vec!["ETH".to_string()],
            arms: vec![Track01ArmShadowConfig {
                symbol: "ETH".to_string(),
                symbol_id: 1002,
                ref_exchange_id: 6,
                tgt_exchange_id: 2,
                clip_usd: 500.0,
                max_l1_depth_fraction: 0.50,
                taker_fee_bps: 2.0,
                strategy: LeadLagTakerConfig {
                    theta_bps: 4.0,
                    tau_ms: 2_000,
                    hold_ms: 1_000,
                    taker_delay_ms: 200,
                    warmup_ms: 1_000,
                    ..LeadLagTakerConfig::default()
                },
            }],
        }
    }

    fn t2_cfg() -> Track02ShadowConfig {
        Track02ShadowConfig {
            capital_usd: 20_000.0,
            daily_loss_limit_pct: 0.02,
            daily_loss_state_path: None,
            pairs: vec![Track02PairShadowConfig {
                label: "lighter>arcus ETH".to_string(),
                symbol_id: 1002,
                maker_exchange_id: 2,
                hedge_exchange_id: 7,
                ref_exchange_id: 6,
                clip_usd: 500.0,
                expected_gross_edge_bps: 6.0,
                min_net_edge_bps: 0.5,
                max_hedge_stale_ms: 5_000,
                require_fail_closed_shield: true,
                economics: ShieldedPairEconomics {
                    maker_fee_bps: 0.0,
                    hedge_taker_fee_bps: 2.30,
                    cross_stablecoin_drag_bps: 0.0,
                    expected_hedge_slip_bps: 1.42,
                    cross_stablecoin: false,
                    same_risk_domain: false,
                },
                strategy: MakerHedgeQuoterConfig {
                    e0_bps: 6.0,
                    skew_bps_per_clip: 0.5,
                    q_max_clips: 4.0,
                    tau_ms: 60_000,
                    warmup_ms: 1_000,
                    ref_shield: RefCancelShieldConfig {
                        enabled: true,
                        adverse_move_bps: 2.5,
                        lookback_ms: 200,
                        allow_reduce_only_through_shield: false,
                        fail_closed_when_unready: true,
                    },
                    ..MakerHedgeQuoterConfig::default()
                },
            }],
        }
    }

    fn ll_touch(bid: f64, ask: f64, recv_ms: u64) -> LeadLagTouch {
        LeadLagTouch { bid, ask, recv_ms }
    }

    fn mk_touch(bid: f64, ask: f64, recv_ms: u64) -> MakerTouch {
        MakerTouch { bid, ask, recv_ms }
    }

    #[test]
    fn track01_config_validation_guards() {
        assert!(t1_cfg().validate().is_ok());

        let mut bad_sym = t1_cfg();
        bad_sym.arms[0].symbol = "SOL".to_string();
        assert!(bad_sym.validate().is_err());

        let mut dup_id = t1_cfg();
        dup_id.allowlisted_symbols.push("SOL".to_string());
        let mut second = dup_id.arms[0].clone();
        second.symbol = "SOL".to_string();
        dup_id.arms.push(second);
        assert!(dup_id.validate().is_err());

        let mut bad_depth = t1_cfg();
        bad_depth.arms[0].max_l1_depth_fraction = 1.5;
        assert!(bad_depth.validate().is_err());
    }

    #[test]
    fn track01_l1_depth_cap_fee_and_drawdown_lifecycle() {
        let mut s = Track01ShadowSession::new(t1_cfg(), utc_day_from_ms(1_000)).unwrap();
        let mut t = 1_000_u64;
        for _ in 0..15 {
            let _ = s
                .on_bbo_pair(
                    1002,
                    t,
                    ll_touch(99.99, 100.01, t),
                    ll_touch(99.99, 100.01, t),
                    10.0,
                    10.0,
                    utc_day_from_ms(t),
                )
                .unwrap();
            t += 100;
        }
        // Ref jumps +10 bp -> Long entry; tgt_ask = 100.01, tgt_ask_sz = 4.0 -> L1 = $400.04 -> 50% cap = $200.02.
        let act = s
            .on_bbo_pair(
                1002,
                t,
                ll_touch(100.09, 100.11, t),
                ll_touch(99.99, 100.01, t),
                10.0,
                4.0,
                utc_day_from_ms(t),
            )
            .unwrap();
        assert!(matches!(
            act,
            Some(LeadLagAction::Enter {
                side: LeadLagSide::Long,
                ..
            })
        ));
        let r1 = s.report();
        assert!((r1.arms[0].open_notional_usd - 200.02).abs() < 1e-6);
        assert!((r1.total_fees_usd - 200.02 * 2e-4).abs() < 1e-9);

        // Target catches up (+10 bp) -> Exit after delay + hold (1200 ms) yields positive net PnL.
        let exit_t = t + 1_200;
        let act = s
            .on_bbo_pair(
                1002,
                exit_t,
                ll_touch(100.09, 100.11, exit_t),
                ll_touch(100.09, 100.11, exit_t),
                10.0,
                10.0,
                utc_day_from_ms(exit_t),
            )
            .unwrap();
        assert_eq!(
            act,
            Some(LeadLagAction::Exit {
                side: LeadLagSide::Long
            })
        );
        let r2 = s.report();
        assert_eq!(r2.trades, 1);
        assert_eq!(r2.wins, 1);
        assert!(r2.realized_pnl_usd > 0.0);
        assert!(r2.arms[0].open_side.is_none());
    }

    #[test]
    fn track01_daily_loss_scales_and_halts_new_entries_while_allowing_exits() {
        let mut cfg = t1_cfg();
        cfg.capital_usd = 1_000.0;
        cfg.daily_loss_limit_pct = 0.02; // $20 budget
        cfg.arms[0].clip_usd = 500.0;
        cfg.arms[0].taker_fee_bps = 0.0;
        let mut s = Track01ShadowSession::new(cfg, utc_day_from_ms(1_000)).unwrap();

        let mut t = 1_000_u64;
        for _ in 0..15 {
            let _ = s
                .on_bbo_pair(
                    1002,
                    t,
                    ll_touch(99.99, 100.01, t),
                    ll_touch(99.99, 100.01, t),
                    100.0,
                    100.0,
                    utc_day_from_ms(t),
                )
                .unwrap();
            t += 100;
        }
        // Enter Long at 100.01 ($500 notional).
        let act = s
            .on_bbo_pair(
                1002,
                t,
                ll_touch(100.09, 100.11, t),
                ll_touch(99.99, 100.01, t),
                100.0,
                100.0,
                utc_day_from_ms(t),
            )
            .unwrap();
        assert!(matches!(act, Some(LeadLagAction::Enter { .. })));

        // Target drops 5% while position is open -> MTM loss ~$25 > $20 daily limit -> halts!
        // Hold hasn't expired yet (t + 500).
        let mid_t = t + 500;
        let _ = s
            .on_bbo_pair(
                1002,
                mid_t,
                ll_touch(95.0, 95.02, mid_t),
                ll_touch(94.99, 95.01, mid_t),
                100.0,
                100.0,
                utc_day_from_ms(mid_t),
            )
            .unwrap();
        assert!(s.is_halted());

        // At exit_due_ms, exit MUST still execute even though halted!
        let exit_t = t + 1_200;
        let act = s
            .on_bbo_pair(
                1002,
                exit_t,
                ll_touch(95.0, 95.02, exit_t),
                ll_touch(94.99, 95.01, exit_t),
                100.0,
                100.0,
                utc_day_from_ms(exit_t),
            )
            .unwrap();
        assert_eq!(
            act,
            Some(LeadLagAction::Exit {
                side: LeadLagSide::Long
            })
        );
        assert_eq!(s.report().trades, 1);

        // Subsequent signal while halted must NOT open a new position.
        let mut t2 = exit_t + 50_000;
        for _ in 0..15 {
            let _ = s
                .on_bbo_pair(
                    1002,
                    t2,
                    ll_touch(99.99, 100.01, t2),
                    ll_touch(99.99, 100.01, t2),
                    100.0,
                    100.0,
                    utc_day_from_ms(t2),
                )
                .unwrap();
            t2 += 100;
        }
        let act = s
            .on_bbo_pair(
                1002,
                t2,
                ll_touch(100.09, 100.11, t2),
                ll_touch(99.99, 100.01, t2),
                100.0,
                100.0,
                utc_day_from_ms(t2),
            )
            .unwrap();
        assert_eq!(act, Some(LeadLagAction::Idle));
        assert_eq!(s.report().trades, 1);
        assert!(s.report().skips >= 1);
    }

    #[test]
    fn track02_config_rejects_unready_fail_open_same_domain_and_fx_drag() {
        assert!(t2_cfg().validate().is_ok());

        // Requires fail-closed shield.
        let mut fail_open = t2_cfg();
        fail_open.pairs[0]
            .strategy
            .ref_shield
            .fail_closed_when_unready = false;
        assert!(fail_open.validate().is_err());

        // Rejects same risk domain.
        let mut same_dom = t2_cfg();
        same_dom.pairs[0].economics.same_risk_domain = true;
        assert!(same_dom.validate().is_err());

        // Cross-stablecoin 1-2 bp drag flips marginal pair unprofitable -> rejected.
        let mut fx_drag = t2_cfg();
        fx_drag.pairs[0].expected_gross_edge_bps = 4.0;
        fx_drag.pairs[0].min_net_edge_bps = 0.0;
        fx_drag.pairs[0].economics.cross_stablecoin = true;
        fx_drag.pairs[0].economics.cross_stablecoin_drag_bps = 1.0;
        // 4.0 - 2.30 - 1.42 - 1.0 = -0.72 < 0.0
        assert!(fx_drag.validate().is_err());
    }

    #[test]
    fn track02_shield_blocks_unready_then_quotes_fills_and_halts_on_stale_hedge() {
        let mut s = Track02ShadowSession::new(t2_cfg(), utc_day_from_ms(10_000)).unwrap();
        let mut t = 10_000_u64;
        for _ in 0..10 {
            let _ = s
                .on_pair_books(
                    1002,
                    t,
                    mk_touch(99.99, 100.01, t),
                    mk_touch(99.99, 100.01, t),
                    utc_day_from_ms(t),
                )
                .unwrap();
            t += 100;
        }
        // Reference shield has 0 samples -> fail-closed blocks both sides and increments shield_blocked_ticks.
        let q_unready = s
            .on_pair_books(
                1002,
                t,
                mk_touch(99.99, 100.01, t),
                mk_touch(99.99, 100.01, t),
                utc_day_from_ms(t),
            )
            .unwrap()
            .unwrap();
        assert_eq!(q_unready.bid, None);
        assert_eq!(q_unready.ask, None);
        assert_eq!(s.report().shield_blocked_ticks, 1);

        // Warm reference shield with 2 fresh samples -> both sides quote.
        s.on_reference_mid(1002, t, 100.0);
        t += 50;
        s.on_reference_mid(1002, t, 100.0);
        t += 50;
        let q_ready = s
            .on_pair_books(
                1002,
                t,
                mk_touch(99.99, 100.01, t),
                mk_touch(99.99, 100.01, t),
                utc_day_from_ms(t),
            )
            .unwrap()
            .unwrap();
        let bid = q_ready.bid.expect("bid should quote");
        assert!(q_ready.ask.is_some());

        // Strict trade-through at exact bid price does NOT fill.
        assert!(
            !s.on_maker_trade_print(
                1002,
                t,
                bid,
                10.0,
                false,
                mk_touch(99.99, 100.01, t),
                utc_day_from_ms(t),
            )
            .unwrap()
        );

        // Strict trade-through below bid DOES fill and immediately hedges at hedge.bid = 99.99.
        assert!(
            s.on_maker_trade_print(
                1002,
                t,
                bid - 0.01,
                10.0,
                false,
                mk_touch(99.99, 100.01, t),
                utc_day_from_ms(t),
            )
            .unwrap()
        );
        let r_after_fill = s.report();
        assert_eq!(r_after_fill.fills, 1);
        assert!(r_after_fill.total_fees_usd > 0.0);
        assert!(r_after_fill.pairs[0].maker_qty > 0.0);
        assert!((r_after_fill.pairs[0].maker_qty + r_after_fill.pairs[0].hedge_qty).abs() < 1e-12);

        // Re-quote, then hit ask with a STALE hedge (> max_hedge_stale_ms) -> fail-closed halt!
        t += 100;
        s.on_reference_mid(1002, t - 20, 100.0);
        s.on_reference_mid(1002, t, 100.0);
        let q2 = s
            .on_pair_books(
                1002,
                t,
                mk_touch(99.99, 100.01, t),
                mk_touch(99.99, 100.01, t),
                utc_day_from_ms(t),
            )
            .unwrap()
            .unwrap();
        let ask = q2.ask.expect("ask should quote");
        assert!(
            s.on_maker_trade_print(
                1002,
                t,
                ask + 0.01,
                10.0,
                true,
                mk_touch(99.99, 100.01, t - 6_000),
                utc_day_from_ms(t),
            )
            .unwrap()
        );
        let r_halted = s.report();
        assert_eq!(r_halted.unhedged_stale_halts, 1);
        assert!(r_halted.unhedged_maker_notional_usd > 0.0);
        assert!(r_halted.pairs[0].net_unhedged_base_qty.abs() > 0.0);
        assert!(r_halted.pairs[0].halted);
        // Subsequent pair books remain cleared when halted.
        t += 100;
        assert_eq!(
            s.on_pair_books(
                1002,
                t,
                mk_touch(99.99, 100.01, t),
                mk_touch(99.99, 100.01, t),
                utc_day_from_ms(t),
            )
            .unwrap(),
            None
        );
    }

    #[test]
    fn track01_and_track02_sessions_remain_isolated() {
        let mut cfg1 = t1_cfg();
        cfg1.capital_usd = 1_000.0;
        cfg1.daily_loss_limit_pct = 0.02;
        cfg1.arms[0].clip_usd = 500.0;
        let mut s1 = Track01ShadowSession::new(cfg1, utc_day_from_ms(10_000)).unwrap();
        let mut s2 = Track02ShadowSession::new(t2_cfg(), utc_day_from_ms(10_000)).unwrap();

        // Trip Track 01 daily loss while Track 02 runs untouched.
        let mut t = 10_000_u64;
        for _ in 0..15 {
            let _ = s1.on_bbo_pair(
                1002,
                t,
                ll_touch(99.99, 100.01, t),
                ll_touch(99.99, 100.01, t),
                100.0,
                100.0,
                utc_day_from_ms(t),
            );
            let _ = s2.on_pair_books(
                1002,
                t,
                mk_touch(99.99, 100.01, t),
                mk_touch(99.99, 100.01, t),
                utc_day_from_ms(t),
            );
            t += 100;
        }
        let _ = s1.on_bbo_pair(
            1002,
            t,
            ll_touch(100.09, 100.11, t),
            ll_touch(99.99, 100.01, t),
            100.0,
            100.0,
            utc_day_from_ms(t),
        );
        let _ = s1.on_bbo_pair(
            1002,
            t + 500,
            ll_touch(94.0, 94.02, t + 500),
            ll_touch(94.0, 94.02, t + 500),
            100.0,
            100.0,
            utc_day_from_ms(t + 500),
        );
        assert!(s1.is_halted());
        assert!(!s2.is_halted());
        assert!((s2.report().risk_scale - 1.0).abs() < 1e-12);
    }

    #[test]
    fn track01_progressive_risk_scale_reduces_entry_notional() {
        let mut cfg = t1_cfg();
        cfg.capital_usd = 1_000.0;
        cfg.daily_loss_limit_pct = 0.02; // $20 budget; >= $10 drawdown -> tier 2 (0.50x)
        cfg.arms[0].clip_usd = 400.0;
        cfg.arms[0].taker_fee_bps = 0.0;
        let mut s = Track01ShadowSession::new(cfg, utc_day_from_ms(1_000)).unwrap();

        let mut t = 1_000_u64;
        for _ in 0..15 {
            let _ = s.on_bbo_pair(
                1002,
                t,
                ll_touch(99.99, 100.01, t),
                ll_touch(99.99, 100.01, t),
                100.0,
                100.0,
                utc_day_from_ms(t),
            );
            t += 100;
        }
        // Trade 1 ($400 notional) loses ~$12 (3% drop on $400) -> 60% of $20 budget -> 0.50x tier.
        assert!(matches!(
            s.on_bbo_pair(
                1002,
                t,
                ll_touch(100.09, 100.11, t),
                ll_touch(99.99, 100.01, t),
                100.0,
                100.0,
                utc_day_from_ms(t),
            )
            .unwrap(),
            Some(LeadLagAction::Enter { .. })
        ));
        let exit_t = t + 1_200;
        assert!(matches!(
            s.on_bbo_pair(
                1002,
                exit_t,
                ll_touch(96.99, 97.01, exit_t),
                ll_touch(96.99, 97.01, exit_t),
                100.0,
                100.0,
                utc_day_from_ms(exit_t),
            )
            .unwrap(),
            Some(LeadLagAction::Exit { .. })
        ));
        assert!(!s.is_halted());
        assert!((s.report().risk_scale - 0.50).abs() < 1e-12);

        // Re-warm and trigger Trade 2 -> notional must scale to $400 * 0.50 = $200.
        let mut t2 = exit_t + 50_000;
        for _ in 0..15 {
            let _ = s.on_bbo_pair(
                1002,
                t2,
                ll_touch(96.99, 97.01, t2),
                ll_touch(96.99, 97.01, t2),
                100.0,
                100.0,
                utc_day_from_ms(t2),
            );
            t2 += 100;
        }
        assert!(matches!(
            s.on_bbo_pair(
                1002,
                t2,
                ll_touch(97.09, 97.11, t2),
                ll_touch(96.99, 97.01, t2),
                100.0,
                100.0,
                utc_day_from_ms(t2),
            )
            .unwrap(),
            Some(LeadLagAction::Enter { .. })
        ));
        assert!((s.report().arms[0].open_notional_usd - 200.0).abs() < 1e-6);
    }

    fn warm_track02_session(
        cfg: Track02ShadowConfig,
    ) -> (Track02ShadowSession, u64, MakerHedgeQuotes) {
        let mut t = 10_000_u64;
        let mut s = Track02ShadowSession::new(cfg, utc_day_from_ms(t)).unwrap();
        for _ in 0..12 {
            let _ = s
                .on_pair_books(
                    1002,
                    t,
                    mk_touch(99.99, 100.01, t),
                    mk_touch(99.99, 100.01, t),
                    utc_day_from_ms(t),
                )
                .unwrap();
            t += 100;
        }
        s.on_reference_mid(1002, t, 100.0);
        t += 50;
        s.on_reference_mid(1002, t, 100.0);
        t += 50;
        let q = s
            .on_pair_books(
                1002,
                t,
                mk_touch(99.99, 100.01, t),
                mk_touch(99.99, 100.01, t),
                utc_day_from_ms(t),
            )
            .unwrap()
            .unwrap();
        (s, t, q)
    }

    #[test]
    fn track02_f3_modeled_hedge_slip_applies_to_hedge_price_and_equity() {
        let mut cfg = t2_cfg();
        cfg.pairs[0].expected_gross_edge_bps = 15.0;
        cfg.pairs[0].economics.maker_fee_bps = 0.0;
        cfg.pairs[0].economics.hedge_taker_fee_bps = 0.0;
        cfg.pairs[0].economics.expected_hedge_slip_bps = 10.0;
        let (mut s, t, q) = warm_track02_session(cfg);
        let bid = q.bid.expect("bid should quote");

        let hedge_touch = mk_touch(99.99, 100.01, t);
        assert!(
            s.on_maker_trade_print(
                1002,
                t,
                bid - 0.01,
                10.0,
                false,
                hedge_touch,
                utc_day_from_ms(t),
            )
            .unwrap()
        );
        let rep = s.report();
        assert_eq!(rep.hedge_slip_mode, "expected_adverse_bps_applied");

        let qty = 500.0 / bid;
        let slip_frac = 10.0 * 1e-4;
        let slipped_hedge_px = hedge_touch.bid * (1.0 - slip_frac);
        let expected_slip_usd = qty * (hedge_touch.bid - slipped_hedge_px).abs();
        assert!((rep.total_modeled_hedge_slip_usd - expected_slip_usd).abs() < 1e-9);
        assert!((rep.pairs[0].total_modeled_hedge_slip_usd - expected_slip_usd).abs() < 1e-9);

        // Compare against zero-slippage MTM equity: equity drops by exact modeled slippage USD.
        let maker_mtm = qty * (100.0 - bid);
        let hedge_mtm = -qty * (100.0 - slipped_hedge_px);
        let expected_equity = rep.capital_usd + maker_mtm + hedge_mtm;
        assert!((rep.equity_usd - expected_equity).abs() < 1e-9);
        let zero_slip_equity = rep.capital_usd + qty * (hedge_touch.bid - bid);
        assert!((zero_slip_equity - rep.equity_usd - expected_slip_usd).abs() < 1e-9);
    }

    #[test]
    fn track02_f2_stale_hedge_records_unhedged_fill_and_mtm_drawdown() {
        let mut cfg = t2_cfg();
        cfg.capital_usd = 1_000.0;
        cfg.daily_loss_limit_pct = 0.02; // $20 budget
        cfg.pairs[0].clip_usd = 500.0;
        let (mut s, mut t, q) = warm_track02_session(cfg);
        let bid = q.bid.expect("bid should quote");

        // Maker bid hit while hedge book is stale (> max_hedge_stale_ms) -> records unhedged long maker leg and halts.
        assert!(
            s.on_maker_trade_print(
                1002,
                t,
                bid - 0.01,
                10.0,
                false,
                mk_touch(99.99, 100.01, t - 6_000),
                utc_day_from_ms(t),
            )
            .unwrap()
        );
        let rep1 = s.report();
        assert_eq!(rep1.fills, 1);
        assert_eq!(rep1.unhedged_stale_halts, 1);
        assert!((rep1.unhedged_maker_notional_usd - 500.0).abs() < 1e-9);
        assert!((rep1.pairs[0].unhedged_maker_notional_usd - 500.0).abs() < 1e-9);
        assert!(rep1.pairs[0].maker_qty > 0.0);
        assert_eq!(rep1.pairs[0].hedge_qty, 0.0);
        assert!((rep1.pairs[0].net_unhedged_base_qty - rep1.pairs[0].maker_qty).abs() < 1e-12);
        assert!(rep1.pairs[0].halted);

        // Adverse 6% market drop marks unhedged long maker position down by ~$30 -> MTM drawdown & daily loss halt.
        t += 100;
        assert_eq!(
            s.on_pair_books(
                1002,
                t,
                mk_touch(93.99, 94.01, t),
                mk_touch(93.99, 94.01, t),
                utc_day_from_ms(t),
            )
            .unwrap(),
            None
        );
        let rep2 = s.report();
        assert!(rep2.unrealized_pnl_usd < -20.0);
        assert!(rep2.max_drawdown_usd > 20.0);
        assert!(rep2.halted);
    }

    #[test]
    fn track02_f4_bbo_cross_diagnostic_fills_hedges_and_sets_evidence_mode() {
        let (mut s, mut t, q) = warm_track02_session(t2_cfg());
        s.set_fill_evidence_mode("shm_bbo_cross_diagnostic_no_trades");
        let ask = q.ask.expect("ask should quote");

        // Maker market spikes strictly above our resting ask -> synthetic buy sweep fills ask + hedges.
        t += 50;
        let maker_spike = mk_touch(ask + 0.02, ask + 0.04, t);
        let hedge_now = mk_touch(ask + 0.01, ask + 0.03, t);
        assert!(
            s.on_maker_bbo_cross_diagnostic(
                1002,
                t,
                maker_spike,
                10.0,
                10.0,
                hedge_now,
                utc_day_from_ms(t),
            )
            .unwrap()
        );
        let rep = s.report();
        assert_eq!(rep.fill_evidence_mode, "shm_bbo_cross_diagnostic_no_trades");
        assert!(rep.fill_evidence_note.contains("BboMatrix SHM"));
        assert_eq!(rep.fills, 1);
        assert!(rep.pairs[0].maker_qty < 0.0);
        assert!(rep.pairs[0].hedge_qty > 0.0);
        assert!(rep.pairs[0].net_unhedged_base_qty.abs() < 1e-12);
        assert!(rep.total_modeled_hedge_slip_usd > 0.0);
    }
}
