#!/usr/bin/env python3
"""Stdlib-only unit tests for `scripts/research_track03_patient_profit_frontier.py`."""

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

import research_track03_patient_profit_frontier as pf  # noqa: E402


class Track03PatientProfitFrontierTests(unittest.TestCase):
    def test_isolated_stdlib_top_level_and_cli_self_test(self) -> None:
        probe = subprocess.run(
            [
                sys.executable,
                "-S",
                "-c",
                (
                    f"import sys\nsys.path.insert(0, {str(SCRIPT_DIR)!r})\n"
                    "for b in ('numpy', 'duckdb', 'pyarrow', 'pandas', 'scipy'):\n"
                    "    sys.modules[b] = None\n"
                    "import research_track03_patient_profit_frontier\n"
                ),
            ],
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertEqual(probe.returncode, 0, probe.stderr or probe.stdout)

        proc = subprocess.run(
            [sys.executable, "-S", str(SCRIPT_DIR / "research_track03_patient_profit_frontier.py"), "--self-test"],
            check=True,
            capture_output=True,
            text=True,
        )
        payload = json.loads(proc.stdout)
        self.assertEqual(payload["self_test"], "ok")
        self.assertEqual(payload["arms_count"], 12)
        self.assertGreater(payload["p1_net_usd"], payload["s4_net_usd"])
        self.assertGreater(payload["p1_avg_hold_ms"], 15_000.0)

    def test_venue_profiles_fee_tiers_and_breakeven_rebate(self) -> None:
        # 1. Without fee evidence, Arcus RWA defaults to public Tier 0 (0.0 / 2.25 bps).
        targets_no_ev, prov = pf.build_frontier_targets()
        self.assertTrue(prov["lighter_standard_scenario"])
        by_vp_no_ev: dict[str, list[pf.FrontierTarget]] = {}
        for t in targets_no_ev:
            by_vp_no_ev.setdefault(t.venue_profile, []).append(t)
        self.assertEqual((by_vp_no_ev["arcus"][0].maker_fee_bps, by_vp_no_ev["arcus"][0].taker_fee_bps), (0.0, 2.25))

        # 2. With OPT-01 verified fee evidence, Arcus RWA resolves to 0.0 / 2.1375 bps,
        #    Lighter Prem to 0.40 / 2.80 bps, Lighter Std to 0.0 / 0.0 bps, LighterRH to 0.0 / 0.0 bps.
        with tempfile.TemporaryDirectory() as tmp:
            ev_path = Path(tmp) / "fee_ev.json"
            ev_path.write_text(
                json.dumps(
                    {
                        "schema_version": "aleph_account_fee_evidence_v1",
                        "venues": {
                            "lighter_robinhood": {
                                "account_fee_status": "verified_account_tier",
                                "maker_fee_bps": 0.0,
                                "taker_fee_bps": 0.0,
                            },
                            "lighter": {
                                "account_fee_status": "verified_account_tier",
                                "maker_fee_bps": 0.40,
                                "taker_fee_bps": 2.80,
                            },
                            "arcus": {
                                "account_fee_status": "verified_account_tier",
                                "crypto_maker_fee_bps": 1.425,
                                "crypto_taker_fee_bps": 4.275,
                                "rwa_maker_fee_bps": 0.0,
                                "rwa_taker_fee_bps": 2.1375,
                            },
                        },
                    }
                ),
                encoding="utf-8",
            )
            targets, _ = pf.build_frontier_targets(fee_evidence_path=ev_path)
        by_vp: dict[str, list[pf.FrontierTarget]] = {}
        for t in targets:
            by_vp.setdefault(t.venue_profile, []).append(t)

        self.assertEqual(len(by_vp["lighter_robinhood"]), 18)
        self.assertEqual(len(by_vp["lighter_std"]), 19)
        self.assertEqual(len(by_vp["lighter_prem"]), 19)
        self.assertEqual(len(by_vp["arcus"]), 17)  # RWA only
        self.assertEqual({t.domain for t in by_vp["arcus"]}, {"rwa"})

        self.assertEqual((by_vp["lighter_robinhood"][0].maker_fee_bps, by_vp["lighter_robinhood"][0].taker_fee_bps), (0.0, 0.0))
        self.assertEqual((by_vp["lighter_std"][0].maker_fee_bps, by_vp["lighter_std"][0].taker_fee_bps), (0.0, 0.0))
        self.assertEqual((by_vp["lighter_prem"][0].maker_fee_bps, by_vp["lighter_prem"][0].taker_fee_bps), (0.40, 2.80))
        self.assertEqual((by_vp["arcus"][0].maker_fee_bps, by_vp["arcus"][0].taker_fee_bps), (0.0, 2.1375))
        # Arcus RWA must not use arcus as its own reference venue!
        self.assertNotIn("arcus", by_vp["arcus"][0].ref_venues)

        be_clamp, be_signed = pf.compute_breakeven_rebate_bps(-0.20, 0.25, 0.40, 2.80)
        # net_cost = 0.40 * 1.75 + 2.80 * 0.25 - (-0.20) = 0.70 + 0.70 + 0.20 = 1.60 bp -> rebate = 1.60 / 1.75
        self.assertAlmostEqual(be_clamp, 1.60 / 1.75, places=5)
        self.assertAlmostEqual(be_signed, 1.60 / 1.75, places=5)

        be_neg_clamp, be_neg_signed = pf.compute_breakeven_rebate_bps(+0.50, 0.0, 0.0, 0.0)
        self.assertEqual(be_neg_clamp, 0.0)
        self.assertAlmostEqual(be_neg_signed, -0.25, places=6)

    def test_w27_campaign_discovery_strictly_excludes_w28(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "parquet" / "bbo_quotes" / "collector_region=tokyo" / "venue=lighter_robinhood"
            for d, cid in (
                ("2026-11-23", "tokyo-rwa-20261123T162953Z-w20"),
                ("2026-11-25", "tokyo-rwa-20261125T194453Z-w25"),
                ("2026-11-26", "tokyo-rwa-20261126T055953Z-w26"),
                ("2026-11-26", "tokyo-rwa-20261126T161454Z-w27"),
                ("2026-11-27", "tokyo-rwa-20261127T023000Z-w28"),
            ):
                (root / f"utc_date={d}" / f"campaign_id={cid}").mkdir(parents=True, exist_ok=True)

            all_wins = pf.discover_campaigns_w27(Path(tmp), "all")
            self.assertEqual(sorted(all_wins.keys()), ["w20", "w25", "w26", "w27"])
            self.assertNotIn("w28", all_wins)
            oos_wins = pf.discover_campaigns_w27(Path(tmp), "w26_w27_oos")
            self.assertEqual(sorted(oos_wins.keys()), ["w26", "w27"])

            with self.assertRaises(ValueError):
                pf.assert_no_w28_leak(["w26", "w28"], [])
            with self.assertRaises(ValueError):
                pf.assert_no_w28_leak(["w26"], [{"win": "w26", "campaign_id": "tokyo-rwa-x-w28"}])

    def test_frontier_aggregation_and_multiple_testing(self) -> None:
        # Verify synthetic multi-window aggregation computes realized vs mtm, breakeven rebate, and DSR/PBO.
        rows = []
        for idx in range(1, 9):
            win = f"w{idx:02d}"
            for arm, net_val, s_val in (
                ("S4_speedbump_combined", -2.0 + 0.1 * idx, 0.30),
                ("W2_wide_15bp_180s", 1.5 + 0.2 * idx, 0.05),
            ):
                ntl = 100_000.0
                tvol = ntl * s_val
                mvol = 2.0 * ntl - tvol
                rows.append(
                    {
                        "win": win,
                        "campaign_id": f"tokyo-rwa-x-{win}",
                        "domain": "rwa",
                        "venue_profile": "lighter_robinhood",
                        "venue": "lighter_robinhood",
                        "symbol": "AAPL",
                        "settlement": "USDG",
                        "maker_fee_bps": 0.0,
                        "taker_fee_bps": 0.0,
                        "rebate_bps": 0.0,
                        "account_fee_status": "verified_account_tier",
                        "arm": arm,
                        "latency_label": "o200_c300_e330",
                        "order_delay_ms": 200,
                        "cancel_delay_ms": 300,
                        "exit_delay_ms": 330,
                        "cont": True,
                        "continuity": {"hours": 10.25},
                        "cyc": 10,
                        "orders_placed": 50,
                        "fills": 20,
                        "maker_fills": 18,
                        "taker_exits": 2,
                        "spr": net_val,
                        "drft": 0.0,
                        "pex": 0.0,
                        "tslp": 0.0,
                        "unrealized_mtm_usd": 0.0,
                        "mtm_gross_usd": net_val,
                        "fee_usd": 0.0,
                        "mtm_net_usd": net_val,
                        "ntl": ntl,
                        "maker_volume_usd": mvol,
                        "taker_volume_usd": tvol,
                        "terminal_residual_notional_usd": 0.0,
                        "hold_weighted_sum_ms": ntl * 45_000.0,
                        "max_hold_observed_ms": 90_000,
                        "depth_clipped_exits": 1,
                        "stale_book_cancels": 0,
                        "stale_exit_blocks": 2,
                        "toxicity_pauses": 0,
                        "markout_1s_bps": 0.25,
                        "markout_1s_den": mvol,
                        "markout_5s_bps": None,
                        "markout_5s_den": 0.0,
                        "markout_10s_bps": None,
                        "markout_10s_den": 0.0,
                        "cancel_in_flight_fills": 1,
                        "cancel_in_flight_notional_usd": 1000.0,
                    }
                )
        cohorts = pf.summarize_frontier_cohorts(rows)
        w2 = cohorts["top_tier_7_rwa"]["W2_wide_15bp_180s|lighter_robinhood|o200_c300_e330"]
        self.assertEqual(w2["windows_covered"], 8)
        self.assertEqual(w2["stale_exit_blocks"], 16)
        self.assertAlmostEqual(w2["avg_hold_s"], 45.0, places=3)
        self.assertEqual(w2["breakeven_rebate_bps"], 0.0)
        self.assertLess(w2["zero_rebate_usd_per_m"], 0.0)

        mt = pf.compute_frontier_multiple_testing(rows, cohort_filter=["AAPL"])
        self.assertIn("lighter_robinhood|o200_c300_e330", mt)
        self.assertEqual(mt["lighter_robinhood|o200_c300_e330"]["windows_aligned"], 8)
        self.assertIsNotNone(mt["lighter_robinhood|o200_c300_e330"]["cscv_pbo"])


if __name__ == "__main__":
    unittest.main()
