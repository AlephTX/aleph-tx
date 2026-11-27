# Track 02: P0 Ledger Fixes & Zero-Fee WS Basis Arbitrage (`Arcus ↔ LighterRH` & `Arcus ↔ Lighter`, `W01–W27`)

**Date:** `2026-11-27` (CST) · **Operational Verdict:** **`HOLD`** (no live orders/deploys; `Arcus ↔ LighterRH` flipped-role fails OOS; `Arcus ↔ Lighter` `USDC/USDC` `B0` is positive across all splits pending `Arcus` `funding_and_ctx` telemetry)
**Owned Files:**
- [scripts/research_funding_basis_carry_portfolio.py](../scripts/research_funding_basis_carry_portfolio.py) (3 P0 ledger fixes)
- [scripts/test_research_funding_basis_carry_portfolio.py](../scripts/test_research_funding_basis_carry_portfolio.py) (8 stdlib-only unit tests including 3 P0 counterexample tests)
- [scripts/research_track02_arcus_rh_zero_fee_basis.py](../scripts/research_track02_arcus_rh_zero_fee_basis.py) (W01–W27 P0-clean evaluator)
- [scripts/test_research_track02_arcus_rh_zero_fee_basis.py](../scripts/test_research_track02_arcus_rh_zero_fee_basis.py) (4 stdlib-only unit tests)
- [docs/TRACK02_ZERO_FEE_WS_BASIS_AND_P0_LEDGER_FIX_2026_11_27.md](TRACK02_ZERO_FEE_WS_BASIS_AND_P0_LEDGER_FIX_2026_11_27.md)

---

## 1. Executive Summary

1. **All 3 P0 Ledger Defects Fixed & Verified (`research_funding_basis_carry_portfolio.py`):**
   - Fixed unclipped hedge depth (`min(filled_notl, max(h_l1, filled_notl))` $\to$ `min(filled_notl, h_l1)`), explicit orphan-leg booking/unwind with realistic slippage & dark-book residual retention, and per-leg fill/exit timestamps (`leg_a_fill_ms`, `leg_b_fill_ms`) enforcing strict `settlement_ms > leg_fill_ms` funding accrual.
   - **Before/After P0 Impact (`W01–W26`):** The previously reported `CONDITIONAL` edge in `USDG/USDC` RWA (`lighter_robinhood ↔ trade_xyz`) collapses once unhedged passive fills are accounted for: at `0 bp` FX, `C2` drops **`+$10.54 → +$3.46`** (`selection_dev`, `8` orphans totaling `$3,805.31`, `-$4.25` orphan PnL) and `C3_top5` drops **`+$11.33/+$2.23/+$0.36 → +$9.42/$0.00/$0.00`** (`DSR_eff` `1.0000 → 0.7673`, `PBO` `0.0286 → 0.3071`). At `1 bp` FX, `C3_top5` turns **`-$2.05/$0.00/$0.00`** (`DSR_eff` `0.9963 → 0.3931`, `PBO` `0.0714 → 0.4214`).
2. **Flipped-Role Zero-Fee `Arcus` Maker (`0.00 bps`) $\to$ `LighterRH` Hedge (`0.00 bps`) Fails OOS (`W01–W27`):**
   - Although `Arcus` RWA (`0.00 / 2.1375 bps`) + `LighterRH` (`0.00 / 0.00 bps`) yields **0.00 bps all-in fees** when `Arcus` is Maker (`36 ms` print-verified) and demeaned `(b - 2h_MA)` basis reverts strongly (`corr15 = -0.450` to `-0.671`, `corr60 = -0.669` to `-0.756` across all 14 RWA symbols), **every flipped-role arm (`A0`, `A1`, `A2`) loses money across `selection_dev` and `holdout`** under P0-clean L1 depth clamping & orphan unwind (`A0`: `-$77.35 / -$13.30 / +$0.90` at `0 bp` FX with `51.32%` orphan rate; `A1` passive-hedge fallback: `-$80.37 / -$28.22 / -$0.42`; `A2` reversion: `-$319.72 / -$19.74 / $0.00`).
   - **Root Cause:** `Arcus` (`0 ms` speedbump, tight `0.15–2.27 bps` spreads) is the fast price-discovery venue. Making passively on `Arcus` and hedging `330 ms` later on `LighterRH` (or `200 ms + 1.5 s` passive hedge) suffers adverse selection and `24.7%–58.5%` unhedged orphan rates when `LighterRH` L1 shifts or thins.
