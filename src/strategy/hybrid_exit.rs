//! Standalone queue-preserving hybrid exit controller and full-cycle FIFO paired
//! cash ledger (`S1-1 / S1-2`, separate from hash-locked `inventory_neutral_mm.rs`).
//!
//! Resolves the two structural failures documented in the 2026-11-25 audit:
//! 1. Decouples reducing exit slots from opening slots with strict `reduce_only = true`,
//!    anti-cross-zero size clamping, and explicit exemptions from `placement_bounds`
//!    and `CRASH-IMP-01` drawdown scaling so passive exits never deadlock until `50 s` timeout.
//! 2. Replaces short-horizon 1s markout optimization with FIFO lot-level full-cycle paired
//!    cash attribution (`spread_capture + holding_drift + passive_exit + taker_slippage
//!    + funding_cash - fees`), explicitly tracking `markout_vs_cash_divergence_usd`.
//!
//! Funding is accrued through [`InventoryCycleCashLedger::record_funding`] as signed cash
//! (positive = received), because perp funding settles on the venue's funding interval
//! independently of fills. It is folded into `paired_cycle_net_cash_usd` so that nominal
//! turnover cannot mask a position that is flat on price but bleeding (or earning) funding.
//! This accrues net funding over the ledger's life against the open inventory; per-lot
//! holding-period funding attribution requires fill/funding timestamps and is deferred.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

/// Discrete passive unwind stage tuned to jump only on elapsed time thresholds,
/// avoiding tick-by-tick repricing that destroys FIFO queue priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PassiveExitStage {
    Stage1Wide,
    Stage2Mid,
    Stage3Touch,
}

/// Configuration for queue-position-aware exit escalation. Default `enabled = false`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct QueueExitConfig {
    pub enabled: bool,
    /// Estimated passive fill rate at touch in base units per second (`> 0`).
    pub fill_intensity_base_per_sec: f64,
    /// Advance passive exit stage one step if `queue_ahead / intensity` exceeds this threshold (`> 0`).
    pub max_expected_fill_sec: f64,
    /// Multiple of `max_expected_fill_sec` triggering immediate reduce-only IOC when unrealized PnL < 0 (`> 1`).
    pub urgent_ioc_fill_sec_mult: f64,
}

impl Default for QueueExitConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            fill_intensity_base_per_sec: 2.0,
            max_expected_fill_sec: 6.0,
            urgent_ioc_fill_sec_mult: 2.5,
        }
    }
}

impl QueueExitConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.fill_intensity_base_per_sec.is_finite() && self.fill_intensity_base_per_sec > 0.0,
            "queue_exit.fill_intensity_base_per_sec must be finite and > 0"
        );
        ensure!(
            self.max_expected_fill_sec.is_finite() && self.max_expected_fill_sec > 0.0,
            "queue_exit.max_expected_fill_sec must be finite and > 0"
        );
        ensure!(
            self.urgent_ioc_fill_sec_mult.is_finite() && self.urgent_ioc_fill_sec_mult > 1.0,
            "queue_exit.urgent_ioc_fill_sec_mult must be finite and > 1.0"
        );
        Ok(())
    }
}

/// Configuration for [`HybridExitController`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HybridExitConfig {
    /// End of stage 1 passive holding window (`ms`, default `4_000`).
    pub stage_1_max_hold_ms: u64,
    /// End of stage 2 passive holding window (`ms`, default `12_000`).
    pub stage_2_max_hold_ms: u64,
    /// Passive exit offset from fair mid in stage 1 (`bps`, default `0.45`).
    pub stage_1_offset_bps: f64,
    /// Passive exit offset from fair mid in stage 2 (`bps`, default `0.15`).
    pub stage_2_offset_bps: f64,
    /// Passive exit offset from BBO touch in stage 3 (`bps`, default `0.02`, rounded >= 1 tick).
    pub stage_3_offset_bps: f64,
    /// Minimum age before the early adverse-drift hazard boundary can fire (`ms`, default `2_000`).
    pub early_hazard_min_hold_ms: u64,
    /// Unrealized PnL trigger (`<=` negative bps, default `-2.5`).
    pub early_hazard_unrealized_pnl_bps: f64,
    /// 3-second adverse momentum trigger (`<=` negative bps in inventory loss direction, default `-1.0`).
    pub early_hazard_momentum_3s_bps: f64,
    /// Hard maximum holding timeout forcing reduce-only flatten (`ms`, default `50_000`).
    pub timeout_ms: u64,
    /// Protective IOC slippage buffer used for hazard hedge locks and timeouts (`bps`).
    pub hedge_ioc_slippage_bps: f64,
    /// Always exempt reducing slots from `placement_bounds` truncation.
    pub exempt_reduce_only_from_placement_bounds: bool,
    /// Always exempt reducing slots from drawdown risk scaling (`CRASH-IMP-01`).
    pub exempt_reduce_only_from_drawdown_brake: bool,
    #[serde(default)]
    pub queue_exit: QueueExitConfig,
    /// When `true`, mild early-hazard triggers escalate first to `Stage3Touch` passive
    /// `reduce_only` quoting (0 taker fee) instead of immediate taker IOC, reserving IOC
    /// for `timeout_ms` or `catastrophe_ioc_unrealized_pnl_bps`. Default `false`.
    #[serde(default)]
    pub passive_touch_before_hazard_ioc: bool,
    /// Optional severe unrealized loss threshold (`< early_hazard_unrealized_pnl_bps`) that
    /// bypasses passive-touch escalation and forces immediate taker IOC unwind.
    #[serde(default)]
    pub catastrophe_ioc_unrealized_pnl_bps: Option<f64>,
}

