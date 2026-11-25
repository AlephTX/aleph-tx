//! Venue-agnostic strategy execution driver (`QuoteSlotTracker` + `GenericExchangeDriver`)
//! and deterministic continuous-window replay evaluator (`S1` / `S2` / `S3` common runner).
//!
//! Contains ZERO imports from `crate::exchanges::*`: drives any [`Exchange`] implementation
//! via [`BatchAction`] while honoring [`HybridExitController`] queue preservation (`reprice_required == false`),
//! `reduce_only` safety, and strict `AGENTS.md` continuous-window warm-up / eval / drain accounting.

use crate::daily_loss::DailyLossGuard;
use crate::exchange::{BatchAction, BatchResult, Exchange, OrderParams, OrderType, Side};
use crate::strategy::hybrid_exit::{
    CycleCashSummary, CycleFillInput, ExitDirective, FillExitMode, InventoryCycleCashLedger,
    QueueAheadTracker,
};
use crate::strategy::microprice::MarkovMicropriceTable;
use crate::strategy::quote_pipeline::{
    ExternalLeaderSignal, GenericInventoryState, GenericMarketState, LeadLagConfig, OpeningQuote,
    QuotePipeline, QuotePipelineConfig, QuotePlan, StrategyMode,
};
use crate::strategy::wallet_toxicity::{
    QuoteSide, WalletToxicityConfig, WalletToxicityTier, salted_wallet_key,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

const QTY_EPS: f64 = 1e-12;
const PX_EPS: f64 = 1e-9;
const CAUSAL_MARKOUT_HORIZON_MS: u64 = 1_000;

#[derive(Debug, Clone, Copy)]
struct PendingFillMarkout {
    fill_ms: u64,
    is_buy: bool,
    fill_price: f64,
}

/// Exact 10-hour minimum continuous evaluation window duration in milliseconds.
pub const FULL_10H_CONTINUOUS_MS: u64 = 36_000_000;
/// Maximum allowed evaluation sample gap (including window lead/tail gaps) for continuous coverage.
pub const DEFAULT_MAX_CONTINUOUS_EVAL_GAP_MS: u64 = 60_000;

/// Logical order slot managed by [`QuoteSlotTracker`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OrderSlotKind {
    OpeningBid,
    OpeningAsk,
    ReduceOnlyExit,
}

/// Resting order metadata tracked per logical slot for queue-preserving diffs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RestingSlotOrder {
    pub client_order_index: i64,
    pub side: Side,
    pub price: f64,
    pub qty: f64,
    pub reduce_only: bool,
    pub faded_by_toxicity: bool,
}

/// Configuration controlling queue-preserving quote diffing and lot quantization.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SlotDiffConfig {
    /// Price drift tolerance (in ticks) inside which an unchanged opening quote stays resting.
    pub reprice_deadband_ticks: u32,
    /// Relative size drift tolerance `[0, 1]` inside which an opening quote stays resting.
    pub min_size_change_frac: f64,
    /// Minimum lot increment used to conservatively floor all order quantities.
    pub lot_step: f64,
    /// Minimum order quantity accepted by the target venue.
    pub min_order_qty: f64,
}

impl Default for SlotDiffConfig {
    fn default() -> Self {
        Self {
            reprice_deadband_ticks: 1,
            min_size_change_frac: 0.10,
            lot_step: 1e-4,
            min_order_qty: 1e-3,
        }
    }
}

impl SlotDiffConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.min_size_change_frac.is_finite()
                && (0.0..=1.0).contains(&self.min_size_change_frac),
            "min_size_change_frac must be finite and in [0, 1]"
        );
        ensure!(
            self.lot_step.is_finite() && self.lot_step > 0.0,
            "lot_step must be finite and > 0"
        );
        ensure!(
            self.min_order_qty.is_finite() && self.min_order_qty >= self.lot_step,
            "min_order_qty must be finite and >= lot_step"
        );
        Ok(())
    }
}

/// Deterministic order diff action emitted by [`QuoteSlotTracker::diff_plan`].
#[derive(Debug, Clone, PartialEq)]
pub enum DriverOrderAction {
    Cancel {
        slot: OrderSlotKind,
        client_order_index: i64,
    },
    Place {
        slot: OrderSlotKind,
        params: OrderParams,
        faded_by_toxicity: bool,
    },
}

impl DriverOrderAction {
    pub fn to_batch_action(&self) -> BatchAction {
        match self {
            Self::Cancel {
                client_order_index, ..
            } => BatchAction::Cancel(*client_order_index),
            Self::Place { params, .. } => BatchAction::Place(params.clone()),
        }
    }
}

