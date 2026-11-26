#!/usr/bin/env python3
"""Track 02 (`02_hedge_arbitrage`) research: executable maker-first basis replay (read-only).

Replaces "share of time the basis exceeds a hurdle" with an executable ledger:

* A causal fair basis ``bbar = EMA_tau(log maker_mid - log hedge_mid)`` (strictly prior data).
* The maker venue rests a bid/ask derived from the HEDGE venue touch so that, if filled,
  the entered basis beats fair by ``e(q) = e0 +/- k*q`` bps (inventory skew; reducing side
  needs less edge). Quotes join the maker best price (optionally improve one tick) and
  never cross. Quotes go live only after the maker-venue order delay.
* Fills require a public print STRICTLY through our price (no at-touch queue credit), with
  filled notional capped by the through-printed notional and the clip.
* Every maker fill is hedged with a taker order on the hedge venue at the touch observed
  AFTER the hedge delay (speed bump + RTT); fees are charged on both legs.
* After the 10 h decision interval quoting stops and the residual pair is unwound with
  taker orders on both legs (terminal drain recorded separately).

Protocol: select on W19 (`selection_dev`), freeze, replay W24 (`holdout`) and W23
(`holdout_2`). Funding is NOT included (flagged); USDC/USDG pairs are flagged for the
cross-stablecoin policy. Aggregates only; research hypothesis, not live evidence.
"""

from __future__ import annotations

import argparse
import json
import math
import sys
from pathlib import Path
from typing import Any

SCRIPT_DIR = Path(__file__).resolve().parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

import lakehouse_grid as lg  # noqa: E402

STEP_MS = 100
WARMUP_MS = 15 * 60_000
DECISION_MS = lg.TEN_HOURS_MS
MAX_AGE_MS = 5_000
CLIP_USD = 1_000.0
Q_MAX = 4
SKEW_BPS_PER_CLIP = 0.5
MIN_FILLS = 20

E0_BPS = (1.0, 2.0, 3.0, 5.0, 8.0)
TAUS_S = (600.0, 3600.0)
IMPROVE = (False, True)
LEAD_BETAS = (0.0, 0.5, 1.0)

# (maker_venue, maker_market, hedge_venue, hedge_market, domain, label)
PAIRS: list[tuple[str, str, str, str, str, str]] = [
    ("arcus", "QQQ-USD", "lighter", "129", "rwa", "arcus>lighter QQQ"),
    ("arcus", "SPY-USD", "lighter", "128", "rwa", "arcus>lighter SPY"),
    ("arcus", "NVDA-USD", "lighter", "110", "rwa", "arcus>lighter NVDA"),
    ("arcus", "QQQ-USD", "lighter_robinhood", "25", "rwa", "arcus>lighter_rh QQQ"),
    ("arcus", "SPY-USD", "lighter_robinhood", "26", "rwa", "arcus>lighter_rh SPY"),
    ("arcus", "NVDA-USD", "lighter_robinhood", "15", "rwa", "arcus>lighter_rh NVDA"),
    ("arcus", "BTC-USD", "lighter", "1", "crypto", "arcus>lighter BTC"),
    ("arcus", "ETH-USD", "lighter", "0", "crypto", "arcus>lighter ETH"),
    ("lighter", "129", "arcus", "QQQ-USD", "rwa", "lighter>arcus QQQ"),
    ("lighter", "128", "arcus", "SPY-USD", "rwa", "lighter>arcus SPY"),
    ("lighter", "1", "arcus", "BTC-USD", "crypto", "lighter>arcus BTC"),
]

ORDER_DELAY_MS = {"arcus": 40, "lighter": 330, "lighter_robinhood": 230}
MAKER_FEE_BPS = {"arcus": 0.0, "lighter": 0.0, "lighter_robinhood": 0.0}
TAKER_FEE_BPS = {
    ("arcus", "rwa"): 1.15,  # level_4; level_0 = 2.25 reported as sensitivity
    ("arcus", "crypto"): 2.30,
    ("lighter", "rwa"): 0.0,
    ("lighter", "crypto"): 0.0,
    ("lighter_robinhood", "rwa"): 0.0,
    ("lighter_robinhood", "crypto"): 0.0,
}
SETTLEMENT = {"arcus": "USDC", "lighter": "USDC", "lighter_robinhood": "USDG"}


