use super::*;

fn make_tracker() -> OrderTracker {
    OrderTracker::new()
}

#[test]
fn test_start_tracking_and_exposure() {
    let tracker = make_tracker();

    // Register a buy order
    tracker.start_tracking(1001, OrderSide::Buy, 3000.0, 0.05);

    assert_eq!(tracker.active_order_count(), 1);
    assert!((tracker.net_pending_exposure() - 0.05).abs() < 1e-10);
    assert!((tracker.worst_case_long() - 0.05).abs() < 1e-10);
    assert!((tracker.worst_case_short() - 0.0).abs() < 1e-10);

    // Register a sell order
    tracker.start_tracking(1002, OrderSide::Sell, 3010.0, 0.05);

    assert_eq!(tracker.active_order_count(), 2);
    // Net pending = +0.05 - 0.05 = 0.0
    assert!((tracker.net_pending_exposure() - 0.0).abs() < 1e-10);
    // But worst-case is NOT zero!
    assert!((tracker.worst_case_long() - 0.05).abs() < 1e-10);
    assert!((tracker.worst_case_short() - (-0.05)).abs() < 1e-10);
}

#[test]
fn test_mark_failed_removes_exposure() {
    let tracker = make_tracker();

    tracker.start_tracking(1001, OrderSide::Buy, 3000.0, 0.05);
    assert!((tracker.worst_case_long() - 0.05).abs() < 1e-10);

    // API call failed → rollback
    tracker.mark_failed(1001);

    assert_eq!(tracker.active_order_count(), 0);
    assert!((tracker.worst_case_long() - 0.0).abs() < 1e-10);
}

#[test]
fn test_batch_order_bilateral_exposure() {
    let tracker = make_tracker();

    // Simulate place_batch: bid + ask registered separately
    tracker.start_tracking(2001, OrderSide::Buy, 3000.0, 0.05);
    tracker.start_tracking(2002, OrderSide::Sell, 3010.0, 0.05);

    // Net exposure = 0 (old bug: in_flight_pos would be 0, hiding risk)
    assert!((tracker.net_pending_exposure() - 0.0).abs() < 1e-10);

    // But worst-case correctly shows bilateral risk
    assert!((tracker.worst_case_long() - 0.05).abs() < 1e-10);
    assert!((tracker.worst_case_short() - (-0.05)).abs() < 1e-10);

    // Now simulate: only bid fills
    let fill_event = ShmPrivateEventV2::order_filled(
        1,    // sequence
        2,    // exchange_id (Lighter)
        1,    // symbol_id (ETH)
        9001, // exchange_order_id
        2001, // client_order_id
        5001, // order_index
        3000.0, 0.05, 0.0, // remaining = 0 (fully filled)
        0.01, false, // is_ask = false (buy)
        0, 7001, // trade_id
    );

    // First we need to simulate OrderCreated to bind IDs
    let created_event = ShmPrivateEventV2::order_created(
        1, // sequence (will be skipped as duplicate, but let's use different)
        2, 1, 9001, // exchange_order_id
        2001, // client_order_id
        5001, // order_index
        3000.0, 0.05, false, 0,
    );

    // Reset sequence for test
    tracker.last_sequence.store(0, Ordering::Release);

    let _ = tracker.apply_event(&created_event);

    // Also create the ask order
    let created_ask = ShmPrivateEventV2::order_created(
        2, 2, 1, 9002, // exchange_order_id
        2002, // client_order_id
        5002, // order_index
        3010.0, 0.05, true, 0,
    );
    let _ = tracker.apply_event(&created_ask);

    // Now apply the fill
    let mut fill = fill_event;
    fill.sequence = 3;
    let _ = tracker.apply_event(&fill);

    // Confirmed position should be +0.05 (bid filled)
    assert!((tracker.confirmed_position() - 0.05).abs() < 1e-10);

    // Ask is still active
    assert_eq!(tracker.active_order_count(), 1);

    // Worst-case long = 0.05 (confirmed, no more bids)
    assert!((tracker.worst_case_long() - 0.05).abs() < 1e-10);

    // Worst-case short = 0.05 - 0.05 = 0.0 (if ask fills, position goes to 0)
    assert!((tracker.worst_case_short() - 0.0).abs() < 1e-10);
}

