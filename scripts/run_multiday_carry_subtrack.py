#!/usr/bin/env python3
"""Segregated multi-day funding-carry sub-track evaluator & CLI wrapper (`S9-3`).

Maintains strict financial & operational isolation from Market-Making and Arbitrage:
  - Dedicated `capital_usd`, `max_loss_budget_usd`, and prorated fixed infra cost;
  - Causal pre-window frozen direction (`settled_at_ms <= decision_start_ms - lag`);
  - Continuous 10h (`36_000_000 ms`) checkpoints without inventory/budget resets;
  - Preserves missing witnessed funding slots as `funding_witness_complete = False`.
"""
import argparse
import json
from pathlib import Path
import subprocess

from audit_lighter_program_scope import score_program_cost_effectiveness


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
    if report["status"] in {"settled_witnessed", "no_signal", "blocked_entry_distance"}:
        report["program_cost_effectiveness"] = score_program_cost_effectiveness(
            program=program,
            net_variable_pnl_quote=report["net_variable_pnl_usd"],
            fixed_infra_quote=report["fixed_infra_usd"],
        )
    return report


def check_carry_entry_distance(*, quote_distance_bps, partner_fee_bps, conditional_markout_bps, min_profit_bps):
    """Verify `entry_distance >= partner_fee_bps + conditional_markout_bps + min_profit_bps`."""
    required = float(partner_fee_bps) + float(conditional_markout_bps) + float(min_profit_bps)
    return {
        "required_distance_bps": required,
        "observed_distance_bps": float(quote_distance_bps),
        "admitted": float(quote_distance_bps) >= required,
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
