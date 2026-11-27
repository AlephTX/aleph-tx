# Track 01 (`01_quant_profit`) Microstructure & RWA Alpha Upgrade (`W01–W26`)

**Date:** `2026-11-27` (CST) · **Operational Mode:** Strictly **`HOLD`** (zero live orders/cancels, zero Tokyo deploys/restarts, read-only `nice -n 10` on `aleph` lakehouse)

This report presents the Stage A root-cause microstructure diagnosis, the first-ever **RWA Cross-Venue Lead-Lag** discovery (`NVDA, AAPL, SPY, QQQ, TSLA, AMZN, MU`), the default-off Rust strategy upgrades in [lead_lag_taker.rs](../src/strategy/lead_lag_taker.rs), and the Round-2 frozen `selection_dev (w01–w20)` -> untouched `holdout (w21–w25)` + `w26_oos` evaluation across `taker_delay_ms in {230, 300, 400, 500}` under Lighter Standard (`0/0 bps`).

> [!IMPORTANT]
> **Fee-tier provenance (`OPT-01`):** `lighter_robinhood` (`USDG`) is live-verified Standard `0.00 / 0.00 bps` (`standard_zero_confirmed=true`). `lighter` (`USDC`) is live-audited Premium `0.40 / 2.80 bps` ([VERIFIED_FEE_W01_W26_THREE_TRACK_REPORT_2026_11_27.md](VERIFIED_FEE_W01_W26_THREE_TRACK_REPORT_2026_11_27.md)), and is modeled in this study explicitly as **`lighter_standard_zero_scenario_0_0` (`0.00 / 0.00 bps`)** to isolate post-switch Standard sensitivity across `{230, 300, 400, 500} ms`. All capacity figures use `min(clip_usd, 0.5 * entry_l1_usd)` for `$500` and `$1000` clips, alongside strict `public_trades` print verification (`1000 ms` window).

---

## 1. Executive Summary

1. **Stage A Root-Cause Diagnosis (`selection_dev w01–w20`, `230–500 ms`):**
   - **Leader-reversal snapbacks** are the single largest drag on baseline Track 01: at `theta = 6.0 bps, hold = 10s, 230 ms`, **`60.0%` (`1,798 / 3,000`)** of Crypto `lighter` trades and **`71.6%` (`2,184 / 3,052`)** of Crypto `lighter_robinhood` trades experience a `<= -5 bp` leader snapback during the hold window, losing **`-1.8203 bp` (`-4.1461 bp` PV)** and **`-2.6468 bp` (`-3.3937 bp` PV)** respectively, whereas non-snapback trades (`> -2 bp`) earn **`+13.9695 bp` (`+8.4628 bp` PV)** and **`+13.1530 bp` (`+14.7583 bp` PV)**. In RWA (`lighter_robinhood <- arcus`), **`85.4%` (`4,294 / 5,030`)** snap back `<= -5 bp` (`-5.0173 bp` quote / `-7.4443 bp` PV) vs **`+13.7385 bp` (`+14.3849 bp` PV)** without snapback.
   - **Opposed Reference OBI** destroys edge: on `lighter_robinhood` Crypto (`theta=6`), trades with aligned strong reference OBI (`>= +0.25`) earn **`+2.5625 bp` quote / `+3.1582 bp` PV**, while opposed OBI (`<= -0.25`) loses **`-0.1830 bp` quote** (`+0.7531 bp` PV).
   - **Wide Target Spreads & Thin Edge-over-Half-Spread** bleed consistently: on `lighter_robinhood` Crypto (`theta=6`), `[0, 0.5) bp` spreads earn **`+3.5567 bp` quote / `+4.4043 bp` PV**, while `[1.0, 2.0)`, `[2.0, 4.0)`, and `>= 4.0 bp` spreads lose **`-0.4086 bp`, `-0.7717 bp`, and `-1.9708 bp` quote** (`-2.0196 / -1.1206 / -1.2531 bp` PV).
   - **Low-threshold `[4, 6) bp` noise** loses `-$119.32` PV (`-0.4361 bp`) on `lighter` and `-$236.78` PV (`-1.0909 bp`) on `lighter_robinhood` (`theta=4.0`).
   - **Latency Sensitivity (`230 -> 500 ms`):** On baseline `theta=6.0`, `lighter_robinhood` Crypto quote PnL decays monotonically from `+1.081 bp` (`-$218.19` `$1k`) at `230 ms` to **`-0.120 / -0.238 / -0.685 bp`** (`-$305.16 / -$325.82 / -$368.70`) at `300 / 400 / 500 ms`, while `lighter` decays from `+3.504 bp` (`+$515.87` `$1k`) to `+1.002 bp` (`+$173.60`).
