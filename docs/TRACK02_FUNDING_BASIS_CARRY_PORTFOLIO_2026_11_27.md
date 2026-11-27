# Track 02 v2 (`02_hedge_arbitrage`): Joint Multi-Hour Funding Carry + Live Executable Basis-Timing Portfolio (`W01–W26`)

**Date:** `2026-11-27` (CST) · **Operational Verdict:** **`NO-GO (HOLD)`** for live deployment · **`CONDITIONAL`** research edge only in `USDG/USDC` RWA (`lighter_robinhood` ↔ `trade_xyz`, `fx_drag <= 1 bp`)
**Owned Files:**
- [src/strategy/funding_basis_carry.rs](../src/strategy/funding_basis_carry.rs) & [src/strategy/mod.rs](../src/strategy/mod.rs) (`pub mod funding_basis_carry;`)
- [scripts/research_funding_basis_carry_portfolio.py](../scripts/research_funding_basis_carry_portfolio.py)
- [scripts/test_research_funding_basis_carry_portfolio.py](../scripts/test_research_funding_basis_carry_portfolio.py)
- [docs/TRACK02_FUNDING_BASIS_CARRY_PORTFOLIO_2026_11_27.md](TRACK02_FUNDING_BASIS_CARRY_PORTFOLIO_2026_11_27.md)

---

## 1. Executive Summary

We designed and evaluated Track 02 v2 (`funding_basis_carry`): a causal joint strategy that combines **multi-hour EWMA cross-venue funding differentials (`bps/hr`)** over horizon `H` with **live executable L1 basis Z-score timing** (`exec_basis_bps = short_leg_bid - long_leg_ask`), replacing earlier synthetic 1–2 bp slippage assumptions with **actual Aleph `W01–W26` L1 `bbo_quotes` `bid/ask` & displayed depth**, **`330 ms` taker execution latency**, **`public_trades` strict trade-through print verification** for `200 ms`-delayed passive entries, and **tape-synchronized stale-snapshot re-centering** on 12.4s-cadence venues (`trade_xyz`, `hyperliquid`, `entropy`).

Across **25 Crypto campaigns (`w01..w26` ex `w08`, ~256h)** and **20 RWA campaigns (`w01..w05, w07, w09, w13..w21, w23..w26`, ~205h)** covering **66 overlapping venue-pair × symbol combinations** across **21 symbols** (**37 distinct-domain audited pairs**):

1. **Naive Funding Carry (`C0_naive_carry`) Is Dead Under Real Costs:** Pure funding carry clears realistic two-leg taker fees + live L1 spreads in **0 trades** across `USDC/USDC` distinct-domain (under verified `lighter` Prem `0.40/2.80`), **0 trades** across `USDG/USDC` distinct-domain, and loses **`-$6.43`** (RWA) in `same_risk_domain`. Even under `lighter` Std `0/0` counterfactual, `C0` records `$0.00` in `holdout (w21–w25)` and `w26_oos`.
2. **Joint Funding + Basis-Timing Passive-First (`C2`) + Inverse-Vol Top-5 (`C3`) Creates Conditional RWA Edge (`USDG/USDC`):**
   - On **`USDG/USDC` distinct-domain RWA** (`lighter_robinhood` `0.0/0.0` ↔ `trade_xyz` `0.30/0.90` `growthMode`, 15 pairs):
     - **`0 bp` FX:** `C2_basis_timed_passive_first` improves from `C0 = $0.00` to **`+$10.54`** (`selection_dev`, `53/1130` passive fills) → **`+$1.02`** (`holdout`) → **`+$0.16`** (`w26_oos`). `C3_portfolio_inv_vol_top5` (`$10,000` capital, `<= 1.0x` gross, `<= 0.40` single-symbol cap, zero cross-subsidy) achieves **`+$11.33`** (`selection_dev`) → **`+$2.23`** (`holdout`) → **`+$0.36`** (`w26_oos`), with **`PSR = 1.0`**, **`DSR_eff = 1.0`**, and **`CSCV PBO = 0.0286`**.
     - **`1 bp` FX:** `C2` delivers **`+$7.24`** (`selection_dev`) → **`+$0.81`** (`holdout`) → **`+$0.06`** (`w26_oos`); `C3_top5` delivers **`+$8.37`** (`selection_dev`) → **`+$1.77`** (`holdout`) → **`+$0.14`** (`w26_oos`), with **`DSR_eff = 0.9963`** and **`CSCV PBO = 0.0714`**.
     - **`3 bp` FX:** FX drag (`$6.83` dev / `$0.16` holdout on `C2`; `$13.18` dev on `C3_top5`) erodes the edge to **`+$0.48` / `+$0.19` / `$0.00`** (`C3_top5`), degrading **`DSR_eff = 0.3266`** and **`PBO = 0.6571`**.
