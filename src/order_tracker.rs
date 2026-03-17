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

use crate::exchange::Exchange;
use crossbeam::utils::CachePadded;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use crate::types::ShmPrivateEventV2;

const POS_SCALE: f64 = 1e8;
const PENDING_CREATE_RECONCILE_GRACE: Duration = Duration::from_secs(3);
const PENDING_CANCEL_RECONCILE_GRACE: Duration = Duration::from_secs(3);
const FILL_COMPLETION_EPS: f64 = 1e-9;
const STARTUP_AUTO_REGISTER_GRACE: Duration = Duration::from_secs(20);

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
    /// Phase 2 optimization: Incremental pending buy exposure (×POS_SCALE)
    /// Sum of all active buy orders' remaining_size
    pub pending_buy_exposure: CachePadded<AtomicI64>,
    /// Phase 2 optimization: Incremental pending sell exposure (×POS_SCALE)
    /// Sum of all active sell orders' remaining_size
    pub pending_sell_exposure: CachePadded<AtomicI64>,
    /// Startup time used to suppress stale open-event auto-registration during cleanup
    started_at: Instant,
}

impl OrderTracker {
    pub fn new() -> Self {
        Self {
            state: RwLock::new(TrackerState::new()),
            confirmed_position: CachePadded::new(AtomicI64::new(0)),
            realized_pnl: CachePadded::new(AtomicI64::new(0)),
            last_sequence: AtomicI64::new(0),
            pending_buy_exposure: CachePadded::new(AtomicI64::new(0)),
            pending_sell_exposure: CachePadded::new(AtomicI64::new(0)),
            started_at: Instant::now(),
        }
    }

    // ─── Read Interface (lock-free atomics + single read-lock) ───────────

    /// Confirmed position (ground truth from fill events)
    #[inline]
    pub fn confirmed_position(&self) -> f64 {
        self.confirmed_position.load(Ordering::Acquire) as f64 / POS_SCALE
    }

    /// Phase 2: Lock-free pending buy exposure
    #[inline]
    fn pending_buy_exposure(&self) -> f64 {
        self.pending_buy_exposure.load(Ordering::Acquire) as f64 / POS_SCALE
    }

    /// Phase 2: Lock-free pending sell exposure
    #[inline]
    fn pending_sell_exposure(&self) -> f64 {
        self.pending_sell_exposure.load(Ordering::Acquire) as f64 / POS_SCALE
    }

    /// Net pending exposure from all active orders (lock-free via atomics)
    pub fn net_pending_exposure(&self) -> f64 {
        self.net_pending_exposure_locked()
    }

    /// Debug: Net pending exposure via read lock (for verification)
    pub fn net_pending_exposure_locked(&self) -> f64 {
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
        self.confirmed_position() + self.net_pending_exposure_locked()
    }

    /// Worst-case long exposure: confirmed + all active buy remaining (lock-free)
    /// Used for pre-trade risk check before placing bids
    pub fn worst_case_long(&self) -> f64 {
        self.worst_case_long_locked()
    }

    /// Debug: Worst-case long via read lock (for verification)
    pub fn worst_case_long_locked(&self) -> f64 {
        let state = self.state.read();
        self.confirmed_position()
            + state
                .active_orders
                .values()
                .filter(|o| o.side == OrderSide::Buy && o.lifecycle.has_pending_exposure())
                .map(|o| o.remaining_size())
                .sum::<f64>()
    }

    /// Worst-case short exposure: confirmed - all active sell remaining (lock-free)
    /// Used for pre-trade risk check before placing asks
    pub fn worst_case_short(&self) -> f64 {
        self.worst_case_short_locked()
    }

    /// Debug: Worst-case short via read lock (for verification)
    pub fn worst_case_short_locked(&self) -> f64 {
        let state = self.state.read();
        self.confirmed_position()
            - state
                .active_orders
                .values()
                .filter(|o| o.side == OrderSide::Sell && o.lifecycle.has_pending_exposure())
                .map(|o| o.remaining_size())
                .sum::<f64>()
    }

