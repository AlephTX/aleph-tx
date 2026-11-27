#!/usr/bin/env python3
"""Stdlib-only unit tests for `scripts/p0_same_event_parity_harness.py`."""

from __future__ import annotations

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

import p0_same_event_parity_harness as harness  # noqa: E402


class P0SameEventParityHarnessTests(unittest.TestCase):
    def test_stdlib_only_top_level_under_python_s(self) -> None:
        probe = subprocess.run(
            [
                sys.executable,
                "-S",
                "-c",
                (
                    f"import sys; sys.path.insert(0, {str(SCRIPT_DIR)!r}); "
                    "import p0_same_event_parity_harness; "
                    "mods = [m for m in ('duckdb', 'numpy', 'pandas', 'pyarrow', 'scipy') if m in sys.modules]; "
                    "assert not mods, f'unexpected third-party imports: {mods}'"
                ),
            ],
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertEqual(probe.returncode, 0, probe.stderr or probe.stdout)

    def test_three_way_parity_all_synthetic_p0_counterexamples(self) -> None:
        bundle = harness.generate_review_bundle(w27_slice_path=None, output_path=None)
        self.assertEqual(bundle["schema_version"], "alephtx_p0_parity_review_bundle_v1")
        self.assertEqual(
            bundle["frozen_plan"]["parity_authority_policy"],
            "post_9c9eaf2_fresh_rebuild_required",
        )
        self.assertEqual(
            bundle["frozen_plan"]["python_arm_role"],
            "diagnostic_reference_omits_synthetic_stale_arrival_ticks",
        )
        self.assertEqual(len(bundle["frozen_plan"]["rust_binary_sha256"]), 64)
        self.assertTrue(bundle["deterministic_result"]["all_synthetic_p0_passed"], bundle)
        self.assertEqual(bundle["independent_review_record"]["evidence_signoff"], "PASS")
        self.assertEqual(bundle["independent_review_record"]["economic_hypothesis_status"], "PENDING_P1_UNSEEN_WINDOW")

        by_id = {c["scenario_id"]: c for c in bundle["deterministic_result"]["synthetic_p0_counterexamples"]}

        c1 = by_id["p0_c1_stale_gap_330ms_requeue"]
        self.assertEqual(c1["parity_status"], "PASS")
        self.assertEqual(c1["rust"]["stale_exit_blocks"], 1)
        self.assertEqual(c1["python"]["stale_exit_blocks"], 1)
        self.assertEqual(c1["reference"]["stale_exit_blocks"], 1)
        self.assertEqual(c1["rust"]["taker_exits"], 1)
        self.assertAlmostEqual(c1["rust"]["terminal_residual_notional_usd"], 0.0, places=6)

        c2 = by_id["p0_c2_two_iocs_same_touch_depth_conservation"]
        self.assertEqual(c2["parity_status"], "PASS")
        self.assertGreaterEqual(c2["rust"]["depth_clipped_exits"], 1)

        c3 = by_id["p0_c3_partial_exit_true_residual"]
        self.assertEqual(c3["parity_status"], "PASS")
        self.assertGreater(c3["rust"]["terminal_residual_notional_usd"], 100.0)

        c4 = by_id["p0_c4_declared_end_ms_late_ref_immunity"]
        self.assertEqual(c4["parity_status"], "PASS")

        c5 = by_id["p0_c5_terminal_drain_ms_flatten"]
        self.assertEqual(c5["parity_status"], "PASS")
        self.assertEqual(c5["rust"]["taker_exits"], 1)
        self.assertAlmostEqual(c5["rust"]["terminal_residual_notional_usd"], 0.0, places=6)

        bounds = bundle["deterministic_result"]["same_ms_dual_ordering_bounds"]
        self.assertGreater(bounds["book_first_maker_fills"], bounds["trade_first_maker_fills"])

        for c in by_id.values():
            sc = c["scorecard"]
            self.assertIn("stage_1_input_coverage", sc)
            self.assertIn("stage_2_on_time_availability", sc)
            self.assertIn("stage_3_executable_and_settleable", sc)
            self.assertIn("stage_4_complete_economic_result", sc)
            self.assertLessEqual(sc["stage_4_complete_economic_result"]["cash_conservation_err_usd"], 1e-9)
            self.assertLessEqual(sc["stage_4_complete_economic_result"]["decomposition_err_usd"], 1e-9)

    def test_first_divergence_locator_and_w28_exclusion(self) -> None:
        m_a = {
            "maker_fills": 2,
            "taker_exits": 1,
            "stale_exit_blocks": 0,
            "depth_clipped_exits": 0,
            "cycles": 1,
            "realized_pnl_usd": 0.12,
            "unrealized_pnl_usd": 0.0,
            "fees_usd": 0.0,
            "closed_notional_usd": 200.0,
            "terminal_residual_notional_usd": 0.0,
            "double_sided_usd_per_m": -3.0,
        }
        m_b = dict(m_a, taker_exits=0, realized_pnl_usd=0.0)
        div = harness.locate_first_divergence(m_a, m_b, m_a)
        self.assertTrue(div["has_divergence"])
        self.assertEqual(div["first_divergence"]["field"], "taker_exits")

        with tempfile.TemporaryDirectory() as tmpdir:
            bad_slice = Path(tmpdir) / "bad_w28.json"
            bad_slice.write_text(json.dumps({"campaign_id": "tokyo-rwa-20261127-w28", "bbo_rows": [], "trade_rows": []}))
            rust_bin = harness._resolve_rust_binary()
            with self.assertRaises(ValueError):
                harness.run_w27_public_slice_parity(bad_slice, rust_bin)

    def test_cli_self_test_isolated(self) -> None:
        proc = subprocess.run(
            [sys.executable, "-S", str(SCRIPT_DIR / "p0_same_event_parity_harness.py"), "--self-test"],
            check=True,
            capture_output=True,
            text=True,
        )
        payload = json.loads(proc.stdout)
        self.assertEqual(payload["self_test"], "ok")
        self.assertEqual(payload["synthetic_cases_passed"], 7)


if __name__ == "__main__":
    unittest.main()
