# Method 1: Single-Venue Arcus RWA High-Frequency MM Benchmark (`W01–W27`, `NOT -w28`)

**Verdict vs `<= $2/M` Target (`zero_points`, Small Capital): FAIL (`NO-GO / HOLD`)**

Across all **21 continuous 10h RWA windows (`W01–W27`, `-w28` strictly excluded, 215.17 hours/symbol, 17,388 simulations)** on **Arcus (`USDC`)** across all **18 RWA perpetual markets** (anchored to `lighter_robinhood` and `trade_xyz` lead-lag BBOs), single-venue market-making under `zero_points` (`rebate_bps = 0.0`) achieves **`0 / 21` profitable windows** on every tested arm and latency cadence. The `all_18_rwa` cost floor is **$110.13–$161.76/M double-sided** (**$220.27–$323.51/M single-sided**) under verified fees (`0.00 / 2.1375 bps`), or **$110.79–$166.29/M double-sided** (**$221.58–$332.59/M single-sided**) under public L0 fees (`0.00 / 2.25 bps`) — **55x–160x above** the `<= $2/M` threshold.

Even on the best single cohort (`core_4_rwa`: `AAPL, AMZN, META, MSFT`) or best single ticker (`AMZN-USD`), `S4_hold600s_only` (`o100_c150_e150`, `$200` clip) still loses **$57.17/M double-sided** (**$114.34/M single-sided**) on `core_4_rwa` and **$45.29/M double-sided** (**$90.58/M single-sided**) on `AMZN-USD`. Extending hold to `600s` + faster Arcus cadence cuts IOC taker fees (`$183.85 -> $22.79`), but structural adverse drift (`drft = -$267.69`, negative `1s` markout on **18 / 18** symbols) is **4.4x larger** than spread capture (`spr = +$60.87`), burning **`$102.11/M` double-sided before any exchange fees**.

---

## 1. Provenance, Methodology & Safety Boundaries

- **Evaluator & Tests**: [research_arcus_single_venue_turnover.py](scripts/research_arcus_single_venue_turnover.py), [test_research_arcus_single_venue_turnover.py](scripts/test_research_arcus_single_venue_turnover.py) (`6/6` `python3 -S -m unittest` pass; isolated top-level stdlib imports verified; `--self-test` pass).
- **Simulator Reuse & P0 Realism**: Reuses `simulate_core4_arm` ([research_track03_core4_hold_ablation.py](scripts/research_track03_core4_hold_ablation.py)) and `prepare_replay_stream` ([research_track03_full_universe_speedbump_mm.py](scripts/research_track03_full_universe_speedbump_mm.py)) with:
  - Single-use L1 displayed depth conservation (`cur_bsz` / `cur_asz` decremented on every IOC exit until next BBO refresh).
  - Non-zero latencies everywhere (`order_delay_ms`, `cancel_delay_ms`, `exit_delay_ms > 0`; never `0 ms`), `max_book_age_ms = 5,000`, `declared_end_ms` cutoff + `terminal_flatten_lead_ms = 1,000` explicit residual tracking (`non_flat_count = 24 / 17,388` rows, max `unrealized + realized - cash` error `5.29e-12 USD`, `max_net_err = 0.0`).
- **Strict `-w28` Exclusion**: Every DuckDB Hive query filters `WHERE NOT regexp_matches(campaign_id, '-w28$')` (`w28_excluded = true`; `21` continuous RWA windows `w01..w05, w07, w09, w13..w21, w23..w27`).
- **Fee Schedules (`zero_points`, `rebate_bps = 0.0`)**:
  - **Verified Account Tier**: `0.00 / 2.1375 bps` (`verified_account_tier`, L0 + 5% discount `feeDiscountPpm = 50000`, verified in `.agent-state/research/account_fee_evidence_20261127.json` `sha256: 4d3a9c1aeae5358bb9c55ab448db392ac3edb0a89244f74c23d89817c3359d92` via [track_fee_evidence.py](scripts/track_fee_evidence.py)).
  - **Public L0 Tier**: `0.00 / 2.25 bps` (`public_tier0_schedule_only`).
- **Dual Volume Denominators (`$/M`, positive = cost, negative = profit)**:
  - **Double-sided (`maker + exit`) `$/M`**: `(-mtm_net_usd / (maker_volume_usd + taker_volume_usd)) * 1e6`.
  - **Single-sided Arcus `$/M`**: `(-mtm_net_usd / (0.5 * (maker_volume_usd + taker_volume_usd))) * 1e6 = 2.0 * double_sided_$/M`.