impl Default for HybridExitConfig {
    fn default() -> Self {
        Self {
            stage_1_max_hold_ms: 4_000,
            stage_2_max_hold_ms: 12_000,
            stage_1_offset_bps: 0.45,
            stage_2_offset_bps: 0.15,
            stage_3_offset_bps: 0.02,
            early_hazard_min_hold_ms: 2_000,
            early_hazard_unrealized_pnl_bps: -2.5,
            early_hazard_momentum_3s_bps: -1.0,
            timeout_ms: 50_000,
            hedge_ioc_slippage_bps: 5.0,
            exempt_reduce_only_from_placement_bounds: true,
            exempt_reduce_only_from_drawdown_brake: true,
            queue_exit: QueueExitConfig::default(),
            passive_touch_before_hazard_ioc: false,
            catastrophe_ioc_unrealized_pnl_bps: None,
        }
    }
}

impl HybridExitConfig {
    /// Fail closed on invalid stage ordering, thresholds, or missing reduce-only exemptions.
    pub fn validate(&self) -> Result<()> {
        self.queue_exit.validate()?;
        ensure!(
            self.stage_1_max_hold_ms > 0
                && self.stage_2_max_hold_ms > self.stage_1_max_hold_ms
                && self.timeout_ms > self.stage_2_max_hold_ms,
            "exit stage thresholds must satisfy 0 < stage_1 < stage_2 < timeout_ms"
        );
        ensure!(
            self.early_hazard_min_hold_ms > 0 && self.early_hazard_min_hold_ms < self.timeout_ms,
            "early_hazard_min_hold_ms must be in (0, timeout_ms)"
        );
        ensure!(
            self.stage_1_offset_bps.is_finite()
                && self.stage_2_offset_bps.is_finite()
                && self.stage_3_offset_bps.is_finite()
                && self.stage_1_offset_bps >= self.stage_2_offset_bps
                && self.stage_2_offset_bps >= self.stage_3_offset_bps
                && self.stage_3_offset_bps >= 0.0,
            "passive exit offsets must be finite and monotonically non-increasing >= 0"
        );
        ensure!(
            self.early_hazard_unrealized_pnl_bps.is_finite()
                && self.early_hazard_unrealized_pnl_bps < 0.0
                && self.early_hazard_momentum_3s_bps.is_finite()
                && self.early_hazard_momentum_3s_bps < 0.0,
            "early hazard triggers must be strictly negative and finite"
        );
        ensure!(
            self.catastrophe_ioc_unrealized_pnl_bps
                .is_none_or(|c| c.is_finite() && c < self.early_hazard_unrealized_pnl_bps),
            "catastrophe_ioc_unrealized_pnl_bps must be finite and < early_hazard_unrealized_pnl_bps"
        );
        ensure!(
            self.hedge_ioc_slippage_bps.is_finite()
                && self.hedge_ioc_slippage_bps >= 0.0
                && self.hedge_ioc_slippage_bps <= 100.0,
            "hedge_ioc_slippage_bps must be in [0, 100]"
        );
        ensure!(
            self.exempt_reduce_only_from_placement_bounds
                && self.exempt_reduce_only_from_drawdown_brake,
            "reduce_only exit slots must remain exempt from placement bounds and drawdown brake"
        );
        Ok(())
    }
}

/// Separate dedicated exit order slot decoupled from opening quotes.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ExitOrderSlot {
    pub reduce_only: bool,
    pub is_buy: bool,
    pub price: f64,
    pub qty: f64,
    pub stage: PassiveExitStage,
    /// `false` when staying inside the same discrete time stage & direction to protect queue priority.
    pub reprice_required: bool,
    pub exempt_from_placement_bounds: bool,
    pub exempt_from_drawdown_brake: bool,
}

/// Action emitted by [`HybridExitController::evaluate`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum ExitDirective {
    Flat,
    PassiveReduce(ExitOrderSlot),
    EarlyHazardHedgeLock {
        reduce_only: bool,
        hedge_is_buy: bool,
        qty: f64,
        limit_price: f64,
        unrealized_pnl_bps: f64,
        momentum_3s_bps: f64,
    },
    TimeoutFlatten {
        reduce_only: bool,
        is_buy: bool,
        qty: f64,
        limit_price: f64,
        hold_ms: u64,
    },
}

/// Market & inventory snapshot passed into [`HybridExitController::evaluate`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ExitEvalInput {
    pub now_ms: u64,
    pub entry_opened_at_ms: u64,
    pub signed_inventory_base: f64,
    pub avg_entry_price: f64,
    pub fair_mid: f64,
    pub bid_touch: f64,
    pub ask_touch: f64,
    pub tick_size: f64,
    pub momentum_3s_bps: f64,
    pub dust_base: f64,
    pub queue_ahead_base: Option<f64>,
}

/// Stateful exit slot supervisor enforcing queue preservation and hazard boundaries.
#[derive(Debug, Clone)]
pub struct HybridExitController {
    config: HybridExitConfig,
    active_slot: Option<ExitOrderSlot>,
}

