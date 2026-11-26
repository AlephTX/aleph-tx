# 场景一刷量 P0–P3 真实 10 小时 RWA 湖仓回放与实证报告

- **日期**：2026-11-27（北京时间）
- **状态**：**纯离线湖仓实证完成（零改动 `src/strategy/*`、`src/exchanges/*`，三轨严格 HOLD）**
- **承接基线**：[SCENARIO1_VOLUME_FARMING_NEUTRAL_EDGE_RESEARCH_2026_11_27.md](SCENARIO1_VOLUME_FARMING_NEUTRAL_EDGE_RESEARCH_2026_11_27.md)（P0–P3 执行方案）
- **复算脚本（纯标准库顶层导入、CI 安全）**：
  - P1 成本口径审计：[eval_volume_farming_cost.py](../scripts/eval_volume_farming_cost.py)（单测 [test_eval_volume_farming_cost.py](../scripts/test_eval_volume_farming_cost.py)）
  - P0 + P2 事件驱动 FIFO 周期回放：[replay_rwa_volume_farming.py](../scripts/replay_rwa_volume_farming.py)（单测 [test_replay_rwa_volume_farming.py](../scripts/test_replay_rwa_volume_farming.py)）
  - P3 分时段宇宙筛选器：[screen_rwa_mm_universe.py](../scripts/screen_rwa_mm_universe.py)（单测 [test_screen_rwa_mm_universe.py](../scripts/test_screen_rwa_mm_universe.py)）
- **实证数据来源（`aleph` WSL 只读 DuckDB `:memory:` 查询）**：
  - `selection_dev`（冻结 4 窗：`w18, w19, w20, w21`）→ `p1_pipeline_bookdiff_rwa_selection.json`（`SHA-256: 229c6cd8117e04335ad1a77598d304ee4c707a8c57e949ff0c1783209eaa6817`，`408/408` 条连续 10h 达标）
  - `holdout`（未触碰盲测 3 窗：`w23, w24, w25`，冻结参数单次执行）→ `p1_pipeline_bookdiff_rwa_holdout.json`（`SHA-256: f9b06c524dc74a5daa0af40ebc687bd97f03d5f02a9a2c42d20e692e31d28c24`，`306/306` 条连续 10h 达标）
  - P3 筛选结果（`w18–w21`）→ `rwa_mm_whitelist_selection.json`（`SHA-256: af627a4d93318afcbe3fe4cbabe06b8d749ce52ac9d711730c7eb837211fc0b0`）
  - 账户费档证据（`OPT-01`）→ [account_fee_evidence_20261123.json](../.agent-state/research/account_fee_evidence_20261123.json)

---

## 0. 核心裁决（直答 P0 命门与策略边界）

Claude 在研究文档第 4 节提出的预警（NVDA 静态 `half_spread / 1s_move = 0.84 < 1`，静态正毛边极可能被成交后逆向漂移吃光）**已在 `aleph` 7 组完整 10 小时真实连续 RWA 湖仓回放中被彻底证实**，并按场所呈现**根本分化**：

1. **`arcus` RWA（含高静态毛边 `DRAM +1.43bp` 与 `SNDK/NVDA/QQQ/SPY/AAPL/TSLA`）全部证伪（`NO-GO`）**：
   - `DRAM @ arcus` 虽然静态半价差高达 `+1.38–1.49 bp`，但在 10h 真实 `public_trades` 队列回放中，成交后漂移（`drft = -2.14` 至 `-5.76 bp`）、高位 IOC 退出比例（`s = 75%–100%`）、Taker 平仓滑点（`tslp = -1.35` 至 `-1.61 bp`）与 `2.25 bp` Taker 费率叠加，使 **全部 6 组策略臂在 `selection_dev`（`28/28` 窗）与 `holdout`（`21/21` 窗）上 `gross_ex_fee_bps` 与 `net_per_ntl_bp` 无一例转正**。
   - **暂不改动 `src/strategy/*` 或 `src/exchanges/*` 去上线 `arcus` RWA 纯刷量**（后续需等待启用 `bbo` 流后的 `W27+` 数据重验微秒撤单能否把 `drft` 压下来）。
