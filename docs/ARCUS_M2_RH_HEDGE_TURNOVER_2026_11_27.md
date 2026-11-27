# Method 2 Empirical Turnover Benchmark: Arcus Passive Maker × Lighter-RH Taker Hedge (`W01–W27`)

Date: 2026-11-27 · Scope: Tokyo `W01–W27` (`21` RWA windows, `26` Crypto windows, `27` Bulk windows; **`-w28` strictly excluded**) · Valuation Regime: **`zero_points` (points value = `$0`)**, small capital (`$100` default clip / `$300` max inventory; no self-trade/S2S rebate) · Live posture: **HOLD / DO NOT DEPLOY FOR ZERO-POINTS TURNOVER**

- **Evaluator Script**: [scripts/research_arcus_rh_maker_hedge_turnover.py](../scripts/research_arcus_rh_maker_hedge_turnover.py) (pure stdlib top-level imports, `python3 -S` verified; `duckdb` lazy-loaded inside lakehouse workers)
- **Unit & Parity Test Suite**: [scripts/test_research_arcus_rh_maker_hedge_turnover.py](../scripts/test_research_arcus_rh_maker_hedge_turnover.py) (`6/6` tests passing under `python3 -S`)
- **Out-of-Git JSON Artifact**: `.agent-state/research/arcus_turnover_20261127/arcus_m2_rh_hedge_w01_w27.json` (`sha256: 470d3c972e90233637626d3ce377967bf8f3af13fc7f2a2f7275e20ca3aa89a2`, `2,494,181` B)
- **Fee Provenance**: `.agent-state/research/account_fee_evidence_20261127.json` (`sha256: 4d3a9c1aeae5358bb9c55ab448db392ac3edb0a89244f74c23d89817c3359d92`) + `.agent-state/research/arcus_turnover_20261127/venue_fee_probe_20261127.json` (`sha256: 31080569dc54a5155fd903aa59a7c1933f42fb1a0c94d51e58b3b2f6808f3b91`, probed by `@fee-probe`)

---

## 1. Executive Verdict (`zero_points` vs `<= $2/M` Cost Target)

**Method 2 (`Arcus 0.00 bps RWA Maker × Lighter-RH 0.00 bps Taker Hedge`) fails the `<= $2/M` zero-points turnover threshold by two orders of magnitude across all `W01–W27` windows (`$215.61–$377.15/M` Arcus single-sided cost; `$107.84–$188.71/M` combined two-leg cost at `0 bp` FX drag).**

1. **Zero Fee Is Not Zero Cost**: Even with `0.00 bps` Arcus RWA maker fee, `0.00 bps` Lighter-RH Standard taker fee, and `0 bps` FX drag (`e = 0.0` baseline, `$5.174M` Arcus / `$10.342M` combined volume across `98,833` fills), passive spread capture (`+$350.17`, `+$67.68/M` Arcus) is overwhelmed by **Arcus adverse selection (`-$870.49`, `-$168.25/M`)** plus **Lighter-RH crossing spread and `330 ms` speed-bump flight slippage (`-$722.63`, `-$139.67/M`)**, yielding net PnL **`-$1,245.18`** (`$240.67/M` Arcus single-sided cost, `$120.40/M` combined two-leg cost).
2. **`USDG/USDC` FX Drag Compounds Losses Linearly**: Under realistic `1 bp` (`3 bps`) `USDG/USDC` settlement drag, `e = 0.0` cost worsens to **`$340.50/M` (`$540.15/M`)** per Arcus `$1M` and **`$170.34/M` (`$270.22/M`)** per combined two-leg `$1M`.
3. **Consistency Across Splits & Symbols**: Out-of-sample splits confirm the structural cost (`e = 0.0`, `fx = 0 bp`): `selection_dev` (`w01–w20`) `$244.85/M` Arcus (`$122.48/M` 2-leg), `holdout` (`w21–w25`) `$238.18/M` (`$119.17/M` 2-leg), and `w26_w27_oos` `$190.95/M` (`$95.53/M` 2-leg). Even the single cheapest RWA symbol (`AMZN`, `$124.00/M` Arcus / `$62.07/M` 2-leg) and calmest session (`weekend`, `$194.59/M` Arcus / `$97.41/M` 2-leg) remain `> 30x–60x` above `$2/M`.
4. **Method 3–6 & Extended Comparison**: No alternative hedge venue clears `$2/M` either:
   - **法5 (`Arcus × Bulk`)** costs `$462.63/M` Arcus (`$231.27/M` 2-leg) on 7 overlapping 1:1 RWA stocks (`1.50 bps` Bulk RWA fee) and `$609.34/M` (`$305.29/M` 2-leg) on Crypto (`1.425 + 3.50 bps`).
   - **Bonus (`Arcus × Extended` Crypto)** costs `$493.17/M` (`$248.43/M` 2-leg) on `W01–W27` (`1.425 + 2.25 bps`), while **Extended RWA** (`0.90 bps` taker fee, `0` lakehouse RWA files) and **法6 (`Arcus × Backpack`, `4.50 bps` taker fee)** already face `$90/M` (`$45/M` 2-leg) and `$450/M` (`$225/M` 2-leg) pure fee floors on RWA before spread/adverse/slippage.
   - **法4 (`Arcus × TradeXYZ`)** and **法3 (`Arcus × Entropy`)** have `$90/M` (`$45/M` 2-leg) `growthMode` fee floors (`0.90 bps` taker; `xyz:MSTR` is `9.00 bps` non-growth!). In the Tokyo lakehouse, `trade_xyz` and `entropy` BBO captures are **`12.4 s` REST polling snapshots** (`dt_p50 = 12,372 ms` vs `53 ms` on RH and `200 ms` on Arcus), which causes severe stale-book blocking at `5 s` freshness (`70k–85k` blocks, `$59k–$91k` unhedged) and illusory positive `rh_hedge_slip` (`+$1,762` / `+$1,079`) under `60 s` freshness — proving lakehouse HIP-3 BBO snapshots cannot be naively replayed as 200ms WS feeds.

