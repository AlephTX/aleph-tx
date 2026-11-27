#!/usr/bin/env python3
"""Track 01 P0 Exit Depth-Conservation Rescore & Controlled Single-Gate Evaluation (`W01–W27`).

100% pure-stdlib at module top level (`duckdb` is imported lazily only for lakehouse loading,
runnable under `python3 -S`).

Implements two linked evaluations over `W01–W27` (excluding `w08` and `-w28`):
  - Part A: Exact before-vs-after P0 exit visible-book depth-conservation rescore on frozen
    `OverreactionReversion` (`lb15m_r60_rf0_f0`) and `OracleDiscoveryMomentum` (`d25_h30m_f1`).
  - Part B: Roadmap `§4` Oasis-style single-factor controlled experiment comparing
    `baseline` vs `single_gate` (single causal gate frozen purely on `selection_dev`) vs `flat`
    over the identical candidate event set, visible timestamps, capital, and exit rules, with
    independent L1 depth budget & inventory state per arm, conditional `1m/5m/15m/60m` markout
    attribution, Politis-Romano stationary block bootstrap 95% CI on `delta(gate - baseline)`,
    raw `N_trials` & `N_eff` DSR, and CSCV PBO.
"""

from __future__ import annotations

import argparse
import bisect
import gc
import hashlib
import importlib.util
import json
import math
import os
import random
import sys
from collections import defaultdict
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Callable, Sequence

MARKOUT_HORIZONS_MS: tuple[tuple[str, int], ...] = (
    ("1m", 60_000),
    ("5m", 300_000),
    ("15m", 900_000),
    ("60m", 3_600_000),
)

FROZEN_TIGHT_SPREAD_MAX_BPS: float = 5.0
FROZEN_PARTICIPATION_MIN_L1_USD: float = 1_000.0
US_RTH_START_UTC_MIN: int = 14 * 60 + 30
US_RTH_END_UTC_MIN: int = 21 * 60
BOOTSTRAP_DEFAULT_ITERS: int = 5_000
BOOTSTRAP_DEFAULT_BLOCK_LEN: float = 3.0
BOOTSTRAP_DEFAULT_SEED: int = 20261127
OOS_DELTA_COST_THRESHOLD_USD: float = 10.0


def _load_local_module(mod_name: str, filename: str) -> Any:
    """Import a sibling pure-stdlib module from the same directory."""
    if mod_name in sys.modules:
        return sys.modules[mod_name]
    path = Path(__file__).resolve().parent / filename
    spec = importlib.util.spec_from_file_location(mod_name, path)
    if spec is None or spec.loader is None:
        raise ImportError(f"Unable to load {path}")
    mod = importlib.util.module_from_spec(spec)
    sys.modules[mod_name] = mod
    spec.loader.exec_module(mod)
    return mod


def _sha256_file(path: Path | None) -> str | None:
    if path is None or not path.is_file():
        return None
    h = hashlib.sha256()
    with path.open("rb") as fh:
        while True:
            chunk = fh.read(65536)
            if not chunk:
                break
            h.update(chunk)
    return h.hexdigest()


def _is_us_rth_utc(ts_ms: int) -> bool:
    dt = datetime.fromtimestamp(ts_ms / 1000.0, tz=timezone.utc)
    hm = dt.hour * 60 + dt.minute
    return US_RTH_START_UTC_MIN <= hm < US_RTH_END_UTC_MIN


def _forward_mid_markouts(
    sig: Any,
    t_ref_ms: int,
    side: int,
    px_entry: float,
    r1: Any,
) -> dict[str, dict[str, float | None]]:
    """Compute causal forward mid-price markouts at 1m/5m/15m/60m without looking past `sig.t1_ms`."""
    q0 = r1._latest_at(sig.bbo, sig.bbo_ts, t_ref_ms)
    mid0 = 0.5 * (q0[1] + q0[3]) if (q0 is not None and q0[1] > 0.0 and q0[3] > q0[1]) else 0.0
    mid_out: dict[str, float | None] = {}
    px_out: dict[str, float | None] = {}
    for lbl, h_ms in MARKOUT_HORIZONS_MS:
        t_h = t_ref_ms + h_ms
        if t_h > sig.t1_ms or mid0 <= 0.0:
            mid_out[lbl] = None
            px_out[lbl] = None
            continue
        qh = r1._latest_at(sig.bbo, sig.bbo_ts, t_h)
        if (
            qh is None
            or (t_h - qh[0]) > r1.DEFAULT_MAX_QUOTE_AGE_MS
            or qh[1] <= 0.0
            or qh[3] <= qh[1]
        ):
            mid_out[lbl] = None
            px_out[lbl] = None
            continue
        mid_h = 0.5 * (qh[1] + qh[3])
        mid_out[lbl] = side * ((mid_h / mid0) - 1.0) * r1.BPS_SCALE
        px_out[lbl] = (
            side * ((mid_h / px_entry) - 1.0) * r1.BPS_SCALE if px_entry > 0.0 else None
        )
    return {"mid_bps": mid_out, "px_in_bps": px_out}


def _summarize_markouts(records: Sequence[dict[str, Any]]) -> dict[str, Any]:
    """Summarize conditional `1m/5m/15m/60m` markouts & realized bps across a record cohort."""
    n = len(records)
    mid_summary: dict[str, Any] = {}
    px_summary: dict[str, Any] = {}
    for lbl, _ in MARKOUT_HORIZONS_MS:
        m_vals = [float(r["mid_bps"][lbl]) for r in records if r["mid_bps"].get(lbl) is not None]
        p_vals = [float(r["px_in_bps"][lbl]) for r in records if r["px_in_bps"].get(lbl) is not None]
        mid_summary[lbl] = {
            "n": len(m_vals),
            "mean_bps": round(sum(m_vals) / len(m_vals), 4) if m_vals else None,
        }
        px_summary[lbl] = {
            "n": len(p_vals),
            "mean_bps": round(sum(p_vals) / len(p_vals), 4) if p_vals else None,
        }
    realized_bps_vals = [float(r["realized_bps"]) for r in records if r.get("realized_bps") is not None]
    return {
        "count": n,
        "mid_markout_bps": mid_summary,
        "px_in_markout_bps": px_summary,
        "mean_realized_bps": round(sum(realized_bps_vals) / len(realized_bps_vals), 4)
        if realized_bps_vals
        else None,
    }


