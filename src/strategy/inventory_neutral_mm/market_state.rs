use crate::shm_reader::{NUM_EXCHANGES, ShmBboMessage};

#[derive(Debug, Clone, Copy)]
pub(super) struct MarketState {
    pub exchanges: [(u8, ShmBboMessage); NUM_EXCHANGES],
    pub bbo: ShmBboMessage,
}

pub(super) fn select_exchange_bbo(
    exchanges: &[(u8, ShmBboMessage); NUM_EXCHANGES],
    exchange_id: u8,
) -> Option<ShmBboMessage> {
    exchanges
        .iter()
        .find(|(exch_id, _)| *exch_id == exchange_id)
        .map(|(_, msg)| *msg)
        .filter(|bbo| bbo.bid_price > 0.0 || bbo.ask_price > 0.0)
}

pub(super) fn data_age_ms(timestamp_ns: u64, now_ns: u64) -> u64 {
    now_ns.saturating_sub(timestamp_ns) / 1_000_000
}

pub(super) fn is_stale_bbo(timestamp_ns: u64, now_ns: u64, threshold_ms: u64) -> bool {
    timestamp_ns > 0 && data_age_ms(timestamp_ns, now_ns) > threshold_ms
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StaleBboAction {
    Fresh,
    Freeze,
    Cancel,
}

fn has_valid_two_sided_book(bbo: &ShmBboMessage) -> bool {
    bbo.bid_price > 0.0 && bbo.ask_price > 0.0 && bbo.ask_price > bbo.bid_price
}

pub(super) fn classify_stale_bbo(
    bbo: &ShmBboMessage,
    now_ns: u64,
    first_stale_seen_ns: Option<u64>,
    freeze_threshold_ms: u64,
    cancel_after_ms: u64,
    static_two_sided_grace_ms: u64,
) -> StaleBboAction {
    let timestamp_ns = bbo.timestamp_ns;
    if !is_stale_bbo(timestamp_ns, now_ns, freeze_threshold_ms) {
        return StaleBboAction::Fresh;
    }

    if has_valid_two_sided_book(bbo) && data_age_ms(timestamp_ns, now_ns) <= static_two_sided_grace_ms {
        return StaleBboAction::Fresh;
    }

    let stale_origin_ns = first_stale_seen_ns.unwrap_or(now_ns);
    let stale_for_ms = data_age_ms(stale_origin_ns, now_ns);
    if stale_for_ms >= cancel_after_ms {
        StaleBboAction::Cancel
    } else {
        StaleBboAction::Freeze
    }
}

pub(super) fn build_market_state(
    exchanges: [(u8, ShmBboMessage); NUM_EXCHANGES],
    exchange_id: u8,
) -> Option<MarketState> {
    let bbo = select_exchange_bbo(&exchanges, exchange_id)?;
    Some(MarketState { exchanges, bbo })
}

pub(super) fn external_reference_mid(
    exchanges: &[(u8, ShmBboMessage); NUM_EXCHANGES],
    local_exchange_id: u8,
    now_ns: u64,
    stale_threshold_ms: u64,
) -> Option<f64> {
    let mut mids: Vec<f64> = exchanges
        .iter()
        .filter(|(exchange_id, _)| *exchange_id != local_exchange_id && *exchange_id != 0)
        .map(|(_, bbo)| *bbo)
        .filter(|bbo| {
            bbo.bid_price > 0.0
                && bbo.ask_price > 0.0
                && bbo.ask_price > bbo.bid_price
                && !is_stale_bbo(bbo.timestamp_ns, now_ns, stale_threshold_ms)
        })
        .map(|bbo| (bbo.bid_price + bbo.ask_price) / 2.0)
        .collect();

    if mids.is_empty() {
        None
    } else {
        mids.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mid_idx = mids.len() / 2;
        if mids.len() % 2 == 1 {
            Some(mids[mid_idx])
        } else {
            Some((mids[mid_idx - 1] + mids[mid_idx]) / 2.0)
        }
    }
}

pub(super) fn cross_exchange_offset_bps(
    local_mid: f64,
    external_mid: f64,
    threshold_bps: f64,
    scale: f64,
    max_overlay_bps: f64,
    sanity_band_bps: f64,
) -> f64 {
    if local_mid <= 0.0
        || external_mid <= 0.0
        || threshold_bps <= 0.0
        || scale <= 0.0
        || max_overlay_bps <= 0.0
        || sanity_band_bps <= 0.0
    {
        return 0.0;
    }

    let raw_delta_bps = (external_mid - local_mid) / local_mid * 10000.0;
    if raw_delta_bps.abs() > sanity_band_bps {
        return 0.0;
    }
    if raw_delta_bps.abs() < threshold_bps {
        return 0.0;
    }

    (raw_delta_bps * scale).clamp(-max_overlay_bps, max_overlay_bps)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(bid_price: f64, ask_price: f64, timestamp_ns: u64) -> ShmBboMessage {
        ShmBboMessage {
            seqlock: 0,
            msg_type: 0,
            exchange_id: 0,
            symbol_id: 0,
            timestamp_ns,
            bid_price,
            bid_size: 1.0,
            ask_price,
            ask_size: 1.0,
            _reserved: [0; 16],
        }
    }

    #[test]
    fn build_market_state_selects_local_bbo_when_one_side_exists() {
        let exchanges = [
            (0, ShmBboMessage::default()),
            (1, msg(2100.0, 0.0, 1)),
            (2, ShmBboMessage::default()),
            (3, ShmBboMessage::default()),
            (4, ShmBboMessage::default()),
            (5, ShmBboMessage::default()),
        ];

        let state = build_market_state(exchanges, 1).expect("local bbo should exist");

        assert_eq!(state.bbo.bid_price, 2100.0);
        assert_eq!(state.bbo.ask_price, 0.0);
    }

    #[test]
    fn is_stale_bbo_respects_threshold() {
        let now_ns = 10_000_000_000;
        assert!(!is_stale_bbo(9_996_000_000, now_ns, 5));
        assert!(is_stale_bbo(9_994_000_000, now_ns, 5));
    }

    #[test]
    fn classify_stale_bbo_freezes_before_canceling() {
        let now_ns = 10_000_000_000;
        let timestamp_ns = 9_994_000_000;
        let two_sided = msg(2100.0, 2101.0, timestamp_ns);
        let one_sided = msg(2100.0, 0.0, timestamp_ns);

        assert_eq!(
            classify_stale_bbo(&one_sided, now_ns, None, 5, 10_000, 30_000),
            StaleBboAction::Freeze
        );
        assert_eq!(
            classify_stale_bbo(&one_sided, now_ns, Some(0), 5, 10_000, 30_000),
            StaleBboAction::Cancel
        );
        assert_eq!(
            classify_stale_bbo(&msg(2100.0, 2101.0, 9_999_000_000), now_ns, None, 5, 10_000, 30_000),
            StaleBboAction::Fresh
        );
        assert_eq!(
            classify_stale_bbo(&two_sided, now_ns, None, 5, 10_000, 30_000),
            StaleBboAction::Fresh
        );
    }

    #[test]
    fn external_reference_mid_uses_only_fresh_non_local_two_sided_books() {
        let now_ns = 10_000_000_000;
        let exchanges = [
            (0, ShmBboMessage::default()),
            (1, msg(2100.0, 2101.0, 9_999_000_000)),
            (2, msg(2000.0, 2001.0, 9_999_000_000)),
            (3, msg(2200.0, 2201.0, 9_999_000_000)),
            (4, msg(2300.0, 0.0, 9_999_000_000)),
            (5, msg(2400.0, 2401.0, 9_000_000_000)),
        ];

        let external_mid =
            external_reference_mid(&exchanges, 2, now_ns, 500).expect("external mid");

        assert!((external_mid - 2150.5).abs() < 1e-9);
    }

    #[test]
    fn cross_exchange_offset_bps_requires_meaningful_divergence() {
        assert_eq!(cross_exchange_offset_bps(2000.0, 2000.5, 5.0, 0.5, 2.0, 25.0), 0.0);

        let offset = cross_exchange_offset_bps(2000.0, 2004.0, 5.0, 0.5, 2.0, 25.0);
        assert!((offset - 2.0).abs() < 1e-9);
    }

    #[test]
    fn cross_exchange_offset_bps_rejects_external_outliers() {
        let offset = cross_exchange_offset_bps(2000.0, 2020.0, 5.0, 0.5, 2.0, 25.0);
        assert_eq!(offset, 0.0);
    }

    #[test]
    fn external_reference_mid_uses_median_not_mean() {
        let now_ns = 10_000_000_000;
        let exchanges = [
            (0, ShmBboMessage::default()),
            (1, msg(2100.0, 2101.0, 9_999_000_000)),
            (2, msg(2100.0, 2101.0, 9_999_000_000)),
            (3, msg(2500.0, 2501.0, 9_999_000_000)),
            (4, ShmBboMessage::default()),
            (5, ShmBboMessage::default()),
        ];

        let external_mid =
            external_reference_mid(&exchanges, 0, now_ns, 500).expect("external mid");

        assert!((external_mid - 2100.5).abs() < 1e-9);
    }
}
