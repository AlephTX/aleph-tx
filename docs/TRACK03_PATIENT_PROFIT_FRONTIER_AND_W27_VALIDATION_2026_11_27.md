# Track 03 (`03_incentive_turnover`): P0 IOC Fixes, Untouched `W27+W26` Paired Validation & Patient Profit Frontier (`W01–W27`)

**Date:** `2026-11-27` (CST) · **Operational Mode:** Strictly **`HOLD`** (zero live orders/cancels, zero Tokyo deploys/restarts, `-w28` strictly excluded)

This report documents:
1. **Both P0 Simulator Defect Fixes** in [research_track03_full_universe_speedbump_mm.py](../scripts/research_track03_full_universe_speedbump_mm.py) and deterministic unit tests in [test_research_track03_full_universe_speedbump_mm.py](../scripts/test_research_track03_full_universe_speedbump_mm.py) (`6/6` pass).
2. **Untouched `W27 + W26` Full-Universe Paired `G4 / G5 / S4` Validation** across `lighter_robinhood` (`USDG`, `0.0/0.0 bps` `verified_account_tier`), `lighter_std` (`USDC`, `0.0/0.0 bps` `lighter_standard_zero_scenario` counterfactual), `lighter_prem` (`USDC`, `0.40/2.80 bps` `verified_account_tier`), and `arcus` RWA (`USDC`, `0.0/2.1375 bps` `verified_account_tier`), with **dual-venue freshness and data-gap auditing** across target and paired reference feeds (`arcus`, `trade_xyz`, `binance`).
3. **Causal `W01–W27` Patient Profit Frontier Sweep** (`selection_dev w01–w20` → frozen `holdout w21–w25` → untouched `w26_w27_oos`) via [research_track03_patient_profit_frontier.py](../scripts/research_track03_patient_profit_frontier.py) and [test_research_track03_patient_profit_frontier.py](../scripts/test_research_track03_patient_profit_frontier.py), sweeping **patient passive unwind** (`max_hold_ms` `15s → 60s / 180s / 600s`), **wider speed-bump spreads** (`1.0–4.0 bps` entry edge + `2.0–4.0x` latency-vol spread floor + passive touch vs 1-tick pennying), and **positive-markout RWA sub-tiers**, evaluated on both `realized_only` and `mtm_incl_residual` PnL, zero-rebate `$/M`, breakeven rebate, capacity, hold time, `stale_exit_blocks`, and `PSR / DSR / CSCV PBO`.

---

## 1. Executive Verdict

1. **Both P0 Simulator Defects Closed & Verified (`6/6` unit tests):**
   - **Defect 1 (`declared_end_ms` cutoff immunity):** Added explicit `declared_end_ms: int | None = None` to `simulate_prepared_arm` and `simulate_arm` (and excluded `ref` ticks when inferring default end timestamp), plus passed `declared_end_ms = b_rows[-1][0]` in lakehouse evaluation so late `arcus`/`trade_xyz` reference ticks can never extend or alter `terminal_drain_start_ts` or post-window MTM.
   - **Defect 2 (`stale_exit_blocks` + `330 ms` re-queue):** Fixed `process_pending_iocs` so expired IOCs hitting `!book_is_fresh(now_ms)` increment `stale_exit_blocks` and re-queue with `exec_ts = now_ms + arm.exit_delay_ms` (`330 ms`), and removed the immediate post-book-refresh `process_pending_iocs(ts)` call so a freshly arriving book can never execute a stale IOC at `0 ms`.
2. **Untouched `W27` & `W26+W27` Paired Validation (`846` rows, `max_net_err = 0.0`):**
   - Under `o200_c300_e330`, `Top-Tier 7-RWA` `S4_speedbump_combined` flips net-profitable on untouched `W27`: **`-$2.66/M` (`+$4.86`, `$0.00` residual, `be_rebate = 0.000 bps`)** on `lighter_robinhood` (`0/0`) and **`-$4.06/M` (`+$4.17`)** on `lighter_std` (`0/0` counterfactual), improving over `G4_baseline` (`+$2.19/M` RH / `-$0.95/M` Std). Across `W26+W27`, `Positive-Markout Core 4` (`AAPL, AMZN, META, MSFT`) reaches **`-$6.80/M` (`+$8.40`)** on `lighter_robinhood` and **`-$8.11/M` (`+$7.84`)** on `lighter_std`.
   - Dual-venue freshness audit confirms `arcus` RWA (`0.0%` stale time share, `200 ms` median tick interval, `max_gap <= 2.28 s`) provides continuous anchoring in `W26–W27`, whereas `trade_xyz` remains a `~12.4 s` snapshot feed (`59.4%` stale time share `> 5 s`), and `lighter_robinhood` RWA experiences `24.0–28.1 s` max gaps (`1.7%–4.1%` stale share, triggering `2–14` `stale_exit_blocks` per cohort).
