# 赛道一/二升级：跨所领跑吃单与 Maker-First 对冲基差可执行回放（2026-11-26）

> **范围与安全**：全部结果来自 `aleph` 湖仓只读 parquet（内存 DuckDB，`2 GB / 2 threads`，`nice 10`），未打开 `lakehouse.duckdb`，未触碰 Codex 的 W25 转存/备份锁（W25 整窗排除）。三赛道维持 **HOLD**，零下单/撤单/重启。输出只含聚合统计，不含原始成交或账户数据。所有结论是研究假设，不代表实盘可盈利。

## 一、本轮新增内容

| 类别 | 文件 | 作用 |
| --- | --- | --- |
| 共享数据层 | [`lakehouse_grid.py`](../scripts/lakehouse_grid.py) | 100 ms 固定步长、按接收时间因果的 BBO 网格（每格只取格末之前已到达的报价），记录盘口年龄与推送节奏（p50/p90 到达间隔、`transport`），并读取公开成交 |
| 赛道一研究 | [`research_lead_lag_quant.py`](../scripts/research_lead_lag_quant.py) | 跨所领跑吃单：`dev = (ln ref − ln tgt) − EMA_τ`（严格因果），越过 θ 后**按场所延迟**吃目标盘、持有固定时长后再吃回；双边计费、持仓不重叠、盘口陈旧不进场；附**成交价复核**与 +250/+1000 ms 额外延迟敏感性 |
| 赛道一前推 | [`walkforward_lead_lag.py`](../scripts/walkforward_lead_lag.py) | 读取 W19 冻结参数（不再重新选参），逐窗回放 W01–W24 全部东京 crypto 窗口（排除 W19 选参窗与正在入库的 W25） |
| 赛道二研究 | [`replay_maker_hedge_basis.py`](../scripts/replay_maker_hedge_basis.py) | 可执行 Maker-First 账本：用对冲所盘口推导 Maker 挂价，按库存偏斜调整所需边际，只认**严格穿价**成交，成交后按对冲所延迟吃单对冲；10h 后双腿吃单清仓；附对冲腿**成交价复核**与 Arcus `level_0 2.25 bp` 敏感性 |
| 测试 | [`test_track01_track02_research.py`](../scripts/test_track01_track02_research.py) | 8 项合成数据测试（网格因果性、EMA 排除当前样本、领跑获利/反向不获利、选参门槛、严格穿价、贴价不成交、`lead_beta` 避开毒性成交） |
| Rust 赛道一 | [`lead_lag_taker.rs`](../src/strategy/lead_lag_taker.rs) | 与交易所解耦的领跑吃单状态机：因果 EMA 偏离、预热、间隔过长重置、盘口陈旧/结构断裂拦截、"延迟 + 持有"计时出场、时钟回退拒绝（4 项单测） |
| Rust 赛道二 | [`basis_maker_hedge.rs`](../src/strategy/basis_maker_hedge.rs) | 与交易所解耦的 Maker-First 报价器：因果公允基差、库存偏斜、持仓上限撤边、只加入不穿越、可选优一跳、`lead_beta` 预期对冲所跟随（5 项单测） |

**研究协议**：只在 W19（`selection_dev`）选参并冻结，原样回放到 W24（`holdout`）和 W23（`holdout_2`），再前推到其余窗口。赛道一每对测了 **84 个变体**（θ 7 档 × 持有 3 档 × τ 4 档），共 16 对；赛道二每对测了 **60 个变体**（e0 5 档 × τ 2 档 × 优一跳 2 档 × `lead_beta` 3 档），共 11 对。所有变体数量都在输出 JSON 中登记。

> [!NOTE]
> **10 小时口径**：每窗采集跨度约 10.25 h，前 15 min 作为预热，决策区间约 10.0 h。逐窗 `continuous_10h` 标记均为 `false`，原因是严格阈值（预热 + 10 h = 36,900,000 ms）比实际跨度多出数秒；各窗最大推送间隔为秒级（例如 W24 Binance 约 5.8 s），未发现分钟级断档。下文所有"窗口"都按这一实际覆盖如实报告。

## 二、关键发现 1：Arcus 公开盘口推送约每 200 ms 一次，此前基于报价的 Arcus 研究偏乐观

各场所目标盘口到达间隔（W19，`transport=ws`）：

| 场所 | p50 | p90 |
| --- | ---: | ---: |
| `lighter` / `lighter_robinhood` | 50–51 ms | 59–102 ms |
| `extended` | 100 ms | 125–139 ms |
| **`arcus`**（`l2Orderbook` WS） | **200 ms** | **208 ms** |