---

## 2. Method 2 Volume vs `$/M` Pareto Frontier (`W01–W27`, 14 RWA Symbols)

Configuration: `clip_usd = $100`, `max_inventory_usd = $300`, `lead_beta = 0.5`, `use_microprice = true`, `fair_basis_tau_ms = 1,800,000` (30m causal EWMA), `queue_ahead_fraction = 0.5`, `submit_rtt_ms = 100`, `cancel_ms = 150`, `hedge_delay_ms = 330`, `orphan_retry_delay_ms = 330`, `max_l1_depth_fraction = 1.0`, `toxic_mode = none`.

| `entry_edge_bps` | Fills | Arcus Vol | 2-Leg Vol | Orphan Rate | `0 bp` FX: Net PnL | `0 bp`: Arcus Cost (`$/M`) | `0 bp`: 2-Leg Cost (`$/M`) | `1 bp`: Arcus / 2-Leg (`$/M`) | `3 bp`: Arcus / 2-Leg (`$/M`) |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `-1.00` | `227,237` | `$11.416M` | `$22.823M` | `6.42%` | `-$2,461.28` | `$215.61` | `$107.84` | `$315.46` / `$157.79` | `$515.18` / `$257.69` |
| `-0.50` | `165,364` | `$8.480M` | `$16.952M` | `7.16%` | `-$1,855.08` | `$218.77` | `$109.43` | `$318.62` / `$159.38` | `$518.32` / `$259.27` |
| `0.00` | `98,833` | `$5.174M` | `$10.342M` | `8.56%` | `-$1,245.18` | `$240.67` | `$120.40` | `$340.50` / `$170.34` | `$540.15` / `$270.22` |
| `0.25` | `75,023` | `$3.925M` | `$7.844M` | `9.44%` | `-$1,017.29` | `$259.21` | `$129.68` | `$359.01` / `$179.62` | `$558.63` / `$279.49` |
| `0.50` | `58,727` | `$3.067M` | `$6.130M` | `10.58%` | `-$852.46` | `$277.94` | `$139.06` | `$377.74` / `$188.99` | `$577.33` / `$288.86` |
| `1.00` | `38,650` | `$1.990M` | `$3.978M` | `12.35%` | `-$640.66` | `$321.87` | `$161.03` | `$421.68` / `$210.97` | `$621.29` / `$310.84` |
| `2.00` | `20,137` | `$1.036M` | `$2.070M` | `15.69%` | `-$390.61` | `$377.15` | `$188.71` | `$476.93` / `$238.64` | `$676.49` / `$338.49` |

### Exact 6-Term Cash PnL Decomposition & Out-of-Sample Split Consistency

Every run satisfies `arcus_spread_capture + arcus_adverse + rh_hedge_slip + fee_cost + fx_drag + residual_drain == total_net_pnl_usd` to `< 1e-6 USD`.

