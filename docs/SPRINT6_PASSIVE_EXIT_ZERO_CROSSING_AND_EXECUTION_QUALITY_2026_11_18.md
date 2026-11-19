# Sprint 6 工程报告：被动退出 `reduce_only` 穿零根治与三赛道执行质量升级（2026-11-18）

> **合规与执行边界**：本报告及 Sprint 6 全部代码改动严格遵守 `AGENTS.md` 规范——新增策略开关默认 `None`/`false`（零破坏既有控制对照与 `W07–W16` 冻结配置）；未改动 `oasis-tokyo`、`aleph`、`.agent-state/forward_ops_20261118/*` 及 Codex 研究脚本；三赛道维持 **HOLD**（未下达任何实盘/模拟下单、撤单、部署或服务重启，未 `git add`/`commit`/`push`）。

---

## 一、 核心病灶定位（承接 Codex Sprint 5 `238` 份同窗实证与穿零核查）

根据 Codex 于 `17:59 CST` 更新的 `.agent-state/research/LATENCY_AND_CASH_2026_11_18.md` 实证汇总：

1. **Quant Reversal 贴盘退出穿零乒乓效应（`P0` 根治，[`AGENT_HANDOFF.md`](../AGENT_HANDOFF.md) 合作交接任务）**：
   - 基线 `entry_touch_offset_bps` 下，14 窗被动成交 `1,332` 笔 / `$55,995.89`，跨零新开反向敞口仅 `15` 笔 / `$1,032.20`；
   - 开启 Sprint 5 贴盘退出（`exit_touch_offset_bps`）后，14 窗被动成交暴涨至 `82,366` 笔 / `$3,810,064.38`，净现金由 `-$7.07` 恶化至 `-$211.20`（压力 `-$212.11`，加权利润门槛版亦为 `71,502` 笔 / `$3,338,200.63` / `-$186.70`）；
   - 按撮合前模拟场所真实仓位核对，其中 **`23,956` 笔被动成交直接穿过零仓位**（覆盖 `13/14` 市场窗），仅计跨零新开反向敞口名义额即达 **`$1,395,445.63`**（平均 1 秒 markout 亏损 `-$103.61` + 额外 0.25 bp 执行开销 `-$95.48`）。
   - **根因诊断**：(a) `local_signal` 在持多 `0.5` 时按减仓方向把普通 `PostOnly` 网格拉贴 BBO，但网格总额由原 `target_sz`（如 `1.0`）和多档递减决定且 `reduce_only` 恒为 `false`；(b) `InventoryNeutralMM` 未将已挂出的 `PendingCreate | Open | PartiallyFilled | PendingCancel` 同侧暴露计入剩余可平额度，在挂单尚未全撤、或账户 `account_stats` 与 `OrderTracker` 因 `100ms` 定时刷新出现短暂偏差（如 `[0.0, +0.5]` 或 `[+0.5, 0.0]`）、或迟到撤单成交（`CancelledOrder` tombstone）并发到达时持续反手超卖穿零；(c) 模拟撮合 `SimVenue` / `recorded::Matching` 尚未对静息 `PostOnly` 订单强制执行交易所级 `reduce_only` 裁剪与零仓自动撤单。
2. **被动做市（MM）重挂冷却有效，但触发安全深档不必要撤单与补单风暴（`S6-2`）**：
   - Codex 实证表明，`adverse_retreat_reentry_cooldown_ms = 1500` 将 `single_ratchet` 14 窗净现金从 **`-$17.11` 收敛至 `-$6.52`**（压力 `-$19.83 -> -$8.22`，最大回撤 `8.89 -> 3.18`，`6/14` 窗严格改善）；但总撤单请求从 `83,632` 升至 `98,449`（原因：退守冷却期内仅清空 `to_place`，而安全深档仍可能因常规周期重挂被撤；冷却结束一拍又一次性重放整组多档新单）。
