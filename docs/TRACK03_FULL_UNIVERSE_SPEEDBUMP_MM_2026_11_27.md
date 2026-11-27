# Track 03 (`03_incentive_turnover`): Full 18-Market Universe Discovery, Speed-Bump Optimal Quoting & Codex #140 Realism (`W01–W26`)

**Date:** `2026-11-27` (CST) · **Operational Mode:** Strictly **`HOLD`** (zero live orders/cancels, zero Tokyo deploys/restarts, zero `crate::exchanges::*` in `src/strategy/`, `rebate_bps = 0.0`, `-w27`/`-w28` excluded)

This report documents Paradigm Shift #3 for Track 03 (`QueuePriorityMmEngine`): (1) institutional speed-bump optimal quoting & toxicity controls in [queue_priority_mm.rs](../src/strategy/queue_priority_mm.rs), (2) Codex #140 & Gemini #140 execution realism fixes (displayed L1 depth clamping, book freshness gating `<= 5,000 ms`, `330 ms` `exit_delay_ms` on both intraday IOC exits and terminal end-of-window flatten, and explicit `realized_only` vs `mtm_incl_residual` accounting), (3) full 18-market `lighter_robinhood` (`USDG`, `0/0` verified) + 19-market `lighter` (`USDC`, `0/0` Standard counterfactual & `0.40/2.80` Premium sensitivity) discovery on `selection_dev (w01–w20)`, and (4) frozen cross-split ablation (`G4_baseline`, `G5_microprice_tfi`, `S1_latency_spread_floor`, `S2_cross_venue_fair_shift`, `S3_toxicity_circuit_breaker`, `S4_speedbump_combined`) across `selection_dev (w01–w20)`, untouched `holdout (w21–w25)`, and `w26_oos` via [research_track03_full_universe_speedbump_mm.py](../scripts/research_track03_full_universe_speedbump_mm.py) and [test_research_track03_full_universe_speedbump_mm.py](../scripts/test_research_track03_full_universe_speedbump_mm.py).

---

## 1. Executive Verdict

1. **Full 18/19-Market Discovery & `ex-NVDA` Sensitivity (`selection_dev w01–w20`, `G5_microprice_tfi`):**
   - Expanding beyond the legacy 5 RWA tickers uncovers **two distinct cost tiers**: mega-cap/index RWAs (`AAPL, AMZN, META, MSFT, QQQ, SPY, TSLA`) achieve low double-sided turnover cost (`META -$0.94/M` and `MSFT +$0.98/M` on `lighter_robinhood`; `AAPL -$4.83/M` and `AMZN -$0.29/M` on `lighter 0/0`), whereas volatile single-name tech/crypto-beta RWAs (`NVDA +$21.97/M`, `GOOGL +$15.43/M`, `MU +$17.62/M`, `AMD +$24.73/M`, `PLTR +$39.14/M`, `CRCL +$48.01/M`, `COIN +$83.16/M` on RH; `HOOD +$44.40/M`, `MSTR +$106.61/M` on Core) drive severe adverse selection.
   - **Explicit `ex-NVDA` sensitivity (`selection_dev`):** Removing `NVDA` cuts Legacy 5-RWA `G5` cost on `lighter_robinhood` (`o200_c300_e330`) from **`+9.17/M` -> `+7.71/M`** (`-15.9%`) and on `lighter` (`0/0`) from **`+4.98/M` -> `+4.13/M`** (`-17.1%` at `200ms`; **`+7.09/M` -> `+4.36/M`**, `-38.5%` at `300ms`).
   - **Causal Frozen `Top-Tier Low-Cost Universe` (`<= $10/M` on `selection_dev`, `>= 10w`, `ex-NVDA`):** Both venues independently select the exact same **7-symbol RWA cohort** (`AAPL, AMZN, META, MSFT, QQQ, SPY, TSLA`), cutting `selection_dev` `G5` cost at `(200, 300, 330)` to **`+7.01/M`** on `lighter_robinhood` (`USDG`, `$21.45M` single-sided) and **`+3.66/M`** on `lighter` (`USDC 0/0`, `$10.00M` single-sided), vs Full-RWA **`+10.09/M`** / **`+7.90/M`**.
2. **Speed-Bump Optimal Quoting & Frozen OOS Generalization (`S4_speedbump_combined` at `(200, 300, 330) ms`):**
   - **`lighter_robinhood` (`USDG`, `0/0` verified) `Top-Tier 7-RWA`:** `S4_speedbump_combined` lowers cost from `G4` **`+10.34/M` -> `+5.61/M`** (`selection_dev`, `-45.7%` vs `G4`, `-20.0%` vs `G5 +7.01/M`, `$0.00` residual), **`+11.69/M` -> `+7.37/M`** (`holdout`, `-37.0%` vs `G4`, `-26.2%` vs `G5 +9.98/M`, `$0.00` residual), and flips **`w26_oos`** net-profitable to **`-2.20/M` (`+$4.66` net)** vs `G4 +0.34/M`.
   - **`lighter` (`USDC`, `0/0` Standard counterfactual) `Top-Tier 7-RWA`:** `S4_speedbump_combined` reaches **`+1.56/M`** (`selection_dev`), **`+6.44/M`** (`holdout`), and **`-2.03/M`** (`+$1.98` net in `w26_oos`); under verified `0.40/2.80 bps` Premium fees (`OPT-01`), the same fills cost **`+95.63/M`** (`dev`), **`+105.53/M`** (`holdout`), and **`+99.03/M`** (`w26_oos`).
3. **Codex #140 Realism & Statistical Audit (`PSR / DSR / PBO`):**
   - Displayed L1 depth clamping clips thousands of thin-book IOC exits (`depth_clipped_exits > 0`), and `<= 5,000 ms` book freshness + `330 ms` terminal flatten leave non-zero residual inventory (`non_flat_count = 8` in Stage A, `51` in Stage B `dev`, `36` in `holdout`, `0` in `w26_oos`), with `max |realized_only - mtm_incl_residual| = $0.0112/M` (`holdout` crypto `S4`).
   - Across all `W=20` aligned RWA windows (`W01–W26`), `CSCV PBO` on `Top-Tier 7-RWA` is **`0.000`** (`lighter_robinhood o200`, `lighter o300`) to **`0.057`** (`lighter_robinhood o300`), confirming zero split overfitting, while **`PSR(0) <= 0.1015`** and **`DSR_eff <= 0.1015`** confirm aggregate 20-window unrebated PnL remains negative (`+$1.56/M` to `+$7.37/M` cost). **Verdict: Strictly `HOLD`** (`NO-GO` for unrebated profit; `Top-Tier 7-RWA` + `S4_speedbump_combined` is the validated minimal-cost turnover configuration under `0/0` Standard).

