# AlephTX Three-Track Profitability & Execution Retrospective (`W01–W27`)

**Date:** `2026-11-27` (CST) · **Code Baseline:** `main` (`aebb451` Track 01, `5cff300` Track 02, `df241df..f41bbdb` Track 03) · **Operational Mode:** Strictly **`HOLD`** across all three tracks (zero live orders/cancellations, zero Tokyo deploys/restarts, `-w28` strictly excluded)

This retrospective synthesizes the verified findings, P0 simulator/ledger repairs, Rust safety hardening, and `W01–W27` out-of-sample evidence across:
- **Track 01 (`01_quant_profit`):** [TRACK01_DIRECTIONAL_ALPHA_PROFITABILITY_2026_11_27.md](TRACK01_DIRECTIONAL_ALPHA_PROFITABILITY_2026_11_27.md) · `.agent-state/research/profitability_upgrade_20261127/track01_directional_alphas_w01_w27.json` (`sha256: 99c47611af395beaf247040acc3518d69b19678b087554a3ab653f97a1718938`)
- **Track 02 (`02_hedge_arbitrage`):** [TRACK02_ZERO_FEE_WS_BASIS_AND_P0_LEDGER_FIX_2026_11_27.md](TRACK02_ZERO_FEE_WS_BASIS_AND_P0_LEDGER_FIX_2026_11_27.md) · `.agent-state/research/profitability_upgrade_20261127/track02_arcus_rh_zero_fee_basis_w01_w27.json` (`sha256: 05da725f8df4b58d274624ecc141e38bf62dade10b4bc58f08f5514f4c693e76`)
- **Track 03 (`03_incentive_turnover`):** [TRACK03_PATIENT_PROFIT_FRONTIER_AND_W27_VALIDATION_2026_11_27.md](TRACK03_PATIENT_PROFIT_FRONTIER_AND_W27_VALIDATION_2026_11_27.md) · `.agent-state/research/profitability_upgrade_20261127/track03_patient_frontier_w01_w27.json` (`sha256: df7a4a41d4a1c5b1f00cb32bcef422394c5c22a46fa0ea2961307f4478266b43`)

---

## 1. Executive Verdict & Cross-Track Readiness Matrix

### 1.1 Operational & Accounting Invariants

1. **Strict `HOLD` Invariant:** All three tracks remain operationally **`HOLD`**. No live trading, order cancellation, or Tokyo deployment is authorized by offline or replay research. Track 01 and Track 02 flipped-role are `NO-GO`; Track 02 reverse-role `B0` is `HOLD` due to missing `Arcus` `funding_and_ctx` telemetry, `NVDA` OOS concentration, and `< 1.77 bps` USDG/USDC FX sensitivity; Track 03 patient passive unwind (`P3_patient_600s` on `lighter_robinhood` `Core 4`) is the sole unrebated cross-split breakthrough (`DSR_eff = 0.9999`, `CSCV PBO = 0.0000`) and enters shadow/instrumentation verification under `HOLD`.
2. **Verified vs. Counterfactual Venue Fee Tiers:**
   - **`lighter_robinhood` (`USDG`, `ws_50ms`, `200 ms` post-only / `330 ms` taker):** Verified **`0.00 / 0.00 bps`** (`verified_account_tier` / `verified_standard_zero`).
   - **`lighter_std` (`USDC`, `ws_50ms`):** Strictly **counterfactual** **`0.00 / 0.00 bps`** (`lighter_standard_zero_scenario` / `lighter_standard_0_0_counterfactual`).
   - **`lighter_prem` (`USDC`, `ws_50ms`):** Verified **`0.40 / 2.80 bps`** (`verified_account_tier`).
   - **`arcus` (`USDC`, `ws_50ms`, `36 ms` order / `33 ms` cancel / `40 ms` exit):** Verified **`level_0` with 5% discount** — **RWA `0.00 / 2.1375 bps`**, **Crypto `1.425 / 4.275 bps`**.
   - **`trade_xyz` (`USDC`, `snapshot_12s`):** Verified **`0.30 / 0.90 bps`** (`growthMode`), audited in Track 02 P0 baseline & Track 03 paired freshness checks.
3. **Causal Split Structure (`W01–W27`, ex `w08`, `-w28` excluded):**
   - `selection_dev` (`w01–w20`): `19` Crypto windows (`w06, w10–w12` crypto-only), `15` RWA windows.
   - `holdout` (`w21–w25`, frozen): `5` Crypto windows (`w22` crypto-only), `4` RWA windows.
   - `w26_w27_oos` (`w26–w27`, untouched OOS): `2` Crypto windows, `2` RWA windows.
4. **`realized_only` vs. `mtm_incl_residual`:** Every track separates closed-fill + realized-funding cash (`realized_only`) from terminal unflattened inventory MTM (`mtm_incl_residual`). In Track 01 global configurations and Track 02 `Arcus ↔ LighterRH` / OOS `Arcus ↔ Lighter`, `terminal_residual_notional_usd = $0.00` (`realized_only == mtm_incl_residual`). In Track 03 `Core 4` `lighter_robinhood` `P3_patient_600s`, `holdout` carries `$177` terminal residual (`-$11.51/M` `realized_only` vs `-$11.48/M` `mtm_incl_residual`, `$0.03/M` delta; `$0.00` residual in `dev` and `w26_w27_oos`).