2. **Stage B Round-2 Upgrades (`M1–M4`) — Untouched `holdout (w21–w25)` & `w26_oos`:**
   - **Crypto `lighter_robinhood` (`0/0` USDG):** `M3_ref_reversal_spread_gate` and `M4_combined` slash `selection_dev` (`230 ms`) quote loss from **`-$190.89`** (`A_baseline`) to **`-$31.10`** (**-83.7%**) and **`-$1.62`** (**-99.2%**, with **`+$11.41` PV**), and **flip untouched `holdout (w21–w25)` quote PnL positive**: **`+$21.63` (`M3`, `+2.313 bp`, `+$0.87` PV)** and **`+$17.74` (`M4`, `+2.639 bp`, `60%` positive windows)** vs `-$15.41` (`A_baseline`). `M4` holds positive quote PnL out to `400 ms` (`+$7.74` at `300 ms`, `+$0.62` at `400 ms`).
   - **Crypto `lighter` (`0/0` Standard scenario):** `M1_microprice_obi` boosts `holdout` PV by **+53.6%** at `230 ms` (**`+$32.90` PV / `+$58.71` quote**, `100%` positive quote windows) vs `A_baseline` (`+$21.42` PV / `+$52.66` quote), and leads PV across `300–400 ms` (`+$46.85` / `+$41.76` PV). `M2_consensus_leader` achieves **`100%` positive PV windows** (`+$40.65` / `+$37.79` PV, `$0.00` PV DD) in holdout `300–400 ms` and flips `w26_oos` quote PnL positive on the selected-positive cohort (`+$4.81` at `230 ms`, `+$3.76` at `300 ms`).
   - **RWA Cross-Venue (`NVDA, AAPL, SPY, QQQ, TSLA, AMZN, MU`):** Ungated `A_baseline` loses **`-$1,353.84` quote / `-$297.33` PV** (`lighter_robinhood`) and **`-$757.45` quote / `-$293.03` PV** (`lighter`) on `selection_dev`, and **`-$411.71` / `-$143.96` quote** on `holdout`. `M4_combined` flips `selection_dev` (`230 ms`) positive to **`+$80.65` quote / `+$17.28` PV** (`lighter_robinhood`) and **`+$68.91` quote / `+$22.22` PV** (`lighter`), and flips `holdout` `lighter_robinhood` quote positive (**`+$8.09`**, `75%` pos windows, `-$0.03` PV) while cutting `lighter` holdout quote loss by **-84.9%** (`-$21.69`) and flipping `w26_oos` `lighter` positive (`+$0.87` quote / `+$0.04` PV).
3. **Go / No-Go Verdict:** Keep **`HOLD`** for live deployment. Although microstructure gating (`M1` + `M3` + `M4`) eliminates ~85%–99% of baseline quote bleed and flips `lighter_robinhood` Crypto and RWA `holdout` quote PnL positive at `230 ms`, `w26_oos` print-verified depth remains thin (`4–15` prints/10h) and RWA/Crypto alpha decays sharply beyond `230–300 ms` latency.

---

## 2. Dataset, Splits, and Protocol (`W01–W26`)

Evaluated strictly read-only on `aleph` Hive parquet (`bbo_quotes` bucketed at `250 ms` + unbucketed `public_trades` for `1000 ms` print verification; `cont=True` requiring `>= 10h` duration and `<= 60s` max gap; `15 min` warm-up):

- **Crypto (`BTC, ETH, SOL`, `25` windows):**
  - `selection_dev` (`19` windows, `w01..w20` ex `w08`): `12` target-leader pairs (`lighter` & `lighter_robinhood` <- `binance` & `coinbase`, with `extended` as 3rd consensus leader).
  - `holdout` (`5` windows, `w21..w25`): replayed **once** with frozen parameters from `selection_dev`.
  - `w26_oos` (`1` window, `w26`): prospective untouched OOS window.
