#!/usr/bin/env python3
"""Track 02 (`02_hedge_arbitrage`) research: dynamic cross-venue funding carry (read-only).

Delta-neutral pairs (short the leg that pays more funding, long the other) across every
venue pair with a common underlying in the Tokyo lakehouse `funding_and_ctx` dataset:
Lighter Core, Lighter RH (both Standard 0/0 fee), Hyperliquid core, TradeXYZ and Entropy
(HIP-3). Extended publishes no funding rate in this dataset and is excluded.

Funding alignment (bps per hourly payment, keyed by PAYMENT time T):
* Lighter / Lighter RH: `market_stats` rows carry the hour `funding_slot_ms` S; the slot's
  rate is the mean of the observed rates inside the slot (TWAP proxy, flagged), paid at
  T = S + 1 h. Mark = last mark observed inside the slot.
* Hyperliquid / TradeXYZ / Entropy: `hourly_funding_history` settled rate at T =
  `funding_slot_ms`. Mark = last `active_asset_ctx` mark before T.

Strategy (strictly causal): decide at T using payments <= T; position earns payments
from T + 1 h on. Signal = EMA (half-life H hours) of d = f_A - f_B. Enter direction
sign(signal) when |signal| * horizon_h >= mult * round-trip cost; exit when the signal
no longer favours the position (<= exit_bps) or after a data gap > 3 h. Every entry and
exit pays taker fee + slippage on both legs; basis P&L uses marks; missing funding
payments earn nothing and are counted.

Protocol: parameters are chosen per cohort (crypto / rwa) on the SELECTION period
(payments before W20 start) by total portfolio P&L, frozen, then replayed unchanged on
the HOLDOUT period (W20 start .. W25 start). W25+ is excluded (still being ingested).
Aggregates only. Fees for HIP-3 venues are unaudited (`OPT-01`) and flagged.
"""

from __future__ import annotations

import argparse
import glob
import json
import math
import sys
from itertools import combinations
from pathlib import Path
from typing import Any

HOUR = 3_600_000
SELECTION_END_MS = 1795451400000  # 2026-11-23T16:30:00Z (W20 start)
HOLDOUT_END_MS = 1795635840000  # 2026-11-25T19:44:00Z (W25 start)
GAP_FORCE_EXIT_H = 3

HALF_LIVES_H = (6.0, 24.0, 72.0)
MULTS = (1.0, 2.0, 3.0)
HORIZON_H = 24.0
EXIT_BPS = 0.0

# taker fee per leg per side (bps), slippage per leg per side (bps) by cohort
TAKER_FEE_BPS = {"lighter": 0.0, "lighter_robinhood": 0.0, "hyperliquid": 4.5, "trade_xyz": 0.9, "entropy": 0.9}
FEE_AUDITED = {"lighter": True, "lighter_robinhood": True, "hyperliquid": True, "trade_xyz": False, "entropy": False}
SLIP_BPS = {"crypto": 1.0, "rwa": 2.0}
STRESS_EXTRA_BPS = 2.0
SETTLEMENT = {"lighter": "USDC", "lighter_robinhood": "USDG", "hyperliquid": "USDC", "trade_xyz": "USDC", "entropy": "USDC"}

CRYPTO = {"BTC", "ETH", "SOL"}


def canon(venue: str, market: str) -> str:
    m = market.split(":", 1)[1] if ":" in market else market
    m = m.replace("-USD", "")
    return {"SP500": "SPX", "XYZ100": "NDX"}.get(m, m)