2. **`lighter`（`USDC`）与 `lighter_robinhood`（`USDG`）上的 RWA 呈现真实且单调的样本外改善（但仍属低成本刷量 / 接近盈亏平衡，非稳赚）**：
   - **P2 冻结臂在未触碰 `holdout (w23–w25)` 上严格复现样本内改进**：
     - **`lighter`（5 标 15 窗均值，`public_trades`）**：`S0_baseline_touch` `\$44.89/M`（`gross -0.90 bp`）→ `S1_markout_shield` `\$40.43/M` → `G1_as_inv_skew` `\$32.17/M` → `G2_markout_skew_pause` `\$18.88/M` → `G3_markout_skew_oracle` `\$20.31/M` → **`G4_quiet_regime_only` `\$12.58/M`（`gross -0.25 bp`，1s 加权 markout 由 `-0.38 bp` 反转为正 `+0.02 bp`，`29,173` 周期 / `\$2.79M` 退出名义额）**。
     - **`lighter_robinhood`（5 标 15 窗均值，`public_trades`）**：`S0` `\$22.41/M` → `G2` `\$16.68/M` → `G3` `\$16.04/M` → **`G4_quiet_regime_only` `\$12.54/M`（`s = 25.6%`，`\$5.80M` 退出名义额，其中 `SPY G4` 达 `\$6.99/M`、`QQQ G4` 达 `\$10.48/M`）**。
   - **单标正样本实证**：`lighter AAPL` 的 `G4_quiet_regime_only` 在 `w18`（`+0.217 bp / -\$10.84/M`）、`w21`（`+0.058 bp / -\$2.90/M`）与 `holdout w25`（`+0.118 bp / -\$5.92/M`）均实现 10h 整窗正毛边，汇总 3 窗 holdout 仅 **`\$2.41/M`**（`mk1s = +0.15 bp`）。
   - **审计边界**：Lighter `0/0 bps` 仍处于 `public_tier0_unverified`（缺签名 `/api/v1/accountLimits` 实盘档位凭证），且 `lighter_robinhood` 结算币为 `USDG`（若跨币种调仓须单列 FX 换汇磨损）。

---

## 1. P1：无返佣 `USD/M` 成本模型与 `Hyperliquid` Maker 费率纠偏

`eval_volume_farming_cost.py` 按严格双边分母 `2*Q` 与纯 Maker 分母 `(2-s)*Q`（`rebate_bps = 0.0`）对现有 `p1_pipeline_bookdiff_holdout.json` 的 `S1_markout`（`gross_ex_fee_bps = -0.4057 bp`，反推 `s = 18.454%`）核算如下：

| 场所档位 (`account_fee_evidence_20261123.json`) | 审计状态 | Maker (bp) | Taker (bp) | 双边成本 (USD/M) | 纯 Maker 成本 (USD/M) | `s->0` 理论地板 (USD/M) |
|---|---|---:|---:|---:|---:|---:|
| `arcus_ant_tier0` | `public_tier0_schedule_only` | 0.0 | 1.12 | **+30.62** | +33.73 | +20.28 |
| `arcus_rwa_tier0` | `public_tier0_schedule_only` | 0.0 | 2.25 | **+41.04** | +45.22 | +20.28 |
| `hyperliquid_verified` | `verified_account_tier` | **1.5** | 4.50 | **+197.96** | +218.09 | **+170.28** |
| `arcus_crypto_tier0` | `public_tier0_schedule_only` | 1.5 | 4.50 | **+197.96** | +218.09 | +170.28 |
| `trade_xyz_growth` | `verified_account_tier` | 0.3 | 0.90 | +55.82 | +61.49 | +50.28 |
| `trade_xyz_standard` | `verified_account_tier` | 3.0 | 9.00 | +375.65 | +413.83 | +320.28 |
| `lighter_public_tier0` | `public_schedule_only_unverified` | 0.0 | 0.00 | +20.28 | +22.35 | +20.28 |

> **关键发现**：此前在 `S1_markout` 按 zero-maker 口径估算 HL 仅看 `4.5 bp * s` 会误得约 `+\$61.8/M`；计入 [account_fee_evidence_20261123.json](../.agent-state/research/account_fee_evidence_20261123.json) 实测的 **Tier 0 `maker_fee = 1.5 bp`** 后，HL 真实双边刷量成本达 **`+\$197.96/M`**（`s->0` 地板 **`+\$170.28/M`**）。