def _tick_from(prices) -> float:
    import numpy as np

    p = np.asarray(prices[np.isfinite(prices)][:20_000])
    if len(p) < 2:
        return 0.01
    d = np.diff(np.unique(np.round(p, 8)))
    d = d[d > 1e-9]
    return float(d.min()) if len(d) else 0.01


def simulate_maker_hedge(
    mk_bid, mk_ask, mk_age, hd_bid, hd_ask, hd_age,
    trades: dict[str, Any] | None,
    *,
    t0_ms: int,
    e0_bps: float,
    alpha: float,
    improve: bool,
    tick: float,
    lead_beta: float = 0.0,
    maker_delay_ms: int,
    hedge_delay_ms: int,
    maker_fee_bps: float,
    maker_taker_fee_bps: float,
    hedge_fee_bps: float,
    warmup_steps: int,
    decision_steps: int,
    max_age_steps: int,
    clip_usd: float = CLIP_USD,
    q_max: int = Q_MAX,
    skew_bps: float = SKEW_BPS_PER_CLIP,
    hedge_prints: dict[str, Any] | None = None,
    print_window_ms: int = 1_000,
) -> dict[str, Any]:
    """Pure causal ledger. ``trades`` = maker-venue prints {ts, sgn(+1 buy aggressor), px, sz}."""
    import numpy as np

    n = len(mk_bid)
    mk_mid = 0.5 * (mk_bid + mk_ask)
    hd_mid = 0.5 * (hd_bid + hd_ask)
    with np.errstate(invalid="ignore", divide="ignore"):
        b = np.log(mk_mid) - np.log(hd_mid)
    bbar = lg.ema_causal(b, alpha)
    res: dict[str, Any] = {"fills": 0}
    if trades is None or len(trades["ts"]) == 0:
        return res
    # Quote computed at decision step k (time t0+(k+1)*step) is live in
    # [t0+(k+1)*step + delay, t0+(k+2)*step + delay)  ->  k = floor((ts-delay-t0)/step) - 1.
    ks = (trades["ts"] - maker_delay_ms - t0_ms) // STEP_MS - 1
    order = np.argsort(ks, kind="stable")
    ks, px, sz, sg = ks[order], trades["px"][order], trades["sz"][order], trades["sgn"][order]
    ts_sorted = trades["ts"][order]
    uniq, starts = np.unique(ks, return_index=True)
    ends = np.append(starts[1:], len(ks))
    pos_mk = 0.0  # base units on maker venue
    pos_hd = 0.0
    cash = 0.0
    fees = 0.0
    mk_notional = 0.0
    hd_notional = 0.0
    edges: list[float] = []
    hedge_slip: list[float] = []
    fills_bid = fills_ask = 0
    max_q = 0.0
    print_delta = 0.0
    hedge_stale = 0
    print_checked = 0
    hp = hedge_prints if hedge_prints is not None and len(hedge_prints["ts"]) else None
    last_k = warmup_steps + decision_steps
    for k, s, e in zip(uniq, starts, ends):
        if k < warmup_steps or k >= last_k or k >= n:
            continue
        if mk_age[k] > max_age_steps or hd_age[k] > max_age_steps:
            continue
        mb, ma, hb, ha, fb = mk_bid[k], mk_ask[k], hd_bid[k], hd_ask[k], bbar[k]
        if not (mb > 0 and ma > mb and hb > 0 and ha > hb and fb == fb):
            continue
        q = pos_mk * mk_mid[k] / clip_usd
        # Lead-adjusted hedge touch: if the maker venue already moved away from fair, the
        # (slower) hedge venue is expected to follow by lead_beta of the gap during the delay.
        if lead_beta:
            shift = math.exp(lead_beta * (b[k] - fb))
            hb, ha = hb * shift, ha * shift
        quotes = []
        if q < q_max:
            e_bid = e0_bps + skew_bps * q
            cap = hb * math.exp(fb - e_bid * 1e-4)
            bid_q = mb + tick if (improve and ma - mb > 1.5 * tick) else mb
            bid_q = min(bid_q, math.floor(cap / tick) * tick)
            if bid_q > 0 and bid_q < ma:
                quotes.append((1, bid_q))
        if q > -q_max:
            e_ask = e0_bps - skew_bps * q
            floor_ = ha * math.exp(fb + e_ask * 1e-4)
            ask_q = ma - tick if (improve and ma - mb > 1.5 * tick) else ma
            ask_q = max(ask_q, math.ceil(floor_ / tick) * tick)
            if ask_q > mb:
                quotes.append((-1, ask_q))
        tpx, tsz, tsg, tts = px[s:e], sz[s:e], sg[s:e], ts_sorted[s:e]
        for side, qp in quotes:
            through = (tsg == -1) & (tpx < qp) if side == 1 else (tsg == 1) & (tpx > qp)
            if not through.any():
                continue
            notional = min(clip_usd, float((tpx[through] * tsz[through]).sum()))
            if notional <= 0:
                continue
            qty = notional / qp
            tf = int(tts[through][0])
            j = (tf + hedge_delay_ms - t0_ms) // STEP_MS - 1
            j = min(max(j, 0), n - 1)
            if hd_age[j] > max_age_steps:
                # Hedge book stale at the delayed hedge time: execute at the first FRESH
                # hedge book afterwards (bounded), never at a stale price; count it.
                j_fresh = j
                while j_fresh < n - 1 and hd_age[j_fresh] > max_age_steps and j_fresh - j < 600:
                    j_fresh += 1
                hedge_stale += 1
                j = j_fresh
            h_px = hd_bid[j] if side == 1 else hd_ask[j]
            h_ref = hd_bid[k] if side == 1 else hd_ask[k]
            if not (h_px > 0):
                continue
            # maker leg
            cash -= side * qty * qp
            fees += qty * qp * maker_fee_bps * 1e-4
            pos_mk += side * qty
            # hedge leg (opposite side, taker)
            cash += side * qty * h_px
            fees += qty * h_px * hedge_fee_bps * 1e-4
            pos_hd -= side * qty
            if hp is not None:
                t_h = tf + hedge_delay_ms
                lo = np.searchsorted(hp["ts"], t_h, side="left")
                hi = np.searchsorted(hp["ts"], t_h + print_window_ms, side="left")
                if hi > lo:
                    m = hp["sgn"][lo:hi] == -side
                    if m.any():
                        p_print = float(hp["px"][lo:hi][m].mean())
                        print_delta += side * qty * (p_print - h_px)
                        print_checked += 1
            mk_notional += qty * qp
            hd_notional += qty * h_px
            entered = math.log(qp) - math.log(h_px)
            edges.append(side * (fb - entered) * 1e4)
            hedge_slip.append(side * (h_px / h_ref - 1.0) * 1e4)
            if side == 1:
                fills_bid += 1
            else:
                fills_ask += 1
            max_q = max(max_q, abs(pos_mk * mk_mid[k] / clip_usd))
    # Terminal drain at end of data with takers on both legs.
    idx_end = n - 1
    mtm_pre = cash - fees + pos_mk * mk_mid[idx_end] + pos_hd * hd_mid[idx_end]
    drain_cash = 0.0
    drain_fees = 0.0
    if pos_mk:
        p = mk_bid[idx_end] if pos_mk > 0 else mk_ask[idx_end]
        drain_cash += pos_mk * p
        drain_fees += abs(pos_mk) * p * maker_taker_fee_bps * 1e-4
    if pos_hd:
        p = hd_bid[idx_end] if pos_hd > 0 else hd_ask[idx_end]
        drain_cash += pos_hd * p
        drain_fees += abs(pos_hd) * p * hedge_fee_bps * 1e-4
    pnl = cash - fees + drain_cash - drain_fees
    total_notional = mk_notional + hd_notional
    fills = fills_bid + fills_ask
    res.update(
        {
            "fills": fills,
            "fills_bid": fills_bid,
            "fills_ask": fills_ask,
            "maker_notional_usd": round(mk_notional, 2),
            "total_notional_usd": round(total_notional, 2),
            "pnl_usd": round(pnl, 4),
            "mtm_before_drain_usd": round(mtm_pre, 4),
            "drain_cost_usd": round(mtm_pre - pnl, 4),
            "fees_usd": round(fees + drain_fees, 4),
            "pnl_bps_of_maker_notional": round(pnl / mk_notional * 1e4, 3) if mk_notional else 0.0,
            "mean_entry_edge_vs_fair_bps": round(float(np.mean(edges)), 3) if edges else 0.0,
            "mean_hedge_slip_bps": round(float(np.mean(hedge_slip)), 3) if hedge_slip else 0.0,
            "max_abs_clips": round(max_q, 2),
            "terminal_clips": round(pos_mk * mk_mid[idx_end] / clip_usd, 2),
            "hedge_print_checked": print_checked,
            "hedge_stale_book_events": hedge_stale,
            "pnl_print_hedge_usd": round(pnl + print_delta, 4) if hp is not None else None,
        }
    )
    return res


