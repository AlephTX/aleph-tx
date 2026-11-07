use crate::{order_tracker::OrderSide, shm_reader::ShmBboMessage};

pub(super) fn local_reference_mid(
    bbo: &ShmBboMessage,
    tick_size: f64,
    fallback_spread_ticks: f64,
) -> f64 {
    if bbo.bid_price > 0.0 && bbo.ask_price > 0.0 {
        bbo.bid_price * 0.5 + bbo.ask_price * 0.5
    } else if bbo.bid_price > 0.0 {
        bbo.bid_price + (tick_size * fallback_spread_ticks)
    } else if bbo.ask_price > 0.0 {
        bbo.ask_price - (tick_size * fallback_spread_ticks)
    } else {
        0.0
    }
}

pub(super) fn cleanup_reference_mid(
    bbo: &ShmBboMessage,
    tick_size: f64,
    fallback_spread_ticks: f64,
) -> Option<f64> {
    let mid = local_reference_mid(bbo, tick_size, fallback_spread_ticks);
    if mid.is_finite() && mid > 0.0 {
        Some(mid)
    } else {
        None
    }
}

pub(super) fn fallback_bbo_prices(mid: f64, bbo: &ShmBboMessage, tick_size: f64) -> (f64, f64) {
    let bid_price = if bbo.bid_price > 0.0 {
        bbo.bid_price
    } else {
        mid - tick_size
    };
    let ask_price = if bbo.ask_price > 0.0 {
        bbo.ask_price
    } else {
        mid + tick_size
    };
    (bid_price, ask_price)
}

pub(super) fn effective_penny_ticks(
    base_penny_ticks: f64,
    as_score: f64,
    as_threshold: f64,
    inventory_urgency_ratio: f64,
) -> f64 {
    let mut extra_ticks = 0.0;
    if as_threshold > 0.0 && as_score > as_threshold {
        extra_ticks += ((as_score / as_threshold) - 1.0).clamp(0.0, 1.0);
    }
    extra_ticks += inventory_urgency_ratio.abs().clamp(0.0, 1.0) * 0.5;
    (base_penny_ticks + extra_ticks).clamp(1.0, base_penny_ticks.max(1.0) + 1.5)
}

#[allow(dead_code)]
pub(super) fn inventory_adjusted_half_spreads(
    base_half_spread: f64,
    urgency_ratio: f64,
) -> (f64, f64) {
    let flatten_progress = urgency_ratio.abs().clamp(0.0, 1.0);
    if flatten_progress <= f64::EPSILON {
        return (base_half_spread, base_half_spread);
    }

    let tighten = (1.0 - 0.45 * flatten_progress).clamp(0.55, 1.0);
    let widen = (1.0 + 0.35 * flatten_progress).clamp(1.0, 1.35);

    match urgency_ratio.partial_cmp(&0.0) {
        Some(std::cmp::Ordering::Greater) => (base_half_spread * widen, base_half_spread * tighten),
        Some(std::cmp::Ordering::Less) => (base_half_spread * tighten, base_half_spread * widen),
        _ => (base_half_spread, base_half_spread),
    }
}

pub(super) struct AnchorParams {
    pub raw_bid: f64,
    pub raw_ask: f64,
    pub bid_touch: f64,
    pub ask_touch: f64,
    pub tick_size: f64,
    pub penny_ticks: f64,
    pub inventory_urgency_ratio: f64,
}

pub(super) fn anchor_quotes_to_touch(params: &AnchorParams) -> (f64, f64) {
    let join_buffer = (params.penny_ticks.max(1.0) * params.tick_size).max(params.tick_size);
    let flatten_bias = params.inventory_urgency_ratio.abs().clamp(0.0, 1.0) * params.tick_size;
    let bid_join_buffer = if params.inventory_urgency_ratio > 0.0 {
        join_buffer + flatten_bias
    } else {
        (join_buffer - flatten_bias).max(params.tick_size)
    };
    let ask_join_buffer = if params.inventory_urgency_ratio < 0.0 {
        join_buffer + flatten_bias
    } else {
        (join_buffer - flatten_bias).max(params.tick_size)
    };
    let join_bid = if params.ask_touch - params.bid_touch > bid_join_buffer {
        params.ask_touch - bid_join_buffer
    } else {
        params.bid_touch
    };
    let join_ask = if params.ask_touch - params.bid_touch > ask_join_buffer {
        params.bid_touch + ask_join_buffer
    } else {
        params.ask_touch
    };
    // Touch limits may make a quote less aggressive, never more aggressive
    // than its fair-value/fee-aware model price. Pulling distant quotes toward
    // the local book can erase edge when the venue dislocates from the anchor.
    let mut bid = params
        .raw_bid
        .min(join_bid)
        .min(params.ask_touch - params.tick_size);
    let mut ask = params
        .raw_ask
        .max(join_ask)
        .max(params.bid_touch + params.tick_size);

    bid = (bid / params.tick_size).floor() * params.tick_size;
    ask = (ask / params.tick_size).ceil() * params.tick_size;
    (bid, ask)
}

