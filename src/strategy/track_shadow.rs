//! Decoupled Track 01 (`01_quant_profit`), Track 02 (`02_hedge_arbitrage`), and
//! Track 03 (`03_incentive_turnover`) shadow sessions.
//!
//! Strictly isolated from exchange execution adapters (`crate::exchanges::*` is forbidden here)
//! and strictly isolated from each other: separate configurations, runtime state,
//! [`DailyLossGuard`] circuit breakers, and reports (`AGENTS.md` non-cross-subsidy contract).

use crate::daily_loss::DailyLossGuard;
use crate::order_budget::OrderIntentClass;
use crate::strategy::basis_maker_hedge::{
    FillSide, MakerHedgeQuoter, MakerHedgeQuoterConfig, MakerHedgeQuotes, ShieldedPairEconomics,
};
use crate::strategy::defense_fsm::{
    DefenseFsm, DefenseFsmConfig, DefenseSide, DefenseState, DefenseTelemetrySnapshot,
    ResetBlockedReason,
};
use crate::strategy::lead_lag_taker::{
    LeadLagAction, LeadLagSide, LeadLagTaker, LeadLagTakerConfig,
};
use crate::strategy::queue_priority_mm::{
    FastTradePrint, InventoryState, MmAction, QueuePriorityMmConfig, QueuePriorityMmEngine,
    QuoteSide, ReferenceBookUpdate, RestingQuote, VenueBookUpdate, quantize_down,
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
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

/// Per-venue feed continuity and drop diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct VenueFeedHealthReport {
    pub exchange_id: u8,
    pub samples: u64,
    pub max_gap_ms: u64,
    pub stale_book_drops: u64,
    pub clock_regression_drops: u64,
    pub crossed_book_drops: u64,
}

#[derive(Debug, Clone, Copy, Default)]
struct VenueFeedTracker {
    exchange_id: u8,
    last_now_ms: Option<u64>,
    last_recv_ms: Option<u64>,
    samples: u64,
    last_gap_ms: u64,
    max_gap_ms: u64,
    stale_book_drops: u64,
    clock_regression_drops: u64,
    crossed_book_drops: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FeedObservation {
    Fresh,
    CrossedOrInvalid,
    ClockRegressed,
    Stale,
}

impl VenueFeedTracker {
    fn new(exchange_id: u8) -> Self {
        Self {
            exchange_id,
            ..Self::default()
        }
    }

    fn observe_bbo(
        &mut self,
        now_ms: u64,
        bid: f64,
        ask: f64,
        recv_ms: u64,
        max_age_ms: u64,
    ) -> FeedObservation {
        if !valid_bid_ask(bid, ask) {
            self.crossed_book_drops += 1;
            return FeedObservation::CrossedOrInvalid;
        }
        if recv_ms > now_ms
            || self.last_now_ms.is_some_and(|prev| now_ms < prev)
            || self.last_recv_ms.is_some_and(|prev| recv_ms < prev)
        {
            self.clock_regression_drops += 1;
            return FeedObservation::ClockRegressed;
        }
        if now_ms - recv_ms > max_age_ms {
            self.stale_book_drops += 1;
            if let Some(prev_recv) = self.last_recv_ms {
                let gap = now_ms - prev_recv;
                self.last_gap_ms = gap;
                self.max_gap_ms = self.max_gap_ms.max(gap);
            }
            self.last_now_ms = Some(now_ms);
            return FeedObservation::Stale;
        }
        if let Some(prev_recv) = self.last_recv_ms {
            let gap = recv_ms.saturating_sub(prev_recv);
            self.last_gap_ms = gap;
            if gap > 0 {
                self.max_gap_ms = self.max_gap_ms.max(gap);
            }
        }
        self.last_recv_ms = Some(recv_ms);
        self.last_now_ms = Some(now_ms);
        self.samples += 1;
        FeedObservation::Fresh
    }

    fn observe_mid(&mut self, now_ms: u64, mid: f64) {
        if !(mid.is_finite() && mid > 0.0) {
            return;
        }
        if self.last_now_ms.is_some_and(|prev| now_ms < prev) {
            self.clock_regression_drops += 1;
            return;
        }
        if let Some(prev_now) = self.last_now_ms {
            let gap = now_ms.saturating_sub(prev_now);
            self.last_gap_ms = gap;
            if gap > 0 {
                self.max_gap_ms = self.max_gap_ms.max(gap);
            }
        }
        self.last_now_ms = Some(now_ms);
        self.samples += 1;
    }

    fn record_crossed_drop(&mut self) {
        self.crossed_book_drops += 1;
    }

    fn report(&self) -> VenueFeedHealthReport {
        VenueFeedHealthReport {
            exchange_id: self.exchange_id,
            samples: self.samples,
            max_gap_ms: self.max_gap_ms,
            stale_book_drops: self.stale_book_drops,
            clock_regression_drops: self.clock_regression_drops,
            crossed_book_drops: self.crossed_book_drops,
        }
    }
}

fn aggregate_venue_health(
    feeds: impl IntoIterator<Item = VenueFeedHealthReport>,
) -> (Vec<VenueFeedHealthReport>, u64, u64, u64, u64) {
    let mut by_exchange: BTreeMap<u8, VenueFeedHealthReport> = BTreeMap::new();
    let mut max_gap_ms = 0_u64;
    let mut stale_book_drops = 0_u64;
    let mut clock_regression_drops = 0_u64;
    let mut crossed_book_drops = 0_u64;

    for feed in feeds {
        max_gap_ms = max_gap_ms.max(feed.max_gap_ms);
        stale_book_drops += feed.stale_book_drops;
        clock_regression_drops += feed.clock_regression_drops;
        crossed_book_drops += feed.crossed_book_drops;

        let entry = by_exchange
            .entry(feed.exchange_id)
            .or_insert_with(|| VenueFeedHealthReport {
                exchange_id: feed.exchange_id,
                ..VenueFeedHealthReport::default()
            });
        entry.samples += feed.samples;
        entry.max_gap_ms = entry.max_gap_ms.max(feed.max_gap_ms);
        entry.stale_book_drops += feed.stale_book_drops;
        entry.clock_regression_drops += feed.clock_regression_drops;
        entry.crossed_book_drops += feed.crossed_book_drops;
    }

    (
        by_exchange.into_values().collect(),
        max_gap_ms,
        stale_book_drops,
        clock_regression_drops,
        crossed_book_drops,
    )
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
    #[serde(default)]
    pub confirm_ref_exchange_id: Option<u8>,
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
            confirm_ref_exchange_id: None,
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
    #[serde(default)]
    pub defense: Option<DefenseFsmConfig>,
}

impl Default for Track01ShadowConfig {
    fn default() -> Self {
        Self {
            capital_usd: 10_000.0,
            daily_loss_limit_pct: 0.02,
            daily_loss_state_path: None,
            allowlisted_symbols: Vec::new(),
            arms: Vec::new(),
            defense: None,
        }
    }
}

impl Track01ShadowConfig {
    /// Validate capital, loss limits, allowlist membership, unique symbol IDs, and arm strategies.
    pub fn validate(&self) -> Result<()> {
        if let Some(ref defense_cfg) = self.defense {
            defense_cfg.validate()?;
        }
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
            if let Some(confirm_id) = arm.confirm_ref_exchange_id {
                ensure!(
                    confirm_id != arm.tgt_exchange_id && confirm_id != arm.ref_exchange_id,
                    "arm '{}' confirm_ref_exchange_id must differ from ref_exchange_id and tgt_exchange_id",
                    arm.symbol
                );
            }
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
    ref_feed: VenueFeedTracker,
    confirm_ref_feed: Option<VenueFeedTracker>,
    tgt_feed: VenueFeedTracker,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirm_ref_exchange_id: Option<u8>,
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
    pub ref_feed: VenueFeedHealthReport,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirm_ref_feed: Option<VenueFeedHealthReport>,
    pub tgt_feed: VenueFeedHealthReport,
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
    pub defense_state_transitions: u64,
    pub defense_blocked_entries: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub defense_state: Option<DefenseState>,
    pub max_gap_ms: u64,
    pub stale_book_drops: u64,
    pub clock_regression_drops: u64,
    pub crossed_book_drops: u64,
    pub venue_feed_health: Vec<VenueFeedHealthReport>,
    pub arms: Vec<Track01ArmReport>,
}

/// Decoupled Track 01 lead-lag taker shadow session with isolated [`DailyLossGuard`].
pub struct Track01ShadowSession {
    cfg: Track01ShadowConfig,
    arms: Vec<Track01ArmRuntime>,
    arm_by_symbol_id: HashMap<u16, usize>,
    daily_loss: DailyLossGuard,
    defense: Option<DefenseFsm>,
    defense_blocked_entries: u64,
    halted: bool,
    peak_equity_usd: f64,
    max_drawdown_usd: f64,
}

impl Track01ShadowSession {
    pub fn new(cfg: Track01ShadowConfig, initial_utc_day: i32) -> Result<Self> {
        cfg.validate()?;
        let defense = cfg.defense.map(DefenseFsm::new).transpose()?;
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
                ref_feed: VenueFeedTracker::new(arm_cfg.ref_exchange_id),
                confirm_ref_feed: arm_cfg.confirm_ref_exchange_id.map(VenueFeedTracker::new),
                tgt_feed: VenueFeedTracker::new(arm_cfg.tgt_exchange_id),
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
            defense,
            defense_blocked_entries: 0,
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

    pub fn defense_state(&self) -> Option<DefenseState> {
        self.defense.as_ref().map(DefenseFsm::state)
    }

    pub fn acknowledge_defense_reset(
        &mut self,
        snapshot: &DefenseTelemetrySnapshot,
    ) -> std::result::Result<DefenseState, ResetBlockedReason> {
        match self.defense.as_mut() {
            Some(fsm) => fsm.acknowledge_reconciled_reset(snapshot),
            None => Err(ResetBlockedReason::NotHalted),
        }
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

    pub fn arm_confirm_exchange_id(&self, symbol_id: u16) -> Option<u8> {
        self.arm_by_symbol_id
            .get(&symbol_id)
            .and_then(|&idx| self.arms[idx].cfg.confirm_ref_exchange_id)
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

    /// Record a crossed or malformed SHM book drop detected before `on_bbo_pair`.
    pub fn record_venue_crossed_drop(&mut self, symbol_id: u16, exchange_id: u8) {
        let Some(&arm_idx) = self.arm_by_symbol_id.get(&symbol_id) else {
            return;
        };
        let arm = &mut self.arms[arm_idx];
        if arm.cfg.ref_exchange_id == exchange_id {
            arm.ref_feed.record_crossed_drop();
        } else if arm.cfg.confirm_ref_exchange_id == Some(exchange_id) {
            if let Some(feed) = arm.confirm_ref_feed.as_mut() {
                feed.record_crossed_drop();
            }
        } else if arm.cfg.tgt_exchange_id == exchange_id {
            arm.tgt_feed.record_crossed_drop();
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
        self.on_bbo_pair_with_confirm(
            symbol_id, now_ms, ref_touch, None, tgt_touch, tgt_bid_sz, tgt_ask_sz, utc_day,
        )
    }

    /// Process one synchronized reference/target BBO pair with an optional confirmation leader
    /// touch wired into [`LeadLagTaker::observe_with_confirm`].
    #[allow(clippy::too_many_arguments)]
    pub fn on_bbo_pair_with_confirm(
        &mut self,
        symbol_id: u16,
        now_ms: u64,
        ref_touch: LeadLagTouch,
        confirm_touch: Option<LeadLagTouch>,
        tgt_touch: LeadLagTouch,
        tgt_bid_sz: f64,
        tgt_ask_sz: f64,
        utc_day: i32,
    ) -> Result<Option<LeadLagAction>> {
        let Some(&arm_idx) = self.arm_by_symbol_id.get(&symbol_id) else {
            return Ok(None);
        };
        let (ref_obs, confirm_obs, tgt_obs) = {
            let arm = &mut self.arms[arm_idx];
            let max_ref_age_ms = arm.cfg.strategy.max_ref_age_ms;
            let max_tgt_age_ms = arm.cfg.strategy.max_tgt_age_ms;
            let ref_obs = arm.ref_feed.observe_bbo(
                now_ms,
                ref_touch.bid,
                ref_touch.ask,
                ref_touch.recv_ms,
                max_ref_age_ms,
            );
            let confirm_obs = confirm_touch.map(|ct| {
                let fallback_id = arm.cfg.confirm_ref_exchange_id.unwrap_or(0);
                let feed = arm
                    .confirm_ref_feed
                    .get_or_insert_with(|| VenueFeedTracker::new(fallback_id));
                feed.observe_bbo(now_ms, ct.bid, ct.ask, ct.recv_ms, max_ref_age_ms)
            });
            let tgt_obs = arm.tgt_feed.observe_bbo(
                now_ms,
                tgt_touch.bid,
                tgt_touch.ask,
                tgt_touch.recv_ms,
                max_tgt_age_ms,
            );
            if tgt_obs == FeedObservation::Fresh {
                arm.last_tgt_touch = Some(tgt_touch);
            }
            (ref_obs, confirm_obs, tgt_obs)
        };
        // Check MTM equity & daily loss brake before sizing any new entry.
        self.check_equity_and_drawdown(utc_day);

        let ref_mid = if ref_touch.valid() && ref_obs != FeedObservation::ClockRegressed {
            ref_touch.mid()
        } else {
            f64::NAN
        };
        let tgt_mid = if tgt_touch.valid() && tgt_obs != FeedObservation::ClockRegressed {
            tgt_touch.mid()
        } else {
            f64::NAN
        };
        let confirm_arg = match (confirm_touch, confirm_obs) {
            (Some(ct), Some(FeedObservation::Fresh)) => Some((ct.mid(), ct.recv_ms)),
            _ => None,
        };
        let action = self.arms[arm_idx].engine.observe_with_confirm(
            now_ms,
            (ref_mid, ref_touch.recv_ms),
            confirm_arg,
            (tgt_mid, tgt_touch.recv_ms),
        );

        let risk_scale = if self.halted {
            0.0
        } else {
            self.daily_loss.risk_scale()
        };
        let dl_util = self.daily_loss.drawdown_budget_ratio(self.equity_usd());
        if self.defense.is_some() {
            let arm_ref = &self.arms[arm_idx];
            let confirm_stale = confirm_touch.map_or(0, |ct| now_ms.saturating_sub(ct.recv_ms));
            let confirm_gap = arm_ref.confirm_ref_feed.map_or(0, |f| f.last_gap_ms);
            let feed_staleness_ms = now_ms
                .saturating_sub(ref_touch.recv_ms)
                .max(now_ms.saturating_sub(tgt_touch.recv_ms))
                .max(confirm_stale);
            let feed_gap_ms = arm_ref
                .ref_feed
                .last_gap_ms
                .max(arm_ref.tgt_feed.last_gap_ms)
                .max(confirm_gap);
            let vol_shock_ratio =
                (arm_ref.engine.sigma_1s_bps() / arm_ref.cfg.strategy.theta_bps.max(1e-6)).max(0.0);
            let oracle_divergence_bps = arm_ref.engine.last_dev_bps().unwrap_or(0.0).abs();
            let clock_regressed_externally = ref_obs == FeedObservation::ClockRegressed
                || tgt_obs == FeedObservation::ClockRegressed
                || confirm_obs == Some(FeedObservation::ClockRegressed);
            let snap = DefenseTelemetrySnapshot {
                now_ms,
                feed_staleness_ms,
                feed_gap_ms,
                vol_shock_ratio,
                trailing_bid_markout_bps: 0.0,
                trailing_ask_markout_bps: 0.0,
                oracle_divergence_bps,
                daily_loss_utilization: dl_util,
                budget_cooldown_active: false,
                budget_halted: false,
                clock_regressed_externally,
            };
            if let Some(ref mut fsm) = self.defense {
                fsm.evaluate(&snap);
            }
        }
        let mut defense_blocked_entry = false;
        let arm = &mut self.arms[arm_idx];
        let mut emitted = action;
        match action {
            LeadLagAction::Enter { side, .. } => {
                let def_side = match side {
                    LeadLagSide::Long => DefenseSide::Buy,
                    LeadLagSide::Short => DefenseSide::Sell,
                };
                let defense_scale = self.defense.as_ref().map_or(1.0, |fsm| {
                    if fsm.permits_intent(OrderIntentClass::RiskIncreasingEntry, Some(def_side)) {
                        fsm.entry_size_scale(Some(def_side))
                    } else {
                        0.0
                    }
                });
                let entry_l1_usd = match side {
                    LeadLagSide::Long => tgt_touch.ask * tgt_ask_sz,
                    LeadLagSide::Short => tgt_touch.bid * tgt_bid_sz,
                };
                let capped = arm
                    .cfg
                    .clip_usd
                    .min(arm.cfg.max_l1_depth_fraction * entry_l1_usd);
                let notional = capped * risk_scale * defense_scale;
                if defense_scale <= 0.0 {
                    defense_blocked_entry = true;
                    arm.engine.clear_position();
                    arm.skips += 1;
                    emitted = LeadLagAction::Idle;
                } else if self.halted
                    || ref_obs != FeedObservation::Fresh
                    || tgt_obs != FeedObservation::Fresh
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
                if tgt_obs == FeedObservation::Fresh && tgt_touch.valid() {
                    arm.exit(tgt_touch);
                    // Shadow execution immediately confirms flat via authoritative exit fill hook.
                    arm.engine.notify_exit_filled();
                } else if arm.open_pos.is_none() {
                    // Resynchronize cleanly if somehow out of sync with no shadow position open.
                    arm.engine.clear_position();
                } else {
                    arm.blocks += 1;
                    emitted = LeadLagAction::Blocked(
                        crate::strategy::lead_lag_taker::LeadLagBlock::StaleTarget,
                    );
                }
            }
            LeadLagAction::Blocked(_) => {
                arm.blocks += 1;
            }
            LeadLagAction::Idle | LeadLagAction::QuotePassiveExit { .. } => {}
        }

        if defense_blocked_entry {
            self.defense_blocked_entries += 1;
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
        let arms: Vec<Track01ArmReport> = self
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
                    confirm_ref_exchange_id: a.cfg.confirm_ref_exchange_id,
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
                    ref_feed: a.ref_feed.report(),
                    confirm_ref_feed: a.confirm_ref_feed.map(|f| f.report()),
                    tgt_feed: a.tgt_feed.report(),
                }
            })
            .collect();
        let (
            venue_feed_health,
            max_gap_ms,
            stale_book_drops,
            clock_regression_drops,
            crossed_book_drops,
        ) = aggregate_venue_health(arms.iter().flat_map(|a| {
            [Some(a.ref_feed), a.confirm_ref_feed, Some(a.tgt_feed)]
                .into_iter()
                .flatten()
        }));
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
            defense_state_transitions: self.defense.as_ref().map_or(0, DefenseFsm::transitions),
            defense_blocked_entries: self.defense_blocked_entries,
            defense_state: self.defense.as_ref().map(DefenseFsm::state),
            max_gap_ms,
            stale_book_drops,
            clock_regression_drops,
            crossed_book_drops,
            venue_feed_health,
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
    #[serde(default)]
    pub defense: Option<DefenseFsmConfig>,
}

impl Default for Track02ShadowConfig {
    fn default() -> Self {
        Self {
            capital_usd: 20_000.0,
            daily_loss_limit_pct: 0.02,
            daily_loss_state_path: None,
            pairs: Vec::new(),
            defense: None,
        }
    }
}

impl Track02ShadowConfig {
    /// Validate capital, loss limits, fail-closed shield enforcement, and pair economics.
    pub fn validate(&self) -> Result<()> {
        if let Some(ref defense_cfg) = self.defense {
            defense_cfg.validate()?;
        }
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
    latest_bid_defense_scale: f64,
    latest_ask_defense_scale: f64,
    last_maker_touch: Option<MakerTouch>,
    last_hedge_touch: Option<MakerTouch>,
    last_ref_sample: Option<(u64, f64)>,
    maker_feed: VenueFeedTracker,
    hedge_feed: VenueFeedTracker,
    ref_feed: VenueFeedTracker,
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
    pub maker_feed: VenueFeedHealthReport,
    pub hedge_feed: VenueFeedHealthReport,
    pub ref_feed: VenueFeedHealthReport,
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
    pub defense_state_transitions: u64,
    pub defense_blocked_entries: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub defense_state: Option<DefenseState>,
    pub max_gap_ms: u64,
    pub stale_book_drops: u64,
    pub clock_regression_drops: u64,
    pub crossed_book_drops: u64,
    pub venue_feed_health: Vec<VenueFeedHealthReport>,
    pub pairs: Vec<Track02PairReport>,
}

/// Decoupled Track 02 shielded maker-hedge shadow session with isolated [`DailyLossGuard`].
pub struct Track02ShadowSession {
    cfg: Track02ShadowConfig,
    pairs: Vec<Track02PairRuntime>,
    pair_by_symbol_id: HashMap<u16, usize>,
    daily_loss: DailyLossGuard,
    defense: Option<DefenseFsm>,
    defense_blocked_entries: u64,
    fill_evidence_mode: &'static str,
    halted: bool,
    peak_equity_usd: f64,
    max_drawdown_usd: f64,
}

impl Track02ShadowSession {
    pub fn new(cfg: Track02ShadowConfig, initial_utc_day: i32) -> Result<Self> {
        cfg.validate()?;
        let defense = cfg.defense.map(DefenseFsm::new).transpose()?;
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
                latest_bid_defense_scale: 1.0,
                latest_ask_defense_scale: 1.0,
                last_maker_touch: None,
                last_hedge_touch: None,
                last_ref_sample: None,
                maker_feed: VenueFeedTracker::new(pair_cfg.maker_exchange_id),
                hedge_feed: VenueFeedTracker::new(pair_cfg.hedge_exchange_id),
                ref_feed: VenueFeedTracker::new(pair_cfg.ref_exchange_id),
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
            defense,
            defense_blocked_entries: 0,
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

    pub fn defense_state(&self) -> Option<DefenseState> {
        self.defense.as_ref().map(DefenseFsm::state)
    }

    pub fn acknowledge_defense_reset(
        &mut self,
        snapshot: &DefenseTelemetrySnapshot,
    ) -> std::result::Result<DefenseState, ResetBlockedReason> {
        match self.defense.as_mut() {
            Some(fsm) => fsm.acknowledge_reconciled_reset(snapshot),
            None => Err(ResetBlockedReason::NotHalted),
        }
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
                pair.latest_bid_defense_scale = 1.0;
                pair.latest_ask_defense_scale = 1.0;
            }
        }
    }

    /// Clear any resting quotes for `symbol_id` (fail-closed on SHM invalidation/disconnect).
    pub fn clear_pair_quotes(&mut self, symbol_id: u16) {
        if let Some(&idx) = self.pair_by_symbol_id.get(&symbol_id) {
            self.pairs[idx].latest_quotes = None;
            self.pairs[idx].latest_bid_defense_scale = 1.0;
            self.pairs[idx].latest_ask_defense_scale = 1.0;
        }
    }

    /// Record a crossed or malformed SHM book drop detected before `on_pair_books` / `on_reference_mid`.
    pub fn record_venue_crossed_drop(&mut self, symbol_id: u16, exchange_id: u8) {
        let Some(&idx) = self.pair_by_symbol_id.get(&symbol_id) else {
            return;
        };
        let pair = &mut self.pairs[idx];
        if pair.cfg.maker_exchange_id == exchange_id {
            pair.maker_feed.record_crossed_drop();
        }
        if pair.cfg.hedge_exchange_id == exchange_id {
            pair.hedge_feed.record_crossed_drop();
        }
        if pair.cfg.ref_exchange_id == exchange_id {
            pair.ref_feed.record_crossed_drop();
        }
    }

    /// Feed an external reference mid price into the pair's cancel shield.
    pub fn on_reference_mid(&mut self, symbol_id: u16, now_ms: u64, ref_mid: f64) {
        let Some(&idx) = self.pair_by_symbol_id.get(&symbol_id) else {
            return;
        };
        let pair = &mut self.pairs[idx];
        pair.ref_feed.observe_mid(now_ms, ref_mid);
        pair.quoter.observe_reference(now_ms, ref_mid);
        pair.ref_tracker
            .observe(now_ms, ref_mid, pair.cfg.strategy.max_gap_ms);
    }

    /// Feed a timestamped external reference BBO snapshot into the pair's cancel shield.
    ///
    /// Rejects stale, future, clock-regressed, or crossed reference books without advancing the
    /// shield clock with wall time, and deduplicates unchanged `(recv_ms, mid)` SHM snapshots so
    /// faster-ticking maker/hedge feeds cannot evict crash history from `REF_RING_CAP`.
    pub fn on_reference_touch(
        &mut self,
        symbol_id: u16,
        now_ms: u64,
        ref_touch: MakerTouch,
    ) -> bool {
        let Some(&idx) = self.pair_by_symbol_id.get(&symbol_id) else {
            return false;
        };
        let pair = &mut self.pairs[idx];
        let obs = pair.ref_feed.observe_bbo(
            now_ms,
            ref_touch.bid,
            ref_touch.ask,
            ref_touch.recv_ms,
            pair.cfg.strategy.max_book_age_ms,
        );
        if obs != FeedObservation::Fresh {
            return false;
        }
        let ref_mid = ref_touch.mid();
        if pair.last_ref_sample == Some((ref_touch.recv_ms, ref_mid)) {
            return false;
        }
        pair.last_ref_sample = Some((ref_touch.recv_ms, ref_mid));
        pair.quoter.observe_reference(ref_touch.recv_ms, ref_mid);
        pair.ref_tracker
            .observe(ref_touch.recv_ms, ref_mid, pair.cfg.strategy.max_gap_ms);
        true
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
        self.on_pair_books_with_l1(symbol_id, now_ms, maker, hedge, None, utc_day)
    }

    /// Update maker/hedge quotes from the latest books with optional hedge L1 sizes wired into
    /// [`MakerHedgeQuoter::step_with_l1`], enforcing `min_hedge_l1_notional_usd` when configured.
    pub fn on_pair_books_with_l1(
        &mut self,
        symbol_id: u16,
        now_ms: u64,
        maker: MakerTouch,
        hedge: MakerTouch,
        hedge_l1_sizes: Option<(f64, f64)>,
        utc_day: i32,
    ) -> Result<Option<MakerHedgeQuotes>> {
        let Some(&idx) = self.pair_by_symbol_id.get(&symbol_id) else {
            return Ok(None);
        };
        let (maker_obs, hedge_obs) = {
            let pair = &mut self.pairs[idx];
            let max_book_age_ms = pair.cfg.strategy.max_book_age_ms;
            let maker_obs = pair.maker_feed.observe_bbo(
                now_ms,
                maker.bid,
                maker.ask,
                maker.recv_ms,
                max_book_age_ms,
            );
            let hedge_obs = pair.hedge_feed.observe_bbo(
                now_ms,
                hedge.bid,
                hedge.ask,
                hedge.recv_ms,
                max_book_age_ms,
            );
            if maker_obs == FeedObservation::Fresh {
                pair.last_maker_touch = Some(maker);
            }
            if hedge_obs == FeedObservation::Fresh {
                pair.last_hedge_touch = Some(hedge);
            }
            (maker_obs, hedge_obs)
        };

        if maker_obs == FeedObservation::ClockRegressed
            || hedge_obs == FeedObservation::ClockRegressed
        {
            self.check_equity_and_drawdown(utc_day);
            let dl_util = self.daily_loss.drawdown_budget_ratio(self.equity_usd());
            if let Some(ref mut fsm) = self.defense {
                let p = &self.pairs[idx];
                fsm.evaluate(&DefenseTelemetrySnapshot {
                    now_ms,
                    feed_staleness_ms: now_ms
                        .saturating_sub(maker.recv_ms)
                        .max(now_ms.saturating_sub(hedge.recv_ms)),
                    feed_gap_ms: p.maker_feed.last_gap_ms.max(p.hedge_feed.last_gap_ms),
                    vol_shock_ratio: (p.quoter.basis_sigma_bps() / p.cfg.strategy.e0_bps.max(1e-6))
                        .max(0.0),
                    trailing_bid_markout_bps: 0.0,
                    trailing_ask_markout_bps: 0.0,
                    oracle_divergence_bps: p
                        .ref_tracker
                        .return_bps(now_ms, &p.cfg.strategy)
                        .unwrap_or(0.0)
                        .abs(),
                    daily_loss_utilization: dl_util,
                    budget_cooldown_active: false,
                    budget_halted: false,
                    clock_regressed_externally: true,
                });
            }
            self.pairs[idx].latest_quotes = None;
            self.pairs[idx].latest_bid_defense_scale = 1.0;
            self.pairs[idx].latest_ask_defense_scale = 1.0;
            return Ok(None);
        }

        let step_res = {
            let pair = &mut self.pairs[idx];
            match hedge_l1_sizes {
                Some((hedge_bid_sz, hedge_ask_sz)) => pair.quoter.step_with_l1(
                    now_ms,
                    maker,
                    hedge,
                    hedge_bid_sz,
                    hedge_ask_sz,
                    pair.inventory_clips,
                ),
                None => pair.quoter.step(now_ms, maker, hedge, pair.inventory_clips),
            }
        };
        self.check_equity_and_drawdown(utc_day);
        let dl_util = self.daily_loss.drawdown_budget_ratio(self.equity_usd());
        if self.defense.is_some() {
            let pair_ref = &self.pairs[idx];
            let snap = DefenseTelemetrySnapshot {
                now_ms,
                feed_staleness_ms: now_ms
                    .saturating_sub(maker.recv_ms)
                    .max(now_ms.saturating_sub(hedge.recv_ms)),
                feed_gap_ms: pair_ref
                    .maker_feed
                    .last_gap_ms
                    .max(pair_ref.hedge_feed.last_gap_ms),
                vol_shock_ratio: (pair_ref.quoter.basis_sigma_bps()
                    / pair_ref.cfg.strategy.e0_bps.max(1e-6))
                .max(0.0),
                trailing_bid_markout_bps: 0.0,
                trailing_ask_markout_bps: 0.0,
                oracle_divergence_bps: pair_ref
                    .ref_tracker
                    .return_bps(now_ms, &pair_ref.cfg.strategy)
                    .unwrap_or(0.0)
                    .abs(),
                daily_loss_utilization: dl_util,
                budget_cooldown_active: false,
                budget_halted: false,
                clock_regressed_externally: false,
            };
            if let Some(ref mut fsm) = self.defense {
                fsm.evaluate(&snap);
            }
        }

        let mut defense_blocked = 0_u64;
        let pair = &mut self.pairs[idx];
        let Ok(mut quotes) = step_res else {
            pair.latest_quotes = None;
            pair.latest_bid_defense_scale = 1.0;
            pair.latest_ask_defense_scale = 1.0;
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
            pair.latest_bid_defense_scale = 1.0;
            pair.latest_ask_defense_scale = 1.0;
            return Ok(None);
        }
        if let Some(ref fsm) = self.defense {
            let bid_is_reduce = pair.inventory_clips < -1e-9;
            let ask_is_reduce = pair.inventory_clips > 1e-9;
            if quotes.bid.is_some() {
                let scale = if bid_is_reduce {
                    1.0
                } else {
                    fsm.entry_size_scale(Some(DefenseSide::Buy))
                };
                if scale <= 0.0 {
                    quotes.bid = None;
                    defense_blocked += 1;
                }
                pair.latest_bid_defense_scale = scale;
            } else {
                pair.latest_bid_defense_scale = 1.0;
            }
            if quotes.ask.is_some() {
                let scale = if ask_is_reduce {
                    1.0
                } else {
                    fsm.entry_size_scale(Some(DefenseSide::Sell))
                };
                if scale <= 0.0 {
                    quotes.ask = None;
                    defense_blocked += 1;
                }
                pair.latest_ask_defense_scale = scale;
            } else {
                pair.latest_ask_defense_scale = 1.0;
            }
        } else {
            pair.latest_bid_defense_scale = 1.0;
            pair.latest_ask_defense_scale = 1.0;
        }
        pair.latest_quotes = Some(quotes);
        self.defense_blocked_entries += defense_blocked;
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
            self.pairs[idx].latest_bid_defense_scale = 1.0;
            self.pairs[idx].latest_ask_defense_scale = 1.0;
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
        let quote_defense_scale = match fill_side {
            FillSide::MakerBought => {
                if self.pairs[idx].inventory_clips < -1e-9 {
                    1.0
                } else {
                    self.pairs[idx].latest_bid_defense_scale
                }
            }
            FillSide::MakerSold => {
                if self.pairs[idx].inventory_clips > 1e-9 {
                    1.0
                } else {
                    self.pairs[idx].latest_ask_defense_scale
                }
            }
        }
        .clamp(0.0, 1.0);

        let hedge_fresh = valid_bid_ask(hedge.bid, hedge.ask)
            && hedge.recv_ms <= now_ms
            && now_ms - hedge.recv_ms <= self.pairs[idx].cfg.max_hedge_stale_ms;
        if !hedge_fresh {
            let pair_clip = self.pairs[idx].cfg.clip_usd;
            let risk_scale = self.daily_loss.risk_scale();
            let fill_notional =
                pair_clip.min(trade_px * trade_sz) * risk_scale.max(0.0) * quote_defense_scale;
            let did_fill = {
                let pair = &mut self.pairs[idx];
                pair.unhedged_stale_halts += 1;
                pair.halted = true;
                pair.latest_quotes = None;
                pair.latest_bid_defense_scale = 1.0;
                pair.latest_ask_defense_scale = 1.0;

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
        if risk_scale <= 0.0 || quote_defense_scale <= 0.0 {
            return Ok(false);
        }
        let pair_clip = self.pairs[idx].cfg.clip_usd;
        let fill_notional = pair_clip.min(trade_px * trade_sz) * risk_scale * quote_defense_scale;
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
        if !valid_bid_ask(maker.bid, maker.ask)
            || maker.recv_ms > now_ms
            || now_ms - maker.recv_ms > self.pairs[idx].cfg.strategy.max_book_age_ms
            || self.pairs[idx]
                .maker_feed
                .last_recv_ms
                .is_some_and(|prev| maker.recv_ms < prev)
        {
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
        let pairs: Vec<Track02PairReport> = self
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
                    maker_feed: p.maker_feed.report(),
                    hedge_feed: p.hedge_feed.report(),
                    ref_feed: p.ref_feed.report(),
                }
            })
            .collect();
        let (
            venue_feed_health,
            max_gap_ms,
            stale_book_drops,
            clock_regression_drops,
            crossed_book_drops,
        ) = aggregate_venue_health(
            pairs
                .iter()
                .flat_map(|p| [p.maker_feed, p.hedge_feed, p.ref_feed]),
        );
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
            defense_state_transitions: self.defense.as_ref().map_or(0, DefenseFsm::transitions),
            defense_blocked_entries: self.defense_blocked_entries,
            defense_state: self.defense.as_ref().map(DefenseFsm::state),
            max_gap_ms,
            stale_book_drops,
            clock_regression_drops,
            crossed_book_drops,
            venue_feed_health,
            pairs,
        }
    }
}

// ============================================================================
// Track 03: 03_incentive_turnover (Queue-Priority G4 Quiet-Regime MM Shadow)
// ============================================================================

fn default_queue_ahead_fraction() -> f64 {
    0.5
}

fn default_track03_account_fee_status() -> String {
    "public_tier0_unverified".to_string()
}

/// Canonical UTC trading session classification consistent with
/// `scripts/research_track03_multivenue_turnover.py`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UtcSession {
    UsRth,
    UsExtended,
    OvernightAsia,
    Weekend,
}

/// Classify Unix epoch milliseconds into canonical UTC session buckets:
/// - `Weekend`: Saturday / Sunday UTC
/// - `OvernightAsia`: 00:00 <= UTC < 11:00
/// - `UsExtended`: 11:00 <= UTC < 13:30 or 20:00 <= UTC < 24:00
/// - `UsRth`: 13:30 <= UTC < 20:00
pub fn classify_utc_session(now_ms: u64) -> UtcSession {
    let days_since_unix = now_ms / 86_400_000;
    // 1970-01-01 was Thursday (index 3 in Mon=0..Sun=6).
    let weekday = (days_since_unix + 3) % 7;
    if weekday >= 5 {
        return UtcSession::Weekend;
    }
    let minute_of_day = ((now_ms / 60_000) % 1_440) as u32;
    if minute_of_day < 660 {
        UtcSession::OvernightAsia
    } else if !(810..1_200).contains(&minute_of_day) {
        UtcSession::UsExtended
    } else {
        UtcSession::UsRth
    }
}

/// Top-of-book snapshot with L1 sizes and local receipt time for Track 03 shadow replay.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
pub struct Track03Touch {
    pub bid: f64,
    pub ask: f64,
    pub bid_sz: f64,
    pub ask_sz: f64,
    pub recv_ms: u64,
}

impl Track03Touch {
    pub fn valid(&self) -> bool {
        valid_bid_ask(self.bid, self.ask)
            && self.bid_sz.is_finite()
            && self.ask_sz.is_finite()
            && self.bid_sz > 0.0
            && self.ask_sz > 0.0
    }

