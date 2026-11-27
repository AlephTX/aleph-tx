# Arcus Turnover Live-Canary Readiness Runbook (2026-11-27)

**Operational Posture:** `OFFLINE & SHADOW ONLY — ZERO LIVE ORDERS AUTHORIZED` (`W01-W27` Verdict: **HOLD / NO-GO** vs `<= $2/M` target).
All canary configurations default to `dry_run = true` and `live_enabled = false`.
Live execution requires explicit operator sign-off on the checklist below, Linux x86-64
Poseidon2/Schnorr signer availability (`lighter-signer-linux-amd64.so`), and the
mandatory `--confirm-live-canary` CLI flag.

## 1. Architecture & Empirical Gate Summary

This infrastructure implements live-canary-ready offline/shadow tooling for two
Arcus-primary zero-points turnover methods (`OPT-01` verified fee tiers: Arcus RWA
`0.00 / 2.1375 bps` `USDC`, Lighter-RH Standard `0.00 / 0.00 bps` `USDG`):

1. **Method 1 (法1: Single-Venue Arcus RWA MM)** — [track03_arcus_rwa_shadow.toml](../examples/track03_arcus_rwa_shadow.toml)
   - Runs on the isolated Track 03 shadow runner ([track01_track02_shadow.rs](../src/bin/track01_track02_shadow.rs)).
   - Targets Arcus (`target_exchange_id = 7`, `USDC`, `0.00 bps` maker / `2.1375 bps` taker)
     with Lighter-RH (`8`) and TradeXYZ (`10`) as lead-lag references on the lowest-cost
     5-ticker RWA cohort (`AAPL, AMZN, META, MSFT, SPY`).
   - Enforces non-zero IOC exit latency (`ioc_exit_delay_ms = 150`), single-use L1
     depth conservation (`max_l1_depth_fraction = 0.50`), `max_inventory_hold_ms = 600000`,
     and the 5-state `[defense]` FSM.
   - **Empirical `W01-W27` Gate (`21` continuous 10h RWA windows):** **FAIL / HOLD** (`0/21`
     profitable windows; all-18 RWA `S4_hold600s_only` `+$114.15/M` double-sided /
     `+$228.31/M` single-sided; best ticker `AMZN` `+$45.29/M` DS / `+$90.58/M` SS). Keep in
     shadow-only `HOLD`.

2. **Method 2 (法2: Arcus Passive PostOnly Maker × Lighter-RH Standard Taker Hedge)**
   - Pure strategy state machine: [arcus_rh_turnover.rs](../src/strategy/arcus_rh_turnover.rs)
     (zero `use crate::exchanges::*` imports, zero hot-path heap allocations,
     `#[serde(default, deny_unknown_fields)]`).
   - Standalone runner: [arcus_rh_turnover.rs (bin)](../src/bin/arcus_rh_turnover.rs).
   - Default fail-closed canary profile: [arcus_rh_turnover_canary.toml](../examples/arcus_rh_turnover_canary.toml)
     (`dry_run = true`, `live_enabled = false`, `$100` clip, `$200` max inventory,
     `$25` daily loss limit on `$500` allocated capital, `hedge_delay_ms = 330`).
   - **Empirical `W01-W27` Gate (`21` RWA windows, `14` overlapping symbols):** **FAIL / HOLD**
     vs `<= $2/M` (`$240.67/M` Arcus single-sided / `$120.40/M` 2-leg at `e=0.0, fx=0 bp`,
     rising to `$540.15/M` Arcus / `$270.22/M` 2-leg under `3 bps` `USDG/USDC` FX drag;
     `strict_through 50%/100%` flips spread capture negative and increases cash burn
     `2.80x-5.85x`; best ticker `AMZN` `$124.00/M`). Live `--canary` remains blocked until
     strategy economics meet the target or an explicit experimental budget override is approved.

## 2. Hard Safety Invariants (Method 2)

1. **Worst-Case Exposure Reservation on Both Legs:**
   - Before placing any opening `PostOnly` quote on Arcus, `ArcusRhTurnoverEngine::on_book`
     computes worst-case bounds on **both** Arcus (confirmed position + active/cancel-pending
     order reservations) and Lighter-RH (confirmed position + pending/in-flight hedge
     exposures) and caps order size so neither leg can exceed `max_inventory_usd`.
