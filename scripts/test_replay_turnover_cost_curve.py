import unittest
from decimal import Decimal
from replay_turnover_cost_curve import COST_CAPS, point, serialized_sum_matches, reward_eligibility
from replay_ten_hour_scenarios import summarize_quant, turnover_policies


class CostCurveTests(unittest.TestCase):
    def test_reward_denominators_are_unknown_with_explicit_fraction_scenarios(self):
        r = reward_eligibility(Decimal(1000), Decimal(800), Decimal("-0.4"))
        self.assertIsNone(r["confirmed_eligible_turnover"])
        self.assertFalse(r["premium_guarantees_rewards"])
        self.assertIsNone(r["fraction_scenarios"]["0"]["cost_bps_per_assumed_eligible_turnover"])
        self.assertEqual(Decimal(r["fraction_scenarios"]["0.2"]["cost_bps_per_assumed_eligible_turnover"]), Decimal(25))
        self.assertIsNone(reward_eligibility(Decimal(1000), None, None)["fraction_scenarios"]["1"]["assumed_eligible_turnover"])
        with self.assertRaises(ValueError):
            reward_eligibility(Decimal(100), Decimal(101), Decimal(0))

    def test_legacy_float_rounding_does_not_mask_cash_errors(self):
        self.assertTrue(serialized_sum_matches([0.1, 0.2], 0.30000000000000004))
        self.assertFalse(serialized_sum_matches([0.1, 0.2], 0.300000001))
        self.assertFalse(serialized_sum_matches(["0.1", "0.2"], "0.30000000000000004"))

    def test_grid_uses_target_volume_prices_and_preserves_all_postures(self):
        pair = {"legs": [{"venue": "lighter"}, {"venue": "lighter_robinhood"}]}
        window = {"start_ms": 1000, "end_ms": 36001000, "drain_until_ms": 36016000}
        grid = list(turnover_policies(pair, window, ("0", "0"), cost_caps=COST_CAPS))
        self.assertEqual(len(grid), 36)
        caps = {p["pair"]["mode"]["incentive"]["max_cost_bps"] for _, p in grid}
        self.assertEqual(caps, set(COST_CAPS))
        self.assertEqual({p["maker_cancel_delay_ms"] for _, p in grid}, {300})
        self.assertEqual({p["trade_delivery_grace_ms"] for _, p in grid}, {750})

    def test_zero_volume_has_no_cost_estimate_and_incomplete_is_not_zero(self):
        row = {"engine_exit_code": 0, "incomplete": None, "net_pnl": "0", "pnl_before_fees": "0",
               "fees": "0", "closed_cycles": 0, "open_positions_base": ["0", "0"],
               "execution_costs": {"cycles": []}, "turnover_feedback": {
                   "modeled_target_turnover_quote": "0", "net_after_overhead": "0", "overhead_quote": "0"}}
        self.assertTrue(point(row)["cash_attribution_reconciled"])
        self.assertIsNone(point(row)["realized_cost_bps_per_target_turnover"])
        self.assertIsNone(point(row)["forced_close_signed_fraction_of_total_cost"])
        self.assertIsNone(point(row | {"incomplete": "trade_feed_gap"})["net_after_overhead"])

    def test_exact_engine_total_avoids_legacy_float_rounding_without_relaxing_proof(self):
        row = {"engine_exit_code": 0, "incomplete": None, "net_pnl": -0.03279459999999999,
               "pnl_before_fees": -0.03279459999999999, "fees": 0, "closed_cycles": 1,
               "open_positions_base": [0, 0], "turnover_feedback": {
                   "modeled_target_turnover_quote": "100", "net_after_overhead": "-0.0427946",
                   "overhead_quote": "0.01"}, "execution_costs": {
                   "whole_ledger_cash_reconciled": True,
                   "whole_ledger_net_before_overhead": "-0.0327946",
                   "whole_ledger_net_after_overhead": "-0.0427946",
                   "cycles": [{"completed": True, "cash_reconciled": True,
                       "price_decomposition_reconciled": True, "net_before_overhead": "-0.0327946",
                       "opening_hedge_slippage_vs_submit_touch": "0", "stages": [],
                       "price_drift_cost_quote": "0.0327946", "entry_markouts": []}]}}
        self.assertTrue(point(row)["cash_attribution_reconciled"])
        self.assertEqual(point(row)["net_after_overhead_exact"], "-0.0427946")
        row["execution_costs"]["whole_ledger_cash_reconciled"] = False
        self.assertFalse(point(row)["cash_attribution_reconciled"])
        row["execution_costs"]["whole_ledger_cash_reconciled"] = True
        row["execution_costs"]["whole_ledger_net_before_overhead"] = "-0.0327945"
        self.assertFalse(point(row)["cash_attribution_reconciled"])

    def test_fee_price_and_overhead_costs_reconcile_per_target_volume(self):
        row = {"engine_exit_code": 0, "incomplete": None, "net_pnl": "-0.3", "pnl_before_fees": "-0.2",
               "fees": "0.1", "closed_cycles": 1, "open_positions_base": ["0", "0"],
               "execution_costs": {"cycles": [{"completed": True, "cash_reconciled": True,
                  "price_decomposition_reconciled": True, "net_before_overhead": "-0.3",
                  "opening_hedge_slippage_vs_submit_touch": "0.02",
                  "stages": [{"phase": "entry_hedge", "execution_cost_quote": "0.05"}],
                  "price_drift_cost_quote": "0.15", "entry_markouts": []}]},
               "turnover_feedback": {"modeled_target_turnover_quote": "1000",
                                      "net_after_overhead": "-0.4", "overhead_quote": "0.1"}}
        p = point(row)
        self.assertEqual(p["realized_cost_bps_per_target_turnover"], "4.0000")
        self.assertEqual(p["cost_quote_per_million_target_turnover"], "400.0000")
        self.assertEqual(p["price_loss_quote"], "0.2")
        self.assertEqual(p["price_drift_both_legs_quote"], "0.15")
        self.assertEqual(p["opening_hedge_slippage_vs_submit_touch_nonadditive"], "0.02")
        self.assertIsNone(p["entry_markouts_nonadditive"]["1000"]["mid_move_cost_bps"])
        row["net_pnl"] = "-0.2"
        self.assertFalse(point(row)["cash_attribution_reconciled"])
        self.assertIsNone(point(row)["net_after_overhead"])

    def test_overhead_is_a_labeled_assumption_and_removal_cannot_flip_negative_gross(self):
        row = {"final_position": 0, "resting_orders": 0, "needs_reconciliation": False,
               "halted": None, "trade_feed_continuity_verified": True,
               "net_liquidation_pnl": -0.03, "maker_turnover": 4000, "taker_turnover": 6000}
        s = summarize_quant(row)
        self.assertAlmostEqual(s["net_bps_before_modeled_overhead"], -0.03)
        self.assertEqual(s["fixed_ledger_overhead_sensitivity"]["0"], -0.03)
        self.assertAlmostEqual(s["fixed_ledger_overhead_sensitivity"]["0.25"], -0.28)
        self.assertIn("not_exchange_fee", s["modeled_overhead_source"])


if __name__ == "__main__":
    unittest.main()
