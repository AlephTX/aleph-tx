use super::*;
use crate::exchange::{
    BatchAction, BatchOrderParams, BatchOrderResult, BatchResult, Exchange, OrderInfo, OrderResult,
    OrderType, Side,
};
use async_trait::async_trait;

#[derive(Clone)]
struct MockExchange {
    active_orders: Result<Vec<OrderInfo>, String>,
}

#[async_trait]
impl Exchange for MockExchange {
    async fn buy(&self, _size: f64, _price: f64) -> anyhow::Result<OrderResult> {
        unreachable!("buy not used in order_tracker tests")
    }

    async fn sell(&self, _size: f64, _price: f64) -> anyhow::Result<OrderResult> {
        unreachable!("sell not used in order_tracker tests")
    }

    async fn place_batch(&self, _params: BatchOrderParams) -> anyhow::Result<BatchOrderResult> {
        unreachable!("place_batch not used in order_tracker tests")
    }

    async fn cancel_order(&self, _order_id: i64) -> anyhow::Result<()> {
        unreachable!("cancel_order not used in order_tracker tests")
    }

    async fn cancel_all(&self) -> anyhow::Result<u32> {
        unreachable!("cancel_all not used in order_tracker tests")
    }

    async fn get_active_orders(&self) -> anyhow::Result<Vec<OrderInfo>> {
        self.active_orders
            .clone()
            .map_err(anyhow::Error::msg)
    }

    async fn close_all_positions(&self, _current_price: f64) -> anyhow::Result<()> {
        unreachable!("close_all_positions not used in order_tracker tests")
    }

    async fn execute_batch(&self, _actions: Vec<BatchAction>) -> anyhow::Result<BatchResult> {
        unreachable!("execute_batch not used in order_tracker tests")
    }

    async fn get_account_stats(
        &self,
    ) -> anyhow::Result<crate::strategy::inventory_neutral_mm::AccountStats> {
        unreachable!("get_account_stats not used in order_tracker tests")
    }

    fn limit_order_type(&self) -> OrderType {
        OrderType::PostOnly
    }
}

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
fn test_mark_pending_cancel_keeps_exposure_until_confirmed() {
    let tracker = make_tracker();

    tracker.start_tracking(5501, OrderSide::Buy, 3000.0, 0.05);
    let created =
        ShmPrivateEventV2::order_created(1, 2, 1, 8501, 5501, 4501, 3000.0, 0.05, false, 0);
    let _ = tracker.apply_event(&created);

    tracker.mark_pending_cancel(5501);

    assert_eq!(tracker.active_order_count(), 1);
    assert!((tracker.worst_case_long() - 0.05).abs() < 1e-10);

    let state = tracker.state.read();
    let order = state.active_orders.get(&5501).expect("order should remain active");
    assert_eq!(order.lifecycle, OrderLifecycle::PendingCancel);
}

#[test]
fn test_revert_pending_cancel_restores_open_or_partially_filled_state() {
    let tracker = make_tracker();

    tracker.start_tracking(5511, OrderSide::Buy, 3000.0, 0.05);
    let created =
        ShmPrivateEventV2::order_created(1, 2, 1, 8511, 5511, 4511, 3000.0, 0.05, false, 0);
    let _ = tracker.apply_event(&created);
    tracker.mark_pending_cancel(5511);
    tracker.revert_pending_cancel(5511);

    tracker.start_tracking(5512, OrderSide::Sell, 3010.0, 0.10);
    let created_2 =
        ShmPrivateEventV2::order_created(2, 2, 1, 8512, 5512, 4512, 3010.0, 0.10, true, 0);
    let _ = tracker.apply_event(&created_2);
    let fill = ShmPrivateEventV2::order_filled(
        3, 2, 1, 8512, 5512, 4512, 3010.0, 0.04, 0.06, 0.005, true, 0, 9921,
    );
    let _ = tracker.apply_event(&fill);
    tracker.mark_pending_cancel(5512);
    tracker.revert_pending_cancel(5512);

    let state = tracker.state.read();
    assert_eq!(
        state.active_orders.get(&5511).map(|o| o.lifecycle),
        Some(OrderLifecycle::Open)
    );
    assert_eq!(
        state.active_orders.get(&5512).map(|o| o.lifecycle),
        Some(OrderLifecycle::PartiallyFilled)
    );
}