3. **Why Overall Verdict Remains `NO-GO (HOLD)`:**
   - **Thin OOS Trade Count & Small Economic Magnitude:** In `USDG/USDC` RWA `holdout (w21–w25)` and `w26_oos`, only `4` trades (`MU`) and `1` trade (`NVDA`) cleared the joint hurdle (`+$2.23` holdout / `+$0.36` w26 on `$10k` capital), which is too thin relative to `12.4s` snapshot book cadence on `trade_xyz` and unverified live USDG↔USDC FX conversion cost (`>= 3 bp` kills the edge).
   - **`USDC/USDC` & Crypto Fail OOS:** Verified `lighter` Prem (`0.40/2.80`) blocks `USDC/USDC` OOS activity (`$0.00` holdout/w26), and even under `lighter` Std `0/0` counterfactual, Crypto `USDC/USDC` `C2` loses **`-$1.73`** (`holdout`) and **`-$1.88`** (`w26_oos`, `PBO = 0.5095`). Crypto `USDG/USDC` (`lighter_robinhood ↔ hyperliquid`) has only `1` dev trade (`PBO = 0.4083–0.7075`).
   - **All Terminal Positions Flat:** Every arm across every split satisfies `terminal_flat_all = True` and `terminal_residual_notional_usd = $0.00`.

---

## 2. Rust Strategy Implementation ([src/strategy/funding_basis_carry.rs](../src/strategy/funding_basis_carry.rs))

Added `pub mod funding_basis_carry;` to [src/strategy/mod.rs](../src/strategy/mod.rs) with **zero `crate::exchanges::*` imports**, **zero hot-path heap allocations** (`Copy` structs/enums), and **`#[serde(default, deny_unknown_fields)]`**:

- **Joint Causal Signal & Hurdle (`FundingBasisCarryController::evaluate_signal`):**
  - Tracks strictly prior causal EWMA cross-venue funding differential (`funding_ema_bps_hr` over `funding_tau_ms`) alongside strictly prior causal fair mid-basis (`fair_mid_basis_bps`) and basis volatility (`basis_sigma_bps`).
  - Computes live executable L1 basis (`exec_basis_bps` from `short_leg_bid - long_leg_ask`) and `basis_zscore = (dir_mid_basis_bps - dir_fair_bps) / max(prior_sigma, basis_vol_floor_bps)`.
  - Approves entry only when `expected_carry_bps >= min_expected_carry_bps`, `basis_zscore >= min_basis_zscore`, and `expected_carry_bps + basis_timing_advantage_bps >= hurdle_mult * (rt_fee_bps + rt_spread_bps + fx_drag_bps)`, subject to displayed L1 depth `>= min_l1_notional_usd`, explicit USDG/USDC FX bound (`fx_drag_bps <= max_fx_drag_bps`), and per-pair cap (`requested_notional_usd <= max_pair_notional_usd`).
- **Execution FSM (`PassiveFirstCarryExecState`):**
  - `Idle -> WorkingPassiveLeg -> HedgingTakerLeg -> ActiveCarry -> Unwinding -> FailClosedHalt`.
  - Automatically rests passive entry on the wider-spread leg (`200 ms` placement latency), cancels cleanly to `Idle` on `PassiveTimeout` or `PassiveHazardEscalation`, latches non-auto-resetting `FailClosedHalt` on `NonMonotonicClock`, `StaleBook`, or `InvalidInput` during open execution (requiring `acknowledge_reconciled_reset`), and triggers `BasisConvergenceTakeProfit`, `AdverseFundingFlip`, `BasisStopLoss`, or `MaxHoldExpired` exits.
  - **Cancel-vs-Fill Race Precedence (`on_passive_fill` authoritative):** During a passive entry cancel (`PassiveTimeout` or `PassiveHazardEscalation`), an in-flight cancel-vs-fill race MUST be resolved with **`on_passive_fill` taking precedence**. Never transition back to `Idle` on cancel dispatch alone; wait for authoritative terminal cancel confirmation proving zero fill, and if a late fill arrives in the interim, call `on_passive_fill` immediately to advance `WorkingPassiveLeg -> HedgingTakerLeg` and dispatch the taker hedge rather than stranding an unhedged leg. `on_taker_hedge_complete` enforces the same monotonic clock guard as `on_passive_fill` and `on_unwind_complete`.