impl HybridExitController {
    pub fn new(config: HybridExitConfig) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            config,
            active_slot: None,
        })
    }

    pub fn active_slot(&self) -> Option<&ExitOrderSlot> {
        self.active_slot.as_ref()
    }

    /// Shrink the active passive exit slot in-place when a partial fill executes,
    /// preserving FIFO queue rank (`reprice_required = false`) while remaining size > 0.
    pub fn notify_partial_fill(&mut self, filled_qty: f64) -> Result<()> {
        ensure!(
            filled_qty.is_finite() && filled_qty >= 0.0,
            "filled_qty must be finite and >= 0"
        );
        if let Some(slot) = self.active_slot.as_mut() {
            let remaining = slot.qty - filled_qty;
            if remaining <= 1e-12 {
                self.active_slot = None;
            } else {
                slot.qty = remaining;
                slot.reprice_required = false;
            }
        }
        Ok(())
    }

    /// Evaluate dedicated reduce-only exit directive without truncating by `placement_bounds`
    /// or `drawdown_brake`, and without repricing mid-stage so FIFO queue rank stays intact.
    pub fn evaluate(&mut self, input: ExitEvalInput) -> Result<ExitDirective> {
        ensure!(
            input.now_ms > 0
                && input.fair_mid.is_finite()
                && input.fair_mid > 0.0
                && input.bid_touch.is_finite()
                && input.bid_touch > 0.0
                && input.ask_touch.is_finite()
                && input.ask_touch > input.bid_touch
                && input.tick_size.is_finite()
                && input.tick_size > 0.0
                && input.signed_inventory_base.is_finite()
                && input.momentum_3s_bps.is_finite()
                && input.dust_base.is_finite()
                && input.dust_base >= 0.0
                && input
                    .queue_ahead_base
                    .is_none_or(|qa| qa.is_finite() && qa >= 0.0),
            "invalid exit evaluation inputs"
        );

        let abs_inv = input.signed_inventory_base.abs();
        if abs_inv <= input.dust_base {
            self.active_slot = None;
            return Ok(ExitDirective::Flat);
        }

        ensure!(
            input.entry_opened_at_ms > 0
                && input.now_ms >= input.entry_opened_at_ms
                && input.avg_entry_price.is_finite()
                && input.avg_entry_price > 0.0,
            "non-flat exit evaluation requires valid causal entry timestamp and positive entry price"
        );

        let inv_long = input.signed_inventory_base > 0.0;
        let is_buy = !inv_long;
        let hold_ms = input.now_ms - input.entry_opened_at_ms;
        let slip = self.config.hedge_ioc_slippage_bps / 10_000.0;

        if hold_ms >= self.config.timeout_ms {
            self.active_slot = None;
            let raw_limit = if is_buy {
                input.ask_touch * (1.0 + slip)
            } else {
                input.bid_touch * (1.0 - slip)
            };
            let limit_price = quantize_price(raw_limit, input.tick_size, is_buy);
            return Ok(ExitDirective::TimeoutFlatten {
                reduce_only: true,
                is_buy,
                qty: abs_inv,
                limit_price,
                hold_ms,
            });
        }

        let signed_unrealized_bps = if inv_long {
            ((input.fair_mid - input.avg_entry_price) / input.avg_entry_price) * 10_000.0
        } else {
            ((input.avg_entry_price - input.fair_mid) / input.avg_entry_price) * 10_000.0
        };
        let adverse_momentum_bps = if inv_long {
            input.momentum_3s_bps
        } else {
            -input.momentum_3s_bps
        };

        let early_hazard_triggered = hold_ms >= self.config.early_hazard_min_hold_ms
            && signed_unrealized_bps <= self.config.early_hazard_unrealized_pnl_bps
            && adverse_momentum_bps <= self.config.early_hazard_momentum_3s_bps;
        let catastrophe_breached = self
            .config
            .catastrophe_ioc_unrealized_pnl_bps
            .is_some_and(|cat| signed_unrealized_bps <= cat);

        let mut forced_stage3_passive = false;
        if early_hazard_triggered {
            if !self.config.passive_touch_before_hazard_ioc || catastrophe_breached {
                self.active_slot = None;
                let raw_limit = if is_buy {
                    input.ask_touch * (1.0 + slip)
                } else {
                    input.bid_touch * (1.0 - slip)
                };
                let limit_price = quantize_price(raw_limit, input.tick_size, is_buy);
                return Ok(ExitDirective::EarlyHazardHedgeLock {
                    reduce_only: true,
                    hedge_is_buy: is_buy,
                    qty: abs_inv,
                    limit_price,
                    unrealized_pnl_bps: signed_unrealized_bps,
                    momentum_3s_bps: adverse_momentum_bps,
                });
            }
            forced_stage3_passive = true;
        }

        let mut stage = if forced_stage3_passive {
            PassiveExitStage::Stage3Touch
        } else if hold_ms < self.config.stage_1_max_hold_ms {
            PassiveExitStage::Stage1Wide
        } else if hold_ms < self.config.stage_2_max_hold_ms {
            PassiveExitStage::Stage2Mid
        } else {
            PassiveExitStage::Stage3Touch
        };
        if self.config.queue_exit.enabled
            && let Some(queue_ahead) = input.queue_ahead_base
        {
            let expected_fill_s = queue_ahead / self.config.queue_exit.fill_intensity_base_per_sec;
            let urgent_limit_s = self.config.queue_exit.max_expected_fill_sec
                * self.config.queue_exit.urgent_ioc_fill_sec_mult;
            if expected_fill_s > urgent_limit_s && signed_unrealized_bps < 0.0 {
                self.active_slot = None;
                let raw_limit = if is_buy {
                    input.ask_touch * (1.0 + slip)
                } else {
                    input.bid_touch * (1.0 - slip)
                };
                let limit_price = quantize_price(raw_limit, input.tick_size, is_buy);
                return Ok(ExitDirective::EarlyHazardHedgeLock {
                    reduce_only: true,
                    hedge_is_buy: is_buy,
                    qty: abs_inv,
                    limit_price,
                    unrealized_pnl_bps: signed_unrealized_bps,
                    momentum_3s_bps: adverse_momentum_bps,
                });
            }
            if expected_fill_s > self.config.queue_exit.max_expected_fill_sec {
                stage = match stage {
                    PassiveExitStage::Stage1Wide => PassiveExitStage::Stage2Mid,
                    PassiveExitStage::Stage2Mid | PassiveExitStage::Stage3Touch => {
                        PassiveExitStage::Stage3Touch
                    }
                };
            }
        }

        // Preserve FIFO queue priority ONLY if already resting on the exact same passive
        // stage, direction, size, AND strictly inside the opposite touch (never crossed).
        let slot_not_crossed = self.active_slot.is_some_and(|prev| {
            (is_buy && prev.price < input.ask_touch - 1e-12)
                || (!is_buy && prev.price > input.bid_touch + 1e-12)
        });
        if let Some(prev) = self.active_slot
            && slot_not_crossed
            && prev.stage == stage
            && prev.is_buy == is_buy
            && prev.qty <= abs_inv + 1e-12
            && (prev.qty - abs_inv).abs() <= 1e-9
        {
            let kept = ExitOrderSlot {
                reprice_required: false,
                ..prev
            };
            self.active_slot = Some(kept);
            return Ok(ExitDirective::PassiveReduce(kept));
        }

        let raw_px = match stage {
            PassiveExitStage::Stage1Wide => {
                let off = self.config.stage_1_offset_bps / 10_000.0;
                if is_buy {
                    (input.fair_mid * (1.0 - off)).min(input.ask_touch - input.tick_size)
                } else {
                    (input.fair_mid * (1.0 + off)).max(input.bid_touch + input.tick_size)
                }
            }
            PassiveExitStage::Stage2Mid => {
                let off = self.config.stage_2_offset_bps / 10_000.0;
                if is_buy {
                    (input.fair_mid * (1.0 - off)).min(input.ask_touch - input.tick_size)
                } else {
                    (input.fair_mid * (1.0 + off)).max(input.bid_touch + input.tick_size)
                }
            }
            PassiveExitStage::Stage3Touch => {
                let off = self.config.stage_3_offset_bps / 10_000.0;
                if is_buy {
                    // Join or step at least 1 tick inside opposing ask without crossing (PostOnly).
                    (input.bid_touch * (1.0 + off)).min(input.ask_touch - input.tick_size)
                } else {
                    (input.ask_touch * (1.0 - off)).max(input.bid_touch + input.tick_size)
                }
            }
        };
        let safe_px = quantize_passive_post_only(
            raw_px,
            is_buy,
            input.bid_touch,
            input.ask_touch,
            input.tick_size,
        );

        let slot = ExitOrderSlot {
            reduce_only: true,
            is_buy,
            price: safe_px,
            qty: abs_inv,
            stage,
            reprice_required: true,
            exempt_from_placement_bounds: self.config.exempt_reduce_only_from_placement_bounds,
            exempt_from_drawdown_brake: self.config.exempt_reduce_only_from_drawdown_brake,
        };
        self.active_slot = Some(slot);
        Ok(ExitDirective::PassiveReduce(slot))
    }
}

