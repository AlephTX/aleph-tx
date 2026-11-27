# Track 03 (`03_incentive_turnover`): Core 4 Single-Factor Hold Ablation & `W28+` Pre-Registration (`W01–W27`)

**Date:** `2026-11-27` (CST) · **Baseline Commit:** `329d86e` · **Operational Mode:** Strictly **`HOLD`** (zero live orders/cancellations, zero Tokyo deploys/restarts, `-w28` strictly excluded)

This report isolates the causal driver of Track 03 profitability on `lighter_robinhood` (`USDG`, verified `0.00 / 0.00 bps` `verified_account_tier`, `account_fee_evidence_20261127.json` `sha256: 4d3a9c1aeae5358bb9c55ab448db392ac3edb0a89244f74c23d89817c3359d92`) across `Core 4` (`AAPL, AMZN, META, MSFT`) under `o200_c300_e330` (`21` RWA windows `W01–W27`, `w08` absent, `-w28` excluded, `756` rows, `max_parity_err = 0.0` vs `simulate_prepared_arm`, `max_mtm_cash_err = 1.30e-12`, `max_net_err = 0.0`):

1. **Pure Single-Factor Hold Ladder (`max_hold_ms in {15s (S4), 60s, 180s, 300s, 600s}`):** Locks every quote parameter, inventory skew (`2.0 bps`), `reduce_only_penny_ticks = 0`, latency (`o200_c300_e330`), queue (`0.5`), and terminal drain (`1,000 ms`) identically to frozen `S4_speedbump_combined`.
2. **Component Attribution Arms (`600 s` & `W5`):** Isolates `reduce_only_penny_ticks` (`0 → 1`) and `inventory_skew_bps_per_full_limit` (`2.0 → 5.0`) relative to `P3_patient_600s` (`penny = 1, skew = 5.0`) and `W5_nopenn_15bp_180s` (for direct `ex-META` comparison).
3. **Roadmap `§2` & `§4` Accounting:** Reports **primary true ledger `$/M = sum(net_cash_cost_usd) / sum(target_executed_fill_notional_usd) * 1e6`** (negative = net profit, positive = net cost) alongside clearly labeled `historical_double_sided_*_usd_per_m` (`2 * sum(ntl)`), `realized_only` vs `mtm_incl_residual`, `zero_rebate`, `maker_volume_usd`, taker share `s`, `max_gross_exposure_usd`, hold quantiles (`p50/p95/p99/max`), `stale_exit_blocks`, `terminal_residual_notional_usd`, `ex-META` sensitivity, per-symbol contribution shares, Politis-Romano stationary block bootstrap 95% CIs (`L = 3.0`, `B = 10,000`, `seed = 20261127`), and multiple-testing diagnostics (`DSR(M_raw)`, `DSR(N_eff)`, cumulative `M = 18` sensitivity, and `CSCV PBO`).
4. **Pre-Registered `W28+` Experiment Manifest & Shadow Config:** Freezes `track03_w28plus_prereg_plan.json` (`plan_sha256: ce851712e70d401a090595f601b5261f71fa7a5fc1ecd743a5ebef37c4aa98d1`) and [track03_core4_patient_shadow.toml](../examples/track03_core4_patient_shadow.toml) (`cargo run --locked --bin track01_track02_shadow -- --track 3 --config examples/track03_core4_patient_shadow.toml --check` verified).

Implemented in [research_track03_core4_hold_ablation.py](../scripts/research_track03_core4_hold_ablation.py) and [test_research_track03_core4_hold_ablation.py](../scripts/test_research_track03_core4_hold_ablation.py), building on [research_track03_full_universe_speedbump_mm.py](../scripts/research_track03_full_universe_speedbump_mm.py), [research_track03_patient_profit_frontier.py](../scripts/research_track03_patient_profit_frontier.py), and [audit_multiple_testing_overfitting.py](../scripts/audit_multiple_testing_overfitting.py) (per `docs/ALEPHTX_RESEARCH_AND_EXECUTION_ROADMAP_2026_11_27.md` `§§2, 4, 5`).

---

