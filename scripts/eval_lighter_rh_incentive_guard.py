#!/usr/bin/env python3
"""Offline Lighter Mainnet <-> Robinhood 2.5x incentive guard & Friday ROI kill-switch.

Implements BARBELL §6.4 Gate 4 strictly offline with exact Decimal accounting:
- Segregated `03_incentive_turnover` cash ledger (`unredeemed_points_cash_value_usd = "0"`).
- Gate 1: Verified account fee-tier evidence on both venues + Standard<->Standard zero-point trap halt.
- Gate 2: Explicit USDC/USDG cross-stablecoin authorization + >=2 distinct fresh oracle depeg quorum
          (`>= 30 bp` -> REDUCE_ONLY, `>= 75 bp` -> HALT_AND_FLATTEN).
- Gate 3: Net Delta bound (`>= 0.5%` -> REDUCE_ONLY, `>= 1.5%` -> HALT_AND_FLATTEN) & cumulative cash loss budget.
- Gate 4: Friday settlement dynamic ROI & coverage-cushion kill-switch (shadow-only hypothesis).
"""

from __future__ import annotations

import argparse
from decimal import Decimal, InvalidOperation
import json
import math
from pathlib import Path
import sys
from typing import Any

SCHEMA_VERSION = "aleph_lighter_rh_incentive_guard_v1"
VERIFIED_ACCOUNT_FEE_STATUS = "verified_account_tier"

ACTION_CONTINUE = "CONTINUE_HEDGE_FARMING"
ACTION_REDUCE_ONLY = "REDUCE_ONLY"
ACTION_HALT = "HALT_AND_FLATTEN"
_ACTION_RANK: dict[str, int] = {
    ACTION_CONTINUE: 0,
    ACTION_REDUCE_ONLY: 1,
    ACTION_HALT: 2,
}

VALID_REDEMPTION_RATE_SOURCES: frozenset[str] = frozenset(
    {"official", "scenario_hypothesis"}
)

BPS_SCALE = Decimal("10000")
DECIMAL_ONE = Decimal("1")
DECIMAL_ZERO = Decimal("0")


def _escalate(current: str, candidate: str) -> str:
    return candidate if _ACTION_RANK[candidate] > _ACTION_RANK[current] else current


def _as_decimal(value: Any, name: str) -> Decimal:
    if isinstance(value, bool) or value is None:
        raise ValueError(f"{name} must be a finite numeric value, got {type(value).__name__}")
    if isinstance(value, float):
        if not math.isfinite(value):
            raise ValueError(f"{name} must be finite")
        raw = str(value)
    elif isinstance(value, (Decimal, int, str)):
        raw = str(value).strip()
    else:
        raise ValueError(f"{name} must be Decimal/int/float/str, got {type(value).__name__}")
    if not raw:
        raise ValueError(f"{name} must not be empty")
    try:
        dec = Decimal(raw)
    except (InvalidOperation, ValueError) as exc:
        raise ValueError(f"{name} is not a valid Decimal: {raw!r}") from exc
    if not dec.is_finite():
        raise ValueError(f"{name} must be finite")
    return dec


def _as_int(value: Any, name: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int):
        raise ValueError(f"{name} must be an integer, got {type(value).__name__}")
    return value


def _as_bool(value: Any, name: str) -> bool:
    if not isinstance(value, bool):
        raise ValueError(f"{name} must be a boolean, got {type(value).__name__}")
    return value


def _as_nonempty_str(value: Any, name: str) -> str:
    if not isinstance(value, str) or not value.strip():
        raise ValueError(f"{name} must be a non-empty string")
    return value.strip()


def _require_mapping(parent: dict[str, Any], key: str) -> dict[str, Any]:
    val = parent.get(key)
    if not isinstance(val, dict):
        raise ValueError(f"{key} must be an object")
    return val


