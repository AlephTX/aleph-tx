//! Venue-agnostic cross-venue basis arbitrage state machine (`S2 / S3` core).
//!
//! Contains ZERO dependencies on `crate::exchanges::*`. Operates strictly on
//! [`NormalizedLegBook`] / [`NormalizedBookSnapshot`], dual-clock freshness bounds,
//! exact shared-grid LCM quantization, causal persistence, Leung–Li entry caps,
//! optional [`KalmanBasisFilter`], reference-oracle entry gating, and surviving-leg
//! `reduce_only` ADL recovery ([`evaluate_adl_delta_breaker`]).

use crate::daily_loss::DailyLossGuard;
use crate::shm_book::NormalizedBookSnapshot;
use crate::strategy::kalman_basis::{
    KalmanBasisConfig, KalmanBasisFilter, KalmanBasisSnapshot, MakerEntrySide, SessionRegime,
};
use crate::strategy::tail_risk::{
    AdlDeltaAction, AdlDeltaBreakerConfig, NetDeltaSnapshot, PairedLeg, TailRiskAction,
    evaluate_adl_delta_breaker,
};
use crate::strategy::venue_guards::{
    MultiOraclePolicy, ReferenceOracleSample, evaluate_multi_source_oracle_median,
    evaluate_reference_oracle_divergence_bps,
};
use anyhow::{Context, Result, ensure};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::{Decimal, RoundingStrategy};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

fn gcd_u128(mut a: u128, mut b: u128) -> u128 {
    while b != 0 {
        let r = a % b;
        a = b;
        b = r;
    }
    a
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

/// Floor a positive quantity onto `step_size` (`Decimal`) cleanly.
pub fn quantize_step_qty(raw_qty: Decimal, step_size: Decimal) -> Result<Decimal> {
    ensure!(
        raw_qty > Decimal::ZERO && step_size > Decimal::ZERO,
        "raw_qty and step_size must be > 0"
    );
    let units = (raw_qty / step_size)
        .round_dp_with_strategy(0, RoundingStrategy::ToZero)
        .max(Decimal::ZERO);
    Ok((units * step_size).normalize())
}

/// Quantize `raw_qty` down onto the exact common LCM grid of two arbitrary decimal lot steps.
pub fn quantize_shared_step_qty(
    raw_qty: Decimal,
    primary_step: Decimal,
    hedge_step: Decimal,
) -> Result<Decimal> {
    ensure!(
        raw_qty > Decimal::ZERO && primary_step > Decimal::ZERO && hedge_step > Decimal::ZERO,
        "invalid quantization parameters"
    );
    let p_norm = primary_step.normalize();
    let h_norm = hedge_step.normalize();
    let scale = p_norm.scale().max(h_norm.scale());
    ensure!(scale <= 12, "step size precision exceeds 12 decimal places");
    let pow10 = 10_i128.checked_pow(scale).context("step scale overflow")?;
    let pow10_dec = Decimal::from_i128_with_scale(pow10, 0);

    let p_scaled = (p_norm * pow10_dec).normalize();
    ensure!(
        p_scaled.fract().is_zero() && p_scaled > Decimal::ZERO,
        "primary_step {primary_step} cannot be represented cleanly at scale {scale}"
    );
    let h_scaled = (h_norm * pow10_dec).normalize();
    ensure!(
        h_scaled.fract().is_zero() && h_scaled > Decimal::ZERO,
        "hedge_step {hedge_step} cannot be represented cleanly at scale {scale}"
    );

    let p_ticks = u128::try_from(p_scaled.mantissa()).context("primary step ticks overflow")?;
    let h_ticks = u128::try_from(h_scaled.mantissa()).context("hedge step ticks overflow")?;
    let g = gcd_u128(p_ticks, h_ticks);
    let lcm_ticks = (p_ticks / g)
        .checked_mul(h_ticks)
        .context("shared step LCM overflow")?;
    let lcm_i128 = i128::try_from(lcm_ticks).context("shared step exceeds Decimal range")?;
    let shared_step = Decimal::from_i128_with_scale(lcm_i128, scale).normalize();

    let units = (raw_qty / shared_step)
        .round_dp_with_strategy(0, RoundingStrategy::ToZero)
        .max(Decimal::ZERO);
    let q = (units * shared_step).normalize();
    if q > Decimal::ZERO {
        ensure!(
            (q % p_norm).is_zero(),
            "quantized qty {q} is not a multiple of primary_step {primary_step}"
        );
        ensure!(
            (q % h_norm).is_zero(),
            "quantized qty {q} is not a multiple of hedge_step {hedge_step}"
        );
    }
    Ok(q)
}

/// Venue-agnostic normalized book for one leg keeping source & local receipt clocks distinct.
#[derive(Debug, Clone, PartialEq)]
pub struct NormalizedLegBook {
    pub source_ms: u64,
    pub receipt_ms: u64,
    pub bid_px: Decimal,
    pub bid_sz: Decimal,
    pub ask_px: Decimal,
    pub ask_sz: Decimal,
    pub bids: Vec<(Decimal, Decimal)>,
    pub asks: Vec<(Decimal, Decimal)>,
}

impl NormalizedLegBook {
    pub fn from_l1(
        source_ms: u64,
        receipt_ms: u64,
        bid_px: Decimal,
        bid_sz: Decimal,
        ask_px: Decimal,
        ask_sz: Decimal,
    ) -> Result<Self> {
        ensure!(
            source_ms > 0
                && receipt_ms > 0
                && bid_px > Decimal::ZERO
                && ask_px > bid_px
                && bid_sz > Decimal::ZERO
                && ask_sz > Decimal::ZERO,
            "invalid NormalizedLegBook L1 snapshot"
        );
        Ok(Self {
            source_ms,
            receipt_ms,
            bid_px,
            bid_sz,
            ask_px,
            ask_sz,
            bids: vec![(bid_px, bid_sz)],
            asks: vec![(ask_px, ask_sz)],
        })
    }

    pub fn visible_size_within_limit(&self, is_buy: bool, limit_price: Decimal) -> Decimal {
        let levels = if is_buy { &self.asks } else { &self.bids };
        if levels.is_empty() {
            let touch = if is_buy { self.ask_px } else { self.bid_px };
            let within = if is_buy {
                touch <= limit_price
            } else {
                touch >= limit_price
            };
            return if within {
                if is_buy { self.ask_sz } else { self.bid_sz }
            } else {
                Decimal::ZERO
            };
        }
        let mut total = Decimal::ZERO;
        for &(px, sz) in levels {
            let ok = if is_buy {
                px <= limit_price
            } else {
                px >= limit_price
            };
            if !ok {
                break;
            }
            total += sz;
        }
        total
    }
}

impl TryFrom<&NormalizedBookSnapshot> for NormalizedLegBook {
    type Error = anyhow::Error;

    /// Fail closed unless an independent `feeder_receipt_ms` is present (never alias `source_ms`).
    fn try_from(snap: &NormalizedBookSnapshot) -> Result<Self> {
        let receipt_ms = snap
            .feeder_receipt_ms
            .context("NormalizedBookSnapshot missing independent feeder_receipt_ms")?;
        ensure!(
            snap.source_ms > 0
                && receipt_ms > 0
                && snap.bid_px > Decimal::ZERO
                && snap.ask_px > snap.bid_px
                && snap.bid_sz > Decimal::ZERO
                && snap.ask_sz > Decimal::ZERO,
            "invalid NormalizedBookSnapshot fields"
        );
        Ok(Self {
            source_ms: snap.source_ms,
            receipt_ms,
            bid_px: snap.bid_px,
            bid_sz: snap.bid_sz,
            ask_px: snap.ask_px,
            ask_sz: snap.ask_sz,
            bids: snap.bids.to_vec(),
            asks: snap.asks.to_vec(),
        })
    }
}

/// Residual imbalance / ADL recovery policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ResidualRecoveryPolicy {
    /// Classic partial-fill mode: neutralize net delta on the liquid hedge venue.
    PreferHedgeVenue,
    /// ADL-safe mode: honor [`evaluate_adl_delta_breaker`] and reduce whichever surviving
    /// leg maintains strict `reduce_only = true` (even if that means unwinding `Primary`).
    #[default]
    PreferReduceOnlySurvivingLeg,
}

/// Venue-agnostic per-pair lot and notional constraints.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BasisLegConstraints {
    pub primary_step_size: Decimal,
    pub primary_min_order_size: Decimal,
    pub primary_min_order_notional_usd: f64,
    pub hedge_step_size: Decimal,
    pub hedge_min_order_size: Decimal,
    pub hedge_min_order_notional_usd: f64,
}

impl Default for BasisLegConstraints {
    fn default() -> Self {
        Self {
            primary_step_size: Decimal::new(1, 4),
            primary_min_order_size: Decimal::new(1, 3),
            primary_min_order_notional_usd: 5.0,
            hedge_step_size: Decimal::new(1, 2),
            hedge_min_order_size: Decimal::new(1, 2),
            hedge_min_order_notional_usd: 10.0,
        }
    }
}

