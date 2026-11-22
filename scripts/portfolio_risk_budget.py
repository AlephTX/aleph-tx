#!/usr/bin/env python3
"""Offline portfolio risk-budgeting and crash stress helpers (`CRASH-IMP-03`).

Pure stdlib implementation for multi-market Carry/Arb sizing and tail risk checks:
- Capped inverse-volatility weights
- Crisis correlation floor (`stress_rho_floor`) & Euler risk contributions
- Non-normal empirical Expected Shortfall (CVaR)
- Strictly capped fractional Kelly scaling (`<= max_scale <= 1.0`, `0.0` if `mean <= 0`)
- Real-time drawdown budget brake parity with `src/daily_loss.rs` (`CRASH-IMP-01`)
- 5-dimension perp stress evaluator
"""

from __future__ import annotations

import math
from typing import Sequence


def _validate_positive_finite(values: Sequence[float], label: str) -> list[float]:
    if not values:
        raise ValueError(f"{label} must be non-empty")
    out = [float(v) for v in values]
    if any(not math.isfinite(v) or v <= 0.0 for v in out):
        raise ValueError(f"{label} must contain positive finite values")
    return out


def _validate_corr_matrix(corr: Sequence[Sequence[float]], n: int) -> list[list[float]]:
    if len(corr) != n:
        raise ValueError(f"corr row count {len(corr)} != {n}")
    mat = [[float(x) for x in row] for row in corr]
    for i, row in enumerate(mat):
        if len(row) != n:
            raise ValueError(f"corr row {i} length {len(row)} != {n}")
        if not math.isfinite(row[i]) or abs(row[i] - 1.0) > 1e-6:
            raise ValueError(f"corr diagonal ({i},{i}) must equal 1.0")
        for j, val in enumerate(row):
            if not math.isfinite(val) or val < -1.0 - 1e-9 or val > 1.0 + 1e-9:
                raise ValueError(f"corr[{i}][{j}] out of [-1, 1]: {val}")
            if abs(val - mat[j][i]) > 1e-6:
                raise ValueError(f"corr matrix must be symmetric at ({i},{j})")
    return mat


def inverse_vol_weights(vols: Sequence[float]) -> list[float]:
    """Compute normalized inverse-volatility weights summing to `1.0`."""
    clean = _validate_positive_finite(vols, "vols")
    inv = [1.0 / v for v in clean]
    denom = sum(inv)
    return [x / denom for x in inv]


def stressed_corr_matrix(
    corr: Sequence[Sequence[float]],
    stress_rho_floor: float = 0.70,
) -> list[list[float]]:
    """Lift off-diagonal correlations to at least `stress_rho_floor` for crisis sizing."""
    if not math.isfinite(stress_rho_floor) or not (0.0 <= stress_rho_floor <= 1.0):
        raise ValueError("stress_rho_floor must be in [0, 1]")
    mat = _validate_corr_matrix(corr, len(corr))
    n = len(mat)
    return [
        [1.0 if i == j else max(mat[i][j], stress_rho_floor) for j in range(n)]
        for i in range(n)
    ]


def portfolio_vol_and_rc(
    weights: Sequence[float],
    vols: Sequence[float],
    corr: Sequence[Sequence[float]],
) -> tuple[float, list[float]]:
    """Return `(portfolio_vol, fractional_risk_contributions)` under `corr`."""
    clean_vols = _validate_positive_finite(vols, "vols")
    n = len(clean_vols)
    if len(weights) != n:
        raise ValueError("weights and vols must have equal length")
    w = [float(x) for x in weights]
    if any(not math.isfinite(x) or x < 0.0 for x in w):
        raise ValueError("weights must be non-negative finite values")
    mat = _validate_corr_matrix(corr, n)

    cov_w = [0.0] * n
    for i in range(n):
        acc = 0.0
        for j in range(n):
            acc += clean_vols[i] * clean_vols[j] * mat[i][j] * w[j]
        cov_w[i] = acc
    port_var = sum(w[i] * cov_w[i] for i in range(n))
    if port_var <= 0.0:
        raise ValueError("portfolio variance must be positive")
    port_vol = math.sqrt(port_var)
    rcs = [(w[i] * cov_w[i]) / port_var for i in range(n)]
    return port_vol, rcs


def empirical_var_es(
    returns: Sequence[float],
    alpha: float = 0.975,
) -> tuple[float, float]:
    """Compute non-normal left-tail VaR and Expected Shortfall as positive loss magnitudes."""
    if not returns:
        raise ValueError("returns must be non-empty")
    if not math.isfinite(alpha) or not (0.5 < alpha < 1.0):
        raise ValueError("alpha must be in (0.5, 1.0)")
    clean = [float(r) for r in returns]
    if any(not math.isfinite(r) for r in clean):
        raise ValueError("returns must be finite")
    losses = sorted((-r for r in clean), reverse=True)
    tail_count = max(1, int(math.ceil(round((1.0 - alpha) * len(losses), 12))))
    var_loss = losses[tail_count - 1]
    es_loss = sum(losses[:tail_count]) / tail_count
    return var_loss, es_loss


