---
description: AlephTX HFT Framework - Technical Implementation Guide
alwaysApply: true
---

# CLAUDE.md

> Rust + Go + Python HFT framework for crypto markets. v6.0.0: external fair value anchor, Binance/HL feeders, momentum-aware spread, position timeout flatten.

## Build & Test (MANDATORY)

**PREFER Makefile for all operations.** Direct cargo/go commands allowed for quick checks (cargo check, cargo clippy), but MUST use Makefile for build/test/run to ensure correct flags and dependencies.

```bash
# Build
make build          # Build all (Go feeder + Rust)

# Run strategies
make lighter-up STRATEGY=inventory_neutral_mm   # Start Lighter DEX
make lighter-down                               # Stop & cleanup
make lighter-logs                               # View logs

# Available strategies: lighter_inventory_mm, lighter_adaptive_mm, backpack_mm, edgex_mm
# Other exchanges: backpack-up, edgex-up

# Build verification
make build          # Verify compilation

# Monitor
make status         # Show running strategies
```

## Architecture (CRITICAL)

**Dual-Track IPC**:
- Track 1: `/dev/shm/aleph-matrix` (Lock-free BBO, Seqlock)
- Track 2: `/dev/shm/aleph-events-v2` (128-byte V2 private event ring, per-order tracking)
- Track 3: `/dev/shm/aleph-depth` (L1-L5 depth for OBI+VWMicro pricing)

`lighter_inventory_mm` now consumes the V2 event ring directly at startup so `OrderTracker`
receives authoritative `created / filled / canceled` transitions and real `order_index` bindings.
Lighter account stats should be treated as websocket-sourced; the previous REST fallback endpoint was invalid.

**No Boomerang**: Rust executes HTTP orders DIRECTLY via FFI. Never sends commands back to Go.

**Optimistic Accounting (v5.0.0)**: `OrderTracker` registers per-order state before API call. Worst-case bilateral exposure checked before every order.

**External Fair Value Anchor (v6.0.0)**: Pricing uses Binance Futures + Hyperliquid mid-price median as the PRIMARY fair value. Lighter local BBO is only used for touch positioning. Key structs: `InventoryContext` (shared inventory params), `AnchorParams` (quote anchoring), `TelemetrySync` (telemetry update).

**Strategy Execution**: Market making is powered by the `InventoryNeutralMM` strategy using external-anchor A-S pricing, momentum-aware asymmetric spread, event-driven order tracking, and multi-tier grid quoting. Position timeout (120s) triggers IOC flatten to prevent overnight inventory drift. Production binaries live in `src/bin/` and run via `cargo run --release --bin lighter_inventory_mm`.
`InventoryNeutralMM` sizing is now equity-aware: prefer USD-notional knobs (`base_order_notional_usd`, `max_position_notional_usd`, `inventory_urgency_notional_usd`) over hardcoded base-unit sizes so the same profile scales across account sizes and symbols.

## Verification Protocol (MANDATORY)

After ANY code changes:
1. Run `make build` to verify compilation
2. For Rust changes: `cargo check` and `cargo clippy` are acceptable for quick verification
3. Never say "完成" or "Phase X 完成" without build verification
4. For multi-phase work, verify each phase independently

## Code Style

**Rust**:
- Use `Result<T, E>`, never `unwrap()` in production
- Hot path: zero heap allocations
- Async: wrap FFI in `tokio::task::spawn_blocking()`
- Never `unsafe` without safety comments

**Go**:
- Always check errors, use `fmt.Errorf` with `%w`
- Minimize CGO calls (expensive)
- Free C memory via Go's `C.free()`, not `libc::free`

**Python**:
- Type hints for function signatures
- Use `with` for resource management

## Global Hard Constraints

- **C-ABI Alignment**: `ShmPrivateEvent` MUST be 64 bytes exactly
- **Divide-by-zero**: Check `last_price == 0.0` before division
- **Memory Barriers**: Use `AtomicU64::load(Ordering::Acquire)` for SHM reads

## Compact Instructions

When compressing context, preserve in priority order:

1. Architecture decisions and constraints (NEVER summarize)
2. Modified files with key changes
3. Current verification status (pass/fail)
4. Open TODOs and rollback notes
5. Tool outputs (can delete, keep pass/fail only)

## Exchanges

| Exchange | REST API | Auth | SHM ID | Role |
|----------|----------|------|--------|------|
| Lighter DEX | `https://mainnet.zklighter.elliot.ai/api/v1/` | Poseidon2 + EdDSA (FFI) | 2 | Trading venue |
| Binance Futures | `wss://fstream.binance.com` (bookTicker) | None (public) | 6 | Fair value anchor |
| Hyperliquid | `wss://api.hyperliquid.xyz/ws` (l2Book) | None (public) | 1 | Fair value anchor |
| EdgeX | `https://pro.edgex.exchange` | StarkNet Pedersen | 3 | Fair value anchor |
| Backpack | `https://api.backpack.exchange` | Ed25519 (Rust) | 5 | Disabled |

**Lighter DEX**: Chain ID 304, price in cents (×100), FFI lib: `src/native/lighter-signer-linux-amd64.so`

## Context Hierarchy

```
CLAUDE.md (root)                    -> Project contract
  feeder/CLAUDE.md                  -> Go WS ingestion, SHM writers
  src/CLAUDE.md                     -> Rust HFT engine
    src/bin/                        -> Production strategy binaries
    src/order_tracker.rs            -> Per-order state machine (v5.0.0)
    src/strategy/CLAUDE.md          -> Strategies
    src/exchanges/CLAUDE.md         -> Exchange integrations
```

Claude auto-loads all CLAUDE.md files = zero warm-up, full project awareness.

## Git Conventions

- Format: `type(scope): description` ([Conventional Commits](https://www.conventionalcommits.org/))
- Types: `feat`, `fix`, `refactor`, `docs`, `test`, `perf`, `chore`
- Subject line < 72 chars

## Git Workflow

After completing a logical unit of work (phase/feature):
1. Run `make build` to verify
2. ASK user: "代码已验证通过。是否创建 commit？"
3. If yes, use /commit skill
4. Never auto-commit without asking

## Documentation Policy

- NEVER create CLAUDE.md files in subdirectories unless explicitly requested
- Architecture documentation belongs in root CLAUDE.md or docs/
- Subdirectory CLAUDE.md only for multi-team projects (not applicable to SIMPLE tier)

## Rollback Procedure

If production issues occur:
1. `git revert <commit-hash>`
2. `make build && make test`
3. `git push`
4. Restart strategies: `make lighter-down && make lighter-up`

## Reference

- **Workflow rules**: `docs/WORKFLOW.md`
- **Version history**: `docs/CHANGELOG.md` (v4.0.0 optimizations, v3.3.0 unified Makefile)
- **CLAUDE.md conventions**: `docs/CLAUDE_MD_CONVENTIONS.md`
