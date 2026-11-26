#!/usr/bin/env python3
"""Track 02 research: executable RWA basis reversion between WS venues (Lighter, Lighter RH,
Arcus) at 1 s resolution with explicit execution lag.

`research_rwa_basis_exec.py` showed that minute-level "edges" involving 15 s-polled HIP-3
books vanish once fills are taken from quotes received AFTER the decision. This replay
only uses WS venues and fills both legs on the touch observed `exec_lag_s` seconds after
the decision (default 2 s; 5 s stress), per 10 h window, warm-up excluded from P&L.

Signal: dev = (ln midA - ln midB) - EMA_hl(prior) [bps]. Enter when |dev| >= k * (taker
round trip + current two-leg spread), sell rich / buy cheap at the lagged touch; exit
when dev crosses 0, after `max_hold_s`, on stale books, or at window end.

Protocol: select (hl, k) on W09-W18 (selection), replay frozen on W20-W24 (holdout);
W19 is reported separately as the selection window used elsewhere; W25+ excluded.
Fees: Lighter / RH 0 bp (audited Standard), Arcus taker 1.15 bp (level_4) with a
2.25 bp (level_0) sensitivity. USDC/USDG pairs are flagged. Aggregates only.
"""

from __future__ import annotations

import argparse
import glob
import json
import math
import re
import sys
from itertools import combinations
from pathlib import Path
from typing import Any

SCRIPT_DIR = Path(__file__).resolve().parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

import lakehouse_grid as lg  # noqa: E402
from eval_multivenue_three_tracks import VENUE_SYMBOL_MAP  # noqa: E402

STEP_MS = 1_000
WARMUP_S = 900
MAX_AGE_S = 5
MAX_HOLD_S = 7_200
HALF_LIVES_S = (300.0, 1_800.0, 7_200.0)
KS = (1.0, 2.0, 3.0)
VENUES = ("lighter", "lighter_robinhood", "arcus")
TAKER = {"lighter": 0.0, "lighter_robinhood": 0.0, "arcus": 1.15}
SETTLE = {"lighter": "USDC", "lighter_robinhood": "USDG", "arcus": "USDC"}
SELECTION = range(9, 19)
HOLDOUT = range(20, 25)


def discover_rwa(lakehouse: Path) -> dict[int, str]:
    out = {}
    for d in glob.glob(str(Path(lakehouse) / "parquet/bbo_quotes/collector_region=tokyo/venue=lighter/utc_date=*/campaign_id=tokyo-rwa-*")):
        cid = d.rsplit("campaign_id=", 1)[1]
        m = re.search(r"-w(\d+)$", cid)
        if m:
            out[int(m.group(1))] = cid
    return out


def assets() -> list[str]:
    common = None
    for v in VENUES:
        ks = {a for a in VENUE_SYMBOL_MAP[v] if a not in ("BTC", "ETH", "SOL")}
        common = ks if common is None else common | ks
    return sorted(a for a in common if sum(a in VENUE_SYMBOL_MAP[v] for v in VENUES) >= 2)


