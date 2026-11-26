# AlephTX 全面复核、三大赛道解耦升级与全交易所 10 小时连续实证报告（2026-11-26）

> **安全与范围声明（`AGENTS.md`）**：本报告全部测试、回放与湖仓统计均在 **`HOLD`（零实盘下单、零撤单、零服务重启）** 约束下完成；所有策略状态机保持 **100% 无 `crate::exchanges::*` 耦合**，三大赛道严格遵循**非交叉补贴契约**（独立配置、独立风控、独立账本、独立报告）。本文不含任何私钥、地址、签名或账户原始流水。

---

## 一、全面工作区复核与工程硬化（Review & Fixes）

在推进跨所升级前，先对当前工作区全量未提交代码与同行协同审计条目（含 `AGENT_HANDOFF.md` 第 118–119 轮及 2026-11-26 V3 东京实盘微校准复盘）逐项核验并修补以下 5 类关键问题：

| 审查发现 | 根因与风险 | 本轮修复方案（代码与单测） |
| --- | --- | --- |
| **1. `Cancel.await` 后盘口陈旧仍发 `PlaceAlo`** | 慢速撤单等待后（如 Entropy `~695 ms`），本地 `tgt` 盘口可能已超 `max_book_age_ms` | 在 [`venue_mm_calibrator.rs`](../src/bin/venue_mm_calibrator.rs) 双循环 `for act in actions` 的 `PlaceAlo` 分支前统一加入 `is_book_fresh_for_place(b.received_at_ms, unix_ms(), engine.config().max_book_age_ms)`，附回归单测 |
| **2. Arcus 停机 `cancelAll` 无挂单误触发 5/5 重试告警** | Arcus 零挂单场景 `/v1/cancelAllOrders` 返回态不匹配 `OrderReceipt::confirm_cancel` 的 `200 + success/cancelled` 严格断言；且不能放宽 `rest.rs` 供 `arcus_xyz_basis` 使用的 `202` 拒认契约 | 保持 [`arcus/rest.rs`](../src/exchanges/arcus/rest.rs) `confirm_cancel` 严格契约不动；在 [`venue_mm_calibrator.rs`](../src/bin/venue_mm_calibrator.rs) 停机与 `--flatten-only` 改用 `arcus_clean_cancel_all_market`（`cancel_all` + 查询 `open_orders` 并对残余逐单 `cancel_by_client_id`/`cancel_by_order_id` 复验空仓） |
| **3. `VenueRegistry::default()` 漏注册 `backpack`** | [`src/venues.rs`](../src/venues.rs) 已在 `audited_onboarding_profiles()` 收录 12 个场所，但 `VenueRegistry::default()` 缺 `backpack`，导致快照/域校验不一致 | 在 [`src/venues.rs`](../src/venues.rs) 补齐 `("backpack", "backpack", "backpack", None, ReadOnly, LimitOrderBook)`，同时保持 `OPT-01` 对未认证费档的 fail-closed 拒绝 |
| **4. 仿真器 `sim_queue_priority_mm.py` 与 V3 实盘/Rust 引擎口径差** | Gemini #119 指出 `E_v3` `min_edge_bps=-100.0`、缺 `MarkoutShield` / weighted OBI、及仅打印 `equity_usd` 而未直接汇总合并盯市 `equity_usd + terminal_basis_usd` | 升级 [`sim_queue_priority_mm.py`](../scripts/sim_queue_priority_mm.py)（附 [`test_sim_queue_priority_mm.py`](../scripts/test_sim_queue_priority_mm.py) `7/7`）：对齐 `min_edge_bps`、`pending_fills` 因果 1s markout shield、3 档 weighted OBI、主动 ALO 防撞保护，并显式输出 `combined_mtm_usd` 与 `combined_cost_per_m_usd` |
| **5. 低频清淡窗 `ReferenceLeadValidator` 12 点暖机过慢** | 180s `SNDK-USD` 实盘结束时 `vrefs=0` 致 `bid_markout=-3.72 bp` 未获外部领跑保护；高波动窄点差 56–65% ALO 拒单 | 在 [`queue_priority_mm.rs`](../src/strategy/queue_priority_mm.rs)（`19/19`）将 `for_profile` 加速至 **5 样本因果验证**（`1_500ms / 0.35bp / min_samples=5`），仍自动静默 `ANT` 等滞后参考源；新增 `alo_flight_guard`（`sigma` + OBI）在不打断 `Keep` 和 `reduce_only` 的前提下主动抑制窄盘追价拒单 |