### 1.2 Cross-Track Readiness Matrix (`W01–W27`)

| Track | Strategy / Cohort (`Fee Tier`) | `selection_dev` (`w01–w20`) | `holdout` (`w21–w25`) | `w26_w27_oos` (`w26–w27`) | `realized` vs `mtm` | `PSR(0)` / `DSR_eff` / `CSCV PBO` | Readiness Verdict |
| --- | --- | --- | --- | --- | --- | --- | --- |
| **01** | `OracleDiscoveryMomentum` `lighter_robinhood` (`0/0`, taker RWA) | `-$74.54` (`491` tr) | `-$8.28` (`53` tr) | `-$18.10` (`108` tr) | Equal (`$0` res) | `0.001 / 0.0004 / 0.086` | **`NO-GO`** |
| **01** | `OverreactionReversion` `lighter_robinhood` (`0/0`, taker `all`) | `+$182.54` (`2,243` tr) | `+$125.06` (`761` tr) | **`-$35.19`** (`93` tr) | Equal (`$0` res) | `0.898` (`0.952` hold) / `0.630` / `0.057` | **`NO-GO (HOLD)`** (OOS decay) |
| **01** | `OverreactionReversion` `lighter_prem` (`0.40/2.80`, taker `all`) | `-$286.58` (`2,283` tr) | `-$48.15` (`853` tr) | `-$70.96` (`109` tr) | Equal (`$0` res) | `0.00/—/—` (Prem net neg) | **`NO-GO`** (fee drag) |
| **02** | P0-Fixed `USDG/USDC` `C3_top5` (`lighter_robinhood ↔ trade_xyz`, `0 bp` / `1 bp` FX) | `+$9.42` / `-$2.05` | `$0.00` / `$0.00` | `$0.00` / `$0.00` | Equal (`$0` res) | `— / 0.767 / 0.307` (`0 bp`) | **`HOLD`** (edge collapsed) |
| **02** | Flipped-Role `A0` `Arcus` Mk → `LighterRH` (`0/0` fees, `0 bp` FX RWA) | `-$77.35` (`51.3%` orp) | `-$13.30` (`44.3%` orp) | `+$0.90` (`1` fill) | Equal (`$0` res) | Negative (`A0–A2` all fail) | **`NO-GO`** (adverse sel.) |
| **02** | Reverse `B0` `Lighter` Mk → `Arcus` Tk (`Prem 0.40/2.80`, RWA) | `+$480.60` (`+3.51 bp`) | `+$21.55` (`+1.82 bp`) | `+$2.95` (`+1.71 bp`) | `+$480.45` vs `+$480.60` (`dev`) | `1.000 / 0.9997 / 0.014` | **`HOLD`** (`NVDA` conc., no `Arcus` fund.) |
| **02** | Reverse `B0` `LighterRH` Mk → `Arcus` Tk (`0/0` + `1 bp` FX, RWA) | `+$159.34` (`+3.52 bp`) | `+$4.85` (`+0.69 bp`) | `+$0.26` (`+0.41 bp`) | Equal (`$0` res) | `0.994 / 0.939 / 0.000` | **`HOLD`** (`ex-NVDA` neg OOS, FX `<1.77bp`) |
| **03** | `P3_patient_600s` `lighter_robinhood` (`0/0`, `Core 4` RWA) | `-$7.62/M` (`+$63.73`) | `-$11.48/M` (`+$24.35`) | `-$23.79/M` (`+$26.31`) | `-$11.51` vs `-$11.48/M` (`hold`) | **`1.0000 / 0.9999 / 0.0000`** | **`HOLD` → Shadow Gate** |
| **03** | `P3_patient_600s` `lighter_std` (`0/0` cf., `Top-Tier 7-RWA`) | `-$10.69/M` (`+$165.01`) | `-$3.83/M` (`+$12.05`) | `-$21.42/M` (`+$37.95`) | `+$12.06` / `+$37.96` real (`ALL -$10.56/M`, `$189.4` res) | **`1.0000 / 1.0000 / 0.0571`** | **`COUNTERFACTUAL`** (`0/0` Std) |
| **03** | `P3_patient_600s` `lighter_prem` (`0.40/2.80`, `Core 4` RWA) | `+$18.00/M` (`-$83.74`) | `+$22.84/M` (`-$23.33`) | `+$8.26/M` (`-$5.29`) | Equal (`$0` res) | `0.388 / 0.000 / 0.000` | **`NO-GO`** (`+$17.80/M` cost) |
| **03** | `P3_patient_600s` `arcus` (`0/2.1375`, `AMZN` / `TT7` RWA) | `+$24.01/M` (`AMZN`) | `+$178.91/M` (`AMZN`) | `+$46.41/M` (`AMZN`) | Equal (`$0` res) | `0.096 / 0.001 / 0.000` | **`NO-GO`** (`+$28.69–$72.09/M` cost) |

*(In Track 03, negative `$/M` denotes net profit per `$1M` exit notional under `rebate_bps = 0.0`, while positive `$/M` denotes net turnover cost.)*

---

## 2. Track 01 Deep Retrospective (`01_quant_profit`)

### 2.1 `OracleDislocationReversion` vs. `OracleDiscoveryMomentum`

