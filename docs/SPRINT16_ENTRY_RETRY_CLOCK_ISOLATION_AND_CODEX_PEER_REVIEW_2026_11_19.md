# Sprint 16: Phase-Isolated Entry Retry Clock & Codex `15:36–17:34 CST` Peer Review (2026-11-19)

> **Status**: Offline engineering, cross-platform verification (Mac + Codex isolated Linux WSL), and peer review complete; Sprint 16 local edit lock released. All three live tracks remain **`HOLD`**. Zero live trading, cancellations, deployments, restarts, or remote mutations.

---

## 1. Executive Summary

Sprint 16 addresses the `entry_only` shared-clock counterexample identified by Codex at `15:36 CST` (`peer_entry_drain_test.rs.txt`), updates [Sprint 15](SPRINT15_SINGLE_LEVEL_DEFER_BUG_ENTRY_ONLY_IOC_AND_MM_RESERVE_REVIEW_2026_11_19.md) (§6 Addendum) to withdraw the falsified single-level FIFO causal claim and clarify W08 `SNDK` zero-price transfer preservation, and completes peer review across Codex's `15:36–17:34 CST` outputs:

1. **`S16-1` (Phase-Isolated `next_entry_ioc_ms` vs `next_ioc_ms` in [maker_first.rs](../src/strategy/relative_value/paper/maker_first.rs), SHA-256 `251341aa47e679ece87bb97b118678651fcb95173b4328fcbd8114bf1e699291`)**:
   - Separated entry-hedge retry pacing (`next_entry_ioc_ms`) from closing/forced-drain IOC pacing (`next_ioc_ms`) when `HedgeIocPolicy::entry_only == true`, while keeping `entry_only == false` **100% byte-identical** and preserving same-leg in-flight (`self.ioc.is_some()`) protection and conservative `reprice_ms` spacing on closing/drain IOCs.
   - Verified on both Mac (`task-4635`, all gates PASS) and Codex's isolated Linux WSL rerun (`16:51 CST`): the `100/2000 ms` entry-retry forced-drain case resolves at `1,310 ms` on both arms (fixing the `2,610 ms` delay), while same-phase entry throttling and reprice spacing remain enforced.
2. **`S16-2` (Peer Review of Codex `15:36–17:34 CST` Studies & `quant_lakehouse.py`)**:
   - **Lakehouse (`scripts/quant_lakehouse.py`)**: `ZeroSizeTradeScope` patch is sound and fail-closed (strict default preserved, `px_dec > 0`, `update/trade`, exact audited `observations`, `seen[tid]` identity conflict detection, positive-quantity zero-price transfers untouched). One minor hygiene observation noted at `L1582` (`zero_size_observations != zero_size_trade_scope.observations` raises before `shutil.rmtree(output_dir)`, unlike `L1579`; still fail-closed because `manifest.json` is not written).
   - **Track 3 (MM)**: Single-level FIFO hypothesis is empirically falsified (`129 -> 115` fills, `-$4.3805 -> -$5.7743`); the decisive decomposition from `17:06–17:34 CST` is that original 2-level MM is **`+$1.42257` positive before fees** (vs `-$4.03954` on 1-level), losing `-$1.72101` in friction (`-$0.73599` overhead + `-$0.98503` taker stress), with **100% of intraday taker IOCs (`46` fills / `$9,069.36` = `92.07%` of all taker volume on 2-level) triggered by `position_timeout_secs` inventory timeout**.
   - **Track 2 (Arb)**: Across Codex's `192` exit-factorial runs and `96` inventory-interval probes, combining `entry_only` + removing extra exit wait is non-additive (`-$0.17030 / -$0.18691` vs `-$0.16949 / -$0.18610`), and pre-first-hedge naked drift (`-$0.16295` over `3,703 ms`) dominates the 3 active `2.5 bp` roundtrips (`-$0.14477` total mid-drift); even mechanically adding back all `+$0.16295` pre-hedge drift leaves `-$0.02315` stress net.
   - **Track 1 (4-Asset Quant)**: Across `168` pair-selection controls over 14 days, 6h reversal earned `+$1.66571` (`9W/5L`, rank `4/24`), **but Nov 14 alone contributed `+$1.67697`** — excluding Nov 14 drops reversal to `-$0.01126` (rank `12/24`). Keep `cross_sectional.rs` frozen.
   - **Ops (`W08` Final Signoff)**: Confirmed `W08` three-site SHA reconciliation (`16,294,410,023` bytes), crypto (`939,648`) and RWA (`1,244,656`) partial DuckDB counts, `11,219` `SNDK` records exact-matched with zero non-positive quantities, and `W08` recovery lock release (`W09` expected seal `~17:59:48–49 CST`, `19.99 GiB` free on Tokyo at `17:32`).

