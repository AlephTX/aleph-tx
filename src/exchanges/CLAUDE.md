---
description: Exchange-specific trading clients with unified Exchange trait abstraction
alwaysApply: true
---

# src/exchanges/

> Modular exchange integrations - each exchange in its own subdirectory with gateway implementing Exchange trait.

## Architecture

```
src/exchanges/
  mod.rs                    # pub mod lighter; pub mod backpack; pub mod edgex;
  lighter/
    mod.rs                  # pub mod ffi; pub mod trading;
    ffi.rs                  # FFI bindings to Go signer (lighter-signer-linux-amd64.so)
    trading.rs              # Lighter DEX trading client (implements Exchange trait)
  backpack/
    mod.rs                  # pub mod client; pub mod gateway; pub mod model;
    client.rs               # BackpackClient - REST client with Ed25519 auth
    gateway.rs              # BackpackGateway - Exchange trait implementation
    model.rs                # Data structures (BackpackOrderRequest, BackpackPosition, etc.)
    CLAUDE.md               # Backpack-specific documentation
  edgex/
    mod.rs                  # pub mod client; pub mod gateway; pub mod model; pub mod signature; pub mod pedersen;
    client.rs               # EdgeXClient - REST client with L2 auth
    gateway.rs              # EdgeXGateway - Exchange trait implementation (buy/sell/cancel/batch)
    model.rs                # Data structures (CreateOrderRequest, OpenOrder, Position, etc.)
    signature.rs            # SignatureManager - StarkNet Pedersen hash + EC_ORDER reduction + local verify
    pedersen/mod.rs         # EdgeX-compatible Pedersen hash implementation
    pedersen/pedersen_points.rs  # Pre-computed constant points for Pedersen hash
    CLAUDE.md               # EdgeX-specific documentation
```

## Exchange Trait Abstraction

All exchanges implement the unified `Exchange` trait defined in `src/exchange.rs`:

```rust
#[async_trait]
pub trait Exchange: Send + Sync {
    async fn buy(&self, size: f64, price: f64) -> Result<OrderResult>;
    async fn sell(&self, size: f64, price: f64) -> Result<OrderResult>;
    async fn place_batch(&self, params: BatchOrderParams) -> Result<BatchOrderResult>;
    async fn cancel_order(&self, order_id: i64) -> Result<()>;
    async fn cancel_all(&self) -> Result<u32>;
    async fn get_active_orders(&self) -> Result<Vec<OrderInfo>>;
    async fn close_all_positions(&self, current_price: f64) -> Result<()>;
}
```

## Implementation Status

| Exchange | Client | Gateway | Status |
|----------|--------|---------|--------|
| Lighter  | ✅ trading.rs | ✅ (native impl) | Production-ready |
| Backpack | ✅ client.rs | ✅ gateway.rs | Functional (no batch API) |
| EdgeX    | ✅ client.rs | ✅ gateway.rs | Functional (L2 Pedersen signature complete) |

## Backward Compatibility

`src/lib.rs` provides re-exports for seamless migration:

```rust
pub use exchanges::lighter::ffi as lighter_ffi;
pub use exchanges::lighter::trading as lighter_trading;
pub use exchanges::backpack as backpack_api;
pub use exchanges::edgex as edgex_api;
```

Existing code using `crate::lighter_trading::*` continues to work without changes.

## Key Differences

### Lighter DEX
- **Native batch API**: `sendTxBatch` for atomic bid+ask placement
- **FFI signing**: Go signer via CGO for Poseidon2 + EdDSA
- **Nonce management**: Auto-reset on 21711 (invalid nonce) errors
- **Optimistic accounting**: Integrates with Shadow Ledger for `in_flight_pos` tracking

### Backpack
- **Ed25519 auth**: Pure Rust signing with `ed25519_dalek`
- **No batch API**: Sequential execution of bid/ask orders
- **String-based IDs**: Order IDs are strings, not integers

### EdgeX
- **L2 StarkNet auth**: Pedersen hash + Stark curve signing
- **Complex signature**: Requires `calc_limit_order_hash` with asset IDs, nonce, expiry
- **Pedersen hash**: Uses EdgeX-specific constant points (not standard StarkNet), with EC_ORDER modular reduction

## Testing

```bash
make build          # Build all exchanges
make test-up        # Integration test (Lighter DEX)
make adaptive-up    # Production adaptive MM (Lighter DEX)
```

## Future Work

1. **Config-driven factory**: `main.rs` should instantiate exchanges based on `config.toml`
2. **Dynamic Makefile**: `make run EXCHANGE=backpack STRATEGY=inventory_neutral_mm`
3. **Cross-exchange strategies**: Arbitrage between Lighter/Backpack/EdgeX
