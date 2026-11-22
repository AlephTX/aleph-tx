# Arcus <-> Hyperliquid HIP-3 (`xyz`) Basis Arbitrage Runbook

Dedicated operational guide for Step 1 (public latency & preflight verification), Step 2 (pre-registered zero-tuning OOS Go/No-Go gate), and Step 3 (`arcus_xyz_basis` graduated Canary & `oasis-tokyo` install).

## Safety & Separation Rules

- Keep live tracks `HOLD` until Step 2 Monday RTH OOS validation passes (`verdict == "GO"`).
- Never store or print signing material, recovery phrases, signed payloads, or raw account identifiers.
- `config.arcus_xyz_basis.example.toml` ships with `dry_run = true`. Live `--canary` execution fails closed unless BOTH `dry_run = false` in your local profile AND `--confirm-live-canary` is passed explicitly.
- On startup and every order cycle, `BasisJournal` (`state/arcus_xyz_basis_journal.ndjson`) fsyncs `IntentOpen` before dispatch and blocks restart until unresolved intents are reconciled via `IntentTerminal`.
- Note on equity accounting: `ArcusInfo` exposes `/v1/positions`, `/v1/openOrders`, and `/v1/fills` (no cash-wallet balance endpoint). Live `DailyLossGuard` tracks a hybrid budget equal to `allocated_capital_usd + reported_hl_multi_dex_equity_delta + estimated_arcus_fill_mid_pnl`, keeping reported HL equity deltas and estimated Arcus realized/unrealized-fee PnL explicitly distinct in cycle logs.

---

## Step 1: Read-Only Public Probe & Live-Key Preflight

1. **Offline profile & topology check**:

```bash
target/release/arcus_xyz_basis --check config.arcus_xyz_basis.example.toml
```

2. **Public read-only RTT (`p50`/`p95`/`min` ms), spread, L1–L3 depth, `growthMode`, and funding snapshot**:

```bash
# On Mac:
target/release/arcus_xyz_basis --probe-public --samples 5

# On oasis-tokyo (via IAP tunnel with sitepackages flag):
CLOUDSDK_PYTHON_SITEPACKAGES=1 gcloud compute ssh oasis-tokyo \
  --zone=asia-northeast1-b --tunnel-through-iap --command \
  "/opt/aleph-tx/current/bin/arcus_xyz_basis --probe-public --samples 10"
```

3. **Zero-credential shadow signal loop** (runs `ArcusXyzBasisEngine` against live books without orders or keys):

```bash
target/release/arcus_xyz_basis --shadow config.arcus_xyz_basis.example.toml --duration-secs 30
```

4. **Read-only live credential & journal preflight** (requires `/etc/aleph-tx/arcus_xyz_basis.env` on host):

Expected environment variables in `/etc/aleph-tx/arcus_xyz_basis.env` (`chmod 640`, `root:aleph-tx-trader`, never in Git):

- `ARCUS_ACCOUNT_ADDRESS`
- `ARCUS_ACCOUNT_INDEX` (optional, default `0`)
- `ARCUS_API_PRIVATE_KEY` (32-byte Ed25519 hex key)
- `HYPERLIQUID_ACCOUNT_ADDRESS` (master wallet address validated by `HYPERLIQUID_AGENT_PRIVATE_KEY`)
- `HYPERLIQUID_VAULT_ADDRESS` (optional subaccount / vault `0x` address; when set, orders, equity, and `xyz` positions route to the vault/subaccount while agent authorization is validated against the master wallet)
- `HYPERLIQUID_AGENT_PRIVATE_KEY` (approved Hyperliquid agent secp256k1 hex key)

To audit your unified `.env.all_exchanges` (zero secret values printed), split least-privilege runtime env files (`arcus_xyz_basis.env`, `live.env`, `monitor.env`), and run the read-only 7-venue readiness check:

```bash
python3 scripts/onboard_exchange_env.py audit --env .env.all_exchanges
python3 scripts/onboard_exchange_env.py split --env .env.all_exchanges --out-dir /tmp/aleph_env_split
python3 scripts/verify_venue_readiness.py --monitor-env /tmp/aleph_env_split/monitor.env --live
target/release/arcus_xyz_basis --canary-preflight /etc/aleph-tx/arcus_xyz_basis.toml --env /etc/aleph-tx/arcus_xyz_basis.env
```

---

## Step 2: Pre-Registered Zero-Tuning OOS Validation Gates (`scripts/run_oos_validation_gate.py`)

Run strictly once per sealed capture window with zero parameter changes:

1. **Monday US Equities RTH `Arcus ANT-USD <-> xyz:ANT` (`EXP-ARB-A2` + Leung-Li `35 bp` + `CRASH-IMP-01` `5%` brake)**:

```bash
# Before decision_start (07:00 ET = 12:00 UTC), freeze prereg manifest:
python3 scripts/rth_ant_prereg_tool.py prereg \
  --rth-date 2026-11-23 \
  --window-start-et 07:00 \
  --output /tmp/monday_rth_prereg_manifest.json

# Create RFC 3161 timestamp query, submit to your trusted TSA, and wrap the .tsr reply before decision_start:
python3 scripts/rth_ant_prereg_tool.py witness-query \
  --prereg-manifest /tmp/monday_rth_prereg_manifest.json \
  --output /tmp/monday_rth_prereg.tsq
# Submit /tmp/monday_rth_prereg.tsq to TSA -> /tmp/monday_rth_prereg.tsr, then wrap:
python3 scripts/rth_ant_prereg_tool.py wrap-witness \
  --prereg-manifest /tmp/monday_rth_prereg_manifest.json \
  --tsr /tmp/monday_rth_prereg.tsr \
  --output /tmp/monday_rth_external_witness.json

# After decision_end (17:00 ET = 22:00 UTC), seal completed capture segments & coverage:
python3 scripts/rth_ant_prereg_tool.py seal \
  --prereg-manifest /tmp/monday_rth_prereg_manifest.json \
  --capture-dirs ~/aleph-tx-lakehouse/research_captures/<MONDAY_RTH_CAPTURE_DIR> \
  --output /tmp/monday_rth_seal_receipt.json

# Run fail-closed OOS validation gate with explicit trusted TSA CA PEM (never embedded in witness JSON):
python3 scripts/run_oos_validation_gate.py rth-ant \
  --capture-dirs ~/aleph-tx-lakehouse/research_captures/<MONDAY_RTH_CAPTURE_DIR> \
  --role monday_rth_oos \
  --prereg-manifest /tmp/monday_rth_prereg_manifest.json \
  --seal-receipt /tmp/monday_rth_seal_receipt.json \
  --external-witness /tmp/monday_rth_external_witness.json \
  --trusted-tsa-ca /path/to/trusted_tsa_ca.pem \
  --output /tmp/rth_ant_oos_gate.json
```

Pass criteria (`verdict == "GO"` requires BOTH metric checks AND fail-closed OOS provenance checks):
- `all_taker_latencies_positive`: `net_cash_usd > 0` across all `100 ms`, `250 ms`, `500 ms` `TAKER_TAKER` runs (`Q = $500`, `in = 10 bp`, `out = 0 bp`, `lad = 2, p = 1`, `ddbrk = $50`)
- `all_terminal_flat`: `terminal_flat == true` across all gate arms (including stale terminal book / incomplete drain checks)
- `zero_missing_funding_witness`: `funding_hours_missing_witness == 0`
- `max_drawdown_within_budget`: max `max_drawdown_usd <= $50.00`
- `sufficient_cycles_and_win_rate`: min `cycles_closed >= 3` and min `win_rate >= 0.60`
- `provenance.qualified == true` (fail-closed provenance verification for `monday_rth_oos`):
  - **Independent RFC 3161 TSA preregistration witness**: `--external-witness` (`aleph_tx_external_prereg_witness_v1`) + `--trusted-tsa-ca` must cryptographically verify via `openssl ts -verify` against `prereg_manifest_sha256` with the latest possible creation time (`genTime + accuracy`) at or before the decision start, advancing `pre_registration_clock_trust` to `verified_rfc3161_tsa_witness` and `checks.independent_preregistration_witness_verified = true`. The reported `witness_time_ms` is the nominal signed time truncated to milliseconds. The comparison uses the complete signed fraction and the token’s declared accuracy, rounded up to the decision clock. A token with unspecified accuracy needs an independently established policy bound, which this verifier does not yet ingest. Any omitted/invalid witness, untrusted CA, digest mismatch, or post-start timestamp fails closed (`witness_time_ms = null`, `pre_registration_clock_trust = self_asserted_json_only_requires_external_witness`). `provenance.structural_checks_passed` continues to report the structural receipt, clock, coverage, and hash checks independently; use `selection_dev_dryrun` for diagnostic replay.
  - **Split two-receipt chain**: pre-capture `--prereg-manifest` (`classification == "monday_rth_oos"`, `frozen_at_ms <= decision_start_ms`, zero post-hoc `segment_sha256` keys) + post-capture `--seal-receipt` (`sealed_at_ms >= decision_end_ms`, matching `prereg_manifest_sha256`)
  - **Exact bounded 10h RTH replay**: engine runs strictly inside half-open `[decision_start_ms, decision_end_ms)` (`== 10h` span covering Monday US RTH `09:30–16:00 ET`), excluding warmup (`< decision_start_ms`) and post-window (`>= decision_end_ms`) records, with `>= 9` witnessed interior UTC funding ticks (`missing == 0`)
  - **Dual-source continuity & integrity**: both `arcus_book` (`ANT-USD`) and `xyz_book` (`xyz:ANT`) independently cover `[decision_start_ms, decision_end_ms)` with `<= 5 min` gaps (`xyz_funding` gap `<= 1h`), consistent with `seal_receipt.measured_source_coverage`, plus exact SHA-256 verification of all sealed `tokyo-research-*.seg*.ndjson.gz` files (`seal_receipt.segment_sha256`) and the versioned replay engine (`scripts/ant_rth_replay.py`). The preregistered strategy contract must equal the parameters admitted by this gate. Both the replay and live runner reject executable entry edges above 35 bp. The weekday override produces a diagnostic manifest. Preserve older manifests and seal receipts with their original engines. A validation run on this version needs a new preregistration before its untouched window. CLI `prereg`/`seal` always stamp live wall-clock time (no `--now-ms` CLI override), and the already-frozen Monday `2026-11-23` (`2a7d2242...`) manifest runs via its isolated legacy runner.