- **Unit Tests:** `4/4` passing in `cargo test --locked --features research-replay --lib funding_basis_carry` (basis timing veto, passive-first fail-closed hedge on stale book, basis-convergence TP + adverse funding flip exits, FX drag bound, per-pair cap, NaN/Inf/clock-regression rejection).

---

## 3. Fee Provenance, Venue Map & Realism Controls

Loaded from `.agent-state/research/account_fee_evidence_20261127.json` via [track_fee_evidence.py](../scripts/track_fee_evidence.py):

| Venue | Settlement | Verified Fee Tier (`maker / taker` bps) | Counterfactual / Status | BBO Cadence (`max_book_age_ms`) |
| --- | --- | --- | --- | --- |
| `lighter_robinhood` | `USDG` | `0.00 / 0.00` | Verified (`account_fee_status=verified_account_tier`) | `ws_50ms` (`5,000 ms`) |
| `lighter` | `USDC` | `0.40 / 2.80` (`Prem`) | `0.00 / 0.00` (`Std` counterfactual tested separately) | `ws_50ms` (`5,000 ms`) |
| `trade_xyz` (`xyz:*`) | `USDC` | `0.30 / 0.90` (`growthMode`) | Verified (`120/125` active markets scale `1`) | `snapshot_12s` (`20,000 ms`, tape-synced) |
| `hyperliquid` | `USDC` | `1.50 / 4.50` | Verified (`BTC, ETH, SOL`) | `snapshot_12s` (`20,000 ms`, tape-synced) |
| `entropy` (`io:*`) | `USDC` | `0.30 / 0.90` | **Unaudited (`account_evidence_required`)** — excluded from audited cohorts | `snapshot_12s` (`20,000 ms`, tape-synced) |

**Realism Enforcement (`W01–W26`, `-w27`/`-w28` explicitly excluded):**
- **Real L1 Depth & `330 ms` Taker Latency:** Taker legs execute at `t + 330 ms` on actual L1 `bbo_quotes` `bid/ask`, entry size is clamped to `min($1,000, l1_bid_notl, l1_ask_notl)` (min `$100`), and exit notional exceeding displayed L1 depth incurs proportional sweep impact.
- **Tape-Synchronized Snapshot Anti-Illusion (`_sync_quote_with_tape`):** Any `>500 ms` `snapshot_12s` BBO is re-centered on newer `public_trades` prints in `(obs_ms, t_exec]` before pricing or execution, eliminating stale-quote arbitrage illusions.
- **Strict Passive Print Verification:** `C2_basis_timed_passive_first` posts at `t + 200 ms` on the wider-spread leg and fills ONLY on strict trade-through prints in `public_trades`, then hedges at `t_fill + 330 ms`.
- **Cohort & Split Discipline:** Parameters are selected exclusively on `selection_dev (w01–w20)` and frozen once for `holdout (w21–w25)` and `w26_oos`, forcing flat at every split boundary (`terminal_flat_all = True`, `terminal_residual_notional_usd = $0.00`).

---

## 4. Cohort Results (`W01–W26`, Verified `lighter` Prem `0.40/2.80` vs `Std 0/0`)

### 4.1 `USDG/USDC` Distinct-Domain RWA (`lighter_robinhood ↔ trade_xyz`, 15 Pairs, Verified Fees)

*(Identical under `Prem` and `Std` since neither leg is `lighter` Core.)*

