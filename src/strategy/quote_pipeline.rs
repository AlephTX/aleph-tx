//! Unified venue-agnostic strategy quoting & exit pipeline (`S1/S2/S3` core).
//!
//! Composes [`MarkovMicropriceTable`], [`GlftQuoter`], [`WalletToxicityShield`],
//! [`HybridExitController`], and [`evaluate_reference_oracle_divergence_f64`] using
//! normalized market/inventory inputs with ZERO dependencies on `crate::exchanges::*`.

use crate::strategy::glft::{GlftParams, GlftQuoter};
use crate::strategy::hybrid_exit::{
    ExitDirective, ExitEvalInput, HybridExitConfig, HybridExitController,
};
use crate::strategy::microprice::{
    MAX_PRODUCTION_MICROPRICE_RESIDUAL_TICKS, MarkovMicropriceTable,
};
use crate::strategy::tail_risk::TailRiskAction;
use crate::strategy::venue_guards::evaluate_reference_oracle_divergence_f64;
use crate::strategy::wallet_toxicity::{
    QuoteSide, WalletToxicityConfig, WalletToxicityShield, WalletToxicityTier,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

/// High-level strategy track mode isolated across independent capital/risk budgets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StrategyMode {
    NeutralMarketMaking,
    RebateLiquidityMaking,
    CrossVenueBasis,
}

/// Cross-venue lead-lag fair-price shift and toxic-jump opening-side veto configuration.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LeadLagConfig {
    pub enabled: bool,
    /// Sensitivity of fair-mid shift to external leader short-horizon return (`(0, 1]`).
    pub beta: f64,
    /// Symmetric clamp on fair-mid shift in bps (`> 0`).
    pub max_shift_bps: f64,
    /// Directional leader jump threshold (bps) that vetoes passive opening quotes on the
    /// exposed side (`ask_open` when leader jumps up, `bid_open` when leader drops).
    pub veto_threshold_bps: f64,
    /// Maximum tolerated leader signal receipt age (`ms`).
    pub max_leader_age_ms: u64,
    /// Maximum allowed positive clock skew on leader `source_ms` relative to local `now_ms`.
    pub max_leader_clock_skew_ms: u64,
}

impl Default for LeadLagConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            beta: 0.5,
            max_shift_bps: 15.0,
            veto_threshold_bps: 3.0,
            max_leader_age_ms: 250,
            max_leader_clock_skew_ms: 2_000,
        }
    }
}

impl LeadLagConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.beta.is_finite() && self.beta > 0.0 && self.beta <= 1.0,
            "lead_lag.beta must be finite and in (0, 1]"
        );
        ensure!(
            self.max_shift_bps.is_finite()
                && self.max_shift_bps > 0.0
                && self.max_shift_bps < 10_000.0,
            "lead_lag.max_shift_bps must be finite and in (0, 10_000)"
        );
        ensure!(
            self.veto_threshold_bps.is_finite() && self.veto_threshold_bps > 0.0,
            "lead_lag.veto_threshold_bps must be finite and > 0"
        );
        ensure!(
            self.max_leader_age_ms > 0 && self.max_leader_clock_skew_ms <= 30_000,
            "lead_lag age > 0 and clock skew <= 30_000 ms required"
        );
        Ok(())
    }
}

/// Online, wallet-agnostic adverse-selection shield driven by realized post-fill markout.
///
/// The offline [`WalletToxicityShield`] only reacts to pre-labeled D9/D10 wallet keys, so an
/// un-tiered sweeper or a regime shift gets zero defense until the next batch recalibration.
/// This complements it: each own-fill feeds a short-horizon markout (post-fill fair-mid revision
/// against the fill, signed so that adverse selection is NEGATIVE), smoothed by an EWMA in bps.
/// When the smoothed adverse markout breaches `widen_threshold_bps`, opening quotes are widened
/// (capped) and optionally size-decayed. Default `enabled = false` so existing behavior and the
/// frozen baseline are untouched until a profile opts in.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MarkoutShieldConfig {
    pub enabled: bool,
    /// EWMA smoothing factor on per-fill markout bps (`(0, 1]`); higher = more reactive.
    pub ewma_alpha: f64,
    /// Smoothed adverse markout (bps, magnitude) beyond which widening engages.
    pub widen_threshold_bps: f64,
    /// Additional spread multiplier at full engagement (`>= 1.0`).
    pub max_widen_mult: f64,
    /// Opening-side size scale at full engagement (`(0, 1]`, `1.0` = no size decay).
    pub engaged_size_scale: f64,
    /// Markout bps magnitude at which widening saturates to `max_widen_mult`.
    pub saturation_bps: f64,
}

impl Default for MarkoutShieldConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            ewma_alpha: 0.2,
            widen_threshold_bps: 0.8,
            max_widen_mult: 2.5,
            engaged_size_scale: 0.5,
            saturation_bps: 3.0,
        }
    }
}

impl MarkoutShieldConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.ewma_alpha.is_finite() && self.ewma_alpha > 0.0 && self.ewma_alpha <= 1.0,
            "markout_shield.ewma_alpha must be finite and in (0, 1]"
        );
        ensure!(
            self.widen_threshold_bps.is_finite() && self.widen_threshold_bps >= 0.0,
            "markout_shield.widen_threshold_bps must be finite and >= 0"
        );
        ensure!(
            self.max_widen_mult.is_finite() && self.max_widen_mult >= 1.0,
            "markout_shield.max_widen_mult must be finite and >= 1.0"
        );
        ensure!(
            self.engaged_size_scale.is_finite()
                && self.engaged_size_scale > 0.0
                && self.engaged_size_scale <= 1.0,
            "markout_shield.engaged_size_scale must be finite and in (0, 1]"
        );
        ensure!(
            self.saturation_bps.is_finite() && self.saturation_bps > self.widen_threshold_bps,
            "markout_shield.saturation_bps must be finite and > widen_threshold_bps"
        );
        Ok(())
    }
}

