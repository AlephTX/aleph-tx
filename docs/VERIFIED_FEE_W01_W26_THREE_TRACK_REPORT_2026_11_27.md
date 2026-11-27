# Verified-Fee `OPT-01` Full `W01–W26` Three-Track & RWA Evaluation Report

**Date:** `2026-11-27` (CST) · **Operational Mode:** Strictly **`HOLD`** (zero live orders/cancels, zero Tokyo deploys/restarts, read-only `nice -n 10` on `aleph` lakehouse)

This report re-runs all upgraded multi-venue Track 01, Track 02, Track 03, RWA universe screening, and Scenario 1 volume-farming cost evaluations across **full `W01–W26`** (`25` crypto windows ≈ `256.25h`, `20` RWA windows ≈ `204.9h`, `26` bulk diagnostic windows ≈ `266.3h`), isolating **`W26` as fresh untouched prospective OOS**, using the live-verified **`OPT-01`** account fee evidence (`account_fee_evidence_20261127.json`, `sha256: 4d3a9c1aeae5358bb9c55ab448db392ac3edb0a89244f74c23d89817c3359d92`, `redaction_verified=true`, `error_venues=0`, `LEAK_COUNT=0`).

---

## 1. Executive Verdict & Key Regime Shifts Under Verified Fees

The single most consequential finding of `OPT-01` live account verification is that **Lighter Core (`USDC`) and Lighter Robinhood (`USDG`) have sharply different verified fee tiers despite both showing `account_type == 0`**:

1. **`lighter` (`USDC`) is NOT `0 / 0 bps` — it is `0.4 / 2.8 bps` (`verified_account_tier`, `user_tier=premium`, `custom_fee_override_detected=true`, ticks `40/280`)**, whereas **`lighter_robinhood` (`USDG`) IS `0.0 / 0.0 bps` (`verified_account_tier`, `user_tier=standard`, `standard_zero_confirmed=true`, ticks `0/0`)**.
2. **Track 01 (`01_quant_profit`)**: Under verified `2.8 bps` taker (`5.6 bps` round-trip), `lighter` (`USDC`) flips from previously assumed `+196.60` (`full_dev`) / `+49.11` (`w21–w25`) print-verified `$1k` to **deeply negative across every arm** (`-$357.20` to `-$1,203.79` `full_dev`, `-$68.61` to `-$227.64` `w21–w25`, `-$9.32` to `-$13.29` on `w26`). **`lighter_robinhood` (`USDG`, `0/0 bps`)** remains the sole print-verified positive venue on `full_dev` (`+$67.62` on `B_vol_adaptive_theta`, `73.7%` win ratio, `$4.41` DD) and `w21–w25` (`+$12.14` `A` / `+$10.21` `B`), turning near-flat (`-$0.10` pv`$1k` on 5 prints; `+$2.06` quote`$1k` on `E_combined_all`) during calm `w26`.
3. **Track 02 (`02_hedge_arbitrage`)**: Verified fees (`lighter` `0.4/2.8`, `arcus` 5% discount `1.425/4.275` Crypto & `0/2.1375` RWA, `extended` `0/2.25` Crypto & `0/0.9` RWA) widen `USDC/USDC` WS distinct-domain `upgraded_track2` losses by **+31.6%** in `full_dev` (`-$5,940.49` vs `-$4,514.26` pre-OPT-01; `-$8,197.94` print) and persist negative in `w21–w25` (`-$1,693.02` quote / `-$2,127.31` print) and `w26` OOS (`-$161.69` quote / `-$190.38` print). `USDG/USDC` WS distinct-domain (`lighter_robinhood<->arcus`) loses `-$1,449.37` (`0bp` FX) in `full_dev`, `-$405.82` in `w21–w25`, and `-$21.59` (`0bp`) / `-$24.37` (`1bp`) / `-$27.15` (`2bp`) in `w26` OOS.
4. **Track 03 (`03_incentive_turnover`, `rebate_bps=0.0`)**: Under `0.4 / 2.8 bps`, `lighter` (`USDC`) RWA `G4_all_hours` cost explodes **19.8x** from `+$5.00/M` (unverified `0/0`) to **`+$98.94/M`** (`-$3,500.42` net `full_dev`), `+$111.79/M` (`w21–w25`), and `+$109.45/M` (`w26` OOS); `lighter AAPL` (`G4_quiet_sessions_only`) flips from `-$6.28/M` (`+$27.85`) to **`+$92.16/M` (`-$408.99`)** in `full_dev`, `+$99.50/M` (`w21–w25`), and `+$92.48/M` (`w26`). **`lighter_robinhood` (`USDG`, `0/0 bps`)** is now **4x–10x cheaper than `lighter` (`USDC`)** (`+$12.37/M` RWA `G4_all_hours` `full_dev`, `+$12.60/M` `w21–w25`, **`+$7.56/M`** `w26` OOS, with `G4_us_rth_only` at **`+$4.78/M`** in `w26`), though still net negative without external incentives.
5. **RWA Universe Screener (`screen_rwa_mm_universe.py`)**: Under `0.4 / 2.8 bps`, **0 / 7 `lighter` (`USDC`) RWA symbols pass** across `W01–W26` (`nonzero_maker_fee` + `half_spread_below_min` on 6/7). Only **`lighter_robinhood` (`USDG`, `0/0 bps`)** (`MU, NVDA, AAPL` across `W01–W26`; `MU, AMZN, NVDA, AAPL, SPY, QQQ` in `W26`) and **`arcus` (`0/2.1375 bps`)** (`DRAM, NVDA` across `W01–W26`; `SNDK, NVDA` in `W26`) pass all frozen gates.
6. **Go / No-Go Verdict:** **Strict `HOLD` across all 3 tracks.** No venue/strategy combination justifies live capital deployment or unapproved incentive farming at `rebate_bps = 0.0`.

