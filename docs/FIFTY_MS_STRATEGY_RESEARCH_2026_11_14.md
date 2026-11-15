# 50 ms Latency-Adapted Strategy Architecture & Continuous 10-Hour Validation (2026-11-14)

This document records the architecture changes, causal microstructure mechanisms,
selection matrix (`W_select`), frozen holdout policy, and untouched prospective
10-hour holdout evaluation (`W_holdout`) designed for AlephTX's physical
execution regime (**~50 ms network transport**, modeled conservatively in public
replay as `250 ms` one-way transport plus `300 ms` Lighter Standard cancel/taker
action delay = **`550 ms` effective cancel/taker action latency**, with explicit
`conservative_cancel_guard` and Premium fee/latency sensitivities).

Related documents:
[Independent strategy research](INDEPENDENT_STRATEGY_RESEARCH.md) ·
[Ten-hour research protocol](TEN_HOUR_RESEARCH.md) ·
[Basis-risk selection](archive/BASIS_RISK_SELECTION_2026_11_14.md) ·
[Research data-source registry](RESEARCH_DATA_SOURCES.md) ·
[Relative-value replay](PERP_RELATIVE_VALUE.md)

---

## 1. Governance & Non-Subsidy Contract

Per `AGENTS.md` and [INDEPENDENT_STRATEGY_RESEARCH.md](INDEPENDENT_STRATEGY_RESEARCH.md):

1. **Strict Scenario Separation**: **Scenario 1 (Quant Profit)**, **Scenario 2
   (Hedge Arbitrage)**, and **Scenario 3 (Incentive Turnover: `neutral`,
   `long_first`, `short_first`)** use separate configurations, execution states,
   capital/loss budgets (`$1,000 / $100` and `$500 / $50`), cash ledgers, and
   reports. Points, expected airdrops, and unconfirmed fee rebates carry `"0"`
   cash value. No run in one scenario subsidizes another.
2. **Continuous 10-Hour Validation Units**: Every validation unit spans an
   uninterrupted **`36,000,000 ms` (10-hour)** decision window with separate
   pre-window warm-up (`>= 60 s`) and real post-cutoff terminal drain (`15 s`
   or `60 s`), carrying positions and cash across shard and hourly funding
   boundaries (`10–11` hourly boundaries per run). Any run failing hourly flat
   settlement or `750 ms` public trade-delivery freshness is reported as
   `incomplete` (`net_pnl = None`), never zero-imputed.
3. **Selection vs. Frozen Untouched Holdout**:
   - **Selection (`W_select`)**: Reused sealed historical archives
     (`rh-turnover-cost-curve-20261114T043334Z/prepared` for Crypto Quant /
     PairHedge / Turnover, `2026-11-13 15:45:09.240–2026-11-14 01:45:09.240 UTC`;
     `extended-lighter-20261113T111337Z-cd4a1633` for Extended/Core;
     `2026-11-12-20h-full` + `public-funding-review-20261114T025117Z` for RWA).
   - **Frozen Holdout (`W_holdout`)**: Before inspecting the newly sealed
     prospective archive `core-rh-prospective-10h-20261114T044946Z`
     (`2026-11-14 05:00:00.000–15:00:00.000 UTC` / `13:00–23:00 Beijing`), all
     holdout candidates, thresholds, capital budgets, and evaluation rules were
     sealed in `~/aleph-tx-research/fifty-ms-strategy-20261114/frozen_holdout_policy.json`
     (SHA-256: `7a4a57fea7288b7646d9b454b49aa5fd3be27c198ff0d4491afac3e9b298fa4b`).

---

## 2. Why 1 ms Microstructure Logic Lost Money at 50 ms (`550 ms` Action Delay)

Empirical inspection of prior 10-hour baselines revealed four structural leaks
caused by running sub-millisecond HFT assumptions over a `50 ms` network +
`300 ms` venue cancel/taker queue:

1. **FIFO Queue Destruction & Cancel-Race Adverse Selection (Scenario 1)**:
   `Q1_core_control` (`1 s` requote interval, `2 bps` threshold, `5 s` TTL,
   symmetric placement/cancel bounds) issued **`20,501–20,991` cancel requests
   per 10 hours**, resetting resting quote age to **`609–980 ms`**. Benign orders
   never reached the front of the FIFO queue, while toxic cross-market sweeps
   selectively filled orders trapped in the `550 ms` cancel-pending window.
   Furthermore, when only one side breached `min_profit_bps`, whole-pair teardown
   needlessly destroyed the healthy opposite side's queue priority, and `84%–95%`
   of Maker fills waited until the `60 s` forced Taker stop-out.
2. **Static Fair-Value Gate vs. Mean-Reverting Cross-Domain Basis (Scenario 2)**:
   Robinhood Chain and Lighter Core maintain persistent structural basis offsets
   and `1–3 bps` touch spreads. Static `min_edge_bps = 1.0` produced `0` cycles
   on `W_select` and `1` losing IOC-unwound cycle (`16.01 bps` cost) on
   `W_holdout`, while `550 ms` Taker-Taker arbitrage could never beat `550 ms`
   adverse price drift on Extended/Core or Hyperliquid HIP-3 RWA.
3. **Aggressive Closing Timeouts & Unfiltered Toxic Hedge Sweeps (Scenario 3)**:
   `S3_control_v2` cancelled closing Maker quotes after `2,000 ms` (`entry_quote_lifetime_ms`)
   and forced emergency IOC exits after `8,000 ms` (`max_close_wait_ms`), turning
   almost every cycle into a double-spread Taker loss during volatile Core moves,
   while stranding unclosed positions across hourly funding boundaries when partial
   closing fills left stale stubs.
4. **Off-Book Zero-Price Bankruptcy Transfers in Lighter Public Trade Streams**:
   On `W_holdout` (`core-rh-prospective-10h-20261114T044946Z`), Lighter Core
   emitted a single off-book liquidation/bankruptcy transfer on ETH (`trade:0`,
   `trade_id = 40254475639`, `source_ms = 1794664911937`, `at_ms = 1794664911987`,
   `price = "0.00"`, `size = "0.0102"`, `is_maker_ask = false`) while `10.249 ETH`
   of resting L2 bids at `$1,865.79` remained untouched (`2.0 ETH` traded at
   `$1,865.79` `27 ms` later). Unfiltered replay treated `price == 0` as a fatal
   corrupt print at minute `541` (`Error: invalid recorded trade`), crashing all
   six prior frozen ETH prospective arms.

---

## 3. Code & Architecture Changes

All changes preserve live trading safety invariants (`QuoteHysteresisConfig`
defaults to `None` in live configs, preserving exact existing behavior unless
opted in; schema and Go/Rust ABI unchanged):

1. **Scenario 1 — Asymmetric Quote Hysteresis, Per-Side Bound Cancel & Age-Progressive Flattening**:
   - `src/config.rs`: Added `QuoteHysteresisConfig` (`cancel_min_profit_bps`,
     `flatten_min_profit_bps`, `flatten_age_start_fraction`,
     `per_side_bound_cancel`) with strict config validation
     (`cancel_min_profit_bps <= min_profit_bps`, `flatten_min_profit_bps <= min_profit_bps`,
     `0 <= flatten_age_start_fraction < 1`).
   - `src/strategy/inventory_neutral_mm/pricing.rs`, `components.rs`,
     `execution.rs`, `inventory_neutral_mm.rs`: Added `MakerPriceBounds::new_asymmetric`
     and `reconcile_side_plan_with_bounds` so resting quotes keep FIFO priority
     inside `[cancel_min_profit_bps, min_profit_bps]`, immediately cancel **only
     the breaching side** when fair value crosses `cancel_min_profit_bps`, and
     linearly relax the inventory-reducing side's half-spread toward
     `flatten_min_profit_bps` as inventory age approaches `max_inventory_age_secs`.
2. **Scenario 2 & Scenario 3 — Receipt-Causal `MakerBasisReversion`, `EntryFlowFilter` & Patient Multi-Level `HedgedExit`**:
   - `src/strategy/relative_value/paper/maker_first/basis_reversion.rs`:
     Receipt-causal, synchronized-book, duplicate-rejecting, gap-checked EWMA
     basis tracker (`warmup_ms`, `half_life_ms`, `max_gap_ms`,
     `min_dislocation_bps`, `cancel_dislocation_bps`, `reversion_fraction`) with
     full reset and diagnostic telemetry (`BasisReversionReport`).
   - `src/strategy/relative_value/paper/maker_first/entry_flow.rs`:
     Receipt-causal liquid hedge-leg trailing momentum and high-low range gate
     (`lookback_ms`, `sample_ms`, `max_gap_ms`, `max_adverse_move_bps`,
     `max_range_bps`, `cancel_resting_entries = false` placement-only mode so
     resting RH queue priority is not churned).
   - `src/strategy/relative_value/paper/maker_first.rs`: Extended `HedgedExit`
     with independent `closing_quote_lifetime_ms` (vs. short `entry_quote_lifetime_ms`),
     `patience_ms`, `target_cycle_edge_bps`, `closing_max_levels` (up to 6 ticks
     from best bid/ask), and immediate cancellation + resizing of stale closing
     stubs after confirmed partial Maker fills when `closing_quote_lifetime_ms`
     is configured.
3. **Provenance-Audited Lighter Zero-Price Off-Book Transfer Handling**:
   - `scripts/public_replay_sources.py` & `scripts/replay_ten_hour_scenarios.py`:
     Added opt-in `exclude_zero_price_transfers: bool = False` (default `False`
     keeps raw fail-closed behavior). When explicitly enabled for Lighter
     domains (`lighter`, `lighter_robinhood`), `price == 0, size > 0` off-book
     transfers are excluded from CLOB fill matching and recorded verbatim in
     `excluded_zero_price_transfers` provenance (`trade_id`, `at_ms`,
     `source_ms`, `size`, `side`), while negative prices, non-positive sizes, or
     non-Lighter zero prices still fail closed.

---

## 4. Scenario 1: Quant Profit — 10-Hour Selection & Untouched Holdout

### 4.1 Selection Matrix (`W_select`: `2026-11-13 15:45:09.240–2026-11-14 01:45:09.240 UTC`, 40 Runs)

Inputs: `rh-turnover-cost-curve-20261114T043334Z/prepared` (`BTC` SHA-256
`210d9793...`, `ETH` SHA-256 `8511c871...`; `60 s` warm-up, `36,000,000 ms`
decision window, `15 s` real post-cutoff drain; `10/10` funding slots and
terminal flat verified on all 40 runs).

