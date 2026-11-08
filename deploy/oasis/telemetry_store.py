"""Ingest strategy telemetry JSONL into SQLite and answer dashboard queries.

The strategy writes one append-only `<session_id>.jsonl` per session (see
src/telemetry_sink.rs). Ingest tracks a byte offset per file, consumes only
complete lines, and keys every row by (session_id, offset), so re-reading a
file is idempotent. Fills are additionally unique by exchange trade ID.

Trade PnL uses the average-cost method, updated incrementally per fill.
Fees are positive costs; negative fees are maker rebates. Hedge-venue IOC
fills (`hedge_fill` lines) are tracked as a separate leg with their own
average cost; funding payments are not included.
"""

import json
import math
import sqlite3
import threading
import time
from functools import wraps
from pathlib import Path

EPS = 1e-12
SCHEMA = """
PRAGMA journal_mode=WAL;
CREATE TABLE IF NOT EXISTS ingest_offsets (
  file TEXT PRIMARY KEY, inode INTEGER NOT NULL, offset INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS ingest_records (
  session_id TEXT NOT NULL, offset INTEGER NOT NULL,
  PRIMARY KEY (session_id, offset)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS sessions (
  session_id TEXT PRIMARY KEY, network TEXT, exchange TEXT, symbol TEXT,
  market_id INTEGER, account INTEGER, hedge_account TEXT,
  started_ms INTEGER, last_ms INTEGER, stopped_ms INTEGER,
  position REAL NOT NULL DEFAULT 0, avg_cost REAL NOT NULL DEFAULT 0,
  realized REAL NOT NULL DEFAULT 0, fees REAL NOT NULL DEFAULT 0,
  volume REAL NOT NULL DEFAULT 0, fills INTEGER NOT NULL DEFAULT 0,
  buy_volume REAL NOT NULL DEFAULT 0, sell_volume REAL NOT NULL DEFAULT 0,
  stream_lost INTEGER NOT NULL DEFAULT 0, rejected INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS fills (
  session_id TEXT NOT NULL, offset INTEGER NOT NULL, ts_ms INTEGER NOT NULL,
  exchange_ts_ns INTEGER, client_order_id INTEGER, side TEXT NOT NULL,
  price REAL NOT NULL, size REAL NOT NULL, notional REAL NOT NULL,
  fee REAL NOT NULL, trade_id INTEGER NOT NULL, realized_delta REAL NOT NULL,
  position_after REAL NOT NULL, avg_cost_after REAL NOT NULL,
  net_pnl_after REAL NOT NULL, PRIMARY KEY (session_id, offset));
CREATE UNIQUE INDEX IF NOT EXISTS fills_trade ON fills (session_id, trade_id) WHERE trade_id > 0;
CREATE INDEX IF NOT EXISTS fills_ts ON fills (ts_ms);
CREATE TABLE IF NOT EXISTS orders (
  session_id TEXT NOT NULL, client_order_id INTEGER NOT NULL, order_index INTEGER,
  side TEXT, price REAL, size REAL, status TEXT NOT NULL,
  filled REAL NOT NULL DEFAULT 0, fill_notional REAL NOT NULL DEFAULT 0,
  fee REAL NOT NULL DEFAULT 0, created_ms INTEGER, updated_ms INTEGER,
  PRIMARY KEY (session_id, client_order_id));
CREATE INDEX IF NOT EXISTS orders_updated ON orders (session_id, updated_ms);
CREATE TABLE IF NOT EXISTS hedge_fills (
  session_id TEXT NOT NULL, offset INTEGER NOT NULL, ts_ms INTEGER NOT NULL,
  venue TEXT NOT NULL, client_order_id TEXT NOT NULL, side TEXT NOT NULL,
  price REAL NOT NULL, price_source TEXT, size REAL NOT NULL, notional REAL NOT NULL,
  fee REAL NOT NULL, realized_delta REAL NOT NULL, position_after REAL NOT NULL,
  PRIMARY KEY (session_id, offset));
CREATE UNIQUE INDEX IF NOT EXISTS hedge_fills_cloid ON hedge_fills (session_id, client_order_id);
CREATE INDEX IF NOT EXISTS hedge_fills_ts ON hedge_fills (ts_ms);
CREATE TABLE IF NOT EXISTS snapshots (
  session_id TEXT NOT NULL, ts_ms INTEGER NOT NULL, equity REAL,
  session_start_equity REAL, available_balance REAL, margin_usage REAL,
  exchange_position REAL, quote_position REAL, confirmed_position REAL,
  pending_exposure REAL, worst_case_long REAL, worst_case_short REAL,
  mid REAL, spread_bps REAL, adverse_selection REAL, trade_cash_flow REAL,
  total_fees REAL, fill_count INTEGER, orders_placed INTEGER,
  orders_rejected INTEGER, active_orders INTEGER, dropped INTEGER,
  PRIMARY KEY (session_id, ts_ms));
"""
SNAPSHOT_FIELDS = ("equity", "session_start_equity", "available_balance", "margin_usage",
                   "exchange_position", "quote_position", "confirmed_position",
                   "pending_exposure", "worst_case_long", "worst_case_short", "mid",
                   "spread_bps", "adverse_selection", "trade_cash_flow", "total_fees",
                   "fill_count", "orders_placed", "orders_rejected", "active_orders", "dropped")