3. **Speedbump Maker $\to$ Fast `Arcus` Taker (`B0_hedge_mk_arcus_tk`) Outperforms Across `USDC/USDC` & `1 bp` `USDG/USDC`:**
   - **`Arcus ↔ Lighter` (`USDC/USDC`, verified `Lighter` Prem `0.40/2.80` + `Arcus` `0.00/2.1375`, 15 RWA symbols):** `B0` (`b0_e10_v0.0_sh0`) achieves **`+$480.60` (`+3.51 bps`, realized `+$480.45`)** on `selection_dev (15w)` $\to$ **`+$21.55` (`+1.82 bps`, `173` fills)** on frozen `holdout (4w)` $\to$ **`+$2.95` (`+1.71 bps`, `24` fills)** on `w26_w27_oos (2w)` after paying `$459.88` total fees (`PSR=1.0000`, `DSR_eff=0.9997`, **`PBO=0.0143`**; `ex-NVDA`: `+$108.28 / +$1.29 / +$0.89`). Under `Lighter` Std (`0/0` counterfactual), `B0` earns **`+$747.98 / +$47.76 / +$6.63`** (RWA) and **`+$150.03 / +$22.54 / +$8.22`** (Crypto, `PBO=0.0000`).
   - **`Arcus ↔ LighterRH` (`USDG/USDC`, 14 RWA symbols):** `B0` (`b0_e10_v1.0_sh2`) remains positive across all splits at `1 bp` FX (**`+$159.34 / +$4.85 / +$0.26`**, `PSR=0.9944`, `DSR_eff=0.9390`, **`PBO=0.0000`**), but turns OOS negative at `2 bp` (`+$119.38 / -$1.47 / -$0.46`) and `3 bp` (`+$79.42 / -$7.79 / -$1.18`), putting OOS FX breakeven at **`~1.77 bps`**.

---

## 2. Track 02 P0 Ledger Fixes & Before/After Impact (`W01–W26`)

In accordance with [ALEPHTX_RESEARCH_AND_EXECUTION_ROADMAP_2026_11_27.md §3](ALEPHTX_RESEARCH_AND_EXECUTION_ROADMAP_2026_11_27.md), all 3 P0 ledger defects in [scripts/research_funding_basis_carry_portfolio.py](../scripts/research_funding_basis_carry_portfolio.py) were fixed and verified with deterministic counterexample tests in [scripts/test_research_funding_basis_carry_portfolio.py](../scripts/test_research_funding_basis_carry_portfolio.py):

| P0 Defect | Root Bug Before Fix | Deterministic Fix Applied | Regression Test |
| --- | --- | --- | --- |
| **1. Unclipped Hedge L1 Depth** | `exec_notl = min(filled_notl, max(h_l1, filled_notl))` always evaluated to `filled_notl` and never truncated to `h_l1`. | Clamped matched notional to `hedged_notl = min(filled_notl, h_l1)` and routed `filled_notl - hedged_notl` into explicit orphan tracking. | `test_p0_defect1_hedge_depth_clamping_and_partial_orphan_unwind` |
| **2. Dropped Orphan A-Leg** | Hedge rejection (`adv_move`, `h_l1 < $100`, missing `hq`) or partial clip silently dropped executed passive exposure. | Explicit `_book_orphan_fill` / `_try_unwind_orphan` tracks `orphan_entries`, `orphan_notional_usd`, taker unwind (`t_hedge + 330ms`), L1 sweep, maker+taker fees, single-leg funding, and dark-book `terminal_residual_notional_usd`. | `test_p0_defect2_orphan_leg_booked_and_unwound_on_hedge_reject_and_missing_book` |
| **3. Atomic Entry / Non-Strict Funding** | Used single `entry_ms = t_hedge` with `entry_ms <= ht`, missing passive-to-hedge latency skew and violating strict `>`. | Tracked per-leg `leg_a_fill_ms`, `leg_b_fill_ms`, `leg_a_exit_ms`, `leg_b_exit_ms` via `_settle_single_leg_funding` strictly requiring `ht > leg_fill_ms`. | `test_p0_defect3_per_leg_fill_timestamps_and_strict_settlement_skew` |

