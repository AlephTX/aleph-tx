//! Shadow Ledger - Optimistic State Machine for Zero-Latency Position Tracking
//!
//! This module maintains a local copy of account state (positions, orders, PnL)
//! by consuming private events from the event ring buffer. This enables:
//! - <1μs state queries (vs 50-200ms REST API)
//! - Real-time event-driven updates
//! - Zero API calls for state queries
//!
//! v4.0.0: real_pos and in_flight_pos use AtomicI64 (scaled 1e8) with CachePadded
//! to eliminate RwLock contention on the hot path.

use crate::error::{Result, TradingError};
use crate::shm_event_reader::ShmEventReader;
use crate::types::{EventType, ShmPrivateEvent};
use crossbeam::utils::CachePadded;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Instant;

/// Scale factor for AtomicI64 position fields (1e8 = 8 decimal places)
const POS_SCALE: f64 = 1e8;

/// Order side (buy or sell)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderSide {
    Buy,
    Sell,
}

impl OrderSide {
    /// Get the signed multiplier for position calculations
    /// Buy = +1, Sell = -1
    pub fn sign(&self) -> f64 {
        match self {
            OrderSide::Buy => 1.0,
            OrderSide::Sell => -1.0,
        }
    }
}

impl std::fmt::Display for OrderSide {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OrderSide::Buy => write!(f, "bid"),
            OrderSide::Sell => write!(f, "ask"),
        }
    }
}

/// Order state tracked in the shadow ledger
#[derive(Debug, Clone)]
pub struct OrderState {
    pub order_id: u64,
    pub symbol_id: u16,
    pub side: OrderSide,
    pub initial_size: f64,
    pub filled_size: f64,
    pub remaining_size: f64,
    pub avg_fill_price: f64,
    pub total_fees: f64,
    pub created_at: Instant,
    pub last_update: Instant,
    /// True if this order was pre-registered via add_in_flight (needs in_flight reconciliation)
    pub tracked: bool,
}

/// Local account state (shadow ledger)
///
/// v4.0.0: `real_pos` and `in_flight_pos` are lock-free AtomicI64 (scaled 1e8)
/// to eliminate cache-coherency ping-pong on the hot path.
/// Other fields are only accessed by the event consumer (single writer).
pub struct ShadowLedger {
    /// Confirmed position (reconciled from WS events) - LOCK-FREE
    pub real_pos: CachePadded<AtomicI64>,

    /// Optimistic in-flight position (orders sent but not yet confirmed) - LOCK-FREE
    pub in_flight_pos: CachePadded<AtomicI64>,

    /// Realized PnL (USD) - only written by event consumer
    pub realized_pnl: f64,

    /// Active orders (order_id -> OrderState) - only written by event consumer
    pub active_orders: HashMap<u64, OrderState>,

    /// Last event sequence number processed
    pub last_sequence: u64,

    /// Last update timestamp
    pub last_update: Instant,
}

impl std::fmt::Debug for ShadowLedger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShadowLedger")
            .field("real_pos", &self.real_pos_f64())
            .field("in_flight_pos", &self.in_flight_pos_f64())
            .field("realized_pnl", &self.realized_pnl)
            .field("active_orders", &self.active_orders.len())
            .field("last_sequence", &self.last_sequence)
            .finish()
    }
}

impl Clone for ShadowLedger {
    fn clone(&self) -> Self {
        Self {
            real_pos: CachePadded::new(AtomicI64::new(self.real_pos.load(Ordering::Acquire))),
            in_flight_pos: CachePadded::new(AtomicI64::new(
                self.in_flight_pos.load(Ordering::Acquire),
            )),
            realized_pnl: self.realized_pnl,
            active_orders: self.active_orders.clone(),
            last_sequence: self.last_sequence,
            last_update: self.last_update,
        }
    }
}

impl Default for ShadowLedger {
    fn default() -> Self {
        Self {
            real_pos: CachePadded::new(AtomicI64::new(0)),
            in_flight_pos: CachePadded::new(AtomicI64::new(0)),
            realized_pnl: 0.0,
            active_orders: HashMap::new(),
            last_sequence: 0,
            last_update: Instant::now(),
        }
    }
}