---

## 2. `S16-1`: Phase-Isolated Entry Retry Clock (`src/strategy/relative_value/paper/maker_first.rs`)

### 2.1 Root Cause (Codex `15:36 CST` Counterexample)
In Sprint 15 (`HedgeIocPolicy::entry_only = true`), `ioc_rules(is_entry_hedge)` and `hedge_retry_ms(is_entry_hedge)` were phase-scoped, but `submit_ioc` read and updated a single `self.next_ioc_ms` field for both entry hedges and closing/forced-drain IOCs. When an opening entry hedge IOC missed at `t = 300 ms` with `entry_only = true` and `hedge_retry_ms = 2000 ms`, `self.next_ioc_ms` was set to `2,300 ms` (plus arrival/retry alignment), delaying a forced-drain IOC triggered at `t = 1,000 ms` from `1,310 ms` to `2,610 ms`.

### 2.2 Fix & Safety Invariants
Added `next_entry_ioc_ms: u64` alongside `next_ioc_ms: u64`, plus two helper methods on `MakerFirstReplay`:
- `next_ioc_ready_ms(&self, is_entry_hedge: bool) -> u64`:
  - When `entry_only == true && is_entry_hedge`, enforces `self.next_entry_ioc_ms.max(self.next_ioc_ms)` (respecting both the entry retry timer and conservative `reprice_ms` spacing).
  - When `entry_only == true && !is_entry_hedge` (closing hedge or forced drain), enforces only `self.next_ioc_ms` (never blocked by an entry-phase `hedge_retry_ms`).
  - When `entry_only == false` (or `hedge_ioc_policy` is `None`), enforces `self.next_ioc_ms` (**100% byte-identical** to Sprint 14/15).
- `record_ioc_pacing(&mut self, now: u64, is_entry_hedge: bool)`:
  - When `entry_only == true && is_entry_hedge`: sets `self.next_entry_ioc_ms = now + hedge_retry_ms(true)` and `self.next_ioc_ms = self.next_ioc_ms.max(now + reprice_ms)` so any subsequent closing/drain IOC still respects conservative `reprice_ms` spacing without inheriting the long entry retry backoff.
  - Otherwise: sets `self.next_ioc_ms = now + hedge_retry_ms(is_entry_hedge)`.
- Same-leg in-flight protection (`self.ioc.is_some()`) remains unconditionally enforced before any new IOC submission.

### 2.3 Cross-Platform Verification
- Added regression test `entry_only_retry_clock_never_delays_forced_drain_and_preserves_reprice_spacing` covering:
  1. Codex's exact `100 ms` vs `2,000 ms` forced-drain counterexample (`forced_drain_triggered_ms == t0 + 1_000`, `risk_resolved_ms == t0 + 1_310` on both arms).
  2. Same-phase entry retry throttling (`entry_only = true`, `retry_ms = 2000`, `reprice_ms = 100` still throttles entry retries with `reprice_throttle_waits >= 2`, `fully_hedged_ms == t0 + 2_610`).
- **Mac (`task-4635`, `17:36 CST`)**: File SHA-256 `251341aa47e679ece87bb97b118678651fcb95173b4328fcbd8114bf1e699291`, all Rust/Go/Python/build/check gates PASS.
- **Linux WSL (Codex `16:51 CST`)**: Independent rerun against the identical SHA-256 prefix `251341aa47e6` confirmed `100/2000 ms` both resolve at `1,310 ms`, plus same-leg in-flight and reprice spacing tests PASS.

---

## 3. `S16-2`: Peer Review of Codex `15:36–17:34 CST` Outputs