def stationary_block_bootstrap_ci(
    series: Sequence[float],
    *,
    mean_block_len: float = BOOTSTRAP_DEFAULT_BLOCK_LEN,
    n_boot: int = BOOTSTRAP_DEFAULT_ITERS,
    seed: int = BOOTSTRAP_DEFAULT_SEED,
) -> dict[str, Any]:
    """Politis & Romano (1994) stationary block bootstrap 95% CI on mean and total delta."""
    vals = [float(x) for x in series]
    t = len(vals)
    if t == 0:
        return {
            "n_windows": 0,
            "mean_block_len": mean_block_len,
            "n_boot": n_boot,
            "seed": seed,
            "observed_total_usd": 0.0,
            "observed_mean_per_window_usd": 0.0,
            "total_ci95_usd": [0.0, 0.0],
            "mean_per_window_ci95_usd": [0.0, 0.0],
            "p_boot_gt_zero": 0.0,
        }

    obs_total = sum(vals)
    obs_mean = obs_total / t
    if t == 1 or all(v == vals[0] for v in vals):
        return {
            "n_windows": t,
            "mean_block_len": mean_block_len,
            "n_boot": n_boot,
            "seed": seed,
            "observed_total_usd": round(obs_total, 4),
            "observed_mean_per_window_usd": round(obs_mean, 4),
            "total_ci95_usd": [round(obs_total, 4), round(obs_total, 4)],
            "mean_per_window_ci95_usd": [round(obs_mean, 4), round(obs_mean, 4)],
            "p_boot_gt_zero": 1.0 if obs_total > 0.0 else 0.0,
        }

    p_restart = min(1.0, max(1e-6, 1.0 / max(1.0, mean_block_len)))
    rng = random.Random(seed)
    boot_totals: list[float] = []
    gt_zero = 0
    for _ in range(n_boot):
        idx = rng.randrange(t)
        acc = 0.0
        for step in range(t):
            if step > 0:
                if rng.random() < p_restart:
                    idx = rng.randrange(t)
                else:
                    idx = (idx + 1) % t
            acc += vals[idx]
        boot_totals.append(acc)
        if acc > 0.0:
            gt_zero += 1

    boot_totals.sort()
    lo_idx = min(n_boot - 1, max(0, int(math.floor(0.025 * (n_boot - 1)))))
    hi_idx = min(n_boot - 1, max(0, int(math.ceil(0.975 * (n_boot - 1)))))
    ci_lo = boot_totals[lo_idx]
    ci_hi = boot_totals[hi_idx]
    return {
        "n_windows": t,
        "mean_block_len": mean_block_len,
        "n_boot": n_boot,
        "seed": seed,
        "observed_total_usd": round(obs_total, 4),
        "observed_mean_per_window_usd": round(obs_mean, 4),
        "total_ci95_usd": [round(ci_lo, 4), round(ci_hi, 4)],
        "mean_per_window_ci95_usd": [round(ci_lo / t, 4), round(ci_hi / t, 4)],
        "p_boot_gt_zero": round(gt_zero / n_boot, 4),
    }


def _aggregate_with_tail(
    rows: Sequence[dict[str, Any]],
    *,
    venue: str,
    r1: Any,
    ordered_windows: Sequence[str] | None = None,
) -> dict[str, Any]:
    """Extend `aggregate_rows` with explicit `worst_window` and ordered per-window series."""
    base = r1.aggregate_rows(rows, venue=venue)
    prem_mk, prem_tk = (0.40, 2.80) if venue == "lighter" else (0.0, 0.0)

    win_keys = list(ordered_windows) if ordered_windows is not None else sorted({str(r["win"]) for r in rows})
    by_win_mtm: dict[str, float] = {w: 0.0 for w in win_keys}
    by_win_real: dict[str, float] = {w: 0.0 for w in win_keys}
    by_win_prem: dict[str, float] = {w: 0.0 for w in win_keys}
    by_win_p1: dict[str, float] = {w: 0.0 for w in win_keys}

    for r in rows:
        w = str(r["win"])
        by_win_mtm[w] = by_win_mtm.get(w, 0.0) + float(r["mtm_incl_residual_gross_usd"])
        by_win_real[w] = by_win_real.get(w, 0.0) + float(r["realized_only_gross_usd"])
        by_win_prem[w] = by_win_prem.get(w, 0.0) + r1.net_pnl_for_fee_bps(r, prem_mk, prem_tk, 0.0)
        by_win_p1[w] = by_win_p1.get(w, 0.0) + r1.net_pnl_for_fee_bps(r, 0.0, 0.0, 1.0)

    mtm_vals = [by_win_mtm[w] for w in win_keys]
    prem_vals = [by_win_prem[w] for w in win_keys]

    def _max_dd(vals: Sequence[float]) -> float:
        cum = peak = dd = 0.0
        for v in vals:
            cum += v
            if cum > peak:
                peak = cum
            if (peak - cum) > dd:
                dd = peak - cum
        return dd

    worst_win_name = min(win_keys, key=lambda w: by_win_mtm[w]) if win_keys else None
    worst_prem_win = min(win_keys, key=lambda w: by_win_prem[w]) if win_keys else None

    return {
        **base,
        "worst_window_0_0_name": worst_win_name,
        "worst_window_0_0_usd": round(by_win_mtm[worst_win_name], 4) if worst_win_name else 0.0,
        "worst_window_prem_name": worst_prem_win,
        "worst_window_prem_usd": round(by_win_prem[worst_prem_win], 4) if worst_prem_win else 0.0,
        "max_dd_prem_usd": round(_max_dd(prem_vals), 4),
        "per_window_mtm_0_0": {w: round(by_win_mtm[w], 4) for w in win_keys},
        "per_window_realized_0_0": {w: round(by_win_real[w], 4) for w in win_keys},
        "per_window_prem_mtm": {w: round(by_win_prem[w], 4) for w in win_keys},
        "per_window_p1bp_mtm": {w: round(by_win_p1[w], 4) for w in win_keys},
    }


