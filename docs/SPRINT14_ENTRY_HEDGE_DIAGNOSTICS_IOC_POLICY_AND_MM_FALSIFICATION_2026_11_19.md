# Sprint 14: Entry-Hedge Diagnostics, Decoupled Hedge IOC Policy & Track 3 Cancel-Pending Audit (2026-11-19)

> **Status**: Offline engineering & Codex peer review complete; all three live tracks remain **`HOLD`**. Zero live trading, cancellations, deployments, restarts, or remote mutations.

---

## 1. Executive Summary

Incorporating Codex's `2026-11-19 12:30–12:42 CST` 339-replay independent verification (`arb_phase_v2`, `arb_latency`, `mm_local_size`, `residual_rank`, `zero_table`, `cold_resume`) and Claude Code's three-scenario report, Sprint 14 delivers three targeted items:

1. **`S14-1` (Track 2 — Arb Entry-Hedge Diagnostics & Decoupled Hedge IOC Policy)**:
   - Added read-only `MakerFirstConfig::diagnose_entry_hedge: bool` (default `false`; report field `entry_hedge_diagnostics` omitted when `false`, keeping all frozen report SHA-256 bytes identical) to answer Codex's priority request (*"复核建对冲时首个可用价与等待流动性的时间"*).
   - Added opt-in `MakerFirstConfig::hedge_ioc_policy: Option<HedgeIocPolicy>` (default `None`) decoupling hedge/unwind IOC protective limit cushion (`limit_buffer_bps`), depth impact cap (`max_depth_impact_bps`), and unhedged retry spacing (`retry_ms`) from opening admission's tight `pair.limit_buffer_bps` / `pair.max_depth_impact_bps` / `reprice_ms`.
2. **`S14-2` (Track 3 — Empirical Falsification of `isolate_cooldown_cancel_churn` & 97% `cancel_pending` Audit)**:
   - Completed the per-window and multi-horizon (`t20/t30/t50`, `2026-07-01_A_BTC`) audit requested by Codex: even with Sprint 13's directional safety guard, `isolate_cooldown_cancel_churn = true` is dominated by `false` in **8/9** `(cohort, horizon)` cells (`1` tied) and *increases* total cancels in `monthly` and `w07`. Formally marked deprecated-experimental (`default = false`) in `src/config.rs`.
   - Quantified the real Track 3 bottleneck directly from Codex's `monthly t50_iso0` markout ledger: **`97.0%` of 1s adverse selection (`-$2.898 / -$2.987`) and `>100%` of 30s markout loss (`-$2.715` vs `+$0.703` on non-cancel-pending fills)** occurs on `cancel_pending` fills (`128/178 = 71.9%`).
3. **`S14-3` (Codex Peer Review — `zero_table`, `cold_resume`, `arb_latency`, `mm_local_size`, `residual_rank`)**:
   - Verified `scripts/quant_lakehouse.py` zero-row table integrity fix (`L1811–1819`), confirmed no other manifest-verifying site skips zero-row tables, and validated all 200 script tests + 42 monitor tests on Mac.

---

## 2. `S14-1`: Track 2 Entry-Hedge Bottleneck (`W01 3.697s` / `W05 0.803s`)

### 2.1 Root-Cause Analysis
Codex's exact zero-residual phase attribution (`arb_phase_v2/summary.json`) showed:
- `W01 ETH` (`strict` & `grace500`): `entry_unhedged = -$0.132275` over **`3,697 ms`** vs `balanced = +$0.004675` (`strict`) / `-$0.003575` (`grace500`).
- `W05 ETH`: `entry_unhedged = -$0.039054` over **`803 ms`** vs `balanced = -$0.019810` / `-$0.003962`.
- Slashing `transport_ms` (`10 -> 1 ms`) and `fill_notice_ms` (`100 -> 0 ms`) in `arb_latency` (`96` replays) only improved stress cash by `16.2%` (`-$0.24457 -> -$0.20486`) while increasing `W01` `ioc_fills` from `3 -> 5`, because venue `taker_delay_ms = [300, 300]` remains fixed and `submit_ioc` (`src/strategy/relative_value/paper/maker_first.rs`) previously:
  1. Reused opening admission's tight `pair.limit_buffer_bps` and `pair.max_depth_impact_bps` in `Depth::build(...)`, so a `1–3 bp` adverse move during the `310 ms` IOC flight triggers `ioc_price_misses` (`poll_ioc`) or shallow partial fills (`last.base < target`), forcing repeated `310 ms + reprice_ms` retry loops while naked.
  2. Throttled unhedged IOC retries with maker `reprice_ms`.
  3. Lacked an episode-level read-only breakdown of `first_available_hedge_touch_at_fill`, `first_available_hedge_touch_at_notice`, and exact `notice_wait_ms + submit_wait_ms + ioc_flight_and_retry_ms == entry_unhedged_ms` attribution.

