#!/usr/bin/env python3
"""Unit tests for `research_track01_controlled_gate_eval.py` (pure-stdlib, `python3 -S`)."""

from __future__ import annotations

import json
import math
import subprocess
import sys
import unittest
from pathlib import Path

_SCRIPT_DIR = Path(__file__).resolve().parent
if str(_SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(_SCRIPT_DIR))

import research_track01_controlled_gate_eval as mod  # noqa: E402
import research_track01_profitable_alphas as r1  # noqa: E402


class TestTrack01ControlledGateEval(unittest.TestCase):
    def test_self_test_passes(self) -> None:
        res = mod.run_self_test()
        self.assertEqual(res["self_test"], "ok")
        self.assertEqual(res["part_a_before_res_usd"], 0.0)
        self.assertGreater(res["part_a_after_res_usd"], 350.0)
        self.assertGreater(res["part_b_gate_rejected"], 0)

    def test_pure_stdlib_top_level_under_python_s_isolation(self) -> None:
        probe = "\n".join(
            [
                f"import json, sys; sys.path.insert(0, {str(_SCRIPT_DIR)!r})",
                "for pkg in ('duckdb', 'numpy', 'pandas', 'pyarrow', 'scipy'):",
                "    sys.modules[pkg] = None",
                "import research_track01_controlled_gate_eval as iso",
                "res = iso.run_self_test()",
                "print(json.dumps(res))",
            ]
        )
        proc = subprocess.run(
            [sys.executable, "-S", "-c", probe],
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(proc.returncode, 0, msg=f"stderr={proc.stderr}\nstdout={proc.stdout}")
        payload = json.loads(proc.stdout.strip().splitlines()[-1])
        self.assertEqual(payload["self_test"], "ok")

    def test_controlled_arms_identical_candidates_independent_state_and_flat(self) -> None:
        t0 = 1_000_000
        bbo = []
        ctx = []
        for i in range(3_000):
            t = t0 + i * 1_000
            m = i // 60
            px = 100.0 if m < 20 else (99.0 if m < 25 else 99.60)
            half = 0.06 if m == 20 else 0.01  # m=20 has 12 bps spread; m=21 has 2 bps spread
            bbo.append((t, px - half, 20.0, px + half, 20.0))
            ctx.append((t, px, px, 0.0))
        sig = r1.precompute_directional_series(bbo, [b[0] for b in bbo], ctx, [c[0] for c in ctx])
        assert sig is not None

        base = mod.simulate_controlled_overreaction_arm(sig, r1, gate_fn=None)
        gate = mod.simulate_controlled_overreaction_arm(
            sig, r1, gate_fn=lambda t_dec, side, spr, l1: spr <= 5.0
        )
        flat = mod._flat_summary(["w01"], venue="lighter_robinhood", r1=r1)

        # Identical candidate denominator
        self.assertEqual(
            base["funnel"]["candidate_events_total"],
            gate["funnel"]["candidate_events_total"],
        )
        # Gate rejects m=20 wide-spread triggers, then enters independently at m=21 tight spread
        self.assertGreater(gate["funnel"]["gate_rejected"], 0)
        self.assertEqual(gate["trades"], 1)
        self.assertEqual(flat["trades"], 0)
        self.assertEqual(flat["mtm_incl_residual_net_0_0_usd"], 0.0)
        self.assertEqual(flat["terminal_residual_notional_usd"], 0.0)

    def test_stationary_block_bootstrap_determinism_and_regression_verdict(self) -> None:
        b1 = mod.stationary_block_bootstrap_ci([1.0, -2.0, 3.0, 0.5, -1.5, 4.0], n_boot=1000, seed=42)
        b2 = mod.stationary_block_bootstrap_ci([1.0, -2.0, 3.0, 0.5, -1.5, 4.0], n_boot=1000, seed=42)
        self.assertEqual(b1, b2)
        self.assertTrue(math.isclose(b1["observed_total_usd"], 5.0, abs_tol=1e-9))

        # Regression verdict must enforce NO-GO / HOLD if w26_w27_oos is negative or CI straddles 0
        fake_splits = {
            "holdout": {
                "baseline": {"mtm_incl_residual_net_0_0_usd": 100.0, "net_0_0_plus_1bp_usd": 50.0, "terminal_residual_notional_usd": 0.0},
                "single_gate": {"mtm_incl_residual_net_0_0_usd": 120.0, "net_0_0_plus_1bp_usd": 70.0, "terminal_residual_notional_usd": 0.0},
            },
            "w26_w27_oos": {
                "baseline": {"mtm_incl_residual_net_0_0_usd": -30.0, "net_0_0_plus_1bp_usd": -35.0, "terminal_residual_notional_usd": 0.0},
                "single_gate": {"mtm_incl_residual_net_0_0_usd": -20.0, "net_0_0_plus_1bp_usd": -24.0, "terminal_residual_notional_usd": 0.0},
            },
            "oos_w21_w27": {
                "baseline": {"mtm_incl_residual_net_0_0_usd": 70.0, "net_0_0_plus_1bp_usd": 15.0, "terminal_residual_notional_usd": 0.0},
                "single_gate": {"mtm_incl_residual_net_0_0_usd": 100.0, "net_0_0_plus_1bp_usd": 46.0, "terminal_residual_notional_usd": 0.0},
            },
        }
        v_hold = mod._compute_regression_verdict(fake_splits, {"total_ci95_usd": [-5.0, 40.0]})
        self.assertEqual(v_hold["verdict"], "NO-GO / HOLD")


if __name__ == "__main__":
    unittest.main()
