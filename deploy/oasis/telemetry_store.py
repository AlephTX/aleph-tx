"""Ingest strategy telemetry JSONL into SQLite and answer dashboard queries.

The strategy writes one append-only `<session_id>.jsonl` per session (see
src/telemetry_sink.rs). Ingest tracks a byte offset per file, consumes only
complete lines, and keys every row by (session_id, offset), so re-reading a
file is idempotent. Fills are additionally unique by exchange trade ID.

Trade PnL uses the average-cost method, updated incrementally per fill.
Fees are positive costs; negative fees are maker rebates. Hedge-venue IOC
fills (`hedge_fill` lines) are tracked as a separate leg with their own
average cost; funding payments are not included.

The summary labels each PnL component (see `valuation`): Lighter fill prices
and sizes are venue-reported. Each Lighter fee is estimated from the rate its
trade declared for the account's role; a fill whose `fee_source` is missing,
invalid, role_unknown or absent (older writers) leaves the fee total unknown,
even when its number is 0. Hedge prices are reported (`fill`), a worst-case
IOC bound (`limit`) or of unknown origin (legacy), and hedge fees are always
estimated from the configured rate. Open positions are marked to the latest
strategy mid; without one the unrealized part and every total that depends
on it is null, never zero. Figures cover this session's trades, not account
equity.
"""

import argparse
import json
import math
import os
import sqlite3
import threading
import time
from functools import wraps
from pathlib import Path

EPS = 1e-12
UNIT_EXCHANGE_FILTERS = {
    "aleph-tx-live": "(exchange IS NULL OR exchange != 'arcus_xyz_basis')",
    "aleph-tx-arcus-xyz": "exchange = 'arcus_xyz_basis'",
}
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
  net_pnl_after REAL NOT NULL, fee_source TEXT, PRIMARY KEY (session_id, offset));
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
  fee_source TEXT, PRIMARY KEY (session_id, offset));
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
# Price evidence behind `mid`; NULL on legacy rows (time and source unknown).
MARK_MIGRATIONS = (("mark_source", "TEXT"), ("mark_sampled_ms", "INTEGER"),
                   ("mark_feed_ms", "INTEGER"))
# Legacy snapshots have no evidence of the persisted daily loss budget.
DAILY_LOSS_MIGRATIONS = (("daily_loss_day_start_ms", "INTEGER"),
                         ("daily_loss_start_equity", "REAL"), ("daily_loss_limit_pct", "REAL"))
GUARD_MIGRATIONS = (("volume_quota_remaining", "INTEGER"), ("defense_regime", "TEXT"))
DEFENSE_REGIMES = (
    "normal",
    "caution",
    "side_defense_buy",
    "side_defense_sell",
    "side_defense_both",
    "reduce_only",
    "halt",
)
MARK_SOURCES = (
    "external_median",
    "lighter_book",
    "lighter_depth",
    "lighter_touch_micro",
    "local_book",
    "local_depth",
    "local_touch_micro",
)
# Columns added after the first release; created on demand for old databases.
SESSION_MIGRATIONS = (("hedge_position", "REAL NOT NULL DEFAULT 0"),
                      ("hedge_avg_cost", "REAL NOT NULL DEFAULT 0"),
                      ("hedge_realized", "REAL NOT NULL DEFAULT 0"),
                      ("hedge_fees", "REAL NOT NULL DEFAULT 0"),
                      ("hedge_volume", "REAL NOT NULL DEFAULT 0"),
                      ("hedge_fills", "INTEGER NOT NULL DEFAULT 0"))
# Hedge fill columns added later; legacy rows keep NULL (= unknown source).
HEDGE_FILL_MIGRATIONS = (("fee_source", "TEXT"),)
HEDGE_PRICE_SOURCES = ("fill", "limit")
HEDGE_FEE_SOURCES = ("configured_rate",)
# Lighter fill fee provenance as the feeder labels it (FeeSource in
# src/types/events.rs). Legacy rows, absent labels (older writers) and the
# writer's "unknown" (older feeders) are all NULL.
FILL_MIGRATIONS = (("fee_source", "TEXT"),)
LIGHTER_FEE_SOURCES = ("rate_derived", "explicit_zero", "missing", "invalid", "role_unknown")
# A fee from these is the declared rate times price and size; any other
# source leaves the fee unknown, even when its number is 0.
LIGHTER_RATED = ("rate_derived", "explicit_zero")
# Worst component quality wins: missing > unknown > estimated > exact kinds.
QUALITY_RANK = {"none": 0, "flat": 0, "reported": 0, "estimated": 1, "unknown": 2, "missing": 3}


