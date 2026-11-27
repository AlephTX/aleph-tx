# Track 01 (`01_quant_profit`) Profitable Directional Alphas & `W01–W27` Validation

**Date:** `2026-11-27` (CST) · **Operational Mode:** Strictly **`HOLD`** (zero live orders/cancellations, zero Tokyo deploys/restarts, read-only `nice -n 10` on `aleph` lakehouse)

**Verdict: `NO-GO (HOLD)` for live deployment.** Across `W01–W27` (`19` `selection_dev` windows `w01–w20` ex `w08`, `5` untouched `holdout` windows `w21–w25`, and `2` untouched `w26_w27_oos` windows `w26–w27`; strictly excluding `-w28`), covering all `18` `lighter_robinhood` (`USDG`, `0.0/0.0 bps`) and `19` `lighter` (`USDC`, verified `0.40/2.80 bps` Premium vs `0.0/0.0 bps` Standard counterfactual) markets:

1. **`OracleDiscoveryMomentum` (`mark_px - oracle_px`)** cuts the prior `OracleDislocationReversion` `selection_dev` loss by **95.7%** (`-$74.54` vs `-$1,731.44` RH taker), confirming that RWA `|mark - oracle| >= 15 bps` propagates in the **same** direction (`+2.62 bp` 5m / `+3.83 bp` 15m raw mark drift). However, executable L1 spread crossing (`330 ms` taker) and passive fill-selection/stop-loss whipsaws (`200 ms` `maker_postonly`) leave net PnL negative across all three splits (`-$74.54 / -$8.28 / -$18.10` RH taker; `-$48.65 / -$4.25 / -$12.85` RH maker; `DSR_eff <= 0.019`).
2. **`OverreactionReversion` (`15m` / `60m` extreme return fading)** achieves strong positive profitability on **both** `selection_dev` (`w01–w20`) and untouched `holdout` (`w21–w25`) under `0/0 bps` fees, surviving up to `+2.0 bps` per-leg slippage stress (`+$182.54` dev / **`+$125.06` holdout**, `PSR(0) = 0.952`, `win_SR = +1.197`, `max_dd = $6.67`, `PBO = 0.0571` on `lighter_robinhood` taker `lb15m_r60_rf0_f0`), **but fails on recent untouched `w26_w27_oos`** (`-$35.19` RH taker across `w26 = -$3.03`, `w27 = -$32.16`; `-$45.11` Lighter Std taker) and **fails under `lighter` verified `0.40/2.80 bps` Premium fees** (`-$286.58` dev / `-$48.15` holdout / `-$70.96` `w26_w27_oos`).

---

## 1. Rust Engineering Implementation (`src/strategy/directional_alpha.rs`)

Implemented in [directional_alpha.rs](../src/strategy/directional_alpha.rs) and registered alphabetically in [mod.rs](../src/strategy/mod.rs):

- **Default-off `serde(default, deny_unknown_fields)` configs**: Both `OracleDiscoveryMomentumConfig` and `OverreactionReversionConfig` default to `enabled: false` and validate finite monotonic parameter bounds at construction (`validate()`).
- **Zero `crate::exchanges::*` & zero hot-path heap allocations**: Pure `Copy` input/decision structs (`L1Quote`, `OracleCtxSnapshot`, `OracleMomentumDecision`, `OverreactionDecision`) and a stack-allocated fixed `[f64; MAX_OVERREACTION_BARS]` (`256`) ring buffer in `OverreactionReversion`.
- **Fail-closed safety guards**: Backward timestamps (`ClockRegression`), future quote/context timestamps (`InvalidInput`), `NaN`/`Inf`/crossed/non-positive prices (`InvalidInput`), stale quotes/context (`StaleQuote` / `StaleContext`), and gaps exceeding `max_gap_ms` (deterministic filter reset into `WarmingUp`) fail closed without mutating position state.
- **`DefenseFsm` scaling & reduce-only exit invariance**: Compatible with [defense_fsm.rs](../src/strategy/defense_fsm.rs) via `observe_with_fsm`. `RiskIncreasingEntry` scales by `DefenseState::entry_size_scale` (`Caution`) and blocks under `SideDefense`/`ReduceOnly`/`Halt`, whereas `ReduceOnlyExit` (`TakeProfit`, `MeanReversion`, `SignalDecay`, `StopLoss`, `MaxHoldTime`) is ALWAYS permitted (`reduce_only: true`) and retains `position` until `notify_exit_filled(exit_ms)`.