def load_pair(con, lakehouse: Path, window: str, pair):
    mv, mm, hv, hm, domain, _ = pair
    camp = lg.WINDOWS[window][domain]
    mf = lg.campaign_files(lakehouse, "bbo_quotes", mv, camp)
    hf = lg.campaign_files(lakehouse, "bbo_quotes", hv, camp)
    ms, hs = lg.campaign_span(con, mf, mm), lg.campaign_span(con, hf, hm)
    if ms is None or hs is None:
        return None
    t0, t1 = max(ms[0], hs[0]), min(ms[1], hs[1])
    mc, mst = lg.load_bucket_rows(con, mf, mm, STEP_MS)
    hc, hst = lg.load_bucket_rows(con, hf, hm, STEP_MS)
    mg, hg = lg.build_grid(mc, mst, t0, t1, STEP_MS), lg.build_grid(hc, hst, t0, t1, STEP_MS)
    trades = lg.load_trades(con, lg.campaign_files(lakehouse, "public_trades", mv, camp), mm, t0, t1)
    hedge_prints = lg.load_trades(con, lg.campaign_files(lakehouse, "public_trades", hv, camp), hm, t0, t1)
    if mg is None or hg is None:
        return None
    return {
        "mk": mg,
        "hd": hg,
        "trades": trades,
        "hedge_prints": hedge_prints,
        "cadence": {"maker": mg.cadence, "hedge": hg.cadence},
        "t0": t0,
        "tick": _tick_from(mg.bid),
        "continuous_10h": (t1 - t0) >= WARMUP_MS + DECISION_MS
        and max(mst["max_gap_ms"], hst["max_gap_ms"]) <= 60_000,
        "maker_prints": 0 if trades is None else int(len(trades["ts"])),
        "span_h": round((t1 - t0) / 3_600_000, 3),
        "max_gap_ms": {"maker": mst["max_gap_ms"], "hedge": hst["max_gap_ms"]},
    }


