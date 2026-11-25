# AlephTX 策略与工程全面优化实施蓝图（2026-11-25）

> **范围与真实性声明**：本文档将 2026-11-25 策略深度优化研究报告与仓库当前实际代码基线（`origin/main = 94e000f` 及其后本轮增量加固）逐项对账，明确区分**已提前修复/现存组件**、**本轮已编码落地的模块与护栏**以及**尚待数据采集或独立人工审批解冻的剩余工作**。本文档不包含任何私钥、账户敏感信息或外部私有链接；所有美元数值均以内联代码标示以隔离渲染语法。

---

## 一、报告审计核对：已修复项与基线现状（避免重复建设）

研究报告基于较早快照（`8a5edab`）提出的部分 S19/基础架构问题，在当前仓库中已经解决或已存在对应模块：

| 报告议题 | 当前仓库实际状态 | 代码位置 |
| --- | --- | --- |
| **S19 R1 · Arcus cancel lease 时钟夹紧导致过期误判** | **已修复**：`renew_cancel_lease` 使用未夹紧的 `self.wall_clock_ns()?` 调用 `begin_renewal(..., sent_mono)` | [src/exchanges/arcus/rest.rs](../src/exchanges/arcus/rest.rs) |
| **S19 R2 · Hyperliquid 同毫秒多笔成交误触发 `recovery_required`** | **已修复**：`source_reversed` 严格检查 `<`，同毫秒 `==` 仅保守置 `attribution_known = false` 而不误报逆序恢复 | [src/hedging/durable.rs](../src/hedging/durable.rs) |
| **Arcus `NativeFill.fee_asset` 为 `None`** | **有意保留**：Arcus API 未返回 `fee_asset`，由测试 `arcus_missing_fee_asset_stays_unknown_even_with_exact_fill_binding` 强制未知保持未知 | [src/hedging/durable/native.rs](../src/hedging/durable/native.rs) |
| **GLFT / Kalman-OU / 多日 Carry / Cross-Venue / Tail Risk 基座** | **已存在**：对应核心模块已在库内独立维护 | [glft.rs](../src/strategy/glft.rs), [kalman_basis.rs](../src/strategy/kalman_basis.rs), [multiday_carry.rs](../src/strategy/multiday_carry.rs), [cross_venue_router.rs](../src/strategy/cross_venue_router.rs), [tail_risk.rs](../src/strategy/tail_risk.rs) |
| **Lighter RH / Backpack DM / Spread-to-Vol 研究评估脚本** | **已存在并由 Patrol 持续维护** | [eval_lighter_rh_incentive_guard.py](../scripts/eval_lighter_rh_incentive_guard.py), [sim_backpack_dm_guardian.py](../scripts/sim_backpack_dm_guardian.py), [screen_market_spread_to_vol.py](../scripts/screen_market_spread_to_vol.py) |
| **冻结核心与红线** | **`inventory_neutral_mm.rs` SHA-256 保持未改，三条实盘/金丝雀轨道维持 `HOLD`** | [src/strategy/inventory_neutral_mm.rs](../src/strategy/inventory_neutral_mm.rs), [AGENT_HANDOFF.md](../AGENT_HANDOFF.md) |

---

## 二、三大场景中性盈利架构总览

为遵守[独立策略研究契约](INDEPENDENT_STRATEGY_RESEARCH.md)（禁止不同风险预算、流动性域或套利机制之间交叉补贴），系统按场景物理拆分为三条清晰演进路径：

