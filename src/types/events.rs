//! Private Event Schema for Lock-Free IPC (V2: 128-byte dual cache line)
//!
//! This module defines the C-ABI compatible event structure for private order flow.
//! Events are written by the Go feeder and consumed by Rust strategies via shared memory.
//!
//! V2 Changes:
//! - Extended to 128 bytes (2 cache lines) for complete order lifecycle tracking
//! - Added client_order_id for per-order state machine reconciliation
//! - Added order_index for cancel API calls
//! - Added trade_id for fill deduplication
//! - Added original_size / order_price for auto-registration of untracked orders

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
    /// Private websocket disconnected and may have dropped order or fill events.
    PrivateStreamLost = 5,
}

impl EventType {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::OrderCreated),
            2 => Some(Self::OrderFilled),
            3 => Some(Self::OrderCanceled),
            4 => Some(Self::OrderRejected),
            5 => Some(Self::PrivateStreamLost),
            _ => None,
        }
    }
}

// ─── V1 Event (64 bytes) — kept for backward compatibility ───────────────────

/// V1 C-ABI compatible private event structure (64 bytes, single cache line)
///
/// DEPRECATED: Use ShmPrivateEventV2 for new code.
#[repr(C, align(64))]
#[derive(Clone, Copy)]
pub struct ShmPrivateEvent {
    pub sequence: u64,
    pub exchange_id: u8,
    pub event_type: u8,
    pub symbol_id: u16,
    _pad1: u32,
    pub order_id: u64,
    pub fill_price: f64,
    pub fill_size: f64,
    pub remaining_size: f64,
    pub fee_paid: f64,
    pub is_ask: u8,
    _padding: [u8; 7],
}

impl ShmPrivateEvent {
    pub fn order_created(
        sequence: u64,
        exchange_id: u8,
        symbol_id: u16,
        order_id: u64,
        size: f64,
        is_ask: bool,
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
            is_ask: if is_ask { 1 } else { 0 },
            _padding: [0; 7],
        }
    }

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
        is_ask: bool,
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
            is_ask: if is_ask { 1 } else { 0 },
            _padding: [0; 7],
        }
    }

    pub fn order_canceled(sequence: u64, exchange_id: u8, symbol_id: u16, order_id: u64) -> Self {
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
            is_ask: 0,
            _padding: [0; 7],
        }
    }

    pub fn order_rejected(sequence: u64, exchange_id: u8, symbol_id: u16, order_id: u64) -> Self {
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
            is_ask: 0,
            _padding: [0; 7],
        }
    }

    pub fn event_type(&self) -> Option<EventType> {
        EventType::from_u8(self.event_type)
    }
}

impl fmt::Debug for ShmPrivateEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ShmPrivateEvent")
            .field("sequence", &self.sequence)
            .field("exchange_id", &self.exchange_id)
            .field(
                "event_type",
                &self.event_type().unwrap_or(EventType::OrderCreated),
            )
            .field("symbol_id", &self.symbol_id)
            .field("order_id", &self.order_id)
            .field("fill_price", &self.fill_price)
            .field("fill_size", &self.fill_size)
            .field("remaining_size", &self.remaining_size)
            .field("fee_paid", &self.fee_paid)
            .field("is_ask", &(self.is_ask != 0))
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
            is_ask: 0,
            _padding: [0; 7],
        }
    }
}

// V1 compile-time assertions
const _: () = {
    assert!(std::mem::size_of::<ShmPrivateEvent>() == 64);
    assert!(std::mem::align_of::<ShmPrivateEvent>() == 64);
};

// ─── V2 Event (128 bytes) — world-class per-order tracking ──────────────────

/// V2 C-ABI compatible private event structure (128 bytes, dual cache line)
///
/// Key additions over V1:
/// - `client_order_id`: enables per-order state machine (delayed binding)
/// - `order_index`: enables cancel API calls without extra lookups
/// - `trade_id`: enables fill deduplication
/// - `original_size` / `order_price`: enables auto-registration of untracked orders
#[repr(C, align(64))]
#[derive(Clone, Copy)]
pub struct ShmPrivateEventV2 {
    // ─── Cache Line 1 (64 bytes) ─────────────────────────────────────
    /// Monotonically increasing sequence number (for gap detection)
    pub sequence: u64,
    /// Exchange ID (Lighter=2, Backpack=5, EdgeX=3)
    pub exchange_id: u8,
    /// Event type (1=Created, 2=Filled, 3=Canceled, 4=Rejected)
    pub event_type: u8,
    /// Shared-memory symbol ID from the session symbol map.
    pub symbol_id: u16,
    /// Padding to align exchange_order_id to 8-byte boundary
    _pad1: u32,
    /// Exchange-assigned order ID (globally unique)
    pub exchange_order_id: u64,
    /// Fill price (0.0 if not a fill event)
    pub fill_price: f64,
    /// Fill size (0.0 if not a fill event)
    pub fill_size: f64,
    /// Remaining order size after this event
    pub remaining_size: f64,
    /// Fee paid for this event (negative = rebate)
    pub fee_paid: f64,
    /// Order direction (1=ask/sell, 0=bid/buy)
    pub is_ask: u8,
    /// Padding to complete cache line 1
    _padding1: [u8; 7],

