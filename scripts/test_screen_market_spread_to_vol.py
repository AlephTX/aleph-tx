import unittest

from screen_market_spread_to_vol import rank_markets, score_market_book_series


class SpreadToVolScreenerTest(unittest.TestCase):
    def test_nonfinite_and_regressed_frames_do_not_change_screen(self):
        def quote(ts, bid, ask):
            return {"timestamp_ms": ts, "bids": [[bid, 10.0]], "asks": [[ask, 10.0]]}

        inputs = dict(venue="lighter", liquidity_domain="lighter", symbol="ETH",
                      asset_class="crypto", account_tier="standard")
        first = quote(1000, 99.99, 100.01)
        last = quote(2000, 99.995, 100.015)
        clean = score_market_book_series([first, last], **inputs)
        bad = [
            quote(float("nan"), 99.99, 100.01),
            quote(1200, 99.99, float("inf")),
            quote(999, 199.99, 200.01),
        ]
        self.assertEqual(score_market_book_series([first, *bad, last], **inputs), clean)

    def test_ranks_wide_calm_zero_fee_rwa_above_crowded_or_high_fee_markets(self):
        # Crowded zero-fee BTC: 0.2 bp spread, 1.0 bp/sqrt(s) volatility -> ratio 0.2
        btc_samples = [
            {"timestamp_ms": 1000 * (i + 1), "bids": [[100000.0 + (10 if i % 2 else -10) - 1.0, 2.0]], "asks": [[100000.0 + (10 if i % 2 else -10) + 1.0, 2.0]]}
            for i in range(6)
        ]
        # Arcus RWA NVDA L0: 4.0 bp spread, 0.5 bp/sqrt(s) volatility -> ratio ~8.0, 0.00 bp maker fee!
        nvda_samples = [
            {"timestamp_ms": 1000 * (i + 1), "bids": [[100.0 + (0.005 if i % 2 else 0.0) - 0.02, 50.0]], "asks": [[100.0 + (0.005 if i % 2 else 0.0) + 0.02, 50.0]]}
            for i in range(6)
        ]
        # Arcus Crypto SOL L0: 2.0 bp spread, 1.50 bp maker fee -> negative net half-spread!
        sol_samples = [
            {"timestamp_ms": 1000 * (i + 1), "bids": [[200.0 - 0.02, 10.0]], "asks": [[200.0 + 0.02, 10.0]]}
            for i in range(6)
        ]

        btc = score_market_book_series(
            btc_samples, venue="lighter", liquidity_domain="lighter", symbol="BTC",
            asset_class="crypto", account_tier="standard",
        )
        nvda = score_market_book_series(
            nvda_samples, venue="arcus", liquidity_domain="arcus", symbol="NVDA",
            asset_class="equity", account_tier="level_0",
        )
        sol = score_market_book_series(
            sol_samples, venue="arcus", liquidity_domain="arcus", symbol="SOL",
            asset_class="crypto", account_tier="level_0",
        )
        self.assertGreater(nvda["spread_to_1s_vol_ratio"], btc["spread_to_1s_vol_ratio"])
        self.assertGreater(nvda["net_half_spread_after_maker_fee_bps"], 0.0)
        self.assertLess(sol["net_half_spread_after_maker_fee_bps"], 0.0)
        ranked = rank_markets([btc, sol, nvda])
        self.assertEqual([m["symbol"] for m in ranked], ["NVDA", "BTC", "SOL"])

    def test_zero_timestamp_included_and_adverse_selection_margin_gates_ranking(self):
        # Verify timestamp_ms == 0 frame is not dropped and adverse-selection margin gates ranking.
        zero_ts_samples = [
            {"timestamp_ms": 0, "bids": [[100.0 - 0.02, 10.0]], "asks": [[100.0 + 0.02, 10.0]]},
            {"timestamp_ms": 1000, "bids": [[100.005 - 0.02, 10.0]], "asks": [[100.005 + 0.02, 10.0]]},
        ]
        passing = score_market_book_series(
            zero_ts_samples,
            venue="lighter",
            liquidity_domain="lighter",
            symbol="NVDA",
            asset_class="equity",
            account_tier="standard",
            expected_1s_adverse_selection_bps=0.5,
        )
        self.assertEqual(passing["samples"], 2)
        self.assertEqual(passing["vol_intervals"], 1)
        self.assertTrue(passing["adverse_selection_margin_ok"])

        failing = score_market_book_series(
            zero_ts_samples,
            venue="lighter_robinhood",
            liquidity_domain="lighter:robinhood",
            symbol="SPY",
            asset_class="equity",
            account_tier="standard",
            expected_1s_adverse_selection_bps=1.5,
        )
        self.assertFalse(failing["adverse_selection_margin_ok"])
        # Rebate/string-zero tolerance + adverse-selection margin gates SPY below NVDA.
        ranked = rank_markets([failing, passing])
        self.assertEqual([m["symbol"] for m in ranked], ["NVDA", "SPY"])


if __name__ == "__main__":
    unittest.main()