---

## 2. Rust Engine Upgrades (`src/strategy/queue_priority_mm.rs`) & Codex #140 Realism

Added three backward-compatible, default-off (`#[serde(default)]`, zero hot-path allocations, zero `crate::exchanges::*`, `G7`/`G8` `reduce_only_penny_ticks = 0` preserved) speed-bump controls to `QueuePriorityMmConfig` and `QueuePriorityMmEngine` in [queue_priority_mm.rs](../src/strategy/queue_priority_mm.rs):

| Mechanism | Config Fields (Default Off) | Frozen Stage B Spec | Invariant / Safety Guard |
|---|---|---|---|
| **Latency-Adjusted Optimal Spread Floor (`S1`)** | `speed_bump_cancel_ms = 0.0`, `latency_vol_spread_mult = 0.0` | `speed_bump_cancel_ms = cancel_delay_ms`, `latency_vol_spread_mult = 1.25` | Adds `mult * sigma_1s * sqrt(cancel_ms / 1000)` bps to `opening_req_half_bps` only; `reduce_only` exits exempt. |
| **Cross-Venue Fair Shift (`S2`)** | `cross_venue_fair_weight = 0.0`, `cross_venue_fair_cap_bps = 0.0` | `cross_venue_fair_weight = 0.75`, `cross_venue_fair_cap_bps = 4.0` | Causally shifts `fair_value` using validated reference consensus (`arcus`, `trade_xyz`, `binance`), clamped to `[-cap, +cap]` bps. |
| **Rolling Symbol Toxicity Circuit Breaker (`S3`)** | `toxicity_pause_ewma_bps = 0.0`, `toxicity_pause_ms = 0`, `toxicity_min_fills = 0` | `toxicity_pause_ewma_bps = 1.50`, `toxicity_pause_ms = 60_000`, `toxicity_min_fills = 6` | Tracks symbol-level matured fill markout EWMA (`0.80/0.20`); cancels resting openings (`toxicity_circuit_breaker`) & blocks new openings while keeping `reduce_only` exits active. |
| **NaN/Inf Fail-Closed Guard** | Always active | Always active | Cancels all resting quotes (`invalid_inventory_state` / `stale_or_invalid_target_book` / `stale_reference_book`) on any non-finite input. |

**Codex #140 & Gemini #140 Realism in [research_track03_full_universe_speedbump_mm.py](../scripts/research_track03_full_universe_speedbump_mm.py):**
- Enforces displayed L1 depth clamping (`cur_bsz` / `cur_asz`), decrementing consumed L1 depth until the next book refresh and counting `depth_clipped_exits`.
- Enforces `<= 5,000 ms` book freshness (`book_is_fresh`) for order activation, quote placement/retention, intraday IOC fills, and terminal drain.
- Applies `330 ms` `exit_delay_ms` to both intraday IOC exits and terminal end-of-window drain (`terminal_flatten_lead_ms = 1,000 ms`), never flattening instantaneously at `last_ts`.
- Reports `terminal_residual_notional_usd`, `depth_clipped_exits`, `realized_only_usd_per_m`, and `mtm_incl_residual_usd_per_m` explicitly (`max_mtm_cash_err <= 8.46e-12`).

---

## 3. Stage A: Full 18/19-Market League Table (`selection_dev w01–w20`, `G5_microprice_tfi`)

Note: `SNDK` (`lighter_robinhood: 32`, `lighter: 139`) appears in `6` early windows (`w01–w07`); all other `17` `lighter_robinhood` and `18` `lighter` markets appear in all `15` RWA / `19` Crypto `selection_dev` windows. `Top-Tier Low-Cost Universe` rule frozen strictly on `selection_dev`: RWA symbol with `windows >= 10`, `symbol != NVDA`, and `(200, 300, 330)` `mtm_incl_residual_usd_per_m <= +$10.00/M`.

### 3.1 `lighter_robinhood` (`USDG`, `0/0` `verified_account_tier`, 18 Markets)

| Domain | Symbol | `W` | `o200` `ntl` | `o200` Real `$/M` | `o200` MTM `$/M` | `o200` `mk1s` | `o200` `cif` | `o200` `s` | `o200` Clips | `o200` Resid ($) | `o300` MTM `$/M` | `o300` `s` | `Top-Tier` |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| rwa | `META` | 15 | `$0.84M` | `-0.94` | `-0.94` | `+0.399` | `0.281` | `0.548` | `741` | `$0.00` | `+0.59` | `0.558` | **YES** |
| rwa | `MSFT` | 15 | `$1.26M` | `+0.98` | `+0.98` | `+0.219` | `0.235` | `0.485` | `2406` | `$0.00` | `+1.63` | `0.494` | **YES** |
| rwa | `TSLA` | 15 | `$1.54M` | `+5.49` | `+5.49` | `+0.098` | `0.233` | `0.398` | `1337` | `$0.00` | `+6.54` | `0.418` | **YES** |
| rwa | `AAPL` | 15 | `$2.06M` | `+6.32` | `+6.32` | `+0.012` | `0.227` | `0.389` | `1479` | `$0.00` | `+6.32` | `0.405` | **YES** |
| rwa | `SPY` | 15 | `$8.94M` | `+7.98` | `+7.98` | `-0.036` | `0.259` | `0.150` | `3734` | `$0.00` | `+8.21` | `0.168` | **YES** |
| rwa | `QQQ` | 15 | `$5.80M` | `+8.37` | `+8.37` | `-0.019` | `0.274` | `0.244` | `3231` | `$0.00` | `+8.62` | `0.270` | **YES** |
| rwa | `AMZN` | 15 | `$1.02M` | `+8.47` | `+8.47` | `+0.210` | `0.235` | `0.537` | `807` | `$0.00` | `+9.14` | `0.548` | **YES** |
| rwa | `SNDK` | 6 | `$0.15M` | `+10.93` | `+10.93` | `+0.282` | `0.362` | `0.531` | `58` | `$0.00` | `+11.12` | `0.556` | No |
| rwa | `GOOGL` | 15 | `$1.98M` | `+15.43` | `+15.43` | `-0.032` | `0.259` | `0.375` | `2144` | `$52.33` | `+15.84` | `0.398` | No |
| rwa | `MU` | 15 | `$0.33M` | `+17.62` | `+17.62` | `+0.295` | `0.327` | `0.608` | `253` | `$0.00` | `+21.52` | `0.629` | No |
| rwa | `NVDA` | 15 | `$2.10M` | `+21.97` | `+21.97` | `-0.073` | `0.276` | `0.284` | `818` | `$190.88` | `+20.44` | `0.299` | No |
| rwa | `AMD` | 15 | `$0.36M` | `+24.73` | `+24.73` | `+0.242` | `0.313` | `0.658` | `182` | `$0.00` | `+24.91` | `0.674` | No |
| rwa | `PLTR` | 15 | `$0.14M` | `+39.14` | `+39.14` | `+0.868` | `0.332` | `0.890` | `164` | `$0.00` | `+41.98` | `0.897` | No |
| rwa | `CRCL` | 15 | `$0.21M` | `+48.01` | `+48.01` | `+0.040` | `0.448` | `0.757` | `249` | `$0.00` | `+49.18` | `0.779` | No |
| rwa | `COIN` | 15 | `$0.15M` | `+83.16` | `+83.16` | `+0.351` | `0.291` | `0.849` | `318` | `$0.00` | `+79.58` | `0.857` | No |
| crypto | `BTC` | 19 | `$2.48M` | `+22.98` | `+22.98` | `-0.043` | `0.554` | `0.366` | `814` | `$0.00` | `+24.06` | `0.420` | Excl (Crypto) |
| crypto | `ETH` | 19 | `$1.00M` | `+29.20` | `+29.20` | `-0.034` | `0.593` | `0.586` | `481` | `$0.00` | `+31.80` | `0.627` | Excl (Crypto) |
| crypto | `SOL` | 19 | `$0.23M` | `+41.55` | `+41.55` | `-0.097` | `0.505` | `0.770` | `386` | `$0.00` | `+42.32` | `0.788` | Excl (Crypto) |