| FX Drag | Arm | Split | Net PnL ($) | Funding ($) | Basis ($) | Spread/Slip ($) | Fees ($) | FX Drag ($) | Trades (`Pas/Att`) | Flat (`Res $`) |
| --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| **`0 bp`** | `C0_naive_carry` | `selection_dev` | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 | 0 (`0/0`) | True (`0.0`) |
| **`0 bp`** | `C1_basis_timed_taker` | `selection_dev` | 5.20 | 0.05 | 50.29 | 32.58 | 12.57 | 0.00 | 98 (`0/0`) | True (`0.0`) |
| **`0 bp`** | `C1_basis_timed_taker` | `holdout` / `w26_oos` | 0.00 / 0.00 | 0.00 / 0.00 | 0.00 / 0.00 | 0.00 / 0.00 | 0.00 / 0.00 | 0.00 / 0.00 | 0 / 0 | True (`0.0`) |
| **`0 bp`** | **`C2_basis_timed_passive_first`** | `selection_dev` | **10.54** | 0.70 | 25.75 | 10.43 | 5.47 | 0.00 | 53 (`53/1130`) | True (`0.0`) |
| **`0 bp`** | **`C2_basis_timed_passive_first`** | `holdout` | **1.02** | 0.00 | 1.48 | 0.08 | 0.38 | 0.00 | 4 (`4/116`) | True (`0.0`) |
| **`0 bp`** | **`C2_basis_timed_passive_first`** | `w26_oos` | **0.16** | -0.02 | 0.53 | 0.17 | 0.18 | 0.00 | 1 (`1/31`) | True (`0.0`) |
| **`0 bp`** | **`C3_portfolio_inv_vol_top5`** | `selection_dev` | **11.33** | 0.81 | 24.16 | 7.39 | 6.23 | 0.00 | — (`maxSym=0.3565`) | True (`0.0`) |
| **`0 bp`** | **`C3_portfolio_inv_vol_top5`** | `holdout` | **2.23** | 0.00 | 3.22 | 0.17 | 0.82 | 0.00 | — (`maxSym=0.2201`) | True (`0.0`) |
| **`0 bp`** | **`C3_portfolio_inv_vol_top5`** | `w26_oos` | **0.36** | -0.05 | 1.16 | 0.36 | 0.39 | 0.00 | — (`maxSym=0.2168`) | True (`0.0`) |
| **`1 bp`** | `C2_basis_timed_passive_first` | `selection_dev` | **7.24** | 0.76 | 25.45 | 10.26 | 5.42 | 3.29 | 52 (`52/1187`) | True (`0.0`) |
| **`1 bp`** | `C2_basis_timed_passive_first` | `holdout` / `w26_oos` | **0.81 / 0.06** | 0.00 / -0.02 | 1.48 / 0.53 | 0.08 / 0.17 | 0.38 / 0.18 | 0.21 / 0.10 | 4 (`4/125`) / 1 (`1/31`) | True (`0.0`) |
| **`1 bp`** | **`C3_portfolio_inv_vol_top5`** | `selection_dev` | **8.37** | 1.14 | 25.57 | 7.53 | 6.69 | 4.13 | — (`maxSym=0.4000`) | True (`0.0`) |
| **`1 bp`** | **`C3_portfolio_inv_vol_top5`** | `holdout` / `w26_oos` | **1.77 / 0.14** | 0.00 / -0.05 | 3.20 / 1.15 | 0.17 / 0.36 | 0.82 / 0.39 | 0.45 / 0.22 | — (`maxSym=0.2192/0.2159`) | True (`0.0`) |
| **`3 bp`** | `C2_basis_timed_passive_first` | `selection_dev` | **5.75** | 0.60 | 22.36 | 6.78 | 3.60 | 6.83 | 34 (`34/737`) | True (`0.0`) |
| **`3 bp`** | `C2_basis_timed_passive_first` | `holdout` / `w26_oos` | **0.08 / 0.00** | 0.00 / 0.00 | 0.45 / 0.00 | 0.10 / 0.00 | 0.10 / 0.00 | 0.16 / 0.00 | 2 (`2/78`) / 0 (`0/14`) | True (`0.0`) |
| **`3 bp`** | `C3_portfolio_inv_vol_top5` | `selection_dev` / `holdout` / `w26` | 0.48 / 0.19 / 0.00 | 1.56 / 0.00 / 0.00 | 26.80 / 1.00 / 0.00 | 7.86 / 0.23 / 0.00 | 6.84 / 0.22 / 0.00 | 13.18 / 0.36 / 0.00 | — (`maxLev=1.0`) | True (`0.0`) |

### 4.2 `USDC/USDC` Distinct-Domain (`lighter ↔ trade_xyz` RWA, `lighter ↔ hyperliquid` Crypto)