def simulate(ga, gb, *, hl_s, k, fee_a, fee_b, lag_s, warm=WARMUP_S):
    import numpy as np

    ma, mb = ga.mid(), gb.mid()
    with np.errstate(invalid="ignore", divide="ignore"):
        x = np.log(ma) - np.log(mb)
        sp = (ga.ask - ga.bid) / ma + (gb.ask - gb.bid) / mb
    ema = lg.ema_causal(x, 1.0 - 0.5 ** (1.0 / hl_s))
    dev = (x - ema) * 1e4
    fee_rt = 2 * (fee_a + fee_b)
    thr = k * (fee_rt + sp * 1e4)
    fresh = (ga.age_steps <= MAX_AGE_S) & (gb.age_steps <= MAX_AGE_S)
    n = len(x)
    cand = np.nonzero(fresh & np.isfinite(dev) & (np.abs(dev) >= thr) & (np.abs(dev) < 300))[0]
    cand = cand[(cand >= warm) & (cand < n - lag_s - 2)]
    pnl = []
    nxt = -1
    for i in cand:
        if i <= nxt:
            continue
        side = 1.0 if dev[i] > 0 else -1.0  # +1: sell A, buy B
        e = i + lag_s
        if not (fresh[e]):
            continue
        end = min(n - lag_s - 1, i + MAX_HOLD_S)
        seg = dev[i + 1 : end]
        hit = np.nonzero((side * seg <= 0) | ~fresh[i + 1 : end])[0]
        j = i + 1 + int(hit[0]) if len(hit) else end
        xj = j + lag_s
        if side > 0:
            r = (ga.bid[e] - ga.ask[xj]) / ga.bid[e] + (gb.bid[xj] - gb.ask[e]) / gb.ask[e]
        else:
            r = (ga.bid[xj] - ga.ask[e]) / ga.ask[e] + (gb.bid[e] - gb.ask[xj]) / gb.bid[e]
        v = r * 1e4 - fee_rt
        if math.isfinite(v):
            pnl.append(v)
        nxt = xj
    return pnl


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--lakehouse", type=Path, default=Path.home() / "aleph-tx-lakehouse")
    ap.add_argument("--output", type=Path, required=True)
    args = ap.parse_args()
    con = lg.connect()
    wins = {w: c for w, c in discover_rwa(args.lakehouse).items() if w in SELECTION or w in HOLDOUT or w == 19}
    pairs = [(a, x, y) for a in assets() for x, y in combinations(VENUES, 2)
             if a in VENUE_SYMBOL_MAP[x] and a in VENUE_SYMBOL_MAP[y]]
    variants = [(hl, k) for hl in HALF_LIVES_S for k in KS]
    # results[(variant, lag, arcus_fee)][pair] -> {window: pnl list}
    res: dict[Any, dict] = {}
    for w, cid in sorted(wins.items()):
        grids = {}
        for v in VENUES:
            files = lg.campaign_files(args.lakehouse, "bbo_quotes", v, cid)
            for a in {p[0] for p in pairs}:
                mkt = VENUE_SYMBOL_MAP[v].get(a)
                if mkt is None or not files:
                    continue
                span = lg.campaign_span(con, files, mkt)
                if span:
                    grids[(v, a)] = (files, mkt, span)
        for a, va, vb in pairs:
            if (va, a) not in grids or (vb, a) not in grids:
                continue
            fa, maa, sa = grids[(va, a)]
            fb, mbb, sb = grids[(vb, a)]
            t0, t1 = max(sa[0], sb[0]), min(sa[1], sb[1])
            if t1 - t0 < 3_600_000:
                continue
            ca, sta = lg.load_bucket_rows(con, fa, maa, STEP_MS)
            cb, stb = lg.load_bucket_rows(con, fb, mbb, STEP_MS)
            ga, gb = lg.build_grid(ca, sta, t0, t1, STEP_MS), lg.build_grid(cb, stb, t0, t1, STEP_MS)
            if ga is None or gb is None:
                continue
            for (hl, k) in variants:
                for lag in (2, 5):
                    for afee in (1.15, 2.25):
                        if lag == 5 and afee == 2.25:
                            continue
                        fa_ = afee if va == "arcus" else TAKER[va]
                        fb_ = afee if vb == "arcus" else TAKER[vb]
                        p = simulate(ga, gb, hl_s=hl, k=k, fee_a=fa_, fee_b=fb_, lag_s=lag)
                        res.setdefault((hl, k, lag, afee), {}).setdefault(f"{a}:{va}:{vb}", {})[w] = p
        print(json.dumps({"window": w, "pairs_done": len(pairs)}), flush=True)

    def agg(key, wset):
        tot, n, pos_w = 0.0, 0, {}
        per_pair = {}
        for pk, wd in res.get(key, {}).items():
            s = sum(sum(v) for ww, v in wd.items() if ww in wset)
            c = sum(len(v) for ww, v in wd.items() if ww in wset)
            per_pair[pk] = {"trades": c, "net_bps": round(s, 2)}
            tot += s
            n += c
            for ww, v in wd.items():
                if ww in wset:
                    pos_w[ww] = pos_w.get(ww, 0.0) + sum(v)
        return {"net_bps_total": round(tot, 2), "trades": n,
                "net_bps_per_trade": round(tot / n, 3) if n else 0.0,
                "positive_windows": sum(1 for v in pos_w.values() if v > 0), "windows": len(pos_w),
                "per_window_bps": {str(k_): round(v, 1) for k_, v in sorted(pos_w.items())}}, per_pair

    sel_set, hold_set = set(SELECTION), set(HOLDOUT)
    grid = []
    for (hl, k) in variants:
        s, _ = agg((hl, k, 2, 1.15), sel_set)
        grid.append({"half_life_s": hl, "k": k, **{kk: s[kk] for kk in ("net_bps_total", "trades", "positive_windows", "windows")}})
    best = max(grid, key=lambda g: g["net_bps_total"])
    fz = (best["half_life_s"], best["k"])
    out: dict[str, Any] = {
        "schema": "track02_ws_trio_basis_v1",
        "windows": {"selection": sorted(w for w in wins if w in sel_set), "holdout": sorted(w for w in wins if w in hold_set)},
        "variants": len(variants), "pairs": len(pairs),
        "selection_grid": grid, "frozen": {"half_life_s": fz[0], "k": fz[1]},
    }
    for label, key in (("lag2_arcus1.15", (*fz, 2, 1.15)), ("lag2_arcus2.25", (*fz, 2, 2.25)), ("lag5_arcus1.15", (*fz, 5, 1.15))):
        h, pp = agg(key, hold_set)
        s, _ = agg(key, sel_set)
        w19, _ = agg(key, {19})
        out[label] = {"selection": s, "w19": w19, "holdout": h,
                      "holdout_top_pairs": sorted(pp.items(), key=lambda kv: -kv[1]["net_bps"])[:12],
                      "holdout_bottom_pairs": sorted(pp.items(), key=lambda kv: kv[1]["net_bps"])[:6]}
    print(json.dumps({"frozen": out["frozen"], "sel": best,
                      "hold_lag2": out["lag2_arcus1.15"]["holdout"]["net_bps_total"],
                      "hold_lag5": out["lag5_arcus1.15"]["holdout"]["net_bps_total"]}), flush=True)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(out, indent=2, allow_nan=False) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    sys.exit(main())
