#!/usr/bin/env python3
"""Stdlib-only unit tests for scripts/research_track02_passive_hedge_arb.py."""

from __future__ import annotations

import os
from pathlib import Path
import subprocess
import sys
import unittest

SCRIPT_DIR = Path(__file__).resolve().parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

import research_track02_passive_hedge_arb as mod  # noqa: E402


class TestResearchTrack02PassiveHedgeArb(unittest.TestCase):
    def test_stdlib_only_top_level_imports_in_isolated_subprocess(self) -> None:
        blocker = "\n".join([
            "import sys",
            "for m in ('numpy','duckdb','pyarrow','pandas','scipy'):",
            "    sys.modules[m] = None",
            "import research_track02_passive_hedge_arb as m",
            "m.run_self_test()",
        ])
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

    def test_splits_and_lighter_standard_zero_provenance(self) -> None:
        self.assertEqual(mod.split_for_window(1), "selection_dev")
        self.assertEqual(mod.split_for_window(20), "selection_dev")
        self.assertEqual(mod.split_for_window(21), "holdout")
        self.assertEqual(mod.split_for_window(25), "holdout")
        self.assertEqual(mod.split_for_window(26), "w26_oos")

        meta, ev, prov = mod.resolve_venue_meta(None, lighter_fee_mode="standard_zero")
        self.assertEqual(meta["lighter"]["maker_fee_bps"]["crypto"], 0.0)
        self.assertEqual(meta["lighter"]["taker_fee_bps"]["crypto"], 0.0)
        self.assertEqual(meta["lighter"]["fee_status"], "lighter_standard_zero_scenario")
        self.assertEqual(prov["lighter_fee_mode"], "standard_zero")

        catalog = mod.build_pair_catalog()
        labels = {p.label for p in catalog}
        self.assertIn("arcus->lighter BTC", labels)
        self.assertIn("extended->lighter ETH", labels)
        self.assertIn("rh->lighter QQQ", labels)
        self.assertIn("lighter->rh QQQ", labels)
        same_dom = [p for p in catalog if p.same_risk_domain]
        self.assertTrue(all({p.maker_venue, p.hedge_venue} == {"lighter", "lighter_robinhood"} for p in same_dom))

    def test_passive_hedge_strict_print_verification_hazard_and_fail_closed(self) -> None:
        n = 160
        mk_bid = [99.99] * n
        mk_ask = [100.01] * n
        mk_bsz = [10.0] * n
        mk_asz = [10.0] * n
        mk_age = [0] * n
        hd_bid = [99.99] * n
        hd_ask = [100.01] * n
        hd_bsz = [20.0] * n
        hd_asz = [20.0] * n
        hd_age = [0] * n
        ref_mid = [100.0] * n
        ref_age = [0] * n

        # Warmup basis vol + cheap maker at k=70
        for k in range(10, 40):
            mk_bid[k], mk_ask[k] = (100.04, 100.06) if k % 2 == 0 else (99.94, 99.96)
        mk_bid[70], mk_ask[70] = 99.89, 99.91
        mk_trades = {"ts": [70 * mod.STEP_MS + 100], "sgn": [-1], "px": [99.85], "sz": [15.0]}

        causal = mod.compute_causal_basis_series(
            mk_bid, mk_ask, mk_bsz, mk_asz, mk_age,
            hd_bid, hd_ask, hd_bsz, hd_asz, hd_age,
            vol_tau_ms=2_000,
        )
        p_pas = mod.StrategyParams(
            name="pas_test",
            arm="T2_passive_then_ioc",
            e0_bps=1.0,
            vol_edge_mult=0.0,
            basis_drift_guard_bps=0.0,
            min_basis_zscore=2.0,
            basis_vol_floor_bps=2.0,
            max_drift_to_vol_ratio=2.0,
            quote_favorable_side_only=True,
            passive_hedge=mod.PassiveHedgeParams(
                enabled=True,
                max_passive_hedge_ms=800,
                hazard_ioc_adverse_bps=2.0,
                max_hedge_book_age_ms=500,
            ),
        )

        # Case 1: Adverse move at k=73 (>= 2.0 bps drop) escalates immediately to CrossIoc.
        hd_bid_haz = list(hd_bid)
        hd_ask_haz = list(hd_ask)
        for k in range(73, n):
            hd_bid_haz[k], hd_ask_haz[k] = 99.94, 99.96
        r_haz = mod.simulate_pair_arb(
            mk_bid=mk_bid, mk_ask=mk_ask, mk_age=mk_age,
            hd_bid=hd_bid_haz, hd_ask=hd_ask_haz, hd_bid_sz=hd_bsz, hd_ask_sz=hd_asz, hd_age=hd_age,
            ref_mid=ref_mid, ref_age=ref_age, causal=causal, mk_trades=mk_trades, hd_trades=None,
            t0_ms=0, tick=0.01, hd_tick=0.01, params=p_pas,
            maker_delay_ms=0, cancel_latency_ms=0, hedge_delay_ms=100, speedbump_ms=0, rtt_ms=40,
            maker_fee_bps=0.0, maker_taker_fee_bps=0.0, hedge_maker_fee_bps=0.0, hedge_taker_fee_bps=2.25,
            warmup_steps=50, decision_steps=80,
        )
        self.assertEqual(r_haz["passive_escalate_hazard"], 1)
        self.assertEqual(r_haz["passive_hedge_fill_pct"], 0.0)

        # Case 2: Stale hedge book during passive wait triggers passive_fail_closed_halts.
        hd_age_stale = list(hd_age)
        for k in range(72, 78):
            hd_age_stale[k] = 100  # 10s > 500ms
        r_st = mod.simulate_pair_arb(
            mk_bid=mk_bid, mk_ask=mk_ask, mk_age=mk_age,
            hd_bid=hd_bid, hd_ask=hd_ask, hd_bid_sz=hd_bsz, hd_ask_sz=hd_asz, hd_age=hd_age_stale,
            ref_mid=ref_mid, ref_age=ref_age, causal=causal, mk_trades=mk_trades, hd_trades=None,
            t0_ms=0, tick=0.01, hd_tick=0.01, params=p_pas,
            maker_delay_ms=0, cancel_latency_ms=0, hedge_delay_ms=100, speedbump_ms=0, rtt_ms=40,
            maker_fee_bps=0.0, maker_taker_fee_bps=0.0, hedge_maker_fee_bps=0.0, hedge_taker_fee_bps=2.25,
            warmup_steps=50, decision_steps=80,
        )
        self.assertEqual(r_st["passive_fail_closed_halts"], 1)

        # Case 3: Wrong-direction / non-through hedge print NEVER fills passively!
        hd_tr_wrong = {"ts": [72 * mod.STEP_MS + 120], "sgn": [-1], "px": [99.98], "sz": [50.0]}
        r_wrong = mod.simulate_pair_arb(
            mk_bid=mk_bid, mk_ask=mk_ask, mk_age=mk_age,
            hd_bid=hd_bid, hd_ask=hd_ask, hd_bid_sz=hd_bsz, hd_ask_sz=hd_asz, hd_age=hd_age,
            ref_mid=ref_mid, ref_age=ref_age, causal=causal, mk_trades=mk_trades, hd_trades=hd_tr_wrong,
            t0_ms=0, tick=0.01, hd_tick=0.01, params=p_pas,
            maker_delay_ms=0, cancel_latency_ms=0, hedge_delay_ms=100, speedbump_ms=0, rtt_ms=40,
            maker_fee_bps=0.0, maker_taker_fee_bps=0.0, hedge_maker_fee_bps=0.0, hedge_taker_fee_bps=2.25,
            warmup_steps=50, decision_steps=80,
        )
        self.assertEqual(r_wrong["passive_hedge_fill_pct"], 0.0)
        self.assertEqual(r_wrong["passive_escalate_timeout"], 1)

    def test_stage_a_attribution_identity_and_stationarity_gate(self) -> None:
        n = 160
        mk_bid = [99.99] * n
        mk_ask = [100.01] * n
        mk_bsz = [10.0] * n
        mk_asz = [10.0] * n
        mk_age = [0] * n
        hd_bid = [99.99] * n
        hd_ask = [100.01] * n
        hd_bsz = [20.0] * n
        hd_asz = [20.0] * n
        hd_age = [0] * n
        ref_mid = [100.0] * n
        ref_age = [0] * n

        # Persistent +15bps regime shift from k=55..100 makes fast_fair diverge from fair -> blocked!
        for k in range(55, 100):
            mk_bid[k], mk_ask[k] = 100.14, 100.16
        mk_trades = {"ts": [95 * mod.STEP_MS + 100], "sgn": [1], "px": [100.20], "sz": [15.0]}
        causal = mod.compute_causal_basis_series(
            mk_bid, mk_ask, mk_bsz, mk_asz, mk_age,
            hd_bid, hd_ask, hd_bsz, hd_asz, hd_age,
            tau_ms=600_000, vol_tau_ms=60_000, drift_tau_ms=1_000,
        )
        p_stat = mod.StrategyParams(
            name="stat_test",
            arm="T2_zscore_selective",
            e0_bps=1.0,
            vol_edge_mult=0.0,
            basis_drift_guard_bps=0.0,
            min_basis_zscore=1.5,
            basis_vol_floor_bps=2.0,
            max_drift_to_vol_ratio=1.5,
            quote_favorable_side_only=True,
        )
        r_blk = mod.simulate_pair_arb(
            mk_bid=mk_bid, mk_ask=mk_ask, mk_age=mk_age,
            hd_bid=hd_bid, hd_ask=hd_ask, hd_bid_sz=hd_bsz, hd_ask_sz=hd_asz, hd_age=hd_age,
            ref_mid=ref_mid, ref_age=ref_age, causal=causal, mk_trades=mk_trades, hd_trades=None,
            t0_ms=0, tick=0.01, hd_tick=0.01, params=p_stat,
            maker_delay_ms=0, cancel_latency_ms=0, hedge_delay_ms=100, speedbump_ms=0, rtt_ms=40,
            maker_fee_bps=0.0, maker_taker_fee_bps=0.0, hedge_maker_fee_bps=0.0, hedge_taker_fee_bps=0.0,
            warmup_steps=50, decision_steps=80, collect_attribution=True,
        )
        self.assertGreaterEqual(r_blk["blocked_nonstationary"], 1)
        self.assertEqual(r_blk["fills"], 0)


if __name__ == "__main__":
    unittest.main()