3. **东京双腿套利事后平仓路径分化显著（`S6-3`）**：
   - `0 bp` 控制组下 `passive_only` 循环合计 **`+$0.430`（11 循环）**，而被迫开仓对冲的循环亏 **`-$1.864`（30 循环）**、强平 IOC 亏 **`-$0.592`（10 循环）**；亟需在**开仓提交前**按因果可见的 L1 盘口价差、对冲触价深度与盘口失衡筛除劣质入场时刻，且绝不阻断平仓与强排。
4. **四币截面动量覆盖率与严格全部到位门禁冲突（`S6-4`）**：
   - 7 个存档日 14 个固定时窗中，严格要求 4 币齐备导致 `10` 窗不可用（仅 `7 月 1 日 SOL/BTC +$0.8231` 与 `9 月 1 日 ETH/BTC +$0.0782` 通过覆盖校验），而多空各 1 腿的策略在 `3/4` 币行情洁净且缺席币种空仓时仍可在洁净子集上保持完全美元中性交易。

---

## 二、 Sprint 6 工程交付清单（`S6-1`–`S6-4`）

### 2.1 `S6-1`（`P0`）：四层纵深 `reduce_only` 被动平仓零穿零不变量

| 层级 | 模块与源码 | 核心不变量与实现细节 |
| --- | --- | --- |
| **L1 · 目标限额层** | [components.rs](../src/strategy/inventory_neutral_mm/components.rs)<br>[local_signal.rs](../src/strategy/inventory_neutral_mm/replay/recorded/local_signal.rs)<br>[inventory_neutral_mm.rs](../src/strategy/inventory_neutral_mm.rs) | 1. 新增 **`closable_reducing_sizes(positions: [f64; 2], step_size: f64) -> [f64; 2]`**：仅在 `exit_touch_offset_bps`（Quant 贴盘退出）或 `passive_touch_flatten_age_fraction`（MM 超时前贴盘退出）启用时生效。<br>2. 正确处理 `recorded.rs` 中 `account_stats` `100ms` 定时滞后与 `OrderTracker` 即时成交回报之间的瞬态差异：`[0.5, 0.5] -> 0.5`、`[-0.5, -0.5] -> 0.5`、`[0.0, 0.5] -> 0.5`（刚成交尚未等来账户定时帧不延误退出）、`[0.5, 0.0] -> 0.0`（确认已归零后立即禁止继续贴盘开空）、`[1.0, 0.4] -> 0.4`（取最小一致减仓上限）、`[0.5, -0.5] -> [0.0, 0.0]`（正负冲突保守锁零）。 |
| **L2 · 多档网格与在途敞口扣减层** | [components.rs](../src/strategy/inventory_neutral_mm/components.rs)<br>[execution.rs](../src/strategy/inventory_neutral_mm/execution.rs)<br>[inventory_neutral_mm.rs](../src/strategy/inventory_neutral_mm.rs) | 1. 新增 **`build_reducing_grid_plan`**：强制每档 `reduce_only = true`，累计多档总挂单量严格 `<= closable_abs`；当减去本档后剩余尾仓满足 `0 < remaining - level_sz < min_quotable_size`（无法独立合规挂单的碎尾），直接将该碎尾并入本档一并挂出，避免留下无法被减仓的尘埃仓位。<br>2. 新增 **`build_side_execution_plan_full`**：在存在 `reducing_cap` 时，统计本侧全部 `lifecycle.has_pending_exposure()`（`PendingCreate | Open | PartiallyFilled | PendingCancel`）订单暴露 `exposed`，将剩余可挂上限 `avail = floor(cap_abs - exposed)`；并在 `reconcile_side_plan_with_options` 中绕过队列保护直接 **`0s` 强撤**一切非 `reduce_only` 或尺寸超限订单，杜绝旧入场单残留穿零。 |
| **L3 · 执行入口边界强校验** | [order_tracker.rs](../src/order_tracker.rs)<br>[exchange.rs](../src/exchange.rs)<br>[trading.rs](../src/exchanges/lighter/trading.rs) | 1. `TrackedOrder` 与 `OrderTracker::start_tracking_order` 记录 `pub reduce_only: bool`（兼容 `start_tracking(..., false)`）。<br>2. **`LighterTrading`（`OPT-04`）**：`ensure_reduce_only_valid_with_pending` 在 `get_nonce` 前与批量签名中同时计入 `OrderTracker` 同侧 `has_pending_exposure()`（含 `PendingCancel` 订单），校验 `pending_same_side + size <= verified_position.abs()`，并用 `submit_prepared_with_flags` 将 `reduce_only` 透传至 tracker 登记。<br>3. **`ShadowExchange`**：新增 `check_reduce_only_total` 对单笔与 `execute_batch` 同向 `reduce_only` 总量校验 `pending_same_side + total_size <= confirmed.abs()` 且要求 `!needs_reconciliation()`。 |
| **L4 · 模拟撮合与 `CancelledOrder` 墓碑硬防线** | [replay.rs](../src/strategy/inventory_neutral_mm/replay.rs)<br>[recorded.rs](../src/strategy/inventory_neutral_mm/replay/recorded.rs) | 1. `SimVenue` `Resting` 记录 `reduce_only`，新增 `reducible_size(is_ask)`。<br>2. `SimVenue::{execute_batch, settle, cancel}` 与 `recorded::Matching::{admit, on_trade, execute}` 在接单、`fill_before_cancel` 竞态及实盘成交推送前（含撤单在途墓碑 `CancelledOrder`）统一对 `reduce_only` 订单按 `reducible_size` 裁剪；零可平量拒绝入簿、成交归零后立即自动撤除 `reduce_only` 余量，物理上彻底杜绝模拟回放穿零。 |

