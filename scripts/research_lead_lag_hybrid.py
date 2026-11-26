#!/usr/bin/env python3
"""Track 01 (`01_quant_profit`) upgrade: hybrid taker-entry + passive reduce_only exit
and L1-depth-capped multi-asset USD portfolio walk-forward (read-only on Aleph).

Motivated by `track01_walkforward_20261126.json` / `track01_lead_lag_v3_20261126.json`:
Lighter Core and Lighter RH charge 0/0 Standard fees and impose 0 ms speed bump on
maker limit placement/cancellation (vs 200-300 ms on takers). After a delayed taker
entry on the lead-lag signal, resting a `reduce_only` passive limit order at the exit
touch captures half-to-full spread whenever public prints cross our price before the
hold timeout, falling back to a taker exit at timeout if unfilled.

Strict causal rules:
* Decision uses only receipt-causal 100 ms grids (`lakehouse_grid.py`).
* Entry executes at `e = i + delay_steps` using `tgt.ask[e]` / `tgt.bid[e]` (and is
  additionally checked against real public prints in `[t_e, t_e + 1000ms]`).
* Passive exit starts at `t_m = t_entry + 40 ms` (maker placement latency), filled ONLY
  when real public prints strictly cross our active limit price (`px > limit_ask` for
  Long exit, `px < limit_bid` for Short exit) inside the print horizon before
  `t_timeout = t_entry + hold_ms`, capped by printed notional. If no print crosses,
  exits via taker IOC at `e_out = e + hold_steps` (or the first fresh quote
  `x_ >= e + hold_steps` when stale; `tgt.bid`/`tgt.ask` and print check).
* L1 depth capacity: trade USD notional is `min(clip_usd, 0.5 * entry_l1_usd)`.
* Protocol: compare exit styles (`taker_only`, `passive_static_touch`,
  `passive_trail_touch`) on W19 (`selection_dev`) only, freeze winning style, then
  walk forward unchanged over W01..W24 (excluding W19 selection; optional post-hoc
  W25 reported separately via `--include-w25`).
"""

from __future__ import annotations

import argparse
import json
import math
import statistics
import sys
from pathlib import Path
from typing import Any

SCRIPT_DIR = Path(__file__).resolve().parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

import lakehouse_grid as lg  # noqa: E402
import research_lead_lag_quant as ll  # noqa: E402
import walkforward_lead_lag as wf  # noqa: E402

STEP_MS = ll.STEP_MS
WARMUP_MS = ll.WARMUP_MS
DECISION_MS = ll.DECISION_MS
MAX_AGE_STEPS = ll.MAX_AGE_MS // STEP_MS
MAKER_PLACEMENT_MS = 40
CLIPS_USD = (500.0, 1_000.0, 2_000.0)
EXIT_MODES = ("taker_only", "passive_static_touch", "passive_trail_touch")

# W19-frozen entry arms (selected strictly on W19 in track01_lead_lag_20261126.json / v3).
FROZEN_ARMS: list[dict[str, Any]] = [
    {
        "pair": ("binance", "ETHUSDT", "lighter_robinhood", "0", "crypto", "lighter_rh ETH <- binance"),
        "theta_bps": 6.0, "hold_steps": 100, "tau_s": 120.0,
    },
    {
        "pair": ("binance", "SOLUSDT", "lighter_robinhood", "3", "crypto", "lighter_rh SOL <- binance"),
        "theta_bps": 8.0, "hold_steps": 100, "tau_s": 120.0,
    },
    {
        "pair": ("binance", "BTCUSDT", "lighter_robinhood", "1", "crypto", "lighter_rh BTC <- binance"),
        "theta_bps": 6.0, "hold_steps": 100, "tau_s": 120.0,
    },
    {
        "pair": ("binance", "ETHUSDT", "lighter", "0", "crypto", "lighter ETH <- binance"),
        "theta_bps": 6.0, "hold_steps": 100, "tau_s": 2.0,
    },
    {
        "pair": ("binance", "SOLUSDT", "lighter", "2", "crypto", "lighter SOL <- binance"),
        "theta_bps": 4.0, "hold_steps": 30, "tau_s": 120.0,
    },
    {
        "pair": ("binance", "BTCUSDT", "lighter", "1", "crypto", "lighter BTC <- binance"),
        "theta_bps": 2.0, "hold_steps": 100, "tau_s": 5.0,
    },
]