| Edge / FX | Split | Arcus Vol | Spread Cap | Arcus Adv | RH Hedge Slip | Fee | FX Drag | Res Drain | Total Net PnL | Arcus `$/M` | 2-Leg `$/M` |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `e=0.0`, `0 bp` | `selection_dev` (`w01–w20`) | `$3.883M` | `+$270.35` | `-$677.41` | `-$541.86` | `$0.00` | `$0.00` | `-$1.83` | `-$950.75` | `$244.85` | `$122.48` |
| `e=0.0`, `0 bp` | `holdout` (`w21–w25`) | `$1.015M` | `+$65.42` | `-$161.74` | `-$145.65` | `$0.00` | `$0.00` | `+$0.12` | `-$241.85` | `$238.18` | `$119.17` |
| `e=0.0`, `0 bp` | `w26_w27_oos` (`w26–w27`) | `$0.275M` | `+$14.39` | `-$31.34` | `-$35.12` | `$0.00` | `$0.00` | `-$0.52` | `-$52.58` | `$190.95` | `$95.53` |
| `e=0.0`, `1 bp` | `all_w01_w27` | `$5.174M` | `+$350.17` | `-$870.49` | `-$722.63` | `$0.00` | `-$516.48` | `-$2.23` | `-$1,761.66` | `$340.50` | `$170.34` |
| `e=0.5`, `0 bp` | `selection_dev` (`w01–w20`) | `$2.324M` | `+$251.13` | `-$561.50` | `-$362.29` | `$0.00` | `$0.00` | `-$1.04` | `-$673.71` | `$289.87` | `$145.02` |
| `e=0.5`, `0 bp` | `holdout` (`w21–w25`) | `$0.599M` | `+$60.01` | `-$116.56` | `-$92.57` | `$0.00` | `$0.00` | `-$0.01` | `-$149.13` | `$248.92` | `$124.57` |
| `e=0.5`, `0 bp` | `w26_w27_oos` (`w26–w27`) | `$0.144M` | `+$11.61` | `-$21.40` | `-$19.69` | `$0.00` | `$0.00` | `-$0.14` | `-$29.62` | `$205.99` | `$103.08` |
| `e=1.0`, `0 bp` | `all_w01_w27` | `$1.990M` | `+$294.99` | `-$589.24` | `-$346.22` | `$0.00` | `$0.00` | `-$0.19` | `-$640.66` | `$321.87` | `$161.03` |

---

## 3. Per-Symbol `$/M` League Table & UTC Session Breakdown (`e = 0.0`, `0 bp` FX)

### 3.1 Per-Symbol Breakdown Across All 21 RWA Windows

| Symbol | Fills | Arcus Vol | Net PnL (`0 bp`) | Arcus Cost (`$/M`) | 2-Leg Cost (`$/M`) | Orphan Rate | Spread Cap | Arcus Adv | RH Slip |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `AMZN` | `8,627` | `$0.605M` | `-$74.96` | `$124.00` | `$62.07` | `6.03%` | `+$42.58` | `-$53.10` | `-$63.88` |
| `SPY` | `11,086` | `$0.729M` | `-$99.10` | `$135.99` | `$67.99` | `4.17%` | `+$23.24` | `-$55.85` | `-$66.45` |
| `QQQ` | `14,577` | `$0.844M` | `-$128.78` | `$152.55` | `$76.25` | `4.60%` | `+$24.54` | `-$76.54` | `-$76.94` |
| `AAPL` | `4,776` | `$0.289M` | `-$51.23` | `$177.02` | `$88.57` | `4.47%` | `+$12.39` | `-$33.25` | `-$30.22` |
| `TSLA` | `5,016` | `$0.178M` | `-$40.30` | `$226.30` | `$113.33` | `13.74%` | `+$10.55` | `-$29.43` | `-$21.06` |
| `MSFT` | `1,572` | `$0.094M` | `-$23.05` | `$245.31` | `$122.82` | `8.60%` | `+$5.38` | `-$16.71` | `-$11.81` |
| `GOOGL` | `4,939` | `$0.292M` | `-$71.63` | `$245.71` | `$123.28` | `27.79%` | `+$13.70` | `-$50.82` | `-$34.04` |
| `META` | `1,777` | `$0.112M` | `-$28.48` | `$253.84` | `$126.94` | `5.49%` | `+$8.47` | `-$19.77` | `-$17.19` |
| `AMD` | `6,514` | `$0.344M` | `-$111.52` | `$324.50` | `$162.20` | `0.91%` | `+$33.43` | `-$68.81` | `-$75.62` |
| `NVDA` | `26,801` | `$1.125M` | `-$386.08` | `$343.20` | `$171.62` | `12.98%` | `+$101.25` | `-$299.83` | `-$187.66` |
| `CRCL` | `2,550` | `$0.138M` | `-$50.50` | `$366.33` | `$183.70` | `19.78%` | `+$21.51` | `-$46.68` | `-$24.90` |
| `MU` | `8,259` | `$0.329M` | `-$136.41` | `$414.09` | `$207.00` | `4.92%` | `+$37.36` | `-$86.44` | `-$87.03` |
| `PLTR` | `738` | `$0.032M` | `-$14.15` | `$448.61` | `$224.30` | `1.68%` | `+$5.31` | `-$11.19` | `-$8.11` |
| `COIN` | `1,601` | `$0.064M` | `-$28.98` | `$454.56` | `$228.41` | `17.67%` | `+$10.45` | `-$22.07` | `-$17.73` |

