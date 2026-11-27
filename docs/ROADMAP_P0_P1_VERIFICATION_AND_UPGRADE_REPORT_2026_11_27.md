# AlephTX Roadmap P0/P1 Verification & Upgrade Synthesis Report (`W01–W27`)

**Date:** `2026-11-27` (CST) · **Code Baseline:** `main` (`83b8352`) · **Operational Mode:** Strictly **`HOLD`** across all tracks (zero live orders, zero cancellations, zero Tokyo deploys or restarts; `-w28` / `>= w28` and crypto `w08` strictly excluded)

This report synthesizes the four independently verified Roadmap P0/P1 workstreams executed against `ALEPHTX_RESEARCH_AND_EXECUTION_ROADMAP_2026_11_27.md`:
- **Infra P0 Parity & Review Bundle:** [INFRA_P0_SAME_EVENT_PARITY_AND_REVIEW_BUNDLE_2026_11_27.md](INFRA_P0_SAME_EVENT_PARITY_AND_REVIEW_BUNDLE_2026_11_27.md) · `.agent-state/research/roadmap_upgrade_20261127/p0_parity_review_bundle.json` (`sha256: c92615ff6defa108c9ba90004a5dce3208299e12ffcf40585429de38938c5345`)
- **Track 03 (`03_incentive_turnover`):** [TRACK03_CORE4_SINGLE_FACTOR_HOLD_AND_PREREG_2026_11_27.md](TRACK03_CORE4_SINGLE_FACTOR_HOLD_AND_PREREG_2026_11_27.md) · `.agent-state/research/roadmap_upgrade_20261127/track03_core4_hold_ablation_w01_w27.json` (`sha256: 0c69090dabb9f035f3b0dfdb5c8d18b3997b7f5c53c639726f228b129e770a0a`) · `.agent-state/research/roadmap_upgrade_20261127/track03_w28plus_prereg_plan.json` (`plan_sha256: ce851712e70d401a090595f601b5261f71fa7a5fc1ecd743a5ebef37c4aa98d1`)
- **Track 02 (`02_hedge_arbitrage`):** [TRACK02_B0_SETTLEABILITY_MARGIN_AND_P0_BOUNDARIES_2026_11_27.md](TRACK02_B0_SETTLEABILITY_MARGIN_AND_P0_BOUNDARIES_2026_11_27.md) · `.agent-state/research/roadmap_upgrade_20261127/track02_b0_settleability_w01_w27.json` (`sha256: f4e9d7db2caa179f7bcb1f01a9f60f2ecd69a20155963d5ca3426f5cc28906cc`)
- **Track 01 (`01_quant_profit`):** [TRACK01_EXIT_DEPTH_CONSERVATION_AND_SINGLE_GATE_2026_11_27.md](TRACK01_EXIT_DEPTH_CONSERVATION_AND_SINGLE_GATE_2026_11_27.md) · `.agent-state/research/roadmap_upgrade_20261127/track01_depth_and_single_gate_w01_w27.json` (`sha256: 0e7802b2e2df6e2d29ceb8d8e88d8b250ddede0a0e34e00af457f0113939ddb8`)

---

## 1. Executive Decision & Roadmap §5 Sign-Off Matrix

### 1.1 Operational Invariants & Unified Fee Provenance

1. **Strict `HOLD` & Data Boundary Invariants:** All three strategy tracks remain operationally **`HOLD`**. No live trading, order cancellation, or Tokyo deployment is authorized. Every `W01–W27` evaluation excludes `w08` and enforces strict `-w28` (`w28+`) exclusion (`w28_excluded = true`).
2. **Unified Verified Fee Provenance (`.agent-state/research/account_fee_evidence_20261127.json`, `sha256: 4d3a9c1aeae5358bb9c55ab448db392ac3edb0a89244f74c23d89817c3359d92`):**
   - `lighter_robinhood` (`USDG`): `verified_account_tier`, **`0.0 / 0.0 bps`** maker/taker (`rebate_bps = 0.0` baseline).
   - `lighter` (`USDC`): `verified_account_tier` (`Premium`), **`0.40 / 2.80 bps`** maker/taker (`0.0 / 0.0 bps` Standard tested only as explicitly labeled counterfactual).
   - `arcus` (`USDC`): `verified_account_tier` (`level_0` with 5% discount), **RWA `0.00 / 2.1375 bps`**, **Crypto `1.425 / 4.275 bps`** maker/taker.
   - `trade_xyz` (`USDC`): `verified_account_tier` (`growthMode` discount schedule).
   - *Provenance audit note:* Track 01, Track 03, and Infra P0 harness verify `4d3a9c1a...` directly (`track01` and `p0_parity_review_bundle` record `fee_evidence_sha256 = 4d3a9c1a...`; Track 02 loads the exact verified tiers from `account_fee_evidence_20261127.json`, while `track02_b0_settleability_w01_w27.json` emitted `fee_provenance.source_sha256: null` due to a key-name lookup difference, verified non-blocking by `verifier-6`).
3. **Sole Candidate Advancing to Frozen Prospective Gate (`HOLD` for Live):** Track 03 **`S4_hold600s_only`** (`max_inventory_hold_ms = 600000`, `reduce_only_penny_ticks = 0`, `inventory_skew_bps_per_full_limit = 2.0`) on `lighter_robinhood` `Core 4` (`AAPL, AMZN, META, MSFT`) is frozen under `plan_sha256: ce851712e70d401a090595f601b5261f71fa7a5fc1ecd743a5ebef37c4aa98d1` and [track03_core4_patient_shadow.toml](../examples/track03_core4_patient_shadow.toml) (`sha256: fb2ad2a281926e566b80e8b07bc77c546db42a18ecc4a8c90b00ca25db68e252`) for unseen `W28+` shadow/replay validation.

### 1.2 `verifier-6` Roadmap §5 Three-Part Sign-Off Matrix

