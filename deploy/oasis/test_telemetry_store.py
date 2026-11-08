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
        self.assertEqual(v["lighter"]["quality"], "reported")
        self.assertEqual(v["hedge"]["price_sources"], {"fill": 2, "limit": 1, "unknown": 0})
        self.assertEqual(v["hedge"]["fee_sources"], {"configured_rate": 3, "unknown": 0})
        self.assertEqual((v["hedge"]["realized"], v["hedge"]["fees"], v["hedge"]["unrealized"]),
                         ("estimated", "estimated", "missing"))
        self.assertEqual(v["combined_quality"], "missing")
        self.assertIsNone(v["mark"]["price"])
        self.assertEqual(v["missing"], ["mark_price", "hedge_fill_price", "hedge_actual_fee", "funding"])
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
        self.assertEqual(session["valuation"]["combined_quality"], "reported")

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
    SKEWED = LEGACY[:2] + [{"type": "snapshot", "ts_ms": 6_100, "mid": 2005.0, "equity": 1000.0,
                            "session_start_equity": 1000.0, "mark_source": "lighter_book",
                            "mark_sampled_ms": 6_000, "mark_feed_ms": 4_000}]
    CASES = {"cost": ("cost_session.jsonl", 10_000), "mark": ("mark_session.jsonl", 3_606_000),
             "legacy": (LEGACY, 10_000), "skewed": (SKEWED, 5_000)}

    def summary(self, source, now_ms):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "s.jsonl"
            if isinstance(source, str):
                path.write_text((Path(__file__).parent / "fixtures" / source).read_text())
            else:
                path.write_text("".join(json.dumps(r) + "\n" for r in source))
            store = TelemetryStore(":memory:", Path(tmp))
            try:
                store.ingest()
                return store.summary(now_ms=now_ms)
            finally:
                store.db.close()

    def test_dashboard_fixtures_match_the_store(self):
        import os
        for name, (source, now_ms) in self.CASES.items():
            produced = json.loads(json.dumps(self.summary(source, now_ms)))
            path = DASHBOARD_FIXTURES / f"summary_{name}.json"
            if os.environ.get("ALEPH_WRITE_FIXTURES"):
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(json.dumps(produced, indent=1, sort_keys=True) + "\n")
            with self.subTest(name):
                self.assertEqual(produced, json.loads(path.read_text()))


if __name__ == "__main__":
    unittest.main()