### 3.2 UTC Session Breakdown (`0 bp` FX, Intraday Net)

| Session | `e = 0.0` Fills | `e = 0.0` Arcus Vol | `e = 0.0` Net | `e = 0.0` Arcus / 2-Leg (`$/M`) | `e = 0.5` Arcus / 2-Leg (`$/M`) |
| --- | ---: | ---: | ---: | ---: | ---: |
| `weekend` | `17,394` | `$0.988M` | `-$192.35` | `$194.59` / `$97.41` | `$216.22` / `$108.22` |
| `overnight_asia` | `33,983` | `$1.679M` | `-$337.01` | `$200.74` / `$100.40` | `$231.81` / `$115.95` |
| `us_extended` | `18,222` | `$0.900M` | `-$196.33` | `$218.15` / `$109.11` | `$259.39` / `$129.76` |
| `us_rth` | `29,234` | `$1.607M` | `-$517.27` | `$321.97` / `$161.07` | `$356.47` / `$178.37` |

US RTH (`13:30–20:00 UTC`) is `~60%` costlier per `$1M` than quiet overnight/weekend sessions (`$321.97/M` vs `$194.59–$200.74/M`), while `GOOGL` (`27.79%`), `CRCL` (`19.78%`), and `COIN` (`17.67%`) suffer high Lighter-RH L1 orphan rates.

---

## 4. P0 Latency, Depth, `lead_beta`, Clip Size, and Toxic-Fill Stress (`e = 0.0`, `0 bp` FX)

### 4.1 Maker Submit/Cancel Latency & Cancel-in-Flight (`cif`) Sensitivity

| `submit_rtt_ms` | `cancel_ms` | Fills | `cif` Fills | Arcus Vol | Net PnL | Arcus `$/M` |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `100` | `50` | `71,420` | `20,966` | `$3.885M` | `-$785.77` | `$202.25` |
| `200` | `150` | `74,040` | `35,264` | `$3.885M` | `-$823.59` | `$211.99` |
| `50` | `50` | `87,304` | `27,578` | `$4.736M` | `-$1,055.61` | `$222.89` |
| `100` | `150` | `98,833` | `51,574` | `$5.174M` | `-$1,245.18` | `$240.67` |
| `200` | `300` | `102,688` | `65,770` | `$5.225M` | `-$1,290.40` | `$246.95` |
| `50` | `150` | `111,790` | `57,921` | `$5.880M` | `-$1,481.49` | `$251.97` |
| `100` | `300` | `123,119` | `79,530` | `$6.285M` | `-$1,658.80` | `$263.92` |

### 4.2 Hedge Flight Latency (`hedge_delay_ms`), `lead_beta`, and Clip Size Sensitivity

- **Hedge Flight Delay (`150 / 330 / 500 ms`)**: At `e = 0.0` (`$5.174M` Arcus vol), `150 ms` costs `$196.76/M` (`-$1,018.00`, `10.36%` orphan rate), `330 ms` costs `$240.67/M` (`-$1,245.18`, `8.56%` orphan rate), and `500 ms` costs `$255.03/M` (`-$1,319.48`, `7.26%` orphan rate).
- **`lead_beta` Ablation**: `lead_beta = 0.0` (`$232.36/M`, `-$1,060.99` on `$4.566M`) outperforms `lead_beta = 0.5` (`$240.67/M`, `-$1,245.18`) and `lead_beta = 1.0` (`$265.08/M`, `-$1,800.99` on `$6.794M`), because shifting quotes with positive `lead_beta` increases fills during adverse momentum bursts.
- **Clip Size (`$50 / $100 / $200 / $500`, `max_inv = 3x` clip)**: Cost per `$1M` stays flat around `~$238–$246/M` Arcus (`~$120–$123/M` 2-leg), while **orphan rate rises monotonically** with clip size: `$50` (`5.50%`, `$2.926M`), `$100` (`8.56%`, `$5.174M`), `$200` (`11.40%`, `$8.818M`), and `$500` (`17.79%`, `$17.291M`, `-$4,119.41` net).

