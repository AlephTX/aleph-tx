# Track 02: Remaining P0 Boundaries & `Core ↔ Arcus` `B0` Settleability / Collateral / Zero-Funding-Boundary Audit (`W01–W27`)

**Date:** `2026-11-27` (CST) · **Operational Verdict:** **`HOLD`** (no live orders, cancellations, or deploys; `-w28` and crypto `w08` excluded; `B0` stays **`HOLD / PENDING_ARCUS_FUNDING_WITNESS`**)
**Owned Files:**
- [scripts/research_funding_basis_carry_portfolio.py](../scripts/research_funding_basis_carry_portfolio.py)
- [scripts/test_research_funding_basis_carry_portfolio.py](../scripts/test_research_funding_basis_carry_portfolio.py)
- [scripts/research_track02_b0_settleability_audit.py](../scripts/research_track02_b0_settleability_audit.py)
- [scripts/test_research_track02_b0_settleability_audit.py](../scripts/test_research_track02_b0_settleability_audit.py)
- [docs/TRACK02_B0_SETTLEABILITY_MARGIN_AND_P0_BOUNDARIES_2026_11_27.md](TRACK02_B0_SETTLEABILITY_MARGIN_AND_P0_BOUNDARIES_2026_11_27.md)

---

## 1. Executive Summary

1. **Remaining Track 02 P0 Boundaries Closed & Verified (`12/12` Unit Tests):**
   - Closed the three remaining simulator boundary defects from `docs/ALEPHTX_RESEARCH_AND_EXECUTION_ROADMAP_2026_11_27.md` §3 #2 in [scripts/research_funding_basis_carry_portfolio.py](../scripts/research_funding_basis_carry_portfolio.py):
     1. `_lookup_bbo_with_recovery` no longer fills an IOC/taker exit or hedge at `0 ms` delay on a recovered future timestamp; upon missing at `t_exec` and finding `recovery_ts > t_exec`, it schedules `t_retry = recovery_ts + TAKER_LATENCY_MS` (`330 ms`), verifies book freshness at `t_retry` within `cutoff_ms` (`split_end_ms`), and otherwise returns `None` to keep exposure explicit or retry on the next iteration.
     2. `_close_position` and `_try_unwind_orphan` enforce strict planned `split_end_ms` cutoffs (`t_exec`, `leg_a_exit_ms`, `leg_b_exit_ms`, `t_retry <= split_end_ms`), removing silent `min(..., split_end_ms)` clamping and recording explicit residual notional and `<= split_end_ms` MTM (or `terminal_residual_mtm_unknown = True` when no valid MTM book exists).
     3. `_load_symbol_funding` and `simulate_carry_pair_timeline` filter all settled and `live_ctx` funding signals strictly by `available_at_ms <= t_dec` (never pre-indexing `live_ctx` by `hr_ms` or `settled` without `available_at_ms`), separate live/predicted rates from settled funding cash, and require `pay_ms > leg_fill_ms` AND `available_at_ms` known (`is not None`) for cash settlement.