---

## 2. `OPT-01` Verified Account Fee-Tier Table

Audited strictly read-only on `2026-11-27T01:54:28Z` against `.env.all_exchanges` (`redaction_verified = true`, `LEAK_COUNT = 0`).

| Venue | Settlement | `account_fee_status` | Verified Maker / Taker (bps) | Pre-OPT-01 Assumed (bps) | Verified Account Evidence Notes |
| --- | --- | --- | ---: | ---: | --- |
| `lighter` | `USDC` | **`verified_account_tier`** | **`0.40 / 2.80`** | `0.00 / 0.00` | `account_type=0`, `user_tier=premium`, ticks `40/280`, `custom_fee_override_detected=true` |
| `lighter_robinhood` | `USDG` | **`verified_account_tier`** | **`0.00 / 0.00`** | `0.00 / 0.00` | `account_type=0`, `user_tier=standard`, ticks `0/0`, `standard_zero_confirmed=true` |
| `arcus` (Crypto) | `USDC` | **`verified_account_tier`** | **`1.425 / 4.275`** | `1.50 / 4.50` (L0) / `2.30` (L4) | `verified_level=0`, `fee_discount_ppm=50000` (5% discount on Tier 0) |
| `arcus` (RWA) | `USDC` | **`verified_account_tier`** | **`0.00 / 2.1375`** | `0.00 / 2.25` (L0) / `1.15` (L4) | 5% discount (`verified_custom_schedule_bps`; `ANT-USD` = `0.00 / 1.064`) |
| `extended` (Crypto) | `USDC` | **`verified_account_tier`** | **`0.00 / 2.25`** | `0.00 / 2.50` | Verified via `GET /api/v1/user/fees` |
| `extended` (24/5 RWA/FX) | `USDC` | **`verified_account_tier`** | **`0.00 / 0.90`** | `0.00 / 2.50` | Verified via `GET /api/v1/user/fees` per-market schedule |
| `hyperliquid` | `USDC` | **`verified_account_tier`** | **`1.50 / 4.50`** | `1.50 / 4.50` | `userFees` Tier 0 (`0.04` referral discount, `0` staking) |
| `trade_xyz` (`growthMode` / `std`) | `USDC` | **`verified_account_tier`** | **`0.30 / 0.90`** (`3.0 / 9.0` std) | `0.30 / 0.90` | `120/125` active markets `growthMode` enabled; `deployer_fee_scale=1` |
| `backpack` (Futures / Spot) | `USDC` | **`verified_account_tier`** | **`1.90 / 4.50`** (`7.0 / 9.0` spot) | `—` | Signed Ed25519 `GET /api/v1/account` (`instruction=accountQuery`) |
| `okx` (SWAP) | `USDT` | **`verified_account_tier`** | **`2.00 / 5.00`** | `—` | `Lv1` SWAP via `GET /api/v5/account/trade-fee` |
| `binance` (Futures) | `USDT` | **`verified_account_tier`** | **`2.00 / 5.00`** | `—` | `BTCUSDT` via `GET /fapi/v1/commissionRate` |
| `bulk` | `USDC` | **`unknown_requires_account_evidence`** | `null / null` (`0/2.0` sim diagnostic) | `0.00 / 2.00` diag | `/exchangeInfo` exposes 37 markets only; no fee-tier endpoint |

---

## 3. Track 01 (`01_quant_profit`): Full `W01–W26` Crypto Lead-Lag (`BTC, ETH, SOL`)

Evaluated with [research_track01_multivenue_lead_lag.py](../scripts/research_track01_multivenue_lead_lag.py) across `25` crypto windows (`full_dev w01..w20 ex w08` = `19w`, `holdout w21..w25` = `5w`, isolated `w26` prospective OOS = `1w`), `cont=True` only (`lighter`: `90/114` `full_dev`, `30/30` `w21–w25`, `6/6` `w26`; all other venues `100%` continuous). Parameters & regime cutoffs frozen strictly on `full_dev`.

### 3.1 `full_dev` (`W01–W20`), `holdout` (`W21–W25`), and Untouched `W26` OOS (`$500 / $1000` `0.5*L1`-Capped)

