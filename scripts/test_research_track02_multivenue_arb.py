#!/usr/bin/env python3
"""Stdlib-only unit tests for scripts/research_track02_multivenue_arb.py."""

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

import research_track02_multivenue_arb as mod  # noqa: E402


class TestResearchTrack02MultivenueArb(unittest.TestCase):
    def test_stdlib_only_top_level_imports_in_isolated_subprocess(self) -> None:
        blocker = "\n".join([
            "import sys",
            "for m in ('numpy','duckdb','pyarrow','pandas','scipy'):",
            "    sys.modules[m] = None",
            "import research_track02_multivenue_arb as m",
            "m.run_self_test()",
        ])
        env = dict(os.environ)
        env["PYTHONPATH"] = str(SCRIPT_DIR)
        proc = subprocess.run(
            [sys.executable, "-c", blocker],
            env=env,
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(proc.returncode, 0, msg=f"stderr={proc.stderr}\nstdout={proc.stdout}")
        self.assertIn('"self_test": "ok"', proc.stdout)

    def test_campaign_dynamic_discovery_cross_midnight_and_splits(self) -> None:
        self.assertEqual(mod.split_for_window(1), "full_dev")
        self.assertEqual(mod.split_for_window(20), "full_dev")
        self.assertEqual(mod.split_for_window(21), "holdout")
        self.assertEqual(mod.split_for_window(25), "holdout")
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            cid_w03 = "tokyo-crypto-20261116T101910Z-w03"
            cid_w08 = "tokyo-crypto-20261118T132948Z-w08"
            cid_w23 = "tokyo-crypto-20261124T231506Z-w23"
            for cid, dates in (
                (cid_w03, ("2026-11-16",)),
                (cid_w08, ("2026-11-18",)),
                (cid_w23, ("2026-11-24", "2026-11-25")),
            ):
                for d in dates:
                    p = (
                        root
                        / "parquet"
                        / "bbo_quotes"
                        / "collector_region=tokyo"
                        / "venue=lighter"
                        / f"utc_date={d}"
                        / f"campaign_id={cid}"
                    )
                    p.mkdir(parents=True, exist_ok=True)
                    (p / "part-0.parquet").write_bytes(b"")
            discovered = mod.discover_campaigns(root, "crypto")
            self.assertEqual([c["window"] for c in discovered], ["w03", "w23"])
            self.assertEqual([c["split"] for c in discovered], ["full_dev", "holdout"])
            files = mod.campaign_parquet_files(root, "bbo_quotes", "lighter", "2026-11-24", cid_w23)
            self.assertEqual(len(files), 2)
            self.assertTrue(any("utc_date=2026-11-24" in f for f in files))
            self.assertTrue(any("utc_date=2026-11-25" in f for f in files))

    def test_regime_stratification_and_tail_risk_curve(self) -> None:
        reg_map = mod.assign_window_regimes({"w01": 1.0, "w02": 1.6, "w03": 9.2, "w04": 1.5, "w05": 1.1})
        self.assertEqual(reg_map["w03"], "extreme_crash")
        self.assertEqual(reg_map["w01"], "calm")
        tail = mod.compute_regime_and_tail_metrics([
            {"window": "w01", "window_num": 1, "regime": "calm", "vol_1s_bps": 1.0, "span_hours": 10.2, "fills": 10, "maker_notional_usd": 10_000.0, "quote_hedged_pnl_usd": 12.0, "print_verified_pnl_usd": 11.0, "funding_carry_usd": 0.5},
            {"window": "w02", "window_num": 2, "regime": "normal", "vol_1s_bps": 1.6, "span_hours": 10.2, "fills": 15, "maker_notional_usd": 15_000.0, "quote_hedged_pnl_usd": -5.0, "print_verified_pnl_usd": -4.5, "funding_carry_usd": 0.2},
            {"window": "w03", "window_num": 3, "regime": "extreme_crash", "vol_1s_bps": 9.2, "span_hours": 10.2, "fills": 25, "maker_notional_usd": 25_000.0, "quote_hedged_pnl_usd": -8.0, "print_verified_pnl_usd": -7.5, "funding_carry_usd": 0.1},
        ])
        self.assertEqual(tail["windows_covered"], 3)
        self.assertAlmostEqual(tail["positive_window_ratio"], 1 / 3, places=3)
        self.assertEqual(tail["worst_10h_window"]["window"], "w03")
        self.assertEqual(tail["max_drawdown_usd"], 13.0)
        self.assertEqual(tail["cumulative_curve"][-1]["cum_pnl_usd"], -1.0)
        self.assertEqual(set(tail["by_regime"].keys()), {"calm", "normal", "extreme_crash"})

    def test_cohort_separation_and_flags(self) -> None:
        catalog = mod.build_pair_catalog()
        usdc = [p for p in catalog if p.cohort == "USDC/USDC"]
        usdg = [p for p in catalog if p.cohort == "USDG/USDC"]
        self.assertTrue(usdc)
        self.assertTrue(usdg)
        for p in usdc:
            self.assertFalse(p.cross_stablecoin)
        for p in usdg:
            self.assertTrue(p.cross_stablecoin)
        same_dom = [p for p in usdg if p.same_risk_domain]
        self.assertTrue(all({p.maker_venue, p.hedge_venue} == {"lighter_robinhood", "lighter"} for p in same_dom))
        diag = [p for p in catalog if p.diagnostic_polled_venue]
        self.assertTrue(all(p.hedge_venue == "trade_xyz" or p.maker_venue == "trade_xyz" for p in diag))

    def test_causal_series_stale_preservation_and_vol_tracking(self) -> None:
        n = 60
        mk_bid = [99.99] * n
        mk_ask = [100.01] * n
        mk_age = [0] * n
        hd_bid = [99.99] * n
        hd_ask = [100.01] * n
        hd_age = [0] * n
        for k in range(5, 30):
            if k % 2 == 0:
                mk_bid[k], mk_ask[k] = 100.09, 100.11
            else:
                mk_bid[k], mk_ask[k] = 99.89, 99.91
        # Inject stale burst at k=35..40 with extreme maker spike: must be ignored.
        for k in range(35, 41):
            mk_bid[k], mk_ask[k] = 120.0, 120.02
            hd_age[k] = 100  # > max_age_steps=50
        c = mod.compute_causal_basis_series(mk_bid, mk_ask, mk_age, hd_bid, hd_ask, hd_age, vol_tau_ms=1_000)
        self.assertGreater(c["prior_sigma_bps"][31], 5.0)
        # Fair basis after stale hedge burst must remain near 0, not pulled to log(1.2).
        self.assertLess(abs(c["prior_fair"][42]), 0.005)

    def test_l1_depth_slippage_cancel_shield_fx_sensitivity_and_funding_carry(self) -> None:
        n = 150
        mk_bid = [99.99] * n
        mk_ask = [100.01] * n
        mk_age = [0] * n
        hd_bid = [99.99] * n
        hd_ask = [100.01] * n
        hd_bsz = [5.0] * n  # ~$500 L1 (< $1000 clip -> triggers depth slip when unguarded)
        hd_asz = [20.0] * n
        hd_age = [0] * n
        ref_mid = [100.0] * n
        ref_age = [0] * n

        # Toxic reference dump inside Lighter speedbump window [tf-300+40, tf-40] for trade at k=70.
        tf_toxic = 70 * 100 + 100 + 40
        ref_mid[69] = 99.94
        ref_mid[70] = 99.94  # -6 bps dump at i1 = _ref_idx(tf_toxic - 40) = 70 vs i0 = 67
        # Clean trade at k=90 (no ref move).
        tf_clean = 90 * 100 + 100 + 40

        mk_trades = {
            "ts": [tf_toxic, tf_clean],
            "sgn": [-1, -1],
            "px": [99.90, 99.90],
            "sz": [15.0, 15.0],
        }
        hd_trades = {
            "ts": [tf_clean + 40],
            "sgn": [-1],
            "px": [99.98],
            "sz": [15.0],
        }
        causal = mod.compute_causal_basis_series(mk_bid, mk_ask, mk_age, hd_bid, hd_ask, hd_age)

        # Shield enabled blocks toxic fill at k=70; unguarded L1 incurs depth_slip_usd at k=90.
        p_sh = mod.StrategyParams(name="sh", e0_bps=1.0, shield_bps=2.5, min_hedge_l1_notional_usd=0.0)
        r_fx0 = mod.simulate_pair_arb(
            mk_bid=mk_bid, mk_ask=mk_ask, mk_age=mk_age,
            hd_bid=hd_bid, hd_ask=hd_ask, hd_bid_sz=hd_bsz, hd_ask_sz=hd_asz, hd_age=hd_age,
            ref_mid=ref_mid, ref_age=ref_age, causal=causal, mk_trades=mk_trades, hd_trades=hd_trades,
            t0_ms=0, tick=0.01, params=p_sh, maker_delay_ms=40, hedge_delay_ms=40,
            speedbump_ms=300, rtt_ms=40, maker_fee_bps=0.0, maker_taker_fee_bps=0.0,
            hedge_fee_bps=1.15, warmup_steps=50, decision_steps=90, cross_stablecoin_bps=0.0,
            maker_fr_bps_hr=-0.5, hedge_fr_bps_hr=0.5,
        )
        self.assertEqual(r_fx0["blocked_toxic"], 1)
        self.assertEqual(r_fx0["fills"], 1)
        self.assertGreater(r_fx0["depth_slip_usd"], 0.0)
        self.assertIsNotNone(r_fx0["print_verified_pnl_usd"])
        self.assertGreater(r_fx0["funding_carry_usd"], 0.0)

        # FX drag 2bp strictly lowers quote_hedged_pnl_usd vs 0bp.
        r_fx2 = mod.simulate_pair_arb(
            mk_bid=mk_bid, mk_ask=mk_ask, mk_age=mk_age,
            hd_bid=hd_bid, hd_ask=hd_ask, hd_bid_sz=hd_bsz, hd_ask_sz=hd_asz, hd_age=hd_age,
            ref_mid=ref_mid, ref_age=ref_age, causal=causal, mk_trades=mk_trades, hd_trades=hd_trades,
            t0_ms=0, tick=0.01, params=p_sh, maker_delay_ms=40, hedge_delay_ms=40,
            speedbump_ms=300, rtt_ms=40, maker_fee_bps=0.0, maker_taker_fee_bps=0.0,
            hedge_fee_bps=1.15, warmup_steps=50, decision_steps=90, cross_stablecoin_bps=2.0,
        )
        self.assertAlmostEqual(r_fx2["fx_drag_usd"], 0.2, places=3)
        self.assertLess(r_fx2["quote_hedged_pnl_usd"], r_fx0["quote_hedged_pnl_usd"])

        # L1 guard blocks thin hedge bid at k=90 when min_hedge_l1_notional_usd = 1000.
        p_l1 = mod.StrategyParams(name="l1", e0_bps=1.0, shield_bps=2.5, min_hedge_l1_notional_usd=1_000.0)
        r_l1 = mod.simulate_pair_arb(
            mk_bid=mk_bid, mk_ask=mk_ask, mk_age=mk_age,
            hd_bid=hd_bid, hd_ask=hd_ask, hd_bid_sz=hd_bsz, hd_ask_sz=hd_asz, hd_age=hd_age,
            ref_mid=ref_mid, ref_age=ref_age, causal=causal, mk_trades=mk_trades, hd_trades=hd_trades,
            t0_ms=0, tick=0.01, params=p_l1, maker_delay_ms=40, hedge_delay_ms=40,
            speedbump_ms=300, rtt_ms=40, maker_fee_bps=0.0, maker_taker_fee_bps=0.0,
            hedge_fee_bps=1.15, warmup_steps=50, decision_steps=90,
        )
        self.assertEqual(r_l1["blocked_l1_thin"], 2)
        self.assertEqual(r_l1["fills"], 0)


if __name__ == "__main__":
    unittest.main()