/// Floor `raw_qty` to an exact non-negative multiple of `lot_step` without ever overshooting `raw_qty`.
pub fn quantize_lot_floor(raw_qty: f64, lot_step: f64) -> f64 {
    if !raw_qty.is_finite() || raw_qty <= 0.0 || !lot_step.is_finite() || lot_step <= 0.0 {
        return 0.0;
    }
    let ratio = raw_qty / lot_step;
    let mut k = (ratio + 1e-12).floor().max(0.0);
    let mut quantized = k * lot_step;
    // Guarantee `quantized <= raw_qty` so `reduce_only` orders never exceed open inventory.
    while quantized > raw_qty + QTY_EPS && k > 0.0 {
        k -= 1.0;
        quantized = k * lot_step;
    }
    quantized.min(raw_qty)
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct TargetOrderSpec {
    side: Side,
    price: f64,
    qty: f64,
    order_type: OrderType,
    reduce_only: bool,
    faded_by_toxicity: bool,
    force_keep_resting: bool,
}

/// Tracks active resting order slots and emits minimal queue-preserving [`DriverOrderAction`] batches.
#[derive(Debug, Clone)]
pub struct QuoteSlotTracker {
    config: SlotDiffConfig,
    opening_bid: Option<RestingSlotOrder>,
    opening_ask: Option<RestingSlotOrder>,
    exit_slot: Option<RestingSlotOrder>,
    dirty: bool,
}

impl QuoteSlotTracker {
    pub fn new(config: SlotDiffConfig) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            config,
            opening_bid: None,
            opening_ask: None,
            exit_slot: None,
            dirty: false,
        })
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn resting(&self, slot: OrderSlotKind) -> Option<RestingSlotOrder> {
        match slot {
            OrderSlotKind::OpeningBid => self.opening_bid,
            OrderSlotKind::OpeningAsk => self.opening_ask,
            OrderSlotKind::ReduceOnlyExit => self.exit_slot,
        }
    }

    /// Mark local state dirty on ambiguous batch failure so caller reconciles before quoting again.
    pub fn invalidate_all_unknown(&mut self) {
        self.opening_bid = None;
        self.opening_ask = None;
        self.exit_slot = None;
        self.dirty = true;
    }

    /// Reset clean flat state after authoritative venue cancellation/reconciliation.
    pub fn reset_flat_clean(&mut self) {
        self.opening_bid = None;
        self.opening_ask = None;
        self.exit_slot = None;
        self.dirty = false;
    }

    /// Apply fill quantity only to the matching resting client order.
    ///
    /// Returns `Ok(false)` for a valid event with a stale identity or an empty slot without
    /// mutating any slot; the caller retains responsibility for reconciling and accounting for
    /// earlier orders. A matching event retains positive residuals (`rem > QTY_EPS`) even below
    /// `min_order_qty` (so subsequent `diff_plan` emits `Cancel` before placing a replacement),
    /// clears on full fill (`rem <= QTY_EPS`), and invalidates all slots fail-closed on overfill.
    pub fn notify_fill(
        &mut self,
        slot: OrderSlotKind,
        client_order_index: i64,
        filled_qty: f64,
    ) -> Result<bool> {
        ensure!(
            filled_qty.is_finite() && filled_qty >= 0.0,
            "filled_qty must be finite and >= 0"
        );
        let Some(order) = self
            .resting(slot)
            .filter(|order| order.client_order_index == client_order_index)
        else {
            return Ok(false);
        };
        if filled_qty > order.qty + QTY_EPS {
            self.invalidate_all_unknown();
            bail!(
                "filled_qty ({filled_qty}) exceeds resting slot qty ({})",
                order.qty
            );
        }
        let rem = order.qty - filled_qty;
        *self.slot_mut(slot) = (rem > QTY_EPS).then_some(RestingSlotOrder { qty: rem, ..order });
        Ok(true)
    }

    /// Clear a resting slot ONLY when authoritative venue terminal evidence matches `client_order_index`.
    pub fn notify_order_terminal(&mut self, slot: OrderSlotKind, client_order_index: i64) {
        let slot_ref = self.slot_mut(slot);
        if slot_ref.is_some_and(|r| r.client_order_index == client_order_index) {
            *slot_ref = None;
        }
    }

    fn slot_mut(&mut self, slot: OrderSlotKind) -> &mut Option<RestingSlotOrder> {
        match slot {
            OrderSlotKind::OpeningBid => &mut self.opening_bid,
            OrderSlotKind::OpeningAsk => &mut self.opening_ask,
            OrderSlotKind::ReduceOnlyExit => &mut self.exit_slot,
        }
    }

    /// Compute cancel-first order actions transitioning current resting slots to `plan`.
    pub fn diff_plan(
        &self,
        plan: &QuotePlan,
        tick_size: f64,
        limit_order_type: OrderType,
    ) -> Result<Vec<DriverOrderAction>> {
        ensure!(
            !self.dirty,
            "slot tracker dirty; reconcile venue before diffing"
        );
        ensure!(
            tick_size.is_finite() && tick_size > 0.0,
            "tick_size must be finite and > 0"
        );

        let bid_target = self.opening_target(plan.bid_open.as_ref(), Side::Buy, limit_order_type);
        let ask_target = self.opening_target(plan.ask_open.as_ref(), Side::Sell, limit_order_type);
        let exit_target = self.exit_target(&plan.exit_directive, limit_order_type);

        let mut cancels = Vec::with_capacity(3);
        let mut places = Vec::with_capacity(3);

        self.diff_single_slot(
            OrderSlotKind::ReduceOnlyExit,
            self.exit_slot,
            exit_target,
            tick_size,
            &mut cancels,
            &mut places,
        );
        self.diff_single_slot(
            OrderSlotKind::OpeningBid,
            self.opening_bid,
            bid_target,
            tick_size,
            &mut cancels,
            &mut places,
        );
        self.diff_single_slot(
            OrderSlotKind::OpeningAsk,
            self.opening_ask,
            ask_target,
            tick_size,
            &mut cancels,
            &mut places,
        );

        // Always emit cancellations BEFORE placements to free margin/position room first.
        cancels.extend(places);
        Ok(cancels)
    }

    /// Apply executed batch placements & cancellations into local slot state atomically.
    pub fn apply_batch_result(
        &mut self,
        actions: &[DriverOrderAction],
        batch_result: &BatchResult,
    ) -> Result<()> {
        let place_actions: Vec<(OrderSlotKind, &OrderParams, bool)> = actions
            .iter()
            .filter_map(|act| match act {
                DriverOrderAction::Place {
                    slot,
                    params,
                    faded_by_toxicity,
                } => Some((*slot, params, *faded_by_toxicity)),
                DriverOrderAction::Cancel { .. } => None,
            })
            .collect();

        if place_actions.len() != batch_result.place_results.len() {
            self.invalidate_all_unknown();
            bail!(
                "batch place_results length mismatch: expected {}, got {}",
                place_actions.len(),
                batch_result.place_results.len()
            );
        }
        for (i, placed) in batch_result.place_results.iter().enumerate() {
            if !placed.price.is_finite() || !placed.size.is_finite() {
                self.invalidate_all_unknown();
                bail!(
                    "batch place_result[{i}] has non-finite price/size ({}, {})",
                    placed.price,
                    placed.size
                );
            }
            if batch_result.place_results[..i]
                .iter()
                .any(|prev| prev.client_order_index == placed.client_order_index)
            {
                self.invalidate_all_unknown();
                bail!(
                    "duplicate client_order_index {} in batch place_results",
                    placed.client_order_index
                );
            }
        }

        for act in actions {
            if let DriverOrderAction::Cancel {
                slot,
                client_order_index,
            } = act
            {
                let slot_ref = self.slot_mut(*slot);
                if slot_ref.is_some_and(|r| r.client_order_index == *client_order_index) {
                    *slot_ref = None;
                }
            }
        }

        // Bind each placed order by matching (side, price, size) rather than trusting the
        // positional order of `place_results`. A venue adapter that returns results out of
        // order (or coalesces identical clips) must not silently bind the wrong
        // client_order_index into a slot — a later Cancel would then target the wrong order
        // while the batch still looked successful. Each result is consumed at most once; an
        // ambiguous (0 or >1) match fails closed via invalidate_all_unknown().
        let mut consumed = vec![false; batch_result.place_results.len()];
        for (slot, params, faded) in place_actions.into_iter() {
            // Require a UNIQUE unconsumed `(side, price, size)` match. An opening placement and a
            // `reduce_only` exit placement (or separate slots) sharing `(side, price, size)` are
            // NOT interchangeable in `RestingSlotOrder` / exit accounting when returned out of order.
            let mut matches =
                batch_result
                    .place_results
                    .iter()
                    .enumerate()
                    .filter(|(i, placed)| {
                        !consumed[*i]
                            && placed.side == params.side
                            && (placed.price - params.price).abs() <= PX_EPS
                            && (placed.size - params.size).abs() <= QTY_EPS
                    });
            let match_idx = matches.next().map(|(i, _)| i);
            if matches.next().is_some() {
                self.invalidate_all_unknown();
                bail!(
                    "ambiguous batch place_result for placement side={:?} price={} size={}",
                    params.side,
                    params.price,
                    params.size
                );
            }
            let Some(idx) = match_idx else {
                self.invalidate_all_unknown();
                bail!(
                    "no batch place_result matched placement side={:?} price={} size={}",
                    params.side,
                    params.price,
                    params.size
                );
            };
            consumed[idx] = true;
            let placed = &batch_result.place_results[idx];

            if params.order_type == OrderType::Ioc || params.order_type == OrderType::Market {
                // IOC/Market orders do not stay resting on the book.
                *self.slot_mut(slot) = None;
                continue;
            }
            *self.slot_mut(slot) = Some(RestingSlotOrder {
                client_order_index: placed.client_order_index,
                side: params.side,
                price: params.price,
                qty: params.size,
                reduce_only: params.reduce_only,
                faded_by_toxicity: faded,
            });
        }
        Ok(())
    }

    fn opening_target(
        &self,
        quote: Option<&OpeningQuote>,
        expected_side: Side,
        limit_order_type: OrderType,
    ) -> Option<TargetOrderSpec> {
        let q = quote?;
        let side = match q.side {
            QuoteSide::Bid => Side::Buy,
            QuoteSide::Ask => Side::Sell,
        };
        if side != expected_side {
            return None;
        }
        let qty = quantize_lot_floor(q.qty, self.config.lot_step);
        if qty + QTY_EPS < self.config.min_order_qty || !q.price.is_finite() || q.price <= 0.0 {
            return None;
        }
        Some(TargetOrderSpec {
            side,
            price: q.price,
            qty,
            order_type: limit_order_type,
            reduce_only: false,
            faded_by_toxicity: q.faded_by_toxicity,
            force_keep_resting: false,
        })
    }

    fn exit_target(
        &self,
        directive: &ExitDirective,
        limit_order_type: OrderType,
    ) -> Option<TargetOrderSpec> {
        match directive {
            ExitDirective::Flat => None,
            ExitDirective::PassiveReduce(slot) => {
                let side = if slot.is_buy { Side::Buy } else { Side::Sell };
                let qty = quantize_lot_floor(slot.qty, self.config.lot_step);
                if qty + QTY_EPS < self.config.min_order_qty
                    || !slot.price.is_finite()
                    || slot.price <= 0.0
                {
                    return None;
                }
                Some(TargetOrderSpec {
                    side,
                    price: slot.price,
                    qty,
                    order_type: limit_order_type,
                    reduce_only: true,
                    faded_by_toxicity: false,
                    force_keep_resting: !slot.reprice_required,
                })
            }
            ExitDirective::EarlyHazardHedgeLock {
                hedge_is_buy,
                qty,
                limit_price,
                ..
            } => {
                let side = if *hedge_is_buy { Side::Buy } else { Side::Sell };
                let q = quantize_lot_floor(*qty, self.config.lot_step);
                if q + QTY_EPS < self.config.min_order_qty
                    || !limit_price.is_finite()
                    || *limit_price <= 0.0
                {
                    return None;
                }
                Some(TargetOrderSpec {
                    side,
                    price: *limit_price,
                    qty: q,
                    order_type: OrderType::Ioc,
                    reduce_only: true,
                    faded_by_toxicity: false,
                    force_keep_resting: false,
                })
            }
            ExitDirective::TimeoutFlatten {
                is_buy,
                qty,
                limit_price,
                ..
            } => {
                let side = if *is_buy { Side::Buy } else { Side::Sell };
                let q = quantize_lot_floor(*qty, self.config.lot_step);
                if q + QTY_EPS < self.config.min_order_qty
                    || !limit_price.is_finite()
                    || *limit_price <= 0.0
                {
                    return None;
                }
                Some(TargetOrderSpec {
                    side,
                    price: *limit_price,
                    qty: q,
                    order_type: OrderType::Ioc,
                    reduce_only: true,
                    faded_by_toxicity: false,
                    force_keep_resting: false,
                })
            }
        }
    }

    fn diff_single_slot(
        &self,
        slot: OrderSlotKind,
        resting: Option<RestingSlotOrder>,
        target: Option<TargetOrderSpec>,
        tick_size: f64,
        cancels: &mut Vec<DriverOrderAction>,
        places: &mut Vec<DriverOrderAction>,
    ) {
        match (resting, target) {
            (None, None) => {}
            (Some(r), None) => {
                cancels.push(DriverOrderAction::Cancel {
                    slot,
                    client_order_index: r.client_order_index,
                });
            }
            (Some(r), Some(t)) => {
                let keep_resting = if t.order_type == OrderType::Ioc {
                    false
                } else if slot == OrderSlotKind::ReduceOnlyExit {
                    t.force_keep_resting
                        && r.side == t.side
                        && r.reduce_only == t.reduce_only
                        && (r.price - t.price).abs() <= PX_EPS
                        && (r.qty - t.qty).abs() <= self.config.lot_step * 0.5 + QTY_EPS
                } else {
                    let max_px_drift =
                        f64::from(self.config.reprice_deadband_ticks) * tick_size + PX_EPS;
                    let rel_qty_diff = (r.qty - t.qty).abs() / r.qty.max(QTY_EPS);
                    let toxicity_newly_engaged = !r.faded_by_toxicity && t.faded_by_toxicity;
                    r.side == t.side
                        && r.reduce_only == t.reduce_only
                        && r.qty + QTY_EPS >= self.config.min_order_qty
                        && (r.price - t.price).abs() <= max_px_drift
                        && rel_qty_diff <= self.config.min_size_change_frac + 1e-12
                        && !toxicity_newly_engaged
                };

                if !keep_resting {
                    cancels.push(DriverOrderAction::Cancel {
                        slot,
                        client_order_index: r.client_order_index,
                    });
                    places.push(DriverOrderAction::Place {
                        slot,
                        params: OrderParams {
                            side: t.side,
                            size: t.qty,
                            price: t.price,
                            order_type: t.order_type,
                            reduce_only: t.reduce_only,
                        },
                        faded_by_toxicity: t.faded_by_toxicity,
                    });
                }
            }
            (None, Some(t)) => {
                places.push(DriverOrderAction::Place {
                    slot,
                    params: OrderParams {
                        side: t.side,
                        size: t.qty,
                        price: t.price,
                        order_type: t.order_type,
                        reduce_only: t.reduce_only,
                    },
                    faded_by_toxicity: t.faded_by_toxicity,
                });
            }
        }
    }
}

/// Generic live/shadow exchange driver coupling [`QuotePipeline`] to any [`Exchange`] adapter.
pub struct GenericExchangeDriver<E: Exchange> {
    exchange: E,
    pipeline: QuotePipeline,
    slots: QuoteSlotTracker,
}

impl<E: Exchange> GenericExchangeDriver<E> {
    pub fn new(exchange: E, pipeline: QuotePipeline, slot_diff: SlotDiffConfig) -> Result<Self> {
        Ok(Self {
            exchange,
            pipeline,
            slots: QuoteSlotTracker::new(slot_diff)?,
        })
    }

    pub fn exchange(&self) -> &E {
        &self.exchange
    }

    pub fn pipeline(&self) -> &QuotePipeline {
        &self.pipeline
    }

    pub fn pipeline_mut(&mut self) -> &mut QuotePipeline {
        &mut self.pipeline
    }

    pub fn slots(&self) -> &QuoteSlotTracker {
        &self.slots
    }

    pub fn slots_mut(&mut self) -> &mut QuoteSlotTracker {
        &mut self.slots
    }

    /// Evaluate the current quote plan and dispatch minimal batch diff actions to `E`.
    /// Fails closed on ambiguous batch failure by invalidating local slot state.
    pub async fn step(
        &mut self,
        market: &GenericMarketState,
        inv: &GenericInventoryState,
        leader: Option<&ExternalLeaderSignal>,
    ) -> Result<(QuotePlan, usize)> {
        ensure!(
            !self.slots.is_dirty(),
            "exchange driver slots are dirty; reconcile authoritative open orders first"
        );
        let plan = self.pipeline.evaluate_with_leader(market, inv, leader)?;
        let actions =
            self.slots
                .diff_plan(&plan, market.tick_size, self.exchange.limit_order_type())?;
        let action_count = actions.len();
        if !actions.is_empty() {
            let batch: Vec<BatchAction> = actions
                .iter()
                .map(DriverOrderAction::to_batch_action)
                .collect();
            match self.exchange.execute_batch(batch).await {
                Ok(res) => {
                    self.slots.apply_batch_result(&actions, &res)?;
                }
                Err(err) => {
                    self.slots.invalidate_all_unknown();
                    return Err(err).context("execute_batch failed; local slots invalidated");
                }
            }
        }
        Ok((plan, action_count))
    }
}

/// Replay window specification separating pre-window warm-up, continuous evaluation, and drain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayWindowSpec {
    pub warmup_start_ms: u64,
    pub eval_start_ms: u64,
    pub eval_end_ms: u64,
    pub drain_end_ms: u64,
}

impl ReplayWindowSpec {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.warmup_start_ms > 0
                && self.warmup_start_ms <= self.eval_start_ms
                && self.eval_start_ms < self.eval_end_ms
                && self.eval_end_ms <= self.drain_end_ms,
            "invalid replay window bounds (require 0 < warmup <= eval_start < eval_end <= drain_end)"
        );
        Ok(())
    }

    pub fn eval_duration_ms(&self) -> u64 {
        self.eval_end_ms.saturating_sub(self.eval_start_ms)
    }

    pub fn is_full_10h_continuous(&self) -> bool {
        self.eval_duration_ms() >= 36_000_000
    }
}

