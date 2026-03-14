use super::*;

// ─── V1 Tests ────────────────────────────────────────────────────

#[test]
fn test_v1_event_size_and_alignment() {
    assert_eq!(std::mem::size_of::<ShmPrivateEvent>(), 64);
    assert_eq!(std::mem::align_of::<ShmPrivateEvent>(), 64);
}

#[test]
fn test_v1_order_created() {
    let event = ShmPrivateEvent::order_created(1, 2, 0, 12345, 1.5, false);
    assert_eq!(event.sequence, 1);
    assert_eq!(event.exchange_id, 2);
    assert_eq!(event.event_type().unwrap(), EventType::OrderCreated);
    assert_eq!(event.symbol_id, 0);
    assert_eq!(event.order_id, 12345);
    assert_eq!(event.remaining_size, 1.5);
}

#[test]
fn test_v1_order_filled() {
    let event = ShmPrivateEvent::order_filled(2, 2, 1, 67890, 3000.0, 0.5, 1.0, 0.15, true);
    assert_eq!(event.sequence, 2);
    assert_eq!(event.event_type().unwrap(), EventType::OrderFilled);
    assert_eq!(event.fill_price, 3000.0);
    assert_eq!(event.fill_size, 0.5);
    assert_eq!(event.remaining_size, 1.0);
    assert_eq!(event.fee_paid, 0.15);
}

#[test]
fn test_v1_order_canceled() {
    let event = ShmPrivateEvent::order_canceled(3, 2, 0, 12345);
    assert_eq!(event.event_type().unwrap(), EventType::OrderCanceled);
    assert_eq!(event.order_id, 12345);
}

// ─── V2 Tests ────────────────────────────────────────────────────

#[test]
fn test_v2_event_size_and_alignment() {
    assert_eq!(std::mem::size_of::<ShmPrivateEventV2>(), 128);
    assert_eq!(std::mem::align_of::<ShmPrivateEventV2>(), 64);
}

#[test]
fn test_v2_order_created() {
    let event = ShmPrivateEventV2::order_created(
        1, 2, 0, 99999, 1710000001, 45678, 3000.0, 0.05, false, 1234567890,
    );
    assert_eq!(event.sequence, 1);
    assert_eq!(event.exchange_id, 2);
    assert_eq!(event.event_type().unwrap(), EventType::OrderCreated);
    assert_eq!(event.exchange_order_id, 99999);
    assert_eq!(event.client_order_id, 1710000001);
    assert_eq!(event.order_index, 45678);
    assert_eq!(event.order_price, 3000.0);
    assert_eq!(event.original_size, 0.05);
    assert_eq!(event.remaining_size, 0.05);
    assert_eq!(event.is_ask, 0);
}

#[test]
fn test_v2_order_filled() {
    let event = ShmPrivateEventV2::order_filled(
        2, 2, 0, 99999, 1710000001, 45678, 3000.0, 0.03, 0.02, 0.15, false, 1234567890, 777,
    );
    assert_eq!(event.event_type().unwrap(), EventType::OrderFilled);
    assert_eq!(event.exchange_order_id, 99999);
    assert_eq!(event.client_order_id, 1710000001);
    assert_eq!(event.fill_price, 3000.0);
    assert_eq!(event.fill_size, 0.03);
    assert_eq!(event.remaining_size, 0.02);
    assert_eq!(event.fee_paid, 0.15);
    assert_eq!(event.trade_id, 777);
}

#[test]
fn test_v2_order_canceled() {
    let event =
        ShmPrivateEventV2::order_canceled(3, 2, 0, 99999, 1710000001, 45678, 0.02, 1234567890);
    assert_eq!(event.event_type().unwrap(), EventType::OrderCanceled);
    assert_eq!(event.exchange_order_id, 99999);
    assert_eq!(event.client_order_id, 1710000001);
    assert_eq!(event.remaining_size, 0.02);
}

#[test]
fn test_v2_order_rejected() {
    let event = ShmPrivateEventV2::order_rejected(4, 2, 0, 1710000001, 1234567890);
    assert_eq!(event.event_type().unwrap(), EventType::OrderRejected);
    assert_eq!(event.client_order_id, 1710000001);
    assert_eq!(event.exchange_order_id, 0);
}

#[test]
fn test_v2_default() {
    let event = ShmPrivateEventV2::default();
    assert_eq!(event.sequence, 0);
    assert_eq!(event.client_order_id, 0);
    assert_eq!(event.trade_id, 0);
}

#[test]
fn test_v2_id_separation() {
    // Verify all four IDs are independent
    let event = ShmPrivateEventV2::order_created(
        1, 2, 0, 99999,      // exchange_order_id
        1710000001, // client_order_id
        45678,      // order_index
        3000.0, 0.05, true, 0,
    );
    assert_ne!(event.exchange_order_id as i64, event.client_order_id);
    assert_ne!(event.order_index, event.client_order_id);
    assert_ne!(event.exchange_order_id, event.order_index as u64);
}
