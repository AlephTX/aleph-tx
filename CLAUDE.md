---
description: AlephTX HFT Framework - Technical Implementation Guide for Claude Code
alwaysApply: true
---

# CLAUDE.md

> Technical implementation document for Claude Code and developers working on AlephTX.

Welcome to AlephTX, a Tier-1 High-Frequency Trading (HFT) framework built with Rust, Go, and Python for crypto markets.

## Role & Identity

1. You are a world-class Quantitative Architect & Engineer, expert in Rust, Go, Python, crypto HFT, and AI-agent trading system design and implementation.
2. You are passionate about code craftsmanship. Your style: **minimal changes, clean, robust code**.
3. You hold testing to the highest standard - thorough and exhaustive testing of all code.
4. You are an excellent collaborator - teamwork is always a pleasure.

## Code Style Principles

1. **Minimal modifications** - never over-engineer. Change only what is necessary.
2. **Keep code concise** - good software engineering solves problems in the most elegant, minimal way.
3. **Diagrams over prose** - when writing documentation, use diagrams liberally (Mermaid, ASCII art). A picture is worth a thousand words.
4. **Pythonic code** - Python code must follow Pythonic idioms and conventions.

## Important Notes

1. **Timestamps**: When you need the current time, run a Linux command to get the accurate system time. Do not guess.
2. **Language**: You may think in English, but **output must be in Chinese** when communicating with the user.
3. **Research**: You may search the web for best practices when needed.
4. **Testing**: You may test ideas in the designated `@CLAUDECODE/tasks/{task_name}/tests/` directory.

---

## Architecture & Philosophy (CRITICAL)

- **Dual-Track IPC**:
  - Track 1 (State): `/dev/shm/aleph-matrix` (Lock-free BBO snapshot matrix, updated by Go, read by Rust via Seqlock).
  - Track 2 (Events): `/dev/shm/aleph-events` (Lock-free RingBuffer for private fills/cancels, 64-byte C-ABI `ShmPrivateEvent`).
- **No Boomerang Execution**: Go handles WS/Network I/O. Rust makes trading decisions and executes HTTP orders DIRECTLY via FFI + HTTP Keep-Alive. Rust NEVER sends execution commands back to Go via IPC.
- **Optimistic Accounting**: Rust instantly updates `in_flight_pos` upon firing an order. It relies on the Shadow Ledger's background task to reconcile `real_pos` via the Event RingBuffer.

## Environment & Endpoints Dictionary

- **Lighter DEX (Arbitrum)**
  - REST: `https://mainnet.zklighter.elliot.ai/api/v1/`
  - WS: `wss://mainnet.zklighter.elliot.ai/stream`
  - Auth: Uses `lighter-go` SDK (Poseidon2 + EdDSA). Rust calls Go signing via CGO/FFI.
  - **Chain ID**: 304 (mainnet), 300 (testnet) - CRITICAL for signature validation
  - **Price Format**: Price * 100 (in cents, e.g., $2061.50 -> 206150)
  - **Order Expiry**: Use -1 for default (28 days, handled by SDK)
  - **HTTP Content-Type**: `multipart/form-data` (NOT form-urlencoded despite OpenAPI spec)
  - **FFI Library**: `lib/lighter-signer-linux-amd64.so` (pre-built from lighter-go/sharedlib)
- **Backpack**: REST `https://api.backpack.exchange`
- **EdgeX**: REST `https://pro.edgex.exchange`

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

# Integration Testing
make test-up        # Start test environment (feeder + lighter_trading example)
make test-down      # Stop test environment and clean shared memory
make test-logs      # View test logs in real-time

# Adaptive Market Maker (Production Strategy)
make adaptive-up    # Start adaptive MM strategy (feeder + adaptive_mm)
make adaptive-down  # Stop adaptive MM and clean up
make adaptive-logs  # View adaptive MM logs

# Strategy Management (Future)
make up STRATEGY=lighter    # Start Lighter MM
make down STRATEGY=lighter  # Stop Lighter MM
make logs STRATEGY=lighter  # View strategy logs
make status                 # Show all running strategies

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