### 3.2 `lighter` (`USDC`, `0/0` `lighter_standard_zero_scenario`, 19 Markets)

| Domain | Symbol | `W` | `o200` `ntl` | `o200` Real `$/M` | `o200` MTM `$/M` | `o200` `mk1s` | `o200` `cif` | `o200` `s` | `o200` Clips | `o200` Resid ($) | `o300` MTM `$/M` | `o300` `s` | `Top-Tier` |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| rwa | `AAPL` | 15 | `$1.87M` | `-4.83` | `-4.83` | `+0.203` | `0.301` | `0.374` | `1738` | `$0.00` | `-3.83` | `0.398` | **YES** |
| rwa | `AMZN` | 15 | `$0.77M` | `-0.29` | `-0.29` | `+0.161` | `0.332` | `0.573` | `1198` | `$0.00` | `+0.25` | `0.591` | **YES** |
| rwa | `MSFT` | 15 | `$0.52M` | `+3.13` | `+3.14` | `+0.305` | `0.231` | `0.553` | `171` | `$106.26` | `+3.47` | `0.560` | **YES** |
| rwa | `META` | 15 | `$0.80M` | `+3.19` | `+3.19` | `+0.209` | `0.259` | `0.504` | `424` | `$0.00` | `+3.27` | `0.524` | **YES** |
| rwa | `QQQ` | 15 | `$2.50M` | `+5.70` | `+5.70` | `+0.099` | `0.327` | `0.435` | `1241` | `$0.00` | `+6.02` | `0.465` | **YES** |
| rwa | `SPY` | 15 | `$2.56M` | `+7.31` | `+7.31` | `+0.044` | `0.296` | `0.434` | `1640` | `$0.00` | `+7.06` | `0.459` | **YES** |
| rwa | `TSLA` | 15 | `$0.98M` | `+8.90` | `+8.90` | `+0.057` | `0.287` | `0.503` | `559` | `$0.00` | `+8.36` | `0.512` | **YES** |
| rwa | `NVDA` | 15 | `$1.52M` | `+9.45` | `+9.45` | `-0.032` | `0.447` | `0.272` | `377` | `$0.00` | `+22.61` | `0.313` | No |
| rwa | `SNDK` | 6 | `$0.30M` | `+19.62` | `+19.62` | `-0.084` | `0.391` | `0.336` | `79` | `$0.00` | `+15.79` | `0.378` | No |
| rwa | `GOOGL` | 15 | `$0.82M` | `+23.64` | `+23.64` | `-0.047` | `0.338` | `0.569` | `458` | `$0.00` | `+23.97` | `0.575` | No |
| rwa | `PLTR` | 15 | `$0.11M` | `+31.91` | `+31.91` | `+0.282` | `0.473` | `0.819` | `400` | `$0.00` | `+31.63` | `0.826` | No |
| rwa | `COIN` | 15 | `$0.15M` | `+35.86` | `+35.86` | `+0.165` | `0.499` | `0.807` | `144` | `$0.00` | `+35.99` | `0.832` | No |
| rwa | `AMD` | 15 | `$0.19M` | `+37.20` | `+37.20` | `+0.039` | `0.429` | `0.712` | `149` | `$0.00` | `+43.03` | `0.724` | No |
| rwa | `HOOD` | 15 | `$78,775` | `+44.40` | `+44.40` | `+0.060` | `0.463` | `0.739` | `176` | `$0.00` | `+41.37` | `0.748` | No |
| rwa | `MU` | 15 | `$0.13M` | `+49.26` | `+49.26` | `-0.208` | `0.487` | `0.681` | `90` | `$0.00` | `+37.32` | `0.688` | No |
| rwa | `MSTR` | 15 | `$32,052` | `+106.61` | `+106.61` | `-1.039` | `0.561` | `0.891` | `17` | `$0.00` | `+109.37` | `0.894` | No |
| crypto | `BTC` | 19 | `$4.81M` | `+16.83` | `+16.83` | `-0.130` | `0.632` | `0.048` | `271` | `$0.00` | `+15.57` | `0.088` | Excl (Crypto) |
| crypto | `ETH` | 19 | `$2.73M` | `+19.96` | `+19.96` | `-0.169` | `0.573` | `0.111` | `293` | `$5.08` | `+20.01` | `0.154` | Excl (Crypto) |
| crypto | `SOL` | 19 | `$0.42M` | `+34.98` | `+34.98` | `-0.341` | `0.608` | `0.370` | `204` | `$0.00` | `+27.88` | `0.416` | Excl (Crypto) |

