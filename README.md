# AlephTX

Institutional-grade High-Frequency Trading framework for crypto perpetual markets. Split architecture: **Go** (network I/O, WebSocket ingestion) + **Rust** (strategy engine, direct HTTP execution), connected via lock-free shared memory IPC.

## Architecture

```
                          Go Feeder                              Rust Core
              ┌─────────────────────────┐          ┌─────────────────────────────┐
              │  Lighter WS (Public)    │          │  ShmReader (Seqlock)        │
              │  Lighter WS (Private)   │──SHM──▶  │  ShmEventReader (SPSC Ring) │
              │  Lighter WS (Account)   │          │  AccountStatsReader         │
              │  Hyperliquid / Backpack │          │          │                  │
              │  EdgeX / 01 / Mock      │          │          ▼                  │
              └─────────────────────────┘          │  ┌───────────────────┐      │
                                                   │  │ Shadow Ledger     │      │
              Shared Memory Regions:               │  │ (real + in_flight)│      │
              /dev/shm/aleph-matrix    (656KB)     │  └───────┬───────────┘      │
              /dev/shm/aleph-events    (64KB)      │          │                  │
              /dev/shm/aleph-account-stats (128B)  │          ▼                  │
                                                   │  Strategy Engine            │
                                                   │  ├─ AdaptiveMM (Lighter)   │
                                                   │  ├─ LighterMM              │
                                                   │  ├─ MarketMaker (EdgeX)    │
                                                   │  ├─ BackpackMM             │
                                                   │  └─ ArbitrageEngine        │
                                                   │          │                  │
                                                   │          ▼                  │
                                                   │  FFI Sign + HTTP Direct    │
                                                   │  (No Boomerang Execution)  │
                                                   └─────────────────────────────┘
```

### Key Design Principles

- **Dual-Track IPC**: Track 1 (BBO state via seqlock matrix) + Track 2 (private events via SPSC ring buffer)
- **No Boomerang Execution**: Rust fires HTTP orders directly to exchanges. Never sends execution commands back to Go.
- **Optimistic Accounting**: Shadow ledger updates `in_flight_pos` before API call; background task reconciles via event stream.
- **Zero Heap Allocations** on hot path (quoting loop < 250ns per tick)

## Quick Start

```bash
# 1. Configure
cp config.example.toml config.toml
# Edit config.toml for your risk parameters

# 2. Set credentials
cp .env.lighter.example .env.lighter
# Fill in API_KEY_PRIVATE_KEY, LIGHTER_ACCOUNT_INDEX, LIGHTER_API_KEY_INDEX

# 3. Build & Run (always use Makefile)
make build              # Build Go feeder + Rust core
make adaptive-up        # Start feeder + adaptive market maker
make adaptive-logs      # Monitor logs
make adaptive-down      # Stop and clean up
```

## Make Targets

| Target | Description |
|--------|-------------|
| `make build` | Build all binaries (Go feeder + Rust) |
| `make test-up` / `test-down` / `test-logs` | Integration test environment |
| `make adaptive-up` / `adaptive-down` / `adaptive-logs` | Production adaptive MM |
| `make status` | Show all running strategies |
| `make clean` | Clean build artifacts |

## Project Structure

```
aleph-tx/
├── feeder/              # Go: WebSocket ingestion, CGO FFI exports
│   ├── exchanges/       #   Exchange adapters (Lighter, Hyper, Backpack, EdgeX, 01)
│   ├── shm/             #   Shared memory writers (BBO matrix, event ring, account stats)
│   ├── config/          #   TOML config loader
│   ├── cmd/             #   Standalone CLI test tools
│   └── test/            #   Integration tests (auth, stream, order)
├── src/                 # Rust: HFT strategy engine
│   ├── strategy/        #   Strategy implementations (adaptive_mm, lighter_mm, arbitrage, etc.)
│   ├── backpack_api/    #   Backpack REST client (Ed25519)
│   ├── edgex_api/       #   EdgeX REST client (StarkNet Pedersen)
│   ├── types/           #   Core types + C-ABI event struct (64 bytes)
│   └── bin/             #   Diagnostic tools (monitors, SHM inspection)
├── examples/            # Entry point binaries for make targets
├── lib/                 # Pre-built FFI shared library (Lighter signer)
├── scripts/             # Operational shell scripts
├── docs/                # Reference documentation
└── proto/               # gRPC service definitions
```

## Supported Exchanges

| Exchange | Role | Auth | Status |
|----------|------|------|--------|
| **Lighter DEX** | Primary (HFT MM) | Poseidon2 + EdDSA via FFI | Production |
| **Backpack** | Secondary (MM) | Ed25519 | Ready |
| **EdgeX** | Secondary (MM) | StarkNet Pedersen | Ready |
| **Hyperliquid** | Data feed | - | Feed only |
| **01 Exchange** | Data feed | - | Feed only |

## Strategy: Adaptive Market Maker (Primary)

The production strategy (`src/strategy/adaptive_mm.rs`) implements fee-aware HFT with microstructure signals:

- **Fee-Aware Spread**: Ensures spread > round-trip fee (0.76 bps for Premium account)
- **Microstructure Tracker**: EWMA fast/slow momentum, realized volatility, adverse selection score
- **Inventory Skew**: Linear position-based adjustment to flatten exposure
- **Batch Quoting**: Paired bid/ask via `sendTxBatch` for atomic updates
- **Dynamic Sizing**: Position scaled by leverage and available balance from account stats

## Configuration

```toml
# config.toml (copy from config.example.toml)
[backpack]
risk_fraction = 0.20          # Fraction of equity at risk
min_spread_bps = 6.0          # Minimum half-spread (bps)
vol_multiplier = 2.5          # spread = max(min_spread, vol * multiplier)
requote_interval_ms = 3000    # Re-quote interval

[edgex]
risk_fraction = 0.10
min_spread_bps = 8.0          # Higher fees -> wider spread
requote_interval_ms = 5000    # Rate limit: 2 req/2s
```

## Credentials

```bash
# .env.lighter
API_KEY_PRIVATE_KEY=<hex>
LIGHTER_ACCOUNT_INDEX=<int>
LIGHTER_API_KEY_INDEX=<int>

# .env.backpack
BACKPACK_PUBLIC_KEY=<key>
BACKPACK_SECRET_KEY=<key>

# .env.edgex
EDGEX_STARK_PRIVATE_KEY=<hex>
EDGEX_ACCOUNT_ID=<id>
```

## Documentation

| Document | Description |
|----------|-------------|
| `CLAUDE.md` (root + per-directory) | Auto-loaded technical context for Claude Code |
| `docs/QUICKSTART.md` | Step-by-step deployment guide |
| `docs/ADAPTIVE_MM_GUIDE.md` | Adaptive MM operational guide |
| `docs/OPTIMIZATION_GUIDE.md` | Strategy math models and parameter tuning |
| `docs/DUAL_TRACK_IPC.md` | IPC architecture deep-dive |
| `docs/ORDER_EXECUTION_REDESIGN.md` | Order execution architecture decisions |
| `CHANGELOG.md` | Version history |

## Disclaimer

This software is for educational and research purposes. Trading cryptocurrencies involves substantial risk of loss. Use at your own risk.
