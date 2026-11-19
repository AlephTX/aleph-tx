# Sprint 7 工程报告：撤单原因/队列时长归因、四币反转与波动率定仓、做市预算限额及东京对冲边际门控（2026-11-18）

> **合规与执行边界**：本报告及 Sprint 7 全部代码改动严格遵守 `AGENTS.md` 规范——新增策略开关默认 `false` / `None` / `Momentum`（报告新增可选诊断字段使用 `skip_serializing_if`），保持既有对照组与 `W07–W16` 冻结配置输出逐字节一致；未改动 `oasis-tokyo`、`aleph`、`.agent-state/forward_ops_20261118/*` 及 Codex 研究脚本；三赛道维持 **HOLD**（未下达任何实盘/模拟下单、撤单、部署或服务重启，未 `git add`/`commit`/`push`）。

---

## 一、 承接 Codex `19:31 CST` 复验交接：签名验收与根因定位

根据 Codex 于 `2026-11-18 19:31 CST` 更新的 `.agent-state/research/LONG_HISTORY_AND_SIZING_2026_11_18.md` 与 [`AGENT_HANDOFF.md`](../AGENT_HANDOFF.md)：

1. **Codex `cross_sectional.rs` 缓存锚点修复签名验收通过**：
   - 只读复核 SHA-256 前缀 `0bb23dd2c797b493`（[cross_sectional.rs](../src/strategy/cross_sectional.rs)）：确认 `ready_return` 将 `has_fresh_book` 与 `causal_return_bps` 绑定；缓存分支在 `now_ms - prev < rebalance_interval_ms` 时统计就绪币数 `ready_count >= min_required`、对任一持仓腿（`held_sign != 0`）缺证立即 fail-closed 返回 `None`，并在任一**空仓缓存目标腿（`target_notional_usd != 0.0`）**失去 24h 历史锚点时置 `active_targets_ready = false` 回落至洁净子集重新排序，`cached_targets_require_ready_universe_and_active_leg_anchors` 全三分支通过。
2. **揭示并根治 `max_initial_placements_per_cycle = Some(1)` 的 `+26,330` 撤单膨胀（`98,449 -> 124,779`）**：
   - 从 `sprint6_matrix_v1/scores.json` 可确认：单独开启 `cooldown_hold_safe_levels` 仅增加 `+16` 撤单（`98,449 -> 98,465`），真正触发 `+26,330` 撤单与 `+26,327` 挂单（`98,531 -> 124,858`）的是在 2 档网格配置（`grid_levels = 2`, `grid_spacing_bps = 2.0`, `entry_requote_threshold_bps = 4.0`, `adverse_retreat_threshold_bps = 1.0`）下直接启用无状态 `max_initial_placements_per_cycle = Some(1)`。
   - **三重合力机制解剖**：
     1. **4 秒 `MICRO_REFRESH_COOLDOWN` 饥饿与对侧防撤绕过**：一侧从 `1.5s` 冷却恢复后先只挂 `L0`，使 `resting_orders` 达到 `3`（`>= MICRO_REFRESH_MIN_RESTING_ORDERS`）。下一 `300ms` 周期中，本该紧跟挂出的 `L1` 被 `should_defer_micro_refresh`（`total_places == 1 && resting_orders >= 3`）阻塞 `4.0s`；与此同时，这 `4.0s` 内该侧 `plan.to_place` 始终非空（含 `[L1]`），致使对侧任何原本应被 `should_defer_one_sided_requote` / `should_defer_cancel_only_refresh` / `should_defer_micro_refresh` 拦截的 1 笔常规重挂因 `!other_has_place == false` 与 `one_sided_activity == false` 被瞬间放行，引发高频短队列撤换（平均撤单间隔由 `5.12s` 缩短至 `4.04s`）。
     2. **延迟 `L1` 价格倒挂触发级联 `AdverseRetreat`**：由于 `entry_requote_threshold_bps (4.0) > grid_spacing_bps (2.0)`，若 `L1` 被饥饿 `4s` 期间盘口同向漂移 `+2.5..3.9 bps`，原 `L0` 仍落在容忍带内未更新，而延迟补挂的 `L1` 按当前更激进的 `desired[1]` 生成，反而挂到**高于原 `L0` 的价格上**，随后微幅回撤 `> 1 bp` 即触发 `AdverseRetreat` 连续扫单与亏损（`-$6.523 -> -$10.766`）。
     3. **存量换单拆批乒乓**：在已有部分挂单换单时截断 `to_place`，将原本一拍同步完成的两档换单拆成交错单拍循环。
