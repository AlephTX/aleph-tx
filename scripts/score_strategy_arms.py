#!/usr/bin/env python3
"""Normalized P0 metric scorer and +/-30% notional comparability gate for AlephTX strategies.

Enforces `net / traded_notional` (bp) and `net / roundtrip` / `net / fee` as primary metrics
instead of raw dollar PnL, verifies +/-30% traded-notional comparability before ranking maker
arms, computes OLS `net ~ traded_notional` regression, separates strict `cont == true` basis
windows, checks OFAT monotonicity, and emits strict JSON (`allow_nan=False`).
"""

from __future__ import annotations

import argparse
from collections import defaultdict
import json
import math
from pathlib import Path
import re
import sys
from typing import Any, Sequence

DEFAULT_MAX_NOTIONAL_DIFF_FRAC = 0.30
DEFAULT_MIN_BASIS_ROUNDTRIPS = 30


def require_finite_float(val: Any, name: str) -> float:
    if isinstance(val, bool) or not isinstance(val, (int, float)):
        raise ValueError(f"{name} must be a real number, got {type(val).__name__}")
    out = float(val)
    if not math.isfinite(out):
        raise ValueError(f"{name} must be finite, got {val}")
    return out


def require_int(val: Any, name: str) -> int:
    if isinstance(val, bool) or not isinstance(val, int):
        raise ValueError(f"{name} must be an integer, got {type(val).__name__}")
    return val


def require_bool(val: Any, name: str) -> bool:
    if not isinstance(val, bool):
        raise ValueError(f"{name} must be bool, got {type(val).__name__}")
    return val


def require_non_empty_str(val: Any, name: str) -> str:
    if not isinstance(val, str) or not val.strip():
        raise ValueError(f"{name} must be a non-empty string")
    return val


