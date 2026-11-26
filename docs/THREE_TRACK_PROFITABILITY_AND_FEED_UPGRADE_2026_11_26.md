# 量化、套利与行情工程并行全面升级实证报告（2026-11-26）

> **安全与范围声明（`AGENTS.md`）**：本报告全部测试、回放与只读探针均在 **严格 `HOLD`（零实盘下单、零撤单、零服务重 deployed/重启，严守不触碰 `oasis-tokyo`、W25 锁与冻结 `inventory_neutral_mm.rs`）** 约束下完成；所有新增策略模块保持 **零 `crate::exchanges::*` 代码耦合**；三大赛道严格遵循非交叉补贴契约。结果均为研究假设，不构成实盘盈利承诺。

---

## 一、工程突破：Arcus & Hyperliquid / HIP-3 实时 `bbo` 提速 80–164 倍

针对用户提问“**Arcus 通过 API Key 是否推送很快**”与本轮实测湖仓数据时发现的 **Hyperliquid / TradeXYZ / Entropy 全线 12.36 秒延迟瓶颈**，已完成公网只读探针取证与 Go Feeder / Capture 工程升级（不部署/重启远端服务）：

### 1.1 公网 WS 只读实测真值

| 交易所 / 频道 | 是否需 API Key | 实测推送间隔（`p50` / `p90` / 频率） | 原采集问题与根因定位 |
| --- | --- | --- | --- |
| **`arcus l2Orderbook`** | 否（WS `authenticate` 仅用于写操作或账户查询） | **`200.2 ms`** / `212.5 ms`（`3.89/s`） | 官方固定 `~200 ms` 快照聚合，此前 Feeder 与 Capture 仅订阅了 `l2Orderbook` |
| **`arcus bbo`** | **否（公开极速）** | **`31.1–37.1 ms`**（`min 0.0 ms`, `BTC 3.73/s`） | 事件驱动推送 BBO；已在本轮同时接入 `bbo` |
| **`hyperliquid l2Book`（默认）** | 否 | **`12,481.5 ms`**（`0.11/s`，Core & HIP-3 一致） | `perp-capture` 只订阅默认 `l2Book` 且未传 `-hl-fast-book`，导致湖仓 `2,996` 行/10h |
| **`hyperliquid l2Book (fast=true)`** | 否 | **`530.9 ms`**（`1.86/s`） | [`aleph-perp-capture-{crypto,rwa}.service`](../deploy/oasis/aleph-perp-capture-rwa.service) 原配置漏传 `-hl-fast-book` |
| **`hyperliquid bbo`** | **否（公开极速）** | **`BTC 75.9 ms` (`9.77/s`) · `xyz:NVDA 107.8 ms` (`5.71/s`) · `io:NVDA 118.5 ms` (`3.17/s`)** | 相比默认 `l2Book` 提速 **105–164 倍**；`feeder/exchanges/hyperliquid.go` 已支持但 `perp-capture` 尚未订阅 |

> 探针工具：[probe_arcus_ws_cadence.py](../scripts/probe_arcus_ws_cadence.py)、[probe_hl_ws_cadence.py](../scripts/probe_hl_ws_cadence.py)（实测回执存 [`.agent-state/research/hl_ws_cadence_probe_20261126.json`](../.agent-state/research/hl_ws_cadence_probe_20261126.json)）。

### 1.2 已落地的 Go Feeder & Capture 工程改造（全 `-race` 通过）

1. **Arcus `bbo` 零分配接入 + 单调序号防回写**（[`feeder/exchanges/arcus.go`](../feeder/exchanges/arcus.go)、[`feeder/cmd/perp-capture/{arcus.go,main.go}`](../feeder/cmd/perp-capture/arcus.go)）：
   - 同时订阅 `l2Orderbook`（维护 5 档深度）与 `bbo`（事件驱动发布 SHM BBO，`BenchmarkArcusHandleMessageBBO` 实测 `0 B/op, 0 allocs/op, 1352 ns/op`）。
   - 为每标的引入共享 BBO `globalSequenceId` 水位线：确保 `200 ms` 到来的 `l2Orderbook` 快照**永远不会覆盖更新的 `bbo`**；交叉/锁定盘口照旧立即置无效并重连。