3. **直面三赛道新发现的规模化约束与盲区**：
   - **四币截面**：Codex 245 窗长周期回测证明 `Momentum` 方向持续亏损，而**反转方向（做多 24h 领跌币、做空领涨币）**固定金额 245 窗净额 `+9.072` 美元（压力 `+4.206` 美元，VWAP `+4.266` 美元），且按传播波动率反比定仓可把共同 209 窗最大压力回撤由 `4.902` 降至 `1.708` 美元——需在主策略 [cross_sectional.rs](../src/strategy/cross_sectional.rs) 原生支持 `Reversal` 与因果 `vol_target_pnl_usd`。
   - **MM 与 Quant 预算失真及基线残余穿零**：(a) `utilization_floor_base_order_size` 按 `usable_balance * max_leverage * target_resting_margin_utilization` 在 `$50k` 权益下将名义下单额强行拉高至 `~$330–$845`（`35,692/35,696` 笔超配 `base_order_notional_usd`），直接导致深度预算实验失真；(b) 普通非 `exit_touch` 基线在仓位归零瞬间仍有 `15` 笔普通减仓网格穿零；(c) Quant 贴盘退出在极端冲击后立即重入使 Maker 周转增至 `$152.1k`（`exit_touch` 1s markout `-$5.098`）。
   - **东京套利强制对冲盲区**：[maker_first.rs](../src/strategy/relative_value/paper/maker_first.rs) 在 `PairHedge + passive_offset` 且未配置 `close_probability` 时，`opening_edge` 直接返回 `passive_cash` 而完全忽略开仓即触发的对冲价差与双边吃单成本，致使无法在开仓前过滤掉占控制组 `30/51` 循环且亏损 `-$1.864` 的被迫开仓对冲时刻。

---

## 二、 Sprint 7 工程交付清单（`S7-1`–`S7-4`）

