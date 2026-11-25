#!/usr/bin/env python3
"""Unit tests for `eval_arcus_maker_rebate.py` preset wrapper."""

from __future__ import annotations

import json
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))

import eval_arcus_maker_rebate as ear  # noqa: E402


class EvalArcusMakerRebateTests(unittest.TestCase):
    def test_enforces_strict_1x_leverage_cap_and_rejects_bool(self) -> None:
        with self.assertRaises(ValueError):
            ear.evaluate_rebate_matrix(
                allocated_capital_usd=25_000.0,
                max_gross_notional_usd=26_000.0,
                tier="T5",
            )
        with self.assertRaises(ValueError):
            ear.evaluate_rebate_matrix(
                allocated_capital_usd=True,  # type: ignore[arg-type]
                max_gross_notional_usd=10.0,
                tier="T5",
            )

    def test_counterparty_taker_fee_caps_nominal_rebate_per_fill(self) -> None:
        cohorts = (
            ear.CounterpartyFillCohort("zero_fee_mm", 0.5, 0.0),
            ear.CounterpartyFillCohort("retail", 0.5, 2.0),
        )
        realized, ratio = ear.compute_capped_rebate_bps(0.20, cohorts)
        self.assertAlmostEqual(realized, 0.10, places=9)
        self.assertAlmostEqual(ratio, 0.50, places=9)

    def test_sensitivity_matrix_covers_volumes_and_wear_costs(self) -> None:
        report = ear.evaluate_rebate_matrix(
            allocated_capital_usd=25_000.0,
            max_gross_notional_usd=25_000.0,
            tier="T6",
        )
        self.assertEqual(report["venue_id"], "arcus_2026_11_18")
        self.assertEqual(len(report["volumes"]), 3)
        for vol_row in report["volumes"]:
            wears = vol_row["wear_scenarios"]
            self.assertEqual([w["wear_usd_per_m"] for w in wears], [0.0, 50.0, 100.0, 200.0])
            self.assertTrue(wears[0]["profitable"])
            self.assertFalse(wears[-1]["profitable"])
        json.dumps(report, allow_nan=False)


if __name__ == "__main__":
    unittest.main()
