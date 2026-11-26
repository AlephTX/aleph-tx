# Track 03 (`03_incentive_turnover`): Rust G4 Promotion & Full W01–W25 Multi-Venue Session-Gated Evaluation

**Date**: `2026-11-27` (CST) · **Operational Mode**: Strictly **`HOLD`** (zero live orders/cancels, zero Tokyo deploys/restarts, zero `crate::exchanges::*` imports in `src/strategy/`)

This report documents:
1. Promotion of proven **`G4_quiet_regime_only`** mechanics into the zero-allocation Rust strategy engine ([queue_priority_mm.rs](../src/strategy/queue_priority_mm.rs)), backward-compatible and default-off.
2. Full **W01–W25** multi-venue Crypto (`BTC, ETH, SOL`: `19w ~194.7h` `full_dev` + `5w ~51.2h` untouched `holdout`) and RWA (`AAPL, SPY, QQQ, NVDA, TSLA, DRAM, SNDK`: `15w ~153.7h` `full_dev` + `4w ~41.0h` untouched `holdout`) plus `bulk` (`20w + 5w ~256.1h`, strictly separated as `book_diff_diagnostic`) turnover & **UTC Session-Gated Ablation** via [research_track03_multivenue_turnover.py](../scripts/research_track03_multivenue_turnover.py) (unit tests: [test_research_track03_multivenue_turnover.py](../scripts/test_research_track03_multivenue_turnover.py)).
3. Reuses `compute_volume_farming_cost` from [eval_volume_farming_cost.py](../scripts/eval_volume_farming_cost.py) with **`rebate_bps = 0.0`** default, keeping **`USDC`** (`lighter`, `extended`, `arcus`, `bulk`) vs **`USDG`** (`lighter_robinhood`) and **`account_fee_status`** (`public_tier0_unverified` / `public_tier0_schedule_only`) strictly explicit.

---

## 1. Executive Verdict

1. **`G4_quiet_regime_only` Mechanics + Session Gating Generalize Across ~246h Crypto & ~195h RWA**:
   - **RWA `lighter` (`USDC`, `public_tier0_unverified`, `0/0 bps`)**:
     - `full_dev` (`15w`, `153.7h`): `S0_all_hours` `+$28.00/M` (`-$1,738.38` net, `max_dd $1,738.38`) drops **82%** to **`G4_all_hours` `+$5.00/M`** (`-$177.07` net, `max_dd $177.07`, `3/15` positive windows) and **`G4_quiet_sessions_only` `+$5.23/M`** (`-$131.85` net, `max_dd $131.85`).
     - Untouched `holdout` (`4w`, `41.0h`): `S0_all_hours` `+$39.18/M` (`-$592.34`) drops **72%** to **`G4_quiet_sessions_only` `+$10.93/M`** (`-$72.49` on `$3.32M` exit notional), outperforming both `G4_all_hours` (`+$12.62/M`) and `G4_us_rth_only` (`+$24.31/M`)!
     - **`lighter AAPL` (`USDC`) achieves sustained positive net MM edge (`<= $0/M`)**: `-$6.28/M` (**`+$27.85` net**, `12/15 = 80%` positive windows, `1s mk = +0.248 bp`) in `full_dev` and **`-$3.62/M`** (**`+$5.09` net**, `3/4 = 75%` positive windows, `1s mk = +0.218 bp`) in untouched `holdout` under `G4_quiet_sessions_only`!
   - **RWA `lighter_robinhood` (`USDG`, `public_tier0_unverified`, `0/0 bps`)**:
     - `full_dev` (`15w`): `S0_all_hours` `+$20.33/M` → **`G4_all_hours` `+$12.37/M`** (`G4_quiet_sessions_only`: `+$13.01/M`).
     - Untouched `holdout` (`4w`): `S0_all_hours` `+$22.33/M` → **`G4_quiet_sessions_only` `+$12.39/M`** (`SPY`: `+$8.04/M`, `QQQ`: `+$9.19/M`, `AAPL`: `+$10.27/M`), beating `G4_us_rth_only` (`+$14.08/M`).
   - **Crypto `lighter` (`USDC`, `public_tier0_unverified`, `0/0 bps`)**:
     - `full_dev` (`19w`, `194.7h`, includes `w03` crash): `S0_all_hours` `+$44.92/M` (`-$5,550.98`) drops **65%** to **`G4_low_vol_q25` `+$15.70/M`** (`-$267.66` on `$8.53M`, `BTC = +$14.46/M`, `ETH = +$16.04/M`), and `G4_all_hours` `+$17.09/M`.
     - Untouched `holdout` (`5w`, `51.2h`): `S0_all_hours` `+$49.43/M` → **`G4_overnight_asia` `+$16.64/M`**, `G4_all_hours` `+$17.23/M` (`BTC = +$13.08/M`), `G4_low_vol_q25` `+$17.71/M`, vs `G4_us_rth` `+$23.51/M`.
   - **`arcus` & `extended` (`NO-GO` for pure turnover at Tier 0) & `bulk` (`book_diff_diagnostic` only)**:
     - `arcus` Crypto (`1.50/4.50 bps` Tier 0) remains `+$346–$405/M` and `arcus` RWA (`0/2.25 bps` Tier 0) remains `+$174–$188/M` (`s ~ 80–85%` taker exit share).
     - `extended` Crypto (`0/2.50 bps`) improves from `+$279/M` (`S0`) to `+$215.90/M` (`G4_all_hours` holdout), constrained by `s = 94.5%` taker exits on thin print volume.
     - `bulk` (`book_diff_diagnostic` only, never mixed into `public_trades` verified totals) sits at `+$136.80/M` (Crypto holdout) and `+$74.48/M` (RWA holdout) under `G4_all_hours`.