2. **Hyperliquid / HIP-3 `bbo` 订阅与 `-hl-fast-book` 补齐**（[`feeder/cmd/perp-capture/{main.go,hyperliquid.go,hyperliquid_test.go}`](../feeder/cmd/perp-capture/hyperliquid.go)、[`aleph-perp-capture-{crypto,rwa}.service`](../deploy/oasis/aleph-perp-capture-crypto.service)）：
   - `perp-capture` 的 `hyperliquid` WS 源同步订阅 `l2Book`、`bbo` 与 `activeAssetCtx`（保留 `raw_source="hyperliquid"` 不破坏下游归档字典），并在服务端/单测验证 `l2Book` 仅在 `-hl-fast-book` 时附带 `"fast": true`。
   - **给 `@Codex` 的入库衔接说明（遵守锁边界，未修改 `scripts/quant_lakehouse.py`）**：待 W25/授权发布重启后，只需在 [`scripts/quant_lakehouse.py#L1188`](../scripts/quant_lakehouse.py) 的 `elif chan == "bbo":` 分支解析 `data["bbo"][0/1]`（结构与 [`hyperliquid.go`](../feeder/exchanges/hyperliquid.go) 一致）调用 `_emit_bbo_row`，并给 Arcus `channel == "bbo"` 补充解析即可将湖仓实时盘口精度从 `200ms / 12.4s` 提升至 `30–118ms`。

---

## 二、赛道一 (`01_quant_profit`)：跨所领跑前推、被动平仓证伪与真实深度组合账本

### 2.1 W19 冻结参数 22 窗（`W01–W24` 剔除 `W19/W25+`）Walk-Forward 实证

所有信号严格按 `dev = (ln ref_mid - ln tgt_mid) - EMA_tau(prior)` 因果计算，并在场所真实吃单延迟（Lighter RH `230 ms`、Core `330 ms`、Arcus `40 ms`）后以盘口价与**真实公开逐笔成交价 (`print_verified`)** 双重核算（[`track01_walkforward_20261126.json`](../.agent-state/research/track01_walkforward_20261126.json)、[`track01_lead_lag_v3_20261126.json`](../.agent-state/research/track01_lead_lag_v3_20261126.json)）：

| 组合（W19 冻结参数，未重选） | 真实成交复核：正窗/总窗 | 全窗均值 | 跨窗 `t` 值 | 仅 `W20–W24` 保留期 | +250 ms 延迟鲁棒性 | 实盘可用性与限制说明 |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| **`lighter_rh SOL ← binance`** (`θ=8, τ=120s, hold=10s`) | **14 / 18** | **`+3.73 bp`** | **`2.36`** | **4 / 5 (`+4.33 bp`)** | 11/16 (`+1.38 bp`) | 边际最高，但 RH SOL 一档深度极浅（中位仅 `$81`） |
| **`lighter_rh ETH ← binance`** (`θ=6, τ=120s, hold=10s`) | **17 / 21** | **`+2.27 bp`** | **`3.40`** | **4 / 5 (`+3.88 bp`)** | 15/21 (`+1.45 bp`) | 等权 bp 强显著（高过 5 对 Bonferroni ~2.6），但注意 W25 事后逆例 `-3.02 bp` 及 L1 中位仅 `$326` |
| **`lighter SOL ← binance`** (`θ=4, τ=120s, hold=3s`) | 15 / 22 | `+0.30 bp` | `2.40` | 3 / 5 (`+0.19 bp`) | 11/22 (`+0.07 bp`) | L1 中位 **`$1,162`**，真实可执行容量高出 RH 10× |
| **`lighter ETH ← binance`** (`θ=6, τ=2s, hold=10s`) | 14 / 22 | `+0.31 bp` | `0.99` | 2 / 5 (`-0.04 bp`) | 10/22 (`+0.25 bp`) | L1 中位 **`$3,309`**，USD 加权下表现最稳健 |
| `lighter BTC ← binance`（对照） | 3 / 22 | `-0.39 bp` | `-4.81` | 0 / 5 (`-0.66 bp`) | 2/22 (`-0.44 bp`) | 噪声过大，验证策略不会虚构全正假象 |
| `arcus` / `extended` ← `binance` | W24 全转负 | `-1.52 ~ -6.40 bp` | 负 | 全负 | 更差 | **证伪**：由 Arcus 200ms `l2Orderbook` 推送节流造成的盘口假象 |

### 2.2 混合平仓测试与 L1 深度封顶美元组合回测（`research_lead_lag_hybrid.py`）

