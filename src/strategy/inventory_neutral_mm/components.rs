use super::ActiveOrder;
use crate::order_tracker::OrderLifecycle;
use crate::config::InventoryNeutralMMConfig;
use crate::exchange::{OrderParams, OrderType, Side};
use std::time::Duration;
use tracing::{debug, warn};

pub(super) const FLOAT_EPSILON: f64 = 1e-9;
const TARGET_RESTING_MARGIN_UTILIZATION: f64 = 0.35;

#[derive(Debug, Clone)]
pub(super) struct RiskSnapshot {
    pub raw_available_balance: f64,
    pub position_for_quoting: f64,
    pub worst_case_long: f64,
    pub worst_case_short: f64,
    pub base_order_size: f64,
    pub max_position: f64,
    pub inventory_urgency_threshold: f64,
    pub min_available_balance: f64,
    pub available_balance: f64,
    pub usable_balance: f64,
    pub margin_per_eth: f64,
    pub grid_multiplier: f64,
}

#[derive(Debug, Clone)]
pub(super) struct QuoteTarget {
    pub bid_price: f64,
    pub ask_price: f64,
    pub bid_size: f64,
    pub ask_size: f64,
}

#[derive(Debug, Clone)]
pub(super) struct ExecutionPlan {
    pub requote_threshold: f64,
    pub target: QuoteTarget,
}

#[derive(Debug, Clone)]
pub(super) enum QuoteCycleDecision {
    Skip,
    ClearForLowMargin,
    FlattenForLowMargin,
    Execute(ExecutionPlan),
}

pub(super) fn round_down_to_step(value: f64, step: f64) -> f64 {
    (((value / step) + FLOAT_EPSILON).floor()) * step
}

pub(super) fn round_up_to_step(value: f64, step: f64) -> f64 {
    (((value / step) - FLOAT_EPSILON).ceil()) * step
}

pub(super) fn min_quotable_size(config: &InventoryNeutralMMConfig, price: f64) -> f64 {
    round_up_to_step(11.0 / price.max(config.tick_size), config.step_size)
        .max(config.step_size)
        + config.step_size
}

pub(super) fn safe_available_balance(
    available_balance: f64,
    portfolio_value: f64,
    margin_usage: f64,
    max_leverage: f64,
) -> f64 {
    if margin_usage > 0.01 && portfolio_value > 0.0 && max_leverage > 0.0 {
        let true_free = portfolio_value * (1.0 - margin_usage / max_leverage);
        available_balance.min(true_free).max(0.0)
    } else {
        available_balance.max(0.0)
    }
}

pub(super) fn residual_exposure_abs(exchange_position: f64, effective_position: f64) -> f64 {
    exchange_position.abs().max(effective_position.abs())
}

pub(super) fn inventory_deadband_size(
    config: &InventoryNeutralMMConfig,
    base_order_size: f64,
    urgency_threshold: f64,
    mid: f64,
) -> f64 {
    let min_deadband_from_notional = if mid > 0.0 && config.min_inventory_notional_usd > 0.0 {
        config.min_inventory_notional_usd / mid
    } else {
        0.0
    };
    min_deadband_from_notional
        .max(config.step_size)
        .max(base_order_size * 0.35)
        .min(urgency_threshold * 0.5)
}

pub(super) fn portfolio_scale(config: &InventoryNeutralMMConfig, portfolio_value: f64) -> f64 {
    let reference = config.reference_portfolio_value.max(1.0);
    let raw = if portfolio_value.is_finite() && portfolio_value > 0.0 {
        portfolio_value / reference
    } else {
        1.0
    };
    raw.clamp(
        config.min_portfolio_scale.max(FLOAT_EPSILON),
        config.max_portfolio_scale.max(1.0),
    )
}

pub(super) fn scaled_base_order_size(
    config: &InventoryNeutralMMConfig,
    portfolio_value: f64,
    mid: f64,
) -> f64 {
    if mid <= 0.0 {
        return config.base_order_size;
    }
    if config.base_order_notional_usd > 0.0 {
        return (config.base_order_notional_usd * portfolio_scale(config, portfolio_value)) / mid;
    }
    config.base_order_size * portfolio_scale(config, portfolio_value)
}

