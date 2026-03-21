---
description: Trading strategies - arbitrage, market making (EdgeX, Backpack, Lighter), adaptive MM
alwaysApply: true
---

# src/strategy/

> Strategy implementations sharing the common `Strategy` trait. Each strategy reads SHM and executes orders directly.

## Key Files

| File | Description |
|------|-------------|
| mod.rs | `Strategy` trait definition (`on_bbo_update`, `on_idle`, `on_shutdown`) |
| arbitrage.rs | Cross-exchange statistical arbitrage scanner (25 bps threshold) |
| edgex_mm.rs | EdgeX market maker V3 (EWMA volatility, dynamic sizing, legacy direct API) |
| backpack_mm.rs | Backpack market maker (Ed25519 auth, momentum-based spread) |
| lighter_adaptive_mm.rs | Lighter DEX adaptive MM (premium account, fee-aware, microstructure signals) |
| inventory_neutral_mm.rs | Inventory-Neutral MM v6.0 - production HFT (external fair value anchor, A-S pricing, momentum spread, position timeout) |

## Strategy Trait

```rust
pub trait Strategy {
    fn name(&self) -> &str;
    fn on_bbo_update(&mut self, symbol_id: u16, exchange_id: u8, bbo: &ShmBboMessage);
    fn on_idle(&mut self);
    fn on_shutdown(&mut self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}
```

## Architecture

```mermaid
graph TD
    TRAIT[Strategy Trait] --> ARB[ArbitrageEngine]
    TRAIT --> MM[MarketMaker - EdgeX]
    TRAIT --> BPM[BackpackMM]
    TRAIT --> LMM[InventoryNeutralMM v6.0]
    TRAIT --> AMM[AdaptiveMM]

    SHM[SHM BBO Matrix 7 exchanges] --> ARB & MM & BPM & LMM & AMM
    BIN[Binance+HL+EdgeX] -->|external_fair_value_mid| LMM
    ACC[SHM Account Stats] --> AMM
    OT[OrderTracker v5.0.0] --> LMM & AMM

    LMM & AMM -->|FFI + HTTP| LIGHTER[Lighter API]
    MM -->|REST| EDGEX[EdgeX API]
    BPM -->|REST| BACKPACK[Backpack API]
```

## Key Design Patterns

- **No Boomerang**: Strategies fire HTTP orders directly, never send commands back to Go.
- **External Fair Value Anchor (v6.0.0)**: `InventoryNeutralMM` uses median of Binance/HL/EdgeX mid-prices as primary fair value. Lighter local BBO only for touch positioning.
- **Momentum-Aware Spread (v6.0.0)**: Asymmetric half-spreads widen against momentum direction to reduce adverse selection.
- **Position Timeout Flatten (v6.0.0)**: Positions held >2min beyond deadband trigger IOC flatten.
- **Optimistic Accounting (v5.0.0)**: Per-order `start_tracking()` before API call. `mark_failed()` on error. Reconciled via `OrderTracker.apply_event()` from V2 event ring buffer.
- **Incremental Quoting**: Only requote when price moves past threshold (reduces API load).
- **Fee-Aware Spread** (adaptive_mm): Ensures spread > round-trip fee (0.76 bps for Premium).

## Key Structs (v6.0.0)

- `InventoryContext`: Shared (config, position, base_size, urgency, mid) for defer/sizing functions
- `AnchorParams`: Quote anchoring parameters for `anchor_quotes_to_touch`
- `TelemetrySync`: Telemetry snapshot fields for `sync_telemetry_snapshot`

## Gotchas

- `lighter_mm.rs` has been deleted. `inventory_neutral_mm.rs` is the production replacement.
- `adaptive_mm.rs`: Uses `MicrostructureTracker` (EWMA fast/slow, realized vol, adverse selection).
- Order TTL: Stale orders canceled after 6s to prevent position drift.
- `inventory_adjusted_half_spreads` is dead code (kept for reference, replaced by momentum spread).