### 3.3 `ex-NVDA` & Universe Cohort Sensitivity (`selection_dev`, `G5_microprice_tfi`)

| Cohort | Venue (`Sett.`) | Lat. (`o_c_e`) | `ntl` | Real `$/M` | MTM `$/M` | Prem `0.40/2.80` `$/M` | `s` | `cif` | Clips | Resid ($) | Net ($) |
|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| Legacy 5-RWA (`incl-NVDA`) | `lighter_robinhood` (`USDG`) | `o200_c300_e330` | `$20.43M` | `+9.17` | `+9.17` | `+77.14` | `0.233` | `0.260` | `10599` | `$190.88` | `$-374.75` |
| Legacy 5-RWA (`incl-NVDA`) | `lighter_robinhood` (`USDG`) | `o300_c300_e330` | `$18.88M` | `+9.28` | `+9.28` | `+79.84` | `0.255` | `0.261` | `10557` | `$286.38` | `$-350.53` |
| Legacy 5-RWA (`incl-NVDA`) | `lighter` (`USDC`) | `o200_c300_e330` | `$9.42M` | `+4.98` | `+4.98` | `+93.40` | `0.403` | `0.330` | `5555` | `$0.00` | `$-93.87` |
| Legacy 5-RWA (`incl-NVDA`) | `lighter` (`USDC`) | `o300_c300_e330` | `$8.70M` | `+7.09` | `+7.09` | `+98.95` | `0.432` | `0.329` | `5630` | `$0.00` | `$-123.29` |
| Legacy 5-RWA (`ex-NVDA`) | `lighter_robinhood` (`USDG`) | `o200_c300_e330` | `$18.33M` | `+7.71` | `+7.71` | `+74.97` | `0.227` | `0.258` | `9781` | `$0.00` | `$-282.65` |
| Legacy 5-RWA (`ex-NVDA`) | `lighter_robinhood` (`USDG`) | `o300_c300_e330` | `$16.89M` | `+7.97` | `+7.97` | `+77.90` | `0.249` | `0.259` | `9739` | `$95.49` | `$-269.19` |
| Legacy 5-RWA (`ex-NVDA`) | `lighter` (`USDC`) | `o200_c300_e330` | `$7.90M` | `+4.13` | `+4.13` | `+95.57` | `0.429` | `0.306` | `5178` | `$0.00` | `$-65.21` |
| Legacy 5-RWA (`ex-NVDA`) | `lighter` (`USDC`) | `o300_c300_e330` | `$7.39M` | `+4.36` | `+4.36` | `+98.74` | `0.453` | `0.305` | `5200` | `$0.00` | `$-64.44` |
| Frozen `Top-Tier 7-RWA` | `lighter_robinhood` (`USDG`) | `o200_c300_e330` | `$21.45M` | `+7.01` | `+7.01` | `+79.37` | `0.270` | `0.257` | `13735` | `$0.00` | `$-300.91` |
| Frozen `Top-Tier 7-RWA` | `lighter_robinhood` (`USDG`) | `o300_c300_e330` | `$19.91M` | `+7.35` | `+7.35` | `+82.38` | `0.292` | `0.257` | `13799` | `$95.49` | `$-292.64` |
| Frozen `Top-Tier 7-RWA` | `lighter` (`USDC`) | `o200_c300_e330` | `$10.00M` | `+3.66` | `+3.66` | `+97.94` | `0.452` | `0.300` | `6971` | `$106.26` | `$-73.16` |
| Frozen `Top-Tier 7-RWA` | `lighter` (`USDC`) | `o300_c300_e330` | `$9.37M` | `+3.91` | `+3.91` | `+100.92` | `0.475` | `0.300` | `6949` | `$70.91` | `$-73.28` |
| Full RWA (`15/16 mkts`) | `lighter_robinhood` (`USDG`) | `o200_c300_e330` | `$26.86M` | `+10.09` | `+10.09` | `+86.02` | `0.299` | `0.262` | `17921` | `$243.21` | `$-541.88` |
| Full RWA (`15/16 mkts`) | `lighter_robinhood` (`USDG`) | `o300_c300_e330` | `$25.00M` | `+10.38` | `+10.38` | `+88.92` | `0.321` | `0.262` | `17843` | `$286.38` | `$-518.87` |
| Full RWA (`15/16 mkts`) | `lighter` (`USDC`) | `o200_c300_e330` | `$13.31M` | `+7.90` | `+7.90` | `+102.13` | `0.452` | `0.330` | `8861` | `$106.26` | `$-210.28` |
| Full RWA (`15/16 mkts`) | `lighter` (`USDC`) | `o300_c300_e330` | `$12.34M` | `+9.31` | `+9.31` | `+106.54` | `0.477` | `0.328` | `8826` | `$70.91` | `$-229.72` |
| Full RWA (`ex-NVDA`) | `lighter_robinhood` (`USDG`) | `o200_c300_e330` | `$24.76M` | `+9.08` | `+9.08` | `+85.16` | `0.301` | `0.260` | `17103` | `$52.33` | `$-449.79` |
| Full RWA (`ex-NVDA`) | `lighter_robinhood` (`USDG`) | `o300_c300_e330` | `$23.01M` | `+9.51` | `+9.51` | `+88.28` | `0.323` | `0.261` | `17025` | `$95.49` | `$-437.52` |
| Full RWA (`ex-NVDA`) | `lighter` (`USDC`) | `o200_c300_e330` | `$11.80M` | `+7.70` | `+7.70` | `+104.71` | `0.475` | `0.313` | `8484` | `$106.26` | `$-181.63` |
| Full RWA (`ex-NVDA`) | `lighter` (`USDC`) | `o300_c300_e330` | `$11.04M` | `+7.74` | `+7.74` | `+107.29` | `0.496` | `0.312` | `8396` | `$70.91` | `$-170.87` |
| Crypto (`BTC, ETH, SOL`) | `lighter_robinhood` (`USDG`) | `o200_c300_e330` | `$3.71M` | `+25.81` | `+25.81` | `+119.85` | `0.450` | `0.561` | `1681` | `$0.00` | `$-191.56` |
| Crypto (`BTC, ETH, SOL`) | `lighter_robinhood` (`USDG`) | `o300_c300_e330` | `$3.11M` | `+27.48` | `+27.48` | `+127.89` | `0.503` | `0.565` | `1510` | `$0.00` | `$-170.87` |
| Crypto (`BTC, ETH, SOL`) | `lighter` (`USDC`) | `o200_c300_e330` | `$7.95M` | `+18.86` | `+18.86` | `+69.28` | `0.087` | `0.611` | `768` | `$5.08` | `$-299.88` |
| Crypto (`BTC, ETH, SOL`) | `lighter` (`USDC`) | `o300_c300_e330` | `$6.21M` | `+17.79` | `+17.79` | `+73.31` | `0.129` | `0.612` | `938` | `$5.08` | `$-220.89` |
| Full Universe (`18/19 mkts`) | `lighter_robinhood` (`USDG`) | `o200_c300_e330` | `$30.57M` | `+12.00` | `+12.00` | `+90.12` | `0.318` | `0.295` | `19602` | `$243.21` | `$-733.44` |
| Full Universe (`18/19 mkts`) | `lighter_robinhood` (`USDG`) | `o300_c300_e330` | `$28.11M` | `+12.27` | `+12.27` | `+93.23` | `0.341` | `0.292` | `19353` | `$286.38` | `$-689.74` |
| Full Universe (`18/19 mkts`) | `lighter` (`USDC`) | `o200_c300_e330` | `$21.26M` | `+12.00` | `+12.00` | `+89.85` | `0.315` | `0.450` | `9629` | `$111.34` | `$-510.16` |
| Full Universe (`18/19 mkts`) | `lighter` (`USDC`) | `o300_c300_e330` | `$18.55M` | `+12.15` | `+12.15` | `+95.42` | `0.361` | `0.437` | `9764` | `$76.00` | `$-450.61` |

