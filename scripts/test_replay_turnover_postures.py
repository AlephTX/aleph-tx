import unittest
from decimal import Decimal

from replay_turnover_postures import policies, summarize_posture


class TurnoverStudyTests(unittest.TestCase):
    def test_missing_exit_evidence_never_becomes_zero_cost(self):
        rows = [{"start_ms": 1000, "end_ms": 5000, "incomplete": "residual_at_cutoff",
                 "net_pnl": None, "open_positions_base": ["0.01", "0"],
                 "turnover_feedback": {"modeled_target_turnover_quote": "50",
                                       "net_after_overhead": None}}]
        summary = summarize_posture(rows)
        self.assertIsNone(summary["cost_bps_per_target_turnover"])
        self.assertIsNone(summary["net_after_overhead_quote"])
        self.assertEqual(summary["target_turnover_quote"], "50")
        self.assertEqual(summary["residual_windows"], 1)

    def test_target_cost_uses_full_pnl_and_no_reward_credit(self):
        rows = [{"start_ms": 1000, "end_ms": 5000, "net_pnl": "0.03", "pnl_before_fees": "0.04",
                 "turnover_feedback": {"modeled_target_turnover_quote": "100",
                                       "net_after_overhead": "0.02"}}]
        summary = summarize_posture(rows)
        self.assertEqual(summary["net_after_overhead_quote"], "0.02")
        self.assertEqual(summary["cost_bps_per_target_turnover"], "-2.00")
        self.assertEqual(summary["points_value_quote"], "0")
        self.assertIsNone(summary["venue_credited_turnover_quote"])
        rows[0]["turnover_feedback"] = {"modeled_target_turnover_quote": 100.0,
                                        "net_after_overhead": 0.02}
        numeric = summarize_posture(rows)
        self.assertEqual(Decimal(numeric["net_after_overhead_quote"]), Decimal("0.02"))

    def test_every_arm_has_separate_objective_and_fixed_risk(self):
        arms = list(policies({"legs": []}, (1000, 901000, "holdout")))
        self.assertEqual(len(arms), 12)
        self.assertEqual(len({label for label, _ in arms}), 12)
        for _, config in arms:
            self.assertEqual(config["pair"]["mode"]["kind"], "turnover")
            self.assertEqual(config["unhedged_timeout_ms"], 2500)
            self.assertNotIn("residual_fraction", config["turnover_posture"])


if __name__ == "__main__":
    unittest.main()