pub(super) fn scaled_max_position(
    config: &InventoryNeutralMMConfig,
    portfolio_value: f64,
    mid: f64,
) -> f64 {
    if mid <= 0.0 {
        return config.max_position;
    }
    if config.max_position_notional_usd > 0.0 {
        return (config.max_position_notional_usd * portfolio_scale(config, portfolio_value)) / mid;
    }
    config.max_position * portfolio_scale(config, portfolio_value)
}

pub(super) fn scaled_inventory_urgency_threshold(
    config: &InventoryNeutralMMConfig,
    portfolio_value: f64,
    mid: f64,
    max_position: f64,
) -> f64 {
    if mid > 0.0 && config.inventory_urgency_notional_usd > 0.0 {
        return ((config.inventory_urgency_notional_usd * portfolio_scale(config, portfolio_value)) / mid)
            .min(max_position)
            .max(config.step_size);
    }

    (config.inventory_urgency_threshold * portfolio_scale(config, portfolio_value))
        .min(max_position)
        .max(config.step_size)
}

pub(super) fn scaled_min_available_balance(
    config: &InventoryNeutralMMConfig,
    portfolio_value: f64,
) -> f64 {
    config.min_available_balance * portfolio_scale(config, portfolio_value)
}

pub(super) fn utilization_floor_base_order_size(
    config: &InventoryNeutralMMConfig,
    base_order_size: f64,
    usable_balance: f64,
    grid_multiplier: f64,
    mid: f64,
) -> f64 {
    if usable_balance <= 0.0 || mid <= 0.0 || config.max_leverage <= 0.0 {
        return base_order_size;
    }

    let top_level_notional_floor = usable_balance
        * config.max_leverage
        * TARGET_RESTING_MARGIN_UTILIZATION
        / (2.0 * grid_multiplier.max(1.0));
    base_order_size.max(top_level_notional_floor / mid)
}

pub(super) fn usable_balance_fraction(position_ratio: f64, margin_usage: f64) -> f64 {
    let inventory_penalty = 0.20 * position_ratio.abs().clamp(0.0, 1.0);
    let margin_penalty = 0.15 * margin_usage.clamp(0.0, 1.0);
    (0.90 - inventory_penalty - margin_penalty).clamp(0.55, 0.90)
}

pub(super) fn toxicity_size_scale(as_score: f64, threshold: f64) -> f64 {
    if threshold <= FLOAT_EPSILON || as_score <= threshold {
        return 1.0;
    }

    (threshold / as_score).clamp(0.25, 1.0)
}

pub(super) fn toxicity_spread_multiplier(as_score: f64, threshold: f64) -> f64 {
    if threshold <= FLOAT_EPSILON || as_score <= threshold {
        return 1.0;
    }

    let excess = (as_score / threshold - 1.0).clamp(0.0, 2.0);
    1.0 + 0.75 * excess
}

pub(super) fn position_for_quoting(
    config: &InventoryNeutralMMConfig,
    exchange_position: f64,
    tracker_effective: f64,
) -> f64 {
    let exchange_abs = exchange_position.abs();
    let tracker_abs = tracker_effective.abs();
    let drift = (tracker_effective - exchange_position).abs();

    if exchange_abs < config.step_size && drift <= config.max_position * 0.25 {
        return tracker_effective;
    }

    if exchange_abs >= config.step_size
        && tracker_abs >= config.step_size
        && exchange_position.signum() != tracker_effective.signum()
    {
        return exchange_position;
    }

    if drift <= config.max_position * 0.25 {
        if exchange_position.signum() == tracker_effective.signum() {
            if tracker_abs >= exchange_abs {
                tracker_effective
            } else {
                exchange_position
            }
        } else {
            exchange_position
        }
    } else {
        exchange_position
    }
}

pub(super) fn inventory_skew_ratio(config: &InventoryNeutralMMConfig, position: f64) -> f64 {
    let threshold = config
        .inventory_urgency_threshold
        .max(config.step_size)
        .max(FLOAT_EPSILON);
    (position / threshold).clamp(-1.0, 1.0)
}

