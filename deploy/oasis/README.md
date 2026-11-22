# AlephTX on Oasis (`oasis-tokyo`)

`oasis-tokyo` (`asia-northeast1-b`, replacing legacy Taiwan `oasis` in `asia-east1-b`) runs the low-latency multi-venue public feeder, three physically isolated 24/7 rolling `10h15m` (`36900s`) public capture streams (`aleph-perp-capture-{crypto,rwa,bulk}.service` writing to `/var/lib/aleph-tx-captures/{crypto,rwa,bulk}`), and a loopback-only dashboard as systemd services. Shared memory (`aleph-matrix`, 12 exchange slots) carries Lighter Core, Lighter Robinhood, Arcus, Hyperliquid Core, Entropy (`io:*`), TradeXYZ (`xyz:*`), and Binance USD-M reference perpetual quotes simultaneously (`deploy/oasis/feeder.toml`); optional RWA/auxiliary reference slots do not raise false weekend quote-stall alerts while closed. Idle-priority flywheel units (`aleph-tx-flywheel.service` / `aleph-tx-flywheel.timer`) and the rolling runner (`scripts/perp_rolling_runner.py`) automatically seal `.bronze_manifest.json` on every completed `wNN` window, upload under `SCHED_IDLE` + `ionice -c 3` (`--bwlimit 3M`) once `ALEPH_PUBLIC_BRONZE_REMOTE` is configured in `/etc/aleph-tx/flywheel.env`, and evict SHA-256-verified older windows when local free disk drops below `16 GiB`. The strategy binaries are installed, and both `aleph-tx-live.service` (`lighter_inventory_mm`) and `aleph-tx-arcus-xyz.service` (`arcus_xyz_basis`) are available for manual operation. This deployment starts the feeder and dashboard only.