- **Out-of-Git Artifact**: `.agent-state/research/arcus_turnover_20261127/arcus_m1_single_venue_w01_w27.json` (`sha256: ba979867c6bae73ff6a408b5d030b850dfb86b9e92f707b34b6260abb6c69ea6`).

---

## 2. Arm Grid × Core Cadence & `spr / drft / pex / tslp` Decomposition (`all_18_rwa`, `W01–W27`, `$200` Clip)

| Arm | Cadence (`o_c_e` ms) | DS Exec Vol ($) | Vol / 10h ($) | Taker Share `s` | `spr` ($) | `drft` ($) | `pex` ($) | `tslp` ($) | Fee Ver ($) | MTM Net Ver ($) | Verified DS `$/M` | Verified SS `$/M` | Public L0 DS `$/M` | Public L0 SS `$/M` | Pos Wins | Res ($) | Max Port Exp ($) |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `S4_hold15s` | `o50_c50_e100` | 2,099,188 | 99,961 | 0.799 | +64.13 | -170.37 | +5.70 | -22.72 | 179.37 | -302.62 | **+144.16** | +288.32 | +148.66 | +297.31 | 0/21 | 0.0 | 844 |
| `S4_hold15s` | `o100_c150_e150` | 2,145,600 | 102,171 | 0.802 | +66.85 | -194.51 | +5.14 | -23.20 | 183.85 | -329.57 | **+153.60** | +307.20 | +158.11 | +316.22 | 0/21 | 0.0 | 1,305 |
| `S4_hold15s` | `o200_c300_e330` | 2,057,123 | 97,958 | 0.807 | +59.24 | -197.76 | +5.00 | -21.87 | 177.37 | -332.75 | **+161.76** | +323.51 | +166.29 | +332.59 | 0/21 | 0.0 | 1,205 |
| `S4_hold60s` | `o50_c50_e100` | 1,976,465 | 94,117 | 0.534 | +61.38 | -199.54 | +12.01 | -14.85 | 112.72 | -253.73 | **+128.37** | +256.75 | +131.38 | +262.75 | 0/21 | 0.0 | 1,033 |
| `S4_hold60s` | `o100_c150_e150` | 2,044,496 | 97,357 | 0.534 | +64.84 | -228.35 | +11.57 | -15.52 | 116.69 | -284.15 | **+138.98** | +277.97 | +141.99 | +283.97 | 0/21 | 0.0 | 1,305 |
| `S4_hold60s` | `o200_c300_e330` | 2,003,394 | 95,400 | 0.536 | +58.88 | -235.11 | +10.82 | -14.90 | 114.75 | -295.05 | **+147.28** | +294.55 | +150.29 | +300.58 | 0/21 | 0.0 | 1,205 |
| `S4_hold180s` | `o50_c50_e100` | 1,906,558 | 90,788 | 0.277 | +59.50 | -226.89 | +17.48 | -9.51 | 56.38 | -215.80 | **+113.19** | +226.37 | +114.74 | +229.49 | 0/21 | 0.0 | 1,232 |
| `S4_hold180s` | `o100_c150_e150` | 1,939,589 | 92,361 | 0.280 | +62.36 | -248.56 | +16.12 | -9.69 | 58.06 | -237.80 | **+122.61** | +245.21 | +124.18 | +248.36 | 0/21 | 76.7 | 1,305 |
| `S4_hold180s` | `o200_c300_e330` | 1,938,397 | 92,305 | 0.280 | +57.28 | -259.05 | +14.62 | -9.15 | 57.93 | -254.22 | **+131.15** | +262.30 | +132.72 | +265.44 | 0/21 | 84.5 | 1,262 |
| `S4_hold600s_only` | `o50_c50_e100` | 1,846,185 | 87,914 | 0.116 | +57.72 | -253.04 | +20.26 | -5.30 | 22.96 | -203.33 | **+110.13** | +220.27 | +110.79 | +221.58 | 0/21 | 0.0 | 1,523 |
| `S4_hold600s_only` | `o100_c150_e150` | 1,889,713 | 89,986 | 0.113 | +60.87 | -267.69 | +19.14 | -5.28 | 22.79 | -215.72 | **+114.15** | +228.31 | +114.79 | +229.58 | 0/21 | 76.7 | 1,523 |
| `S4_hold600s_only` | `o200_c300_e330` | 1,885,919 | 89,806 | 0.112 | +56.11 | -278.11 | +17.47 | -5.06 | 22.60 | -232.17 | **+123.10** | +246.21 | +123.74 | +247.47 | 0/21 | 84.5 | 1,411 |
| `W5` | `o50_c50_e100` | 931,737 | 44,368 | 0.218 | +36.46 | -127.40 | +7.74 | -3.27 | 21.76 | -108.22 | **+116.15** | +232.29 | +117.37 | +234.75 | 0/21 | 0.0 | 800 |
| `W5` | `o100_c150_e150` | 933,715 | 44,463 | 0.226 | +38.55 | -141.66 | +6.74 | -3.16 | 22.50 | -121.98 | **+130.64** | +261.28 | +131.91 | +263.81 | 0/21 | 176.7 | 803 |
| `W5` | `o200_c300_e330` | 913,956 | 43,522 | 0.252 | +35.32 | -150.19 | +5.41 | -3.39 | 24.62 | -137.48 | **+150.42** | +300.84 | +151.84 | +303.68 | 0/21 | 0.0 | 1,003 |