/// Stateful EWMA of realized post-fill adverse markout (bps). Adverse selection is negative;
/// the engagement fraction maps the magnitude of adverse markout above the threshold onto
/// `[0, 1]`, saturating at `saturation_bps`.
#[derive(Debug, Clone, Copy)]
pub struct MarkoutShield {
    config: MarkoutShieldConfig,
    ewma_markout_bps: Option<f64>,
}

impl MarkoutShield {
    pub fn new(config: MarkoutShieldConfig) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            config,
            ewma_markout_bps: None,
        })
    }

    /// Feed one realized post-fill markout in bps, signed so adverse selection is NEGATIVE
    /// (fill worse than subsequent fair mid). Ignores non-finite inputs fail-safe.
    pub fn observe_markout_bps(&mut self, markout_bps: f64) {
        if !markout_bps.is_finite() {
            return;
        }
        let a = self.config.ewma_alpha;
        self.ewma_markout_bps = Some(match self.ewma_markout_bps {
            Some(prev) => a * markout_bps + (1.0 - a) * prev,
            None => markout_bps,
        });
    }

    pub fn smoothed_markout_bps(&self) -> Option<f64> {
        self.ewma_markout_bps
    }

    /// Engagement fraction in `[0, 1]`: 0 below threshold, ramping to 1 at `saturation_bps`.
    fn engagement(&self) -> f64 {
        if !self.config.enabled {
            return 0.0;
        }
        let adverse = self.ewma_markout_bps.map(|m| (-m).max(0.0)).unwrap_or(0.0);
        if adverse <= self.config.widen_threshold_bps {
            return 0.0;
        }
        let span = self.config.saturation_bps - self.config.widen_threshold_bps;
        ((adverse - self.config.widen_threshold_bps) / span).clamp(0.0, 1.0)
    }

    /// Current (spread_multiplier >= 1.0, size_scale in (0, 1]) from the shield.
    fn adjustment(&self) -> (f64, f64) {
        let e = self.engagement();
        if e <= 0.0 {
            return (1.0, 1.0);
        }
        let mult = 1.0 + e * (self.config.max_widen_mult - 1.0);
        let size = 1.0 + e * (self.config.engaged_size_scale - 1.0);
        (mult, size)
    }
}

/// Venue-agnostic external leader price/jump signal for lead-lag protection.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ExternalLeaderSignal {
    pub leader_venue_id: u8,
    pub reference_mid_px: f64,
    /// Leader short-horizon move or basis-adjusted dislocation in bps (positive = leader jumped up).
    pub leader_return_bps: f64,
    pub source_ms: u64,
    pub receipt_ms: u64,
}

/// Funding-rate reservation-price skewing configuration. Default `enabled = false`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FundingBiasConfig {
    pub enabled: bool,
    /// Effective carrying horizon in hours (`> 0`).
    pub reservation_shift_hours: f64,
    /// Maximum symmetric reservation shift in bps (`> 0 && < 10_000`).
    pub max_reservation_shift_bps: f64,
}

impl Default for FundingBiasConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            reservation_shift_hours: 1.0,
            max_reservation_shift_bps: 10.0,
        }
    }
}

impl FundingBiasConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.reservation_shift_hours.is_finite() && self.reservation_shift_hours > 0.0,
            "funding_bias.reservation_shift_hours must be finite and > 0"
        );
        ensure!(
            self.max_reservation_shift_bps.is_finite()
                && self.max_reservation_shift_bps > 0.0
                && self.max_reservation_shift_bps < 10_000.0,
            "funding_bias.max_reservation_shift_bps must be finite and in (0, 10_000)"
        );
        Ok(())
    }
}

/// Inventory-aware opening clip tapering configuration. Default `enabled = false`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InventoryClipConfig {
    pub enabled: bool,
    /// Minimum opening clip multiplier when inventory reaches `max_inventory_base` (`(0, 1]`).
    pub min_clip_scale: f64,
    /// Convexity exponent shaping decay as `|inv| / max_inventory_base` rises (`>= 1.0`).
    pub taper_power: f64,
}

impl Default for InventoryClipConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            min_clip_scale: 0.25,
            taper_power: 1.0,
        }
    }
}

impl InventoryClipConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.min_clip_scale.is_finite()
                && self.min_clip_scale > 0.0
                && self.min_clip_scale <= 1.0,
            "inventory_clip.min_clip_scale must be finite and in (0, 1]"
        );
        ensure!(
            self.taper_power.is_finite() && self.taper_power >= 1.0 && self.taper_power <= 8.0,
            "inventory_clip.taper_power must be finite and in [1.0, 8.0]"
        );
        Ok(())
    }
}

/// Unified venue-agnostic configuration for [`QuotePipeline`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuotePipelineConfig {
    pub mode: StrategyMode,
    pub glft: GlftParams,
    #[serde(default)]
    pub wallet_toxicity: WalletToxicityConfig,
    #[serde(default)]
    pub hybrid_exit: HybridExitConfig,
    #[serde(default)]
    pub lead_lag: LeadLagConfig,
    #[serde(default)]
    pub markout_shield: MarkoutShieldConfig,
    #[serde(default)]
    pub funding_bias: FundingBiasConfig,
    #[serde(default)]
    pub inventory_clip: InventoryClipConfig,
    /// Maximum tolerated fair-mid vs oracle divergence in bps before dropping to reduce-only.
    pub max_oracle_divergence_bps: f64,
    /// Nominal per-side opening order size in base units prior to drawdown/toxicity scaling.
    pub base_order_qty: f64,
}

impl QuotePipelineConfig {
    pub fn validate(&self) -> Result<()> {
        let _ = GlftQuoter::new(self.glft)?;
        self.wallet_toxicity.validate()?;
        self.hybrid_exit.validate()?;
        self.lead_lag.validate()?;
        self.markout_shield.validate()?;
        self.funding_bias.validate()?;
        self.inventory_clip.validate()?;
        let combined_shift_bps = (if self.lead_lag.enabled {
            self.lead_lag.max_shift_bps
        } else {
            0.0
        }) + (if self.funding_bias.enabled {
            self.funding_bias.max_reservation_shift_bps
        } else {
            0.0
        });
        ensure!(
            combined_shift_bps < 10_000.0,
            "combined enabled lead_lag.max_shift_bps + funding_bias.max_reservation_shift_bps ({combined_shift_bps}) must be < 10000"
        );
        ensure!(
            self.max_oracle_divergence_bps.is_finite() && self.max_oracle_divergence_bps > 0.0,
            "max_oracle_divergence_bps must be finite and > 0"
        );
        ensure!(
            self.base_order_qty.is_finite() && self.base_order_qty > 0.0,
            "base_order_qty must be finite and > 0"
        );
        Ok(())
    }
}

