#!/usr/bin/env python3
"""Track 02 (`02_hedge_arbitrage`) upgrade: Speed-Bump Maker + External Cancel Shield.

Exploits Lighter's asymmetric speed-bump physics (Maker place/cancel = 0 ms speed bump
+ ~40 ms Tokyo RTT; Taker = 200 ms RH / 300 ms Core + RTT):
Before accepting a trade-through fill on Lighter at ``t_fill``, inspect an external
reference feed (Binance for crypto; peer WS venue for RWA) strictly inside the causal
cancel window ``[t_fill - speedbump_ms + rtt_ms, t_fill - rtt_ms]``. If the reference
moved adversely by ``>= shield_bps`` within that window, our 0 ms speed-bump cancel
arrives at Lighter before the incoming taker executes, blocking the toxic fill.

Cohorts evaluated (strictly separated):
* Cohort A (`lighter_robinhood <-> lighter`, 0/0 Standard fee, 50 ms WS): flagged as
  `cross_stablecoin=True` (USDG/USDC) and `same_risk_domain=True`.
* Cohort B (Cross-Risk-Domain `lighter / lighter_rh Maker -> arcus / extended Hedge`):
  reports quote-hedged P&L, strict print-verified hedge P&L (`pnl_print_hedge_usd`),
  `mean_hedge_slip_bps` improvement, `arcus level_0 2.25 bp` fee sensitivity, and
  `rtt_ms = 80 ms` 2x latency stress.

Protocol: select `(e0_bps, tau_s, shield_bps)` on W19 (`selection_dev`) only, freeze,
then replay unchanged on W20..W24 (untouched holdout) and W15..W18 (regression). W25+
excluded. Aggregates only.
"""

from __future__ import annotations

import argparse
import glob
import json
import math
import re
import sys
from pathlib import Path
from typing import Any

SCRIPT_DIR = Path(__file__).resolve().parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

import lakehouse_grid as lg  # noqa: E402

STEP_MS = 50
WARMUP_MS = 15 * 60_000
DECISION_MS = lg.TEN_HOURS_MS
MAX_AGE_MS = 5_000
CLIP_USD = 1_000.0
Q_MAX = 4
SKEW_BPS_PER_CLIP = 0.5
MIN_FILLS_SEL = 15

SPEEDBUMP_MS = {"lighter": 300, "lighter_robinhood": 200}
HEDGE_DELAY_MS = {"lighter": 330, "lighter_robinhood": 230, "arcus": 40, "extended": 60}
TAKER_FEE_BPS = {
    ("lighter", "crypto"): 0.0,
    ("lighter", "rwa"): 0.0,
    ("lighter_robinhood", "crypto"): 0.0,
    ("lighter_robinhood", "rwa"): 0.0,
    ("arcus", "crypto"): 2.30,
    ("arcus", "rwa"): 1.15,
    ("extended", "crypto"): 2.50,
}
SETTLEMENT = {"lighter": "USDC", "lighter_robinhood": "USDG", "arcus": "USDC", "extended": "USDC"}

# Grid over W19 (18 variants per pair).
E0_GRID = (2.0, 4.0, 6.0)
TAU_GRID_S = (600.0, 3_600.0)
SHIELD_GRID_BPS: tuple[float | None, ...] = (None, 1.5, 2.5, 4.0)