---

## 2. Research Protocol, Lakehouse Coverage & Execution Realism

Executed via [research_track01_profitable_alphas.py](../scripts/research_track01_profitable_alphas.py) and tested via [test_research_track01_profitable_alphas.py](../scripts/test_research_track01_profitable_alphas.py). Redacted summary stored outside Git at `.agent-state/research/profitability_upgrade_20261127/track01_directional_alphas_w01_w27.json` (`sha256: 99c47611af395beaf247040acc3518d69b19678b087554a3ab653f97a1718938`, zero secrets/account IDs).

- **Splits (`W01–W27`, excluding `w08` and `-w28`)**:
  - `selection_dev` (`19` windows): `w01–w07, w09–w20` (`19` crypto windows, `15` RWA windows; `w06, w10–w12` are crypto-only weekend windows).
  - `holdout` (`5` windows, frozen): `w21–w25` (`5` crypto windows, `4` RWA windows; `w22` is crypto-only).
  - `w26_w27_oos` (`2` windows, frozen): `w26, w27` (`2` crypto windows, `2` RWA windows).
- **Continuous `>= 10h` (`<= 60s` max gap) market coverage (`W01–W27`)**:
  - `lighter_robinhood` (`18` markets): `BTC/ETH/SOL` `26/26`, `14` RWA `21/21`, `SNDK` `6/26`.
  - `lighter` (`19` markets): `BTC/ETH/SOL` `22/26`, `15` RWA `20/26`, `SNDK` `6/26`.
- **Execution, Funding & Accounting**:
  - `taker`: L1 `bid/ask` crossing at `t_decision + 330 ms`, `$500` target clip (`$1,000` capacity stress separately) clamped to `<= 0.50 * L1_depth` on entry and iterative slice unwinds on exit.
  - `maker_postonly`: `PostOnly` resting at `t_decision + 200 ms` strictly print-verified against `public_trades` over `10 s` (zero fabricated fills), with `330 ms` IOC fallback for stop-loss / max-hold / terminal flatten.
  - **Realized vs Residual MTM**: `realized_only_net_0_0_usd` tracks closed fills + realized funding; `mtm_incl_residual_net_0_0_usd` adds conservative MTM on any unflattened terminal residual (`terminal_residual_notional_usd = $0.00` across all primary global configurations).
  - **Fee & Stress Models**: `lighter_robinhood` (`0.0/0.0 bps` USDG) vs `lighter` (`0.0/0.0 bps` Standard counterfactual and `0.40/2.80 bps` Premium), plus `+0.5 / +1.0 / +2.0 bps` per-leg slippage stress.

---

## 3. Empirical Signal Verification (`selection_dev` `w01–w20` Raw Mechanism)

Prior to running full execution backtests, `funding_and_ctx` (`5s` / `1m` causal snapshots) on `selection_dev (w01–w20)` confirmed both directional hypotheses:

1. **`OracleDiscoveryMomentum` (`mark_px - oracle_px`)**:
   - On **RWA** (`lighter_robinhood`): `|mark - oracle| in [15, 25) bps` (`N = 49,216`) drifts **in the same direction** by `+1.16 bp` (`5m`), `+1.20 bp` (`15m`), `+1.25 bp` (`30m`); `>= 25 bps` (`N = 57,202`) drifts **`+3.82 bp` (`5m`), `+6.00 bp` (`15m`), `+7.73 bp` (`30m`)**. On `lighter` RWA: `+1.61 / +3.04 / +4.53 bp` (`15–25 bps`) and `+3.99 / +6.38 / +8.30 bp` (`>= 25 bps`).
   - On **Crypto**: `|mark - oracle| >= 10 bps` is rare (`N = 422` RH, `N = 914` Lighter) and reverts (`-4.7` to `-49.8 bps`).
2. **`OverreactionReversion` (`60m` & `15m` returns)**:
   - On `60m` `|ret| >= 30 bps`, forward `60m` returns revert sharply by **`+14.12 bp`** on `lighter_robinhood` Crypto (`N = 15,804`) and **`+14.61 bp`** on `lighter` Crypto (`N = 15,712`), and by **`+2.32 bp`** (`N = 42,745` RH) and **`+3.98 bp`** (`N = 46,719` Lighter) on RWA.

---

## 4. Family 1 Results: `OracleDiscoveryMomentum` (`24` Grid Arms)