def load_series(lakehouse: Path) -> dict[tuple[str, str], dict[int, tuple[float, float]]]:
    import duckdb

    con = duckdb.connect(":memory:")
    con.execute("SET preserve_insertion_order=false; SET threads=2; SET memory_limit='2GB';")
    base = Path(lakehouse) / "parquet/funding_and_ctx/collector_region=tokyo"
    out: dict[tuple[str, str], dict[int, tuple[float, float]]] = {}
    for venue in TAKER_FEE_BPS:
        files = [f for f in glob.glob(str(base / f"venue={venue}/utc_date=*/campaign_id=*/*.parquet"))
                 if "-w25" not in f and "-w26" not in f and "-w27" not in f]
        if not files:
            continue
        src = "[" + ",".join(f"'{f}'" for f in files) + "]"
        if venue.startswith("lighter"):
            rows = con.execute(f"""
                SELECT market, funding_slot_ms + {HOUR} AS pay,
                       avg(funding_rate_bps_f64) AS rate,
                       arg_max(mark_px_f64, available_at_ms) AS mark
                FROM read_parquet({src})
                WHERE ctx_kind = 'market_stats' AND funding_slot_ms IS NOT NULL
                  AND funding_rate_bps_f64 IS NOT NULL AND mark_px_f64 > 0
                  AND available_at_ms >= funding_slot_ms AND available_at_ms < funding_slot_ms + {HOUR}
                GROUP BY 1, 2""").fetchall()
        else:
            fund = con.execute(f"""
                SELECT market, funding_slot_ms AS pay, arg_max(funding_rate_bps_f64, available_at_ms)
                FROM read_parquet({src})
                WHERE ctx_kind = 'hourly_funding_history' AND funding_slot_ms IS NOT NULL
                  AND funding_rate_bps_f64 IS NOT NULL
                GROUP BY 1, 2""").fetchall()
            marks = con.execute(f"""
                SELECT market, (available_at_ms // {HOUR}) * {HOUR} + {HOUR} AS pay,
                       arg_max(mark_px_f64, available_at_ms)
                FROM read_parquet({src})
                WHERE ctx_kind = 'active_asset_ctx' AND mark_px_f64 > 0
                GROUP BY 1, 2""").fetchall()
            mk = {(m, int(p)): float(x) for m, p, x in marks}
            rows = [(m, p, r, mk.get((m, int(p)))) for m, p, r in fund]
        for m, pay, rate, mark in rows:
            if rate is None or mark is None or not (mark > 0):
                continue
            out.setdefault((venue, canon(venue, m)), {})[int(pay)] = (float(rate), float(mark))
    return out


def simulate_pair(
    a: dict[int, tuple[float, float]],
    b: dict[int, tuple[float, float]],
    *,
    half_life_h: float,
    mult: float,
    rt_cost_bps: float,
    start_ms: int,
    end_ms: int,
    horizon_h: float = HORIZON_H,
    exit_bps: float = EXIT_BPS,
) -> dict[str, Any]:
    """Causal hourly ledger in bps of per-leg notional. Signal warms up on all prior data."""
    times = sorted(set(a) & set(b))
    alpha = 1.0 - 0.5 ** (1.0 / half_life_h)
    ema = None
    pos = 0  # +1 = short A / long B
    entry_marks = None
    last_t = None
    funding = basis = costs = 0.0
    trades = 0
    held_h = 0
    missing_while_held = 0
    for t in times:
        fa, ma = a[t]
        fb, mb = b[t]
        gap_h = (t - last_t) / HOUR if last_t is not None else 1.0
        in_window = start_ms <= t < end_ms
        if pos and in_window:
            if gap_h > 1.0:
                missing_while_held += int(round(gap_h)) - 1
            funding += pos * (fa - fb)  # payment at t accrues to positions held over (t-1h, t]
            held_h += 1
        # Decision at t (after observing payment at t).
        d = fa - fb
        ema = d if ema is None or gap_h > GAP_FORCE_EXIT_H else ema + alpha * (d - ema)
        if pos:
            force = gap_h > GAP_FORCE_EXIT_H
            if force or pos * ema <= exit_bps or not in_window:
                ea, eb = entry_marks
                basis += pos * (-(ma / ea - 1.0) + (mb / eb - 1.0)) * 1e4
                costs += rt_cost_bps / 2.0
                pos = 0
        if not pos and in_window and t + HOUR < end_ms and abs(ema) * horizon_h >= mult * rt_cost_bps:
            pos = 1 if ema > 0 else -1
            entry_marks = (ma, mb)
            costs += rt_cost_bps / 2.0
            trades += 1
        last_t = t
    if pos:  # terminal unwind at last observed marks
        ea, eb = entry_marks
        ma, mb = a[last_t][1], b[last_t][1]
        basis += pos * (-(ma / ea - 1.0) + (mb / eb - 1.0)) * 1e4
        costs += rt_cost_bps / 2.0
    net = funding + basis - costs
    return {
        "entries": trades,
        "held_hours": held_h,
        "funding_bps": round(funding, 3),
        "basis_bps": round(basis, 3),
        "cost_bps": round(costs, 3),
        "net_bps": round(net, 3),
        "missing_payments_while_held": missing_while_held,
    }