| Workstream (`main` Commit · Audited Tip) | `evidence_qualified` | `hypothesis_supported` | `advance_to_next_review` | Key Verification & Blocking Gate |
| --- | :---: | :---: | :---: | --- |
| **Infra P0 Parity** (`6f24370` / `ecf376f` · `4cc1dcb`) | **PASS** | `PENDING_P1_UNSEEN_WINDOW` | **YES (`HOLD_FOR_P1_CORE4_TRIAL`)** | Rust `34/34` + `8/8`, Python `4/4` + `7/7` `--self-test`, `W27` slice `33c0d12d...`, `4d3a9c1a...` fee digest, doc-vs-JSON reconciled |
| **Track 03 (`P1`)** (`d67d155` · `c7e86485`) | **PASS** | **YES on `W01–W27` (`PENDING_W28_PLUS_PROSPECTIVE`)** | **YES — ADVANCE TO FROZEN `W28+` SHADOW GATE (`HOLD` FOR LIVE)** | `756` rows, `max_parity_err = 0.0`, `5/5` tests, `--self-test`, Rust `--check` OK; frozen `plan_sha256: ce851712...` |
| **Track 02 (`P0+P1`)** (`83b8352` · `56dc1c3e`) | **PASS** | **`HOLD / PENDING_ARCUS_FUNDING_WITNESS`** | **NO (`HOLD`)** | `17/17` tests (`12/12` + `5/5`), both `--self-test`, `crossed_funding_hours == 0 / 3,045` (`B0_funding_flat`); blocked by `0` `Arcus` lakehouse `funding_and_ctx` files |
| **Track 01 (`P0+P1`)** (`b2f707f` / `4f22a70` · `171ff90`) | **PASS** | **NO (`NO-GO / HOLD`)** | **NO (`NO-GO / HOLD`)** | `11/11` tests, both `--self-test`, P0 exit depth fixed; `w26_w27_oos` remains `-$34.68`, `oos_w21_w27` `delta = -$9.40` (95% CI `[-$24.53, +$3.53]`) |

---

## 2. P0 Same-Event Execution Parity & Ledger Boundary Closure

### 2.1 Infra P0 Parity (`src/strategy/track_shadow.rs`, `src/bin/track01_track02_shadow.rs`, `scripts/p0_same_event_parity_harness.py`)

Three P0 execution defects in [track_shadow.rs](../src/strategy/track_shadow.rs) and [track01_track02_shadow.rs](../src/bin/track01_track02_shadow.rs) were closed and cross-checked three ways against `IndependentReferenceSimulator` and Python [research_track03_full_universe_speedbump_mm.py](../scripts/research_track03_full_universe_speedbump_mm.py) via [p0_same_event_parity_harness.py](../scripts/p0_same_event_parity_harness.py) (`frozen_plan_sha256: 21c5f7c2ce49119ba74457ef1cb157e9e57f4173f6add5f6a49dc482f448c6bb`):

1. **Defect A — Pre-Install Pending IOC Processing & Stale `330 ms` Re-Queue:** `on_target_l2_bbo` calls `process_pending_ioc_exits` against `prev_touch` and `prev_book_fresh` *before* installing a newly arrived fresh touch, and processes expired pending IOCs on stale/crossed `on_target_l2_bbo` and `on_trade_print` ticks by incrementing `stale_exit_blocks` and re-queueing at `exec_at_ms = now_ms + ioc_exit_delay_ms` (`330 ms`) rather than filling at `0 ms` upon recovery.
2. **Defect B — Single-Use L1 Visible Depth Budget per Touch:** `Track03ArmRuntime` initializes `rem_bid_sz` and `rem_ask_sz` to `l1_sz * max_l1_depth_fraction` only when a fresh `on_target_l2_bbo` installs a touch and decrements the corresponding side on every `execute_ioc_exit` fill, preventing two IOC exits on the same snapshot from double-spending L1 liquidity.
3. **Defect C — `stale_exit_blocks` Telemetry & Default-Off `declared_end_ms` / `terminal_drain_ms`:** Added `stale_exit_blocks: u64` to `Track03ArmReport` and `Track03ShadowReport`, plus `#[serde(default)]` `declared_end_ms: Option<u64>` and `terminal_drain_ms: Option<u64>` (defaulting to `None`). Events strictly past `declared_end_ms` are ignored across `on_reference_touch`, `on_target_bbo`, `on_target_l2_bbo`, `on_trade_print`, and `on_bbo_cross_diagnostic`; `terminal_drain_ms` suppresses new openings while maintaining reducing quotes and scheduling delayed reduce-only IOC flatten.

#### Synthetic P0 Counterexamples (`7 / 7 PASS`, `<= 1e-4 USD`, `0.0 USD` Conservation & Decomposition Error)

| Scenario ID | Ref (M/T) | Py (M/T) | Rust (M/T) | `stale_exit_blocks` | `depth_clipped_exits` | Realized PnL (USD) | Unrealized PnL (USD) | Residual (USD) | Status |
| --- | :---: | :---: | :---: | :---: | :---: | ---: | ---: | ---: | :---: |
| `p0_c1_stale_gap_330ms_requeue` | `1 / 1` | `1 / 1` | `1 / 1` | `1 / 1 / 1` | `0 / 0 / 0` | `-0.0800` | `0.0000` | `0.0000` | **PASS** |
| `p0_c2_two_iocs_same_touch_depth_conservation` | `1 / 2` | `1 / 2` | `1 / 2` | `0 / 0 / 0` | `2 / 2 / 2` | `+0.0300` | `+0.0200` | `50.0200` | **PASS** |
| `p0_c3_partial_exit_true_residual` | `1 / 1` | `1 / 1` | `1 / 1` | `0 / 0 / 0` | `1 / 1 / 1` | `+0.0400` | `+0.1500` | `150.1500` | **PASS** |
| `p0_c4_declared_end_ms_late_ref_immunity` | `1 / 0` | `1 / 0` | `1 / 0` | `0 / 0 / 0` | `0 / 0 / 0` | `0.0000` | `+0.1200` | `200.1200` | **PASS** |
| `p0_c5_terminal_drain_ms_flatten` | `1 / 1` | `1 / 1` | `1 / 1` | `0 / 0 / 0` | `0 / 0 / 0` | `+0.0400` | `0.0000` | `0.0000` | **PASS** |
| `p0_c6_same_ms_book_first` | `2 / 0` | `2 / 0` | `2 / 0` | `0 / 0 / 0` | `0 / 0 / 0` | `+0.0800` | `+0.0000` | `0.0800` | **PASS** |
| `p0_c6_same_ms_trade_first` | `1 / 0` | `n/a` | `1 / 0` | `0 / - / 0` | `0 / - / 0` | `0.0000` | `0.0000` | `200.0000` | **PASS** |

- `p0_c6_same_ms_trade_first` is evaluated on Reference and Rust (`unrealized_pnl_usd = 2.84e-14`, `terminal_residual_notional_usd = 199.999968`) because `prepare_replay_stream` canonically sorts same-ms events `book` before `trade`; dual-ordering bounds are `maker_fills in [1, 2]` and `realized_pnl_usd in [0.0, 0.079968]`.
- Bundled Track 01/02 reference checks confirm single-use L1 exit depth (`1.2` pos, `1.5 * 0.5 = 0.75` touch budget $\to$ `0.75 + 0.0`, `0.45` residual) and causal orphan-leg settlement (`200 ms` orphan window, `+$0.0269885` net after `2.30 bps` hedge fee).