## 1. Executive Verdict: Why `S4_hold600s_only` Dominates `P3_patient_600s`

1. **Extending `max_hold_ms` Alone (`15s → 600s`) Explains >100% of `P3`'s Gain and Eliminates `P3`'s Regressions:**
   - Pure single-factor `S4_hold600s_only` (`penny = 0`, `skew = 2.0`) achieves **`-$13.66/M` (`+$151.26` MTM net / `+$151.32` realized on `$11.07M` target executed fill notional, `21/21` (`100%`) positive windows, `s = 0.0020`, `stale_exit_blocks = 0`, `$176.80` residual in holdout only)** vs frozen `S4_speedbump_combined` (`+$2.64/M`, `-$33.79` on `$12.79M`, `s = 0.4678`, `35` blocks, `33%` positive windows).
   - Paired Politis-Romano bootstrap vs `S4` gives **`+$8.812/win` (95% CI `[+$7.915, +$9.679]`, `p = 0.0000`)** and **`-16.30 $/M` (95% CI `[-18.03, -14.63] $/M`, `p = 0.0000`)**.
   - Monotonic hold progression across all 5 rungs: **`+$2.64/M` (`15s`) → `-$7.91/M` (`60s`) → `-$12.21/M` (`180s`) → `-$13.13/M` (`300s`) → `-$13.66/M` (`600s`)**, while forced IOC share `s` collapses **`0.4678 → 0.1637 → 0.0333 → 0.0113 → 0.0020`**.
2. **Adding `reduce_only_penny_ticks = 1` and `skew = 5.0` (`P3_patient_600s`) Actively Degraded Edge and `ex-META` Stability:**
   - At `600 s` hold, turning on **`reduce_only_penny_ticks = 1`** (`S4_hold600s_penny1`) costs **`-$25.74` (`+3.19 $/M` worse)** overall and flips `AAPL` from **`+$6.76` (`-$1.46/M`)** to **`-$11.50` (`+$2.23/M`)** while cutting `AMZN` (`+$27.24 → +$16.27`).
   - Raising **`inventory_skew_bps_per_full_limit` `2.0 → 5.0`** (`S4_hold600s_skew5`) costs **`-$17.40` (`+0.85 $/M` worse)** and drops executed notional by `-$626.7k`.
   - Combined (`P3_patient_600s`) loses **`-$36.86` (`+3.79 $/M` worse)** vs `S4_hold600s_only` (`+$114.39` / `-$9.87/M` vs `+$151.26` / `-$13.66/M`).
3. **`ex-META` Stability & Concentration Fix:**
   - Under `P3_patient_600s`, `ex-META` (`AAPL + AMZN + MSFT`) earned `+$35.11` (`-$3.62/M`, `71%` pos windows; collapsing to `+$0.36`, `-$0.21/M`, `50%` pos in `holdout`), leaving `META` at `69.3%` of `W01–W27` net PnL (`98.5%` in `holdout`).
   - Under **`S4_hold600s_only`**, **`ex-META` more than doubles to `+$72.64` (`-$7.97/M`, `20/21` (`95%`) positive windows)** — **`+$53.55` (`-$7.96/M`, `100%` pos) dev → `+$6.52` (`-$3.94/M`, `75%` pos) holdout → `+$12.57` (`-$17.20/M`, `100%` pos) `w26_w27_oos`**, and **all 4 symbols are individually profitable** (`META +$78.61` (`52.0%`), `MSFT +$38.64` (`25.5%`), `AMZN +$27.24` (`18.0%`), `AAPL +$6.76` (`4.5%`)).
   - Compared to `W5_nopenn_15bp_180s` (`ex-META -$9.99/M`, `+$15.59` on `$1.56M`, `71%` pos windows), `S4_hold600s_only` delivers **`4.7x` more `ex-META` cash (`+$72.64` vs `+$15.59`)** and **`5.8x` higher volume (`$9.11M` vs `$1.56M`)** with `95%` vs `71%` positive windows.