### 2.2 `S6-2`–`S6-4` 策略执行质量升级与 Codex 截面 freshness 协同审阅

| 编号 | 模块与源码 | 核心改动与因果边界 |
| --- | --- | --- |
| **`S6-2`** | [config.rs](../src/config.rs)<br>[execution.rs](../src/strategy/inventory_neutral_mm/execution.rs)<br>[inventory_neutral_mm.rs](../src/strategy/inventory_neutral_mm.rs) | **`QuoteHysteresisConfig` 冷却防撤与限速补单（默认全关）**：<br>1. `cooldown_hold_safe_levels: bool`：处于 `adverse_retreat_reentry_cooldown_ms` 冷却期内的增仓侧，只允许 `age = 0` 紧急风险撤单与价带越界撤单，**抑制常规重挂撤单（`normal_cancels`）**，保留安全深档现有队列与降低无谓撤单消耗。<br>2. `max_initial_placements_per_cycle: Option<usize>`（约束范围 `1..=5`）：限制单周期常规开仓侧最多下发的新档位数，冷却恢复后阶梯式重建多层盘口，避免集中脉冲扫空/被逆向选择。 |
| **`S6-3`** | [entry_book.rs](../src/strategy/relative_value/paper/maker_first/entry_book.rs)<br>[maker_first.rs](../src/strategy/relative_value/paper/maker_first.rs) | **东京 `maker_first` 因果 L1 盘口质量开仓门控（`EntryBookFilter`，默认 `None`）**：<br>在 `Replay::quote` 的 `!closing` 开仓路径检查因果同步后的双方 L1 盘口：`min_maker_spread_bps`（保证 Maker 静态毛点差空间）、`max_hedge_spread_bps`（避开对冲腿点差扩宽期）、`min_hedge_touch_notional_mult`（按 Maker 开仓 `side` 取对冲腿 `opposite(side)[0]` 触价深度与 `quote_notional` 倍数校验，避开被迫穿透深档/强平 IOC）及 `max_adverse_book_imbalance`（限制不利 L1 挂单失衡）；输出独立 `EntryBookReport`，**严格旁路所有平仓、对冲与 terminal drain**。 |
| **`S6-4`** | [cross_sectional.rs](../src/strategy/cross_sectional.rs) | **1. 完成 Codex `cross_sectional.rs`（`6c1e7aab14069cb5`）只读复审并签收通过**：确认 `has_fresh_book`（`now_ms - latest_source_ms <= max_age_ms`）、`causal_return_bps`（`s.source_ms >= oldest_acceptable_ms && s.receipt_ms - s.source_ms <= max_staleness_ms`）、单调决策时钟校验及 `last_targets` 缓存新鲜度检查 100% 闭环。<br>2. **新增 `min_active_symbols: Option<usize>`（`2 * legs_per_side <= min_n <= symbols.len()`）**：在严格保留 **“任一当前持仓腿（`|usd| > 1.0`）缺少新鲜盘口或有效历史锚点立即全盘 fail-closed（`None`）”** 及 **非空缓存目标腿全量新鲜校验** 的前提下，允许 `{BTC, ETH, SOL, XRP}` 等组合在空仓缺席 `<= N - min_n` 个币种时，在其余洁净子集（如 3-of-4）上维持完全美元中性轮动。 |

