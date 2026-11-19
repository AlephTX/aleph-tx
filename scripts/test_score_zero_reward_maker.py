import unittest

from score_zero_reward_maker import score_window


def config():
    return dict(start_ms=0, trade_from_ms=60_000, end_ms=36_060_000, drain_until_ms=36_075_000,
                flat_at_funding=dict(offset_ms=0, interval_ms=3_600_000))


def flat_report():
    return dict(trade_feed_continuity_verified=True, funding_boundaries_verified=10, final_position=0,
                resting_orders=0, needs_reconciliation=False, net_liquidation_pnl=0.1,
                maker_turnover=800, taker_turnover=200)


class ZeroRewardCashTest(unittest.TestCase):
    def test_charge_overhead_once_and_score_all_cash_against_each_denominator(self):
        result = score_window(flat_report(), config())
        costs = result["overhead_bps_on_gross"]["0.25"]
        self.assertEqual(costs["net_cash"], 0.075)
        self.assertEqual(costs["cost_per_million_gross"], -75)
        self.assertEqual(costs["cost_per_million_maker"], -93.75)
        self.assertEqual(result["net025_plus_1bp_on_taker_turnover"], 0.055)
        self.assertEqual(result["break_even_overhead_bps_on_gross"], 1)

    def test_missing_cash_or_source_continuity_withholds_all_economic_claims(self):
        for field, value in [("trade_feed_continuity_verified", False), ("net_liquidation_pnl", None),
                             ("final_position", 1), ("funding_boundaries_verified", 9)]:
            with self.subTest(field=field):
                result = score_window(flat_report() | {field: value}, config())
                self.assertTrue(result["cash_blockers"])
                self.assertIsNone(result["overhead_bps_on_gross"]["0.25"]["cost_per_million_maker"])

    def test_zero_capacity_is_undefined_cost(self):
        r = flat_report() | dict(maker_turnover=0, taker_turnover=0, net_liquidation_pnl=0)
        result = score_window(r, config())
        self.assertIsNone(result["overhead_bps_on_gross"]["0"]["cost_per_million_gross"])
        self.assertEqual(result["overhead_bps_on_gross"]["0"]["net_cash"], 0)
        self.assertIsNone(result["break_even_overhead_bps_on_gross"])
        self.assertNotIn("decomposed_overhead", result)

    def test_decomposed_fixed_overhead_preserves_default_and_scores_fixed_windows(self):
        result = score_window(flat_report(), config(), fixed_overhead_usd_per_day="2.4")
        d = result["decomposed_overhead"]
        self.assertEqual(d["fixed_window_overhead_quote"], 1.0)
        self.assertAlmostEqual(d["variable_net_cash"], 0.1)
        self.assertAlmostEqual(d["net_cash_after_fixed_overhead"], -0.9)
        self.assertAlmostEqual(d["variable_cost_per_million_maker"], -125.0)
        self.assertAlmostEqual(d["all_in_cost_per_million_maker_observed_pace"], 1125.0)
        self.assertAlmostEqual(d["all_in_cost_per_million_maker_50m_30d_pace"], -123.56)
