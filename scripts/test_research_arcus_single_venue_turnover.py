#!/usr/bin/env python3
"""Unit tests for Method 1 Single-Venue Arcus RWA MM Benchmark (`python3 -S -m unittest`)."""

from __future__ import annotations

import builtins
import inspect
import json
import math
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

import research_arcus_single_venue_turnover as m1  # noqa: E402
from research_track03_core4_hold_ablation import simulate_core4_arm  # noqa: E402
from research_track03_full_universe_speedbump_mm import prepare_replay_stream  # noqa: E402
from research_track03_patient_profit_frontier import assert_no_w28_leak  # noqa: E402


class TestArcusSingleVenueTurnover(unittest.TestCase):
    def test_isolated_stdlib_top_level_imports(self) -> None:
        real_import = builtins.__import__
        blocked = {"duckdb", "numpy", "pandas", "pyarrow", "scipy"}

        def _guarded_import(name, globals=None, locals=None, fromlist=(), level=0):
            if name.split(".")[0] in blocked:
                raise AssertionError(f"Forbidden non-stdlib top-level import: {name}")
            return real_import(name, globals, locals, fromlist, level)

        saved = sys.modules.pop("research_arcus_single_venue_turnover", None)
        builtins.__import__ = _guarded_import
        try:
            mod = __import__("research_arcus_single_venue_turnover")
            self.assertEqual(mod.run_self_test()["self_test"], "ok")
        finally:
            builtins.__import__ = real_import
            if saved is not None:
                sys.modules["research_arcus_single_venue_turnover"] = saved

    def test_strict_w28_exclusion_in_all_duckdb_sql(self) -> None:
        src = inspect.getsource(m1)
        self.assertIn("NOT regexp_matches(campaign_id, '-w28$')", src)
        for builder in (m1._build_bbo_stats_sql, m1._build_bbo_bucket_sql, m1._build_trades_sql):
            if builder is m1._build_bbo_bucket_sql:
                sql = builder("['a.parquet']", "'AAPL-USD'", 250)
            else:
                sql = builder("['a.parquet']", "'AAPL-USD'")
            self.assertIn("NOT regexp_matches(campaign_id, '-w28$')", sql)
            self.assertIn("hive_partitioning=true", sql)
        with self.assertRaises(ValueError):
            assert_no_w28_leak(["w28"], [])
        with self.assertRaises(ValueError):
            assert_no_w28_leak(["w27"], [{"win": "w27", "campaign_id": "tokyo-rwa-20261127T000000Z-w28"}])

    def test_nonzero_latency_and_l1_depth_conservation_and_declared_end_cutoff(self) -> None:
        with self.assertRaises(ValueError):
            m1.build_arm_config("S4_hold15s", order_delay_ms=0, cancel_delay_ms=150, exit_delay_ms=150)
        with self.assertRaises(ValueError):
            m1.build_arm_config("S4_hold15s", order_delay_ms=100, cancel_delay_ms=0, exit_delay_ms=150)
        with self.assertRaises(ValueError):
            m1.build_arm_config("S4_hold15s", order_delay_ms=100, cancel_delay_ms=150, exit_delay_ms=0)

        # Shallow L1 bid depth (0.50) clips 15s IOC exit of 8.0 units -> explicit residual + non-zero exit delay
        events = [
            {"ts": 0, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.06, 10.0)]},
            {"ts": 0, "kind": "ref", "venue": "lighter_robinhood", "bid": 100.00, "ask": 100.06},
            {"ts": 300, "kind": "trade", "side": "SELL", "price": 100.00, "size": 8.0},
            {"ts": 4_000, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.06, 10.0)]},
            {"ts": 8_000, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.06, 10.0)]},
            {"ts": 12_000, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.06, 10.0)]},
            {"ts": 15_300, "kind": "book", "bids": [(99.98, 0.5)], "asks": [(100.04, 5.0)]},
            {"ts": 15_500, "kind": "book", "bids": [(99.98, 0.5)], "asks": [(100.04, 5.0)]},
            # Post declared_end_ms ref tick must NOT affect valuation
            {"ts": 99_000, "kind": "ref", "venue": "trade_xyz", "bid": 120.00, "ask": 120.06},
        ]
        stream = prepare_replay_stream(events)
        arm = m1.build_arm_config("S4_hold15s", order_delay_ms=100, cancel_delay_ms=150, exit_delay_ms=150)
        res = simulate_core4_arm(stream, 0.01, arm, declared_end_ms=15_500)
        self.assertGreaterEqual(res["depth_clipped_exits"], 1)
        self.assertGreater(res["terminal_residual_notional_usd"], 0.0)
        self.assertFalse(res["terminal_flat"])
        self.assertAlmostEqual(res["_exact_mtm_gross_usd"], res["_exact_cash_ex_fee_usd"], places=6)

    def test_dual_fee_schedules_and_dual_volume_denominators(self) -> None:
        events = [
            {"ts": 0, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.06, 10.0)]},
            {"ts": 0, "kind": "ref", "venue": "trade_xyz", "bid": 100.00, "ask": 100.06},
            {"ts": 300, "kind": "trade", "side": "SELL", "price": 100.00, "size": 8.0},
            {"ts": 4_000, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.06, 10.0)]},
            {"ts": 8_000, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.06, 10.0)]},
            {"ts": 12_000, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.06, 10.0)]},
            {"ts": 15_500, "kind": "book", "bids": [(99.98, 5.0)], "asks": [(100.04, 5.0)]},
            {"ts": 16_000, "kind": "book", "bids": [(99.98, 5.0)], "asks": [(100.04, 5.0)]},
        ]
        stream = prepare_replay_stream(events)
        arm_ver = m1.build_arm_config("S4_hold15s", order_delay_ms=100, cancel_delay_ms=150, exit_delay_ms=150, taker_fee_bps=2.1375)
        arm_l0 = m1.build_arm_config("S4_hold15s", order_delay_ms=100, cancel_delay_ms=150, exit_delay_ms=150, taker_fee_bps=2.25)
        r_ver = simulate_core4_arm(stream, 0.01, arm_ver, declared_end_ms=16_000)
        r_l0 = simulate_core4_arm(stream, 0.01, arm_l0, declared_end_ms=16_000)

        self.assertAlmostEqual(r_ver["maker_volume_usd"], r_l0["maker_volume_usd"], places=6)
        self.assertAlmostEqual(r_ver["taker_volume_usd"], r_l0["taker_volume_usd"], places=6)
        self.assertAlmostEqual(r_ver["mtm_gross_usd"], r_l0["mtm_gross_usd"], places=6)

        c_l0 = m1.compute_dual_volume_costs(
            realized_gross_usd=float(r_ver["gross_usd"]),
            mtm_gross_usd=float(r_ver["mtm_gross_usd"]),
            ntl=float(r_ver["ntl"]),
            maker_volume_usd=float(r_ver["maker_volume_usd"]),
            taker_volume_usd=float(r_ver["taker_volume_usd"]),
            maker_fee_bps=0.0,
            taker_fee_bps=2.25,
        )
        self.assertAlmostEqual(c_l0["mtm_net_usd"], r_l0["mtm_net_usd"], places=5)
        self.assertAlmostEqual(
            c_l0["single_sided_exec_mtm_usd_per_m"],
            2.0 * c_l0["double_sided_exec_mtm_usd_per_m"],
            places=3,
        )
        self.assertGreater(c_l0["double_sided_exec_mtm_usd_per_m"], 0.0)

    def test_toxic_book_cross_increases_fills_and_worsens_adverse_selection(self) -> None:
        # Ask quote rests at 100.06 (activated at ts=200), then book jumps up across 100.06 at ts=500 without a trade print -> unprinted toxic fill!
        events = [
            {"ts": 0, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.06, 10.0)]},
            {"ts": 0, "kind": "ref", "venue": "trade_xyz", "bid": 100.00, "ask": 100.06},
            {"ts": 200, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.06, 10.0)]},
            {"ts": 500, "kind": "book", "bids": [(100.12, 10.0)], "asks": [(100.18, 10.0)]},
            {"ts": 1_500, "kind": "book", "bids": [(100.12, 10.0)], "asks": [(100.18, 10.0)]},
        ]
        stream = prepare_replay_stream(events)
        arm_base = m1.build_arm_config("S4_hold600s_only", order_delay_ms=100, cancel_delay_ms=150, exit_delay_ms=150)
        arm_st50 = m1.build_arm_config(
            "S4_hold600s_only",
            order_delay_ms=100,
            cancel_delay_ms=150,
            exit_delay_ms=150,
            book_cross_mode="strict_through",
            book_cross_toxic_fill_frac=0.50,
        )
        r_base = simulate_core4_arm(stream, 0.01, arm_base, declared_end_ms=1_500)
        r_st50 = simulate_core4_arm(stream, 0.01, arm_st50, declared_end_ms=1_500)
        self.assertEqual(r_base["unprinted_book_cross_fills"], 0)
        self.assertGreater(r_st50["unprinted_book_cross_fills"], 0)
        self.assertLess(r_st50["mtm_net_usd"], r_base["mtm_net_usd"])

    def test_aggregation_and_fee_provenance_loader(self) -> None:
        specs = m1.build_benchmark_specs(compact=False)
        self.assertEqual(len({sp.spec_id for sp in specs}), len(specs))
        self.assertTrue(any(sp.arm_name == "W5" for sp in specs))
        self.assertTrue(any(sp.scenario_id == "opposite_touch_50" for sp in specs))
        self.assertTrue(any(sp.quote_usd == 500.0 for sp in specs))

        with tempfile.TemporaryDirectory() as tmp:
            ev_path = Path(tmp) / "fee.json"
            ev_path.write_text(
                json.dumps(
                    {
                        "schema_version": "test_v1",
                        "audited_at_utc": "2026-11-27T00:00:00Z",
                        "venues": {
                            "arcus": {
                                "account_fee_status": "verified_account_tier",
                                "rwa_maker_fee_bps": "0",
                                "rwa_taker_fee_bps": "2.1375",
                            }
                        },
                    }
                ),
                encoding="utf-8",
            )
            mk, tk, st, src, prov = m1.resolve_arcus_fee_provenance(ev_path)
            self.assertEqual(mk, 0.0)
            self.assertEqual(tk, 2.1375)
            self.assertEqual(st, "verified_account_tier")
            self.assertTrue(prov["fee_evidence_loaded"])


if __name__ == "__main__":
    unittest.main()
