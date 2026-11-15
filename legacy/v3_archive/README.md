# Legacy v3 / v4 Code Archive

This directory preserves historical v3/v4 exchanges, strategies, accounting primitives, protobuf stubs, and diagnostic examples that have been decoupled from active AlephTX v5.0+ Rust and Go builds.

Keeping these modules outside `src/`, `examples/`, and `feeder/` retains full `git log --follow` provenance without compiling unused execution paths or exposing unmaintained order-submission binaries.

## Archived Tree

| Path | Former Active Path | Reason Archived |
| --- | --- | --- |
| `src/exchanges/backpack/` | `src/exchanges/backpack/` | Superseded v3 Backpack REST/Ed25519 adapter |
| `src/exchanges/edgex/` | `src/exchanges/edgex/` | Superseded v3 EdgeX StarkEx Pedersen/ECDSA adapter |
| `src/strategy/backpack_mm.rs` | `src/strategy/backpack_mm.rs` | Legacy single-leg Backpack market maker |
| `src/strategy/edgex_mm.rs` | `src/strategy/edgex_mm.rs` | Legacy single-leg EdgeX market maker |
| `src/strategy/lighter_adaptive_mm.rs` | `src/strategy/lighter_adaptive_mm.rs` | Replaced by `src/strategy/inventory_neutral_mm.rs` + `OrderTracker` |
| `src/strategy/arbitrage.rs` | `src/strategy/arbitrage.rs` | Early BBO scanner replaced by `src/strategy/relative_value/` and `live_hedge/` |
| `src/shadow_ledger.rs`, `src/shadow_ledger/` | `src/shadow_ledger*` | Dual-accumulator accounting replaced by per-order `src/order_tracker.rs` + V2 events |
| `src/main.rs` | `src/main.rs` (`aleph-tx`) | Legacy multi-strategy runner replaced by supervised `src/bin/lighter_inventory_mm.rs` and `src/bin/cross_venue.rs` |
| `src/bin/{backpack_mm,edgex_mm,lighter_adaptive_mm}.rs` | `src/bin/*.rs` | Legacy standalone strategy launchers |
| `feeder/exchanges/{backpack,edgex,01}.go` | `feeder/exchanges/*.go` | Unused v3 WebSocket adapters |
| `examples/*.rs` (14 files) | `examples/*.rs` | EdgeX StarkEx Pedersen/signature verification utilities |
| `proto/` | `proto/` | Unused v3 gRPC `.proto` definitions (IPC uses lock-free `/dev/shm` mappings) |

## Preserved Compatibility Boundaries

- **Shared-Memory Matrix ABI (`NUM_EXCHANGES = 7`)**: Slot IDs `EXCH_EDGEX = 3`, `EXCH_01 = 4`, and `EXCH_BACKPACK = 5` remain reserved in [src/config.rs](../../src/config.rs) and [feeder/exchanges/common.go](../../feeder/exchanges/common.go) so the 64-byte `ShmBboMessage` matrix layout stays byte-identical across Go and Rust.
- **Profile Compatibility**: Optional `[backpack]` and `[edgex]` tables remain accepted (and ignored when `feeder_enabled = false`) by [src/config.rs](../../src/config.rs) and [feeder/config/config.go](../../feeder/config/config.go) so older local profiles do not fail TOML parsing.