4. **Pre-Registered `W28+` Verdict:** Freeze **`S4_hold600s_only`** (`max_inventory_hold_ms = 600000`, `reduce_only_penny_ticks = 0`, `inventory_skew_bps_per_full_limit = 2.0`) as the sole primary `W28+` candidate in `track03_w28plus_prereg_plan.json` and [track03_core4_patient_shadow.toml](../examples/track03_core4_patient_shadow.toml), remaining operationally **`HOLD`** until uninspected `W28+` windows seal and pass all 9 falsification gates + Rust/Python parity verification.

---

## 2. Full `W01–W27` Single-Factor Hold Ladder & Component Attribution (`Core 4`, `lighter_robinhood` `0/0`)

Across all `21` RWA windows (`0` zero-fill windows; `w08` absent; `-w28` excluded; `rebate_bps = 0.0`, so `zero_rebate_ledger_mtm_usd_per_m == ledger_mtm_usd_per_m`):

| Arm (`o200_c300_e330`) | Ledger MTM `$/M` (`Real`) | Hist. DS `$/M` | MTM Net (`Real`) | Exec Notional (`Maker Vol`) | `s` | Max Port. Exp (`Max Sym`) | Hold `avg` / `p50` / `p95` / `p99` / `max` | `blk` | Resid ($) | `Pos Win` |
|---|---:|---:|---:|---:|---:|---:|---|---:|---:|---:|
| `S4_speedbump_combined` (`15s`) | `+2.64` (`+2.64`) | `+2.64` | `-$33.79` (`-$33.79`) | `$12.79M` (`$9.80M`) | `0.4678` | `$1,200.1` (`$600.2`) | `9.89s` / `11.4s` / `16.4s` / `20.1s` / `48.1s` | `35` | `$0.00` | `0.33` |
| `S4_hold60s` (`60s`) | `-7.91` (`-7.92`) | `-7.91` | `+$91.73` (`+$91.80`) | `$11.59M` (`$10.64M`) | `0.1637` | `$1,237.1` (`$600.2`) | `21.64s` / `13.9s` / `60.7s` / `62.2s` / `97.5s` | `7` | `$176.80` | `0.90` |
| `S4_hold180s` (`180s`) | `-12.21` (`-12.21`) | `-12.21` | `+$136.56` (`+$136.62`) | `$11.19M` (`$11.00M`) | `0.0333` | `$1,226.9` (`$600.2`) | `29.79s` / `14.6s` / `136.0s` / `180.6s` / `188.3s` | `2` | `$176.80` | `1.00` |
| `S4_hold300s` (`300s`) | `-13.13` (`-13.13`) | `-13.13` | `+$145.74` (`+$145.80`) | `$11.10M` (`$11.04M`) | `0.0113` | `$1,295.5` (`$600.2`) | `31.58s` / `14.7s` / `143.3s` / `281.7s` / `307.3s` | `0` | `$176.80` | `1.00` |
| **`S4_hold600s_only`** (`600s, p0, sk2`) | **`-13.66`** (`-13.67`) | **`-13.66`** | **`+$151.26`** (`+$151.32`) | `$11.07M` (`$11.06M`) | `0.0020` | `$1,295.5` (`$600.2`) | `32.71s` / `14.7s` / `147.6s` / `318.6s` / `603.3s` | `0` | `$176.80` | **`1.00`** |
| `S4_hold600s_penny1` (`600s, p1, sk2`) | `-10.47` (`-10.48`) | `-10.47` | `+$125.51` (`+$125.58`) | `$11.99M` (`$11.97M`) | `0.0018` | `$1,294.7` (`$600.2`) | `25.03s` / `10.1s` / `120.6s` / `280.8s` / `601.8s` | `0` | `$176.80` | `0.95` |
| `S4_hold600s_skew5` (`600s, p0, sk5`) | `-12.81` (`-12.82`) | `-12.81` | `+$133.85` (`+$133.92`) | `$10.45M` (`$10.44M`) | `0.0020` | `$1,060.2` (`$599.2`) | `32.16s` / `14.3s` / `146.8s` / `320.8s` / `601.8s` | `0` | `$176.80` | `1.00` |
| `P3_patient_600s` (`600s, p1, sk5`) | `-9.87` (`-9.88`) | `-9.87` | `+$114.39` (`+$114.46`) | `$11.59M` (`$11.58M`) | `0.0016` | `$1,000.0` (`$524.8`) | `24.22s` / `9.6s` / `117.0s` / `272.2s` / `601.8s` | `0` | `$176.80` | `0.95` |
| `W5_nopenn_15bp_180s` (`180s, p0, sk4`) | `-15.82` (`-15.82`) | `-15.82` | `+$30.50` (`+$30.50`) | `$1.93M` (`$1.90M`) | `0.0314` | `$751.2` (`$425.8`) | `27.58s` / `12.2s` / `146.4s` / `180.7s` / `183.2s` | `0` | `$0.00` | `0.95` |