#### Sealed Public `W27` `AAPL` & `META` Slice Four-Stage Denominator Scorecard & First Divergences

Source: `.agent-state/research/roadmap_upgrade_20261127/w27_aapl_meta_public_slice.json` (`campaign_id = tokyo-rwa-20261126T161454Z-w27`, `1795737600000..1795739400000` ms UTC, `sha256: 33c0d12d0218a10434ac26a73760cd9d3b606e85b4effb7b926e6aec6f06cd3b`):

| Stage / Metric (`matched_execution_baseline`, `o200_c0_e330`) | `AAPL` (`symbol_id=1006`) | `META` (`symbol_id=1009`) |
| --- | --- | --- |
| **Stage 1: Input Coverage** | `11,798` events (`4,572` BBO, `5,700` Ref, `1,526` Trades); `book_coverage_ratio = 1.0`; `0` drops | `11,338` events (`5,446` BBO, `5,700` Ref, `191` Trades inside `declared_end_ms`); `book_coverage_ratio = 1.0`; `0` drops |
| **Stage 2: On-Time Availability** | `on_time_ratio = 1.0`; `stale_exit_blocks = 0`; wall Ref `32.06` / Py `50.07` / Rust `80.68 ms`; per-event `p50=0.0035 / p95=0.0048 / p99=0.0073 ms`; queue `p50=276.0 / p95=865.4 / p99=1207.08 ms`; IOC `p50=500.0 / p95=1450.95 / p99=4111.87 ms` | `on_time_ratio = 1.0`; `stale_exit_blocks = 0`; wall Ref `29.76` / Py `52.74` / Rust `81.89 ms`; per-event `p50=0.0034 / p95=0.0048 / p99=0.0063 ms`; queue `p50=289.0 / p95=712.7 / p99=1150.86 ms`; IOC `p50=497.0 / p95=998.5 / p99=1420.55 ms` |
| **Stage 3: Executable & Settleable (Rust)** | `397` orders, `16` ALO rej, `220` maker fills, `58` taker exits, `19` depth-clipped, `$0.00` residual, `USDG` | `1,176` orders, `8` ALO rej, `47` maker fills, `30` taker exits, `6` depth-clipped, `$0.00` residual, `USDG` |
| **Stage 4: Complete Economic Result (Rust)** | Realized `+$0.048662`, Fees `$0.00`, Closed Ntl `$16,512.07`, `-$1.47/M`, `s = 0.8055`, `max_dd = $0.1934`, err `0.0` | Realized `+$0.023856`, Fees `$0.00`, Closed Ntl `$5,477.29`, `-$2.18/M`, `s = 0.8885`, `max_dd = $0.2611`, err `0.0` |

- **Exact Remaining Strategy-Layer First Divergences (non-P0):**
  - On matched `o200_c0_e330` with `arcus` references present, the first divergence is `maker_fills` (`AAPL`: Ref `362` / Py `240` / Rust `220`, `delta = 142`; `META`: Ref `54` / Py `48` / Rust `47`, `delta = 7`), driven by: (1) Python triggering `decide()` on `ref` updates and keeping passive reducing quotes while an IOC exit is pending, (2) Rust evaluating on target BBO updates and cancelling resting quotes on `inventory_hold_timeout`, and (3) Reference omitting `arcus` lead-lag cancel pulls.
  - On production `S4` diagnostics (`AAPL`: Py `o200_c300_e330` `170` maker / `19` taker / `-$0.056172` vs Rust `o200_c0_e330` `108` maker / `41` taker / `+$0.049126`; `META`: Py `14` maker / `8` taker / `-$0.119410` vs Rust `15` maker / `11` taker / `-$0.060274`), Python models `cancel_delay_ms = 300` (with cancel-in-flight fills) and `max_l1_depth_fraction = 1.0`, whereas Rust cancels at `0 ms` with `lot_size = 0.01` and `max_l1_depth_fraction = 0.50`.

### 2.2 Track 02 Remaining P0 Boundaries (`scripts/research_funding_basis_carry_portfolio.py`)

All three remaining carry-simulator boundary defects from `ALEPHTX_RESEARCH_AND_EXECUTION_ROADMAP_2026_11_27.md` §3 #2 were closed and verified (`12/12` tests in [test_research_funding_basis_carry_portfolio.py](../scripts/test_research_funding_basis_carry_portfolio.py)):
1. **Stale IOC Recovery (`_lookup_bbo_with_recovery`):** Eliminated `0 ms` recovered fills on stale/dark misses; upon finding `cand_ms > t_ms`, schedules `t_retry = cand_ms + TAKER_LATENCY_MS` (`330 ms`), verifies book freshness at `t_retry <= cutoff_ms` (`split_end_ms`), syncs tape at `t_retry`, and returns `None` otherwise so unclosed exposure stays explicit.
2. **Strict `split_end_ms` Cutoff (`_close_position`, `_try_unwind_orphan`):** Removed silent `min(..., split_end_ms)` clamping; fills with `t_exec`, `t_retry`, `leg_a_exit_ms`, or `leg_b_exit_ms > split_end_ms` are refused, preserving explicit `terminal_residual_notional_usd` and `<= split_end_ms` MTM (or `terminal_residual_mtm_unknown = True`).
3. **Causal `available_at_ms <= t_dec` Funding Gate (`_load_symbol_funding`, `simulate_carry_pair_timeline`):** Replaced hour-bucket pre-indexing with raw time-ordered `(available_at_ms, rate_bps)` (`live_ctx`) and `(available_at_ms, pay_ms, rate_bps)` (`settled`) streams strictly gated by `available_at_ms <= t_dec` (rejecting `available_at_ms is None` and late revisions), separating live/predicted signals from settled cash and requiring `pay_ms > leg_fill_ms`.

### 2.3 Track 01 P0 Exit Visible-Book Depth Conservation (`scripts/research_track01_profitable_alphas.py`)

