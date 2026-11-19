# Sprint 8 工程交付与可行性重校准报告：开销拆分、分母核查、概率排队对照及 Carry / MM 转向（2026-11-18）

> **边界与数据声明**：本文档所有工程实现与验证均在本地 Mac 仓库与离线只读汇总数据上完成，严格遵守 `AGENTS.md`（未触碰 `.agent-state/forward_ops_20261118/`、`oasis-tokyo` 或 `aleph` 线上作业；三赛道继续维持 **HOLD**）。文中数值全部直接取自本轮实跑脚本与源码单元测试输出。

---

## 一、 P0-A：拆解 `overhead_bps = 0.25` 模型税与历史账本重计分（`S8-1`）

### 1.1 问题根因与代码改造
过去回放脚本对双边总成交强行征收假设性的 `0.25 bps/腿`（双边合计 `~50 美元/百万目标额`，即 `$2/M` 预算的 **25×**），系统性掩盖了策略真实的微结构净期望（尤其对 Binance 现货-永续 Carry、Core/RH 跨所 Carry 与 zero-fee Standard 做市）。

本轮按“**固定基建费（`$/天`） + 实测手续费与滑点（0 假设额外可变 bp）**”重构计价口径，且 **100% 保持已有冻结字段与现金核对不变**：
1. **Rust 回放引擎**（[maker_first.rs](../src/strategy/relative_value/paper/maker_first.rs)）：`TurnoverFeedback` 新增可选 `fixed_overhead_usd_per_day: Option<Decimal>`，`TurnoverFeedbackReport` 新增可选 `fixed_window_overhead_quote` 与 `net_after_fixed_window_overhead`（跳过 `None` 序列化；保留 `net_after_overhead` 供 `costs::reconcile_ledger` 精确闭合）。
2. **Python 成本曲线与零奖励评分器**（[replay_turnover_cost_curve.py](../scripts/replay_turnover_cost_curve.py)、[score_zero_reward_maker.py](../scripts/score_zero_reward_maker.py)）：保留默认参数输出零漂移，新增 `strip_engine_overhead=True` 与 `fixed_overhead_usd_per_day` 可选参数，输出 `decomposed_overhead`（同时汇报**实测产出节奏**与按 `$50M / 30d` 摊销的 **`$1.00/M` 固定成本**）。
3. **只读重计分工具**（[rescore_decomposed_overhead.py](../scripts/rescore_decomposed_overhead.py)）：在不修改任何历史冻结文件的前提下，对本地 `.agent-state/forward_ops_20261118/` 已有账本执行双轨对比。

### 1.2 核心队列重计分对照（固定基建按 `$50/30d = $1.6667/天 ≈ $0.6944 / 10h 窗`）

| 队列 / 策略对照 | 样本数 | Maker / Gross 额（USD） | 旧口径 0.25 bp 净额（压力） | **去假想税可变净额（0 bp 附加）** | 变量 $/M Maker | 实测节奏含固定基建净额 | `$50M/30d` 节奏全成本 $/M Maker | 诚实限制与口径说明 |
| --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | --- |
| Sprint 7 做市原控 `reentry_ref` | 14 窗 | `$29,786` / `$46,722` | `−$6.5225`（`−$8.2161`） | **`−$5.3545`**（`−$7.0481`） | `+$179.76` | `−$15.0767` | `+$180.76` | 剥税后仍负 |
| Sprint 7 做市 `1% 盘口`（`reentry_depth1`） | 14 窗 | `$19,154` / `$29,118` | `−$0.6013`（`−$1.5977`） | **`+$0.1266`**（`−$0.8697`） | **`−$6.61`（盈余）** | `−$9.5956` | **`−$5.61`** | 容量极低（`$137/h`），且 14 窗选样未含独立七月负窗 |
| Sprint 7 1s 本地微价储备（`1s_close125`） | 14 窗 | `$19,589` / `$29,439` | `+$0.6866`（`−$0.2984`） | **`+$1.4226`**（**`+$0.4375`**） | **`−$72.62`（盈余）** | `−$8.2997` | **`−$71.62`** | 仅改变 14 窗中的 2 窗；台湾 ETH 2 窗仍净亏 `−$10.0574`（去税后） |
| 台湾 Core BTC `$500` Mode B（`d25`） | 2 窗 | N/A / `$96,171` Gross | `+$2.0709` | **`+$4.4752`** | N/A（Gross `−$46.53/M`） | `+$3.0863` | `−$45.53/M Gross` | `rh_eligible = $0`；`taiwan_eligibility.json` 不拆 Maker/Taker |
| 台湾 Core BTC `$5,000` Mode B（`d25`） | 2 窗 | N/A / `$601,746` Gross | `+$5.4343` | **`+$20.4780`** | N/A（Gross `−$34.03/M`） | **`+$19.0891`** | `−$33.03/M Gross` | **`rh_eligible = $0`**，且基于 `$50k` 假设资金，待 `$1,000+25x` 独立日复验 |
| 东京 Core/RH 套利开仓筛选（`edge0_qNone`） | 11 窗 | `$699` / `$1,598` | `−$0.1055`（`−$0.1954`） | **`−$0.0656`**（`−$0.1554`） | `+$93.80` | `−$7.7045` | `+$94.80` | 剥税后依然亏损，确认竞速套利无正期望 |