    // ─── Cache Line 2 (64 bytes) ─────────────────────────────────────
    /// Client order ID (your local ID, exchange echoes it back)
    pub client_order_id: i64,
    /// Exchange internal order index (used for cancel API)
    pub order_index: i64,
    /// Original order size (for auto-registration)
    pub original_size: f64,
    /// Order price (for auto-registration)
    pub order_price: f64,
    /// Event timestamp in nanoseconds (feeder wall clock)
    pub timestamp_ns: u64,
    /// Trade ID (for fill deduplication, 0 if not a fill)
    pub trade_id: u64,
    /// Provenance of `fee_paid` (see [`FeeSource`]); offset 112. This byte
    /// was reserved and always 0 before it carried a source, so 0 stays
    /// unknown: an older feeder's fills read as unknown, never as zero.
    pub fee_source: u8,
    /// Reserved for future use
    _reserved: [u8; 15],
}

/// Where a V2 fill's `fee_paid` came from, as the Go feeder classifies the
/// rate field selected for this account's role. `fee_paid` itself is
/// unchanged; only the label is new.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FeeSource {
    /// Older writer or no evidence (byte 0).
    Unknown = 0,
    /// Explicit nonzero integer rate in ppm times price and size.
    RateDerived = 1,
    /// The role's rate field was an explicit 0.
    ExplicitZero = 2,
    /// The role's rate field was absent or null; Lighter omits zero or empty
    /// fields, so this is not a known zero.
    Missing = 3,
    /// The role's rate field was not an integer within +-1e6 ppm.
    Invalid = 4,
    /// `is_maker_ask` was not a boolean, so the role was guessed.
    RoleUnknown = 5,
}

impl FeeSource {
    /// Unrecognized bytes (a newer writer) read as unknown.
    pub fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::RateDerived,
            2 => Self::ExplicitZero,
            3 => Self::Missing,
            4 => Self::Invalid,
            5 => Self::RoleUnknown,
            _ => Self::Unknown,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::RateDerived => "rate_derived",
            Self::ExplicitZero => "explicit_zero",
            Self::Missing => "missing",
            Self::Invalid => "invalid",
            Self::RoleUnknown => "role_unknown",
        }
    }
}

impl ShmPrivateEventV2 {
    /// Create an OrderCreated event
    #[allow(clippy::too_many_arguments)]
    pub fn order_created(
        sequence: u64,
        exchange_id: u8,
        symbol_id: u16,
        exchange_order_id: u64,
        client_order_id: i64,
        order_index: i64,
        price: f64,
        size: f64,
        is_ask: bool,
        timestamp_ns: u64,
    ) -> Self {
        Self {
            sequence,
            exchange_id,
            event_type: EventType::OrderCreated as u8,
            symbol_id,
            _pad1: 0,
            exchange_order_id,
            fill_price: 0.0,
            fill_size: 0.0,
            remaining_size: size,
            fee_paid: 0.0,
            is_ask: if is_ask { 1 } else { 0 },
            _padding1: [0; 7],
            client_order_id,
            order_index,
            original_size: size,
            order_price: price,
            timestamp_ns,
            trade_id: 0,
            fee_source: 0,
            _reserved: [0; 15],
        }
    }

    /// Create an OrderFilled event
    #[allow(clippy::too_many_arguments)]
    pub fn order_filled(
        sequence: u64,
        exchange_id: u8,
        symbol_id: u16,
        exchange_order_id: u64,
        client_order_id: i64,
        order_index: i64,
        fill_price: f64,
        fill_size: f64,
        remaining_size: f64,
        fee_paid: f64,
        is_ask: bool,
        timestamp_ns: u64,
        trade_id: u64,
    ) -> Self {
        Self {
            sequence,
            exchange_id,
            event_type: EventType::OrderFilled as u8,
            symbol_id,
            _pad1: 0,
            exchange_order_id,
            fill_price,
            fill_size,
            remaining_size,
            fee_paid,
            is_ask: if is_ask { 1 } else { 0 },
            _padding1: [0; 7],
            client_order_id,
            order_index,
            original_size: 0.0,
            order_price: 0.0,
            timestamp_ns,
            trade_id,
            fee_source: FeeSource::Unknown as u8,
            _reserved: [0; 15],
        }
    }

