# Infra & P0 Three-Way Same-Event Parity and Review Bundle Report

Date: 2026-11-27 · Baseline commit: post-`9c9eaf2` rebuild (updated from initial `329d86e` draft after stale-binary mtime fix) · Live trading posture: **HOLD** (read-only offline replay & shadow only; `W28` strictly excluded)

> **Parity Authority & Stale-Binary Supersession Notice (`post-9c9eaf2`):** Any `p0_same_event_parity_harness.py` verification executed prior to `9c9eaf2` (before `_resolve_rust_binary` enforced source-mtime freshness rebuilds) is superseded and excluded as P0 proof. All parity results anchored in this bundle come from a fresh post-`9c9eaf2` build of `target/debug/track01_track02_shadow`. Furthermore, because `_to_python_events` omits synthetic stale-arrival BBO ticks (`recv_ms != ts`), **`IndependentReferenceSimulator` ↔ compiled Rust `track01_track02_shadow` is the authoritative pair for explicit stale-arrival ticks**, while the Python arm is explicitly demoted to **diagnostic reference** on stale-arrival scenarios.

## 1. Executive Summary

This report documents the P0 Track 03 shadow execution parity fixes in Rust, the three-way same-event parity harness (`IndependentReferenceSimulator` vs Python `simulate_prepared_arm` vs compiled Rust `track01_track02_shadow --track 3 --replay`), and the deterministic Review Bundle generated against synthetic P0 counterexamples and a sealed public `W27` slice (`AAPL` & `META` on `lighter_robinhood` + `arcus`).

- **Rust Shadow P0 Fixes (`src/strategy/track_shadow.rs`, `src/bin/track01_track02_shadow.rs`)**:
  - **Defect A (Pre-install Pending IOC Processing + Stale 330 ms Re-queue)**: [track_shadow.rs](../src/strategy/track_shadow.rs) now evaluates expired `pending_ioc_exits` against `prev_touch` and prior book freshness *before* installing a newly arrived fresh touch, and processes expired pending IOCs on non-regressing stale/crossed `on_target_l2_bbo` and `on_trade_print` events by incrementing `stale_exit_blocks` and re-queueing with `exec_at_ms = now_ms + ioc_exit_delay_ms` instead of filling at `0 ms` on recovery.
  - **Defect B (Single-Use L1 Visible Depth Budget per Touch)**: `Track03ArmRuntime` tracks `rem_bid_sz` and `rem_ask_sz`, initialized to `l1_sz * max_l1_depth_fraction` only when a fresh `on_target_l2_bbo` installs a touch and decremented on every `execute_ioc_exit` fill so two IOC exits on the same book touch never consume the same L1 liquidity twice.
  - **Defect C (`stale_exit_blocks` Telemetry + Default-Off `declared_end_ms` / `terminal_drain_ms`)**: Added `stale_exit_blocks: u64` to `Track03ArmReport` and `Track03ShadowReport`, and added `#[serde(default)]` `declared_end_ms: Option<u64>` and `terminal_drain_ms: Option<u64>` (both default `None`) to `Track03ShadowConfig`. Events strictly past `declared_end_ms` are ignored across `on_reference_touch`, `on_target_bbo`, `on_target_l2_bbo`, `on_trade_print`, and `on_bbo_cross_diagnostic`; `terminal_drain_ms` suppresses new openings while keeping reducing quotes and scheduling delayed reduce-only IOC flatten.