/// Deterministic public taker sweep event optionally coinciding with a replay tick.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ReplayTakerSweep {
    pub wallet_key: u64,
    pub taker_is_buy: bool,
    pub is_twap_sub: bool,
    pub sweep_size_base: f64,
}

/// One chronological market snapshot in a continuous replay tape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplayTick {
    pub market: GenericMarketState,
    pub leader: Option<ExternalLeaderSignal>,
    pub taker_sweep: Option<ReplayTakerSweep>,
}

/// Configuration for [`run_continuous_pipeline_replay`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PipelineReplayConfig {
    pub pipeline: QuotePipelineConfig,
    pub slot_diff: SlotDiffConfig,
    pub window: ReplayWindowSpec,
    pub initial_capital_usd: f64,
    pub daily_loss_limit_pct: f64,
    pub maker_fee_bps: f64,
    pub taker_fee_bps: f64,
    pub queue_fill_delay_ms: u64,
    /// Deterministic cadence (`0` disables ambient non-sweep touch fills) for touch resting fills.
    pub passive_touch_fill_stride: u64,
    /// Initial wallet tiers pre-registered prior to replay start (strictly causal).
    pub pre_registered_wallets: Vec<(u64, WalletToxicityTier)>,
}

impl PipelineReplayConfig {
    pub fn validate(&self) -> Result<()> {
        self.pipeline.validate()?;
        self.slot_diff.validate()?;
        self.window.validate()?;
        ensure!(
            self.initial_capital_usd.is_finite() && self.initial_capital_usd > 0.0,
            "initial_capital_usd must be finite and > 0"
        );
        ensure!(
            self.daily_loss_limit_pct.is_finite()
                && self.daily_loss_limit_pct > 0.0
                && self.daily_loss_limit_pct < 1.0,
            "daily_loss_limit_pct must be finite and in (0, 1)"
        );
        ensure!(
            self.maker_fee_bps.is_finite() && self.taker_fee_bps.is_finite(),
            "fee bps must be finite"
        );
        Ok(())
    }
}

/// Complete continuous replay report respecting warm-up/eval/drain boundaries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PipelineReplayReport {
    pub mode: StrategyMode,
    pub eval_duration_ms: u64,
    pub observed_eval_span_ms: u64,
    pub max_eval_gap_ms: u64,
    pub is_full_10h_continuous: bool,
    pub warmup_ticks: u64,
    pub eval_ticks: u64,
    pub drain_ticks: u64,
    pub max_tick_gap_ms: u64,
    pub orders_placed: u64,
    pub reprice_cancels: u64,
    pub queue_preserved_ticks: u64,
    pub passive_open_fills: u64,
    pub passive_exit_fills: u64,
    pub hazard_ioc_fills: u64,
    pub timeout_ioc_fills: u64,
    pub toxic_sweep_hits: u64,
    pub leader_veto_ticks: u64,
    pub toxicity_faded_ticks: u64,
    pub peak_abs_inventory_base: f64,
    pub terminal_open_inventory_base: f64,
    pub daily_loss_risk_scale_end: f64,
    pub warmup_ledger: CycleCashSummary,
    /// Eval-window cash only. Does NOT include the terminal reduce-only drain (see `drain_ledger`).
    pub eval_ledger: CycleCashSummary,
    /// Terminal drain (post-eval reduce-only unwind) cash, kept separate from the eval figure.
    pub drain_ledger: CycleCashSummary,
}

#[derive(Debug, Clone, Copy)]
struct SimOrder {
    slot: OrderSlotKind,
    client_order_index: i64,
    side: Side,
    price: f64,
    qty: f64,
    reduce_only: bool,
    placed_at_ms: u64,
}

fn forward_1s_markout_usd(
    ticks: &[ReplayTick],
    tick_idx: usize,
    now_ms: u64,
    is_buy: bool,
    qty: f64,
    fill_price: f64,
) -> Option<f64> {
    let target_ms = now_ms.saturating_add(1_000);
    let forward = ticks
        .get(tick_idx + 1..)?
        .iter()
        .find(|t| t.market.now_ms >= target_ms)?;
    let mid_1s = 0.5 * (forward.market.bid_px + forward.market.ask_px);
    if !mid_1s.is_finite() || mid_1s <= 0.0 {
        return None;
    }
    let sign = if is_buy { 1.0 } else { -1.0 };
    Some(sign * qty * (mid_1s - fill_price))
}

fn sim_utc_day(now_ms: u64) -> i32 {
    let days_since_unix = (now_ms / 86_400_000).min(i32::MAX as u64 - 719_163) as i32;
    719_163 + days_since_unix
}