/// Venue-agnostic top-of-book & oracle snapshot consumed by [`QuotePipeline::evaluate`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GenericMarketState {
    pub now_ms: u64,
    pub bid_px: f64,
    pub ask_px: f64,
    pub bid_sz: f64,
    pub ask_sz: f64,
    pub tick_size: f64,
    pub momentum_3s_bps: f64,
    pub oracle_px: Option<f64>,
    #[serde(default)]
    pub funding_rate_bps_per_hour: Option<f64>,
    #[serde(default)]
    pub exit_queue_ahead_base: Option<f64>,
}

/// Venue-agnostic inventory & drawdown brake state consumed by [`QuotePipeline::evaluate`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GenericInventoryState {
    pub signed_inventory_base: f64,
    pub avg_entry_price: f64,
    pub entry_opened_at_ms: Option<u64>,
    pub dust_base: f64,
    /// Progressive daily drawdown risk scale in `[0.0, 1.0]`.
    pub daily_loss_risk_scale: f64,
}

/// One passive opening/adding quote emitted when risk gates allow new exposure.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct OpeningQuote {
    pub side: QuoteSide,
    pub price: f64,
    pub qty: f64,
    pub spread_offset_bps: f64,
    pub faded_by_toxicity: bool,
}

/// Complete deterministic quote & exit directive produced by [`QuotePipeline::evaluate`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct QuotePlan {
    pub mode: StrategyMode,
    pub fair_mid: f64,
    pub lead_lag_shift_bps: f64,
    pub funding_shift_bps: f64,
    pub leader_stale: bool,
    pub leader_vetoed_bid: bool,
    pub leader_vetoed_ask: bool,
    pub oracle_divergence_bps: Option<f64>,
    pub oracle_reduce_only: bool,
    pub bid_open: Option<OpeningQuote>,
    pub ask_open: Option<OpeningQuote>,
    pub exit_directive: ExitDirective,
}

/// Venue-agnostic strategy evaluation pipeline decoupling signal math from exchange adapters.
#[derive(Debug, Clone)]
pub struct QuotePipeline {
    config: QuotePipelineConfig,
    microprice: Option<MarkovMicropriceTable>,
    quoter: GlftQuoter,
    toxicity: WalletToxicityShield,
    hybrid_exit: HybridExitController,
    markout_shield: MarkoutShield,
}

impl QuotePipeline {
    fn validate_microprice_table(table: Option<&MarkovMicropriceTable>) -> Result<()> {
        if let Some(tbl) = table {
            tbl.validate_for_production(MAX_PRODUCTION_MICROPRICE_RESIDUAL_TICKS)
                .with_context(|| {
                    format!(
                        "MarkovMicropriceTable (residual {:.3e}, tol {:.3e}) rejected by QuotePipeline",
                        tbl.residual_inf_norm(),
                        tbl.convergence_tol()
                    )
                })?;
        }
        Ok(())
    }

    pub fn new(
        config: QuotePipelineConfig,
        microprice: Option<MarkovMicropriceTable>,
    ) -> Result<Self> {
        config.validate()?;
        Self::validate_microprice_table(microprice.as_ref())?;
        let quoter = GlftQuoter::new(config.glft)?;
        let toxicity = WalletToxicityShield::new(config.wallet_toxicity)?;
        let hybrid_exit = HybridExitController::new(config.hybrid_exit)?;
        let markout_shield = MarkoutShield::new(config.markout_shield)?;
        Ok(Self {
            config,
            microprice,
            quoter,
            toxicity,
            hybrid_exit,
            markout_shield,
        })
    }

    /// Feed one realized own-fill post-fill markout (bps, signed so adverse selection is
    /// NEGATIVE) into the online adverse-selection shield. No-op when the shield is disabled.
    pub fn observe_fill_markout_bps(&mut self, markout_bps: f64) {
        self.markout_shield.observe_markout_bps(markout_bps);
    }

    /// Current smoothed adverse-selection markout estimate (bps), if any fills observed.
    pub fn markout_estimate_bps(&self) -> Option<f64> {
        self.markout_shield.smoothed_markout_bps()
    }

    pub fn set_microprice_table(&mut self, table: Option<MarkovMicropriceTable>) -> Result<()> {
        Self::validate_microprice_table(table.as_ref())?;
        self.microprice = table;
        Ok(())
    }

    pub fn update_glft_intensity(&mut self, intensity_a: f64, decay_k: f64) -> Result<()> {
        self.quoter.update_intensity(intensity_a, decay_k)
    }

    pub fn upsert_wallet_tier(&mut self, wallet_key: u64, tier: WalletToxicityTier) -> Result<()> {
        self.toxicity.upsert_wallet_tier(wallet_key, tier)
    }

    pub fn observe_public_taker_sweep(
        &mut self,
        now_ms: u64,
        wallet_key: u64,
        taker_is_buy: bool,
        is_twap_sub: bool,
    ) -> Result<Option<QuoteSide>> {
        self.toxicity
            .observe_taker_sweep(now_ms, wallet_key, taker_is_buy, is_twap_sub)
    }

    pub fn notify_exit_partial_fill(&mut self, filled_qty: f64) -> Result<()> {
        self.hybrid_exit.notify_partial_fill(filled_qty)
    }

    /// Evaluate unified fair mid, oracle guard, dedicated exit slot, and gated opening quotes.
    pub fn evaluate(
        &mut self,
        market: &GenericMarketState,
        inv: &GenericInventoryState,
    ) -> Result<QuotePlan> {
        self.evaluate_with_leader(market, inv, None)
    }