### 2.2 Code Changes (`src/strategy/relative_value/paper/maker_first.rs`)
- **`MakerFirstConfig::diagnose_entry_hedge: bool`** (default `false`) + **`MakerFirstReport::entry_hedge_diagnostics: Option<EntryHedgeDiagnostics>`** (`skip_serializing_if = "Option::is_none"`):
  - Purely read-only observer; zero impact on execution or serialized JSON bytes when `false`.
  - Records per-episode `opening_fill_ms`, `first_notice_ms`, `first_ioc_submit_ms`, `first_ioc_arrival_ms`, `first_hedge_fill_ms`, `first_hedge_book_ms`, `fully_hedged_ms`, exact additive millisecond split (`notice_wait_ms`, `submit_wait_ms`, `ioc_flight_and_retry_ms`), `first_available_hedge_touch_at_fill`, `first_available_hedge_mid_at_fill`, `first_available_hedge_touch_at_notice`, `first_ioc_submit_touch`, `first_ioc_limit`, `entry_hedge_vwap`, `outcome` (`"hedged"`, `"passive_offset_flat"`, `"forced_drain_triggered"`, `"forced_drain"`, `"open_at_cutoff"`), and failure/wait counts (`ioc_price_misses`, `ioc_depth_misses`, `ioc_expired`, `stale_book_waits`, `depth_unavailable_waits`, `below_minimum_waits`, `reprice_throttle_waits`).
- **`MakerFirstConfig::hedge_ioc_policy: Option<HedgeIocPolicy>`** (default `None`):
  - Scoped strictly to `submit_ioc` (`ioc_rules()` + `hedge_retry_ms()`) and `paired_drain_blocked` retry pacing; never loosens opening/closing maker admission (`quote()`, `reduce_limit()`), `final_mark()`, `book.in_bounds(limit)`, `rules.participation`, `rules.max_single_leg`, `min_order_base` / `min_reduce_ioc_base`, `taker_delay_ms`, or `order.quantity <= exposure.abs()`.
  - Validation fails closed on `limit_buffer_bps` outside `0..=100`, `max_depth_impact_bps` outside `0.01..=1000`, `limit_buffer_bps >= max_impact_bps` when `limit_buffer_bps > 0` (preventing tick-alignment truncation from emptying level 0 in `Depth::build`), or `retry_ms` outside `1..=2_000`.
- **Unit/Replay Tests Added (`+3`)**:
  - `entry_hedge_diagnostics_is_read_only_and_attributes_price_miss_retries`
  - `hedge_ioc_policy_avoids_price_miss_and_cuts_unhedged_lag`
  - `hedge_ioc_policy_validation_rejects_invalid_bounds`

### 2.3 Recommended Codex Ablation on `aleph` (Offline Tokyo `W01–W06`)
1. Flip `diagnose_entry_hedge: true` (keeping `hedge_ioc_policy: null`) on `W01`/`W05` ETH `grace200` to verify byte-identical PnL/fills while obtaining the exact millisecond & price-miss/depth-wait breakdown of `3,697 ms` / `803 ms`.
2. Run a paired counterfactual with `hedge_ioc_policy: {"limit_buffer_bps": "10", "max_depth_impact_bps": "15", "retry_ms": 25}` (and `"limit_buffer_bps": "5", "max_depth_impact_bps": "10", "retry_ms": 25`) on the same 24 Tokyo wallets to measure whether single-shot hedge fills compress `entry_unhedged_ms` and drift loss.

---

## 3. `S14-2`: Track 3 `isolate_cooldown_cancel_churn` Falsification & `cancel_pending` Diagnosis

### 3.1 Multi-Horizon Falsification (`s13_final_20261119/mm/review.json` & `summary.json`)