| Arm | Policy Summary | BTC `$1000/$100` Raw / Net025 (`$`) | BTC `$500/$50` Raw / Net025 (`$`) | ETH `$1000/$100` Raw / Net025 (`$`) | ETH `$500/$50` Raw / Net025 (`$`) | Cancels (`10h`, BTC / ETH `$1k`) | Mean Queue Age (`ms`, BTC / ETH `$1k`) |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `Q0_ext_control` | Coinbase+HL fair value, `8 bps` entry, `1 s` / `2 bps` requote, `5 s` TTL | `-0.0809` / `-0.3129` | `-0.4229` / `-0.5526` | `-0.0274` / `-0.2735` | `-0.1802` / `-0.3340` | `12,079` / `12,579` | `2,094` / `1,712` |
| `Q1_core_control` | `core_only`, `8 bps` entry, `1 s` / `2 bps` requote, `5 s` TTL | `-0.0128` / `-0.0563` | `-0.1557` / `-0.1713` | `+0.4703` / `+0.4181` | `+0.1517` / `+0.1270` | `20,770` / `20,861` | `907` / `677` |
| `Q2_core_local_rev` | Prior Nov 14 local-reversion arm (`18 bps` floor) | `0.0000` / `0.0000` | `0.0000` / `0.0000` | `0.0000` / `0.0000` | `0.0000` / `0.0000` | `1,980` / `1,713` | `n/a` (`0` fills) |
| `Q3_hysteresis_only` | `core_only`, `12 bps` entry, `12 s` TTL, `3 bps` cancel floor | `-0.3704` / `-0.6727` | `+0.2060` / `-0.0172` | `-1.2070` / `-1.5150` | `-0.6896` / `-0.9612` | `7,043` / `7,156` | `5,962` / `5,416` |
| `Q4_hysteresis_flatten` | `Q3` + `0 bps` progressive flatten (`start = 0.25`) | `+0.2460` / `+0.0100` | `+0.7784` / `+0.6156` | `-0.6627` / `-0.9863` | `+0.1114` / `-0.1144` | `6,714` / `6,576` | `5,868` / `5,179` |
| `Q5_50ms_patient_flatten` | `core_only`, `12 bps` entry, `30 s` TTL, `3 bps` cancel, `0 bps` flatten | `-0.6056` / `-0.9511` | `-0.2068` / `-0.4314` | `+0.0502` / `-0.3527` | `+0.0093` / `-0.2505` | `2,506` / `2,608` | `15,647` / `13,247` |
| `Q5_50ms_patient_premium_fee` | `Q5` under `0.2 / 2.0 bps` Maker/Taker sensitivity (`$1k`) | `-0.8864` / `-1.1872` | — | `-1.2900` / `-1.6525` | — | `2,511` / `2,621` | `15,843` / `13,843` |
| **`Q6_50ms_wider_16bps`** | **`core_only`, `16 bps` entry, `30 s` TTL, `5 bps` cancel, `1 bps` flatten, per-side cancel** | **`+0.3216` / `+0.2080`** | **`+0.2684` / `+0.1797`** | **`+0.3475` / `+0.2011`** | **`+0.0586` / `-0.0466`** | **`2,546` / `2,638`** | **`16,660` / `13,828`** |
| `Q6_50ms_wider_premium_fee` | `Q6` under `0.2 / 2.0 bps` Maker/Taker sensitivity (`$1k`) | `+0.1585` / `+0.0712` | — | `-0.1501` / `-0.2527` | — | `2,561` / `2,686` | `17,537` / `13,674` |
| `Q7_50ms_wider_16bps_12sttl` | `Q6` with `12 s` TTL (`2 s` requote interval) | `+0.1947` / `+0.1239` | `-0.0188` / `-0.0899` | `+0.5998` / `+0.4640` | `+0.4108` / `+0.3061` | `5,791` / `5,685` | `8,602` / `6,750` |
| `Q8_50ms_15bps_15sttl` | `core_only`, `15 bps` entry, `15 s` TTL, `4 bps` cancel, `1 bps` flatten | `+0.1044` / `-0.0127` | `+0.6074` / `+0.4947` | `+0.1841` / `-0.0169` | `+0.1132` / `-0.0365` | `4,602` / `4,643` | `11,385` / `9,179` |

`Q6_50ms_wider_16bps` was the **only** candidate on `W_select` achieving `4/4`
positive raw PnL across BTC & ETH and both capital tiers (`3/4` positive after
`0.25 bps` overhead), cutting cancel churn by **`87%–94%`** (`1,226–2,638` vs.
`20,770–20,991`) and extending mean queue age `15x–50x` (`13.8–49.6 s`), and was
therefore frozen as the primary Scenario 1 holdout candidate.

### 4.2 Untouched Prospective 10-Hour Holdout (`W_holdout`: `2026-11-14 05:00:00.000–15:00:00.000 UTC`, 17 Runs)

Source: `core-rh-prospective-10h-20261114T044946Z` (`archive_manifest_sha256 =
b5baf44aa18085da7b9778c457d3ec843a27df20b2b422982b59540ecd1124fe`; `BTC` input
SHA-256 `6b9224fb...` with `0` excluded transfers; `ETH` raw input SHA-256
`a96182a3...`, `ETH` CLOB-filtered input SHA-256 `6b5b2b8e...` with `1` audited
zero-price bankruptcy transfer `trade_id = 40254475639`). Summary SHA-256:
`ecbfd26b2c4b6545e9026c582b3e3b0e693edbd75e287c09a3c51d28340b1d01`. All 16
CLOB-valid runs verified `11/11` hourly funding slots and ended `100%` flat.

| Symbol & Budget | Arm | Raw PnL (`$`) | Net @ `0.10 bps` (`$`) | Net @ `0.25 bps` (`$`) | Net @ `0.50 bps` (`$`) | Net (`bps`) | Maker / Taker Fills | Taker Share of Maker | Cancels (`10h`) | Mean Queue Age (`ms`) | Max DD (`$`) |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `BTC $1000/$100` | `Q0_ext_control` | `+0.7969` | `+0.6924` | `+0.5356` | `+0.2743` | `+0.762` | `118 / 49` | `0.5605` | `12,231` | `1,474.0` | `0.7995` |
| `BTC $1000/$100` | `Q1_core_control` | `+0.0452` | `+0.0206` | `-0.0164` | `-0.0781` | `+0.183` | `24 / 17` | `0.7358` | `20,501` | `768.2` | `0.4463` |
| `BTC $1000/$100` | `Q6_50ms_wider_16bps` | `-0.2810` | `-0.3376` | `-0.4225` | `-0.5639` | `-0.497` | `59 / 55` | `1.0002` | `2,498` | `14,616.1` | `0.6976` |
| `BTC $1000/$100` | `Q6_50ms_wider_premium_02_20` | `-0.1625` | `-0.2135` | `-0.2900` | `-0.4175` | `-0.319` | `53 / 49` | `1.0002` | `2,512` | `18,268.4` | `0.6321` |
| `BTC $1000/$100` | `Q6_50ms_wider_premium_04_28` | `-0.3129` | `-0.3554` | `-0.4192` | `-0.5256` | `-0.735` | `46 / 46` | `1.0002` | `2,515` | `18,718.5` | `0.6355` |
| **`BTC $500/$50`** | `Q0_ext_control` | `-0.2191` | `-0.2870` | `-0.3887` | `-0.5584` | `-0.323` | `138 / 49` | `0.5432` | `11,657` | `1,451.9` | `0.5224` |
| **`BTC $500/$50`** | `Q1_core_control` | `+0.1109` | `+0.0986` | `+0.0801` | `+0.0493` | `+0.901` | `26 / 15` | `0.7598` | `20,679` | `609.0` | `0.2408` |
| **`BTC $500/$50`** | **`Q6_50ms_wider_16bps`** | **`+0.2068`** | **`+0.1636`** | **`+0.0989`** | `-0.0090` | **`+0.479`** | **`66 / 61`** | `0.9551` | **`1,172`** | **`35,329.7`** | **`0.2425`** |
| `ETH $1000/$100` | `Q6_50ms_wider_16bps_RAW_UNFILTERED` | `FAIL` | `FAIL` | `FAIL` | `FAIL` | `FAIL` | — | — | — | — | `invalid recorded trade` (`trade_id=40254475639`) |
| `ETH $1000/$100` | `Q0_ext_control` | `-1.7150` | `-1.8864` | `-2.1434` | `-2.5718` | `-1.001` | `180 / 75` | `0.5296` | `11,743` | `1,777.7` | `1.8338` |
| `ETH $1000/$100` | `Q1_core_control` | `+0.0872` | `+0.0290` | `-0.0583` | `-0.2037` | `+0.150` | `51 / 21` | `0.3594` | `20,541` | `885.6` | `0.9550` |
| `ETH $1000/$100` | `Q6_50ms_wider_16bps` | `-0.7390` | `-0.8161` | `-0.9317` | `-1.1245` | `-0.959` | `96 / 65` | `0.8353` | `2,602` | `8,000.7` | `1.4174` |
| `ETH $1000/$100` | `Q6_50ms_wider_premium_02_20` | `-1.0719` | `-1.1415` | `-1.2461` | `-1.4203` | `-1.538` | `82 / 52` | `0.8083` | `2,643` | `8,072.5` | `1.3712` |
| `ETH $1000/$100` | `Q6_50ms_wider_premium_04_28` | `-1.0368` | `-1.1017` | `-1.1991` | `-1.3614` | `-1.597` | `85 / 48` | `0.8194` | `2,655` | `7,949.0` | `1.2502` |
| `ETH $500/$50` | `Q0_ext_control` | `-1.1103` | `-1.2164` | `-1.3755` | `-1.6407` | `-1.047` | `214 / 69` | `0.4266` | `11,484` | `1,454.4` | `1.1905` |
| `ETH $500/$50` | `Q1_core_control` | `-0.7641` | `-0.7910` | `-0.8313` | `-0.8985` | `-2.845` | `50 / 20` | `0.3198` | `20,789` | `811.4` | `1.0687` |
| `ETH $500/$50` | `Q6_50ms_wider_16bps` | `-0.4521` | `-0.5128` | `-0.6038` | `-0.7555` | `-0.745` | `103 / 65` | `0.8423` | `1,465` | `18,064.4` | `0.8665` |

**Scenario 1 Diagnosis & Takeaway**:
- **Microstructure invariants held out-of-sample**: Cancel churn fell **`87%–94%`**
  (`1,172–2,602` vs. `20,501–20,789`) and mean queue age increased **`9x–58x`**.
- **Out-of-sample profitability is not yet regime-invariant**: `Q6` stayed net
  profitable out-of-sample on **`BTC $500/$50` (`+$0.2068` raw, `+$0.0989` after
  `0.25 bps` overhead, beating both `Q0` and `Q1`)** and cut `ETH $500/$50`
  losses below both `Q0` and `Q1`, yet lost on `BTC $1000` (`-$0.2810`) and
  `ETH $1000` (`-$0.7390`).