*Accounting note:* `target_executed_fill_notional_usd = maker_volume_usd + taker_volume_usd`. Because all 4 symbols have tiny price drift across short `~25–33 s` holding times and `$176.80` total residual across `$11M`, `ledger_mtm_usd_per_m` and `historical_double_sided_mtm_usd_per_m` agree within `$0.0002/M`. On `S4_hold600s_only`, `max_drawdown_usd = $0.00` and `worst_10h_window = w13 (+$1.98)` (`P3_patient_600s` `max_drawdown_usd = $0.59`, `w13 = -$0.59`).

### 2.1 Factorial Component Attribution (`W01–W27`)

| Attribution Step | `Core 4` `ΔNet` ($) | `Core 4` `ΔLedger $/M` | `Core 4` `Δs` | `Core 4` `ΔExec` ($) | `ex-META` `ΔNet` ($) | `ex-META` `ΔLedger $/M` |
|---|---:|---:|---:|---:|---:|---:|
| **1. Pure hold (`15s → 600s`)**: `S4_hold600s_only - S4` | **`+$185.05`** | **`-16.30`** | `-0.4658` | `-$1,718,978` | **`+$119.48`** | **`-12.46`** |
| **2. Pure penny (`0 → 1` @ `600s`)**: `S4_hold600s_penny1 - S4_hold600s_only` | `-$25.74` | `+3.19` | `-0.0003` | `+$912,097` | `-$31.43` | `+3.83` |
| **3. Pure skew (`2.0 → 5.0` @ `600s`)**: `S4_hold600s_skew5 - S4_hold600s_only` | `-$17.40` | `+0.85` | `-0.0000` | `-$626,651` | `-$8.53` | `+0.53` |
| **4. Combined (`penny=1, skew=5`)**: `P3_patient_600s - S4_hold600s_only` | `-$36.86` | `+3.79` | `-0.0004` | `+$514,704` | `-$37.53` | `+4.35` |
| **Interaction (`penny x skew`)**: `P3 - (penny1 + skew5 - base)` | `+$6.28` | `—` | `—` | `—` | `+$2.43` | `—` |
| **Net `P3_patient_600s - S4`** | `+$148.18` | `-12.51` | `-0.4662` | `-$1,204,275` | `+$81.96` | `-8.11` |

---

## 3. Cross-Split Generalization, `ex-META` Sensitivity & Per-Symbol Shares

### 3.1 Cross-Split `Core 4` vs. `ex-META` (`selection_dev w01–w20` → `holdout w21–w25` → `w26_w27_oos`)