> [!NOTE]
> **冻结与隔离检查**：[`src/strategy/inventory_neutral_mm.rs`](../src/strategy/inventory_neutral_mm.rs)（SHA-256 `ce0930d3...`）、`Cargo.lock` 与同行在途 6 个未提交修改文件均未改动；`src/strategy/` 下 `rg "crate::exchanges"` 输出为空（`ZERO_EXCHANGE_COUPLING=OK`）。

---

## 二、`aleph` 湖仓全交易所数据盘点与可用性矩阵

对 `aleph` 只读 parquet 分区与 `lakehouse.duckdb` 的全量扫描显示，东京节点除 Arcus / Entropy 外还长期连续采集了 **Lighter Robinhood、Lighter Core、Extended、TradeXYZ、Bulk、Hyperliquid Core、Binance、Coinbase**：

| 交易所 (`venue`) | 东京已录 BBO 行数 | 东京已录逐笔 (`v_public_trades`) | 资金费率 (`funding_and_ctx`) | 10h 连续覆盖与信号形态 | 费用核准边界 (`OPT-01`) |
| --- | ---: | ---: | --- | --- | --- |
| **`arcus`** | 57.13M (21 市场) | 2.29M (`$1.85B`) | 无（以基差/点差为主） | WS L2 + 逐笔，10.25h 连续 | **已核准**（RWA `0/1.15–2.25 bp`，Crypto `0–1.5/2.3–4.5 bp`） |
| **`lighter_robinhood`** | 119.65M (18 市场) | 7.11M (`$4.43B`) | 有（~3.2 万条/10h） | WS L2 + 逐笔，10.25h 连续；`200ms` 速度缓冲 | **Standard `0/0 bp`（`USDG`）**；Prem/Plus 已入表 |
| **`lighter`** Core | 142.50M (19 市场) | 12.05M (`$18.13B`) | 有（~3.2 万条/10h） | WS L2 + 逐笔，10.25h 连续；`0/300ms` 速度缓冲 | **Standard `0/0 bp`（`USDC`）**；Prem/Plus 已入表 |
| **`extended`** | 20.24M (3 Crypto) | 0.45M (`$0.39B`) | 有 | WS L2 + 逐笔，10.25h 连续（`BTC/ETH/SOL`） | 需账户级费档证据（模拟按 `0.0 bp` 保守参考） |
| **`bulk`** | 64.55M (14 市场) | **0 行（缺 `-bulk-trades`）** | 无 | **独立 `tokyo-bulk-*` 活动** WS L2，10.25h 连续 | 需账户级证据；**Tracker-3 仅作 `book_diff_diagnostic` 口径** |
| **`trade_xyz`** | 0.97M (18 RWA) | 7.61M (`$16.33B`) | 有 | **15s REST 轮询 BBO** + 逐笔 | `0.30/0.90 bp` HIP-3 Growth Mode，需逐资产证明 |
| **`entropy`** | 0.32M (6 RWA) | 0.63M (`$0.34B`) | 有 | **15s REST 轮询 BBO** + 逐笔 | `0.30/0.90 bp` HIP-3 Growth Mode，需逐资产证明 |
| **`hyperliquid`** Core | 0.21M (3 Crypto) | 7.71M (`$54.27B`) | 有 | 15s REST BBO + 逐笔 | 已核准基准 `1.50/4.50 bp` |
| **`binance` / `coinbase`** | 3.69B / 37.57M | 无 | 无 | 极高速 WS 外部参考锚（`BTC/ETH/SOL`） | 需账户级 VIP 费档证据（仅作价格/流动性参考锚） |
| **`backpack`** | **暂无湖仓录制** | 无 | 无 | 依赖 [`sim_backpack_dm_guardian.py`](../scripts/sim_backpack_dm_guardian.py) 与 API 对接 | `AccountEvidenceRequired`（100ms 速度缓冲 + DM 加权深度） |