2. **`Core ↔ Arcus` (`USDC/USDC`) Non-Linear Clip Scaling (`$500`, `$1,000`, `$2,000` Event-by-Event Replay):**
   - **`$500` RWA (`B0_original`, `b0_e10_v0.0_sh0`) delivers the highest per-notional edge (`+4.039 bps`) and stays positive in every split & `ex-NVDA`:** `+$350.89` (`selection_dev`, 15w) $\to$ **`+$25.74`** (`holdout`, 4w) $\to$ **`+$1.27`** (`w26_w27_oos`, 2w) = **`+$377.90`** (`95% CI [+$132.33, +$762.29]`, `p(>0) = 1.000`; `ex-NVDA`: `+$119.44 / +$12.57 / +$0.63` = `+$132.64`).
   - **`$1,000` RWA (`B0_original`):** `+$482.18 / +$19.95 / +$1.54` = **`+$503.66` (`+3.350 bps`)** (`95% CI [+$131.38, +$1,088.74]`), but `ex-NVDA` turns slightly negative OOS (`+$109.86 / -$0.30 / -$0.52`).
   - **`$2,000` RWA (`B0_original`) fails linear scaling:** total PnL **drops** below `$1,000` to **`+$485.89` (`+2.187 bps`)**, `w26_w27_oos` turns **`-$3.26`**, `ex-NVDA` collapses to **`-$112.61`** (`-$80.64 / -$26.16 / -$5.82`), and orphan unwind losses jump from `-$35.78` (`$500`, `22.68%` rate) $\to$ `-$98.58` (`$1,000`, `25.14%`) $\to$ **`-$228.69` (`$2,000`, `29.26%`)**.
   - **Crypto (`BTC/ETH/SOL`):** `$500` is positive across all splits (**`+$74.43`**, `+1.464 bps`: `+$56.78 / +$12.20 / +$5.45`, `95% CI [+$15.56, +$151.02]`), `$1,000` earns `+$77.62` (`+$72.86 / -$2.43 / +$7.19`, `95% CI [-$5.65, +$192.30]`), and `$2,000` flips negative (**`-$20.70`**, `-$240.63` orphan PnL).
3. **`B0_funding_flat` Zero-Funding-Boundary Proof (`TEN_HOUR_RESEARCH.md`):**
   - Blocking new risk-increasing opens `120 s` before `:00` UTC and forcing dual-leg taker flatten `30 s` before `:00` achieves **`crossed_funding_hours == 0 / 3,045`** (`funding_avoidance_qualified = True`, `funding_cash_usd = $0.00`, `terminal_flat_all = True`) across all three clip sizes in RWA, and `0 / 759` in Crypto at `$1,000` and `$2,000` (`1 / 759` at `$500` due to a dark book before `:00`).
   - However, forcing flat hourly slashes `B0`'s mean hold (`~175 min → ~25 min`) and triggers extra taker flatten fees (`240` forced flattens at `$1,000` RWA): `B0_funding_flat` drops **`+$377.90 → +$230.72`** (`$500`, paired delta `-$147.17`, `95% CI [-$220.23, -$87.87]`, `holdout +$3.99`, `w26_w27_oos -$0.21`), **`+$503.66 → +$262.15`** (`$1,000`, `holdout -$8.19`, `w26_w27_oos -$3.35`, `ex-NVDA -$45.70`), and **`+$485.89 → +$75.74`** (`$2,000`, `holdout -$28.09`, `w26_w27_oos -$7.47`, `ex-NVDA -$308.75`). In Crypto, `B0_funding_flat` is negative at every size (`-$37.47 / -$104.65 / -$257.51`).
   - **Operational Conclusion:** `B0`'s OOS profitability depends materially on holding across `:00` UTC funding boundaries (`2,170 / 3,045` hours crossed at `$1,000` RWA), which remains **unverified** while `Arcus` `funding_and_ctx` is absent in the Aleph lakehouse. Keep **`HOLD`** until `Arcus` funding settlement is captured in the lakehouse.

---

## 2. Deliverable 1: P0 Simulator Boundary Fixes & Counterexample Tests

Updated [scripts/research_funding_basis_carry_portfolio.py](../scripts/research_funding_basis_carry_portfolio.py) and [scripts/test_research_funding_basis_carry_portfolio.py](../scripts/test_research_funding_basis_carry_portfolio.py):

