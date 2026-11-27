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
        self.assertEqual(p1["schema"], "aleph_tx_track03_w28plus_prereg_plan_v2")
        self.assertEqual(p1["plan_sha256"], p2["plan_sha256"])
        self.assertEqual(len(p1["falsification_thresholds"]), 12)
        self.assertIn("10_directional_symmetry", p1["falsification_thresholds"])
        self.assertIn("11_market_neutrality_beta", p1["falsification_thresholds"])
        self.assertIn("12_toxic_book_cross_robustness", p1["falsification_thresholds"])
        self.assertTrue(math.isfinite(p1["falsification_thresholds"]["6_forced_ioc_share"]["max_taker_exit_share_s"]))

    def test_unprinted_book_cross_toxic_fill_parity_and_single_trigger(self) -> None:
        # Resting bid at 100.00 is crossed (no trade print) when best ask drops to 99.96, then later to 99.95,
        # before recovering to 100.04 on exit print. Single-trigger ensures frac=0.50 fills once (not repeatedly).
        events = [
            {"ts": 0, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.06, 10.0)]},
            {"ts": 250, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.06, 10.0)]},
            {"ts": 500, "kind": "book", "bids": [(99.92, 10.0)], "asks": [(99.96, 10.0)]},
            {"ts": 800, "kind": "book", "bids": [(99.91, 10.0)], "asks": [(99.95, 10.0)]},
            {"ts": 2_000, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.04, 10.0)]},
            {"ts": 3_000, "kind": "trade", "side": "BUY", "price": 100.03, "size": 8.0},
            {"ts": 4_000, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.04, 10.0)]},
        ]
        stream = sb.prepare_replay_stream(events)
        base_arm = sb.SpeedbumpArmConfig(
            name="test_base",
            order_delay_ms=200,
            cancel_delay_ms=300,
            exit_delay_ms=330,
            min_entry_edge_bps=0.1,
            vol_spread_mult=0.0,
            quiet_regime_only=False,
            use_oracle_anchor=False,
        )
        r0 = ab.simulate_core4_arm(stream, 0.01, base_arm, declared_end_ms=4_000)
        self.assertEqual(r0["unprinted_book_cross_fills"], 0)
        self.assertEqual(r0["maker_fills"], 0)
        self.assertAlmostEqual(r0["underlying_return_bps"], ((100.02 / 100.03) - 1.0) * 1e4, places=4)

        for mode in ("strict_through", "opposite_touch", "level_evicted"):
            arm_50 = sb.SpeedbumpArmConfig(
                **{**base_arm.__dict__, "book_cross_toxic_fill_frac": 0.50, "book_cross_mode": mode}
            )
            r_c4 = ab.simulate_core4_arm(stream, 0.01, arm_50, declared_end_ms=4_000)
            r_sb = sb.simulate_prepared_arm(stream, 0.01, arm_50, declared_end_ms=4_000)
            self.assertEqual(r_c4["unprinted_book_cross_fills"], 1)
            self.assertAlmostEqual(r_c4["net"], r_sb["net"], places=10)
            self.assertAlmostEqual(r_c4["unprinted_book_cross_notional_usd"], 100.0, places=2)
            self.assertAlmostEqual(
                r_c4["unprinted_book_cross_notional_usd"],
                r_sb["unprinted_book_cross_notional_usd"],
                places=8,
            )

    def test_ols_beta_regression_and_regime_symmetry_math(self) -> None:
        # Exact linear relationship y = 2.0 + 3.0 * x + small noise -> beta ~ 3.0, p < 0.001
        xs = [-2.0, -1.0, 0.0, 1.0, 2.0, 3.0, -3.0, 4.0]
        ys = [2.0 + 3.0 * x + (0.01 if i % 2 == 0 else -0.01) for i, x in enumerate(xs)]
        reg = ab.compute_ols_beta_regression(xs, ys)
        self.assertEqual(reg["n"], 8)
        self.assertAlmostEqual(reg["alpha"], 2.0, places=2)
        self.assertAlmostEqual(reg["beta"], 3.0, places=2)
        self.assertGreater(reg["r_squared"], 0.999)
        self.assertLess(reg["p_beta"], 1e-4)

        # Pure orthogonal constant y -> beta = 0, r_squared = 0, p_beta = 1.0
        reg0 = ab.compute_ols_beta_regression([-1.0, 1.0, -2.0, 2.0], [5.0, 5.0, 5.0, 5.0])
        self.assertAlmostEqual(reg0["beta"], 0.0, places=9)
        self.assertEqual(reg0["p_beta"], 1.0)

        up = [
            {"mtm_net_usd": 4.0, "target_executed_fill_notional_usd": 100_000.0, "ret_bps": 50.0, "spr": 5.0, "drft": -1.0, "pex": 0.0, "tslp": 0.0},
            {"mtm_net_usd": 6.0, "target_executed_fill_notional_usd": 100_000.0, "ret_bps": 60.0, "spr": 7.0, "drft": -1.0, "pex": 0.0, "tslp": 0.0},
            {"mtm_net_usd": 5.0, "target_executed_fill_notional_usd": 100_000.0, "ret_bps": 55.0, "spr": 6.0, "drft": -1.0, "pex": 0.0, "tslp": 0.0},
            {"mtm_net_usd": 5.5, "target_executed_fill_notional_usd": 100_000.0, "ret_bps": 52.0, "spr": 6.5, "drft": -1.0, "pex": 0.0, "tslp": 0.0},
        ]
        dn = [
            {"mtm_net_usd": -2.0, "target_executed_fill_notional_usd": 100_000.0, "ret_bps": -40.0, "spr": 3.0, "drft": -5.0, "pex": 0.0, "tslp": 0.0},
            {"mtm_net_usd": -1.0, "target_executed_fill_notional_usd": 100_000.0, "ret_bps": -30.0, "spr": 4.0, "drft": -5.0, "pex": 0.0, "tslp": 0.0},
            {"mtm_net_usd": -1.5, "target_executed_fill_notional_usd": 100_000.0, "ret_bps": -35.0, "spr": 3.5, "drft": -5.0, "pex": 0.0, "tslp": 0.0},
            {"mtm_net_usd": -1.2, "target_executed_fill_notional_usd": 100_000.0, "ret_bps": -32.0, "spr": 3.8, "drft": -5.0, "pex": 0.0, "tslp": 0.0},
        ]
        sym = ab.compute_regime_symmetry(up, dn)
        self.assertEqual(sym["up_windows"]["n_units"], 4)
        self.assertEqual(sym["down_windows"]["n_units"], 4)
        self.assertGreater(sym["welch_t_up_minus_down_mean_usd"], 5.0)
        self.assertLess(sym["welch_p_two_sided"], 0.01)


if __name__ == "__main__":
    unittest.main()