> [!WARNING]
> 本轮交叉比对发现三处关键数据映射与诊断边界：
> 1. **`bulk` 独立录制流**：`bulk` 存储在独立 `tokyo-bulk-*-w19/w24` 活动中（不在 `tokyo-crypto`/`tokyo-rwa` 内），已接入 [`eval_multivenue_three_tracks.py`](../scripts/eval_multivenue_three_tracks.py)；由于缺 `-bulk-trades`，其 Track 3 结果严格标注为 `book_diff_diagnostic`。
> 2. **`lighter_robinhood` 与 `trade_xyz` 标识区分**：`lighter_robinhood`（`1=BTC, 0=ETH, 15=NVDA, 25=QQQ, 26=SPY`）与 `lighter` Core（`1=BTC, 0=ETH, 110=NVDA, 128=SPY, 129=QQQ`）拥有完全不同的 `market_id` 空间和结算稳定币（`USDG` vs `USDC`）；`xyz:SP500 (~7134)` 与 `xyz:XYZ100 (~25720)` 分别是 `SPX/NDX` 指数倍标而非 ETF `SPY/QQQ (~711/626)`。
> 3. **`trade_xyz` / `entropy` 为 15s 轮询 BBO**：Track 3 模拟标注 `polled_bbo_diagnostic`，且在 `Arcus SNDK-USD` 的高频 WS 对照中，15s 滞后流会被 `ReferenceLeadValidator` 正确静默。

---

## 三、赛道一 (`01_quant_profit`)：策略工程升级与多所微观结构实证

### 3.1 源码级核心升级（针对 P1 连续 10 小时回放实证痛点）

针对 [2026-11-25 蓝图 §4.4](STRATEGY_AND_ENGINEERING_OPTIMIZATION_BLUEPRINT_2026_11_25.md) 中 `S1_markout`/`S1_volmom` 虽将 `1s markout` 从 `-1.020 bp` 压降至 `-0.700 bp` 却因**过度加宽挂至 L2 + 频繁触发 `3.5 bp` Taker `hazard_ioc`** 导致净现金反而恶化至 `-2.482 bp` 的病灶，落地两项**默认关闭（零冻结漂移）、可独立消融**的改进：

1. **分边开仓毒流否决（[`quote_pipeline.rs`](../src/strategy/quote_pipeline.rs)）**：在 `MarkoutShieldConfig` 新增 `adverse_veto_bps: Option<f64>` 与 `MarkoutShield::vetoes_opening_side(side)`；当某侧平滑 markout `<= -veto_bps` 时直接撤销该侧**开仓挂单**（避免被 L2 扫穿套牢），同时保持健康对侧开仓与 `reduce_only` 减仓通道畅通（`quote_pipeline` `10/10` 通过）。
2. **被动优先、灾难兜底的高费 Taker 防爆机制（[`hybrid_exit.rs`](../src/strategy/hybrid_exit.rs)）**：在 `HybridExitConfig` 新增 `passive_touch_before_hazard_ioc: bool` 与 `catastrophe_ioc_unrealized_pnl_bps: Option<f64>`；轻度不利触发时先切入 `Stage3Touch` 零 Taker 费被动跟盘脱身，仅当浮亏穿透灾难阈值或触发 `timeout_ms` 时才动用 Taker IOC（`hybrid_exit` `9/9` 通过）。

### 3.2 跨所 10.25h `spread / 1s_vol` 盈利可行性边界（`W19 selection_dev` vs `W24 holdout`）

由 [`eval_multivenue_three_tracks.py`](../scripts/eval_multivenue_three_tracks.py) 直接在 `aleph` 只读测算的 **10.25 小时连续实证数据**（详见 `.agent-state/research/multivenue_three_tracks_{sel_w19,holdout_w24}_20261126.json` 与 `multivenue_bulk_diag_20261126.json`）明确揭示“为什么普通挂单在多数主流币种难以正期望，而少数利基可为”：