| 场景 | 目标机制与交易所域 | 核心 Alpha 与防亏损边界 | 本轮落地核心组件 |
| --- | --- | --- | --- |
| **Scenario 1** | **高频混合做市 + 毒性防护**（Lighter Core/RH） | Stoikov Markov 微价格去毒 + 双态动态 `kappa` + 队列保持减仓退出 + 盐化钱包毒性护盾 | [microprice.rs](../src/strategy/microprice.rs), [hybrid_exit.rs](../src/strategy/hybrid_exit.rs), [wallet_toxicity.rs](../src/strategy/wallet_toxicity.rs), [glft.rs](../src/strategy/glft.rs), [validate_wallet_toxicity.py](../scripts/validate_wallet_toxicity.py) |
| **Scenario 2** | **结构性流动性返佣 + 跨所领航做市**（Arcus T5/T6 + Hyperliquid） | Nov 18 返佣阶梯（`T5 >1.5% / -0.20 bp`, `T6 >4.0% / -0.30 bp`）按对手方实付 Taker 费封顶，严守 `<= 1.0x` 杠杆并联动 100 ms ADL 熔断 | [eval_arcus_maker_rebate.py](../scripts/eval_arcus_maker_rebate.py), [tail_risk.rs](../src/strategy/tail_risk.rs) |
| **Scenario 3** | **跨所期现/永续基差套利**（Arcus ↔ Hyperliquid `xyz` HIP-3） | Growth Mode `enabled` 费率 preflight + 实时 Oracle 偏离限新门控 + 可选 Kalman/OU 过滤与半衰期止损 + `/dev/shm` 零拷贝读取与自动 REST 回退 | [arcus_xyz_basis.rs](../src/strategy/arcus_xyz_basis.rs), [src/bin/arcus_xyz_basis.rs](../src/bin/arcus_xyz_basis.rs), [config.arcus_xyz_basis.example.toml](../config.arcus_xyz_basis.example.toml) |

---

## 三、本轮已落地的工程与策略改造详情

### 3.1 Scenario 1：微价格、排队保持退出与反逆向选择体系

1. **Stoikov (2018) 离散 Markov 微价格**（[src/strategy/microprice.rs](../src/strategy/microprice.rs)）
   - 离线/周期通过 `MarkovMicropriceTable::fit` 拟合有限步转移矩阵并与线性方程组求解公平调整量 `g_star`，线上热路径 `MarkovMicropriceTable::microprice` 仅执行 `O(1)` 最近分桶查表（阶梯函数，无桶间插值，零堆分配），并保持严格对称失衡分桶（`symmetric_imbalance_bucket`）。
2. **三阶段排队保持被动退出与周期现金流台账**（[src/strategy/hybrid_exit.rs](../src/strategy/hybrid_exit.rs)）
   - `PassiveExitStage` + `HybridExitController` 在持仓同阶段内保持 `reprice_required = false`，避免无效撤挂丢失队列优先权；所有退出指令强制 `reduce_only = true`，确保不被开仓限额与渐进回撤刹车误拦截。
   - `InventoryCycleCashLedger` 按开/平周期累计已实现价差与费用，并经 `record_funding` 将资金费作为独立带符号项（正为收、负为付）在账本存续期内净计入 `paired_cycle_net_cash_usd`（逐手持有期资金费归因暂缓），防止名义成交量虚胖掩盖现金流亏损。
3. **对手方毒性护盾与严格 10 天非重叠 OOS 统计门控**（[src/strategy/wallet_toxicity.rs](../src/strategy/wallet_toxicity.rs), [scripts/validate_wallet_toxicity.py](../scripts/validate_wallet_toxicity.py)）
   - 运行端使用混淆加盐键 `salted_wallet_key` 与有界内存表跟踪短窗毒流分档：`D9` 触发对威胁侧 `2.0x` 点差加宽、`D10` 触发 `3.0x` 加宽；凡触发加宽的毒流态（`D9` 与 `D10`）均在 `1,000 ms` 冷却期内将开仓侧挂单规模缩至 `25%`（即衰减 `75%`）；对减仓侧及合法大单拆单流（`is_twap_sub = true`）永久豁免。
   - 离线验证器按严格非重叠 10 天窗口（`864,000,000 ms`）因果切分样本，要求样本外 Spearman `rho >= 0.30` 且 `OOS D10 - D5 >= 0.80 bp`（自动过滤 `twap_sub` 样本，严格 `allow_nan=False` JSON 输出）。
4. **双态滚动动态 `kappa` 校准与 `S12-3` 成交分散度守卫**（[src/strategy/glft.rs](../src/strategy/glft.rs)）
   - 在保留现有浅层 OLS 基线的基础上，新增 `RegimeDynamicKappaCalibrator`（`Calm` / `HighVol` 双态滞回阈值 `1.5x / 1.2x`、5 秒校准周期、10 分钟滚动窗口），并内嵌 `S12-3` 单窗口成交额占比 `< 35%` 与最小非重叠窗口门槛校验。

