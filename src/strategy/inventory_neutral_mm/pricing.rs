use crate::shm_reader::ShmBboMessage;

pub(super) fn local_reference_mid(
    bbo: &ShmBboMessage,
    tick_size: f64,
    fallback_spread_ticks: f64,
) -> f64 {
    if bbo.bid_price > 0.0 && bbo.ask_price > 0.0 {
        (bbo.bid_price + bbo.ask_price) / 2.0
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

pub(super) fn anchor_quotes_to_touch(
    raw_bid: f64,
    raw_ask: f64,
    bid_touch: f64,
    ask_touch: f64,
    mid: f64,
    tick_size: f64,
    penny_ticks: f64,
    inventory_urgency_ratio: f64,
    max_touch_offset_bps: f64,
) -> (f64, f64) {
    let join_buffer = (penny_ticks.max(1.0) * tick_size).max(tick_size);
    let flatten_bias = inventory_urgency_ratio.abs().clamp(0.0, 1.0) * tick_size;
    let bid_join_buffer = if inventory_urgency_ratio > 0.0 {
        join_buffer + flatten_bias
    } else {
        (join_buffer - flatten_bias).max(tick_size)
    };
    let ask_join_buffer = if inventory_urgency_ratio < 0.0 {
        join_buffer + flatten_bias
    } else {
        (join_buffer - flatten_bias).max(tick_size)
    };
    let join_bid = if ask_touch - bid_touch > bid_join_buffer {
        ask_touch - bid_join_buffer
    } else {
        bid_touch
    };
    let join_ask = if ask_touch - bid_touch > ask_join_buffer {
        bid_touch + ask_join_buffer
    } else {
        ask_touch
    };
    let max_touch_offset = mid * max_touch_offset_bps / 10000.0;

    let mut bid = raw_bid.max(join_bid - max_touch_offset);
    let mut ask = raw_ask.min(join_ask + max_touch_offset);

    bid = bid.min(ask_touch - tick_size);
    ask = ask.max(bid_touch + tick_size);

    bid = (bid / tick_size).floor() * tick_size;
    ask = (ask / tick_size).ceil() * tick_size;
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
        || tick_size <= 0.0
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
