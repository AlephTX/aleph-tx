import json
import io
from contextlib import redirect_stdout
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

from archive_public_capture import digest
from replay_trade_preflight import audit
from replay_ten_hour_scenarios import execute_turnover_grid, main, turnover_policies


class TradePreflightTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        self.tape = self.root / "synthetic.ndjson"
        self.config = {"start_ms": 10_000, "end_ms": 20_000, "maker_leg": 1,
                       "trade_delivery_grace_ms": 750, "flat_at_funding": {"settlement_guard_ms": 1000}}

    def trade(self, at=11_000, source=10_250, leg=1):
        return {"kind": "trade", "at_ms": at, "source_ms": source, "leg": leg,
                "price": "100", "contracts": "0.1", "aggressor": "buy"}

    def write(self, events, end=True):
        if end:
            events = [*events, {"kind": "end", "at_ms": self.config["end_ms"]}]
        self.tape.write_text("".join(json.dumps(event) + "\n" for event in events))

    def test_inclusive_delivery_bound_and_one_ms_violation(self):
        self.write([self.trade(), self.trade(at=11_001)])
        report = audit(self.tape, self.config)
        self.assertFalse(report["trade_timing_admitted"])
        self.assertEqual(report["blockers"], ["over_delivery_bound"])
        self.assertEqual(report["by_leg"][1]["over_delivery_bound"], 1)
        self.assertEqual(report["by_leg"][1]["first_violation_at_ms"], 11_001)
        self.assertEqual(report["input_sha256"], digest(self.tape))
        self.assertNotIn("price", json.dumps(report))

    def test_future_source_time_fails_even_inside_clock_guard(self):
        self.write([self.trade(source=11_001)])
        report = audit(self.tape, self.config)
        self.assertEqual(report["blockers"], ["future_source_times"])
        self.assertEqual(report["by_leg"][1]["min_delivery_ms"], -1)

    def test_only_prestart_prints_are_excluded_from_flat_start_timing(self):
        self.write([self.trade(source=9999), self.trade(at=11_001, source=10_000)])
        report = audit(self.tape, self.config)
        self.assertEqual(report["by_leg"][1]["ignored_prestart_prints"], 1)
        self.assertEqual(report["by_leg"][1]["over_delivery_bound"], 1)

    def test_hedge_print_delay_is_visible_without_rejecting_maker(self):
        self.write([self.trade(source=10_000, leg=0), self.trade()])
        report = audit(self.tape, self.config)
        self.assertTrue(report["trade_timing_admitted"])
        self.assertEqual(report["by_leg"][0]["over_delivery_bound"], 1)
        self.assertFalse(report["stream_quality_verified"])
        self.assertFalse(report["execution_qualified"])

    def test_feed_gap_is_evidence_not_a_guessed_exposure_failure(self):
        self.write([{"kind": "trade_feed", "at_ms": 10_000, "leg": 1, "online": False}, self.trade()])
        report = audit(self.tape, self.config)
        self.assertTrue(report["trade_timing_admitted"])
        self.assertEqual(report["by_leg"][1]["feed_offline_events"], 1)
        self.assertFalse(report["stream_quality_verified"])

    def test_terminal_marker_order_and_types_are_required(self):
        cases = ([self.trade()], [self.trade(at=12_000), self.trade()],
                 [self.trade(leg=True)], [self.trade(source=True)],
                 [{"kind": "end", "at_ms": 19_999}])
        for events in cases:
            with self.subTest(events=events):
                self.write(events, end=False)
                with self.assertRaises(ValueError):
                    audit(self.tape, self.config)
        self.write([], end=True)
        with self.tape.open("a") as stream:
            stream.write(json.dumps(self.trade(at=20_000)) + "\n")
        with self.assertRaisesRegex(ValueError, "order"):
            audit(self.tape, self.config)

    def test_changed_input_cannot_publish_admission(self):
        self.write([self.trade()])
        from replay_trade_preflight import rows
        def changing_rows(stream):
            yield from rows(stream)
            self.tape.touch()
        with patch("replay_trade_preflight.rows", side_effect=changing_rows):
            with self.assertRaisesRegex(ValueError, "changed"):
                audit(self.tape, self.config)

    def test_rejected_grid_keeps_every_config_and_never_starts_engine(self):
        self.write([self.trade(source=10_000)])
        (self.root / "configs").mkdir()
        policies = [(name, self.config | {"posture": name}) for name in ("neutral", "long_first", "short_first")]
        with patch("replay_ten_hour_scenarios.turnover_policies", return_value=iter(policies)), \
                patch("replay_ten_hour_scenarios.execute") as execute:
            results, evidence = execute_turnover_grid(Path("unused"), self.root, "BTC", {}, {}, (),
                                                      self.tape, digest(self.tape))
        execute.assert_not_called()
        self.assertFalse(evidence["trade_timing_admitted"])
        self.assertEqual(len(list((self.root / "configs").glob("*.json"))), 3)
        for row in results.values():
            self.assertIsNone(row["net_pnl"])
            self.assertFalse(row["execution_started"])
            self.assertEqual(row["incomplete"], "input_trade_timing_rejected")
            self.assertEqual(row["trade_preflight_sha256"], evidence["sha256"])

    def test_two_second_sensitivity_keeps_original_tape_and_is_separate(self):
        self.write([self.trade(source=10_000)])
        original = digest(self.tape)
        self.assertFalse(audit(self.tape, self.config)["trade_timing_admitted"])
        report = audit(self.tape, self.config | {"trade_delivery_grace_ms": 2000})
        self.assertTrue(report["trade_timing_admitted"])
        self.assertEqual(report["input_sha256"], original)
        self.assertEqual(report["by_leg"][1]["max_delivery_ms"], 1000)
        pair = {"legs": [{"venue": "lighter"}, {"venue": "extended"}]}
        window = {"start_ms": 10_000, "end_ms": 20_000, "drain_until_ms": 35_000}
        for _, policy in turnover_policies(pair, window, ("0", "2.5"), 2000):
            self.assertEqual(policy["trade_delivery_grace_ms"], 2000)
            self.assertEqual(policy["drain_ms"], 15_000)
        with self.assertRaises(ValueError):
            list(turnover_policies(pair, window, ("0", "2.5"), 5001))

    def test_turnover_only_cli_needs_no_quant_engine_or_config_and_reports_rejection(self):
        engine = self.root / "unused-pair-engine"
        engine.write_bytes(b"synthetic hash identity; must never execute")
        (self.root / "campaign.json").write_text(json.dumps({"selected": {"lighter": {"ETH": {"id": 0}}}}))
        terms = self.root / "terms.json"
        terms.write_text("{}")
        window = {"start_ms": 10_000, "end_ms": 19_000, "drain_until_ms": 20_000}
        def prepare(campaign, symbol, window, output, **kwargs):
            self.assertEqual(symbol, "ETH")
            self.tape = output
            self.write([self.trade(source=10_000)])
            return {"legs": [{"venue": "lighter"}, {"venue": "extended"}]}, {
                "explicit_feed_states_by_leg": [1, 1], "input_sha256": digest(output)}
        args = ["runner", str(self.root), str(self.root/'missing-quant-engine'), str(engine),
                str(self.root/'missing-quant-config'), str(terms), str(self.root/'out'),
                "--program", "turnover", "--symbols", "ETH", "--turnover-venue", "extended",
                "--turnover-fees-bps", "0", "2.5"]
        with patch.object(sys, "argv", args), patch("replay_ten_hour_scenarios.time_window", return_value=window), \
                patch("replay_ten_hour_scenarios.turnover_input", side_effect=prepare), \
                patch("replay_ten_hour_scenarios.quant_input") as quant, \
                patch("replay_ten_hour_scenarios.execute") as execute, redirect_stdout(io.StringIO()):
            self.assertEqual(main(), 1)
        quant.assert_not_called()
        execute.assert_not_called()
        report = json.loads((self.root/'out/report.json').read_text())
        self.assertEqual(report["quant"], {})
        self.assertEqual(report["symbols"], ["ETH"])
        self.assertEqual(set(report["engine_sha256"]), {"turnover"})
        self.assertEqual(len(report["turnover"]["ETH"]), 12)


if __name__ == "__main__":
    unittest.main()
