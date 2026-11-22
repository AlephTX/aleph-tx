#!/usr/bin/env python3
"""Offline BTC-factor residual ranks and exact fixed-path cash feasibility.

Bar times are event times, not measured receipt times. A caller must record the
publication-delay assumption. This module never fetches data or sends orders.
"""
from bisect import bisect_right
from decimal import Decimal
import math


def asof_prices(trades, cutoffs_us, publication_lag_us=1_000_000,
                max_trade_age_us=10_000_000):
    """Latest actual trade available by each cutoff; never fill across a gap."""
    if publication_lag_us < 0 or max_trade_age_us < 0:
        raise ValueError("negative availability bound")
    times, prices = [], []
    for at, price in trades:
        if type(at) is not int or at <= 0 or (times and at <= times[-1]):
            raise ValueError("trade timestamps must strictly increase")
        price = float(price)
        if not math.isfinite(price) or price <= 0:
            raise ValueError("invalid trade price")
        times.append(at)
        prices.append(price)
    result = []
    for at in cutoffs_us:
        if type(at) is not int or at <= 0:
            raise ValueError("invalid observation cutoff")
        limit = at - publication_lag_us
        i = bisect_right(times, limit) - 1
        result.append(prices[i] if i >= 0 and limit - times[i] <= max_trade_age_us else None)
    return result


def fit_betas(samples, factor="BTC", min_pairs=288):
    """Centered OLS beta on common adjacent one-minute log returns.

    Do not bridge missing endpoints. Intercepts are reported but the subsequent
    signal removes only market exposure, explicitly assuming zero residual drift.
    Window size/clock, split and minimum pairs belong in the frozen caller plan.
    """
    if factor not in samples or min_pairs < 2:
        raise ValueError("invalid factor or minimum")
    n = len(samples[factor])
    if n < 2 or any(len(v) != n for v in samples.values()):
        raise ValueError("unaligned sample lengths")
    for values in samples.values():
        if any(v is not None and (not math.isfinite(v) or v <= 0) for v in values):
            raise ValueError("invalid sample price")
    f = samples[factor]
    out = {}
    for symbol, values in samples.items():
        pairs = [(math.log(f[i] / f[i-1]), math.log(values[i] / values[i-1]))
                 for i in range(1, n)
                 if all(v is not None for v in (f[i-1], f[i], values[i-1], values[i]))]
        row = {"pairs": len(pairs), "beta": None, "intercept_per_minute": None}
        out[symbol] = row
        if len(pairs) < min_pairs:
            row["status"] = "insufficient_common_returns"
            continue
        mx = math.fsum(x for x, _ in pairs) / len(pairs)
        my = math.fsum(y for _, y in pairs) / len(pairs)
        xx = math.fsum((x - mx)**2 for x, _ in pairs)
        if xx == 0:
            row["status"] = "constant_factor"
            continue
        beta = math.fsum((x - mx) * (y - my) for x, y in pairs) / xx
        row.update(status="estimated", beta=beta, intercept_per_minute=my-beta*mx)
    return out


def residual_scores(anchors, fits, factor="BTC"):
    if set(anchors) != set(fits) or factor not in anchors:
        raise ValueError("mismatched signal universe")
    returns = {}
    for symbol, endpoints in anchors.items():
        if (endpoints is None or fits[symbol]["beta"] is None):
            return None
        a, b = map(float, endpoints)
        if not all(math.isfinite(v) and v > 0 for v in (a, b)):
            raise ValueError("invalid signal anchor")
        returns[symbol] = math.log(b / a)
    return {s: returns[s] - fits[s]["beta"] * returns[factor] for s in returns}


def rank_targets(scores, per_leg=25):
    """Equal-dollar reversal; exact tied cutoff abstains for the entire basket."""
    if scores is None:
        return None
    if not scores or len(scores) % 2 or per_leg <= 0:
        raise ValueError("invalid basket")
    if any(not math.isfinite(v) for v in scores.values()):
        raise ValueError("invalid score")
    order = sorted(scores, key=scores.get)
    half = len(order) // 2
    if scores[order[half-1]] == scores[order[half]]:
        return {}
    return {s: per_leg if s in order[:half] else -per_leg for s in scores}


def cash_budget(rows, fixed_cost=Decimal(0)):
    """Fee headroom on unchanged settled fills, with unknown wallets explicit.

    Each row supplies net cash after original fees/funding, executed maker/taker
    notional, and *additional* maker/taker bp charges. Costs never change fills.
    A missing wallet withholds the full-cohort cash and break-even fee. Negative
    headroom means even zero additional friction cannot cover this fixed cost.
    """
    zero = Decimal(0)
    fixed_cost = Decimal(str(fixed_cost))
    if not fixed_cost.is_finite() or fixed_cost < 0:
        raise ValueError("invalid fixed cost")
    net = maker = taker = charge = zero
    known = 0
    for row in rows:
        if row["cash"] is None:
            continue
        vals = [Decimal(str(row[k])) for k in
                ("cash", "maker", "taker", "maker_bps", "taker_bps")]
        if not all(v.is_finite() for v in vals) or any(v < 0 for v in vals[1:]):
            raise ValueError("invalid cash or turnover/cost")
        c, m, t, mb, tb = vals
        net += c
        maker += m
        taker += t
        charge += (m * mb + t * tb) / 10000
        known += 1
    complete = known == len(rows)
    total = maker + taker
    return {"wallets": len(rows), "known": known, "unknown": len(rows)-known,
            "known_cash_before_additional_cost": str(net), "maker_turnover": str(maker),
            "taker_turnover": str(taker), "known_additional_cost": str(charge),
            "known_net_after_additional_cost": str(net-charge), "fixed_cost": str(fixed_cost),
            "complete_net_after_fixed": str(net-charge-fixed_cost) if complete else None,
            "uniform_additional_break_even_bps": str((net-fixed_cost)*10000/total)
            if complete and total else None}
