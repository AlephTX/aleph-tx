import json
from pathlib import Path
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import patch

from run_multiday_carry_subtrack import check_carry_entry_distance, run_multiday_carry_binary


class RunMultidayCarrySubtrackTest(unittest.TestCase):
    def test_incomplete_evidence_withholds_program_score(self):
        for status in ("incomplete_missing_witness", "incomplete_missing_checkpoint"):
            with self.subTest(status=status), patch("subprocess.run") as invoke:
                invoke.return_value.stdout = json.dumps({
                    "status": status, "net_variable_pnl_usd": "5.00", "fixed_infra_usd": "1.00"
                })
                result = run_multiday_carry_binary(Path("cross_venue"), Path("scenario.json"))
                self.assertIsNone(result["program_cost_effectiveness"])

    def test_entry_distance_formula_matches_fee_markout_min_profit(self):
        ok = check_carry_entry_distance(
            quote_distance_bps=4.5,
            partner_fee_bps=2.0,
            conditional_markout_bps=1.5,
            min_profit_bps=1.0,
        )
        self.assertTrue(ok["admitted"])
        self.assertAlmostEqual(ok["required_distance_bps"], 4.5)

        rej = check_carry_entry_distance(
            quote_distance_bps=4.4,
            partner_fee_bps=2.0,
            conditional_markout_bps=1.5,
            min_profit_bps=1.0,
        )
        self.assertFalse(rej["admitted"])

        # Exact Decimal threshold avoids binary-float 0.1 + 0.1 + 0.1 = 0.30000000000000004 drift.
        exact = check_carry_entry_distance(
            quote_distance_bps="0.3",
            partner_fee_bps="0.1",
            conditional_markout_bps="0.1",
            min_profit_bps="0.1",
        )
        self.assertTrue(exact["admitted"])
        self.assertEqual(exact["required_distance_bps_exact"], "0.3")

        for bad_kwargs in (
            dict(quote_distance_bps="1.0", partner_fee_bps="-0.1", conditional_markout_bps="0", min_profit_bps="0"),
            dict(quote_distance_bps="NaN", partner_fee_bps="0", conditional_markout_bps="0", min_profit_bps="0"),
        ):
            with self.assertRaises(ValueError):
                check_carry_entry_distance(**bad_kwargs)

    def test_cash_identity_unreconciled_withholds_program_score(self):
        with patch("run_multiday_carry_subtrack.subprocess.run") as invoke:
            invoke.return_value.stdout = json.dumps({
                "status": "settled_witnessed",
                "net_variable_pnl_usd": "5.00",
                "fixed_infra_usd": "1.00",
                "cash_identity_reconciled": False,
            })
            result = run_multiday_carry_binary(Path("cross_venue"), Path("scenario.json"))
            self.assertIsNone(result["program_cost_effectiveness"])

    def test_declared_reconciliation_requires_matching_cash_components(self):
        report = {
            "status": "settled_witnessed",
            "cash_identity_reconciled": True,
            "capital_usd": "100.00",
            "terminal_equity_usd": "103.00",
            "price_pnl_usd": "1.00",
            "witnessed_funding_cash_usd": "2.50",
            "fees_usd": "0.50",
            "net_variable_pnl_usd": "3.00",
            "fixed_infra_usd": "1.00",
            "net_all_in_pnl_usd": "2.00",
        }
        for field, faulty in (
            ("witnessed_funding_cash_usd", "1.50"),
            ("terminal_equity_usd", "103.01"),
            ("net_all_in_pnl_usd", "4.00"),
            ("fees_usd", None),
        ):
            with self.subTest(field=field), patch("run_multiday_carry_subtrack.subprocess.run") as invoke:
                bad = report.copy()
                if faulty is None:
                    del bad[field]
                else:
                    bad[field] = faulty
                invoke.return_value.stdout = json.dumps(bad)
                result = run_multiday_carry_binary(Path("cross_venue"), Path("scenario.json"))
                self.assertIsNone(result["program_cost_effectiveness"])

        with patch("run_multiday_carry_subtrack.subprocess.run") as invoke:
            # Adding capital can round the low digits of a Rust Decimal cash component.
            rounded = {**report, "witnessed_funding_cash_usd": "2.5000000000000000000000000012"}
            invoke.return_value.stdout = json.dumps(rounded)
            result = run_multiday_carry_binary(Path("cross_venue"), Path("scenario.json"))
            self.assertIsNotNone(result["program_cost_effectiveness"])

    def test_wrapper_invokes_cross_venue_multiday_carry_and_attaches_budget_score(self):
        fake_stdout = json.dumps({
            "subtrack": "multiday_funding_carry_isolated",
            "market": "SOL",
            "legs": ["lighter", "lighter_robinhood"],
            "status": "settled_witnessed",
            "net_variable_pnl_usd": "3.50",
            "fixed_infra_usd": "2.0833333333",
            "net_all_in_pnl_usd": "1.4166666667",
            "capital_usd": "100.00",
            "terminal_equity_usd": "103.50",
            "price_pnl_usd": "1.00",
            "witnessed_funding_cash_usd": "3.00",
            "fees_usd": "0.50",
            "cash_identity_reconciled": True,
        })

        class FakeProc:
            stdout = fake_stdout

        with TemporaryDirectory() as tmp:
            sc = Path(tmp) / "carry.json"
            sc.write_text("{}")
            with patch("run_multiday_carry_subtrack.subprocess.run", return_value=FakeProc()) as m:
                out = run_multiday_carry_binary(Path("/bin/cross_venue"), sc)
        m.assert_called_once()
        self.assertTrue(out["program_cost_effectiveness"]["surplus_with_positive_pnl"])
        self.assertAlmostEqual(float(out["program_cost_effectiveness"]["all_in_pnl_quote"]), 3.50 - 2.0833333333, places=6)


if __name__ == "__main__":
    unittest.main()
