#!/usr/bin/env python3
"""Unit tests for eval_lighter_rh_incentive_guard.py (BARBELL §6.4 Gate 4)."""

from decimal import Decimal
import json
from pathlib import Path
import subprocess
import sys
import unittest

try:
    from scripts.eval_lighter_rh_incentive_guard import (
        ACTION_CONTINUE,
        ACTION_HALT,
        ACTION_REDUCE_ONLY,
        _base_healthy_payload,
        evaluate_lighter_rh_incentive,
    )
except ImportError:
    from eval_lighter_rh_incentive_guard import (
        ACTION_CONTINUE,
        ACTION_HALT,
        ACTION_REDUCE_ONLY,
        _base_healthy_payload,
        evaluate_lighter_rh_incentive,
    )


class LighterRhIncentiveGuardTests(unittest.TestCase):
    def test_healthy_plus_account_admits_farming_and_keeps_points_zero_cash(self) -> None:
        res = evaluate_lighter_rh_incentive(_base_healthy_payload())
        self.assertEqual(res["decision_action"], ACTION_CONTINUE)
        self.assertTrue(res["allow_new_farming"])
        self.assertEqual(res["reasons"], [])
        self.assertEqual(res["cash_ledger"]["unredeemed_points_cash_value_usd"], "0")
        self.assertEqual(res["cash_ledger"]["realized_cash_pnl_usd"], "-250")
        self.assertEqual(res["cash_ledger"]["weekly_cash_cost_usd"], "250")
        self.assertEqual(res["cash_ledger"]["cumulative_cash_loss_usd"], "650")
        self.assertEqual(res["cash_ledger"]["remaining_loss_budget_usd"], "1350")
        self.assertEqual(
            res["shadow_incentive_valuation"]["valuation_basis"],
            "shadow_hypothesis_not_cash",
        )
        self.assertEqual(
            res["shadow_incentive_valuation"]["implied_cost_per_point_usd"], "0.25"
        )
        self.assertEqual(
            Decimal(res["shadow_incentive_valuation"]["shadow_points_value_usd"]),
            Decimal("542"),
        )
        self.assertEqual(
            Decimal(res["shadow_incentive_valuation"]["shadow_coverage_ratio"]),
            Decimal("2.168"),
        )

    def test_unverified_tier_or_standard_pair_halts(self) -> None:
        p_unverified = _base_healthy_payload()
        p_unverified["account_tiers"]["rh_fee_status"] = "unknown_requires_account_evidence"
        res_u = evaluate_lighter_rh_incentive(p_unverified)
        self.assertEqual(res_u["decision_action"], ACTION_HALT)
        self.assertFalse(res_u["allow_new_farming"])
        self.assertIn("unverified_account_tier", res_u["reasons"])

        p_std = _base_healthy_payload()
        p_std["account_tiers"]["mainnet_tier"] = "standard"
        p_std["account_tiers"]["rh_tier"] = "Standard"
        res_s = evaluate_lighter_rh_incentive(p_std)
        self.assertEqual(res_s["decision_action"], ACTION_HALT)
        self.assertIn("zero_point_standard_pair", res_s["reasons"])

    def test_cross_stablecoin_authorization_quorum_and_depeg_thresholds(self) -> None:
        now_ms = 1_764_000_000_000

        # Missing explicit cross-stablecoin authorization -> HALT
        p_unauth = _base_healthy_payload(now_ms)
        p_unauth["stablecoin_guard"]["cross_stablecoin_authorized"] = False
        r_unauth = evaluate_lighter_rh_incentive(p_unauth)
        self.assertEqual(r_unauth["decision_action"], ACTION_HALT)
        self.assertIn("cross_stablecoin_unauthorized", r_unauth["reasons"])

        # Duplicate single source OR stale/future observation breaks 2-source quorum -> HALT
        p_dup = _base_healthy_payload(now_ms)
        p_dup["stablecoin_guard"]["peg_observations"][1]["source"] = "pyth"
        r_dup = evaluate_lighter_rh_incentive(p_dup)
        self.assertEqual(r_dup["decision_action"], ACTION_HALT)
        self.assertEqual(r_dup["gate_diagnostics"]["peg_quorum_count"], 1)
        self.assertIn("depeg_oracle_quorum_unavailable", r_dup["reasons"])

        p_stale = _base_healthy_payload(now_ms)
        p_stale["stablecoin_guard"]["peg_observations"][1]["observed_at_ms"] = (
            now_ms - 60_001
        )
        r_stale = evaluate_lighter_rh_incentive(p_stale)
        self.assertEqual(r_stale["decision_action"], ACTION_HALT)
        self.assertEqual(r_stale["gate_diagnostics"]["rejected_peg_observations"], 1)
        self.assertIn("depeg_oracle_quorum_unavailable", r_stale["reasons"])

        p_future = _base_healthy_payload(now_ms)
        p_future["stablecoin_guard"]["peg_observations"][0]["observed_at_ms"] = (
            now_ms + 5_001
        )
        r_future = evaluate_lighter_rh_incentive(p_future)
        self.assertEqual(r_future["decision_action"], ACTION_HALT)
        self.assertEqual(r_future["gate_diagnostics"]["rejected_peg_observations"], 1)
        self.assertIn("depeg_oracle_quorum_unavailable", r_future["reasons"])

        # >= 30 bp -> REDUCE_ONLY; >= 75 bp -> HALT
        p_30 = _base_healthy_payload(now_ms)
        p_30["stablecoin_guard"]["peg_observations"][0]["usdc_usdg_price"] = "1.0030"
        r_30 = evaluate_lighter_rh_incentive(p_30)
        self.assertEqual(r_30["decision_action"], ACTION_REDUCE_ONLY)
        self.assertIn("usdc_usdg_depeg_reduce_only", r_30["reasons"])

        p_75 = _base_healthy_payload(now_ms)
        p_75["stablecoin_guard"]["peg_observations"][1]["usdc_usdg_price"] = "0.9925"
        r_75 = evaluate_lighter_rh_incentive(p_75)
        self.assertEqual(r_75["decision_action"], ACTION_HALT)
        self.assertIn("usdc_usdg_depeg_halt", r_75["reasons"])

    def test_net_delta_and_loss_budget_gates(self) -> None:
        p_delta_ro = _base_healthy_payload()
        p_delta_ro["position_and_budget"]["net_delta_ratio"] = "-0.005"
        r_dro = evaluate_lighter_rh_incentive(p_delta_ro)
        self.assertEqual(r_dro["decision_action"], ACTION_REDUCE_ONLY)
        self.assertIn("net_delta_reduce_only", r_dro["reasons"])

        p_delta_halt = _base_healthy_payload()
        p_delta_halt["position_and_budget"]["net_delta_ratio"] = "0.015"
        r_dh = evaluate_lighter_rh_incentive(p_delta_halt)
        self.assertEqual(r_dh["decision_action"], ACTION_HALT)
        self.assertIn("net_delta_halt_breach", r_dh["reasons"])

        p_budget = _base_healthy_payload()
        p_budget["position_and_budget"]["prior_cumulative_cash_loss_usd"] = "1750"
        r_budget = evaluate_lighter_rh_incentive(p_budget)
        self.assertEqual(r_budget["decision_action"], ACTION_HALT)
        self.assertEqual(r_budget["cash_ledger"]["remaining_loss_budget_usd"], "0")
        self.assertIn("loss_budget_exhausted", r_budget["reasons"])

    def test_friday_roi_kill_switch_zero_points_and_cash_positive_week(self) -> None:
        # Unannounced redemption rate always halts fail-closed
        p_none = _base_healthy_payload()
        p_none["friday_settlement"]["lit_per_point"] = None
        p_none["friday_settlement"].pop("redemption_rate_source")
        r_none = evaluate_lighter_rh_incentive(p_none)
        self.assertEqual(r_none["decision_action"], ACTION_HALT)
        self.assertIn("unannounced_redemption_rate", r_none["reasons"])

        # Zero points while burning positive cash cost -> HALT
        p_zero = _base_healthy_payload()
        p_zero["friday_settlement"]["points_received"] = "0"
        r_zero = evaluate_lighter_rh_incentive(p_zero)
        self.assertEqual(r_zero["decision_action"], ACTION_HALT)
        self.assertIn("zero_points_with_positive_cash_cost", r_zero["reasons"])

        # Cash-positive week with announced official rate continues cleanly
        p_pos = _base_healthy_payload()
        p_pos["weekly_cash_ledger"]["trading_spread_pnl_usd"] = "500"
        p_pos["friday_settlement"]["redemption_rate_source"] = "official"
        r_pos = evaluate_lighter_rh_incentive(p_pos)
        self.assertEqual(r_pos["decision_action"], ACTION_CONTINUE)
        self.assertEqual(r_pos["cash_ledger"]["realized_cash_pnl_usd"], "130")
        self.assertEqual(r_pos["cash_ledger"]["weekly_cash_cost_usd"], "0")
        self.assertEqual(
            r_pos["shadow_incentive_valuation"]["redemption_rate_source"], "official"
        )
        self.assertEqual(
            r_pos["shadow_incentive_valuation"]["implied_cost_per_point_usd"], "0"
        )

    def test_rejects_malformed_nan_bool_and_runs_cli_self_test(self) -> None:
        for bad_val in (True, "NaN", "Inf", "-Inf", float("nan"), float("inf")):
            p_bad = _base_healthy_payload()
            p_bad["friday_settlement"]["lit_price_usd"] = bad_val
            with self.assertRaises(ValueError):
                evaluate_lighter_rh_incentive(p_bad)

        p_bad_source = _base_healthy_payload()
        p_bad_source["friday_settlement"]["redemption_rate_source"] = "invented_source"
        with self.assertRaises(ValueError):
            evaluate_lighter_rh_incentive(p_bad_source)

        script_path = Path(__file__).resolve().with_name(
            "eval_lighter_rh_incentive_guard.py"
        )
        proc = subprocess.run(
            [sys.executable, str(script_path), "--self-test"],
            check=True,
            capture_output=True,
            text=True,
        )
        out = json.loads(proc.stdout)
        self.assertEqual(out["self_test"], "ok")
        self.assertEqual(out["depeg_35bp_action"], ACTION_REDUCE_ONLY)
        self.assertEqual(out["depeg_80bp_action"], ACTION_HALT)

    def test_case_alias_quorum_single_source_depeg_and_zero_cost_roi_hurdle_fail_closed(
        self,
    ) -> None:
        now_ms = 1_764_000_000_000

        # Same oracle differing only by case cannot fabricate a 2-source quorum, and
        # any single surviving fresh depeg breaching 75 bp is still surfaced diagnostically.
        p_alias = _base_healthy_payload(now_ms)
        p_alias["stablecoin_guard"]["peg_observations"] = [
            {
                "source": "pyth",
                "usdc_usdg_price": "1.0002",
                "observed_at_ms": now_ms - 1_000,
            },
            {
                "source": "Pyth",
                "usdc_usdg_price": "0.9910",
                "observed_at_ms": now_ms - 500,
            },
        ]
        r_alias = evaluate_lighter_rh_incentive(p_alias)
        self.assertEqual(r_alias["decision_action"], ACTION_HALT)
        self.assertEqual(r_alias["gate_diagnostics"]["peg_quorum_count"], 1)
        self.assertEqual(r_alias["gate_diagnostics"]["peg_quorum_sources"], ["pyth"])
        self.assertEqual(Decimal(r_alias["gate_diagnostics"]["max_abs_deviation_bps"]), Decimal("90"))
        self.assertIn("depeg_oracle_quorum_unavailable", r_alias["reasons"])
        self.assertIn("usdc_usdg_depeg_halt", r_alias["reasons"])

        # Zero weekly cash loss cannot bypass a positive minimum annualized ROI hurdle.
        p_zero_cost_low_roi = _base_healthy_payload(now_ms)
        p_zero_cost_low_roi["weekly_cash_ledger"]["trading_spread_pnl_usd"] = "370"
        p_zero_cost_low_roi["friday_settlement"]["points_received"] = "10"
        p_zero_cost_low_roi["friday_settlement"]["min_annualized_roi_pct"] = "10"
        r_low_roi = evaluate_lighter_rh_incentive(p_zero_cost_low_roi)
        self.assertEqual(r_low_roi["cash_ledger"]["weekly_cash_cost_usd"], "0")
        self.assertEqual(r_low_roi["decision_action"], ACTION_HALT)
        self.assertFalse(r_low_roi["allow_new_farming"])
        self.assertIn("friday_roi_below_safety_cushion", r_low_roi["reasons"])

        p_zero_cost_ok_roi = _base_healthy_payload(now_ms)
        p_zero_cost_ok_roi["weekly_cash_ledger"]["trading_spread_pnl_usd"] = "370"
        p_zero_cost_ok_roi["friday_settlement"]["points_received"] = "1000"
        p_zero_cost_ok_roi["friday_settlement"]["min_annualized_roi_pct"] = "10"
        r_ok_roi = evaluate_lighter_rh_incentive(p_zero_cost_ok_roi)
        self.assertEqual(r_ok_roi["decision_action"], ACTION_CONTINUE)
        self.assertTrue(r_ok_roi["allow_new_farming"])
        self.assertGreaterEqual(
            Decimal(r_ok_roi["shadow_incentive_valuation"]["annualized_shadow_roi_pct"]),
            Decimal("10"),
        )


if __name__ == "__main__":
    unittest.main()
