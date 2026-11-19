# Sprint 5 诊断与升级报告：被动 Maker 平仓闭环、`VMM-07` 单位/缺口归一化、端到端遥测告警与零凭证 `Shadow` 模式（2026-11-18）

> **合规与执行边界**：本报告及对应 Sprint 5 改动全部遵循 `AGENTS.md` 规范：新增开关默认关闭（零破坏既有回放基线与 `W07–W16` 冻结配置）；严格避让 Codex 在锁中的 `src/strategy/cross_sectional.rs` 与采集/冻结目录；三赛道维持 **HOLD**（未触发实盘下单、撤单、部署或服务重启）。

---

## 一、 核心病灶定位（基于 Codex `aleph` 14 窗 `140h` Sprint 3 实证数据）

根据 `.agent-state/research/LATENCY_AND_CASH_2026_11_18.md` 中 Codex 完成的跨月 14 窗（首批 6 窗 + 跨月 8 窗）实证归因，Sprint 3 首轮出现了两项典型**执行微结构病灶**：

1. **`at_touch` 反转单挂单有效，但 `~95%` 仓位拖到 60s 被迫 Taker 吃单止损**：
   - `reversal_ctrl` 下反转单深挂导致 `1006` 笔订单零成交；启用 `entry_touch_offset_bps`（`at_touch`）后，14 窗被动成交提升至 `1,257` 次 / `$51,829.77`，且挂单后 1 秒 markout 损失收窄至仅 **`-$1.62`（`-0.31 bps`）**。
   - 但由于 `local_signal.rs` 只将**开仓入场侧**拉贴盘口，减仓平仓侧仍按原 A–S / `min_profit_bps` 深挂，导致大量已成交仓位无法被动平仓，最终在 `position_timeout_secs = 60s` 后触发 `execute_timeout_flatten`（20 bps `OrderType::Ioc` Taker），白白损失双边吃单费与滑点（12 窗压力净现金从 `-$6.06` 跌至 `-$12.46`）。
2. **`breaching_any` 误伤贴盘挂单导致假撤单风暴（`4,758` vs `986` 笔受理）**：
   - `inventory_neutral_mm.rs` 的 `execute_quoting_cycle` 用含 `min_profit_bps > 0` 的 `MakerPriceBounds` 校验已有挂单；而 `local_participation` 在之后才将挂单价改贴 BBO，结果上一拍刚放上的贴盘买/卖单下一拍立刻触发 `breaching_any` 被 `"maker edge below floor"` 强制全撤，引发高频撤挂空转与队列丢失。
3. **非反转做市（MM）也存在“死等 60s IOC”与毒性防抖缺失**：
   - `asymmetric_entry_ratchet` 虽在 14 窗将净亏损从 `-$24.04` 压至 `-$15.61`，但缺乏**撤单后重挂冷却**（容易在连续下跌中每拍追挂下跌档位）与**超时前平滑贴盘被动平仓**机制；同时 `LocalToxicityConfig` 缺少滞回施密特防抖与保持冷却，毒性边界来回抖动。

---

## 二、 Sprint 5 工程交付总览（`S5-1`–`S5-5`）

