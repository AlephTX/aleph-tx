//! Track 02 (`02_hedge_arbitrage`) venue-decoupled maker-first basis quoter.
//!
//! Rests passive quotes on a *maker* venue whose prices are derived from the *hedge*
//! venue touch, so that a fill can be immediately hedged with a taker order while the
//! entered basis beats a causal fair basis by an inventory-skewed edge:
//!
//! ```text
//! b      = ln(maker_mid) - ln(hedge_mid)
//! fair   = EMA_tau(b)  (strictly prior observations)
//! shift  = exp(lead_beta * (b - fair))        // hedge venue expected to follow the maker
//! bid   <= hedge_bid * shift * exp(fair - e_bid),  e_bid = e0 + skew * q
//! ask   >= hedge_ask * shift * exp(fair + e_ask),  e_ask = e0 - skew * q
//! ```
//!
//! Quotes join (or optionally improve by one tick) the maker best price, never cross it,
//! and are withdrawn on stale books, warm-up or at the inventory cap. `lead_beta`
//! counters the dominant loss seen in `scripts/replay_maker_hedge_basis.py`: a maker
//! fill on the fast venue is followed by the slow hedge venue moving against the hedge
//! during its speed bump (mean hedge slip about -2 to -4 bp on Arcus -> Lighter RWA).
//!
//! Pure decision logic with zero exchange-adapter imports. Execution adapters must
//! reserve worst-case exposure for both legs before dispatch and treat the hedge leg's
//! ambiguity per the live-hedge journal rules (`AGENTS.md`).

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

/// Top-of-book snapshot with local receipt time.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
pub struct Touch {
    pub bid: f64,
    pub ask: f64,
    pub recv_ms: u64,
}

impl Touch {
    fn valid(&self) -> bool {
        self.bid.is_finite() && self.ask.is_finite() && self.bid > 0.0 && self.ask > self.bid
    }

    pub fn mid(&self) -> f64 {
        0.5 * (self.bid + self.ask)
    }

    /// L1 size-weighted microprice `(bid * ask_sz + ask * bid_sz) / (bid_sz + ask_sz)`,
    /// falling back to [`Self::mid`] if sizes are missing or non-positive.
    pub fn microprice(&self, bid_sz: f64, ask_sz: f64) -> f64 {
        if self.valid() && bid_sz.is_finite() && ask_sz.is_finite() && bid_sz > 0.0 && ask_sz > 0.0
        {
            let tot = bid_sz + ask_sz;
            if tot > 0.0 && tot.is_finite() {
                return (self.bid * ask_sz + self.ask * bid_sz) / tot;
            }
        }
        self.mid()
    }
}

/// Why no quotes were produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum QuoteBlock {
    InvalidBook,
    StaleMaker,
    StaleHedge,
    WarmingUp,
}

/// Passive quotes for the maker venue (`None` = do not rest that side).
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
pub struct MakerHedgeQuotes {
    pub bid: Option<f64>,
    pub ask: Option<f64>,
    /// Causal fair basis used for this decision, bps.
    pub fair_basis_bps: f64,
    /// Current basis minus fair, bps.
    pub dislocation_bps: f64,
}

/// Side of a maker fill.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum FillSide {
    /// Our maker bid was hit: we are long the maker leg -> sell the hedge leg.
    MakerBought,
    /// Our maker ask was lifted: we are short the maker leg -> buy the hedge leg.
    MakerSold,
}

/// External reference cancel-shield configuration (exploits zero-speed-bump maker cancels).
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RefCancelShieldConfig {
    /// Enable external-reference adverse-selection quote withdrawal (default `false`).
    pub enabled: bool,
    /// Withdraw the exposed side if external reference moved adversely by `>= adverse_move_bps`.
    pub adverse_move_bps: f64,
    /// Trailing lookback window for external reference return, ms (`> 0`).
    pub lookback_ms: u64,
    /// Allow inventory-reducing side even when the external shield would block it (`false` = fail-closed).
    pub allow_reduce_only_through_shield: bool,
    /// Withdraw both sides when the shield is enabled but reference return is unready, stale, or future-dated
    /// (`false` = fail-open for backwards compatibility; still respects `allow_reduce_only_through_shield`).
    pub fail_closed_when_unready: bool,
}

impl Default for RefCancelShieldConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            adverse_move_bps: 2.5,
            lookback_ms: 200,
            allow_reduce_only_through_shield: false,
            fail_closed_when_unready: false,
        }
    }
}

/// Decoupled economic viability model for a shielded maker-hedge pair.
///
/// Accounts for maker/hedge fees, cross-stablecoin settlement drag, expected hedge
/// execution slippage, and risk-domain separation with zero exchange-adapter coupling.
#[derive(Debug, Clone, Copy, PartialEq, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ShieldedPairEconomics {
    /// Maker-leg fee in bps (positive = cost, negative = rebate).
    pub maker_fee_bps: f64,
    /// Hedge-leg taker fee in bps (e.g. `2.25` for Arcus RWA level_0).
    pub hedge_taker_fee_bps: f64,
    /// Additional basis/conversion drag in bps applied when `cross_stablecoin` is `true`.
    pub cross_stablecoin_drag_bps: f64,
    /// Expected adverse hedge execution slippage in bps.
    pub expected_hedge_slip_bps: f64,
    /// Whether the maker and hedge legs settle in different stablecoins (e.g. USDG vs USDC).
    pub cross_stablecoin: bool,
    /// Whether both legs share the same counterparty/liquidation risk domain.
    pub same_risk_domain: bool,
}

impl ShieldedPairEconomics {
    /// Compute expected net edge in bps after fees, expected hedge slippage, and
    /// cross-stablecoin drag (when `cross_stablecoin` is `true`).
    pub fn net_expected_edge_bps(&self, expected_gross_edge_bps: f64) -> f64 {
        let stablecoin_drag = if self.cross_stablecoin {
            self.cross_stablecoin_drag_bps
        } else {
            0.0
        };
        if !expected_gross_edge_bps.is_finite()
            || !self.maker_fee_bps.is_finite()
            || !self.hedge_taker_fee_bps.is_finite()
            || !stablecoin_drag.is_finite()
            || !self.expected_hedge_slip_bps.is_finite()
        {
            return f64::NAN;
        }
        expected_gross_edge_bps
            - self.maker_fee_bps
            - self.hedge_taker_fee_bps
            - stablecoin_drag
            - self.expected_hedge_slip_bps
    }

    /// Return `true` only when the legs are in distinct risk domains (`!same_risk_domain`)
    /// and finite net expected edge is at least `min_net_edge_bps`.
    pub fn is_economically_viable(
        &self,
        expected_gross_edge_bps: f64,
        min_net_edge_bps: f64,
    ) -> bool {
        if self.same_risk_domain || !min_net_edge_bps.is_finite() {
            return false;
        }
        let net = self.net_expected_edge_bps(expected_gross_edge_bps);
        net.is_finite() && net >= min_net_edge_bps
    }
}

/// Configuration for [`MakerHedgeQuoter`].
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct MakerHedgeQuoterConfig {
    /// Base required edge versus fair basis, bps (`>= 0`).
    pub e0_bps: f64,
    /// Additional edge per clip of inventory on the increasing side, bps (`>= 0`).
    pub skew_bps_per_clip: f64,
    /// Maximum absolute inventory in clips (`> 0`).
    pub q_max_clips: f64,
    /// Fair-basis EMA time constant, ms (`> 0`).
    pub tau_ms: u64,
    /// Fraction of the current dislocation the hedge venue is expected to follow (`0..=1.5`).
    pub lead_beta: f64,
    /// Improve the maker best price by one tick when the spread allows it.
    pub improve_one_tick: bool,
    /// Maker venue tick size (`> 0`).
    pub tick_size: f64,
    /// Maximum book age for either venue, ms (`> 0`).
    pub max_book_age_ms: u64,
    /// Observation time before quoting, ms.
    pub warmup_ms: u64,
    /// Gap after which the fair EMA is re-seeded and warm-up restarts, ms (`> 0`).
    pub max_gap_ms: u64,
    /// External reference cancel shield configuration (opt-in, disabled by default).
    pub ref_shield: RefCancelShieldConfig,
    /// Multiplier on causal basis volatility (`basis_sigma_bps`) added to `e0_bps`
    /// (`effective_min_edge_bps = e0_bps + vol_edge_mult * basis_sigma_bps`). Default `0.0` (off).
    pub vol_edge_mult: f64,
    /// EWMA time constant for causal basis volatility tracking, ms (`> 0`). Default `300_000`.
    pub basis_vol_tau_ms: u64,
    /// Nonlinear inventory skew power near `q_max_clips` (`>= 1.0`). Default `1.0` (linear).
    pub nonlinear_skew_power: f64,
    /// Suppress opening quotes when short-horizon basis drift moves adversely by `>= threshold` bps.
    /// Default `0.0` (off).
    pub basis_drift_guard_bps: f64,
    /// Short-horizon EMA time constant for basis-drift tracking, ms (`> 0`). Default `5_000`.
    pub basis_drift_tau_ms: u64,
    /// Minimum opposite-side hedge BBO L1 notional (`bid * bid_sz` for maker bid,
    /// `ask * ask_sz` for maker ask) required before quoting opening size, USD (`>= 0`). Default `0.0` (off).
    pub min_hedge_l1_notional_usd: f64,
    /// Minimum favorable basis z-score `|(b - fair) * 1e4| / max(basis_sigma_bps, basis_vol_floor_bps)`
    /// required to open new inventory (`>= 0.0`). Default `0.0` (off). Never blocks reduce-only exits.
    pub min_basis_zscore: f64,
    /// Volatility floor in bps used in the denominator of basis z-score and drift-to-vol ratio (`>= 0.0`).
    /// Default `0.0`.
    pub basis_vol_floor_bps: f64,
    /// Maximum allowed trend drift-to-volatility ratio `|(fast_fair - fair) * 1e4| / denom_bps`
    /// for opening quotes (`>= 0.0`). Default `0.0` (off). Never blocks reduce-only exits.
    pub max_drift_to_vol_ratio: f64,
    /// When `true`, quote only the statistically favorable mean-reverting opening side
    /// (`bid` when `b < fair`, `ask` when `b > fair`) while keeping reduce-only exits active.
    /// Default `false`.
    pub quote_favorable_side_only: bool,
    /// Anchor basis computation `b = ln(maker_anchor) - ln(hedge_anchor)` on L1 microprice
    /// instead of simple mid when L1 sizes are supplied via [`MakerHedgeQuoter::step_with_l1_microprice`].
    /// Default `false`.
    pub use_microprice_anchor: bool,
    /// Optional passive-first-then-IOC hedge execution controller config (default-off).
    pub passive_hedge: PassiveHedgeConfig,
}

