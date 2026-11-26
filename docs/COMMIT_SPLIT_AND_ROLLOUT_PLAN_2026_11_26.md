# Conventional Commit-Split Plan & Tokyo `W26+` Rollout Blueprint

**Date**: `2026-11-26` · **Operational Mode**: Strictly **`HOLD`** (zero live orders/cancels, zero Tokyo deploys/restarts, `STAGED=0` preserved)

This blueprint defines:
1. An atomic, independently verifiable **Conventional Commit-split plan** for the current unstaged workspace deliverables (separating peer-modified files and local-only untracked state).
2. A **phased, user-authorized `oasis-tokyo` `W26+` rollout runbook** covering Go `perp-capture` (`bbo` + `-hl-fast-book`), Lakehouse `bbo` ingestion verification, and zero-order `track01_track02_shadow` live SHM observation.

---

## 1. Conventional Commit-Split Plan (Current Unstaged Changes)

> [!CAUTION]
> **Hard `AGENTS.md` / Git Safety Boundary — NEVER Stage or Commit:**
> - `AGENT_HANDOFF.md`
> - `.agent-state/` (including `.agent-state/research/`, `.agent-state/calibration/`, `.agent-state/archive/`)
> - `.env*` (`.env`, `.env.all_exchanges`, `.env.backpack`, `.env.edgex`, `.env.lighter`, `.env.testnet`)
> - `pkg/` (if present)
> - `docs/ARCUS_ENTROPY_MM_OPTIMIZATION_AND_CALIBRATION_2026_11_26.md` (local-only calibration record containing small-size account calibration fills/PnL and authenticated account status — keep preserved locally outside Git)
> - Never use `git add -A` or `git add .`. Stage explicit path lists only, verify `git diff --cached` before every commit, and preserve `Cargo.lock` and [inventory_neutral_mm.rs](../src/strategy/inventory_neutral_mm.rs) (`ce0930d3...`) untouched.

### 1.1 Pre-Staging Secret & Boundary Gate (`AGENTS.md`)

Run before staging **each** commit bucket:

```sh
# 1. Confirm staging area is clean before selecting the next bucket
test "$(git diff --cached --name-only | wc -l | tr -d ' ')" = "0"

# 2. Confirm frozen baseline strategy and lockfile are untouched
shasum -a 256 src/strategy/inventory_neutral_mm.rs
# Expected: ce0930d3a843ee4b8a15d27f3d9987ec1cea7d2c60bcd1c5a3a4064501fe1488
git diff --exit-code Cargo.lock

# 3. After `git add -- <explicit_paths>`, audit staged paths & content before committing
git diff --cached --name-only | rg -n '(^AGENT_HANDOFF\.md$|^\.agent-state/|^\.env|^pkg/)' && exit 1 || true
git diff --cached --check
git diff --cached
```

### 1.2 Atomic Commit Buckets

#### Commit 1: `feat(feeder): add zero-alloc Arcus bbo and Hyperliquid bbo/fast-book capture`

- **Scope**: Event-driven `bbo` subscription + monotonic `globalSequenceId` ordering for Arcus (`0 B/op`, zero `feeder/shm/` ABI changes), Hyperliquid/HIP-3 `bbo` + `activeAssetCtx` capture in `perp-capture`, and `-hl-fast-book` in Tokyo `crypto`/`rwa` capture units.
- **Files**:
  - [arcus.go](../feeder/exchanges/arcus.go)
  - [arcus_test.go](../feeder/exchanges/arcus_test.go)
  - [multivenue_test.go](../feeder/exchanges/multivenue_test.go)
  - [main.go](../feeder/cmd/perp-capture/main.go)
  - [arcus.go](../feeder/cmd/perp-capture/arcus.go)
  - [arcus_test.go](../feeder/cmd/perp-capture/arcus_test.go)
  - [hyperliquid.go](../feeder/cmd/perp-capture/hyperliquid.go)
  - [hyperliquid_test.go](../feeder/cmd/perp-capture/hyperliquid_test.go)
  - [aleph-perp-capture-crypto.service](../deploy/oasis/aleph-perp-capture-crypto.service)
  - [aleph-perp-capture-rwa.service](../deploy/oasis/aleph-perp-capture-rwa.service)
- **Pre-commit verification**:
  ```sh
  (cd feeder && go test -race -count=1 ./...)
  make check
  ```

#### Commit 2: `feat(lakehouse): normalize Arcus and Hyperliquid bbo channels into Silver/DuckDB`