def evaluate_lighter_rh_incentive(payload: dict[str, Any]) -> dict[str, Any]:
    """Evaluate all 4 BARBELL §6.4 gates and emit a fail-closed supervisory decision."""
    if not isinstance(payload, dict):
        raise ValueError("payload must be a JSON object")

    now_ms = _as_int(payload.get("now_ms"), "now_ms")
    if now_ms <= 0:
        raise ValueError("now_ms must be > 0")

    account_tiers = _require_mapping(payload, "account_tiers")
    stablecoin_guard = _require_mapping(payload, "stablecoin_guard")
    position_and_budget = _require_mapping(payload, "position_and_budget")
    weekly_ledger = _require_mapping(payload, "weekly_cash_ledger")
    friday_settlement = _require_mapping(payload, "friday_settlement")

    action = ACTION_CONTINUE
    reasons: list[str] = []

    # ------------------------------------------------------------------
    # Gate 1: Verified account fee-tier evidence & Standard/Standard trap
    # ------------------------------------------------------------------
    mainnet_fee_status = _as_nonempty_str(
        account_tiers.get("mainnet_fee_status"), "account_tiers.mainnet_fee_status"
    )
    rh_fee_status = _as_nonempty_str(
        account_tiers.get("rh_fee_status"), "account_tiers.rh_fee_status"
    )
    mainnet_tier = _as_nonempty_str(
        account_tiers.get("mainnet_tier"), "account_tiers.mainnet_tier"
    )
    rh_tier = _as_nonempty_str(account_tiers.get("rh_tier"), "account_tiers.rh_tier")

    tiers_verified = (
        mainnet_fee_status == VERIFIED_ACCOUNT_FEE_STATUS
        and rh_fee_status == VERIFIED_ACCOUNT_FEE_STATUS
    )
    if not tiers_verified:
        action = _escalate(action, ACTION_HALT)
        reasons.append("unverified_account_tier")

    is_standard_pair = (
        mainnet_tier.lower() == "standard" and rh_tier.lower() == "standard"
    )
    if is_standard_pair:
        action = _escalate(action, ACTION_HALT)
        reasons.append("zero_point_standard_pair")

    # ------------------------------------------------------------------
    # Gate 2: Cross-stablecoin authorization & multi-source depeg quorum
    # ------------------------------------------------------------------
    cross_stablecoin_authorized = _as_bool(
        stablecoin_guard.get("cross_stablecoin_authorized"),
        "stablecoin_guard.cross_stablecoin_authorized",
    )
    max_peg_age_ms = _as_int(
        stablecoin_guard.get("max_peg_age_ms", 60_000),
        "stablecoin_guard.max_peg_age_ms",
    )
    max_future_skew_ms = _as_int(
        stablecoin_guard.get("max_future_skew_ms", 5_000),
        "stablecoin_guard.max_future_skew_ms",
    )
    if max_peg_age_ms <= 0:
        raise ValueError("stablecoin_guard.max_peg_age_ms must be > 0")
    if max_future_skew_ms < 0:
        raise ValueError("stablecoin_guard.max_future_skew_ms must be >= 0")

    depeg_reduce_only_bps = _as_decimal(
        stablecoin_guard.get("depeg_reduce_only_bps", Decimal("30")),
        "stablecoin_guard.depeg_reduce_only_bps",
    )
    depeg_halt_bps = _as_decimal(
        stablecoin_guard.get("depeg_halt_bps", Decimal("75")),
        "stablecoin_guard.depeg_halt_bps",
    )
    if depeg_reduce_only_bps <= DECIMAL_ZERO or depeg_halt_bps <= depeg_reduce_only_bps:
        raise ValueError("invalid stablecoin_guard depeg thresholds")

    raw_peg_obs = stablecoin_guard.get("peg_observations")
    if not isinstance(raw_peg_obs, list):
        raise ValueError("stablecoin_guard.peg_observations must be a list")

    valid_sources: dict[str, Decimal] = {}
    rejected_peg_observations = 0
    for idx, obs in enumerate(raw_peg_obs):
        if not isinstance(obs, dict):
            raise ValueError(f"stablecoin_guard.peg_observations[{idx}] must be an object")
        source = _as_nonempty_str(
            obs.get("source"), f"stablecoin_guard.peg_observations[{idx}].source"
        )
        price = _as_decimal(
            obs.get("usdc_usdg_price"),
            f"stablecoin_guard.peg_observations[{idx}].usdc_usdg_price",
        )
        if price <= DECIMAL_ZERO:
            raise ValueError(
                f"stablecoin_guard.peg_observations[{idx}].usdc_usdg_price must be > 0"
            )
        observed_at_ms = _as_int(
            obs.get("observed_at_ms"),
            f"stablecoin_guard.peg_observations[{idx}].observed_at_ms",
        )
        if observed_at_ms <= 0:
            rejected_peg_observations += 1
            continue
        if observed_at_ms > now_ms + max_future_skew_ms:
            rejected_peg_observations += 1
            continue
        if observed_at_ms < now_ms - max_peg_age_ms:
            rejected_peg_observations += 1
            continue

        dev_bps = abs(price - DECIMAL_ONE) * BPS_SCALE
        # Canonicalize oracle names case-insensitively so casing aliases cannot spoof quorum.
        source_key = source.lower()
        prev = valid_sources.get(source_key)
        if prev is None or dev_bps > prev:
            valid_sources[source_key] = dev_bps

    if not cross_stablecoin_authorized:
        action = _escalate(action, ACTION_HALT)
        reasons.append("cross_stablecoin_unauthorized")

    quorum_count = len(valid_sources)
    max_abs_deviation_bps: Decimal | None = (
        max(valid_sources.values()) if valid_sources else None
    )
    if quorum_count < 2:
        action = _escalate(action, ACTION_HALT)
        reasons.append("depeg_oracle_quorum_unavailable")
    if max_abs_deviation_bps is not None:
        if max_abs_deviation_bps >= depeg_halt_bps:
            action = _escalate(action, ACTION_HALT)
            reasons.append("usdc_usdg_depeg_halt")
        elif max_abs_deviation_bps >= depeg_reduce_only_bps:
            action = _escalate(action, ACTION_REDUCE_ONLY)
            reasons.append("usdc_usdg_depeg_reduce_only")

    # ------------------------------------------------------------------
    # Gate 3: Cash ledger ($0 points) + Net Delta + Loss Budget
    # ------------------------------------------------------------------
    period_days = _as_decimal(
        weekly_ledger.get("period_days", 7), "weekly_cash_ledger.period_days"
    )
    if period_days <= DECIMAL_ZERO:
        raise ValueError("weekly_cash_ledger.period_days must be > 0")

    trading_spread_pnl_usd = _as_decimal(
        weekly_ledger.get("trading_spread_pnl_usd"),
        "weekly_cash_ledger.trading_spread_pnl_usd",
    )
    mainnet_fees_usd = _as_decimal(
        weekly_ledger.get("mainnet_fees_usd"),
        "weekly_cash_ledger.mainnet_fees_usd",
    )
    rh_fees_usd = _as_decimal(
        weekly_ledger.get("rh_fees_usd"),
        "weekly_cash_ledger.rh_fees_usd",
    )
    slippage_usd = _as_decimal(
        weekly_ledger.get("slippage_usd"), "weekly_cash_ledger.slippage_usd"
    )
    net_funding_usd = _as_decimal(
        weekly_ledger.get("net_funding_usd"),
        "weekly_cash_ledger.net_funding_usd",
    )
    margin_opportunity_cost_usd = _as_decimal(
        weekly_ledger.get("margin_opportunity_cost_usd"),
        "weekly_cash_ledger.margin_opportunity_cost_usd",
    )
    usdc_usdg_fx_cost_usd = _as_decimal(
        weekly_ledger.get("usdc_usdg_fx_cost_usd"),
        "weekly_cash_ledger.usdc_usdg_fx_cost_usd",
    )
    for neg_check_name, neg_check_val in (
        ("weekly_cash_ledger.slippage_usd", slippage_usd),
        ("weekly_cash_ledger.margin_opportunity_cost_usd", margin_opportunity_cost_usd),
        ("weekly_cash_ledger.usdc_usdg_fx_cost_usd", usdc_usdg_fx_cost_usd),
    ):
        if neg_check_val < DECIMAL_ZERO:
            raise ValueError(f"{neg_check_name} must be >= 0")

    realized_cash_pnl_usd = (
        trading_spread_pnl_usd
        - mainnet_fees_usd
        - rh_fees_usd
        - slippage_usd
        + net_funding_usd
        - margin_opportunity_cost_usd
        - usdc_usdg_fx_cost_usd
    )
    weekly_cash_cost_usd = max(DECIMAL_ZERO, -realized_cash_pnl_usd)

    net_delta_ratio = _as_decimal(
        position_and_budget.get("net_delta_ratio"),
        "position_and_budget.net_delta_ratio",
    )
    prior_cumulative_cash_loss_usd = _as_decimal(
        position_and_budget.get("prior_cumulative_cash_loss_usd"),
        "position_and_budget.prior_cumulative_cash_loss_usd",
    )
    loss_budget_usd = _as_decimal(
        position_and_budget.get("loss_budget_usd"),
        "position_and_budget.loss_budget_usd",
    )
    total_locked_margin_usd = _as_decimal(
        position_and_budget.get("total_locked_margin_usd"),
        "position_and_budget.total_locked_margin_usd",
    )
    delta_reduce_only_ratio = _as_decimal(
        position_and_budget.get("delta_reduce_only_ratio", Decimal("0.005")),
        "position_and_budget.delta_reduce_only_ratio",
    )
    delta_halt_ratio = _as_decimal(
        position_and_budget.get("delta_halt_ratio", Decimal("0.015")),
        "position_and_budget.delta_halt_ratio",
    )
    if prior_cumulative_cash_loss_usd < DECIMAL_ZERO:
        raise ValueError("position_and_budget.prior_cumulative_cash_loss_usd must be >= 0")
    if loss_budget_usd <= DECIMAL_ZERO:
        raise ValueError("position_and_budget.loss_budget_usd must be > 0")
    if total_locked_margin_usd <= DECIMAL_ZERO:
        raise ValueError("position_and_budget.total_locked_margin_usd must be > 0")
    if (
        delta_reduce_only_ratio <= DECIMAL_ZERO
        or delta_halt_ratio <= delta_reduce_only_ratio
    ):
        raise ValueError("invalid net delta thresholds")

    abs_delta_ratio = abs(net_delta_ratio)
    if abs_delta_ratio >= delta_halt_ratio:
        action = _escalate(action, ACTION_HALT)
        reasons.append("net_delta_halt_breach")
    elif abs_delta_ratio >= delta_reduce_only_ratio:
        action = _escalate(action, ACTION_REDUCE_ONLY)
        reasons.append("net_delta_reduce_only")

    cumulative_cash_loss_usd = prior_cumulative_cash_loss_usd + weekly_cash_cost_usd
    if cumulative_cash_loss_usd >= loss_budget_usd:
        action = _escalate(action, ACTION_HALT)
        reasons.append("loss_budget_exhausted")

    # ------------------------------------------------------------------
    # Gate 4: Friday settlement dynamic ROI & cushion kill-switch
    # ------------------------------------------------------------------
    points_received = _as_decimal(
        friday_settlement.get("points_received"), "friday_settlement.points_received"
    )
    if points_received < DECIMAL_ZERO:
        raise ValueError("friday_settlement.points_received must be >= 0")

    lit_price_usd = _as_decimal(
        friday_settlement.get("lit_price_usd"), "friday_settlement.lit_price_usd"
    )
    if lit_price_usd <= DECIMAL_ZERO:
        raise ValueError("friday_settlement.lit_price_usd must be > 0")

    min_safety_margin_ratio = _as_decimal(
        friday_settlement.get("min_safety_margin_ratio", Decimal("1.25")),
        "friday_settlement.min_safety_margin_ratio",
    )
    if min_safety_margin_ratio < DECIMAL_ONE:
        raise ValueError("friday_settlement.min_safety_margin_ratio must be >= 1")

    min_annualized_roi_pct = _as_decimal(
        friday_settlement.get("min_annualized_roi_pct", DECIMAL_ZERO),
        "friday_settlement.min_annualized_roi_pct",
    )

    raw_lit_per_point = friday_settlement.get("lit_per_point")
    lit_per_point: Decimal | None
    redemption_rate_source: str | None
    if raw_lit_per_point is None:
        lit_per_point = None
        redemption_rate_source = None
        action = _escalate(action, ACTION_HALT)
        reasons.append("unannounced_redemption_rate")
    else:
        lit_per_point = _as_decimal(
            raw_lit_per_point, "friday_settlement.lit_per_point"
        )
        if lit_per_point <= DECIMAL_ZERO:
            raise ValueError("friday_settlement.lit_per_point must be > 0 when present")
        redemption_rate_source = _as_nonempty_str(
            friday_settlement.get("redemption_rate_source"),
            "friday_settlement.redemption_rate_source",
        )
        if redemption_rate_source not in VALID_REDEMPTION_RATE_SOURCES:
            raise ValueError(
                "friday_settlement.redemption_rate_source must be 'official' or 'scenario_hypothesis'"
            )

    implied_cost_per_point_usd: Decimal | None = None
    shadow_points_value_usd: Decimal | None = None
    shadow_coverage_ratio: Decimal | None = None
    annualized_shadow_roi_pct: Decimal | None = None

    evaluate_shadow_roi = True
    if weekly_cash_cost_usd > DECIMAL_ZERO:
        if points_received <= DECIMAL_ZERO:
            action = _escalate(action, ACTION_HALT)
            reasons.append("zero_points_with_positive_cash_cost")
            evaluate_shadow_roi = False
        else:
            implied_cost_per_point_usd = weekly_cash_cost_usd / points_received
    else:
        implied_cost_per_point_usd = DECIMAL_ZERO

    if evaluate_shadow_roi and lit_per_point is not None:
        shadow_points_value_usd = points_received * lit_per_point * lit_price_usd
        if weekly_cash_cost_usd > DECIMAL_ZERO:
            shadow_coverage_ratio = shadow_points_value_usd / weekly_cash_cost_usd
        annualized_shadow_roi_pct = (
            ((shadow_points_value_usd - weekly_cash_cost_usd) / total_locked_margin_usd)
            * (Decimal("365") / period_days)
            * Decimal("100")
        )
        if (
            shadow_coverage_ratio is not None
            and shadow_coverage_ratio < min_safety_margin_ratio
        ) or annualized_shadow_roi_pct < min_annualized_roi_pct:
            action = _escalate(action, ACTION_HALT)
            reasons.append("friday_roi_below_safety_cushion")

    return {
        "schema_version": SCHEMA_VERSION,
        "track": "03_incentive_turnover",
        "decision_action": action,
        "allow_new_farming": action == ACTION_CONTINUE,
        "reasons": reasons,
        "cash_ledger": {
            "unredeemed_points_cash_value_usd": "0",
            "realized_cash_pnl_usd": str(realized_cash_pnl_usd),
            "weekly_cash_cost_usd": str(weekly_cash_cost_usd),
            "prior_cumulative_cash_loss_usd": str(prior_cumulative_cash_loss_usd),
            "cumulative_cash_loss_usd": str(cumulative_cash_loss_usd),
            "loss_budget_usd": str(loss_budget_usd),
            "remaining_loss_budget_usd": str(
                max(DECIMAL_ZERO, loss_budget_usd - cumulative_cash_loss_usd)
            ),
        },
        "gate_diagnostics": {
            "tiers_verified": tiers_verified,
            "is_standard_pair": is_standard_pair,
            "cross_stablecoin_authorized": cross_stablecoin_authorized,
            "peg_quorum_sources": sorted(valid_sources.keys()),
            "peg_quorum_count": quorum_count,
            "rejected_peg_observations": rejected_peg_observations,
            "max_abs_deviation_bps": (
                str(max_abs_deviation_bps) if max_abs_deviation_bps is not None else None
            ),
            "abs_net_delta_ratio": str(abs_delta_ratio),
        },
        "shadow_incentive_valuation": {
            "valuation_basis": "shadow_hypothesis_not_cash",
            "points_received": str(points_received),
            "lit_price_usd": str(lit_price_usd),
            "lit_per_point": str(lit_per_point) if lit_per_point is not None else None,
            "redemption_rate_source": redemption_rate_source,
            "implied_cost_per_point_usd": (
                str(implied_cost_per_point_usd)
                if implied_cost_per_point_usd is not None
                else None
            ),
            "shadow_points_value_usd": (
                str(shadow_points_value_usd)
                if shadow_points_value_usd is not None
                else None
            ),
            "shadow_coverage_ratio": (
                str(shadow_coverage_ratio)
                if shadow_coverage_ratio is not None
                else None
            ),
            "annualized_shadow_roi_pct": (
                str(annualized_shadow_roi_pct)
                if annualized_shadow_roi_pct is not None
                else None
            ),
        },
    }


