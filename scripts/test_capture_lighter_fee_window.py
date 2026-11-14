import copy
import gzip
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from capture_lighter_fee_window import protected_unchanged, run, seal, validate
from capture_binance_window import digest


class FeeCaptureTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        self.protected = self.root / "existing-policy.json"
        self.protected.write_text('{"frozen":true}')
        self.plan = {"capture_from_ms": 1_800_000_000_000,
                     "validation_start_ms": 1_800_000_060_000,
                     "validation_end_ms": 1_800_036_060_000,
                     "capture_until_ms": 1_800_036_090_000,
                     "disk_reserve_bytes": 16 * 2**30, "max_capture_bytes": 512 * 2**20,
                     "markets": {"lighter": [0, 1], "lighter_robinhood": [0, 1]},
                     "protected_artifacts": {str(self.protected): digest(self.protected)}}

    def test_window_and_resource_limits_fail_closed(self):
        validate(self.plan)
        for key, value in [("validation_end_ms", self.plan["validation_end_ms"] - 1),
                           ("disk_reserve_bytes", 14 * 2**30), ("max_capture_bytes", 2**30),
                           ("capture_from_ms", True), ("protected_artifacts", {})]:
            with self.subTest(key=key), self.assertRaises(ValueError):
                validate(self.plan | {key: value})
        plan = copy.deepcopy(self.plan)
        plan["markets"]["lighter_robinhood"] = [0, 0]
        with self.assertRaises(ValueError):
            validate(plan)

    def test_protected_identity_failure_prevents_child_start(self):
        binary = self.root / "perp-capture"
        binary.write_bytes(b"synthetic collector identity")
        self.plan["collector_sha256"] = digest(binary)
        (self.root / "capture-plan.json").write_text(json.dumps(self.plan))
        self.protected.write_text('{"frozen":false}')
        self.assertFalse(protected_unchanged(self.plan))
        with patch("capture_lighter_fee_window.subprocess.Popen") as start:
            with self.assertRaisesRegex(ValueError, "protected"):
                run(self.root)
            start.assert_not_called()
        self.assertFalse((self.root / "capture-driver.lock").exists())

    def capture(self, version=1, final_delta=0):
        first, last = self.plan["capture_from_ms"], self.plan["capture_until_ms"] + final_delta
        rows = []
        for index, (at, source, event) in enumerate([(first, "lighter_trades", "message"),
                                                   (last, "collector", "end")], 1):
            rows.append({"source": source, "event": event, "received_at_ms": at, "available_at_ms": at,
                         "payload": {"fee_fields_version": version}, "capture_timing": {
                             "run_id": "b" * 32, "sequence": index,
                             "connection_epoch": 0 if source == "collector" else 1,
                             "receipt_wall_ns": at * 1_000_000, "available_wall_ns": at * 1_000_000,
                             "receipt_monotonic_ns": (at - first) * 1_000_000,
                             "available_monotonic_ns": (at - first) * 1_000_000}})
        with gzip.open(self.root / "public.ndjson.gz.partial", "wt") as stream:
            stream.write("".join(json.dumps(row) + "\n" for row in rows))

    def test_seal_preserves_partial_and_is_not_strategy_validation(self):
        self.capture()
        result = seal(self.root, self.plan, "synthetic-plan-hash")
        self.assertFalse(result["strategy_validation"])
        self.assertIsNone(result["confirmed_eligible_volume"])
        self.assertEqual((self.root / "public.ndjson.gz").stat().st_ino,
                         (self.root / "public.ndjson.gz.partial").stat().st_ino)
        with self.assertRaises(FileExistsError):
            seal(self.root, self.plan, "synthetic-plan-hash")

    def test_legacy_fields_and_short_capture_cannot_seal(self):
        self.capture(version=0)
        with self.assertRaisesRegex(ValueError, "fee evidence"):
            seal(self.root, self.plan, "synthetic-plan-hash")
        self.capture(final_delta=-30_000)
        with self.assertRaisesRegex(ValueError, "incomplete"):
            seal(self.root, self.plan, "synthetic-plan-hash")
        self.assertFalse((self.root / "public.ndjson.gz").exists())


if __name__ == "__main__":
    unittest.main()