---

## 4. Stage B: Speed-Bump Quoting Ablation (`selection_dev` → Frozen `holdout` → `w26_oos`)

### 4.1 Frozen `Top-Tier 7-RWA` (`AAPL, AMZN, META, MSFT, QQQ, SPY, TSLA`)

| Split | Venue (`Sett.`) | Lat. (`o_c_e`) | `G4_base` `$/M` | `G5_mic` `$/M` | `S1_lat` `$/M` | `S2_fair` `$/M` | `S3_tox` `$/M` | `S4_comb` MTM `$/M` | `S4` Real `$/M` | `S4` `ntl` | `S4` `s` | `S4` Resid ($) |
|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `selection_dev` | `lighter_robinhood` (`USDG`) | `o200_c300_e330` | `+10.34` | `+7.01` | `+6.71` | `+6.37` | `+7.01` | **+5.61** | `+5.61` | `$17.84M` | `0.258` | `$0.00` |
| `selection_dev` | `lighter_robinhood` (`USDG`) | `o300_c300_e330` | `+10.67` | `+7.35` | `+6.78` | `+6.64` | `+7.24` | **+5.91** | `+5.91` | `$16.53M` | `0.281` | `$0.00` |
| `selection_dev` | `lighter` (`USDC`) | `o200_c300_e330` | `+5.63` | `+3.66` | `+1.48` | `+3.14` | `+3.53` | **+1.56** | `+1.56` | `$8.31M` | `0.451` | `$5.95` |
| `selection_dev` | `lighter` (`USDC`) | `o300_c300_e330` | `+6.30` | `+3.91` | `+1.83` | `+3.96` | `+3.84` | **+2.20** | `+2.20` | `$7.82M` | `0.474` | `$185.53` |
| `holdout` | `lighter_robinhood` (`USDG`) | `o200_c300_e330` | `+11.69` | `+9.98` | `+9.21` | `+9.07` | `+9.85` | **+7.37** | `+7.37` | `$4.25M` | `0.273` | `$0.00` |
| `holdout` | `lighter_robinhood` (`USDG`) | `o300_c300_e330` | `+12.19` | `+10.19` | `+9.88` | `+9.35` | `+10.07` | **+8.30** | `+8.30` | `$3.90M` | `0.298` | `$0.00` |
| `holdout` | `lighter` (`USDC`) | `o200_c300_e330` | `+11.76` | `+10.16` | `+8.43` | `+10.29` | `+10.00` | **+6.44** | `+6.43` | `$1.74M` | `0.492` | `$248.48` |
| `holdout` | `lighter` (`USDC`) | `o300_c300_e330` | `+12.30` | `+10.94` | `+8.71` | `+10.53` | `+10.56` | **+7.22** | `+7.21` | `$1.60M` | `0.517` | `$248.48` |
| `w26_oos` | `lighter_robinhood` (`USDG`) | `o200_c300_e330` | `+0.34` | `-1.36` | `-2.17` | `-1.36` | `-1.29` | **-2.20** | `-2.20` | `$1.06M` | `0.330` | `$0.00` |
| `w26_oos` | `lighter_robinhood` (`USDG`) | `o300_c300_e330` | `+1.19` | `-0.95` | `-2.21` | `-0.96` | `-0.80` | **-1.85** | `-1.85` | `$0.99M` | `0.346` | `$0.00` |
| `w26_oos` | `lighter` (`USDC`) | `o200_c300_e330` | `+2.32` | `+0.54` | `-2.10` | `-0.76` | `+0.58` | **-2.03** | `-2.03` | `$0.49M` | `0.509` | `$0.00` |
| `w26_oos` | `lighter` (`USDC`) | `o300_c300_e330` | `+3.08` | `+1.94` | `-0.51` | `+0.82` | `+1.94` | **-0.91** | `-0.91` | `$0.47M` | `0.527` | `$0.00` |

### 4.2 Legacy 5-RWA (`incl-NVDA`: `AAPL, SPY, QQQ, NVDA, TSLA`)