### Before vs. After P0 Re-Run (`USDG/USDC` RWA, `lighter_robinhood ↔ trade_xyz`, Verified Fees)

Comparing the pre-fix baseline (`track02_funding_basis_carry_w01_w26.json`) against the post-P0 rerun on Aleph:

| FX | Arm | Split | Pre-P0 Net ($) | Post-P0 Net ($) | Orphans (`Notl $`, `Orp PnL $`) | Clipped Entries | Pre $\to$ Post `DSR_eff` / `CSCV PBO` |
| --- | --- | --- | ---: | ---: | --- | ---: | --- |
| `0 bp` | `C2` | `selection_dev` | `+10.5381` | **`+3.4613`** | `8` (`$3,805.31`, `-$4.2511`) | `39` | — |
| `0 bp` | `C2` | `holdout` | `+1.0233` | **`+0.8827`** | `1` (`$185.73`, `-$0.0246`) | `3` | — |
| `0 bp` | `C2` | `w26_oos` | `+0.1646` | **`+0.1646`** | `0` (`$0.00`, `$0.0000`) | `0` | — |
| `0 bp` | `C3_top5` | `dev / hold / w26` | `+11.33 / +2.23 / +0.36` | **`+9.42 / 0.00 / 0.00`** | — | — | `1.0000 → 0.7673` / `0.0286 → 0.3071` |
| `1 bp` | `C2` | `dev / hold / w26` | `+7.24 / +0.81 / +0.06` | **`+1.72 / 0.00 / 0.00`** | `3` (`$1,531.41`, `-$1.4797`) | `8` | — |
| `1 bp` | `C3_top5` | `dev / hold / w26` | `+8.37 / +1.77 / +0.14` | **`-2.05 / 0.00 / 0.00`** | — | — | `0.9963 → 0.3931` / `0.0714 → 0.4214` |
| `3 bp` | `C2` | `dev / hold / w26` | `+5.75 / +0.08 / 0.00` | **`+1.90 / 0.00 / 0.00`** | `1` (`$101.68`, `-$0.0191`) | `5` | — |
| `3 bp` | `C3_top5` | `dev / hold / w26` | `+0.48 / +0.19 / 0.00` | **`+1.17 / 0.00 / 0.00`** | — | — | `0.3266 → 0.3848` / `0.6571 → 0.4895` |

Other cohorts shifted consistently: `same_risk_domain` RWA `C2` `holdout` flipped **`+$1.6939 → -$0.4039`** (`2` orphans, `-$0.7974`), and `standard_zero USDC_USDC_distinct` RWA `C2` dropped **`-$15.7215 → -$22.8053`** (`dev`, `19` orphans) and **`$0.00 → -$1.2333`** (`holdout`, `1` orphan).

---

## 3. Track 02 Zero-Fee WS Basis Arbitrage Methodology (`W01–W27`)

Implemented in [scripts/research_track02_arcus_rh_zero_fee_basis.py](../scripts/research_track02_arcus_rh_zero_fee_basis.py) (pure stdlib top-level imports; `duckdb` & `numpy` lazy-loaded; `NOT regexp_matches(campaign_id, '-w28$')` enforced everywhere):

- **Fee & Latency Schedules** (from `.agent-state/research/account_fee_evidence_20261127.json` + [src/venues.rs](../src/venues.rs)):
  - `arcus` (`USDC`, `ws_50ms`, `36 ms` order delay): verified `level_0` w/ 5% discount — **RWA `0.00 / 2.1375 bps`**, **Crypto `1.425 / 4.275 bps`**.
  - `lighter_robinhood` (`USDG`, `ws_50ms`, `200 ms` post-only / `330 ms` taker): verified **`0.00 / 0.00 bps`**.
  - `lighter` (`USDC`, `ws_50ms`): verified **`Prem` `0.40 / 2.80 bps`** (`40 ms` maker / `140 ms` taker) vs counterfactual **`Std` `0.00 / 0.00 bps`** (`40 ms` maker / `330 ms` taker).