### 1.3 资金费 Carry（`carry_results.json` & Binance 2023–2025）去税对照
- **Core vs. RH 双域 78 窗 Temporal 样本（`l6_d2` 严格 2 小时滞后信号）**：
  - `XRP temporal`（13 活跃窗 / 78）：旧 `after_1bp`（扣除 4 笔 × `0.25 bp = 1 bp/轮`）为 `+0.60 bps`，**0 费率 `PostOnly` 毛Carry（扣税前）实为 `+13.60 bps`（11/13 窗为正，均值 `+1.046 bps/轮`）**。
  - `SOL temporal`（5 活跃窗 / 78）：旧 `after_1bp` 为 `−2.43 bps`，**0 费率 `PostOnly` 毛Carry 为 `+2.57 bps`（5/5 活跃窗全正）**；加上前期 `early` 22 窗总计 `+42.50 bps`。
  - *关键提示*：双域利率筛查不含执行盘口价差与两域 FX/转仓风险，仅证伪了“1 bp 假设税导致全负”的假象，证实 `~1 bp/轮` 以内的 `PostOnly` 被动入场具备正费率余量；**Binance 2025 现货-永续 Carry** 同理（正费率筛选盈亏平衡线为 `0.134 bps/笔`，`0.25 bps` 模型税导致 `-0.00759 USD/窗`，切换 0 费率 Lighter Standard `PostOnly` 即跨过纯手续费门槛，剩余核心考核为进出场基差/价差控制）。

---

## 二、 P0-B：多交易所“合格目标场所 Maker 成交量”分母定义与可行边界（`S8-2`）

在 [audit_lighter_program_scope.py](../scripts/audit_lighter_program_scope.py) 中保留既有 `program_scope`，新增 `cost_per_million_eligible_maker()`（当合格分母为 `None` 或 `0` 时强制返回 `None`，严禁除以 Raw 成交额伪造达标）与 `multi_venue_program_scope()` 编码一手规则：

