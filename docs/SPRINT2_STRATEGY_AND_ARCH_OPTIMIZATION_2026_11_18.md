# AlephTX 实证归因、业界顶级策略对照与 Sprint 2 架构/策略优化报告

> **状态说明**：本报告基于截至 **2026-11-18** 的本机源码、`aleph` 只读实证结果（`latency_matched_v1`、`native_cadence_v1`、`fair_value_shadow_v1`、`fair_value_execution_v1`、`quote_distance_v1`、`carry_rate_screen_v2`、`zero_reward_hist_v1`）与业界微观结构文献交叉分析形成。**三大赛道仍维持 HOLD；未经单独授权禁止实盘下单、撤单、重启或部署。**

---

## 一、 `aleph` 实证为什么亏钱：从数据中提炼的核心微结构病灶

过去 24 小时对数百组连续 10h 因果回放与跨市场样本的拆解（见 `.agent-state/research/LATENCY_AND_CASH_2026_11_18.md`、`.agent-state/research/ZERO_REWARD_MM_2026_11_18.md`、`.agent-state/research/STRATEGY_VALIDATION_2026_11_18.md`）暴露了三个此前被“平均净值”或“100ms 粗糙采样”掩盖的一阶事实：

### 1.1 赛道一（`01_quant_profit` / Core 做市与方向量化）的三大结构性失血点

| 实证实验 | 关键观测数据 | 真实微结构病灶 |
| --- | --- | --- |
| **同带时延 × 盘口分辨率**<br>(`592 + 128` 组) | `100ms` 网格降时延（`250ms→10ms`）使 `$500` 做市亏损从 `-$53.92` (`$366.15/M`) 降至 `-$39.31` (`$310.09/M`)；**但还原网格间真实盘口更新后方向反转**：`250ms` 为 `-$42.59` (`$272.76/M`)，而 `10ms` 恶化为 `-$45.82` (`$334.45/M`)。`100ms` 现金三项全负：1s markout `-$16.86`、吃单执行 `-$6.00`、持仓漂移 `-$10.46`、开销 `-$6.00`。 | 1. **Lighter Standard 存在不对称 `300ms` 撤单/吃单减速带（`cancel_delay = 300ms`）**：更灵敏的盘口追踪在缺乏撤单保护与队列粘性时，反而频繁撤掉队首健康挂单并重挂在更脆弱的价格前沿，放大了在途被狙击暴露。<br>2. **回放局部公允价与生产未同核**：`ShrunkImbalance`/`TouchMicro` 仅存在于 `research-replay` 的 `recorded/local_fair_value.rs`，生产主循环 `InventoryNeutralMM` 在无外部参考时缺少本地微价、本地 BBO 订单流与撤单期权缓冲。 |
| **外部参考新鲜度 × 报价距离**<br>(`80 + 48` 组) | 8 窗 `2,886,000` 个网格帧中，Hyperliquid 满足 `250ms` 新鲜度仅 `41,997` 帧（`1.455%`），加 `50ms` 后仅余 `7,266` 帧（`0.252%`）；`3bp` 窄档把 8 窗挂单量推至 `$3.2785M`，但 **8 窗全亏**（`-$85.38/M`）。 | 1. **跨所参考长期盲区**：外部参考缺席 `98.5%+` 帧时段时，原先 `order_flow_reserve` 与 `markout_reserve` 直接归零休眠。<br>2. **唯成交量窄档陷阱**：收窄报价下限只增加了“高成交概率 = 高逆向选择”的毒单成交占比（Albers 等 2025 实证结论），单位成本不降反升。 |
| **原生微价 vs. `shrunk` 分桶**<br>(`72` 组原生回放) | 两笔库存上限（`$1,000` max / `$500` urgency）+ Stoikov **最佳档数量加权微价 (`TouchMicro`)**：全 8 窗亏幅收窄至 `-$16.93`（`229.56/M`），后 4 窗净盈 `+$1.04`（吃单 `+1bp` 压力下 `-$0.416`）；同组下量化 `TouchMicro` 后 4 窗 `+$0.037`（压力后 `+$0.011`）。相比之下 45 桶 `shrunk` 在 8 窗亏 `-$20.16`（base `-$47.39`），且 BTC MSE 未改善。 | 1. `shrunk` 以 `cur → cur+1010ms` 训练（混入 `10ms` 发送前不可交易时段），且五月/七月只有两个完整日期，45 个硬边界分桶在尾部稀疏桶引入估计方差。<br>2. **`LocalReversalFilter` 静态阈值过稀**：东京 W05 仅 1 笔成交，高/低波动切换期无法按实时实现了波动率自适应缩放。 |

