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


if __name__ == "__main__":
    unittest.main()
