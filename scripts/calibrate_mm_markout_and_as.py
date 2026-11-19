#!/usr/bin/env python3
"""Calibrate Avellaneda-Stoikov (kappa, gamma) and multi-horizon markout quote floors (`S9-2`).

Implements empirical calibration replacing static guesses:
  1. Fit order-arrival intensity decay `kappa_bps` by Poisson maximum likelihood with an
     exposure offset, `fills_k ~ Poisson(exposure_secs_k * A * exp(-kappa_bps * delta_bps_k))`.
     Exposed bins with zero fills are evidence of decay and are kept (`S11b`; the previous
     log-linear fit dropped them and biased kappa low). The log-linear estimate over
     positive bins is still reported for comparison. Converts `as_kappa = kappa_bps * 10_000`;
     fails closed if non-decaying, unbounded, or under-identified.
  2. Derive inventory risk-budgeted `as_gamma` & `as_risk_budget_bps_at_max_pos` from
     empirical sigma (per sqrt(s)), time horizon T, and normalized max inventory.
  3. Compute `[1s, 5s, 30s, 60s]` empirical markout quantiles (`Q_p`, `p = 0.25 / 0.50`)
     and recommend `quote_floor_half_bps = maker_fee_bps + max_h(-Q_p) + 0.5 * min_profit_bps`.
"""
import argparse
import json
import math
from pathlib import Path

CURVE_HORIZONS = ("1s", "5s", "30s", "60s")


def _quantile(sorted_vals, p):
    if not sorted_vals:
        return None
    pos = max(0.0, min(1.0, p)) * (len(sorted_vals) - 1)
    lo = math.floor(pos)
    hi = math.ceil(pos)
    frac = pos - lo
    return sorted_vals[lo] + frac * (sorted_vals[hi] - sorted_vals[lo])


def _loglinear_positive_bins(rows):
    """Legacy fill-weighted log-linear fit over bins with fills (diagnostic only)."""
    valid = [(d, n, math.log(n / t)) for d, n, t in rows if n > 0.0]
    if len(valid) < 2 or len({d for d, _, _ in valid}) < 2:
        return None
    sum_w = sum(w for _, w, _ in valid)
    mean_d = sum(w * d for d, w, _ in valid) / sum_w
    mean_y = sum(w * y for _, w, y in valid) / sum_w
    sxx = sum(w * (d - mean_d) ** 2 for d, w, _ in valid)
    if sxx <= 0.0:
        return None
    sxy = sum(w * (d - mean_d) * (y - mean_y) for d, w, y in valid)
    return -sxy / sxx


def fit_exponential_kappa(depth_bins, *, min_total_fills=1.0):
    """Poisson MLE of `lambda(delta) = A * exp(-kappa * delta)` with exposure offsets.

    Profile likelihood: for fixed kappa, `A = N / sum_k T_k exp(-kappa d_k)`, and the score
    is `N * m(kappa) - sum_k n_k d_k` where `m` is the exposure-and-intensity weighted mean
    distance. `m` is strictly decreasing, so the root is unique when it exists.
    """
    rows = []
    for row in depth_bins:
        delta_bps = float(row["distance_bps"])
        fills = float(row["fill_count"])
        exposure_secs = float(row["exposure_secs"])
        if not (
            math.isfinite(delta_bps)
            and delta_bps >= 0.0
            and math.isfinite(exposure_secs)
            and exposure_secs > 0.0
            and math.isfinite(fills)
            and fills >= 0.0
        ):
            raise ValueError("invalid depth bin distance, fills, or exposure")
        rows.append((delta_bps, fills, exposure_secs))
    if len({d for d, _, _ in rows}) < 2:
        raise ValueError("need at least 2 distinct exposed quote distances to fit kappa")
    total_fills = sum(n for _, n, _ in rows)
    if total_fills < min_total_fills or total_fills <= 0.0:
        raise ValueError(f"need at least {min_total_fills} fills to fit kappa")
    fill_mean_d = sum(n * d for d, n, _ in rows) / total_fills
    d_min = min(d for d, _, _ in rows)

    def weighted_mean(kappa):
        # Shift by d_min for numerical stability at large kappa.
        w = [t * math.exp(-kappa * (d - d_min)) for d, _, t in rows]
        sw = sum(w)
        return sum(wi * d for wi, (d, _, _) in zip(w, rows)) / sw, sw, w

    m0, _, _ = weighted_mean(0.0)
    if fill_mean_d >= m0:
        raise ValueError(
            f"empirical fill intensity is non-decaying (fill-weighted distance {fill_mean_d:.4f} "
            f">= exposure-weighted {m0:.4f}); refuse unsafe fit"
        )
    if fill_mean_d <= d_min:
        raise ValueError("all fills sit at the nearest exposed distance; kappa is unbounded")
    lo, hi = 0.0, 1.0
    while weighted_mean(hi)[0] > fill_mean_d:
        hi *= 2.0
        if hi > 1e6:
            raise ValueError("kappa root not bracketed")
    for _ in range(200):
        mid = 0.5 * (lo + hi)
        if weighted_mean(mid)[0] > fill_mean_d:
            lo = mid
        else:
            hi = mid
    kappa_bps = 0.5 * (lo + hi)
    m, sw, w = weighted_mean(kappa_bps)
    var = sum(wi * (d - m) ** 2 for wi, (d, _, _) in zip(w, rows)) / sw
    # A = N / sum T exp(-kappa d) = N / (sw * exp(-kappa d_min)).
    log_a = math.log(total_fills) - math.log(sw) + kappa_bps * d_min
    return {
        "method": "poisson_mle_exposure_offset",
        "kappa_per_bps": kappa_bps,
        "kappa_se_per_bps": 1.0 / math.sqrt(total_fills * var) if var > 0.0 else None,
        "as_kappa_fractional": kappa_bps * 10_000.0,
        "arrival_intensity_at_zero_per_sec": math.exp(log_a),
        "bins_used": len(rows),
        "zero_fill_bins": sum(1 for _, n, _ in rows if n == 0.0),
        "total_fills": total_fills,
        "loglinear_positive_bins_kappa_per_bps": _loglinear_positive_bins(rows),
    }