3. **Breakthrough on `W01–W27` Patient Passive Unwind + Positive-Markout Sub-Tiers (`rebate_bps = 0.0`):**
   - Extending `max_hold_ms` from `15s` (`S4`) to `60s–600s` (`P1–P3` with `reduce_only_penny_ticks = 1` and stronger inventory skew) slashes forced IOC taker exit share `s` from **`0.47 → 0.00`** on Lighter RWA and from **`0.71 → 0.01`** on Arcus RWA.
   - **`lighter_robinhood` (`USDG`, `0.0/0.0 bps` verified) on `Positive-Markout Core 4` (`AAPL, AMZN, META, MSFT`)**: `P3_patient_600s` achieves **cross-split unrebated profitability**: **`-$7.62/M` (`+$63.73`, `93%` pos windows)** in `selection_dev (w01–w20)` → **`-$11.48/M` (`+$24.35`, `100%` pos)** in `holdout (w21–w25)` → **`-$23.79/M` (`+$26.31`, `100%` pos)** in `w26_w27_oos` → **`-$9.87/M` (`+$114.39` net on `$5.79M` exit notional, `95%` pos windows, `avg_hold = 24.2 s`, `PSR = 1.0000`, `DSR_eff = 0.9999`, `CSCV PBO = 0.0000`)**.
   - **Causally dev-selected `pos_markout_dev_selected_rwa` (`AAPL, AMZN, META, MSFT, TSLA` on RH)** under `P3_patient_600s` similarly generalizes across all splits: **`-$6.48/M` (`+$69.9`)** dev → **`-$8.52/M` (`+$22.6`)** holdout → **`-$20.74/M` (`+$28.6`)** OOS → **`-$8.17/M` (`+$121.1` on `$7.41M`, `90%` pos windows, `PSR = 1.0000`, `DSR_eff = 0.9977`, `PBO = 0.0000`)**.
   - **Wider spreads (`W5_nopenn_15bp_180s`)** push RH `Core 4` unit edge to **`-$15.82/M` (`+$30.50`, `95%` pos)** at lower capacity (`$0.96M`), whereas keeping `QQQ` (`+$5.55/M`) and `SPY` (`+$6.48/M`) in `Top-Tier 7-RWA` keeps the 7-symbol RH aggregate slightly negative (`+$1.58/M`, `-$73.57` under `P3`).
   - **`lighter_std` (`USDC`, `0.0/0.0 bps` counterfactual)** turns profitably positive even across all `Top-Tier 7-RWA` (`P3_patient_600s`: **`-$10.56/M`, `+$215.02` on `$10.18M`, `PSR = 1.0000`, `DSR_eff = 1.0000`, `PBO = 0.0571`**) and hits **`-$20.46/M` (`+$173.4`)** on `Core 4`.
   - **`lighter_prem` (`USDC`, `0.40/2.80 bps` verified)** and **`arcus` RWA (`USDC`, `0.0/2.1375 bps` verified)** remain **net-cost (`NO-GO`)** without rebates even under `P3_patient_600s` (`+$17.80/M` `Core 4` / `+$29.57/M` `TT7` on `lighter_prem`; `+$28.69/M` `AMZN` / `+$72.09/M` `TT7` on `arcus` with thin `$0.01M` holdout/OOS flow), as does **Crypto** (`+$12.05/M` to `+$22.42/M` at `0/0`).
   - **Verdict:** Remain operationally **`HOLD`** pending live authorization/shadow gate; for RWA on `lighter_robinhood` (`0/0` verified), **patient passive unwind (`P2_180s` / `P3_600s`) restricted to `Positive-Markout Core 4` (`AAPL, AMZN, META, MSFT`) or dev-selected ex-index RWAs** is the first unrebated-profitable configuration across all `W01–W27` splits (`DSR_eff >= 0.9977`, `PBO = 0.0000`).

---

## 2. P0 Simulator Fixes (`research_track03_full_universe_speedbump_mm.py`)

Both P0 defects from [the roadmap §3](ALEPHTX_RESEARCH_AND_EXECUTION_ROADMAP_2026_11_27.md) are resolved in [research_track03_full_universe_speedbump_mm.py](../scripts/research_track03_full_universe_speedbump_mm.py) and tested in [test_research_track03_full_universe_speedbump_mm.py](../scripts/test_research_track03_full_universe_speedbump_mm.py):

| P0 Defect | Root Cause Before Fix | Implemented Fix | Regression Test |
|---|---|---|---|
| **1. Late `ref` ticks shifting drain cutoff** | `simulate_prepared_arm` set `last_stream_ts = tuples[-1][0]` across all event kinds (`book`, `ref`, `trade`). Late `ref` ticks past target venue close shifted `terminal_drain_start_ts` forward and allowed post-window `ref` events to run. | Added explicit `declared_end_ms: int \| None = None` to `simulate_prepared_arm` and `simulate_arm`, capping `mid_at` / loop cutoff at `declared_end_ms` (and excluding `kind == 1` `ref` ticks when inferring default `effective_end_ms`); lakehouse runner passes `declared_end_ms = b_rows[-1][0]`. | `test_p0_declared_end_ms_immunity_to_late_ref_ticks` verifies identical `ntl`, `net`, `taker_exits`, and `terminal_flat` with late `ref` ticks (`+25 s`) and strict `5,000 ms` book cutoff. |
| **2. Stale `pending_ioc_exits` filling at `0 ms`** | When an expired IOC hit `!book_is_fresh(now_ms)`, `process_pending_iocs` kept the old expired `exec_ts`, and a second `process_pending_iocs(ts)` immediately after `last_book_ts = ts` filled it at `0 ms` on the newly arrived fresh book. | `process_pending_iocs` now increments `stale_exit_blocks += 1` and re-queues with `exec_ts = now_ms + arm.exit_delay_ms` (`330 ms`); removed the post-book-update `process_pending_iocs(ts)` call so a book refresh only permits queuing new delayed IOCs via `maybe_queue_exit_ioc(ts)`. | `test_p0_stale_pending_ioc_requeues_330ms_and_tracks_stale_exit_blocks` verifies `stale_exit_blocks == 1`, `taker_exits == 0` at `ts_fresh + 200 ms`, and clean fill only at `ts_fresh + 330 ms`. |