2. **`W15+` Core Standard RWA cost validation (`<= $20/M`)**

```bash
# Diagnostic mode on an unsealed or historical summary:
python3 scripts/run_oos_validation_gate.py w15-rwa --role diagnostic \
  --rwa-summary-json .agent-state/research/ant_three_venue_20261120/core_rwa_maker_w15.json \
  --output /tmp/w15_rwa_gate.json

# Bounded 10h simulation emitting formal_replay_receipt (`aleph_tx_w15_rwa_replay_receipt_v1`):
python3 scripts/sim_lighter_core_rwa_maker.py \
  --input <CAPTURE_OR_SEGMENT.ndjson.gz> \
  --decision-start-ms <START_MS> \
  --decision-end-ms <END_MS> \
  --emit-formal-receipt \
  --output /tmp/w15_rwa_formal_summary.json

# Check the v1 receipt and retain the diagnostic result for the bounded simulation.
python3 scripts/run_oos_validation_gate.py w15-rwa --role formal \
  --rwa-summary-json /tmp/w15_rwa_formal_summary.json \
  --seal-receipt /tmp/w15_seal_receipt.json \
  --output /tmp/w15_rwa_formal_gate.json
```

The diagnostic report always uses `DIAGNOSTIC_ONLY` and records its `diagnostic_verdict` and applied threshold. The default formal role returns `FORMAL_WITHHELD` (exit 2) for legacy inputs and for internally consistent v1 receipts. A valid v1 receipt covers every segment in the supplied seal exactly once. It records the method present at replay time and the requested ten-hour duration. Formal admission requires an independent pre-window record that fixes the actual bounds and method, plus a seal and measured source coverage tied to that record. The v1 report preserves receipt verification, both economic arms and their diagnostic verdict. Invalid v1 receipt checks return `NO_GO` (exit 1).

Generate bounded research summaries with the simulation command above. The summary carries exact decimal terminal cash and maker turnover alongside the display values. For older summaries the gate evaluates the full rounding interval and records `insufficient_precision` if it straddles the cost limit.

3. **Frozen `W17+` `crash_pause` A/B/C artifact integrity verification**:

```bash
python3 scripts/run_oos_validation_gate.py w17-crash-pause-verify \
  --prereg .agent-state/research/ant_three_venue_20261120/crash_pause_abc_preregistration_v1_frozen.json
```

---

## Step 3: Graduated Live Canary Execution (Only After Step 2 `GO`)

1. Copy/install release artifacts on `oasis-tokyo` (includes `arcus_xyz_basis` and manual-start `aleph-tx-arcus-xyz.service` via `deploy/oasis/install.sh`).
2. Create host config `/etc/aleph-tx/arcus_xyz_basis.toml` from `config.arcus_xyz_basis.example.toml` with `dry_run = false`, keeping conservative Canary caps (`clip_usd = 50.0`, `max_notional_usd = 100.0`, `canary_max_order_usd = 50.0`, `canary_max_total_notional_usd = 100.0`), and place mode `0640` `root:aleph-tx-trader` credentials at `/etc/aleph-tx/arcus_xyz_basis.env`.
3. Re-run `--canary-preflight`, then start explicitly via systemd (never enabled on boot) or CLI confirmation flag:

```bash
# Option A: manual systemd unit (integrates with dashboard status, logs, alerts, and emergency stop):
sudo systemctl start aleph-tx-arcus-xyz.service

# Option B: foreground CLI test with explicit confirmation:
/opt/aleph-tx/current/bin/arcus_xyz_basis \
  --canary /etc/aleph-tx/arcus_xyz_basis.toml \
  --env /etc/aleph-tx/arcus_xyz_basis.env \
  --confirm-live-canary
```

Operational notes for live `aleph-tx-arcus-xyz.service`:
- Normal admission requires zero open orders and exactly flat positions on both configured markets. The live process holds exclusive locks on the journal and both venue accounts. Linux CLI and systemd use the same account lock directory under `/var/lib/aleph-tx-arcus-xyz/locks`. Run the foreground CLI as the service user with the same working directory and state files. The runner retains a durable path binding for the account pair and market, and rejects a later launch against different state files.
- The private `.basis` file adjacent to `daily_loss_state_path` contains the original HL equity reference and cumulative Arcus fill ledger. Preserve both files across restarts. A legacy guard without its matching checkpoint requires manual risk reconciliation. Each valuation uses new books and account reads with checked timestamps and positions. The runner persists verified fills and the UTC guard before clearing the intent barrier.
- Entry, paired exit and residual orders are clipped to the protected leg values and checked again on normalized orders. Small reduce-only exits still respect venue lots. Both signed requests are prepared locally before paired dispatch. After the durable intent write, shutdown, lease and data age are checked again. An intent blocked at this point receives `intent_not_sent`. Previously dispatched orders still require terminal proof.
- The console reports combined trade PnL as unknown while the Arcus primary fill ledger is absent from telemetry. Hedge PnL and strategy equity change are reported separately.

- Transient REST polling errors are tolerated up to `12` consecutive failed polls before running `handle_canary_shutdown` and failing closed. `CancelLease` faults/expiries enter `defense_regime = "reduce_only"` immediately and attempt bounded recovery (`cancel_all` + empty open-orders check + dual-venue reconciliation + fresh lease renewal), failing closed into `defense_regime = "halt"` after `3` consecutive recovery failures. Entry/residual sizing enforces both HL (`$10` non-reduce-only notional / `sz_decimals` lot) and Arcus (`min_order_size` / `min_order_notional`) constraints; an unhedgeable residual (`< 1 lot`, `< $10` non-reduce-only, or `3` consecutive HL rejections) triggers an immediate `halt` shutdown. Book freshness validates both per-leg local receipt age and venue source timestamps (`arc.source_ms` / `xyz.bbo.timestamp_ms`) with configurable clock-skew tolerance. Post-dispatch venue reconciliation requires authoritative per-leg terminal proof (`client_id`/`order_id` status + Arcus history/fills & HL clearinghouse watermark `hl_account_time_ms >= terminal_at_ms`), treating Arcus HTTP `202` as non-terminal and Hyperliquid `Resting` IOC as fatal so unresolved intents remain armed in `BasisJournal`.
- Every matched Arcus fill must respect the journaled IOC limit (`buy <= limit`, `sell >= limit`), and duplicate `trade_id` records must agree on size and price (conflicting duplicates fail closed). Hyperliquid `FilledAck` average price must likewise be positive and respect the journaled IOC limit (`buy <= limit`, `sell >= limit`). An out-of-limit or conflicting fill keeps the intent unresolved even when quantity and position delta agree.
- Hyperliquid fill acknowledgements must name the same venue order as the terminal lookup, and `open_orders` must exclude matching `cloid` and order IDs from the acknowledgement or lookup. Arcus checks its open orders against the order ID from matched history even when the submission and open-order row omit the venue order ID and client ID respectively. Arcus direct/history order records matching the intent must agree on `market_id`, `client_id`, immutable attributes, and terminal state (retrying if one source is still non-terminal). Matched Arcus fills must carry a non-empty `trade_id`, match `market_id` and `order_id`, and (when present) match the journaled `client_id`. Conflicting identities keep the intent unresolved.
- On `SIGTERM` (`systemctl stop` or dashboard emergency stop) / `SIGINT`, the runner cancels open Arcus orders, checks open orders + positions on both venues via `classify_shutdown`, writes `CleanShutdown` ONLY when cancel succeeded and both venues are confirmed `0` open orders / `0` position (otherwise recording `ShutdownUnverified`, where `cancel_unknown` / `open_orders_remaining` stay sticky across subsequent restarts until resolved), and flushes `TelemetrySink` (`StrategySnapshot` + `HedgeFillRecord` using fill avg price when present).
- Dashboard `/api/status`, `/api/alerts` (`arcus_live_failed` + telemetry/drawdown/imbalance gating), `/api/logs?unit=aleph-tx-arcus-xyz`, and the emergency stop button monitor `aleph-tx-arcus-xyz.service` without interfering when `inactive`.