| Fee Regime | Domain | Arm | `selection_dev` ($) | `holdout` ($) | `w26_oos` ($) | Funding (`dev/hold/w26`) | Trades (`dev/hold/w26`) |
| --- | --- | --- | ---: | ---: | ---: | --- | --- |
| **`verified_prem` (`0.4/2.8`)** | `crypto` | `C0_naive_carry` | 0.00 | 0.00 | 0.00 | 0.00 / 0.00 / 0.00 | 0 / 0 / 0 |
| **`verified_prem` (`0.4/2.8`)** | `crypto` | `C1_basis_timed_taker` | 0.00 | 0.00 | 0.00 | 0.00 / 0.00 / 0.00 | 0 / 0 / 0 |
| **`verified_prem` (`0.4/2.8`)** | `crypto` | `C2_basis_timed_passive_first` | 0.78 | 0.00 | 0.00 | 0.00 / 0.00 / 0.00 | 2 / 0 / 0 |
| **`verified_prem` (`0.4/2.8`)** | `crypto` | `C3_top3` / `C3_top5` | 2.61 | 0.00 | 0.00 | 0.00 / 0.00 / 0.00 | — |
| **`verified_prem` (`0.4/2.8`)** | `rwa` | `C0_naive_carry` | 0.00 | 0.00 | 0.00 | 0.00 / 0.00 / 0.00 | 0 / 0 / 0 |
| **`verified_prem` (`0.4/2.8`)** | `rwa` | `C1_basis_timed_taker` | 1.72 | 0.00 | 0.00 | 0.00 / 0.00 / 0.00 | 6 / 0 / 0 |
| **`verified_prem` (`0.4/2.8`)** | `rwa` | `C2_basis_timed_passive_first` | -17.41 | 0.00 | 0.00 | 0.00 / 0.00 / 0.00 | 2 / 0 / 0 |
| **`verified_prem` (`0.4/2.8`)** | `rwa` | `C3_top3` / `C3_top5` | 0.56 | 0.00 | 0.00 | 0.00 / 0.00 / 0.00 | — |
| **`standard_zero` (`0/0` cf.)** | `crypto` | `C1_basis_timed_taker` | 9.14 | 0.00 | 0.00 | 0.01 / 0.00 / 0.00 | 2 / 0 / 0 |
| **`standard_zero` (`0/0` cf.)** | `crypto` | `C2_basis_timed_passive_first` | 2.21 | -1.73 | -1.88 | 0.00 / 0.06 / 0.01 | 3 / 4 / 1 |
| **`standard_zero` (`0/0` cf.)** | `rwa` | `C0_naive_carry` | 3.08 | 0.00 | 0.00 | 0.24 / 0.00 / 0.00 | 10 / 0 / 0 |
| **`standard_zero` (`0/0` cf.)** | `rwa` | `C1_basis_timed_taker` | -7.47 | 0.00 | 0.00 | -0.60 / 0.00 / 0.00 | 143 / 0 / 0 |
| **`standard_zero` (`0/0` cf.)** | `rwa` | `C2_basis_timed_passive_first` | -15.72 | 0.00 | 0.00 | 0.63 / 0.00 / 0.00 | 51 / 0 / 0 |

### 4.3 `USDG/USDC` Distinct-Domain Crypto (`lighter_robinhood ↔ hyperliquid`) & Segregated `same_risk_domain`

- **`USDG_USDC_distinct` Crypto (`0 bp` FX):** `C0 = $0.00` all splits; `C1 = +$13.38` (`selection_dev`, 1 trade, `top1% = 100.0`) → `$0.00` (`holdout` & `w26_oos`); `C3_top5 = +$44.59` (`selection_dev`) → `$0.00` (`holdout` & `w26_oos`). At `3 bp` FX (`hm=1.2` winner): `C1 = +$13.08` (`selection_dev`) → `+$0.59` (`holdout`, 1 `ETH` trade) → `$0.00` (`w26_oos`).
- **`same_risk_domain` (`lighter ↔ lighter_robinhood` & unaudited `trade_xyz ↔ entropy`):**
  - Crypto (`Prem`): `C2 = +$0.89` (`selection_dev`) → `$0.00` (`holdout`) → `-$0.09` (`w26_oos`); `C3_top5 = +$3.44` → `$0.00` → `-$0.35`. Under `Std 0/0`: `C3_top5 = +$4.93` (`selection_dev`) → `-$4.67` (`holdout`).
  - RWA (`Prem`): `C0 = -$4.24` (`selection_dev`), `C1 = -$47.97` / `-$3.11` / `-$0.71`, `C2 = -$5.73` / `+$1.69` / `+$0.09`, `C3_top5 = -$6.94` / `$0.00` / `$0.00`.

