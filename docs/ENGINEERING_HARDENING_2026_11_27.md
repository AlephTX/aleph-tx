# Engineering Hardening: Shadow Audit Closure, SHM Feed Continuity Telemetry, and Go BBO Validation (2026-11-27)

**Status:** Verified offline in shared worktree `swarm-eng-hardening-8e904134` (strict HOLD: zero live orders, zero cancels, zero deploys; `feeder/shm/` ABI untouched; zero `crate::exchanges::*` imports in `src/strategy/`).

## 1. Gemini #125 Audit Closures (`src/strategy/track_shadow.rs`)

### 1.1 Maker Freshness & Non-Crossed Validation in `Track02ShadowSession::on_maker_bbo_cross_diagnostic`
- **Problem:** Previously, [`Track02ShadowSession::on_maker_bbo_cross_diagnostic`](../src/strategy/track_shadow.rs) checked `valid_bid_ask(maker.bid, maker.ask)` but did not check `maker.recv_ms` against `now_ms`, `pair.cfg.strategy.max_book_age_ms`, or prior maker receipt watermarks. On live SHM, a frozen maker slot alongside an active hedge slot could trigger synthetic BBO-cross fills from stale maker quotes.
- **Invariant:** Before triggering any diagnostic cross fill via `on_maker_trade_print`, `on_maker_bbo_cross_diagnostic` now requires:
  1. `valid_bid_ask(maker.bid, maker.ask)` (finite, `bid > 0.0`, `ask > bid`),
  2. `maker.recv_ms <= now_ms` (no future receipt clock),
  3. `now_ms - maker.recv_ms <= pair.cfg.strategy.max_book_age_ms` (fresh maker book), and
  4. `maker.recv_ms >= last_recv_ms` when a prior maker receipt exists (no clock regression).
- **Test coverage:** `track02_bbo_cross_diagnostic_drops_stale_and_clock_regressed_maker` in [`src/strategy/track_shadow.rs`](../src/strategy/track_shadow.rs) verifies stale (`> max_book_age_ms`), future (`recv_ms > now_ms`), clock-regressed (`recv_ms < last_recv_ms`), and crossed maker quotes all return `Ok(false)` with `fills == 0`.

### 1.2 Authoritative Exit Confirmation in `Track01ShadowSession::on_bbo_pair`
- **Problem:** When shadow exit execution confirmed flat on `LeadLagAction::Exit`, [`Track01ShadowSession::on_bbo_pair`](../src/strategy/track_shadow.rs) previously called `arm.engine.clear_position()`, conflating authoritative exit fill confirmation with manual/skip resets.
- **Invariant:** Confirmed shadow exits (`tgt_touch.valid()`) now call `arm.engine.notify_exit_filled()` (matching [`LeadLagTaker`](../src/strategy/lead_lag_taker.rs)), while `clear_position()` remains reserved for skipped entries and out-of-sync flat resets.
- **Test coverage:** `track01_exit_confirms_flat_via_notify_exit_filled_and_handles_skip_distinctly` in [`src/strategy/track_shadow.rs`](../src/strategy/track_shadow.rs) exercises both the skip (`clear_position()`) and confirmed exit (`notify_exit_filled()`) paths.

## 2. Read-Only SHM Feed Health & Continuity Diagnostics

Implemented per-venue feed telemetry via `VenueFeedTracker` / `VenueFeedHealthReport` in [`src/strategy/track_shadow.rs`](../src/strategy/track_shadow.rs) and SHM slot classification in [`src/bin/track01_track02_shadow.rs`](../src/bin/track01_track02_shadow.rs) (extending the shadow harness documented in [`TRACK01_TRACK02_LEAD_LAG_AND_MAKER_HEDGE_2026_11_26.md`](TRACK01_TRACK02_LEAD_LAG_AND_MAKER_HEDGE_2026_11_26.md)):

- **Reported metrics** (surfaced per-arm/per-pair, aggregated per `exchange_id` in `venue_feed_health`, and summarized at the top level of `Track01ShadowReport`, `Track02ShadowReport`, and `track01_track02_shadow` JSON output):
  - `samples`: count of valid, non-regressed, fresh observations.
  - `max_gap_ms`: maximum observed gap between fresh venue receipt timestamps (`recv_ms - prev_recv_ms`), also updated on stale freezes (`now_ms - prev_recv_ms`) so unrecovered dead feeds are not masked as zero-gap.
  - `stale_book_drops`: books exceeding `max_ref_age_ms` / `max_tgt_age_ms` (Track 01) or `max_book_age_ms` (Track 02).
  - `clock_regression_drops`: books with `recv_ms > now_ms`, `now_ms < prev_now_ms`, or `recv_ms < prev_recv_ms` (which also fail closed in `on_bbo_pair` and `on_pair_books`).
  - `crossed_book_drops`: books failing `valid_bid_ask(bid, ask)` (`bid >= ask`, non-positive, or non-finite), plus populated SHM slots (`msg_type == 1 && timestamp_ns > 0`) rejected by `decode_shm_slot` as `ShmSlotDecode::InvalidOrCrossed`.