1. **为什么“进场后立刻挂被动 `reduce_only` 平仓”反而亏损？（重大反直觉发现）**：
   在 W19 对 6 个组合全部实测 `taker_only` vs `passive_static_touch` vs `passive_trail_touch`（[`track01_hybrid_portfolio_20261126.json`](../.agent-state/research/track01_hybrid_portfolio_20261126.json)）：
   - `taker_only` 在 6 个组合全正（如 `lighter ETH +$35.31`、`lighter BTC +$48.39`、`lighter SOL +$24.23`）；
   - **`passive_static_touch` 与 `passive_trail_touch` 6 组全军覆没**（如 `lighter BTC` 从 **`+$48.39` 暴跌至 `-$151.92`**，`lighter ETH` 从 **`+$35.31` 跌至 `-$30.17`**）。
   - **根因**：领跑吃单是**短线顺势动量信号**；一旦立刻在盘口对面挂被动止盈单，涨/跌势延续时你的盈利会被死死封顶在 ~0 bp 立即被扫走，而所有逆转亏损单永远没人扫你的被动单，全部扛满 10 秒超时按 Taker 止损！**顺势短周期必须保留右侧尾部（`taker_only`）**。
2. **真实 L1 深度封顶（`min($1,000, 0.5 × L1_USD)`）22 窗美元归因**：
   - **`lighter` Core `ETH + SOL`（`taker_only`）**：22 窗合计净赚 **`+$124.67 USD`**（`ETH +$65.52` / 17/22 正窗 / `t=2.12`；`SOL +$59.15` / 16/22 正窗 / `t=1.58`），单笔真实 L1 深度 `$1,162–$3,309`。
   - **为什么未筛选的 6 组拼盘亏损 `-$96.77`？**：全由 `lighter BTC`（`-$195.74`，7/22 窗正，`theta=2.0` 过窄）拖累，同时 `lighter_rh` 因 L1 中位仅 `$81–$326`，大单被腰斩到 `$40–$160`，少数深盘亏损笔拉低 USD 权重（RH 等权 bp 虽佳但绝对美元容积有限）。
3. **Rust 解耦落地**：[`lead_lag_taker.rs`](../src/strategy/lead_lag_taker.rs) 新增 opt-in `LeadLagExitMode`（默认维持 `TakerOnly`）、`QuotePassiveExit` 与 `notify_passive_exit_filled`（共 5 项单测全过，零交易所依赖）。

---

## 三、赛道二 (`02_hedge_arbitrage`)：速度缓冲 Maker + 外部撤单盾护突破盈利

### 3.1 已证伪路线对照
- **双边吃单均值回归**（[`track02_ws_trio_basis_20261126.json`](../.agent-state/research/track02_ws_trio_basis_20261126.json) & [`track02_rwa_basis_exec_20261126.json`](../.agent-state/research/track02_rwa_basis_exec_20261126.json)）：15s 轮询与次分钟级执行滞后下全负（2s 延迟 `-3,072 bp` / `0/7` 窗；1m 延迟 `-8,923 bp`）。
- **纯粹动态资金费套息**（[`track02_funding_carry_20261126.json`](../.agent-state/research/track02_funding_carry_20261126.json)）：计入双边 Taker 与滑点门槛后，Crypto `0` 活跃配对，RWA 保留期 `-13.81 bp`。

### 3.2 核心突破：利用 Lighter 不对称速度缓冲的 **外部参照撤单盾护 (`Pre-Fill Cancel Shield`)**

物理机理：Lighter Core/RH **Maker 挂撤延迟仅 ~40 ms（0 ms 速度缓冲），而 Taker 强制延迟 200 ms (RH) / 300 ms (Core)**。当我们在 Lighter 挂被动 Maker 单并在外部快速参照源（Crypto 用 Binance 50 ms；RWA 用对侧 WS）发现突发反向跳价 `>= shield_bps` 时，撤单在 `[t_fill - speedbump + rtt, t_fill - rtt]` 窗口内抵达撮合引擎，**直接截杀正在路上的毒流 Taker 单**！

下表汇总 **W19 选参冻结 -> W20–W24 保留期（Holdout）与 W15–W18 回归期（Regression）** 全样本实证（[`track02_shielded_maker_hedge_20261126.json`](../.agent-state/research/track02_shielded_maker_hedge_20261126.json)，每笔 `$1,000`，仅认严格穿价成交）：