def _base_healthy_payload(now_ms: int = 1_764_000_000_000) -> dict[str, Any]:
    return {
        "now_ms": now_ms,
        "account_tiers": {
            "mainnet_fee_status": VERIFIED_ACCOUNT_FEE_STATUS,
            "mainnet_tier": "Plus",
            "rh_fee_status": VERIFIED_ACCOUNT_FEE_STATUS,
            "rh_tier": "Plus",
        },
        "stablecoin_guard": {
            "cross_stablecoin_authorized": True,
            "max_peg_age_ms": 60_000,
            "max_future_skew_ms": 5_000,
            "depeg_reduce_only_bps": "30",
            "depeg_halt_bps": "75",
            "peg_observations": [
                {
                    "source": "pyth",
                    "usdc_usdg_price": "1.0004",
                    "observed_at_ms": now_ms - 1_500,
                },
                {
                    "source": "chainlink",
                    "usdc_usdg_price": "0.9997",
                    "observed_at_ms": now_ms - 2_000,
                },
            ],
        },
        "position_and_budget": {
            "net_delta_ratio": "0.002",
            "prior_cumulative_cash_loss_usd": "400",
            "loss_budget_usd": "2000",
            "total_locked_margin_usd": "20000",
        },
        "weekly_cash_ledger": {
            "period_days": 7,
            "trading_spread_pnl_usd": "120",
            "mainnet_fees_usd": "150",
            "rh_fees_usd": "150",
            "slippage_usd": "70",
            "net_funding_usd": "20",
            "margin_opportunity_cost_usd": "15",
            "usdc_usdg_fx_cost_usd": "5",
        },
        "friday_settlement": {
            "points_received": "1000",
            "lit_price_usd": "2.71",
            "lit_per_point": "0.20",
            "redemption_rate_source": "scenario_hypothesis",
            "min_safety_margin_ratio": "1.25",
            "min_annualized_roi_pct": "0",
        },
    }