def _extract_pipeline_row(raw: dict[str, Any], idx: int) -> dict[str, Any]:
    if not isinstance(raw, dict):
        raise ValueError(f"pipeline[{idx}] must be an object")
    replay = raw.get("replay")
    if isinstance(replay, dict):
        eval_ledger = replay.get("eval_ledger", {})
        net = require_finite_float(
            raw.get("net", eval_ledger.get("paired_cycle_net_cash_usd")),
            f"pipeline[{idx}].net",
        )
        ntl_default = eval_ledger.get("passive_exit_notional_usd", 0.0) + eval_ledger.get(
            "taker_exit_notional_usd", 0.0
        )
        ntl = require_finite_float(
            raw.get("ntl", replay.get("eval_traded_notional_usd", ntl_default)),
            f"pipeline[{idx}].ntl",
        )
        cyc = require_int(
            raw.get("cyc", eval_ledger.get("closed_cycles", 0)),
            f"pipeline[{idx}].cyc",
        )
        can = require_int(
            raw.get("can", replay.get("reprice_cancels", 0)),
            f"pipeline[{idx}].can",
        )
        haz = require_int(
            raw.get("haz", replay.get("hazard_ioc_fills", 0)),
            f"pipeline[{idx}].haz",
        )
        qpres = require_int(
            raw.get("qpres", replay.get("queue_preserved_ticks", 0)),
            f"pipeline[{idx}].qpres",
        )
        cont = require_bool(
            raw.get("cont", replay.get("is_full_10h_continuous", False)),
            f"pipeline[{idx}].cont",
        )
        pt_fills = require_int(
            raw.get("price_through_fills", replay.get("price_through_fills", 0)),
            f"pipeline[{idx}].price_through_fills",
        )
        sc_fills = require_int(
            raw.get("sweep_cleared_fills", replay.get("sweep_cleared_fills", 0)),
            f"pipeline[{idx}].sweep_cleared_fills",
        )
        st_fills = require_int(
            raw.get("sweep_touch_fills", replay.get("sweep_touch_fills", 0)),
            f"pipeline[{idx}].sweep_touch_fills",
        )
        tr_fills = require_int(
            raw.get("stride_touch_fills", replay.get("stride_touch_fills", 0)),
            f"pipeline[{idx}].stride_touch_fills",
        )
        mk_usd_raw = raw.get(
            "markout_1s_usd", eval_ledger.get("cumulative_1s_markout_usd")
        )
        buy_sweeps = require_int(
            raw.get("inferred_buy_sweeps", raw.get("buy_sweeps", 0)),
            f"pipeline[{idx}].inferred_buy_sweeps",
        )
        sell_sweeps = require_int(
            raw.get("inferred_sell_sweeps", raw.get("sell_sweeps", 0)),
            f"pipeline[{idx}].inferred_sell_sweeps",
        )
    else:
        net = require_finite_float(raw.get("net"), f"pipeline[{idx}].net")
        ntl = require_finite_float(raw.get("ntl"), f"pipeline[{idx}].ntl")
        cyc = require_int(raw.get("cyc", 0), f"pipeline[{idx}].cyc")
        can = require_int(raw.get("can", 0), f"pipeline[{idx}].can")
        haz = require_int(raw.get("haz", 0), f"pipeline[{idx}].haz")
        qpres = require_int(raw.get("qpres", 0), f"pipeline[{idx}].qpres")
        cont = require_bool(raw.get("cont", False), f"pipeline[{idx}].cont")
        pt_fills = require_int(
            raw.get("price_through_fills", raw.get("pt_fills", 0)),
            f"pipeline[{idx}].price_through_fills",
        )
        sc_fills = require_int(
            raw.get("sweep_cleared_fills", raw.get("sc_fills", 0)),
            f"pipeline[{idx}].sweep_cleared_fills",
        )
        st_fills = require_int(
            raw.get("sweep_touch_fills", raw.get("st_fills", 0)),
            f"pipeline[{idx}].sweep_touch_fills",
        )
        tr_fills = require_int(
            raw.get("stride_touch_fills", raw.get("tr_fills", 0)),
            f"pipeline[{idx}].stride_touch_fills",
        )
        buy_sweeps = require_int(
            raw.get("inferred_buy_sweeps", raw.get("buy_sweeps", 0)),
            f"pipeline[{idx}].inferred_buy_sweeps",
        )
        sell_sweeps = require_int(
            raw.get("inferred_sell_sweeps", raw.get("sell_sweeps", 0)),
            f"pipeline[{idx}].inferred_sell_sweeps",
        )
        mk_usd_raw = raw.get("markout_1s_usd")
        if mk_usd_raw is None and raw.get("markout_1s_bps") is not None:
            mk_bps = require_finite_float(
                raw["markout_1s_bps"], f"pipeline[{idx}].markout_1s_bps"
            )
            mk_usd_raw = mk_bps * ntl / 10_000.0

    mk_usd = (
        require_finite_float(mk_usd_raw, f"pipeline[{idx}].markout_1s_usd")
        if mk_usd_raw is not None
        else None
    )

    return {
        "win": require_non_empty_str(raw.get("win"), f"pipeline[{idx}].win"),
        "sym": require_non_empty_str(raw.get("sym"), f"pipeline[{idx}].sym"),
        "arm": require_non_empty_str(raw.get("arm"), f"pipeline[{idx}].arm"),
        "cont": cont,
        "net": net,
        "ntl": ntl,
        "cyc": cyc,
        "can": can,
        "haz": haz,
        "qpres": qpres,
        "price_through_fills": pt_fills,
        "sweep_cleared_fills": sc_fills,
        "sweep_touch_fills": st_fills,
        "stride_touch_fills": tr_fills,
        "inferred_buy_sweeps": buy_sweeps,
        "inferred_sell_sweeps": sell_sweeps,
        "markout_1s_usd": mk_usd,
    }


