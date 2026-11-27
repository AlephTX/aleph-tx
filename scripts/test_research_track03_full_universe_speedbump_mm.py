#!/usr/bin/env python3
"""Stdlib-only unit tests for `scripts/research_track03_full_universe_speedbump_mm.py`."""

from __future__ import annotations

import json
import subprocess
import sys
import unittest
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

import research_track03_full_universe_speedbump_mm as mm  # noqa: E402


class Track03FullUniverseSpeedbumpTests(unittest.TestCase):
    def test_stdlib_only_top_level_and_full_market_map(self) -> None:
        probe = subprocess.run(
            [
                sys.executable,
                "-S",
                "-c",
                (
                    f"import sys; sys.path.insert(0, {str(SCRIPT_DIR)!r}); "
                    "import research_track03_full_universe_speedbump_mm; "
                    "mods = [m for m in ('duckdb', 'numpy', 'pandas', 'pyarrow', 'scipy') if m in sys.modules]; "
                    "assert not mods, f'unexpected imports: {mods}'"
                ),
            ],
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertEqual(probe.returncode, 0, probe.stderr or probe.stdout)

        targets, prov = mm.build_full_universe_targets()
        self.assertTrue(prov["lighter_standard_scenario"])
        rh_targets = [t for t in targets if t.venue == "lighter_robinhood"]
        lt_targets = [t for t in targets if t.venue == "lighter"]
        self.assertEqual(len(rh_targets), 18)
        self.assertEqual(len(lt_targets), 19)
        self.assertEqual({t.settlement for t in rh_targets}, {"USDG"})
        self.assertEqual({t.settlement for t in lt_targets}, {"USDC"})
        # trade_xyz index mapping check
        self.assertEqual(mm.VENUE_SYMBOL_MAP["trade_xyz"]["SPY"], "xyz:SP500")
        self.assertEqual(mm.VENUE_SYMBOL_MAP["trade_xyz"]["QQQ"], "xyz:XYZ100")

    def test_codex140_l1_depth_clamping_freshness_and_330ms_terminal_residual(self) -> None:
        # 1. Shallow displayed L1 bid depth (cur_bsz=0.50) clips 2.00-unit IOC exit,
        #    leaving terminal_residual_notional_usd > 0 and distinguishing realized_only vs mtm_incl_residual!
        arm = mm.SpeedbumpArmConfig(
            name="G5_microprice_tfi",
            order_delay_ms=200,
            cancel_delay_ms=300,
            exit_delay_ms=330,
            quote_usd=200.0,
            max_pos_usd=400.0,
            max_hold_ms=10_000,
            max_book_age_ms=5_000,
            terminal_flatten_lead_ms=0,
            **mm.G5_BASE_PARAMS,
        )
        events_clip = [
            {"ts": 0, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.04, 10.0)]},
            {"ts": 250, "kind": "trade", "side": "SELL", "price": 99.99, "size": 5.0},
            {"ts": 4_500, "kind": "book", "bids": [(99.98, 10.0)], "asks": [(100.02, 10.0)]},
            {"ts": 9_000, "kind": "book", "bids": [(99.96, 10.0)], "asks": [(100.00, 10.0)]},
            # At ts=10_300: hold timeout >= 10_000ms queues SELL IOC for 10_300 + 330 = 10_630ms.
            {"ts": 10_300, "kind": "book", "bids": [(99.92, 0.5)], "asks": [(99.96, 5.0)]},
            # Book at 10_700 has ONLY 0.50 bid depth -> clips 2.00-unit exit to 0.50!
            {"ts": 10_700, "kind": "book", "bids": [(99.90, 0.5)], "asks": [(99.94, 5.0)]},
        ]
        res_clip = mm.simulate_arm(events_clip, 0.01, arm)
        self.assertFalse(res_clip["terminal_flat"])
        self.assertGreaterEqual(res_clip["depth_clipped_exits"], 1)
        self.assertGreater(res_clip["terminal_residual_notional_usd"], 100.0)
        self.assertNotAlmostEqual(res_clip["realized_only_usd_per_m"], res_clip["mtm_incl_residual_usd_per_m"], places=1)
        self.assertAlmostEqual(
            res_clip["_exact_mtm_gross_usd"], res_clip["_exact_cash_ex_fee_usd"], places=6
        )

        # 2. Stale terminal book (> max_book_age_ms=5_000) blocks IOC exit and prevents silent 0ms flatten!
        events_stale = [
            {"ts": 0, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.04, 10.0)]},
            {"ts": 250, "kind": "trade", "side": "SELL", "price": 99.99, "size": 5.0},
            # No book after ts=0, only a trade at ts=12_000 (book age 12_000 > 5_000ms)!
            {"ts": 12_000, "kind": "trade", "side": "BUY", "price": 100.04, "size": 1.0},
        ]
        res_stale = mm.simulate_arm(events_stale, 0.01, arm)
        self.assertFalse(res_stale["terminal_flat"])
        self.assertEqual(res_stale["taker_exits"], 0)
        self.assertGreater(res_stale["terminal_residual_notional_usd"], 150.0)

        # 3. Fresh book at exec_ts >= terminal_drain + 330ms with ample depth flattens cleanly!
        arm_drain = mm.SpeedbumpArmConfig(
            name="G5_microprice_tfi",
            order_delay_ms=200,
            cancel_delay_ms=300,
            exit_delay_ms=330,
            quote_usd=200.0,
            max_pos_usd=400.0,
            max_hold_ms=10_000,
            max_book_age_ms=5_000,
            terminal_flatten_lead_ms=1_000,
            **mm.G5_BASE_PARAMS,
        )
        events_clean = [
            {"ts": 0, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.04, 10.0)]},
            {"ts": 250, "kind": "trade", "side": "SELL", "price": 99.99, "size": 5.0},
            {"ts": 4_100, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.04, 10.0)]},
            {"ts": 5_000, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.04, 10.0)]},
        ]
        res_clean = mm.simulate_arm(events_clean, 0.01, arm_drain)
        self.assertTrue(res_clean["terminal_flat"])
        self.assertEqual(res_clean["terminal_residual_notional_usd"], 0.0)
        self.assertEqual(res_clean["taker_exits"], 1)

    def test_s1_s2_s3_speedbump_mechanisms_and_reduce_only_exemption(self) -> None:
        # S3 toxicity circuit breaker pauses opening quotes after adverse fills without blocking reduce_only!
        s3 = mm.build_stage_b_arms(
            latency_triples=[(200, 300, 330)],
            arm_names=["S3_toxicity_circuit_breaker"],
            param_overrides={
                "S3_toxicity_circuit_breaker": {
                    **mm.G5_BASE_PARAMS,
                    "toxicity_pause_ewma_bps": 1.0,
                    "toxicity_pause_ms": 5_000,
                    "toxicity_min_fills": 1,
                }
            },
        )[0]
        events_tox = [
            {"ts": 0, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.04, 10.0)]},
            {"ts": 250, "kind": "trade", "side": "SELL", "price": 99.99, "size": 8.0},
            # Price drops -15 bps at 1_500ms -> markout matures < -1.0 bps -> triggers toxicity pause
            # and cancels old resting ask (effective at 1_800ms).
            {"ts": 1_500, "kind": "book", "bids": [(99.85, 10.0)], "asks": [(99.89, 10.0)]},
            # At 1_850ms (during pause!), reduce_only Ask is placed at 99.88 (active at 2_050ms).
            {"ts": 1_850, "kind": "book", "bids": [(99.85, 10.0)], "asks": [(99.89, 10.0)]},
            # Trade at 2_100ms fills reduce_only Ask exit cleanly during pause!
            {"ts": 2_100, "kind": "trade", "side": "BUY", "price": 99.89, "size": 8.0},
            {"ts": 4_000, "kind": "book", "bids": [(99.85, 10.0)], "asks": [(99.89, 10.0)]},
        ]
        res_s3 = mm.simulate_arm(events_tox, 0.01, s3)
        self.assertGreaterEqual(res_s3["toxicity_pauses"], 1)
        self.assertTrue(res_s3["terminal_flat"])
        self.assertGreaterEqual(res_s3["maker_fills"], 2)

        # Verify S1 & S2 arms run cleanly and produce finite metrics.
        for arm_name in ("S1_latency_spread_floor", "S2_cross_venue_fair_shift", "S4_speedbump_combined"):
            arm_cfg = mm.build_stage_b_arms(latency_triples=[(200, 300, 330)], arm_names=[arm_name])[0]
            res = mm.simulate_arm(events_tox, 0.01, arm_cfg)
            self.assertIn("mtm_incl_residual_usd_per_m", res)
            self.assertIn("realized_only_usd_per_m", res)

    def test_p0_declared_end_ms_immunity_to_late_ref_ticks(self) -> None:
        arm = mm.SpeedbumpArmConfig(
            name="G5_microprice_tfi",
            order_delay_ms=200,
            cancel_delay_ms=300,
            exit_delay_ms=330,
            quote_usd=200.0,
            max_pos_usd=400.0,
            max_hold_ms=10_000,
            max_book_age_ms=5_000,
            terminal_flatten_lead_ms=1_000,
            **mm.G5_BASE_PARAMS,
        )
        base_events = [
            {"ts": 0, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.04, 10.0)]},
            {"ts": 0, "kind": "ref", "venue": "arcus", "bid": 100.00, "ask": 100.04},
            {"ts": 250, "kind": "trade", "side": "SELL", "price": 99.99, "size": 5.0},
            {"ts": 4_100, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.04, 10.0)]},
            {"ts": 5_000, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.04, 10.0)]},
        ]
        late_ref_events = list(base_events) + [
            {"ts": 25_000, "kind": "ref", "venue": "arcus", "bid": 100.10, "ask": 100.14},
            {"ts": 30_000, "kind": "ref", "venue": "trade_xyz", "bid": 100.12, "ask": 100.16},
        ]
        res_base = mm.simulate_arm(base_events, 0.01, arm, declared_end_ms=5_000)
        res_late_explicit = mm.simulate_arm(late_ref_events, 0.01, arm, declared_end_ms=5_000)
        res_late_auto = mm.simulate_arm(late_ref_events, 0.01, arm)

        self.assertTrue(res_base["terminal_flat"])
        self.assertEqual(res_base["taker_exits"], 1)
        for res in (res_late_explicit, res_late_auto):
            self.assertTrue(res["terminal_flat"])
            self.assertEqual(res["taker_exits"], 1)
            self.assertEqual(res["terminal_residual_notional_usd"], 0.0)
            self.assertAlmostEqual(res["ntl"], res_base["ntl"], places=6)
            self.assertAlmostEqual(res["net"], res_base["net"], places=8)

        # Even if extra book events exist past declared_end_ms, declared_end_ms cuts off at 5_000 ms.
        extended_events = list(late_ref_events) + [
            {"ts": 20_000, "kind": "book", "bids": [(101.00, 10.0)], "asks": [(101.04, 10.0)]},
        ]
        res_cutoff = mm.simulate_arm(extended_events, 0.01, arm, declared_end_ms=5_000)
        self.assertTrue(res_cutoff["terminal_flat"])
        self.assertAlmostEqual(res_cutoff["net"], res_base["net"], places=8)

    def test_p0_stale_pending_ioc_requeues_330ms_and_tracks_stale_exit_blocks(self) -> None:
        arm = mm.SpeedbumpArmConfig(
            name="G5_microprice_tfi",
            order_delay_ms=200,
            cancel_delay_ms=300,
            exit_delay_ms=330,
            quote_usd=200.0,
            max_pos_usd=400.0,
            max_hold_ms=10_000,
            max_book_age_ms=5_000,
            terminal_flatten_lead_ms=0,
            **mm.G5_BASE_PARAMS,
        )
        # Book at 5_300 is fresh at 10_250 (age 4_950 <= 5_000), so maybe_queue_exit_ioc(10_250)
        # queues SELL IOC for exec_ts = 10_580. By 10_580, age is 5_280 > 5_000 (stale!).
        # When a fresh book arrives at 12_000, the expired IOC hits !book_is_fresh BEFORE update,
        # increments stale_exit_blocks, and re-queues for 12_000 + 330 = 12_330 (NOT 0 ms fill!).
        events_before_330 = [
            {"ts": 0, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.04, 10.0)]},
            {"ts": 250, "kind": "trade", "side": "SELL", "price": 99.99, "size": 5.0},
            {"ts": 4_500, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.04, 10.0)]},
            {"ts": 5_300, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.04, 10.0)]},
            {"ts": 10_250, "kind": "trade", "side": "BUY", "price": 100.05, "size": 0.1},
            {"ts": 12_000, "kind": "book", "bids": [(99.96, 10.0)], "asks": [(100.00, 10.0)]},
            {"ts": 12_200, "kind": "book", "bids": [(99.96, 10.0)], "asks": [(100.00, 10.0)]},
        ]
        res_early = mm.simulate_arm(events_before_330, 0.01, arm)
        self.assertEqual(res_early["stale_exit_blocks"], 1)
        self.assertEqual(res_early["taker_exits"], 0)
        self.assertFalse(res_early["terminal_flat"])

        # Once a fresh tick arrives at 12_330 (12_000 + 330 ms), the re-queued IOC fills cleanly!
        events_at_330 = list(events_before_330) + [
            {"ts": 12_330, "kind": "book", "bids": [(99.96, 10.0)], "asks": [(100.00, 10.0)]},
        ]
        res_done = mm.simulate_arm(events_at_330, 0.01, arm)
        self.assertEqual(res_done["stale_exit_blocks"], 1)
        self.assertEqual(res_done["taker_exits"], 1)
        self.assertTrue(res_done["terminal_flat"])
        self.assertEqual(res_done["terminal_residual_notional_usd"], 0.0)

    def test_cli_self_test(self) -> None:
        proc = subprocess.run(
            [sys.executable, "-S", str(SCRIPT_DIR / "research_track03_full_universe_speedbump_mm.py"), "--self-test"],
            check=True,
            capture_output=True,
            text=True,
        )
        payload = json.loads(proc.stdout)
        self.assertEqual(payload["self_test"], "ok")
        self.assertEqual(len(payload["arms_verified"]), 6)
        self.assertEqual(payload["lighter_robinhood_mkts"], 18)
        self.assertEqual(payload["lighter_mkts"], 19)


if __name__ == "__main__":
    unittest.main()