    /// Create an OrderCanceled event
    #[allow(clippy::too_many_arguments)]
    pub fn order_canceled(
        sequence: u64,
        exchange_id: u8,
        symbol_id: u16,
        exchange_order_id: u64,
        client_order_id: i64,
        order_index: i64,
        remaining_size: f64,
        timestamp_ns: u64,
    ) -> Self {
        Self {
            sequence,
            exchange_id,
            event_type: EventType::OrderCanceled as u8,
            symbol_id,
            _pad1: 0,
            exchange_order_id,
            fill_price: 0.0,
            fill_size: 0.0,
            remaining_size,
            fee_paid: 0.0,
            is_ask: 0,
            _padding1: [0; 7],
            client_order_id,
            order_index,
            original_size: 0.0,
            order_price: 0.0,
            timestamp_ns,
            trade_id: 0,
            fee_source: 0,
            _reserved: [0; 15],
        }
    }

    /// Create an OrderRejected event
    pub fn order_rejected(
        sequence: u64,
        exchange_id: u8,
        symbol_id: u16,
        client_order_id: i64,
        timestamp_ns: u64,
    ) -> Self {
        Self {
            sequence,
            exchange_id,
            event_type: EventType::OrderRejected as u8,
            symbol_id,
            _pad1: 0,
            exchange_order_id: 0,
            fill_price: 0.0,
            fill_size: 0.0,
            remaining_size: 0.0,
            fee_paid: 0.0,
            is_ask: 0,
            _padding1: [0; 7],
            client_order_id,
            order_index: 0,
            original_size: 0.0,
            order_price: 0.0,
            timestamp_ns,
            trade_id: 0,
            fee_source: 0,
            _reserved: [0; 15],
        }
    }

    /// Get the event type as an enum
    pub fn event_type(&self) -> Option<EventType> {
        EventType::from_u8(self.event_type)
    }

    /// Provenance of `fee_paid`; unknown for older writers.
    pub fn fee_source(&self) -> FeeSource {
        FeeSource::from_u8(self.fee_source)
    }

    /// The same event labeled with a fee source, as the feeder writes it.
    pub fn with_fee_source(mut self, source: FeeSource) -> Self {
        self.fee_source = source as u8;
        self
    }
}

impl fmt::Debug for ShmPrivateEventV2 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ShmPrivateEventV2")
            .field("sequence", &self.sequence)
            .field("exchange_id", &self.exchange_id)
            .field(
                "event_type",
                &self.event_type().unwrap_or(EventType::OrderCreated),
            )
            .field("symbol_id", &self.symbol_id)
            .field("exchange_order_id", &self.exchange_order_id)
            .field("client_order_id", &self.client_order_id)
            .field("order_index", &self.order_index)
            .field("fill_price", &self.fill_price)
            .field("fill_size", &self.fill_size)
            .field("remaining_size", &self.remaining_size)
            .field("fee_paid", &self.fee_paid)
            .field("is_ask", &(self.is_ask != 0))
            .field("original_size", &self.original_size)
            .field("order_price", &self.order_price)
            .field("trade_id", &self.trade_id)
            .field("fee_source", &self.fee_source())
            .finish()
    }
}

impl Default for ShmPrivateEventV2 {
    fn default() -> Self {
        Self {
            sequence: 0,
            exchange_id: 0,
            event_type: 0,
            symbol_id: 0,
            _pad1: 0,
            exchange_order_id: 0,
            fill_price: 0.0,
            fill_size: 0.0,
            remaining_size: 0.0,
            fee_paid: 0.0,
            is_ask: 0,
            _padding1: [0; 7],
            client_order_id: 0,
            order_index: 0,
            original_size: 0.0,
            order_price: 0.0,
            timestamp_ns: 0,
            trade_id: 0,
            fee_source: 0,
            _reserved: [0; 15],
        }
    }
}

// V2 compile-time assertions: strict 128-byte, 64-byte aligned, and the
// offsets the Go writer uses (feeder/shm TestEventV2LayoutMatchesRust).
const _: () = {
    use std::mem::offset_of;
    assert!(std::mem::size_of::<ShmPrivateEventV2>() == 128);
    assert!(std::mem::align_of::<ShmPrivateEventV2>() == 64);
    assert!(offset_of!(ShmPrivateEventV2, fee_paid) == 48);
    assert!(offset_of!(ShmPrivateEventV2, is_ask) == 56);
    assert!(offset_of!(ShmPrivateEventV2, client_order_id) == 64);
    assert!(offset_of!(ShmPrivateEventV2, timestamp_ns) == 96);
    assert!(offset_of!(ShmPrivateEventV2, trade_id) == 104);
    assert!(offset_of!(ShmPrivateEventV2, fee_source) == 112);
    assert!(offset_of!(ShmPrivateEventV2, _reserved) == 113);
};

#[cfg(test)]
mod tests;