pub(super) fn build_grid_plan(
    config: &InventoryNeutralMMConfig,
    side: Side,
    order_type: OrderType,
    start_px: f64,
    top_level_sz: f64,
) -> Vec<OrderParams> {
    let min_level_size = min_quotable_size(config, start_px);
    if top_level_sz + FLOAT_EPSILON < min_level_size {
        return Vec::new();
    }

    let mut current_px = start_px;
    let mut quotes = Vec::with_capacity(usize::from(config.grid_levels.max(1)));
    let spacing = config.grid_spacing_bps / 10000.0 * start_px;

    for level in 0..config.grid_levels.max(1) {
        let weight = config.grid_size_decay.powi(level as i32);
        let level_sz = round_down_to_step(top_level_sz * weight, config.step_size);
        if level_sz + FLOAT_EPSILON < min_level_size {
            break;
        }

        quotes.push(OrderParams {
            size: level_sz,
            price: current_px,
            side,
            order_type,
            reduce_only: false,
        });

        if side == Side::Buy {
            current_px -= spacing;
        } else {
            current_px += spacing;
        }
    }

    quotes
}

pub(super) fn reconcile_side_plan(
    existing_orders: &[ActiveOrder],
    desired_quotes: &[OrderParams],
    threshold: f64,
    step_size: f64,
    min_lifetime: Duration,
) -> (Vec<i64>, Vec<OrderParams>) {
    let mut matched_existing = vec![false; existing_orders.len()];
    let mut to_place = Vec::new();

    for desired in desired_quotes {
        if let Some((idx, _)) = existing_orders
            .iter()
            .enumerate()
            .filter(|(idx, _)| !matched_existing[*idx])
            .find(|(_, order)| {
                let size_tolerance = (desired.size * 0.08).max(step_size);
                (order.price - desired.price).abs() <= threshold
                    && (order.size - desired.size).abs() <= size_tolerance
            })
        {
            matched_existing[idx] = true;
        } else {
            to_place.push(desired.clone());
        }
    }

    let to_cancel: Vec<i64> = existing_orders
        .iter()
        .enumerate()
        .filter(|(idx, order)| !matched_existing[*idx] && order.placed_at.elapsed() >= min_lifetime)
        .filter(|(_, order)| {
            order.order_index.is_some() && order.lifecycle != OrderLifecycle::PendingCancel
        })
        .filter_map(|(_, order)| order.order_index)
        .collect();

    let remaining_existing = existing_orders.len().saturating_sub(to_cancel.len());
    let placement_capacity = desired_quotes.len().saturating_sub(remaining_existing);
    if to_place.len() > placement_capacity {
        to_place.truncate(placement_capacity);
    }

    (to_cancel, to_place)
}