def finite(value, default=0.0):
    try:
        value = float(value)
    except (TypeError, ValueError):
        return default
    return value if math.isfinite(value) else default


def positive_int(value):
    """Epoch-ms evidence time, or None when absent or not a positive integer."""
    if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
        return None
    return value


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
        for table, migrations in (("snapshots", SNAPSHOT_MIGRATIONS + MARK_MIGRATIONS
                                   + DAILY_LOSS_MIGRATIONS + GUARD_MIGRATIONS),
                                  ("hedge_fills", HEDGE_FILL_MIGRATIONS),
                                  ("fills", FILL_MIGRATIONS)):
            columns = {row["name"] for row in self.db.execute(f"PRAGMA table_info({table})")}
            for name, ddl in migrations:
                if name not in columns:
                    self.db.execute(f"ALTER TABLE {table} ADD COLUMN {name} {ddl}")
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
            source = record.get("mark_source")
            values += [source if source in MARK_SOURCES else None,
                       positive_int(record.get("mark_sampled_ms")),
                       positive_int(record.get("mark_feed_ms"))]
            values += [integer(record.get("daily_loss_day_start_ms"), None),
                       finite(record.get("daily_loss_start_equity"), None),
                       finite(record.get("daily_loss_limit_pct"), None)]
            raw_quota = record.get("volume_quota_remaining")
            quota = None if isinstance(raw_quota, bool) else integer(raw_quota, None)
            defense = record.get("defense_regime")
            values += [quota, defense if defense in DEFENSE_REGIMES else None]
            fields = (*SNAPSHOT_FIELDS, *extra,
                      *(name for name, _ in MARK_MIGRATIONS + DAILY_LOSS_MIGRATIONS + GUARD_MIGRATIONS))
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
        # Unrecognized provenance is stored as NULL: unknown, never promoted.
        price_source = r.get("price_source") if r.get("price_source") in HEDGE_PRICE_SOURCES else None
        fee_source = r.get("fee_source") if r.get("fee_source") in HEDGE_FEE_SOURCES else None
        inserted = self.db.execute(
            "INSERT OR IGNORE INTO hedge_fills (session_id, offset, ts_ms, venue, client_order_id,"
            " side, price, price_source, size, notional, fee, realized_delta, position_after,"
            " fee_source) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
            (sid, offset, ts, str(r.get("venue") or "hyperliquid"), cloid, side, price,
             price_source, qty, notional, fee, delta, finite(r.get("position_after")), fee_source))
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
        # The label only describes `fee`; the number is booked as before.
        fee_source = r.get("fee_source") if r.get("fee_source") in LIGHTER_FEE_SOURCES else None
        inserted = self.db.execute(
            "INSERT OR IGNORE INTO fills (session_id, offset, ts_ms, exchange_ts_ns,"
            " client_order_id, side, price, size, notional, fee, trade_id, realized_delta,"
            " position_after, avg_cost_after, net_pnl_after, fee_source)"
            " VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
            (sid, offset, ts, r.get("exchange_ts_ns"), coid, side, price, qty, notional, fee,
             trade_id, delta, position, avg_cost, realized - fees, fee_source))
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
    def latest_session_for_unit(self, unit):
        clause = UNIT_EXCHANGE_FILTERS.get(unit)
        if not clause:
            return None
        row = self.db.execute(
            f"SELECT * FROM sessions WHERE {clause} ORDER BY started_ms DESC LIMIT 1"
        ).fetchone()
        return dict(row) if row else None

    @locked
    def unit_summaries(self, now_ms=None):
        now_ms = now_ms or int(time.time() * 1000)
        out = {}
        for unit in UNIT_EXCHANGE_FILTERS:
            session = self.latest_session_for_unit(unit)
            if session is None:
                continue
            enriched, snapshot = self._enrich_session(session, now_ms)
            out[unit] = {"session": enriched, "snapshot": snapshot}
        return out

    @locked
    def ingested_offsets(self):
        rows = self.db.execute("SELECT file, inode, offset FROM ingest_offsets").fetchall()
        return {row["file"]: (int(row["inode"]), int(row["offset"])) for row in rows}

    @locked
    def session(self, session_id):
        row = self.db.execute("SELECT * FROM sessions WHERE session_id=?", (session_id,)).fetchone()
        return dict(row) if row else None

    def _enrich_session(self, session, now_ms):
        snapshot = self.latest_snapshot(session["session_id"])
        open_orders = self.db.execute(
            "SELECT count(*) FROM orders WHERE session_id=? AND status IN ('open','partial')",
            (session["session_id"],),
        ).fetchone()[0]
        enriched = {
            **session,
            **self._session_pnl(session, snapshot, now_ms),
            "open_orders": open_orders,
        }
        return enriched, snapshot

    @locked
    def sessions(self, limit=50, now_ms=None):
        now_ms = now_ms or int(time.time() * 1000)
        rows = self._rows("SELECT * FROM sessions ORDER BY started_ms DESC LIMIT ?", (limit,))
        return [self._enrich_session(row, now_ms)[0] for row in rows]

    @locked
    def latest_snapshot(self, session_id):
        row = self.db.execute("SELECT * FROM snapshots WHERE session_id=? ORDER BY ts_ms DESC LIMIT 1",
                              (session_id,)).fetchone()
        return dict(row) if row else None

    def _window(self, since_ms, session_id=None):
        where = "ts_ms>=?" if session_id is None else "ts_ms>=? AND session_id=?"
        args = (since_ms,) if session_id is None else (since_ms, session_id)
        row = self.db.execute(
            "SELECT count(*) AS fills, coalesce(sum(notional),0) AS volume,"
            " coalesce(sum(realized_delta),0) AS realized, coalesce(sum(fee),0) AS fees,"
            " coalesce(sum(CASE WHEN fee<0 THEN -fee ELSE 0 END),0) AS rebates,"
            " coalesce(sum(CASE WHEN side='buy' THEN notional ELSE 0 END),0) AS buy_volume,"
            " coalesce(sum(CASE WHEN side='sell' THEN notional ELSE 0 END),0) AS sell_volume,"
            " coalesce(sum(CASE WHEN realized_delta>0 THEN 1 ELSE 0 END),0) AS winning,"
            " coalesce(sum(CASE WHEN realized_delta<0 THEN 1 ELSE 0 END),0) AS losing"
            f" FROM fills WHERE {where}", args).fetchone()
        result = dict(row)
        result["net_pnl"] = result["realized"] - result["fees"]
        closes = result["winning"] + result["losing"]
        result["win_rate"] = result["winning"] / closes if closes else None
        result["net_bps"] = result["net_pnl"] / result["volume"] * 1e4 if result["volume"] > 0 else None
        return result

    def _hedge_sources(self, session_id):
        price = {"fill": 0, "limit": 0, "unknown": 0}
        fee = {"configured_rate": 0, "unknown": 0}
        for row in self.db.execute(
                "SELECT price_source, fee_source FROM hedge_fills WHERE session_id=?", (session_id,)):
            price[row["price_source"] if row["price_source"] in HEDGE_PRICE_SOURCES else "unknown"] += 1
            fee[row["fee_source"] if row["fee_source"] in HEDGE_FEE_SOURCES else "unknown"] += 1
        return price, fee

    def _lighter_fee_sources(self, session_id):
        counts = dict.fromkeys((*LIGHTER_FEE_SOURCES, "unknown"), 0)
        for row in self.db.execute("SELECT fee_source, count(*) AS n FROM fills WHERE session_id=?"
                                   " GROUP BY fee_source", (session_id,)):
            source = row["fee_source"]
            counts[source if source in LIGHTER_FEE_SOURCES else "unknown"] += row["n"]
        return counts

    @staticmethod
    def _mark(session, snapshot, mid, now_ms):
        """Where the mark came from and how old its evidence is.

        Three clocks meet here: `feed_ms` is the feeder's SHM timestamp
        (mostly venue time), `sampled_ms` the strategy host's wall clock and
        `now_ms` the monitor's. `snapshot_ms` is only when the record was
        written; a later snapshot can carry an old price.

        Freshness is `clock_mismatch` when the known times contradict the
        required order feed <= sampled <= now; ages are then None, never
        clamped, and the raw times stay for diagnosis. `unknown` means a time
        is missing. Skew that keeps that order is not detectable here.
        """
        if mid is None:
            return {"price": None, "source": None, "snapshot_ms": snapshot["ts_ms"] if snapshot else None,
                    "sampled_ms": None, "feed_ms": None, "sample_age_ms": None,
                    "feed_age_ms": None, "freshness": "none",
                    "scope": "both legs marked to the strategy reference mid"}
        sampled, feed = snapshot.get("mark_sampled_ms"), snapshot.get("mark_feed_ms")
        known = [t for t in (feed, sampled) if t is not None]
        mismatch = (any(t > now_ms for t in known)
                    or (feed is not None and sampled is not None and feed > sampled))
        if mismatch:
            freshness = "clock_mismatch"
        elif feed is None or sampled is None:
            freshness = "unknown"  # legacy record or no feed time behind the sample
        elif session.get("stopped_ms"):
            freshness = "historical"  # last mark of a stopped session
        else:
            freshness = "observed"  # times consistent; see the ages
        age = lambda ts: None if mismatch or ts is None else now_ms - ts
        return {"price": mid, "source": snapshot.get("mark_source") or "unknown",
                "snapshot_ms": snapshot["ts_ms"], "sampled_ms": sampled, "feed_ms": feed,
                "sample_age_ms": age(sampled), "feed_age_ms": age(feed), "freshness": freshness,
                "scope": "both legs marked to the strategy reference mid"}

    def _session_pnl(self, session, snapshot, now_ms):
        """PnL fields plus `valuation`, which labels where each number came from.

        A nonzero position without a valid mark has unknown unrealized PnL, so
        it and every total built on it are None. A flat position is exactly 0.
        """
        mid = snapshot["mid"] if snapshot and (snapshot["mid"] or 0) > 0 else None
        missing = []

        def mark(position, avg_cost):
            if abs(position) <= EPS:
                return 0.0, "flat"
            if mid is None:
                return None, "missing"
            return position * (mid - avg_cost), "estimated"

        def worst(*labels):
            label = max(labels, key=QUALITY_RANK.__getitem__)
            return "reported" if QUALITY_RANK[label] == 0 else label

        net = session["realized"] - session["fees"]
        unrealized, lighter_mark = mark(session["position"], session["avg_cost"])
        total = None if unrealized is None else net + unrealized
        # Lighter prices and sizes come from venue fill events. Each fee is the
        # rate its trade declared for this account's role times price and
        # size, so even a fully rated session is an estimate of the charge.
        # One fill without a usable rate, or from an older writer, makes the
        # fee total unknown, even when that fill's number is 0.
        lighter_fee = self._lighter_fee_sources(session["session_id"])
        rated = sum(lighter_fee[name] for name in LIGHTER_RATED)
        lighter_fees = ("none" if not sum(lighter_fee.values())
                        else "estimated" if rated == sum(lighter_fee.values()) else "unknown")
        lighter = {"realized": "reported", "fees": lighter_fees, "unrealized": lighter_mark,
                   "fee_sources": lighter_fee,
                   "quality": worst("reported", lighter_fees, lighter_mark)}

        price, fee = self._hedge_sources(session["session_id"])
        # Average cost carries every earlier price into later realized PnL, so
        # one limit or unknown price taints the whole leg.
        hedge_realized = ("none" if not sum(price.values()) else "unknown" if price["unknown"]
                          else "estimated" if price["limit"] else "reported")
        hedge_fees = ("none" if not sum(fee.values()) else "unknown" if fee["unknown"]
                      else "estimated")
        hedge_net = session["hedge_realized"] - session["hedge_fees"]
        # The hedge is marked to the same reference mid as Lighter; venue basis
        # makes this approximate.
        hedge_unrealized, hedge_mark = mark(session["hedge_position"], session["hedge_avg_cost"])
        hedge_total = None if hedge_unrealized is None else hedge_net + hedge_unrealized
        hedge = {"realized": hedge_realized, "fees": hedge_fees, "unrealized": hedge_mark,
                 "price_sources": price, "fee_sources": fee,
                 "quality": worst(hedge_realized, hedge_fees, hedge_mark)}

        mark_info = self._mark(session, snapshot, mid, now_ms)
        if "missing" in (lighter_mark, hedge_mark):
            missing.append("mark_price")
        elif "estimated" in (lighter_mark, hedge_mark) and mark_info["feed_ms"] is None:
            missing.append("mark_time")
        if rated:
            missing.append("lighter_actual_fee")
        if lighter_fee["missing"] + lighter_fee["invalid"] + lighter_fee["role_unknown"]:
            missing.append("lighter_fee_rate")
        if lighter_fee["unknown"]:
            missing.append("lighter_fee_source")
        if price["limit"]:
            missing.append("hedge_fill_price")
        if price["unknown"]:
            missing.append("hedge_price_source")
        if fee["configured_rate"]:
            missing.append("hedge_actual_fee")
        if fee["unknown"]:
            missing.append("hedge_fee_source")
        missing.append("funding")
        start_equity = snapshot["session_start_equity"] if snapshot else 0.0
        return {
            "unrealized": unrealized, "net_pnl": net, "total_pnl": total,
            "hedge_unrealized": hedge_unrealized, "hedge_net_pnl": hedge_net,
            "hedge_total_pnl": hedge_total,
            "combined_pnl": None if total is None or hedge_total is None else total + hedge_total,
            "equity_change": (snapshot["equity"] - start_equity
                              if snapshot and start_equity and start_equity > 0 else None),
            "valuation": {
                "mark": mark_info,
                "lighter": lighter, "hedge": hedge,
                "combined_quality": worst(lighter["quality"], hedge["quality"]),
                "missing": missing, "excluded": ["funding"],
                "scope": "strategy session trades, not account equity"},
        }

    @locked
    def summary(self, now_ms=None, session_id=None):
        now_ms = now_ms or int(time.time() * 1000)
        day_start = now_ms - now_ms % 86_400_000
        if session_id is None:
            session = self.latest_session()
        else:
            session = self.session(session_id)
            if session is None:
                raise ValueError("unknown session")
        current, snapshot = (None, None) if not session else self._enrich_session(session, now_ms)
        return {
            "session": current,
            "snapshot": snapshot,
            "windows": {
                "today": self._window(day_start, session_id=session_id),
                "24h": self._window(now_ms - 86_400_000, session_id=session_id),
                "7d": self._window(now_ms - 7 * 86_400_000, session_id=session_id),
                "all": self._window(0, session_id=session_id),
            },
        }

    @locked
    def alert_summary(self, now_ms=None):
        """Return latest summary augmented with per-unit sessions for isolated alerting."""
        now_ms = now_ms or int(time.time() * 1000)
        payload = self.summary(now_ms=now_ms)
        payload["unit_sessions"] = self.unit_summaries(now_ms=now_ms)
        return payload

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
    def pnl_series(self, bucket="5m", since_ms=0, session_id=None):
        width = self.BUCKETS[bucket]
        if session_id is not None and self.session(session_id) is None:
            raise ValueError("unknown session")
        fill_where = "ts_ms>=?" if session_id is None else "ts_ms>=? AND session_id=?"
        fill_args = (width, width, since_ms) if session_id is None else (width, width, since_ms, session_id)
        base_where = "ts_ms<?" if session_id is None else "ts_ms<? AND session_id=?"
        base_args = (since_ms,) if session_id is None else (since_ms, session_id)
        snap_where = "ts_ms>=? AND equity>0" if session_id is None else "ts_ms>=? AND equity>0 AND session_id=?"
        snap_args = (width, width, since_ms) if session_id is None else (width, width, since_ms, session_id)
        # Cumulative net PnL across sessions (or within the requested session) at the end of each bucket.
        rows = self._rows(
            f"SELECT (ts_ms/?)*? AS t, sum(realized_delta - fee) AS net, sum(notional) AS volume,"
            f" count(*) AS fills FROM fills WHERE {fill_where} GROUP BY t ORDER BY t", fill_args)
        base = self.db.execute(f"SELECT coalesce(sum(realized_delta - fee),0) FROM fills WHERE {base_where}",
                               base_args).fetchone()[0]
        cumulative = base
        for row in rows:
            cumulative += row["net"]
            row["cumulative"] = cumulative
        equity = self._rows(
            # SQLite returns bare columns from the row holding max(ts_ms).
            f"SELECT (ts_ms/?)*? AS t, max(ts_ms) AS last_ts, equity, exchange_position AS position,"
            f" mid FROM snapshots WHERE {snap_where} GROUP BY t ORDER BY t",
            snap_args)
        return {"bucket": bucket, "pnl": rows, "equity": equity}

    @locked
    def prune_expired(self, retention_days=14, now_ms=None):
        """Delete stopped sessions older than `retention_days` and trim old snapshots."""
        if retention_days is None or retention_days <= 0:
            return {"sessions": 0, "snapshots": 0, "fills": 0, "hedge_fills": 0, "orders": 0}
        now = int(time.time() * 1000) if now_ms is None else int(now_ms)
        cutoff_ms = now - int(float(retention_days) * 86_400_000)
        expired_ids = [
            row["session_id"]
            for row in self.db.execute(
                "SELECT session_id FROM sessions WHERE stopped_ms IS NOT NULL AND stopped_ms < ?",
                (cutoff_ms,),
            ).fetchall()
        ]
        counts = {"sessions": 0, "snapshots": 0, "fills": 0, "hedge_fills": 0, "orders": 0}
        self.db.execute("BEGIN IMMEDIATE")
        try:
            for sid in expired_ids:
                for table in ("snapshots", "fills", "hedge_fills", "orders"):
                    cur = self.db.execute(f"DELETE FROM {table} WHERE session_id=?", (sid,))
                    counts[table] += cur.rowcount
                self.db.execute("DELETE FROM ingest_records WHERE session_id=?", (sid,))
                # Keep ingest_offsets while the JSONL remains on disk so expired sessions are not re-ingested.
                if not (self.source_dir / f"{sid}.jsonl").is_file():
                    self.db.execute("DELETE FROM ingest_offsets WHERE file=?", (f"{sid}.jsonl",))
                cur = self.db.execute("DELETE FROM sessions WHERE session_id=?", (sid,))
                counts["sessions"] += cur.rowcount
            cur = self.db.execute(
                "DELETE FROM snapshots WHERE ts_ms < ? AND ts_ms < COALESCE("
                "(SELECT MAX(s2.ts_ms) FROM snapshots s2 WHERE s2.session_id = snapshots.session_id), 0)",
                (cutoff_ms,),
            )
            counts["snapshots"] += cur.rowcount
            self.db.execute("COMMIT")
        except BaseException:
            self.db.execute("ROLLBACK")
            raise
        if any(counts.values()):
            self.db.execute("PRAGMA wal_checkpoint(TRUNCATE)")
        return counts