- **Exact remaining bottleneck (`taker_share_of_maker = 0.835–1.000`)**: Because
  `order_ttl_secs = 30` and `requote_threshold_bps = 3.0` apply uniformly to both
  opening and reducing legs, once an opening quote is filled during a directional
  move (`W_holdout` ETH liquidation cascade), the passive reducing quote waits up
  to `30 s` to re-anchor, causing `84%–100%` of Maker fills to hit the `60 s`
  forced Taker stop-out. Decoupling a fast `3–5 s` / `1 bps` passive reducing-leg
  TTL while keeping `30 s` queue-preserving TTL on the opening leg, combined with
  a lagged cross-market toxicity pause, is the clear next engineering target.

---

## 5. Scenario 2: Hedge Arbitrage — Selection & Untouched Holdout Across Three Venue Pairs

### 5.1 Lighter Robinhood vs. Lighter Core (`PairHedge`, `W_select` 18 Runs & Untouched `W_holdout` 11 Runs)

`S2_reversion_2bps_flow` pairs receipt-causal `MakerBasisReversion`
(`warmup_ms = 60,000`, `half_life_ms = 120,000`, `min_dislocation_bps = 2.0`,
`cancel_dislocation_bps = 0.50`, `reversion_fraction = 0.85`) with Core
`EntryFlowFilter` (`lookback_ms = 2,000`, `max_adverse_move_bps = 3.0`,
`max_range_bps = 8.0`, placement-only) and patient 6-level Maker exit
(`closing_quote_lifetime_ms = 20,000`, `patience_ms = 45,000`).

| Window | Symbol & Budget | Arm | Incomplete | Closed Cycles (`Maker / IOC`) | Gross Turnover (`$`) | Raw PnL (`$`) | Net @ `0.25 bps` (`$`) | Raw Cost (`bps`) | Cost @ `0.25 bps` (`bps`) |
| --- | --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `W_select` | `BTC` & `ETH` (`$1k`, `$500`) | `S2_control_static` (`1.0 bps` static) | `None` | `0 (0 / 0)` | `0.00` | `0.00000` | `0.00000` | `n/a` | `n/a` |
| `W_select` | `BTC $1000/$100` | `S2_reversion_1_5bps_flow` | `None` | `1 (0 / 1)` | `398.52` | `-0.00333` | `-0.01330` | `0.335` | `1.334` |
| `W_select` | `ETH $500/$50` | `S2_reversion_1_5bps_flow` | `None` | `2 (0 / 2)` | `391.42` | `-0.00458` | `-0.01436` | `0.468` | `1.467` |
| `W_select` | `BTC $1000/$100` | `S2_reversion_2bps_flow_ccg` | `None` | `1 (0 / 1)` | `399.21` | **`+0.00184`** | `-0.00814` | **`-0.184`** | `0.817` |
| `W_select` | `ETH $1000/$100` | `S2_reversion_2bps_flow_ccg` | `None` | `1 (0 / 1)` | `399.31` | `-0.02131` | `-0.03129` | `2.134` | `3.133` |
| **`W_holdout`** | `ETH $1000/$100` | `S2_reversion_2bps_flow_RAW_UNFILTERED` | `FAIL` | — | — | — | — | `invalid maker-first print` (`trade_id=40254475639`) | — |
| **`W_holdout`** | `ETH $1000/$100` | `S2_control_static` | `None` | `1 (0 / 1)` | `81.61` | `-0.03058` | `-0.03262` | `15.010` | `16.011` |
| **`W_holdout`** | **`ETH $1000/$100`** | **`S2_reversion_2bps_flow`** | **`None`** | **`7 (3 / 4)`** | **`1,932.61`** | **`-0.00117`** | `-0.04949` | **`0.016`** | **`0.675`** |
| **`W_holdout`** | `ETH $1000/$100` | `S2_reversion_2bps_flow_ccg` | `None` | `8 (4 / 4)` | `2,509.65` | `-0.08286` | `-0.14561` | `0.884` | `1.553` |
| **`W_holdout`** | `ETH $500/$50` | `S2_control_static` | `None` | `1 (0 / 1)` | `81.61` | `-0.03058` | `-0.03262` | `15.010` | `16.011` |
| **`W_holdout`** | **`ETH $500/$50`** | **`S2_reversion_2bps_flow`** | **`None`** | **`7 (3 / 4)`** | **`1,246.48`** | **`+0.01094`** | `-0.02022` | **`-0.237`** | **`0.438`** |
| **`W_holdout`** | `BTC` (all 5 arms) | `S2_control_static` & `S2_reversion_2bps_flow` | `funding_trade_delivery_bound_violated` | `0–1` | `0–198.42` | `None` | `None` | `None` | `None` |

**RH/Core PairHedge Findings**:
- On untouched **`W_holdout` ETH**, `S2_reversion_2bps_flow` (`EntryFlowFilter`
  blocked `8,173–8,222` buy, `8,449–8,516` sell, and `4,708–4,776` high-range
  evaluations) expanded completed flat-verified cycles from `1` (`$81.61`, `0`
  paired Maker closes) to **`7` (`$1,246–$1,933` gross turnover, `3/7` paired
  Maker closes)**, cutting unit cost by **`95.8%`** (`0.675 bps` vs. `16.011 bps`
  on `$1000/$100`) and achieving **positive raw cash PnL (`+$0.01094`, `-0.237 bps`
  cost)** on `ETH $500/$50`.
- However, because RH/Core max 10-hour EWMA basis dislocation is structurally
  tight (`5.44–9.64 bps` BTC, `11.08–14.06 bps` ETH), RH/Core statistical basis
  reversion has tiny standalone dollar capacity (`<$0.02 / 10h`) and turns mildly
  negative after `0.25 bps` overhead (`-$0.020` to `-$0.049`). In addition, all
  BTC RH/Core holdout arms tripped `funding_trade_delivery_bound_violated`
  because Robinhood's BTC public trade feed exceeded the `750 ms` delivery bound
  near an hourly boundary.

### 5.2 Extended vs. Lighter Core (`330,948` Synchronized 10-Hour Frames)

Across `extended-lighter-20261113T111337Z-cd4a1633` (`330,948` frames, `$100`
capped depth-walked notional, `550 ms` action delay, `60 s` real post-cutoff IOC
drain), `basis_reversion` produced **`0` executable entry signals** on both BTC
and ETH:
- Under `0 / 2.5 bps` Extended/Lighter Taker fees: `best_net_edge_bps = -5.421 bps`
  (BTC) and `-6.144 bps` (ETH).
- Even under counterfactual **`0 / 0 bps` zero fees**: `best_net_edge_bps =
  -0.433 bps` (BTC) and `-1.152 bps` (ETH). Cross-venue Taker-Taker arbitrage on
  Extended/Core is structurally non-viable at `550 ms`.

### 5.3 Hyperliquid HIP-3 RWA (`io` Entropy vs. `xyz` TradeXYZ: 10-Hour Replay + 14-Day Settled Funding Screen)

1. **Continuous 10-Hour `550 ms` Delayed L2 Replay** (`2026-11-12-20h-full`,
   archive SHA-256 `80d94600ac6cf41e12ee843eec36240eea1091791dc09361ab5c27063e42ddb2`,
   `130,932–130,933` synchronized frames, `$100/leg`, public 5-level L2 without
   trade-print queue fills):
   - **`550 ms` Taker Basis Reversion (`4 / 6 / 8 bps` entry)**: Lost money across
     every symbol and threshold **even before fees** (`NVDA`: `-$2.74 / -$0.42 /
     -$0.12`; `SNDK`: `-$10.65 / -$3.13 / -$1.12`; `SKHX`: `-$3.81 / -$1.42 /
     -$0.51`), and `-$0.23` to `-$18.19` under HIP-3 Growth-Mode Taker (`0.45 bps`).
   - **10-Hour Forced-Unwind Carry (`Long xyz / Short io`, `10` funding slots)**:
     Paying 4 Taker spread + fee legs inside a single 10-hour window lost
     `-$0.100` (`NVDA`), `-$0.060` (`SNDK`), and `-$0.120` (`SKHX`) under Growth
     Mode (`-$0.223` to `-$0.282` under non-Growth `4.50 bps` Taker fees).
2. **14-Day (`336` Matched Hours, `33` Disjoint 10-Hour Sums) Settled Funding
   Screen** (`public-funding-review-20261114T025117Z`, Oct 31–Nov 14, historical
   screening only — direction `Long xyz / Short io` vs. Arcus):
   - **`NVDA`**: `+0.1147 bps/h` (`+38.55 bps` cumulative 14d single-leg, `19/33`
     positive 10h blocks, `10.05%` APR; Growth-Mode Taker fee breakeven **`15.7 h`**,
     Standard Taker fee breakeven **`156.9 h`**).
   - **`SNDK`**: `+0.0730 bps/h` (`+24.51 bps` cumulative 14d single-leg, `25/33`
     positive 10h blocks, `6.39%` APR; Growth-Mode Taker fee breakeven **`24.7 h`**,
     Standard Taker fee breakeven **`246.7 h`**).
   - **Implication**: RWA `io`/`xyz` cannot be traded as intraday `550 ms` Taker
     reversion or forced-10h carry; it only merits further qualification as a
     **multi-day (`>= 48 h–14 d`) Growth-Mode Maker-entry funding carry** strategy
     after contract identity, dividend/split rules, and witnessed settlement cash
     flows are audited.

---

## 6. Scenario 3: Incentive Turnover (Lighter Robinhood Maker / Core Hedge, `"0"` Points Cash Value)

`S3_50ms_flow_patient` combines `4,000 ms` RH opening quote lifetime, Core
placement-only `EntryFlowFilter` (`2,000 ms`, `3 bps` move / `8 bps` range),
`20,000 ms` closing quote lifetime with partial-fill stub resize, and `40,000 ms`
6-level patient Maker exit targeting `+0.50 bps` cycle edge before IOC fallback.

### 6.1 Selection Matrix Summary (`W_select`: 70 Runs Across `1 / 2 / 4 bps` Caps)

- At **`1 bps` cap**, all control and 50 ms arms had `0` fills (`$0` turnover).
- At **`2 bps` cap** (`100%` flat-verified on BTC `$1000`, BTC `$500`, and ETH
  `$1000`):
  - **ETH `$1000/$100` `neutral` & `short_first`**: `S3_control_v2` achieved **`$0`
    turnover (`0` cycles)**, whereas `S3_50ms_flow_patient` unlocked **`$199.95`
    eligible turnover at `0.654 bps` raw / `1.154 bps` after `0.25 bps` overhead**
    (`S3_50ms_queue_exit` reached `$399.69` at `1.060 bps` on `short_first`).
  - **BTC `$1000/$100` `neutral`**: `S3_50ms_flow_patient` cut raw loss by **`48.6%`**
    (`-$0.0191` vs. `-$0.0372`) and unit cost to **`0.709 bps` raw / `1.209 bps`
    after `0.25 bps` overhead** (vs. `1.292 bps` control).
  - **BTC `$1000/$100` `long_first`**: `S3_50ms_flow_patient` achieved positive raw
    PnL (`+$0.00073`, **`-0.103 bps` raw / `0.397 bps` after `0.25 bps` overhead**).