# (cohort, mk_venue, mk_mkt, hd_venue, hd_mkt, ref_venue, ref_mkt, domain, label)
COHORT_A_PAIRS = [
    ("cohort_a", "lighter_robinhood", "1", "lighter", "1", "binance", "BTCUSDT", "crypto", "rh>core BTC (0/0)"),
    ("cohort_a", "lighter_robinhood", "0", "lighter", "0", "binance", "ETHUSDT", "crypto", "rh>core ETH (0/0)"),
    ("cohort_a", "lighter_robinhood", "3", "lighter", "2", "binance", "SOLUSDT", "crypto", "rh>core SOL (0/0)"),
    ("cohort_a", "lighter", "1", "lighter_robinhood", "1", "binance", "BTCUSDT", "crypto", "core>rh BTC (0/0)"),
    ("cohort_a", "lighter", "0", "lighter_robinhood", "0", "binance", "ETHUSDT", "crypto", "core>rh ETH (0/0)"),
    ("cohort_a", "lighter", "2", "lighter_robinhood", "3", "binance", "SOLUSDT", "crypto", "core>rh SOL (0/0)"),
    ("cohort_a", "lighter_robinhood", "26", "lighter", "128", "lighter", "128", "rwa", "rh>core SPY (0/0)"),
    ("cohort_a", "lighter_robinhood", "25", "lighter", "129", "lighter", "129", "rwa", "rh>core QQQ (0/0)"),
    ("cohort_a", "lighter_robinhood", "15", "lighter", "110", "lighter", "110", "rwa", "rh>core NVDA (0/0)"),
    ("cohort_a", "lighter_robinhood", "16", "lighter", "112", "lighter", "112", "rwa", "rh>core TSLA (0/0)"),
    ("cohort_a", "lighter_robinhood", "10", "lighter", "113", "lighter", "113", "rwa", "rh>core AAPL (0/0)"),
    ("cohort_a", "lighter_robinhood", "11", "lighter", "114", "lighter", "114", "rwa", "rh>core AMZN (0/0)"),
    ("cohort_a", "lighter_robinhood", "31", "lighter", "164", "lighter", "164", "rwa", "rh>core MU (0/0)"),
]

COHORT_B_PAIRS = [
    ("cohort_b", "lighter", "1", "arcus", "BTC-USD", "binance", "BTCUSDT", "crypto", "lighter>arcus BTC"),
    ("cohort_b", "lighter", "0", "arcus", "ETH-USD", "binance", "ETHUSDT", "crypto", "lighter>arcus ETH"),
    ("cohort_b", "lighter", "2", "arcus", "SOL-USD", "binance", "SOLUSDT", "crypto", "lighter>arcus SOL"),
    ("cohort_b", "lighter_robinhood", "0", "extended", "ETH-USD", "binance", "ETHUSDT", "crypto", "rh>extended ETH"),
    ("cohort_b", "lighter_robinhood", "1", "extended", "BTC-USD", "binance", "BTCUSDT", "crypto", "rh>extended BTC"),
    ("cohort_b", "lighter", "128", "arcus", "SPY-USD", "arcus", "SPY-USD", "rwa", "lighter>arcus SPY"),
    ("cohort_b", "lighter", "129", "arcus", "QQQ-USD", "arcus", "QQQ-USD", "rwa", "lighter>arcus QQQ"),
    ("cohort_b", "lighter", "110", "arcus", "NVDA-USD", "arcus", "NVDA-USD", "rwa", "lighter>arcus NVDA"),
    ("cohort_b", "lighter_robinhood", "25", "arcus", "QQQ-USD", "arcus", "QQQ-USD", "rwa", "rh>arcus QQQ"),
    ("cohort_b", "lighter_robinhood", "26", "arcus", "SPY-USD", "arcus", "SPY-USD", "rwa", "rh>arcus SPY"),
]

ALL_PAIRS = COHORT_A_PAIRS + COHORT_B_PAIRS

# External third-venue reference feeds for RWA shield ablation (--rwa-external-ref).
# Keyed by (mk_venue, mk_mkt, hd_venue, hd_mkt) -> (ref_venue, ref_mkt), strictly
# distinct from both mk_venue and hd_venue so shield_is_self_hedge_venue is False.
RWA_EXTERNAL_REFS: dict[tuple[str, str, str, str], tuple[str, str]] = {
    ("lighter_robinhood", "26", "lighter", "128"): ("arcus", "SPY-USD"),
    ("lighter_robinhood", "25", "lighter", "129"): ("arcus", "QQQ-USD"),
    ("lighter_robinhood", "15", "lighter", "110"): ("arcus", "NVDA-USD"),
    ("lighter_robinhood", "16", "lighter", "112"): ("arcus", "TSLA-USD"),
    ("lighter_robinhood", "10", "lighter", "113"): ("arcus", "AAPL-USD"),
    ("lighter_robinhood", "11", "lighter", "114"): ("arcus", "AMZN-USD"),
    ("lighter_robinhood", "31", "lighter", "164"): ("arcus", "MU-USD"),
    ("lighter", "128", "arcus", "SPY-USD"): ("lighter_robinhood", "26"),
    ("lighter", "129", "arcus", "QQQ-USD"): ("lighter_robinhood", "25"),
    ("lighter", "110", "arcus", "NVDA-USD"): ("lighter_robinhood", "15"),
    ("lighter_robinhood", "25", "arcus", "QQQ-USD"): ("lighter", "129"),
    ("lighter_robinhood", "26", "arcus", "SPY-USD"): ("lighter", "128"),
}


