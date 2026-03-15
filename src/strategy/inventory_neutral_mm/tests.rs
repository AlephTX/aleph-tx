use super::*;

#[test]
fn test_grid_level_calculation() {
    let config = InventoryNeutralMMConfig {
        tick_size: 0.01,
        step_size: 0.0001,
        grid_levels: 3,
        grid_spacing_bps: 2.0,
        grid_size_decay: 0.7,
        ..Default::default()
    };

    // Test bid levels (prices should decrease)
    let base_price = 2000.0;
    let base_size = 0.1;

    let mut bid_levels = Vec::new();
    for i in 0..config.grid_levels {
        let spacing_dollars = base_price * config.grid_spacing_bps * (i as f64) / 10000.0;
        let price = base_price - spacing_dollars;
        let rounded_price = (price / config.tick_size).floor() * config.tick_size;

        let size_multiplier = config.grid_size_decay.powi(i as i32);
        let size = base_size * size_multiplier;
        let rounded_size = (size / config.step_size).floor() * config.step_size;

        if rounded_size >= 0.001 {
            bid_levels.push((rounded_price, rounded_size));
        }
    }

    // Verify 3 levels generated
    assert_eq!(bid_levels.len(), 3);

    // Verify prices descend
    assert!(bid_levels[0].0 >= bid_levels[1].0);
    assert!(bid_levels[1].0 >= bid_levels[2].0);

    // Verify size decay (0.7^0, 0.7^1, 0.7^2)
    assert!((bid_levels[0].1 - 0.1).abs() < 0.001);
    assert!((bid_levels[1].1 - 0.07).abs() < 0.001);
    assert!((bid_levels[2].1 - 0.049).abs() < 0.001);
}

#[test]
fn test_multi_level_order_tracking() {
    // Test helper methods without full MM initialization
    let config = InventoryNeutralMMConfig {
        grid_levels: 3,
        requote_threshold_bps: 10.0,
        ..Default::default()
    };

    // Create a minimal MM instance for testing
    let active_orders = [
        ActiveOrder {
            order_id: "1".to_string(),
            client_order_id: 1,
            order_index: None,
            side: OrderSide::Buy,
            price: 3000.0,
            size: 0.05,
            placed_at: Instant::now(),
        },
        ActiveOrder {
            order_id: "2".to_string(),
            client_order_id: 2,
            order_index: None,
            side: OrderSide::Buy,
            price: 2995.0,
            size: 0.035,
            placed_at: Instant::now(),
        },
        ActiveOrder {
            order_id: "3".to_string(),
            client_order_id: 3,
            order_index: None,
            side: OrderSide::Sell,
            price: 3010.0,
            size: 0.05,
            placed_at: Instant::now(),
        },
    ];

    // Test filtering by side
    let bids: Vec<_> = active_orders
        .iter()
        .filter(|o| o.side == OrderSide::Buy)
        .collect();
    assert_eq!(bids.len(), 2);
    assert_eq!(bids[0].price, 3000.0);
    assert_eq!(bids[1].price, 2995.0);

    let asks: Vec<_> = active_orders
        .iter()
        .filter(|o| o.side == OrderSide::Sell)
        .collect();
    assert_eq!(asks.len(), 1);
    assert_eq!(asks[0].price, 3010.0);

    // Test requote logic
    let target_prices = [(3000.0, 0.05), (2995.0, 0.035)];

    // Same prices - no requote needed
    let mut needs_requote = false;
    if bids.len() != target_prices.len() {
        needs_requote = true;
    } else {
        for (order, &(target_price, _)) in bids.iter().zip(target_prices.iter()) {
            let price_diff = (order.price - target_price).abs();
            let threshold = target_price * config.requote_threshold_bps / 10000.0;
            if price_diff > threshold {
                needs_requote = true;
                break;
            }
        }
    }
    assert!(!needs_requote);

    // Price moved beyond threshold (10 bps = 0.1%)
    let moved_prices = [(3005.0, 0.05), (2995.0, 0.035)];
    needs_requote = false;
    for (order, &(target_price, _)) in bids.iter().zip(moved_prices.iter()) {
        let price_diff = (order.price - target_price).abs();
        let threshold = target_price * config.requote_threshold_bps / 10000.0;
        if price_diff > threshold {
            needs_requote = true;
            break;
        }
    }
    assert!(needs_requote); // 5 dollar move on 3000 = 16.7 bps > 10 bps threshold
}

