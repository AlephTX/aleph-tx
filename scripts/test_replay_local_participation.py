import unittest

from replay_local_participation import control_matches, summarize_local


class LocalParticipationSummaryTests(unittest.TestCase):
    def test_horizons_and_cancel_cohorts_keep_their_own_denominators(self):
        report = summarize_local({"local_participation": {"decisions": 100, "ready_decisions": 5},
                                  "cross_market_attribution": {"maker_markouts": [
                                      {"horizon_ms": 1000, "marked_turnover": 100, "markout_quote": -0.1,
                                       "cancel_pending_marked_turnover": 10, "cancel_pending_markout_quote": -0.05,
                                       "expired_fills": 2, "pending_at_cutoff": 1}]}})
        self.assertEqual(report["local_ready_fraction"], 0.05)
        self.assertAlmostEqual(report["maker_markouts"][0]["markout_bps"], -10)
        self.assertAlmostEqual(report["maker_markouts"][0]["cancel_pending_markout_bps"], -50)
        self.assertEqual(report["maker_markouts"][0]["expired_fills"], 2)
        self.assertEqual(report["maker_markouts"][0]["pending_at_cutoff"], 1)

    def test_unobserved_is_not_zero_and_control_drift_is_rejected(self):
        self.assertIsNone(summarize_local({})["local_ready_fraction"])
        self.assertIsNone(summarize_local({})["cancel_timing"])
        ref = {"terminal_flat_and_feed_verified": True, "net_after_overhead": -0.1,
               "maker_turnover": 100, "taker_turnover": 50}
        self.assertTrue(control_matches(ref, ref))
        for change in ({"maker_turnover": 99}, {"net_after_overhead": None},
                       {"terminal_flat_and_feed_verified": False}):
            self.assertFalse(control_matches(ref, ref | change))

    def test_cancel_source_cohorts_preserve_unknown_clock_and_own_markout_denominators(self):
        timing = {"source_clock_execution_verified": False, "guard_calibrated": False,
                  "late_receipt_fills": 2, "cohorts": [
                      {"source_timing": "before_cancel_request", "fills": 2, "turnover": 200,
                       "markouts": [{"horizon_ms": 1000, "marked_fills": 1, "marked_turnover": 50,
                                     "markout_quote": 0.01, "expired_fills": 1, "pending_at_cutoff": 0}]},
                      {"source_timing": "during_cancel_window", "fills": 0, "turnover": 0,
                       "markouts": [{"horizon_ms": 1000, "marked_fills": 0, "marked_turnover": 0,
                                     "markout_quote": 0, "expired_fills": 0, "pending_at_cutoff": 0}]}]}
        report = summarize_local({"cross_market_attribution": {"cancel_timing": timing}})
        result = report["cancel_timing"]
        self.assertFalse(result["source_clock_execution_verified"])
        self.assertFalse(result["guard_calibrated"])
        self.assertEqual(result["cohorts"][0]["markouts"][0]["markout_bps"], 2)
        self.assertIsNone(result["cohorts"][1]["markouts"][0]["markout_bps"])
        self.assertNotIn("markout_bps", timing["cohorts"][0]["markouts"][0])
        self.assertEqual(report["cancel_pending_label_basis"],
                         "cancel_request_observed_by_simulated_fill_receipt")


if __name__ == "__main__":
    unittest.main()