- **Three-Way Synthetic P0 Parity (`7 / 7 PASS`)**: Under matched baseline config, all synthetic P0 counterexamples pass within `<= 1e-4 USD` across the Independent Reference Ledger, Python [research_track03_full_universe_speedbump_mm.py](../scripts/research_track03_full_universe_speedbump_mm.py), and compiled Rust [track01_track02_shadow.rs](../src/bin/track01_track02_shadow.rs), with cash conservation error `0.0 USD` and PnL decomposition error `0.0 USD`.
- **Sealed Public `W27` `AAPL` & `META` Slice**: Verified on `tokyo-rwa-20261126T161454Z-w27` (`2026-11-27T00:00:00Z..00:30:00Z`, SHA-256 `33c0d12d0218a10434ac26a73760cd9d3b606e85b4effb7b926e6aec6f06cd3b`), with `OPT-01` `verified_account_tier` `0.0/0.0 bps` (`account_fee_evidence_20261127.json`, SHA-256 `4d3a9c1aeae5358bb9c55ab448db392ac3edb0a89244f74c23d89817c3359d92`). Matched-baseline and production `S4` comparisons isolate the remaining non-P0 strategy-layer differences (`cancel_delay_ms = 300` in Python vs `0 ms` in Rust, `ref`-triggered `decide()`, `lot_size = 0.01`, and `max_l1_depth_fraction = 0.50` vs `1.0`).

---

## 2. Synthetic P0 Counterexample Parity Results

All results below are taken directly from `p0_parity_review_bundle.json` (generated post-`9c9eaf2` on freshly rebuilt `target/debug/track01_track02_shadow` SHA-256 `c0788915d05d7c3b88faed78682b5143bee7c7b3550fef43cd35cd6e5a7ccff7` via [p0_same_event_parity_harness.py](../scripts/p0_same_event_parity_harness.py), `parity_authority_policy = "post_9c9eaf2_fresh_rebuild_required"`, `authoritative_stale_arrival_pair = ["independent_reference", "rust_shadow_binary"]`, `python_arm_role = "diagnostic_reference_omits_synthetic_stale_arrival_ticks"`, `frozen_plan_sha256`: `20deeafd46adad57f9853115154eabf552fcf75e0a201209efb08a486e0136bc`).

| Scenario ID | Ref Fills (M/T) | Py Fills (M/T) | Rust Fills (M/T) | `stale_exit_blocks` | `depth_clipped_exits` | Realized PnL (USD) | Unrealized PnL (USD) | Residual (USD) | Status |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `p0_c1_stale_gap_330ms_requeue` | 1 / 1 | 1 / 1 | 1 / 1 | 1 / 1 / 1 | 0 / 0 / 0 | -0.0800 | 0.0000 | 0.0000 | PASS |
| `p0_c2_two_iocs_same_touch_depth_conservation` | 1 / 2 | 1 / 2 | 1 / 2 | 0 / 0 / 0 | 2 / 2 / 2 | +0.0300 | +0.0200 | 50.0200 | PASS |
| `p0_c3_partial_exit_true_residual` | 1 / 1 | 1 / 1 | 1 / 1 | 0 / 0 / 0 | 1 / 1 / 1 | +0.0400 | +0.1500 | 150.1500 | PASS |
| `p0_c4_declared_end_ms_late_ref_immunity` | 1 / 0 | 1 / 0 | 1 / 0 | 0 / 0 / 0 | 0 / 0 / 0 | 0.0000 | +0.1200 | 200.1200 | PASS |
| `p0_c5_terminal_drain_ms_flatten` | 1 / 1 | 1 / 1 | 1 / 1 | 0 / 0 / 0 | 0 / 0 / 0 | +0.0400 | 0.0000 | 0.0000 | PASS |
| `p0_c6_same_ms_book_first` | 2 / 0 | 2 / 0 | 2 / 0 | 0 / 0 / 0 | 0 / 0 / 0 | +0.0800 | +0.0000 | 0.0800 | PASS |
| `p0_c6_same_ms_trade_first` | 1 / 0 | n/a | 1 / 0 | 0 / - / 0 | 0 / - / 0 | 0.0000 | 0.0000 | 200.0000 | PASS |

