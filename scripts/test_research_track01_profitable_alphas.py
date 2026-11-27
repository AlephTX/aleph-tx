#!/usr/bin/env python3
"""Unit tests for `research_track01_profitable_alphas.py` (pure-stdlib, `python3 -S`)."""

from __future__ import annotations

import json
import math
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

_SCRIPT_DIR = Path(__file__).resolve().parent
if str(_SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(_SCRIPT_DIR))

import research_track01_profitable_alphas as mod  # noqa: E402


class TestResearchTrack01ProfitableAlphas(unittest.TestCase):
    def test_self_test_passes(self) -> None:
        res = mod.run_self_test()
        self.assertEqual(res["self_test"], "ok")
        self.assertGreaterEqual(res["mom_trades"], 1)
        self.assertGreater(res["mom_realized_gross_usd"], 0.0)
        self.assertGreaterEqual(res["rev_trades"], 1)
        self.assertGreater(res["rev_realized_gross_usd"], 0.0)

    def test_pure_stdlib_top_level_under_python_s_isolation(self) -> None:
        probe = "\n".join(
            [
                f"import json, sys; sys.path.insert(0, {str(_SCRIPT_DIR)!r})",
                "for pkg in ('duckdb', 'numpy', 'pandas', 'pyarrow', 'scipy'):",
                "    sys.modules[pkg] = None",
                "import research_track01_profitable_alphas as iso",
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

    def test_market_id_maps_and_w28_w08_exclusion(self) -> None:
        self.assertEqual(len(mod.LIGHTER_ROBINHOOD_MARKETS), 18)
        self.assertEqual(len(mod.LIGHTER_MARKETS), 19)
        self.assertEqual(mod.LIGHTER_ROBINHOOD_MARKETS["NVDA"], "15")
        self.assertEqual(mod.LIGHTER_MARKETS["NVDA"], "110")

        with tempfile.TemporaryDirectory() as td:
            root = Path(td)
            base = (
                root
                / "parquet"
                / "bbo_quotes"
                / "collector_region=tokyo"
                / "venue=lighter"
                / "utc_date=2026-11-26"
            )
            for cid in (
                "tokyo-rwa-20261115T000000Z-w01",
                "tokyo-rwa-20261118T000000Z-w08",
                "tokyo-rwa-20261124T000000Z-w21",
                "tokyo-rwa-20261126T000000Z-w26",
                "tokyo-rwa-20261126T160000Z-w27",
                "tokyo-rwa-20261127T020000Z-w28",
            ):
                (base / f"campaign_id={cid}").mkdir(parents=True, exist_ok=True)
            wins = mod.discover_windows_w01_w27(root)
            self.assertEqual(sorted(wins.keys()), ["w01", "w21", "w26", "w27"])
            self.assertEqual(wins["w01"]["split"], "selection_dev")
            self.assertEqual(wins["w21"]["split"], "holdout")
            self.assertEqual(wins["w26"]["split"], "w26_w27_oos")
            self.assertEqual(wins["w27"]["split"], "w26_w27_oos")

    def test_momentum_filter_clock_regression_and_stale_fail_closed(self) -> None:
        flt = mod.OracleMomStatePy(warmup_ms=2_000, min_samples=2)
        q = (10_000, 99.9, 10.0, 100.1, 10.0)
        c = (10_000, 100.2, 100.0, 0.0)
        st0, _, _, _ = flt.step(10_000, q, c)
        self.assertEqual(st0, 0)
        # Backward clock fails closed
        st_b, _, _, _ = flt.step(9_000, q, c)
        self.assertEqual(st_b, 0)
        # Stale context fails closed
        st_s, _, _, _ = flt.step(100_000, (100_000, 99.9, 10.0, 100.1, 10.0), c)
        self.assertEqual(st_s, 0)

    def test_print_verification_and_residual_mtm_vs_realized(self) -> None:
        ok0, n0, _ = mod._check_print_fill(None, 1_000, 5_000, 1, 100.0, 500.0, 2_000.0)
        self.assertFalse(ok0)
        self.assertEqual(n0, 0.0)
        tr_ok = ([2_000], [-1], [100.0], [4.0])
        ok1, n1, _ = mod._check_print_fill(tr_ok, 1_000, 5_000, 1, 100.0, 500.0, 2_000.0)
        self.assertTrue(ok1)
        self.assertTrue(math.isclose(n1, 200.0, abs_tol=1e-9))

        # Residual MTM separation when exit L1 depth collapses
        t0 = 1_000_000
        bbo = []
        ctx = []
        for i in range(400):
            t = t0 + i * 1_000
            px = 100.20 if i >= 100 else 100.0
            bsz = 0.05 if i >= 350 else 20.0
            bbo.append((t, px - 0.01, bsz, px + 0.01, 20.0))
            ctx.append((t, px, 100.0, 0.0))
        sig = mod.precompute_directional_series(bbo, [b[0] for b in bbo], ctx, [c[0] for c in ctx])
        assert sig is not None
        res = mod.simulate_oracle_momentum(
            sig, min_dislocation_bps=15.0, max_hold_ms=260_000, exec_mode="taker", clip_usd=500.0
        )
        self.assertGreaterEqual(res["trades"], 1)
        self.assertFalse(res["terminal_flat_all"])
        self.assertGreater(res["terminal_residual_notional_usd"], 400.0)
        self.assertNotEqual(res["realized_only_gross_usd"], res["mtm_incl_residual_gross_usd"])

        agg = mod.aggregate_rows([{**res, "win": "w01", "symbol": "NVDA"}], venue="lighter")
        self.assertLess(agg["net_0_0_plus_1bp_usd"], agg["mtm_incl_residual_net_0_0_usd"])
        self.assertLess(agg["net_0_0_plus_2bp_usd"], agg["net_0_0_plus_1bp_usd"])


if __name__ == "__main__":
    unittest.main()