    /// Evaluate unified fair mid (optionally blended with an external leader signal),
    /// oracle guard, dedicated reduce-only exit slot, and directional opening quotes.
    ///
    /// Performs ALL market, inventory, leader, and oracle validations BEFORE mutating
    /// internal exit-controller state so failed evaluations remain atomic.
    pub fn evaluate_with_leader(
        &mut self,
        market: &GenericMarketState,
        inv: &GenericInventoryState,
        leader: Option<&ExternalLeaderSignal>,
    ) -> Result<QuotePlan> {
        ensure!(market.now_ms > 0, "market.now_ms must be > 0");
        ensure!(
            market.bid_px.is_finite()
                && market.ask_px.is_finite()
                && market.bid_px > 0.0
                && market.ask_px > market.bid_px
                && market.bid_sz.is_finite()
                && market.bid_sz > 0.0
                && market.ask_sz.is_finite()
                && market.ask_sz > 0.0
                && market.tick_size.is_finite()
                && market.tick_size > 0.0
                && market.momentum_3s_bps.is_finite()
                && market
                    .funding_rate_bps_per_hour
                    .is_none_or(|r| r.is_finite())
                && market
                    .exit_queue_ahead_base
                    .is_none_or(|qa| qa.is_finite() && qa >= 0.0),
            "invalid market book inputs"
        );
        ensure!(
            inv.signed_inventory_base.is_finite()
                && inv.dust_base.is_finite()
                && inv.dust_base >= 0.0
                && inv.daily_loss_risk_scale.is_finite()
                && (0.0..=1.0).contains(&inv.daily_loss_risk_scale),
            "invalid inventory or drawdown risk scale"
        );

        let carrying_inventory = inv.signed_inventory_base.abs() > inv.dust_base;
        if carrying_inventory {
            let entry_ts = inv
                .entry_opened_at_ms
                .context("non-flat inventory requires entry_opened_at_ms")?;
            ensure!(
                entry_ts > 0
                    && market.now_ms >= entry_ts
                    && inv.avg_entry_price.is_finite()
                    && inv.avg_entry_price > 0.0,
                "non-flat inventory requires valid causal entry timestamp and positive entry price"
            );
        }

        let (lead_lag_shift_bps, leader_stale, leader_vetoed_bid, leader_vetoed_ask) =
            if self.config.lead_lag.enabled {
                if let Some(sig) = leader {
                    let ll = &self.config.lead_lag;
                    ensure!(
                        sig.reference_mid_px.is_finite() && sig.reference_mid_px > 0.0,
                        "leader.reference_mid_px must be finite and > 0"
                    );
                    ensure!(
                        sig.leader_return_bps.is_finite(),
                        "leader.leader_return_bps must be finite"
                    );
                    ensure!(
                        sig.receipt_ms > 0 && sig.receipt_ms <= market.now_ms,
                        "invalid or future leader.receipt_ms ({} vs {})",
                        sig.receipt_ms,
                        market.now_ms
                    );
                    ensure!(
                        sig.source_ms > 0
                            && sig.source_ms
                                <= market.now_ms.saturating_add(ll.max_leader_clock_skew_ms),
                        "invalid or future leader.source_ms ({} vs {})",
                        sig.source_ms,
                        market.now_ms
                    );

                    let receipt_stale =
                        market.now_ms.saturating_sub(sig.receipt_ms) > ll.max_leader_age_ms;
                    let source_stale = market.now_ms.saturating_sub(sig.source_ms)
                        > ll.max_leader_age_ms
                            .saturating_add(ll.max_leader_clock_skew_ms);
                    if receipt_stale || source_stale {
                        (0.0, true, false, false)
                    } else {
                        let shift = (ll.beta * sig.leader_return_bps)
                            .clamp(-ll.max_shift_bps, ll.max_shift_bps);
                        let veto_ask = sig.leader_return_bps >= ll.veto_threshold_bps;
                        let veto_bid = sig.leader_return_bps <= -ll.veto_threshold_bps;
                        (shift, false, veto_bid, veto_ask)
                    }
                } else {
                    (0.0, false, false, false)
                }
            } else {
                (0.0, false, false, false)
            };

        let raw_mid = 0.5 * (market.bid_px + market.ask_px);
        let base_fair_mid = self
            .microprice
            .as_ref()
            .and_then(|table| {
                table.microprice(
                    market.bid_px,
                    market.ask_px,
                    market.bid_sz,
                    market.ask_sz,
                    market.tick_size,
                )
            })
            .unwrap_or(raw_mid);
        let funding_shift_bps = if self.config.funding_bias.enabled
            && let Some(rate) = market.funding_rate_bps_per_hour
        {
            let fb = &self.config.funding_bias;
            (-rate * fb.reservation_shift_hours)
                .clamp(-fb.max_reservation_shift_bps, fb.max_reservation_shift_bps)
        } else {
            0.0
        };
        let fair_mid = base_fair_mid * (1.0 + (lead_lag_shift_bps + funding_shift_bps) / 10_000.0);
        ensure!(
            fair_mid.is_finite() && fair_mid > 0.0,
            "computed fair_mid must be finite and > 0"
        );

        let (oracle_divergence_bps, oracle_reduce_only) = match market.oracle_px {
            Some(opx) => {
                let (div_bps, act) = evaluate_reference_oracle_divergence_f64(
                    fair_mid,
                    opx,
                    self.config.max_oracle_divergence_bps,
                )?;
                (Some(div_bps), act == TailRiskAction::ReduceOnly)
            }
            None => (None, false),
        };

        let can_open = !oracle_reduce_only && inv.daily_loss_risk_scale > 0.0;
        let glft_q = if can_open {
            Some(self.quoter.quote(inv.signed_inventory_base)?)
        } else {
            None
        };

        let exit_queue_ahead = market.exit_queue_ahead_base.or_else(|| {
            (carrying_inventory && self.config.hybrid_exit.queue_exit.enabled).then_some(
                if inv.signed_inventory_base > 0.0 {
                    market.ask_sz
                } else {
                    market.bid_sz
                },
            )
        });
        let exit_directive = if carrying_inventory {
            let entry_ts = inv
                .entry_opened_at_ms
                .context("non-flat inventory requires entry_opened_at_ms")?;
            self.hybrid_exit.evaluate(ExitEvalInput {
                now_ms: market.now_ms,
                entry_opened_at_ms: entry_ts,
                signed_inventory_base: inv.signed_inventory_base,
                avg_entry_price: inv.avg_entry_price,
                fair_mid,
                bid_touch: market.bid_px,
                ask_touch: market.ask_px,
                tick_size: market.tick_size,
                momentum_3s_bps: market.momentum_3s_bps,
                dust_base: inv.dust_base,
                queue_ahead_base: exit_queue_ahead,
            })?
        } else {
            self.hybrid_exit.evaluate(ExitEvalInput {
                now_ms: market.now_ms,
                entry_opened_at_ms: market.now_ms,
                signed_inventory_base: 0.0,
                avg_entry_price: fair_mid,
                fair_mid,
                bid_touch: market.bid_px,
                ask_touch: market.ask_px,
                tick_size: market.tick_size,
                momentum_3s_bps: market.momentum_3s_bps,
                dust_base: inv.dust_base,
                queue_ahead_base: None,
            })?
        };

        let suppress_bid_open =
            (carrying_inventory && inv.signed_inventory_base < 0.0) || leader_vetoed_bid;
        let suppress_ask_open =
            (carrying_inventory && inv.signed_inventory_base > 0.0) || leader_vetoed_ask;

        let (bid_open, ask_open) = if let Some(glft_q) = glft_q {
            let bid = if suppress_bid_open {
                None
            } else {
                Some(self.build_opening_quote(
                    market,
                    inv.daily_loss_risk_scale,
                    inv.signed_inventory_base,
                    fair_mid,
                    QuoteSide::Bid,
                    glft_q.bid_offset_bps,
                ))
            };
            let ask = if suppress_ask_open {
                None
            } else {
                Some(self.build_opening_quote(
                    market,
                    inv.daily_loss_risk_scale,
                    inv.signed_inventory_base,
                    fair_mid,
                    QuoteSide::Ask,
                    glft_q.ask_offset_bps,
                ))
            };
            (bid, ask)
        } else {
            (None, None)
        };

        Ok(QuotePlan {
            mode: self.config.mode,
            fair_mid,
            lead_lag_shift_bps,
            funding_shift_bps,
            leader_stale,
            leader_vetoed_bid,
            leader_vetoed_ask,
            oracle_divergence_bps,
            oracle_reduce_only,
            bid_open,
            ask_open,
            exit_directive,
        })
    }

