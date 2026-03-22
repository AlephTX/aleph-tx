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

fn has_quoteable_book(bbo: &ShmBboMessage) -> bool {
    bbo.bid_price > 0.0 || bbo.ask_price > 0.0
}

pub(super) fn classify_stale_bbo(
    bbo: &ShmBboMessage,
    now_ns: u64,
    first_stale_seen_ns: Option<u64>,
    freeze_threshold_ms: u64,
    cancel_after_ms: u64,
    static_two_sided_grace_ms: u64,
    static_quoteable_grace_ms: u64,
) -> StaleBboAction {
    let timestamp_ns = bbo.timestamp_ns;
    if !is_stale_bbo(timestamp_ns, now_ns, freeze_threshold_ms) {
        return StaleBboAction::Fresh;
    }

    let age_ms = data_age_ms(timestamp_ns, now_ns);
    if has_valid_two_sided_book(bbo) && age_ms <= static_two_sided_grace_ms {
        return StaleBboAction::Fresh;
    }

    if has_quoteable_book(bbo) && age_ms <= static_quoteable_grace_ms {
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

/// Primary fair value anchor from external high-liquidity exchanges.
///
/// Takes ALL non-local exchange BBOs (exchange_id != local AND != 0),
/// filters stale data (> staleness_ms), calculates mid for each valid
/// exchange, and returns the median (robust against single-exchange outlier).
/// Returns None if no valid external data is available.
///
/// v6.0.1: Added DEBUG logging to track staleness and fallback behavior.
pub(super) fn external_fair_value_mid(
    exchanges: &[(u8, ShmBboMessage); NUM_EXCHANGES],
    local_exchange_id: u8,
    now_ns: u64,
    staleness_ms: u64,
) -> Option<f64> {
    let external_exchanges: Vec<(u8, ShmBboMessage)> = exchanges
        .iter()
        .filter(|(exchange_id, _)| *exchange_id != local_exchange_id && *exchange_id != 0)
        .map(|(id, bbo)| (*id, *bbo))
        .collect();

    let mut valid_mids: Vec<(u8, f64, u64)> = Vec::new();
    let mut stale_count = 0;
    let mut invalid_count = 0;

    for (exchange_id, bbo) in external_exchanges {
        let age_ms = data_age_ms(bbo.timestamp_ns, now_ns);

        if bbo.bid_price <= 0.0 || bbo.ask_price <= 0.0 || bbo.ask_price <= bbo.bid_price {
            invalid_count += 1;
            continue;
        }

        if is_stale_bbo(bbo.timestamp_ns, now_ns, staleness_ms) {
            stale_count += 1;
            tracing::debug!(
                "External exchange {} stale: age={}ms (threshold={}ms)",
                exchange_id,
                age_ms,
                staleness_ms
            );
            continue;
        }

        let mid = (bbo.bid_price + bbo.ask_price) / 2.0;
        valid_mids.push((exchange_id, mid, age_ms));
    }

    if valid_mids.is_empty() {
        if stale_count > 0 || invalid_count > 0 {
            tracing::debug!(
                "External fair value unavailable: stale={} invalid={} → fallback to local VWMicro",
                stale_count,
                invalid_count
            );
        }
        None
    } else {
        valid_mids.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        let mid_idx = valid_mids.len() / 2;
        let median = if valid_mids.len() % 2 == 1 {
            valid_mids[mid_idx].1
        } else {
            (valid_mids[mid_idx - 1].1 + valid_mids[mid_idx].1) / 2.0
        };

        tracing::debug!(
            "External fair value: median=${:.2} from {} exchanges: {:?}",
            median,
            valid_mids.len(),
            valid_mids.iter().map(|(id, mid, age)| format!("{}@${:.2}({}ms)", id, mid, age)).collect::<Vec<_>>()
        );

        Some(median)
    }
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

    fn default_exchange() -> (u8, ShmBboMessage) {
        (0, ShmBboMessage::default())
    }

    #[test]
    fn build_market_state_selects_local_bbo_when_one_side_exists() {
        let exchanges = [
            default_exchange(),
            (1, msg(2100.0, 0.0, 1)),
            default_exchange(),
            default_exchange(),
            default_exchange(),
            default_exchange(),
            default_exchange(),
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
        let now_ns = 40_000_000_000;
        let timestamp_ns = 27_000_000_000;
        let two_sided = msg(2100.0, 2101.0, timestamp_ns);
        let one_sided = msg(2100.0, 0.0, timestamp_ns);

        assert_eq!(
            classify_stale_bbo(&one_sided, now_ns, None, 5, 10_000, 30_000, 12_000),
            StaleBboAction::Freeze
        );
        assert_eq!(
            classify_stale_bbo(&one_sided, now_ns, Some(0), 5, 10_000, 30_000, 12_000),
            StaleBboAction::Cancel
        );
        assert_eq!(
            classify_stale_bbo(
                &msg(2100.0, 2101.0, 39_000_000_000),
                now_ns,
                None,
                5,
                10_000,
                30_000,
                12_000,
            ),
            StaleBboAction::Fresh
        );
        assert_eq!(
            classify_stale_bbo(&two_sided, now_ns, None, 5, 10_000, 30_000, 12_000),
            StaleBboAction::Fresh
        );

        let quoteable_one_sided = msg(2100.0, 0.0, 34_000_000_000);
        assert_eq!(
            classify_stale_bbo(&quoteable_one_sided, now_ns, None, 5, 10_000, 30_000, 12_000),
            StaleBboAction::Fresh
        );
    }

    #[test]
    fn external_fair_value_mid_uses_only_fresh_non_local_two_sided_books() {
        let now_ns = 10_000_000_000;
        let exchanges = [
            default_exchange(),
            (1, msg(2100.0, 2101.0, 9_999_000_000)),
            (2, msg(2000.0, 2001.0, 9_999_000_000)),
            (3, msg(2200.0, 2201.0, 9_999_000_000)),
            (4, msg(2300.0, 0.0, 9_999_000_000)),   // one-sided, filtered out
            (5, msg(2400.0, 2401.0, 9_000_000_000)), // stale at 500ms threshold
            default_exchange(),
        ];

        // local=2 excluded; exch 4 one-sided excluded; exch 5 stale excluded
        // remaining: exch 1 (2100.5), exch 3 (2200.5) → median = (2100.5+2200.5)/2 = 2150.5
        let external_mid =
            external_fair_value_mid(&exchanges, 2, now_ns, 500).expect("external mid");

        assert!((external_mid - 2150.5).abs() < 1e-9);
    }

    #[test]
    fn external_fair_value_mid_uses_median_not_mean() {
        let now_ns = 10_000_000_000;
        let exchanges = [
            default_exchange(),
            (1, msg(2100.0, 2101.0, 9_999_000_000)),
            (2, msg(2100.0, 2101.0, 9_999_000_000)),
            (3, msg(2500.0, 2501.0, 9_999_000_000)),
            default_exchange(),
            default_exchange(),
            default_exchange(),
        ];

        // local=0 excluded; 3 valid mids: 2100.5, 2100.5, 2500.5 → median = 2100.5
        let external_mid =
            external_fair_value_mid(&exchanges, 0, now_ns, 500).expect("external mid");

        assert!((external_mid - 2100.5).abs() < 1e-9);
    }

    #[test]
    fn external_fair_value_mid_returns_none_when_all_stale() {
        let now_ns = 10_000_000_000;
        let exchanges = [
            default_exchange(),
            (1, msg(2100.0, 2101.0, 7_000_000_000)), // 3000ms old, stale at 2000ms
            default_exchange(),
            default_exchange(),
            default_exchange(),
            default_exchange(),
            default_exchange(),
        ];

        let external_mid = external_fair_value_mid(&exchanges, 0, now_ns, 2000);

        assert!(external_mid.is_none());
    }
}