# Snapshot columns added after the first release, ingested when present.
SNAPSHOT_MIGRATIONS = (("markout_marked", "INTEGER"), ("markout_expired", "INTEGER"),
                       ("markout_overwritten", "INTEGER"), ("markout_missed", "INTEGER"),
                       ("markout_last_delay_ms", "REAL"), ("markout_ewma_bps", "REAL"))
# Columns added after the first release; created on demand for old databases.
SESSION_MIGRATIONS = (("hedge_position", "REAL NOT NULL DEFAULT 0"),
                      ("hedge_avg_cost", "REAL NOT NULL DEFAULT 0"),
                      ("hedge_realized", "REAL NOT NULL DEFAULT 0"),
                      ("hedge_fees", "REAL NOT NULL DEFAULT 0"),
                      ("hedge_volume", "REAL NOT NULL DEFAULT 0"),
                      ("hedge_fills", "INTEGER NOT NULL DEFAULT 0"))


def finite(value, default=0.0):
    try:
        value = float(value)
    except (TypeError, ValueError):
        return default
    return value if math.isfinite(value) else default


def integer(value, default=0):
    """Keep signed 64-bit IDs exact; a float round trip aliases adjacent IDs."""
    try:
        if isinstance(value, float) and not value.is_integer():
            return default
        parsed = int(value)
    except (TypeError, ValueError, OverflowError):
        return default
    return parsed if -(2**63) <= parsed < 2**63 else default


def apply_fill(position, avg_cost, sign, price, qty):
    """Average-cost update. Returns (position, avg_cost, realized_delta)."""
    realized = 0.0
    if abs(position) < EPS or (position > 0) == (sign > 0):
        new = position + sign * qty
        avg_cost = (abs(position) * avg_cost + qty * price) / abs(new)
        return new, avg_cost, realized
    close = min(qty, abs(position))
    realized = close * (price - avg_cost) * (1 if position > 0 else -1)
    position += sign * close
    remainder = qty - close
    if abs(position) < EPS:
        position, avg_cost = 0.0, 0.0
    if remainder > EPS:
        position, avg_cost = sign * remainder, price
    return position, avg_cost, realized


def locked(method):
    """Serialize access: one connection is shared by the ingest and HTTP threads."""
    @wraps(method)
    def wrapper(self, *args, **kwargs):
        with self.lock:
            return method(self, *args, **kwargs)
    return wrapper


