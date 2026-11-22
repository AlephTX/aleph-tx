#!/usr/bin/env python3
"""Unit tests for `scripts/portfolio_risk_budget.py` (`CRASH-IMP-03`)."""

from __future__ import annotations

import math
import unittest

from scripts.portfolio_risk_budget import (
    capped_fractional_kelly,
    empirical_var_es,
    evaluate_perp_stress_scenarios,
    inverse_vol_weights,
    portfolio_vol_and_rc,
    real_time_drawdown_brake_scale,
    stressed_corr_matrix,
)


class PortfolioRiskBudgetTests(unittest.TestCase):
    def test_inverse_vol_weights_and_risk_contributions(self) -> None:
        vols = [0.02, 0.04, 0.04]
        w = inverse_vol_weights(vols)
        self.assertAlmostEqual(sum(w), 1.0, places=12)
        self.assertAlmostEqual(w[0], 0.5, places=12)
        self.assertAlmostEqual(w[1], 0.25, places=12)
        # Under identity correlation, inverse-vol equalizes marginal risk contributions!
        eye = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]
        port_vol, rcs = portfolio_vol_and_rc(w, vols, eye)
        self.assertGreater(port_vol, 0.0)
        self.assertAlmostEqual(sum(rcs), 1.0, places=12)
        for rc in rcs:
            self.assertAlmostEqual(rc, 1.0 / 3.0, places=12)

    def test_crisis_correlation_floor_lifts_portfolio_vol(self) -> None:
        corr = [[1.0, 0.1], [0.1, 1.0]]
        vols = [0.02, 0.02]
        w = [0.5, 0.5]
        base_vol, _ = portfolio_vol_and_rc(w, vols, corr)
        stressed = stressed_corr_matrix(corr, stress_rho_floor=0.70)
        self.assertEqual(stressed[0][0], 1.0)
        self.assertAlmostEqual(stressed[0][1], 0.70, places=12)
        stress_vol, rcs = portfolio_vol_and_rc(w, vols, stressed)
        self.assertGreater(stress_vol, base_vol)
        self.assertAlmostEqual(sum(rcs), 1.0, places=12)

    def test_empirical_es_captures_fat_left_tail(self) -> None:
        # 39 calm +0.1% bars + 1 crash -12.0% bar
        returns = [0.001] * 39 + [-0.12]
        var_975, es_975 = empirical_var_es(returns, alpha=0.975)
        self.assertAlmostEqual(var_975, 0.12, places=12)
        self.assertAlmostEqual(es_975, 0.12, places=12)
        # 90% level covers 4 tail observations: 1 crash + 3 calm bars -> ES < VaR? No, ES >= VaR always for worst tail!
        var_90, es_90 = empirical_var_es(returns, alpha=0.90)
        self.assertGreaterEqual(es_90, var_90)
        self.assertGreater(es_90, 0.029)

    def test_capped_fractional_kelly_zero_on_non_positive_and_never_exceeds_cap(self) -> None:
        self.assertEqual(capped_fractional_kelly(0.0, 0.01), 0.0)
        self.assertEqual(capped_fractional_kelly(-0.005, 0.01), 0.0)
        self.assertEqual(capped_fractional_kelly(0.01, 0.0), 0.0)
        # Huge edge/variance would yield un-capped 5.0 -> strictly capped at <= 1.0!
        self.assertEqual(capped_fractional_kelly(0.20, 0.01, fraction=0.25, max_scale=1.0), 1.0)
        self.assertLessEqual(capped_fractional_kelly(0.20, 0.01, fraction=0.25, max_scale=0.6), 0.6)
        self.assertAlmostEqual(capped_fractional_kelly(0.01, 0.01, fraction=0.25), 0.25, places=12)

    def test_real_time_drawdown_brake_matches_rust_hysteresis_spec(self) -> None:
        tier, scale = real_time_drawdown_brake_scale(1000.0, 987.5, 50.0, prev_tier=0)
        self.assertEqual((tier, scale), (1, 0.75))
        # Stay Tier 1 in hysteresis band (r_dd = 0.22 >= 0.20)
        tier, scale = real_time_drawdown_brake_scale(1000.0, 989.0, 50.0, prev_tier=tier)
        self.assertEqual((tier, scale), (1, 0.75))
        # Drop to r_dd = 0.75 -> Tier 3
        tier, scale = real_time_drawdown_brake_scale(1000.0, 962.5, 50.0, prev_tier=tier)
        self.assertEqual((tier, scale), (3, 0.25))
        # Rebound to r_dd = 0.0 -> cleanly recovers to Tier 0 (no 260-period deadlock!)
        tier, scale = real_time_drawdown_brake_scale(1000.0, 1000.0, 50.0, prev_tier=tier)
        self.assertEqual((tier, scale), (0, 1.0))

    def test_perp_stress_evaluator_and_input_validation(self) -> None:
        stress = evaluate_perp_stress_scenarios(
            1000.0,
            [0.01, 0.02],
            [0.6, 0.4],
            [[1.0, 0.1], [0.1, 1.0]],
        )
        self.assertGreater(stress["stressed_portfolio_vol"], stress["base_portfolio_vol"])
        self.assertGreater(stress["combined_crash_loss_usd"], 0.0)
        self.assertTrue(all(math.isfinite(v) for v in stress.values()))

        with self.assertRaises(ValueError):
            inverse_vol_weights([0.0, 0.1])
        with self.assertRaises(ValueError):
            capped_fractional_kelly(0.01, 0.01, max_scale=1.5)


if __name__ == "__main__":
    unittest.main()
