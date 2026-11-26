#!/usr/bin/env python3
"""Unit tests for Track 01 multi-venue crypto lead-lag evaluator (100% stdlib)."""

from __future__ import annotations

import dataclasses
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

import research_track01_multivenue_lead_lag as mv  # noqa: E402


class TestTrack01MultivenueLeadLag(unittest.TestCase):
    def test_self_test_passes(self) -> None:
        res = mv.run_self_test()
        self.assertEqual(res["self_test"], "ok")
        self.assertEqual(res["precomputed_parity"], "exact")
        self.assertGreaterEqual(res["baseline_trades"], 1)
        self.assertGreaterEqual(res["catchup_exits"], 1)
        self.assertEqual(res["dual_reject_trades"], 0)
        self.assertGreaterEqual(res["dual_agree_trades"], 1)

    def test_stdlib_only_import_without_scientific_stack(self) -> None:
        cmd = [
            sys.executable,
            "-c",
            (
                "import sys; "
                "[sys.modules.__setitem__(m, None) for m in ('numpy','duckdb','pyarrow','pandas','scipy')]; "
                "import scripts.research_track01_multivenue_lead_lag as m; "
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

    def test_canonical_windows_and_venue_fee_provenance(self) -> None:
        self.assertEqual(
            sorted(mv.CANONICAL_CRYPTO_WINDOWS["selection_dev"].keys()),
            ["w18", "w19", "w20", "w21"],
        )
        self.assertEqual(
            sorted(mv.CANONICAL_CRYPTO_WINDOWS["holdout"].keys()),
            ["w21", "w22", "w23", "w24", "w25"],
        )
        self.assertEqual(mv.VENUE_PROFILES["lighter"]["settlement"], "USDC")
        self.assertEqual(mv.VENUE_PROFILES["lighter"]["account_fee_status"], "public_tier0_unverified")
        self.assertEqual(mv.VENUE_PROFILES["lighter_robinhood"]["settlement"], "USDG")
        self.assertEqual(mv.VENUE_PROFILES["lighter_robinhood"]["account_fee_status"], "public_tier0_unverified")
        self.assertEqual(mv.VENUE_PROFILES["extended"]["taker_fee_bps"], 2.50)
        self.assertEqual(mv.VENUE_PROFILES["arcus"]["taker_fee_bps"], 4.50)
        self.assertEqual(len(mv.build_default_pairs()), 24)

    def test_cross_midnight_parquet_globbing_and_dynamic_discovery(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            for w_num, d_str in ((1, "2026-11-15"), (3, "2026-11-16"), (8, "2026-11-18"), (20, "2026-11-23"), (21, "2026-11-24"), (25, "2026-11-25")):
                cid = f"tokyo-crypto-20261115T000000Z-w{w_num:02d}"
                d = root / f"parquet/bbo_quotes/collector_region=tokyo/venue=lighter/utc_date={d_str}/campaign_id={cid}"
                d.mkdir(parents=True, exist_ok=True)
                (d / "part-0.parquet").write_bytes(b"")

            splits = mv.resolve_crypto_splits(root, "both")
            self.assertEqual(sorted(splits["full_dev"].keys()), ["w01", "w03", "w20"])
            self.assertEqual(sorted(splits["holdout"].keys()), ["w21", "w25"])

            # Cross-midnight check for w01
            cid_1 = splits["full_dev"]["w01"]["campaign_id"]
            d_next = root / f"parquet/bbo_quotes/collector_region=tokyo/venue=lighter/utc_date=2026-11-16/campaign_id={cid_1}"
            d_next.mkdir(parents=True, exist_ok=True)
            (d_next / "part-1.parquet").write_bytes(b"")
            found = mv._campaign_parquet_files(root, "bbo_quotes", "lighter", "2026-11-15", cid_1)
            self.assertEqual(len(found), 2)

    def test_vol_adaptive_theta_suppresses_noisy_marginal_entries_and_ignores_stale(self) -> None:
        cfg = mv.LeadLagArmParams(
            theta_bps=3.0,
            tau_ms=2_000,
            hold_ms=1_000,
            taker_delay_ms=200,
            warmup_ms=1_000,
            vol_theta_mult=1.5,
            vol_ewma_tau_ms=2_000,
        )
        eng = mv.LeadLagTakerPy(cfg)
        t = 10_000
        for i in range(10):
            tgt = 100.0 if i % 2 == 0 else 100.10
            eng.observe_with_confirm(t, (tgt, t), None, (tgt, t))
            t += 500
        sigma = eng.sigma_1s_bps()
        self.assertGreater(sigma, 5.0)
        self.assertGreater(eng.effective_theta_bps(), 7.5)

        t_last = t - 500
        self.assertEqual(
            eng.observe_with_confirm(t, (100.0, t), None, (150.0, t - 6_000))[0],
            "Blocked:StaleTarget",
        )
        self.assertEqual(
            eng.observe_with_confirm(t_last - 100, (100.0, t_last - 100), None, (150.0, t_last - 100))[0],
            "Blocked:InvalidInput",
        )
        self.assertAlmostEqual(eng.sigma_1s_bps(), sigma, places=12)

        eng.observe_with_confirm(t, (100.0, t), None, (100.0, t))
        self.assertEqual(
            eng.observe_with_confirm(t + 250, (100.05, t + 250), None, (100.0, t + 250))[0],
            "Idle",
        )
        self.assertEqual(
            eng.observe_with_confirm(t + 500, (100.20, t + 500), None, (100.0, t + 500))[0],
            "Enter",
        )

    def test_dual_leader_confirm_and_stale_fallback(self) -> None:
        cfg = mv.LeadLagArmParams(
            theta_bps=3.0,
            tau_ms=2_000,
            hold_ms=1_000,
            taker_delay_ms=200,
            warmup_ms=1_000,
            require_dual_leader_confirm=True,
        )
        eng = mv.LeadLagTakerPy(cfg)
        t = 10_000
        for _ in range(10):
            eng.observe_with_confirm(t, (100.0, t), (100.0, t), (100.0, t))
            t += 250

        act = eng.observe_with_confirm(t, (100.05, t), (99.98, t), (100.0, t))
        self.assertEqual(act[0], "Idle")
        self.assertIsNone(eng.pos)

        t += 6_000
        eng.observe_with_confirm(t, (100.0, t), (130.0, t - 6_000), (100.0, t))
        t += 250
        act2 = eng.observe_with_confirm(t, (100.05, t), (100.02, t), (100.0, t))
        self.assertEqual(act2[0], "Enter")
        self.assertEqual(act2[1], 1)

    def test_precomputed_parity_across_all_enhancements(self) -> None:
        ref_books = []
        tgt_books = []
        conf_books = []
        trades = []
        for k in range(200):
            t = k * 250
            pulse1 = 30 <= k < 42
            catch1 = 34 <= k < 42
            pulse2 = 90 <= k < 102
            catch2 = 94 <= k < 102
            pulse3 = 140 <= k < 152
            r_mid = 100.12 if (pulse1 or pulse3) else (99.88 if pulse2 else 100.00)
            c_mid = 100.07 if pulse1 else (99.93 if pulse2 else 100.00)  # disagrees on pulse3!
            t_mid = 100.10 if catch1 else (99.90 if catch2 else 100.00)
            ref_books.append((t, r_mid - 0.005, 10.0, r_mid + 0.005, 10.0))
            conf_books.append((t, c_mid - 0.005, 10.0, c_mid + 0.005, 10.0))
            tgt_books.append((t, t_mid - 0.005, 8.0, t_mid + 0.005, 8.0))
            if k in (31, 91, 141):
                sgn = -1 if k == 91 else 1
                trades.append((t, sgn, t_mid + 0.005 * sgn, 2.0))
            if k in (38, 98, 148):
                sgn = 1 if k == 98 else -1
                trades.append((t, sgn, t_mid - 0.005 * sgn, 2.0))

        sig = mv.precompute_pair_signals(
            ref_books,
            tgt_books,
            confirm_books=conf_books,
            trades=trades,
            tau_values_ms=(30_000,),
            warmup_ms=2_000,
            min_cand_theta_bps=4.0,
        )
        assert sig is not None
        for mod in (
            dict(vol_theta_mult=0.0, require_dual_leader_confirm=False, exit_on_catchup_frac=None),
            dict(vol_theta_mult=1.5, require_dual_leader_confirm=False, exit_on_catchup_frac=None),
            dict(vol_theta_mult=0.0, require_dual_leader_confirm=True, exit_on_catchup_frac=None),
            dict(vol_theta_mult=0.0, require_dual_leader_confirm=False, exit_on_catchup_frac=0.70),
            dict(vol_theta_mult=1.5, require_dual_leader_confirm=True, exit_on_catchup_frac=0.70),
        ):
            cfg = mv.LeadLagArmParams(
                theta_bps=4.0,
                tau_ms=30_000,
                hold_ms=3_000,
                taker_delay_ms=230,
                warmup_ms=2_000,
                taker_fee_bps=0.0,
                **mod,
            )
            slow = mv.simulate_lead_lag_streams(ref_books, tgt_books, cfg, confirm_books=conf_books, trades=trades)
            fast = mv.simulate_precomputed_arm(sig, cfg)
            self.assertEqual(fast, slow, f"Mismatch for {mod}: fast={fast} vs slow={slow}")

    def test_print_verification_fees_and_tradeless_honesty(self) -> None:
        ref_books = []
        tgt_books = []
        for k in range(40):
            t = k * 250
            r_mid = 100.10 if 15 <= k < 25 else 100.00
            t_mid = 100.08 if 20 <= k < 25 else 100.00
            ref_books.append((t, r_mid - 0.005, 10.0, r_mid + 0.005, 10.0))
            tgt_books.append((t, t_mid - 0.005, 10.0, t_mid + 0.005, 10.0))

        cfg = mv.LeadLagArmParams(
            theta_bps=4.0,
            tau_ms=20_000,
            hold_ms=1_500,
            taker_delay_ms=250,
            warmup_ms=2_000,
            taker_fee_bps=2.50,
        )
        res_no_tr = mv.simulate_lead_lag_streams(ref_books, tgt_books, cfg, trades=None)
        self.assertEqual(res_no_tr["trades"], 1)
        self.assertEqual(res_no_tr["print_verified_trades"], 0)
        self.assertIsNone(res_no_tr["print_verified_net_bps_mean"])

        trades = [
            (4_250, 1, 100.01, 1.0),
            (5_800, -1, 100.09, 1.0),
        ]
        res_tr = mv.simulate_lead_lag_streams(ref_books, tgt_books, cfg, trades=trades)
        self.assertEqual(res_tr["print_verified_trades"], 1)
        self.assertAlmostEqual(res_tr["print_verified_net_bps_mean"], 2.9967, places=2)
        self.assertAlmostEqual(res_tr["pv_clip_pnl_usd"]["500"], res_tr["pv_clip_pnl_usd"]["1000"], places=6)

    def test_regime_stratification_and_tail_risk_curve(self) -> None:
        dev_rows = [
            {"win": "w01", "symbol": "BTC", "cont": True, "tgt_realized_1s_vol_bps": 0.4},
            {"win": "w02", "symbol": "BTC", "cont": True, "tgt_realized_1s_vol_bps": 0.8},
            {"win": "w03", "symbol": "BTC", "cont": True, "tgt_realized_1s_vol_bps": 3.5},
        ]
        cutoffs = mv._compute_regime_cutoffs_from_dev(dev_rows)
        self.assertEqual(mv._classify_regime("BTC", 0.4, cutoffs), "calm")
        self.assertEqual(mv._classify_regime("BTC", 0.8, cutoffs), "normal")
        self.assertEqual(mv._classify_regime("BTC", 3.5, cutoffs), "extreme_crash")

        grp = [
            {
                "win": "w01",
                "regime": "calm",
                "trades": 10,
                "print_verified_trades": 8,
                "quote_net_bps_mean": 2.0,
                "print_verified_net_bps_mean": 1.5,
                "quote_clip_pnl_usd": {"1000": 5.0},
                "pv_clip_pnl_usd": {"1000": 4.0},
            },
            {
                "win": "w02",
                "regime": "normal",
                "trades": 10,
                "print_verified_trades": 8,
                "quote_net_bps_mean": -3.0,
                "print_verified_net_bps_mean": -2.5,
                "quote_clip_pnl_usd": {"1000": -7.0},
                "pv_clip_pnl_usd": {"1000": -6.0},
            },
            {
                "win": "w03",
                "regime": "extreme_crash",
                "trades": 10,
                "print_verified_trades": 8,
                "quote_net_bps_mean": 4.0,
                "print_verified_net_bps_mean": 3.5,
                "quote_clip_pnl_usd": {"1000": 10.0},
                "pv_clip_pnl_usd": {"1000": 8.0},
            },
        ]
        tail = mv._tail_risk_and_curve(grp)
        self.assertAlmostEqual(tail["max_drawdown_quote_1000_usd"], 7.0, places=4)
        self.assertAlmostEqual(tail["max_drawdown_pv_1000_usd"], 6.0, places=4)
        self.assertEqual(tail["worst_10h_window_quote"]["win"], "w02")
        self.assertAlmostEqual(tail["positive_window_ratio_quote"], 0.6667, places=4)
        self.assertAlmostEqual(tail["cumulative_curve"][-1]["cum_quote_1000_usd"], 8.0, places=4)

    def test_selection_freeze_and_arm_configs(self) -> None:
        pair = mv.CryptoLeadLagPair("lighter_robinhood", "ETH", "binance", "coinbase")
        ref_books = []
        tgt_books = []
        conf_books = []
        for k in range(120):
            t = k * 250
            pulse = (40 <= k <= 44) or (80 <= k <= 84)
            catch = (43 <= k <= 46) or (83 <= k <= 86)
            r_mid = 100.10 if pulse else 100.00
            t_mid = 100.09 if catch else 100.00
            ref_books.append((t, r_mid - 0.005, 10.0, r_mid + 0.005, 10.0))
            conf_books.append((t, r_mid - 0.005, 10.0, r_mid + 0.005, 10.0))
            tgt_books.append((t, t_mid - 0.005, 10.0, t_mid + 0.005, 10.0))

        streams = {
            "w18": {"ref": ref_books, "tgt": tgt_books, "confirm": conf_books, "trades": [], "cont": True},
        }
        frozen, diag = mv._select_pair_frozen_params(pair, streams, warmup_ms=2_000)
        self.assertEqual(frozen["pair_id"], pair.pair_id)
        self.assertEqual(diag["baseline_grid_count"], 12)
        arm_cfgs = mv._arm_configs_from_frozen(pair, frozen, warmup_ms=2_000)
        self.assertEqual(tuple(arm_cfgs.keys()), mv.ARM_NAMES)
        self.assertEqual(arm_cfgs["B_vol_adaptive_theta"].vol_theta_mult, frozen["vol_theta_mult"])
        self.assertTrue(arm_cfgs["C_dual_leader"].require_dual_leader_confirm)
        self.assertEqual(arm_cfgs["D_catchup_exit"].exit_on_catchup_frac, frozen["exit_on_catchup_frac"])


if __name__ == "__main__":
    unittest.main()