#[test]
fn test_order_canceled_removes_exposure() {
    let tracker = make_tracker();

    tracker.start_tracking(3001, OrderSide::Buy, 3000.0, 0.1);

    // Simulate OrderCreated
    let created =
        ShmPrivateEventV2::order_created(1, 2, 1, 8001, 3001, 4001, 3000.0, 0.1, false, 0);
    let _ = tracker.apply_event(&created);

    assert!((tracker.worst_case_long() - 0.1).abs() < 1e-10);

    // Simulate OrderCanceled
    let canceled = ShmPrivateEventV2::order_canceled(2, 2, 1, 8001, 3001, 4001, 0.1, 0);
    let _ = tracker.apply_event(&canceled);

    // Exposure should be zero
    assert_eq!(tracker.active_order_count(), 0);
    assert!((tracker.worst_case_long() - 0.0).abs() < 1e-10);
}

#[test]
fn test_partial_fill() {
    let tracker = make_tracker();

    tracker.start_tracking(4001, OrderSide::Sell, 3010.0, 0.10);

    // OrderCreated
    let created =
        ShmPrivateEventV2::order_created(1, 2, 1, 7001, 4001, 6001, 3010.0, 0.10, true, 0);
    let _ = tracker.apply_event(&created);

    // Partial fill: 0.04 of 0.10
    let fill = ShmPrivateEventV2::order_filled(
        2, 2, 1, 7001, 4001, 6001, 3010.0, 0.04, 0.06, 0.005, true, 0, 9001,
    );
    let _ = tracker.apply_event(&fill);

    // Confirmed position = -0.04
    assert!((tracker.confirmed_position() - (-0.04)).abs() < 1e-10);

    // Remaining sell exposure = 0.06
    assert!((tracker.worst_case_short() - (-0.04 - 0.06)).abs() < 1e-10);

    // Still active
    assert_eq!(tracker.active_order_count(), 1);
}

#[test]
fn test_force_sync_position() {
    let tracker = make_tracker();

    // Simulate drift: tracker thinks 0.0, exchange says 0.05
    let delta = tracker.force_sync_position(0.05);
    assert!((delta - 0.05).abs() < 1e-10);
    assert!((tracker.confirmed_position() - 0.05).abs() < 1e-10);
}

#[test]
fn test_gc_completed_orders() {
    let tracker = make_tracker();

    tracker.start_tracking(5001, OrderSide::Buy, 3000.0, 0.05);
    tracker.mark_failed(5001);

    {
        let state = tracker.state.read();
        assert_eq!(state.completed_orders.len(), 1);
    }

    // GC with 0 TTL should remove it
    tracker.gc_completed_orders(Duration::from_secs(0));

    {
        let state = tracker.state.read();
        assert_eq!(state.completed_orders.len(), 0);
    }
}

#[test]
fn test_duplicate_fill_dedup() {
    let tracker = make_tracker();

    tracker.start_tracking(6001, OrderSide::Buy, 3000.0, 0.10);

    let created =
        ShmPrivateEventV2::order_created(1, 2, 1, 6601, 6001, 6501, 3000.0, 0.10, false, 0);
    let _ = tracker.apply_event(&created);

    // First fill
    let fill1 = ShmPrivateEventV2::order_filled(
        2, 2, 1, 6601, 6001, 6501, 3000.0, 0.05, 0.05, 0.005, false, 0,
        8801, // trade_id = 8801
    );
    let _ = tracker.apply_event(&fill1);

    assert!((tracker.confirmed_position() - 0.05).abs() < 1e-10);

    // Duplicate fill (same trade_id)
    let mut fill2 = fill1;
    fill2.sequence = 3;
    let _ = tracker.apply_event(&fill2);

    // Position should NOT double-count
    assert!((tracker.confirmed_position() - 0.05).abs() < 1e-10);
}