| Split | Venue (`Sett.`, Verified Taker) | Arm | `w` (Pair-W) | Trades (`PV`) | Quote Mean (bp) | PV Mean (bp) | Quote `$500` / `$1000` ($) | PV `$500` / `$1000` ($) | PV DD (`$1k`) | PV Win Ratio |
| --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `full_dev` | `lighter` (`USDC`, `2.80bp`) | `A_baseline_fixed_theta` | 90 | 2,982 (2,064) | `-3.75` | `-5.24` | `-423.84 / -743.06` | `-372.36 / -654.29` | `$654.29` | `0.0%` |
| `full_dev` | `lighter` (`USDC`, `2.80bp`) | `B_vol_adaptive_theta` | 90 | 2,507 (1,689) | `-2.86` | `-4.45` | `-273.34 / -483.54` | `-253.84 / -451.12` | `$451.12` | `0.0%` |
| `full_dev` | `lighter` (`USDC`, `2.80bp`) | `F_vol_plus_dual` | 90 | 2,034 (1,403) | `-2.59` | `-4.24` | `-200.67 / -357.21` | `-199.96 / -357.20` | `$357.20` | `0.0%` |
| `full_dev` | `lighter_robinhood` (`USDG`, `0.00bp`) | `A_baseline_fixed_theta` | 114 | 3,212 (575) | `+0.98` | `+3.42` | `-82.94 / -190.68` | `+28.39 / +50.91` | `$4.41` | `68.4%` |
| `full_dev` | `lighter_robinhood` (`USDG`, `0.00bp`) | **`B_vol_adaptive_theta`** | 114 | 3,160 (560) | `+1.14` | `+3.88` | `-63.91 / -158.16` | **`+38.56 / +67.62`** | `$4.41` | `73.7%` |
| `full_dev` | `lighter_robinhood` (`USDG`, `0.00bp`) | `F_vol_plus_dual` | 114 | 2,345 (439) | `+1.07` | `+4.08` | `-32.34 / -86.01` | **`+36.49 / +65.53`** | **`$1.73`** | `73.7%` |
| `full_dev` | `extended` (`USDC`, `2.25bp`) | `B_vol_adaptive_theta` | 114 | 2,525 (176) | `-0.71` | `+4.28` | `-185.65 / -351.43` | `+16.04 / +23.63` | `$10.35` | `46.7%` |
| `full_dev` | `arcus` (`USDC`, `4.275bp`) | `B_vol_adaptive_theta` | 114 | 5,724 (251) | `-2.54` | `-4.16` | `-592.49 / -1,035.48` | `-32.73 / -61.45` | `$61.45` | `10.5%` |
| `holdout (w21–w25)` | `lighter` (`USDC`, `2.80bp`) | `A_baseline_fixed_theta` | 30 | 665 (401) | `-4.94` | `-4.08` | `-133.05 / -231.03` | `-58.73 / -101.40` | `$101.40` | `0.0%` |
| `holdout (w21–w25)` | `lighter` (`USDC`, `2.80bp`) | `B_vol_adaptive_theta` | 30 | 613 (357) | `-5.00` | `-4.18` | `-123.53 / -210.35` | `-52.83 / -88.27` | `$88.27` | `0.0%` |
| `holdout (w21–w25)` | `lighter` (`USDC`, `2.80bp`) | `F_vol_plus_dual` | 30 | 461 (281) | `-4.83` | `-4.31` | `-88.01 / -148.01` | `-41.36 / -68.61` | `$68.61` | `0.0%` |
| `holdout (w21–w25)` | `lighter_robinhood` (`USDG`, `0.00bp`) | **`A_baseline_fixed_theta`** | 30 | 952 (114) | `+0.25` | `+0.52` | `-14.18 / -29.74` | **`+6.21 / +12.14`** | **`$0.00`** | `100.0%` |
| `holdout (w21–w25)` | `lighter_robinhood` (`USDG`, `0.00bp`) | `B_vol_adaptive_theta` | 30 | 946 (112) | `+0.26` | `+0.45` | `-15.97 / -33.57` | `+5.31 / +10.21` | `$1.76` | `80.0%` |
| `holdout (w21–w25)` | `lighter_robinhood` (`USDG`, `0.00bp`) | `F_vol_plus_dual` | 30 | 779 (96) | `+0.36` | `-0.00` | `-14.25 / -29.44` | `+3.18 / +6.02` | `$2.11` | `40.0%` |
| `holdout (w21–w25)` | `extended` (`USDC`, `2.25bp`) | `B_vol_adaptive_theta` | 30 | 534 (33) | `-1.21` | `+4.32` | `-42.38 / -80.57` | `-0.43 / -0.45` | `$2.06` | `60.0%` |
| `holdout (w21–w25)` | `arcus` (`USDC`, `4.275bp`) | `B_vol_adaptive_theta` | 30 | 1,368 (44) | `-3.28` | `-12.07` | `-198.32 / -342.33` | `-21.33 / -35.96` | `$38.57` | `20.0%` |
| **`w26_oos`** | `lighter` (`USDC`, `2.80bp`) | `A_baseline_fixed_theta` | 6 | 45 (19) | `-7.81` | `-8.47` | `-15.01 / -26.00` | `-7.25 / -13.29` | `$13.29` | `0.0%` |
| **`w26_oos`** | `lighter` (`USDC`, `2.80bp`) | `B_vol_adaptive_theta` | 6 | 43 (17) | `-7.40` | `-7.89` | `-13.62 / -24.19` | `-6.12 / -11.58` | `$11.58` | `0.0%` |
| **`w26_oos`** | `lighter` (`USDC`, `2.80bp`) | `F_vol_plus_dual` | 6 | 28 (15) | `-8.64` | `-7.85` | `-9.48 / -16.55` | `-5.73 / -10.35` | `$10.35` | `0.0%` |
| **`w26_oos`** | `lighter_robinhood` (`USDG`, `0.00bp`) | `A_baseline_fixed_theta` | 6 | 56 (5) | `-1.80` | `-6.53` | `-3.44 / -6.55` | `-0.10 / -0.10` | `$0.10` | `0.0%` |
| **`w26_oos`** | `lighter_robinhood` (`USDG`, `0.00bp`) | `B_vol_adaptive_theta` | 6 | 56 (5) | `-1.80` | `-6.53` | `-3.44 / -6.55` | `-0.10 / -0.10` | `$0.10` | `0.0%` |
| **`w26_oos`** | `lighter_robinhood` (`USDG`, `0.00bp`) | `E_combined_all` | 6 | 52 (16) | `+0.45` | `-0.96` | `+0.90 / +2.06` | `-0.09 / -0.08` | `$0.08` | `0.0%` |
| **`w26_oos`** | `extended` (`USDC`, `2.25bp`) | `E_combined_all` | 6 | 58 (4) | `+0.87` | `-3.55` | `+1.22 / +2.22` | `-0.14 / -0.25` | `$0.25` | `0.0%` |
| **`w26_oos`** | `arcus` (`USDC`, `4.275bp`) | `B_vol_adaptive_theta` | 6 | 94 (2) | `-3.83` | `-4.39` | `-15.99 / -27.30` | `-0.44 / -0.88` | `$0.88` | `0.0%` |