### 3.2 Scenario 2 & 3：ADL 快速熔断、HIP-3 Growth Mode 与基差执行加固

1. **Sub-100 ms ADL 净敞口紧急恢复断路器**（[src/strategy/tail_risk.rs](../src/strategy/tail_risk.rs)）
   - 新增 `AdlDeltaBreakerConfig` 与 `evaluate_adl_delta_breaker`：一旦非计划净 Delta（例如盈利腿被交易所 ADL 强减）超过基准腿名义/数量 `1.0%`，立即判定 `EmergencyReduceLeg`（目标 `100 ms` 内触发只减仓对冲），若持续超过 `200 ms` 未愈合则强制升级为 `HaltAndFlatten`。
2. **Arcus ↔ Hyperliquid `xyz` 执行器与监控集成**（[src/strategy/arcus_xyz_basis.rs](../src/strategy/arcus_xyz_basis.rs), [src/bin/arcus_xyz_basis.rs](../src/bin/arcus_xyz_basis.rs), [config.arcus_xyz_basis.example.toml](../config.arcus_xyz_basis.example.toml)）
   - **Growth Mode 硬核校验（`validate_hl_market_preflight`）**：当 `hl_dex` 非空时默认开启 `require_hl_growth_mode = true`，启动阶段（含 `--shadow` 与 `--canary`）强制验证 `growthMode == "enabled"`、未退市（`!is_delisted`）及 `deployer_fee_scale <= max_hl_deployer_fee_scale`。
   - **实时 Oracle 偏离限新门控（`evaluate_oracle_divergence_bps`）**：`reconcile_both_venues` 并发刷新当前 `hl_oracle_px`；若 Arcus 参考中间价相对 HL Oracle 偏离超过 `max_oracle_divergence_bps`（默认 `25.0 bp`），在 `run_canary_live` 中将动作降级为 `ReduceOnly`（仅阻止新开仓 `EnterPairIoc`，保持平仓与残差对冲通路畅通），坏值即刻失败关闭。
   - **可选 Kalman/OU 基差过滤（`on_books_with_regime`）**：通过 `kalman_basis` 配置可选启用（默认 `None`，保持预注册 `EXP-ARB-A2` 基线参数零漂移）；启用后自动阻断未平稳（`WarmingUp`/`NonStationary`）或超界开仓，并以半衰期倍数触发 `timed_out` 收敛退出。
   - **零拷贝 `/dev/shm` 行情接入与 fail-closed 回退（`ShmBookFeed`）**：同步解码 `/dev/shm/aleph-matrix`（`ShmReader`, slots `7 = Arcus`, `10 = TradeXYZ`）与 `/dev/shm/aleph-depth`（`ShmDepthReader`，强制 L2 单调性、非交叉与 L1/L2 同时间戳价位对齐）；由于当前 64B V12 Matrix 仅携带交易所源时间戳（`source_ms`，无独立 feeder 收包钟，`feeder_receipt_ms = None`），`ShmBookFeed::read_books` 在缺少独立本地收包钟时严格失败关闭并回退至带真实 `unix_ms()` 收包时间的并发 REST（`fetch_rest_books`）。
3. **Arcus Maker Rebate T5/T6 敏感性与杠杆约束评估器**（[scripts/eval_arcus_maker_rebate.py](../scripts/eval_arcus_maker_rebate.py)）
   - 严格建模 2026-11-18 Arcus 费率阶梯（`T5 >1.5% / -0.20 bp`、`T6 >4.0% / -0.30 bp`）与“单笔 Maker 返佣以对手方实际支付 Taker 费为上限”机制，在 `$0.9B / $1.5B / $3.0B` 日成交量及 `$0..$200/M` 逆向选择磨损矩阵下输出损益平衡点，并硬性拒绝 `> 1.0x` 名义杠杆。

---

## 四、尚未完成的工作与三条实盘轨道解冻前置条件

### 4.1 交易所彻底解耦的通用底层与薄适配层架构

为确保策略数学、风控熔断、共享内存解码与研究评估可跨任意新接入交易所零阻力复用，底层模块全面实行**“零 `crate::exchanges::*` 依赖”**物理隔离：