| 队列与配对 | 跨币/域标志 | W19 冻结参数 (`e0/τ/shield`) | W20–W24 报价对冲盈亏 (盾护 vs 无盾) | **W20–W24 真实成交对冲 (盾护 vs 无盾)** | W15–W18 真实成交对冲 | 对冲滑点改善 / 截杀毒单 | `rtt80` / Arcus `lvl0` 敏感性 |
| --- | --- | --- | ---: | ---: | ---: | ---: | ---: |
| **A · `core>rh ETH (0/0)`** | ⚠️ `USDG` / 同域 | `4.0 / 1h / 4.0` | `+$73.96` vs `+$54.41` (4/5正) | **`+$10.66`** vs `-$21.10` | **`+$24.59`** vs `+$4.11` | `-3.46 -> -3.01 bp` / 截 **203** 笔 | `+$52.97` (`rtt80`) |
| **A · `core>rh SOL (0/0)`** | ⚠️ `USDG` / 同域 | `4.0 / 1h / 4.0` | `+$25.63` vs `+$20.97` (5/5正) | **`+$9.26`** vs `-$13.18` | **`+$2.05`** vs `-$0.05` | `-4.06 -> -3.33 bp` / 截 **167** 笔 | `+$13.47` (`rtt80`) |
| **A · `rh>core SOL (0/0)`** | ⚠️ `USDG` / 同域 | `2.0 / 10m / 1.5` | `+$13.64` vs `+$10.17` (4/5正) | **`+$11.57`** vs `+$5.77` | **`+$3.49`** vs `+$0.43` | `-2.55 -> -2.01 bp` / 截 **170** 笔 | `+$3.07` (`rtt80`) |
| **A · `rh>core BTC (0/0)`** | ⚠️ `USDG` / 同域 | `2.0 / 10m / 1.5` | `+$31.78` vs `+$16.63` (3/5正) | **`+$5.81`** vs `-$14.73` | **`+$45.48`** vs `-$0.57` | `-2.04 -> -1.78 bp` / 截 **264** 笔 | `+$1.61` (`rtt80`) |
| **A · `rh>core AMZN (0/0)`** | ⚠️ `USDG` / 同域 | `2.0 / 1h / 2.5` | `+$53.92` vs `+$53.39` (4/4正) | **`+$52.16`** vs `+$51.56` | **`+$23.71`** vs `+$22.91` | `-0.75 -> -0.73 bp` | `+$48.66` (`rtt80`) |
| **A · `rh>core MU / AAPL / TSLA / QQQ`** | ⚠️ `USDG` / 同域 | `2.0 / 1h / None–1.5` | **`+$31.48 / +16.35 / +12.07 / +16.43`** (全部 4/4 正，除 `SPY` 3/4 `+$4.11`) | **`+$29.47 / +$14.60 / +$13.58 / +$12.25`** | **`+$18.59 / +$6.78 / +$15.52 / +$1.07`** | `rh>core NVDA` 盾护将真实成交从 `+$4.16 -> +$7.69` (`+$25.28` 报价) | 全组 `rtt80` 保持正 |
| **B · `lighter>arcus ETH`** | ✅ 独立域 `USDC` | `6.0 / 1h / None` | **`+$108.70`** (5/5正) | **`+$20.49`** (5/5正) | **`+$47.93`** (4/4正) | `-1.42 bp` (Arcus `lvl4=1.15`仅RWA；Crypto为`2.30`) | `+$96.07` (`rtt80`) |
| **B · `lighter>arcus QQQ`** | ✅ 独立域 `USDC` | `4.0 / 1h / None` | **`+$18.54`** (4/4正) | **`+$12.87`** | **`+$11.44`** | `-0.45 bp` | **`+$7.85` (`lvl0 2.25`)** |
| **B · `lighter>arcus NVDA`** | ✅ 独立域 `USDC` | `6.0 / 10m / 4.0` | **`+$111.98`** (4/4正) | **`+$4.66`** vs `+$2.93` | **`+$37.69`** vs `+$36.94` | `-1.23 -> -1.19 bp` | **`+$82.33` (`lvl0 2.25`)** |
| **B · `lighter>arcus SPY`** | ✅ 独立域 `USDC` | `2.0 / 1h / 1.5` | `+$21.16` (4/4正) | **`+$9.97`** | `+$5.07` | `-0.15 bp` | `-$15.33` (`lvl0 2.25`) |
| **B · `rh>arcus QQQ / SPY`** | ⚠️ `USDG` / 独立域 | `2.0 / 10m / None` | `+$37.70 / +$27.22` (4/4正) | `+$16.04 / +$9.14` | `-$4.12 / +$7.44` | 强依赖 Arcus `level_4 (1.15 bp)` | `-$44.31 / -$33.26` (`lvl0`) |
| B · `lighter>arcus BTC / SOL` & `rh>ext` | — | `4–6 / 10m–1h` | 报价虽正 (`+$2~+$155`) | 真实成交 **`-$65.06 / -$5.40 / -$4.09 / -$0.62`** | 全负 | 仅 `rh>extended ETH` 盾护将真实成交亏损从 `-$17.88` 压至 `-$0.62` (`-96.5%`) | 不建议单作 BTC 跨所 |

