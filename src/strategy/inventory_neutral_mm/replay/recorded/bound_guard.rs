//! Offline-only placement headroom for quotes pointing toward flat at decision.
//! It does not classify executions as closing, relax cancellation thresholds,
//! change quantities, or release any pending exposure. The fixed reserve must be
//! estimated and frozen outside the evaluated period by the research protocol.
use super::MakerPriceBounds;
use std::cell::Cell;

thread_local! {
    static RESERVE_BPS: Cell<Option<f64>> = const { Cell::new(None) };
}

pub(in crate::strategy::inventory_neutral_mm) fn reset(reserve_bps: Option<f64>) {
    RESERVE_BPS.set(reserve_bps);
}

pub(in crate::strategy::inventory_neutral_mm) fn placement(
    bounds: MakerPriceBounds,
    position: f64,
    mid: f64,
    tick: f64,
) -> MakerPriceBounds {
    let Some(reserve) = RESERVE_BPS.get() else {
        return bounds;
    };
    apply(bounds, position, mid, tick, reserve)
}

fn apply(
    mut bounds: MakerPriceBounds,
    position: f64,
    mid: f64,
    tick: f64,
    reserve: f64,
) -> MakerPriceBounds {
    let extra = mid * reserve / 10_000.0;
    // Never round inward, even for a reserve smaller than one tick. The
    // existing cancellation bound is computed separately and remains intact.
    if position < 0.0 {
        bounds.max_bid = ((bounds.max_bid - extra) / tick).floor() * tick;
    } else if position > 0.0 {
        bounds.min_ask = ((bounds.min_ask + extra) / tick).ceil() * tick;
    }
    bounds
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bound_guard_moves_only_toward_flat_side_outward() {
        let b = MakerPriceBounds {
            max_bid: 99.9,
            min_ask: 100.1,
        };
        let long = apply(b, 1.0, 100.0, 0.1, 0.1);
        assert_eq!(long.max_bid, b.max_bid);
        assert!(long.min_ask > b.min_ask);
        let short = apply(b, -1.0, 100.0, 0.1, 0.1);
        assert_eq!(short.min_ask, b.min_ask);
        assert!(short.max_bid < b.max_bid);
        let flat = apply(b, 0.0, 100.0, 0.1, 10.0);
        assert_eq!((flat.max_bid, flat.min_ask), (b.max_bid, b.min_ask));
        for p in [1.0, -1.0] {
            for r in [0.1, 1.0, 10.0] {
                let q = apply(b, p, 100.0, 0.1, r);
                assert!(q.max_bid <= b.max_bid && q.min_ask >= b.min_ask);
                assert!((q.max_bid / 0.1 - (q.max_bid / 0.1).round()).abs() < 1e-9);
                assert!((q.min_ask / 0.1 - (q.min_ask / 0.1).round()).abs() < 1e-9);
            }
        }
    }

    #[test]
    fn bound_guard_disabled_and_reset_restore_exact_original_bounds() {
        let b = MakerPriceBounds {
            max_bid: 99.9,
            min_ask: 100.1,
        };
        reset(Some(1.0));
        assert!(placement(b, 1.0, 100.0, 0.1).min_ask > b.min_ask);
        reset(None);
        let q = placement(b, 1.0, 100.0, 0.1);
        assert_eq!((q.max_bid, q.min_ask), (b.max_bid, b.min_ask));
    }
}