| 交易所 / 标的 | W19 `spread` / `1s_vol` (`sp/v`) | W24 `spread` / `1s_vol` (`sp/v`) | W24 `half_spread - fee` | W24 中位 L1 | 赛道一 (`01_quant_profit`) 客观结论 |
| --- | --- | --- | ---: | ---: | --- |
| **`arcus BTC`** | `0.016 / 1.434` (`0.011`) | `0.017 / 1.511` (`0.011`) | `+0.009 bp` | `$1,247` | **不可盈利**（点差仅波动率 1%） |
| **`bulk BTC`** | `0.002 / 1.414` (`0.001`) | `0.002 / 1.423` (`0.001`) | `+0.001 bp` | `$15,999` | **不可盈利**（极致卷盘口） |
| **`extended BTC`** | `0.163 / 1.279` (`0.127`) | `0.171 / 1.334` (`0.128`) | `+0.085 bp` | `$90,437` | 极深低波动盘口，纯点差不足覆盖 1s 逆选 |
| **`arcus NVDA`** | `0.737 / 4.268` (`0.173`) | `0.682 / 2.305` (`0.296`) | `+0.341 bp` | `$4,038` | 需依赖高频领跑撤单才能守住微利 |
| **`lighter_robinhood SPY`** | `0.425 / 0.736` (`0.578`) | `0.710 / 0.750` (`0.947`) | `+0.355 bp` | `$800` | **低波动 + `0/0` 零费**，接近盈亏平衡前沿 |
| **`lighter_robinhood NVDA`** | `1.493 / 3.806` (`0.392`) | `2.039 / 2.159` (`0.945`) | `+1.019 bp` | `$750` | **`W24` `sp/v ≈ 0.95`**，被动平仓成本低 |
| **`lighter_robinhood BTC / ETH`** | `BTC 0.778 / ETH 1.039` | `BTC 0.968 / ETH 0.913` | `+0.708 / +0.793 bp` | `$963 / $762` | `0/0` 费档下少数 `sp/v ≈ 1.0` 的加密资产 |
| **`arcus DRAM`** | `2.761 / 2.544` (`1.085`) | **`2.980 / 2.536` (`1.175`)** | **`+1.490 bp`** | **`$3,958`** | **唯一 `Arcus WS` 连续双窗 `sp/v > 1.0` RWA** |
| **`entropy SNDK` (`io:SNDK`)** | `2.361 / 3.338` (`0.707`) | **`4.103 / 2.235` (`1.836`)** | **`+1.752 bp`** | `$1,312` | 点差极宽（但受制 L1 `670ms` 下撤延迟） |

---

## 四、赛道二 (`02_hedge_arbitrage`)：路由器升级与多所同步基差/资金费矩阵

### 4.1 工程升级（`src/strategy/cross_venue_router.rs`）

在 [`cross_venue_router.rs`](../src/strategy/cross_venue_router.rs) 新增解耦的 `MakerFirstEdgeEstimate` 与 `evaluate_maker_first_net_edge_bps(maker_cap, hedge_cap, gross_basis_bps, expected_midflight_slippage_bps, min_net_edge_bps)`（`17/17` 通过）：严格从 [`VenueExecutionCapability`](../src/venues.rs) 读取双边费用与结算币种，精确核算 `roundtrip_fee = maker_fee + hedge_taker_fee`、识别 `USDC/USDG` 跨稳定币治理标志 `requires_cross_stablecoin_policy`，彻底杜绝将 `Dual-Taker` 高门槛误套用于 `Maker-First` 或忽略跨币种风险。

### 4.2 全所同步 10.25h 基差偏离与资金费穿门率（`W19` vs `W24`）