| P0 Boundary | Root Defect Before Fix | Causal Fix Applied | Verification Test |
| --- | --- | --- | --- |
| **1. Future-Liquidity `0 ms` Recovery** | `_lookup_bbo_with_recovery` returned recovered quote at `cand_ms` directly (`0 ms` delay) and `_sync_quote_with_tape` used pre-recovery `t_exec`. | On miss at `t_ms`, scans to first valid `cand_ms`, schedules `t_retry = cand_ms + retry_latency_ms` (`330 ms`), verifies book freshness at `t_retry <= cutoff_ms` via `_lookup_bbo_at`, syncs tape at `t_retry`, and returns `t_retry` (or `None` if dark/crossed/past cutoff). | `test_p0_boundary_stale_ioc_recovery_requeues_taker_latency` |
| **2. Planned Cutoff `split_end_ms`** | `_close_position` / `_try_unwind_orphan` silently clamped `leg_exit_ms = min(exit_ms, split_end_ms)` and allowed post-`split_end_ms` recovery quotes. | Enforces `cutoff_ms = split_end_ms` across all lookup/close/unwind paths; refuses fills with `t_exec`, `t_retry`, `leg_a_exit_ms`, or `leg_b_exit_ms > split_end_ms`, preserving explicit residual notional and `<= split_end_ms` MTM (or `terminal_residual_mtm_unknown`). | `test_p0_boundary_cutoff_split_end_ms_rejects_late_exit_and_keeps_residual`, `test_p0_boundary_partial_hedge_and_unknown_terminal_state` |
| **3. Causal `available_at_ms` Funding Gate** | `_load_symbol_funding` collapsed `live_ctx` by `hr_ms` (`arg_max` over full hour) and `hourly_funding_history` by `pay_ms` without `available_at_ms` filtering. | Stores raw time-ordered `(available_at_ms, rate_bps)` for `live_ctx` (`_lookup_live_ctx_at` bisects `ts <= t_dec`) and `(available_at_ms, pay_ms, rate_bps)` for `settled`; ignores `available_at_ms is None`, prevents late revisions from leaking before `available_at_ms <= t_dec`, and requires `pay_ms > leg_fill_ms` for cash. | `test_p0_boundary_funding_available_at_ms_causal_gate_and_late_revision` |

---

## 3. Deliverable 2 Methodology: `Core ↔ Arcus` (`USDC/USDC`), Fees, Margin & Funding Avoidance

Implemented in [scripts/research_track02_b0_settleability_audit.py](../scripts/research_track02_b0_settleability_audit.py) (pure stdlib top-level imports; `duckdb` & `numpy` lazy-loaded; `NOT regexp_matches(campaign_id, '-w28$')` enforced across all queries):

- **Verified Fee Schedules** (loaded dynamically from `.agent-state/research/account_fee_evidence_20261127.json`, `sha256: 4d3a9c1aeae5358bb9c55ab448db392ac3edb0a89244f74c23d89817c3359d92`):
  - `lighter` (`Core`, `USDC`): verified **`0.40 bps` Maker / `2.80 bps` Taker** (`40 ms` maker delay, `140 ms` taker delay, `140 ms` speedbump).
  - `arcus` (`USDC`): verified `level_0` w/ 5% discount — **RWA `0.00 / 2.1375 bps`**, **Crypto `1.425 / 4.275 bps`** (`36 ms` order delay, `0 ms` speedbump).
- **Primary Lakehouse Coverage vs. Read-Only Public Metadata Probe:**
  - **Primary Lakehouse Facts:** `21` RWA windows (`15` `selection_dev` `w01–w20`, `4` `holdout` `w21–w25`, `2` `w26_w27_oos`, `15` symbols, `3,045` expected hourly boundaries) and `26` Crypto windows (`19` `selection_dev`, `5` `holdout`, `2` `w26_w27_oos`, `3` symbols, `759` expected hourly boundaries; `w08` and `-w28` excluded). `lighter` has `235` hourly funding slots/symbol (RWA) and `293` (Crypto); **`arcus` has `0` `funding_and_ctx` parquet files in the Aleph lakehouse**.
  - **Optional Read-Only Public Probe (`--probe-public-meta`, no auth/keys):** `GET https://api.arcus.xyz/v1/markets` (`87` markets) and `GET https://api.arcus.xyz/v1/fundingRates?market=NVDA-USD&limit=500` confirm Arcus exposes hourly public rates (`500` recent hours, `time` `1793980800000..1795777200000` ms) and live `fundingRate` / `nextFundingRate` / `nextFundingAt`, **without** lakehouse-captured per-fill settlement cash or settlement oracle witness. Lakehouse facts remain primary (`arcus_funding = None` in simulation).
