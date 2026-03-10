//! World-Class Per-Order State Machine (OrderTracker v5.0)
//!
//! Replaces the old ShadowLedger's dual-accumulator model with a per-order
//! state machine that eliminates state drift, ID mismatch, and net-value
//! masking of bilateral exposure.
//!
//! # Architecture
//!
//! - Uses `RwLock<TrackerState>` (NOT DashMap) for O(N) atomic traversal
//! - Single read-lock for worst_case_long/short calculation (<20ns for N<20)
//! - Per-order lifecycle: PendingCreate → Open → PartiallyFilled → Filled
//! - Delayed binding: client_order_id (local) → exchange_order_id (exchange)
//! - TTL cache for completed orders to handle late fill events
//!
//! # Design Principles (Citadel/Jump/Hummingbot)
//!
//! 1. Event Sourcing: all state changes driven by events
//! 2. Optimistic Concurrency: register before API call, rollback on failure
//! 3. Defense in Depth: tracker + exchange WS + drift detection
//! 4. Worst-Case Risk: check max exposure per side, not net

use crossbeam::utils::CachePadded;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use crate::types::ShmPrivateEventV2;

const POS_SCALE: f64 = 1e8;

// ─── Order Side ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderSide {
    Buy,
    Sell,
}

impl OrderSide {
    #[inline]
    pub fn sign(&self) -> f64 {
        match self {
            Self::Buy => 1.0,
            Self::Sell => -1.0,
        }
    }
}

impl std::fmt::Display for OrderSide {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Buy => write!(f, "bid"),
            Self::Sell => write!(f, "ask"),
        }
    }
}

// ─── Order Lifecycle State Machine ───────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderLifecycle {
    /// Sent to exchange, awaiting ACK
    PendingCreate,
    /// Exchange confirmed, sitting in orderbook
    Open,
    /// Partially filled
    PartiallyFilled,
    /// Fully filled
    Filled,
    /// Cancel request sent, awaiting confirmation
    PendingCancel,
    /// Canceled by exchange
    Canceled,
    /// Rejected by exchange
    Rejected,
}

impl OrderLifecycle {
    /// Whether this order still has pending exposure
    #[inline]
    pub fn has_pending_exposure(&self) -> bool {
        matches!(
            self,
            Self::PendingCreate | Self::Open | Self::PartiallyFilled | Self::PendingCancel
        )
    }

    /// Whether this order has reached a terminal state
    #[inline]
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Filled | Self::Canceled | Self::Rejected)
    }
}

// ─── Tracked Order ───────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct TrackedOrder {
    /// Client order ID (locally generated, primary key)
    pub client_order_id: i64,
    /// Exchange order ID (delayed binding, set on OrderCreated event)
    pub exchange_order_id: Option<u64>,
    /// Exchange order index (used for cancel API)
    pub order_index: Option<i64>,
    /// Order side
    pub side: OrderSide,
    /// Order price
    pub price: f64,
    /// Original order size
    pub original_size: f64,
    /// Cumulative filled size
    pub filled_size: f64,
    /// Lifecycle state
    pub lifecycle: OrderLifecycle,
    /// Creation time
    pub created_at: Instant,
    /// Last update time
    pub last_update: Instant,
    /// Cumulative fees
    pub total_fee: f64,
    /// Fill records: (trade_id, fill_size, fill_price)
    pub fills: Vec<(u64, f64, f64)>,
}

impl TrackedOrder {
    #[inline]
    pub fn remaining_size(&self) -> f64 {
        (self.original_size - self.filled_size).max(0.0)
    }

    #[inline]
    pub fn pending_exposure(&self) -> f64 {
        if !self.lifecycle.has_pending_exposure() {
            return 0.0;
        }
        self.side.sign() * self.remaining_size()
    }

    pub fn average_fill_price(&self) -> Option<f64> {
        if self.filled_size < 1e-12 {
            return None;
        }
        let total_value: f64 = self.fills.iter().map(|(_, sz, px)| sz * px).sum();
        Some(total_value / self.filled_size)
    }
}

// ─── Tracker State (protected by single RwLock) ─────────────────────────────

pub struct TrackerState {
    /// Active orders: client_order_id → TrackedOrder
    pub active_orders: HashMap<i64, TrackedOrder>,
    /// Reverse mapping: exchange_order_id → client_order_id
    pub exchange_to_client: HashMap<u64, i64>,
    /// Completed orders TTL cache (for late events)
    pub completed_orders: HashMap<i64, TrackedOrder>,
}