- At **`4 bps` cap**:
  - **`BTC long_first $1000/$100`**: `S3_50ms_flow_patient` scaled completed
    flat-verified RH turnover **`50x`** from `$199.84` (`4.138 bps` cost) to
    **`$10,014.72` (`60` cycles, `30` paired Maker closes) at `0.843 bps` raw /
    `1.335 bps` after `0.25 bps` overhead**!
  - Most other `4 bps` arms hit `resting_order_during_trade_gap` (3 historical RH
    disconnects in `W_select`) or sub-`$10` partial-fill dust stranding, so
    **`2 bps` was frozen as the primary safe holdout cap** (with `4 bps` retained
    only as an explicit stress test).

### 6.2 Untouched Prospective 10-Hour Holdout (`W_holdout`: 26 Runs, Summary SHA-256 `73440834...`)

| Symbol, Posture, Budget & Cap | Arm | Incomplete Reason | Cycles (`Maker / IOC`) | Eligible RH Maker Volume (`$`) | Gross 2-Leg Volume (`$`) | Raw PnL (`$`) | Net @ `0.25 bps` (`$`) | Raw Cost (`bps`) | Cost @ `0.25 bps` (`bps`) |
| --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `ETH neutral $1000/$100 1bps` | `S3_control_v2` / `S3_50ms_flow_patient` | `None` / `None` | `0` / `0` | `0.00` / `0.00` | `0.00` / `0.00` | `0.0000` / `0.0000` | `0.0000` / `0.0000` | `n/a` | `n/a` |
| **`ETH neutral $1000/$100 2bps`** | `S3_control_v2` | `funding_boundary_not_flat_and_resolved` | `1 (0 / 1)` | `199.78` | `781.82` | `None` | `None` | `None` | `None` |
| **`ETH neutral $1000/$100 2bps`** | **`S3_50ms_flow_patient`** | **`None` (11/11 verified)** | **`6 (2 / 4)`** | **`1,198.65`** | **`2,397.22`** | **`-0.1857`** | **`-0.2456`** | **`1.549`** | **`2.049`** |
| **`ETH neutral $1000/$100 2bps`** | **`S3_50ms_flow_patient_ccg`** | **`None` (11/11 verified)** | **`10 (4 / 6)`** | **`1,915.51`** | **`3,830.89`** | **`-0.3753`** | **`-0.4710`** | **`1.959`** | **`2.459`** |
| **`ETH short_first $1000/$100 2bps`** | `S3_control_v2` | `funding_boundary_not_flat_and_resolved` | `1 (0 / 1)` | `199.78` | `781.82` | `None` | `None` | `None` | `None` |
| **`ETH short_first $1000/$100 2bps`** | **`S3_50ms_flow_patient`** | **`None` (11/11 verified)** | **`5 (2 / 3)`** | **`999.57`** | **`1,999.18`** | **`+0.0132`** | **`-0.0368`** | **`-0.132`** | **`0.368`** |
| `ETH long_first $1000/$100 2bps` | `S3_control_v2` | `None` (11/11 verified) | `2 (0 / 2)` | `399.68` | `799.36` | `-0.0803` | `-0.1003` | `2.010` | `2.510` |
| **`ETH long_first $1000/$100 2bps`** | **`S3_50ms_flow_patient`** | **`None` (11/11 verified)** | **`2 (0 / 2)`** | **`399.68`** | **`799.35`** | **`-0.0757`** | **`-0.0957`** | **`1.893`** | **`2.393`** |
| `ETH neutral $500/$50 2bps` | `S3_control_v2` | `None` (11/11 verified) | `6 (4 / 2)` | `598.54` | `1,197.05` | `-0.0520` | `-0.0820` | `0.870` | `1.370` |
| **`ETH neutral $500/$50 2bps`** | **`S3_50ms_flow_patient`** | **`None` (11/11 verified)** | **`6 (3 / 3)`** | **`598.62`** | **`1,197.26`** | **`-0.0293`** | **`-0.0593`** | **`0.490`** | **`0.990`** |
| `ETH neutral $1000/$100 4bps` | `S3_control_v2` / `S3_50ms_flow_patient` | `funding_boundary_not_flat_and_resolved` | `1` / `0` | `183.62` / `0.00` | `388.81` / `0.19` | `None` | `None` | `None` | `None` |
| `BTC` (`1 / 2 bps`, all 11 arms) | `S3_control_v2` & `S3_50ms_flow_patient` | `funding_trade_delivery_bound_violated` | `0` | `0.00` | `0.00` | `None` | `None` | `None` | `None` |
| `BTC neutral $1000/$100 4bps` | `S3_control_v2` / `S3_50ms_flow_patient` | `funding_boundary_not_flat_and_resolved` | `1` / `6` | `198.65` / `1,038.61` | `400.68` / `2,078.63` | `None` | `None` | `None` | `None` |

**Scenario 3 Out-of-Sample Breakthroughs & Remaining Gate Limits**:
1. **Clean Out-of-Sample Dominance Across Every `2 bps` ETH Posture**:
   - On `ETH neutral $1000/$100 2bps` and `ETH short_first $1000/$100 2bps`,
     `S3_control_v2` failed hourly flat settlement (`funding_boundary_not_flat_and_resolved`),
     whereas **`S3_50ms_flow_patient` completed `100%` cleanly (`11/11` funding
     slots flat & verified)**, generating **`$1,198.65` eligible RH turnover at
     `1.549 bps` raw (`2.049 bps` after `0.25 bps` overhead)** on `neutral`
     (**`$1,915.51` at `2.459 bps` under `conservative_cancel_guard`**) and
     **`$999.57` eligible RH turnover with `+$0.0132` POSITIVE RAW CASH PROFIT
     (`-0.132 bps` raw cost, `0.368 bps` after `0.25 bps` overhead, i.e.,
     `~$36.8` all-in cost per `$1M` target RH Maker volume)** on `short_first`!
   - On `ETH neutral $500/$50 2bps`, `S3_50ms_flow_patient` cut raw cash loss by
     **`43.7%`** (`-$0.0293` vs. `-$0.0520`, **`0.490 bps` raw / `0.990 bps`
     after `0.25 bps` overhead**).
2. **Sub-`$10` Dust Stranding at `4 bps` Cap**: On `ETH neutral 4bps`, a `$0.19`
   opening print fell below the `$10` venue minimum order notional, stranding
   dust across the funding boundary. Adding opening min-notional dust aggregation
   (or reduce-only dust sweep where supported by Lighter) is required before
   raising `max_cycle_cost_bps` above `2.0 bps`.
3. **Robinhood BTC Public Trade Delivery Lag (`> 750 ms`)**: All BTC RH/Core
   holdout arms (both control and `50 ms`) tripped `funding_trade_delivery_bound_violated`,
   confirming that live RH deployment must monitor WebSocket trade-stream lag and
   pause opening quotes before hourly funding windows when stream lag spikes.

---

## 7. Verification & Reproducibility Summary

- **Local Toolchain Checks (`0` warnings / `0` failures)**:
  - `make fmt-check`: passed (`cargo fmt --check` + `gofmt -l .`).
  - `make check`: passed (`cargo clippy --locked --features research-replay --lib --bins -- -D warnings` + `go vet ./...`).
  - `NO_PROXY=127.0.0.1,localhost make test`: passed (**`572/572` Rust tests**,
    full Go `-race` suite, **`128/128` Python research archive/replay tests**).
  - `make test-python`: passed (**`37/37` `deploy/oasis` telemetry/monitor tests**).
  - `make build` & `make trade MODE=check CONFIG=config.example.toml`: passed.
- **Immutable Oasis Campaign Artifacts (`~/aleph-tx-research/fifty-ms-strategy-20261114/`)**:
  - `manifest.json`: `a5d72c5a18812e4d524adfbfb5e0c8576dba894deb9f95d8e4ef3107a5b8add9`
  - `frozen_holdout_policy.json`: `7a4a57fea7288b7646d9b454b49aa5fd3be27c198ff0d4491afac3e9b298fa4b`
  - `holdout_quant/summary.json`: `ecbfd26b2c4b6545e9026c582b3e3b0e693edbd75e287c09a3c51d28340b1d01`
  - `holdout_rh_core/summary.json`: `73440834f7901da80a519e685f49b47816b5087e5215149268ca993fd180b811`

---

## 8. Phase 2 (V2) Root-Cause Resolution & Engine Upgrades (`2026-11-15`)

Inspection of the Phase 1 (`fifty-ms-strategy-20261114`) untouched holdout
outcomes isolated three remaining `550 ms` execution bottlenecks and resolved
each directly in the Rust execution/replay engines before sealing
`frozen_v2_policy.json` (SHA-256:
`a4e0aa9e8c287501a2df3ee8a8def6091e7e19e73581f712c13248b0414eb9ec`):

1. **Decoupled Fast Inventory-Reducing Leg + Local Shock Gate (`Scenario 1` &
   `Scenario 3` Lighter CLOB)**:
   - *Bottleneck*: Uniform `order_ttl_secs = 30` and `requote_threshold_bps = 3.0`
     kept opening quotes at the front of the FIFO queue, but also froze passive
     inventory-reducing quotes for up to `30 s` during directional cascades
     (`taker_share_of_maker = 0.835–1.000` on `W_holdout`).
   - *Fix* (`src/config.rs`, `src/strategy/inventory_neutral_mm.rs`,
     `components.rs`, `execution.rs`): Added `reducing_order_ttl_secs` (`4 s` on
     Quant, `3 s` on Turnover), `reducing_requote_threshold_bps` (`1.0 bps`), and
     receipt-causal `max_local_momentum_bps` (`12.0 bps` shock gate pausing fresh
     inventory-expanding entries while keeping reducing quotes active), alongside
     opt-in `zero_deadband_flatten`, `breakeven_exit_bps`, and
     `conservative_dual_anchor`.
2. **Lot-Exact Reduce-Only Dust Clearance & Decoupled Hedge Slippage Buffers
   (`Scenario 2` & `Scenario 3` Cross-Venue)**:
   - *Bottleneck*: `MakerFirstReplay` applied opening `$10` `min_notional` checks
     to reduce-only IOC closes on Lighter/HIP-3 venues (stranding sub-`$10`
     partial-fill stubs across hourly funding boundaries) and forced
     `PairHedge` to pay `exit_basis_reserve_bps` (`5.0 bps`) as IOC slippage
     protection even on `1.5–2.5 bps` spreads.
   - *Fix* (`src/strategy/relative_value/paper/maker_first.rs`): Added
     `min_reduce_ioc_base` matching live venue gateways (`terms.base_step()` for
     `lighter`, `lighter_robinhood`, `trade_xyz`, `entropy`),
     `aggregate_sub_minimum_fills`, and explicit `hedge_protection_bps` /
     `closing_hedge_protection_bps` decoupled from `exit_basis_reserve_bps`.