def pair_cost(va: str, vb: str, cohort: str, stress: bool = False) -> float:
    per_side = TAKER_FEE_BPS[va] + TAKER_FEE_BPS[vb] + 2 * SLIP_BPS[cohort]
    if stress:
        per_side += 2 * STRESS_EXTRA_BPS
    return 2.0 * per_side


def build_pairs(series):
    by_asset: dict[str, list[str]] = {}
    for (v, c) in series:
        by_asset.setdefault(c, []).append(v)
    pairs = []
    for c, vs in sorted(by_asset.items()):
        for va, vb in combinations(sorted(vs), 2):
            pairs.append((c, va, vb))
    return pairs


def run(series, start, end, hl, mult, stress=False):
    rows = []
    for c, va, vb in build_pairs(series):
        cohort = "crypto" if c in CRYPTO else "rwa"
        r = simulate_pair(series[(va, c)], series[(vb, c)], half_life_h=hl, mult=mult,
                          rt_cost_bps=pair_cost(va, vb, cohort, stress), start_ms=start, end_ms=end)
        r.update({"asset": c, "a": va, "b": vb, "cohort": cohort,
                  "fees_audited": FEE_AUDITED[va] and FEE_AUDITED[vb],
                  "cross_stablecoin": SETTLEMENT[va] != SETTLEMENT[vb],
                  "same_risk_domain": va.startswith("lighter") and vb.startswith("lighter")})
        rows.append(r)
    return rows


def cohort_summary(rows, cohort, period_h):
    sel = [r for r in rows if r["cohort"] == cohort]
    tot = sum(r["net_bps"] for r in sel)
    return {
        "pairs": len(sel),
        "active_pairs": sum(1 for r in sel if r["entries"]),
        "positive_pairs": sum(1 for r in sel if r["net_bps"] > 0),
        "sum_net_bps": round(tot, 2),
        "sum_funding_bps": round(sum(r["funding_bps"] for r in sel), 2),
        "sum_basis_bps": round(sum(r["basis_bps"] for r in sel), 2),
        "sum_cost_bps": round(sum(r["cost_bps"] for r in sel), 2),
        "period_hours": round(period_h, 1),
    }


BASIS_HALF_LIVES_H = (6.0, 24.0, 72.0)
BASIS_ENTRY_K = (1.5, 3.0, 5.0)
BASIS_MAX_HOLD_H = 48