| 编号 | 模块与源码 | 核心能力与严格不变量（默认全关 / 100% 兼容基线） |
| --- | --- | --- |
| **`S7-1`** | [cross_sectional.rs](../src/strategy/cross_sectional.rs) | **1. 四币反转方向 `CrossSectionalDirection::{Momentum, Reversal}`（默认 `Momentum`）**：`Reversal` 按 `-return_bps` 排序（领跌做多、领涨做空），严格保留 `min_spread_return_bps`、`rank_hysteresis_bps` 在位滞后保护及 `max_adverse_funding_bps` veto，并继续在 `TargetLeg` 输出真实原始资产收益 `return_bps`。<br>**2. Moreira–Muir 因果传播波动率定仓 `vol_target_pnl_usd` + `min_leg_notional_usd`**：新增 `causal_pair_sigma_rebalance`，仅利用已闭环因果环形槽 `[now_ms - lookback_ms, now_ms]` 对多空配对收益方差按时间加权归算至单次换仓周期 $\sigma_{\text{rebalance}}$，按 `pair_usd = (target_usd / sigma).min(leg_notional_usd)` 等额赋值 `+pair_usd / -pair_usd` 保持严格美元中性；低于 `min_leg_notional_usd` 自动观望归 `0.0`，持仓腿缺失足够波动率样本直接 fail-closed。 |
| **`S7-2a`** | [components.rs](../src/strategy/inventory_neutral_mm/components.rs)<br>[execution.rs](../src/strategy/inventory_neutral_mm/execution.rs)<br>[inventory_neutral_mm.rs](../src/strategy/inventory_neutral_mm.rs)<br>[recorded.rs](../src/strategy/inventory_neutral_mm/replay/recorded.rs) | **1. 正交零干扰撤单与排队时长归因（`diagnose_cancel_attribution: bool`）**：通过 `CancelReason`、`CancelReasonScope` 与 `BatchCancelReasons` 完整分类 `edge_floor_cancel_all`、`risk_or_spread_cancel_all`、`timeout_or_shutdown_cancel_all`、`side_withdrawn`、`bound_breach`、`adverse_retreat`、`reducing_cap_or_non_ro`、`normal_price_requote`、`normal_size_or_grid_requote` 及撮合端 `reduce_only_auto_cancels`；使用独立的 `Queue::cancel_attr_recorded` 标志，**绝不触动 `CrossMarketAttribution` 或任何撮合/模拟行为**，并输出撤单前保住队列时长（`total_preserved_queue_ms`、`mean_preserved_queue_ms`、五分位直方图）及对比 `maker_fills` 平均排队时长。<br>**2. 三重根治 `staged_reentry_only: bool`（需配合 `max_initial_placements_per_cycle`）**：仅当某侧当前空仓（`!side_empty == false`）时生效补单上限；对纯深档补齐（`is_pure_grid_fill_in`）**强制单调价格钳制（Buy 不高于在挂最低买价减网格间距，Sell 不低于最高卖价加网格间距）**，防止 delayed `L1` 倒挂；在撤单防线前隔离 `is_pure_grid_fill_in` 避免破坏对侧 `4s` 冷却防撤，同时豁免纯补齐免受 `MICRO_REFRESH_COOLDOWN` `4s` 饥饿阻塞。 |
| **`S7-2b`** | [config.rs](../src/config.rs)<br>[components.rs](../src/strategy/inventory_neutral_mm/components.rs)<br>[inventory_neutral_mm.rs](../src/strategy/inventory_neutral_mm.rs) | **1. `cap_utilization_floor_to_base_notional: bool`**：关闭 `target_resting_margin_utilization` 对小额预算的下限抬升覆盖，保证名义受理金额忠于 `base_order_notional_usd` / `base_order_size`。<br>**2. `max_top_book_depth_fraction: Option<f64>`**：使用当帧新鲜本地 L1 触价名义深度 `[bid_touch * bid_size, ask_touch * ask_size]` 作为因果上限缩放增仓单尺寸，**且绝不限制减仓单**。<br>**3. `QuoteHysteresisConfig::enforce_reduce_only_on_reducing_side: bool`**：为任意确认可平仓减仓网格全量启用 `reduce_only = true` 与在途总暴露上限（含 `PendingCancel` 与瞬时撤单），同步清除普通非 `exit_touch` 基线中的 `15` 笔穿零。 |
| **`S7-3`** | [local_signal.rs](../src/strategy/inventory_neutral_mm/replay/recorded/local_signal.rs) | **1. `post_exit_cooldown_ms: u64`**：仓位归零并清空挂单许可后锁存短暂防追杀冷却，抑制激进反转平仓后在余震尾流中立刻反手再开导致的周转倍增。<br>**2. `hold_exit_while_flow_favorable_min_imbalance: Option<f64>`**：在 `age_fraction < 0.85` 安全上限内，当近期因果成交流继续强顺着减仓有利方向运行（`Long Ask close` 时 `flow_imb >= min_imb`，`Short Bid close` 时 `flow_imb <= -min_imb`）时暂时保持 `exit_min_profit_bps` 利润护城河，衰减后仍按线性追盘并在超期前全额被动平仓。 |
| **`S7-4`** | [entry_book.rs](../src/strategy/relative_value/paper/maker_first/entry_book.rs)<br>[maker_first.rs](../src/strategy/relative_value/paper/maker_first.rs) | **扩展 `EntryBookFilter`（严格仅作用于 `!closing` 开仓路径，永不阻塞平仓/对冲/drain）**：<br>**1. `min_hedged_opening_edge_bps: Option<Decimal>`**：因果计算开仓瞬间经对冲腿触价即时对冲净边际（Buy 为 `(hedge_bid - maker_bid) / hedge_mid * BPS - maker_fee_bps - hedge_taker_fee_bps`，Sell 反号），直接在入场前淘汰注定产生高昂对冲滑点/价差亏损的 `PairHedge + passive_offset` 挂单机会（记录于 `blocked_hedged_opening_edge`）。<br>**2. `max_maker_queue_notional_mult: Option<Decimal>`**：限制 Maker 挂单前方可见 L1 排队名义额相对 `quote_notional` 的最大倍数（记录于 `blocked_maker_queue_depth`），避开只有在逆向穿仓sweep中才成交的超长队列开仓。 |