fn quantize_price(px: f64, tick: f64, is_buy: bool) -> f64 {
    let safe = px.max(tick);
    if is_buy {
        ((safe / tick) - 1e-9).ceil().max(1.0) * tick
    } else {
        ((safe / tick) + 1e-9).floor().max(1.0) * tick
    }
}

fn quantize_passive_post_only(
    raw: f64,
    is_buy: bool,
    bid_touch: f64,
    ask_touch: f64,
    tick: f64,
) -> f64 {
    if is_buy {
        let cap = (ask_touch - tick).max(tick);
        let rounded = ((raw.min(cap) / tick) + 1e-9).floor().max(1.0) * tick;
        rounded.min(cap)
    } else {
        let floor = bid_touch + tick;
        let rounded = ((raw.max(floor) / tick) - 1e-9).ceil().max(1.0) * tick;
        rounded.max(floor)
    }
}

#[derive(Debug, Clone, Copy)]
struct OpenLot {
    long_sign: bool,
    qty: f64,
    fill_price: f64,
    fair_mid_at_entry: f64,
    remaining_entry_fee_usd: f64,
}

/// Liquidity classification when attributing a full inventory cycle's realized cash.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FillExitMode {
    OpeningOrAdding,
    PassiveReduceOnly,
    TakerOrHazardClose,
}

/// Fill input passed into [`InventoryCycleCashLedger::record_fill`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CycleFillInput {
    pub is_buy: bool,
    pub qty: f64,
    pub fill_price: f64,
    pub fair_mid: f64,
    pub fee_usd: f64,
    pub exit_mode: FillExitMode,
    pub markout_1s_usd: Option<f64>,
}

/// Summary of paired inventory cycle cash vs short-horizon 1s markout.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct CycleCashSummary {
    pub spread_capture_usd: f64,
    pub holding_drift_usd: f64,
    pub passive_exit_pnl_usd: f64,
    pub taker_exit_slippage_usd: f64,
    pub explicit_fees_usd: f64,
    pub paired_cycle_fees_usd: f64,
    /// Net funding cash accrued over the ledger's life (positive = received, negative = paid).
    pub funding_cash_usd: f64,
    pub paired_cycle_net_cash_usd: f64,
    pub cumulative_1s_markout_usd: f64,
    pub markout_vs_cash_divergence_usd: f64,
    pub closed_cycles: u64,
    pub passive_exit_notional_usd: f64,
    pub taker_exit_notional_usd: f64,
    pub open_inventory_base: f64,
}

/// FIFO lot-paired cash ledger separating micro-spread capture, drift, passive exit,
/// discrete taker/hedge slippage, pro-rata paired cycle fees, and 1-second markout divergence.
#[derive(Debug, Clone, Default)]
pub struct InventoryCycleCashLedger {
    lots: VecDeque<OpenLot>,
    summary: CycleCashSummary,
}