- **RWA (`NVDA, AAPL, SPY, QQQ, TSLA, AMZN, MU`, `20` windows):**
  - `selection_dev` (`15` windows, `w01..w05, w07, w09, w13..w20`): `28` target-leader pairs (`lighter` & `lighter_robinhood` <- `arcus`, plus cross-domain `lighter <-> lighter_robinhood`).
  - `holdout` (`4` windows, `w21, w23, w24, w25`) and `w26_oos` (`1` window, `w26`).

---

## 3. Stage A: Root-Cause Diagnosis & RWA Discovery (`selection_dev w01–w20`)

### 3.1 Baseline Latency Decay (`theta = 6.0 bps, tau = 120s, hold = 10s`)

| Domain | Cohort / Target (`0/0` Standard) | Delay (`ms`) | Trades (`PV`) | Quote Mean (bp) | PV Mean (bp) | Quote `$500` / `$1000` ($) | PV `$500` / `$1000` ($) |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Crypto | `lighter` (`USDC` scenario) | `230` | `3,000` (`2,066`) | `+3.504` | `+0.5791` | `+308.38 / +515.87` | `+28.88 / +38.85` |
| Crypto | `lighter` (`USDC` scenario) | `300` | `2,983` (`2,063`) | `+1.963` | `+0.4764` | `+178.83 / +300.46` | `+38.65 / +65.87` |
| Crypto | `lighter` (`USDC` scenario) | `400` | `2,983` (`2,059`) | `+1.703` | `+0.2954` | `+166.19 / +286.39` | `+36.84 / +68.77` |
| Crypto | `lighter` (`USDC` scenario) | `500` | `2,983` (`2,020`) | `+1.002` | `+0.2597` | `+96.25 / +173.60` | `+21.76 / +47.55` |
| Crypto | `lighter_robinhood` (`USDG`) | `230` | `3,052` (`810`) | `+1.081` | `+1.8551` | `-95.08 / -218.19` | `-4.33 / -13.46` |
| Crypto | `lighter_robinhood` (`USDG`) | `300` | `3,032` (`804`) | `-0.120` | `+1.3664` | `-148.71 / -305.16` | `-3.88 / -6.70` |
| Crypto | `lighter_robinhood` (`USDG`) | `400` | `3,032` (`775`) | `-0.238` | `+1.0860` | `-160.85 / -325.82` | `-13.80 / -25.55` |
| Crypto | `lighter_robinhood` (`USDG`) | `500` | `3,032` (`744`) | `-0.685` | `+0.8669` | `-189.08 / -368.70` | `-27.39 / -55.42` |
| RWA | `lighter <- arcus` | `230` | `5,314` (`1,093`) | `-1.608` | `-3.0354` | `-348.22 / -546.39` | `-120.57 / -197.32` |
| RWA | `lighter <- arcus` | `300` | `5,180` (`1,034`) | `-2.102` | `-4.5667` | `-459.43 / -736.82` | `-206.54 / -358.36` |
| RWA | `lighter <- lighter_robinhood` | `230` | `2,538` (`758`) | `-1.437` | `-1.7385` | `-201.09 / -343.41` | `-87.37 / -136.46` |
| RWA | `lighter_robinhood <- arcus` | `230` | `5,030` (`819`) | `-2.848` | `-2.7894` | `-602.79 / -995.28` | `-94.05 / -163.78` |
| RWA | `lighter_robinhood <- arcus` | `300` | `4,917` (`781`) | `-3.020` | `-3.3366` | `-624.19 / -1,034.51` | `-130.90 / -237.17` |
| RWA | `lighter_robinhood <- lighter` | `230` | `2,538` (`595`) | `-1.450` | `-3.4024` | `-250.55 / -427.18` | `-75.73 / -140.19` |

### 3.2 Microstructure Attribution Drivers (`230 ms`, `theta = 6.0 bps`)

