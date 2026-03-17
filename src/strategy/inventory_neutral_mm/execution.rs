use super::ActiveOrder;
use super::components::{
    build_grid_plan, inventory_deadband_size, min_quotable_size, reconcile_side_plan,
    FLOAT_EPSILON,
};
use crate::config::InventoryNeutralMMConfig;
use crate::error::TradingError;
use crate::exchange::{BatchResult, OrderParams, OrderType, Side};
use crate::order_tracker::{OrderLifecycle, OrderSide};
use crate::telemetry::TelemetryCollector;
use std::time::Duration;

const CALM_SIDE_REQUOTE_REPLACEMENTS_PER_CYCLE: usize = 1;
const URGENT_SIDE_REQUOTE_REPLACEMENTS_PER_CYCLE: usize = 2;
const CALM_SIZE_TOLERANCE_RATIO: f64 = 0.25;
const PRE_URGENCY_SIZE_TOLERANCE_RATIO: f64 = 0.18;
const ACTIVE_SIZE_TOLERANCE_RATIO: f64 = 0.12;

#[derive(Debug, Clone)]
pub(super) struct SideExecutionPlan {
    pub to_cancel: Vec<i64>,
    pub to_place: Vec<OrderParams>,
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
    if total_sz + FLOAT_EPSILON < min_size {
        let min_lifetime = Duration::from_secs(config.order_ttl_secs.max(1));
        return SideExecutionPlan {
            to_cancel: side_orders
                .iter()
                .filter(|order| {
                    order.order_index.is_some()
                        && order.lifecycle != OrderLifecycle::PendingCancel
                        && order.placed_at.elapsed() >= min_lifetime
                })
                .filter_map(|order| order.order_index)
                .collect(),
            to_place: Vec::new(),
        };
    }

    let desired_quotes = build_grid_plan(config, side, order_type, target_px, total_sz);
    if desired_quotes.is_empty() {
        return SideExecutionPlan {
            to_cancel: Vec::new(),
            to_place: Vec::new(),
        };
    }

    let min_lifetime = Duration::from_secs(config.order_ttl_secs.max(1));
    let (to_cancel, to_place) = reconcile_side_plan(
        &side_orders,
        &desired_quotes,
        threshold,
        config.step_size,
        size_tolerance_ratio,
        min_lifetime,
        max_replacements_per_cycle,
    );

    SideExecutionPlan { to_cancel, to_place }
}

pub(super) fn size_tolerance_ratio_for_requote(
    config: &InventoryNeutralMMConfig,
    position_for_quoting: f64,
    base_order_size: f64,
    inventory_urgency_threshold: f64,
    mid: f64,
) -> f64 {
    let deadband = inventory_deadband_size(
        config,
        base_order_size,
        inventory_urgency_threshold.max(config.step_size),
        mid,
    );
    let urgency = inventory_urgency_threshold.max(config.step_size);
    if position_for_quoting.abs() <= deadband {
        CALM_SIZE_TOLERANCE_RATIO
    } else if position_for_quoting.abs() <= urgency {
        PRE_URGENCY_SIZE_TOLERANCE_RATIO
    } else {
        ACTIVE_SIZE_TOLERANCE_RATIO
    }
}

