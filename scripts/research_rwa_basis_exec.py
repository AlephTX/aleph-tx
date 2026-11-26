#!/usr/bin/env python3
"""Track 02 research: executable cross-venue RWA basis mean reversion on touch prices.

Motivated by `research_funding_carry.py`: hourly MARK basis between HIP-3 venues
(Entropy `io:*`, TradeXYZ `xyz:*`) and Lighter / Lighter RH / Arcus dislocates by tens to
hundreds of bps and reverts, while funding differentials are ~1 bp/day. Marks are not
tradable, so this replay uses BBO touches only:

* 1-minute last-quote buckets from `bbo_quotes` (receipt-causal), forward-filled at most
  2 minutes (HIP-3 books are 15 s REST polls; Lighter/Arcus are WS).
* Decision every 5 minutes. dev = (ln midA - ln midB) - EMA_hl(prior) [bps].
  Enter when |dev| >= k * (round-trip taker fees + current two-leg spread): sell the
  rich leg at its BID, buy the cheap leg at its ASK. Exit when dev crosses 0, after
  48 h, on a data gap, or at period end - crossing both books again.
* Fees: taker on all four fills. HIP-3 growth-mode 0.9 bp is UNAUDITED (`OPT-01`);
  a no-growth-mode stress (9 bp) and a +1 bp/leg slippage stress are reported.
* Capacity: the min L1 notional on the two entry touches is recorded per trade.

Protocol: choose (half-life, k) on SELECTION (before W20 start) by total P&L, freeze,
replay HOLDOUT (W20 start .. W25 start). W25+ excluded. Funding ignored (measured
~1 bp/day). Aggregates only; research hypothesis, not live evidence.
"""

from __future__ import annotations

import argparse
import glob
import json
import math
import statistics
import sys
from itertools import combinations
from pathlib import Path
from typing import Any

SCRIPT_DIR = Path(__file__).resolve().parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

from eval_multivenue_three_tracks import VENUE_SYMBOL_MAP  # noqa: E402

MIN = 60_000
SELECTION_END_MS = 1795451400000  # 2026-11-23T16:30Z (W20 start)
HOLDOUT_END_MS = 1795635840000  # 2026-11-25T19:44Z (W25 start)
DECISION_MIN = 5
FFILL_MIN = 2
MAX_HOLD_MIN = 48 * 60
HALF_LIVES_H = (1.0, 6.0, 24.0)
KS = (1.0, 2.0, 3.0)
VENUES = ("entropy", "trade_xyz", "lighter", "lighter_robinhood", "arcus")
TAKER_BPS = {"entropy": 0.9, "trade_xyz": 0.9, "lighter": 0.0, "lighter_robinhood": 0.0, "arcus": 1.15}
NO_GROWTH_TAKER_BPS = {**TAKER_BPS, "entropy": 9.0, "trade_xyz": 9.0}
AUDITED = {"entropy": False, "trade_xyz": False, "lighter": True, "lighter_robinhood": True, "arcus": True}
SETTLEMENT = {"entropy": "USDC", "trade_xyz": "USDC", "lighter": "USDC", "lighter_robinhood": "USDG", "arcus": "USDC"}


def load_minutes(lakehouse: Path) -> dict[tuple[str, str], dict[int, tuple[float, float, float, float]]]:
    import duckdb

    con = duckdb.connect(":memory:")
    con.execute("SET preserve_insertion_order=false; SET threads=2; SET memory_limit='2GB';")
    base = Path(lakehouse) / "parquet/bbo_quotes/collector_region=tokyo"
    assets = sorted(set(VENUE_SYMBOL_MAP["entropy"]) | set(VENUE_SYMBOL_MAP["trade_xyz"]))
    out = {}
    for v in VENUES:
        files = [f for f in glob.glob(str(base / f"venue={v}/utc_date=*/campaign_id=tokyo-rwa-*/*.parquet"))
                 if not any(f"-w{n}" in f for n in (25, 26, 27, 28))]
        if not files:
            continue
        src = "[" + ",".join(f"'{f}'" for f in files) + "]"
        for a in assets:
            mkt = VENUE_SYMBOL_MAP.get(v, {}).get(a)
            if mkt is None:
                continue
            rows = con.execute(f"""
                SELECT available_at_ms // {MIN} AS m,
                       arg_max(bid_px_f64, available_at_ms), arg_max(ask_px_f64, available_at_ms),
                       arg_max(CAST(bid_sz AS DOUBLE), available_at_ms), arg_max(CAST(ask_sz AS DOUBLE), available_at_ms)
                FROM read_parquet({src})
                WHERE market = ? AND tradable AND bid_px_f64 > 0 AND ask_px_f64 > bid_px_f64
                  AND available_at_ms < {HOLDOUT_END_MS}
                GROUP BY 1""", [mkt]).fetchall()
            if rows:
                # bucket m holds quotes received in [m, m+1) min -> usable at decision time (m+1) min
                out[(v, a)] = {int(m) + 1: (b, k, bs or 0.0, ks or 0.0) for m, b, k, bs, ks in rows}
    return out