| Arm | `dev` `Core 4` `$/M` (`Net`, `pw`) | `holdout` `Core 4` `$/M` (`Net`, `pw`) | `w26_w27` `Core 4` `$/M` (`Net`, `pw`) | `dev` `ex-META` `$/M` (`Net`, `pw`) | `holdout` `ex-META` `$/M` (`Net`, `pw`) | `w26_w27` `ex-META` `$/M` (`Net`, `pw`) |
|---|---:|---:|---:|---:|---:|---:|
| `S4_speedbump_combined` | `+3.62` (`-$33.22`, `0.27`) | `+3.76` (`-$8.97`, `0.25`) | `-6.80` (`+$8.40`, `1.00`) | `+4.28` (`-$33.02`, `0.07`) | `+8.12` (`-$15.10`, `0.00`) | `-1.45` (`+$1.27`, `0.50`) |
| `S4_hold60s` | `-6.37` (`+$52.89`, `0.87`) | `-7.81` (`+$17.14`, `1.00`) | `-19.95` (`+$21.70`, `1.00`) | `-4.07` (`+$28.44`, `0.87`) | `-0.57` (`+$0.99`, `0.50`) | `-10.82` (`+$8.38`, `1.00`) |
| `S4_hold180s` | `-9.89` (`+$79.56`, `1.00`) | `-13.35` (`+$28.20`, `1.00`) | `-27.88` (`+$28.80`, `1.00`) | `-7.13` (`+$48.24`, `1.00`) | `-2.13` (`+$3.55`, `0.75`) | `-16.38` (`+$12.19`, `1.00`) |
| `S4_hold300s` | `-10.91` (`+$87.13`, `1.00`) | `-13.71` (`+$28.76`, `1.00`) | `-29.35` (`+$29.84`, `1.00`) | `-7.87` (`+$53.00`, `1.00`) | `-3.10` (`+$5.15`, `0.75`) | `-17.65` (`+$13.00`, `1.00`) |
| **`S4_hold600s_only`** | **`-11.35` (`+$90.44`, `1.00`)** | **`-14.80` (`+$31.01`, `1.00`)** | **`-29.52` (`+$29.81`, `1.00`)** | **`-7.96` (`+$53.55`, `1.00`)** | **`-3.94` (`+$6.52`, `0.75`)** | **`-17.20` (`+$12.57`, `1.00`)** |
| `S4_hold600s_penny1` | `-7.84` (`+$67.64`, `0.93`) | `-12.61` (`+$27.90`, `1.00`) | `-26.22` (`+$29.97`, `1.00`) | `-3.90` (`+$28.68`, `0.73`) | `-0.86` (`+$1.51`, `0.50`) | `-13.04` (`+$11.01`, `1.00`) |
| `S4_hold600s_skew5` | `-10.50` (`+$78.87`, `1.00`) | `-14.28` (`+$28.35`, `1.00`) | `-28.09` (`+$26.64`, `1.00`) | `-7.25` (`+$46.03`, `1.00`) | `-4.38` (`+$6.90`, `0.75`) | `-16.11` (`+$11.18`, `1.00`) |
| `P3_patient_600s` | `-7.62` (`+$63.73`, `0.93`) | `-11.48` (`+$24.35`, `1.00`) | `-23.79` (`+$26.31`, `1.00`) | `-3.50` (`+$25.07`, `0.73`) | `-0.21` (`+$0.36`, `0.50`) | `-11.59` (`+$9.68`, `1.00`) |
| `W5_nopenn_15bp_180s` | `-14.96` (`+$22.34`, `1.00`) | `-11.80` (`+$3.42`, `0.75`) | `-32.67` (`+$4.74`, `1.00`) | `-9.99` (`+$12.05`, `0.73`) | `-2.73` (`+$0.65`, `0.50`) | `-24.89` (`+$2.89`, `1.00`) |

### 3.2 Per-Symbol PnL, Ledger `$/M`, and Contribution Shares (`All W01–W27`)