| Driver / Bucket | Crypto `lighter` (Trades, Quote Mean, PV Mean) | Crypto `lighter_rh` (Trades, Quote Mean, PV Mean) | RWA `rh <- arcus` (Trades, Quote Mean, PV Mean) |
| --- | --- | --- | --- |
| Entry `|dev|` `[6, 8)` bp | `1,998`, `+2.0576`, `-0.0035` | `2,119`, `+0.0987`, `+0.3079` | `3,659`, `-2.7876`, `-2.5693` |
| Entry `|dev|` `[8, 12)` bp | `749`, `+4.8015`, `+0.8046` | `740`, `+2.4294`, `+3.4790` | `1,135`, `-3.4760`, `-2.6127` |
| Entry `|dev|` `[12, 20)` bp | `223`, `+9.2999`, `+2.2851` | `174`, `+5.0961`, `+4.6752` | `198`, `-1.9092`, `-3.4095` |
| Ref OBI Aligned (`>= +0.25`) | `1,417`, **`+4.5030`**, **`+1.1519`** | `1,295`, **`+2.5625`**, **`+3.1582`** | `1,743`, `-2.7374`, `-5.1985` |
| Ref OBI Opposed (`<= -0.25`) | `1,072`, `+2.4045`, `-0.0506` | `1,236`, **`-0.1830`**, `+0.7531` | `2,383`, `-2.8753`, `-1.1979` |
| Target Spread `[0, 0.5)` bp | `1,554`, `+4.1195`, `+0.8731` | `1,149`, **`+3.5567`**, **`+4.4043`** | `214`, `-0.4261`, `-0.9882` |
| Target Spread `[1.0, 2.0)` bp | `474`, `+3.8973`, `+1.6554` | `630`, `-0.4086`, `-2.0196` | `625`, `-0.9597`, `-0.4568` |
| Target Spread `>= 4.0` bp | `190`, `+2.4154`, `-0.1000` | `165`, **`-1.9708`**, **`-1.2531`** | `1,771`, **`-4.8162`**, **`-5.3631`** |
| Snapback `<= -5 bp` | `1,798`, **`-1.8203`**, **`-4.1461`** | `2,184`, **`-2.6468`**, **`-3.3937`** | `4,294`, **`-5.0173`**, **`-7.4443`** |
| No Snapback (`> -2 bp`) | `749`, **`+13.9695`**, **`+8.4628`** | `515`, **`+13.1530`**, **`+14.7583`** | `383`, **`+13.7385`**, **`+14.3849`** |
| `1s / 3s / 10s` Markout (bp) | `+3.1229 / +3.6655 / +3.8394` | `+1.2104 / +1.3541 / +1.9595` | `-1.4037 / -1.1006 / -1.1071` |
| `10s` MFE / MAE (bp) | `+10.8170 / -6.4020` | `+8.1623 / -7.4901` | `+5.0591 / -11.2095` |

### 3.3 Top RWA Cross-Venue Discovery Pairs (`selection_dev w01–w20`, `230 ms`)

| RWA Pair (`0/0` Standard) | Best Baseline (`theta, hold, tau`) | Trades (`PV`) | Quote Mean (bp) | PV Mean (bp) | Quote `$1k` ($) | PV `$1k` ($) |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| `lighter MU <- lighter_robinhood` | `8.0 bp, 10s, 120s` | `384` (`66`) | `+1.372` | `+5.4189` | **`+60.50`** | **`+33.43`** |
| `lighter MU <- arcus` | `4.0 bp, 10s, 120s` | `5,869` (`152`) | `-1.646` | `+2.3999` | `-612.75` | `+29.18` |
| `lighter QQQ <- lighter_robinhood` | `4.0 bp, 10s, 30s` | `194` (`62`) | `+1.519` | `+3.0173` | **`+8.19`** | **`+13.56`** |
| `lighter_robinhood MU <- arcus` | `8.0 bp, 10s, 120s` | `1,040` (`28`) | `-4.440` | `+3.4086` | `-290.90` | `+9.41` |
| `lighter_robinhood QQQ <- arcus` | `8.0 bp, 10s, 120s` | `44` (`30`) | `+1.028` | `+1.3548` | **`+10.01`** | **`+8.46`** |
| `lighter SPY <- arcus` | `4.0 bp, 10s, 30s` | `122` (`43`) | `+0.239` | `+2.2220` | `-3.03` | `+5.75` |
| `lighter TSLA <- arcus` | `6.0 bp, 10s, 30s` | `363` (`54`) | `+0.117` | `+2.4979` | **`+2.57`** | **`+4.98`** |
| `lighter_robinhood AAPL <- lighter` | `6.0 bp, 10s, 30s` | `70` (`13`) | `+1.466` | `+6.1374` | **`+9.06`** | **`+4.57`** |