### 1.2 赛道二（`02_hedge_arbitrage`）与赛道三（`03_incentive_turnover`）

1. **双吃单（Dual-IOC）为什么 80 组全部零成交**：补充真实 Coinbase 参考与 `$100/leg` 资金后，在 Core/RH 与 Core/HL 的延迟（`300ms` taker 减速带）、双边价差与费用叠加约束下，可无风险击穿的可执行价差机会为零；只有“低毒挂单先行 + 瞬时保护性对冲（Maker-First / XEMM）”与“严格时段/基差感知的多结算周期套利”具备正期望路径（东京 RWA SPY/QQQ 闭合循环在开销后各录得 `+$0.1393` / `+$0.0648`）。
2. **资金费套利 524 窗**：仅 XRP 有微弱毛利（13 次触发 `+0.60bp`，但对摩擦极度敏感，加 `4bp` 即转负至 `-51.40bp`）；说明**双腿都走被动或低磨损被动平仓**是压缩进出场基差与手续费损耗的唯一通道。
3. **激励刷量（`03_incentive_turnover`）**：`scripts/audit_lighter_program_scope.py` 已证实 Core Standard 单腿 `$500` 对 RH 活动两窗合格量为 `$0` 且不满足 LPP 资格。在无明确账号白名单与实测积分兑现前，必须坚持“零奖励现金亏损收敛至 `$0..<$2/M` + 严格仓位上限（2 笔）+ 高耐心被动平仓”。

---

## 二、 业界最佳实践对标与 Sprint 2 核心落地改进

结合 Stoikov (2018) Microprice、Cont–Kukanov–Stoikov (2014) Order Book Imbalance、Cartea–Donnelly–Jaimungal 逆向选择与存货最优控制、Albers 等 (2025, `arXiv:2502.18625`) 永续合约做市“成交概率 vs. 成交后收益”悖论以及欧交所（Eurex PLP）/ 减速带做市保护机制，本轮 Sprint 2 将实证有效的改进直接下沉至**生产与回放同核**与**执行风控边界**（全部新策略开关采用 `#[serde(default)]` 默认关闭，**零破坏既有冻结实验与控制基线逐字节复现**）：

### 2.1 `Opt S2-1 & S2-2`：生产 + 回放同核微价、本地毒流/减速带期权保护与挂单队列粘性
- **[src/config.rs](../src/config.rs) / [market_state.rs](../src/strategy/inventory_neutral_mm/market_state.rs) / [inventory_neutral_mm.rs](../src/strategy/inventory_neutral_mm.rs)**：
  1. **同核本地公允价 (`LocalFairValueConfig`: `Mid` / `TouchMicro` / `DepthMicro`)**：在无外部参考或本地模式时，生产 `InventoryNeutralMM::calculate_pricing_inputs` 可直接启用 Stoikov L1 数量加权微价（限幅 `±max_shift_bps`，默认 `2.0 bps`）并标注证据来源 `"lighter_touch_micro"`；同时在 [deploy/oasis/telemetry_store.py](../deploy/oasis/telemetry_store.py) 的 `MARK_SOURCES` 注册该来源以便监控台精确溯源。
  2. **本地毒流 + 撤单减速带看跌/看涨期权储备 (`LocalToxicityConfig`)**：
     - **本地 BBO 订单流失衡 (`ofi_half_life_ms`, `ofi_max_widen_bps`)**：复用经过审计的 `OrderFlow` 指数衰减估计，在外部参考断连/超时（`98.5%+` 非新鲜时段）时依然仅向**加仓危险侧**扩宽报价；
     - **撤单减速带期权定价 (`cancel_speed_bump_ms`)**：将 Lighter Standard 端到端撤单延迟 $\tau_{\text{cancel}}$（如 `310 ms`）的自由选择权风险显式量化为 $\text{mult}\cdot\sigma/\sqrt{\text{s}}\cdot\sqrt{\tau_{\text{cancel}}}$（默认理论 Bachelier 系数 `0.4`），**仅作用于 `q >= 0` 的买侧与 `q <= 0` 的卖侧（加仓方向）**，绝不阻碍减仓平仓侧队列；
     - **本地微价事后 Markout (`enable_local_markout`)**：启用时使用因果 `local_touch_micro_price` 而非静止中间价作为 `MarkoutTracker` 锚点，让 `markout_reserve` 在单场所 Core 模式下也能工作。
  3. **入场队列优先级滞回 (`QuoteHysteresisConfig.entry_requote_threshold_bps`)**：在不突破 `cancel_bounds` 安全利润底线的前提下，仅对**非减仓方向**放宽改单阈值，保住已有 FIFO 队首优先级并抑制 `300ms` 撤改路径下的无谓撤单重挂；减仓方向继续保持紧跟踪与 `breakeven_exit_bps` 被动退出。

