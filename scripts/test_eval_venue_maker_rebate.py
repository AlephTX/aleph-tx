#!/usr/bin/env python3
"""Unit tests for venue-agnostic `eval_venue_maker_rebate.py`."""

from __future__ import annotations

import json
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))

import eval_venue_maker_rebate as evm  # noqa: E402


class EvalVenueMakerRebateTests(unittest.TestCase):
    def test_supports_custom_venue_schedule_json_and_uncapped_mode(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            p = Path(tmpdir) / "schedule.json"
            p.write_text(
                json.dumps(
                    {
                        "venue_id": "custom_venue_v1",
                        "cap_by_counterparty_taker_fee": False,
                        "tiers": [{"name": "MAKER_VIP", "min_share": 0.02, "nominal_rebate_bps": 0.25}],
                    }
                ),
                encoding="utf-8",
            )
            sched = evm.load_schedule_from_json(p)
            report = evm.evaluate_venue_rebate_matrix(
                schedule=sched,
                allocated_capital_usd=20_000.0,
                max_gross_notional_usd=20_000.0,
                tier="MAKER_VIP",
            )
            self.assertEqual(report["venue_id"], "custom_venue_v1")
            self.assertEqual(report["realized_capped_rebate_bps"], 0.25)
            json.dumps(report, allow_nan=False)

    def test_rejects_bool_inputs_directory_paths_and_excess_leverage(self) -> None:
        with self.assertRaises(ValueError):
            evm.evaluate_venue_rebate_matrix(
                schedule=evm.BUILTIN_SCHEDULES["arcus_2026_11_18"],
                allocated_capital_usd=True,  # type: ignore[arg-type]
                max_gross_notional_usd=100.0,
                tier="T5",
            )
        with self.assertRaises(ValueError):
            evm.CounterpartyFillCohort("bad_bool", True, 0.5)  # type: ignore[arg-type]
        with self.assertRaises(ValueError):
            evm.evaluate_venue_rebate_matrix(
                schedule=evm.BUILTIN_SCHEDULES["arcus_2026_11_18"],
                allocated_capital_usd=10_000.0,
                max_gross_notional_usd=10_500.0,
                tier="T5",
            )
        with tempfile.TemporaryDirectory() as tmpdir:
            with self.assertRaises(ValueError):
                evm.load_schedule_from_json(Path(tmpdir))
            bad_str_bool = Path(tmpdir) / "bad_bool.json"
            bad_str_bool.write_text(
                json.dumps(
                    {
                        "venue_id": "v",
                        "cap_by_counterparty_taker_fee": "false",
                        "tiers": [{"name": "T1", "min_share": 0.01, "nominal_rebate_bps": 0.2}],
                    }
                ),
                encoding="utf-8",
            )
            with self.assertRaises(ValueError):
                evm.load_schedule_from_json(bad_str_bool)

            bad_venue_type = Path(tmpdir) / "bad_venue.json"
            bad_venue_type.write_text(
                json.dumps(
                    {
                        "venue_id": None,
                        "cap_by_counterparty_taker_fee": True,
                        "tiers": [{"name": "T1", "min_share": 0.01, "nominal_rebate_bps": 0.2}],
                    }
                ),
                encoding="utf-8",
            )
            with self.assertRaises(ValueError):
                evm.load_schedule_from_json(bad_venue_type)

            bad_cohort = Path(tmpdir) / "bad_cohort.json"
            bad_cohort.write_text(
                json.dumps([{"name": 123, "notional_share": 1.0, "taker_fee_bps": 1.0}]),
                encoding="utf-8",
            )
            with self.assertRaises(ValueError):
                evm.load_cohorts_from_json(bad_cohort)


if __name__ == "__main__":
    unittest.main()