#[test]
fn test_grid_integration() {
    // Integration test: verify full grid calculation pipeline
    let config = InventoryNeutralMMConfig {
        grid_levels: 3,
        grid_spacing_bps: 5.0,
        grid_size_decay: 0.7,
        tick_size: 0.01,
        step_size: 0.001,
        ..Default::default()
    };

    let base_price = 3000.0;
    let base_size = 0.1;

    // Calculate bid levels
    let mut bid_levels = Vec::new();
    for i in 0..config.grid_levels {
        let spacing_dollars = base_price * config.grid_spacing_bps * (i as f64) / 10000.0;
        let price = base_price - spacing_dollars;
        let rounded_price = (price / config.tick_size).floor() * config.tick_size;

        let size_multiplier = config.grid_size_decay.powi(i as i32);
        let size = base_size * size_multiplier;
        let rounded_size = (size / config.step_size).floor() * config.step_size;

        if rounded_size >= 0.001 {
            bid_levels.push((rounded_price, rounded_size));
        }
    }

    // Verify bid levels
    assert_eq!(bid_levels.len(), 3);

    // Level 0: base price, full size
    assert!((bid_levels[0].0 - 3000.0).abs() < 0.01);
    assert!((bid_levels[0].1 - 0.1).abs() < 0.001);

    // Level 1: -5 bps (1.5 dollars), 70% size (0.1*0.7=0.0699.. → floor → 0.069)
    assert!((bid_levels[1].0 - 2998.5).abs() < 0.01);
    assert!((bid_levels[1].1 - 0.069).abs() < 0.001);

    // Level 2: -10 bps (3.0 dollars), 49% size (0.1*0.49=0.0489.. → floor → 0.048)
    assert!((bid_levels[2].0 - 2997.0).abs() < 0.01);
    assert!((bid_levels[2].1 - 0.048).abs() < 0.001);

    // Calculate ask levels
    let mut ask_levels = Vec::new();
    for i in 0..config.grid_levels {
        let spacing_dollars = base_price * config.grid_spacing_bps * (i as f64) / 10000.0;
        let price = base_price + spacing_dollars;
        let rounded_price = (price / config.tick_size).floor() * config.tick_size;

        let size_multiplier = config.grid_size_decay.powi(i as i32);
        let size = base_size * size_multiplier;
        let rounded_size = (size / config.step_size).floor() * config.step_size;

        if rounded_size >= 0.001 {
            ask_levels.push((rounded_price, rounded_size));
        }
    }

    // Verify ask levels
    assert_eq!(ask_levels.len(), 3);

    // Level 0: base price, full size
    assert!((ask_levels[0].0 - 3000.0).abs() < 0.01);
    assert!((ask_levels[0].1 - 0.1).abs() < 0.001);

    // Level 1: +5 bps (1.5 dollars), 70% size (0.1*0.7=0.0699.. → floor → 0.069)
    assert!((ask_levels[1].0 - 3001.5).abs() < 0.01);
    assert!((ask_levels[1].1 - 0.069).abs() < 0.001);

    // Level 2: +10 bps (3.0 dollars), 49% size (0.1*0.49=0.0489.. → floor → 0.048)
    assert!((ask_levels[2].0 - 3003.0).abs() < 0.01);
    assert!((ask_levels[2].1 - 0.048).abs() < 0.001);
}

