# 订单执行架构重新设计 - 进度报告

## 当前状态

已完成订单请求共享内存架构的基础设施，但由于 lighter-go SDK 兼容性问题，订单执行器暂时禁用。

## 已完成的工作

### 1. Rust 端：订单请求发送器
**文件**: `src/order_request_buffer.rs`

- ✅ Lock-free ring buffer 实现
- ✅ `OrderRequest` 结构 (64字节，C-ABI 兼容)
- ✅ `OrderRequestWriter` 用于发送订单请求
- ✅ 支持两种请求类型：
  - `PlaceLimit`: 下限价单
  - `Cancel`: 撤单

### 2. Go 端：订单请求读取器
**文件**: `feeder/shm/order_requests.go`

- ✅ `OrderRequestBuffer` 实现
- ✅ `TryRead()` 非阻塞读取
- ✅ Gap 检测和恢复
- ✅ 与 Rust 端内存布局完全兼容

### 3. Go 端：订单执行器（未完成）
**文件**: `feeder/exchanges/lighter_executor.go.disabled`

- ⏸️ 使用 lighter-go SDK 执行订单
- ⏸️ Poseidon2 + Schnorr 签名认证
- ⏸️ 订单结果通过 WebSocket 事件返回

**阻塞原因**: lighter-go SDK v1.0.1 和 v1.0.2 都有编译错误：
```
client.go:30:28: too many arguments in call to curve.SampleScalar
```

这是 `poseidon_crypto` 依赖版本不匹配导致的。

## 架构设计

```
┌─────────────────────────────────────────────────────────────┐
│                      Rust Strategy                          │
│  (lighter_mm.rs)                                            │
└──────────────────┬──────────────────────────────────────────┘
                   │
                   │ place_order()
                   ▼
┌─────────────────────────────────────────────────────────────┐
│           OrderRequestWriter (Rust)                         │
│  /dev/shm/aleph-order-requests                              │
│  - Lock-free ring buffer                                    │
│  - 256 slots × 64 bytes                                     │
└──────────────────┬──────────────────────────────────────────┘
                   │
                   │ shared memory
                   ▼
┌─────────────────────────────────────────────────────────────┐
│           OrderRequestBuffer (Go)                           │
│  TryRead() every 1ms                                        │
└──────────────────┬──────────────────────────────────────────┘
                   │
                   ▼
┌─────────────────────────────────────────────────────────────┐
│         LighterOrderExecutor (Go) [DISABLED]                │
│  - lighter-go TxClient                                      │
│  - Poseidon2 + Schnorr signing                              │
│  - CreateLimitOrder()                                       │
│  - CancelLimitOrder()                                       │
└──────────────────┬──────────────────────────────────────────┘
                   │
                   │ HTTPS + Starknet签名
                   ▼
┌─────────────────────────────────────────────────────────────┐
│              Lighter DEX API                                │
│  https://mainnet.zklighter.elliot.ai/api/v1                 │
└──────────────────┬──────────────────────────────────────────┘
                   │
                   │ WebSocket events
                   ▼
┌─────────────────────────────────────────────────────────────┐
│         LighterPrivate (Go)                                 │
│  - OrderCreated, OrderFilled, OrderCanceled                 │
│  - Push to EventRingBuffer                                  │
└──────────────────┬──────────────────────────────────────────┘
                   │
                   ▼
┌─────────────────────────────────────────────────────────────┐
│           ShadowLedger (Rust)                               │
│  - Reconcile in_flight → real_pos                           │
│  - <1μs state queries                                       │
└─────────────────────────────────────────────────────────────┘
```

## 待完成的工作

### 方案 A: 修复 lighter-go SDK 兼容性 ⭐ 推荐
1. 调查 `poseidon_crypto` 版本冲突
2. 可能需要 fork lighter-go 并修复依赖
3. 或者联系 Lighter 团队获取修复

### 方案 B: 使用 Rust Starknet 库
1. 使用 `starknet-rs` 或 `cairo-rs`
2. 在 Rust 端实现 Poseidon2 + Schnorr 签名
3. 直接从 Rust 调用 Lighter HTTP API
4. 优点：不依赖 Go SDK
5. 缺点：需要重新实现签名逻辑

### 方案 C: 使用 lighter-go 的 WASM 版本
1. lighter-go 提供了 WASM 构建
2. 可以从 Rust 调用 WASM 模块
3. 需要研究 WASM 集成方式

## 当前临时方案

`src/lighter_orders.rs` 仍然使用错误的 HMAC-SHA256 认证，**不能用于实盘交易**。

建议：
1. 先修复 lighter-go SDK 兼容性问题
2. 完成 `LighterOrderExecutor` 实现
3. 更新 `lighter_mm.rs` 使用新的订单请求架构
4. 删除或废弃 `lighter_orders.rs`

## 性能预期

- 订单请求延迟: <10μs (共享内存写入)
- Go 处理延迟: <1ms (1ms 轮询间隔)
- 总延迟: <2ms (vs 当前 HTTP 直接调用的 50-200ms)
- 吞吐量: >1000 orders/sec

## 文件清单

### 新增文件
- `src/order_request_buffer.rs` - Rust 订单请求发送器
- `feeder/shm/order_requests.go` - Go 订单请求读取器
- `feeder/exchanges/lighter_executor.go.disabled` - Go 订单执行器（禁用）

### 修改文件
- `src/lib.rs` - 添加 `order_request_buffer` 模块
- `feeder/go.mod` - 降级 lighter-go 到 v1.0.1

### 待修改文件
- `src/strategy/lighter_mm.rs` - 切换到新的订单请求架构
- `feeder/main.go` - 启动 `LighterOrderExecutor`

## 下一步行动

1. **调查 lighter-go SDK 问题** (最高优先级)
   - 检查 poseidon_crypto 版本
   - 尝试不同的依赖组合
   - 考虑联系 Lighter 团队

2. **完成订单执行器**
   - 修复 SDK 兼容性后启用 `lighter_executor.go`
   - 集成到 `main.go`
   - 测试订单执行流程

3. **更新策略代码**
   - 修改 `lighter_mm.rs` 使用 `OrderRequestWriter`
   - 移除 `LighterHttpClient` 依赖
   - 测试完整流程

4. **实盘测试**
   - 小额测试订单
   - 验证签名正确性
   - 监控订单执行延迟
