# AlephTX 实盘盈利演进总案：交易所解耦架构与 Carbon × Astra 6.2 协同路线图（2026-11-18）

> **边界与合规声明**：本总案基于仓库源码审计与 `aleph` 离线因果实证数据编写，严格隔离私有凭证与远端运行时。三赛道当前维持 **HOLD**；未经用户单独授权并满足第 4 节全部晋级门禁前，严禁实盘下单、撤单、部署或重启服务。

---

## 零、 Sprint 8 实证纠偏与可行边界勘误（Superseding Errata · 2026-11-18）

> **重要更正（吸收 Claude Code 只读审计 + Codex Sprint 7 全量多月证据）**：本总案初版（`15:50 CST`）存在数处过度乐观或后验切片偏差，特此正式更正，以下口径覆盖正文对应段落，详证见 [Sprint 8 可行性边界与执行pivoting报告](SPRINT8_FEASIBILITY_BOUNDARIES_QUEUE_SENSITIVITY_AND_CARRY_PIVOT_2026_11_18.md)：
>
> 1. **关于“底层 `~85%+` 交易所解耦”的定位**：解耦（`Exchange` / `HedgeVenue` / SHM 槽位 / 通用 `DailyLossGuard` 与 `OrderTracker`）是**工程卫生条件**，不代表策略具备正期望；按当前模型与 `$500–$1,000` 资金约束，三条赛道**尚无任何一条具备已证明的正期望**。
> 2. **赛道 `1A`（四币动量）初版盈利宣称已证伪并正式转向**：
>    - 原称 `basis_hours_v2` 最后 78 窗 `+2.68 美元`，忽略了全年 1–4 月 245 窗反转 95% 区间 `[−0.047, +0.082]`/窗跨零（K 线内最差价 `-64.7`）、四币原生盘口 13 窗反转 `-2.788` vs 动量 `+1.912` 符号反转、以及决策时钟由 `13:00` 前移至 `07:00 UTC` 后七个月首从 `+4.070` 翻负至 `-1.141`。
>    - **Pivot（`S8-4`）**：`cross_sectional.rs` 不再单独使用 4 币 5m K 线排名作为独立开仓 Alpha，转向：(a) **20–50 市场 `PostOnly` 零费 `FundingCarry`**（配合零假设可变开销校准），以及 (b) **MM 单边做空/做多屏蔽器 `mm_blocked_entry_sides` + `InventoryNeutralMM::set_external_entry_side_veto`（“决定哪一侧不做”）**。
> 3. **赛道 `2`（50–200 ms 吃单竞速套利）正式退役**：
>    - 原 W05 `SPY/QQQ`（`+0.139/+0.065`）仅为单一历史窗且 QQQ 落在异常窗口内；80 组双 IOC `0` 成交；东京 Core/RH 强制 IOC 10 轮亏 `−0.592`、非强制 30 轮亏 `−1.864`、加入开仓盘口过滤仅余 `1–3` 轮仍负（RH `200/200/300 ms` 官方时延下仍负）；HL 对冲吃单费高达 `4.5 bps`。
>    - **Pivot**：停止在 50–200 ms 吃单竞速套利投入新开发；**仅保留双腿 `PostOnly` 被动循环（`passive_only` 原 11 轮 `+0.430`）与时延不敏感的跨所资金费率 Carry**。
> 4. **赛道 `3`（`$100 / $50M = $2/M = 0.02 bp`）可行边界与防过拟合冻结**：
>    - 初版引用“后 4 窗 `+1.04`”在全 8 窗实为 `−16.93`（即便 `1 ms` 网络 + `0 ms` 延迟 + `0` 费率在拥挤的 BTC/ETH 上仍亏 `−6.66`）；按每百万合格目标所 Maker 成交分母与三项结构性成本，`$2/M` 在 `$500–$1,000` 资金 + `50–200 ms` 条件下距现实差 `15×–75×`（Arcus Crypto L0 maker `$150/M`、HIP-3 Growth Mode 最低 `$30/M` 且排除主流 Crypto 并削减 90% 返佣与量级贡献、HL 返佣需 `>0.5%` 14 天 Maker 份额、Lighter RH 排除 Standard-vs-Standard、Arcus Annex C 禁止纯积分对冲循环）。
>    - **可执行修正**：(a) 用 [rescore_decomposed_overhead.py](../scripts/rescore_decomposed_overhead.py) 剥离 `0.25 bps` 拍脑袋模型税（拆分为 `$50/30d` 固定基建 + 实测滑点/费用），(b) 用 `ProbabilisticQueueConfig` 报告悲观/乐观双队列边界（拒绝单一历史过拟合），(c) 在**严格冻结参数**下仅通过 `W07–W16` 前瞻留出期验证两项高潜线索（`$1,000` + `25×` 杠杆台湾 Core BTC Mode B，明确单列 `RH 合格量=$0`；与 `screen_market_spread_to_vol.py` 筛选高价差/短波比的 Arcus RWA 0 费/长尾市场），并将刷量目标实事求是锚定在 **合格 Maker 分母 `$20–$50/M`（或相应提高资金规模）**。
>
> 5. **Sprint 9（2026-11-19）升级摘要**：按“五项优先动作 + Codex 队列/`$1k/25x` 复核”完成策略与代码全面升级（详情见 [Sprint 9 交付报告](SPRINT9_MARKOUT_CALIBRATION_PASSIVE_CARRY_AND_DSR_AUDIT_2026_11_19.md)）：
>    - **`S9-0/S9-1`（账本剥离与 `OPT-01` 闭环）**：增补 `ReplayQueueDepth::trade` sweep-through 单调性边界、HIP-3 Growth Mode/部署者费率公式（`compute_hip3_execution_fees`）与三分解摩擦账本（无 `0.25 bp` 模型税）+ 分项目 `eligible_score_per_usd_cost`；重算 Codex `$1,000 / 25×` Core BTC 12 个已结算窗（`rh_points = $0`），剥离 `-$4.4375` 模型税后变动净额仍为 `-$7.7321`（全包含 `-$16.0654`，OOS 7 月新窗 7/18+7/21 合亏 `-$21.5396`、`7/24` `trade_withheld`），实证证伪台湾单窗幻觉。
>    - **`S9-2`（Markout 驱动做市与 `(κ, γ)` 解耦校准）**：[markout.rs](../src/strategy/inventory_neutral_mm/markout.rs) 零分配维护 `[1s, 5s, 30s, 60s]` 曲线与非减仓入场分位保护 `quantile_floor_p`；[inventory_neutral_mm.rs](../src/strategy/inventory_neutral_mm.rs) 以 `as_risk_budget_bps_at_max_pos` 独立推导 `gamma_inv`，消除默认 `γ` 仅移动 `0.005 bps` 却被固定 `κ` 拉宽 `2.5 bps` 的结构性错配；配套 [calibrate_mm_markout_and_as.py](../scripts/calibrate_mm_markout_and_as.py)。
>    - **`S9-3`（纯被动循环与独立多日 Carry 子赛道）**：[maker_first.rs](../src/strategy/relative_value/paper/maker_first.rs) 新增 `PassiveOffset::passive_loop_only`（避免 `max_wait_ms` 到期主动吃单 `externalize`）与因果 `EntryMarkoutGate`；新增隔离仓位与 `10h` 连续计分单元的 [multiday_carry.rs](../src/strategy/multiday_carry.rs)（`cross_venue multiday-carry` + [run_multiday_carry_subtrack.py](../scripts/run_multiday_carry_subtrack.py)）。
>    - **`S9-4/S9-5`（DSR/PBO 回溯审计与 `W08–W16` 前瞻冻结）**：[audit_multiple_testing_overfitting.py](../scripts/audit_multiple_testing_overfitting.py) 验证 `early_rank_2026` 3 臂（206 窗）`reverse_fixed` 虽为 `+$1.9249`，但 `dsr_raw = 0.4737`、`min_trl_95 = 13,136` 窗、`CSCV PBO = 0.8286`（正式冻结 `cross_sectional.rs` 新增变体）；[freeze_sprint9_w08_holdout_manifest.py](../scripts/freeze_sprint9_w08_holdout_manifest.py) 预注册 `W08–W16` 留出验证清单（保持在采 `W07` 不受污染）。

