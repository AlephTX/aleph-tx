#!/usr/bin/env python3
"""Stdlib-only unit tests for scripts/research_track02_b0_settleability_audit.py."""

from __future__ import annotations

from datetime import datetime, timezone
import os
from pathlib import Path
import subprocess
import sys
import unittest

SCRIPT_DIR = Path(__file__).resolve().parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

import research_track02_b0_settleability_audit as mod  # noqa: E402


class TestResearchTrack02B0SettleabilityAudit(unittest.TestCase):
    def test_stdlib_only_top_level_imports_in_isolated_subprocess(self) -> None:
        blocker = "\n".join(
            [
                "import sys",
                "for m in ('numpy','duckdb','pyarrow','pandas','scipy'):",
                "    sys.modules[m] = None",
                "import research_track02_b0_settleability_audit as m",
                "import json",
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
        self.assertIn('"self_test": "ok"', proc.stdout)
        self.assertIn('"ff_500_crossed_hours": 0', proc.stdout)

    def test_lookup_valid_step_with_retry_delays_recovery_and_enforces_cutoff(self) -> None:
        # 10 steps @ 200ms starting at t0=0 -> step k corresponds to t = (k+1)*200
        age = [0, 0, 100, 100, 0, 0, 0, 0, 0, 0]  # stale at k=2 (600ms), k=3 (800ms); recovers at k=4 (1000ms)
        bid = [100.0] * 10
        ask = [100.1] * 10
        # Target t=600ms (k=2, stale) -> recovers at k=4 (1000ms) -> schedules retry at 1000+200=1200ms (k=5)!
        hit = mod._lookup_valid_step_with_retry(
            age, bid, ask, target_ms=600, t0_ms=0, step_ms=200,
            max_age_steps=25, max_recovery_steps=25, retry_latency_ms=200, cutoff_ms=2_000,
        )
        self.assertEqual(hit, (5, 1_200))
        # If cutoff is 1_100ms (< t_retry 1_200ms), MUST refuse fill -> None!
        self.assertIsNone(
            mod._lookup_valid_step_with_retry(
                age, bid, ask, target_ms=600, t0_ms=0, step_ms=200,
                max_age_steps=25, max_recovery_steps=25, retry_latency_ms=200, cutoff_ms=1_100,
            )
        )

    def test_b0_funding_flat_avoids_boundary_and_reports_honest_failure_on_dark_book(self) -> None:
        n = 420
        step_ms = 200
        ht_target = 1_795_777_200_000  # exact :00 UTC hour
        t0 = ht_target - 251 * step_ms
        warmup_steps = 40

        a_bid = [99.99] * n
        a_ask = [100.01] * n
        a_bsz = [20.0] * n
        a_asz = [20.0] * n
        a_age = [0] * n
        h_bid = [99.99] * n
        h_ask = [100.01] * n
        h_bsz = [20.0] * n
        h_asz = [20.0] * n
        h_age = [0] * n
        h_bid[45:145] = [100.14] * 100
        h_ask[45:145] = [100.16] * 100

        causal = mod.compute_causal_basis_2h(a_bid, a_ask, a_age, h_bid, h_ask, h_age, step_ms=step_ms)
        tf = t0 + 120 * step_ms + 200 + 40
        # Also include a trade print inside open_block window [ht - 20s, ht + 2s] at k=210 (t_cur = ht - 8.2s)
        tf_blocked = t0 + 210 * step_ms + 200 + 40
        lt_tr = {"ts": [tf, tf_blocked], "sgn": [1, 1], "px": [100.25, 100.25], "sz": [20.0, 20.0]}
        lt_fund = {ht_target: 10.0}
        p_1k = mod.frozen_b0_params(1_000.0)

        r_orig = mod.simulate_b0_settleability(
            symbol="NVDA", domain="rwa",
            arcus_bid=a_bid, arcus_ask=a_ask, arcus_bsz=a_bsz, arcus_asz=a_asz, arcus_age=a_age,
            hedge_bid=h_bid, hedge_ask=h_ask, hedge_bsz=h_bsz, hedge_asz=h_asz, hedge_age=h_age,
            causal=causal, lighter_trades=lt_tr, t0_ms=t0, tick=0.01, params=p_1k, mode="B0_original",
            warmup_steps=warmup_steps, decision_steps=n - warmup_steps, clip_usd=1_000.0,
            lighter_funding=lt_fund, open_block_before_hour_ms=20_000, flatten_before_hour_ms=10_000, guard_half_ms=2_000,
        )
        r_ff = mod.simulate_b0_settleability(
            symbol="NVDA", domain="rwa",
            arcus_bid=a_bid, arcus_ask=a_ask, arcus_bsz=a_bsz, arcus_asz=a_asz, arcus_age=a_age,
            hedge_bid=h_bid, hedge_ask=h_ask, hedge_bsz=h_bsz, hedge_asz=h_asz, hedge_age=h_age,
            causal=causal, lighter_trades=lt_tr, t0_ms=t0, tick=0.01, params=p_1k, mode="B0_funding_flat",
            warmup_steps=warmup_steps, decision_steps=n - warmup_steps, clip_usd=1_000.0,
            lighter_funding=lt_fund, open_block_before_hour_ms=20_000, flatten_before_hour_ms=10_000, guard_half_ms=2_000,
        )
        r_flat = mod.simulate_b0_settleability(
            symbol="NVDA", domain="rwa",
            arcus_bid=a_bid, arcus_ask=a_ask, arcus_bsz=a_bsz, arcus_asz=a_asz, arcus_age=a_age,
            hedge_bid=h_bid, hedge_ask=h_ask, hedge_bsz=h_bsz, hedge_asz=h_asz, hedge_age=h_age,
            causal=causal, lighter_trades=lt_tr, t0_ms=t0, tick=0.01, params=p_1k, mode="flat",
            warmup_steps=warmup_steps, decision_steps=n - warmup_steps, clip_usd=1_000.0,
            lighter_funding=lt_fund, open_block_before_hour_ms=20_000, flatten_before_hour_ms=10_000, guard_half_ms=2_000,
        )
        self.assertEqual(r_orig["crossed_funding_hours"], 1)
        self.assertFalse(r_orig["funding_avoidance_qualified"])
        self.assertNotEqual(r_orig["funding_cash_usd"], 0.0)

        self.assertEqual(r_ff["crossed_funding_hours"], 0)
        self.assertTrue(r_ff["funding_avoidance_qualified"])
        self.assertEqual(r_ff["funding_cash_usd"], 0.0)
        self.assertEqual(r_ff["forced_funding_flattens"], 1)
        self.assertGreater(r_ff["lighter_fees_usdc"], 0.0)
        self.assertGreater(r_ff["arcus_fees_usdc"], 0.0)
        self.assertAlmostEqual(r_ff["fees_usdc"], r_ff["lighter_fees_usdc"] + r_ff["arcus_fees_usdc"], places=6)

        self.assertEqual(r_flat["fills"], 0)
        self.assertEqual(r_flat["mtm_incl_residual_pnl_usd"], 0.0)
        self.assertEqual(r_flat["crossed_funding_hours"], 0)

        # Dark-book across the flatten & guard window -> MUST fail funding avoidance honestly!
        a_age_dark = list(a_age)
        for kd in range(180, 270):
            a_age_dark[kd] = 500
        r_ff_dark = mod.simulate_b0_settleability(
            symbol="NVDA", domain="rwa",
            arcus_bid=a_bid, arcus_ask=a_ask, arcus_bsz=a_bsz, arcus_asz=a_asz, arcus_age=a_age_dark,
            hedge_bid=h_bid, hedge_ask=h_ask, hedge_bsz=h_bsz, hedge_asz=h_asz, hedge_age=h_age,
            causal=causal, lighter_trades=lt_tr, t0_ms=t0, tick=0.01, params=p_1k, mode="B0_funding_flat",
            warmup_steps=warmup_steps, decision_steps=n - warmup_steps, clip_usd=1_000.0,
            lighter_funding=lt_fund, open_block_before_hour_ms=20_000, flatten_before_hour_ms=10_000, guard_half_ms=2_000,
        )
        self.assertEqual(r_ff_dark["crossed_funding_hours"], 1)
        self.assertFalse(r_ff_dark["funding_avoidance_qualified"])

    def test_nonlinear_clip_sizes_offhours_margin_and_concurrent_aggregation(self) -> None:
        # Verify RWA off-hours IMF switches from 0.05 (RTH) to 0.075 (outside 04:00-20:00 ET / weekend)
        ts_rth = int(datetime(2026, 11, 24, 15, 0, tzinfo=timezone.utc).timestamp() * 1000)  # Tue 10:00 ET
        ts_off = int(datetime(2026, 11, 24, 2, 0, tzinfo=timezone.utc).timestamp() * 1000)   # Mon 21:00 ET
        self.assertAlmostEqual(mod.venue_margin_fractions("NVDA", "rwa", ts_rth)["arcus_imf"], 0.05, places=6)
        self.assertAlmostEqual(mod.venue_margin_fractions("NVDA", "rwa", ts_off)["arcus_imf"], 0.075, places=6)

        # Verify $500 vs $1000 vs $2000 non-linear event-by-event replay when L1 depth = $1,200
        n = 200
        step_ms = 200
        t0 = ts_rth
        a_bid, a_ask = [99.99] * n, [100.01] * n
        a_bsz, a_asz, a_age = [12.0] * n, [12.0] * n, [0] * n  # ~$1,200 L1
        h_bid, h_ask = [99.99] * n, [100.01] * n
        h_bsz, h_asz, h_age = [30.0] * n, [30.0] * n, [0] * n
        h_bid[45:145] = [100.14] * 100
        h_ask[45:145] = [100.16] * 100
        causal = mod.compute_causal_basis_2h(a_bid, a_ask, a_age, h_bid, h_ask, h_age, step_ms=step_ms)
        tf = t0 + 120 * step_ms + 200 + 40
        lt_tr = {"ts": [tf], "sgn": [1], "px": [100.25], "sz": [30.0]}

        runs_by_clip = {}
        for clip_usd in mod.CLIP_SIZES_USD:
            res = mod.simulate_b0_settleability(
                symbol="NVDA", domain="rwa",
                arcus_bid=a_bid, arcus_ask=a_ask, arcus_bsz=a_bsz, arcus_asz=a_asz, arcus_age=a_age,
                hedge_bid=h_bid, hedge_ask=h_ask, hedge_bsz=h_bsz, hedge_asz=h_asz, hedge_age=h_age,
                causal=causal, lighter_trades=lt_tr, t0_ms=t0, tick=0.01,
                params=mod.frozen_b0_params(clip_usd), mode="B0_original",
                warmup_steps=40, decision_steps=150, clip_usd=clip_usd, checkpoint_step_ms=1_000,
            )
            res["window_num"] = 1
            res["split"] = "selection_dev"
            runs_by_clip[clip_usd] = res

        self.assertEqual(runs_by_clip[500.0]["hedged_notional_usd"], 500.0)
        self.assertEqual(runs_by_clip[1_000.0]["hedged_notional_usd"], 1_000.0)
        # $2,000 clip is blocked by $1,200 L1 gate (NOT linearly scaled 2x of $1,000!)
        self.assertEqual(runs_by_clip[2_000.0]["fills"], 0)

        agg = mod.aggregate_settleability_runs([runs_by_clip[1_000.0]], clip_usd=1_000.0)
        col = agg["collateral_and_exposure"]
        self.assertGreater(col["concurrent_peak_imr_lighter_usd"], 60.0)
        self.assertGreater(col["concurrent_peak_imr_arcus_usd"], 45.0)
        self.assertGreater(col["required_total_equity_usdc"], col["concurrent_peak_imr_lighter_usd"] + col["concurrent_peak_imr_arcus_usd"])
        self.assertAlmostEqual(
            agg["fee_attribution_by_asset"]["USDC"]["total_fees_usdc"],
            runs_by_clip[1_000.0]["fees_usdc"],
            places=4,
        )

    def test_stationary_block_bootstrap_deterministic_and_nondegenerate(self) -> None:
        ser = [12.0, -3.0, 8.5, 4.0, -1.0, 10.0, 6.0, 2.5, 7.0, -2.0]
        b1 = mod.stationary_block_bootstrap_ci(ser, mean_block_len=3.0, n_boot=500, seed=42)
        b2 = mod.stationary_block_bootstrap_ci(ser, mean_block_len=3.0, n_boot=500, seed=42)
        self.assertEqual(b1, b2)
        self.assertLessEqual(b1["ci95_total"][0], b1["total"])
        self.assertGreaterEqual(b1["ci95_total"][1], b1["total"])


if __name__ == "__main__":
    unittest.main()
