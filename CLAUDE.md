# AlephTX Autonomous Agent Runbook & Global Constraints

Welcome to AlephTX, a Tier-1 High-Frequency Trading (HFT) framework. You are an autonomous Quantitative Infrastructure Engineer. You have permission to write code, compile, run tests, spin up integration environments, read logs, and clean up.

## 🏗️ 1. Architecture & Philosophy (CRITICAL)
- **Dual-Track IPC**: 
  - Track 1 (State): `/dev/shm/aleph-matrix` (Lock-free BBO snapshot matrix, updated by Go, read by Rust via Seqlock).
  - Track 2 (Events): `/dev/shm/aleph-events` (Lock-free RingBuffer for private fills/cancels, 64-byte C-ABI `ShmPrivateEvent`).
- **No Boomerang Execution**: Go handles WS/Network I/O. Rust makes trading decisions and executes HTTP orders DIRECTLY via FFI + HTTP Keep-Alive. Rust NEVER sends execution commands back to Go via IPC.
- **Optimistic Accounting**: Rust instantly updates `in_flight_pos` upon firing an order. It relies on the Shadow Ledger's background task to reconcile `real_pos` via the Event RingBuffer.

## 🌐 2. Environment & Endpoints Dictionary
- **Lighter DEX (Arbitrum)**
  - REST: `https://mainnet.zklighter.elliot.ai/api/v1/`
  - WS: `wss://mainnet.zklighter.elliot.ai/stream`
  - Auth: Uses `lighter-go` SDK (Poseidon2 + EdDSA). Rust calls Go signing via CGO/FFI.
  - **Chain ID**: 304 (mainnet), 300 (testnet) - CRITICAL for signature validation
  - **Price Format**: Price * 100 (in cents, e.g., $2061.50 → 206150)
  - **Order Expiry**: Use -1 for default (28 days, handled by SDK)
  - **HTTP Content-Type**: `multipart/form-data` (NOT form-urlencoded despite OpenAPI spec)
  - **FFI Library**: `lib/lighter-signer-linux-amd64.so` (pre-built from lighter-go/sharedlib)
- **Backpack**: REST `https://api.backpack.exchange`
- **EdgeX**: REST `https://pro.edgex.exchange`

## 🚀 3. Build & Test Workflow (MANDATORY)

### CRITICAL RULE: Always Use Makefile
**ALL build, test, and run operations MUST go through the Makefile.**
- ❌ NEVER run `cargo build`, `cargo run`, `go build` directly
- ❌ NEVER create custom shell scripts for building/running
- ✅ ALWAYS use `make <target>` commands

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

## ⚠️ 4. Global Hard Constraints
- **C-ABI Alignment**: `ShmPrivateEvent` MUST be EXACTLY 64 bytes. Verify with `static_assertions::assert_eq_size!`.
- **Incremental Quoting Math**: Protect against divide-by-zero (e.g., if `last_price == 0.0` during incremental quoting calculations, return `true` to force initial quote).

## 🔧 5. Lighter DEX Integration Debugging Notes
### Common Issues & Solutions
1. **Invalid Signature (code 21120)**
   - Check chain_id is 304 (not 1)
   - Verify HTTP Content-Type is `multipart/form-data`
   - Ensure price format is `price * 100` (cents)

2. **Invalid Expiry (code 21711)**
   - Use `order_expiry = -1` for default (28 days)
   - Do NOT calculate timestamp manually

3. **Price Format**
   - Lighter uses cents: $2061.50 → 206150
   - Python example: `price=4050_00` means $4050.00
   - Rust: `let price_int = (order_req.price * 100.0) as u32;`

4. **Base Amount Format**
   - Size in base units: 0.001 ETH → 1000 (multiply by 1e6)
   - Rust: `let base_amount = (order_req.size * 1_000_000.0) as i64;`

### Reference Implementation
Check `lighter-python` SDK for correct API usage:
- Repository: `git@github.com:elliottech/lighter-python.git`
- Key file: `lighter/api/transaction_api.py` (shows multipart/form-data)
- Example: `examples/create_modify_cancel_order_http.py`