def prune_telemetry_dir(
    telemetry_dir,
    max_age_days=7,
    max_total_bytes=200 * 1024 * 1024,
    active_grace_secs=600,
    now_secs=None,
    ingested_offsets=None,
    require_ingested=False,
):
    """Prune closed `*.jsonl` telemetry files by age first, then total size cap.

    When `ingested_offsets` (mapping filename -> (inode, offset)) is supplied or
    `require_ingested=True`, non-empty JSONL files are deleted only after their
    committed SQLite ingest offset covers the full on-disk size for the matching inode.
    """
    root = Path(telemetry_dir)
    if not root.is_dir():
        return {"deleted": [], "deleted_bytes": 0, "remaining_bytes": 0, "skipped_uningested": []}
    if require_ingested and ingested_offsets is None:
        raise ValueError("require_ingested=True requires ingested_offsets")
    verify_ingest = ingested_offsets is not None
    offsets = ingested_offsets or {}

    now = time.time() if now_secs is None else float(now_secs)
    age_cutoff = None if (max_age_days is None or max_age_days <= 0) else float(max_age_days) * 86_400.0
    byte_cap = None if (max_total_bytes is None or max_total_bytes < 0) else int(max_total_bytes)

    files = []
    for path in root.glob("*.jsonl"):
        if not path.is_file():
            continue
        try:
            st = path.stat()
        except FileNotFoundError:
            continue
        files.append((st.st_mtime, st.st_size, st.st_ino, path))
    files.sort(key=lambda item: (item[0], item[3].name))

    total_bytes = sum(item[1] for item in files)
    deleted = []
    deleted_bytes = 0
    skipped_uningested = []
    survivors = []

    def _is_ingested(filename, size, inode):
        if not verify_ingest or size == 0:
            return True
        rec = offsets.get(filename)
        if rec is None:
            return False
        rec_ino, rec_off = rec
        return rec_ino == inode and rec_off >= size

    for mtime, size, inode, path in files:
        age = max(0.0, now - mtime)
        past_grace = age >= active_grace_secs
        ingested = _is_ingested(path.name, size, inode)
        if past_grace and not ingested and age_cutoff is not None and age > age_cutoff:
            skipped_uningested.append(path.name)
        eligible = past_grace and ingested
        if eligible and age_cutoff is not None and age > age_cutoff:
            os.unlink(path)
            deleted.append(path.name)
            deleted_bytes += size
            total_bytes -= size
        else:
            survivors.append((mtime, size, inode, path, eligible, past_grace, ingested))

    if byte_cap is not None and total_bytes > byte_cap:
        for _mtime, size, _inode, path, eligible, past_grace, ingested in survivors:
            if total_bytes <= byte_cap:
                break
            if not eligible:
                if past_grace and not ingested and path.name not in skipped_uningested:
                    skipped_uningested.append(path.name)
                continue
            os.unlink(path)
            deleted.append(path.name)
            deleted_bytes += size
            total_bytes -= size

    return {
        "deleted": deleted,
        "deleted_bytes": deleted_bytes,
        "remaining_bytes": max(0, total_bytes),
        "skipped_uningested": skipped_uningested,
    }