pub(super) fn max_side_requote_replacements_per_cycle(
    config: &InventoryNeutralMMConfig,
    position_for_quoting: f64,
    base_order_size: f64,
    inventory_urgency_threshold: f64,
    mid: f64,
) -> usize {
    let deadband = inventory_deadband_size(
        config,
        base_order_size,
        inventory_urgency_threshold.max(config.step_size),
        mid,
    );
    if position_for_quoting.abs() <= deadband {
        return CALM_SIDE_REQUOTE_REPLACEMENTS_PER_CYCLE;
    }

    let urgency = inventory_urgency_threshold.max(config.step_size);
    let bias_progress = ((position_for_quoting.abs() - deadband) / (urgency - deadband).max(config.step_size))
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
    config: &InventoryNeutralMMConfig,
    position_for_quoting: f64,
    base_order_size: f64,
    inventory_urgency_threshold: f64,
    mid: f64,
    bid_plan: &SideExecutionPlan,
    ask_plan: &SideExecutionPlan,
) -> bool {
    let symmetric_mode = position_for_quoting.abs()
        <= inventory_deadband_size(
            config,
            base_order_size,
            inventory_urgency_threshold.max(config.step_size),
            mid,
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
    (bid_has_cancel && !bid_has_place && !ask_has_cancel && !ask_has_place)
        || (ask_has_cancel && !ask_has_place && !bid_has_cancel && !bid_has_place)
}

pub(super) fn should_defer_cancel_only_refresh(
    config: &InventoryNeutralMMConfig,
    position_for_quoting: f64,
    base_order_size: f64,
    inventory_urgency_threshold: f64,
    mid: f64,
    bid_plan: &SideExecutionPlan,
    ask_plan: &SideExecutionPlan,
) -> bool {
    let symmetric_mode = position_for_quoting.abs()
        <= inventory_deadband_size(
            config,
            base_order_size,
            inventory_urgency_threshold.max(config.step_size),
            mid,
        );
    if !symmetric_mode {
        return false;
    }

    let has_any_place = !bid_plan.to_place.is_empty() || !ask_plan.to_place.is_empty();
    let has_any_cancel = !bid_plan.to_cancel.is_empty() || !ask_plan.to_cancel.is_empty();

    has_any_cancel && !has_any_place
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

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
        assert!(plan.to_place.len() >= 1);
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

    #[test]
    fn calm_inventory_uses_wider_size_tolerance() {
        let config = config();
        let calm_ratio = size_tolerance_ratio_for_requote(
            &config,
            config.step_size,
            config.base_order_size,
            config.inventory_urgency_threshold,
            2100.0,
        );
        let active_ratio = size_tolerance_ratio_for_requote(
            &config,
            config.inventory_urgency_threshold * 2.0,
            config.base_order_size,
            config.inventory_urgency_threshold,
            2100.0,
        );
        let pre_urgency_ratio = size_tolerance_ratio_for_requote(
            &config,
            config.inventory_urgency_threshold * 0.75,
            config.base_order_size,
            config.inventory_urgency_threshold,
            2100.0,
        );

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

    #[test]
    fn defer_one_sided_requote_in_symmetric_mode_when_only_one_side_cancels() {
        let config = config();
        let bid_plan = SideExecutionPlan {
            to_cancel: vec![101],
            to_place: Vec::new(),
        };
        let ask_plan = SideExecutionPlan {
            to_cancel: Vec::new(),
            to_place: Vec::new(),
        };

        assert!(should_defer_one_sided_requote(
            &config, 0.005, 0.015, 0.08, 2100.0, &bid_plan, &ask_plan
        ));
    }

    #[test]
    fn defer_one_sided_requote_for_small_residual_inventory_up_to_top_level_size() {
        let config = config();
        let bid_plan = SideExecutionPlan {
            to_cancel: Vec::new(),
            to_place: Vec::new(),
        };
        let ask_plan = SideExecutionPlan {
            to_cancel: vec![201],
            to_place: Vec::new(),
        };

        assert!(should_defer_one_sided_requote(
            &config, 0.005, 0.015, 0.08, 2100.0, &bid_plan, &ask_plan
        ));
    }

    #[test]
    fn do_not_defer_one_sided_replenishment_in_symmetric_mode() {
        let config = config();
        let bid_plan = SideExecutionPlan {
            to_cancel: Vec::new(),
            to_place: vec![OrderParams {
                size: 0.015,
                price: 2100.0,
                side: Side::Buy,
                order_type: OrderType::PostOnly,
                reduce_only: false,
            }],
        };
        let ask_plan = SideExecutionPlan {
            to_cancel: Vec::new(),
            to_place: Vec::new(),
        };

        assert!(!should_defer_one_sided_requote(
            &config, 0.005, 0.015, 0.08, 2100.0, &bid_plan, &ask_plan
        ));
    }

    #[test]
    fn defer_cancel_only_refresh_in_symmetric_mode() {
        let config = config();
        let bid_plan = SideExecutionPlan {
            to_cancel: vec![101],
            to_place: Vec::new(),
        };
        let ask_plan = SideExecutionPlan {
            to_cancel: vec![201],
            to_place: Vec::new(),
        };

        assert!(should_defer_cancel_only_refresh(
            &config, 0.0, 0.015, 0.08, 2100.0, &bid_plan, &ask_plan
        ));
    }

    #[test]
    fn do_not_defer_cancel_only_refresh_when_replenishment_needed() {
        let config = config();
        let bid_plan = SideExecutionPlan {
            to_cancel: vec![101],
            to_place: vec![OrderParams {
                size: 0.015,
                price: 2100.0,
                side: Side::Buy,
                order_type: OrderType::PostOnly,
                reduce_only: false,
            }],
        };
        let ask_plan = SideExecutionPlan {
            to_cancel: vec![201],
            to_place: Vec::new(),
        };

        assert!(!should_defer_cancel_only_refresh(
            &config, 0.0, 0.015, 0.08, 2100.0, &bid_plan, &ask_plan
        ));
    }

    #[test]
    fn calm_inventory_uses_one_side_replacement_per_cycle() {
        let config = config();
        assert_eq!(
            max_side_requote_replacements_per_cycle(&config, 0.0, 0.015, 0.08, 2100.0),
            1
        );
    }

    #[test]
    fn urgent_inventory_uses_two_side_replacements_per_cycle() {
        let config = config();
        assert_eq!(
            max_side_requote_replacements_per_cycle(&config, 0.08, 0.015, 0.08, 2100.0),
            2
        );
    }
}
