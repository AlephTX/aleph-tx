use super::tests::{START, book, config, maker};
use super::*;
use crate::exchange::OrderParams;

fn cohort(stats: &serde_json::Value, name: &str) -> serde_json::Value {
    stats["cohorts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["source_timing"] == name)
        .unwrap()
        .clone()
}

async fn delayed_pre_request_fill(
    diagnose: bool,
) -> (serde_json::Value, Option<serde_json::Value>) {
    let _clock = ReplayClock::open(START, diagnose);
    let mut cfg = config();
    cfg.diagnose_cross_market = diagnose;
    let tracker = Arc::new(OrderTracker::new());
    let mut venue = SimVenue::new(Arc::clone(&tracker), 0.4);
    venue.recorded = Some(Matching::new(&cfg));
    let matching = venue.recorded.as_ref().unwrap();
    matching.on_book(book());
    venue.execute_batch(vec![maker()]).await.unwrap();
    tokio::time::advance(Duration::from_millis(400)).await;
    let source_ms = now_ms() - 200;
    venue.cancel_all().await.unwrap();
    tokio::time::advance(Duration::from_millis(100)).await;
    matching
        .on_trade(&venue, now_ms(), source_ms, 100.0, 0.05, false)
        .unwrap();
    tokio::time::advance(Duration::from_millis(1_000)).await;
    matching.on_book(book());
    let ledger = *venue.ledger.lock();
    let market = matching.market.lock();
    let stats = market.attribution.as_ref().map(|stats| {
        assert_eq!(stats.all_maker_fills.cancel_pending_fills, 1);
        assert_eq!(stats.all_maker_fills.marked_fills_1s, 1);
        serde_json::to_value(&stats.cancel_timing).unwrap()
    });
    let economy = serde_json::json!({
        "position_e8": ledger.position_e8,
        "cash": ledger.cash,
        "maker_notional": ledger.maker_notional,
        "taker_notional": ledger.taker_notional,
        "maker_fees": ledger.maker_fees,
        "taker_fees": ledger.taker_fees,
        "maker_fills": ledger.maker_fills,
        "taker_fills": ledger.taker_fills,
        "confirmed_position": tracker.confirmed_position(),
        "exposure_bounds": tracker.exposure_bounds(),
        "needs_reconciliation": tracker.needs_reconciliation(),
        "cash_flow": tracker.trade_cash_flow(),
        "resting_orders": venue.book.lock().len(),
        "late_cancel_fills": market.late_cancel_fills,
        "end_ms": now_ms(),
    });
    (economy, stats)
}

#[tokio::test(start_paused = true)]
async fn cancel_clock_diagnostics_preserve_the_delayed_fill_ledger() {
    let (control, no_stats) = delayed_pre_request_fill(false).await;
    let (observed, stats) = delayed_pre_request_fill(true).await;
    assert_eq!(control, observed);
    assert!(no_stats.is_none());
    let stats = stats.unwrap();
    assert_eq!(observed["maker_fills"], 1);
    assert_eq!(observed["late_cancel_fills"], 1);
    assert_eq!(stats["late_receipt_fills"], 1);
    assert_eq!(stats["source_clock_execution_verified"], false);
    assert_eq!(stats["guard_calibrated"], false);
    let before = cohort(&stats, "before_cancel_request");
    assert_eq!(before["fills"], 1);
    assert_eq!(before["markouts"][0]["marked_fills"], 1);
    assert_eq!(before["markouts"][0]["marked_turnover"], 5.0);
    assert_eq!(cohort(&stats, "during_cancel_window")["fills"], 0);
}

#[tokio::test(start_paused = true)]
async fn mixed_batch_uses_the_actual_slowest_action_deadline() {
    let _clock = ReplayClock::open(START, true);
    let mut cfg = config();
    cfg.diagnose_cross_market = true;
    cfg.taker_transport_ms = Some(1_200);
    let tracker = Arc::new(OrderTracker::new());
    let mut venue = SimVenue::new(tracker, 0.0);
    venue.recorded = Some(Matching::new(&cfg));
    let matching = venue.recorded.as_ref().unwrap();
    matching.on_book(book());
    venue.execute_batch(vec![maker()]).await.unwrap();
    let id = *venue.book.lock().keys().next().unwrap();
    let actions = vec![
        BatchAction::Cancel(id),
        BatchAction::Place(OrderParams {
            side: Side::Sell,
            price: 99.0,
            size: 0.05,
            order_type: OrderType::Ioc,
            reduce_only: true,
        }),
    ];
    assert_eq!(matching.batch_delay(&actions), Duration::from_millis(1_340));
    let execute = venue.execute_batch(actions);
    let fill = async {
        tokio::time::sleep(Duration::from_millis(800)).await;
        matching.on_book(book());
        matching
            .on_trade(&venue, now_ms(), now_ms(), 100.0, 0.05, false)
            .unwrap();
    };
    let (result, ()) = tokio::join!(execute, fill);
    result.unwrap();
    let state = matching.market.lock();
    let stats = serde_json::to_value(&state.attribution.as_ref().unwrap().cancel_timing).unwrap();
    assert_eq!(cohort(&stats, "during_cancel_window")["fills"], 1);
    assert_eq!(cohort(&stats, "after_cancel_effective")["fills"], 0);
    assert_eq!(stats["late_receipt_fills"], 0);
    assert!(
        stats["cancel_effective_to_receipt_ms"]["max_ms"]
            .as_i64()
            .unwrap()
            < -400
    );
}

#[tokio::test(start_paused = true)]
async fn repeated_cancel_keeps_first_request_and_earliest_effective_deadline() {
    let _clock = ReplayClock::open(START, true);
    let mut cfg = config();
    cfg.diagnose_cross_market = true;
    let tracker = Arc::new(OrderTracker::new());
    let mut venue = SimVenue::new(tracker, 0.0);
    venue.recorded = Some(Matching::new(&cfg));
    let matching = venue.recorded.as_ref().unwrap();
    matching.on_book(book());
    venue.execute_batch(vec![maker()]).await.unwrap();
    let id = *venue.book.lock().keys().next().unwrap();
    let first = now_ms();
    matching.record_cancel_with_delay(Some(id), Duration::from_millis(1_000));
    tokio::time::advance(Duration::from_millis(200)).await;
    matching.record_cancel_with_delay(Some(id), Duration::from_millis(100));
    let state = matching.market.lock();
    let window = state.queue[&id].cancel_window.unwrap();
    assert_eq!(window.requested_ms, first);
    assert_eq!(window.effective_ms, first + 300);
    assert_eq!(state.attribution.as_ref().unwrap().maker_cancel_requests, 1);
}

#[tokio::test(start_paused = true)]
async fn source_cohort_markouts_preserve_missing_labels() {
    let _clock = ReplayClock::open(START, true);
    let mut cfg = config();
    cfg.diagnose_cross_market = true;
    let matching = Matching::new(&cfg);
    let timing = cancel_timing::SourceTiming::RequestBoundaryOverlap;
    {
        let mut state = matching.market.lock();
        let stats = state.attribution.as_mut().unwrap();
        for (queue, horizon) in stats.pending_marks.iter_mut().zip(MARKOUT_HORIZONS_MS) {
            queue.push_back(PendingMarkout {
                due_ms: START + horizon,
                signed_quantity: -0.1,
                price: 100.0,
                changed_side: false,
                cancel_pending: true,
                cancel_source_timing: timing,
            });
        }
    }
    tokio::time::advance(Duration::from_millis(2_001)).await;
    matching.on_book(book());
    let mut state = matching.market.lock();
    let stats = state.attribution.as_mut().unwrap();
    // Simulate the report cutoff while the 5s/30s marks still lack a future
    // label. Missing marks stay visible; they are never scored as zero edge.
    for (horizon, pending) in stats.pending_marks.iter().enumerate() {
        for fill in pending {
            stats
                .cancel_timing
                .pending_at_cutoff(horizon, fill.cancel_source_timing);
        }
    }
    let report = serde_json::to_value(&stats.cancel_timing).unwrap();
    let selected = cohort(&report, "request_boundary_overlap");
    assert_eq!(selected["markouts"][0]["expired_fills"], 1);
    assert_eq!(selected["markouts"][0]["marked_fills"], 0);
    assert_eq!(selected["markouts"][1]["pending_at_cutoff"], 1);
    assert_eq!(selected["markouts"][2]["pending_at_cutoff"], 1);
}