### Marginal Cadence Sweep on `S4_hold600s_only` (`all_18_rwa`, `$200` Clip)

| Sweep Dimension | Cadence (`o_c_e` ms) | DS Exec Vol ($) | Taker `s` | MTM Net Ver ($) | Verified DS `$/M` | Verified SS `$/M` | Public L0 DS `$/M` |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Submit RTT (`50 / 100 / 200`) | `o50_c150_e150` | 1,919,301 | 0.112 | -220.91 | +115.10 | +230.19 | +115.73 |
| Submit RTT (`50 / 100 / 200`) | `o100_c150_e150` | 1,889,713 | 0.113 | -215.72 | +114.15 | +228.31 | +114.79 |
| Submit RTT (`50 / 100 / 200`) | `o200_c150_e150` | 1,811,302 | 0.119 | -207.21 | +114.40 | +228.80 | +115.07 |
| Cancel Latency (`50 / 150 / 300`) | `o100_c50_e150` | 1,798,395 | 0.118 | -196.92 | **+109.50** | +218.99 | +110.16 |
| Cancel Latency (`50 / 150 / 300`) | `o100_c150_e150` | 1,889,713 | 0.113 | -215.72 | +114.15 | +228.31 | +114.79 |
| Cancel Latency (`50 / 150 / 300`) | `o100_c300_e150` | 1,970,221 | 0.108 | -238.16 | +120.88 | +241.76 | +121.49 |
| IOC Exit Delay (`100 / 150 / 330`) | `o100_c150_e100` | 1,889,729 | 0.113 | -215.65 | +114.11 | +228.23 | +114.75 |
| IOC Exit Delay (`100 / 150 / 330`) | `o100_c150_e150` | 1,889,713 | 0.113 | -215.72 | +114.15 | +228.31 | +114.79 |
| IOC Exit Delay (`100 / 150 / 330`) | `o100_c150_e330` | 1,889,705 | 0.113 | -215.57 | +114.07 | +228.15 | +114.71 |

**Key takeaway**: Only `cancel_ms` moves unit cost (`300 -> 50 ms` saves `$11.38/M` double-sided by reducing cancel-in-flight adverse fills), while `submit_rtt_ms` and `ioc_exit_delay_ms` leave unit cost nearly unchanged (`~$114/M`). Even at `o100_c50_e150`, cost is **`+$109.50/M` double-sided** (`+$218.99/M` single-sided).

---

## 3. Splits, Cohorts, Bootstrap CIs, Symmetry & Market-Neutral Beta (`o100_c150_e150`, `$200` Clip)

### Temporal Splits (`selection_dev w01–w20` / `holdout w21–w25` / `w26–w27 OOS`)