def simulate_hybrid(
    ref_mid,
    tgt_bid,
    tgt_ask,
    tgt_bid_sz,
    tgt_ask_sz,
    ref_age,
    tgt_age,
    prints: dict[str, Any] | None,
    *,
    t0_ms: int,
    theta_bps: float,
    hold_steps: int,
    alpha: float,
    delay_steps: int,
    taker_fee_bps: float,
    maker_fee_bps: float = 0.0,
    exit_mode: str = "passive_static_touch",
    warmup_steps: int = WARMUP_MS // STEP_MS,
    decision_steps: int = DECISION_MS // STEP_MS,
    max_age_steps: int = MAX_AGE_STEPS,
    maker_delay_ms: int = MAKER_PLACEMENT_MS,
) -> dict[str, Any]:
    """Causal hybrid lead-lag simulation with real public-print passive exit fills and L1 capacity."""
    import numpy as np

    tgt_mid = 0.5 * (tgt_bid + tgt_ask)
    with np.errstate(invalid="ignore", divide="ignore"):
        x = np.log(ref_mid) - np.log(tgt_mid)
    dev = (x - lg.ema_causal(x, alpha)) * 1e4
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

    has_prints = prints is not None and len(prints["ts"]) > 0
    if has_prints:
        p_ts, p_sg, p_px = prints["ts"], prints["sgn"], prints["px"]
    else:
        p_ts = p_sg = p_px = None

    def mean_print_px(t_start: int, window_ms: int, sgn: int):
        if p_ts is None:
            return None
        lo = int(np.searchsorted(p_ts, t_start, side="left"))
        hi = int(np.searchsorted(p_ts, t_start + window_ms, side="left"))
        if hi <= lo:
            return None
        m = p_sg[lo:hi] == sgn
        return float(p_px[lo:hi][m].mean()) if m.any() else None

    nets: list[float] = []
    pv_nets: list[float] = []
    entry_l1_usd: list[float] = []
    passive_fills = 0
    stale_entry_skips = 0
    deferred_exits = 0
    unsettled_exits = 0
    nxt = -1

    for i in cand:
        if i <= nxt:
            continue
        side = 1.0 if dev[i] > 0 else -1.0
        e = int(i + delay_steps)
        if tgt_age[e] > max_age_steps:
            stale_entry_skips += 1
            continue
        entry_px = float(tgt_ask[e] if side > 0 else tgt_bid[e])
        entry_sz = float(tgt_ask_sz[e] if side > 0 else tgt_bid_sz[e])
        if not (math.isfinite(entry_px) and entry_px > 0):
            continue
        l1_usd = entry_px * entry_sz if entry_sz > 0 and math.isfinite(entry_sz) else 0.0

        t_entry_wall = t0_ms + (e + 1) * STEP_MS
        t_hold_end = t_entry_wall + hold_steps * STEP_MS

        # Optional print-verified entry price (1000ms window at entry fill wall time).
        sg = 1 if side > 0 else -1
        pe = mean_print_px(t_entry_wall, 1_000, sg)

        filled_passive = False
        t_p = t_entry_wall
        exit_px = 0.0
        exit_fee = taker_fee_bps
        nx_step = e + hold_steps

        if exit_mode != "taker_only" and p_ts is not None:
            t_active = t_entry_wall + maker_delay_ms
            lo = int(np.searchsorted(p_ts, t_active, side="left"))
            hi = int(np.searchsorted(p_ts, t_hold_end, side="left"))
            static_limit = float(tgt_ask[e] if side > 0 else tgt_bid[e])
            for idx in range(lo, hi):
                if p_sg[idx] != sg:
                    continue
                t_p = int(p_ts[idx])
                if exit_mode == "passive_trail_touch":
                    k_book = int((t_p - maker_delay_ms - t0_ms) // STEP_MS) - 1
                    k_book = min(max(k_book, e), n - 1)
                    if tgt_age[k_book] > max_age_steps:
                        continue
                    lim = float(tgt_ask[k_book] if side > 0 else tgt_bid[k_book])
                else:
                    lim = static_limit
                if not (lim > 0):
                    continue
                px_p = float(p_px[idx])
                # Strict trade-through: ask limit filled when buyer hits strictly above ask;
                # bid limit filled when seller hits strictly below bid.
                if (side > 0 and px_p > lim) or (side < 0 and px_p < lim):
                    filled_passive = True
                    exit_px = lim
                    exit_fee = maker_fee_bps
                    nx_step = max(e, int((t_p - t0_ms) // STEP_MS))
                    break

        if not filled_passive:
            x_orig = e + hold_steps
            x_ = x_orig
            while x_ < n:
                cand_px = float(tgt_bid[x_] if side > 0 else tgt_ask[x_])
                if tgt_age[x_] <= max_age_steps and math.isfinite(cand_px) and cand_px > 0:
                    break
                x_ += 1
            if x_ >= n:
                unsettled_exits += 1
                break
            if x_ > x_orig:
                deferred_exits += 1
            exit_px = float(tgt_bid[x_] if side > 0 else tgt_ask[x_])
            exit_fee = taker_fee_bps
            nx_step = x_
            px_ver = mean_print_px(t0_ms + (x_ + 1) * STEP_MS, 1_000, -sg)
        else:
            # The passive-exit witness and later prints cannot price the earlier entry.
            pe = mean_print_px(t_entry_wall, min(1_000, max(0, t_p - t_entry_wall)), sg)
            px_ver = exit_px  # Already confirmed by strict trade-through print!

        net = side * (exit_px / entry_px - 1.0) * 1e4 - taker_fee_bps - exit_fee
        nets.append(net)
        entry_l1_usd.append(l1_usd)
        if filled_passive:
            passive_fills += 1
        if pe and px_ver and pe > 0 and px_ver > 0:
            pv_nets.append(side * (px_ver / pe - 1.0) * 1e4 - taker_fee_bps - exit_fee)
        nxt = nx_step

    k = len(nets)
    execution = {
        "stale_entry_skips": stale_entry_skips,
        "deferred_exits": deferred_exits,
        "unsettled_exits": unsettled_exits,
    }
    if k == 0:
        return {**execution, "trades": 0, "net_bps_total": 0.0, "net_bps_mean": 0.0, "passive_fills": 0,
                "clip_pnl_usd": {str(int(c)): 0.0 for c in CLIPS_USD},
                "pv_clip_pnl_usd": {str(int(c)): 0.0 for c in CLIPS_USD}}

    clip_usd_out = {}
    for clip in CLIPS_USD:
        tot = sum(min(clip, 0.5 * d) * b * 1e-4 for b, d in zip(nets, entry_l1_usd))
        clip_usd_out[str(int(clip))] = round(tot, 4)

    arr = np.asarray(nets)
    sd = float(arr.std(ddof=1)) if k > 1 else 0.0
    pv_arr = np.asarray(pv_nets) if pv_nets else np.asarray([])
    return {
        **execution,
        "trades": k,
        "passive_fills": passive_fills,
        "passive_fill_rate": round(passive_fills / k, 3),
        "net_bps_total": round(float(arr.sum()), 3),
        "net_bps_mean": round(float(arr.mean()), 4),
        "hit_rate": round(float((arr > 0).mean()), 3),
        "t_stat": round(float(arr.mean() / (sd / math.sqrt(k))) if sd > 0 else 0.0, 2),
        "median_l1_usd": round(float(statistics.median(entry_l1_usd)), 1),
        "print_verified_trades": int(len(pv_arr)),
        "print_verified_net_bps_mean": round(float(pv_arr.mean()), 4) if len(pv_arr) else None,
        "clip_pnl_usd": clip_usd_out,
    }


def run_arm_on_data(arm: dict[str, Any], data: dict[str, Any], exit_mode: str) -> dict[str, Any]:
    pair = arm["pair"]
    tgt_v, domain = pair[2], pair[4]
    delay_steps = max(1, math.ceil(ll.TAKER_DELAY_MS[tgt_v] / STEP_MS))
    alpha = 1.0 - math.exp(-STEP_MS / (arm["tau_s"] * 1000.0))
    return simulate_hybrid(
        data["ref"].mid(),
        data["tgt"].bid,
        data["tgt"].ask,
        data["tgt"].bid_sz,
        data["tgt"].ask_sz,
        data["ref"].age_steps,
        data["tgt"].age_steps,
        data.get("prints"),
        t0_ms=data["ref"].t0_ms,
        theta_bps=arm["theta_bps"],
        hold_steps=arm["hold_steps"],
        alpha=alpha,
        delay_steps=delay_steps,
        taker_fee_bps=ll.TAKER_FEE_BPS[(tgt_v, domain)],
        exit_mode=exit_mode,
    )


def summarize_usd_series(values: list[float]) -> dict[str, Any]:
    n = len(values)
    if n == 0:
        return {"windows": 0}
    tot = sum(values)
    mean = tot / n
    sd = math.sqrt(sum((v - mean) ** 2 for v in values) / (n - 1)) if n > 1 else 0.0
    cum = 0.0
    peak = 0.0
    mdd = 0.0
    for v in values:
        cum += v
        peak = max(peak, cum)
        mdd = max(mdd, peak - cum)
    return {
        "windows": n,
        "positive_windows": sum(v > 0 for v in values),
        "total_usd": round(tot, 2),
        "mean_usd_per_window": round(mean, 2),
        "t_across_windows": round(mean / (sd / math.sqrt(n)), 2) if sd > 0 else None,
        "max_drawdown_usd": round(mdd, 2),
        # Explicitly marked research estimate (non-overlapping 10.25h windows, no capital compounding).
        "annualized_sharpe_estimate": round((mean / sd) * math.sqrt(365.0 * 24.0 / 10.25), 2) if sd > 0 else None,
    }


CORE_ETH_SOL_LABELS = ("lighter ETH <- binance", "lighter SOL <- binance")


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--lakehouse", type=Path, default=Path.home() / "aleph-tx-lakehouse")
    ap.add_argument("--output", type=Path, required=True)
    ap.add_argument("--include-w25", action="store_true", help="include post-hoc W25 separately from W01..W24")
    args = ap.parse_args()
    con = lg.connect()

    # Step 1: Compare exit modes strictly on W19 (selection_dev) for each frozen arm.
    w19_eval: dict[str, dict[str, Any]] = {}
    selected_arms: list[dict[str, Any]] = []
    for arm in FROZEN_ARMS:
        lbl = arm["pair"][5]
        data = ll.load_pair(con, args.lakehouse, "w19", arm["pair"])
        if data is None:
            continue
        modes = {m: run_arm_on_data(arm, data, m) for m in EXIT_MODES}
        w19_eval[lbl] = modes
        # Pick the best positive W19 exit mode with >= 20 trades at $1k cap.
        best_m = max(
            EXIT_MODES,
            key=lambda m: modes[m]["clip_pnl_usd"]["1000"] if modes[m]["trades"] >= 20 else -1e9,
        )
        if modes[best_m]["trades"] >= 20 and modes[best_m]["clip_pnl_usd"]["1000"] > 0:
            selected_arms.append({**arm, "selected_exit_mode": best_m, "w19": modes[best_m]})
        del data

    # Step 2: Walk forward unchanged across all discovered Tokyo crypto windows (ex W19 & W25+ unless --include-w25).
    max_w = 25 if args.include_w25 else 24
    windows = {w: c for w, c in wf.discover_crypto_windows(args.lakehouse).items() if w != 19 and w <= max_w}
    per_arm_wf: dict[str, list[dict[str, Any]]] = {a["pair"][5]: [] for a in selected_arms}
    per_win_portfolio: dict[int, dict[str, float]] = {}
    core_win_portfolio: dict[int, dict[str, float]] = {}

    for w, cid in sorted(windows.items()):
        lg.WINDOWS[f"w{w:02d}"] = {"crypto": cid, "rwa": "", "split": ""}
        win_usd = {str(int(c)): 0.0 for c in CLIPS_USD}
        core_usd = {str(int(c)): 0.0 for c in CLIPS_USD}
        for arm in selected_arms:
            lbl = arm["pair"][5]
            data = ll.load_pair(con, args.lakehouse, f"w{w:02d}", arm["pair"])
            if data is None:
                continue
            r = run_arm_on_data(arm, data, arm["selected_exit_mode"])
            r_taker = run_arm_on_data(arm, data, "taker_only")
            per_arm_wf[lbl].append({
                "window": w,
                "trades": r["trades"],
                "stale_entry_skips": r.get("stale_entry_skips", 0),
                "deferred_exits": r.get("deferred_exits", 0),
                "unsettled_exits": r.get("unsettled_exits", 0),
                "passive_fill_rate": r.get("passive_fill_rate", 0.0),
                "net_bps_mean": r["net_bps_mean"],
                "print_verified_net_bps_mean": r.get("print_verified_net_bps_mean"),
                "clip_1000_usd": r["clip_pnl_usd"]["1000"],
                "taker_only_1000_usd": r_taker["clip_pnl_usd"]["1000"],
                "median_l1_usd": r.get("median_l1_usd", 0.0),
            })
            for c in CLIPS_USD:
                k_c = str(int(c))
                win_usd[k_c] += r["clip_pnl_usd"][k_c]
                if lbl in CORE_ETH_SOL_LABELS:
                    core_usd[k_c] += r["clip_pnl_usd"][k_c]
            del data
        per_win_portfolio[w] = win_usd
        core_win_portfolio[w] = core_usd

    out: dict[str, Any] = {
        "schema": "track01_hybrid_portfolio_v1",
        "protocol": "W19 selects exit_mode per arm; frozen replay across W01..W24 (ex W19; optional post-hoc W25 separate)",
        "print_price_method": "window_1000ms_mean",
        "include_w25": bool(args.include_w25),
        "exit_modes_tested": list(EXIT_MODES),
        "w19_selection": w19_eval,
        "frozen_portfolio_arms": [
            {"label": a["pair"][5], "exit_mode": a["selected_exit_mode"], "theta_bps": a["theta_bps"],
             "hold_steps": a["hold_steps"], "tau_s": a["tau_s"]}
            for a in selected_arms
        ],
        "per_arm_walkforward": {},
        "portfolio_by_clip_usd": {},
        "forward_w20_w24_by_clip_usd": {},
        "core_eth_sol_basket_usd": {},
        "w25_untouched": None,
    }
    for lbl, rows in per_arm_wf.items():
        rows_24 = [x for x in rows if x["window"] <= 24]
        usd_vals = [x["clip_1000_usd"] for x in rows_24]
        taker_vals = [x["taker_only_1000_usd"] for x in rows_24]
        w25_arm = next((x for x in rows if x["window"] == 25), None)
        out["per_arm_walkforward"][lbl] = {
            "summary_1000_usd": summarize_usd_series(usd_vals),
            "taker_only_1000_usd": summarize_usd_series(taker_vals),
            "mean_passive_fill_rate": round(statistics.mean(x["passive_fill_rate"] for x in rows_24), 3) if rows_24 else 0.0,
            "total_trades": sum(x["trades"] for x in rows_24),
            "median_l1_usd": round(statistics.median(x["median_l1_usd"] for x in rows_24 if x["trades"] > 0), 1)
            if any(x["trades"] > 0 for x in rows_24) else 0.0,
            "w25_untouched": w25_arm,
        }
    for c in CLIPS_USD:
        k_c = str(int(c))
        all_vals = [per_win_portfolio[w][k_c] for w in sorted(per_win_portfolio) if w <= 24]
        fwd_vals = [per_win_portfolio[w][k_c] for w in sorted(per_win_portfolio) if 20 <= w <= 24]
        out["portfolio_by_clip_usd"][k_c] = summarize_usd_series(all_vals)
        out["forward_w20_w24_by_clip_usd"][k_c] = summarize_usd_series(fwd_vals)

    out["core_eth_sol_basket_usd"] = {
        "arms": [a["pair"][5] for a in selected_arms if a["pair"][5] in CORE_ETH_SOL_LABELS],
        "w01_w24_by_clip_usd": {
            str(int(c)): summarize_usd_series([core_win_portfolio[w][str(int(c))] for w in sorted(core_win_portfolio) if w <= 24])
            for c in CLIPS_USD
        },
        "forward_w20_w24_by_clip_usd": {
            str(int(c)): summarize_usd_series([core_win_portfolio[w][str(int(c))] for w in sorted(core_win_portfolio) if 20 <= w <= 24])
            for c in CLIPS_USD
        },
        "w25_untouched_by_clip_usd": {
            str(int(c)): round(core_win_portfolio[25][str(int(c))], 2)
            for c in CLIPS_USD
        } if 25 in core_win_portfolio else None,
    }
    if 25 in per_win_portfolio:
        out["w25_untouched"] = {
            "window": 25,
            "note": "posthoc_untouched_diagnostic_not_mixed_into_w01_w24",
            "by_clip_usd": {str(int(c)): round(per_win_portfolio[25][str(int(c))], 2) for c in CLIPS_USD},
            "core_eth_sol_basket_usd": out["core_eth_sol_basket_usd"]["w25_untouched_by_clip_usd"],
            "per_arm": {lbl: next((x for x in rows if x["window"] == 25), None) for lbl, rows in per_arm_wf.items()},
        }

    out["per_window_1000_usd"] = {str(w): round(per_win_portfolio[w]["1000"], 2) for w in sorted(per_win_portfolio) if w <= 24}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(out, indent=2, allow_nan=False) + "\n", encoding="utf-8")
    print(json.dumps({
        "arms": len(selected_arms),
        "portfolio_1000": out["portfolio_by_clip_usd"]["1000"],
        "forward_1000": out["forward_w20_w24_by_clip_usd"]["1000"],
    }), flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