def _main():
    parser = argparse.ArgumentParser(description="Manage AlephTX strategy telemetry SQLite & JSONL retention.")
    sub = parser.add_subparsers(dest="command", required=True)
    p = sub.add_parser("prune", help="Prune expired JSONL files and optionally SQLite telemetry rows")
    p.add_argument("--dir", type=Path, required=True, help="Telemetry JSONL directory")
    p.add_argument("--max-age-days", type=float, default=7.0)
    p.add_argument("--max-mb", type=float, default=200.0)
    p.add_argument("--active-grace-secs", type=float, default=600.0)
    p.add_argument("--db", type=Path, default=None, help="Optional SQLite path to verify ingestion & prune")
    p.add_argument("--retention-days", type=float, default=14.0)
    p.add_argument(
        "--allow-uningested",
        action="store_true",
        help="Allow deleting un-verified JSONL files without `--db` offset checks",
    )
    args = parser.parse_args()
    if args.command == "prune":
        if args.db is not None:
            store = TelemetryStore(args.db, args.dir)
            try:
                # Flush complete records before file deletion so monitor downtime never drops uningested fills.
                ingested_lines = store.ingest()
                offsets = store.ingested_offsets()
                file_stats = prune_telemetry_dir(
                    args.dir,
                    max_age_days=args.max_age_days,
                    max_total_bytes=int(args.max_mb * 1024 * 1024),
                    active_grace_secs=args.active_grace_secs,
                    ingested_offsets=offsets,
                    require_ingested=True,
                )
                file_stats["ingested_before_prune"] = ingested_lines
                db_stats = store.prune_expired(retention_days=args.retention_days)
            finally:
                store.db.close()
        elif args.allow_uningested:
            file_stats = prune_telemetry_dir(
                args.dir,
                max_age_days=args.max_age_days,
                max_total_bytes=int(args.max_mb * 1024 * 1024),
                active_grace_secs=args.active_grace_secs,
            )
            db_stats = None
        else:
            raise SystemExit("refusing to prune JSONL without --db ingest verification (pass --allow-uningested to override)")
        print(json.dumps({"files": file_stats, "db": db_stats}, indent=2))


if __name__ == "__main__":
    _main()
