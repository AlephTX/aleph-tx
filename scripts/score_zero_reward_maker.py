"""Cash, capacity and risk measures for a continuous recorded maker window."""
from decimal import Decimal


def _amount(value):
    result = Decimal(str(value))
    if not result.is_finite():
        raise ValueError("nonfinite ledger value")
    return result


def _cost_per_million(net, volume):
    if net is None or volume == 0:
        return None
    return float(-net * 1_000_000 / volume)


def score_window(report, config, *, fixed_overhead_usd_per_day=None):
    """Score the whole strategy's cash against gross and passive volume separately.

    Entry/exit prices, charged fees and flattening costs are already in the
    terminal cash ledger. Additional per-fill overhead is a sensitivity.
    """
    schedule = config["flat_at_funding"]
    interval = schedule["interval_ms"]
    offset = schedule["offset_ms"]
    expected = ((config["drain_until_ms"] - offset) // interval
                - (config["start_ms"] - offset) // interval)
    maker = _amount(report["maker_turnover"])
    taker = _amount(report["taker_turnover"])
    if min(maker, taker) < 0:
        raise ValueError("negative gross turnover")
    gross = maker + taker
    blockers = []
    if config["end_ms"] - config["trade_from_ms"] != 36_000_000:
        blockers.append("decision_duration")
    if report.get("trade_feed_continuity_verified") is not True:
        blockers.append("trade_feed")
    if report.get("funding_boundaries_verified") != expected:
        blockers.append("funding_slots")
    if report.get("final_position") != 0 or report.get("resting_orders") != 0:
        blockers.append("open_risk")
    if report.get("needs_reconciliation") is not False or report.get("halted") or report.get("incomplete"):
        blockers.append("unsettled")
    if report.get("net_liquidation_pnl") is None:
        blockers.append("cash_unknown")

    raw = None if blockers else _amount(report["net_liquidation_pnl"])
    costs = {}
    for bps in ("0", "0.25", "0.5", "1"):
        net = None if raw is None else raw - gross * Decimal(bps) / 10_000
        costs[bps] = {
            "net_cash": None if net is None else float(net),
            "cost_per_million_gross": _cost_per_million(net, gross),
            "cost_per_million_maker": _cost_per_million(net, maker),
        }
    stressed = None if raw is None else raw - gross * Decimal("0.000025") - taker / 10_000
    extra_fixed = {}
    if fixed_overhead_usd_per_day is not None:
        per_day = _amount(fixed_overhead_usd_per_day)
        if per_day < 0:
            raise ValueError("negative fixed overhead")
        decision_hours = Decimal(config["end_ms"] - config["trade_from_ms"]) / Decimal(3_600_000)
        fixed_window = per_day * decision_hours / Decimal(24)
        net_after_fixed = None if raw is None else raw - fixed_window
        extra_fixed["decomposed_overhead"] = {
            "fixed_overhead_usd_per_day": float(per_day),
            "fixed_window_overhead_quote": float(fixed_window),
            "variable_net_cash": None if raw is None else float(raw),
            "variable_cost_per_million_maker": _cost_per_million(raw, maker),
            "net_cash_after_fixed_overhead": None if net_after_fixed is None else float(net_after_fixed),
            "all_in_cost_per_million_maker_observed_pace": _cost_per_million(net_after_fixed, maker),
            "all_in_cost_per_million_maker_50m_30d_pace": (
                None if _cost_per_million(raw, maker) is None
                else float(_amount(_cost_per_million(raw, maker)) + per_day * Decimal(30) / Decimal(50))
            ),
        }
    return {
        "cash_blockers": blockers,
        "gross_turnover": float(gross),
        "maker_turnover": float(maker),
        "taker_turnover": float(taker),
        "maker_share": float(maker / gross) if gross else None,
        "maker_fills": report.get("maker_fills"),
        "taker_fills": report.get("taker_fills"),
        "maker_fees": report.get("maker_fees"),
        "taker_fees": report.get("taker_fees"),
        "peak_inventory_notional": report.get("max_inventory_notional"),
        "model_drawdown_before_extra_overhead": report.get("max_drawdown"),
        "verified_funding_slots": report.get("funding_boundaries_verified"),
        "expected_funding_slots": expected,
        "overhead_bps_on_gross": costs,
        "net025_plus_1bp_on_taker_turnover": None if stressed is None else float(stressed),
        "break_even_overhead_bps_on_gross": float(raw / gross * 10_000) if raw is not None and gross else None,
        "reward_income": 0,
        **extra_fixed,
    }