### 2.2 `Opt S2-3`：波动率自适应逆向反转门控（`LocalReversalFilter`）
- **[src/strategy/inventory_neutral_mm/replay/recorded/local_signal.rs](../src/strategy/inventory_neutral_mm/replay/recorded/local_signal.rs)**：
  - 新增可选字段 `vol_shock_mult`、`vol_recovery_mult`、`min_shock_bps_floor`。
  - 利用环状缓冲区内严格的因果历史桶（`book_at <= at`）实时计算过去 6.4s 的 1 秒滚动实现波动率 RMS $\sigma_{1\text{s}}$（要求至少 5 个有效因果样本，否则 fail-closed），将冲击与回撤门槛自适应缩放为 $\text{clamp}(k\cdot\sigma_{1\text{s}}, \text{floor}, \text{shock\_bps})$，解决静态阈值在高波动误伤或低波动全年零成交的两难问题。

### 2.3 `Opt S2-4`：执行边界与跨所对冲因果安全加固（Codex 审阅闭环 + `EXEC-H5/H6`）
1. **Codex 审阅两项 P0 闭环**：
   - [scripts/flywheel_sync.py](../scripts/flywheel_sync.py)：将 `ship_public_bronze` 中的 `FileNotFoundError` 捕获范围严格收缩至本地待封存/待淘汰采集文件自身并发消失场景，禁止吞掉 `rclone` 等上传子进程缺失或远端校验报错，确保缺执行程序时显式报错重试。
   - [src/exchanges/lighter/trading.rs](../src/exchanges/lighter/trading.rs) & [recovery.rs](../src/exchanges/lighter/trading/recovery.rs)：`ensure_reduce_only_valid(..., None)` 在 `order_tracker` 缺失或处于 `needs_reconciliation()` 时严格 **fail-closed 拒绝**；紧急恢复平仓 `RecoveryMode::Flatten` 改为显式传入 REST 核实持仓证据 `place_order_with_proof(..., Some(pos_f64))`；`execute_batch` 将全部订单归一化、`ensure_can_place` 与**同批次累计 `reduce_only` 总量校验**提前至 `lane.get_nonce(...)` 与签名前执行，并通过环回 HTTP 计数回归验证“拒绝时零网络请求”。
   - [local_fair_value.rs](../src/strategy/inventory_neutral_mm/replay/recorded/local_fair_value.rs)：在 `Signal::book` 增加 `bid_size`/`ask_size`/5 档深度非负有限性及源码时钟倒流重置防护，杜绝 `0/0 = NaN` 经 `partition_point` 静默落入 `[0][0][0]` 极端桶的风险。
2. **对冲腿残余死角清理（`EXEC-H5` / `EXEC-H6`）**：
   - [src/live_hedge.rs](../src/live_hedge.rs)：`Observation` 与 `PendingIoc` 记录发单前远端清算所快照时间 `pre_dispatch_account_time_ms`；当本地墙钟慢于远端 `state.time` 时，即使同步 `response` 以 `sent_at_ms` 记账，也强制等待远端账户快照严格推进后才允许释放 IOC 敞口，同时完全保留 `audit.rs` 对 `terminal_at_ms == submitted_at_ms` 的账本不变量。
   - 新增共享校验 `validate_preflight_deadband`，强制 Hyperliquid `deadband_base` 在非扁平区间同时覆盖 `1 lot` 与 `$10` 最小名义额（不再放行 `deadband_base == 0`），根除小额残仓触发 `MAX_BLOCKED` 崩溃。