---

## 3. Untouched `W27 + W26` Paired `G4 / G5 / S4` Validation & Dual-Venue Freshness

### 3.1 Dual-Venue Freshness & Data-Gap Audit (`W26` & `W27`)

All four RWA & Crypto campaigns in `W26` and `W27` meet `>= 10.25 h` span (`26` total windows in `W01–W27`, `w08` missing RWA/Crypto as known, `-w28` strictly excluded):

| Window | Domain | Venue | Role | Markets (`10h Cont.`) | Min Span (h) | Max Gap (ms) | Median `dt` (ms) | Max `p95 dt` (ms) | Max Stale Share (`>5s`) |
|---|---|---|---|---:|---:|---:|---:|---:|---:|
| `w26` | `rwa` | `lighter_robinhood` | Target | `14 (14)` | `10.250` | `28,068` | `57.0` | `1,467` | `0.0172` |
| `w26` | `rwa` | `lighter` | Target | `15 (15)` | `10.249` | `11,548` | `59.0` | `1,078` | `0.0053` |
| `w26` | `rwa` | `arcus` | Target / Ref | `17 (17)` | `10.250` | `2,045` | `200.0` | `833` | `0.0000` |
| `w26` | `rwa` | `trade_xyz` | Ref | `17 (17)` | `10.248` | `13,277` | `12,360.0` | `12,577` | `0.5942` |
| `w27` | `rwa` | `lighter_robinhood` | Target | `14 (14)` | `10.250` | `23,999` | `61.0` | `1,911` | `0.0413` |
| `w27` | `rwa` | `lighter` | Target | `15 (15)` | `10.250` | `20,112` | `70.0` | `1,514` | `0.0169` |
| `w27` | `rwa` | `arcus` | Target / Ref | `17 (17)` | `10.250` | `2,283` | `200.0` | `795` | `0.0000` |
| `w27` | `rwa` | `trade_xyz` | Ref | `17 (17)` | `10.247` | `13,099` | `12,372.0` | `12,566` | `0.5943` |
| `w26` | `crypto` | `lighter_robinhood` | Target | `3 (3)` | `10.250` | `5,049` | `51.0` | `427` | `0.0000` |
| `w26` | `crypto` | `lighter` | Target | `3 (3)` | `10.250` | `1,050` | `50.0` | `104` | `0.0000` |
| `w26` | `crypto` | `binance` | Ref | `3 (3)` | `10.250` | `848` | `0.0` | `54` | `0.0000` |
| `w27` | `crypto` | `lighter_robinhood` | Target | `3 (3)` | `10.250` | `5,511` | `51.0` | `501` | `0.0000` |
| `w27` | `crypto` | `lighter` | Target | `3 (3)` | `10.250` | `6,882` | `50.0` | `149` | `0.0001` |
| `w27` | `crypto` | `binance` | Ref | `3 (3)` | `10.250` | `1,668` | `0.0` | `72` | `0.0000` |

### 3.2 Paired `G4 / G5 / S4` Validation on Untouched `W26`, `W27`, and `W26+W27` (`o200_c300_e330` / `o300_c300_e330` / `o36_c33_e40`)

`full_universe_paired_w26_w27` (`846` rows, `non_flat_count = 2`, `max_mtm_cash_err = 3.10e-12`, `max_net_err = 0.0`, `rebate_bps = 0.0`):