    pub fn mid(&self) -> f64 {
        0.5 * (self.bid + self.ask)
    }
}

/// Summary of one Track 03 BBO step (allocation-free).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Track03StepOutcome {
    pub utc_session: UtcSession,
    pub session_gated: bool,
    pub halted: bool,
    pub resting_orders: usize,
    pub placed: usize,
    pub cancelled: usize,
    pub kept: usize,
    pub taker_exited: bool,
}

/// Shadow configuration for a single Track 03 queue-priority market-making arm.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Track03ArmShadowConfig {
    pub symbol: String,
    pub symbol_id: u16,
    pub target_exchange_id: u8,
    pub ref_exchange_ids: Vec<u8>,
    pub maker_fee_bps: f64,
    pub taker_fee_bps: f64,
    pub rebate_bps: f64,
    #[serde(default = "default_queue_ahead_fraction")]
    pub queue_ahead_fraction: f64,
    pub allowed_utc_sessions: Option<Vec<UtcSession>>,
    #[serde(default = "default_track03_account_fee_status")]
    pub account_fee_status: String,
    #[serde(default)]
    pub ioc_exit_delay_ms: u64,
    #[serde(default = "default_max_l1_depth_fraction")]
    pub max_l1_depth_fraction: f64,
    pub strategy: QueuePriorityMmConfig,
}

impl Default for Track03ArmShadowConfig {
    fn default() -> Self {
        Self {
            symbol: String::new(),
            symbol_id: 0,
            target_exchange_id: 2,
            ref_exchange_ids: Vec::new(),
            maker_fee_bps: 0.0,
            taker_fee_bps: 0.0,
            rebate_bps: 0.0,
            queue_ahead_fraction: default_queue_ahead_fraction(),
            allowed_utc_sessions: None,
            account_fee_status: default_track03_account_fee_status(),
            ioc_exit_delay_ms: 0,
            max_l1_depth_fraction: default_max_l1_depth_fraction(),
            strategy: QueuePriorityMmConfig::default(),
        }
    }
}

/// Isolated shadow session configuration for Track 03 (`03_incentive_turnover`).
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Track03ShadowConfig {
    pub capital_usd: f64,
    pub daily_loss_limit_pct: f64,
    pub daily_loss_state_path: Option<PathBuf>,
    pub allowlisted_symbols: Vec<String>,
    pub arms: Vec<Track03ArmShadowConfig>,
    #[serde(default)]
    pub defense: Option<DefenseFsmConfig>,
    #[serde(default)]
    pub declared_end_ms: Option<u64>,
    #[serde(default)]
    pub terminal_drain_ms: Option<u64>,
}

impl Default for Track03ShadowConfig {
    fn default() -> Self {
        Self {
            capital_usd: 10_000.0,
            daily_loss_limit_pct: 0.02,
            daily_loss_state_path: None,
            allowlisted_symbols: Vec::new(),
            arms: Vec::new(),
            defense: None,
            declared_end_ms: None,
            terminal_drain_ms: None,
        }
    }
}

impl Track03ShadowConfig {
    /// Validate capital, loss limits, allowlist, unique symbol IDs, fee invariants, and G4 configs.
    pub fn validate(&self) -> Result<()> {
        if let Some(ref defense_cfg) = self.defense {
            defense_cfg.validate()?;
        }
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
            let mut seen_refs = HashSet::with_capacity(arm.ref_exchange_ids.len());
            for &ref_id in &arm.ref_exchange_ids {
                ensure!(
                    ref_id != arm.target_exchange_id,
                    "arm '{}' ref_exchange_id {} must differ from target_exchange_id",
                    arm.symbol,
                    ref_id
                );
                ensure!(
                    seen_refs.insert(ref_id),
                    "arm '{}' duplicate ref_exchange_id {}",
                    arm.symbol,
                    ref_id
                );
            }
            ensure!(
                arm.maker_fee_bps.is_finite() && arm.maker_fee_bps >= -5.0,
                "arm '{}' maker_fee_bps invalid",
                arm.symbol
            );
            ensure!(
                arm.taker_fee_bps.is_finite() && arm.taker_fee_bps >= 0.0,
                "arm '{}' taker_fee_bps must be finite and >= 0",
                arm.symbol
            );
            ensure!(
                arm.rebate_bps.is_finite() && arm.rebate_bps >= 0.0,
                "arm '{}' rebate_bps must be finite and >= 0",
                arm.symbol
            );
            ensure!(
                arm.queue_ahead_fraction.is_finite()
                    && (0.0..=1.0).contains(&arm.queue_ahead_fraction),
                "arm '{}' queue_ahead_fraction must be in [0, 1]",
                arm.symbol
            );
            ensure!(
                arm.max_l1_depth_fraction.is_finite()
                    && arm.max_l1_depth_fraction > 0.0
                    && arm.max_l1_depth_fraction <= 1.0,
                "arm '{}' max_l1_depth_fraction must be in (0, 1]",
                arm.symbol
            );
            if let Some(sessions) = &arm.allowed_utc_sessions {
                ensure!(
                    !sessions.is_empty(),
                    "arm '{}' allowed_utc_sessions must be non-empty when set",
                    arm.symbol
                );
            }
            arm.strategy.validate()?;
            ensure!(
                (arm.maker_fee_bps - arm.strategy.maker_fee_bps).abs() <= 1e-9,
                "arm '{}' maker_fee_bps ({}) must match strategy.maker_fee_bps ({})",
                arm.symbol,
                arm.maker_fee_bps,
                arm.strategy.maker_fee_bps
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Track03FillRole {
    Maker,
    Taker,
}

#[derive(Debug, Clone, Copy)]
struct Track03OpenLot {
    sgn: f64,
    qty: f64,
    entry_px: f64,
    entry_mid: f64,
    entry_ts_ms: u64,
}

#[derive(Debug, Clone, Copy)]
struct Track03PendingMarkout {
    eval_ts_ms: u64,
    side: QuoteSide,
    fill_px: f64,
    fill_notional_usd: f64,
}

#[derive(Debug, Clone, Copy, Default)]
struct Track03OrderMeta {
    queue_ahead: f64,
    active_at_ms: u64,
    activated: bool,
    reduce_only: bool,
}

#[derive(Debug, Clone, Copy)]
struct Track03PendingIocExit {
    exec_at_ms: u64,
    side: QuoteSide,
    limit_price: f64,
    qty: f64,
}

struct Track03RefSlot {
    exchange_id: u8,
    venue_id: String,
    feed: VenueFeedTracker,
    last_sample: Option<(u64, f64, f64)>,
    scratch_idx: Option<usize>,
}

const TRACK03_MAX_RESTING: usize = 6;
const TRACK03_MAX_PENDING_MARKOUTS: usize = 256;
const TRACK03_MAX_PENDING_IOC_EXITS: usize = 16;

struct Track03ArmRuntime {
    cfg: Track03ArmShadowConfig,
    engine: QueuePriorityMmEngine,
    target_feed: VenueFeedTracker,
    last_valid_target_touch: Option<Track03Touch>,
    rem_bid_sz: f64,
    rem_ask_sz: f64,
    ref_slots: Vec<Track03RefSlot>,
    scratch_book: VenueBookUpdate,
    scratch_refs: Vec<ReferenceBookUpdate>,
    scratch_resting: Vec<RestingQuote>,
    resting_meta: [Track03OrderMeta; TRACK03_MAX_RESTING],
    resting_seq: u64,
    open_lots: VecDeque<Track03OpenLot>,
    pending_markouts: VecDeque<Track03PendingMarkout>,
    pending_ioc_exits: VecDeque<Track03PendingIocExit>,
    pos_qty: f64,
    realized_gross_usd: f64,
    fees_usd: f64,
    spread_capture_usd: f64,
    holding_drift_usd: f64,
    passive_exit_pnl_usd: f64,
    taker_exit_slippage_usd: f64,
    closed_notional_usd: f64,
    maker_volume_usd: f64,
    taker_volume_usd: f64,
    markout_1s_num_bps_usd: f64,
    markout_1s_den_usd: f64,
    maker_fills: u64,
    taker_exits: u64,
    depth_clipped_exits: u64,
    stale_exit_blocks: u64,
    cycles: u64,
    orders_placed: u64,
    cancels: u64,
    queue_preserved: u64,
    alo_rejects: u64,
    session_gated_ticks: u64,
    total_book_ticks: u64,
    halted: bool,
}

impl Track03ArmRuntime {
    fn realized_pnl_usd(&self) -> f64 {
        self.realized_gross_usd - self.fees_usd
    }

    fn unrealized_pnl_usd(&self) -> f64 {
        let Some(touch) = self.last_valid_target_touch else {
            return 0.0;
        };
        let mid = touch.mid();
        self.open_lots
            .iter()
            .map(|lot| lot.sgn * lot.qty * (mid - lot.entry_px))
            .sum()
    }

    fn total_pnl_usd(&self) -> f64 {
        self.realized_pnl_usd() + self.unrealized_pnl_usd()
    }

    fn session_allows(&self, session: UtcSession) -> bool {
        self.cfg
            .allowed_utc_sessions
            .as_ref()
            .is_none_or(|allowed| allowed.contains(&session))
    }

    fn is_pure_reducing_order(&self, side: QuoteSide, qty: f64, meta: Track03OrderMeta) -> bool {
        let half_lot = self.cfg.strategy.lot_size * 0.5;
        if meta.reduce_only {
            return match side {
                QuoteSide::Bid => self.pos_qty < -half_lot && qty <= -self.pos_qty + 1e-12,
                QuoteSide::Ask => self.pos_qty > half_lot && qty <= self.pos_qty + 1e-12,
            };
        }
        match side {
            QuoteSide::Bid => self.pos_qty < -half_lot && qty <= -self.pos_qty + 1e-12,
            QuoteSide::Ask => self.pos_qty > half_lot && qty <= self.pos_qty + 1e-12,
        }
    }

    fn remove_resting_at(&mut self, idx: usize) {
        let last = self.scratch_resting.len() - 1;
        self.scratch_resting.swap_remove(idx);
        if idx < last {
            self.resting_meta[idx] = self.resting_meta[last];
        }
        self.resting_meta[last] = Track03OrderMeta::default();
    }

    fn clear_all_resting(&mut self) -> usize {
        let cancelled = self.scratch_resting.len();
        self.scratch_resting.clear();
        self.resting_meta = [Track03OrderMeta::default(); TRACK03_MAX_RESTING];
        self.cancels += cancelled as u64;
        cancelled
    }

    fn cancel_opening_resting(&mut self) -> usize {
        let mut cancelled = 0;
        let mut idx = 0;
        while idx < self.scratch_resting.len() {
            let side = self.scratch_resting[idx].side;
            let qty = self.scratch_resting[idx].qty;
            let meta = self.resting_meta[idx];
            if self.is_pure_reducing_order(side, qty, meta) {
                idx += 1;
            } else {
                self.remove_resting_at(idx);
                self.cancels += 1;
                cancelled += 1;
            }
        }
        cancelled
    }

    fn mature_markouts(&mut self, mid_now: f64, now_ms: u64) {
        if !(mid_now.is_finite() && mid_now > 0.0) {
            return;
        }
        while let Some(&pf) = self.pending_markouts.front() {
            if now_ms < pf.eval_ts_ms {
                break;
            }
            self.pending_markouts.pop_front();
            if pf.fill_px <= 0.0 || pf.fill_notional_usd <= 0.0 {
                continue;
            }
            let sgn = match pf.side {
                QuoteSide::Bid => 1.0,
                QuoteSide::Ask => -1.0,
            };
            let mk_bps = sgn * (mid_now - pf.fill_px) / pf.fill_px * 1e4;
            if mk_bps.is_finite() {
                self.markout_1s_num_bps_usd += pf.fill_notional_usd * mk_bps;
                self.markout_1s_den_usd += pf.fill_notional_usd;
            }
        }
    }

    fn activate_arrivals(&mut self, now_ms: u64, bb: f64, bsz: f64, ba: f64, asz: f64) {
        let tol_25 = self.cfg.strategy.tick_size * 0.25;
        let mut idx = 0;
        while idx < self.scratch_resting.len() {
            if self.resting_meta[idx].activated {
                let ord = &self.scratch_resting[idx];
                match ord.side {
                    QuoteSide::Bid
                        if (ord.price - bb).abs() <= tol_25
                            && bsz < self.resting_meta[idx].queue_ahead =>
                    {
                        self.resting_meta[idx].queue_ahead = bsz;
                    }
                    QuoteSide::Ask
                        if (ord.price - ba).abs() <= tol_25
                            && asz < self.resting_meta[idx].queue_ahead =>
                    {
                        self.resting_meta[idx].queue_ahead = asz;
                    }
                    _ => {}
                }
                idx += 1;
                continue;
            }
            if now_ms < self.resting_meta[idx].active_at_ms {
                idx += 1;
                continue;
            }
            let side = self.scratch_resting[idx].side;
            let px = self.scratch_resting[idx].price;
            let crosses = match side {
                QuoteSide::Bid => px >= ba - 1e-12,
                QuoteSide::Ask => px <= bb + 1e-12,
            };
            if crosses {
                self.remove_resting_at(idx);
                self.alo_rejects += 1;
                self.engine.on_alo_reject(side, now_ms);
                continue;
            }
            let queue_ahead = match side {
                QuoteSide::Bid if (px - bb).abs() <= tol_25 || px <= bb + tol_25 => {
                    self.cfg.queue_ahead_fraction * bsz
                }
                QuoteSide::Ask if (px - ba).abs() <= tol_25 || px >= ba - tol_25 => {
                    self.cfg.queue_ahead_fraction * asz
                }
                _ => 0.0,
            };
            self.resting_meta[idx].queue_ahead = queue_ahead;
            self.resting_meta[idx].activated = true;
            self.engine.on_alo_accepted(side);
            idx += 1;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn record_fill(
        &mut self,
        ts_ms: u64,
        side: QuoteSide,
        fill_px: f64,
        fill_qty: f64,
        role: Track03FillRole,
        mid_now: f64,
        notify_markout_shield: bool,
    ) {
        if !(fill_qty.is_finite() && fill_qty > 1e-12 && fill_px.is_finite() && fill_px > 0.0) {
            return;
        }
        let fill_sgn = match side {
            QuoteSide::Bid => 1.0,
            QuoteSide::Ask => -1.0,
        };
        let notl = fill_qty * fill_px;
        let fee_bp = match role {
            Track03FillRole::Maker => {
                self.maker_volume_usd += notl;
                self.maker_fills += 1;
                if self.pending_markouts.len() >= TRACK03_MAX_PENDING_MARKOUTS {
                    self.pending_markouts.pop_front();
                }
                self.pending_markouts.push_back(Track03PendingMarkout {
                    eval_ts_ms: ts_ms.saturating_add(1_000),
                    side,
                    fill_px,
                    fill_notional_usd: notl,
                });
                if notify_markout_shield {
                    self.engine.on_fill(side, fill_px, fill_qty, ts_ms);
                }
                self.cfg.maker_fee_bps - self.cfg.rebate_bps
            }
            Track03FillRole::Taker => {
                self.taker_volume_usd += notl;
                self.taker_exits += 1;
                self.cfg.taker_fee_bps
            }
        };
        self.fees_usd += notl * (fee_bp * 1e-4);

        let mut rem = fill_qty;
        let mut closed_any = false;
        while rem > 1e-12 {
            let Some(lot) = self.open_lots.front_mut() else {
                break;
            };
            if (lot.sgn - fill_sgn).abs() < 0.5 {
                break;
            }
            let closed_qty = rem.min(lot.qty);
            let entry_spr = closed_qty * lot.sgn * (lot.entry_mid - lot.entry_px);
            let drft_part = closed_qty * lot.sgn * (mid_now - lot.entry_mid);
            let exit_edge = closed_qty * lot.sgn * (fill_px - mid_now);
            self.spread_capture_usd += entry_spr;
            self.holding_drift_usd += drft_part;
            match role {
                Track03FillRole::Maker => self.passive_exit_pnl_usd += exit_edge,
                Track03FillRole::Taker => self.taker_exit_slippage_usd += exit_edge,
            }
            self.realized_gross_usd += entry_spr + drft_part + exit_edge;
            self.closed_notional_usd += closed_qty * fill_px;
            closed_any = true;
            rem -= closed_qty;
            lot.qty -= closed_qty;
            if lot.qty <= 1e-12 {
                self.open_lots.pop_front();
            }
        }
        if closed_any {
            self.cycles += 1;
        }
        if rem > 1e-12 {
            self.open_lots.push_back(Track03OpenLot {
                sgn: fill_sgn,
                qty: rem,
                entry_px: fill_px,
                entry_mid: mid_now,
                entry_ts_ms: ts_ms,
            });
        }
        self.pos_qty = self.open_lots.iter().map(|l| l.sgn * l.qty).sum();
        if self.open_lots.is_empty() || self.pos_qty.abs() <= 1e-12 {
            self.pos_qty = 0.0;
        }
    }

    fn cost_decomposition(&self) -> (f64, f64, f64, f64, f64, f64, f64) {
        let ntl = self.closed_notional_usd;
        if ntl <= 0.0 {
            let eff_maker_bp = self.cfg.maker_fee_bps - self.cfg.rebate_bps;
            let double_sided_usd_per_m = eff_maker_bp * 100.0;
            return (0.0, 0.0, 0.0, 0.0, 0.0, 0.0, double_sided_usd_per_m);
        }
        let spread_capture_bps = (self.spread_capture_usd / ntl) * 1e4;
        let holding_drift_bps = (self.holding_drift_usd / ntl) * 1e4;
        let passive_exit_pnl_bps = (self.passive_exit_pnl_usd / ntl) * 1e4;
        let taker_exit_slippage_bps = (self.taker_exit_slippage_usd / ntl) * 1e4;
        let gross_ex_fee_bps = (self.realized_gross_usd / ntl) * 1e4;
        let s = (self.taker_volume_usd / ntl).clamp(0.0, 1.0);
        let eff_maker_bp = self.cfg.maker_fee_bps - self.cfg.rebate_bps;
        let fee_cycle_bp = eff_maker_bp * (2.0 - s) + self.cfg.taker_fee_bps * s;
        let net_cost_cycle_bp = fee_cycle_bp - gross_ex_fee_bps;
        let double_sided_usd_per_m = (net_cost_cycle_bp / 2.0) * 100.0;
        (
            spread_capture_bps,
            holding_drift_bps,
            passive_exit_pnl_bps,
            taker_exit_slippage_bps,
            gross_ex_fee_bps,
            s,
            double_sided_usd_per_m,
        )
    }

    fn execute_ioc_exit(
        &mut self,
        now_ms: u64,
        side: QuoteSide,
        limit_price: f64,
        requested_qty: f64,
        touch: Track03Touch,
    ) -> bool {
        if !touch.valid() {
            return false;
        }
        let (exec_px, rem_avail, crosses_limit) = match side {
            QuoteSide::Bid => (touch.ask, self.rem_ask_sz, touch.ask <= limit_price + 1e-9),
            QuoteSide::Ask => (touch.bid, self.rem_bid_sz, touch.bid >= limit_price - 1e-9),
        };
        let needed = match side {
            QuoteSide::Bid => (-self.pos_qty).max(0.0).min(requested_qty),
            QuoteSide::Ask => self.pos_qty.max(0.0).min(requested_qty),
        };
        if !(needed > 1e-12 && crosses_limit) {
            return false;
        }
        let max_fill_qty = rem_avail.max(0.0);
        let exec_qty = needed.min(max_fill_qty);
        if exec_qty + 1e-12 < needed {
            self.depth_clipped_exits += 1;
        }
        if exec_qty > 1e-12 {
            match side {
                QuoteSide::Bid => self.rem_ask_sz = (self.rem_ask_sz - exec_qty).max(0.0),
                QuoteSide::Ask => self.rem_bid_sz = (self.rem_bid_sz - exec_qty).max(0.0),
            }
            self.record_fill(
                now_ms,
                side,
                exec_px,
                exec_qty,
                Track03FillRole::Taker,
                touch.mid(),
                false,
            );
            true
        } else {
            false
        }
    }

    fn queue_or_exec_ioc_exit(
        &mut self,
        now_ms: u64,
        side: QuoteSide,
        limit_price: f64,
        qty: f64,
        touch: Track03Touch,
    ) -> bool {
        if self.cfg.ioc_exit_delay_ms == 0 {
            return self.execute_ioc_exit(now_ms, side, limit_price, qty, touch);
        }
        let desired = match side {
            QuoteSide::Bid => (-self.pos_qty).max(0.0).min(qty),
            QuoteSide::Ask => self.pos_qty.max(0.0).min(qty),
        };
        let already_pending: f64 = self
            .pending_ioc_exits
            .iter()
            .filter(|p| p.side == side)
            .map(|p| p.qty)
            .sum();
        let needed = desired - already_pending;
        if needed > 1e-12 && self.pending_ioc_exits.len() < TRACK03_MAX_PENDING_IOC_EXITS {
            self.pending_ioc_exits.push_back(Track03PendingIocExit {
                exec_at_ms: now_ms.saturating_add(self.cfg.ioc_exit_delay_ms),
                side,
                limit_price,
                qty: needed,
            });
        }
        false
    }

    fn process_pending_ioc_exits(
        &mut self,
        now_ms: u64,
        prev_touch: Option<Track03Touch>,
        book_fresh: bool,
    ) -> bool {
        let mut exited = false;
        let mut rem = self.pending_ioc_exits.len();
        while rem > 0 {
            rem -= 1;
            let Some(p) = self.pending_ioc_exits.pop_front() else {
                break;
            };
            if now_ms < p.exec_at_ms {
                self.pending_ioc_exits.push_back(p);
                continue;
            }
            let Some(touch) = prev_touch.filter(|_| book_fresh) else {
                self.stale_exit_blocks += 1;
                if self.pending_ioc_exits.len() < TRACK03_MAX_PENDING_IOC_EXITS {
                    self.pending_ioc_exits.push_back(Track03PendingIocExit {
                        exec_at_ms: now_ms.saturating_add(self.cfg.ioc_exit_delay_ms),
                        side: p.side,
                        limit_price: p.limit_price,
                        qty: p.qty,
                    });
                }
                continue;
            };
            if self.execute_ioc_exit(now_ms, p.side, p.limit_price, p.qty, touch) {
                exited = true;
            }
        }
        exited
    }
}

/// Per-arm shadow summary for Track 03 (`03_incentive_turnover`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Track03ArmReport {
    pub symbol: String,
    pub symbol_id: u16,
    pub target_exchange_id: u8,
    pub ref_exchange_ids: Vec<u8>,
    pub account_fee_status: String,
    pub realized_pnl_usd: f64,
    pub unrealized_pnl_usd: f64,
    pub fees_usd: f64,
    pub spread_capture_usd: f64,
    pub holding_drift_usd: f64,
    pub passive_exit_pnl_usd: f64,
    pub taker_exit_slippage_usd: f64,
    pub spread_capture_bps: f64,
    pub holding_drift_bps: f64,
    pub passive_exit_pnl_bps: f64,
    pub taker_exit_slippage_bps: f64,
    pub gross_ex_fee_bps: f64,
    pub double_sided_usd_per_m: f64,
    pub maker_volume_usd: f64,
    pub taker_volume_usd: f64,
    pub closed_notional_usd: f64,
    pub taker_exit_share_s: f64,
    pub markout_1s_bps: Option<f64>,
    pub markout_1s_den_usd: f64,
    pub position_qty: f64,
    pub position_notional_usd: f64,
    pub terminal_residual_notional_usd: f64,
    pub fills: u64,
    pub maker_fills: u64,
    pub taker_exits: u64,
    pub depth_clipped_exits: u64,
    pub stale_exit_blocks: u64,
    pub cycles: u64,
    pub orders_placed: u64,
    pub cancels: u64,
    pub queue_preserved: u64,
    pub alo_rejects: u64,
    pub session_gated_ticks: u64,
    pub total_book_ticks: u64,
    pub halted: bool,
    pub target_feed: VenueFeedHealthReport,
    pub ref_feeds: Vec<VenueFeedHealthReport>,
}

/// Aggregate shadow summary for Track 03 (`03_incentive_turnover`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Track03ShadowReport {
    pub track: &'static str,
    pub fill_evidence_mode: &'static str,
    pub fill_evidence_note: &'static str,
    pub capital_usd: f64,
    pub equity_usd: f64,
    pub realized_pnl_usd: f64,
    pub unrealized_pnl_usd: f64,
    pub fees_usd: f64,
    pub spread_capture_bps: f64,
    pub holding_drift_bps: f64,
    pub passive_exit_pnl_bps: f64,
    pub taker_exit_slippage_bps: f64,
    pub gross_ex_fee_bps: f64,
    pub double_sided_usd_per_m: f64,
    pub maker_volume_usd: f64,
    pub taker_volume_usd: f64,
    pub closed_notional_usd: f64,
    pub taker_exit_share_s: f64,
    pub markout_1s_bps: Option<f64>,
    pub max_drawdown_usd: f64,
    pub fills: u64,
    pub maker_fills: u64,
    pub taker_exits: u64,
    pub depth_clipped_exits: u64,
    pub stale_exit_blocks: u64,
    pub terminal_residual_notional_usd: f64,
    pub cycles: u64,
    pub orders_placed: u64,
    pub cancels: u64,
    pub queue_preserved: u64,
    pub alo_rejects: u64,
    pub session_gated_ticks: u64,
    pub risk_scale: f64,
    pub halted: bool,
    pub defense_state_transitions: u64,
    pub defense_blocked_entries: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub defense_state: Option<DefenseState>,
    pub max_gap_ms: u64,
    pub stale_book_drops: u64,
    pub clock_regression_drops: u64,
    pub crossed_book_drops: u64,
    pub venue_feed_health: Vec<VenueFeedHealthReport>,
    pub arms: Vec<Track03ArmReport>,
}

/// Decoupled Track 03 queue-priority G4 shadow session with isolated [`DailyLossGuard`].
pub struct Track03ShadowSession {
    cfg: Track03ShadowConfig,
    arms: Vec<Track03ArmRuntime>,
    arm_by_symbol_id: HashMap<u16, usize>,
    daily_loss: DailyLossGuard,
    defense: Option<DefenseFsm>,
    defense_blocked_entries: u64,
    fill_evidence_mode: &'static str,
    halted: bool,
    peak_equity_usd: f64,
    max_drawdown_usd: f64,
}

impl Track03ShadowSession {
    pub fn new(cfg: Track03ShadowConfig, initial_utc_day: i32) -> Result<Self> {
        cfg.validate()?;
        let defense = cfg.defense.map(DefenseFsm::new).transpose()?;
        let daily_loss = DailyLossGuard::open(
            cfg.daily_loss_state_path.clone(),
            cfg.daily_loss_limit_pct,
            cfg.capital_usd,
            initial_utc_day,
        )?;
        let mut arms = Vec::with_capacity(cfg.arms.len());
        let mut arm_by_symbol_id = HashMap::with_capacity(cfg.arms.len());
        for (idx, arm_cfg) in cfg.arms.iter().enumerate() {
            let engine = QueuePriorityMmEngine::new(arm_cfg.strategy.clone())?;
            arm_by_symbol_id.insert(arm_cfg.symbol_id, idx);
            let ref_slots = arm_cfg
                .ref_exchange_ids
                .iter()
                .map(|&exch_id| Track03RefSlot {
                    exchange_id: exch_id,
                    venue_id: format!("exch_{exch_id}"),
                    feed: VenueFeedTracker::new(exch_id),
                    last_sample: None,
                    scratch_idx: None,
                })
                .collect();
            arms.push(Track03ArmRuntime {
                cfg: arm_cfg.clone(),
                engine,
                target_feed: VenueFeedTracker::new(arm_cfg.target_exchange_id),
                last_valid_target_touch: None,
                rem_bid_sz: 0.0,
                rem_ask_sz: 0.0,
                ref_slots,
                scratch_book: VenueBookUpdate {
                    bids: Vec::with_capacity(8),
                    asks: Vec::with_capacity(8),
                    received_at_ms: 0,
                    source_ms: None,
                },
                scratch_refs: Vec::with_capacity(arm_cfg.ref_exchange_ids.len()),
                scratch_resting: Vec::with_capacity(TRACK03_MAX_RESTING),
                resting_meta: [Track03OrderMeta::default(); TRACK03_MAX_RESTING],
                resting_seq: 0,
                open_lots: VecDeque::with_capacity(32),
                pending_markouts: VecDeque::with_capacity(TRACK03_MAX_PENDING_MARKOUTS),
                pending_ioc_exits: VecDeque::with_capacity(TRACK03_MAX_PENDING_IOC_EXITS),
                pos_qty: 0.0,
                realized_gross_usd: 0.0,
                fees_usd: 0.0,
                spread_capture_usd: 0.0,
                holding_drift_usd: 0.0,
                passive_exit_pnl_usd: 0.0,
                taker_exit_slippage_usd: 0.0,
                closed_notional_usd: 0.0,
                maker_volume_usd: 0.0,
                taker_volume_usd: 0.0,
                markout_1s_num_bps_usd: 0.0,
                markout_1s_den_usd: 0.0,
                maker_fills: 0,
                taker_exits: 0,
                depth_clipped_exits: 0,
                stale_exit_blocks: 0,
                cycles: 0,
                orders_placed: 0,
                cancels: 0,
                queue_preserved: 0,
                alo_rejects: 0,
                session_gated_ticks: 0,
                total_book_ticks: 0,
                halted: false,
            });
        }
        Ok(Self {
            peak_equity_usd: cfg.capital_usd,
            cfg,
            arms,
            arm_by_symbol_id,
            daily_loss,
            defense,
            defense_blocked_entries: 0,
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
            "BboMatrix SHM has no public trade prints; Track 03 fills are conservative BBO-cross sweep diagnostics only"
        } else {
            "trade_print_fifo_queue_and_strict_trade_through"
        }
    }

    pub fn config(&self) -> &Track03ShadowConfig {
        &self.cfg
    }

    fn past_declared_end(&self, now_ms: u64) -> bool {
        self.cfg.declared_end_ms.is_some_and(|end| now_ms > end)
    }

    fn in_terminal_drain(&self, now_ms: u64) -> bool {
        matches!(
            (self.cfg.declared_end_ms, self.cfg.terminal_drain_ms),
            (Some(end_ms), Some(drain_ms)) if drain_ms > 0 && now_ms >= end_ms.saturating_sub(drain_ms)
        )
    }

    pub fn is_halted(&self) -> bool {
        self.halted
    }

    pub fn defense_state(&self) -> Option<DefenseState> {
        self.defense.as_ref().map(DefenseFsm::state)
    }

    pub fn acknowledge_defense_reset(
        &mut self,
        snapshot: &DefenseTelemetrySnapshot,
    ) -> std::result::Result<DefenseState, ResetBlockedReason> {
        match self.defense.as_mut() {
            Some(fsm) => fsm.acknowledge_reconciled_reset(snapshot),
            None => Err(ResetBlockedReason::NotHalted),
        }
    }

    pub fn arm_ids(&self) -> impl Iterator<Item = (u16, u8, &[u8])> + '_ {
        self.arms.iter().map(|a| {
            (
                a.cfg.symbol_id,
                a.cfg.target_exchange_id,
                a.cfg.ref_exchange_ids.as_slice(),
            )
        })
    }

    fn equity_usd(&self) -> f64 {
        let pnl: f64 = self.arms.iter().map(Track03ArmRuntime::total_pnl_usd).sum();
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
            for arm in &mut self.arms {
                arm.cancel_opening_resting();
            }
        }
    }

    /// Clear all resting quotes for `symbol_id` (fail-closed on SHM invalidation/disconnect).
    pub fn clear_arm_quotes(&mut self, symbol_id: u16) {
        if let Some(&idx) = self.arm_by_symbol_id.get(&symbol_id) {
            self.arms[idx].clear_all_resting();
        }
    }

    /// Record a crossed or malformed SHM book drop detected before `on_target_bbo` / `on_reference_touch`.
    pub fn record_venue_crossed_drop(&mut self, symbol_id: u16, exchange_id: u8) {
        let Some(&idx) = self.arm_by_symbol_id.get(&symbol_id) else {
            return;
        };
        let arm = &mut self.arms[idx];
        if arm.cfg.target_exchange_id == exchange_id {
            arm.target_feed.record_crossed_drop();
        }
        for slot in &mut arm.ref_slots {
            if slot.exchange_id == exchange_id {
                slot.feed.record_crossed_drop();
            }
        }
    }

    /// Feed a timestamped external reference BBO snapshot into the arm's multi-venue fair anchor.
    ///
    /// Rejects stale, future, clock-regressed, or crossed reference books without overwriting
    /// healthy history, and deduplicates unchanged `(recv_ms, bid, ask)` SHM snapshots.
    pub fn on_reference_touch(
        &mut self,
        symbol_id: u16,
        exchange_id: u8,
        now_ms: u64,
        ref_touch: Track03Touch,
    ) -> bool {
        if self.past_declared_end(now_ms) {
            return false;
        }
        let Some(&idx) = self.arm_by_symbol_id.get(&symbol_id) else {
            return false;
        };
        let arm = &mut self.arms[idx];
        let max_book_age_ms = arm.cfg.strategy.max_book_age_ms;
        let Some(slot) = arm
            .ref_slots
            .iter_mut()
            .find(|s| s.exchange_id == exchange_id)
        else {
            return false;
        };
        let obs = slot.feed.observe_bbo(
            now_ms,
            ref_touch.bid,
            ref_touch.ask,
            ref_touch.recv_ms,
            max_book_age_ms,
        );
        if obs != FeedObservation::Fresh {
            return false;
        }
        let sample = (ref_touch.recv_ms, ref_touch.bid, ref_touch.ask);
        if slot.last_sample == Some(sample) {
            return false;
        }
        slot.last_sample = Some(sample);
        if let Some(ref_idx) = slot.scratch_idx {
            let entry = &mut arm.scratch_refs[ref_idx];
            entry.bid = ref_touch.bid;
            entry.ask = ref_touch.ask;
            entry.received_at_ms = ref_touch.recv_ms;
        } else {
            let ref_idx = arm.scratch_refs.len();
            slot.scratch_idx = Some(ref_idx);
            arm.scratch_refs.push(ReferenceBookUpdate {
                venue_id: slot.venue_id.clone(),
                bid: ref_touch.bid,
                ask: ref_touch.ask,
                received_at_ms: ref_touch.recv_ms,
            });
        }
        true
    }

    /// Process one target L1 BBO update for `symbol_id`.
    pub fn on_target_bbo(
        &mut self,
        symbol_id: u16,
        now_ms: u64,
        target: Track03Touch,
        utc_day: i32,
    ) -> Result<Option<Track03StepOutcome>> {
        if self.past_declared_end(now_ms) {
            return Ok(None);
        }
        if !target.valid() {
            let Some(&idx) = self.arm_by_symbol_id.get(&symbol_id) else {
                return Ok(None);
            };
            {
                let arm = &mut self.arms[idx];
                arm.target_feed.record_crossed_drop();
                arm.clear_all_resting();
                if arm
                    .target_feed
                    .last_now_ms
                    .is_none_or(|prev| now_ms >= prev)
                {
                    let prev_touch = arm.last_valid_target_touch;
                    let _ = arm.process_pending_ioc_exits(now_ms, prev_touch, false);
                }
            }
            self.check_equity_and_drawdown(utc_day);
            return Ok(None);
        }
        let bids = [(target.bid, target.bid_sz)];
        let asks = [(target.ask, target.ask_sz)];
        self.on_target_l2_bbo(symbol_id, now_ms, &bids, &asks, target.recv_ms, utc_day)
    }

    /// Process one target L2/BBO snapshot for `symbol_id` using pre-allocated book buffers.
    pub fn on_target_l2_bbo(
        &mut self,
        symbol_id: u16,
        now_ms: u64,
        bids: &[(f64, f64)],
        asks: &[(f64, f64)],
        recv_ms: u64,
        utc_day: i32,
    ) -> Result<Option<Track03StepOutcome>> {
        if self.past_declared_end(now_ms) {
            return Ok(None);
        }
        let Some(&idx) = self.arm_by_symbol_id.get(&symbol_id) else {
            return Ok(None);
        };
        let valid_levels = |levels: &[(f64, f64)]| -> bool {
            !levels.is_empty()
                && levels
                    .iter()
                    .all(|(p, s)| p.is_finite() && s.is_finite() && *p > 0.0 && *s > 0.0)
        };
        if !valid_levels(bids) || !valid_levels(asks) {
            {
                let arm = &mut self.arms[idx];
                arm.target_feed.record_crossed_drop();
                arm.clear_all_resting();
                if arm
                    .target_feed
                    .last_now_ms
                    .is_none_or(|prev| now_ms >= prev)
                {
                    let prev_touch = arm.last_valid_target_touch;
                    let _ = arm.process_pending_ioc_exits(now_ms, prev_touch, false);
                }
            }
            self.check_equity_and_drawdown(utc_day);
            return Ok(None);
        }
        let (bb, bsz) = bids[0];
        let (ba, asz) = asks[0];

        let obs = {
            let arm = &mut self.arms[idx];
            let max_book_age_ms = arm.cfg.strategy.max_book_age_ms;
            arm.target_feed
                .observe_bbo(now_ms, bb, ba, recv_ms, max_book_age_ms)
        };
        if obs != FeedObservation::Fresh {
            {
                let arm = &mut self.arms[idx];
                arm.clear_all_resting();
                if obs != FeedObservation::ClockRegressed {
                    let prev_touch = arm.last_valid_target_touch;
                    let _ = arm.process_pending_ioc_exits(now_ms, prev_touch, false);
                }
            }
            self.check_equity_and_drawdown(utc_day);
            let dl_util = self.daily_loss.drawdown_budget_ratio(self.equity_usd());
            if let Some(ref mut fsm) = self.defense {
                let arm_ref = &self.arms[idx];
                let tel = arm_ref.engine.telemetry();
                let vol_base = arm_ref
                    .cfg
                    .strategy
                    .quiet_regime_max_entry_vol_bps
                    .filter(|&v| v > 0.0)
                    .unwrap_or_else(|| arm_ref.cfg.strategy.lead_cancel_threshold_bps.max(1.0));
                let vol_shock_ratio = (tel.sigma_bps_per_sqrt_s / vol_base).max(0.0);
                fsm.evaluate(&DefenseTelemetrySnapshot {
                    now_ms,
                    feed_staleness_ms: now_ms.saturating_sub(recv_ms),
                    feed_gap_ms: arm_ref.target_feed.last_gap_ms,
                    vol_shock_ratio,
                    trailing_bid_markout_bps: tel.bid_markout_ewma_bps,
                    trailing_ask_markout_bps: tel.ask_markout_ewma_bps,
                    oracle_divergence_bps: tel.lead_gap_bps.abs(),
                    daily_loss_utilization: dl_util,
                    budget_cooldown_active: false,
                    budget_halted: false,
                    clock_regressed_externally: obs == FeedObservation::ClockRegressed,
                });
            }
            return Ok(None);
        }

        let target_touch = Track03Touch {
            bid: bb,
            ask: ba,
            bid_sz: bsz,
            ask_sz: asz,
            recv_ms,
        };
        let in_terminal_drain = self.in_terminal_drain(now_ms);
        let delayed_taker_exited = {
            let arm = &mut self.arms[idx];
            // P0 Defect A: evaluate pending IOCs against the PRIOR touch & freshness
            // BEFORE installing the new touch so expired stale IOCs re-queue cleanly.
            let prev_touch = arm.last_valid_target_touch;
            let prev_book_fresh = prev_touch.is_some_and(|pt| {
                now_ms >= pt.recv_ms && now_ms - pt.recv_ms <= arm.cfg.strategy.max_book_age_ms
            });
            let ioc_exited = arm.process_pending_ioc_exits(now_ms, prev_touch, prev_book_fresh);

            arm.last_valid_target_touch = Some(target_touch);
            arm.rem_bid_sz = (bsz * arm.cfg.max_l1_depth_fraction).max(0.0);
            arm.rem_ask_sz = (asz * arm.cfg.max_l1_depth_fraction).max(0.0);
            arm.total_book_ticks += 1;
            arm.scratch_book.bids.clear();
            arm.scratch_book.bids.extend_from_slice(bids);
            arm.scratch_book.asks.clear();
            arm.scratch_book.asks.extend_from_slice(asks);
            arm.scratch_book.received_at_ms = recv_ms;
            arm.scratch_book.source_ms = Some(recv_ms);

            arm.mature_markouts(target_touch.mid(), now_ms);
            arm.activate_arrivals(now_ms, bb, bsz, ba, asz);
            ioc_exited
        };

        // Check MTM equity & daily loss brake before sizing any new quotes.
        self.check_equity_and_drawdown(utc_day);
        let risk_scale = if self.halted {
            0.0
        } else {
            self.daily_loss.risk_scale()
        };
        let session = classify_utc_session(now_ms);
        let session_halted = self.halted;
        let dl_util = self.daily_loss.drawdown_budget_ratio(self.equity_usd());

        let (
            placed,
            cancelled,
            kept,
            taker_exited,
            sess_blocked,
            resting_orders,
            arm_halted,
            defense_blocked_entries,
        ) = {
            let arm = &mut self.arms[idx];
            let sess_blocked = !arm.session_allows(session);
            if sess_blocked {
                arm.session_gated_ticks += 1;
            }
            let inv = InventoryState {
                target_qty: arm.pos_qty,
                external_net_qty: 0.0,
                inventory_since_ms: arm.open_lots.front().map(|l| l.entry_ts_ms),
                realized_pnl_usd: arm.realized_pnl_usd(),
                unrealized_pnl_usd: arm.unrealized_pnl_usd(),
            };
            let actions = arm.engine.evaluate(
                Some(&arm.scratch_book),
                &arm.scratch_refs,
                &arm.scratch_resting,
                inv,
                now_ms,
            );

            if let Some(ref mut fsm) = self.defense {
                let tel = arm.engine.telemetry();
                let vol_base = arm
                    .cfg
                    .strategy
                    .quiet_regime_max_entry_vol_bps
                    .filter(|&v| v > 0.0)
                    .unwrap_or_else(|| arm.cfg.strategy.lead_cancel_threshold_bps.max(1.0));
                let vol_shock_ratio = (tel.sigma_bps_per_sqrt_s / vol_base).max(0.0);
                fsm.evaluate(&DefenseTelemetrySnapshot {
                    now_ms,
                    feed_staleness_ms: now_ms.saturating_sub(recv_ms),
                    feed_gap_ms: arm.target_feed.last_gap_ms,
                    vol_shock_ratio,
                    trailing_bid_markout_bps: tel.bid_markout_ewma_bps,
                    trailing_ask_markout_bps: tel.ask_markout_ewma_bps,
                    oracle_divergence_bps: tel.lead_gap_bps.abs(),
                    daily_loss_utilization: dl_util,
                    budget_cooldown_active: false,
                    budget_halted: false,
                    clock_regressed_externally: false,
                });
            }
            let defense_ref = self.defense.as_ref();
            let mut defense_blocked_entries = 0_u64;

            let mut placed = 0_usize;
            let mut cancelled = 0_usize;
            let mut kept = 0_usize;
            let mut taker_exited = delayed_taker_exited;
            let suppress_openings = sess_blocked
                || session_halted
                || arm.halted
                || risk_scale <= 0.0
                || in_terminal_drain;
            let delay_ms = arm.cfg.strategy.submit_rtt_ms.max(0.0).round() as u64;
            let tol_25 = arm.cfg.strategy.tick_size * 0.25;

            for act in actions {
                match act {
                    MmAction::Cancel { client_id, .. } => {
                        if let Some(pos) = arm
                            .scratch_resting
                            .iter()
                            .position(|r| r.client_id == client_id)
                        {
                            arm.remove_resting_at(pos);
                            arm.cancels += 1;
                            cancelled += 1;
                        }
                    }
                    MmAction::Keep {
                        client_id, side, ..
                    } => {
                        if let Some(pos) = arm
                            .scratch_resting
                            .iter()
                            .position(|r| r.client_id == client_id)
                        {
                            let qty = arm.scratch_resting[pos].qty;
                            let meta = arm.resting_meta[pos];
                            let is_pure_reduce = arm.is_pure_reducing_order(side, qty, meta);
                            let def_side = match side {
                                QuoteSide::Bid => DefenseSide::Buy,
                                QuoteSide::Ask => DefenseSide::Sell,
                            };
                            let defense_blocks_keep = !is_pure_reduce
                                && defense_ref.is_some_and(|fsm| {
                                    !fsm.permits_intent(
                                        OrderIntentClass::RiskIncreasingEntry,
                                        Some(def_side),
                                    )
                                });
                            if (suppress_openings && !is_pure_reduce) || defense_blocks_keep {
                                arm.remove_resting_at(pos);
                                arm.cancels += 1;
                                cancelled += 1;
                                if defense_blocks_keep {
                                    defense_blocked_entries += 1;
                                }
                            } else {
                                arm.resting_meta[pos].reduce_only = is_pure_reduce;
                                arm.queue_preserved += 1;
                                kept += 1;
                            }
                        }
                    }
                    MmAction::PlaceAlo {
                        side,
                        price,
                        qty,
                        level_idx,
                        reduce_only,
                    } => {
                        if !reduce_only && suppress_openings {
                            continue;
                        }
                        let def_side = match side {
                            QuoteSide::Bid => DefenseSide::Buy,
                            QuoteSide::Ask => DefenseSide::Sell,
                        };
                        let defense_scale = if reduce_only {
                            1.0
                        } else {
                            defense_ref.map_or(1.0, |fsm| fsm.entry_size_scale(Some(def_side)))
                        };
                        if !reduce_only && defense_scale <= 0.0 {
                            defense_blocked_entries += 1;
                            continue;
                        }
                        if arm.scratch_resting.len() >= TRACK03_MAX_RESTING {
                            continue;
                        }
                        if arm
                            .scratch_resting
                            .iter()
                            .any(|r| r.side == side && r.level_idx == level_idx)
                        {
                            continue;
                        }
                        let combined_scale = risk_scale * defense_scale;
                        let final_qty = if !reduce_only && combined_scale < 1.0 {
                            quantize_down(qty * combined_scale, arm.cfg.strategy.lot_size)
                        } else {
                            qty
                        };
                        if final_qty <= 0.0
                            || (!reduce_only
                                && final_qty * price < arm.cfg.strategy.min_notional_usd)
                        {
                            continue;
                        }
                        let crosses = match side {
                            QuoteSide::Bid => price >= ba - 1e-12,
                            QuoteSide::Ask => price <= bb + 1e-12,
                        };
                        if delay_ms == 0 && crosses {
                            arm.alo_rejects += 1;
                            arm.engine.on_alo_reject(side, now_ms);
                            continue;
                        }
                        let (activated, queue_ahead) = if delay_ms == 0 {
                            let q_ahead = match side {
                                QuoteSide::Bid
                                    if (price - bb).abs() <= tol_25 || price <= bb + tol_25 =>
                                {
                                    arm.cfg.queue_ahead_fraction * bsz
                                }
                                QuoteSide::Ask
                                    if (price - ba).abs() <= tol_25 || price >= ba - tol_25 =>
                                {
                                    arm.cfg.queue_ahead_fraction * asz
                                }
                                _ => 0.0,
                            };
                            arm.engine.on_alo_accepted(side);
                            (true, q_ahead)
                        } else {
                            (false, 0.0)
                        };
                        let slot_idx = arm.scratch_resting.len();
                        arm.resting_seq = arm.resting_seq.saturating_add(1);
                        let client_id = format!(
                            "{}{}_{}",
                            match side {
                                QuoteSide::Bid => "b",
                                QuoteSide::Ask => "a",
                            },
                            level_idx,
                            arm.resting_seq
                        );
                        arm.scratch_resting.push(RestingQuote {
                            client_id,
                            side,
                            price,
                            qty: final_qty,
                            level_idx,
                            placed_at_ms: now_ms,
                        });
                        arm.resting_meta[slot_idx] = Track03OrderMeta {
                            queue_ahead,
                            active_at_ms: now_ms.saturating_add(delay_ms),
                            activated,
                            reduce_only,
                        };
                        arm.orders_placed += 1;
                        placed += 1;
                    }
                    MmAction::ReduceOnlyIoc {
                        side,
                        limit_price,
                        qty,
                        ..
                    } => {
                        if arm.queue_or_exec_ioc_exit(now_ms, side, limit_price, qty, target_touch)
                        {
                            taker_exited = true;
                        }
                    }
                    MmAction::HedgeIoc { .. } => {}
                    MmAction::Halt { .. } => {
                        arm.halted = true;
                    }
                }
            }
            if in_terminal_drain && arm.pos_qty.abs() > 1e-12 {
                let (side, limit_price) = if arm.pos_qty > 0.0 {
                    (QuoteSide::Ask, bb * 0.999)
                } else {
                    (QuoteSide::Bid, ba * 1.001)
                };
                if arm.queue_or_exec_ioc_exit(
                    now_ms,
                    side,
                    limit_price,
                    arm.pos_qty.abs(),
                    target_touch,
                ) {
                    taker_exited = true;
                }
            }
            (
                placed,
                cancelled,
                kept,
                taker_exited,
                sess_blocked,
                arm.scratch_resting.len(),
                arm.halted || session_halted,
                defense_blocked_entries,
            )
        };

        self.defense_blocked_entries += defense_blocked_entries;
        self.check_equity_and_drawdown(utc_day);
        Ok(Some(Track03StepOutcome {
            utc_session: session,
            session_gated: sess_blocked,
            halted: arm_halted,
            resting_orders,
            placed,
            cancelled,
            kept,
            taker_exited,
        }))
    }

    /// Process a public trade print using FIFO queue depletion and strict trade-through fill rules.
    #[allow(clippy::too_many_arguments)]
    pub fn on_trade_print(
        &mut self,
        symbol_id: u16,
        now_ms: u64,
        price: f64,
        size: f64,
        is_buy_aggressor: bool,
        recv_ms: u64,
        utc_day: i32,
    ) -> Result<bool> {
        if self.past_declared_end(now_ms) {
            return Ok(false);
        }
        let Some(&idx) = self.arm_by_symbol_id.get(&symbol_id) else {
            return Ok(false);
        };
        if !(price.is_finite() && price > 0.0 && size.is_finite() && size > 0.0) || recv_ms > now_ms
        {
            return Ok(false);
        }
        let session_halted = self.halted;
        let in_terminal_drain = self.in_terminal_drain(now_ms);
        let filled_any = {
            let arm = &mut self.arms[idx];
            if arm
                .target_feed
                .last_now_ms
                .is_some_and(|prev| now_ms < prev)
            {
                return Ok(false);
            }
            let book_fresh = arm.last_valid_target_touch.is_some_and(|t| {
                now_ms >= t.recv_ms && now_ms - t.recv_ms <= arm.cfg.strategy.max_book_age_ms
            });
            let _ = arm.process_pending_ioc_exits(now_ms, arm.last_valid_target_touch, book_fresh);
            if !book_fresh || now_ms - recv_ms > arm.cfg.strategy.max_book_age_ms {
                return Ok(false);
            }
            let Some(touch) = arm.last_valid_target_touch else {
                return Ok(false);
            };
            arm.activate_arrivals(now_ms, touch.bid, touch.bid_sz, touch.ask, touch.ask_sz);

            let resting_side = if is_buy_aggressor {
                QuoteSide::Ask
            } else {
                QuoteSide::Bid
            };
            let tol = arm.cfg.strategy.tick_size * 0.10;
            let cur_mid = touch.mid();
            let mut rem_print = size;
            let mut filled_any = false;
            let arm_halted = session_halted || arm.halted;

            // Process best-level resting orders on `resting_side` in level_idx order.
            for lvl in 0..arm.cfg.strategy.ladder_levels {
                if rem_print <= 1e-12 {
                    break;
                }
                let Some(pos) = arm
                    .scratch_resting
                    .iter()
                    .position(|r| r.side == resting_side && r.level_idx == lvl)
                else {
                    continue;
                };
                if !arm.resting_meta[pos].activated {
                    continue;
                }
                let ord_px = arm.scratch_resting[pos].price;
                let ord_qty = arm.scratch_resting[pos].qty;
                let ord_meta = arm.resting_meta[pos];
                let is_pure_reduce = arm.is_pure_reducing_order(resting_side, ord_qty, ord_meta);
                if arm_halted && !is_pure_reduce {
                    continue;
                }
                let trades_through = match resting_side {
                    QuoteSide::Ask => price > ord_px + tol,
                    QuoteSide::Bid => price < ord_px - tol,
                };
                let at_touch = (price - ord_px).abs() <= tol;
                let filled_qty = if trades_through {
                    ord_qty.min(rem_print)
                } else if at_touch {
                    if rem_print > arm.resting_meta[pos].queue_ahead {
                        let overflow = rem_print - arm.resting_meta[pos].queue_ahead;
                        arm.resting_meta[pos].queue_ahead = 0.0;
                        ord_qty.min(overflow)
                    } else {
                        arm.resting_meta[pos].queue_ahead -= rem_print;
                        rem_print = 0.0;
                        0.0
                    }
                } else {
                    0.0
                };
                if filled_qty > 1e-12 {
                    arm.record_fill(
                        now_ms,
                        resting_side,
                        ord_px,
                        filled_qty,
                        Track03FillRole::Maker,
                        cur_mid,
                        !is_pure_reduce,
                    );
                    filled_any = true;
                    rem_print -= filled_qty;
                    let left = arm.scratch_resting[pos].qty - filled_qty;
                    if left <= 1e-12 {
                        arm.remove_resting_at(pos);
                    } else {
                        arm.scratch_resting[pos].qty = left;
                    }
                }
            }

            let taker_side = if is_buy_aggressor {
                QuoteSide::Bid
            } else {
                QuoteSide::Ask
            };
            arm.engine.on_trade_print(
                FastTradePrint {
                    taker_side,
                    price,
                    size,
                    received_at_ms: recv_ms,
                },
                Some(&arm.scratch_book),
            );
            if in_terminal_drain && arm.pos_qty.abs() > 1e-12 {
                let (side, limit_price) = if arm.pos_qty > 0.0 {
                    (QuoteSide::Ask, touch.bid * 0.999)
                } else {
                    (QuoteSide::Bid, touch.ask * 1.001)
                };
                let _ =
                    arm.queue_or_exec_ioc_exit(now_ms, side, limit_price, arm.pos_qty.abs(), touch);
            }
            filled_any
        };

        self.check_equity_and_drawdown(utc_day);
        Ok(filled_any)
    }

    /// Diagnostic fill probe for SHM streams lacking public trade prints.
    ///
    /// Triggers a synthetic strict trade-through only when the incoming target BBO
    /// strictly crosses an active resting quote (`target.ask < bid` or `target.bid > ask`).
    pub fn on_bbo_cross_diagnostic(
        &mut self,
        symbol_id: u16,
        now_ms: u64,
        target: Track03Touch,
        utc_day: i32,
    ) -> Result<bool> {
        if self.past_declared_end(now_ms) {
            return Ok(false);
        }
        let Some(&idx) = self.arm_by_symbol_id.get(&symbol_id) else {
            return Ok(false);
        };
        if !target.valid()
            || target.recv_ms > now_ms
            || now_ms - target.recv_ms > self.arms[idx].cfg.strategy.max_book_age_ms
            || self.arms[idx]
                .target_feed
                .last_recv_ms
                .is_some_and(|prev| target.recv_ms < prev)
        {
            return Ok(false);
        }
        let Some(prev_touch) = self.arms[idx].last_valid_target_touch else {
            return Ok(false);
        };
        let (has_cross_bid, has_cross_ask) = {
            let arm = &self.arms[idx];
            let bid_px = arm
                .scratch_resting
                .iter()
                .enumerate()
                .filter(|&(i, r)| r.side == QuoteSide::Bid && arm.resting_meta[i].activated)
                .map(|(_, r)| r.price)
                .fold(f64::NEG_INFINITY, f64::max);
            let ask_px = arm
                .scratch_resting
                .iter()
                .enumerate()
                .filter(|&(i, r)| r.side == QuoteSide::Ask && arm.resting_meta[i].activated)
                .map(|(_, r)| r.price)
                .fold(f64::INFINITY, f64::min);
            (target.ask < bid_px, target.bid > ask_px)
        };
        let _ = prev_touch;
        let mut bid_hit = false;
        if has_cross_bid {
            bid_hit = self.on_trade_print(
                symbol_id,
                now_ms,
                target.ask,
                target.ask_sz,
                false,
                target.recv_ms,
                utc_day,
            )?;
        }
        let mut ask_hit = false;
        if has_cross_ask {
            ask_hit = self.on_trade_print(
                symbol_id,
                now_ms,
                target.bid,
                target.bid_sz,
                true,
                target.recv_ms,
                utc_day,
            )?;
        }
        Ok(bid_hit || ask_hit)
    }

    pub fn report(&self) -> Track03ShadowReport {
        let mut realized = 0.0;
        let mut unrealized = 0.0;
        let mut fees_usd = 0.0;
        let mut spread_capture_usd = 0.0;
        let mut holding_drift_usd = 0.0;
        let mut passive_exit_pnl_usd = 0.0;
        let mut taker_exit_slippage_usd = 0.0;
        let mut closed_notional_usd = 0.0;
        let mut maker_volume_usd = 0.0;
        let mut taker_volume_usd = 0.0;
        let mut mk1s_num = 0.0;
        let mut mk1s_den = 0.0;
        let mut weighted_net_cost_cycle_num = 0.0;
        let mut maker_fills = 0_u64;
        let mut taker_exits = 0_u64;
        let mut depth_clipped_exits = 0_u64;
        let mut stale_exit_blocks = 0_u64;
        let mut terminal_residual_notional_usd = 0.0_f64;
        let mut cycles = 0_u64;
        let mut orders_placed = 0_u64;
        let mut cancels = 0_u64;
        let mut queue_preserved = 0_u64;
        let mut alo_rejects = 0_u64;
        let mut session_gated_ticks = 0_u64;

        let arms: Vec<Track03ArmReport> = self
            .arms
            .iter()
            .map(|a| {
                let r_pnl = a.realized_pnl_usd();
                let u_pnl = a.unrealized_pnl_usd();
                let (
                    spread_capture_bps,
                    holding_drift_bps,
                    passive_exit_pnl_bps,
                    taker_exit_slippage_bps,
                    gross_ex_fee_bps,
                    taker_exit_share_s,
                    double_sided_usd_per_m,
                ) = a.cost_decomposition();
                let mark_mid = a.last_valid_target_touch.map_or(0.0, |t| t.mid());
                let pos_notional = a.pos_qty * mark_mid;
                let arm_residual_notional = if a.pos_qty.abs() > 1e-12 {
                    pos_notional.abs()
                } else {
                    0.0
                };
                let mk1s = if a.markout_1s_den_usd > 0.0 {
                    Some(a.markout_1s_num_bps_usd / a.markout_1s_den_usd)
                } else {
                    None
                };

                realized += r_pnl;
                unrealized += u_pnl;
                fees_usd += a.fees_usd;
                spread_capture_usd += a.spread_capture_usd;
                holding_drift_usd += a.holding_drift_usd;
                passive_exit_pnl_usd += a.passive_exit_pnl_usd;
                taker_exit_slippage_usd += a.taker_exit_slippage_usd;
                closed_notional_usd += a.closed_notional_usd;
                maker_volume_usd += a.maker_volume_usd;
                taker_volume_usd += a.taker_volume_usd;
                mk1s_num += a.markout_1s_num_bps_usd;
                mk1s_den += a.markout_1s_den_usd;
                if a.closed_notional_usd > 0.0 {
                    weighted_net_cost_cycle_num +=
                        (double_sided_usd_per_m / 50.0) * a.closed_notional_usd;
                }
                maker_fills += a.maker_fills;
                taker_exits += a.taker_exits;
                depth_clipped_exits += a.depth_clipped_exits;
                stale_exit_blocks += a.stale_exit_blocks;
                terminal_residual_notional_usd += arm_residual_notional;
                cycles += a.cycles;
                orders_placed += a.orders_placed;
                cancels += a.cancels;
                queue_preserved += a.queue_preserved;
                alo_rejects += a.alo_rejects;
                session_gated_ticks += a.session_gated_ticks;

                Track03ArmReport {
                    symbol: a.cfg.symbol.clone(),
                    symbol_id: a.cfg.symbol_id,
                    target_exchange_id: a.cfg.target_exchange_id,
                    ref_exchange_ids: a.cfg.ref_exchange_ids.clone(),
                    account_fee_status: a.cfg.account_fee_status.clone(),
                    realized_pnl_usd: r_pnl,
                    unrealized_pnl_usd: u_pnl,
                    fees_usd: a.fees_usd,
                    spread_capture_usd: a.spread_capture_usd,
                    holding_drift_usd: a.holding_drift_usd,
                    passive_exit_pnl_usd: a.passive_exit_pnl_usd,
                    taker_exit_slippage_usd: a.taker_exit_slippage_usd,
                    spread_capture_bps,
                    holding_drift_bps,
                    passive_exit_pnl_bps,
                    taker_exit_slippage_bps,
                    gross_ex_fee_bps,
                    double_sided_usd_per_m,
                    maker_volume_usd: a.maker_volume_usd,
                    taker_volume_usd: a.taker_volume_usd,
                    closed_notional_usd: a.closed_notional_usd,
                    taker_exit_share_s,
                    markout_1s_bps: mk1s,
                    markout_1s_den_usd: a.markout_1s_den_usd,
                    position_qty: a.pos_qty,
                    position_notional_usd: pos_notional,
                    terminal_residual_notional_usd: arm_residual_notional,
                    fills: a.maker_fills + a.taker_exits,
                    maker_fills: a.maker_fills,
                    taker_exits: a.taker_exits,
                    depth_clipped_exits: a.depth_clipped_exits,
                    stale_exit_blocks: a.stale_exit_blocks,
                    cycles: a.cycles,
                    orders_placed: a.orders_placed,
                    cancels: a.cancels,
                    queue_preserved: a.queue_preserved,
                    alo_rejects: a.alo_rejects,
                    session_gated_ticks: a.session_gated_ticks,
                    total_book_ticks: a.total_book_ticks,
                    halted: self.halted || a.halted,
                    target_feed: a.target_feed.report(),
                    ref_feeds: a.ref_slots.iter().map(|s| s.feed.report()).collect(),
                }
            })
            .collect();

        let (
            venue_feed_health,
            max_gap_ms,
            stale_book_drops,
            clock_regression_drops,
            crossed_book_drops,
        ) = aggregate_venue_health(
            arms.iter()
                .flat_map(|a| std::iter::once(a.target_feed).chain(a.ref_feeds.iter().copied())),
        );

        let (
            spread_capture_bps,
            holding_drift_bps,
            passive_exit_pnl_bps,
            taker_exit_slippage_bps,
            gross_ex_fee_bps,
            taker_exit_share_s,
            double_sided_usd_per_m,
        ) = if closed_notional_usd > 0.0 {
            let gross_usd = spread_capture_usd
                + holding_drift_usd
                + passive_exit_pnl_usd
                + taker_exit_slippage_usd;
            let net_cost_cycle_bp = weighted_net_cost_cycle_num / closed_notional_usd;
            (
                (spread_capture_usd / closed_notional_usd) * 1e4,
                (holding_drift_usd / closed_notional_usd) * 1e4,
                (passive_exit_pnl_usd / closed_notional_usd) * 1e4,
                (taker_exit_slippage_usd / closed_notional_usd) * 1e4,
                (gross_usd / closed_notional_usd) * 1e4,
                (taker_volume_usd / closed_notional_usd).clamp(0.0, 1.0),
                (net_cost_cycle_bp / 2.0) * 100.0,
            )
        } else {
            let fallback_ds = if arms.len() == 1 {
                arms[0].double_sided_usd_per_m
            } else {
                0.0
            };
            (0.0, 0.0, 0.0, 0.0, 0.0, 0.0, fallback_ds)
        };

        Track03ShadowReport {
            track: "03_incentive_turnover",
            fill_evidence_mode: self.fill_evidence_mode,
            fill_evidence_note: self.fill_evidence_note(),
            capital_usd: self.cfg.capital_usd,
            equity_usd: self.cfg.capital_usd + realized + unrealized,
            realized_pnl_usd: realized,
            unrealized_pnl_usd: unrealized,
            fees_usd,
            spread_capture_bps,
            holding_drift_bps,
            passive_exit_pnl_bps,
            taker_exit_slippage_bps,
            gross_ex_fee_bps,
            double_sided_usd_per_m,
            maker_volume_usd,
            taker_volume_usd,
            closed_notional_usd,
            taker_exit_share_s,
            markout_1s_bps: (mk1s_den > 0.0).then_some(mk1s_num / mk1s_den),
            max_drawdown_usd: self.max_drawdown_usd,
            fills: maker_fills + taker_exits,
            maker_fills,
            taker_exits,
            depth_clipped_exits,
            stale_exit_blocks,
            terminal_residual_notional_usd,
            cycles,
            orders_placed,
            cancels,
            queue_preserved,
            alo_rejects,
            session_gated_ticks,
            risk_scale: if self.halted {
                0.0
            } else {
                self.daily_loss.risk_scale()
            },
            halted: self.halted,
            defense_state_transitions: self.defense.as_ref().map_or(0, DefenseFsm::transitions),
            defense_blocked_entries: self.defense_blocked_entries,
            defense_state: self.defense.as_ref().map(DefenseFsm::state),
            max_gap_ms,
            stale_book_drops,
            clock_regression_drops,
            crossed_book_drops,
            venue_feed_health,
            arms,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strategy::basis_maker_hedge::RefCancelShieldConfig;
    use crate::strategy::defense_fsm::BlockedSide;

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
                confirm_ref_exchange_id: None,
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
            defense: None,
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
            defense: None,
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

    #[test]
    fn track02_bbo_cross_diagnostic_drops_stale_and_clock_regressed_maker() {
        let (mut s, t, q) = warm_track02_session(t2_cfg());
        s.set_fill_evidence_mode("shm_bbo_cross_diagnostic_no_trades");
        let ask = q.ask.expect("ask should quote");
        let bid = q.bid.expect("bid should quote");

        // Advance clock past max_book_age_ms (5000ms) while maker book stays frozen at `t`.
        let now_stale = t + 5_001;
        let stale_maker = mk_touch(ask + 0.02, ask + 0.04, t);
        let fresh_hedge = mk_touch(ask + 0.01, ask + 0.03, now_stale);
        assert!(
            !s.on_maker_bbo_cross_diagnostic(
                1002,
                now_stale,
                stale_maker,
                10.0,
                10.0,
                fresh_hedge,
                utc_day_from_ms(now_stale),
            )
            .unwrap()
        );
        assert_eq!(s.report().fills, 0);

        // Future maker recv_ms (> now_ms) must also be dropped.
        let future_maker = mk_touch(bid - 0.04, bid - 0.02, t + 50);
        assert!(
            !s.on_maker_bbo_cross_diagnostic(
                1002,
                t,
                future_maker,
                10.0,
                10.0,
                mk_touch(99.99, 100.01, t),
                utc_day_from_ms(t),
            )
            .unwrap()
        );
        assert_eq!(s.report().fills, 0);

        // Clock-regressed maker recv_ms (< previous maker recv_ms `t`) must also be dropped.
        let regressed_maker = mk_touch(ask + 0.02, ask + 0.04, t - 10);
        assert!(
            !s.on_maker_bbo_cross_diagnostic(
                1002,
                t + 20,
                regressed_maker,
                10.0,
                10.0,
                mk_touch(ask + 0.01, ask + 0.03, t + 20),
                utc_day_from_ms(t + 20),
            )
            .unwrap()
        );
        assert_eq!(s.report().fills, 0);

        // Crossed maker book must also be dropped.
        let crossed_maker = mk_touch(ask + 0.05, ask + 0.02, t + 30);
        assert!(
            !s.on_maker_bbo_cross_diagnostic(
                1002,
                t + 30,
                crossed_maker,
                10.0,
                10.0,
                mk_touch(ask + 0.01, ask + 0.03, t + 30),
                utc_day_from_ms(t + 30),
            )
            .unwrap()
        );
        assert_eq!(s.report().fills, 0);
    }

    #[test]
    fn track01_exit_confirms_flat_via_notify_exit_filled_and_handles_skip_distinctly() {
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
        // Zero target ask size triggers skip -> clear_position() keeps engine flat.
        let skipped = s
            .on_bbo_pair(
                1002,
                t,
                ll_touch(100.09, 100.11, t),
                ll_touch(99.99, 100.01, t),
                10.0,
                0.0,
                utc_day_from_ms(t),
            )
            .unwrap();
        assert_eq!(skipped, Some(LeadLagAction::Idle));
        assert_eq!(s.report().skips, 1);
        assert_eq!(s.report().trades, 0);
        assert!(s.arms[0].engine.position().is_none());

        // Re-warm and execute a full entry -> exit cycle; exit calls notify_exit_filled().
        t += 50_000;
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
        assert!(matches!(
            s.on_bbo_pair(
                1002,
                t,
                ll_touch(100.09, 100.11, t),
                ll_touch(99.99, 100.01, t),
                10.0,
                10.0,
                utc_day_from_ms(t),
            )
            .unwrap(),
            Some(LeadLagAction::Enter {
                side: LeadLagSide::Long,
                ..
            })
        ));
        assert_eq!(s.arms[0].engine.position(), Some(LeadLagSide::Long));

        let exit_t = t + 1_200;
        assert_eq!(
            s.on_bbo_pair(
                1002,
                exit_t,
                ll_touch(100.09, 100.11, exit_t),
                ll_touch(100.09, 100.11, exit_t),
                10.0,
                10.0,
                utc_day_from_ms(exit_t),
            )
            .unwrap(),
            Some(LeadLagAction::Exit {
                side: LeadLagSide::Long
            })
        );
        assert!(s.arms[0].engine.position().is_none());
        assert_eq!(s.report().trades, 1);
    }

    #[test]
    fn track01_feed_health_tracks_stale_clock_regression_crossed_and_gap() {
        let mut s = Track01ShadowSession::new(t1_cfg(), utc_day_from_ms(1_000)).unwrap();
        let day = utc_day_from_ms(1_000);

        // Sample 1 & 2 establish baseline and 250ms gap.
        let _ = s
            .on_bbo_pair(
                1002,
                1_000,
                ll_touch(99.99, 100.01, 1_000),
                ll_touch(99.99, 100.01, 1_000),
                10.0,
                10.0,
                day,
            )
            .unwrap();
        let _ = s
            .on_bbo_pair(
                1002,
                1_250,
                ll_touch(99.99, 100.01, 1_250),
                ll_touch(99.99, 100.01, 1_250),
                10.0,
                10.0,
                day,
            )
            .unwrap();

        // Crossed ref book + valid tgt book.
        let _ = s
            .on_bbo_pair(
                1002,
                1_350,
                ll_touch(100.02, 100.01, 1_350),
                ll_touch(99.99, 100.01, 1_350),
                10.0,
                10.0,
                day,
            )
            .unwrap();

        // Clock regression on tgt (recv_ms < prev_recv_ms = 1_350) + future recv_ms on ref.
        let _ = s
            .on_bbo_pair(
                1002,
                1_400,
                ll_touch(99.99, 100.01, 1_500),
                ll_touch(99.99, 100.01, 1_300),
                10.0,
                10.0,
                day,
            )
            .unwrap();

        // Stale ref book (> default max_ref_age_ms=5000) while tgt updates cleanly.
        let _ = s
            .on_bbo_pair(
                1002,
                7_000,
                ll_touch(99.99, 100.01, 1_250),
                ll_touch(99.99, 100.01, 7_000),
                10.0,
                10.0,
                day,
            )
            .unwrap();

        let rep = s.report();
        assert_eq!(rep.crossed_book_drops, 1);
        assert_eq!(rep.clock_regression_drops, 2);
        assert_eq!(rep.stale_book_drops, 1);
        assert_eq!(rep.max_gap_ms, 5_750);
        assert_eq!(rep.arms[0].ref_feed.crossed_book_drops, 1);
        assert_eq!(rep.arms[0].ref_feed.clock_regression_drops, 1);
        assert_eq!(rep.arms[0].ref_feed.stale_book_drops, 1);
        assert_eq!(rep.arms[0].ref_feed.max_gap_ms, 5_750);
        assert_eq!(rep.arms[0].tgt_feed.clock_regression_drops, 1);
        assert_eq!(rep.arms[0].tgt_feed.max_gap_ms, 5_650);
        assert_eq!(rep.venue_feed_health.len(), 2);

        let json = serde_json::to_value(&rep).unwrap();
        assert_eq!(json["max_gap_ms"], 5_750);
        assert_eq!(json["stale_book_drops"], 1);
        assert_eq!(json["clock_regression_drops"], 2);
        assert_eq!(json["crossed_book_drops"], 1);
    }

    #[test]
    fn track02_feed_health_tracks_stale_clock_regression_crossed_and_gap() {
        let mut s = Track02ShadowSession::new(t2_cfg(), utc_day_from_ms(10_000)).unwrap();
        let day = utc_day_from_ms(10_000);

        s.on_reference_mid(1002, 10_000, 100.0);
        s.on_reference_mid(1002, 10_400, 100.0);
        // Regressed reference timestamp.
        s.on_reference_mid(1002, 10_300, 100.0);

        let _ = s
            .on_pair_books(
                1002,
                10_000,
                mk_touch(99.99, 100.01, 10_000),
                mk_touch(99.99, 100.01, 10_000),
                day,
            )
            .unwrap();
        // Crossed maker book.
        let _ = s
            .on_pair_books(
                1002,
                10_200,
                mk_touch(100.05, 100.01, 10_200),
                mk_touch(99.99, 100.01, 10_200),
                day,
            )
            .unwrap();
        // Regressed hedge recv_ms.
        let _ = s
            .on_pair_books(
                1002,
                10_300,
                mk_touch(99.99, 100.01, 10_300),
                mk_touch(99.99, 100.01, 10_150),
                day,
            )
            .unwrap();
        // Stale maker book (> max_book_age_ms = 5000ms) while hedge is fresh.
        let _ = s
            .on_pair_books(
                1002,
                17_000,
                mk_touch(99.99, 100.01, 10_300),
                mk_touch(99.99, 100.01, 17_000),
                day,
            )
            .unwrap();

        let rep = s.report();
        assert_eq!(rep.crossed_book_drops, 1);
        assert_eq!(rep.clock_regression_drops, 2);
        assert_eq!(rep.stale_book_drops, 1);
        assert_eq!(rep.max_gap_ms, 6_800);
        assert_eq!(rep.pairs[0].maker_feed.crossed_book_drops, 1);
        assert_eq!(rep.pairs[0].maker_feed.stale_book_drops, 1);
        assert_eq!(rep.pairs[0].hedge_feed.clock_regression_drops, 1);
        assert_eq!(rep.pairs[0].hedge_feed.max_gap_ms, 6_800);
        assert_eq!(rep.pairs[0].ref_feed.clock_regression_drops, 1);
        assert_eq!(rep.pairs[0].ref_feed.max_gap_ms, 400);
        assert_eq!(rep.venue_feed_health.len(), 3);
    }

    #[test]
    fn track02_on_reference_touch_rejects_stale_and_deduplicates_frozen_shm_slot() {
        let mut s = Track02ShadowSession::new(t2_cfg(), utc_day_from_ms(10_000)).unwrap();
        let day = utc_day_from_ms(10_000);
        let mut t = 10_000_u64;
        for _ in 0..14 {
            let _ = s
                .on_pair_books(
                    1002,
                    t,
                    mk_touch(99.99, 100.01, t),
                    mk_touch(99.99, 100.01, t),
                    day,
                )
                .unwrap();
            t += 100;
        }
        assert!(s.on_reference_touch(1002, t, mk_touch(99.99, 100.01, t)));
        t += 100;
        // Sharp -5 bps move triggers pre-fill cancel shield on the bid leg.
        let frozen_crash_touch = mk_touch(99.94, 99.96, t);
        assert!(s.on_reference_touch(1002, t, frozen_crash_touch));

        // 80 faster maker/hedge wakeups repeating the identical SHM ref slot must deduplicate
        // without evicting the earlier reference sample from REF_RING_CAP (= 64).
        for step in 1..=80_u64 {
            assert!(!s.on_reference_touch(1002, t + step, frozen_crash_touch));
        }
        let q_shielded = s
            .on_pair_books(
                1002,
                t + 80,
                mk_touch(99.99, 100.01, t + 80),
                mk_touch(99.99, 100.01, t + 80),
                day,
            )
            .unwrap()
            .expect("pair should emit protected ask quote");
        assert!(q_shielded.bid.is_none());
        assert!(q_shielded.ask.is_some());

        // Once now_ms - ref_recv_ms exceeds max_book_age_ms (5_000), frozen SHM ref must be
        // rejected as stale rather than stamped with now_ms -> fail-closed withdraws both legs.
        let stale_now = t + 5_001;
        assert!(!s.on_reference_touch(1002, stale_now, frozen_crash_touch));
        let q_expired = s
            .on_pair_books(
                1002,
                stale_now,
                mk_touch(99.99, 100.01, stale_now),
                mk_touch(99.99, 100.01, stale_now),
                day,
            )
            .unwrap()
            .expect("pair should emit empty quotes when ref shield expires fail-closed");
        assert!(q_expired.bid.is_none());
        assert!(q_expired.ask.is_none());
        let rep = s.report();
        assert_eq!(rep.pairs[0].ref_feed.stale_book_drops, 1);
    }

    #[test]
    fn track01_stale_or_regressed_target_never_overwrites_mtm_or_fills_exit() {
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
        assert!(matches!(
            s.on_bbo_pair(
                1002,
                t,
                ll_touch(100.09, 100.11, t),
                ll_touch(99.99, 100.01, t),
                10.0,
                10.0,
                utc_day_from_ms(t),
            )
            .unwrap(),
            Some(LeadLagAction::Enter {
                side: LeadLagSide::Long,
                ..
            })
        ));
        let baseline_unrealized = s.report().unrealized_pnl_usd;

        // Stale target quote beyond exit_due_ms & max_tgt_age_ms must not overwrite MTM or fill exit.
        let stale_now = t + 6_000;
        assert!(matches!(
            s.on_bbo_pair(
                1002,
                stale_now,
                ll_touch(100.09, 100.11, stale_now),
                ll_touch(199.99, 200.01, t),
                10.0,
                10.0,
                utc_day_from_ms(stale_now),
            )
            .unwrap(),
            Some(LeadLagAction::Blocked(_))
        ));
        assert_eq!(s.report().trades, 0);
        assert_eq!(s.report().arms[0].tgt_feed.stale_book_drops, 1);
        assert!((s.report().unrealized_pnl_usd - baseline_unrealized).abs() < 1e-12);

        // Clock-regressed target quote must also fail closed without mutating MTM or position.
        let regr_now = stale_now + 100;
        assert!(matches!(
            s.on_bbo_pair(
                1002,
                regr_now,
                ll_touch(100.09, 100.11, regr_now),
                ll_touch(299.99, 300.01, t - 10),
                10.0,
                10.0,
                utc_day_from_ms(regr_now),
            )
            .unwrap(),
            Some(LeadLagAction::Blocked(_))
        ));
        assert_eq!(s.report().trades, 0);
        assert_eq!(s.report().arms[0].tgt_feed.clock_regression_drops, 1);
        assert!((s.report().unrealized_pnl_usd - baseline_unrealized).abs() < 1e-12);

        // Fresh advancing target touch completes the exit cleanly.
        let exit_now = regr_now + 100;
        assert_eq!(
            s.on_bbo_pair(
                1002,
                exit_now,
                ll_touch(100.09, 100.11, exit_now),
                ll_touch(100.09, 100.11, exit_now),
                10.0,
                10.0,
                utc_day_from_ms(exit_now),
            )
            .unwrap(),
            Some(LeadLagAction::Exit {
                side: LeadLagSide::Long
            })
        );
        assert_eq!(s.report().trades, 1);
        assert!(s.report().arms[0].open_side.is_none());
    }

    fn t3_cfg() -> Track03ShadowConfig {
        Track03ShadowConfig {
            capital_usd: 10_000.0,
            daily_loss_limit_pct: 0.02,
            daily_loss_state_path: None,
            allowlisted_symbols: vec!["AAPL".to_string()],
            arms: vec![Track03ArmShadowConfig {
                symbol: "AAPL".to_string(),
                symbol_id: 1006,
                target_exchange_id: 2,
                ref_exchange_ids: vec![7],
                maker_fee_bps: 0.0,
                taker_fee_bps: 0.0,
                rebate_bps: 0.0,
                queue_ahead_fraction: 0.5,
                allowed_utc_sessions: Some(vec![
                    UtcSession::OvernightAsia,
                    UtcSession::UsExtended,
                    UtcSession::Weekend,
                ]),
                account_fee_status: "public_tier0_unverified".to_string(),
                ioc_exit_delay_ms: 0,
                max_l1_depth_fraction: 0.50,
                strategy: QueuePriorityMmConfig {
                    tick_size: 0.01,
                    lot_size: 0.01,
                    min_notional_usd: 10.0,
                    maker_fee_bps: 0.0,
                    quote_notional_usd: 200.0,
                    max_position_notional_usd: 400.0,
                    ladder_levels: 1,
                    min_penny_spread_ticks: 99,
                    min_entry_edge_bps: 0.10,
                    lead_cancel_threshold_bps: 1.50,
                    max_queue_keep_ticks: 3,
                    sweep_print_pull_ratio: 0.75,
                    sweep_cooldown_ms: 1_200,
                    submit_rtt_ms: 0.0,
                    as_inventory_skew_gamma: 2.0,
                    vol_spread_mult: 0.5,
                    directional_markout_skew_mult: 1.0,
                    quiet_regime_min_spread_to_vol: 0.80,
                    quiet_regime_max_entry_vol_bps: Some(1.2),
                    max_book_age_ms: 5_000,
                    max_session_loss_usd: 50.0,
                    max_inventory_hold_ms: 15_000,
                    require_fresh_reference: false,
                    ..QueuePriorityMmConfig::default()
                },
            }],
            defense: None,
            declared_end_ms: None,
            terminal_drain_ms: None,
        }
    }

    fn t3_touch(bid: f64, ask: f64, sz: f64, recv_ms: u64) -> Track03Touch {
        Track03Touch {
            bid,
            ask,
            bid_sz: sz,
            ask_sz: sz,
            recv_ms,
        }
    }

    #[test]
    fn track01_confirm_leader_wiring_enforces_consensus_and_records_health() {
        let mut cfg = t1_cfg();
        cfg.arms[0].confirm_ref_exchange_id = Some(8);
        cfg.arms[0].strategy.require_dual_leader_confirm = true;
        let mut s = Track01ShadowSession::new(cfg, utc_day_from_ms(10_000)).unwrap();
        assert_eq!(s.arm_confirm_exchange_id(1002), Some(8));

        let mut t = 10_000_u64;
        for _ in 0..15 {
            let _ = s
                .on_bbo_pair_with_confirm(
                    1002,
                    t,
                    ll_touch(99.99, 100.01, t),
                    Some(ll_touch(99.99, 100.01, t)),
                    ll_touch(99.99, 100.01, t),
                    10.0,
                    10.0,
                    utc_day_from_ms(t),
                )
                .unwrap();
            t += 100;
        }

        // Disagreeing confirm leader suppresses entry -> Idle.
        let disagree = s
            .on_bbo_pair_with_confirm(
                1002,
                t,
                ll_touch(100.09, 100.11, t),
                Some(ll_touch(99.95, 99.97, t)),
                ll_touch(99.99, 100.01, t),
                10.0,
                10.0,
                utc_day_from_ms(t),
            )
            .unwrap();
        assert_eq!(disagree, Some(LeadLagAction::Idle));
        assert_eq!(s.report().trades, 0);

        // Stale confirm leader (advancing now_ms past max_ref_age_ms while confirm recv_ms stays frozen)
        // suppresses entry and records stale_book_drops = 1 on confirm_ref_feed.
        let stale_now = t + 6_000;
        let stale_confirm = s
            .on_bbo_pair_with_confirm(
                1002,
                stale_now,
                ll_touch(100.09, 100.11, stale_now),
                Some(ll_touch(100.09, 100.11, t)),
                ll_touch(99.99, 100.01, stale_now),
                10.0,
                10.0,
                utc_day_from_ms(stale_now),
            )
            .unwrap();
        assert_eq!(stale_confirm, Some(LeadLagAction::Idle));

        // Re-warm primary & target gently at 100.0, then fresh agreeing confirm leader enters Long.
        t = stale_now + 100;
        for _ in 0..15 {
            let _ = s
                .on_bbo_pair_with_confirm(
                    1002,
                    t,
                    ll_touch(99.99, 100.01, t),
                    Some(ll_touch(99.99, 100.01, t)),
                    ll_touch(99.99, 100.01, t),
                    10.0,
                    10.0,
                    utc_day_from_ms(t),
                )
                .unwrap();
            t += 100;
        }
        let agree = s
            .on_bbo_pair_with_confirm(
                1002,
                t,
                ll_touch(100.09, 100.11, t),
                Some(ll_touch(100.03, 100.05, t)),
                ll_touch(99.99, 100.01, t),
                10.0,
                10.0,
                utc_day_from_ms(t),
            )
            .unwrap();
        assert!(matches!(
            agree,
            Some(LeadLagAction::Enter {
                side: LeadLagSide::Long,
                ..
            })
        ));
        let rep = s.report();
        assert_eq!(rep.arms[0].confirm_ref_exchange_id, Some(8));
        let cfeed = rep.arms[0]
            .confirm_ref_feed
            .expect("confirm_ref_feed should be reported");
        assert_eq!(cfeed.exchange_id, 8);
        assert_eq!(cfeed.stale_book_drops, 1);
        assert_eq!(rep.venue_feed_health.len(), 3);
    }

    #[test]
    fn track02_l1_wiring_gates_thin_hedge_and_preserves_reduce_only() {
        let mut cfg = t2_cfg();
        cfg.pairs[0].strategy.e0_bps = 2.0;
        cfg.pairs[0].strategy.min_hedge_l1_notional_usd = 1_000.0;
        let (mut s, mut t, _) = warm_track02_session(cfg);

        // Legacy on_pair_books without L1 sizes fails closed on opening sides when min_hedge_l1_notional_usd > 0.
        t += 50;
        s.on_reference_mid(1002, t, 100.0);
        let q_no_l1 = s
            .on_pair_books(
                1002,
                t,
                mk_touch(99.99, 100.01, t),
                mk_touch(99.99, 100.01, t),
                utc_day_from_ms(t),
            )
            .unwrap()
            .unwrap();
        assert_eq!(q_no_l1.bid, None);
        assert_eq!(q_no_l1.ask, None);

        // With hedge L1 sizes: bid thin ($500 < $1000), ask deep ($2000 >= $1000).
        // Opening bid blocked; opening ask allowed.
        t += 50;
        s.on_reference_mid(1002, t, 100.0);
        let q_l1 = s
            .on_pair_books_with_l1(
                1002,
                t,
                mk_touch(99.99, 100.01, t),
                mk_touch(99.99, 100.01, t),
                Some((5.0, 20.0)),
                utc_day_from_ms(t),
            )
            .unwrap()
            .unwrap();
        assert_eq!(q_l1.bid, None);
        let ask_px = q_l1.ask.expect("deep hedge ask should allow maker ask");

        // Fill maker ask to build short inventory (-1 clip).
        assert!(
            s.on_maker_trade_print(
                1002,
                t,
                ask_px + 0.01,
                10.0,
                true,
                mk_touch(99.99, 100.01, t),
                utc_day_from_ms(t),
            )
            .unwrap()
        );
        assert!(s.report().pairs[0].inventory_clips < 0.0);

        // Now short: bid is inventory-reducing so it MUST stay active despite thin hedge bid!
        t += 50;
        s.on_reference_mid(1002, t, 100.0);
        let q_reduce = s
            .on_pair_books_with_l1(
                1002,
                t,
                mk_touch(99.99, 100.01, t),
                mk_touch(99.99, 100.01, t),
                Some((5.0, 20.0)),
                utc_day_from_ms(t),
            )
            .unwrap()
            .unwrap();
        assert!(
            q_reduce.bid.is_some(),
            "reducing bid must stay active even with thin hedge bid L1"
        );
    }

    #[test]
    fn track03_utc_session_gating_cancels_openings_in_rth_and_keeps_reducing_exits() {
        // 2026-11-25 is a Wednesday:
        //   00:00 UTC = 1_795_564_800_000 ms (overnight_asia)
        //   11:00 UTC = +39_600_000 ms (us_extended)
        //   13:30 UTC = +48_600_000 ms (us_rth)
        // 2026-11-28 is a Saturday: +3 days = 1_795_824_000_000 ms (weekend)
        let wed_00_00 = 1_795_564_800_000_u64;
        assert_eq!(classify_utc_session(wed_00_00), UtcSession::OvernightAsia);
        assert_eq!(
            classify_utc_session(wed_00_00 + 11 * 3_600_000),
            UtcSession::UsExtended
        );
        assert_eq!(
            classify_utc_session(wed_00_00 + 13 * 3_600_000 + 30 * 60_000),
            UtcSession::UsRth
        );
        assert_eq!(
            classify_utc_session(wed_00_00 + 3 * 86_400_000 + 15 * 3_600_000),
            UtcSession::Weekend
        );

        let mut s = Track03ShadowSession::new(t3_cfg(), utc_day_from_ms(wed_00_00)).unwrap();
        // US Extended (13:29:55 UTC, 5s before US RTH open): places both Bid & Ask.
        let t_ext = wed_00_00 + 13 * 3_600_000 + 29 * 60_000 + 55_000;
        let out_ext = s
            .on_target_bbo(
                1006,
                t_ext,
                t3_touch(99.98, 100.02, 10.0, t_ext),
                utc_day_from_ms(t_ext),
            )
            .unwrap()
            .unwrap();
        assert_eq!(out_ext.utc_session, UtcSession::UsExtended);
        assert!(!out_ext.session_gated);
        assert_eq!(out_ext.resting_orders, 2);

        // Fill our resting Bid via strict trade-through to become LONG 2.0 shares ($200).
        assert!(
            s.on_trade_print(
                1006,
                t_ext + 50,
                99.97,
                5.0,
                false,
                t_ext + 50,
                utc_day_from_ms(t_ext),
            )
            .unwrap()
        );
        assert!(s.report().arms[0].position_qty > 1.5);

        // Transition into US RTH 6s later (13:30:01 UTC, within max_inventory_hold_ms=15_000):
        // Opening Bid MUST be suppressed/cancelled, while reducing Ask MUST stay resting!
        let t_rth = t_ext + 6_000;
        let out_rth = s
            .on_target_bbo(
                1006,
                t_rth,
                t3_touch(99.98, 100.02, 10.0, t_rth),
                utc_day_from_ms(t_rth),
            )
            .unwrap()
            .unwrap();
        assert_eq!(out_rth.utc_session, UtcSession::UsRth);
        assert!(out_rth.session_gated);
        assert_eq!(out_rth.resting_orders, 1);
        assert_eq!(s.arms[0].scratch_resting[0].side, QuoteSide::Ask);
        assert!(s.arms[0].resting_meta[0].reduce_only);
        assert_eq!(s.report().session_gated_ticks, 1);

        // Weekend session re-enables quoting cleanly.
        let t_wknd = wed_00_00 + 3 * 86_400_000 + 15 * 3_600_000;
        let out_wknd = s
            .on_target_bbo(
                1006,
                t_wknd,
                t3_touch(99.98, 100.02, 10.0, t_wknd),
                utc_day_from_ms(t_wknd),
            )
            .unwrap()
            .unwrap();
        assert_eq!(out_wknd.utc_session, UtcSession::Weekend);
        assert!(!out_wknd.session_gated);
    }

    #[test]
    fn track03_volatile_regime_cancels_resting_openings_and_preserves_reducing_exits() {
        let mut s =
            Track03ShadowSession::new(t3_cfg(), utc_day_from_ms(1_795_564_800_000)).unwrap();
        let mut t = 1_795_564_800_000_u64; // Wednesday 00:00 UTC (overnight_asia)
        let day = utc_day_from_ms(t);

        // Seed calm book (L1 size 20.0 -> queue_ahead=10.0) -> places Bid & Ask.
        let out1 = s
            .on_target_bbo(1006, t, t3_touch(99.98, 100.02, 20.0, t), day)
            .unwrap()
            .unwrap();
        assert_eq!(out1.resting_orders, 2);

        // Fill Bid at touch via queue overflow (12.0 < 0.75 * 20.0 so sweep cooldown stays off).
        t += 100;
        assert!(
            s.on_trade_print(1006, t, 99.98, 12.0, false, t, day)
                .unwrap()
        );
        // Re-evaluate at calm price -> now Bid (opening) and Ask (reducing) both rest.
        t += 100;
        let out2 = s
            .on_target_bbo(1006, t, t3_touch(99.98, 100.02, 20.0, t), day)
            .unwrap()
            .unwrap();
        assert_eq!(out2.resting_orders, 2);

        // Spike volatility (+20 bps in 500ms -> sigma_1s > 1.2 bps/sqrt(s)):
        // Resting opening Bid MUST cancel ("quiet_regime_gate"), while reducing Ask MUST stay!
        t += 500;
        let out_storm = s
            .on_target_bbo(1006, t, t3_touch(100.18, 100.22, 20.0, t), day)
            .unwrap()
            .unwrap();
        assert_eq!(out_storm.resting_orders, 1);
        assert_eq!(s.arms[0].scratch_resting[0].side, QuoteSide::Ask);
        assert!(s.report().cancels >= 1);
    }

    #[test]
    fn track03_daily_loss_guard_halts_openings_while_preserving_reducing_exit_and_isolation() {
        let mut cfg3 = t3_cfg();
        cfg3.capital_usd = 1_000.0;
        cfg3.daily_loss_limit_pct = 0.02; // $20 budget
        cfg3.arms[0].strategy.quote_notional_usd = 400.0;
        cfg3.arms[0].strategy.max_position_notional_usd = 400.0;
        cfg3.arms[0].strategy.max_session_loss_usd = 100.0; // isolate DailyLossGuard above engine loss breaker
        cfg3.arms[0].strategy.quiet_regime_max_entry_vol_bps = None;
        cfg3.arms[0].strategy.quiet_regime_min_spread_to_vol = 0.0;

        let t0 = 1_795_564_800_000_u64;
        let day = utc_day_from_ms(t0);
        let mut s3 = Track03ShadowSession::new(cfg3, day).unwrap();
        let mut s1 = Track01ShadowSession::new(t1_cfg(), day).unwrap();

        let out0 = s3
            .on_target_bbo(1006, t0, t3_touch(99.98, 100.02, 20.0, t0), day)
            .unwrap()
            .unwrap();
        assert_eq!(out0.resting_orders, 2);

        // Fill Bid for $400 (~4 shares at 99.98).
        assert!(
            s3.on_trade_print(1006, t0 + 50, 99.97, 20.0, false, t0 + 50, day)
                .unwrap()
        );

        // Price plunges 7% -> MTM loss ~$28 > $20 DailyLossGuard budget -> Track 03 halts!
        let t1 = t0 + 1_000;
        let out_drop = s3
            .on_target_bbo(1006, t1, t3_touch(92.98, 93.02, 20.0, t1), day)
            .unwrap()
            .unwrap();
        assert!(s3.is_halted());
        assert!(out_drop.halted);
        // Reducing Ask MUST still be resting while opening Bid is suppressed!
        assert_eq!(out_drop.resting_orders, 1);
        assert_eq!(s3.arms[0].scratch_resting[0].side, QuoteSide::Ask);
        assert!(s3.arms[0].resting_meta[0].reduce_only);

        // Track 01 stays completely unhalted (non-cross-subsidy isolation).
        assert!(!s1.is_halted());
        assert!((s1.report().risk_scale - 1.0).abs() < 1e-12);
        let _ = s1
            .on_bbo_pair(
                1002,
                t1,
                ll_touch(99.99, 100.01, t1),
                ll_touch(99.99, 100.01, t1),
                10.0,
                10.0,
                day,
            )
            .unwrap();

        // Exit print closes position via the preserved reduce-only Ask; subsequent ticks stay flat.
        assert!(
            s3.on_trade_print(1006, t1 + 50, 93.05, 20.0, true, t1 + 50, day)
                .unwrap()
        );
        let out_flat = s3
            .on_target_bbo(1006, t1 + 200, t3_touch(92.98, 93.02, 20.0, t1 + 200), day)
            .unwrap()
            .unwrap();
        assert_eq!(out_flat.resting_orders, 0);
        assert_eq!(s3.report().cycles, 1);
    }

    #[test]
    fn track03_feed_tracker_rejects_stale_regressed_crossed_and_dedups_refs() {
        let mut s =
            Track03ShadowSession::new(t3_cfg(), utc_day_from_ms(1_795_564_800_000)).unwrap();
        let t = 1_795_564_800_000_u64;
        let day = utc_day_from_ms(t);

        assert!(s.on_reference_touch(1006, 7, t, t3_touch(99.99, 100.01, 10.0, t)));
        // Identical SHM snapshot deduplicates cleanly.
        assert!(!s.on_reference_touch(1006, 7, t + 50, t3_touch(99.99, 100.01, 10.0, t)));
        // Stale & clock-regressed ref rejected.
        assert!(!s.on_reference_touch(1006, 7, t + 6_000, t3_touch(99.99, 100.01, 10.0, t)));
        assert!(!s.on_reference_touch(1006, 7, t + 6_100, t3_touch(99.99, 100.01, 10.0, t - 10)));

        // Healthy target quote places resting orders.
        assert_eq!(
            s.on_target_bbo(1006, t + 100, t3_touch(99.98, 100.02, 10.0, t + 100), day)
                .unwrap()
                .unwrap()
                .resting_orders,
            2
        );
        // Crossed target clears resting quotes fail-closed and increments crossed_book_drops.
        assert!(
            s.on_target_bbo(1006, t + 200, t3_touch(100.05, 100.01, 10.0, t + 200), day)
                .unwrap()
                .is_none()
        );
        assert!(s.arms[0].scratch_resting.is_empty());

        // Clock-regressed target & stale target increment drops cleanly.
        assert!(
            s.on_target_bbo(1006, t + 300, t3_touch(99.98, 100.02, 10.0, t + 50), day)
                .unwrap()
                .is_none()
        );
        assert!(
            s.on_target_bbo(1006, t + 6_500, t3_touch(99.98, 100.02, 10.0, t + 100), day)
                .unwrap()
                .is_none()
        );

        let rep = s.report();
        assert_eq!(rep.arms[0].ref_feeds[0].stale_book_drops, 1);
        assert_eq!(rep.arms[0].ref_feeds[0].clock_regression_drops, 1);
        assert_eq!(rep.arms[0].target_feed.crossed_book_drops, 1);
        assert_eq!(rep.arms[0].target_feed.clock_regression_drops, 1);
        assert_eq!(rep.arms[0].target_feed.stale_book_drops, 1);
        assert_eq!(rep.venue_feed_health.len(), 2);
    }

    #[test]
    fn track03_rebate_zero_fifo_queue_markout_and_exact_cost_decomposition() {
        let mut cfg = t3_cfg();
        cfg.arms[0].maker_fee_bps = 0.0;
        cfg.arms[0].taker_fee_bps = 2.0;
        cfg.arms[0].rebate_bps = 0.0;
        cfg.arms[0].queue_ahead_fraction = 0.5;
        cfg.arms[0].strategy.quote_notional_usd = 200.5;
        cfg.arms[0].strategy.as_inventory_skew_gamma = 0.0;
        cfg.arms[0].strategy.vol_spread_mult = 0.0;
        cfg.arms[0].strategy.directional_markout_skew_mult = 0.0;
        cfg.arms[0].strategy.quiet_regime_min_spread_to_vol = 0.0;
        cfg.arms[0].strategy.quiet_regime_max_entry_vol_bps = None;
        cfg.arms[0].strategy.max_inventory_hold_ms = 5_000;

        let t0 = 1_795_564_800_000_u64;
        let day = utc_day_from_ms(t0);
        let mut s = Track03ShadowSession::new(cfg, day).unwrap();

        // Book 99.98 x 10 / 100.02 x 10 -> Bid @ 99.98 with queue_ahead = 0.5 * 10 = 5.0.
        let out = s
            .on_target_bbo(1006, t0, t3_touch(99.98, 100.02, 10.0, t0), day)
            .unwrap()
            .unwrap();
        assert_eq!(out.resting_orders, 2);

        // At-touch sell print of 3.0 only depletes queue_ahead (5.0 -> 2.0), zero fill!
        assert!(
            !s.on_trade_print(1006, t0 + 100, 99.98, 3.0, false, t0 + 100, day)
                .unwrap()
        );
        assert_eq!(s.report().fills, 0);

        // Next at-touch sell print of 4.0 overflows queue_ahead (2.0) -> fills 2.0 @ 99.98 (~$199.96)!
        assert!(
            s.on_trade_print(1006, t0 + 200, 99.98, 4.0, false, t0 + 200, day)
                .unwrap()
        );
        assert_eq!(s.report().maker_fills, 1);

        // 1.1s later, mid rises to 100.04 (100.02 / 100.06) -> matures 1s markout (+6.0012 bps),
        // and passive Ask at 100.02 (from prior tick) or new Ask gets lifted by strict trade-through!
        let t1 = t0 + 1_300;
        let _ = s
            .on_target_bbo(1006, t1, t3_touch(100.02, 100.06, 10.0, t1), day)
            .unwrap();
        assert!(
            s.on_trade_print(1006, t1 + 50, 100.07, 5.0, true, t1 + 50, day)
                .unwrap()
        );
        let rep1 = s.report();
        assert_eq!(rep1.maker_fills, 2);
        assert_eq!(rep1.taker_exits, 0);
        assert_eq!(rep1.cycles, 1);
        assert!((rep1.arms[0].position_qty).abs() < 1e-12);
        assert_eq!(rep1.fees_usd, 0.0); // maker_fee_bps = 0.0, rebate_bps = 0.0
        assert!(rep1.markout_1s_bps.unwrap() > 5.0);
        let sum_bps = rep1.spread_capture_bps
            + rep1.holding_drift_bps
            + rep1.passive_exit_pnl_bps
            + rep1.taker_exit_slippage_bps;
        assert!((sum_bps - rep1.gross_ex_fee_bps).abs() < 1e-9);
        assert!(
            (rep1.double_sided_usd_per_m - (-rep1.gross_ex_fee_bps / 2.0 * 100.0)).abs() < 1e-6
        );

        // Cycle 2: enter Long @ 100.02, hold past max_inventory_hold_ms (5000ms) -> ReduceOnlyIoc taker exit!
        assert!(
            s.on_trade_print(1006, t1 + 100, 100.01, 5.0, false, t1 + 100, day)
                .unwrap()
        );
        let t2 = t1 + 5_200;
        let out_ioc = s
            .on_target_bbo(1006, t2, t3_touch(100.02, 100.06, 10.0, t2), day)
            .unwrap()
            .unwrap();
        assert!(out_ioc.taker_exited);
        let rep2 = s.report();
        assert_eq!(rep2.taker_exits, 1);
        assert!(rep2.fees_usd > 0.0);
        assert!(rep2.taker_exit_share_s > 0.0);
        assert!(rep2.taker_exit_slippage_bps < 0.0);
        let sum2_bps = rep2.spread_capture_bps
            + rep2.holding_drift_bps
            + rep2.passive_exit_pnl_bps
            + rep2.taker_exit_slippage_bps;
        assert!((sum2_bps - rep2.gross_ex_fee_bps).abs() < 1e-9);
        let expected_fee_cycle = 2.0 * rep2.taker_exit_share_s; // eff_maker=0, taker=2.0
        let expected_ds = ((expected_fee_cycle - rep2.gross_ex_fee_bps) / 2.0) * 100.0;
        assert!((rep2.double_sided_usd_per_m - expected_ds).abs() < 1e-6);
    }

    #[test]
    fn track01_defense_fsm_throttles_caution_blocks_entries_and_preserves_exits() {
        let mut cfg = t1_cfg();
        cfg.arms[0].taker_fee_bps = 0.0;
        cfg.arms[0].strategy.max_dev_bps = 120.0;
        cfg.defense = Some(DefenseFsmConfig {
            caution_oracle_divergence_bps: 8.0,
            reduce_only_oracle_divergence_bps: 30.0,
            halt_oracle_divergence_bps: 80.0,
            caution_size_scale: 0.50,
            min_dwell_ms: 5_000,
            ..DefenseFsmConfig::default()
        });
        let mut s = Track01ShadowSession::new(cfg, utc_day_from_ms(1_000)).unwrap();
        assert_eq!(s.defense_state(), Some(DefenseState::Normal));

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
        // +10 bp divergence (>= 8.0 caution threshold) -> Caution scales clip $500 * 0.50 = $250!
        let act = s
            .on_bbo_pair(
                1002,
                t,
                ll_touch(100.09, 100.11, t),
                ll_touch(99.99, 100.01, t),
                10.0,
                10.0,
                utc_day_from_ms(t),
            )
            .unwrap();
        assert!(matches!(act, Some(LeadLagAction::Enter { .. })));
        assert_eq!(s.defense_state(), Some(DefenseState::Caution));
        let r1 = s.report();
        assert!((r1.arms[0].open_notional_usd - 250.0).abs() < 1e-6);
        assert_eq!(r1.defense_state_transitions, 1);

        // Divergence blows out (+40 bp >= 30.0 reduce_only threshold) while open -> escalates to ReduceOnly,
        // yet exit at exit_due_ms MUST still succeed!
        let exit_t = t + 1_200;
        let exit_act = s
            .on_bbo_pair(
                1002,
                exit_t,
                ll_touch(100.39, 100.41, exit_t),
                ll_touch(99.99, 100.01, exit_t),
                10.0,
                10.0,
                utc_day_from_ms(exit_t),
            )
            .unwrap();
        assert_eq!(
            exit_act,
            Some(LeadLagAction::Exit {
                side: LeadLagSide::Long
            })
        );
        assert_eq!(s.defense_state(), Some(DefenseState::ReduceOnly));
        assert_eq!(s.report().trades, 1);

        // Next entry signal while still inside min_dwell_ms (ReduceOnly) is blocked!
        let block_t = exit_t + 100;
        let blocked = s
            .on_bbo_pair(
                1002,
                block_t,
                ll_touch(100.09, 100.11, block_t),
                ll_touch(99.99, 100.01, block_t),
                10.0,
                10.0,
                utc_day_from_ms(block_t),
            )
            .unwrap();
        assert_eq!(blocked, Some(LeadLagAction::Idle));
        let r2 = s.report();
        assert!(r2.defense_blocked_entries >= 1);
        assert_eq!(r2.defense_state_transitions, 2);
        assert_eq!(r2.defense_state, Some(DefenseState::ReduceOnly));
    }

    #[test]
    fn track02_defense_fsm_blocks_opening_quotes_and_preserves_reducing_leg() {
        let mut cfg = t2_cfg();
        cfg.defense = Some(DefenseFsmConfig {
            reduce_only_feed_staleness_ms: 3_000,
            halt_feed_staleness_ms: 10_000,
            min_dwell_ms: 5_000,
            ..DefenseFsmConfig::default()
        });
        let (mut s, mut t, q) = warm_track02_session(cfg);
        let bid = q.bid.expect("bid should quote");

        // Fill maker bid to carry +1.0 clip long inventory (ask is reduce-only, bid is opening).
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
        assert!(s.report().pairs[0].inventory_clips > 0.0);

        // Stale hedge book (now_ms - hedge.recv_ms = 3_500 >= 3_000 reduce_only threshold, < 5_000 max_book_age_ms):
        // DefenseFsm escalates to ReduceOnly -> opening bid MUST be blocked while reducing ask MUST remain!
        t += 3_500;
        s.on_reference_mid(1002, t - 20, 100.0);
        s.on_reference_mid(1002, t, 100.0);
        let q_def = s
            .on_pair_books(
                1002,
                t,
                mk_touch(99.99, 100.01, t),
                mk_touch(99.99, 100.01, t - 3_500),
                utc_day_from_ms(t),
            )
            .unwrap()
            .unwrap();
        assert_eq!(s.defense_state(), Some(DefenseState::ReduceOnly));
        assert_eq!(q_def.bid, None, "opening bid must be blocked in ReduceOnly");
        assert!(
            q_def.ask.is_some(),
            "reducing ask must stay active in ReduceOnly"
        );
        let rep = s.report();
        assert!(rep.defense_state_transitions >= 1);
        assert!(rep.defense_blocked_entries >= 1);
    }

    #[test]
    fn track03_defense_fsm_enforces_side_defense_halt_reset_and_preserves_exits() {
        let mut cfg = t3_cfg();
        cfg.arms[0].strategy.quiet_regime_max_entry_vol_bps = None;
        cfg.arms[0].strategy.quiet_regime_min_spread_to_vol = 0.0;
        cfg.arms[0].strategy.markout_shield_adverse_threshold_bps = 0.0;
        cfg.defense = Some(DefenseFsmConfig {
            side_defense_adverse_markout_bps: 1.5,
            reduce_only_adverse_markout_bps: 8.0,
            caution_vol_shock_ratio: 25.0,
            reduce_only_vol_shock_ratio: 50.0,
            halt_vol_shock_ratio: 100.0,
            halt_feed_staleness_ms: 4_500,
            min_dwell_ms: 2_000,
            ..DefenseFsmConfig::default()
        });
        let t0 = 1_795_564_800_000_u64;
        let day = utc_day_from_ms(t0);
        let mut s = Track03ShadowSession::new(cfg, day).unwrap();

        // Seed Bid & Ask resting at 99.98 / 100.02.
        let out0 = s
            .on_target_bbo(1006, t0, t3_touch(99.98, 100.02, 20.0, t0), day)
            .unwrap()
            .unwrap();
        assert_eq!(out0.resting_orders, 2);

        // Fill Bid at 99.98; then drop mid sharply to 99.84 (-14 bps) after 1.1s so bid_markout_ewma_bps <= -1.5 bps!
        assert!(
            s.on_trade_print(1006, t0 + 50, 99.97, 20.0, false, t0 + 50, day)
                .unwrap()
        );
        let t1 = t0 + 1_200;
        let out1 = s
            .on_target_bbo(1006, t1, t3_touch(99.82, 99.86, 20.0, t1), day)
            .unwrap()
            .unwrap();
        assert!(matches!(
            s.defense_state(),
            Some(DefenseState::SideDefense {
                blocked_side: BlockedSide::Buy
            })
        ));
        // Long inventory (>0): opening Bid blocked by SideDefense{Buy}, reducing Ask stays resting!
        assert_eq!(out1.resting_orders, 1);
        assert_eq!(s.arms[0].scratch_resting[0].side, QuoteSide::Ask);
        assert!(s.arms[0].resting_meta[0].reduce_only);

        // Stale target book (> 5000ms max_book_age_ms and >= 4500ms halt_feed_staleness_ms)
        // triggers Halt in DefenseFsm while clearing resting quotes fail-closed.
        let t_halt = t1 + 5_100;
        assert!(
            s.on_target_bbo(1006, t_halt, t3_touch(99.82, 99.86, 20.0, t1), day)
                .unwrap()
                .is_none()
        );
        assert_eq!(s.defense_state(), Some(DefenseState::Halt));

        // Fresh tick alone does NOT auto-clear Halt!
        let t_rec = t_halt + 3_000;
        let out_halted = s
            .on_target_bbo(1006, t_rec, t3_touch(99.82, 99.86, 20.0, t_rec), day)
            .unwrap()
            .unwrap();
        assert_eq!(s.defense_state(), Some(DefenseState::Halt));
        // Taker ReduceOnlyIoc exit (max_inventory_hold_ms) or reduce-only ask still allowed even in Halt!
        assert!(out_halted.taker_exited || out_halted.resting_orders <= 1);

        // Explicit reconciled reset lifts Halt -> Normal once dwell elapsed and snapshot healthy.
        let reset_snap = DefenseTelemetrySnapshot {
            now_ms: t_rec + 100,
            feed_staleness_ms: 0,
            feed_gap_ms: 100,
            vol_shock_ratio: 1.0,
            trailing_bid_markout_bps: 0.0,
            trailing_ask_markout_bps: 0.0,
            oracle_divergence_bps: 0.0,
            daily_loss_utilization: 0.0,
            budget_cooldown_active: false,
            budget_halted: false,
            clock_regressed_externally: false,
        };
        assert_eq!(
            s.acknowledge_defense_reset(&reset_snap),
            Ok(DefenseState::Normal)
        );
        assert_eq!(s.defense_state(), Some(DefenseState::Normal));
        let rep = s.report();
        assert!(rep.defense_state_transitions >= 3);
        assert!(rep.defense_blocked_entries >= 1);
    }

    #[test]
    fn track02_defense_fsm_scales_caution_and_keeps_reducing_unthrottled() {
        let mut cfg = t2_cfg();
        cfg.pairs[0].clip_usd = 500.0;
        cfg.defense = Some(DefenseFsmConfig {
            caution_feed_staleness_ms: 1_000,
            reduce_only_feed_staleness_ms: 4_000,
            halt_feed_staleness_ms: 10_000,
            caution_size_scale: 0.50,
            min_dwell_ms: 5_000,
            ..DefenseFsmConfig::default()
        });
        let (mut s, mut t, _) = warm_track02_session(cfg);

        // Trigger Caution via hedge feed staleness = 1_500ms (>= 1_000 caution, < 4_000 reduce_only).
        t += 1_500;
        s.on_reference_mid(1002, t - 20, 100.0);
        s.on_reference_mid(1002, t, 100.0);
        let q_caution = s
            .on_pair_books(
                1002,
                t,
                mk_touch(99.99, 100.01, t),
                mk_touch(99.99, 100.01, t - 1_500),
                utc_day_from_ms(t),
            )
            .unwrap()
            .unwrap();
        assert_eq!(s.defense_state(), Some(DefenseState::Caution));
        let bid = q_caution.bid.expect("opening bid allowed in Caution");

        // Opening fill in Caution must scale by caution_size_scale (0.50 * $500 = $250 -> +0.50 clips).
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
        let r_after_open = s.report();
        assert!((r_after_open.pairs[0].inventory_clips - 0.50).abs() < 1e-9);
        assert!((r_after_open.pairs[0].maker_qty * bid - 250.0).abs() < 1e-6);

        // Now long (+0.50 clips): reducing ask must stay UNTHROTTLED (1.0 * $500) even in Caution!
        t += 100;
        s.on_reference_mid(1002, t, 100.0);
        let q_next = s
            .on_pair_books(
                1002,
                t,
                mk_touch(99.99, 100.01, t),
                mk_touch(99.99, 100.01, t),
                utc_day_from_ms(t),
            )
            .unwrap()
            .unwrap();
        let ask = q_next.ask.expect("reducing ask allowed");
        assert!(
            s.on_maker_trade_print(
                1002,
                t,
                ask + 0.01,
                10.0,
                true,
                mk_touch(99.99, 100.01, t),
                utc_day_from_ms(t),
            )
            .unwrap()
        );
        let r_after_reduce = s.report();
        // Full 1.0 clip ($500) executed on the reducing ask (+0.50 - 1.00 = -0.50 clips).
        assert!((r_after_reduce.pairs[0].inventory_clips - (-0.50)).abs() < 1e-9);
    }

    #[test]
    fn track03_delayed_and_depth_clamped_ioc_exit_and_stale_terminal_residual() {
        let mut cfg = t3_cfg();
        cfg.arms[0].ioc_exit_delay_ms = 330;
        cfg.arms[0].max_l1_depth_fraction = 0.50;
        cfg.arms[0].strategy.quote_notional_usd = 200.5;
        cfg.arms[0].strategy.as_inventory_skew_gamma = 0.0;
        cfg.arms[0].strategy.vol_spread_mult = 0.0;
        cfg.arms[0].strategy.directional_markout_skew_mult = 0.0;
        cfg.arms[0].strategy.quiet_regime_min_spread_to_vol = 0.0;
        cfg.arms[0].strategy.quiet_regime_max_entry_vol_bps = None;
        cfg.arms[0].strategy.max_inventory_hold_ms = 5_000;
        cfg.arms[0].strategy.max_book_age_ms = 5_000;

        let t0 = 1_795_564_800_000_u64;
        let day = utc_day_from_ms(t0);
        let mut s = Track03ShadowSession::new(cfg, day).unwrap();

        // Enter long 2.0 shares @ 99.98 via strict trade-through.
        let _ = s
            .on_target_bbo(1006, t0, t3_touch(99.98, 100.02, 20.0, t0), day)
            .unwrap()
            .unwrap();
        assert!(
            s.on_trade_print(1006, t0 + 100, 99.97, 5.0, false, t0 + 100, day)
                .unwrap()
        );
        assert!((s.report().arms[0].position_qty - 2.0).abs() < 1e-9);

        // At timeout (t1 = t0 + 5_200), ReduceOnlyIoc is scheduled for +330ms, NOT filled immediately.
        let t1 = t0 + 5_200;
        let out_sched = s
            .on_target_bbo(1006, t1, t3_touch(100.00, 100.04, 10.0, t1), day)
            .unwrap()
            .unwrap();
        assert!(!out_sched.taker_exited);
        assert_eq!(s.report().taker_exits, 0);
        assert!((s.report().terminal_residual_notional_usd - 200.04).abs() < 1e-6);

        // Intermediate tick before 330ms delay elapses sets thin opposite touch (bid_sz=2.0 -> 0.50 * 2.0 = 1.0 max),
        // without executing the IOC early.
        let out_wait = s
            .on_target_bbo(1006, t1 + 200, t3_touch(100.00, 100.04, 2.0, t1 + 200), day)
            .unwrap()
            .unwrap();
        assert!(!out_wait.taker_exited);
        assert_eq!(s.report().taker_exits, 0);

        // After 330ms delay, pre-install evaluation executes against the prior fresh touch (bid_sz=2.0 -> 1.0 max),
        // filling only 1.0 share, incrementing depth_clipped_exits, and leaving 1.0 share open!
        let t_exec = t1 + 350;
        let out_clip = s
            .on_target_bbo(1006, t_exec, t3_touch(100.00, 100.04, 2.0, t_exec), day)
            .unwrap()
            .unwrap();
        assert!(out_clip.taker_exited);
        let rep_clip = s.report();
        assert_eq!(rep_clip.taker_exits, 1);
        assert_eq!(rep_clip.depth_clipped_exits, 1);
        assert_eq!(rep_clip.arms[0].depth_clipped_exits, 1);
        assert!((rep_clip.arms[0].position_qty - 1.0).abs() < 1e-9);
        assert!((rep_clip.terminal_residual_notional_usd - 100.02).abs() < 1e-6);

        // Stale book (> max_book_age_ms = 5_000) must NEVER fill the remaining 1.0 share;
        // instead it increments stale_exit_blocks and re-queues for t_stale + 330ms.
        let t_stale = t_exec + 5_100;
        assert!(
            s.on_target_bbo(1006, t_stale, t3_touch(100.00, 100.04, 20.0, t_exec), day)
                .unwrap()
                .is_none()
        );
        let rep_stale = s.report();
        assert_eq!(rep_stale.taker_exits, 1);
        assert_eq!(rep_stale.stale_book_drops, 1);
        assert_eq!(rep_stale.stale_exit_blocks, 1);
        assert!((rep_stale.arms[0].position_qty - 1.0).abs() < 1e-9);
        assert!((rep_stale.terminal_residual_notional_usd - 100.02).abs() < 1e-6);

        // Fresh book at +100ms (< 330ms re-queued delay) must NOT fill at 0ms.
        let t_early = t_stale + 100;
        let out_early = s
            .on_target_bbo(1006, t_early, t3_touch(100.00, 100.04, 4.0, t_early), day)
            .unwrap()
            .unwrap();
        assert!(!out_early.taker_exited);
        assert_eq!(s.report().taker_exits, 1);

        // Fresh book after full 330ms retry delay closes the remaining 1.0 share cleanly -> residual becomes 0.0.
        let t_close = t_stale + 350;
        let out_close = s
            .on_target_bbo(1006, t_close, t3_touch(100.00, 100.04, 4.0, t_close), day)
            .unwrap()
            .unwrap();
        assert!(out_close.taker_exited);
        let rep_flat = s.report();
        assert_eq!(rep_flat.taker_exits, 2);
        assert_eq!(rep_flat.depth_clipped_exits, 1);
        assert_eq!(rep_flat.stale_exit_blocks, 1);
        assert!(rep_flat.arms[0].position_qty.abs() < 1e-9);
        assert!(rep_flat.terminal_residual_notional_usd.abs() < 1e-9);
    }

    fn t3_p0_cfg() -> Track03ShadowConfig {
        let mut cfg = t3_cfg();
        cfg.arms[0].ioc_exit_delay_ms = 330;
        cfg.arms[0].max_l1_depth_fraction = 1.0;
        cfg.arms[0].strategy.quote_notional_usd = 200.0;
        cfg.arms[0].strategy.max_position_notional_usd = 400.0;
        cfg.arms[0].strategy.as_inventory_skew_gamma = 0.0;
        cfg.arms[0].strategy.vol_spread_mult = 0.0;
        cfg.arms[0].strategy.directional_markout_skew_mult = 0.0;
        cfg.arms[0].strategy.quiet_regime_min_spread_to_vol = 0.0;
        cfg.arms[0].strategy.quiet_regime_max_entry_vol_bps = None;
        cfg.arms[0].strategy.max_inventory_hold_ms = 5_000;
        cfg.arms[0].strategy.max_book_age_ms = 5_000;
        cfg
    }

    #[test]
    fn track03_p0_stale_gap_recovery_requeues_330ms_without_zero_ms_fill() {
        let cfg = t3_p0_cfg();
        let t0 = 1_795_564_800_000_u64;
        let day = utc_day_from_ms(t0);
        let mut s = Track03ShadowSession::new(cfg, day).unwrap();

        // Enter long 2.0 shares @ 99.98.
        let _ = s
            .on_target_bbo(1006, t0, t3_touch(99.98, 100.02, 20.0, t0), day)
            .unwrap()
            .unwrap();
        assert!(
            s.on_trade_print(1006, t0 + 100, 99.97, 5.0, false, t0 + 100, day)
                .unwrap()
        );

        // Fresh touch at t0 + 4_900 stays fresh at t0 + 5_100 (age 200ms <= 5_000ms),
        // scheduling ReduceOnlyIoc for (t0 + 5_100) + 330 = t0 + 5_430.
        let t_book = t0 + 4_900;
        let _ = s
            .on_target_bbo(1006, t_book, t3_touch(100.00, 100.04, 10.0, t_book), day)
            .unwrap()
            .unwrap();
        let t_sched = t0 + 5_100;
        let out_sched = s
            .on_target_bbo(1006, t_sched, t3_touch(100.00, 100.04, 10.0, t_sched), day)
            .unwrap()
            .unwrap();
        assert!(!out_sched.taker_exited);

        // Silent gap of 5_500ms (> max_book_age_ms=5_000): at t_recover = t_sched + 5_500,
        // a brand-new fresh touch arrives, AFTER exec_at_ms = t_sched + 330 has passed.
        // Pre-install freshness sees prev_touch was stale -> increments stale_exit_blocks = 1,
        // re-queues for t_recover + 330, and MUST NOT fill at 0ms on t_recover!
        let t_recover = t_sched + 5_500;
        let out_recover = s
            .on_target_bbo(
                1006,
                t_recover,
                t3_touch(100.00, 100.04, 10.0, t_recover),
                day,
            )
            .unwrap()
            .unwrap();
        assert!(!out_recover.taker_exited);
        let rep_rec = s.report();
        assert_eq!(rep_rec.stale_exit_blocks, 1);
        assert_eq!(rep_rec.arms[0].stale_exit_blocks, 1);
        assert_eq!(rep_rec.taker_exits, 0);
        assert!((rep_rec.arms[0].position_qty - 2.0).abs() < 1e-9);

        // At t_recover + 200 (< 330ms): still waiting.
        let out_mid = s
            .on_target_bbo(
                1006,
                t_recover + 200,
                t3_touch(100.00, 100.04, 10.0, t_recover + 200),
                day,
            )
            .unwrap()
            .unwrap();
        assert!(!out_mid.taker_exited);

        // At t_recover + 330: fills cleanly on the prior fresh touch!
        let out_fill = s
            .on_target_bbo(
                1006,
                t_recover + 330,
                t3_touch(100.00, 100.04, 10.0, t_recover + 330),
                day,
            )
            .unwrap()
            .unwrap();
        assert!(out_fill.taker_exited);
        let rep_done = s.report();
        assert_eq!(rep_done.stale_exit_blocks, 1);
        assert_eq!(rep_done.taker_exits, 1);
        assert!(rep_done.arms[0].position_qty.abs() < 1e-9);
    }

    #[test]
    fn track03_p0_two_iocs_on_same_touch_conserve_single_use_l1_depth() {
        let mut cfg = t3_p0_cfg();
        cfg.arms[0].max_l1_depth_fraction = 0.50;
        let t0 = 1_795_564_800_000_u64;
        let day = utc_day_from_ms(t0);
        let mut s = Track03ShadowSession::new(cfg, day).unwrap();

        // Enter long 2.0 shares @ 99.98.
        let _ = s
            .on_target_bbo(1006, t0, t3_touch(99.98, 100.02, 20.0, t0), day)
            .unwrap()
            .unwrap();
        assert!(
            s.on_trade_print(1006, t0 + 100, 99.97, 5.0, false, t0 + 100, day)
                .unwrap()
        );
        assert!((s.report().arms[0].position_qty - 2.0).abs() < 1e-9);

        // Install a touch with bid_sz = 3.0 -> 0.50 * 3.0 = 1.50 total L1 IOC budget for this touch.
        let touch_shared = t3_touch(100.00, 100.04, 3.0, t0 + 200);
        let _ = s.on_target_bbo(1006, t0 + 200, touch_shared, day).unwrap();
        assert!((s.arms[0].rem_bid_sz - 1.5).abs() < 1e-12);

        // IOC #1 asks for 1.0 share -> fills 1.0 share, leaving 0.50 share on this same touch!
        assert!(s.arms[0].execute_ioc_exit(t0 + 250, QuoteSide::Ask, 99.90, 1.0, touch_shared));
        assert!((s.arms[0].rem_bid_sz - 0.5).abs() < 1e-12);
        assert!((s.arms[0].pos_qty - 1.0).abs() < 1e-9);

        // IOC #2 on the SAME touch asks for 1.0 share -> MUST only fill 0.50 share (NOT 1.0!),
        // exhausting rem_bid_sz to 0.0 and incrementing depth_clipped_exits!
        assert!(s.arms[0].execute_ioc_exit(t0 + 260, QuoteSide::Ask, 99.90, 1.0, touch_shared));
        assert!(s.arms[0].rem_bid_sz.abs() < 1e-12);
        assert!((s.arms[0].pos_qty - 0.5).abs() < 1e-9);
        assert_eq!(s.report().depth_clipped_exits, 1);

        // IOC #3 on the SAME touch fills 0.0 shares and clips again.
        assert!(!s.arms[0].execute_ioc_exit(t0 + 270, QuoteSide::Ask, 99.90, 0.5, touch_shared));
        assert_eq!(s.report().depth_clipped_exits, 2);

        // Only a subsequent fresh on_target_bbo replenishes the depth budget.
        let _ = s
            .on_target_bbo(1006, t0 + 300, t3_touch(100.00, 100.04, 4.0, t0 + 300), day)
            .unwrap();
        assert!((s.arms[0].rem_bid_sz - 2.0).abs() < 1e-12);
    }

    #[test]
    fn track03_p0_partial_exit_preserves_true_residual_and_mtm() {
        let mut cfg = t3_p0_cfg();
        cfg.arms[0].max_l1_depth_fraction = 0.50;
        let t0 = 1_795_564_800_000_u64;
        let day = utc_day_from_ms(t0);
        let mut s = Track03ShadowSession::new(cfg, day).unwrap();

        let _ = s
            .on_target_bbo(1006, t0, t3_touch(99.98, 100.02, 20.0, t0), day)
            .unwrap()
            .unwrap();
        assert!(
            s.on_trade_print(1006, t0 + 100, 99.97, 5.0, false, t0 + 100, day)
                .unwrap()
        );

        // Timeout at t1 = t0 + 5_200 on thin touch (bid_sz=1.0 -> 0.50 max).
        let t1 = t0 + 5_200;
        let _ = s
            .on_target_bbo(1006, t1, t3_touch(100.08, 100.12, 1.0, t1), day)
            .unwrap()
            .unwrap();
        let t_exec = t1 + 350;
        let out = s
            .on_target_bbo(1006, t_exec, t3_touch(100.08, 100.12, 1.0, t_exec), day)
            .unwrap()
            .unwrap();
        assert!(out.taker_exited);

        let rep = s.report();
        // Closed 0.5 share (99.98 -> 100.08 = +$0.05 realized), remaining 1.5 shares marked at mid 100.10.
        assert!((rep.arms[0].position_qty - 1.5).abs() < 1e-9);
        assert!((rep.realized_pnl_usd - 0.05).abs() < 1e-6);
        assert!((rep.unrealized_pnl_usd - 1.5 * (100.10 - 99.98)).abs() < 1e-6);
        assert!((rep.terminal_residual_notional_usd - 1.5 * 100.10).abs() < 1e-6);
        assert_eq!(rep.depth_clipped_exits, 1);
    }

    #[test]
    fn track03_p0_declared_end_ms_ignores_late_ref_bbo_and_print_ticks() {
        let t0 = 1_795_564_800_000_u64;
        let end_ms = t0 + 3_000;
        let day = utc_day_from_ms(t0);

        let mut cfg_base = t3_p0_cfg();
        cfg_base.declared_end_ms = Some(end_ms);
        let mut s_base = Track03ShadowSession::new(cfg_base.clone(), day).unwrap();
        let mut s_late = Track03ShadowSession::new(cfg_base, day).unwrap();

        for sess in [&mut s_base, &mut s_late] {
            assert!(sess.on_reference_touch(1006, 7, t0, t3_touch(99.99, 100.01, 10.0, t0)));
            let _ = sess
                .on_target_bbo(1006, t0, t3_touch(99.98, 100.02, 10.0, t0), day)
                .unwrap()
                .unwrap();
            assert!(
                sess.on_trade_print(1006, t0 + 100, 99.97, 5.0, false, t0 + 100, day)
                    .unwrap()
            );
            let _ = sess
                .on_target_bbo(1006, end_ms, t3_touch(100.04, 100.08, 10.0, end_ms), day)
                .unwrap()
                .unwrap();
        }

        // Feed late ref, late BBO, and late trade print strictly after declared_end_ms.
        assert!(!s_late.on_reference_touch(
            1006,
            7,
            end_ms + 500,
            t3_touch(105.00, 105.04, 10.0, end_ms + 500)
        ));
        assert!(
            s_late
                .on_target_bbo(
                    1006,
                    end_ms + 1_000,
                    t3_touch(105.00, 105.04, 10.0, end_ms + 1_000),
                    day
                )
                .unwrap()
                .is_none()
        );
        assert!(
            !s_late
                .on_trade_print(
                    1006,
                    end_ms + 1_500,
                    105.10,
                    20.0,
                    true,
                    end_ms + 1_500,
                    day
                )
                .unwrap()
        );

        let r_base = s_base.report();
        let r_late = s_late.report();
        assert_eq!(r_base.fills, r_late.fills);
        assert_eq!(
            r_base.arms[0].total_book_ticks,
            r_late.arms[0].total_book_ticks
        );
        assert!((r_base.realized_pnl_usd - r_late.realized_pnl_usd).abs() < 1e-12);
        assert!((r_base.unrealized_pnl_usd - r_late.unrealized_pnl_usd).abs() < 1e-12);
        assert!(
            (r_base.terminal_residual_notional_usd - r_late.terminal_residual_notional_usd).abs()
                < 1e-12
        );
    }

    #[test]
    fn track03_p0_terminal_drain_ms_cancels_openings_and_flattens_after_delay() {
        let t0 = 1_795_564_800_000_u64;
        let end_ms = t0 + 4_000;
        let day = utc_day_from_ms(t0);

        let mut cfg = t3_p0_cfg();
        cfg.arms[0].strategy.max_inventory_hold_ms = 60_000; // long hold so ONLY terminal drain triggers
        cfg.declared_end_ms = Some(end_ms);
        cfg.terminal_drain_ms = Some(1_000); // drain starts at t0 + 3_000
        let mut s = Track03ShadowSession::new(cfg, day).unwrap();

        let _ = s
            .on_target_bbo(1006, t0, t3_touch(99.98, 100.02, 20.0, t0), day)
            .unwrap()
            .unwrap();
        assert!(
            s.on_trade_print(1006, t0 + 100, 99.97, 5.0, false, t0 + 100, day)
                .unwrap()
        );

        // Before terminal drain (t0 + 2_000): both opening bid & reducing ask stay resting.
        let out_pre = s
            .on_target_bbo(
                1006,
                t0 + 2_000,
                t3_touch(99.98, 100.02, 20.0, t0 + 2_000),
                day,
            )
            .unwrap()
            .unwrap();
        assert_eq!(out_pre.resting_orders, 2);
        assert!(!out_pre.taker_exited);

        // At terminal drain start (t0 + 3_000): opening bid cancelled (only reducing ask stays),
        // and IOC flatten is scheduled for t0 + 3_330 (NOT filled at 0ms).
        let out_drain = s
            .on_target_bbo(
                1006,
                t0 + 3_000,
                t3_touch(99.98, 100.02, 20.0, t0 + 3_000),
                day,
            )
            .unwrap()
            .unwrap();
        assert_eq!(out_drain.resting_orders, 1);
        assert_eq!(s.arms[0].scratch_resting[0].side, QuoteSide::Ask);
        assert!(!out_drain.taker_exited);

        // Before +330ms delay: still open.
        let out_wait = s
            .on_target_bbo(
                1006,
                t0 + 3_200,
                t3_touch(99.98, 100.02, 20.0, t0 + 3_200),
                day,
            )
            .unwrap()
            .unwrap();
        assert!(!out_wait.taker_exited);

        // At t0 + 3_350 (<= declared_end_ms): terminal drain IOC executes and flattens cleanly!
        let out_flat = s
            .on_target_bbo(
                1006,
                t0 + 3_350,
                t3_touch(99.98, 100.02, 20.0, t0 + 3_350),
                day,
            )
            .unwrap()
            .unwrap();
        assert!(out_flat.taker_exited);
        let rep = s.report();
        assert_eq!(rep.taker_exits, 1);
        assert!(rep.arms[0].position_qty.abs() < 1e-9);
        assert!(rep.terminal_residual_notional_usd.abs() < 1e-9);
    }

    #[test]
    fn track03_p0_same_ms_dual_ordering_and_invalid_clock_regression_guards() {
        let t0 = 1_795_564_800_000_u64;
        let day = utc_day_from_ms(t0);

        // Order A: book first, then trade print at identical t0 + 100 -> resting quote exists, fills 1.
        let mut s_book_first = Track03ShadowSession::new(t3_p0_cfg(), day).unwrap();
        let _ = s_book_first
            .on_target_bbo(1006, t0 + 100, t3_touch(99.98, 100.02, 20.0, t0 + 100), day)
            .unwrap()
            .unwrap();
        assert!(
            s_book_first
                .on_trade_print(1006, t0 + 100, 99.97, 5.0, false, t0 + 100, day)
                .unwrap()
        );
        assert_eq!(s_book_first.report().maker_fills, 1);

        // Order B: trade print before any book at t0 + 100 -> zero fills, zero state corruption;
        // then invalid/crossed & clock-regressed ticks are rejected fail-closed.
        let mut s_print_first = Track03ShadowSession::new(t3_p0_cfg(), day).unwrap();
        assert!(
            !s_print_first
                .on_trade_print(1006, t0 + 100, 99.97, 5.0, false, t0 + 100, day)
                .unwrap()
        );
        assert_eq!(s_print_first.report().maker_fills, 0);

        // Invalid (crossed) BBO and invalid (NaN / negative) trade print and clock-regressed BBO.
        assert!(
            s_print_first
                .on_target_bbo(
                    1006,
                    t0 + 200,
                    t3_touch(100.05, 100.01, 10.0, t0 + 200),
                    day
                )
                .unwrap()
                .is_none()
        );
        assert!(
            s_print_first
                .on_target_bbo(1006, t0 + 300, t3_touch(99.98, 100.02, 10.0, t0 + 300), day)
                .unwrap()
                .is_some()
        );
        // Clock-regressed BBO (now_ms = t0 + 250 < prev t0 + 300) drops cleanly.
        assert!(
            s_print_first
                .on_target_bbo(1006, t0 + 250, t3_touch(99.98, 100.02, 10.0, t0 + 250), day)
                .unwrap()
                .is_none()
        );
        assert!(
            !s_print_first
                .on_trade_print(1006, t0 + 350, f64::NAN, 1.0, false, t0 + 350, day)
                .unwrap()
        );
        let rep = s_print_first.report();
        assert_eq!(rep.crossed_book_drops, 1);
        assert_eq!(rep.clock_regression_drops, 1);
        assert_eq!(rep.stale_exit_blocks, 0);
    }
}
