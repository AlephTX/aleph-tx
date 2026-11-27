# Core Engineering Upgrades: Deterministic Event Defense FSM (`OPT-07`), Transaction-Weighted Batch Quota Arbiter (`OPT-05`), and Unified 3-Track Shadow Defense Integration (`2026-11-27`)

> **Operational Mode:** Strictly **`HOLD`** · Offline / read-only shadow, replay, and unit verification only. Zero `crate::exchanges::*` imports in `src/strategy/`, zero live order placement/cancellation/signing paths, and zero modifications to `inventory_neutral_mm.rs`, `feeder/shm/`, `Cargo.lock`, or `AGENT_HANDOFF.md`.

---

## 1. `OPT-07` Deterministic Event Defense FSM ([`src/strategy/defense_fsm.rs`](../src/strategy/defense_fsm.rs))

Exported via [`src/strategy/mod.rs`](../src/strategy/mod.rs) as `pub mod defense_fsm;`.

### 1.1 Architecture & Invariants

- **Zero-Allocation 5-State Controller (`DefenseState`):**
  - `DefenseState::{Normal, Caution, SideDefense { blocked_side }, ReduceOnly, Halt}` where `blocked_side: BlockedSide::{Buy, Sell, Both}` and all runtime types (`DefenseFsm`, `DefenseFsmConfig`, `DefenseTelemetrySnapshot`, `DefenseTransition`, `ResetBlockedReason`) are `Copy` with zero heap allocations.
- **Structured Telemetry Inputs (`DefenseTelemetrySnapshot`):**
  - `now_ms`, `feed_staleness_ms`, `feed_gap_ms`, `vol_shock_ratio`, `trailing_bid_markout_bps`, `trailing_ask_markout_bps`, `oracle_divergence_bps`, `daily_loss_utilization`, `budget_cooldown_active`, `budget_halted`, and `clock_regressed_externally`.
  - Non-finite (`NaN`/`Inf`) or negative unsigned-domain metrics fail closed to `Halt` (or `ReduceOnly` for non-finite markout).
- **Mandatory Monotonic Clock Guard:**
  - Backward timestamps (`snapshot.now_ms < last_now_ms`) are rejected without updating `last_now_ms`, increment `clock_regressions`, forbid de-escalation, and fail closed to `ReduceOnly` (or remain in `Halt`).
- **De-Escalation Hysteresis (`min_dwell_ms` + `recovery_hysteresis_ratio`):**
  - Escalation on deterioration is immediate and monotonic (`combine` unions blocked sides in `SideDefense` or takes the higher severity).
  - De-escalation requires both `now_ms - state_entered_at_ms >= min_dwell_ms` and recovery signals falling strictly below `recovery_hysteresis_ratio * threshold` (default `0.80`), preventing threshold-boundary flapping.
- **Non-Auto-Resetting `Halt`:**
  - `DefenseFsm::evaluate` never clears `DefenseState::Halt`, even after `min_dwell_ms` elapses and telemetry returns to healthy baselines.
  - Lifting `Halt` requires explicit `DefenseFsm::acknowledge_reconciled_reset(&snapshot)`, which still rejects backward clocks (`MonotonicClockViolation`), insufficient dwell (`DwellNotElapsed`), and weak recovery signals (`RecoverySignalsTooWeak`).
- **Compile-Time & Unit-Tested Exit/Cancel Exemption:**
  - `DefenseState::permits_intent(intent, side)` is a `const fn` verified by compile-time `const _: () = { ... };` assertions and unit tests proving `OrderIntentClass::{ReduceOnlyExit, EmergencyHedge, Cancel}` are **always** `true` across all `DefenseState` and `BlockedSide` variants, while `OrderIntentClass::RiskIncreasingEntry` is scaled by `caution_size_scale` in `Caution`, selectively blocked in `SideDefense`, and strictly blocked in `ReduceOnly` and `Halt`.

---

## 2. `OPT-05` Transaction-Weighted Batch Quota Arbiter ([`src/order_budget.rs`](../src/order_budget.rs))

- Added `BatchQuotaMode::{SingleUnit, PerTransaction}` and `BatchBudgetDecision` to [`src/order_budget.rs`](../src/order_budget.rs), alongside `VenueOrderBudget::with_batch_mode`, `batch_mode()`, `set_batch_mode()`, `admit_batch(now_ms, &[OrderIntentClass])`, `admit_batch_with_mode(...)`, and `record_batch_dispatch(...)`.
- **`PerTransaction` (`SendTxBatch` Semantics) & Anti-Starvation Ordering:**
  - Exempt items (`OrderIntentClass::{ReduceOnlyExit, EmergencyHedge, Cancel}`) in a batch are always admitted first and reserve 1 weight unit each before any `RiskIncreasingEntry` headroom is allocated.
  - Earlier `RiskIncreasingEntry` items in a mixed batch can never starve or block `ReduceOnlyExit`, `EmergencyHedge`, or `Cancel` items, nor push total dispatched batch weight past the soft/hard caps.
