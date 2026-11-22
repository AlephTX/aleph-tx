#!/usr/bin/env python3
"""Segregated multi-day funding-carry sub-track evaluator & CLI wrapper (`S9-3`).

Maintains strict financial & operational isolation from Market-Making and Arbitrage:
  - Dedicated `capital_usd`, `max_loss_budget_usd`, and prorated fixed infra cost;
  - Causal pre-window frozen direction (`settled_at_ms <= decision_start_ms - lag`);
  - Continuous 10h (`36_000_000 ms`) checkpoints without inventory/budget resets;
  - Preserves missing witnessed funding slots as `funding_witness_complete = False`.
"""
import argparse
from decimal import Decimal, DecimalException, InvalidOperation, localcontext
import json
from pathlib import Path
import subprocess

from audit_lighter_program_scope import score_program_cost_effectiveness


def _finite_decimal(value, *, name, allow_negative=True) -> Decimal:
    try:
        dec = Decimal(str(value))
    except (InvalidOperation, ValueError) as exc:
        raise ValueError(f"{name} must be a valid decimal") from exc
    if not dec.is_finite() or (not allow_negative and dec < 0):
        raise ValueError(f"{name} must be finite" + (" and nonnegative" if not allow_negative else ""))
    return dec


def _cash_components_reconcile(report) -> bool:
    fields = (
        "price_pnl_usd", "witnessed_funding_cash_usd", "fees_usd",
        "capital_usd", "terminal_equity_usd", "net_variable_pnl_usd",
        "fixed_infra_usd", "net_all_in_pnl_usd",
    )
    try:
        price, funding, fees, capital, equity, variable, infra, all_in = (
            _finite_decimal(report[field], name=field) for field in fields
        )
        with localcontext() as context:
            # Compare at the source engine's precision without rounding stored cash.
            context.prec = 64
            magnitude = abs(capital) + abs(price) + abs(funding) + abs(fees) + abs(infra)
            # Rust Decimal has at least 28 significant digits and at most 28 decimal places.
            # Allow four arithmetic rounding steps, capped at one trillionth of a dollar.
            tolerance = min(Decimal("1e-12"), Decimal(4).scaleb(max(-28, magnitude.adjusted() - 27)))
            return (infra >= 0 and abs(price + funding - fees - variable) <= tolerance
                    and abs(capital + variable - equity) <= tolerance
                    and abs(variable - infra - all_in) <= tolerance)
    except (KeyError, ValueError, DecimalException):
        return False


def run_multiday_carry_binary(cross_venue_bin: Path, scenario_path: Path, *, program="zero_reward"):
    """Invoke `cross_venue multiday-carry FILE` and attach isolated sub-track cost effectiveness."""
    proc = subprocess.run(
        [str(cross_venue_bin), "multiday-carry", str(scenario_path)],
        check=True,
        capture_output=True,
        text=True,
    )
    report = json.loads(proc.stdout)
    report["program_cost_effectiveness"] = None
    if (
        report.get("cash_identity_reconciled") is True
        and report.get("status") in {"settled_witnessed", "no_signal", "blocked_entry_distance"}
        and _cash_components_reconcile(report)
    ):
        report["program_cost_effectiveness"] = score_program_cost_effectiveness(
            program=program,
            net_variable_pnl_quote=report["net_variable_pnl_usd"],
            fixed_infra_quote=report["fixed_infra_usd"],
        )
    return report


def check_carry_entry_distance(*, quote_distance_bps, partner_fee_bps, conditional_markout_bps, min_profit_bps):
    """Verify exact Decimal `entry_distance >= partner_fee_bps + conditional_markout_bps + min_profit_bps`."""
    observed = _finite_decimal(quote_distance_bps, name="quote_distance_bps")
    fee = _finite_decimal(partner_fee_bps, name="partner_fee_bps", allow_negative=False)
    markout = _finite_decimal(conditional_markout_bps, name="conditional_markout_bps", allow_negative=False)
    min_profit = _finite_decimal(min_profit_bps, name="min_profit_bps")
    required = fee + markout + min_profit
    return {
        "required_distance_bps": float(required),
        "observed_distance_bps": float(observed),
        "required_distance_bps_exact": str(required.normalize()),
        "observed_distance_bps_exact": str(observed.normalize()),
        "admitted": observed >= required,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("cross_venue_bin", type=Path)
    parser.add_argument("scenario_json", type=Path)
    parser.add_argument("--program", default="zero_reward")
    args = parser.parse_args()
    out = run_multiday_carry_binary(args.cross_venue_bin, args.scenario_json, program=args.program)
    print(json.dumps(out, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
