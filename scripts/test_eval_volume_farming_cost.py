#!/usr/bin/env python3
"""Stdlib-only unit tests for `scripts/eval_volume_farming_cost.py`."""

from __future__ import annotations

import json
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

import eval_volume_farming_cost as vf  # noqa: E402


class VolumeFarmingCostTests(unittest.TestCase):
    def test_cost_model_includes_nonzero_maker_fee_and_rebates(self) -> None:
        # With maker_fee=0 and rebate=0, double-sided USD/M reduces to (taker * s - gross) / 2 * 100.
        bd_zero_mk = vf.compute_volume_farming_cost(
            gross_ex_fee_bps=-0.4057,
            taker_exit_share_s=0.1845,
            taker_fee_bps=1.12,
            maker_fee_bps=0.0,
        )
        self.assertAlmostEqual(bd_zero_mk.double_sided_usd_per_m, 30.617, places=2)
        self.assertIsNone(bd_zero_mk.qualified_usd_per_m)

        # Real Hyperliquid verified Tier 0 has maker_fee_bps=1.5 across (2 - s) maker legs.
        bd_hl = vf.compute_volume_farming_cost(
            gross_ex_fee_bps=-0.4057,
            taker_exit_share_s=0.1845,
            taker_fee_bps=4.5,
            maker_fee_bps=1.5,
        )
        self.assertAlmostEqual(bd_hl.double_sided_usd_per_m, 197.96, places=4)
        self.assertAlmostEqual(bd_hl.s_zero_double_sided_floor_usd_per_m, 170.285, places=4)

        # Positive rebate across passive (2 - s) maker legs lowers cost cleanly.
        bd_rebate = vf.compute_volume_farming_cost(
            gross_ex_fee_bps=-0.4057,
            taker_exit_share_s=0.1845,
            taker_fee_bps=1.12,
            maker_fee_bps=0.0,
            rebate_bps=0.5,
        )
        expected_rebate = (-(0.5) * (2.0 - 0.1845) + 1.12 * 0.1845 + 0.4057) / 2.0 * 100.0
        self.assertAlmostEqual(bd_rebate.double_sided_usd_per_m, expected_rebate, places=3)

    def test_maker_only_vs_double_sided_and_explicit_funding_separation(self) -> None:
        # 100% taker exit: 1 maker entry + 1 taker exit => maker denom = 1.0, double denom = 2.0.
        bd = vf.compute_volume_farming_cost(
            gross_ex_fee_bps=0.0,
            taker_exit_share_s=1.0,
            taker_fee_bps=3.5,
            maker_fee_bps=0.0,
            funding_cash_bps=0.5,
            qualified_volume_ratio=1.0,
        )
        self.assertAlmostEqual(bd.double_sided_usd_per_m, 150.0, places=4)
        self.assertAlmostEqual(bd.maker_volume_usd_per_m, 300.0, places=4)
        self.assertAlmostEqual(bd.qualified_usd_per_m, 300.0, places=4)

    def test_rejects_bool_nan_and_out_of_range_inputs(self) -> None:
        with self.assertRaises(ValueError):
            vf.compute_volume_farming_cost(True, 0.2, 1.0)  # type: ignore[arg-type]
        with self.assertRaises(ValueError):
            vf.compute_volume_farming_cost(float("nan"), 0.2, 1.0)
        with self.assertRaises(ValueError):
            vf.compute_volume_farming_cost(-0.4, 1.05, 1.0)
        with self.assertRaises(ValueError):
            vf.compute_volume_farming_cost(-0.4, 0.2, -0.1)

    def test_load_fee_evidence_and_evaluate_synthetic_bookdiff_and_track3(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            fee_path = root / "fee.json"
            fee_path.write_text(
                json.dumps(
                    {
                        "venues": {
                            "arcus": {
                                "account_fee_status": "public_tier0_schedule_only",
                                "public_tier0_schedule_summary": {
                                    "tier0_fee_profiles": [
                                        {"maker_fee_bps": "0", "taker_fee_bps": "2.25"},
                                        {"maker_fee_bps": "1.5", "taker_fee_bps": "4.5"},
                                    ],
                                    "custom_schedule_tier0_bps": {
                                        "ANT-USD": {"maker_fee_bps": "0", "taker_fee_bps": "1.12"}
                                    },
                                },
                            },
                            "hyperliquid": {
                                "account_fee_status": "verified_account_tier",
                                "maker_fee_bps": "1.5",
                                "taker_fee_bps": "4.5",
                            },
                        }
                    }
                ),
                encoding="utf-8",
            )
            bd_path = root / "bookdiff.json"
            bd_path.write_text(
                json.dumps(
                    {
                        "pipeline": [
                            {
                                "win": "w21",
                                "sym": "BTC_core_hl",
                                "arm": "S1_markout",
                                "cont": True,
                                "net": -1.0516,
                                "spr": -0.10,
                                "drft": -0.20,
                                "pex": -0.05,
                                "tslp": -0.0557,
                                "ntl": 10000.0,
                            },
                            {
                                "win": "w21",
                                "sym": "BTC_core_hl",
                                "arm": "S1_markout",
                                "cont": False,
                                "net": -999.0,
                                "spr": 0.0,
                                "drft": 0.0,
                                "pex": 0.0,
                                "tslp": 0.0,
                                "ntl": 10000.0,
                            },
                        ]
                    }
                ),
                encoding="utf-8",
            )
            mv_path = root / "mv.json"
            mv_path.write_text(
                json.dumps(
                    {
                        "split": "holdout_w25",
                        "track1_quant_profit": [
                            {"venue": "arcus", "symbol": "DRAM", "half_spread_minus_fee_bps": 1.417}
                        ],
                        "track3_incentive_turnover": [
                            {
                                "venue": "arcus",
                                "symbol": "SNDK",
                                "maker_fee_bps": 0.0,
                                "fee_audit_status": "public_tier0_schedule_only",
                                "continuity": {"hours": 10.25},
                                "arms": [
                                    {
                                        "arm": "F_v4_adaptive_shield",
                                        "fill_evidence_mode": "public_trades",
                                        "inferred_book_diff_sweeps": 0,
                                        "fills": 368,
                                        "maker_volume_usd": 41588.0,
                                        "spread_capture_bps": 0.695,
                                        "markouts": {"1s_bps": -3.0323},
                                        "equity_usd": -4.20,
                                        "cost_per_m_usd": 101.10,
                                        "combined_cost_per_m_usd": 101.10,
                                    }
                                ],
                            }
                        ],
                    }
                ),
                encoding="utf-8",
            )

            profiles = vf.load_fee_profiles_from_evidence(fee_path)
            self.assertEqual([p.key for p in profiles], ["arcus_ant_tier0", "arcus_rwa_tier0", "arcus_crypto_tier0", "hyperliquid_verified"])

            res = vf.evaluate_bookdiff_arms(bd_path, profiles)
            arm = res["arms"]["S1_markout"]
            self.assertEqual(arm["rows"], 1)  # cont=False excluded
            self.assertFalse(arm["funding_and_fees_separated"])
            self.assertAlmostEqual(arm["gross_ex_fee_bps"], -0.4057, places=4)
            self.assertAlmostEqual(arm["inferred_taker_exit_share_s"], 0.184543, places=4)
            self.assertAlmostEqual(arm["venues"]["arcus_ant_tier0"]["double_sided_usd_per_m"], 30.6194, places=2)
            self.assertAlmostEqual(arm["venues"]["hyperliquid_verified"]["double_sided_usd_per_m"], 197.965, places=2)

            mv = vf.summarize_multivenue_track3([mv_path])
            w25 = mv["windows"]["holdout_w25"]
            self.assertFalse(w25["dram_present_in_track3"])
            self.assertEqual(len(w25["rwa_track3_entries"]), 1)
            self.assertEqual(w25["rwa_track3_entries"][0]["arms"][0]["markouts"]["1s_bps"], -3.0323)

            self.assertEqual(vf.run_self_test()["self_test"], "ok")


if __name__ == "__main__":
    unittest.main()
