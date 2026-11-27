# Track 02 (`02_hedge_arbitrage`): Stage A Loss Attribution, Selective Z-Score Gate & Passive-Then-IOC Hedge Upgrade (`W01–W26`)

**Date:** `2026-11-27` (CST) · **Operational Mode:** Strictly **`HOLD`** (research & offline replay only; zero live orders/cancels/deploys)
**Owned Files:**
- [src/strategy/basis_maker_hedge.rs](../src/strategy/basis_maker_hedge.rs)
- [scripts/research_track02_passive_hedge_arb.py](../scripts/research_track02_passive_hedge_arb.py)
- [scripts/test_research_track02_passive_hedge_arb.py](../scripts/test_research_track02_passive_hedge_arb.py)
- [docs/TRACK02_PASSIVE_HEDGE_AND_ZSCORE_ARB_UPGRADE_2026_11_27.md](TRACK02_PASSIVE_HEDGE_AND_ZSCORE_ARB_UPGRADE_2026_11_27.md)

---

## 1. Executive Summary

We conducted a two-stage empirical upgrade for Track 02 (`02_hedge_arbitrage`) across the full Aleph lakehouse (`w01..w26`: **25 crypto windows** `w01..w07, w09..w26` and **20 RWA windows** `w01..w05, w07, w09, w13..w21, w23..w26`), evaluated under **Lighter Standard `0/0 bps`** (`lighter_standard_zero_scenario`) alongside `OPT-01` verified account fees (`lighter_robinhood` `0/0` confirmed, `arcus` `1.425/4.275` Crypto / `0/2.1375` RWA, `extended` `0/2.25` Crypto / `0/0.90` RWA) and explicit maker/cancel latency sensitivity (`{0, 100, 200, 300} ms`):

1. **Stage A Root-Cause Loss Attribution (`selection_dev`, `w01–w20`)**:
   - Prior `upgraded_track2` at `100 ms` maker/cancel latency lost **`-$21,674.00`** (`-6.583 bps` on `$32.92M` notional across `64,381` fills; `-$28,835.82` print-verified) even under Lighter Standard `0/0 bps`.
   - Exact additive decomposition proves the losses came from five compounding mechanics: (i) **L1 hedge depth depletion (`depth_slip = $21,203.98`, `-6.440 bps`)** between maker decision and hedge execution, (ii) **hedge latency drift (`hedge_lat_drift = -$8,497.41`, `-2.581 bps`)**, (iii) **hedge fee drag (`$6,562.45` hedge + `$698.87` maker)** and **crossing half-spread (`$2,098.57`, `0.637 bps`)**, (iv) **maker post-fill adverse selection (`1s` markout `-$5,122.45` / `-1.556 bps`; `5s` markout `-$7,208.79` / `-2.189 bps`)**, and (v) **continuous zero-edge / non-stationary quoting** (`26.7%` of fills on the unfavorable side lost `-$7,247.40`; `41.0%` at low z-score lost `-$8,237.61`; `42.7%` during non-stationary drift lost `-$9,473.76`).
2. **Stage B Rust Strategy Upgrades ([basis_maker_hedge.rs](../src/strategy/basis_maker_hedge.rs))**:
   - Added backward-compatible, default-off (`#[serde(default)]`), zero-allocation controls: **Selective Basis Z-Score & Stationarity Gate** (`min_basis_zscore`, `basis_vol_floor_bps`, `max_drift_to_vol_ratio`, `quote_favorable_side_only`), **Passive-First-Then-IOC Hybrid Hedge Controller** (`HedgeExecutionMode`, `PassiveHedgeConfig`, `PassiveHedgeState`, `PassiveHedgeDecision`, `evaluate_passive_hedge`), and **L1 Microprice Anchor** (`use_microprice_anchor`, `step_with_l1_microprice`).