class TelemetryStore:
    def __init__(self, db_path, source_dir):
        self.lock = threading.RLock()
        self.source_dir = Path(source_dir)
        self.db = sqlite3.connect(db_path, check_same_thread=False, isolation_level=None)
        self.db.row_factory = sqlite3.Row
        self.db.executescript(SCHEMA)
        columns = {row["name"] for row in self.db.execute("PRAGMA table_info(sessions)")}
        for name, ddl in SESSION_MIGRATIONS:
            if name not in columns:
                self.db.execute(f"ALTER TABLE sessions ADD COLUMN {name} {ddl}")
        columns = {row["name"] for row in self.db.execute("PRAGMA table_info(snapshots)")}
        for name, ddl in SNAPSHOT_MIGRATIONS:
            if name not in columns:
                self.db.execute(f"ALTER TABLE snapshots ADD COLUMN {name} {ddl}")
        self.db.execute("PRAGMA synchronous=NORMAL")

    # ── ingest ──────────────────────────────────────────────────────────
    @locked
    def ingest(self, max_bytes=8_000_000):
        """Consume new complete lines from every session file. Returns lines read."""
        total = 0
        if not self.source_dir.is_dir():
            return 0
        for path in sorted(self.source_dir.glob("*.jsonl")):
            total += self._ingest_file(path, max_bytes)
        return total

    def _ingest_file(self, path, max_bytes):
        try:
            stat = path.stat()
        except FileNotFoundError:
            return 0
        row = self.db.execute("SELECT inode, offset FROM ingest_offsets WHERE file=?",
                              (path.name,)).fetchone()
        # A session filename is immutable, even if an archive restore changes
        # its inode. Retain the committed checkpoint, including for databases
        # created before per-record deduplication was introduced.
        offset = row["offset"] if row else 0
        if stat.st_size <= offset:
            return 0
        with path.open("rb") as handle:
            handle.seek(offset)
            chunk = handle.read(max_bytes)
        end = chunk.rfind(b"\n")
        if end < 0:
            return 0
        session_id = path.stem
        count = 0
        self.db.execute("BEGIN IMMEDIATE")
        try:
            position = offset
            for raw in chunk[:end + 1].splitlines(keepends=True):
                line_offset, position = position, position + len(raw)
                try:
                    record = json.loads(raw)
                except ValueError:
                    continue
                if isinstance(record, dict):
                    self._apply(session_id, line_offset, record)
                    count += 1
            self.db.execute("INSERT INTO ingest_offsets VALUES (?,?,?) ON CONFLICT(file) DO UPDATE "
                            "SET inode=excluded.inode, offset=excluded.offset",
                            (path.name, stat.st_ino, position))
            self.db.execute("COMMIT")
        except BaseException:
            self.db.execute("ROLLBACK")
            raise
        return count

    def _apply(self, sid, offset, record):
        # The source may be restored/copied to a new inode. Deduplicate before
        # any additive update, in the same transaction as the file checkpoint.
        inserted = self.db.execute("INSERT OR IGNORE INTO ingest_records VALUES (?,?)", (sid, offset))
        if inserted.rowcount == 0:
            return
        kind = record.get("type")
        ts = integer(record.get("ts_ms"))
        if kind == "session":
            self.db.execute(
                "INSERT INTO sessions (session_id, network, exchange, symbol, market_id, account,"
                " hedge_account, started_ms, last_ms) VALUES (?,?,?,?,?,?,?,?,?)"
                " ON CONFLICT(session_id) DO NOTHING",
                (sid, record.get("network"), record.get("exchange"), record.get("symbol"),
                 record.get("market_id"), record.get("account"), record.get("hedge_account"), ts, ts))
            return
        self.db.execute("INSERT INTO sessions (session_id, started_ms, last_ms) VALUES (?,?,?)"
                        " ON CONFLICT(session_id) DO UPDATE SET last_ms=max(last_ms, excluded.last_ms)",
                        (sid, ts, ts))
        if kind == "snapshot":
            values = [finite(record.get(name)) for name in SNAPSHOT_FIELDS]
            # Optional markout fields: null stays null (estimate not warm yet).
            extra = [name for name, _ in SNAPSHOT_MIGRATIONS]
            values += [None if record.get(name) is None else finite(record.get(name))
                       for name in extra]
            fields = (*SNAPSHOT_FIELDS, *extra)
            self.db.execute(f"INSERT OR REPLACE INTO snapshots (session_id, ts_ms, {', '.join(fields)})"
                            f" VALUES (?,?{',?' * len(fields)})", (sid, ts, *values))
        elif kind == "stopped":
            self.db.execute("UPDATE sessions SET stopped_ms=? WHERE session_id=?", (ts, sid))
        elif kind == "order":
            self._apply_order(sid, offset, ts, record)
        elif kind == "hedge_fill":
            self._apply_hedge_fill(sid, offset, ts, record)

    def _apply_hedge_fill(self, sid, offset, ts, r):
        side, cloid = r.get("side"), r.get("client_order_id")
        price, qty, fee = finite(r.get("price")), finite(r.get("size")), finite(r.get("fee"))
        if side not in ("buy", "sell") or not isinstance(cloid, str) or not cloid \
                or price <= 0 or qty <= 0:
            return
        s = self.db.execute("SELECT hedge_position, hedge_avg_cost FROM sessions WHERE session_id=?",
                            (sid,)).fetchone()
        position, avg_cost, delta = apply_fill(s["hedge_position"], s["hedge_avg_cost"],
                                               1 if side == "buy" else -1, price, qty)
        notional = price * qty
        inserted = self.db.execute(
            "INSERT OR IGNORE INTO hedge_fills VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)",
            (sid, offset, ts, str(r.get("venue") or "hyperliquid"), cloid, side, price,
             r.get("price_source"), qty, notional, fee, delta, finite(r.get("position_after"))))
        if inserted.rowcount == 0:
            return
        self.db.execute(
            "UPDATE sessions SET hedge_position=?, hedge_avg_cost=?, hedge_realized=hedge_realized+?,"
            " hedge_fees=hedge_fees+?, hedge_volume=hedge_volume+?, hedge_fills=hedge_fills+1"
            " WHERE session_id=?", (position, avg_cost, delta, fee, notional, sid))

    def _apply_order(self, sid, offset, ts, r):
        event = r.get("kind")
        coid = integer(r.get("client_order_id"))
        # V2 cancel/reject events do not carry side; their zero is not a buy.
        side = (r.get("side") if event in ("created", "filled")
                and r.get("side") in ("buy", "sell") else None)
        if event == "stream_lost":
            self.db.execute("UPDATE sessions SET stream_lost=stream_lost+1 WHERE session_id=?", (sid,))
            return
        if event == "rejected":
            self.db.execute("UPDATE sessions SET rejected=rejected+1 WHERE session_id=?", (sid,))
        if event in ("created", "canceled", "rejected"):
            status = {"created": "open", "canceled": "canceled", "rejected": "rejected"}[event]
            price, size = finite(r.get("price")), finite(r.get("size"))
            self.db.execute(
                "INSERT INTO orders (session_id, client_order_id, order_index, side, price, size, status,"
                " created_ms, updated_ms) VALUES (?,?,?,?,?,?,?,?,?) ON CONFLICT DO UPDATE SET"
                " order_index=coalesce(nullif(excluded.order_index,0), order_index),"
                " side=coalesce(excluded.side, side),"
                " price=CASE WHEN excluded.price>0 THEN excluded.price ELSE price END,"
                " size=CASE WHEN excluded.size>0 THEN excluded.size ELSE size END,"
                " status=CASE WHEN status IN ('filled','canceled','rejected')"
                " OR (status='partial' AND excluded.status='open') THEN status ELSE excluded.status END,"
                " updated_ms=excluded.updated_ms",
                (sid, coid, r.get("order_index"), side, price, size, status, ts, ts))
        elif event == "filled":
            self._apply_fill(sid, offset, ts, coid, side, r)

    def _apply_fill(self, sid, offset, ts, coid, side, r):
        price, qty, fee = finite(r.get("fill_price")), finite(r.get("fill_size")), finite(r.get("fee"))
        trade_id = integer(r.get("trade_id"))
        if side is None or price <= 0 or qty <= 0:
            return
        if trade_id and self.db.execute("SELECT 1 FROM fills WHERE session_id=? AND trade_id=?",
                                        (sid, trade_id)).fetchone():
            return
        s = self.db.execute("SELECT position, avg_cost, realized, fees FROM sessions WHERE session_id=?",
                            (sid,)).fetchone()
        position, avg_cost, delta = apply_fill(s["position"], s["avg_cost"],
                                               1 if side == "buy" else -1, price, qty)
        realized, fees, notional = s["realized"] + delta, s["fees"] + fee, price * qty
        inserted = self.db.execute(
            "INSERT OR IGNORE INTO fills VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
            (sid, offset, ts, r.get("exchange_ts_ns"), coid, side, price, qty, notional, fee,
             trade_id, delta, position, avg_cost, realized - fees))
        if inserted.rowcount == 0:
            return
        self.db.execute(
            "UPDATE sessions SET position=?, avg_cost=?, realized=?, fees=?, volume=volume+?,"
            " fills=fills+1, buy_volume=buy_volume+?, sell_volume=sell_volume+? WHERE session_id=?",
            (position, avg_cost, realized, fees, notional, notional if side == "buy" else 0.0,
             notional if side == "sell" else 0.0, sid))
        remaining = finite(r.get("remaining"))
        self.db.execute(
            "INSERT INTO orders (session_id, client_order_id, order_index, side, status, filled,"
            " fill_notional, fee, created_ms, updated_ms) VALUES (?,?,?,?,?,?,?,?,?,?)"
            " ON CONFLICT DO UPDATE SET filled=filled+excluded.filled,"
            " fill_notional=fill_notional+excluded.fill_notional, fee=fee+excluded.fee,"
            " status=CASE WHEN status IN ('canceled','rejected') THEN status ELSE excluded.status END,"
            " side=coalesce(side, excluded.side), updated_ms=excluded.updated_ms",
            (sid, coid, r.get("order_index"), side, "filled" if remaining <= EPS else "partial",
             qty, notional, fee, ts, ts))

    # ── queries ─────────────────────────────────────────────────────────
    BUCKETS = {"1m": 60_000, "5m": 300_000, "15m": 900_000, "1h": 3_600_000, "1d": 86_400_000}

    def _rows(self, sql, args=()):
        return [dict(row) for row in self.db.execute(sql, args).fetchall()]

    @locked
    def latest_session(self):
        row = self.db.execute("SELECT * FROM sessions ORDER BY started_ms DESC LIMIT 1").fetchone()
        return dict(row) if row else None

    @locked
    def sessions(self, limit=50):
        rows = self._rows("SELECT * FROM sessions ORDER BY started_ms DESC LIMIT ?", (limit,))
        for row in rows:
            row["net_pnl"] = row["realized"] - row["fees"]
            row["hedge_net_pnl"] = row["hedge_realized"] - row["hedge_fees"]
        return rows

    @locked
    def latest_snapshot(self, session_id):
        row = self.db.execute("SELECT * FROM snapshots WHERE session_id=? ORDER BY ts_ms DESC LIMIT 1",
                              (session_id,)).fetchone()
        return dict(row) if row else None

    def _window(self, since_ms):
        row = self.db.execute(
            "SELECT count(*) AS fills, coalesce(sum(notional),0) AS volume,"
            " coalesce(sum(realized_delta),0) AS realized, coalesce(sum(fee),0) AS fees,"
            " coalesce(sum(CASE WHEN fee<0 THEN -fee ELSE 0 END),0) AS rebates,"
            " coalesce(sum(CASE WHEN side='buy' THEN notional ELSE 0 END),0) AS buy_volume,"
            " coalesce(sum(CASE WHEN side='sell' THEN notional ELSE 0 END),0) AS sell_volume,"
            " coalesce(sum(CASE WHEN realized_delta>0 THEN 1 ELSE 0 END),0) AS winning,"
            " coalesce(sum(CASE WHEN realized_delta<0 THEN 1 ELSE 0 END),0) AS losing"
            " FROM fills WHERE ts_ms>=?", (since_ms,)).fetchone()
        result = dict(row)
        result["net_pnl"] = result["realized"] - result["fees"]
        closes = result["winning"] + result["losing"]
        result["win_rate"] = result["winning"] / closes if closes else None
        result["net_bps"] = result["net_pnl"] / result["volume"] * 1e4 if result["volume"] > 0 else None
        return result

    @locked
    def summary(self, now_ms=None):
        now_ms = now_ms or int(time.time() * 1000)
        day_start = now_ms - now_ms % 86_400_000
        session = self.latest_session()
        snapshot = self.latest_snapshot(session["session_id"]) if session else None
        current = None
        if session:
            mid = snapshot["mid"] if snapshot and snapshot["mid"] > 0 else None
            unrealized = (session["position"] * (mid - session["avg_cost"])
                          if mid and abs(session["position"]) > EPS else 0.0)
            # The hedge leg is marked to the same reference mid; venue basis
            # makes this approximate, and funding is not included.
            hedge_unrealized = (session["hedge_position"] * (mid - session["hedge_avg_cost"])
                                if mid and abs(session["hedge_position"]) > EPS else 0.0)
            hedge_net = session["hedge_realized"] - session["hedge_fees"]
            start_equity = snapshot["session_start_equity"] if snapshot else 0.0
            total = session["realized"] - session["fees"] + unrealized
            current = {**session, "unrealized": unrealized,
                       "net_pnl": session["realized"] - session["fees"],
                       "total_pnl": total,
                       "hedge_unrealized": hedge_unrealized,
                       "hedge_net_pnl": hedge_net,
                       "hedge_total_pnl": hedge_net + hedge_unrealized,
                       "combined_pnl": total + hedge_net + hedge_unrealized,
                       "equity_change": (snapshot["equity"] - start_equity
                                         if snapshot and start_equity > 0 else None),
                       "open_orders": self.db.execute(
                           "SELECT count(*) FROM orders WHERE session_id=? AND status IN ('open','partial')",
                           (session["session_id"],)).fetchone()[0]}
        return {"session": current, "snapshot": snapshot,
                "windows": {"today": self._window(day_start),
                            "24h": self._window(now_ms - 86_400_000),
                            "7d": self._window(now_ms - 7 * 86_400_000),
                            "all": self._window(0)}}

    @locked
    def fills(self, limit=200, before_ms=None, session_id=None):
        clauses, args = ["1=1"], []
        if before_ms:
            clauses.append("ts_ms<?"); args.append(before_ms)
        if session_id:
            clauses.append("session_id=?"); args.append(session_id)
        return self._rows(f"SELECT * FROM fills WHERE {' AND '.join(clauses)}"
                          " ORDER BY ts_ms DESC, offset DESC LIMIT ?", (*args, limit))

    @locked
    def hedge_fills(self, limit=200, before_ms=None, session_id=None):
        clauses, args = ["1=1"], []
        if before_ms:
            clauses.append("ts_ms<?"); args.append(before_ms)
        if session_id:
            clauses.append("session_id=?"); args.append(session_id)
        return self._rows(f"SELECT * FROM hedge_fills WHERE {' AND '.join(clauses)}"
                          " ORDER BY ts_ms DESC, offset DESC LIMIT ?", (*args, limit))

    @locked
    def orders(self, status="open", limit=200, session_id=None):
        session_id = session_id or (self.latest_session() or {}).get("session_id")
        if not session_id:
            return []
        if status == "open":
            return self._rows("SELECT * FROM orders WHERE session_id=? AND status IN ('open','partial')"
                              " ORDER BY price DESC", (session_id,))
        return self._rows("SELECT * FROM orders WHERE session_id=? ORDER BY updated_ms DESC LIMIT ?",
                          (session_id, limit))

    @locked
    def pnl_series(self, bucket="5m", since_ms=0):
        width = self.BUCKETS[bucket]
        # Cumulative net PnL across sessions at the end of each bucket.
        rows = self._rows(
            "SELECT (ts_ms/?)*? AS t, sum(realized_delta - fee) AS net, sum(notional) AS volume,"
            " count(*) AS fills FROM fills WHERE ts_ms>=? GROUP BY t ORDER BY t", (width, width, since_ms))
        base = self.db.execute("SELECT coalesce(sum(realized_delta - fee),0) FROM fills WHERE ts_ms<?",
                               (since_ms,)).fetchone()[0]
        cumulative = base
        for row in rows:
            cumulative += row["net"]
            row["cumulative"] = cumulative
        equity = self._rows(
            # SQLite returns bare columns from the row holding max(ts_ms).
            "SELECT (ts_ms/?)*? AS t, max(ts_ms) AS last_ts, equity, exchange_position AS position,"
            " mid FROM snapshots WHERE ts_ms>=? AND equity>0 GROUP BY t ORDER BY t",
            (width, width, since_ms))
        return {"bucket": bucket, "pnl": rows, "equity": equity}