def run_variant(data, pair, e0, tau_s, improve, lead_beta=0.0, arcus_taker_override: float | None = None):
    mv, _, hv, _, domain, _ = pair
    def taker(v):
        if v == "arcus" and arcus_taker_override is not None:
            return arcus_taker_override
        return TAKER_FEE_BPS[(v, domain)]
    return simulate_maker_hedge(
        data["mk"].bid, data["mk"].ask, data["mk"].age_steps,
        data["hd"].bid, data["hd"].ask, data["hd"].age_steps,
        data["trades"],
        t0_ms=data["t0"],
        e0_bps=e0,
        alpha=1.0 - math.exp(-STEP_MS / (tau_s * 1000.0)),
        improve=improve,
        tick=data["tick"],
        lead_beta=lead_beta,
        maker_delay_ms=ORDER_DELAY_MS[mv],
        hedge_delay_ms=ORDER_DELAY_MS[hv],
        maker_fee_bps=MAKER_FEE_BPS[mv],
        maker_taker_fee_bps=taker(mv),
        hedge_fee_bps=taker(hv),
        warmup_steps=WARMUP_MS // STEP_MS,
        decision_steps=DECISION_MS // STEP_MS,
        max_age_steps=MAX_AGE_MS // STEP_MS,
        hedge_prints=data.get("hedge_prints"),
    )


