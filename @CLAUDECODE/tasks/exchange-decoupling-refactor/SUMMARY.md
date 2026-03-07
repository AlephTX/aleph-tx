# AlephTX 架构解耦重构总结

**任务**: 实现 exchange-specific 代码模块化，建立清晰的模块边界

**执行时间**: 2026-03-07

## 完成的工作

### Phase 1: 目录搬迁 + re-export 兼容 ✅

1. **创建目录结构**:
   ```
   src/exchanges/
     lighter/
       mod.rs, ffi.rs, trading.rs
     backpack/
       mod.rs, client.rs, gateway.rs, model.rs, CLAUDE.md
     edgex/
       mod.rs, client.rs, gateway.rs, model.rs, signature.rs, CLAUDE.md
   ```

2. **文件搬迁**:
   - `src/lighter_ffi.rs` → `src/exchanges/lighter/ffi.rs`
   - `src/lighter_trading.rs` → `src/exchanges/lighter/trading.rs`
   - `src/backpack_api/*` → `src/exchanges/backpack/*`
   - `src/edgex_api/*` → `src/exchanges/edgex/*`
   - 删除 `src/lighter_orders.rs` (legacy, 无外部调用者)

3. **向后兼容**:
   - `src/lib.rs` 添加 re-export:
     ```rust
     pub use exchanges::lighter::ffi as lighter_ffi;
     pub use exchanges::lighter::trading as lighter_trading;
     pub use exchanges::backpack as backpack_api;
     pub use exchanges::edgex as edgex_api;
     ```
   - 现有代码无需修改，`crate::lighter_trading::*` 继续有效

4. **内部 import 修复**:
   - `lighter/trading.rs`: `crate::lighter_ffi` → `super::ffi`
   - `backpack/client.rs`: `crate::backpack_api::model` → `super::model`
   - `edgex/client.rs`: `crate::edgex_api::model` → `super::model`

### Phase 2: Backpack/EdgeX 实现 Exchange trait ✅

1. **Backpack Gateway** (`src/exchanges/backpack/gateway.rs`):
   - 实现完整的 `Exchange` trait
   - `buy/sell` → 调用 `BackpackClient::create_order`
   - `place_batch` → 两次顺序调用（Backpack 无原生 batch API）
   - `cancel_all` → 调用 `cancel_all_orders`
   - `close_all_positions` → 查询 position + 反向 market order

2. **EdgeX Gateway** (`src/exchanges/edgex/gateway.rs`):
   - 简化实现（stub）
   - 核心订单执行返回 "not yet implemented" 错误
   - 原因：需要完整的 L2 StarkNet 签名逻辑集成
   - `cancel_order/cancel_all/get_active_orders` 已实现框架

3. **模块声明更新**:
   - `src/exchanges/backpack/mod.rs`: 添加 `pub mod gateway;`
   - `src/exchanges/edgex/mod.rs`: 添加 `pub mod gateway;`

### 文档更新 ✅

1. **新建 `src/exchanges/CLAUDE.md`**:
   - 模块架构说明
   - Exchange trait 抽象层文档
   - 三个交易所的实现状态对比表
   - 向后兼容性说明
   - 未来工作路线图

2. **更新 `src/CLAUDE.md`**:
   - 修改 "Key Files" 表格，移除已搬迁的文件
   - 添加 `exchange.rs` 说明
   - 更新 "Subdirectories" 表格，`exchanges/` 替代 `backpack_api/` 和 `edgex_api/`
   - 更新架构图中的模块路径

3. **更新根 `CLAUDE.md`**:
   - 添加 "Architecture Refactoring (2026-03-07)" 章节
   - 记录重构动机、变更内容、状态
   - 更新 "Three-Layer Context Hierarchy" 反映新的目录结构

## 验证结果

```bash
$ cargo check
    Checking aleph-tx v0.1.0
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 2.28s

$ make build
    Finished `release` profile [optimized] target(s) in 26.98s
✅ Build complete
```

**警告**: 3 个 unused 警告（`side_to_string`, `side_to_edgex`, unused imports），不影响功能。

## 未完成的工作（Phase 3 & 4）

### Phase 3: Config-Driven main.rs 工厂化