def simulate_basis_pair(
    a: dict[int, tuple[float, float]],
    b: dict[int, tuple[float, float]],
    *,
    half_life_h: float,
    entry_k: float,
    rt_cost_bps: float,
    start_ms: int,
    end_ms: int,
    max_hold_h: int = BASIS_MAX_HOLD_H,
) -> dict[str, Any]:
    """Causal hourly basis mean reversion (marks), funding included, bps of per-leg notional.

    dev = (ln mA - ln mB) - EMA(prior). Enter short-rich/long-cheap when |dev| >= entry_k *
    round-trip cost; exit when dev crosses zero, after max_hold_h, on a > 3 h gap, or at
    window end. pos=+1 means short A / long B.
    """
    times = sorted(set(a) & set(b))
    alpha = 1.0 - 0.5 ** (1.0 / half_life_h)
    ema = None
    pos = 0
    entry = None
    entry_t = None
    last_t = None
    funding = basis = costs = 0.0
    trades = 0
    held = 0
    wins = 0
    trade_pnl = 0.0
    for t in times:
        fa, ma = a[t]
        fb, mb = b[t]
        x = math.log(ma) - math.log(mb)
        gap_h = (t - last_t) / HOUR if last_t is not None else 1.0
        in_window = start_ms <= t < end_ms
        if pos and in_window:
            funding += pos * (fa - fb)
            trade_pnl += pos * (fa - fb)
            held += 1
        dev = None if ema is None or gap_h > GAP_FORCE_EXIT_H else (x - ema) * 1e4
        if pos:
            if (gap_h > GAP_FORCE_EXIT_H or not in_window or (dev is not None and pos * dev <= 0)
                    or (t - entry_t) / HOUR >= max_hold_h):
                ea, eb = entry
                leg = pos * (-(ma / ea - 1.0) + (mb / eb - 1.0)) * 1e4
                basis += leg
                costs += rt_cost_bps / 2.0
                trade_pnl += leg - rt_cost_bps
                wins += trade_pnl > 0
                pos = 0
        if (not pos and in_window and dev is not None and t + HOUR < end_ms
                and abs(dev) >= entry_k * rt_cost_bps and abs(dev) < 500.0):
            pos = 1 if dev > 0 else -1
            entry, entry_t, trade_pnl = (ma, mb), t, 0.0
            costs += rt_cost_bps / 2.0
            trades += 1
        ema = x if ema is None or gap_h > GAP_FORCE_EXIT_H else ema + alpha * (x - ema)
        last_t = t
    if pos:
        ea, eb = entry
        ma, mb = a[last_t][1], b[last_t][1]
        leg = pos * (-(ma / ea - 1.0) + (mb / eb - 1.0)) * 1e4
        basis += leg
        costs += rt_cost_bps / 2.0
        trade_pnl += leg - rt_cost_bps
        wins += trade_pnl > 0
    return {
        "entries": trades,
        "wins": wins,
        "held_hours": held,
        "funding_bps": round(funding, 3),
        "basis_bps": round(basis, 3),
        "cost_bps": round(costs, 3),
        "net_bps": round(funding + basis - costs, 3),
    }


def run_basis(series, start, end, hl, k, stress=False):
    rows = []
    for c, va, vb in build_pairs(series):
        cohort = "crypto" if c in CRYPTO else "rwa"
        r = simulate_basis_pair(series[(va, c)], series[(vb, c)], half_life_h=hl, entry_k=k,
                                rt_cost_bps=pair_cost(va, vb, cohort, stress), start_ms=start, end_ms=end)
        r.update({"asset": c, "a": va, "b": vb, "cohort": cohort,
                  "fees_audited": FEE_AUDITED[va] and FEE_AUDITED[vb],
                  "cross_stablecoin": SETTLEMENT[va] != SETTLEMENT[vb],
                  "same_risk_domain": va.startswith("lighter") and vb.startswith("lighter")})
        rows.append(r)
    return rows