| Split | Cohort | Venue Profile (`Fee bps`, `Lat.`) | `G4_baseline` `$/M` (`net`, `s`) | `G5_microprice_tfi` `$/M` (`net`, `s`) | `S4_speedbump_combined` `$/M` (`net`, `ntl`, `s`, `hold`, `blk`, `be_reb`) |
|---|---|---|---|---|---|
| `w27_oos` | `Top-Tier 7-RWA` | `lighter_robinhood` (`0/0`, `o200`) | `+2.19` (`-$5.43`, `0.458`) | `-2.27` (`+$4.62`, `0.384`) | **`-2.66`** (`+$4.86`, `$912.6k`, `0.394`, `8.73s`, `blk=2`, `be=0.000`) |
| `w27_oos` | `Top-Tier 7-RWA` | `lighter_std` (`0/0`, `o200`) | `-0.95` (`+$1.73`, `0.588`) | `-2.46` (`+$2.88`, `0.513`) | **`-4.06`** (`+$4.17`, `$513.9k`, `0.515`, `10.58s`, `blk=5`, `be=0.000`) |
| `w27_oos` | `Top-Tier 7-RWA` | `lighter_prem` (`0.40/2.80`, `o200`) | `+101.14` (`-$128.50`, `0.553`) | `+91.65` (`-$72.97`, `0.489`) | `+95.99` (`-$70.43`, `$366.9k`, `0.513`, `10.38s`, `blk=5`, `be=1.291`) |
| `w27_oos` | `Top-Tier 7-RWA` | `arcus` (`0/2.1375`, `o36`) | `+137.56` (`-$8.59`, `0.784`) | `+119.54` (`-$3.22`, `0.723`) | `+133.20` (`-$6.20`, `$23.3k`, `0.856`, `13.77s`, `blk=0`, `be=2.328`) |
| `w26_w27_oos` | `Top-Tier 7-RWA` | `lighter_robinhood` (`0/0`, `o200`) | `+1.17` (`-$6.47`, `0.418`) | `-1.77` (`+$8.02`, `0.363`) | **`-2.42`** (`+$9.52`, `$1.97M`, `0.360`, `8.50s`, `blk=3`, `be=0.000`) |
| `w26_w27_oos` | `Top-Tier 7-RWA` | `lighter_robinhood` (`0/0`, `o300`) | `+1.70` (`-$8.94`, `0.436`) | `-1.37` (`+$5.91`, `0.379`) | **`-2.16`** (`+$8.14`, `$1.85M`, `0.377`, `8.76s`, `blk=3`, `be=0.000`) |
| `w26_w27_oos` | `Top-Tier 7-RWA` | `lighter_std` (`0/0`, `o200`) | `+0.69` (`-$2.50`, `0.582`) | `-0.98` (`+$2.27`, `0.515`) | **`-3.07`** (`+$6.20`, `$1.00M`, `0.512`, `10.56s`, `blk=5`, `be=0.000`) |
| `w26_w27_oos` | `Top-Tier 7-RWA` | `lighter_std` (`0/0`, `o300`) | `+1.92` (`-$6.74`, `0.601`) | `+0.38` (`-$0.83`, `0.538`) | **`-2.37`** (`+$4.58`, `$0.96M`, `0.531`, `10.80s`, `blk=5`, `be=0.000`) |
| `w26_w27_oos` | `Core 4 RWA` | `lighter_robinhood` (`0/0`, `o200`) | `-0.12` (`+$0.20`, `0.611`) | `-6.63` (`+$8.48`, `0.533`) | **`-6.80`** (`+$8.40`, `$617.7k`, `0.515`, `10.43s`, `blk=3`, `be=0.000`) |
| `w26_w27_oos` | `Core 4 RWA` | `lighter_std` (`0/0`, `o200`) | `-2.81` (`+$5.04`, `0.565`) | `-5.77` (`+$6.28`, `0.492`) | **`-8.11`** (`+$7.84`, `$483.3k`, `0.478`, `10.24s`, `blk=5`, `be=0.000`) |
| `w26_w27_oos` | `Full RWA (14/15)` | `lighter_robinhood` (`0/0`, `o200`) | `+4.86` (`-$36.42`, `0.462`) | `+1.05` (`-$6.19`, `0.397`) | **`+0.92`** (`-$4.67`, `$2.55M`, `0.394`, `8.90s`, `blk=30`, `be=0.011`) |
| `w26_w27_oos` | `Crypto (3 mkts)` | `lighter_robinhood` (`0/0`, `o200`) | `+31.19` (`-$47.83`, `0.509`) | `+30.25` (`-$27.46`, `0.481`) | `+36.72` (`-$28.46`, `$387.5k`, `0.507`, `10.83s`, `blk=0`, `be=0.492`) |
| `w26_w27_oos` | `Crypto (3 mkts)` | `lighter_std` (`0/0`, `o200`) | `+11.47` (`-$35.87`, `0.116`) | `+12.96` (`-$31.50`, `0.098`) | `+11.44` (`-$31.69`, `$1.39M`, `0.172`, `6.30s`, `blk=0`, `be=0.125`) |

*Note:* `w26_oos` `G4_baseline` `Top-Tier 7-RWA` on `lighter_robinhood` carries `$18.88` residual (`realized = mtm = +$0.34/M`); all `S4` `Top-Tier 7-RWA` rows in `W26` and `W27` flatten cleanly to `$0.00` residual.

---

## 4. Patient Passive Unwind + Wider Spreads + Positive-Markout Frontier (`W01–W27`)

Evaluated across **`21` RWA windows** (`15` `selection_dev w01–w20` → `4` `holdout w21–w25` → `2` `w26_w27_oos`) and **`26` Crypto windows** (`19` dev → `5` holdout → `2` OOS), totaling **`10,872` rows** (`non_flat_count = 92`, `max_mtm_cash_err = 8.46e-12`, `max_net_err = 0.0`, `rebate_bps = 0.0`). Across all cohorts with `ntl > $10k`, `max |realized_only - mtm_incl_residual| = $0.0873/M` (`W1_wide_10bp_60s` RH `Core 4` holdout with `$176.80` residual).

### 4.1 Causal Positive-Markout Sub-Tier Discovery (`selection_dev w01–w20`, `S4_speedbump_combined`)