---

## 一、 核心架构审计：底层是否与具体交易所解耦？

**结论：底层消息总线、双边风控账本、交易抽象接口与多资产截面引擎已实现交易所无关（`~85%+` 核心解耦）；新接绝大多数交易所做单腿策略或做市已可“只写适配器 + 注册表”，唯独双腿对冲控制器 `LiveInventoryHedge` 为 `W07–W16` 冻结期保守未改其内部具体类型，仍留有一处待阶段二切换的接口抽象。**

### 1.1 已有解耦底座与 Sprint 4（`S4-0`）补齐的 5 处耦合渗漏

| 系统层级 | 解耦现状与 Sprint 4 补齐内容 | 对应源码契约 |
| --- | --- | --- |
| **Go ↔ Rust SHM IPC** | **完全解耦**：64B BBO 矩阵按槽位 `exchange_id (1..11)` 隔离，128B V2 私有事件与 128B 账户快照均只承载中性方向、数量、价格与权威时间戳 | [matrix.go](../feeder/shm/matrix.go)、[events.rs](../src/types/events.rs)、[shm_depth_reader.rs](../src/shm_depth_reader.rs) |
| **执行与保证金风控接口** | **Sprint 4 完成解耦**：`Exchange` 新增 `venue_name()`、`volume_quota_snapshot()`（`OPT-05`）、`set_reference_price_band()`（`§7.E` 防呆价带）与 `evaluate_venue_margin_health()`（`MarginCheckInputs` 通用永续保证金计算，默认提供通用 Fair-Mark 评估，各交易所可覆写）；并抽出跨所 IOC 对冲 trait `HedgeVenue` | [exchange.rs](../src/exchange.rs)、[trading.rs](../src/exchanges/lighter/trading.rs) |
| **订单状态机与风控** | **完全解耦**：`OrderTracker` 仅维护本地/交易所中立订单生命周期、最坏净敞口与平仓水位；`DailyLossGuard` 按 UTC 日切基线持久化止损 | [order_tracker.rs](../src/order_tracker.rs)、[daily_loss.rs](../src/daily_loss.rs) |
| **配置与做市主核** | **Sprint 4 移除 Lighter 硬编码**：`InventoryNeutralMMConfig::validate()` 开放全部有效 `SHM exchange_id (1..11)`；`RiskSnapshot.margin_per_eth` 更名为 `margin_per_unit`；`InventoryNeutralMM` 不再直连 `lighter::risk` 也不再硬编码 `"lighter_*"` 标记来源（非 Lighter 场所自动输出 `"local_book"` / `"local_depth"` / `"local_touch_micro"`，并打通 Python 监控与 React 看板字典） | [config.rs](../src/config.rs)、[inventory_neutral_mm.rs](../src/strategy/inventory_neutral_mm.rs)、[telemetry_store.py](../deploy/oasis/telemetry_store.py)、[valuation.ts](../dashboard/src/valuation.ts) |
| **慢 Alpha 多资产引擎** | **100% 交易所无关（Sprint 4 新建 `S4-3`）**：只消费 `symbol_id`、收件因果 BBO 与资金费率，输出标准化 `OrderParams`（`PostOnly` 贴盘 / 有界 `Ioc` / 自动 `reduce_only`），可直接跑在任何 `impl Exchange` 之上 | [cross_sectional.rs](../src/strategy/cross_sectional.rs) |
| **双腿对冲器（阶段二待收尾）** | **明确透明披露**：[live_hedge.rs](../src/live_hedge.rs) 与 [session.rs](../src/bin/lighter_inventory_mm/session.rs) 当前分别针对 Hyperliquid IOC 对冲腿与 Lighter 主腿实例化（`W07–W16` 封窗期间保持零结构扰动）。Trait `HedgeVenue` 已于本轮定义就绪，待 `W07–W16` 结束后可把 `LiveInventoryHedge` 内部字段切换为 `Box<dyn HedgeVenue>` 彻底泛型化 | [live_hedge.rs](../src/live_hedge.rs)、[session.rs](../src/bin/lighter_inventory_mm/session.rs) |