impl BasisLegConstraints {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.primary_step_size > Decimal::ZERO
                && self.primary_min_order_size >= self.primary_step_size
                && self.hedge_step_size > Decimal::ZERO
                && self.hedge_min_order_size >= self.hedge_step_size,
            "leg step sizes and minimum order sizes must be positive"
        );
        ensure!(
            self.primary_min_order_notional_usd.is_finite()
                && self.primary_min_order_notional_usd >= 0.0
                && self.hedge_min_order_notional_usd.is_finite()
                && self.hedge_min_order_notional_usd >= 0.0,
            "minimum order notionals must be finite and >= 0"
        );
        Ok(())
    }

    pub fn min_entry_notional_usd(&self) -> f64 {
        self.primary_min_order_notional_usd
            .max(self.hedge_min_order_notional_usd)
    }
}

fn default_require_reference_oracle_for_entry() -> bool {
    true
}

/// Cross-venue funding settlement clock snapshot passed into [`BasisCoreEngine::on_books_with_context`].
///
/// `primary_funding_bps_per_period` and `hedge_funding_bps_per_period` represent cashflows
/// settling specifically at `next_settlement_ms`; any leg whose next settlement is strictly
/// later than `next_settlement_ms` must be reported as `0.0` at this settlement point.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FundingClockSample {
    pub next_settlement_ms: u64,
    pub primary_funding_bps_per_period: f64,
    pub hedge_funding_bps_per_period: f64,
}

/// Funding settlement blackout & exit-threshold adjustment config. Default `enabled = false`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FundingSettlementConfig {
    pub enabled: bool,
    /// Block adverse-carry entries inside this window prior to next settlement (`ms`).
    pub pre_settlement_blackout_ms: u64,
    /// Adverse net carry magnitude (`bps`) that triggers the pre-settlement entry block (`>= 0`).
    pub adverse_funding_block_bps: f64,
    /// Weight applied to net funding carry (`hedge - primary` for +1 pair) when shifting `exit_bp`.
    pub funding_exit_weight: f64,
    /// Symmetric clamp on funding-induced `exit_bp` adjustment (`bps >= 0`).
    pub max_exit_bp_adjust: f64,
}

impl Default for FundingSettlementConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            pre_settlement_blackout_ms: 120_000,
            adverse_funding_block_bps: 0.5,
            funding_exit_weight: 0.5,
            max_exit_bp_adjust: 5.0,
        }
    }
}

impl FundingSettlementConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.adverse_funding_block_bps.is_finite() && self.adverse_funding_block_bps >= 0.0,
            "funding_settlement.adverse_funding_block_bps must be finite and >= 0"
        );
        ensure!(
            self.funding_exit_weight.is_finite() && self.funding_exit_weight >= 0.0,
            "funding_settlement.funding_exit_weight must be finite and >= 0"
        );
        ensure!(
            self.max_exit_bp_adjust.is_finite() && self.max_exit_bp_adjust >= 0.0,
            "funding_settlement.max_exit_bp_adjust must be finite and >= 0"
        );
        Ok(())
    }
}

/// Rolling hedge-leg RTT latency budget & adaptive entry slippage/clip config. Default `enabled = false`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LatencyBudgetConfig {
    pub enabled: bool,
    pub window_samples: usize,
    /// Target p95 RTT (`ms`) above which entry clip starts tapering.
    pub target_rtt_ms: f64,
    /// Hard p95 RTT budget (`ms`) above which new entries are paused (exits remain open).
    pub max_entry_rtt_ms: f64,
    /// Volatility drift multiplier for adaptive hedge-window slippage (`>= 0`).
    pub vol_drift_k: f64,
    /// Upper cap on effective entry slippage buffer (`bps`).
    pub max_slippage_bps: f64,
    /// Minimum entry clip multiplier at `p95_rtt >= max_entry_rtt_ms` (`(0, 1]`).
    pub min_clip_scale: f64,
}

impl Default for LatencyBudgetConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            window_samples: 32,
            target_rtt_ms: 50.0,
            max_entry_rtt_ms: 250.0,
            vol_drift_k: 1.5,
            max_slippage_bps: 25.0,
            min_clip_scale: 0.25,
        }
    }
}

impl LatencyBudgetConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.window_samples >= 1 && self.window_samples <= 1024,
            "latency_budget.window_samples must be in [1, 1024]"
        );
        ensure!(
            self.target_rtt_ms.is_finite()
                && self.max_entry_rtt_ms.is_finite()
                && self.target_rtt_ms > 0.0
                && self.max_entry_rtt_ms >= self.target_rtt_ms,
            "latency_budget requires 0 < target_rtt_ms <= max_entry_rtt_ms"
        );
        ensure!(
            self.vol_drift_k.is_finite() && self.vol_drift_k >= 0.0,
            "latency_budget.vol_drift_k must be finite and >= 0"
        );
        ensure!(
            self.max_slippage_bps.is_finite()
                && self.max_slippage_bps > 0.0
                && self.max_slippage_bps <= 100.0,
            "latency_budget.max_slippage_bps must be in (0, 100]"
        );
        ensure!(
            self.min_clip_scale.is_finite()
                && self.min_clip_scale > 0.0
                && self.min_clip_scale <= 1.0,
            "latency_budget.min_clip_scale must be finite and in (0, 1]"
        );
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct RollingRttTracker {
    cap: usize,
    samples: VecDeque<f64>,
}

impl RollingRttTracker {
    fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            samples: VecDeque::with_capacity(cap.min(256)),
        }
    }

    fn push(&mut self, rtt_ms: f64) {
        if !rtt_ms.is_finite() || rtt_ms <= 0.0 {
            return;
        }
        if self.samples.len() == self.cap {
            self.samples.pop_front();
        }
        self.samples.push_back(rtt_ms);
    }

    fn p95_ms(&self) -> Option<f64> {
        if self.samples.is_empty() {
            return None;
        }
        let mut sorted: Vec<f64> = self.samples.iter().copied().collect();
        sorted.sort_by(f64::total_cmp);
        let idx = ((sorted.len() * 95).saturating_sub(1)) / 100;
        sorted.get(idx).copied()
    }
}

/// Optional real-time funding, multi-source oracle, and latency context for [`BasisCoreEngine`].
#[derive(Debug, Clone, Copy, Default)]
pub struct BasisMarketContext<'a> {
    pub funding_clock: Option<FundingClockSample>,
    pub reference_oracles: Option<&'a [ReferenceOracleSample]>,
    pub hedge_rtt_ms: Option<f64>,
    pub realized_vol_bps_per_sqrt_s: Option<f64>,
}

/// Venue-agnostic cross-venue basis arbitrage configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BasisCoreConfig {
    pub primary_venue_id: String,
    pub hedge_venue_id: String,
    pub entry_bp: f64,
    pub exit_bp: f64,
    pub max_entry_bp: f64,
    pub persist_ms: u64,
    pub post_trade_refresh_lock: bool,
    pub clip_usd: f64,
    pub max_notional_usd: f64,
    pub max_order_usd: f64,
    pub max_total_notional_usd: f64,
    pub ladder_floor_frac: f64,
    pub ladder_scale_bps: f64,
    pub ladder_convex_power: f64,
    pub max_hold_ms: u64,
    pub max_primary_book_age_ms: u64,
    pub max_hedge_book_age_ms: u64,
    pub max_venue_clock_skew_ms: u64,
    pub slippage_bps: f64,
    pub max_oracle_divergence_bps: f64,
    #[serde(default = "default_require_reference_oracle_for_entry")]
    pub require_reference_oracle_for_entry: bool,
    #[serde(default)]
    pub residual_policy: ResidualRecoveryPolicy,
    #[serde(default)]
    pub adl_delta_breaker: AdlDeltaBreakerConfig,
    #[serde(default)]
    pub kalman_basis: Option<KalmanBasisConfig>,
    #[serde(default)]
    pub funding_settlement: FundingSettlementConfig,
    #[serde(default)]
    pub multi_oracle: Option<MultiOraclePolicy>,
    #[serde(default)]
    pub latency_budget: LatencyBudgetConfig,
    /// Optional entry hurdle surcharge proportional to simultaneous top-of-book spread sum (`>= 0.0`).
    #[serde(default)]
    pub spread_cost_entry_coeff: f64,
}