1. **通用策略与数据底层（零交易所耦合）**：
   - [src/shm_book.rs](../src/shm_book.rs)（`GenericShmBookFeed`、`NormalizedBookSnapshot`、`FixedPriceLevels`、`FastBookSnapshot`、`read_pair_fast`）：统一 `/dev/shm/aleph-matrix` + `/dev/shm/aleph-depth` 栈上定长零堆分配解码（消除热路径 `Vec` 与可选 `Decimal` 往返）、显式 `DepthStatus::Unavailable` 降级可观测性、槽位身份校核、L2 单调/非交叉/L1 对齐检查，并明确区分 `source_ms` 与可选 `feeder_receipt_ms: Option<u64>`（V12 置 `None`）。
   - [src/strategy/venue_guards.rs](../src/strategy/venue_guards.rs)（`VenueMarketMetadata`、`VenuePreflightPolicy`、`evaluate_reference_oracle_divergence_bps`、`MultiOraclePolicy`、`evaluate_multi_source_oracle_median`）：通用市场资格/费用折扣档预检、精确 `U320` 十进制 Oracle 偏离门控（消除 `#686` 浮点舍入误拒），以及抗单源操纵/过期分裂的多源中位数共识校验。
   - [src/strategy/quote_pipeline.rs](../src/strategy/quote_pipeline.rs)（`StrategyMode`、`LeadLagConfig`、`MarkoutShieldConfig`、`FundingBiasConfig`、`InventoryClipConfig`、`QuotePipeline`、`QuotePlan`）：单一纯通用策略编排核心，集成 Stoikov Markov 微价格、GLFT 闭式做市、外部领航盘前向移位与单边毒流开仓否决、在线逐笔 EWMA markout 逆选护盾（`MarkoutShield`）、资金费率保留价偏置（`FundingBiasConfig`）、库存感知开仓渐缩（`InventoryClipConfig`）、队列位置退出联动（`QueueExitConfig`）、对手方毒性护盾及回撤刹车（全部新策略增强默认 `enabled = false`，零冻结漂移）。
   - [src/strategy/pipeline_driver.rs](../src/strategy/pipeline_driver.rs)（`QuoteSlotTracker`、`GenericExchangeDriver`、`run_continuous_pipeline_replay`、`run_pipeline_benchmark`）：基于 `Exchange::execute_batch` 的 `(side, price, size)` 精确匹配先撤后挂差量执行器与 `#706` 客户端订单号迟到成交隔离，以及严格分拆 `warmup_ledger` / `eval_ledger` / `drain_ledger`（含持仓资金费计提）的连续确定性回放引擎。
   - [src/strategy/pipeline_tape.rs](../src/strategy/pipeline_tape.rs)（`PipelineTapeConfig`、`run_perp_frames_pipeline_replay`、`run_pipeline_tape_file`）：严格因果 `PerpFrame`/JSON/NDJSON 磁带适配器（接入 `cross_venue pipeline-replay`），仅用热身期样本拟合 Stoikov Markov 微价格并在未收敛/缺样本时自动退化，同步计算滚动因果波动率状态机。
   - [src/strategy/basis_core.rs](../src/strategy/basis_core.rs)（`quantize_shared_step_qty`、`NormalizedLegBook`、`ResidualRecoveryPolicy`、`FundingSettlementConfig`、`LatencyBudgetConfig`、`BasisCoreEngine::on_books_with_context`）：完全无 `crate::exchanges::*` 依赖的跨所基差核心，支持任意双所精度 LCM 网格、强制独立 `feeder_receipt_ms` 验证、多源中位数/单源 Oracle 入口阻断且放行减仓、可选 Kalman/OU 与非对称阶梯加仓、资金费结算前静默窗与 Carry 调整退出、滚动 p95 RTT 延迟预算与自适应滑点/缩量，并在 hedge 腿遭遇 ADL 强减时经 `PreferReduceOnlySurvivingLeg` 发出存活主腿 `reduce_only = true` 紧急减仓指令。
   - [src/strategy/basis_replay.rs](../src/strategy/basis_replay.rs)（`BasisReplayConfig`、`run_continuous_basis_replay`、`run_basis_tape_file`、`run_basis_benchmark`）：基于 `BasisCoreEngine` 的连续 10 小时跨所基差离线回放与 ADL/Oracle 对照基准（接入 `cross_venue basis-replay` 与 `cross_venue basis-bench`），落实可见深度钳位、`#694` 连续覆盖校验及热身/评测/排空隔离。
   - [src/strategy/track_coordinator.rs](../src/strategy/track_coordinator.rs)（`TrackSpec`、`TrackCoordinator`、`FleetRiskReport`）：三场景（中性做市 / 返佣做市 / 跨所基差）零交叉补贴监督器，强制原子化输入校验、每轨独立 `DailyLossGuard`、缺失依赖场所快照强制 `ReduceOnly` 保护（`missing_venue_snapshot:<domain>`）、单所严重风险升级保留，并按规范化 `(venue_domain, market_key)` 物理拒绝跨轨同市场自成交重叠。
   - [scripts/eval_venue_maker_rebate.py](../scripts/eval_venue_maker_rebate.py)：通用返佣/对手 Taker 上限/杠杆与磨损评估引擎（支持任意 `--schedule-json` 与预设档位）。