| Cohort | Arm | Dev DS `$/M` (`15w`) | Holdout DS `$/M` (`4w`) | W26–W27 OOS DS `$/M` (`2w`) | Full W01–W27 DS `$/M` (`21w`) | Full W01–W27 SS `$/M` | Full DS Vol ($) | Full MTM Net Ver ($) |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `all_18_rwa` | `S4_hold15s` | +147.48 | +195.07 | +165.80 | +153.60 | +307.20 | 2,145,600 | -329.57 |
| `all_18_rwa` | `S4_hold600s_only` | +108.37 | +159.96 | +119.75 | +114.15 | +228.31 | 1,889,713 | -215.72 |
| `all_18_rwa` | `W5` | +126.57 | +166.67 | +123.96 | +130.64 | +261.28 | 933,715 | -121.98 |
| `ex_nvda_17_rwa` | `S4_hold600s_only` | +102.06 | +158.28 | +114.75 | +108.32 | +216.64 | 1,809,613 | -196.02 |
| `core_4_rwa` (`AAPL,AMZN,META,MSFT`) | `S4_hold600s_only` | +50.87 | +113.95 | +99.45 | **+57.17** | **+114.34** | 914,176 | -52.26 |
| `top5_dev_selected_rwa` (`AMZN,MU,META,SPY,AMD`) | `S4_hold600s_only` | +62.81 | +189.59 | +109.94 | +73.49 | +146.98 | 1,210,500 | -88.96 |

### Statistical Checks (`all_18_rwa`, `W01–W27`)

- **Politis-Romano Stationary Block Bootstrap (`L = 3.0`, `10,000` draws, `seed = 20261127`)**:
  - `S4_hold600s_only` (`o100_c150_e150`): Verified DS cost `+$114.15/M` (95% CI `[+$97.84, +$141.52]/M`); SS cost `+$228.31/M` (95% CI `[+$195.69, +$283.04]/M`); mean net PnL `-$10.27/win` (95% CI `[-$11.99, -$8.56]`).
  - Paired improvement `S4_hold600s_only` vs `S4_hold15s`: `+$5.42/win` (95% CI `[+$3.56, +$7.21]`), `-39.45 $/M` double-sided (95% CI `[-50.10, -28.54] $/M`, `p_non_improvement = 0.0`). Hold extension helps significantly, yet never approaches profitability or `$2/M`.
- **Multiple Testing (`21` aligned windows, `21` baseline `$200` arms)**: `effective_independent_trials = 1.60`, `best_trial` SR = `-2.788`, **`dsr_raw = 0.0`**, **`dsr_effective = 0.0`**, `cscv_pbo = 0.3286`.
- **UP/DOWN Market Symmetry (`S4_hold600s_only @ o100_c150_e150`)**:
  - `11` UP windows (mean EW RWA return `+162.62 bps`): MTM net `-$121.66` (`+$113.84/M` DS, `0/11` positive).
  - `10` DOWN windows (mean EW RWA return `-154.68 bps`): MTM net `-$94.06` (`+$114.56/M` DS, `0/10` positive).
  - Two-sided Welch `t = -1.2221` (`p = 0.2383`); pooled 378 symbol-windows `t = -1.2764` (`p = 0.2026`). Losses are symmetric across market directions.
- **OLS Beta vs RWA Return (`S4_hold600s_only @ o100_c150_e150`)**:
  - Portfolio (`n = 21`): `alpha = -$10.25/win`, `beta = -0.2134 USD/%` (`t = -0.6046`, `p = 0.5526`, `R^2 = 0.0189`).
  - Pooled symbol-windows (`n = 378`): `alpha = -$0.569`, `beta = -0.0126 USD/%` (`t = -0.6443`, `p = 0.5198`, `R^2 = 0.0011`).

---

## 4. Per-Symbol Breakdown (`S4_hold600s_only @ o100_c150_e150`, `$200` Clip, `W01–W27`)

All 18 Arcus RWA markets are continuous (`21/21` windows, `215.17h` each) and **all 18 have negative `1s` markout (`-0.01` to `-3.53 bps`) and net losses**:

