# Engineering Quick Reference

## Core Safety & Execution Invariants

| Boundary | Invariant & Review Criterion | Primary Implementation |
| --- | --- | --- |
| **Shared-Memory ABI (`12` Slots)** | Keep `symbol_id`, exchange slot ID (`1..12`), and venue `market_id` distinct. Verify seqlock size, alignment (`64B` BBO / `256B` Depth / `128B` V2 Events), and publication order across Go and Rust. | [Dual-track IPC](DUAL_TRACK_IPC.md) · [Go matrix](../feeder/shm/matrix.go) · [Rust reader](../src/shm_reader.rs) |
| **Order & Risk Accounting** | Reserve worst-case exposure before dispatch and retain it across pending cancels, ambiguous acks, and stream gaps. Release risk only after authoritative terminal evidence and fresh position/fill watermarks. | [Order tracker](../src/order_tracker.rs) |
| **Quoting & Adverse-Selection Shield** | Enforce fresh books, post-only execution when hedged, native tick/lot precision, and per-side exposure bounds. Use asymmetric per-side cancel shields (`per_side_bound_cancel`) rather than full-book micro-noise cancels to preserve FIFO queue priority. | [Inventory strategy](../src/strategy/inventory_neutral_mm.rs) |
| **Lighter Multi-Lane & Nonce State** | Isolate Maker, Cancel (`LIGHTER_CANCEL_*`), and Taker (`LIGHTER_TAKER_*`) signing lanes when configured. Preserve unconsumed local nonces on pre-execution rejections (`21104`/`23000`) and block cross-collateral (`USDG`/`USDC`) hedging on `RobinhoodMainnet` (`466324`). | [Lighter trading](../src/exchanges/lighter/trading.rs) · [Lighter risk](../src/exchanges/lighter/risk.rs) |
| **Protected Hedge IOC & WAL Recovery** | Persist write-ahead hedge intent before dispatch. Enforce explicit slippage limit bounds on IOC hedges. Block restart while an active journal exists until both venues reconcile flat/clean. | [Live hedge](../src/live_hedge.rs) · [Recovery runbook](LIVE_TRADING.md#execution-and-recovery) |
| **24×7 Capture & Causal Research** | Bound paired VDSO clock reads (`<25 µs`, `±1 ms` drift gate), enforce arrival ordering (`available_at_ms >= received_at_ms`), freeze parameters prior to 10-hour holdouts, and keep `01_quant_profit`, `02_hedge_arbitrage`, and `03_incentive_turnover` ledgers strictly non-subsidized. | [Capture](../src/capture.rs) · [Lakehouse](QUANT_DATA_LAKEHOUSE.md) · [Relative value](PERP_RELATIVE_VALUE.md) |

---

## Standard Verification Suite

Run these commands from the repository root against offline fixtures and synthetic test services (no credentials or live connections required):

```sh
make fmt-check
make check
make test
make test-python
make dashboard-build
make build
make trade MODE=check CONFIG=config.example.toml
```

When modifying a shared wire format or telemetry schema, update and test the Go/Rust writers, Rust/Python readers, TypeScript console types, and synthetic fixtures together.
[Repository engineering guide](../AGENTS.md)
