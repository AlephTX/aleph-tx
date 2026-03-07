# AlephTX 架构现状与建议

## 当前架构分析

### 双轨运行模式

AlephTX 目前有两种运行模式：

#### 1. 生产模式（推荐）
```bash
make live-up        # 启动 examples/inventory_neutral_mm.rs
make adaptive-up    # 启动 examples/adaptive_mm.rs
```

**特点**:
- 单策略专注执行
- 完整的 Shadow Ledger + Event Consumer 集成
- 优雅关闭（15s 超时，自动撤单平仓）
- 日志独立（`logs/inventory-neutral-live.log`）

**架构**:
```
examples/inventory_neutral_mm.rs
  ├─ ShadowLedgerManager::new()
  ├─ ShmEventReader::new_default()
  ├─ ledger_manager.spawn_consumer(event_reader)
  ├─ ShmReader::open("/dev/shm/aleph-matrix")
  ├─ AccountStatsReader::open("/dev/shm/aleph-account-stats")
  ├─ LighterTrading::new(market_id)
  └─ InventoryNeutralMM::new(...).run()
```

#### 2. 通用多策略引擎（实验性）
```bash
make up STRATEGY=lighter    # 启动 main.rs (多策略)
make up STRATEGY=edgex
make up STRATEGY=backpack
```

**特点**:
- 多策略并行执行（ArbitrageEngine + MarketMakerStrategy + BackpackMMStrategy）
- 共享 ShmReader
- 通用的 `Strategy` trait 接口
- 适合跨交易所套利

**架构**:
```
src/main.rs
  ├─ ShmReader::open("/dev/shm/aleph-matrix")
  ├─ Vec<Box<dyn Strategy>>
  │   ├─ ArbitrageEngine::new(25.0)
  │   ├─ MarketMakerStrategy::new(EdgeX)
  │   └─ BackpackMMStrategy::new(Backpack)
  └─ Main loop: reader.try_poll() → strategy.on_bbo_update()
```

## 重构完成状态

### ✅ Phase 1 & 2: 模块化交易所集成

**完成内容**:
1. 创建 `src/exchanges/` 目录结构
2. 搬迁 lighter/backpack/edgex 代码
3. 实现 `Exchange` trait gateway（Backpack 完整，EdgeX 简化）
4. 向后兼容的 re-export

**验证**:
```bash
✅ cargo check - 通过
✅ LD_LIBRARY_PATH=./lib cargo test --lib - 20 个测试全部通过
✅ make build - 通过
```

### ⚠️ Phase 3 & 4: Config-Driven Factory（未实施）

**原计划**:
- 扩展 `config.toml` 添加 `[strategy]` section
- 重写 `main.rs` 为工厂模式
- 动态 Makefile: `make run EXCHANGE=backpack STRATEGY=inventory_neutral_mm`

**现状分析**:
- `main.rs` 已经是多策略引擎，不适合改造为单策略工厂
- `examples/` 中的单策略入口点已经很好地工作
- Makefile 已经支持 `make up STRATEGY=xxx` 动态模式

**建议**: **不需要实施 Phase 3 & 4**，原因：
1. 当前架构已经满足需求（生产模式 + 实验模式）
2. `examples/` 提供了清晰的单策略入口点
3. `main.rs` 作为多策略引擎有其独特价值（跨交易所套利）
4. 过度工厂化会增加复杂度，违反"最小修改"原则

## 建议的改进方向

### 1. 创建更多 examples/ 入口点

为每个交易所创建专用的 example：

```bash
examples/
  inventory_neutral_mm.rs    # ✅ 已存在 (Lighter)
  adaptive_mm.rs             # ✅ 已存在 (Lighter)
  backpack_mm.rs             # 🆕 使用 BackpackGateway
  edgex_mm.rs                # 🆕 使用 EdgeXGateway (需要 L2 签名)
```

### 2. 完善 EdgeX Gateway

`src/exchanges/edgex/gateway.rs` 当前是简化 stub，需要：
- 完整的 L2 StarkNet 签名集成
- 调用 `SignatureManager::calc_limit_order_hash`
- 实现 `create_order_internal` 的完整逻辑

### 3. 统一配置管理

扩展 `config.toml` 添加交易所配置：

```toml
[exchanges.lighter]
base_url = "https://mainnet.zklighter.elliot.ai"
chain_id = 304
market_id = 0
env_file = ".env.lighter"

[exchanges.backpack]
base_url = "https://api.backpack.exchange"
symbol = "ETH_USDC_PERP"
env_file = ".env.backpack"

[exchanges.edgex]
base_url = "https://pro.edgex.exchange"
contract_id = 10000002
env_file = ".env.edgex"
```

### 4. 改进 Makefile

为新的 examples 添加快捷命令：

```makefile
# Backpack MM
backpack-up: build-feeder
    @export $$(cat .env.backpack | xargs) && \
        cargo run --release --example backpack_mm > logs/backpack-mm.log 2>&1 &

# EdgeX MM
edgex-up: build-feeder
    @export $$(cat .env.edgex | xargs) && \
        cargo run --release --example edgex_mm > logs/edgex-mm.log 2>&1 &
```

## 测试计划

### 单元测试 ✅
```bash
LD_LIBRARY_PATH=./lib cargo test --lib
# 结果: 20 passed; 0 failed
```

### 集成测试（需要实际环境）

#### Lighter DEX (生产就绪)
```bash
make live-up
make live-logs
# 观察订单执行、成交、Shadow Ledger 更新
make live-down
```

#### Backpack (需要创建 example)
```bash
# TODO: 创建 examples/backpack_mm.rs
# 使用 BackpackGateway + InventoryNeutralMM
make backpack-up
```

#### EdgeX (需要 L2 签名实现)
```bash
# TODO: 完成 edgex/gateway.rs L2 签名
# TODO: 创建 examples/edgex_mm.rs
make edgex-up
```

## 技术债务清单

| 项目 | 优先级 | 工作量 | 描述 |
|------|--------|--------|------|
| EdgeX L2 签名 | 高 | 中 | 完成 `gateway.rs` 的 `create_order_internal` |
| Backpack Example | 中 | 低 | 创建 `examples/backpack_mm.rs` |
| EdgeX Example | 中 | 低 | 创建 `examples/edgex_mm.rs` |
| 统一配置 | 低 | 低 | 扩展 `config.toml` 添加 `[exchanges.*]` |
| 清理 unused 警告 | 低 | 极低 | 删除 `side_to_string`, `side_to_edgex` 等 |

## 结论

**Phase 1 & 2 重构成功完成**，代码库更加模块化和可维护。

**Phase 3 & 4 不建议实施**，因为：
1. 当前架构已经很好地支持多交易所
2. `examples/` 提供了清晰的单策略入口点
3. `main.rs` 作为多策略引擎有其独特价值
4. 过度工厂化会增加不必要的复杂度

**下一步建议**:
1. 完成 EdgeX Gateway 的 L2 签名集成
2. 创建 `examples/backpack_mm.rs` 和 `examples/edgex_mm.rs`
3. 在实际环境中测试 Backpack 和 EdgeX 集成

重构为未来的多交易所支持奠定了坚实基础，同时保持了代码的简洁性和可维护性。