def capped_fractional_kelly(
    mean_return: float,
    variance: float,
    fraction: float = 0.25,
    max_scale: float = 1.0,
) -> float:
    """Return fractional Kelly sizing strictly capped in `[0.0, max_scale]` (`<= 1.0`)."""
    if not math.isfinite(fraction) or not (0.0 < fraction <= 0.5):
        raise ValueError("fraction must be in (0, 0.5]")
    if not math.isfinite(max_scale) or not (0.0 < max_scale <= 1.0):
        raise ValueError("max_scale must be in (0, 1.0]")
    if not math.isfinite(mean_return) or not math.isfinite(variance):
        raise ValueError("mean_return and variance must be finite")
    if mean_return <= 0.0 or variance <= 0.0:
        return 0.0
    raw = fraction * (mean_return / variance)
    return max(0.0, min(max_scale, raw))


def real_time_drawdown_brake_scale(
    peak_equity: float,
    equity: float,
    daily_budget_usd: float,
    prev_tier: int = 0,
) -> tuple[int, float]:
    """Python mirror of `DailyLossGuard` progressive brake (`CRASH-IMP-01`).

    Returns `(next_tier, scale)` with `0.05` recovery hysteresis and strict cap `<= 1.0`.
    """
    if not (math.isfinite(peak_equity) and math.isfinite(equity) and math.isfinite(daily_budget_usd)):
        raise ValueError("inputs must be finite")
    if daily_budget_usd <= 0.0:
        raise ValueError("daily_budget_usd must be positive")
    r_dd = max(0.0, peak_equity - equity) / daily_budget_usd
    if r_dd >= 1.0:
        return 4, 0.0
    if r_dd >= 0.75:
        escalated = 3
    elif r_dd >= 0.50:
        escalated = 2
    elif r_dd >= 0.25:
        escalated = 1
    else:
        escalated = 0

    tier = max(0, min(3, prev_tier))
    if escalated >= tier:
        tier = escalated
    else:
        if tier == 3 and r_dd < 0.70:
            tier = 2
        if tier == 2 and r_dd < 0.45:
            tier = 1
        if tier == 1 and r_dd < 0.20:
            tier = 0
    scale = {0: 1.0, 1: 0.75, 2: 0.50, 3: 0.25}[tier]
    return tier, scale


def evaluate_perp_stress_scenarios(
    gross_notional_usd: float,
    leg_vols: Sequence[float],
    weights: Sequence[float],
    corr: Sequence[Sequence[float]],
    *,
    basis_shock_bps: float = 40.0,
    hedge_lag_slip_bps: float = 15.0,
    depth_drop_frac: float = 0.80,
    funding_flip_bps_10h: float = 10.0,
    stress_rho_floor: float = 0.70,
) -> dict[str, float]:
    """Evaluate 5-dimension perp stress loss and crisis volatility amplification."""
    if not math.isfinite(gross_notional_usd) or gross_notional_usd <= 0.0:
        raise ValueError("gross_notional_usd must be positive finite")
    if not (0.0 <= depth_drop_frac < 1.0):
        raise ValueError("depth_drop_frac must be in [0, 1)")
    base_vol, _ = portfolio_vol_and_rc(weights, leg_vols, corr)
    stress_corr = stressed_corr_matrix(corr, stress_rho_floor=stress_rho_floor)
    stress_vol, stress_rc = portfolio_vol_and_rc(weights, leg_vols, stress_corr)

    basis_loss_usd = gross_notional_usd * max(0.0, basis_shock_bps) / 1e4
    # Thin-depth multiplier penalizes hedge execution slippage as L1-L3 depth evaporates.
    thin_slip_mult = 1.0 / max(0.10, 1.0 - depth_drop_frac)
    hedge_lag_loss_usd = gross_notional_usd * max(0.0, hedge_lag_slip_bps) * thin_slip_mult / 1e4
    funding_flip_loss_usd = gross_notional_usd * max(0.0, funding_flip_bps_10h) / 1e4
    return {
        "base_portfolio_vol": base_vol,
        "stressed_portfolio_vol": stress_vol,
        "vol_stress_multiplier": stress_vol / base_vol,
        "max_stressed_rc": max(stress_rc),
        "basis_shock_loss_usd": basis_loss_usd,
        "thin_hedge_lag_loss_usd": hedge_lag_loss_usd,
        "funding_flip_10h_loss_usd": funding_flip_loss_usd,
        "combined_crash_loss_usd": basis_loss_usd + hedge_lag_loss_usd + funding_flip_loss_usd,
    }