def _flat_summary(ordered_windows: Sequence[str], *, venue: str, r1: Any) -> dict[str, Any]:
    return _summarize_flat(ordered_windows, venue=venue, r1=r1)


def _summarize_flat(ordered_windows: Sequence[str], *, venue: str, r1: Any) -> dict[str, Any]:
    empty = _aggregate_with_tail([], venue=venue, r1=r1, ordered_windows=ordered_windows)
    empty["unique_windows"] = len(ordered_windows)
    empty["funnel"] = {
        "candidate_events_total": 0,
        "off_cooldown_checks": 0,
        "gate_rejected": 0,
        "entry_depth_rejected": 0,
        "filled_trades": 0,
    }
    return empty


def simulate_controlled_overreaction_arm(
    sig: Any,
    r1: Any,
    *,
    gate_fn: Callable[[int, int, float, float], bool] | None = None,
    lookback_bars: int = 15,
    min_return_bps: float = 60.0,
    spread_mult: float = 2.0,
    max_return_bps: float = 500.0,
    max_entry_spread_bps: float = 20.0,
    stop_loss_bps: float = 45.0,
    cooldown_ms: int = 300_000,
    clip_usd: float = 500.0,
    collect_markouts: bool = True,
) -> dict[str, Any]:
    """Simulate frozen `OverreactionReversion` `lb15m_r60_rf0_f0` RH `0/0` with optional causal gate.

    `gate_fn(t_dec_ms, side, spread_bps, l1_dec_usd) -> bool`:
      - Evaluated ONLY at candidate events where all base trigger criteria hold and the arm is flat
        and off cooldown (`independent inventory state & L1 depth budget per arm`).
      - Returns `True` to allow entry or `False` to stay flat (remaining ready for subsequent events).
    """
    ts_ms = sig.ts_ms
    n = len(ts_ms)
    if n < 10:
        out = r1._empty_trade_result()
        out["funnel"] = {
            "candidate_events_total": 0,
            "off_cooldown_checks": 0,
            "gate_rejected": 0,
            "entry_depth_rejected": 0,
            "filled_trades": 0,
        }
        out["trade_markouts"] = []
        return out

    max_hold_ms = lookback_bars * r1.REV_BAR_MS
    t_stop_entries = sig.t1_ms - 10_000
    rdy = sig.rev_ready
    ret_arr = sig.rev_r60_bps if lookback_bars >= 60 else sig.rev_r15_bps
    spr_arr = sig.spread_bps
    bid_arr = sig.bid_px
    ask_arr = sig.ask_px

    # Count shared candidate events independent of position/cooldown for identical-set denominator
    candidate_events_total = 0
    for idx in range(n):
        t_idx = ts_ms[idx]
        if t_idx >= t_stop_entries:
            break
        if rdy[idx] == 0:
            continue
        rv = ret_arr[idx]
        if not math.isfinite(rv):
            continue
        ar = abs(rv)
        if ar < min_return_bps or ar >= max_return_bps:
            continue
        sp = spr_arr[idx]
        if sp > max_entry_spread_bps:
            continue
        if ar >= max(min_return_bps, spread_mult * sp):
            candidate_events_total += 1

    trades = long_trades = pv_trades = pv_unverifiable = 0
    depth_clipped_entries = depth_clipped_exits = residual_trades = 0
    off_cooldown_checks = gate_rejected = entry_depth_rejected = 0
    exits_by_reason: dict[str, int] = defaultdict(int)
    realized_px_pnl = realized_fnd = res_mtm_px_pnl = res_fnd = 0.0
    maker_vol = taker_vol = total_entry_notl = terminal_residual_notl = 0.0
    hold_ms_list: list[int] = []
    trade_markouts: list[dict[str, Any]] = []
    last_exit_ms = -1

    k = 0
    while k < n:
        t_dec = ts_ms[k]
        if t_dec >= t_stop_entries:
            break
        if last_exit_ms >= 0 and (t_dec - last_exit_ms) < cooldown_ms:
            k += 1
            continue
        if rdy[k] == 0:
            k += 1
            continue
        rval = ret_arr[k]
        if not math.isfinite(rval):
            k += 1
            continue
        abs_r = abs(rval)
        if abs_r < min_return_bps or abs_r >= max_return_bps:
            k += 1
            continue
        spr = spr_arr[k]
        if spr > max_entry_spread_bps:
            k += 1
            continue
        hurdle = max(min_return_bps, spread_mult * spr)
        if abs_r < hurdle:
            k += 1
            continue

        side = 1 if rval <= -hurdle else -1
        off_cooldown_checks += 1
        if gate_fn is not None:
            q_dec = r1._latest_at(sig.bbo, sig.bbo_ts, t_dec)
            l1_dec = (
                (q_dec[3] * q_dec[4] if side > 0 else q_dec[1] * q_dec[2])
                if q_dec is not None
                else 0.0
            )
            if not gate_fn(t_dec, side, spr, l1_dec):
                gate_rejected += 1
                k += 1
                continue

        ok_e, px_in, notl, t_fill_in, _, d_clip, _ = r1._execute_entry(
            sig, t_dec, t_stop_entries, side, "taker", clip_usd
        )
        if not ok_e:
            entry_depth_rejected += 1
            k += 1
            continue
        taker_vol += notl
        if d_clip:
            depth_clipped_entries += 1

        j = max(k + 1, bisect.bisect_left(ts_ms, t_fill_in))
        exit_reason = "terminal_flatten"
        j_exit = n - 1
        while j < n:
            tj = ts_ms[j]
            if tj >= t_stop_entries:
                j_exit = j
                exit_reason = "terminal_flatten"
                break
            if (tj - t_fill_in) >= max_hold_ms:
                j_exit = j
                exit_reason = "max_hold"
                break
            ex_px = bid_arr[j] if side > 0 else ask_arr[j]
            if ex_px > 0.0:
                unr_bps = (
                    ((ex_px / px_in) - 1.0) * r1.BPS_SCALE
                    if side > 0
                    else ((px_in - ex_px) / px_in) * r1.BPS_SCALE
                )
                if stop_loss_bps > 0.0 and unr_bps <= -stop_loss_bps:
                    j_exit = j
                    exit_reason = "stop_loss"
                    break
            j += 1

        (
            rpx,
            rfnd,
            rmtm,
            rfnd_res,
            mv,
            tv,
            last_fill_out,
            ex_clip,
            rem_res,
            j_cur,
        ) = r1._execute_exit(
            sig,
            j_exit,
            side,
            px_in,
            notl,
            t_fill_in,
            exit_reason,
            ("mean_reversion",),
            "taker",
            conserve_exit_depth=True,
        )
        realized_px_pnl += rpx
        realized_fnd += rfnd
        res_mtm_px_pnl += rmtm
        res_fnd += rfnd_res
        maker_vol += mv
        taker_vol += tv
        if ex_clip:
            depth_clipped_exits += 1
        if rem_res > 1e-6:
            residual_trades += 1
        terminal_residual_notl += rem_res

        total_entry_notl += notl
        trades += 1
        pv_trades += 1
        if side > 0:
            long_trades += 1
        exits_by_reason[exit_reason] += 1
        hold_ms_list.append(max(0, last_fill_out - t_fill_in))
        if collect_markouts:
            mk = _forward_mid_markouts(sig, t_dec, side, px_in, r1)
            mk["spread_bps"] = spr
            mk["realized_bps"] = ((rpx + rfnd + rmtm + rfnd_res) / notl) * r1.BPS_SCALE if notl > 0.0 else 0.0
            trade_markouts.append(mk)
        last_exit_ms = last_fill_out
        k = max(j_cur + 1, bisect.bisect_left(ts_ms, last_fill_out))

    res = r1._finalize_trade_result(
        trades=trades,
        long_trades=long_trades,
        pv_trades=pv_trades,
        pv_unverifiable=pv_unverifiable,
        realized_px_pnl=realized_px_pnl,
        realized_fnd=realized_fnd,
        res_mtm_px_pnl=res_mtm_px_pnl,
        res_fnd=res_fnd,
        maker_vol=maker_vol,
        taker_vol=taker_vol,
        total_entry_notl=total_entry_notl,
        hold_ms_list=hold_ms_list,
        depth_clipped_entries=depth_clipped_entries,
        depth_clipped_exits=depth_clipped_exits,
        residual_trades=residual_trades,
        terminal_residual_notl=terminal_residual_notl,
        exits_by_reason=dict(exits_by_reason),
    )
    res["funnel"] = {
        "candidate_events_total": candidate_events_total,
        "off_cooldown_checks": off_cooldown_checks,
        "gate_rejected": gate_rejected,
        "entry_depth_rejected": entry_depth_rejected,
        "filled_trades": trades,
    }
    res["trade_markouts"] = trade_markouts
    return res