---

## 2. P0 & P2：10 小时真实 `public_trades` RWA 周期分解（`Selection w18–w21` vs `Holdout w23–w25`）

回测采用 `replay_rwa_volume_farming.py` 严格的逐笔 FIFO 排队模型与精确现金恒等分解（全量验证 `gross_usd == spr + drft + pex + tslp`、`net == gross_usd - fee_usd` 且全部残余仓位在末帧按对价 + Taker 费强平 `terminal_flat == True`）。

### 2.1 分场所 × 策略臂汇总（全部连续 10h 窗）

| 分组 | 场所 (结算币) | 策略臂 | 窗×标数 | 退出名义额 `ntl` | Taker 占比 `s` | `spr` (bp) | `drft` (bp) | `pex` (bp) | `tslp` (bp) | `gross` (bp) | `net` (bp) | `1s mk` (bp) | 双边 `$/M` |
|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `selection` | `arcus` (`USDC`) | `S0_baseline_touch` | 28 | \$4,884,982 | 72.7% | +0.22 | -2.70 | +0.09 | -0.18 | -2.57 | -4.20 | -1.38 | **+210.13** |
| `selection` | `arcus` (`USDC`) | `S1_markout_shield` | 28 | \$3,722,009 | 73.3% | +0.25 | -2.52 | +0.11 | -0.16 | -2.32 | -3.97 | -1.32 | **+198.27** |
| `selection` | `arcus` (`USDC`) | `G2_markout_skew_pause` | 28 | \$740,755 | 75.1% | +0.62 | -2.33 | +0.04 | -0.15 | -1.82 | -3.51 | -0.92 | **+175.37** |
| `selection` | `arcus` (`USDC`) | `G4_quiet_regime_only` | 28 | \$166,606 | 79.0% | +0.73 | -2.36 | +0.06 | -0.24 | -1.81 | -3.59 | -1.00 | **+179.49** |
| `holdout` | `arcus` (`USDC`) | `S0_baseline_touch` | 21 | \$3,481,699 | 80.1% | +0.21 | -2.12 | +0.04 | -0.20 | -2.08 | -3.88 | -1.23 | **+194.07** |
| `holdout` | `arcus` (`USDC`) | `G3_markout_skew_oracle` | 21 | \$455,077 | 73.2% | +0.70 | -2.42 | +0.06 | -0.18 | -1.84 | -3.49 | -0.98 | **+174.45** |
| `selection` | `lighter` (`USDC`) | `S0_baseline_touch` | 20 | \$9,022,756 | 54.4% | +0.21 | -1.12 | +0.10 | -0.26 | -1.07 | -1.07 | -0.48 | **+53.70** |
| `selection` | `lighter` (`USDC`) | `S1_markout_shield` | 20 | \$9,288,749 | 53.9% | +0.19 | -0.90 | +0.09 | -0.26 | -0.89 | -0.89 | -0.38 | **+44.61** |
| `selection` | `lighter` (`USDC`) | `G1_as_inv_skew` | 20 | \$7,371,118 | 40.0% | +0.36 | -1.07 | +0.14 | -0.20 | -0.78 | -0.78 | -0.31 | **+38.79** |
| `selection` | `lighter` (`USDC`) | `G2_markout_skew_pause` | 20 | \$5,198,143 | 43.8% | +0.29 | -0.52 | +0.13 | -0.21 | -0.31 | -0.31 | -0.04 | **+15.74** |
| `selection` | `lighter` (`USDC`) | `G4_quiet_regime_only` | 20 | \$4,106,218 | 45.0% | +0.31 | -0.42 | +0.13 | -0.22 | -0.20 | -0.20 | **+0.03** | **+9.98** |
| `holdout` | `lighter` (`USDC`) | `S0_baseline_touch` | 15 | \$6,027,330 | 58.8% | +0.18 | -0.85 | +0.06 | -0.29 | -0.90 | -0.90 | -0.38 | **+44.89** |
| `holdout` | `lighter` (`USDC`) | `S1_markout_shield` | 15 | \$6,227,047 | 57.9% | +0.16 | -0.75 | +0.06 | -0.29 | -0.81 | -0.81 | -0.32 | **+40.43** |
| `holdout` | `lighter` (`USDC`) | `G1_as_inv_skew` | 15 | \$4,880,178 | 45.6% | +0.27 | -0.78 | +0.10 | -0.23 | -0.64 | -0.64 | -0.21 | **+32.17** |
| `holdout` | `lighter` (`USDC`) | `G2_markout_skew_pause` | 15 | \$3,640,665 | 47.8% | +0.28 | -0.53 | +0.11 | -0.24 | -0.38 | -0.38 | -0.07 | **+18.88** |
| `holdout` | `lighter` (`USDC`) | `G4_quiet_regime_only` | 15 | \$2,793,739 | 49.3% | +0.30 | -0.42 | +0.11 | -0.25 | -0.25 | -0.25 | **+0.02** | **+12.58** |
| `selection` | `lighter_robinhood` (`USDG`) | `S0_baseline_touch` | 20 | \$16,687,433 | 30.1% | +0.25 | -0.82 | +0.18 | -0.17 | -0.56 | -0.56 | -0.20 | **+27.96** |
| `selection` | `lighter_robinhood` (`USDG`) | `G4_quiet_regime_only` | 20 | \$7,930,419 | 22.8% | +0.27 | -0.56 | +0.14 | -0.11 | -0.25 | -0.25 | -0.06 | **+12.72** |
| `holdout` | `lighter_robinhood` (`USDG`) | `S0_baseline_touch` | 15 | \$10,370,716 | 36.0% | +0.24 | -0.63 | +0.15 | -0.21 | -0.45 | -0.45 | -0.12 | **+22.41** |
| `holdout` | `lighter_robinhood` (`USDG`) | `G4_quiet_regime_only` | 15 | \$5,796,700 | 25.6% | +0.28 | -0.54 | +0.15 | -0.14 | -0.25 | -0.25 | -0.03 | **+12.54** |

