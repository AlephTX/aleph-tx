import unittest
from decimal import Decimal
from pathlib import Path
from tempfile import TemporaryDirectory
import json

from rescore_decomposed_overhead import (
    DEFAULT_FIXED_INFRA_USD_PER_DAY,
    collect_local_rescoring,
    rescore_windows,
    summarize_carry_cohort,
)


class RescoreDecomposedOverheadTest(unittest.TestCase):
    def test_strips_legacy_025_bp_gross_tax_and_separates_fixed_infra(self):
        rows = [
            {"maker": "20000", "taker": "10000", "net": "-0.60"},
        ]
        # 0.25 bps * $30,000 gross = $0.75 modeled tax -> variable net = +0.15 -> -$7.50/M maker
        out = rescore_windows(
            rows,
            fixed_overhead_usd_per_day=Decimal("2.4"),
            hours_per_window=Decimal("10"),
        )
        self.assertAlmostEqual(out["modeled_gross_tax_stripped_usd"], 0.75)
        self.assertAlmostEqual(out["variable_net_usd"], 0.15)
        self.assertAlmostEqual(out["variable_cost_per_million_maker"], -7.5)
        self.assertAlmostEqual(out["fixed_infra_total_usd"], 1.0)
        self.assertAlmostEqual(out["all_in_net_observed_pace_usd"], -0.85)
        self.assertAlmostEqual(out["all_in_cost_per_million_maker_observed_pace"], 42.5)
        self.assertAlmostEqual(out["all_in_cost_per_million_maker_50m_30d_pace"], -7.5 + 1.44)

    def test_carry_and_empty_dir_safe_handling(self):
        carry = [
            {"market": "SOL", "split": "temporal", "missing": 0, "arms": {"l6_d2": {"active": True, "gross_bps": 1.2, "after_1bp": 0.2, "after_5bp": -3.8}}},
            {"market": "SOL", "split": "temporal", "missing": 0, "arms": {"l6_d2": {"active": True, "gross_bps": 0.8, "after_1bp": -0.2, "after_5bp": -4.2}}},
        ]
        s = summarize_carry_cohort(carry)
        self.assertEqual(s["active_windows"], 2)
        self.assertAlmostEqual(s["gross_sum_bps_zero_fee_postonly"], 2.0)
        self.assertAlmostEqual(s["after_1bp_legacy_025_x4_bps"], 0.0)
        self.assertEqual(s["positive_windows_zero_overhead"], 2)
        self.assertEqual(s["positive_windows_legacy_1bp"], 1)

        with TemporaryDirectory() as tmp:
            rep = collect_local_rescoring(Path(tmp))
            self.assertAlmostEqual(rep["fixed_infra_usd_per_day"], float(DEFAULT_FIXED_INFRA_USD_PER_DAY))
            self.assertEqual(rep["cohorts"], {})

            cap_dir = Path(tmp) / "sprint8_capital_transfer_v2"
            cap_dir.mkdir()
            (cap_dir / "fresh-1.json").write_text(json.dumps({
                "rows": [
                    {"day": "2026-07-18", "cohort": "fresh_july", "status": "settled", "maker": 5000, "taker": 5000, "net": -3.45, "stress": -3.95},
                    {"day": "2026-07-24", "cohort": "fresh_july", "status": "trade_withheld"},
                ]
            }))
            rep2 = collect_local_rescoring(Path(tmp))
            cap = rep2["sprint8_core_btc_1000_25x"]
            self.assertEqual(len(cap["withheld"]), 1)
            self.assertEqual(cap["cohorts"]["fresh_july"]["windows"], 1)
            self.assertIsNone(cap["cohorts"]["fresh_july"]["program_score_rh"]["eligible_score_per_usd_cost"])
            self.assertEqual(cap["cohorts"]["fresh_july"]["evidence_class"], "replay_settled_cash")
            self.assertTrue(cap["cohorts"]["fresh_july"]["counts_as_settled_replay_cash"])
            ref = rep2["binance_2025_carry_reference"]
            self.assertEqual(ref["evidence_class"], "valuation_only_reference")
            self.assertFalse(ref["counts_as_settled_replay_cash"])


if __name__ == "__main__":
    unittest.main()
