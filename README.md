# AlephTX - AI Quantitative Trading System

> High-performance, extensible quantitative trading system built with Rust

## Core Principles
- 🚀 **Speed First** - Rust for maximum performance
- 🔒 **Security** - Never expose sensitive information
- 🏗️ **Extensibility** - Support multiple exchanges & strategies
- 📐 **Jeff Dean Quality** - Strict typing, zero-cost abstractions

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                      AlephTX Core                           │
├─────────────────────────────────────────────────────────────┤
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────────┐  │
│  │   Feeds    │  │  Strategies  │  │  Risk Manager   │  │
│  │  (Market)  │  │   (Logic)    │  │   (Protection)  │  │
│  └──────┬──────┘  └──────┬──────────────┬────────┘  │
┘  └│         │                 │                   │            │
│  ┌──────┴─────────────────┴───────────────────┴────────┐ │
│  │                    Execution Layer                     │ │
│  │  ┌─────────┐ ┌─────────┐ ┌─────────┐ ┌───────────┐  │ │
│  │  │Binance │ │   OKX   │ │ EdgeX   │ │  Others   │  │ │
│  │  └─────────┘ └─────────┘ └─────────┘ └───────────┘  │ │
│  └───────────────────────────────────────────────────────┘ │
│                            │                                │
│  ┌─────────────────────────┴─────────────────────────────┐ │
│  │              Telegram Controller (@AlephTXBot)         │ │
│  └───────────────────────────────────────────────────────┘ │
└─────────────────────────────────────────────────────────────┘
```

## Supported Exchanges (Planned)

### Spot
- [ ] Binance
- [ ] OKX

### Perpetual DEX
- [ ] EdgeX
- [ ] GMX
- [ ] dYdX
- [ ] 01.xyz
- [ ] Vertex
- [ ] Hyperliquid

## Features

- [ ] WebSocket real-time market data
- [ ] REST API fallback
- [ ] Cross-exchange arbitrage
- [ ] Grid trading strategy
- [ ] Trend following strategy
- [ ] Paper trading mode
- [ ] Telegram bot control

## Code Standards

```rust
// Zero-cost abstractions, strong typing
pub trait Exchange: Send + Sync {
    async fn fetch_ticker(&self, symbol: &str) -> Result<Ticker, Error>;
    async fn place_order(&self, order: Order) -> Result<Order, Error>;
    // ... trait methods
}
```

## Tech Stack

- **Language**: Rust (2021 edition)
- **Runtime**: Tokio (async)
- **HTTP**: Reqwest
- **WebSocket**: Tungstenite
- **Serialization**: Serde + Protobuf
- **Config**: TOML + Schema validation

## Getting Started

```bash
# Build
cargo build --release

# Run (paper trading)
cargo run -- --mode paper

# Run with Telegram
cargo run -- --telegram-bot-token YOUR_TOKEN
```

## Modules

| Module | Description |
|--------|-------------|
| `core` | Common types, traits, errors |
| `feeds` | Market data ingestion (WS + REST) |
| `exchanges` | Exchange implementations |
| `strategies` | Trading strategies |
| `execution` | Order management |
| `risk` | Risk controls |
| `telegram` | Bot controller |

## Development

```bash
# Format
cargo fmt

# Lint
cargo clippy

# Test
cargo test --all

# Benchmarks
cargo bench
```

## Roadmap

1. [x] Project setup (Rust)
2. [ ] Core traits & types
3. [ ] WebSocket feed implementation
4. [ ] Exchange trait + Binance impl
5. [ ] Basic strategy framework
6. [ ] Paper trading mode
7. [ ] Telegram integration
8. [ ] Add more exchanges
9. [ ] Arbitrage strategies