---

## 2. Rust Strategy Promotion (`src/strategy/queue_priority_mm.rs`)

Added backward-compatible, default-off (`#[serde(default)]`, zero heap allocation on quote path, zero `crate::exchanges::*`) fields to `QueuePriorityMmConfig` and `QueuePriorityMmEngine::evaluate` in [queue_priority_mm.rs](../src/strategy/queue_priority_mm.rs):

| Parameter | Default | Role in `QueuePriorityMmEngine::evaluate` |
|---|---:|---|
| `as_inventory_skew_gamma` | `0.0` | Avellaneda-Stoikov reservation skew `inv_ratio * gamma * (1.0 + var_bps2_per_s)`, widening opening quotes on the over-inventory side while keeping `reduce_only` exits at touch/pennied. |
| `vol_spread_mult` | `0.0` | Causal volatility-adaptive half-spread addition `vol_spread_mult * sigma_bps_per_sqrt_s` bps applied to opening quotes only. |
| `directional_markout_skew_mult` | `0.0` | Asymmetric per-side negative-markout skew: widens ONLY the side with `markout_ewma_bps[side] < 0` by `(-markout_ewma_bps[side]) * mult` bps instead of symmetric cooldown alone. |
| `quiet_regime_min_spread_to_vol` | `0.0` | Quiet-regime ratio gate: suppresses NEW opening `PlaceAlo` quotes when `spread_bps / sigma_1s.max(0.05) < min_ratio`, while ALWAYS keeping `reduce_only` exits active. |
| `quiet_regime_max_entry_vol_bps` | `None` | Quiet-regime max volatility gate: suppresses NEW opening `PlaceAlo` quotes when `sigma_1s > max_vol`, while ALWAYS keeping `reduce_only` exits active when `position != 0`. |

Unit-tested across `23/23` tests in `queue_priority_mm::tests` (including `g4_defaults_remain_backward_compatible_off`, `g4_as_inventory_skew_and_vol_adaptive_spread_widen_opening_quotes`, `g4_directional_markout_skew_widens_only_negative_markout_side`, and `g4_quiet_regime_gates_suppress_opening_but_preserve_reducing_exits`).

---

## 3. Full W01–W25 Multi-Venue Results (`full_dev` vs Untouched `holdout`)

All results use `rebate_bps = 0.0`, strict `terminal_flat = True`, and exact cycle identity `gross_usd == spr + drft + pex + tslp`, `net_usd == gross_usd - fee_usd`.