impl ShadowLedger {
    /// Read real_pos as f64 (lock-free)
    #[inline]
    pub fn real_pos_f64(&self) -> f64 {
        self.real_pos.load(Ordering::Acquire) as f64 / POS_SCALE
    }

    /// Read in_flight_pos as f64 (lock-free)
    #[inline]
    pub fn in_flight_pos_f64(&self) -> f64 {
        self.in_flight_pos.load(Ordering::Acquire) as f64 / POS_SCALE
    }

    /// Get total exposure (real + in_flight) - LOCK-FREE
    #[inline]
    pub fn total_exposure(&self) -> f64 {
        self.real_pos_f64() + self.in_flight_pos_f64()
    }

    /// Add to in_flight position (optimistic accounting) - LOCK-FREE
    pub fn add_in_flight(&self, delta: f64) {
        let delta_scaled = (delta * POS_SCALE) as i64;
        self.in_flight_pos.fetch_add(delta_scaled, Ordering::AcqRel);
        tracing::debug!(
            "In-flight updated: delta={:.4} new_in_flight={:.4} total_exposure={:.4}",
            delta,
            self.in_flight_pos_f64(),
            self.total_exposure()
        );
    }

    /// Register a new order in the shadow ledger (called when order is sent)
    pub fn register_order(
        &mut self,
        order_id: u64,
        symbol_id: u16,
        side: OrderSide,
        price: f64,
        size: f64,
    ) {
        self.active_orders.insert(
            order_id,
            OrderState {
                order_id,
                symbol_id,
                side,
                initial_size: size,
                filled_size: 0.0,
                remaining_size: size,
                avg_fill_price: price,
                total_fees: 0.0,
                created_at: Instant::now(),
                last_update: Instant::now(),
                tracked: true,
            },
        );
        tracing::debug!(
            "Order registered: id={} side={} price={} size={}",
            order_id,
            side,
            price,
            size
        );
    }
    /// Apply an event to the shadow ledger with proper validation and reconciliation
    pub fn apply_event(&mut self, event: &ShmPrivateEvent) -> Result<()> {
        tracing::debug!(
            "📨 Event received: seq={} type={} order_id={} fill_price={:.2} fill_size={:.4}",
            event.sequence,
            event.event_type,
            event.order_id,
            event.fill_price,
            event.fill_size
        );

        // Detect out-of-order or duplicate events
        if event.sequence <= self.last_sequence && self.last_sequence > 0 {
            tracing::warn!(
                "Out-of-order event: got seq={} but last_sequence={}",
                event.sequence,
                self.last_sequence
            );
            return Err(TradingError::OutOfOrderEvent {
                expected: self.last_sequence + 1,
                actual: event.sequence,
            });
        }

        // Detect gaps
        if event.sequence > self.last_sequence + 1 && self.last_sequence > 0 {
            let gap_size = event.sequence - self.last_sequence - 1;
            tracing::error!(
                "Event gap detected: expected seq={} got seq={} (gap={})",
                self.last_sequence + 1,
                event.sequence,
                gap_size
            );
            // Continue processing but log the gap
        }

        self.last_sequence = event.sequence;
        self.last_update = Instant::now();

        match event.event_type() {
            Some(EventType::OrderCreated) => {
                let is_ask = event.is_ask != 0;
                let side = if is_ask {
                    OrderSide::Sell
                } else {
                    OrderSide::Buy
                };

                if let Some(order) = self.active_orders.get_mut(&event.order_id) {
                    // Update the order state with confirmed data
                    order.remaining_size = event.remaining_size;
                    order.last_update = Instant::now();
                    tracing::debug!(
                        "Order confirmed: id={} side={} size={:.4}",
                        event.order_id,
                        order.side,
                        event.remaining_size
                    );
                } else {
                    // Auto-register untracked orders so fills can be reconciled
                    self.active_orders.insert(
                        event.order_id,
                        OrderState {
                            order_id: event.order_id,
                            symbol_id: event.symbol_id,
                            side,
                            initial_size: event.remaining_size,
                            filled_size: 0.0,
                            remaining_size: event.remaining_size,
                            avg_fill_price: 0.0,
                            total_fees: 0.0,
                            created_at: Instant::now(),
                            last_update: Instant::now(),
                            tracked: false,
                        },
                    );
                    tracing::debug!(
                        "Auto-registered order: id={} side={} size={:.4}",
                        event.order_id,
                        side,
                        event.remaining_size
                    );
                }
            }
            Some(EventType::OrderFilled) => {
                let is_ask = event.is_ask != 0;

                if let Some(order) = self.active_orders.get_mut(&event.order_id) {
                    let prev_filled = order.filled_size;
                    order.filled_size += event.fill_size;
                    order.remaining_size = event.remaining_size;
                    order.total_fees += event.fee_paid;
                    order.last_update = Instant::now();

                    // Update average fill price
                    if order.filled_size > 0.0 {
                        order.avg_fill_price = ((order.avg_fill_price * prev_filled)
                            + (event.fill_price * event.fill_size))
                            / order.filled_size;
                    }

                    // Calculate signed fill size based on order side
                    let signed_fill = order.side.sign() * event.fill_size;
                    let order_side = order.side;
                    let order_tracked = order.tracked;
                    let should_remove = order.remaining_size <= 0.0;

                    // Only reconcile in_flight for tracked orders (pre-registered via add_in_flight)
                    if order_tracked {
                        let delta_scaled = (signed_fill * POS_SCALE) as i64;
                        self.in_flight_pos.fetch_sub(delta_scaled, Ordering::AcqRel);
                    }
                    // Always update real position
                    let delta_scaled = (signed_fill * POS_SCALE) as i64;
                    self.real_pos.fetch_add(delta_scaled, Ordering::AcqRel);

                    // Update realized PnL correctly for both sides
                    match order_side {
                        OrderSide::Buy => {
                            self.realized_pnl -=
                                event.fill_price * event.fill_size + event.fee_paid;
                        }
                        OrderSide::Sell => {
                            self.realized_pnl +=
                                event.fill_price * event.fill_size - event.fee_paid;
                        }
                    }

                    let total_exp = self.total_exposure();

                    tracing::debug!(
                        "Fill reconciled: order={} side={} size={:.4} price={:.2} real_pos={:.4} in_flight={:.4} total={:.4}",
                        event.order_id,
                        order_side,
                        event.fill_size,
                        event.fill_price,
                        self.real_pos_f64(),
                        self.in_flight_pos_f64(),
                        total_exp
                    );

                    // Remove order if fully filled
                    if should_remove {
                        self.active_orders.remove(&event.order_id);
                    }
                } else {
                    // Untracked fill — use is_ask from event to determine side
                    let side = if is_ask {
                        OrderSide::Sell
                    } else {
                        OrderSide::Buy
                    };
                    let signed_fill = side.sign() * event.fill_size;

                    // Update real_pos directly (no in_flight to reconcile)
                    let delta_scaled = (signed_fill * POS_SCALE) as i64;
                    self.real_pos.fetch_add(delta_scaled, Ordering::AcqRel);

                    match side {
                        OrderSide::Buy => {
                            self.realized_pnl -=
                                event.fill_price * event.fill_size + event.fee_paid;
                        }
                        OrderSide::Sell => {
                            self.realized_pnl +=
                                event.fill_price * event.fill_size - event.fee_paid;
                        }
                    }

                    tracing::warn!(
                        "Untracked fill: order={} side={} size={:.4} price={:.2} -> real_pos={:.4}",
                        event.order_id,
                        side,
                        event.fill_size,
                        event.fill_price,
                        self.real_pos_f64()
                    );
                }
            }
            Some(EventType::OrderCanceled) => {
                // Rollback in_flight for tracked (pre-registered) canceled orders
                if let Some(order) = self.active_orders.get(&event.order_id) {
                    if order.tracked {
                        let signed_remaining = order.side.sign() * order.remaining_size;
                        let delta_scaled = (signed_remaining * POS_SCALE) as i64;
                        self.in_flight_pos.fetch_sub(delta_scaled, Ordering::AcqRel);
                        tracing::debug!(
                            "Order canceled (tracked): id={} side={} remaining={:.4} rolled back in_flight",
                            event.order_id,
                            order.side,
                            order.remaining_size
                        );
                    } else {
                        tracing::debug!(
                            "Order canceled (auto): id={} side={} remaining={:.4}",
                            event.order_id,
                            order.side,
                            order.remaining_size
                        );
                    }
                }
                self.active_orders.remove(&event.order_id);
            }
            Some(EventType::OrderRejected) => {
                // Rollback in_flight for tracked (pre-registered) rejected orders
                if let Some(order) = self.active_orders.get(&event.order_id) {
                    if order.tracked {
                        let signed_remaining = order.side.sign() * order.remaining_size;
                        let delta_scaled = (signed_remaining * POS_SCALE) as i64;
                        self.in_flight_pos.fetch_sub(delta_scaled, Ordering::AcqRel);
                        tracing::warn!(
                            "Order rejected (tracked): id={} side={} size={:.4} rolled back in_flight",
                            event.order_id,
                            order.side,
                            order.initial_size
                        );
                    } else {
                        tracing::warn!(
                            "Order rejected (auto): id={} side={} size={:.4}",
                            event.order_id,
                            order.side,
                            order.initial_size
                        );
                    }
                }
                self.active_orders.remove(&event.order_id);
            }
            None => {
                tracing::warn!("Unknown event type: {}", event.event_type);
                return Err(TradingError::InvalidEventType(event.event_type));
            }
        }

        Ok(())
    }

