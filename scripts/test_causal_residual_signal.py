import math
import unittest

from causal_residual_signal import asof_prices, cash_budget, fit_betas, rank_targets, residual_scores


class ResidualSignalTests(unittest.TestCase):
    def test_publication_boundary_and_no_future_trade(self):
        self.assertEqual(asof_prices([(10, 2), (11, 3), (30, 7)], [12, 13, 14], 2, 10), [2, 3, 3])

    def test_expired_trade_remains_unknown(self):
        self.assertEqual(asof_prices([(10, 2)], [22, 23], 2, 10), [2, None])

    def test_duplicate_clock_and_nonfinite_price_rejected(self):
        for trades in ([(10, 2), (10, 3)], [(10, math.inf)], [(10, 0)]):
            with self.assertRaises(ValueError):
                asof_prices(trades, [12])

    def test_common_factor_does_not_change_residual_rank(self):
        base = [100, 102, 101, 105, 106]
        alt = [p*p/100 for p in base]
        fits = fit_betas({"BTC": base, "ALT": alt}, min_pairs=4)
        self.assertAlmostEqual(fits['ALT']['beta'], 2)
        scores = residual_scores({"BTC": (100, 110), "ALT": (100, 121)}, fits)
        self.assertAlmostEqual(scores['ALT'], 0, places=13)
        self.assertEqual(scores['BTC'], 0)

    def test_no_return_bridging_a_missing_minute(self):
        row = fit_betas({'BTC': [100, 101, None, 104, 103]}, min_pairs=3)['BTC']
        self.assertEqual(row['pairs'], 2)
        self.assertIsNone(row['beta'])

    def test_constant_factor_withheld(self):
        self.assertIsNone(fit_betas({'BTC': [100, 100, 100]}, min_pairs=2)['BTC']['beta'])

    def test_cutoff_tie_abstains(self):
        self.assertEqual(rank_targets({'a': -1, 'b': 0, 'c': 0, 'd': 2}), {})

    def test_negative_factor_beta_is_not_clipped(self):
        p = [100, 102, 101, 105]
        f = fit_betas({'BTC': p, 'ALT': [10000/x for x in p]}, min_pairs=3)
        self.assertAlmostEqual(f['ALT']['beta'], -1)

    def test_missing_fit_cannot_create_zero_alpha(self):
        self.assertIsNone(residual_scores({'BTC': (1, 2)}, {'BTC': {'beta': None}}))

    def test_cash_unknown_is_not_zero_or_full_cohort_profit(self):
        r = cash_budget([dict(cash=2, maker=100, taker=100, maker_bps=.25, taker_bps=1.25), dict(cash=None)])
        self.assertEqual(r['known_net_after_additional_cost'], '1.985')
        self.assertEqual(r['unknown'], 1)
        self.assertIsNone(r['complete_net_after_fixed'])
        self.assertIsNone(r['uniform_additional_break_even_bps'])

    def test_execution_optimization_cannot_pay_negative_headroom(self):
        r = cash_budget([dict(cash=1, maker=1000, taker=0, maker_bps=0, taker_bps=0)], fixed_cost=2)
        self.assertEqual(r['uniform_additional_break_even_bps'], '-10')
        self.assertEqual(r['complete_net_after_fixed'], '-1')

    def test_fees_use_actual_turnover_and_distinct_roles(self):
        r = cash_budget([dict(cash=1, maker=1000, taker=100, maker_bps=.25, taker_bps=1.25)])
        self.assertEqual(r['known_additional_cost'], '0.0375')
        self.assertEqual(r['complete_net_after_fixed'], '0.9625')

    def test_invalid_budget_rejected(self):
        with self.assertRaises(ValueError):
            cash_budget([dict(cash=1, maker=-1, taker=0, maker_bps=0, taker_bps=0)])


if __name__ == '__main__':
    unittest.main()
