# Sprint 15: Single-Level Micro-Refresh Deferral Fix, Entry-Only Hedge IOC Policy & Reserve/Cancel-Pending Audit (2026-11-19)

> **Status**: Offline engineering & Codex peer review complete; all three live tracks remain **`HOLD`**. Zero live trading, cancellations, deployments, restarts, or remote mutations.

---

## 1. Executive Summary

Following Codex's `2026-11-19 13:31–15:24 CST` independent studies (`maker_grid_v2`, `hedge_transfer`, `mm_reserve`, `mm_wire`, `arb_admission`, `offmonth_transfer`, and W08 RWA `SNDK` zero-size isolation), Sprint 15 resolves two concrete code bugs uncovered directly by Codex's ablations and completes the requested cancel-cause & exposure-time cross-review:

1. **`S15-1` (Track 3 — Fix `should_defer_micro_refresh` Dead-Code Bug for `grid_levels == 1`)**:
   - Located why Codex's `maker_grid_v2` `one` arm (`grid_levels = 1`, `$500` single level) collapsed from `21 -> 2` fills on Taiwan W2, `2 -> 0` fills on independent July, and `-$0.298 -> -$4.381` on monthly: `MICRO_REFRESH_MIN_RESTING_ORDERS` (`= 3`) in `src/strategy/inventory_neutral_mm/execution.rs:23,519` is mathematically unreachable when `grid_levels == 1` (max resting orders = `2`), disabling `4s` `MICRO_REFRESH_COOLDOWN` FIFO queue protection 100% of the time on single-level grids!
   - Capped `min_resting_orders` at `MICRO_REFRESH_MIN_RESTING_ORDERS.min(2 * grid_levels.max(1))` AND enforced `!single_level_place_only` (`!(grid_levels <= 1 && total_cancels == 0)`) so single-level grids defer 1-sided micro-refresh cancel churn only when both sides are resting (`>= 2`) and NEVER defer a pure missing-side placement (even if the opposite side transiently holds 2 orders), while `grid_levels >= 2` stays **100% byte-identical (`>= 3`)**.
2. **`S15-2` (Track 2 — `HedgeIocPolicy::entry_only` Decoupling Entry Hedge vs Closing/Unwind IOCs)**:
   - Codex's `240`-replay `hedge_transfer` + `144`-replay `arb_admission` proved that widening IOC limits cut `W01` entry unhedged exposure from `3,697 -> 1,508 ms` (`3 -> 0` price misses, `+$0.06271`), **yet hurt `W05` by `-$0.02124` despite 100% identical entry (`803 ms`, `1` IOC, `0` misses)** and worsened `W06 BTC role 1` (`2.5 bp`) by `-$0.01086` (`-$0.07038 -> -$0.08124`) because `HedgeIocPolicy` also widened closing/forced-unwind IOCs!
   - Added `#[serde(default)] pub entry_only: bool` (default `false` -> byte-identical with Sprint 14 configs) to `HedgeIocPolicy` in `src/strategy/relative_value/paper/maker_first.rs`, scoping `ioc_rules(is_entry_hedge)` and `hedge_retry_ms(is_entry_hedge)` strictly to opening entry hedges when `entry_only == true`.
3. **`S15-3` (Cross-Review of `mm_reserve`, `mm_wire`, `W01` Remaining `1,508 ms`, `W06 BTC`, & Track 1/Ops)**:
   - Quantified why doubling OFI (`3 -> 6 bp`) in `mm_reserve` worsened stress cash (`-$0.298 -> -$4.629`) even while improving `cancel_pending` 1s markout (`-2.15 -> -1.38 bp`): noisy 500 ms OFI jitter raised total cancels (`499,956 -> 503,221`, `+3,265`), raised `cancel_pending` fill share from **`71.9%` (`128/178`) to `83.6%` (`112/134`)**, and wiped out **`-65%` of profitable clean resting turnover (`$6,120 -> $2,151`, `50 -> 22` fills)**!

---

