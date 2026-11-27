#!/usr/bin/env python3
"""Stdlib-only unit tests for `scripts/research_track03_core4_hold_ablation.py`."""

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

import research_track03_core4_hold_ablation as ab  # noqa: E402
import research_track03_full_universe_speedbump_mm as sb  # noqa: E402


class Track03Core4HoldAblationTests(unittest.TestCase):
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
                    "import research_track03_core4_hold_ablation\n"
                ),
            ],
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertEqual(probe.returncode, 0, probe.stderr or probe.stdout)

        proc = subprocess.run(
            [sys.executable, "-S", str(SCRIPT_DIR / "research_track03_core4_hold_ablation.py"), "--self-test"],
            check=True,
            capture_output=True,
            text=True,
        )
        payload = json.loads(proc.stdout)
        self.assertEqual(payload["self_test"], "ok")
        self.assertEqual(payload["arms_count"], 9)
        self.assertLessEqual(payload["max_parity_diff"], 1e-12)
        self.assertGreater(payload["s4_hold600s_only_net_usd"], payload["s4_net_usd"])
        self.assertEqual(len(payload["prereg_plan_sha256"]), 64)

    def test_single_factor_hold_ladder_and_component_specs_locked(self) -> None:
        arms = {a.name: a for a in ab.build_core4_ablation_arms()}
        self.assertEqual(set(arms.keys()), set(ab.CORE4_ABLATION_ARMS_SPEC.keys()))

        # Pure hold ladder locks everything except max_hold_ms identically to S4_speedbump_combined.
        s4 = arms["S4_speedbump_combined"]
        expected_holds = {
            "S4_speedbump_combined": 15_000,
            "S4_hold60s": 60_000,
            "S4_hold180s": 180_000,
            "S4_hold300s": 300_000,
            "S4_hold600s_only": 600_000,
        }
        for name, hold_ms in expected_holds.items():
            arm = arms[name]
            self.assertEqual(arm.max_hold_ms, hold_ms)
            self.assertEqual(arm.reduce_only_penny_ticks, 0)
            self.assertEqual(arm.inventory_skew_bps_per_full_limit, 2.0)
            self.assertEqual(arm.min_entry_edge_bps, s4.min_entry_edge_bps)
            self.assertEqual(arm.latency_vol_spread_mult, s4.latency_vol_spread_mult)
            self.assertEqual(arm.cross_venue_fair_weight, s4.cross_venue_fair_weight)
            self.assertEqual(arm.toxicity_pause_ewma_bps, s4.toxicity_pause_ewma_bps)
            self.assertEqual(arm.latency_label, "o200_c300_e330")

        # Component attribution arms isolate penny=1 and skew=5.0 at 600s.
        self.assertEqual(arms["S4_hold600s_penny1"].reduce_only_penny_ticks, 1)
        self.assertEqual(arms["S4_hold600s_penny1"].inventory_skew_bps_per_full_limit, 2.0)
        self.assertEqual(arms["S4_hold600s_skew5"].reduce_only_penny_ticks, 0)
        self.assertEqual(arms["S4_hold600s_skew5"].inventory_skew_bps_per_full_limit, 5.0)
        self.assertEqual(arms["P3_patient_600s"].reduce_only_penny_ticks, 1)
        self.assertEqual(arms["P3_patient_600s"].inventory_skew_bps_per_full_limit, 5.0)
        self.assertEqual(arms["W5_nopenn_15bp_180s"].reduce_only_penny_ticks, 0)
        self.assertEqual(arms["W5_nopenn_15bp_180s"].min_entry_edge_bps, 1.50)

    def test_parity_with_speedbump_sim_plus_hold_quantiles_and_exposure(self) -> None:
        events = [
            {"ts": 0, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.06, 10.0)]},
            {"ts": 0, "kind": "ref", "venue": "arcus", "bid": 100.00, "ask": 100.06},
            {"ts": 300, "kind": "trade", "side": "SELL", "price": 100.00, "size": 8.0},
            {"ts": 4_000, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.06, 10.0)]},
            {"ts": 8_000, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.06, 10.0)]},
            {"ts": 12_000, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.06, 10.0)]},
            {"ts": 22_000, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.06, 10.0)]},
            {"ts": 25_000, "kind": "trade", "side": "BUY", "price": 100.06, "size": 8.0},
            {"ts": 29_000, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.06, 10.0)]},
        ]
        stream = sb.prepare_replay_stream(events)
        for arm in ab.build_core4_ablation_arms(arm_names=["S4_speedbump_combined", "S4_hold600s_only"]):
            r_new = ab.simulate_core4_arm(stream, 0.01, arm, declared_end_ms=29_000)
            r_ref = sb.simulate_prepared_arm(stream, 0.01, arm, declared_end_ms=29_000)
            for k in ("net", "realized_net_usd", "ntl", "maker_volume_usd", "taker_volume_usd", "stale_exit_blocks"):
                self.assertAlmostEqual(float(r_new[k]), float(r_ref[k]), places=12)
            self.assertGreater(r_new["max_gross_exposure_usd"], 150.0)
            self.assertIsNotNone(r_new["hold_p50_ms"])
            self.assertIsNotNone(r_new["hold_p95_ms"])
            self.assertIsNotNone(r_new["hold_p99_ms"])
            self.assertEqual(r_new["hold_max_ms"], r_new["max_hold_observed_ms"])

    def test_roadmap_ledger_accounting_zero_fill_windows_and_bootstrap(self) -> None:
        # Construct 8 aligned windows across Core 4 including 1 zero-fill window (w04) for S4_hold600s_only.
        rows = []
        for idx in range(1, 9):
            win = f"w{idx:02d}"
            for arm in ("S4_speedbump_combined", "S4_hold600s_only", "P3_patient_600s"):
                for sym in ab.CORE_4_SYMBOLS:
                    if arm == "S4_hold600s_only" and idx == 4:
                        exec_notl = 0.0
                        mvol = 0.0
                        tvol = 0.0
                        ntl = 0.0
                        net_usd = 0.0
                        fills = 0
                        hold_s = []
                        exp_tl = []
                    else:
                        ntl = 10_000.0
                        tvol = 4_000.0 if arm == "S4_speedbump_combined" else 0.0
                        mvol = 2.0 * ntl - tvol
                        exec_notl = mvol + tvol
                        net_usd = (-0.5 if arm == "S4_speedbump_combined" else (1.5 if sym == "META" else 0.4))
                        fills = 4
                        hold_s = [10_000, 20_000]
                        exp_tl = [(1000, 200.0), (5000, 0.0)]
                    rows.append(
                        {
                            "win": win,
                            "campaign_id": f"tokyo-rwa-x-{win}",
                            "domain": "rwa",
                            "venue": "lighter_robinhood",
                            "symbol": sym,
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
                            "cyc": 1 if fills else 0,
                            "orders_placed": 10,
                            "fills": fills,
                            "maker_fills": fills if tvol == 0 else 2,
                            "taker_exits": 0 if tvol == 0 else 2,
                            "spr": net_usd,
                            "drft": 0.0,
                            "pex": 0.0,
                            "tslp": 0.0,
                            "unrealized_mtm_usd": 0.0,
                            "mtm_gross_usd": net_usd,
                            "fee_usd": 0.0,
                            "realized_net_usd": net_usd,
                            "mtm_net_usd": net_usd,
                            "ntl": ntl,
                            "maker_volume_usd": mvol,
                            "taker_volume_usd": tvol,
                            "target_executed_fill_notional_usd": exec_notl,
                            "terminal_residual_notional_usd": 0.0,
                            "max_gross_exposure_usd": 200.0 if fills else 0.0,
                            "hold_weighted_sum_ms": ntl * 15_000.0,
                            "max_hold_observed_ms": 20_000 if fills else 0,
                            "depth_clipped_exits": 0,
                            "stale_book_cancels": 0,
                            "stale_exit_blocks": 0,
                            "toxicity_pauses": 0,
                            "markout_1s_bps": 0.2 if fills else None,
                            "markout_1s_den": mvol,
                            "markout_5s_bps": None,
                            "markout_5s_den": 0.0,
                            "markout_10s_bps": None,
                            "markout_10s_den": 0.0,
                            "cancel_in_flight_fills": 0,
                            "cancel_in_flight_notional_usd": 0.0,
                            "_hold_samples_ms": hold_s,
                            "_exposure_timeline": exp_tl,
                        }
                    )

        summary = ab.summarize_core4_split(rows)
        h600 = summary["core4"]["S4_hold600s_only"]
        # Zero-fill window w04 MUST be kept in windows_covered=8 with unit_cost=None.
        self.assertEqual(h600["windows_covered"], 8)
        self.assertEqual(h600["zero_fill_windows"], 1)
        self.assertEqual(h600["active_fill_windows"], 7)
        w04 = [w for w in h600["by_window"] if w["win"] == "w04"][0]
        self.assertIsNone(w04["ledger_mtm_usd_per_m"])
        self.assertIsNone(w04["historical_double_sided_mtm_usd_per_m"])

        # True ledger $/M = sum(-mtm_net_usd) / sum(target_executed_fill_notional_usd) * 1e6
        expected_ledger = (-h600["mtm_net_usd"] / h600["target_executed_fill_notional_usd"]) * 1e6
        self.assertAlmostEqual(h600["ledger_mtm_usd_per_m"], expected_ledger, places=3)
        self.assertEqual(h600["max_gross_exposure_usd"], 800.0)  # 4 symbols simultaneously at $200

        # Paired Politis-Romano bootstrap CI is deterministic and strictly positive improvement.
        boot = summary["paired_bootstrap_vs_s4"]["core4"]["S4_hold600s_only"]
        self.assertGreater(boot["delta_net_usd_per_win"]["ci95_low"], 0.0)
        self.assertLess(boot["delta_ledger_usd_per_m"]["ci95_high"], 0.0)

        # ex-META & contribution shares
        contrib = summary["symbol_contributions"]["S4_hold600s_only"]
        self.assertGreater(contrib["ex_meta_mtm_net_usd"], 0.0)
        self.assertAlmostEqual(contrib["symbols"]["META"]["pnl_share_of_core4"], 1.5 / 2.7, places=5)

    def test_fee_provenance_sha256_and_w28_exclusion_and_prereg_determinism(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            fee_path = Path(tmp) / "fee.json"
            fee_path.write_text(
                json.dumps(
                    {
                        "schema_version": "aleph_account_fee_evidence_v1",
                        "venues": {
                            "lighter_robinhood": {
                                "account_fee_status": "verified_account_tier",
                                "maker_fee_bps": 0.0,
                                "taker_fee_bps": 0.0,
                            }
                        },
                    }
                ),
                encoding="utf-8",
            )
            mk, tk, st, _, prov = ab.resolve_core4_rh_fee_provenance(fee_path)
            self.assertEqual((mk, tk, st), (0.0, 0.0, "verified_account_tier"))
            self.assertEqual(len(prov["fee_evidence_sha256"]), 64)

        p1 = ab.build_w28plus_prereg_plan(shadow_config_sha256="abc123")
        p2 = ab.build_w28plus_prereg_plan(shadow_config_sha256="abc123")
        self.assertEqual(p1["plan_sha256"], p2["plan_sha256"])
        self.assertEqual(len(p1["falsification_thresholds"]), 9)
        self.assertTrue(math.isfinite(p1["falsification_thresholds"]["6_forced_ioc_share"]["max_taker_exit_share_s"]))


if __name__ == "__main__":
    unittest.main()