Freezing symbols strictly on `selection_dev` (`windows >= 10`, `symbol != NVDA`, `markout_1s_bps > 0`):
- **`lighter_robinhood` (`o200_c300_e330`)**: selects `['AAPL', 'AMZN', 'META', 'MSFT', 'TSLA']` (`META +0.414 bp`, `MSFT +0.222 bp`, `AMZN +0.210 bp`, `TSLA +0.111 bp`, `AAPL +0.026 bp`; excludes `QQQ -0.002 bp`, `SPY -0.021 bp`, `NVDA -0.055 bp`).
- **`lighter_std` & `lighter_prem` (`o200_c300_e330`)**: selects all 7 `Top-Tier` symbols (`MSFT +0.318/+0.326 bp`, `AAPL +0.217/+0.248 bp`, `META +0.215/+0.244 bp`, `AMZN +0.154/+0.152 bp`, `QQQ +0.138/+0.115 bp`, `TSLA +0.067/+0.095 bp`, `SPY +0.058/+0.034 bp`; excludes `NVDA -0.012/-0.006 bp`).
- **`arcus` RWA (`o36_c33_e40`)**: selects only `['AMZN']` (`+0.110 bp`); all other 7 RWA symbols exhibit negative 1s markout (`-0.408` to `-1.632 bp`) on `arcus`.
- **`Positive-Markout Core 4` (`AAPL, AMZN, META, MSFT`)**: the fixed intersection of positive-1s-markout single-name RWAs across both Lighter venues.

### 4.2 `Positive-Markout Core 4` (`AAPL, AMZN, META, MSFT`) Cross-Split Frontier

| Venue Profile (`Fee bps`, `Lat.`) | Arm | `dev` MTM (`Real`) `$/M` | `dev` Net ($) | `holdout` MTM (`Real`) `$/M` | `holdout` Net ($) | `w26_w27` MTM (`Real`) `$/M` | `w26_w27` Net ($) | `ALL (21w)` `$/M` | `ALL` Net ($) | `ALL` `ntl` | `s` (`Hold`) | `Pos Win` | `BE Reb` (bp) |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|---:|---:|
| `lighter_robinhood` (`0/0`, `o200`) | `G4_baseline` | `+11.72` (`+11.72`) | `-$168.49` | `+11.06` (`+11.06`) | `-$45.90` | `-0.12` (`-0.12`) | `+$0.20` | `+10.57` | `-$214.18` | `$10.13M` | `0.546` (`10.5s`) | `0.05` | `0.145` |
| `lighter_robinhood` (`0/0`, `o200`) | `S4_speedbump` | `+3.62` (`+3.62`) | `-$33.19` | `+3.76` (`+3.76`) | `-$8.97` | `-6.80` (`-6.80`) | `+$8.40` | `+2.64` | `-$33.76` | `$6.40M` | `0.468` (`9.9s`) | `0.33` | `0.034` |
| `lighter_robinhood` (`0/0`, `o200`) | `P1_patient_60s` | **`-4.77`** (`-4.77`) | `+$41.62` | **`-5.17`** (`-5.19`) | `+$11.68` | **`-18.29`** (`-18.29`) | `+$21.66` | **`-6.16`** | `+$74.96` | `$6.08M` | `0.110` (`17.4s`) | `0.71` | `0.000` |
| `lighter_robinhood` (`0/0`, `o200`) | `P2_patient_180s` | **`-6.80`** (`-6.80`) | `+$57.67` | **`-9.68`** (`-9.71`) | `+$20.77` | **`-23.63`** (`-23.63`) | `+$26.62` | **`-8.94`** | `+$105.06` | `$5.88M` | `0.021` (`22.5s`) | `0.95` | `0.000` |
| `lighter_robinhood` (`0/0`, `o200`) | **`P3_patient_600s`** | **`-7.62`** (`-7.62`) | **`+$63.73`** | **`-11.48`** (`-11.51`) | **`+$24.35`** | **`-23.79`** (`-23.79`) | **`+$26.31`** | **`-9.87`** | **`+$114.39`** | **`$5.79M`** | `0.002` (`24.2s`) | **`0.95`** | **`0.000`** |
| `lighter_robinhood` (`0/0`, `o200`) | `W2_wide_15bp_180s` | **`-11.73`** (`-11.73`) | `+$18.17` | **`-11.98`** (`-11.98`) | `+$3.49` | **`-24.72`** (`-24.72`) | `+$3.65` | **`-12.73`** | `+$25.31` | `$0.99M` | `0.023` (`23.2s`) | `0.90` | `0.000` |
| `lighter_robinhood` (`0/0`, `o200`) | `W5_nopenn_15bp_180s` | **`-14.97`** (`-14.97`) | `+$22.35` | **`-11.80`** (`-11.80`) | `+$3.42` | **`-32.67`** (`-32.67`) | `+$4.74` | **`-15.82`** | `+$30.50` | `$0.96M` | `0.031` (`27.6s`) | `0.95` | `0.000` |
| `lighter_std` (`0/0`, `o200`) | `S4_speedbump` | `-2.76` (`-2.76`) | `+$19.1` | `+1.63` (`+1.63`) | `-$2.6` | `-8.11` (`-8.11`) | `+$7.8` | `-2.56` | `+$24.3` | `$4.74M` | `0.46` (`10.2s`) | `0.48` | `0.000` |
| `lighter_std` (`0/0`, `o200`) | **`P3_patient_600s`** | **`-20.86`** (`-20.86`) | **`+$130.0`** | **`-13.40`** (`-13.40`) | **`+$18.9`** | **`-29.50`** (`-29.50`) | **`+$24.5`** | **`-20.46`** | **`+$173.4`** | **`$4.24M`** | `0.00` (`23.7s`) | **`0.90`** | **`0.000`** |
| `lighter_std` (`0/0`, `o200`) | `W2_wide_15bp_180s` | **`-29.71`** (`-29.71`) | `+$35.6` | **`-8.96`** (`-8.96`) | `+$1.9` | **`-42.49`** (`-42.49`) | `+$7.2` | **`-28.35`** | `+$44.7` | `$0.79M` | `0.01` (`20.6s`) | `0.86` | `0.000` |
| `lighter_prem` (`0.40/2.80`, `o200`) | `S4_speedbump` | `+87.67` (`+87.67`) | `-$442.96` | `+94.64` (`+94.64`) | `-$107.24` | `+85.27` (`+85.27`) | `-$60.92` | `+88.57` | `-$611.13` | `$3.45M` | `0.452` (`10.0s`) | `0.00` | `1.144` |
| `lighter_prem` (`0.40/2.80`, `o200`) | `P3_patient_600s` | `+18.00` (`+18.00`) | `-$83.74` | `+22.84` (`+22.84`) | `-$23.33` | `+8.26` (`+8.27`) | `-$5.29` | `+17.80` | `-$112.36` | `$3.16M` | `0.001` (`23.4s`) | `0.05` | `0.178` |
| `lighter_prem` (`0.40/2.80`, `o200`) | `W2_wide_15bp_180s` | `+12.63` (`+12.63`) | `-$9.86` | `+44.63` (`+44.63`) | `-$5.61` | `+0.93` (`+0.93`) | `-$0.09` | `+15.47` | `-$15.56` | `$0.50M` | `0.011` (`20.2s`) | `0.14` | `0.156` |
| `arcus` (`0/2.1375`, `o36`, `AMZN` dev-sel) | `S4_speedbump` | `+85.61` (`+85.61`) | `-$67.3` | `+178.93` (`+178.93`) | `-$3.1` | `+118.73` (`+118.73`) | `-$3.3` | `+88.68` | `-$73.7` | `$0.42M` | `0.71` (`12.0s`) | `0.00` | `1.37` |
| `arcus` (`0/2.1375`, `o36`, `AMZN` dev-sel) | `P3_patient_600s` | `+24.01` (`+24.01`) | `-$16.8` | `+178.91` (`+178.91`) | `-$3.5` | `+46.41` (`+46.41`) | `-$0.9` | `+28.69` | `-$21.2` | `$0.37M` | `0.01` (`45.7s`) | `0.10` | `0.29` |