### 2.2 重点单标对照（未触碰 `Holdout w23–w25`）

| 场所 | 标的 | 最差基准 (`S0` `$/M`) | P2 最佳臂 | 最佳 `gross` (bp) | 最佳 `drft` (bp) | 最佳 `1s mk` (bp) | 最佳 `$/M` | 整窗正收益 |
|---|---|---|---|---:|---:|---:|---:|---:|
| `arcus` (`USDC`) | `DRAM` | +240.56 (`S0`) | `G2_markout_skew_pause` | -1.73 | -2.14 | -0.36 | **+196.03** | 0/3 |
| `arcus` (`USDC`) | `SNDK` | +343.25 (`S0`) | `G2_markout_skew_pause` | -2.99 | -3.52 | -1.90 | **+252.99** | 0/3 |
| `arcus` (`USDC`) | `NVDA` | +226.64 (`S0`) | `G4_quiet_regime_only` | -2.41 | -3.06 | -1.24 | **+205.38** | 0/3 |
| `lighter` (`USDC`) | **`AAPL`** | +21.73 (`S0`) | **`G4_quiet_regime_only`** | **-0.05** | -0.42 | **+0.15** | **+2.41** | **1/3** (`w25: -\$5.92/M`) |
| `lighter` (`USDC`) | `SPY` | +26.70 (`S0`) | `G4_quiet_regime_only` | -0.26 | -0.40 | -0.02 | **+13.05** | 0/3 |
| `lighter` (`USDC`) | `QQQ` | +36.31 (`S0`) | `G4_quiet_regime_only` | -0.29 | -0.41 | **+0.01** | **+14.54** | 0/3 |
| `lighter` (`USDC`) | `TSLA` | +46.89 (`S0`) | `G4_quiet_regime_only` | -0.35 | -0.34 | -0.01 | **+17.38** | 0/3 |
| `lighter` (`USDC`) | `NVDA` | +72.54 (`S0`) | `G4_quiet_regime_only` | -0.44 | -0.67 | -0.11 | **+21.98** | 0/3 |
| `lighter_robinhood` (`USDG`) | **`SPY`** | +9.52 (`S0`) | **`G4_quiet_regime_only`** | -0.14 | -0.46 | -0.03 | **+6.99** | 0/3 |
| `lighter_robinhood` (`USDG`) | `QQQ` | +15.18 (`S0`) | `G4_quiet_regime_only` | -0.21 | -0.57 | -0.05 | **+10.48** | 0/3 |
| `lighter_robinhood` (`USDG`) | `AAPL` | +14.76 (`S0`) | `G4_quiet_regime_only` | -0.25 | -0.53 | +0.03 | **+12.67** | 0/3 |

