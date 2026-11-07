#!/usr/bin/env python3
"""Loopback dashboard API: feeder quotes, services, accounts, strategy telemetry, and stop control."""

import hmac
import json
import math
import os
import re
import socket
import struct
import subprocess
import threading
import time
from datetime import datetime, timezone
from decimal import Decimal
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.error import HTTPError
from urllib.parse import parse_qs, urlsplit
from urllib.request import Request, urlopen

from alerts import Alerter, evaluate
from telemetry_store import TelemetryStore

ROOT = Path(__file__).resolve().parent
MATRIX = "/run/aleph-tx-market/aleph-matrix"
SERVICES = ("aleph-tx-feeder", "aleph-tx-monitor", "aleph-tx-live")
# Layout in feeder/shm/matrix.go; symbol and exchange IDs in feeder/exchanges/common.go.
BBO = struct.Struct("<IBBHQdddd16x")
SEQUENCE = struct.Struct("<I")
MARKETS = (("Lighter", "ETH", 2, 1002), ("Hyperliquid", "ETH", 1, 1002),
           ("Hyperliquid", "BTC", 1, 1001))
ACCOUNT_REFRESH = 20
STATIC = Path(os.environ.get("ALEPH_DASHBOARD_DIR", ROOT / "dashboard")).resolve()
TELEMETRY_DIR = os.environ.get("ALEPH_TELEMETRY_DIR", "/var/lib/aleph-tx-telemetry")
DB_PATH = os.environ.get("ALEPH_MONITOR_DB", "/var/lib/aleph-tx-monitor/telemetry.sqlite3")
CONTROL_TOKEN = os.environ.get("ALEPH_MONITOR_CONTROL_TOKEN", "")
ALERT_WEBHOOK = os.environ.get("ALEPH_MONITOR_ALERT_WEBHOOK", "").strip()
ALERT_INTERVAL = 15
MAX_DAILY_LOSS_PCT = float(os.environ.get("ALEPH_MONITOR_MAX_DAILY_LOSS_PCT", "0.05"))
LIVE_UNIT = "aleph-tx-live.service"
MIME = {".html": "text/html; charset=utf-8", ".js": "text/javascript; charset=utf-8",
        ".css": "text/css; charset=utf-8", ".svg": "image/svg+xml", ".png": "image/png",
        ".ico": "image/x-icon", ".json": "application/json", ".woff2": "font/woff2",
        ".map": "application/json"}
LOG_PRIORITY = {"0": "emerg", "1": "alert", "2": "crit", "3": "error", "4": "warn",
                "5": "notice", "6": "info", "7": "debug"}
ANSI = re.compile(r"\x1b\[[0-9;]*m")


def now_iso():
    return datetime.now(timezone.utc).isoformat(timespec="seconds")