    fn build_opening_quote(
        &self,
        market: &GenericMarketState,
        risk_scale: f64,
        signed_inventory_base: f64,
        fair_mid: f64,
        side: QuoteSide,
        base_offset_bps: f64,
    ) -> OpeningQuote {
        let tox = self.toxicity.quote_adjustment(market.now_ms, side, false);
        // Online markout shield widens symmetrically and may decay size; it composes
        // multiplicatively with the wallet-tier toxicity adjustment. Disabled -> (1.0, 1.0).
        let (markout_mult, markout_size) = self.markout_shield.adjustment();
        let inv_clip_scale = if self.config.inventory_clip.enabled {
            let ic = &self.config.inventory_clip;
            let u =
                (signed_inventory_base.abs() / self.config.glft.max_inventory_base).clamp(0.0, 1.0);
            (1.0 - (1.0 - ic.min_clip_scale) * u.powf(ic.taper_power)).clamp(ic.min_clip_scale, 1.0)
        } else {
            1.0
        };
        let eff_offset_bps = base_offset_bps * tox.spread_multiplier * markout_mult;
        let frac = eff_offset_bps / 10_000.0;
        let tick = market.tick_size;
        let price = match side {
            QuoteSide::Bid => {
                let raw = fair_mid * (1.0 - frac);
                let cap = (market.ask_px - tick).max(tick);
                (((raw.min(cap) / tick) + 1e-9).floor().max(1.0) * tick).min(cap)
            }
            QuoteSide::Ask => {
                let raw = fair_mid * (1.0 + frac);
                let floor = market.bid_px + tick;
                (((raw.max(floor) / tick) - 1e-9).ceil().max(1.0) * tick).max(floor)
            }
        };
        let qty = self.config.base_order_qty
            * risk_scale
            * inv_clip_scale
            * tox.size_scale
            * markout_size;
        OpeningQuote {
            side,
            price,
            qty,
            spread_offset_bps: eff_offset_bps,
            faded_by_toxicity: tox.faded,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strategy::microprice::{MarkovBookObservation, MarkovMicropriceConfig};
    use crate::strategy::wallet_toxicity::salted_wallet_key;

    fn pipeline_cfg() -> QuotePipelineConfig {
        QuotePipelineConfig {
            mode: StrategyMode::NeutralMarketMaking,
            glft: GlftParams {
                gamma: 0.08,
                xi: 0.08,
                delta_qty: 1.0,
                intensity_a: 2.98,
                decay_k: 0.85,
                sigma_bps_per_sqrt_s: 1.335,
                max_inventory_base: 10.0,
                inventory_skew_scale: 1.0,
                fee_floor_bps: 0.40,
                max_half_spread_bps: 50.0,
            },
            wallet_toxicity: WalletToxicityConfig::default(),
            hybrid_exit: HybridExitConfig::default(),
            lead_lag: LeadLagConfig::default(),
            markout_shield: MarkoutShieldConfig::default(),
            funding_bias: FundingBiasConfig::default(),
            inventory_clip: InventoryClipConfig::default(),
            max_oracle_divergence_bps: 25.0,
            base_order_qty: 2.0,
        }
    }

    fn synth_microprice() -> MarkovMicropriceTable {
        let mut obs = Vec::new();
        let mut mid = 100.05_f64;
        let tick = 0.1_f64;
        for step in 0..40_u64 {
            let bid_px = ((mid - 0.5 * tick) / tick).round() * tick;
            let (bid_sz, ask_sz) = match step % 4 {
                0 | 1 => (9.0, 1.0),
                2 => (5.0, 5.0),
                _ => (1.0, 9.0),
            };
            obs.push(MarkovBookObservation {
                timestamp_ms: 1_000 + step * 50,
                bid_px,
                ask_px: bid_px + tick,
                bid_sz,
                ask_sz,
                tick_size: tick,
            });
            if step % 4 == 1 {
                mid += tick;
            } else if step % 4 == 3 {
                mid -= tick;
            }
        }
        MarkovMicropriceTable::fit(
            &MarkovMicropriceConfig {
                imbalance_buckets: 2,
                spread_buckets: 1,
                ..MarkovMicropriceConfig::default()
            },
            &obs,
        )
        .unwrap()
    }

    #[test]
    fn quote_pipeline_integrates_microprice_toxicity_oracle_and_reduce_only_exits() {
        let mut pipe = QuotePipeline::new(pipeline_cfg(), Some(synth_microprice())).unwrap();
        let d10 = salted_wallet_key(99, "toxic_taker");
        pipe.upsert_wallet_tier(d10, WalletToxicityTier::Decile(10))
            .unwrap();

        // Toxic buyer sweep at t=10_000 fades ONLY Ask opening quote while keeping Bid intact.
        pipe.observe_public_taker_sweep(10_000, d10, true, false)
            .unwrap();
        let market = GenericMarketState {
            now_ms: 10_200,
            bid_px: 100.0,
            ask_px: 100.1,
            bid_sz: 9.0,
            ask_sz: 1.0,
            tick_size: 0.1,
            momentum_3s_bps: 0.0,
            oracle_px: Some(100.05),
            funding_rate_bps_per_hour: None,
            exit_queue_ahead_base: None,
        };
        let flat = GenericInventoryState {
            signed_inventory_base: 0.0,
            avg_entry_price: 0.0,
            entry_opened_at_ms: None,
            dust_base: 1e-6,
            daily_loss_risk_scale: 1.0,
        };
        let p1 = pipe.evaluate(&market, &flat).unwrap();
        assert!(p1.fair_mid > 100.05); // microprice tilts up on bid-heavy book
        assert!(!p1.oracle_reduce_only);
        assert!(matches!(p1.exit_directive, ExitDirective::Flat));
        let bid_q = p1.bid_open.unwrap();
        let ask_q = p1.ask_open.unwrap();
        assert!(!bid_q.faded_by_toxicity && (bid_q.qty - 2.0).abs() < 1e-9);
        assert!(ask_q.faded_by_toxicity && (ask_q.qty - 0.5).abs() < 1e-9);

        // Carrying long inventory suppresses opening Ask (reducing side handled by HybridExitController).
        let long_inv = GenericInventoryState {
            signed_inventory_base: 3.0,
            avg_entry_price: 100.0,
            entry_opened_at_ms: Some(9_000),
            dust_base: 1e-6,
            daily_loss_risk_scale: 1.0,
        };
        let p_long = pipe.evaluate(&market, &long_inv).unwrap();
        assert!(p_long.bid_open.is_some());
        assert!(p_long.ask_open.is_none());
        assert!(matches!(
            p_long.exit_directive,
            ExitDirective::PassiveReduce(slot) if slot.reduce_only && !slot.is_buy
        ));

        // Oracle divergence > 25 bps drops opening quotes while KEEPING reduce-only exit alive!
        let mut diverged_market = market;
        diverged_market.oracle_px = Some(100.40);
        let p_div = pipe.evaluate(&diverged_market, &long_inv).unwrap();
        assert!(p_div.oracle_reduce_only);
        assert!(p_div.bid_open.is_none() && p_div.ask_open.is_none());
        assert!(matches!(
            p_div.exit_directive,
            ExitDirective::PassiveReduce(slot) if slot.reduce_only
        ));

        // Drawdown risk_scale == 0 also suppresses opening quotes while preserving exit_directive.
        let mut halted_inv = long_inv;
        halted_inv.daily_loss_risk_scale = 0.0;
        let p_halt = pipe.evaluate(&market, &halted_inv).unwrap();
        assert!(p_halt.bid_open.is_none() && p_halt.ask_open.is_none());
        assert!(matches!(
            p_halt.exit_directive,
            ExitDirective::PassiveReduce(slot) if slot.reduce_only
        ));
    }

    #[test]
    fn quote_pipeline_rejects_unconverged_microprice_table() {
        let mut alt_obs = Vec::new();
        for i in 0..7_u64 {
            let right = i % 2 == 1;
            let bid = if right { 101.0 } else { 100.0 };
            alt_obs.push(MarkovBookObservation {
                timestamp_ms: 1_000 + i * 100,
                bid_px: bid,
                ask_px: bid + 1.0,
                bid_sz: if right { 3.0 } else { 1.0 },
                ask_sz: if right { 1.0 } else { 3.0 },
                tick_size: 1.0,
            });
        }
        let unconverged = MarkovMicropriceTable::fit(
            &MarkovMicropriceConfig {
                imbalance_buckets: 2,
                spread_buckets: 1,
                max_iters: 64,
                tol: 1e-9,
                min_transitions_per_state: 2,
                require_convergence: false,
            },
            &alt_obs,
        )
        .unwrap();
        assert!(!unconverged.is_converged());

        assert!(QuotePipeline::new(pipeline_cfg(), Some(unconverged.clone())).is_err());

        let mut pipe = QuotePipeline::new(pipeline_cfg(), Some(synth_microprice())).unwrap();
        let market = GenericMarketState {
            now_ms: 10_200,
            bid_px: 100.0,
            ask_px: 100.1,
            bid_sz: 9.0,
            ask_sz: 1.0,
            tick_size: 0.1,
            momentum_3s_bps: 0.0,
            oracle_px: Some(100.05),
            funding_rate_bps_per_hour: None,
            exit_queue_ahead_base: None,
        };
        let flat = GenericInventoryState {
            signed_inventory_base: 0.0,
            avg_entry_price: 0.0,
            entry_opened_at_ms: None,
            dust_base: 1e-6,
            daily_loss_risk_scale: 1.0,
        };
        let orig_mid = pipe.evaluate(&market, &flat).unwrap().fair_mid;
        assert!(pipe.set_microprice_table(Some(unconverged)).is_err());
        assert_eq!(pipe.evaluate(&market, &flat).unwrap().fair_mid, orig_mid);

        // Legacy DTO missing `converged` must fail closed at both construction and swap,
        // preserving any previously loaded converged table.
        let valid_json = serde_json::to_value(&synth_microprice()).unwrap();
        let mut legacy_json = valid_json.clone();
        legacy_json.as_object_mut().unwrap().remove("converged");
        let legacy: MarkovMicropriceTable = serde_json::from_value(legacy_json).unwrap();
        assert!(QuotePipeline::new(pipeline_cfg(), Some(legacy.clone())).is_err());
        assert!(pipe.set_microprice_table(Some(legacy)).is_err());
        assert_eq!(pipe.evaluate(&market, &flat).unwrap().fair_mid, orig_mid);

        // Loose-tolerance serialized witness (`convergence_tol: 0.5, residual_inf_norm: 0.25`)
        // must also be rejected at the production consumer boundary while preserving prior state.
        let mut loose_json = valid_json;
        loose_json["convergence_tol"] = serde_json::json!(0.5);
        loose_json["residual_inf_norm"] = serde_json::json!(0.25);
        let loose: MarkovMicropriceTable = serde_json::from_value(loose_json).unwrap();
        assert!(loose.is_converged());
        assert!(QuotePipeline::new(pipeline_cfg(), Some(loose.clone())).is_err());
        assert!(pipe.set_microprice_table(Some(loose)).is_err());
        assert_eq!(pipe.evaluate(&market, &flat).unwrap().fair_mid, orig_mid);

        assert!(pipe.set_microprice_table(None).is_ok());
    }

    #[test]
    fn quote_pipeline_lead_lag_shifts_fair_mid_vetoes_exposed_open_and_preserves_exits() {
        let mut cfg = pipeline_cfg();
        cfg.lead_lag = LeadLagConfig {
            enabled: true,
            beta: 0.5,
            max_shift_bps: 10.0,
            veto_threshold_bps: 3.0,
            max_leader_age_ms: 250,
            max_leader_clock_skew_ms: 2_000,
        };
        let mut pipe = QuotePipeline::new(cfg, None).unwrap();
        let market = GenericMarketState {
            now_ms: 10_200,
            bid_px: 100.0,
            ask_px: 100.1,
            bid_sz: 5.0,
            ask_sz: 5.0,
            tick_size: 0.1,
            momentum_3s_bps: 0.0,
            oracle_px: None,
            funding_rate_bps_per_hour: None,
            exit_queue_ahead_base: None,
        };
        let flat = GenericInventoryState {
            signed_inventory_base: 0.0,
            avg_entry_price: 0.0,
            entry_opened_at_ms: None,
            dust_base: 1e-6,
            daily_loss_risk_scale: 1.0,
        };

        // Strong positive leader jump shifts fair mid (+10 bps clamped) and vetoes Ask open.
        let jump_up = ExternalLeaderSignal {
            leader_venue_id: 2,
            reference_mid_px: 100.30,
            leader_return_bps: 30.0,
            source_ms: 10_180,
            receipt_ms: 10_190,
        };
        let p_up = pipe
            .evaluate_with_leader(&market, &flat, Some(&jump_up))
            .unwrap();
        assert!(!p_up.leader_stale);
        assert!((p_up.lead_lag_shift_bps - 10.0).abs() < 1e-12);
        assert!((p_up.fair_mid - 100.05 * 1.001).abs() < 1e-9);
        assert!(!p_up.leader_vetoed_bid);
        assert!(p_up.leader_vetoed_ask);
        assert!(p_up.bid_open.is_some());
        assert!(p_up.ask_open.is_none());

        // Strong negative leader drop while short preserves reduce-only BUY exit while vetoing Bid open.
        let short_inv = GenericInventoryState {
            signed_inventory_base: -2.0,
            avg_entry_price: 100.05,
            entry_opened_at_ms: Some(9_000),
            dust_base: 1e-6,
            daily_loss_risk_scale: 1.0,
        };
        let drop_dn = ExternalLeaderSignal {
            leader_venue_id: 2,
            reference_mid_px: 99.95,
            leader_return_bps: -5.0,
            source_ms: 10_180,
            receipt_ms: 10_190,
        };
        let p_dn = pipe
            .evaluate_with_leader(&market, &short_inv, Some(&drop_dn))
            .unwrap();
        assert!(p_dn.leader_vetoed_bid);
        assert!(p_dn.bid_open.is_none());
        assert!(matches!(
            p_dn.exit_directive,
            ExitDirective::PassiveReduce(slot) if slot.reduce_only && slot.is_buy
        ));

        // Stale leader signal (age 300ms > 250ms) yields zero shift and no veto.
        let stale = ExternalLeaderSignal {
            leader_venue_id: 2,
            reference_mid_px: 100.30,
            leader_return_bps: 30.0,
            source_ms: 9_890,
            receipt_ms: 9_900,
        };
        let p_stale = pipe
            .evaluate_with_leader(&market, &flat, Some(&stale))
            .unwrap();
        assert!(p_stale.leader_stale);
        assert_eq!(p_stale.lead_lag_shift_bps, 0.0);
        assert!(!p_stale.leader_vetoed_bid && !p_stale.leader_vetoed_ask);
        assert!(p_stale.bid_open.is_some() && p_stale.ask_open.is_some());

        // Future receipt_ms or non-finite leader return fails closed atomically without mutating state.
        let future_sig = ExternalLeaderSignal {
            leader_venue_id: 2,
            reference_mid_px: 100.30,
            leader_return_bps: 10.0,
            source_ms: 10_200,
            receipt_ms: 10_201,
        };
        assert!(
            pipe.evaluate_with_leader(&market, &short_inv, Some(&future_sig))
                .is_err()
        );
        let mut nan_sig = jump_up;
        nan_sig.leader_return_bps = f64::NAN;
        assert!(
            pipe.evaluate_with_leader(&market, &short_inv, Some(&nan_sig))
                .is_err()
        );

        let invalid_shift = LeadLagConfig {
            max_shift_bps: 10_000.0,
            ..LeadLagConfig::default()
        };
        assert!(invalid_shift.validate().is_err());
    }

    #[test]
    fn markout_shield_widens_and_decays_only_when_enabled_and_adverse() {
        let market = GenericMarketState {
            now_ms: 10_000,
            bid_px: 100.0,
            ask_px: 100.1,
            bid_sz: 5.0,
            ask_sz: 5.0,
            tick_size: 0.1,
            momentum_3s_bps: 0.0,
            oracle_px: None,
            funding_rate_bps_per_hour: None,
            exit_queue_ahead_base: None,
        };
        let flat = GenericInventoryState {
            signed_inventory_base: 0.0,
            avg_entry_price: 0.0,
            entry_opened_at_ms: None,
            dust_base: 1e-6,
            daily_loss_risk_scale: 1.0,
        };

        // Disabled shield (default): adverse markout observations have ZERO effect on quotes.
        let mut off = QuotePipeline::new(pipeline_cfg(), None).unwrap();
        let base = off.evaluate(&market, &flat).unwrap();
        let base_ask = base.ask_open.unwrap();
        for _ in 0..20 {
            off.observe_fill_markout_bps(-5.0); // strongly adverse
        }
        let after = off.evaluate(&market, &flat).unwrap();
        let after_ask = after.ask_open.unwrap();
        assert!((after_ask.spread_offset_bps - base_ask.spread_offset_bps).abs() < 1e-9);
        assert!((after_ask.qty - base_ask.qty).abs() < 1e-9);

        // Enabled shield: sustained adverse markout widens spread and decays size; a benign
        // (positive) markout stream keeps quotes at baseline.
        let mut cfg = pipeline_cfg();
        cfg.markout_shield = MarkoutShieldConfig {
            enabled: true,
            ..MarkoutShieldConfig::default()
        };
        let mut on = QuotePipeline::new(cfg.clone(), None).unwrap();
        let on_base = on.evaluate(&market, &flat).unwrap();
        let on_base_ask = on_base.ask_open.unwrap();

        // Benign fills: no engagement.
        for _ in 0..20 {
            on.observe_fill_markout_bps(2.0);
        }
        let benign = on.evaluate(&market, &flat).unwrap();
        assert!(
            (benign.ask_open.unwrap().spread_offset_bps - on_base_ask.spread_offset_bps).abs()
                < 1e-9
        );

        // Adverse fills past threshold: wider spread AND smaller size on both sides.
        for _ in 0..20 {
            on.observe_fill_markout_bps(-5.0);
        }
        assert!(on.markout_estimate_bps().unwrap() < -1.0);
        let adverse = on.evaluate(&market, &flat).unwrap();
        let adv_ask = adverse.ask_open.unwrap();
        let adv_bid = adverse.bid_open.unwrap();
        assert!(adv_ask.spread_offset_bps > on_base_ask.spread_offset_bps + 1e-6);
        assert!(adv_bid.qty < on_base.bid_open.unwrap().qty - 1e-9);

        // Invalid shield config fails closed at construction.
        let mut bad = pipeline_cfg();
        bad.markout_shield = MarkoutShieldConfig {
            enabled: true,
            saturation_bps: 0.1,
            widen_threshold_bps: 0.8,
            ..MarkoutShieldConfig::default()
        };
        assert!(bad.validate().is_err());
    }

    #[test]
    fn funding_bias_inventory_clip_and_queue_exit_only_engage_when_enabled() {
        let mut cfg = pipeline_cfg();
        cfg.funding_bias = FundingBiasConfig {
            enabled: true,
            reservation_shift_hours: 1.0,
            max_reservation_shift_bps: 5.0,
        };
        cfg.inventory_clip = InventoryClipConfig {
            enabled: true,
            min_clip_scale: 0.25,
            taper_power: 1.0,
        };
        cfg.hybrid_exit.queue_exit.enabled = true;
        cfg.hybrid_exit.queue_exit.max_expected_fill_sec = 5.0;
        let mut pipe = QuotePipeline::new(cfg, None).unwrap();

        let market = GenericMarketState {
            now_ms: 10_500,
            bid_px: 100.0,
            ask_px: 100.1,
            bid_sz: 5.0,
            ask_sz: 5.0,
            tick_size: 0.1,
            momentum_3s_bps: 0.0,
            oracle_px: None,
            funding_rate_bps_per_hour: Some(20.0), // clamped to -5 bp reservation shift
            exit_queue_ahead_base: Some(20.0),     // 20 / 2.0 = 10s > 5s -> advances exit stage
        };
        let half_inv = GenericInventoryState {
            signed_inventory_base: 5.0, // 50% of max_inventory_base=10 -> clip_scale = 0.625
            avg_entry_price: 100.05,
            entry_opened_at_ms: Some(10_000),
            dust_base: 1e-6,
            daily_loss_risk_scale: 1.0,
        };

        let plan = pipe.evaluate(&market, &half_inv).unwrap();
        assert!((plan.funding_shift_bps - (-5.0)).abs() < 1e-12);
        assert!((plan.fair_mid - 100.05 * (1.0 - 5.0 / 10_000.0)).abs() < 1e-9);
        let bid = plan.bid_open.unwrap();
        assert!((bid.qty - 2.0 * 0.625).abs() < 1e-9);
        assert!(matches!(
            plan.exit_directive,
            ExitDirective::PassiveReduce(slot) if slot.stage == crate::strategy::hybrid_exit::PassiveExitStage::Stage2Mid
        ));

        // Combined enabled lead_lag + funding_bias shift >= 10000 bps must be rejected.
        let mut combined_bad = pipeline_cfg();
        combined_bad.lead_lag.enabled = true;
        combined_bad.lead_lag.max_shift_bps = 6_000.0;
        combined_bad.funding_bias.enabled = true;
        combined_bad.funding_bias.max_reservation_shift_bps = 4_000.0;
        assert!(combined_bad.validate().is_err());

        combined_bad.lead_lag.enabled = false;
        assert!(combined_bad.validate().is_ok());

        combined_bad.lead_lag.enabled = true;
        combined_bad.funding_bias.max_reservation_shift_bps = 3_999.0;
        assert!(combined_bad.validate().is_ok());
    }
}
