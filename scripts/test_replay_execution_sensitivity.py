import unittest
from replay_quant_execution_sensitivity import variants
from replay_cancel_guard import control_matches


class ExecutionSensitivityTests(unittest.TestCase):
    def baseline(self):
        return {"core_only": True, "strategy": {"maker_fee_bps": 0, "order_ttl_secs": 5,
                "max_position_notional_usd": 150}, "taker_fee_bps": 0,
                "venue_cancel_delay_ms": 300, "venue_taker_delay_ms": 300,
                "order_entry_ms": 250, "order_response_ms": 100,
                "trade_from_ms": 1000, "end_ms": 36001000,
                "flat_at_funding": {"guard_ms": 1000}}

    def test_fee_latency_and_ttl_are_separate_with_risk_controls_unchanged(self):
        cfg = self.baseline()
        arms = dict(variants(cfg))
        premium = arms["premium-counterfactual"]
        self.assertEqual((premium["strategy"]["maker_fee_bps"], premium["taker_fee_bps"]), (0.4, 2.8))
        self.assertEqual((premium["venue_cancel_delay_ms"], premium["venue_taker_delay_ms"]), (0, 140))
        for arm in arms.values():
            self.assertEqual(arm["order_entry_ms"], 250)
            self.assertEqual(arm["flat_at_funding"], cfg["flat_at_funding"])
            self.assertEqual(arm["strategy"]["max_position_notional_usd"], 150)
        for seconds in (15, 30):
            arm = arms[f"standard-ttl-{seconds}s"]
            self.assertEqual(arm["strategy"]["order_ttl_secs"], seconds)
            self.assertEqual(arm["venue_cancel_delay_ms"], 300)
            self.assertEqual(arm["taker_fee_bps"], 0)
        self.assertEqual(cfg["strategy"]["order_ttl_secs"], 5)
        self.assertEqual(cfg["strategy"]["maker_fee_bps"], 0)

    def test_invalid_parent_and_changed_economic_control_are_rejected(self):
        with self.assertRaises(ValueError):
            list(variants(self.baseline() | {"core_only": False}))
        old = {"ledger_complete": True, "incomplete": None, "closed_cycles": 1,
               "observed_target_turnover": "100", "net_after_overhead": "-0.1"}
        self.assertTrue(control_matches(old, old | {"cancel_guard_overlap_prints": 3}))
        self.assertFalse(control_matches(old, old | {"incomplete": "resting_order_during_trade_gap"}))
        self.assertFalse(control_matches(old, old | {"net_after_overhead": "0.1"}))


if __name__ == "__main__":
    unittest.main()