### 3.1 Crypto (`BTC, ETH, SOL`) — `full_dev` (`19w`, `194.7h` `public_trades`; `bulk` `20w`, `204.9h`) vs `holdout` (`5w`, `51.2h`)

| Split | Venue (Settlement / Fee Status) | Arm (Session / Vol Gate) | `w` | `ntl` ($) | `s` | `gross` (bp) | `$/M` Double-Sided | `net` ($) | `max_dd` ($) | `pos_win` |
|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| `full_dev` | `lighter` (`USDC`, `public_tier0_unverified`) | `S0_all_hours` | 19 | 61,786,570 | 0.237 | -0.898 | **+44.92** | -5,550.98 | 5,550.98 | 0.00 |
| `full_dev` | `lighter` (`USDC`, `public_tier0_unverified`) | `S0_low_vol_q25` | 19 | 19,889,166 | 0.242 | -0.684 | **+34.21** | -1,360.71 | 1,360.71 | 0.00 |
| `full_dev` | `lighter` (`USDC`, `public_tier0_unverified`) | `G4_all_hours` | 19 | 17,130,532 | 0.102 | -0.342 | **+17.09** | -585.50 | 585.50 | 0.00 |
| `full_dev` | `lighter` (`USDC`, `public_tier0_unverified`) | **`G4_low_vol_q25`** | 19 | 8,525,740 | 0.097 | -0.314 | **+15.70** | -267.66 | 267.66 | 0.00 |
| `full_dev` | `lighter` (`USDC`, `public_tier0_unverified`) | `G4_overnight_asia` | 19 | 8,201,824 | 0.106 | -0.331 | **+16.53** | -271.12 | 271.12 | 0.00 |
| `full_dev` | `lighter` (`USDC`, `public_tier0_unverified`) | `G4_us_rth` | 19 | 3,771,085 | 0.089 | -0.388 | +19.39 | -146.27 | 146.27 | 0.00 |
| `full_dev` | `lighter_robinhood` (`USDG`, `public_tier0_unverified`) | `S0_all_hours` | 19 | 16,957,585 | 0.601 | -1.100 | **+55.01** | -1,865.73 | 1,865.73 | 0.00 |
| `full_dev` | `lighter_robinhood` (`USDG`, `public_tier0_unverified`) | **`G4_all_hours`** | 19 | 7,214,251 | 0.512 | -0.464 | **+23.19** | -334.59 | 334.59 | 0.00 |
| `full_dev` | `lighter_robinhood` (`USDG`, `public_tier0_unverified`) | `G4_low_vol_q25` | 19 | 2,584,204 | 0.515 | -0.509 | +25.44 | -131.51 | 131.51 | 0.00 |
| `full_dev` | `extended` (`USDC`, `public_tier0_unverified`, `0/2.50`) | `S0_all_hours` | 19 | 2,885,718 | 0.890 | -3.269 | +274.65 | -1,585.12 | 1,585.12 | 0.00 |
| `full_dev` | `extended` (`USDC`, `public_tier0_unverified`, `0/2.50`) | `G4_all_hours` | 19 | 60,703 | 0.944 | -2.756 | **+255.82** | -31.06 | 31.06 | 0.00 |
| `full_dev` | `arcus` (`USDC`, `public_tier0_schedule_only`, `1.50/4.50`) | `S0_all_hours` | 19 | 6,178,363 | 0.759 | -2.493 | +388.46 | -4,800.13 | 4,800.13 | 0.00 |
| `full_dev` | `arcus` (`USDC`, `public_tier0_schedule_only`, `1.50/4.50`) | `G4_all_hours` | 19 | 37,274 | 0.842 | -2.578 | +405.17 | -30.20 | 30.20 | 0.00 |
| `full_dev` | `bulk` (`USDC`, `book_diff_diagnostic`, `0/2.00`) | `G4_all_hours` | 20 | 261,779 | 0.170 | -2.677 | +150.81 | -78.96 | 78.96 | 0.00 |
| **`holdout`** | `lighter` (`USDC`, `public_tier0_unverified`) | `S0_all_hours` | 5 | 16,491,966 | 0.226 | -0.989 | **+49.43** | -1,630.25 | 1,630.25 | 0.00 |
| **`holdout`** | `lighter` (`USDC`, `public_tier0_unverified`) | `S0_low_vol_q25` | 5 | 5,380,248 | 0.234 | -0.744 | +37.19 | -400.14 | 400.14 | 0.00 |
| **`holdout`** | `lighter` (`USDC`, `public_tier0_unverified`) | `G4_all_hours` | 5 | 4,245,296 | 0.116 | -0.345 | **+17.23** | -146.33 | 146.33 | 0.00 |
| **`holdout`** | `lighter` (`USDC`, `public_tier0_unverified`) | `G4_low_vol_q25` | 5 | 2,067,425 | 0.109 | -0.354 | **+17.71** | -73.24 | 73.24 | 0.00 |
| **`holdout`** | `lighter` (`USDC`, `public_tier0_unverified`) | **`G4_overnight_asia`** | 5 | 2,434,362 | 0.111 | -0.333 | **+16.64** | -81.02 | 81.02 | 0.00 |
| **`holdout`** | `lighter` (`USDC`, `public_tier0_unverified`) | `G4_us_rth` | 5 | 573,435 | 0.097 | -0.470 | +23.51 | -26.96 | 26.96 | 0.00 |
| **`holdout`** | `lighter_robinhood` (`USDG`, `public_tier0_unverified`) | `S0_all_hours` | 5 | 5,197,143 | 0.529 | -1.353 | +67.67 | -703.40 | 703.40 | 0.00 |
| **`holdout`** | `lighter_robinhood` (`USDG`, `public_tier0_unverified`) | `G4_all_hours` | 5 | 1,627,403 | 0.499 | -0.623 | **+31.13** | -101.31 | 101.31 | 0.00 |
| **`holdout`** | `lighter_robinhood` (`USDG`, `public_tier0_unverified`) | **`G4_low_vol_q25`** | 5 | 643,700 | 0.514 | -0.621 | **+31.05** | -39.97 | 39.97 | 0.00 |
| **`holdout`** | `extended` (`USDC`, `public_tier0_unverified`, `0/2.50`) | `G4_all_hours` | 5 | 7,996 | 0.945 | -1.955 | +215.90 | -3.45 | 3.45 | 0.00 |
| **`holdout`** | `arcus` (`USDC`, `public_tier0_schedule_only`, `1.50/4.50`) | `G4_all_hours` | 5 | 6,375 | 0.660 | -1.953 | +346.68 | -4.42 | 4.42 | 0.00 |
| **`holdout`** | `bulk` (`USDC`, `book_diff_diagnostic`, `0/2.00`) | `G4_all_hours` | 5 | 162,935 | 0.057 | -2.622 | +136.80 | -44.58 | 44.58 | 0.00 |