    /// Get current confirmed position - LOCK-FREE
    pub fn position(&self) -> f64 {
        self.real_pos_f64()
    }

    /// Get total exposure (real + in_flight)
    pub fn exposure(&self) -> f64 {
        self.total_exposure()
    }

    /// Get realized PnL
    pub fn pnl(&self) -> f64 {
        self.realized_pnl
    }

    /// Get number of active orders
    pub fn active_order_count(&self) -> usize {
        self.active_orders.len()
    }

    /// Check if an order is active
    pub fn has_active_order(&self, order_id: u64) -> bool {
        self.active_orders.contains_key(&order_id)
    }

    /// Force-sync real_pos from an authoritative source (e.g., REST API / AccountStats).
    /// Call periodically to correct drift from missed events.
    /// Returns the correction delta applied.
    pub fn force_sync_position(&self, authoritative_pos: f64) -> f64 {
        let current = self.real_pos_f64();
        let delta = authoritative_pos - current;
        if delta.abs() > 1e-8 {
            tracing::warn!(
                "Ledger force_sync: real_pos {:.6} → {:.6} (delta={:.6}, in_flight={:.6})",
                current,
                authoritative_pos,
                delta,
                self.in_flight_pos_f64()
            );
            let new_scaled = (authoritative_pos * POS_SCALE) as i64;
            self.real_pos.store(new_scaled, Ordering::Release);
        }
        delta
    }
}