| 跨所配对 (`Maker <-> Hedge`) | 结算币种 (`FX`) | W19 `p95` 绝对偏离 (`MF 穿门率`) | W24 `p95` 绝对偏离 (`MF 穿门率`) | W24 `Dual-Taker` 门槛 (`穿门率`) | 策略定性 |
| --- | --- | --- | --- | --- | --- |
| **`arcus <-> lighter_robinhood SPY`** | `USDC/USDG` ⚠️ | `2.276 bp` (`87.45%`) | `1.453 bp` (`63.30%`) | `1.576 bp` (`3.44%`) | `Dual-Taker` 仅 3.4% 机会，**必须切 `Maker-First`** 且挂载 `CrossStablecoinHedgePolicy` |
| **`arcus <-> lighter_robinhood QQQ`** | `USDC/USDG` ⚠️ | `2.922 bp` (`83.10%`) | `2.676 bp` (`76.72%`) | `1.712 bp` (`21.71%`) | 基差中位 `-6.1 bp`，均值回归空间充裕 |
| **`arcus <-> lighter_robinhood NVDA`** | `USDC/USDG` ⚠️ | `6.485 bp` (`72.31%`) | `3.021 bp` (`52.11%`) | `2.510 bp` (`9.29%`) | 高波动 RWA 基差，对冲侧 0 Taker 费优势明显 |
| **`arcus <-> lighter QQQ`** | `USDC/USDC` | `2.577 bp` (`75.41%`) | `2.820 bp` (`68.85%`) | `1.803 bp` (`18.15%`) | 同稳定币零 FX 风险，可优先作为 Track 2 合规基座 |
| **`arcus <-> trade_xyz SNDK`** | `USDC/USDC` | `9.214 bp` (`64.70%`) | `7.771 bp` (`67.71%`) | `3.157 bp` (`31.22%`) | 极大基差（`p99=13.3 bp`），须严格执行 `HYPOTHESIS/OPT-01` Growth Mode 校验 |
| **`arcus <-> bulk NVDA / BTC`** | `USDC/USDC` | `17.69 / 3.92 bp` (`29%`) | `5.37 / 3.00 bp` (`37.5% / 17.4%`) | `4.86 / 4.01 bp` (`9.8% / 1.0%`) | 对冲 Taker 费较高，待 `-bulk-trades` 与账本接入后复算 |
| **`lighter_robinhood <-> lighter BTC`** | `USDG/USDC` ⚠️ | `1.573 bp` (`61.97%`) | `1.469 bp` (`58.17%`) | `1.026 bp` (`12.17%`) | 同族双域低摩擦，但须注意两者同属 `lighter` 风险域，不计入跨风险域对冲 |

*资金费套利摘要（`W24`）*：`lighter MU`（均 `+0.1487 bp/期`，峰 `+0.41`）、`lighter SOL`（`-0.1451 bp/期`，峰 `-0.63`）、`lighter_robinhood BTC`（`+0.1181 bp/期`）与 `lighter_robinhood NVDA`（均 `+0.0886` 峰 `+0.57`）存在显著跨标的/跨期资金费波动，适合纳入 `MultiDayCarry` 同结算时钟连续检验。

> [!NOTE]
> **Arcus 费档敏感性边界**：[`eval_multivenue_three_tracks.py`](../scripts/eval_multivenue_three_tracks.py) 中 `arcus` 股指/RWA Taker 口径采用 `level_4 = 1.15 bp`；若账户处于 [`src/venues.rs`](../src/venues.rs) 默认 `level_0 = 2.25 bp`，则 **`Dual-Taker` 门槛需上调 `+1.10 bp`**，而以 Arcus 作为 Maker（`0.0 bp`）的 **`Maker-First` 门槛与穿门率保持不变**。

---

## 五、赛道三 (`03_incentive_turnover`)：V4 解耦状态机与全交易所 10h 刷量成本前沿

### 5.1 核心策略升级（`QueuePriorityMmEngine` V4 & 4 类物理画像）

在 [`queue_priority_mm.rs`](../src/strategy/queue_priority_mm.rs) 完全无交易所耦合的架构下，本轮将 V3 扩充为 **4 套纯物理画像**：
- `fast_zero_fee_alo`（`36/33 ms`, `0.0 bp`, ALO+DMS — 映射 Arcus 类）
- `speedbump_zero_fee_wide`（`200/200 ms`, `0.0/0.0 bp` — 映射 Lighter Core/RH 类）
- `fast_fee_rebate_clob`（`40/40 ms`, `0.0/2.0 bp`, ALO — 映射 Extended/Bulk/Backpack MM 档）
- `slow_l1_wide_spread`（`670/695 ms`, `0.30/0.90 bp` — 映射 Entropy/HL L1 类）