def _extract_basis_row(raw: dict[str, Any], idx: int) -> dict[str, Any]:
    if not isinstance(raw, dict):
        raise ValueError(f"basis[{idx}] must be an object")
    ntl_raw = raw.get("ntl", raw.get("traded_notional_usd"))
    ntl = (
        require_finite_float(ntl_raw, f"basis[{idx}].ntl")
        if ntl_raw is not None
        else None
    )
    return {
        "win": require_non_empty_str(raw.get("win"), f"basis[{idx}].win"),
        "sym": require_non_empty_str(raw.get("sym"), f"basis[{idx}].sym"),
        "arm": require_non_empty_str(raw.get("arm"), f"basis[{idx}].arm"),
        "cont": require_bool(raw.get("cont"), f"basis[{idx}].cont"),
        "ent": require_int(raw.get("ent"), f"basis[{idx}].ent"),
        "ex": require_int(raw.get("ex"), f"basis[{idx}].ex"),
        "pnl": require_finite_float(raw.get("pnl"), f"basis[{idx}].pnl"),
        "fee": require_finite_float(raw.get("fee"), f"basis[{idx}].fee"),
        "net": require_finite_float(raw.get("net"), f"basis[{idx}].net"),
        "ntl": ntl,
    }


def compute_net_vs_notional_regression(rows: Sequence[dict[str, Any]]) -> dict[str, Any]:
    n = len(rows)
    if n < 2:
        return {"n": n, "slope_bps": None, "intercept_usd": None, "r_squared": None}
    xs = [r["ntl"] for r in rows]
    ys = [r["net"] for r in rows]
    mx = sum(xs) / n
    my = sum(ys) / n
    sxx = sum((x - mx) ** 2 for x in xs)
    sxy = sum((x - mx) * (y - my) for x, y in zip(xs, ys))
    syy = sum((y - my) ** 2 for y in ys)
    if sxx <= 0.0:
        return {"n": n, "slope_bps": None, "intercept_usd": None, "r_squared": None}
    slope = sxy / sxx
    intercept = my - slope * mx
    r_squared = (sxy * sxy) / (sxx * syy) if syy > 0.0 else 0.0
    return {
        "n": n,
        "slope_bps": slope * 10_000.0,
        "intercept_usd": intercept,
        "r_squared": r_squared,
    }


def _aggregate_pipeline_group(rows: Sequence[dict[str, Any]]) -> dict[str, Any]:
    net_usd = sum(r["net"] for r in rows)
    ntl_usd = sum(r["ntl"] for r in rows)
    cyc = sum(r["cyc"] for r in rows)
    can = sum(r["can"] for r in rows)
    haz = sum(r["haz"] for r in rows)
    qpres = sum(r["qpres"] for r in rows)
    pt_fills = sum(r["price_through_fills"] for r in rows)
    sc_fills = sum(r["sweep_cleared_fills"] for r in rows)
    st_fills = sum(r["sweep_touch_fills"] for r in rows)
    tr_fills = sum(r["stride_touch_fills"] for r in rows)
    buy_sweeps = sum(r["inferred_buy_sweeps"] for r in rows)
    sell_sweeps = sum(r["inferred_sell_sweeps"] for r in rows)

    all_mk = [r["markout_1s_usd"] for r in rows if r["markout_1s_usd"] is not None]
    mk_bps = (
        (sum(all_mk) / ntl_usd) * 10_000.0
        if len(all_mk) == len(rows) and ntl_usd > 0.0
        else None
    )
    net_bps = (net_usd / ntl_usd) * 10_000.0 if ntl_usd > 0.0 else None

    return {
        "rows": len(rows),
        "cont_true_rows": sum(1 for r in rows if r["cont"]),
        "net_usd": net_usd,
        "traded_notional_usd": ntl_usd,
        "net_per_notional_bps": net_bps,
        "markout_1s_bps": mk_bps,
        "closed_cycles": cyc,
        "cancels": can,
        "hazard_ioc_fills": haz,
        "queue_preserved_ticks": qpres,
        "price_through_fills": pt_fills,
        "sweep_cleared_fills": sc_fills,
        "sweep_touch_fills": st_fills,
        "stride_touch_fills": tr_fills,
        "inferred_buy_sweeps": buy_sweeps,
        "inferred_sell_sweeps": sell_sweeps,
        "price_through_only_warning": (sc_fills + st_fills + tr_fills) == 0,
    }


