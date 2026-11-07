"""Unit tests for telemetry_store. Run: python3 -m unittest discover deploy/oasis"""

import json
import tempfile
import unittest
from pathlib import Path

from telemetry_store import TelemetryStore, apply_fill


def fill(ts, side, price, size, fee, trade_id, coid=1, remaining=0.0):
    return {"type": "order", "ts_ms": ts, "kind": "filled", "client_order_id": coid,
            "order_index": 9, "side": side, "fill_price": price, "fill_size": size,
            "fee": fee, "trade_id": trade_id, "remaining": remaining}


class AverageCostTest(unittest.TestCase):
    def test_open_add_reduce_flip(self):
        pos, avg, r = apply_fill(0.0, 0.0, 1, 100.0, 1.0)
        self.assertEqual((pos, avg, r), (1.0, 100.0, 0.0))
        pos, avg, r = apply_fill(pos, avg, 1, 110.0, 1.0)
        self.assertAlmostEqual(avg, 105.0)
        pos, avg, r = apply_fill(pos, avg, -1, 115.0, 0.5)
        self.assertAlmostEqual(r, 5.0)
        self.assertAlmostEqual(pos, 1.5)
        pos, avg, r = apply_fill(pos, avg, -1, 95.0, 2.0)
        self.assertAlmostEqual(r, -15.0)
        self.assertAlmostEqual(pos, -0.5)
        self.assertAlmostEqual(avg, 95.0)
        pos, avg, r = apply_fill(pos, avg, 1, 90.0, 0.5)
        self.assertAlmostEqual(r, 2.5)
        self.assertEqual((pos, avg), (0.0, 0.0))


class StoreTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self.tmp.name)
        self.file = self.dir / "s1.jsonl"
        self.store = TelemetryStore(":memory:", self.dir)

    def tearDown(self):
        self.store.db.close()
        self.tmp.cleanup()

    def write(self, *records, partial=""):
        with self.file.open("a") as handle:
            for record in records:
                handle.write(json.dumps(record) + "\n")
            handle.write(partial)

    def test_round_trip_summary_and_idempotent_reingest(self):
        self.write({"type": "session", "ts_ms": 1000, "network": "Mainnet", "exchange": "lighter",
                    "symbol": "ETH", "market_id": 0, "account": 7},
                   {"type": "order", "ts_ms": 1100, "kind": "created", "client_order_id": 1,
                    "order_index": 9, "side": "buy", "price": 2000.0, "size": 0.1},
                   {"type": "order", "ts_ms": 1150, "kind": "created", "client_order_id": 2,
                    "order_index": 10, "side": "sell", "price": 2010.0, "size": 0.1},
                   fill(1200, "buy", 2000.0, 0.1, 0.02, 501),
                   fill(1300, "sell", 2010.0, 0.1, -0.01, 502, coid=2),
                   fill(1300, "sell", 2010.0, 0.1, -0.01, 502, coid=2),  # duplicate trade id
                   {"type": "snapshot", "ts_ms": 1400, "equity": 1001.0, "session_start_equity": 1000.0,
                    "mid": 2005.0, "dropped": 0})
        self.assertEqual(self.store.ingest(), 7)
        self.assertEqual(self.store.ingest(), 0)
        summary = self.store.summary(now_ms=2000)
        session = summary["session"]
        self.assertEqual(session["fills"], 2)
        self.assertAlmostEqual(session["realized"], 1.0)
        self.assertAlmostEqual(session["fees"], 0.01)
        self.assertAlmostEqual(session["net_pnl"], 0.99)
        self.assertAlmostEqual(session["volume"], 401.0)
        self.assertAlmostEqual(session["equity_change"], 1.0)
        self.assertEqual(session["open_orders"], 0)
        self.assertAlmostEqual(summary["windows"]["all"]["rebates"], 0.01)
        self.assertEqual(summary["windows"]["all"]["win_rate"], 1.0)
        orders = {o["client_order_id"]: o for o in self.store.orders(status="all")}
        self.assertEqual(orders[1]["status"], "filled")
        self.assertAlmostEqual(orders[1]["price"], 2000.0)
        self.assertEqual(len(self.store.fills()), 2)

    def test_partial_line_waits_for_newline(self):
        self.write({"type": "session", "ts_ms": 1, "symbol": "ETH"}, partial='{"type":"order","ts')
        self.assertEqual(self.store.ingest(), 1)
        with self.file.open("a") as handle:
            handle.write('_ms":5,"kind":"stream_lost"}\n')
        self.assertEqual(self.store.ingest(), 1)
        self.assertEqual(self.store.latest_session()["stream_lost"], 1)

    def test_pnl_series_is_cumulative(self):
        self.write({"type": "session", "ts_ms": 0, "symbol": "ETH"},
                   fill(10_000, "buy", 100.0, 1.0, 0.0, 1),
                   fill(70_000, "sell", 101.0, 1.0, 0.0, 2),
                   fill(130_000, "buy", 100.0, 1.0, 0.0, 3),
                   fill(190_000, "sell", 102.0, 1.0, 0.0, 4))
        self.store.ingest()
        series = self.store.pnl_series("1m")["pnl"]
        self.assertEqual([row["cumulative"] for row in series], [0.0, 1.0, 1.0, 3.0])

    def test_replaced_file_does_not_reapply_fills_or_event_counters(self):
        self.write({"type": "session", "ts_ms": 1},
                   fill(2, "buy", 100.0, 1.0, 0.1, 0),
                   {"type": "order", "ts_ms": 3, "kind": "stream_lost"},
                   {"type": "order", "ts_ms": 4, "kind": "rejected", "client_order_id": 2})
        self.store.ingest()
        before = self.store.latest_session()
        replacement = self.dir / "replacement"
        replacement.write_bytes(self.file.read_bytes())
        replacement.replace(self.file)
        self.store.ingest()
        self.assertEqual(self.store.latest_session(), before)
        self.assertEqual(len(self.store.fills()), 1)

        # An explicit full replay is also idempotent for every event kind.
        self.store.db.execute("DELETE FROM ingest_offsets")
        self.store.ingest()
        self.assertEqual(self.store.latest_session(), before)

    def test_existing_checkpoint_survives_upgrade_and_inode_change(self):
        self.write({"type": "session", "ts_ms": 1},
                   {"type": "order", "ts_ms": 2, "kind": "stream_lost"})
        self.store.ingest()
        # Old databases had a file checkpoint but no per-record registry.
        self.store.db.execute("DELETE FROM ingest_records")
        replacement = self.dir / "replacement"
        replacement.write_bytes(self.file.read_bytes())
        replacement.replace(self.file)
        self.write({"type": "order", "ts_ms": 3, "kind": "stream_lost"})
        self.store.ingest()
        self.assertEqual(self.store.latest_session()["stream_lost"], 2)

    def test_integer_identifiers_are_not_rounded_through_float(self):
        first = 2**53
        self.write({"type": "session", "ts_ms": 1},
                   fill(2, "buy", 100.0, 1.0, 0.0, first, coid=first),
                   fill(3, "sell", 101.0, 1.0, 0.0, first + 1, coid=first + 1))
        self.store.ingest()
        self.assertEqual({r["trade_id"] for r in self.store.fills()}, {first, first + 1})
        self.assertEqual({r["client_order_id"] for r in self.store.orders("all")},
                         {first, first + 1})
        self.assertAlmostEqual(self.store.latest_session()["realized"], 1.0)

    def test_late_create_does_not_reopen_a_canceled_order(self):
        self.write({"type": "session", "ts_ms": 1},
                   {"type": "order", "ts_ms": 2, "kind": "canceled", "client_order_id": 1,
                    "order_index": 9, "side": "buy", "price": 0.0, "size": 0.0},
                   {"type": "order", "ts_ms": 3, "kind": "created", "client_order_id": 1,
                    "order_index": 9, "side": "sell", "price": 100.0, "size": 1.0})
        self.store.ingest()
        self.assertEqual(self.store.orders(), [])
        order = self.store.orders("all")[0]
        self.assertEqual(order["status"], "canceled")
        self.assertEqual(order["side"], "sell")

    def test_late_create_preserves_partial_fill_state(self):
        self.write({"type": "session", "ts_ms": 1},
                   fill(2, "sell", 100.0, 0.5, 0.0, 1, remaining=0.5),
                   {"type": "order", "ts_ms": 3, "kind": "created", "client_order_id": 1,
                    "order_index": 9, "side": "sell", "price": 100.0, "size": 1.0})
        self.store.ingest()
        order = self.store.orders()[0]
        self.assertEqual(order["status"], "partial")
        self.assertEqual(order["filled"], 0.5)

    def test_hedge_fills_track_a_separate_leg_and_dedupe_by_cloid(self):
        hedge = lambda ts, side, price, size, cloid: {
            "type": "hedge_fill", "ts_ms": ts, "venue": "hyperliquid", "client_order_id": cloid,
            "side": side, "size": size, "price": price, "price_source": "fill",
            "fee": price * size * 4.5e-4, "position_after": 0.0}
        self.write({"type": "session", "ts_ms": 1000, "exchange": "lighter", "symbol": "ETH"},
                   fill(1100, "buy", 2000.0, 0.1, 0.0, 601),
                   hedge(1200, "sell", 2001.0, 0.1, "0xa"),
                   hedge(1200, "sell", 2001.0, 0.1, "0xa"),  # duplicate cloid
                   fill(1300, "sell", 2010.0, 0.1, 0.0, 602, coid=2),
                   hedge(1400, "buy", 2008.0, 0.1, "0xb"),
                   {"type": "hedge_fill", "ts_ms": 1450, "side": "buy", "size": 0.1, "price": 0},
                   {"type": "snapshot", "ts_ms": 1500, "mid": 2009.0, "session_start_equity": 1.0})
        self.store.ingest()
        session = self.store.summary(now_ms=2000)["session"]
        # Lighter-only figures are unchanged by the hedge leg.
        self.assertAlmostEqual(session["net_pnl"], 1.0)
        self.assertEqual(session["hedge_fills"], 2)
        self.assertAlmostEqual(session["hedge_position"], 0.0)
        self.assertAlmostEqual(session["hedge_realized"], -0.7)
        fees = (2001.0 + 2008.0) * 0.1 * 4.5e-4
        self.assertAlmostEqual(session["hedge_net_pnl"], -0.7 - fees)
        self.assertAlmostEqual(session["combined_pnl"], 1.0 - 0.7 - fees)
        self.assertEqual(len(self.store.hedge_fills()), 2)

    def test_old_database_gains_hedge_columns(self):
        import sqlite3
        path = self.dir / "old.sqlite3"
        db = sqlite3.connect(path)
        db.execute("CREATE TABLE sessions (session_id TEXT PRIMARY KEY, network TEXT, exchange TEXT,"
                   " symbol TEXT, market_id INTEGER, account INTEGER, hedge_account TEXT,"
                   " started_ms INTEGER, last_ms INTEGER, stopped_ms INTEGER,"
                   " position REAL NOT NULL DEFAULT 0, avg_cost REAL NOT NULL DEFAULT 0,"
                   " realized REAL NOT NULL DEFAULT 0, fees REAL NOT NULL DEFAULT 0,"
                   " volume REAL NOT NULL DEFAULT 0, fills INTEGER NOT NULL DEFAULT 0,"
                   " buy_volume REAL NOT NULL DEFAULT 0, sell_volume REAL NOT NULL DEFAULT 0,"
                   " stream_lost INTEGER NOT NULL DEFAULT 0, rejected INTEGER NOT NULL DEFAULT 0)")
        db.execute("INSERT INTO sessions (session_id, started_ms) VALUES ('old', 1)")
        db.commit()
        db.close()
        store = TelemetryStore(str(path), self.dir)
        try:
            row = store.sessions()[0]
            self.assertEqual((row["hedge_fills"], row["hedge_net_pnl"]), (0, 0.0))
        finally:
            store.db.close()


if __name__ == "__main__":
    unittest.main()