2. **`on_passive_fill` Precedence Over Cancel-in-Flight:**
   - If a passive fill arrives while a resting quote is `CancelPending`, `on_passive_fill`
     takes strict precedence: it updates `arc_confirmed_pos`, consumes the slot reservation,
     and schedules the Lighter-RH IOC hedge (`+hedge_delay_ms`). Subsequent `on_cancel_ack`
     is a safe no-op for already-filled quantity.
3. **Monotonic Clock Guard on Every Callback:**
   - `on_book`, `on_passive_fill`, `on_cancel_ack`, `on_hedge_complete`, `on_usdg_usdc_peg`,
     and `reconcile_positions` all enforce `now_ms > 0 && now_ms >= last_callback_ms`,
     latching `HaltReason::ClockRegression` on violation.
4. **Mandatory Non-Zero Hedge Latency (`hedge_delay_ms = 330`):**
   - `ArcusRhTurnoverConfig::validate()` and `--check` reject `hedge_delay_ms == 0`.
   - Initial hedges and orphan retries are always scheduled at `now_ms + hedge_delay_ms`.
5. **Bounded Orphan Retry & Fail-Closed Circuit Breakers:**
   - Halts immediately and pulls quotes (`CancelAll`) on:
     - `ConsecutiveHedgeFailures` (`>= max_consecutive_hedge_failures`, default `3`),
     - `OrphanRetriesExhausted` (`> max_orphan_retries`, default `3`),
     - `ExcessOrphanNotional` (`> max_orphan_notional_usd`, default `$150`),
     - `UsdgUsdcDivergence` (`|peg - 1.0| * 10_000 > max_usdg_usdc_divergence_bps` or stale peg),
     - `DailyLossLimit` (`total_equity_pnl_usd < -max_daily_loss_usd` + persistent `DailyLossGuard`),
     - `CostPerMillionBreached` (realized cash cost `$/M > max_cost_per_million_usd` after
       `min_cost_breaker_volume_usd` warmup).
6. **Fail-Closed Live Canary Gates (`--canary`):**
   - Refuses unless `dry_run = false`, `live_enabled = true`, `require_arcus_cancel_lease = true`,
     explicit `--confirm-live-canary` flag is passed, `TurnoverJournal` passes clean-restart
     verification, both venues start flat with zero open orders, and `LighterTrading`
     can initialize the pinned Linux x86-64 signer on Robinhood Chain (`chain_id = 466324`).

## 3. Verification & Operator Commands

```bash
# 1. Method 1 (Track 03 Arcus RWA) config check
cargo run --bin track01_track02_shadow -- --track 3 --config examples/track03_arcus_rwa_shadow.toml --check

# 2. Method 2 offline config validation
cargo run --bin arcus_rh_turnover -- --check examples/arcus_rh_turnover_canary.toml

# 3. Method 2 deterministic P0 replay (offline JSONL tape)
cargo run --bin arcus_rh_turnover -- --replay examples/arcus_rh_turnover_canary.toml --tape <tape.jsonl> --strict-through-prob 0.0

# 4. Zero-credential read-only public BBO & RTT probe
cargo run --bin arcus_rh_turnover -- --probe-public --samples 5 --arcus-symbol NVDA-USD --rh-market-id 15

# 5. Zero-credential read-only shadow loop (REST/SHM)
cargo run --bin arcus_rh_turnover -- --shadow examples/arcus_rh_turnover_canary.toml --duration-secs 30

# 6. Read-only authenticated preflight (ZERO orders, ZERO cancels)
cargo run --bin arcus_rh_turnover -- --canary-preflight examples/arcus_rh_turnover_canary.toml --env <path/to/.env>
```

## 4. Explicit User-Approval Checklist Before Any Live `--canary`

- [ ] Empirical `W01-W27` benchmark and toxic-fill stress (`strict_through 50%`, `opposite_touch`) reviewed and approved (`$/M` meets target).
- [ ] `examples/arcus_rh_turnover_canary.toml` copied to a private runtime profile outside Git with `dry_run = false` and `live_enabled = true`.
- [ ] `ARCUS_*` (`USDC`) and `LIGHTER_*` (`chain_id = 466324`, `USDG`, Standard `0/0 bps` verified, non-157 API key index) credentials verified via `--canary-preflight` on the target Linux x86-64 host.
- [ ] Both venues confirmed flat (`position = 0`, `open_orders = 0`) and `TurnoverJournal` + `DailyLossGuard` state paths clean and writable (`0600`).
- [ ] Explicit human authorization obtained before executing `--canary ... --confirm-live-canary`.