pub(super) fn stabilize_crossed_quotes(
    bid: f64,
    ask: f64,
    bid_touch: f64,
    ask_touch: f64,
    tick_size: f64,
) -> Option<(f64, f64)> {
    if !bid.is_finite()
        || !ask.is_finite()
        || !bid_touch.is_finite()
        || !ask_touch.is_finite()
        || !tick_size.is_finite()
        || tick_size <= 0.0
        || bid <= 0.0
        || ask <= 0.0
        || bid_touch <= 0.0
        || ask_touch <= bid_touch
    {
        return None;
    }

    if bid < ask {
        return Some((bid, ask));
    }

    if ask_touch <= bid_touch {
        return None;
    }

    let safe_bid = (bid_touch / tick_size).floor() * tick_size;
    let safe_ask = (ask_touch / tick_size).ceil() * tick_size;

    if safe_bid < safe_ask {
        Some((safe_bid, safe_ask))
    } else {
        None
    }
}

/// Preserve the fee/profit floor even when volatility would prefer a tighter cap.
pub(super) fn fee_aware_half_spread(
    raw: f64,
    mid: f64,
    fee_floor_bps: f64,
    cap_bps: f64,
) -> Option<f64> {
    if !raw.is_finite()
        || !mid.is_finite()
        || mid <= 0.0
        || !fee_floor_bps.is_finite()
        || !cap_bps.is_finite()
        || fee_floor_bps < 0.0
        || cap_bps < fee_floor_bps
    {
        return None;
    }
    Some(raw.clamp(mid * fee_floor_bps / 20000.0, mid * cap_bps / 20000.0))
}

pub(super) fn momentum_half_spreads(half_spread: f64, momentum_bps: f64, scale: f64) -> (f64, f64) {
    let adjustment = (momentum_bps / 10.0).clamp(-0.5, 0.5) * scale.clamp(0.0, 1.0);
    // Rising fair value exposes the resting ask to informed buyers; widen the
    // ask. Falling fair value symmetrically widens the bid against sellers.
    (
        half_spread * (1.0 - adjustment),
        half_spread * (1.0 + adjustment),
    )
}

#[derive(Clone, Copy)]
pub(super) struct MakerPriceBounds {
    pub max_bid: f64,
    pub min_ask: f64,
}

impl MakerPriceBounds {
    pub fn new(
        sell_proceeds: f64,
        buy_cost: f64,
        maker_fee_bps: f64,
        min_profit_bps: f64,
        tick: f64,
    ) -> Option<Self> {
        // The config's profit target is per round trip. Reserve half on
        // each fill, including fees charged on that fill's own notional.
        let reserve = (maker_fee_bps + min_profit_bps / 2.0) / 10_000.0;
        if !reserve.is_finite()
            || !(0.0..1.0).contains(&reserve)
            || !tick.is_finite()
            || tick <= 0.0
        {
            return None;
        }
        let max_bid = (sell_proceeds / (1.0 + reserve) / tick).floor() * tick;
        let min_ask = (buy_cost / (1.0 - reserve) / tick).ceil() * tick;
        (max_bid.is_finite() && min_ask.is_finite() && max_bid > 0.0 && min_ask >= max_bid)
            .then_some(Self { max_bid, min_ask })
    }

    pub fn allows(self, side: OrderSide, price: f64) -> bool {
        match side {
            OrderSide::Buy => price <= self.max_bid,
            OrderSide::Sell => price >= self.min_ask,
        }
    }
}