---

## 4. Rust Strategy Upgrades (`src/strategy/lead_lag_taker.rs`)

Implemented strictly behind default-off `serde(default)` knobs in [lead_lag_taker.rs](../src/strategy/lead_lag_taker.rs) with zero `use crate::exchanges::*`, zero hot-path heap allocations, and `observe(...)` / `observe_with_confirm(...)` 100% preserved:

1. **`L1BookObs` & `observe_l1_books(...)`**: computes arithmetic `mid()`, `spread_bps()`, size-weighted `microprice()`, and signed `obi()`; enables `use_microprice_fair: bool` (default `false`) and `ref_obi_confirm_min: f64` (default `0.0`, requiring `side.sign() * ref_obi >= ref_obi_confirm_min`).
2. **Target Spread-Cost Entry Gate**: `max_entry_tgt_spread_bps: Option<f64>` (default `None`) and `min_edge_over_spread_bps: Option<f64>` (default `None`, enforcing `|dev_bps| - 0.5 * tgt_spread_bps >= min_edge`).
3. **Leader-Reversal Early Stop**: `ref_reversal_exit_bps: Option<f64>` (default `None`). Once `now_ms >= entry_filled_est_ms` and both feeds are fresh, emits `LeadLagAction::Exit` if primary reference snaps back against entry by `>= ref_reversal_exit_bps` while retaining `self.position` until `notify_exit_filled()` (`AGENTS.md`).
4. **Zero-Alloc Consensus Helper `consensus_ref_mid`**: stack-only median over 3 fresh valid leaders (mean over 2, single over 1, `None` over 0), filtering NaN/Inf, non-positive prices, future receipts, and stale feeds (`now_ms - recv_ms > max_ref_age_ms`).

---

## 5. Stage B: Round-2 Ablation & Frozen Holdout / `W26` OOS Validation

All 40 pair parameter sets (`12` Crypto + `28` RWA; `18/40` selected jointly positive on baseline, `22/40` fallback `6.0 bp / 10s`) were selected and **frozen strictly on `selection_dev (w01–w20)`**, then replayed **once** on untouched `holdout (w21–w25)` and `w26_oos`.

### 5.1 Crypto (`BTC, ETH, SOL`) Across Splits & Latencies (`230 / 300 / 400 / 500 ms`)