- **Universe & Splits (`W01–W27`, excluding `w08` crypto and `-w28`):**
  - `Arcus ↔ LighterRH` (`USDG/USDC`): **14 RWA** (`AAPL, AMD, AMZN, COIN, CRCL, GOOGL, META, MSFT, MU, NVDA, PLTR, QQQ, SPY, TSLA`) + **3 Crypto** (`BTC, ETH, SOL`).
  - `Arcus ↔ Lighter` (`USDC/USDC`): **15 RWA** (same 14 ex-`CRCL` + `HOOD, MSTR`) + **3 Crypto**.
  - **Splits:** `selection_dev` (`w01–w20`: 15 RWA / 19 Crypto), frozen `holdout` (`w21–w25`: 4 RWA / 5 Crypto), `w26_w27_oos` (`w26–w27`: 2 RWA / 2 Crypto).
- **Pre-Registered Arms (`tau_s = 7200s` = `2h_MA`, `$1,000` clip, `4x` max inventory, `$1,000` min L1 hedge guard, `2 bp` 5s fast-drift guard, P0 L1 clamp + orphan unwind + FIFO per-leg funding):**
  - `A0_arcus_mk_hedge_tk`: Flipped-role `Arcus` Maker (`36 ms`) $\to$ Hedge Taker.
  - `A1_arcus_mk_hedge_mk_fb`: Flipped-role `Arcus` Maker (`36 ms`) $\to$ Hedge Maker (`200 ms` RH / `40 ms` Lighter, `1.5 s` passive wait) w/ Taker fallback.
  - `A2_arcus_mk_demeaned_reversion`: Flipped-role `(b - 2h_MA)` reversion entry/exit (`1 h` max hold).
  - `B0_hedge_mk_arcus_tk`: Speedbump/Hedge Maker (`LighterRH` `200 ms` / `Lighter` `40 ms`) $\to$ Fast `Arcus` Taker (`36 ms`).
- **Funding Data Caveat:** Aleph lakehouse contains `235–293` hourly funding slots per symbol in `funding_and_ctx` for `lighter_robinhood` and `lighter` (keyed by symbol name, e.g. `NVDA`), but **zero `funding_and_ctx` files for `arcus`**. Thus `funding_leg_mk_usd` on `Arcus` is unknown (`$0.00` reported), while `LighterRH`/`Lighter` funding leg is settled strictly on `ht > leg_fill_ms`.

---

## 4. Empirical Results (`W01–W27`)

### 4.1 `Arcus ↔ LighterRH` (`USDG/USDC`, 14 RWA & 3 Crypto, FX Drag `{0, 1, 2, 3} bp`)