- **`w03` Crash Highlight (`full_dev`)**: `lighter` (`2.80bp` taker) flips from `+$10.20` pv (pre-OPT-01 `B_vol_adaptive`) to **`-$25.83` pv (`-$46.78` quote)**; `lighter_robinhood` (`0.00bp`) earns `+$3.55` pv (`34/203` trades, `-$5.89` quote) on `A`/`B` and `+$3.14` pv on `F_vol_plus_dual`.
- **Catch-up Exit (`D_catchup_exit`) Remains Rejected:** `lighter_robinhood` drops from `+$67.62` (`B`) to `-$38.77` pv (`full_dev`) and `lighter` worsens to `-$1,203.79` pv (`full_dev`) / `-$227.64` pv (`w21–w25`).

---

## 4. Track 02 (`02_hedge_arbitrage`): Full `W01–W26` Multi-Venue Arb & Funding Carry

Evaluated with [research_track02_multivenue_arb.py](../scripts/research_track02_multivenue_arb.py) across **25 crypto windows (`w01..w07, w09..w26`)** and **20 RWA windows (`w01..w05, w07, w09, w13..w21, w23..w26`)** with strict separation of settlement cohorts (`USDC/USDC` vs `USDG/USDC`), `same_risk_domain` (`rh<->lighter`), and `polled_15s_diagnostic` (`trade_xyz`).

### 4.1 Cohort Summaries (`full_dev` `w01–w20`, `holdout` `w21–w25`, and Isolated `w26` OOS)

| Cohort (`pairs`) | Mode | Split | Pair-W | Fills | Quote PnL ($) | Print-Verified PnL ($) | Funding ($) | Max Pair DD ($) | `w03` Crash ($) |
| --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `USDC/USDC` WS distinct (`25`) | `baseline_unshielded` | `full_dev` | 435 | 49,456 | `-28,698.54` | `-35,324.78` | `-1.85` | `5,112.25` | `-1,245.32` |
| `USDC/USDC` WS distinct (`25`) | `baseline_shielded` | `full_dev` | 435 | 37,118 | `-20,249.65` | `-25,014.69` | `-1.37` | `3,529.81` | `-979.74` |
| `USDC/USDC` WS distinct (`25`) | **`upgraded_track2`** | `full_dev` | 435 | 12,286 | **`-5,940.49`** | **`-8,197.94`** | `-0.91` | **`953.84`** | **`-263.27`** |
| `USDC/USDC` WS distinct (`25`) | `baseline_unshielded` | `holdout (w21–w25)` | 115 | 11,504 | `-7,255.25` | `-8,507.16` | `-0.09` | — | — |
| `USDC/USDC` WS distinct (`25`) | `baseline_shielded` | `holdout (w21–w25)` | 115 | 8,521 | `-5,209.95` | `-6,057.91` | `-0.06` | — | — |
| `USDC/USDC` WS distinct (`25`) | **`upgraded_track2`** | `holdout (w21–w25)` | 115 | 2,867 | **`-1,693.02`** | **`-2,127.31`** | `+0.06` | `309.69` (`w21–w26`) | — |
| `USDC/USDC` WS distinct (`25`) | `baseline_unshielded` | **`w26_oos`** | 25 | 1,056 | `-675.74` | `-779.87` | `-1.00` | — | — |
| `USDC/USDC` WS distinct (`25`) | `baseline_shielded` | **`w26_oos`** | 25 | 750 | `-455.56` | `-522.46` | `-1.38` | — | — |
| `USDC/USDC` WS distinct (`25`) | **`upgraded_track2`** | **`w26_oos`** | 25 | 231 | **`-161.69`** | **`-190.38`** | `+0.43` | — | — |
| `USDG/USDC` WS distinct (`16`, `0bp` FX) | `baseline_unshielded` | `full_dev` | 264 | 17,907 | `-5,838.88` | `-8,109.89` | `-0.21` | `1,109.80` | `-288.25` |
| `USDG/USDC` WS distinct (`16`, `0bp` FX) | `baseline_shielded` | `full_dev` | 264 | 14,794 | `-5,176.27` | `-6,936.05` | `-0.48` | `1,109.80` | `-257.74` |
| `USDG/USDC` WS distinct (`16`, `0bp` FX) | **`upgraded_track2`** | `full_dev` | 264 | 4,694 | **`-1,449.37`** | **`-2,181.06`** | `+0.37` | **`357.75`** | **`-56.58`** |
| `USDG/USDC` WS distinct (`16`, `0bp` FX) | **`upgraded_track2`** | `holdout (w21–w25)` | 70 | 1,025 | **`-405.82`** | **`-551.37`** | `+0.07` | `79.03` (`w21–w26`) | — |
| `USDG/USDC` WS distinct (`16`, `0bp` FX) | **`upgraded_track2`** | **`w26_oos`** | 16 | 69 | **`-21.59`** | **`-26.98`** | `-0.35` | — | — |

- **`USDG/USDC` Cross-Stablecoin FX Sensitivity (`upgraded_track2`, WS distinct-domain `16` pairs)**:
  - `full_dev`: `-$1,449.37` (`0bp`) / `-$1,663.01` (`1bp`) / `-$1,876.61` (`2bp`).
  - `holdout (w21–w26)`: `-$427.41` (`0bp`) / `-$476.06` (`1bp`) / `-$524.75` (`2bp`).
  - Isolated `w26` OOS: `-$21.59` (`0bp`) / `-$24.37` (`1bp`) / `-$27.15` (`2bp`).