def collect_candidate_markouts(sig: Any, r1: Any) -> tuple[list[dict[str, Any]], list[dict[str, Any]]]:
    """Collect de-clustered (`>= 60s` spacing) candidate event markouts split by `spread_bps <= 5.0`."""
    ts_ms = sig.ts_ms
    n = len(ts_ms)
    t_stop = sig.t1_ms - 10_000
    pass_mk: list[dict[str, Any]] = []
    fail_mk: list[dict[str, Any]] = []
    last_sampled_ms = -1_000_000
    for idx in range(n):
        t_dec = ts_ms[idx]
        if t_dec >= t_stop:
            break
        if sig.rev_ready[idx] == 0:
            continue
        rval = sig.rev_r15_bps[idx]
        if not math.isfinite(rval):
            continue
        abs_r = abs(rval)
        if abs_r < 60.0 or abs_r >= 500.0:
            continue
        spr = sig.spread_bps[idx]
        if spr > 20.0 or abs_r < max(60.0, 2.0 * spr):
            continue
        if (t_dec - last_sampled_ms) < 60_000:
            continue
        last_sampled_ms = t_dec
        side = 1 if rval <= -max(60.0, 2.0 * spr) else -1
        q_dec = r1._latest_at(sig.bbo, sig.bbo_ts, t_dec)
        px_ref = (q_dec[3] if side > 0 else q_dec[1]) if q_dec is not None else 0.0
        mk = _forward_mid_markouts(sig, t_dec, side, px_ref, r1)
        if spr <= FROZEN_TIGHT_SPREAD_MAX_BPS:
            pass_mk.append(mk)
        else:
            fail_mk.append(mk)
    return pass_mk, fail_mk


def _delta_dict(after_agg: dict[str, Any], before_agg: dict[str, Any]) -> dict[str, Any]:
    keys = (
        "trades",
        "realized_only_net_0_0_usd",
        "mtm_incl_residual_net_0_0_usd",
        "net_prem_realized_usd",
        "net_prem_mtm_usd",
        "terminal_residual_notional_usd",
        "residual_trades",
        "depth_clipped_exits",
        "total_vol_usd",
        "max_dd_0_0_usd",
        "worst_window_0_0_usd",
    )
    return {
        k: round(float(after_agg[k]) - float(before_agg[k]), 4)
        if not isinstance(after_agg[k], int)
        else int(after_agg[k]) - int(before_agg[k])
        for k in keys
    }


