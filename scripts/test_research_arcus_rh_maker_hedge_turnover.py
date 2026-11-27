#!/usr/bin/env python3
"""Stdlib-only unit tests & 24-event P0 parity fixture for Method 2 turnover research."""

from __future__ import annotations

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPT_DIR = Path(__file__).resolve().parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

import research_arcus_rh_maker_hedge_turnover as mod  # noqa: E402


class TestArcusRhMakerHedgeTurnover(unittest.TestCase):
    def test_stdlib_only_top_level_imports_in_isolated_subprocess(self) -> None:
        blocker = "\n".join(
            [
                "import json, sys",
                "for m in ('numpy','duckdb','pyarrow','pandas','scipy'):",
                "    sys.modules[m] = None",
                "import research_arcus_rh_maker_hedge_turnover as m",
                "print(json.dumps(m.run_self_test()))",
            ]
        )
        env = dict(os.environ)
        env["PYTHONPATH"] = str(SCRIPT_DIR)
        proc = subprocess.run(
            [sys.executable, "-S", "-c", blocker],
            env=env,
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(proc.returncode, 0, msg=f"stderr={proc.stderr}\nstdout={proc.stdout}")
        payload = json.loads(proc.stdout)
        self.assertEqual(payload["self_test"], "ok")
        self.assertEqual(payload["fixture_events"], 24)

    def test_w28_exclusion_and_fee_schedule_resolution(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            for cid, udate in (
                ("tokyo-rwa-20261123T162953Z-w20", "2026-11-23"),
                ("tokyo-rwa-20261124T024453Z-w21", "2026-11-24"),
                ("tokyo-rwa-20261126T161454Z-w27", "2026-11-26"),
                ("tokyo-rwa-20261127T023000Z-w28", "2026-11-27"),
            ):
                p = (
                    root
                    / "parquet"
                    / "bbo_quotes"
                    / "collector_region=tokyo"
                    / "venue=arcus"
                    / f"utc_date={udate}"
                    / f"campaign_id={cid}"
                )
                p.mkdir(parents=True, exist_ok=True)
                (p / "part-0.parquet").write_bytes(b"")
            camps = mod.discover_w01_w27_campaigns(root, "rwa")
            self.assertEqual([c["window"] for c in camps], ["w20", "w21", "w27"])
            self.assertEqual([c["split"] for c in camps], ["selection_dev", "holdout", "w26_w27_oos"])
            self.assertEqual(
                mod.campaign_parquet_files(root, "bbo_quotes", "arcus", "2026-11-27", "tokyo-rwa-20261127T023000Z-w28"),
                [],
            )

        fees, _ = mod.resolve_benchmark_fees(None, None)
        self.assertAlmostEqual(fees["arcus_rwa_mk"], 0.0)
        self.assertAlmostEqual(fees["arcus_rwa_tk"], 2.1375)
        self.assertAlmostEqual(fees["rh_tk"], 0.0)
        self.assertAlmostEqual(fees["xyz_gm_tk"], 0.90)
        self.assertAlmostEqual(fees["xyz_std_tk"], 9.00)
        self.assertAlmostEqual(fees["entropy_tk"], 0.90)
        self.assertAlmostEqual(fees["bulk_rwa_tk"], 1.50)
        self.assertAlmostEqual(fees["bulk_crypto_tk"], 3.50)
        self.assertAlmostEqual(fees["backpack_tk"], 4.50)
        self.assertAlmostEqual(fees["extended_rwa_tk"], 0.90)

    def test_deterministic_24_event_synthetic_parity_fixture(self) -> None:
        cfg, events, res = mod.build_synthetic_parity_fixture()
        self.assertEqual(len(events), 24)
        self.assertEqual(cfg.name, "p0_synthetic_parity_24")
        self.assertEqual(res["arcus_fills"], 4)
        self.assertEqual(res["hedge_fills"], 4)
        self.assertEqual(res["orders_placed"], 8)
        self.assertEqual(res["cancels_sent"], 6)
        self.assertEqual(res["cancel_in_flight_fills"], 1)
        self.assertEqual(res["orphan_events"], 1)
        self.assertEqual(res["orphan_retries"], 4)
        self.assertEqual(res["depth_clipped_hedges"], 2)
        self.assertEqual(res["stale_hedge_blocks"], 2)
        self.assertEqual(res["terminal_pending_hedges"], 1)
        self.assertEqual(res["terminal_unhedged_orphans"], 1)
        self.assertAlmostEqual(res["arcus_volume_usd"], 298.9749, places=4)
        self.assertAlmostEqual(res["hedge_volume_usd"], 248.97, places=4)
        self.assertAlmostEqual(res["combined_volume_usd"], 547.9449, places=4)
        self.assertAlmostEqual(res["arcus_spread_capture"], 0.0497, places=4)
        self.assertAlmostEqual(res["arcus_adverse"], 0.001, places=4)
        self.assertAlmostEqual(res["rh_hedge_slip"], -0.0358, places=4)
        self.assertAlmostEqual(res["residual_drain"], 0.0, places=6)
        self.assertAlmostEqual(res["total_net_pnl_usd"], 0.0149, places=4)
        self.assertLess(res["conservation_error_usd"], 1e-8)

    def test_zero_delay_rejected_and_single_use_l1_depth_conservation(self) -> None:
        with self.assertRaises(ValueError):
            mod.ArcusHedgeTurnoverConfig(hedge_delay_ms=0)
        with self.assertRaises(ValueError):
            mod.ArcusHedgeTurnoverConfig(orphan_retry_delay_ms=0)

        t0 = 1_700_000_000_000
        cfg = mod.ArcusHedgeTurnoverConfig(
            name="l1_single_use",
            symbol="NVDA",
            tick_size=0.01,
            lot_size=0.01,
            clip_usd=100.0,
            max_inventory_usd=300.0,
            entry_edge_bps=-1.0,
            skew_bps_per_clip=0.0,
            nonlinear_skew_power=1.0,
            lead_beta=0.0,
            use_microprice=False,
            queue_ahead_fraction=0.0,
            submit_rtt_ms=10,
            cancel_ms=50,
            hedge_delay_ms=330,
            orphan_retry_delay_ms=330,
            warmup_ms=0,
            terminal_drain_ms=0,
            declared_end_ms=t0 + 2_000,
        )
        # Two consecutive 1.0-unit Arcus bid fills against a SINGLE hedge touch of 1.20 units:
        # First hedge takes 1.00 (leaving 0.20); second hedge takes 0.20 (NOT 1.00!) and triggers orphan retry!
        events = [
            {"ts": t0 + 0, "kind": "arcus_book", "bid": 99.99, "bsz": 5.0, "ask": 100.01, "asz": 5.0},
            {"ts": t0 + 0, "kind": "hedge_book", "bid": 99.99, "bsz": 1.20, "ask": 100.01, "asz": 5.0},
            {"ts": t0 + 20, "kind": "arcus_trade", "side": "SELL", "price": 99.99, "size": 1.0},
            {"ts": t0 + 40, "kind": "arcus_book", "bid": 99.99, "bsz": 5.0, "ask": 100.01, "asz": 5.0},
            {"ts": t0 + 60, "kind": "arcus_trade", "side": "SELL", "price": 99.99, "size": 1.0},
            {"ts": t0 + 400, "kind": "arcus_book", "bid": 99.99, "bsz": 5.0, "ask": 100.01, "asz": 5.0},
            {"ts": t0 + 800, "kind": "hedge_book", "bid": 99.99, "bsz": 2.00, "ask": 100.01, "asz": 5.0},
            {"ts": t0 + 1200, "kind": "arcus_book", "bid": 99.99, "bsz": 5.0, "ask": 100.01, "asz": 5.0},
        ]
        res = mod.simulate_arcus_hedge_turnover(events, cfg)
        self.assertEqual(res["arcus_fills"], 2)
        self.assertEqual(res["hedge_fills"], 3)
        self.assertEqual(res["orphan_events"], 1)
        self.assertEqual(res["orphan_retries"], 2)
        self.assertEqual(res["depth_clipped_hedges"], 2)
        self.assertLess(res["conservation_error_usd"], 1e-8)

    def test_unprinted_toxic_fill_stress_modes(self) -> None:
        t0 = 1_700_000_000_000
        # Quote Bid@99.99 active at t0+10, then book jumps to 99.97/99.99 (opposite_touch) and 99.95/99.97 (strict_through)!
        events_opp = [
            {"ts": t0 + 0, "kind": "arcus_book", "bid": 99.99, "bsz": 2.0, "ask": 100.01, "asz": 2.0},
            {"ts": t0 + 0, "kind": "hedge_book", "bid": 99.99, "bsz": 5.0, "ask": 100.01, "asz": 5.0},
            {"ts": t0 + 50, "kind": "arcus_book", "bid": 99.97, "bsz": 2.0, "ask": 99.99, "asz": 2.0},
            {"ts": t0 + 400, "kind": "hedge_book", "bid": 99.97, "bsz": 5.0, "ask": 99.99, "asz": 5.0},
        ]
        events_strict = [
            {"ts": t0 + 0, "kind": "arcus_book", "bid": 99.99, "bsz": 2.0, "ask": 100.01, "asz": 2.0},
            {"ts": t0 + 0, "kind": "hedge_book", "bid": 99.99, "bsz": 5.0, "ask": 100.01, "asz": 5.0},
            {"ts": t0 + 50, "kind": "arcus_book", "bid": 99.96, "bsz": 2.0, "ask": 99.98, "asz": 2.0},
            {"ts": t0 + 400, "kind": "hedge_book", "bid": 99.96, "bsz": 5.0, "ask": 99.98, "asz": 5.0},
        ]
        kw = dict(
            symbol="NVDA", tick_size=0.01, lot_size=0.01, clip_usd=100.0, max_inventory_usd=300.0,
            entry_edge_bps=-1.0, skew_bps_per_clip=0.0, nonlinear_skew_power=1.0, lead_beta=0.0,
            use_microprice=False, submit_rtt_ms=10, cancel_ms=200, hedge_delay_ms=330,
            orphan_retry_delay_ms=330, warmup_ms=0, terminal_drain_ms=0, declared_end_ms=t0 + 2_000,
        )
        r_none = mod.simulate_arcus_hedge_turnover(events_strict, mod.ArcusHedgeTurnoverConfig(toxic_mode="none", toxic_fill_frac=0.0, **kw))
        r_opp_on_strict = mod.simulate_arcus_hedge_turnover(events_opp, mod.ArcusHedgeTurnoverConfig(toxic_mode="strict_through", toxic_fill_frac=0.50, **kw))
        r_opp = mod.simulate_arcus_hedge_turnover(events_opp, mod.ArcusHedgeTurnoverConfig(toxic_mode="opposite_touch", toxic_fill_frac=0.25, **kw))
        r_strict = mod.simulate_arcus_hedge_turnover(events_strict, mod.ArcusHedgeTurnoverConfig(toxic_mode="strict_through", toxic_fill_frac=0.50, **kw))

        self.assertEqual(r_none["arcus_fills"], 0)
        self.assertEqual(r_opp_on_strict["unprinted_toxic_fills"], 0)
        self.assertEqual(r_opp["unprinted_toxic_fills"], 1)
        self.assertAlmostEqual(r_opp["arcus_volume_usd"], 25.0 * 0.9999, places=3)
        self.assertEqual(r_strict["unprinted_toxic_fills"], 1)
        self.assertAlmostEqual(r_strict["arcus_volume_usd"], 50.0 * 0.9999, places=3)

    def test_fx_clone_exact_parity_and_denominators(self) -> None:
        cfg, events, res0 = mod.build_synthetic_parity_fixture()
        for fx_bp in (1.0, 3.0):
            cfg_fx = dataclasses_replace(cfg, fx_drag_bps=fx_bp)
            direct = mod.simulate_arcus_hedge_turnover(events, cfg_fx)
            cloned = mod.clone_result_with_fx(res0, fx_bp)
            self.assertAlmostEqual(cloned["fx_drag"], direct["fx_drag"], places=6)
            self.assertAlmostEqual(cloned["total_net_pnl_usd"], direct["total_net_pnl_usd"], places=6)
            self.assertAlmostEqual(
                cloned["arcus_single_sided_cost_usd_per_m"],
                direct["arcus_single_sided_cost_usd_per_m"],
                places=4,
            )
            self.assertAlmostEqual(
                cloned["combined_two_leg_cost_usd_per_m"],
                direct["combined_two_leg_cost_usd_per_m"],
                places=4,
            )


def dataclasses_replace(cfg: mod.ArcusHedgeTurnoverConfig, **changes: object) -> mod.ArcusHedgeTurnoverConfig:
    import dataclasses

    return dataclasses.replace(cfg, **changes)  # type: ignore[arg-type]


if __name__ == "__main__":
    unittest.main()
