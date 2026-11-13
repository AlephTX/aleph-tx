import copy
import json
from pathlib import Path
import sys
import tempfile
import time
import unittest

from replay_ten_hour_arbitrage import TEN_HOURS, run_replay


class TenHourRunnerTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.start = 1_800_000_000_000
        self.cutoff = self.start + 60_000 + TEN_HOURS
        self.header = {
            "pair": {
                "legs": [{"venue": "lighter"}, {"venue": "hyperliquid"}],
                "basis": {},
                "planner": {"max_leg_notional_usd": 50, "max_unhedged_notional_usd": 200},
            }
        }
        self.rows = [self.frame(t) for t in (
            self.start, self.start + 60_000, self.cutoff - 1000, self.cutoff + 2000,
        )]
        self.engine = self.root / "synthetic-engine"
        self.engine.write_text(
            f"#!{sys.executable}\n"
            "import json, sys\n"
            "header = json.loads(sys.stdin.readline())\n"
            "rows = [json.loads(line) for line in sys.stdin]\n"
            "eligible = [row for row in rows if row['now_ms'] < header['drain_until_ms']]\n"
            "drain = {'complete': rows[-1]['now_ms'] >= header['drain_until_ms'],\n"
            " 'last_execution_frame_ms': eligible[-1]['now_ms'] if eligible else None}\n"
            "print(json.dumps({'frames': len(rows), 'halted': None, 'open_phase': None,\n"
            " 'open_positions_base': ['0','0'], 'entry_signals': 0, 'paired_entries': 0,\n"
            " 'terminal_drain': drain, 'entry_diagnostics': {'header': header, 'rows': rows}}))\n"
        )
        self.engine.chmod(0o700)

    def frame(self, timestamp):
        return {"now_ms": timestamp, "legs": [{"book": {
            "available_notional": allowance, "bids": [["100", "2"]],
            "asks": [["101", "3"]], "source_ms": timestamp - 1,
            "tradable": True,
        }} for allowance in ("0", "150")]}

    def replay(self, rows=None, decompressor_exit=0, timeout=5, suffix=""):
        source = self.root / ("synthetic-public-tape" + suffix)
        source.write_text("\n".join(json.dumps(row) for row in [self.header] +
                                   (self.rows if rows is None else rows)) + "\n")
        decoder = [sys.executable, "-c",
                   "import sys; sys.stdout.buffer.write(open(sys.argv[1], 'rb').read()); "
                   "sys.stdout.buffer.flush(); sys.exit(int(sys.argv[2]))",
                   str(source), str(decompressor_exit)]
        out = self.root / ("output" + suffix)
        report = run_replay(self.engine, source, out, timeout=timeout, decompressor=decoder, drain_ms=2000)
        self.assertEqual(report, json.loads((out / "manifest.json").read_text()))
        self.assertFalse((out / "input.ndjson").is_symlink())
        self.assertFalse(report["execution_qualified"])
        self.assertFalse(report["production_qualified"])
        return report

    def test_preserves_depth_caps_only_downward_and_exposes_unexecuted_boundary(self):
        original = copy.deepcopy(self.rows)
        report = self.replay()
        self.assertTrue(report["diagnostic_run_succeeded"])
        self.assertTrue(report["compressed_archive_verified"])
        self.assertEqual(report["decision_window"]["end_ms"] -
                         report["decision_window"]["start_ms"], TEN_HOURS)
        self.assertEqual(report["frames"], 4)
        self.assertEqual(report["last_replayed_frame_ms"], self.cutoff - 1000)
        self.assertEqual(report["first_frame_at_or_after_cutoff_ms"], self.cutoff + 2000)
        self.assertEqual(report["terminal_gap_ms"], 3000)
        self.assertEqual(report["decision_cutoff_gap_ms"], 1000)
        self.assertTrue(report["terminal_execution_complete"])
        self.assertEqual(report["execution_blockers"], ["fees_and_stream_quality_unverified"])
        received = report["result"]["entry_diagnostics"]
        self.assertEqual(received["header"]["pair"]["planner"], {
            "max_leg_notional_usd": 50, "max_unhedged_notional_usd": 100,
        })
        for row, expected in zip(received["rows"], original, strict=False):
            for i, leg in enumerate(row["legs"]):
                book = leg["book"]
                self.assertEqual(book["available_notional"], ("0", "100")[i])
                for field in ("bids", "asks", "source_ms", "tradable"):
                    self.assertEqual(book[field], expected["legs"][i]["book"][field])
        for key in ("source_sha256", "engine_sha256", "runner_sha256", "config_sha256"):
            self.assertEqual(len(report[key]), 64)

    def test_terminal_source_gap_is_included_in_max_gap(self):
        rows = [self.frame(self.start), self.frame(self.start + 60_000),
                self.frame(self.cutoff + 2000)]
        report = self.replay(rows)
        self.assertEqual(report["max_observed_frame_gap_ms"], TEN_HOURS + 2000)
        self.assertEqual(report["terminal_gap_ms"], TEN_HOURS + 2000)
        self.assertFalse(report["coverage_quality_verified"])

    def test_source_shortfall_is_not_a_complete_ten_hour_diagnostic(self):
        report = self.replay(self.rows[:-1])
        self.assertFalse(report["diagnostic_run_succeeded"])
        self.assertFalse(report["source_covers_cutoff"])
        self.assertIn("source_does_not_reach_cutoff", report["failures"])

    def test_compression_footer_failure_invalidates_otherwise_successful_replay(self):
        report = self.replay(decompressor_exit=9)
        self.assertEqual(report["engine_exit_code"], 0)
        self.assertEqual(report["archive_exit_code"], 9)
        self.assertFalse(report["compressed_archive_verified"])
        self.assertFalse(report["diagnostic_run_succeeded"])
        self.assertIn("compressed_archive_verification_failed", report["failures"])

    def test_bad_input_and_early_child_exit_leave_failure_manifests(self):
        bad_rows = [self.rows[0], {"invalid": "synthetic"}]
        report = self.replay(bad_rows)
        self.assertFalse(report["diagnostic_run_succeeded"])
        self.assertTrue(report["compressed_archive_verified"])
        self.assertIn("input_or_stream_failure:KeyError", report["failures"])
        self.engine.write_text(f"#!{sys.executable}\nraise SystemExit(7)\n")
        report = self.replay(suffix="-early-exit")
        self.assertFalse(report["diagnostic_run_succeeded"])
        self.assertEqual(report["engine_exit_code"], 7)
        self.assertIsNone(report["last_replayed_frame_ms"])

    def test_blocked_writer_deadline_kills_children_and_records_failure(self):
        self.engine.write_text(f"#!{sys.executable}\nimport time\ntime.sleep(60)\n")
        rows = [self.frame(self.start + i * 100) for i in range(2000)]
        rows.append(self.frame(self.cutoff))
        before = time.monotonic()
        report = self.replay(rows, timeout=0.25)
        self.assertLess(time.monotonic() - before, 5)
        self.assertIn("runner_deadline_expired", report["failures"])
        self.assertIsNotNone(report["engine_exit_code"])
        self.assertIsNotNone(report["archive_exit_code"])
        self.assertIsNone(report["last_replayed_frame_ms"])

    def test_engine_halt_is_not_a_successful_diagnostic(self):
        self.engine.write_text(self.engine.read_text().replace("'halted': None", "'halted': 'risk limit'"))
        report = self.replay()
        self.assertFalse(report["diagnostic_run_succeeded"])
        self.assertIn("engine_halted", report["failures"])

    def test_missing_terminal_execution_evidence_is_not_accepted(self):
        self.engine.write_text(self.engine.read_text().replace("'terminal_drain': drain,", ""))
        report = self.replay()
        self.assertFalse(report["diagnostic_run_succeeded"])
        self.assertFalse(report["terminal_execution_complete"])
        self.assertIn("terminal_drain_incomplete", report["failures"])

    def test_malformed_engine_report_still_leaves_failure_manifest(self):
        self.engine.write_text(f"#!{sys.executable}\nimport sys\nsys.stdin.read()\nprint('[]')\n")
        report = self.replay()
        self.assertFalse(report["diagnostic_run_succeeded"])
        self.assertIn("runner_failure:ValueError", report["failures"])


if __name__ == "__main__":
    unittest.main()