| Arm | `AAPL` Net (`$/M`, `PnL%`, `Vol%`) | `AMZN` Net (`$/M`, `PnL%`, `Vol%`) | `META` Net (`$/M`, `PnL%`, `Vol%`) | `MSFT` Net (`$/M`, `PnL%`, `Vol%`) | `ex-META` Total (`$/M`) | Max Sym Share |
|---|---|---|---|---|---|---:|
| `S4_speedbump_combined` | `-$22.19` (`+4.21`, `65.7%` loss, `41.2%`) | `-$20.61` (`+8.41`, `61.0%` loss, `19.2%`) | `+$13.05` (`-5.55`, `-38.6%` loss, `18.4%`) | `-$4.05` (`+1.48`, `12.0%` loss, `21.3%`) | `-$46.84` (`+4.49`) | `—` |
| **`S4_hold600s_only`** | **`+$6.76` (`-1.46`, `4.5%`, `41.9%`)** | **`+$27.24` (`-12.89`, `18.0%`, `19.1%`)** | **`+$78.61` (`-40.09`, `52.0%`, `17.7%`)** | **`+$38.64` (`-16.35`, `25.5%`, `21.3%`)** | **`+$72.64` (`-7.97`)** | **`0.5197`** |
| `S4_hold600s_penny1` | `-$11.50` (`+2.23`, `-9.2%`, `43.0%`) | `+$16.27` (`-7.27`, `13.0%`, `18.7%`) | `+$84.30` (`-41.48`, `67.2%`, `17.0%`) | `+$36.44` (`-14.24`, `29.0%`, `21.4%`) | `+$41.21` (`-4.14`) | `0.6717` |
| `S4_hold600s_skew5` | `+$5.40` (`-1.24`, `4.0%`, `41.6%`) | `+$23.56` (`-11.83`, `17.6%`, `19.1%`) | `+$69.74` (`-38.20`, `52.1%`, `17.5%`) | `+$35.15` (`-15.40`, `26.3%`, `21.9%`) | `+$64.11` (`-7.44`) | `0.5210` |
| `P3_patient_600s` | `-$11.08` (`+2.19`, `-9.7%`, `43.6%`) | `+$14.02` (`-6.52`, `12.3%`, `18.6%`) | `+$79.28` (`-41.89`, `69.3%`, `16.3%`) | `+$32.17` (`-12.90`, `28.1%`, `21.5%`) | `+$35.11` (`-3.62`) | `0.6931` |
| `W5_nopenn_15bp_180s` | `+$5.02` (`-6.84`, `16.5%`, `38.1%`) | `+$7.84` (`-13.53`, `25.7%`, `30.1%`) | `+$14.91` (`-40.63`, `48.9%`, `19.0%`) | `+$2.73` (`-11.03`, `8.9%`, `12.8%`) | `+$15.59` (`-9.99`) | `0.4889` |

---

## 4. Politis-Romano Stationary Bootstrap & Multiple-Testing Audit (`W01–W27`)

### 4.1 Politis-Romano Paired Stationary Block Bootstrap vs. `S4_speedbump_combined` (`L = 3.0`, `B = 10,000`, `seed = 20261127`)

| Split | Cohort | Arm vs `S4` | `ΔNet/win` ($) (`95% CI`) | `ΔLedger $/M` (`95% CI`) | `p_non_impr` |
|---|---|---|---:|---:|---:|
| `all_w01_w27` (`21w`) | `Core 4` | **`S4_hold600s_only`** | **`+$8.812`** (`[+$7.915, +$9.679]`) | **`-16.30`** (`[-18.03, -14.63]`) | **`0.0000`** |
| `all_w01_w27` (`21w`) | `Core 4` | `S4_hold300s` | `+$8.549` (`[+$7.758, +$9.304]`) | `-15.77` (`[-17.35, -14.26]`) | `0.0000` |
| `all_w01_w27` (`21w`) | `Core 4` | `S4_hold180s` | `+$8.112` (`[+$7.330, +$8.892]`) | `-14.85` (`[-16.45, -13.30]`) | `0.0000` |
| `all_w01_w27` (`21w`) | `Core 4` | `P3_patient_600s` | `+$7.056` (`[+$6.047, +$8.140]`) | `-12.51` (`[-14.44, -10.85]`) | `0.0000` |
| `all_w01_w27` (`21w`) | `Core 4` | `W5_nopenn_15bp_180s` | `+$3.061` (`[+$1.773, +$4.394]`) | `-18.46` (`[-22.46, -14.62]`) | `0.0000` |
| `all_w01_w27` (`21w`) | `ex-META` | **`S4_hold600s_only`** | **`+$5.690`** (`[+$5.058, +$6.380]`) | **`-12.46`** (`[-14.42, -10.77]`) | **`0.0000`** |
| `all_w01_w27` (`21w`) | `ex-META` | `P3_patient_600s` | `+$3.903` (`[+$3.451, +$4.439]`) | `-8.11` (`[-9.36, -6.99]`) | `0.0000` |
| `all_w01_w27` (`21w`) | `ex-META` | `W5_nopenn_15bp_180s` | `+$2.973` (`[+$2.143, +$3.797]`) | `-14.47` (`[-20.13, -9.22]`) | `0.0000` |
| `selection_dev` (`15w`) | `Core 4` | **`S4_hold600s_only`** | `+$8.244` (`[+$7.385, +$9.143]`) | `-14.97` (`[-17.01, -13.30]`) | `0.0000` |
| `selection_dev` (`15w`) | `ex-META` | **`S4_hold600s_only`** | `+$5.771` (`[+$4.899, +$6.705]`) | `-12.25` (`[-15.13, -9.95]`) | `0.0000` |
| `holdout` (`4w`) | `Core 4` | **`S4_hold600s_only`** | `+$9.995` (`[+$8.790, +$11.549]`) | `-18.56` (`[-20.46, -16.56]`) | `0.0000` |
| `holdout` (`4w`) | `ex-META` | **`S4_hold600s_only`** | `+$5.405` (`[+$4.856, +$5.816]`) | `-12.06` (`[-13.10, -10.95]`) | `0.0000` |