/// Run a deterministic single continuous replay across `warmup -> eval -> drain` without
/// resetting inventory, equity, or drawdown state.
pub fn run_continuous_pipeline_replay(
    cfg: &PipelineReplayConfig,
    microprice: Option<MarkovMicropriceTable>,
    ticks: &[ReplayTick],
) -> Result<PipelineReplayReport> {
    cfg.validate()?;
    ensure!(!ticks.is_empty(), "replay tape must not be empty");

    let mut pipeline = QuotePipeline::new(cfg.pipeline.clone(), microprice)?;
    for &(wallet_key, tier) in &cfg.pre_registered_wallets {
        pipeline.upsert_wallet_tier(wallet_key, tier)?;
    }

    let mut tracker = QuoteSlotTracker::new(cfg.slot_diff)?;
    let mut daily_loss = DailyLossGuard::open(
        None,
        cfg.daily_loss_limit_pct,
        cfg.initial_capital_usd,
        sim_utc_day(cfg.window.warmup_start_ms),
    )?;

    let mut warmup_ledger = InventoryCycleCashLedger::new();
    let mut eval_ledger = InventoryCycleCashLedger::new();
    // Terminal drain (reduce-only unwind after eval_end) is accounted separately so its
    // forced-exit PnL never contaminates the reported 10h eval cash figure
    // (AGENTS.md research contract: record pre-window warm-up and terminal drain separately).
    let mut drain_ledger = InventoryCycleCashLedger::new();
    let mut sim_open_bid: Option<SimOrder> = None;
    let mut sim_open_ask: Option<SimOrder> = None;
    let mut sim_exit: Option<SimOrder> = None;
    let mut pending_markouts: VecDeque<PendingFillMarkout> = VecDeque::new();
    let mut exit_queue_tracker: Option<QueueAheadTracker> = None;

    let mut signed_inv = 0.0_f64;
    let mut avg_entry_price = 0.0_f64;
    let mut entry_opened_at_ms: Option<u64> = None;
    let mut cash_usd = cfg.initial_capital_usd;
    let mut next_coi = 1_i64;
    let mut prev_ms = 0_u64;
    let mut max_tick_gap_ms = 0_u64;
    let mut first_eval_ms: Option<u64> = None;
    let mut last_eval_ms: Option<u64> = None;
    let mut prev_eval_ms: Option<u64> = None;
    let mut max_eval_gap_ms = 0_u64;

    let mut warmup_ticks = 0_u64;
    let mut eval_ticks = 0_u64;
    let mut drain_ticks = 0_u64;
    let mut orders_placed = 0_u64;
    let mut reprice_cancels = 0_u64;
    let mut queue_preserved_ticks = 0_u64;
    let mut passive_open_fills = 0_u64;
    let mut passive_exit_fills = 0_u64;
    let mut hazard_ioc_fills = 0_u64;
    let mut timeout_ioc_fills = 0_u64;
    let mut toxic_sweep_hits = 0_u64;
    let mut leader_veto_ticks = 0_u64;
    let mut toxicity_faded_ticks = 0_u64;
    let mut peak_abs_inventory_base = 0.0_f64;

    for (tick_idx, tick) in ticks.iter().enumerate() {
        let now_ms = tick.market.now_ms;
        ensure!(now_ms > 0, "tick {tick_idx}: now_ms must be > 0");
        ensure!(
            now_ms >= cfg.window.warmup_start_ms && now_ms <= cfg.window.drain_end_ms,
            "tick {tick_idx}: timestamp {now_ms} outside [warmup_start_ms, drain_end_ms]"
        );
        let dt_ms = if prev_ms > 0 {
            ensure!(
                now_ms > prev_ms,
                "tick {tick_idx}: non-increasing timestamp {now_ms} <= {prev_ms}"
            );
            let gap = now_ms - prev_ms;
            max_tick_gap_ms = max_tick_gap_ms.max(gap);
            gap
        } else {
            0
        };
        prev_ms = now_ms;

        let in_warmup = now_ms < cfg.window.eval_start_ms;
        let in_drain = now_ms > cfg.window.eval_end_ms;
        if in_warmup {
            warmup_ticks += 1;
        } else if in_drain {
            drain_ticks += 1;
        } else {
            eval_ticks += 1;
            if first_eval_ms.is_none() {
                first_eval_ms = Some(now_ms);
            }
            if let Some(prev_eval) = prev_eval_ms {
                max_eval_gap_ms = max_eval_gap_ms.max(now_ms.saturating_sub(prev_eval));
            }
            prev_eval_ms = Some(now_ms);
            last_eval_ms = Some(now_ms);
        }

        let tick_mid = 0.5 * (tick.market.bid_px + tick.market.ask_px);
        while let Some(front) = pending_markouts.front().copied() {
            if now_ms < front.fill_ms.saturating_add(CAUSAL_MARKOUT_HORIZON_MS) {
                break;
            }
            pending_markouts.pop_front();
            if front.fill_price > 0.0 && tick_mid.is_finite() && tick_mid > 0.0 {
                let signed_bps = if front.is_buy {
                    ((tick_mid - front.fill_price) / front.fill_price) * 10_000.0
                } else {
                    ((front.fill_price - tick_mid) / front.fill_price) * 10_000.0
                };
                if signed_bps.is_finite() {
                    pipeline.observe_fill_markout_bps(signed_bps);
                }
            }
        }
        if dt_ms > 0
            && signed_inv.abs() > QTY_EPS
            && let Some(funding_rate_bps) = tick.market.funding_rate_bps_per_hour
        {
            ensure!(
                funding_rate_bps.is_finite(),
                "tick {tick_idx}: funding_rate_bps_per_hour must be finite"
            );
            let dt_hours = (dt_ms as f64) / 3_600_000.0;
            let funding_usd = -signed_inv * tick_mid * (funding_rate_bps / 10_000.0) * dt_hours;
            ensure!(
                funding_usd.is_finite(),
                "tick {tick_idx}: non-finite funding"
            );
            cash_usd += funding_usd;
            let phase_ledger = if in_warmup {
                &mut warmup_ledger
            } else if in_drain {
                &mut drain_ledger
            } else {
                &mut eval_ledger
            };
            phase_ledger.record_funding(funding_usd)?;
        }

        // Step A: Settle fills against currently resting orders BEFORE this tick's cancel/replace.
        if let Some(sweep) = tick.taker_sweep {
            ensure!(
                sweep.sweep_size_base.is_finite() && sweep.sweep_size_base > 0.0,
                "tick {tick_idx}: sweep_size_base must be finite and > 0"
            );
        }

        let mut check_passive_fill = |slot_opt: &mut Option<SimOrder>,
                                      is_exit: bool|
         -> Result<()> {
            let Some(order) = *slot_opt else {
                return Ok(());
            };
            let rested_ms = now_ms.saturating_sub(order.placed_at_ms);
            let swept = tick.taker_sweep.is_some_and(|s| {
                let maker_side_matches = match order.side {
                    Side::Buy => !s.taker_is_buy,
                    Side::Sell => s.taker_is_buy,
                };
                let touch_ok = match order.side {
                    Side::Buy => order.price >= tick.market.bid_px - tick.market.tick_size - PX_EPS,
                    Side::Sell => {
                        order.price <= tick.market.ask_px + tick.market.tick_size + PX_EPS
                    }
                };
                maker_side_matches && touch_ok
            });
            let price_through = match order.side {
                Side::Buy => tick.market.ask_px <= order.price + PX_EPS,
                Side::Sell => tick.market.bid_px >= order.price - PX_EPS,
            };
            let stride_touch = cfg.passive_touch_fill_stride > 0
                && (tick_idx as u64).is_multiple_of(cfg.passive_touch_fill_stride)
                && rested_ms >= cfg.queue_fill_delay_ms
                && match order.side {
                    Side::Buy => order.price >= tick.market.bid_px - PX_EPS,
                    Side::Sell => order.price <= tick.market.ask_px + PX_EPS,
                };

            let fill_cap = if let Some(s) = tick.taker_sweep
                && swept
            {
                s.sweep_size_base
            } else {
                order.qty
            };
            if (swept && rested_ms >= 10) || price_through || stride_touch {
                let raw_qty = if order.reduce_only {
                    let reducible = if order.side == Side::Buy {
                        (-signed_inv).max(0.0)
                    } else {
                        signed_inv.max(0.0)
                    };
                    order.qty.min(fill_cap).min(reducible)
                } else {
                    order.qty.min(fill_cap)
                };
                let fqty = quantize_lot_floor(raw_qty, cfg.slot_diff.lot_step);
                if fqty >= cfg.slot_diff.lot_step - QTY_EPS {
                    let fee_usd = fqty * order.price * (cfg.maker_fee_bps / 10_000.0);
                    let exit_mode = if is_exit {
                        passive_exit_fills += 1;
                        FillExitMode::PassiveReduceOnly
                    } else {
                        passive_open_fills += 1;
                        FillExitMode::OpeningOrAdding
                    };
                    if swept {
                        toxic_sweep_hits += 1;
                    }
                    let markout_est = forward_1s_markout_usd(
                        ticks,
                        tick_idx,
                        now_ms,
                        order.side == Side::Buy,
                        fqty,
                        order.price,
                    );
                    apply_fill_state(
                        &mut signed_inv,
                        &mut avg_entry_price,
                        &mut entry_opened_at_ms,
                        &mut cash_usd,
                        now_ms,
                        order.side == Side::Buy,
                        fqty,
                        order.price,
                        fee_usd,
                    );
                    let target_ledger = if in_warmup {
                        &mut warmup_ledger
                    } else if in_drain {
                        &mut drain_ledger
                    } else {
                        &mut eval_ledger
                    };
                    target_ledger.record_fill(CycleFillInput {
                        is_buy: order.side == Side::Buy,
                        qty: fqty,
                        fill_price: order.price,
                        fair_mid: tick_mid,
                        fee_usd,
                        exit_mode,
                        markout_1s_usd: markout_est,
                    })?;
                    pending_markouts.push_back(PendingFillMarkout {
                        fill_ms: now_ms,
                        is_buy: order.side == Side::Buy,
                        fill_price: order.price,
                    });
                    if is_exit {
                        pipeline.notify_exit_partial_fill(fqty)?;
                        if let Some(qt) = exit_queue_tracker.as_mut() {
                            qt.on_partial_fill(fqty);
                        }
                    }
                    ensure!(
                        tracker.notify_fill(order.slot, order.client_order_index, fqty)?,
                        "replay fill does not match the resting slot's client order"
                    );
                    if order.qty - fqty <= QTY_EPS {
                        *slot_opt = None;
                        if is_exit {
                            exit_queue_tracker = None;
                        }
                    } else if let Some(existing) = slot_opt.as_mut() {
                        existing.qty -= fqty;
                    }
                }
            }
            Ok(())
        };

        check_passive_fill(&mut sim_exit, true)?;
        check_passive_fill(&mut sim_open_bid, false)?;
        check_passive_fill(&mut sim_open_ask, false)?;

        // Step B: Observe public taker sweep for causal toxicity shielding on subsequent quotes.
        if let Some(sweep) = tick.taker_sweep {
            let _ = pipeline.observe_public_taker_sweep(
                now_ms,
                sweep.wallet_key,
                sweep.taker_is_buy,
                sweep.is_twap_sub,
            )?;
        }

        // Update continuous mark-to-market equity for DailyLossGuard (breach drops risk_scale to 0).
        let equity_usd = cash_usd + signed_inv * tick_mid;
        ensure!(
            equity_usd.is_finite(),
            "non-finite replay equity at tick {tick_idx}"
        );
        let _ = daily_loss.check(equity_usd, sim_utc_day(now_ms));
        peak_abs_inventory_base = peak_abs_inventory_base.max(signed_inv.abs());

        let inv_state = GenericInventoryState {
            signed_inventory_base: signed_inv,
            avg_entry_price,
            entry_opened_at_ms,
            dust_base: cfg.slot_diff.lot_step * 0.5,
            daily_loss_risk_scale: if in_drain {
                0.0
            } else {
                daily_loss.risk_scale()
            },
        };

        let mut eval_market = tick.market;
        if cfg.pipeline.hybrid_exit.queue_exit.enabled
            && eval_market.exit_queue_ahead_base.is_none()
            && let Some(qt) = exit_queue_tracker.as_mut()
        {
            let exit_sweep_hit = tick.taker_sweep.map_or(0.0, |s| {
                let matches_exit = sim_exit.is_some_and(|eo| match eo.side {
                    Side::Buy => !s.taker_is_buy,
                    Side::Sell => s.taker_is_buy,
                });
                if matches_exit { s.sweep_size_base } else { 0.0 }
            });
            eval_market.exit_queue_ahead_base = Some(qt.on_book_and_trade(
                tick.market.bid_px,
                tick.market.bid_sz,
                tick.market.ask_px,
                tick.market.ask_sz,
                exit_sweep_hit,
            ));
        }

        let mut plan =
            pipeline.evaluate_with_leader(&eval_market, &inv_state, tick.leader.as_ref())?;
        if in_drain {
            plan.bid_open = None;
            plan.ask_open = None;
        }
        if plan.leader_vetoed_bid || plan.leader_vetoed_ask {
            leader_veto_ticks += 1;
        }
        if plan.bid_open.is_some_and(|q| q.faded_by_toxicity)
            || plan.ask_open.is_some_and(|q| q.faded_by_toxicity)
        {
            toxicity_faded_ticks += 1;
        }

        let actions = tracker.diff_plan(&plan, tick.market.tick_size, OrderType::PostOnly)?;
        if actions.is_empty()
            && (sim_open_bid.is_some() || sim_open_ask.is_some() || sim_exit.is_some())
        {
            queue_preserved_ticks += 1;
        }

        let mut place_results = Vec::new();
        for act in &actions {
            match act {
                DriverOrderAction::Cancel {
                    slot,
                    client_order_index,
                } => {
                    reprice_cancels += 1;
                    match slot {
                        OrderSlotKind::OpeningBid => {
                            if sim_open_bid
                                .is_some_and(|o| o.client_order_index == *client_order_index)
                            {
                                sim_open_bid = None;
                            }
                        }
                        OrderSlotKind::OpeningAsk => {
                            if sim_open_ask
                                .is_some_and(|o| o.client_order_index == *client_order_index)
                            {
                                sim_open_ask = None;
                            }
                        }
                        OrderSlotKind::ReduceOnlyExit => {
                            if sim_exit.is_some_and(|o| o.client_order_index == *client_order_index)
                            {
                                sim_exit = None;
                                exit_queue_tracker = None;
                            }
                        }
                    }
                }
                DriverOrderAction::Place { slot, params, .. } => {
                    let coi = next_coi;
                    next_coi += 1;
                    orders_placed += 1;
                    place_results.push(crate::exchange::PlaceResult {
                        client_order_index: coi,
                        side: params.side,
                        price: params.price,
                        size: params.size,
                    });

                    if params.order_type == OrderType::Ioc {
                        let marketable = match params.side {
                            Side::Buy => params.price >= tick.market.ask_px - PX_EPS,
                            Side::Sell => params.price <= tick.market.bid_px + PX_EPS,
                        };
                        let reducible = if params.side == Side::Buy {
                            (-signed_inv).max(0.0)
                        } else {
                            signed_inv.max(0.0)
                        };
                        let cap = if params.reduce_only {
                            params.size.min(reducible)
                        } else {
                            params.size
                        };
                        let exec_qty = quantize_lot_floor(cap, cfg.slot_diff.lot_step);
                        if marketable && exec_qty >= cfg.slot_diff.min_order_qty {
                            let fill_px = if params.side == Side::Buy {
                                tick.market.ask_px
                            } else {
                                tick.market.bid_px
                            };
                            let fee_usd = exec_qty * fill_px * (cfg.taker_fee_bps / 10_000.0);
                            match plan.exit_directive {
                                ExitDirective::EarlyHazardHedgeLock { .. } => hazard_ioc_fills += 1,
                                ExitDirective::TimeoutFlatten { .. } => timeout_ioc_fills += 1,
                                _ => {}
                            }
                            apply_fill_state(
                                &mut signed_inv,
                                &mut avg_entry_price,
                                &mut entry_opened_at_ms,
                                &mut cash_usd,
                                now_ms,
                                params.side == Side::Buy,
                                exec_qty,
                                fill_px,
                                fee_usd,
                            );
                            if *slot == OrderSlotKind::ReduceOnlyExit {
                                pipeline.notify_exit_partial_fill(exec_qty)?;
                            }
                            let target_ledger = if in_warmup {
                                &mut warmup_ledger
                            } else if in_drain {
                                &mut drain_ledger
                            } else {
                                &mut eval_ledger
                            };
                            let markout_est = forward_1s_markout_usd(
                                ticks,
                                tick_idx,
                                now_ms,
                                params.side == Side::Buy,
                                exec_qty,
                                fill_px,
                            );
                            target_ledger.record_fill(CycleFillInput {
                                is_buy: params.side == Side::Buy,
                                qty: exec_qty,
                                fill_price: fill_px,
                                fair_mid: tick_mid,
                                fee_usd,
                                exit_mode: FillExitMode::TakerOrHazardClose,
                                markout_1s_usd: markout_est,
                            })?;
                            pending_markouts.push_back(PendingFillMarkout {
                                fill_ms: now_ms,
                                is_buy: params.side == Side::Buy,
                                fill_price: fill_px,
                            });
                            if *slot == OrderSlotKind::ReduceOnlyExit
                                && let Some(qt) = exit_queue_tracker.as_mut()
                            {
                                qt.on_partial_fill(exec_qty);
                            }
                        }
                    } else {
                        let sim_order = SimOrder {
                            slot: *slot,
                            client_order_index: coi,
                            side: params.side,
                            price: params.price,
                            qty: params.size,
                            reduce_only: params.reduce_only,
                            placed_at_ms: now_ms,
                        };
                        match slot {
                            OrderSlotKind::OpeningBid => sim_open_bid = Some(sim_order),
                            OrderSlotKind::OpeningAsk => sim_open_ask = Some(sim_order),
                            OrderSlotKind::ReduceOnlyExit => {
                                sim_exit = Some(sim_order);
                                if cfg.pipeline.hybrid_exit.queue_exit.enabled {
                                    let init_touch_sz = if params.side == Side::Buy {
                                        tick.market.bid_sz
                                    } else {
                                        tick.market.ask_sz
                                    };
                                    exit_queue_tracker = Some(QueueAheadTracker::new(
                                        params.side == Side::Buy,
                                        params.price,
                                        params.size,
                                        init_touch_sz.max(0.0),
                                    )?);
                                }
                            }
                        }
                    }
                }
            }
        }
        let batch_res = BatchResult {
            tx_hashes: vec!["replay".into()],
            place_results,
        };
        tracker.apply_batch_result(&actions, &batch_res)?;
    }

    let (observed_eval_span_ms, effective_max_eval_gap_ms) =
        if let (Some(first), Some(last)) = (first_eval_ms, last_eval_ms) {
            let lead_gap = first.saturating_sub(cfg.window.eval_start_ms);
            let tail_gap = cfg.window.eval_end_ms.saturating_sub(last);
            (
                last.saturating_sub(first),
                max_eval_gap_ms.max(lead_gap).max(tail_gap),
            )
        } else {
            (0, cfg.window.eval_duration_ms())
        };
    let is_full_10h_continuous = cfg.window.is_full_10h_continuous()
        && eval_ticks >= 2
        && observed_eval_span_ms >= FULL_10H_CONTINUOUS_MS
        && effective_max_eval_gap_ms <= DEFAULT_MAX_CONTINUOUS_EVAL_GAP_MS;

    Ok(PipelineReplayReport {
        mode: cfg.pipeline.mode,
        eval_duration_ms: cfg.window.eval_duration_ms(),
        observed_eval_span_ms,
        max_eval_gap_ms: effective_max_eval_gap_ms,
        is_full_10h_continuous,
        warmup_ticks,
        eval_ticks,
        drain_ticks,
        max_tick_gap_ms,
        orders_placed,
        reprice_cancels,
        queue_preserved_ticks,
        passive_open_fills,
        passive_exit_fills,
        hazard_ioc_fills,
        timeout_ioc_fills,
        toxic_sweep_hits,
        leader_veto_ticks,
        toxicity_faded_ticks,
        peak_abs_inventory_base,
        terminal_open_inventory_base: signed_inv,
        daily_loss_risk_scale_end: daily_loss.risk_scale(),
        warmup_ledger: warmup_ledger.summary(),
        eval_ledger: eval_ledger.summary(),
        drain_ledger: drain_ledger.summary(),
    })
}