| Split | Venue (`0/0` Std) | Arm | `230 ms` Quote / PV `$1k` ($) | `300 ms` Quote / PV `$1k` ($) | `400 ms` Quote / PV `$1k` ($) | `500 ms` Quote / PV `$1k` ($) | `230 ms` Trades (`PV`) | `230 ms` Pos Win (Q / PV) |
| --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `selection_dev` | `lighter_robinhood` | `A_baseline` | `-190.89 / +10.16` | `-243.65 / +6.30` | `-263.13 / -15.18` | `-284.55 / -52.68` | `2,571` (`624`) | `15.8% / 50.0%` |
| `selection_dev` | `lighter_robinhood` | `B_vol_adaptive` | `-120.20 / +25.61` | `-170.45 / +31.22` | `-173.88 / +31.46` | `-188.37 / -10.39` | `2,356` (`535`) | `26.3% / 55.6%` |
| `selection_dev` | `lighter_robinhood` | `M1_microprice_obi` | `-124.20 / +19.31` | `-176.22 / -3.50` | `-174.04 / -10.13` | `-185.09 / -27.72` | `1,984` (`490`) | `26.3% / 47.4%` |
| `selection_dev` | `lighter_robinhood` | `M2_consensus_leader` | `-686.99 / -6.87` | `-710.35 / -11.09` | `-730.25 / -25.33` | `-723.86 / -18.32` | `4,495` (`406`) | `10.5% / 52.9%` |
| `selection_dev` | `lighter_robinhood` | `M3_ref_reversal_spread_gate` | `-31.10 / -1.99` | `-126.41 / -8.17` | `-141.10 / -10.27` | `-157.95 / -15.73` | `1,998` (`845`) | `42.1% / 31.6%` |
| `selection_dev` | `lighter_robinhood` | **`M4_combined`** | **`-1.62 / +11.41`** | `-60.32 / +0.56` | `-69.41 / +1.84` | `-82.89 / -8.22` | `1,215` (`507`) | `36.8% / 52.6%` |
| `selection_dev` | `lighter` | `A_baseline` | `+685.35 / +238.91` | `+435.13 / +184.43` | `+390.59 / +168.41` | `+273.31 / +131.89` | `1,779` (`1,325`) | `100.0% / 60.0%` |
| `selection_dev` | `lighter` | `M1_microprice_obi` | `+510.81 / +186.64` | `+353.04 / +119.43` | `+319.43 / +120.05` | `+248.44 / +137.48` | `1,334` (`1,011`) | `93.3% / 66.7%` |
| `selection_dev` | `lighter` | **`M3_ref_reversal_spread_gate`** | **`+741.62`** `/ +182.05` | **`+453.26`** `/ +123.18` | **`+395.77`** `/ +99.04` | `+246.11 / +63.43` | `1,957` (`1,592`) | `100.0% / 66.7%` |
| `selection_dev` | `lighter` | `M4_combined` | `+542.77 / +136.68` | `+385.60 / +116.14` | `+340.46 / +108.77` | `+231.38 / +79.39` | `1,382` (`1,111`) | `100.0% / 73.3%` |
| **`holdout`** | `lighter_robinhood` | `A_baseline` | `-15.41 / +2.41` | `-39.61 / -8.01` | `-49.74 / -8.37` | `-58.86 / -13.52` | `654` (`117`) | `20.0% / 40.0%` |
| **`holdout`** | `lighter_robinhood` | **`M3_ref_reversal_spread_gate`** | **`+21.63 / +0.87`** | `+1.90 / -5.31` | `-7.82 / -2.62` | `-12.88 / -5.32` | `553` (`190`) | `60.0% / 40.0%` |
| **`holdout`** | `lighter_robinhood` | **`M4_combined`** | **`+17.74`** `/ -3.01` | **`+7.74`** `/ -0.93` | **`+0.62 / +0.50`** | `-4.01 / -1.85` | `362` (`132`) | `60.0% / 20.0%` |
| **`holdout`** | `lighter` | `A_baseline` | `+52.66 / +21.42` | `+21.45 / +44.19` | `+25.18 / +43.47` | `+28.95 / +42.05` | `366` (`228`) | `60.0% / 40.0%` |
| **`holdout`** | `lighter` | **`M1_microprice_obi`** | `+58.71` / **`+32.90`** | `+38.13` / **`+46.85`** | `+34.59` / **`+41.76`** | `+18.92 / +35.11` | `252` (`178`) | `100.0% / 60.0%` |
| **`holdout`** | `lighter` | `M2_consensus_leader` | `+35.61 / +38.56` | `+25.88 / +40.65` | `+26.92 / +37.79` | `+4.17 / +22.16` | `278` (`149`) | `80.0% / 100.0%` |
| **`holdout`** | `lighter` | **`M3_ref_reversal_spread_gate`** | **`+70.28`** `/ +6.74` | `+19.60 / +9.73` | `+21.35 / +10.25` | `+10.33 / +7.32` | `363` (`266`) | `80.0% / 40.0%` |
| **`holdout`** | `lighter` | `M4_combined` | `+56.99 / +9.40` | `+22.81 / +7.73` | `+16.61 / +5.94` | `+0.97 / +4.83` | `238` (`190`) | `100.0% / 60.0%` |
| **`w26_oos`** | `lighter_robinhood` | `A_baseline` | `-9.30 / -0.07` | `-9.54 / -0.68` | `-10.22 / -0.04` | `-13.78 / -0.49` | `46` (`4`) | `0.0% / 0.0%` |
| **`w26_oos`** | `lighter_robinhood` | `M4_combined` | `-0.93 / -0.22` | `-1.44` / **`+0.89`** | `-3.91 / -1.04` | `-6.36 / -2.20` | `22` (`7`) | `0.0% / 0.0%` |
| **`w26_oos`** | `lighter` | `A_baseline` | `-5.02 / -6.65` | `-6.51 / -6.09` | `-6.42 / -4.85` | `-5.33 / -1.85` | `21` (`8`) | `0.0% / 0.0%` |
| **`w26_oos`** | `lighter` | `M3_ref_reversal_spread_gate` | **`+2.21`** `/ -2.66` | **`+0.72`** `/ -2.92` | **`+0.76`** `/ -2.72` | `-0.14 / -2.84` | `19` (`12`) | `100.0% / 0.0%` |
| **`w26_oos`** | `lighter` | `M4_combined` | **`+1.82`** `/ -3.06` | `-0.39 / -3.15` | `-1.21 / -3.51` | `-1.77 / -3.61` | `12` (`7`) | `100.0% / 0.0%` |