Fixed `_execute_exit` in [research_track01_profitable_alphas.py](../scripts/research_track01_profitable_alphas.py) and verified (`7/7` tests in [test_research_track01_profitable_alphas.py](../scripts/test_research_track01_profitable_alphas.py)):
- **Conserved `rem_avail_by_quote`:** Tracks remaining L1 depth (`L1_DEPTH_CAP_FRAC * rp * rs`) keyed by quote timestamp `q_ts` across both `maker_postonly` partial exit and the subsequent `1 s` IOC taker fallback loop so an unchanged book snapshot is never consumed twice (e.g., `$200` L1 bid @ `50%` cap fills `$100.00` total and leaves `$400.00` terminal residual until a new book timestamp arrives).
- **Strict Cutoff (`ts_ms[j_cur] + TAKER_DELAY_MS <= sig.t1_ms`):** Replaces silent `min(sig.t1_ms, ts_ms[j_cur] + TAKER_DELAY_MS)` clamping, retaining unclosed post-cutoff exits in `terminal_residual_notional_usd`.
- **Exact Before-vs-After `W01–W27` Rescore Impact (`track01_depth_and_single_gate_w01_w27.json`):**

| Frozen Arm (`$500` Taker) | Split | Before `0/0` ($) | After `0/0` ($) | After `Prem 0.40/2.80` ($) | `Clipped Exits` (Before $\to$ After) | `After - Before` (`0/0` / `Prem` $) |
| --- | --- | ---: | ---: | ---: | :---: | ---: |
| `Overreaction` `lb15m_r60_rf0_f0` RH (`0/0`) | `selection_dev` / `holdout` / `w26_w27_oos` | `+182.5426 / +125.0551 / -35.1894` | `+182.4925 / +125.0551 / -35.1833` | `same` | `742→742` / `284→284` / `28→28` | `-0.0501 / 0.0000 / +0.0061` |
| `Overreaction` `lb15m_r60_rf0_f0` Lighter (`0.40/2.80`) | `selection_dev` / `holdout` / `w26_w27_oos` | `+172.2719 / +121.1670 / -50.4447` (`0/0`) | `+172.5090 / +121.1766 / -50.4575` (`0/0`) | **`-301.8363 / -54.4363 / -77.1790`** | `764→764` / `313→313` / `32→32` | `+0.2371 (+0.2624 Prem) / +0.0096 / -0.0128` |
| `OracleMom` `d25_h30m_f1` RH (`rwa_only` = `all`) | `selection_dev` / `holdout` / `w26_w27_oos` | `-74.5440 / -8.2834 / -18.0968` | `-74.5535 / -8.2834 / -18.0968` | `same` | `159→159` / `21→21` / `43→43` | `-0.0095 / 0.0000 / 0.0000` |
| `OracleMom` `d25_h30m_f1` Lighter (`rwa_only` = `all`) | `selection_dev` / `holdout` / `w26_w27_oos` | `-66.5374 / -9.8495 / -34.6410` (`0/0`) | `-66.5355 / -9.8495 / -34.6504` (`0/0`) | **`-168.2533 / -23.5298 / -65.2303`** | `184→183` / `17→17` / `43→43` | `+0.0019 / 0.0000 / -0.0094` |

*(Across all splits, `terminal_residual_notional_usd = $0.00` before and after; `500 ms` lakehouse BBO updates refresh rapidly enough before `sig.t1_ms` that `w26_w27_oos` losses are confirmed as genuine signal decay rather than an exit-depth bug.)*

---

## 3. Track 03 (`P1`) Core 4 Single-Factor Hold Ablation & `W28+` Pre-Registration

Evaluated via [research_track03_core4_hold_ablation.py](../scripts/research_track03_core4_hold_ablation.py) on `lighter_robinhood` (`USDG`, `0.00/0.00 bps`, `rebate_bps = 0.0`) across `Core 4` (`AAPL, AMZN, META, MSFT`, `o200_c300_e330`, `21` RWA windows `W01–W27`, `0` zero-fill windows, `756` rows, `max_parity_err = 0.0`, `max_mtm_cash_err = 1.30e-12`, `max_net_err = 0.0`). Primary accounting uses **true ledger `$/M = sum(net_cash_cost_usd) / sum(target_executed_fill_notional_usd) * 1e6`** (negative = net profit; `target_executed_fill_notional_usd = maker_volume_usd + taker_volume_usd`), reported alongside `historical_double_sided_mtm_usd_per_m` (`2 * sum(ntl)`), which agree within `$0.0002/M` here due to tiny drift over `~25–33 s` holding times.

### 3.1 Monotonic Hold Ladder (`penny=0, skew=2.0`) & Component Attribution (`W01–W27`)

| Arm (`o200_c300_e330`) | Ledger MTM `$/M` (`Real`) | Hist. DS `$/M` | MTM Net (`Real`) | Exec Notl (`Maker Vol`) | `s` | Max Port (`Max Sym`) | Hold `avg / p50 / p95 / p99 / max` | `blk` | Resid ($) | `Pos Win` |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | --- | ---: | ---: | ---: |
| `S4_speedbump_combined` (`15s`) | `+2.64` (`+2.64`) | `+2.64` | `-$33.79` (`-$33.79`) | `$12.79M` (`$9.80M`) | `0.4678` | `$1,200.1` (`$600.2`) | `9.89s / 11.4s / 16.4s / 20.1s / 48.1s` | `35` | `$0.00` | `0.33` |
| `S4_hold60s` (`60s`) | `-7.91` (`-7.92`) | `-7.91` | `+$91.73` (`+$91.80`) | `$11.59M` (`$10.64M`) | `0.1637` | `$1,237.1` (`$600.2`) | `21.64s / 13.9s / 60.7s / 62.2s / 97.5s` | `7` | `$176.80` | `0.90` |
| `S4_hold180s` (`180s`) | `-12.21` (`-12.21`) | `-12.21` | `+$136.56` (`+$136.62`) | `$11.19M` (`$11.00M`) | `0.0333` | `$1,226.9` (`$600.2`) | `29.79s / 14.6s / 136.0s / 180.6s / 188.3s` | `2` | `$176.80` | `1.00` |
| `S4_hold300s` (`300s`) | `-13.13` (`-13.13`) | `-13.13` | `+$145.74` (`+$145.80`) | `$11.10M` (`$11.04M`) | `0.0113` | `$1,295.5` (`$600.2`) | `31.58s / 14.7s / 143.3s / 281.7s / 307.3s` | `0` | `$176.80` | `1.00` |
| **`S4_hold600s_only`** (`600s, p0, sk2`) | **`-13.66`** (`-13.67`) | **`-13.66`** | **`+$151.26`** (`+$151.32`) | `$11.07M` (`$11.06M`) | `0.0020` | `$1,295.5` (`$600.2`) | `32.71s / 14.7s / 147.6s / 318.6s / 603.3s` | `0` | `$176.80` | **`1.00`** (`21/21`) |
| `S4_hold600s_penny1` (`600s, p1, sk2`) | `-10.47` (`-10.48`) | `-10.47` | `+$125.51` (`+$125.58`) | `$11.99M` (`$11.97M`) | `0.0018` | `$1,294.7` (`$600.2`) | `25.03s / 10.1s / 120.6s / 280.8s / 601.8s` | `0` | `$176.80` | `0.95` |
| `S4_hold600s_skew5` (`600s, p0, sk5`) | `-12.81` (`-12.82`) | `-12.81` | `+$133.85` (`+$133.92`) | `$10.45M` (`$10.44M`) | `0.0020` | `$1,060.2` (`$599.2`) | `32.16s / 14.3s / 146.8s / 320.8s / 601.8s` | `0` | `$176.80` | `1.00` |
| `P3_patient_600s` (`600s, p1, sk5`) | `-9.87` (`-9.88`) | `-9.87` | `+$114.39` (`+$114.46`) | `$11.59M` (`$11.58M`) | `0.0016` | `$1,000.0` (`$524.8`) | `24.22s / 9.6s / 117.0s / 272.2s / 601.8s` | `0` | `$176.80` | `0.95` |
| `W5_nopenn_15bp_180s` (`180s, p0, sk4`) | `-15.82` (`-15.82`) | `-15.82` | `+$30.50` (`+$30.50`) | `$1.93M` (`$1.90M`) | `0.0314` | `$751.2` (`$425.8`) | `27.58s / 12.2s / 146.4s / 180.7s / 183.2s` | `0` | `$0.00` | `0.95` |