From your Mac, run `bash deploy/oasis/dashboard.sh` (override `OASIS_HOST`, `OASIS_ZONE`, `OASIS_SSH_TARGET`, or `OASIS_GCP_PROJECT` if needed) and open [the dashboard](http://127.0.0.1:8765/). The Google Cloud CLI connects via IAP, and SSH forwards the remote loopback port to the local loopback port. No dashboard port is exposed on the VM's public interface. Stop the command with Ctrl+C when finished. If a tunnel is already running, open the dashboard directly.

The console (`dashboard/`, Vite + React 19 + TypeScript) organizes monitoring into four accessible workspaces: overview and risk guards (persisted UTC daily loss breaker, EWMA post-fill markout quality, and per-session unit cost per `$1M` gross turnover), orders and dual-leg fills (`fills` and `hedge_fills` with IOC price/fee provenance badges), a sortable multi-session matrix with one-click session pinning (`?session=<id>`), and shared-memory market basis alongside journal logs. Status, orders, and fills refresh every three seconds; quotes older than 15 seconds are labelled delayed. A failed account request is shown separately from a successful request with no open positions.

## Telemetry, retention and history

When `ALEPH_TELEMETRY_DIR` is set, live strategies (`lighter_inventory_mm`, `arcus_xyz_basis`) append one JSONL file per session (orders, fills, risk snapshots) to `/var/lib/aleph-tx-telemetry`. Writes go through a bounded channel off the trading path; dropped records are counted and shown on the console. Closed JSONL files expire after 7 days (`systemd-tmpfiles` via `aleph-tx-telemetry.conf`), and `python3 /opt/aleph-tx/current/telemetry_store.py prune --dir /var/lib/aleph-tx-telemetry --max-age-days 7 --max-mb 200 --db /var/lib/aleph-tx-monitor/telemetry.sqlite3 --retention-days 14` enforces a `200 MB` JSONL cap plus SQLite pruning. The monitor ingests those files incrementally into SQLite at `/var/lib/aleph-tx-monitor/telemetry.sqlite3` and prunes stopped sessions older than `ALEPH_MONITOR_RETENTION_DAYS` (default `14` days) hourly, so history survives restarts while bounding disk growth. PnL uses average cost, includes fees and rebates, and marks open inventory to the latest snapshot mid.

API: `/api/status`, `/api/summary?session=`, `/api/alerts`, `/api/orders?status=open|all&limit=&session=`, `/api/fills?limit=&before=&session=`, `/api/hedge_fills?limit=&before=&session=`, `/api/pnl?bucket=1m|5m|15m|1h|1d&since=&session=`, `/api/sessions?limit=`, `/api/logs?unit=aleph-tx-live|aleph-tx-arcus-xyz|aleph-tx-feeder|aleph-tx-monitor`.

Lighter trade PnL covers Lighter fills, fees and rebates. Each Lighter fee
is the rate its trade declares for the account's role times price and size,
so the console labels it an estimate, not the amount charged. Every fill
records a `fee_source`: `rate_derived`, `explicit_zero`, `missing`
(Lighter omits zero fields, so an absent rate is not a known zero),
`invalid` or `role_unknown`. Fills from older feeders, and rows ingested
before this field existed, are `null`. Any fill without a declared rate
makes the session's Lighter fee total unknown; fee numbers are unchanged.
When the inventory
hedge is enabled, each reconciled Hyperliquid IOC is exported as a
`hedge_fill` record after it is journaled, and tracked as a separate leg with
its own average cost and an estimated taker fee. Its price is the venue's
average fill price, or the journaled IOC limit when that is unavailable. The
console's combined PnL marks both legs to the same reference mid; funding
payments are not included. Lighter account equity is shown separately. A successful
strategy shutdown drains the telemetry queue and persists the final marker
before returning. Ingest preserves checkpoints when session files are copied
or restored to new inodes, and deduplicates each record before updating totals.

## Alerts

The monitor evaluates alert rules every 15 seconds: feeder down, live unit failed (`aleph-tx-live` or `aleph-tx-arcus-xyz`), stale or missing quotes while `aleph-tx-live` is active, stale telemetry, private-stream gaps, dropped telemetry records, daily drawdown approaching the loss breaker, cross-venue imbalance while hedged, and host free disk space (`disk_space_low`, warning `< 8 GiB`, critical `< 3 GiB`). Daily drawdown uses the strategy's
persisted UTC-day equity baseline and configured `max_daily_loss_pct` from
telemetry. It warns at 60% and turns critical at 90% of that limit. Snapshots
without a baseline for the current UTC day cannot drive this alert. Active alerts appear
as banners in the console and at `/api/alerts`. To also receive a JSON
`{"text": ...}` POST when an alert is raised, changes severity, or resolves, set
`ALEPH_MONITOR_ALERT_WEBHOOK` in `/etc/aleph-tx/monitor.env` and restart the
monitor. Alerts never act on trading.

## Emergency stop

The console's stop button sends `systemctl stop --no-block aleph-tx-live.service aleph-tx-arcus-xyz.service`; each strategy cancels open orders and reconciles positions on `SIGTERM` / `SIGINT`. A polkit rule allows user `aleph-tx` to stop those two units and nothing else; there is no start control. `install.sh` generates the control token once in `/etc/aleph-tx/monitor.env` (mode 0600). Read it with `sudo grep CONTROL_TOKEN /etc/aleph-tx/monitor.env`; the browser keeps it only for the tab session. Remove the line and restart the monitor to disable the button.

## Add read-only positions

On Oasis, extract public-only identifiers directly from a single local `.env` without duplicating or copying any private keys using `deploy/oasis/sync_monitor_env.py`:

```sh
python3 /opt/aleph-tx/current/sync_monitor_env.py \
  --source /etc/aleph-tx/.env.all_exchanges \
  --output /etc/aleph-tx/monitor.env
sudo systemctl restart aleph-tx-monitor.service
```

Supported public variables in `/etc/aleph-tx/monitor.env` (no API/signing keys; preserves existing `ALEPH_MONITOR_CONTROL_TOKEN` and webhook settings):

```dotenv
ALEPH_MONITOR_LIGHTER_INDEX=<numeric Lighter Core account index>
ALEPH_MONITOR_LIGHTER_RH_INDEX=<numeric Lighter Robinhood account index>
ALEPH_MONITOR_HL_ADDRESS=<Hyperliquid 0x address (also used for TradeXYZ & Entropy unless overridden)>
ALEPH_MONITOR_HL_XYZ_ADDRESS=<optional separate xyz 0x address>
ALEPH_MONITOR_HL_IO_ADDRESS=<optional separate io 0x address>
ALEPH_MONITOR_ARCUS_ADDRESS=<Arcus 0x address>
ALEPH_MONITOR_ARCUS_INDEX=<Arcus index 0..9, default 0>
ALEPH_MONITOR_BULK_PUBKEY=<BULK Solana Base58 pubkey>
```

The monitor polls these public read-only endpoints (`Lighter`, `LighterRH`, `Hyperliquid`, `TradeXYZ`, `Entropy`, `Arcus`, `BULK`) about every 20 seconds without private keys, leaving trading latency and rate limits untouched. Apply changes with `sudo systemctl restart aleph-tx-monitor.service`.

## Operations

- `systemctl status aleph-tx-feeder aleph-tx-monitor aleph-tx-live aleph-tx-arcus-xyz`
- For console development, keep the tunnel open and run `make dashboard-dev`; Vite proxies `/api` to `127.0.0.1:8765` (override with `ALEPH_API`).
- `sudo journalctl -u aleph-tx-feeder -u aleph-tx-monitor -n 80 --no-pager`
- `/opt/aleph-tx/current/release.json` records the source bundle hash and base commit; the bundle can include uncommitted workspace changes.
- For deployment, run `deploy/oasis/bootstrap.sh` as the SSH user if the host needs tools. Run `make dashboard-build` locally (Node 22.12+ or 24+) so the snapshot contains `dashboard/dist`. Build the credential-free source snapshot on Linux x86-64 with `RUSTUP_TOOLCHAIN=1.100.0 make build`, then run `deploy/oasis/install.sh RELEASE SHA256 COMMIT` from the snapshot. Use a release ID of the form `YYYYMMDD-<12 lowercase hexadecimal characters>`.
- Keep live configuration separate from the monitor. The live units (`aleph-tx-live.service` with `/etc/aleph-tx/live.{toml,env}` and `aleph-tx-arcus-xyz.service` with `/etc/aleph-tx/arcus_xyz_basis.{toml,env}`) are manual and require files readable by `aleph-tx-trader`; use group-readable, non-world-readable credential files (`chmod 640`, owner `root:aleph-tx-trader`). Review the live trading profile and account before starting either unit. Starting either unit can place and cancel real orders.
