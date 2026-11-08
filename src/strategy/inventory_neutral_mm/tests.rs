use super::components::{
    QuoteCycleDecision, build_execution_plan, decide_quote_cycle, inventory_skew_ratio,
    min_quotable_size, position_for_quoting, residual_exposure_abs, safe_available_balance,
    scaled_base_order_size, scaled_inventory_urgency_threshold, scaled_max_position,
    toxicity_size_scale, toxicity_spread_multiplier, usable_balance_fraction,
    utilization_floor_base_order_size,
};
use super::execution::InventoryContext;
use super::pricing::{
    AnchorParams, anchor_quotes_to_touch, cleanup_reference_mid, effective_penny_ticks,
    fallback_bbo_prices, local_reference_mid, stabilize_crossed_quotes,
};
use super::*;
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
        active_order(
            1,
            101,
            OrderSide::Buy,
            desired[0].price,
            desired[0].size,
            30,
        ),
        active_order(
            2,
            102,
            OrderSide::Buy,
            desired[1].price,
            desired[1].size,
            30,
        ),
        active_order(
            3,
            103,
            OrderSide::Buy,
            desired[2].price,
            desired[2].size,
            30,
        ),
        active_order(
            4,
            104,
            OrderSide::Buy,
            desired[3].price,
            desired[3].size,
            30,
        ),
        active_order(
            5,
            105,
            OrderSide::Buy,
            desired[4].price,
            desired[4].size,
            30,
        ),
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
        active_order(
            20,
            301,
            OrderSide::Sell,
            desired[0].price,
            desired[0].size,
            30,
        ),
        active_order(
            21,
            302,
            OrderSide::Sell,
            desired[1].price + 0.75,
            desired[1].size,
            30,
        ),
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
        active_order(
            1,
            101,
            OrderSide::Buy,
            desired[0].price + 0.80,
            desired[0].size,
            1,
        ),
        active_order(
            2,
            102,
            OrderSide::Buy,
            desired[1].price + 0.80,
            desired[1].size,
            1,
        ),
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
        active_order(
            2,
            102,
            OrderSide::Buy,
            desired[1].price,
            desired[1].size,
            30,
        ),
        active_order(
            3,
            103,
            OrderSide::Buy,
            desired[2].price,
            desired[2].size,
            30,
        ),
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
        active_order(
            2,
            102,
            OrderSide::Buy,
            desired[1].price,
            desired[1].size,
            30,
        ),
        active_order(
            3,
            103,
            OrderSide::Buy,
            desired[2].price,
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
        active_order(
            1,
            101,
            OrderSide::Buy,
            desired[0].price,
            desired[0].size,
            30,
        ),
        active_order(
            2,
            102,
            OrderSide::Buy,
            desired[1].price,
            desired[1].size,
            30,
        ),
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
        let price = if i == 1 {
            quote.price + 2.0
        } else {
            quote.price
        };
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
    let config = InventoryNeutralMMConfig {
        order_ttl_secs: 5,
        ..test_config()
    };
    let desired = InventoryNeutralMM::build_grid_plan(
        &config,
        Side::Buy,
        OrderType::PostOnly,
        2100.0,
        config.base_order_size,
    );

    let mut existing = Vec::new();
    for (i, quote) in desired.iter().enumerate() {
        let age_secs = 6;
        let price = if i < 2 {
            quote.price + 1.20
        } else {
            quote.price
        };
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

    assert_eq!(bid_size, 0.0);
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
    assert_eq!(ask_size, 0.0);
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
fn inventory_beyond_urgency_threshold_disables_same_side() {
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
    assert_eq!(ask_size, 0.0);
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
fn sub_step_sizes_request_clear_of_existing_risk() {
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

    assert!(matches!(decision, QuoteCycleDecision::ClearForRisk));
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
fn anchored_quotes_preserve_fair_value_edge_without_crossing() {
    let (bid, ask) = anchor_quotes_to_touch(&AnchorParams {
        raw_bid: 2106.89,
        raw_ask: 2118.53,
        bid_touch: 2112.20,
        ask_touch: 2112.30,
        tick_size: 0.01,
        penny_ticks: 1.0,
        inventory_urgency_ratio: 0.0,
    });

    assert!(bid < 2112.30);
    assert!(ask > 2112.20);
    assert!(bid <= 2106.89);
    assert!(ask >= 2118.53);
}

#[test]
fn maker_price_bounds_preserve_each_fills_net_edge_after_fees() {
    let bounds = MakerPriceBounds::new(1_997.10, 2_003.90, 0.38, 1.0, 0.01).unwrap();
    let maker_fee = 0.38 / 10_000.0;
    let profit = 0.5 / 10_000.0;
    assert!(1_997.10 - bounds.max_bid * (1.0 + maker_fee) >= bounds.max_bid * profit);
    assert!(bounds.min_ask * (1.0 - maker_fee) - 2_003.90 >= bounds.min_ask * profit);
    assert!(bounds.allows(OrderSide::Buy, bounds.max_bid));
    assert!(bounds.allows(OrderSide::Sell, bounds.min_ask));
    // A wide two-sided spread can hide a losing fill on one leg.
    assert!(!bounds.allows(OrderSide::Buy, 2_000.00));
    assert!(!bounds.allows(OrderSide::Sell, 2_002.00));
}

#[test]
fn anchored_quotes_respect_configured_join_buffer() {
    let (bid, ask) = anchor_quotes_to_touch(&AnchorParams {
        raw_bid: 2100.0,
        raw_ask: 2110.0,
        bid_touch: 2105.00,
        ask_touch: 2105.03,
        tick_size: 0.01,
        penny_ticks: 2.0,
        inventory_urgency_ratio: 0.0,
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
fn anchored_quotes_shift_toward_flatten_side_when_inventory_is_biased() {
    let (flat_bid, flat_ask) = anchor_quotes_to_touch(&AnchorParams {
        raw_bid: 2100.0,
        raw_ask: 2110.0,
        bid_touch: 2105.00,
        ask_touch: 2105.20,
        tick_size: 0.01,
        penny_ticks: 2.0,
        inventory_urgency_ratio: 0.0,
    });
    let (long_bid, long_ask) = anchor_quotes_to_touch(&AnchorParams {
        raw_bid: 2100.0,
        raw_ask: 2110.0,
        bid_touch: 2105.00,
        ask_touch: 2105.20,
        tick_size: 0.01,
        penny_ticks: 2.0,
        inventory_urgency_ratio: 1.0,
    });
    let (short_bid, short_ask) = anchor_quotes_to_touch(&AnchorParams {
        raw_bid: 2100.0,
        raw_ask: 2110.0,
        bid_touch: 2105.00,
        ask_touch: 2105.20,
        tick_size: 0.01,
        penny_ticks: 2.0,
        inventory_urgency_ratio: -1.0,
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

#[test]
fn risk_budgets_hold_across_inventory_and_margin_scenarios() {
    let config = test_config();
    for position in [-0.2, -0.15, -0.09, -0.01, 0.0, 0.01, 0.09, 0.15, 0.2] {
        for balance in [0.0, 0.1, 1.0, 5.0, 10.0, 30.0, 100.0] {
            let risk = RiskSnapshot {
                raw_available_balance: balance,
                position_for_quoting: position,
                worst_case_long: position + 0.025,
                worst_case_short: position - 0.025,
                base_order_size: config.base_order_size,
                max_position: config.max_position,
                inventory_urgency_threshold: config.inventory_urgency_threshold,
                min_available_balance: config.min_available_balance,
                available_balance: balance,
                usable_balance: balance,
                margin_per_eth: 210.0,
                grid_multiplier: 4.0,
            };
            let (bid, ask) =
                InventoryNeutralMM::calculate_asymmetric_sizes_for_config(&config, &risk, 2100.0);
            assert!(bid.is_finite() && ask.is_finite() && bid >= 0.0 && ask >= 0.0);
            assert!(
                (bid + ask) * risk.grid_multiplier * risk.margin_per_eth <= balance + 1e-9,
                "margin exceeded: position={position} balance={balance}, bid={bid} ask={ask}"
            );
            assert!(
                bid * risk.grid_multiplier
                    <= (risk.max_position - risk.worst_case_long).max(0.0) + 1e-9
            );
            assert!(
                ask * risk.grid_multiplier
                    <= (risk.max_position + risk.worst_case_short).max(0.0) + 1e-9
            );
            if position >= config.inventory_urgency_threshold {
                assert_eq!(bid, 0.0);
            }
            if position <= -config.inventory_urgency_threshold {
                assert_eq!(ask, 0.0);
            }
        }
    }
}

#[test]
fn grid_prices_are_tick_aligned_and_every_level_meets_minimum_notional() {
    let config = InventoryNeutralMMConfig {
        tick_size: 0.25,
        grid_levels: 20,
        grid_spacing_bps: 0.1,
        grid_size_decay: 1.0,
        ..test_config()
    };
    for side in [Side::Buy, Side::Sell] {
        let quotes =
            InventoryNeutralMM::build_grid_plan(&config, side, OrderType::PostOnly, 2100.13, 0.006);
        assert!(!quotes.is_empty());
        for quote in &quotes {
            assert!(
                (quote.price / config.tick_size - (quote.price / config.tick_size).round()).abs()
                    < 1e-9
            );
            assert!(quote.price * quote.size >= 12.5 - 1e-9);
        }
        for pair in quotes.windows(2) {
            assert!((pair[0].price - pair[1].price).abs() >= config.tick_size - 1e-9);
        }
    }
    let mut config = config;
    config.grid_spacing_bps = 100.0;
    let quotes =
        InventoryNeutralMM::build_grid_plan(&config, Side::Buy, OrderType::PostOnly, 2100.0, 0.006);
    assert_eq!(
        quotes.len(),
        1,
        "deeper bid would fall below the notional floor"
    );
}

#[test]
fn zero_budget_cancels_fresh_orders_without_waiting_for_ttl() {
    let config = test_config();
    let mut order = active_order(1, 101, OrderSide::Buy, 2100.0, 0.015, 0);
    order.placed_at = Instant::now();
    let plan = execution::build_side_execution_plan(
        &config,
        &[order],
        OrderType::PostOnly,
        Side::Buy,
        2100.0,
        0.0,
        1.0,
        0.25,
        1,
    );
    assert_eq!(plan.to_cancel, vec![101]);
    assert!(plan.to_place.is_empty());
}

#[test]
fn inventory_timeout_survives_repeated_fills_and_failed_flatten_attempts() {
    let now = Instant::now();
    let timeout = Duration::from_secs(60);
    let mut age = InventoryAge::default();
    age.observe(0.02, 0.005, now);
    // These increasing positions model continued same-direction fills.
    for (seconds, position) in [(20, 0.03), (40, 0.06), (61, 0.09)] {
        age.observe(position, 0.005, now + Duration::from_secs(seconds));
    }
    assert!(age.is_expired(now + Duration::from_secs(61), timeout));
    age.last_attempt = Some(now + Duration::from_secs(61));
    assert!(!age.can_retry(now + Duration::from_secs(62)));
    assert!(age.can_retry(now + Duration::from_secs(66)));
    assert!(age.is_expired(now + Duration::from_secs(66), timeout));
    age.observe(0.0, 0.005, now + Duration::from_secs(67));
    assert!(!age.is_expired(now + Duration::from_secs(70), timeout));
    age.observe(-0.02, 0.005, now + Duration::from_secs(71));
    assert!(!age.is_expired(now + Duration::from_secs(72), timeout));
}

#[test]
fn rolling_volatility_matches_reference_across_window_evictions() {
    for window in [0, 2, 3, 17, 200] {
        let mut micro = MicrostructureTracker::new(window, 5, 20);
        let capacity = micro.price_samples.capacity();
        for i in 0..10000 {
            let price = 2100.0 + (i as f64 * 0.017).sin() * 15.0 + (i as f64 * 0.39).cos();
            micro.update(price);
            if micro.price_samples.len() >= 2 {
                let returns: Vec<_> = micro
                    .price_samples
                    .iter()
                    .zip(micro.price_samples.iter().skip(1))
                    .map(|(a, b)| (b / a - 1.0) * 10000.0)
                    .collect();
                let mean = returns.iter().sum::<f64>() / returns.len() as f64;
                let expected = (returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>()
                    / returns.len() as f64)
                    .sqrt()
                    .max(1.0);
                assert!((micro.volatility_bps() - expected).abs() < 1e-7);
            }
        }
        assert_eq!(
            micro.price_samples.capacity(),
            capacity,
            "hot-path ring reallocated"
        );
        let old_price = micro.price_samples.back().copied();
        for invalid in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            micro.update(invalid);
        }
        assert_eq!(micro.price_samples.back().copied(), old_price);
        assert!(micro.momentum_bps().is_finite());
    }
}

#[test]
fn trend_and_dislocation_replay_preserves_quote_edge() {
    use super::pricing::{fee_aware_half_spread, momentum_half_spreads};
    let mut micro = MicrostructureTracker::new(200, 5, 20);
    // Deterministic rising/falling fair-value paths with a lagging venue.
    for direction in [-1.0, 1.0] {
        for i in 0..100 {
            let fair = 2100.0 + direction * i as f64 * 0.1;
            micro.update(fair);
            let (bid_half, ask_half) = momentum_half_spreads(2.0, micro.momentum_bps(), 0.3);
            let raw_bid = fair - bid_half;
            let raw_ask = fair + ask_half;
            let local = fair - direction * 10.0;
            let (bid, ask) = anchor_quotes_to_touch(&AnchorParams {
                raw_bid,
                raw_ask,
                bid_touch: local - 0.05,
                ask_touch: local + 0.05,
                tick_size: 0.01,
                penny_ticks: 1.0,
                inventory_urgency_ratio: 0.0,
            });
            assert!(bid <= raw_bid + 1e-9 && ask >= raw_ask - 1e-9);
            assert!(bid < ask && ask - bid >= 4.0 - 1e-9);
        }
    }
    let (bid, ask) = momentum_half_spreads(2.0, 10.0, 0.3);
    assert!(
        ask > 2.0 && bid < 2.0,
        "upward momentum must protect the ask"
    );
    let (bid, ask) = momentum_half_spreads(2.0, -10.0, 0.3);
    assert!(
        bid > 2.0 && ask < 2.0,
        "downward momentum must protect the bid"
    );
    assert!(fee_aware_half_spread(1.0, 2100.0, 10.0, 8.0).is_none());
    assert_eq!(fee_aware_half_spread(0.0, 2000.0, 10.0, 20.0), Some(1.0));
}

#[test]
#[ignore = "manual release-mode performance benchmark"]
fn benchmark_rolling_volatility_vs_window_scan() {
    use std::hint::black_box;
    let iterations = 200_000;
    let mut micro = MicrostructureTracker::new(200, 5, 20);
    for i in 0..200 {
        micro.update(2100.0 + (i as f64 * 0.1).sin());
    }
    let started = Instant::now();
    for i in 0..iterations {
        micro.update(black_box(2100.0 + (i as f64 * 0.1).sin()));
        black_box(micro.volatility_bps());
    }
    let rolling = started.elapsed();
    // Baseline reproduces the previous full-window scan on every quote cycle.
    let mut prices = micro.price_samples.clone();
    let started = Instant::now();
    for i in 0..iterations {
        prices.pop_front();
        prices.push_back(black_box(2100.0 + (i as f64 * 0.1).sin()));
        let (mut sum, mut squares) = (0.0, 0.0);
        for j in 0..prices.len() - 1 {
            let value = (prices[j + 1] / prices[j] - 1.0) * 10000.0;
            sum += value;
            squares += value * value;
        }
        let n = (prices.len() - 1) as f64;
        black_box((squares / n - (sum / n).powi(2)).sqrt().max(1.0));
    }
    let scan = started.elapsed();
    println!(
        "volatility: {iterations} updates, window=200; rolling={rolling:?}, previous_scan={scan:?}, speedup={:.2}x",
        scan.as_secs_f64() / rolling.as_secs_f64()
    );
}

#[test]
fn account_freshness_requires_independent_equity_and_position_timestamps() {
    let now = wall_time_ns();
    let mut snapshot = AccountStatsSnapshot {
        portfolio_value: 100.0,
        available_balance: 100.0,
        updated_at: now,
        position_updated_at: now,
        ..Default::default()
    };
    assert!(AccountStats::from(snapshot).is_usable());
    for (equity_time, position_time) in [
        (0, now),
        (now, 0),
        (now + 1_000_000_000, now),
        (now, now + 1_000_000_000),
        (now, now - 31_000_000_000),
        (now - 31_000_000_000, now),
    ] {
        snapshot.updated_at = equity_time;
        snapshot.position_updated_at = position_time;
        assert!(!AccountStats::from(snapshot).is_usable());
    }
    assert!(
        !AccountStats::from(AccountStatsSnapshot::default()).is_usable(),
        "a failed SHM read is not a confirmed zero position"
    );
}

#[tokio::test]
async fn cancellation_acknowledgement_does_not_release_unknown_submission() {
    use crate::exchange::{
        BatchOrderParams, BatchOrderResult, BatchResult, OrderInfo, OrderResult,
    };
    struct EmptyBook;
    #[async_trait::async_trait]
    impl Exchange for EmptyBook {
        async fn buy(&self, _: f64, _: f64) -> anyhow::Result<OrderResult> {
            anyhow::bail!("unexpected buy")
        }
        async fn sell(&self, _: f64, _: f64) -> anyhow::Result<OrderResult> {
            anyhow::bail!("unexpected sell")
        }
        async fn place_batch(&self, _: BatchOrderParams) -> anyhow::Result<BatchOrderResult> {
            anyhow::bail!("unexpected place")
        }
        async fn cancel_order(&self, _: i64) -> anyhow::Result<()> {
            Ok(())
        }
        async fn cancel_all(&self) -> anyhow::Result<u32> {
            Ok(0)
        }
        async fn get_active_orders(&self) -> anyhow::Result<Vec<OrderInfo>> {
            Ok(vec![])
        }
        async fn close_all_positions(&self, _: f64) -> anyhow::Result<()> {
            anyhow::bail!("unexpected close")
        }
        async fn execute_batch(&self, _: Vec<BatchAction>) -> anyhow::Result<BatchResult> {
            anyhow::bail!("unexpected batch")
        }
        async fn get_account_stats(&self) -> anyhow::Result<AccountStats> {
            anyhow::bail!("unexpected account query")
        }
        fn limit_order_type(&self) -> OrderType {
            OrderType::PostOnly
        }
    }
    let directory = std::env::temp_dir().join(format!("aleph-strategy-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&directory).unwrap();
    let matrix_path = directory.join("matrix");
    let account_path = directory.join("account");
    std::fs::write(&matrix_path, vec![0u8; 2048 * 8 + 7 * 64]).unwrap();
    std::fs::write(&account_path, [0u8; 128]).unwrap();
    let shm = ShmReader::open(matrix_path.to_str().unwrap(), 1).unwrap();
    let account = AccountStatsReader::open(account_path.to_str().unwrap()).unwrap();
    let tracker = Arc::new(OrderTracker::new());
    tracker.start_tracking(1, OrderSide::Buy, 2100.0, 0.05);
    tracker.mark_submission_uncertain(1);
    let mut strategy = InventoryNeutralMM::new(
        test_config(),
        Arc::new(EmptyBook),
        Arc::clone(&tracker),
        shm,
        account,
    );
    assert!(
        !strategy
            .cancel_all_and_sync("test uncertain cancellation")
            .await
    );
    assert!((tracker.worst_case_long() - 0.05).abs() < 1e-9);
    assert_eq!(tracker.active_orders_snapshot().len(), 1);
    drop(strategy);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn time_scaled_sigma_is_independent_of_sampling_interval() {
    // A random walk with sigma = 2e-4 per sqrt(second), observed at 100 ms and
    // at 1 s, must yield the same A-S sigma; the per-sample figure would differ
    // by sqrt(10).
    let target = 2e-4;
    let estimate = |dt_ms: u64| {
        let mut vol = TimeScaledVolatility::new();
        let start = Instant::now();
        let step = target * (dt_ms as f64 / 1000.0).sqrt();
        let mut mid = 2000.0;
        for i in 0..5_000u64 {
            vol.update(mid, start + Duration::from_millis(i * dt_ms));
            mid *= if i % 2 == 0 {
                step.exp()
            } else {
                (-step).exp()
            };
        }
        vol.sigma().expect("warm estimate")
    };
    for dt_ms in [100, 250, 1000] {
        let sigma = estimate(dt_ms);
        assert!(
            (sigma - target).abs() / target < 0.01,
            "dt={dt_ms}ms sigma={sigma}"
        );
    }
}

#[test]
fn time_scaled_sigma_ignores_invalid_and_stalled_samples() {
    let mut vol = TimeScaledVolatility::new();
    let start = Instant::now();
    assert!(vol.sigma().is_none());
    vol.update(2000.0, start);
    for invalid in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        vol.update(invalid, start + Duration::from_secs(1));
    }
    assert!(vol.sigma().is_none());
    // A 60 s gap restarts the baseline instead of booking one huge return.
    vol.update(2100.0, start + Duration::from_secs(60));
    assert!(vol.sigma().is_none());
    // Sub-10 ms updates are merged rather than divided by a tiny dt.
    vol.update(2100.5, start + Duration::from_millis(60_001));
    assert!(vol.sigma().is_none());
}

#[test]
fn cycle_wake_respects_floor_ceiling_and_events() {
    let ms = Duration::from_millis;
    let (min, max) = (ms(50), ms(250));
    // Never earlier than the floor, even with fresh data.
    assert!(!cycle_wake_due(ms(10), min, max, true));
    // After the floor, data or fills wake the loop immediately.
    assert!(cycle_wake_due(ms(50), min, max, true));
    assert!(!cycle_wake_due(ms(120), min, max, false));
    // Without events the old fixed interval still applies.
    assert!(cycle_wake_due(ms(250), min, max, false));
}

/// Offline strategy over temp SHM files for timing tests. Symbol 0 so the
/// one-symbol matrix covers it; the exchange is never called.
fn wake_test_strategy(
    poll_ms: u64,
    min_ms: u64,
) -> (InventoryNeutralMM, Arc<OrderTracker>, std::path::PathBuf) {
    use crate::exchange::{
        BatchOrderParams, BatchOrderResult, BatchResult, OrderInfo, OrderResult,
    };
    struct Offline;
    #[async_trait::async_trait]
    impl Exchange for Offline {
        async fn buy(&self, _: f64, _: f64) -> anyhow::Result<OrderResult> {
            anyhow::bail!("offline")
        }
        async fn sell(&self, _: f64, _: f64) -> anyhow::Result<OrderResult> {
            anyhow::bail!("offline")
        }
        async fn place_batch(&self, _: BatchOrderParams) -> anyhow::Result<BatchOrderResult> {
            anyhow::bail!("offline")
        }
        async fn cancel_order(&self, _: i64) -> anyhow::Result<()> {
            anyhow::bail!("offline")
        }
        async fn cancel_all(&self) -> anyhow::Result<u32> {
            anyhow::bail!("offline")
        }
        async fn get_active_orders(&self) -> anyhow::Result<Vec<OrderInfo>> {
            anyhow::bail!("offline")
        }
        async fn close_all_positions(&self, _: f64) -> anyhow::Result<()> {
            anyhow::bail!("offline")
        }
        async fn execute_batch(&self, _: Vec<BatchAction>) -> anyhow::Result<BatchResult> {
            anyhow::bail!("offline")
        }
        async fn get_account_stats(&self) -> anyhow::Result<AccountStats> {
            anyhow::bail!("offline")
        }
        fn limit_order_type(&self) -> OrderType {
            OrderType::PostOnly
        }
    }
    let directory = std::env::temp_dir().join(format!("aleph-wake-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&directory).unwrap();
    let matrix_path = directory.join("matrix");
    let account_path = directory.join("account");
    std::fs::write(&matrix_path, vec![0u8; 2048 * 8 + 7 * 64]).unwrap();
    std::fs::write(&account_path, [0u8; 128]).unwrap();
    let shm = ShmReader::open(matrix_path.to_str().unwrap(), 1).unwrap();
    let account = AccountStatsReader::open(account_path.to_str().unwrap()).unwrap();
    let tracker = Arc::new(OrderTracker::new());
    let config = InventoryNeutralMMConfig {
        symbol_id: 0,
        poll_interval_ms: poll_ms,
        min_cycle_interval_ms: min_ms,
        ..test_config()
    };
    let strategy = InventoryNeutralMM::new(
        config,
        Arc::new(Offline),
        Arc::clone(&tracker),
        shm,
        account,
    );
    (strategy, tracker, directory)
}

/// Apply one owned fill to the tracker (client order `coid`).
fn apply_test_fill(tracker: &OrderTracker, coid: i64, sequence: u64) {
    use crate::types::ShmPrivateEventV2;
    tracker.start_tracking(coid, OrderSide::Buy, 2000.0, 0.01);
    let exchange_id = 10_000 + coid as u64;
    let created = ShmPrivateEventV2::order_created(
        sequence,
        2,
        0,
        exchange_id,
        coid,
        coid,
        2000.0,
        0.01,
        false,
        0,
    );
    tracker.apply_event(&created).unwrap();
    let fill = ShmPrivateEventV2::order_filled(
        sequence + 1,
        2,
        0,
        exchange_id,
        coid,
        coid,
        2000.0,
        0.01,
        0.0,
        0.0,
        false,
        0,
        90_000 + coid as u64,
    );
    tracker.apply_event(&fill).unwrap();
}

/// Bump the symbol-0 SHM version, as the feeder does after a BBO write.
fn bump_bbo_version(directory: &std::path::Path) {
    use std::io::{Read, Seek, SeekFrom, Write};
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(directory.join("matrix"))
        .unwrap();
    let mut version = [0u8; 8];
    file.read_exact(&mut version).unwrap();
    let next = u64::from_le_bytes(version) + 1;
    file.seek(SeekFrom::Start(0)).unwrap();
    file.write_all(&next.to_le_bytes()).unwrap();
}

#[tokio::test(start_paused = true)]
async fn fill_between_inventory_read_and_wait_wakes_at_the_floor() {
    let (mut strategy, tracker, directory) = wake_test_strategy(250, 50);
    // Cycle start: baseline captured before reading inventory.
    strategy.cycle_fill_baseline = tracker.total_fill_stats().0;
    // A fill lands after this cycle consumed inventory but before it waits.
    apply_test_fill(&tracker, 1, 1);
    let start = tokio::time::Instant::now();
    strategy.wait_for_next_cycle(None).await;
    // With the old wait-entry baseline this took the full 250 ms poll.
    assert_eq!(start.elapsed(), Duration::from_millis(50));
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test(start_paused = true)]
async fn quiet_market_waits_for_the_poll_interval() {
    let (mut strategy, tracker, directory) = wake_test_strategy(250, 50);
    strategy.cycle_fill_baseline = tracker.total_fill_stats().0;
    let start = tokio::time::Instant::now();
    strategy.wait_for_next_cycle(None).await;
    assert_eq!(start.elapsed(), Duration::from_millis(250));
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test(start_paused = true)]
async fn continuous_bbo_updates_never_beat_the_minimum_cycle() {
    let (mut strategy, tracker, directory) = wake_test_strategy(250, 50);
    strategy.cycle_fill_baseline = tracker.total_fill_stats().0;
    bump_bbo_version(&directory); // already pending at wait entry
    let start = tokio::time::Instant::now();
    strategy.wait_for_next_cycle(None).await;
    assert_eq!(start.elapsed(), Duration::from_millis(50));
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test(start_paused = true)]
async fn bbo_update_mid_wait_wakes_within_one_check_interval() {
    let (mut strategy, tracker, directory) = wake_test_strategy(250, 50);
    strategy.cycle_fill_baseline = tracker.total_fill_stats().0;
    let start = tokio::time::Instant::now();
    let wait = strategy.wait_for_next_cycle(None);
    let bump = async {
        tokio::time::sleep(Duration::from_millis(120)).await;
        bump_bbo_version(&directory);
    };
    tokio::join!(wait, bump);
    let elapsed = start.elapsed();
    assert!(
        elapsed >= Duration::from_millis(120) && elapsed <= Duration::from_millis(125),
        "woke after {elapsed:?}"
    );
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test(start_paused = true)]
async fn shutdown_wakes_after_the_floor() {
    let (mut strategy, tracker, directory) = wake_test_strategy(250, 50);
    strategy.cycle_fill_baseline = tracker.total_fill_stats().0;
    let (stop, shutdown) = tokio::sync::watch::channel(false);
    stop.send(true).unwrap();
    let start = tokio::time::Instant::now();
    strategy.wait_for_next_cycle(Some(&shutdown)).await;
    assert_eq!(start.elapsed(), Duration::from_millis(50));
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
#[ignore = "manual release-mode performance benchmark"]
fn benchmark_wake_check_cost() {
    // One wake check = SHM version load + local version + tracker read lock.
    // At WAKE_CHECK_INTERVAL (5 ms) this bounds the idle polling CPU cost.
    use std::hint::black_box;
    let (mut strategy, tracker, directory) = wake_test_strategy(250, 50);
    for coid in 1..=32 {
        apply_test_fill(&tracker, coid, coid as u64 * 2);
    }
    strategy.cycle_fill_baseline = tracker.total_fill_stats().0;
    let symbol = strategy.config.symbol_id;
    let iterations = 2_000_000u32;
    let start = Instant::now();
    let mut changed = 0u32;
    for _ in 0..iterations {
        let c = strategy.shm_reader.shared_version(symbol)
            != strategy.shm_reader.local_version(symbol)
            || black_box(&tracker).total_fill_stats().0 != strategy.cycle_fill_baseline;
        changed += u32::from(black_box(c));
    }
    let per_check = start.elapsed() / iterations;
    let checks_per_sec = 1.0 / WAKE_CHECK_INTERVAL.as_secs_f64();
    println!(
        "wake check: {per_check:?}/check; idle CPU at {checks_per_sec:.0} checks/s ≈ {:.4}% of one core (changed={changed})",
        per_check.as_secs_f64() * checks_per_sec * 100.0
    );
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test(start_paused = true)]
async fn wake_latency_distribution_vs_fixed_sleep() {
    // Deterministic virtual-time model of the pacing layer only: a BBO lands
    // at a pseudo-random offset after the previous cycle ended (cycle work is
    // treated as instantaneous). Network, signing and order I/O are excluded,
    // so these are scheduler latencies, not end-to-end guarantees.
    let (mut strategy, tracker, directory) = wake_test_strategy(250, 50);
    let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
    let mut event_driven = Vec::new();
    for _ in 0..400 {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let arrival = Duration::from_micros(seed % 250_000);
        strategy.cycle_fill_baseline = tracker.total_fill_stats().0;
        let begin = tokio::time::Instant::now();
        let wait = strategy.wait_for_next_cycle(None);
        let bump = async {
            tokio::time::sleep(arrival).await;
            bump_bbo_version(&directory);
        };
        tokio::join!(wait, bump);
        let woke = begin.elapsed();
        event_driven.push(woke.saturating_sub(arrival));
        // Consume the update as the next cycle's read_all_exchanges would.
        let _ = strategy.shm_reader.read_all_exchanges(0);
        // Fixed 250 ms sleep would have reacted at 250 ms.
    }
    let fixed: Vec<Duration> = {
        let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
        (0..400)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                Duration::from_millis(250) - Duration::from_micros(seed % 250_000)
            })
            .collect()
    };
    let pct = |samples: &[Duration], p: f64| {
        let mut sorted = samples.to_vec();
        sorted.sort_unstable();
        sorted[((sorted.len() - 1) as f64 * p).round() as usize]
    };
    let (e50, e95, e99) = (
        pct(&event_driven, 0.5),
        pct(&event_driven, 0.95),
        pct(&event_driven, 0.99),
    );
    let (f50, f95, f99) = (pct(&fixed, 0.5), pct(&fixed, 0.95), pct(&fixed, 0.99));
    println!(
        "pacing latency event-driven p50={e50:?} p95={e95:?} p99={e99:?}; fixed-250ms p50={f50:?} p95={f95:?} p99={f99:?}"
    );
    // Bounded by the 50 ms floor plus one 5 ms check interval.
    assert!(e99 <= Duration::from_millis(55), "p99={e99:?}");
    assert!(e50 < f50);
    std::fs::remove_dir_all(directory).unwrap();
}
