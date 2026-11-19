#!/usr/bin/env python3
"""Full multiple-testing retrospective audit: PSR, N_eff, DSR, MinTRL, and CSCV PBO (`S9-4`).

Follows Bailey & López de Prado (2014, 'The Deflated Sharpe Ratio') and Bailey et al.
(2016, 'The Probability of Backtest Overfitting') using pure Python stdlib so no new quant
variants are introduced before full retrospective accounting passes (`Priority #4`).
"""
import argparse
from collections import defaultdict
import itertools
import json
import math
from pathlib import Path

EULER_MASCHERONI = 0.5772156649015329
SQRT_2 = math.sqrt(2.0)


def norm_cdf(x):
    return 0.5 * (1.0 + math.erf(x / SQRT_2))


def norm_ppf(p):
    """Peter John Acklam inverse normal CDF rational approximation."""
    if not (0.0 < p < 1.0):
        raise ValueError("p must lie strictly inside (0, 1)")
    a = (
        -3.969683028665376e01,
        2.209460984245205e02,
        -2.759285104469687e02,
        1.383577518672690e02,
        -3.066479806614716e01,
        2.506628277459239e00,
    )
    b = (
        -5.447609879822406e01,
        1.615858368580409e02,
        -1.556989798598866e02,
        6.680131188771972e01,
        -1.328068155288572e01,
    )
    c = (
        -7.784894002430293e-03,
        -3.223964580411365e-01,
        -2.400758277161838e00,
        -2.549732539343734e00,
        4.374664141464968e00,
        2.938163982698783e00,
    )
    d = (
        7.784695709041462e-03,
        3.224671290700398e-01,
        2.445134137142996e00,
        3.754408661907416e00,
    )
    p_low = 0.02425
    p_high = 1.0 - p_low
    if p < p_low:
        q = math.sqrt(-2.0 * math.log(p))
        num = (((((c[0] * q + c[1]) * q + c[2]) * q + c[3]) * q + c[4]) * q + c[5])
        den = ((((d[0] * q + d[1]) * q + d[2]) * q + d[3]) * q + 1.0)
        return num / den
    if p <= p_high:
        q = p - 0.5
        r = q * q
        num = (((((a[0] * r + a[1]) * r + a[2]) * r + a[3]) * r + a[4]) * r + a[5]) * q
        den = (((((b[0] * r + b[1]) * r + b[2]) * r + b[3]) * r + b[4]) * r + 1.0)
        return num / den
    q = math.sqrt(-2.0 * math.log(1.0 - p))
    num = (((((c[0] * q + c[1]) * q + c[2]) * q + c[3]) * q + c[4]) * q + c[5])
    den = ((((d[0] * q + d[1]) * q + d[2]) * q + d[3]) * q + 1.0)
    return -(num / den)


def series_moments(returns):
    """Return `(mean, std, sr, t_stat, skew, pearson_kurtosis, t_count)` for non-annualized returns."""
    vals = [float(x) for x in returns]
    if not all(math.isfinite(x) for x in vals):
        raise ValueError("trial observations must be finite on every aligned window")
    t = len(vals)
    if t < 3:
        raise ValueError("need at least 3 observations for Sharpe/moments")
    mean = sum(vals) / t
    centered = [x - mean for x in vals]
    m2 = sum(c * c for c in centered) / t
    s2 = sum(c * c for c in centered) / (t - 1)
    std = math.sqrt(s2)
    if std <= 0.0:
        return {"t": t, "mean": mean, "std": 0.0, "sr": 0.0, "t_stat": 0.0, "skew": 0.0, "kurtosis": 3.0}
    sr = mean / std
    se_mean = std / math.sqrt(t)
    t_stat = mean / se_mean
    m3 = sum(c ** 3 for c in centered) / t
    m4 = sum(c ** 4 for c in centered) / t
    skew = m3 / (m2 ** 1.5)
    kurt = m4 / (m2 ** 2)
    return {
        "t": t,
        "mean": mean,
        "std": std,
        "sr": sr,
        "t_stat": t_stat,
        "skew": skew,
        "kurtosis": kurt,
    }


