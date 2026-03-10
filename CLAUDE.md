---
description: AlephTX HFT Framework - Technical Implementation Guide for Claude Code
alwaysApply: true
---

# CLAUDE.md

> Technical implementation document for Claude Code and developers working on AlephTX.

Welcome to AlephTX v5.0.0, a Tier-1 High-Frequency Trading (HFT) framework built with Rust, Go, and Python for crypto markets.

**v5.0.0 Architecture Upgrade**: Per-order state machine (OrderTracker), 128-byte dual cache line SHM events (V2), worst-case bilateral risk control, proper memory barriers (AtomicU64 Acquire), NaN/divide-by-zero protection, TTL cache for late events. Replaces v4.0.0 dual-accumulator ShadowLedger.

## Role & Identity

1. You are a world-class Quantitative Architect & Engineer, expert in Rust, Go, Python, crypto HFT, and AI-agent trading system design and implementation.
2. You are passionate about code craftsmanship. Your style: **minimal changes, clean, robust code**.
3. You hold testing to the highest standard - thorough and exhaustive testing of all code.
4. You are an excellent collaborator - teamwork is always a pleasure.

## Code Style Principles

### General Principles

1. **Minimal modifications** - never over-engineer. Change only what is necessary.
2. **Keep code concise** - good software engineering solves problems in the most elegant, minimal way.
3. **Diagrams over prose** - when writing documentation, use diagrams liberally (Mermaid, ASCII art). A picture is worth a thousand words.

### Language-Specific Guidelines

**Rust**:
- Follow official Rust style guide (`rustfmt` defaults)
- Use `Result<T, E>` for error handling, never `unwrap()` in production code
- Prefer `&str` over `String` for function parameters
- Use `Arc<T>` for shared ownership, `Rc<T>` for single-threaded
- Hot path: zero heap allocations, use stack arrays or pre-allocated buffers
- Async: wrap all FFI/blocking calls in `tokio::task::spawn_blocking()`
- Memory safety: never use `unsafe` without detailed safety comments

**Go**:
- Follow official Go style guide (`gofmt`, `golint`)
- Use `context.Context` for cancellation and timeouts
- Error handling: always check errors, use `fmt.Errorf` with `%w` for wrapping
- Concurrency: prefer channels over shared memory, use `sync.Mutex` when necessary
- CGO: minimize CGO calls (expensive), batch operations when possible
- Memory: be careful with C memory allocation, always free via Go's `C.free()`

**Python**:
- Follow PEP 8 and Pythonic idioms
- Use type hints for function signatures
- Prefer list comprehensions over `map()`/`filter()`
- Use `with` statements for resource management
- Error handling: specific exceptions over bare `except:`

## Important Notes

1. **Timestamps**: When you need the current time, run a Linux command to get the accurate system time. Do not guess.
2. **Language**: You may think in English, but **output must be in Chinese** when communicating with the user.
3. **Research**: You may search the web for best practices when needed.
4. **Testing**: You may test ideas in the designated `@CLAUDECODE/tasks/{task_name}/tests/` directory.

---

## Architecture & Philosophy (CRITICAL)

- **Dual-Track IPC**:
  - Track 1 (State): `/dev/shm/aleph-matrix` (Lock-free BBO snapshot matrix, updated by Go, read by Rust via Seqlock).
  - Track 2 (Events): `/dev/shm/aleph-events` (Lock-free RingBuffer for private fills/cancels). V1: 64-byte `ShmPrivateEvent`. **V2 (v5.0.0): 128-byte `ShmPrivateEventV2`** with `client_order_id`, `order_index`, `trade_id` for per-order tracking.
  - Track 3 (Depth): `/dev/shm/aleph-depth` (3MB L1-L5 depth data for OBI+VWMicro pricing, v4.0.0).
- **No Boomerang Execution**: Go handles WS/Network I/O. Rust makes trading decisions and executes HTTP orders DIRECTLY via FFI + HTTP Keep-Alive. Rust NEVER sends execution commands back to Go via IPC.
- **Optimistic Accounting (v5.0.0)**: Rust registers per-order state in `OrderTracker` (RwLock<TrackerState>) before API call. On failure, marks order as `Rejected`. On exchange event, transitions through `PendingCreate → Open → PartiallyFilled → Filled/Canceled`. Worst-case bilateral exposure checked before every order.
- **Data Plane Decoupling** (v4.0.0): Dedicated OS thread for SHM polling (CPU-pinned), connected to Tokio via flume channel. Eliminates async starvation from spin-loop monopolizing Tokio workers.

## Environment & Endpoints