在 Arcus 上，按报价计算的领跑吃单看上去很好（如 `arcus SOL ← binance`：W24 `+2.84 bp`、W23 `+4.95 bp`，信息系数约 0.35–0.43），但这正是盘口推送节流的特征：记录到的盘口落后于真实盘口。改用**真实公开成交价**复核后：

| 目标 | W24 报价口径 | W24 成交价复核 | W23 成交价复核 | +1000 ms 报价口径 |
| --- | ---: | ---: | ---: | ---: |
| `arcus BTC ← binance` | `+1.69 bp` | `−1.52 bp`（12 笔可核） | `−6.97 bp` | `−4.24 bp` |
| `arcus ETH ← binance` | `+1.55 bp` | `−6.40 bp`（7 笔） | 仅 1 笔 | `−4.55 bp` |
| `extended BTC ← binance` | `+0.48 bp` | `−2.62 bp`（20 笔） | 仅 2 笔 | `−4.46 bp` |

**结论**：Arcus/Extended 的领跑吃单**不成立**。另外，凡是以 Arcus 公开盘口做撮合/排队仿真的研究（包括赛道三 Arcus 队列仿真），都应把 200 ms 的盘口节流视为偏乐观来源，并尽量改用逐笔成交校准。

## 三、关键发现 2（赛道一）：Lighter RH ETH ← Binance 是唯一通过成交价复核前推的组合

W19 冻结参数，逐窗前推（每窗成交价复核净值的均值；跨窗 t 值）：

| 组合（冻结参数） | 成交价复核：正收益窗 / 总窗 | 窗均净值 | 跨窗 t | 仅 W20–W24 | +250 ms 额外延迟 |
| --- | ---: | ---: | ---: | ---: | ---: |
| **`lighter_rh ETH ← binance`**（θ 6 bp，τ 120 s，持有 10 s，延迟 230 ms） | **17 / 21** | **`+2.27 bp`** | **3.40** | 4/5，`+3.88 bp` | 15/21，`+1.45 bp`（t 1.67） |
| `lighter_rh BTC ← binance`（θ 6，τ 120 s，持有 10 s） | 11 / 22 | `+1.65 bp` | 1.59 | 2/5，`+1.82 bp` | 13/22，`+1.36 bp` |
| `lighter SOL ← binance`（θ 4，τ 120 s，持有 3 s） | 15 / 22 | `+0.30 bp` | 2.40 | 3/5，`+0.19 bp` | 11/22，`+0.07 bp` |
| `lighter ETH ← binance`（θ 6，τ 2 s，持有 10 s） | 14 / 22 | `+0.31 bp` | 0.99 | 2/5，`−0.04 bp` | 10/22，`+0.25 bp` |
| `lighter BTC ← binance`（对照） | 3 / 22 | `−0.39 bp` | −4.81 | 0/5，`−0.66 bp` | 2/22，`−0.44 bp` |

- 费用：Lighter Standard 吃单 `0 bp`（结果已含双边费用 0 与盘口点差）；延迟 = 速度缓冲 + 东京 RTT（Core 330 ms、RH 230 ms）。
- 交易频次：`lighter_rh ETH` 全部窗口共 932 笔（约 42 笔/窗）。容量受 RH ETH 一档深度限制（此前测得中位约 `$762`）；按 `$1–2k` 小单计，单窗毛利仅为十几至几十美元量级。
- 多重检验：从 16 对 × 84 变体中选出，再对 5 对做前推；`t = 3.40` 高于 5 对 Bonferroni 校正后的约 2.6，但仍属研究假设。
- Lighter BTC 对照稳定为负，说明这套方法不会"处处为正"。

**Rust 落地**：[`LeadLagTakerConfig::default()`](../src/strategy/lead_lag_taker.rs) 采用上述 RH ETH 冻结参数，状态机只做决策，不下单。

## 四、关键发现 3（赛道二）：可执行 Maker-First 在 RWA 上约为盈亏平衡，主要亏损来自对冲腿在延迟期内的滑点

固定参数：每笔 `$1,000`，库存上限 4 笔，偏斜 `0.5 bp/笔`，只认严格穿价成交，不含资金费。