Grid swept strictly on `selection_dev`: `min_dislocation_bps in {12, 15, 20, 25}` x `max_hold_ms in {5m, 15m, 30m}` x `require_funding_alignment in {False, True}` (`spread_mult = 1.5`, `TP = 15 bps`, `SL = 25 bps`, `cooldown = 60s`).

| Venue & Mode | Cohort (`Best Dev Arm`) | `selection_dev` Realized `0/0` (`Trades`) | `holdout` Realized `0/0` (`Trades`) | `w26_w27_oos` Realized `0/0` (`Trades`) | `holdout` Prem / `+1bp` | `w01–w27` `DSR_eff` / `PBO` | `holdout` Avg Notl / Res ($) |
| :--- | :--- | ---: | ---: | ---: | ---: | ---: | ---: |
| `lighter_robinhood` `taker` | `rwa_only` (`d25_h30m_f1`) | `-$74.54` (`491`) | `-$8.28` (`53`) | `-$18.10` (`108`) | `-$12.03` (`+1bp`) | `0.000 / 0.086` | `$353.0 / $0.00` |
| `lighter_robinhood` `maker_postonly` | `rwa_only` (`d25_h5m_f1`) | `-$48.65` (`515`) | `-$4.25` (`57`) | `-$12.85` (`113`) | `-$6.62` (`+1bp`) | `0.000 / 0.214` | `$207.8 / $0.00` |
| `lighter_robinhood` `maker_postonly` | `frozen_global_pos_dev` (`5` syms) | — | `+$0.19` (`6`) | `-$0.37` (`6`) | `+$0.03` (`+1bp`) | — | `$133.5 / $0.00` |
| `lighter` `taker` | `rwa_only` (`d20_h30m_f1`) | `-$42.62` (`735`) | `-$22.96` (`106`) | `-$41.38` (`214`) | `-$46.18` (`Prem`) | `0.000 / 0.601` | `$391.1 / $0.00` |
| `lighter` `maker_postonly` | `rwa_only` (`d20_h30m_f1`) | `-$23.89` (`650`) | `-$11.93` (`81`) | `-$16.63` (`181`) | `-$18.17` (`Prem`) | `0.019 / 0.658` | `$259.3 / $0.00` |
| `lighter_robinhood` `taker` | `crypto_only` (`d12_h5m_f0`) | `-$9.28` (`41`) | `-$1.89` (`3`) | `$0.00` (`0`) | `-$2.19` (`+1bp`) | `0.193 / 0.136` | `$500.0 / $0.00` |

- **Why momentum fails net of execution**: Although flipping from reversion (`-$1,731.44` dev) to momentum (`-$74.54` dev) removes 95.7% of the bleed, RWA dislocations (`>= 20–25 bps`) coincide with wide L1 spreads and noisy stop-loss whipsaws (`-25 bps`) before the slower `15m–30m` oracle catch-up completes. Per-symbol in-sample selection (`frozen_persym_pos_dev_symbols`) earned `+$52.72` on `selection_dev` (`lighter_robinhood` taker, `PSR = 0.999`), then collapsed to `-$6.55` on `holdout` and `-$19.52` on `w26_w27_oos` (`-$15.94` / `-$23.03` on `lighter` taker).
- **Capacity `$1,000` stress (`mom_d15_h15m_f1`)**: Heavy L1 depth clipping (`777/1,371` entries on RH dev, `avg_entry_notl = $612.2`) worsened PnL to `-$348.54` dev / `-$38.15` holdout / `-$43.69` `w26_w27_oos` (`-$194.76 / -$52.72 / -$78.03` on `lighter` Std).

---

## 5. Family 2 Results: `OverreactionReversion` (`32` Grid Arms)

Grid swept strictly on `selection_dev`: `lookback_bars in {15m, 60m}` x `min_return_bps in {25, 30, 45, 60}` x `reversion_target_fraction in {0.0, 0.40}` x `require_funding_alignment in {False, True}` (`spread_mult = 2.0`, `stop_loss = 45 bps`, `max_hold = lookback`, `cooldown = 5m`).

### 5.1 Split-by-Split Performance, Fee Tiers, & Multiple Testing (`$500` Clip)

