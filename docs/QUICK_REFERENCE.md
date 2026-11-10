# Engineering quick reference

## Execution invariants

| Boundary | Review criterion | Code |
| --- | --- | --- |
| Order accounting | Count resting creates, pending cancels and uncertain submissions in worst-case exposure. Release risk after authoritative terminal and position evidence. | [Order tracker](../src/order_tracker.rs) |
| Quotes | Enforce fresh books, post-only execution when hedged, native precision and per-side exposure before submission. | [Inventory strategy](../src/strategy/inventory_neutral_mm.rs) |
| Private events | Treat ring overflow, an inconsistent sequence or a stream gap as a reason to reconcile against the account. | [Private event ABI](DUAL_TRACK_IPC.md) |
| Hedge IOC | Persist the intent before dispatch. Reconcile a missing acknowledgment by client order ID and account position before any replacement. | [Live hedge](../src/live_hedge.rs) |
| Session recovery | Preserve the active journal until both venues confirm cleanup. Bind evidence to the exact account, market and order. | [Recovery runbook](LIVE_TRADING.md#execution-and-recovery) |
| Paper execution | Enforce source and availability times, native minimums, common lots and residual exposure. Report every unresolved outcome. | [Relative value](PERP_RELATIVE_VALUE.md) |

## Verification

Run these commands from the repository root on a local fixture or test
service. They exercise the Rust and Go paths without starting a trading
session.

```sh
make fmt-check
make check
make test
make build
make trade MODE=check CONFIG=config.example.toml
```

For a change to a shared wire format, check both writer and reader and
compare ABI sizes, field offsets and publication order. For recovery
changes, exercise partial fills, missing acknowledgments, stale account
reads and interrupted cleanup. [Engineering guide](../AGENTS.md).