| 组合（W19 冻结） | W24 成交数 / 盈亏（报价对冲） | W24 盈亏（成交价对冲） | W23 盈亏（报价 / 成交价对冲） | 平均对冲滑点（W19 → W24） |
| --- | ---: | ---: | ---: | ---: |
| `arcus → lighter QQQ`（e0 5 bp，τ 1 h） | 16 / `+$3.56` | `+$1.36` | `+$0.32` / `+$0.27` | `−3.79 → −2.39 bp` |
| `arcus → lighter_rh QQQ`（USDC/USDG ⚠️） | 12 / `+$2.21` | `−$1.14` | `+$1.57` / `−$0.21` | `−2.68 → −1.48 bp` |
| `lighter → arcus QQQ`（e0 8 bp，τ 10 min） | 13 / `+$3.56` | `+$2.59` | `+$0.36` / `−$0.03` | `−3.45 → −1.53 bp` |
| `lighter → arcus SPY`（e0 5 bp） | 29 / `+$3.11` | `+$1.43` | `−$0.13` / `−$0.13` | `−2.38 → −1.77 bp` |
| `arcus → lighter BTC`（e0 8 bp） | 41 / `+$5.07` | `−$10.33` | `−$1.92` / `−$6.95` | `−6.06 → −6.85 bp` |
| `lighter → arcus BTC`（e0 8 bp） | 253 / `+$6.32` | `−$26.12` | `+$4.18` / `−$13.86` | `−5.35 → −4.96 bp` |
| `arcus → lighter NVDA`、`arcus → lighter_rh NVDA` | W19 无正收益变体（0/60），不进入保留期 | — | — | — |

- 同行第 121 轮指出对冲时刻盘口陈旧时仍会读旧价；已改为顺延到之后第一个新鲜盘口成交并计数（`hedge_stale_book_events`）。重跑后 W24/W23 所有组合该计数均为 0，上表数值不变。
- Arcus 吃单费改为 `level_0 2.25 bp` 后，RWA 正收益组合在 W24 仍为正但收窄（如 `lighter → arcus SPY`：`+$3.11 → +$1.38`）。
- **结构性结论**：成交本身是真实的（严格穿价），但 Maker 成交大多发生在价格快速移动时，速度缓冲（或 200 ms 的节流盘口）使对冲腿平均吃亏 `1.5–9 bp`，基本抵消了入场时相对公允的 `1–6 bp` 优势。此前"超过门槛的时间占比"（52–87%）**不能**代表可执行盈利。
- `lead_beta`（预期对冲所跟随）只在 1 个组合中被选中；它已作为可选参数接入 Rust 报价器，但尚无保留期证据证明能稳定改善。

## 五、下一步（全部仍在 HOLD 之下）

1. **赛道一**：把 `lighter_rh ETH ← binance` 加入预注册前推（W25 起冻结参数不变），并补充按一档深度计算的容量与真实成交滑点；在无费用证据的场所不扩展。
2. **数据工程**：核实 Arcus 是否有更快的 BBO/逐笔频道；在拿到之前，Arcus 的报价口径研究一律附带成交价复核。
3. **赛道二**：优先研究"慢所挂单 + 快所对冲"、按延迟调整所需边际（`e0 ≥ 预期对冲滑点`）以及同结算币（USDC/USDC）组合；USDG 组合继续要求跨稳定币策略。
4. **赛道三**：用逐笔成交重新校准 Arcus 队列/ALO 仿真，修正 200 ms 盘口节流带来的偏乐观。

## 六、验证

- 本轮代码改动后 Mac 全栈门禁：`make fmt-check`、`make check`、`make test`（Rust lib **`1085 passed; 0 failed; 6 ignored`**；bins `21+9+11+0+3+11`；Go feeder 5 包通过（cached）；research **`473`** 项 OK）、`make test-python` **`55`** 项 OK、`unittest`（新测试 8 项 + 两个已有测试文件）共 **`18`** 项 OK、`make build`、`git diff --check`、`STAGED=0`、[`inventory_neutral_mm.rs`](../src/strategy/inventory_neutral_mm.rs) SHA-256 `ce0930d3...` 未变。
- 门禁之后的改动：两个新 Rust 文件的文档措辞、`LeadLagTakerConfig::default()` 改为 RH ETH 参数、`exit_due_ms` 改为 `saturating_add`（同行第 121 轮建议），以及赛道二陈旧对冲盘口处理；改后的复核见交接记录与最终回复。
- 结果文件（本地，不入 Git）：`.agent-state/research/track01_lead_lag_20261126.json`、`track01_walkforward_20261126.json`、`track02_maker_hedge_20261126.json`。
