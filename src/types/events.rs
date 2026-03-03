//! Private Event Schema for Lock-Free IPC
//!
//! This module defines the C-ABI compatible event structure for private order flow.
//! Events are written by the Go feeder and consumed by Rust strategies via shared memory.

use std::fmt;

/// Private event types from exchange WebSocket streams
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventType {
    /// Order successfully created on exchange
    OrderCreated = 1,
    /// Order filled (partial or complete)
    OrderFilled = 2,
    /// Order canceled
    OrderCanceled = 3,
    /// Order rejected by exchange
    OrderRejected = 4,
}

impl EventType {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::OrderCreated),
            2 => Some(Self::OrderFilled),
            3 => Some(Self::OrderCanceled),
            4 => Some(Self::OrderRejected),
            _ => None,
        }
    }
}

/// C-ABI compatible private event structure
///
/// Memory layout is critical for cross-language IPC:
/// - 64-byte aligned for cache line optimization
/// - repr(C) for stable memory layout
/// - All fields are POD types (no pointers, no heap allocation)
#[repr(C, align(64))]
#[derive(Clone, Copy)]
pub struct ShmPrivateEvent {
    /// Monotonically increasing sequence number (for detecting gaps)
    pub sequence: u64,

    /// Exchange ID (Lighter = 2, Backpack = 5, EdgeX = 3)
    pub exchange_id: u8,

    /// Event type (see EventType enum)
    pub event_type: u8,

    /// Symbol ID (BTC = 0, ETH = 1, etc.)
    pub symbol_id: u16,

    /// Padding to align order_id to 8-byte boundary
    _pad1: u32,

    /// Exchange-specific order ID
    pub order_id: u64,

    /// Fill price (0.0 if not a fill event)
    pub fill_price: f64,

    /// Fill size (0.0 if not a fill event)
    pub fill_size: f64,

    /// Remaining order size after this event
    pub remaining_size: f64,

    /// Fee paid for this event (negative = rebate)
    pub fee_paid: f64,

    /// Padding to ensure 64-byte total size
    _padding: [u8; 8],
}

impl ShmPrivateEvent {
    /// Create a new order created event
    pub fn order_created(
        sequence: u64,
        exchange_id: u8,
        symbol_id: u16,
        order_id: u64,
        size: f64,
    ) -> Self {
        Self {
            sequence,
            exchange_id,
            event_type: EventType::OrderCreated as u8,
            symbol_id,
            _pad1: 0,
            order_id,
            fill_price: 0.0,
            fill_size: 0.0,
            remaining_size: size,
            fee_paid: 0.0,
            _padding: [0; 8],
        }
    }

    /// Create a new order filled event
    #[allow(clippy::too_many_arguments)]
    pub fn order_filled(
        sequence: u64,
        exchange_id: u8,
        symbol_id: u16,
        order_id: u64,
        fill_price: f64,
        fill_size: f64,
        remaining_size: f64,
        fee_paid: f64,
    ) -> Self {
        Self {
            sequence,
            exchange_id,
            event_type: EventType::OrderFilled as u8,
            symbol_id,
            _pad1: 0,
            order_id,
            fill_price,
            fill_size,
            remaining_size,
            fee_paid,
            _padding: [0; 8],
        }
    }

    /// Create a new order canceled event
    pub fn order_canceled(
        sequence: u64,
        exchange_id: u8,
        symbol_id: u16,
        order_id: u64,
    ) -> Self {
        Self {
            sequence,
            exchange_id,
            event_type: EventType::OrderCanceled as u8,
            symbol_id,
            _pad1: 0,
            order_id,
            fill_price: 0.0,
            fill_size: 0.0,
            remaining_size: 0.0,
            fee_paid: 0.0,
            _padding: [0; 8],
        }
    }

    /// Create a new order rejected event
    pub fn order_rejected(
        sequence: u64,
        exchange_id: u8,
        symbol_id: u16,
        order_id: u64,
    ) -> Self {
        Self {
            sequence,
            exchange_id,
            event_type: EventType::OrderRejected as u8,
            symbol_id,
            _pad1: 0,
            order_id,
            fill_price: 0.0,
            fill_size: 0.0,
            remaining_size: 0.0,
            fee_paid: 0.0,
            _padding: [0; 8],
        }
    }

    /// Get the event type as an enum
    pub fn event_type(&self) -> Option<EventType> {
        EventType::from_u8(self.event_type)
    }
}

impl fmt::Debug for ShmPrivateEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ShmPrivateEvent")
            .field("sequence", &self.sequence)
            .field("exchange_id", &self.exchange_id)
            .field("event_type", &self.event_type().unwrap_or(EventType::OrderCreated))
            .field("symbol_id", &self.symbol_id)
            .field("order_id", &self.order_id)
            .field("fill_price", &self.fill_price)
            .field("fill_size", &self.fill_size)
            .field("remaining_size", &self.remaining_size)
            .field("fee_paid", &self.fee_paid)
            .finish()
    }
}

impl Default for ShmPrivateEvent {
    fn default() -> Self {
        Self {
            sequence: 0,
            exchange_id: 0,
            event_type: 0,
            symbol_id: 0,
            _pad1: 0,
            order_id: 0,
            fill_price: 0.0,
            fill_size: 0.0,
            remaining_size: 0.0,
            fee_paid: 0.0,
            _padding: [0; 8],
        }
    }
}

// Compile-time assertions to ensure correct memory layout
const _: () = {
    assert!(std::mem::size_of::<ShmPrivateEvent>() == 64);
    assert!(std::mem::align_of::<ShmPrivateEvent>() == 64);
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_event_size_and_alignment() {
        assert_eq!(std::mem::size_of::<ShmPrivateEvent>(), 64);
        assert_eq!(std::mem::align_of::<ShmPrivateEvent>(), 64);
    }

    #[test]
    fn test_order_created() {
        let event = ShmPrivateEvent::order_created(1, 2, 0, 12345, 1.5);
        assert_eq!(event.sequence, 1);
        assert_eq!(event.exchange_id, 2);
        assert_eq!(event.event_type().unwrap(), EventType::OrderCreated);
        assert_eq!(event.symbol_id, 0);
        assert_eq!(event.order_id, 12345);
        assert_eq!(event.remaining_size, 1.5);
    }

    #[test]
    fn test_order_filled() {
        let event = ShmPrivateEvent::order_filled(2, 2, 1, 67890, 3000.0, 0.5, 1.0, 0.15);
        assert_eq!(event.sequence, 2);
        assert_eq!(event.event_type().unwrap(), EventType::OrderFilled);
        assert_eq!(event.fill_price, 3000.0);
        assert_eq!(event.fill_size, 0.5);
        assert_eq!(event.remaining_size, 1.0);
        assert_eq!(event.fee_paid, 0.15);
    }

    #[test]
    fn test_order_canceled() {
        let event = ShmPrivateEvent::order_canceled(3, 2, 0, 12345);
        assert_eq!(event.event_type().unwrap(), EventType::OrderCanceled);
        assert_eq!(event.order_id, 12345);
    }
}
