//! Shadow Ledger - Optimistic State Machine for Zero-Latency Position Tracking
//!
//! This module maintains a local copy of account state (positions, orders, PnL)
//! by consuming private events from the event ring buffer. This enables:
//! - <1μs state queries (vs 50-200ms REST API)
//! - Real-time event-driven updates
//! - Zero API calls for state queries

use crate::error::{Result, TradingError};
use crate::shm_event_reader::ShmEventReader;
use crate::types::{EventType, ShmPrivateEvent};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

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
}

/// Local account state (shadow ledger)
#[derive(Debug, Clone)]
pub struct ShadowLedger {
    /// Confirmed position (reconciled from WS events)
    pub real_pos: f64,

    /// Optimistic in-flight position (orders sent but not yet confirmed)
    pub in_flight_pos: f64,

    /// Realized PnL (USD)
    pub realized_pnl: f64,

    /// Active orders (order_id -> OrderState)
    pub active_orders: HashMap<u64, OrderState>,

    /// Last event sequence number processed
    pub last_sequence: u64,

    /// Last update timestamp
    pub last_update: Instant,
}

impl Default for ShadowLedger {
    fn default() -> Self {
        Self {
            real_pos: 0.0,
            in_flight_pos: 0.0,
            realized_pnl: 0.0,
            active_orders: HashMap::new(),
            last_sequence: 0,
            last_update: Instant::now(),
        }
    }
}

impl ShadowLedger {
    /// Get total exposure (real + in_flight)
    pub fn total_exposure(&self) -> f64 {
        self.real_pos + self.in_flight_pos
    }

