# Track 02 (`02_hedge_arbitrage`): `MakerHedgeQuoter` Upgrade & Full W01–W25 Multi-Venue Arb Evaluation

**Date:** 2026-11-27
**Status:** Strict HOLD (research & offline replay only; zero live orders/cancels/deploys)
**Owned Scope:**
- [src/strategy/basis_maker_hedge.rs](../src/strategy/basis_maker_hedge.rs)
- [scripts/research_track02_multivenue_arb.py](../scripts/research_track02_multivenue_arb.py)
- [scripts/test_research_track02_multivenue_arb.py](../scripts/test_research_track02_multivenue_arb.py)
- [docs/TRACK02_MULTIVENUE_ARB_UPGRADE_2026_11_27.md](TRACK02_MULTIVENUE_ARB_UPGRADE_2026_11_27.md)

---

## 1. Executive Summary

We upgraded [MakerHedgeQuoter](../src/strategy/basis_maker_hedge.rs) with four backward-compatible, default-off, zero-allocation risk/pricing controls and evaluated the full Aleph lakehouse history (`w01..w25`: **24 crypto campaigns ≈ 246h** and **19 RWA campaigns ≈ 195h**, ~58 pairs, >7,100 pair-hours) using [research_track02_multivenue_arb.py](../scripts/research_track02_multivenue_arb.py).

Across both `full_dev` (`w01..w20`, `ex w08` crypto) and untouched `holdout` (`w21..w25` crypto; `w21, w23..w25` RWA):
- **`USDC/USDC` WS distinct-domain (25 pairs):** `upgraded_track2` reduced `full_dev` quote-hedged loss by **81.1%** vs `baseline_unshielded` (`-$4,514.26` vs `-$23,899.48`) and **72.5%** vs `baseline_shielded` (`-$16,441.11`), cutting worst-pair max drawdown from `$5,046.67` to `$936.50`. On untouched `holdout`, loss fell from `-$6,183.14` (`unshielded`) / `-$4,373.07` (`shielded`) to **`-$1,353.50`** (`-$1,787.87` print-verified, max pair drawdown `$283.68`).
- **`USDG/USDC` WS distinct-domain (16 pairs, `0bp` FX):** `upgraded_track2` reduced `full_dev` loss from `-$4,904.78` (`unsh`) / `-$4,383.30` (`sh`) to **`-$1,173.78`** (`max DD $324.85`), and `holdout` loss from `-$1,092.41` / `-$997.81` to **`-$334.04`** (`-$379.95` at `1bp` FX, `-$425.85` at `2bp` FX).
- **`w03` Crash Regime Resilience:** On the `w03` crash window, `upgraded_track2` cut `USDC/USDC` WS losses by **81.0%** (`-$193.89` vs `-$1,018.55` unshielded) and `USDG/USDC` WS losses by **82.9%** (`-$40.65` vs `-$237.31` unshielded).
- **Structural Reality & HOLD Verdict:** Despite large relative risk/drawdown reductions, **cross-venue maker-first arb remains net negative on Crypto and most RWA pairs** once hedge taker fees, L1 depth slippage, and real `public_trades` print verification are enforced. Only low-frequency RWA pockets (`lighter->arcus QQQ` `+$2.46` / `+$0.72` print, `lighter->arcus SPY` `+$1.94` / `+$1.56` print, `rh->arcus QQQ` `+$2.40` / `+$2.24` print at `0bp` FX) were positive in `holdout`. Strict **HOLD** remains in force.

---

## 2. Rust Strategy Upgrade (`src/strategy/basis_maker_hedge.rs`)

All new fields on `MakerHedgeQuoterConfig` use `#[serde(default, deny_unknown_fields)]` and default to opt-off values so existing configs, [track_shadow.rs](../src/strategy/track_shadow.rs), and F1 stale-EMA behavior remain 100% preserved:

