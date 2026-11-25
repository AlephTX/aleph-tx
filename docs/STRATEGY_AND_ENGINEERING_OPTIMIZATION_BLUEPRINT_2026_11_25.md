# AlephTX 策略与工程全面优化实施蓝图（2026-11-25）

> **范围与真实性声明**：本文档将 2026-11-25 策略深度优化研究报告与仓库当前实际代码基线（`origin/main = 8068b0d` 及其后 P0–P3 策略与工程增量加固）逐项对账，明确区分**已提前修复/现存组件**、**本轮已编码落地的模块与护栏**以及**尚待数据采集或独立人工审批解冻的剩余工作**。本文档不包含任何私钥、账户敏感信息或外部私有链接；所有美元数值均以内联代码标示以隔离渲染语法。

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

### 4.4 本轮增量工程加固与指标纠偏（`94e000f -> 8068b0d` 及其后 P0–P3 推进）

- **Codex `#714` 同形开/平批回报身份消歧与基础防护基座**（[src/strategy/pipeline_driver.rs](../src/strategy/pipeline_driver.rs), [src/strategy/hybrid_exit.rs](../src/strategy/hybrid_exit.rs), [src/strategy/quote_pipeline.rs](../src/strategy/quote_pipeline.rs), [src/strategy/basis_core.rs](../src/strategy/basis_core.rs)）：`QuoteSlotTracker::apply_batch_result` 对同 `(side, price, size)` 多重候选报错并触发 `invalidate_all_unknown()`；`run_continuous_pipeline_replay` 严格按 `1_000 ms` 因果延迟队列向 `MarkoutShield`（含 `per_side` 模式）反馈实现 markout 并驱动 `QueueAheadTracker`；`QuotePipelineConfig` 提供默认关闭的 `VolatilityMomentumDefenseConfig`；`SlotDiffConfig` 提供尺度无关 `reprice_deadband_bps`（与 tick 死区取较大值）；`BasisCoreConfig` 提供默认 `0.0` 的 `spread_cost_entry_coeff`（仅上调入场门槛，不影响出场）。
- **P2 因果量纲、真实波动率桥接与审计隔离修复（已落地代码与单测）**：
  - **`funding_interval_ms` 周期-小时量纲归一化**（[src/strategy/pipeline_tape.rs](../src/strategy/pipeline_tape.rs)）：`causal_funding_rate_bps_per_hour` 显式除以 `funding_interval_ms / 3_600_000.0`，将按结算周期计价的 `FundingPoint.rate` 准确折算为每小时 bp；`run_pipeline_tape_file` 自动从磁带头解析并校验显式配置一致性，未知周期 fails closed 返回 `None`；同时兼容 NDJSON 首行前置空行。
  - **Codex `#717` 规范场所抵押品去重与 `#719` 异步双腿资金费率时钟隔离**（[src/strategy/tail_risk.rs](../src/strategy/tail_risk.rs), [src/strategy/basis_replay.rs](../src/strategy/basis_replay.rs), [src/strategy/basis_core.rs](../src/strategy/basis_core.rs)）：`evaluate_collateral_rebalance` 按大小写折叠与首尾去空白后的规范 venue ID 拒绝重复快照，扣减在途出金、不预计在途入金并拦截超时在途划转；`basis_frames_from_perp_frames` 在两腿 `next_settlement_ms` 相差 `> 1 ms` 时仅采用较早到期单腿时钟并在 `max_horizon_ms` 内将远端腿速率保守置 `0.0`。
  - **严格因果实现波动率估计与零分配直通桥接**（[src/shm_book.rs](../src/shm_book.rs), [src/strategy/quote_pipeline.rs](../src/strategy/quote_pipeline.rs), [src/strategy/pipeline_tape.rs](../src/strategy/pipeline_tape.rs)）：新增只向前单遍滑动实现波动率估计器 `CausalRealizedVolEstimator`（屏蔽非递增时钟与非正价格），并在 `GenericMarketState::from_fast_book` 显式接收 `realized_vol_bps_per_sqrt_s: Option<f64>`。注：`QuotePipeline`、`FastBookSnapshot` 与 `CausalRealizedVolEstimator` 均尚未接入仍处于人工审批冻结期的 [src/strategy/inventory_neutral_mm.rs](../src/strategy/inventory_neutral_mm.rs)。