3. **Sub-Second Lighter RTT-Probe Blip Immunity While Delta-Neutral (`Scenario 2`
   & `Scenario 3` Cross-Venue)**:
   - *Bottleneck*: Lighter public streams emit ~`150` sub-second (`< 1 s`)
     `timestamp_ms = 0, tradable = false` RTT-probe blips per 10 hours. In V1,
     `HedgedExit` treated every single-frame `usable == false` blip while fully
     hedged (`net == 0`) as a fatal `hedged_book_stops` emergency IOC unwind.
   - *Fix* (`src/strategy/relative_value/paper/maker_first.rs`): Added
     `last_usable_at_ms: [Option<u64>; 2]` and `max_hedged_book_stale_ms` on
     `HedgedExit` (`150,000 ms` grace while `net == 0`), while still **immediately
     cancelling active resting Maker quotes** on any `< 1 s` unusable book frame.
     This reduced `hedged_book_stops` (`b_stp`) to **`0` across `100%` of `W1`,
     `W2`, and `SNDK` runs**.

---

## 9. Scenario 1 (`Quant Profit`) — V2 Multi-Window Validation (`8/8` Raw Profitable, `7/8` Net025 Profitable)

`V9_Q_FastRed_Primary` (`core_only = true`, `BTC: 16 bps`, `ETH: 20 bps`,
`order_ttl_secs = 30`, `requote_threshold_bps = 3.0`,
`reducing_order_ttl_secs = 4`, `reducing_requote_threshold_bps = 1.0`,
`max_local_momentum_bps = 12.0`, `cancel_min_profit_bps = 5.0`,
`flatten_min_profit_bps = 1.0`, `flatten_age_start_fraction = 0.10`,
`per_side_bound_cancel = true`). All V2 parameter tuning (`selection_quant_v2..v8`,
`selection_final_v7`) ran strictly on `W1`; `W2` (`core-rh-prospective-10h-20261114T044946Z`)
was never used in V2 grid searches and was re-evaluated once under sealed
`frozen_v2_policy.json` (`w2_lighter_quant_and_s3/summary.json`, SHA-256:
`3777c2b4764e8647853c5664308564b737401ce94212032f05735d58fa70ffca`).

| Symbol & Budget | Arm | `W1` Raw / Net025 (`$`) | `W2` Raw / Net025 (`$`) | `W2` Net (`bps`) | `W2` Gross Turnover (`$`) | `W2` Fills (`M / T`) | `W2` Cancels (`10h`) | vs. V1 `Q6` on `W2` (`$`) |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| **`BTC $1000/$100`** | **`V9_Q_FastRed_Primary`** | **`+0.5672` / `+0.3954`** | **`+0.3755` / `+0.1649`** | **`+0.446`** | `8,426.70` | `81 / 64` | `1,170` | **`+0.6565` raw** (`-0.2810` -> `+0.3755`) |
| **`BTC $500/$50`** | **`V9_Q_FastRed_Primary`** | **`+0.2684` / `+0.1797`** | **`+0.2064` / `+0.0985`** | **`+0.478`** | `4,316.08` | `66 / 61` | `1,172` | Beats `Q0` (`-0.2191`) & `Q1` (`+0.1109`) |
| **`ETH $1000/$100`** | **`V9_Q_FastRed_Primary`** | **`+0.3758` / `+0.2662`** | **`+0.2285` / `+0.0236`** | **`+0.279`** | `8,192.38` | `69 / 50` | `1,344` | **`+0.9675` raw** (`-0.7390` -> `+0.2285`) |
| **`ETH $500/$50`** | **`V9_Q_FastRed_Primary`** | **`+0.3665` / `+0.3053`** | **`+0.0065` / `-0.0906`** | **`+0.017`** | `3,883.62` | `59 / 47` | `1,350` | **`+0.4586` raw** (`-0.4521` -> `+0.0065`) |
| `ETH $1000/$100` | `V9_Q_FastRed_Uniform16` (ablation) | `+0.2060` / `+0.0074` | `-1.2748` / `-1.5575` | `-1.127` | `11,311.32` | `96 / 73` | `1,473` | Shows `ETH` needs `20 bps` floor during liquidation cascades |
| `BTC $1000/$100` | `V9_Q_FastRed_Prem_02_20` / `04_28` | — | `-0.7742` / `-1.1176` raw | `-1.164` / `-1.834` | `6,653` / `6,095` | `68/58` / `60/52` | `1,189` / `1,196` | Proves `0 bps` Standard tier is essential at `550 ms` |
| `ETH $1000/$100` | `V9_Q_FastRed_Prem_02_20` / `04_28` | — | `-0.6606` / `-0.6496` raw | `-0.936` / `-1.007` | `7,055` / `6,449` | `56/41` / `56/34` | `1,355` / `1,383` | Proves `0 bps` Standard tier is essential at `550 ms` |

All 18 CLOB-valid `W2` Quant runs verified `11/11` hourly funding boundaries and
`100%` flat termination (`V9_Q_FastRed_Primary_RAW_UNFILTERED__ETH-1000c-100s`
preserved the expected fail-closed `invalid recorded trade` check on
`trade_id = 40254475639`).

---

## 10. Scenario 2 (`Hedge Arbitrage`) — V2 Multi-Window & Untouched RWA Holdout Validation

On `W2` (`w2_rh_core/summary.json`, SHA-256:
`549871849fb722506d72e7685dc1b229220c616a9407652c4d913a3f4f571680`), setting
`trade_delivery_grace_ms = 2,500` (inside a `1,500 ms` flat-at-funding guard),
`aggregate_sub_minimum_fills = true`, and `max_hedged_book_stale_ms = 150,000`
eliminated **`100%` of V1's `BTC` `funding_trade_delivery_bound_violated`
incompletes** and cut `hedged_book_stops` to **`0` across all 32 CLOB-valid runs**.

### 10.1 Mechanism 2A — Crypto Cross-Venue (`Lighter Core` vs. `Robinhood Chain`)

| Mechanism & Arm | Symbol & Budget | `W1` Raw / Net025 (`$`) | `W2` Cycles (`Passive / MakerClose / IOC`) | `W2` Gross (`$`) | `W2` Raw / Net025 (`$`) | `W2` `b_stp` / Incomplete |
| --- | --- | ---: | ---: | ---: | ---: | --- |
| **`S2_V2_TailHedge_primary` (`p6_4`)** | `BTC $1000/$100` | `0.0000` / `0.0000` | `2 (2 / 0 / 0)` | `399.11` | **`+0.2069` / `+0.1969`** | `0` / `None` |
| **`S2_V2_TailHedge_primary` (`p6_4`)** | `ETH $1000/$100` | **`+0.0174` / `+0.0155`** | `1 (0 / 1 / 0)` | `398.03` | `-0.1394` / `-0.1493` | `0` / `None` |
| **2-Market Portfolio (`BTC + ETH`)** | **`2x $1000/$100`** | **`+0.0174` / `+0.0155`** | `3` | `797.14` | **`+0.0675` / `+0.0476`** | **`0` / `None` (Both Windows Profitable)** |
| **2-Market Portfolio (`BTC + ETH`)** | **`2x $500/$50`** | **`+0.0174` / `+0.0155`** | `3` | `397.91` | **`+0.0299` / `+0.0199`** | **`0` / `None` (Both Windows Profitable)** |
| `S2_V2_TailHedge_p5_8` (diagnostic) | `BTC + ETH` (`2x $1k`) | `-0.2676` / `-0.2826` | `4 (3 / 1 / 0)` | `997.04` | **`+0.1347` / `+0.1097`** | `0` / `None` |
| **`S2_V2_Paired_CoreMaker` (`net == 0`)** | `BTC $1000` / `$500` | **`+0.0003`** / `-0.0158` raw | `1` / `1` | `398.70` / `198.27` | **`+0.0078` / `+0.0191` raw** (`+0.0142` net025 on `$500`) | `0` / `None` |
| **`S2_V2_Paired_RHMaker` (`net == 0`)** | `BTC $1000` / `$500` | `-0.0016` / `-0.0264` raw | `4 (0/2/2)` / `4 (0/2/2)` | `1,595.02` / `796.71` | **`+0.0031` / `+0.0210` raw** (`+0.0010` net025 on `$500`) | `0` / `None` |
| `S2_V2_Paired_RHMaker` (`net == 0`) | `ETH $1000` / `$500` | `-0.0088` / `-0.0331` raw | `4 (0/3/1)` / `5 (0/5/0)` | `1,064.63` / `998.15` | `-0.0363` / `-0.0145` raw | `0` / `None` |

### 10.2 Mechanism 2B — Untouched HIP-3 RWA `SNDK` 10-Hour MakerFirst Holdout (`holdout_rwa_sndk/summary.json`)

`SNDK.ndjson` (SHA-256: `98c55de7c6b211c6be08f3ccc28301ffc5d3b179bd506aabd9290c1d6c1284dd`,
`66,549` books, `40,226` `xyz:SNDK` trades, `24,377` `io:SNDK` trades, asymmetric
lot steps `0.001` vs. `0.0001`) was compiled and evaluated **only after**
`frozen_v2_policy.json` was sealed (summary SHA-256:
`317d3bf35e4b93319a62466449211c170f5fe60ed255ff00f767633bc5a21ff6`):

- **`8/8` runs finished `incomplete = None`, `verified_slots = [10, 10]`,
  `hedged_book_stops = 0`, `whole_ledger_cash_reconciled = true`**, proving zero
  dust stranding across `0.001` vs. `0.0001` lot asymmetry.
- **Intraday flat-at-hourly-funding PnL**: `S2_V2_RWA_xyzPaired_d3_2_diag`
  preserved `$0.0000` (`0` losing trades); `S2_V2_RWA_xyzTail_p5_5_diag` lost
  `-$0.00014` (`$22.42` gross); `S2_V2_RWA_xyzTail_p8_5_primary` lost `-$0.0950`
  (`$1k`) / `-$0.0471` (`$500`); aggressive `p2_4_diag` lost `-$3.6650` across
  `45` cycles (`$15,754` gross). This confirms out-of-sample that HIP-3's
  `2.10 bps` Growth-Mode round-trip fee (`0.15 bps` Maker + `0.90 bps` Taker x 2)
  cannot be overcome by sub-hour basis scalping, whereas **multi-day (`48 h–14 d`)
  Hold-Through Growth-Mode Funding Carry (`Long xyz / Short io`)** captures
  **`+38.55 bps` (`10.05%` APR, `15.7 h` fee breakeven) on `NVDA`** and
  **`+24.51 bps` (`6.39%` APR, `24.7 h` fee breakeven) on `SNDK`**.

---

## 11. Scenario 3 (`Incentive Turnover`) — V2 Mode A (`<= 0 bps`) & Mode B (`<= $100 / $50M`) Validation

All Scenario 3 runs carry **`$0.00` points valuation** and strictly isolated
capital/loss budgets (`$1,000` Mode A; `$50,000` Mode B with `25%` real L2 depth
participation cap).

