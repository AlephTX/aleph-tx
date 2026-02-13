# AlephTX 🦀 - Institutional Quantitative Trading System

> High-performance, multi-strategy, cross-market quantitative trading platform

## Architecture Overview

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                           AlephTX "Kraken" Architecture                      │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                             │
│  ┌─────────────────────────────────────────────────────────────────────┐  │
│  │                    L2: Agent Layer (Python/AI)                       │  │
│  │         AlephTX Agent │ Decision Making │ Strategy Research          │  │
│  │                    ↓ gRPC/Redis Pub/Sub                             │  │
│  └─────────────────────────────────────────────────────────────────────┘  │
│                                    ↓                                        │
│  ┌─────────────────────────────────────────────────────────────────────┐  │
│  │               L1: Strategy Bus (Rust + NATS)                       │  │
│  │     Arbitrage │ Grid │ Trend │ Mean Reversion │ Multi-Signal       │  │
│  └─────────────────────────────────────────────────────────────────────┘  │
│                                    ↓                                        │
│  ┌─────────────────────────────────────────────────────────────────────┐  │
│  │              L0: Core Engine (Rust - Microsecond)                   │  │
│  │  ┌────────────────┐  ┌────────────────┐  ┌────────────────────┐   │  │
│  │  │  Order Manager │  │  Risk Engine   │  │  Global State      │   │  │
│  │  └───────┬────────┘  └───────┬────────┘  └─────────┬──────────┘   │  │
│  │          │                    │                     │               │  │
│  └──────────┼────────────────────┼─────────────────────┼──────────────┘  │
│             ↓                    ↓                     ↓                   │
│  ┌─────────────────────────────────────────────────────────────────────┐  │
│  │              Universal Adapter Layer (Plugin System)                 │  │
│  │  ┌──────────┐ ┌──────────┐ ┌──────────┐ ┌──────────┐ ┌─────────┐  │  │
│  │  │Binance  │ │   OKX    │ │  EdgeX   │ │Hyperliquid│ │ 01.xyz │  │  │
│  │  └──────────┘ └──────────┘ └──────────┘ └──────────┘ └─────────┘  │  │
│  └─────────────────────────────────────────────────────────────────────┘  │
│                                                                             │
└─────────────────────────────────────────────────────────────────────────────┘
```

## Core Design Principles

### 1. Modular Monolith + Plugin System
- **Everything is a plugin**: Exchanges, Strategies, Signers, Risk Modules
- Add new exchange in minutes, not months
- Hot-swap strategies without restart

### 2. Layered Execution
| Layer | Latency | Components | Deployment |
|-------|---------|------------|------------|
| L0 | Nanosecond | MEV, Sandwich, Market Making | Co-located |
| L1 | Microsecond | Arbitrage, HFT | Cloud (Tokyo/Singapore) |
| L2 | Second | Agent Decision Making | Local (4070 Ti) |

### 3. Universal Adapter Pattern

```rust
/// Universal Exchange Adapter Trait
/// Every exchange (CEX/DEX) implements this same interface
#[async_trait]
pub trait ExchangeAdapter: Send + Sync {
    /// Subscribe to orderbook updates (WS or Chain Event)
    async fn subscribe_orderbook(&self, symbols: &[Symbol]) -> Result<()>;
    
    /// Place order (REST API or Smart Contract)
    async fn place_order(&self, order: OrderRequest) -> Result<OrderId>;
    
    /// Get current positions
    async fn get_positions(&self) -> Result<Vec<Position>>;
    
