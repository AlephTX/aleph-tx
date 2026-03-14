# Quick Reference - 代码优化要点

## 关键修复

### 1. Shadow Ledger 对账逻辑
```rust
// ❌ 错误：没有考虑订单方向
self.in_flight_pos -= event.fill_size;
self.real_pos += event.fill_size;

// ✅ 正确：使用 OrderSide.sign()
let signed_fill = order.side.sign() * event.fill_size;
self.in_flight_pos -= signed_fill;
self.real_pos += signed_fill;
```

### 2. PnL 计算
```rust
// ❌ 错误：只处理买入
self.realized_pnl -= event.fill_price * event.fill_size + event.fee_paid;

// ✅ 正确：区分买卖
match order.side {
    OrderSide::Buy => {
        self.realized_pnl -= event.fill_price * event.fill_size + event.fee_paid;
    }
    OrderSide::Sell => {
        self.realized_pnl += event.fill_price * event.fill_size - event.fee_paid;
    }
}
```

### 3. BBO 读取
```rust
// ❌ 错误：API 不存在
let bbo = self.shm_reader.read_bbo(self.symbol_id, 2)?;

// ✅ 正确：使用实际 API
let exchanges = self.shm_reader.read_all_exchanges(self.symbol_id);
let lighter_bbo = exchanges.iter()
    .find(|(exch_id, _)| *exch_id == 2)
    .map(|(_, msg)| msg);
```

### 4. 竞态条件
```rust
// ❌ 错误：多次读取共享状态
let total_exposure = { let ledger = self.ledger.read(); ... };
// ... 很多代码 ...
let ledger_state = self.ledger.read().state(); // 可能已改变

// ✅ 正确：一次性读取
let (total_exposure, ledger_state) = {
    let ledger = self.ledger.read();
    let state = ledger.state();
    (state.read().total_exposure(), Arc::clone(&state))
};
```

### 5. 错误处理
```rust
// ❌ 错误：丢失类型信息
pub async fn foo() -> Result<T, Box<dyn Error>> { ... }

// ✅ 正确：类型化错误
use crate::error::{Result, TradingError};
pub async fn foo() -> Result<T> { ... }
```

### 6. 重试逻辑
```rust
// ❌ 错误：失败立即回滚
if !response.status().is_success() {
    ledger.write().add_in_flight(-signed_size);
    return Err(...);
}

// ✅ 正确：指数退避重试
const MAX_RETRIES: u32 = 3;
for retry in 0..MAX_RETRIES {
    match self.send_order(&order_req).await {
        Ok(id) => return Ok(id),
        Err(e) if retry < MAX_RETRIES - 1 => {
            let backoff = 100 * 2u64.pow(retry);
            tokio::time::sleep(Duration::from_millis(backoff)).await;
        }
        Err(e) => {
            ledger.write().add_in_flight(-signed_size);
            return Err(...);
        }
    }
}
```

### 7. 间隙检测
```rust
// ✅ 在 try_read() 中检测 ring buffer 溢出
let unread = write_idx.saturating_sub(self.local_read_idx);
if unread > RING_BUFFER_SLOTS {
    let gap_size = unread - RING_BUFFER_SLOTS;
    tracing::error!("Event gap: {} events lost", gap_size);
    self.local_read_idx = write_idx.saturating_sub(RING_BUFFER_SLOTS);
}
```

## 编程规范

### 错误处理
- ✅ 使用 `thiserror` 定义类型化错误
- ✅ 返回 `Result<T>` 而不是 `Result<T, Box<dyn Error>>`
- ✅ 错误包含上下文信息
- ✅ 关键操作有重试机制

### 并发安全
- ✅ 最小化锁持有时间
- ✅ 避免在锁内执行异步操作
- ✅ 一次性读取共享状态避免竞态
- ✅ 使用 `Arc<RwLock<T>>` 共享状态

### 文档
- ✅ 所有公共 API 有文档注释
- ✅ 不安全代码有安全说明
- ✅ 模块级文档解释架构
- ✅ 复杂逻辑有内联注释

### 代码组织
- ✅ 删除未使用的代码
- ✅ 配置可调不硬编码
- ✅ 单一职责原则
- ✅ 清晰的命名

## 运行测试

```bash
# 编译检查
cargo check

# 运行单元测试
cargo test --lib

# 运行特定测试
cargo test shadow_ledger::tests

# 运行策略
cargo run --release --bin inventory_neutral_mm
```

## 下一步

1. 在事件结构中添加 `order_side` 字段
2. 添加集成测试
3. 添加 Prometheus 指标
4. 将配置移到 TOML 文件