def ffill(series: dict[int, tuple], t: int):
    for lag in range(FFILL_MIN + 1):
        q = series.get(t - lag)
        if q is not None:
            return q
    return None


def simulate(a, b, *, hl_h, k, fee_a, fee_b, start_min, end_min, slip_bps=0.0, exec_lag_min=0):
    times = range(start_min - 48 * 60, end_min, DECISION_MIN)  # warm EMA on 48 h before start
    alpha = 1.0 - 0.5 ** (DECISION_MIN / (hl_h * 60.0))
    ema = None
    pos = 0
    ent = None
    t_in = None
    last_ok = None
    pnl = []
    caps = []
    fee_rt = 2 * (fee_a + fee_b) + 4 * slip_bps
    for t in times:
        qa, qb = ffill(a, t), ffill(b, t)
        if qa is None or qb is None:
            continue
        ba, aa, bsa, asa = qa
        bb, ab, bsb, asb = qb
        x = math.log((ba + aa) / 2) - math.log((bb + ab) / 2)
        gap = last_ok is not None and t - last_ok > 30
        dev = None if ema is None or gap else (x - ema) * 1e4
        in_win = start_min <= t < end_min
        if pos:
            if gap or not in_win or (dev is not None and pos * dev <= 0) or t - t_in >= MAX_HOLD_MIN:
                eba, eab = ent  # pos=+1: sold A at bid eba, bought B at ask eab
                if exec_lag_min:
                    xa, xb = a.get(t + exec_lag_min), b.get(t + exec_lag_min)
                    if xa is None or xb is None:
                        xa, xb = ffill(a, t + exec_lag_min), ffill(b, t + exec_lag_min)
                    if xa is None or xb is None:
                        xa, xb = qa, qb
                    ba, aa, _, _ = xa
                    bb, ab, _, _ = xb
                if pos > 0:
                    r = (eba - aa) / eba + (bb - eab) / eab
                else:
                    r = (ba - eba) / eba + (eab - ab) / eab
                pnl.append(r * 1e4 - fee_rt)
                pos = 0
        if not pos and in_win and dev is not None and t + DECISION_MIN < end_min:
            spread_bps = ((aa - ba) / ((aa + ba) / 2) + (ab - bb) / ((ab + bb) / 2)) * 1e4
            if abs(dev) >= k * (fee_rt + spread_bps) and abs(dev) < 1000:
                pos = 1 if dev > 0 else -1
                if exec_lag_min:
                    # Fill on the first quotes RECEIVED after the decision (strictly later
                    # bucket), not on the quotes that triggered it.
                    xa, xb = a.get(t + exec_lag_min), b.get(t + exec_lag_min)
                    if xa is None or xb is None:
                        pos = 0
                        ema = x if ema is None or gap else ema + alpha * (x - ema)
                        last_ok = t
                        continue
                    ba, aa, bsa, asa = xa
                    bb, ab, bsb, asb = xb
                ent = (ba, ab) if pos > 0 else (aa, bb)
                t_in = t
                caps.append(min(ba * bsa, ab * asb) if pos > 0 else min(aa * asa, bb * bsb))
        ema = x if ema is None or gap else ema + alpha * (x - ema)
        last_ok = t
    n = len(pnl)
    return {
        "trades": n,
        "net_bps_total": round(sum(pnl), 2),
        "net_bps_mean": round(sum(pnl) / n, 3) if n else 0.0,
        "hit_rate": round(sum(p > 0 for p in pnl) / n, 3) if n else 0.0,
        "worst_bps": round(min(pnl), 2) if n else 0.0,
        "median_entry_l1_usd": round(statistics.median(caps), 0) if caps else 0.0,
    }


def pairs_of(data):
    by = {}
    for v, a in data:
        by.setdefault(a, []).append(v)
    return [(a, x, y) for a, vs in sorted(by.items()) for x, y in combinations(sorted(vs), 2)]