- **`trade_xyz` `polled_15s_diagnostic` Separation (`upgraded_track2`)**:
  - `lighter->trade_xyz NVDA` displays illusory positive quote PnL (`+$712.49` `full_dev`, `+$138.16` `w21–w26`, `+$7.04` `w26`), which flips to **`-$76.55` (`full_dev`) / `-$78.42` (`w21–w26`) / `-$12.87` (`w26`)** under `public_trades` print verification. Similarly `rh->trade_xyz NVDA` flips from `+$835.19` / `+$153.81` / `+$10.53` quote to **`-$128.28` / `-$70.55` / `-$12.01`** print-verified.
- **RWA Positive Pockets Eroded by Verified Fees:** `lighter->arcus SPY` holds `+$0.83 / +$0.45` print in `w21–w26` (`0` fills `w26`, `-$6.42` `full_dev`), `lighter->arcus QQQ` drops to `+$0.27 / -$1.47` print in `w21–w26` (`-$0.24` `w26`), and `rh->arcus QQQ` (`USDG/USDC` `0bp` FX) holds `+$1.33 / +$1.18` print in `w21–w26` (`0` fills `w26`, `-$6.54` `full_dev`).

---

## 5. Track 03 (`03_incentive_turnover`): Full `W01–W26` Turnover & Session Gating (`rebate_bps = 0.0`)

Executed via [research_track03_multivenue_turnover.py](../scripts/research_track03_multivenue_turnover.py) across `full_dev` (`3,403` rows), `holdout w21–w25` (`890` rows), and isolated `w26` OOS (`195` rows), with `bulk` strictly separated as `book_diff_diagnostic`. **Invariant check across all `4,488` active rows:** `non_flat = 0` (`terminal_flat == True` 100%), `max_gross_err <= 2.00e-08`, `max_net_err <= 1.00e-08`.

### 5.1 Crypto (`BTC, ETH, SOL`) — `full_dev` (`19w`), `holdout` (`w21–w25`, `5w`), `w26` OOS (`1w`)

| Split | Venue (`Sett.`, Verified Fee bps) | Arm | `w` (`h`) | `ntl` ($) | `s` | `gross` (bp) | `$/M` Double-Sided | `net` ($) | `max_dd` ($) |
| --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `full_dev` | `lighter` (`USDC`, `0.40/2.80`) | `S0_all_hours` | 19 (194.7) | `61,786,454` | `0.237` | `-0.898` | **`+113.34`** | `-14,005.25` | `14,005.25` |
| `full_dev` | `lighter` (`USDC`, `0.40/2.80`) | `G4_low_vol_q25` | 19 (194.7) | `8,524,243` | `0.097` | `-0.314` | **`+67.32`** | `-1,147.63` | `1,147.63` |
| `full_dev` | `lighter_robinhood` (`USDG`, `0/0`) | `S0_all_hours` | 19 (194.7) | `16,957,585` | `0.601` | `-1.100` | `+55.01` | `-1,865.73` | `1,865.73` |
| `full_dev` | `lighter_robinhood` (`USDG`, `0/0`) | **`G4_all_hours`** | 19 (194.7) | `7,213,811` | `0.512` | `-0.463` | **`+23.16`** | `-334.19` | `334.19` |
| `full_dev` | `extended` (`USDC`, `0/2.25`) | `G4_all_hours` | 19 (194.7) | `60,680` | `0.944` | `-2.761` | `+244.20` | `-29.64` | `29.64` |
| `full_dev` | `arcus` (`USDC`, `1.425/4.275`) | `G4_all_hours` | 19 (194.7) | `37,274` | `0.842` | `-2.578` | `+391.36` | `-29.17` | `29.17` |
| `full_dev` | `bulk` (`USDC`, `book_diff_diag`) | `G4_all_hours` | 20 (204.9) | `261,779` | `0.170` | `-2.677` | `+150.81` | `-78.96` | `78.96` |
| `holdout (w21–w25)` | `lighter` (`USDC`, `0.40/2.80`) | `S0_all_hours` | 5 (51.2) | `16,491,966` | `0.226` | `-0.989` | `+116.58` | `-3,845.26` | `3,845.26` |
| `holdout (w21–w25)` | `lighter` (`USDC`, `0.40/2.80`) | `G4_low_vol_q25` | 5 (51.2) | `2,067,425` | `0.109` | `-0.354` | `+70.77` | `-292.61` | `292.61` |
| `holdout (w21–w25)` | `lighter_robinhood` (`USDG`, `0/0`) | `S0_all_hours` | 5 (51.2) | `5,197,143` | `0.529` | `-1.353` | `+67.67` | `-703.40` | `703.40` |
| `holdout (w21–w25)` | `lighter_robinhood` (`USDG`, `0/0`) | **`G4_low_vol_q25`** | 5 (51.2) | `643,754` | `0.514` | `-0.621` | **`+31.06`** | `-39.99` | `39.99` |
| `holdout (w21–w25)` | `extended` (`USDC`, `0/2.25`) | `G4_all_hours` | 5 (51.2) | `7,996` | `0.945` | `-1.955` | `+204.09` | `-3.26` | `3.26` |
| `holdout (w21–w25)` | `arcus` (`USDC`, `1.425/4.275`) | `G4_all_hours` | 5 (51.2) | `6,375` | `0.660` | `-1.953` | `+334.23` | `-4.26` | `4.26` |
| `holdout (w21–w25)` | `bulk` (`USDC`, `book_diff_diag`) | `G4_all_hours` | 5 (51.2) | `162,935` | `0.057` | `-2.622` | `+136.80` | `-44.58` | `44.58` |
| **`w26_oos`** | `lighter` (`USDC`, `0.40/2.80`) | `S0_all_hours` | 1 (10.2) | `2,966,238` | `0.284` | `-0.555` | `+101.85` | `-604.21` | `604.21` |
| **`w26_oos`** | `lighter` (`USDC`, `0.40/2.80`) | `G4_low_vol_q25` | 1 (10.2) | `499,735` | `0.132` | `-0.113` | `+61.45` | `-61.42` | `61.42` |
| **`w26_oos`** | `lighter_robinhood` (`USDG`, `0/0`) | `S0_all_hours` | 1 (10.2) | `818,930` | `0.634` | `-0.995` | `+49.74` | `-81.47` | `81.47` |
| **`w26_oos`** | `lighter_robinhood` (`USDG`, `0/0`) | **`G4_us_rth`** | 1 (10.2) | `110,512` | `0.507` | `-0.387` | **`+19.33`** | `-4.27` | `4.27` |
| **`w26_oos`** | `lighter_robinhood` (`USDG`, `0/0`) | `G4_all_hours` | 1 (10.2) | `409,136` | `0.557` | `-0.528` | `+26.38` | `-21.59` | `21.59` |
| **`w26_oos`** | `extended` (`USDC`, `0/2.25`) | `G4_all_hours` | 1 (10.2) | `2,669` | `0.963` | `-1.525` | `+184.54` | `-0.99` | `0.99` |
| **`w26_oos`** | `arcus` (`USDC`, `1.425/4.275`) | `G4_all_hours` | 1 (10.2) | `1,479` | `0.835` | `-1.896` | `+356.27` | `-1.05` | `1.05` |
| **`w26_oos`** | `bulk` (`USDC`, `book_diff_diag`) | `G4_all_hours` | 1 (10.2) | `25,310` | `0.223` | `-2.502` | `+147.39` | `-7.46` | `7.46` |