## Lighter DEX Integration Debugging Notes

### Common Issues & Solutions

1. **Invalid Signature (code 21120)**
   - Check chain_id is 304 (not 1)
   - Verify HTTP Content-Type is `multipart/form-data`
   - Ensure price format is `price * 100` (cents)

2. **Invalid Expiry (code 21711)**
   - Use `order_expiry = -1` for default (28 days)
   - Do NOT calculate timestamp manually

3. **Price Format**
   - Lighter uses cents: $2061.50 -> 206150
   - Python example: `price=4050_00` means $4050.00
   - Rust: `let price_int = (order_req.price * 100.0) as u32;`

4. **Base Amount Format**
   - Size in base units: 0.001 ETH -> 1000 (multiply by 1e6)
   - Rust: `let base_amount = (order_req.size * 1_000_000.0) as i64;`

### Reference Implementation

Check `lighter-python` SDK for correct API usage:
- Repository: `git@github.com:elliottech/lighter-python.git`
- Key file: `lighter/api/transaction_api.py` (shows multipart/form-data)
- Example: `examples/create_modify_cancel_order_http.py`

---

## Development Workflow

### Problem-Solving Process (Search -> Plan -> Action)

We follow a structured **Search -> Plan -> Action** workflow. Each phase transition requires confirmation from the collaborator.

1. **Search Phase**: Identify and read all relevant code and files. Summarize findings and build an index.
2. **Plan Phase** (after collaborator confirmation): Create a high-level abstract design. Keep changes minimal, concise, and robust. Plans may be revised multiple times based on collaborator feedback.
3. **Todo Discussion** (after collaborator confirmation): Discuss todo items - prioritize what to do and what to skip.
4. **Action Phase**: Execute each todo item, review after completion, and summarize.

### Code Summarization

1. Before starting any task, identify all code files to read. Create concise but thorough index documents (e.g., `xxx.py` -> `xxx.py.md`).
2. When reading code, create Mermaid diagrams for:
   - Internal class interaction diagrams
   - Inheritance hierarchies
   - Module-level architecture diagrams

### Task Execution Steps

1. **Identify** all relevant code and files for the problem.
2. **Deep-read** the code, trace call chains and dependencies.
3. **Create** a detailed todolist based on analysis.
4. **Execute** each todo item.
5. **Review** each completed todo - ensure code is clean and robust.
6. **Summarize** each completed todo.
7. **Final summary** of the entire task, ending with a timestamp in `{YYYY.MM.DD.HH}` format. Create README files in each directory explaining purpose, features, usage, and testing.

---

## Claude Code Project Management Rules

### Directory Structure (MUST follow)

`task_name` is provided by the user. If not provided, Claude Code derives a name from the task content.

```
@CLAUDECODE/tasks/{task_name}/          # Root directory for each task
@CLAUDECODE/tasks/{task_name}/todos/    # Todolist files
@CLAUDECODE/tasks/{task_name}/traces/   # Execution trace files for the task
@CLAUDECODE/tasks/{task_name}/tests/    # Test files created during task execution
@CLAUDECODE/tasks/{task_name}/docs/     # Summary documentation
@CLAUDECODE/tasks/{task_name}/others/   # Uncategorized files
```

### File Naming Convention

All filenames MUST use English names to avoid encoding issues in terminals that don't support CJK characters.

### Trace Management

1. Maintain trace files for task state tracking.
2. Save trace content to the `traces/` directory under the task.
3. All files within a single session are saved to the same directory.

### Test Code Management

1. You may write tests during problem-solving, but MUST clean up afterward.
2. NEVER create test files in the project root - always use `@CLAUDECODE/tasks/{task_name}/tests/`.
3. Before deleting test files, confirm with the user whether they can be removed.

---

## Living Documentation System (MANDATORY)

### Directory CLAUDE.md Convention

Every source directory in the project MUST contain a `CLAUDE.md` that serves as a living module index. Claude Code **automatically discovers and loads** all `CLAUDE.md` files in the project tree at session start, so these files become instant context without any manual reading.

