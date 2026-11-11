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
        self.active_orders.clone().map_err(anyhow::Error::msg)
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
    let order = state
        .active_orders
        .get(&5501)
        .expect("order should remain active");
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
fn test_pending_cancel_partial_fill_then_canceled_preserves_position_and_clears_remaining_exposure()
{
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
    assert!(
        state
            .completed_orders
            .values()
            .all(|order| order.lifecycle == OrderLifecycle::Canceled)
    );
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
fn test_zero_remaining_fill_does_not_complete_until_original_size_is_reached() {
    let tracker = make_tracker();

    tracker.start_tracking(6003, OrderSide::Sell, 3005.0, 0.0156);

    let created =
        ShmPrivateEventV2::order_created(1, 2, 1, 6603, 6003, 6503, 3005.0, 0.0156, true, 0);
    let _ = tracker.apply_event(&created);

    let premature_terminal = ShmPrivateEventV2::order_filled(
        2, 2, 1, 6603, 6003, 6503, 3005.0, 0.0111, 0.0, 0.001, true, 0, 8803,
    );
    let _ = tracker.apply_event(&premature_terminal);

    assert_eq!(tracker.active_order_count(), 1);
    assert!((tracker.confirmed_position() - (-0.0111)).abs() < 1e-10);
    assert!((tracker.net_pending_exposure() - (-0.0045)).abs() < 1e-10);

    {
        let state = tracker.state.read();
        let active = state
            .active_orders
            .get(&6003)
            .expect("order should stay active");
        assert_eq!(active.lifecycle, OrderLifecycle::PartiallyFilled);
        assert!((active.remaining_size() - 0.0045).abs() < 1e-10);
    }

    let final_fill = ShmPrivateEventV2::order_filled(
        3, 2, 1, 6603, 6003, 6503, 3005.0, 0.0045, 0.0, 0.001, true, 0, 8804,
    );
    let _ = tracker.apply_event(&final_fill);

    assert_eq!(tracker.active_order_count(), 0);
    assert!((tracker.confirmed_position() - (-0.0156)).abs() < 1e-8);
    assert!((tracker.net_pending_exposure() - 0.0).abs() < 1e-10);
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
        state
            .active_orders
            .get(&6101)
            .and_then(|o| o.exchange_order_id),
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
    assert!(
        (tracker.effective_position()
            - (tracker.confirmed_position() + tracker.net_pending_exposure_locked()))
        .abs()
            < 1e-10
    );
}

#[test]
fn test_cancel_without_open_ack_falls_back_to_client_order_id() {
    let tracker = make_tracker();

    tracker.start_tracking(6201, OrderSide::Buy, 3000.0, 0.10);

    let canceled = ShmPrivateEventV2::order_canceled(1, 2, 1, 6801, 6201, 0, 0.10, 0);
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
async fn test_reconcile_absent_open_order_preserves_exposure_and_requests_cleanup() {
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

    assert_eq!(stale_count, 0);
    assert_eq!(tracker.active_order_count(), 2);
    assert!((tracker.worst_case_long() - 0.05).abs() < 1e-10);
    assert!((tracker.worst_case_short() - (-0.04)).abs() < 1e-10);
    assert!(tracker.needs_reconciliation());
    let state = tracker.state.read();
    assert!(state.active_orders[&7001].submission_uncertain);
    assert_eq!(state.active_orders[&7001].lifecycle, OrderLifecycle::Open);
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
async fn test_reconcile_absent_pending_create_preserves_exposure_and_requests_cleanup() {
    let tracker = make_tracker();
    tracker.start_tracking(8101, OrderSide::Buy, 3000.0, 0.05);

    {
        let mut state = tracker.state.write();
        let order = state
            .active_orders
            .get_mut(&8101)
            .expect("pending create should exist");
        order.created_at = tokio::time::Instant::now() - Duration::from_secs(5);
        order.last_update = order.created_at;
    }

    let exchange = MockExchange {
        active_orders: Ok(vec![]),
    };

    let stale_count = tracker.reconcile_with_exchange(&exchange).await.unwrap();

    assert_eq!(stale_count, 0);
    assert_eq!(tracker.active_order_count(), 1);
    assert!((tracker.net_pending_exposure() - 0.05).abs() < 1e-10);
    assert!(tracker.needs_reconciliation());
    let state = tracker.state.read();
    assert_eq!(
        state.active_orders[&8101].lifecycle,
        OrderLifecycle::PendingCreate
    );
    assert!(state.active_orders[&8101].submission_uncertain);
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
        order.last_update = tokio::time::Instant::now() - Duration::from_secs(5);
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

#[test]
fn test_startup_grace_ignores_untracked_open_events() {
    let tracker = make_tracker();

    let created =
        ShmPrivateEventV2::order_created(1, 2, 1, 990001, 880001, 770001, 3000.0, 0.05, false, 0);
    let _ = tracker.apply_event(&created);

    assert_eq!(tracker.active_order_count(), 0);
    assert!((tracker.net_pending_exposure() - 0.0).abs() < 1e-10);
}

#[test]
fn duplicate_registration_preserves_original_exposure() {
    let tracker = make_tracker();
    assert!(tracker.start_tracking(1, OrderSide::Buy, 100.0, 0.5));
    assert!(!tracker.start_tracking(1, OrderSide::Sell, 101.0, 4.0));
    assert_eq!(tracker.worst_case_long(), 0.5);
    assert_eq!(tracker.worst_case_short(), 0.0);
    assert_eq!(tracker.pending_buy_exposure(), 0.5);
    assert_eq!(tracker.pending_sell_exposure(), 0.0);
    tracker.mark_failed(1);
    assert!(!tracker.start_tracking(1, OrderSide::Buy, 100.0, 0.5));
}

#[test]
fn invalid_inputs_do_not_poison_positions_or_sequence() {
    let tracker = make_tracker();
    assert!(!tracker.start_tracking(1, OrderSide::Buy, f64::NAN, 1.0));
    assert!(!tracker.start_tracking(2, OrderSide::Buy, 100.0, -1.0));
    assert_eq!(tracker.active_order_count(), 0);
    tracker.force_sync_position(0.5);
    tracker.force_sync_position(f64::INFINITY);
    assert_eq!(tracker.confirmed_position(), 0.5);
    let mut event =
        ShmPrivateEventV2::order_filled(0, 2, 1, 10, 1, 10, 100.0, f64::NAN, 0.0, 0.0, false, 0, 1);
    assert!(tracker.apply_event(&event).is_err());
    assert_eq!(tracker.last_sequence.load(Ordering::Acquire), -1);
    event.fill_size = 0.1;
    tracker.apply_event(&event).unwrap();
    tracker.apply_event(&event).unwrap();
    assert_eq!(tracker.confirmed_position(), 0.6);
}

#[test]
fn ws_acknowledgment_wins_over_late_http_failure() {
    let tracker = make_tracker();
    tracker.start_tracking(1, OrderSide::Buy, 100.0, 0.5);
    tracker
        .apply_event(&ShmPrivateEventV2::order_created(
            0, 2, 1, 10, 1, 10, 100.0, 0.5, false, 0,
        ))
        .unwrap();
    tracker.mark_failed(1);
    assert_eq!(tracker.active_order_count(), 1);
    assert_eq!(tracker.worst_case_long(), 0.5);
}

#[test]
fn late_fill_without_client_id_is_accounted_and_deduplicated() {
    let tracker = make_tracker();
    tracker.start_tracking(1, OrderSide::Buy, 100.0, 0.5);
    tracker
        .apply_event(&ShmPrivateEventV2::order_created(
            0, 2, 1, 10, 1, 10, 100.0, 0.5, false, 0,
        ))
        .unwrap();
    tracker
        .apply_event(&ShmPrivateEventV2::order_canceled(
            1, 2, 1, 10, 1, 10, 0.5, 0,
        ))
        .unwrap();
    let mut event =
        ShmPrivateEventV2::order_filled(2, 2, 1, 10, 0, 10, 100.0, 0.1, 0.0, 0.02, false, 0, 99);
    tracker.apply_event(&event).unwrap();
    event.sequence += 1;
    tracker.apply_event(&event).unwrap();
    assert_eq!(tracker.confirmed_position(), 0.1);
    assert!((tracker.realized_pnl() + 10.02).abs() < 1e-8);
    assert_eq!(tracker.total_fill_stats(), (1, 0.02));
    assert_eq!(tracker.pending_buy_exposure(), 0.0);
    tracker.gc_completed_orders(Duration::ZERO);
    assert!(tracker.state.read().exchange_to_client.is_empty());
}

#[test]
fn untracked_owned_fill_is_deduplicated_and_keeps_remaining_risk() {
    let tracker = make_tracker();
    let mut event =
        ShmPrivateEventV2::order_filled(0, 2, 1, 10, 1, 10, 100.0, 0.1, 0.4, 0.02, false, 0, 99);
    tracker.apply_event(&event).unwrap();
    event.sequence += 1;
    tracker.apply_event(&event).unwrap();
    assert_eq!(tracker.confirmed_position(), 0.1);
    assert!((tracker.worst_case_long() - 0.5).abs() < 1e-8);
    assert_eq!(tracker.total_fill_stats(), (1, 0.02));
    assert!((tracker.realized_pnl() + 10.02).abs() < 1e-8);
}

#[test]
fn fill_preserves_pending_cancel_and_atomic_exposure_drains_exactly() {
    let tracker = make_tracker();
    tracker.start_tracking(1, OrderSide::Buy, 100.0, 0.0156);
    tracker.mark_pending_cancel(1);
    let first =
        ShmPrivateEventV2::order_filled(0, 2, 1, 10, 1, 10, 100.0, 0.0111, 0.0, 0.0, false, 0, 99);
    tracker.apply_event(&first).unwrap();
    assert_eq!(
        tracker.state.read().active_orders[&1].lifecycle,
        OrderLifecycle::PendingCancel
    );
    let last = ShmPrivateEventV2::order_filled(
        1, 2, 1, 10, 1, 10, 100.0, 0.0045, 0.001, 0.0, false, 0, 100,
    );
    tracker.apply_event(&last).unwrap();
    assert_eq!(tracker.pending_buy_exposure.load(Ordering::Acquire), 0);
    assert_eq!(tracker.active_order_count(), 0);
}

#[tokio::test]
async fn reconcile_cannot_release_uncertain_submission_from_absence_alone() {
    let tracker = make_tracker();
    tracker.start_tracking(1, OrderSide::Buy, 100.0, 0.5);
    tracker.mark_submission_uncertain(1);
    tracker
        .state
        .write()
        .active_orders
        .get_mut(&1)
        .unwrap()
        .created_at = Instant::now() - Duration::from_secs(30);
    let exchange = MockExchange {
        active_orders: Ok(vec![]),
    };
    assert_eq!(tracker.reconcile_with_exchange(&exchange).await.unwrap(), 0);
    assert_eq!(tracker.worst_case_long(), 0.5);
}

#[tokio::test]
async fn stale_rest_response_cannot_cancel_orders_updated_during_request() {
    struct DelayedExchange {
        started: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }
    #[async_trait]
    impl Exchange for DelayedExchange {
        async fn buy(&self, _: f64, _: f64) -> anyhow::Result<OrderResult> {
            unreachable!()
        }
        async fn sell(&self, _: f64, _: f64) -> anyhow::Result<OrderResult> {
            unreachable!()
        }
        async fn place_batch(&self, _: BatchOrderParams) -> anyhow::Result<BatchOrderResult> {
            unreachable!()
        }
        async fn cancel_order(&self, _: i64) -> anyhow::Result<()> {
            unreachable!()
        }
        async fn cancel_all(&self) -> anyhow::Result<u32> {
            unreachable!()
        }
        async fn get_active_orders(&self) -> anyhow::Result<Vec<OrderInfo>> {
            self.started.notify_one();
            self.release.notified().await;
            Ok(vec![])
        }
        async fn close_all_positions(&self, _: f64) -> anyhow::Result<()> {
            unreachable!()
        }
        async fn execute_batch(&self, _: Vec<BatchAction>) -> anyhow::Result<BatchResult> {
            unreachable!()
        }
        async fn get_account_stats(
            &self,
        ) -> anyhow::Result<crate::strategy::inventory_neutral_mm::AccountStats> {
            unreachable!()
        }
        fn limit_order_type(&self) -> OrderType {
            OrderType::PostOnly
        }
    }
    let tracker = std::sync::Arc::new(make_tracker());
    let exchange = std::sync::Arc::new(DelayedExchange {
        started: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    tracker.start_tracking(1, OrderSide::Buy, 100.0, 0.5);
    let worker_tracker = tracker.clone();
    let worker_exchange = exchange.clone();
    let worker = tokio::spawn(async move {
        worker_tracker
            .reconcile_with_exchange(worker_exchange.as_ref())
            .await
    });
    exchange.started.notified().await;
    tracker
        .apply_event(&ShmPrivateEventV2::order_created(
            0, 2, 1, 10, 1, 10, 100.0, 0.5, false, 0,
        ))
        .unwrap();
    exchange.release.notify_one();
    assert_eq!(worker.await.unwrap().unwrap(), 0);
    assert_eq!(tracker.active_order_count(), 1);
    assert_eq!(tracker.worst_case_long(), 0.5);
}

#[test]
fn event_gap_latches_reconciliation_until_explicit_cleanup() {
    let tracker = make_tracker();
    tracker
        .apply_event(&ShmPrivateEventV2::order_filled(
            0, 2, 1, 10, 1, 10, 100.0, 0.1, 0.0, 0.0, false, 0, 99,
        ))
        .unwrap();
    assert!(!tracker.needs_reconciliation());
    tracker
        .apply_event(&ShmPrivateEventV2::order_filled(
            2, 2, 1, 11, 2, 11, 100.0, 0.2, 0.0, 0.0, false, 0, 100,
        ))
        .unwrap();
    assert!(tracker.needs_reconciliation());
    tracker.clear_reconciliation_required();
    assert!(!tracker.needs_reconciliation());
    tracker.reset_event_sequence();
    assert!(tracker.needs_reconciliation());
    assert_eq!(tracker.last_sequence.load(Ordering::Acquire), -1);
}

#[test]
fn lifetime_fill_statistics_survive_completed_order_gc() {
    let tracker = make_tracker();
    let fill =
        ShmPrivateEventV2::order_filled(0, 2, 1, 10, 1, 10, 100.0, 0.1, 0.0, -0.01, false, 0, 99);
    tracker.apply_event(&fill).unwrap();
    assert_eq!(tracker.total_fill_stats(), (1, -0.01));
    tracker.gc_completed_orders(Duration::ZERO);
    assert!(tracker.state.read().completed_orders.is_empty());
    assert_eq!(tracker.total_fill_stats(), (1, -0.01));
}

#[test]
fn cleanup_cannot_acknowledge_an_integrity_failure_that_arrived_during_await() {
    let tracker = make_tracker();
    tracker.require_reconciliation();
    let cleanup_epoch = tracker.reconciliation_epoch();
    tracker.require_reconciliation();
    assert!(!tracker.clear_reconciliation_if_epoch(cleanup_epoch));
    assert!(tracker.needs_reconciliation());
    assert!(tracker.clear_reconciliation_if_epoch(tracker.reconciliation_epoch()));
    assert!(!tracker.needs_reconciliation());
}

#[test]
fn cleanup_commit_is_atomic_and_preserves_risk_when_epoch_changed() {
    let tracker = make_tracker();
    tracker.start_tracking(1, OrderSide::Buy, 100.0, 0.5);
    tracker.require_reconciliation();
    let old_epoch = tracker.reconciliation_epoch();
    tracker.require_reconciliation();
    assert!(!tracker.confirm_cleanup(old_epoch, 0.0));
    assert_eq!(tracker.active_order_count(), 1);
    assert_eq!(tracker.worst_case_long(), 0.5);
    assert!(!tracker.confirm_cleanup(tracker.reconciliation_epoch(), f64::NAN));
    assert!(tracker.confirm_cleanup(tracker.reconciliation_epoch(), 0.01));
    assert_eq!(tracker.active_order_count(), 0);
    assert_eq!(tracker.pending_buy_exposure(), 0.0);
    assert_eq!(tracker.confirmed_position(), 0.01);
    assert!(!tracker.needs_reconciliation());
    tracker.require_reconciliation();
    assert!(tracker.needs_reconciliation());
}

#[test]
fn late_fills_cannot_change_the_rebased_position_even_after_order_gc() {
    let tracker = make_tracker();
    assert!(tracker.start_tracking(1, OrderSide::Buy, 100.0, 0.1));
    assert!(tracker.confirm_cleanup(tracker.reconciliation_epoch(), 0.0));
    let mut late =
        ShmPrivateEventV2::order_filled(0, 2, 1, 10, 1, 10, 100.0, 0.1, 0.0, -0.01, false, 0, 99);
    tracker.apply_event(&late).unwrap();
    assert_eq!(tracker.confirmed_position(), 0.0);
    assert_eq!(tracker.total_fill_stats(), (0, 0.0));
    assert!(tracker.needs_reconciliation());
    assert_eq!(tracker.recovery_cois(), BTreeSet::from([1]));
    let epoch = tracker.reconciliation_epoch();
    assert!(!tracker.confirm_cleanup(epoch, 0.0));
    assert!(!tracker.clear_reconciliation_if_epoch(epoch));
    assert!(!tracker.confirm_recovered_flat(epoch, &BTreeSet::new()));
    let proven = BTreeSet::from([1]);
    assert!(tracker.confirm_recovered_flat(epoch, &proven));

    tracker.gc_completed_orders(Duration::ZERO);
    late.sequence = 1;
    tracker.apply_event(&late).unwrap();
    assert!(tracker.needs_reconciliation());
    assert_eq!(tracker.worst_case_long(), 0.0);
    assert!(!tracker.confirm_recovered_flat(epoch, &proven));
    assert!(tracker.confirm_recovered_flat(tracker.reconciliation_epoch(), &proven));

    // A current order still credits each fill exactly once, even before ACK.
    assert!(tracker.start_tracking(2, OrderSide::Buy, 100.0, 0.1));
    let mut fill =
        ShmPrivateEventV2::order_filled(2, 2, 1, 20, 2, 20, 100.0, 0.1, 0.0, -0.01, false, 0, 100);
    tracker.apply_event(&fill).unwrap();
    fill.sequence = 3;
    tracker.apply_event(&fill).unwrap();
    assert!(!tracker.needs_reconciliation());
    assert_eq!(tracker.confirmed_position(), 0.1);
    assert_eq!(tracker.total_fill_stats(), (1, -0.01));
}

#[test]
fn recent_fill_ring_exposes_side_price_and_sequence() {
    let tracker = OrderTracker::new();
    assert!(tracker.fill_at(0).is_none());
    assert!(tracker.fill_at(1).is_none());
    tracker.start_tracking(41, OrderSide::Sell, 2500.0, 0.02);
    let created = ShmPrivateEventV2::order_created(1, 2, 1, 777, 41, 5, 2500.0, 0.02, true, 0);
    tracker.apply_event(&created).unwrap();
    let fill =
        ShmPrivateEventV2::order_filled(2, 2, 1, 777, 41, 5, 2500.5, 0.02, 0.0, 0.0, true, 0, 9901);
    tracker.apply_event(&fill).unwrap();
    let print = tracker.fill_at(1).expect("first fill retained");
    assert_eq!(print.sequence, 1);
    assert_eq!(print.side, OrderSide::Sell);
    assert_eq!(print.price, 2500.5);
    assert!(tracker.fill_at(2).is_none());
    // A sequence that aliases into the same slot but was overwritten is rejected.
    assert!(
        tracker
            .fill_at(1 + crate::order_tracker::RECENT_FILLS as u64)
            .is_none()
    );
}