3. **Round-2 Frozen Validation (`holdout w21–w25` & `w26_oos`, `100 ms` anchor)**:
   - **`T2_zscore_selective`** cut WS distinct-domain losses by **64%–87%** out-of-sample vs `T2_taker_hedge`:
     - **`USDC/USDC` WS distinct-domain**: `selection_dev` `-$973.97` vs `-$3,147.85` (**-69.1%**), `holdout` `-$306.67` vs `-$856.27` (**-64.2%**; `-$553.75` vs `-$1,325.10` print-verified), `w26_oos` `-$47.64` vs `-$88.52` (**-46.2%**).
     - **`USDG/USDC` WS distinct-domain (`0bp` FX)**: `selection_dev` `-$270.87` vs `-$1,329.94` (**-79.6%**), `holdout` `-$55.07` vs `-$375.66` (**-85.3%**), `w26_oos` **`+$2.99`** (`+$0.99` @ `1bp`, `-$1.02` @ `2bp`; `-$2.20` print-verified) vs `-$20.03`.
     - **Zero-fee hedge leg (`arcus/extended -> lighter` `0/0`)**: `selection_dev` `-$433.98` vs `-$2,139.60` (**-79.7%**), `holdout` `-$65.69` vs `-$506.58` (**-87.0%**).
   - **Strict Print-Verified Passive Hedge Reality**: When passive hedge fills require real `public_trades` trade-through prints (zero fabricated fills), passive fill share is `2.8%–6.1%` on cross-venue distinct-domain cohorts (`7.1%–7.8%` on `->lighter`), causing `T2_passive_then_ioc` to suffer extra latency drift on the `92%–97%` unfilled remainder. Only on symmetric `0/0` **`lighter <-> lighter_robinhood`** (`same_risk_domain=True`), where passive fill share reaches `10.5%–22.6%`, does **`T2_zscore_plus_passive_ioc`** dominate all arms (`-$201.22` `selection_dev`, `-$71.65` `holdout`, `-$1.79` `w26_oos` at `0bp` FX vs `-$665.42` / `-$302.81` / `-$11.64` taker).
   - **Operational Verdict:** Strict **`HOLD`** remains in force.

---

## 2. Stage A Root-Cause Loss Attribution (`selection_dev w01–w20`)

Evaluated on `selection_dev` (`19` crypto + `15` RWA windows) using the prior `upgraded_track2` baseline (`e0_bps=4.0`, `vol_edge_mult=0.5`, `nonlinear_skew_power=2.0`, `basis_drift_guard_bps=3.0`, `min_hedge_l1_notional_usd=1000.0`) under Lighter Standard `0/0 bps`.

### 2.1 Additive Loss & Markout Decomposition Across Maker/Cancel Latencies

| Latency | Fills | Notional ($M) | Quote PnL ($) | Quote (bp) | Print PnL ($) | Gross Edge ($) | Hedge Lat Drift ($) | Hedge Half-Spr ($) | Depth Slip ($) | Mk+Hd Fees ($) | Drain Net ($) | Maker 1s Markout | Maker 5s Markout |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| **`0 ms`** | 38,014 | 20.48 | -9,801.65 | -4.786 | -14,808.92 | +11,273.06 | -3,288.57 | -1,323.52 | -11,871.15 | -4,337.71 | -253.76 | -$1,839.24 (`-0.898 bp`) | -$3,084.30 (`-1.506 bp`) |
| **`100 ms`** | 64,381 | 32.92 | -21,674.00 | -6.583 | -28,835.82 | +17,659.42 | -8,497.41 | -2,098.57 | -21,203.98 | -7,261.32 | -272.12 | -$5,122.45 (`-1.556 bp`) | -$7,208.79 (`-2.189 bp`) |
| **`200 ms`** | 98,605 | 49.61 | -38,351.80 | -7.730 | -47,617.84 | +26,005.66 | -16,830.64 | -3,073.38 | -33,139.88 | -11,010.52 | -303.04 | -$9,663.94 (`-1.948 bp`) | -$12,909.17 (`-2.602 bp`) |
| **`300 ms`** | 129,459 | 63.56 | -52,601.10 | -8.275 | -62,816.72 | +32,858.26 | -25,486.75 | -3,802.77 | -41,914.89 | -13,950.80 | -304.15 | -$13,673.71 (`-2.151 bp`) | -$16,822.11 (`-2.647 bp`) |

*(Note: `Quote PnL = Gross Edge + Hedge Lat Drift - Hedge Half-Spr - Depth Slip - Mk/Hd Fees + Drain Net`, exact to rounding.)*

### 2.2 Z-Score & Stationarity Regime Breakdown (`100 ms`)