### 5.2 RWA (`AAPL, SPY, QQQ, NVDA, TSLA, DRAM, SNDK`) — `full_dev` (`15w`), `holdout` (`4w`), `w26` OOS (`1w`)

| Split | Venue (`Sett.`, Verified Fee bps) | Arm | `w` (`h`) | `ntl` ($) | `s` | `gross` (bp) | `$/M` Double-Sided | `net` ($) | `max_dd` ($) |
| --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `full_dev` | `lighter` (`USDC`, `0.40/2.80`) | `S0_all_hours` | 15 (153.7) | `31,045,918` | `0.595` | `-0.560` | `+139.41` | `-8,655.94` | `8,655.94` |
| `full_dev` | `lighter` (`USDC`, `0.40/2.80`) | `G4_all_hours` | 15 (153.7) | `17,689,566` | `0.449` | `-0.100` | `+98.94` | `-3,500.42` | `3,500.42` |
| `full_dev` | `lighter` (`USDC`, `0.40/2.80`) | `G4_quiet_sessions_only` | 15 (153.7) | `12,610,799` | `0.476` | `-0.105` | `+102.35` | `-2,581.54` | `2,581.54` |
| `full_dev` | `lighter_robinhood` (`USDG`, `0/0`) | `S0_all_hours` | 15 (153.7) | `58,939,332` | `0.289` | `-0.407` | `+20.33` | `-2,396.86` | `2,396.86` |
| `full_dev` | `lighter_robinhood` (`USDG`, `0/0`) | **`G4_all_hours`** | 15 (153.7) | `28,996,902` | `0.260` | `-0.247` | **`+12.37`** | `-717.29` | `717.29` |
| `full_dev` | `lighter_robinhood` (`USDG`, `0/0`) | `G4_quiet_sessions_only` | 15 (153.7) | `22,336,914` | `0.259` | `-0.260` | `+13.01` | `-581.40` | `581.40` |
| `full_dev` | `lighter_robinhood` (`USDG`, `0/0`) | `G4_us_rth_only` | 15 (153.7) | `6,694,325` | `0.265` | `-0.204` | `+10.21` | `-136.75` | `136.75` |
| `full_dev` | `arcus` (`USDC`, `0/2.1375`) | `G4_all_hours` | 15 (153.7) | `554,652` | `0.823` | `-1.864` | `+181.15` | `-200.95` | `200.95` |
| `full_dev` | `bulk` (`USDC`, `book_diff_diag`) | `G4_all_hours` | 20 (204.9) | `32,411,814` | `0.322` | `-1.035` | `+83.96` | `-5,442.73` | `5,442.73` |
| `holdout (w21–w25)` | `lighter` (`USDC`, `0.40/2.80`) | `G4_quiet_sessions_only` | 4 (41.0) | `3,316,176` | `0.515` | `-0.218` | `+112.70` | `-747.48` | `747.48` |
| `holdout (w21–w25)` | `lighter_robinhood` (`USDG`, `0/0`) | `S0_all_hours` | 4 (41.0) | `13,993,329` | `0.343` | `-0.447` | `+22.33` | `-624.90` | `624.90` |
| `holdout (w21–w25)` | `lighter_robinhood` (`USDG`, `0/0`) | **`G4_quiet_sessions_only`** | 4 (41.0) | `6,747,980` | `0.254` | `-0.248` | **`+12.39`** | `-167.28` | `167.28` |
| `holdout (w21–w25)` | `arcus` (`USDC`, `0/2.1375`) | `G4_all_hours` | 4 (41.0) | `180,912` | `0.798` | `-1.695` | `+170.04` | `-61.52` | `61.52` |
| `holdout (w21–w25)` | `bulk` (`USDC`, `book_diff_diag`) | `G4_all_hours` | 5 (51.2) | `10,404,550` | `0.189` | `-1.112` | `+74.48` | `-1,549.78` | `1,549.78` |
| **`w26_oos`** | `lighter` (`USDC`, `0.40/2.80`) | `S0_all_hours` | 1 (10.2) | `1,468,704` | `0.688` | `-0.395` | `+142.26` | `-417.88` | `417.88` |
| **`w26_oos`** | `lighter` (`USDC`, `0.40/2.80`) | `G4_all_hours` | 1 (10.2) | `1,014,236` | `0.539` | `-0.095` | `+109.45` | `-222.02` | `222.02` |
| **`w26_oos`** | `lighter_robinhood` (`USDG`, `0/0`) | `S0_all_hours` | 1 (10.2) | `2,987,509` | `0.384` | `-0.150` | `+7.48` | `-44.67` | `44.67` |
| **`w26_oos`** | `lighter_robinhood` (`USDG`, `0/0`) | **`G4_all_hours`** | 1 (10.2) | `1,768,600` | `0.348` | `-0.151` | **`+7.56`** | `-26.76` | `26.76` |
| **`w26_oos`** | `lighter_robinhood` (`USDG`, `0/0`) | **`G4_us_rth_only`** | 1 (10.2) | `432,712` | `0.401` | `-0.096` | **`+4.78`** | `-4.13` | `4.13` |
| **`w26_oos`** | `arcus` (`USDC`, `0/2.1375`) | `G4_all_hours` | 1 (10.2) | `70,221` | `0.870` | `-1.789` | `+182.43` | `-25.62` | `25.62` |
| **`w26_oos`** | `bulk` (`USDC`, `book_diff_diag`) | `G4_all_hours` | 1 (10.2) | `1,368,914` | `0.277` | `-1.314` | `+93.42` | `-255.77` | `255.77` |