def probabilistic_sharpe_ratio(moments, sr_benchmark=0.0):
    """PSR(SR*) accounting for non-normal skewness and Pearson kurtosis (Bailey & LdP 2012)."""
    t = moments["t"]
    sr = moments["sr"]
    var_factor = 1.0 - moments["skew"] * sr + ((moments["kurtosis"] - 1.0) / 4.0) * (sr ** 2)
    var_factor = max(var_factor, 1e-6)
    z = (sr - sr_benchmark) * math.sqrt(t - 1) / math.sqrt(var_factor)
    return norm_cdf(z)


def min_track_record_length(moments, *, z_target=1.6448536269514722, sr_benchmark=0.0):
    """Minimum number of independent windows needed for SR > SR* at significance z."""
    diff = moments["sr"] - sr_benchmark
    if diff <= 0.0:
        return None
    var_factor = 1.0 - moments["skew"] * moments["sr"] + ((moments["kurtosis"] - 1.0) / 4.0) * (moments["sr"] ** 2)
    var_factor = max(var_factor, 1e-6)
    return 1.0 + var_factor * ((z_target / diff) ** 2)


def _pearson_corr(x, y):
    n = len(x)
    mx = sum(x) / n
    my = sum(y) / n
    num = sum((a - mx) * (b - my) for a, b in zip(x, y))
    denx = sum((a - mx) ** 2 for a in x)
    deny = sum((b - my) ** 2 for b in y)
    if denx <= 0.0 or deny <= 0.0:
        return 0.0
    return max(-1.0, min(1.0, num / math.sqrt(denx * deny)))


def effective_num_trials(trials_matrix):
    """Compute correlation-adjusted effective trials N_eff = M^2 / ||R||_F^2 from M aligned series."""
    m = len(trials_matrix)
    if m <= 1:
        return float(m)
    frob_sq = 0.0
    for i in range(m):
        frob_sq += 1.0
        for j in range(i + 1, m):
            rho = _pearson_corr(trials_matrix[i], trials_matrix[j])
            frob_sq += 2.0 * (rho ** 2)
    return max(1.0, (m * m) / max(frob_sq, 1.0))


def expected_max_sharpe(trial_sharpes, n_trials):
    """Expected maximum SR across N trials with cross-trial variance V[SR_k]."""
    m = len(trial_sharpes)
    if n_trials <= 1.0 or m < 2:
        return 0.0
    mean_sr = sum(trial_sharpes) / m
    var_sr = sum((s - mean_sr) ** 2 for s in trial_sharpes) / max(m - 1, 1)
    if var_sr <= 0.0:
        return 0.0
    z1 = norm_ppf(1.0 - 1.0 / n_trials)
    z2 = norm_ppf(1.0 - 1.0 / (n_trials * math.e))
    # The large-N approximation can turn negative near one effective trial.
    # Searching multiple centered trials cannot lower the zero-trial benchmark.
    return max(0.0, math.sqrt(var_sr) * ((1.0 - EULER_MASCHERONI) * z1 + EULER_MASCHERONI * z2))


def deflated_sharpe_report(trial_dict):
    """Evaluate best trial PSR, DSR(M_raw), DSR(N_eff), and MinTRL for t=1.645 (95%) and t=2.0."""
    names = sorted(trial_dict.keys())
    matrix = [list(trial_dict[k]) for k in names]
    if not matrix or any(len(s) != len(matrix[0]) for s in matrix):
        raise ValueError("all trial series must be aligned and non-empty")

    per_trial = {}
    for name, series in zip(names, matrix):
        mom = series_moments(series)
        mom["psr_zero"] = probabilistic_sharpe_ratio(mom, 0.0)
        mom["min_trl_95"] = min_track_record_length(mom, z_target=norm_ppf(0.95), sr_benchmark=0.0)
        mom["min_trl_t2"] = min_track_record_length(mom, z_target=2.0, sr_benchmark=0.0)
        per_trial[name] = mom

    best_name = max(names, key=lambda n: per_trial[n]["sr"])
    best_mom = per_trial[best_name]
    sharpes = [per_trial[n]["sr"] for n in names]
    m_raw = len(names)
    n_eff = effective_num_trials(matrix)

    sr_thresh_raw = expected_max_sharpe(sharpes, float(m_raw))
    sr_thresh_eff = expected_max_sharpe(sharpes, n_eff)

    return {
        "best_trial": best_name,
        "windows": len(matrix[0]),
        "num_trials_raw": m_raw,
        "effective_independent_trials": n_eff,
        "best_trial_moments": best_mom,
        "sr_threshold_raw": sr_thresh_raw,
        "sr_threshold_effective": sr_thresh_eff,
        "dsr_raw": probabilistic_sharpe_ratio(best_mom, sr_thresh_raw),
        "dsr_effective": probabilistic_sharpe_ratio(best_mom, sr_thresh_eff),
        "trials": per_trial,
    }