impl BasisCoreConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.primary_venue_id.trim().is_empty()
                && !self.hedge_venue_id.trim().is_empty()
                && self.primary_venue_id != self.hedge_venue_id,
            "primary_venue_id and hedge_venue_id must be distinct non-empty strings"
        );
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
                && self.max_order_usd.is_finite()
                && self.max_total_notional_usd.is_finite()
                && self.clip_usd > 0.0
                && self.max_order_usd > 0.0
                && self.max_notional_usd >= self.clip_usd
                && self.max_total_notional_usd >= self.max_order_usd.min(self.clip_usd),
            "invalid basis clip or notional bounds"
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
                && self.max_primary_book_age_ms > 0
                && self.max_hedge_book_age_ms > 0
                && self.max_venue_clock_skew_ms <= 30_000,
            "invalid hold, freshness, or clock-skew timing bounds"
        );
        ensure!(
            self.slippage_bps.is_finite() && self.slippage_bps > 0.0 && self.slippage_bps <= 100.0,
            "slippage_bps must be in (0, 100]"
        );
        ensure!(
            self.max_oracle_divergence_bps.is_finite() && self.max_oracle_divergence_bps > 0.0,
            "max_oracle_divergence_bps must be finite and > 0"
        );
        self.adl_delta_breaker.validate()?;
        if let Some(kalman) = &self.kalman_basis {
            kalman.validate()?;
        }
        self.funding_settlement.validate()?;
        if let Some(multi) = &self.multi_oracle {
            multi.validate()?;
        }
        self.latency_budget.validate()?;
        if self.latency_budget.enabled {
            ensure!(
                self.latency_budget.max_slippage_bps >= self.slippage_bps,
                "latency_budget.max_slippage_bps ({}) must be >= slippage_bps ({})",
                self.latency_budget.max_slippage_bps,
                self.slippage_bps
            );
        }
        ensure!(
            self.spread_cost_entry_coeff.is_finite()
                && self.spread_cost_entry_coeff >= 0.0
                && self.spread_cost_entry_coeff <= 5.0,
            "spread_cost_entry_coeff must be finite and in [0.0, 5.0]"
        );
        Ok(())
    }

    pub fn book_problem(
        &self,
        now_ms: u64,
        primary: &NormalizedLegBook,
        hedge: &NormalizedLegBook,
    ) -> Option<&'static str> {
        if now_ms == 0 {
            return Some("invalid_eval_time");
        }
        if primary.bid_px <= Decimal::ZERO
            || primary.ask_px <= primary.bid_px
            || primary.bid_sz <= Decimal::ZERO
            || primary.ask_sz <= Decimal::ZERO
        {
            return Some("invalid_primary_book");
        }
        if hedge.bid_px <= Decimal::ZERO
            || hedge.ask_px <= hedge.bid_px
            || hedge.bid_sz <= Decimal::ZERO
            || hedge.ask_sz <= Decimal::ZERO
        {
            return Some("invalid_hedge_book");
        }
        if primary.receipt_ms == 0 || primary.receipt_ms > now_ms {
            return Some("invalid_primary_receipt_time");
        }
        if now_ms.saturating_sub(primary.receipt_ms) > self.max_primary_book_age_ms {
            return Some("stale_primary_book");
        }
        if hedge.receipt_ms == 0 || hedge.receipt_ms > now_ms {
            return Some("invalid_hedge_receipt_time");
        }
        if now_ms.saturating_sub(hedge.receipt_ms) > self.max_hedge_book_age_ms {
            return Some("stale_hedge_book");
        }
        let skew = self.max_venue_clock_skew_ms;
        if primary.source_ms == 0 || primary.source_ms > now_ms.saturating_add(skew) {
            return Some("invalid_primary_source_time");
        }
        if now_ms.saturating_sub(primary.source_ms)
            > self.max_primary_book_age_ms.saturating_add(skew)
        {
            return Some("stale_primary_source_book");
        }
        if hedge.source_ms == 0 || hedge.source_ms > now_ms.saturating_add(skew) {
            return Some("invalid_hedge_source_time");
        }
        if now_ms.saturating_sub(hedge.source_ms) > self.max_hedge_book_age_ms.saturating_add(skew)
        {
            return Some("stale_hedge_source_book");
        }
        None
    }

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
        // Reduce-only risk unwinds are gated by `max_order_usd` rather than throttled by
        // the smaller `clip_usd` entry tranche size.
        let order_cap_usd = if reduce_only {
            self.max_order_usd
        } else {
            self.max_order_usd.min(self.clip_usd)
        };
        let order = decimal_from_f64(order_cap_usd).unwrap_or_default() / price;
        let along = if buy { position } else { -position };
        let room = if reduce_only {
            -along
        } else {
            decimal_from_f64(self.max_notional_usd.min(self.max_total_notional_usd))
                .unwrap_or_default()
                / price
                - along
        };
        order.min(room).max(Decimal::ZERO)
    }
}

/// Guarded action emitted by [`BasisCoreEngine::on_books`].
#[derive(Debug, Clone, PartialEq)]
pub enum BasisCoreAction {
    Hold {
        reason: &'static str,
    },
    HaltAndFlatten {
        net_delta_base: f64,
        elapsed_ms: u64,
        reason: String,
    },
    EmergencyReducePrimary {
        is_buy: bool,
        qty: Decimal,
        limit_price: Decimal,
        reduce_only: bool,
    },
    EmergencyReduceHedge {
        is_buy: bool,
        qty: Decimal,
        limit_price: Decimal,
        reduce_only: bool,
    },
    ExitPairIoc {
        direction_primary_buy: bool,
        qty: Decimal,
        primary_limit_px: Decimal,
        hedge_limit_px: Decimal,
        timed_out: bool,
    },
    EnterPairIoc {
        direction_primary_buy: bool,
        qty: Decimal,
        primary_limit_px: Decimal,
        hedge_limit_px: Decimal,
        raw_edge_bp: f64,
        req_edge_bp: f64,
        brake_scale: f64,
    },
}

#[derive(Debug, Default, Clone)]
pub struct BasisCoreCounters {
    pub entries_emitted: u64,
    pub exits_emitted: u64,
    pub primary_residual_hedges: u64,
    pub hedge_residual_hedges: u64,
    pub adl_halts_emitted: u64,
    pub oracle_blocked_entries: u64,
    pub leung_li_blocked: u64,
    pub persist_reset_count: u64,
}

/// Venue-agnostic paired-venue basis engine.
#[derive(Debug)]
pub struct BasisCoreEngine {
    config: BasisCoreConfig,
    constraints: BasisLegConstraints,
    inv_primary: Decimal,
    inv_hedge: Decimal,
    in_flight: bool,
    entry_open_ms: Option<u64>,
    unplanned_residual_since_ms: Option<u64>,
    last_trade_primary_rx_ms: u64,
    last_trade_hedge_rx_ms: u64,
    armed_dir: Option<i8>,
    armed_since_ms: u64,
    dust_qty: Decimal,
    kalman: Option<KalmanBasisFilter>,
    last_kalman_snapshot: Option<KalmanBasisSnapshot>,
    rtt_tracker: RollingRttTracker,
    pub counters: BasisCoreCounters,
}

impl BasisCoreEngine {
    pub fn new(config: BasisCoreConfig, constraints: BasisLegConstraints) -> Result<Self> {
        config.validate()?;
        constraints.validate()?;
        let rtt_cap = config.latency_budget.window_samples;
        let kalman = config
            .kalman_basis
            .map(KalmanBasisFilter::new)
            .transpose()?;
        Ok(Self {
            config,
            constraints,
            inv_primary: Decimal::ZERO,
            inv_hedge: Decimal::ZERO,
            in_flight: false,
            entry_open_ms: None,
            unplanned_residual_since_ms: None,
            last_trade_primary_rx_ms: 0,
            last_trade_hedge_rx_ms: 0,
            armed_dir: None,
            armed_since_ms: 0,
            dust_qty: constraints
                .primary_step_size
                .min(constraints.hedge_step_size)
                * Decimal::new(5, 1),
            kalman,
            last_kalman_snapshot: None,
            rtt_tracker: RollingRttTracker::new(rtt_cap),
            counters: BasisCoreCounters::default(),
        })
    }

    pub fn inv_primary(&self) -> Decimal {
        self.inv_primary
    }

    pub fn inv_hedge(&self) -> Decimal {
        self.inv_hedge
    }

    pub fn in_flight(&self) -> bool {
        self.in_flight
    }

    pub fn last_kalman_snapshot(&self) -> Option<KalmanBasisSnapshot> {
        self.last_kalman_snapshot
    }