/// Shadow Ledger Manager
///
/// Spawns a background task that continuously consumes events from the ring buffer
/// and updates the local state. The hot-path can read state with <1μs latency.
pub struct ShadowLedgerManager {
    state: Arc<RwLock<ShadowLedger>>,
}

impl ShadowLedgerManager {
    /// Create a new shadow ledger manager
    pub fn new() -> Self {
        Self {
            state: Arc::new(RwLock::new(ShadowLedger::default())),
        }
    }

    /// Get a read-only handle to the state (for hot-path queries)
    pub fn state(&self) -> Arc<RwLock<ShadowLedger>> {
        Arc::clone(&self.state)
    }

    /// Spawn the background event consumer task
    ///
    /// This task continuously polls the event ring buffer and updates the local state.
    /// It yields periodically to avoid burning CPU.
    pub fn spawn_consumer(self, mut event_reader: ShmEventReader) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            tracing::info!("🔄 Shadow Ledger: event consumer started");

            loop {
                // Try to read events
                let mut events_processed = 0;
                while let Some(event) = event_reader.try_read() {
                    // Apply event to state (lock scope is minimal)
                    {
                        let mut state = self.state.write();
                        if let Err(e) = state.apply_event(&event) {
                            tracing::error!("Failed to apply event: {}", e);
                        }
                    } // Lock released here

                    events_processed += 1;

                    // Yield after processing a batch to avoid starving other tasks
                    if events_processed >= 100 {
                        tokio::task::yield_now().await;
                        events_processed = 0;
                    }
                }

                // No events available, yield to avoid CPU burn
                tokio::task::yield_now().await;
                tokio::time::sleep(tokio::time::Duration::from_micros(100)).await;
            }
        })
    }
}

impl Default for ShadowLedgerManager {
    fn default() -> Self {
        Self::new()
    }
}


#[cfg(test)]
mod tests;