- **Factorial Component Attribution:** Extending `max_hold_ms` alone (`15s -> 600s`) delivers **`+$185.05` (`-16.30 $/M`)** on `Core 4` and **`+$119.48` (`-12.46 $/M`)** on `ex-META`, collapsing forced IOC share `s` from `0.4678 -> 0.0020`. Adding `reduce_only_penny_ticks = 1` (`S4_hold600s_penny1`) degrades net PnL by **`-$25.74` (`+3.19 $/M` worse)** and flips `AAPL` from `+$6.76` (`-$1.46/M`) to `-$11.50` (`+$2.23/M`); raising `inventory_skew_bps_per_full_limit` `2.0 -> 5.0` (`S4_hold600s_skew5`) degrades PnL by **`-$17.40` (`+0.85 $/M` worse)**; combined `P3_patient_600s` is **`-$36.86` (`+3.79 $/M` worse)** than `S4_hold600s_only`.

### 3.2 `ex-META` Generalization, Concentration, Bootstrap CI & Multiple Testing

- **Cross-Split & `ex-META` Stability:** Under `S4_hold600s_only`, `Core 4` earns **`-$11.35/M` (`+$90.44`, `100%` pos) `dev` $\to$ `-$14.80/M` (`+$31.01`, `100%` pos) `holdout` $\to$ `-$29.52/M` (`+$29.81`, `100%` pos) `w26_w27_oos`**. `ex-META` (`AAPL + AMZN + MSFT`) more than doubles vs `P3_patient_600s`: **`-$7.97/M` (`+$72.64`, `20/21` = `95.2%` pos)** vs `-$3.62/M` (`+$35.11`, `71.4%` pos), advancing across all 3 splits (**`-$7.96/M` `+$53.55` `dev` $\to$ `-$3.94/M` `+$6.52` `holdout` $\to$ `-$17.20/M` `+$12.57` `w26_w27_oos`**) and outperforming `W5_nopenn_15bp_180s` (`-$9.99/M`, `+$15.59`, `71.4%` pos) by **`4.7x` cash** and **`5.8x` volume** (`$9.11M` vs `$1.56M`).
- **Symbol Diversification:** All 4 symbols are profitable under `S4_hold600s_only`: `META +$78.61` (`-40.09 $/M`, **`52.0%`** [`0.5197`] PnL share vs **`69.3%`** [`0.6931`] in `P3`), `MSFT +$38.64` (`-16.35 $/M`, `25.5%`), `AMZN +$27.24` (`-12.89 $/M`, `18.0%`), `AAPL +$6.76` (`-1.46 $/M`, `4.5%`).
- **Politis-Romano Stationary Bootstrap vs `S4` (`L = 3.0`, `B = 10,000`, `seed = 20261127`):**
  - `Core 4` (`21w`): **`+$8.812/win`** (`95% CI [+$7.915, +$9.679]`), **`-16.30 $/M`** (`95% CI [-18.03, -14.63]`, `p_non_impr = 0.0000`).
  - `ex-META` (`21w`): **`+$5.690/win`** (`95% CI [+$5.058, +$6.380]`), **`-12.46 $/M`** (`95% CI [-14.42, -10.77]`, `p_non_impr = 0.0000`).
- **Multiple-Testing Audit (`21` windows, `8` CSCV blocks = `70` splits):**
  - `core4_hold_ladder_w01_w27` (`5` arms): `S4_hold600s_only` `SR = 1.6924`, `PSR(0) = 1.000000`, `DSR(M_raw=5) = 0.998436`, `N_eff = 1.1169`, `DSR(N_eff) = 1.000000`, `DSR(M_cum=18) = 0.472853`, `CSCV PBO = 0.4571` (`prob_is_winner_oos_nonpositive = 0.0000`; rank swaps solely between `S4_hold300s` `SR=1.6872` and `S4_hold600s_only`).
  - `core4_all_ablation_w01_w27` (`9` arms): `S4_hold600s_only` `SR = 1.6924`, `PSR(0) = 1.000000`, **`DSR(M_raw=9) = 0.999544`**, `N_eff = 1.2842`, **`DSR(N_eff) = 1.000000`**, **`DSR(M_cum=18) = 0.986137`**, **`CSCV PBO = 0.0714`** (`oos<=0 = 0.0000`).
  - `ex_meta_all_ablation_w01_w27` (`9` arms): `S4_hold600s_skew5` `SR = 1.3504` (`S4_hold600s_only` `1.3289`), `DSR(M_raw=9) = 0.802097`, `N_eff = 1.3647`, `DSR(N_eff) = 0.999999`, `DSR(M_cum=18) = 0.473519`, `CSCV PBO = 0.0143` (`oos<=0 = 0.0000`).

### 3.3 `track03_core4_patient_shadow.toml` & Frozen `W28+` Falsification Thresholds

