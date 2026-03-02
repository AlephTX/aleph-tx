//! Shadow Ledger - Optimistic State Machine for Zero-Latency Position Tracking
//!
//! This module maintains a local copy of account state (positions, orders, PnL)
//! by consuming private events from the event ring buffer. This enables:
//! - <1μs state queries (vs 50-200ms REST API)
//! - Real-time event-driven updates
//! - Zero API calls for state queries

use crate::shm_event_reader::ShmEventReader;
use crate::types::{EventType, ShmPrivateEvent};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

/// Order state tracked in the shadow ledger
#[derive(Debug, Clone)]
pub struct OrderState {
    pub order_id: u64,
    pub symbol_id: u16,
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
pub struct LocalState {
    /// Current position size (positive = long, negative = short)
    pub live_pos: f64,

    /// Realized PnL (USD)
    pub realized_pnl: f64,

    /// Active orders (order_id -> OrderState)
    pub active_orders: HashMap<u64, OrderState>,

    /// Last event sequence number processed
    pub last_sequence: u64,

    /// Last update timestamp
    pub last_update: Instant,
}

impl Default for LocalState {
    fn default() -> Self {
        Self {
            live_pos: 0.0,
            realized_pnl: 0.0,
            active_orders: HashMap::new(),
            last_sequence: 0,
            last_update: Instant::now(),
        }
    }
}

impl LocalState {
    /// Apply an event to the local state
    pub fn apply_event(&mut self, event: &ShmPrivateEvent) {
        self.last_sequence = event.sequence;
        self.last_update = Instant::now();

        match event.event_type() {
            Some(EventType::OrderCreated) => {
                self.active_orders.insert(
                    event.order_id,
                    OrderState {
                        order_id: event.order_id,
                        symbol_id: event.symbol_id,
                        initial_size: event.remaining_size,
                        filled_size: 0.0,
                        remaining_size: event.remaining_size,
                        avg_fill_price: 0.0,
                        total_fees: 0.0,
                        created_at: Instant::now(),
                        last_update: Instant::now(),
                    },
                );
            }
            Some(EventType::OrderFilled) => {
                if let Some(order) = self.active_orders.get_mut(&event.order_id) {
                    // Update order state
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

                    // Update position (assuming buy increases position)
                    // TODO: Need to track order side (buy/sell) in event
                    self.live_pos += event.fill_size;

                    // Update realized PnL (negative = cost)
                    self.realized_pnl -= event.fill_price * event.fill_size + event.fee_paid;

                    // Remove order if fully filled
                    if order.remaining_size <= 0.0 {
                        self.active_orders.remove(&event.order_id);
                    }
                }
            }
            Some(EventType::OrderCanceled) => {
                self.active_orders.remove(&event.order_id);
            }
            Some(EventType::OrderRejected) => {
                self.active_orders.remove(&event.order_id);
            }
            None => {
                tracing::warn!("Unknown event type: {}", event.event_type);
            }
        }
    }

    /// Get current position size
    pub fn position(&self) -> f64 {
        self.live_pos
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
pub struct ShadowLedger {
    state: Arc<RwLock<LocalState>>,
}

impl ShadowLedger {
    /// Create a new shadow ledger
    pub fn new() -> Self {
        Self {
            state: Arc::new(RwLock::new(LocalState::default())),
        }
    }

    /// Get a read-only handle to the state (for hot-path queries)
    pub fn state(&self) -> Arc<RwLock<LocalState>> {
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
                        state.apply_event(&event);
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

impl Default for ShadowLedger {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_local_state_order_created() {
        let mut state = LocalState::default();
        let event = ShmPrivateEvent::order_created(1, 2, 0, 12345, 1.5);

        state.apply_event(&event);

        assert_eq!(state.active_order_count(), 1);
        assert!(state.has_active_order(12345));
    }

    #[test]
    fn test_local_state_order_filled() {
        let mut state = LocalState::default();

        // Create order
        let create_event = ShmPrivateEvent::order_created(1, 2, 0, 12345, 1.5);
        state.apply_event(&create_event);

        // Fill order
        let fill_event = ShmPrivateEvent::order_filled(2, 2, 0, 12345, 3000.0, 0.5, 1.0, 0.15);
        state.apply_event(&fill_event);

        assert_eq!(state.position(), 0.5);
        assert_eq!(state.active_order_count(), 1); // Still active (partial fill)

        let order = state.active_orders.get(&12345).unwrap();
        assert_eq!(order.filled_size, 0.5);
        assert_eq!(order.remaining_size, 1.0);
    }

    #[test]
    fn test_local_state_order_canceled() {
        let mut state = LocalState::default();

        // Create order
        let create_event = ShmPrivateEvent::order_created(1, 2, 0, 12345, 1.5);
        state.apply_event(&create_event);

        // Cancel order
        let cancel_event = ShmPrivateEvent::order_canceled(2, 2, 0, 12345);
        state.apply_event(&cancel_event);

        assert_eq!(state.active_order_count(), 0);
        assert!(!state.has_active_order(12345));
    }
}