## 2. `S15-1`: Single-Level Micro-Refresh Deferral Bug (`src/strategy/inventory_neutral_mm/execution.rs`)

### 2.1 Root Cause in `maker_grid_v2`
In `maker_grid_v2` (`SPRINT14_INDEPENDENT_TIMING_AND_HEDGE_EXECUTION_2026_11_19.md`), Codex tested equal `$500`/side nominal budgets across `grid_levels = 1, 2, 3` (`one`, `two`, `three`) vs `original` (`grid_levels = 2`, `$850`/side):
- `original` (`grid_levels = 2`, max resting = `4 >= 3`): Taiwan W2 `21` fills (`$2,998`, `+$1.6915`), monthly `178` fills (`-$0.298`).
- `two` (`grid_levels = 2`, max resting = `4 >= 3`): Taiwan W2 `21` fills (`$2,601`, `+$2.7895`).
- `one` (`grid_levels = 1`, max resting = `2 < 3`): Taiwan W2 **`2` fills (`$177`, `+$0.2009`)**, Taiwan W1 **`0` fills**, independent July **`0` fills**, monthly **`129` fills (`-$4.381`)**!

Because `should_defer_micro_refresh` checked `resting_orders >= MICRO_REFRESH_MIN_RESTING_ORDERS` (`3`), `grid_levels == 1` was **never** protected by `MICRO_REFRESH_COOLDOWN` (`4s`), continuously resetting FIFO queue priority on 1-tick micro-moves.

### 2.2 Fix & Counterexample Invariant
- `min_resting_orders = MICRO_REFRESH_MIN_RESTING_ORDERS.min(usize::from(ctx.config.grid_levels.max(1)).saturating_mul(2))`
- `single_level_place_only = ctx.config.grid_levels <= 1 && total_cancels == 0`
- Invariant:
  - `grid_levels >= 2`: `min_resting_orders == 3` and `single_level_place_only == false` -> **100% byte-identical** to all frozen configs.
  - `grid_levels == 1`: defers 1-sided micro-refresh churn (`total_cancels == 1`) only when full 2-sided book is resting (`resting_orders >= 2`); never defers when a side is missing (`resting_orders <= 1`) or on any pure placement (`total_cancels == 0`, even under a transient 2-orders-on-one-side state).
- Added unit test `defer_micro_refresh_activates_for_full_two_sided_single_level_grid_only`.
- **Recommended Codex ablation**: re-run ONLY the 19-wallet `one` arm of `maker_grid_v2` against `original`/`two`/`three` (which remain byte-identical) to measure single-level performance with apples-to-apples FIFO deferral restored.

---

## 3. `S15-2`: Entry-Only Hedge IOC Policy (`src/strategy/relative_value/paper/maker_first.rs`)

### 3.1 Root Cause in `hedge_transfer` (`W01`/`W05`) & `arb_admission` (`W06 BTC`)
- **`W01 ETH` remaining `1,508 ms` decomposition (`hedge_transfer/review.json`)**:
  - Wide limit (`buffer10`) eliminated all `3` price misses (`ioc_submitted = 1`, `ioc_fills = 1`, `p_miss = 0`, `d_miss = 0`), cutting unhedged time from `3,697 -> 1,508 ms` and improving `W01` by **`+$0.06271`**.
  - The remaining `1,508 ms` splits into `notice + submit = 201 ms` (invariant across `10/100 ms` and `1/0 ms` clocks) + `1,307 ms` on a single filled IOC (`310 ms` venue taker delay + `~997 ms` Core ETH book inter-frame gap after `arrival_ms`, which is tape-intrinsic rather than engine retry overhead).
- **`W05 ETH` & `W06 BTC` exit-widening penalty**:
  - `W05 ETH` (`baseline`) had **identical entry** under `diagnostics` vs `buffer10` (`803 ms`, `1` IOC, `0` misses), yet `buffer10` lost **`-$0.02124`** (`-$0.09581 -> -$0.11706`) solely because widening also applied to the closing/unwind IOC!
  - `W06 BTC role 1` (`2.5 bp` floor) similarly saw `combined` widen its forced-close path, worsening total cash `-$0.07038 -> -$0.08124` (`-$0.01086`, `mid_drift -$0.04618 -> -$0.06373`).

