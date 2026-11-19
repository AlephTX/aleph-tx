import unittest

from prepare_leveraged_taiwan_lead import build_leveraged_taiwan_core_btc_spec


class LeveragedTaiwanLeadTest(unittest.TestCase):
    def test_validates_1000_equity_25x_leverage_and_blocks_naive_downscaling_traps(self):
        spec = build_leveraged_taiwan_core_btc_spec(
            start_ms=1_800_000_000_000,
            end_ms=1_800_036_000_000,
            drain_until_ms=1_800_036_015_000,
        )
        cfg = spec["replay_config"]
        self.assertEqual(cfg["initial_equity"], 1000.0)
        self.assertEqual(cfg["strategy"]["exchange_id"], 2)
        self.assertEqual(cfg["trade_from_ms"], cfg["start_ms"])
        self.assertEqual(cfg["strategy"]["base_order_notional_usd"], 5000.0)
        self.assertEqual(cfg["strategy"]["reference_portfolio_value"], 1000.0)
        self.assertEqual(cfg["strategy"]["min_portfolio_scale"], 1.0)
        self.assertEqual(cfg["strategy"]["max_portfolio_scale"], 1.0)
        self.assertTrue(cfg["strategy"]["cap_utilization_floor_to_base_notional"])
        self.assertEqual(cfg["strategy"]["max_leverage"], 25.0)
        self.assertEqual(spec["margin_audit"]["initial_margin_at_max_pos_usd"], 400.0)
        self.assertEqual(spec["margin_audit"]["daily_loss_hard_stop_usd"], 50.0)
        self.assertEqual(spec["eligibility_audit"]["eligible_target_maker_ceiling_quote"], "0")

        # Naive 10x leverage fails margin bound on $10k position at $1k equity.
        with self.assertRaises(ValueError):
            build_leveraged_taiwan_core_btc_spec(
                start_ms=1_800_000_000_000,
                end_ms=1_800_036_000_000,
                drain_until_ms=1_800_036_015_000,
                max_leverage=10.0,
            )

        # Naive 0.2% daily loss cap ($2) rejects before historical $22.99/$40.00 drawdown variance.
        with self.assertRaises(ValueError):
            build_leveraged_taiwan_core_btc_spec(
                start_ms=1_800_000_000_000,
                end_ms=1_800_036_000_000,
                drain_until_ms=1_800_036_015_000,
                max_daily_loss_pct=0.002,
            )


if __name__ == "__main__":
    unittest.main()
