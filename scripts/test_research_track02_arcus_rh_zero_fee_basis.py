#!/usr/bin/env python3
"""Stdlib-only unit tests for scripts/research_track02_arcus_rh_zero_fee_basis.py."""

from __future__ import annotations

import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPT_DIR = Path(__file__).resolve().parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

import research_track02_arcus_rh_zero_fee_basis as mod  # noqa: E402


class TestResearchTrack02ArcusRhZeroFeeBasis(unittest.TestCase):
    def test_stdlib_only_top_level_imports_in_isolated_subprocess(self) -> None:
        blocker = "\n".join(
            [
                "import sys",
                "for m in ('numpy','duckdb','pyarrow','pandas','scipy'):",
                "    sys.modules[m] = None",
                "import research_track02_arcus_rh_zero_fee_basis as m",
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

    def test_w28_exclusion_and_fee_schedules_and_symbol_catalog(self) -> None:
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
            self.assertEqual(mod.campaign_parquet_files(root, "bbo_quotes", "arcus", "2026-11-27", "tokyo-rwa-20261127T023000Z-w28"), [])

        fees_prem, _ = mod.resolve_track02_fees(None, lighter_mode="verified_prem")
        fees_std, _ = mod.resolve_track02_fees(None, lighter_mode="standard_zero")
        self.assertAlmostEqual(fees_prem["arcus"]["rwa"]["maker_fee_bps"], 0.0)
        self.assertAlmostEqual(fees_prem["arcus"]["rwa"]["taker_fee_bps"], 2.1375)
        self.assertAlmostEqual(fees_prem["arcus"]["crypto"]["maker_fee_bps"], 1.425)
        self.assertAlmostEqual(fees_prem["arcus"]["crypto"]["taker_fee_bps"], 4.275)
        self.assertAlmostEqual(fees_prem["lighter_robinhood"]["rwa"]["taker_fee_bps"], 0.0)
        self.assertAlmostEqual(fees_prem["lighter"]["rwa"]["taker_fee_bps"], 2.80)
        self.assertAlmostEqual(fees_std["lighter"]["rwa"]["taker_fee_bps"], 0.0)

        cat = mod.build_symbol_catalog()
        rh_rwa = [s.symbol for s in cat if s.pair_group == "arcus_rh" and s.domain == "rwa"]
        self.assertEqual(len(rh_rwa), 14)
        self.assertTrue({"NVDA", "CRCL", "TSLA", "QQQ", "AMZN", "META", "AMD", "MU", "GOOGL", "AAPL"}.issubset(set(rh_rwa)))

    def test_l1_clamp_orphan_unwind_and_residual_mtm_separation(self) -> None:
        n = 140
        t0 = 1_700_000_400_000
        a_bid = [99.99] * n
        a_ask = [100.01] * n
        a_bsz = [20.0] * n
        a_asz = [20.0] * n
        a_age = [0] * n
        h_bid = [99.99] * n
        h_ask = [100.01] * n
        h_bsz = [3.5] * n  # ~$350 L1 (< $1000 -> clamps to $350 and unwinds $650 orphan!)
        h_asz = [20.0] * n
        h_age = [0] * n

        a_bid[75:85] = [99.89] * 10
        a_ask[75:85] = [99.91] * 10
        causal = mod.compute_causal_basis_2h(a_bid, a_ask, a_age, h_bid, h_ask, h_age, step_ms=mod.STEP_MS)
        tf = t0 + 80 * mod.STEP_MS + 200 + 36
        a_tr = {"ts": [tf], "sgn": [-1], "px": [99.80], "sz": [12.0]}
        p_drift = mod.BasisArbParams(name="drift_block", arm="A0_arcus_mk_hedge_tk", e0_bps=4.0, basis_drift_guard_bps=2.0, min_hedge_l1_notional_usd=0.0)
        res_drift = mod.simulate_ws_basis_arb(
            arcus_bid=a_bid, arcus_ask=a_ask, arcus_bsz=a_bsz, arcus_asz=a_asz, arcus_age=a_age,
            hedge_bid=h_bid, hedge_ask=h_ask, hedge_bsz=h_bsz, hedge_asz=h_asz, hedge_age=h_age,
            causal=causal, arcus_trades=a_tr, hedge_trades=None, t0_ms=t0, tick=0.01, params=p_drift,
            arcus_delay_ms=36, hedge_maker_delay_ms=200, hedge_taker_delay_ms=330, hedge_speedbump_ms=200,
            arcus_mk_fee_bps=0.0, arcus_tk_fee_bps=2.1375, hedge_mk_fee_bps=0.0, hedge_tk_fee_bps=0.0,
            warmup_steps=50, decision_steps=85, fx_drag_bps=2.0,
        )
        self.assertEqual(res_drift["blocked_drift"], 1)
        self.assertEqual(res_drift["fills"], 0)

        p = mod.BasisArbParams(name="clamp_test", arm="A0_arcus_mk_hedge_tk", e0_bps=4.0, basis_drift_guard_bps=0.0, min_hedge_l1_notional_usd=0.0)

        res = mod.simulate_ws_basis_arb(
            arcus_bid=a_bid, arcus_ask=a_ask, arcus_bsz=a_bsz, arcus_asz=a_asz, arcus_age=a_age,
            hedge_bid=h_bid, hedge_ask=h_ask, hedge_bsz=h_bsz, hedge_asz=h_asz, hedge_age=h_age,
            causal=causal, arcus_trades=a_tr, hedge_trades=None, t0_ms=t0, tick=0.01, params=p,
            arcus_delay_ms=36, hedge_maker_delay_ms=200, hedge_taker_delay_ms=330, hedge_speedbump_ms=200,
            arcus_mk_fee_bps=0.0, arcus_tk_fee_bps=2.1375, hedge_mk_fee_bps=0.0, hedge_tk_fee_bps=0.0,
            warmup_steps=50, decision_steps=85, fx_drag_bps=2.0,
        )
        self.assertEqual(res["fills"], 1)
        self.assertAlmostEqual(res["hedged_notional_usd"], 3.5 * 99.99, places=1)
        self.assertEqual(res["orphan_events"], 1)
        self.assertAlmostEqual(res["orphan_notional_usd"], 1000.0 - 3.5 * 99.99, places=1)
        self.assertGreater(res["orphan_unwind_rate"], 0.60)
        self.assertGreater(res["fx_drag_usd"], 0.0)
        self.assertTrue(res["terminal_flat"])

        # Now make terminal books stale at the end -> terminal_flat=False & residual_notional > 0
        h_age_dark = list(h_age)
        for k in range(110, n):
            h_age_dark[k] = 500
        res_dark = mod.simulate_ws_basis_arb(
            arcus_bid=a_bid, arcus_ask=a_ask, arcus_bsz=a_bsz, arcus_asz=a_asz, arcus_age=a_age,
            hedge_bid=h_bid, hedge_ask=h_ask, hedge_bsz=h_bsz, hedge_asz=h_asz, hedge_age=h_age_dark,
            causal=causal, arcus_trades=a_tr, hedge_trades=None, t0_ms=t0, tick=0.01, params=p,
            arcus_delay_ms=36, hedge_maker_delay_ms=200, hedge_taker_delay_ms=330, hedge_speedbump_ms=200,
            arcus_mk_fee_bps=0.0, arcus_tk_fee_bps=2.1375, hedge_mk_fee_bps=0.0, hedge_tk_fee_bps=0.0,
            warmup_steps=50, decision_steps=85,
        )
        self.assertFalse(res_dark["terminal_flat"])
        self.assertGreater(res_dark["terminal_residual_notional_usd"], 300.0)
        self.assertAlmostEqual(res_dark["realized_only_pnl_usd"], res_dark["intraday_realized_pnl_usd"], places=4)

    def test_per_leg_funding_skew_and_maker_fallback_hedge(self) -> None:
        n = 160
        t0 = 1_700_000_400_000
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

        a_bid[75:85] = [99.89] * 10
        a_ask[75:85] = [99.91] * 10
        causal = mod.compute_causal_basis_2h(a_bid, a_ask, a_age, h_bid, h_ask, h_age, step_ms=mod.STEP_MS)
        tf = t0 + 80 * mod.STEP_MS + 200 + 36
        a_tr = {"ts": [tf], "sgn": [-1], "px": [99.80], "sz": [15.0]}
        # Passive hedge print on LighterRH at tf + 300 (> tf + 200 maker delay)
        h_tr = {"ts": [tf + 300], "sgn": [1], "px": [100.05], "sz": [15.0]}
        # Settlement at tf + 150 (strictly > leg_a_fill_ms=tf, strictly < leg_b_fill_ms=tf+300) AND at tf + 500 (> both)
        h_fund = {tf + 150: 10.0, tf + 500: 10.0}
        # Also include Arcus funding only at tf (== leg_a_fill_ms -> must NOT settle due to strict >) and tf + 500
        a_fund = {tf: 10.0, tf + 500: 5.0}

        p_fb = mod.BasisArbParams(
            name="fb_test",
            arm="A1_arcus_mk_hedge_mk_fb",
            flipped_role=True,
            hedge_mode="maker_fallback",
            e0_bps=4.0,
            basis_drift_guard_bps=0.0,
            max_hedge_passive_wait_ms=1_500,
        )
        res = mod.simulate_ws_basis_arb(
            arcus_bid=a_bid, arcus_ask=a_ask, arcus_bsz=a_bsz, arcus_asz=a_asz, arcus_age=a_age,
            hedge_bid=h_bid, hedge_ask=h_ask, hedge_bsz=h_bsz, hedge_asz=h_asz, hedge_age=h_age,
            causal=causal, arcus_trades=a_tr, hedge_trades=h_tr, t0_ms=t0, tick=0.01, params=p_fb,
            arcus_delay_ms=36, hedge_maker_delay_ms=200, hedge_taker_delay_ms=330, hedge_speedbump_ms=200,
            arcus_mk_fee_bps=0.0, arcus_tk_fee_bps=2.1375, hedge_mk_fee_bps=0.0, hedge_tk_fee_bps=0.0,
            warmup_steps=50, decision_steps=100, arcus_funding=a_fund, hedge_funding=h_fund, record_trades=True,
        )
        self.assertEqual(res["hedge_passive_fills"], 1)
        self.assertEqual(res["hedge_taker_fallbacks"], 0)
        # Hedge leg only settles tf + 500 (+1.00 USD), NOT tf + 150 (which was before hedge filled at tf + 300)!
        self.assertAlmostEqual(res["funding_leg_hd_usd"], 1.0, delta=0.02)
        # Maker leg only settles tf + 500 (-0.50 USD), NOT tf (which equaled leg_a_fill_ms)!
        self.assertAlmostEqual(res["funding_leg_mk_usd"], -0.50, delta=0.02)

        # Verify _clone_run_with_fx matches direct fx_drag_bps=3.0 simulation exactly!
        res_fx3_direct = mod.simulate_ws_basis_arb(
            arcus_bid=a_bid, arcus_ask=a_ask, arcus_bsz=a_bsz, arcus_asz=a_asz, arcus_age=a_age,
            hedge_bid=h_bid, hedge_ask=h_ask, hedge_bsz=h_bsz, hedge_asz=h_asz, hedge_age=h_age,
            causal=causal, arcus_trades=a_tr, hedge_trades=h_tr, t0_ms=t0, tick=0.01, params=p_fb,
            arcus_delay_ms=36, hedge_maker_delay_ms=200, hedge_taker_delay_ms=330, hedge_speedbump_ms=200,
            arcus_mk_fee_bps=0.0, arcus_tk_fee_bps=2.1375, hedge_mk_fee_bps=0.0, hedge_tk_fee_bps=0.0,
            warmup_steps=50, decision_steps=100, fx_drag_bps=3.0, arcus_funding=a_fund, hedge_funding=h_fund,
        )
        res_fx3_cloned = mod._clone_run_with_fx(res, 3.0)
        for k in ("fx_drag_usd", "intraday_realized_pnl_usd", "terminal_drain_pnl_usd", "realized_only_pnl_usd", "mtm_incl_residual_pnl_usd"):
            self.assertAlmostEqual(res_fx3_cloned[k], res_fx3_direct[k], places=3, msg=f"mismatch on {k}")


if __name__ == "__main__":
    unittest.main()