| Area | Artifact / Code Reviewed | Peer-Review Verdict & Actionable Follow-Up |
| --- | --- | --- |
| Lakehouse `SNDK` Isolation | `scripts/quant_lakehouse.py` (`ZeroSizeTradeScope`) | **PASS (minor hygiene note)**. Default strict mode and positive-quantity zero-price transfer flags are completely preserved; `allow_zero_size` enforces `px_dec > 0`, `event_type == "update/trade"`, exact audited `observations` count, and keeps `seen[tid]` economic conflict checks. Minor hygiene note: on `L1582` (`zero_size_observations != zero_size_trade_scope.observations`), raising `ValueError` skips `shutil.rmtree(output_dir, ignore_errors=True)` unlike `L1579` — `manifest.json` is still omitted so `build-duckdb` remains fail-closed, but a fresh staging directory could leave orphan partial files on mismatch. |
| Track 3 (MM Single-Level & Taker Clock) | `S16_GEMINI_134_NATIVE_CONTROLS_2026_11_19.md`, `FOUR_ASSET_PAIR_SELECTION_AND_PRICE_COSTS_2026_11_19.md`, `NATIVE_INVENTORY_INTERVALS_AND_MAKER_IOC_2026_11_19.md` | **Confirmed & Sprint 15 Updated**. Single-level FIFO causal claim withdrawn in [Sprint 15 §6](SPRINT15_SINGLE_LEVEL_DEFER_BUG_ENTRY_ONLY_IOC_AND_MM_RESERVE_REVIEW_2026_11_19.md). Two-level monthly MM is **`+$1.42257` before fees** (vs `-$4.03954` on 1-level), and **100% of 46 intraday taker IOCs (`$9,069.36` = `92.07%` of taker volume)** originate from `position_timeout_secs` timeout (funding pre-drain is only `$633.96` = `6.4%`). Strongly endorse Codex's in-progress `65%`-age passive reduction + equal-risk reduce-only ablation on the 19 wallets; Gemini will next inspect `cancel_attribution.by_reason` (`499,956` cancels on monthly 2-level) to isolate what drives the `71.9%` `cancel_pending` fill share without destroying clean fills. |
| Track 2 (Arb Exit Factorial & Intervals) | `S16_TWO_WEEKS_AND_EXIT_FACTORIAL_2026_11_19.md`, `NATIVE_INVENTORY_INTERVALS_AND_MAKER_IOC_2026_11_19.md` | **Confirmed**. `entry_only` recovers `W05` (`-$0.11706 -> -$0.09581`) while keeping `W01` (`1,508 ms`, `0` misses), but `entry_only` + removing extra exit wait is non-additive (`-$0.17030 / -$0.18691` vs `-$0.16949 / -$0.18610`). In the 3 active `2.5 bp` roundtrips, pre-first-hedge naked drift (`-$0.16295` over `3,703 ms`) exceeds paired gain (`+$0.06720` over `9,905 ms`); even adding back all `+$0.16295` leaves `-$0.02315` stress net. No promotion; keep Track 2 on `HOLD`. |
| Track 1 (4-Asset 14-Day Pair Controls) | `FOUR_ASSET_PAIR_SELECTION_AND_PRICE_COSTS_2026_11_19.md` | **Confirmed**. 6h reversal `+1.66571` (`9W/5L`, rank `4/24`) relies on a single outlier day (`Nov 14: +$1.67697`); excluding Nov 14 yields `-$0.01126` (rank `12/24`). Maintain `cross_sectional.rs` freeze pending Codex's next 2-week frozen run (`Oct 19–25` & `Nov 2–8`). |
| Ops (`W08` Signoff & `W09` Tracking) | `s17_w08_readback_20261119/result.json`, `tokyo-1732.json` | **Confirmed**. `W08` signed off cleanly (`analysis.scope` + `analysis.done` at `17:32 CST`), three-site SHA matched (`16,294,410,023` bytes), recovery lock released; `W09` healthy (`19.99 GiB` free at `17:32 CST`, expected seal `~17:59:48–49 CST`). |

---

## 4. Mac Verification Gates (`2026-11-19 17:36 CST`, Post-Final-Edit, `task-4635`)

| Gate | Command | Result |
| --- | --- | --- |
| Source SHA-256 | `shasum -a 256 src/strategy/relative_value/paper/maker_first.rs` | `251341aa47e679ece87bb97b118678651fcb95173b4328fcbd8114bf1e699291` |
| Formatting | `make fmt-check` | PASS (`rustfmt --edition 2024` on modified file only; `gofmt` clean) |
| Static checks | `make check` | PASS (`clippy` + `go vet`) |
| Clippy (default) | `cargo clippy --locked --all-targets -- -D warnings` | PASS (0 warnings) |
| Clippy (`research-replay`) | `cargo clippy --locked --features research-replay --all-targets -- -D warnings` | PASS (0 warnings) |
| Rust tests (default) | `cargo test --locked --quiet` | PASS (`619+5+8`, `6 ignored`; `+1` new Sprint 16 test) |
| Rust tests (`research-replay`) | `make test` & `cargo test --locked --features research-replay --quiet` | PASS (`678+5+8`, `6 ignored`; `+1` new Sprint 16 test) |
| Go tests (uncached race) | `cd feeder && go test -race -count=1 ./...` | PASS (5/5 packages) |
| Research scripts | `make test` (`scripts/test_*.py`) | PASS (`202` tests, incl. Codex's 2 `ZeroSizeTradeScope` tests) |
| Monitor tests | `make test-python` | PASS (`42` tests) |
| Release build | `make build` | PASS (Rust release + Go feeder) |
| Config check | `make trade MODE=check CONFIG=config.example.toml` | PASS |
| Dashboard | `dashboard/` untouched in Sprint 16 | Not re-run |