- **Contract Margin & Off-Hours Schedule:**
  - `arcus`: `initialMarginFraction` (`0.02` SPY, `0.04` QQQ/ETH, `0.025` BTC, `0.05` AAPL/AMZN/GOOGL/META/MSFT/NVDA/TSLA/SOL, `0.10` AMD/HOOD/MSTR/MU/PLTR, `0.20` COIN) and `offHoursInitialMarginFraction` (`1.5x` RTH IMF outside `04:00–20:00 America/New_York` Mon–Fri for RWA: e.g., `0.075` NVDA, `0.15` AMD/HOOD/MSTR/MU/PLTR, `0.30` COIN), plus `maintenanceMarginFraction` (`0.0133–0.1333`).
  - `lighter`: `default_initial_margin_fraction` (`0.05` BTC/ETH, `0.0666` NVDA/AAPL/AMZN/GOOGL/META/MSFT/MU/QQQ/SPY/TSLA/SOL, `0.10` AMD/COIN/HOOD/MSTR/PLTR) and `maintenance_margin_fraction` (`0.012–0.06`).
- **Arms Compared on Identical Windows/Events at `$500`, `$1,000`, and `$2,000` Clip Sizes (Frozen `b0_e10_v0.0_sh0`, `Q_MAX_CLIPS = 4.0`, `min_hedge_l1_notional_usd = clip_usd`):**
  - `B0_original`: Original `B0` holding across `:00` UTC hourly boundaries.
  - `B0_funding_flat`: Blocks new risk-increasing opens `[ht - 120s, ht + 5s]` and cancels quotes + forces dual-leg taker flatten `[ht - 30s, ht + 5s]` across every `:00` UTC boundary `ht` (formal `TEN_HOUR_RESEARCH.md` slot-level guard checks).
  - `flat`: Zero-trade baseline (`$0.00` PnL, `$0.00` margin, `0` crossed funding hours).

---

## 4. Empirical Results: Clip Size Non-Linearity & Zero-Funding-Boundary Audit (`W01–W27`)

### 4.1 RWA (`15` Symbols, `21` Windows) — `B0_original` vs. `B0_funding_flat` (`flat = $0.00`)