### 3.2 Fix & Recommended Codex Ablation
- Added `HedgeIocPolicy::entry_only: bool` (`#[serde(default)]`, default `false`).
- Scoped `ioc_rules(is_entry_hedge)` and `hedge_retry_ms(is_entry_hedge)` so `entry_only: true` widens ONLY opening entry hedges (`!unwind && !closing_hedge`) while keeping closing hedges and forced unwinds (`paired_drain_blocked`, `sub_min_top_up`) on conservative `pair` bounds.
- Added unit/replay test `hedge_ioc_policy_entry_only_widens_entry_without_widening_unwind_ioc`.
- **Recommended Codex ablation**: test `buffer10` with `entry_only: true` on `W01–W06` ETH/BTC (`1.5 bp` & `2.5 bp`) to verify whether `W01` retains `+$0.06271` while `W05` and `W06` eliminate the `-$0.02124` / `-$0.01086` exit-widening penalty.

---

## 4. `S15-3`: Cross-Review of Codex `14:08–15:24 CST` Studies

| Study | Key Finding & Verification |
| --- | --- |
| `mm_reserve` (`171` runs) | Doubling `OFI` (`3 -> 6 bp`, `vol_k = 0.4`) cut `cancel_pending` 1s markout (`-2.15 -> -1.38 bp`), **but** 500 ms OFI jitter increased cancels (`499,956 -> 503,221`, `+3,265`), pushed `cancel_pending` fill share from **`71.9% -> 83.6%` (`112/134`)**, and destroyed **`-65%` of clean profitable resting turnover (`$6,120 -> $2,151`, `50 -> 22` fills)**, explaining the `-$0.298 -> -$4.629` stress cash drop. Confirms reactive short-half-life OFI widening amplifies cancel churn rather than avoiding it. |
| `mm_wire` (`114` runs) | Slowing ONLY cancel wire latency (`10 -> 250 ms`) inflated maker fills (`178 -> 272`, `+$5,624`) and caused `-$4.9839` extra loss (`-$0.2984 -> -$5.2823`), confirming physical cancel speed dominates quote-reserve padding (`550 ms` reserve still worsened `base10` `-$0.298 -> -$3.382` by cutting clean fills). |
| Track 1 (`offmonth_transfer` & July 17–27) | Non-month-start `13:00 UTC` 6h momentum is `4W/4L` (`+$0.8415`), reversal `4W/4L` (`-$1.3708`); July 17–27 added 5 settled days (`4W/1L +$0.6734`, endpoints only). Month-start `7/7` does not generalize; maintain `cross_sectional.rs` freeze. |
| Ops (`W08 RWA` `SNDK` zero-size & Tokyo eviction) | Agree with isolating RH market 32 (`SNDK`) `size == 0, price > 0` strictly on the public partitioner while preserving strict catalog mode and zero-price rejection (`11,219 / 11,270` kept). Confirmed Tokyo auto-evicted W08 RWA (`6.83 GB`) under verified upload receipts (`21.98 GiB` free at `15:13 CST`). |

---

## 5. Mac Verification Gates (`2026-11-19 15:30 CST`, Post-Final-Edit)

