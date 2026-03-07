# Adaptive Market Maker - 快速启动指南

## 前置检查

1. **确认环境配置**：
```bash
cat .env.lighter  # 检查API密钥和账户配置
```

2. **测试账户统计**：
```bash
./test_adaptive_mm.sh
```

预期输出应显示账户余额（如$202.08）。

## 启动策略

```bash
make adaptive-up
```

这将：
1. 清理旧的共享内存文件
2. 启动Go feeder（WebSocket数据流）
3. 等待5秒让feeder初始化
4. 启动Rust策略

## 监控运行

### 实时日志
```bash
make adaptive-logs
```

### 关键指标
观察日志中的：
- ✅ Account stats loaded: $XXX available
- 📈 Buy: $XXXX x 0.XXXX ETH
- 📉 Sell: $XXXX x 0.XXXX ETH
- 💰 PnL updates (每60秒)

### 预期行为
- **双边挂单**：同时看到买单和卖单
- **高频刷新**：每1秒刷新订单
- **动态订单大小**：基于可用余额的1%
- **仓位控制**：最大0.03 ETH，超过自动平仓

## 停止策略

```bash
make adaptive-down
```

这将：
1. 发送SIGINT信号（graceful shutdown）
2. 策略自动取消所有订单
3. 策略自动平掉所有仓位
4. 打印会话总结
5. 等待10秒后强制终止（如果需要）

## 故障排查

### 问题1: 余额显示$0.00
**检查**：
```bash
# 查看feeder日志
grep "collateral=" logs/feeder-adaptive.log | tail -5

# 查看共享内存
hexdump -C /dev/shm/aleph-account-stats | head -5
```

**解决**：
- 等待更长时间（策略会自动轮询10秒）
- 检查feeder是否正常连接WebSocket
- 检查API密钥是否正确

### 问题2: 只有单边订单
**检查**：
```bash
# 查看仓位
grep "position" logs/adaptive-mm.log | tail -10
```

**原因**：
- 仓位接近max_position（0.03 ETH）
- 策略会暂停单边挂单直到仓位降低

### 问题3: 订单频率太低
**检查**：
```bash
# 统计订单频率
grep -E "Buy:|Sell:" logs/adaptive-mm.log | tail -20
```

**预期**：
- 每1秒至少刷新一次订单
- 价格变化>1bps时立即刷新

### 问题4: 套牢（仓位过大）
**自动处理**：
- 策略检测到仓位>max_position时自动平仓
- 查看日志中的"Position exceeds max"警告

**手动处理**：
```bash
# 停止策略（会自动平仓）
make adaptive-down
```

## 性能指标

### 目标
- **订单频率**：1秒/单（高频）
- **价差**：0.03% (3bps) 基准
- **订单大小**：0.001-0.01 ETH
- **最大仓位**：0.03 ETH
- **最大杠杆**：10x

### 监控
```bash
# 每分钟PnL更新
grep "PnL" logs/adaptive-mm.log

# 订单成交统计
grep "filled" logs/adaptive-mm.log | wc -l

# 平均价差
grep "spread=" logs/adaptive-mm.log | tail -20
```

## 安全机制

1. **启动安全检查**：
   - 杠杆 < 10x
   - 余额 > $10
   - 自动平掉现有仓位

2. **运行时风控**：
   - 杠杆 > 10x：停止下单，取消现有订单
   - 余额 < $2：停止下单
   - 仓位 > 0.03 ETH：自动平仓

3. **Graceful Shutdown**：
   - 取消所有订单
   - 平掉所有仓位
   - 打印会话总结

## 参数调整

如需调整参数，编辑`src/strategy/adaptive_mm.rs`：

```rust
base_spread_bps: 3,            // 价差（越小越激进）
base_order_size: 0.001,        // 订单大小
max_position: 0.03,            // 最大仓位
max_leverage: 10.0,            // 最大杠杆
min_available_balance: 2.0,    // 最小余额
```

修改后重新编译：
```bash
cargo build --release --example adaptive_mm
```

## 日志文件

- `logs/feeder-adaptive.log` - Go feeder日志
- `logs/adaptive-mm.log` - Rust策略日志
- `pids/feeder-adaptive.pid` - Feeder进程ID
- `pids/adaptive-mm.pid` - 策略进程ID

## 紧急停止

如果`make adaptive-down`无效：
```bash
# 强制终止所有进程
pkill -9 -f feeder-app
pkill -9 -f adaptive_mm

# 清理PID文件
rm -f pids/feeder-adaptive.pid pids/adaptive-mm.pid
```

## 支持

遇到问题请查看：
1. `OPTIMIZATION_SUMMARY.md` - 详细技术文档
2. `CLAUDE.md` - 项目架构说明
3. 日志文件 - 运行时详细信息