- **Fail-closed SHM handling in [`src/bin/track01_track02_shadow.rs`](../src/bin/track01_track02_shadow.rs):** `decode_shm_slot` distinguishes `Empty`, `InvalidOrCrossed`, and `Valid`. In `run_shm`, `InvalidOrCrossed` slots record `record_venue_crossed_drop`, and any non-`Valid` Track 02 maker/hedge slot calls `clear_pair_quotes` so resting quotes cannot linger over an invalidated or crossed SHM book.
- **Test coverage:**
  - `track01_feed_health_tracks_stale_clock_regression_crossed_and_gap`
  - `track02_feed_health_tracks_stale_clock_regression_crossed_and_gap`
  - `decode_shm_slot_distinguishes_empty_invalid_crossed_and_valid` and `check_and_replay_work_for_track02` in [`src/bin/track01_track02_shadow.rs`](../src/bin/track01_track02_shadow.rs)

## 3. Go Feeder BBO Hardening (`feeder/exchanges/arcus.go`, `feeder/exchanges/hyperliquid.go`)

All `feeder/shm/` ABI structs and seqlock layouts remain untouched.

### 3.1 Arcus ([`feeder/exchanges/arcus.go`](../feeder/exchanges/arcus.go), [`feeder/exchanges/arcus_test.go`](../feeder/exchanges/arcus_test.go))
- **Strict decimal float syntax (`isValidDecimalFloatBytes`):** `parseFloatBytes` rejects Go-specific `_` digit separators, hex floats (`0x...`), whitespace, and non-decimal strings with zero heap allocations, alongside `math.IsNaN`, `math.IsInf`, `px <= 0`, `sz < 0` (L2) / `sz <= 0` (BBO), and `bid >= ask` checks.
- **Epoch sanity check (`minArcusUnixMicro = 1_000_000_000_000_000`):** Rejects sub-microsecond epoch units (seconds/milliseconds mistakenly passed as microseconds) alongside the future timestamp bound (`> now + 1s`).
- **Watermark hardening (`arcusFresh`):** Once `lastSeq > 0`, requires `seq > lastSeq && tsUs >= lastTsUs`, preventing missing/zero-`globalSequenceId` messages from bypassing the sequence watermark via timestamp fallback.

### 3.2 Hyperliquid ([`feeder/exchanges/hyperliquid.go`](../feeder/exchanges/hyperliquid.go), [`feeder/exchanges/hyperliquid_test.go`](../feeder/exchanges/hyperliquid_test.go))
- **Epoch sanity check (`minHyperliquidUnixMilli = 1_000_000_000_000`):** Rejects seconds-scale/invalid epochs on both `l2Book` (`handleL2Book`) and `bbo` (`handleBBO`) alongside existing `> now + 1s` and monotonic `lastBboPub`/`lastDepthPub` watermarks.
- **Fail-closed `l2Book` side count guard:** If `book.Levels.sideCount != 2` at `book.Time >= lastBboPub[target]` or `lastDepthPub[target]`, both BBO and depth are invalidated rather than leaving stale quotes live.
- **Zero-alloc key matching on hot path:** Replaced per-call `[]byte("px")` / `bytes.EqualFold` in `hlL2Levels.parseObjectLevel` with package-level `hlKeyPx` / `hlKeySz` and `asciiEqualFoldBytes`.
- **Table-driven & race tests:** Added `TestArcusHardenedValidationTableDriven`, `TestArcusConcurrentBBOAndL2Race`, and `TestHyperliquidHardenedValidationTableDriven` covering malformed JSON, `NaN`/`Inf`/`_`/`0x`, `<= 0` prices/sizes, crossed/locked books, regressed sequences/timestamps, bad epochs, and `-race` concurrency.

## 4. Verification Summary

All checks ran inside this worktree after the final code edit:

| Check | Result |
| --- | --- |
| `cargo fmt --check` | Pass |
| `cargo clippy --locked --features research-replay --lib --bins -- -D warnings` | Pass |
| `cargo test --locked --lib track_shadow` | Pass (`14 passed; 0 failed`) |
| `cargo test --locked --bin track01_track02_shadow` | Pass (`4 passed; 0 failed`) |
| `cd feeder && gofmt -l .` | Clean (no unformatted files) |
| `cd feeder && go vet ./...` | Pass |
| `cd feeder && go test -race -count=1 ./...` | Pass (`cmd/perp-capture`, `config`, `exchanges`, `internal/wsutil`, `shm`) |
| `cd feeder && go test -run=^$ -bench="Benchmark(Arcus|Hyperliquid)" -benchmem ./exchanges/` | Pass (`0 B/op`, `0 allocs/op` across all 4 Arcus/HL L2+BBO benchmarks) |
