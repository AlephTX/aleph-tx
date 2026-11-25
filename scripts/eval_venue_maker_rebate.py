#!/usr/bin/env python3
"""Venue-agnostic maker rebate, counterparty taker-fee cap, & wear sensitivity evaluator.

Supports built-in venue presets (`arcus_2026_11_18`, `generic_uncapped`) as well as
custom JSON schedules via `--schedule-json`. Enforces strict `<= 1.0x` leverage, rejects
`bool` masquerading as numbers, and guarantees `allow_nan=False` JSON output.
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass
import json
import math
from pathlib import Path
import sys
from typing import Any, Sequence

DEFAULT_VENUE_DAILY_VOLUMES_USD = (900_000_000.0, 1_500_000_000.0, 3_000_000_000.0)
DEFAULT_WEAR_COSTS_PER_MILLION_USD = (0.0, 50.0, 100.0, 200.0)
MAX_ACCOUNT_LEVERAGE = 1.0


def require_finite_float(val: Any, name: str) -> float:
    if isinstance(val, bool) or not isinstance(val, (int, float)):
        raise ValueError(f"{name} must be a real number, got {type(val).__name__}")
    out = float(val)
    if not math.isfinite(out):
        raise ValueError(f"{name} must be finite, got {val}")
    return out


def require_positive_float(val: Any, name: str) -> float:
    out = require_finite_float(val, name)
    if out <= 0.0:
        raise ValueError(f"{name} must be > 0, got {out}")
    return out


def require_non_empty_str(val: Any, name: str) -> str:
    if not isinstance(val, str) or not val.strip():
        raise ValueError(f"{name} must be a non-empty string")
    return val


def require_bool(val: Any, name: str) -> bool:
    if not isinstance(val, bool):
        raise ValueError(f"{name} must be bool, got {type(val).__name__}")
    return val


@dataclass(frozen=True)
class CounterpartyFillCohort:
    """Distribution slice of taker counterparties executing against resting maker quotes."""

    name: str
    notional_share: float
    taker_fee_bps: float

    def __post_init__(self) -> None:
        if not isinstance(self.name, str) or not self.name.strip():
            raise ValueError("cohort name must be a non-empty string")
        share = require_finite_float(self.notional_share, "notional_share")
        if not (0.0 <= share <= 1.0):
            raise ValueError(f"invalid notional_share: {share}")
        fee = require_finite_float(self.taker_fee_bps, "taker_fee_bps")
        if fee < 0.0:
            raise ValueError(f"invalid taker_fee_bps: {fee}")


@dataclass(frozen=True)
class VenueRebateTier:
    """Single maker rebate tier definition for any perpetual or spot venue."""

    name: str
    min_share: float
    nominal_rebate_bps: float

    def __post_init__(self) -> None:
        if not isinstance(self.name, str) or not self.name.strip():
            raise ValueError("tier name must be non-empty")
        share = require_finite_float(self.min_share, "min_share")
        if not (0.0 < share <= 1.0):
            raise ValueError(f"tier min_share must be in (0, 1], got {share}")
        rebate = require_finite_float(self.nominal_rebate_bps, "nominal_rebate_bps")
        if rebate < 0.0:
            raise ValueError(f"nominal_rebate_bps must be >= 0, got {rebate}")


@dataclass(frozen=True)
class VenueRebateSchedule:
    """Venue-agnostic maker rebate schedule with optional per-fill taker fee cap."""

    venue_id: str
    cap_by_counterparty_taker_fee: bool
    tiers: tuple[VenueRebateTier, ...]

    def __post_init__(self) -> None:
        if not isinstance(self.venue_id, str) or not self.venue_id.strip():
            raise ValueError("venue_id must be non-empty")
        if not isinstance(self.cap_by_counterparty_taker_fee, bool):
            raise ValueError("cap_by_counterparty_taker_fee must be bool")
        if not self.tiers:
            raise ValueError("tiers must not be empty")

    def tier(self, tier_name: str) -> VenueRebateTier:
        key = tier_name.strip().upper()
        for t in self.tiers:
            if t.name.upper() == key:
                return t
        available = [t.name for t in self.tiers]
        raise ValueError(
            f"unsupported tier '{tier_name}' for '{self.venue_id}', expected one of {available}"
        )


BUILTIN_SCHEDULES: dict[str, VenueRebateSchedule] = {
    "arcus_2026_11_18": VenueRebateSchedule(
        venue_id="arcus_2026_11_18",
        cap_by_counterparty_taker_fee=True,
        tiers=(
            VenueRebateTier(name="T5", min_share=0.015, nominal_rebate_bps=0.20),
            VenueRebateTier(name="T6", min_share=0.040, nominal_rebate_bps=0.30),
        ),
    ),
    "generic_uncapped": VenueRebateSchedule(
        venue_id="generic_uncapped",
        cap_by_counterparty_taker_fee=False,
        tiers=(
            VenueRebateTier(name="T1", min_share=0.010, nominal_rebate_bps=0.15),
            VenueRebateTier(name="T2", min_share=0.030, nominal_rebate_bps=0.25),
        ),
    ),
}

DEFAULT_COHORTS: tuple[CounterpartyFillCohort, ...] = (
    CounterpartyFillCohort(name="retail_standard", notional_share=0.55, taker_fee_bps=2.50),
    CounterpartyFillCohort(name="vip_taker", notional_share=0.25, taker_fee_bps=0.50),
    CounterpartyFillCohort(name="fee_discounted_inst", notional_share=0.20, taker_fee_bps=0.10),
)


def compute_effective_rebate_bps(
    nominal_rebate_bps: float,
    cohorts: Sequence[CounterpartyFillCohort],
    *,
    cap_by_taker_fee: bool = True,
) -> tuple[float, float]:
    """Return `(realized_rebate_bps, capture_ratio)` with optional counterparty taker cap."""
    nominal = require_finite_float(nominal_rebate_bps, "nominal_rebate_bps")
    if nominal < 0.0:
        raise ValueError(f"nominal_rebate_bps must be >= 0, got {nominal}")
    if not cohorts:
        raise ValueError("cohorts must not be empty")
    total_share = sum(c.notional_share for c in cohorts)
    if not math.isclose(total_share, 1.0, rel_tol=1e-6, abs_tol=1e-6):
        raise ValueError(f"cohort notional_share must sum to 1.0, got {total_share}")

    if not cap_by_taker_fee:
        return nominal, 1.0

    realized = sum(c.notional_share * min(nominal, c.taker_fee_bps) for c in cohorts)
    capture_ratio = (realized / nominal) if nominal > 0.0 else 1.0
    return realized, capture_ratio


def evaluate_venue_rebate_matrix(
    *,
    schedule: VenueRebateSchedule,
    allocated_capital_usd: float,
    max_gross_notional_usd: float,
    tier: str,
    venue_daily_volumes_usd: Sequence[float] = DEFAULT_VENUE_DAILY_VOLUMES_USD,
    wear_costs_per_m_usd: Sequence[float] = DEFAULT_WEAR_COSTS_PER_MILLION_USD,
    cohorts: Sequence[CounterpartyFillCohort] = DEFAULT_COHORTS,
) -> dict[str, object]:
    """Evaluate daily/30d net rebate economics across venue volumes and adverse-selection wear."""
    capital = require_positive_float(allocated_capital_usd, "allocated_capital_usd")
    max_gross = require_positive_float(max_gross_notional_usd, "max_gross_notional_usd")
    leverage = max_gross / capital
    if leverage > MAX_ACCOUNT_LEVERAGE + 1e-12:
        raise ValueError(
            f"enforces <= {MAX_ACCOUNT_LEVERAGE:.1f}x gross leverage; got {leverage:.4f}x"
        )
    if not venue_daily_volumes_usd:
        raise ValueError("venue_daily_volumes_usd must not be empty")
    if not wear_costs_per_m_usd:
        raise ValueError("wear_costs_per_m_usd must not be empty")

    tier_spec = schedule.tier(tier)
    realized_rebate_bps, rebate_capture_ratio = compute_effective_rebate_bps(
        tier_spec.nominal_rebate_bps,
        cohorts,
        cap_by_taker_fee=schedule.cap_by_counterparty_taker_fee,
    )
    gross_rebate_usd_per_m = realized_rebate_bps * 100.0
    breakeven_wear_usd_per_m = gross_rebate_usd_per_m

    volume_rows: list[dict[str, object]] = []
    for raw_vol in venue_daily_volumes_usd:
        venue_vol = require_positive_float(raw_vol, "venue_daily_volume_usd")
        required_daily_maker_usd = venue_vol * tier_spec.min_share
        required_turnover_per_capital_day = required_daily_maker_usd / max_gross
        required_millions = required_daily_maker_usd / 1_000_000.0
        gross_daily_rebate_usd = required_millions * gross_rebate_usd_per_m

        wear_scenarios: list[dict[str, object]] = []
        for raw_wear in wear_costs_per_m_usd:
            wear_per_m = require_finite_float(raw_wear, "wear_usd_per_m")
            if wear_per_m < 0.0:
                raise ValueError(f"wear_usd_per_m must be >= 0, got {wear_per_m}")
            daily_wear_usd = required_millions * wear_per_m
            net_daily_usd = gross_daily_rebate_usd - daily_wear_usd
            wear_scenarios.append(
                {
                    "wear_usd_per_m": round(wear_per_m, 4),
                    "daily_wear_usd": round(daily_wear_usd, 4),
                    "net_daily_usd": round(net_daily_usd, 4),
                    "net_30d_usd": round(net_daily_usd * 30.0, 4),
                    "annualized_roc_pct": round((net_daily_usd * 365.0 / capital) * 100.0, 4),
                    "profitable": net_daily_usd > 0.0,
                }
            )

        volume_rows.append(
            {
                "venue_daily_volume_usd": round(venue_vol, 2),
                "required_daily_maker_usd": round(required_daily_maker_usd, 2),
                "required_daily_turnover_x": round(required_turnover_per_capital_day, 4),
                "gross_daily_rebate_usd": round(gross_daily_rebate_usd, 4),
                "wear_scenarios": wear_scenarios,
            }
        )

    return {
        "schema_version": 2,
        "venue_id": schedule.venue_id,
        "cap_by_counterparty_taker_fee": schedule.cap_by_counterparty_taker_fee,
        "tier": tier_spec.name,
        "target_maker_share": tier_spec.min_share,
        "allocated_capital_usd": round(capital, 2),
        "max_gross_notional_usd": round(max_gross, 2),
        "effective_leverage": round(leverage, 6),
        "nominal_rebate_bps": tier_spec.nominal_rebate_bps,
        "realized_capped_rebate_bps": round(realized_rebate_bps, 6),
        "rebate_capture_ratio": round(rebate_capture_ratio, 6),
        "gross_rebate_usd_per_m": round(gross_rebate_usd_per_m, 4),
        "breakeven_wear_usd_per_m": round(breakeven_wear_usd_per_m, 4),
        "volumes": volume_rows,
    }


def load_schedule_from_json(path: Path) -> VenueRebateSchedule:
    if not path.is_file():
        raise ValueError(f"schedule file does not exist or is not a file: {path}")
    data = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(data, dict):
        raise ValueError("schedule JSON root must be an object")
    raw_tiers = data.get("tiers")
    if not isinstance(raw_tiers, list):
        raise ValueError("schedule JSON 'tiers' must be a list")
    tiers_list: list[VenueRebateTier] = []
    for idx, t in enumerate(raw_tiers):
        if not isinstance(t, dict):
            raise ValueError(f"schedule tier at index {idx} must be an object")
        tiers_list.append(
            VenueRebateTier(
                name=require_non_empty_str(t.get("name"), f"tiers[{idx}].name"),
                min_share=require_positive_float(t.get("min_share"), f"tiers[{idx}].min_share"),
                nominal_rebate_bps=require_finite_float(
                    t.get("nominal_rebate_bps"), f"tiers[{idx}].nominal_rebate_bps"
                ),
            )
        )
    return VenueRebateSchedule(
        venue_id=require_non_empty_str(data.get("venue_id"), "venue_id"),
        cap_by_counterparty_taker_fee=require_bool(
            data.get("cap_by_counterparty_taker_fee"), "cap_by_counterparty_taker_fee"
        ),
        tiers=tuple(tiers_list),
    )


def load_cohorts_from_json(path: Path) -> tuple[CounterpartyFillCohort, ...]:
    if not path.is_file():
        raise ValueError(f"cohorts file does not exist or is not a file: {path}")
    raw = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(raw, list):
        raise ValueError("cohorts JSON root must be a list")
    cohorts: list[CounterpartyFillCohort] = []
    for idx, r in enumerate(raw):
        if not isinstance(r, dict):
            raise ValueError(f"cohort at index {idx} must be an object")
        cohorts.append(
            CounterpartyFillCohort(
                name=require_non_empty_str(r.get("name"), f"cohorts[{idx}].name"),
                notional_share=require_finite_float(
                    r.get("notional_share"), f"cohorts[{idx}].notional_share"
                ),
                taker_fee_bps=require_finite_float(
                    r.get("taker_fee_bps"), f"cohorts[{idx}].taker_fee_bps"
                ),
            )
        )
    return tuple(cohorts)


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--preset", choices=sorted(BUILTIN_SCHEDULES), default="arcus_2026_11_18")
    parser.add_argument("--schedule-json", type=Path, help="Custom VenueRebateSchedule JSON path")
    parser.add_argument("--capital-usd", type=float, default=25_000.0)
    parser.add_argument("--max-gross-usd", type=float, default=25_000.0)
    parser.add_argument("--tier", default="T5")
    parser.add_argument("--cohorts-json", type=Path, help="Optional counterparty fill cohorts JSON")
    parser.add_argument(
        "--self-test",
        action="store_true",
        help="Run built-in capped vs uncapped & <=1x leverage checks",
    )
    args = parser.parse_args(argv)

    if args.self_test:
        if args.schedule_json is not None or args.cohorts_json is not None:
            parser.error("--self-test cannot be combined with --schedule-json or --cohorts-json")
        capped = evaluate_venue_rebate_matrix(
            schedule=BUILTIN_SCHEDULES["arcus_2026_11_18"],
            allocated_capital_usd=25_000.0,
            max_gross_notional_usd=25_000.0,
            tier="T5",
        )
        uncapped = evaluate_venue_rebate_matrix(
            schedule=BUILTIN_SCHEDULES["generic_uncapped"],
            allocated_capital_usd=25_000.0,
            max_gross_notional_usd=25_000.0,
            tier="T2",
        )
        assert capped["realized_capped_rebate_bps"] < capped["nominal_rebate_bps"]
        assert uncapped["realized_capped_rebate_bps"] == uncapped["nominal_rebate_bps"]
        print(
            json.dumps(
                {"self_test": "ok", "capped": capped, "uncapped": uncapped},
                indent=2,
                allow_nan=False,
            )
        )
        return 0

    schedule = (
        load_schedule_from_json(args.schedule_json)
        if args.schedule_json
        else BUILTIN_SCHEDULES[args.preset]
    )
    cohorts = load_cohorts_from_json(args.cohorts_json) if args.cohorts_json else DEFAULT_COHORTS
    report = evaluate_venue_rebate_matrix(
        schedule=schedule,
        allocated_capital_usd=args.capital_usd,
        max_gross_notional_usd=args.max_gross_usd,
        tier=args.tier,
        cohorts=cohorts,
    )
    print(json.dumps(report, indent=2, allow_nan=False))
    return 0


if __name__ == "__main__":
    sys.exit(main())