def run_all(data, start_ms, end_ms, hl, k, fees=TAKER_BPS, slip=0.0, lag=0):
    rows = []
    for a, va, vb in pairs_of(data):
        r = simulate(data[(va, a)], data[(vb, a)], hl_h=hl, k=k, fee_a=fees[va], fee_b=fees[vb],
                     start_min=start_ms // MIN, end_min=end_ms // MIN, slip_bps=slip, exec_lag_min=lag)
        r.update({"asset": a, "a": va, "b": vb, "fees_audited": AUDITED[va] and AUDITED[vb],
                  "cross_stablecoin": SETTLEMENT[va] != SETTLEMENT[vb]})
        rows.append(r)
    return rows


def total(rows):
    return round(sum(r["net_bps_total"] for r in rows), 2)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--lakehouse", type=Path, default=Path.home() / "aleph-tx-lakehouse")
    ap.add_argument("--output", type=Path, required=True)
    args = ap.parse_args()
    data = load_minutes(args.lakehouse)
    start = min(min(s) for s in data.values()) * MIN + 48 * 3_600_000
    out: dict[str, Any] = {
        "schema": "track02_rwa_basis_exec_v1",
        "series_minutes": {f"{v}:{a}": len(s) for (v, a), s in sorted(data.items())},
        "selection": {"start_ms": start, "end_ms": SELECTION_END_MS},
        "holdout": {"start_ms": SELECTION_END_MS, "end_ms": HOLDOUT_END_MS},
        "grid": {"half_life_h": HALF_LIVES_H, "k": KS, "decision_min": DECISION_MIN},
        "variants": len(HALF_LIVES_H) * len(KS),
        "pairs": len(pairs_of(data)),
    }
    grid = []
    for hl in HALF_LIVES_H:
        for k in KS:
            rows = run_all(data, start, SELECTION_END_MS, hl, k)
            grid.append({"half_life_h": hl, "k": k, "sum_net_bps": total(rows),
                         "trades": sum(r["trades"] for r in rows)})
    out["selection_grid"] = grid
    best = max(grid, key=lambda g: g["sum_net_bps"])
    hl, k = best["half_life_h"], best["k"]
    out["frozen"] = {"half_life_h": hl, "k": k}
    sel = run_all(data, start, SELECTION_END_MS, hl, k)
    hold = run_all(data, SELECTION_END_MS, HOLDOUT_END_MS, hl, k)
    out["selection_pairs"] = sorted([r for r in sel if r["trades"]], key=lambda r: -r["net_bps_total"])
    out["holdout_pairs"] = sorted([r for r in hold if r["trades"]], key=lambda r: -r["net_bps_total"])
    out["holdout_total_bps"] = total(hold)
    out["holdout_audited_only_bps"] = total([r for r in hold if r["fees_audited"]])
    out["holdout_stress_slip1"] = total(run_all(data, SELECTION_END_MS, HOLDOUT_END_MS, hl, k, slip=1.0))
    out["holdout_stress_no_growth_fee"] = total(
        run_all(data, SELECTION_END_MS, HOLDOUT_END_MS, hl, k, fees=NO_GROWTH_TAKER_BPS))
    for lag in (1, 2):
        rows = run_all(data, SELECTION_END_MS, HOLDOUT_END_MS, hl, k, lag=lag)
        out[f"holdout_exec_lag_{lag}min_total_bps"] = total(rows)
        out[f"holdout_exec_lag_{lag}min_audited_bps"] = total([r for r in rows if r["fees_audited"]])
        out[f"holdout_exec_lag_{lag}min_pairs"] = sorted([r for r in rows if r["trades"]], key=lambda r: -r["net_bps_total"])
        srows = run_all(data, start, SELECTION_END_MS, hl, k, lag=lag)
        out[f"selection_exec_lag_{lag}min_total_bps"] = total(srows)
    print(json.dumps({"lag1": out["holdout_exec_lag_1min_total_bps"], "lag2": out["holdout_exec_lag_2min_total_bps"],
                      "lag1_aud": out["holdout_exec_lag_1min_audited_bps"]}), flush=True)
    print(json.dumps({"frozen": out["frozen"], "sel": best, "holdout": out["holdout_total_bps"],
                      "audited": out["holdout_audited_only_bps"], "slip1": out["holdout_stress_slip1"],
                      "no_growth": out["holdout_stress_no_growth_fee"]}), flush=True)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(out, indent=2, allow_nan=False) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    sys.exit(main())
