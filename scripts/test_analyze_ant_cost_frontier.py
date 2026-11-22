import itertools
import unittest
from decimal import Decimal as D

from analyze_ant_cost_frontier import (
    fee_routes, funding_overlap, max_hedge_ratio, minimum_hedge_quantity, paired_taker_cash, wallet_frontier,
)


class FrontierTests(unittest.TestCase):
    def wallet(self, terminal="-2", position="0"):
        return {"terminal_pnl_usd": terminal,
                "known_turnover_usd": {"arcus": "10000", "xyz": "10000"},
                "venue_known_fees": {"arcus": "1", "xyz": "2"},
                "position_bounds": {"arcus": [position, position], "xyz": ["0", "0"]}}

    def test_unknown_fees_do_not_turn_unsettled_cash_into_gross(self):
        result = wallet_frontier(self.wallet(None, "2"))
        self.assertEqual(result["status"], "unresolved")
        self.assertIsNone(result["gross_before_fees"])

    def test_only_flat_terminal_wallet_is_priceable(self):
        with self.assertRaises(ValueError):
            wallet_frontier(self.wallet(position="1"))
        result = wallet_frontier(self.wallet())
        self.assertEqual(result["gross_before_fees"], D(1))
        self.assertEqual(result["break_even_fee_bps_all_turnover"], D(".5"))

    def test_idle_wallet_not_positive_evidence(self):
        w = self.wallet("0")
        w["known_turnover_usd"] = w["venue_known_fees"] = {"arcus": "0", "xyz": "0"}
        self.assertEqual(wallet_frontier(w)["status"], "idle")
        self.assertIsNone(wallet_frontier(w)["break_even_fee_bps_all_turnover"])

    def test_fee_denominators_and_negative_rebates(self):
        r = fee_routes()["arcus_MM_xyz_TT"]
        self.assertEqual(r["cycle_bps_one_leg"], D("1.8"))
        self.assertEqual(r["usd_per_million_all_venues"], D(45))
        self.assertEqual(r["usd_per_million_arcus"], D(90))
        self.assertEqual(fee_routes("-.3", ".47")["arcus_MM"]["usd_per_million_arcus"], D(-30))

    def test_cost_cap_preserves_infeasible_overhead(self):
        self.assertIsNone(max_hedge_ratio("20", ".25"))
        self.assertEqual(max_hedge_ratio("50", ".25"), D(".25") / D(".9"))

    def test_inventory_terminal_flatten_not_free(self):
        result = minimum_hedge_quantity([1, 1, 1], 2)
        self.assertEqual(result["hedge_quantity_before_drain"], D(1))
        self.assertEqual(result["terminal_flatten_quantity"], D(2))
        self.assertEqual(result["total_hedge_quantity"], D(3))
        self.assertEqual(minimum_hedge_quantity([1, -1], 2)["total_hedge_quantity"], 0)

    def test_projection_matches_exhaustive_optimum_including_drain(self):
        # Independent dynamic program over every permitted integer inventory.
        for band in (0, 1, 2):
            for deltas in itertools.product(range(-2, 3), repeat=4):
                states = {0: 0}
                for delta in deltas:
                    states = {new: min(cost + abs(old + delta - new)
                                       for old, cost in states.items())
                              for new in range(-band, band + 1)}
                optimum = min(cost + abs(pos) for pos, cost in states.items())
                self.assertEqual(minimum_hedge_quantity(deltas, band)["total_hedge_quantity"], optimum)

    def test_funding_hour_join_and_unit_boundary(self):
        arc = [{"time": 3600000000, "marketId": 88, "fundingRate": ".0001"}]
        xyz = [{"time": 3600000, "coin": "xyz:ANT", "fundingRate": ".00002"}]
        result = funding_overlap(arc, xyz, 0, 7200000)
        self.assertEqual(result["observed_spread_sum_bps"], D(".8"))
        self.assertFalse(result["complete"])
        self.assertIsNone(result["full_interval_spread_bps"])
        xyz.append(dict(xyz[0], fundingRate=".00003"))
        with self.assertRaises(ValueError):
            funding_overlap(arc, xyz, 0, 7200000)

    def test_funding_bounded_publication_offset_is_not_freshness_substitution(self):
        arc = [{"time": 3600000000, "marketId": 88, "fundingRate": ".0001"}]
        xyz = [{"time": 3600098, "coin": "xyz:ANT", "fundingRate": ".00002"}]
        result = funding_overlap(arc, xyz, 0, 3600000)
        self.assertTrue(result["complete"])
        self.assertEqual(result["source_offsets_ms"]["xyz"], [98])
        xyz[0]["time"] = 3601001
        with self.assertRaises(ValueError):
            funding_overlap(arc, xyz, 0, 3600000)

    def test_four_leg_exit_cost_and_direction(self):
        # Short Arcus 101, long xyz 100; cover Arcus 100, sell xyz 100.
        result = paired_taker_cash("101", "100", "100", "100", False)
        self.assertEqual(result["gross"], D(1))
        self.assertEqual(result["fees"], D(".040512"))
        self.assertEqual(result["net"], D(".959488"))
        reverse = paired_taker_cash("101", "100", "100", "100", True)
        self.assertEqual(reverse["net"], D("-1.040512"))


if __name__ == "__main__":
    unittest.main()