| Cohort | Arm | Stress Cash | Maker Fills / Turnover | Taker Turnover | Cancels | 1s Markout | 5s Markout | 30s Markout |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `monthly` (14w) | `t20_iso0` | `-$2.486` | `153` / `$15,399` | `$10,730` | `501,480` | `-1.09 bp` | `-1.19 bp` | `-1.51 bp` |
| `monthly` (14w) | `t20_iso1` | `-$3.841` | `187` / `$17,739` | `$9,969` | `501,655` | `-1.08 bp` | `-1.24 bp` | `-2.11 bp` |
| `monthly` (14w) | `t30_iso0` | `-$1.131` | `165` / `$16,848` | `$10,821` | `500,791` | `-1.29 bp` | `-1.29 bp` | `-0.96 bp` |
| `monthly` (14w) | `t30_iso1` | `-$4.125` | `191` / `$17,560` | `$10,063` | `501,305` | `-1.26 bp` | `-1.60 bp` | `-2.19 bp` |
| `monthly` (14w) | `t50_iso0` | `-$0.298` | `178` / `$19,589` | `$9,850` | `499,956` | `-1.52 bp` | `-1.60 bp` | `-1.03 bp` |
| `monthly` (14w) | `t50_iso1` | `-$3.879` | `199` / `$18,947` | `$10,394` | `499,975` | `-1.13 bp` | `-1.33 bp` | `-1.72 bp` |
| `taiwan` (2w) | `t50_iso0` / `iso1` | `+$1.725` / `+$1.725` | `22` / `$3,339` | `$2,136` | `99,890` / `99,886` | `-3.26 bp` | `-2.79 bp` | `+1.46 bp` |
| `w07` (1w) | `t50_iso0` / `iso1` | `+$0.131` / `+$0.095` | `5` / `$242` | `$242` | `49,057` / `49,220` | `+1.47 bp` | `+1.92 bp` | `+5.25 bp` |

- **Why `2026-07-01_A_BTC` swung `+$2.2675 -> -$1.1504` (`-$3.4179`) while 1s/5s markout improved (`-1.52 -> -1.13 bp`)**:
  - Deferring passive-side cancels on partner retreat/cooldown/re-entry drops that side's paired `to_place` refresh (`inventory_neutral_mm.rs:3797,3803`). In the short term (`1–5s`), leaving the surviving side farther from mid avoids some immediate run-overs (`cancel_pending` 1s markout improved `-$2.90 -> -$1.65`). Over `30s`, however, the surviving side fails to step with fair value and passively unload inventory, driving smaller average maker fills (`$110.05 -> $95.21`), worse 30s markout (`-1.03 -> -1.72 bp`), higher taker unwinds on `2026-07-01_A_BTC` (`$3,066.7 -> $3,424.1`, `9 -> 10` fills), and *more* total cancels (`46,760 -> 46,943`).
  - **Action (`src/config.rs`)**: Formally marked `isolate_cooldown_cancel_churn` empirically falsified / deprecated-experimental; keep `false` default and retain field only for historical replay reproducibility.

### 3.2 Real Track 3 Bottleneck: `97.0%` of 1s Adverse Selection Is `cancel_pending`
From `s13_final_20261119/mm/review.json` (`monthly t50_iso0`, 178 maker fills / `$19,589.12` turnover):
- **`cancel_pending` fills (`128` fills, `$13,469.07`)**: `1s` markout = **`-$2.898`** (`-2.15 bp`), `30s` markout = **`-$2.715`** (`-2.02 bp`).
- **Clean non-cancel-pending fills (`50` fills, `$6,120.05`)**: `1s` markout = **`-$0.089`** (`-0.15 bp`), `30s` markout = **`+$0.703`** (`+1.15 bp` positive!).
- **Taiwan `t50_iso0`**: Clean fills (`9` fills, `$1,783`) have **`+3.40 bp` (`+$0.607`)** 30s markout vs **`-5.09 bp` (`-$0.793`)** 1s markout on `cancel_pending` fills (`13` fills, `$1,557`).
- **Recommendation for Codex**: Rather than post-hoc cancel deferral (`isolate_cooldown_cancel_churn`) or flat size cuts (`mm_local_size`, which leaves quotes at the same tight cancel-race distance), test **proactive pre-cancel directional widening** already wired in `inventory_neutral_mm.rs` (`local_toxicity.ofi_max_widen_bps` + `cancel_speed_bump_ms`) on `t50_iso0`!