#[test]
fn test_pending_cancel_partial_fill_then_canceled_preserves_position_and_clears_remaining_exposure() {
    let tracker = make_tracker();

    tracker.start_tracking(5551, OrderSide::Sell, 3010.0, 0.10);
    let created =
        ShmPrivateEventV2::order_created(1, 2, 1, 8551, 5551, 4551, 3010.0, 0.10, true, 0);
    let _ = tracker.apply_event(&created);

    tracker.mark_pending_cancel(5551);

    let fill = ShmPrivateEventV2::order_filled(
        2, 2, 1, 8551, 5551, 4551, 3010.0, 0.04, 0.06, 0.005, true, 0, 9901,
    );
    let _ = tracker.apply_event(&fill);

    assert!((tracker.confirmed_position() - (-0.04)).abs() < 1e-10);
    assert!((tracker.worst_case_short() - (-0.10)).abs() < 1e-10);

    let canceled = ShmPrivateEventV2::order_canceled(3, 2, 1, 8551, 5551, 4551, 0.06, 0);
    let _ = tracker.apply_event(&canceled);

    assert_eq!(tracker.active_order_count(), 0);
    assert!((tracker.confirmed_position() - (-0.04)).abs() < 1e-10);
    assert!((tracker.worst_case_short() - (-0.04)).abs() < 1e-10);
}

#[test]
fn test_pending_cancel_final_fill_moves_order_to_completed_filled() {
    let tracker = make_tracker();

    tracker.start_tracking(5552, OrderSide::Buy, 3000.0, 0.05);
    let created =
        ShmPrivateEventV2::order_created(1, 2, 1, 8552, 5552, 4552, 3000.0, 0.05, false, 0);
    let _ = tracker.apply_event(&created);

    tracker.mark_pending_cancel(5552);

    let fill = ShmPrivateEventV2::order_filled(
        2, 2, 1, 8552, 5552, 4552, 3000.0, 0.05, 0.0, 0.005, false, 0, 9902,
    );
    let _ = tracker.apply_event(&fill);

    assert_eq!(tracker.active_order_count(), 0);
    assert!((tracker.confirmed_position() - 0.05).abs() < 1e-10);
    assert!((tracker.worst_case_long() - 0.05).abs() < 1e-10);

    let state = tracker.state.read();
    let completed = state
        .completed_orders
        .get(&5552)
        .expect("filled order should be moved to completed");
    assert_eq!(completed.lifecycle, OrderLifecycle::Filled);
    assert!((completed.filled_size - 0.05).abs() < 1e-10);
}

#[test]
fn test_cancel_all_active_clears_bilateral_exposure() {
    let tracker = make_tracker();

    tracker.start_tracking(5601, OrderSide::Buy, 3000.0, 0.05);
    tracker.start_tracking(5602, OrderSide::Sell, 3010.0, 0.04);

    let canceled = tracker.cancel_all_active();

    assert_eq!(canceled, 2);
    assert_eq!(tracker.active_order_count(), 0);
    assert!((tracker.net_pending_exposure() - 0.0).abs() < 1e-10);
    assert!((tracker.worst_case_long() - 0.0).abs() < 1e-10);
    assert!((tracker.worst_case_short() - 0.0).abs() < 1e-10);

    let state = tracker.state.read();
    assert_eq!(state.completed_orders.len(), 2);
    assert!(state
        .completed_orders
        .values()
        .all(|order| order.lifecycle == OrderLifecycle::Canceled));
}

#[test]
fn test_late_fill_after_cancel_updates_confirmed_position_only() {
    let tracker = make_tracker();

    tracker.start_tracking(5651, OrderSide::Buy, 3000.0, 0.05);
    let created =
        ShmPrivateEventV2::order_created(1, 2, 1, 8651, 5651, 4651, 3000.0, 0.05, false, 0);
    let _ = tracker.apply_event(&created);

    let canceled = ShmPrivateEventV2::order_canceled(2, 2, 1, 8651, 5651, 4651, 0.05, 0);
    let _ = tracker.apply_event(&canceled);

    let late_fill = ShmPrivateEventV2::order_filled(
        3, 2, 1, 8651, 5651, 4651, 3000.0, 0.01, 0.0, 0.001, false, 0, 9911,
    );
    let _ = tracker.apply_event(&late_fill);

    assert_eq!(tracker.active_order_count(), 0);
    assert!((tracker.confirmed_position() - 0.01).abs() < 1e-10);
    assert!((tracker.worst_case_long() - 0.01).abs() < 1e-10);

    let state = tracker.state.read();
    let completed = state
        .completed_orders
        .get(&5651)
        .expect("completed order should remain in cache");
    assert!((completed.filled_size - 0.01).abs() < 1e-10);
}