def basis_section(series, sel_start, sel_h, hold_h):
    res: dict[str, Any] = {"grid": {"half_life_h": BASIS_HALF_LIVES_H, "entry_k": BASIS_ENTRY_K,
                                    "max_hold_h": BASIS_MAX_HOLD_H},
                           "variants_per_cohort": len(BASIS_HALF_LIVES_H) * len(BASIS_ENTRY_K)}
    for cohort in ("crypto", "rwa"):
        grid = []
        for hl in BASIS_HALF_LIVES_H:
            for k in BASIS_ENTRY_K:
                s = cohort_summary(run_basis(series, sel_start, SELECTION_END_MS, hl, k), cohort, sel_h)
                s.update({"half_life_h": hl, "entry_k": k})
                grid.append(s)
        best = max(grid, key=lambda g: g["sum_net_bps"])
        hl, k = best["half_life_h"], best["entry_k"]
        hold = run_basis(series, SELECTION_END_MS, HOLDOUT_END_MS, hl, k)
        stress = run_basis(series, SELECTION_END_MS, HOLDOUT_END_MS, hl, k, stress=True)
        sel_rows = run_basis(series, sel_start, SELECTION_END_MS, hl, k)
        res[cohort] = {
            "selection_grid": grid,
            "frozen": {"half_life_h": hl, "entry_k": k},
            "status": "evaluated" if best["sum_net_bps"] > 0 else "no_go_selection",
            "holdout": cohort_summary(hold, cohort, hold_h),
            "holdout_stress": cohort_summary(stress, cohort, hold_h),
            "holdout_audited_fee_pairs_sum_net_bps": round(
                sum(r["net_bps"] for r in hold if r["cohort"] == cohort and r["fees_audited"]), 2),
            "holdout_pairs": sorted([r for r in hold if r["cohort"] == cohort and r["entries"]],
                                    key=lambda r: -r["net_bps"]),
            "selection_pairs": sorted([r for r in sel_rows if r["cohort"] == cohort and r["entries"]],
                                      key=lambda r: -r["net_bps"]),
        }
        print(json.dumps({"basis_cohort": cohort, "frozen": res[cohort]["frozen"], "sel": best["sum_net_bps"],
                          "holdout": res[cohort]["holdout"]["sum_net_bps"],
                          "stress": res[cohort]["holdout_stress"]["sum_net_bps"]}), flush=True)
    return res


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--lakehouse", type=Path, default=Path.home() / "aleph-tx-lakehouse")
    ap.add_argument("--output", type=Path, required=True)
    args = ap.parse_args()
    series = load_series(args.lakehouse)
    all_t = sorted({t for s in series.values() for t in s})
    sel_start = all_t[0]
    out: dict[str, Any] = {
        "schema": "track02_funding_carry_v1",
        "series": {f"{v}:{c}": {"payments": len(s), "first": min(s), "last": max(s)} for (v, c), s in series.items()},
        "selection": {"start_ms": sel_start, "end_ms": SELECTION_END_MS},
        "holdout": {"start_ms": SELECTION_END_MS, "end_ms": HOLDOUT_END_MS},
        "grid": {"half_life_h": HALF_LIVES_H, "mult": MULTS, "horizon_h": HORIZON_H},
        "variants_per_cohort": len(HALF_LIVES_H) * len(MULTS),
        "cost_model": {"taker_fee_bps": TAKER_FEE_BPS, "slippage_bps_per_leg_side": SLIP_BPS,
                       "stress_extra_bps_per_leg_side": STRESS_EXTRA_BPS},
        "cohorts": {},
    }
    sel_h = (SELECTION_END_MS - sel_start) / HOUR
    hold_h = (HOLDOUT_END_MS - SELECTION_END_MS) / HOUR
    for cohort in ("crypto", "rwa"):
        grid = []
        for hl in HALF_LIVES_H:
            for mult in MULTS:
                rows = run(series, sel_start, SELECTION_END_MS, hl, mult)
                s = cohort_summary(rows, cohort, sel_h)
                s.update({"half_life_h": hl, "mult": mult})
                grid.append(s)
        best = max(grid, key=lambda g: g["sum_net_bps"])
        rec: dict[str, Any] = {"selection_grid": grid, "frozen": {"half_life_h": best["half_life_h"], "mult": best["mult"]}}
        if best["sum_net_bps"] <= 0:
            rec["status"] = "no_go_selection"
        hl, mult = best["half_life_h"], best["mult"]
        hold_rows = run(series, SELECTION_END_MS, HOLDOUT_END_MS, hl, mult)
        stress_rows = run(series, SELECTION_END_MS, HOLDOUT_END_MS, hl, mult, stress=True)
        rec["holdout"] = cohort_summary(hold_rows, cohort, hold_h)
        rec["holdout_stress"] = cohort_summary(stress_rows, cohort, hold_h)
        audited = [r for r in hold_rows if r["cohort"] == cohort and r["fees_audited"]]
        rec["holdout_audited_fee_pairs_sum_net_bps"] = round(sum(r["net_bps"] for r in audited), 2)
        rec["holdout_pairs"] = sorted([r for r in hold_rows if r["cohort"] == cohort and r["entries"]],
                                      key=lambda r: -r["net_bps"])
        sel_rows = run(series, sel_start, SELECTION_END_MS, hl, mult)
        rec["selection_pairs_top"] = sorted([r for r in sel_rows if r["cohort"] == cohort and r["entries"]],
                                            key=lambda r: -r["net_bps"])[:15]
        out["cohorts"][cohort] = rec
        print(json.dumps({"cohort": cohort, "frozen": rec["frozen"], "sel": best["sum_net_bps"],
                          "holdout": rec["holdout"], "stress": rec["holdout_stress"]["sum_net_bps"]}), flush=True)
    out["basis_mean_reversion"] = basis_section(series, sel_start, sel_h, hold_h)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(out, indent=2, allow_nan=False) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    sys.exit(main())
