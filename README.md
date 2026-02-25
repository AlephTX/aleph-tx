# AlephTX: The Ultimate Quantitative Trading & Arbitrage Framework

AlephTX is an institutional-grade, zero-latency high-frequency trading (HFT) and cross-chain arbitrage framework. Designed with a split architecture (Rust Core & Go Feeder), it bridges the gap between massive concurrent I/O scaling and microsecond-level order execution.

> **"The speed of light is our only real limit."**

## 🏗️ System Architecture

The core of AlephTX is designed around a **Lock-Free Zero-Copy Shared Memory Matrix** (`/dev/shm/aleph-matrix`).

### 1. The Feeder (Go)
Located in `/feeder`, this component is a highly-concurrent multiplexer. It connects to dozens of different exchanges (Hyperliquid, EdgeX, Lighter, Backpack, 01, etc.) via WebSockets. It normalizes all orderbook data and writes it directly to the OS shared memory.
*   **Dynamic Configuration**: Exchanges and symbols are dynamically loaded from `config.toml`.
*   **Fault Tolerant**: Handles WebSocket disconnects, rate limits, and reconnection jitter natively.

### 2. The Core Engine (Rust)
Located in `src/`, the true heart of AlephTX. Bypassing standard JSON-RPC overheads, it reads directly from the shared memory matrix using seqlocks.
*   **Sub-microsecond Latency**: The polling loop takes less than 250 nanoseconds to detect a global market shift.
*   **Strategy Engine**: Multiplexes shared memory events through a `Strategy` trait interface, allowing dozens of concurrent trading algorithms to react simultaneously.

## � Supported Trading Modalities

### Cross-Exchange Arbitrage (`strategy/arbitrage.rs`)
The system simultaneously scans all interconnected exchanges. When a mispricing (spread) between Exchange A (e.g., Hyperliquid) and Exchange B (e.g., EdgeX) exceeds the configured trigger, it fires a parallel execution signal to both networks.

### Single-Exchange Quantitative Strategies (`strategy/market_maker.rs`)
AlephTX isn't just for arbitrage. Using the powerful Rust `Strategy` multiplexer, quantitative developers can implement local strategies:
*   High-Frequency Market Making (Grid Trading)
*   Statistical Arbitrage (Mean Reversion)
*   Momentum Ignition & Trend Following

---

## 🚀 Grand Vision & Roadmap

Our long-term masterplan is to establish AlephTX as the dominant unseen force across decentralized derivative markets (EVM, Solana, AppChains).

### Phase 1: Foundation (Completed)
- [x] Integrate Hyperliquid, EdgeX, Lighter, Backpack, and 01 Exchange via WebSockets.
- [x] Build shared memory lock-free matrix for Zero-Copy IPC.
- [x] Implement Rust Strategy Engine Base (Arbitrage + Single-Exchange Quant).
- [x] Refactor Go feeder into a unified Configuration architecture.

### Phase 2: Execution & Routing (Up Next)
- [ ] **Smart Order Routing (SOR)**: Optimize swap paths to minimize slippage across split-routing.
- [ ] **Native Wallets & Signing**: Implement highly optimized Ed25519 & ECDSA signing natively in Rust.
- [ ] **Inventory Management**: Global real-time risk evaluation and portfolio balancing.

### Phase 3: Hardware & Dominance
- [ ] **FPGA Acceleration**: Move the shared memory reading and order generation directly to hardware logic gates.
- [ ] **Proprietary Network Stack**: Bypass kernel TCP/IP using Kernel Bypass (DPDK/Solarflare) for sub-10 microsecond tick-to-trade.
- [ ] **Cross-Chain Atomic Settlement**: Exploit block-space arbitrage directly on L1/L2 sequencers.

---

## 🛠️ Usage

1. Setup the configuration:
   ```bash
   cp config.example.toml config.toml
   # Edit config.toml to enable/disable specific exchanges
   ```

2. Run the Data Feeder (Terminal 1)
   ```bash
   cd feeder
   go run .
   ```

3. Run the Arbitrage & Strategy Core (Terminal 2)
   ```bash
   cargo run --release
   ```