def select_params(grid: list[dict[str, Any]]) -> dict[str, Any] | None:
    ok = [r for r in grid if r["fills"] >= MIN_FILLS and r.get("pnl_usd", 0) > 0]
    return max(ok, key=lambda r: r["pnl_usd"]) if ok else None


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--lakehouse", type=Path, default=Path.home() / "aleph-tx-lakehouse")
    ap.add_argument("--output", type=Path, required=True)
    ap.add_argument("--pairs", default="")
    args = ap.parse_args()
    con = lg.connect()
    filt = [s.strip() for s in args.pairs.split(",") if s.strip()]
    pairs = [p for p in PAIRS if not filt or any(f in p[5] for f in filt)]
    out: dict[str, Any] = {
        "schema": "track02_maker_hedge_basis_v1",
        "protocol": "select on w19; frozen replay on w24 (holdout) and w23 (holdout_2)",
        "grid": {"e0_bps": E0_BPS, "tau_s": TAUS_S, "improve_one_tick": IMPROVE, "lead_beta": LEAD_BETAS},
        "variants_per_pair": len(E0_BPS) * len(TAUS_S) * len(IMPROVE) * len(LEAD_BETAS),
        "fixed": {"clip_usd": CLIP_USD, "q_max_clips": Q_MAX, "skew_bps_per_clip": SKEW_BPS_PER_CLIP,
                  "fill_rule": "strict trade-through only", "funding": "excluded"},
        "order_delay_ms": ORDER_DELAY_MS,
        "pairs": [],
    }
    for pair in pairs:
        rec: dict[str, Any] = {
            "label": pair[5],
            "maker": f"{pair[0]}:{pair[1]}",
            "hedge": f"{pair[2]}:{pair[3]}",
            "settlement": f"{SETTLEMENT[pair[0]]}/{SETTLEMENT[pair[2]]}",
            "cross_stablecoin": SETTLEMENT[pair[0]] != SETTLEMENT[pair[2]],
        }
        sel = load_pair(con, args.lakehouse, "w19", pair)
        if sel is None:
            rec["status"] = "missing_selection_data"
            out["pairs"].append(rec)
            continue
        rec["w19_continuous_10h"] = sel["continuous_10h"]
        rec["w19_maker_prints"] = sel["maker_prints"]
        rec["w19_span_h"] = sel["span_h"]
        rec["w19_max_gap_ms"] = sel["max_gap_ms"]
        grid = []
        for beta in LEAD_BETAS:
            for tau in TAUS_S:
                for imp in IMPROVE:
                    for e0 in E0_BPS:
                        r = run_variant(sel, pair, e0, tau, imp, beta)
                        r.update({"e0_bps": e0, "tau_s": tau, "improve": imp, "lead_beta": beta})
                        grid.append(r)
        rec["w19_positive_variants"] = sum(1 for r in grid if r["fills"] >= MIN_FILLS and r.get("pnl_usd", 0) > 0)
        chosen = select_params(grid)
        rec["w19_best"] = chosen
        rec["w19_grid"] = [{k: r.get(k) for k in ("e0_bps", "tau_s", "improve", "lead_beta", "fills", "pnl_usd", "maker_notional_usd", "mean_hedge_slip_bps")} for r in grid]
        del sel
        if chosen is None:
            rec["status"] = "no_go_selection"
            out["pairs"].append(rec)
            print(json.dumps({"pair": pair[5], "status": "no_go"}), flush=True)
            continue
        frozen = {k: chosen[k] for k in ("e0_bps", "tau_s", "improve", "lead_beta")}
        rec["frozen_params"] = frozen
        for w in ("w24", "w23"):
            data = load_pair(con, args.lakehouse, w, pair)
            if data is None:
                rec[w] = {"status": "missing"}
                continue
            r = run_variant(data, pair, frozen["e0_bps"], frozen["tau_s"], frozen["improve"], frozen["lead_beta"])
            r["continuous_10h"] = data["continuous_10h"]
            r["span_h"] = data["span_h"]
            r["cadence"] = data["cadence"]
            r["max_gap_ms"] = data["max_gap_ms"]
            if "arcus" in (pair[0], pair[2]) and pair[4] == "rwa":
                r["arcus_level0_2p25"] = run_variant(
                    data, pair, frozen["e0_bps"], frozen["tau_s"], frozen["improve"], frozen["lead_beta"],
                    arcus_taker_override=2.25
                ).get("pnl_usd")
            rec[w] = r
        rec["status"] = "evaluated"
        out["pairs"].append(rec)
        print(json.dumps({"pair": pair[5], "frozen": frozen, "w19": chosen.get("pnl_usd"),
                          "w24": rec.get("w24", {}).get("pnl_usd"), "w23": rec.get("w23", {}).get("pnl_usd")}), flush=True)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(out, indent=2, allow_nan=False) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    sys.exit(main())