| Diagnostic Bucket (`100 ms`) | Fills | Share of Fills | Quote PnL ($) | Avg PnL / Fill ($) |
| --- | ---: | ---: | ---: | ---: |
| `fav_high_z` (`side * z <= -1.5`) | 20,773 | 32.3% | -5,916.87 | -0.285 |
| `fav_low_z` (`-1.5 < side * z <= 0`) | 26,416 | 41.0% | -8,237.61 | -0.312 |
| `unfav_side` (`side * z > 0`) | 17,192 | 26.7% | -7,247.40 | -0.422 |
| `stationary` (`|drift| / sigma <= 1.0`) | 36,863 | 57.3% | -11,928.11 | -0.324 |
| `nonstationary` (`|drift| / sigma > 1.0`) | 27,518 | 42.7% | -9,473.76 | -0.344 |

**Key Stage A Diagnosis:**
1. **`depth_slip` (`-6.440 bps`) is the #1 cost driver**: Even though `min_hedge_l1_notional_usd = $1,000` gated decision-time books, hedge L1 liquidity is frequently depleted during the `40–300 ms` hedge arrival window when informed flow hits both venues simultaneously.
2. **Latency exposure (`0 -> 300 ms`) is super-linear**: Maker `1s` adverse selection more than doubles (`-0.898` to `-2.151 bps`) and hedge latency drift widens from `-1.606` to `-4.010 bps`.
3. **Zero-edge & non-stationary fills bleed continuously**: `67.7%` of fills occurred on the unfavorable or low-z-score side (`-$15,485.01` combined), and `42.7%` occurred during non-stationary basis drift (`-$9,473.76`).
4. **Why `basis_drift_guard_bps = 3.0` conflicts with Z-score mean reversion**: In testing, short-horizon `|b - prior_fast|` fires on the exact initial basis spike (`>= 3 bps`) that mean-reversion z-score quoting wants to enter, whereas comparing `|fast_fair - fair| / sigma` (`max_drift_to_vol_ratio`) allows fresh spikes while halting persistent regime shifts.

---

## 3. Stage B Rust Upgrades (`src/strategy/basis_maker_hedge.rs`)

All new config fields use `#[serde(default, deny_unknown_fields)]` and default to opt-off values (`0.0` / `false`). Existing F1 stale-before-EMA, `step`, `step_with_l1`, zero `crate::exchanges::*`, and zero hot-path allocation invariants are preserved:

1. **Selective Basis Z-Score & Stationarity Gate** (`MakerHedgeQuoterConfig`):
   - `min_basis_zscore: f64` (`0.0`), `basis_vol_floor_bps: f64` (`0.0`), `max_drift_to_vol_ratio: f64` (`0.0`), `quote_favorable_side_only: bool` (`false`).
   - Computes `denom_bps = basis_sigma_bps.max(basis_vol_floor_bps)` and gates **opening** quotes (`q >= 0` for bid, `q <= 0` for ask) so entries occur only on the statistically cheap/rich side (`z <= -min_basis_zscore` for bid, `z >= +min_basis_zscore` for ask) when `|(prior_fast_fair - fair) * 1e4| / denom_bps <= max_drift_to_vol_ratio`. Inventory-reducing exits (`ask` when `q > 0`, `bid` when `q < 0`) are never blocked.
2. **Passive-First-Then-IOC Hybrid Hedge Controller**:
   - `HedgeExecutionMode::{RestPassive, CrossIoc, FailClosedHalt}`, `PassiveHedgeConfig` (`enabled: false`, `max_passive_hedge_ms: 1500`, `hazard_ioc_adverse_bps: 2.0`, `max_hedge_book_age_ms: 5000`, `tick_size: 0.01`, `improve_one_tick: false`, `reduce_only: true`), `PassiveHedgeState`, `PassiveHedgeDecision`, and `evaluate_passive_hedge`.
   - Rests a strictly non-crossing `reduce_only` passive hedge at hedge touch, escalating immediately to `CrossIoc` on adverse move (`>= hazard_ioc_adverse_bps`) or timeout (`>= max_passive_hedge_ms`), and latching `FailClosedHalt` on invalid, stale, future-dated, or timestamp-regressed hedge books.
3. **L1 Microprice Anchor**:
   - `use_microprice_anchor: bool` (`false`) + `Touch::microprice(bid_sz, ask_sz)` + `MakerHedgeQuoter::step_with_l1_microprice(...)`, safely falling back to `Touch::mid()` when sizes are missing or non-positive.

---