    /// Sign payload (HMAC for CEX, Private Key for DEX)
    fn signer(&self) -> &dyn Signer;
}
```

## Supported Exchanges

### CEX (Centralized Exchanges)
- [ ] Binance (Spot + Futures)
- [ ] OKX (Spot + Futures)
- [ ] Bybit (Perpetual)

### Perp DEX (Decentralized Exchanges)
- [ ] EdgeX
- [ ] Hyperliquid
- [ ] 01.xyz
- [ ] dYdX
- [ ] GMX
- [ ] Vertex

## Technical Stack

| Component | Technology | Reason |
|-----------|------------|--------|
| Core Engine | Rust 2024 | Zero-cost abstractions, type safety |
| Agent/Strategy | Python 3.12 + Polars | AI/ML ecosystem, DataFrame performance |
| Communication | gRPC + NATS | Type-safe, high-throughput messaging |
| Blockchain | Alloy + Reth | fastest EVM interaction |
| Simulation | Revm | In-memory EVM for MEV |
| Database | TimescaleDB | Time-series for tick data |
| Monitoring | Prometheus + Grafana | Observability |

## Project Structure

```
aleph-tx/
├── aleph-core/              # Core engine (Rust)
│   ├── src/
│   │   ├── adapter/        # Universal exchange adapters
│   │   │   ├── traits.rs   # ExchangeAdapter trait
│   │   │   ├── binance.rs
│   │   │   ├── okx.rs
│   │   │   ├── hyperliquid.rs
│   │   │   └── edgex.rs
│   │   ├── engine/         # Core trading engine
│   │   │   ├── state.rs    # Global world state
│   │   │   ├── order.rs    # Order management
│   │   │   └── risk.rs     # Risk engine
│   │   ├── signer/         # Multi-sig support
│   │   │   ├── hmac.rs     # CEX signing
│   │   │   ├── evm.rs      # EVM signing (k256)
│   │   │   └── starknet.rs # StarkNet signing
│   │   ├── messaging/      # NATS/gRPC
│   │   └── lib.rs
│   └── Cargo.toml
│
├── aleph-agent/            # AI Agent (Python)
│   ├── src/
│   │   ├── agent.py        # Main agent logic
│   │   ├── strategies/     # Strategy implementations
│   │   ├── signals/        # Signal generation
│   │   └── learning/       # ML models
│   ├── proto/              # gRPC definitions
│   └── pyproject.toml
│
├── aleph-mev/              # MEV/Sandwich (Rust)
│   ├── src/
│   │   ├── mempool.rs      # Mempool listener
│   │   ├── sandwich.rs     # Sandwich attack
│   │   └── executor.rs     # Bundle execution
│   └── Cargo.toml
│
├── configs/                # Configuration
│   ├── docker-compose.yml
│   ├── prometheus.yml
│   └── grafana/
│
└── docs/
    ├── architecture.md
    ├── exchange-adapter.md
    └── roadmap.md
```

## Communication Protocol (gRPC)

```protobuf
// proto/aleph.proto
syntax = "proto3";

package aleph;

service AlephCore {
    // Market Data
    rpc SubscribeOrderbook(OrderbookRequest) returns (stream OrderbookUpdate);
    rpc SubscribeTicker(TickerRequest) returns (stream Ticker);
    
    // Trading
    rpc PlaceOrder(OrderRequest) returns (OrderResponse);
    rpc CancelOrder(CancelRequest) returns (OrderResponse);
    rpc GetPositions(PositionsRequest) returns (PositionsResponse);
    rpc GetBalance(BalanceRequest) returns (BalanceResponse);
    
    // State
    rpc GetGlobalState(StateRequest) returns (GlobalState);
}

message OrderRequest {
    string symbol = 1;
    Side side = 2;
    OrderType order_type = 3;
    string quantity = 4;
    string price = 5;
}
```

## Getting Started

### Prerequisites
- Rust 1.83+
- Python 3.12+
- Docker & Docker Compose

### Build

```bash
# Clone
git clone https://github.com/AlephTX/aleph-tx.git
cd aleph-tx

# Build core (Rust)
cargo build --release -p aleph-core

# Setup Python environment
cd aleph-agent
poetry install

# Run with Docker
docker-compose up -d
```

### Development

```bash
# Format code
cargo fmt
black .

# Lint
cargo clippy
ruff check .

# Test
cargo test
pytest
```

## Roadmap

### Phase 1: Foundation (MVP)
- [x] Project architecture
- [ ] Universal Adapter trait
- [ ] Binance adapter (Spot)
- [ ] Basic order management
- [ ] Paper trading mode
- [ ] Telegram bot

### Phase 2: Multi-Exchange
- [ ] OKX adapter
- [ ] EdgeX adapter
- [ ] Hyperliquid adapter
- [ ] Cross-exchange arbitrage

### Phase 3: Agent Integration
- [ ] gRPC protocol
- [ ] Agent strategy layer
- [ ] Historical data pipeline
- [ ] Backtesting framework

### Phase 4: MEV/On-Chain
- [ ] Reth/Alloy integration
- [ ] Mempool listener
- [ ] Sandwich bot
- [ ] Private mempool (Flashbots)

### Phase 5: Production
- [ ] Co-location setup
- [ ] Risk management hardening
- [ ] Full test coverage
- [ ] Monitoring & alerts

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for development guidelines.

## License

MIT
