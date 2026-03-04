# Adaptive Market Maker - 优化总结

## 问题诊断

### 问题1: 账户余额显示$0.00
**原因**:
- Makefile在启动策略前删除共享内存文件
- Feeder需要时间连接WebSocket并接收第一条账户统计消息
- 策略启动时共享内存还未写入数据

**解决方案**:
1. 增加Makefile启动延迟（2秒→5秒）
2. 策略启动时轮询等待账户统计可用（最多10秒）
3. 验证数据有效性（collateral > 0 或 available_balance > 0）

### 问题2: 单边成交导致套牢
**原因**:
- 卖单频繁成交，买单很少成交
- 导致持续做空并亏损

**解决方案**:
1. 放宽双边挂单阈值到90%（之前可能更严格）
2. 添加套牢检测和自动平仓逻辑
3. 独立的买单和卖单逻辑，确保双边挂单

### 问题3: 订单频率不够高
**原因**:
- 订单TTL太长（30秒）
- Requote阈值太大（5bps）

**解决方案**:
1. 强制1秒刷新订单（无论价格变化）
2. 降低requote阈值到1bps
3. 主循环200ms延迟（5次/秒检查）

## 核心优化

### 1. 账户统计初始化（src/strategy/adaptive_mm.rs:203-225）
```rust
// 轮询等待账户统计可用
info!("⏳ Waiting for account stats from feeder...");
let mut retries = 0;
let max_retries = 10;
loop {
    let stats = self.account_stats_reader.read();
    if stats.collateral > 0.0 || stats.available_balance > 0.0 {
        self.account_stats = stats.into();
        self.session_start_balance = self.account_stats.available_balance;
        info!("✅ Account stats loaded: ${:.2} available",
              self.account_stats.available_balance);
        break;
    }

    retries += 1;
    if retries >= max_retries {
        error!("❌ Timeout waiting for account stats after {}s", max_retries);
        return Err(...);
    }

    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
}
```

### 2. 高频刷新逻辑（src/strategy/adaptive_mm.rs:608-622）
```rust
fn should_requote(&self, active_order: &Option<ActiveOrder>, new_price: f64) -> bool {
    match active_order {
        None => true,
        Some(order) => {
            // 1. 价格偏离检查（>1bps）
            let price_diff = (new_price - order.price).abs();
            let deviation_bps = (price_diff / order.price) * 10000.0;

            // 2. 时间强制刷新（>1秒）
            let age = order.placed_at.elapsed();

            deviation_bps > 1.0 || age > Duration::from_secs(1)
        }
    }
}
```

### 3. 双边挂单逻辑（src/strategy/adaptive_mm.rs:464-552）
```rust
// 买单：仓位 < 90% max_position
let can_buy = total_exposure < self.max_position * 0.9;
if should_requote_bid && can_buy {
    // 取消旧订单
    if let Some(ref order) = self.active_bid {
        self.http_client.cancel_order(...).await;
    }
    // 下新买单
    self.http_client.place_order_optimistic(...).await;
}

// 卖单：仓位 > -90% max_position
let can_sell = total_exposure > -self.max_position * 0.9;
if should_requote_ask && can_sell {
    // 取消旧订单
    if let Some(ref order) = self.active_ask {
        self.http_client.cancel_order(...).await;
    }
    // 下新卖单
    self.http_client.place_order_optimistic(...).await;
}
```

### 4. 套牢检测和平仓（src/strategy/adaptive_mm.rs:389-420）
```rust
// 检测过度仓位
if total_exposure.abs() > self.max_position {
    let excess = total_exposure.abs() - self.max_position;
    warn!("⚠️  Position {:.4} exceeds max {:.4}, closing excess {:.4}",
          total_exposure, self.max_position, excess);

    let close_side = if total_exposure > 0.0 {
        OrderSide::Sell
    } else {
        OrderSide::Buy
    };

    match self.http_client.place_market_order(
        self.market_id,
        close_side,
        excess
    ).await {
        Ok(_) => info!("✅ Closed excess position: {:.4} ETH", excess),
        Err(e) => error!("❌ Failed to close excess: {:?}", e),
    }

    tokio::time::sleep(Duration::from_millis(2000)).await;
    continue;
}
```

### 5. Graceful Shutdown（src/strategy/adaptive_mm.rs:318-343）
```rust
if let Some(ref mut rx) = shutdown && *rx.borrow() {
    info!("Shutdown signal received, cleaning up...");

    // 1. 取消所有订单
    if let Err(e) = self.http_client.cancel_all_open_orders(
        self.market_id as u8
    ).await {
        error!("Failed to cancel orders: {:?}", e);
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    // 2. 检查并平仓
    let net_pos = self.ledger.read().position();
    if net_pos.abs() > 0.0001 {
        info!("🔄 Closing position: {:.4} ETH", net_pos);
        let close_side = if net_pos > 0.0 {
            OrderSide::Sell
        } else {
            OrderSide::Buy
        };
        match self.http_client.place_market_order(
            self.market_id,
            close_side,
            net_pos.abs()
        ).await {
            Ok(_) => info!("✅ Position closed"),
            Err(e) => error!("❌ Failed to close: {:?}", e),
        }
    }

    self.print_session_summary();
    return Ok(());
}
```

### 6. 动态订单大小（src/strategy/adaptive_mm.rs:565-577）
```rust
fn calculate_order_size(&self, available_balance: f64, mid_price: f64) -> f64 {
    // 使用可用余额的1%（高频小单）
    let size_from_balance = (available_balance * 0.01) / mid_price;

    // 使用base_size作为最小值
    let size = size_from_balance.max(self.base_order_size);

    // 上限0.01 ETH (~$20)
    let size = size.min(0.01);

    // 四舍五入到step_size
    (size / self.step_size).floor() * self.step_size
}
```

## 关键参数

```rust
base_spread_bps: 3,            // 0.03% 超紧价差
min_spread_bps: 2,             // 0.02% 最小
max_spread_bps: 15,            // 0.15% 最大
base_order_size: 0.001,        // 0.001 ETH (~$2)
max_position: 0.03,            // 0.03 ETH 最大仓位
max_leverage: 10.0,            // 10x 最大杠杆
inventory_skew_factor: 0.05,   // 5% 库存偏移
min_available_balance: 2.0,    // $2 最小余额
```

## 启动流程

1. **取消所有现有订单**
2. **等待账户统计可用**（最多10秒）
3. **检查并平掉现有仓位**
4. **安全检查**：
   - 杠杆 < 10x
   - 余额 > $10
5. **开始高频双边做市**

## 停止流程（Makefile）

```makefile
adaptive-down:
	@echo "🛑 Stopping adaptive market maker..."
	@if [ -f pids/adaptive-mm.pid ]; then \
		echo "📤 Sending graceful shutdown signal (SIGINT)..."; \
		kill -2 $$(cat pids/adaptive-mm.pid) 2>/dev/null || true; \
		sleep 10; \
		kill -9 $$(cat pids/adaptive-mm.pid) 2>/dev/null || true; \
	fi
```

使用`kill -2` (SIGINT)允许程序graceful shutdown，等待10秒后才强制终止。

## 测试验证

运行测试脚本：
```bash
./test_adaptive_mm.sh
```

预期输出：
- ✅ 账户统计读取正常（$202.08）
- ✅ 共享内存文件存在
- ✅ Feeder正常推送账户统计

## 下一步

1. 启动策略：`make adaptive-up`
2. 观察日志：`make adaptive-logs`
3. 验证双边挂单和高频刷新
4. 监控仓位和PnL
5. 测试graceful shutdown：`make adaptive-down`