- **P0 归一化评估契约与可比性门限**（[scripts/score_strategy_arms.py](../scripts/score_strategy_arms.py), [scripts/test_score_strategy_arms.py](../scripts/test_score_strategy_arms.py), [src/strategy/pipeline_driver.rs](../src/strategy/pipeline_driver.rs), [src/strategy/basis_replay.rs](../src/strategy/basis_replay.rs)）：
  - 在 `PipelineReplayReport` 与 `BasisReplayReport` 内建输出 `eval_traded_notional_usd` / `eval_net_per_notional_bps` / `eval_markout_1s_bps` 与 `traded_notional_usd` / `net_per_roundtrip_usd` / `net_to_fee_ratio` / `net_per_notional_bps`（非 ADL 配对成交）。
  - 统一使用 `scripts/score_strategy_arms.py` 以 **`net / traded_notional`（bp）** 作为做市主指标、以 **`net / roundtrip`（美元）** 与 **`net / fee`** 作为套利主指标，强制施加相对基线成交名义额 **`±30%` 可比性门限**与 `Basis` 最小往返次数 `thin_sample_warning`（默认 `< 30 rt`）。
- **历史 `stage_c` / `stage_d` 统计口径退化纠偏**（重算档案 `.agent-state/research/aleph_10h_real_eval_20261126/p0_rescore_legacy.json`）：
  - **Scenario 1（旧版仅 `price_through` 扫穿，无盘口差分成交）**：全部 `64` 组 10h 跑结果的 `net ~ traded_notional_usd` 线性回归斜率 **`-2.0053 bp`**、**`R² = 0.9193`**（`selection_dev n=48`: `-1.9950 bp`, `R² = 0.9071`；`holdout n=16`: `-1.8022 bp`, `R² = 0.9702`）。原 `A0 -> C5` holdout 美元亏损 `-$254.00 -> -$30.00` 的“收敛 88%”主要来源于成交额从 `$1,398,594` 坍缩至 `$221,381`（`0.158×`，未通过 `±30%` 可比门限）。按 `net / traded_notional` 口径，holdout `A0_base` 为 **`-1.816 bp`**（BTC `-1.748 bp` / ETH `-1.883 bp`）、`C5_flr1.5_combined` 为 **`-1.355 bp`**（BTC `-1.341 bp` / ETH `-1.370 bp`），真实单位成本改善约为 **`25.4%`**（BTC `23.3%` / ETH `27.3%`），而非 `88%`；同批 `C4_combined_opt`（**`-1.393 bp`**，BTC `-1.386 bp` / ETH `-1.402 bp`，`0.532×`）单位成本实则**优于** `A2_markout_only`（**`-1.958 bp`**，BTC `-1.974 bp` / ETH `-1.945 bp`，`0.187×`）。`selection_dev` 仅 `C1_db0.5`（`1.151×`，`-2.190 bp` vs `A0 -2.256 bp`，`+0.066 bp`）通过可比门限；`C2`（`1.419×`，`-1.653 bp`）、`C3`（`0.355×`，`-2.046 bp`）、`C4`（`0.321×`，`-1.506 bp`）、`C5`（`0.062×`，`-1.653 bp`）均因名义额偏离 `> 30%` 判定为不可比。纯 `price_through` 下做市方 100% 逆向成交，数学上不可能转正。
  - **Scenario 3 历史混杂臂重算**：`holdout (4/4 cont=true)` 总样本仅 `26` 次往返（触发 `thin_sample_warning`）：`B0_ref` `6 rt` / `+$0.9130` / **`$0.1522/rt`** / `net/fee = 0.5646`，`B1_c300_sc025` `14 rt` / `+$1.4309` / **`$0.1022/rt`** / `net/fee = 0.3789`（因同时变动 `clip` 与 `persist_ms`，`+$0.9130 -> +$1.4309` 实为放大低质量交易次数而非单次质量提升），`B2_c500_sc025` `6 rt` / `+$1.7009` / **`$0.2835/rt`** / `net/fee = 0.6307`。`selection_dev`（`11/11` 全窗 vs `9/11 strict_cont_only`）：`B0_ref` `33 rt` / `+$6.0546` / `$0.1835/rt` / `0.7130`（连续 `26 rt` / `+$4.8220` / `$0.1855/rt` / `0.7301`）、`B1_c300_sc025` `72 rt` / `+$7.9874` / `$0.1109/rt` / `0.4350`（连续 `51 rt` / `+$5.7182` / `$0.1121/rt` / `0.4503`）、`B2_c500_sc025` `36 rt` / `+$12.2399` / `$0.3400/rt` / `0.7895`（连续 `28 rt` / `+$9.7470` / `$0.3481/rt` / `0.8185`）、`B3_c500_sc035` `38 rt` / `+$11.0742` / `$0.2914/rt` / `0.6751`（连续 `29 rt` / `+$8.5467` / `$0.2947/rt` / `0.6915`）。