def histogram_quantile(hist, p):
    """Linear-interpolated quantile of a fixed-width bps histogram.

    `hist = {"bin_bps", "min_bps", "counts", "underflow", "overflow"}`. Returns
    `(value_bps, censored)`; `censored` is True when the quantile falls in the
    underflow/overflow tail, in which case the finite edge is returned and the
    caller must treat the value as a bound rather than an estimate.
    """
    width = float(hist["bin_bps"])
    lo = float(hist["min_bps"])
    counts = [int(c) for c in hist["counts"]]
    under = int(hist.get("underflow", 0))
    over = int(hist.get("overflow", 0))
    if width <= 0.0 or any(c < 0 for c in counts) or under < 0 or over < 0:
        raise ValueError("invalid markout histogram")
    total = under + sum(counts) + over
    if total == 0:
        return None, False
    target = max(0.0, min(1.0, p)) * total
    if target <= under and under > 0:
        return lo, True
    cum = float(under)
    for i, c in enumerate(counts):
        if c and cum + c >= target:
            frac = (target - cum) / c
            return lo + (i + frac) * width, False
        cum += c
    return lo + len(counts) * width, True


def histogram_mean(hist):
    width = float(hist["bin_bps"])
    lo = float(hist["min_bps"])
    counts = [int(c) for c in hist["counts"]]
    n = sum(counts)
    if n == 0:
        return None
    return sum((lo + (i + 0.5) * width) * c for i, c in enumerate(counts)) / n


def summarize_multi_horizon_markout(
    samples_by_horizon=None, histograms_by_horizon=None, quantile_p=0.50, min_samples=1
):
    """Compute Q25, Q50, Q_p, mean, and adverse loss across `[1s, 5s, 30s, 60s]` markouts.

    Accepts raw samples or fixed-width histograms (recorded replay `calibration`).
    """
    samples_by_horizon = samples_by_horizon or {}
    histograms_by_horizon = histograms_by_horizon or {}
    summary = {}
    worst = {"q25": 0.0, "q50": 0.0, "qp": 0.0}
    censored_any = False
    qualifying = 0
    for h in CURVE_HORIZONS:
        if h in histograms_by_horizon:
            hist = histograms_by_horizon[h]
            q25, c25 = histogram_quantile(hist, 0.25)
            q50, c50 = histogram_quantile(hist, 0.50)
            qp, cp = histogram_quantile(hist, quantile_p)
            count = int(hist.get("underflow", 0)) + sum(int(c) for c in hist["counts"]) + int(hist.get("overflow", 0))
            mean = histogram_mean(hist)
            censored = c25 or c50 or cp
        else:
            vals = sorted(float(x) for x in samples_by_horizon.get(h, []) if math.isfinite(float(x)))
            count = len(vals)
            q25 = _quantile(vals, 0.25)
            q50 = _quantile(vals, 0.50)
            qp = _quantile(vals, quantile_p)
            mean = sum(vals) / len(vals) if vals else None
            censored = False
        if not count or q25 is None:
            summary[h] = {"count": 0, "mean_bps": None, "q25_bps": None, "q50_bps": None, "qp_bps": None}
            continue
        if count < min_samples:
            summary[h] = {"count": count, "excluded": "below_min_samples"}
            continue
        qualifying += 1
        censored_any = censored_any or censored
        worst["q25"] = max(worst["q25"], max(0.0, -q25))
        worst["q50"] = max(worst["q50"], max(0.0, -q50))
        worst["qp"] = max(worst["qp"], max(0.0, -qp))
        summary[h] = {
            "count": count,
            "mean_bps": mean,
            "q25_bps": q25,
            "q50_bps": q50,
            "qp_bps": qp,
            "censored": censored,
        }
    return {
        "horizons": summary,
        "quantile_p": quantile_p,
        "worst_adverse_q25_loss_bps": worst["q25"],
        "worst_adverse_q50_loss_bps": worst["q50"],
        "worst_adverse_qp_loss_bps": worst["qp"],
        "any_censored": censored_any,
        "qualifying_horizons": qualifying,
        "min_samples": min_samples,
    }