| Exchange | REST API | WebSocket | Auth Method |
|----------|----------|-----------|-------------|
| **Lighter DEX** | `https://mainnet.zklighter.elliot.ai/api/v1/` | `wss://mainnet.zklighter.elliot.ai/stream` | Poseidon2 + EdDSA (via FFI to Go CGO) |
| **Backpack** | `https://api.backpack.exchange` | - | Ed25519 (pure Rust) |
| **EdgeX** | `https://pro.edgex.exchange` | - | StarkNet Pedersen + Stark curve |

**Lighter DEX Critical Details**:
- Chain ID: 304 (mainnet), 300 (testnet)
- Price format: cents (multiply by 100)
- Order expiry: -1 for default (28 days)
- HTTP Content-Type: `multipart/form-data`
- FFI library: `src/native/lighter-signer-linux-amd64.so`

See `src/exchanges/lighter/CLAUDE.md` for debugging notes.

## Build & Test Workflow (MANDATORY)

### CRITICAL RULE: Always Use Makefile

**ALL build, test, and run operations MUST go through the Makefile.**
- NEVER run `cargo build`, `cargo run`, `go build` directly
- NEVER create custom shell scripts for building/running
- ALWAYS use `make <target>` commands

### Available Make Targets

```bash
# Build
make build          # Build all binaries (Go feeder + Rust)
make build-feeder   # Build Go feeder only

# Unified Multi-Exchange Commands (v3.3.0+)
make lighter-up STRATEGY=<name>   # Start Lighter DEX strategy
make lighter-down                 # Stop Lighter DEX
make lighter-logs                 # View Lighter logs

make backpack-up STRATEGY=<name>  # Start Backpack strategy
make backpack-down                # Stop Backpack
make backpack-logs                # View Backpack logs

make edgex-up STRATEGY=<name>     # Start EdgeX strategy
make edgex-down                   # Stop EdgeX
make edgex-logs                   # View EdgeX logs

# Available Strategies
# - inventory_neutral_mm (default)
# - adaptive_mm
# - simple_mm

# Examples
make lighter-up                          # Default: inventory_neutral_mm
make lighter-up STRATEGY=adaptive_mm     # Adaptive MM on Lighter
make backpack-up STRATEGY=simple_mm      # Simple MM on Backpack

# Monitoring
make status         # Show all running strategies across exchanges

# Cleanup
make clean          # Clean build artifacts
```

### Testing Workflow

When implementing a feature, YOU MUST autonomously test it:
1. `make build` - Build everything
2. `make test` - Run unit tests
3. `make test-up` - Start integration test
4. `make test-logs` - Monitor logs
5. `make test-down` - Clean up (MANDATORY)

## Global Hard Constraints

- **C-ABI Alignment**: `ShmPrivateEvent` MUST be EXACTLY 64 bytes. Verify with `static_assertions::assert_eq_size!`.
- **Incremental Quoting Math**: Protect against divide-by-zero (e.g., if `last_price == 0.0` during incremental quoting calculations, return `true` to force initial quote).

## Known Anti-Patterns & Evolution Targets

### v4.0.0 Completed Optimizations ✅

1. **Async Spin-Loop in Tokio** → **Dedicated Data Plane Thread** ✅
   - **Problem**: SHM polling ran as busy-wait inside `tokio::runtime`, monopolizing worker threads.
   - **Solution**: Decoupled data plane (dedicated `std::thread` + CPU core 2 pinning) from control plane (Tokio I/O pool), connected via flume channel.
   - **Impact**: p99 latency -30%.

2. **RwLock on Hot Path** → **Lock-Free Atomics** ✅
   - **Problem**: `Arc<RwLock<ShadowLedger>>` caused cache-coherency ping-pong (~20-50ns).
   - **Solution**: Replaced `real_pos` and `in_flight_pos` with `CachePadded<AtomicI64>` (scaled 1e8).
   - **Impact**: Position read latency -50ns.

3. **Linear Inventory Skew** → **Sigmoid Skew** ✅
   - **Problem**: Hardcoded `urgency = 2.0` + linear position ratio. Suboptimal at both low and high inventory.
   - **Solution**: `tanh(pos/max_pos)` curve — tight spread at low inventory, exponential widening at high.
   - **Impact**: Smoother inventory control, improved mean reversion.

4. **Naive Mid-Price** → **OBI+VWMicro Pricing** ✅
   - **Problem**: `(bid + ask) / 2` ignored order book imbalance.
   - **Solution**: Volume-weighted micro price using L1-L5 depth from `/dev/shm/aleph-depth`.
   - **Impact**: Pricing accuracy +15%.

