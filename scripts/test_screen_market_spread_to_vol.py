import contextlib
import io
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPTS_DIR = Path(__file__).resolve().parent
if str(SCRIPTS_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPTS_DIR))

from screen_market_spread_to_vol import (
    evaluate_lighter_premium_upgrade,
    main,
    rank_markets,
    score_market_book_series,
)


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

    def test_barbell_section_2_1_premium_upgrade_benchmark_admitted(self):
        gate = evaluate_lighter_premium_upgrade(
            median_spread_bps=2.30,
            realized_1s_vol_bps=1.336,
        )
        self.assertEqual(gate["standard_delta_t_ms"], 302.0)
        self.assertEqual(gate["premium_delta_t_ms"], 2.0)
        self.assertAlmostEqual(gate["standard_latency_as_bps"], 0.642, places=3)
        self.assertAlmostEqual(gate["premium_latency_as_bps"], 0.052, places=3)
        self.assertAlmostEqual(gate["delta_as_300ms_to_0ms_bps"], 0.590, places=3)
        self.assertGreater(gate["delta_as_300ms_to_0ms_bps"], 0.40)
        self.assertAlmostEqual(gate["standard_net_edge_bps"], 0.308, places=3)
        self.assertAlmostEqual(gate["premium_net_edge_bps"], 0.498, places=3)
        self.assertTrue(gate["delta_as_exceeds_fee"])
        self.assertTrue(gate["premium_positive_net_edge"])
        self.assertTrue(gate["upgrade_admitted"])
        self.assertEqual(
            gate["operational_guardrails"],
            {
                "lit_stake_required": 0,
                "subaccounts_irreversible": True,
                "downgrade_cooldown_hours": 24,
            },
        )

    def test_narrow_spread_core_btc_eth_rwa_rejects_premium_upgrade(self):
        gate = evaluate_lighter_premium_upgrade(
            median_spread_bps=0.30,
            realized_1s_vol_bps=1.336,
        )
        self.assertTrue(gate["delta_as_exceeds_fee"])
        self.assertFalse(gate["premium_positive_net_edge"])
        self.assertLess(gate["premium_net_edge_bps"], 0.0)
        self.assertFalse(gate["upgrade_admitted"])

        low_vol_wide = evaluate_lighter_premium_upgrade(
            median_spread_bps=3.0,
            realized_1s_vol_bps=0.20,
        )
        self.assertFalse(low_vol_wide["delta_as_exceeds_fee"])
        self.assertTrue(low_vol_wide["premium_positive_net_edge"])
        self.assertFalse(low_vol_wide["upgrade_admitted"])

    def test_evaluate_lighter_premium_upgrade_fails_closed_on_invalid_inputs(self):
        base = dict(median_spread_bps=2.30, realized_1s_vol_bps=1.336)
        invalid_cases = [
            {"median_spread_bps": float("nan")},
            {"median_spread_bps": float("inf")},
            {"realized_1s_vol_bps": float("-inf")},
            {"median_spread_bps": -0.5},
            {"median_spread_bps": 0.0},
            {"realized_1s_vol_bps": -0.1},
            {"median_spread_bps": True},
            {"realized_1s_vol_bps": False},
            {"median_spread_bps": "2.30"},
            {"realized_1s_vol_bps": None},
            {"informed_flow_prob": -0.01},
            {"informed_flow_prob": 1.01},
            {"informed_sigma_mult": -1.0},
            {"non_latency_as_bps": -0.01},
            {"local_book_ms": -0.1},
            {"standard_cancel_ms": -1.0},
            {"premium_maker_fee_bps": -0.01},
        ]
        for overrides in invalid_cases:
            kwargs = {**base, **overrides}
            with self.subTest(overrides=overrides):
                with self.assertRaises(ValueError):
                    evaluate_lighter_premium_upgrade(**kwargs)

    def test_score_market_book_series_adds_drift_and_premium_gate(self):
        mid0 = 100.0
        half0 = mid0 * (1.15 / 10_000.0)
        mid1 = mid0 * (1.0 + 1.336 / 10_000.0)
        half1 = mid1 * (1.15 / 10_000.0)
        wide_frames = [
            {"timestamp_ms": 1000, "bids": [[mid0 - half0, 50.0]], "asks": [[mid0 + half0, 50.0]]},
            {"timestamp_ms": 2000, "bids": [[mid1 - half1, 50.0]], "asks": [[mid1 + half1, 50.0]]},
        ]
        scored = score_market_book_series(
            wide_frames,
            venue="lighter",
            liquidity_domain="lighter",
            symbol="S1_WIDE",
            asset_class="crypto",
            account_tier="standard",
        )
        self.assertAlmostEqual(scored["estimated_1s_abs_drift_bps"], 1.336, places=6)
        self.assertIsNotNone(scored["lighter_premium_gate"])
        self.assertTrue(scored["lighter_premium_gate"]["upgrade_admitted"])
        self.assertAlmostEqual(scored["lighter_premium_gate"]["premium_net_edge_bps"], 0.498, places=3)

        single_frame = score_market_book_series(
            wide_frames[:1],
            venue="lighter",
            liquidity_domain="lighter",
            symbol="S1_WIDE",
            asset_class="crypto",
            account_tier="standard",
        )
        self.assertIsNone(single_frame["estimated_1s_abs_drift_bps"])
        self.assertIsNone(single_frame["lighter_premium_gate"])

    def test_main_self_test_and_cli_execution(self):
        stdout_buf = io.StringIO()
        with contextlib.redirect_stdout(stdout_buf):
            rc = main(["--self-test"])
        self.assertEqual(rc, 0)
        payload = json.loads(stdout_buf.getvalue())
        self.assertEqual(payload["self_test"], "ok")
        self.assertTrue(payload["wide_s1_benchmark"]["upgrade_admitted"])
        self.assertFalse(payload["narrow_core_benchmark"]["upgrade_admitted"])

        proc = subprocess.run(
            [sys.executable, str(SCRIPTS_DIR / "screen_market_spread_to_vol.py"), "--self-test"],
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(proc.returncode, 0, msg=proc.stderr)
        cli_payload = json.loads(proc.stdout)
        self.assertEqual(cli_payload["self_test"], "ok")

        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            tape_path = tmp_path / "tape.jsonl"
            manifest_path = tmp_path / "manifest.json"
            mid0 = 100.0
            half0 = mid0 * (1.15 / 10_000.0)
            mid1 = mid0 * (1.0 + 1.336 / 10_000.0)
            half1 = mid1 * (1.15 / 10_000.0)
            tape_path.write_text(
                json.dumps({"timestamp_ms": 1000, "bids": [[mid0 - half0, 50.0]], "asks": [[mid0 + half0, 50.0]]})
                + "\n"
                + json.dumps({"timestamp_ms": 2000, "bids": [[mid1 - half1, 50.0]], "asks": [[mid1 + half1, 50.0]]})
                + "\n",
                encoding="utf-8",
            )
            manifest_path.write_text(
                json.dumps(
                    [
                        {
                            "jsonl": str(tape_path),
                            "venue": "lighter",
                            "liquidity_domain": "lighter",
                            "symbol": "S1_WIDE",
                            "asset_class": "crypto",
                            "account_tier": "standard",
                        }
                    ]
                ),
                encoding="utf-8",
            )
            manifest_buf = io.StringIO()
            with contextlib.redirect_stdout(manifest_buf):
                rc_manifest = main([str(manifest_path)])
            self.assertEqual(rc_manifest, 0)
            ranked = json.loads(manifest_buf.getvalue())
            self.assertEqual(len(ranked), 1)
            self.assertEqual(ranked[0]["symbol"], "S1_WIDE")
            self.assertTrue(ranked[0]["lighter_premium_gate"]["upgrade_admitted"])

        stderr_buf = io.StringIO()
        with contextlib.redirect_stderr(stderr_buf):
            with self.assertRaises(SystemExit) as ctx:
                main([])
        self.assertEqual(ctx.exception.code, 2)


if __name__ == "__main__":
    unittest.main()