| Split | Venue (`Sett.`) | Lat. (`o_c_e`) | `G4_base` `$/M` | `G5_mic` `$/M` | `S1_lat` `$/M` | `S2_fair` `$/M` | `S3_tox` `$/M` | `S4_comb` MTM `$/M` | `S4` Real `$/M` | `S4` `ntl` | `S4` `s` | `S4` Resid ($) |
|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `selection_dev` | `lighter_robinhood` (`USDG`) | `o200_c300_e330` | `+11.33` | `+9.17` | `+8.81` | `+8.36` | `+9.03` | **+7.50** | `+7.50` | `$16.71M` | `0.219` | `$107.04` |
| `selection_dev` | `lighter_robinhood` (`USDG`) | `o300_c300_e330` | `+11.94` | `+9.28` | `+8.30` | `+8.56` | `+9.26` | **+7.92** | `+7.92` | `$15.41M` | `0.240` | `$107.04` |
| `selection_dev` | `lighter` (`USDC`) | `o200_c300_e330` | `+6.01` | `+4.98` | `+3.20` | `+4.59` | `+4.82` | **+2.85** | `+2.85` | `$7.68M` | `0.405` | `$0.00` |
| `selection_dev` | `lighter` (`USDC`) | `o300_c300_e330` | `+7.08` | `+7.09` | `+5.09` | `+6.57` | `+6.98` | **+4.75** | `+4.75` | `$7.12M` | `0.434` | `$0.00` |
| `holdout` | `lighter_robinhood` (`USDG`) | `o200_c300_e330` | `+13.52` | `+12.71` | `+11.51` | `+11.07` | `+12.49` | **+9.38** | `+9.38` | `$3.85M` | `0.225` | `$0.00` |
| `holdout` | `lighter_robinhood` (`USDG`) | `o300_c300_e330` | `+13.63` | `+12.56` | `+11.94` | `+11.55` | `+12.32` | **+10.42** | `+10.42` | `$3.52M` | `0.251` | `$0.00` |
| `holdout` | `lighter` (`USDC`) | `o200_c300_e330` | `+13.41` | `+12.54` | `+10.12` | `+12.22` | `+11.99` | **+8.66** | `+8.66` | `$1.48M` | `0.468` | `$248.48` |
| `holdout` | `lighter` (`USDC`) | `o300_c300_e330` | `+14.38` | `+13.33` | `+11.25` | `+12.45` | `+12.73` | **+9.26** | `+9.25` | `$1.34M` | `0.499` | `$248.48` |
| `w26_oos` | `lighter_robinhood` (`USDG`) | `o200_c300_e330` | `+2.59` | `+2.44` | `+0.47` | `+3.41` | `+2.44` | **+1.21** | `+1.21` | `$0.98M` | `0.280` | `$0.00` |
| `w26_oos` | `lighter_robinhood` (`USDG`) | `o300_c300_e330` | `+2.76` | `+2.38` | `+0.52` | `+3.71` | `+2.83` | **+1.37** | `+1.37` | `$0.91M` | `0.297` | `$0.00` |
| `w26_oos` | `lighter` (`USDC`) | `o200_c300_e330` | `+4.55` | `+3.62` | `+1.46` | `+3.91` | `+3.91` | **+2.80** | `+2.80` | `$0.43M` | `0.457` | `$0.00` |
| `w26_oos` | `lighter` (`USDC`) | `o300_c300_e330` | `+4.63` | `+5.92` | `+3.27` | `+4.72` | `+6.06` | **+3.27** | `+3.27` | `$0.40M` | `0.489` | `$0.00` |

### 4.3 Legacy 5-RWA (`ex-NVDA`: `AAPL, SPY, QQQ, TSLA`)

| Split | Venue (`Sett.`) | Lat. (`o_c_e`) | `G4_base` `$/M` | `G5_mic` `$/M` | `S1_lat` `$/M` | `S2_fair` `$/M` | `S3_tox` `$/M` | `S4_comb` MTM `$/M` | `S4` Real `$/M` | `S4` `ntl` | `S4` `s` | `S4` Resid ($) |
|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `selection_dev` | `lighter_robinhood` (`USDG`) | `o200_c300_e330` | `+9.85` | `+7.71` | `+7.62` | `+6.71` | `+7.68` | **+6.17** | `+6.17` | `$15.12M` | `0.212` | `$0.00` |
| `selection_dev` | `lighter_robinhood` (`USDG`) | `o300_c300_e330` | `+10.13` | `+7.97` | `+7.71` | `+6.94` | `+7.87` | **+6.49** | `+6.49` | `$13.88M` | `0.233` | `$0.00` |
| `selection_dev` | `lighter` (`USDC`) | `o200_c300_e330` | `+5.20` | `+4.13` | `+2.19` | `+3.58` | `+3.95` | **+1.85** | `+1.85` | `$6.50M` | `0.427` | `$0.00` |
| `selection_dev` | `lighter` (`USDC`) | `o300_c300_e330` | `+5.87` | `+4.36` | `+2.43` | `+4.37` | `+4.28` | **+2.55** | `+2.55` | `$6.09M` | `0.453` | `$0.00` |
| `holdout` | `lighter_robinhood` (`USDG`) | `o200_c300_e330` | `+11.82` | `+10.98` | `+10.51` | `+9.62` | `+10.95` | **+8.14** | `+8.14` | `$3.57M` | `0.218` | `$0.00` |
| `holdout` | `lighter_robinhood` (`USDG`) | `o300_c300_e330` | `+12.17` | `+11.01` | `+11.23` | `+10.07` | `+11.00` | **+9.41** | `+9.41` | `$3.24M` | `0.243` | `$0.00` |
| `holdout` | `lighter` (`USDC`) | `o200_c300_e330` | `+11.79` | `+10.91` | `+9.45` | `+10.68` | `+10.63` | **+6.80** | `+6.79` | `$1.33M` | `0.479` | `$248.48` |
| `holdout` | `lighter` (`USDC`) | `o300_c300_e330` | `+12.27` | `+11.63` | `+9.62` | `+11.09` | `+11.22` | **+7.86** | `+7.86` | `$1.21M` | `0.504` | `$248.48` |
| `w26_oos` | `lighter_robinhood` (`USDG`) | `o200_c300_e330` | `+1.64` | `+1.16` | `+0.45` | `+0.83` | `+1.19` | **+0.09** | `+0.09` | `$0.85M` | `0.277` | `$0.00` |
| `w26_oos` | `lighter_robinhood` (`USDG`) | `o300_c300_e330` | `+2.32` | `+1.54` | `+0.41` | `+1.29` | `+1.73` | **+0.25** | `+0.25` | `$0.80M` | `0.291` | `$0.00` |
| `w26_oos` | `lighter` (`USDC`) | `o200_c300_e330` | `+2.87` | `+2.69` | `-0.47` | `+0.52` | `+2.76` | **-0.31** | `-0.31` | `$0.35M` | `0.492` | `$0.00` |
| `w26_oos` | `lighter` (`USDC`) | `o300_c300_e330` | `+3.75` | `+4.32` | `+0.95` | `+2.64` | `+4.32` | **+1.13** | `+1.13` | `$0.34M` | `0.513` | `$0.00` |

