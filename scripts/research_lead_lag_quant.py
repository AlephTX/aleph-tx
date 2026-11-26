#!/usr/bin/env python3
"""Track 01 (`01_quant_profit`) research: cross-venue lead-lag taker study (read-only).

Hypothesis: fast reference books (Binance for crypto, Arcus WS for RWA) lead slower or
speed-bumped venues. A causal cointegration deviation

    x_t   = log(ref_mid_t) - log(tgt_mid_t)
    dev_t = x_t - EMA_tau(x)_{t-1}                       (bps)

predicts the target's next move. When |dev| >= theta we cross the target book AFTER the
venue's taker delay (speed bump + RTT), hold a fixed horizon and cross back. Every price
is a touch price looked up after the delay, fees are charged on both sides, positions
never overlap, and stale books (either side) block entries.

Protocol (AGENTS.md strategy research contract): parameters are selected on W19
(`selection_dev`) only, frozen, then replayed unchanged on W24 (`holdout`) and W23
(`holdout_2`). All tested variants are counted and reported. Results are a research
hypothesis, not evidence of live profitability. Output holds aggregates only.
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

THETAS_BPS = (1.0, 1.5, 2.0, 3.0, 4.0, 6.0, 8.0)
HOLD_STEPS = (10, 30, 100)
TAUS_S = (2.0, 5.0, 30.0, 120.0)
MIN_TRADES = 30
EXIT_MODES = (False,)

# (ref_venue, ref_market, tgt_venue, tgt_market, domain, label)
PAIRS: list[tuple[str, str, str, str, str, str]] = [
    ("binance", "BTCUSDT", "lighter", "1", "crypto", "lighter BTC <- binance"),
    ("binance", "ETHUSDT", "lighter", "0", "crypto", "lighter ETH <- binance"),
    ("binance", "SOLUSDT", "lighter", "2", "crypto", "lighter SOL <- binance"),
    ("binance", "BTCUSDT", "lighter_robinhood", "1", "crypto", "lighter_rh BTC <- binance"),
    ("binance", "ETHUSDT", "lighter_robinhood", "0", "crypto", "lighter_rh ETH <- binance"),
    ("binance", "SOLUSDT", "lighter_robinhood", "3", "crypto", "lighter_rh SOL <- binance"),
    ("coinbase", "ETH-USD", "lighter_robinhood", "0", "crypto", "lighter_rh ETH <- coinbase"),
    ("coinbase", "BTC-USD", "lighter_robinhood", "1", "crypto", "lighter_rh BTC <- coinbase"),
    ("binance", "BTCUSDT", "arcus", "BTC-USD", "crypto", "arcus BTC <- binance"),
    ("binance", "ETHUSDT", "arcus", "ETH-USD", "crypto", "arcus ETH <- binance"),
    ("binance", "SOLUSDT", "arcus", "SOL-USD", "crypto", "arcus SOL <- binance"),
    ("binance", "BTCUSDT", "extended", "BTC-USD", "crypto", "extended BTC <- binance"),
    ("binance", "ETHUSDT", "extended", "ETH-USD", "crypto", "extended ETH <- binance"),
    ("arcus", "NVDA-USD", "lighter", "110", "rwa", "lighter NVDA <- arcus"),
    ("arcus", "SPY-USD", "lighter", "128", "rwa", "lighter SPY <- arcus"),
    ("arcus", "QQQ-USD", "lighter", "129", "rwa", "lighter QQQ <- arcus"),
    ("arcus", "NVDA-USD", "lighter_robinhood", "15", "rwa", "lighter_rh NVDA <- arcus"),
    ("arcus", "SPY-USD", "lighter_robinhood", "26", "rwa", "lighter_rh SPY <- arcus"),
    ("arcus", "QQQ-USD", "lighter_robinhood", "25", "rwa", "lighter_rh QQQ <- arcus"),
]

# Taker execution delay (speed bump + Tokyo RTT) and audited/assumed taker fees (bps).
TAKER_DELAY_MS = {"lighter": 330, "lighter_robinhood": 230, "arcus": 40, "extended": 60}
TAKER_FEE_BPS = {
    ("lighter", "crypto"): 0.0,
    ("lighter", "rwa"): 0.0,
    ("lighter_robinhood", "crypto"): 0.0,
    ("lighter_robinhood", "rwa"): 0.0,
    ("arcus", "crypto"): 2.30,
    ("arcus", "rwa"): 1.15,
    ("extended", "crypto"): 2.50,
}


def simulate_lead_lag(
    ref_mid,
    tgt_bid,
    tgt_ask,
    ref_age,
    tgt_age,
    *,
    theta_bps: float,
    hold_steps: int,
    alpha: float,
    delay_steps: int,
    fee_bps: float,
    warmup_steps: int,
    decision_steps: int,
    max_age_steps: int,
    prints: dict[str, Any] | None = None,
    t0_ms: int = 0,
    step_ms: int = STEP_MS,
    print_window_ms: int = 1_000,
    exit_on_revert: bool = False,
) -> dict[str, Any]:
    """Pure, causal, non-overlapping taker simulation over aligned grids.

    When target ``prints`` are supplied, every trade is also re-priced with REAL public
    prints (entry = mean price of same-direction aggressor prints in the ``print_window_ms``
    after the delayed entry time, exit likewise with opposite-direction prints). This
    guards against throttled/polled target books that lag the true book: a print-verified
    edge cannot come from stale quotes.
    """
    import numpy as np

    tgt_mid = 0.5 * (tgt_bid + tgt_ask)
    with np.errstate(invalid="ignore", divide="ignore"):
        x = np.log(ref_mid) - np.log(tgt_mid)
    ema = lg.ema_causal(x, alpha)
    dev = (x - ema) * 1e4
    n = len(x)
    last_decision = min(n - delay_steps - hold_steps - 1, warmup_steps + decision_steps)
    ok = (
        np.isfinite(dev)
        & (np.abs(dev) >= theta_bps)
        & (ref_age <= max_age_steps)
        & (tgt_age <= max_age_steps)
    )
    cand = np.nonzero(ok)[0]
    cand = cand[(cand >= warmup_steps) & (cand < last_decision)]
    nets: list[float] = []
    gross_mid: list[float] = []
    verified: list[float] = []
    unverifiable = 0
    if prints is not None and len(prints["ts"]):
        p_ts, p_sg, p_px = prints["ts"], prints["sgn"], prints["px"]
    else:
        p_ts = None

    def print_px(t_start: int, sgn: int):
        lo = np.searchsorted(p_ts, t_start, side="left")
        hi = np.searchsorted(p_ts, t_start + print_window_ms, side="left")
        if hi <= lo:
            return None
        m = p_sg[lo:hi] == sgn
        return float(p_px[lo:hi][m].mean()) if m.any() else None

    longs = 0
    nxt = -1
    stale_entry_skips = 0
    deferred_exits = 0
    unsettled_exits = 0
    for i in cand:
        if i <= nxt:
            continue
        side = 1.0 if dev[i] > 0 else -1.0
        e = i + delay_steps
        if tgt_age[e] > max_age_steps:
            stale_entry_skips += 1
            continue
        entry = tgt_ask[e] if side > 0 else tgt_bid[e]
        if not (np.isfinite(entry) and entry > 0):
            continue
        x_ = e + hold_steps
        if exit_on_revert:
            # Exit order is sent at the first decision after entry where the deviation no
            # longer favours the position; it fills after the same taker delay.
            seg = dev[e + 1 : e + hold_steps - delay_steps + 1]
            hit = np.nonzero(side * seg <= 0)[0]
            if len(hit):
                x_ = e + 1 + int(hit[0]) + delay_steps
        x_orig = x_
        exit_quotes = tgt_bid if side > 0 else tgt_ask
        while x_ < n and (
            tgt_age[x_] > max_age_steps or not np.isfinite(exit_quotes[x_]) or exit_quotes[x_] <= 0
        ):
            x_ += 1
        if x_ >= n:
            unsettled_exits += 1
            break
        if x_ > x_orig:
            deferred_exits += 1
        exitp = exit_quotes[x_]
        nets.append(side * (exitp / entry - 1.0) * 1e4 - 2.0 * fee_bps)
        if p_ts is not None:
            sg = 1 if side > 0 else -1
            pe = print_px(t0_ms + (e + 1) * step_ms, sg)
            px_ = print_px(t0_ms + (x_ + 1) * step_ms, -sg)
            if pe and px_:
                verified.append(side * (px_ / pe - 1.0) * 1e4 - 2.0 * fee_bps)
            else:
                unverifiable += 1
        gross_mid.append(side * (tgt_mid[x_] / tgt_mid[e] - 1.0) * 1e4)
        longs += side > 0
        nxt = x_
    k = len(nets)
    execution = {
        "stale_entry_skips": stale_entry_skips,
        "deferred_exits": deferred_exits,
        "unsettled_exits": unsettled_exits,
    }
    if k == 0:
        return {**execution, "trades": 0, "net_bps_total": 0.0, "net_bps_mean": 0.0, "t_stat": 0.0}
    arr = np.asarray(nets)
    sd = float(arr.std(ddof=1)) if k > 1 else 0.0
    pv: dict[str, Any] = {}
    if p_ts is not None:
        va = np.asarray(verified)
        pv = {
            "print_verified_trades": int(len(va)),
            "print_unverifiable": unverifiable,
            "print_verified_net_bps_mean": round(float(va.mean()), 4) if len(va) else None,
            "print_verified_t_stat": round(float(va.mean() / (va.std(ddof=1) / math.sqrt(len(va)))), 2)
            if len(va) > 2 and va.std(ddof=1) > 0 else None,
        }
    return {
        **pv,
        **execution,
        "trades": k,
        "long_share": round(longs / k, 3),
        "net_bps_total": round(float(arr.sum()), 3),
        "net_bps_mean": round(float(arr.mean()), 4),
        "gross_mid_bps_mean": round(float(np.mean(gross_mid)), 4),
        "hit_rate": round(float((arr > 0).mean()), 3),
        "t_stat": round(float(arr.mean() / (sd / math.sqrt(k))) if sd > 0 else 0.0, 2),
        "p05_bps": round(float(np.percentile(arr, 5)), 3),
    }


def information_coefficient(ref_mid, tgt_bid, tgt_ask, alpha, delay_steps, hold_steps, warmup_steps, stride=10):
    import numpy as np

    tgt_mid = 0.5 * (tgt_bid + tgt_ask)
    with np.errstate(invalid="ignore", divide="ignore"):
        x = np.log(ref_mid) - np.log(tgt_mid)
        dev = (x - lg.ema_causal(x, alpha)) * 1e4
        idx = np.arange(warmup_steps, len(x) - delay_steps - hold_steps - 1, stride)
        fwd = (np.log(tgt_mid[idx + delay_steps + hold_steps]) - np.log(tgt_mid[idx + delay_steps])) * 1e4
    d = dev[idx]
    m = np.isfinite(d) & np.isfinite(fwd)
    if m.sum() < 100:
        return None
    c = float(np.corrcoef(d[m], fwd[m])[0, 1])
    return round(c, 4) if math.isfinite(c) else None


def load_pair(con, lakehouse: Path, window: str, pair) -> dict[str, Any] | None:
    ref_v, ref_m, tgt_v, tgt_m, domain, _ = pair
    camp = lg.WINDOWS[window][domain]
    rf = lg.campaign_files(lakehouse, "bbo_quotes", ref_v, camp)
    tf = lg.campaign_files(lakehouse, "bbo_quotes", tgt_v, camp)
    rs, ts = lg.campaign_span(con, rf, ref_m), lg.campaign_span(con, tf, tgt_m)
    if rs is None or ts is None:
        return None
    t0, t1 = max(rs[0], ts[0]), min(rs[1], ts[1])
    rc, rst = lg.load_bucket_rows(con, rf, ref_m, STEP_MS)
    tc, tst = lg.load_bucket_rows(con, tf, tgt_m, STEP_MS)
    rg, tg = lg.build_grid(rc, rst, t0, t1, STEP_MS), lg.build_grid(tc, tst, t0, t1, STEP_MS)
    if rg is None or tg is None:
        return None
    prints = lg.load_trades(con, lg.campaign_files(lakehouse, "public_trades", tgt_v, camp), tgt_m, t0, t1)
    return {
        "ref": rg,
        "tgt": tg,
        "prints": prints,
        "cadence": {"ref": rg.cadence, "tgt": tg.cadence},
        "span_ms": t1 - t0,
        "continuous_10h": (t1 - t0) >= WARMUP_MS + DECISION_MS
        and max(rst["max_gap_ms"], tst["max_gap_ms"]) <= 60_000,
        "max_gap_ms": {"ref": rst["max_gap_ms"], "tgt": tst["max_gap_ms"]},
    }


def run_variant(data, pair, theta, hold, tau_s, extra_delay_ms=0, with_prints=False, exit_on_revert=False):
    ref_v, _, tgt_v, _, domain, _ = pair
    delay_steps = max(1, math.ceil((TAKER_DELAY_MS[tgt_v] + extra_delay_ms) / STEP_MS))
    return simulate_lead_lag(
        data["ref"].mid(),
        data["tgt"].bid,
        data["tgt"].ask,
        data["ref"].age_steps,
        data["tgt"].age_steps,
        theta_bps=theta,
        hold_steps=hold,
        alpha=1.0 - math.exp(-STEP_MS / (tau_s * 1000.0)),
        delay_steps=delay_steps,
        fee_bps=TAKER_FEE_BPS[(tgt_v, domain)],
        warmup_steps=WARMUP_MS // STEP_MS,
        decision_steps=DECISION_MS // STEP_MS,
        max_age_steps=MAX_AGE_MS // STEP_MS,
        prints=data.get("prints") if with_prints else None,
        t0_ms=data["ref"].t0_ms,
        exit_on_revert=exit_on_revert,
    )


def select_params(grid_results: list[dict[str, Any]]) -> dict[str, Any] | None:
    eligible = [r for r in grid_results if r["trades"] >= MIN_TRADES and r["net_bps_mean"] > 0]
    if not eligible:
        return None
    return max(eligible, key=lambda r: r["net_bps_total"])


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--lakehouse", type=Path, default=Path.home() / "aleph-tx-lakehouse")
    ap.add_argument("--output", type=Path, required=True)
    ap.add_argument("--pairs", default="", help="comma-separated substrings to filter pair labels")
    ap.add_argument("--exit-modes", default="fixed", help="comma list of fixed,revert")
    args = ap.parse_args()
    global EXIT_MODES
    EXIT_MODES = tuple(m.strip() == "revert" for m in args.exit_modes.split(",") if m.strip())
    con = lg.connect()
    filt = [s.strip() for s in args.pairs.split(",") if s.strip()]
    pairs = [p for p in PAIRS if not filt or any(f in p[5] for f in filt)]
    out: dict[str, Any] = {
        "schema": "track01_lead_lag_taker_v1",
        "protocol": "select on w19 (selection_dev); frozen replay on w24 (holdout) and w23 (holdout_2)",
        "grid": {"theta_bps": THETAS_BPS, "hold_steps_100ms": HOLD_STEPS, "tau_s": TAUS_S},
        "variants_per_pair": len(THETAS_BPS) * len(HOLD_STEPS) * len(TAUS_S) * len(EXIT_MODES),
        "exit_modes": ["revert" if m else "fixed" for m in EXIT_MODES],
        "taker_delay_ms": TAKER_DELAY_MS,
        "robustness": "print-verified repricing; +250 ms and +1000 ms extra taker delay",
        "taker_fee_bps": {f"{k[0]}:{k[1]}": v for k, v in TAKER_FEE_BPS.items()},
        "pairs": [],
    }
    for pair in pairs:
        rec: dict[str, Any] = {"label": pair[5], "ref": f"{pair[0]}:{pair[1]}", "tgt": f"{pair[2]}:{pair[3]}"}
        sel = load_pair(con, args.lakehouse, "w19", pair)
        if sel is None:
            rec["status"] = "missing_selection_data"
            out["pairs"].append(rec)
            continue
        rec["w19_continuous_10h"] = sel["continuous_10h"]
        rec["w19_span_h"] = round(sel["span_ms"] / 3_600_000, 3)
        rec["w19_max_gap_ms"] = sel["max_gap_ms"]
        rec["w19_cadence"] = sel["cadence"]
        grid = []
        for rev in EXIT_MODES:
            for tau in TAUS_S:
                for hold in HOLD_STEPS:
                    for theta in THETAS_BPS:
                        r = run_variant(sel, pair, theta, hold, tau, exit_on_revert=rev)
                        r.update({"theta_bps": theta, "hold_steps": hold, "tau_s": tau, "exit_on_revert": rev})
                        grid.append(r)
        rec["w19_ic_tau30_h10"] = information_coefficient(
            sel["ref"].mid(), sel["tgt"].bid, sel["tgt"].ask,
            1.0 - math.exp(-STEP_MS / 30_000.0),
            max(1, math.ceil(TAKER_DELAY_MS[pair[2]] / STEP_MS)), 10, WARMUP_MS // STEP_MS,
        )
        rec["w19_positive_variants"] = sum(1 for r in grid if r["trades"] >= MIN_TRADES and r["net_bps_mean"] > 0)
        chosen = select_params(grid)
        rec["w19_best"] = chosen
        if chosen is not None:
            rec["w19_best_print_check"] = run_variant(
                sel, pair, chosen["theta_bps"], chosen["hold_steps"], chosen["tau_s"], with_prints=True,
                exit_on_revert=bool(chosen.get("exit_on_revert", False)),
            )
        del sel
        if chosen is None:
            rec["status"] = "no_go_selection"
            out["pairs"].append(rec)
            print(json.dumps({"pair": pair[5], "status": "no_go"}), flush=True)
            continue
        frozen = {k: chosen[k] for k in ("theta_bps", "hold_steps", "tau_s")}
        frozen["exit_on_revert"] = bool(chosen.get("exit_on_revert", False))
        rec["frozen_params"] = frozen
        for w in ("w24", "w23"):
            data = load_pair(con, args.lakehouse, w, pair)
            if data is None:
                rec[w] = {"status": "missing"}
                continue
            r = run_variant(data, pair, frozen["theta_bps"], frozen["hold_steps"], frozen["tau_s"], with_prints=True,
                            exit_on_revert=frozen["exit_on_revert"])
            r["continuous_10h"] = data["continuous_10h"]
            r["cadence"] = data["cadence"]
            r["span_h"] = round(data["span_ms"] / 3_600_000, 3)
            r["max_gap_ms"] = data["max_gap_ms"]
            for extra in (250, 1000):
                r[f"delay_plus_{extra}ms"] = run_variant(
                    data, pair, frozen["theta_bps"], frozen["hold_steps"], frozen["tau_s"],
                    extra_delay_ms=extra, with_prints=True, exit_on_revert=frozen["exit_on_revert"],
                )
            rec[w] = r
        rec["status"] = "evaluated"
        out["pairs"].append(rec)
        print(json.dumps({"pair": pair[5], "frozen": frozen, "w19": chosen.get("net_bps_mean"),
                          "w24": rec.get("w24", {}).get("net_bps_mean"),
                          "w23": rec.get("w23", {}).get("net_bps_mean")}), flush=True)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(out, indent=2, allow_nan=False) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    sys.exit(main())