| FX | Domain | Arm (`selection_dev` Winner) | Split | Fills | Maker Notl ($) | Hedged Notl ($) | Orphan Rate (`Orp PnL $`) | Fees ($) | FX ($) | Funding ($) | Realized ($) | MTM ($) (`bps`) | `ex-NVDA` ($) | Peak ($) | Hold (`m`) |
| --- | --- | --- | --- | ---: | ---: | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `0 bp` | `rwa` | `A0` (`a0_e15_v1.0`) | `dev / hold / oos` | `225 / 14 / 1` | `158,302 / 6,641 / 1,000` | `77,058 / 3,701 / 1,000` | `51.3% (-79.22) / 44.3% (-7.60) / 0% (0)` | `23.99 / 1.14 / 0.22` | `0.00` | `-2.05 / +0.02 / -0.21` | `-77.35 / -13.30 / +0.90` | **`-77.35 / -13.30 / +0.90`** (`-4.89 / -20.03 / +8.98`) | `-8.06 / -7.85 / +0.90` | `3,596` | `158` |
| `0 bp` | `rwa` | `A1` (`a1_e12_v1.0`) | `dev / hold / oos` | `317 / 36 / 2` | `228,580 / 20,841 / 1,407` | `172,087 / 14,273 / 1,407` | `24.7% (-11.91) / 31.5% (-10.79) / 0% (0)` | `22.21 / 2.75 / 0.31` | `0.00` | `-3.59 / +0.05 / -0.22` | `-80.37 / -28.22 / -0.42` | **`-80.37 / -28.22 / -0.42`** (`-3.52 / -13.54 / -2.95`) | `-84.96 / -16.34 / -0.42` | `4,265` | `189` |
| `0 bp` | `rwa` | `A2` (`a2_dev8_edge3`) | `dev / hold / oos` | `2591 / 68 / 0` | `438,711 / 26,582 / 0` | `182,187 / 15,528 / 0` | `58.5% (-269.46) / 41.6% (-12.96) / —` | `55.31 / 2.71 / 0.00` | `0.00` | `-0.69 / +0.01 / 0.00` | `-319.72 / -19.74 / 0.00` | **`-319.72 / -19.74 / 0.00`** (`-7.29 / -7.43 / 0.00`) | `-203.45 / -10.00 / 0.00` | `1,000` | `9` |
| `0 bp` | `rwa` | `B0` (`b0_e4_v1.0_sh2`) | `dev / hold / oos` | `5265 / 1281 / 189` | `3,416,152 / 742,209 / 111,711` | `2,599,435 / 546,478 / 88,334` | `23.9% (-412.32) / 26.4% (-103.15) / 20.9% (-11.78)` | `582.31 / 123.29 / 23.03` | `0.00` | `+2.00 / +0.22 / +0.46` | `+233.35 / -22.57 / -5.97` | **`+233.35 / -22.57 / -5.97`** (`+0.68 / -0.30 / -0.54`) | `+80.65 / -30.38 / -8.30` | `4,941` | `98` |
| `1 bp` | `rwa` | **`B0` (`b0_e10_v1.0_sh2`)** | `dev / hold / oos` | `682 / 114 / 11` | `452,217 / 70,481 / 6,330` | `319,674 / 48,643 / 4,330` | `29.3% (-56.57) / 31.0% (-15.94) / 31.6% (-0.87)` | `85.36 / 13.50 / 1.53` | `39.96 / 6.32 / 0.72` | `+2.00 / -0.13 / -0.07` | `+159.34 / +4.85 / +0.26` | **`+159.34 / +4.85 / +0.26`** (`+3.52 / +0.69 / +0.41`) | `+61.57 / -3.62 / -0.19` | `4,631` | `206` |
| `2 bp` | `rwa` | `B0` (`b0_e10_v1.0_sh2`) | `dev / hold / oos` | `682 / 114 / 11` | `452,217 / 70,481 / 6,330` | `319,674 / 48,643 / 4,330` | `29.3% / 31.0% / 31.6%` | `85.36 / 13.50 / 1.53` | `79.92 / 12.63 / 1.43` | `+2.00 / -0.13 / -0.07` | `+119.38 / -1.47 / -0.46` | **`+119.38 / -1.47 / -0.46`** (`+2.64 / -0.21 / -0.73`) | `+37.32 / -8.26 / -0.58` | `4,631` | `206` |
| `3 bp` | `rwa` | `B0` (`b0_e10_v1.0_sh2`) | `dev / hold / oos` | `682 / 114 / 11` | `452,217 / 70,481 / 6,330` | `319,674 / 48,643 / 4,330` | `29.3% / 31.0% / 31.6%` | `85.36 / 13.50 / 1.53` | `119.88 / 18.95 / 2.15` | `+2.00 / -0.13 / -0.07` | `+79.42 / -7.79 / -1.18` | **`+79.42 / -7.79 / -1.18`** (`+1.76 / -1.10 / -1.86`) | `+13.08 / -12.89 / -0.97` | `4,631` | `206` |
| `0 bp` | `crypto` | `A0` (`a0_e15_v1.0`) | `dev / hold / oos` | `106 / 14 / 0` | `91,446 / 12,896 / 0` | `21,807 / 1,454 / 0` | `76.2% (-10.34) / 88.7% (-1.04) / —` | `46.39 / 7.12 / 0.00` | `0.00` | `-0.01 / 0.00 / 0.00` | `-9.96 / -1.63 / 0.00` | **`-9.96 / -1.63 / 0.00`** (`-1.09 / -1.26 / 0.00`) | — | `2,789` | `108` |
| `0 bp` | `crypto` | `B0` (`b0_e10_v1.0_sh2`) | `dev / hold / oos` | `282 / 60 / 11` | `159,685 / 37,045 / 8,282` | `128,338 / 30,971 / 5,120` | `19.6% (-39.81) / 16.4% (-8.96) / 38.2% (-2.21)` | `66.83 / 17.80 / 2.89` | `0.00` | `+0.54 / -0.18 / +0.01` | `-11.42 / -15.29 / -2.26` | **`-11.42 / -15.29 / -2.26`** (`-0.71 / -4.13 / -2.73`) | — | `4,146` | `182` |

