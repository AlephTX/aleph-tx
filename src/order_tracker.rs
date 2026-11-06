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
use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
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
    /// Submission outcome is unknown; absence from open orders cannot release risk.
    pub submission_uncertain: bool,
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
    total_fill_count: u64,
    total_fees: f64,
}

impl TrackerState {
    fn new() -> Self {
        Self {
            active_orders: HashMap::with_capacity(64),
            exchange_to_client: HashMap::with_capacity(64),
            completed_orders: HashMap::with_capacity(128),
            total_fill_count: 0,
            total_fees: 0.0,
        }
    }
}

// ─── Order Tracker ───────────────────────────────────────────────────────────

pub struct OrderTracker {
    /// All mutable order state behind a single RwLock for atomic traversal
    pub state: RwLock<TrackerState>,
    /// Confirmed position (only driven by fill events, ground truth)
    pub confirmed_position: CachePadded<AtomicI64>,
    /// Legacy field name: signed trade cash flow less fees, scaled by POS_SCALE.
    /// This does not include inventory cost basis or mark-to-market PnL.
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
    event_processing: Mutex<()>,
    reconciliation_epoch: AtomicU64,
    reconciled_epoch: AtomicU64,
    reconciliation_gate: Mutex<()>,
}

impl OrderTracker {
    pub fn new() -> Self {
        Self {
            state: RwLock::new(TrackerState::new()),
            confirmed_position: CachePadded::new(AtomicI64::new(0)),
            realized_pnl: CachePadded::new(AtomicI64::new(0)),
            last_sequence: AtomicI64::new(-1),
            pending_buy_exposure: CachePadded::new(AtomicI64::new(0)),
            pending_sell_exposure: CachePadded::new(AtomicI64::new(0)),
            started_at: Instant::now(),
            event_processing: Mutex::new(()),
            reconciliation_epoch: AtomicU64::new(0),
            reconciled_epoch: AtomicU64::new(0),
            reconciliation_gate: Mutex::new(()),
        }
    }

    /// Stop new risk until cancel/open-order and fresh position reconciliation succeeds.
    pub fn require_reconciliation(&self) {
        let _gate = self.reconciliation_gate.lock();
        self.reconciliation_epoch.fetch_add(1, Ordering::AcqRel);
    }

    pub fn reconciliation_epoch(&self) -> u64 {
        self.reconciliation_epoch.load(Ordering::Acquire)
    }

    pub fn needs_reconciliation(&self) -> bool {
        let reconciled = self.reconciled_epoch.load(Ordering::Acquire);
        self.reconciliation_epoch() != reconciled
    }

    /// A cleanup may acknowledge only the epoch it observed before starting.
    /// A new gap during the cleanup always remains visible, even if it races
    /// between the comparison and the acknowledgment store.
    pub fn clear_reconciliation_if_epoch(&self, epoch: u64) -> bool {
        let _gate = self.reconciliation_gate.lock();
        if self.reconciliation_epoch() != epoch {
            return false;
        }
        self.reconciled_epoch.store(epoch, Ordering::Release);
        self.reconciliation_epoch() == epoch
    }

    /// Atomically acknowledge an authoritative cleanup: the caller has confirmed
    /// no exchange open orders and obtained a fresh independent position snapshot.
    /// State mutations and gap detection cannot interleave with this commit.
    pub fn confirm_cleanup(&self, epoch: u64, authoritative_position: f64) -> bool {
        if !authoritative_position.is_finite()
            || authoritative_position.abs() * POS_SCALE >= i64::MAX as f64
        {
            return false;
        }
        let _events = self.event_processing.lock();
        let mut state = self.state.write();
        let _gate = self.reconciliation_gate.lock();
        if self.reconciliation_epoch() != epoch {
            return false;
        }
        let active: Vec<_> = state.active_orders.drain().collect();
        for (cid, mut order) in active {
            order.lifecycle = OrderLifecycle::Canceled;
            order.last_update = Instant::now();
            state.completed_orders.insert(cid, order);
        }
        self.pending_buy_exposure.store(0, Ordering::Release);
        self.pending_sell_exposure.store(0, Ordering::Release);
        self.confirmed_position.store(
            (authoritative_position * POS_SCALE).round() as i64,
            Ordering::Release,
        );
        self.reconciled_epoch.store(epoch, Ordering::Release);
        true
    }

    /// For synchronous initialization only; asynchronous cleanup must use an epoch.
    pub fn clear_reconciliation_required(&self) {
        self.clear_reconciliation_if_epoch(self.reconciliation_epoch());
    }

