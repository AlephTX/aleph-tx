# Gemini 3.1 Pro Review — Issue 验证与分析

> 逐一验证 6 个 Issue 的真实性，标注 ✅ 真实 Bug / ⚠️ 部分正确 / ❌ 误报

---

## Issue 1: Architecture Decoupling & Directory Structure ⚠️ 部分正确

**Gemini 说法**: exchange 代码散落在 `src/` 根目录，策略硬编码 exchange ID。

**实际验证**:
- `lighter_trading.rs`, `lighter_ffi.rs`, `lighter_orders.rs` 确实在 `src/` 根目录 ✅
- `backpack_api/`, `edgex_api/` 已经是子目录结构 ❌（Gemini 说"dumped directly"不准确）
- `exchange.rs` 已有 `Exchange` trait 抽象 ❌（Gemini 说需要创建，实际已存在）
- `config.toml` 已有 `[inventory_neutral_mm]` 配置节 ❌（已经 config-driven）
- `main.rs` 确实硬编码了 exchange ID (3, 5) 和 symbol ID (1002) ✅
- `inventory_neutral_mm.rs` 通过 config 读取 exchange_id/symbol_id ✅（已解耦）

**结论**: Lighter 相关文件可以移入 `src/exchanges/lighter/`，但这是**重构优化**，不是 critical bug。`main.rs` 的硬编码是遗留代码（旧策略），新策略已经 config-driven。

**优先级**: P3 (低) — 纯重构，不影响交易正确性

---

## Issue 2: Shadow Ledger Leak in `lighter_trading.rs` ✅ 真实 Bug

**Gemini 说法**: `place_order` 调用 `add_in_flight` 但没有 `register_order`；`place_batch` 完全没有 optimistic accounting。

**实际验证** (`lighter_trading.rs:522-612`):

### `place_order` (L522-554):
```rust
// L528-530: ✅ 有 add_in_flight
if let Some(ref ledger) = self.ledger {
    ledger.write().add_in_flight(signed_size);
}
// ❌ 没有 register_order(client_order_index, side, size)
// 成功后直接返回，没有注册订单到 ledger
```
→ **确认 Bug**: 当 fill event 到达时，ledger 无法匹配 order，触发 "Untracked fill"，`in_flight_pos` 永远不会被扣减。

### `place_batch` (L581-612):
```rust
// ❌ 完全没有 add_in_flight
// ❌ 完全没有 register_order
// 直接签名 → send_tx_batch → 返回
```
→ **确认 Bug**: 批量下单期间（~50ms RTT），策略对 in-flight exposure 完全无感知。

### `shadow_ledger.rs` 验证:
- `register_order(order_id, symbol_id, side, size)` 方法存在 (L87-110)
- `apply_event` 中 OrderFilled 处理 (L140-190): 如果 order 不在 `active_orders` 中，走 "Untracked fill" 路径，只更新 `real_pos`，不扣减 `in_flight_pos`

**优先级**: P0 (Critical) — 直接导致 exposure 膨胀，可能触发过度下单

---

## Issue 3: Event Reconciliation & Safety in `shadow_ledger.rs` ✅ 真实 Bug

**Gemini 说法**: 如果 Go feeder 丢事件，`real_pos` 永久损坏。需要 REST API 强制同步。

**实际验证** (`shadow_ledger.rs:120-138`):
```rust
// L120-138: sequence gap 检测
if event.sequence <= self.last_sequence {
    return Err(TradingError::OutOfOrderEvent { ... });
}
if event.sequence > self.last_sequence + 1 {
    tracing::warn!("Sequence gap: {} -> {}", self.last_sequence, event.sequence);
    // ⚠️ 只是 warn，继续处理，但丢失的事件中的 fill 永远不会被计入
}
```

**但是**: `inventory_neutral_mm.rs:251-252` 每个循环都从 `AccountStatsReader` 读取 position:
```rust
let stats = self.account_stats_reader.read();
self.account_stats = stats.into();
let position = self.account_stats.position;
```
→ 策略实际上用的是 SHM 中 Go feeder 写入的 `position`，不是 `shadow_ledger.real_pos`！

**结论**: 对于 `inventory_neutral_mm`，这个 bug 的影响被 `AccountStatsReader` 缓解了。但 `adaptive_mm` 也有类似模式。真正的风险在于 `in_flight_pos` 的准确性（与 Issue 2 关联）。

添加 `force_sync` 方法仍然是好的防御性编程。

**优先级**: P1 (High) — 防御性修复，与 Issue 2 配合