def cscv_pbo(trial_dict, num_blocks=8):
    """Combinatorially Symmetric Cross-Validation Probability of Backtest Overfitting (Bailey et al. 2016)."""
    if num_blocks < 4 or num_blocks % 2 != 0:
        raise ValueError("num_blocks must be an even integer >= 4")
    names = sorted(trial_dict.keys())
    m = len(names)
    if m < 2:
        raise ValueError("CSCV PBO requires at least 2 candidate strategies")
    t = len(trial_dict[names[0]])
    if any(len(trial_dict[name]) != t for name in names):
        raise ValueError("all trial series must share the same window roster")
    if t < num_blocks * 2:
        raise ValueError("not enough windows to populate CSCV blocks")

    block_size = t // num_blocks
    t_used = block_size * num_blocks
    blocks = []
    for s in range(num_blocks):
        lo = s * block_size
        hi = lo + block_size
        blocks.append({n: list(trial_dict[n][lo:hi]) for n in names})

    half = num_blocks // 2
    logits = []
    oos_nonpos = 0.0
    tied_splits = 0
    for is_indices in itertools.combinations(range(num_blocks), half):
        is_set = set(is_indices)
        is_sr = {}
        oos_sr = {}
        for n in names:
            is_vals = [x for idx in is_indices for x in blocks[idx][n]]
            oos_vals = [x for idx in range(num_blocks) if idx not in is_set for x in blocks[idx][n]]
            is_sr[n] = series_moments(is_vals)["sr"]
            oos_sr[n] = series_moments(oos_vals)["sr"]

        best = max(is_sr.values())
        winners = [name for name in names if is_sr[name] == best]
        tied_splits += len(winners) > 1
        weight = 1.0 / len(winners)
        for name in winners:
            score = oos_sr[name]
            oos_nonpos += weight * (score <= 0.0)
            lower = sum(other < score for other in oos_sr.values())
            tied = sum(other == score for other in oos_sr.values())
            rel_rank = (lower + (tied + 1) / 2) / (m + 1)
            logits.append((math.log(rel_rank / (1.0 - rel_rank)), weight))

    n_splits = math.comb(num_blocks, half)
    pbo = math.fsum(w for value, w in logits if value <= 0.0) / n_splits
    cumulative = 0.0
    for median_logit, weight in sorted(logits):
        cumulative += weight
        if cumulative >= n_splits / 2:
            break
    return {
        "blocks": num_blocks,
        "windows_used": t_used,
        "splits": n_splits,
        "strategies": m,
        "pbo": pbo,
        "prob_is_winner_oos_nonpositive": oos_nonpos / n_splits,
        "median_logit": median_logit,
        "in_sample_tied_splits": tied_splits,
    }


