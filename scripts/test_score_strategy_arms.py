#!/usr/bin/env python3
"""Unit tests for scripts/score_strategy_arms.py."""

from __future__ import annotations

import json
import math
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))
import score_strategy_arms as scorer  # noqa: E402


class ScoreStrategyArmsTest(unittest.TestCase):
    def test_pipeline_regression_and_notional_comparability_gate(self) -> None:
        rows = [
            {
                "win": "w21",
                "sym": "BTC_core_hl",
                "arm": "A0_base",
                "cont": True,
                "net": -100.0,
                "ntl": 500_000.0,
                "cyc": 1000,
                "can": 10000,
            },
            {
                "win": "w21",
                "sym": "BTC_core_hl",
                "arm": "C5_low_part",
                "cont": True,
                "net": -15.0,
                "ntl": 100_000.0,
                "cyc": 200,
                "can": 2000,
            },
            {
                "win": "w21",
                "sym": "BTC_core_hl",
                "arm": "S1_comparable",
                "cont": True,
                "net": -60.0,
                "ntl": 400_000.0,
                "cyc": 800,
                "can": 4000,
                "sweep_cleared_fills": 250,
                "sweep_touch_fills": 150,
                "markout_1s_bps": 0.25,
            },
        ]
        res = scorer.score_pipeline_rows(rows, baseline_arm="A0_base")
        reg = res["regression_net_vs_notional"]
        self.assertEqual(reg["n"], 3)
        self.assertAlmostEqual(reg["slope_bps"], -1.9807692307692308, places=6)
        self.assertAlmostEqual(reg["r_squared"], 0.9401807869549805, places=6)

        comps = {c["arm"]: c for c in res["comparisons"] if c["scope"] == "ALL"}
        self.assertFalse(comps["C5_low_part"]["comparable"])
        self.assertEqual(
            comps["C5_low_part"]["verdict"], "not_comparable_notional_divergence"
        )
        self.assertAlmostEqual(comps["C5_low_part"]["notional_diff_frac"], 0.80, places=6)

        self.assertTrue(comps["S1_comparable"]["comparable"])
        self.assertEqual(comps["S1_comparable"]["verdict"], "comparable")
        self.assertAlmostEqual(comps["S1_comparable"]["unit_delta_bps"], 0.50, places=6)
        self.assertTrue(res["all_symbols"]["A0_base"]["price_through_only_warning"])
        self.assertFalse(res["all_symbols"]["S1_comparable"]["price_through_only_warning"])
        self.assertAlmostEqual(
            res["all_symbols"]["S1_comparable"]["markout_1s_bps"], 0.25, places=6
        )

    def test_basis_splits_cont_windows_and_checks_ofat_monotonicity(self) -> None:
        rows = [
            {
                "win": "w13",
                "sym": "BTC_core_hl",
                "arm": "OFAT300_sc000",
                "cont": True,
                "ent": 20,
                "ex": 20,
                "pnl": 6.0,
                "fee": 4.0,
                "net": 2.0,
                "ntl": 24_000.0,
            },
            {
                "win": "w14",
                "sym": "BTC_core_hl",
                "arm": "OFAT300_sc000",
                "cont": False,
                "ent": 12,
                "ex": 12,
                "pnl": 3.0,
                "fee": 2.4,
                "net": 0.6,
                "ntl": 14_400.0,
            },
            {
                "win": "w13",
                "sym": "BTC_core_hl",
                "arm": "OFAT300_sc025",
                "cont": True,
                "ent": 10,
                "ex": 10,
                "pnl": 4.0,
                "fee": 2.0,
                "net": 2.0,
                "ntl": 12_000.0,
            },
        ]
        res = scorer.score_basis_rows(rows, min_basis_roundtrips=15)
        all_sc0 = res["all_windows"]["arms"]["OFAT300_sc000"]
        cont_sc0 = res["strict_cont_only"]["arms"]["OFAT300_sc000"]
        self.assertEqual(all_sc0["roundtrips"], 32)
        self.assertFalse(all_sc0["thin_sample_warning"])
        self.assertEqual(cont_sc0["roundtrips"], 20)
        self.assertAlmostEqual(cont_sc0["net_per_roundtrip_usd"], 0.10, places=6)
        self.assertAlmostEqual(cont_sc0["net_to_fee_ratio"], 0.50, places=6)
        self.assertAlmostEqual(
            cont_sc0["net_per_notional_bps"], (2.0 / 24_000.0) * 10_000.0, places=6
        )

        mono = res["strict_cont_only"]["ofat_monotonicity"][0]
        self.assertTrue(mono["roundtrips_monotone_non_increasing"])
        self.assertTrue(mono["net_per_roundtrip_monotone_non_decreasing"])
        self.assertTrue(mono["net_to_fee_monotone_non_decreasing"])

    def test_rejects_bool_nan_missing_file_and_self_test_conflict(self) -> None:
        with self.assertRaises(ValueError):
            scorer.require_finite_float(True, "net")
        with self.assertRaises(ValueError):
            scorer.require_finite_float(math.nan, "net")
        with self.assertRaises(ValueError):
            scorer.require_int(False, "cyc")
        with self.assertRaises(ValueError):
            scorer.main(["--self-test", "examples/crypto_funding_paper.json"])
        with self.assertRaises(FileNotFoundError):
            scorer.main(["/nonexistent/path/that/does/not/exist.json"])

        with tempfile.TemporaryDirectory() as tmp:
            p = Path(tmp) / "sample.json"
            p.write_text(
                json.dumps(
                    {
                        "pipeline": [
                            {
                                "win": "w1",
                                "sym": "BTC",
                                "arm": "A0",
                                "cont": True,
                                "net": -1.0,
                                "ntl": 1000.0,
                            }
                        ],
                        "basis": [],
                    }
                ),
                encoding="utf-8",
            )
            out_p = Path(tmp) / "out.json"
            rc = scorer.main([str(p), "--output", str(out_p)])
            self.assertEqual(rc, 0)
            parsed = json.loads(out_p.read_text(encoding="utf-8"))
            self.assertTrue(parsed["reports"]["sample.json"]["pipeline"]["present"])


if __name__ == "__main__":
    unittest.main()