并落地两项直接由 2026-11-26 东京 V3 实盘触发的战术改进：
1. **5 样本快速因果领跑暖机**（`lead_validation_horizon_ms=1_500`, `min_move=0.35 bp`, `min_samples=5`）：相比 V3（12 样本，180s 清淡窗未暖机完毕），可在 `15–30 s` 内完成有效领跑信号激活，同时继续自动屏蔽 `trade_xyz`（15s 轮询）等滞后参考源。
2. **窄点差主动 ALO 防撞护盾（`alo_flight_guard`）**：仅对**`<= 2 tick` 的新开仓追价**检查飞行波动率 `sigma * sqrt(submit_rtt_ms / 1000) >= alo_flight_guard_sigma_bps` 或逆向 weighted OBI，不破坏已排队老单 `Keep`，也永不阻断 `reduce_only` 减仓。

### 5.2 4 切片真实东京 RTT（`36ms/33ms`）V3 vs V4 本地对照

使用修复后的 [`sim_queue_priority_mm.py`](../scripts/sim_queue_priority_mm.py)（含 `combined_mtm_usd` 与 `min_edge_bps=0.10` 对齐）实跑 `.agent-state/calibration/sim_v4_arcus_tokyo_rtt.json`：

- **`SPY-USD` (`raw-spyqqq`)**：`A` `$8.57/M` (`-$0.1424`)，**`F_v4` 将 ALO 拒单从 `E_v3` `9` 次压至 `3` 次（`-67%`）并提升点差捕获 `+0.0843 -> +0.1405 bp`（净盈亏 `-$0.1911` 优于 `E_v3 -$0.3738`）**；`D` 的历史虚高 `equity_usd (+$11.21)` 被合并盯市 `combined_mtm_usd (+$0.79)` 彻底纠偏。
- **`QQQ-USD` (`raw-spyqqq`)**：`E_v3`/`F_v4` 将 `A/B` 的 `-20 USD` 单腿暴露扭转为可控 `-$1.22 / -$1.11`，`F_v4` ALO 拒单减半（`29 -> 12`，`-59%`）。
- **`ANT-USD` (`raw-rthopen` / `2055`)**：`E_v3` 在 `rthopen` 录得 **`+$4.7376` (`-$130.74/M`)**，`F_v4` 在 `2055` 减少 ALO 拒单（`156 -> 142`）并保持滞后源静默 `vref=0`。

### 5.3 **全交易所连续 10.25 小时（`W19` + `W24`）刷量成本前沿总表**

> [!IMPORTANT]
> **连续性验证**：除 `trade_xyz` W19 含 1 次 104s gap 外，以下所有 `Arcus` / `Lighter RH` / `Lighter Core` / `Extended` / `Bulk` 测试均满足 **`10.25 h`（`36,900,000 ms`）且最大断档 `<= 3.2 s`（`is_continuous_10h = true`）**，按最严 **`combined_mtm_usd`**（合并盯市）口径统计，不计任何未审计返佣或补贴！