def resolve_pairs(rwa_external_ref: bool = False) -> list[tuple[str, str, str, str, str, str, str, str, str]]:
    """Return configured pairs, optionally replacing RWA shield refs with a third venue."""
    if not rwa_external_ref:
        return list(ALL_PAIRS)
    resolved = []
    for pair in ALL_PAIRS:
        cohort, mv, mm, hv, hm, rv, rm, domain, label = pair
        if domain == "rwa":
            rv, rm = RWA_EXTERNAL_REFS[(mv, mm, hv, hm)]
        resolved.append((cohort, mv, mm, hv, hm, rv, rm, domain, label))
    return resolved


def _tick_from(prices) -> float:
    import numpy as np

    p = np.asarray(prices[np.isfinite(prices)][:20_000])
    if len(p) < 2:
        return 0.01
    d = np.diff(np.unique(np.round(p, 8)))
    d = d[d > 1e-9]
    return float(d.min()) if len(d) else 0.01


def simulate_shielded_maker_hedge(
    mk_bid,
    mk_ask,
    mk_age,
    hd_bid,
    hd_ask,
    hd_age,
    ref_mid,
    ref_age,
    mk_trades: dict[str, Any] | None,
    hd_trades: dict[str, Any] | None,
    *,
    t0_ms: int,
    e0_bps: float,
    alpha: float,
    tick: float,
    speedbump_ms: int,
    rtt_ms: int,
    hedge_delay_ms: int,
    maker_taker_fee_bps: float,
    hedge_fee_bps: float,
    shield_bps: float | None,
    warmup_steps: int = WARMUP_MS // STEP_MS,
    decision_steps: int = DECISION_MS // STEP_MS,
    max_age_steps: int = MAX_AGE_MS // STEP_MS,
    clip_usd: float = CLIP_USD,
    q_max: int = Q_MAX,
    skew_bps: float = SKEW_BPS_PER_CLIP,
    cross_stablecoin_bps: float = 0.0,
) -> dict[str, Any]:
    """Causal Maker-First ledger with pre-fill external cancel shield inside Lighter speed bump."""
    import numpy as np

    n = len(mk_bid)
    mk_mid = 0.5 * (mk_bid + mk_ask)
    hd_mid = 0.5 * (hd_bid + hd_ask)
    with np.errstate(invalid="ignore", divide="ignore"):
        b = np.log(mk_mid) - np.log(hd_mid)
    bbar = lg.ema_causal(b, alpha)
    if mk_trades is None or len(mk_trades["ts"]) == 0:
        return {"fills": 0, "blocked_toxic": 0, "unhedged_stale_skips": 0, "pnl_usd": 0.0, "pnl_print_hedge_usd": None}

    # Passive quote computed at index k (wall time t0 + (k+1)*STEP_MS) goes live after 40ms maker RTT.
    maker_delay_ms = rtt_ms
    ks = (mk_trades["ts"] - maker_delay_ms - t0_ms) // STEP_MS - 1
    order = np.argsort(ks, kind="stable")
    ks = ks[order]
    px = mk_trades["px"][order]
    sz = mk_trades["sz"][order]
    sg = mk_trades["sgn"][order]
    ts_sorted = mk_trades["ts"][order]
    uniq, starts = np.unique(ks, return_index=True)
    ends = np.append(starts[1:], len(ks))

    hp = hd_trades if hd_trades is not None and len(hd_trades["ts"]) > 0 else None

    pos_mk = 0.0
    pos_hd = 0.0
    cash = 0.0
    fees = 0.0
    mk_notional = 0.0
    hedge_slip: list[float] = []
    fills_bid = fills_ask = 0
    blocked_toxic = 0
    unhedged_stale_skips = 0
    print_delta = 0.0
    print_checked = 0
    last_k = warmup_steps + decision_steps

    def ref_idx_at(t_ms: int) -> int:
        idx = int((t_ms - t0_ms) // STEP_MS) - 1
        return min(max(idx, 0), n - 1)

    for k, s, e in zip(uniq, starts, ends):
        if k < warmup_steps or k >= last_k or k >= n:
            continue
        if mk_age[k] > max_age_steps or hd_age[k] > max_age_steps:
            continue
        mb, ma, hb, ha, fb = mk_bid[k], mk_ask[k], hd_bid[k], hd_ask[k], bbar[k]
        if not (mb > 0 and ma > mb and hb > 0 and ha > hb and fb == fb):
            continue
        q = pos_mk * mk_mid[k] / clip_usd
        quotes = []
        if q < q_max:
            cap = hb * math.exp(fb - (e0_bps + skew_bps * q) * 1e-4)
            bid_q = min(mb, math.floor(cap / tick + 1e-9) * tick)
            if bid_q > 0 and bid_q < ma:
                quotes.append((1, bid_q))
        if q > -q_max:
            floor_ = ha * math.exp(fb + (e0_bps - skew_bps * q) * 1e-4)
            ask_q = max(ma, math.ceil(floor_ / tick - 1e-9) * tick)
            if ask_q > mb:
                quotes.append((-1, ask_q))
        tpx, tsz, tsg, tts = px[s:e], sz[s:e], sg[s:e], ts_sorted[s:e]
        for side, qp in quotes:
            through = (tsg == -1) & (tpx < qp) if side == 1 else (tsg == 1) & (tpx > qp)
            if not through.any():
                continue
            tf = int(tts[through][0])
            # Causal Pre-Fill Cancel Shield check inside [tf - speedbump_ms + rtt_ms, tf - rtt_ms].
            if shield_bps is not None and speedbump_ms > 2 * rtt_ms:
                i_start = ref_idx_at(tf - speedbump_ms + rtt_ms)
                i_end = ref_idx_at(tf - rtt_ms)
                if (
                    ref_age[i_start] <= max_age_steps
                    and ref_age[i_end] <= max_age_steps
                    and ref_mid[i_start] > 0
                    and ref_mid[i_end] > 0
                ):
                    ref_ret_bps = (ref_mid[i_end] / ref_mid[i_start] - 1.0) * 1e4
                    # Bid (side=+1) is toxic when ref dumps <= -shield_bps; Ask (side=-1) when ref spikes >= +shield_bps.
                    if (side == 1 and ref_ret_bps <= -shield_bps) or (side == -1 and ref_ret_bps >= shield_bps):
                        blocked_toxic += 1
                        continue
            notional = min(clip_usd, float((tpx[through] * tsz[through]).sum()))
            if notional <= 0:
                continue
            qty = notional / qp
            j = min(max(int((tf + hedge_delay_ms - t0_ms) // STEP_MS) - 1, 0), n - 1)
            if hd_age[j] > max_age_steps:
                j_f = j
                while j_f < n - 1 and hd_age[j_f] > max_age_steps and j_f - j < 1_200:
                    j_f += 1
                j = j_f
            h_px = float(hd_bid[j] if side == 1 else hd_ask[j])
            h_ref = float(hb if side == 1 else ha)
            if hd_age[j] > max_age_steps or not (h_px > 0):
                unhedged_stale_skips += 1
                continue
            cash -= side * qty * qp
            pos_mk += side * qty
            cash += side * qty * h_px
            fees += qty * h_px * hedge_fee_bps * 1e-4
            pos_hd -= side * qty
            fill_notl = qty * qp
            mk_notional += fill_notl
            if cross_stablecoin_bps:
                fees += fill_notl * cross_stablecoin_bps * 1e-4
            hedge_slip.append(side * (h_px / h_ref - 1.0) * 1e4)
            if hp is not None:
                t_h = tf + hedge_delay_ms
                lo = int(np.searchsorted(hp["ts"], t_h, side="left"))
                hi = int(np.searchsorted(hp["ts"], t_h + 1_000, side="left"))
                if hi > lo:
                    m = hp["sgn"][lo:hi] == -side
                    if m.any():
                        p_print = float(hp["px"][lo:hi][m].mean())
                        print_delta += side * qty * (p_print - h_px)
                        print_checked += 1
            if side == 1:
                fills_bid += 1
            else:
                fills_ask += 1

    idx_end = n - 1
    drain_cash = drain_fees = 0.0
    if pos_mk:
        p = float(mk_bid[idx_end] if pos_mk > 0 else mk_ask[idx_end])
        drain_cash += pos_mk * p
        drain_fees += abs(pos_mk) * p * maker_taker_fee_bps * 1e-4
    if pos_hd:
        p = float(hd_bid[idx_end] if pos_hd > 0 else hd_ask[idx_end])
        drain_cash += pos_hd * p
        drain_fees += abs(pos_hd) * p * hedge_fee_bps * 1e-4
    pnl = cash - fees + drain_cash - drain_fees
    fills = int(fills_bid + fills_ask)
    return {
        "fills": fills,
        "blocked_toxic": int(blocked_toxic),
        "unhedged_stale_skips": int(unhedged_stale_skips),
        "maker_notional_usd": round(float(mk_notional), 2),
        "pnl_usd": round(float(pnl), 4),
        "pnl_print_hedge_usd": round(float(pnl + print_delta), 4) if hp is not None else None,
        "hedge_print_checked": int(print_checked),
        "mean_hedge_slip_bps": round(float(np.mean(hedge_slip)), 3) if hedge_slip else 0.0,
    }


def discover_windows(lakehouse: Path, domain: str, max_window: int = 24) -> dict[int, str]:
    sub = "tokyo-crypto-*" if domain == "crypto" else "tokyo-rwa-*"
    pat = str(Path(lakehouse) / f"parquet/bbo_quotes/collector_region=tokyo/venue=lighter/utc_date=*/campaign_id={sub}")
    out = {}
    for d in glob.glob(pat):
        cid = d.rsplit("campaign_id=", 1)[1]
        m = re.search(r"-w(\d+)$", cid)
        if m:
            w = int(m.group(1))
            if 15 <= w <= max_window:
                out[w] = cid
    return dict(sorted(out.items()))


def load_pair_data(con, lakehouse: Path, cid: str, pair, raw_cache: dict | None = None):
    _, mv, mm, hv, hm, rv, rm, _, _ = pair

    def get_bbo(v: str, m: str):
        key = ("bbo", cid, v, m)
        if raw_cache is not None and key in raw_cache:
            return raw_cache[key]
        fs = lg.campaign_files(lakehouse, "bbo_quotes", v, cid)
        sp = lg.campaign_span(con, fs, m)
        res = (*lg.load_bucket_rows(con, fs, m, STEP_MS), sp) if sp else (None, None, None)
        if raw_cache is not None:
            raw_cache[key] = res
        return res

    def get_tr(v: str, m: str):
        key = ("tr", cid, v, m)
        if raw_cache is not None and key in raw_cache:
            return raw_cache[key]
        fs = lg.campaign_files(lakehouse, "public_trades", v, cid)
        res = lg.load_trades(con, fs, m, 0, 9_999_999_999_999) if fs else None
        if raw_cache is not None:
            raw_cache[key] = res
        return res

    mc, mst, ms = get_bbo(mv, mm)
    hc, hst, hs = get_bbo(hv, hm)
    rc, rst, rs = (hc, hst, hs) if (rv == hv and rm == hm) else get_bbo(rv, rm)
    if not (ms and hs and rs):
        return None
    t0, t1 = max(ms[0], hs[0], rs[0]), min(ms[1], hs[1], rs[1])
    if t1 - t0 < 2 * WARMUP_MS:
        return None
    mg = lg.build_grid(mc, mst, t0, t1, STEP_MS)
    hg = lg.build_grid(hc, hst, t0, t1, STEP_MS)
    rg = lg.build_grid(rc, rst, t0, t1, STEP_MS)
    if not (mg and hg and rg):
        return None
    mt = get_tr(mv, mm)
    ht = get_tr(hv, hm)
    return {"mk": mg, "hd": hg, "ref": rg, "mt": mt, "ht": ht, "t0": t0, "tick": _tick_from(mg.bid)}


def eval_pair_variant(
    data,
    pair,
    e0: float,
    tau_s: float,
    shield_bps: float | None,
    *,
    rtt_ms=40,
    arcus_fee=None,
    cross_stablecoin_bps: float = 0.0,
):
    _, mv, _, hv, _, _, _, domain, _ = pair
    h_fee = arcus_fee if (hv == "arcus" and arcus_fee is not None) else TAKER_FEE_BPS[(hv, domain)]
    m_tfee = arcus_fee if (mv == "arcus" and arcus_fee is not None) else TAKER_FEE_BPS[(mv, domain)]
    return simulate_shielded_maker_hedge(
        data["mk"].bid, data["mk"].ask, data["mk"].age_steps,
        data["hd"].bid, data["hd"].ask, data["hd"].age_steps,
        data["ref"].mid(), data["ref"].age_steps,
        data["mt"], data["ht"],
        t0_ms=data["t0"], e0_bps=e0, alpha=1.0 - math.exp(-STEP_MS / (tau_s * 1000.0)),
        tick=data["tick"], speedbump_ms=SPEEDBUMP_MS[mv], rtt_ms=rtt_ms,
        hedge_delay_ms=HEDGE_DELAY_MS[hv], maker_taker_fee_bps=m_tfee, hedge_fee_bps=h_fee,
        shield_bps=shield_bps,
        cross_stablecoin_bps=cross_stablecoin_bps,
    )


COHORT_B_SELECTED_LABELS = ("lighter>arcus ETH", "lighter>arcus QQQ", "lighter>arcus NVDA")


def _summarize_cohort(records: list[dict[str, Any]], split_key: str) -> dict[str, Any]:
    rows = [r[split_key] for r in records if r.get("status") == "evaluated" and r.get(split_key)]
    rows = [x for x in rows if x.get("windows", 0) > 0]
    if not rows:
        return {"pairs": 0, "windows": 0}
    pr = [x["print_hedge_pnl_usd"] for x in rows if x.get("print_hedge_pnl_usd") is not None]
    upr = [x["unshielded_print_hedge_pnl_usd"] for x in rows if x.get("unshielded_print_hedge_pnl_usd") is not None]
    cs1 = [x["cross_stablecoin_1bp_pnl_usd"] for x in rows if x.get("cross_stablecoin_1bp_pnl_usd") is not None]
    cs2 = [x["cross_stablecoin_2bp_pnl_usd"] for x in rows if x.get("cross_stablecoin_2bp_pnl_usd") is not None]
    return {
        "pairs": len(rows),
        "pnl_usd": round(float(sum(x["pnl_usd"] for x in rows)), 2),
        "unshielded_pnl_usd": round(float(sum(x["unshielded_pnl_usd"] for x in rows)), 2),
        "print_hedge_pnl_usd": round(float(sum(pr)), 2) if pr else None,
        "unshielded_print_hedge_pnl_usd": round(float(sum(upr)), 2) if upr else None,
        "fills": int(sum(x["fills"] for x in rows)),
        "blocked_toxic": int(sum(x["blocked_toxic"] for x in rows)),
        "unhedged_stale_skips": int(sum(x.get("unhedged_stale_skips", 0) for x in rows)),
        "cross_stablecoin_1bp_pnl_usd": round(float(sum(cs1)), 2) if cs1 else None,
        "cross_stablecoin_2bp_pnl_usd": round(float(sum(cs2)), 2) if cs2 else None,
    }


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--lakehouse", type=Path, default=Path.home() / "aleph-tx-lakehouse")
    ap.add_argument("--output", type=Path, required=True)
    ap.add_argument("--max-window", type=int, default=24, help="maximum window number to include (25 for post-hoc W25)")
    ap.add_argument(
        "--rwa-external-ref",
        action="store_true",
        help="use a strictly external third venue as the RWA shield reference feed",
    )
    args = ap.parse_args()
    con = lg.connect()
    wins_by_dom = {
        "crypto": discover_windows(args.lakehouse, "crypto", max_window=args.max_window),
        "rwa": discover_windows(args.lakehouse, "rwa", max_window=args.max_window),
    }

    out: dict[str, Any] = {
        "schema": "track02_shielded_maker_hedge_v1",
        "protocol": "W19 selects (e0, tau_s, shield_bps); frozen replay on W20-W24 holdout + W15-W18 regression (optional W25 separate)",
        "max_window": int(args.max_window),
        "rwa_external_ref_mode": bool(args.rwa_external_ref),
        "variants_per_pair": len(E0_GRID) * len(TAU_GRID_S) * len(SHIELD_GRID_BPS),
        "pairs": [],
    }
    raw_cache: dict[Any, Any] = {}
    last_dom = None
    for pair in sorted(resolve_pairs(args.rwa_external_ref), key=lambda x: x[7]):
        cohort, mv, mm, hv, hm, rv, rm, domain, label = pair
        if last_dom != domain:
            raw_cache.clear()
            last_dom = domain
        rec: dict[str, Any] = {
            "cohort": cohort, "label": label, "domain": domain,
            "settlement": f"{SETTLEMENT[mv]}/{SETTLEMENT[hv]}",
            "cross_stablecoin": SETTLEMENT[mv] != SETTLEMENT[hv],
            "same_risk_domain": mv.startswith("lighter") and hv.startswith("lighter"),
            "shield_is_self_hedge_venue": bool(rv == hv and rm == hm),
        }
        w19_cid = wins_by_dom[domain].get(19)
        d19 = load_pair_data(con, args.lakehouse, w19_cid, pair, raw_cache) if w19_cid else None
        if d19 is None:
            rec["status"] = "missing_w19"
            out["pairs"].append(rec)
            continue
        grid = []
        for e0 in E0_GRID:
            for tau in TAU_GRID_S:
                for sh in SHIELD_GRID_BPS:
                    r = eval_pair_variant(d19, pair, e0, tau, sh)
                    r.update({"e0_bps": e0, "tau_s": tau, "shield_bps": sh})
                    grid.append(r)
        ok = [g for g in grid if g["fills"] >= MIN_FILLS_SEL and g["pnl_usd"] > 0]
        rec["w19_grid"] = grid
        rec["w19_best_unshielded"] = max(
            [g for g in grid if g["shield_bps"] is None and g["fills"] >= 5],
            key=lambda g: g["pnl_usd"],
            default=None,
        )
        if not ok:
            rec["status"] = "no_go_w19"
            out["pairs"].append(rec)
            del d19
            continue
        best = max(ok, key=lambda g: g["pnl_usd"])
        fz = {"e0_bps": best["e0_bps"], "tau_s": best["tau_s"], "shield_bps": best["shield_bps"]}
        rec["frozen"] = fz
        rec["w19_best"] = best
        best_unsh = rec["w19_best_unshielded"]
        del d19

        hold_rows, reg_rows, w25_rows = [], [], []
        for w, cid in sorted(wins_by_dom[domain].items()):
            if w == 19:
                continue
            d = load_pair_data(con, args.lakehouse, cid, pair, raw_cache)
            if d is None:
                continue
            r_sh = eval_pair_variant(d, pair, fz["e0_bps"], fz["tau_s"], fz["shield_bps"])
            r_un = eval_pair_variant(d, pair, fz["e0_bps"], fz["tau_s"], None)
            r_rtt80 = eval_pair_variant(d, pair, fz["e0_bps"], fz["tau_s"], fz["shield_bps"], rtt_ms=80)
            row: dict[str, Any] = {
                "window": w, "shielded": r_sh, "unshielded_same_params": r_un,
                "rtt80_pnl_usd": r_rtt80["pnl_usd"], "rtt80_fills": r_rtt80["fills"],
            }
            if hv == "arcus" and domain == "rwa":
                row["arcus_level0_2p25_pnl_usd"] = eval_pair_variant(
                    d, pair, fz["e0_bps"], fz["tau_s"], fz["shield_bps"], arcus_fee=2.25
                )["pnl_usd"]
            if rec["cross_stablecoin"]:
                row["cross_stablecoin_1bp_pnl_usd"] = eval_pair_variant(
                    d, pair, fz["e0_bps"], fz["tau_s"], fz["shield_bps"], cross_stablecoin_bps=1.0
                )["pnl_usd"]
                row["cross_stablecoin_2bp_pnl_usd"] = eval_pair_variant(
                    d, pair, fz["e0_bps"], fz["tau_s"], fz["shield_bps"], cross_stablecoin_bps=2.0
                )["pnl_usd"]
            if w == 25:
                w25_rows.append(row)
            elif w >= 20:
                hold_rows.append(row)
            else:
                reg_rows.append(row)
            del d

        def _agg(rows):
            if not rows:
                return {"windows": 0}
            pnls = [x["shielded"]["pnl_usd"] for x in rows]
            upnls = [x["unshielded_same_params"]["pnl_usd"] for x in rows]
            pr_pnls = [x["shielded"]["pnl_print_hedge_usd"] for x in rows
                       if x["shielded"]["pnl_print_hedge_usd"] is not None]
            upr_pnls = [x["unshielded_same_params"]["pnl_print_hedge_usd"] for x in rows
                        if x["unshielded_same_params"]["pnl_print_hedge_usd"] is not None]
            sh_slips = [x["shielded"]["mean_hedge_slip_bps"] for x in rows if x["shielded"]["fills"] > 0]
            un_slips = [x["unshielded_same_params"]["mean_hedge_slip_bps"] for x in rows
                        if x["unshielded_same_params"]["fills"] > 0]
            return {
                "windows": int(len(rows)),
                "positive_windows": int(sum(1 for v in pnls if v > 0)),
                "pnl_usd": round(float(sum(pnls)), 2),
                "unshielded_pnl_usd": round(float(sum(upnls)), 2),
                "print_hedge_pnl_usd": round(float(sum(pr_pnls)), 2) if pr_pnls else None,
                "unshielded_print_hedge_pnl_usd": round(float(sum(upr_pnls)), 2) if upr_pnls else None,
                "mean_hedge_slip_bps": round(float(sum(sh_slips) / len(sh_slips)), 3) if sh_slips else None,
                "unshielded_mean_hedge_slip_bps": round(float(sum(un_slips) / len(un_slips)), 3) if un_slips else None,
                "rtt80_pnl_usd": round(float(sum(x["rtt80_pnl_usd"] for x in rows)), 2),
                "fills": int(sum(int(x["shielded"]["fills"]) for x in rows)),
                "unshielded_fills": int(sum(int(x["unshielded_same_params"]["fills"]) for x in rows)),
                "blocked_toxic": int(sum(int(x["shielded"]["blocked_toxic"]) for x in rows)),
                "unhedged_stale_skips": int(sum(int(x["shielded"].get("unhedged_stale_skips", 0)) for x in rows)),
                "arcus_level0_2p25_pnl_usd": round(
                    float(sum(x["arcus_level0_2p25_pnl_usd"] for x in rows)), 2
                ) if "arcus_level0_2p25_pnl_usd" in rows[0] else None,
                "cross_stablecoin_1bp_pnl_usd": round(
                    float(sum(x["cross_stablecoin_1bp_pnl_usd"] for x in rows)), 2
                ) if "cross_stablecoin_1bp_pnl_usd" in rows[0] else None,
                "cross_stablecoin_2bp_pnl_usd": round(
                    float(sum(x["cross_stablecoin_2bp_pnl_usd"] for x in rows)), 2
                ) if "cross_stablecoin_2bp_pnl_usd" in rows[0] else None,
            }

        rec["status"] = "evaluated"
        rec["w19_best_unshielded"] = best_unsh
        rec["holdout_w20_w24"] = _agg(hold_rows)
        rec["regression_w15_w18"] = _agg(reg_rows)
        rec["holdout_w25_untouched"] = _agg(w25_rows) if w25_rows else None
        rec["per_window_holdout"] = hold_rows
        if w25_rows:
            rec["per_window_w25"] = w25_rows
        out["pairs"].append(rec)
        print(json.dumps({"cohort": cohort, "label": label, "frozen": fz,
                          "hold": rec["holdout_w20_w24"], "reg": rec["regression_w15_w18"],
                          "w25": rec["holdout_w25_untouched"]}), flush=True)

    cohort_a = [r for r in out["pairs"] if r.get("cohort") == "cohort_a"]
    cohort_b = [r for r in out["pairs"] if r.get("cohort") == "cohort_b"]
    cohort_b_sel = [r for r in cohort_b if r.get("label") in COHORT_B_SELECTED_LABELS]
    out["summary"] = {
        "cohort_a_full": {
            "holdout_w20_w24": _summarize_cohort(cohort_a, "holdout_w20_w24"),
            "regression_w15_w18": _summarize_cohort(cohort_a, "regression_w15_w18"),
            "holdout_w25_untouched": _summarize_cohort(cohort_a, "holdout_w25_untouched") if args.max_window >= 25 else None,
        },
        "cohort_b_full": {
            "holdout_w20_w24": _summarize_cohort(cohort_b, "holdout_w20_w24"),
            "regression_w15_w18": _summarize_cohort(cohort_b, "regression_w15_w18"),
            "holdout_w25_untouched": _summarize_cohort(cohort_b, "holdout_w25_untouched") if args.max_window >= 25 else None,
        },
        "cohort_b_selected_eth_qqq_nvda": {
            "labels": list(COHORT_B_SELECTED_LABELS),
            "holdout_w20_w24": _summarize_cohort(cohort_b_sel, "holdout_w20_w24"),
            "regression_w15_w18": _summarize_cohort(cohort_b_sel, "regression_w15_w18"),
            "holdout_w25_untouched": _summarize_cohort(cohort_b_sel, "holdout_w25_untouched") if args.max_window >= 25 else None,
        },
    }

    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(out, indent=2, allow_nan=False) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    sys.exit(main())