| Symbol | Med Spread (bps) | Fills | DS Vol ($) | Vol / 10h ($) | Taker `s` | `spr` ($) | `drft` ($) | `pex` ($) | Fee Ver ($) | Net Ver ($) | Verified DS `$/M` | Verified SS `$/M` | Public L0 DS `$/M` | `1s` Markout (bps) |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `AMZN` | 0.39 | 6,567 | 708,479 | 33,737 | 0.018 | +19.70 | -61.63 | +11.38 | 1.40 | -32.09 | **+45.29** | +90.58 | +45.39 | -0.0057 |
| `META` | 0.17 | 655 | 45,160 | 2,150 | 0.325 | +1.05 | -3.35 | +0.10 | 1.57 | -3.83 | **+84.78** | +169.55 | +86.60 | -1.1155 |
| `MSFT` | 0.20 | 733 | 61,056 | 2,907 | 0.291 | +1.79 | -5.45 | +0.20 | 1.90 | -5.52 | **+90.42** | +180.83 | +92.05 | -1.0150 |
| `MU` | 0.17 | 234 | 11,269 | 537 | 0.207 | +0.28 | -1.19 | +0.01 | 0.25 | -1.16 | **+103.23** | +206.46 | +104.40 | -1.3245 |
| `AAPL` | 0.28 | 1,450 | 99,481 | 4,737 | 0.094 | +3.37 | -13.75 | +0.62 | 1.00 | -10.83 | **+108.83** | +217.67 | +109.36 | -0.7940 |
| `SPY` | 0.14 | 4,633 | 426,714 | 20,320 | 0.050 | +3.13 | -50.94 | +1.37 | 2.28 | -48.80 | **+114.37** | +228.74 | +114.65 | -0.4834 |
| `GOOGL` | 0.36 | 1,035 | 77,288 | 3,680 | 0.139 | +3.51 | -12.56 | +0.55 | 1.15 | -9.74 | **+126.05** | +252.10 | +126.83 | -1.2241 |
| `QQQ` | 0.16 | 1,506 | 111,526 | 5,311 | 0.054 | +1.84 | -15.92 | +0.23 | 0.64 | -14.51 | **+130.08** | +260.16 | +130.38 | -0.8217 |
| `AMD` | 0.23 | 357 | 18,878 | 899 | 0.115 | +0.44 | -3.28 | -0.00 | 0.23 | -3.08 | **+163.25** | +326.50 | +163.90 | -1.6630 |
| `TSLA` | 0.26 | 1,191 | 45,265 | 2,155 | 0.202 | +1.50 | -8.08 | +0.19 | 0.98 | -7.44 | **+164.41** | +328.81 | +165.54 | -0.9608 |
| `SNDK` | 1.20 | 461 | 20,549 | 978 | 0.210 | +1.61 | -4.73 | +0.31 | 0.46 | -3.41 | **+165.86** | +331.72 | +167.04 | -2.5220 |
| `PLTR` | 1.21 | 440 | 24,701 | 1,176 | 0.509 | +1.71 | -5.11 | +0.17 | 1.34 | -4.96 | **+200.84** | +401.67 | +203.70 | -2.6012 |
| `HOOD` | 1.49 | 654 | 46,802 | 2,229 | 0.412 | +4.48 | -12.53 | +1.01 | 2.06 | -9.81 | **+209.70** | +419.40 | +212.01 | -2.7356 |
| `NVDA` | 0.66 | 2,162 | 80,100 | 3,814 | 0.042 | +5.43 | -25.78 | +1.06 | 0.36 | -19.70 | **+245.95** | +491.90 | +246.18 | -1.6632 |
| `DRAM` | 2.83 | 259 | 32,014 | 1,524 | 0.803 | +3.01 | -8.23 | +0.48 | 2.75 | -9.29 | **+290.07** | +580.15 | +294.59 | -1.0405 |
| `COIN` | 0.94 | 410 | 22,067 | 1,051 | 0.616 | +2.07 | -8.22 | +0.26 | 1.45 | -7.69 | **+348.50** | +697.01 | +351.96 | -2.1704 |
| `CRCL` | 2.00 | 313 | 22,486 | 1,071 | 0.445 | +2.44 | -9.94 | +0.75 | 1.07 | -8.30 | **+369.07** | +738.14 | +371.57 | -3.5280 |
| `MSTR` | 1.23 | 445 | 35,879 | 1,708 | 0.499 | +3.52 | -16.99 | +0.45 | 1.91 | -15.56 | **+433.59** | +867.18 | +436.40 | -2.8908 |

---

## 5. Capacity & Clip Size Sensitivity (`$100 / $200 / $500`, `o100_c150_e150`, `all_18_rwa`)

