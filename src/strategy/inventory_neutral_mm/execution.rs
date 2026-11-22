use super::ActiveOrder;
use super::components::{
    CancelReason, EntryRatchetPolicy, FLOAT_EPSILON, build_grid_plan, build_reducing_grid_plan,
    inventory_deadband_size, min_quotable_size, reconcile_side_plan_with_options,
    round_down_to_step,
};
use super::pricing::MakerPriceBounds;
use crate::config::InventoryNeutralMMConfig;
use crate::error::TradingError;
use crate::exchange::{BatchResult, OrderParams, OrderType, Side};
use crate::order_tracker::{OrderLifecycle, OrderSide};
use crate::telemetry::TelemetryCollector;
use std::time::Duration;
use tokio::time::Instant;

const CALM_SIDE_REQUOTE_REPLACEMENTS_PER_CYCLE: usize = 1;
pub(super) const URGENT_SIDE_REQUOTE_REPLACEMENTS_PER_CYCLE: usize = 2;
const CALM_SIZE_TOLERANCE_RATIO: f64 = 0.25;
const PRE_URGENCY_SIZE_TOLERANCE_RATIO: f64 = 0.18;
const ACTIVE_SIZE_TOLERANCE_RATIO: f64 = 0.12;
const POST_FILL_REPLENISH_COOLDOWN: Duration = Duration::from_millis(500);
const MICRO_REFRESH_COOLDOWN: Duration = Duration::from_secs(4);
const MICRO_REFRESH_MIN_RESTING_ORDERS: usize = 3;
const DEFER_INVENTORY_DEADBAND_MULTIPLIER: f64 = 1.5;

/// Shared inventory context passed to defer/sizing functions to avoid repeating 5 args.
#[derive(Debug, Clone, Copy)]
pub(super) struct InventoryContext<'a> {
    pub config: &'a InventoryNeutralMMConfig,
    pub position_for_quoting: f64,
    pub base_order_size: f64,
    pub inventory_urgency_threshold: f64,
    pub mid: f64,
}

#[derive(Debug, Clone)]
pub(super) struct SideExecutionPlan {
    pub to_cancel: Vec<i64>,
    #[allow(dead_code)]
    pub cancel_reasons: Vec<(i64, CancelReason)>,
    pub to_place: Vec<OrderParams>,
    pub had_adverse_retreat: bool,
    pub is_pure_grid_fill_in: bool,
}