---

## 5. Multiple-Testing & Overfitting Audit (`PSR / N_eff / DSR / MinTRL / CSCV PBO`)

Computed via [audit_multiple_testing_overfitting.py](../scripts/audit_multiple_testing_overfitting.py) across aligned `w01–w26` window returns:

| Fee Regime | Cohort | Domain | Varying / Raw Trials | `N_eff` | Best Trial | `SR` | `PSR(0)` | `DSR(N_eff)` | `MinTRL(95%)` | `CSCV PBO` |
| --- | --- | --- | ---: | ---: | --- | ---: | ---: | ---: | ---: | ---: |
| `verified_prem` | `USDG_USDC_distinct_fx0bp` | `rwa` | 44 / 46 | 2.65 | `C3_portfolio_inv_vol_top5` | 0.9171 | **1.0000** | **1.0000** | 2.5 | **0.0286** |
| `verified_prem` | `USDG_USDC_distinct_fx1bp` | `rwa` | 42 / 46 | 3.86 | `C3_portfolio_inv_vol_top5` | 0.7648 | **1.0000** | **0.9963** | 2.9 | **0.0714** |
| `verified_prem` | `USDG_USDC_distinct_fx3bp` | `rwa` | 39 / 46 | 4.57 | `C2_basis_timed_passive_first` | 0.2698 | 0.9645 | 0.3266 | 16.8 | **0.6571** |
| `verified_prem` | `USDG_USDC_distinct_fx0bp` | `crypto` | 18 / 46 | 1.21 | `C1_basis_timed_taker` | 0.2374 | 0.9920 | 0.9920 | 12.2 | **0.6757** |
| `verified_prem` | `USDG_USDC_distinct_fx3bp` | `crypto` | 14 / 46 | 1.11 | `C1_basis_timed_taker` | 0.2408 | 0.9829 | 0.9829 | 15.5 | **0.7075** |
| `verified_prem` | `USDC_USDC_distinct` | `crypto` | 6 / 46 | 1.00 | `C3_portfolio_inv_vol_top3` | 0.2000 | 0.9676 | 0.9676 | 20.0 | **1.0000** |
| `verified_prem` | `USDC_USDC_distinct` | `rwa` | 36 / 46 | 3.05 | `C1_basis_timed_taker` | 0.3370 | 0.9980 | 0.8939 | 7.2 | **0.1571** |
| `standard_zero` | `USDC_USDC_distinct` | `crypto` | 18 / 46 | 1.78 | `C2_basis_timed_passive_first` | 0.2246 | 0.9877 | 0.9034 | 13.8 | **0.5095** |
| `standard_zero` | `USDC_USDC_distinct` | `rwa` | 44 / 46 | 4.06 | `C0_naive_carry` | 0.0657 | 0.6203 | 0.3246 | 549.4 | **0.5286** |
| `verified_prem` | `same_risk_domain` | `crypto` | 24 / 46 | 3.42 | `C2_basis_timed_passive_first` | 0.3185 | 0.9960 | 0.7338 | 10.2 | **0.1280** |
| `verified_prem` | `same_risk_domain` | `rwa` | 46 / 46 | 4.49 | `C3_portfolio_inv_vol_top3` | -0.2239 | 0.0348 | 0.0001 | — | **0.3714** |

---

## 6. Verification & Artifacts

- **Pulled Redacted Artifact** (untracked `.agent-state/research/quant_upgrade_20261127/track02_funding_basis_carry_w01_w26.json`, `LEAK_COUNT = 0`):
  - `sha256: 91e27fe85e5dd03dc66503d76d0ca1d3cd5afeba4ede3b069b6f7e342131e857`
- **Checks Executed:**
  - `make fmt-check`
  - `make check` (`cargo clippy --locked --features research-replay --lib --bins -- -D warnings`, `go vet ./...`)
  - `cargo test --locked --features research-replay --lib`
  - `python3 -S scripts/research_funding_basis_carry_portfolio.py --self-test`
  - `python3 -S -m unittest -v scripts/test_research_funding_basis_carry_portfolio.py`
  - Programmatic `MISMATCH_COUNT = 0` cross-check against `track02_funding_basis_carry_w01_w26.json`, `BAD_LINKS = []`, and `git diff --check`.