| Feature | Config Field(s) & Default | Causal Behavior & Safety Invariant |
| --- | --- | --- |
| **Causal Basis-Vol Edge Scaling** | `vol_edge_mult: 0.0`, `basis_vol_tau_ms: 300_000` | Tracks EWMA variance of `(b - prior_fair) * 1e4` **only when both maker & hedge books are fresh and clock advances (`g > 0`)**; resets cleanly on `max_gap_ms`. Computes `effective_min_edge_bps = e0_bps + vol_edge_mult * basis_sigma_bps`. |
| **Nonlinear Inventory Skew** | `nonlinear_skew_power: 1.0` (`>= 1.0`) | Scales signed inventory `q * (1 + (p - 1) * (|q|/q_max)^p)` to steepen opening edge & accelerate unwind near `q_max_clips`; bit-identical to linear `q` when `1.0`. |
| **Adverse Basis-Drift Guard** | `basis_drift_guard_bps: 0.0`, `basis_drift_tau_ms: 5_000` | Tracks short-horizon causal EMA `fast_fair`; suppresses opening bid (`q >= 0`) when `(b - prior_fast) * 1e4 <= -threshold` and opening ask (`q <= 0`) when `>= +threshold`. Never blocks inventory-reducing quotes. |
| **Hedge Opposite-BBO L1 Liquidity Guard** | `min_hedge_l1_notional_usd: 0.0` + `step_with_l1(...)` | Verifies `hedge.bid * hedge_bid_sz >= min` (for opening maker bid) and `hedge.ask * hedge_ask_sz >= min` (for opening maker ask); fails closed on opening legs if enabled without L1 sizes while keeping reducing legs active. |

Zero heap allocations, zero blocking I/O, and zero `crate::exchanges::*` imports on the quote path.

---

## 3. Evaluation Protocol & Aleph Artifact Provenance

- **Script:** [scripts/research_track02_multivenue_arb.py](../scripts/research_track02_multivenue_arb.py) (100% Python stdlib at top level; lazy `duckdb`/`numpy` inside lakehouse loader; DuckDB `:memory:` with `SET preserve_insertion_order=false; SET threads=2; SET memory_limit='2GB';`; cross-midnight `utc_date` + `utc_date + 1 day` Hive globbing).
- **Remote Execution Host & Command:** Read-only on `aleph` under `nice -n 10` using `/home/metaverse/aleph-tx-lakehouse/.venv/bin/python3`.
- **Artifact Path on `aleph`:** `/tmp/swarm_track2/track02_multivenue_arb_w01_w25.json`
- **Artifact SHA-256:** `af05c89146db0709ac4200633583eb9446bf4409a1913e79ad750eb43252da78`
- **Discovered Campaigns (W01–W25):**
  - **Crypto (24 windows):** `full_dev` = `w01..w07, w09..w20` (19 windows), `holdout` = `w21..w25` (5 windows).
  - **RWA (19 windows):** `full_dev` = `w01..w05, w07, w09, w13..w20` (15 windows), `holdout` = `w21, w23..w25` (4 windows).
- **Fee & Risk Caveats:** `lighter` and `lighter_robinhood` standard `0.0` bps fees are tagged `public_tier0_unverified`; `extended` (`2.50` bps taker) and `trade_xyz` (`0.30/0.90` bps) are tagged `account_evidence_required`; `arcus` uses audited `2.30` bps crypto / `1.15` bps RWA taker. `trade_xyz` (`polled_15s_diagnostic`) and `lighter_robinhood<->lighter` (`same_risk_domain=True`) are strictly separated from deployable WS distinct-domain totals.
- **Continuity Reporting:** All intersected WS windows span ~10.24–10.25 hours (`>= 36,000,000 ms` 10h threshold, `max_gap_ms <= 60,000 ms`). Because `WARMUP_MS + TEN_HOURS_MS` (`36,900,000 ms` = 10.25h) exceeded intersected campaign span by ~1–30 seconds on most pairs, `continuous_10h_windows` strictly recorded `0` with `short_span_10.25h` flagged in `incomplete_windows`. Zero `unhedged_stale_halts` occurred in any WS distinct-domain cohort.

---

## 4. Cohort Results: `full_dev` (W01–W20) vs Untouched `holdout` (W21–W25)

### 4.1 Same-Settlement `USDC/USDC` (WS Distinct-Domain: 25 Pairs)

Covers `lighter<->arcus`, `lighter<->extended`, `arcus<->extended` across Crypto (`BTC, ETH, SOL`) and `lighter<->arcus` across RWA (`NVDA, QQQ, SPY, AAPL, TSLA`).