Frozen in `.agent-state/research/roadmap_upgrade_20261127/track03_w28plus_prereg_plan.json` (`plan_sha256: ce851712e70d401a090595f601b5261f71fa7a5fc1ecd743a5ebef37c4aa98d1`) and [track03_core4_patient_shadow.toml](../examples/track03_core4_patient_shadow.toml) (`capital_usd = 10000.0`, `daily_loss_limit_pct = 0.02`, `[defense]` enabled, `AAPL/AMZN/META/MSFT` `1006..1009`, `target_exchange_id = 8`, `ref_exchange_ids = [7, 10]`, `max_inventory_hold_ms = 600000`, `reduce_only_penny_ticks = 0`, `inventory_skew_bps_per_full_limit = 2.0`), gating **`min_independent_windows = 10`** (`-w28` / `>= w28` once sealed, `10h` continuous units):

1. `1_cost_reduction_vs_s4`: `ledger_mtm_usd_per_m < 0.0`, `Δledger_vs_s4 <= -5.0 $/M`, stationary 95% CI upper bound `< 0.0`.
2. `2_volume_floor`: `target_executed_fill_notional_per_window_usd >= $400,000`, `maker_volume_per_window_usd >= $380,000`.
3. `3_absolute_cash_cost`: `mtm_net_cash_cost_usd <= 0.0`, `realized_net_cash_cost_usd <= 0.0`, `positive_window_ratio >= 0.70`.
4. `4_max_gross_exposure`: `max_single_symbol_gross_exposure_usd <= $650.0`, `max_portfolio_gross_exposure_usd <= $1,600.0`.
5. `5_hold_tail_bounds`: `hold_p95_ms <= 240,000`, `max_hold_observed_ms <= 630,000`.
6. `6_forced_ioc_share`: `taker_exit_share_s <= 0.03`.
7. `7_terminal_residual_cap`: `terminal_residual_per_window_usd <= $100.0`.
8. `8_ex_meta_stability_cap`: `ex_meta_ledger_mtm_usd_per_m <= 0.0`, `ex_meta_mtm_net_cash_cost_usd <= 0.0`.
9. `9_single_symbol_share_cap`: `max_single_symbol_positive_pnl_share <= 0.80`.

---

## 4. Track 02 (`P1`) Non-Linear `Core ↔ Arcus` `B0` Settleability, Collateral & Zero-Funding-Boundary Audit

Evaluated event-by-event via [research_track02_b0_settleability_audit.py](../scripts/research_track02_b0_settleability_audit.py) (`Core ↔ Arcus` `USDC/USDC`: `lighter` `0.40/2.80 bps` + `arcus` RWA `0.00/2.1375 bps` / Crypto `1.425/4.275 bps`; frozen `b0_e10_v0.0_sh0`, `Q_MAX_CLIPS = 4.0`, `min_hedge_l1_notional_usd = clip_usd`) at `$500`, `$1,000`, and `$2,000` clip sizes across `B0_original`, `B0_funding_flat` (blocks risk-increasing opens `[ht - 120s, ht + 5s]`, forces dual-leg taker flatten `[ht - 30s, ht + 5s]`), and `flat` (`$0.00`) over `21` RWA (`15` symbols, `3,045` hourly boundaries) and `26` Crypto (`BTC/ETH/SOL`, `759` hourly boundaries) windows.

### 4.1 Non-Linear Clip Scaling, `ex-NVDA` Sensitivity & Zero-Funding-Boundary Proof (`W01–W27`)

| Domain / Clip | Arm | `dev / hold / oos` MTM ($) | Total MTM (`bps`) | `ex-NVDA` `dev / hold / oos` ($) | Orphans (`Rate`, `PnL $`) | Crossed Hrs | `funding_cash` | `95% CI` Total ($) (`p>0`) |
| --- | --- | ---: | ---: | ---: | --- | ---: | ---: | --- |
| `RWA $500` | `B0_original` | `+350.89 / +25.74 / +1.27` | **`+377.90`** (`+4.039`) | `+119.44 / +12.57 / +0.63` (`+132.64`) | `596` (`22.68%`, `-35.78`) | `2,240 / 3,045` | `-$1.35` | `[+132.33, +762.29]` (`1.000`) |
| `RWA $500` | `B0_funding_flat` | `+226.94 / +3.99 / -0.21` | **`+230.72`** (`+2.857`) | `+29.88 / -4.16 / -0.27` (`+25.45`) | `495` (`22.68%`, `-24.30`) | **`0 / 3,045`** | **`$0.00`** | `[+31.68, +550.26]` (`0.999`) |
| `RWA $1,000` | `B0_original` | `+482.18 / +19.95 / +1.54` | **`+503.66`** (`+3.350`) | `+109.86 / -0.30 / -0.52` (`+109.04`) | `616` (`25.14%`, `-98.58`) | `2,170 / 3,045` | `-$1.51` | `[+131.38, +1,088.74]` (`1.000`) |
| `RWA $1,000` | `B0_funding_flat` | `+273.69 / -8.19 / -3.35` | **`+262.15`** (`+2.048`) | `-26.64 / -16.83 / -2.23` (`-45.70`) | `496` (`24.92%`, `-63.37`) | **`0 / 3,045`** | **`$0.00`** | `[-42.45, +768.68]` (`0.882`) |
| `RWA $2,000` | `B0_original` | `+485.10 / +4.05 / -3.26` | **`+485.89`** (`+2.187`) | `-80.64 / -26.16 / -5.82` (`-112.61`) | `596` (`29.26%`, `-228.69`) | `2,033 / 3,045` | `-$0.12` | `[+29.72, +1,274.42]` (`0.996`) |
| `RWA $2,000` | `B0_funding_flat` | `+111.31 / -28.09 / -7.47` | **`+75.74`** (`+0.406`) | `-264.31 / -37.81 / -6.64` (`-308.75`) | `460` (`27.80%`, `-202.08`) | **`0 / 3,045`** | **`$0.00`** | `[-355.34, +717.55]` (`0.608`) |
| `Crypto $500` | `B0_original` | `+56.78 / +12.20 / +5.45` | **`+74.43`** (`+1.464`) | `n/a` | `255` (`19.77%`, `-29.97`) | `755 / 759` | `+$0.42` | `[+15.56, +151.02]` (`0.998`) |
| `Crypto $1,000` | `B0_original` | `+72.86 / -2.43 / +7.19` | **`+77.62`** (`+0.857`) | `n/a` | `277` (`22.20%`, `-89.92`) | `754 / 759` | `+$0.73` | `[-5.65, +192.30]` (`0.958`) |
| `Crypto $2,000` | `B0_original` | `+11.19 / -38.21 / +6.32` | **`-20.70`** (`-0.146`) | `n/a` | `345` (`28.77%`, `-240.63`) | `748 / 759` | `+$1.27` | `[-107.79, +75.94]` (`0.321`) |