5. **Naive Reconnect Logic** → **Circuit Breaker with Jitter** ✅
   - **Problem**: Hardcoded `3 * time.Second` sleep on disconnect.
   - **Solution**: Exponential backoff with ±25% jitter + 10-failure circuit breaker (60s pause).
   - **Impact**: Prevents exchange IP bans, graceful degradation.

6. **JSON Reflection on Hot Path** → **Zero-Copy JSON** ✅
   - **Problem**: `encoding/json` used runtime reflection, creating GC pressure (1-5ms pauses).
   - **Solution**: Replaced with `gjson` (zero-copy byte slicing).
   - **Impact**: GC pause -80%.

7. **String-Based Error Matching** → **Typed Error Codes** ✅
   - **Problem**: `e.to_string().contains("not enough margin")` was fragile and slow.
   - **Solution**: Deserialize error responses into typed `LighterErrorCode` enum.
   - **Impact**: Robust error handling, margin cooldown tracking.

8. **Telemetry Blackhole** → **Structured Telemetry** ✅
   - **Problem**: Strategy set 5s margin cooldown silently. No metrics exported.
   - **Solution**: Introduced `TelemetryCollector` module with 30s periodic export.
   - **Impact**: Production observability (orders, margin cooldown, spread, adverse selection).

### Remaining Targets

9. **Single-Level Quoting**: One bid + one ask at BBO. Fully exposed to adverse selection, misses flash wick profits from liquidation cascades.
   - **Target**: Grid Laddering (3-5 levels per side). Level 1 tight/small, Level 2 +5bps/2x, Level 3 +15bps/4x. Captures wick bottoms during cascade events.

## Three-Layer Context Hierarchy

```
CLAUDE.md (root)                         -> Project architecture, constraints, workflows (v4.0.0)
  feeder/CLAUDE.md                       -> Go feeder: WS ingestion, CGO, SHM writers
    feeder/exchanges/CLAUDE.md           -> Exchange adapters (Lighter, Hyper, Backpack, EdgeX, 01)
    feeder/shm/CLAUDE.md                 -> Shared memory layouts (BBO matrix, depth, event ring, account stats)
      feeder/shm/depth.go                -> Depth writer (v4.0.0)
  src/CLAUDE.md                          -> Rust core: HFT engine, FFI, shadow ledger
    src/order_tracker.rs                 -> Per-order state machine (v5.0.0, replaces shadow_ledger)
    src/data_plane.rs                    -> Dedicated data plane thread (v4.0.0)
    src/shm_depth_reader.rs              -> L1-L5 depth reader (v4.0.0)
    src/telemetry.rs                     -> Telemetry module (v4.0.0)
    src/strategy/CLAUDE.md               -> Strategies (arbitrage, MM, adaptive MM, inventory-neutral MM)
    src/exchanges/CLAUDE.md              -> Modular exchange integrations (lighter/, backpack/, edgex/)
      src/exchanges/lighter/CLAUDE.md    -> Lighter DEX client (Poseidon2 + EdDSA via FFI)
        src/exchanges/lighter/error.rs   -> Typed error codes (v4.0.0)
      src/exchanges/backpack/CLAUDE.md   -> Backpack REST client (Ed25519)
      src/exchanges/edgex/CLAUDE.md      -> EdgeX REST client (StarkNet Pedersen)
    src/types/CLAUDE.md                  -> Core types + C-ABI event struct
  examples/CLAUDE.md                     -> Entry points for make targets
  src/native/CLAUDE.md                   -> Native FFI libraries (Lighter signer .so)
  docs/CLAUDE.md                         -> Reference documentation (architecture, optimization)
  proto/CLAUDE.md                        -> gRPC service definitions
```

Claude auto-loads all CLAUDE.md files at session start = zero warm-up time, full project awareness.

## Git Commit Conventions

- Commit messages follow [Conventional Commits](https://www.conventionalcommits.org/): `type(scope): description`
- Types: `feat`, `fix`, `refactor`, `docs`, `test`, `perf`, `chore`
- Scope should be the module name (e.g., `feeder`, `strategy`, `shm`, `lighter`)
- Keep the subject line under 72 characters
- When documentation is updated alongside code, include both changes in the same commit rather than splitting them

---

## Reference Documentation

For detailed information on specific topics, see:

- **Development workflow rules**: `docs/WORKFLOW.md` (Search→Plan→Action, task management, @CLAUDECODE directory structure)
- **CLAUDE.md writing conventions**: `docs/CLAUDE_MD_CONVENTIONS.md` (How to create effective module documentation)
- **Version history**: `docs/CHANGELOG.md` (Major architectural refactors: v3.3.0 unified Makefile, v3.2.0 exchange decoupling)