- **Terminal Flatness:** All `Arcus ↔ LighterRH` splits across all FX scenarios have `terminal_flat_all = True` (`terminal_residual_notional_usd = $0.00`).
- **FX Drag Breakeven:** For `B0` (`b0_e4_v1.0_sh2`) on `selection_dev`, linear FX breakeven is **`3.153 bps`**; for `b0_e10_v1.0_sh2`, `selection_dev` stays positive through `3 bp` (`+$79.42`), while `holdout` crosses `$0` at **`~1.77 bps`** (`+$4.85` at `1 bp` vs `-$1.47` at `2 bp`) and `w26_w27_oos` crosses `$0` at **`~1.36 bps`** (`+$0.26` at `1 bp` vs `-$0.46` at `2 bp`).

### 4.2 `Arcus ↔ Lighter` (`USDC/USDC`, Zero FX Drag, `Prem 0.40/2.80` vs `Std 0.00/0.00` Counterfactual)

| Regime | Domain | Arm (`selection_dev` Winner) | Split | Fills | Maker Notl ($) | Hedged Notl ($) | Orphan Rate (`Orp PnL $`) | Fees ($) | Funding ($) | Realized ($) | MTM ($) (`bps`) | `ex-NVDA` ($) | Flat (`Res $`) | Peak ($) |
| --- | --- | --- | --- | ---: | ---: | ---: | --- | ---: | ---: | ---: | ---: | ---: | --- | ---: |
| `Prem` (`0.4/2.8`) | `rwa` | `A0` (`a0_e15_v1.0`) | `dev / hold / oos` | `150 / 14 / 1` | `109,730 / 8,740 / 1,000` | `81,613 / 7,009 / 1,000` | `25.6% (+37.05) / 19.8% (+0.84) / 0%` | `43.03 / 5.54 / 0.79` | `-1.14 / -0.02 / +0.01` | `+81.61 / -4.67 / -0.00` | **`+81.61 / -4.67 / -0.00`** (`+7.44 / -5.34 / -0.01`) | `+34.35 / -2.29 / -0.00` | `True (0)` | `4,762` |
| `Prem` (`0.4/2.8`) | `rwa` | `A1` (`a1_e12_v1.0`) | `dev / hold / oos` | `261 / 25 / 5` | `180,708 / 16,778 / 3,710` | `151,199 / 10,918 / 1,793` | `16.3% (+15.83) / 34.9% (-2.88) / 51.7% (-0.17)` | `65.71 / 8.23 / 1.57` | `-1.28 / -0.17 / -0.02` | `-89.56 / -26.19 / -1.21` | **`-89.56 / -26.19 / -1.21`** (`-4.96 / -15.61 / -3.26`) | `-88.33 / -23.83 / +0.11` | `True (0)` | `4,919` |
| `Prem` (`0.4/2.8`) | `rwa` | `A2` (`a2_dev8_edge3`) | `dev / hold / oos` | `1924 / 58 / 2` | `293,300 / 15,021 / 1,209` | `182,221 / 9,146 / 1,209` | `37.9% (-58.38) / 39.1% (-7.43) / 0%` | `80.55 / 4.12 / 0.73` | `-0.71 / -0.01 / 0.00` | `-139.20 / -11.76 / -2.07` | **`-139.20 / -11.76 / -2.07`** (`-4.75 / -7.83 / -17.14`) | `-112.97 / -6.84 / -2.07` | `True (0)` | `1,000` |
| `Prem` (`0.4/2.8`) | `rwa` | **`B0` (`b0_e10_v0.0_sh0`)** | `dev / hold / oos` | `1816 / 173 / 24` | `1,368,164 / 118,046 / 17,255` | `1,020,191 / 89,596 / 15,679` | `25.4% (-79.73) / 24.1% (-18.62) / 9.1% (-0.23)` | `411.17 / 41.35 / 7.36` | `-2.01 / +0.31 / +0.18` | `+480.45 / +21.55 / +2.95` | **`+480.60 / +21.55 / +2.95`** (`+3.51 / +1.82 / +1.71`) | `+108.28 / +1.29 / +0.89` | `False* (1000)` / `True` / `True` | `4,990` |
| `Prem` (`0.4/2.8`) | `crypto` | `A0` (`a0_e15_v1.0`) | `dev / hold / oos` | `157 / 24 / 2` | `140,945 / 21,874 / 1,966` | `102,952 / 16,553 / 0` | `27.0% (-4.39) / 24.3% (+2.82) / 100% (-0.46)` | `84.63 / 18.08 / 1.12` | `-1.11 / +0.11 / 0.00` | `+52.95 / +4.06 / -0.46` | **`+52.95 / +4.06 / -0.46`** (`+3.76 / +1.85 / -2.33`) | — | `True (0)` | `4,798` |
| `Prem` (`0.4/2.8`) | `crypto` | `B0` (`b0_e10_v0.0_sh0`) | `dev / hold / oos` | `807 / 231 / 35` | `681,056 / 196,474 / 28,363` | `535,447 / 146,833 / 22,533` | `21.4% (-72.18) / 25.3% (-18.15) / 20.6% (+0.42)` | `324.93 / 99.81 / 14.04` | `+0.63 / +0.09 / +0.01` | `+70.80 / -5.30 / +4.80` | **`+70.80 / -5.30 / +4.80`** (`+1.04 / -0.27 / +1.69`) | — | `True (0)` | `4,876` |
| `Std` (`0/0` cf.) | `rwa` | **`B0` (`b0_e4_v1.0_sh0`)** | `dev / hold / oos` | `4350 / 898 / 136` | `2,983,266 / 580,791 / 92,960` | `2,299,819 / 415,411 / 73,863` | `22.9% (-74.30) / 28.5% (-56.48) / 20.5% (-10.29)` | `516.25 / 95.49 / 17.85` | `+2.89 / +0.01 / -0.47` | `+747.23 / +47.76 / +6.63` | **`+747.98 / +47.76 / +6.63`** (`+2.51 / +0.82 / +0.71`) | `+253.06 / +16.11 / +7.99` | `False* (1736)` / `True` / `True` | `4,997` |
| `Std` (`0/0` cf.) | `crypto` | **`B0` (`b0_e10_v0.0_sh0`)** | `dev / hold / oos` | `807 / 231 / 35` | `681,056 / 196,474 / 28,363` | `535,447 / 146,833 / 22,533` | `21.4% (-25.59) / 25.3% (-2.27) / 20.6% (+2.28)` | `245.70 / 71.97 / 10.62` | `+0.63 / +0.09 / +0.01` | `+150.03 / +22.54 / +8.22` | **`+150.03 / +22.54 / +8.22`** (`+2.20 / +1.15 / +2.90`) | — | `True (0)` | `4,876` |

