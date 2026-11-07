# AlephTX on Oasis

Oasis (`asia-east1-b`) runs the public feeder and a loopback-only dashboard as systemd services. The Lighter ETH and Hyperliquid ETH/BTC perpetual quotes come from the feeder's shared memory. The strategy binaries are installed, and `aleph-tx-live.service` is available for manual operation. This deployment starts the feeder and dashboard only.

From your Mac, run `bash deploy/oasis/dashboard.sh` and open [the dashboard](http://127.0.0.1:8765/). The Google Cloud CLI connects via IAP, and SSH forwards the remote loopback port to the local loopback port. No dashboard port is exposed on the VM's public interface. Stop the command with Ctrl+C when finished. If a tunnel is already running, open the dashboard directly.

The console (`dashboard/`, Vite + React) shows service state, quotes, exchange positions, open orders, fills, PnL and equity charts, per-session history, and journal logs. Status, orders, and fills refresh every three seconds; quotes older than 15 seconds are labelled delayed. A failed account request is shown separately from a successful request with no open positions.

## Telemetry and history

When `ALEPH_TELEMETRY_DIR` is set, `lighter_inventory_mm` appends one JSONL file per session (orders, fills, risk snapshots) to `/var/lib/aleph-tx-telemetry`. Writes go through a bounded queue off the trading path; dropped records are counted and shown on the console. The monitor ingests those files incrementally into SQLite at `/var/lib/aleph-tx-monitor/telemetry.sqlite3`, so history survives restarts and the database can be deleted and rebuilt from the JSONL files. PnL uses average cost, includes fees and rebates, and marks open inventory to the latest snapshot mid.

API: `/api/status`, `/api/summary`, `/api/alerts`, `/api/orders?status=open|all`, `/api/fills?limit=&before=&session=`, `/api/hedge_fills?limit=&before=&session=`, `/api/pnl?bucket=1m|5m|1h|1d&since=`, `/api/sessions`, `/api/logs?unit=aleph-tx-live|aleph-tx-feeder|aleph-tx-monitor`.

Lighter trade PnL covers Lighter fills, fees and rebates. When the inventory
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

The monitor evaluates alert rules every 15 seconds: feeder down, live unit
failed, stale or missing quotes while live, stale telemetry, private-stream
gaps, dropped telemetry records, session drawdown approaching the daily loss
breaker (warning at 60%, critical at 90% of `ALEPH_MONITOR_MAX_DAILY_LOSS_PCT`,
default `0.05`), and a cross-venue imbalance while hedged. Active alerts appear
as banners in the console and at `/api/alerts`. To also receive a JSON
`{"text": ...}` POST when an alert is raised or resolved, set
`ALEPH_MONITOR_ALERT_WEBHOOK` in `/etc/aleph-tx/monitor.env` and restart the
monitor. Alerts never act on trading.

## Emergency stop

The console's stop button sends `systemctl stop aleph-tx-live.service`; the strategy cancels its orders and checks flat on SIGTERM. A polkit rule allows user `aleph-tx` to stop that one unit and nothing else; there is no start control. `install.sh` generates the control token once in `/etc/aleph-tx/monitor.env` (mode 0600). Read it with `sudo grep CONTROL_TOKEN /etc/aleph-tx/monitor.env`; the browser keeps it only for the tab session. Remove the line and restart the monitor to disable the button.

## Add read-only positions

On Oasis, use `sudoedit /etc/aleph-tx/monitor.env` and set either or both of these variables:

```dotenv
ALEPH_MONITOR_LIGHTER_INDEX=<numeric account index>
ALEPH_MONITOR_HL_ADDRESS=<actual account or subaccount address starting with 0x>
```

Use mainnet identifiers. Hyperliquid reads perpetuals on the default DEX. The monitor polls the exchanges' public account endpoints about every 20 seconds, and requires no API key or signing key. Apply changes with `sudo systemctl restart aleph-tx-monitor.service`. This does not affect the feeder or the trading strategy.

## Operations

- `systemctl status aleph-tx-feeder aleph-tx-monitor aleph-tx-live`
- For console development, keep the tunnel open and run `make dashboard-dev`; Vite proxies `/api` to `127.0.0.1:8765` (override with `ALEPH_API`).
- `sudo journalctl -u aleph-tx-feeder -u aleph-tx-monitor -n 80 --no-pager`
- `/opt/aleph-tx/current/release.json` records the source bundle hash and base commit; the bundle can include uncommitted workspace changes.
- For deployment, run `deploy/oasis/bootstrap.sh` as the SSH user if the host needs tools. Run `make dashboard-build` locally (Node 22.12+ or 24+) so the snapshot contains `dashboard/dist`. Build the credential-free source snapshot on Linux x86-64 with `RUSTUP_TOOLCHAIN=1.95.0 make build`, then run `deploy/oasis/install.sh RELEASE SHA256 COMMIT` from the snapshot. Use a release ID of the form `YYYYMMDD-<12 lowercase hexadecimal characters>`.
- Keep live configuration separate from the monitor. The live unit is manual and requires `/etc/aleph-tx/live.toml` and `/etc/aleph-tx/live.env` readable by `aleph-tx-trader`; use a group-readable, non-world-readable credential file. Review the live trading profile and account before starting it. Starting it can place and cancel real orders.
