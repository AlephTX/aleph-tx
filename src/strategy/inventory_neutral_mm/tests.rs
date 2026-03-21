use super::*;
use super::components::{
    build_execution_plan, decide_quote_cycle, inventory_skew_ratio, min_quotable_size,
    position_for_quoting, residual_exposure_abs, safe_available_balance, scaled_base_order_size,
    scaled_inventory_urgency_threshold,
    scaled_max_position, toxicity_size_scale, toxicity_spread_multiplier, utilization_floor_base_order_size,
    usable_balance_fraction, QuoteCycleDecision,
};
use super::execution::InventoryContext;
use super::pricing::{
    anchor_quotes_to_touch, cleanup_reference_mid, effective_penny_ticks,
    fallback_bbo_prices, inventory_adjusted_half_spreads, local_reference_mid,
    stabilize_crossed_quotes, AnchorParams,
};
use crate::exchange::{OrderType, Side};
use crate::order_tracker::OrderLifecycle;

fn test_config() -> InventoryNeutralMMConfig {
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

fn size_for_notional(notional_usd: f64, mid: f64) -> f64 {
    notional_usd / mid
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
fn builds_multiple_grid_levels_when_inventory_budget_allows() {
    let config = test_config();

    let quotes = InventoryNeutralMM::build_grid_plan(
        &config,
        Side::Buy,
        OrderType::PostOnly,
        2100.0,
        config.base_order_size,
    );

    assert!(quotes.len() >= 4);
    assert_eq!(quotes[0].price, 2100.0);
    assert!(quotes[1].price < quotes[0].price);
    assert!(quotes[2].price < quotes[1].price);
    assert!(quotes[0].size >= quotes[1].size);
    assert!(quotes[1].size >= quotes[2].size);
}

#[test]
fn min_quotable_size_rounds_up_to_safe_step_boundary() {
    let config = test_config();
    let min_size = min_quotable_size(&config, 2254.57);

    assert!((min_size - 0.0056).abs() < 1e-10);
}

#[test]
fn min_quotable_size_stays_above_recent_lighter_reject_edge_case() {
    let config = test_config();
    let min_size = min_quotable_size(&config, 2290.62);

    assert!((min_size - 0.0055).abs() < 1e-10);
}

#[test]
fn keeps_existing_levels_that_already_match_grid() {
    let config = test_config();
    let desired = InventoryNeutralMM::build_grid_plan(
        &config,
        Side::Buy,
        OrderType::PostOnly,
        2100.0,
        config.base_order_size,
    );

    let existing = vec![
        active_order(1, 101, OrderSide::Buy, desired[0].price, desired[0].size, 30),
        active_order(2, 102, OrderSide::Buy, desired[1].price, desired[1].size, 30),
        active_order(3, 103, OrderSide::Buy, desired[2].price, desired[2].size, 30),
        active_order(4, 104, OrderSide::Buy, desired[3].price, desired[3].size, 30),
        active_order(5, 105, OrderSide::Buy, desired[4].price, desired[4].size, 30),
    ];

    let (to_cancel, to_place) = InventoryNeutralMM::reconcile_side_plan(
        &existing,
        &desired,
        0.05,
        config.step_size,
        0.12,
        Duration::from_secs(config.order_ttl_secs),
        2,
    );

    assert!(to_cancel.is_empty());
    assert!(to_place.is_empty());
}

#[test]
fn ttl_prevents_fresh_orders_from_immediate_requote_cancels() {
    let config = test_config();
    let desired = InventoryNeutralMM::build_grid_plan(
        &config,
        Side::Sell,
        OrderType::PostOnly,
        2110.0,
        config.base_order_size,
    );

    let existing = vec![active_order(
        10,
        201,
        OrderSide::Sell,
        desired[0].price + 1.0,
        desired[0].size,
        1,
    )];

    let (to_cancel, to_place) = InventoryNeutralMM::reconcile_side_plan(
        &existing,
        &desired,
        0.05,
        config.step_size,
        0.12,
        Duration::from_secs(config.order_ttl_secs),
        2,
    );

    assert!(to_cancel.is_empty());
    assert_eq!(to_place.len(), desired.len().saturating_sub(existing.len()));
}

#[test]
fn stale_mismatched_orders_are_canceled_and_missing_levels_replaced() {
    let config = test_config();
    let desired = InventoryNeutralMM::build_grid_plan(
        &config,
        Side::Sell,
        OrderType::PostOnly,
        2110.0,
        config.base_order_size,
    );

    let existing = vec![
        active_order(20, 301, OrderSide::Sell, desired[0].price, desired[0].size, 30),
        active_order(21, 302, OrderSide::Sell, desired[1].price + 0.75, desired[1].size, 30),
    ];

    let (to_cancel, to_place) = InventoryNeutralMM::reconcile_side_plan(
        &existing,
        &desired,
        0.05,
        config.step_size,
        0.12,
        Duration::from_secs(config.order_ttl_secs),
        2,
    );

    assert_eq!(to_cancel, vec![302]);
    assert_eq!(to_place.len(), desired.len().saturating_sub(1));
    assert_eq!(to_place[0].price, desired[1].price);
    assert_eq!(to_place[1].price, desired[2].price);
}

#[test]
fn fresh_mismatched_orders_do_not_allow_side_order_count_to_balloon() {
    let config = test_config();
    let desired = InventoryNeutralMM::build_grid_plan(
        &config,
        Side::Buy,
        OrderType::PostOnly,
        2100.0,
        config.base_order_size,
    );

    let existing = vec![
        active_order(1, 101, OrderSide::Buy, desired[0].price + 0.80, desired[0].size, 1),
        active_order(2, 102, OrderSide::Buy, desired[1].price + 0.80, desired[1].size, 1),
    ];

    let (to_cancel, to_place) = InventoryNeutralMM::reconcile_side_plan(
        &existing,
        &desired,
        0.05,
        config.step_size,
        0.12,
        Duration::from_secs(config.order_ttl_secs),
        2,
    );

    assert!(to_cancel.is_empty());
    assert_eq!(to_place.len(), desired.len().saturating_sub(existing.len()));
}

#[test]
fn modest_size_drift_within_tolerance_does_not_force_requote() {
    let config = test_config();
    let desired = InventoryNeutralMM::build_grid_plan(
        &config,
        Side::Buy,
        OrderType::PostOnly,
        2100.0,
        0.0190,
    );

    let existing = vec![
        active_order(1, 101, OrderSide::Buy, desired[0].price, 0.0189, 30),
        active_order(2, 102, OrderSide::Buy, desired[1].price, desired[1].size, 30),
        active_order(3, 103, OrderSide::Buy, desired[2].price, desired[2].size, 30),
    ];

    let (to_cancel, to_place) = InventoryNeutralMM::reconcile_side_plan(
        &existing,
        &desired,
        0.05,
        config.step_size,
        0.12,
        Duration::from_secs(config.order_ttl_secs),
        2,
    );

    assert!(to_cancel.is_empty());
    assert!(to_place.len() < desired.len().saturating_sub(2));
}

#[test]
fn top_levels_use_stickier_price_threshold_to_reduce_queue_churn() {
    let config = test_config();
    let desired = InventoryNeutralMM::build_grid_plan(
        &config,
        Side::Buy,
        OrderType::PostOnly,
        2100.0,
        config.base_order_size,
    );

    let base_threshold = 2100.0 * config.grid_spacing_bps / 10000.0;
    let sticky_threshold = base_threshold * 1.5;
    let top_level_drift = base_threshold * 1.25;
    assert!(top_level_drift > base_threshold);
    assert!(top_level_drift < sticky_threshold);

    let existing = vec![
        active_order(
            1,
            101,
            OrderSide::Buy,
            desired[0].price + top_level_drift,
            desired[0].size,
            30,
        ),
        active_order(2, 102, OrderSide::Buy, desired[1].price, desired[1].size, 30),
        active_order(3, 103, OrderSide::Buy, desired[2].price, desired[2].size, 30),
    ];

    let (to_cancel, to_place) = InventoryNeutralMM::reconcile_side_plan(
        &existing,
        &desired,
        base_threshold,
        config.step_size,
        0.12,
        Duration::from_secs(config.order_ttl_secs),
        2,
    );

    assert!(to_cancel.is_empty());
    assert!(to_place.len() < desired.len().saturating_sub(2));
}

#[test]
fn calm_mode_widens_matching_threshold_for_deeper_levels_too() {
    let config = test_config();
    let desired = InventoryNeutralMM::build_grid_plan(
        &config,
        Side::Buy,
        OrderType::PostOnly,
        2100.0,
        config.base_order_size,
    );

    let base_threshold = 2100.0 * config.grid_spacing_bps / 10000.0;
    let calm_deeper_drift = base_threshold * 1.4;
    assert!(calm_deeper_drift > base_threshold);

    let existing = vec![
        active_order(1, 101, OrderSide::Buy, desired[0].price, desired[0].size, 30),
        active_order(2, 102, OrderSide::Buy, desired[1].price, desired[1].size, 30),
        active_order(
            3,
            103,
            OrderSide::Buy,
            desired[2].price + calm_deeper_drift,
            desired[2].size,
            30,
        ),
    ];

    let (to_cancel, to_place) = InventoryNeutralMM::reconcile_side_plan(
        &existing,
        &desired,
        base_threshold,
        config.step_size,
        0.12,
        Duration::from_secs(config.order_ttl_secs),
        1,
    );

    assert!(to_cancel.is_empty());
    assert!(to_place.len() < desired.len().saturating_sub(2));
}

#[test]
fn calm_side_requote_cooldown_defers_back_to_back_replace_cycles() {
    let config = test_config();
    let desired = InventoryNeutralMM::build_grid_plan(
        &config,
        Side::Buy,
        OrderType::PostOnly,
        2100.0,
        config.base_order_size,
    );

    let mut existing = Vec::new();
    for (i, quote) in desired.iter().enumerate() {
        let age_secs = if i == 0 { 1 } else { 30 };
        let price = if i == 1 { quote.price + 2.0 } else { quote.price };
        existing.push(active_order(
            (i + 1) as i64,
            (101 + i) as i64,
            OrderSide::Buy,
            price,
            quote.size,
            age_secs,
        ));
    }

    let (to_cancel, to_place) = InventoryNeutralMM::reconcile_side_plan(
        &existing,
        &desired,
        0.05,
        config.step_size,
        0.12,
        Duration::from_secs(config.order_ttl_secs),
        1,
    );

    assert!(to_cancel.is_empty());
    assert!(to_place.is_empty());
}

#[test]
fn calm_mode_requires_longer_min_lifetime_before_replacing_top_levels() {
    let config = test_config();
    let desired = InventoryNeutralMM::build_grid_plan(
        &config,
        Side::Buy,
        OrderType::PostOnly,
        2100.0,
        config.base_order_size,
    );

    let mut existing = Vec::new();
    for (i, quote) in desired.iter().enumerate() {
        let age_secs = 11;
        let price = if i < 2 { quote.price + 1.20 } else { quote.price };
        existing.push(active_order(
            (i + 1) as i64,
            (101 + i) as i64,
            OrderSide::Buy,
            price,
            quote.size,
            age_secs,
        ));
    }

    let (to_cancel, to_place) = InventoryNeutralMM::reconcile_side_plan(
        &existing,
        &desired,
        0.05,
        config.step_size,
        0.12,
        Duration::from_secs(config.order_ttl_secs),
        1,
    );

    assert!(to_cancel.is_empty());
    assert!(to_place.is_empty());
}

#[test]
fn execution_plan_uses_at_least_half_grid_spacing_as_requote_threshold() {
    let config = test_config();
    let target = QuoteTarget {
        bid_price: 2090.0,
        ask_price: 2092.0,
        bid_size: config.base_order_size,
        ask_size: config.base_order_size,
    };

    let plan = build_execution_plan(
        &config,
        target,
        2100.0,
        config.inventory_urgency_threshold * 2.0,
        config.base_order_size,
        config.inventory_urgency_threshold,
    );
    let half_grid_spacing = 2100.0 * config.grid_spacing_bps / 20000.0;

    assert!((plan.requote_threshold - half_grid_spacing).abs() <= 1e-9);
}

#[test]
fn execution_plan_uses_full_grid_spacing_inside_inventory_deadband() {
    let config = test_config();
    let target = QuoteTarget {
        bid_price: 2090.0,
        ask_price: 2092.0,
        bid_size: config.base_order_size,
        ask_size: config.base_order_size,
    };

    let plan = build_execution_plan(
        &config,
        target,
        2100.0,
        config.step_size,
        config.base_order_size,
        config.inventory_urgency_threshold,
    );
    let full_grid_spacing = 2100.0 * config.grid_spacing_bps / 10000.0;

    assert!((plan.requote_threshold - full_grid_spacing).abs() <= 1e-9);
}

#[test]
fn execution_plan_uses_full_grid_spacing_below_inventory_urgency() {
    let config = test_config();
    let target = QuoteTarget {
        bid_price: 2090.0,
        ask_price: 2092.0,
        bid_size: config.base_order_size,
        ask_size: config.base_order_size,
    };

    let plan = build_execution_plan(
        &config,
        target,
        2100.0,
        config.inventory_urgency_threshold * 0.9,
        config.base_order_size,
        config.inventory_urgency_threshold,
    );
    let full_grid_spacing = 2100.0 * config.grid_spacing_bps / 10000.0;

    assert!((plan.requote_threshold - full_grid_spacing).abs() <= 1e-9);
}

#[test]
fn pending_create_without_order_index_is_not_scheduled_for_cancel() {
    let config = test_config();
    let desired = InventoryNeutralMM::build_grid_plan(
        &config,
        Side::Sell,
        OrderType::PostOnly,
        2110.0,
        config.base_order_size,
    );

    let existing = vec![ActiveOrder {
        client_order_id: 99,
        order_index: None,
        lifecycle: OrderLifecycle::PendingCreate,
        side: OrderSide::Sell,
        price: desired[0].price + 1.0,
        size: desired[0].size,
        placed_at: Instant::now() - Duration::from_secs(30),
    }];

    let (to_cancel, to_place) = InventoryNeutralMM::reconcile_side_plan(
        &existing,
        &desired,
        0.05,
        config.step_size,
        0.12,
        Duration::from_secs(config.order_ttl_secs),
        2,
    );

    assert!(to_cancel.is_empty());
    assert_eq!(to_place.len(), desired.len().saturating_sub(existing.len()));
}

#[test]
fn risk_limits_respect_existing_worst_case_exposure() {
    let config = test_config();
    let risk = RiskSnapshot {
        raw_available_balance: 100.0,
        position_for_quoting: 0.0,
        worst_case_long: 0.19,
        worst_case_short: -0.05,
        base_order_size: config.base_order_size,
        max_position: config.max_position,
        inventory_urgency_threshold: config.inventory_urgency_threshold,
        min_available_balance: config.min_available_balance,
        available_balance: 100.0,
        usable_balance: 70.0,
        margin_per_eth: 210.0,
        grid_multiplier: 2.0,
    };

    let (bid_size, ask_size) =
        InventoryNeutralMM::apply_risk_limits(&config, &risk, 0.03, 0.03, 2100.0);

    assert!(bid_size <= 0.01 + 1e-6);
    assert!(ask_size > 0.0);
}

#[test]
fn risk_limits_scale_down_when_margin_budget_is_tight() {
    let config = test_config();
    let risk = RiskSnapshot {
        raw_available_balance: 20.0,
        position_for_quoting: 0.0,
        worst_case_long: 0.0,
        worst_case_short: 0.0,
        base_order_size: config.base_order_size,
        max_position: config.max_position,
        inventory_urgency_threshold: config.inventory_urgency_threshold,
        min_available_balance: config.min_available_balance,
        available_balance: 20.0,
        usable_balance: 5.0,
        margin_per_eth: 210.0,
        grid_multiplier: 4.0,
    };

    let (bid_size, ask_size) =
        InventoryNeutralMM::apply_risk_limits(&config, &risk, 0.03, 0.03, 2100.0);

    assert!(bid_size < 0.03);
    assert!(ask_size < 0.03);
}

#[test]
fn pre_urgency_long_inventory_under_margin_pressure_keeps_both_sides_meaningful() {
    let config = test_config();
    let risk = RiskSnapshot {
        raw_available_balance: 25.0,
        position_for_quoting: config.base_order_size * 2.0,
        worst_case_long: 0.02,
        worst_case_short: -0.01,
        base_order_size: config.base_order_size,
        max_position: config.max_position,
        inventory_urgency_threshold: config.inventory_urgency_threshold,
        min_available_balance: config.min_available_balance,
        available_balance: 21.0,
        usable_balance: 15.0,
        margin_per_eth: 210.0,
        grid_multiplier: 4.0,
    };

    let (bid_size, ask_size) =
        InventoryNeutralMM::apply_risk_limits(&config, &risk, 0.013, 0.017, 2100.0);

    assert!(ask_size > bid_size);
    assert!(bid_size > config.step_size);
    assert!(ask_size < config.base_order_size);
}

#[test]
fn risk_limits_disable_same_side_when_inventory_urgency_triggers_long() {
    let config = test_config();
    let risk = RiskSnapshot {
        raw_available_balance: 100.0,
        position_for_quoting: config.inventory_urgency_threshold + 0.01,
        worst_case_long: 0.05,
        worst_case_short: -0.05,
        base_order_size: config.base_order_size,
        max_position: config.max_position,
        inventory_urgency_threshold: config.inventory_urgency_threshold,
        min_available_balance: config.min_available_balance,
        available_balance: 100.0,
        usable_balance: 100.0,
        margin_per_eth: 210.0,
        grid_multiplier: 2.0,
    };

    let (bid_size, ask_size) =
        InventoryNeutralMM::apply_risk_limits(&config, &risk, 0.03, 0.01, 2100.0);

    assert!(bid_size > 0.0);
    assert!(bid_size < config.base_order_size);
    assert!(ask_size >= config.base_order_size - config.step_size);
}

#[test]
fn risk_limits_disable_same_side_when_inventory_urgency_triggers_short() {
    let config = test_config();
    let risk = RiskSnapshot {
        raw_available_balance: 100.0,
        position_for_quoting: -(config.inventory_urgency_threshold + 0.01),
        worst_case_long: 0.05,
        worst_case_short: -0.05,
        base_order_size: config.base_order_size,
        max_position: config.max_position,
        inventory_urgency_threshold: config.inventory_urgency_threshold,
        min_available_balance: config.min_available_balance,
        available_balance: 100.0,
        usable_balance: 100.0,
        margin_per_eth: 210.0,
        grid_multiplier: 2.0,
    };

    let (bid_size, ask_size) =
        InventoryNeutralMM::apply_risk_limits(&config, &risk, 0.01, 0.03, 2100.0);

    assert!(bid_size >= config.base_order_size - config.step_size);
    assert!(ask_size > 0.0);
    assert!(ask_size < config.base_order_size);
}

#[test]
fn risk_limits_cap_flatten_side_when_pending_sell_would_flip_past_flat() {
    let config = test_config();
    let risk = RiskSnapshot {
        raw_available_balance: 100.0,
        position_for_quoting: 0.03,
        worst_case_long: 0.04,
        worst_case_short: -0.08,
        base_order_size: config.base_order_size,
        max_position: config.max_position,
        inventory_urgency_threshold: config.inventory_urgency_threshold,
        min_available_balance: config.min_available_balance,
        available_balance: 100.0,
        usable_balance: 100.0,
        margin_per_eth: 210.0,
        grid_multiplier: 4.0,
    };

    let (bid_size, ask_size) =
        InventoryNeutralMM::apply_risk_limits(&config, &risk, 0.01, 0.02, 2100.0);

    assert!(bid_size > 0.0);
    assert!(ask_size <= config.base_order_size + config.step_size);
}

#[test]
fn risk_limits_cap_flatten_side_when_pending_buy_would_flip_past_flat() {
    let config = test_config();
    let risk = RiskSnapshot {
        raw_available_balance: 100.0,
        position_for_quoting: -0.03,
        worst_case_long: 0.08,
        worst_case_short: -0.04,
        base_order_size: config.base_order_size,
        max_position: config.max_position,
        inventory_urgency_threshold: config.inventory_urgency_threshold,
        min_available_balance: config.min_available_balance,
        available_balance: 100.0,
        usable_balance: 100.0,
        margin_per_eth: 210.0,
        grid_multiplier: 4.0,
    };

    let (bid_size, ask_size) =
        InventoryNeutralMM::apply_risk_limits(&config, &risk, 0.02, 0.01, 2100.0);

    assert!(ask_size > 0.0);
    assert!(bid_size <= config.base_order_size + config.step_size);
}

#[test]
fn tiny_inventory_inside_deadband_keeps_both_sides_but_applies_soft_skew() {
    let config = test_config();
    let mid = 2100.0;
    let risk = RiskSnapshot {
        raw_available_balance: 100.0,
        position_for_quoting: size_for_notional(8.0, mid),
        worst_case_long: 0.0,
        worst_case_short: 0.0,
        base_order_size: config.base_order_size,
        max_position: config.max_position,
        inventory_urgency_threshold: config.inventory_urgency_threshold,
        min_available_balance: config.min_available_balance,
        available_balance: 100.0,
        usable_balance: 100.0,
        margin_per_eth: 210.0,
        grid_multiplier: 2.0,
    };

    let (bid_size, ask_size) =
        InventoryNeutralMM::calculate_asymmetric_sizes_for_config(&config, &risk, mid);

    assert!(ask_size > bid_size);
    assert!(bid_size > 0.0);
    assert!(ask_size > 0.0);
    assert!(bid_size >= config.base_order_size * 0.75);
    assert!(ask_size <= config.base_order_size * 1.25);
}

#[test]
fn inventory_outside_deadband_starts_directional_skew() {
    let config = test_config();
    let mid = 2100.0;
    let risk = RiskSnapshot {
        raw_available_balance: 100.0,
        position_for_quoting: size_for_notional(14.0, mid),
        worst_case_long: 0.0,
        worst_case_short: 0.0,
        base_order_size: config.base_order_size,
        max_position: config.max_position,
        inventory_urgency_threshold: config.inventory_urgency_threshold,
        min_available_balance: config.min_available_balance,
        available_balance: 100.0,
        usable_balance: 100.0,
        margin_per_eth: 210.0,
        grid_multiplier: 2.0,
    };

    let (bid_size, ask_size) =
        InventoryNeutralMM::calculate_asymmetric_sizes_for_config(&config, &risk, mid);

    assert!(ask_size > bid_size);
}

#[test]
fn inventory_just_outside_deadband_keeps_both_sides_meaningful() {
    let config = test_config();
    let mid = 2100.0;
    let risk = RiskSnapshot {
        raw_available_balance: 100.0,
        position_for_quoting: -size_for_notional(12.0, mid),
        worst_case_long: 0.0,
        worst_case_short: 0.0,
        base_order_size: config.base_order_size,
        max_position: config.max_position,
        inventory_urgency_threshold: config.inventory_urgency_threshold,
        min_available_balance: config.min_available_balance,
        available_balance: 100.0,
        usable_balance: 100.0,
        margin_per_eth: 210.0,
        grid_multiplier: 2.0,
    };

    let (bid_size, ask_size) =
        InventoryNeutralMM::calculate_asymmetric_sizes_for_config(&config, &risk, mid);

    assert!(bid_size > config.base_order_size);
    assert!(ask_size >= config.base_order_size * 0.5);
    assert!(bid_size > ask_size);
}

#[test]
fn inventory_beyond_urgency_threshold_reduces_same_side_to_keepalive() {
    let config = test_config();
    let risk = RiskSnapshot {
        raw_available_balance: 100.0,
        position_for_quoting: -(config.inventory_urgency_threshold + 0.01),
        worst_case_long: 0.0,
        worst_case_short: 0.0,
        base_order_size: config.base_order_size,
        max_position: config.max_position,
        inventory_urgency_threshold: config.inventory_urgency_threshold,
        min_available_balance: config.min_available_balance,
        available_balance: 100.0,
        usable_balance: 100.0,
        margin_per_eth: 210.0,
        grid_multiplier: 2.0,
    };

    let (bid_size, ask_size) =
        InventoryNeutralMM::calculate_asymmetric_sizes_for_config(&config, &risk, 2100.0);

    assert!(bid_size >= config.base_order_size - config.step_size);
    assert!(ask_size < config.base_order_size * 0.5);
}

#[test]
fn moderate_inventory_below_urgency_threshold_keeps_same_side_non_zero() {
    let config = test_config();
    let risk = RiskSnapshot {
        raw_available_balance: 100.0,
        position_for_quoting: -(config.base_order_size * 2.0),
        worst_case_long: 0.0,
        worst_case_short: 0.0,
        base_order_size: config.base_order_size,
        max_position: config.max_position,
        inventory_urgency_threshold: config.inventory_urgency_threshold,
        min_available_balance: config.min_available_balance,
        available_balance: 100.0,
        usable_balance: 100.0,
        margin_per_eth: 210.0,
        grid_multiplier: 2.0,
    };

    let (bid_size, ask_size) =
        InventoryNeutralMM::calculate_asymmetric_sizes_for_config(&config, &risk, 2100.0);

    assert!(bid_size > ask_size);
    assert!(ask_size > config.step_size);
    assert!(bid_size >= config.base_order_size * 1.15);
    assert!(ask_size <= config.base_order_size * 0.85);
}

#[test]
fn scaled_but_quotable_sizes_do_not_skip_execution() {
    let config = test_config();
    let target = QuoteTarget {
        bid_price: 2090.0,
        ask_price: 2092.0,
        bid_size: config.base_order_size - 5.0 * config.step_size,
        ask_size: config.base_order_size - 6.0 * config.step_size,
    };

    let decision = decide_quote_cycle(
        &InventoryContext {
            config: &config,
            position_for_quoting: 0.0,
            base_order_size: config.base_order_size,
            inventory_urgency_threshold: config.inventory_urgency_threshold,
            mid: 2091.0,
        },
        target,
        config.min_available_balance + 1.0,
        0.0,
    );

    assert!(matches!(decision, QuoteCycleDecision::Execute(_)));
}

#[test]
fn sub_step_sizes_still_skip_when_not_low_margin() {
    let config = test_config();
    let target = QuoteTarget {
        bid_price: 2090.0,
        ask_price: 2092.0,
        bid_size: config.step_size * 0.5,
        ask_size: config.step_size * 0.5,
    };

    let decision = decide_quote_cycle(
        &InventoryContext {
            config: &config,
            position_for_quoting: 0.0,
            base_order_size: config.base_order_size,
            inventory_urgency_threshold: config.inventory_urgency_threshold,
            mid: 2091.0,
        },
        target,
        config.min_available_balance + 1.0,
        0.0,
    );

    assert!(matches!(decision, QuoteCycleDecision::Skip));
}

#[test]
fn usable_balance_fraction_is_more_aggressive_when_inventory_and_margin_are_low() {
    let calm = usable_balance_fraction(0.0, 0.0);
    let stressed = usable_balance_fraction(1.0, 1.0);

    assert!(calm > 0.85);
    assert!(stressed < calm);
    assert!(stressed >= 0.55);
}

#[test]
fn inventory_skew_ratio_clamps_to_urgency_threshold() {
    let config = test_config();

    assert_eq!(inventory_skew_ratio(&config, 0.0), 0.0);
    assert!(inventory_skew_ratio(&config, config.inventory_urgency_threshold * 2.0) > 0.99);
    assert!(inventory_skew_ratio(&config, -config.inventory_urgency_threshold * 2.0) < -0.99);
}

#[test]
fn local_reference_mid_uses_only_local_bid_when_ask_missing() {
    let bbo = ShmBboMessage {
        seqlock: 0,
        msg_type: 0,
        exchange_id: 0,
        symbol_id: 0,
        timestamp_ns: 1,
        bid_price: 2100.0,
        bid_size: 1.0,
        ask_price: 0.0,
        ask_size: 0.0,
        _reserved: [0; 16],
    };

    let mid = local_reference_mid(&bbo, 0.01, 2.0);

    assert_eq!(mid, 2100.02);
}

#[test]
fn fallback_bbo_prices_synthesize_missing_side_around_local_mid() {
    let bbo = ShmBboMessage {
        seqlock: 0,
        msg_type: 0,
        exchange_id: 0,
        symbol_id: 0,
        timestamp_ns: 1,
        bid_price: 0.0,
        bid_size: 0.0,
        ask_price: 2100.5,
        ask_size: 1.0,
        _reserved: [0; 16],
    };

    let mid = local_reference_mid(&bbo, 0.01, 2.0);
    let (bid_price, ask_price) = fallback_bbo_prices(mid, &bbo, 0.01);

    assert_eq!(mid, 2100.48);
    assert_eq!(bid_price, 2100.47);
    assert_eq!(ask_price, 2100.5);
}

#[test]
fn anchored_quotes_stay_close_to_local_touch_without_crossing() {
    let (bid, ask) =
        anchor_quotes_to_touch(&AnchorParams {
            raw_bid: 2106.89, raw_ask: 2118.53, bid_touch: 2112.20, ask_touch: 2112.30,
            mid: 2112.25, tick_size: 0.01, penny_ticks: 1.0, inventory_urgency_ratio: 0.0, max_touch_offset_bps: 8.0,
        });

    assert!(bid < 2112.30);
    assert!(ask > 2112.20);
    assert!(2112.20 - bid <= 2112.25 * 8.0 / 10000.0 + 0.02);
    assert!(ask - 2112.30 <= 2112.25 * 8.0 / 10000.0 + 0.02);
}

#[test]
fn anchored_quotes_respect_configured_join_buffer() {
    let (bid, ask) =
        anchor_quotes_to_touch(&AnchorParams {
            raw_bid: 2100.0, raw_ask: 2110.0, bid_touch: 2105.00, ask_touch: 2105.03,
            mid: 2105.015, tick_size: 0.01, penny_ticks: 2.0, inventory_urgency_ratio: 0.0, max_touch_offset_bps: 8.0,
        });

    assert!(bid <= 2105.01);
    assert!(ask >= 2105.02);
}

#[test]
fn stabilize_crossed_quotes_recovers_to_safe_touch_band() {
    let stabilized =
        stabilize_crossed_quotes(2188.82, 2187.31, 2188.07, 2188.17, 0.01).expect("stabilized");

    assert!(stabilized.0 < stabilized.1);
    assert!((stabilized.0 - 2188.07).abs() < 1e-9);
    assert!((stabilized.1 - 2188.17).abs() < 1e-9);
}

#[test]
fn stabilize_crossed_quotes_returns_none_when_touch_is_inverted() {
    let stabilized = stabilize_crossed_quotes(2188.82, 2187.31, 2188.17, 2188.16, 0.01);

    assert!(stabilized.is_none());
}

#[test]
fn effective_penny_ticks_widens_with_toxicity_and_inventory() {
    let calm = effective_penny_ticks(1.0, 1.0, 3.0, 0.0);
    let toxic = effective_penny_ticks(1.0, 4.5, 3.0, 0.0);
    let inventory = effective_penny_ticks(1.0, 1.0, 3.0, 0.8);

    assert_eq!(calm, 1.0);
    assert!(toxic > calm);
    assert!(inventory > calm);
}

#[test]
fn inventory_adjusted_half_spreads_tighten_flatten_side() {
    let (flat_bid, flat_ask) = inventory_adjusted_half_spreads(10.0, 0.0);
    let (long_bid, long_ask) = inventory_adjusted_half_spreads(10.0, 1.0);
    let (short_bid, short_ask) = inventory_adjusted_half_spreads(10.0, -1.0);

    assert_eq!((flat_bid, flat_ask), (10.0, 10.0));
    assert!(long_bid > 10.0);
    assert!(long_ask < 10.0);
    assert!(short_bid < 10.0);
    assert!(short_ask > 10.0);
}

#[test]
fn anchored_quotes_shift_toward_flatten_side_when_inventory_is_biased() {
    let (flat_bid, flat_ask) =
        anchor_quotes_to_touch(&AnchorParams {
            raw_bid: 2100.0, raw_ask: 2110.0, bid_touch: 2105.00, ask_touch: 2105.20,
            mid: 2105.10, tick_size: 0.01, penny_ticks: 2.0, inventory_urgency_ratio: 0.0, max_touch_offset_bps: 8.0,
        });
    let (long_bid, long_ask) =
        anchor_quotes_to_touch(&AnchorParams {
            raw_bid: 2100.0, raw_ask: 2110.0, bid_touch: 2105.00, ask_touch: 2105.20,
            mid: 2105.10, tick_size: 0.01, penny_ticks: 2.0, inventory_urgency_ratio: 1.0, max_touch_offset_bps: 8.0,
        });
    let (short_bid, short_ask) =
        anchor_quotes_to_touch(&AnchorParams {
            raw_bid: 2100.0, raw_ask: 2110.0, bid_touch: 2105.00, ask_touch: 2105.20,
            mid: 2105.10, tick_size: 0.01, penny_ticks: 2.0, inventory_urgency_ratio: -1.0, max_touch_offset_bps: 8.0,
        });

    assert!(long_ask <= flat_ask);
    assert!(long_bid <= flat_bid);
    assert!(short_bid >= flat_bid);
    assert!(short_ask >= flat_ask);
}

#[test]
fn safe_available_balance_caps_raw_balance_with_margin_usage() {
    let safe_available = safe_available_balance(100.0, 50.0, 5.0, 10.0);

    assert_eq!(safe_available, 25.0);
}

#[test]
fn safe_available_balance_falls_back_to_raw_when_margin_usage_unavailable() {
    let safe_available = safe_available_balance(42.0, 0.0, 0.0, 10.0);

    assert_eq!(safe_available, 42.0);
}

#[test]
fn residual_exposure_abs_uses_more_conservative_of_exchange_and_effective_positions() {
    assert_eq!(residual_exposure_abs(-0.03, 0.05), 0.05);
    assert_eq!(residual_exposure_abs(0.07, 0.02), 0.07);
}

#[test]
fn position_for_quoting_does_not_flip_direction_against_real_exchange_inventory() {
    let config = test_config();

    let q = position_for_quoting(&config, 0.0675, -0.0372);

    assert!((q - 0.0675).abs() < 1e-9);
}

#[test]
fn position_for_quoting_uses_tracker_when_exchange_is_flat_and_drift_is_small() {
    let config = test_config();

    let q = position_for_quoting(&config, 0.0, -0.0125);

    assert!((q + 0.0125).abs() < 1e-9);
}

#[test]
fn scaled_sizes_grow_with_portfolio_value_when_using_legacy_base_units() {
    let mut config = test_config();
    config.base_order_notional_usd = 0.0;
    config.max_position_notional_usd = 0.0;
    config.inventory_urgency_notional_usd = 0.0;
    let mid = 2000.0;

    let small = scaled_base_order_size(&config, 100.0, mid);
    let large = scaled_base_order_size(&config, 1000.0, mid);
    let max_pos = scaled_max_position(&config, 1000.0, mid);

    assert!(large > small);
    assert!(max_pos > config.max_position);
}

#[test]
fn scaled_sizes_prefer_usd_notional_when_configured() {
    let mut config = test_config();
    config.base_order_notional_usd = 40.0;
    config.max_position_notional_usd = 400.0;
    config.inventory_urgency_notional_usd = 160.0;

    let mid = 2000.0;

    assert!((scaled_base_order_size(&config, 100.0, mid) - 0.02).abs() < 1e-9);
    assert!((scaled_max_position(&config, 100.0, mid) - 0.2).abs() < 1e-9);
    assert!((scaled_inventory_urgency_threshold(&config, 100.0, mid, 0.2) - 0.08).abs() < 1e-9);
}

#[test]
fn usd_notional_scales_up_with_portfolio_value() {
    let config = test_config();
    let mid = 2000.0;

    let small = scaled_base_order_size(&config, 100.0, mid);
    let large = scaled_base_order_size(&config, 1000.0, mid);
    let max_pos_large = scaled_max_position(&config, 1000.0, mid);

    assert!(large > small);
    assert!(max_pos_large > scaled_max_position(&config, 100.0, mid));
}

#[test]
fn utilization_floor_increases_top_level_size_when_budget_is_underused() {
    let config = test_config();
    let floored = utilization_floor_base_order_size(&config, 0.015, 90.0, 4.46, 2100.0);

    assert!(floored > 0.015);
}

#[test]
fn toxicity_controls_shrink_size_and_widen_spread_without_zeroing_quotes() {
    let threshold = 3.0;

    let size_scale = toxicity_size_scale(4.5, threshold);
    let spread_mult = toxicity_spread_multiplier(4.5, threshold);

    assert!(size_scale < 1.0);
    assert!(size_scale >= 0.25);
    assert!(spread_mult > 1.0);
}

#[test]
fn toxicity_controls_are_neutral_below_threshold() {
    let threshold = 3.0;

    assert_eq!(toxicity_size_scale(2.0, threshold), 1.0);
    assert_eq!(toxicity_spread_multiplier(2.0, threshold), 1.0);
}

#[test]
fn cleanup_reference_mid_rejects_empty_book() {
    let bbo = ShmBboMessage::default();

    assert!(cleanup_reference_mid(&bbo, 0.01, 2.0).is_none());
}

#[test]
fn decide_quote_cycle_requests_clear_when_low_margin_and_quotes_are_too_small() {
    let config = test_config();
    let target = QuoteTarget {
        bid_price: 2100.0,
        ask_price: 2100.1,
        bid_size: 0.0,
        ask_size: 0.0,
    };

    let decision = decide_quote_cycle(
        &InventoryContext {
            config: &config,
            position_for_quoting: 0.0,
            base_order_size: config.base_order_size,
            inventory_urgency_threshold: config.inventory_urgency_threshold,
            mid: 2100.0,
        },
        target,
        1.0,
        0.0,
    );

    assert!(matches!(decision, QuoteCycleDecision::ClearForLowMargin));
}

#[test]
fn decide_quote_cycle_executes_when_at_least_one_side_is_actionable() {
    let config = test_config();
    let target = QuoteTarget {
        bid_price: 2100.0,
        ask_price: 2100.1,
        bid_size: config.base_order_size,
        ask_size: 0.0,
    };

    let decision = decide_quote_cycle(
        &InventoryContext {
            config: &config,
            position_for_quoting: 0.0,
            base_order_size: config.base_order_size,
            inventory_urgency_threshold: config.inventory_urgency_threshold,
            mid: 2100.0,
        },
        target.clone(),
        10.0,
        0.0,
    );

    match decision {
        QuoteCycleDecision::Execute(plan) => {
            assert_eq!(plan.target.bid_price, target.bid_price);
            assert_eq!(plan.target.bid_size, target.bid_size);
        }
        _ => panic!("expected execute decision"),
    }
}

#[test]
fn decide_quote_cycle_flattens_when_low_margin_and_position_is_not_flat() {
    let config = test_config();
    let target = QuoteTarget {
        bid_price: 2100.0,
        ask_price: 2100.1,
        bid_size: 0.0,
        ask_size: 0.0,
    };

    let decision = decide_quote_cycle(
        &InventoryContext {
            config: &config,
            position_for_quoting: 0.02,
            base_order_size: config.base_order_size,
            inventory_urgency_threshold: config.inventory_urgency_threshold,
            mid: 2100.0,
        },
        target,
        1.0,
        0.02,
    );

    assert!(matches!(decision, QuoteCycleDecision::FlattenForLowMargin));
}
