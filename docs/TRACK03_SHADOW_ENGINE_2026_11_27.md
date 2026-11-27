# Track 03 (`03_incentive_turnover`) Decoupled Shadow Engine & 3-Track Shadow Parity (`2026-11-27`)

> **Operational Mode**: Strictly **`HOLD`** · Read-only shadow / replay / SHM observer only. Zero `crate::exchanges::*` imports in `src/strategy/`, zero order placement/cancellation/signing paths, and zero cross-subsidy between tracks (`AGENTS.md`).

## 1. Architecture Summary

All three decoupled strategy tracks share the read-only shadow harness in [`src/strategy/track_shadow.rs`](../src/strategy/track_shadow.rs) and the unified CLI runner [`src/bin/track01_track02_shadow.rs`](../src/bin/track01_track02_shadow.rs), each with an independent [`DailyLossGuard`](../src/daily_loss.rs), configuration, runtime state, and report:

- **Track 01 (`01_quant_profit`) — `Track01ShadowSession`**:
  - Wraps [`LeadLagTaker`](../src/strategy/lead_lag_taker.rs) with optional `confirm_ref_exchange_id` wired to `LeadLagTaker::observe_with_confirm` via `on_bbo_pair_with_confirm` (preserving `on_bbo_pair` backward compatibility).
  - Tracks per-venue `VenueFeedTracker` continuity (including `confirm_ref_feed` when configured), L1 depth caps, and progressive `DailyLossGuard` scaling while preserving exits under halt.
- **Track 02 (`02_hedge_arbitrage`) — `Track02ShadowSession`**:
  - Wraps [`MakerHedgeQuoter`](../src/strategy/basis_maker_hedge.rs) with `on_pair_books_with_l1` wired to `MakerHedgeQuoter::step_with_l1` (passing SHM / replay hedge L1 sizes when configured, while `on_pair_books` remains backward compatible and fails closed if `min_hedge_l1_notional_usd > 0` without L1 sizes).
  - Preserves fail-closed `RefCancelShieldConfig`, deduplicated `on_reference_touch`, `ShieldedPairEconomics`, and `unhedged_stale_halts`.
- **Track 03 (`03_incentive_turnover`) — `Track03ShadowSession`**:
  - Wraps [`QueuePriorityMmEngine`](../src/strategy/queue_priority_mm.rs) (`G4` quiet-regime gates `quiet_regime_min_spread_to_vol` and `quiet_regime_max_entry_vol_bps`, `as_inventory_skew_gamma`, `vol_spread_mult`, `directional_markout_skew_mult`, sweep-print cooldown, and `max_inventory_hold_ms` reduce-only flatten).
  - **Optional UTC Quiet-Session Filter**: `UtcSession` (`overnight_asia`, `us_extended`, `us_rth`, `weekend`) via `classify_utc_session(now_ms)` consistent with [`research_track03_multivenue_turnover.py`](../scripts/research_track03_multivenue_turnover.py). During gated sessions or `DailyLossGuard` halt, opening quotes are suppressed and resting opening quotes are cancelled while pure `reduce_only` exits stay active.
  - **FIFO / Queue / Print Accounting & Cost Decomposition**:
    - Default `rebate_bps = 0.0` (never assumes rebates without verified account evidence).
    - Strict FIFO lot matching (`spread_capture_bps + holding_drift_bps + passive_exit_pnl_bps + taker_exit_slippage_bps == gross_ex_fee_bps`), `1s` notional-weighted maker markout (`markout_1s_bps`), `fees_usd`, and double-sided turnover cost `double_sided_usd_per_m` consistent with [`eval_volume_farming_cost.py`](../scripts/eval_volume_farming_cost.py).
  - **Feed Continuity & Zero-Alloc Hot Path**:
    - Per-venue `VenueFeedTracker` (`Target` + `Ref` slots) rejects stale, future, clock-regressed, or crossed books without advancing MTM or polluting engine state, and deduplicates unchanged SHM reference snapshots.
    - Reuses pre-allocated `scratch_book`, `scratch_refs`, and `scratch_resting` buffers on target/ref BBO ticks.

## 2. Isolation & Safety Invariants

1. **Zero Exchange Execution Coupling**: `src/strategy/track_shadow.rs` and `src/bin/track01_track02_shadow.rs` contain zero `crate::exchanges::*` imports and zero order signing/submission/cancellation clients.
2. **Non-Cross-Subsidy**: `Track01ShadowSession`, `Track02ShadowSession`, and `Track03ShadowSession` each own isolated `DailyLossGuard` instances and capital bases; tripping Track 03's daily loss brake halts Track 03 openings while preserving Track 03 reducing exits and leaving Track 01 / Track 02 unaffected.
3. **Fail-Closed Feed Integrity**: Crossed, stale, or clock-regressed target books clear Track 03 resting orders fail-closed without overwriting `last_valid_target_touch` MTM.
4. **Fee Provenance (`OPT-01`)**: [`examples/track03_queue_priority_mm_shadow.toml`](../examples/track03_queue_priority_mm_shadow.toml) freezes `lighter AAPL G4_quiet_sessions_only` with `rebate_bps = 0.0` and explicitly documents `account_fee_status = "public_tier0_unverified"`, requiring `OPT-01` account fee verification before any live promotion.

## 3. CLI Usage (`track01_track02_shadow`)

```bash
# Validate frozen Track 01 / 02 / 03 profiles offline
cargo run --locked --bin track01_track02_shadow -- --track 1 --config examples/track01_lead_lag_shadow.toml --check
cargo run --locked --bin track01_track02_shadow -- --track 2 --config examples/track02_shielded_maker_hedge_shadow.toml --check
cargo run --locked --bin track01_track02_shadow -- --track 3 --config examples/track03_queue_priority_mm_shadow.toml --check

# Deterministic offline JSONL replay (Track 03 accepts track3_ref, track3_bbo [optional L2 bids/asks], track3_print)
cargo run --locked --bin track01_track02_shadow -- --track 3 --config examples/track03_queue_priority_mm_shadow.toml --replay <events.jsonl>

# Read-only shared-memory shadow observer (BBO-cross diagnostic mode when no trade prints exist in SHM)
cargo run --locked --bin track01_track02_shadow -- --track 3 --config examples/track03_queue_priority_mm_shadow.toml --shm-path /dev/shm/aleph-matrix --duration-secs 60
```

## 4. Verified Local Checks

| Check | Outcome |
|---|---|
| `make fmt-check` | Pass (`cargo fmt --check`, `gofmt`) |
| `make check` | Pass (`cargo clippy --locked --features research-replay --lib --bins -- -D warnings`, `go vet ./...`) |
| `cargo test --locked --features research-replay --lib --bins` | Pass (`1131 passed; 0 failed; 6 ignored` lib; bins `21 + 9 + 11 + 0 + 3 + 6 + 11` passed, including `6/6` in `track01_track02_shadow`) |
| `--track {1,2,3} --config ... --check` | Pass (`"Configuration OK"` JSON on all 3 profiles) |
| `git diff --check` | Pass |