def read_quote(fd, exchange, symbol, now_ns):
    offset = 2048 * 8 + (symbol * 7 + exchange) * BBO.size
    for _ in range(4):
        before = os.pread(fd, SEQUENCE.size, offset)
        raw = os.pread(fd, BBO.size, offset)
        after = os.pread(fd, SEQUENCE.size, offset)
        if len(raw) != BBO.size or len(before) != SEQUENCE.size or len(after) != SEQUENCE.size:
            return None
        seq, msg_type, venue, asset, timestamp, bid, bid_size, ask, ask_size = BBO.unpack(raw)
        if seq & 1 or before != after or raw[:4] != before:
            continue
        if (msg_type != 1 or venue != exchange or asset != symbol or timestamp == 0
                or timestamp > now_ns + 10**9 or bid >= ask
                or not all(math.isfinite(x) and x > 0 for x in (bid, bid_size, ask, ask_size))):
            return None
        age_ms = max(0, (now_ns - timestamp) // 10**6)
        return {"state": "live" if age_ms <= 15000 else "stale", "bid": bid, "ask": ask,
                "mid": bid + (ask - bid) / 2, "spread_bps": (ask - bid) / (bid / 2 + ask / 2) * 10000,
                "age_ms": age_ms}
    return None


def markets():
    fd = None
    try:
        fd = os.open(MATRIX, os.O_RDONLY)
    except FileNotFoundError:
        pass
    try:
        clock = time.time_ns()
        return [{"venue": venue, "symbol": symbol, **(
            (read_quote(fd, exchange_id, symbol_id, clock) if fd is not None else None)
            or {"state": "waiting"})} for venue, symbol, exchange_id, symbol_id in MARKETS]
    finally:
        if fd is not None:
            os.close(fd)


def services():
    keys = ("Id", "LoadState", "ActiveState", "SubState", "NRestarts", "MainPID", "Result")
    try:
        proc = subprocess.run(["systemctl", "show", "--no-pager", "--property=" + ",".join(keys),
                               *(name + ".service" for name in SERVICES)], capture_output=True,
                              text=True, timeout=2, check=False)
    except (OSError, subprocess.TimeoutExpired):
        return {name: {"ActiveState": "unknown"} for name in SERVICES}
    entries = (dict(line.split("=", 1) for line in block.splitlines() if "=" in line)
               for block in proc.stdout.strip().split("\n\n"))
    by_id = {entry.get("Id", "").removesuffix(".service"): entry for entry in entries}
    return {name: by_id.get(name, {"ActiveState": "unknown"}) for name in SERVICES}


def request_json(url, body=None):
    payload = None if body is None else json.dumps(body).encode()
    request = Request(url, data=payload, headers={"Content-Type": "application/json",
                                                  "User-Agent": "AlephTX-monitor/1.0"})
    with urlopen(request, timeout=5) as response:
        return json.loads(response.read(2_000_000))


def decimal_string(value):
    value = Decimal(value)
    if not value.is_finite():
        raise ValueError("invalid number")
    return str(value)


def position(symbol, size, entry, pnl, liquidation):
    size = decimal_string(size)
    return {"symbol": symbol, "size": size, "entry": None if entry is None else decimal_string(entry),
            "unrealized_pnl": decimal_string(pnl),
            "liquidation": None if liquidation is None else decimal_string(liquidation)}


def lighter_positions(index):
    if not re.fullmatch(r"[0-9]{1,19}", index) or int(index) > 2**63 - 1:
        raise ValueError("invalid account index")
    data = request_json(f"https://mainnet.zklighter.elliot.ai/api/v1/account?by=index&value={index}")
    if data["code"] != 200:
        raise ValueError("account query failed")
    account = next(a for a in data["accounts"] if a["index"] == int(index))
    rows = []
    for p in account["positions"]:
        size = Decimal(p["position"])
        if size == 0:
            continue
        if not size.is_finite() or size < 0 or p["sign"] not in (-1, 1):
            raise ValueError("invalid Lighter position")
        rows.append(position(p["symbol"], size if p["sign"] == 1 else size.copy_negate(),
                             p["avg_entry_price"], p["unrealized_pnl"], p.get("liquidation_price")))
    return rows


def hyperliquid_positions(address):
    if not re.fullmatch(r"0x[0-9a-fA-F]{40}", address):
        raise ValueError("invalid wallet address")
    data = request_json("https://api.hyperliquid.xyz/info",
                        {"type": "clearinghouseState", "user": address, "dex": ""})
    return [position(p["coin"], p["szi"], p.get("entryPx"), p["unrealizedPnl"], p.get("liquidationPx"))
            for item in data["assetPositions"] if Decimal((p := item["position"])["szi"]) != 0]


class Accounts:
    def __init__(self):
        self.sources = (("Lighter", os.environ.get("ALEPH_MONITOR_LIGHTER_INDEX", "").strip(), lighter_positions),
                        ("Hyperliquid", os.environ.get("ALEPH_MONITOR_HL_ADDRESS", "").strip(), hyperliquid_positions))
        self.snapshots = {venue: {"state": "waiting" if account else "unconfigured"}
                          for venue, account, _ in self.sources}

    def start(self):
        for venue, account, fetch in self.sources:
            if account:
                threading.Thread(target=self.poll, args=(venue, account, fetch), daemon=True).start()

    def poll(self, venue, account, fetch):
        while True:
            try:
                rows = fetch(account)
                snapshot = {"state": "ready", "positions": rows, "as_of": now_iso()}
            except Exception as exc:
                error = f"HTTP {exc.code}" if isinstance(exc, HTTPError) else type(exc).__name__
                snapshot = {"state": "error", "error": error, "as_of": now_iso()}
            self.snapshots[venue] = snapshot
            time.sleep(ACCOUNT_REFRESH)


def journal_tail(unit, lines):
    """Recent journal lines for one AlephTX unit (requires systemd-journal group)."""
    if unit not in SERVICES:
        raise ValueError("unknown unit")
    proc = subprocess.run(["journalctl", "-u", unit + ".service", "-n", str(lines), "-o", "json",
                           "--no-pager"], capture_output=True, text=True, timeout=4, check=False)
    rows = []
    for line in proc.stdout.splitlines():
        try:
            entry = json.loads(line)
        except ValueError:
            continue
        message = entry.get("MESSAGE")
        if isinstance(message, list):  # journald encodes non-UTF-8 payloads as byte arrays
            message = bytes(message).decode("utf-8", "replace")
        rows.append({"ts_us": int(entry.get("__REALTIME_TIMESTAMP", 0)),
                     "level": LOG_PRIORITY.get(entry.get("PRIORITY", "6"), "info"),
                     "message": ANSI.sub("", str(message or ""))[:2000]})
    return rows


def stop_live():
    """Stop the live unit. The strategy cancels orders and verifies flat on SIGTERM."""
    proc = subprocess.run(["systemctl", "stop", "--no-block", LIVE_UNIT], capture_output=True,
                          text=True, timeout=10, check=False)
    if proc.returncode != 0:
        raise PermissionError(proc.stderr.strip()[:300] or "systemctl stop failed")


def ingest_loop(store):
    while True:
        try:
            store.ingest()
        except Exception as exc:  # keep serving; the next pass retries
            print(f"telemetry ingest failed: {type(exc).__name__}: {exc}", flush=True)
        time.sleep(2)


def alert_loop(store, alerter):
    while True:
        try:
            status = {"services": services(), "markets": markets()}
            alerter.update(evaluate(status, store.summary(), int(time.time() * 1000),
                                    MAX_DAILY_LOSS_PCT))
        except Exception as exc:  # keep serving; the next pass retries
            print(f"alert evaluation failed: {type(exc).__name__}: {exc}", flush=True)
        time.sleep(ALERT_INTERVAL)


class Handler(BaseHTTPRequestHandler):
    server_version = "AlephTX"
    sys_version = ""

    def allowed_host(self):
        # Prevent a third-party website from reading the loopback API via DNS rebinding.
        return re.fullmatch(r"(?:localhost|127\.0\.0\.1)(?::[0-9]{1,5})?", self.headers.get("Host", ""))

    def do_GET(self):
        if not self.allowed_host():
            return self.send_error(403)
        url = urlsplit(self.path)
        query = {key: values[-1] for key, values in parse_qs(url.query).items()}
        try:
            payload = self.api(url.path, query)
        except (ValueError, KeyError) as exc:
            return self.json({"error": str(exc) or "bad request"}, 400)
        if isinstance(payload, tuple):
            return self.json(*payload)
        if payload is not None:
            return self.json(payload)
        return self.static(url.path)

    def api(self, route, q):
        store = self.server.store
        if route == "/api/status":
            return {"host": socket.gethostname(), "checked_at": now_iso(),
                    "release": self.server.release, "services": services(),
                    "markets": markets(), "accounts": self.server.accounts.snapshots.copy(),
                    "control_enabled": bool(CONTROL_TOKEN),
                    "alerts": self.server.alerter.snapshot()}
        if route == "/api/alerts":
            return {"alerts": self.server.alerter.snapshot(), "webhook": bool(ALERT_WEBHOOK)}
        if route == "/api/summary":
            return store.summary()
        if route == "/api/fills":
            return {"fills": store.fills(limit=bounded(q.get("limit"), 200, 1000),
                                         before_ms=int(q["before"]) if "before" in q else None,
                                         session_id=q.get("session"))}
        if route == "/api/hedge_fills":
            return {"fills": store.hedge_fills(limit=bounded(q.get("limit"), 200, 1000),
                                               before_ms=int(q["before"]) if "before" in q else None,
                                               session_id=q.get("session"))}
        if route == "/api/orders":
            status = q.get("status", "open")
            if status not in ("open", "all"):
                raise ValueError("status must be open or all")
            return {"orders": store.orders(status=status, limit=bounded(q.get("limit"), 200, 1000),
                                           session_id=q.get("session"))}
        if route == "/api/pnl":
            bucket = q.get("bucket", "5m")
            if bucket not in store.BUCKETS:
                raise ValueError("unsupported bucket")
            since = int(q.get("since", 0))
            return store.pnl_series(bucket, since)
        if route == "/api/sessions":
            return {"sessions": store.sessions(limit=bounded(q.get("limit"), 50, 500))}
        if route == "/api/logs":
            try:
                rows = journal_tail(q.get("unit", "aleph-tx-live"), bounded(q.get("lines"), 200, 1000))
            except (OSError, subprocess.TimeoutExpired) as exc:
                return {"error": f"journal unavailable: {exc}"}, 503
            return {"logs": rows}
        if route.startswith("/api/"):
            return {"error": "unknown endpoint"}, 404
        return None

    def do_POST(self):
        if not self.allowed_host():
            return self.send_error(403)
        if urlsplit(self.path).path != "/api/live/stop":
            return self.send_error(404)
        if not CONTROL_TOKEN:
            return self.json({"error": "control disabled"}, 403)
        # A custom header forces a CORS preflight, which this server never approves.
        supplied = self.headers.get("X-Aleph-Control", "")
        if not hmac.compare_digest(supplied.encode(), CONTROL_TOKEN.encode()):
            return self.json({"error": "invalid control token"}, 403)
        try:
            stop_live()
        except (OSError, subprocess.TimeoutExpired, PermissionError) as exc:
            return self.json({"error": str(exc)}, 500)
        print("live strategy stop requested from dashboard", flush=True)
        return self.json({"ok": True, "requested_at": now_iso()}, 202)

    def static(self, route):
        target = (STATIC / route.lstrip("/")).resolve()
        if not target.is_relative_to(STATIC) or not target.is_file():
            target = STATIC / "index.html"  # SPA fallback for client-side routes
            if not target.is_file():
                return self.send_error(404)
        immutable = "/assets/" in route
        return self.respond(target.read_bytes(), MIME.get(target.suffix, "application/octet-stream"),
                            cache="public, max-age=31536000, immutable" if immutable else "no-store")

    def json(self, payload, status=200):
        return self.respond(json.dumps(payload, allow_nan=False, default=str).encode(),
                            "application/json", status=status)

    def respond(self, payload, kind, status=200, cache="no-store"):
        self.send_response(status)
        self.send_header("Content-Type", kind)
        self.send_header("Content-Length", str(len(payload)))
        self.send_header("Cache-Control", cache)
        self.send_header("X-Content-Type-Options", "nosniff")
        self.send_header("Referrer-Policy", "no-referrer")
        self.send_header("Content-Security-Policy", "default-src 'self'; style-src 'self' 'unsafe-inline';"
                         " img-src 'self' data:; frame-ancestors 'none'; base-uri 'none'; form-action 'none'")
        self.end_headers()
        try:
            self.wfile.write(payload)
        except (BrokenPipeError, ConnectionResetError):
            pass

    def log_request(self, code="-", size="-"):
        if str(code) not in ("200", "202"):
            super().log_request(code, size)


def bounded(value, default, maximum):
    return max(1, min(int(value), maximum)) if value is not None else default


if __name__ == "__main__":
    port = int(os.environ.get("ALEPH_MONITOR_PORT", "8765"))
    server = ThreadingHTTPServer(("127.0.0.1", port), Handler)
    metadata = ROOT / "release.json"
    server.release = json.loads(metadata.read_text()) if metadata.exists() else None
    server.accounts = Accounts()
    server.accounts.start()
    Path(DB_PATH).parent.mkdir(parents=True, exist_ok=True)
    server.store = TelemetryStore(DB_PATH, TELEMETRY_DIR)
    threading.Thread(target=ingest_loop, args=(server.store,), daemon=True).start()
    server.alerter = Alerter(ALERT_WEBHOOK)
    threading.Thread(target=alert_loop, args=(server.store, server.alerter), daemon=True).start()
    print(f"AlephTX dashboard listening on http://127.0.0.1:{port}", flush=True)
    server.serve_forever()