| Gate | Command | Result |
| --- | --- | --- |
| Formatting | `make fmt-check` | PASS (`rustfmt` on modified files only; `gofmt` clean) |
| Static checks | `make check` | PASS (`clippy` + `go vet`) |
| Clippy (default) | `cargo clippy --locked --all-targets -- -D warnings` | PASS (0 warnings) |
| Clippy (`research-replay`) | `cargo clippy --locked --features research-replay --all-targets -- -D warnings` | PASS (0 warnings) |
| Rust tests (default) | `cargo test --locked --quiet` | PASS (`618+5+8`, `6 ignored`; `+2` new Sprint 15 tests) |
| Rust tests (`research-replay`) | `make test` & `cargo test --locked --features research-replay --quiet` | PASS (`677+5+8`, `6 ignored`; `+2` new Sprint 15 tests) |
| Go tests (uncached race) | `cd feeder && go test -race -count=1 ./...` | PASS (5/5 packages) |
| Research scripts | `make test` (`scripts/test_*.py`) | PASS (`200` tests) |
| Monitor tests | `make test-python` | PASS (`42` tests) |
| Release build | `make build` | PASS (Rust release + Go feeder) |
| Config check | `make trade MODE=check CONFIG=config.example.toml` | PASS |
| Dashboard | `dashboard/` untouched in Sprint 15 | Not re-run |

---

## 6. Post-Review Addendum (2026-11-19 17:40 CST — Codex `15:36–17:34 CST` Peer Review)

Per Codex's independent verification (`S16_GEMINI_134_NATIVE_CONTROLS_2026_11_19.md`, `15:36`, `15:54`, `16:13`, and `17:06–17:34 CST`), three clarifications and one empirical retraction are recorded here so this report does not overstate Sprint 15 conclusions:

1. **Empirical Withdrawal of the `S15-1` Single-Level FIFO Causal Hypothesis**:
   - Restoring `MICRO_REFRESH_COOLDOWN` on `grid_levels == 1` (`S15-1`) did **not** explain or fix single-level underperformance in Codex's 19-wallet rerun (`S16_GEMINI_134_NATIVE_CONTROLS_2026_11_19.md`): Taiwan W2 remained at `2` fills (`$177.15`), independent July remained `0` fills, monthly fills fell `129 -> 115` (`$15,338 -> $15,647`), cancel requests edged `267,461 -> 267,488`, and monthly stress cash worsened `-$4.3805 -> -$5.7743`.
   - The code invariant itself (`min_resting_orders` capped at `2 * grid_levels` with `!single_level_place_only`) remains valid and preserves `grid_levels >= 2` byte-identically, **but the claim that unreachable `MICRO_REFRESH_MIN_RESTING_ORDERS = 3` caused single-level collapse is withdrawn**. Codex's `17:06–17:34 CST` decomposition shows single-level already loses `-$4.03954` *before fees* (vs `+$1.42257` *before fees* on 2-level), and `92.07%` / `92.70%` of taker volume (`$9,069.36` / `$9,964.04`) originates from `position_timeout_secs` inventory timeouts.
2. **`S15-2` `entry_only` Retry-Clock Boundary (Resolved in Sprint 16)**:
   - As shipped in Sprint 15, `HedgeIocPolicy::entry_only` isolated `ioc_rules` and `hedge_retry_ms` by phase, **but `submit_ioc` still shared a single `next_ioc_ms` pacing timestamp across entry hedges and closing/forced-drain IOCs**. Codex's `15:36 CST` counterexample (`peer_entry_drain_test.rs.txt`) showed that a long entry retry (`2000 ms`) could delay a subsequent forced-drain IOC from `1310 ms` to `2610 ms`.
   - Therefore, Sprint 15 `entry_only` must **not** be described as fully clock-isolated across phases. Full phase-clock separation (`next_entry_ioc_ms` vs `next_ioc_ms`) was implemented and cross-platform verified in [Sprint 16](SPRINT16_ENTRY_RETRY_CLOCK_ISOLATION_AND_CODEX_PEER_REVIEW_2026_11_19.md) (`maker_first.rs` SHA-256 `251341aa47e6...`).
3. **`S15-3` W08 RWA `SNDK` Zero-Size Scope Precision**:
   - In `scripts/quant_lakehouse.py`, the `ZeroSizeTradeScope` isolation for RH market 32 (`SNDK`) requires `px_dec > 0`, `event_type == "update/trade"`, and an exact audited `observations` count on the partial projection path (`--allow-partial-projection`); **positive-quantity, zero-price transfers keep their original flag untouched** (`allow_zero_price` semantics unchanged, strict default catalog preserved).
