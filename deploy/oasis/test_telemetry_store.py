"""Unit tests for telemetry_store. Run: python3 -m unittest discover deploy/oasis"""

import json
import tempfile
import unittest
from pathlib import Path

from telemetry_store import TelemetryStore, apply_fill


def fill(ts, side, price, size, fee, trade_id, coid=1, remaining=0.0, fee_source=None):
    """A Lighter fill line; without `fee_source` it is a legacy writer's."""
    record = {"type": "order", "ts_ms": ts, "kind": "filled", "client_order_id": coid,
              "order_index": 9, "side": side, "fill_price": price, "fill_size": size,
              "fee": fee, "trade_id": trade_id, "remaining": remaining}
    if fee_source is not None:
        record["fee_source"] = fee_source
    return record


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

    def test_snapshot_markout_fields_are_stored_with_null_until_warm(self):
        self.write({"type": "session", "ts_ms": 1000, "exchange": "lighter", "symbol": "ETH"},
                   {"type": "snapshot", "ts_ms": 1100, "equity": 1.0, "markout_marked": 0,
                    "markout_expired": 2, "markout_ewma_bps": None},
                   {"type": "snapshot", "ts_ms": 1200, "equity": 1.0, "markout_marked": 9,
                    "markout_expired": 2, "markout_last_delay_ms": 120.5,
                    "markout_ewma_bps": -1.25})
        self.store.ingest()
        rows = self.store.db.execute("SELECT ts_ms, markout_marked, markout_expired,"
                                     " markout_last_delay_ms, markout_ewma_bps FROM snapshots"
                                     " ORDER BY ts_ms").fetchall()
        self.assertEqual(tuple(rows[0]), (1100, 0, 2, None, None))
        self.assertEqual(tuple(rows[1]), (1200, 9, 2, 120.5, -1.25))

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

    def hedge(self, ts, side, price, size, cloid, price_source="fill", fee_source="configured_rate"):
        record = {"type": "hedge_fill", "ts_ms": ts, "venue": "hyperliquid",
                  "client_order_id": cloid, "side": side, "size": size, "price": price,
                  "fee": price * size * 4.5e-4, "position_after": 0.0}
        if price_source is not None:
            record["price_source"] = price_source
        if fee_source is not None:
            record["fee_source"] = fee_source
        return record

    def test_writer_fixture_labels_every_cost_and_keeps_missing_mark_unknown(self):
        # Produced by the Rust TelemetrySink (cost_fixture_matches_the_writer).
        fixture = Path(__file__).parent / "fixtures" / "cost_session.jsonl"
        self.file.write_text(fixture.read_text())
        self.store.ingest()
        self.store.ingest()  # duplicate ingest changes nothing
        session = self.store.summary(now_ms=2000)["session"]
        self.assertEqual(session["fills"], 2)
        self.assertAlmostEqual(session["net_pnl"], 1.004)
        self.assertEqual(session["unrealized"], 0.0)  # flat, so exact
        self.assertAlmostEqual(session["total_pnl"], 1.004)
        self.assertEqual(session["hedge_fills"], 3)
        self.assertAlmostEqual(session["hedge_position"], -0.05)
        self.assertAlmostEqual(session["hedge_realized"], -1.1)
        self.assertAlmostEqual(session["hedge_fees"], 0.2257875)
        # Open hedge with mid 0: valuation is unknown, never zero.
        self.assertIsNone(session["hedge_unrealized"])
        self.assertIsNone(session["hedge_total_pnl"])
        self.assertIsNone(session["combined_pnl"])
        self.assertAlmostEqual(session["hedge_net_pnl"], -1.1 - 0.2257875)
        v = session["valuation"]
        # Both Lighter fees are declared rates times notional: estimates.
        self.assertEqual((v["lighter"]["fees"], v["lighter"]["quality"]), ("estimated", "estimated"))
        self.assertEqual(v["lighter"]["fee_sources"],
                         {"rate_derived": 2, "explicit_zero": 0, "missing": 0, "invalid": 0,
                          "role_unknown": 0, "unknown": 0})
        self.assertEqual(v["hedge"]["price_sources"], {"fill": 2, "limit": 1, "unknown": 0})
        self.assertEqual(v["hedge"]["fee_sources"], {"configured_rate": 3, "unknown": 0})
        self.assertEqual((v["hedge"]["realized"], v["hedge"]["fees"], v["hedge"]["unrealized"]),
                         ("estimated", "estimated", "missing"))
        self.assertEqual(v["combined_quality"], "missing")
        self.assertIsNone(v["mark"]["price"])
        self.assertEqual(v["missing"], ["mark_price", "lighter_actual_fee", "hedge_fill_price",
                                        "hedge_actual_fee", "funding"])
        sources = [(r["price_source"], r["fee_source"]) for r in self.store.hedge_fills()]
        self.assertEqual(sorted(sources), [("fill", "configured_rate"), ("fill", "configured_rate"),
                                           ("limit", "configured_rate")])

    def test_open_hedge_without_snapshot_has_no_combined_value(self):
        self.write({"type": "session", "ts_ms": 1000, "exchange": "lighter", "symbol": "ETH"},
                   self.hedge(1100, "sell", 2000.0, 0.1, "0xa"))
        self.store.ingest()
        session = self.store.summary(now_ms=2000)["session"]
        self.assertIsNone(session["combined_pnl"])
        self.assertIsNone(session["hedge_unrealized"])
        self.assertEqual(session["total_pnl"], 0.0)  # Lighter leg is flat
        self.assertIn("mark_price", session["valuation"]["missing"])

    def test_marked_open_position_is_estimated_and_flat_control_is_exact(self):
        self.write({"type": "session", "ts_ms": 1000, "exchange": "lighter", "symbol": "ETH"},
                   fill(1100, "buy", 2000.0, 0.1, 0.0, 701),
                   {"type": "snapshot", "ts_ms": 1200, "mid": 2005.0})
        self.store.ingest()
        session = self.store.summary(now_ms=2000)["session"]
        self.assertAlmostEqual(session["unrealized"], 0.5)
        self.assertAlmostEqual(session["combined_pnl"], 0.5)
        v = session["valuation"]
        self.assertEqual(v["lighter"]["unrealized"], "estimated")
        self.assertEqual(v["hedge"]["quality"], "reported")  # no hedge fills: nothing estimated
        self.assertEqual(v["hedge"]["realized"], "none")
        # Hand-written legacy snapshot: only its write time is known.
        self.assertEqual((v["mark"]["price"], v["mark"]["source"], v["mark"]["snapshot_ms"]),
                         (2005.0, "unknown", 1200))
        self.assertEqual((v["mark"]["sampled_ms"], v["mark"]["feed_ms"], v["mark"]["feed_age_ms"],
                          v["mark"]["freshness"]), (None, None, None, "unknown"))
        self.assertNotIn("mark_price", v["missing"])
        self.assertIn("mark_time", v["missing"])
        self.write(fill(1300, "sell", 2010.0, 0.1, 0.0, 702, coid=2),
                   {"type": "snapshot", "ts_ms": 1400, "mid": 0.0})
        self.store.ingest()
        session = self.store.summary(now_ms=2000)["session"]
        self.assertEqual(session["unrealized"], 0.0)  # flat: no mark needed
        self.assertAlmostEqual(session["combined_pnl"], 1.0)
        v = session["valuation"]
        self.assertEqual((v["lighter"]["unrealized"], v["hedge"]["unrealized"]), ("flat", "flat"))
        self.assertNotIn("mark_price", v["missing"])
        # The mark plays no part; only these legacy fills' fee source is unknown.
        self.assertEqual((v["lighter"]["fees"], v["combined_quality"]), ("unknown", "unknown"))

    def test_hedge_reversal_with_mixed_and_legacy_sources_is_never_reported(self):
        self.write({"type": "session", "ts_ms": 1000, "exchange": "lighter", "symbol": "ETH"},
                   self.hedge(1100, "buy", 2000.0, 0.1, "0xa"),
                   self.hedge(1200, "buy", 2004.0, 0.1, "0xb"),          # add
                   self.hedge(1300, "sell", 2010.0, 0.3, "0xc", price_source="limit"),  # reverse
                   {"type": "snapshot", "ts_ms": 1400, "mid": 2008.0})
        self.store.ingest()
        session = self.store.summary(now_ms=2000)["session"]
        self.assertAlmostEqual(session["hedge_realized"], (2010.0 - 2002.0) * 0.2)
        self.assertAlmostEqual(session["hedge_position"], -0.1)
        self.assertAlmostEqual(session["hedge_unrealized"], -0.1 * (2008.0 - 2010.0))
        v = session["valuation"]["hedge"]
        self.assertEqual((v["realized"], v["quality"]), ("estimated", "estimated"))
        # Legacy lines without provenance (and unrecognized labels) are unknown.
        self.write(self.hedge(1500, "buy", 2008.0, 0.1, "0xd", price_source=None, fee_source=None),
                   self.hedge(1600, "buy", 2008.0, 0.1, "0xe", price_source="venue",
                              fee_source="exact"))
        self.store.ingest()
        session = self.store.summary(now_ms=2000)["session"]
        v = session["valuation"]
        self.assertEqual(v["hedge"]["price_sources"], {"fill": 2, "limit": 1, "unknown": 2})
        self.assertEqual(v["hedge"]["fee_sources"], {"configured_rate": 3, "unknown": 2})
        self.assertEqual((v["hedge"]["realized"], v["hedge"]["fees"]), ("unknown", "unknown"))
        self.assertEqual(v["combined_quality"], "unknown")
        self.assertIn("hedge_price_source", v["missing"])
        self.assertIn("hedge_fee_source", v["missing"])

    def test_old_hedge_fill_table_gains_fee_source_as_unknown(self):
        import sqlite3
        path = self.dir / "old_hedge.sqlite3"
        old = TelemetryStore(str(path), self.dir)
        self.write({"type": "session", "ts_ms": 1000, "exchange": "lighter", "symbol": "ETH"},
                   self.hedge(1100, "sell", 2000.0, 0.1, "0xa"))
        old.ingest()
        old.db.close()
        db = sqlite3.connect(path)
        db.execute("ALTER TABLE hedge_fills DROP COLUMN fee_source")
        db.commit()
        db.close()
        store = TelemetryStore(str(path), self.dir)
        try:
            store.ingest()
            self.assertIsNone(store.hedge_fills()[0]["fee_source"])
            session = store.summary(now_ms=2000)["session"]
            self.assertEqual(session["hedge_fills"], 1)
            self.assertEqual(session["valuation"]["hedge"]["fees"], "unknown")
        finally:
            store.db.close()

    def test_writer_fee_sources_keep_missing_apart_from_explicit_zero(self):
        # Feeder bytes from the real processTradeFast (Go fixture test), read by
        # the Rust V2 reader and written by TelemetrySink.
        self.load_fixture("fee_source_session.jsonl")
        self.assertEqual(self.store.ingest(), 0)  # duplicate ingest changes nothing
        rows = {r["trade_id"]: (r["fee"], r["fee_source"]) for r in self.store.fills()}
        # Same fee number, different evidence: a known zero vs. no rate.
        self.assertEqual((rows[503], rows[505], rows[508]),
                         ((0.0, "explicit_zero"), (0.0, "missing"), (0.0, None)))
        self.assertEqual([rows[t][1] for t in sorted(rows)],
                         ["rate_derived", "rate_derived", "explicit_zero", "rate_derived",
                          "missing", "invalid", "role_unknown", None])
        session = self.store.summary(now_ms=20_000)["session"]
        # Fees are booked as before; the label never changes a number.
        self.assertEqual(session["fills"], 8)
        self.assertAlmostEqual(session["fees"],
                               0.0004001 + 0.008002 - 0.000500125 + 0.0000500125 + 0.0004001)
        v = session["valuation"]["lighter"]
        self.assertEqual(v["fee_sources"], {"rate_derived": 3, "explicit_zero": 1, "missing": 1,
                                            "invalid": 1, "role_unknown": 1, "unknown": 1})
        self.assertEqual((v["realized"], v["fees"]), ("reported", "unknown"))
        missing = session["valuation"]["missing"]
        for key in ("lighter_actual_fee", "lighter_fee_rate", "lighter_fee_source"):
            self.assertIn(key, missing)

    def test_lighter_fees_are_estimated_only_when_every_fill_declares_a_rate(self):
        self.write({"type": "session", "ts_ms": 1000, "exchange": "lighter", "symbol": "ETH"},
                   fill(1100, "buy", 2000.0, 0.1, 0.008, 601, fee_source="rate_derived"),
                   fill(1200, "sell", 2010.0, 0.1, 0.0, 602, coid=2, fee_source="explicit_zero"))
        self.store.ingest()
        session = self.store.summary(now_ms=2000)["session"]
        v = session["valuation"]
        self.assertEqual((v["lighter"]["fees"], v["lighter"]["quality"]), ("estimated", "estimated"))
        self.assertIn("lighter_actual_fee", v["missing"])
        self.assertNotIn("lighter_fee_rate", v["missing"])
        self.assertNotIn("lighter_fee_source", v["missing"])
        # A replayed trade at a new offset neither books again nor relabels.
        self.write(fill(1250, "buy", 2000.0, 0.1, 0.008, 601, fee_source="missing"))
        self.store.ingest()
        session = self.store.summary(now_ms=2000)["session"]
        self.assertEqual(session["fills"], 2)
        self.assertAlmostEqual(session["fees"], 0.008)
        self.assertEqual(session["valuation"]["lighter"]["fee_sources"],
                         {"rate_derived": 1, "explicit_zero": 1, "missing": 0, "invalid": 0,
                          "role_unknown": 0, "unknown": 0})
        self.assertEqual(session["valuation"]["lighter"]["fees"], "estimated")
        # An unrecognized label is stored as unknown, never promoted.
        self.write(fill(1300, "buy", 2000.0, 0.1, 0.0, 603, coid=3, fee_source="exact"))
        self.store.ingest()
        self.assertIsNone(self.store.fills()[0]["fee_source"])
        v = self.store.summary(now_ms=2000)["session"]["valuation"]
        self.assertEqual((v["lighter"]["fee_sources"]["unknown"], v["lighter"]["fees"]), (1, "unknown"))
        self.assertIn("lighter_fee_source", v["missing"])

    def test_old_fill_table_gains_fee_source_as_unknown(self):
        import sqlite3
        path = self.dir / "old_fills.sqlite3"
        old = TelemetryStore(str(path), self.dir)
        self.write({"type": "session", "ts_ms": 1000, "exchange": "lighter", "symbol": "ETH"},
                   fill(1100, "buy", 2000.0, 0.1, 0.0, 951, fee_source="explicit_zero"))
        old.ingest()
        old.db.close()
        db = sqlite3.connect(path)
        db.execute("ALTER TABLE fills DROP COLUMN fee_source")
        db.commit()
        db.close()
        store = TelemetryStore(str(path), self.dir)
        try:
            self.write(fill(1200, "sell", 2000.0, 0.1, 0.0, 952, coid=2, fee_source="explicit_zero"))
            store.ingest()
            self.assertEqual({r["trade_id"]: r["fee_source"] for r in store.fills()},
                             {951: None, 952: "explicit_zero"})
            session = store.summary(now_ms=2000)["session"]
            self.assertEqual(session["fills"], 2)
            lighter = session["valuation"]["lighter"]
            self.assertEqual((lighter["fee_sources"]["unknown"], lighter["fees"]), (1, "unknown"))
        finally:
            store.db.close()

    def load_fixture(self, name):
        # Produced by the Rust TelemetrySink (see check_fixture in telemetry_sink.rs).
        self.file.write_text((Path(__file__).parent / "fixtures" / name).read_text())
        self.store.ingest()

    def test_writer_mark_evidence_ages_across_repeated_snapshots(self):
        self.load_fixture("mark_session.jsonl")
        early = self.store.summary(now_ms=6_000)["session"]
        late = self.store.summary(now_ms=3_606_000)["session"]
        for session in (early, late):
            self.assertAlmostEqual(session["unrealized"], 0.5)
            mark = session["valuation"]["mark"]
            # The newest snapshot (written at 4000) re-exported the 1400 sample.
            self.assertEqual((mark["source"], mark["snapshot_ms"], mark["sampled_ms"],
                              mark["feed_ms"]), ("external_median", 4000, 1500, 1400))
            self.assertEqual(mark["freshness"], "historical")  # session stopped
            self.assertNotIn("mark_time", session["valuation"]["missing"])
        self.assertEqual(early["valuation"]["mark"]["feed_age_ms"], 4_600)
        self.assertEqual(late["valuation"]["mark"]["feed_age_ms"], 3_604_600)
        self.assertEqual(late["valuation"]["mark"]["sample_age_ms"], 3_604_500)

    def test_mark_evidence_clock_mismatch_and_garbage_are_never_fresh(self):
        self.write({"type": "session", "ts_ms": 1000, "exchange": "lighter", "symbol": "ETH"},
                   fill(1100, "buy", 2000.0, 0.1, 0.0, 801),
                   {"type": "snapshot", "ts_ms": 1200, "mid": 2005.0,
                    "mark_source": "external_median", "mark_sampled_ms": 1150,
                    "mark_feed_ms": 9_000})
        self.store.ingest()
        mark = self.store.summary(now_ms=5_000)["session"]["valuation"]["mark"]
        self.assertEqual((mark["freshness"], mark["feed_age_ms"]), ("clock_mismatch", None))
        self.write({"type": "snapshot", "ts_ms": 1300, "mid": 2005.0, "mark_source": "made_up",
                    "mark_sampled_ms": "1200", "mark_feed_ms": -5})
        self.store.ingest()
        mark = self.store.summary(now_ms=5_000)["session"]["valuation"]["mark"]
        self.assertEqual((mark["source"], mark["sampled_ms"], mark["feed_ms"], mark["freshness"]),
                         ("unknown", None, None, "unknown"))
        self.write({"type": "snapshot", "ts_ms": 1400, "mid": 2005.0,
                    "mark_source": "lighter_book", "mark_sampled_ms": 1350, "mark_feed_ms": 1300})
        self.store.ingest()
        mark = self.store.summary(now_ms=5_000)["session"]["valuation"]["mark"]
        self.assertEqual((mark["source"], mark["freshness"], mark["feed_age_ms"]),
                         ("lighter_book", "observed", 3_700))
        self.write({"type": "snapshot", "ts_ms": 1500, "mid": 2005.0,
                    "mark_source": "local_touch_micro", "mark_sampled_ms": 1450, "mark_feed_ms": 1400})
        self.store.ingest()
        mark = self.store.summary(now_ms=5_000)["session"]["valuation"]["mark"]
        self.assertEqual((mark["source"], mark["freshness"], mark["feed_age_ms"]),
                         ("local_touch_micro", "observed", 3_600))

    def test_every_contradicting_clock_order_is_a_mismatch_without_ages(self):
        now = 5_000
        cases = [  # (sampled, feed, stopped) -> freshness, sample age, feed age
            ((6_000, 4_000, False), ("clock_mismatch", None, None)),  # feed <= now < sample
            ((4_000, 4_500, False), ("clock_mismatch", None, None)),  # feed > sample
            ((4_000, 9_000, False), ("clock_mismatch", None, None)),  # feed > now
            ((9_000, 9_500, True), ("clock_mismatch", None, None)),   # stopped still checked
            ((None, 4_000, False), ("unknown", None, 1_000)),          # sample time missing
            ((4_500, None, False), ("unknown", 500, None)),            # feed time missing
            ((4_500, 4_000, False), ("observed", 500, 1_000)),
            ((4_500, 4_000, True), ("historical", 500, 1_000)),
            ((5_000, 5_000, False), ("observed", 0, 0)),               # equal is consistent
        ]
        for (sampled, feed, stopped), expected in cases:
            with self.subTest(sampled=sampled, feed=feed, stopped=stopped):
                self.tearDown(); self.setUp()
                records = [{"type": "session", "ts_ms": 1000, "exchange": "lighter", "symbol": "ETH"},
                           fill(1100, "buy", 2000.0, 0.1, 0.0, 811),
                           {"type": "snapshot", "ts_ms": 1200, "mid": 2005.0,
                            "mark_source": "lighter_book", "mark_sampled_ms": sampled,
                            "mark_feed_ms": feed}]
                if stopped:
                    records.append({"type": "stopped", "ts_ms": 1300})
                self.write(*records)
                self.store.ingest()
                session = self.store.summary(now_ms=now)["session"]
                mark = session["valuation"]["mark"]
                self.assertEqual((mark["freshness"], mark["sample_age_ms"], mark["feed_age_ms"]),
                                 expected)
                # Raw evidence is kept for diagnosis; valuation is unchanged.
                self.assertEqual((mark["sampled_ms"], mark["feed_ms"]), (sampled, feed))
                self.assertAlmostEqual(session["unrealized"], 0.5)

    def test_old_snapshot_table_gains_mark_and_daily_loss_columns_as_unknown(self):
        import sqlite3
        path = self.dir / "old_mark.sqlite3"
        old = TelemetryStore(str(path), self.dir)
        self.write({"type": "session", "ts_ms": 1000, "exchange": "lighter", "symbol": "ETH"},
                   fill(1100, "buy", 2000.0, 0.1, 0.0, 901),
                   {"type": "snapshot", "ts_ms": 1200, "mid": 2005.0})
        old.ingest()
        old.db.close()
        db = sqlite3.connect(path)
        daily_loss = ("daily_loss_day_start_ms", "daily_loss_start_equity", "daily_loss_limit_pct")
        for name in ("mark_source", "mark_sampled_ms", "mark_feed_ms", *daily_loss):
            db.execute(f"ALTER TABLE snapshots DROP COLUMN {name}")
        db.commit()
        db.close()
        store = TelemetryStore(str(path), self.dir)
        try:
            summary = store.summary(now_ms=2_000)
            session = summary["session"]
            self.assertTrue(all(summary["snapshot"][name] is None for name in daily_loss))
            mark = session["valuation"]["mark"]
            self.assertEqual((mark["snapshot_ms"], mark["feed_ms"], mark["freshness"]),
                             (1200, None, "unknown"))
            self.assertAlmostEqual(session["unrealized"], 0.5)
            self.assertIn("mark_time", session["valuation"]["missing"])
        finally:
            store.db.close()

    def test_snapshot_ingests_quota_and_defense_regime_and_keeps_unknown_as_null(self):
        self.write(
            {"type": "session", "ts_ms": 1000, "exchange": "lighter", "symbol": "ETH"},
            {"type": "snapshot", "ts_ms": 1100, "mid": 2000.0,
             "volume_quota_remaining": 180, "defense_regime": "reduce_only"},
            {"type": "snapshot", "ts_ms": 1200, "mid": 2001.0,
             "volume_quota_remaining": True, "defense_regime": "bogus_regime"},
        )
        self.store.ingest()
        rows = self.store.db.execute(
            "SELECT ts_ms, volume_quota_remaining, defense_regime FROM snapshots ORDER BY ts_ms"
        ).fetchall()
        self.assertEqual(
            [(r["ts_ms"], r["volume_quota_remaining"], r["defense_regime"]) for r in rows],
            [(1100, 180, "reduce_only"), (1200, None, None)],
        )