2. **交易所薄适配层（仅做字段翻译与指令路由）**：
   - [src/strategy/arcus_xyz_basis.rs](../src/strategy/arcus_xyz_basis.rs) 与 [src/bin/arcus_xyz_basis.rs](../src/bin/arcus_xyz_basis.rs) 仅负责将 Hyperliquid `PerpMarket` 映射为 `VenueMarketMetadata` 并调用 `GenericShmBookFeed`；[eval_arcus_maker_rebate.py](../scripts/eval_arcus_maker_rebate.py) 作为 `arcus_2026_11_18` 预设薄封装复用通用引擎。

### 4.2 独立代码/工程复审捕获并修复的 5 项关键缺陷

在初步实现后通过独立源码审查与自动化边界检测，定位并修正了以下 5 处高危量化/执行与工程规范问题：

1. **Stoikov (2018) 离散更新算子与热路径零分配**（[microprice.rs](../src/strategy/microprice.rs)）：将原迭代转移矩阵从 `Q + T`（会重复计入零变动停留步）修正为精确 Theorem 1 更新算子 `B^* = (I - Q)^{-1} T` 与 `G_1 = (I - Q)^{-1} r`，并以双缓冲 `std::mem::swap` 消除迭代堆分配；新增与精确有限步级数对齐的回归测试。
2. **ADL 紧急减仓选腿反转风险**（[tail_risk.rs](../src/strategy/tail_risk.rs)）：新增 `PairedLeg::{Primary, Hedge}`，保证 `evaluate_adl_delta_breaker` 恒定挑选与净敞口 `net_delta_base` 同向的存活腿执行 `reduce_only = true` 缩仓并将数量钳位于该腿实际持仓，避免被减腿再遭交易所 `reduce_only` 拒单。
3. **双态动态 `kappa` 非原子切换与重复 Tick 虚高聚合**（[glft.rs](../src/strategy/glft.rs)）：将所有填充窗口合法性检验前置至 `self.regime` 切换之前，防止坏输入污染内部状态；对多窗口内相同 `distance_ticks` 的观测先按 `sample_count` 加权归并后再入线性回归，杜绝稀薄双档位在多窗下虚增 `retained_buckets`。
4. **IEEE-754 边界舍入、跨触价保持与开仓手续费污染**（[hybrid_exit.rs](../src/strategy/hybrid_exit.rs)）：加入 `1e-9` 容差保护 `1-tick` 价差取整，增加队列保持前的非交叉触价校核，补充 `notify_partial_fill` 原地缩仓接口，并将 `record_fill` 重构为 `CycleFillInput` 结构体同时按 `OpenLot` 比例分摊入场手续费（`paired_cycle_fees_usd`），避免未平新仓手续费扭曲已闭合周期的真实净现金。
5. **Python 研究验证器类型与口径加固**（[validate_wallet_toxicity.py](../scripts/validate_wallet_toxicity.py), [eval_venue_maker_rebate.py](../scripts/eval_venue_maker_rebate.py)）：在全入口强制 `bool` 类型拒收（`isinstance(val, bool)`）、文件存在性检查（`is_file()`）、`--self-test` 参数冲突拒绝，以及按 `notional_usd` 加权聚合成交标记盈亏。