def score_pipeline_rows(
    raw_rows: Sequence[dict[str, Any]],
    baseline_arm: str | None = None,
    max_notional_diff_frac: float = DEFAULT_MAX_NOTIONAL_DIFF_FRAC,
) -> dict[str, Any]:
    rows = [_extract_pipeline_row(r, i) for i, r in enumerate(raw_rows)]
    if not rows:
        return {"present": False, "rows": 0}

    arms_seen: list[str] = []
    for r in rows:
        if r["arm"] not in arms_seen:
            arms_seen.append(r["arm"])

    chosen_baseline = baseline_arm
    if chosen_baseline is None:
        for cand in ("A0_base", "S1_base"):
            if cand in arms_seen:
                chosen_baseline = cand
                break
        if chosen_baseline is None:
            chosen_baseline = arms_seen[0]

    by_arm_rows: dict[str, list[dict[str, Any]]] = defaultdict(list)
    by_sym_arm_rows: dict[tuple[str, str], list[dict[str, Any]]] = defaultdict(list)
    for r in rows:
        by_arm_rows[r["arm"]].append(r)
        by_sym_arm_rows[(r["sym"], r["arm"])].append(r)

    all_arms = {arm: _aggregate_pipeline_group(rs) for arm, rs in by_arm_rows.items()}
    by_symbol: dict[str, dict[str, Any]] = defaultdict(dict)
    for (sym, arm), rs in sorted(by_sym_arm_rows.items()):
        by_symbol[sym][arm] = _aggregate_pipeline_group(rs)

    def _compare(
        scope: str, group_map: dict[str, dict[str, Any]]
    ) -> list[dict[str, Any]]:
        if chosen_baseline not in group_map:
            return []
        base = group_map[chosen_baseline]
        base_ntl = base["traded_notional_usd"]
        base_bps = base["net_per_notional_bps"]
        comps: list[dict[str, Any]] = []
        for arm, agg in group_map.items():
            if arm == chosen_baseline:
                continue
            arm_ntl = agg["traded_notional_usd"]
            arm_bps = agg["net_per_notional_bps"]
            if base_ntl > 0.0:
                ratio = arm_ntl / base_ntl
                diff_frac = abs(arm_ntl - base_ntl) / base_ntl
                comparable = diff_frac <= max_notional_diff_frac
            else:
                ratio = None
                diff_frac = None
                comparable = False
            unit_delta_bps = (
                arm_bps - base_bps
                if arm_bps is not None and base_bps is not None
                else None
            )
            unit_rel_impr = (
                (arm_bps - base_bps) / abs(base_bps)
                if unit_delta_bps is not None and base_bps is not None and abs(base_bps) > 1e-12
                else None
            )
            comps.append(
                {
                    "scope": scope,
                    "baseline_arm": chosen_baseline,
                    "arm": arm,
                    "arm_notional_usd": arm_ntl,
                    "baseline_notional_usd": base_ntl,
                    "notional_ratio": ratio,
                    "notional_diff_frac": diff_frac,
                    "comparable": comparable,
                    "verdict": (
                        "comparable"
                        if comparable
                        else "not_comparable_notional_divergence"
                    ),
                    "arm_net_per_notional_bps": arm_bps,
                    "baseline_net_per_notional_bps": base_bps,
                    "unit_delta_bps": unit_delta_bps,
                    "unit_relative_improvement_frac": unit_rel_impr,
                }
            )
        return comps

    comparisons = _compare("ALL", all_arms)
    for sym in sorted(by_symbol):
        comparisons.extend(_compare(sym, by_symbol[sym]))

    return {
        "present": True,
        "rows": len(rows),
        "baseline_arm": chosen_baseline,
        "max_notional_diff_frac": max_notional_diff_frac,
        "regression_net_vs_notional": compute_net_vs_notional_regression(rows),
        "all_symbols": all_arms,
        "by_symbol": dict(by_symbol),
        "comparisons": comparisons,
    }