- **Mechanism Inversion Confirmed:** Earlier research in [STAT_ARB_AND_ORACLE_REVERSION_RESEARCH_2026_11_27.md](STAT_ARB_AND_ORACLE_REVERSION_RESEARCH_2026_11_27.md) showed `OracleDislocationReversion` (`d12, s2.0, f=True`) lost **`-$1,731.44`** (`10,362` trades) on `lighter_robinhood` taker `selection_dev` (`-$485.08` on `holdout`) because RWA perp prices lead lagging equity oracles during off-hours and macro discovery. Causal `5s`/`1m` `funding_and_ctx` verification on `selection_dev` confirmed that RWA `|mark_px - oracle_px|` dislocations propagate in the **same** direction: on `lighter_robinhood`, `[15, 25) bps` (`N = 49,216`) drifts **`+1.16 / +1.20 / +1.25 bp`** over `5m / 15m / 30m`, and `>= 25 bps` (`N = 57,202`) drifts **`+3.82 / +6.00 / +7.73 bp`** (overall `>= 15 bps` drift reported as `+2.62 bp` 5m / `+3.83 bp` 15m; `lighter` RWA `+1.61/+3.04/+4.53 bp` and `+3.99/+6.38/+8.30 bp`), whereas rare Crypto dislocations (`>= 10 bps`, `N = 422` RH / `914` Lighter) revert (`-4.7` to `-49.8 bps`).
- **95.7% Loss Reduction, Yet Still Net-Negative After L1 Friction (`NO-GO`):** Flipping the trade sign in [research_track01_profitable_alphas.py](../scripts/research_track01_profitable_alphas.py) (`24` grid arms) cut `selection_dev` RH taker loss by **95.7%** (`-$1,731.44` → **`-$74.54`**, `d25_h30m_f1`), with `-$8.28` (`holdout`) and `-$18.10` (`w26_w27_oos`). `maker_postonly` (`200 ms`, strict `public_trades` verification) improved to `-$48.65` / `-$4.25` / `-$12.85`, and `lighter` maker reached `-$23.89` / `-$11.93` / `-$16.63`. Across all cohorts, `DSR_eff <= 0.019` (except sparse Crypto `d15_h15m_f0` taker `DSR_eff = 0.193`, still negative `-$9.28` dev / `-$1.89` holdout).
- **Microstructure Failure Mode:** Large RWA dislocations (`>= 20–25 bps`) coincide with wide L1 spreads and transient noise that triggers the `-25 bps` stop-loss before the slower `15m–30m` oracle catch-up completes. Increasing target clip from `$500` (`all_symbols` `d15_h15m_f1`: `-$199.58`, `1,379` trades) to `$1,000` (`mom_d15_h15m_f1`: `1,371` trades) clipped `777/1,371` entries on RH dev (`avg_entry_notl = $612.2`) and worsened PnL to `-$348.54` (`-$38.15` holdout / `-$43.69` `w26_w27_oos`). Per-symbol in-sample selection (`frozen_persym_pos_dev_symbols`) overfit (`+$52.72` dev → `-$6.55` holdout → `-$19.52` `w26_w27_oos`).

### 2.2 `OverreactionReversion`: Dev + Holdout Success Followed by `w26_w27_oos` Decay

- **Strong `0/0` Dev & Holdout Performance:** Extreme `60m` returns (`|ret| >= 30 bps`) exhibit strong raw reversion (`+14.12 bp` RH Crypto, `+14.61 bp` Lighter Crypto, `+2.32 bp` RH RWA, `+3.98 bp` Lighter RWA). Across `32` grid arms (`$500` clip, `330 ms` taker), `lb15m_r60_rf0_f0` on `lighter_robinhood` (`0/0` verified) delivered **`+$182.54`** (`2,243` trades, `win_SR = +0.216`) in `selection_dev` and **`+$125.06`** (`761` trades, **`win_SR = +1.197`**, `PSR(0) = 0.952`, `max_dd = $6.67`, `4/5` positive windows, `+$92.85` ex-top1 `NVDA`, `PBO = 0.057`) in frozen `holdout`, surviving `+1.0 bp` (`+$65.15`) and `+2.0 bp` (`+$5.25`) per-leg slippage stress.
- **Why Pure Horizon Exits (`rf0`) Beat Partial-Reversion Targets (`rf40`):** `reversion_target_fraction = 0.40` clipped the right tail of mean-reversion bounces while absorbing full `-45 bps` stop-losses (`rev_60m_r30_rf40_f1` at `$1,000` clip lost `-$534.59` dev / `-$427.57` holdout on RH with `$150.07` dev terminal residual).
- **Why Verdict Remains `NO-GO (HOLD)`:**
  1. **Recent `w26_w27_oos` Regime Decay:** On untouched `w26–w27`, RH taker `lb15m_r60_rf0_f0` dropped to **`-$35.19`** (`w26 = -$3.03`, `w27 = -$32.16`, `93` trades), `lighter_robinhood` `rwa_only` lost `-$25.50`, RH `maker_postonly` lost `-$21.41`, and `lighter` Std `lb15m_r60_rf0_f1` lost `-$45.11`. Moreover, observed `26` windows remain below `MinTRL_95 = 43–60` independent `10h` windows required at `0.216` dev Sharpe (`w01–w27` `DSR_eff = 0.630`).
  2. **Fee-Tier Barrier (`0.40/2.80 bps` Prem):** Verified `lighter` Premium fees turned `lb15m_r60_rf0_f1` deeply negative across every split (`-$286.58` dev / `-$48.15` holdout / `-$70.96` `w26_w27_oos`). Only `lighter` `maker_postonly` `crypto_only` (`lb15m_r45_rf0_f0`) stayed positive in `holdout` (`+$50.03` Prem), but lost `-$2.47` in `dev` and `-$15.89` in `w26_w27_oos` with heavy `ETH` concentration (`$55.21/$80.12` dev and `$52.61/$81.81` holdout).
  3. **Per-Symbol Overfitting Trap:** `frozen_persym_pos_dev_symbols` earned `+$627.37` in `dev`, then collapsed to `-$31.78` (`holdout`) and `-$42.28` (`w26_w27_oos`).