- **Top RWA Symbol Impact (`G4_quiet_sessions_only`)**: `lighter AAPL` (`0.40/2.80`) flips from `-$6.28/M` (`+$27.85`, `12/15` pos) to **`+$92.16/M` (`-$408.99`, `0/15` pos)** in `full_dev`, `+$99.50/M` (`-$139.67`) in `w21–w25`, and `+$92.48/M` (`-$26.05`) in `w26` OOS. On `lighter_robinhood` (`0/0` USDG), `SPY` runs `+$8.41/M` (`full_dev`) / `+$8.04/M` (`w21–w25`) / **`+$4.58/M`** (`w26`), and `QQQ` runs `+$10.13/M` (`full_dev`) / `+$9.19/M` (`w21–w25`) / **`+$4.42/M`** (`w26`).
- **`w03` Crash Highlight (`Track 03`)**: `lighter ETH` `S0` `+$111.04/M` (`-$250.37`) → `G4_low_vol_q25` `+$59.16/M` (`-$22.81`); `lighter_robinhood ETH` `S0` `+$89.09/M` (`-$55.85`) → `G4_all_hours` `+$17.44/M` (`-$3.71`).

---

## 6. RWA Universe Screener & Scenario 1 Book-Diff Cost Audit

### 6.1 Sample-Weighted RWA Screener ([screen_rwa_mm_universe.py](../scripts/screen_rwa_mm_universe.py))

| Scope | Legacy Unverified Whitelist (`0/0` on `lighter`) | Verified-Fee `OPT-01` Whitelist (`0.4/2.8` on `lighter`) | Impact of `OPT-01` Fee Evidence |
| --- | --- | --- | --- |
| **`W01–W26` (`20w`)** | **6 pass / 17 excl:** `arcus DRAM` (`2.4048`), `rh MU` (`1.5905`), `rh NVDA` (`0.7700`), **`lighter QQQ` (`0.7058`)**, `rh AAPL` (`0.4610`), `arcus NVDA` (`0.1747`) | **5 pass / 18 excl:** `arcus DRAM` (`2.4048`), `rh MU` (`1.5905`), `rh NVDA` (`0.7700`), `rh AAPL` (`0.4610`), `arcus NVDA` (`0.1747`) | **`lighter QQQ` eliminated; 0/7 `lighter` pass** (`nonzero_maker_fee` + `half_spread < 0.25bp` on 6/7) |
| **`W18–W25` (`7w`)** | `—` | **5 pass / 18 excl:** `arcus DRAM` (`1.9080`), `rh AMZN` (`1.5762`), `rh MU` (`1.3860`), `rh NVDA` (`0.6835`), `rh AAPL` (`0.4552`) | `lighter_robinhood` (`USDG`) owns 4/5 slots; `0/7` `lighter` pass |
| **`W26` OOS (`1w`)** | `—` | **8 pass / 15 excl:** `rh MU` (`2.3024`), `rh AMZN` (`1.6244`), `rh NVDA` (`1.5856`), `arcus SNDK` (`0.7193`), `rh AAPL` (`0.5630`), `rh SPY` (`0.5542`), `rh QQQ` (`0.5179`), `arcus NVDA` (`0.1941`) | `lighter_robinhood` passes 6/7 symbols (`TSLA` fails `L1 $241.03 < $300`); `arcus DRAM` misses on `44.1 < 50 tph` |

### 6.2 Scenario 1 Volume-Farming Cost Audit ([eval_volume_farming_cost.py](../scripts/eval_volume_farming_cost.py))

On `p1_pipeline_bookdiff_holdout.json` best arm **`S1_markout`** (`gross_ex_fee_bps = -0.4057 bp`, inferred `s = 0.184539`, `rebate_bps = 0.0`):

