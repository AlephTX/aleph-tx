# Paradigm Shift #1 Research: Speed-Bump-Immune 1m–4h Statistical Arbitrage & Perp-vs-Oracle Dislocation Reversion (`W01–W26`)

**Verdict: `NO-GO (HOLD)`** — Do **not** deploy `PairsStatArb` or `OracleDislocationReversion` live. Across `W01–W26` (`19` `selection_dev` windows `w01–w20` excl. `w08`, `5` untouched `holdout` windows `w21–w25`, and `1` `w26_oos` window) covering all `18` `lighter_robinhood` (`0/0` verified USDG) and `19` `lighter` (`0/0` Standard counterfactual and `0.40/2.80` verified Premium) markets, neither family passes out-of-sample generalization, fee stress (`+1/+2 bps`), or multiple-testing controls (`DSR_eff <= 0.0478`, `PSR(0) <= 0.3114`, `MinTRL_95 = None`).

---

## 1. Engineering Implementation & Safety Invariants

Implemented in [stat_arb.rs](../src/strategy/stat_arb.rs) and registered alphabetically in [mod.rs](../src/strategy/mod.rs):

- **Zero hot-path allocations & isolation**: Fixed-capacity stack `Ar1Ring` (`MAX_STAT_ARB_WINDOW = 256`), `Copy` structs (`L1Quote`, `OracleSnapshot`, `PairsStatArbDecision`, `OracleDislocationDecision`), zero `use crate::exchanges::*`, and default-off config switches (`enabled: false` with `#[serde(default, deny_unknown_fields)]`).
- **Monotonic clock & fail-closed guards**: Backward timestamps (`now_ms < prev_ms`), future quote/oracle timestamps, crossed/non-finite inputs, and stale feeds (`max_quote_age_ms`, `max_oracle_age_ms`) fail closed (`StatArbAction::Blocked`) without mutating valid filter state; gaps exceeding `max_gap_ms` reset rolling EWMA/AR(1) estimators and re-enter `WarmingUp`.
- **`DefenseFsm` & reduce-only exit safety**: Compatible with [defense_fsm.rs](../src/strategy/defense_fsm.rs) (`observe_with_fsm` / `DefenseState`). `RiskIncreasingEntry` scales by `DefenseState::entry_size_scale` (taking the conservative minimum across both legs in `PairsStatArb` under `SideDefense`) and is strictly blocked in `ReduceOnly`/`Halt`, while `ReduceOnlyExit` on open positions (`MeanReversion`, `StopLoss`, `MaxHoldTime`) is always permitted.
- **Two causal engines**:
  1. `PairsStatArb`: causal online EWMA OLS hedge ratio `beta_t` (clamped `[beta_min, beta_max]`), demeaned log-spread volatility & Z-score, causal AR(1) `phi` / half-life stationarity gate (`StationarityStatus::Stationary` required for entry), and max hold time stop.
  2. `OracleDislocationReversion`: causal `raw_dislocation_bps = (perp_mid - oracle_px) / oracle_px * 10_000` demeaned by slow EWMA equilibrium, spread-adjusted hurdle `max(min_dislocation_bps, spread_mult * spread_bps)`, optional `funding_rate_bps` alignment filter, convergence exit, structural-break guard, and time stop.

---

## 2. Experimental Protocol, Fee Evidence & Execution Realism

Evaluated via [research_stat_arb_and_oracle_reversion.py](../scripts/research_stat_arb_and_oracle_reversion.py) and companion tests [test_research_stat_arb_and_oracle_reversion.py](../scripts/test_research_stat_arb_and_oracle_reversion.py), pulling redacted results into `.agent-state/research/quant_upgrade_20261127/stat_arb_oracle_w01_w26.json` (`sha256: d45455ba393c7be93eec9bf5eb7989c2154d692215b69c4d5271c6f1b79ff295`).

- **Chronological splits (strictly excluding `w08` and `w27+`)**:
  - `selection_dev` (`19` windows): `w01–w07, w09–w20`
  - `holdout` (`5` windows, untouched during selection): `w21–w25`
  - `w26_oos` (`1` window, untouched): `w26`