### 2.3 Production-Ready Rust Engine Hardening (`src/strategy/directional_alpha.rs`)

Even though both signal families remain `NO-GO (HOLD)`, the production Rust module [directional_alpha.rs](../src/strategy/directional_alpha.rs) (registered in [mod.rs](../src/strategy/mod.rs), `8/8` unit tests passing) leaves the execution boundary ready for any future OOS-validated alpha:
- **Zero-Allocation Hot Path & Pure Boundary:** Zero `crate::exchanges::*` imports; `Copy` snapshots (`L1Quote`, `OracleCtxSnapshot`, `OracleMomentumDecision`, `OverreactionDecision`) and fixed-capacity stack ring buffers (`[f64; MAX_OVERREACTION_BARS]` with `MAX_OVERREACTION_BARS = 256` for `closes` and `bar_rets_bps` in `OverreactionReversion`).
- **Default-Off Strict Configs:** `OracleDiscoveryMomentumConfig` and `OverreactionReversionConfig` enforce `enabled: false` by default and `#[serde(default, deny_unknown_fields)]` with monotonic bound checks in `validate()`.
- **Fail-Closed Input & Clock Guards:** Rejects backward clocks (`DirectionalBlockReason::ClockRegression`), future quote/context timestamps or `NaN`/`Inf`/crossed prices (`InvalidInput`), stale feeds (`StaleQuote`, `StaleContext`), wide spreads (`WideSpread`), structural breaks (`StructuralBreak`), low volatility Z-scores (`LowVolZ`), adverse funding (`AdverseFunding`), and inter-tick gaps exceeding `max_gap_ms` (deterministic filter/history reset into `WarmingUp`) without mutating open position state.
- **`DefenseFsm` & `ReduceOnlyExit` Invariance:** Via `observe_with_fsm`, `OrderIntentClass::RiskIncreasingEntry` scales down under `DefenseState::Caution` (`entry_size_scale`) and blocks under `SideDefense`/`ReduceOnly`/`Halt` (`DefenseBlocked`), whereas `OrderIntentClass::ReduceOnlyExit` (`TakeProfit`, `MeanReversion`, `SignalDecay`, `StopLoss`, `MaxHoldTime`) is **always** emitted with `reduce_only: true` even under `DefenseState::Halt` and clears `position` only upon `notify_exit_filled(exit_ms)`.

---

## 3. Track 02 Deep Retrospective (`02_hedge_arbitrage`)

### 3.1 All 3 P0 Carry Ledger Fixes & Quantitative Before/After Impact

Prior to Track 02 `W01–W27` evaluation, [research_funding_basis_carry_portfolio.py](../scripts/research_funding_basis_carry_portfolio.py) contained three P0 accounting defects identified in `ALEPHTX_RESEARCH_AND_EXECUTION_ROADMAP_2026_11_27.md` §3. All three were patched and pinned with deterministic counterexample tests (`8/8` passing in [test_research_funding_basis_carry_portfolio.py](../scripts/test_research_funding_basis_carry_portfolio.py)):

1. **P0 Defect 1 — Hedge L1 Depth Clamping:** Replaced `min(filled_notl, max(h_l1, filled_notl))` (which always returned `filled_notl`) with `hedged_notl = min(filled_notl, h_l1)`, routing the unmatched `filled_notl - hedged_notl` excess to explicit orphan accounting (`test_p0_defect1_hedge_depth_clamping_and_partial_orphan_unwind`).
2. **P0 Defect 2 — Orphan-Leg Booking, Unwind & Residual Retention:** Replaced silent dropping of unhedged passive fills on hedge rejection (`adv_move`, `h_l1 < $100`, missing `hq`) with `_book_orphan_fill` and `_try_unwind_orphan`, recording `orphan_entries`, `orphan_notional_usd`, delayed `t_hedge + 330 ms` taker unwind across displayed L1 depth, maker+taker fees, single-leg funding, and dark-book `terminal_residual_notional_usd` (`test_p0_defect2_orphan_leg_booked_and_unwound_on_hedge_reject_and_missing_book`).
3. **P0 Defect 3 — Per-Leg Timestamps & Strict `ht > leg_fill_ms` Funding:** Replaced atomic `entry_ms = t_hedge` (`<= ht`) with per-leg `leg_a_fill_ms`, `leg_b_fill_ms`, `leg_a_exit_ms`, `leg_b_exit_ms` via `_settle_single_leg_funding`, strictly requiring `ht > leg_fill_ms` (`test_p0_defect3_per_leg_fill_timestamps_and_strict_settlement_skew`).