Crypto per-symbol holdout highlights (`G4_all_hours` / `G4_low_vol_q25`):
- `lighter BTC`: `+$13.08/M` (`G4_all_hours`, `$2.40M`), `+$13.64/M` (`G4_low_vol_q25`, `$1.17M`).
- `lighter ETH`: `+$20.52/M` (`G4_all_hours`, `$1.49M`), `+$20.16/M` (`G4_low_vol_q25`, `$740k`).
- `lighter SOL`: `+$31.23/M` (`G4_all_hours`, `$363k`), `+$36.04/M` (`G4_low_vol_q25`, `$161k`).

### 3.2 RWA (`AAPL, SPY, QQQ, NVDA, TSLA, DRAM, SNDK`) — `full_dev` (`15w`, `153.7h` `public_trades`; `bulk` `20w`) vs `holdout` (`4w`, `41.0h`; `bulk` `5w`)

| Split | Venue (Settlement / Fee Status) | Arm (Session Gate) | `w` | `ntl` ($) | `s` | `gross` (bp) | `$/M` Double-Sided | `net` ($) | `max_dd` ($) | `pos_win` |
|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| `full_dev` | `lighter` (`USDC`, `public_tier0_unverified`) | `S0_all_hours` | 15 | 31,045,918 | 0.595 | -0.560 | **+28.00** | -1,738.38 | 1,738.38 | 0.00 |
| `full_dev` | `lighter` (`USDC`, `public_tier0_unverified`) | `S0_quiet_sessions_only` | 15 | 21,088,519 | 0.638 | -0.432 | +21.62 | -911.93 | 911.93 | 0.00 |
| `full_dev` | `lighter` (`USDC`, `public_tier0_unverified`) | **`G4_all_hours`** | 15 | 17,689,566 | 0.449 | -0.100 | **+5.00** | -177.07 | 177.07 | **0.20** |
| `full_dev` | `lighter` (`USDC`, `public_tier0_unverified`) | **`G4_quiet_sessions_only`** | 15 | 12,610,799 | 0.476 | -0.105 | **+5.23** | -131.85 | 131.85 | **0.20** |
| `full_dev` | `lighter` (`USDC`, `public_tier0_unverified`) | `G4_us_rth_only` | 15 | 5,085,224 | 0.383 | -0.087 | +4.35 | -44.28 | 50.48 | 0.20 |
| `full_dev` | `lighter_robinhood` (`USDG`, `public_tier0_unverified`) | `S0_all_hours` | 15 | 58,939,332 | 0.289 | -0.407 | +20.33 | -2,396.86 | 2,396.86 | 0.00 |
| `full_dev` | `lighter_robinhood` (`USDG`, `public_tier0_unverified`) | `S0_quiet_sessions_only` | 15 | 44,710,419 | 0.283 | -0.384 | +19.21 | -1,717.70 | 1,717.70 | 0.00 |
| `full_dev` | `lighter_robinhood` (`USDG`, `public_tier0_unverified`) | **`G4_all_hours`** | 15 | 28,996,902 | 0.260 | -0.247 | **+12.37** | -717.29 | 717.29 | 0.00 |
| `full_dev` | `lighter_robinhood` (`USDG`, `public_tier0_unverified`) | `G4_quiet_sessions_only` | 15 | 22,336,914 | 0.259 | -0.260 | +13.01 | -581.40 | 581.40 | 0.00 |
| `full_dev` | `arcus` (`USDC`, `public_tier0_schedule_only`, `0/2.25`) | `S0_all_hours` | 15 | 12,818,168 | 0.781 | -1.876 | +181.73 | -4,658.80 | 4,658.80 | 0.00 |
| `full_dev` | `arcus` (`USDC`, `public_tier0_schedule_only`, `0/2.25`) | `G4_all_hours` | 15 | 554,652 | 0.823 | -1.864 | +185.78 | -206.09 | 206.09 | 0.00 |
| `full_dev` | `bulk` (`USDC`, `book_diff_diagnostic`, `0/2.00`) | `G4_all_hours` | 20 | 32,411,814 | 0.322 | -1.035 | +83.96 | -5,442.73 | 5,442.73 | 0.00 |
| **`holdout`** | `lighter` (`USDC`, `public_tier0_unverified`) | `S0_all_hours` | 4 | 7,559,989 | 0.604 | -0.784 | **+39.18** | -592.34 | 592.34 | 0.00 |
| **`holdout`** | `lighter` (`USDC`, `public_tier0_unverified`) | `S0_quiet_sessions_only` | 4 | 5,795,225 | 0.656 | -0.605 | +30.23 | -350.34 | 350.34 | 0.00 |
| **`holdout`** | `lighter` (`USDC`, `public_tier0_unverified`) | `G4_all_hours` | 4 | 3,805,342 | 0.493 | -0.252 | **+12.62** | -96.05 | 96.05 | 0.00 |
| **`holdout`** | `lighter` (`USDC`, `public_tier0_unverified`) | **`G4_quiet_sessions_only`** | 4 | 3,315,775 | 0.515 | -0.219 | **+10.93** | -72.49 | 72.49 | 0.00 |
| **`holdout`** | `lighter` (`USDC`, `public_tier0_unverified`) | `G4_us_rth_only` | 4 | 490,644 | 0.347 | -0.486 | +24.31 | -23.85 | 23.85 | 0.00 |
| **`holdout`** | `lighter_robinhood` (`USDG`, `public_tier0_unverified`) | `S0_all_hours` | 4 | 13,993,329 | 0.343 | -0.447 | +22.33 | -624.90 | 624.90 | 0.00 |
| **`holdout`** | `lighter_robinhood` (`USDG`, `public_tier0_unverified`) | `S0_quiet_sessions_only` | 4 | 12,016,853 | 0.333 | -0.394 | +19.71 | -473.64 | 473.64 | 0.00 |
| **`holdout`** | `lighter_robinhood` (`USDG`, `public_tier0_unverified`) | `G4_all_hours` | 4 | 7,736,440 | 0.251 | -0.252 | **+12.60** | -194.98 | 194.98 | 0.00 |
| **`holdout`** | `lighter_robinhood` (`USDG`, `public_tier0_unverified`) | **`G4_quiet_sessions_only`** | 4 | 6,747,980 | 0.254 | -0.248 | **+12.39** | -167.28 | 167.28 | 0.00 |
| **`holdout`** | `lighter_robinhood` (`USDG`, `public_tier0_unverified`) | `G4_us_rth_only` | 4 | 990,360 | 0.230 | -0.282 | +14.08 | -27.88 | 27.88 | 0.00 |
| **`holdout`** | `arcus` (`USDC`, `public_tier0_schedule_only`, `0/2.25`) | `G4_all_hours` | 4 | 180,912 | 0.798 | -1.695 | +174.53 | -63.15 | 63.15 | 0.00 |
| **`holdout`** | `bulk` (`USDC`, `book_diff_diagnostic`, `0/2.00`) | `G4_all_hours` | 5 | 10,404,550 | 0.189 | -1.112 | +74.48 | -1,549.78 | 1,549.78 | 0.00 |

