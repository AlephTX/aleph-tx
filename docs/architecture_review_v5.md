# AlephTX System Architecture & Deep Review (v5.1+)

## 1. 系统架构全景视图 (Architecture Overview)

AlephTX 采用了**双子星架构 (Dual-Plane Architecture)**，将对延迟极度敏感的行情接收（Data Plane）与复杂的交易逻辑（Control/Strategy Plane）分离，通过共享内存（SHM）实现极低延迟的进程间通信。

### 核心组件拓扑

```mermaid
graph TD
    GoFeeder[Go Feeder Process] -->|BBO Updates| SHM_Matrix[/dev/shm/aleph-matrix]
    GoFeeder -->|Depth Updates| SHM_Depth[/dev/shm/aleph-depth]
    GoFeeder -->|Trade Events| SHM_Events[/dev/shm/aleph-events-v2]
    
    subgraph Rust Strategy Process
        DataThread[Data Plane Thread] -->|tokio channel| Router[Strategy Router]
        Router --> IN_MM[InventoryNeutralMM]
        Router --> Arb[ArbitrageEngine]
        
        IN_MM -->|Read| Tracker[OrderTracker v5]
        IN_MM -->|Read| SHM_Depth
        
        EventThread[Event Consumer Thread] -->|Poll| SHM_Events
        EventThread -->|Update| Tracker
        
        IN_MM -->|Sign via FFI| LighterSigner[lighter_ffi C++]
        IN_MM -->|HTTP REST| LighterAPI[Lighter Exchange]
    end
```

### 关键设计决策记录

1. **共享内存 IPC (Zero-Copy)**: 使用 `memmap2` 和 Lock-Free Ring Buffer (带有准确的 `Release-Acquire` memory barriers)，使得行情从 Go 传递到 Rust 实现了 sub-microsecond 的开销。
2. **乐观无锁状态机 (OrderTracker v5)**: 
   - 彻底废弃了 ShadowLedger 的双累加器模型。
   - 采用 per-order 生命周期状态追踪。下单前先注入追踪态 (PendingCreate)，API返回后绑定 Exchange ID，事件到达后更新 Fill 状态。
   - 保证了在任何时刻、任意并发下都不会出现单边暴算（Bilateral exposure hiding）。
3. **Avellaneda-Stoikov (A-S) 算法统一定价**:
   - 代替了基于 Hardcoded penny jumps + ad-hoc inventory skew 的粗暴逻辑。
   - 将波动率系数 (`σ`)、风险厌恶 (`γ`)、时间窗口 (`T`)、预估成交率 (`κ`) 融为一体。
4. **跨所Alpha信号融合**:
   - 读取其他所（EdgeX、Hyperliquid 等）的 BBO，生成外部共识价格，以避免在 Lighter DEX 上遭遇毒性订单 (Adverse Selection)。

---

## 2. 深度审查：哪些地方不合理或可优化 (Optimization Opportunities)

尽管当前架构已经接近机构级水准，经过深入梳理，依然存在以下几个阻碍“超低延迟 (Ultra-Low Latency)”和“极致性能”的关键瓶颈：

### 🔴 优化点 1: SHM Event Consumer 的睡眠调度 (Latency Degradation)
**现象**：
在 `inventory_neutral_mm.rs` 的 run 循环中启动了 V2 event_reader，当队列空时，它调用了 `tokio::time::sleep(Duration::from_millis(1)).await`。
**本质问题**：
在 Tokio 的调度器中，1ms的 sleep 会将当前 task 挂起交出控制权。由于 Linux 调度器的颗粒度和 Tokio 反应堆唤醒延迟，实际休眠时间经常在 1ms ~ 2ms。这意味着一笔订单在交易所成交后，策略层**最晚要等 2ms 才能感知到成交并重置 OrderTracker**。在 HFT 场景下，这可能导致错失下一步操作窗口。
**优化方案**：
将 Event Consumer 从 `tokio::spawn` 移至 `std::thread::spawn` (OS 原生线程)，使用 `std::thread::yield_now()` (Spin-Yield) 或 `std::hint::spin_loop()` 替代 `tokio::sleep`。

### 🟡 优化点 2: LighterTrading Client 发单阻塞隔离不够
**现象**：
`sign_order` 利用了 `tokio::task::spawn_blocking` 来调用 C++ Stark 签名。
**本质问题**：
`spawn_blocking` 依赖于 Tokio 的 blocking thread pool。如果同时发许多请求，或者其他策略阻塞了这个池子，签名会面临排队延迟。
**优化方案**：
鉴于 FFI 签名非常快 (<100us)，直接在 async task 中同步调用往往比切换到 blocking pool 所消耗的 context switch 的开销（~10us）更有效率。建议 Benchmark 确认后，考虑移除 `spawn_blocking`。

### 🟡 优化点 3: Main.rs 的主入口一致性
**现象**：
系统真正的核心主策略 `InventoryNeutralMM` 并没有直接注册在 `src/main.rs` 的 `strategies` 列表里。当前 `main.rs` 仅初始化了 EdgeX 和 Backpack 以及 ArbitrageEngine。
**本质问题**：
这使得主逻辑脱离了 `main.rs` 的全局优雅停机和数据流管线，通常意味着用户必须通过 `examples/` 或 `src/bin/` 单独启动 `InventoryNeutralMM`，降低了工程的一致性。
**优化方案**：
收敛入口点，将 `InventoryNeutralMM` 作为可选 feature 或根据 `config.toml` 配置在 `main.rs` 里统一启动。

### 🔵 优化点 4: OrderTracker 的 RwLock 粒度
**现象**：
`OrderTracker` 用了一把大的 `RwLock` 锁住整个 `TrackerState` (包含 active 和 completed 字典)。
**本质问题**：
每收到一笔 fill event 就要 write_lock()；每一轮行情轮询就算 net_exposure 也要 read_lock()。由于 HFT 每秒可能有数千次轮询，读锁和写锁之间会发生轻微争抢。
**优化方案**：
当前数量级（N<20 active orders）下这 <100ns 延迟完全可接受。但在未来扩展为数百个 grid levels 时，建议使用并发结构如 `DashMap` (带有 shared count)，或是完全 Lock-Free 的 State Machine (例如将 Exposure 单独抽成 Atomic Array)。

---

## 3. 下一步文档维护行动

要使得整个工程变成完全体并利于后续迭代，我们将需要更新工程的总架构文档（即 `CLAUDE.md` / `README.md`），以反映上述这些最新的改动（尤其是 `OrderTracker v5` 以及 `A-S` 算法的使用）。