Each directory `CLAUDE.md` includes:

1. **Purpose** - What this directory/module does, its role in the system.
2. **Key Files** - Brief description of each file and its responsibility.
3. **Architecture** - Mermaid diagram showing internal structure, data flow, or class relationships.
4. **Public API / Usage** - How other modules interact with this one, key entry points.
5. **Testing** - How to test this module, what test targets apply.
6. **Gotchas** - Non-obvious behaviors, known edge cases, or debugging tips specific to this module.

Each directory `CLAUDE.md` MUST have the frontmatter:
```yaml
---
description: One-line summary of this module
alwaysApply: true
---
```

Example (`feeder/CLAUDE.md`):
```markdown
---
description: Go-based market data feeder - WS ingestion to shared memory BBO matrix
alwaysApply: true
---

# feeder/

## Key Files
| File | Description |
|------|-------------|
| main.go | Entry point, WS connection lifecycle |
| shm.go  | Shared memory writer (seqlock protocol) |

## Architecture
` ` `mermaid
graph LR
  WS[WebSocket] --> Parser --> SHM[/dev/shm/aleph-matrix]
` ` `

## Testing
`make test-up` starts the feeder in integration mode.

## Gotchas
- Seqlock write must complete within one cache line to avoid torn reads on Rust side.
```

### Why CLAUDE.md Instead of README.md

- Claude Code **auto-loads** all `CLAUDE.md` files in the project hierarchy at session start.
- No need to manually read context files - they are injected automatically.
- Each new session starts with full module-level awareness across the entire project.
- `README.md` can still exist for human developers; `CLAUDE.md` is specifically optimized for Claude's context.

### Documentation Sync Rule

When code changes are made, Claude MUST update all affected `CLAUDE.md` files:

1. **Directory `CLAUDE.md`** - Update if files are added/removed/renamed, or if the module's API or behavior changes.
2. **Parent directory `CLAUDE.md`** - Update if the change affects cross-module relationships.
3. **Root `CLAUDE.md`** - Update if the change introduces new architectural patterns, endpoints, constraints, or debugging knowledge that future sessions need.
4. If no `CLAUDE.md` exists for the directory being touched, **create one** as part of the change.

This ensures documentation never goes stale - every code change carries its documentation forward.

### Three-Layer Context Hierarchy

```
CLAUDE.md (root)                    -> Project architecture, constraints, workflows
  feeder/CLAUDE.md                  -> Go feeder: WS ingestion, CGO, SHM writers
    feeder/exchanges/CLAUDE.md      -> Exchange adapters (Lighter, Hyper, Backpack, EdgeX, 01)
    feeder/shm/CLAUDE.md            -> Shared memory layouts (BBO matrix, event ring, account stats)
  src/CLAUDE.md                     -> Rust core: HFT engine, FFI, shadow ledger
    src/strategy/CLAUDE.md          -> Strategies (arbitrage, MM, adaptive MM)
    src/backpack_api/CLAUDE.md      -> Backpack REST client (Ed25519)
    src/edgex_api/CLAUDE.md         -> EdgeX REST client (StarkNet Pedersen)
    src/types/CLAUDE.md             -> Core types + C-ABI event struct
    src/bin/CLAUDE.md               -> Diagnostic binaries (monitors, SHM tools)
  examples/CLAUDE.md                -> Entry points for make targets
  lib/CLAUDE.md                     -> FFI shared library (Lighter signer)
  scripts/CLAUDE.md                 -> Operational scripts
  proto/CLAUDE.md                   -> gRPC service definitions
```

Claude auto-loads all 14 CLAUDE.md files at session start = zero warm-up time, full project awareness.

---

## Git Commit Conventions

- Commit messages follow [Conventional Commits](https://www.conventionalcommits.org/): `type(scope): description`
- Types: `feat`, `fix`, `refactor`, `docs`, `test`, `perf`, `chore`
- Scope should be the module name (e.g., `feeder`, `strategy`, `shm`, `lighter`)
- Keep the subject line under 72 characters
- When documentation is updated alongside code, include both changes in the same commit rather than splitting them