- **Market coverage (continuous `>= 10h` windows with `<= 60s` max gap)**:
  - `lighter_robinhood` (`18` markets): `BTC/ETH/SOL` `25/25`, `14` RWA markets `20/20`, `SNDK` `6/25` (skipped cleanly when absent).
  - `lighter` (`19` markets): `BTC/ETH/SOL` `21/25`, `15` RWA markets `19/25`, `SNDK` `6/25`.
- **OPT-01 fee provenance** via [track_fee_evidence.py](../scripts/track_fee_evidence.py) (`account_fee_evidence_20261127.json`, `sha256: 4d3a9c1aeae5358bb9c55ab448db392ac3edb0a89244f74c23d89817c3359d92`):
  - `lighter_robinhood` (`USDG`): verified `0.0 / 0.0 bps` (`maker / taker`).
  - `lighter` (`USDC`): `0.0 / 0.0 bps` Standard counterfactual **and** verified `0.40 / 2.80 bps` Premium comparison.
  - Sensitivity stress: `+1.0 bps` and `+2.0 bps` per-leg fee stress.
- **Execution realism**:
  - `taker`: real L1 `bid/ask` crossing at `t_decision + 330 ms`, target clip `$500` clamped to `<= 0.50 * L1_depth` on entry and iterative slice unwinds on exit, plus causal hourly `funding_and_ctx` cash and mandatory terminal flatten.
  - `maker_postonly`: `PostOnly` resting at `t_decision + 200 ms` strictly print-verified against `public_trades` over `10 s` (zero fabricated fills), with `330 ms` IOC hedge/exit fallback when an orphan leg or timeout requires completion.

---

## 3. Family A Results: Multi-Asset Stat-Arb Pairs (`PairsStatArb`)

Tested `15` pairs (`MSTR-BTC`, `COIN-BTC`, `CRCL-ETH`, `NVDA-AMD`, `NVDA-MU`, `AMD-QQQ`, `NVDA-QQQ`, `AAPL-QQQ`, `MSFT-QQQ`, `GOOGL-QQQ`, `META-AMZN`, `SPY-QQQ`, `ETH-BTC`, `SOL-BTC`, `SOL-ETH`) across `ewma_tau_ms in {15m, 60m, 240m}` x `z_entry in {2.0, 2.5, 3.0}` (`9` grid arms per venue/execution mode).

### 3.1 Frozen Selection vs Untouched Holdout & `w26_oos`

All `9` global multi-pair configurations were **negative** on `selection_dev` (best global arm was `tau=15m, z=3.0`: `-$45.02` RH taker, `-$107.50` RH maker, `-$15.00` Lighter Std taker, `-$102.42` Lighter Std maker). Freezing only positive-dev pairs (`frozen_positive_dev_pairs` / `frozen_top5_pairs`) yielded positive in-sample PnL on `selection_dev` that **collapsed out-of-sample** on `holdout (w21–w25)`:

| Venue & Mode | Portfolio | `selection_dev` Net `0/0` (`Trades`) | `holdout` Net `0/0` (`Trades`) | `holdout` Prem / `+1bp` | `w26_oos` Net `0/0` (`Trades`) | `holdout` Ex-Top1 `0/0` | `Res. ($)` |
| :--- | :--- | ---: | ---: | ---: | ---: | ---: | ---: |
| `lighter_robinhood` `taker` | `all_pairs_best_global` (`15m, z=3.0`) | `-$45.02` (`1108`) | `+$1.66` (`277`) | `-$29.82` (`+1bp`) | `-$4.39` (`60`) | `-$1.59` | `$0.00` |
| `lighter_robinhood` `taker` | `frozen_positive_dev_pairs` (`7`) | `+$60.26` (`941`) | `-$10.63` (`200`) | `-$35.99` (`+1bp`) | `+$2.75` (`31`) | `-$13.61` | `$0.00` |
| `lighter_robinhood` `taker` | `frozen_top5_pairs` | `+$57.03` (`683`) | `-$8.32` (`149`) | `-$28.51` (`+1bp`) | `+$2.31` (`22`) | `-$11.30` | `$0.00` |
| `lighter_robinhood` `maker_postonly` | `frozen_positive_dev_pairs` (`5`) | `+$24.12` (`567`) | `-$10.60` (`140`) | `-$20.31` (`+1bp`) | `-$0.68` (`11`) | `-$10.46` | `$0.00` |
| `lighter` `taker` | `all_pairs_best_global` (`15m, z=3.0`) | `-$15.00` (`1071`) | `+$0.53` (`289`) | `-$98.29` (`Prem`) | `+$2.21` (`68`) | `-$5.03` | `$0.00` |
| `lighter` `taker` | `frozen_positive_dev_pairs` (`10`) | `+$77.70` (`1926`) | `-$17.08` (`563`) | `-$217.10` (`Prem`) | `-$3.45` (`115`) | `-$20.78` | `$0.00` |
| `lighter` `taker` | `frozen_top5_pairs` | `+$66.13` (`1154`) | `-$10.74` (`344`) | `-$135.10` (`Prem`) | `-$0.84` (`69`) | `-$14.44` | `$0.00` |
| `lighter` `maker_postonly` | `frozen_positive_dev_pairs` (`5`) | `+$13.95` (`490`) | `-$7.82` (`125`) | `-$23.67` (`Prem`) | `-$0.05` (`19`) | `-$8.59` | `$0.00` |

### 3.2 Notable Pair Diagnostics & Multiple Testing (`audit_multiple_testing_overfitting.py`)

- **Top dev pair reversal**: `NVDA-AMD` led `selection_dev` on both venues (`+$18.23` RH taker, `+$25.20` Lighter Std taker) yet flipped negative OOS (`-$6.76` RH, `-$0.93` Lighter Std). Only `NVDA-QQQ` stayed positive in both `selection_dev` (`+$9.41` RH taker, `+$10.58` Lighter Std taker) and combined OOS (`+$4.00` RH, `+$4.45` Lighter Std), while still losing `-1bp` stress (`-$4.11` RH dev, `-$12.72` Lighter dev).
- **Crypto-equity & Crypto pairs**: `MSTR-BTC` (`-$5.03` OOS `0/0` / `-$84.72` Prem taker), `COIN-BTC`, `CRCL-ETH`, `ETH-BTC`, `SOL-BTC`, and `SOL-ETH` all lost money on `selection_dev` on `lighter_robinhood` and failed OOS on `lighter`.
- **DSR / PBO** (`9` global grid arms across `selection_dev` / `w01–w26`):
  - `lighter_robinhood` `taker`: best `tau15m_z3.0`, `SR = -0.3094` (`-0.2692` full), `PSR(0) = 0.0785` (`0.0794` full), `N_eff = 2.07/9` (`2.12/9`), `DSR_eff = 0.0174` (`0.0102`), `MinTRL_95 = None`, `CSCV PBO = 0.0429` (`0.0000` full, i.e., all arms consistently non-positive).
  - `lighter` `taker`: best `tau15m_z3.0`, `SR = -0.1209` (`-0.0755` full), `PSR(0) = 0.3114` (`0.3596` full), `N_eff = 3.01/9` (`3.22/9`), `DSR_eff = 0.0478` (`0.0181`), `MinTRL_95 = None`, `CSCV PBO = 0.1714` (`0.3714` full).

---

## 4. Family B Results: Perp-vs-Oracle Dislocation Reversion (`OracleDislocationReversion`)

Swept `min_dislocation_bps in {3, 5, 8, 12}` x `spread_mult in {1.0, 1.5, 2.0}` x `require_funding_alignment in {False, True}` (`24` arms) across all `18` `lighter_robinhood` and `19` `lighter` markets.