#[test]
fn test_counterparty_fill_without_client_id_is_ignored() {
    let tracker = make_tracker();

    let fill = ShmPrivateEventV2::order_filled(
        1, 2, 1, 999001, 0, 0, 3000.0, 0.02, 0.0, 0.001, true, 0, 19901,
    );
    let _ = tracker.apply_event(&fill);

    assert_eq!(tracker.active_order_count(), 0);
    assert!((tracker.confirmed_position() - 0.0).abs() < 1e-10);
    assert!((tracker.worst_case_long() - 0.0).abs() < 1e-10);
    assert!((tracker.worst_case_short() - 0.0).abs() < 1e-10);
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

#[test]
fn test_duplicate_terminal_fill_without_trade_id_is_ignored() {
    let tracker = make_tracker();

    tracker.start_tracking(6002, OrderSide::Buy, 3000.0, 0.10);

    let created =
        ShmPrivateEventV2::order_created(1, 2, 1, 6602, 6002, 6502, 3000.0, 0.10, false, 0);
    let _ = tracker.apply_event(&created);

    let fill = ShmPrivateEventV2::order_filled(
        2, 2, 1, 6602, 6002, 6502, 3000.0, 0.10, 0.0, 0.005, false, 0, 0,
    );
    let _ = tracker.apply_event(&fill);

    assert!((tracker.confirmed_position() - 0.10).abs() < 1e-10);

    let mut duplicate_fill = fill;
    duplicate_fill.sequence = 3;
    let _ = tracker.apply_event(&duplicate_fill);

    assert!((tracker.confirmed_position() - 0.10).abs() < 1e-10);
}

#[test]
fn test_duplicate_order_created_does_not_auto_register_or_double_exposure() {
    let tracker = make_tracker();

    tracker.start_tracking(6101, OrderSide::Buy, 3000.0, 0.10);

    let created =
        ShmPrivateEventV2::order_created(1, 2, 1, 6701, 6101, 6601, 3000.0, 0.10, false, 0);
    let _ = tracker.apply_event(&created);

    assert_eq!(tracker.active_order_count(), 1);
    assert!((tracker.worst_case_long() - 0.10).abs() < 1e-10);

    let duplicate_created =
        ShmPrivateEventV2::order_created(2, 2, 1, 6701, 6101, 6601, 3000.0, 0.10, false, 0);
    let _ = tracker.apply_event(&duplicate_created);

    assert_eq!(tracker.active_order_count(), 1);
    assert!((tracker.net_pending_exposure() - 0.10).abs() < 1e-10);
    assert!((tracker.worst_case_long() - 0.10).abs() < 1e-10);

    let state = tracker.state.read();
    assert_eq!(state.completed_orders.len(), 0);
    assert_eq!(
        state.active_orders.get(&6101).and_then(|o| o.exchange_order_id),
        Some(6701)
    );
}

#[test]
fn effective_position_matches_locked_exposure_after_duplicate_open_events() {
    let tracker = make_tracker();

    tracker.start_tracking(6102, OrderSide::Sell, 3001.0, 0.10);

    let created =
        ShmPrivateEventV2::order_created(1, 2, 1, 6702, 6102, 6602, 3001.0, 0.10, true, 0);
    let _ = tracker.apply_event(&created);

    for seq in 2..=5 {
        let duplicate =
            ShmPrivateEventV2::order_created(seq, 2, 1, 6702, 6102, 6602, 3001.0, 0.10, true, 0);
        let _ = tracker.apply_event(&duplicate);
    }

    assert!((tracker.net_pending_exposure() - tracker.net_pending_exposure_locked()).abs() < 1e-10);
    assert!((tracker.worst_case_long() - tracker.worst_case_long_locked()).abs() < 1e-10);
    assert!((tracker.worst_case_short() - tracker.worst_case_short_locked()).abs() < 1e-10);
    assert!((tracker.effective_position() - (tracker.confirmed_position() + tracker.net_pending_exposure_locked())).abs() < 1e-10);
}

#[test]
fn test_cancel_without_open_ack_falls_back_to_client_order_id() {
    let tracker = make_tracker();

    tracker.start_tracking(6201, OrderSide::Buy, 3000.0, 0.10);

    let canceled =
        ShmPrivateEventV2::order_canceled(1, 2, 1, 6801, 6201, 0, 0.10, 0);
    let _ = tracker.apply_event(&canceled);

    assert_eq!(tracker.active_order_count(), 0);
    assert!((tracker.net_pending_exposure() - 0.0).abs() < 1e-10);
    assert!((tracker.worst_case_long() - 0.0).abs() < 1e-10);

    let state = tracker.state.read();
    let completed = state
        .completed_orders
        .get(&6201)
        .expect("pending-create order should move to completed on cancel");
    assert_eq!(completed.lifecycle, OrderLifecycle::Canceled);
}

#[tokio::test]
async fn test_reconcile_with_exchange_removes_stale_open_orders_and_exposure() {
    let tracker = make_tracker();

    tracker.start_tracking(7001, OrderSide::Buy, 3000.0, 0.05);
    tracker.start_tracking(7002, OrderSide::Sell, 3010.0, 0.04);

    let created_bid =
        ShmPrivateEventV2::order_created(1, 2, 1, 9701, 7001, 7101, 3000.0, 0.05, false, 0);
    let created_ask =
        ShmPrivateEventV2::order_created(2, 2, 1, 9702, 7002, 7102, 3010.0, 0.04, true, 0);
    let _ = tracker.apply_event(&created_bid);
    let _ = tracker.apply_event(&created_ask);

    let exchange = MockExchange {
        active_orders: Ok(vec![OrderInfo {
            order_id: "9702".to_string(),
            client_order_index: 7002,
            side: Side::Sell,
            price: 3010.0,
            size: 0.04,
            filled: 0.0,
        }]),
    };

    let stale_count = tracker.reconcile_with_exchange(&exchange).await.unwrap();

    assert_eq!(stale_count, 1);
    assert_eq!(tracker.active_order_count(), 1);
    assert!((tracker.worst_case_long() - 0.0).abs() < 1e-10);
    assert!((tracker.worst_case_short() - (-0.04)).abs() < 1e-10);

    let state = tracker.state.read();
    let stale = state
        .completed_orders
        .get(&7001)
        .expect("stale order should move to completed");
    assert_eq!(stale.lifecycle, OrderLifecycle::Canceled);
}

#[tokio::test]
async fn test_reconcile_with_exchange_propagates_exchange_errors() {
    let tracker = make_tracker();
    tracker.start_tracking(8001, OrderSide::Buy, 3000.0, 0.05);

    let exchange = MockExchange {
        active_orders: Err("exchange unavailable".to_string()),
    };

    let err = tracker
        .reconcile_with_exchange(&exchange)
        .await
        .expect_err("reconcile should bubble up exchange error");

    assert!(err.to_string().contains("exchange unavailable"));
    assert_eq!(tracker.active_order_count(), 1);
    assert!((tracker.worst_case_long() - 0.05).abs() < 1e-10);
}

#[tokio::test]
async fn test_reconcile_with_exchange_removes_stale_pending_create_orders() {
    let tracker = make_tracker();
    tracker.start_tracking(8101, OrderSide::Buy, 3000.0, 0.05);

    {
        let mut state = tracker.state.write();
        let order = state
            .active_orders
            .get_mut(&8101)
            .expect("pending create should exist");
        order.created_at = std::time::Instant::now() - Duration::from_secs(5);
        order.last_update = order.created_at;
    }

    let exchange = MockExchange {
        active_orders: Ok(vec![]),
    };

    let stale_count = tracker.reconcile_with_exchange(&exchange).await.unwrap();

    assert_eq!(stale_count, 1);
    assert_eq!(tracker.active_order_count(), 0);
    assert!((tracker.net_pending_exposure() - 0.0).abs() < 1e-10);

    let state = tracker.state.read();
    let completed = state
        .completed_orders
        .get(&8101)
        .expect("stale pending-create order should move to completed");
    assert_eq!(completed.lifecycle, OrderLifecycle::Rejected);
}

#[tokio::test]
async fn test_reconcile_with_exchange_rebinds_pending_create_and_reverts_stuck_pending_cancel() {
    let tracker = make_tracker();
    tracker.start_tracking(8201, OrderSide::Buy, 3000.0, 0.05);
    tracker.start_tracking(8202, OrderSide::Sell, 3010.0, 0.04);

    let created =
        ShmPrivateEventV2::order_created(1, 2, 1, 9822, 8202, 7202, 3010.0, 0.04, true, 0);
    let _ = tracker.apply_event(&created);
    tracker.mark_pending_cancel(8202);

    {
        let mut state = tracker.state.write();
        let order = state
            .active_orders
            .get_mut(&8202)
            .expect("pending cancel should exist");
        order.last_update = std::time::Instant::now() - Duration::from_secs(5);
    }

    let exchange = MockExchange {
        active_orders: Ok(vec![
            OrderInfo {
                order_id: "9821".to_string(),
                client_order_index: 8201,
                side: Side::Buy,
                price: 3000.0,
                size: 0.05,
                filled: 0.0,
            },
            OrderInfo {
                order_id: "9822".to_string(),
                client_order_index: 8202,
                side: Side::Sell,
                price: 3010.0,
                size: 0.04,
                filled: 0.0,
            },
        ]),
    };

    let stale_count = tracker.reconcile_with_exchange(&exchange).await.unwrap();

    assert_eq!(stale_count, 0);
    assert_eq!(tracker.active_order_count(), 2);
    let state = tracker.state.read();
    let pending_create = state
        .active_orders
        .get(&8201)
        .expect("pending create should still be active");
    assert_eq!(pending_create.lifecycle, OrderLifecycle::Open);
    assert_eq!(pending_create.exchange_order_id, Some(9821));

    let pending_cancel = state
        .active_orders
        .get(&8202)
        .expect("pending cancel should still be active");
    assert_eq!(pending_cancel.lifecycle, OrderLifecycle::Open);
}