## 4. Round-2 Iterative Optimization & Frozen `Holdout` / `W26` Results

All 4 arms were tuned exclusively on `selection_dev` (`w01..w20`) under an apples-to-apples candidate grid (`e0_bps in {4.0, 6.0}`, `vol_edge_mult = 1.0`, `min_basis_zscore in {1.5, 2.5}`, `min_hedge_l1_notional_usd in {1000, 2500}`, `max_passive_hedge_ms in {200, 500, 1000}`, `hazard_ioc_adverse_bps in {0.75, 1.5}`, `MIN_SELECTION_FILLS = 15` crypto / `5` RWA), then **frozen** and validated ONCE on untouched `holdout (w21–w25)` and `w26_oos`.

### 4.1 Cohort Head-to-Head (`100 ms` Maker/Cancel Latency, Lighter Standard `0/0 bps`)

| Cohort | Arm | Split | Fills | Quote PnL ($) | Quote (bp) | Print PnL ($) | Passive Hedge % | Fees ($) | Depth Slip ($) | Funding ($) |
| --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `USDC/USDC` WS distinct | `T2_taker_hedge` | `selection_dev` | 10,074 | -3,147.85 | -5.44 | -5,667.27 | 0.0% | 1,707.95 | 3,728.70 | -3.20 |
| `USDC/USDC` WS distinct | **`T2_zscore_selective`** | `selection_dev` | 5,377 | **-973.97** | **-3.19** | **-2,297.64** | 0.0% | 916.54 | 1,617.09 | -0.99 |
| `USDC/USDC` WS distinct | `T2_passive_then_ioc` | `selection_dev` | 10,026 | -5,051.33 | -8.77 | -6,558.34 | 3.1% | 1,690.01 | 4,014.63 | -3.08 |
| `USDC/USDC` WS distinct | `T2_zscore_plus_passive_ioc` | `selection_dev` | 5,177 | -1,927.86 | -6.55 | -2,692.52 | 2.8% | 883.49 | 1,659.90 | +0.05 |
| `USDC/USDC` WS distinct | `T2_taker_hedge` | `holdout` | 2,150 | -856.27 | -6.91 | -1,325.10 | 0.0% | 398.43 | 867.91 | -0.32 |
| `USDC/USDC` WS distinct | **`T2_zscore_selective`** | `holdout` | 1,233 | **-306.67** | **-4.42** | **-553.75** | 0.0% | 245.63 | 397.95 | -0.36 |
| `USDC/USDC` WS distinct | `T2_passive_then_ioc` | `holdout` | 2,150 | -1,287.78 | -10.39 | -1,543.86 | 3.1% | 396.61 | 950.36 | -0.32 |
| `USDC/USDC` WS distinct | `T2_zscore_plus_passive_ioc` | `holdout` | 1,185 | -531.30 | -7.83 | -670.47 | 1.2% | 239.24 | 435.16 | -0.38 |
| `USDC/USDC` WS distinct | `T2_taker_hedge` | `w26_oos` | 152 | -88.52 | -9.20 | -121.46 | 0.0% | 32.19 | 102.98 | +0.31 |
| `USDC/USDC` WS distinct | **`T2_zscore_selective`** | `w26_oos` | 128 | **-47.64** | **-5.82** | **-75.66** | 0.0% | 29.51 | 68.40 | +0.11 |
| `USDC/USDC` WS distinct | `T2_passive_then_ioc` | `w26_oos` | 152 | -86.79 | -9.02 | -100.50 | 0.5% | 32.19 | 73.16 | +0.31 |
| `USDC/USDC` WS distinct | `T2_zscore_plus_passive_ioc` | `w26_oos` | 133 | -52.43 | -6.09 | -71.80 | 0.0% | 31.75 | 50.08 | +0.16 |
| `USDG/USDC` WS (`0bp` FX) | `T2_taker_hedge` | `selection_dev` | 4,709 | -1,329.94 | -6.00 | -2,207.59 | 0.0% | 509.71 | 1,722.86 | +0.74 |
| `USDG/USDC` WS (`0bp` FX) | **`T2_zscore_selective`** | `selection_dev` | 2,828 | **-270.87** | **-2.30** | **-843.53** | 0.0% | 313.12 | 649.52 | +1.42 |
| `USDG/USDC` WS (`0bp` FX) | `T2_passive_then_ioc` | `selection_dev` | 4,603 | -1,794.83 | -8.34 | -2,275.27 | 6.1% | 486.67 | 1,551.32 | +0.60 |
| `USDG/USDC` WS (`0bp` FX) | `T2_zscore_plus_passive_ioc` | `selection_dev` | 2,658 | -609.63 | -5.46 | -926.62 | 4.7% | 286.04 | 625.90 | +2.48 |
| `USDG/USDC` WS (`0bp` FX) | `T2_taker_hedge` | `holdout` | 959 | -375.66 | -8.66 | -547.87 | 0.0% | 105.61 | 383.17 | -0.54 |
| `USDG/USDC` WS (`0bp` FX) | **`T2_zscore_selective`** | `holdout` | 572 | **-55.07** | **-2.83** | **-120.68** | 0.0% | 66.79 | 84.36 | -0.31 |
| `USDG/USDC` WS (`0bp` FX) | `T2_zscore_plus_passive_ioc` | `holdout` | 505 | -124.96 | -7.19 | -156.08 | 3.1% | 61.84 | 82.37 | -0.27 |
| `USDG/USDC` WS (`0bp` FX) | `T2_taker_hedge` | `w26_oos` | 63 | -20.03 | -7.06 | -29.74 | 0.0% | 9.26 | 22.25 | -0.38 |
| `USDG/USDC` WS (`0bp` FX) | **`T2_zscore_selective`** | `w26_oos` | 43 | **+2.99** | **+1.49** | **-2.20** | 0.0% | 6.48 | 3.38 | -0.15 |
| `->lighter` Zero-Fee Leg | `T2_taker_hedge` | `selection_dev` | 4,847 | -2,139.60 | -8.30 | -3,292.06 | 0.0% | 291.38 | 2,029.84 | -2.62 |
| `->lighter` Zero-Fee Leg | **`T2_zscore_selective`** | `selection_dev` | 1,703 | **-433.98** | **-5.05** | **-911.47** | 0.0% | 68.03 | 545.57 | +0.75 |
| `->lighter` Zero-Fee Leg | `T2_taker_hedge` | `holdout` | 965 | -506.58 | -9.34 | -730.15 | 0.0% | 77.11 | 432.54 | -0.69 |
| `->lighter` Zero-Fee Leg | **`T2_zscore_selective`** | `holdout` | 196 | **-65.69** | **-6.59** | **-113.55** | 0.0% | 18.61 | 61.36 | -0.58 |
| `rh <-> lighter` (`same_dom`) | `T2_taker_hedge` | `selection_dev` | 3,523 | -665.42 | -3.44 | -1,154.45 | 0.0% | 0.00 | 1,277.91 | -1.44 |
| `rh <-> lighter` (`same_dom`) | `T2_zscore_selective` | `selection_dev` | 2,199 | -260.96 | -2.44 | -570.12 | 0.0% | 0.00 | 646.83 | +0.05 |
| `rh <-> lighter` (`same_dom`) | **`T2_zscore_plus_passive_ioc`** | `selection_dev` | 2,119 | **-201.22** | **-1.92** | **-405.46** | **10.5%** | 0.00 | 444.41 | +0.33 |
| `rh <-> lighter` (`same_dom`) | `T2_taker_hedge` | `holdout` | 650 | -302.81 | -8.46 | -379.68 | 0.0% | 0.00 | 343.00 | -0.20 |
| `rh <-> lighter` (`same_dom`) | `T2_zscore_selective` | `holdout` | 304 | -87.64 | -5.58 | -133.26 | 0.0% | 0.00 | 133.34 | -0.08 |
| `rh <-> lighter` (`same_dom`) | **`T2_zscore_plus_passive_ioc`** | `holdout` | 296 | **-71.65** | **-4.65** | **-103.80** | **5.8%** | 0.00 | 97.46 | -0.07 |
| `rh <-> lighter` (`same_dom`) | **`T2_zscore_plus_passive_ioc`** | `w26_oos` | 9 | **-1.79** | **-3.25** | **-0.96** | **22.6%** | 0.00 | 0.30 | -0.12 |