| Venue & Mode | Cohort (`Best Dev Arm`) | `selection_dev` Realized `0/0` (`Trades`, `Win SR`) | `holdout` Realized `0/0` (`Trades`, `Win SR`) | `holdout` `Prem` / `+1bp` / `+2bp` | `w26_w27_oos` Realized `0/0` (`Trades`) | `w26_w27_oos` `Prem` / `+1bp` | `w01–w27` `DSR_eff` / `PBO` | `holdout` Ex-Top1 / DD ($) |
| :--- | :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `lighter_robinhood` `taker` | `all_symbols` (`lb15m_r60_rf0_f0`) | **`+$182.54`** (`2,243`, `+0.216`) | **`+$125.06`** (`761`, **`+1.197`**) | `+$65.15` (`+1bp`) / **`+$5.25`** (`+2bp`) | **`-$35.19`** (`93`) | `-$42.14` (`+1bp`) | `0.630` / **`0.057`** | `+$92.85` / `$6.67` |
| `lighter_robinhood` `taker` | `rwa_only` (`lb15m_r60_rf0_f0`) | **`+$186.24`** (`1,614`, `+0.256`) | **`+$66.98`** (`600`, `+0.656`) | `+$20.37` (`+1bp`) / `-$26.25` (`+2bp`) | `-$25.50` (`68`) | `-$30.47` (`+1bp`) | `0.558` / **`0.071`** | `+$34.77` / `$14.62` |
| `lighter_robinhood` `taker` | `crypto_only` (`lb60m_r45_rf0_f0`) | `+$175.28` (`586`, `+0.378`) | `-$53.62` (`168`, `-0.556`) | `-$67.36` (`+1bp`) / `-$81.10` (`+2bp`) | `+$0.29` (`47`) | `-$3.13` (`+1bp`) | `0.617` / `0.700` | `-$54.26` / `$66.27` |
| `lighter_robinhood` `maker_postonly` | `all_symbols` (`lb15m_r30_rf0_f0`) | `+$55.97` (`4,325`, `+0.063`) | `+$21.18` (`1,437`, `+0.183`) | `-$36.91` (`+1bp`) / `-$95.00` (`+2bp`) | `-$21.41` (`298`) | `-$30.72` (`+1bp`) | `0.316` / `0.229` | `-$2.79` / `$21.94` |
| `lighter_robinhood` `maker_postonly` | `crypto_only` (`lb60m_r60_rf0_f0`) | `+$43.84` (`502`, `+0.171`) | `-$45.26` (`142`, `-0.615`) | `-$50.95` (`+1bp`) / `-$56.65` (`+2bp`) | `+$6.68` (`33`) | `+$5.88` (`+1bp`) | `0.318` / `0.914` | `-$42.93` / `$45.61` |
| `lighter` `taker` | `all_symbols` (`lb15m_r60_rf0_f1`) | `+$183.55` (`2,283`, `+0.253`) | **`+$126.87`** (`853`, `+1.000`) | **`-$48.15`** (`Prem`) / `+$64.37` (`+1bp`) | `-$45.11` (`109`) | `-$70.96` (`Prem`) | `0.686` / `0.143` | `+$101.69` / `$0.00` |
| `lighter` `taker` | `rwa_only` (`lb15m_r60_rf0_f1`) | `+$180.77` (`1,802`, `+0.262`) | `+$75.90` (`689`, `+0.511`) | `-$59.02` (`Prem`) / `+$27.71` (`+1bp`) | `-$37.02` (`84`) | `-$56.46` (`Prem`) | `0.622` / `0.143` | `+$50.71` / `$8.96` |
| `lighter` `taker` | `crypto_only` (`lb60m_r45_rf0_f0`) | `+$118.37` (`450`, `+0.313`) | `-$66.18` (`170`, `-0.624`) | `-$107.38` (`Prem`) / `-$80.90` (`+1bp`) | `-$0.66` (`47`) | `-$11.94` (`Prem`) | `0.889` / `0.686` | `-$60.37` / `$80.56` |
| `lighter` `maker_postonly` | `crypto_only` (`lb15m_r45_rf0_f0`) | `+$80.12` (`742`, `+0.404`) | **`+$81.81`** (`262`, `+1.413`) | **`+$50.03`** (`Prem`) / `+$61.95` (`+1bp`) | `-$10.19` (`48`) | `-$15.89` (`Prem`) | `0.939` / `0.371` | `+$29.20` / `$0.00` |
| `lighter` `maker_postonly` | `all_symbols` (`lb15m_r45_rf0_f1`) | `+$24.45` (`2,909`, `+0.057`) | `+$120.22` (`1,100`, `+0.827`) | `+$38.61` (`Prem`) / `+$69.21` (`+1bp`) | `-$20.68` (`175`) | `-$35.67` (`Prem`) | `0.544` / `0.043` | `+$67.61` / `$12.46` |