def _build_split_summary(
    *,
    rows_by_arm: dict[str, list[dict[str, Any]]],
    windows_by_split: dict[str, list[str]],
    venue: str,
    r1: Any,
    arm_names: Sequence[str],
) -> dict[str, Any]:
    split_defs = {
        **windows_by_split,
        "oos_w21_w27": sorted(windows_by_split["holdout"] + windows_by_split["w26_w27_oos"]),
        "all_w01_w27": sorted(
            windows_by_split["selection_dev"]
            + windows_by_split["holdout"]
            + windows_by_split["w26_w27_oos"]
        ),
    }
    out: dict[str, Any] = {}
    for sp_name, win_list in split_defs.items():
        win_set = set(win_list)
        sp_arms: dict[str, Any] = {}
        for arm in arm_names:
            if arm == "flat":
                sp_arms["flat"] = _flat_summary(win_list, venue=venue, r1=r1)
                continue
            sub = [r for r in rows_by_arm[arm] if r["win"] in win_set]
            agg = _aggregate_with_tail(sub, venue=venue, r1=r1, ordered_windows=win_list)
            agg["funnel"] = {
                "candidate_events_total": sum(r["funnel"]["candidate_events_total"] for r in sub),
                "off_cooldown_checks": sum(r["funnel"]["off_cooldown_checks"] for r in sub),
                "gate_rejected": sum(r["funnel"]["gate_rejected"] for r in sub),
                "entry_depth_rejected": sum(r["funnel"]["entry_depth_rejected"] for r in sub),
                "filled_trades": sum(r["funnel"]["filled_trades"] for r in sub),
            }
            sp_arms[arm] = agg
        out[sp_name] = sp_arms
    return out


def _compute_regression_verdict(part_b_splits: dict[str, Any], boot_oos: dict[str, Any]) -> dict[str, Any]:
    """Enforce strict regression verdict (`NO-GO / HOLD` unless OOS clears all cost & CI gates)."""
    ho = part_b_splits["holdout"]["single_gate"]
    w26_27 = part_b_splits["w26_w27_oos"]["single_gate"]
    oos = part_b_splits["oos_w21_w27"]["single_gate"]
    ho_base = part_b_splits["holdout"]["baseline"]
    w26_27_base = part_b_splits["w26_w27_oos"]["baseline"]
    oos_base = part_b_splits["oos_w21_w27"]["baseline"]

    checks = {
        "holdout_mtm_positive": ho["mtm_incl_residual_net_0_0_usd"] > 0.0,
        "holdout_plus_1bp_positive": ho["net_0_0_plus_1bp_usd"] > 0.0,
        "w26_w27_oos_mtm_positive": w26_27["mtm_incl_residual_net_0_0_usd"] > 0.0,
        "w26_w27_oos_delta_positive": (
            w26_27["mtm_incl_residual_net_0_0_usd"] - w26_27_base["mtm_incl_residual_net_0_0_usd"]
        )
        > 0.0,
        "oos_w21_w27_delta_clears_threshold": (
            oos["mtm_incl_residual_net_0_0_usd"] - oos_base["mtm_incl_residual_net_0_0_usd"]
        )
        >= OOS_DELTA_COST_THRESHOLD_USD,
        "oos_w21_w27_boot_ci95_lower_gt_zero": boot_oos["total_ci95_usd"][0] > 0.0,
        "holdout_zero_residual": ho["terminal_residual_notional_usd"] <= 1e-6,
        "w26_w27_zero_residual": w26_27["terminal_residual_notional_usd"] <= 1e-6,
    }
    passed_all = all(checks.values())
    return {
        "verdict": "PROCEED_TO_SHADOW" if passed_all else "NO-GO / HOLD",
        "oos_delta_cost_threshold_usd": OOS_DELTA_COST_THRESHOLD_USD,
        "checks": checks,
        "holdout_delta_usd": round(
            ho["mtm_incl_residual_net_0_0_usd"] - ho_base["mtm_incl_residual_net_0_0_usd"], 4
        ),
        "w26_w27_oos_delta_usd": round(
            w26_27["mtm_incl_residual_net_0_0_usd"] - w26_27_base["mtm_incl_residual_net_0_0_usd"],
            4,
        ),
        "oos_w21_w27_delta_usd": round(
            oos["mtm_incl_residual_net_0_0_usd"] - oos_base["mtm_incl_residual_net_0_0_usd"], 4
        ),
    }