| Clip | Arm | Split | Fills | Maker Notl ($) | Hedged Notl ($) | Orphans (`Rate`, `Orp PnL $`) | Realized ($) | MTM ($) (`bps`) | `ex-NVDA` MTM ($) | Crossed Hrs | Flat (`Res $`) | Max DD / Worst W ($) |
| --- | --- | --- | ---: | ---: | ---: | --- | ---: | ---: | ---: | ---: | --- | --- |
| `$500` | `B0_original` | `dev / hold / oos` | `1,916 / 217 / 28` | `833,975 / 90,154 / 11,514` | `644,556 / 68,010 / 10,830` | `596` (`22.68%`, `-35.78`) | `+350.82 / +25.74 / +1.27` | **`+350.89 / +25.74 / +1.27`** (`+4.039` all) | **`+119.44 / +12.57 / +0.63`** | `2,240 / 3,045` | `False* (500) / True / True` | `2.53 / -2.32` |
| `$500` | `B0_funding_flat` | `dev / hold / oos` | `1,684 / 162 / 15` | `734,639 / 66,693 / 6,308` | `566,986 / 51,639 / 5,808` | `495` (`22.68%`, `-24.30`) | `+226.94 / +3.99 / -0.21` | **`+226.94 / +3.99 / -0.21`** (`+2.857` all) | `+29.88 / -4.16 / -0.27` | **`0 / 3,045`** | `True (0)` | `10.50 / -4.23` |
| `$1,000` | `B0_original` | `dev / hold / oos` | `1,816 / 173 / 24` | `1,368,164 / 118,046 / 17,255` | `1,020,191 / 89,596 / 15,679` | `616` (`25.14%`, `-98.58`) | `+482.03 / +19.95 / +1.54` | **`+482.18 / +19.95 / +1.54`** (`+3.350` all) | `+109.86 / -0.30 / -0.52` | `2,170 / 3,045` | `False* (1,000) / True / True` | `5.07 / -2.47` |
| `$1,000` | `B0_funding_flat` | `dev / hold / oos` | `1,571 / 118 / 13` | `1,187,518 / 83,004 / 9,748` | `886,664 / 65,771 / 8,748` | `496` (`24.92%`, `-63.37`) | `+273.69 / -8.19 / -3.35` | **`+273.69 / -8.19 / -3.35`** (`+2.048` all) | `-26.64 / -16.83 / -2.23` | **`0 / 3,045`** | `True (0)` | `25.89 / -14.36` |
| `$2,000` | `B0_original` | `dev / hold / oos` | `1,644 / 133 / 23` | `2,043,435 / 151,487 / 27,163` | `1,440,145 / 108,976 / 22,740` | `596` (`29.26%`, `-228.69`) | `+484.91 / +4.05 / -3.26` | **`+485.10 / +4.05 / -3.26`** (`+2.187` all) | `-80.64 / -26.16 / -5.82` | `2,033 / 3,045` | `False* (1,294.1) / True / True` | `16.39 / -14.53` |
| `$2,000` | `B0_funding_flat` | `dev / hold / oos` | `1,390 / 94 / 13` | `1,743,759 / 108,152 / 15,343` | `1,249,250 / 84,891 / 14,075` | `460` (`27.80%`, `-202.08`) | `+111.31 / -28.09 / -7.47` | **`+111.31 / -28.09 / -7.47`** (`+0.406` all) | `-264.31 / -37.81 / -6.64` | **`0 / 3,045`** | `True (0)` | `171.18 / -96.69` |

*\*Note:* `B0_original` `selection_dev` terminal residual (`$500` / `$1,000` / `$1,294.10`) occurs in `w17` due to a stale end-of-window book under strict `cutoff_ms = t_end_ms`; `holdout` and `w26_w27_oos` are `100%` flat (`$0.00` residual). Also note `w19` contributes `+$307.48` of the `$503.66` RWA `$1,000` `B0_original` total.

### 4.2 Crypto (`BTC, ETH, SOL`, `26` Windows) — `B0_original` vs. `B0_funding_flat` (`flat = $0.00`)