### 3.3 RWA Top-Symbol Comparison (`full_dev 15w` vs Untouched `holdout 4w`, `G4_quiet_sessions_only`)

| Venue (Settlement) | Symbol | `S0_all_hours` `full_dev` / `holdout` (`$/M`) | `G4_quiet_sessions_only` `full_dev` (`$/M`, `net`, `pos_win`, `1s mk`) | `G4_quiet_sessions_only` `holdout` (`$/M`, `net`, `pos_win`, `1s mk`) |
|---|---|---:|---|---|
| `lighter` (`USDC`) | **`AAPL`** | +13.93 / +18.64 | **`-$6.28/M`** (`+$27.85`, `12/15`, `+0.248 bp`) | **`-$3.62/M`** (`+$5.09`, `3/4`, `+0.218 bp`) |
| `lighter` (`USDC`) | **`QQQ`** | +18.79 / +33.75 | **`+$4.28/M`** (`-$28.04`, `3/15`, `+0.117 bp`) | **`+$14.17/M`** (`-$26.33`, `0/4`, `+0.026 bp`) |
| `lighter` (`USDC`) | **`SPY`** | +16.39 / +24.70 | **`+$7.81/M`** (`-$49.37`, `0/15`, `+0.040 bp`) | **`+$10.14/M`** (`-$15.33`, `0/4`, `+0.031 bp`) |
| `lighter_robinhood` (`USDG`) | **`SPY`** | +10.29 / +10.69 | **`+$8.41/M`** (`-$141.34`, `0/15`) | **`+$8.04/M`** (`-$40.64`, `0/4`) |
| `lighter_robinhood` (`USDG`) | **`QQQ`** | +14.61 / +14.76 | **`+$10.13/M`** (`-$122.30`, `1/15`) | **`+$9.19/M`** (`-$37.35`, `0/4`) |
| `lighter_robinhood` (`USDG`) | **`AAPL`** | +15.08 / +13.56 | **`+$15.27/M`** (`-$66.53`, `0/15`) | **`+$10.27/M`** (`-$15.29`, `0/4`) |