- **`lighter_robinhood` (`0.00 / 0.00 bps` verified Standard USDG):** **`+$20.28 / M`** double-sided (floor `$20.28 / M`).
- **`extended` RWA (`0.00 / 0.90 bps` verified):** **`+$28.59 / M`** (floor `$20.28 / M`).
- **`arcus` `ANT-USD` (`0.00 / 1.064 bps` verified 5% disc.):** **`+$30.10 / M`** (vs `+$30.62 / M` at `1.12` Tier 0); **`arcus` RWA (`0.00 / 2.1375 bps`):** **`+$40.01 / M`**.
- **`trade_xyz` `growthMode` (`0.30 / 0.90 bps` verified):** **`+$55.82 / M`** (floor `$50.28 / M`; standard `3.0 / 9.0` = `+$375.65 / M`).
- **`lighter` (`0.40 / 2.80 bps` verified Premium USDC):** **`+$82.43 / M`** (floor `$60.28 / M`) — **4.06x higher** than the pre-OPT-01 `0/0 bps` assumption (`+$20.28 / M`).
- **`hyperliquid` (`1.50 / 4.50 bps` verified):** **`+$197.96 / M`** (floor `$170.28 / M`).

---

## 7. Before-vs-After Sensitivity Summary & Artifact Provenance

| Track / Venue | Pre-OPT-01 Assumption | Post-OPT-01 Verified Result | Economic Conclusion |
| --- | --- | --- | --- |
| Track 01 `lighter` (`USDC`) | `0/0` (`+$196.60` `full_dev` / `+$49.11` `w21–w25` pv`$1k`) | `0.4/2.8` (**`-$451.12` / `-$88.27` / `-$11.58` `w26`** pv`$1k`) | **NO-GO** (`2.8bp` taker destroys lead-lag alpha) |
| Track 01 `lighter_robinhood` (`USDG`) | `0/0` (`+$67.62` `full_dev` / `+$10.21` `w21–w25` pv`$1k`) | `0/0` confirmed (`+$67.62` / `+$10.21` / `-$0.10` `w26` pv`$1k`) | Marginal positive in `full_dev`/`w21–w25`; thin `w26` prints; keep **HOLD** |
| Track 02 `USDC/USDC` WS distinct | `upgraded_track2`: `-$4,514.26` `full_dev` / `-$1,353.50` `w21–w25` | `upgraded_track2`: **`-$5,940.49` / `-$1,693.02` / `-$161.69` `w26`** | **NO-GO** (hedge taker fees + L1 slip dominate) |
| Track 03 `lighter` RWA (`G4_all_hours`) | `0/0` (`+$5.00/M` `full_dev`, `AAPL -$6.28/M`) | `0.4/2.8` (**`+$98.94/M`** `full_dev`, `+$109.45/M` `w26`; `AAPL +$92.16/M`) | **NO-GO** (`0.4bp` maker fee destroys passive edge) |
| Track 03 `lighter_robinhood` RWA (`G4`) | `0/0` (`+$12.37/M` `full_dev` / `+$12.39/M` `w21–w25`) | `0/0` confirmed (`+$12.37/M` / `+$12.39/M` / **`+$7.56/M` `w26`**) | Lowest verified turnover cost (`$4.78–12.60/M`), still > `$0/M` at `rebate=0` |

### Related Docs & Pulled Local Artifacts (Untracked `.agent-state/`)

- Prior unverified baseline reports: [TRACK01_MULTIVENUE_QUANT_UPGRADE_2026_11_27.md](TRACK01_MULTIVENUE_QUANT_UPGRADE_2026_11_27.md), [TRACK02_MULTIVENUE_ARB_UPGRADE_2026_11_27.md](TRACK02_MULTIVENUE_ARB_UPGRADE_2026_11_27.md), [TRACK03_MULTIVENUE_TURNOVER_UPGRADE_2026_11_27.md](TRACK03_MULTIVENUE_TURNOVER_UPGRADE_2026_11_27.md), [SCENARIO1_RWA_REAL_REPLAY_RESULTS_2026_11_27.md](SCENARIO1_RWA_REAL_REPLAY_RESULTS_2026_11_27.md), and shared resolver [track_fee_evidence.py](../scripts/track_fee_evidence.py).
- Redacted `OPT-01` evidence SHA-256: `4d3a9c1aeae5358bb9c55ab448db392ac3edb0a89244f74c23d89817c3359d92` (`account_fee_evidence_20261127.json`).
- Pulled `W01–W26` JSON SHA-256 digests (`verified_fee_w01_w26_20261127/`, excluded from Git):
  - `track01_verified_fee_w01_w26.json`: `0f43f622054b29a6c30d636a096517c7531536c496a30aec8177fdc2e7a9c4ad`
  - `track02_verified_fee_w01_w26.json`: `89711a62dd9f922d912b955e67826caa7790c04436affc968d1fceba55a69e7b`
  - `track02_verified_fee_w26_oos.json`: `c82c943bd055c1201e47467a9e9c095c02ca6f35e74661e068506c183e71a2b9`
  - `track03_verified_fee_full_dev.json`: `89deb70aa3c6cf767d5ec77584072cd4fea06e9b0d5eec708b113dad0b23c2ca` (`3,403` rows, `non_flat=0`)
  - `track03_verified_fee_holdout_w21_w25.json`: `dca493fee0471dc841fa98ee3ab0bda7ca0510d684c5490e9b4d39b8d38eaa43` (`890` rows, `non_flat=0`)
  - `track03_verified_fee_w26_oos.json`: `87e3a39882dc851bdbb12e75ca4ffc2cbd02da8e4e93cc8edd0f62c6e4fad9e1` (`195` rows, `non_flat=0`)
  - `rwa_whitelist_w01_w26_verified_fee.json`: `eb7a85a65210cb9e0df294eab2bccce86ff34764706b65f3ef565cb6f2fe5c49`
  - `rwa_whitelist_w26_oos_verified_fee.json`: `e5573a033faff2b5179a6a242c73c05d868c14122914ebe679296b77494313ea`
  - `eval_volume_farming_cost_audit.json`: `3576cbb697e6943a00fe2c58996b04968e668becf8821910d1d600a32aa38106`