**Quantitative Impact (`USDG/USDC` RWA, `lighter_robinhood ↔ trade_xyz`, `W01–W26`):**
The previously `CONDITIONAL` edge in [TRACK02_FUNDING_BASIS_CARRY_PORTFOLIO_2026_11_27.md](TRACK02_FUNDING_BASIS_CARRY_PORTFOLIO_2026_11_27.md) collapsed once unhedged passive exposure was charged to the ledger:
- **`0 bp` FX:** `C2` `selection_dev` fell **`+$10.54 → +$3.46`** (`8` orphans totaling `$3,805.31`, `-$4.25` orphan PnL, `39` clipped entries), `holdout` **`+$1.02 → +$0.88`**, and `C3_top5` **`+$11.33 / +$2.23 / +$0.36 → +$9.42 / $0.00 / $0.00`** (`DSR_eff` `1.0000 → 0.7673`, `CSCV PBO` `0.0286 → 0.3071`).
- **`1 bp` FX:** `C2` dropped **`+$7.24 / +$0.81 / +$0.06 → +$1.72 / $0.00 / $0.00`** (`3` orphans, `-$1.48`), and `C3_top5` turned negative **`+$8.37 / +$1.77 / +$0.14 → -$2.05 / $0.00 / $0.00`** (`DSR_eff` `0.9963 → 0.3931`, `PBO` `0.0714 → 0.4214`).
- **`3 bp` FX:** `C2` dropped **`+$5.75 / +$0.08 / $0.00 → +$1.90 / $0.00 / $0.00`**, and `C3_top5` stayed near zero **`+$0.48 / +$0.19 / $0.00 → +$1.17 / $0.00 / $0.00`** (`PBO` `0.6571 → 0.4895`).
- Same-domain RWA `C2` `holdout` flipped **`+$1.69 → -$0.40`**, and `standard_zero USDC/USDC` RWA `C2` worsened **`-$15.72 → -$22.81`** (`dev`) and **`$0.00 → -$1.23`** (`holdout`).

### 3.2 Flipped-Role `Arcus` Maker → `LighterRH` vs. Reverse `B0_hedge_mk_arcus_tk` (`W01–W27`)

In [research_track02_arcus_rh_zero_fee_basis.py](../scripts/research_track02_arcus_rh_zero_fee_basis.py) (P0-clean, `4/4` unit tests in [test_research_track02_arcus_rh_zero_fee_basis.py](../scripts/test_research_track02_arcus_rh_zero_fee_basis.py)), we tested whether `Arcus` (`0.00 / 2.1375 bps` RWA, `36 ms` delay, `ws_50ms`) paired with `LighterRH` (`0.00 / 0.00 bps`) or `Lighter` (`0.40 / 2.80 bps` Prem vs `0.00 / 0.00 bps` Std) could capture zero-fee basis reversion (`corr15 = -0.450` to `-0.671`, `corr60 = -0.669` to `-0.756` on `(b - 2h_MA)`):

1. **Why Flipped-Role (`A0/A1/A2`: `Arcus` Maker → Hedge) Failed:** Even at `0.00 bps` all-in fees (`Arcus` Maker + `LighterRH` Taker) and `0 bp` FX, **every flipped-role arm lost money across `selection_dev` and `holdout`**: `A0` (`-$77.35 / -$13.30 / +$0.90`, **`51.32%` dev orphan rate**, `-$79.22` orphan PnL), `A1` passive-hedge fallback (`-$80.37 / -$28.22 / -$0.42`, `24.72%` dev orphan), and `A2` demeaned reversion (`-$319.72 / -$19.74 / $0.00`, `58.47%` dev orphan). **Mechanism:** `Arcus` (`0 ms` speedbump, tight `0.15–2.27 bps` spreads) is the fast price-discovery venue; passive fills on `Arcus` are informed flow, and during the `330 ms` hedge delay on `LighterRH` (or `200 ms + 1.5 s` passive hedge), `LighterRH` quotes shift or thin out, leaving `25%–59%` unhedged toxic inventory.
2. **Why Reverse `B0_hedge_mk_arcus_tk` (Speedbump Maker → `Arcus` Taker) Succeeded Statistically:**
   - **`Arcus ↔ Lighter` (`USDC/USDC`, verified `Prem` `0.40/2.80` + `Arcus` `0.00/2.1375`, 15 RWA):** `B0` (`b0_e10_v0.0_sh0`) paid `$459.88` total fees (`$411.17 + $41.35 + $7.36`) and still earned **`+$480.60`** (`+3.51 bps`, realized `+$480.45`, `$1,000` `MSFT` dev residual) in `selection_dev` → **`+$21.55`** (`+1.82 bps`, `173` fills) in `holdout` → **`+$2.95`** (`+1.71 bps`, `24` fills) in `w26_w27_oos` (`PSR = 1.0000`, `DSR_eff = 0.9997`, **`CSCV PBO = 0.0143`**). Under `Lighter` Std (`0/0` counterfactual), `B0` earned **`+$747.98 / +$47.76 / +$6.63`** (RWA) and **`+$150.03 / +$22.54 / +$8.22`** (Crypto, `PBO = 0.0000`).
   - **`Arcus ↔ LighterRH` (`USDG/USDC`, 14 RWA) at `1 bp` FX:** `B0` (`b0_e10_v1.0_sh2`) stayed positive across all splits at `1 bp` FX (**`+$159.34 / +$4.85 / +$0.26`**, `PSR = 0.9944`, `DSR_eff = 0.9390`, **`CSCV PBO = 0.0000`**), but turned OOS negative at `2 bp` (`+$119.38 / -$1.47 / -$0.46`) and `3 bp` (`+$79.42 / -$7.79 / -$1.18`), pinning `holdout` FX breakeven at **`~1.77 bps`** (`~1.36 bps` on `w26_w27_oos`).