| 场所 / 域 / 账户档 | 公开费率（Maker / Taker） | 合格目标 Maker 量规则与关键限制 | 对 `$2 / $1M` 目标影响 | 一手出处 |
| --- | --- | --- | --- | --- |
| **Lighter Core Standard** | `0 / 0 bps` | **RH Points = `$0` 合格量**；LPP 明确拒绝 Standard 账户（须 Premium/Advanced + 随机挂单快照） | 仅可计入零奖励自营或未来 Core 专项，不可计入 RH/LPP 合格量 | [LPP 文档](https://docs.lighter.xyz/programs/liquidity-partner-program.md) |
| **Lighter RH Standard** | `0 / 0 bps` | **明确排除 Standard-vs-Standard 成交**（`0` 实时/周积分）；Maker/Cancel/Taker 带 `200/200/300 ms` 减速带 | 若对手方同为 Standard，合格分母=`$0`；即使全合格，W01 单所 RH ETH 实测成本仍达 `$47.87/M`（无税下 `$22.87/M`） | [RH Points 规则](https://docs.lighter.xyz/programs/lighter-on-robinhood-chain-points.md) |
| **Arcus RWA Level 0** | **`0.00 / 2.25 bps`** | 公开 `0` Maker 费，但 **Annex C 明确授权取消纯刷积分之市场中性、对冲或循环成交资格** | 仅适合方向/慢轮动或双 Maker 真实报价；目前正向对冲吃单率 `31%–50%` 触发 `2.25 bps` 成本（门槛 `0.889%`） | [Arcus 费率](https://docs.arcus.xyz/concepts/perpetuals/fees.md) · [Annex C](https://arcus.xyz/legal/protocol-terms) |
| **Arcus Crypto L0 / L4** | L0 `1.50 / 4.50`；L4 `0.00 / 2.30` | L0 光 Maker 显式费即 **`$150/M`（75× 预算）**；L4 零 Maker 费需 **30 天 `$200M` 成交** | `$50M` 目标本身低于 L4 门槛，Crypto L0 完全不可行 | [Arcus API Fees](../.agent-state/research/INCENTIVE_TURNOVER_COST_100_50M_2026_11_17.md) |
| **Hyperliquid Core & HIP-3 Growth Mode** | HL Tier0 `1.50 / 4.50`；HIP-3 Base `2×`，开 Growth Mode 降 **90%** | 1. HIP-3 **Growth Mode 明确排除已验证 Core Crypto 资产**（资产互斥）；2. Growth Mode 最低 Maker 费约 `0.30 bps`（**`$30/M`，15× 预算**）且**同时削减 90% 返佣与 14 天费率定级成交量贡献**；3. HL Maker 负返佣（`−0.1 bps`）需 **14d 全所 Maker 份额 `>0.5%`** | `$500–$1,000` 资金下 HIP-3 Growth Mode 既不适用于 BTC/ETH，也至少 `$30/M`，且 HL 0.5% 做市份额不可及 | [HL Fees](https://hyperliquid.gitbook.io/hyperliquid-docs/trading/fees) · [HIP-3 Growth Mode](https://hyperliquid.gitbook.io/hyperliquid-docs/hyperliquid-improvement-proposals-hips/hip-3-builder-deployed-perpetuals) |

**诚实可行边界结论**：在 `$500–$1,000` 本金、50–200 ms 公网延迟约束下，`$2/M` 要求 1 秒 markout 与价差捕获几乎完全对冲所有逆向选择，相当于接近无损的顶级做市商；现有实证距该水平仍有 `15×–75×` 差距。务实路径二选一：**（A）将合格激励单位预算重锚定到 `$20–$50/M` 并聚焦 `0 bp` Maker 场所（Arcus RWA / Lighter 核实账户）；（B）在保持 `$1,000` 保证金的前提下通过受控杠杆（`25×`）与高价差/短波比非主流标的放大有效容量。**

---

## 三、 P1-A：概率排队位置敏感性模型与撤单归因闭环（`S8-3`）

针对 [recorded.rs](../src/strategy/inventory_neutral_mm/replay/recorded.rs) 与 [maker_first.rs](../src/strategy/relative_value/paper/maker_first.rs) 原先保守假定“全档深度均排在己方之前（`ahead = level_size`）且队列前方零撤单”所导致的**只在档位被扫穿时成交（系统性放大 1s 逆向选择与库存亏损）**问题，本轮在两套核心回放器上同步实现**默认关闭、零基线漂移的 `ProbabilisticQueueConfig`**：

- **盘口与逐笔的到达次序。** 11 月 19 日复核发现，原实现假定逐笔先到。在同价深度从 1.00 降至 0.70、实际成交 0.30 的合成回放中，先收到盘口时，后续 0.50 的逐笔让单所模型多报 0.10 成交量，双所模型多报 39.992 美元名义量。两个场景都没有撤单。本条修复前的概率队列结果仅作故障对照。
- **修复后的推断。** 挂单生效后取首个可见且新鲜的档位作基准。仅在后续盘口到达、对应区间的时钟误差与逐笔送达宽限均已过去后，才以档位净减少量减去可能落在该区间的同价公开逐笔量。剩余减少量给出可推断撤单的上界。失联、档位离开视野、时间倒退或同时间戳深度冲突均需重新建立基准。只有公开逐笔能触发成交。市场按价聚合的深度仍无法识别撤单在己方之前还是之后。
- **排队位置假设。** 令 $x$ 为当前前方数量占扣除成交后的原档位数量的比例，限于 $[0,1]$。前方撤单份额为 $x^a/(x^a+(1-x)^a)$，对应 hftbacktest 的 `PowerProbQueueFunc`。推进数量同时受可归因的减少量和当前前方数量约束。`initial_ahead_fraction` 取 1.0 或 0.5 分别测试队尾与中间起点。[上游队列实现](https://github.com/nkaz001/hftbacktest/blob/master/hftbacktest/src/backtest/models/queue.rs)
- **Sprint 7 撤单膨胀归因与修复闭环确认**：通过 `recorded::tests::cancel_attribution_preserves_baseline_json_and_decomposes_reasons_and_queue_ages` 与 `recorded::tests::probabilistic_queue_avoids_trade_double_count_and_recovers_touch_fills` / `maker_first::tests::probabilistic_queue_avoids_trade_double_counting_and_tracks_fixed_overhead`（以及 `maker_first` 与 `recorded` 双边逐字回归），确认 `diagnose_cancel_attribution` 全字段零漂移且 `staged_reentry_only` 显式收敛 `AdverseRetreat`/`NormalPriceRequote` 级联撤单。

---

## 四、 P1-B & P2：双高潜线索预备、Carry/MM 单边转型与选标筛选（`S8-4` & `S8-5`）

1. **`cross_sectional.rs` 与 `InventoryNeutralMM` 角色转型**：
   - 新增 `CrossSectionalDirection::FundingCarry`（配合 `max_funding_staleness_ms`，默认 2h），在 [cross_sectional.rs](../src/strategy/cross_sectional.rs) 验证 **24 个市场 + 8 小时低频调仓 + 纯 `PostOnlyPeg` 入场（无 IOC 兜底）** 全因果链路；
   - 新增通用单边屏蔽接口 `CrossSectionalEngine::mm_blocked_entry_sides(symbol_id, now_ms, tail_count)`，并与 [inventory_neutral_mm.rs](../src/strategy/inventory_neutral_mm.rs) 的无配置扩张运行时开关 `set_external_entry_side_veto` 对接（多空双侧极端尾部仅屏蔽加仓边，持仓逆向减仓边与 TTL 到期自动放行 **100% 不受阻断**）。
2. **`$1,000` 保证金 + `25×` 杠杆台湾 Core BTC Mode B 冻结规格**（[prepare_leveraged_taiwan_lead.py](../scripts/prepare_leveraged_taiwan_lead.py)）：
   - 明确阻断 `$50k -> $1k` 直接缩容时的三大工程陷阱：(a) `reference_portfolio_value` 错配或 `portfolio_scale` 把 `$5,000` 单笔缩小 50× 至 `$100`；(b) 默认 `10×` 杠杆无法通过 `$10,000` 峰值仓位的初始保证金校验；(c) `0.2%` 日损阈值（`$2`）在历史正常回撤（`$22.99` / `$40.00`）内提前熔断（固定为 `5%` = `$50` 硬止损 + `cap_utilization_floor_to_base_notional = true` + `MMF = 0.012` / `1.8×` 强平前退出），并自动附带 `rh_direct_rule_eligible_turnover_quote = "0"` 的诚实标签供 `aleph` 放上独立新日期 10h 验证。
3. **价差 / 短期波动率（`spread/vol`）选标筛选器**（[screen_market_spread_to_vol.py](../scripts/screen_market_spread_to_vol.py)）：计算 `median_spread_bps / realized_1s_vol_bps` 与 `net_half_spread_after_maker_fee_bps = 0.5 * spread - maker_fee_bps`，并与 `multi_venue_program_scope` 绑定，指导做市退出拥挤的 0 费 BTC/ETH、转向 Arcus RWA 0 费标的与宽价差长尾市场。
4. **总案纠偏与参数冻结纪律**：[实盘盈利演进总案](LIVE_PROFITABILITY_MASTERPLAN_2026_11_18.md) 顶部已增补 `§0 Sprint 8 实证纠偏与可行边界勘误`，明确停止 `50–200 ms` 吃单竞速套利，冻结参数并在 `W07–W16` 留出窗验收，杜绝同一批历史 14 窗继续多参数选优。

---

## 五、 本轮 Mac 全门禁验证证据

| 门禁 / 检查项 | 验证范围与说明 | 结果 |
| --- | --- | --- |
| `make fmt-check` | `cargo fmt --all -- --check` + `gofmt -l` 全仓格式检查 | **PASS** |
| `make check` + 双特性 `clippy` | `cargo check` + 默认与 `--features research-replay` 双特性 `cargo clippy --locked --all-targets -- -D warnings` | **PASS（0 warnings）** |
| `make test` + `--features research-replay` | Rust `651` lib + `4` `cross_venue` + `8` `lighter_inventory_mm`（`6` ignored；含 `maker_first` `61/61`、`recorded` `27/27`、`cross_sectional` `10/10`、`inventory_neutral_mm` 单边 veto 集成测试）、Go 全 5 包 `-race`、`scripts` `173/173`（含 `prepare_leveraged_taiwan_lead.py` 终版修改后复验 `173/173`） | **PASS** |
| `make test-python` | `deploy/oasis` 监控、存储与告警 `42/42` | **PASS** |
| `make dashboard-build` | `oxlint`（0 warnings / 0 errors）、`vitest` `19/19`、`tsc -b && vite build` | **PASS** |
| `make build` | Rust release 二进制 + Go `feeder` 二进制全量构建 | **PASS** |
| `make trade MODE=check CONFIG=config.example.toml` | `Configuration OK: Mainnet Lighter ETH market 0, strategy_mode=quant, scenario=unspecified` | **PASS** |
| Git / 隔离边界检查 | `git diff --check` 零告警；`git diff --cached` 为空；`.agent-state/forward_ops_20261118/` 保持只读零改动；三赛道维持 `HOLD` | **PASS** |

*(注：上述代码与脚本构建/单测门禁均在所有 Rust/Go/Python/TS 代码定稿后实跑通过；本报告与交接板末段文档更新完成后再次复核 `git diff --check` 与 `git diff --cached`。)*