### 4.4 Full-Universe RWA (`15` RH / `16` Core Markets)

| Split | Venue (`Sett.`) | Lat. (`o_c_e`) | `G4_base` `$/M` | `G5_mic` `$/M` | `S1_lat` `$/M` | `S2_fair` `$/M` | `S3_tox` `$/M` | `S4_comb` MTM `$/M` | `S4` Real `$/M` | `S4` `ntl` | `S4` `s` | `S4` Resid ($) |
|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `selection_dev` | `lighter_robinhood` (`USDG`) | `o200_c300_e330` | `+14.64` | `+10.09` | `+9.43` | `+9.78` | `+9.92` | **+8.66** | `+8.66` | `$22.28M` | `0.290` | `$193.90` |
| `selection_dev` | `lighter_robinhood` (`USDG`) | `o300_c300_e330` | `+15.36` | `+10.38` | `+9.14` | `+10.19` | `+10.26` | **+9.14** | `+9.14` | `$20.75M` | `0.313` | `$107.04` |
| `selection_dev` | `lighter` (`USDC`) | `o200_c300_e330` | `+10.21` | `+7.90` | `+5.78` | `+7.70` | `+7.76` | **+6.16** | `+6.16` | `$11.06M` | `0.454` | `$5.95` |
| `selection_dev` | `lighter` (`USDC`) | `o300_c300_e330` | `+11.15` | `+9.31` | `+7.25` | `+9.37` | `+9.12` | **+7.65** | `+7.65` | `$10.31M` | `0.479` | `$185.53` |
| `holdout` | `lighter_robinhood` (`USDG`) | `o200_c300_e330` | `+15.73` | `+13.62` | `+12.25` | `+12.82` | `+13.08` | **+11.03** | `+11.03` | `$5.08M` | `0.306` | `$0.00` |
| `holdout` | `lighter_robinhood` (`USDG`) | `o300_c300_e330` | `+16.00` | `+13.84` | `+12.74` | `+13.36` | `+13.40` | **+12.10** | `+12.10` | `$4.67M` | `0.332` | `$0.00` |
| `holdout` | `lighter` (`USDC`) | `o200_c300_e330` | `+17.10` | `+14.82` | `+13.65` | `+15.66` | `+14.54` | **+12.88** | `+12.88` | `$2.16M` | `0.504` | `$248.48` |
| `holdout` | `lighter` (`USDC`) | `o300_c300_e330` | `+17.88` | `+15.64` | `+14.30` | `+15.72` | `+15.15` | **+13.02** | `+13.01` | `$1.98M` | `0.532` | `$248.48` |
| `w26_oos` | `lighter_robinhood` (`USDG`) | `o200_c300_e330` | `+4.82` | `+1.67` | `-0.31` | `+3.51` | `+1.81` | **+1.90** | `+1.90` | `$1.35M` | `0.365` | `$0.00` |
| `w26_oos` | `lighter_robinhood` (`USDG`) | `o300_c300_e330` | `+5.21` | `+2.01` | `+0.34` | `+3.67` | `+2.40` | **+2.14** | `+2.14` | `$1.28M` | `0.383` | `$0.00` |
| `w26_oos` | `lighter` (`USDC`) | `o200_c300_e330` | `+9.67` | `+6.48` | `+4.30` | `+6.63` | `+6.75` | **+5.92** | `+5.92` | `$0.66M` | `0.504` | `$0.00` |
| `w26_oos` | `lighter` (`USDC`) | `o300_c300_e330` | `+10.23` | `+9.29` | `+6.54` | `+8.22` | `+9.40` | **+6.59** | `+6.59` | `$0.62M` | `0.528` | `$0.00` |

### 4.5 Full 18/19-Market Universe (`Crypto + RWA`)

| Split | Venue (`Sett.`) | Lat. (`o_c_e`) | `G4_base` `$/M` | `G5_mic` `$/M` | `S1_lat` `$/M` | `S2_fair` `$/M` | `S3_tox` `$/M` | `S4_comb` MTM `$/M` | `S4` Real `$/M` | `S4` `ntl` | `S4` `s` | `S4` Resid ($) |
|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `selection_dev` | `lighter_robinhood` (`USDG`) | `o200_c300_e330` | `+16.55` | `+12.00` | `+10.77` | `+12.02` | `+11.81` | **+10.75** | `+10.75` | `$25.04M` | `0.311` | `$193.90` |
| `selection_dev` | `lighter_robinhood` (`USDG`) | `o300_c300_e330` | `+17.35` | `+12.27` | `+10.57` | `+12.32` | `+12.21` | **+11.10** | `+11.10` | `$23.08M` | `0.334` | `$107.04` |
| `selection_dev` | `lighter` (`USDC`) | `o200_c300_e330` | `+13.53` | `+12.01` | `+9.72` | `+12.20` | `+11.75` | **+11.04** | `+11.04` | `$18.78M` | `0.329` | `$5.95` |
| `selection_dev` | `lighter` (`USDC`) | `o300_c300_e330` | `+13.27` | `+12.14` | `+10.20` | `+12.29` | `+12.02` | **+11.02** | `+11.02` | `$16.64M` | `0.374` | `$185.53` |
| `holdout` | `lighter_robinhood` (`USDG`) | `o200_c300_e330` | `+18.35` | `+16.05` | `+13.76` | `+16.37` | `+15.51` | **+14.09** | `+14.09` | `$5.83M` | `0.332` | `$122.42` |
| `holdout` | `lighter_robinhood` (`USDG`) | `o300_c300_e330` | `+18.31` | `+15.69` | `+14.01` | `+16.08` | `+15.48` | **+15.14** | `+15.14` | `$5.31M` | `0.358` | `$0.00` |
| `holdout` | `lighter` (`USDC`) | `o200_c300_e330` | `+18.25` | `+16.53` | `+15.99` | `+17.62` | `+16.21` | **+16.25** | `+16.25` | `$4.19M` | `0.344` | `$488.15` |
| `holdout` | `lighter` (`USDC`) | `o300_c300_e330` | `+17.58` | `+16.58` | `+15.63` | `+16.51` | `+16.41` | **+15.80** | `+15.80` | `$3.58M` | `0.402` | `$487.55` |
| `w26_oos` | `lighter_robinhood` (`USDG`) | `o200_c300_e330` | `+9.11` | `+5.02` | `+2.54` | `+8.04` | `+5.28` | **+6.05** | `+6.05` | `$1.53M` | `0.382` | `$0.00` |
| `w26_oos` | `lighter_robinhood` (`USDG`) | `o300_c300_e330` | `+8.75` | `+5.26` | `+2.92` | `+7.71` | `+5.55` | **+6.43** | `+6.43` | `$1.43M` | `0.400` | `$0.00` |
| `w26_oos` | `lighter` (`USDC`) | `o200_c300_e330` | `+10.32` | `+9.81` | `+8.60` | `+10.04` | `+9.99` | **+9.55** | `+9.55` | `$1.36M` | `0.334` | `$0.00` |
| `w26_oos` | `lighter` (`USDC`) | `o300_c300_e330` | `+11.28` | `+11.73` | `+10.65` | `+11.00` | `+11.56` | **+10.37** | `+10.37` | `$1.19M` | `0.386` | `$0.00` |