def calibrate_mm_parameters(
    *,
    depth_bins,
    sigma_per_sqrt_sec,
    markout_samples_by_horizon=None,
    markout_histograms_by_horizon=None,
    time_horizon_sec=1.0,
    max_pos_for_as=1.0,
    target_risk_budget_bps_at_max_pos=6.0,
    maker_fee_bps=0.0,
    min_profit_bps=1.0,
    quantile_p=0.50,
    min_markout_samples=20,
    min_total_fills=20,
):
    """Compute calibrated A-S `(as_gamma, as_kappa, as_risk_budget_bps_at_max_pos)` + markout floor."""
    if not (sigma_per_sqrt_sec > 0.0 and math.isfinite(sigma_per_sqrt_sec)):
        raise ValueError("sigma_per_sqrt_sec must be positive")
    if not (time_horizon_sec > 0.0 and max_pos_for_as > 0.0 and target_risk_budget_bps_at_max_pos > 0.0):
        raise ValueError("horizon, max_pos_for_as, and target_risk_budget must be positive")
    if not (0.0 < quantile_p <= 0.5):
        raise ValueError("quantile_p must lie in (0.0, 0.5]")

    if not markout_samples_by_horizon and not markout_histograms_by_horizon:
        raise ValueError("markout samples or histograms are required")
    if int(min_markout_samples) < 1:
        raise ValueError("min_markout_samples must be at least 1")
    kappa_fit = fit_exponential_kappa(depth_bins, min_total_fills=min_total_fills)
    curve = summarize_multi_horizon_markout(
        markout_samples_by_horizon, markout_histograms_by_horizon, quantile_p, int(min_markout_samples)
    )
    if curve["qualifying_horizons"] == 0:
        raise ValueError(
            f"no markout horizon has >= {min_markout_samples} labelled fills; refuse to emit a floor"
        )
    if curve["any_censored"]:
        raise ValueError("markout quantile falls in a histogram tail; widen bins before calibrating")
    adverse_loss = curve["worst_adverse_qp_loss_bps"]

    denom = (sigma_per_sqrt_sec ** 2) * time_horizon_sec * max_pos_for_as
    recommended_gamma = (target_risk_budget_bps_at_max_pos / 10_000.0) / denom
    gamma_safe = max(recommended_gamma, 1e-6)
    as_kappa = kappa_fit["as_kappa_fractional"]
    as_half_spread_bps = (
        0.5 * (recommended_gamma * (sigma_per_sqrt_sec ** 2) * time_horizon_sec + (2.0 / gamma_safe) * math.log1p(gamma_safe / as_kappa))
    ) * 10_000.0
    markout_half_floor_bps = maker_fee_bps + adverse_loss + 0.5 * min_profit_bps

    return {
        "kappa_fit": kappa_fit,
        "markout_curve": curve,
        "recommended_config": {
            "as_kappa": as_kappa,
            "as_gamma": recommended_gamma,
            "as_risk_budget_bps_at_max_pos": target_risk_budget_bps_at_max_pos,
            "markout_reserve_quantile_floor_p": quantile_p,
            "recommended_half_spread_floor_bps": max(as_half_spread_bps, markout_half_floor_bps),
            "as_half_spread_bps": as_half_spread_bps,
            "markout_half_spread_floor_bps": markout_half_floor_bps,
        },
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("input_json", type=Path)
    args = parser.parse_args()
    payload = json.loads(args.input_json.read_text())
    result = calibrate_mm_parameters(**payload)
    print(json.dumps(result, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