| Mode | Split | Pair-Hours | Fills | Quote PnL ($) | Print-Verified PnL ($) | Funding Carry ($) | Max Pair DD ($) | `w03` Crash ($) |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `baseline_unshielded` | `full_dev` | 4,456.73 | 49,456 | -23,899.48 | -30,525.75 | -1.85 | 5,046.67 | -1,018.55 |
| `baseline_shielded` | `full_dev` | 4,456.73 | 37,112 | -16,441.11 | -21,204.98 | -1.36 | 3,478.59 | -788.99 |
| **`upgraded_track2`** | **`full_dev`** | **4,456.73** | **12,313** | **-4,514.26** | **-6,776.88** | **-0.92** | **936.50** | **-193.89** |
| `baseline_unshielded` | `holdout` | 1,178.75 | 11,504 | -6,183.14 | -7,435.01 | -0.08 | 1,464.23 | — |
| `baseline_shielded` | `holdout` | 1,178.75 | 8,521 | -4,373.07 | -5,220.97 | -0.07 | 1,032.72 | — |
| **`upgraded_track2`** | **`holdout`** | **1,178.75** | **2,868** | **-1,353.50** | **-1,787.87** | **+0.04** | **283.68** | — |

**Regime Breakdown (`USDC/USDC` WS Distinct-Domain, `upgraded_track2`):**
- `full_dev`: `calm` (`160` pw, `1,888` fills) `-$913.00` quote / `-$1,199.26` print; `normal` (`238` pw, `8,687` fills) `-$3,122.65` quote / `-$4,671.87` print; `extreme_crash` (`37` pw, `1,738` fills) `-$478.57` quote / `-$905.72` print.
- `holdout`: `calm` (`20` pw, `411` fills) `-$204.55` quote / `-$261.66` print; `normal` (`87` pw, `2,412` fills) `-$1,145.16` quote / `-$1,520.29` print; `extreme_crash` (`8` pw, `45` fills) `-$3.78` quote / `-$5.92` print.

### 4.2 Cross-Stablecoin `USDG/USDC` (WS Distinct-Domain: 16 Pairs, FX Sensitivity)

Covers `lighter_robinhood<->arcus` across Crypto (`BTC, ETH, SOL`) and RWA (`NVDA, QQQ, SPY, AAPL, TSLA`), strictly excluding `same_risk_domain=True` (`rh<->lighter`) and polled `trade_xyz`.

| Mode | Split | Pair-Hours | Fills | PnL @ 0bp FX ($) | PnL @ 1bp FX ($) | PnL @ 2bp FX ($) | Print PnL @ 0bp ($) | Max Pair DD ($) |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `baseline_unshielded` | `full_dev` | 2,704.70 | 17,907 | -4,904.78 | -5,627.67 | -6,350.59 | -7,175.79 | 1,108.00 |
| `baseline_shielded` | `full_dev` | 2,704.70 | 14,794 | -4,383.30 | -5,004.57 | -5,625.85 | -6,143.06 | 1,108.00 |
| **`upgraded_track2`** | **`full_dev`** | **2,704.70** | **4,694** | **-1,173.78** | **-1,387.41** | **-1,601.02** | **-1,905.47** | **324.85** |
| `baseline_unshielded` | `holdout` | 717.50 | 3,684 | -1,092.41 | -1,238.68 | -1,384.96 | -1,461.50 | 182.82 |
| `baseline_shielded` | `holdout` | 717.50 | 3,111 | -997.81 | -1,125.57 | -1,253.33 | -1,326.07 | 166.27 |
| **`upgraded_track2`** | **`holdout`** | **717.50** | **1,025** | **-334.04** | **-379.95** | **-425.85** | **-479.63** | **58.14** |

- **`USDG/USDC` including `same_risk_domain` (`rh<->lighter`, 27 pairs total):** `upgraded_track2` `full_dev` = `-$414.62` (`0bp`) / `-$1,022.53` (`1bp`) / `-$1,630.44` (`2bp`), `holdout` = `-$252.28` (`0bp`) / `-$387.11` (`1bp`) / `-$521.94` (`2bp`). These totals include non-deployable single-counterparty `rh<->lighter` and polled `trade_xyz` pairs and are kept separate from Section 4.2.

---

## 5. Selected Per-Pair & Print-vs-Quote Verification Highlights (`upgraded_track2`)

