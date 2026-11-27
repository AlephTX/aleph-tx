#!/usr/bin/env python3
"""Stdlib-only unit tests for `scripts/replay_rwa_volume_farming.py`."""

from __future__ import annotations

import json
import math
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

import eval_volume_farming_cost as vf  # noqa: E402
import replay_rwa_volume_farming as rwa  # noqa: E402


class ReplayRwaVolumeFarmingTests(unittest.TestCase):
    def test_stdlib_only_top_level_and_canonical_metadata(self) -> None:
        # duckdb/numpy/pandas must not be imported at module top-level (isolated subprocess check)
        probe = subprocess.run(
            [
                sys.executable,
                "-c",
                (
                    f"import sys; sys.path.insert(0, {str(SCRIPT_DIR)!r}); "
                    "import replay_rwa_volume_farming; "
                    "mods = [m for m in ('duckdb', 'numpy', 'pandas', 'pyarrow', 'scipy') if m in sys.modules]; "
                    "assert not mods, f'unexpected third-party imports: {mods}'"
                ),
            ],
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertEqual(probe.returncode, 0, probe.stderr or probe.stdout)
        self.assertEqual(
            sorted(rwa.CANONICAL_RWA_WINDOWS["selection_dev"].keys()),
            ["w18", "w19", "w20", "w21"],
        )
        self.assertEqual(
            sorted(rwa.CANONICAL_RWA_WINDOWS["holdout"].keys()),
            ["w23", "w24", "w25"],
        )
        self.assertEqual(rwa.VENUE_SYMBOL_MAP["lighter"]["NVDA"], "110")
        self.assertEqual(rwa.VENUE_SYMBOL_MAP["lighter_robinhood"]["NVDA"], "15")
        self.assertEqual(rwa.VENUE_SYMBOL_MAP["arcus"]["NVDA"], "NVDA-USD")
        self.assertEqual(rwa.VENUE_SYMBOL_MAP["trade_xyz"]["DRAM"], "xyz:DRAM")
        self.assertEqual(rwa.VENUE_LATENCY_MS["lighter"], (200, 200, 330))
        self.assertEqual(rwa.VENUE_LATENCY_MS["lighter_robinhood"], (200, 200, 330))
        self.assertEqual(rwa.build_arms("lighter")[0].exit_delay_ms, 330)
        self.assertEqual(rwa.build_arms("lighter_robinhood")[0].exit_delay_ms, 330)

        by_venue_settlement = {(t.venue, t.settlement, t.account_fee_status) for t in rwa.DEFAULT_TARGETS}
        self.assertIn(("arcus", "USDC", "public_tier0_schedule_only"), by_venue_settlement)
        self.assertIn(("lighter", "USDC", "public_tier0_unverified"), by_venue_settlement)
        self.assertIn(("lighter_robinhood", "USDG", "public_tier0_unverified"), by_venue_settlement)

    def test_cross_midnight_hive_globbing(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            cid = "tokyo-rwa-20261124T231453Z-w23"
            d0 = root / "parquet/bbo_quotes/collector_region=tokyo/venue=arcus/utc_date=2026-11-24" / f"campaign_id={cid}"
            d1 = root / "parquet/bbo_quotes/collector_region=tokyo/venue=arcus/utc_date=2026-11-25" / f"campaign_id={cid}"
            d2 = root / "parquet/bbo_quotes/collector_region=tokyo/venue=arcus/utc_date=2026-11-26" / f"campaign_id={cid}"
            for d in (d0, d1, d2):
                d.mkdir(parents=True, exist_ok=True)
            f0 = d0 / "part-0.parquet"
            f1 = d1 / "part-1.parquet"
            f2 = d2 / "part-2.parquet"
            for f in (f0, f1, f2):
                f.write_bytes(b"PAR1")

            found = rwa._campaign_parquet_files(root, "bbo_quotes", "arcus", "2026-11-24", cid)
            self.assertEqual(found, sorted([str(f0), str(f1)]))

    def test_fifo_queue_ahead_shrink_touch_and_trade_through(self) -> None:
        # Arcus order_delay=36ms, cancel_delay=33ms.
        # At ts=0: bid 100.00 x 10.0 -> at activation (ts>=36), queue_ahead = 0.5*10 = 5.0.
        # At ts=50: BBO shrinks to 100.00 x 3.0 -> queue_ahead shrinks to 3.0.
        # At ts=60: SELL trade 1.0 @ 100.00 -> consumes queue_ahead (3.0 -> 2.0), 0 fill.
        # At ts=80: SELL trade 3.0 @ 100.00 -> consumes remaining 2.0 queue_ahead, 1.0 fills BID!
        # At ts=500: BUY trade 2.0 @ 100.05 (strict trade-through over 100.04 ASK) -> fills passive ASK exit.
        arm = rwa.build_arms("arcus", maker_fee_bps=0.0, taker_fee_bps=2.25, arm_names=["S0_baseline_touch"])[0]
        events = [
            {"ts": 0, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.04, 10.0)]},
            # Trade before order_delay_ms (ts=20 < 36) must NOT fill
            {"ts": 20, "kind": "trade", "side": "SELL", "price": 99.90, "size": 50.0},
            {"ts": 50, "kind": "book", "bids": [(100.00, 3.0)], "asks": [(100.04, 10.0)]},
            {"ts": 60, "kind": "trade", "side": "SELL", "price": 100.00, "size": 1.0},
            {"ts": 80, "kind": "trade", "side": "SELL", "price": 100.00, "size": 3.0},
            {"ts": 500, "kind": "trade", "side": "BUY", "price": 100.05, "size": 2.0},
            {"ts": 1_500, "kind": "book", "bids": [(100.01, 5.0)], "asks": [(100.05, 5.0)]},
        ]
        res = rwa.simulate_rwa_cycle_arm(events, 0.01, arm)
        self.assertTrue(res["terminal_flat"])
        self.assertGreaterEqual(res["maker_fills"], 2)
        self.assertEqual(res["price_through_fills"], 1)
        self.assertAlmostEqual(res["_exact_gross_usd"], res["_exact_cash_ex_fee_usd"], places=10)
        self.assertAlmostEqual(
            res["spr"] + res["drft"] + res["pex"] + res["tslp"],
            res["gross_usd"],
            places=6,
        )

    def test_keep_queue_ticks_preserves_queue_vs_s0_resets(self) -> None:
        # Bid moves from 100.00 to 100.01 (1 tick).
        # S0 (keep_queue_ticks=0) cancels and rejoins back of queue (queue_ahead=5.0 at 100.01),
        # while S1 (keep_queue_ticks=3) keeps resting order at 100.00 and gets filled by trade-through!
        s0, s1 = rwa.build_arms(
            "arcus",
            maker_fee_bps=0.0,
            taker_fee_bps=2.25,
            arm_names=["S0_baseline_touch", "S1_markout_shield"],
        )
        events = [
            {"ts": 0, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.04, 10.0)]},
            {"ts": 100, "kind": "book", "bids": [(100.01, 10.0)], "asks": [(100.05, 10.0)]},
            # At ts=150: S0 cancelled at 100+33=133 and placed at 100.01 (active at 133+36=169 -> NOT active yet at 150!)
            # S1 kept order at 100.00 -> active and fills on trade at 100.00!
            {"ts": 150, "kind": "trade", "side": "SELL", "price": 99.99, "size": 2.0},
            {"ts": 1_500, "kind": "book", "bids": [(100.01, 10.0)], "asks": [(100.05, 10.0)]},
        ]
        res_s0 = rwa.simulate_rwa_cycle_arm(events, 0.01, s0)
        res_s1 = rwa.simulate_rwa_cycle_arm(events, 0.01, s1)
        self.assertEqual(res_s0["maker_fills"], 0)
        self.assertEqual(res_s1["maker_fills"], 1)
        self.assertGreater(res_s1["qpres"], 0)

    def test_tradeless_feed_produces_zero_fills(self) -> None:
        events = [
            {"ts": 0, "kind": "book", "bids": [(100.00, 5.0)], "asks": [(100.05, 5.0)]},
            {"ts": 250, "kind": "book", "bids": [(100.10, 5.0)], "asks": [(100.15, 5.0)]},
            {"ts": 500, "kind": "book", "bids": [(99.95, 5.0)], "asks": [(100.00, 5.0)]},
        ]
        for arm in rwa.build_arms("lighter", maker_fee_bps=0.0, taker_fee_bps=0.0):
            res = rwa.simulate_rwa_cycle_arm(events, 0.01, arm)
            self.assertEqual(res["fills"], 0)
            self.assertEqual(res["ntl"], 0.0)
            self.assertEqual(res["fill_evidence_mode"], "no_trades_in_window")
            self.assertTrue(res["terminal_flat"])

    def test_exact_accounting_and_cost_identity_all_six_arms(self) -> None:
        # Build a realistic 30s stream with passive fills, adverse drift, timeouts, oracle updates, and high vol
        events = [
            {"ts": 0, "kind": "book", "bids": [(100.00, 4.0)], "asks": [(100.06, 4.0)]},
            {"ts": 0, "kind": "ref", "venue": "trade_xyz", "bid": 100.00, "ask": 100.06},
            {"ts": 100, "kind": "trade", "side": "SELL", "price": 99.99, "size": 5.0},
            {"ts": 500, "kind": "book", "bids": [(100.02, 4.0)], "asks": [(100.08, 4.0)]},
            {"ts": 500, "kind": "ref", "venue": "trade_xyz", "bid": 100.02, "ask": 100.08},
            {"ts": 600, "kind": "trade", "side": "BUY", "price": 100.09, "size": 5.0},
            # Adverse jump + second entry that hits 15s IOC exit
            {"ts": 1_200, "kind": "book", "bids": [(100.00, 2.0)], "asks": [(100.06, 2.0)]},
            {"ts": 1_300, "kind": "trade", "side": "SELL", "price": 99.98, "size": 5.0},
            {"ts": 2_000, "kind": "book", "bids": [(99.95, 4.0)], "asks": [(100.01, 4.0)]},
            {"ts": 7_000, "kind": "book", "bids": [(99.94, 4.0)], "asks": [(100.00, 4.0)]},
            {"ts": 12_000, "kind": "book", "bids": [(99.93, 4.0)], "asks": [(99.99, 4.0)]},
            {"ts": 17_000, "kind": "book", "bids": [(99.92, 4.0)], "asks": [(99.98, 4.0)]},
            {"ts": 18_000, "kind": "book", "bids": [(99.92, 4.0)], "asks": [(99.98, 4.0)]},
        ]
        arms = rwa.build_arms("arcus", maker_fee_bps=0.0, taker_fee_bps=2.25, rebate_bps=0.0)
        self.assertEqual(len(arms), 6)
        self.assertEqual(
            [a.name for a in arms],
            [
                "S0_baseline_touch",
                "S1_markout_shield",
                "G1_as_inv_skew",
                "G2_markout_skew_pause",
                "G3_markout_skew_oracle",
                "G4_quiet_regime_only",
            ],
        )

        for arm in arms:
            res = rwa.simulate_rwa_cycle_arm(events, 0.01, arm)
            self.assertTrue(res["terminal_flat"])
            self.assertAlmostEqual(res["_exact_gross_usd"], res["_exact_cash_ex_fee_usd"], places=9)
            self.assertAlmostEqual(
                res["spr"] + res["drft"] + res["pex"] + res["tslp"],
                res["gross_usd"],
                places=6,
            )
            self.assertAlmostEqual(res["net"], res["gross_usd"] - res["fee_usd"], places=6)
            if res["ntl"] > 0.0:
                bd = vf.compute_volume_farming_cost(
                    res["gross_ex_fee_bps"],
                    res["taker_exit_share_s"],
                    arm.taker_fee_bps,
                    maker_fee_bps=arm.maker_fee_bps,
                    rebate_bps=arm.rebate_bps,
                )
                self.assertAlmostEqual(res["double_sided_usd_per_m"], bd.double_sided_usd_per_m, places=3)

        # Check S0 has 1s, 5s, 10s markouts computed from causal mid_at
        res_s0 = rwa.simulate_rwa_cycle_arm(events, 0.01, arms[0])
        self.assertIsNotNone(res_s0["markout_1s_bps"])
        self.assertIsNotNone(res_s0["markout_5s_bps"])
        self.assertIsNotNone(res_s0["markout_10s_bps"])

    def test_g3_oracle_suppression_and_ref_unavailable_fallback(self) -> None:
        g3 = rwa.build_arms("arcus", maker_fee_bps=0.0, taker_fee_bps=2.25, arm_names=["G3_markout_skew_oracle"])[0]
        # Ref moves sharply UP by +10 bps while local venue lags -> ASK is stale-cheap and must be suppressed!
        events_with_ref = [
            {"ts": 0, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.04, 10.0)]},
            {"ts": 0, "kind": "ref", "venue": "trade_xyz", "bid": 100.00, "ask": 100.04},
            # Cancel resting ASK once ref jumps +10 bps at ts=100 (cancel effective at 133ms)
            {"ts": 100, "kind": "ref", "venue": "trade_xyz", "bid": 100.10, "ask": 100.14},
            # Buy trade arrives at ts=200 hitting local ASK -> should NOT fill G3 because ASK was canceled!
            {"ts": 200, "kind": "trade", "side": "BUY", "price": 100.04, "size": 20.0},
            {"ts": 500, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.04, 10.0)]},
        ]
        res_oracle = rwa.simulate_rwa_cycle_arm(events_with_ref, 0.01, g3)
        self.assertTrue(res_oracle["ref_available"])
        self.assertEqual(res_oracle["fills"], 0)

        # Without ref events, G3 runs cleanly with ref_available=False and fills normally
        events_no_ref = [e for e in events_with_ref if e["kind"] != "ref"]
        res_no_ref = rwa.simulate_rwa_cycle_arm(events_no_ref, 0.01, g3)
        self.assertFalse(res_no_ref["ref_available"])
        self.assertGreater(res_no_ref["fills"], 0)

    def test_g4_quiet_regime_blocks_high_vol_entries_but_allows_exits(self) -> None:
        g4 = rwa.build_arms("arcus", maker_fee_bps=0.0, taker_fee_bps=2.25, arm_names=["G4_quiet_regime_only"])[0]
        # Start quiet -> get filled on BID -> then vol spikes (> 1.2 bps) -> opening BID blocked,
        # but reducing ASK exit is still placed and fills passively!
        events = [
            {"ts": 0, "kind": "book", "bids": [(100.00, 2.0)], "asks": [(100.04, 2.0)]},
            {"ts": 100, "kind": "trade", "side": "SELL", "price": 99.99, "size": 5.0},
            # Sharp price jump triggers high realized sigma_1s > 1.2 bps
            {"ts": 350, "kind": "book", "bids": [(100.10, 2.0)], "asks": [(100.12, 2.0)]},
            # Next book tick after cancel_delay_ms (33ms) places reducing ASK (active at 400+36=436ms)
            {"ts": 400, "kind": "book", "bids": [(100.10, 2.0)], "asks": [(100.12, 2.0)]},
            # Both BUY and SELL trades occur after vol spike:
            # BUY @ 100.12 MUST close existing long (reducing ASK permitted!),
            # SELL @ 100.09 must NOT open new long (BID canceled/blocked by quiet gate).
            {"ts": 500, "kind": "trade", "side": "BUY", "price": 100.12, "size": 10.0},
            {"ts": 600, "kind": "trade", "side": "SELL", "price": 100.09, "size": 10.0},
            {"ts": 1_500, "kind": "book", "bids": [(100.10, 2.0)], "asks": [(100.12, 2.0)]},
        ]
        res = rwa.simulate_rwa_cycle_arm(events, 0.01, g4)
        self.assertTrue(res["terminal_flat"])
        self.assertEqual(res["maker_fills"], 2)  # 1 initial BID entry + 1 reducing ASK exit
        self.assertEqual(res["taker_exits"], 0)  # exited passively, no IOC needed
        self.assertGreater(res["high_vol_ticks"], 0)

    def test_fee_evidence_whitelist_and_aggregation(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            fee_path = root / "fee.json"
            fee_path.write_text(
                json.dumps(
                    {
                        "venues": {
                            "arcus": {
                                "account_fee_status": "public_tier0_schedule_only",
                                "public_tier0_schedule_summary": {
                                    "tier0_fee_profiles": [
                                        {"maker_fee_bps": "0", "taker_fee_bps": "2.25"},
                                    ]
                                },
                            },
                            "lighter": {
                                "account_fee_status": "public_schedule_only",
                                "public_schedule_maker_fee_bps": "0",
                                "public_schedule_taker_fee_bps": "0",
                            },
                        }
                    }
                ),
                encoding="utf-8",
            )
            wl_path = root / "whitelist.json"
            wl_path.write_text(
                json.dumps(
                    {
                        "targets": [
                            {"venue": "arcus", "symbol": "DRAM"},
                            {"venue": "lighter_robinhood", "symbol": "NVDA"},
                        ]
                    }
                ),
                encoding="utf-8",
            )
            profiles = vf.load_fee_profiles_from_evidence(fee_path)
            targets = rwa.build_default_targets(profiles)
            filtered = rwa.filter_targets_by_whitelist(targets, rwa.load_whitelist_filter(wl_path))
            self.assertEqual([(t.venue, t.symbol) for t in filtered], [("arcus", "DRAM"), ("lighter_robinhood", "NVDA")])
            self.assertEqual(filtered[0].settlement, "USDC")
            self.assertEqual(filtered[0].taker_fee_bps, 2.25)
            self.assertEqual(filtered[1].settlement, "USDG")
            self.assertEqual(filtered[1].account_fee_status, "public_schedule_only")

        # Verify aggregate_arm_summary separates (arm, venue) and (arm, venue, symbol)
        fake_rows = [
            {
                "win": "w23",
                "arm": "S0_baseline_touch",
                "venue": "arcus",
                "symbol": "NVDA",
                "settlement": "USDC",
                "account_fee_status": "public_tier0_schedule_only",
                "cont": True,
                "cyc": 2,
                "fills": 4,
                "spr": 0.4,
                "drft": -0.1,
                "pex": 0.2,
                "tslp": -0.05,
                "fee_usd": 0.045,
                "ntl": 1000.0,
                "maker_volume_usd": 1800.0,
                "taker_volume_usd": 200.0,
                "maker_fee_bps": 0.0,
                "taker_fee_bps": 2.25,
                "rebate_bps": 0.0,
                "markout_1s_bps": 1.0,
                "markout_1s_den": 1000.0,
                "markout_5s_bps": 0.5,
                "markout_5s_den": 1000.0,
                "markout_10s_bps": None,
                "markout_10s_den": 0.0,
                "high_vol_ticks": 3,
            },
            {
                "win": "w23",
                "arm": "S0_baseline_touch",
                "venue": "lighter_robinhood",
                "symbol": "NVDA",
                "settlement": "USDG",
                "account_fee_status": "public_tier0_unverified",
                "cont": True,
                "cyc": 1,
                "fills": 2,
                "spr": 0.3,
                "drft": -0.1,
                "pex": 0.1,
                "tslp": 0.0,
                "fee_usd": 0.0,
                "ntl": 500.0,
                "maker_volume_usd": 1000.0,
                "taker_volume_usd": 0.0,
                "maker_fee_bps": 0.0,
                "taker_fee_bps": 0.0,
                "rebate_bps": 0.0,
                "markout_1s_bps": 0.5,
                "markout_1s_den": 500.0,
                "markout_5s_bps": None,
                "markout_5s_den": 0.0,
                "markout_10s_bps": None,
                "markout_10s_den": 0.0,
                "high_vol_ticks": 1,
            },
        ]
        summary = rwa.aggregate_arm_summary(fake_rows)
        self.assertIn("S0_baseline_touch|arcus", summary["by_arm_venue"])
        self.assertIn("S0_baseline_touch|lighter_robinhood", summary["by_arm_venue"])
        self.assertEqual(summary["by_arm_venue"]["S0_baseline_touch|arcus"]["settlement"], "USDC")
        self.assertEqual(summary["by_arm_venue"]["S0_baseline_touch|lighter_robinhood"]["settlement"], "USDG")
        self.assertAlmostEqual(summary["by_arm_venue"]["S0_baseline_touch|arcus"]["gross_ex_fee_bps"], 4.5, places=4)

    def test_verified_fee_profiles_precedence_and_standard_zero_scenario_toggle(self) -> None:
        # 1. Verified profiles MUST take precedence over tier0 fallbacks for arcus, lighter, lighter_robinhood.
        profiles_with_both = [
            vf.VenueFeeProfile(
                key="arcus_rwa_tier0",
                venue="arcus",
                scope="tier0",
                maker_fee_bps=0.0,
                taker_fee_bps=2.25,
                account_fee_status="public_tier0_schedule_only",
            ),
            vf.VenueFeeProfile(
                key="arcus_rwa_verified",
                venue="arcus",
                scope="verified",
                maker_fee_bps=0.0,
                taker_fee_bps=2.1375,
                account_fee_status="verified_account_tier",
                eligible_verified=True,
            ),
            vf.VenueFeeProfile(
                key="lighter_public_tier0",
                venue="lighter",
                scope="tier0",
                maker_fee_bps=0.0,
                taker_fee_bps=0.0,
                account_fee_status="public_schedule_only",
            ),
            vf.VenueFeeProfile(
                key="lighter_verified",
                venue="lighter",
                scope="verified",
                maker_fee_bps=0.4,
                taker_fee_bps=2.8,
                account_fee_status="verified_account_tier",
                eligible_verified=True,
            ),
            vf.VenueFeeProfile(
                key="lighter_robinhood_public_tier0",
                venue="lighter_robinhood",
                scope="tier0",
                maker_fee_bps=0.0,
                taker_fee_bps=0.0,
                account_fee_status="public_schedule_only",
            ),
            vf.VenueFeeProfile(
                key="lighter_robinhood_verified",
                venue="lighter_robinhood",
                scope="verified",
                maker_fee_bps=0.0,
                taker_fee_bps=0.0,
                account_fee_status="verified_account_tier",
                eligible_verified=True,
            ),
        ]
        verified_targets = rwa.build_default_targets(profiles_with_both)
        by_v = {t.venue: t for t in verified_targets}
        self.assertAlmostEqual(by_v["arcus"].taker_fee_bps, 2.1375, places=6)
        self.assertEqual(by_v["arcus"].account_fee_status, "verified_account_tier")
        self.assertAlmostEqual(by_v["lighter"].maker_fee_bps, 0.4, places=6)
        self.assertAlmostEqual(by_v["lighter"].taker_fee_bps, 2.8, places=6)
        self.assertEqual(by_v["lighter"].account_fee_status, "verified_account_tier")
        self.assertAlmostEqual(by_v["lighter_robinhood"].maker_fee_bps, 0.0, places=6)
        self.assertAlmostEqual(by_v["lighter_robinhood"].taker_fee_bps, 0.0, places=6)
        self.assertEqual(by_v["lighter_robinhood"].account_fee_status, "verified_account_tier")

        # 2. Explicit counterfactual scenario overrides Lighter venues to 0/0 with explicit provenance label.
        scenario_targets = rwa.build_default_targets(profiles_with_both, assume_lighter_standard_zero=True)
        sc_by_v = {t.venue: t for t in scenario_targets}
        self.assertAlmostEqual(sc_by_v["lighter"].maker_fee_bps, 0.0, places=6)
        self.assertAlmostEqual(sc_by_v["lighter"].taker_fee_bps, 0.0, places=6)
        self.assertEqual(sc_by_v["lighter"].account_fee_status, "lighter_standard_zero_scenario")
        self.assertAlmostEqual(sc_by_v["lighter_robinhood"].maker_fee_bps, 0.0, places=6)
        self.assertAlmostEqual(sc_by_v["lighter_robinhood"].taker_fee_bps, 0.0, places=6)
        self.assertEqual(sc_by_v["lighter_robinhood"].account_fee_status, "lighter_standard_zero_scenario")
        # Arcus stays verified
        self.assertAlmostEqual(sc_by_v["arcus"].taker_fee_bps, 2.1375, places=6)
        self.assertEqual(sc_by_v["arcus"].account_fee_status, "verified_account_tier")

        # 3. Also test loading a synthetic verified evidence JSON file end-to-end.
        with tempfile.TemporaryDirectory() as tmp:
            ev_path = Path(tmp) / "verified_fee.json"
            ev_path.write_text(
                json.dumps(
                    {
                        "venues": {
                            "arcus": {
                                "account_fee_status": "verified_account_tier",
                                "rwa_maker_fee_bps": "0",
                                "rwa_taker_fee_bps": "2.1375",
                            },
                            "lighter": {
                                "account_fee_status": "verified_account_tier",
                                "maker_fee_bps": "0.4",
                                "taker_fee_bps": "2.8",
                            },
                            "lighter_robinhood": {
                                "account_fee_status": "verified_account_tier",
                                "maker_fee_bps": "0",
                                "taker_fee_bps": "0",
                            },
                        }
                    }
                ),
                encoding="utf-8",
            )
            loaded = vf.load_fee_profiles_from_evidence(ev_path)
            loaded_targets = rwa.build_default_targets(loaded)
            lt_by_v = {t.venue: t for t in loaded_targets}
            self.assertEqual(lt_by_v["lighter"].maker_fee_bps, 0.4)
            self.assertEqual(lt_by_v["lighter"].taker_fee_bps, 2.8)
            self.assertEqual(lt_by_v["lighter"].account_fee_status, "verified_account_tier")
            self.assertEqual(lt_by_v["lighter_robinhood"].account_fee_status, "verified_account_tier")
            self.assertAlmostEqual(lt_by_v["arcus"].taker_fee_bps, 2.1375, places=6)

        # 4. Verify CLI exposes --assume-lighter-standard-zero flag cleanly.
        help_proc = subprocess.run(
            [sys.executable, str(SCRIPT_DIR / "replay_rwa_volume_farming.py"), "--help"],
            check=True,
            capture_output=True,
            text=True,
        )
        self.assertIn("--assume-lighter-standard-zero", help_proc.stdout)

    def test_cli_self_test(self) -> None:
        proc = subprocess.run(
            [sys.executable, str(SCRIPT_DIR / "replay_rwa_volume_farming.py"), "--self-test"],
            check=True,
            capture_output=True,
            text=True,
        )
        payload = json.loads(proc.stdout)
        self.assertEqual(payload["self_test"], "ok")
        self.assertEqual(len(payload["arms_verified"]), 6)
        self.assertEqual(payload["tradeless_fills"], 0)


if __name__ == "__main__":
    unittest.main()
