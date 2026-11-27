#!/usr/bin/env python3
"""Unit tests for `research_stat_arb_and_oracle_reversion.py` (pure-stdlib, runnable under `python3 -S`)."""

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

import research_stat_arb_and_oracle_reversion as mod  # noqa: E402


class TestResearchStatArbAndOracleReversion(unittest.TestCase):
    def test_self_test_passes(self) -> None:
        res = mod.run_self_test()
        self.assertEqual(res["self_test"], "ok")
        self.assertGreaterEqual(res["pairs_taker_trades"], 1)
        self.assertGreater(res["pairs_taker_gross_usd"], 0.0)
        self.assertGreaterEqual(res["oracle_trades"], 1)
        self.assertGreater(res["oracle_funding_usd"], 0.0)

    def test_pure_stdlib_top_level_and_duckdb_absent_safe(self) -> None:
        # Probe in a clean isolated `python3 -S` subprocess so shared unittest-discover
        # runs (where sibling tests import numpy) do not pollute the top-level import check.
        probe = "\n".join(
            [
                f"import json, sys; sys.path.insert(0, {str(_SCRIPT_DIR)!r})",
                "for pkg in ('duckdb', 'numpy', 'pandas', 'pyarrow', 'scipy'):",
                "    sys.modules[pkg] = None",
                "import research_stat_arb_and_oracle_reversion as iso",
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

    def test_market_id_maps_and_window_exclusion(self) -> None:
        self.assertEqual(len(mod.LIGHTER_ROBINHOOD_MARKETS), 18)
        self.assertEqual(len(mod.LIGHTER_MARKETS), 19)
        self.assertEqual(mod.LIGHTER_ROBINHOOD_MARKETS["NVDA"], "15")
        self.assertEqual(mod.LIGHTER_MARKETS["NVDA"], "110")
        self.assertEqual(mod.LIGHTER_MARKETS["MSTR"], "122")

        with tempfile.TemporaryDirectory() as td:
            root = Path(td)
            base = root / "parquet" / "bbo_quotes" / "collector_region=tokyo" / "venue=lighter" / "utc_date=2026-11-26"
            for cid in (
                "tokyo-rwa-20261115T000000Z-w01",
                "tokyo-rwa-20261118T000000Z-w08",
                "tokyo-rwa-20261124T000000Z-w21",
                "tokyo-rwa-20261126T000000Z-w26",
                "tokyo-rwa-20261127T000000Z-w27",
                "tokyo-rwa-20261128T000000Z-w28",
            ):
                (base / f"campaign_id={cid}").mkdir(parents=True, exist_ok=True)
            wins = mod.discover_windows_w01_w26(root)
            self.assertEqual(sorted(wins.keys()), ["w01", "w21", "w26"])
            self.assertEqual(wins["w01"]["split"], "selection_dev")
            self.assertEqual(wins["w21"]["split"], "holdout")
            self.assertEqual(wins["w26"]["split"], "w26_oos")

    def test_pairs_filter_clock_and_stale_fail_closed(self) -> None:
        flt = mod.PairsSignalFilterPy(mod.PairsStatArbCfg(warmup_ms=2_000, min_samples=4, ar_sample_ms=1_000))
        q = (10_000, 99.9, 10.0, 100.1, 10.0)
        ok, st, _, _, _, _ = flt.step(10_000, q, q)
        self.assertTrue(ok)
        self.assertEqual(st, "warming_up")
        # Clock regression fails closed
        ok_b, st_b, _, _, _, _ = flt.step(9_000, q, q)
        self.assertFalse(ok_b)
        self.assertEqual(st_b, "clock_regression")
        # Stale quote fails closed
        ok_s, st_s, _, _, _, _ = flt.step(100_000, (10_000, 99.9, 10.0, 100.1, 10.0), (100_000, 99.9, 10.0, 100.1, 10.0))
        self.assertFalse(ok_s)
        self.assertEqual(st_s, "stale_quote")

    def test_print_verification_zero_fabricated_fills_and_depth_cap(self) -> None:
        # Empty prints -> False
        ok0, n0, _ = mod._check_print_fill(None, 1_000, 5_000, 1, 100.0, 500.0, 2_000.0)
        self.assertFalse(ok0)
        self.assertEqual(n0, 0.0)
        # Wrong aggressor side above bid -> False
        tr_wrong = ([2_000], [1], [100.05], [10.0])
        ok1, n1, _ = mod._check_print_fill(tr_wrong, 1_000, 5_000, 1, 100.0, 500.0, 2_000.0)
        self.assertFalse(ok1)
        self.assertEqual(n1, 0.0)
        # Matching sell print at bid capped by 0.5 * print_notional (0.5 * 400 = 200)
        tr_ok = ([2_000], [-1], [100.0], [4.0])
        ok2, n2, ts2 = mod._check_print_fill(tr_ok, 1_000, 5_000, 1, 100.0, 500.0, 2_000.0)
        self.assertTrue(ok2)
        self.assertTrue(math.isclose(n2, 200.0, abs_tol=1e-9))
        self.assertEqual(ts2, 2_000)

    def test_terminal_residual_and_fee_stress_accounting(self) -> None:
        # Tiny exit depth -> depth_clipped_exits > 0 and terminal_flat_all == False
        bbo = [(1_000_000 + i * 1_000, 99.99, 20.0, 100.01, 20.0) for i in range(1_200)]
        # Shock down then shrink exit L1 depth near window end
        for i in range(1_000, 1_195):
            px = 99.80
            bbo[i] = (1_000_000 + i * 1_000, px - 0.01, 20.0, px + 0.01, 20.0)
        for i in range(1_180, 1_200):
            # Tiny bid depth (0.5 * 100 * 0.1 = $5) when closing a $500 long!
            px = 99.80
            bbo[i] = (1_000_000 + i * 1_000, px - 0.01, 0.1, px + 0.01, 20.0)
        ora = [(1_000_000 + i * 1_000, 100.0, 0.0) for i in range(1_200)]
        sig = mod.precompute_oracle_signal(bbo, [b[0] for b in bbo], ora, [o[0] for o in ora])
        assert sig is not None
        res = mod.simulate_oracle_signal(sig, min_dislocation_bps=5.0, spread_mult=1.0, clip_usd=500.0)
        self.assertGreaterEqual(res["trades"], 1)
        self.assertFalse(res["terminal_flat_all"])
        self.assertGreater(res["terminal_residual_notional_usd"], 400.0)
        self.assertGreaterEqual(res["depth_clipped_exits"], 1)

        # Fee stress check
        agg = mod.aggregate_rows([{**res, "win": "w01", "symbol": "NVDA"}], venue="lighter")
        self.assertLess(agg["net_0_0_plus_1bp_usd"], agg["net_0_0_usd"])
        self.assertLess(agg["net_0_0_plus_2bp_usd"], agg["net_0_0_plus_1bp_usd"])


if __name__ == "__main__":
    unittest.main()