---

## 5. Multiple-Testing Audit (`PSR / DSR / CSCV PBO` across `W01–W26`)

Aligned across all `W=20` RWA windows (`W01–W26`, `num_blocks = 8`, `70` CSCV splits) over the 6 candidate arms (`G4`, `G5`, `S1`, `S2`, `S3`, `S4`):

| Cohort | Venue | Lat. | `W` | Best Arm (Sharpe) | `PSR(0)` | `N_eff` | `DSR(M=6)` | `DSR(N_eff)` | `CSCV PBO` |
|---|---|---|---:|---|---:|---:|---:|---:|---:|
| `Top-Tier 7-RWA` | `lighter_robinhood` | `o200_c300_e330` | 20 | `S4_speedbump_combined` | `0.0002` | `1.04` | `0.0000` | `0.0002` | `0.0000` |
| `Top-Tier 7-RWA` | `lighter_robinhood` | `o300_c300_e330` | 20 | `S4_speedbump_combined` | `0.0003` | `1.03` | `0.0000` | `0.0003` | `0.0571` |
| `Top-Tier 7-RWA` | `lighter` | `o200_c300_e330` | 20 | `S4_speedbump_combined` | `0.1015` | `1.04` | `0.0046` | `0.1015` | `0.0143` |
| `Top-Tier 7-RWA` | `lighter` | `o300_c300_e330` | 20 | `S1_latency_spread_floor` | `0.0565` | `1.03` | `0.0014` | `0.0565` | `0.0000` |
| Legacy 5-RWA (`ex-NVDA`) | `lighter_robinhood` | `o200_c300_e330` | 20 | `S4_speedbump_combined` | `0.0000` | `1.03` | `0.0000` | `0.0000` | `0.0143` |
| Legacy 5-RWA (`ex-NVDA`) | `lighter_robinhood` | `o300_c300_e330` | 20 | `S4_speedbump_combined` | `0.0000` | `1.02` | `0.0000` | `0.0000` | `0.0571` |
| Legacy 5-RWA (`ex-NVDA`) | `lighter` | `o200_c300_e330` | 20 | `S4_speedbump_combined` | `0.0701` | `1.04` | `0.0056` | `0.0701` | `0.0000` |
| Legacy 5-RWA (`ex-NVDA`) | `lighter` | `o300_c300_e330` | 20 | `S4_speedbump_combined` | `0.0393` | `1.04` | `0.0021` | `0.0393` | `0.0000` |
| Legacy 5-RWA (`incl-NVDA`) | `lighter_robinhood` | `o200_c300_e330` | 20 | `S4_speedbump_combined` | `0.0000` | `1.07` | `0.0000` | `0.0000` | `0.1857` |
| Legacy 5-RWA (`incl-NVDA`) | `lighter_robinhood` | `o300_c300_e330` | 20 | `S1_latency_spread_floor` | `0.0000` | `1.05` | `0.0000` | `0.0000` | `0.5000` |
| Legacy 5-RWA (`incl-NVDA`) | `lighter` | `o200_c300_e330` | 20 | `S1_latency_spread_floor` | `0.0110` | `1.05` | `0.0003` | `0.0110` | `0.0000` |
| Legacy 5-RWA (`incl-NVDA`) | `lighter` | `o300_c300_e330` | 20 | `S1_latency_spread_floor` | `0.0007` | `1.09` | `0.0000` | `0.0007` | `0.0000` |
| Full RWA (`15/16 mkts`) | `lighter_robinhood` | `o200_c300_e330` | 20 | `S1_latency_spread_floor` | `0.0000` | `1.09` | `0.0000` | `0.0000` | `0.3286` |
| Full RWA (`15/16 mkts`) | `lighter_robinhood` | `o300_c300_e330` | 20 | `S1_latency_spread_floor` | `0.0000` | `1.07` | `0.0000` | `0.0000` | `0.1286` |
| Full RWA (`15/16 mkts`) | `lighter` | `o200_c300_e330` | 20 | `S1_latency_spread_floor` | `0.0002` | `1.07` | `0.0000` | `0.0002` | `0.0000` |
| Full RWA (`15/16 mkts`) | `lighter` | `o300_c300_e330` | 20 | `S1_latency_spread_floor` | `0.0000` | `1.08` | `0.0000` | `0.0000` | `0.0143` |

---

## 6. Verification & Artifact Provenance

- **Redacted Research JSON:** `.agent-state/research/quant_upgrade_20261127/track03_full_universe_speedbump_w01_w26.json` (`sha256: 6b81c0f8b08681d23f974807fc91fe2b3d106fc128d4d870198582fdf46d66b9`, `LEAK_COUNT = 0`, `MISMATCH_COUNT = 0`, `rebate_bps = 0.0`, `max |realized_only - mtm_incl_residual| = 0.0112 $/M`).
- **Split Invariants:** `stage_a_selection_dev` (`1140` rows, `non_flat = 8`, `max_mtm_cash_err = 6.43e-12`), `stage_b_selection_dev` (`6750` rows, `non_flat = 51`), `stage_b_holdout` (`1760` rows, `non_flat = 36`), `stage_b_w26_oos` (`422` rows, `non_flat = 0`).
- **Verification Commands:** `make fmt-check` PASS, `make check` (`cargo clippy --locked --features research-replay --lib --bins -- -D warnings` + `go vet`) PASS, `cargo test --locked --features research-replay --lib` PASS, `python3 -S -m unittest -v scripts/test_research_track03_full_universe_speedbump_mm.py` (`4/4` OK) & `--self-test` OK, `BAD_LINKS = []`, `git diff --check` clean.
