#!/usr/bin/env python3
"""Unit tests for Track 01 Microstructure & RWA Alpha Upgrade Evaluator (100% pure stdlib)."""

from __future__ import annotations

import dataclasses
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

import research_track01_rwa_and_alpha_upgrade as t1  # noqa: E402


class TestTrack01RwaAndAlphaUpgrade(unittest.TestCase):
    def test_self_test_passes(self) -> None:
        res = t1.run_self_test()
        self.assertEqual(res["self_test"], "ok")
        self.assertTrue(res["obi_gated"])
        self.assertTrue(res["ref_reversal_exit"])
        self.assertTrue(res["spread_gated"])

    def test_stdlib_only_import_without_scientific_stack(self) -> None:
        cmd = [
            sys.executable,
            "-S",
            "-c",
            (
                "import sys; "
                "[sys.modules.__setitem__(m, None) for m in ('numpy','duckdb','pyarrow','pandas','scipy')]; "
                "import scripts.research_track01_rwa_and_alpha_upgrade as m; "
                "assert m.run_self_test()['self_test'] == 'ok'"
            ),
        ]
        proc = subprocess.run(
            cmd,
            cwd=str(SCRIPT_DIR.parent),
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(proc.returncode, 0, f"stderr: {proc.stderr}\nstdout: {proc.stdout}")

    def test_consensus_ref_mid_median_mean_and_filtering(self) -> None:
        self.assertEqual(
            t1.consensus_ref_mid(10_000, 5_000, [(100.3, 9_800), (100.1, 9_950), (100.2, 9_700)]),
            (100.2, 9_950),
        )
        two = t1.consensus_ref_mid(10_000, 5_000, [(100.1, 9_900), (200.0, 4_000), (100.3, 9_800)])
        self.assertIsNotNone(two)
        assert two is not None
        self.assertAlmostEqual(two[0], 100.2, places=9)
        self.assertEqual(two[1], 9_900)
        self.assertEqual(
            t1.consensus_ref_mid(10_000, 5_000, [(float("nan"), 9_900), (100.5, 10_001), (100.25, 9_500)]),
            (100.25, 9_500),
        )
        self.assertIsNone(t1.consensus_ref_mid(10_000, 5_000, [(-1.0, 9_900), (0.0, 9_900), None]))

    def test_l1_microprice_obi_and_spread_gates(self) -> None:
        mid, micro, obi, spr = t1.compute_l1_microprice_and_obi(99.95, 9.0, 100.05, 1.0)
        self.assertAlmostEqual(mid, 100.0, places=9)
        self.assertAlmostEqual(obi, 0.8, places=9)
        self.assertAlmostEqual(micro, 100.04, places=9)
        self.assertAlmostEqual(spr, 10.0, places=9)

        cfg = t1.LeadLagUpgradeConfig(
            theta_bps=4.0,
            tau_ms=2_000,
            hold_ms=1_000,
            taker_delay_ms=200,
            warmup_ms=1_000,
            use_microprice_fair=True,
            ref_obi_confirm_min=0.3,
            max_entry_tgt_spread_bps=3.0,
            min_edge_over_spread_bps=3.5,
        )
        eng = t1.LeadLagTakerUpgradePy(cfg)
        t = 10_000
        for _ in range(10):
            eng.observe_l1_books(t, (t, 99.995, 10.0, 100.005, 10.0), None, (t, 99.995, 10.0, 100.005, 10.0))
            t += 250

        # Wide target spread (4bp > 3.0) blocked
        self.assertEqual(
            eng.observe_l1_books(t, (t, 100.055, 9.0, 100.065, 1.0), None, (t, 99.98, 10.0, 100.02, 10.0))[0],
            "Idle",
        )
        # Opposed OBI (-0.8 < 0.3) blocked
        self.assertEqual(
            eng.observe_l1_books(t + 250, (t + 250, 100.055, 1.0, 100.065, 9.0), None, (t + 250, 99.995, 10.0, 100.005, 10.0))[0],
            "Idle",
        )
        # Aligned OBI + tight spread -> Enter Long
        act = eng.observe_l1_books(t + 500, (t + 500, 100.055, 9.0, 100.065, 1.0), None, (t + 500, 99.995, 10.0, 100.005, 10.0))
        self.assertEqual(act[0], "Enter")
        self.assertEqual(act[1], 1)

    def test_ref_reversal_early_exit_retains_position_and_stage_a_attribution(self) -> None:
        ref_books = []
        tgt_books = []
        trades = []
        for k in range(80):
            t = k * 250
            if 20 <= k < 24:
                r_mid = 100.10  # +10bp jump
            elif k >= 24:
                r_mid = 100.02  # snaps back -8bp!
            else:
                r_mid = 100.00
            t_mid = 100.00
            ref_books.append((t, r_mid - 0.005, 10.0, r_mid + 0.005, 10.0))
            tgt_books.append((t, t_mid - 0.005, 10.0, t_mid + 0.005, 10.0))
            if k == 21:
                trades.append((t, 1, 100.005, 2.0))
            if k == 25:
                trades.append((t, -1, 99.995, 2.0))

        cfg = t1.LeadLagUpgradeConfig(
            theta_bps=4.0,
            tau_ms=10_000,
            hold_ms=5_000,
            taker_delay_ms=200,
            warmup_ms=2_000,
            ref_reversal_exit_bps=3.0,
        )
        res = t1.simulate_upgrade_streams(ref_books, tgt_books, cfg, trades=trades, collect_trade_details=True)
        self.assertGreaterEqual(res["trades"], 1)
        self.assertGreaterEqual(res["early_ref_reversal_exits"], 1)
        summary = t1.summarize_stage_a_trades(res["trade_details"])
        self.assertGreaterEqual(summary["total_trades"], 1)
        self.assertIn("1s", summary["trajectory_mfe_mae"])

        # Bit-for-bit parity check between streaming and precomputed replay across modes
        for use_micro, use_cons, rev_bps, spr_max in [
            (False, False, None, None),
            (False, False, 3.0, 5.0),
            (True, False, 3.0, 5.0),
            (False, True, None, None),
        ]:
            c_test = dataclasses.replace(
                cfg,
                use_microprice_fair=use_micro,
                use_consensus_leader=use_cons,
                ref_reversal_exit_bps=rev_bps,
                max_entry_tgt_spread_bps=spr_max,
            )
            r_stream = t1.simulate_upgrade_streams(
                ref_books, tgt_books, c_test, confirm_books=ref_books, tertiary_books=ref_books, trades=trades
            )
            sig = t1.precompute_upgrade_signals(
                ref_books,
                tgt_books,
                confirm_books=ref_books,
                tertiary_books=ref_books,
                trades=trades,
                use_microprice_fair=use_micro,
                use_consensus_leader=use_cons,
                tau_values_ms=(10_000,),
                warmup_ms=2_000,
                min_cand_theta_bps=4.0,
            )
            self.assertIsNotNone(sig)
            assert sig is not None
            r_pre = t1.simulate_precomputed_upgrade(sig, c_test)
            self.assertEqual(r_stream, r_pre, f"parity mismatch for {(use_micro, use_cons, rev_bps, spr_max)}")

    def test_campaign_discovery_and_splits_crypto_and_rwa(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            for dom, ws in (("crypto", (1, 19, 21, 25, 26)), ("rwa", (1, 19, 21, 25, 26))):
                for w in ws:
                    cid = f"tokyo-{dom}-20261115T000000Z-w{w:02d}"
                    d = root / f"parquet/bbo_quotes/collector_region=tokyo/venue=lighter/utc_date=2026-11-15/campaign_id={cid}"
                    d.mkdir(parents=True, exist_ok=True)
                    (d / "part-0.parquet").write_bytes(b"")
            c_splits = t1.split_campaign_windows(t1.discover_campaigns(root, "crypto"))
            r_splits = t1.split_campaign_windows(t1.discover_campaigns(root, "rwa"))
            self.assertEqual(sorted(c_splits["selection_dev"].keys()), ["w01", "w19"])
            self.assertEqual(sorted(c_splits["holdout"].keys()), ["w21", "w25"])
            self.assertEqual(sorted(c_splits["w26_oos"].keys()), ["w26"])
            self.assertEqual(sorted(r_splits["selection_dev"].keys()), ["w01", "w19"])
            self.assertEqual(sorted(r_splits["holdout"].keys()), ["w21", "w25"])
            self.assertEqual(sorted(r_splits["w26_oos"].keys()), ["w26"])

    def test_stage_b_arm_configs_and_aggregation(self) -> None:
        pair = t1.LeadLagPair(
            domain="crypto",
            tgt_venue="lighter_robinhood",
            symbol="ETH",
            ref_venue="binance",
            confirm_venue="coinbase",
            tertiary_venue="extended",
        )
        frozen = {
            "theta_bps": 6.0,
            "hold_ms": 10_000,
            "tau_ms": 120_000,
            "vol_theta_mult": 1.5,
            "ref_obi_confirm_min": 0.25,
            "max_entry_tgt_spread_bps": 2.5,
            "min_edge_over_spread_bps": 4.0,
            "ref_reversal_exit_bps": 3.5,
            "m4_mode": "vol_m1_cons_m3",
        }
        cfgs = t1._arm_configs_stage_b(pair, frozen, taker_delay_ms=300, warmup_ms=2_000)
        self.assertEqual(tuple(cfgs.keys()), t1.STAGE_B_ARM_NAMES)
        self.assertEqual(cfgs["A_baseline"].taker_delay_ms, 300)
        self.assertFalse(cfgs["A_baseline"].use_microprice_fair)
        self.assertTrue(cfgs["M1_microprice_obi"].use_microprice_fair)
        self.assertEqual(cfgs["M1_microprice_obi"].ref_obi_confirm_min, 0.25)
        self.assertTrue(cfgs["M2_consensus_leader"].use_consensus_leader)
        self.assertEqual(cfgs["M3_ref_reversal_spread_gate"].ref_reversal_exit_bps, 3.5)
        self.assertTrue(cfgs["M4_combined"].use_consensus_leader)
        self.assertEqual(cfgs["M4_combined"].vol_theta_mult, 1.5)

        self.assertGreater(
            t1._score_candidate({"q1000": 10.0, "pv1000": 5.0, "pv_trades": 20}),
            t1._score_candidate({"q1000": -50.0, "pv1000": 20.0, "pv_trades": 20}),
        )


if __name__ == "__main__":
    unittest.main()