#[allow(clippy::too_many_arguments)]
fn apply_fill_state(
    signed_inv: &mut f64,
    avg_entry: &mut f64,
    entry_opened_at_ms: &mut Option<u64>,
    cash_usd: &mut f64,
    now_ms: u64,
    is_buy: bool,
    qty: f64,
    price: f64,
    fee_usd: f64,
) {
    let signed_delta = if is_buy { qty } else { -qty };
    *cash_usd -= signed_delta * price + fee_usd;

    let prev = *signed_inv;
    let next = prev + signed_delta;
    if prev.abs() <= QTY_EPS || prev.signum() == signed_delta.signum() {
        let prev_abs = prev.abs();
        let next_abs = next.abs();
        if next_abs > QTY_EPS {
            *avg_entry = (prev_abs * (*avg_entry) + qty * price) / next_abs;
        }
        if entry_opened_at_ms.is_none() {
            *entry_opened_at_ms = Some(now_ms);
        }
    } else if next.abs() <= QTY_EPS {
        *avg_entry = 0.0;
        *entry_opened_at_ms = None;
    } else if next.signum() != prev.signum() {
        *avg_entry = price;
        *entry_opened_at_ms = Some(now_ms);
    }
    *signed_inv = if next.abs() <= QTY_EPS { 0.0 } else { next };
}

/// Paired diagnostic report emitted by `cross_venue pipeline-bench`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PipelineBenchmarkReport {
    pub disclaimer: String,
    pub naive_reprice_arm: PipelineReplayReport,
    pub shielded_hybrid_arm: PipelineReplayReport,
    pub cancel_reduction_count: i64,
    pub net_cash_improvement_usd: f64,
}