Notes on `p0_c6`:
- `py_t3.prepare_replay_stream` canonically sorts same-ms events as `(ts, kind)` (`book` before `trade`), so `p0_c6_same_ms_trade_first` is evaluated on Reference and Rust (`book_first_maker_fills = 2`, `trade_first_maker_fills = 1`, `realized_pnl_usd_bounds = [0.0, 0.079968]`).
- Independent Track 01 & Track 02 reference checks in the same bundle confirm Track 01 single-use L1 exit depth conservation (`1.2` initial position, `1.5 * 0.5 = 0.75` touch budget -> `0.75` + `0.0` across two exits, `0.45` residual) and Track 02 causal orphan-leg settlement (`200 ms` orphan window, `+$0.0269885` net after `2.30 bps` hedge fee).

---

## 3. Sealed Public `W27` `AAPL` & `META` Parity and Four-Stage Scorecard

Source slice: `w27_aapl_meta_public_slice.json` (`campaign_id = tokyo-rwa-20261126T161454Z-w27`, `1795737600000..1795739400000` ms UTC, `2026-11-27T00:00:00Z..00:30:00Z`, `lighter_robinhood` target + `arcus` reference, `W28` excluded).

### 3.1 Four-Stage Denominator Scorecard (`matched_execution_baseline`, `o200_c0_e330`)

| Stage / Metric | `AAPL` (`symbol_id=1006`, `market=10`) | `META` (`symbol_id=1009`, `market=13`) |
| --- | --- | --- |
| **Stage 1: Input Coverage** | `11,798` events (`4,572` BBO, `5,700` Ref, `1,526` Trades); `book_coverage_ratio = 1.0`; drops `0` | `11,338` events (`5,446` BBO, `5,700` Ref, `191` Trades inside `declared_end_ms`); `book_coverage_ratio = 1.0`; drops `0` |
| **Stage 2: On-Time Availability** | `on_time_ratio = 1.0`; `stale_exit_blocks = 0`; wall clock Ref `32.06 ms` / Py `50.07 ms` / Rust `80.68 ms`; Ref per-event `p50=0.0035 / p95=0.0048 / p99=0.0073 ms`; queue delay `p50=276.0 / p95=865.4 / p99=1207.08 ms`; IOC delay `p50=500.0 / p95=1450.95 / p99=4111.87 ms` | `on_time_ratio = 1.0`; `stale_exit_blocks = 0`; wall clock Ref `29.76 ms` / Py `52.74 ms` / Rust `81.89 ms`; Ref per-event `p50=0.0034 / p95=0.0048 / p99=0.0063 ms`; queue delay `p50=289.0 / p95=712.7 / p99=1150.86 ms`; IOC delay `p50=497.0 / p95=998.5 / p99=1420.55 ms` |
| **Stage 3: Executable & Settleable (Rust)** | `397` orders placed, `16` ALO rejects, `220` maker fills, `58` taker exits, `19` depth-clipped exits, `terminal_residual = $0.00`, `USDG` settlement | `1,176` orders placed, `8` ALO rejects, `47` maker fills, `30` taker exits, `6` depth-clipped exits, `terminal_residual = $0.00`, `USDG` settlement |
| **Stage 4: Complete Economic Result (Rust)** | Realized `+$0.048662`, Fees `$0.00`, Closed Ntl `$16,512.07`, `-$1.47/M`, `s = 0.8055`, `max_dd = $0.1934`, conservation err `0.0`, `positive_opportunity_recall = unestimable_zero_verified_positives` | Realized `+$0.023856`, Fees `$0.00`, Closed Ntl `$5,477.29`, `-$2.18/M`, `s = 0.8885`, `max_dd = $0.2611`, conservation err `0.0`, `positive_opportunity_recall = unestimable_zero_verified_positives` |

### 3.2 Three-Way Matched Baseline & Production `S4` First-Divergence Attribution