def run_evaluation(
    lakehouse_dir: Path,
    *,
    fee_evidence_path: Path | None = None,
    output_path: Path | None = None,
) -> dict[str, Any]:
    """Run full Part A (P0 before/after) & Part B (controlled single-gate) evaluation across W01–W27."""
    r1 = _load_local_module("research_track01_profitable_alphas", "research_track01_profitable_alphas.py")
    amto = _load_local_module("audit_multiple_testing_overfitting", "audit_multiple_testing_overfitting.py")

    windows = r1.discover_windows_w01_w27(lakehouse_dir)
    if not windows:
        raise RuntimeError(f"No w01..w27 partitions discovered under {lakehouse_dir}")

    fee_meta = r1.resolve_all_fee_models(fee_evidence_path)
    fee_sha = _sha256_file(fee_evidence_path)

    windows_by_split: dict[str, list[str]] = {
        "selection_dev": sorted([w for w, i in windows.items() if i["split"] == "selection_dev"]),
        "holdout": sorted([w for w, i in windows.items() if i["split"] == "holdout"]),
        "w26_w27_oos": sorted([w for w, i in windows.items() if i["split"] == "w26_w27_oos"]),
    }

    # Part A storage: (arm_key, venue, mode_flag) -> rows
    part_a_rows: dict[tuple[str, str, str], list[dict[str, Any]]] = defaultdict(list)

    # Part B storage (`lighter_robinhood`, `lb15m_r60_rf0_f0`, P0-fixed)
    gate_defs: dict[str, Callable[[int, int, float, float], bool] | None] = {
        "baseline": None,
        "single_gate": lambda t_dec, side, spr, l1: spr <= FROZEN_TIGHT_SPREAD_MAX_BPS,
        "screen_us_rth_utc": lambda t_dec, side, spr, l1: _is_us_rth_utc(t_dec),
        "screen_l1_ge_1000": lambda t_dec, side, spr, l1: l1 >= FROZEN_PARTICIPATION_MIN_L1_USD,
    }
    part_b_rows: dict[str, list[dict[str, Any]]] = defaultdict(list)
    trade_mk_by_arm_split: dict[tuple[str, str], list[dict[str, Any]]] = defaultdict(list)
    cand_mk_by_gate_split: dict[tuple[str, str], list[dict[str, Any]]] = defaultdict(list)

    for wid in sorted(windows.keys()):
        winfo = windows[wid]
        split_name = winfo["split"]
        campaigns = winfo["campaigns"]
        for venue in ("lighter_robinhood", "lighter"):
            bundle = r1.load_window_venue_bundle(lakehouse_dir, venue, campaigns)
            bbo = bundle["bbo"]
            bbo_ts = bundle["bbo_ts"]
            cont = bundle["cont"]
            ctx = bundle["ctx"]
            ctx_ts = bundle["ctx_ts"]

            for sym in sorted(r1.VENUE_MARKETS[venue].keys()):
                if sym not in bbo or sym not in ctx or not cont[sym]["is_continuous_10h"]:
                    continue
                sig = r1.precompute_directional_series(bbo[sym], bbo_ts[sym], ctx[sym], ctx_ts[sym])
                if sig is None:
                    continue

                # Part A: Frozen OverreactionReversion lb15m_r60_rf0_f0 & OracleDiscoveryMomentum d25_h30m_f1
                for mode_lbl, conserve_flag in (("before_p0", False), ("after_p0", True)):
                    res_rev = r1.simulate_overreaction_reversion(
                        sig,
                        lookback_bars=15,
                        min_return_bps=60.0,
                        reversion_target_fraction=0.0,
                        require_funding_alignment=False,
                        exec_mode="taker",
                        clip_usd=500.0,
                        conserve_exit_depth=conserve_flag,
                    )
                    part_a_rows[("rev_lb15m_r60_rf0_f0", venue, mode_lbl)].append(
                        {"venue": venue, "split": split_name, "win": wid, "symbol": sym, **res_rev}
                    )

                    res_mom = r1.simulate_oracle_momentum(
                        sig,
                        min_dislocation_bps=25.0,
                        max_hold_ms=1_800_000,
                        require_funding_alignment=True,
                        exec_mode="taker",
                        clip_usd=500.0,
                        conserve_exit_depth=conserve_flag,
                    )
                    part_a_rows[("mom_d25_h30m_f1", venue, mode_lbl)].append(
                        {"venue": venue, "split": split_name, "win": wid, "symbol": sym, **res_mom}
                    )

                # Part B: Controlled Single-Factor Experiment on RH lb15m_r60_rf0_f0
                if venue == "lighter_robinhood":
                    c_pass, c_fail = collect_candidate_markouts(sig, r1)
                    cand_mk_by_gate_split[("gate_pass", split_name)].extend(c_pass)
                    cand_mk_by_gate_split[("gate_fail", split_name)].extend(c_fail)

                    for arm_name, gfn in gate_defs.items():
                        collect_mk = arm_name in ("baseline", "single_gate")
                        res_b = simulate_controlled_overreaction_arm(
                            sig, r1, gate_fn=gfn, collect_markouts=collect_mk
                        )
                        mk_list = res_b.pop("trade_markouts", [])
                        if collect_mk:
                            trade_mk_by_arm_split[(arm_name, split_name)].extend(mk_list)
                        part_b_rows[arm_name].append(
                            {"venue": venue, "split": split_name, "win": wid, "symbol": sym, **res_b}
                        )

            del bundle
            gc.collect()

    # Summarize Part A
    part_a_summary: dict[str, Any] = {}
    all_wins_sorted = sorted(windows.keys())
    split_windows_map = {**windows_by_split, "all_w01_w27": all_wins_sorted}
    part_a_configs = (
        ("rev_lb15m_r60_rf0_f0", "lighter_robinhood", "all_symbols"),
        ("rev_lb15m_r60_rf0_f0", "lighter", "all_symbols"),
        ("mom_d25_h30m_f1", "lighter_robinhood", "rwa_only"),
        ("mom_d25_h30m_f1", "lighter_robinhood", "all_symbols"),
        ("mom_d25_h30m_f1", "lighter", "rwa_only"),
        ("mom_d25_h30m_f1", "lighter", "all_symbols"),
    )
    for arm_key, venue, cohort in part_a_configs:
        slot_key = f"{arm_key}__{venue}__{cohort}"
        part_a_summary[slot_key] = {}
        for sp_name, wlist in split_windows_map.items():
            wset = set(wlist)
            before_sub = [
                r
                for r in part_a_rows[(arm_key, venue, "before_p0")]
                if r["win"] in wset and (cohort == "all_symbols" or r["symbol"] not in r1.CRYPTO_SET)
            ]
            after_sub = [
                r
                for r in part_a_rows[(arm_key, venue, "after_p0")]
                if r["win"] in wset and (cohort == "all_symbols" or r["symbol"] not in r1.CRYPTO_SET)
            ]
            b_agg = _aggregate_with_tail(before_sub, venue=venue, r1=r1, ordered_windows=wlist)
            a_agg = _aggregate_with_tail(after_sub, venue=venue, r1=r1, ordered_windows=wlist)
            part_a_summary[slot_key][sp_name] = {
                "before_p0": b_agg,
                "after_p0": a_agg,
                "delta_after_minus_before": _delta_dict(a_agg, b_agg),
            }

    # Summarize Part B (`baseline`, `single_gate`, `flat` + screening diagnostics)
    part_b_splits = _build_split_summary(
        rows_by_arm=part_b_rows,
        windows_by_split=windows_by_split,
        venue="lighter_robinhood",
        r1=r1,
        arm_names=("baseline", "single_gate", "flat"),
    )
    dev_screen_splits = _build_split_summary(
        rows_by_arm=part_b_rows,
        windows_by_split=windows_by_split,
        venue="lighter_robinhood",
        r1=r1,
        arm_names=("baseline", "single_gate", "screen_us_rth_utc", "screen_l1_ge_1000"),
    )

    # Stationary block bootstrap 95% CI on delta(single_gate - baseline)
    bootstrap_delta: dict[str, Any] = {}
    for sp_name in ("selection_dev", "holdout", "w26_w27_oos", "oos_w21_w27", "all_w01_w27"):
        base_sp = part_b_splits[sp_name]["baseline"]
        gate_sp = part_b_splits[sp_name]["single_gate"]
        w_ord = sorted(base_sp["per_window_mtm_0_0"].keys())
        delta_0_0 = [gate_sp["per_window_mtm_0_0"][w] - base_sp["per_window_mtm_0_0"][w] for w in w_ord]
        delta_p1 = [gate_sp["per_window_p1bp_mtm"][w] - base_sp["per_window_p1bp_mtm"][w] for w in w_ord]
        bootstrap_delta[sp_name] = {
            "mtm_0_0_usd": stationary_block_bootstrap_ci(delta_0_0),
            "mtm_plus_1bp_usd": stationary_block_bootstrap_ci(delta_p1),
        }

    # Conditional markout attribution (1m / 5m / 15m / 60m)
    markout_attribution = {
        "filled_trades": {
            sp: {
                arm: _summarize_markouts(trade_mk_by_arm_split[(arm, sp)])
                for arm in ("baseline", "single_gate")
            }
            for sp in ("selection_dev", "holdout", "w26_w27_oos")
        },
        "candidate_events_declustered_60s": {
            sp: {
                lbl: _summarize_markouts(cand_mk_by_gate_split[(lbl, sp)])
                for lbl in ("gate_pass", "gate_fail")
            }
            for sp in ("selection_dev", "holdout", "w26_w27_oos")
        },
    }
    for cohort_map, key_pair in (
        (markout_attribution["filled_trades"], ("baseline", "single_gate")),
        (markout_attribution["candidate_events_declustered_60s"], ("gate_pass", "gate_fail")),
    ):
        cohort_map["oos_w21_w27"] = {
            lbl: _summarize_markouts(
                (trade_mk_by_arm_split if key_pair[0] == "baseline" else cand_mk_by_gate_split)[(lbl, "holdout")]
                + (trade_mk_by_arm_split if key_pair[0] == "baseline" else cand_mk_by_gate_split)[
                    (lbl, "w26_w27_oos")
                ]
            )
            for lbl in key_pair
        }

    # Multiple testing: raw N_trials & N_eff DSR + CSCV PBO (both 2-arm controlled and 4-arm honest screen)
    def _mt_for(arms: Sequence[str], sp_name: str) -> dict[str, Any]:
        w_ord = sorted(part_b_splits[sp_name]["baseline"]["per_window_mtm_0_0"].keys())
        tdict = {
            a: [dev_screen_splits[sp_name][a]["per_window_mtm_0_0"][w] for w in w_ord]
            for a in arms
        }
        return r1.compute_multiple_testing_metrics(tdict, num_blocks=8)

    multiple_testing = {
        "controlled_2arm_dev": _mt_for(("baseline", "single_gate"), "selection_dev"),
        "controlled_2arm_w01_w27": _mt_for(("baseline", "single_gate"), "all_w01_w27"),
        "screened_4arm_dev": _mt_for(
            ("baseline", "single_gate", "screen_us_rth_utc", "screen_l1_ge_1000"), "selection_dev"
        ),
        "screened_4arm_w01_w27": _mt_for(
            ("baseline", "single_gate", "screen_us_rth_utc", "screen_l1_ge_1000"), "all_w01_w27"
        ),
    }

    regression_verdict = _compute_regression_verdict(part_b_splits, bootstrap_delta["oos_w21_w27"]["mtm_0_0_usd"])

    out = {
        "schema": "track01_depth_and_single_gate_w01_w27_v1",
        "operational_mode": "HOLD",
        "protocol": {
            "selection_dev": windows_by_split["selection_dev"],
            "holdout": windows_by_split["holdout"],
            "w26_w27_oos": windows_by_split["w26_w27_oos"],
            "excluded_windows": ["w08", "w28+"],
            "clip_usd": 500.0,
            "taker_delay_ms": r1.TAKER_DELAY_MS,
            "l1_depth_cap_frac": r1.L1_DEPTH_CAP_FRAC,
            "frozen_single_gate": {
                "name": "tight_spread_le_5bp",
                "rule": f"spread_bps <= {FROZEN_TIGHT_SPREAD_MAX_BPS}",
                "frozen_on_split": "selection_dev (w01-w20)",
                "multi_parameter_grid_search": False,
                "dev_screen_candidates_considered": [
                    "tight_spread_le_5bp (spread_bps <= 5.0)",
                    "us_rth_utc (14:30 <= UTC < 21:00)",
                    "l1_depth_ge_1000 (entry_l1_usd >= 1000.0)",
                ],
            },
        },
        "fee_models": fee_meta,
        "fee_evidence_sha256": fee_sha,
        "part_a_p0_depth_conservation_impact": part_a_summary,
        "part_b_controlled_single_gate": {
            "base_strategy": "OverreactionReversion lb15m_r60_rf0_f0 lighter_robinhood taker 0/0 bps ($500 clip, P0-fixed)",
            "splits": part_b_splits,
            "dev_screen_arms_selection_dev": dev_screen_splits["selection_dev"],
            "stationary_block_bootstrap_delta_gate_minus_baseline": bootstrap_delta,
            "conditional_markout_attribution": markout_attribution,
            "multiple_testing": multiple_testing,
            "regression_verdict": regression_verdict,
        },
    }

    if output_path is not None:
        output_path.parent.mkdir(parents=True, exist_ok=True)
        output_path.write_text(json.dumps(out, indent=2, allow_nan=False) + "\n", encoding="utf-8")
    return out