*\*Note on `selection_dev` terminal residual:* `MSFT` (`Prem`, `$1,000` residual, `+$0.15` MTM vs realized) and `MSFT`/`GOOGL` (`Std`, `$1,736` residual, `+$0.75` MTM vs realized) had stale end-of-window books in `selection_dev`; `holdout` and `w26_w27_oos` are 100% `terminal_flat_all = True` (`$0.00` residual).

### 4.3 Multiple-Testing & Overfitting Audit (`PSR / N_eff / DSR / CSCV PBO` Across `W01–W27`)

| Cohort | Domain | Trials (`Var/Raw`) | `N_eff` | Best Trial | `SR` | `PSR(0)` | `DSR(N_eff)` | `CSCV PBO` |
| --- | --- | ---: | ---: | --- | ---: | ---: | ---: | ---: |
| `arcus_rh_usdg_usdc` (`0 bp`) | `rwa` | `42 / 42` | `1.6536` | `B0:b0_e8_v0.0_sh2` | `0.4990` | `0.9999` | **`0.9953`** | **`0.0000`** |
| `arcus_rh_usdg_usdc` (`1 bp`) | `rwa` | `42 / 42` | `1.6888` | `B0:b0_e10_v0.0_sh2` | `0.3681` | `0.9944` | **`0.9390`** | **`0.0000`** |
| `arcus_rh_usdg_usdc` (`2 bp`) | `rwa` | `42 / 42` | `1.6993` | `B0:b0_e10_v1.0_sh2` | `0.2242` | `0.9663` | `0.7351` | `0.0143` |
| `arcus_rh_usdg_usdc` (`3 bp`) | `rwa` | `42 / 42` | `1.6747` | `B0:b0_e10_v1.0_sh2` | `0.1497` | `0.8317` | `0.5097` | `0.0429` |
| `arcus_rh_usdg_usdc` (`0 bp`) | `crypto` | `42 / 42` | `1.9968` | `A0:a0_e15_v1.0` | `-0.2193` | `0.1000` | `0.0086` | `0.0429` |
| `arcus_lighter_usdc_usdc_prem` | `rwa` | `42 / 42` | `1.5930` | `B0:b0_e8_v0.0_sh2` | `0.4151` | `1.0000` | **`0.9997`** | **`0.0143`** |
| `arcus_lighter_usdc_usdc_prem` | `crypto` | `42 / 42` | `2.0691` | `A0:a0_e15_v1.0` | `0.4576` | `1.0000` | `0.6963` | `0.0000` |
| `arcus_lighter_usdc_usdc_std` | `rwa` | `42 / 42` | `1.5545` | `B0:b0_e6_v0.0_sh2` | `0.5728` | `1.0000` | **`1.0000`** | **`0.0000`** |
| `arcus_lighter_usdc_usdc_std` | `crypto` | `42 / 42` | `2.2572` | `B0:b0_e10_v0.0_sh0` | `0.4410` | `1.0000` | `0.2553` | `0.0000` |