impl InventoryCycleCashLedger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn summary(&self) -> CycleCashSummary {
        self.summary
    }

    /// Record one executed fill alongside contemporaneous fair mid and optional 1s markout USD.
    pub fn record_fill(&mut self, fill: CycleFillInput) -> Result<()> {
        ensure!(
            fill.qty.is_finite()
                && fill.qty > 0.0
                && fill.fill_price.is_finite()
                && fill.fill_price > 0.0
                && fill.fair_mid.is_finite()
                && fill.fair_mid > 0.0
                && fill.fee_usd.is_finite(),
            "invalid cycle fill accounting inputs"
        );
        if let Some(m) = fill.markout_1s_usd {
            ensure!(m.is_finite(), "markout_1s_usd must be finite");
            self.summary.cumulative_1s_markout_usd += m;
        }
        self.summary.explicit_fees_usd += fill.fee_usd;

        let mut remaining = fill.qty;
        while remaining > 1e-12 {
            let Some(mut front) = self.lots.pop_front() else {
                break;
            };
            if front.long_sign == fill.is_buy {
                self.lots.push_front(front);
                break;
            }

            let matched = remaining.min(front.qty);
            let sign = if front.long_sign { 1.0 } else { -1.0 };

            // 1. Entry half-spread captured vs fair mid at open.
            let entry_spread_usd = sign * matched * (front.fair_mid_at_entry - front.fill_price);
            // 2. Fair-mid drift over holding period.
            let hold_drift_usd = sign * matched * (fill.fair_mid - front.fair_mid_at_entry);
            // 3. Exit execution edge vs fair mid at close (passive half-spread vs taker slippage).
            let exit_exec_usd = sign * matched * (fill.fill_price - fill.fair_mid);
            // 4. Pro-rata entry & exit fees allocated strictly to matched paired lots.
            let front_entry_fee = front.remaining_entry_fee_usd * (matched / front.qty);
            let exit_fee = fill.fee_usd * (matched / fill.qty);

            self.summary.spread_capture_usd += entry_spread_usd;
            self.summary.holding_drift_usd += hold_drift_usd;
            self.summary.paired_cycle_fees_usd += front_entry_fee + exit_fee;
            match fill.exit_mode {
                FillExitMode::TakerOrHazardClose => {
                    self.summary.taker_exit_slippage_usd += exit_exec_usd;
                    self.summary.taker_exit_notional_usd += matched * fill.fill_price;
                }
                FillExitMode::OpeningOrAdding | FillExitMode::PassiveReduceOnly => {
                    self.summary.passive_exit_pnl_usd += exit_exec_usd;
                    self.summary.passive_exit_notional_usd += matched * fill.fill_price;
                }
            }

            remaining -= matched;
            front.qty -= matched;
            front.remaining_entry_fee_usd -= front_entry_fee;
            if front.qty > 1e-12 {
                self.lots.push_front(front);
            } else {
                self.summary.closed_cycles += 1;
            }
        }

        if remaining > 1e-12 {
            let open_fee = fill.fee_usd * (remaining / fill.qty);
            self.lots.push_back(OpenLot {
                long_sign: fill.is_buy,
                qty: remaining,
                fill_price: fill.fill_price,
                fair_mid_at_entry: fill.fair_mid,
                remaining_entry_fee_usd: open_fee,
            });
        }

        self.summary.open_inventory_base = self
            .lots
            .iter()
            .map(|lot| if lot.long_sign { lot.qty } else { -lot.qty })
            .sum();
        self.recompute_net_cash();
        Ok(())
    }

    /// Accrue a settled funding payment as signed cash (positive = received, negative = paid)
    /// and fold it into `paired_cycle_net_cash_usd`. Perp funding settles on the venue's
    /// funding interval independently of fills, so it is recorded as a separate term rather
    /// than attributed to any single paired lot.
    pub fn record_funding(&mut self, funding_usd: f64) -> Result<()> {
        ensure!(funding_usd.is_finite(), "funding_usd must be finite");
        self.summary.funding_cash_usd += funding_usd;
        self.recompute_net_cash();
        Ok(())
    }

    /// Recompute net cash and markout divergence from the current component accumulators.
    fn recompute_net_cash(&mut self) {
        self.summary.paired_cycle_net_cash_usd = self.summary.spread_capture_usd
            + self.summary.holding_drift_usd
            + self.summary.passive_exit_pnl_usd
            + self.summary.taker_exit_slippage_usd
            + self.summary.funding_cash_usd
            - self.summary.paired_cycle_fees_usd;
        self.summary.markout_vs_cash_divergence_usd =
            self.summary.cumulative_1s_markout_usd - self.summary.paired_cycle_net_cash_usd;
    }
}

/// Stack-only conservative FIFO queue-ahead estimator for one resting reduce-only exit order.
///
/// Tracks estimated volume ahead at the exit order's price level using public sweep depletion
/// and top-of-book depth shrinkage (accounting for ahead cancellations conservatively without
/// claiming queue improvement when behind-us liquidity is added).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QueueAheadTracker {
    is_buy: bool,
    order_price: f64,
    order_qty: f64,
    queue_ahead_base: f64,
}

impl QueueAheadTracker {
    pub fn new(
        is_buy: bool,
        order_price: f64,
        order_qty: f64,
        initial_touch_size_base: f64,
    ) -> Result<Self> {
        ensure!(
            order_price.is_finite()
                && order_price > 0.0
                && order_qty.is_finite()
                && order_qty > 0.0
                && initial_touch_size_base.is_finite()
                && initial_touch_size_base >= 0.0,
            "invalid QueueAheadTracker initialization inputs"
        );
        Ok(Self {
            is_buy,
            order_price,
            order_qty,
            queue_ahead_base: initial_touch_size_base,
        })
    }

    pub fn matches_order(&self, is_buy: bool, order_price: f64) -> bool {
        self.is_buy == is_buy && (self.order_price - order_price).abs() <= 1e-9
    }