### 5.2 RWA (`NVDA, AAPL, SPY, QQQ, TSLA, AMZN, MU`) Across Splits & Latencies

| Split | Venue (`0/0` Std) | Arm | `230 ms` Quote / PV `$1k` ($) | `300 ms` Quote / PV `$1k` ($) | `400 ms` Quote / PV `$1k` ($) | `500 ms` Quote / PV `$1k` ($) | `230 ms` Trades (`PV`) | `230 ms` Pos Win (Q / PV) |
| --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `selection_dev` | `lighter_robinhood` | `A_baseline` | `-1,353.84 / -297.33` | `-1,479.97 / -403.18` | `-1,496.71 / -387.69` | `-1,415.09 / -382.85` | `7,234` (`1,356`) | `0.0% / 26.7%` |
| `selection_dev` | `lighter_robinhood` | `B_vol_adaptive` | `-745.40 / -170.20` | `-924.73 / -235.73` | `-955.68 / -236.06` | `-839.40 / -147.46` | `5,349` (`929`) | `6.7% / 40.0%` |
| `selection_dev` | `lighter_robinhood` | `M3_ref_reversal_spread_gate` | `+3.20 / -8.90` | `-262.71 / -77.58` | `-291.72 / -82.12` | `-325.19 / -96.27` | `3,480` (`1,302`) | `26.7% / 13.3%` |
| `selection_dev` | `lighter_robinhood` | **`M4_combined`** | **`+80.65 / +17.28`** | **`+20.96 / +17.80`** | `-0.46` / **`+12.27`** | `-8.34` / **`+3.36`** | `828` (`276`) | `60.0% / 35.7%` |
| `selection_dev` | `lighter` | `A_baseline` | `-757.45 / -293.03` | `-981.56 / -487.65` | `-953.29 / -537.12` | `-902.09 / -388.89` | `7,408` (`1,883`) | `14.3% / 21.4%` |
| `selection_dev` | `lighter` | `M3_ref_reversal_spread_gate` | `-173.50 / -250.65` | `-473.10 / -263.73` | `-472.87 / -250.56` | `-345.11 / -92.38` | `6,155` (`2,391`) | `14.3% / 28.6%` |
| `selection_dev` | `lighter` | **`M4_combined`** | **`+68.91 / +22.22`** | **`+1.16 / +26.86`** | `-10.40` / **`+25.23`** | `-57.68` / **`+2.30`** | `1,815` (`514`) | `57.1% / 53.8%` |
| **`holdout`** | `lighter_robinhood` | `A_baseline` | `-411.71 / -29.17` | `-411.80 / -31.25` | `-392.99 / -34.59` | `-385.87 / -26.20` | `2,258` (`187`) | `0.0% / 0.0%` |
| **`holdout`** | `lighter_robinhood` | `M3_ref_reversal_spread_gate` | `-27.62 / -9.13` | `-55.05 / -16.08` | `-56.12 / -18.40` | `-65.66 / -18.15` | `772` (`163`) | `0.0% / 25.0%` |
| **`holdout`** | `lighter_robinhood` | **`M4_combined`** | **`+8.09`** `/ -0.03` | `-6.01 / -10.58` | `-3.54 / -10.63` | `-5.50 / -7.78` | `204` (`50`) | **`75.0%`** `/ 25.0%` |
| **`holdout`** | `lighter` | `A_baseline` | `-143.96 / -25.94` | `-132.64 / -34.20` | `-132.35 / -38.20` | `-140.05 / -24.97` | `2,239` (`358`) | `0.0% / 0.0%` |
| **`holdout`** | `lighter` | `M1_microprice_obi` | `-69.48` / **`+2.52`** | `-40.21` / **`+10.53`** | `-39.88` / **`+17.76`** | `-30.29` / **`+16.18`** | `1,315` (`210`) | `0.0% / 50.0%` |
| **`holdout`** | `lighter` | `M4_combined` | `-21.69 / -7.80` | `-26.22 / -6.21` | `-26.36 / -5.48` | `-22.67 / -2.70` | `497` (`110`) | `0.0% / 25.0%` |
| **`w26_oos`** | `lighter_robinhood` | `A_baseline` | `-15.05 / -0.05` | `-14.01 / -1.36` | `-12.99 / -1.86` | `-14.15 / -1.46` | `76` (`15`) | `0.0% / 0.0%` |
| **`w26_oos`** | `lighter_robinhood` | `M4_combined` | `-2.24 / -0.51` | `-3.59 / -1.23` | `-3.52 / -0.99` | `-3.18 / -0.47` | `8` (`4`) | `0.0% / 0.0%` |
| **`w26_oos`** | `lighter` | `A_baseline` | `-7.10 / -2.36` | `-3.30 / -2.19` | `-3.39 / -1.90` | `-3.62 / -1.28` | `79` (`13`) | `0.0% / 0.0%` |
| **`w26_oos`** | `lighter` | **`M4_combined`** | **`+0.87 / +0.04`** | **`+0.38`** `/ +0.00` | **`+0.38 / +0.02`** | **`+0.54 / +0.02`** | `5` (`1`) | **`100.0% / 100.0%`** |