def run_self_test() -> dict[str, Any]:
    """Deterministic pure-stdlib synthetic verification of Part A, Part B, markouts, bootstrap, and DSR/PBO."""
    r1 = _load_local_module("research_track01_profitable_alphas", "research_track01_profitable_alphas.py")

    # 1. Synthetic Part A before-vs-after P0 depth conservation check on OverreactionReversion
    # Terminal flatten at i=2110 sees a single stale ~$198.78 L1 bid from i=2100 -> ~$99.39 fill + ~$400.61 residual
    t0 = 1_000_000
    bbo_a2: list[tuple[int, float, float, float, float]] = []
    for i in range(2_121):
        t = t0 + i * 1_000
        m = i // 60
        px = 100.0 if m < 20 else (99.0 if m < 25 else 99.40)
        if i <= 2_099:
            bbo_a2.append((t, px - 0.01, 20.0, px + 0.01, 20.0))
        elif i == 2_100:
            bbo_a2.append((t, px - 0.01, 2.0, px + 0.01, 20.0))
        elif i == 2_120:
            bbo_a2.append((t, px - 0.01, 0.0, px + 0.01, 20.0))
    ctx_a2 = [(t0 + i * 1_000, 99.40, 99.40, 0.0) for i in range(2_121)]
    sig_a2 = r1.precompute_directional_series(bbo_a2, [b[0] for b in bbo_a2], ctx_a2, [c[0] for c in ctx_a2])
    assert sig_a2 is not None
    before = r1.simulate_overreaction_reversion(
        sig_a2, lookback_bars=15, min_return_bps=60.0, reversion_target_fraction=0.0,
        require_funding_alignment=False, exec_mode="taker", clip_usd=500.0, conserve_exit_depth=False
    )
    after = r1.simulate_overreaction_reversion(
        sig_a2, lookback_bars=15, min_return_bps=60.0, reversion_target_fraction=0.0,
        require_funding_alignment=False, exec_mode="taker", clip_usd=500.0, conserve_exit_depth=True
    )
    assert before["trades"] >= 1 and after["trades"] >= 1
    assert before["terminal_residual_notional_usd"] == 0.0
    assert after["terminal_residual_notional_usd"] > 350.0
    assert after["residual_trades"] >= 1

    # 2. Synthetic Part B controlled gate vs baseline vs flat + markouts + bootstrap + DSR/PBO
    bbo_b: list[tuple[int, float, float, float, float]] = []
    ctx_b: list[tuple[int, float, float, float]] = []
    for i in range(6_000):
        t = t0 + i * 1_000
        m = i // 60
        px = 100.0 if m < 20 else (99.0 if m < 35 else 99.60)
        # Wide spread (12 bps) at m=20..22 -> gate rejects; tight spread (2 bps) at m=23+ -> gate accepts
        half = 0.06 if 20 <= m <= 22 else 0.01
        bbo_b.append((t, px - half, 20.0, px + half, 20.0))
        ctx_b.append((t, px, px, 0.0))
    sig_b = r1.precompute_directional_series(bbo_b, [b[0] for b in bbo_b], ctx_b, [c[0] for c in ctx_b])
    assert sig_b is not None
    arm_base = simulate_controlled_overreaction_arm(sig_b, r1, gate_fn=None)
    arm_gate = simulate_controlled_overreaction_arm(
        sig_b, r1, gate_fn=lambda t_dec, side, spr, l1: spr <= FROZEN_TIGHT_SPREAD_MAX_BPS
    )
    assert arm_base["trades"] >= 1 and arm_gate["trades"] >= 1
    assert arm_gate["funnel"]["gate_rejected"] > 0
    assert arm_base["funnel"]["candidate_events_total"] == arm_gate["funnel"]["candidate_events_total"]
    mk_sum = _summarize_markouts(arm_gate["trade_markouts"])
    assert mk_sum["count"] == arm_gate["trades"]

    boot = stationary_block_bootstrap_ci([2.0, -1.0, 3.0, 1.5, 2.5, -0.5, 4.0], n_boot=500)
    assert boot["n_windows"] == 7 and boot["total_ci95_usd"][0] <= boot["observed_total_usd"] <= boot["total_ci95_usd"][1]

    mt = r1.compute_multiple_testing_metrics(
        {
            "baseline": [2.0 + (i % 3) for i in range(20)],
            "single_gate": [3.5 + (i % 2) for i in range(20)],
        },
        num_blocks=8,
    )
    assert mt["status"] == "ok"

    return {
        "self_test": "ok",
        "part_a_before_res_usd": before["terminal_residual_notional_usd"],
        "part_a_after_res_usd": after["terminal_residual_notional_usd"],
        "part_b_base_trades": arm_base["trades"],
        "part_b_gate_trades": arm_gate["trades"],
        "part_b_gate_rejected": arm_gate["funnel"]["gate_rejected"],
        "boot_ci95": boot["total_ci95_usd"],
        "dsr_eff": mt["dsr"]["dsr_effective"],
    }


def main(argv: Sequence[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--lakehouse", type=Path, default=Path(os.path.expanduser("~/aleph-tx-lakehouse")))
    ap.add_argument("--fee-evidence", type=Path, default=None)
    ap.add_argument(
        "--output",
        type=Path,
        default=Path("/tmp/roadmap_upgrade_20261127/track01_depth_and_single_gate_w01_w27.json"),
    )
    ap.add_argument("--self-test", action="store_true")
    args = ap.parse_args(argv)

    if args.self_test:
        print(json.dumps(run_self_test(), indent=2, allow_nan=False))
        return 0

    res = run_evaluation(
        args.lakehouse,
        fee_evidence_path=args.fee_evidence,
        output_path=args.output,
    )
    print(
        json.dumps(
            {
                "ok": True,
                "output": str(args.output),
                "verdict": res["part_b_controlled_single_gate"]["regression_verdict"]["verdict"],
                "holdout_delta_usd": res["part_b_controlled_single_gate"]["regression_verdict"][
                    "holdout_delta_usd"
                ],
                "w26_w27_oos_delta_usd": res["part_b_controlled_single_gate"]["regression_verdict"][
                    "w26_w27_oos_delta_usd"
                ],
            },
            indent=2,
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