impl TrackerState {
    fn new() -> Self {
        Self {
            active_orders: HashMap::with_capacity(64),
            exchange_to_client: HashMap::with_capacity(64),
            completed_orders: HashMap::with_capacity(128),
        }
    }
}

// ─── Order Tracker ───────────────────────────────────────────────────────────

pub struct OrderTracker {
    /// All mutable order state behind a single RwLock for atomic traversal
    pub state: RwLock<TrackerState>,
    /// Confirmed position (only driven by fill events, ground truth)
    pub confirmed_position: CachePadded<AtomicI64>,
    /// Realized PnL in USD (scaled by POS_SCALE)
    pub realized_pnl: CachePadded<AtomicI64>,
    /// Last processed event sequence number
    pub last_sequence: AtomicI64,
}

impl OrderTracker {
    pub fn new() -> Self {
        Self {
            state: RwLock::new(TrackerState::new()),
            confirmed_position: CachePadded::new(AtomicI64::new(0)),
            realized_pnl: CachePadded::new(AtomicI64::new(0)),
            last_sequence: AtomicI64::new(0),
        }
    }

    // ─── Read Interface (lock-free atomics + single read-lock) ───────────

    /// Confirmed position (ground truth from fill events)
    #[inline]
    pub fn confirmed_position(&self) -> f64 {
        self.confirmed_position.load(Ordering::Acquire) as f64 / POS_SCALE
    }

    /// Net pending exposure from all active orders (requires read lock)
    pub fn net_pending_exposure(&self) -> f64 {
        let state = self.state.read();
        state
            .active_orders
            .values()
            .map(|o| o.pending_exposure())
            .sum()
    }

    /// Effective position = confirmed + net pending
    #[inline]
    pub fn effective_position(&self) -> f64 {
        self.confirmed_position() + self.net_pending_exposure()
    }

    /// Worst-case long exposure: confirmed + all active buy remaining
    /// Used for pre-trade risk check before placing bids
    pub fn worst_case_long(&self) -> f64 {
        let state = self.state.read();
        self.confirmed_position()
            + state
                .active_orders
                .values()
                .filter(|o| o.side == OrderSide::Buy && o.lifecycle.has_pending_exposure())
                .map(|o| o.remaining_size())
                .sum::<f64>()
    }

    /// Worst-case short exposure: confirmed - all active sell remaining
    /// Used for pre-trade risk check before placing asks
    pub fn worst_case_short(&self) -> f64 {
        let state = self.state.read();
        self.confirmed_position()
            - state
                .active_orders
                .values()
                .filter(|o| o.side == OrderSide::Sell && o.lifecycle.has_pending_exposure())
                .map(|o| o.remaining_size())
                .sum::<f64>()
    }

    /// Realized PnL in USD
    #[inline]
    pub fn realized_pnl(&self) -> f64 {
        self.realized_pnl.load(Ordering::Acquire) as f64 / POS_SCALE
    }

    /// Number of active orders
    pub fn active_order_count(&self) -> usize {
        self.state.read().active_orders.len()
    }

    /// Get all active order client_order_ids
    pub fn active_cois(&self) -> Vec<i64> {
        self.state.read().active_orders.keys().copied().collect()
    }

    /// Get order_index for a tracked order (used for cancel API)
    pub fn get_order_index(&self, client_order_id: i64) -> Option<i64> {
        let state = self.state.read();
        state
            .active_orders
            .get(&client_order_id)
            .and_then(|o| o.order_index)
    }

    // ─── Write Interface ─────────────────────────────────────────────────

    /// Register a new order before sending to exchange (optimistic accounting)
    pub fn start_tracking(
        &self,
        client_order_id: i64,
        side: OrderSide,
        price: f64,
        size: f64,
    ) {
        let order = TrackedOrder {
            client_order_id,
            exchange_order_id: None,
            order_index: None,
            side,
            price,
            original_size: size,
            filled_size: 0.0,
            lifecycle: OrderLifecycle::PendingCreate,
            created_at: Instant::now(),
            last_update: Instant::now(),
            total_fee: 0.0,
            fills: Vec::new(),
        };

        let mut state = self.state.write();
        state.active_orders.insert(client_order_id, order);

        tracing::debug!(
            "📝 Order tracking started: coi={} side={} price={:.2} size={:.4}",
            client_order_id,
            side,
            price,
            size
        );
    }