def collect_local_multiple_testing_audit(base_dir: Path, research_dir: Path | None = None):
    """Run read-only DSR / MinTRL / CSCV PBO over local `.agent-state/` retrospective campaigns."""
    out = {"cohorts": {}, "methodology_inventory_records": 0}

    # 1. Early Jan-Apr 4-coin cross-sectional (fixed momentum vs reversal vs risk_scaled across 209 covered).
    early_path = base_dir / "early_rank_2026_v1" / "units.json"
    if early_path.is_file():
        rows = json.loads(early_path.read_text())["rows"]
        arms = ("fixed", "reverse_fixed", "risk_scaled")
        covered = sorted((r for r in rows if all(r.get("arms", {}).get(a, {}).get("net") is not None for a in arms)), key=lambda r: r["start"])
        if len(covered) >= 24:
            series = {a: [float(r["arms"][a]["net"]) for r in covered] for a in arms}
            dsr = deflated_sharpe_report(series)
            pbo = cscv_pbo(series, num_blocks=8)
            out["cohorts"]["early_rank_2026_3arms"] = {
                "dsr": dsr,
                "cscv": pbo,
                "net_sum_by_arm": {a: sum(series[a]) for a in arms},
                "scheduled": len(rows),
                "zero_capacity_by_arm": {a: sum(not r["arms"][a]["active"] for r in covered) for a in arms},
            }

    # 2. Cross-domain carry 78 temporal (4 funding/basis strategies across 4 symbols).
    bh_path = base_dir / "basis_hours_v2" / "units.json"
    if bh_path.is_file():
        rows = json.loads(bh_path.read_text())
        by_track = defaultdict(list)
        for row in rows:
            if row.get("split") in {"temporal", "late"}:
                by_track[row["track"]].append(row)
        for track, roster in sorted(by_track.items()):
            trials = defaultdict(dict)
            starts = sorted({r["start"] for r in roster})
            for row in roster:
                trial = trials[(row["sym"], row["arm"])]
                if row["start"] in trial:
                    raise ValueError("duplicate track, market, arm and start in carry inventory")
                trial[row["start"]] = row.get("net025")
            common = [t for t in starts if all(values.get(t) is not None for values in trials.values())]
            aligned = {f"{sym}_{arm}": [float(values[t]) for t in common] for (sym, arm), values in trials.items()}
            constant = sorted(name for name, values in aligned.items() if len(set(values)) <= 1)
            cohort = {
                "track": track, "scheduled": len(starts), "common": len(common), "constant_trials": constant,
                "missing_by_trial": {f"{sym}_{arm}": sum(values.get(t) is None for t in starts)
                                     for (sym, arm), values in trials.items()},
            }
            varying = {name: values for name, values in aligned.items() if name not in constant}
            if len(varying) >= 2 and len(common) >= 16:
                cohort.update(dsr=deflated_sharpe_report(varying), cscv=cscv_pbo(varying, num_blocks=8))
            key = "basis_hours_v2_temporal" if track == "arb" else f"basis_hours_v2_temporal_{track}"
            out["cohorts"][key] = cohort

    # 3. Codex Sprint 8 Core BTC $1,000 / 25x capital transfer OOS vs non-Taiwan generalization check.
    cap_dir = base_dir / "sprint8_capital_transfer_v2"
    if cap_dir.is_dir():
        existing_files = sorted(cap_dir.glob("existing-*.json"))
        fresh_files = sorted(cap_dir.glob("fresh-*.json"))
        cap_rows = []
        if existing_files:
            cap_rows.extend(json.loads(existing_files[-1].read_text()).get("rows", []))
        if fresh_files:
            cap_rows.extend(json.loads(fresh_files[-1].read_text()).get("rows", []))
        settled_var = [float(r.get("raw_cash", r["net"])) for r in cap_rows if r.get("status") == "settled"]
        if len(settled_var) >= 4:
            mom = series_moments(settled_var)
            mom["psr_zero"] = probabilistic_sharpe_ratio(mom, 0.0)
            mom["min_trl_95"] = min_track_record_length(mom, z_target=norm_ppf(0.95), sr_benchmark=0.0)
            out["cohorts"]["sprint8_core_btc_1k_25x_12w"] = mom

    if research_dir is not None:
        inv = research_dir / "methodology_inventory.ndjson"
        if inv.is_file():
            out["methodology_inventory_records"] = sum(1 for ln in inv.read_text().splitlines() if ln.strip())
    return out


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--summary-dir", type=Path, default=Path(".agent-state/forward_ops_20261118"))
    parser.add_argument("--research-dir", type=Path, default=Path(".agent-state/research"))
    parser.add_argument("--output", type=Path, default=None)
    args = parser.parse_args()
    rep = collect_local_multiple_testing_audit(args.summary_dir, args.research_dir)
    text = json.dumps(rep, indent=2, sort_keys=True)
    if args.output is not None:
        args.output.write_text(text + "\n")
    else:
        print(text)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
