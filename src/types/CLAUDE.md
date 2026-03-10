---
description: Core type definitions - C-ABI events (V1 64-byte + V2 128-byte), order types, symbols, positions
alwaysApply: true
---

# src/types/

> Core type definitions shared across the entire Rust codebase.

## Key Files

| File | Description |
|------|-------------|
| mod.rs | General types: `Symbol`, `Side`, `OrderType`, `OrderStatus`, `Order`, `Position`, `Balance`, `Orderbook` |
| events.rs | `ShmPrivateEvent` (V1, 64-byte) + `ShmPrivateEventV2` (V2, 128-byte) with compile-time size assertions |

## ShmPrivateEvent V1 (64 bytes) — DEPRECATED

Legacy 64-byte event. Lacks `client_order_id`, causing ID mismatch in order tracking.

```
#[repr(C, align(64))]  // 64 bytes, single cache line
ShmPrivateEvent {
    sequence: u64,        // 0..8
    exchange_id: u8,      // 8
    event_type: u8,       // 9
    symbol_id: u16,       // 10..12
    _pad1: u32,           // 12..16
    order_id: u64,        // 16..24
    fill_price: f64,      // 24..32
    fill_size: f64,       // 32..40
    remaining_size: f64,  // 40..48
    fee_paid: f64,        // 48..56
    is_ask: u8,           // 56
    _padding: [u8; 7],    // 57..64
}
```

## ShmPrivateEventV2 (128 bytes) — v5.0.0

Dual cache line event with full order lifecycle support.

```
#[repr(C, align(64))]  // 128 bytes, dual cache line
ShmPrivateEventV2 {
    // Cache Line 1 (64 bytes)
    sequence: u64,              // Monotonic sequence number
    exchange_id: u8,            // Exchange identifier
    event_type: u8,             // 1=Created, 2=Filled, 3=Canceled, 4=Rejected
    symbol_id: u16,             // Trading pair
    _pad1: u32,
    exchange_order_id: u64,     // Exchange-assigned order ID
    fill_price: f64,            // Fill price (0 if not fill)
    fill_size: f64,             // Fill size (0 if not fill)
    remaining_size: f64,        // Remaining order size
    fee_paid: f64,              // Fee (negative = rebate)
    is_ask: u8,                 // 1=sell, 0=buy
    _padding1: [u8; 7],

    // Cache Line 2 (64 bytes) — NEW in v5.0.0
    client_order_id: i64,       // YOUR order ID (delayed binding key)
    order_index: i64,           // Exchange order index (for cancel API)
    original_size: f64,         // Original order size
    order_price: f64,           // Order price
    timestamp_ns: u64,          // Event timestamp (nanoseconds)
    trade_id: u64,              // Trade ID (for fill dedup)
    _reserved: [u8; 16],       // Reserved for future use
}
```

## Gotchas

- V2 struct MUST remain exactly 128 bytes. Verified at compile time.
- Any field change requires updating BOTH Rust (`events.rs`) AND Go (`feeder/shm/events.go`).
- V1 kept for backward compatibility. New code should use V2.
