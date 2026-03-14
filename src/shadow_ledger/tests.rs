use super::*;

#[test]
fn test_order_side_sign() {
    assert_eq!(OrderSide::Buy.sign(), 1.0);
    assert_eq!(OrderSide::Sell.sign(), -1.0);
}

#[test]
fn test_order_side_display() {
    assert_eq!(OrderSide::Buy.to_string(), "bid");
    assert_eq!(OrderSide::Sell.to_string(), "ask");
}

#[test]
fn test_shadow_ledger_initial_state() {
    let ledger = ShadowLedger::default();
    assert_eq!(ledger.real_pos_f64(), 0.0);
    assert_eq!(ledger.in_flight_pos_f64(), 0.0);
    assert_eq!(ledger.realized_pnl, 0.0);
    assert_eq!(ledger.total_exposure(), 0.0);
    assert_eq!(ledger.active_order_count(), 0);
}

#[test]
fn test_add_in_flight() {
    let ledger = ShadowLedger::default();

    ledger.add_in_flight(1.5);
    assert!((ledger.in_flight_pos_f64() - 1.5).abs() < 1e-6);
    assert!((ledger.total_exposure() - 1.5).abs() < 1e-6);

    ledger.add_in_flight(-0.5);
    assert!((ledger.in_flight_pos_f64() - 1.0).abs() < 1e-6);
    assert!((ledger.total_exposure() - 1.0).abs() < 1e-6);
}

#[test]
fn test_shadow_ledger_order_created() {
    let mut state = ShadowLedger::default();

    // First register the order (simulating place_order_optimistic)
    state.register_order(12345, 0, OrderSide::Buy, 3000.0, 1.5);
    assert_eq!(state.active_order_count(), 1);

    // Then receive the OrderCreated event (confirmation from exchange)
    let event = ShmPrivateEvent::order_created(1, 2, 0, 12345, 1.5, false);
    let result = state.apply_event(&event);
    assert!(result.is_ok());

    // Order should still be active (not duplicated)
    assert_eq!(state.active_order_count(), 1);
    assert!(state.has_active_order(12345));
}

#[test]
fn test_shadow_ledger_optimistic_fill() {
    let mut state = ShadowLedger::default();

    // Optimistically add in_flight for a buy order
    state.add_in_flight(1.5);
    assert!((state.in_flight_pos_f64() - 1.5).abs() < 1e-6);
    assert!((state.total_exposure() - 1.5).abs() < 1e-6);

    // Create order (with side)
    state.active_orders.insert(
        12345,
        OrderState {
            order_id: 12345,
            symbol_id: 0,
            side: OrderSide::Buy,
            initial_size: 1.5,
            filled_size: 0.0,
            remaining_size: 1.5,
            avg_fill_price: 0.0,
            total_fees: 0.0,
            created_at: Instant::now(),
            last_update: Instant::now(),
            tracked: true,
        },
    );

    // Fill order (reconciles in_flight -> real_pos)
    let fill_event =
        ShmPrivateEvent::order_filled(2, 2, 0, 12345, 3000.0, 0.5, 1.0, 0.15, false);
    state.apply_event(&fill_event).unwrap();

    assert!((state.real_pos_f64() - 0.5).abs() < 1e-6);
    assert!((state.in_flight_pos_f64() - 1.0).abs() < 1e-6); // 1.5 - 0.5 = 1.0
    assert!((state.total_exposure() - 1.5).abs() < 1e-6);
    assert_eq!(state.active_order_count(), 1); // Still active (partial fill)

    let order = state.active_orders.get(&12345).unwrap();
    assert_eq!(order.filled_size, 0.5);
    assert_eq!(order.remaining_size, 1.0);
}

#[test]
fn test_shadow_ledger_order_canceled() {
    let mut state = ShadowLedger::default();

    // Optimistically add in_flight
    state.add_in_flight(1.5);

    // Create order with side
    state.active_orders.insert(
        12345,
        OrderState {
            order_id: 12345,
            symbol_id: 0,
            side: OrderSide::Buy,
            initial_size: 1.5,
            filled_size: 0.0,
            remaining_size: 1.5,
            avg_fill_price: 0.0,
            total_fees: 0.0,
            created_at: Instant::now(),
            last_update: Instant::now(),
            tracked: true,
        },
    );

    // Cancel order (rollback in_flight)
    let cancel_event = ShmPrivateEvent::order_canceled(2, 2, 0, 12345);
    state.apply_event(&cancel_event).unwrap();

    assert_eq!(state.active_order_count(), 0);
    assert!(!state.has_active_order(12345));
    assert!((state.in_flight_pos_f64()).abs() < 1e-6); // Rolled back
}

#[test]
fn test_sell_order_pnl() {
    let mut state = ShadowLedger::default();

    // Optimistically add in_flight for sell order (negative)
    state.add_in_flight(-1.0);

    // Create sell order
    state.active_orders.insert(
        12346,
        OrderState {
            order_id: 12346,
            symbol_id: 0,
            side: OrderSide::Sell,
            initial_size: 1.0,
            filled_size: 0.0,
            remaining_size: 1.0,
            avg_fill_price: 0.0,
            total_fees: 0.0,
            created_at: Instant::now(),
            last_update: Instant::now(),
            tracked: true,
        },
    );

    // Fill sell order
    let fill_event =
        ShmPrivateEvent::order_filled(2, 2, 0, 12346, 51000.0, 1.0, 0.0, 3.0, true);
    state.apply_event(&fill_event).unwrap();

    // Check reconciliation
    assert!((state.real_pos_f64() - (-1.0)).abs() < 1e-6);
    assert!((state.in_flight_pos_f64()).abs() < 1e-6);

    // PnL should be positive (revenue from selling)
    assert!(state.realized_pnl > 0.0);
    let expected_pnl = 51000.0 * 1.0 - 3.0;
    assert!((state.realized_pnl - expected_pnl).abs() < 0.01);
}

#[test]
fn test_sequence_validation() {
    let mut ledger = ShadowLedger::default();

    // First event
    let event1 = ShmPrivateEvent::order_created(1, 2, 0, 12349, 1.0, false);
    assert!(ledger.apply_event(&event1).is_ok());
    assert_eq!(ledger.last_sequence, 1);

    // Out of order event (should error)
    let event_old = ShmPrivateEvent::order_created(1, 2, 0, 12350, 1.0, false);
    let result = ledger.apply_event(&event_old);
    assert!(result.is_err());

    // Gap in sequence (should log warning but continue)
    let event_gap = ShmPrivateEvent::order_created(5, 2, 0, 12351, 1.0, false);
    assert!(ledger.apply_event(&event_gap).is_ok());
    assert_eq!(ledger.last_sequence, 5);
}