| 交易所 / 标的 | 证据口径 | W19 `A` 基线成本 | W19 `E_v3` / `F_v4` 最优成本（10.25h 量） | W24 `A` 基线成本 | W24 `E_v3` / `F_v4` 最优成本（10.25h 量） |
| --- | --- | ---: | ---: | ---: | ---: |
| **`lighter_robinhood SPY`** (`26`) | 10.25h WS+逐笔 | `$9.31/M` (`-$34.48`) | **`-$0.88/M` (`+$1.54` 净赚! `F_v4`, `$1.75M` 量)** | `$5.71/M` (`-$14.00`) | **`$1.26/M` (`B`, `$3.10M`) / `$2.54/M` (`F_v4`, `-$4.11`)** |
| **`lighter_robinhood ETH`** (`0`) | 10.25h WS+逐笔 | `$66.35/M` (`-$61.23`) | **`$5.42/M` (`E_v3`, `-$3.73`, `$0.69M`)** | `$124.05/M` (`-$131.0`) | **`$54.48/M` (`F_v4`, `-$30.96`, `$0.57M`)** |
| **`lighter SPY`** (`128`) | 10.25h WS+逐笔 | `$28.57/M` (`-$31.38`) | **`$14.79/M` (`F_v4`, `-$12.15`, `$0.82M`)** | `$19.36/M` (`-$18.01`) | **`$19.58/M` (`F_v4`, `-$15.34`, `$0.78M`)** |
| **`lighter QQQ`** (`129`) | 10.25h WS+逐笔 | `$43.56/M` (`-$46.68`) | **`$12.40/M` (`F_v4`, `-$10.68`, `$0.86M`)** | `$31.32/M` (`-$30.17`) | **`$26.45/M` (`F_v4`, `-$23.09`, `$0.87M`)** |
| **`lighter_robinhood NVDA`** (`15`) | 10.25h WS+逐笔 | `$93.59/M` (`-$215.97`) | **`$46.52/M` (`F_v4`, `-$62.13`, `$1.34M`)** | `$89.53/M` (`-$124.7`) | **`$51.71/M` (`F_v4`, `-$50.63`, `$0.98M`)** |
| **`extended ETH`** (`ETH-USD`) | 10.25h WS+逐笔 | `$414.09/M` (`-$82.95`) | **`$162.17/M` (`E_v3`) / `$215.80/M` (`F_v4`)** | `$379.42/M` (`-$43.03`) | **`$87.96/M` (`F_v4`, `-$2.03`! 降幅 `-77%`)** |
| **`arcus SPY`** (`SPY-USD`) | 10.25h WS+逐笔 | `$144.02/M` (`-$89.49`) | **`$118.06/M` (`F_v4`, `-$17.50`)** | `$119.10/M` (`-$61.11`) | **`$111.35/M` (`E_v3`) / `$113.86/M` (`F_v4`, `-$19.57`)** |
| **`arcus QQQ`** (`QQQ-USD`) | 10.25h WS+逐笔 | `$139.72/M` (`-$87.64`) | **`$113.05/M` (`E_v3`, `-$29.41`)** | `$194.07/M` (`-$78.27`) | **`$164.75/M` (`F_v4`, `-$29.76`)** |
| **`arcus NVDA`** (`NVDA-USD`) | 10.25h WS+逐笔 | `$425.79/M` (`-$432.15`) | **`$215.94/M` (`F_v4`, `-$73.70`)** | `$243.35/M` (`-$124.9`) | **`$141.46/M` (`E_v3`, `-$45.86`)** |
| **`arcus BTC`** (`BTC-USD`) | 10.25h WS+逐笔 | `$290.59/M` (`-$115.14`) | **`$24.04/M` (`E_v3`, `-$4.02`)** | `$174.49/M` (`-$88.12`) | **`$161.54/M` (`E_v3`, `-$31.56`)** |
| **`extended BTC`** (`BTC-USD`) | 10.25h WS+逐笔 | `$333.62/M` (`-$112.08`) | `$354.21/M` (`F_v4`, 仅 `$43k`) | `$367.89/M` (`-$110.1`) | **`$297.07/M` (`F_v4`, `-$8.39`)** |
| **`bulk SNDK`** (`SNDK-USD`) | ⚠️ `book_diff_diagnostic` | `$37.54/M` (`-$203.8`) | **`$8.02/M` (`F_v4`, `-$26.70`, `$3.33M`)** | `$131.43/M` (`-$1029.5`) | **`$117.34/M` (`E_v3`, `-$639.6`)** |
| **`bulk NVDA`** (`NVDA-USD`) | ⚠️ `book_diff_diagnostic` | `$120.73/M` (`-$598.0`) | **`$87.24/M` (`F_v4`, `-$302.1`, `$3.46M`)** | `$82.84/M` (`-$579.7`) | **`$72.70/M` (`E_v3`) / `$73.40/M` (`F_v4`)** |
| **`trade_xyz / entropy SNDK`** | ⚠️ `polled_15s_diagnostic` | `$583 / $770/M` | `$590 / $935/M`（15s 轮询 BBO 高滑） | `$487 / $671/M` | `$516 / $727/M`（仅作诊断） |
| **`backpack`** | ⚠️ 暂无湖仓采集 | — | 见 [`sim_backpack_dm_guardian.py`](../scripts/sim_backpack_dm_guardian.py) | — | 须先补 `backpack` WS/逐笔录制与账号级费档证据 |