| 编号 | 模块与源码 | 核心机制与关键改动 |
| --- | --- | --- |
| **`S5-1`** | [local_signal.rs](../src/strategy/inventory_neutral_mm/replay/recorded/local_signal.rs)<br>[inventory_neutral_mm.rs](../src/strategy/inventory_neutral_mm.rs) | 1. **`LocalReversalFilter` 被动 Maker 止盈/贴盘平仓**：新增可选 `exit_touch_offset_bps`、`exit_min_profit_bps` 与 `exit_chase_start_fraction`（配 `LocalBookContext` 传入真实 `entry_vwap` 与持仓时间占比 `age_fraction`）；前半期保留盈利目标，后半期线性衰减至 BBO 被动贴盘追平（严守 `PostOnly` 不穿盘）。<br>2. **彻底修复 `breaching_any` 假撤单风暴**：在 `execute_quoting_cycle` 检验 `breaching_any` 前，只针对 `active_touch_sides` / `passive_touch_sides` 实际启用贴盘的单侧放宽 `bounds` 至 `ask_touch - tick` / `bid_touch + tick`，保留另一侧严格边界。<br>3. 为 `is_reducing_exit_active` 自动开启 `fast_reducing` + `1 tick / 1s` 快平覆盖。 |
| **`S5-2`** | [config.rs](../src/config.rs)<br>[components.rs](../src/strategy/inventory_neutral_mm/components.rs)<br>[execution.rs](../src/strategy/inventory_neutral_mm/execution.rs)<br>[inventory_neutral_mm.rs](../src/strategy/inventory_neutral_mm.rs) | 1. **毒性撤单施密特触发与冷却**：`LocalToxicityConfig` 新增 `withdraw_cooldown_ms` 与 `withdraw_clear_ratio`，触发增仓侧硬撤单后保持锁存直到风险指标跌破回滞下限且冷却期满（永不阻断减仓侧）。<br>2. **不利回撤强撤后冷却**：`SideExecutionPlan` 暴露 `had_adverse_retreat`（且紧急撤单记为 `risk_reducing = true` 禁止被 defer），配置 `adverse_retreat_reentry_cooldown_ms` 后 `0ms` 撤单并冻结同侧重挂，杜绝跌势接飞刀。<br>3. **超时前被动贴盘平仓**：`QuoteHysteresisConfig.passive_touch_flatten_age_fraction` 在临近 `position_timeout_secs` 阶段平滑把减仓报价推向 BBO 被动盘口并放宽对应最小点差阈值至 `1 tick`。 |
| **`S5-3`** | [config.rs](../src/config.rs)<br>[inventory_neutral_mm.rs](../src/strategy/inventory_neutral_mm.rs)<br>[tests.rs](../src/strategy/inventory_neutral_mm/tests.rs) | **`VMM-07` 跨品种量纲与缺口语义可控化**：新增 `as_inventory_notional_usd`（按 `q * pricing_mid / unit_usd` 归一化 A–S 库存偏移项，使 BTC `$100k` 与 XRP `$0.5` 同 `$500` 名义敞口得到完全一致的 bps 偏移）、`vol_gap_reset_secs`（`TimeScaledVolatility::update_with_gap_reset` 在长断线后清空陈旧方差重新预热）、`normalize_cold_start_sigma_by_poll`（冷启动按 `poll_interval_secs^{-1/2}` 缩放）；补齐可控单测。 |
| **`S5-4`** | [telemetry_sink.rs](../src/telemetry_sink.rs)<br>[defense.rs](../src/strategy/inventory_neutral_mm/defense.rs)<br>[telemetry_store.py](../deploy/oasis/telemetry_store.py)<br>[alerts.py](../deploy/oasis/alerts.py)<br>[types.ts](../dashboard/src/types.ts) | **打通 `OPT-05` 额度 & `OPT-07` 防御状态机全链路观测**：`StrategySnapshot` 增可选 `volume_quota_remaining` 与 `defense_regime`；`telemetry_store.py` 入库迁移并过滤合法状态（未知保持 `NULL`）；`alerts.py` 新增额度低位告警（`<=250` WARNING / `<=50` CRITICAL）与防御状态机 `ReduceOnly` / `Halt` 告警；同步更新 `dashboard/src/types.ts` 与合约夹具。 |
| **`S5-5`** | [exchange.rs](../src/exchange.rs)<br>[lighter_inventory_mm.rs](../src/bin/lighter_inventory_mm.rs)<br>[Makefile](../Makefile)<br>[LIVE_TRADING.md](../docs/LIVE_TRADING.md) | **Phase 3 零凭证只读实盘影子模式（`MODE=shadow`）**：`ShadowExchange` 不含任何 HTTP/WS/私钥签名组件，预检 `set_reference_price_band` 并在本地合成 V2 订单事件驱动真实 `OrderTracker`；`--shadow CONFIG` 可直接接入实时 SHM BBO/Depth（无私有账户 SHM 时自动创建并心跳保活 `$50,000` 合成平仓只读页），支撑实盘前 24–72h 零风险影子运行。 |

---

## 三、 给 Astra 6.2（Codex）的 Sprint 5 离线实证消融建议（`aleph`）

建议在保持原有冻结对照逐字节一致的前提下，新增以下两套组合消融对照：

1. **量化反转赛道（解决 95% Taker 超时与假撤单）**：
   - 保持 `entry_touch_offset_bps = Some(0.0)`；
   - 新增：`exit_touch_offset_bps = Some(0.0)`、`exit_min_profit_bps = Some(2.0)`、`exit_chase_start_fraction = Some(0.4)`；
   - 观测核心指标：`accepted_orders` 是否从 `4,758` 回落至正常区间、`taker_turnover / maker_turnover` 是否从 `~95%` 骤降、以及 14 窗总净现金是否由负转正。
2. **被动做市赛道（解决连续跌势重挂损耗与 60s 吃单强平）**：
   - 在 `ratchet`（`asymmetric_entry_ratchet = true`）基础上叠加：`adverse_retreat_reentry_cooldown_ms = 1500`、`passive_touch_flatten_age_fraction = Some(0.65)`、`LocalToxicityConfig` 的 `withdraw_cooldown_ms = 1500` + `withdraw_clear_ratio = Some(0.6)`；
   - 跨品种组补充对比开启 `as_inventory_notional_usd = Some(500.0)` + `vol_gap_reset_secs = Some(60)` 后的跨币净现金与尾部最大回撤。