| Venue & Mode | Cohort / Portfolio | `selection_dev` Net `0/0` (`Trades`) | `holdout` Net `0/0` (`Trades`) | `holdout` Prem / `+1bp` | `w26_oos` Net `0/0` (`Trades`) | `Res. ($)` |
| :--- | :--- | ---: | ---: | ---: | ---: | ---: |
| `lighter_robinhood` `taker` | `all_symbols_best_global` (`d12, s2.0, f=True`) | `-$1,731.44` (`10362`) | `-$485.08` (`3405`) | `-$742.70` (`+1bp`) | `+$2.65` (`300`) | `$104.40` dev / `$0.00` OOS |
| `lighter_robinhood` `taker` | `frozen_positive_dev` (`SPY` only) | `+$4.20` (`46`) | `+$0.53` (`7`) | `+$0.13` (`+1bp`) | `+$0.06` (`8`) | `$0.00` |
| `lighter_robinhood` `maker_postonly` | `all_symbols_best_global` (`d12, s2.0, f=True`) | `-$749.87` (`5380`) | `-$152.67` (`1569`) | `-$209.11` (`+1bp`) | `-$1.78` (`228`) | `$240.95` dev / `$0.00` OOS |
| `lighter_robinhood` `maker_postonly` | `frozen_positive_dev` (`AMZN, SPY`) | `+$7.07` (`148`) | `-$0.67` (`30`) | `-$2.50` (`+1bp`) | `+$0.39` (`16`) | `$0.00` |
| `lighter` `taker` | `all_symbols_best_global` (`d12, s2.0, f=True`) | `-$1,406.86` (`12385`) | `-$448.41` (`4895`) | `-$1,385.13` (`Prem`) | `+$12.37` (`353`) | `$0.00` |
| `lighter` `taker` | `frozen_positive_dev` (none) | `$0.00` (`0`) | `$0.00` (`0`) | `$0.00` | `$0.00` (`0`) | `$0.00` |
| `lighter` `maker_postonly` | `all_symbols_best_global` (`d12, s2.0, f=True`) | `-$1,008.36` (`5641`) | `-$226.73` (`1791`) | `-$319.65` (`Prem`) | `+$4.71` (`262`) | `$0.00` |
| `lighter` `maker_postonly` | `frozen_positive_dev` (none) | `$0.00` (`0`) | `$0.00` (`0`) | `$0.00` | `$0.00` (`0`) | `$0.00` |

### 4.1 Empirical Mechanism & Multiple Testing

- **Why oracle reversion loses systematically**: On RWA perps (`MU`, `AMD`, `NVDA`, `CRCL`, `COIN`, `MSTR`, `SNDK`), large deviations between `perp_mid` and `funding_and_ctx.oracle_px` reflect active off-hours/intraday perp price discovery against a lagging equity oracle rather than transient liquidity noise. Fading those dislocations suffers severe adverse selection (`-$1,570.57` RWA vs `-$160.87` Crypto on `lighter_robinhood` taker `selection_dev`; `-$1,322.20` RWA vs `-$84.67` Crypto on `lighter` Std taker `selection_dev`).
- **Funding-alignment filter**: Adding `require_funding_alignment = True` reduced `selection_dev` loss across every parameter bucket (e.g., `-$1,731.44` vs `-$1,799.12` on RH taker `d12, s2.0`; `-$1,406.86` vs `-$1,452.71` on Lighter taker), and earned positive `funding_cash_usd` (`+$6.83` dev RH taker, `+$6.77` dev Lighter taker), but cannot offset adverse price drift.
- **DSR / PBO** (`24` global grid arms): `PSR(0) = 0.0000`, `DSR_eff = 0.0000`, `MinTRL_95 = None`, `N_eff = 1.11–1.15 / 24`, `CSCV PBO = 0.1143–0.5571`.

---

## 5. Verification Summary

- **Rust**: `cargo fmt --check`, `cargo clippy --locked --features research-replay --lib -- -D warnings`, and `cargo test --locked --features research-replay --lib` (`5/5` `stat_arb` tests + full lib suite).
- **Python pure-stdlib**: `python3 -S scripts/research_stat_arb_and_oracle_reversion.py --self-test` and `python3 -S -m unittest -v scripts/test_research_stat_arb_and_oracle_reversion.py` (`6/6` tests OK with `duckdb`/`numpy`/`pandas`/`pyarrow`/`scipy` blocked).
- **Data & report hygiene**: `LEAK_COUNT = 0`, `MISMATCH_COUNT = 0`, `BAD_LINKS = []`, `git diff --check` clean, zero frozen/peer files touched.