### 4.3 仍待补齐的数据与架构能力

1. **Go Feeder 侧 `account_id` 实盘采集接通**：当前 [wallet_toxicity.rs](../src/strategy/wallet_toxicity.rs) 与 [validate_wallet_toxicity.py](../scripts/validate_wallet_toxicity.py) 已完成 Rust 状态机与离线门控；线上激活仍需在合规公开逐笔或自身成交链路确认可用对手方标识后（如 [DUAL_TRACK_IPC.md](DUAL_TRACK_IPC.md) 所述保持 `128B` `ShmAccountStats` 契约不动），通过 10 天连续样本验证 Spearman `rho >= 0.30` 方可上线。
2. **`BULK` 逐笔成交（`-bulk-trades`）与实盘执行适配**：当前仓库在 [src/capture.rs](../src/capture.rs) 与 [src/exchanges/bulk/](../src/exchanges/) 已支持 L2 快照录制与只读回放，尚未补齐 `-bulk-trades`（`TODO-DATA-02`）与实盘下单执行器，须按 [TOKYO_RESEARCH_AND_EXECUTION_TODO.md](TOKYO_RESEARCH_AND_EXECUTION_TODO.md) 独立推进。
3. **统一策略模式路由器（`strategy_mode`）实盘适配器**：已通过 [quote_pipeline.rs](../src/strategy/quote_pipeline.rs) 完成无交易所耦合核心；待 Scenario 1 / 2 / 3 各自 10 小时 OOS 验证放行后，由轻量化 venue runner 调用 `QuotePipeline::evaluate` 并严守跨轨道独立资金隔离（不写入冻结基线 `config.example.toml`）。

### 4.4 本轮增量工程加固（`94e000f` 之后）

- **Codex `#714` 同形开/平批回报身份消歧**（[src/strategy/pipeline_driver.rs](../src/strategy/pipeline_driver.rs)）：`QuoteSlotTracker::apply_batch_result` 对同批次内同 `(side, price, size)` 的多重候选报错并触发 `invalidate_all_unknown()`（防止乱序批回报把 `Opening` 与 `ReduceOnlyExit` 槽位 `client_order_index` 交叉错绑）。
- **因果闭环 `MarkoutShield` + FIFO 排队估计与回放接通**（[src/strategy/pipeline_driver.rs](../src/strategy/pipeline_driver.rs), [src/strategy/hybrid_exit.rs](../src/strategy/hybrid_exit.rs)）：`run_continuous_pipeline_replay` 新增严格因果 `1_000 ms` 延迟队列反馈已实现 markout 至 `MarkoutShield`，并集成栈上 `QueueAheadTracker` 在启用 `queue_exit` 时持续跟踪减仓单前方排队量。
- **零堆分配行情直通与双所在途抵押品守卫**（[src/shm_book.rs](../src/shm_book.rs), [src/strategy/quote_pipeline.rs](../src/strategy/quote_pipeline.rs), [src/strategy/tail_risk.rs](../src/strategy/tail_risk.rs)）：`FastBookSnapshot` 提供零分配 `mid_f64 / spread_bps / top_depth_imbalance` 及 `GenericMarketState::from_fast_book` 直通桥接；`evaluate_collateral_rebalance` 按悲观口径扣减在途出金、不预计在途入金并拦截超时在途划转。

### 4.5 三条实盘 / Canary 轨道维持 `HOLD` 的客观放行条件

- **`lighter_inventory_mm`（Core / RH）**：维持 `HOLD`，需先在独立账户与实盘/录制验证中消除零返佣磨损（`S12` 实盘回放与正净利留存门控通过）。
- **`relative_value`**：维持 `HOLD`，须等待美股 RTH 时段 Step 2 Gate 采集及跨所 Oracle/Funding OOS 验证全部放行。
- **`arcus_xyz_basis`**：维持 `HOLD`；虽然本轮已补齐 Growth Mode、Oracle 偏离、ADL 断路器与 SHM 读取保护，且默认示例配置保持 `dry_run = true`，实盘启动仍严格要求完成周一 RTH Step 2 Gate 检查，并在运维窗口内显式传入 `--canary --confirm-live-canary` 授权。