def run_self_test() -> dict[str, Any]:
    """Run deterministic offline scenarios covering every §6.4 gate."""
    # 1. Healthy Plus <-> Plus weekly farming ($250 cash cost covered >1.25x in shadow view, points=$0 cash)
    s1 = evaluate_lighter_rh_incentive(_base_healthy_payload())
    assert s1["decision_action"] == ACTION_CONTINUE, s1
    assert s1["cash_ledger"]["unredeemed_points_cash_value_usd"] == "0"
    assert s1["cash_ledger"]["weekly_cash_cost_usd"] == "250"

    # 2. Standard <-> Standard zero-point trap -> HALT_AND_FLATTEN
    p2 = _base_healthy_payload()
    p2["account_tiers"]["mainnet_tier"] = "Standard"
    p2["account_tiers"]["rh_tier"] = "Standard"
    s2 = evaluate_lighter_rh_incentive(p2)
    assert s2["decision_action"] == ACTION_HALT, s2
    assert "zero_point_standard_pair" in s2["reasons"]

    # 3. 35 bp -> REDUCE_ONLY; 80 bp -> HALT_AND_FLATTEN
    p3a = _base_healthy_payload()
    p3a["stablecoin_guard"]["peg_observations"][0]["usdc_usdg_price"] = "1.0035"
    s3a = evaluate_lighter_rh_incentive(p3a)
    assert s3a["decision_action"] == ACTION_REDUCE_ONLY, s3a
    assert "usdc_usdg_depeg_reduce_only" in s3a["reasons"]

    p3b = _base_healthy_payload()
    p3b["stablecoin_guard"]["peg_observations"][1]["usdc_usdg_price"] = "0.9920"
    s3b = evaluate_lighter_rh_incentive(p3b)
    assert s3b["decision_action"] == ACTION_HALT, s3b
    assert "usdc_usdg_depeg_halt" in s3b["reasons"]

    # 4. Friday ROI below 1.25x safety cushion -> HALT_AND_FLATTEN
    p4 = _base_healthy_payload()
    p4["friday_settlement"]["points_received"] = "200"
    s4 = evaluate_lighter_rh_incentive(p4)
    assert s4["decision_action"] == ACTION_HALT, s4
    assert "friday_roi_below_safety_cushion" in s4["reasons"]

    # 5. Unannounced redemption rate (lit_per_point = None) -> fail closed HALT_AND_FLATTEN
    p5 = _base_healthy_payload()
    p5["friday_settlement"]["lit_per_point"] = None
    p5["friday_settlement"].pop("redemption_rate_source", None)
    s5 = evaluate_lighter_rh_incentive(p5)
    assert s5["decision_action"] == ACTION_HALT, s5
    assert "unannounced_redemption_rate" in s5["reasons"]

    return {
        "self_test": "ok",
        "schema_version": SCHEMA_VERSION,
        "healthy_weekly_cash_cost_usd": s1["cash_ledger"]["weekly_cash_cost_usd"],
        "healthy_shadow_coverage_ratio": s1["shadow_incentive_valuation"][
            "shadow_coverage_ratio"
        ],
        "depeg_35bp_action": s3a["decision_action"],
        "depeg_80bp_action": s3b["decision_action"],
        "low_roi_action": s4["decision_action"],
        "unannounced_rate_action": s5["decision_action"],
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Offline Lighter Mainnet <-> Robinhood 2.5x incentive guard & Friday ROI kill-switch."
    )
    parser.add_argument("--input", type=Path, help="Path to JSON settlement/state snapshot.")
    parser.add_argument(
        "--self-test", action="store_true", help="Run built-in offline invariant checks."
    )
    args = parser.parse_args(argv)

    if args.self_test:
        print(json.dumps(run_self_test(), indent=2, sort_keys=True))
        return 0

    if args.input is None:
        parser.error("either --input FILE or --self-test is required")

    payload = json.loads(args.input.read_text(encoding="utf-8"), parse_float=str)
    result = evaluate_lighter_rh_incentive(payload)
    print(json.dumps(result, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(main())