    /// Mark order as failed (API call failed, rollback optimistic accounting)
    pub fn mark_failed(&self, client_order_id: i64) {
        let mut state = self.state.write();
        if let Some(mut order) = state.active_orders.remove(&client_order_id) {
            order.lifecycle = OrderLifecycle::Rejected;
            order.last_update = Instant::now();
            state.completed_orders.insert(client_order_id, order);
        }
        tracing::warn!("❌ Order marked failed: coi={}", client_order_id);
    }

    /// Mark order as pending cancel (cancel request sent)
    pub fn mark_pending_cancel(&self, client_order_id: i64) {
        let mut state = self.state.write();
        if let Some(order) = state.active_orders.get_mut(&client_order_id) {
            order.lifecycle = OrderLifecycle::PendingCancel;
            order.last_update = Instant::now();
        }
    }

    /// Force sync confirmed position from exchange REST API
    pub fn force_sync_position(&self, exchange_position: f64) -> f64 {
        let current = self.confirmed_position();
        let delta = exchange_position - current;

        if delta.abs() > 1e-8 {
            tracing::warn!(
                "⚠️  Position drift: tracker={:.6} exchange={:.6} delta={:.6}",
                current,
                exchange_position,
                delta
            );
            let scaled = (exchange_position * POS_SCALE) as i64;
            self.confirmed_position.store(scaled, Ordering::Release);
        }

        delta
    }

    /// GC completed orders older than TTL
    pub fn gc_completed_orders(&self, ttl: Duration) {
        let cutoff = Instant::now() - ttl;
        let mut state = self.state.write();
        state
            .completed_orders
            .retain(|_, order| order.last_update > cutoff);
    }

    /// Cancel all active orders in tracker (when exchange cancel_all is called)
    /// Moves all active orders to completed with Canceled lifecycle
    pub fn cancel_all_active(&self) -> usize {
        let mut state = self.state.write();
        let drained: Vec<_> = state.active_orders.drain().collect();
        let count = drained.len();
        let now = Instant::now();
        for (coi, mut order) in drained {
            order.lifecycle = OrderLifecycle::Canceled;
            order.last_update = now;
            state.completed_orders.insert(coi, order);
        }
        count
    }
}

impl OrderTracker {
    // ─── Event Processing (State Machine Transitions) ────────────────────

    /// Process a V2 event from SHM ring buffer
    pub fn apply_event(&self, event: &ShmPrivateEventV2) -> anyhow::Result<()> {
        // Sequence gap detection
        let prev_seq = self.last_sequence.load(Ordering::Acquire);
        if event.sequence <= prev_seq as u64 && prev_seq > 0 {
            tracing::warn!(
                "Out-of-order event: seq={} prev={}",
                event.sequence,
                prev_seq
            );
            return Ok(()); // Skip duplicate
        }
        if event.sequence > prev_seq as u64 + 1 && prev_seq > 0 {
            let gap = event.sequence - prev_seq as u64 - 1;
            tracing::error!(
                "🚨 Event gap: expected={} got={} gap={}",
                prev_seq + 1,
                event.sequence,
                gap
            );
        }
        self.last_sequence
            .store(event.sequence as i64, Ordering::Release);

        match event.event_type {
            1 => self.handle_order_created(event),
            2 => self.handle_order_filled(event),
            3 => self.handle_order_canceled(event),
            4 => self.handle_order_rejected(event),
            _ => {
                tracing::warn!("Unknown event type: {}", event.event_type);
                Ok(())
            }
        }
    }

