#!/usr/bin/env python3
"""Sensitivity & feasibility preset wrapper for Scenario 2 Arcus Maker Rebates (T5/T6).

Delegates all underlying schedule, taker-fee cap, leverage, and finite-input validation
math to the venue-agnostic [`eval_venue_maker_rebate`] module with preset
`arcus_2026_11_18`.
"""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import sys
from typing import Sequence

sys.path.insert(0, str(Path(__file__).resolve().parent))

from eval_venue_maker_rebate import (  # noqa: E402
    BUILTIN_SCHEDULES,
    CounterpartyFillCohort,
    DEFAULT_COHORTS,
    DEFAULT_VENUE_DAILY_VOLUMES_USD,
    DEFAULT_WEAR_COSTS_PER_MILLION_USD,
    MAX_ACCOUNT_LEVERAGE,
    compute_effective_rebate_bps,
    evaluate_venue_rebate_matrix,
    load_cohorts_from_json,
)

ARCUS_SCHEDULE = BUILTIN_SCHEDULES["arcus_2026_11_18"]
TIER_SPECS = {
    t.name: {"min_share": t.min_share, "nominal_rebate_bps": t.nominal_rebate_bps}
    for t in ARCUS_SCHEDULE.tiers
}


def compute_capped_rebate_bps(
    nominal_rebate_bps: float,
    cohorts: Sequence[CounterpartyFillCohort],
) -> tuple[float, float]:
    """Return `(realized_rebate_bps, haircut_ratio)` with strict per-fill taker fee capping."""
    return compute_effective_rebate_bps(
        nominal_rebate_bps,
        cohorts,
        cap_by_taker_fee=True,
    )


def evaluate_rebate_matrix(
    *,
    allocated_capital_usd: float,
    max_gross_notional_usd: float,
    tier: str = "T5",
    venue_daily_volumes_usd: Sequence[float] = DEFAULT_VENUE_DAILY_VOLUMES_USD,
    wear_costs_per_m_usd: Sequence[float] = DEFAULT_WEAR_COSTS_PER_MILLION_USD,
    cohorts: Sequence[CounterpartyFillCohort] = DEFAULT_COHORTS,
) -> dict[str, object]:
    """Evaluate Arcus T5/T6 net rebate economics across venue volumes and wear regimes."""
    return evaluate_venue_rebate_matrix(
        schedule=ARCUS_SCHEDULE,
        allocated_capital_usd=allocated_capital_usd,
        max_gross_notional_usd=max_gross_notional_usd,
        tier=tier,
        venue_daily_volumes_usd=venue_daily_volumes_usd,
        wear_costs_per_m_usd=wear_costs_per_m_usd,
        cohorts=cohorts,
    )


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--capital-usd", type=float, default=25_000.0)
    parser.add_argument("--max-gross-usd", type=float, default=25_000.0)
    parser.add_argument("--tier", choices=sorted(TIER_SPECS), default="T5")
    parser.add_argument(
        "--cohorts-json",
        type=Path,
        help="Optional JSON list of {name, notional_share, taker_fee_bps}",
    )
    parser.add_argument(
        "--self-test",
        action="store_true",
        help="Run built-in capped-rebate & <=1x leverage invariant checks and print sample matrix",
    )
    args = parser.parse_args(argv)

    if args.self_test:
        if args.cohorts_json is not None:
            parser.error("--self-test cannot be combined with --cohorts-json")
        t5 = evaluate_rebate_matrix(
            allocated_capital_usd=25_000.0,
            max_gross_notional_usd=25_000.0,
            tier="T5",
        )
        t6 = evaluate_rebate_matrix(
            allocated_capital_usd=50_000.0,
            max_gross_notional_usd=50_000.0,
            tier="T6",
        )
        assert t5["effective_leverage"] <= MAX_ACCOUNT_LEVERAGE
        assert t5["realized_capped_rebate_bps"] < t5["nominal_rebate_bps"]
        assert len(t5["volumes"]) == 3 and len(t6["volumes"]) == 3
        print(json.dumps({"self_test": "ok", "T5": t5, "T6": t6}, indent=2, allow_nan=False))
        return 0

    cohorts = load_cohorts_from_json(args.cohorts_json) if args.cohorts_json else DEFAULT_COHORTS
    report = evaluate_rebate_matrix(
        allocated_capital_usd=args.capital_usd,
        max_gross_notional_usd=args.max_gross_usd,
        tier=args.tier,
        cohorts=cohorts,
    )
    print(json.dumps(report, indent=2, allow_nan=False))
    return 0


if __name__ == "__main__":
    sys.exit(main())