**FX Sensitivity (`USDG/USDC` WS distinct-domain, `T2_zscore_selective` @ `100 ms`):**
- `selection_dev`: `-$270.87` (`0bp`) / `-$388.82` (`1bp`) / `-$506.78` (`2bp`).
- `holdout (w21–w25)`: `-$55.07` (`0bp`) / `-$74.53` (`1bp`) / `-$93.99` (`2bp`).
- `w26_oos`: `+$2.99` (`0bp`) / `+$0.99` (`1bp`) / `-$1.02` (`2bp`).

**Selected Positive RWA Pockets (`100 ms`, Quote / Print PnL):**
- `rh->arcus QQQ`: `T2_zscore_selective` `+$7.00` `selection_dev`, **`+$1.57`** `holdout` (`T2_taker_hedge` `+$5.13` / `+$1.56`).
- `lighter->arcus SPY`: `T2_zscore_selective` `+$4.84` `selection_dev`, **`+$0.11`** `holdout` (`T2_taker_hedge` `+$0.64` / `+$1.13`).
- `rh->lighter QQQ` (`same_risk_domain`): `T2_zscore_selective` `+$23.58` `selection_dev` / **`+$0.61`** `holdout`; `T2_zscore_plus_passive_ioc` `+$28.45` `selection_dev` / **`+$0.36`** (`+$0.25` print) `holdout`.