---

## 4. Regime Stratification (`calm` / `normal` / `extreme_crash`) & `w03` Crash Highlight

Window regimes are stratified using frozen `full_dev` per-`(domain, symbol)` `p33` / `p80` realized 1s volatility cutoffs (with `w03` explicitly tagged `extreme_crash` in Crypto), then applied unchanged to `holdout`:

| Split & Arm | `calm` (`$/M`, `ntl`) | `normal` (`$/M`, `ntl`) | `extreme_crash` (`$/M`, `ntl`) | Worst 10h Window (`win`, `regime`, `net`, `$/M`) |
|---|---:|---:|---:|---|
| Crypto `lighter` `S0_all_hours` (`full_dev`) | +32.92 ($15.95M) | +41.10 ($26.12M) | **+59.68** ($19.72M) | `w14` (`extreme_crash`, `-$589.48`, `+$79.52/M`) |
| Crypto `lighter` `G4_low_vol_q25` (`full_dev`) | **+13.46** ($2.58M) | **+17.21** ($3.60M) | **+15.84** ($2.35M) | `w02` (`normal`, `-$18.93`, `+$21.42/M`) |
| Crypto `lighter` `S0_all_hours` (`holdout`) | +39.29 ($1.55M) | +42.25 ($9.30M) | **+64.03** ($5.64M) | `w24` (`extreme_crash`, `-$561.67`, `+$71.27/M`) |
| Crypto `lighter` `G4_all_hours` (`holdout`) | +21.44 ($488k) | +16.70 ($2.82M) | **+16.65** ($935k) | `w23` (`normal`, `-$36.60`, `+$19.84/M`) |
| RWA `lighter` `S0_all_hours` (`full_dev`) | +11.79 ($8.22M) | +24.09 ($14.49M) | **+50.77** ($8.33M) | `w19` (`extreme_crash`, `-$391.79`, `+$77.87/M`) |
| RWA `lighter` `G4_quiet_sessions_only` (`full_dev`) | **+1.99** ($3.62M) | **+6.49** ($6.50M) | **+6.65** ($2.48M) | `w14` (`normal`, `-$22.35`, `+$9.91/M`) |
| RWA `lighter` `S0_all_hours` (`holdout`) | +13.36 ($567k) | +32.30 ($4.59M) | **+58.44** ($2.40M) | `w24` (`extreme_crash`, `-$280.53`, `+$58.44/M`) |
| RWA `lighter` `G4_quiet_sessions_only` (`holdout`) | **`-0.95`** ($401k, `+$0.77`) | **+13.12** ($2.54M) | **+8.75** ($371k) | `w23` (`normal`, `-$27.20`, `+$14.12/M`) |