impl SideExecutionPlan {
    pub(super) fn empty() -> Self {
        Self {
            to_cancel: Vec::new(),
            cancel_reasons: Vec::new(),
            to_place: Vec::new(),
            had_adverse_retreat: false,
            is_pure_grid_fill_in: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BatchFailureAction {
    Noop,
    EnterMarginCooldown,
}

pub(super) fn apply_batch_success(
    total_orders_placed: &mut u64,
    telemetry: &mut TelemetryCollector,
    result: &BatchResult,
) {
    for _ in &result.place_results {
        *total_orders_placed += 1;
        telemetry.record_order_placed();
    }
}

pub(super) fn classify_batch_failure(
    telemetry: &mut TelemetryCollector,
    err: &anyhow::Error,
    cooldown_secs: u64,
) -> BatchFailureAction {
    telemetry.record_order_rejected(&err.to_string());
    if matches!(
        err.downcast_ref::<TradingError>(),
        Some(TradingError::InsufficientMargin)
    ) {
        telemetry.record_margin_cooldown(cooldown_secs);
        BatchFailureAction::EnterMarginCooldown
    } else {
        BatchFailureAction::Noop
    }
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(super) fn build_side_execution_plan(
    config: &InventoryNeutralMMConfig,
    active_orders: &[ActiveOrder],
    order_type: OrderType,
    side: Side,
    target_px: f64,
    total_sz: f64,
    threshold: f64,
    size_tolerance_ratio: f64,
    max_replacements_per_cycle: usize,
) -> SideExecutionPlan {
    build_side_execution_plan_with_bounds(
        config,
        active_orders,
        order_type,
        side,
        target_px,
        total_sz,
        threshold,
        size_tolerance_ratio,
        max_replacements_per_cycle,
        None,
    )
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(super) fn build_side_execution_plan_with_bounds(
    config: &InventoryNeutralMMConfig,
    active_orders: &[ActiveOrder],
    order_type: OrderType,
    side: Side,
    target_px: f64,
    total_sz: f64,
    threshold: f64,
    size_tolerance_ratio: f64,
    max_replacements_per_cycle: usize,
    cancel_bounds: Option<MakerPriceBounds>,
) -> SideExecutionPlan {
    build_side_execution_plan_with_options(
        config,
        active_orders,
        order_type,
        side,
        target_px,
        total_sz,
        threshold,
        size_tolerance_ratio,
        max_replacements_per_cycle,
        cancel_bounds,
        None,
        false,
        None,
    )
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(super) fn build_side_execution_plan_with_options(
    config: &InventoryNeutralMMConfig,
    active_orders: &[ActiveOrder],
    order_type: OrderType,
    side: Side,
    target_px: f64,
    total_sz: f64,
    threshold: f64,
    size_tolerance_ratio: f64,
    max_replacements_per_cycle: usize,
    cancel_bounds: Option<MakerPriceBounds>,
    min_lifetime_override: Option<Duration>,
    fast_reducing: bool,
    entry_ratchet: Option<EntryRatchetPolicy>,
) -> SideExecutionPlan {
    build_side_execution_plan_full(
        config,
        active_orders,
        order_type,
        side,
        target_px,
        total_sz,
        threshold,
        size_tolerance_ratio,
        max_replacements_per_cycle,
        cancel_bounds,
        min_lifetime_override,
        fast_reducing,
        entry_ratchet,
        None,
        false,
        None,
    )
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(super) fn build_side_execution_plan_full(
    config: &InventoryNeutralMMConfig,
    active_orders: &[ActiveOrder],
    order_type: OrderType,
    side: Side,
    target_px: f64,
    total_sz: f64,
    threshold: f64,
    size_tolerance_ratio: f64,
    max_replacements_per_cycle: usize,
    cancel_bounds: Option<MakerPriceBounds>,
    min_lifetime_override: Option<Duration>,
    fast_reducing: bool,
    entry_ratchet: Option<EntryRatchetPolicy>,
    reducing_cap: Option<f64>,
    suppress_normal_cancels: bool,
    max_placements_cap: Option<usize>,
) -> SideExecutionPlan {
    build_side_execution_plan_with_withdrawal_reason(
        config,
        active_orders,
        order_type,
        side,
        target_px,
        total_sz,
        threshold,
        size_tolerance_ratio,
        max_replacements_per_cycle,
        cancel_bounds,
        min_lifetime_override,
        fast_reducing,
        entry_ratchet,
        reducing_cap,
        suppress_normal_cancels,
        max_placements_cap,
        CancelReason::SideWithdrawn,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn build_side_execution_plan_with_withdrawal_reason(
    config: &InventoryNeutralMMConfig,
    active_orders: &[ActiveOrder],
    order_type: OrderType,
    side: Side,
    target_px: f64,
    total_sz: f64,
    threshold: f64,
    size_tolerance_ratio: f64,
    max_replacements_per_cycle: usize,
    cancel_bounds: Option<MakerPriceBounds>,
    min_lifetime_override: Option<Duration>,
    fast_reducing: bool,
    entry_ratchet: Option<EntryRatchetPolicy>,
    reducing_cap: Option<f64>,
    suppress_normal_cancels: bool,
    max_placements_cap: Option<usize>,
    side_withdrawn_reason: CancelReason,
) -> SideExecutionPlan {
    let order_side = match side {
        Side::Buy => OrderSide::Buy,
        Side::Sell => OrderSide::Sell,
    };
    let side_orders: Vec<_> = active_orders
        .iter()
        .filter(|order| order.side == order_side)
        .cloned()
        .collect();

    let min_size = min_quotable_size(config, target_px);
    if total_sz + FLOAT_EPSILON < min_size
        || reducing_cap.is_some_and(|cap| cap + FLOAT_EPSILON < min_size)
    {
        // Zero budget / urgency is a risk withdrawal, not a queue refresh.
        // Cancel immediately even if the resting order was just submitted.
        let reason = if total_sz + FLOAT_EPSILON < min_size {
            side_withdrawn_reason
        } else {
            CancelReason::SideWithdrawn
        };
        let to_cancel: Vec<i64> = side_orders
            .iter()
            .filter(|order| {
                order.order_index.is_some() && order.lifecycle != OrderLifecycle::PendingCancel
            })
            .filter_map(|order| order.order_index)
            .collect();
        let cancel_reasons = to_cancel.iter().map(|oid| (*oid, reason)).collect();
        return SideExecutionPlan {
            to_cancel,
            cancel_reasons,
            to_place: Vec::new(),
            had_adverse_retreat: false,
            is_pure_grid_fill_in: false,
        };
    }

    let desired_quotes = if let Some(cap_abs) = reducing_cap {
        build_reducing_grid_plan(config, side, order_type, target_px, total_sz, cap_abs)
    } else {
        build_grid_plan(config, side, order_type, target_px, total_sz)
    };
    if desired_quotes.is_empty() {
        let to_cancel: Vec<i64> = side_orders
            .iter()
            .filter(|order| {
                (reducing_cap.is_some()
                    || cancel_bounds.is_some_and(|bounds| !bounds.allows(order.side, order.price)))
                    && order.order_index.is_some()
                    && order.lifecycle != OrderLifecycle::PendingCancel
            })
            .filter_map(|order| order.order_index)
            .collect();
        let reason = if reducing_cap.is_some() {
            CancelReason::ReducingCapOrNonRo
        } else {
            CancelReason::BoundBreach
        };
        let cancel_reasons = to_cancel.iter().map(|oid| (*oid, reason)).collect();
        return SideExecutionPlan {
            to_cancel,
            cancel_reasons,
            to_place: Vec::new(),
            had_adverse_retreat: false,
            is_pure_grid_fill_in: false,
        };
    }

    let min_lifetime =
        min_lifetime_override.unwrap_or_else(|| Duration::from_secs(config.order_ttl_secs.max(1)));
    let reconciled = reconcile_side_plan_with_options(
        &side_orders,
        &desired_quotes,
        threshold,
        config.step_size,
        size_tolerance_ratio,
        min_lifetime,
        max_replacements_per_cycle,
        cancel_bounds,
        fast_reducing,
        entry_ratchet,
        reducing_cap,
        suppress_normal_cancels,
    );
    let to_cancel = reconciled.to_cancel;
    let cancel_reasons = reconciled.cancel_reasons;
    let mut to_place = reconciled.to_place;
    let had_adverse_retreat = reconciled.had_adverse_retreat;

    if let Some(cap_abs) = reducing_cap {
        let exposed: f64 = side_orders
            .iter()
            .filter(|order| order.lifecycle.has_pending_exposure())
            .map(|order| order.size)
            .sum();
        let mut avail = round_down_to_step((cap_abs - exposed).max(0.0), config.step_size);
        let mut bounded_places = Vec::with_capacity(to_place.len());
        for mut order in to_place {
            let level_min = min_quotable_size(config, order.price);
            if !avail.is_finite() || avail + FLOAT_EPSILON < level_min {
                break;
            }
            let mut sz = round_down_to_step(order.size.min(avail), config.step_size);
            if sz + FLOAT_EPSILON < level_min
                || (avail - sz > FLOAT_EPSILON && avail - sz + FLOAT_EPSILON < level_min)
            {
                sz = avail;
            }
            if !sz.is_finite() || sz + FLOAT_EPSILON < level_min {
                break;
            }
            order.size = sz;
            order.reduce_only = true;
            bounded_places.push(order);
            avail = round_down_to_step((avail - sz).max(0.0), config.step_size);
            if avail + FLOAT_EPSILON < config.step_size {
                break;
            }
        }
        to_place = bounded_places;
    }

    if let Some(max_places) = max_placements_cap
        && to_place.len() > max_places
    {
        to_place.truncate(max_places);
    }

    let is_pure_grid_fill_in =
        to_cancel.is_empty() && !side_orders.is_empty() && !to_place.is_empty();

    SideExecutionPlan {
        to_cancel,
        cancel_reasons,
        to_place,
        had_adverse_retreat,
        is_pure_grid_fill_in,
    }
}

pub(super) fn size_tolerance_ratio_for_requote(ctx: &InventoryContext) -> f64 {
    let deadband = inventory_deadband_size(
        ctx.config,
        ctx.base_order_size,
        ctx.inventory_urgency_threshold.max(ctx.config.step_size),
        ctx.mid,
    );
    let urgency = ctx.inventory_urgency_threshold.max(ctx.config.step_size);
    if ctx.position_for_quoting.abs() <= deadband {
        CALM_SIZE_TOLERANCE_RATIO
    } else if ctx.position_for_quoting.abs() <= urgency {
        PRE_URGENCY_SIZE_TOLERANCE_RATIO
    } else {
        ACTIVE_SIZE_TOLERANCE_RATIO
    }
}

pub(super) fn max_side_requote_replacements_per_cycle(ctx: &InventoryContext) -> usize {
    let deadband = inventory_deadband_size(
        ctx.config,
        ctx.base_order_size,
        ctx.inventory_urgency_threshold.max(ctx.config.step_size),
        ctx.mid,
    );
    if ctx.position_for_quoting.abs() <= deadband {
        return CALM_SIDE_REQUOTE_REPLACEMENTS_PER_CYCLE;
    }

    let urgency = ctx.inventory_urgency_threshold.max(ctx.config.step_size);
    let bias_progress = ((ctx.position_for_quoting.abs() - deadband)
        / (urgency - deadband).max(ctx.config.step_size))
    .clamp(0.0, 1.0);
    if bias_progress < 0.5 {
        CALM_SIDE_REQUOTE_REPLACEMENTS_PER_CYCLE
    } else {
        URGENT_SIDE_REQUOTE_REPLACEMENTS_PER_CYCLE
    }
}

pub(super) fn resolve_cancel_client_order_ids(
    active_orders: &[ActiveOrder],
    to_cancel: &[i64],
) -> Vec<i64> {
    to_cancel
        .iter()
        .filter_map(|oid| {
            active_orders
                .iter()
                .find(|order| {
                    order.order_index == Some(*oid)
                        && order.lifecycle != OrderLifecycle::PendingCancel
                })
                .map(|order| order.client_order_id)
        })
        .collect()
}

pub(super) fn should_defer_one_sided_requote(
    ctx: &InventoryContext,
    bid_plan: &SideExecutionPlan,
    ask_plan: &SideExecutionPlan,
) -> bool {
    let symmetric_mode = ctx.position_for_quoting.abs()
        <= inventory_deadband_size(
            ctx.config,
            ctx.base_order_size,
            ctx.inventory_urgency_threshold.max(ctx.config.step_size),
            ctx.mid,
        );
    if !symmetric_mode {
        return false;
    }

    let bid_has_cancel = !bid_plan.to_cancel.is_empty();
    let ask_has_cancel = !ask_plan.to_cancel.is_empty();
    let bid_has_place = !bid_plan.to_place.is_empty();
    let ask_has_place = !ask_plan.to_place.is_empty();

    // Allow one-sided replenishment and same-side cancel+replace.
    // Only defer pure one-sided cancel churn when the other side is completely idle.
    let bid_cancel_only = bid_has_cancel && !bid_has_place;
    let ask_cancel_only = ask_has_cancel && !ask_has_place;
    (bid_cancel_only && !ask_has_cancel && !ask_has_place)
        || (ask_cancel_only && !bid_has_cancel && !bid_has_place)
}

pub(super) fn should_defer_cancel_only_refresh(
    ctx: &InventoryContext,
    bid_plan: &SideExecutionPlan,
    ask_plan: &SideExecutionPlan,
) -> bool {
    let symmetric_mode = ctx.position_for_quoting.abs()
        <= inventory_deadband_size(
            ctx.config,
            ctx.base_order_size,
            ctx.inventory_urgency_threshold.max(ctx.config.step_size),
            ctx.mid,
        );
    if !symmetric_mode {
        return false;
    }

    let has_any_place = !bid_plan.to_place.is_empty() || !ask_plan.to_place.is_empty();
    let has_any_cancel = !bid_plan.to_cancel.is_empty() || !ask_plan.to_cancel.is_empty();

    has_any_cancel && !has_any_place
}

pub(super) fn should_defer_post_fill_replenishment(
    ctx: &InventoryContext,
    bid_plan: &SideExecutionPlan,
    ask_plan: &SideExecutionPlan,
    last_fill_at: Option<Instant>,
    now: Instant,
) -> bool {
    let Some(last_fill_at) = last_fill_at else {
        return false;
    };
    if now.duration_since(last_fill_at) > POST_FILL_REPLENISH_COOLDOWN {
        return false;
    }

    let deadband = inventory_deadband_size(
        ctx.config,
        ctx.base_order_size,
        ctx.inventory_urgency_threshold.max(ctx.config.step_size),
        ctx.mid,
    );
    let defer_inventory_limit = (deadband * DEFER_INVENTORY_DEADBAND_MULTIPLIER)
        .max(ctx.config.step_size)
        .min(ctx.inventory_urgency_threshold.max(ctx.config.step_size));
    if ctx.position_for_quoting.abs() > defer_inventory_limit {
        return false;
    }

    let total_places = bid_plan.to_place.len() + ask_plan.to_place.len();
    let has_any_cancel = !bid_plan.to_cancel.is_empty() || !ask_plan.to_cancel.is_empty();

    total_places > 0 && total_places <= 2 && !has_any_cancel
}

pub(super) fn should_defer_micro_refresh(
    ctx: &InventoryContext,
    bid_plan: &SideExecutionPlan,
    ask_plan: &SideExecutionPlan,
    last_execution_at: Option<Instant>,
    now: Instant,
    resting_orders: usize,
) -> bool {
    let Some(last_execution_at) = last_execution_at else {
        return false;
    };
    if now.duration_since(last_execution_at) > MICRO_REFRESH_COOLDOWN {
        return false;
    }

    let deadband = inventory_deadband_size(
        ctx.config,
        ctx.base_order_size,
        ctx.inventory_urgency_threshold.max(ctx.config.step_size),
        ctx.mid,
    );
    let defer_inventory_limit = (deadband * DEFER_INVENTORY_DEADBAND_MULTIPLIER)
        .max(ctx.config.step_size)
        .min(ctx.inventory_urgency_threshold.max(ctx.config.step_size));
    if ctx.position_for_quoting.abs() > defer_inventory_limit {
        return false;
    }

    let total_places = bid_plan.to_place.len() + ask_plan.to_place.len();
    let total_cancels = bid_plan.to_cancel.len() + ask_plan.to_cancel.len();
    let one_sided_activity = (bid_plan.to_place.is_empty() && bid_plan.to_cancel.is_empty())
        ^ (ask_plan.to_place.is_empty() && ask_plan.to_cancel.is_empty());
    // Single-level grids (`grid_levels == 1`) can have at most 2 resting orders (1 bid + 1 ask),
    // so requiring `MICRO_REFRESH_MIN_RESTING_ORDERS` (3) made micro-refresh deferral unreachable
    // on single-level ladders (`S15-1`). Cap the threshold at `2 * grid_levels` AND require
    // `total_cancels > 0` for `grid_levels == 1` (so a pure missing-side placement can never be
    // deferred even if the opposite side transiently holds 2 orders), while `grid_levels >= 2`
    // stays identically `3`.
    let min_resting_orders = MICRO_REFRESH_MIN_RESTING_ORDERS
        .min(usize::from(ctx.config.grid_levels.max(1)).saturating_mul(2));
    let single_level_place_only = ctx.config.grid_levels <= 1 && total_cancels == 0;

    resting_orders >= min_resting_orders
        && !single_level_place_only
        && one_sided_activity
        && total_places <= 1
        && total_cancels <= 1
        && (total_places + total_cancels) > 0
}

/// Computes the effective resting-order count for `should_defer_micro_refresh`.
///
/// When `isolate_cooldown_cancel_churn` is active and the partner side is temporarily absent due to
/// an adverse-retreat cooldown or pure re-entry, keep micro-refresh deferral active for **routine
/// cancel/replace churn** on a non-empty surviving side (`resting_orders_raw > 0` and
/// `has_cancel_in_batch`). Pure placements (`!has_cancel_in_batch`) and empty books
/// (`resting_orders_raw == 0`) always use the real resting count so initial placements and pure
/// fill-ins are never artificially deferred.
pub(super) fn effective_resting_orders_for_micro_refresh(
    resting_orders_raw: usize,
    grid_levels: u8,
    isolate_cooldown_cancel_churn: bool,
    partner_side_absent_for_cooldown_or_reentry: bool,
    has_cancel_in_batch: bool,
) -> usize {
    if isolate_cooldown_cancel_churn
        && partner_side_absent_for_cooldown_or_reentry
        && has_cancel_in_batch
        && resting_orders_raw > 0
    {
        resting_orders_raw.saturating_add(usize::from(grid_levels))
    } else {
        resting_orders_raw
    }
}

/// Returns `true` only when every order scheduled to cancel in `to_cancel` is at or on the
/// passive/conservative side of the current top-of-grid `target_px` (`S13-1` directional safety
/// guard):
/// - `Side::Buy`: every canceled bid has `price <= target_px + FLOAT_EPSILON` (resting bid is at
///   or below our current fair target bid — never stale-aggressive above target).
/// - `Side::Sell`: every canceled ask has `price >= target_px - FLOAT_EPSILON` (resting ask is at
///   or above our current fair target ask — never stale-aggressive below target).
///
/// Returns `false` if `to_cancel` is empty, `target_px` is non-positive/non-finite, or any
/// canceled order cannot be resolved in `active_orders`.
pub(super) fn cancels_are_strictly_passive(
    active_orders: &[ActiveOrder],
    side: Side,
    to_cancel: &[i64],
    target_px: f64,
) -> bool {
    if to_cancel.is_empty() || !target_px.is_finite() || target_px <= 0.0 {
        return false;
    }
    let expected_order_side = match side {
        Side::Buy => OrderSide::Buy,
        Side::Sell => OrderSide::Sell,
    };
    to_cancel.iter().all(|cancel_id| {
        active_orders
            .iter()
            .find(|o| {
                o.order_index == Some(*cancel_id)
                    && o.side == expected_order_side
                    && o.lifecycle != OrderLifecycle::PendingCancel
            })
            .is_some_and(|order| match side {
                Side::Buy => order.price <= target_px + FLOAT_EPSILON,
                Side::Sell => order.price >= target_px - FLOAT_EPSILON,
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> InventoryNeutralMMConfig {
        InventoryNeutralMMConfig {
            tick_size: 0.01,
            step_size: 0.0001,
            base_order_size: 0.015,
            base_order_notional_usd: 32.0,
            max_position_notional_usd: 425.0,
            inventory_urgency_notional_usd: 170.0,
            min_inventory_notional_usd: 10.0,
            grid_levels: 10,
            grid_spacing_bps: 5.0,
            grid_size_decay: 0.8,
            order_ttl_secs: 10,
            ..Default::default()
        }
    }

    fn active_order(
        client_order_id: i64,
        order_index: i64,
        side: OrderSide,
        price: f64,
        size: f64,
        age_secs: u64,
    ) -> ActiveOrder {
        ActiveOrder {
            client_order_id,
            order_index: Some(order_index),
            lifecycle: OrderLifecycle::Open,
            side,
            price,
            size,
            reduce_only: false,
            placed_at: Instant::now() - Duration::from_secs(age_secs),
        }
    }

    #[test]
    fn planner_returns_places_for_missing_grid_levels() {
        let config = config();
        let plan = build_side_execution_plan(
            &config,
            &[],
            OrderType::PostOnly,
            Side::Buy,
            2100.0,
            config.base_order_size,
            0.05,
            ACTIVE_SIZE_TOLERANCE_RATIO,
            2,
        );

        assert_eq!(plan.to_cancel.len(), 0);
        assert!(plan.to_place.len() >= 4);
    }

    #[test]
    fn planner_cancels_only_stale_mismatched_side_orders() {
        let config = config();
        let active_orders = vec![
            active_order(1, 101, OrderSide::Buy, 2100.0, 0.015, 30),
            active_order(2, 102, OrderSide::Buy, 2098.0, 0.015, 30),
            active_order(3, 201, OrderSide::Sell, 2110.0, 0.015, 30),
        ];

        let plan = build_side_execution_plan(
            &config,
            &active_orders,
            OrderType::PostOnly,
            Side::Buy,
            2100.0,
            config.base_order_size,
            0.05,
            ACTIVE_SIZE_TOLERANCE_RATIO,
            2,
        );

        assert_eq!(plan.to_cancel, vec![102]);
        assert!(!plan.to_place.is_empty());
        assert!(plan.to_place.iter().all(|order| order.side == Side::Buy));
    }

    #[test]
    fn planner_caps_side_requote_replacements_per_cycle() {
        let config = config();
        let active_orders = vec![
            active_order(1, 101, OrderSide::Buy, 2090.0, 0.015, 30),
            active_order(2, 102, OrderSide::Buy, 2089.0, 0.012, 30),
            active_order(3, 103, OrderSide::Buy, 2088.0, 0.010, 30),
            active_order(4, 104, OrderSide::Buy, 2087.0, 0.008, 30),
            active_order(5, 105, OrderSide::Buy, 2086.0, 0.006, 30),
        ];

        let plan = build_side_execution_plan(
            &config,
            &active_orders,
            OrderType::PostOnly,
            Side::Buy,
            2100.0,
            0.015,
            0.05,
            ACTIVE_SIZE_TOLERANCE_RATIO,
            1,
        );

        assert_eq!(plan.to_cancel.len(), 1);
        assert_eq!(plan.to_place.len(), 1);
    }

    #[test]
    fn planner_cancels_existing_side_orders_when_target_side_is_disabled() {
        let config = config();
        let active_orders = vec![
            active_order(1, 101, OrderSide::Buy, 2100.0, 0.015, 30),
            active_order(2, 102, OrderSide::Buy, 2099.0, 0.015, 30),
            active_order(3, 201, OrderSide::Sell, 2110.0, 0.015, 30),
        ];

        let plan = build_side_execution_plan(
            &config,
            &active_orders,
            OrderType::PostOnly,
            Side::Buy,
            2100.0,
            0.0,
            0.05,
            ACTIVE_SIZE_TOLERANCE_RATIO,
            2,
        );

        assert_eq!(plan.to_cancel, vec![101, 102]);
        assert!(plan.to_place.is_empty());
    }

    #[test]
    fn planner_places_scaled_quotes_above_min_quotable_size() {
        let config = config();

        let plan = build_side_execution_plan(
            &config,
            &[],
            OrderType::PostOnly,
            Side::Buy,
            2100.0,
            0.0102,
            0.05,
            ACTIVE_SIZE_TOLERANCE_RATIO,
            2,
        );

        assert!(plan.to_cancel.is_empty());
        assert!(!plan.to_place.is_empty());
        assert!(plan.to_place.iter().all(|order| order.side == Side::Buy));
    }

    #[test]
    fn apply_batch_success_records_each_placed_order() {
        let mut total_orders_placed = 0;
        let mut telemetry = TelemetryCollector::new();
        let result = BatchResult {
            tx_hashes: vec!["0x1".to_string(), "0x2".to_string()],
            place_results: vec![
                crate::exchange::PlaceResult {
                    client_order_index: 1,
                    side: Side::Buy,
                    price: 2100.0,
                    size: 0.01,
                },
                crate::exchange::PlaceResult {
                    client_order_index: 2,
                    side: Side::Sell,
                    price: 2100.1,
                    size: 0.01,
                },
            ],
        };

        apply_batch_success(&mut total_orders_placed, &mut telemetry, &result);

        assert_eq!(total_orders_placed, 2);
        assert_eq!(telemetry.orders_placed, 2);
    }

    fn inv_ctx(
        config: &InventoryNeutralMMConfig,
        position_for_quoting: f64,
        mid: f64,
    ) -> InventoryContext<'_> {
        InventoryContext {
            config,
            position_for_quoting,
            base_order_size: config.base_order_size,
            inventory_urgency_threshold: config.inventory_urgency_threshold,
            mid,
        }
    }

    #[test]
    fn calm_inventory_uses_wider_size_tolerance() {
        let config = config();
        let calm_ratio =
            size_tolerance_ratio_for_requote(&inv_ctx(&config, config.step_size, 2100.0));
        let active_ratio = size_tolerance_ratio_for_requote(&inv_ctx(
            &config,
            config.inventory_urgency_threshold * 2.0,
            2100.0,
        ));
        let pre_urgency_ratio = size_tolerance_ratio_for_requote(&inv_ctx(
            &config,
            config.inventory_urgency_threshold * 0.75,
            2100.0,
        ));

        assert!(calm_ratio > pre_urgency_ratio);
        assert!(pre_urgency_ratio > active_ratio);
        assert!((calm_ratio - CALM_SIZE_TOLERANCE_RATIO).abs() < 1e-9);
        assert!((pre_urgency_ratio - PRE_URGENCY_SIZE_TOLERANCE_RATIO).abs() < 1e-9);
        assert!((active_ratio - ACTIVE_SIZE_TOLERANCE_RATIO).abs() < 1e-9);
    }

    #[test]
    fn resolve_cancel_client_order_ids_matches_order_index_only() {
        let active_orders = vec![
            active_order(11, 101, OrderSide::Buy, 2100.0, 0.015, 30),
            active_order(22, 202, OrderSide::Sell, 2101.0, 0.015, 30),
        ];

        let resolved = resolve_cancel_client_order_ids(&active_orders, &[101, 22, 999]);

        assert_eq!(resolved, vec![11]);
    }

    #[test]
    fn resolve_cancel_client_order_ids_skips_pending_cancel_orders() {
        let mut active = active_order(11, 101, OrderSide::Buy, 2100.0, 0.015, 30);
        active.lifecycle = OrderLifecycle::PendingCancel;

        let resolved = resolve_cancel_client_order_ids(&[active], &[101]);

        assert!(resolved.is_empty());
    }

    #[test]
    fn classify_batch_failure_detects_margin_rejection() {
        let mut telemetry = TelemetryCollector::new();
        let err = anyhow::Error::new(TradingError::InsufficientMargin);

        let action = classify_batch_failure(&mut telemetry, &err, 7);

        assert_eq!(action, BatchFailureAction::EnterMarginCooldown);
        assert_eq!(telemetry.orders_rejected, 1);
        assert_eq!(telemetry.margin_cooldown_events, 1);
        assert!(telemetry.is_in_margin_cooldown(7));
    }

    #[test]
    fn classify_batch_failure_leaves_non_margin_errors_as_noop() {
        let mut telemetry = TelemetryCollector::new();
        let err = anyhow::anyhow!("temporary exchange error");

        let action = classify_batch_failure(&mut telemetry, &err, 7);

        assert_eq!(action, BatchFailureAction::Noop);
        assert_eq!(telemetry.orders_rejected, 1);
        assert_eq!(telemetry.margin_cooldown_events, 0);
    }

    fn side_plan(to_cancel: Vec<i64>, to_place: Vec<OrderParams>) -> SideExecutionPlan {
        SideExecutionPlan {
            to_cancel,
            cancel_reasons: Vec::new(),
            to_place,
            had_adverse_retreat: false,
            is_pure_grid_fill_in: false,
        }
    }

    #[test]
    fn defer_one_sided_requote_in_symmetric_mode_when_only_one_side_cancels() {
        let config = config();
        let bid_plan = side_plan(vec![101], Vec::new());
        let ask_plan = side_plan(Vec::new(), Vec::new());

        assert!(should_defer_one_sided_requote(
            &inv_ctx(&config, 0.005, 2100.0),
            &bid_plan,
            &ask_plan
        ));
    }

    #[test]
    fn defer_one_sided_requote_for_small_residual_inventory_up_to_top_level_size() {
        let config = config();
        let bid_plan = side_plan(Vec::new(), Vec::new());
        let ask_plan = side_plan(vec![201], Vec::new());

        assert!(should_defer_one_sided_requote(
            &inv_ctx(&config, 0.005, 2100.0),
            &bid_plan,
            &ask_plan
        ));
    }

    #[test]
    fn do_not_defer_one_sided_replenishment_in_symmetric_mode() {
        let config = config();
        let bid_plan = side_plan(
            Vec::new(),
            vec![OrderParams {
                size: 0.015,
                price: 2100.0,
                side: Side::Buy,
                order_type: OrderType::PostOnly,
                reduce_only: false,
            }],
        );
        let ask_plan = side_plan(Vec::new(), Vec::new());

        assert!(!should_defer_one_sided_requote(
            &inv_ctx(&config, 0.005, 2100.0),
            &bid_plan,
            &ask_plan
        ));
    }

    #[test]
    fn defer_cancel_only_refresh_in_symmetric_mode() {
        let config = config();
        let bid_plan = side_plan(vec![101], Vec::new());
        let ask_plan = side_plan(vec![201], Vec::new());

        assert!(should_defer_cancel_only_refresh(
            &inv_ctx(&config, 0.0, 2100.0),
            &bid_plan,
            &ask_plan
        ));
    }

    #[test]
    fn do_not_defer_cancel_only_refresh_when_replenishment_needed() {
        let config = config();
        let bid_plan = side_plan(
            vec![101],
            vec![OrderParams {
                size: 0.015,
                price: 2100.0,
                side: Side::Buy,
                order_type: OrderType::PostOnly,
                reduce_only: false,
            }],
        );
        let ask_plan = side_plan(vec![201], Vec::new());

        assert!(!should_defer_cancel_only_refresh(
            &inv_ctx(&config, 0.0, 2100.0),
            &bid_plan,
            &ask_plan
        ));
    }

    #[test]
    fn defer_small_post_fill_replenishment_in_pre_urgency_inventory() {
        let config = config();
        let bid_plan = side_plan(
            Vec::new(),
            vec![OrderParams {
                size: 0.0082,
                price: 2100.0,
                side: Side::Buy,
                order_type: OrderType::PostOnly,
                reduce_only: false,
            }],
        );
        let ask_plan = side_plan(Vec::new(), Vec::new());

        let now = Instant::now();
        assert!(should_defer_post_fill_replenishment(
            &inv_ctx(&config, 0.005, 2100.0),
            &bid_plan,
            &ask_plan,
            Some(now - Duration::from_millis(500)),
            now,
        ));
    }

    #[test]
    fn do_not_defer_post_fill_replenishment_when_inventory_is_urgent() {
        let config = config();
        let bid_plan = side_plan(
            Vec::new(),
            vec![OrderParams {
                size: 0.0082,
                price: 2100.0,
                side: Side::Buy,
                order_type: OrderType::PostOnly,
                reduce_only: false,
            }],
        );
        let ask_plan = side_plan(Vec::new(), Vec::new());

        assert!(!should_defer_post_fill_replenishment(
            &inv_ctx(&config, 0.12, 2100.0),
            &bid_plan,
            &ask_plan,
            Some(Instant::now() - Duration::from_millis(500)),
            Instant::now(),
        ));
    }

    #[test]
    fn do_not_defer_post_fill_replenishment_for_medium_inventory_above_deadband() {
        let config = config();
        let bid_plan = side_plan(
            Vec::new(),
            vec![OrderParams {
                size: 0.0132,
                price: 2234.72,
                side: Side::Buy,
                order_type: OrderType::PostOnly,
                reduce_only: false,
            }],
        );
        let ask_plan = side_plan(Vec::new(), Vec::new());

        assert!(!should_defer_post_fill_replenishment(
            &inv_ctx(&config, 0.02, 2232.0),
            &bid_plan,
            &ask_plan,
            Some(Instant::now() - Duration::from_millis(500)),
            Instant::now(),
        ));
    }

    #[test]
    fn defer_micro_refresh_when_recent_small_one_sided_requote() {
        let config = config();
        let bid_plan = side_plan(
            vec![101],
            vec![OrderParams {
                size: 0.0132,
                price: 2234.72,
                side: Side::Buy,
                order_type: OrderType::PostOnly,
                reduce_only: false,
            }],
        );
        let ask_plan = side_plan(Vec::new(), Vec::new());

        assert!(should_defer_micro_refresh(
            &inv_ctx(&config, -0.005, 2232.0),
            &bid_plan,
            &ask_plan,
            Some(Instant::now() - Duration::from_secs(3)),
            Instant::now(),
            4,
        ));
    }

    #[test]
    fn do_not_defer_micro_refresh_when_recent_window_has_expired() {
        let config = config();
        let bid_plan = side_plan(
            vec![101],
            vec![OrderParams {
                size: 0.0132,
                price: 2234.72,
                side: Side::Buy,
                order_type: OrderType::PostOnly,
                reduce_only: false,
            }],
        );
        let ask_plan = side_plan(Vec::new(), Vec::new());

        assert!(!should_defer_micro_refresh(
            &inv_ctx(&config, -0.02, 2232.0),
            &bid_plan,
            &ask_plan,
            Some(Instant::now() - Duration::from_secs(13)),
            Instant::now(),
            4,
        ));
    }

    #[test]
    fn do_not_defer_micro_refresh_for_multi_side_rebuild() {
        let config = config();
        let bid_plan = side_plan(
            vec![101],
            vec![OrderParams {
                size: 0.0132,
                price: 2234.72,
                side: Side::Buy,
                order_type: OrderType::PostOnly,
                reduce_only: false,
            }],
        );
        let ask_plan = side_plan(
            Vec::new(),
            vec![OrderParams {
                size: 0.0059,
                price: 2237.12,
                side: Side::Sell,
                order_type: OrderType::PostOnly,
                reduce_only: false,
            }],
        );

        assert!(!should_defer_micro_refresh(
            &inv_ctx(&config, -0.02, 2232.0),
            &bid_plan,
            &ask_plan,
            Some(Instant::now() - Duration::from_secs(3)),
            Instant::now(),
            4,
        ));
    }

    #[test]
    fn do_not_defer_micro_refresh_for_medium_inventory_above_deadband() {
        let config = config();
        let bid_plan = side_plan(
            Vec::new(),
            vec![OrderParams {
                size: 0.0132,
                price: 2234.72,
                side: Side::Buy,
                order_type: OrderType::PostOnly,
                reduce_only: false,
            }],
        );
        let ask_plan = side_plan(Vec::new(), Vec::new());

        assert!(!should_defer_micro_refresh(
            &inv_ctx(&config, 0.02, 2232.0),
            &bid_plan,
            &ask_plan,
            Some(Instant::now() - Duration::from_secs(2)),
            Instant::now(),
            3,
        ));
    }

    #[test]
    fn defer_micro_refresh_even_with_three_resting_orders_in_calm_market() {
        let config = config();
        let bid_plan = side_plan(vec![101], Vec::new());
        let ask_plan = side_plan(Vec::new(), Vec::new());

        assert!(should_defer_micro_refresh(
            &inv_ctx(&config, 0.0, 2232.0),
            &bid_plan,
            &ask_plan,
            Some(Instant::now() - Duration::from_secs(2)),
            Instant::now(),
            3,
        ));
    }

    #[test]
    fn defer_micro_refresh_activates_for_full_two_sided_single_level_grid_only() {
        let mut single = config();
        single.grid_levels = 1;
        let bid_plan = side_plan(
            vec![101],
            vec![OrderParams {
                size: 0.0132,
                price: 2234.72,
                side: Side::Buy,
                order_type: OrderType::PostOnly,
                reduce_only: false,
            }],
        );
        let ask_plan = side_plan(Vec::new(), Vec::new());
        let now = Instant::now();
        let recent = Some(now - Duration::from_secs(2));

        // Full two-sided single-level book (1 bid + 1 ask = 2 resting) defers 1-sided micro-refresh.
        assert!(should_defer_micro_refresh(
            &inv_ctx(&single, 0.0, 2232.0),
            &bid_plan,
            &ask_plan,
            recent,
            now,
            2,
        ));
        // Missing side (1 resting) or empty book (0 resting) must never defer replenishment.
        assert!(!should_defer_micro_refresh(
            &inv_ctx(&single, 0.0, 2232.0),
            &bid_plan,
            &ask_plan,
            recent,
            now,
            1,
        ));
        // Empty book (0 resting) or pure missing-side placement (even if resting == 2 transiently)
        // must never defer on a single-level grid.
        assert!(!should_defer_micro_refresh(
            &inv_ctx(&single, 0.0, 2232.0),
            &bid_plan,
            &ask_plan,
            recent,
            now,
            0,
        ));
        let pure_place_ask = side_plan(
            Vec::new(),
            vec![OrderParams {
                size: 0.0132,
                price: 2237.12,
                side: Side::Sell,
                order_type: OrderType::PostOnly,
                reduce_only: false,
            }],
        );
        assert!(!should_defer_micro_refresh(
            &inv_ctx(&single, 0.0, 2232.0),
            &SideExecutionPlan::empty(),
            &pure_place_ask,
            recent,
            now,
            2,
        ));

        // Multi-level grids (`grid_levels >= 2`) keep the exact historical threshold of 3.
        let mut two_level = config();
        two_level.grid_levels = 2;
        assert!(!should_defer_micro_refresh(
            &inv_ctx(&two_level, 0.0, 2232.0),
            &bid_plan,
            &ask_plan,
            recent,
            now,
            2,
        ));
        assert!(should_defer_micro_refresh(
            &inv_ctx(&two_level, 0.0, 2232.0),
            &bid_plan,
            &ask_plan,
            recent,
            now,
            3,
        ));
    }

    #[test]
    fn calm_inventory_uses_one_side_replacement_per_cycle() {
        let config = config();
        assert_eq!(
            max_side_requote_replacements_per_cycle(&inv_ctx(&config, 0.0, 2100.0)),
            1
        );
    }

    #[test]
    fn urgent_inventory_uses_two_side_replacements_per_cycle() {
        let config = config();
        assert_eq!(
            max_side_requote_replacements_per_cycle(&inv_ctx(&config, 0.08, 2100.0)),
            2
        );
    }

    #[test]
    fn reducing_plan_caps_multi_level_grid_deducts_pending_cancel_and_pulls_oversized_or_non_ro() {
        let mut config = config();
        config.step_size = 0.01;
        config.grid_levels = 3;
        config.grid_size_decay = 0.7;
        // 1. Fresh plan for +0.05 long (at price=2000, min_quotable_size = 0.01) caps cumulative grid <= 0.05, reduce_only = true.
        let fresh = build_side_execution_plan_full(
            &config,
            &[],
            OrderType::PostOnly,
            Side::Sell,
            2000.0,
            0.03,
            0.05,
            ACTIVE_SIZE_TOLERANCE_RATIO,
            2,
            None,
            Some(Duration::from_secs(1)),
            true,
            None,
            Some(0.05),
            false,
            None,
        );
        assert!(fresh.to_cancel.is_empty());
        assert_eq!(fresh.to_place.len(), 2);
        assert!(fresh.to_place.iter().all(|o| o.reduce_only));
        let sum: f64 = fresh.to_place.iter().map(|o| o.size).sum();
        assert!((sum - 0.05).abs() < 1e-9);

        // 2. Dust tail below min_quotable_size (0.035 with min=0.01, top=0.03) is absorbed into single 0.035 lot.
        let dust_cfg = InventoryNeutralMMConfig {
            step_size: 0.005,
            ..config.clone()
        };
        let dust_absorbed = build_side_execution_plan_full(
            &dust_cfg,
            &[],
            OrderType::PostOnly,
            Side::Sell,
            2000.0,
            0.03,
            0.05,
            ACTIVE_SIZE_TOLERANCE_RATIO,
            2,
            None,
            Some(Duration::from_secs(1)),
            true,
            None,
            Some(0.035),
            false,
            None,
        );
        assert_eq!(dust_absorbed.to_place.len(), 1);
        assert!((dust_absorbed.to_place[0].size - 0.035).abs() < 1e-9);

        // 3. PendingCancel exposure (0.05) blocks placing duplicate 0.05; partial PendingCancel (0.02) allows 0.03 only.
        let pending_full = ActiveOrder {
            client_order_id: 1,
            order_index: Some(11),
            lifecycle: OrderLifecycle::PendingCancel,
            side: OrderSide::Sell,
            price: 2001.0,
            size: 0.05,
            reduce_only: true,
            placed_at: Instant::now(),
        };
        let blocked = build_side_execution_plan_full(
            &config,
            &[pending_full],
            OrderType::PostOnly,
            Side::Sell,
            2000.0,
            0.05,
            0.05,
            ACTIVE_SIZE_TOLERANCE_RATIO,
            2,
            None,
            Some(Duration::from_secs(1)),
            true,
            None,
            Some(0.05),
            false,
            None,
        );
        assert!(blocked.to_cancel.is_empty());
        assert!(blocked.to_place.is_empty());

        let pending_partial = ActiveOrder {
            size: 0.02,
            ..active_order(2, 12, OrderSide::Sell, 2002.0, 0.02, 0)
        };
        let mut pending_partial_pc = pending_partial;
        pending_partial_pc.lifecycle = OrderLifecycle::PendingCancel;
        pending_partial_pc.reduce_only = true;
        let partial = build_side_execution_plan_full(
            &config,
            &[pending_partial_pc],
            OrderType::PostOnly,
            Side::Sell,
            2000.0,
            0.03,
            0.05,
            ACTIVE_SIZE_TOLERANCE_RATIO,
            2,
            None,
            Some(Duration::from_secs(1)),
            true,
            None,
            Some(0.05),
            false,
            None,
        );
        assert_eq!(partial.to_place.len(), 1);
        assert!((partial.to_place[0].size - 0.03).abs() < 1e-9);
        assert!(partial.to_place[0].reduce_only);

        // 4. Non-reduce-only or oversized open order is force-canceled at age = 0, and double exposure waits for cancel ACK.
        let legacy_non_ro = ActiveOrder {
            reduce_only: false,
            ..active_order(3, 13, OrderSide::Sell, 2000.0, 0.05, 0)
        };
        let pull_legacy = build_side_execution_plan_full(
            &config,
            &[legacy_non_ro],
            OrderType::PostOnly,
            Side::Sell,
            2000.0,
            0.05,
            0.05,
            ACTIVE_SIZE_TOLERANCE_RATIO,
            2,
            None,
            Some(Duration::from_secs(1)),
            true,
            None,
            Some(0.05),
            false,
            None,
        );
        assert_eq!(pull_legacy.to_cancel, vec![13]);
        assert!(pull_legacy.to_place.is_empty());
    }

    #[test]
    fn cooldown_hold_safe_levels_and_placement_cap_limit_cancel_churn() {
        let mut config = config();
        config.grid_levels = 3;
        config.step_size = 0.001;
        let existing = vec![
            active_order(1, 101, OrderSide::Buy, 2099.0, 0.015, 30),
            active_order(2, 102, OrderSide::Buy, 2098.0, 0.012, 30),
        ];
        let normal = build_side_execution_plan_full(
            &config,
            &existing,
            OrderType::PostOnly,
            Side::Buy,
            2100.0,
            0.045,
            0.05,
            ACTIVE_SIZE_TOLERANCE_RATIO,
            1,
            None,
            None,
            false,
            None,
            None,
            false,
            Some(1),
        );
        assert_eq!(normal.to_cancel, vec![101]);

        // Normal cancels suppressed while holding inside bounds, capping placements to 1 when active.
        let held = build_side_execution_plan_full(
            &config,
            &existing,
            OrderType::PostOnly,
            Side::Buy,
            2100.0,
            0.045,
            0.05,
            ACTIVE_SIZE_TOLERANCE_RATIO,
            1,
            None,
            None,
            false,
            None,
            None,
            true,
            Some(1),
        );
        assert!(held.to_cancel.is_empty());
        assert!(held.to_place.len() <= 1);

        let unconstrained = build_side_execution_plan_full(
            &config,
            &[],
            OrderType::PostOnly,
            Side::Buy,
            2100.0,
            0.045,
            0.05,
            ACTIVE_SIZE_TOLERANCE_RATIO,
            1,
            None,
            None,
            false,
            None,
            None,
            false,
            None,
        );
        assert_eq!(unconstrained.to_place.len(), 3);

        let ramp = build_side_execution_plan_full(
            &config,
            &[],
            OrderType::PostOnly,
            Side::Buy,
            2100.0,
            0.045,
            0.05,
            ACTIVE_SIZE_TOLERANCE_RATIO,
            1,
            None,
            None,
            false,
            None,
            None,
            false,
            Some(1),
        );
        assert_eq!(ramp.to_place.len(), 1);
    }

    #[test]
    fn cooldown_micro_refresh_does_not_defer_first_placement_on_empty_book() {
        let mut cfg = config();
        cfg.grid_levels = 3;
        let ctx = InventoryContext {
            config: &cfg,
            position_for_quoting: 0.0,
            base_order_size: 0.015,
            inventory_urgency_threshold: 0.1,
            mid: 2000.0,
        };
        let idle = SideExecutionPlan::empty();
        let first_place = SideExecutionPlan {
            to_place: vec![OrderParams {
                side: Side::Sell,
                price: 2001.0,
                size: 0.015,
                order_type: OrderType::PostOnly,
                reduce_only: false,
            }],
            ..SideExecutionPlan::empty()
        };
        let single_replace = SideExecutionPlan {
            to_cancel: vec![11],
            to_place: first_place.to_place.clone(),
            ..SideExecutionPlan::empty()
        };
        let now = Instant::now();
        let one_sec_ago = Some(now - Duration::from_secs(1));
        let five_secs_ago = Some(now - Duration::from_secs(5));

        // 1) Actual resting == 0, partner side in cooldown, grid_levels == 3:
        //    Pure first placement on the opposite side MUST NOT be artificially deferred!
        let eff_first =
            effective_resting_orders_for_micro_refresh(0, cfg.grid_levels, true, true, false);
        assert_eq!(eff_first, 0);
        assert!(!should_defer_micro_refresh(
            &ctx,
            &idle,
            &first_place,
            one_sec_ago,
            now,
            eff_first,
        ));

        // 2) Actual resting == 2 on surviving side, partner side in cooldown:
        //    Routine cancel+replace IS deferred at 1s, and allowed at 5s.
        let eff_replace_cd =
            effective_resting_orders_for_micro_refresh(2, cfg.grid_levels, true, true, true);
        assert_eq!(eff_replace_cd, 5);
        assert!(should_defer_micro_refresh(
            &ctx,
            &idle,
            &single_replace,
            one_sec_ago,
            now,
            eff_replace_cd,
        ));
        assert!(!should_defer_micro_refresh(
            &ctx,
            &idle,
            &single_replace,
            five_secs_ago,
            now,
            eff_replace_cd,
        ));

        // 3) Actual resting == 2 on surviving side, pure 1-level fill-in (no cancel):
        //    Pure placement is NOT artificially bumped to >= 3.
        let eff_fill_in =
            effective_resting_orders_for_micro_refresh(2, cfg.grid_levels, true, true, false);
        assert_eq!(eff_fill_in, 2);
        assert!(!should_defer_micro_refresh(
            &ctx,
            &idle,
            &first_place,
            one_sec_ago,
            now,
            eff_fill_in,
        ));

        // 4) Real resting == 3: normal replacement still defers at 1s regardless of isolation.
        let eff_real_three =
            effective_resting_orders_for_micro_refresh(3, cfg.grid_levels, false, false, true);
        assert_eq!(eff_real_three, 3);
        assert!(should_defer_micro_refresh(
            &ctx,
            &idle,
            &single_replace,
            one_sec_ago,
            now,
            eff_real_three,
        ));
    }

    #[test]
    fn passive_cancel_guard_never_defers_adverse_price_drift() {
        let now = Instant::now();
        let active = vec![
            ActiveOrder {
                client_order_id: 1,
                order_index: Some(101),
                lifecycle: OrderLifecycle::Open,
                side: OrderSide::Buy,
                price: 2000.0,
                size: 0.015,
                reduce_only: false,
                placed_at: now,
            },
            ActiveOrder {
                client_order_id: 2,
                order_index: Some(102),
                lifecycle: OrderLifecycle::Open,
                side: OrderSide::Sell,
                price: 2002.0,
                size: 0.015,
                reduce_only: false,
                placed_at: now,
            },
        ];
        // Buy adverse drift (target dropped 2000 -> 1999): old bid is stale-aggressive -> FALSE.
        assert!(!cancels_are_strictly_passive(
            &active,
            Side::Buy,
            &[101],
            1999.0,
        ));
        // Buy passive drift (target rose 2000 -> 2001): old bid is strictly safer -> TRUE.
        assert!(cancels_are_strictly_passive(
            &active,
            Side::Buy,
            &[101],
            2001.0,
        ));
        // Sell adverse drift (target rose 2002 -> 2003): old ask is stale-aggressive -> FALSE.
        assert!(!cancels_are_strictly_passive(
            &active,
            Side::Sell,
            &[102],
            2003.0,
        ));
        // Sell passive drift (target dropped 2002 -> 2001): old ask is strictly safer -> TRUE.
        assert!(cancels_are_strictly_passive(
            &active,
            Side::Sell,
            &[102],
            2001.0,
        ));
        // Non-positive / invalid target price, empty cancel list, or client_order_id (not order_index): fail closed -> FALSE.
        assert!(!cancels_are_strictly_passive(
            &active,
            Side::Buy,
            &[101],
            0.0
        ));
        assert!(!cancels_are_strictly_passive(
            &active,
            Side::Buy,
            &[],
            2001.0
        ));
        assert!(!cancels_are_strictly_passive(
            &active,
            Side::Buy,
            &[1],
            2001.0,
        ));
        assert!(!cancels_are_strictly_passive(
            &active,
            Side::Buy,
            &[999],
            2001.0,
        ));

        // Codex 11:49 CST cross-collision & lifecycle regression:
        // Order A (client=77, order_index=10, price=101 aggressive vs target 100) collides
        // numerically with Order B (client=10, order_index=88, price=98 passive). Canceling
        // order_index=10 MUST resolve Order A and return false!
        let collided = vec![
            ActiveOrder {
                client_order_id: 77,
                order_index: Some(10),
                lifecycle: OrderLifecycle::Open,
                side: OrderSide::Buy,
                price: 101.0,
                size: 0.015,
                reduce_only: false,
                placed_at: now,
            },
            ActiveOrder {
                client_order_id: 10,
                order_index: Some(88),
                lifecycle: OrderLifecycle::Open,
                side: OrderSide::Buy,
                price: 98.0,
                size: 0.015,
                reduce_only: false,
                placed_at: now,
            },
        ];
        assert!(!cancels_are_strictly_passive(
            &collided,
            Side::Buy,
            &[10],
            100.0,
        ));
        // Passive order_index=88 (price=98 <= 100) returns true; pending-cancel order fails closed.
        assert!(cancels_are_strictly_passive(
            &collided,
            Side::Buy,
            &[88],
            100.0,
        ));
        let mut pending = collided[1].clone();
        pending.lifecycle = OrderLifecycle::PendingCancel;
        assert!(!cancels_are_strictly_passive(
            &[pending],
            Side::Buy,
            &[88],
            100.0,
        ));
    }
}