    /// Rebase sequence after a detected producer restart, while trading is halted.
    pub fn reset_event_sequence(&self) {
        let _event_guard = self.event_processing.lock();
        self.require_reconciliation();
        self.last_sequence.store(-1, Ordering::Release);
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
        let state = self.state.read();
        self.confirmed_position()
            + state
                .active_orders
                .values()
                .map(|o| o.pending_exposure())
                .sum::<f64>()
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

    /// Signed sale proceeds minus purchase costs and fees. This is cash flow,
    /// not realized profit when the inventory position is nonzero.
    #[inline]
    pub fn trade_cash_flow(&self) -> f64 {
        self.realized_pnl.load(Ordering::Acquire) as f64 / POS_SCALE
    }

    /// Legacy alias for trade_cash_flow(); does not calculate cost-basis PnL.
    #[inline]
    pub fn realized_pnl(&self) -> f64 {
        self.trade_cash_flow()
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

    /// Lifetime fill count and fees, independent of completed-order TTL retention.
    /// O(1): telemetry no longer traverses every fill-bearing order on each tick.
    pub fn total_fill_stats(&self) -> (u64, f64) {
        let state = self.state.read();
        (state.total_fill_count, state.total_fees)
    }

    /// Get all active order client_order_ids
    pub fn active_cois(&self) -> Vec<i64> {
        self.state.read().active_orders.keys().copied().collect()
    }

    /// Snapshot all currently tracked active orders for strategy-side reconciliation.
    pub fn active_orders_snapshot(&self) -> Vec<TrackedOrder> {
        self.state.read().active_orders.values().cloned().collect()
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
    ) -> bool {
        if client_order_id == 0
            || !price.is_finite()
            || price <= 0.0
            || !size.is_finite()
            || size <= 0.0
            || size * POS_SCALE >= i64::MAX as f64
        {
            tracing::error!("Invalid order registration: coi={}", client_order_id);
            return false;
        }
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
            submission_uncertain: false,
        };

        let mut state = self.state.write();
        if state.active_orders.contains_key(&client_order_id)
            || state.completed_orders.contains_key(&client_order_id)
        {
            tracing::error!("Duplicate client order id: {}", client_order_id);
            return false;
        }
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
        true
    }

    /// Mark order as failed (API call failed, rollback optimistic accounting)
    pub fn mark_failed(&self, client_order_id: i64) {
        let mut state = self.state.write();
        // A WS acknowledgment/fill wins a race against an HTTP failure. Never
        // erase a known live order because the HTTP response arrived later.
        if !state
            .active_orders
            .get(&client_order_id)
            .is_some_and(|order| order.lifecycle == OrderLifecycle::PendingCreate)
        {
            return;
        }
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

    /// Preserve exposure after a transport failure or ambiguous exchange response.
    pub fn mark_submission_uncertain(&self, client_order_id: i64) {
        let mut state = self.state.write();
        if let Some(order) = state.active_orders.get_mut(&client_order_id)
            && order.lifecycle == OrderLifecycle::PendingCreate
        {
            order.submission_uncertain = true;
            order.last_update = Instant::now();
        }
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
        if !exchange_position.is_finite() || exchange_position.abs() * POS_SCALE >= i64::MAX as f64
        {
            tracing::error!("Ignoring invalid authoritative position");
            return 0.0;
        }
        let _state = self.state.write();
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
        let mut state = self.state.write();
        state
            .completed_orders
            .retain(|_, order| order.last_update.elapsed() < ttl);
        // Reverse bindings live exactly as long as active/completed records so
        // late fills without a client ID can still be deduplicated.
        let TrackerState {
            active_orders,
            completed_orders,
            exchange_to_client,
            ..
        } = &mut *state;
        exchange_to_client
            .retain(|_, cid| active_orders.contains_key(cid) || completed_orders.contains_key(cid));
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
        let request_started = Instant::now();
        let open_orders = match exchange.get_active_orders().await {
            Ok(orders) => orders,
            Err(e) => {
                return Err(e);
            }
        };

        let mut state = self.state.write();
        let mut unresolved_count = 0;
        let mut exchange_bindings = Vec::new();
        let now = Instant::now();

        for (coi, order) in &mut state.active_orders {
            // REST may describe a time before an order/fill received during await.
            if order.last_update >= request_started {
                continue;
            }
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
                    order.submission_uncertain = false;
                    order.last_update = now;
                    if order.exchange_order_id.is_none()
                        && let Ok(exchange_order_id) = oo.order_id.parse::<u64>()
                    {
                        order.exchange_order_id = Some(exchange_order_id);
                        exchange_bindings.push((exchange_order_id, *coi));
                    }
                }
                (OrderLifecycle::PendingCreate, None)
                    if order.created_at.elapsed() >= PENDING_CREATE_RECONCILE_GRACE =>
                {
                    // Absence cannot distinguish rejection, a fill or delayed acceptance.
                    order.submission_uncertain = true;
                    unresolved_count += 1;
                }
                (OrderLifecycle::PendingCancel, Some(_))
                    if order.last_update.elapsed() >= PENDING_CANCEL_RECONCILE_GRACE =>
                {
                    order.lifecycle = if order.filled_size > 1e-12 {
                        OrderLifecycle::PartiallyFilled
                    } else {
                        OrderLifecycle::Open
                    };
                    order.last_update = now;
                }
                (
                    OrderLifecycle::Open
                    | OrderLifecycle::PartiallyFilled
                    | OrderLifecycle::PendingCancel,
                    None,
                ) => {
                    order.submission_uncertain = true;
                    unresolved_count += 1;
                }
                _ => {}
            }
        }

        for (exchange_order_id, client_order_id) in exchange_bindings {
            state
                .exchange_to_client
                .insert(exchange_order_id, client_order_id);
        }

        if unresolved_count > 0 {
            self.require_reconciliation();
            tracing::error!(
                "{} tracked orders missing from REST; exposure retained pending authoritative cleanup",
                unresolved_count
            );
        }
        // No exposure is released based solely on an open-order snapshot.
        Ok(0)
    }
}