    fn handle_order_created(&self, event: &ShmPrivateEventV2) -> anyhow::Result<()> {
        let client_id = event.client_order_id;
        let mut state = self.state.write();

        if let Some(order) = state.active_orders.get_mut(&client_id) {
            // Delayed binding: attach exchange IDs to locally registered order
            order.exchange_order_id = Some(event.exchange_order_id);
            order.order_index = Some(event.order_index);
            order.lifecycle = OrderLifecycle::Open;
            order.last_update = Instant::now();

            // Build reverse mapping
            state
                .exchange_to_client
                .insert(event.exchange_order_id, client_id);

            tracing::debug!(
                "✅ Order confirmed: coi={} exch_id={} order_idx={}",
                client_id,
                event.exchange_order_id,
                event.order_index
            );
        } else {
            // Auto-register untracked order (e.g. from restart, or manual order)
            let side = if event.is_ask != 0 {
                OrderSide::Sell
            } else {
                OrderSide::Buy
            };
            let order = TrackedOrder {
                client_order_id: client_id,
                exchange_order_id: Some(event.exchange_order_id),
                order_index: Some(event.order_index),
                side,
                price: event.order_price,
                original_size: event.original_size,
                filled_size: 0.0,
                lifecycle: OrderLifecycle::Open,
                created_at: Instant::now(),
                last_update: Instant::now(),
                total_fee: 0.0,
                fills: Vec::new(),
            };
            state
                .exchange_to_client
                .insert(event.exchange_order_id, client_id);
            state.active_orders.insert(client_id, order);

            tracing::warn!(
                "⚠️  Auto-registered untracked order: coi={} exch_id={}",
                client_id,
                event.exchange_order_id
            );
        }

        Ok(())
    }

    fn handle_order_filled(&self, event: &ShmPrivateEventV2) -> anyhow::Result<()> {
        let mut state = self.state.write();

        // Look up client_id via exchange_order_id reverse mapping
        let client_id = state
            .exchange_to_client
            .get(&event.exchange_order_id)
            .copied();

        if let Some(cid) = client_id {
            let (is_filled, side) = if let Some(order) = state.active_orders.get_mut(&cid) {
                // Deduplicate fills by trade_id
                if event.trade_id != 0
                    && order.fills.iter().any(|(tid, _, _)| *tid == event.trade_id)
                {
                    tracing::warn!(
                        "Duplicate fill ignored: trade_id={} coi={}",
                        event.trade_id,
                        cid
                    );
                    return Ok(());
                }

                order.filled_size += event.fill_size;
                order.total_fee += event.fee_paid;
                order.last_update = Instant::now();
                order
                    .fills
                    .push((event.trade_id, event.fill_size, event.fill_price));

                if event.remaining_size < 1e-12 {
                    order.lifecycle = OrderLifecycle::Filled;
                } else {
                    order.lifecycle = OrderLifecycle::PartiallyFilled;
                }

                (order.lifecycle == OrderLifecycle::Filled, order.side)
            } else {
                // Not in active_orders — might be in completed (late event)
                if let Some(order) = state.completed_orders.get_mut(&cid) {
                    order.filled_size += event.fill_size;
                    order.total_fee += event.fee_paid;
                    order
                        .fills
                        .push((event.trade_id, event.fill_size, event.fill_price));
                    tracing::warn!(
                        "Late fill on completed order: coi={} size={:.4}",
                        cid,
                        event.fill_size
                    );
                }
                // Still update confirmed_position for late fills
                let side = if event.is_ask != 0 {
                    OrderSide::Sell
                } else {
                    OrderSide::Buy
                };
                let signed = side.sign() * event.fill_size;
                let delta = (signed * POS_SCALE) as i64;
                self.confirmed_position.fetch_add(delta, Ordering::AcqRel);
                return Ok(());
            };

            // Move to completed if fully filled
            if is_filled {
                let completed = state.active_orders.remove(&cid);
                if let Some(completed) = completed {
                    state.completed_orders.insert(cid, completed);
                }
                state.exchange_to_client.remove(&event.exchange_order_id);
            }

            // Update confirmed_position (ground truth)
            let signed = side.sign() * event.fill_size;
            let delta = (signed * POS_SCALE) as i64;
            self.confirmed_position.fetch_add(delta, Ordering::AcqRel);

            // Update realized PnL
            let pnl_delta = match side {
                OrderSide::Buy => -(event.fill_price * event.fill_size + event.fee_paid),
                OrderSide::Sell => event.fill_price * event.fill_size - event.fee_paid,
            };
            let pnl_scaled = (pnl_delta * POS_SCALE) as i64;
            self.realized_pnl.fetch_add(pnl_scaled, Ordering::AcqRel);

            tracing::info!(
                "💰 Fill: coi={} side={} size={:.4} price={:.2} remaining={:.4} pos={:.4}",
                cid,
                side,
                event.fill_size,
                event.fill_price,
                event.remaining_size,
                self.confirmed_position()
            );
        } else {
            // Completely untracked fill (e.g. order from before restart)
            let side = if event.is_ask != 0 {
                OrderSide::Sell
            } else {
                OrderSide::Buy
            };
            let signed = side.sign() * event.fill_size;
            let delta = (signed * POS_SCALE) as i64;
            self.confirmed_position.fetch_add(delta, Ordering::AcqRel);

            tracing::warn!(
                "⚠️  Untracked fill: exch_id={} side={} size={:.4} price={:.2}",
                event.exchange_order_id,
                side,
                event.fill_size,
                event.fill_price
            );
        }

        Ok(())
    }