- **P1 真实 L2 盘口差分扫盘模型 (`BookDiffSweepConfig` + FIFO `queue_ahead_base`) 实测**（[src/strategy/pipeline_tape.rs](../src/strategy/pipeline_tape.rs), [src/strategy/pipeline_driver.rs](../src/strategy/pipeline_driver.rs)；`maker=0 bps, taker=3.5 bps`, `initial_queue_ahead_frac=1.0`, `trade_fraction=0.35, dominance_ratio=1.5`；完整报表归档于 `.agent-state/research/aleph_10h_real_eval_20261126/`；盘口差分仅作离线因果诊断增强，正式策略选型仍需补充真实逐笔主动成交与盘口队列证据）：
  - **`selection_dev`（`w13..w16` BTC/ETH，全部 `8/8 cont=true`）**：彻底消除成交量坍塌伪相关（`net ~ ntl` 回归 **`R² = 0.2487`**, 斜率 `-1.1005 bp`）；全部 `7/7` 臂通过全市场 `±30%` 名义额可比门限（`0.868×` 至 `1.146×`），累计推断单边扫盘 `662,833` 买 + `664,613` 卖：
    - `S1_base`（基线）：`81,639` cyc, `-$2,198.05`, `$14,159,667`, **`-1.552 bp`**, `mo1s -1.020 bp`, 撤单 `2,035,714`（成交通道 `pt=39,093 / sc=91,285 / st=1,654`）
    - `S1_db05`（`reprice_deadband_bps=0.5`）：`86,983` cyc, `-$2,343.89`, `$15,862,878`（`1.120×` 可比）, **`-1.478 bp`**, `mo1s -0.988 bp`, 撤单 `469,122`（**-77.0%**，`pt=44,593 / sc=104,709 / st=1,819`）
    - `S1_leadlag`：`89,151` cyc, `-$2,463.74`, `$16,221,046`（`1.146×` 可比）, **`-1.519 bp`**, `mo1s -0.972 bp`, 撤单 `491,570`（`pt=47,234 / sc=104,846 / st=1,787`）
    - `S1_markout`（分边 markout）：`63,801` cyc, `-$2,396.14`, `$12,287,921`（`0.868×` 可比）, **`-1.950 bp`**, `mo1s -0.889 bp`, 撤单 `402,612`（`pt=29,482 / sc=68,095 / st=946`）
    - `S1_volmom`：`78,695` cyc, `-$3,096.18`, `$14,179,075`（`1.001×` 可比）, **`-2.184 bp`**, `mo1s -0.751 bp`, 撤单 `423,647`（`pt=39,478 / sc=74,367 / st=1,293`）
    - `S1_combined`：`67,189` cyc, `-$3,055.46`, `$12,311,884`（`0.870×` 可比）, **`-2.482 bp`**, `mo1s -0.700 bp`, 撤单 `437,344`（`pt=32,219 / sc=61,155 / st=1,075`）
    - `S1_touch_tight`（`decay_k=6.0, fee_floor=0.05`）：`99,711` cyc, `-$4,739.63`, `$14,280,696`（`1.009×` 可比）, **`-3.319 bp`**, `mo1s -0.764 bp`, 撤单 `97,013`（`pt=56,625 / sc=70,540 / st=1,930`）
  - **`holdout`（`w21/w23` BTC/ETH，全部 `4/4 cont=true`）**：`net ~ ntl` 回归 **`R² = 0.0131`**，推断扫盘 `362,313` 买 + `374,210` 卖：
    - `S1_base`：`18,186` cyc, `-$743.12`, `$4,755,187`, **`-1.563 bp`**, `mo1s -0.446 bp`, 撤单 `1,062,156`（`pt=6,474 / sc=23,150 / st=464`）
    - `S1_db05`：`25,505` cyc, `-$819.41`, `$6,510,754`（`1.369×`, 超出 `±30%` 门限）, **`-1.259 bp`**, `mo1s -0.408 bp`, 撤单 `201,385`（**-81.0%**，`pt=8,895 / sc=33,490 / st=626`）
    - `S1_leadlag`：`27,968` cyc, `-$900.92`, `$6,973,822`（`1.467×`, 超出 `±30%`）, **`-1.292 bp`**, `mo1s -0.429 bp`, 撤单 `216,894`（`pt=10,838 / sc=35,129 / st=621`）
    - `S1_markout`：`25,524` cyc, `-$688.45`, `$6,546,930`（`1.377×`, 超出 `±30%`）, **`-1.052 bp`**, `mo1s -0.365 bp`, 撤单 `209,249`（`pt=8,851 / sc=34,882 / st=668`）
    - `S1_volmom`：`35,039` cyc, `-$1,334.57`, `$7,213,758`（`1.517×`, 超出 `±30%`）, **`-1.850 bp`**, `mo1s -0.437 bp`, 撤单 `218,791`（`pt=11,465 / sc=37,060 / st=516`）
    - `S1_combined`：`31,540` cyc, `-$1,492.25`, `$6,119,682`（`1.287×` 可比）, **`-2.438 bp`**, `mo1s -0.444 bp`, 撤单 `203,336`（`pt=9,912 / sc=29,851 / st=455`）
    - `S1_touch_tight`：`38,724` cyc, `-$2,102.78`, `$5,227,955`（`1.099×` 可比）, **`-4.022 bp`**, `mo1s -0.522 bp`, 撤单 `21,789`（`pt=15,345 / sc=26,820 / st=631`）
  - **P1 工程与策略结论**：盘口差分扫盘与排队优先级链路验证有效，但在零 Maker 返佣（`0 bps`）与 `3.5 bps` IOC 出场口径下全 `7` 臂仍为负单位收益（`-1.052 bp` 至 `-4.022 bp`）。`S1_db05` 稳健减少撤单 `77.0%–81.0%` 且在可比 `selection_dev` 小幅改善 `-1.552 -> -1.478 bp`；相反 `S1_markout` / `S1_volmom` / `S1_combined` 虽压低 `1s markout`（`-1.020 -> -0.700 bp`），却因过度加宽与频繁触发 `hazard_ioc` 推高实际实现亏损（`-2.482 bp` sel / `-2.438 bp` holdout），`S1_touch_tight` 贴盘过深同样扩大穿单失血。结论：做市防守全量组合目前**不达标**，解冻正现金评估仍需引入真实成交回执流与非零 Maker 返佣档位重新标定。