### 4.3 Mandatory Unprinted Toxic-Fill Stress (`e = 0.0`, `0 bp` FX)

| Stress Mode | Total Fills | Unprinted Toxic Fills | Arcus Vol | Spread Cap | Arcus Adv | RH Slip | Net PnL | Arcus `$/M` | 2-Leg `$/M` |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `none_0` (baseline) | `98,833` | `0` | `$5.174M` | `+$350.17` | `-$870.49` | `-$722.63` | `-$1,245.18` | `$240.67` | `$120.40` |
| `strict_through_25` | `365,811` | `272,294` | `$10.194M` | `-$291.01` | `-$849.54` | `-$1,107.47` | `-$2,249.21` | `$220.63` | `$110.33` |
| `strict_through_50` | `364,694` | `271,027` | `$16.081M` | `-$946.01` | `-$905.38` | `-$1,637.55` | `-$3,489.15` | `$216.98` | `$108.50` |
| `strict_through_100` | `384,824` | `291,803` | `$33.660M` | `-$2,760.86` | `-$1,194.12` | `-$3,326.25` | `-$7,279.96` | `$216.28` | `$108.24` |
| `opposite_touch_25` | `565,377` | `473,014` | `$14.176M` | `-$433.60` | `-$853.95` | `-$1,434.77` | `-$2,722.28` | `$192.04` | `$96.03` |
| `opposite_touch_50` | `565,147` | `472,359` | `$24.490M` | `-$1,252.53` | `-$945.15` | `-$2,379.35` | `-$4,575.06` | `$186.81` | `$93.42` |

Under unprinted book crosses, `arcus_spread_capture` collapses from `+$350.17` to `-$946.01` (`strict_through_50`) and `-$2,760.86` (`strict_through_100`), increasing absolute cash burn by `2.80x` to `5.85x`.

---

## 5. Multi-Venue Head-to-Head Comparison (`W01–W27` Empirical + `@fee-probe` Verified Rates)