- **Scope**: Add `_parse_bbo_side` and `channel == "bbo"` / `chan == "bbo"` normalization in `quant_lakehouse.py` (`bestBid`/`bestAsk`, `globalSequenceId -> venue_seq`, `depth_levels=1`, one-sided/zero-size handling, coexistence with `l2Orderbook` in `v_bbo_quotes`) plus unit tests.
- **Files**:
  - [quant_lakehouse.py](../scripts/quant_lakehouse.py)
  - [test_quant_lakehouse.py](../scripts/test_quant_lakehouse.py)
- **Pre-commit verification**:
  ```sh
  python3 -m unittest scripts/test_quant_lakehouse.py
  make test-research
  ```

#### Commit 3: `feat(strategy): add decoupled Track 01/02/03 engines, router economics, and shadow state`

- **Scope**: Pure, zero-`crate::exchanges::*` strategy engines (`QueuePriorityMmEngine` V4, `LeadLagTakerEngine` with clock-regression hardening & opt-in `LeadLagExitMode`, `BasisMakerHedge` with `RefCancelShieldConfig.fail_closed_when_unready` & `ShieldedPairEconomics`, `MarkoutShield` side veto, `HybridExit` passive-touch-before-IOC, `CrossVenueRouter` Maker-First economics, `VenueRegistry::default()` `backpack` registration, and `track_shadow` wrapper).
- **Files**:
  - [queue_priority_mm.rs](../src/strategy/queue_priority_mm.rs)
  - [lead_lag_taker.rs](../src/strategy/lead_lag_taker.rs)
  - [basis_maker_hedge.rs](../src/strategy/basis_maker_hedge.rs)
  - [track_shadow.rs](../src/strategy/track_shadow.rs)
  - [quote_pipeline.rs](../src/strategy/quote_pipeline.rs)
  - [hybrid_exit.rs](../src/strategy/hybrid_exit.rs)
  - [cross_venue_router.rs](../src/strategy/cross_venue_router.rs)
  - [mod.rs](../src/strategy/mod.rs)
  - [venues.rs](../src/venues.rs)
- **Pre-commit verification**:
  ```sh
  make fmt-check
  make check
  # Confirm zero exchange coupling across strategy modules
  rg -n 'crate::exchanges' src/strategy/{queue_priority_mm,lead_lag_taker,basis_maker_hedge,track_shadow,quote_pipeline,hybrid_exit,cross_venue_router,mod}.rs src/venues.rs && exit 1 || true
  shasum -a 256 src/strategy/inventory_neutral_mm.rs
  git diff --exit-code Cargo.lock
  ```

#### Commit 4: `feat(execution): harden Arcus/Hyperliquid clients and add calibrator & shadow binaries`

- **Scope**: Exchange execution hygiene (`ArcusInfo::account`, `cancel_lease` / `scheduled_cancel` / `rest` hardening, Hyperliquid `cancelByCloid` / `batchModify` / `scheduleCancel` / `spotClearinghouseState` / `userFillsByTime`), `venue_mm_calibrator`, and the zero-order `track01_track02_shadow` binary with example TOML profiles.
- **Files**:
  - [arcus.rs](../src/exchanges/arcus.rs)
  - [cancel_lease.rs](../src/exchanges/arcus/cancel_lease.rs)
  - [tests.rs](../src/exchanges/arcus/cancel_lease/tests.rs)
  - [rest.rs](../src/exchanges/arcus/rest.rs)
  - [scheduled_cancel.rs](../src/exchanges/arcus/scheduled_cancel.rs)
  - [client.rs](../src/exchanges/hyperliquid/client.rs)
  - [mod.rs](../src/exchanges/hyperliquid/mod.rs)
  - [model.rs](../src/exchanges/hyperliquid/model.rs)
  - [tests.rs](../src/exchanges/hyperliquid/tests.rs)
  - [venue_mm_calibrator.rs](../src/bin/venue_mm_calibrator.rs)
  - [track01_track02_shadow.rs](../src/bin/track01_track02_shadow.rs)
  - [Cargo.toml](../Cargo.toml)
  - [track01_lead_lag_shadow.toml](../examples/track01_lead_lag_shadow.toml)
  - [track02_shielded_maker_hedge_shadow.toml](../examples/track02_shielded_maker_hedge_shadow.toml)
- **Pre-commit verification**:
  ```sh
  make test
  make build
  ./target/release/track01_track02_shadow --track 1 --config examples/track01_lead_lag_shadow.toml --check
  ./target/release/track01_track02_shadow --track 2 --config examples/track02_shielded_maker_hedge_shadow.toml --check
  ```