---

## 三、 给 Astra 6.2（Codex）的 `aleph` Sprint 6 验证矩阵建议

建议保持既有 `sprint5_*` 历史账本与 `W07–W16` 冻结配置完全不变，在独立实验目录按同引擎对比以下 4 组消融：

1. **Quant Reversal 零穿零复验（14 窗 `140h`，复测 `sprint5_cash_v1` 失败根因）**：
   - 基线：`entry_touch_offset_bps = Some(0.0)`；
   - Sprint 6 对照组：`entry_touch_offset_bps = Some(0.0)` + `exit_touch_offset_bps = Some(0.0)`（以及 `+ exit_min_profit_bps = Some(2.0), exit_chase_start_fraction = Some(0.4)`）；
   - **关键验收指标**：模拟撮合 `zero_crossing_fills` 须由 **`23,956 -> 0`**、跨零相反方向新开名义额由 **`$1,395,445.63 -> $0`**、总挂单周转回落至正常区间并检验 1 秒 markout 与净现金表现。
2. **MM 冷却撤单抑制消融（14 窗 `140h`）**：
   - 在最佳单边基线 `asymmetric_entry_ratchet + adverse_retreat_reentry_cooldown_ms = 1500`（原 `-$6.52`、`98,449` 次撤单）上叠加 `cooldown_hold_safe_levels = true` 及 `max_initial_placements_per_cycle = Some(1)`，核对 `cancel_requests` 回落幅度、单位每百万成本（`$218.98/M` -> 是否继续压降）及净现金变化；同步对比开启 `passive_touch_flatten_age_fraction` 时在 L1–L4 硬 `reduce_only` 保护下是否消除余仓过平干扰。
3. **东京 Core/RH 因果入场门控消融（`W01–W05` `10` 窗 + `W06`）**：
   - 为 `MakerFirstConfig` 增加 `entry_book_filter` 网格（例如 `min_maker_spread_bps = Some("2.0")`, `min_hedge_touch_notional_mult = Some("1.5")`, `max_hedge_spread_bps = Some("3.0")`），观察是否显著提高 `passive_only_cycles`（`+$0.430`）占比并压低被迫对冲（`-$1.864`）与强平 IOC（`-$0.592`）循环比例。
4. **四币截面 3-of-4 洁净子集扩展消融（7 个采样日 + `2026-01..04` 扩展期）**：
   - 保持 `min_active_symbols: None` 为严格全集对照，独立记录 `min_active_symbols: Some(3)` 在 `May 1 B` 等 14 个固定时窗上的额外可结算覆盖数、美元中性对及费后净现金/尾部回撤（两种口径分栏汇报，绝不混用）。

---

## 四、 Mac 验收范围说明

本次交付需在最后一次代码与文档修改后于 Mac 完成全量工具链验收，确认通过：
- `make fmt-check`、`make check`、`cargo clippy --locked --all-targets --features research-replay -- -D warnings`、`cargo check --locked`
- `make test`（Rust 全量单测/集成测 + `research-replay` 特性测试 + Go 全 5 测试包 `-race` + Python `scripts`）
- `make test-python`（`deploy/oasis` 监控/存储/告警套件）
- `make dashboard-build`（React/Vite 看板测试与构建）
- `make build`、`make trade MODE=check CONFIG=config.example.toml`、`git diff --check`
- `AGENTS.md` 敏感信息排查（确认 `git diff --cached` 为空，文档及 handoff 中无密钥、IP、邮箱或个人账户标识）。