DASHBOARD_FIXTURES = Path(__file__).resolve().parents[2] / "dashboard" / "src" / "fixtures"


class DashboardContractTest(unittest.TestCase):
    """Real summaries the React tests consume; regenerate with ALEPH_WRITE_FIXTURES=1.

    Inputs are the Rust-written session fixtures (plus one legacy session
    without mark evidence); `now_ms` is fixed so ages are deterministic.
    """
    LEGACY = [{"type": "session", "ts_ms": 1000, "exchange": "lighter", "symbol": "ETH",
               "account": 7, "network": "Testnet", "hedge_account": "0xaa"},
              fill(1100, "buy", 2000.0, 0.1, 0.0, 1),
              {"type": "snapshot", "ts_ms": 1200, "mid": 2005.0, "equity": 1000.0,
               "session_start_equity": 1000.0}]
    # Strategy clock ahead of the monitor: feed 4000 <= now 5000 < sample 6000.
    # A current writer's fill, so only the mark's freshness is unknown.
    SKEWED = [LEGACY[0], fill(1100, "buy", 2000.0, 0.1, 0.0, 1, fee_source="explicit_zero"),
              {"type": "snapshot", "ts_ms": 6_100, "mid": 2005.0, "equity": 1000.0,
               "session_start_equity": 1000.0, "mark_source": "lighter_book",
               "mark_sampled_ms": 6_000, "mark_feed_ms": 4_000}]
    CASES = {"cost": ("cost_session.jsonl", 10_000), "mark": ("mark_session.jsonl", 3_606_000),
             "legacy": (LEGACY, 10_000), "skewed": (SKEWED, 5_000)}

    def query(self, source, ask):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "s.jsonl"
            if isinstance(source, str):
                path.write_text((Path(__file__).parent / "fixtures" / source).read_text())
            else:
                path.write_text("".join(json.dumps(r) + "\n" for r in source))
            store = TelemetryStore(":memory:", Path(tmp))
            try:
                store.ingest()
                return json.loads(json.dumps(ask(store)))
            finally:
                store.db.close()

    def check(self, name, produced):
        import os
        path = DASHBOARD_FIXTURES / f"{name}.json"
        if os.environ.get("ALEPH_WRITE_FIXTURES"):
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(json.dumps(produced, indent=1, sort_keys=True) + "\n")
        with self.subTest(name):
            self.assertEqual(produced, json.loads(path.read_text()))

    def test_dashboard_fixtures_match_the_store(self):
        for name, (source, now_ms) in self.CASES.items():
            self.check(f"summary_{name}",
                       self.query(source, lambda store, now_ms=now_ms: store.summary(now_ms=now_ms)))

    def test_fill_fixture_matches_the_store(self):
        # The feeder's fee-source session as /api/fills returns it.
        self.check("fills_fee_sources",
                   self.query("fee_source_session.jsonl", lambda store: store.fills()))