| Pair | Cohort | Flags | `full_dev` Quote / Print ($) | `holdout` Quote / Print ($) | `holdout` Fills | `holdout` Slip (bps) | `holdout` Funding ($) |
| --- | --- | --- | ---: | ---: | ---: | ---: | ---: |
| `lighter->arcus BTC` | `USDC/USDC` | WS distinct | -611.69 / -766.25 | -189.85 / -238.48 | 429 | 10.14 | +0.18 |
| `extended->arcus BTC` | `USDC/USDC` | WS distinct | -28.89 / -154.51 | -7.98 / -35.94 | 78 | 3.53 | 0.00 |
| `lighter->extended ETH` | `USDC/USDC` | WS distinct | -551.15 / -586.23 | -152.21 / -178.05 | 245 | 11.73 | +0.07 |
| `extended->arcus SOL` | `USDC/USDC` | WS distinct | +7.77 / -20.99 | -0.53 / -4.25 | 105 | 2.24 | 0.00 |
| `lighter->arcus NVDA` | `USDC/USDC` | WS distinct | +43.17 / -337.66 | -29.85 / -91.06 | 224 | 8.79 | -0.03 |
| `lighter->arcus QQQ` | `USDC/USDC` | WS distinct | -2.74 / -9.78 | **+2.46 / +0.72** | 17 | 4.32 | 0.00 |
| `lighter->arcus SPY` | `USDC/USDC` | WS distinct | +0.37 / -8.59 | **+1.94 / +1.56** | 9 | 0.59 | +0.01 |
| `arcus->lighter TSLA` | `USDC/USDC` | WS distinct | -12.67 / -27.15 | +0.31 / +0.58 | 3 | 6.35 | -0.01 |
| `rh->arcus QQQ` (`0/1/2bp`) | `USDG/USDC` | WS distinct | +1.31 / -10.90 | **+2.40 (+1.56/+0.71) / +2.24** | 12 | 2.12 | -0.12 |
| `rh->arcus NVDA` (`0/1/2bp`) | `USDG/USDC` | WS distinct | +37.67 / -150.17 | -20.85 (-28.53/-36.21) / -49.57 | 236 | 7.44 | 0.00 |
| `lighter->trade_xyz NVDA` | `USDC/USDC` | **`polled_15s_diagnostic`** | +828.20 / **-215.34** | +181.25 / **-100.95** | 2,330 | 1.16 | +0.02 |
| `rh->trade_xyz NVDA` | `USDG/USDC` | **`polled_15s_diagnostic`** | +835.19 / **-128.28** | +143.28 / **-58.54** | 2,369 | 1.14 | +0.01 |

**Key Diagnostic Findings:**
1. **Print Verification Exposes Polled-15s Illusion:** `trade_xyz` uses 15s polled BBO snapshots (`polled_15s_diagnostic`), creating illusory `+$828.20` (`full_dev`) / `+$181.25` (`holdout`) quote-hedged PnL on `lighter->trade_xyz NVDA` that flips to **`-$215.34` / `-$100.95`** under real `public_trades` print verification.
2. **Funding Carry Synergy Is Second-Order:** `funding_carry_usd` remained within `[-$2.87, +$1.03]` across all pairs (`+$0.04` to `+$0.07` total in `holdout`) because balanced hedged inventory turnover keeps net dollar-hour exposure small, and `arcus`/`extended` lack `funding_and_ctx` rate records (`funding_available_legs <= 1` on those pairs).
3. **L1 Depth & Adverse Hedge Slippage Dominate:** Even with `min_hedge_l1_notional_usd = $1,000` blocking >2.4M thin ticks in `USDC/USDC` and >1.1M in `USDG/USDC`, residual hedge latency moves + L1 sweeps still impose `2–17 bps` expected hedge slippage on crypto and `NVDA`/`TSLA`, keeping Track 02 on strict **HOLD** outside tiny low-frequency RWA pockets (`QQQ`, `SPY`).

---

## 6. Verification Checklist

- `cargo fmt --check`: PASS
- `cargo clippy --locked --features research-replay --lib --bins -- -D warnings`: PASS
- `cargo test --locked --lib basis_maker_hedge`: PASS (14/14 tests)
- `python3 -m unittest -v scripts/test_research_track02_multivenue_arb.py`: PASS (6/6 tests)
- `python3 scripts/research_track02_multivenue_arb.py --self-test`: PASS
- Isolated stdlib-only check blocking `numpy,duckdb,pyarrow,pandas,scipy`: PASS
- Relative markdown link check & `git diff --check`: `BAD_LINKS=[]`, clean