impl OrderTracker {
    // ─── Event Processing (State Machine Transitions) ────────────────────

    /// Process a V2 event from SHM ring buffer
    pub fn apply_event(&self, event: &ShmPrivateEventV2) -> anyhow::Result<()> {
        let result = self.apply_event_checked(event);
        if result.is_err() {
            self.require_reconciliation();
        }
        result
    }

    fn apply_event_checked(&self, event: &ShmPrivateEventV2) -> anyhow::Result<()> {
        anyhow::ensure!(
            event.sequence <= i64::MAX as u64,
            "event sequence out of range"
        );
        anyhow::ensure!((1..=4).contains(&event.event_type), "invalid event type");
        anyhow::ensure!(event.is_ask <= 1, "invalid event side");
        anyhow::ensure!(
            event.remaining_size.is_finite()
                && event.remaining_size >= 0.0
                && event.remaining_size * POS_SCALE < i64::MAX as f64,
            "invalid remaining size"
        );
        if event.event_type == 1 {
            anyhow::ensure!(
                event.order_price.is_finite()
                    && event.order_price > 0.0
                    && event.original_size.is_finite()
                    && event.original_size > 0.0
                    && event.original_size * POS_SCALE < i64::MAX as f64,
                "invalid order confirmation"
            );
        }
        if event.event_type == 2 {
            anyhow::ensure!(
                event.fill_size.is_finite()
                    && event.fill_size > 0.0
                    && event.fill_price.is_finite()
                    && event.fill_price > 0.0
                    && event.fee_paid.is_finite(),
                "invalid fill"
            );
        }
        // Serialize sequence acceptance with the state transition; sequence zero
        // is a real first event, not an uninitialized sentinel.
        let _event_guard = self.event_processing.lock();
        let prev_seq = self.last_sequence.load(Ordering::Acquire);
        if prev_seq >= 0 && event.sequence <= prev_seq as u64 {
            return Ok(());
        }
        if prev_seq >= 0 && event.sequence > prev_seq as u64 + 1 {
            self.require_reconciliation();
            tracing::error!(
                "Event gap: expected={} got={}",
                prev_seq + 1,
                event.sequence
            );
        }
        match event.event_type {
            1 => self.handle_order_created(event)?,
            2 => self.handle_order_filled(event)?,
            3 => self.handle_order_canceled(event)?,
            4 => self.handle_order_rejected(event)?,
            _ => unreachable!("validated above"),
        }
        self.last_sequence
            .store(event.sequence as i64, Ordering::Release);
        Ok(())
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
            if order.lifecycle == OrderLifecycle::PendingCreate {
                order.lifecycle = OrderLifecycle::Open;
            }
            order.submission_uncertain = false;
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
                submission_uncertain: false,
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
        let Some(cid) = state
            .exchange_to_client
            .get(&event.exchange_order_id)
            .copied()
            .or_else(|| (event.client_order_id != 0).then_some(event.client_order_id))
        else {
            // Private trade messages may include an unowned counterparty leg.
            return Ok(());
        };
        let existing = state
            .active_orders
            .get(&cid)
            .or_else(|| state.completed_orders.get(&cid));
        if let Some(order) = existing {
            if event.trade_id != 0 && order.fills.iter().any(|(tid, _, _)| *tid == event.trade_id) {
                return Ok(());
            }
            if event.trade_id == 0
                && order.lifecycle == OrderLifecycle::Filled
                && order.fills.iter().any(|(tid, size, price)| {
                    *tid == 0
                        && (*size - event.fill_size).abs() < 1e-12
                        && (*price - event.fill_price).abs() < 1e-9
                })
            {
                return Ok(());
            }
            anyhow::ensure!(
                order.filled_size + event.fill_size <= order.original_size + FILL_COMPLETION_EPS,
                "fill exceeds original order size for {}",
                cid
            );
        }
        let side = existing
            .map(|order| order.side)
            .unwrap_or(if event.is_ask == 1 {
                OrderSide::Sell
            } else {
                OrderSide::Buy
            });
        anyhow::ensure!(
            (side == OrderSide::Sell) == (event.is_ask == 1),
            "fill side mismatch"
        );
        let position_delta = side.sign() * event.fill_size * POS_SCALE;
        let cash_delta =
            (-side.sign() * event.fill_price * event.fill_size - event.fee_paid) * POS_SCALE;
        anyhow::ensure!(
            position_delta.is_finite()
                && position_delta.abs() < i64::MAX as f64
                && cash_delta.is_finite()
                && cash_delta.abs() < i64::MAX as f64,
            "fill accounting overflow"
        );
        let new_position = self
            .confirmed_position
            .load(Ordering::Acquire)
            .checked_add(position_delta.round() as i64)
            .ok_or_else(|| anyhow::anyhow!("position overflow"))?;
        let new_cash = self
            .realized_pnl
            .load(Ordering::Acquire)
            .checked_add(cash_delta.round() as i64)
            .ok_or_else(|| anyhow::anyhow!("cash flow overflow"))?;
        let now = Instant::now();
        if let Some(order) = state.active_orders.get_mut(&cid) {
            let before = (order.remaining_size() * POS_SCALE) as i64;
            order.filled_size += event.fill_size;
            order.total_fee += event.fee_paid;
            order.last_update = now;
            order
                .fills
                .push((event.trade_id, event.fill_size, event.fill_price));
            let complete = order.remaining_size() <= FILL_COMPLETION_EPS;
            let after = if complete {
                0
            } else {
                (order.remaining_size() * POS_SCALE) as i64
            };
            // Difference of rounded totals, rather than rounding each individual
            // fill, prevents residual/negative atomic exposure after many fills.
            match side {
                OrderSide::Buy => {
                    self.pending_buy_exposure
                        .fetch_sub(before - after, Ordering::AcqRel);
                }
                OrderSide::Sell => {
                    self.pending_sell_exposure
                        .fetch_sub(before - after, Ordering::AcqRel);
                }
            }
            if complete {
                order.lifecycle = OrderLifecycle::Filled;
            } else if order.lifecycle != OrderLifecycle::PendingCancel {
                order.lifecycle = OrderLifecycle::PartiallyFilled;
            }
            if complete && let Some(order) = state.active_orders.remove(&cid) {
                state.completed_orders.insert(cid, order);
            }
        } else if let Some(order) = state.completed_orders.get_mut(&cid) {
            // Late fills still change cash flow and fees, but cannot revive a
            // canceled order's pending exposure.
            order.filled_size += event.fill_size;
            order.total_fee += event.fee_paid;
            order.last_update = now;
            order
                .fills
                .push((event.trade_id, event.fill_size, event.fill_price));
        } else {
            // Retain owned fills received before an ACK (or after restart) so
            // retransmissions can be deduplicated just like normal fills.
            let mut order = TrackedOrder {
                client_order_id: cid,
                exchange_order_id: Some(event.exchange_order_id),
                order_index: (event.order_index != 0).then_some(event.order_index),
                side,
                price: event.fill_price,
                original_size: event.fill_size + event.remaining_size,
                filled_size: event.fill_size,
                lifecycle: OrderLifecycle::Filled,
                created_at: now,
                last_update: now,
                total_fee: event.fee_paid,
                fills: vec![(event.trade_id, event.fill_size, event.fill_price)],
                submission_uncertain: false,
            };
            if event.remaining_size > FILL_COMPLETION_EPS {
                order.lifecycle = OrderLifecycle::PartiallyFilled;
                let remaining = (order.remaining_size() * POS_SCALE) as i64;
                match side {
                    OrderSide::Buy => {
                        self.pending_buy_exposure
                            .fetch_add(remaining, Ordering::AcqRel);
                    }
                    OrderSide::Sell => {
                        self.pending_sell_exposure
                            .fetch_add(remaining, Ordering::AcqRel);
                    }
                }
                state.active_orders.insert(cid, order);
            } else {
                state.completed_orders.insert(cid, order);
            }
        }
        if event.exchange_order_id != 0 {
            state
                .exchange_to_client
                .insert(event.exchange_order_id, cid);
        }
        state.total_fill_count += 1;
        state.total_fees += event.fee_paid;
        self.confirmed_position
            .store(new_position, Ordering::Release);
        self.realized_pnl.store(new_cash, Ordering::Release);
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