3. **Honest Gaps Keeping Track 02 in `HOLD`:**
   - **Missing `Arcus` Funding Telemetry:** Aleph lakehouse has `235–293` hourly `funding_and_ctx` slots per symbol on `LighterRH`/`Lighter`, but **zero `funding_and_ctx` files for `Arcus`**, leaving `funding_leg_mk_usd = $0.00` unverified across `100–206 min` median holding periods.
   - **Single-Name `NVDA` OOS Concentration:** Excluding `NVDA` reduces `Arcus ↔ Lighter` `Prem` `B0` from `+$480.60 / +$21.55 / +$2.95` to `+$108.28 / +$1.29 / +$0.89`, and flips `Arcus ↔ LighterRH` (`1 bp` FX) OOS negative (`+$61.57 / -$3.62 / -$0.19`).

---

## 4. Track 03 Deep Retrospective (`03_incentive_turnover`)

### 4.1 Both P0 IOC Simulator Fixes (`research_track03_full_universe_speedbump_mm.py`)

Both P0 defects from `ALEPHTX_RESEARCH_AND_EXECUTION_ROADMAP_2026_11_27.md` §3 were resolved in [research_track03_full_universe_speedbump_mm.py](../scripts/research_track03_full_universe_speedbump_mm.py) and verified (`6/6` tests in [test_research_track03_full_universe_speedbump_mm.py](../scripts/test_research_track03_full_universe_speedbump_mm.py)):
1. **`declared_end_ms` Cutoff Immunity:** Added `declared_end_ms: int | None = None` to `simulate_prepared_arm` and `simulate_arm` (and excluded `kind == 1` `ref` ticks when inferring default end timestamp), with lakehouse evaluation passing `declared_end_ms = b_rows[-1][0]` so late `arcus`/`trade_xyz` reference ticks cannot extend `terminal_drain_start_ts` or post-window MTM (`test_p0_declared_end_ms_immunity_to_late_ref_ticks`).
2. **`stale_exit_blocks` & `330 ms` Delayed IOC Re-Queue:** Fixed `process_pending_iocs` so expired IOCs hitting `!book_is_fresh(now_ms)` increment `stale_exit_blocks += 1` and re-queue with `exec_ts = now_ms + arm.exit_delay_ms` (`330 ms`), and removed the immediate post-book-refresh `process_pending_iocs(ts)` call so newly arriving books never execute stale IOCs at `0 ms` (`test_p0_stale_pending_ioc_requeues_330ms_and_tracks_stale_exit_blocks`).

### 4.2 Untouched `W27 + W26` Paired Dual-Venue Validation & Freshness Audit

Across `W26` and `W27` (`846` paired rows, `non_flat_count = 2`, `max_net_err = 0.0`, `max_mtm_cash_err = 3.10e-12`, `rebate_bps = 0.0`):
- **Dual-Venue Freshness Reality:** `arcus` RWA maintains **`0.0%` stale time share (`>5 s`)**, `200 ms` median `dt`, and `max_gap <= 2.28 s` (`2,045 ms` W26 / `2,283 ms` W27), giving continuous cross-venue anchoring where `trade_xyz` remains a **`~12.4 s` snapshot feed (`59.4%` stale share, `13.1–13.3 s` max gap)**. Meanwhile, `lighter_robinhood` RWA (`14` markets) experienced **`24.0–28.1 s` max gaps** (`1.7%–4.1%` stale share), triggering **`2–30` `stale_exit_blocks`** across cohorts (`8,854–13,898 ms` max gap on the 8-market Top-Tier subset).
- **`W27` & `W26+W27` Paired `G4 / G5 / S4` Results (`o200_c300_e330`):** On untouched `W27`, `Top-Tier 7-RWA` `S4_speedbump_combined` flipped net-profitable at **`-$2.66/M`** (`+$4.86`, `$0.00` residual) on `lighter_robinhood` (`0/0`) and **`-$4.06/M`** (`+$4.17`) on `lighter_std` (`0/0` counterfactual), beating `G4_baseline` (`+$2.19/M` RH / `-$0.95/M` Std). Across `W26+W27`, `Top-Tier 7-RWA` `S4` reached **`-$2.42/M`** (`+$9.52`) RH / **`-$3.07/M`** (`+$6.15`) Std, and `Positive-Markout Core 4` (`AAPL, AMZN, META, MSFT`) hit **`-$6.80/M`** (`+$8.40`) RH / **`-$8.11/M`** (`+$7.84`) Std.

### 4.3 The Patient Passive Unwind Frontier Breakthrough (`W01–W27`)