/// Deterministic offline stress benchmark comparing queue-churning unshielded quoting vs
/// `HybridExitController` + deadband queue preservation + `WalletToxicityShield` + `LeadLagConfig`.
pub fn run_pipeline_benchmark() -> Result<PipelineBenchmarkReport> {
    let toxic_wallet = salted_wallet_key(42, "toxic_taker");
    let window = ReplayWindowSpec {
        warmup_start_ms: 1_000_000,
        eval_start_ms: 1_005_000,
        eval_end_ms: 1_065_000,
        drain_end_ms: 1_080_000,
    };

    let mut ticks = Vec::new();
    let mut mid = 100.00_f64;
    for step in 0..80_u64 {
        let now_ms = 1_000_000 + step * 1_000;
        let in_toxic_shock = (20..=26).contains(&step) || (50..=55).contains(&step);
        if in_toxic_shock {
            mid += 0.03;
        } else if step % 2 == 0 {
            mid += 0.01;
        } else {
            mid -= 0.01;
        }
        let tick_sz = 0.01_f64;
        let bid_px = ((mid - 0.01) / tick_sz).round() * tick_sz;
        let ask_px = ((bid_px + 0.02) / tick_sz).round() * tick_sz;
        let leader = if in_toxic_shock {
            Some(ExternalLeaderSignal {
                leader_venue_id: 2,
                reference_mid_px: mid + 0.06,
                leader_return_bps: 6.0,
                source_ms: now_ms.saturating_sub(20),
                receipt_ms: now_ms.saturating_sub(5),
            })
        } else {
            None
        };
        let taker_sweep = if in_toxic_shock {
            Some(ReplayTakerSweep {
                wallet_key: toxic_wallet,
                taker_is_buy: true,
                is_twap_sub: false,
                sweep_size_base: 2.0,
            })
        } else {
            None
        };
        ticks.push(ReplayTick {
            market: GenericMarketState {
                now_ms,
                bid_px,
                ask_px,
                bid_sz: 5.0,
                ask_sz: 5.0,
                tick_size: tick_sz,
                momentum_3s_bps: if in_toxic_shock { 2.5 } else { 0.0 },
                oracle_px: Some(mid),
                funding_rate_bps_per_hour: None,
                exit_queue_ahead_base: None,
            },
            leader,
            taker_sweep,
        });
    }

    let base_pipeline = QuotePipelineConfig {
        mode: StrategyMode::NeutralMarketMaking,
        glft: crate::strategy::glft::GlftParams {
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
        hybrid_exit: crate::strategy::hybrid_exit::HybridExitConfig::default(),
        lead_lag: LeadLagConfig::default(),
        markout_shield: crate::strategy::quote_pipeline::MarkoutShieldConfig::default(),
        funding_bias: Default::default(),
        inventory_clip: Default::default(),
        max_oracle_divergence_bps: 25.0,
        base_order_qty: 1.0,
    };

    let naive_cfg = PipelineReplayConfig {
        pipeline: base_pipeline.clone(),
        slot_diff: SlotDiffConfig {
            reprice_deadband_ticks: 0,
            min_size_change_frac: 0.0,
            lot_step: 0.01,
            min_order_qty: 0.01,
        },
        window,
        initial_capital_usd: 1_000.0,
        daily_loss_limit_pct: 0.05,
        maker_fee_bps: 0.0,
        taker_fee_bps: 3.5,
        queue_fill_delay_ms: 500,
        passive_touch_fill_stride: 3,
        pre_registered_wallets: vec![],
    };

    let mut shielded_pipeline = base_pipeline;
    shielded_pipeline.lead_lag.enabled = true;
    let shielded_cfg = PipelineReplayConfig {
        pipeline: shielded_pipeline,
        slot_diff: SlotDiffConfig {
            reprice_deadband_ticks: 2,
            min_size_change_frac: 0.15,
            lot_step: 0.01,
            min_order_qty: 0.01,
        },
        window,
        initial_capital_usd: 1_000.0,
        daily_loss_limit_pct: 0.05,
        maker_fee_bps: 0.0,
        taker_fee_bps: 3.5,
        queue_fill_delay_ms: 500,
        passive_touch_fill_stride: 3,
        pre_registered_wallets: vec![(toxic_wallet, WalletToxicityTier::Decile(10))],
    };

    let naive_reprice_arm = run_continuous_pipeline_replay(&naive_cfg, None, &ticks)?;
    let shielded_hybrid_arm = run_continuous_pipeline_replay(&shielded_cfg, None, &ticks)?;
    let cancel_reduction_count =
        naive_reprice_arm.reprice_cancels as i64 - shielded_hybrid_arm.reprice_cancels as i64;
    let net_cash_improvement_usd = shielded_hybrid_arm.eval_ledger.paired_cycle_net_cash_usd
        - naive_reprice_arm.eval_ledger.paired_cycle_net_cash_usd;

    Ok(PipelineBenchmarkReport {
        disclaimer: "synthetic_diagnostic_not_10h_oos_evidence".into(),
        naive_reprice_arm,
        shielded_hybrid_arm,
        cancel_reduction_count,
        net_cash_improvement_usd,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exchange::{
        BatchOrderParams, BatchOrderResult, OrderInfo, OrderResult, PlaceResult,
    };
    use crate::strategy::glft::GlftParams;
    use crate::strategy::hybrid_exit::HybridExitConfig;
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct MockBatchExchange {
        batches: Arc<Mutex<Vec<Vec<BatchAction>>>>,
        fail_next: Arc<Mutex<bool>>,
        next_idx: Arc<Mutex<i64>>,
    }

    #[async_trait]
    impl Exchange for MockBatchExchange {
        async fn buy(&self, _size: f64, _price: f64) -> Result<OrderResult> {
            unreachable!()
        }
        async fn sell(&self, _size: f64, _price: f64) -> Result<OrderResult> {
            unreachable!()
        }
        async fn place_batch(&self, _params: BatchOrderParams) -> Result<BatchOrderResult> {
            unreachable!()
        }
        async fn cancel_order(&self, _order_id: i64) -> Result<()> {
            Ok(())
        }
        async fn cancel_all(&self) -> Result<u32> {
            Ok(0)
        }
        async fn get_active_orders(&self) -> Result<Vec<OrderInfo>> {
            Ok(vec![])
        }
        async fn close_all_positions(&self, _current_price: f64) -> Result<()> {
            Ok(())
        }
        async fn execute_batch(&self, actions: Vec<BatchAction>) -> Result<BatchResult> {
            if *self.fail_next.lock().unwrap() {
                anyhow::bail!("simulated batch failure");
            }
            let mut place_results = Vec::new();
            let mut next = self.next_idx.lock().unwrap();
            for act in &actions {
                if let BatchAction::Place(p) = act {
                    *next += 1;
                    place_results.push(PlaceResult {
                        client_order_index: *next,
                        side: p.side,
                        price: p.price,
                        size: p.size,
                    });
                }
            }
            self.batches.lock().unwrap().push(actions);
            Ok(BatchResult {
                tx_hashes: vec!["mock-tx".into()],
                place_results,
            })
        }
        async fn get_account_stats(
            &self,
        ) -> Result<crate::strategy::inventory_neutral_mm::AccountStats> {
            unreachable!()
        }
        fn limit_order_type(&self) -> OrderType {
            OrderType::PostOnly
        }
    }

    fn base_pipe_cfg() -> QuotePipelineConfig {
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
            markout_shield: crate::strategy::quote_pipeline::MarkoutShieldConfig::default(),
            funding_bias: Default::default(),
            inventory_clip: Default::default(),
            max_oracle_divergence_bps: 25.0,
            base_order_qty: 1.0,
        }
    }

    #[test]
    fn slot_tracker_preserves_queue_within_deadband_and_replaces_immediately_on_toxicity() {
        let mut tracker = QuoteSlotTracker::new(SlotDiffConfig {
            reprice_deadband_ticks: 1,
            min_size_change_frac: 0.15,
            lot_step: 0.01,
            min_order_qty: 0.01,
        })
        .unwrap();
        let make_plan = |ask_px: f64, faded: bool| QuotePlan {
            mode: StrategyMode::NeutralMarketMaking,
            fair_mid: 100.0,
            lead_lag_shift_bps: 0.0,
            funding_shift_bps: 0.0,
            leader_stale: false,
            leader_vetoed_bid: false,
            leader_vetoed_ask: false,
            oracle_divergence_bps: None,
            oracle_reduce_only: false,
            bid_open: Some(OpeningQuote {
                side: QuoteSide::Bid,
                price: 99.90,
                qty: 1.0,
                spread_offset_bps: 10.0,
                faded_by_toxicity: false,
            }),
            ask_open: Some(OpeningQuote {
                side: QuoteSide::Ask,
                price: ask_px,
                qty: 1.0,
                spread_offset_bps: 10.0,
                faded_by_toxicity: faded,
            }),
            exit_directive: ExitDirective::Flat,
        };

        let acts1 = tracker
            .diff_plan(&make_plan(100.10, false), 0.01, OrderType::PostOnly)
            .unwrap();
        assert_eq!(acts1.len(), 2);
        tracker
            .apply_batch_result(
                &acts1,
                &BatchResult {
                    tx_hashes: vec![],
                    place_results: vec![
                        PlaceResult {
                            client_order_index: 10,
                            side: Side::Buy,
                            price: 99.90,
                            size: 1.0,
                        },
                        PlaceResult {
                            client_order_index: 11,
                            side: Side::Sell,
                            price: 100.10,
                            size: 1.0,
                        },
                    ],
                },
            )
            .unwrap();

        // 1-tick drift inside deadband -> ZERO cancel/place churn!
        let acts2 = tracker
            .diff_plan(&make_plan(100.11, false), 0.01, OrderType::PostOnly)
            .unwrap();
        assert!(acts2.is_empty());

        // Toxicity engages even within price deadband -> MUST cancel+replace Ask immediately!
        let acts3 = tracker
            .diff_plan(&make_plan(100.11, true), 0.01, OrderType::PostOnly)
            .unwrap();
        assert_eq!(acts3.len(), 2);
        assert!(matches!(
            acts3[0],
            DriverOrderAction::Cancel {
                slot: OrderSlotKind::OpeningAsk,
                client_order_index: 11
            }
        ));
    }

    #[test]
    fn apply_batch_result_binds_by_side_price_size_not_position_and_fails_closed_on_no_match() {
        let slot_diff = SlotDiffConfig {
            reprice_deadband_ticks: 1,
            min_size_change_frac: 0.15,
            lot_step: 0.01,
            min_order_qty: 0.01,
        };
        let make_plan = || QuotePlan {
            mode: StrategyMode::NeutralMarketMaking,
            fair_mid: 100.0,
            lead_lag_shift_bps: 0.0,
            funding_shift_bps: 0.0,
            leader_stale: false,
            leader_vetoed_bid: false,
            leader_vetoed_ask: false,
            oracle_divergence_bps: None,
            oracle_reduce_only: false,
            bid_open: Some(OpeningQuote {
                side: QuoteSide::Bid,
                price: 99.90,
                qty: 1.0,
                spread_offset_bps: 10.0,
                faded_by_toxicity: false,
            }),
            ask_open: Some(OpeningQuote {
                side: QuoteSide::Ask,
                price: 100.10,
                qty: 1.0,
                spread_offset_bps: 10.0,
                faded_by_toxicity: false,
            }),
            exit_directive: ExitDirective::Flat,
        };

        let mut tracker = QuoteSlotTracker::new(slot_diff).unwrap();
        let acts = tracker
            .diff_plan(&make_plan(), 0.01, OrderType::PostOnly)
            .unwrap();
        assert_eq!(acts.len(), 2);

        // Venue returns place_results in the OPPOSITE order to the emitted placements.
        // Positional zip would bind the Sell index into the Buy slot; (side,price,size)
        // matching must bind Buy->10 and Sell->11 regardless of result order.
        tracker
            .apply_batch_result(
                &acts,
                &BatchResult {
                    tx_hashes: vec![],
                    place_results: vec![
                        PlaceResult {
                            client_order_index: 11,
                            side: Side::Sell,
                            price: 100.10,
                            size: 1.0,
                        },
                        PlaceResult {
                            client_order_index: 10,
                            side: Side::Buy,
                            price: 99.90,
                            size: 1.0,
                        },
                    ],
                },
            )
            .unwrap();

        // A subsequent toxicity-forced ask replace must cancel the RIGHT index (11), proving
        // the sell index did not leak into the bid slot.
        let plan_toxic = {
            let mut p = make_plan();
            if let Some(a) = p.ask_open.as_mut() {
                a.faded_by_toxicity = true;
            }
            p
        };
        let acts_tox = tracker
            .diff_plan(&plan_toxic, 0.01, OrderType::PostOnly)
            .unwrap();
        assert!(acts_tox.iter().any(|a| matches!(
            a,
            DriverOrderAction::Cancel {
                slot: OrderSlotKind::OpeningAsk,
                client_order_index: 11
            }
        )));

        // Fail-closed: a place_result that matches no emitted placement must error and
        // invalidate all slots rather than silently bind the wrong order.
        let mut tracker2 = QuoteSlotTracker::new(slot_diff).unwrap();
        let acts2 = tracker2
            .diff_plan(&make_plan(), 0.01, OrderType::PostOnly)
            .unwrap();
        let bad = tracker2.apply_batch_result(
            &acts2,
            &BatchResult {
                tx_hashes: vec![],
                place_results: vec![
                    PlaceResult {
                        client_order_index: 10,
                        side: Side::Buy,
                        price: 99.90,
                        size: 1.0,
                    },
                    PlaceResult {
                        // Wrong price: matches no emitted placement.
                        client_order_index: 11,
                        side: Side::Sell,
                        price: 123.45,
                        size: 1.0,
                    },
                ],
            },
        );
        assert!(bad.is_err());

        // Fail-closed: duplicate client_order_index in BatchResult must error and dirty tracker.
        let mut tracker3 = QuoteSlotTracker::new(slot_diff).unwrap();
        let acts3 = tracker3
            .diff_plan(&make_plan(), 0.01, OrderType::PostOnly)
            .unwrap();
        let dup_id = tracker3.apply_batch_result(
            &acts3,
            &BatchResult {
                tx_hashes: vec![],
                place_results: vec![
                    PlaceResult {
                        client_order_index: 77,
                        side: Side::Buy,
                        price: 99.90,
                        size: 1.0,
                    },
                    PlaceResult {
                        client_order_index: 77,
                        side: Side::Sell,
                        price: 100.10,
                        size: 1.0,
                    },
                ],
            },
        );
        assert!(dup_id.is_err());
        assert!(tracker3.is_dirty());
    }

    #[tokio::test]
    async fn generic_exchange_driver_dispatches_reduce_only_and_fails_closed_on_batch_err() {
        let mock = MockBatchExchange::default();
        let pipe = QuotePipeline::new(base_pipe_cfg(), None).unwrap();
        let mut driver = GenericExchangeDriver::new(
            mock.clone(),
            pipe,
            SlotDiffConfig {
                lot_step: 0.01,
                min_order_qty: 0.01,
                ..SlotDiffConfig::default()
            },
        )
        .unwrap();

        let market = GenericMarketState {
            now_ms: 10_000,
            bid_px: 100.0,
            ask_px: 100.1,
            bid_sz: 5.0,
            ask_sz: 5.0,
            tick_size: 0.01,
            momentum_3s_bps: 0.0,
            oracle_px: Some(100.05),
            funding_rate_bps_per_hour: None,
            exit_queue_ahead_base: None,
        };
        let long_inv = GenericInventoryState {
            signed_inventory_base: 1.239,
            avg_entry_price: 100.0,
            entry_opened_at_ms: Some(9_000),
            dust_base: 1e-6,
            daily_loss_risk_scale: 1.0,
        };

        let (plan, count) = driver.step(&market, &long_inv, None).await.unwrap();
        assert!(matches!(
            plan.exit_directive,
            ExitDirective::PassiveReduce(_)
        ));
        assert_eq!(count, 2); // ReduceOnlyExit + OpeningBid (OpeningAsk suppressed while long)

        let dispatched = mock.batches.lock().unwrap()[0].clone();
        assert!(matches!(
            &dispatched[0],
            BatchAction::Place(p) if p.reduce_only && p.side == Side::Sell && (p.size - 1.23).abs() < 1e-9
        ));

        // On batch error, slots go dirty and refuse further steps until reconciled.
        *mock.fail_next.lock().unwrap() = true;
        let mut m2 = market;
        m2.now_ms = 16_000; // advances exit stage -> triggers reprice attempt
        assert!(driver.step(&m2, &long_inv, None).await.is_err());
        assert!(driver.slots().is_dirty());
        *mock.fail_next.lock().unwrap() = false;
        assert!(driver.step(&m2, &long_inv, None).await.is_err());
    }

    #[test]
    fn continuous_replay_rejects_regression_and_benchmark_reduces_churn() {
        let report = run_pipeline_benchmark().unwrap();
        assert!(!report.shielded_hybrid_arm.is_full_10h_continuous);
        assert!(report.shielded_hybrid_arm.eval_ticks > 0);
        assert!(report.cancel_reduction_count > 0);
        assert!(report.shielded_hybrid_arm.leader_veto_ticks > 0);

        // Regression test on non-monotonic timestamp.
        let cfg = PipelineReplayConfig {
            pipeline: base_pipe_cfg(),
            slot_diff: SlotDiffConfig::default(),
            window: ReplayWindowSpec {
                warmup_start_ms: 1000,
                eval_start_ms: 2000,
                eval_end_ms: 5000,
                drain_end_ms: 6000,
            },
            initial_capital_usd: 1000.0,
            daily_loss_limit_pct: 0.05,
            maker_fee_bps: 0.0,
            taker_fee_bps: 3.5,
            queue_fill_delay_ms: 100,
            passive_touch_fill_stride: 0,
            pre_registered_wallets: vec![],
        };
        let t1 = ReplayTick {
            market: GenericMarketState {
                now_ms: 2_500,
                bid_px: 100.0,
                ask_px: 100.1,
                bid_sz: 1.0,
                ask_sz: 1.0,
                tick_size: 0.1,
                momentum_3s_bps: 0.0,
                oracle_px: None,
                funding_rate_bps_per_hour: None,
                exit_queue_ahead_base: None,
            },
            leader: None,
            taker_sweep: None,
        };
        let mut t2 = t1.clone();
        t2.market.now_ms = 2_400;
        assert!(run_continuous_pipeline_replay(&cfg, None, &[t1, t2]).is_err());
    }

    #[test]
    fn replay_ioc_enforces_marketability_touch_fill_and_reduce_only_constraints() {
        let mut p_cfg = base_pipe_cfg();
        p_cfg.hybrid_exit.stage_1_max_hold_ms = 1_000;
        p_cfg.hybrid_exit.stage_2_max_hold_ms = 2_000;
        p_cfg.hybrid_exit.timeout_ms = 5_000;
        p_cfg.hybrid_exit.hedge_ioc_slippage_bps = 0.0;
        p_cfg.glft.max_half_spread_bps = 100.0;
        let cfg = PipelineReplayConfig {
            pipeline: p_cfg,
            slot_diff: SlotDiffConfig {
                reprice_deadband_ticks: 0,
                min_size_change_frac: 0.0,
                lot_step: 0.1,
                min_order_qty: 0.1,
            },
            window: ReplayWindowSpec {
                warmup_start_ms: 1_000,
                eval_start_ms: 2_000,
                eval_end_ms: 15_000,
                drain_end_ms: 20_000,
            },
            initial_capital_usd: 1_000.0,
            daily_loss_limit_pct: 0.25,
            maker_fee_bps: 0.0,
            taker_fee_bps: 5.0,
            queue_fill_delay_ms: 100,
            passive_touch_fill_stride: 0,
            pre_registered_wallets: vec![],
        };

        // Tick 0: places OpeningBid at ~1.0 (oracle 1.1 -> no oracle halt).
        // Tick 1 (bid=0.85, ask=0.95, oracle=1.1): fills OpeningBid (+1.0 long), and >25bp oracle
        // divergence prevents placing a fresh OpeningBid that would otherwise fill on Tick 2.
        // Tick 2 (10_000 > timeout_ms=5_000): timeout SELL IOC on `bid_px=0.05, tick=0.1` produces
        // clamped `limit_price = 0.10 > bid_px = 0.05` (UNMARKETABLE!) -> must NOT phantom-fill!
        // Tick 3 (11_000): `bid_px=0.15, tick=0.1` -> `limit_price = 0.10 <= bid_px` (MARKETABLE!)
        // -> fills strictly at touch `bid_px = 0.15` and closes long to flat 0.0.
        let ticks = vec![
            ReplayTick {
                market: GenericMarketState {
                    now_ms: 2_000,
                    bid_px: 1.0,
                    ask_px: 1.2,
                    bid_sz: 5.0,
                    ask_sz: 5.0,
                    tick_size: 0.1,
                    momentum_3s_bps: 0.0,
                    oracle_px: Some(1.1),
                    funding_rate_bps_per_hour: None,
                    exit_queue_ahead_base: None,
                },
                leader: None,
                taker_sweep: None,
            },
            ReplayTick {
                market: GenericMarketState {
                    now_ms: 2_500,
                    bid_px: 0.85,
                    ask_px: 0.95,
                    bid_sz: 5.0,
                    ask_sz: 5.0,
                    tick_size: 0.1,
                    momentum_3s_bps: 0.0,
                    oracle_px: Some(1.1),
                    funding_rate_bps_per_hour: None,
                    exit_queue_ahead_base: None,
                },
                leader: None,
                taker_sweep: None,
            },
            ReplayTick {
                market: GenericMarketState {
                    now_ms: 10_000,
                    bid_px: 0.05,
                    ask_px: 0.25,
                    bid_sz: 5.0,
                    ask_sz: 5.0,
                    tick_size: 0.1,
                    momentum_3s_bps: 0.0,
                    oracle_px: None,
                    funding_rate_bps_per_hour: None,
                    exit_queue_ahead_base: None,
                },
                leader: None,
                taker_sweep: None,
            },
            ReplayTick {
                market: GenericMarketState {
                    now_ms: 11_000,
                    bid_px: 0.15,
                    ask_px: 0.35,
                    bid_sz: 5.0,
                    ask_sz: 5.0,
                    tick_size: 0.1,
                    momentum_3s_bps: 0.0,
                    oracle_px: None,
                    funding_rate_bps_per_hour: None,
                    exit_queue_ahead_base: None,
                },
                leader: None,
                taker_sweep: None,
            },
        ];

        // Also test direct replay stopping right after Tick 2 to prove the unmarketable IOC
        // on Tick 2 does not phantom-fill before Tick 3's marketable touch appears.
        let report_unmarketable = run_continuous_pipeline_replay(&cfg, None, &ticks[..3]).unwrap();
        assert_eq!(report_unmarketable.passive_open_fills, 1);
        assert_eq!(report_unmarketable.timeout_ioc_fills, 0);
        assert!((report_unmarketable.terminal_open_inventory_base - 1.0).abs() <= 1e-12);

        let report_marketable = run_continuous_pipeline_replay(&cfg, None, &ticks).unwrap();
        assert_eq!(report_marketable.passive_open_fills, 1);
        assert_eq!(report_marketable.timeout_ioc_fills, 1);
        assert!(report_marketable.terminal_open_inventory_base.abs() <= 1e-12);
        // Touch fill at bid_px = 0.15 (`notional = 0.15`) rather than unmarketable `0.10`.
        // The flatten lands at now_ms = 11_000 < eval_end_ms = 15_000, so it is eval cash.
        assert!((report_marketable.eval_ledger.taker_exit_notional_usd - 0.15).abs() <= 1e-9);
    }

    #[test]
    fn replay_10h_continuous_requires_observed_span_and_bounded_eval_gaps() {
        let cfg = PipelineReplayConfig {
            pipeline: base_pipe_cfg(),
            slot_diff: SlotDiffConfig::default(),
            window: ReplayWindowSpec {
                warmup_start_ms: 1_000,
                eval_start_ms: 2_000,
                eval_end_ms: 36_002_000,
                drain_end_ms: 36_002_500,
            },
            initial_capital_usd: 10_000.0,
            daily_loss_limit_pct: 0.25,
            maker_fee_bps: 0.0,
            taker_fee_bps: 0.0,
            queue_fill_delay_ms: 0,
            passive_touch_fill_stride: 0,
            pre_registered_wallets: vec![],
        };
        let tick_at = |now_ms: u64| ReplayTick {
            market: GenericMarketState {
                now_ms,
                bid_px: 99.90,
                ask_px: 100.10,
                bid_sz: 5.0,
                ask_sz: 5.0,
                tick_size: 0.01,
                momentum_3s_bps: 0.0,
                oracle_px: None,
                funding_rate_bps_per_hour: None,
                exit_queue_ahead_base: None,
            },
            leader: None,
            taker_sweep: None,
        };

        // #694 counterexamples: warmup-only, 1s prefix, and endpoint-only pairs MUST return false.
        let warmup_only =
            run_continuous_pipeline_replay(&cfg, None, &[tick_at(1_000), tick_at(1_250)]).unwrap();
        assert!(!warmup_only.is_full_10h_continuous);
        assert_eq!(warmup_only.eval_ticks, 0);
        assert_eq!(warmup_only.observed_eval_span_ms, 0);

        let one_second =
            run_continuous_pipeline_replay(&cfg, None, &[tick_at(2_000), tick_at(3_000)]).unwrap();
        assert!(!one_second.is_full_10h_continuous);
        assert_eq!(one_second.observed_eval_span_ms, 1_000);

        let only_endpoints =
            run_continuous_pipeline_replay(&cfg, None, &[tick_at(2_000), tick_at(36_002_000)])
                .unwrap();
        assert!(!only_endpoints.is_full_10h_continuous);
        assert_eq!(only_endpoints.max_eval_gap_ms, 36_000_000);

        // Positive 10h continuous control: 60s stride across [2_000, 36_002_000] (601 eval ticks).
        let continuous_ticks: Vec<ReplayTick> =
            (0..=600_u64).map(|k| tick_at(2_000 + k * 60_000)).collect();
        let full = run_continuous_pipeline_replay(&cfg, None, &continuous_ticks).unwrap();
        assert!(full.is_full_10h_continuous);
        assert_eq!(full.observed_eval_span_ms, 36_000_000);
        assert_eq!(full.max_eval_gap_ms, 60_000);
        assert_eq!(full.eval_ticks, 601);
    }

    #[test]
    fn slot_tracker_retains_sub_minimum_partial_remainder_and_emits_cancel_before_replace() {
        let mut tracker = QuoteSlotTracker::new(SlotDiffConfig {
            lot_step: 0.001,
            min_order_qty: 0.01,
            ..SlotDiffConfig::default()
        })
        .unwrap();
        let plan = QuotePlan {
            mode: StrategyMode::NeutralMarketMaking,
            fair_mid: 100.0,
            lead_lag_shift_bps: 0.0,
            funding_shift_bps: 0.0,
            leader_stale: false,
            leader_vetoed_bid: false,
            leader_vetoed_ask: false,
            oracle_divergence_bps: None,
            oracle_reduce_only: false,
            bid_open: Some(OpeningQuote {
                side: QuoteSide::Bid,
                price: 99.90,
                qty: 0.02,
                spread_offset_bps: 10.0,
                faded_by_toxicity: false,
            }),
            ask_open: None,
            exit_directive: ExitDirective::Flat,
        };

        let acts1 = tracker.diff_plan(&plan, 0.01, OrderType::PostOnly).unwrap();
        assert_eq!(acts1.len(), 1);
        tracker
            .apply_batch_result(
                &acts1,
                &BatchResult {
                    tx_hashes: vec!["local-tx".into()],
                    place_results: vec![PlaceResult {
                        client_order_index: 100,
                        side: Side::Buy,
                        price: 99.90,
                        size: 0.02,
                    }],
                },
            )
            .unwrap();

        // #694 counterexample: 0.015 filled on 0.020 leaves 0.005 < min_order_qty (0.01).
        // Slot MUST retain client_order_index 100 and emit Cancel(100) before Place(0.02)!
        assert!(
            tracker
                .notify_fill(OrderSlotKind::OpeningBid, 100, 0.015)
                .unwrap()
        );
        let rem = tracker.resting(OrderSlotKind::OpeningBid).unwrap();
        assert_eq!(rem.client_order_index, 100);
        assert!((rem.qty - 0.005).abs() <= 1e-12);

        let acts2 = tracker.diff_plan(&plan, 0.01, OrderType::PostOnly).unwrap();
        assert_eq!(acts2.len(), 2);
        assert!(matches!(
            acts2[0],
            DriverOrderAction::Cancel {
                slot: OrderSlotKind::OpeningBid,
                client_order_index: 100,
            }
        ));
        assert!(matches!(
            acts2[1],
            DriverOrderAction::Place {
                slot: OrderSlotKind::OpeningBid,
                ..
            }
        ));

        // Wrong terminal ID leaves resting order intact; matching ID clears it.
        tracker.notify_order_terminal(OrderSlotKind::OpeningBid, 999);
        assert!(tracker.resting(OrderSlotKind::OpeningBid).is_some());
        tracker.notify_order_terminal(OrderSlotKind::OpeningBid, 100);
        assert!(tracker.resting(OrderSlotKind::OpeningBid).is_none());

        // Overfill beyond resting qty fails closed and marks tracker dirty.
        tracker
            .apply_batch_result(
                &acts1,
                &BatchResult {
                    tx_hashes: vec!["local-tx-2".into()],
                    place_results: vec![PlaceResult {
                        client_order_index: 101,
                        side: Side::Buy,
                        price: 99.90,
                        size: 0.02,
                    }],
                },
            )
            .unwrap();
        assert!(
            tracker
                .notify_fill(OrderSlotKind::OpeningBid, 101, 0.025)
                .is_err()
        );
        assert!(tracker.is_dirty());
    }

    #[test]
    fn slot_tracker_ignores_stale_client_order_fills_without_mutating_replacement() {
        let mut tracker = QuoteSlotTracker::new(SlotDiffConfig {
            lot_step: 0.001,
            min_order_qty: 0.01,
            ..SlotDiffConfig::default()
        })
        .unwrap();
        let make_plan = |bid_px: f64| QuotePlan {
            mode: StrategyMode::NeutralMarketMaking,
            fair_mid: 100.0,
            lead_lag_shift_bps: 0.0,
            funding_shift_bps: 0.0,
            leader_stale: false,
            leader_vetoed_bid: false,
            leader_vetoed_ask: false,
            oracle_divergence_bps: None,
            oracle_reduce_only: false,
            bid_open: Some(OpeningQuote {
                side: QuoteSide::Bid,
                price: bid_px,
                qty: 0.02,
                spread_offset_bps: 10.0,
                faded_by_toxicity: false,
            }),
            ask_open: Some(OpeningQuote {
                side: QuoteSide::Ask,
                price: 100.10,
                qty: 0.02,
                spread_offset_bps: 10.0,
                faded_by_toxicity: false,
            }),
            exit_directive: ExitDirective::Flat,
        };

        // Seed bid order 100 and ask order 102.
        let acts1 = tracker
            .diff_plan(&make_plan(99.90), 0.01, OrderType::PostOnly)
            .unwrap();
        assert_eq!(acts1.len(), 2);
        tracker
            .apply_batch_result(
                &acts1,
                &BatchResult {
                    tx_hashes: vec!["seed".into()],
                    place_results: vec![
                        PlaceResult {
                            client_order_index: 100,
                            side: Side::Buy,
                            price: 99.90,
                            size: 0.02,
                        },
                        PlaceResult {
                            client_order_index: 102,
                            side: Side::Sell,
                            price: 100.10,
                            size: 0.02,
                        },
                    ],
                },
            )
            .unwrap();

        // Replace bid with order 101 at a repriced level beyond deadband.
        let acts2 = tracker
            .diff_plan(&make_plan(99.80), 0.01, OrderType::PostOnly)
            .unwrap();
        assert_eq!(acts2.len(), 2);
        tracker
            .apply_batch_result(
                &acts2,
                &BatchResult {
                    tx_hashes: vec!["replace".into()],
                    place_results: vec![PlaceResult {
                        client_order_index: 101,
                        side: Side::Buy,
                        price: 99.80,
                        size: 0.02,
                    }],
                },
            )
            .unwrap();

        // #706: Late fills for replaced order 100 (partial or larger than slot qty) must return
        // Ok(false) and leave replacement 101, ask 102, and clean state untouched.
        assert!(
            !tracker
                .notify_fill(OrderSlotKind::OpeningBid, 100, 0.005)
                .unwrap()
        );
        assert!(
            !tracker
                .notify_fill(OrderSlotKind::OpeningBid, 100, 0.025)
                .unwrap()
        );
        assert!(!tracker.is_dirty());
        let bid = tracker.resting(OrderSlotKind::OpeningBid).unwrap();
        assert_eq!(bid.client_order_index, 101);
        assert!((bid.qty - 0.02).abs() <= 1e-12);
        let ask = tracker.resting(OrderSlotKind::OpeningAsk).unwrap();
        assert_eq!(ask.client_order_index, 102);
        assert!((ask.qty - 0.02).abs() <= 1e-12);

        // Empty slot valid fill returns Ok(false) and stays clean; malformed qty still errors.
        assert!(
            !tracker
                .notify_fill(OrderSlotKind::ReduceOnlyExit, 555, 0.01)
                .unwrap()
        );
        assert!(!tracker.is_dirty());
        assert!(
            tracker
                .notify_fill(OrderSlotKind::OpeningBid, 100, f64::NAN)
                .is_err()
        );
        assert!(
            tracker
                .notify_fill(OrderSlotKind::ReduceOnlyExit, 555, -0.001)
                .is_err()
        );

        // Matching identity updates partial, clears on exact full fill, and trips dirty on overfill.
        assert!(
            tracker
                .notify_fill(OrderSlotKind::OpeningBid, 101, 0.005)
                .unwrap()
        );
        let rem = tracker.resting(OrderSlotKind::OpeningBid).unwrap();
        assert_eq!(rem.client_order_index, 101);
        assert!((rem.qty - 0.015).abs() <= 1e-12);
        assert!(
            tracker
                .notify_fill(OrderSlotKind::OpeningBid, 101, 0.015)
                .unwrap()
        );
        assert!(tracker.resting(OrderSlotKind::OpeningBid).is_none());
        assert!(!tracker.is_dirty());
        assert!(
            tracker
                .notify_fill(OrderSlotKind::OpeningAsk, 102, 0.025)
                .is_err()
        );
        assert!(tracker.is_dirty());
    }

    #[test]
    fn replay_accrues_funding_cash_and_nets_into_eval_ledger() {
        let mut p_cfg = base_pipe_cfg();
        p_cfg.funding_bias.enabled = true;
        p_cfg.hybrid_exit.timeout_ms = 5_000_000;
        let cfg = PipelineReplayConfig {
            pipeline: p_cfg,
            slot_diff: SlotDiffConfig {
                reprice_deadband_ticks: 0,
                min_size_change_frac: 0.0,
                lot_step: 1.0,
                min_order_qty: 1.0,
            },
            window: ReplayWindowSpec {
                warmup_start_ms: 500,
                eval_start_ms: 1_000,
                eval_end_ms: 4_000_000,
                drain_end_ms: 4_100_000,
            },
            initial_capital_usd: 1_000.0,
            daily_loss_limit_pct: 0.25,
            maker_fee_bps: 0.0,
            taker_fee_bps: 0.0,
            queue_fill_delay_ms: 0,
            passive_touch_fill_stride: 1,
            pre_registered_wallets: vec![],
        };
        let mk_tick = |now_ms: u64, bid_px: f64, ask_px: f64| ReplayTick {
            market: GenericMarketState {
                now_ms,
                bid_px,
                ask_px,
                bid_sz: 5.0,
                ask_sz: 5.0,
                tick_size: 0.1,
                momentum_3s_bps: 0.0,
                oracle_px: None,
                funding_rate_bps_per_hour: Some(100.0), // 100 bp/h -> longs pay shorts
                exit_queue_ahead_base: None,
            },
            leader: None,
            taker_sweep: None,
        };
        // Tick 0 places bid; Tick 1 fills +1 long via ask <= order_px; Tick 2 holds +1 long 1h later.
        let ticks = vec![
            mk_tick(1_000, 99.9, 100.1),
            mk_tick(2_000, 95.0, 96.0),
            mk_tick(3_602_000, 99.9, 100.1),
        ];
        let report = run_continuous_pipeline_replay(&cfg, None, &ticks).unwrap();
        assert!(report.eval_ledger.funding_cash_usd < -0.5);
        assert!(report.eval_ledger.paired_cycle_net_cash_usd < 0.0);
    }

    #[test]
    fn apply_batch_result_fails_closed_on_ambiguous_open_and_reduce_only_same_shape() {
        let mut tracker = QuoteSlotTracker::new(SlotDiffConfig {
            reprice_deadband_ticks: 0,
            min_size_change_frac: 0.0,
            lot_step: 0.01,
            min_order_qty: 0.01,
        })
        .unwrap();

        let actions = vec![
            DriverOrderAction::Place {
                slot: OrderSlotKind::OpeningBid,
                params: OrderParams {
                    side: Side::Buy,
                    order_type: OrderType::PostOnly,
                    price: 100.0,
                    size: 1.0,
                    reduce_only: false,
                },
                faded_by_toxicity: false,
            },
            DriverOrderAction::Place {
                slot: OrderSlotKind::ReduceOnlyExit,
                params: OrderParams {
                    side: Side::Buy,
                    order_type: OrderType::PostOnly,
                    price: 100.0,
                    size: 1.0,
                    reduce_only: true,
                },
                faded_by_toxicity: false,
            },
        ];

        // Venue returns both identical-shape Buy placements in swapped order -> must fail closed!
        let err = tracker.apply_batch_result(
            &actions,
            &BatchResult {
                tx_hashes: vec!["swapped".into()],
                place_results: vec![
                    PlaceResult {
                        client_order_index: 202,
                        side: Side::Buy,
                        price: 100.0,
                        size: 1.0,
                    },
                    PlaceResult {
                        client_order_index: 101,
                        side: Side::Buy,
                        price: 100.0,
                        size: 1.0,
                    },
                ],
            },
        );
        assert!(err.is_err());
        assert!(tracker.is_dirty());
        assert!(tracker.resting(OrderSlotKind::OpeningBid).is_none());
        assert!(tracker.resting(OrderSlotKind::ReduceOnlyExit).is_none());
    }

    #[test]
    fn replay_feeds_causal_markout_shield_without_lookahead_and_tracks_queue_ahead() {
        let mut off_cfg = PipelineReplayConfig {
            pipeline: base_pipe_cfg(),
            slot_diff: SlotDiffConfig {
                reprice_deadband_ticks: 0,
                min_size_change_frac: 0.0,
                lot_step: 1.0,
                min_order_qty: 1.0,
            },
            window: ReplayWindowSpec {
                warmup_start_ms: 500,
                eval_start_ms: 1_000,
                eval_end_ms: 15_000,
                drain_end_ms: 18_000,
            },
            initial_capital_usd: 1_000.0,
            daily_loss_limit_pct: 0.25,
            maker_fee_bps: 0.0,
            taker_fee_bps: 0.0,
            queue_fill_delay_ms: 0,
            passive_touch_fill_stride: 1,
            pre_registered_wallets: vec![],
        };
        off_cfg.pipeline.hybrid_exit.timeout_ms = 20_000;

        let mk_tick = |now_ms: u64, bid_px: f64, ask_px: f64| ReplayTick {
            market: GenericMarketState {
                now_ms,
                bid_px,
                ask_px,
                bid_sz: 20.0,
                ask_sz: 20.0,
                tick_size: 0.1,
                momentum_3s_bps: 0.0,
                oracle_px: None,
                funding_rate_bps_per_hour: None,
                exit_queue_ahead_base: None,
            },
            leader: None,
            taker_sweep: None,
        };
        // Fill buy @ t=2000 (around 99.9) while mid drops sharply by t=3000 -> causal 1s markout < 0.
        let ticks = vec![
            mk_tick(1_000, 99.9, 100.1),
            mk_tick(2_000, 95.0, 95.2),
            mk_tick(3_000, 94.0, 94.2),
            mk_tick(4_000, 94.0, 94.2),
            mk_tick(5_000, 94.0, 94.2),
        ];

        let off_report = run_continuous_pipeline_replay(&off_cfg, None, &ticks).unwrap();

        let mut on_cfg = off_cfg.clone();
        on_cfg.pipeline.markout_shield.enabled = true;
        on_cfg.pipeline.markout_shield.ewma_alpha = 1.0;
        on_cfg.pipeline.markout_shield.widen_threshold_bps = 0.5;
        on_cfg.pipeline.markout_shield.max_widen_mult = 3.0;
        on_cfg.pipeline.markout_shield.engaged_size_scale = 0.5;
        on_cfg.pipeline.hybrid_exit.queue_exit.enabled = true;
        on_cfg.pipeline.hybrid_exit.queue_exit.max_expected_fill_sec = 5.0;

        let on_report = run_continuous_pipeline_replay(&on_cfg, None, &ticks).unwrap();
        assert_ne!(off_report.orders_placed + off_report.reprice_cancels, 0);
        // Causal adverse markout + queue exit change downstream quoting decisions once matured (>= +1s).
        assert!(
            on_report.orders_placed != off_report.orders_placed
                || on_report.eval_ledger.paired_cycle_net_cash_usd
                    != off_report.eval_ledger.paired_cycle_net_cash_usd
                || on_report.queue_preserved_ticks != off_report.queue_preserved_ticks
        );
    }
}