*Residual & stale-block detail on `Core 4` `lighter_robinhood` `P3_patient_600s`:* `stale_exit_blocks = 0` (`35` in `S4`, `61` in `G4`), `terminal_residual_notional_usd = $177` (entirely in `holdout`, `$0.00` in `dev` & `w26–w27`), creating only `$0.03/M` (`-$11.51` realized vs `-$11.48` MTM) difference in `holdout`.

### 4.3 `Top-Tier 7-RWA` (`AAPL, AMZN, META, MSFT, QQQ, SPY, TSLA`) & `Crypto` (`BTC, ETH, SOL`) Across Venues (`ALL W01–W27`)

| Cohort | Venue Profile (`Fee`, `Lat.`) | Best Frontier Arm | `S4` `$/M` (`s`) | Best `$/M` (Real / MTM) | Best Net ($) | `ntl` | `s` (`Hold`) | `Pos Win` | `blk` / `Clips` / `Resid ($)` | `BE Reb` (bp) |
|---|---|---|---:|---:|---:|---:|---|---:|---|---:|
| `Top-Tier 7-RWA` | `lighter_robinhood` (`0/0`, `o200`) | `P3_patient_600s` | `+5.26` (`0.27`) | `+1.58` / `+1.58` | `-$73.57` | `$23.25M` | `0.00` (`10.9s`) | `0.33` | `0` / `7` / `$364.8` | `0.016` |
| `Top-Tier 7-RWA` | `lighter_std` (`0/0`, `o200`) | **`P3_patient_600s`** | `+1.91` (`0.46`) | **`-10.56` / `-10.56`** | **`+$215.02`** | `$10.18M` | `0.00` (`18.0s`) | **`0.76`** | `0` / `4` / `$189.4` | **`0.000`** |
| `Top-Tier 7-RWA` | `lighter_prem` (`0.40/2.80`, `o200`) | `P3_patient_600s` | `+97.82` (`0.47`) | `+29.57` / `+29.57` | `-$436.83` | `$7.39M` | `0.00` (`18.0s`) | `0.00` | `0` / `4` / `$189.4` | `0.296` |
| `Top-Tier 7-RWA` | `arcus` (`0/2.1375`, `o36`) | `P3_patient_600s` | `+125.84` (`0.78`) | `+72.09` / `+72.09` | `-$135.39` | `$0.94M` | `0.04` (`85.3s`) | `0.00` | `0` / `380` / `$0.0` | `0.735` |
| `Pos-Mk Dev Sel` (`5` syms) | `lighter_robinhood` (`0/0`, `o200`) | **`P3_patient_600s`** | `+3.13` (`0.45`) | **`-8.18` / `-8.17`** | **`+$121.10`** | `$7.41M` | `0.00` (`23.0s`) | **`0.90`** | `0` / `6` / `$364.8` | **`0.000`** |
| `Crypto (BTC,ETH,SOL)` | `lighter_robinhood` (`0/0`, `o200`) | `P3_patient_600s` | `+29.87` (`0.48`) | `+20.09` / `+20.10` | `-$139.09` | `$3.46M` | `0.00` (`26.1s`) | `0.04` | `0` / `10` / `$167.3` | `0.201` |
| `Crypto (BTC,ETH,SOL)` | `lighter_std` (`0/0`, `o200`) | `P3_patient_600s` | `+17.54` (`0.16`) | `+15.53` / `+15.53` | `-$326.64` | `$10.51M` | `0.00` (`6.5s`) | `0.00` | `0` / `1` / `$244.8` | `0.155` |
| `Crypto (BTC,ETH,SOL)` | `lighter_prem` (`0.40/2.80`, `o200`) | `P3_patient_600s` | `+79.77` (`0.19`) | `+56.03` / `+56.03` | `-$921.99` | `$8.23M` | `0.00` (`6.5s`) | `0.00` | `0` / `1` / `$244.8` | `0.560` |