- **Non-Linear Capacity Collapse at `$2,000`:** Total RWA `B0_original` PnL drops below `$1,000` (`+$503.66 -> +$485.89`), `w26_w27_oos` flips negative (`-$3.26`), `ex-NVDA` turns negative in every split (`-$112.61` total), and orphan unwind losses surge (`-$35.78 -> -$98.58 -> -$228.69`, `22.68% -> 25.14% -> 29.26%` orphan rate); Crypto `$2,000` flips net negative (`-$20.70`, `-$240.63` orphan PnL).
- **Zero-Funding-Boundary Proof & Cost:** `B0_funding_flat` achieves **`crossed_funding_hours == 0 / 3,045`** (`funding_avoidance_qualified = True`, `funding_cash_usd = $0.00`, `terminal_flat_all = True`) across all 3 RWA sizes (`0 / 759` in Crypto `$1,000/$2,000`; `1 / 759` in Crypto `$500` due to one dark book before `:00`). However, shortening mean holds (`~170–180 min -> ~25 min`) and forcing `277 / 240 / 209` hourly flattens slashes RWA PnL by paired deltas **`-$147.17`** (`[-$220.23, -$87.87]`), **`-$241.51`** (`[-$341.85, -$155.33]`), and **`-$410.15`** (`[-$640.44, -$198.67]`), flipping OOS negative (`w26_w27_oos -$0.21 / -$3.35 / -$7.47`) and Crypto completely negative (`-$37.47 / -$104.65 / -$257.51`).

### 4.2 Per-Venue Collateral, Off-Hours IMR Bottleneck, Multiple Testing & `HOLD` Verdict

| Cohort (`B0_original`) | Conc. Peak IMR (Lt / Arc $) | Req. Equity (Lt / Arc / Total USDC) | `$10k` Budget IMR Util (Lt / Arc) | `$10k` Min MM Mult (Lt / Arc) | Reconciled `USDC` Fees (Lt / Arc / Tot $) |
| --- | ---: | ---: | ---: | ---: | ---: |
| `RWA $500` | `$749.9 / $890.3` | `$1,251 / $1,392 / $2,643` | `15.0% / 17.8%` | `13.10x / 10.63x` | `$114.85 / $168.25 / $283.11` |
| `RWA $1,000` | `$1,150.1 / $1,628.9` | `$2,153 / $2,631 / $4,784` | `23.0% / 32.6%` | `8.88x / 6.88x` | `$196.32 / $263.55 / $459.88` |
| `RWA $2,000` | `$1,794.6 / $2,936.3` | `$3,803 / $4,944 / $8,747` | `35.9% / 58.7%` | `5.13x / 3.83x` | `$317.61 / $371.35 / $688.95` |

- **Arcus Off-Hours IMR Bottleneck:** `arcus` applies `1.5x` RTH IMR outside `04:00–20:00 America/New_York` Mon–Fri (`0.075` NVDA/core RWA, `0.15` AMD/HOOD/MSTR/MU/PLTR, `0.30` COIN), making Arcus IMR the binding collateral leg across every RWA size (`$890.3 / $1,628.9 / $2,936.3` vs Lighter `$749.9 / $1,150.1 / $1,794.6`). `100%` of fees settle in `USDC` (`fill_coverage_reconciled = True`).
- **Multiple Testing (`DSR` & `CSCV PBO`):** RWA `1k_grid` (`32` trials): `N_eff = 1.2534`, `DSR(N_eff) = 1.0000`, `DSR(M_raw=32) = 0.0000`, `CSCV PBO = 0.1429` (`16w`); RWA `all_audit_trials` (`36` trials): `N_eff = 1.2351`, `DSR(N_eff) = 1.0000`, `DSR(M_raw=36) = 0.0000`, `CSCV PBO = 0.1571`. Crypto (`24w`): `N_eff = 3.0166` / `2.9817`, `DSR(N_eff) = 0.0000`, `DSR(M_raw) = 0.0000`, `CSCV PBO = 0.0000`.
- **Why Track 02 Stays `HOLD`:** Aleph lakehouse contains **`0` `arcus` `funding_and_ctx` parquet files** (vs `235` RWA / `293` Crypto hours on `lighter`; `--probe-public-meta` confirms public `/v1/fundingRates` returns only recent 500h rates without per-fill settlement cash/oracle witness). Because `B0_original` crosses `2,170 / 3,045` funding boundaries (`$1,000` RWA) and `B0_funding_flat` loses OOS profitability, Track 02 remains **`HOLD / PENDING_ARCUS_FUNDING_WITNESS`**.

---

## 5. Track 01 (`P1`) Controlled Single-Factor Gate (`tight_spread_le_5bp`) & Honest `NO-GO / HOLD`

Evaluated via [research_track01_controlled_gate_eval.py](../scripts/research_track01_controlled_gate_eval.py) on `OverreactionReversion` (`lb15m_r60_rf0_f0`, `lighter_robinhood` `0.0/0.0 bps`, `$500` taker, P0-fixed `_execute_exit`) across identical `978,539` candidate `(win, symbol, t_dec)` events (`700,336` `selection_dev`, `252,937` `holdout`, `25,266` `w26_w27_oos`). Purely on `selection_dev` (`w01–w20`), `spread_bps > 5.0` lost `-$49.47` (`160` trades); comparing the three single-factor directions (`baseline +$182.49`, `tight_spread_le_5bp +$217.70`, `us_rth_utc +$216.62`, `l1_depth_ge_1000 +$215.91`), **`tight_spread_le_5bp` (`spread_bps <= 5.0`)** was frozen before inspecting `holdout` or `w26_w27_oos`.

### 5.1 Identical-Candidate Arm Comparison & Paired Stationary Bootstrap (`L = 3.0`, `B = 5,000`, `seed = 20261127`)