    /// Add to in_flight position (optimistic accounting)
    pub fn add_in_flight(&mut self, delta: f64) {
        self.in_flight_pos += delta;
        tracing::debug!(
            "In-flight updated: delta={:.4} new_in_flight={:.4} total_exposure={:.4}",
            delta,
            self.in_flight_pos,
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
                // OrderCreated events confirm that the order was accepted by the exchange
                // We should already have this order in active_orders from place_order_optimistic
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
                    // This can happen if the order was placed before the shadow ledger started
                    // We cannot track it properly without knowing the side
                    tracing::warn!(
                        "OrderCreated event for unknown order: {} (placed before shadow ledger started)",
                        event.order_id
                    );
                }
            }
            Some(EventType::OrderFilled) => {
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
                    let should_remove = order.remaining_size <= 0.0;

                    // CRITICAL: Reconcile in_flight → real_pos
                    // Subtract from in_flight (order is now confirmed)
                    self.in_flight_pos -= signed_fill;
                    // Add to real position
                    self.real_pos += signed_fill;

                    // Update realized PnL correctly for both sides
                    match order_side {
                        OrderSide::Buy => {
                            // Buying costs money (negative PnL)
                            self.realized_pnl -= event.fill_price * event.fill_size + event.fee_paid;
                        }
                        OrderSide::Sell => {
                            // Selling generates revenue (positive PnL)
                            self.realized_pnl += event.fill_price * event.fill_size - event.fee_paid;
                        }
                    }

                    let total_exp = self.total_exposure();

                    tracing::debug!(
                        "Fill reconciled: order={} side={} size={:.4} real_pos={:.4} in_flight={:.4} total={:.4}",
                        event.order_id,
                        order_side,
                        event.fill_size,
                        self.real_pos,
                        self.in_flight_pos,
                        total_exp
                    );

                    // Remove order if fully filled
                    if should_remove {
                        self.active_orders.remove(&event.order_id);
                    }
                } else {
                    tracing::warn!(
                        "Fill event for unknown order: {} (may have been placed before shadow ledger started)",
                        event.order_id
                    );
                }
            }
            Some(EventType::OrderCanceled) => {
                // Rollback in_flight for canceled orders
                if let Some(order) = self.active_orders.get(&event.order_id) {
                    let signed_remaining = order.side.sign() * order.remaining_size;
                    self.in_flight_pos -= signed_remaining;
                    tracing::debug!(
                        "Order canceled: id={} side={} remaining={:.4} rolled back in_flight",
                        event.order_id,
                        order.side,
                        order.remaining_size
                    );
                }
                self.active_orders.remove(&event.order_id);
            }
            Some(EventType::OrderRejected) => {
                // Rollback in_flight for rejected orders
                if let Some(order) = self.active_orders.get(&event.order_id) {
                    let signed_remaining = order.side.sign() * order.remaining_size;
                    self.in_flight_pos -= signed_remaining;
                    tracing::warn!(
                        "Order rejected: id={} side={} size={:.4} rolled back in_flight",
                        event.order_id,
                        order.side,
                        order.initial_size
                    );
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

    /// Get current confirmed position
    pub fn position(&self) -> f64 {
        self.real_pos
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
mod tests {
    use super::*;

    #[test]
    fn test_order_side_sign() {
        assert_eq!(OrderSide::Buy.sign(), 1.0);
        assert_eq!(OrderSide::Sell.sign(), -1.0);
    }

    #[test]
    fn test_order_side_display() {
        assert_eq!(OrderSide::Buy.to_string(), "buy");
        assert_eq!(OrderSide::Sell.to_string(), "sell");
    }

    #[test]
    fn test_shadow_ledger_initial_state() {
        let ledger = ShadowLedger::default();
        assert_eq!(ledger.real_pos, 0.0);
        assert_eq!(ledger.in_flight_pos, 0.0);
        assert_eq!(ledger.realized_pnl, 0.0);
        assert_eq!(ledger.total_exposure(), 0.0);
        assert_eq!(ledger.active_order_count(), 0);
    }

    #[test]
    fn test_add_in_flight() {
        let mut ledger = ShadowLedger::default();

        ledger.add_in_flight(1.5);
        assert_eq!(ledger.in_flight_pos, 1.5);
        assert_eq!(ledger.total_exposure(), 1.5);

        ledger.add_in_flight(-0.5);
        assert_eq!(ledger.in_flight_pos, 1.0);
        assert_eq!(ledger.total_exposure(), 1.0);
    }

    #[test]
    fn test_shadow_ledger_order_created() {
        let mut state = ShadowLedger::default();

        // First register the order (simulating place_order_optimistic)
        state.register_order(12345, 0, OrderSide::Buy, 3000.0, 1.5);
        assert_eq!(state.active_order_count(), 1);

        // Then receive the OrderCreated event (confirmation from exchange)
        let event = ShmPrivateEvent::order_created(1, 2, 0, 12345, 1.5);
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
        assert_eq!(state.in_flight_pos, 1.5);
        assert_eq!(state.total_exposure(), 1.5);

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
            },
        );

        // Fill order (reconciles in_flight → real_pos)
        let fill_event = ShmPrivateEvent::order_filled(2, 2, 0, 12345, 3000.0, 0.5, 1.0, 0.15);
        state.apply_event(&fill_event).unwrap();

        assert_eq!(state.real_pos, 0.5);
        assert_eq!(state.in_flight_pos, 1.0); // 1.5 - 0.5 = 1.0
        assert_eq!(state.total_exposure(), 1.5);
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
            },
        );

        // Cancel order (rollback in_flight)
        let cancel_event = ShmPrivateEvent::order_canceled(2, 2, 0, 12345);
        state.apply_event(&cancel_event).unwrap();

        assert_eq!(state.active_order_count(), 0);
        assert!(!state.has_active_order(12345));
        assert_eq!(state.in_flight_pos, 0.0); // Rolled back
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
            },
        );

        // Fill sell order
        let fill_event = ShmPrivateEvent::order_filled(2, 2, 0, 12346, 51000.0, 1.0, 0.0, 3.0);
        state.apply_event(&fill_event).unwrap();

        // Check reconciliation
        assert_eq!(state.real_pos, -1.0);
        assert_eq!(state.in_flight_pos, 0.0);

        // PnL should be positive (revenue from selling)
        assert!(state.realized_pnl > 0.0);
        let expected_pnl = 51000.0 * 1.0 - 3.0;
        assert!((state.realized_pnl - expected_pnl).abs() < 0.01);
    }

    #[test]
    fn test_sequence_validation() {
        let mut ledger = ShadowLedger::default();

        // First event
        let event1 = ShmPrivateEvent::order_created(1, 2, 0, 12349, 1.0);
        assert!(ledger.apply_event(&event1).is_ok());
        assert_eq!(ledger.last_sequence, 1);

        // Out of order event (should error)
        let event_old = ShmPrivateEvent::order_created(1, 2, 0, 12350, 1.0);
        let result = ledger.apply_event(&event_old);
        assert!(result.is_err());

        // Gap in sequence (should log warning but continue)
        let event_gap = ShmPrivateEvent::order_created(5, 2, 0, 12351, 1.0);
        assert!(ledger.apply_event(&event_gap).is_ok());
        assert_eq!(ledger.last_sequence, 5);
    }
}