- **P3 纯净 `2×4` Scenario 3 OFAT 验证**（固定 `persist_ms=200, entry_bp=8.0, exit_bp=0.0`，费率严格取磁带头，`max_notional_usd = 2 × clip_usd`，仅正交扫描 `clip_usd ∈ {300, 500}` × `spread_cost_entry_coeff ∈ {0.0, 0.15, 0.25, 0.35}`）：
  - **`selection_dev`（全 `11` 窗含非连续 `w14` [`gap = 106,375 ms`] 与严格连续 `9/11` 窗 [`w03 NVDA`、`w05 NVDA/AMZN` 在 `entry_bp=8.0` 且 `2.25+0.90 bps` 费率下 `0` 成交，全部往返来自 BTC/ETH]）**：两档 clip 的往返单调递减、`net/rt` 单调递增、`net/fee` 单调递增 **3 项检验全部通过**（`thin=False`）：
    - **严格连续 `9/11`（`6/9` 胜）**：
      - `OFAT300`（`sc000 -> sc015 -> sc025 -> sc035`）：`rt 115 -> 108 -> 101 -> 95`，净现 `+$7.8983 -> +$8.3097 -> +$8.5168 -> +$8.5065`，**`net/rt` `$0.0687 -> $0.0769 -> $0.0843 -> $0.0895`（`+30.3%`）**，`net/fee 0.2362 -> 0.2679 -> 0.2952 -> 0.3123`，`net/ntl +0.531 -> +0.603 -> +0.664 -> +0.703 bp`。
      - `OFAT500`：`rt 115 -> 108 -> 101 -> 95`，净现 `+$13.1671 -> +$13.8526 -> +$14.1978 -> +$14.1811`，**`net/rt` `$0.1145 -> $0.1283 -> $0.1406 -> $0.1493`**，`net/fee 0.2362 -> 0.3123`，`net/ntl +0.531 -> +0.703 bp`（`$300/$500` 在 BTC/ETH 薄档可吃满区内呈同比例放大）。
    - **全 `11/11`（`8/11` 胜，含非连续 `w14`）**：同样 100% 单调：`OFAT300` `rt 265 -> 250 -> 232 -> 222`、`+$20.8724 -> +$22.5380 -> +$22.4547 -> +$22.6296`、**`net/rt` `$0.0788 -> $0.0902 -> $0.0968 -> $0.1019`**、`net/fee 0.2735 -> 0.3556`、`net/ntl +0.615 -> +0.800 bp`；`OFAT500` `+$34.7947 -> +$37.7241`、`net/rt $0.1313 -> $0.1699`。
  - **`holdout`（`w21/w23` BTC/ETH，全部 `4/4 cont=true`，全部达到 `42–49 rt`，已消除 `< 30 rt` 薄样本告警）**：两档 `clip` 在未触碰留出窗上 **100% 复现三条单调性**：
    - `OFAT300`（`4/4` 胜）：`rt 49 -> 44 -> 43 -> 42`，净现 `+$1.9089 -> +$1.8860 -> +$1.8933 -> +$2.0682`，**`net/rt` `$0.0390 -> $0.0429 -> $0.0440 -> $0.0492`（`+26.2%`）**，**`net/fee` `0.1287 -> 0.1399 -> 0.1463 -> 0.1632`**，**`net/ntl` `+0.290 -> +0.315 -> +0.329 -> +0.367 bp`**。
    - `OFAT500`（`4/4` 胜）：`rt 49 -> 44 -> 43 -> 42`，净现 `+$3.1817 -> +$3.1435 -> +$3.1561 -> +$3.4475`，**`net/rt` `$0.0649 -> $0.0714 -> $0.0734 -> $0.0821`**，`net/fee 0.1287 -> 0.1632`，`net/ntl +0.290 -> +0.367 bp`。
    - 分标的细部披露：ETH 上行单调平稳（`OFAT300 net/rt $0.0506 -> $0.0669`, `net/ntl +0.401 -> +0.523 bp`），BTC 上 `sc025` 存在轻微波动（`$0.0357 -> $0.0346 -> $0.0395`），整体以 **`spread_cost_entry_coeff = 0.25–0.35`** 兼顾过滤与成交；RWA `NVDA/AMZN` 需独立校准入场阈值而非直接沿用 crypto `8.0 bp`。

### 4.5 三条实盘 / Canary 轨道维持 `HOLD` 的客观放行条件

- **`lighter_inventory_mm`（Core / RH）**：维持 `HOLD`，需先在独立账户与实盘/录制验证中消除零返佣磨损（`S12` 实盘回放与正净利留存门控通过）。
- **`relative_value`**：维持 `HOLD`，须等待美股 RTH 时段 Step 2 Gate 采集及跨所 Oracle/Funding OOS 验证全部放行。
- **`arcus_xyz_basis`**：维持 `HOLD`；虽然本轮已补齐 Growth Mode、Oracle 偏离、ADL 断路器与 SHM 读取保护，且默认示例配置保持 `dry_run = true`，实盘启动仍严格要求完成周一 RTH Step 2 Gate 检查，并在运维窗口内显式传入 `--canary --confirm-live-canary` 授权。