### 2.4 `Opt S2-5`：飞轮数据与监控台全量会话契约一致（`MON-H1`）
- [deploy/oasis/telemetry_store.py](../deploy/oasis/telemetry_store.py)：抽出 `_enrich_session` 并应用到 `TelemetryStore.sessions()`，使 `/api/sessions` 与 `/api/summary?session=` 返回完全一致的 `total_pnl`、`combined_pnl`、`equity_change`、`open_orders` 与 `valuation` 质量标签（缺失估值严格保留 `null`），彻底消除控制台多会话矩阵“含浮盈总计”整列显示 `—` 的盲区。

---

## 三、 对 Codex 新增代码的独立双边复核意见

1. **`local_fair_value.rs` 报价时钟与因果性**：
   - 核查结论：**未发现前视偏差（Lookahead Free）**。`recorded.rs:1553` 按 `now_ms()` 在延迟发布后入队 `Signal::book`，`inventory_neutral_mm.rs:2064` 使用 `now_ns / 1_000_000` 查询，`Signal::price` 检查 `o.at <= now && now - o.source <= 250` 且 `ShrunkImbalance` 的回望步 `past.at <= now - 1000 && at - past.source <= 250` 不做线性插值。**此前 72 组原生执行实验全部有效，无需重跑。**（唯一潜在隐患是零/NaN 挂单量与源时间戳倒流，已在本次同步加固。）
2. **`fair_value_shadow_v1` 日内训练切分与标签对齐**：
   - 标签使用 `cur → cur+1010ms`（包含 `10ms` 单程网络飞行），而评估另列 `post = arr → arr+1000ms`；两个训练集（May/Jul）各只覆盖 1 个日内窗口且同日 5m 块高度相关，建议后续若再迭代统计模型，直接以 `arr → arr+1000ms`（到达后收益）及“挂单成交流后 1s 逆向标记”为训练目标，以对齐真实挂单被吃时的条件分布。
3. **`LATENCY_AND_CASH_2026_11_18.md` 单位成交额分母**：
   - 经在 `aleph` 对 `fair_value_execution_v1/scores.json` 与 `score_zero_reward_maker.py` 逐组复算确认：`每百万挂单成本` 为 `-sum(net) * 1e6 / sum(maker)`、`每百万总成交成本` 为 `-sum(net) * 1e6 / sum(maker + taker)`，完全自洽。需提醒：当过滤器把挂单量从 `$126k` 压至 `$14.5k` 同时净现金扣除双边总额 `0.25 bps` 固定税时，单百万比率对微弱分子变化极敏感（即比率幻觉），后续晋级评审建议固定“单位净现金、总成交额分母成本、挂单分母成本与资金年化收益率”四元并列。
4. **`scripts/live_attribution.py`（Schema v2）**：
   - `summarize_fill_metric` 对缺失/非正权重与 partial-fill 拆分的加权不变性处理严谨，建议未来在跨洋抓包场景增加 `alignment_clock="receipt"` 可选模式，避免远端 `exchange_ts_ns` 与东京 `available_at_ms` 混比引入跨洋时钟偏差。

---

## 四、 后续验证矩阵建议（不扰动冻结 `W07–W16`）

1. 在后续独立离线研究队列（如 `quality_windows_v1` 增量连续 10h 窗）上，围绕两笔库存基线做最小正交 4 臂消融：
   - **Ctrl**：`TouchMicro` + `two_clips`（既有最佳对照）；
   - **Arm A**：`Ctrl` + `LocalToxicityConfig`（`ofi_half_life_ms=500, ofi_max_widen_bps=3.0, cancel_speed_bump_ms=310, cancel_option_mult=0.4, enable_local_markout=true`）；
   - **Arm B**：`Arm A` + `entry_requote_threshold_bps = 4.0`（队列粘性滞回）；
   - **Arm C（Quant 01）**：`LocalReversalFilter` + `vol_shock_mult = 2.0, min_shock_bps_floor = 0.8`，检验在不增加强退吃单的前提下能否突破“东京无成交 / 静止全亏”瓶颈。
2. `02_hedge_arbitrage` 在获得高完整度连续流后，优先验证 **Passive-Entry + Passive-Exit 资金费差** 与 **`hedge_protection_bps` + 多档 depth-walk 对冲**，而非已证伪的同步 Dual-IOC。