---

## Issue 4: Strategy Loop Inefficiencies & AS Filter Flaw ⚠️ 部分正确

### 4a: 100ms sleep 毁了纳秒架构

**实际验证**:
- `adaptive_mm.rs:651`: `tokio::time::sleep(Duration::from_millis(100)).await;` ✅
- `inventory_neutral_mm.rs:288`: `tokio::time::sleep(Duration::from_millis(100)).await;` — 需要确认

**但是**: Gemini 的建议（1ms sleep 或 yield_now）在实际场景中不合理：
- Lighter DEX 限流: 6000 req/min = 100 req/s
- 每个 cycle 消耗 ~3 req (cancel + batch)
- 100ms = 10 cycles/s = 30 req/s，已经是合理的节奏
- 改成 1ms = 1000 cycles/s，但 99% 的 cycle 会因为 `should_requote` 返回 false 而空转
- 真正的优化是**事件驱动**（BBO 变化时才唤醒），但这需要重构 SHM reader 为 futex/eventfd

**结论**: 100ms 对当前 Lighter DEX 是合理的。可以改为 config-driven 的 `poll_interval_ms`。

### 4b: AS Filter 不撤单 ✅ 真实 Bug

**实际验证** (`inventory_neutral_mm.rs:286-289`):
```rust
if as_score > self.config.adverse_selection_threshold {
    debug!("AS filter triggered: score={:.2} (pausing)", as_score);
    tokio::time::sleep(Duration::from_millis(50)).await;
    continue;  // ❌ 没有 cancel_all_orders()！
}
```
→ **确认 Bug**: 检测到毒性流后暂停报价，但已挂的订单还在 book 上，会被 adverse flow 吃掉。

`adaptive_mm.rs` 同样的问题 (L486-489 区域需要确认)。

**优先级**: P0 (Critical) — 直接导致亏损

---

## Issue 5: IPC Seqlock Deadlock Risk ✅ 真实 Bug

**实际验证** (`shm_reader.rs:86-106`):
```rust
loop {
    let seq1 = unsafe { (*seq_ptr).load(Ordering::Acquire) };
    if seq1 & 1 != 0 {
        std::hint::spin_loop();
        continue; // ❌ 无限循环，如果 Go feeder 崩溃在写入中间
    }
    // ...
    let seq2 = unsafe { (*seq_ptr).load(Ordering::Acquire) };
    if seq1 == seq2 {
        break;
    }
    // ❌ 外层也是无限循环
}
```

同样的问题在 `account_stats_reader.rs:94-121`:
```rust
pub fn read(&mut self) -> AccountStatsSnapshot {
    loop {
        let version_before = self.stats.version.load(Ordering::Acquire);
        if !version_before.is_multiple_of(2) {
            std::hint::spin_loop();
            continue; // ❌ 同样的无限循环风险
        }
        // ...
    }
}
```

**优先级**: P1 (High) — 生产环境中 feeder 崩溃会导致 Rust 进程 100% CPU 挂死

---

## Issue 6: Strategy Capital Efficiency ⚠️ 部分正确

### 6a: Post-Only Orders
**验证**: `lighter_ffi.rs:CreateOrderTxReq` 有 `time_in_force: u8` 字段。
当前 `lighter_trading.rs` 中 `sign_order` 使用 `time_in_force: 0`（GTC）。
Lighter 支持 Post-Only (TIF=4)。
→ ✅ 可以改进，但需要确认 Lighter 的 TIF 值

### 6b: Grid/Ladder Quoting
→ 这是**新功能**，不是 bug fix。需要大量设计工作。

### 6c: Dynamic Size Scaling
→ `inventory_neutral_mm` 已经有 `calculate_asymmetric_sizes`，根据 inventory 动态调整。
→ 但没有根据 volatility 调整深度分布。

**优先级**: P2 (Medium) — Post-Only 是快速改进；Grid quoting 是新功能

---

## 优先级排序

| Priority | Issue | Description | Effort |
|----------|-------|-------------|--------|
| P0 | #2 | Shadow Ledger leak (register_order missing) | 2h |
| P0 | #4b | AS filter 不撤单 | 30min |
| P1 | #5 | Seqlock 无限循环保护 | 1h |
| P1 | #3 | Shadow Ledger force_sync | 1h |
| P2 | #6a | Post-Only orders | 30min |
| P2 | #4a | Poll interval config-driven | 30min |
| P3 | #1 | Directory restructure | 4h |
| P3 | #6b/c | Grid quoting / dynamic sizing | 8h+ |
