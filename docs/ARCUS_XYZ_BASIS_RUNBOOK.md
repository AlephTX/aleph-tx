# Arcus <-> Hyperliquid HIP-3 (`xyz`) Basis Arbitrage Runbook

Dedicated operational guide for Step 1 (public latency & preflight verification), Step 2 (pre-registered zero-tuning OOS Go/No-Go gate), and Step 3 (`arcus_xyz_basis` graduated Canary & `oasis-tokyo` install).

## Safety & Separation Rules

- Keep live tracks `HOLD` until Step 2 Monday RTH OOS validation passes (`verdict == "GO"`).
- Never store or print signing material, recovery phrases, signed payloads, or raw account identifiers.
- `config.arcus_xyz_basis.example.toml` ships with `dry_run = true`. Live `--canary` execution fails closed unless BOTH `dry_run = false` in your local profile AND `--confirm-live-canary` is passed explicitly.
- On startup and every order cycle, `BasisJournal` (`state/arcus_xyz_basis_journal.ndjson`) fsyncs `IntentOpen` before dispatch and blocks restart until unresolved intents are reconciled via `IntentTerminal`.
- Note on equity accounting: `ArcusInfo` exposes `/v1/positions`, `/v1/openOrders`, and `/v1/fills` (no cash-wallet balance endpoint), so `DailyLossGuard` tracks live Hyperliquid multi-DEX equity delta relative to initial `allocated_capital_usd` without inventing unverified Arcus wallet cash.

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

4. **Read-only live credential & journal preflight** (requires `/etc/aleph-tx/.env.arcus_xyz` on host):

Expected environment variables in `.env.arcus_xyz` (`chmod 600`, root/trader-only, never in Git):

- `ARCUS_ACCOUNT_ADDRESS`
- `ARCUS_ACCOUNT_INDEX` (optional, default `0`)
- `ARCUS_API_PRIVATE_KEY` (32-byte Ed25519 hex key)
- `HYPERLIQUID_ACCOUNT_ADDRESS` (master/subaccount address)
- `HYPERLIQUID_AGENT_PRIVATE_KEY` (approved Hyperliquid agent secp256k1 hex key)

```bash
target/release/arcus_xyz_basis --canary-preflight /etc/aleph-tx/arcus_xyz_basis.toml --env /etc/aleph-tx/.env.arcus_xyz
```

---

## Step 2: Pre-Registered Zero-Tuning OOS Validation Gates (`scripts/run_oos_validation_gate.py`)

Run strictly once per sealed capture window with zero parameter changes:

1. **Monday US Equities RTH `Arcus ANT-USD <-> xyz:ANT` (`EXP-ARB-A2` + Leung-Li `35 bp` + `CRASH-IMP-01` `5%` brake)**:

```bash
python3 scripts/run_oos_validation_gate.py rth-ant \
  --capture-dirs ~/aleph-tx-lakehouse/research_captures/<MONDAY_RTH_CAPTURE_DIR> \
  --role monday_rth_oos \
  --prereg-manifest <MONDAY_RTH_PREREG_MANIFEST.json> \
  --output /tmp/rth_ant_oos_gate.json
```

Pass criteria (`verdict == "GO"` requires BOTH metric checks AND fail-closed OOS provenance checks):
- `all_taker_latencies_positive`: `net_cash_usd > 0` across all `100 ms`, `250 ms`, `500 ms` `TAKER_TAKER` runs (`Q = $500`, `in = 10 bp`, `out = 0 bp`, `lad = 2, p = 1`, `ddbrk = $50`)
- `all_terminal_flat`: `terminal_flat == true` across all gate arms
- `zero_missing_funding_witness`: `funding_hours_missing_witness == 0`
- `max_drawdown_within_budget`: max `max_drawdown_usd <= $50.00`
- `sufficient_cycles_and_win_rate`: min `cycles_closed >= 3` and min `win_rate >= 0.60`
- `provenance.qualified == true` (fail-closed for `monday_rth_oos`):
  - `--prereg-manifest` supplied with `classification == "monday_rth_oos"` and `frozen_at_ms <= decision_start_ms`
  - Continuous `>= 10h` span covering Monday US RTH (`09:30–16:00 ET`), matching observed `first_rx_ns`/`last_rx_ns` coverage and `>= 9` witnessed interior UTC funding ticks (`missing == 0`)
  - Exact SHA-256 verification of all sealed `tokyo-research-*.seg*.ndjson.gz` files (`segment_sha256`) and frozen ANT engine scripts (`frozen_engine_sha256`: `sim_ant_arcus_xyz_basis.py`, `run_arb_imp_ablation_w01_w02.py`)

2. **`W15+` Core Standard RWA cost gate (`<= $20/M`)**:

```bash
python3 scripts/run_oos_validation_gate.py w15-rwa \
  --rwa-summary-json .agent-state/research/ant_three_venue_20261120/core_rwa_maker_w15.json \
  --output /tmp/w15_rwa_gate.json
```

3. **Frozen `W17+` `crash_pause` A/B/C artifact integrity verification**:

```bash
python3 scripts/run_oos_validation_gate.py w17-crash-pause-verify \
  --prereg .agent-state/research/ant_three_venue_20261120/crash_pause_abc_preregistration_v1_frozen.json
```

---

## Step 3: Graduated Live Canary Execution (Only After Step 2 `GO`)

1. Copy/install release artifacts on `oasis-tokyo` (includes `arcus_xyz_basis` via `deploy/oasis/install.sh`).
2. Create host config `/etc/aleph-tx/arcus_xyz_basis.toml` from `config.arcus_xyz_basis.example.toml` with `dry_run = false`, keeping conservative Canary caps (`clip_usd = 50.0`, `max_notional_usd = 100.0`, `canary_max_order_usd = 50.0`, `canary_max_total_notional_usd = 100.0`).
3. Re-run `--canary-preflight`, then start explicitly with confirmation flag:

```bash
/opt/aleph-tx/current/bin/arcus_xyz_basis \
  --canary /etc/aleph-tx/arcus_xyz_basis.toml \
  --env /etc/aleph-tx/.env.arcus_xyz \
  --confirm-live-canary
```