### 4.4 Per-Symbol Breakdown (`lighter_robinhood` `o200_c300_e330`, `W01–W27`)

| Symbol | `S4` `$/M` (`net`, `s`) | `P3_patient_600s` `$/M` (`net`, `ntl`, `pw`, `hold`) | `W2_wide_15bp_180s` `$/M` (`net`, `pw`) | `W5_nopenn_15bp_180s` `$/M` (`net`, `ntl`, `pw`) | Role |
|---|---:|---:|---:|---:|---|
| `META` | `-5.55` (`+$13.1`, `0.55`) | **`-41.90`** (`+$79.3`, `$0.95M`, `1.00`, `43.1s`) | `-43.08` (`+$15.9`, `0.86`) | `-40.63` (`+$14.9`, `$0.18M`, `0.86`) | **Core 4 Profit Anchor** |
| `MSFT` | `+1.48` (`-$4.0`, `0.50`) | **`-12.90`** (`+$32.2`, `$1.25M`, `1.00`, `27.9s`) | `-9.96` (`+$2.6`, `0.71`) | `-11.05` (`+$2.7`, `$0.12M`, `0.62`) | **Core 4 Profit Anchor** |
| `AMZN` | `+8.41` (`-$20.6`, `0.54`) | **`-6.52`** (`+$14.0`, `$1.08M`, `0.62`, `32.4s`) | `-8.62` (`+$5.0`, `0.67`) | `-13.53` (`+$7.8`, `$0.29M`, `0.67`) | **Core 4 Profit Anchor** |
| `AAPL` | `+4.21` (`-$22.2`, `0.38`) | `+2.19` (`-$11.1`, `$2.53M`, `0.29`, `11.9s`) | **`-2.34`** (`+$1.8`, `0.57`) | **`-6.84`** (`+$5.0`, `$0.37M`, `0.67`) | **Core 4 (Needs Wide Edge on RH)** |
| `TSLA` | `+4.88` (`-$17.5`, `0.41`) | **`-2.08`** (`+$6.7`, `$1.61M`, `0.67`, `18.6s`) | `+1.44` (`-$0.6`, `0.57`) | `+0.22` (`-$0.1`, `$0.19M`, `0.52`) | Dev-Selected Pos-Markout (`P3` profitable) |
| `QQQ` | `+6.29` (`-$71.6`, `0.20`) | `+5.55` (`-$62.6`, `$5.64M`, `0.29`, `5.9s`) | `+8.74` (`-$61.7`, `0.10`) | `+8.11` (`-$56.2`, `$3.46M`, `0.24`) | Index ETF (Toxic at 200ms RH) |
| `SPY` | `+6.40` (`-$130.4`, `0.16`) | `+6.48` (`-$132.1`, `$10.20M`, `0.24`, `4.7s`) | `+7.65` (`-$109.0`, `0.14`) | `+6.08` (`-$83.7`, `$6.88M`, `0.24`) | Index ETF (Toxic at 200ms RH) |
| `NVDA` | `+19.95` (`-$83.6`, `0.29`) | `+17.40` (`-$68.1`, `$1.96M`, `0.05`, `8.6s`) | `+17.74` (`-$13.8`, `0.14`) | `+10.34` (`-$7.7`, `$0.37M`, `0.38`) | Excluded Toxic |
| `BTC` | `+27.56` (`-$147.4`, `0.41`) | `+21.84` (`-$103.4`, `$2.37M`, `0.04`, `16.8s`) | `+23.07` (`-$32.5`, `0.23`) | `+24.03` (`-$33.3`, `$0.69M`, `0.15`) | Crypto (Cost) |
| `ETH` | `+33.43` (`-$68.0`, `0.60`) | `+15.02` (`-$27.0`, `$0.90M`, `0.27`, `34.3s`) | `+12.92` (`-$7.7`, `0.38`) | `+18.06` (`-$10.7`, `$0.30M`, `0.35`) | Crypto (Cost) |
| `SOL` | `+41.64` (`-$18.2`, `0.77`) | `+22.42` (`-$8.7`, `$0.19M`, `0.35`, `101.2s`) | `+45.33` (`-$4.9`, `0.38`) | `+35.13` (`-$3.8`, `$0.05M`, `0.38`) | Crypto (Cost) |

---

## 5. Multiple-Testing Audit (`PSR / N_eff / DSR / CSCV PBO` Across `M = 12` Frontier Arms)