---

## 三、 给 Astra 6.2（Codex）的 `aleph` Sprint 7 验证矩阵建议

建议在冻结对照组与 `W07–W16` 原口径完全独立的新目录中按同引擎运行以下 5 组消融：

1. **MM `+26,330` 撤单法医归因（2 组 × 14 窗，无需改参即刻复核）**：
   - 将 `diagnose_cancel_attribution = true` 加入现有 Sprint 6 `reentry`（`98,449`）与 `limit`（`124,779`）14 窗配置，直接导出按 `CancelReason` 拆分的撤单数与 `mean_preserved_queue_ms` 直方图，并叠加对比 `staged_reentry_only = true` 是否将撤单、成交成本及 `max_drawdown` 同步压回/优于 `reentry`（兼测 `enforce_reduce_only_on_reducing_side = true` 是否消除基线 `15` 笔普通单穿零）。
2. **MM 真实预算对齐消融（`S7-2b` 14 窗）**：
   - 在 `cooldown` / `cooldown + passive_exit` 与深度预算配置上启用 `cap_utilization_floor_to_base_notional = true`（及 `max_top_book_depth_fraction = Some(0.01)` / `Some(0.02)`），核对实际受理订单金额分布与 `$500` / `$250` / 深度预算的一一对应及单位成本变化。
3. **Quant 反转消融（`S7-3` 14 窗）**：
   - 在 `exit_vwap_decay` 与 `exit_touch` 上独立消融 `post_exit_cooldown_ms ∈ {1500, 3000}` 与 `hold_exit_while_flow_favorable_min_imbalance = Some(0.5)`，对比 `maker_turnover`、`taker_turnover` 与 1s markout 收敛幅度。
4. **东京套利带符号开仓对冲边际筛选（`S7-4` `W01–W05` 10 窗 + `W06`）**：
   - 保持对冲盘口与资金流条件不变，验证 `EntryBookFilter::{min_hedged_opening_edge_bps, max_maker_queue_notional_mult}` 网格（如 `min_hedged_opening_edge_bps ∈ {"-2.0", "-1.0", "0.0"}`、`max_maker_queue_notional_mult ∈ {"5.0", "10.0"}`）对被迫对冲循环数（原 `30/51`）与最终费后现金的区分度。
5. **四币反转原生入场/退出与动态定仓（`S7-1`）**：
   - 使用 [cross_sectional.rs](../src/strategy/cross_sectional.rs) 原生 `CrossSectionalDirection::Reversal` + `vol_target_pnl_usd` + `min_leg_notional_usd` 在独立历史月份及双可计价原生窗复核实盘可执行路径与风控回撤边界。

---

## 四、 Mac 全量工具链验收说明

本交付将在最终文档与代码定稿并执行 `cargo fmt` 后于 Mac 串行通过全部既定门禁：
`make fmt-check`、`make check`、`cargo clippy --locked --all-targets --features research-replay -- -D warnings`、`cargo check --locked`、`make test`（Rust 全量单测/集成测、`research-replay` 特性测、Go 5 包 `-race`、Python `scripts`）、`make test-python`（`deploy/oasis`）、`make dashboard-build`（React/Vite）、`make build`、`make trade MODE=check CONFIG=config.example.toml`、`git diff --check`、空 `git diff --cached` 及敏感信息扫描。