Sweeping `12` arms across `21` RWA and `26` Crypto windows in [research_track03_patient_profit_frontier.py](../scripts/research_track03_patient_profit_frontier.py) (`10,872` rows, `non_flat_count = 92`, `max_mtm_cash_err = 8.46e-12`, `rebate_bps = 0.0`, `4/4` unit tests in [test_research_track03_patient_profit_frontier.py](../scripts/test_research_track03_patient_profit_frontier.py)) revealed the structural driver of Track 03 profitability:

1. **Eliminating Forced IOC Exit Churn (`s: 0.47 → 0.00`):** Short `15 s` hold limits (`S4_speedbump_combined`) forced `46.8%` of `Core 4` RH exits (`46.3%` on `lighter_std` `TT7`; `71%–84%` on `arcus`) to cross the spread via delayed `330 ms` IOC takers. Extending `max_hold_ms` to `600 s` with `reduce_only_penny_ticks = 1` and `inventory_skew_bps_per_full_limit = 5.0` (`P3_patient_600s`) collapsed taker exit share **`s` from `0.47 → 0.00`** (`0.002` on RH `Core 4`, `0.001` on Std `TT7`) and from **`0.71 → 0.01`** on `arcus` `AMZN` (`0.84 → 0.04` on `TT7`), while keeping `stale_exit_blocks = 0`.
2. **`lighter_robinhood` (`0/0` Verified) `Positive-Markout Core 4` (`AAPL, AMZN, META, MSFT`) Generalizes Across Every Split:**
   - `P3_patient_600s` delivered **`-$7.62/M`** (`+$63.73`) in `selection_dev` → **`-$11.48/M`** (`+$24.35`, `-$11.51/M` realized with `$177` residual) in `holdout` → **`-$23.79/M`** (`+$26.31`) in `w26_w27_oos`, totaling **`-$9.87/M` (`+$114.39` net on `$5.79M` exit notional, `95%` positive windows, `24.2 s` avg hold, `PSR = 1.0000`, `DSR_eff = 0.9999`, `CSCV PBO = 0.0000`)**.
   - Causally dev-selected `pos_markout_dev_selected_rwa` (`AAPL, AMZN, META, MSFT, TSLA` on RH) similarly generalized across all splits (`-$6.48/M` → `-$8.52/M` → `-$20.74/M`; `-$8.17/M` overall, `+$121.10` on `$7.41M`, `90%` pos windows, **`PSR = 1.0000`, `DSR_eff = 0.9977`, `CSCV PBO = 0.0000`**).
   - Wider passive quotes (`W5_nopenn_15bp_180s`) boosted RH `Core 4` unit edge to **`-$15.82/M`** (`+$30.50` on `$0.96M`) and turned `AAPL` profitable (`-$6.84/M` vs `+$2.19/M` under `P3`), while `META` (`-$41.90/M`, `+$79.3`), `MSFT` (`-$12.90/M`, `+$32.2`), and `AMZN` (`-$6.52/M`, `+$14.0`) anchored `P3`. Including index ETFs `QQQ` (`+$5.55/M`, `-$62.6`) and `SPY` (`+$6.48/M`, `-$132.1`) kept RH `Top-Tier 7-RWA` slightly net-cost (`+$1.58/M`, `-$73.57`) due to `200 ms` RH adverse selection on fast ETFs.
3. **`lighter_std` (`0/0` Counterfactual) vs. `lighter_prem`, `arcus`, and Crypto Fee Barriers:**
   - Under `lighter_std` (`0/0` counterfactual), `P3_patient_600s` turned profitable across all `Top-Tier 7-RWA` (**`-$10.56/M`**, `+$215.02` on `$10.18M`, `76%` pos windows, `PSR = 1.0000`, `DSR_eff = 1.0000`, `CSCV PBO = 0.0571`) and `Core 4` (**`-$20.46/M`**, `+$173.4` on `$4.24M`, `90%` pos windows, `DSR_eff = 1.0000`; `PBO = 0.5000` solely from competition among profitable `P1–P3/W1–W2/W5` arms).
   - Verified fee-bearing tiers remained strictly net-cost (`NO-GO` without rebates) even under `P3_patient_600s`: `lighter_prem` (`0.40/2.80`: `+$17.80/M` `Core 4` / `+$29.57/M` `TT7`), `arcus` RWA (`0/2.1375`: `+$28.69/M` `AMZN` / `+$72.09/M` `TT7`), and Crypto (`+$15.53/M` `lighter_std`, `+$20.10/M` `lighter_robinhood`, `+$56.03/M` `lighter_prem`).

---

## 5. Cross-Track Quantitative Synthesis & Next Engineering Gates (Under Strict `HOLD`)

### 5.1 Unified Quantitative Lessons Across All Three Tracks

