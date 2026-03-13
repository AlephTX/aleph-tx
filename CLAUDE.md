---
description: AlephTX HFT Framework - Technical Implementation Guide
alwaysApply: true
---

# CLAUDE.md

> Rust + Go + Python HFT framework for crypto markets. v5.0.0: Per-order state machine, worst-case bilateral risk, lock-free SHM.

## Build & Test (MANDATORY)

**ALL operations MUST use Makefile. NEVER run cargo/go commands directly.**

```bash
# Build
make build          # Build all (Go feeder + Rust)

# Run strategies
make lighter-up STRATEGY=inventory_neutral_mm   # Start Lighter DEX
make lighter-down                               # Stop & cleanup
make lighter-logs                               # View logs

# Available strategies: inventory_neutral_mm, adaptive_mm, simple_mm
# Other exchanges: backpack-up, edgex-up

# Test
make test           # Unit tests
make test-up        # Integration test
make test-down      # Cleanup (MANDATORY)

# Monitor
make status         # Show running strategies
```

## Architecture (CRITICAL)

**Dual-Track IPC**:
- Track 1: `/dev/shm/aleph-matrix` (Lock-free BBO, Seqlock)
- Track 2: `/dev/shm/aleph-events` (128-byte V2 events, per-order tracking)
- Track 3: `/dev/shm/aleph-depth` (L1-L5 depth for OBI+VWMicro pricing)

**No Boomerang**: Rust executes HTTP orders DIRECTLY via FFI. Never sends commands back to Go.

**Optimistic Accounting (v5.0.0)**: `OrderTracker` registers per-order state before API call. Worst-case bilateral exposure checked before every order.

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

| Exchange | REST API | Auth |
|----------|----------|------|
| Lighter DEX | `https://mainnet.zklighter.elliot.ai/api/v1/` | Poseidon2 + EdDSA (FFI) |
| Backpack | `https://api.backpack.exchange` | Ed25519 (Rust) |
| EdgeX | `https://pro.edgex.exchange` | StarkNet Pedersen |

**Lighter DEX**: Chain ID 304, price in cents (×100), FFI lib: `src/native/lighter-signer-linux-amd64.so`

## Context Hierarchy

```
CLAUDE.md (root)                    -> Project contract
  feeder/CLAUDE.md                  -> Go WS ingestion, SHM writers
  src/CLAUDE.md                     -> Rust HFT engine
    src/order_tracker.rs            -> Per-order state machine (v5.0.0)
    src/strategy/CLAUDE.md          -> Strategies
    src/exchanges/CLAUDE.md         -> Exchange integrations
```

Claude auto-loads all CLAUDE.md files = zero warm-up, full project awareness.

## Git Conventions

- Format: `type(scope): description` ([Conventional Commits](https://www.conventionalcommits.org/))
- Types: `feat`, `fix`, `refactor`, `docs`, `test`, `perf`, `chore`
- Subject line < 72 chars

## Reference

- **Workflow rules**: `docs/WORKFLOW.md`
- **Version history**: `docs/CHANGELOG.md` (v4.0.0 optimizations, v3.3.0 unified Makefile)
- **CLAUDE.md conventions**: `docs/CLAUDE_MD_CONVENTIONS.md`
