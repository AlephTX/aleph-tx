# Sprint 19: Arcus CancelLease / Durable Ledger Red-Team Audit & Multi-Agent Handoff Protocol v2 (2026-11-21)

> **Status**: Read-only security/safety audit, Sprint 18 residual hygiene closure, and multi-agent handoff protocol upgrade. All three live tracks remain **`HOLD`**. Zero live trading, cancellations, deployments, restarts, or remote mutations.

---

## 1. Sprint 18 Residual Closure & Hygiene Fixes

1. **`t50_iso0` Cancel-Bound Micro-Tuning Formally Closed**:
   - Following Sprint 18 (`bound_breach` 50.07% of monthly 1s cancel-pending markout), Codex completed full quote-fill linkage ([RESEARCH_SYNTHESIS_AND_QUOTE_CONSTRAINTS_2026_11_20.md](RESEARCH_SYNTHESIS_AND_QUOTE_CONSTRAINTS_2026_11_20.md)), tested the pre-registered causal `0.7 bp` `toward_flat_placement_reserve` guard ([BOUND_BREACH_LINKAGE_AND_GUARD_2026_11_20.md](BOUND_BREACH_LINKAGE_AND_GUARD_2026_11_20.md)), and reconciled full FIFO inventory cycles ([INVENTORY_CYCLE_CASH_ATTRIBUTION_2026_11_20.md](INVENTORY_CYCLE_CASH_ATTRIBUTION_2026_11_20.md)).
   - Result: although `cancel_pending` 1s markout improved `+$1.001293`, settled cash across the 15 later windows fell **`-$3.301493`** (`-$3.238423` stress net, negative under every leave-one-date-out slice), because 69 `bound_breach` fills had `reduce_only=false` (`57.39%` of notional actually added or flipped risk at fill time) and `86.78%` (`-$2.865099`) of the cash drop came from altered inventory drift paths. The `t50_iso0` cancel-bound micro-tuning branch is closed.