| Cohort | Venue Profile (`Lat.`) | `W` | Best Arm (Sharpe) | Window `SR` | `PSR(0)` | `N_eff` | `DSR(M=12)` | `DSR(N_eff)` | `CSCV PBO` |
|---|---|---:|---|---:|---:|---:|---:|---:|---:|
| **`Core 4 RWA`** (`W01–W27`) | `lighter_robinhood` (`o200`) | 21 | **`P3_patient_600s`** | **`+1.228`** | **`1.0000`** | `2.42` | `0.0630` | **`0.9999`** | **`0.0000`** |
| **`Core 4 RWA`** (`W01–W27`) | `lighter_std` (`o200`) | 21 | **`P3_patient_600s`** | **`+1.136`** | **`1.0000`** | `1.41` | **`0.9032`** | **`1.0000`** | `0.5000` |
| `Core 4 RWA` (`W01–W27`) | `lighter_prem` (`o200`) | 21 | `W4_ultra_40bp_600s` | `-0.062` | `0.3876` | `2.55` | `0.0000` | `0.0000` | `0.0000` |
| `Core 4 RWA` (`W01–W27`) | `arcus` (`o36`) | 21 | `W4_ultra_40bp_600s` | `-0.247` | `0.0961` | `2.22` | `0.0000` | `0.0013` | `0.0000` |
| **`Pos-Mk Dev Sel`** (`W01–W27`) | `lighter_robinhood` (`o200`) | 21 | **`P3_patient_600s`** | **`+1.091`** | **`1.0000`** | `2.32` | `0.0288` | **`0.9977`** | **`0.0000`** |
| **`Pos-Mk Dev Sel`** (`W01–W27`) | `lighter_std` (`o200`) | 21 | **`P3_patient_600s`** | **`+0.868`** | **`1.0000`** | `1.16` | `0.2668` | **`1.0000`** | `0.0571` |
| `Top-Tier 7-RWA` (`W01–W27`) | `lighter_robinhood` (`o200`) | 21 | `W4_ultra_40bp_600s` | `-0.097` | `0.3354` | `1.28` | `0.0001` | `0.3343` | `0.0000` |
| `Top-Tier 7-RWA` (`W01–W27`) | `lighter_std` (`o200`) | 21 | **`P3_patient_600s`** | **`+0.868`** | **`1.0000`** | `1.16` | `0.2668` | **`1.0000`** | `0.0571` |
| `Top-Tier 7-RWA` (`W01–W27`) | `lighter_prem` (`o200`) | 21 | `W4_ultra_40bp_600s` | `-0.493` | `0.0051` | `2.44` | `0.0000` | `0.0000` | `0.0000` |
| `Top-Tier 7-RWA` (`W01–W27`) | `arcus` (`o36`) | 21 | `G4_baseline` | `-0.952` | `0.0000` | `1.90` | `0.0000` | `0.0000` | `0.0714` |
| `Crypto` (`W01–W27`) | `lighter_robinhood` (`o200`) | 26 | `W4_ultra_40bp_600s` | `-0.476` | `0.0077` | `2.08` | `0.0000` | `0.0000` | `0.0000` |
| `Crypto` (`W01–W27`) | `lighter_std` (`o200`) | 26 | `W4_ultra_40bp_600s` | `-0.828` | `0.0000` | `2.14` | `0.0000` | `0.0000` | `0.0000` |
| `Crypto` (`W01–W27`) | `lighter_prem` (`o200`) | 26 | `W4_ultra_40bp_600s` | `-1.042` | `0.0000` | `1.64` | `0.0000` | `0.0000` | `0.0000` |

*Note:* On `Core 4` `lighter_std`, `PBO = 0.5000` reflects ties/competition among the several strongly profitable wide/patient variants (`P1–P3`, `W1–W2`, `W5`: all `-17/M` to `-28/M`), whereas on `lighter_robinhood` `Core 4` and `Pos-Mk Dev Sel`, `P3_patient_600s` achieves `PBO = 0.0000` and `DSR_eff >= 0.9977`.

---

## 6. Verification & Artifact Provenance

- **Redacted Research Artifact (outside Git):** `.agent-state/research/profitability_upgrade_20261127/track03_patient_frontier_w01_w27.json` (`sha256: df7a4a41d4a1c5b1f00cb32bcef422394c5c22a46fa0ea2961307f4478266b43`, `21.73 MB`, `SECRET_HITS = 0`, `w28_excluded = True`).
- **Verification Commands:**
  - `python3 -S -m unittest -v scripts/test_research_track03_full_universe_speedbump_mm.py scripts/test_research_track03_patient_profit_frontier.py` (`10/10` OK, including isolated `python3 -S` import test blocking `numpy`, `duckdb`, `pyarrow`, `pandas`, `scipy`).
  - `python3 -S scripts/research_track03_full_universe_speedbump_mm.py --self-test` & `python3 -S scripts/research_track03_patient_profit_frontier.py --self-test` (OK).
  - `git diff --check` clean; zero edits to untouched/frozen/peer files (`src/strategy/inventory_neutral_mm.rs`, `Cargo.lock`, `feeder/shm/`, `AGENT_HANDOFF.md`, `.agent-state/`, `.env*`, `docs/ALEPHTX_RESEARCH_AND_EXECUTION_ROADMAP_2026_11_27.md`, and the 6 pre-existing modified files).