| Clip | Arm | Split | Fills | Maker Notl ($) | Hedged Notl ($) | Orphans (`Rate`, `Orp PnL $`) | Realized = MTM ($) (`bps`) | Crossed Hrs | Flat (`Res $`) | Max DD / Worst W ($) |
| --- | --- | --- | ---: | ---: | ---: | --- | ---: | --- | --- | --- |
| `$500` | `B0_original` | `dev / hold / oos` | `820 / 243 / 36` | `378,928 / 112,573 / 16,799` | `306,456 / 87,384 / 13,962` | `255` (`19.77%`, `-29.97`) | **`+56.78 / +12.20 / +5.45`** (`+1.464` all) | `755 / 759` | `True (0)` | `4.88 / -2.90` |
| `$500` | `B0_funding_flat` | `dev / hold / oos` | `684 / 201 / 31` | `315,346 / 92,799 / 14,049` | `269,775 / 75,138 / 11,718` | `170` (`15.53%`, `-26.40`) | **`-18.13 / -20.57 / +1.23`** (`-0.888` all) | `1* / 759` | `True (0)` | `38.71 / -10.55` |
| `$1,000` | `B0_original` | `dev / hold / oos` | `807 / 231 / 35` | `681,056 / 196,474 / 28,363` | `535,447 / 146,833 / 22,533` | `277` (`22.20%`, `-89.92`) | **`+72.86 / -2.43 / +7.19`** (`+0.857` all) | `754 / 759` | `True (0)` | `15.76 / -8.41` |
| `$1,000` | `B0_funding_flat` | `dev / hold / oos` | `684 / 191 / 28` | `574,570 / 162,401 / 23,025` | `464,868 / 127,108 / 18,833` | `209` (`19.63%`, `-67.17`) | **`-64.42 / -43.44 / +3.21`** (`-1.377` all) | **`0 / 759`** | `True (0)` | `107.87 / -22.02` |
| `$2,000` | `B0_original` | `dev / hold / oos` | `717 / 188 / 31` | `1,070,876 / 301,627 / 42,844` | `776,497 / 195,483 / 36,163` | `345` (`28.77%`, `-240.63`) | **`+11.19 / -38.21 / +6.32`** (`-0.146` all) | `748 / 759` | `True (0)` | `45.32 / -28.85` |
| `$2,000` | `B0_funding_flat` | `dev / hold / oos` | `587 / 129 / 23` | `882,551 / 206,992 / 33,739` | `676,477 / 153,921 / 28,575` | `225` (`23.53%`, `-154.84`) | **`-171.84 / -86.39 / +0.72`** (`-2.292` all) | **`0 / 759`** | `True (0)` | `258.22 / -54.50` |

*\*Note:* In Crypto `$500` `B0_funding_flat`, `1` slot in `selection_dev` (`1 / 759`) could not flatten before the `:00` guard due to a stale book, so `funding_avoidance_qualified = False` is honestly reported for that window (`holdout` and `w26_w27_oos` are `0` crossed hours).

### 4.3 Stationary Block Bootstrap 95% CIs (`L = 3.0`, `B = 2,000`, `seed = 20261127`, `all_w01_w27`)

| Domain | Clip | `B0_original` Total ($) `[95% CI]` (`p>0`) | `B0_funding_flat` Total ($) `[95% CI]` (`p>0`) | Paired `Delta(ff - orig)` Total ($) `[95% CI]` |
| --- | --- | --- | --- | --- |
| `rwa` (`t=21`) | `$500` | **`+377.90`** `[+132.33, +762.29]` (`1.000`) | **`+230.72`** `[+31.68, +550.26]` (`0.999`) | **`-147.17`** `[-220.23, -87.87]` (`dev -123.95 / hold -21.75 / oos -1.47`) |
| `rwa` (`t=21`) | `$1,000` | **`+503.66`** `[+131.38, +1,088.74]` (`1.000`) | **`+262.15`** `[-42.45, +768.68]` (`0.882`) | **`-241.51`** `[-341.85, -155.33]` (`dev -208.49 / hold -28.13 / oos -4.89`) |
| `rwa` (`t=21`) | `$2,000` | **`+485.89`** `[+29.72, +1,274.42]` (`0.996`) | **`+75.74`** `[-355.34, +717.55]` (`0.608`) | **`-410.15`** `[-640.44, -198.67]` (`dev -373.79 / hold -32.14 / oos -4.21`) |
| `crypto` (`t=26`) | `$500` | **`+74.43`** `[+15.56, +151.02]` (`0.998`) | **`-37.47`** `[-79.61, +3.98]` (`0.042`) | **`-111.91`** `[-180.85, -50.78]` (`dev -74.91 / hold -32.77 / oos -4.22`) |
| `crypto` (`t=26`) | `$1,000` | **`+77.62`** `[-5.65, +192.30]` (`0.958`) | **`-104.65`** `[-178.65, -34.54]` (`0.002`) | **`-182.27`** `[-307.10, -71.30]` (`dev -137.28 / hold -41.01 / oos -3.98`) |
| `crypto` (`t=26`) | `$2,000` | **`-20.70`** `[-107.79, +75.94]` (`0.321`) | **`-257.51`** `[-409.21, -118.04]` (`0.000`) | **`-236.80`** `[-403.60, -95.56]` (`dev -183.03 / hold -48.17 / oos -5.60`) |