---

## 5. Verification, Redacted Artifacts & Honest Caveats

- **Redacted Output JSON** (outside Git): `.agent-state/research/profitability_upgrade_20261127/track02_arcus_rh_zero_fee_basis_w01_w27.json`
  - `sha256`: `05da725f8df4b58d274624ecc141e38bf62dade10b4bc58f08f5514f4c693e76`
  - `SECRET_HITS = 0` (zero account IDs, keys, mnemonics, or local private paths).
- **Checks Executed:**
  - `python3 -S -m unittest -v scripts/test_research_funding_basis_carry_portfolio.py` (`8/8` OK, including isolated subprocess blocking `numpy,duckdb,pyarrow,pandas,scipy` + 3 P0 counterexamples).
  - `python3 -S scripts/research_funding_basis_carry_portfolio.py --self-test` (`self_test: ok`).
  - `python3 -S -m unittest -v scripts/test_research_track02_arcus_rh_zero_fee_basis.py` (`4/4` OK, including isolated subprocess stdlib check + FX clone parity).
  - `python3 -S scripts/research_track02_arcus_rh_zero_fee_basis.py --self-test` (`self_test: ok`).
  - Markdown link check & `git diff --check` clean.
- **Why Operational Status Remains `HOLD`:**
  1. `Arcus` has zero `funding_and_ctx` data in the Aleph lakehouse, leaving the `Arcus` leg funding carry unverified (though `100–206 min` median hold times limit hourly exposure).
  2. `Arcus ↔ Lighter` (`Prem`) RWA OOS profitability (`+$21.55` holdout / `+$2.95` w26_w27_oos) is concentrated in `NVDA` (`ex-NVDA` `+$1.29` / `+$0.89`), while `Arcus ↔ LighterRH` requires USDG/USDC conversion cost strictly `< 1.77 bps` and flips negative in the flipped zero-fee role (`A0/A1/A2`).