    fn handle_order_canceled(&self, event: &ShmPrivateEventV2) -> anyhow::Result<()> {
        let mut state = self.state.write();

        let client_id = state
            .exchange_to_client
            .get(&event.exchange_order_id)
            .copied();

        if let Some(cid) = client_id {
            if let Some(order) = state.active_orders.get_mut(&cid) {
                tracing::info!(
                    "🚫 Order canceled: coi={} side={} remaining={:.4}",
                    cid,
                    order.side,
                    order.remaining_size()
                );
                order.lifecycle = OrderLifecycle::Canceled;
                order.last_update = Instant::now();
            }

            // Move to completed
            if let Some(completed) = state.active_orders.remove(&cid) {
                state.completed_orders.insert(cid, completed);
                state.exchange_to_client.remove(&event.exchange_order_id);
            }
        }

        Ok(())
    }

    fn handle_order_rejected(&self, event: &ShmPrivateEventV2) -> anyhow::Result<()> {
        let client_id = event.client_order_id;
        let mut state = self.state.write();

        if let Some(order) = state.active_orders.get_mut(&client_id) {
            tracing::warn!(
                "🚫 Order rejected: coi={} side={} size={:.4}",
                client_id,
                order.side,
                order.original_size
            );
            order.lifecycle = OrderLifecycle::Rejected;
            order.last_update = Instant::now();
        }

        if let Some(completed) = state.active_orders.remove(&client_id) {
            state.completed_orders.insert(client_id, completed);
        }

        Ok(())
    }
}

impl Default for OrderTracker {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
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
            3000.0,
            0.05,
            0.0, // remaining = 0 (fully filled)
            0.01,
            false, // is_ask = false (buy)
            0,
            7001, // trade_id
        );

        // First we need to simulate OrderCreated to bind IDs
        let created_event = ShmPrivateEventV2::order_created(
            1,    // sequence (will be skipped as duplicate, but let's use different)
            2,
            1,
            9001, // exchange_order_id
            2001, // client_order_id
            5001, // order_index
            3000.0,
            0.05,
            false,
            0,
        );

        // Reset sequence for test
        tracker.last_sequence.store(0, Ordering::Release);

        let _ = tracker.apply_event(&created_event);

        // Also create the ask order
        let created_ask = ShmPrivateEventV2::order_created(
            2, 2, 1,
            9002, // exchange_order_id
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
        let created = ShmPrivateEventV2::order_created(
            1, 2, 1, 8001, 3001, 4001, 3000.0, 0.1, false, 0,
        );
        let _ = tracker.apply_event(&created);

        assert!((tracker.worst_case_long() - 0.1).abs() < 1e-10);

        // Simulate OrderCanceled
        let canceled = ShmPrivateEventV2::order_canceled(
            2, 2, 1, 8001, 3001, 4001, 0.1, 0,
        );
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
        let created = ShmPrivateEventV2::order_created(
            1, 2, 1, 7001, 4001, 6001, 3010.0, 0.10, true, 0,
        );
        let _ = tracker.apply_event(&created);

        // Partial fill: 0.04 of 0.10
        let fill = ShmPrivateEventV2::order_filled(
            2, 2, 1, 7001, 4001, 6001,
            3010.0, 0.04, 0.06, 0.005, true, 0, 9001,
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

        let created = ShmPrivateEventV2::order_created(
            1, 2, 1, 6601, 6001, 6501, 3000.0, 0.10, false, 0,
        );
        let _ = tracker.apply_event(&created);

        // First fill
        let fill1 = ShmPrivateEventV2::order_filled(
            2, 2, 1, 6601, 6001, 6501,
            3000.0, 0.05, 0.05, 0.005, false, 0, 8801, // trade_id = 8801
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
}