class MultiSessionAndMarketsTest(unittest.TestCase):
    def test_session_scoped_summary_and_pnl_series_isolate_multi_scenario_sessions(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "s_quant.jsonl").write_text("\n".join(json.dumps(r) for r in (
                {"type": "session", "ts_ms": 1_000, "exchange": "lighter", "symbol": "ETH", "account": 10},
                fill(10_000, "buy", 2000.0, 0.1, 0.0, 101, fee_source="explicit_zero"),
                fill(70_000, "sell", 2020.0, 0.1, 0.0, 102, coid=2, fee_source="explicit_zero"),
                {"type": "snapshot", "ts_ms": 75_000, "equity": 1002.0, "session_start_equity": 1000.0,
                 "mid": 2020.0, "daily_loss_day_start_ms": 0, "daily_loss_start_equity": 1000.0,
                 "daily_loss_limit_pct": 0.03},
            )) + "\n")
            (root / "s_hedge.jsonl").write_text("\n".join(json.dumps(r) for r in (
                {"type": "session", "ts_ms": 80_000, "exchange": "lighter", "symbol": "BTC",
                 "account": 20, "hedge_account": "0x1111111111111111111111111111111111111111"},
                fill(90_000, "buy", 68000.0, 0.01, 0.0, 201, fee_source="explicit_zero"),
                fill(130_000, "sell", 68100.0, 0.01, 0.0, 202, coid=2, fee_source="explicit_zero"),
                {"type": "snapshot", "ts_ms": 135_000, "equity": 501.0, "session_start_equity": 500.0,
                 "mid": 68100.0},
            )) + "\n")
            store = TelemetryStore(":memory:", root)
            try:
                store.ingest()
                # Default summary points to latest session (`s_hedge`) with global windows.
                latest = store.summary(now_ms=140_000)
                self.assertEqual(latest["session"]["session_id"], "s_hedge")
                self.assertEqual(latest["windows"]["all"]["fills"], 4)

                # Scoped summary isolates `s_quant` session, snapshot, and windows.
                quant = store.summary(now_ms=140_000, session_id="s_quant")
                self.assertEqual(quant["session"]["session_id"], "s_quant")
                self.assertAlmostEqual(quant["session"]["net_pnl"], 2.0)
                self.assertEqual(quant["windows"]["all"]["fills"], 2)
                self.assertAlmostEqual(quant["windows"]["all"]["net_pnl"], 2.0)
                self.assertAlmostEqual(quant["snapshot"]["daily_loss_limit_pct"], 0.03)

                # Scoped PnL series isolates `s_quant` buckets and equity snapshots.
                series_q = store.pnl_series("1m", since_ms=0, session_id="s_quant")
                self.assertEqual([r["cumulative"] for r in series_q["pnl"]], [0.0, 2.0])
                self.assertEqual([r["equity"] for r in series_q["equity"]], [1002.0])

                series_h = store.pnl_series("1m", since_ms=0, session_id="s_hedge")
                self.assertEqual([r["cumulative"] for r in series_h["pnl"]], [0.0, 1.0])
                self.assertEqual([r["equity"] for r in series_h["equity"]], [501.0])

                with self.assertRaises(ValueError):
                    store.summary(now_ms=140_000, session_id="missing_session")
                with self.assertRaises(ValueError):
                    store.pnl_series("1m", session_id="missing_session")

                # /api/sessions returns full per-session valuation, open_orders, and equity_change
                # matching /api/summary?session=<id> (`MON-H1`) without coercing missing marks to 0.
                rows = {row["session_id"]: row for row in store.sessions(now_ms=140_000)}
                self.assertEqual(set(rows), {"s_quant", "s_hedge"})
                self.assertEqual(rows["s_quant"], quant["session"])
                self.assertEqual(rows["s_hedge"], latest["session"])
                self.assertAlmostEqual(rows["s_quant"]["total_pnl"], 2.0)
                self.assertAlmostEqual(rows["s_quant"]["equity_change"], 2.0)
                self.assertEqual(rows["s_quant"]["valuation"]["lighter"]["unrealized"], "flat")
            finally:
                store.db.close()

    def test_monitor_markets_reads_binance_lighter_hyperliquid_shm_slots(self):
        import os
        import monitor

        self.assertIn(("Binance", "ETH", 6, 1002), monitor.MARKETS)
        self.assertIn(("Binance", "BTC", 6, 1001), monitor.MARKETS)
        self.assertIn(("Lighter", "BTC", 2, 1001), monitor.MARKETS)

        with tempfile.TemporaryDirectory() as tmp:
            shm_path = Path(tmp) / "matrix"
            now_ns = 1_700_000_000 * 10**9
            max_offset = monitor.NUM_SYMBOLS * 8 + (1004 * monitor.NUM_EXCHANGES) * monitor.BBO.size
            shm_path.write_bytes(b"\x00" * max_offset)
            fd = os.open(shm_path, os.O_RDWR)
            try:
                # Write valid Binance ETH BBO (exchange=6, symbol=1002) with even seqlock seq=4
                offset = monitor.NUM_SYMBOLS * 8 + (1002 * monitor.NUM_EXCHANGES + 6) * monitor.BBO.size
                packed = monitor.BBO.pack(4, 1, 6, 1002, now_ns - 25_000_000, 2500.0, 12.5, 2500.1, 8.0)
                os.pwrite(fd, packed, offset)
                quote = monitor.read_quote(fd, 6, 1002, now_ns)
                self.assertEqual(quote["state"], "live")
                self.assertEqual(quote["age_ms"], 25)
                self.assertAlmostEqual(quote["bid"], 2500.0)
                self.assertAlmostEqual(quote["ask"], 2500.1)

                # Write valid Arcus NVDA (exchange=7, symbol=1003) and TradeXYZ NVDA (exchange=10, symbol=1003)
                for exch_id, bid_px, ask_px in ((7, 145.10, 145.15), (8, 2501.0, 2501.2), (9, 145.00, 145.10), (10, 145.05, 145.12)):
                    sym_id = 1002 if exch_id == 8 else 1003
                    slot_off = monitor.NUM_SYMBOLS * 8 + (sym_id * monitor.NUM_EXCHANGES + exch_id) * monitor.BBO.size
                    os.pwrite(fd, monitor.BBO.pack(2, 1, exch_id, sym_id, now_ns - 10_000_000, bid_px, 5.0, ask_px, 6.0), slot_off)
                    q = monitor.read_quote(fd, exch_id, sym_id, now_ns)
                    self.assertEqual(q["state"], "live")
                    self.assertAlmostEqual(q["bid"], bid_px)

                # Odd seqlock sequence (write in progress) is rejected
                odd_packed = monitor.BBO.pack(5, 1, 6, 1002, now_ns - 25_000_000, 2500.0, 12.5, 2500.1, 8.0)
                os.pwrite(fd, odd_packed, offset)
                self.assertIsNone(monitor.read_quote(fd, 6, 1002, now_ns))

                # Configured feeder.toml parses all multi-venue sections cleanly
                feeder_toml = Path(__file__).resolve().parent / "feeder.toml"
                cfg_rows = monitor.configured_markets(feeder_toml)
                venues = {(v, s, e) for v, s, e, _, _ in cfg_rows}
                self.assertIn(("Arcus", "NVDA", 7), venues)
                self.assertIn(("LighterRH", "ETH", 8), venues)
                self.assertIn(("Entropy", "NVDA", 9), venues)
                self.assertIn(("TradeXYZ", "NVDA", 10), venues)

                # Primary non-BTC/ETH symbols (e.g. SOL) on core venues must stay non-optional,
                # whereas feeder_public_symbols and RWA/USDG sections stay optional.
                sol_cfg = Path(tmp) / "feeder_sol.toml"
                sol_cfg.write_text(
                    '[symbols]\nSOL = 1005\nNVDA = 1003\n\n'
                    '[lighter]\nfeeder_enabled = true\n'
                    'feeder_symbols = { SOL = 2 }\n'
                    'feeder_public_symbols = { NVDA = 110 }\n\n'
                    '[binance]\nfeeder_enabled = true\n'
                    'feeder_symbols = { SOL = "SOLUSDT" }\n\n'
                    '[arcus]\nfeeder_enabled = true\n'
                    'feeder_symbols = { SOL = "SOL-USD" }\n',
                    encoding="utf-8",
                )
                sol_rows = {(v, s): opt for v, s, _, _, opt in monitor.configured_markets(sol_cfg)}
                self.assertIs(sol_rows[("Lighter", "SOL")], False)
                self.assertIs(sol_rows[("Binance", "SOL")], False)
                self.assertIs(sol_rows[("Lighter", "NVDA")], True)
                self.assertIs(sol_rows[("Arcus", "SOL")], True)
            finally:
                os.close(fd)

    def test_prune_expired_removes_old_stopped_sessions_and_preserves_latest_snapshot(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            day_ms = 86_400_000
            now_ms = 30 * day_ms
            (root / "old_stopped.jsonl").write_text("\n".join(json.dumps(r) for r in (
                {"type": "session", "ts_ms": 1_000, "exchange": "lighter", "symbol": "ETH", "account": 1},
                fill(10_000, "buy", 2000.0, 0.1, 0.0, 101, fee_source="explicit_zero"),
                {"type": "snapshot", "ts_ms": 15_000, "equity": 1000.0, "session_start_equity": 1000.0, "mid": 2000.0},
                {"type": "stopped", "ts_ms": 20_000},
            )) + "\n")
            (root / "active_long.jsonl").write_text("\n".join(json.dumps(r) for r in (
                {"type": "session", "ts_ms": 1_000, "exchange": "arcus_xyz_basis", "symbol": "ANT-USD", "account": 0},
                {"type": "snapshot", "ts_ms": 15_000, "equity": 500.0, "session_start_equity": 500.0, "mid": 88.5},
                {"type": "snapshot", "ts_ms": 25_000, "equity": 502.0, "session_start_equity": 500.0, "mid": 88.6},
            )) + "\n")
            store = TelemetryStore(":memory:", root)
            try:
                store.ingest()
                noop = store.prune_expired(retention_days=0, now_ms=now_ms)
                self.assertEqual(noop["sessions"], 0)
                stats = store.prune_expired(retention_days=14, now_ms=now_ms)
                self.assertEqual(stats["sessions"], 1)
                self.assertEqual(stats["fills"], 1)
                self.assertEqual(stats["snapshots"], 2)  # old_stopped snap + active_long's older snap
                self.assertIsNone(store.session("old_stopped"))
                self.assertIsNotNone(store.session("active_long"))
                # Latest snapshot on surviving session stays intact even when older than cutoff.
                active_summary = store.summary(now_ms=now_ms, session_id="active_long")
                self.assertAlmostEqual(active_summary["snapshot"]["equity"], 502.0)
                # Checkpoint was retained while old_stopped.jsonl stays on disk -> zero re-ingest.
                self.assertEqual(store.ingest(), 0)
                self.assertIsNone(store.session("old_stopped"))
            finally:
                store.db.close()

    def test_prune_telemetry_dir_enforces_age_max_bytes_and_active_grace(self):
        import os
        from telemetry_store import prune_telemetry_dir

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            now_secs = 2_000_000.0
            specs = (
                ("too_old.jsonl", 50, now_secs - 8 * 86_400),
                ("closed_oldest.jsonl", 60, now_secs - 2 * 86_400),
                ("closed_newer.jsonl", 60, now_secs - 3_600),
                ("still_active.jsonl", 80, now_secs - 60),
            )
            for name, size, mtime in specs:
                p = root / name
                p.write_bytes(b"x" * size)
                os.utime(p, (mtime, mtime))

            res = prune_telemetry_dir(
                root, max_age_days=7, max_total_bytes=150, active_grace_secs=600, now_secs=now_secs
            )
            self.assertEqual(res["deleted"], ["too_old.jsonl", "closed_oldest.jsonl"])
            self.assertEqual(res["deleted_bytes"], 110)
            self.assertEqual(res["remaining_bytes"], 140)
            self.assertFalse((root / "too_old.jsonl").exists())
            self.assertFalse((root / "closed_oldest.jsonl").exists())
            self.assertTrue((root / "closed_newer.jsonl").exists())
            self.assertTrue((root / "still_active.jsonl").exists())

    def test_monitor_multivenue_parsers_accounts_and_disk_status(self):
        import unittest.mock
        import monitor

        hl_addr = "0x1111111111111111111111111111111111111111"
        arc_addr = "0x2222222222222222222222222222222222222222"
        bulk_key = "FuueqefENiGEW6uMqZQgmwjzgpnb85EgUcZa5Em4PQh7"

        accts = monitor.Accounts({
            "ALEPH_MONITOR_LIGHTER_INDEX": "42",
            "ALEPH_MONITOR_LIGHTER_RH_INDEX": "43",
            "ALEPH_MONITOR_HL_ADDRESS": hl_addr,
            "ALEPH_MONITOR_ARCUS_ADDRESS": arc_addr,
            "ALEPH_MONITOR_ARCUS_INDEX": "1",
            "ALEPH_MONITOR_BULK_PUBKEY": bulk_key,
        })
        self.assertEqual(
            set(accts.snapshots),
            {"Lighter", "LighterRH", "Hyperliquid", "TradeXYZ", "Entropy", "Arcus", "BULK"},
        )
        self.assertTrue(all(v["state"] == "waiting" for v in accts.snapshots.values()))
        # TradeXYZ & Entropy inherit ALEPH_MONITOR_HL_ADDRESS automatically when omitted.
        by_name = {name: target for name, target, _ in accts.sources}
        self.assertEqual(by_name["TradeXYZ"], hl_addr)
        self.assertEqual(by_name["Entropy"], hl_addr)
        self.assertEqual(by_name["Arcus"], f"{arc_addr}:1")

        def fake_request(url, body=None):
            if "api.rh.lighter.xyz" in url:
                return {"code": 200, "accounts": [{"index": 43, "positions": [
                    {"symbol": "ETH", "position": "0.5", "sign": -1, "avg_entry_price": "3000", "unrealized_pnl": "12.5"}
                ]}]}
            if "api.hyperliquid.xyz" in url:
                return {"assetPositions": [{"position": {"coin": f"{body['dex']}:ANT", "szi": "-10", "entryPx": "88.5", "unrealizedPnl": "3.2"}}]}
            if "api.arcus.xyz" in url:
                return {"positions": {"88": {"marketId": 88, "marketDisplayName": "ANT-USD", "size": "10"}}}
            if "bulk.trade" in url:
                return [{"fullAccount": {"positions": [
                    {"symbol": "NVDA-USD", "size": "2.0", "entryPrice": "129.0", "unrealizedPnl": "1.5"},
                    {"symbol": "BTC-USD", "size": "0", "entryPrice": "0", "unrealizedPnl": "0"},
                ]}}]
            raise AssertionError(f"unexpected url: {url}")

        with unittest.mock.patch.object(monitor, "request_json", side_effect=fake_request):
            rh_rows = monitor.lighter_positions("43", "https://api.rh.lighter.xyz")
            self.assertEqual(rh_rows[0]["size"], "-0.5")
            xyz_rows = monitor.hyperliquid_positions(hl_addr, "xyz")
            self.assertEqual(xyz_rows[0]["symbol"], "xyz:ANT")
            arc_rows = monitor.arcus_positions(f"{arc_addr}:1")
            self.assertEqual(arc_rows[0]["symbol"], "ANT-USD")
            self.assertEqual(arc_rows[0]["unrealized_pnl"], "0")
            bulk_rows = monitor.bulk_positions(bulk_key)
            self.assertEqual(len(bulk_rows), 1)
            self.assertEqual(bulk_rows[0]["symbol"], "NVDA-USD")

        disk = monitor.disk_status(("/",))
        self.assertIsNotNone(disk)
        self.assertGreater(disk["total_gib"], 0.0)

    def test_sync_monitor_env_extracts_public_ids_and_preserves_operational_keys(self):
        import os
        import sync_monitor_env

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            src = root / ".env.private"
            dst = root / "monitor.env"
            priv_tag = "PRIVATE_" + "KEY"
            src.write_text(
                "\n".join([
                    "ARCUS_ACCOUNT_ADDRESS=0x2222222222222222222222222222222222222222",
                    "ARCUS_ACCOUNT_INDEX=2",
                    f"ARCUS_API_{priv_tag}=" + "ab" * 32,
                    "HYPERLIQUID_ACCOUNT_ADDRESS=0x1111111111111111111111111111111111111111",
                    f"HYPERLIQUID_AGENT_{priv_tag}=" + "cd" * 32,
                    "LIGHTER_ACCOUNT_INDEX=77",
                    f"LIGHTER_{priv_tag}=" + "ef" * 40,
                    "LIGHTER_RH_ACCOUNT_INDEX=88",
                    f"LIGHTER_RH_{priv_tag}=" + "12" * 40,
                    "BULK_ACCOUNT_PUBKEY=FuueqefENiGEW6uMqZQgmwjzgpnb85EgUcZa5Em4PQh7",
                    f"BULK_{priv_tag}=syntheticbase58token",
                    "OKX_API_SECRET=nevercopy",
                ]) + "\n",
                encoding="utf-8",
            )
            dst.write_text("ALEPH_MONITOR_CONTROL_TOKEN=keep-me-123\nALEPH_MONITOR_PORT=8765\n", encoding="utf-8")
            rendered = sync_monitor_env.sync_monitor_env_file(src, dst)
            mode = os.stat(dst).st_mode & 0o777
            self.assertEqual(mode, 0o600)
            self.assertIn("ALEPH_MONITOR_CONTROL_TOKEN=keep-me-123", rendered)
            self.assertIn("ALEPH_MONITOR_LIGHTER_INDEX=77", rendered)
            self.assertIn("ALEPH_MONITOR_LIGHTER_RH_INDEX=88", rendered)
            self.assertIn("ALEPH_MONITOR_HL_ADDRESS=0x1111111111111111111111111111111111111111", rendered)
            self.assertIn("ALEPH_MONITOR_ARCUS_ADDRESS=0x2222222222222222222222222222222222222222", rendered)
            self.assertIn("ALEPH_MONITOR_ARCUS_INDEX=2", rendered)
            self.assertIn("ALEPH_MONITOR_BULK_PUBKEY=FuueqefENiGEW6uMqZQgmwjzgpnb85EgUcZa5Em4PQh7", rendered)
            for forbidden in ("ab" * 8, "cd" * 8, "ef" * 8, "12" * 8, "syntheticbase58token", "nevercopy", "PRIVATE", "SECRET"):
                self.assertNotIn(forbidden, rendered)


if __name__ == "__main__":
    unittest.main()