**核心战略启示（为什么三大场景多所协同是唯一破局点）**：
1. **单看纯粹刷量（Track 3），最低天然成本在 `Lighter RH` 与 `Lighter Core` RWA/宽点差大盘（`$1.26–$26/M`，W19 `RH SPY` 甚至达到 `-$0.88/M` 负成本，单标的 10.25h 即可吞吐 `$1.75M–$3.10M`）**；`Extended ETH` 在 `F_v4` 下可将成本骤降 **-77%（`$379 -> $88/M`）**；而 `Arcus` 零返佣直做单所虽经 `E_v3/F_v4` 大幅压损（W24 `NVDA -42%` / W19 `BTC $24/M` / W24 `SPY $111/M`），其真正突破正收益的杠杆仍在 **T5/T6 结构性返佣（`-0.20 / -0.30 bp`）与 Track 2 `Maker-First` 跨所基差**。
2. **`F_v4_adaptive_shield` 跨全交易所一致性收益**：在几乎所有 WS+逐笔 10.25h 连续窗中，`F_v4` 显著压缩 ALO 拒单率（`-40% ~ -71%`）与美元最大亏空，把原本百美元级亏损收束在十美元级甚至翻正（`RH SPY W19 +$1.54`）。

---

## 六、全栈验证门禁与严格 `HOLD` 放行清单

- **全栈验证状态（Mac 当前工作树实测全绿，`STAGED=0`）**：
  - `make fmt-check` = 0，`make check` = 0（`cargo clippy --locked --features research-replay --lib --bins -- -D warnings` + `go vet ./...`），`make build` = 0（release 二进制与 Go feeder），`git diff --check` = 0。
  - `make test` = 0：Rust lib **`1076 passed; 0 failed; 6 ignored`**（`22.89s`）；bins `arcus_xyz_basis` **`21/21`**、`cross_venue` **`9/9`**、`lighter_inventory_mm` **`11/11`**、`maker_replay` **`0/0`**、`shm_abi_check` **`3/3`**、`venue_mm_calibrator` **`11/11`**；Go feeder 5 包通过（同行第 120 轮复验 `go test -race -count=1 ./...` **`5/5`**）；`make test-research` **`465/465`**（`14.253s`）。
  - Python 验证：`make test-python` **`55/55`**（`0.776s`）；`python3 -m unittest` 运行 [`test_sim_queue_priority_mm.py`](../scripts/test_sim_queue_priority_mm.py)（`7/7`）+ [`test_eval_multivenue_three_tracks.py`](../scripts/test_eval_multivenue_three_tracks.py)（`3/3`）共 **`10/10`** OK。
  - 冻结与解耦核验：`shasum -a 256 src/strategy/inventory_neutral_mm.rs` = `ce0930d3a843ee4b8a15d27f3d9987ec1cea7d2c60bcd1c5a3a4064501fe1488`；[`src/strategy/`](../src/strategy/) 下解耦模块 `crate::exchanges` 引用数为 **0**。
  - *未运行范围*：6 个 `#[ignore]` 性能基准、`make dashboard-build`（本轮无前端/监控 API 变更）、以及 WSL 端全套测试。
- **各赛道仍维持 `HOLD` 的客观前置门禁**：
  1. **Track 01 (`01_quant_profit`)**：先于隔离回放中实证 `adverse_veto_bps` + `passive_touch_before_hazard_ioc` 在非零返佣 / 宽点差池（`Arcus DRAM` / `Lighter RH`）的正净现金留存。
  2. **Track 02 (`02_hedge_arbitrage`)**：`Arcus <-> Lighter RH` 启用跨稳定币 `CrossStablecoinHedgePolicy` 与实时双源 PEG Quorum；`Arcus <-> TradeXYZ/Extended/Bulk` 补齐 `OPT-01` 逐资产费用证明与周一 RTH 外部见证。
  3. **Track 03 (`03_incentive_turnover`)**：多所联合刷量优先锁定 **`Lighter RH (SPY/NVDA/ETH)` + `Lighter Core (SPY/QQQ)` + `Arcus (SPY/QQQ/NVDA)`**，并在小额实盘前核准实际 `order_budget` 配额、防自成交 (`TrackCoordinator`) 及 `Bulk -bulk-trades` / `Backpack` 录制补齐。