| Method & Pair | Verified Fee (Mk / Tk) | Lakehouse Replay Scope (`W01–W27`) | Empirical Arcus / 2-Leg Cost (`$/M`, `e = 0.0`) | Fee Floor Only (Arcus / 2-Leg) | Empirical Finding & Lakehouse Data Caveat |
| --- | --- | --- | --- | --- | --- |
| **法2: `Arcus × Lighter-RH`** | `0.00` / `0.00` (`USDG`) | `14` RWA (`21w`), `12`-stock intersect | `$240.67` / `$120.40` (`0 bp` FX) · `$282.29` / `$141.26` (12-stk) | `$0` / `$0` (`+100/50` per `1 bp` FX) | `53 ms` WS BBO; `330 ms` speed bump + adverse flow makes it `$241–$540/M` Arcus (`0–3 bp` FX). |
| **法3: `Arcus × Entropy` (`io:*`)** | `0.00` / `0.90` (`0.864` ref) | `5` RWA (`21w`: `AAPL, CRCL, DRAM, NVDA, SNDK`) | `5 s` stale: `$25.84` / `$14.76` (`35,462` stale blks, `$28.7k` unhedged); `60 s` diag: `-$148.64` / `-$77.89` (`rhl=+$1,079.2`) | `$90.00` / `$45.00` | **`12.4 s` REST polling BBO artifact** (`dt_p50=12,372 ms`, L1 `$4,017`). True fee + spread (`1.08 bp`) + adverse > `$250/M` Arcus. Exclude `io:PRL` (`9.00 bps`). |
| **法4: `Arcus × TradeXYZ` (`xyz:*`)** | `0.00` / `0.90` (`MSTR` `9.00`) | `16` 1:1 RWA (`21w`) & `12`-stk intersect (excl. `XYZ100`/`SP500` index scale mismatch) | `5 s` stale (`12`-stk): `$64.41` / `$35.61` (`70,505` blks, `$68.6k` unhedged); `60 s` diag: `-$136.19` / `-$69.04` (`rhl=+$1,761.5`) | `$90.00` / `$45.00` (`$900` on `MSTR`) | **`12.4 s` REST polling BBO artifact** (`dt_p50=12,372 ms`, L1 `$19,945`). Excluding `xyz:MSTR` (`9.00 bps`) & index scale mismatches (`XYZ100 ~26.2k`, `SP500 ~7.2k`), real-time fee (`$90/M`) + crossing (`~$36/M`) + adverse (`~$150/M`) exceeds `$250/M` Arcus. |
| **法5: `Arcus × Bulk`** | RWA `0.00` / `1.50` · Crypto `1.425` / `3.50` | `7` 1:1 RWA (`21w`, excl. `US500`/`SPY`) + `3` Crypto (`26w`) | **RWA**: `$462.63` / `$231.27` · **Crypto**: `$609.34` / `$305.29` | RWA `$150` / `$75` · Crypto `$492.5` / `$246.3` | `5 Hz` (`200 ms`) BBO snapshots, `0` `public_trades` in lakehouse; `0` stale blocks, `100%` hedged. **No taker rebate (`< 0 bps`)**; `14` active RWA markets confirmed by `@fee-probe`. |
| **法6: `Arcus × Backpack`** | RWA `0.00` / `4.50` · Crypto `1.425` / `4.50` | `0` Tokyo lakehouse files | Not replayable in Tokyo lakehouse; fee floor alone is `$450/M` (`$225/M` 2-leg) RWA | RWA `$450` / `$225` · Crypto `$592.5` / `$296.3` | `19` `.US_USDC_PERP` RWA + `85` Crypto perps verified; `4.50 bps` taker fee makes zero-points farming unviable. |
| **Bonus: `Arcus × Extended`** | RWA `0.00` / `0.90` · Crypto `1.425` / `2.25` | `3` Crypto (`26w`: `BTC, ETH, SOL`; `0` RWA lakehouse files) | **Crypto**: `$493.17` / `$248.43` (`fee=-$1,546.22` on `$4.246M`) | RWA `$90` / `$45` · Crypto `$367.5` / `$183.8` | Continuous `ws_50ms` Crypto replay confirms high cost (`$493/M` Arcus); RWA (`0.90 bps` taker across `183` mkts) faces same `$90/M` fee floor as TradeXYZ/Entropy. |

---

## 6. Deterministic 24-Event P0 Parity Fixture & Verification

`build_synthetic_parity_fixture()` in [scripts/research_arcus_rh_maker_hedge_turnover.py](../scripts/research_arcus_rh_maker_hedge_turnover.py) defines a 24-event deterministic tape (`t0 = 1700000000000`, `declared_end_ms = t0 + 10000`, `terminal_drain_ms = 500`, `hedge_delay_ms = 330`, `submit_rtt_ms = 50`, `cancel_ms = 100`) shared with `@live-infra` for Rust P0 parity:
- **Expected Outputs**: `arcus_fills = 4`, `hedge_fills = 4`, `orders_placed = 8`, `cancels_sent = 6`, `cancel_in_flight_fills = 1`, `orphan_events = 1`, `orphan_retries = 4`, `depth_clipped_hedges = 2`, `stale_hedge_blocks = 2`, `terminal_pending_hedges = 1`, `terminal_unhedged_orphans = 1`, `arcus_volume_usd = $298.9749`, `hedge_volume_usd = $248.9700`, `combined_volume_usd = $547.9449`, `arcus_spread_capture = +$0.0497`, `arcus_adverse = +$0.0010`, `rh_hedge_slip = -$0.0358`, `total_net_pnl_usd = +$0.0149`, `conservation_error_usd = 0.0`.
- **Checks Executed**:
  - `python3 -S scripts/research_arcus_rh_maker_hedge_turnover.py --self-test` — PASS (`24` events, `conservation_error_usd = 0.0`).
  - `python3 -S -m unittest -v scripts/test_research_arcus_rh_maker_hedge_turnover.py` — `6/6` tests PASS (`0.063s`, includes isolated `python3 -S` blocking `numpy`/`duckdb`/`pyarrow`/`pandas`/`scipy`, `-w28` exclusion, `0 ms` hedge delay rejection, single-use L1 depth conservation, `strict_through` / `opposite_touch` unprinted toxic fills, and FX clone parity).