### 5.3 Tradeoffs, Selected-Positive Cohorts & Honest Limits

1. **Early Reversal Exit vs Print Verification (`M3`/`M4` vs `M1`):** `ref_reversal_exit_bps` dramatically slashes quote drawdowns (e.g. `lighter_robinhood` Crypto `selection_dev` DD `$204.62 -> $16.82`, RWA `selection_dev` DD `$1,353.84 -> $9.36`), because cutting losing reversals early captures better quote exits. However, when a trade exits early via an aggressive taker order after a snapback, `public_trades` in the `1000 ms` print window capture immediate adverse flow; consequently, **`M1_microprice_obi` outperforms `M3`/`M4` on print-verified holdout PnL** (`+$32.90` at `230 ms` and `+$46.85` at `300 ms` on `lighter` Crypto; `+$2.52` to `+$17.76` on `lighter` RWA).
2. **`M2_consensus_leader` (`binance + coinbase + extended`):** Strict 3-leader median filtering reduces single-venue spikes on `lighter` Crypto (`100%` positive holdout PV windows at `300–400 ms`), but on `lighter_robinhood` Crypto (`selection_dev`) un-gated consensus triggers more small deviations (`4,495` trades, `-$686.99` quote) unless paired with spread & OBI gates.
3. **`w26_oos` Sample Size:** Calm `w26` produces very few `>= 6–8 bp` RWA/Crypto triggers (`5–22` trades on `M4`, `1–12` print-verified), so `w26_oos` is diagnostic only and does not supersede the `holdout (w21–w25)` evidence.

---

## 6. Verification & Pulled Artifacts

- **Owned files:** [lead_lag_taker.rs](../src/strategy/lead_lag_taker.rs), [research_track01_rwa_and_alpha_upgrade.py](../scripts/research_track01_rwa_and_alpha_upgrade.py), [test_research_track01_rwa_and_alpha_upgrade.py](../scripts/test_research_track01_rwa_and_alpha_upgrade.py), and this report.
- **Rust / Go / Python checks:**
  - `make fmt-check` & `make check` (`cargo clippy --locked --features research-replay --lib --bins -- -D warnings`, `go vet ./...`) — PASS
  - `cargo test --locked --features research-replay --lib --bins` (`1,142` lib including `17/17` `lead_lag_taker` tests + all bin tests) — PASS
  - `python3 -S -m unittest -v scripts/test_research_track01_rwa_and_alpha_upgrade.py` (`7/7` pure-stdlib tests) & `python3 -S scripts/research_track01_rwa_and_alpha_upgrade.py --self-test` — PASS
- **Pulled Aleph JSON Artifacts (`.agent-state/research/strategy_upgrade_20261127/`, excluded from Git, `LEAK_COUNT=0`):**
  - `track01_stage_a_diagnosis.json`: `sha256: 7983e09ac8b0c60460ed9df843c5987a1dfe62c0ad1e0e85bea40239f46e6b8d`
  - `track01_stage_b_upgrade.json`: `sha256: 3a7cf59e4f1e4e3308e355d8b416a39a1e806d8e08ebb793ec9ae04785e31e45`