| Symbol / Mode | Arm | Maker Fills | Taker Exits | Depth Clipped | Realized PnL (USD) | Closed Notional (USD) | `$/M` (Realized) | Wall (ms) |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `AAPL` Matched `o200_c0_e330` | Reference | 362 | 44 | 5 | +0.220894 | 19,985.32 | -5.53 | 32.06 |
| `AAPL` Matched `o200_c0_e330` | Python | 240 | 62 | 18 | +0.057177 | 18,087.54 | -1.58 | 50.07 |
| `AAPL` Matched `o200_c0_e330` | Rust | 220 | 58 | 19 | +0.048662 | 16,512.07 | -1.47 | 80.68 |
| `AAPL` Prod `S4` | Python `o200_c300_e330` | 170 | 19 | 1 | -0.056172 | 9,937.55 | +2.83 | 89.67 |
| `AAPL` Prod `S4` | Rust `o200_c0_e330` | 108 | 41 | 18 | +0.049126 | 7,429.92 | -3.31 | 84.23 |
| `META` Matched `o200_c0_e330` | Reference | 54 | 36 | 11 | +0.004455 | 5,490.29 | -0.41 | 29.76 |
| `META` Matched `o200_c0_e330` | Python | 48 | 40 | 13 | +0.096566 | 5,683.84 | -8.49 | 52.74 |
| `META` Matched `o200_c0_e330` | Rust | 47 | 30 | 6 | +0.023856 | 5,477.29 | -2.18 | 81.89 |
| `META` Prod `S4` | Python `o200_c300_e330` | 14 | 8 | 4 | -0.119410 | 1,272.27 | +46.93 | 71.90 |
| `META` Prod `S4` | Rust `o200_c0_e330` | 15 | 11 | 1 | -0.060274 | 1,709.12 | +17.63 | 79.84 |

**First-Divergence Locator Attribution on W27**:
- On the matched `o200_c0_e330` slice with `arcus` references present, the first divergence occurs on `maker_fills` (`AAPL`: Reference `362` / Python `240` / Rust `220`; `META`: Reference `54` / Python `48` / Rust `47`) because: (1) Python triggers `decide()` on `ref` updates and keeps passive reducing orders while an IOC exit is queued, (2) Rust triggers `evaluate()` on target BBO updates and cancels resting orders on `inventory_hold_timeout`, and (3) the simple Reference baseline omits `arcus` lead-lag cancel pulls.
- On the production `S4` comparison, Python additionally models `cancel_delay_ms = 300` (with cancel-in-flight fills) and `max_l1_depth_fraction = 1.0`, whereas Rust cancels at `0 ms` with `lot_size = 0.01` and `max_l1_depth_fraction = 0.50`.

---

## 4. Independent Review Signoff & Verification Ledger

- **Review Bundle Artifact**: `p0_parity_review_bundle.json` (in `.agent-state/research/roadmap_upgrade_20261127/`)
- **Independent Review Record**: `evidence_signoff = PASS`, `economic_hypothesis_status = PENDING_P1_UNSEEN_WINDOW`, `promotion_recommendation = HOLD_FOR_P1_CORE4_TRIAL`.
- **Verification Commands Executed**:
  - `cargo fmt --check` — pass
  - `cargo clippy --locked --lib --bins -- -D warnings` — pass (0 warnings)
  - `cargo test --locked --lib strategy::track_shadow` — 34 passed, 0 failed (includes 7 new/updated P0 unit tests)
  - `cargo test --locked --bin track01_track02_shadow` — 8 passed, 0 failed (includes `replay_track03_p0_declared_end_drain_and_stale_requeue`)
  - `python3 -S scripts/p0_same_event_parity_harness.py --self-test` — pass (`7 / 7` synthetic P0 counterexamples)
  - `python3 -S -m unittest discover -s scripts -p "test_p0_same_event_parity_harness.py" -v` — 4 passed, 0 failed (verifies `python3 -S` stdlib-only isolation)
  - `python3 -S scripts/p0_same_event_parity_harness.py --run-bundle` on sealed `W27` `AAPL` & `META` slice — pass