#### Commit 5: `feat(research): add Track 01/02/03 replay, walk-forward, cadence probes, and unit tests`

- **Scope**: Causal 100 ms BBO grid, Track 01 lead-lag & hybrid exit walk-forward, Track 02 shielded maker-hedge & basis replay, Track 03 multi-venue queue-priority evaluation, read-only WS cadence probes, and offline synthetic unit suites.
- **Files**:
  - [lakehouse_grid.py](../scripts/lakehouse_grid.py)
  - [research_basis_ws_trio.py](../scripts/research_basis_ws_trio.py)
  - [research_funding_carry.py](../scripts/research_funding_carry.py)
  - [research_lead_lag_hybrid.py](../scripts/research_lead_lag_hybrid.py)
  - [research_lead_lag_quant.py](../scripts/research_lead_lag_quant.py)
  - [research_rwa_basis_exec.py](../scripts/research_rwa_basis_exec.py)
  - [research_shielded_maker_hedge.py](../scripts/research_shielded_maker_hedge.py)
  - [walkforward_lead_lag.py](../scripts/walkforward_lead_lag.py)
  - [replay_maker_hedge_basis.py](../scripts/replay_maker_hedge_basis.py)
  - [sim_queue_priority_mm.py](../scripts/sim_queue_priority_mm.py)
  - [eval_multivenue_three_tracks.py](../scripts/eval_multivenue_three_tracks.py)
  - [probe_arcus_ws_cadence.py](../scripts/probe_arcus_ws_cadence.py)
  - [probe_hl_ws_cadence.py](../scripts/probe_hl_ws_cadence.py)
  - [test_track01_track02_research.py](../scripts/test_track01_track02_research.py)
  - [test_sim_queue_priority_mm.py](../scripts/test_sim_queue_priority_mm.py)
  - [test_eval_multivenue_three_tracks.py](../scripts/test_eval_multivenue_three_tracks.py)
- **Pre-commit verification**:
  ```sh
  make test-research
  make test-python
  ```

#### Commit 6: `docs: add 2026-11-26 three-track engineering, W25 regression, and rollout reports`

- **Scope**: Public-data empirical reports, W25 regression & Lakehouse `bbo` upgrade report, and this commit-split / `W26+` rollout guide (excluding local-only `docs/ARCUS_ENTROPY_MM_OPTIMIZATION_AND_CALIBRATION_2026_11_26.md`).
- **Files**:
  - [THREE_TRACK_MULTIVENUE_REVIEW_AND_UPGRADE_2026_11_26.md](THREE_TRACK_MULTIVENUE_REVIEW_AND_UPGRADE_2026_11_26.md)
  - [THREE_TRACK_PROFITABILITY_AND_FEED_UPGRADE_2026_11_26.md](THREE_TRACK_PROFITABILITY_AND_FEED_UPGRADE_2026_11_26.md)
  - [THREE_TRACK_W25_REGRESSION_AND_LAKEHOUSE_BBO_UPGRADE_2026_11_26.md](THREE_TRACK_W25_REGRESSION_AND_LAKEHOUSE_BBO_UPGRADE_2026_11_26.md)
  - [TRACK01_TRACK02_LEAD_LAG_AND_MAKER_HEDGE_2026_11_26.md](TRACK01_TRACK02_LEAD_LAG_AND_MAKER_HEDGE_2026_11_26.md)
  - [COMMIT_SPLIT_AND_ROLLOUT_PLAN_2026_11_26.md](COMMIT_SPLIT_AND_ROLLOUT_PLAN_2026_11_26.md)
- **Pre-commit verification**:
  ```sh
  git diff --check
  # Verify every markdown link in docs/*_2026_11_26.md resolves cleanly (`BAD_LINKS=[]`)
  python3 -c '
  import pathlib, re
  bad = []
  for p in sorted(pathlib.Path("docs").glob("*_2026_11_26.md")):
      text = p.read_text()
      for m in re.finditer(r"\[[^\]]+\]\(([^)]+)\)", text):
          target = m.group(1).split("#", 1)[0]
          if "://" in target or not target:
              continue
          if not (p.parent / target).resolve().exists():
              bad.append((str(p), target))
  print("BAD_LINKS=" + repr(bad))
  raise SystemExit(1 if bad else 0)
  '
  ```

### 1.3 Separate Peer-Modified Bucket (Do Not Mix Blindly)