def _aggregate_basis_group(
    rows: Sequence[dict[str, Any]], min_basis_roundtrips: int
) -> dict[str, Any]:
    entries = sum(r["ent"] for r in rows)
    roundtrips = sum(r["ex"] for r in rows)
    pnl_usd = sum(r["pnl"] for r in rows)
    fees_usd = sum(r["fee"] for r in rows)
    net_usd = sum(r["net"] for r in rows)
    wins = sum(1 for r in rows if r["net"] > 0.0)
    net_per_rt = net_usd / roundtrips if roundtrips > 0 else None
    net_to_fee = net_usd / fees_usd if fees_usd > 0.0 else None
    all_ntl = [r["ntl"] for r in rows if r["ntl"] is not None]
    traded_ntl = sum(all_ntl) if len(all_ntl) == len(rows) else None
    net_per_ntl_bps = (
        (net_usd / traded_ntl) * 10_000.0
        if traded_ntl is not None and traded_ntl > 0.0
        else None
    )
    return {
        "rows": len(rows),
        "windows": [f"{r['win']}:{r['sym']}" for r in rows],
        "wins": wins,
        "entries": entries,
        "roundtrips": roundtrips,
        "pnl_usd": pnl_usd,
        "fees_usd": fees_usd,
        "net_usd": net_usd,
        "net_per_roundtrip_usd": net_per_rt,
        "net_to_fee_ratio": net_to_fee,
        "traded_notional_usd": traded_ntl,
        "net_per_notional_bps": net_per_ntl_bps,
        "thin_sample_warning": roundtrips < min_basis_roundtrips,
    }


def _check_basis_ofat_monotonicity(
    arm_aggs: dict[str, dict[str, Any]]
) -> list[dict[str, Any]]:
    series_map: dict[str, list[tuple[float, str]]] = defaultdict(list)
    pat = re.compile(r"^(.+?)_sc(\d+)$")
    for arm in arm_aggs:
        m = pat.match(arm)
        if m:
            prefix, sc_str = m.group(1), m.group(2)
            sc_val = float(sc_str) / 100.0
            series_map[prefix].append((sc_val, arm))

    checks: list[dict[str, Any]] = []
    for prefix, items in sorted(series_map.items()):
        if len(items) < 2:
            continue
        items.sort()
        rts = [arm_aggs[a]["roundtrips"] for _, a in items]
        net_rts = [arm_aggs[a]["net_per_roundtrip_usd"] for _, a in items]
        net_fees = [arm_aggs[a]["net_to_fee_ratio"] for _, a in items]
        rt_non_increasing = all(rts[i] >= rts[i + 1] for i in range(len(rts) - 1))
        valid_nr = [x for x in net_rts if x is not None]
        net_rt_non_decreasing = len(valid_nr) == len(items) and all(
            valid_nr[i] <= valid_nr[i + 1] + 1e-12 for i in range(len(valid_nr) - 1)
        )
        valid_nf = [x for x in net_fees if x is not None]
        net_fee_non_decreasing = len(valid_nf) == len(items) and all(
            valid_nf[i] <= valid_nf[i + 1] + 1e-12 for i in range(len(valid_nf) - 1)
        )
        checks.append(
            {
                "series_prefix": prefix,
                "arms_ordered": [a for _, a in items],
                "spread_coeffs": [sc for sc, _ in items],
                "roundtrips": rts,
                "net_per_roundtrip_usd": net_rts,
                "net_to_fee_ratio": net_fees,
                "roundtrips_monotone_non_increasing": rt_non_increasing,
                "net_per_roundtrip_monotone_non_decreasing": net_rt_non_decreasing,
                "net_to_fee_monotone_non_decreasing": net_fee_non_decreasing,
            }
        )
    return checks