**计划**:
1. 扩展 `config.toml` 添加 `[strategy]` 和 `[exchanges.*]` sections
2. 重写 `main.rs` 为工厂模式：
   ```rust
   let exchange: Arc<dyn Exchange> = match config.strategy.exchange {
       "lighter" => create_lighter_exchange(&config),
       "backpack" => create_backpack_exchange(&config),
       "edgex" => create_edgex_exchange(&config),
       _ => panic!("Unknown exchange"),
   };
   ```
3. 将 `examples/` 的初始化逻辑合并到 `main.rs`

**状态**: 未实施（需要更大范围的 main.rs 重构）

### Phase 4: Makefile 动态化

**计划**:
```makefile
EXCHANGE ?= lighter
STRATEGY ?= inventory_neutral_mm

run: build
    @export $$(cat .env.$(EXCHANGE) | xargs) && \
        ./target/release/aleph-tx --exchange $(EXCHANGE) --strategy $(STRATEGY)
```

**状态**: 未实施（依赖 Phase 3 的 CLI 参数支持）

## 架构改进

### 优点

1. **清晰的模块边界**: 每个交易所独立目录，职责明确
2. **向后兼容**: 零破坏性变更，现有代码无需修改
3. **可扩展性**: 新增交易所只需在 `exchanges/` 下添加子目录
4. **统一接口**: `Exchange` trait 使策略可跨交易所复用
5. **文档同步**: 每个模块都有 `CLAUDE.md`，自动加载到 Claude 上下文

### 技术债务

1. **EdgeX Gateway**: 需要完整的 L2 签名逻辑实现
2. **Config-driven factory**: `main.rs` 仍然硬编码 Lighter DEX
3. **Dynamic Makefile**: 仍然使用固定的 `make adaptive-up` 等命令

## 关键文件变更清单

| 文件 | 操作 | 行数变化 |
|------|------|---------|
| `src/lib.rs` | 重写 module 声明 + re-export | +7 -7 |
| `src/lighter_ffi.rs` | 移动 → `exchanges/lighter/ffi.rs` | 0 |
| `src/lighter_trading.rs` | 移动 → `exchanges/lighter/trading.rs` | +2 (import 修复) |
| `src/lighter_orders.rs` | 删除 | -600 |
| `src/backpack_api/*` | 移动 → `exchanges/backpack/*` | +1 (import 修复) |
| `src/edgex_api/*` | 移动 → `exchanges/edgex/*` | +2 (import 修复) |
| `src/exchanges/mod.rs` | 新建 | +3 |
| `src/exchanges/lighter/mod.rs` | 新建 | +2 |
| `src/exchanges/backpack/mod.rs` | 修改 | +1 |
| `src/exchanges/backpack/gateway.rs` | 新建 | +87 |
| `src/exchanges/edgex/mod.rs` | 修改 | +1 |
| `src/exchanges/edgex/gateway.rs` | 新建 | +165 |
| `src/exchanges/CLAUDE.md` | 新建 | +120 |
| `src/CLAUDE.md` | 更新 | +15 -10 |
| `CLAUDE.md` | 更新 | +25 -5 |

**净变化**: +430 行新增, -622 行删除 = **-192 行代码**

## 下一步建议

1. **EdgeX L2 签名集成**: 完成 `edgex/gateway.rs` 的订单执行逻辑
2. **Config-driven factory**: 实现 Phase 3，使 `main.rs` 支持多交易所切换
3. **CLI 参数支持**: 添加 `--exchange` 和 `--strategy` 参数
4. **Dynamic Makefile**: 实现 `make run EXCHANGE=backpack STRATEGY=adaptive_mm`
5. **跨交易所套利**: 利用统一的 `Exchange` trait 实现 Lighter ↔ Backpack 套利策略

## 测试建议

```bash
# 验证 Lighter DEX 仍然正常工作
make adaptive-up
# 观察日志，确认订单执行正常
make adaptive-logs
# 停止
make adaptive-down

# 验证 Backpack Gateway（需要 Backpack 账户）
# TODO: 创建 examples/backpack_mm.rs 测试 BackpackGateway

# 验证 EdgeX Gateway（需要 EdgeX 账户 + L2 签名实现）
# TODO: 完成 L2 签名后测试
```

---

**结论**: Phase 1 & 2 成功完成，代码库更加模块化和可维护。Phase 3 & 4 留待未来实施，当前重构已为 config-driven 架构奠定基础。