**核心结论**：
1. **Cohort A RWA（7 标全胜 `W15–W18` & `W20–W24` 双阶段、引号与真实成交双正）** 与 **Crypto `core>rh ETH/SOL`、`rh>core SOL/BTC`** 证明：借助 Lighter 200/300 ms 速度缓冲窗口内的 Binance 外部撤单盾护，能把原本真实成交亏损 `-$13 ~ -$21` 的 Maker-First 策略**系统性地翻正**。注意该队列同属 Lighter 生态且涉及 `USDC/USDG`，必须配齐 `CrossStablecoinHedgePolicy` 且不充当跨风控域额度。
2. **Cohort B 跨风控域（`USDC/USDC` 零 FX 风险）** 中，**`lighter>arcus ETH`**（Crypto）与 **`lighter>arcus QQQ` / `NVDA`**（RWA，即使 Arcus 在 `level_0 = 2.25 bp` 仍保持 `+$7.85 / +$82.33` 正盈利）是最扎实的可跨所真实盈利底座；此前在 [`TRACK01_TRACK02...md`](TRACK01_TRACK02_LEAD_LAG_AND_MAKER_HEDGE_2026_11_26.md) 用 100ms 步长且无盾护时看亏的 `ETH`，在 50ms + 慢所挂单 (`Lighter` `0/0`) 对冲快所 (`Arcus`) 下彻底跑通。
3. **Rust 解耦落地**：[`basis_maker_hedge.rs`](../src/strategy/basis_maker_hedge.rs) 新增 opt-in `RefCancelShieldConfig` 与热路径零堆分配的 `[RefPoint; 64]` 环形缓冲区 `observe_reference`，单边撤出暴露在外部毒流下的挂单。

---

## 四、全栈验证门禁与 `HOLD` 上线前提

- **Mac 全栈验证（`task-1372`，覆盖全部源码与测试终态，退出码 `0`）**：
  - `make fmt-check=0`、`make check=0`、`make build=0`（`release` `1m 56s`）、`git diff --check=0`、`STAGED=0`。
  - `(cd feeder && go test -race -count=1 ./...)` 全 5 包通过（`perp-capture 4.066s`、`config 1.522s`、`exchanges 1.913s`、`wsutil 6.374s`、`shm 1.423s`）。
  - `make test=0`：Rust lib **`1087 passed; 0 failed; 6 ignored`**；bins **`21+9+11+0+3+11`**；Go feeder 5 包过；`make test-research` **`479`** 项通过（`12.162s`）。
  - Python：`make test-python` **`55`** 项通过（`0.721s`）；`python3 -m unittest`（[`test_track01_track02_research.py`](../scripts/test_track01_track02_research.py) `14` 项 + [`test_sim_queue_priority_mm.py`](../scripts/test_sim_queue_priority_mm.py) + [`test_eval_multivenue_three_tracks.py`](../scripts/test_eval_multivenue_three_tracks.py)）共 **`24`** 项通过（`0.017s`）。
  - 冻结与隔离：`inventory_neutral_mm.rs` SHA-256 = `ce0930d3a843ee4b8a15d27f3d9987ec1cea7d2c60bcd1c5a3a4064501fe1488`；所有新策略与既有解耦模块零 `crate::exchanges::*` 耦合。
  - *未运行范围*：前端 `dashboard/`、6 个 `#[ignore]` 基准及 WSL 全套测试。
- **严格维持 `HOLD` 与放行清单**：
  1. **Capture / Lakehouse 落地顺序（需授权窗口后操作）**：先合并 `@Codex` 的 `scripts/quant_lakehouse.py`（增加 `channel == "bbo"` 对 `Arcus` 与 `Hyperliquid` 的解析），再按流程授权更新并重启 `oasis-tokyo` 服务以开始收集真实 `30–118 ms` 盘口；随后用新采集数据重跑 Track 03 Arcus 排队校准与 `lighter>arcus` 实测。
  2. **Track 01**：先将 `lighter Core ETH/SOL ← binance`（大深度）与 `lighter_rh ETH/SOL ← binance`（小单深控）写入 `methodology_inventory.ndjson` 并在 W26+ 预注册前瞻；禁止直接接入高频被动止盈模式于顺势领跑信号。
  3. **Track 02**：优先准备 **`lighter>arcus QQQ/NVDA`（`USDC/USDC`）** 与 **`lighter>arcus ETH`** 的执行适配器接线与 `RefCancelShieldConfig` 联动（`BTC/SOL` 暂不放行跨所）；`rh <-> core` 组合在启用 `CrossStablecoinHedgePolicy` 前保持仅离线验证。