def score_basis_rows(
    raw_rows: Sequence[dict[str, Any]], min_basis_roundtrips: int = DEFAULT_MIN_BASIS_ROUNDTRIPS
) -> dict[str, Any]:
    rows = [_extract_basis_row(r, i) for i, r in enumerate(raw_rows)]
    if not rows:
        return {"present": False, "rows": 0}

    def _summarize(subset: Sequence[dict[str, Any]]) -> dict[str, Any]:
        by_arm: dict[str, list[dict[str, Any]]] = defaultdict(list)
        by_sym_arm: dict[tuple[str, str], list[dict[str, Any]]] = defaultdict(list)
        for r in subset:
            by_arm[r["arm"]].append(r)
            by_sym_arm[(r["sym"], r["arm"])].append(r)
        arms_agg = {
            arm: _aggregate_basis_group(rs, min_basis_roundtrips)
            for arm, rs in by_arm.items()
        }
        sym_agg: dict[str, dict[str, Any]] = defaultdict(dict)
        for (sym, arm), rs in sorted(by_sym_arm.items()):
            sym_agg[sym][arm] = _aggregate_basis_group(rs, min_basis_roundtrips)
        return {
            "rows": len(subset),
            "arms": arms_agg,
            "by_symbol": dict(sym_agg),
            "ofat_monotonicity": _check_basis_ofat_monotonicity(arms_agg),
        }

    cont_rows = [r for r in rows if r["cont"]]
    return {
        "present": True,
        "rows": len(rows),
        "min_basis_roundtrips": min_basis_roundtrips,
        "all_windows": _summarize(rows),
        "strict_cont_only": _summarize(cont_rows),
    }


def score_payload(
    payload: dict[str, Any],
    baseline_arm: str | None = None,
    max_notional_diff_frac: float = DEFAULT_MAX_NOTIONAL_DIFF_FRAC,
    min_basis_roundtrips: int = DEFAULT_MIN_BASIS_ROUNDTRIPS,
) -> dict[str, Any]:
    if not isinstance(payload, dict):
        raise ValueError("input JSON root must be an object")
    pipe_raw = payload.get("pipeline", [])
    basis_raw = payload.get("basis", [])
    if not isinstance(pipe_raw, list) or not isinstance(basis_raw, list):
        raise ValueError("pipeline and basis fields must be JSON arrays")
    return {
        "pipeline": score_pipeline_rows(
            pipe_raw,
            baseline_arm=baseline_arm,
            max_notional_diff_frac=max_notional_diff_frac,
        ),
        "basis": score_basis_rows(
            basis_raw,
            min_basis_roundtrips=min_basis_roundtrips,
        ),
    }