impl Default for MakerHedgeQuoterConfig {
    fn default() -> Self {
        Self {
            e0_bps: 5.0,
            skew_bps_per_clip: 0.5,
            q_max_clips: 4.0,
            tau_ms: 3_600_000,
            lead_beta: 0.0,
            improve_one_tick: false,
            tick_size: 0.01,
            max_book_age_ms: 5_000,
            warmup_ms: 900_000,
            max_gap_ms: 60_000,
            ref_shield: RefCancelShieldConfig::default(),
            vol_edge_mult: 0.0,
            basis_vol_tau_ms: 300_000,
            nonlinear_skew_power: 1.0,
            basis_drift_guard_bps: 0.0,
            basis_drift_tau_ms: 5_000,
            min_hedge_l1_notional_usd: 0.0,
            min_basis_zscore: 0.0,
            basis_vol_floor_bps: 0.0,
            max_drift_to_vol_ratio: 0.0,
            quote_favorable_side_only: false,
            use_microprice_anchor: false,
            passive_hedge: PassiveHedgeConfig::default(),
        }
    }
}

impl MakerHedgeQuoterConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.e0_bps.is_finite() && self.e0_bps >= 0.0,
            "e0_bps must be finite and >= 0"
        );
        ensure!(
            self.skew_bps_per_clip.is_finite() && self.skew_bps_per_clip >= 0.0,
            "skew_bps_per_clip must be finite and >= 0"
        );
        ensure!(
            self.q_max_clips.is_finite() && self.q_max_clips > 0.0,
            "q_max_clips must be finite and > 0"
        );
        ensure!(
            self.lead_beta.is_finite() && (0.0..=1.5).contains(&self.lead_beta),
            "lead_beta must be in [0, 1.5]"
        );
        ensure!(
            self.tick_size.is_finite() && self.tick_size > 0.0,
            "tick_size must be finite and > 0"
        );
        ensure!(self.tau_ms > 0, "tau_ms must be > 0");
        ensure!(self.max_book_age_ms > 0, "max_book_age_ms must be > 0");
        ensure!(self.max_gap_ms > 0, "max_gap_ms must be > 0");
        ensure!(
            self.ref_shield.adverse_move_bps.is_finite() && self.ref_shield.adverse_move_bps > 0.0,
            "ref_shield.adverse_move_bps must be finite and > 0"
        );
        ensure!(
            self.ref_shield.lookback_ms > 0,
            "ref_shield.lookback_ms must be > 0"
        );
        ensure!(
            self.vol_edge_mult.is_finite() && self.vol_edge_mult >= 0.0,
            "vol_edge_mult must be finite and >= 0"
        );
        ensure!(self.basis_vol_tau_ms > 0, "basis_vol_tau_ms must be > 0");
        ensure!(
            self.nonlinear_skew_power.is_finite() && self.nonlinear_skew_power >= 1.0,
            "nonlinear_skew_power must be finite and >= 1.0"
        );
        ensure!(
            self.basis_drift_guard_bps.is_finite() && self.basis_drift_guard_bps >= 0.0,
            "basis_drift_guard_bps must be finite and >= 0"
        );
        ensure!(
            self.basis_drift_tau_ms > 0,
            "basis_drift_tau_ms must be > 0"
        );
        ensure!(
            self.min_hedge_l1_notional_usd.is_finite() && self.min_hedge_l1_notional_usd >= 0.0,
            "min_hedge_l1_notional_usd must be finite and >= 0"
        );
        ensure!(
            self.min_basis_zscore.is_finite() && self.min_basis_zscore >= 0.0,
            "min_basis_zscore must be finite and >= 0"
        );
        ensure!(
            self.basis_vol_floor_bps.is_finite() && self.basis_vol_floor_bps >= 0.0,
            "basis_vol_floor_bps must be finite and >= 0"
        );
        ensure!(
            self.max_drift_to_vol_ratio.is_finite() && self.max_drift_to_vol_ratio >= 0.0,
            "max_drift_to_vol_ratio must be finite and >= 0"
        );
        self.passive_hedge.validate()?;
        Ok(())
    }
}

/// Execution mode chosen by [`evaluate_passive_hedge`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum HedgeExecutionMode {
    /// Rest a passive `reduce_only` limit order at the hedge venue touch.
    RestPassive,
    /// Cross the hedge venue spread immediately with an IOC/taker order.
    CrossIoc,
    /// Fail closed: halt hedge dispatch on invalid, stale, or regressed hedge book.
    FailClosedHalt,
}

/// Configuration for [`evaluate_passive_hedge`] (default-off).
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PassiveHedgeConfig {
    /// Enable passive-first hedge resting before IOC escalation (default `false`).
    pub enabled: bool,
    /// Maximum milliseconds to rest passively at hedge touch before escalating to `CrossIoc` (`> 0`).
    pub max_passive_hedge_ms: u64,
    /// Immediate IOC escalation threshold on adverse hedge move from anchor, bps (`> 0`).
    pub hazard_ioc_adverse_bps: f64,
    /// Maximum allowed hedge book age before [`HedgeExecutionMode::FailClosedHalt`], ms (`> 0`).
    pub max_hedge_book_age_ms: u64,
    /// Hedge venue tick size (`> 0`).
    pub tick_size: f64,
    /// Improve passive hedge touch by one tick when spread > 1.5 ticks (`false` = join).
    pub improve_one_tick: bool,
    /// Enforce `reduce_only = true` on passive hedge orders (`true` by default).
    pub reduce_only: bool,
}

impl Default for PassiveHedgeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_passive_hedge_ms: 1_500,
            hazard_ioc_adverse_bps: 2.0,
            max_hedge_book_age_ms: 5_000,
            tick_size: 0.01,
            improve_one_tick: false,
            reduce_only: true,
        }
    }
}

impl PassiveHedgeConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.max_passive_hedge_ms > 0,
            "passive_hedge.max_passive_hedge_ms must be > 0"
        );
        ensure!(
            self.hazard_ioc_adverse_bps.is_finite() && self.hazard_ioc_adverse_bps > 0.0,
            "passive_hedge.hazard_ioc_adverse_bps must be finite and > 0"
        );
        ensure!(
            self.max_hedge_book_age_ms > 0,
            "passive_hedge.max_hedge_book_age_ms must be > 0"
        );
        ensure!(
            self.tick_size.is_finite() && self.tick_size > 0.0,
            "passive_hedge.tick_size must be finite and > 0"
        );
        Ok(())
    }
}

/// State of an active hedge leg evaluated by [`evaluate_passive_hedge`].
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
pub struct PassiveHedgeState {
    pub fill_side: FillSide,
    pub started_ms: u64,
    pub last_eval_ms: u64,
    pub last_hedge_recv_ms: u64,
    pub anchor_hedge_mid: f64,
    pub remaining_qty: f64,
    pub mode: HedgeExecutionMode,
}

impl PassiveHedgeState {
    /// Initialize a passive-first hedge state at `started_ms` from the hedge touch at fill time.
    pub fn new(
        fill_side: FillSide,
        started_ms: u64,
        initial_hedge: Touch,
        remaining_qty: f64,
    ) -> Result<Self> {
        ensure!(initial_hedge.valid(), "initial_hedge touch must be valid");
        ensure!(
            initial_hedge.recv_ms <= started_ms,
            "initial_hedge.recv_ms cannot be in the future"
        );
        ensure!(
            remaining_qty.is_finite() && remaining_qty > 0.0,
            "remaining_qty must be finite and > 0"
        );
        Ok(Self {
            fill_side,
            started_ms,
            last_eval_ms: started_ms,
            last_hedge_recv_ms: initial_hedge.recv_ms,
            anchor_hedge_mid: initial_hedge.mid(),
            remaining_qty,
            mode: HedgeExecutionMode::RestPassive,
        })
    }
}

/// Action returned by [`evaluate_passive_hedge`].
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
pub struct PassiveHedgeDecision {
    pub mode: HedgeExecutionMode,
    pub limit_px: Option<f64>,
    pub reduce_only: bool,
    pub adverse_move_bps: f64,
    pub elapsed_ms: u64,
}