**`w03` Crash Highlight**:
- On `lighter ETH` during `w03` (`vol_1s = 1.363 bps`, `p99_1s_ret = 4.871 bps`): `S0_all_hours` lost `-$92.23` (`+$40.91/M`), `G4_all_hours` reduced loss to `-$7.67` (`+$11.02/M`), and `G4_low_vol_q25` cut it further to **`-$2.80` (`+$7.25/M`, -82%)**.
- On `lighter AAPL` during `w03` (`vol_1s = 0.571 bps`, `p99_1s_ret = 2.220 bps`): `S0_all_hours` lost `-$2.11` (`+$3.25/M`), while `G4_all_hours` earned **`+$17.33` (`-$27.30/M`)** and `G4_quiet_sessions_only` earned **`+$6.61` (`-$35.40/M`)**.

---

## 5. Artifacts & Verification Summary

- **Aleph Read-Only Outputs** (executed under `nice -n 10` with `/home/metaverse/aleph-tx-lakehouse/.venv/bin/python3`):
  - `/tmp/swarm_track3/track03_multivenue_full_dev.json` (`3,403` rows, `sha256: e0dd0440b708fad967497cf33b954b66c9cce280e6985acfedb918e964d85305`)
  - `/tmp/swarm_track3/track03_multivenue_holdout.json` (`890` rows, `sha256: e9b47da01341da695532886fef288735dd0a2b8b9ffc69d8e846a50e8458c0f2`)
- **Audit Caveats**:
  - `lighter` and `lighter_robinhood` `0/0 bps` remain `public_tier0_unverified` pending signed `/api/v1/accountLimits` verification.
  - `lighter_robinhood` settles in `USDG` vs `USDC` on `lighter`/`extended`/`arcus`/`bulk`; cross-domain FX conversion friction is not included in single-venue cost curves.
  - `bulk` has zero `public_trades` files on disk and is strictly tagged `book_diff_diagnostic` and excluded from `public_trades` totals.