def _run_self_test() -> dict[str, Any]:
    sample = {
        "pipeline": [
            {
                "win": "w1",
                "sym": "BTC_core_hl",
                "arm": "A0_base",
                "cont": True,
                "net": -20.0,
                "ntl": 100_000.0,
                "cyc": 100,
                "can": 1000,
                "haz": 10,
                "qpres": 500,
                "price_through_fills": 200,
                "sweep_cleared_fills": 0,
                "sweep_touch_fills": 0,
            },
            {
                "win": "w1",
                "sym": "BTC_core_hl",
                "arm": "C5_lowvol",
                "cont": True,
                "net": -3.0,
                "ntl": 20_000.0,
                "cyc": 20,
                "can": 200,
                "haz": 2,
                "qpres": 800,
                "price_through_fills": 40,
                "sweep_cleared_fills": 0,
                "sweep_touch_fills": 0,
            },
            {
                "win": "w1",
                "sym": "BTC_core_hl",
                "arm": "S1_fair",
                "cont": True,
                "net": -12.0,
                "ntl": 90_000.0,
                "cyc": 90,
                "can": 900,
                "haz": 5,
                "qpres": 700,
                "price_through_fills": 20,
                "sweep_cleared_fills": 30,
                "sweep_touch_fills": 40,
                "markout_1s_bps": 0.45,
            },
        ],
        "basis": [
            {
                "win": "w1",
                "sym": "BTC_core_hl",
                "arm": "OFAT300_sc000",
                "cont": True,
                "ent": 10,
                "ex": 10,
                "pnl": 3.0,
                "fee": 2.0,
                "net": 1.0,
            },
            {
                "win": "w1",
                "sym": "BTC_core_hl",
                "arm": "OFAT300_sc025",
                "cont": True,
                "ent": 6,
                "ex": 6,
                "pnl": 2.4,
                "fee": 1.2,
                "net": 1.2,
            },
        ],
    }
    scored = score_payload(sample, baseline_arm="A0_base")
    comps = {c["arm"]: c for c in scored["pipeline"]["comparisons"] if c["scope"] == "ALL"}
    assert comps["C5_lowvol"]["comparable"] is False
    assert comps["C5_lowvol"]["verdict"] == "not_comparable_notional_divergence"
    assert comps["S1_fair"]["comparable"] is True
    assert scored["pipeline"]["all_symbols"]["A0_base"]["price_through_only_warning"] is True
    assert scored["pipeline"]["all_symbols"]["S1_fair"]["price_through_only_warning"] is False
    mono = scored["basis"]["strict_cont_only"]["ofat_monotonicity"][0]
    assert mono["roundtrips_monotone_non_increasing"] is True
    assert mono["net_per_roundtrip_monotone_non_decreasing"] is True
    return {"self_test": "ok", "scored": scored}


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Score AlephTX strategy arms using P0 normalized metrics & comparability gates."
    )
    parser.add_argument("inputs", nargs="*", help="Evaluation JSON report path(s).")
    parser.add_argument("--baseline-arm", default=None, help="Explicit baseline arm name.")
    parser.add_argument(
        "--max-notional-diff-frac",
        type=float,
        default=DEFAULT_MAX_NOTIONAL_DIFF_FRAC,
        help="Max relative notional divergence allowed for cross-arm ranking (default 0.30).",
    )
    parser.add_argument(
        "--min-basis-roundtrips",
        type=int,
        default=DEFAULT_MIN_BASIS_ROUNDTRIPS,
        help="Minimum basis roundtrips before thin_sample_warning clears (default 30).",
    )
    parser.add_argument(
        "--output", default=None, help="Optional JSON file path to write scored output."
    )
    parser.add_argument(
        "--self-test", action="store_true", help="Run built-in scorer self-test."
    )
    return parser.parse_args(argv)


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv)
    if args.self_test and args.inputs:
        raise ValueError("--self-test cannot be combined with input files")
    if not args.self_test and not args.inputs:
        raise ValueError("must provide at least one input JSON file or --self-test")
    max_diff = require_finite_float(
        args.max_notional_diff_frac, "max_notional_diff_frac"
    )
    if not (0.0 < max_diff <= 10.0):
        raise ValueError("max_notional_diff_frac must be in (0, 10]")
    min_rt = require_int(args.min_basis_roundtrips, "min_basis_roundtrips")
    if min_rt < 1:
        raise ValueError("min_basis_roundtrips must be >= 1")

    if args.self_test:
        out = _run_self_test()
    else:
        reports = {}
        for raw_path in args.inputs:
            path = Path(raw_path)
            if not path.is_file():
                raise FileNotFoundError(f"input file does not exist: {path}")
            payload = json.loads(path.read_text(encoding="utf-8"))
            reports[path.name] = score_payload(
                payload,
                baseline_arm=args.baseline_arm,
                max_notional_diff_frac=max_diff,
                min_basis_roundtrips=min_rt,
            )
        out = {"reports": reports}

    rendered = json.dumps(out, indent=2, allow_nan=False)
    if args.output:
        Path(args.output).write_text(rendered + "\n", encoding="utf-8")
    print(rendered)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as exc:  # pragma: no cover
        print(f"error: {exc}", file=sys.stderr)
        raise SystemExit(2)