/// Evaluate whether to rest a `reduce_only` passive hedge at hedge touch, escalate to `CrossIoc`
/// on adverse move (`>= hazard_ioc_adverse_bps`) or timeout (`>= max_passive_hedge_ms`), or
/// fail closed (`FailClosedHalt`) on invalid, stale, or timestamp-regressed hedge books.
pub fn evaluate_passive_hedge(
    cfg: &PassiveHedgeConfig,
    state: &mut PassiveHedgeState,
    now_ms: u64,
    hedge: Touch,
) -> PassiveHedgeDecision {
    let elapsed_ms = now_ms.saturating_sub(state.started_ms);
    let invalid_state = cfg.validate().is_err()
        || !state.anchor_hedge_mid.is_finite()
        || state.anchor_hedge_mid <= 0.0
        || !state.remaining_qty.is_finite()
        || state.remaining_qty <= 0.0;
    let bad_clock = now_ms < state.last_eval_ms || now_ms < state.started_ms;
    let bad_book = !hedge.valid()
        || hedge.recv_ms > now_ms
        || hedge.recv_ms < state.last_hedge_recv_ms
        || now_ms - hedge.recv_ms > cfg.max_hedge_book_age_ms;

    if state.mode == HedgeExecutionMode::FailClosedHalt || invalid_state || bad_clock || bad_book {
        state.mode = HedgeExecutionMode::FailClosedHalt;
        return PassiveHedgeDecision {
            mode: HedgeExecutionMode::FailClosedHalt,
            limit_px: None,
            reduce_only: cfg.reduce_only,
            adverse_move_bps: f64::NAN,
            elapsed_ms,
        };
    }

    state.last_eval_ms = now_ms;
    state.last_hedge_recv_ms = hedge.recv_ms;

    let mid = hedge.mid();
    let adverse_move_bps = match state.fill_side {
        // MakerBought -> short hedge (sell): adverse when hedge drops.
        FillSide::MakerBought => (1.0 - mid / state.anchor_hedge_mid) * 1e4,
        // MakerSold -> long hedge (buy): adverse when hedge rallies.
        FillSide::MakerSold => (mid / state.anchor_hedge_mid - 1.0) * 1e4,
    };

    let cross_px = match state.fill_side {
        FillSide::MakerBought => hedge.bid,
        FillSide::MakerSold => hedge.ask,
    };

    if !cfg.enabled || state.mode == HedgeExecutionMode::CrossIoc {
        state.mode = HedgeExecutionMode::CrossIoc;
        return PassiveHedgeDecision {
            mode: HedgeExecutionMode::CrossIoc,
            limit_px: Some(cross_px),
            reduce_only: cfg.reduce_only,
            adverse_move_bps,
            elapsed_ms,
        };
    }

    if elapsed_ms >= cfg.max_passive_hedge_ms || adverse_move_bps >= cfg.hazard_ioc_adverse_bps {
        state.mode = HedgeExecutionMode::CrossIoc;
        return PassiveHedgeDecision {
            mode: HedgeExecutionMode::CrossIoc,
            limit_px: Some(cross_px),
            reduce_only: cfg.reduce_only,
            adverse_move_bps,
            elapsed_ms,
        };
    }

    let tick = cfg.tick_size;
    let can_improve = cfg.improve_one_tick && (hedge.ask - hedge.bid) > 1.5 * tick;
    let passive_px = match state.fill_side {
        FillSide::MakerBought => {
            let px = if can_improve {
                ((hedge.ask - tick) / tick + TICK_EPS).floor() * tick
            } else {
                hedge.ask
            };
            if px > hedge.bid { px } else { hedge.ask }
        }
        FillSide::MakerSold => {
            let px = if can_improve {
                ((hedge.bid + tick) / tick - TICK_EPS).ceil() * tick
            } else {
                hedge.bid
            };
            if px < hedge.ask { px } else { hedge.bid }
        }
    };

    PassiveHedgeDecision {
        mode: HedgeExecutionMode::RestPassive,
        limit_px: Some(passive_px),
        reduce_only: cfg.reduce_only,
        adverse_move_bps,
        elapsed_ms,
    }
}

const REF_RING_CAP: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Default)]
struct RefPoint {
    ts_ms: u64,
    mid: f64,
}

/// Causal fair-basis tracker plus inventory-skewed maker quote derivation.
#[derive(Debug, Clone)]
pub struct MakerHedgeQuoter {
    cfg: MakerHedgeQuoterConfig,
    fair: Option<f64>,
    basis_var_bps2: Option<f64>,
    fast_fair: Option<f64>,
    first_ms: Option<u64>,
    last_ms: Option<u64>,
    ref_ring: [RefPoint; REF_RING_CAP],
    ref_head: usize,
    ref_len: usize,
}

const TICK_EPS: f64 = 1e-9;

impl MakerHedgeQuoter {
    pub fn new(cfg: MakerHedgeQuoterConfig) -> Result<Self> {
        cfg.validate()?;
        Ok(Self {
            cfg,
            fair: None,
            basis_var_bps2: None,
            fast_fair: None,
            first_ms: None,
            last_ms: None,
            ref_ring: [RefPoint::default(); REF_RING_CAP],
            ref_head: 0,
            ref_len: 0,
        })
    }

    pub fn config(&self) -> &MakerHedgeQuoterConfig {
        &self.cfg
    }

    /// Current causal basis volatility estimate (`sqrt(EWMA_var)` in bps).
    pub fn basis_sigma_bps(&self) -> f64 {
        self.basis_var_bps2
            .map(|v| v.max(0.0).sqrt())
            .unwrap_or(0.0)
    }

    /// Current effective minimum edge in bps (`e0_bps + vol_edge_mult * basis_sigma_bps`).
    pub fn effective_min_edge_bps(&self) -> f64 {
        if self.cfg.vol_edge_mult > 0.0 {
            self.cfg.e0_bps + self.cfg.vol_edge_mult * self.basis_sigma_bps()
        } else {
            self.cfg.e0_bps
        }
    }

    /// Record an external fast-reference mid price (zero heap allocations) for pre-fill
    /// cancel-shield protection on subsequent [`Self::step`] calls.
    pub fn observe_reference(&mut self, now_ms: u64, ref_mid: f64) {
        if !(ref_mid.is_finite() && ref_mid > 0.0) {
            return;
        }
        if self.ref_len > 0 {
            let last_idx = (self.ref_head + REF_RING_CAP - 1) % REF_RING_CAP;
            let last_ts = self.ref_ring[last_idx].ts_ms;
            if now_ms < last_ts {
                return;
            }
            if now_ms.saturating_sub(last_ts) > self.cfg.max_gap_ms {
                self.ref_head = 0;
                self.ref_len = 0;
            }
        }
        self.ref_ring[self.ref_head] = RefPoint {
            ts_ms: now_ms,
            mid: ref_mid,
        };
        self.ref_head = (self.ref_head + 1) % REF_RING_CAP;
        self.ref_len = (self.ref_len + 1).min(REF_RING_CAP);
    }