### 1.2 新接入一家交易所的标准化 4 步清单（无需改核心策略/风控代码）

1. **Go 行情与私有流插件**：在 `feeder/exchanges/` 实现公开盘口（写 64B `BboMatrix[sym][EXCH_NEW]`）与私有回报/账户统计，并在 `feeder/config/` 与 `src/config.rs` 注册 `EXCH_*` 常量（注意：`ShmBboMessage` 严禁写入 `time.Now()` 伪造交易所时间）。
2. **版本化场所能力与费率表**：在 [venues.rs](../src/venues.rs) 登记该场所的真实 `maker/taker/cancel` 减速带、费用结构、额度语义与官方一手出处（`OPT-01`）。
3. **Rust 执行适配器**：在 `src/exchanges/<venue>/` 实现 `Exchange` trait（含批处理、预签名 `ensure_can_place` 与可选 `volume_quota_snapshot` / `evaluate_venue_margin_health`）；若用于对冲腿，额外实现 `HedgeVenue` trait。
4. **独立 Profile 配置与离线自检**：新建配置文件并运行 `cargo run --bin cross_venue --` / `--check` 校验符号、精度与多账户 STP 隔离后，进入历史/前瞻回放。

---

## 二、 为什么过去亏钱，以及实盘真正能赚钱的 4 条清晰路径