### 4.2 Multiple Testing (`PSR`, `DSR(M_raw)`, `DSR(N_eff)`, `DSR(M=18)`, `CSCV PBO`)

| Suite (`21` aligned windows, `8` CSCV blocks = `70` splits) | Best Trial | `SR` | `PSR(0)` | `M_raw` | `DSR(M_raw)` | `N_eff` | `DSR(N_eff)` | `DSR(M_cum=18)` | `CSCV PBO` (`oos<=0`) |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| `core4_hold_ladder_w01_w27` | `S4_hold600s_only` | `1.6924` | `1.000000` | `5` | `0.998436` | `1.1169` | `1.000000` | `0.472853` | `0.4571` (`0.0000`) |
| **`core4_all_ablation_w01_w27`** | **`S4_hold600s_only`** | **`1.6924`** | **`1.000000`** | **`9`** | **`0.999544`** | **`1.2842`** | **`1.000000`** | **`0.986137`** | **`0.0714`** (`0.0000`) |
| `ex_meta_all_ablation_w01_w27` | `S4_hold600s_skew5` | `1.3504` | `1.000000` | `9` | `0.802097` | `1.3647` | `0.999999` | `0.473519` | `0.0143` (`0.0000`) |

*Note on `core4_hold_ladder_w01_w27` `PBO = 0.4571`:* Across the 5 pure hold rungs, `S4_hold300s` (`SR = 1.6872`) and `S4_hold600s_only` (`SR = 1.6924`) sit within `0.005` Sharpe of each other, so adjacent-rung IS/OOS rank swaps occur in `32/70` splits with **`prob_is_winner_oos_nonpositive = 0.0000`** (`0/70` splits have non-positive OOS Sharpe). Across all 9 arms, `S4_hold600s_only` reaches **`DSR(M=9) = 0.999544`, `DSR(N_eff) = 1.000000`, `DSR(M_cum=18) = 0.986137`, and `CSCV PBO = 0.0714`**. On `ex-META`, `S4_hold600s_only` achieves `SR = 1.3289` (`t = 6.09`, `PSR(0) = 1.000000`) right next to `S4_hold600s_skew5` (`SR = 1.3504`).

---

## 5. `W28+` Pre-Registered Manifest & Shadow Config (`HOLD` Gate)