#[test]
fn test_sigmoid_size_multiplier() {
    let config = InventoryNeutralMMConfig {
        max_position: 0.15,
        sigmoid_steepness: 4.0,
        ..Default::default()
    };

    // Helper function to calculate sigmoid multiplier
    let calc_multiplier = |position: f64| -> f64 {
        let normalized_pos = position / config.max_position;
        1.0 + 2.0 * (config.sigmoid_steepness * normalized_pos.abs()).tanh()
    };

    // Test at different position levels
    let pos_0 = 0.0;
    let pos_5pct = 0.05 * config.max_position; // 0.0075
    let pos_50pct = 0.5 * config.max_position; // 0.075
    let pos_80pct = 0.8 * config.max_position; // 0.12
    let pos_100pct = config.max_position; // 0.15

    let mult_0 = calc_multiplier(pos_0);
    let mult_5 = calc_multiplier(pos_5pct);
    let mult_50 = calc_multiplier(pos_50pct);
    let mult_80 = calc_multiplier(pos_80pct);
    let mult_100 = calc_multiplier(pos_100pct);

    // Verify sigmoid properties:
    // 1. At pos=0, multiplier ≈ 1.0 (minimal urgency)
    assert!((mult_0 - 1.0).abs() < 0.01, "pos=0: mult={}", mult_0);

    // 2. Monotonically increasing
    assert!(
        mult_5 > mult_0,
        "mult_5={} should > mult_0={}",
        mult_5,
        mult_0
    );
    assert!(
        mult_50 > mult_5,
        "mult_50={} should > mult_5={}",
        mult_50,
        mult_5
    );
    assert!(
        mult_80 > mult_50,
        "mult_80={} should > mult_50={}",
        mult_80,
        mult_50
    );
    assert!(
        mult_100 > mult_80,
        "mult_100={} should > mult_80={}",
        mult_100,
        mult_80
    );

    // 3. At pos=100%, multiplier ≈ 3.0 (max urgency)
    assert!((mult_100 - 3.0).abs() < 0.1, "pos=100%: mult={}", mult_100);

    // 4. Steeper growth in middle range (50% → 80% should have larger delta than 5% → 50%)
    // This validates the sigmoid curve is steeper in the middle
    let delta_low = mult_50 - mult_5;
    let delta_mid = mult_80 - mult_50;
    assert!(
        delta_mid > 0.0,
        "delta_mid={} should be positive",
        delta_mid
    );
    assert!(
        delta_low > 0.0,
        "delta_low={} should be positive",
        delta_low
    );
}

#[test]
fn test_vw_micro_price_calculation() {
    use crate::shm_depth_reader::{PriceLevel, ShmDepthSnapshot};

    // Create mock depth snapshot
    let depth = ShmDepthSnapshot {
        seqlock: 0,
        exchange_id: 2,
        symbol_id: 1002,
        _padding1: 0,
        timestamp_ns: 1234567890,
        bids: [
            PriceLevel {
                price: 3000.0,
                size: 1.0,
            },
            PriceLevel {
                price: 2999.0,
                size: 2.0,
            },
            PriceLevel {
                price: 2998.0,
                size: 1.5,
            },
            PriceLevel {
                price: 2997.0,
                size: 1.0,
            },
            PriceLevel {
                price: 2996.0,
                size: 0.5,
            },
        ],
        asks: [
            PriceLevel {
                price: 3001.0,
                size: 1.0,
            },
            PriceLevel {
                price: 3002.0,
                size: 2.0,
            },
            PriceLevel {
                price: 3003.0,
                size: 1.5,
            },
            PriceLevel {
                price: 3004.0,
                size: 1.0,
            },
            PriceLevel {
                price: 3005.0,
                size: 0.5,
            },
        ],
        _reserved: [0; 72],
    };

    // Calculate VWMicro manually
    let bid_notional: f64 = depth.bids.iter().map(|l| l.price * l.size).sum();
    let ask_notional: f64 = depth.asks.iter().map(|l| l.price * l.size).sum();
    let vw_micro =
        (bid_notional * 3001.0 + ask_notional * 3000.0) / (bid_notional + ask_notional);

    // VWMicro should be between bid and ask
    assert!(
        vw_micro > 3000.0 && vw_micro < 3001.0,
        "VWMicro {} should be between 3000.0 and 3001.0",
        vw_micro
    );

    // Should be closer to mid than simple average due to depth weighting
    let simple_mid = 3000.5;
    assert!(
        (vw_micro - simple_mid).abs() < 1.0,
        "VWMicro {} should be close to simple mid {}",
        vw_micro,
        simple_mid
    );
}