| Split (`Windows`) | Arm | `Cand -> Chk -> Rej -> Fill` | `Realized = MTM` ($) | `+1.0 bp` ($) | `Worst Win` ($) | `Max DD` ($) | `Win SR` / `PSR(0)` | `Ex-Top1` (`Top1`) | `Delta (gate - base)` (`95% CI`, `P_boot>0`) |
| --- | --- | --- | ---: | ---: | ---: | ---: | :---: | --- | --- |
| `selection_dev` (`19`) | `baseline` | `700,336 -> 2,244 -> 0 -> 2,243` | `+182.49` | `+2.43` | `-95.91` (`w14`) | `147.58` | `+0.216 / 0.818` | `+94.24` (`NVDA +88.25`) | `—` |
| `selection_dev` (`19`) | `single_gate` | `700,336 -> 3,323 -> 1,088 -> 2,234` | **`+217.70`** | **`+38.67`** | `-84.65` (`w14`) | `135.66` | `+0.262 / 0.874` | `+117.12` (`NVDA +100.58`) | **`+$35.21`** (`[+$11.06, +$61.44]`, `0.9978`) |
| `holdout` (`5`) | `baseline` | `252,937 -> 761 -> 0 -> 761` | **`+125.06`** | **`+65.15`** | `-6.67` (`w21`) | `6.67` | `+1.197 / 0.952` | `+92.85` (`NVDA +32.20`) | `—` |
| `holdout` (`5`) | `single_gate` | `252,937 -> 1,334 -> 572 -> 762` | `+115.15` | `+55.45` | `-8.98` (`w21`) | `8.98` | `+1.037 / 0.950` | `+82.94` (`NVDA +32.20`) | **`-$9.91`** (`[-$23.55, +$3.74]`, `0.1080`) |
| `w26_w27_oos` (`2`) | `baseline` | `25,266 -> 93 -> 0 -> 93` | `-35.18` | `-42.13` | `-32.16` (`w27`) | `35.18` | `0.000 / 0.000` | `-35.18` (`AAPL 0.00`) | `—` |
| `w26_w27_oos` (`2`) | `single_gate` | `25,266 -> 99 -> 7 -> 92` | **`-34.68`** | `-41.61` | `-31.74` (`w27`) | `34.68` | `0.000 / 0.000` | `-34.68` (`AAPL 0.00`) | `+$0.51` (`[+$0.19, +$0.82]`, `1.0000`) |
| `oos_w21_w27` (`7`) | `baseline` | `278,203 -> 854 -> 0 -> 854` | `+89.87` | `+23.02` | `-32.16` (`w27`) | `35.18` | `+0.456 / 0.846` | `+63.94` (`NVDA +25.94`) | `—` |
| `oos_w21_w27` (`7`) | `single_gate` | `278,203 -> 1,433 -> 579 -> 854` | `+80.47` | `+13.84` | `-31.74` (`w27`) | `34.68` | `+0.410 / 0.830` | `+54.54` (`NVDA +25.94`) | **`-$9.40`** (`[-$24.53, +$3.53]`, `0.1142`) |
| `all_w01_w27` (`26`) | `baseline` / `single_gate` | `3,097` / `3,088` fills | `+272.36` / `+298.17` | `+25.45` / `+52.51` | `-95.91` / `-84.65` | `147.58` / `135.66` | `+0.261` / `+0.290` | `+158.18` / `+171.66` | `+$25.81` (`[-$13.03, +$62.79]`, `0.8990`) |

*(All `flat` arms = `$0.00` across `0` trades; `terminal_residual_notional_usd = $0.00` across every arm and split.)*

### 5.2 Multiple Testing, Markout Attribution & Why Track 01 Stays `NO-GO / HOLD`

- **Multiple Testing (`DSR` & `CSCV PBO`):**
  - Controlled `2`-arm (`baseline`, `single_gate`): `selection_dev` (`19w`) `N_eff = 1.0043`, `DSR_raw = 0.8580`, `DSR_eff = 0.8740`, `CSCV PBO = 0.2000`; `all_w01_w27` (`26w`) `N_eff = 1.0061`, `DSR_raw = 0.9229`, `DSR_eff = 0.9306`, **`CSCV PBO = 0.4286`**.
  - Discovery-adjusted `4`-arm (`baseline`, `single_gate`, `screen_us_rth_utc`, `screen_l1_ge_1000`): `selection_dev` `N_eff = 1.6035`, `DSR_raw = 0.9762`, `DSR_eff = 0.9937`, `CSCV PBO = 0.2143`; `all_w01_w27` `N_eff = 1.5822`, `DSR_raw = 0.9944`, `DSR_eff = 0.9992`, **`CSCV PBO = 0.5857`**.
- **Causal Attribution & Regression Failures:** In `selection_dev`, `60s`-declustered `gate_fail` (`spread_bps > 5.0`) candidates posted `-6.51 bp` (`5m`) and `-4.17 bp` (`15m`) mid markouts, explaining the `+$35.21` dev lift. In `holdout`, `gate_fail` bounced **`+22.39 bp`** (`15m`), causing `-$9.91` degradation; in `w26_w27_oos`, even `gate_pass` `5m/15m` mid markouts turned negative (`-1.60 bp / -3.24 bp`), confirming underlying signal regime decay. Three of eight regression checks fail (`w26_w27_oos_mtm_positive`: `-$34.68 <= $0.00`; `oos_w21_w27_delta_clears_threshold`: `-$9.40 < $10.00`; `oos_w21_w27_boot_ci95_lower_gt_zero`: `-$24.53 <= $0.00`), locking Track 01 at **`NO-GO / HOLD`**.

---

## 6. Next Execution Gates & Operational Posture (`HOLD`)

No track is authorized for live orders, order cancellations, or Tokyo production restarts (`HOLD` remains in effect):

1. **Track 03 (`S4_hold600s_only`) Frozen `W28+` Shadow & Prospective Validation Gate:**
   - Run [track01_track02_shadow.rs](../src/bin/track01_track02_shadow.rs) with [track03_core4_patient_shadow.toml](../examples/track03_core4_patient_shadow.toml) in offline replay / read-only shadow mode only once **`>= 10` independent continuous `10 h` windows (`-w28` / `>= w28`)** seal, verifying all 9 pre-registered thresholds in `track03_w28plus_prereg_plan.json` (`plan_sha256: ce851712...`) and monitoring the non-P0 strategy-layer parity items (`cancel_delay_ms`, `ref`-triggered evaluation, `lot_size`, `max_l1_depth_fraction`) documented in §2.1.
2. **Track 02 (`B0`) Lakehouse Telemetry Gate:**
   - Instrument `Arcus` `funding_and_ctx` capture in the Tokyo/Aleph lakehouse (`symbol` + timestamped hourly rates + settlement witness) before re-scoring `Core ↔ Arcus` `B0`; until settled `Arcus` funding cash is observed in the lakehouse, keep `B0` strictly on `HOLD`.
3. **Track 01 (`OverreactionReversion` & `OracleDiscoveryMomentum`) Freeze:**
   - Keep [directional_alpha.rs](../src/strategy/directional_alpha.rs) disabled (`enabled: false`) and halt further short-horizon gate or threshold tuning unless fresh `>= w28` evidence demonstrates positive OOS edge surviving verified friction.