pub(super) fn apply_risk_limits(
    config: &InventoryNeutralMMConfig,
    risk: &RiskSnapshot,
    desired_bid_size: f64,
    desired_ask_size: f64,
    mid: f64,
) -> (f64, f64) {
    let min_size = min_quotable_size(config, mid);
    let grid_multiplier = risk.grid_multiplier.max(1.0);

    let bid_headroom = (risk.max_position - risk.worst_case_long).max(0.0);
    let ask_headroom = (risk.worst_case_short + risk.max_position).max(0.0);
    let bid_top_headroom = bid_headroom / grid_multiplier;
    let ask_top_headroom = ask_headroom / grid_multiplier;
    let bid_size = desired_bid_size.min(bid_top_headroom);
    let ask_size = desired_ask_size.min(ask_top_headroom);

    let bid_margin_required = bid_size * risk.margin_per_eth * grid_multiplier;
    let ask_margin_required = ask_size * risk.margin_per_eth * grid_multiplier;
    let total_margin_required = bid_margin_required + ask_margin_required;

    let (bid_size, ask_size) =
        if total_margin_required > risk.usable_balance && total_margin_required > 0.0 {
            let scale_factor = (risk.usable_balance / total_margin_required).min(1.0);
            if scale_factor < 0.1 {
                warn!(
                    "Insufficient margin: available=${:.2} required=${:.2} (scale={:.1}%), skipping quotes",
                    risk.available_balance,
                    total_margin_required,
                    scale_factor * 100.0
                );
                (0.0, 0.0)
            } else {
                debug!(
                    "Margin constraint: scaled orders by {:.1}% (available=${:.2})",
                    scale_factor * 100.0,
                    risk.available_balance
                );
                (bid_size * scale_factor, ask_size * scale_factor)
            }
        } else {
            (bid_size, ask_size)
        };

    let hard_cap = risk.base_order_size * config.flattening_cap_mult;
    let mut bid_size = bid_size.min(hard_cap);
    let mut ask_size = ask_size.min(hard_cap);

    let inventory_deadband = inventory_deadband_size(
        config,
        risk.base_order_size,
        risk.inventory_urgency_threshold.max(config.step_size),
        mid,
    );
    let urgency_threshold = risk.inventory_urgency_threshold.max(config.step_size);
    let urgency_ratio = (risk.position_for_quoting / urgency_threshold).clamp(-1.0, 1.0);
    let bias_progress = if risk.position_for_quoting.abs() <= inventory_deadband {
        0.0
    } else {
        ((risk.position_for_quoting.abs() - inventory_deadband)
            / (urgency_threshold - inventory_deadband).max(config.step_size))
            .clamp(0.0, 1.0)
    };
    let soft_same_side_floor = round_down_to_step(
        (risk.base_order_size * (1.0 - 0.60 * bias_progress))
            .max(min_size + config.step_size)
            .max(config.step_size),
        config.step_size,
    );
    let hard_keepalive_size = round_down_to_step(
        (risk.base_order_size * (0.15 * (1.0 - urgency_ratio.abs()) + 0.05))
            .max(config.step_size)
            .max(min_size + config.step_size),
        config.step_size,
    );
    if risk.position_for_quoting.abs() <= inventory_deadband {
        bid_size = bid_size.max(risk.base_order_size.min(hard_cap).min(bid_top_headroom));
        ask_size = ask_size.max(risk.base_order_size.min(hard_cap).min(ask_top_headroom));
    } else if urgency_ratio > 0.0 {
        // Long inventory: bias toward asks. Stay near-symmetric until urgency, then fall back to keepalive.
        let pre_urgency = bias_progress < 1.0;
        let same_side_floor = if pre_urgency {
            soft_same_side_floor
        } else {
            hard_keepalive_size
        };
        bid_size = bid_size.min(same_side_floor);
        if pre_urgency {
            let flatten_boost = 1.0 + 0.35 * bias_progress;
            ask_size = (ask_size * flatten_boost)
                .min(hard_cap)
                .min(ask_top_headroom);
        } else {
            ask_size = ask_size.max(risk.base_order_size.min(hard_cap).min(ask_top_headroom));
        }
    } else if urgency_ratio < 0.0 {
        // Short inventory: bias toward bids. Stay near-symmetric until urgency, then fall back to keepalive.
        let pre_urgency = bias_progress < 1.0;
        let same_side_floor = if pre_urgency {
            soft_same_side_floor
        } else {
            hard_keepalive_size
        };
        ask_size = ask_size.min(same_side_floor);
        if pre_urgency {
            let flatten_boost = 1.0 + 0.35 * bias_progress;
            bid_size = (bid_size * flatten_boost)
                .min(hard_cap)
                .min(bid_top_headroom);
        } else {
            bid_size = bid_size.max(risk.base_order_size.min(hard_cap).min(bid_top_headroom));
        }
    }

    let bid_size = if bid_size + FLOAT_EPSILON < min_size {
        0.0
    } else {
        round_down_to_step(bid_size, config.step_size)
    };
    let ask_size = if ask_size + FLOAT_EPSILON < min_size {
        0.0
    } else {
        round_down_to_step(ask_size, config.step_size)
    };

    (bid_size, ask_size)
}

pub(super) fn build_execution_plan(
    config: &InventoryNeutralMMConfig,
    target: QuoteTarget,
    mid: f64,
) -> ExecutionPlan {
    let requote_threshold = (mid * config.requote_threshold_bps / 10000.0)
        .max(mid * config.grid_spacing_bps / 20000.0);
    ExecutionPlan {
        requote_threshold,
        target,
    }
}

pub(super) fn decide_quote_cycle(
    config: &InventoryNeutralMMConfig,
    target: QuoteTarget,
    mid: f64,
    available_balance: f64,
    position_abs: f64,
) -> QuoteCycleDecision {
    let min_quotable_size = config.step_size.max(0.0);
    if target.bid_size < min_quotable_size && target.ask_size < min_quotable_size {
        if available_balance < config.min_available_balance {
            if position_abs >= config.step_size {
                QuoteCycleDecision::FlattenForLowMargin
            } else {
                QuoteCycleDecision::ClearForLowMargin
            }
        } else {
            QuoteCycleDecision::Skip
        }
    } else {
        QuoteCycleDecision::Execute(build_execution_plan(config, target, mid))
    }
}