截至 `2026-11-18` 在 `aleph` 完成的千余场连续 10h 因果回放已经给出铁证：**任何“无条件连续对称做市”或“盲目吃单套利”都无法跨越微结构负期望天花板**（`venue_delay_v1` 即使设为 1ms 网络 + 0ms 交易所撤单延迟 + 0 费率，连续做市 8 窗仍亏 `-$6.66`；0-stake Premium `0.4/2.8bp` 录得 `-$16.20`，与免费 Standard `-$16.93` 几乎一致；80 组双吃单全部零成交）。

真正具备正期望且工程可落地的 **4 个盈利利基**：

| 赛道 | 盈利逻辑与实证锚点 | 核心实现模块 | 实盘前核心目标 |
| --- | --- | --- | --- |
| **1A · 4 币截面强弱动量**<br>（纯盈利首选慢 Alpha） | `basis_hours_v2`：`{BTC, ETH, SOL, XRP}` 24h 强弱美元中性多强空弱，78 窗在 `0.25bp` 盈利 **`+$2.68`**、在 `1.25bp` 盈利 **`+$1.13`**（每美元换手毛优势 `+1.98 bp`，仅被旧 5m K 线滑点模型拖负） | [cross_sectional.rs](../src/strategy/cross_sectional.rs)<br>（因果排序 + rank 滞回 + 资金费 veto + Lighter `0ms PostOnly` 贴盘追价 + 有界 IOC 兜底） | 用 `aleph` 真实逐笔/盘口带替换 5m K 线近似，验证全时段扣费正现金与最大回撤 |
| **1B · 状态机非连续做市 & 贴盘反转做市** | 避开突破毒流量，只在平静均值回归区间与砸盘耗竭见底瞬间提供流动性；配合 `asymmetric_entry_ratchet`（0 秒强撤不利挂单）、`ofi_withdraw_threshold`（硬撤单而非加宽 2bp）与 `entry_touch_offset_bps`（解决 1,006 笔零成交） | [defense.rs](../src/strategy/inventory_neutral_mm/defense.rs)、[local_signal.rs](../src/strategy/inventory_neutral_mm/replay/recorded/local_signal.rs)、[components.rs](../src/strategy/inventory_neutral_mm/components.rs) | 通过 6/8 窗全套 Sprint 3+4 消融 A/B，并在 `W07–W16` 验证连续 10h 净现金转正 |
| **2 · 美股 RWA / 跨所基差套利** | 只在高确定性美股正规时段做 `MakerFirst` 被动入场 + 对冲腿流量撤单保护（W05 SPY/QQQ 闭合循环分别 `+0.1393` / `+0.0648` 美元）；拒绝盲目高频对冲 | [maker_first.rs](../src/strategy/relative_value/paper/maker_first.rs)、`HedgeVenue` | 完成 W06 部分投影核算与 `W07–W16` 冻结前瞻全量评分（`score_forward.py`） |
| **3 · 低损高确定性激励做市** | 限缩 2 笔库存 + Stoikov `TouchMicro` + `OPT-05` 额度保护（后 4 窗 BTC/ETH 净盈 `+$1.04`；台湾 Mode B `$500` 三档深度两窗正现金），严格先核实账户/资产返佣资格（`VMM-01`） | [inventory_neutral_mm.rs](../src/strategy/inventory_neutral_mm.rs)、[trading.rs](../src/exchanges/lighter/trading.rs) | 未完成真实账户等级与合格量实证前，禁止为刷量硬扛现金负期望 |