*(All rows above have `terminal_flat_all = true`, `terminal_residual_notional_usd = $0.00`, so `realized_only == mtm_incl_residual`. Annualized Sharpe scales `win_SR * sqrt(876)` — e.g. `+1.197 -> +35.43` in `holdout`, `+0.216 -> +6.40` in `selection_dev` — and requires `MinTRL_95 = 43`–`60` independent 10h windows vs `26` observed).*

### 5.2 Key Diagnostics & Stress Findings

1. **Premature partial-reversion exits (`rf40`) hurt vs pure time-horizon exits (`rf0`)**: Across `selection_dev`, `rf0` (`max_hold = 15m` or `60m` with `45 bps` stop-loss) consistently outperformed `rf40` (`40%` reversion take-profit), because taking small quick profits while absorbing `45 bps` tail stops clipped the right tail of mean-reversion bounces (`rev_60m_r30_rf40_f1` at `$1,000` clip lost `-$534.59` dev / `-$427.57` holdout / `-$45.83` `w26_w27_oos` on RH, with `$150.07` dev terminal residual).
2. **Untouched `holdout (w21–w25)` strength vs `w26_w27_oos` failure**:
   - On `lighter_robinhood` `taker` `lb15m_r60_rf0_f0`, `all_symbols` earned `+$125.06` across `w21–w25` (`w21 = -$6.67`, `w22 = +$38.15`, `w23 = +$18.28`, `w24 = +$27.50`, `w25 = +$47.79`; `4/5` positive windows, `PSR(0) = 0.952`, `avg_hold = 709.8s`, `avg_entry_notl = $393.6`, `302/761` entries depth-clipped, `+$92.85` ex-NVDA), and `frozen_global_pos_dev_symbols` (`10` symbols) earned `+$82.14` (`448` trades, `+$11.73` at `+2.0 bp` stress).
   - However, **`w26_w27_oos` flipped negative** (`w26 = -$3.03`, `w27 = -$32.16`, total `-$35.19`, `-5.06 bps/vol`), and per-symbol cherry-picking (`frozen_persym_pos_dev_symbols`) overfit badly (`+$627.37` dev -> `-$31.78` holdout -> `-$42.28` `w26_w27_oos`).
3. **Fee-tier divergence (`0/0` vs `0.40/2.80 bps` Premium)**: `lighter` `taker` (`lb15m_r60_rf0_f1`) matched `lighter_robinhood` under the `0/0` Standard counterfactual (`+$183.55` dev, `+$126.87` holdout, `-$45.11` `w26_w27_oos`), yet lost `-$286.58 / -$48.15 / -$70.96` under verified `0.40/2.80 bps` Premium fees. Only `lighter` `maker_postonly` `crypto_only` (`lb15m_r45_rf0_f0`) stayed positive under Premium fees in `holdout` (`+$50.03`), while still losing `-$2.47` in `selection_dev` and `-$15.89` in `w26_w27_oos` (`ETH` accounted for `$55.21/$80.12` dev and `$52.61/$81.81` holdout).

---

## 6. Verification Summary

- **Rust (`directional_alpha.rs` + `mod.rs`)**:
  - `cargo fmt --check`: PASS
  - `cargo clippy --locked --lib -- -D warnings`: PASS
  - `cargo test --lib strategy::directional_alpha`: **8/8 tests PASS** (default-off/invalid config fail-closed, `OracleDiscoveryMomentum` triggers & cooldown, `OverreactionReversion` triggers & `MeanReversion` exit, `DefenseFsm` `Caution` scaling / `SideDefense` block / `Halt` reduce-only exit invariance, stale/non-monotonic/NaN/future-timestamp/gap-reset guards, funding alignment & stop-loss).
- **Python (`research_track01_profitable_alphas.py` + `test_research_track01_profitable_alphas.py`)**:
  - `python3 -S scripts/research_track01_profitable_alphas.py --self-test`: PASS
  - `python3 -S -m unittest -v scripts/test_research_track01_profitable_alphas.py`: **5/5 tests PASS** (isolated `python3 -S` blocking `duckdb`/`numpy`/`pandas`/`pyarrow`/`scipy`, `W01–W27` discovery excluding `w08` and `-w28`, monotonic/stale fail-closed checks, print verification, realized vs residual MTM accounting).
  - Redacted JSON (`track01_directional_alphas_w01_w27.json`): `SECRET_HITS = 0`, `w28` strictly excluded.