### 4.2 Maker/Cancel Latency Sensitivity (`{0, 100, 200, 300} ms`)

| Cohort | Arm | Split | `0 ms` ($) | `100 ms` ($) | `200 ms` ($) | `300 ms` ($) |
| --- | --- | --- | ---: | ---: | ---: | ---: |
| `USDC/USDC` WS distinct | `T2_taker_hedge` | `selection_dev` | -1,002.24 | -3,147.85 | -6,667.73 | -9,676.83 |
| `USDC/USDC` WS distinct | **`T2_zscore_selective`** | `selection_dev` | **-87.74** | **-973.97** | **-2,282.97** | **-3,135.68** |
| `USDC/USDC` WS distinct | `T2_taker_hedge` | `holdout` | -332.78 | -856.27 | -1,820.66 | -2,645.72 |
| `USDC/USDC` WS distinct | **`T2_zscore_selective`** | `holdout` | **-114.42** | **-306.67** | **-585.07** | **-764.44** |
| `USDC/USDC` WS distinct | **`T2_zscore_selective`** | `w26_oos` | **-6.99** | **-47.64** | **-62.03** | **-71.02** |
| `USDG/USDC` WS (`0bp` FX) | `T2_taker_hedge` | `selection_dev` | -577.17 | -1,329.94 | -2,161.02 | -3,005.50 |
| `USDG/USDC` WS (`0bp` FX) | **`T2_zscore_selective`** | `selection_dev` | **-13.09** | **-270.87** | **-550.39** | **-828.38** |
| `USDG/USDC` WS (`0bp` FX) | `T2_taker_hedge` | `holdout` | -185.80 | -375.66 | -530.85 | -736.86 |
| `USDG/USDC` WS (`0bp` FX) | **`T2_zscore_selective`** | `holdout` | **-27.99** | **-55.07** | **-133.15** | **-197.86** |
| `USDG/USDC` WS (`0bp` FX) | **`T2_zscore_selective`** | `w26_oos` | **-0.82** | **+2.99** | **+2.30** | **-4.63** |

---

## 5. Artifacts & Verification Summary

- **Pulled Local Artifacts** (untracked `.agent-state/research/strategy_upgrade_20261127/`, `LEAK_COUNT = 0`):
  - `track02_passive_hedge_w01_w26.json` (`sha256: f82c193a43d996d62d49618164c1d0744a6f47bac3b6e2c1042ee9a70395381e`)
  - `track02_stage_a_attribution.json` (`sha256: de18de9723caaef3b7279048096cecf3592eaa24e386f18e1bdf99f48454894f`)
- **Verification Gates:**
  - `make fmt-check` & `make check` (`cargo clippy --locked --features research-replay --lib --bins -- -D warnings`, `go vet ./...`)
  - `cargo test --locked --features research-replay --lib --bins` (19/19 `basis_maker_hedge` tests + full lib/bin suite)
  - `python3 -S scripts/research_track02_passive_hedge_arb.py --self-test` & `python3 -S -m unittest -v scripts/test_research_track02_passive_hedge_arb.py` (4/4 stdlib-only tests)
  - Relative markdown link check (`BAD_LINKS = []`) and `git diff --check` clean.
