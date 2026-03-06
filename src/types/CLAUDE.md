---
description: Core type definitions - C-ABI events, order types, symbols, positions
alwaysApply: true
---

# src/types/

> Core type definitions shared across the entire Rust codebase.

## Key Files

| File | Description |
|------|-------------|
| mod.rs | General types: `Symbol`, `Side`, `OrderType`, `OrderStatus`, `Order`, `Position`, `Balance`, `Orderbook` |
| events.rs | `ShmPrivateEvent` (64-byte C-ABI struct) with compile-time size assertions |

## Critical: ShmPrivateEvent (64 bytes)

This struct is shared with Go via shared memory. Any change MUST be mirrored in `feeder/shm/events.go`.

```rust
#[repr(C)]
pub struct ShmPrivateEvent {
    pub sequence: u64,        // 0..8
    pub exchange_id: u8,      // 8
    pub event_type: u8,       // 9  (1=Created, 2=Filled, 3=Canceled, 4=Rejected)
    pub symbol_id: u16,       // 10..12
    pub _pad1: u32,           // 12..16
    pub order_id: u64,        // 16..24
    pub fill_price: f64,      // 24..32
    pub fill_size: f64,       // 32..40
    pub remaining_size: f64,  // 40..48
    pub fee_paid: f64,        // 48..56
    pub _padding: [u8; 8],    // 56..64
}
static_assertions::assert_eq_size!(ShmPrivateEvent, [u8; 64]);
```

## Gotchas

- **C-ABI Alignment**: `ShmPrivateEvent` MUST remain exactly 64 bytes. Verified at compile time.
- Any field change requires updating BOTH Rust (`events.rs`) AND Go (`feeder/shm/events.go`).