- **Telemetry Helpers:**
  - Added `VenueOrderBudget::remaining_entry_headroom(&mut self, now_ms)` (returns `0` when halted or in cooldown) and `VenueOrderBudget::utilization_pct(&mut self, now_ms)` (`window_weight / max_weight_per_window * 100.0`).

---

## 3. Unified 3-Track Shadow Defense Integration & Example Profiles

- **Shadow Session Wiring ([`src/strategy/track_shadow.rs`](../src/strategy/track_shadow.rs) & [`src/bin/track01_track02_shadow.rs`](../src/bin/track01_track02_shadow.rs)):**
  - Added `#[serde(default)] pub defense: Option<DefenseFsmConfig>` (default `None` for 100% backward compatibility) to `Track01ShadowConfig`, `Track02ShadowConfig`, and `Track03ShadowConfig`, validated at startup and surfaced as `defense_enabled` in `--check` JSON output.
  - Added `defense_state_transitions` and `defense_blocked_entries` (plus optional `defense_state` when configured) to `Track01ShadowReport`, `Track02ShadowReport`, and `Track03ShadowReport`.
  - Track 01 throttles `RiskIncreasingEntry` notional in `Caution` and blocks entries in `ReduceOnly`/`Halt` while keeping `LeadLagAction::Exit` unblocked.
  - Track 02 blocks opening maker legs in `ReduceOnly`/`Halt` while preserving inventory-reducing `MakerHedgeQuotes` legs.
  - Track 03 evaluates `DefenseFsm` immediately after `QueuePriorityMmEngine::evaluate` so newly matured trailing markout and volatility shock metrics take effect on the same tick, enforces `SideDefense`/`Caution`/`ReduceOnly`/`Halt` on opening `PlaceAlo`/`Keep` actions, and preserves `reduce_only` exits and `ReduceOnlyIoc`.
- **Lighter Standard vs. Premium Fee & Latency Profiles:**
  - Created [`examples/track03_lighter_rh_standard_shadow.toml`](../examples/track03_lighter_rh_standard_shadow.toml) targeting `lighter_robinhood` (`USDG`, `target_exchange_id = 8`, `account_fee_status = "verified_account_tier"`, `0.0 / 0.0 bps`, `rebate_bps = 0.0`, `submit_rtt_ms = 200.0`) with `[defense]` enabled.
  - Updated [`examples/track03_queue_priority_mm_shadow.toml`](../examples/track03_queue_priority_mm_shadow.toml) comments documenting `OPT-01` verified fee divergence: `lighter` (`USDC`, `user_tier = premium`, `0.40 / 2.80 bps`, low latency ~35 ms -> passive RWA `NO-GO`) vs. Lighter Standard (`0.00 / 0.00 bps` post-switch or `lighter_robinhood` `USDG` verified `0/0`, carrying ~200–310 ms speed-bump maker latency sensitivity).

---

## 4. Verification Summary

All checks ran inside this worktree after the final code edit:

| Check | Result |
| --- | --- |
| `make fmt-check` | Pass (`cargo fmt --check`, `gofmt -l .` clean) |
| `make check` | Pass (`cargo clippy --locked --features research-replay --lib --bins -- -D warnings`, `go vet ./...`) |
| `cargo test --locked --features research-replay --lib --bins` | Pass (`lib`: `1142 passed; 0 failed; 6 ignored`; `bins`: `21 + 9 + 11 + 0 + 3 + 7 + 11` passed, `0 failed`) |
| `track01_track02_shadow --track 1 --config examples/track01_lead_lag_shadow.toml --check` | Pass (`"Configuration OK"`, `arms=2`, `defense_enabled=false`) |
| `track01_track02_shadow --track 2 --config examples/track02_shielded_maker_hedge_shadow.toml --check` | Pass (`"Configuration OK"`, `pairs=3`, `defense_enabled=false`) |
| `track01_track02_shadow --track 3 --config examples/track03_queue_priority_mm_shadow.toml --check` | Pass (`"Configuration OK"`, `arms=1`, `defense_enabled=false`) |
| `track01_track02_shadow --track 3 --config examples/track03_lighter_rh_standard_shadow.toml --check` | Pass (`"Configuration OK"`, `arms=1`, `defense_enabled=true`) |