---

## 三、 Carbon（Antigravity）× Astra 6.2（Codex）下一步协同作战分工

| 阶段 | **Carbon（Antigravity — 内核与策略架构主力）** | **Astra 6.2（Codex — 数据、大矩阵因果评测与 Linux 验证主力）** | 协同验收标准（Done Looks Like） |
| --- | --- | --- | --- |
| **Phase 1：实证验证与新引擎回放对接** | 提供并维护 **Sprint 3 + Sprint 4** 全部可选执行/策略/风控原语；根据 Astra 6.2 回放报告做分钟级/成交级微结构归因修复 | 1. 跑 `aleph` 6/8 窗 A/B 消融（`asymmetric_entry_ratchet`、`ofi_withdraw_threshold`、`entry_touch_offset_bps`、`defense_policy`）；<br>2. 将 `cross_sectional.rs` 接入 `aleph` 4 币 `{BTC, ETH, SOL, XRP}` 原生 BBO/资金费回放；<br>3. 执行 `W07–W16` `score_forward.py` 前瞻评分并补 Linux x86_64/CGo 验收 | 每套候选均有独立 10h 因果账本、费用与资金费明细、最坏回撤与零成交统计；无参数偷看 |
| **Phase 2：实盘前核心硬闭环** | 1. 将 `live_hedge.rs` 彻底切换至 `Box<dyn HedgeVenue>` 并补齐 `OPT-04` 通用批边界；<br>2. 对接 `OPT-05` 额度告警至 `deploy/oasis` 监控面板；<br>3. 在新测试账户/本地 fake venue 压测 `defense.rs` 与 `trading.rs` 防呆价带/额度保护 | 1. 完成 `OPT-01` 全 runner 逐资产/账户真实费率表；<br>2. 对前瞻胜出策略做不同市场体制（牛/熊/横盘/高波动）稳定性检验；<br>3. 核验 `W06–W16` 数据全量 SHA 与 Drive 归档一致性 | `OPT-01/04/05/07` 与对应 VMM 核对项闭环，全门禁 Mac + Linux 双平台零告警通过 |
| **Phase 3：只读影子验证（零实盘风险）** | 构建与 `InventoryNeutralMM` / `CrossSectionalEngine` 同核的 `shadow` 只读观察运行模式（连接真实 Go Feeder SHM 与账户只读接口，只吐决策意图与实时 `markout`/风控状态机日志） | 连续 24–72 小时在 `tokyo` / `aleph` 对比线上真实盘口延迟、SHM 跳变、`volume_quota` 真实变化与策略状态机触发频率，排查任何实盘环境差异 | 实盘网速/jitter 分布、消息队列零断档、0 个非预期风控误触发 |
| **Phase 4：授权后微仓金丝雀实盘** | **需用户明确授权**：先单开表现最强且有严格止损/风控的独立赛道（预计首选 Track 1A 低换手 4 币 `PostOnly` 截面动量或经 `W07–W16` 验证的正期望 `TouchMicro` 做市），`DailyLossGuard + FairMark + DefenseStateMachine + OPT-05` 全量开启 | 借助 `live_attribution.py` 与 `telemetry_store.py` 逐小时自动对账：实盘 1s/5s markout、成交价滑点、实际手续费/资金费与离线回放的偏差归因 | 小资金（如 `$100–$500`）连续稳定正收益、回撤可控且实盘 vs. 回放误差收敛，方可讨论阶梯放大本金 |
