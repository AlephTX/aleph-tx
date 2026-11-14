from decimal import Decimal
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from archive_public_capture import ingest
from audit_lighter_public_fees import BookMarks, audit, classify, load_policy
from public_replay_sources import trade_events

POLICY = Path(__file__).resolve().parents[1] / "examples/lighter_fee_signature_review.json"


class PublicFeeTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root, self.t = Path(temporary.name), 1_800_000_000_000

    def trade(self, identity, **fees):
        return {"trade_id": identity, "market_id": 0, "timestamp": self.t,
                "price": "100", "size": "1", "is_maker_ask": True, **fees}

    def row(self, at, trades=None, snapshot=False, version=1, event="message", source="lighter_robinhood_trades"):
        payload = {"channel": "trade:0", "type": "subscribed/trade" if snapshot else "update/trade",
                   "trades": trades or []}
        if version is not None:
            payload["fee_fields_version"] = version
        return {"source": source, "event": event, "available_at_ms": self.t + at,
                "received_at_ms": self.t + at, "payload": payload}

    def archive(self, rows):
        capture = self.root / "capture.ndjson"
        capture.write_text("".join(json.dumps(r) + "\n" for r in [*rows, self.row(10_000, event="end", source="collector")]))
        archive = self.root / "archive"
        ingest(capture, archive)
        return archive

    def test_fee_classes_are_domain_specific_and_nonzero_is_not_premium(self):
        policy = load_policy(POLICY)
        core, rh = (policy["domains"][v] for v in ("lighter", "lighter_robinhood"))
        for value, expected in [(350, "premium_schedule_match"), (150, "plus_schedule_match"),
                                (100, "standard_special_order_compatible"), (196, "unmapped_integer"),
                                (0, "explicit_zero_standard_compatible"), (-1, "unmapped_integer"),
                                (True, "invalid_integer"), ("350", "invalid_integer"), (1.5, "invalid_integer")]:
            self.assertEqual(classify({"taker_fee": value}, "taker", 1, rh), expected)
        self.assertEqual(classify({"taker_fee": 196}, "taker", 1, core), "premium_schedule_match")
        self.assertEqual(classify({}, "maker", 1, rh), "wire_omitted_zero_or_empty")
        self.assertEqual(classify({}, "maker", None, rh), "legacy_or_unverified_missing")

    def test_notional_weighting_and_unknown_qualification(self):
        archive = self.archive([self.row(1, snapshot=True), self.row(10, [
            self.trade(1, maker_fee=120, taker_fee=350),
            self.trade(2, taker_fee=100) | {"size": "3"}, self.trade(3, taker_fee=0)])])
        result = audit(archive, "lighter_robinhood", "0", POLICY)
        self.assertEqual(result["total_public_notional"], "500")
        self.assertEqual(result["groups"]["taker/premium_schedule_match"]["notional_fraction"], "0.2")
        self.assertEqual(result["groups"]["taker/standard_special_order_compatible"]["notional_fraction"], "0.6")
        self.assertIsNone(result["confirmed_eligible_volume"])
        self.assertIsNone(result["strategy_fill_eligible_fraction"])
        self.assertFalse(result["account_tier_confirmed"])
        for private in ("trade_id", "account_id", "is_maker_ask", '"price"', '"size"'):
            self.assertNotIn(private, json.dumps(result))
        # Existing replay observations do not acquire fee-based decisions.
        events = list(trade_events(archive, "lighter_robinhood", "0", 0))
        self.assertEqual(sum(e["kind"] == "trade" for e in events), 3)
        self.assertNotIn("taker_fee", json.dumps(events))

    def test_legacy_fees_are_not_reconstructed(self):
        archive = self.archive([self.row(1, snapshot=True, version=None),
                                self.row(10, [self.trade(1)], version=None)])
        result = audit(archive, "lighter_robinhood", "0", POLICY)
        self.assertEqual(result["groups"]["taker/legacy_or_unverified_missing"]["count"], 1)
        self.assertEqual(result["counts"]["fee_retaining_collector_records"], 0)

    def test_snapshot_dedup_conflicts_and_offline_records_stay_explicit(self):
        one = self.trade(1, taker_fee=350)
        two = self.trade(2, taker_fee=100)
        archive = self.archive([self.row(1, [one], snapshot=True), self.row(2, [one]),
            self.row(10, [two]), self.row(11, [two | {"taker_fee": 350}]),
            self.row(20, event="disconnect"), self.row(30, [self.trade(3)])])
        result = audit(archive, "lighter_robinhood", "0", POLICY, start_ms=self.t + 5, end_ms=self.t + 31)
        self.assertEqual(result["counts"]["live_unique_trades"], 1)
        self.assertEqual(result["counts"]["conflicting_duplicates"], 1)
        self.assertEqual(result["counts"]["unacknowledged_records"], 1)
        core = audit(archive, "lighter", "0", POLICY)
        self.assertEqual(core["groups"], {})

    @staticmethod
    def book(at, mid, valid=True):
        return {"timestamp_ms": at, "tradable": valid,
                "bids": [{"price": str(Decimal(mid) - Decimal("0.01")), "contracts": "10"}],
                "asks": [{"price": str(Decimal(mid) + Decimal("0.01")), "contracts": "10"}]}

    def test_markout_direction_no_lookahead_stale_gaps_and_end_censoring(self):
        frames = [(t, self.book(t, "100" if t < 2000 else "101")) for t in range(900, 2301, 100)]
        marks = BookMarks(frames)
        self.assertEqual(marks.mark(1000, 1000, Decimal(100), True), (Decimal(100), Decimal(100)))
        self.assertEqual(marks.mark(1000, 1000, Decimal(100), False), (Decimal(-100), Decimal(-100)))
        self.assertIsNone(marks.mark(900, 1000, Decimal(100), True))  # no earlier entry book
        self.assertIsNone(marks.mark(1500, 1000, Decimal(100), True))  # beyond coverage
        frames[5] = (1400, self.book(1400, "100", valid=False))
        self.assertIsNone(BookMarks(frames).mark(1000, 1000, Decimal(100), True))
        self.assertIsNone(BookMarks([frames[0], frames[-1]]).mark(1000, 1000, Decimal(100), True))

    def test_archive_changed_is_rejected(self):
        archive = self.archive([self.row(1, snapshot=True), self.row(10, [self.trade(1)])])
        from audit_lighter_public_fees import records
        def changed(*args, **kwargs):
            yield from records(*args, **kwargs)
            path = archive / "manifest.json"
            path.write_text(path.read_text() + "\n")
        with patch("audit_lighter_public_fees.records", side_effect=changed):
            with self.assertRaisesRegex(ValueError, "changed"):
                audit(archive, "lighter_robinhood", "0", POLICY)

    def test_invalid_policy_and_book_mapping_arguments_rejected(self):
        for kwargs in ({"start_ms": self.t}, {"max_book_age_ms": True}, {"books": self.root / "book"}):
            with self.assertRaises(ValueError):
                audit(self.root, "lighter_robinhood", "0", POLICY, **kwargs)
        policy = json.loads(POLICY.read_text())
        policy["domains"]["lighter"]["maker"]["plus_schedule_match"] = [40]
        path = self.root / "policy.json"
        path.write_text(json.dumps(policy))
        with self.assertRaises(ValueError):
            load_policy(path)


if __name__ == "__main__":
    unittest.main()