    pub fn queue_ahead_base(&self) -> f64 {
        self.queue_ahead_base
    }

    /// Update estimated queue ahead from contemporaneous top-of-book state and hit sweep volume.
    pub fn on_book_and_trade(
        &mut self,
        best_bid_px: f64,
        best_bid_sz: f64,
        best_ask_px: f64,
        best_ask_sz: f64,
        sweep_hit_qty: f64,
    ) -> f64 {
        if sweep_hit_qty.is_finite() && sweep_hit_qty > 0.0 {
            self.queue_ahead_base = (self.queue_ahead_base - sweep_hit_qty).max(0.0);
        }
        let (touch_px, touch_sz) = if self.is_buy {
            (best_bid_px, best_bid_sz)
        } else {
            (best_ask_px, best_ask_sz)
        };
        if (self.order_price - touch_px).abs() <= 1e-9 && touch_sz.is_finite() && touch_sz >= 0.0 {
            self.queue_ahead_base = self
                .queue_ahead_base
                .min((touch_sz - self.order_qty).max(0.0));
        }
        self.queue_ahead_base
    }

    /// Once our order experiences a fill, all volume strictly ahead in FIFO at this level has cleared.
    pub fn on_partial_fill(&mut self, filled_qty: f64) {
        if filled_qty.is_finite() && filled_qty > 0.0 {
            self.order_qty = (self.order_qty - filled_qty).max(0.0);
            self.queue_ahead_base = 0.0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_input(now_ms: u64, inv: f64, fair_mid: f64, mom_3s_bps: f64) -> ExitEvalInput {
        ExitEvalInput {
            now_ms,
            entry_opened_at_ms: 1_000_000,
            signed_inventory_base: inv,
            avg_entry_price: 100.0,
            fair_mid,
            bid_touch: fair_mid - 0.01,
            ask_touch: fair_mid + 0.01,
            tick_size: 0.01,
            momentum_3s_bps: mom_3s_bps,
            dust_base: 1e-6,
            queue_ahead_base: None,
        }
    }

    #[test]
    fn hybrid_exit_preserves_queue_per_stage_exempts_bounds_and_never_crosses_zero() {
        let mut ctrl = HybridExitController::new(HybridExitConfig::default()).unwrap();

        // Stage 1 (0..4s): reprices initially; subsequent tick in same stage preserves queue (reprice_required = false).
        let d1 = ctrl
            .evaluate(base_input(1_001_000, 5.0, 100.02, 0.0))
            .unwrap();
        let ExitDirective::PassiveReduce(slot1) = d1 else {
            panic!("expected PassiveReduce, got {d1:?}");
        };
        assert!(slot1.reduce_only);
        assert!(!slot1.is_buy);
        assert_eq!(slot1.qty, 5.0);
        assert_eq!(slot1.stage, PassiveExitStage::Stage1Wide);
        assert!(slot1.reprice_required);
        assert!(slot1.exempt_from_placement_bounds);
        assert!(slot1.exempt_from_drawdown_brake);

        // Fair mid moves slightly at t=3s, still in Stage 1 and still non-crossing -> keeps resting slot without churn!
        let d1b = ctrl
            .evaluate(base_input(1_003_000, 5.0, 100.00, -0.2))
            .unwrap();
        let ExitDirective::PassiveReduce(slot1b) = d1b else {
            panic!("expected PassiveReduce");
        };
        assert!(!slot1b.reprice_required);
        assert_eq!(slot1b.price, slot1.price);

        // Stage 2 (4..12s): advances cleanly with reprice_required = true.
        let d2 = ctrl
            .evaluate(base_input(1_005_000, 5.0, 100.00, 0.0))
            .unwrap();
        let ExitDirective::PassiveReduce(slot2) = d2 else {
            panic!("expected Stage2 PassiveReduce");
        };
        assert_eq!(slot2.stage, PassiveExitStage::Stage2Mid);
        assert!(slot2.reprice_required);

        // Stage 3 (>12s): joins BBO touch without crossing.
        let d3 = ctrl
            .evaluate(base_input(1_015_000, 5.0, 100.00, 0.0))
            .unwrap();
        let ExitDirective::PassiveReduce(slot3) = d3 else {
            panic!("expected Stage3 PassiveReduce");
        };
        assert_eq!(slot3.stage, PassiveExitStage::Stage3Touch);
        assert!(slot3.price >= 100.00);
        assert!(slot3.qty <= 5.0);
    }

    #[test]
    fn hybrid_exit_handles_1tick_ieee754_rounding_partial_fills_and_crossed_bbo_reprice() {
        let mut ctrl = HybridExitController::new(HybridExitConfig::default()).unwrap();
        // Tight 1-tick spread where `100.01 - 0.01` in binary f64 must stay exactly at `100.00` without losing a tick.
        let tight = ExitEvalInput {
            now_ms: 1_001_000,
            entry_opened_at_ms: 1_000_000,
            signed_inventory_base: -4.0,
            avg_entry_price: 100.0,
            fair_mid: 100.005,
            bid_touch: 100.00,
            ask_touch: 100.01,
            tick_size: 0.01,
            momentum_3s_bps: 0.0,
            dust_base: 1e-6,
            queue_ahead_base: None,
        };
        let ExitDirective::PassiveReduce(slot_buy) = ctrl.evaluate(tight).unwrap() else {
            panic!("expected PassiveReduce buy");
        };
        assert!((slot_buy.price - 100.00).abs() < 1e-9);

        // Partial fill of 1.5 leaves 2.5 in active slot with FIFO rank intact.
        ctrl.notify_partial_fill(1.5).unwrap();
        let mut partial_input = tight;
        partial_input.now_ms = 1_002_000;
        partial_input.signed_inventory_base = -2.5;
        let ExitDirective::PassiveReduce(slot_after_pf) = ctrl.evaluate(partial_input).unwrap()
        else {
            panic!("expected PassiveReduce");
        };
        assert!(!slot_after_pf.reprice_required);
        assert!((slot_after_pf.qty - 2.5).abs() < 1e-12);

        // If ask_touch drops onto/below our resting buy price, same-stage evaluation MUST reprice cleanly!
        let mut crossed_input = partial_input;
        crossed_input.now_ms = 1_002_500;
        crossed_input.fair_mid = 99.995;
        crossed_input.bid_touch = 99.99;
        crossed_input.ask_touch = 100.00;
        let ExitDirective::PassiveReduce(slot_repriced) = ctrl.evaluate(crossed_input).unwrap()
        else {
            panic!("expected PassiveReduce");
        };
        assert!(slot_repriced.reprice_required);
        assert!(slot_repriced.price < crossed_input.ask_touch - 1e-12);
    }

    #[test]
    fn hybrid_exit_triggers_early_hazard_boundary_before_50s_timeout() {
        let mut ctrl = HybridExitController::new(HybridExitConfig::default()).unwrap();

        // At t=+3s, unrealized loss = -4.0 bps (<= -2.5 bps) and 3s momentum = -1.5 bps (<= -1.0 bps) -> early lock!
        let hazard = ctrl
            .evaluate(base_input(1_003_000, 4.0, 99.96, -1.5))
            .unwrap();
        assert!(matches!(
            hazard,
            ExitDirective::EarlyHazardHedgeLock {
                reduce_only: true,
                hedge_is_buy: false,
                qty,
                ..
            } if (qty - 4.0).abs() < 1e-12
        ));

        // At t=+50s without recovery -> TimeoutFlatten.
        let timeout = ctrl
            .evaluate(base_input(1_050_000, -3.0, 99.99, 0.0))
            .unwrap();
        assert!(matches!(
            timeout,
            ExitDirective::TimeoutFlatten {
                reduce_only: true,
                is_buy: true,
                qty,
                hold_ms: 50_000,
                ..
            } if (qty - 3.0).abs() < 1e-12
        ));
    }

    #[test]
    fn cycle_ledger_exposes_positive_1s_markout_but_negative_full_cycle_cash() {
        let mut ledger = InventoryCycleCashLedger::new();
        // Open long 10 units at $100.01 vs mid $100.00 (adverse fill -$0.10 spread, yet +$14.20 1s markout).
        ledger
            .record_fill(CycleFillInput {
                is_buy: true,
                qty: 10.0,
                fill_price: 100.01,
                fair_mid: 100.00,
                fee_usd: 0.50,
                exit_mode: FillExitMode::OpeningOrAdding,
                markout_1s_usd: Some(14.20),
            })
            .unwrap();
        // Later exits via forced taker hedge when mid drifted -$1.00 and slippage -$2.50:
        ledger
            .record_fill(CycleFillInput {
                is_buy: false,
                qty: 10.0,
                fill_price: 98.75,
                fair_mid: 99.00,
                fee_usd: 1.00,
                exit_mode: FillExitMode::TakerOrHazardClose,
                markout_1s_usd: Some(0.0),
            })
            .unwrap();
        // Add an unclosed second lot with a $5.00 entry fee -> must NOT distort closed-cycle cash!
        ledger
            .record_fill(CycleFillInput {
                is_buy: true,
                qty: 5.0,
                fill_price: 99.00,
                fair_mid: 99.00,
                fee_usd: 5.00,
                exit_mode: FillExitMode::OpeningOrAdding,
                markout_1s_usd: None,
            })
            .unwrap();

        let s = ledger.summary();
        assert!((s.spread_capture_usd - (-0.10)).abs() < 1e-9);
        assert!((s.holding_drift_usd - (-10.00)).abs() < 1e-9);
        assert!((s.taker_exit_slippage_usd - (-2.50)).abs() < 1e-9);
        assert!((s.explicit_fees_usd - 6.50).abs() < 1e-9);
        assert!((s.paired_cycle_fees_usd - 1.50).abs() < 1e-9);
        assert!((s.paired_cycle_net_cash_usd - (-14.10)).abs() < 1e-9);
        assert!((s.cumulative_1s_markout_usd - 14.20).abs() < 1e-9);
        assert!((s.markout_vs_cash_divergence_usd - 28.30).abs() < 1e-9);
        assert_eq!(s.closed_cycles, 1);
        assert_eq!(s.open_inventory_base, 5.0);
    }

    #[test]
    fn cycle_ledger_funding_flips_flat_price_cycle_to_negative_net_cash() {
        let mut ledger = InventoryCycleCashLedger::new();
        // Open long 10 @ mid (zero spread, zero fee) then flat-exit at the same mid: price-flat cycle.
        ledger
            .record_fill(CycleFillInput {
                is_buy: true,
                qty: 10.0,
                fill_price: 100.00,
                fair_mid: 100.00,
                fee_usd: 0.0,
                exit_mode: FillExitMode::OpeningOrAdding,
                markout_1s_usd: None,
            })
            .unwrap();
        ledger
            .record_fill(CycleFillInput {
                is_buy: false,
                qty: 10.0,
                fill_price: 100.00,
                fair_mid: 100.00,
                fee_usd: 0.0,
                exit_mode: FillExitMode::PassiveReduceOnly,
                markout_1s_usd: None,
            })
            .unwrap();

        // Price-flat, fee-free: net cash is exactly zero before funding.
        assert!((ledger.summary().paired_cycle_net_cash_usd).abs() < 1e-12);

        // Two funding intervals: received +$3.00 then paid -$8.50 while holding.
        ledger.record_funding(3.00).unwrap();
        ledger.record_funding(-8.50).unwrap();

        let s = ledger.summary();
        assert!((s.funding_cash_usd - (-5.50)).abs() < 1e-9);
        // The cycle looks break-even on turnover but is -$5.50 once funding is netted.
        assert!((s.paired_cycle_net_cash_usd - (-5.50)).abs() < 1e-9);
        assert_eq!(s.closed_cycles, 1);
        assert!(s.open_inventory_base.abs() < 1e-12);
    }

    #[test]
    fn cycle_ledger_rejects_non_finite_funding() {
        let mut ledger = InventoryCycleCashLedger::new();
        assert!(ledger.record_funding(f64::NAN).is_err());
        assert!(ledger.record_funding(f64::INFINITY).is_err());
        assert!((ledger.summary().funding_cash_usd).abs() < 1e-12);
    }

    #[test]
    fn queue_exit_advances_stage_and_escalates_to_ioc_when_deep_backlog_with_loss() {
        let mut off = HybridExitController::new(HybridExitConfig::default()).unwrap();
        let mut inp = base_input(1_001_000, 5.0, 99.98, 0.0); // long, -2 bp unrealized
        inp.queue_ahead_base = Some(40.0); // 40 / 2.0 = 20s expected wait
        let ExitDirective::PassiveReduce(off_slot) = off.evaluate(inp).unwrap() else {
            panic!("expected PassiveReduce");
        };
        assert_eq!(off_slot.stage, PassiveExitStage::Stage1Wide);

        let mut on_cfg = HybridExitConfig::default();
        on_cfg.queue_exit.enabled = true;
        let mut on = HybridExitController::new(on_cfg).unwrap();

        // 15s wait (> 6s max_expected_fill_sec, <= 15s urgent threshold) -> advances Stage1 -> Stage2Mid.
        let mut mid_inp = inp;
        mid_inp.queue_ahead_base = Some(20.0); // 10s wait
        let ExitDirective::PassiveReduce(adv_slot) = on.evaluate(mid_inp).unwrap() else {
            panic!("expected PassiveReduce");
        };
        assert_eq!(adv_slot.stage, PassiveExitStage::Stage2Mid);

        // 40s wait (> 15s urgent limit) + negative unrealized PnL -> immediate EarlyHazardHedgeLock.
        let urgent = on.evaluate(inp).unwrap();
        assert!(matches!(
            urgent,
            ExitDirective::EarlyHazardHedgeLock {
                reduce_only: true,
                hedge_is_buy: false,
                ..
            }
        ));
    }

    #[test]
    fn queue_ahead_tracker_depletes_via_sweeps_touch_shrinkage_and_partial_fill() {
        let mut tracker = QueueAheadTracker::new(false, 100.1, 2.0, 12.0).unwrap();
        assert!((tracker.queue_ahead_base() - 12.0).abs() < 1e-12);

        // Sweep eats 3.0 ahead and ask touch shrinks from 12.0 to 10.0 (meaning <= 8.0 ahead of our 2.0).
        let q1 = tracker.on_book_and_trade(100.0, 5.0, 100.1, 10.0, 3.0);
        assert!((q1 - 8.0).abs() < 1e-12);

        // New size joining behind us (ask_sz = 15.0) must NOT increase our estimated queue ahead.
        let q2 = tracker.on_book_and_trade(100.0, 5.0, 100.1, 15.0, 0.0);
        assert!((q2 - 8.0).abs() < 1e-12);

        // Partial fill clears remaining queue ahead to zero.
        tracker.on_partial_fill(0.5);
        assert!((tracker.queue_ahead_base()).abs() < 1e-12);
    }

    #[test]
    fn passive_touch_before_hazard_ioc_avoids_taker_fee_until_catastrophe_or_timeout() {
        let mut invalid = HybridExitConfig::default();
        invalid.catastrophe_ioc_unrealized_pnl_bps = Some(-2.0); // not strictly < -2.5 early_hazard
        assert!(invalid.validate().is_err());

        let mild_adverse = base_input(1_002_500, 5.0, 99.96, -1.5); // -4 bp unrealized, -1.5 bp mom
        let mut legacy = HybridExitController::new(HybridExitConfig::default()).unwrap();
        assert!(matches!(
            legacy.evaluate(mild_adverse).unwrap(),
            ExitDirective::EarlyHazardHedgeLock { .. }
        ));

        let passive_cfg = HybridExitConfig {
            passive_touch_before_hazard_ioc: true,
            catastrophe_ioc_unrealized_pnl_bps: Some(-10.0),
            ..HybridExitConfig::default()
        };
        passive_cfg.validate().unwrap();
        let mut passive = HybridExitController::new(passive_cfg).unwrap();
        let ExitDirective::PassiveReduce(slot) = passive.evaluate(mild_adverse).unwrap() else {
            panic!("expected Stage3Touch PassiveReduce before catastrophe threshold");
        };
        assert_eq!(slot.stage, PassiveExitStage::Stage3Touch);
        assert!(slot.reduce_only);

        // Severe loss below catastrophe threshold escalates immediately to IOC hedge lock.
        let catastrophe = base_input(1_002_500, 5.0, 99.88, -1.5); // -12 bp <= -10 bp
        assert!(matches!(
            passive.evaluate(catastrophe).unwrap(),
            ExitDirective::EarlyHazardHedgeLock {
                reduce_only: true,
                ..
            }
        ));

        // Timeout still forces TimeoutFlatten even if loss is mild.
        let timed_out = base_input(1_050_000, 5.0, 99.96, -1.5);
        assert!(matches!(
            passive.evaluate(timed_out).unwrap(),
            ExitDirective::TimeoutFlatten {
                reduce_only: true,
                ..
            }
        ));
    }
}
