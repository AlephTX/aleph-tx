#!/usr/bin/env python3
"""Stdlib-only unit tests for `scripts/research_track03_microstructure_latency_mm.py`."""

from __future__ import annotations

import json
import subprocess
import sys
import unittest
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

import research_track03_microstructure_latency_mm as mm  # noqa: E402


class Track03MicrostructureLatencyTests(unittest.TestCase):
    def test_stdlib_only_top_level_and_provenance(self) -> None:
        probe = subprocess.run(
            [
                sys.executable,
                "-S",
                "-c",
                (
                    f"import sys; sys.path.insert(0, {str(SCRIPT_DIR)!r}); "
                    "import research_track03_microstructure_latency_mm; "
                    "mods = [m for m in ('duckdb', 'numpy', 'pandas', 'pyarrow', 'scipy') if m in sys.modules]; "
                    "assert not mods, f'unexpected imports: {mods}'"
                ),
            ],
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertEqual(probe.returncode, 0, probe.stderr or probe.stdout)

        targets, prov = mm.build_standard_targets()
        self.assertTrue(prov["lighter_standard_scenario"])
        by_v = {t.venue: t for t in targets}
        self.assertEqual(by_v["lighter"].settlement, "USDC")
        self.assertEqual(by_v["lighter"].account_fee_status, "lighter_standard_zero_scenario")
        self.assertEqual(by_v["lighter"].maker_fee_bps, 0.0)
        self.assertEqual(by_v["lighter"].taker_fee_bps, 0.0)
        self.assertEqual(by_v["lighter_robinhood"].settlement, "USDG")

    def test_cancel_in_flight_and_same_side_cluster_diagnostics(self) -> None:
        # Order placed at ts=0 (active at 200ms) at 100.00, activated on ts=220 book.
        # At ts=300: volatile jump triggers cancel (effective at 300+300=600ms).
        # At ts=400 (in-flight!): SELL trade fills BID -> counted in cancel_in_flight_fills!
        arm = mm.build_stage_a_arms(latency_pairs=[(200, 300)])[0]
        events = [
            {"ts": 0, "kind": "book", "bids": [(100.00, 2.0)], "asks": [(100.04, 2.0)]},
            {"ts": 220, "kind": "book", "bids": [(100.00, 2.0)], "asks": [(100.04, 2.0)]},
            {"ts": 300, "kind": "book", "bids": [(99.80, 2.0)], "asks": [(99.84, 2.0)]},
            {"ts": 400, "kind": "trade", "side": "SELL", "price": 99.79, "size": 5.0},
            {"ts": 1_500, "kind": "book", "bids": [(99.78, 2.0)], "asks": [(99.82, 2.0)]},
            {"ts": 17_000, "kind": "book", "bids": [(99.78, 2.0)], "asks": [(99.82, 2.0)]},
        ]
        res = mm.simulate_arm(events, 0.01, arm)
        self.assertTrue(res["terminal_flat"])
        self.assertGreaterEqual(res["cancel_in_flight_fills"], 1)
        self.assertGreater(res["cancel_in_flight_share"], 0.0)

        # Same-side cluster check: 2 BID fills within 500ms (< 2000ms) under G4_baseline.
        events_cluster = [
            {"ts": 0, "kind": "book", "bids": [(100.00, 2.0)], "asks": [(100.04, 2.0)]},
            {"ts": 250, "kind": "trade", "side": "SELL", "price": 99.99, "size": 5.0},
            {"ts": 350, "kind": "book", "bids": [(100.00, 2.0)], "asks": [(100.04, 2.0)]},
            {"ts": 700, "kind": "trade", "side": "SELL", "price": 99.99, "size": 5.0},
            {"ts": 2_000, "kind": "book", "bids": [(100.00, 2.0)], "asks": [(100.04, 2.0)]},
        ]
        res_cl = mm.simulate_arm(events_cluster, 0.01, arm)
        self.assertTrue(res_cl["terminal_flat"])
        self.assertGreaterEqual(res_cl["same_side_cluster_fills"], 1)

    def test_g5_tfi_veto_g6_self_fill_stepback_and_g7_patient_pennying(self) -> None:
        # G5 TFI veto pulls Ask during strong BUY flow without blocking reduce_only
        g5 = mm.build_stage_b_arms(latency_pairs=[(200, 300)], arm_names=["G5_microprice_tfi"])[0]
        events_tfi = [
            {"ts": 0, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.04, 10.0)]},
            {"ts": 250, "kind": "trade", "side": "BUY", "price": 100.04, "size": 2.0},
            {"ts": 260, "kind": "trade", "side": "BUY", "price": 100.04, "size": 2.0},
            {"ts": 650, "kind": "trade", "side": "BUY", "price": 100.05, "size": 5.0},
            {"ts": 1_500, "kind": "book", "bids": [(100.02, 10.0)], "asks": [(100.06, 10.0)]},
        ]
        res_g5 = mm.simulate_arm(events_tfi, 0.01, g5)
        self.assertTrue(res_g5["terminal_flat"])
        self.assertGreater(res_g5["tfi_veto_cancels"] + res_g5["tfi_veto_blocks"], 0)
        # Because Ask was pulled at ts=250 (effective 550ms < 650ms), the 650ms sweep does NOT fill Ask!
        self.assertEqual(res_g5["maker_fills"], 0)

        # G7 patient passive-pennying unwind steps inside wide spread and avoids IOC bleed
        g4 = mm.build_stage_b_arms(latency_pairs=[(200, 300)], arm_names=["G4_baseline"])[0]
        g7 = mm.build_stage_b_arms(latency_pairs=[(200, 300)], arm_names=["G7_patient_passive_unwind"])[0]
        events_wide = [
            {"ts": 0, "kind": "book", "bids": [(100.00, 2.0)], "asks": [(100.08, 2.0)]},
            {"ts": 250, "kind": "trade", "side": "SELL", "price": 99.99, "size": 5.0},
            {"ts": 300, "kind": "book", "bids": [(100.00, 2.0)], "asks": [(100.08, 2.0)]},
            # After cancel effective (600ms), ts=620 book places G7's 1-tick pennied Ask at 100.07 (active at 820ms),
            # whereas G4 keeps its resting Ask at 100.08!
            {"ts": 620, "kind": "book", "bids": [(100.00, 2.0)], "asks": [(100.08, 2.0)]},
            # Trade at 100.07 (ts=900) fills G7's pennied Ask (100.07) while missing G4's resting Ask (100.08)!
            {"ts": 900, "kind": "trade", "side": "BUY", "price": 100.07, "size": 5.0},
            {"ts": 20_000, "kind": "book", "bids": [(100.00, 2.0)], "asks": [(100.08, 2.0)]},
        ]
        r_g4 = mm.simulate_arm(events_wide, 0.01, g4)
        r_g7 = mm.simulate_arm(events_wide, 0.01, g7)
        self.assertTrue(r_g4["terminal_flat"] and r_g7["terminal_flat"])
        self.assertEqual(r_g4["taker_exits"], 1)
        self.assertEqual(r_g7["taker_exits"], 0)
        self.assertGreaterEqual(r_g7["reduce_penny_placements"], 1)

    def test_cli_self_test(self) -> None:
        proc = subprocess.run(
            [sys.executable, "-S", str(SCRIPT_DIR / "research_track03_microstructure_latency_mm.py"), "--self-test"],
            check=True,
            capture_output=True,
            text=True,
        )
        payload = json.loads(proc.stdout)
        self.assertEqual(payload["self_test"], "ok")
        self.assertEqual(len(payload["arms_verified"]), 5)


if __name__ == "__main__":
    unittest.main()
