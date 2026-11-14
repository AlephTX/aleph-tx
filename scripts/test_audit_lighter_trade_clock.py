import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from archive_public_capture import ingest
from audit_lighter_trade_clock import audit, write_report
from public_replay_sources import trade_events


class LighterClockAuditTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        self.t = 1_800_000_000_000
        self.source = "lighter_trades"
        self.venue = "lighter"

    def row(self, offset, payload=None, event="message"):
        return {"source": self.source, "event": event, "received_at_ms": self.t + offset,
                "available_at_ms": self.t + offset + 1, "payload": payload or {}}

    def trade(self, identity, **kwargs):
        return {"trade_id": identity, "market_id": 1, "timestamp": self.t,
                "price": "100", "size": "0.1", "is_maker_ask": True, **kwargs}

    def message(self, offset, trades, subscribed=False):
        return self.row(offset, {"type": "subscribed/trade" if subscribed else "update/trade",
                                "channel": "trade:1", "trades": trades})

    def archive(self, rows):
        capture = self.root / "capture.ndjson"
        capture.write_text("".join(json.dumps(row) + "\n" for row in [
            *rows, self.row(10_000, event="end") | {"source": "collector"}]))
        archive = self.root / "archive"
        ingest(capture, archive)
        return archive

    def test_execution_microseconds_retained_and_existing_match_clock_unchanged(self):
        trade = self.trade(1, transaction_time=self.t * 1000 + 32_065, block_height=123,
                           sequence_index=0)
        archive = self.archive([self.message(1, [], True), self.message(100, [trade])])
        result = audit(archive, self.venue, "1")
        self.assertTrue(result["execution_fields_complete"])
        self.assertEqual(result["distributions"]["execution_minus_block_us"]["sum_us"], 32_065)
        self.assertEqual(result["distributions"]["receipt_minus_execution_us"]["min_us"], 67_935)
        self.assertEqual(result["timing_fields"]["sequence_index"]["present"], 1)
        self.assertFalse(result["client_sequencer_offset_calibrated"])
        self.assertFalse(result["execution_qualified"])
        self.assertFalse(result["source_timestamp_substituted"])
        prints = [r for r in trade_events(archive, self.venue, "1", 0) if r["kind"] == "trade"]
        self.assertEqual(prints[0]["source_ms"], self.t)
        self.assertNotIn("trade_id", json.dumps(result))
        self.assertNotIn('"price"', json.dumps(result))

    def test_legacy_missing_zero_and_invalid_execution_fields_remain_unknown(self):
        archive = self.archive([self.message(1, [], True), self.message(100, [
            self.trade(1), self.trade(2, transaction_time=0),
            self.trade(3, transaction_time=True), self.trade(4, transaction_time="1800000000000000")])])
        result = audit(archive, self.venue, "1")
        self.assertEqual(result["counts"]["missing_execution_time"], 2)
        self.assertEqual(result["counts"]["invalid_execution_time"], 2)
        self.assertEqual(result["execution_field_coverage"], 0)
        self.assertIsNone(result["distributions"]["receipt_minus_execution_us"]["min_us"])
        self.assertFalse(result["execution_fields_complete"])

    def test_proxy_bias_and_future_clock_observations_are_not_network_latency(self):
        archive = self.archive([self.message(1, [], True), self.message(100, [
            self.trade(1, transaction_time=self.t * 1000 + 200_001),
            self.trade(2, transaction_time=self.t * 1000 - 1)])])
        result = audit(archive, self.venue, "1")
        self.assertEqual(result["counts"]["block_execution_delta_exceeds_guard"], 1)
        self.assertEqual(result["counts"]["execution_after_local_receipt"], 1)
        self.assertEqual(result["counts"]["execution_before_block_time"], 1)
        self.assertFalse(result["clock_guard_calibrated"])
        self.assertFalse(result["execution_fields_complete"])

    def test_snapshot_duplicate_and_window_selection_follow_receipt_order(self):
        first = self.trade(1, transaction_time=self.t * 1000 + 1)
        second = self.trade(2, transaction_time=self.t * 1000 + 2)
        archive = self.archive([self.message(1, [first], True), self.message(10, [first]),
                                self.message(20, [second]), self.message(30, [second])])
        result = audit(archive, self.venue, "1", start_ms=self.t + 20, end_ms=self.t + 31)
        self.assertEqual(result["counts"]["subscription_acks"], 0)
        self.assertEqual(result["counts"]["live_unique_trades"], 1)
        self.assertEqual(result["counts"]["duplicate_trade_records"], 0)
        self.assertTrue(result["execution_fields_complete"])
        empty = audit(archive, self.venue, "1", start_ms=self.t + 31, end_ms=self.t + 32)
        self.assertEqual(empty["counts"]["duplicate_trade_records"], 1)
        self.assertIsNone(empty["execution_field_coverage"])
        self.assertFalse(empty["execution_fields_complete"])

    def test_disconnect_conflict_and_domain_identity_remain_explicit(self):
        self.venue, self.source = "lighter_robinhood", "lighter_robinhood_trades"
        trade = self.trade(1, transaction_time=self.t * 1000 + 1)
        archive = self.archive([self.message(1, [], True), self.message(10, [trade]),
            self.message(11, [trade | {"transaction_time": self.t * 1000 + 2}]),
            self.row(20, event="disconnect"), self.message(30, [self.trade(2)])])
        result = audit(archive, self.venue, "1")
        self.assertEqual(result["counts"]["conflicting_duplicate_records"], 1)
        self.assertEqual(result["counts"]["unacknowledged_trade_records"], 1)
        self.assertEqual(result["counts"]["feed_offline_events"], 1)
        self.assertFalse(result["execution_fields_complete"])
        core = audit(archive, "lighter", "1")
        self.assertIsNone(core["execution_field_coverage"])

    def test_archive_changed_during_audit_is_rejected(self):
        archive = self.archive([self.message(1, [], True), self.message(20, [self.trade(1)])])
        from audit_lighter_trade_clock import records
        def changed(*args, **kwargs):
            yield from records(*args, **kwargs)
            path = archive / "manifest.json"
            path.write_text(path.read_text() + "\n")
        with patch("audit_lighter_trade_clock.records", side_effect=changed):
            with self.assertRaisesRegex(ValueError, "changed"):
                audit(archive, self.venue, "1")

    def test_invalid_policy_is_rejected(self):
        for kwargs in ({"clock_guard_ms": True}, {"start_ms": self.t},
                       {"start_ms": self.t, "end_ms": self.t}):
            with self.subTest(kwargs=kwargs), self.assertRaises(ValueError):
                audit(self.root, self.venue, "1", **kwargs)
        with self.assertRaises(ValueError):
            audit(self.root, self.venue, "01")

    def test_atomic_publication_keeps_partial_results_and_overwrites_out(self):
        output = self.root / "review.json"
        with self.assertRaises(ValueError):
            write_report(output, {"partial": 1, "bad": float("nan")})
        self.assertFalse(output.exists())
        self.assertEqual(list(self.root.iterdir()), [])
        write_report(output, {"complete": True})
        before = output.read_bytes()
        with self.assertRaises(FileExistsError):
            write_report(output, {"complete": False})
        self.assertEqual(output.read_bytes(), before)
        self.assertEqual(list(self.root.iterdir()), [output])


if __name__ == "__main__":
    unittest.main()
