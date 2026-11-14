import unittest

from replay_quant_participation import summarize


class ParticipationSummaryTests(unittest.TestCase):
    def test_signal_availability_is_separate_from_full_ledger_result(self):
        baseline = {"final_position": 0, "resting_orders": 0, "needs_reconciliation": False,
                    "halted": None, "trade_feed_continuity_verified": True,
                    "net_liquidation_pnl": -1, "maker_turnover": 100, "taker_turnover": 0}
        summary = summarize(baseline | {"quote_participation": {
            "decisions": 100_000, "fresh_reference_decisions": 23}})
        self.assertEqual(summary["fresh_reference_fraction"], 23 / 100_000)
        self.assertTrue(summary["terminal_flat_and_feed_verified"])
        self.assertLess(summary["net_after_overhead"], 0)

    def test_missing_coverage_remains_unknown_and_observed_zero_remains_zero(self):
        for participation in (None, {}, {"decisions": 0, "fresh_reference_decisions": 0}, {"decisions": 10}):
            self.assertIsNone(summarize({"quote_participation": participation})["fresh_reference_fraction"])
        self.assertEqual(summarize({"quote_participation": {
            "decisions": 10, "fresh_reference_decisions": 0}})["fresh_reference_fraction"], 0)


if __name__ == "__main__":
    unittest.main()