The following **6 pre-existing modified files** belong to ongoing peer (`Codex` / `Gemini`) scoring, basis replay, and data-source registry work (`+1,204 / -140` lines). **Do not fold them into Commits 1–6**; review and commit them separately with peer coordination:

- [RESEARCH_DATA_SOURCES.md](RESEARCH_DATA_SOURCES.md)
- [score_strategy_arms.py](../scripts/score_strategy_arms.py)
- [test_score_strategy_arms.py](../scripts/test_score_strategy_arms.py)
- [basis_core.rs](../src/strategy/basis_core.rs)
- [basis_replay.rs](../src/strategy/basis_replay.rs)
- [pipeline_driver.rs](../src/strategy/pipeline_driver.rs)

---

## 2. Phased Tokyo `W26+` Maintenance-Window Rollout Runbook

> [!WARNING]
> **Hard Hold Until Post-W26 Boundary & Explicit User Authorization:**
> - **DO NOT** deploy, install, or restart any service on `oasis-tokyo` during the active `W26` capture window (scheduled seal around **`00:15 CST`**).
> - [perp_rolling_runner.py](../scripts/perp_rolling_runner.py) moves any interrupted `.partial` capture to `quarantine/` on `SIGTERM`/`SIGINT`, and [install.sh](../deploy/oasis/install.sh) atomically restarts `aleph-tx-feeder.service` and `aleph-tx-monitor.service`.
> - **Prerequisites before Phase 1**: (1) `W26` window naturally sealed (`3/3` streams `.ndjson.gz` + `.bronze_manifest.json`); (2) Codex `W26` Drive transfer, SHA-256 readback verification, and backup verification complete; (3) explicit user sign-off for the maintenance window.

### Phase 1: Deploy Go `perp-capture` (`bbo` + `-hl-fast-book`) at Authorized Window Boundary

1. **Pre-snapshot local checks (Mac)** and **release build (Linux x86-64)**:
   - Run the full local verification gates in [Makefile](../Makefile) (`make fmt-check`, `make check`, `make test`, `make dashboard-build`) so `dashboard/dist/index.html` is present for [install.sh](../deploy/oasis/install.sh).
   - Build the credential-free Linux x86-64 release snapshot (`RUSTUP_TOOLCHAIN=1.100.0 make build`) per [deploy/oasis/README.md](../deploy/oasis/README.md).
2. **Pre-deploy host safety checks on `oasis-tokyo`**:
   - Verify live trading units are stopped (`bash deploy/oasis/check-stopped-units.sh aleph-tx-live.service aleph-tx-arcus-xyz.service`).
   - Verify free disk budget is **above `16 GiB`** (`df -BG /var/lib/aleph-tx-captures`, matching `--evict-below-free-gib 16.0` in the rolling capture units and [aleph-tx-flywheel.service](../deploy/oasis/aleph-tx-flywheel.service)).
   - Confirm `W26` has finished sealing and uploading before touching capture services.
3. **Install release & restart capture streams at window boundary**:
   - Run `deploy/oasis/install.sh RELEASE SHA256 COMMIT` from the built snapshot (updates `/opt/aleph-tx/current`, installs unit files including `aleph-perp-capture-{crypto,rwa}.service`, reloads systemd, and restarts `aleph-tx-feeder.service` + `aleph-tx-monitor.service`).
   - Because `install.sh` does **not** restart the rolling capture daemons, explicitly restart only the upgraded `crypto` and `rwa` units immediately after a window seal boundary (leaving `aleph-perp-capture-bulk.service` untouched unless required):
     ```sh
     sudo systemctl restart aleph-perp-capture-crypto.service aleph-perp-capture-rwa.service
     ```
   - Verify health:
     - `systemctl status aleph-tx-feeder aleph-tx-monitor aleph-perp-capture-crypto aleph-perp-capture-rwa aleph-perp-capture-bulk`
     - `curl --fail --silent http://127.0.0.1:8765/api/status | jq .`
     - Confirm `/var/lib/aleph-tx-captures/{crypto,rwa}` starts a fresh `W27+` `.ndjson.gz.partial` cleanly without growing `quarantine/` unexpectedly and free disk stays `> 16 GiB`.

### Phase 2: Verify `quant_lakehouse.py` `bbo` Ingestion on First Live `W27+` Window

Once the first full `10h15m` window containing live `bbo` + `-hl-fast-book` rows (`W27+`) seals and transfers cleanly:

1. **Run Bronze -> Silver -> Parquet/DuckDB ingestion** using [quant_lakehouse.py](../scripts/quant_lakehouse.py) against an isolated staging/DuckDB output directory:
   ```sh
   python3 scripts/quant_lakehouse.py normalize \
     <sealed_w27_capture.ndjson.gz> \
     --output-dir <silver_out_dir> \
     --campaign-id <campaign_id> \
     --collector-region tokyo \
     --precise-times

   python3 scripts/quant_lakehouse.py build-duckdb \
     --staging-dir <silver_out_dir> \
     --parquet-root <parquet_out_dir> \
     --db-path <w27_verify.duckdb> \
     --require-duckdb
   ```
2. **Required `v_bbo_quotes` schema & cadence assertions (read-only DuckDB query)**:
   - **Coexistence on `venue = 'arcus'`**: verify both `bbo` rows (`depth_levels_bid IN (0, 1)`, `depth_levels_ask IN (0, 1)`, **`venue_seq IS NOT NULL`**) and `l2Orderbook` rows (`depth_levels_bid > 1` or `depth_levels_ask > 1`, **`venue_seq IS NULL`**) coexist without schema conflict.
   - **Hyperliquid / HIP-3 cadence uplift**: verify `venue IN ('hyperliquid', 'trade_xyz', 'entropy')` contains high-frequency `bbo` rows (`depth_levels <= 1`, `venue_seq IS NULL`) alongside `fast=true` `l2Book` rows, confirming median arrival interval dropped from `~12.4 s` to `~75–120 ms` (and Arcus `~30–40 ms`).
   - **Zero non-monotonic timestamp/sequence regressions** in causal `(available_at_ms, sequence)` ordering.

### Phase 3: Launch Zero-Order `track01_track02_shadow` Against Live Tokyo SHM (`10h` Gate)

Only after Phase 1 & Phase 2 pass and `track01_track02_shadow` is deployed to `/opt/aleph-tx/current/bin/` (note: [install.sh](../deploy/oasis/install.sh) currently installs `arcus_xyz_basis`, `cross_venue`, and `lighter_inventory_mm`; add `track01_track02_shadow` to `install.sh` in a small follow-up or stage the built binary under `/opt/aleph-tx/current/bin/` during the authorized release):

1. **Verify profile validity offline first (`--check`, zero SHM/network)**:
   ```sh
   /opt/aleph-tx/current/bin/track01_track02_shadow --track 1 --config /opt/aleph-tx/current/examples/track01_lead_lag_shadow.toml --check
   /opt/aleph-tx/current/bin/track01_track02_shadow --track 2 --config /opt/aleph-tx/current/examples/track02_shielded_maker_hedge_shadow.toml --check
   ```
2. **Run in read-only, zero-credential, zero-order shadow mode** alongside `aleph-tx-feeder` (`ALEPH_SHM_DIR=/run/aleph-tx-market`, `bbo` shared memory file per [aleph-tx-feeder.service](../deploy/oasis/aleph-tx-feeder.service)):
   ```sh
   # Track 01 shadow observation (36000s = 10h continuous unit):
   /opt/aleph-tx/current/bin/track01_track02_shadow \
     --track 1 \
     --config /opt/aleph-tx/current/examples/track01_lead_lag_shadow.toml \
     --shm-path /run/aleph-tx-market/bbo_matrix.shm \
     --duration-secs 36000
   # Track 02 shadow observation (USDC/USDC Cohort B selected scope: lighter>arcus ETH/QQQ/NVDA):
   /opt/aleph-tx/current/bin/track01_track02_shadow \
     --track 2 \
     --config /opt/aleph-tx/current/examples/track02_shielded_maker_hedge_shadow.toml \
     --shm-path /run/aleph-tx-market/bbo_matrix.shm \
     --duration-secs 36000
   ```
3. **Continuous `10h` shadow acceptance criteria (required before ANY live canary proposal)**:
   - **Zero order / cancel emission**: shadow mode stays read-only with zero exchange credentials and zero REST/WS order writes.
   - **Live SHM latency & freshness**: verify SHM read latency and quote freshness match expected `bbo` cadence without unexpected stale-book halts.
   - **Shield trigger rate & fail-closed behavior**: verify `RefCancelShield` triggers cleanly on external reference moves and fails closed (`fail_closed_when_unready = true`) on unready/stale/future reference timestamps.
   - **Risk invariants**: verify `DailyLossGuard` state transitions remain healthy and **`unhedged_stale_halts == 0`** over a full continuous `10h` window.
   - Only after a clean continuous `10h` shadow record (and separate Track 01/02 budgets per `AGENTS.md`) may a gated live canary proposal be prepared for user authorization.