    /// Reconcile authoritative positions from both venues after dispatch or on startup.
    pub fn reconcile_positions(
        &mut self,
        inv_primary: Decimal,
        inv_hedge: Decimal,
        primary_rx_ms: u64,
        hedge_rx_ms: u64,
        now_ms: u64,
    ) {
        self.inv_primary = inv_primary;
        self.inv_hedge = inv_hedge;
        self.in_flight = false;
        let residual = (self.inv_primary + self.inv_hedge).abs();
        if residual <= self.dust_qty {
            self.unplanned_residual_since_ms = None;
        } else if self.unplanned_residual_since_ms.is_none() {
            self.unplanned_residual_since_ms = Some(now_ms);
        }
        self.last_trade_primary_rx_ms = self.last_trade_primary_rx_ms.max(primary_rx_ms);
        self.last_trade_hedge_rx_ms = self.last_trade_hedge_rx_ms.max(hedge_rx_ms);
        if self.inv_primary.abs() <= self.dust_qty && self.inv_hedge.abs() <= self.dust_qty {
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

    fn dual_refresh_ok(&self, primary_rx_ms: u64, hedge_rx_ms: u64) -> bool {
        if !self.config.post_trade_refresh_lock
            || (self.last_trade_primary_rx_ms == 0 && self.last_trade_hedge_rx_ms == 0)
        {
            return true;
        }
        primary_rx_ms > self.last_trade_primary_rx_ms && hedge_rx_ms > self.last_trade_hedge_rx_ms
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

    fn build_single_leg_residual(
        &self,
        leg: PairedLeg,
        book: &NormalizedLegBook,
        target_delta: Decimal,
        require_reduce_only: bool,
    ) -> Option<(bool, Decimal, Decimal, bool)> {
        let (leg_pos, step_size, min_order_size, min_order_notional_usd) = match leg {
            PairedLeg::Primary => (
                self.inv_primary,
                self.constraints.primary_step_size,
                self.constraints.primary_min_order_size,
                self.constraints.primary_min_order_notional_usd,
            ),
            PairedLeg::Hedge => (
                self.inv_hedge,
                self.constraints.hedge_step_size,
                self.constraints.hedge_min_order_size,
                self.constraints.hedge_min_order_notional_usd,
            ),
        };
        let is_buy = target_delta < Decimal::ZERO;
        let reducible_qty = if is_buy {
            (-leg_pos).max(Decimal::ZERO)
        } else {
            leg_pos.max(Decimal::ZERO)
        };
        let eff_abs_delta = if require_reduce_only {
            target_delta.abs().min(reducible_qty)
        } else {
            target_delta.abs()
        };
        if eff_abs_delta <= Decimal::ZERO {
            return None;
        }
        let touch = if is_buy { book.ask_px } else { book.bid_px };
        let limit_price = protective_limit(touch, is_buy, self.config.slippage_bps)?;
        let cons_px = touch.min(limit_price);
        let q_target = quantize_step_qty(eff_abs_delta, step_size).ok()?;
        if q_target <= Decimal::ZERO || q_target < min_order_size {
            return None;
        }
        let reduce_only_full = (is_buy && leg_pos < Decimal::ZERO && leg_pos.abs() >= q_target)
            || (!is_buy && leg_pos > Decimal::ZERO && leg_pos >= q_target);
        if require_reduce_only && !reduce_only_full {
            return None;
        }
        let target_notional = (q_target * cons_px).to_f64().unwrap_or(0.0);
        if !reduce_only_full && target_notional < min_order_notional_usd {
            return None;
        }

        let visible = book.visible_size_within_limit(is_buy, limit_price);
        let raw_qty = eff_abs_delta.min(visible).min(self.config.leg_capacity(
            touch.max(limit_price),
            leg_pos,
            is_buy,
            reduce_only_full,
        ));
        if raw_qty <= Decimal::ZERO {
            return None;
        }
        let qty = quantize_step_qty(raw_qty, step_size).ok()?;
        let reduce_only = (is_buy && leg_pos < Decimal::ZERO && leg_pos.abs() >= qty)
            || (!is_buy && leg_pos > Decimal::ZERO && leg_pos >= qty);
        if require_reduce_only && !reduce_only {
            return None;
        }
        let visible_notional = (qty * cons_px).to_f64().unwrap_or(0.0);
        if qty < min_order_size || (!reduce_only && visible_notional < min_order_notional_usd) {
            return None;
        }
        Some((is_buy, qty, limit_price, reduce_only))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn on_books(
        &mut self,
        now_ms: u64,
        primary: &NormalizedLegBook,
        hedge: &NormalizedLegBook,
        daily_loss: &DailyLossGuard,
        primary_lease_ok: bool,
        reference_oracle_px: Option<Decimal>,
        regime: SessionRegime,
    ) -> BasisCoreAction {
        self.on_books_with_context(
            now_ms,
            primary,
            hedge,
            daily_loss,
            primary_lease_ok,
            reference_oracle_px,
            regime,
            BasisMarketContext::default(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn on_books_with_context(
        &mut self,
        now_ms: u64,
        primary: &NormalizedLegBook,
        hedge: &NormalizedLegBook,
        daily_loss: &DailyLossGuard,
        primary_lease_ok: bool,
        reference_oracle_px: Option<Decimal>,
        regime: SessionRegime,
        ctx: BasisMarketContext<'_>,
    ) -> BasisCoreAction {
        if self.in_flight {
            return BasisCoreAction::Hold {
                reason: "in_flight",
            };
        }
        if let Some(reason) = self.config.book_problem(now_ms, primary, hedge) {
            self.reset_persist();
            return BasisCoreAction::Hold { reason };
        }
        if self.config.latency_budget.enabled
            && let Some(rtt_ms) = ctx.hedge_rtt_ms
        {
            self.rtt_tracker.push(rtt_ms);
        }

        // Priority 0: Handle unplanned residual / ADL imbalance on either leg.
        let net_residual = self.inv_primary + self.inv_hedge;
        if net_residual.abs() > self.dust_qty {
            let strict_reduce =
                self.config.residual_policy == ResidualRecoveryPolicy::PreferReduceOnlySurvivingLeg;
            let adl_snap = NetDeltaSnapshot {
                now_ms,
                unplanned_since_ms: self.unplanned_residual_since_ms,
                target_leg_base: self.inv_primary.to_f64().unwrap_or(f64::NAN),
                hedge_leg_base: self.inv_hedge.to_f64().unwrap_or(f64::NAN),
                reference_gross_base: self
                    .inv_primary
                    .abs()
                    .max(self.inv_hedge.abs())
                    .to_f64()
                    .unwrap_or(f64::NAN),
            };
            let adl_eval = evaluate_adl_delta_breaker(&self.config.adl_delta_breaker, &adl_snap);
            match adl_eval {
                Err(err) => {
                    self.reset_persist();
                    self.counters.adl_halts_emitted += 1;
                    return BasisCoreAction::HaltAndFlatten {
                        net_delta_base: net_residual.to_f64().unwrap_or(0.0),
                        elapsed_ms: 0,
                        reason: err.to_string(),
                    };
                }
                Ok(AdlDeltaAction::HaltAndFlatten {
                    net_delta_base,
                    elapsed_ms,
                    reason,
                }) => {
                    self.reset_persist();
                    self.counters.adl_halts_emitted += 1;
                    return BasisCoreAction::HaltAndFlatten {
                        net_delta_base,
                        elapsed_ms,
                        reason,
                    };
                }
                Ok(AdlDeltaAction::EmergencyReduceLeg {
                    leg: PairedLeg::Primary,
                    ..
                }) if strict_reduce => {
                    if let Some((is_buy, qty, limit_price, reduce_only)) = self
                        .build_single_leg_residual(PairedLeg::Primary, primary, net_residual, true)
                    {
                        self.reset_persist();
                        self.in_flight = true;
                        self.counters.primary_residual_hedges += 1;
                        return BasisCoreAction::EmergencyReducePrimary {
                            is_buy,
                            qty,
                            limit_price,
                            reduce_only,
                        };
                    }
                }
                Ok(_) => {}
            }

            if let Some((is_buy, qty, limit_price, reduce_only)) =
                self.build_single_leg_residual(PairedLeg::Hedge, hedge, net_residual, strict_reduce)
            {
                self.reset_persist();
                self.in_flight = true;
                self.counters.hedge_residual_hedges += 1;
                return BasisCoreAction::EmergencyReduceHedge {
                    is_buy,
                    qty,
                    limit_price,
                    reduce_only,
                };
            }
            self.reset_persist();
            return BasisCoreAction::Hold {
                reason: "residual_recovery_blocked",
            };
        }

        let Some(pb) = primary.bid_px.to_f64() else {
            return BasisCoreAction::Hold {
                reason: "primary_px_overflow",
            };
        };
        let Some(pa) = primary.ask_px.to_f64() else {
            return BasisCoreAction::Hold {
                reason: "primary_px_overflow",
            };
        };
        let Some(hb) = hedge.bid_px.to_f64() else {
            return BasisCoreAction::Hold {
                reason: "hedge_px_overflow",
            };
        };
        let Some(ha) = hedge.ask_px.to_f64() else {
            return BasisCoreAction::Hold {
                reason: "hedge_px_overflow",
            };
        };
        let pm = 0.5 * (pb + pa);
        let hm = 0.5 * (hb + ha);
        if pm <= 0.0 || pa <= 0.0 || ha <= 0.0 || hm <= 0.0 {
            return BasisCoreAction::Hold {
                reason: "invalid_prices",
            };
        }

        let combined_spread_bps = (((pa - pb) / pm + (ha - hb) / hm) * 1e4).max(0.0);
        let kalman_snap = if let Some(kalman) = self.kalman.as_mut() {
            let basis_bps = (pm / hm - 1.0) * 1e4;
            let snap = kalman.observe(now_ms, basis_bps, combined_spread_bps, regime);
            self.last_kalman_snapshot = Some(snap);
            Some(snap)
        } else {
            None
        };

        let edge_pos_bp = (hb / pa - 1.0) * 1e4;
        let edge_neg_bp = (pb / ha - 1.0) * 1e4;
        let refresh_ok = self.dual_refresh_ok(primary.receipt_ms, hedge.receipt_ms);

        // Priority 1: Paired exit (NEVER blocked by oracle divergence).
        if self.inv_primary.abs() > self.dust_qty {
            let pos_sign: i8 = if self.inv_primary > Decimal::ZERO {
                1
            } else {
                -1
            };
            let hold_ms = now_ms.saturating_sub(self.entry_open_ms.unwrap_or(now_ms));
            let kalman_time_stop = self
                .kalman
                .as_ref()
                .zip(kalman_snap.as_ref())
                .is_some_and(|(k, s)| k.should_time_stop(s, hold_ms));
            let timed_out = hold_ms >= self.config.max_hold_ms || kalman_time_stop;
            let close_edge_bp = if pos_sign > 0 {
                edge_neg_bp
            } else {
                edge_pos_bp
            };
            let funding_exit_adj_bp = if self.config.funding_settlement.enabled
                && let Some(fc) = ctx.funding_clock
                && fc.primary_funding_bps_per_period.is_finite()
                && fc.hedge_funding_bps_per_period.is_finite()
            {
                let fs = &self.config.funding_settlement;
                let pos_carry = if pos_sign > 0 {
                    fc.hedge_funding_bps_per_period - fc.primary_funding_bps_per_period
                } else {
                    fc.primary_funding_bps_per_period - fc.hedge_funding_bps_per_period
                };
                (pos_carry * fs.funding_exit_weight)
                    .clamp(-fs.max_exit_bp_adjust, fs.max_exit_bp_adjust)
            } else {
                0.0
            };
            let eff_exit_bp = self.config.exit_bp + funding_exit_adj_bp;
            if timed_out || (close_edge_bp >= eff_exit_bp && refresh_ok) {
                let direction_primary_buy = pos_sign < 0;
                let p_touch = if direction_primary_buy {
                    primary.ask_px
                } else {
                    primary.bid_px
                };
                let h_touch = if direction_primary_buy {
                    hedge.bid_px
                } else {
                    hedge.ask_px
                };
                let Some(primary_limit_px) =
                    protective_limit(p_touch, direction_primary_buy, self.config.slippage_bps)
                else {
                    return BasisCoreAction::Hold {
                        reason: "invalid_exit_limit",
                    };
                };
                let Some(hedge_limit_px) =
                    protective_limit(h_touch, !direction_primary_buy, self.config.slippage_bps)
                else {
                    return BasisCoreAction::Hold {
                        reason: "invalid_exit_limit",
                    };
                };
                let p_vis =
                    primary.visible_size_within_limit(direction_primary_buy, primary_limit_px);
                let h_vis = hedge.visible_size_within_limit(!direction_primary_buy, hedge_limit_px);
                let raw_close = self
                    .inv_primary
                    .abs()
                    .min(p_vis)
                    .min(h_vis)
                    .min(self.config.leg_capacity(
                        p_touch.max(primary_limit_px),
                        self.inv_primary,
                        direction_primary_buy,
                        true,
                    ))
                    .min(self.config.leg_capacity(
                        h_touch.max(hedge_limit_px),
                        self.inv_hedge,
                        !direction_primary_buy,
                        true,
                    ));
                let max_close = if raw_close > Decimal::ZERO {
                    quantize_shared_step_qty(
                        raw_close,
                        self.constraints.primary_step_size,
                        self.constraints.hedge_step_size,
                    )
                    .unwrap_or(Decimal::ZERO)
                } else {
                    Decimal::ZERO
                };
                if max_close <= self.dust_qty
                    || max_close < self.constraints.primary_min_order_size
                    || max_close < self.constraints.hedge_min_order_size
                {
                    return BasisCoreAction::Hold {
                        reason: "thin_exit_book",
                    };
                }
                self.reset_persist();
                self.in_flight = true;
                self.counters.exits_emitted += 1;
                return BasisCoreAction::ExitPairIoc {
                    direction_primary_buy,
                    qty: max_close,
                    primary_limit_px,
                    hedge_limit_px,
                    timed_out,
                };
            }
        }

        // Priority 2: Paired entry under strict drawdown, oracle, lease, refresh, and cap gates.
        let brake_scale = daily_loss.risk_scale();
        if brake_scale <= 0.0 {
            self.reset_persist();
            return BasisCoreAction::Hold {
                reason: "daily_loss_halted",
            };
        }
        if !primary_lease_ok {
            self.reset_persist();
            return BasisCoreAction::Hold {
                reason: "primary_lease_required",
            };
        }
        if !refresh_ok {
            return BasisCoreAction::Hold {
                reason: "awaiting_post_trade_refresh",
            };
        }
        let (rtt_clip_scale, eff_entry_slippage_bps) = if self.config.latency_budget.enabled {
            let lb = &self.config.latency_budget;
            if let Some(p95_ms) = self
                .rtt_tracker
                .p95_ms()
                .or_else(|| ctx.hedge_rtt_ms.filter(|r| r.is_finite() && *r > 0.0))
            {
                if p95_ms > lb.max_entry_rtt_ms {
                    self.reset_persist();
                    return BasisCoreAction::Hold {
                        reason: "latency_budget_exceeded",
                    };
                }
                let sigma = ctx
                    .realized_vol_bps_per_sqrt_s
                    .filter(|v| v.is_finite() && *v > 0.0)
                    .unwrap_or(1.0);
                let drift_bps = lb.vol_drift_k * sigma * (p95_ms / 1_000.0).sqrt();
                let eff_slip = (self.config.slippage_bps + drift_bps).clamp(
                    self.config.slippage_bps,
                    lb.max_slippage_bps.max(self.config.slippage_bps),
                );
                let scale = if p95_ms > lb.target_rtt_ms {
                    let span = (lb.max_entry_rtt_ms - lb.target_rtt_ms).max(1.0);
                    let frac = ((p95_ms - lb.target_rtt_ms) / span).clamp(0.0, 1.0);
                    (1.0 - (1.0 - lb.min_clip_scale) * frac).clamp(lb.min_clip_scale, 1.0)
                } else {
                    1.0
                };
                (scale, eff_slip)
            } else {
                (1.0, self.config.slippage_bps)
            }
        } else {
            (1.0, self.config.slippage_bps)
        };

        let effective_oracle_px = if let Some(ref policy) = self.config.multi_oracle {
            let samples = ctx.reference_oracles.unwrap_or(&[]);
            match evaluate_multi_source_oracle_median(now_ms, samples, policy) {
                Ok(consensus) => Some(consensus.median_price),
                Err(_) => {
                    self.reset_persist();
                    self.counters.oracle_blocked_entries += 1;
                    return BasisCoreAction::Hold {
                        reason: "multi_oracle_consensus_failed",
                    };
                }
            }
        } else {
            reference_oracle_px
        };

        match effective_oracle_px {
            Some(oracle_px) => {
                let pair_ref = (primary.bid_px + primary.ask_px) * Decimal::new(5, 1);
                match evaluate_reference_oracle_divergence_bps(
                    pair_ref,
                    oracle_px,
                    self.config.max_oracle_divergence_bps,
                ) {
                    Ok((_, TailRiskAction::Normal)) => {}
                    Ok((_, TailRiskAction::ReduceOnly))
                    | Ok((_, TailRiskAction::HaltAndFlatten))
                    | Err(_) => {
                        self.reset_persist();
                        self.counters.oracle_blocked_entries += 1;
                        return BasisCoreAction::Hold {
                            reason: "oracle_divergence_reduce_only",
                        };
                    }
                }
            }
            None if self.config.require_reference_oracle_for_entry => {
                self.reset_persist();
                self.counters.oracle_blocked_entries += 1;
                return BasisCoreAction::Hold {
                    reason: "oracle_missing_reduce_only",
                };
            }
            None => {}
        }

        let min_entry_usd = self.constraints.min_entry_notional_usd();
        let pos_usd = self.inv_primary.abs().to_f64().unwrap_or(f64::MAX) * pm;
        let remaining_usd = self.config.max_notional_usd - pos_usd;
        if remaining_usd < min_entry_usd {
            self.reset_persist();
            return BasisCoreAction::Hold {
                reason: "max_notional_reached",
            };
        }
        if self.config.funding_settlement.enabled
            && let Some(fc) = ctx.funding_clock
            && (!fc.primary_funding_bps_per_period.is_finite()
                || !fc.hedge_funding_bps_per_period.is_finite())
        {
            self.reset_persist();
            return BasisCoreAction::Hold {
                reason: "invalid_funding_clock",
            };
        }

        let allowed_dirs: [i8; 2] = if self.inv_primary.abs() > self.dust_qty {
            let s = if self.inv_primary > Decimal::ZERO {
                1
            } else {
                -1
            };
            [s, 0]
        } else {
            [1, -1]
        };

        let penalty_bp = self.ladder_add_penalty_bp(pos_usd);
        let spread_surcharge_bp = if self.config.spread_cost_entry_coeff > 0.0 {
            self.config.spread_cost_entry_coeff * combined_spread_bps
        } else {
            0.0
        };
        let req_edge_bp = self.config.entry_bp + penalty_bp + spread_surcharge_bp;
        let mut best_dir = 0_i8;
        let mut best_raw_bp = f64::NEG_INFINITY;
        let mut leung_li_hit = false;
        let mut kalman_blocked = false;
        let mut funding_blocked = false;
        let kalman_eval = self
            .kalman
            .as_ref()
            .zip(kalman_snap.as_ref())
            .map(|(k, s)| k.evaluate_entry(s));

        for &dir in &allowed_dirs {
            if dir == 0 {
                continue;
            }
            let raw_bp = if dir > 0 { edge_pos_bp } else { edge_neg_bp };
            if raw_bp > self.config.max_entry_bp {
                leung_li_hit = true;
                continue;
            }
            if let Some(eval) = kalman_eval {
                let side_matches = (dir > 0 && eval.preferred_side == Some(MakerEntrySide::Buy))
                    || (dir < 0 && eval.preferred_side == Some(MakerEntrySide::Sell));
                if !eval.allowed || !side_matches {
                    if raw_bp >= req_edge_bp {
                        kalman_blocked = true;
                    }
                    continue;
                }
            }
            if self.config.funding_settlement.enabled
                && let Some(fc) = ctx.funding_clock
                && fc.primary_funding_bps_per_period.is_finite()
                && fc.hedge_funding_bps_per_period.is_finite()
                && fc.next_settlement_ms >= now_ms
                && fc.next_settlement_ms - now_ms
                    <= self.config.funding_settlement.pre_settlement_blackout_ms
            {
                let dir_carry = if dir > 0 {
                    fc.hedge_funding_bps_per_period - fc.primary_funding_bps_per_period
                } else {
                    fc.primary_funding_bps_per_period - fc.hedge_funding_bps_per_period
                };
                if dir_carry < -self.config.funding_settlement.adverse_funding_block_bps {
                    if raw_bp >= req_edge_bp {
                        funding_blocked = true;
                    }
                    continue;
                }
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
            return BasisCoreAction::Hold {
                reason: if leung_li_hit {
                    "leung_li_structural_cap"
                } else if funding_blocked {
                    "funding_blackout_adverse_carry"
                } else if kalman_blocked {
                    "kalman_ou_entry_blocked"
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
                return BasisCoreAction::Hold {
                    reason: "persistence_arming",
                };
            }
            if now_ms.saturating_sub(self.armed_since_ms) < self.config.persist_ms {
                return BasisCoreAction::Hold {
                    reason: "persistence_waiting",
                };
            }
        }

        let eff_clip_usd = (self.config.clip_usd * brake_scale * rtt_clip_scale).min(remaining_usd);
        if eff_clip_usd < min_entry_usd {
            self.reset_persist();
            return BasisCoreAction::Hold {
                reason: "braked_below_min_notional",
            };
        }
        let direction_primary_buy = best_dir > 0;
        let p_touch = if direction_primary_buy {
            primary.ask_px
        } else {
            primary.bid_px
        };
        let h_touch = if direction_primary_buy {
            hedge.bid_px
        } else {
            hedge.ask_px
        };
        let Some(primary_limit_px) =
            protective_limit(p_touch, direction_primary_buy, eff_entry_slippage_bps)
        else {
            return BasisCoreAction::Hold {
                reason: "invalid_entry_limit",
            };
        };
        let Some(hedge_limit_px) =
            protective_limit(h_touch, !direction_primary_buy, eff_entry_slippage_bps)
        else {
            return BasisCoreAction::Hold {
                reason: "invalid_entry_limit",
            };
        };
        let p_vis = primary.visible_size_within_limit(direction_primary_buy, primary_limit_px);
        let h_vis = hedge.visible_size_within_limit(!direction_primary_buy, hedge_limit_px);
        let p_bound = p_touch.max(primary_limit_px);
        let h_bound = h_touch.max(hedge_limit_px);
        let clip = decimal_from_f64(eff_clip_usd).unwrap_or_default() / p_bound.max(h_bound);
        let raw_capped_qty = clip
            .min(p_vis)
            .min(h_vis)
            .min(
                self.config
                    .leg_capacity(p_bound, self.inv_primary, direction_primary_buy, false),
            )
            .min(
                self.config
                    .leg_capacity(h_bound, self.inv_hedge, !direction_primary_buy, false),
            );
        let qty = if raw_capped_qty > Decimal::ZERO {
            quantize_shared_step_qty(
                raw_capped_qty,
                self.constraints.primary_step_size,
                self.constraints.hedge_step_size,
            )
            .unwrap_or(Decimal::ZERO)
        } else {
            Decimal::ZERO
        };
        if qty <= self.dust_qty
            || qty < self.constraints.primary_min_order_size
            || qty < self.constraints.hedge_min_order_size
        {
            self.reset_persist();
            return BasisCoreAction::Hold {
                reason: "thin_entry_book",
            };
        }
        let p_notional_usd = (qty * p_touch.min(primary_limit_px))
            .to_f64()
            .unwrap_or(0.0);
        let h_notional_usd = (qty * h_touch.min(hedge_limit_px)).to_f64().unwrap_or(0.0);
        if p_notional_usd < self.constraints.primary_min_order_notional_usd
            || h_notional_usd < self.constraints.hedge_min_order_notional_usd
        {
            self.reset_persist();
            return BasisCoreAction::Hold {
                reason: "entry_below_venue_min",
            };
        }

        self.armed_dir = None;
        self.armed_since_ms = 0;
        self.in_flight = true;
        self.counters.entries_emitted += 1;
        BasisCoreAction::EnterPairIoc {
            direction_primary_buy,
            qty,
            primary_limit_px,
            hedge_limit_px,
            raw_edge_bp: best_raw_bp,
            req_edge_bp,
            brake_scale,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daily_loss::utc_day_now;
    use crate::shm_book::FixedPriceLevels;

    fn d(s: &str) -> Decimal {
        Decimal::from_str_exact(s).unwrap()
    }

    fn base_cfg() -> BasisCoreConfig {
        BasisCoreConfig {
            primary_venue_id: "venue_a".into(),
            hedge_venue_id: "venue_b".into(),
            entry_bp: 10.0,
            exit_bp: 0.0,
            max_entry_bp: 35.0,
            persist_ms: 0,
            post_trade_refresh_lock: false,
            clip_usd: 100.0,
            max_notional_usd: 500.0,
            max_order_usd: 250.0,
            max_total_notional_usd: 500.0,
            ladder_floor_frac: 0.25,
            ladder_scale_bps: 2.0,
            ladder_convex_power: 1.0,
            max_hold_ms: 3_600_000,
            max_primary_book_age_ms: 250,
            max_hedge_book_age_ms: 2_000,
            max_venue_clock_skew_ms: 2_000,
            slippage_bps: 8.0,
            max_oracle_divergence_bps: 25.0,
            require_reference_oracle_for_entry: true,
            residual_policy: ResidualRecoveryPolicy::PreferReduceOnlySurvivingLeg,
            adl_delta_breaker: AdlDeltaBreakerConfig::default(),
            kalman_basis: None,
            funding_settlement: FundingSettlementConfig::default(),
            multi_oracle: None,
            latency_budget: LatencyBudgetConfig::default(),
            spread_cost_entry_coeff: 0.0,
        }
    }

    #[test]
    fn shared_step_lcm_handles_arbitrary_steps_and_fails_closed() {
        let q1 = quantize_shared_step_qty(d("1.237"), d("0.005"), d("0.01")).unwrap();
        assert_eq!(q1, d("1.23"));
        let q2 = quantize_shared_step_qty(d("1.23"), d("0.25"), d("0.1")).unwrap();
        assert_eq!(q2, d("1.0"));
        assert!(quantize_shared_step_qty(Decimal::ZERO, d("0.01"), d("0.01")).is_err());
        assert!(quantize_shared_step_qty(d("1.0"), Decimal::ZERO, d("0.01")).is_err());
    }

    #[test]
    fn normalized_leg_book_rejects_missing_feeder_receipt_clock() {
        let missing = NormalizedBookSnapshot {
            source_ms: 1_000,
            feeder_receipt_ms: None,
            bid_px: d("99.9"),
            bid_sz: d("10"),
            ask_px: d("100.1"),
            ask_sz: d("10"),
            bids: FixedPriceLevels::from_l1(d("99.9"), d("10")),
            asks: FixedPriceLevels::from_l1(d("100.1"), d("10")),
        };
        assert!(NormalizedLegBook::try_from(&missing).is_err());

        let mut ok = missing;
        ok.feeder_receipt_ms = Some(1_002);
        assert!(NormalizedLegBook::try_from(&ok).is_ok());
    }

    #[test]
    fn adl_on_hedge_unwinds_surviving_primary_leg_with_reduce_only_and_escalates_past_sla() {
        let constraints = BasisLegConstraints {
            primary_step_size: d("0.01"),
            primary_min_order_size: d("0.01"),
            primary_min_order_notional_usd: 5.0,
            hedge_step_size: d("0.01"),
            hedge_min_order_size: d("0.01"),
            hedge_min_order_notional_usd: 10.0,
        };
        let mut engine = BasisCoreEngine::new(base_cfg(), constraints).unwrap();
        let guard = DailyLossGuard::open(None, 0.05, 1_000.0, utc_day_now()).unwrap();
        let p_book =
            NormalizedLegBook::from_l1(1_000, 1_000, d("100.00"), d("20"), d("100.02"), d("20"))
                .unwrap();
        let h_book =
            NormalizedLegBook::from_l1(1_000, 1_000, d("100.00"), d("20"), d("100.02"), d("20"))
                .unwrap();

        // Hedge leg shrunk by ADL from -10 to -8 while Primary stays +10 -> net residual +2.
        // MUST reduce Primary (-2, reduce_only=true) instead of opening risky non-reduce short on Hedge!
        engine.reconcile_positions(d("10"), d("-8"), 1_000, 1_000, 1_000);
        let act = engine.on_books(
            1_050,
            &p_book,
            &h_book,
            &guard,
            true,
            None,
            SessionRegime::UsRth,
        );
        assert!(matches!(
            act,
            BasisCoreAction::EmergencyReducePrimary {
                is_buy: false,
                qty,
                reduce_only: true,
                ..
            } if qty == d("2")
        ));

        // If Primary was ADL-shrunk (+8 primary, -10 hedge -> residual -2), reduce Hedge (+2, reduce_only=true).
        engine.reconcile_positions(d("8"), d("-10"), 1_100, 1_100, 1_100);
        let p_book2 =
            NormalizedLegBook::from_l1(1_150, 1_150, d("100.00"), d("20"), d("100.02"), d("20"))
                .unwrap();
        let h_book2 =
            NormalizedLegBook::from_l1(1_150, 1_150, d("100.00"), d("20"), d("100.02"), d("20"))
                .unwrap();
        let act2 = engine.on_books(
            1_150,
            &p_book2,
            &h_book2,
            &guard,
            true,
            None,
            SessionRegime::UsRth,
        );
        assert!(matches!(
            act2,
            BasisCoreAction::EmergencyReduceHedge {
                is_buy: true,
                qty,
                reduce_only: true,
                ..
            } if qty == d("2")
        ));

        // Beyond halt_sla_ms (200 ms) escalates cleanly to HaltAndFlatten!
        let p_book3 =
            NormalizedLegBook::from_l1(1_350, 1_350, d("100.00"), d("20"), d("100.02"), d("20"))
                .unwrap();
        let h_book3 =
            NormalizedLegBook::from_l1(1_350, 1_350, d("100.00"), d("20"), d("100.02"), d("20"))
                .unwrap();
        engine.reconcile_positions(d("8"), d("-10"), 1_100, 1_100, 1_100);
        let act3 = engine.on_books(
            1_350,
            &p_book3,
            &h_book3,
            &guard,
            true,
            None,
            SessionRegime::UsRth,
        );
        assert!(matches!(act3, BasisCoreAction::HaltAndFlatten { .. }));
    }

    #[test]
    fn oracle_divergence_blocks_entry_while_allowing_paired_exit() {
        let constraints = BasisLegConstraints::default();
        let mut cfg = base_cfg();
        cfg.persist_ms = 0;
        let mut engine = BasisCoreEngine::new(cfg, constraints).unwrap();
        let guard = DailyLossGuard::open(None, 0.05, 1_000.0, utc_day_now()).unwrap();

        // Rich entry edge (+20 bps) but oracle diverged by 40 bps -> entry MUST block.
        let p_book =
            NormalizedLegBook::from_l1(2_000, 2_000, d("99.98"), d("10"), d("100.00"), d("10"))
                .unwrap();
        let h_book =
            NormalizedLegBook::from_l1(2_000, 2_000, d("100.20"), d("10"), d("100.22"), d("10"))
                .unwrap();
        let blocked = engine.on_books(
            2_000,
            &p_book,
            &h_book,
            &guard,
            true,
            Some(d("99.50")),
            SessionRegime::UsRth,
        );
        assert_eq!(
            blocked,
            BasisCoreAction::Hold {
                reason: "oracle_divergence_reduce_only"
            }
        );

        // Carrying paired position + exit condition -> exit proceeds EVEN WITH oracle diverged!
        engine.reconcile_positions(d("0.5"), d("-0.5"), 2_000, 2_000, 2_000);
        let p_exit =
            NormalizedLegBook::from_l1(2_100, 2_100, d("100.05"), d("10"), d("100.07"), d("10"))
                .unwrap();
        let h_exit =
            NormalizedLegBook::from_l1(2_100, 2_100, d("99.98"), d("10"), d("100.00"), d("10"))
                .unwrap();
        let exited = engine.on_books(
            2_100,
            &p_exit,
            &h_exit,
            &guard,
            true,
            Some(d("99.50")),
            SessionRegime::UsRth,
        );
        assert!(matches!(exited, BasisCoreAction::ExitPairIoc { .. }));
    }

    #[test]
    fn missing_oracle_blocks_new_entries_when_required_while_keeping_exits_and_adl_alive() {
        let constraints = BasisLegConstraints::default();
        let mut guard = DailyLossGuard::open(None, 0.05, 1_000.0, utc_day_now()).unwrap();
        let _ = &mut guard;

        let p_entry =
            NormalizedLegBook::from_l1(2_000, 2_000, d("99.98"), d("10"), d("100.00"), d("10"))
                .unwrap();
        let h_entry =
            NormalizedLegBook::from_l1(2_000, 2_000, d("100.20"), d("10"), d("100.22"), d("10"))
                .unwrap();

        // Required oracle missing (None) -> new entry MUST block fail-closed.
        let mut req_engine = BasisCoreEngine::new(base_cfg(), constraints).unwrap();
        let blocked = req_engine.on_books(
            2_000,
            &p_entry,
            &h_entry,
            &guard,
            true,
            None,
            SessionRegime::UsRth,
        );
        assert_eq!(
            blocked,
            BasisCoreAction::Hold {
                reason: "oracle_missing_reduce_only"
            }
        );
        assert_eq!(req_engine.counters.oracle_blocked_entries, 1);

        // Paired exit and ADL recovery remain permitted even when oracle is missing.
        req_engine.reconcile_positions(d("0.5"), d("-0.5"), 2_000, 2_000, 2_000);
        let p_exit =
            NormalizedLegBook::from_l1(2_100, 2_100, d("100.05"), d("10"), d("100.07"), d("10"))
                .unwrap();
        let h_exit =
            NormalizedLegBook::from_l1(2_100, 2_100, d("99.98"), d("10"), d("100.00"), d("10"))
                .unwrap();
        assert!(matches!(
            req_engine.on_books(
                2_100,
                &p_exit,
                &h_exit,
                &guard,
                true,
                None,
                SessionRegime::UsRth,
            ),
            BasisCoreAction::ExitPairIoc { .. }
        ));

        req_engine.reconcile_positions(d("2.0"), d("-1.5"), 2_200, 2_200, 2_200);
        let p_adl =
            NormalizedLegBook::from_l1(2_250, 2_250, d("100.00"), d("10"), d("100.02"), d("10"))
                .unwrap();
        let h_adl =
            NormalizedLegBook::from_l1(2_250, 2_250, d("100.00"), d("10"), d("100.02"), d("10"))
                .unwrap();
        assert!(matches!(
            req_engine.on_books(
                2_250,
                &p_adl,
                &h_adl,
                &guard,
                true,
                None,
                SessionRegime::UsRth,
            ),
            BasisCoreAction::EmergencyReducePrimary {
                reduce_only: true,
                ..
            }
        ));

        // Explicit opt-out allows entry with None oracle.
        let mut opt_cfg = base_cfg();
        opt_cfg.require_reference_oracle_for_entry = false;
        let mut opt_engine = BasisCoreEngine::new(opt_cfg, constraints).unwrap();
        assert!(matches!(
            opt_engine.on_books(
                2_000,
                &p_entry,
                &h_entry,
                &guard,
                true,
                None,
                SessionRegime::UsRth,
            ),
            BasisCoreAction::EnterPairIoc { .. }
        ));
    }

    #[test]
    fn strict_reduce_only_recovery_holds_when_surviving_leg_book_thin_never_opening_hedge() {
        let constraints = BasisLegConstraints {
            primary_step_size: d("0.01"),
            primary_min_order_size: d("0.01"),
            primary_min_order_notional_usd: 5.0,
            hedge_step_size: d("0.01"),
            hedge_min_order_size: d("0.01"),
            hedge_min_order_notional_usd: 10.0,
        };
        let mut engine = BasisCoreEngine::new(base_cfg(), constraints).unwrap();
        let guard = DailyLossGuard::open(None, 0.05, 1_000.0, utc_day_now()).unwrap();

        // Hedge leg was ADL-shrunk (-10 -> -8 while Primary is +10 -> net +2).
        // Under PreferReduceOnlySurvivingLeg, if Primary has insufficient liquidity (< min_order_size),
        // engine MUST Hold rather than falling through to a non-reduce_only Hedge short!
        engine.reconcile_positions(d("10"), d("-8"), 1_000, 1_000, 1_000);
        let thin_primary =
            NormalizedLegBook::from_l1(1_050, 1_050, d("100.00"), d("0.001"), d("100.02"), d("10"))
                .unwrap();
        let deep_hedge =
            NormalizedLegBook::from_l1(1_050, 1_050, d("100.00"), d("50"), d("100.02"), d("50"))
                .unwrap();
        let act = engine.on_books(
            1_050,
            &thin_primary,
            &deep_hedge,
            &guard,
            true,
            Some(d("100.01")),
            SessionRegime::UsRth,
        );
        assert_eq!(
            act,
            BasisCoreAction::Hold {
                reason: "residual_recovery_blocked"
            }
        );
        assert_eq!(engine.counters.hedge_residual_hedges, 0);
    }

    #[test]
    fn basis_core_enforces_funding_blackout_multi_oracle_consensus_and_rtt_budget() {
        let constraints = BasisLegConstraints {
            primary_step_size: d("0.01"),
            primary_min_order_size: d("0.01"),
            primary_min_order_notional_usd: 5.0,
            hedge_step_size: d("0.01"),
            hedge_min_order_size: d("0.01"),
            hedge_min_order_notional_usd: 10.0,
        };
        let mut cfg = base_cfg();
        cfg.funding_settlement = FundingSettlementConfig {
            enabled: true,
            pre_settlement_blackout_ms: 120_000,
            adverse_funding_block_bps: 0.5,
            funding_exit_weight: 0.5,
            max_exit_bp_adjust: 5.0,
        };
        cfg.multi_oracle = Some(MultiOraclePolicy {
            min_sources: 2,
            max_oracle_age_ms: 2_000,
            max_future_skew_ms: 2_000,
            max_inter_source_spread_bps: 30.0,
        });
        cfg.latency_budget = LatencyBudgetConfig {
            enabled: true,
            window_samples: 8,
            target_rtt_ms: 50.0,
            max_entry_rtt_ms: 200.0,
            vol_drift_k: 2.0,
            max_slippage_bps: 25.0,
            min_clip_scale: 0.25,
        };
        let mut engine = BasisCoreEngine::new(cfg, constraints).unwrap();
        let guard = DailyLossGuard::open(None, 0.05, 1_000.0, utc_day_now()).unwrap();
        let now_ms = 10_000_u64;
        let p_book =
            NormalizedLegBook::from_l1(now_ms, now_ms, d("99.90"), d("10"), d("99.92"), d("10"))
                .unwrap();
        let h_book =
            NormalizedLegBook::from_l1(now_ms, now_ms, d("100.10"), d("10"), d("100.12"), d("10"))
                .unwrap();
        let good_oracles = [
            ReferenceOracleSample {
                source_id: "s1".into(),
                price: d("100.00"),
                source_ms: 9_800,
                receipt_ms: 9_900,
            },
            ReferenceOracleSample {
                source_id: "s2".into(),
                price: d("100.04"),
                source_ms: 9_850,
                receipt_ms: 9_950,
            },
        ];

        // 1. Pre-settlement adverse carry blocks entry.
        let blk = engine.on_books_with_context(
            now_ms,
            &p_book,
            &h_book,
            &guard,
            true,
            None,
            SessionRegime::UsRth,
            BasisMarketContext {
                funding_clock: Some(FundingClockSample {
                    next_settlement_ms: now_ms + 30_000,
                    primary_funding_bps_per_period: 3.0,
                    hedge_funding_bps_per_period: 0.0, // +1 carry = -3.0 bps < -0.5
                }),
                reference_oracles: Some(&good_oracles),
                hedge_rtt_ms: Some(40.0),
                realized_vol_bps_per_sqrt_s: Some(1.0),
            },
        );
        assert_eq!(
            blk,
            BasisCoreAction::Hold {
                reason: "funding_blackout_adverse_carry"
            }
        );

        // 2. Single oracle / missing quorum fails closed on multi_oracle check.
        let single_oracle = &good_oracles[..1];
        let bad_quorum = engine.on_books_with_context(
            now_ms,
            &p_book,
            &h_book,
            &guard,
            true,
            None,
            SessionRegime::UsRth,
            BasisMarketContext {
                funding_clock: None,
                reference_oracles: Some(single_oracle),
                hedge_rtt_ms: Some(40.0),
                realized_vol_bps_per_sqrt_s: Some(1.0),
            },
        );
        assert_eq!(
            bad_quorum,
            BasisCoreAction::Hold {
                reason: "multi_oracle_consensus_failed"
            }
        );

        // 3. Elevated RTT within budget scales down clip and widens slippage; >max_entry_rtt_ms pauses entry.
        let scaled = engine.on_books_with_context(
            now_ms,
            &p_book,
            &h_book,
            &guard,
            true,
            None,
            SessionRegime::UsRth,
            BasisMarketContext {
                funding_clock: None,
                reference_oracles: Some(&good_oracles),
                hedge_rtt_ms: Some(150.0),
                realized_vol_bps_per_sqrt_s: Some(1.0),
            },
        );
        let BasisCoreAction::EnterPairIoc {
            qty,
            hedge_limit_px,
            ..
        } = scaled
        else {
            panic!("expected scaled EnterPairIoc, got {scaled:?}");
        };
        assert!(qty < d("0.8")); // tapered down by p95 RTT
        assert!(hedge_limit_px < d("100.10") * d("0.9992")); // widened slippage beyond base 8 bp
        engine.reconcile_positions(Decimal::ZERO, Decimal::ZERO, now_ms, now_ms, now_ms);

        let paused = engine.on_books_with_context(
            now_ms,
            &p_book,
            &h_book,
            &guard,
            true,
            None,
            SessionRegime::UsRth,
            BasisMarketContext {
                funding_clock: None,
                reference_oracles: Some(&good_oracles),
                hedge_rtt_ms: Some(300.0),
                realized_vol_bps_per_sqrt_s: Some(1.0),
            },
        );
        assert_eq!(
            paused,
            BasisCoreAction::Hold {
                reason: "latency_budget_exceeded"
            }
        );
    }

    #[test]
    fn basis_core_rejects_under_floor_latency_slippage_and_fails_closed_on_nan_funding_while_allowing_exit()
     {
        let mut under_floor_cfg = base_cfg();
        under_floor_cfg.latency_budget = LatencyBudgetConfig {
            enabled: true,
            window_samples: 8,
            target_rtt_ms: 50.0,
            max_entry_rtt_ms: 200.0,
            vol_drift_k: 2.0,
            max_slippage_bps: 5.0, // < slippage_bps (8.0)
            min_clip_scale: 0.25,
        };
        assert!(under_floor_cfg.validate().is_err());
        under_floor_cfg.latency_budget.max_slippage_bps = under_floor_cfg.slippage_bps;
        assert!(under_floor_cfg.validate().is_ok());

        let constraints = BasisLegConstraints {
            primary_step_size: d("0.01"),
            primary_min_order_size: d("0.01"),
            primary_min_order_notional_usd: 5.0,
            hedge_step_size: d("0.01"),
            hedge_min_order_size: d("0.01"),
            hedge_min_order_notional_usd: 10.0,
        };
        let mut cfg = base_cfg();
        cfg.funding_settlement = FundingSettlementConfig {
            enabled: true,
            pre_settlement_blackout_ms: 120_000,
            adverse_funding_block_bps: 0.5,
            funding_exit_weight: 0.5,
            max_exit_bp_adjust: 5.0,
        };
        let mut engine = BasisCoreEngine::new(cfg, constraints).unwrap();
        let guard = DailyLossGuard::open(None, 0.05, 1_000.0, utc_day_now()).unwrap();
        let now_ms = 10_000_u64;
        let nan_funding = BasisMarketContext {
            funding_clock: Some(FundingClockSample {
                next_settlement_ms: now_ms + 30_000,
                primary_funding_bps_per_period: f64::NAN,
                hedge_funding_bps_per_period: 0.0,
            }),
            reference_oracles: Some(&[ReferenceOracleSample {
                source_id: "s1".into(),
                price: d("100.00"),
                source_ms: 9_850,
                receipt_ms: 9_950,
            }]),
            hedge_rtt_ms: Some(40.0),
            realized_vol_bps_per_sqrt_s: Some(1.0),
        };
        let entry_p =
            NormalizedLegBook::from_l1(now_ms, now_ms, d("99.90"), d("10"), d("99.92"), d("10"))
                .unwrap();
        let entry_h =
            NormalizedLegBook::from_l1(now_ms, now_ms, d("100.10"), d("10"), d("100.12"), d("10"))
                .unwrap();

        let blocked = engine.on_books_with_context(
            now_ms,
            &entry_p,
            &entry_h,
            &guard,
            true,
            Some(d("100.00")),
            SessionRegime::UsRth,
            nan_funding,
        );
        assert_eq!(
            blocked,
            BasisCoreAction::Hold {
                reason: "invalid_funding_clock"
            }
        );

        // Open paired position must still emit reduce-only exit under the same NaN funding clock.
        engine.reconcile_positions(
            d("0.5"),
            d("-0.5"),
            now_ms - 5_000,
            now_ms - 5_000,
            now_ms - 5_000,
        );
        let exit_p =
            NormalizedLegBook::from_l1(now_ms, now_ms, d("100.10"), d("10"), d("100.12"), d("10"))
                .unwrap();
        let exit_h =
            NormalizedLegBook::from_l1(now_ms, now_ms, d("99.90"), d("10"), d("99.92"), d("10"))
                .unwrap();
        let exit_act = engine.on_books_with_context(
            now_ms,
            &exit_p,
            &exit_h,
            &guard,
            true,
            Some(d("100.00")),
            SessionRegime::UsRth,
            nan_funding,
        );
        assert!(matches!(
            exit_act,
            BasisCoreAction::ExitPairIoc {
                direction_primary_buy: false,
                ..
            }
        ));
    }

    #[test]
    fn spread_cost_entry_coeff_blocks_wide_simultaneous_books_and_defaults_to_zero() {
        let mut cfg = base_cfg();
        cfg.spread_cost_entry_coeff = 0.5;
        let guard = DailyLossGuard::open(None, 0.05, 1_000.0, 1_000).unwrap();
        let mut engine = BasisCoreEngine::new(cfg, BasisLegConstraints::default()).unwrap();

        // Wide 20 bps combined book spread adds 0.5 * 20 = +10 bps hurdle above entry_bp=10 => req=20 bps.
        // Observed edge (hb/pa - 1) = (100.12 / 100.01 - 1) = +11 bps < 20 bps -> blocked!
        let p =
            NormalizedLegBook::from_l1(10_000, 10_000, d("99.91"), d("10"), d("100.01"), d("10"))
                .unwrap();
        let h =
            NormalizedLegBook::from_l1(10_000, 10_000, d("100.12"), d("10"), d("100.22"), d("10"))
                .unwrap();
        let act = engine.on_books_with_context(
            10_000,
            &p,
            &h,
            &guard,
            true,
            Some(d("100.05")),
            SessionRegime::OffHours,
            BasisMarketContext::default(),
        );
        assert_eq!(
            act,
            BasisCoreAction::Hold {
                reason: "below_entry_threshold"
            }
        );

        let mut bad = base_cfg();
        bad.spread_cost_entry_coeff = -0.1;
        assert!(bad.validate().is_err());
    }
}