2. **`ReasonMarkout` Comment Precision ([recorded.rs](../src/strategy/inventory_neutral_mm/replay/recorded.rs#L896-L902))**:
   - Fixed the doc comment flagged in Codex's Sprint 18 review: per-reason `marked_fills` and non-negative `marked_turnover` sum to at most the global values, whereas **signed** `markout_quote` equals global `cancel_pending_markout_quote` when all cancel-pending fills are attributed and can be either larger or smaller when unattributed fills exist.
3. **Private Share-Link Redaction ([RESIDUAL_SIGNAL_AND_CASH_FEASIBILITY_2026_11_20.md](RESIDUAL_SIGNAL_AND_CASH_FEASIBILITY_2026_11_20.md#L10))**:
   - Removed the embedded private report URL from trackable `docs/` line 10 while retaining the local `.agent-state/research/report_synthesis_20261120/sources/report_review.json` evidence pointer, aligning with `AGENTS.md` and the `2026-11-19 00:21 CST` local-only rule.

---

## 2. Red-Team Security & Safety Audit: `CancelLease`, `ArcusTrading`, and `ExecutionJournal`

Audit scope (read-only in this turn so Codex's verified SHA-256 baseline stays intact pending coordinated lock claim):
- [scheduled_cancel.rs](../src/exchanges/arcus/scheduled_cancel.rs)
- [cancel_lease.rs](../src/exchanges/arcus/cancel_lease.rs)
- [rest.rs](../src/exchanges/arcus/rest.rs)
- [durable.rs](../src/hedging/durable.rs) and submodules [model.rs](../src/hedging/durable/model.rs), [native.rs](../src/hedging/durable/native.rs), [storage.rs](../src/hedging/durable/storage.rs)

### 2.1 Findings Table

| ID | Severity | Location | Finding & Impact | Recommended Fix |
| --- | --- | --- | --- | --- |
| **R1** | **High** | `src/exchanges/arcus/rest.rs:115, 119, 158, 186–194` | **Wall-Clock Step Detector Masked by Monotonic Clamping.** `ArcusTrading::renew_cancel_lease` passes `self.timestamp()?` as `sent_wall_ns` and `received_wall_ns` to `CancelLease::{begin_renewal,confirm_renewal}`, and `schedule_cancel` passes `self.timestamp()?` as `received_ns` to `ScheduledCancelReceipt::validate`. Because `self.timestamp()` computes `last.checked_add(1).map(|v| v.max(now))`, an OS/NTP backward jump (e.g., `-10s`) during HTTP RTT is clamped to `+1/+2ns` above `sent_wall_ns` — silently bypassing `"Arcus wall clock moved backwards"` and `wall_elapsed.abs_diff(elapsed) <= max_clock_step` in `cancel_lease.rs:225-232`, and masking clock-synced expiry validation in `scheduled_cancel.rs:100-107`. | Keep `self.timestamp()` strictly for signing headers/nonces, and pass raw `SystemTime::now()` into `CancelLease` & `ScheduledCancelReceipt` validation so genuine OS wall-clock jumps trip `max_clock_step` / expiry rejection; add a backward-step test in `rest/cancel_lease_tests.rs`. |
| **R2** | **High** | `src/hedging/durable.rs:559–578`; `native.rs:162–167`; `model.rs:191–195` | **Same-Millisecond Multi-Fill IOC Halts Session.** `ExecutionJournal::record_fill` checks `known.venue_at_us < fill.venue_at_us` (strict `<`) for `source_ordered` and sets `self.recovery_required = true` whenever `!source_ordered`. Hyperliquid (`xyz:ANT`) `userFills` timestamps are millisecond-granularity (`ms_to_us` scales ms by `1000`). Any routine multi-level Hyperliquid IOC sweep producing >=2 partial fills in the same millisecond satisfies `fill.venue_at_us == known.venue_at_us`, sets `!source_ordered`, and trips the session-halting recovery barrier (`prepare_intent` fails) mid-hedge. | Separate strict regression (`fill.venue_at_us < max_known_venue_at_us`, which must trip `recovery_required`) from same-timestamp ties (`==`, which should leave `FillEffect::{closed_base,opened_base,crossed_zero}` as `None` per `model.rs:191` without tripping a session halt), and add a same-ms 2-fill IOC unit test. |
| **R3** | **Medium** | `src/hedging/durable.rs:125–140, 259–272`; `model.rs:34–42` | **Missing Price-Tick & Worst-Case Quote Budget Check.** `Event::Prepared` verifies `quantity_step` and `max_order_notional`, but does not enforce venue `price_tick` (`limit_price % price_tick == 0`) or reserve quote/margin budget across open orders at `limit_price` + max fee. Directly relevant after Codex's `FOUR_COIN_FIRST_FINAL_AND_CORE_2026_11_21.md` finding where a fixed-size offline grid checked decision mid against remaining `$501.22`, slipped 30 bp at arrival (`$501.55`), and overdrew budget by `-$0.32`. | Add `price_tick: Decimal` to `LegLimits` (validated > 0 and enforced on `limit_price`) and enforce worst-case quote + max-fee reservation before `prepare_intent` in live supervisor wiring. |
| **R4** | **Low-Medium** | `src/hedging/durable.rs:420–428` | **`Event::Closed` Double-Freshness Window.** `Event::Reconciled` checks `now_us - evidence.received_at_us <= max_observation_age_us`, whereas `Event::Closed` checks `now_us - self.written_at_us <= max_observation_age_us`. Up to `2 * max_observation_age_us` (e.g. `10s`) can elapse between the actual venue observation and `Closed` admission. | Track `last_reconciled_received_at_us` (or require `now_us - min(received_at_us) <= max_observation_age_us`) inside `Ledger::apply(Event::Closed)`. |
| **R5** | **Low** | `src/hedging/durable/storage.rs:128–136` | **`.account.lock` Concurrent Creation Race.** `if lock_path.exists() { private_file } else { create_file }` fails closed on `AlreadyExists` when two processes race to initialize the lock file simultaneously (safe, but noisy on legitimate second reader/auditor or restart). | Handle `ErrorKind::AlreadyExists` from `create_file` by falling through to `private_file(&lock_path, true)` while preserving all `0o600` / symlink / `nlink == 1` / `flock` guards. |

### 2.2 Positive Safety Invariants Verified
- **`CancelLease`**: Sticky `faulted = true` before every state transition (`begin_renewal` / `confirm_renewal`), consumed single-use pending state, dropped async future invalidation via `CancelLeaseAttempt`, dispatch-anchored monotonic deadline (`deadline_us > last_deadline_us`), and non-serializable restart posture (`Restart` must re-reconcile before constructing a new lease).
- **`ExecutionJournal` & `storage.rs`**: Fail-closed `poisoned = true` prior to file write/fsync, checksum-chained JSONL + atomic `.active.json` tip marker detecting silent tail truncation, `0o700`/`0o600`/`!symlink`/`nlink==1` inode verification, `native.rs` strict rejection of unknown Arcus fee assets and unproven HL cancel quantities, and read-only `audit_journal` hardcoding `restart_allowed: false`.

---

## 3. Cross-Agent Operational & Research Recommendations

1. **Tokyo Disk Headroom (`14.20 GiB`) & Drive 7× EOF Mitigation**:
   - `W13 crypto` (`6.096 GB`) has suffered 7 unexpected EOFs during whole-file Google Drive readback while `W14` writes `~1.8 GiB/h` and Tokyo is below its `16 GiB` eviction floor (`2 vCPU` at load `~3.0`, `13–17%` cgroup throttling across the 3 main streams).
   - Recommend adding HTTP Range chunked/resumable Drive readback for multi-GB archives if EOF persists, and closely monitoring main `lighter_trades` gap counts while `aleph-research-capture.service` is active.
2. **ANT 48h Critical Path**:
   - Deliver Tokyo's two already-sealed `12:15–13:30` ANT raw segments (`59.4 MB` + `36.4 MB`) to Codex's `ant_dual_capture_clock_20261121` matcher before any further ANT strategy tuning.
   - Align Claude Code's `maker_hedge.py` / `leadlag.py` with Codex's accounting boundary: explicitly separate `0s publication_lag` (`~93ms` Arcus L2 snapshot delay) from `true future exchange-clock move`, and deduct two-leg residual unwind slippage/fees instead of zero-cost mid-mark valuation.
   - Once decoupled cleanly from snapshot lag, test **Arcus trade-feed (`~28ms`, 65ms faster than L2 snapshots) + RH lead cancellation**, and verify whether SPY/QQQ's constant `8–14 bp` Arcus-vs-Lighter RTH discount stems from oracle/index source or dividend/collateral convention before treating it as arb edge.
3. **Workspace Hygiene**:
   - The working tree holds 50+ modified/untracked files across 10 workstreams. With user approval, verified offline infrastructure (`S9–S18` diagnostics, `CancelLease`/`scheduled_cancel`, `durable` ledger, Backpack capture) should be committed cleanly by module while keeping `.agent-state/` and local-only docs (`AGENTS.md`) untracked/unstaged.

---

## 4. Multi-Agent Handoff Protocol v2 (`Codex` · `Claude Code` · `Gemini` · `Trae/DeepSeek`)

To avoid duplicated experiments or stale TODO lists across 3–4 concurrent agents:
1. **`AGENT_HANDOFF.md` §1.3 Live Agent Roster & Lock Table**: Each agent maintains its exact `Status / Lock Scope / Current Priority` row in §1.3 and must check §1.3 before editing any file under `src/`, `scripts/`, or `deploy/`.
2. **Explicit `Next Actions` Routing in §2**: Every new Section 2 entry ends with explicit `@Codex` / `@Claude` / `@Gemini` / `@Trae` bullets distinguishing *(a) blocking code/data handoffs* from *(b) completed/closed branches* so returning agents do not re-run falsified work.
3. **Strict Domain Partition**:
   - **Codex (`ChatGPT`)**: Tokyo–Drive–aleph custody/seals, frozen `W09–W16` & 7-day 4-coin forward observers, full replay/accounting arbitration.
   - **Claude Code**: Tokyo independent ANT/RWA measurement (authorized), exchange rule/fee/oracle API verification, Backpack capture & trade-feed lead/lag diagnostics.
   - **Gemini (`Antigravity`)**: Hourly standing handoff supervision (`35 * * * *`), Rust/Go execution-safety red-team auditing (`P0-B` / `OPT-01..04`), hygiene & cross-agent synthesis.
   - **Trae (`DeepSeek`, when onboarded)**: Independent self-contained items after reading `AGENTS.md` + `AGENT_HANDOFF.md` §1.3 — e.g., `OPT-01` runner capability table unification or Backpack fee/incentive verification — never duplicating live/remote capture or frozen replays.

---

## 5. Mac Verification Gates (`2026-11-21 18:19 CST`, Post-Final-Edit)

| Gate | Command | Result |
| --- | --- | --- |
| Format (`recorded.rs`) | `rustfmt --edition 2024 --check src/strategy/inventory_neutral_mm/replay/recorded.rs` | PASS |
| Workspace fmt / check | `make fmt-check` & `make check` | PASS |
| Clippy (default) | `cargo clippy --locked --all-targets -- -D warnings` | PASS (0 warnings) |
| Clippy (`research-replay`) | `cargo clippy --locked --features research-replay --all-targets -- -D warnings` | PASS (0 warnings) |
| Rust tests (default) | `cargo test --locked --quiet` | PASS (`694+5+8`, `6 ignored`) |
| Rust tests (`research-replay`) | `make test` (`--features research-replay --lib --bins`) | PASS (`764+5+8`, `6 ignored`) |
| Go tests (uncached race) | `cd feeder && go test -race -count=1 ./...` | PASS (5/5 packages) |
| Research Python | `make test` (`scripts/test_*.py`) | PASS (`285` tests) |
| Monitor Python | `make test-python` | PASS (`42` tests) |
| Release build | `make build` | PASS |
| Dashboard | `dashboard/` untouched | Not re-run |
