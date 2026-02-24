# AlephTX

**Institutional-grade trading infrastructure** — built for speed, reliability, and extensibility.

AlephTX is a Go-Rust hybrid trading engine designed to power a wide range of trading strategies and execution contexts:

- **Agent Trading** — LLM/AI agent-driven order execution with structured signal interfaces
- **Quantitative Trading** — systematic strategy execution with low-latency market data
- **Perp DEX** — on-chain perpetuals trading (dYdX, GMX, Hyperliquid, etc.)
- **CEX Arbitrage** — cross-exchange spread capture with unified adapter layer
- **Prediction Markets** — Polymarket and similar platforms via dedicated adapters

---

## Architecture

```
┌─────────────────────────────────────────────────────────┐
│                  AlephTX System                         │
│                                                         │
│  ┌──────────────────────────────────────────────────┐   │
│  │  Go Data Layer — "The Tentacles"                 │   │
│  │  • WebSocket connections to 20+ exchanges        │   │
│  │  • Ping/Pong, reconnect, error handling          │   │
│  │  • Normalise all formats → AlephTX standard      │   │
│  │  • Push via Unix Socket (IPC) to Rust            │   │
│  └────────────────────┬─────────────────────────────┘   │
│                       │ Unix Socket (JSON-lines)         │
│  ┌────────────────────▼─────────────────────────────┐   │
│  │  Rust Strategy Engine — "The Brain"              │   │
│  │  • In-memory local orderbook                     │   │
│  │  • Signal processing (from AI agents or quant)   │   │
│  │  • Risk Gate — hard limits, position sizing      │   │
│  │  • Generates signed raw transaction bytes        │   │
│  └────────────────────┬─────────────────────────────┘   │
│                       │                                  │
│  ┌────────────────────▼─────────────────────────────┐   │
│  │  Execution Layer — "The Hands"                   │   │
│  │  • CEX: REST/WS order placement (Go or Rust)     │   │
│  │  • DEX: signed tx broadcast (Rust → RPC node)    │   │
│  │  • Prediction markets: Polymarket API adapter    │   │
│  └──────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────┘
```

## Why Go + Rust?

| Layer | Language | Reason |
|-------|----------|--------|
| Network / Exchange adapters | Go | Fast iteration, easy JSON, great concurrency |
| Strategy / Risk / Signing | Rust | Zero GC pauses, memory safety, raw speed |
| On-chain execution | Rust | Direct tx signing, no runtime overhead |

Go's GC only affects the data ingestion path. The critical strategy and risk calculation path runs entirely in Rust with deterministic latency.

---

## Project Structure

```
aleph-tx/
├── feeder/              # Go data ingestion layer
│   ├── binance/         # Binance WebSocket adapter
│   ├── ipc/             # Unix socket client (→ Rust core)
│   └── main.go
├── src/                 # Rust strategy engine
│   ├── adapter.rs       # Exchange adapter trait + Binance REST
│   ├── engine.rs        # StateMachine (in-memory state)
│   ├── ipc.rs           # Unix socket server (← Go feeder)
│   ├── signer.rs        # HMAC / tx signing
│   ├── types.rs         # Canonical types (Order, Ticker, Position…)
│   └── main.rs
└── proto/
    └── aleph.proto      # gRPC service definitions
```

## Running

```bash
# Terminal 1 — Rust core (server, creates Unix socket)
cargo run

# Terminal 2 — Go feeder (client, connects to socket)
cd feeder && go run .
```

Environment variables:
- `ALEPH_SOCKET` — Unix socket path (default: `/tmp/aleph-feeder.sock`)

---

## Roadmap

- [ ] Local orderbook (L2 depth maintenance in Rust)
- [ ] Risk Gate (position limits, drawdown circuit breaker)
- [ ] OKX / Bybit / Hyperliquid adapters (Go feeder)
- [ ] DEX adapter (on-chain tx signing + broadcast)
- [ ] Polymarket adapter
- [ ] Python agent signal interface (gRPC)
- [ ] Strategy backtesting harness

---

## Status

Early development. Core IPC pipeline (Go → Rust) operational.