| Arm | Clip (`max_pos`) | DS Vol ($) | Vol / 10h ($) | Annualized / 24h ($) | Taker `s` | Clipped Exits | MTM Net Ver ($) | Verified DS `$/M` | Verified SS `$/M` | Max Port Exp ($) |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `S4_hold15s` | `$100` (`$200`) | 1,193,385 | 56,828 | 136,387 | 0.792 | 2,478 | -184.94 | +154.97 | +309.95 | 699.9 |
| `S4_hold15s` | `$200` (`$400`) | 2,145,600 | 102,171 | 245,211 | 0.802 | 4,044 | -329.57 | +153.60 | +307.20 | 1,305.3 |
| `S4_hold15s` | `$500` (`$1000`) | 4,358,889 | 207,566 | 498,159 | 0.827 | 6,530 | -669.99 | +153.71 | +307.41 | 3,105.7 |
| `S4_hold600s_only` | `$100` (`$200`) | 1,069,557 | 50,931 | 122,235 | 0.110 | 126 | -126.16 | +117.96 | +235.91 | 867.4 |
| `S4_hold600s_only` | `$200` (`$400`) | 1,889,713 | 89,986 | 215,967 | 0.113 | 250 | -215.72 | +114.15 | +228.31 | 1,523.1 |
| `S4_hold600s_only` | `$500` (`$1000`) | 3,848,805 | 183,276 | 439,863 | 0.118 | 487 | -439.79 | +114.27 | +228.53 | 3,464.3 |
| `W5` | `$100` (`$200`) | 525,498 | 25,024 | 60,057 | 0.221 | 214 | -70.46 | +134.09 | +268.18 | 427.1 |
| `W5` | `$200` (`$400`) | 933,715 | 44,463 | 106,710 | 0.226 | 401 | -121.98 | +130.64 | +261.28 | 802.7 |
| `W5` | `$500` (`$1000`) | 1,922,052 | 91,526 | 219,663 | 0.236 | 704 | -249.45 | +129.78 | +259.57 | 1,892.7 |

Capacity scales near-linearly from `$50.9k/10h` (`$100`) to `$183.3k/10h` (`$500`, `~$440k/24h` DS) on `S4_hold600s_only` with flat unit cost (`$114–$118/M` DS) and `$3,464` peak simultaneous exposure (requiring `>= $1,000–$3,500` USDC margin depending on leverage), but every `$1M` double-sided costs `~$114` (`~$228` per `$1M` single-sided).

---

## 6. Mandatory Unprinted Toxic-Fill Stress (`o100_c150_e150`, `$200` Clip, `all_18_rwa`)