---

## 5. Per-Venue Collateral, Margin Utilization, In-Flight Exposure & `USDC` Fee Attribution

All exposure and margin figures below are measured across concurrent 1-minute checkpoints and event-level `36 ms` hedge-flight peaks over `W01–W27` (`all_w01_w27`, zero linear scaling):

| Domain / Clip / Arm | Conc. Peak Notl (Lt / Arc $) | Conc. Net / Worst In-Flight ($) | Single-Sym Peak / 1-Leg ($) | Conc. Peak IMR (Lt / Arc $) | Conc. Peak MMR (Lt / Arc $) | Req. Equity (Lt / Arc / Total USDC) | `$10k` Budget IMR Util (Lt / Arc %) | `$10k` Min MM Mult (Lt / Arc) | `USDC` Fees (Lt / Arc / Tot $) |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `rwa $500 B0_orig` | `9,663 / 9,655` | `42.4 / 10,163` | `2,503 / 504.8` | `749.9 / 890.3` | `381.7 / 470.5` | `1,251 / 1,392 / 2,643` | `15.0% / 17.8%` | `13.1x / 10.6x` | `114.85 / 168.25 / 283.11` |
| `rwa $500 B0_ff` | `8,717 / 8,710` | `41.8 / 9,217` | `2,504 / 505.3` | `677.8 / 667.9` | `334.7 / 412.5` | `1,183 / 1,248 / 2,431` | `13.6% / 13.4%` | `14.9x / 12.1x` | `122.26 / 162.87 / 285.13` |
| `rwa $1,000 B0_orig` | `14,916 / 14,906` | `67.4 / 15,742` | `4,994 / 1,010.4` | `1,150.1 / 1,628.9` | `563.3 / 727.1` | `2,153 / 2,631 / 4,784` | `23.0% / 32.6%` | `8.88x / 6.88x` | `196.32 / 263.55 / 459.88` |
| `rwa $1,000 B0_ff` | `14,556 / 14,542` | `67.2 / 15,556` | `5,001 / 1,011.3` | `1,138.0 / 1,322.5` | `548.8 / 657.8` | `2,151 / 2,335 / 4,486` | `22.8% / 26.5%` | `9.11x / 7.60x` | `200.88 / 251.34 / 452.22` |
| `rwa $2,000 B0_orig` | `24,990 / 24,974` | `50.4 / 26,990` | `10,036 / 2,017.3` | `1,794.6 / 2,936.3` | `974.6 / 1,305.0` | `3,803 / 4,944 / 8,747` | `35.9% / 58.7%` | `5.13x / 3.83x` | `317.61 / 371.35 / 688.95` |
| `rwa $2,000 B0_ff` | `22,253 / 22,230` | `49.5 / 24,253` | `10,001 / 2,017.2` | `1,715.7 / 1,853.0` | `824.0 / 958.2` | `3,801 / 3,939 / 7,740` | `34.3% / 37.1%` | `6.07x / 5.22x` | `307.23 / 354.50 / 661.72` |
| `crypto $500 B0_orig` | `4,517 / 4,510` | `8.4 / 5,017` | `2,277 / 507.9` | `238.6 / 157.6` | `72.5 / 105.1` | `741 / 692 / 1,433` | `4.8% / 3.2%` | `69.0x / 47.6x` | `57.40 / 187.73 / 245.13` |
| `crypto $1,000 B0_orig` | `9,259 / 9,244` | `15.5 / 10,259` | `4,932 / 1,009.9` | `477.7 / 304.8` | `136.3 / 203.2` | `1,486 / 1,374 / 2,859` | `9.6% / 6.1%` | `36.7x / 24.6x` | `110.49 / 328.29 / 438.78` |
| `crypto $2,000 B0_orig` | `16,351 / 16,324` | `27.5 / 18,351` | `8,999 / 2,010.7` | `837.0 / 530.0` | `210.2 / 353.4` | `2,860 / 2,659 / 5,518` | `16.7% / 10.6%` | `23.8x / 14.2x` | `197.84 / 471.90 / 669.74` |