    /// Debug: Verify atomic exposure matches locked traversal.
    /// NOTE: Not linearizable — TOCTOU window between locked and atomic reads.
    /// False positives possible under concurrent mutation. Use only for diagnostics.
    pub fn debug_verify_exposure(&self) {
        let locked_long = self.worst_case_long_locked();
        let atomic_long = self.confirmed_position() + self.pending_buy_exposure();
        if (locked_long - atomic_long).abs() > 1e-6 {
            tracing::error!(
                "EXPOSURE DRIFT long: locked={:.8} atomic={:.8}",
                locked_long,
                atomic_long
            );
        }
        let locked_short = self.worst_case_short_locked();
        let atomic_short = self.confirmed_position() - self.pending_sell_exposure();
        if (locked_short - atomic_short).abs() > 1e-6 {
            tracing::error!(
                "EXPOSURE DRIFT short: locked={:.8} atomic={:.8}",
                locked_short,
                atomic_short
            );
        }
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

    /// Count of filled orders within a given duration (for fill rate estimation)
    pub fn filled_count_since(&self, duration: Duration) -> usize {
        let state = self.state.read();
        state
            .completed_orders
            .values()
            .filter(|o| o.lifecycle == OrderLifecycle::Filled && o.last_update.elapsed() < duration)
            .count()
    }

    /// Total fill count and fees from all completed orders (for telemetry sync)
    pub fn total_fill_stats(&self) -> (u64, f64) {
        let state = self.state.read();
        let mut count = 0u64;
        let mut fees = 0.0f64;
        for order in state.completed_orders.values() {
            count += order.fills.len() as u64;
            fees += order.total_fee;
        }
        for order in state.active_orders.values() {
            count += order.fills.len() as u64;
            fees += order.total_fee;
        }
        (count, fees)
    }

    /// Get all active order client_order_ids
    pub fn active_cois(&self) -> Vec<i64> {
        self.state.read().active_orders.keys().copied().collect()
    }

    /// Snapshot all currently tracked active orders for strategy-side reconciliation.
    pub fn active_orders_snapshot(&self) -> Vec<TrackedOrder> {
        self.state
            .read()
            .active_orders
            .values()
            .cloned()
            .collect()
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
    pub fn start_tracking(&self, client_order_id: i64, side: OrderSide, price: f64, size: f64) {
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

        // Phase 2: Increment atomic exposure
        let size_scaled = (size * POS_SCALE) as i64;
        match side {
            OrderSide::Buy => {
                self.pending_buy_exposure
                    .fetch_add(size_scaled, Ordering::AcqRel);
            }
            OrderSide::Sell => {
                self.pending_sell_exposure
                    .fetch_add(size_scaled, Ordering::AcqRel);
            }
        }

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
            // Phase 2: Decrement atomic exposure
            let remaining_scaled = (order.remaining_size() * POS_SCALE) as i64;
            match order.side {
                OrderSide::Buy => {
                    self.pending_buy_exposure
                        .fetch_sub(remaining_scaled, Ordering::AcqRel);
                }
                OrderSide::Sell => {
                    self.pending_sell_exposure
                        .fetch_sub(remaining_scaled, Ordering::AcqRel);
                }
            }

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

    /// Revert a pending cancel back to the last known live state when cancel submission failed.
    pub fn revert_pending_cancel(&self, client_order_id: i64) {
        let mut state = self.state.write();
        if let Some(order) = state.active_orders.get_mut(&client_order_id)
            && order.lifecycle == OrderLifecycle::PendingCancel
        {
            order.lifecycle = if order.filled_size > 1e-12 {
                OrderLifecycle::PartiallyFilled
            } else {
                OrderLifecycle::Open
            };
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
            // Phase 2: Decrement atomic exposure
            if order.lifecycle.has_pending_exposure() {
                let remaining_scaled = (order.remaining_size() * POS_SCALE) as i64;
                match order.side {
                    OrderSide::Buy => {
                        self.pending_buy_exposure
                            .fetch_sub(remaining_scaled, Ordering::AcqRel);
                    }
                    OrderSide::Sell => {
                        self.pending_sell_exposure
                            .fetch_sub(remaining_scaled, Ordering::AcqRel);
                    }
                }
            }
            order.lifecycle = OrderLifecycle::Canceled;
            order.last_update = now;
            state.completed_orders.insert(coi, order);
        }
        count
    }

    /// Reconcile active orders with exchange actual open orders
    pub async fn reconcile_with_exchange(&self, exchange: &dyn Exchange) -> anyhow::Result<usize> {
        let open_orders = match exchange.get_active_orders().await {
            Ok(orders) => orders,
            Err(e) => {
                return Err(e);
            }
        };

        let mut state = self.state.write();
        let mut stale_ids = Vec::new();
        let mut exchange_bindings = Vec::new();
        let now = Instant::now();

        for (coi, order) in &mut state.active_orders {
            let matched_open = open_orders.iter().find(|oo| {
                if oo.client_order_index == *coi {
                    return true;
                }
                order
                    .exchange_order_id
                    .map(|exch_id| oo.order_id == exch_id.to_string())
                    .unwrap_or(false)
            });

            match (order.lifecycle, matched_open) {
                (OrderLifecycle::PendingCreate, Some(oo)) => {
                    order.lifecycle = OrderLifecycle::Open;
                    order.last_update = now;
                    if order.exchange_order_id.is_none()
                        && let Ok(exchange_order_id) = oo.order_id.parse::<u64>()
                    {
                        order.exchange_order_id = Some(exchange_order_id);
                        exchange_bindings.push((exchange_order_id, *coi));
                    }
                }
                (OrderLifecycle::PendingCreate, None) => {
                    if order.created_at.elapsed() >= PENDING_CREATE_RECONCILE_GRACE {
                        stale_ids.push((*coi, OrderLifecycle::Rejected));
                    }
                }
                (OrderLifecycle::PendingCancel, Some(_)) => {
                    if order.last_update.elapsed() >= PENDING_CANCEL_RECONCILE_GRACE {
                        order.lifecycle = if order.filled_size > 1e-12 {
                            OrderLifecycle::PartiallyFilled
                        } else {
                            OrderLifecycle::Open
                        };
                        order.last_update = now;
                    }
                }
                (
                    OrderLifecycle::Open
                    | OrderLifecycle::PartiallyFilled
                    | OrderLifecycle::PendingCancel,
                    None,
                ) => {
                    stale_ids.push((*coi, OrderLifecycle::Canceled));
                }
                _ => {}
            }
        }

        for (exchange_order_id, client_order_id) in exchange_bindings {
            state
                .exchange_to_client
                .insert(exchange_order_id, client_order_id);
        }

        let count = stale_ids.len();
        for (coi, lifecycle) in stale_ids {
            if let Some(mut order) = state.active_orders.remove(&coi) {
                if order.lifecycle.has_pending_exposure() {
                    let remaining_scaled = (order.remaining_size() * POS_SCALE) as i64;
                    match order.side {
                        OrderSide::Buy => {
                            self.pending_buy_exposure
                                .fetch_sub(remaining_scaled, Ordering::AcqRel);
                        }
                        OrderSide::Sell => {
                            self.pending_sell_exposure
                                .fetch_sub(remaining_scaled, Ordering::AcqRel);
                        }
                    }
                }
                if let Some(exchange_order_id) = order.exchange_order_id {
                    state.exchange_to_client.remove(&exchange_order_id);
                }
                order.lifecycle = lifecycle;
                order.last_update = now;
                state.completed_orders.insert(coi, order);
            }
        }
        Ok(count)
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
            if order.exchange_order_id == Some(event.exchange_order_id)
                && order.order_index == Some(event.order_index)
                && order.lifecycle != OrderLifecycle::PendingCreate
            {
                order.last_update = Instant::now();
                tracing::debug!(
                    "Ignoring duplicate order confirmation: coi={} exch_id={} order_idx={}",
                    client_id,
                    event.exchange_order_id,
                    event.order_index
                );
                return Ok(());
            }

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
        } else if let Some(existing_cid) = state
            .exchange_to_client
            .get(&event.exchange_order_id)
            .copied()
        {
            if let Some(order) = state.active_orders.get_mut(&existing_cid) {
                order.last_update = Instant::now();
                tracing::debug!(
                    "Ignoring duplicate order-created event by exchange binding: exch_id={} coi={} event_coi={}",
                    event.exchange_order_id,
                    existing_cid,
                    client_id
                );
                return Ok(());
            }

            if state.completed_orders.contains_key(&existing_cid) {
                tracing::debug!(
                    "Ignoring duplicate order-created event for completed order: exch_id={} coi={}",
                    event.exchange_order_id,
                    existing_cid
                );
                return Ok(());
            }
        } else {
            if let Some(order) = state.completed_orders.get_mut(&client_id) {
                order.last_update = Instant::now();
                tracing::debug!(
                    "Ignoring duplicate order-created event for completed order: coi={} exch_id={}",
                    client_id,
                    event.exchange_order_id
                );
                return Ok(());
            }

            // Auto-register untracked order (e.g. from restart, or manual order)
            if self.started_at.elapsed() < STARTUP_AUTO_REGISTER_GRACE {
                tracing::debug!(
                    "Ignoring untracked order during startup grace: coi={} exch_id={}",
                    client_id,
                    event.exchange_order_id
                );
                return Ok(());
            }

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

            // Phase 2: Auto-registered order needs atomic exposure
            let size_scaled = (event.original_size * POS_SCALE) as i64;
            match side {
                OrderSide::Buy => {
                    self.pending_buy_exposure
                        .fetch_add(size_scaled, Ordering::AcqRel);
                }
                OrderSide::Sell => {
                    self.pending_sell_exposure
                        .fetch_add(size_scaled, Ordering::AcqRel);
                }
            }

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
            .copied()
            .or_else(|| (event.client_order_id != 0).then_some(event.client_order_id));

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

                // Capture remaining before updating filled_size (for atomic exposure fix)
                let pre_fill_remaining = order.remaining_size();

                let post_fill_size = order.filled_size + event.fill_size;
                let residual_after_fill = (order.original_size - post_fill_size).max(0.0);

                order.filled_size = post_fill_size;
                order.total_fee += event.fee_paid;
                order.last_update = Instant::now();
                order
                    .fills
                    .push((event.trade_id, event.fill_size, event.fill_price));

                let is_final_fill =
                    event.remaining_size < 1e-12 && residual_after_fill <= FILL_COMPLETION_EPS;

                // Phase 2: Decrement atomic exposure
                // On final fill, drain full pre-fill remaining to avoid f64 accumulation drift
                let exposure_reduction = if is_final_fill {
                    (pre_fill_remaining * POS_SCALE) as i64
                } else {
                    (event.fill_size * POS_SCALE) as i64
                };
                match order.side {
                    OrderSide::Buy => {
                        self.pending_buy_exposure
                            .fetch_sub(exposure_reduction, Ordering::AcqRel);
                    }
                    OrderSide::Sell => {
                        self.pending_sell_exposure
                            .fetch_sub(exposure_reduction, Ordering::AcqRel);
                    }
                }

                if is_final_fill {
                    order.lifecycle = OrderLifecycle::Filled;
                } else {
                    order.lifecycle = OrderLifecycle::PartiallyFilled;
                }

                (order.lifecycle == OrderLifecycle::Filled, order.side)
            } else {
                // Not in active_orders — might be in completed (late event)
                if let Some(order) = state.completed_orders.get_mut(&cid) {
                    if event.trade_id == 0
                        && event.remaining_size < 1e-12
                        && order.lifecycle == OrderLifecycle::Filled
                        && order.fills.iter().any(|(tid, sz, px)| {
                            *tid == 0
                                && (*sz - event.fill_size).abs() < 1e-12
                                && (*px - event.fill_price).abs() < 1e-9
                        })
                    {
                        tracing::warn!(
                            "Duplicate completed-order terminal fill ignored: coi={} size={:.4} price={:.2}",
                            cid,
                            event.fill_size,
                            event.fill_price
                        );
                        return Ok(());
                    }
                    if event.trade_id != 0
                        && order.fills.iter().any(|(tid, _, _)| *tid == event.trade_id)
                    {
                        tracing::warn!(
                            "Duplicate completed-order fill ignored: trade_id={} coi={}",
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
            let side = if event.is_ask != 0 {
                OrderSide::Sell
            } else {
                OrderSide::Buy
            };

            if event.client_order_id == 0 {
                tracing::warn!(
                    "⚠️  Ignoring unowned counterparty fill: exch_id={} side={} size={:.4} price={:.2} trade_id={}",
                    event.exchange_order_id,
                    side,
                    event.fill_size,
                    event.fill_price,
                    event.trade_id
                );
                return Ok(());
            }

            // Untracked fill with a client_order_id is still assumed to be ours,
            // e.g. after restart or if create/cancel ordering was missed.
            let signed = side.sign() * event.fill_size;
            let delta = (signed * POS_SCALE) as i64;
            self.confirmed_position.fetch_add(delta, Ordering::AcqRel);

            tracing::warn!(
                "⚠️  Untracked owned fill: exch_id={} coi={} side={} size={:.4} price={:.2}",
                event.exchange_order_id,
                event.client_order_id,
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
            .copied()
            .or_else(|| (event.client_order_id != 0).then_some(event.client_order_id));

        if let Some(cid) = client_id {
            if let Some(order) = state.active_orders.get_mut(&cid) {
                // Phase 2: Decrement atomic exposure by remaining_size
                if order.lifecycle.has_pending_exposure() {
                    let remaining_scaled = (order.remaining_size() * POS_SCALE) as i64;
                    match order.side {
                        OrderSide::Buy => {
                            self.pending_buy_exposure
                                .fetch_sub(remaining_scaled, Ordering::AcqRel);
                        }
                        OrderSide::Sell => {
                            self.pending_sell_exposure
                                .fetch_sub(remaining_scaled, Ordering::AcqRel);
                        }
                    }
                }

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
            // Phase 2: Decrement atomic exposure by remaining_size
            if order.lifecycle.has_pending_exposure() {
                let remaining_scaled = (order.remaining_size() * POS_SCALE) as i64;
                match order.side {
                    OrderSide::Buy => {
                        self.pending_buy_exposure
                            .fetch_sub(remaining_scaled, Ordering::AcqRel);
                    }
                    OrderSide::Sell => {
                        self.pending_sell_exposure
                            .fetch_sub(remaining_scaled, Ordering::AcqRel);
                    }
                }
            }

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
mod tests;