---

## 4. `S14-3`: Peer Review of Codex's `12:33` / `12:42 CST` Deliveries

- **`zero_table/review.patch` (`scripts/quant_lakehouse.py:1811-1819`, `scripts/test_quant_lakehouse.py`)**: Verified clean and complete. `digest_unchanged` now checks `compressed_sha256` and `compressed_bytes` for every Silver table including `rows == 0` before creating output directories; audited all other `rows == 0` / `compressed_sha256` call sites in `scripts/` and confirmed no remaining manifest-verification bypasses.
- **`cold_resume/review.md`**: Re-verifying full archive length, SHA-256, and gzip EOF before certifying interrupted cold-archive receipts closes a real crash-window gap; 9/9 existing `W06–W08` receipts remain consistent.
- **`residual_rank` (Track 1)**: BTC-factor residual reversal improved `230` common 5m windows (`+$4.827 -> +$9.922`) and shifted `Nov 18` short leg `XRP -> SOL` (`-$0.351 -> -$0.290`), but block-bootstrap 95% CI (`[-$0.0179, +$0.0602]/window`) still crosses zero, worst-price stress is `-$57.070`, and both settled native reversal windows remain negative (`Nov 4 -$1.058`, `Nov 18 -$0.290`). Agree with keeping `cross_sectional.rs` frozen.
- **`mm_local_size` (`57` replays)**: Confirms shrinking size under local 5m range (`$15.24 -> $197.35/M`) or constant halving (`$228.38/M`) degrades monthly stress cash (`-$0.298 -> -$3.270 / -$3.623`) because it alters inventory trajectory without addressing `cancel_pending` entry distance.

---

## 5. Mac Verification Gates (`2026-11-19 13:01 CST`, Post-Final-Edit)

| Gate | Command | Result |
| --- | --- | --- |
| Formatting | `make fmt-check` | PASS (`rustfmt` on modified files only; `gofmt` clean) |
| Static checks | `make check` | PASS (`clippy` + `go vet`) |
| Clippy (default) | `cargo clippy --locked --all-targets -- -D warnings` | PASS (0 warnings) |
| Clippy (`research-replay`) | `cargo clippy --locked --features research-replay --all-targets -- -D warnings` | PASS (0 warnings) |
| Rust tests (default) | `cargo test --locked` | PASS (`616+5+8`, `6 ignored`) |
| Rust tests (`research-replay`) | `make test` & `cargo test --locked --features research-replay` | PASS (`675+5+8`, `6 ignored`; `+3` new Sprint 14 tests) |
| Go tests (uncached race) | `cd feeder && go test -race -count=1 ./...` | PASS (5/5 packages) |
| Research scripts | `make test` (`scripts/test_*.py`) | PASS (`200` tests) |
| Monitor tests | `make test-python` | PASS (`42` tests) |
| Release build | `make build` | PASS (Rust release + Go feeder) |
| Config check | `make trade MODE=check CONFIG=config.example.toml` | PASS |
| Dashboard | `dashboard/` untouched in Sprint 14 | Not re-run |

---

## 6. Post-Handoff Codex Peer-Review Addendum (`2026-11-19 13:25 CST`)

- **Risk-Clock Refinement (`maker_first.rs` SHA `7125a6ff3a1e`)**: Codex caught and independently fixed a subtle diagnostic clock truncation on forced-drain exits (separating `forced_drain_triggered_ms`, `risk_resolved_ms`, `fully_hedged_ms` restricted strictly to `"hedged"`, and `censored_unhedged_ms` for `"open_at_cutoff"`, plus distinguishing `"hedged_within_deadband"`), documented in [PERP_RELATIVE_VALUE.md](PERP_RELATIVE_VALUE.md) and verified across Mac + isolated Linux.
- **240-Replay Historical Validation**: Across 240 historical Tokyo arb replays (`48` silent byte-identical, `192` diagnostic cash/fill identical), `hedge_ioc_policy` wide limits eliminated all **3 `W01` price misses (`3 -> 0`)** and cut original-latency `W01` entry unhedged time from **`3,697 ms -> 1,508 ms` (`-59.2%`)**, whereas shortening `retry_ms` alone had no effect. Best low-latency historical stress cash remains `-$0.15063` (`19` empty, `2` active ETH roundtrips, `3` unsettled).