### 11.1 Mode A — Zero / Negative Cost Medium Volume (`$1,000` Capital, `<= 0.00 bps` Target)

| Venue & Arm | Symbol | `W1` Gross 10h (`$`) | `W1` Raw / Net025 (`$`) | `W1` Cost (`$ / $1M`) | `W2` Gross 10h (`$`) | `W2` Raw / Net025 (`$`) | `W2` Cost (`$ / $1M`) |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| **`S3_ModeA_L1k_200s_FR_16bps`** | **`BTC`** | **`14,636.76`** | **`+1.5260` / `+1.1601`** | **`-104.26` (profit)** | **`17,755.56`** | **`+0.1421`** / `-0.3017` | **`-8.01` (`-0.080 bps` profit)** |
| **`S3_ModeA_L1k_200s_FR_16bps`** | **`ETH`** | **`18,603.67`** | **`+0.6086` / `+0.1435`** | **`-32.71` (profit)** | **`32,459.77`** | **`+0.2976`** / `-0.5139` | **`-9.17` (`-0.092 bps` profit)** |
| **Combined `FR_16bps` (`BTC + ETH`)** | **`2x $1k`** | **`33,240.43` (`~79.8k/d`)** | **`+2.1346` / `+1.3036`** | **`-64.22` (profit)** | **`50,215.33` (`~120.5k/d`)** | **`+0.4397`** / `-0.8157` | **`-8.76` (Both Windows `< 0 bps`)** |
| `S3_ModeA_L1k_200s_FR_12bps` | `BTC` / `ETH` | `33,737` / `48,035` | `-1.4059` / `-0.2050` raw | `+41.67` / `+4.27` | `42,205` / `73,571` | `-2.5916` / `-5.5943` raw | `+61.41` / `+76.04` |
| `S3_ModeA_L1k_200s_Std_8bps` | `BTC` / `ETH` | `92,790` / `103,515` | `-3.3110` / **`+0.6728`** raw | `+35.68` / **`-6.50`** | `89,618` / `119,955` | `-5.7343` / `-7.9323` raw | `+63.99` / `+66.13` |
| **`S3_ModeA_RH_FlowPatient` (`short_first`)** | **RH `ETH`** | `399.90` (`199.95` RH) | `-0.0131` / `-0.0231` | `+32.70` | **`1,598.46` (`799.24` RH)** | **`+0.0055`** / `-0.0345` | **`-3.43` (`-0.034 bps` profit)** |

### 11.2 Mode B — Ultra-Low-Cost / Negative-Cost High Volume (`$50,000` Capital, Target `<= $2.00 / $1M` = `<= $100 / $50M`)

| Venue & Arm | Symbol | `W1` Gross 10h (`$`) | `W1` Raw / Net025 (`$`) | `W1` Cost (`$ / $1M`) | `W2` Gross 10h (`$`) | `W2` Raw / Net025 (`$`) | `W2` Cost (`$ / $1M`) |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| **`S3_ModeB_L50k_5000s_FR_16bps`** | **`BTC`** | **`275,641.43`** | **`+9.0215` / `+2.1305`** | **`-32.73` (profit)** | **`326,105.00` (`~782.7k/d`)** | **`+11.4565` / `+3.3039`** | **`-35.13` (`+$1,757` profit / `$50M`!)** |
| **`S3_ModeB_L50k_5000s_FR_20bps`** | **`BTC`** | — | — | — | **`213,429.02` (`~512.2k/d`)** | **`+3.9643`** / `-1.3714` | **`-18.57` (`+$929` profit / `$50M`)** |
| `S3_ModeB_L50k_5000s_FR_12bps` | `BTC` | **`646,777.58`** | **`+8.2247`** / `-7.9447` | **`-12.72` (profit)** | `766,484.19` (`~1.84M/d`) | `-51.7420` / `-70.9041` | `+67.51` |
| `S3_ModeB_L50k_5000s_Std_20bps` | `BTC` | — | — | — | `180,528.11` | `-1.9789` / `-6.4921` | `+10.96` |
| `S3_ModeB_L50k_5000s_Std_20bps` | `ETH` | `181,662.83` | **`+22.9328` / `+18.3912`** | **`-126.24` (profit)** | `582,268.00` | `-29.9644` / `-44.5211` | `+51.46` (cascade size impact) |
| `S3_ModeB_L50k_5000s_FR_20bps` / `FR_16bps` | `ETH` | `193,668` / `372,810` | **`+17.8340`** / `-19.3651` raw | **`-92.09`** / `+51.94` | `660,866` / `950,969` | `-81.4912` / `-258.0451` raw | `+123.31` / `+271.35` |
| `S3_ModeB_Inst50k_CoreTailHedge_p2_6` | `BTC` / `ETH` | `138,303` / `202,684` | `-0.7812` / `-17.1692` raw | **`+5.65`** / `+84.71` | `77,708` / `197,536` | `-6.1618` / `-17.3861` raw | `+79.29` / `+88.02` |

**Key Mode B Operational Finding (`BTC` vs. `ETH` Order-Clip Allocation)**:
`S3_ModeB_L50k_5000s_FR_16bps__BTC` (`$5,000` clip) achieved **strictly negative
cost (net cash profit even after `0.25 bps` overhead!) across both `W1` and `W2`**,
generating `$275.6k–$326.1k` per 10h (`~$660k–$783k/day` on a single market) while
**earning `+$32.73` to `+$35.13` per `$1M` volume** (`+$1,636` to `+$1,757` net
cash profit per `$50M` volume vs. the `-$100 / $50M` budget). By contrast, `$5,000`
clips on thinner `ETH` books suffered heavy adverse selection during `W2`'s
liquidation cascade (`+$51.46–$271.35 / $1M` cost), whereas `$200` `ETH` clips
(`S3_ModeA_L1k_200s_FR_16bps__ETH`) earned `+$0.2976` profit (`-$9.17 / $1M`) on
the exact same `W2` tape. High-volume institutional turnover should therefore
route `$2,500–$5,000` clips to `BTC` and cap `ETH` clips at `$200–$1,000`.

---

## 12. Untouched `W3_holdout` (`extended-lighter-index-10h-20261113T174747Z`) Execution & Feed-Gap Audit

`extended-lighter-index-10h-20261113T174747Z` (`2026-11-13 17:57:40–2026-11-14
03:57:40 UTC`, status SHA-256:
`13add4cd211039660fb86d1e5197ee8e595dd2e0aefeebc6c4debebde264f3be`) was never
inspected prior to sealing `frozen_v2_policy.json` (summary SHA-256:
`804226fe823ad6fa0f1a0b1a4adc07c83e5c383f15bbeef61c656167a1c479b9`):

- **Execution & Lifecycle Verification (`32/32` Runs Clean)**: Every one of the
  `32` runs completed with `incomplete = None`, `halted = None`,
  `funding_boundaries_verified = 10` (`10/10`), and `terminal_flat_verified = true`
  (`final_position = 0.0`, `resting_orders = 0`), generating `$9,181.79` (`BTC`)
  and `$8,007.43` (`ETH`) on `V9_Q_FastRed_Primary ($1k)`, `$21,743.21` (`BTC`)
  and `$28,277.53` (`ETH`) on `S3_ModeA_FR_16bps`, and **`$423,009.25` (`BTC`
  `FR_16bps`) / `$886,413.50` (`BTC` `FR_12bps`)** on `S3_ModeB`.
- **Fail-Closed Cash-PnL Gate Preserved**: Because Lighter's public trade
  WebSocket recorded `2` reconnects across the 10-hour capture
  (`trade_feed_gaps = 2`), `recorded.rs` (`trade_feed_continuity_verified = false`)
  intentionally withheld `net_liquidation_pnl = None` across all 32 runs per unit
  test `reconnect_preserves_gap_evidence_and_guard_disconnect_cannot_be_certified`.
  `W3_holdout` is therefore reported strictly as a continuous 10-hour lifecycle,
  flat-settlement, and turnover-scale audit, without claiming certified cash PnL
  across public WebSocket reconnect gaps.

---

## 13. Phase 2 Reproducibility & Pre-Live Checklist

- **Toolchain Verification (`0` warnings / `0` errors)**:
  - `make fmt-check` & `make check` (`--features research-replay -D warnings`): passed.
  - `NO_PROXY=127.0.0.1,localhost make test`: passed (**`578/578` Rust tests**
    including 6 new V2 unit tests in `inventory_neutral_mm/tests.rs` and
    `maker_first.rs`, full Go `-race` suite, **`128/128` Python research tests**).
  - `make test-python` (**`37/37` `deploy/oasis` tests**), `make build`, and
    `make trade MODE=check CONFIG=config.example.toml`: passed.
- **Sealed Oasis V2 Artifacts (`~/aleph-tx-research/fifty-ms-strategy-20261115-v2/`)**:
  - `manifest.json`: `b59cc2415ab0809773bbe10038539922e67b0415b5ac23aabf440cd8be1dd708`
  - `frozen_v2_policy.json`: `a4e0aa9e8c287501a2df3ee8a8def6091e7e19e73581f712c13248b0414eb9ec`
  - `w2_lighter_quant_and_s3/summary.json`: `3777c2b4764e8647853c5664308564b737401ce94212032f05735d58fa70ffca`
  - `w2_rh_core/summary.json`: `549871849fb722506d72e7685dc1b229220c616a9407652c4d913a3f4f571680`
  - `holdout_w3_untouched/summary.json`: `804226fe823ad6fa0f1a0b1a4adc07c83e5c383f15bbeef61c656167a1c479b9`
  - `inventory-entry.json`: `2918d5c7a30bbae673417a85ff9620358b6a85e1bd9fe034a9aa53cad5ad80ff`
- **Mandatory Pre-Live Gateways Before Any Live Authorization**:
  1. Confirm Lighter Standard `0 bps` Maker/Taker fee status on the target account
     (Premium `0.2/2.0` or `0.4/2.8 bps` turns `550 ms` passive MM negative).
  2. Confirm USDG/USDC conversion cost and RH API point-eligibility rules before
     scaling cross-venue `RH/Core` capital.
  3. Complete contract-payoff, equity corporate-action/session, and witnessed
     hourly settlement checks before deploying multi-day `NVDA`/`SNDK` HIP-3
     Growth-Mode funding carry.

---

## 14. Phase 3 (`Step 1 & Step 2`) — Execution Sensitivity, Fee Dilution & Multi-Venue Audits (`2026-11-15`)

