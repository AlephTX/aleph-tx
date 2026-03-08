# PROMPT TO SEND TO CLAUDE

You can copy and paste the entire block below into Claude to initiate the next phase of refactoring:

---

**Role & Context:**
You are an Elite (Tier-1) Quantitative Trading Architect & Rust/Go Systems Engineer, working at the level of Citadel or Jump Trading.
We are upgrading our high-frequency crypto market maker "**AlephTX**". The system uses a Dual-Track IPC architecture: Go (`feeder/`) handles WebSockets and writes to `/dev/shm`, while Rust (`src/`) reads shared memory via Seqlocks and executes trades over HTTP. 
The foundation is solid (perfect 64-byte C-ABI alignment, Shadow Ledger bugs are fixed), but to compete with apex predators we need to perform a "Tier-1 Architectural & Alpha Leap".

I need you to implement the following high-priority refactoring items. Please provide the implementation plan first, and once we agree, we will implement them module by module.

### Part 1: Quantitative Alpha Engine Upgrades (Rust)
Our current `InventoryNeutralMM` strategy is too crude. Please upgrade it with the following mathematical models:
1. **Sigmoid Inventory Skew (S-Curve):** Replace the current linear inventory skew logic (`inv_ratio_clamp`). At low inventory, prioritize earning the spread. Squeeze the skew exponentially only when approaching the maximum inventory limit.
2. **Order Book Imbalance (OBI) & VWMicro Pricing:** Stop pricing purely off `(Bid + Ask) / 2`. Introduce L2 depth parsing to calculate Imbalance-Weighted Mid-Price. We must proactively skew our BBO based on L5 order book density to prevent being spoofed by Top-of-Book manipulation.
3. **Grid Quoting (Laddering):** Instead of firing a single monolithic batch order at the BBO, implement dynamic multi-level laddering (e.g., 3-5 tiers on each side: Level 1 aggressive, Level 2 passive, Level 3 deep). This captures volatile flash-crash wicks.

### Part 2: Extreme Latency & Systems Refactoring (Rust)
1. **Eliminate the Async Spin-Loop Trap:** In `src/main.rs`, we are currently running `std::hint::spin_loop()` directly inside a `tokio::runtime::Runtime` worker thread. This starves network I/O. Decouple the engine: Use a bare-metal `std::thread::spawn` pinned to a specific CPU core (`core_affinity`) for the SHM read loop. Communicate with a dedicated Tokio background task pool (for sending HTTP orders) using a lock-free channel (e.g., `crossbeam-channel` or `flume`).
2. **Lock-Free Shadow Ledger:** The hot path uses `Arc<RwLock<ShadowLedger>>`, which causes cross-thread cache-coherency ping-ponging. Redesign the state sharing. Use cache-aligned `AtomicI64` for critical metrics like `in_flight_pos`, or ensure the strategy thread holds exclusive mutation rights while event updates are handled lock-free.
3. **Robust Telemetry & Typed Errors:** Stop using naive `.to_string().contains("not enough margin")` error parsing. Convert REST errors to strongly-typed Enums. Furthermore, implement an asynchronous UDP `TelemetrySender` module to broadcast AS scores, spread latency, and rejection warnings.

### Part 3: Go Feeder Core GC & Resilience (Go)
1. **Purge `encoding/json` from the Hot Path:** In `feeder/exchanges/lighter*.go`, the heavy reliance on the standard library JSON unmarshaler creates horrific GC pressure and STW spikes due to reflection and heap allocations. Replace it with zero-copy byte extraction (SIMD/Fastjson) or at least `easyjson`. Float parsing must not allocate strings.
2. **Exponential Backoff Connection Manager:** The connection loop in `base.go` uses a hardcoded `time.Sleep(3s)` for reconnects. Refactor this to use industry-standard Exponential Backoff with Jitter to prevent API rate-limit bans during widespread exchange outages.

**Instructions:**
Please respond by drafting a step-by-step implementation plan. Start with "Part 1: Quantitative Alpha" so we can tackle the most complex logic first, and then move towards the latency and GC optimizations.
