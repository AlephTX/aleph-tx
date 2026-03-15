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

pub(super) fn anchor_quotes_to_touch(
    raw_bid: f64,
    raw_ask: f64,
    bid_touch: f64,
    ask_touch: f64,
    mid: f64,
    tick_size: f64,
    penny_ticks: f64,
    max_touch_offset_bps: f64,
) -> (f64, f64) {
    let join_buffer = (penny_ticks.max(1.0) * tick_size).max(tick_size);
    let join_bid = if ask_touch - bid_touch > join_buffer {
        ask_touch - join_buffer
    } else {
        bid_touch
    };
    let join_ask = if ask_touch - bid_touch > join_buffer {
        bid_touch + join_buffer
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