Phase 3 (`fifty-ms-strategy-20261115-step1-step2`) completed all remaining execution
realism sensitivities and multi-venue structural audits across `240` new replay runs
(`80` `W1/W2` 10-hour execution-sensitivity runs, `84` HIP-3 `0 ms` cancel-priority
10-hour `ALO` runs, `32` Binance `2x2` factorial diagnostic runs, plus `4` Lighter
fee-signature/markout books and `6` Arcus 10-hour RWA/Crypto books), alongside Go
Feeder 6-slot Binance reference plumbing (`feeder/exchanges/binance.go`,
`feeder/shm/matrix.go`), optional multi-profile process isolation (`src/config.rs`,
`profiles/*.example.toml`), and `MakerFirst` `post_only_rejected_{crossed,beyond_depth,new_level}`
+ `opening_cancel_requested` attribution (`src/strategy/relative_value/paper/maker_first.rs`).

### 14.1 Bidirectional Cancel-Overlap (`strict` vs. `conservative_cancel_guard`) & Official RH `200 ms` Delay (`W1` + `W2`, `80` Continuous 10h Runs)

Lighter's official documentation specifies `300 ms` Standard Taker/Cancel delay on
Core (`lighter`) and **`200 ms`** Standard delay on `lighter_robinhood`. Every frozen
Scenario 2 (`TailHedge_primary`, `Paired_CoreMaker`, `Paired_RHMaker`) and Scenario 3
(`S3_ModeA_RH_FlowPatient`) arm was re-evaluated across both `W1` (local summary
SHA-256: `26106e202e0f498669e649ed7bc4cc446d3cfb6b9ba2c48b57cb770c4b6d514d`) and
`W2` (Oasis summary SHA-256:
`24aa0a59bc78337a058fc7c0e00274234c4f6a0287d4725ade3b61af13327c65`):

| Arm & Market (`$1,000` unless noted) | Mode (`Delay / Cancel`) | `W1` Net @ `0.25 bps` (`$`) | `W1` Overlap Fills | `W2` Raw / Net025 (`$`) | `W2` Overlap Fills | `W2` `post_only_rejected` (`cross / beyond / new`) | `W2` Cost @ `0.25` (`bps` elig) |
| --- | --- | ---: | ---: | ---: | ---: | --- | ---: |
| **`S2_V2_TailHedge_primary` (`BTC`)** | `strict_300ms` | `0.0000` | `0` | **`+0.2069` / `+0.1969`** | **`0`** | `10 (3 / 7 / 0)` | **`-4.933` (profit)** |
| **`S2_V2_TailHedge_primary` (`BTC`)** | `guard_300ms` | `0.0000` | `0` | **`+0.2069` / `+0.1969`** | **`0`** | `10 (3 / 7 / 0)` | **`-4.933` (profit)** |
| **`S2_V2_TailHedge_primary` (`ETH`)** | `strict_300ms` | **`+0.0155`** | `0` | `-0.1394` / `-0.1493` | **`0`** | `25 (4 / 21 / 0)` | `7.500` |
| **`S2_V2_TailHedge_primary` (`ETH`)** | `guard_300ms` | **`+0.0208`** | `2` | `-0.1394` / `-0.1493` | **`0`** | `25 (4 / 21 / 0)` | `7.500` |
| `S2_V2_Paired_CoreMaker` (`BTC $500`) | `strict_300ms` / `guard_300ms` | `-0.0183` / `-0.1190` | `0` / `6` | `+0.0191` / `-0.0199` raw | `0` / **`5`** | `61` / `57` (`new=0`) | `-2.856` / `+2.750` |
| `S2_V2_Paired_RHMaker` (`BTC $500`) | `strict_rh200ms` / `guard_rh200ms` | `-0.0211` / `-0.0684` | `0` / `5` | **`+0.0326`** / `-0.0359` raw | `0` / **`5`** | `13` / `10` (`new=0`) | **`-0.706`** / `+1.283` |
| **`S3_ModeA_RH_FlowPatient` (`BTC neutral`)** | `strict_300ms` -> **`strict_rh200ms`** | `-0.0090` -> **`-0.0040`** | `0` -> `0` | `0.0000` -> `0.0000` | `0` -> `0` | `82 (8 / 14 / 60)` | `0.902` -> **`0.397` (`W1`)** |
| **`S3_ModeA_RH_FlowPatient` (`ETH neutral`)** | `strict_300ms` -> **`strict_rh200ms`** | `-0.0178` -> **`-0.0096`** | `0` -> `0` | `-0.3138` -> **`-0.0067`** raw | `0` -> `0` | `325 (25 / 28 / 272)` | `3.593` -> **`0.567` (`W2`)** |
| **`S3_ModeA_RH_FlowPatient` (`ETH neutral`)** | `guard_300ms` -> **`guard_rh200ms`** | `-0.1422` -> `-0.1623` | `7` -> `9` | `-0.3166` -> **`-0.1622`** raw | `5` -> `11` | `320 (33 / 28 / 259)` | `2.867` -> **`1.346` (`W2`)** |

**Key Step 1 Findings**:
1. **Asymmetric `TailHedge` Is Structurally Cancel-Overlap-Immune**: Because
   `S2_V2_TailHedge_primary` rests wide (`10–12 bps`), long-TTL (`60 s`) Core quotes
   with only `6–27` opening cancels per 10 hours, it suffered **`0` cancel-overlap
   prints and `0` overlap fills across all 4 `W2` cells**, preserving its positive
   2-market portfolio profit (`+$0.0476` net025 on `W2`, `+$0.0208` net025 on `W1`)
   even under adversarial `conservative_cancel_guard = true`. By contrast,
   high-churn `Paired_CoreMaker` and `Paired_RHMaker` (`48–177` cancels/10h) flip
   from positive under `strict` to negative under `guard`.
2. **Official RH `200 ms` Delay Unlocks Sub-`0.60 bps` Scenario 3 RH Execution**:
   Lowering RH's modeled Standard delay from `300 ms` to the documented `200 ms`
   cuts `S3_ModeA_RH_FlowPatient` `ETH` raw loss on `W2` **47-fold** (`-$0.3138` ->
   **`-$0.0067`** on `$999.35` RH Maker / `$1,998.69` gross turnover, **`0.567 bps`**
   all-in after `0.25 bps` overhead, and **`1.346 bps`** even under adversarial
   `guard_rh200ms`), and reduces `W1` costs to **`0.397 bps` (`BTC`)** and
   **`0.478 bps` (`ETH`)**.
3. **Exact `post_only_rejected` Attribution**: `83.7%` (`272 / 325` on `W2 ETH`) to
   `86.2%` (`232 / 269` on `W1 ETH`) of RH `post_only` rejections are
   `post_only_rejected_new_level` (conservative paper rule refusing to synthesize an
   unobserved price level inside a multi-tick spread), while genuine crossed-book
   races (`post_only_rejected_crossed`) are only `7.7%–11.4%`.

---

### 14.2 Lighter Public Fee-Signature & `1 s / 5 s` Taker-Tier Markout Diagnostic (`21.41 min`, `$89.15M` Public Notional)

Using `public-fee-observability-20261114T134000Z/public.ndjson.gz.partial` (`21.41`
minutes live post-subscribe capture stopped by Oasis disk-reserve guard; summary
SHA-256: `79469efd724d51cea62903b5072b7d0f9d2182a904a3fd462d4d5f5b6b3e5b08`;
**diagnostic only, not 10-hour qualification**), `scripts/analyze_lighter_fee_signatures.py`
classified `36,043` live unique trades (`$89.15M` notional) by public wire
`maker_fee` and `taker_fee` integers:

| Book (`21.41 min` Live) | Unique Trades / Notional | Maker Premium Share | Taker Standard `0 bps` Share (`p_std`) | Taker Premium / Plus Share | Standard Taker `1s / 5s` Mid Move (`bps`) | Premium Taker `1s / 5s` Mid Move (`bps`) | Max Raw Cost (`bps` of Total Maker TO) for `2.0 bps` Eligible Cap |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `lighter:BTC` | `18,412` / `$53.63M` | `93.32%` | **`89.61%`** | `7.42%` / `2.25%` | **`-0.19` / `+0.16`** | `+0.73` / `+0.11` (Plus `+2.36`) | **`1.792 bps`** |
| `lighter:ETH` | `9,401` / `$23.61M` | `97.61%` | **`80.44%`** | `18.19%` / `1.09%` | **`+0.59` / `+0.65`** | **`+1.72` / `+4.54`** (Plus `+4.71`) | **`1.609 bps`** |
| `lighter_robinhood:BTC` | `3,358` / `$6.49M` | `98.68%` | **`90.63%`** | `9.20%` / `0.08%` | **`+0.03` / `-0.14`** | `+0.16` / **`+1.26`** | **`1.813 bps`** |
| **`lighter_robinhood:ETH`** | `4,872` / `$5.42M` | `98.38%` | **`52.48%`** | **`47.14%`** / `0.13%` | **`+0.13` / `+0.45`** | **`+0.61` / `+0.59`** | **`1.050 bps`** |

**Strategic Implications**:
- **Why `550 ms` Tight-Spread MM Loses on Lighter**: `93.3%–98.7%` of Maker notional
  is supplied by **Premium `0 ms` Market Makers** (`0.28–0.40 bps` Core, `0.84–1.20 bps`
  RH), and **Premium/Plus API Takers** (`18.2%` of Core `ETH`, **`47.1%` of RH
  `ETH`**) generate `+1.72` to `+4.71 bps` adverse `1 s / 5 s` mid moves, whereas
  Standard `0 bps` retail Takers exhibit benign or mean-reverting price markouts
  (`-0.77 bps` `1 s` price markout on `lighter:BTC`, `-0.77 bps` on `RH:ETH`).
- **Scenario 3 Eligibility Dilution Check**: Even under `lighter_robinhood:ETH`'s
  `47.52%` non-Standard Taker dilution (`p_std = 52.48%`), `S3_ModeA_RH_FlowPatient`
  at official `rh200ms` (`0.067 bps` raw / `0.567 bps` `net025` per total RH Maker
  turnover on `W2`, `-0.004 bps` raw / `0.478 bps` `net025` on `W1`) translates to
  **`0.128 bps` raw / `1.080 bps` all-in per Standard-Taker-matched RH turnover**,
  comfortably inside the `2.0 bps` Scenario 3 ceiling (`BTC` has minimal `9.37%`
  dilution).

---

### 14.3 Binance Reference `2x2` Factorial Diagnostic Attribution (`6.45 h` Continuous Interior, `32` Runs)

Evaluated over the continuous `6.45 h` interior (`1794669600000`–`1794692820000`,
`6` verified hourly funding boundaries) of `binance-reference-ab-20261114T152000Z-v2`
(`26,531,201` records; Oasis summary SHA-256:
`39ca925c576edcb8b0b26c86443d7d43725b871f28263a3ad0d94c4a237ac371`). Because the
capture ended at `6.63 h` (disk-reserve floor) and included `1` Lighter public trade
reconnect (`trade_feed_gaps = 1`), `maker_replay` strictly sets
`net_liquidation_pnl = None`; results below report diagnostic
`observed_tape_unverified_cash_pnl` and `binance_participation` only:

- **Co-Located Binance Stream Quality**: `ETHUSDT` delivered `21,504,622 / 21,504,623`
  valid BBOs (`max_valid_gap_ms = 1,359 ms`, median spread `0.0541 bps`,
  **`99.96%–99.998%` fresh `<= 500 ms` availability** at Lighter decision instants);
  `NVDAUSDT` (`785,699` valid BBOs, `p50` spread `0.624 bps`) and `SNDKUSDT`
  (`2,452,224` valid BBOs, `p50` spread `0.117 bps`) were verified clean on the same
  shard.
- **`2x2` Attribution (`A_control` vs. `B_combined` vs. `C_stale_only` vs. `D_shock_only`)**:

| Family, Domain & Capital | `A_control` Unver. Cash (`$`) / Fills (`M/T`) | `C_stale_only` (`500ms`) Cash (`$`) / Withdrawn | `D_shock_only` (`1bps/1s`) Cash (`$`) / Withdrawn | `B_combined` Cash (`$`) / Fills (`M/T`) | Mechanism Diagnosis |
| --- | ---: | ---: | ---: | ---: | --- |
| `Frozen12bps` `lighter` `$1,000` | `-4.4019` (`170/68`) | `-3.7696` (`14/14`) | **`-3.1517`** (`7,690/7,442`) | `-3.5022` (`143/61`) | `D_shock` cuts tight-spread loss `28.4%` |
| `Frozen12bps` `lighter` `$500` | `-1.7468` (`182/70`) | `-1.6304` (`14/14`) | `-0.4190` (`8,125/8,032`) | **`-0.1434`** (`133/54`) | `B_combined` cuts loss `91.8%` (`DD $2.09 -> $1.13`) |
| `Frozen12bps` `lighter_rh` `$500` | **`+0.8872`** (`90/35`) | `+0.7387` (`7/7`) | `-0.3944` (`7,138/6,870`) | `-0.3944` (`69/30`) | `1 bps` shock over-filters benign RH spread capture |
| `V9_FastRed20bps` `lighter` `$1,000` | **`-2.8435`** (`199/126`) | `-2.8448` (`7/8`) | `-3.5270` (`20,365/21,153`) | `-3.5270` (`52/26`) | `1 bps` shock starves `74%` of `20 bps` Maker fills |
| `V9_FastRed20bps` `lighter` `$500` | **`-1.0706`** (`180/120`) | `-1.0706` (`7/7`) | `-1.3211` (`20,673/21,188`) | `-1.3211` (`49/24`) | Same over-filtering (`180 -> 49` Maker fills) |
| `V9_FastRed20bps` `lighter_rh` `$500` | **`-0.4286`** (`115/66`) | `-0.4366` (`13/13`) | `-2.0089` (`22,630/22,070`) | `-2.0729` (`63/18`) | `1 bps` shock starves mean-reverting RH fills |

  - **`C_stale_only` (`block_unavailable = true, max_age_ms = 500`)** is a near-zero-cost
    circuit breaker (`7–14` withdrawals in `6.45 h`) and safe to keep enabled.
  - **`D_shock_only` (`shock_bps = 1.0, lookback_ms = 1000`)** accounts for **`>99.9%`**
    of `B_combined`'s withdrawals (`~33%–37%` of all decisions). While helpful on
    tight `12 bps` Core quotes, a `1.0 bps` threshold over-filters `20 bps` wide-spread
    quotes (`V9_FastRed20bps`), starving `70%–75%` of fills without turning `6.45 h`
    single-venue Lighter MM positive — confirming that `shock_bps` must scale to the
    quote half-spread (`>= 6–10 bps`) rather than `1.0 bps` micro-noise.

---

### 14.4 Hyperliquid HIP-3 `0 ms` Cancel-Priority 10h `ALO` (`84` Runs) & Arcus `10.25 h` RWA Audit

1. **HIP-3 (`TradeXYZ` `xyz` & `Entropy` `io`) Single-Venue `ALO` Maker with
   `venue_cancel_delay_ms = 0` (`84` 10-hour runs across `NVDA` & `SNDK`, `50 ms`
   vs. `250 ms` network latency, Growth Mode `0.048/0.45 bps` & `0.15/0.90 bps` +
   Top-Tier `0.00/0.90 bps`, `6 / 10 / 16 / 24 bps` spreads; local summary SHA-256:
   `26106e202e0f498669e649ed7bc4cc446d3cfb6b9ba2c48b57cb770c4b6d514d`)**:
   - Because the public trade stream recorded `3` brief reconnects (`~1.15 s` each),
     `maker_replay` kept `net_liquidation_pnl = None` (`10/10` funding boundaries
     verified flat on every run).
   - On `observed_tape_unverified_cash_pnl`, **`81 / 84` runs were raw negative and
     `83 / 84` were negative after `0.25 bps` overhead** (the sole marginal positive
     was counterfactual Top-Tier `0.00/0.90 bps` `io:NVDA` `10 bps` at `50 ms`:
     `+$0.3764` raw / `+$0.0963` `net025`, while `SNDK` under the identical arm lost
     `-$4.7254`). Standalone intraday HIP-3 `ALO` market making is **NO-GO** due to
     positive Maker/Taker fees and discrete equity oracle jumps.
2. **Arcus `10.25 h` Microstructure, Cross-Venue Basis & Rulebook Annex C §8(c)
   Audit** (`public-venues-10h-20261113T183044Z` + `arcus-funding-supplement`; Oasis
   summary SHA-256: `fef515005a8d7400f1e1038623e5391433537b425ead5ea1d7ef808b246857e4`):
   - **Continuous `10.25 h` L2 Coverage**: `145,977–145,997` WS updates per market,
     `0` gaps `> 5 s` (`max_gap_ms = 2,101 ms`), source-to-Oasis lag `p50 = 97–104 ms`
     (`p90 = 145–152 ms`), plus `1,000` verified hourly funding records per symbol.
   - **RWA `0.00 bps` Maker Fee & Deep Top-of-Book Liquidity**: Unlike `BTC-USD`
     (`1.50 bps` Maker / `4.50 bps` Taker), all five RWA perps carry **`0.00 bps`
     Maker / `2.25 bps` Taker** (`50%` RWA discount) with tight median spreads and
     deep books: `SPY-USD` (`0.133 bps`, `$164.3k` top1 / `$338.9k` top5), `QQQ-USD`
     (`0.148 bps`, `$71.0k` top1 / `$212.2k` top5), `NVDA-USD` (`0.619 bps`, `$27.2k`
     top1 / `$89.5k` top5), `SNDK-USD` (`0.116 bps`, `$5.8k` top1 / `$19.1k` top5),
     and `MU-USD` (`0.131 bps`, `$3.3k` top1 / `$16.6k` top5).
   - **Persistent Cross-Venue Basis vs. HIP-3 `xyz`**: `Arcus NVDA-USD vs. xyz:NVDA`
     median basis **`-3.09 bps`** (`p10 = -6.16`, `p90 = -0.62`); `Arcus SNDK-USD vs.
     xyz:SNDK` median basis **`-7.34 bps`** (`p10 = -9.33`, `p90 = -5.07`).
   - **Qualification Gate (`execution_qualified = false`)**: Because `trade_stream`
     was absent in `public-venues-10h` (`trade_fills_simulated = false`) and Arcus
     Rulebook **Annex C §8(c)** permits dynamic latency speedbumps or Market Maker
     Fee Programme reclassification on high cancel-to-fill API flow, Arcus RWA cross-venue
     relative value is **promising but gated** on capturing a live `10 h` Arcus
     `trades` stream and enforcing low-churn (`TTL >= 30–60 s`) passive quoting.

---

### 14.5 Consolidated Final Verdict Across All Three Independent Scenarios (`50 ms` Network Tier)

| Independent Scenario | Qualified Primary Candidate & Venue | Certified `10 h` Out-of-Sample Evidence | Operational Gate & Verdict |
| --- | --- | --- | --- |
| **Scenario 1: Quant Profit** | `V9_Q_FastRed_Primary` (`BTC 16 bps`, `ETH 20 bps`, `30s` open / `4s` fast-reduce TTL, `12 bps` local shock gate + optional `500 ms` Binance staleness breaker) on **Lighter Core Standard (`0/0 bps`)** | `W1` (`4/4` net025 positive) + `W2` (`4/4` raw positive, `3/4` net025 positive: `BTC $1k +$0.376`, `ETH $1k +$0.229`); `6.45h` Binance capture negative (`-$0.43` to `-$3.53` unverified across `1` WS gap); HIP-3 `0ms` `ALO` `83/84` negative | **Selective `BTC`-led `0 bps` Standard Pilot Only** (`HOLD` on Premium fee accounts & standalone HIP-3 `ALO`; never enable `1.0 bps` micro-shock filter on `20 bps` quotes) |
| **Scenario 2: Hedge Arbitrage** | **(a)** `S2_V2_TailHedge_primary` (`p6_4`) on `Lighter Core` + `Robinhood`; **(b)** Multi-day (`>= 48h–14d`) HIP-3 Growth-Mode Maker-Entry Funding Carry (`Long xyz / Short io` `NVDA` & `SNDK`) | **(a)** `BTC+ETH` portfolio profitable on both `W1` (`+$0.0155`–`+$0.0208` net025) & `W2` (`+$0.0476` net025) with **`0` cancel-overlap fills on `W2`**; **(b)** `14d` settled funding `+10.05%` APR (`NVDA`, `15.7h` breakeven) & `+6.39%` APR (`SNDK`, `24.7h` breakeven) | **GO for Small-Capital `TailHedge` Pilot** (confirm USDG/USDC parity); **Gate Multi-Day RWA Carry** on corporate-action & witnessed settlement audit; capture Arcus `trades` WS next |
| **Scenario 3: Incentive Turnover (`"0"` Points Cash Value)** | **Mode A (`<= 0 bps`)**: `S3_ModeA_L1k_200s_FR_16bps` (`BTC` & `ETH`) + `S3_ModeA_RH_FlowPatient` (`rh200ms`); **Mode B (`<= $100 / $50M`)**: `S3_ModeB_L50k_5000s_FR_16bps__BTC` (`$5k` clip on `BTC`, `$200–$1k` clip on `ETH`) | **Mode A**: Negative cost (net cash profit!) on both `W1` (`-$64.22 / $1M`) & `W2` (`-$8.76 / $1M`, `$50.2k / 10h`); RH `ETH` at official `200ms` costs **`0.478 bps` (`W1`) / `0.567 bps` (`W2`)** (`1.080 bps` after `52.48%` Standard-Taker dilution). **Mode B `BTC`**: **`-$32.73` (`W1`) & `-$35.13 / $1M` (`W2`)** (`+$1,636` to `+$1,757` net cash profit per `$50M` volume at `~$783k/day`!) | **STRONGEST IMMEDIATE PRODUCTION FIT** — achieves **zero/negative cost** on Mode A and **net cash profit (`-$35 / $1M` vs. `+$2 / $1M` budget)** on Mode B `BTC` without assuming any point value |