- **Canonical Pre-Registered Manifest (untracked):** `.agent-state/research/roadmap_upgrade_20261127/track03_w28plus_prereg_plan.json` (`plan_sha256: ce851712e70d401a090595f601b5261f71fa7a5fc1ecd743a5ebef37c4aa98d1`, `file sha256: 2dd01a7af6f285de0b19a3b5ecf6041f023fb03f1c72d29404b4a9e619351ec0`, `SECRET_HITS = 0`).
- **Shadow Config (tracked):** [track03_core4_patient_shadow.toml](../examples/track03_core4_patient_shadow.toml) (`sha256: fb2ad2a281926e566b80e8b07bc77c546db42a18ecc4a8c90b00ca25db68e252`), configuring `AAPL` (`1006`), `AMZN` (`1007`), `META` (`1008`), `MSFT` (`1009`) on `target_exchange_id = 8` (`lighter_robinhood`), `ref_exchange_ids = [7, 10]`, `0.0/0.0 bps` `verified_account_tier`, `[defense]` enabled (`defense_enabled = true`), `max_inventory_hold_ms = 600000`, `reduce_only_penny_ticks = 0`, `inventory_skew_bps_per_full_limit = 2.0`, `capital_usd = 10000.0`, `daily_loss_limit_pct = 0.02`.
- **Pre-Registered `W28+` Falsification Thresholds (`min_independent_windows = 10`, `-w28` / `>= w28` only once sealed):**
  1. `1_cost_reduction_vs_s4`: `ledger_mtm_usd_per_m < 0.0`, `Δledger_vs_s4 <= -5.0 $/M`, Politis-Romano 95% CI upper bound `< 0.0`.
  2. `2_volume_floor`: `target_executed_fill_notional_per_window_usd >= $400,000`, `maker_volume_per_window_usd >= $380,000`.
  3. `3_absolute_cash_cost`: `mtm_net_cash_cost_usd <= 0.0`, `realized_net_cash_cost_usd <= 0.0`, `positive_window_ratio >= 0.70`.
  4. `4_max_gross_exposure`: `max_single_symbol_gross_exposure_usd <= $650.0` (1.0x `$400` limit + 1x `$200` resting fill + drift buffer), `max_portfolio_gross_exposure_usd <= $1,600.0`.
  5. `5_hold_tail_bounds`: `hold_p95_ms <= 240,000`, `max_hold_observed_ms <= 630,000`.
  6. `6_forced_ioc_share`: `taker_exit_share_s <= 0.03`.
  7. `7_terminal_residual_cap`: `terminal_residual_per_window_usd <= $100.0`.
  8. `8_ex_meta_stability_cap`: `ex_meta_ledger_mtm_usd_per_m <= 0.0`, `ex_meta_mtm_net_cash_cost_usd <= 0.0`.
  9. `9_single_symbol_share_cap`: `max_single_symbol_positive_pnl_share <= 0.80`.

---

## 6. Verification & Artifact Provenance

- **Redacted Research Artifacts (outside Git):**
  - `.agent-state/research/roadmap_upgrade_20261127/track03_core4_hold_ablation_w01_w27.json` (`sha256: 0c69090dabb9f035f3b0dfdb5c8d18b3997b7f5c53c639726f228b129e770a0a`, `SECRET_HITS = 0`, `W28_HITS = 0`).
  - `.agent-state/research/roadmap_upgrade_20261127/track03_w28plus_prereg_plan.json` (`sha256: 2dd01a7af6f285de0b19a3b5ecf6041f023fb03f1c72d29404b4a9e619351ec0`, `plan_sha256: ce851712e70d401a090595f601b5261f71fa7a5fc1ecd743a5ebef37c4aa98d1`, `SECRET_HITS = 0`).
- **Verification Commands:**
  - `python3 -S -m unittest -v scripts/test_research_track03_core4_hold_ablation.py` (5/5 OK, including isolated `python3 -S` import check blocking `numpy`, `duckdb`, `pyarrow`, `pandas`, `scipy`).
  - `python3 -S scripts/research_track03_core4_hold_ablation.py --self-test` (OK, `max_parity_diff = 0.0`).
  - `cargo run --locked --bin track01_track02_shadow -- --track 3 --config examples/track03_core4_patient_shadow.toml --check` (`"status": "Configuration OK"`, `"arms": 4`, `"defense_enabled": true`).
  - `git diff --check` clean; zero edits to untouched/frozen/peer files (`src/strategy/inventory_neutral_mm.rs`, `Cargo.lock`, `feeder/shm/`, `AGENT_HANDOFF.md`, `.agent-state/`, `.env*`, `docs/ALEPHTX_RESEARCH_AND_EXECUTION_ROADMAP_2026_11_27.md`, and the 6 pre-existing modified files).