- **Fee Attribution:** `100%` of fees on both `lighter` and `arcus` are settled and reconciled in **`USDC`** (`fill_coverage_reconciled = True`). Notice in RWA `B0_original`, `arcus` off-hours RWA IMR (`0.075–0.30`) makes **Arcus IMR the binding collateral leg** (`$890 / $1,629 / $2,936` vs. Lighter `$750 / $1,150 / $1,795`).

---

## 6. Multiple-Testing & Overfitting Audit (`DSR` & `CSCV PBO`)

| Domain | Trial Scope | `Raw / Var` | `N_eff` | Best Trial | `SR` | `PSR(0)` | `DSR(N_eff)` | `DSR(Raw)` | `CSCV PBO` |
| --- | --- | ---: | ---: | --- | ---: | ---: | ---: | ---: | ---: |
| `rwa` (`t=21`) | `1k_grid` (`16 x 2`) | `32 / 32` | `1.2534` | `B0_original:b0_e8_v0.0_sh0` | `0.4110` | `1.0000` | **`1.0000`** | `0.0000` | **`0.1429`** (`16w`) |
| `rwa` (`t=21`) | `all_audit_trials` | `36 / 36` | `1.2351` | `clip_$500:B0_original:b0_e10_v0.0_sh0` | `0.4147` | `1.0000` | **`1.0000`** | `0.0000` | **`0.1571`** (`16w`) |
| `crypto` (`t=26`) | `1k_grid` (`16 x 2`) | `32 / 32` | `3.0166` | `B0_original:b0_e10_v1.0_sh0` | `0.2880` | `0.9923` | **`0.0000`** | `0.0000` | **`0.0000`** (`24w`) |
| `crypto` (`t=26`) | `all_audit_trials` | `36 / 36` | `2.9817` | `clip_$500:B0_original:b0_e10_v0.0_sh0` | `0.3752` | `0.9999` | **`0.0000`** | `0.0000` | **`0.0000`** (`24w`) |

*Note on `DSR` sensitivity:* In RWA, extreme positive skew (`+3.66` to `+3.82`, driven by `w19`) and high cross-trial correlation (`N_eff ≈ 1.24–1.25`) yield `DSR(N_eff) ≈ 1.0000` while `DSR(M_raw = 32..36)` is `0.0000` (`sr_threshold_raw ≈ 1.27–1.30`). In Crypto, `N_eff ≈ 3.0` (`sr_threshold_effective ≈ 0.77`) exceeds observed `SR` (`0.29–0.38`), yielding `DSR(N_eff) = 0.0000`.

---

## 7. Verification & Artifacts

- **Redacted JSON Output** (outside Git): `.agent-state/research/roadmap_upgrade_20261127/track02_b0_settleability_w01_w27.json`
  - `sha256`: `f4e9d7db2caa179f7bcb1f01a9f60f2ecd69a20155963d5ca3426f5cc28906cc`
  - `SECRET_HITS = 0`, `W28_HITS = 0`.
- **Checks Executed:**
  - `python3 -S -m unittest -v scripts/test_research_funding_basis_carry_portfolio.py scripts/test_research_track02_b0_settleability_audit.py` (`17/17` OK, including isolated `python3 -S` subprocesses blocking `numpy/duckdb/pyarrow/pandas/scipy`).
  - `python3 -S scripts/research_funding_basis_carry_portfolio.py --self-test` (`self_test: ok`).
  - `python3 -S scripts/research_track02_b0_settleability_audit.py --self-test` (`self_test: ok`).
  - `git diff --check` clean; zero untouched/frozen files edited; zero `.agent-state/` staged or committed.