| Arm | Stress Scenario | DS Vol ($) | Unprinted Fills (`$`) | `spr` ($) | `drft` ($) | `pex` ($) | Fee Ver ($) | MTM Net Ver ($) | Verified DS `$/M` | Verified SS `$/M` | Public L0 DS `$/M` |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `S4_hold15s` | `baseline_prints_only` | 2,145,600 | 0 (`0`) | +66.85 | -194.51 | +5.14 | 183.85 | **-329.57** | +153.60 | +307.20 | +158.11 |
| `S4_hold15s` | `strict_through_25` | 2,683,808 | 20,107 (`539,459`) | +8.79 | -172.14 | -6.78 | 211.01 | **-405.41** | +151.06 | +302.11 | +155.19 |
| `S4_hold15s` | `strict_through_50` | 3,511,225 | 19,766 (`1,162,641`) | -39.49 | -182.40 | -27.62 | 251.75 | **-530.86** | +151.19 | +302.38 | +154.96 |
| `S4_hold15s` | `strict_through_100` | 5,130,141 | 15,863 (`2,542,011`) | -135.25 | -211.63 | -86.17 | 303.00 | **-772.77** | +150.63 | +301.27 | +153.74 |
| `S4_hold15s` | `opposite_touch_25` | 5,391,416 | 89,610 (`2,214,149`) | -61.23 | -221.57 | -33.55 | 396.47 | **-774.81** | +143.71 | +287.42 | +147.58 |
| `S4_hold15s` | `opposite_touch_50` | 8,540,719 | 85,815 (`4,822,089`) | -181.77 | -263.75 | -108.00 | 507.09 | **-1,142.96** | +133.82 | +267.65 | +136.95 |
| `S4_hold60s` | `strict_through_50` | 3,330,845 | 29,732 (`1,405,505`) | — | — | — | — | **-448.94** | +134.78 | +269.57 | +136.73 |
| `S4_hold60s` | `opposite_touch_25` | 5,029,092 | 139,374 (`2,571,175`) | — | — | — | — | **-617.43** | +122.77 | +245.54 | +124.81 |
| `S4_hold180s` | `strict_through_50` | 3,236,278 | 35,652 (`1,534,473`) | — | — | — | — | **-397.17** | +122.72 | +245.45 | +123.46 |
| `S4_hold180s` | `opposite_touch_25` | 4,765,554 | 162,083 (`2,739,371`) | — | — | — | — | **-510.02** | +107.02 | +214.04 | +107.78 |
| `S4_hold600s_only` | `baseline_prints_only` | 1,889,713 | 0 (`0`) | +60.87 | -267.69 | +19.14 | 22.79 | **-215.72** | +114.15 | +228.31 | +114.79 |
| `S4_hold600s_only` | `strict_through_25` | 2,390,971 | 45,115 (`762,585`) | +8.40 | -242.78 | -26.45 | 14.91 | **-278.48** | +116.47 | +232.94 | +116.80 |
| `S4_hold600s_only` | `strict_through_50` | 3,192,076 | 37,659 (`1,577,400`) | -35.44 | -252.91 | -71.69 | 13.93 | **-376.60** | +117.98 | +235.96 | +118.21 |
| `S4_hold600s_only` | `strict_through_100` | 4,738,127 | 20,246 (`3,217,204`) | -125.01 | -279.54 | -170.69 | 7.06 | **-584.01** | +123.26 | +246.52 | +123.34 |
| `S4_hold600s_only` | `opposite_touch_25` | 4,589,727 | 167,756 (`2,779,035`) | -48.88 | -309.39 | -87.43 | 14.98 | **-463.37** | +100.96 | +201.92 | +101.13 |
| `S4_hold600s_only` | `opposite_touch_50` | 7,589,627 | 134,575 (`5,803,173`) | -161.30 | -351.36 | -205.31 | 13.87 | **-734.35** | +96.76 | +193.52 | +96.85 |
| `W5` | `baseline_prints_only` | 933,715 | 0 (`0`) | +38.55 | -141.66 | +6.74 | 22.50 | **-121.98** | +130.64 | +261.28 | +131.91 |
| `W5` | `strict_through_25` | 1,033,102 | 11,427 (`222,145`) | +17.51 | -128.02 | -6.83 | 16.31 | **-135.70** | +131.35 | +262.70 | +132.18 |
| `W5` | `strict_through_50` | 1,226,434 | 9,677 (`441,757`) | +4.05 | -133.18 | -19.77 | 15.45 | **-166.40** | +135.68 | +271.35 | +136.34 |
| `W5` | `strict_through_100` | 1,676,727 | 5,611 (`925,375`) | -25.05 | -140.09 | -50.81 | 12.95 | **-230.69** | +137.58 | +275.17 | +137.99 |
| `W5` | `opposite_touch_25` | 1,358,340 | 29,624 (`509,091`) | +12.61 | -144.21 | -12.67 | 18.41 | **-165.10** | +121.54 | +243.09 | +122.26 |
| `W5` | `opposite_touch_50` | 1,842,607 | 23,651 (`1,022,477`) | -8.01 | -153.39 | -32.52 | 17.63 | **-213.78** | +116.02 | +232.05 | +116.53 |

Unprinted toxic book-crosses flip `spr` from `+$60.87` to `-$125.01` (`strict_through 100%`) and `-$161.30` (`opposite_touch 50%`) on `S4_hold600s_only` and flip `pex` to `-$170.69` / `-$205.31`, inflating dollar losses by **2.7x–3.4x** (`-$215.72 -> -$584.01 / -$734.35`).

---

## 7. Honest Verdict & Shadow Profile Guidance

1. **Verdict (`zero_points`, Small Capital)**: **FAIL (`NO-GO / HOLD` for live capital)** — Method 1 cannot reach `<= $2/M` on Arcus RWA (`$110–$162/M` double-sided / `$220–$324/M` single-sided across all 18 markets; `$45.29/M` DS / `$90.58/M` SS even on best single symbol `AMZN`). Zero-fee maker (`0.00 bps`) does not offset adverse selection on `0.14–0.41 bps` Arcus L1 spreads.
2. **Shadow Profile Ticker Allowlist (for `@live-infra` `examples/track03_arcus_rwa_shadow.toml` only, `HOLD` for live orders)**: If running read-only shadow telemetry on Arcus RWA, restrict to the lowest-cost 5-ticker cohort (`AMZN-USD, META-USD, MSFT-USD, AAPL-USD, SPY-USD`, or `core_4_rwa` ex-`NVDA`) using `S4_hold600s_only` (`o100_c150_e150`, `ioc_exit_delay_ms = 150`, `0.00 / 2.1375 bps`), strictly keeping `dry_run = true` / shadow mode only.