---

## 3. P3：标的与 UTC 时段筛选结果（`w18–w21`）

`screen_rwa_mm_universe.py` 按冻结门禁（`maker_fee=0`、`half_spread>=0.25bp`、`spread/vol>=0.45`、`L1>=$300`、`vol_1s<=2.5bp`、`trades/h>=50`）从 23 组 `(venue, symbol)` 中筛出 **9 组通过白名单、14 组剔除**：

- **通过白名单（Top 6）**：
  1. `arcus DRAM` (`USDC`, `score=1.844`, `hs=+1.262bp`, `spr/vol=1.460`, `L1=\$1304`) — 注：静态合格但已被 P0 回放因真实 `drft=-3.7~-5.8bp` 与 `s>86%` 证伪为 `\$196–\$360/M`。
  2. `lighter_robinhood AMZN` (`USDG`, `score=1.576`, `hs=+0.788bp`, `spr/vol=2.434`, `L1=\$399`)
  3. `lighter_robinhood MU` (`USDG`, `score=1.347`, `hs=+1.134bp`, `spr/vol=1.188`, `L1=\$327`)
  4. `lighter_robinhood TSLA` (`USDG`, `score=0.939`, `hs=+0.650bp`, `spr/vol=1.444`, `L1=\$353`)
  5. `lighter AAPL` (`USDC`, `score=0.770`, `hs=+0.456bp`, `spr/vol=1.688`, `L1=\$328`)
  6. `lighter QQQ` / `lighter SPY` (`USDC`, `score=0.638/0.627`) 及 `lighter_robinhood NVDA/AAPL` (`USDG`, `score=0.567/0.399`)。
- **重要剔除**：`arcus SNDK` 因高波动（`vol=2.70 > 2.5bp`）被剔除；`arcus NVDA` 因 `spr/vol=0.34 < 0.45` 被剔除；`arcus QQQ/SPY/AAPL/AMZN/TSLA` 因半价差过窄（`< 0.25bp`）被剔除。
- **UTC 时段分化（直接解释 `G4_quiet_regime_only` 为何在样本外有效）**：
  - `lighter QQQ`：`us_rth`（13:30–20:00 UTC）`hs/move1s = 1.001`，而 **`overnight_asia`（00:00–11:00 UTC）达 `2.438`**、`us_extended` 达 **`2.439`**；
  - `lighter SPY`：`us_rth` `hs/move1s = 1.142`，**`overnight_asia` 达 `4.445`**、`us_extended` 达 **`3.893`**；
  - `arcus NVDA`：`us_rth` `hs/move1s = 0.215`（每 1 秒波动吃掉近 5 倍半价差），`overnight_asia` 也仅 `0.756 < 1`；`arcus DRAM` 在 `us_rth` `0.562`，仅 `overnight_asia` 勉强 `1.105`。

---

## 4. 下一步工程边界建议（不改策略前门，先补证据）

1. **禁止在未看到 `W27+` `bbo` 流回放前把 `arcus` RWA 上线为“正毛边做市”**：`W18–W25` 上全 7 个 RWA 标的真实成本仍在 `+\$137` 至 `+\$363/M`。
2. **如需做低损耗刷量对照（仅研究验证），唯一接近成本地板的路径是 `lighter` / `lighter_robinhood` 上的平静时段 RWA（`AAPL / SPY / QQQ`，`G4` 策略逻辑：`\$2.4–\$14.5/M` 双边）**，但前提仍是完成 `OPT-01` Lighter signed `/api/v1/accountLimits` 实盘零费率凭证校验。
3. **本轮严格守门**：所有脚本均为离线只读/合成测试，**未触碰** `src/strategy/*`、`src/exchanges/*`、`feeder/*`、`oasis-tokyo` 或任何实盘状态。