1. **Maker-First on `0/0` Venues (`USDG`/`Std`) vs. Fee Drag (`Prem`/`Arcus`):** Verified `0.00/0.00 bps` `lighter_robinhood` (`USDG`) is the decisive economic enabler: Track 01 `OverreactionReversion` survived `+2 bp` slippage in `holdout` at `0/0` yet lost `-$405.69` across `W01–W27` at `0.40/2.80` Prem; Track 03 `P3_patient_600s` earned `+$114.39` (`-$9.87/M`) on `Core 4` at `0/0` yet lost `-$112.36` (`+$17.80/M`) at `0.40/2.80` Prem and `-$21.2` (`+$28.69/M`) on `Arcus` `AMZN`. Only Track 02 `B0` cleared `Prem` taker fees on `NVDA`-heavy RWA basis.
2. **L1 Depth Clamping, Orphan Accounting & Feed Staleness Modeling:** Unclipped L1 hedge sizing, silent orphan dropping, `0 ms` stale-IOC fills, and un-recentered `12.4 s` snapshots (`trade_xyz`, `59.4%` stale) all produced illusory profits. Fixing both P0 suites collapsed Track 02 `USDG/USDC` `C3_top5` (`DSR_eff` `1.0000 → 0.7673`, `PBO` `0.0286 → 0.3071`) and exposed `24.7%–58.5%` orphan rates in flipped-role `Arcus` Maker → `LighterRH`.
3. **Patience vs. Adverse Selection:** When quoting behind a `200 ms` post-only speedbump on `0/0` RWAs, patience (`max_hold_ms` `15 s → 600 s`) eliminates `46.8%` forced IOC crossing without sacrificing inventory safety (`24.2 s` avg hold on `Core 4`). Conversely, quoting `200 ms` on fast ETFs (`QQQ`, `SPY`) or `0 ms` on price-discovering `Arcus` invites toxic adverse selection that speedbumps cannot absorb.
4. **Single-Name & Regime Concentration:** Headline PnL repeatedly depended on single symbols or regimes: `NVDA` drove `70%–94%` of Track 02 `B0` PnL (`77.5%` dev, `94.0%` holdout, `69.8%` `w26_w27_oos`; `ex-NVDA` flipped `Arcus ↔ LighterRH 1bp` OOS negative), `ETH` drove Track 01 `Prem` Crypto `holdout`, `META` anchored `$79.3 / $114.4` of Track 03 RH `Core 4`, and Track 01 `OverreactionReversion` decayed in `w26_w27_oos` (`-$35.19`) despite strong `w21–w25` `holdout` (`+$125.06`).
5. **Multiple-Testing & Telemetry Discipline:** Evaluating `N_eff`, `DSR_eff`, `MinTRL`, and `CSCV PBO` (with strict `selection_dev → holdout → w26_w27_oos` freezing and `-w28` exclusion) cleanly separated Track 03 `P3_patient_600s` (`DSR_eff = 0.9999`, `PBO = 0.0000`) from overfit per-symbol Track 01 configs (`+$627.37 → -$31.78 → -$42.28`). Missing `Arcus` `funding_and_ctx` telemetry remains a hard blocker on Track 02 promotion.

### 5.2 Actionable Offline, Simulator & Instrumentation Roadmap (Strictly `HOLD`)

No track is cleared for live orders or Tokyo deploys. Before any shadow or future-window gate is reconsidered:

1. **Track 03 Shadow & Rust Parity Gate (`Core 4` Patient Unwind):**
   - Port/verify `P2_patient_180s` / `P3_patient_600s` (`max_hold_ms` `180s–600s`, `reduce_only_penny_ticks = 1`, `inventory_skew_bps_per_full_limit = 4.0–5.0`, and per-symbol `W5`-style `min_entry_edge_bps = 1.5` for `AAPL`) in read-only shadow replay ([track_shadow.rs](../src/strategy/track_shadow.rs)), confirming event-by-event cash, `stale_exit_blocks`, `s <= 0.02`, and `terminal_residual_notional_usd` parity against [research_track03_patient_profit_frontier.py](../scripts/research_track03_patient_profit_frontier.py).
   - Register a **pre-registered future-window validation** (`>= w28` once sealed) testing `Core 4` (`AAPL, AMZN, META, MSFT`) and `Core 4 + TSLA` with strict `QQQ`/`SPY`/`NVDA` exclusion and single-symbol PnL-concentration caps (`META` dependency audit).
2. **Track 02 Instrumentation & Settlement Friction Gate (`B0`):**
   - Instrument `Arcus` `funding_and_ctx` collection in the Tokyo/Aleph lakehouse pipeline (`symbol` + timestamped hourly rates) so `funding_leg_mk_usd` is empirically settled rather than `$0.00` unknown.
   - Measure realized `USDG ↔ USDC` conversion/rebalancing friction (including capital lockup and transfer slippage) to verify whether true all-in FX drag sits strictly below the `~1.36–1.77 bps` OOS breakeven on `Arcus ↔ LighterRH`, and audit `ex-NVDA` `B0` edge stability on `Arcus ↔ Lighter` (`Prem`).
3. **Track 01 Research Freeze & Diagnostic Gate:**
   - Halt further short-horizon parameter/threshold grid expansion on `OracleDiscoveryMomentum` and `OverreactionReversion` (as directed in `ALEPHTX_RESEARCH_AND_EXECUTION_ROADMAP_2026_11_27.md` §2). Keep [directional_alpha.rs](../src/strategy/directional_alpha.rs) disabled (`enabled: false`) in production builds unless a pre-registered regime/participation gate proves positive net cash across fresh `>= w28` windows and survives `+1–2 bps` latency/slippage stress.
4. **Shared Statistical & Ledger Audit Gate:**
   - Run [audit_multiple_testing_overfitting.py](../scripts/audit_multiple_testing_overfitting.py) and independent cash-ledger cross-checks on all newly sealed windows (`>= w28`), tracking `realized_only` vs `mtm_incl_residual`, `N_eff`, `DSR_eff`, `MinTRL`, and `CSCV PBO` without retroactively re-opening frozen splits.