    fn reference_return_bps(&self, now_ms: u64) -> Option<f64> {
        if !self.cfg.ref_shield.enabled || self.ref_len < 2 {
            return None;
        }
        let newest_idx = (self.ref_head + REF_RING_CAP - 1) % REF_RING_CAP;
        let newest = self.ref_ring[newest_idx];
        // Reject future reference timestamps instead of saturating age to 0.
        if newest.ts_ms > now_ms || now_ms - newest.ts_ms > self.cfg.max_book_age_ms {
            return None;
        }
        let cutoff = now_ms.saturating_sub(self.cfg.ref_shield.lookback_ms);
        // Find the sample at or immediately before cutoff, else the oldest in window.
        let mut base = None;
        for offset in 0..self.ref_len {
            let idx = (self.ref_head + REF_RING_CAP - 1 - offset) % REF_RING_CAP;
            let pt = self.ref_ring[idx];
            if pt.ts_ms > now_ms || now_ms - pt.ts_ms > self.cfg.max_book_age_ms {
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

    /// Quote using the fair basis from strictly prior observations, then fold this
    /// observation into the fair basis. `inventory_clips` is signed (`+` = long maker leg).
    pub fn step(
        &mut self,
        now_ms: u64,
        maker: Touch,
        hedge: Touch,
        inventory_clips: f64,
    ) -> std::result::Result<MakerHedgeQuotes, QuoteBlock> {
        self.step_inner(now_ms, maker, None, hedge, None, inventory_clips)
    }

    /// Quote with explicit hedge BBO L1 sizes (`hedge_bid_sz`, `hedge_ask_sz`) so
    /// [`MakerHedgeQuoterConfig::min_hedge_l1_notional_usd`] can gate opening quotes against
    /// thin or empty hedge books.
    pub fn step_with_l1(
        &mut self,
        now_ms: u64,
        maker: Touch,
        hedge: Touch,
        hedge_bid_sz: f64,
        hedge_ask_sz: f64,
        inventory_clips: f64,
    ) -> std::result::Result<MakerHedgeQuotes, QuoteBlock> {
        self.step_inner(
            now_ms,
            maker,
            None,
            hedge,
            Some((hedge_bid_sz, hedge_ask_sz)),
            inventory_clips,
        )
    }

    /// Quote with explicit maker and hedge L1 sizes, anchoring the basis on L1 microprice
    /// when [`MakerHedgeQuoterConfig::use_microprice_anchor`] is `true` while also enforcing
    /// [`MakerHedgeQuoterConfig::min_hedge_l1_notional_usd`].
    #[allow(clippy::too_many_arguments)]
    pub fn step_with_l1_microprice(
        &mut self,
        now_ms: u64,
        maker: Touch,
        maker_bid_sz: f64,
        maker_ask_sz: f64,
        hedge: Touch,
        hedge_bid_sz: f64,
        hedge_ask_sz: f64,
        inventory_clips: f64,
    ) -> std::result::Result<MakerHedgeQuotes, QuoteBlock> {
        self.step_inner(
            now_ms,
            maker,
            Some((maker_bid_sz, maker_ask_sz)),
            hedge,
            Some((hedge_bid_sz, hedge_ask_sz)),
            inventory_clips,
        )
    }

    fn step_inner(
        &mut self,
        now_ms: u64,
        maker: Touch,
        maker_l1: Option<(f64, f64)>,
        hedge: Touch,
        hedge_l1: Option<(f64, f64)>,
        inventory_clips: f64,
    ) -> std::result::Result<MakerHedgeQuotes, QuoteBlock> {
        if !maker.valid() || !hedge.valid() || !inventory_clips.is_finite() {
            return Err(QuoteBlock::InvalidBook);
        }
        if maker.recv_ms > now_ms || hedge.recv_ms > now_ms {
            return Err(QuoteBlock::InvalidBook);
        }
        if let Some(last) = self.last_ms
            && now_ms < last
        {
            return Err(QuoteBlock::InvalidBook);
        }
        // Check book staleness before folding into `self.fair` or advancing `self.last_ms`,
        // so frozen/stale feeds never poison the fair-basis EMA or mask `max_gap_ms` re-seeding.
        if now_ms - maker.recv_ms > self.cfg.max_book_age_ms {
            return Err(QuoteBlock::StaleMaker);
        }
        if now_ms - hedge.recv_ms > self.cfg.max_book_age_ms {
            return Err(QuoteBlock::StaleHedge);
        }
        let (maker_anchor, hedge_anchor) = if self.cfg.use_microprice_anchor {
            let ma = match maker_l1 {
                Some((bs, as_)) => maker.microprice(bs, as_),
                None => maker.mid(),
            };
            let ha = match hedge_l1 {
                Some((bs, as_)) => hedge.microprice(bs, as_),
                None => hedge.mid(),
            };
            (ma, ha)
        } else {
            (maker.mid(), hedge.mid())
        };
        let b = maker_anchor.ln() - hedge_anchor.ln();
        let gap = self.last_ms.map(|l| now_ms - l);
        let (prior, prior_var, prior_fast) = match (self.fair, gap) {
            (Some(f), Some(g)) if g <= self.cfg.max_gap_ms => {
                (Some(f), self.basis_var_bps2, self.fast_fair)
            }
            _ => (None, None, None),
        };
        self.fair = Some(match (prior, gap) {
            (Some(f), Some(g)) => {
                let alpha = 1.0 - (-(g as f64) / self.cfg.tau_ms as f64).exp();
                f + alpha * (b - f)
            }
            _ => {
                self.first_ms = Some(now_ms);
                b
            }
        });
        match (prior, gap) {
            (Some(f), Some(g)) if g > 0 => {
                let dev_bps = (b - f) * 1e4;
                let alpha_vol = 1.0 - (-(g as f64) / self.cfg.basis_vol_tau_ms as f64).exp();
                self.basis_var_bps2 = Some(match prior_var {
                    Some(v) => (1.0 - alpha_vol) * v + alpha_vol * dev_bps * dev_bps,
                    None => dev_bps * dev_bps,
                });
                let alpha_drift = 1.0 - (-(g as f64) / self.cfg.basis_drift_tau_ms as f64).exp();
                self.fast_fair = Some(match prior_fast {
                    Some(ff) => ff + alpha_drift * (b - ff),
                    None => b,
                });
            }
            (Some(_), Some(_)) => {
                // Clock did not advance (`g == 0`): preserve variance and drift trackers untouched.
            }
            _ => {
                self.basis_var_bps2 = None;
                self.fast_fair = Some(b);
            }
        }
        self.last_ms = Some(now_ms);

        let Some(fair) = prior else {
            return Err(QuoteBlock::WarmingUp);
        };
        if now_ms - self.first_ms.unwrap_or(now_ms) < self.cfg.warmup_ms {
            return Err(QuoteBlock::WarmingUp);
        }
        Ok(self.derive_quotes(
            now_ms,
            maker,
            hedge,
            hedge_l1,
            b,
            fair,
            prior_var,
            prior_fast,
            inventory_clips,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn derive_quotes(
        &self,
        now_ms: u64,
        maker: Touch,
        hedge: Touch,
        hedge_l1: Option<(f64, f64)>,
        b: f64,
        fair: f64,
        prior_var_bps2: Option<f64>,
        prior_fast_fair: Option<f64>,
        q: f64,
    ) -> MakerHedgeQuotes {
        let c = &self.cfg;
        let tick = c.tick_size;
        let shift = (c.lead_beta * (b - fair)).exp();
        let (hb, ha) = (hedge.bid * shift, hedge.ask * shift);
        let can_improve = c.improve_one_tick && maker.ask - maker.bid > 1.5 * tick;

        let basis_sigma_bps = prior_var_bps2.map(|v| v.max(0.0).sqrt()).unwrap_or(0.0);
        let effective_e0 = if c.vol_edge_mult > 0.0 {
            c.e0_bps + c.vol_edge_mult * basis_sigma_bps
        } else {
            c.e0_bps
        };
        let skew_q = if c.nonlinear_skew_power > 1.0 && q != 0.0 {
            let u = (q.abs() / c.q_max_clips).min(1.0);
            q * (1.0 + (c.nonlinear_skew_power - 1.0) * u.powf(c.nonlinear_skew_power))
        } else {
            q
        };

        let (raw_block_bid, raw_block_ask) = if c.ref_shield.enabled {
            match self.reference_return_bps(now_ms) {
                Some(r) => (
                    r <= -c.ref_shield.adverse_move_bps,
                    r >= c.ref_shield.adverse_move_bps,
                ),
                None if c.ref_shield.fail_closed_when_unready => (true, true),
                None => (false, false),
            }
        } else {
            (false, false)
        };
        let (drift_block_bid, drift_block_ask) = if c.basis_drift_guard_bps > 0.0 {
            match prior_fast_fair {
                Some(ff) => {
                    let drift_bps = (b - ff) * 1e4;
                    (
                        q >= 0.0 && drift_bps <= -c.basis_drift_guard_bps,
                        q <= 0.0 && drift_bps >= c.basis_drift_guard_bps,
                    )
                }
                None => (false, false),
            }
        } else {
            (false, false)
        };
        let (l1_block_bid, l1_block_ask) = if c.min_hedge_l1_notional_usd > 0.0 {
            match hedge_l1 {
                Some((hb_sz, ha_sz)) => {
                    let bid_ok = hb_sz.is_finite()
                        && hb_sz > 0.0
                        && hedge.bid * hb_sz >= c.min_hedge_l1_notional_usd;
                    let ask_ok = ha_sz.is_finite()
                        && ha_sz > 0.0
                        && hedge.ask * ha_sz >= c.min_hedge_l1_notional_usd;
                    (q >= 0.0 && !bid_ok, q <= 0.0 && !ask_ok)
                }
                // Fail-closed on opening sides when L1 guard is enabled without L1 sizes.
                None => (q >= 0.0, q <= 0.0),
            }
        } else {
            (false, false)
        };

        // Selective Basis Z-Score & Stationarity Gate (opening sides only; never blocks reduce-only).
        let disloc_bps = (b - fair) * 1e4;
        let denom_bps = basis_sigma_bps.max(c.basis_vol_floor_bps);
        let z_ready = denom_bps.is_finite() && denom_bps > 1e-9;

        let nonstationary = if c.max_drift_to_vol_ratio > 0.0 {
            match (prior_fast_fair, z_ready) {
                (Some(ff), true) => {
                    let trend_bps = (ff - fair).abs() * 1e4;
                    (trend_bps / denom_bps) > c.max_drift_to_vol_ratio
                }
                _ => true,
            }
        } else {
            false
        };

        let (z_block_bid, z_block_ask) = if c.min_basis_zscore > 0.0 {
            if !z_ready {
                (q >= 0.0, q <= 0.0)
            } else {
                let z = disloc_bps / denom_bps;
                (
                    q >= 0.0 && z > -c.min_basis_zscore,
                    q <= 0.0 && z < c.min_basis_zscore,
                )
            }
        } else if c.quote_favorable_side_only {
            (q >= 0.0 && disloc_bps >= 0.0, q <= 0.0 && disloc_bps <= 0.0)
        } else {
            (false, false)
        };

        let stationarity_block_bid = nonstationary && q >= 0.0;
        let stationarity_block_ask = nonstationary && q <= 0.0;

        let block_bid = (raw_block_bid
            && !(c.ref_shield.allow_reduce_only_through_shield && q < 0.0))
            || drift_block_bid
            || l1_block_bid
            || z_block_bid
            || stationarity_block_bid;
        let block_ask = (raw_block_ask
            && !(c.ref_shield.allow_reduce_only_through_shield && q > 0.0))
            || drift_block_ask
            || l1_block_ask
            || z_block_ask
            || stationarity_block_ask;

        let mut bid = None;
        if q < c.q_max_clips && !block_bid {
            let e_bid = effective_e0 + c.skew_bps_per_clip * skew_q;
            let cap = hb * (fair - e_bid * 1e-4).exp();
            let join = if can_improve {
                maker.bid + tick
            } else {
                maker.bid
            };
            let px = join.min((cap / tick + TICK_EPS).floor() * tick);
            if px > 0.0 && px < maker.ask {
                bid = Some(px);
            }
        }
        let mut ask = None;
        if q > -c.q_max_clips && !block_ask {
            let e_ask = effective_e0 - c.skew_bps_per_clip * skew_q;
            let floor = ha * (fair + e_ask * 1e-4).exp();
            let join = if can_improve {
                maker.ask - tick
            } else {
                maker.ask
            };
            let px = join.max((floor / tick - TICK_EPS).ceil() * tick);
            if px > maker.bid {
                ask = Some(px);
            }
        }
        MakerHedgeQuotes {
            bid,
            ask,
            fair_basis_bps: fair * 1e4,
            dislocation_bps: (b - fair) * 1e4,
        }
    }
}

/// Entered basis edge versus fair for a completed maker fill + hedge, in bps.
/// Positive means the pair was entered on the favourable side of fair.
pub fn entered_edge_bps(
    side: FillSide,
    maker_px: f64,
    hedge_px: f64,
    fair_basis_bps: f64,
) -> Option<f64> {
    if !(maker_px.is_finite() && maker_px > 0.0 && hedge_px.is_finite() && hedge_px > 0.0)
        || !fair_basis_bps.is_finite()
    {
        return None;
    }
    let entered = (maker_px.ln() - hedge_px.ln()) * 1e4;
    Some(match side {
        FillSide::MakerBought => fair_basis_bps - entered,
        FillSide::MakerSold => entered - fair_basis_bps,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> MakerHedgeQuoterConfig {
        MakerHedgeQuoterConfig {
            e0_bps: 5.0,
            skew_bps_per_clip: 0.5,
            q_max_clips: 4.0,
            tau_ms: 60_000,
            lead_beta: 0.0,
            improve_one_tick: false,
            tick_size: 0.01,
            max_book_age_ms: 5_000,
            warmup_ms: 1_000,
            max_gap_ms: 60_000,
            ref_shield: RefCancelShieldConfig::default(),
            ..MakerHedgeQuoterConfig::default()
        }
    }

    fn t(bid: f64, ask: f64, ms: u64) -> Touch {
        Touch {
            bid,
            ask,
            recv_ms: ms,
        }
    }

    fn px(v: Option<f64>, want: f64) {
        let got = v.unwrap_or_else(|| panic!("expected a quote near {want}"));
        assert!((got - want).abs() < 1e-6, "got {got}, want {want}");
    }

    fn warm(q: &mut MakerHedgeQuoter, mk: (f64, f64), hd: (f64, f64)) -> u64 {
        let mut now = 10_000;
        for _ in 0..20 {
            let _ = q.step(now, t(mk.0, mk.1, now), t(hd.0, hd.1, now), 0.0);
            now += 100;
        }
        now
    }

    #[test]
    fn quotes_are_capped_by_hedge_touch_minus_edge_and_never_cross() {
        let mut q = MakerHedgeQuoter::new(cfg()).unwrap();
        // Maker and hedge identical -> fair basis 0.
        let now = warm(&mut q, (99.99, 100.01), (99.99, 100.01));
        let out = q
            .step(now, t(99.99, 100.01, now), t(99.99, 100.01, now), 0.0)
            .unwrap();
        assert!(out.fair_basis_bps.abs() < 1e-9);
        // bid cap = 99.99 * exp(-5bp) = 99.940 -> floor tick 99.94 (below maker best).
        px(out.bid, 99.94);
        // ask floor = 100.01 * exp(+5bp) = 100.06002 -> ceil tick 100.07.
        px(out.ask, 100.07);
        // With a cheap hedge-relative edge requirement of 0 we join the maker touch.
        let mut q0 = MakerHedgeQuoter::new(MakerHedgeQuoterConfig {
            e0_bps: 0.0,
            ..cfg()
        })
        .unwrap();
        let now = warm(&mut q0, (99.99, 100.01), (99.99, 100.01));
        let out = q0
            .step(now, t(99.99, 100.01, now), t(99.99, 100.01, now), 0.0)
            .unwrap();
        px(out.bid, 99.99);
        px(out.ask, 100.01);
    }

    #[test]
    fn inventory_skew_and_cap_withdraw_the_increasing_side() {
        let mut q = MakerHedgeQuoter::new(cfg()).unwrap();
        let now = warm(&mut q, (99.99, 100.01), (99.99, 100.01));
        let long = q
            .step(now, t(99.99, 100.01, now), t(99.99, 100.01, now), 2.0)
            .unwrap();
        // e_bid = 6 bp (deeper bid), e_ask = 4 bp (tighter ask) when long 2 clips.
        px(long.bid, 99.93);
        px(long.ask, 100.06);
        let capped = q
            .step(
                now + 100,
                t(99.99, 100.01, now + 100),
                t(99.99, 100.01, now + 100),
                4.0,
            )
            .unwrap();
        assert_eq!(capped.bid, None);
        assert!(capped.ask.is_some());
    }

    #[test]
    fn lead_beta_moves_quotes_with_the_expected_hedge_catch_up() {
        let mut base = MakerHedgeQuoter::new(MakerHedgeQuoterConfig {
            e0_bps: 1.0,
            ..cfg()
        })
        .unwrap();
        let mut lead = MakerHedgeQuoter::new(MakerHedgeQuoterConfig {
            e0_bps: 1.0,
            lead_beta: 1.0,
            ..cfg()
        })
        .unwrap();
        let now = warm(&mut base, (99.99, 100.01), (99.99, 100.01));
        let _ = warm(&mut lead, (99.99, 100.01), (99.99, 100.01));
        // Maker venue drops 10 bp, hedge has not moved yet.
        let mk = t(99.89, 99.91, now);
        let hd = t(99.99, 100.01, now);
        let b0 = base.step(now, mk, hd, 0.0).unwrap();
        let b1 = lead.step(now, mk, hd, 0.0).unwrap();
        assert!(b0.dislocation_bps < -9.0);
        // Without lead adjustment the stale hedge touch lets us join the maker bid
        // (the toxic fill); with beta = 1 the bid cap follows the expected hedge drop.
        px(b0.bid, 99.89);
        px(b1.bid, 99.88);
    }

    #[test]
    fn blocks_on_stale_books_warmup_and_invalid_input() {
        let mut q = MakerHedgeQuoter::new(cfg()).unwrap();
        assert_eq!(
            q.step(
                10_000,
                t(99.99, 100.01, 10_000),
                t(99.99, 100.01, 10_000),
                0.0
            ),
            Err(QuoteBlock::WarmingUp)
        );
        let now = warm(&mut q, (99.99, 100.01), (99.99, 100.01));
        assert_eq!(
            q.step(
                now,
                t(99.99, 100.01, now - 6_000),
                t(99.99, 100.01, now),
                0.0
            ),
            Err(QuoteBlock::StaleMaker)
        );
        assert_eq!(
            q.step(
                now + 100,
                t(99.99, 100.01, now + 100),
                t(99.99, 100.01, now - 6_000),
                0.0
            ),
            Err(QuoteBlock::StaleHedge)
        );
        assert_eq!(
            q.step(
                now + 200,
                t(100.01, 99.99, now + 200),
                t(99.99, 100.01, now + 200),
                0.0
            ),
            Err(QuoteBlock::InvalidBook)
        );
        assert!(
            MakerHedgeQuoter::new(MakerHedgeQuoterConfig {
                lead_beta: 2.0,
                ..cfg()
            })
            .is_err()
        );
    }

    #[test]
    fn entered_edge_sign_convention() {
        // Bought maker at 99.94 vs hedge sold at 99.99 with fair 0: ~ +5 bp favourable.
        let e = entered_edge_bps(FillSide::MakerBought, 99.94, 99.99, 0.0).unwrap();
        assert!((e - 5.0).abs() < 0.05, "{e}");
        let e = entered_edge_bps(FillSide::MakerSold, 100.06, 100.01, 0.0).unwrap();
        assert!((e - 5.0).abs() < 0.05, "{e}");
        assert!(entered_edge_bps(FillSide::MakerSold, 0.0, 1.0, 0.0).is_none());
    }

    #[test]
    fn ref_shield_withdraws_only_toxic_side_and_resets_after_stale_or_recovery() {
        let mut q = MakerHedgeQuoter::new(MakerHedgeQuoterConfig {
            e0_bps: 1.0,
            ref_shield: RefCancelShieldConfig {
                enabled: true,
                adverse_move_bps: 2.5,
                lookback_ms: 200,
                allow_reduce_only_through_shield: false,
                fail_closed_when_unready: false,
            },
            ..cfg()
        })
        .unwrap();
        let now = warm(&mut q, (99.99, 100.01), (99.99, 100.01));
        q.observe_reference(now - 150, 100.0);
        // External ref dumps -5 bps in 150 ms -> bid is toxic and withdrawn, ask stays live.
        q.observe_reference(now, 99.95);
        let out = q
            .step(now, t(99.99, 100.01, now), t(99.99, 100.01, now), 0.0)
            .unwrap();
        assert_eq!(out.bid, None);
        assert!(out.ask.is_some());

        // External ref rallies +5 bps over the lookback -> ask is toxic and withdrawn, bid live.
        let t2 = now + 500;
        q.observe_reference(t2 - 150, 99.95);
        q.observe_reference(t2, 100.00);
        let out = q
            .step(t2, t(99.99, 100.01, t2), t(99.99, 100.01, t2), 0.0)
            .unwrap();
        assert!(out.bid.is_some());
        assert_eq!(out.ask, None);

        // Once calm (no lookback move >= 2.5 bps) or after a gap, both sides resume.
        let t3 = t2 + 1_000;
        q.observe_reference(t3 - 150, 100.00);
        q.observe_reference(t3, 100.00);
        let out = q
            .step(t3, t(99.99, 100.01, t3), t(99.99, 100.01, t3), 0.0)
            .unwrap();
        assert!(out.bid.is_some() && out.ask.is_some());

        assert!(
            MakerHedgeQuoter::new(MakerHedgeQuoterConfig {
                ref_shield: RefCancelShieldConfig {
                    enabled: true,
                    adverse_move_bps: 0.0,
                    lookback_ms: 200,
                    allow_reduce_only_through_shield: false,
                    fail_closed_when_unready: false,
                },
                ..cfg()
            })
            .is_err()
        );
    }

    #[test]
    fn ref_shield_fail_closed_when_unready_blocks_until_two_fresh_valid_samples() {
        let shield_cfg = |allow_reduce_only: bool| MakerHedgeQuoterConfig {
            e0_bps: 1.0,
            ref_shield: RefCancelShieldConfig {
                enabled: true,
                adverse_move_bps: 2.5,
                lookback_ms: 200,
                allow_reduce_only_through_shield: allow_reduce_only,
                fail_closed_when_unready: true,
            },
            ..cfg()
        };

        // 0 samples (unwarmed shield) -> fail-closed on both sides.
        let mut q = MakerHedgeQuoter::new(shield_cfg(false)).unwrap();
        let now = warm(&mut q, (99.99, 100.01), (99.99, 100.01));
        let out = q
            .step(now, t(99.99, 100.01, now), t(99.99, 100.01, now), 0.0)
            .unwrap();
        assert_eq!(out.bid, None);
        assert_eq!(out.ask, None);

        // 1 sample -> still unready -> fail-closed.
        q.observe_reference(now, 100.0);
        let t1 = now + 50;
        let out = q
            .step(t1, t(99.99, 100.01, t1), t(99.99, 100.01, t1), 0.0)
            .unwrap();
        assert_eq!(out.bid, None);
        assert_eq!(out.ask, None);

        // 2 fresh valid samples -> recovery, both sides quote.
        q.observe_reference(t1, 100.0);
        let t2 = t1 + 50;
        let out = q
            .step(t2, t(99.99, 100.01, t2), t(99.99, 100.01, t2), 0.0)
            .unwrap();
        assert!(out.bid.is_some() && out.ask.is_some());

        // Stale reference (> max_book_age_ms = 5_000) -> fail-closed.
        let t_stale = t2 + 5_500;
        let out = q
            .step(
                t_stale,
                t(99.99, 100.01, t_stale),
                t(99.99, 100.01, t_stale),
                0.0,
            )
            .unwrap();
        assert_eq!(out.bid, None);
        assert_eq!(out.ask, None);

        // Future reference timestamp (ref_ts_ms > now_ms) -> rejected, fail-closed.
        let t_future = t_stale + 1_000;
        q.observe_reference(t_future + 500, 100.0);
        let out = q
            .step(
                t_future,
                t(99.99, 100.01, t_future),
                t(99.99, 100.01, t_future),
                0.0,
            )
            .unwrap();
        assert_eq!(out.bid, None);
        assert_eq!(out.ask, None);

        // Recovery once clock reaches and advances past the future sample with a second fresh one.
        let t_rec = t_future + 600;
        q.observe_reference(t_rec, 100.0);
        let out = q
            .step(t_rec, t(99.99, 100.01, t_rec), t(99.99, 100.01, t_rec), 0.0)
            .unwrap();
        assert!(out.bid.is_some() && out.ask.is_some());

        // Reduce-only escape when holding long or short inventory while unready.
        let mut q_ro = MakerHedgeQuoter::new(shield_cfg(true)).unwrap();
        let t_ro = warm(&mut q_ro, (99.99, 100.01), (99.99, 100.01));
        let long_out = q_ro
            .step(t_ro, t(99.99, 100.01, t_ro), t(99.99, 100.01, t_ro), 1.0)
            .unwrap();
        assert_eq!(long_out.bid, None);
        assert!(long_out.ask.is_some());

        let short_out = q_ro
            .step(
                t_ro + 50,
                t(99.99, 100.01, t_ro + 50),
                t(99.99, 100.01, t_ro + 50),
                -1.0,
            )
            .unwrap();
        assert!(short_out.bid.is_some());
        assert_eq!(short_out.ask, None);
    }

    #[test]
    fn ref_shield_fail_open_when_unready_preserves_legacy_behavior() {
        let mut q = MakerHedgeQuoter::new(MakerHedgeQuoterConfig {
            e0_bps: 1.0,
            ref_shield: RefCancelShieldConfig {
                enabled: true,
                adverse_move_bps: 2.5,
                lookback_ms: 200,
                allow_reduce_only_through_shield: false,
                fail_closed_when_unready: false,
            },
            ..cfg()
        })
        .unwrap();
        let now = warm(&mut q, (99.99, 100.01), (99.99, 100.01));
        // 0 samples: fail-open quotes both sides.
        let out = q
            .step(now, t(99.99, 100.01, now), t(99.99, 100.01, now), 0.0)
            .unwrap();
        assert!(out.bid.is_some() && out.ask.is_some());

        // 1 sample + future sample: still fail-open when fail_closed_when_unready = false,
        // while still rejecting the future reference in reference_return_bps.
        q.observe_reference(now + 500, 99.50);
        assert!(q.reference_return_bps(now + 100).is_none());
        let out = q
            .step(
                now + 100,
                t(99.99, 100.01, now + 100),
                t(99.99, 100.01, now + 100),
                0.0,
            )
            .unwrap();
        assert!(out.bid.is_some() && out.ask.is_some());
    }

    #[test]
    fn shielded_pair_economics_accounts_for_fees_slippage_stablecoin_drag_and_domain() {
        let same_coin = ShieldedPairEconomics {
            maker_fee_bps: 0.0,
            hedge_taker_fee_bps: 1.15,
            cross_stablecoin_drag_bps: 2.0,
            expected_hedge_slip_bps: 1.5,
            cross_stablecoin: false,
            same_risk_domain: false,
        };
        let net_same = same_coin.net_expected_edge_bps(4.0);
        assert!((net_same - 1.35).abs() < 1e-9, "got {net_same}");
        assert!(same_coin.is_economically_viable(4.0, 1.0));

        // Enabling cross_stablecoin charges cross_stablecoin_drag_bps and flips edge negative.
        let cross_coin = ShieldedPairEconomics {
            cross_stablecoin: true,
            ..same_coin
        };
        let net_cross = cross_coin.net_expected_edge_bps(4.0);
        assert!((net_cross + 0.65).abs() < 1e-9, "got {net_cross}");
        assert!(!cross_coin.is_economically_viable(4.0, 0.0));

        // Arcus level_0 2.25 bp hedge taker fee stress.
        let arcus_l0 = ShieldedPairEconomics {
            maker_fee_bps: 0.0,
            hedge_taker_fee_bps: 2.25,
            cross_stablecoin_drag_bps: 0.0,
            expected_hedge_slip_bps: 1.5,
            cross_stablecoin: false,
            same_risk_domain: false,
        };
        assert!((arcus_l0.net_expected_edge_bps(4.0) - 0.25).abs() < 1e-9);
        assert!(arcus_l0.is_economically_viable(4.0, 0.0));
        assert!(!arcus_l0.is_economically_viable(4.0, 0.5));
        assert!((arcus_l0.net_expected_edge_bps(3.0) + 0.75).abs() < 1e-9);
        assert!(!arcus_l0.is_economically_viable(3.0, 0.0));

        // Same risk domain is never economically viable as a cross-venue hedge pair.
        let same_domain = ShieldedPairEconomics {
            same_risk_domain: true,
            ..same_coin
        };
        assert!((same_domain.net_expected_edge_bps(10.0) - 7.35).abs() < 1e-9);
        assert!(!same_domain.is_economically_viable(10.0, 0.0));

        // Non-finite inputs fail closed.
        assert!(same_coin.net_expected_edge_bps(f64::NAN).is_nan());
        assert!(!same_coin.is_economically_viable(f64::INFINITY, 0.0));
        assert!(!same_coin.is_economically_viable(10.0, f64::NAN));
    }

    #[test]
    fn stale_hedge_does_not_pollute_fair_and_re_warms_after_long_outage() {
        let c = cfg();
        let mut q = MakerHedgeQuoter::new(c).unwrap();
        let now = warm(&mut q, (99.99, 100.01), (99.99, 100.01));
        let last_fresh = now - 100;

        // Short stale-hedge burst while maker ticks sharply higher (+200 bps):
        // must return StaleHedge without moving fair_basis_bps.
        let frozen_hedge = t(99.99, 100.01, last_fresh);
        for dt in [6_000, 8_000, 10_000] {
            let t_stale = last_fresh + dt;
            assert_eq!(
                q.step(t_stale, t(101.99, 102.01, t_stale), frozen_hedge, 0.0),
                Err(QuoteBlock::StaleHedge)
            );
        }

        // Hedge recovers within max_gap_ms (60_000 ms) at matching price: fair stays ~ 0 bps.
        let t_rec = last_fresh + 12_000;
        let out = q
            .step(
                t_rec,
                t(101.99, 102.01, t_rec),
                t(101.99, 102.01, t_rec),
                0.0,
            )
            .unwrap();
        assert!(
            out.fair_basis_bps.abs() < 1e-9,
            "fair basis was polluted during stale hedge: {}",
            out.fair_basis_bps
        );

        // Long stale-hedge outage exceeding max_gap_ms (60_000 ms):
        // stale ticks must not advance last_ms, so recovery re-seeds and re-warms.
        let frozen_long = t(101.99, 102.01, t_rec);
        for dt in [10_000, 35_000, c.max_gap_ms + 5_000] {
            let t_stale = t_rec + dt;
            assert_eq!(
                q.step(t_stale, t(103.99, 104.01, t_stale), frozen_long, 0.0),
                Err(QuoteBlock::StaleHedge)
            );
        }

        // Fresh books arrive after > max_gap_ms with a +100 bps basis shift:
        // must re-seed and return WarmingUp until warmup_ms elapses.
        let t_reseed = t_rec + c.max_gap_ms + 6_000;
        assert_eq!(
            q.step(
                t_reseed,
                t(100.99, 101.01, t_reseed),
                t(99.99, 100.01, t_reseed),
                0.0,
            ),
            Err(QuoteBlock::WarmingUp)
        );
        assert_eq!(
            q.step(
                t_reseed + c.warmup_ms - 100,
                t(100.99, 101.01, t_reseed + c.warmup_ms - 100),
                t(99.99, 100.01, t_reseed + c.warmup_ms - 100),
                0.0,
            ),
            Err(QuoteBlock::WarmingUp)
        );
        let t_ready = t_reseed + c.warmup_ms;
        let out_after = q
            .step(
                t_ready,
                t(100.99, 101.01, t_ready),
                t(99.99, 100.01, t_ready),
                0.0,
            )
            .unwrap();
        let expected_bps = (101.0_f64.ln() - 100.0_f64.ln()) * 1e4;
        assert!(
            (out_after.fair_basis_bps - expected_bps).abs() < 1e-6,
            "got {}, expected {expected_bps}",
            out_after.fair_basis_bps
        );
    }

    #[test]
    fn defaults_preserve_backward_compatible_opt_off_behavior() {
        let def = MakerHedgeQuoterConfig::default();
        assert_eq!(def.vol_edge_mult, 0.0);
        assert_eq!(def.nonlinear_skew_power, 1.0);
        assert_eq!(def.basis_drift_guard_bps, 0.0);
        assert_eq!(def.min_hedge_l1_notional_usd, 0.0);
        assert!(def.validate().is_ok());

        // Validation guards on new fields.
        assert!(
            MakerHedgeQuoterConfig {
                vol_edge_mult: -0.1,
                ..cfg()
            }
            .validate()
            .is_err()
        );
        assert!(
            MakerHedgeQuoterConfig {
                basis_vol_tau_ms: 0,
                ..cfg()
            }
            .validate()
            .is_err()
        );
        assert!(
            MakerHedgeQuoterConfig {
                nonlinear_skew_power: 0.5,
                ..cfg()
            }
            .validate()
            .is_err()
        );
        assert!(
            MakerHedgeQuoterConfig {
                basis_drift_guard_bps: -1.0,
                ..cfg()
            }
            .validate()
            .is_err()
        );
        assert!(
            MakerHedgeQuoterConfig {
                basis_drift_tau_ms: 0,
                ..cfg()
            }
            .validate()
            .is_err()
        );
        assert!(
            MakerHedgeQuoterConfig {
                min_hedge_l1_notional_usd: -10.0,
                ..cfg()
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn causal_basis_vol_scales_edge_and_ignores_stale_or_zero_dt() {
        let mut q = MakerHedgeQuoter::new(MakerHedgeQuoterConfig {
            e0_bps: 2.0,
            vol_edge_mult: 1.0,
            basis_vol_tau_ms: 1_000,
            ..cfg()
        })
        .unwrap();
        let mut now = warm(&mut q, (99.99, 100.01), (99.99, 100.01));
        assert!(q.basis_sigma_bps() < 1e-9);

        // Inject alternating +-10 bps basis moves on advancing fresh ticks.
        for i in 0..20 {
            let mk = if i % 2 == 0 {
                (100.09, 100.11)
            } else {
                (99.89, 99.91)
            };
            let _ = q.step(now, t(mk.0, mk.1, now), t(99.99, 100.01, now), 0.0);
            now += 100;
        }
        let sigma_after_vol = q.basis_sigma_bps();
        assert!(
            sigma_after_vol > 5.0,
            "expected basis_sigma_bps > 5.0, got {sigma_after_vol}"
        );
        assert!((q.effective_min_edge_bps() - (2.0 + sigma_after_vol)).abs() < 1e-9);

        // Quotes at neutral basis must reflect widened effective_min_edge_bps (> 7 bps from 100).
        let out = q
            .step(now, t(99.99, 100.01, now), t(99.99, 100.01, now), 0.0)
            .unwrap();
        assert!(out.bid.unwrap() <= 99.92, "bid should widen: {:?}", out.bid);
        assert!(
            out.ask.unwrap() >= 100.08,
            "ask should widen: {:?}",
            out.ask
        );

        // Same-timestamp (dt = 0) and stale-feed steps must NOT alter basis_sigma_bps.
        let sigma_before = q.basis_sigma_bps();
        let _ = q.step(now, t(105.0, 105.02, now), t(99.99, 100.01, now), 0.0);
        assert!(
            (q.basis_sigma_bps() - sigma_before).abs() < 1e-12,
            "dt=0 polluted sigma"
        );
        let stale_ts = now + 100;
        assert_eq!(
            q.step(
                stale_ts,
                t(105.0, 105.02, stale_ts - 6_000),
                t(99.99, 100.01, stale_ts),
                0.0
            ),
            Err(QuoteBlock::StaleMaker)
        );
        assert!(
            (q.basis_sigma_bps() - sigma_before).abs() < 1e-12,
            "stale maker polluted sigma"
        );
    }

    #[test]
    fn nonlinear_skew_steepens_near_cap_and_drift_guard_gates_opening_only() {
        let mut q_lin = MakerHedgeQuoter::new(MakerHedgeQuoterConfig {
            e0_bps: 4.0,
            skew_bps_per_clip: 1.0,
            nonlinear_skew_power: 1.0,
            ..cfg()
        })
        .unwrap();
        let mut q_nl = MakerHedgeQuoter::new(MakerHedgeQuoterConfig {
            e0_bps: 4.0,
            skew_bps_per_clip: 1.0,
            nonlinear_skew_power: 2.0,
            ..cfg()
        })
        .unwrap();
        let now = warm(&mut q_lin, (99.99, 100.01), (99.99, 100.01));
        let _ = warm(&mut q_nl, (99.99, 100.01), (99.99, 100.01));

        // Near cap (q = 3.5 / 4.0): nonlinear skew pushes opening bid deeper and reducing ask tighter.
        let lin = q_lin
            .step(now, t(99.99, 100.01, now), t(99.99, 100.01, now), 3.5)
            .unwrap();
        let nl = q_nl
            .step(now, t(99.99, 100.01, now), t(99.99, 100.01, now), 3.5)
            .unwrap();
        assert!(nl.bid.unwrap() < lin.bid.unwrap(), "nl={nl:?} lin={lin:?}");
        assert!(nl.ask.unwrap() <= lin.ask.unwrap(), "nl={nl:?} lin={lin:?}");

        // Adverse basis-drift guard suppresses opening leg only.
        let mut q_drift = MakerHedgeQuoter::new(MakerHedgeQuoterConfig {
            e0_bps: 1.0,
            basis_drift_guard_bps: 3.0,
            basis_drift_tau_ms: 5_000,
            ..cfg()
        })
        .unwrap();
        let t0 = warm(&mut q_drift, (99.99, 100.01), (99.99, 100.01));
        // Sharp -10 bps basis drop (maker dropped while hedge stayed at 100):
        // opening bid (q >= 0) is blocked; opening ask (q <= 0) stays active;
        // reducing bid (q < 0) stays active!
        let out_flat = q_drift
            .step(t0, t(99.89, 99.91, t0), t(99.99, 100.01, t0), 0.0)
            .unwrap();
        assert_eq!(out_flat.bid, None);
        assert!(out_flat.ask.is_some());

        let t1 = t0 + 100;
        let out_short = q_drift
            .step(t1, t(99.89, 99.91, t1), t(99.99, 100.01, t1), -1.0)
            .unwrap();
        assert!(
            out_short.bid.is_some(),
            "reduce-only bid should remain active"
        );
    }

    #[test]
    fn min_hedge_l1_notional_gates_thin_opening_books_but_allows_reduce() {
        let mut q = MakerHedgeQuoter::new(MakerHedgeQuoterConfig {
            e0_bps: 1.0,
            min_hedge_l1_notional_usd: 1_000.0,
            ..cfg()
        })
        .unwrap();
        let now = warm(&mut q, (99.99, 100.01), (99.99, 100.01));

        // Hedge bid thin ($500 < $1000), hedge ask deep ($2000 >= $1000) when flat (q = 0):
        // opening bid blocked (would sell into thin hedge bid), opening ask allowed.
        let thin_bid = q
            .step_with_l1(
                now,
                t(99.99, 100.01, now),
                t(99.99, 100.01, now),
                5.0,
                20.0,
                0.0,
            )
            .unwrap();
        assert_eq!(thin_bid.bid, None);
        assert!(thin_bid.ask.is_some());

        // When short (q = -1.0), bid is inventory-reducing so it stays active despite thin hedge bid.
        let reduce_bid = q
            .step_with_l1(
                now + 100,
                t(99.99, 100.01, now + 100),
                t(99.99, 100.01, now + 100),
                5.0,
                20.0,
                -1.0,
            )
            .unwrap();
        assert!(reduce_bid.bid.is_some());

        // Legacy step() without L1 sizes fails closed on opening sides when guard is enabled.
        let no_l1 = q
            .step(
                now + 200,
                t(99.99, 100.01, now + 200),
                t(99.99, 100.01, now + 200),
                0.0,
            )
            .unwrap();
        assert_eq!(no_l1.bid, None);
        assert_eq!(no_l1.ask, None);
    }

    #[test]
    fn selective_zscore_quotes_only_favorable_side_and_keeps_reduce_only_open() {
        let mut q = MakerHedgeQuoter::new(MakerHedgeQuoterConfig {
            e0_bps: 1.0,
            min_basis_zscore: 2.0,
            basis_vol_floor_bps: 2.0,
            basis_vol_tau_ms: 5_000,
            quote_favorable_side_only: true,
            ..cfg()
        })
        .unwrap();
        let now = warm(&mut q, (99.99, 100.01), (99.99, 100.01));

        // Near fair (disloc = 0, z = 0 < 2.0): when flat, both opening sides blocked.
        let flat = q
            .step(now, t(99.99, 100.01, now), t(99.99, 100.01, now), 0.0)
            .unwrap();
        assert_eq!(flat.bid, None);
        assert_eq!(flat.ask, None);

        // When holding long inventory (q = +1.0) at fair basis, reduce-only ask remains open.
        let unwind_long = q
            .step(
                now + 100,
                t(99.99, 100.01, now + 100),
                t(99.99, 100.01, now + 100),
                1.0,
            )
            .unwrap();
        assert_eq!(unwind_long.bid, None);
        assert!(unwind_long.ask.is_some());

        // Maker cheap by -10 bps (z = -5.0 <= -2.0): ONLY favorable opening bid quotes.
        let cheap = q
            .step(
                now + 200,
                t(99.89, 99.91, now + 200),
                t(99.99, 100.01, now + 200),
                0.0,
            )
            .unwrap();
        assert!(cheap.bid.is_some());
        assert_eq!(cheap.ask, None);

        // Maker rich by +10 bps (z >= +2.0): ONLY favorable opening ask quotes.
        let rich = q
            .step(
                now + 300,
                t(100.09, 100.11, now + 300),
                t(99.99, 100.01, now + 300),
                0.0,
            )
            .unwrap();
        assert_eq!(rich.bid, None);
        assert!(rich.ask.is_some());

        // quote_favorable_side_only alone (min_basis_zscore = 0.0) blocks unfavorable & zero disloc.
        let mut q_fav = MakerHedgeQuoter::new(MakerHedgeQuoterConfig {
            e0_bps: 1.0,
            quote_favorable_side_only: true,
            ..cfg()
        })
        .unwrap();
        let t_fav = warm(&mut q_fav, (99.99, 100.01), (99.99, 100.01));
        let zero_disloc = q_fav
            .step(t_fav, t(99.99, 100.01, t_fav), t(99.99, 100.01, t_fav), 0.0)
            .unwrap();
        assert_eq!(zero_disloc.bid, None);
        assert_eq!(zero_disloc.ask, None);
        let neg_disloc = q_fav
            .step(
                t_fav + 100,
                t(99.95, 99.97, t_fav + 100),
                t(99.99, 100.01, t_fav + 100),
                0.0,
            )
            .unwrap();
        assert!(neg_disloc.bid.is_some());
        assert_eq!(neg_disloc.ask, None);
    }

    #[test]
    fn stationarity_gate_blocks_nonstationary_trend_but_allows_reduce_only() {
        let mut q = MakerHedgeQuoter::new(MakerHedgeQuoterConfig {
            e0_bps: 1.0,
            tau_ms: 600_000,
            basis_drift_tau_ms: 1_000,
            basis_vol_tau_ms: 60_000,
            basis_vol_floor_bps: 2.0,
            max_drift_to_vol_ratio: 1.5,
            ..cfg()
        })
        .unwrap();
        let mut now = warm(&mut q, (99.99, 100.01), (99.99, 100.01));

        // Persistent +15 bps regime shift over 10 seconds drives |fast_fair - fair| / denom >> 1.5.
        for _ in 0..100 {
            let _ = q.step(now, t(100.14, 100.16, now), t(99.99, 100.01, now), 0.0);
            now += 100;
        }
        let blocked = q
            .step(now, t(100.14, 100.16, now), t(99.99, 100.01, now), 0.0)
            .unwrap();
        assert_eq!(blocked.bid, None);
        assert_eq!(blocked.ask, None);

        // Existing long inventory (q = +1.0) can still passively reduce via ask.
        let reduce = q
            .step(
                now + 100,
                t(100.14, 100.16, now + 100),
                t(99.99, 100.01, now + 100),
                1.0,
            )
            .unwrap();
        assert_eq!(reduce.bid, None);
        assert!(reduce.ask.is_some());
    }

    #[test]
    fn passive_hedge_fsm_rests_escalates_on_hazard_timeout_and_halts_on_stale() {
        let p_cfg = PassiveHedgeConfig {
            enabled: true,
            max_passive_hedge_ms: 1_000,
            hazard_ioc_adverse_bps: 2.0,
            max_hedge_book_age_ms: 500,
            tick_size: 0.01,
            improve_one_tick: true,
            reduce_only: true,
        };

        // 1. MakerBought -> sell hedge passively inside spread (spread 0.04 > 1.5 * 0.01 -> improve 1 tick).
        let mut st =
            PassiveHedgeState::new(FillSide::MakerBought, 10_000, t(99.98, 100.02, 10_000), 1.0)
                .unwrap();
        let d0 = evaluate_passive_hedge(&p_cfg, &mut st, 10_100, t(99.98, 100.02, 10_100));
        assert_eq!(d0.mode, HedgeExecutionMode::RestPassive);
        assert!(d0.reduce_only);
        assert!((d0.limit_px.unwrap() - 100.01).abs() < 1e-9);

        // 2. Adverse drop of 3 bps (>= 2.0 bps hazard) escalates immediately to CrossIoc at hedge.bid.
        let d_haz = evaluate_passive_hedge(&p_cfg, &mut st, 10_200, t(99.95, 99.99, 10_200));
        assert_eq!(d_haz.mode, HedgeExecutionMode::CrossIoc);
        assert!(d_haz.adverse_move_bps >= 2.0);
        assert!((d_haz.limit_px.unwrap() - 99.95).abs() < 1e-9);

        // 3. Timeout (>= 1_000 ms) escalates MakerSold -> CrossIoc buy at hedge.ask.
        // With 2-tick spread (0.02 > 1.5 * 0.01) and improve_one_tick=true, passive bid steps inside to 100.00.
        let mut st_to =
            PassiveHedgeState::new(FillSide::MakerSold, 20_000, t(99.99, 100.01, 20_000), 1.0)
                .unwrap();
        let d_wait = evaluate_passive_hedge(&p_cfg, &mut st_to, 20_500, t(99.99, 100.01, 20_500));
        assert_eq!(d_wait.mode, HedgeExecutionMode::RestPassive);
        assert!((d_wait.limit_px.unwrap() - 100.00).abs() < 1e-9);
        // On a 1-tick spread (0.01 <= 1.5 * 0.01), passive bid joins 99.99 without crossing.
        let d_1t = evaluate_passive_hedge(&p_cfg, &mut st_to, 20_600, t(99.99, 100.00, 20_600));
        assert_eq!(d_1t.mode, HedgeExecutionMode::RestPassive);
        assert!((d_1t.limit_px.unwrap() - 99.99).abs() < 1e-9);
        let d_to = evaluate_passive_hedge(&p_cfg, &mut st_to, 21_000, t(99.99, 100.01, 21_000));
        assert_eq!(d_to.mode, HedgeExecutionMode::CrossIoc);
        assert!((d_to.limit_px.unwrap() - 100.01).abs() < 1e-9);

        // 4. Stale hedge book (> 500 ms) triggers sticky FailClosedHalt.
        let mut st_stale =
            PassiveHedgeState::new(FillSide::MakerBought, 30_000, t(99.99, 100.01, 30_000), 1.0)
                .unwrap();
        let d_stale =
            evaluate_passive_hedge(&p_cfg, &mut st_stale, 30_600, t(99.99, 100.01, 30_000));
        assert_eq!(d_stale.mode, HedgeExecutionMode::FailClosedHalt);
        assert_eq!(d_stale.limit_px, None);
        // Recovery tick remains FailClosedHalt (fail-closed latch).
        let d_rec = evaluate_passive_hedge(&p_cfg, &mut st_stale, 30_700, t(99.99, 100.01, 30_700));
        assert_eq!(d_rec.mode, HedgeExecutionMode::FailClosedHalt);

        // 5. Regressed hedge recv_ms triggers FailClosedHalt.
        let mut st_reg =
            PassiveHedgeState::new(FillSide::MakerBought, 40_000, t(99.99, 100.01, 40_000), 1.0)
                .unwrap();
        let d_reg = evaluate_passive_hedge(&p_cfg, &mut st_reg, 40_100, t(99.99, 100.01, 39_900));
        assert_eq!(d_reg.mode, HedgeExecutionMode::FailClosedHalt);

        // 6. Disabled config falls back immediately to CrossIoc on valid fresh book.
        let mut st_off =
            PassiveHedgeState::new(FillSide::MakerBought, 50_000, t(99.99, 100.01, 50_000), 1.0)
                .unwrap();
        let d_off = evaluate_passive_hedge(
            &PassiveHedgeConfig::default(),
            &mut st_off,
            50_050,
            t(99.99, 100.01, 50_050),
        );
        assert_eq!(d_off.mode, HedgeExecutionMode::CrossIoc);
        assert!((d_off.limit_px.unwrap() - 99.99).abs() < 1e-9);
    }

    #[test]
    fn microprice_anchor_tilts_dislocation_and_falls_back_to_mid_when_off_or_empty() {
        let mk = t(99.90, 100.10, 20_000);
        let hd = t(99.90, 100.10, 20_000);
        // Balanced sizes -> microprice == mid == 100.0.
        assert!((mk.microprice(10.0, 10.0) - 100.0).abs() < 1e-9);
        // Heavy bid size (90 vs 10) -> microprice leans toward ask: (99.90*10 + 100.10*90)/100 = 100.08.
        assert!((mk.microprice(90.0, 10.0) - 100.08).abs() < 1e-9);
        // Missing/zero sizes fall back to mid.
        assert!((mk.microprice(0.0, 10.0) - 100.0).abs() < 1e-9);
        assert!((mk.microprice(f64::NAN, 10.0) - 100.0).abs() < 1e-9);

        let mut q_off = MakerHedgeQuoter::new(MakerHedgeQuoterConfig {
            e0_bps: 1.0,
            use_microprice_anchor: false,
            ..cfg()
        })
        .unwrap();
        let mut q_on = MakerHedgeQuoter::new(MakerHedgeQuoterConfig {
            e0_bps: 1.0,
            use_microprice_anchor: true,
            ..cfg()
        })
        .unwrap();
        let now_off = warm(&mut q_off, (99.90, 100.10), (99.90, 100.10));
        let now_on = warm(&mut q_on, (99.90, 100.10), (99.90, 100.10));

        let out_off = q_off
            .step_with_l1_microprice(
                now_off,
                t(99.90, 100.10, now_off),
                90.0,
                10.0,
                t(99.90, 100.10, now_off),
                10.0,
                10.0,
                0.0,
            )
            .unwrap();
        let out_on = q_on
            .step_with_l1_microprice(
                now_on,
                t(99.90, 100.10, now_on),
                90.0,
                10.0,
                t(99.90, 100.10, now_on),
                10.0,
                10.0,
                0.0,
            )
            .unwrap();
        assert!(out_off.dislocation_bps.abs() < 1e-9);
        // Microprice anchor reflects +8 bps maker bid pressure!
        assert!(out_on.dislocation_bps > 7.5 && out_on.dislocation_bps < 8.5);
        assert_eq!(hd.mid(), 100.0);
    }

    #[test]
    fn legacy_serde_defaults_preserve_opt_off_new_fields_and_validation() {
        let parsed: MakerHedgeQuoterConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed, MakerHedgeQuoterConfig::default());
        assert_eq!(parsed.min_basis_zscore, 0.0);
        assert_eq!(parsed.basis_vol_floor_bps, 0.0);
        assert_eq!(parsed.max_drift_to_vol_ratio, 0.0);
        assert!(!parsed.quote_favorable_side_only);
        assert!(!parsed.use_microprice_anchor);
        assert!(!parsed.passive_hedge.enabled);
        assert!(parsed.passive_hedge.reduce_only);
        assert!(parsed.validate().is_ok());

        for bad in [
            MakerHedgeQuoterConfig {
                min_basis_zscore: -0.1,
                ..cfg()
            },
            MakerHedgeQuoterConfig {
                basis_vol_floor_bps: -1.0,
                ..cfg()
            },
            MakerHedgeQuoterConfig {
                max_drift_to_vol_ratio: -0.5,
                ..cfg()
            },
            MakerHedgeQuoterConfig {
                passive_hedge: PassiveHedgeConfig {
                    max_passive_hedge_ms: 0,
                    ..PassiveHedgeConfig::default()
                },
                ..cfg()
            },
            MakerHedgeQuoterConfig {
                passive_hedge: PassiveHedgeConfig {
                    hazard_ioc_adverse_bps: 0.0,
                    ..PassiveHedgeConfig::default()
                },
                ..cfg()
            },
        ] {
            assert!(bad.validate().is_err());
        }
    }
}
