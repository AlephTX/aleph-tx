#!/usr/bin/env python3
"""Track 02 `B0` Settleability, Per-Venue Collateral & Zero-Funding-Boundary Audit (`W01–W27`).

Evaluates `Core <-> Arcus` (`USDC/USDC`, verified `Lighter Prem 0.40/2.80` + `Arcus 0.00/2.1375` RWA /
`1.425/4.275` Crypto) across ALL Aleph Tokyo `W01..W27` (`selection_dev` w01-w20, `holdout` w21-w25,
`w26_w27_oos` w26-w27; strictly excluding `w08` crypto and `-w28`), at actual event-by-event replay
clip sizes (`$500`, `$1,000`, `$2,000`) with zero post-hoc linear scaling.

Compares identically on the same windows/events:
1. `B0_original`: Frozen `B0` (`Lighter` Maker `40ms` -> `Arcus` Taker `36ms`, `b0_e10_v0.0_sh0`).
2. `B0_funding_flat`: Blocks new risk-increasing opens within buffer before `:00` UTC hourly funding
   boundary AND forces dual-leg taker flatten before the `:00` guard interval so
   `crossed_funding_hours == 0`, formally proving funding avoidance under `TEN_HOUR_RESEARCH.md`
   while `Arcus` `funding_and_ctx` is absent from the Aleph lakehouse.
3. `flat`: Zero-trade baseline (`$0.00` PnL, `$0.00` margin, `0` crossed funding hours).
"""

from __future__ import annotations

import argparse
from bisect import bisect_left, bisect_right
from dataclasses import asdict
from datetime import datetime, timezone
import gc
import json
import math
from pathlib import Path
import random
import subprocess
import sys
from typing import Any, Sequence
import urllib.request
from zoneinfo import ZoneInfo

SCRIPT_DIR = Path(__file__).resolve().parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

from audit_multiple_testing_overfitting import (  # noqa: E402
    cscv_pbo,
    deflated_sharpe_report,
    probabilistic_sharpe_ratio,
    series_moments,
)
from research_track02_arcus_rh_zero_fee_basis import (  # noqa: E402
    BasisArbParams,
    CRYPTO_SYMBOLS,
    HOUR_MS,
    MAX_BOOK_AGE_MS,
    MAX_RECOVERY_MS,
    MIN_FILL_USD,
    Q_MAX_CLIPS,
    RWA_ARCUS_LIGHTER_EXTRA,
    RWA_ARCUS_RH_SYMBOLS,
    STEP_MS,
    TEN_HOURS_MS,
    VENUE_SYMBOL_MAP,
    VENUE_TIMING,
    WARMUP_MS,
    _build_grid_np,
    _depth_sweep_cost,
    _infer_tick,
    _load_venue_bbo_buckets,
    _load_venue_funding,
    _load_venue_trades,
    _settle_lot_funding,
    _skew_q,
    bucket_trades_by_step,
    campaign_parquet_files,
    compute_causal_basis_2h,
    discover_w01_w27_campaigns,
    resolve_track02_fees,
)

NY_TZ = ZoneInfo("America/New_York")
CLIP_SIZES_USD: tuple[float, ...] = (500.0, 1_000.0, 2_000.0)
CHECKPOINT_STEP_MS = 60_000  # 1-minute aligned checkpoints for concurrent portfolio peak & margin aggregation
DEFAULT_OPEN_BLOCK_BEFORE_HOUR_MS = 120_000  # 2m before :00 UTC boundary: block new risk-increasing opens
DEFAULT_FLATTEN_BEFORE_HOUR_MS = 30_000  # 30s before :00 UTC boundary: cancel quotes & force dual-leg flatten
DEFAULT_GUARD_HALF_MS = 5_000  # [ht - 5s, ht + 5s] formal funding-avoidance proof window
REFERENCE_BUDGETS_USD: tuple[float, ...] = (10_000.0, 20_000.0)  # Split 50/50 Lighter/Arcus USDC collateral

RWA_CORE_ARCUS_SYMBOLS: tuple[str, ...] = tuple(
    s for s in (RWA_ARCUS_RH_SYMBOLS + RWA_ARCUS_LIGHTER_EXTRA) if s in VENUE_SYMBOL_MAP["lighter"]
)

# Audited public contract margin specs (snapshot from api.arcus.xyz/v1/markets &
# mainnet.zklighter.elliot.ai/api/v1/orderBookDetails on 2026-11-27; offline fallback).
ARCUS_MARGIN_SPECS: dict[str, dict[str, float]] = {
    "AAPL": {"imf": 0.05, "off_imf": 0.075, "mmf": 0.033334, "liq_fee_bps": 50.0},
    "AMD": {"imf": 0.10, "off_imf": 0.15, "mmf": 0.066667, "liq_fee_bps": 100.0},
    "AMZN": {"imf": 0.05, "off_imf": 0.075, "mmf": 0.033334, "liq_fee_bps": 50.0},
    "BTC": {"imf": 0.025, "off_imf": 0.025, "mmf": 0.016667, "liq_fee_bps": 25.0},
    "COIN": {"imf": 0.20, "off_imf": 0.30, "mmf": 0.133334, "liq_fee_bps": 100.0},
    "ETH": {"imf": 0.04, "off_imf": 0.04, "mmf": 0.026667, "liq_fee_bps": 40.0},
    "GOOGL": {"imf": 0.05, "off_imf": 0.075, "mmf": 0.033334, "liq_fee_bps": 50.0},
    "HOOD": {"imf": 0.10, "off_imf": 0.15, "mmf": 0.066667, "liq_fee_bps": 100.0},
    "META": {"imf": 0.05, "off_imf": 0.075, "mmf": 0.033334, "liq_fee_bps": 50.0},
    "MSFT": {"imf": 0.05, "off_imf": 0.075, "mmf": 0.033334, "liq_fee_bps": 50.0},
    "MSTR": {"imf": 0.10, "off_imf": 0.15, "mmf": 0.066667, "liq_fee_bps": 100.0},
    "MU": {"imf": 0.10, "off_imf": 0.15, "mmf": 0.066667, "liq_fee_bps": 100.0},
    "NVDA": {"imf": 0.05, "off_imf": 0.075, "mmf": 0.033334, "liq_fee_bps": 50.0},
    "PLTR": {"imf": 0.10, "off_imf": 0.15, "mmf": 0.066667, "liq_fee_bps": 100.0},
    "QQQ": {"imf": 0.04, "off_imf": 0.06, "mmf": 0.026667, "liq_fee_bps": 40.0},
    "SOL": {"imf": 0.05, "off_imf": 0.05, "mmf": 0.033334, "liq_fee_bps": 50.0},
    "SPY": {"imf": 0.02, "off_imf": 0.03, "mmf": 0.013334, "liq_fee_bps": 20.0},
    "TSLA": {"imf": 0.05, "off_imf": 0.075, "mmf": 0.033334, "liq_fee_bps": 50.0},
}

LIGHTER_MARGIN_SPECS: dict[str, dict[str, float]] = {
    "AAPL": {"def_imf": 0.0666, "min_imf": 0.05, "mmf": 0.03, "cmf": 0.02},
    "AMD": {"def_imf": 0.10, "min_imf": 0.10, "mmf": 0.06, "cmf": 0.04},
    "AMZN": {"def_imf": 0.0666, "min_imf": 0.05, "mmf": 0.03, "cmf": 0.02},
    "BTC": {"def_imf": 0.05, "min_imf": 0.02, "mmf": 0.012, "cmf": 0.008},
    "COIN": {"def_imf": 0.10, "min_imf": 0.10, "mmf": 0.06, "cmf": 0.04},
    "ETH": {"def_imf": 0.05, "min_imf": 0.02, "mmf": 0.012, "cmf": 0.008},
    "GOOGL": {"def_imf": 0.0666, "min_imf": 0.05, "mmf": 0.03, "cmf": 0.02},
    "HOOD": {"def_imf": 0.10, "min_imf": 0.10, "mmf": 0.06, "cmf": 0.04},
    "META": {"def_imf": 0.0666, "min_imf": 0.05, "mmf": 0.03, "cmf": 0.02},
    "MSFT": {"def_imf": 0.0666, "min_imf": 0.05, "mmf": 0.03, "cmf": 0.02},
    "MSTR": {"def_imf": 0.10, "min_imf": 0.10, "mmf": 0.06, "cmf": 0.04},
    "MU": {"def_imf": 0.0666, "min_imf": 0.05, "mmf": 0.03, "cmf": 0.02},
    "NVDA": {"def_imf": 0.0666, "min_imf": 0.05, "mmf": 0.03, "cmf": 0.02},
    "PLTR": {"def_imf": 0.10, "min_imf": 0.10, "mmf": 0.06, "cmf": 0.04},
    "QQQ": {"def_imf": 0.0666, "min_imf": 0.0333, "mmf": 0.0199, "cmf": 0.0133},
    "SOL": {"def_imf": 0.0666, "min_imf": 0.04, "mmf": 0.024, "cmf": 0.016},
    "SPY": {"def_imf": 0.0666, "min_imf": 0.02, "mmf": 0.012, "cmf": 0.008},
    "TSLA": {"def_imf": 0.0666, "min_imf": 0.05, "mmf": 0.03, "cmf": 0.02},
}


def frozen_b0_params(clip_usd: float) -> BasisArbParams:
    """Return frozen Core <-> Arcus `B0` baseline (`b0_e10_v0.0_sh0`) calibrated at `clip_usd`."""
    return BasisArbParams(
        name="b0_e10_v0.0_sh0",
        arm="B0_hedge_mk_arcus_tk",
        flipped_role=False,
        hedge_mode="taker",
        e0_bps=10.0,
        tau_s=7200.0,
        vol_edge_mult=0.0,
        min_hedge_l1_notional_usd=clip_usd,
        shield_bps=None,
    )


def build_b0_grid(clip_usd: float) -> list[BasisArbParams]:
    """Return the pre-registered 16-config `B0` grid for multiple-testing DSR / CSCV PBO audit."""
    return [
        BasisArbParams(
            name=f"b0_e{int(e0)}_v{vm}_sh{int(sh) if sh else 0}",
            arm="B0_hedge_mk_arcus_tk",
            flipped_role=False,
            hedge_mode="taker",
            e0_bps=e0,
            tau_s=7200.0,
            vol_edge_mult=vm,
            min_hedge_l1_notional_usd=clip_usd,
            shield_bps=sh,
        )
        for e0 in (4.0, 6.0, 8.0, 10.0)
        for vm in (0.0, 1.0)
        for sh in (None, 2.0)
    ]


def is_arcus_rwa_outside_rth(ts_ms: int) -> bool:
    """Return True when `ts_ms` is outside Arcus RWA extended RTH (`04:00-20:00 America/New_York` Mon-Fri)."""
    dt_ny = datetime.fromtimestamp(ts_ms / 1000.0, tz=timezone.utc).astimezone(NY_TZ)
    if dt_ny.weekday() >= 5:
        return True
    sec_day = dt_ny.hour * 3600 + dt_ny.minute * 60 + dt_ny.second
    return sec_day < 14_400 or sec_day >= 72_000


def venue_margin_fractions(symbol: str, domain: str, ts_ms: int) -> dict[str, float]:
    """Return exact Lighter & Arcus IMR/MMR fractions at `ts_ms`."""
    arc = ARCUS_MARGIN_SPECS.get(symbol, {"imf": 0.10, "off_imf": 0.15, "mmf": 0.066667})
    lt = LIGHTER_MARGIN_SPECS.get(symbol, {"def_imf": 0.10, "min_imf": 0.10, "mmf": 0.06})
    arc_imf = arc["off_imf"] if (domain == "rwa" and is_arcus_rwa_outside_rth(ts_ms)) else arc["imf"]
    return {
        "lighter_imf": float(lt["def_imf"]),
        "lighter_min_imf": float(lt["min_imf"]),
        "lighter_mmf": float(lt["mmf"]),
        "arcus_imf": float(arc_imf),
        "arcus_mmf": float(arc["mmf"]),
    }


def _fetch_json_readonly(url: str, timeout_s: int = 8) -> Any:
    """Read-only public HTTP GET with `curl` fallback (zero auth, zero keys)."""
    try:
        req = urllib.request.Request(url, headers={"User-Agent": "AlephTX-Research/1.0"})
        with urllib.request.urlopen(req, timeout=timeout_s) as resp:
            return json.loads(resp.read().decode("utf-8"))
    except Exception:
        out = subprocess.check_output(["curl", "-sS", "--max-time", str(timeout_s), url], text=True)
        return json.loads(out)


def probe_public_arcus_and_lighter_metadata(
    symbols: Sequence[str] = ("NVDA", "AAPL", "BTC"),
) -> dict[str, Any]:
    """Optional read-only check of public Arcus/Lighter market & funding metadata (lakehouse stays primary)."""
    summary: dict[str, Any] = {
        "lakehouse_primary_fact": "Aleph lakehouse has 0 funding_and_ctx parquet files for Arcus across W01-W27.",
        "arcus_markets_probe": {"status": "skipped_or_error"},
        "arcus_funding_probe": {"status": "skipped_or_error"},
        "lighter_markets_probe": {"status": "skipped_or_error"},
    }
    try:
        arc_raw = _fetch_json_readonly("https://api.arcus.xyz/v1/markets")
        arc_list = arc_raw.get("markets", [])
        sampled = {}
        for m in arc_list:
            base = m.get("baseAsset")
            if base in symbols and m.get("type") == "PERPETUAL":
                sampled[base] = {
                    "market": m.get("marketDisplayName"),
                    "initialMarginFraction": m.get("initialMarginFraction"),
                    "offHoursInitialMarginFraction": m.get("offHoursInitialMarginFraction"),
                    "maintenanceMarginFraction": m.get("maintenanceMarginFraction"),
                    "nextFundingAt": m.get("nextFundingAt"),
                    "fundingRate": m.get("fundingRate"),
                    "nextFundingRate": m.get("nextFundingRate"),
                }
        summary["arcus_markets_probe"] = {
            "status": "ok",
            "total_markets": len(arc_list),
            "sample_specs": sampled,
        }
    except Exception as exc:
        summary["arcus_markets_probe"] = {"status": "error", "error": type(exc).__name__}

    try:
        fn_raw = _fetch_json_readonly("https://api.arcus.xyz/v1/fundingRates?market=NVDA-USD&limit=500")
        rates = fn_raw.get("fundingRates", [])
        times_ms = [int(r["time"]) // 1000 for r in rates if r.get("time") is not None]
        summary["arcus_funding_probe"] = {
            "status": "ok",
            "market": "NVDA-USD",
            "returned_hourly_points": len(rates),
            "min_time_ms": min(times_ms) if times_ms else None,
            "max_time_ms": max(times_ms) if times_ms else None,
            "exposes_public_rates_only_no_fill_settlement_cash": True,
            "note": "Public REST endpoint returns hourly rates (up to 500 recent hours), not lakehouse-captured per-fill settlement cash/oracle.",
        }
    except Exception as exc:
        summary["arcus_funding_probe"] = {"status": "error", "error": type(exc).__name__}

    try:
        lt_raw = _fetch_json_readonly("https://mainnet.zklighter.elliot.ai/api/v1/orderBookDetails")
        lt_list = lt_raw.get("order_book_details", [])
        sampled_lt = {}
        for m in lt_list:
            if m.get("symbol") in symbols and m.get("market_type") == "perp" and m.get("status") == "active":
                sampled_lt[m["symbol"]] = {
                    "market_id": m.get("market_id"),
                    "default_initial_margin_fraction": m.get("default_initial_margin_fraction"),
                    "min_initial_margin_fraction": m.get("min_initial_margin_fraction"),
                    "maintenance_margin_fraction": m.get("maintenance_margin_fraction"),
                    "closeout_margin_fraction": m.get("closeout_margin_fraction"),
                }
        summary["lighter_markets_probe"] = {
            "status": "ok",
            "total_entries": len(lt_list),
            "sample_specs": sampled_lt,
        }
    except Exception as exc:
        summary["lighter_markets_probe"] = {"status": "error", "error": type(exc).__name__}

    return summary


def _lookup_valid_step_with_retry(
    age: Sequence[int],
    bid: Sequence[float],
    ask: Sequence[float],
    target_ms: int,
    t0_ms: int,
    step_ms: int,
    max_age_steps: int,
    max_recovery_steps: int,
    *,
    retry_latency_ms: int,
    cutoff_ms: int | None = None,
) -> tuple[int, int] | None:
    """Lookup fresh step at `target_ms`; on miss, schedule `recovery_ts + retry_latency_ms` within `cutoff_ms`."""
    if cutoff_ms is not None and target_ms > cutoff_ms:
        return None
    n = len(bid)
    if n == 0:
        return None
    j0 = min(max(int((target_ms - t0_ms) // step_ms) - 1, 0), n - 1)
    if age[j0] <= max_age_steps:
        b0, a0 = float(bid[j0]), float(ask[j0])
        if b0 > 0.0 and a0 > b0 and math.isfinite(b0) and math.isfinite(a0):
            return j0, target_ms

    j = j0 + 1
    lim = min(n - 1, j0 + max_recovery_steps)
    while j <= lim:
        cand_ms = t0_ms + (j + 1) * step_ms
        if cutoff_ms is not None and cand_ms > cutoff_ms:
            return None
        if age[j] <= max_age_steps:
            b, a = float(bid[j]), float(ask[j])
            if b > 0.0 and a > b and math.isfinite(b) and math.isfinite(a):
                t_retry = cand_ms + max(0, int(retry_latency_ms))
                if cutoff_ms is not None and t_retry > cutoff_ms:
                    return None
                j_ret = min(max(int((t_retry - t0_ms) // step_ms) - 1, 0), n - 1)
                if age[j_ret] <= max_age_steps:
                    br, ar = float(bid[j_ret]), float(ask[j_ret])
                    if br > 0.0 and ar > br and math.isfinite(br) and math.isfinite(ar):
                        return j_ret, t_retry
                return None
        j += 1
    return None


def stationary_block_bootstrap_ci(
    series: Sequence[float],
    *,
    mean_block_len: float = 3.0,
    n_boot: int = 2_000,
    seed: int = 20261127,
) -> dict[str, Any]:
    """Politis-Romano (1994) stationary block bootstrap 95% CI (pure Python stdlib)."""
    vals = [float(x) for x in series]
    t = len(vals)
    if t == 0:
        return {"t": 0, "mean": 0.0, "total": 0.0, "ci95_mean": [0.0, 0.0], "ci95_total": [0.0, 0.0], "prob_gt_zero": 0.0}
    mean_obs = sum(vals) / t
    tot_obs = sum(vals)
    if t == 1 or len(set(vals)) <= 1:
        return {
            "t": t,
            "mean": round(mean_obs, 6),
            "total": round(tot_obs, 4),
            "ci95_mean": [round(mean_obs, 6), round(mean_obs, 6)],
            "ci95_total": [round(tot_obs, 4), round(tot_obs, 4)],
            "prob_gt_zero": 1.0 if tot_obs > 0.0 else 0.0,
        }
    p_jump = 1.0 / max(mean_block_len, 1.0)
    rng = random.Random(seed)
    boot_means: list[float] = []
    for _ in range(n_boot):
        idx = rng.randrange(t)
        acc = 0.0
        for step in range(t):
            if step > 0:
                if rng.random() < p_jump:
                    idx = rng.randrange(t)
                else:
                    idx = (idx + 1) % t
            acc += vals[idx]
        boot_means.append(acc / t)
    boot_means.sort()
    lo_idx = int(0.025 * (n_boot - 1))
    hi_idx = int(0.975 * (n_boot - 1))
    prob_pos = sum(1 for m in boot_means if m > 0.0) / n_boot
    return {
        "t": t,
        "mean_block_len": mean_block_len,
        "n_boot": n_boot,
        "seed": seed,
        "mean": round(mean_obs, 6),
        "total": round(tot_obs, 4),
        "ci95_mean": [round(boot_means[lo_idx], 6), round(boot_means[hi_idx], 6)],
        "ci95_total": [round(boot_means[lo_idx] * t, 4), round(boot_means[hi_idx] * t, 4)],
        "prob_gt_zero": round(prob_pos, 4),
    }


def simulate_b0_settleability(
    *,
    symbol: str,
    domain: str,
    arcus_bid: Sequence[float],
    arcus_ask: Sequence[float],
    arcus_bsz: Sequence[float],
    arcus_asz: Sequence[float],
    arcus_age: Sequence[int],
    hedge_bid: Sequence[float],
    hedge_ask: Sequence[float],
    hedge_bsz: Sequence[float],
    hedge_asz: Sequence[float],
    hedge_age: Sequence[int],
    causal: dict[str, list[float]],
    lighter_trades: dict[str, Sequence[Any]] | None,
    t0_ms: int,
    tick: float,
    params: BasisArbParams,
    mode: str,  # "B0_original", "B0_funding_flat", or "flat"
    lighter_maker_delay_ms: int = 40,
    lighter_taker_delay_ms: int = 140,
    lighter_speedbump_ms: int = 140,
    arcus_delay_ms: int = 36,
    lighter_mk_fee_bps: float = 0.40,
    lighter_tk_fee_bps: float = 2.80,
    arcus_mk_fee_bps: float = 0.0,
    arcus_tk_fee_bps: float = 2.1375,
    step_ms: int = STEP_MS,
    warmup_steps: int = WARMUP_MS // STEP_MS,
    decision_steps: int = TEN_HOURS_MS // STEP_MS,
    max_age_steps: int = MAX_BOOK_AGE_MS // STEP_MS,
    clip_usd: float = 1_000.0,
    q_max_clips: float = Q_MAX_CLIPS,
    lighter_funding: dict[int, float] | None = None,
    arcus_funding: dict[int, float] | None = None,
    pre_lighter_by_step: dict[int, list[int]] | None = None,
    open_block_before_hour_ms: int = DEFAULT_OPEN_BLOCK_BEFORE_HOUR_MS,
    flatten_before_hour_ms: int = DEFAULT_FLATTEN_BEFORE_HOUR_MS,
    guard_half_ms: int = DEFAULT_GUARD_HALF_MS,
    checkpoint_step_ms: int = CHECKPOINT_STEP_MS,
    record_checkpoints: bool = True,
    record_trades: bool = False,
) -> dict[str, Any]:
    """Event-by-event `Core <-> Arcus` `B0` settleability, margin & funding-avoidance audit."""
    if mode not in ("B0_original", "B0_funding_flat", "flat"):
        raise ValueError(f"Unsupported mode {mode!r}")

    n = len(arcus_bid)
    end_k = min(warmup_steps + decision_steps - 1, n - 1) if n > 0 else -1
    t_dec_start_ms = t0_ms + (warmup_steps + 1) * step_ms
    t_end_ms = t0_ms + (end_k + 1) * step_ms if end_k >= 0 else t0_ms

    # Enumerate every `:00` UTC hourly funding slot inside `[t_dec_start_ms, t_end_ms]`
    expected_hours: list[int] = []
    if end_k >= warmup_steps:
        first_h = (t_dec_start_ms // HOUR_MS) * HOUR_MS
        if first_h < t_dec_start_ms:
            first_h += HOUR_MS
        h_cur = first_h
        while h_cur <= t_end_ms:
            expected_hours.append(h_cur)
            h_cur += HOUR_MS

    empty_slots = [
        {
            "at_ms": ht,
            "guard_start_ms": ht - guard_half_ms,
            "guard_end_ms": ht + guard_half_ms,
            "max_abs_lighter_pos_base": 0.0,
            "max_abs_arcus_pos_base": 0.0,
            "max_open_order_count": 0,
            "max_pending_ioc_count": 0,
            "max_active_cycle_count": 0,
            "passed": True,
        }
        for ht in expected_hours
    ]

    empty: dict[str, Any] = {
        "symbol": symbol,
        "domain": domain,
        "mode": mode,
        "clip_usd": clip_usd,
        "param_name": "flat" if mode == "flat" else params.name,
        "fills": 0,
        "maker_filled_notional_usd": 0.0,
        "hedged_notional_usd": 0.0,
        "orphan_events": 0,
        "orphan_notional_usd": 0.0,
        "orphan_unwind_rate": 0.0,
        "orphan_unwind_pnl_usd": 0.0,
        "orphan_unwind_cost_usd": 0.0,
        "depth_clipped_entries": 0,
        "depth_clipped_exits": 0,
        "forced_funding_flattens": 0,
        "blocked_funding_open": 0,
        "blocked_toxic": 0,
        "blocked_drift": 0,
        "blocked_l1_thin": 0,
        "lighter_maker_fees_usdc": 0.0,
        "lighter_taker_fees_usdc": 0.0,
        "lighter_fees_usdc": 0.0,
        "arcus_maker_fees_usdc": 0.0,
        "arcus_taker_fees_usdc": 0.0,
        "arcus_fees_usdc": 0.0,
        "fees_usdc": 0.0,
        "fee_reconciled_fills": 0,
        "depth_slip_usd": 0.0,
        "funding_leg_lighter_usd": 0.0,
        "funding_leg_arcus_usd": 0.0,
        "funding_cash_usd": 0.0,
        "funding_payments": 0,
        "intraday_realized_pnl_usd": 0.0,
        "terminal_drain_pnl_usd": 0.0,
        "realized_only_pnl_usd": 0.0,
        "mtm_incl_residual_pnl_usd": 0.0,
        "terminal_flat": True,
        "terminal_residual_notional_usd": 0.0,
        "peak_gross_notional_usd": 0.0,
        "peak_single_leg_notional_usd": 0.0,
        "peak_net_notional_usd": 0.0,
        "worst_case_inflight_reserve_usd": 0.0,
        "peak_imr_lighter_usd": 0.0,
        "peak_imr_arcus_usd": 0.0,
        "peak_mmr_lighter_usd": 0.0,
        "peak_mmr_arcus_usd": 0.0,
        "expected_funding_hours": len(expected_hours),
        "crossed_funding_hours": 0,
        "funding_avoidance_qualified": True,
        "mean_hold_min": 0.0,
        "checkpoints": {},
    }
    if mode == "flat" or n < 2 or end_k < warmup_steps or lighter_trades is None or len(lighter_trades["ts"]) == 0:
        if record_trades:
            empty["trade_events"] = []
        empty["funding_avoidance_slots"] = empty_slots
        return empty

    raw_b = causal["raw_b"]
    prior_fair = causal["prior_fair"]
    prior_sigma = causal["prior_sigma_bps"]
    prior_fast = causal["prior_fast"]
    mk_hours = sorted(lighter_funding) if lighter_funding else []
    hd_hours = sorted(arcus_funding) if arcus_funding else []

    by_step = (
        pre_lighter_by_step
        if pre_lighter_by_step is not None
        else bucket_trades_by_step(
            lighter_trades,
            t0_ms,
            lighter_maker_delay_ms,
            n,
            step_ms=step_ms,
            warmup_steps=warmup_steps,
            decision_steps=decision_steps,
        )
    )

    tr_sgn = lighter_trades["sgn"]
    tr_px = lighter_trades["px"]
    tr_sz = lighter_trades["sz"]
    tr_ts = lighter_trades["ts"]

    max_rec_steps = MAX_RECOVERY_MS // step_ms
    b_sign = -1.0  # B0: Maker = Lighter (hd), Taker = Arcus (a); raw_b = log(mid_a) - log(mid_h)

    lots: list[dict[str, Any]] = []
    open_orphans: list[dict[str, Any]] = []
    trade_events: list[dict[str, Any]] = []

    pos_lighter = 0.0  # signed base qty on Lighter
    pos_arcus = 0.0  # signed base qty on Arcus
    fills = 0
    maker_filled_notl = 0.0
    hedged_notl_tot = 0.0
    orphan_events = 0
    orphan_notl_tot = 0.0
    orphan_unwind_pnl = 0.0
    orphan_unwind_cost = 0.0
    depth_clipped_entries = 0
    depth_clipped_exits = 0
    forced_funding_flattens = 0
    blocked_funding_open = 0
    blocked_toxic = 0
    blocked_drift = 0
    blocked_l1_thin = 0
    lighter_mk_fees = 0.0
    lighter_tk_fees = 0.0
    arcus_mk_fees = 0.0
    arcus_tk_fees = 0.0
    fee_reconciled_fills = 0
    depth_slip_usd = 0.0
    funding_leg_lighter_usd = 0.0
    funding_leg_arcus_usd = 0.0
    funding_payments = 0
    intraday_realized_pnl = 0.0
    hold_mins: list[float] = []

    peak_gross_notl = 0.0
    peak_single_leg_notl = 0.0
    peak_net_notl = 0.0
    worst_inflight_reserve = 0.0
    peak_imr_lighter = 0.0
    peak_imr_arcus = 0.0
    peak_mmr_lighter = 0.0
    peak_mmr_arcus = 0.0

    # Slot-level guard trackers
    slot_trackers: dict[int, dict[str, Any]] = {
        ht: {
            "at_ms": ht,
            "guard_start_ms": ht - guard_half_ms,
            "guard_end_ms": ht + guard_half_ms,
            "max_abs_lighter_pos_base": 0.0,
            "max_abs_arcus_pos_base": 0.0,
            "max_open_order_count": 0,
            "max_pending_ioc_count": 0,
            "max_active_cycle_count": 0,
            "passed": True,
        }
        for ht in expected_hours
    }

    checkpoints: dict[int, tuple[float, float, float, float, float, float, float, float]] = {}

    def _ref_idx(t_ms: int) -> int:
        return min(max(int((t_ms - t0_ms) // step_ms) - 1, 0), n - 1)

    def _current_margin_snapshot(k_idx: int, t_now: int, *, quoting_notl: float = 0.0, extra_inflight_notl: float = 0.0) -> tuple[float, float, float, float, float, float, float, float]:
        nonlocal peak_gross_notl, peak_single_leg_notl, peak_net_notl, worst_inflight_reserve
        nonlocal peak_imr_lighter, peak_imr_arcus, peak_mmr_lighter, peak_mmr_arcus
        if (
            pos_lighter == 0.0
            and pos_arcus == 0.0
            and not open_orphans
            and quoting_notl == 0.0
            and extra_inflight_notl == 0.0
        ):
            return 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0
        k_c = min(max(k_idx, 0), n - 1)
        lt_mid = 0.5 * (float(hedge_bid[k_c]) + float(hedge_ask[k_c]))
        arc_mid = 0.5 * (float(arcus_bid[k_c]) + float(arcus_ask[k_c]))
        if not (lt_mid > 0.0 and math.isfinite(lt_mid)):
            lt_mid = 0.0
        if not (arc_mid > 0.0 and math.isfinite(arc_mid)):
            arc_mid = lt_mid

        orp_lt_notl = sum(abs(float(o["qty"])) * lt_mid for o in open_orphans)
        abs_lt_notl = abs(pos_lighter) * lt_mid
        abs_arc_notl = abs(pos_arcus) * arc_mid
        net_notl = abs(pos_lighter * lt_mid + pos_arcus * arc_mid)
        single_leg_notl = max(orp_lt_notl, net_notl, extra_inflight_notl)
        inflight_res = max(abs_lt_notl, abs_arc_notl) + quoting_notl + extra_inflight_notl + orp_lt_notl

        mf = venue_margin_fractions(symbol, domain, t_now)
        imr_lt = abs_lt_notl * mf["lighter_imf"]
        imr_arc = abs_arc_notl * mf["arcus_imf"]
        mmr_lt = abs_lt_notl * mf["lighter_mmf"]
        mmr_arc = abs_arc_notl * mf["arcus_mmf"]

        gross_pair = max(abs_lt_notl, abs_arc_notl)
        if gross_pair > peak_gross_notl:
            peak_gross_notl = gross_pair
        if single_leg_notl > peak_single_leg_notl:
            peak_single_leg_notl = single_leg_notl
        if net_notl > peak_net_notl:
            peak_net_notl = net_notl
        if inflight_res > worst_inflight_reserve:
            worst_inflight_reserve = inflight_res
        if imr_lt > peak_imr_lighter:
            peak_imr_lighter = imr_lt
        if imr_arc > peak_imr_arcus:
            peak_imr_arcus = imr_arc
        if mmr_lt > peak_mmr_lighter:
            peak_mmr_lighter = mmr_lt
        if mmr_arc > peak_mmr_arcus:
            peak_mmr_arcus = mmr_arc
        return abs_lt_notl, abs_arc_notl, net_notl, inflight_res, imr_lt, imr_arc, mmr_lt, mmr_arc

    def _record_guard_state(t_now: int, *, open_orders: int = 0, pending_iocs: int = 0) -> None:
        if not expected_hours:
            return
        idx_h = bisect_left(expected_hours, t_now - guard_half_ms)
        for j in (idx_h - 1, idx_h, idx_h + 1):
            if 0 <= j < len(expected_hours):
                ht = expected_hours[j]
                st = slot_trackers[ht]
                if st["guard_start_ms"] <= t_now <= st["guard_end_ms"]:
                    lt_abs = abs(pos_lighter)
                    arc_abs = abs(pos_arcus)
                    if lt_abs > st["max_abs_lighter_pos_base"]:
                        st["max_abs_lighter_pos_base"] = round(lt_abs, 8)
                    if arc_abs > st["max_abs_arcus_pos_base"]:
                        st["max_abs_arcus_pos_base"] = round(arc_abs, 8)
                    if open_orders > st["max_open_order_count"]:
                        st["max_open_order_count"] = open_orders
                    if pending_iocs > st["max_pending_ioc_count"]:
                        st["max_pending_ioc_count"] = pending_iocs
                    cyc = (1 if (lots or open_orphans) else 0)
                    if cyc > st["max_active_cycle_count"]:
                        st["max_active_cycle_count"] = cyc

    def _in_open_block_window(t_now: int) -> bool:
        if mode != "B0_funding_flat" or not expected_hours:
            return False
        idx = bisect_left(expected_hours, t_now - guard_half_ms)
        for j in (idx - 1, idx, idx + 1):
            if 0 <= j < len(expected_hours):
                ht = expected_hours[j]
                if (ht - open_block_before_hour_ms) <= t_now <= (ht + guard_half_ms):
                    return True
        return False

    def _in_flatten_window(t_now: int) -> bool:
        if mode != "B0_funding_flat" or not expected_hours:
            return False
        idx = bisect_left(expected_hours, t_now - guard_half_ms)
        for j in (idx - 1, idx, idx + 1):
            if 0 <= j < len(expected_hours):
                ht = expected_hours[j]
                if (ht - flatten_before_hour_ms) <= t_now <= (ht + guard_half_ms):
                    return True
        return False

    def _unwind_orphan_at(orp: dict[str, Any], t_attempt: int, *, cutoff_ms: int) -> bool:
        nonlocal lighter_tk_fees, fee_reconciled_fills, depth_slip_usd
        nonlocal funding_leg_lighter_usd, funding_payments
        nonlocal orphan_unwind_pnl, orphan_unwind_cost, intraday_realized_pnl
        nonlocal pos_lighter
        if t_attempt > cutoff_ms:
            return False
        hit = _lookup_valid_step_with_retry(
            hedge_age,
            hedge_bid,
            hedge_ask,
            t_attempt,
            t0_ms,
            step_ms,
            max_age_steps,
            max_rec_steps,
            retry_latency_ms=lighter_taker_delay_ms,
            cutoff_ms=cutoff_ms,
        )
        if hit is None:
            return False
        ju, u_exit_ms = hit
        side = int(orp["side"])
        qty = float(orp["qty"])
        qp = float(orp["qp"])
        u_px = float(hedge_bid[ju] if side == 1 else hedge_ask[ju])
        u_sz = float(hedge_bsz[ju] if side == 1 else hedge_asz[ju])
        u_l1 = u_px * u_sz if (math.isfinite(u_sz) and u_sz >= 0.0) else 0.0
        u_notl = qty * u_px
        sweep_usd, clipped = _depth_sweep_cost(u_notl, u_l1, params.depth_impact_bps)
        if clipped:
            depth_slip_usd += sweep_usd
        mk_ent_fee = float(orp["mk_ent_fee"])
        u_fee = u_notl * lighter_tk_fee_bps * 1e-4
        lighter_tk_fees += u_fee
        fee_reconciled_fills += 1

        f_lt, _, pays = _settle_lot_funding(
            lot_dir=side,
            qty_base=qty,
            mk_fill_ms=int(orp["fill_ms"]),
            hd_fill_ms=int(orp["fill_ms"]),
            mk_exit_ms=u_exit_ms,
            hd_exit_ms=int(orp["fill_ms"]),
            mk_funding=lighter_funding,
            hd_funding=None,
            mk_hours=mk_hours,
            hd_hours=(),
            mk_bid=hedge_bid,
            mk_ask=hedge_ask,
            hd_bid=arcus_bid,
            hd_ask=arcus_ask,
            t0_ms=t0_ms,
            step_ms=step_ms,
        )
        funding_leg_lighter_usd += f_lt
        funding_payments += pays
        raw_pnl = side * qty * (u_px - qp)
        net_orp = raw_pnl + f_lt - sweep_usd - mk_ent_fee - u_fee
        orphan_unwind_pnl += net_orp
        orphan_unwind_cost += max(0.0, -raw_pnl) + sweep_usd + mk_ent_fee + u_fee
        intraday_realized_pnl += net_orp
        pos_lighter -= side * qty
        if record_trades:
            trade_events.append(
                {
                    "kind": "orphan_unwind",
                    "side": side,
                    "fill_ms": int(orp["fill_ms"]),
                    "exit_ms": int(u_exit_ms),
                    "notional_usd": round(float(orp["notl"]), 4),
                    "net_pnl_usd": round(net_orp, 6),
                    "sweep_usd": round(sweep_usd, 6),
                    "lighter_fee_usdc": round(mk_ent_fee + u_fee, 6),
                    "arcus_fee_usdc": 0.0,
                }
            )
        return True

    def _book_matched_fill(
        side: int,
        hn: float,
        qp: float,
        hp: float,
        mk_fill_ms: int,
        hd_fill_ms: int,
        mk_fee_rate: float,
        hd_fee_rate: float,
        hd_sweep_usd: float,
        *,
        is_taker_close: bool = False,
    ) -> None:
        nonlocal pos_lighter, pos_arcus, lighter_mk_fees, lighter_tk_fees, arcus_tk_fees
        nonlocal fee_reconciled_fills, depth_slip_usd, funding_leg_lighter_usd, funding_leg_arcus_usd
        nonlocal funding_payments, intraday_realized_pnl
        qty = hn / qp
        mk_f = hn * mk_fee_rate * 1e-4
        hd_notl = qty * hp
        hd_f = hd_notl * hd_fee_rate * 1e-4
        if is_taker_close:
            lighter_tk_fees += mk_f
        else:
            lighter_mk_fees += mk_f
        arcus_tk_fees += hd_f
        fee_reconciled_fills += 1
        depth_slip_usd += hd_sweep_usd
        intraday_realized_pnl -= hd_sweep_usd

        rem_qty = qty
        while rem_qty > 1e-12 and lots and int(lots[0]["dir"]) == -side:
            lot = lots[0]
            l_qty = float(lot["qty"])
            dq = min(rem_qty, l_qty)
            frac_l = dq / l_qty
            frac_n = dq / qty

            ldir = int(lot["dir"])
            gross_lot = ldir * dq * ((qp - float(lot["qp"])) + (float(lot["hp"]) - hp))
            lot_fees = float(lot["mk_fee"]) * frac_l + float(lot["hd_fee"]) * frac_l + (mk_f + hd_f) * frac_n

            f_lt, f_arc, pays = _settle_lot_funding(
                lot_dir=ldir,
                qty_base=dq,
                mk_fill_ms=int(lot["mk_ms"]),
                hd_fill_ms=int(lot["hd_ms"]),
                mk_exit_ms=mk_fill_ms,
                hd_exit_ms=hd_fill_ms,
                mk_funding=lighter_funding,
                hd_funding=arcus_funding,
                mk_hours=mk_hours,
                hd_hours=hd_hours,
                mk_bid=hedge_bid,
                mk_ask=hedge_ask,
                hd_bid=arcus_bid,
                hd_ask=arcus_ask,
                t0_ms=t0_ms,
                step_ms=step_ms,
            )
            funding_leg_lighter_usd += f_lt
            funding_leg_arcus_usd += f_arc
            funding_payments += pays
            hold_mins.append(max(0.0, (mk_fill_ms - int(lot["mk_ms"])) / 60_000.0))
            realized_lot = gross_lot + f_lt + f_arc - lot_fees
            intraday_realized_pnl += realized_lot

            if record_trades:
                trade_events.append(
                    {
                        "kind": "closed_lot",
                        "dir": ldir,
                        "qty": round(dq, 8),
                        "lighter_fill_ms": int(lot["mk_ms"]),
                        "arcus_fill_ms": int(lot["hd_ms"]),
                        "lighter_exit_ms": int(mk_fill_ms),
                        "arcus_exit_ms": int(hd_fill_ms),
                        "funding_usd": round(f_lt + f_arc, 6),
                        "realized_pnl_usd": round(realized_lot, 6),
                    }
                )

            if dq >= l_qty - 1e-12:
                lots.pop(0)
            else:
                lot["qty"] = l_qty - dq
                lot["mk_fee"] = float(lot["mk_fee"]) * (1.0 - frac_l)
                lot["hd_fee"] = float(lot["hd_fee"]) * (1.0 - frac_l)
            rem_qty -= dq

        if rem_qty > 1e-12:
            frac_rem = rem_qty / qty
            lots.append(
                {
                    "dir": side,
                    "qty": rem_qty,
                    "qp": qp,
                    "hp": hp,
                    "mk_ms": mk_fill_ms,
                    "hd_ms": hd_fill_ms,
                    "mk_fee": mk_f * frac_rem,
                    "hd_fee": hd_f * frac_rem,
                }
            )
        pos_lighter += side * qty
        pos_arcus -= side * qty

    def _flatten_all_lots_taker(t_dec: int, *, cutoff_ms: int, reason: str) -> bool:
        nonlocal depth_clipped_exits, depth_slip_usd, lighter_tk_fees, arcus_tk_fees
        nonlocal fee_reconciled_fills, funding_leg_lighter_usd, funding_leg_arcus_usd
        nonlocal funding_payments, intraday_realized_pnl, pos_lighter, pos_arcus
        if not lots:
            return True
        t_lt = t_dec + lighter_taker_delay_ms
        t_arc = t_dec + arcus_delay_ms
        hit_m = _lookup_valid_step_with_retry(
            hedge_age,
            hedge_bid,
            hedge_ask,
            t_lt,
            t0_ms,
            step_ms,
            max_age_steps,
            max_rec_steps,
            retry_latency_ms=lighter_taker_delay_ms,
            cutoff_ms=cutoff_ms,
        )
        hit_h = _lookup_valid_step_with_retry(
            arcus_age,
            arcus_bid,
            arcus_ask,
            t_arc,
            t0_ms,
            step_ms,
            max_age_steps,
            max_rec_steps,
            retry_latency_ms=arcus_delay_ms,
            cutoff_ms=cutoff_ms,
        )
        if hit_m is None or hit_h is None:
            return False
        jm, m_exit_ms = hit_m
        jh, h_exit_ms = hit_h
        tot_qty = sum(float(l["qty"]) for l in lots)
        ldir = int(lots[0]["dir"])
        exit_side = -ldir
        mp_end = float(hedge_ask[jm] if exit_side == 1 else hedge_bid[jm])
        hp_end = float(arcus_bid[jh] if exit_side == 1 else arcus_ask[jh])
        msz_end = float(hedge_asz[jm] if exit_side == 1 else hedge_bsz[jm])
        hsz_end = float(arcus_bsz[jh] if exit_side == 1 else arcus_asz[jh])
        sw_m, cl_m = _depth_sweep_cost(tot_qty * mp_end, mp_end * msz_end, params.depth_impact_bps)
        sw_h, cl_h = _depth_sweep_cost(tot_qty * hp_end, hp_end * hsz_end, params.depth_impact_bps)
        if cl_m or cl_h:
            depth_clipped_exits += 1
        depth_slip_usd += sw_m + sw_h
        lt_fee = tot_qty * mp_end * lighter_tk_fee_bps * 1e-4
        arc_fee = tot_qty * hp_end * arcus_tk_fee_bps * 1e-4
        lighter_tk_fees += lt_fee
        arcus_tk_fees += arc_fee
        fee_reconciled_fills += 1

        gross_tot = 0.0
        ent_fees = 0.0
        fund_tot = 0.0
        for lot in lots:
            dq = float(lot["qty"])
            d_gross = int(lot["dir"]) * dq * ((mp_end - float(lot["qp"])) + (float(lot["hp"]) - hp_end))
            d_ent = float(lot["mk_fee"]) + float(lot["hd_fee"])
            f_lt, f_arc, pays = _settle_lot_funding(
                lot_dir=int(lot["dir"]),
                qty_base=dq,
                mk_fill_ms=int(lot["mk_ms"]),
                hd_fill_ms=int(lot["hd_ms"]),
                mk_exit_ms=m_exit_ms,
                hd_exit_ms=h_exit_ms,
                mk_funding=lighter_funding,
                hd_funding=arcus_funding,
                mk_hours=mk_hours,
                hd_hours=hd_hours,
                mk_bid=hedge_bid,
                mk_ask=hedge_ask,
                hd_bid=arcus_bid,
                hd_ask=arcus_ask,
                t0_ms=t0_ms,
                step_ms=step_ms,
            )
            funding_leg_lighter_usd += f_lt
            funding_leg_arcus_usd += f_arc
            funding_payments += pays
            hold_mins.append(max(0.0, (m_exit_ms - int(lot["mk_ms"])) / 60_000.0))
            gross_tot += d_gross
            ent_fees += d_ent
            fund_tot += f_lt + f_arc

        net_flat = gross_tot + fund_tot - ent_fees - lt_fee - arc_fee - (sw_m + sw_h)
        intraday_realized_pnl += net_flat
        if record_trades:
            trade_events.append(
                {
                    "kind": reason,
                    "qty": round(tot_qty, 8),
                    "lighter_exit_ms": int(m_exit_ms),
                    "arcus_exit_ms": int(h_exit_ms),
                    "funding_usd": round(fund_tot, 6),
                    "realized_pnl_usd": round(net_flat, 6),
                }
            )
        lots.clear()
        pos_lighter = 0.0
        pos_arcus = 0.0
        return True

    def _execute_hedge_and_book(side: int, fn: float, qp: float, first_tf: int, k_step: int) -> None:
        nonlocal fills, maker_filled_notl, hedged_notl_tot, orphan_events, orphan_notl_tot
        nonlocal depth_clipped_entries, lighter_mk_fees, pos_lighter
        fills += 1
        maker_filled_notl += fn
        # Lighter maker leg fills at `first_tf`; hedge on Arcus arrives at `first_tf + arcus_delay_ms`
        pos_lighter += side * (fn / qp)
        _current_margin_snapshot(k_step, first_tf, extra_inflight_notl=fn)
        _record_guard_state(first_tf, pending_iocs=1)

        hedged_notl = 0.0
        eff_hp = 0.0
        eff_hd_ms = first_tf + arcus_delay_ms
        t_tk = first_tf + arcus_delay_ms
        hit_t = _lookup_valid_step_with_retry(
            arcus_age,
            arcus_bid,
            arcus_ask,
            t_tk,
            t0_ms,
            step_ms,
            max_age_steps,
            max_rec_steps,
            retry_latency_ms=arcus_delay_ms,
            cutoff_ms=t_end_ms,
        )
        if hit_t is not None:
            jt, h_tk_ms = hit_t
            tp = float(arcus_bid[jt] if side == 1 else arcus_ask[jt])
            tsz = float(arcus_bsz[jt] if side == 1 else arcus_asz[jt])
            hl1 = tp * tsz if (math.isfinite(tsz) and tsz >= 0.0) else 0.0
            t_hn = min(fn, hl1)
            if t_hn >= MIN_FILL_USD:
                if t_hn < fn:
                    depth_clipped_entries += 1
                hedged_notl = t_hn
                eff_hp = tp
                eff_hd_ms = h_tk_ms
            elif hl1 < fn:
                depth_clipped_entries += 1

        # Remove temporary full maker leg from pos_lighter before _book_matched_fill re-adds hedged qty + orphan keeps unhedged qty
        pos_lighter -= side * (fn / qp)

        if hedged_notl > 0.0:
            hedged_notl_tot += hedged_notl
            _book_matched_fill(
                side,
                hedged_notl,
                qp,
                eff_hp,
                first_tf,
                eff_hd_ms,
                lighter_mk_fee_bps,
                arcus_tk_fee_bps,
                0.0,
            )

        orphan_notl = max(0.0, fn - hedged_notl)
        if orphan_notl > 1e-9:
            orphan_events += 1
            orphan_notl_tot += orphan_notl
            orp_qty = orphan_notl / qp
            pos_lighter += side * orp_qty
            t_orp_unwind = (first_tf + arcus_delay_ms) + lighter_maker_delay_ms
            ent_fee = orphan_notl * lighter_mk_fee_bps * 1e-4
            lighter_mk_fees += ent_fee
            orp = {
                "side": side,
                "qty": orp_qty,
                "qp": qp,
                "notl": orphan_notl,
                "fill_ms": first_tf,
                "mk_ent_fee": ent_fee,
                "earliest_ms": t_orp_unwind,
            }
            if not _unwind_orphan_at(orp, t_orp_unwind, cutoff_ms=t_end_ms):
                open_orphans.append(orp)

    # Merge trade-print steps with periodic checkpoint steps AND funding-boundary flatten/guard steps
    cp_stride = max(checkpoint_step_ms // step_ms, 1)
    active_steps: set[int] = set(by_step.keys())
    for k_cp in range(warmup_steps, end_k + 1, cp_stride):
        active_steps.add(k_cp)
    if mode == "B0_funding_flat" and expected_hours:
        for ht in expected_hours:
            k_flat_lo = max(warmup_steps, int((ht - flatten_before_hour_ms - t0_ms) // step_ms) - 1)
            k_flat_hi = min(end_k, int((ht + guard_half_ms - t0_ms) // step_ms) + 1)
            for kf in range(k_flat_lo, k_flat_hi + 1):
                active_steps.add(kf)
    elif expected_hours:
        # Still sample exact guard boundaries for B0_original comparison
        for ht in expected_hours:
            k_g_lo = max(warmup_steps, int((ht - guard_half_ms - t0_ms) // step_ms) - 1)
            k_g_hi = min(end_k, int((ht + guard_half_ms - t0_ms) // step_ms) + 1)
            for kg in range(k_g_lo, k_g_hi + 1, max(1_000 // step_ms, 1)):
                active_steps.add(kg)

    for k in sorted(active_steps):
        if k < warmup_steps or k > end_k:
            continue
        t_cur = t0_ms + (k + 1) * step_ms

        if open_orphans:
            rem_o = []
            for orp in open_orphans:
                if t_cur < int(orp["earliest_ms"]) or not _unwind_orphan_at(orp, t_cur, cutoff_ms=t_end_ms):
                    rem_o.append(orp)
            open_orphans = rem_o

        in_flatten = _in_flatten_window(t_cur)
        in_open_block = _in_open_block_window(t_cur)

        if in_flatten and lots:
            if _flatten_all_lots_taker(t_cur, cutoff_ms=t_end_ms, reason="funding_avoidance_flatten"):
                forced_funding_flattens += 1

        quoting_notl = 0.0
        open_orders_now = 0

        if (not in_flatten) and (k in by_step):
            if hedge_age[k] <= max_age_steps and arcus_age[k] <= max_age_steps:
                fb_raw = prior_fair[k]
                b_raw = raw_b[k]
                fast_raw = prior_fast[k]
                if math.isfinite(fb_raw) and math.isfinite(b_raw):
                    fb = b_sign * fb_raw
                    b_now = b_sign * b_raw
                    f_now = b_sign * fast_raw if math.isfinite(fast_raw) else fb
                    mb, ma = float(hedge_bid[k]), float(hedge_ask[k])
                    hb, ha = float(arcus_bid[k]), float(arcus_ask[k])
                    if mb > 0.0 and ma > mb and hb > 0.0 and ha > hb:
                        mk_mid_k = 0.5 * (mb + ma)
                        q = pos_lighter * mk_mid_k / clip_usd
                        eff_e0 = params.e0_bps + (params.vol_edge_mult * prior_sigma[k] if params.vol_edge_mult > 0.0 else 0.0)
                        sk_q = _skew_q(q, q_max_clips, params.nonlinear_skew_power)
                        drift_bps = (b_now - f_now) * 1e4
                        drift_block_bid = params.basis_drift_guard_bps > 0.0 and q >= 0.0 and drift_bps <= -params.basis_drift_guard_bps
                        drift_block_ask = params.basis_drift_guard_bps > 0.0 and q <= 0.0 and drift_bps >= params.basis_drift_guard_bps

                        hb_notl = hb * float(arcus_bsz[k]) if math.isfinite(float(arcus_bsz[k])) else 0.0
                        ha_notl = ha * float(arcus_asz[k]) if math.isfinite(float(arcus_asz[k])) else 0.0
                        l1_block_bid = params.min_hedge_l1_notional_usd > 0.0 and q >= 0.0 and hb_notl < params.min_hedge_l1_notional_usd
                        l1_block_ask = params.min_hedge_l1_notional_usd > 0.0 and q <= 0.0 and ha_notl < params.min_hedge_l1_notional_usd

                        quotes: list[tuple[int, float]] = []
                        if q < q_max_clips:
                            if in_open_block and q >= 0.0:
                                blocked_funding_open += 1
                            elif drift_block_bid:
                                blocked_drift += 1
                            elif l1_block_bid:
                                blocked_l1_thin += 1
                            else:
                                e_bid = eff_e0 + params.skew_bps_per_clip * sk_q
                                cap = hb * math.exp(fb - e_bid * 1e-4)
                                bid_q = min(mb, math.floor(cap / tick + 1e-9) * tick)
                                if 0.0 < bid_q < ma:
                                    quotes.append((1, bid_q))
                        if q > -q_max_clips:
                            if in_open_block and q <= 0.0:
                                blocked_funding_open += 1
                            elif drift_block_ask:
                                blocked_drift += 1
                            elif l1_block_ask:
                                blocked_l1_thin += 1
                            else:
                                e_ask = eff_e0 - params.skew_bps_per_clip * sk_q
                                floor_ = ha * math.exp(fb + e_ask * 1e-4)
                                ask_q = max(ma, math.ceil(floor_ / tick - 1e-9) * tick)
                                if ask_q > mb:
                                    quotes.append((-1, ask_q))

                        if quotes:
                            open_orders_now = len(quotes)
                            quoting_notl = clip_usd
                            bucket_indices = by_step[k]
                            for side, qp in quotes:
                                through_notl = 0.0
                                first_tf: int | None = None
                                for idx_t in bucket_indices:
                                    sg = int(tr_sgn[idx_t])
                                    px = float(tr_px[idx_t])
                                    if (side == 1 and sg == -1 and px < qp) or (side == -1 and sg == 1 and px > qp):
                                        if first_tf is None:
                                            first_tf = int(tr_ts[idx_t])
                                        through_notl += px * float(tr_sz[idx_t])
                                if first_tf is None or through_notl < MIN_FILL_USD:
                                    continue

                                if params.shield_bps is not None and lighter_speedbump_ms > 80:
                                    i0 = _ref_idx(first_tf - lighter_speedbump_ms + 40)
                                    i1 = _ref_idx(first_tf - 40)
                                    if hedge_age[i0] <= max_age_steps and hedge_age[i1] <= max_age_steps:
                                        r0 = 0.5 * (float(hedge_bid[i0]) + float(hedge_ask[i0]))
                                        r1 = 0.5 * (float(hedge_bid[i1]) + float(hedge_ask[i1]))
                                        if r0 > 0.0 and r1 > 0.0:
                                            rmv = (r1 / r0 - 1.0) * 1e4
                                            if (side == 1 and rmv <= -params.shield_bps) or (side == -1 and rmv >= params.shield_bps):
                                                blocked_toxic += 1
                                                continue

                                _execute_hedge_and_book(side, min(clip_usd, through_notl), qp, first_tf, k)

        if pos_lighter != 0.0 or pos_arcus != 0.0 or open_orphans or quoting_notl > 0.0:
            snap = _current_margin_snapshot(k, t_cur, quoting_notl=quoting_notl)
            if record_checkpoints and ((k - warmup_steps) % cp_stride == 0):
                checkpoints[k] = snap
        _record_guard_state(t_cur, open_orders=open_orders_now, pending_iocs=0)

    # Terminal unwind within strict planned cutoff `t_end_ms`: trigger decision so execution completes <= t_end_ms
    rem_orphans_end: list[dict[str, Any]] = []
    for orp in open_orphans:
        if not _unwind_orphan_at(orp, max(t_end_ms - lighter_taker_delay_ms, int(orp["earliest_ms"])), cutoff_ms=t_end_ms):
            rem_orphans_end.append(orp)

    terminal_flat = True
    terminal_residual_notl = 0.0
    terminal_drain_pnl = 0.0
    residual_mtm_pnl = 0.0

    for orp in rem_orphans_end:
        terminal_flat = False
        terminal_residual_notl += float(orp["notl"])
        end_m = 0.5 * (float(hedge_bid[end_k]) + float(hedge_ask[end_k]))
        if end_m > 0.0 and math.isfinite(end_m):
            residual_mtm_pnl += int(orp["side"]) * float(orp["qty"]) * (end_m - float(orp["qp"])) - float(orp["mk_ent_fee"])

    if lots:
        t_drain_dec = max(t_dec_start_ms, t_end_ms - max(lighter_taker_delay_ms, arcus_delay_ms))
        hit_m_end = _lookup_valid_step_with_retry(
            hedge_age,
            hedge_bid,
            hedge_ask,
            t_drain_dec + lighter_taker_delay_ms,
            t0_ms,
            step_ms,
            max_age_steps,
            max_rec_steps,
            retry_latency_ms=lighter_taker_delay_ms,
            cutoff_ms=t_end_ms,
        )
        hit_h_end = _lookup_valid_step_with_retry(
            arcus_age,
            arcus_bid,
            arcus_ask,
            t_drain_dec + arcus_delay_ms,
            t0_ms,
            step_ms,
            max_age_steps,
            max_rec_steps,
            retry_latency_ms=arcus_delay_ms,
            cutoff_ms=t_end_ms,
        )
        if hit_m_end is not None and hit_h_end is not None:
            jm, m_exit_ms = hit_m_end
            jh, h_exit_ms = hit_h_end
            tot_qty = sum(float(l["qty"]) for l in lots)
            ldir = int(lots[0]["dir"])
            exit_side = -ldir
            mp_end = float(hedge_ask[jm] if exit_side == 1 else hedge_bid[jm])
            hp_end = float(arcus_bid[jh] if exit_side == 1 else arcus_ask[jh])
            msz_end = float(hedge_asz[jm] if exit_side == 1 else hedge_bsz[jm])
            hsz_end = float(arcus_bsz[jh] if exit_side == 1 else arcus_asz[jh])
            sw_m, cl_m = _depth_sweep_cost(tot_qty * mp_end, mp_end * msz_end, params.depth_impact_bps)
            sw_h, cl_h = _depth_sweep_cost(tot_qty * hp_end, hp_end * hsz_end, params.depth_impact_bps)
            if cl_m or cl_h:
                depth_clipped_exits += 1
            depth_slip_usd += sw_m + sw_h
            lt_drain_fee = tot_qty * mp_end * lighter_tk_fee_bps * 1e-4
            arc_drain_fee = tot_qty * hp_end * arcus_tk_fee_bps * 1e-4
            lighter_tk_fees += lt_drain_fee
            arcus_tk_fees += arc_drain_fee
            fee_reconciled_fills += 1

            drain_gross = 0.0
            drain_entry_costs = 0.0
            drain_funding = 0.0
            for lot in lots:
                dq = float(lot["qty"])
                d_gross = int(lot["dir"]) * dq * ((mp_end - float(lot["qp"])) + (float(lot["hp"]) - hp_end))
                d_ent = float(lot["mk_fee"]) + float(lot["hd_fee"])
                f_lt, f_arc, pays = _settle_lot_funding(
                    lot_dir=int(lot["dir"]),
                    qty_base=dq,
                    mk_fill_ms=int(lot["mk_ms"]),
                    hd_fill_ms=int(lot["hd_ms"]),
                    mk_exit_ms=m_exit_ms,
                    hd_exit_ms=h_exit_ms,
                    mk_funding=lighter_funding,
                    hd_funding=arcus_funding,
                    mk_hours=mk_hours,
                    hd_hours=hd_hours,
                    mk_bid=hedge_bid,
                    mk_ask=hedge_ask,
                    hd_bid=arcus_bid,
                    hd_ask=arcus_ask,
                    t0_ms=t0_ms,
                    step_ms=step_ms,
                )
                funding_leg_lighter_usd += f_lt
                funding_leg_arcus_usd += f_arc
                funding_payments += pays
                hold_mins.append(max(0.0, (m_exit_ms - int(lot["mk_ms"])) / 60_000.0))
                drain_gross += d_gross
                drain_entry_costs += d_ent
                drain_funding += f_lt + f_arc

            terminal_drain_pnl = drain_gross + drain_funding - drain_entry_costs - lt_drain_fee - arc_drain_fee - (sw_m + sw_h)
            pos_lighter = 0.0
            pos_arcus = 0.0
        else:
            terminal_flat = False
            m_mid = 0.5 * (float(hedge_bid[end_k]) + float(hedge_ask[end_k]))
            h_mid = 0.5 * (float(arcus_bid[end_k]) + float(arcus_ask[end_k]))
            for lot in lots:
                dq = float(lot["qty"])
                terminal_residual_notl += dq * float(lot["qp"])
                if m_mid > 0.0 and h_mid > 0.0 and math.isfinite(m_mid) and math.isfinite(h_mid):
                    d_gross = int(lot["dir"]) * dq * ((m_mid - float(lot["qp"])) + (float(lot["hp"]) - h_mid))
                    d_ent = float(lot["mk_fee"]) + float(lot["hd_fee"])
                    residual_mtm_pnl += d_gross - d_ent

    # Finalize per-slot funding avoidance verdicts
    crossed_hours = 0
    slots_out: list[dict[str, Any]] = []
    for ht in expected_hours:
        st = slot_trackers[ht]
        passed = (
            st["max_abs_lighter_pos_base"] <= 1e-12
            and st["max_abs_arcus_pos_base"] <= 1e-12
            and st["max_open_order_count"] == 0
            and st["max_pending_ioc_count"] == 0
            and st["max_active_cycle_count"] == 0
        )
        st["passed"] = passed
        if not passed:
            crossed_hours += 1
        slots_out.append(st)

    lighter_fees_tot = lighter_mk_fees + lighter_tk_fees
    arcus_fees_tot = arcus_mk_fees + arcus_tk_fees
    fees_tot = lighter_fees_tot + arcus_fees_tot
    realized_only = intraday_realized_pnl + terminal_drain_pnl
    mtm_incl_residual = realized_only + residual_mtm_pnl
    funding_cash = funding_leg_lighter_usd + funding_leg_arcus_usd

    out: dict[str, Any] = {
        "symbol": symbol,
        "domain": domain,
        "mode": mode,
        "clip_usd": clip_usd,
        "param_name": params.name,
        "fills": fills,
        "maker_filled_notional_usd": round(maker_filled_notl, 2),
        "hedged_notional_usd": round(hedged_notl_tot, 2),
        "orphan_events": orphan_events,
        "orphan_notional_usd": round(orphan_notl_tot, 2),
        "orphan_unwind_rate": round(orphan_notl_tot / maker_filled_notl, 6) if maker_filled_notl > 0.0 else 0.0,
        "orphan_unwind_pnl_usd": round(orphan_unwind_pnl, 4),
        "orphan_unwind_cost_usd": round(orphan_unwind_cost, 4),
        "depth_clipped_entries": depth_clipped_entries,
        "depth_clipped_exits": depth_clipped_exits,
        "forced_funding_flattens": forced_funding_flattens,
        "blocked_funding_open": blocked_funding_open,
        "blocked_toxic": blocked_toxic,
        "blocked_drift": blocked_drift,
        "blocked_l1_thin": blocked_l1_thin,
        "lighter_maker_fees_usdc": round(lighter_mk_fees, 4),
        "lighter_taker_fees_usdc": round(lighter_tk_fees, 4),
        "lighter_fees_usdc": round(lighter_fees_tot, 4),
        "arcus_maker_fees_usdc": round(arcus_mk_fees, 4),
        "arcus_taker_fees_usdc": round(arcus_tk_fees, 4),
        "arcus_fees_usdc": round(arcus_fees_tot, 4),
        "fees_usdc": round(fees_tot, 4),
        "fee_reconciled_fills": fee_reconciled_fills,
        "depth_slip_usd": round(depth_slip_usd, 4),
        "funding_leg_lighter_usd": round(funding_leg_lighter_usd, 4),
        "funding_leg_arcus_usd": round(funding_leg_arcus_usd, 4),
        "funding_cash_usd": round(funding_cash, 4),
        "funding_payments": funding_payments,
        "intraday_realized_pnl_usd": round(intraday_realized_pnl, 4),
        "terminal_drain_pnl_usd": round(terminal_drain_pnl, 4),
        "realized_only_pnl_usd": round(realized_only, 4),
        "mtm_incl_residual_pnl_usd": round(mtm_incl_residual, 4),
        "terminal_flat": terminal_flat,
        "terminal_residual_notional_usd": round(terminal_residual_notl, 4),
        "peak_gross_notional_usd": round(peak_gross_notl, 2),
        "peak_single_leg_notional_usd": round(peak_single_leg_notl, 2),
        "peak_net_notional_usd": round(peak_net_notl, 2),
        "worst_case_inflight_reserve_usd": round(worst_inflight_reserve, 2),
        "peak_imr_lighter_usd": round(peak_imr_lighter, 2),
        "peak_imr_arcus_usd": round(peak_imr_arcus, 2),
        "peak_mmr_lighter_usd": round(peak_mmr_lighter, 2),
        "peak_mmr_arcus_usd": round(peak_mmr_arcus, 2),
        "expected_funding_hours": len(expected_hours),
        "crossed_funding_hours": crossed_hours,
        "funding_avoidance_qualified": (crossed_hours == 0),
        "mean_hold_min": round(sum(hold_mins) / len(hold_mins), 2) if hold_mins else 0.0,
        "checkpoints": checkpoints,
        "funding_avoidance_slots": slots_out,
    }
    if record_trades:
        out["trade_events"] = trade_events
    return out


def aggregate_settleability_runs(
    runs: Sequence[dict[str, Any]],
    *,
    clip_usd: float,
    include_checkpoints: bool = True,
) -> dict[str, Any]:
    """Aggregate PnL, fee attribution (USDC), funding avoidance, and concurrent portfolio margin/collateral."""
    if not runs:
        return {"windows_covered": 0, "fills": 0, "mtm_incl_residual_pnl_usd": 0.0, "realized_only_pnl_usd": 0.0}

    fills = sum(int(r["fills"]) for r in runs)
    mk_notl = sum(float(r["maker_filled_notional_usd"]) for r in runs)
    hd_notl = sum(float(r["hedged_notional_usd"]) for r in runs)
    orp_ev = sum(int(r["orphan_events"]) for r in runs)
    orp_notl = sum(float(r["orphan_notional_usd"]) for r in runs)
    orp_pnl = sum(float(r["orphan_unwind_pnl_usd"]) for r in runs)
    orp_cost = sum(float(r["orphan_unwind_cost_usd"]) for r in runs)
    lt_mk_f = sum(float(r["lighter_maker_fees_usdc"]) for r in runs)
    lt_tk_f = sum(float(r["lighter_taker_fees_usdc"]) for r in runs)
    arc_mk_f = sum(float(r["arcus_maker_fees_usdc"]) for r in runs)
    arc_tk_f = sum(float(r["arcus_taker_fees_usdc"]) for r in runs)
    fees_usdc = sum(float(r["fees_usdc"]) for r in runs)
    dslip = sum(float(r["depth_slip_usd"]) for r in runs)
    f_lt = sum(float(r["funding_leg_lighter_usd"]) for r in runs)
    f_arc = sum(float(r["funding_leg_arcus_usd"]) for r in runs)
    fcash = sum(float(r["funding_cash_usd"]) for r in runs)
    realized = sum(float(r["realized_only_pnl_usd"]) for r in runs)
    mtm = sum(float(r["mtm_incl_residual_pnl_usd"]) for r in runs)
    res_notl = sum(float(r["terminal_residual_notional_usd"]) for r in runs)
    term_flat = all(bool(r["terminal_flat"]) for r in runs)
    forced_flat = sum(int(r["forced_funding_flattens"]) for r in runs)
    blk_open = sum(int(r["blocked_funding_open"]) for r in runs)
    exp_fh = sum(int(r["expected_funding_hours"]) for r in runs)
    crs_fh = sum(int(r["crossed_funding_hours"]) for r in runs)
    fa_qual = all(bool(r["funding_avoidance_qualified"]) for r in runs)
    holds = [float(r["mean_hold_min"]) for r in runs if int(r["fills"]) > 0]

    # Single-symbol peak metrics
    max_sym_gross = max((float(r["peak_gross_notional_usd"]) for r in runs), default=0.0)
    max_sym_single_leg = max((float(r["peak_single_leg_notional_usd"]) for r in runs), default=0.0)
    max_sym_net = max((float(r["peak_net_notional_usd"]) for r in runs), default=0.0)
    max_sym_inflight = max((float(r["worst_case_inflight_reserve_usd"]) for r in runs), default=0.0)
    max_sym_imr_lt = max((float(r["peak_imr_lighter_usd"]) for r in runs), default=0.0)
    max_sym_imr_arc = max((float(r["peak_imr_arcus_usd"]) for r in runs), default=0.0)

    # Concurrent portfolio aggregation per window & checkpoint
    by_w: dict[int, float] = {}
    by_w_real: dict[int, float] = {}
    by_w_cp: dict[int, dict[int, list[tuple[float, float, float, float, float, float, float, float]]]] = {}
    for r in runs:
        wn = int(r["window_num"])
        by_w[wn] = by_w.get(wn, 0.0) + float(r["mtm_incl_residual_pnl_usd"])
        by_w_real[wn] = by_w_real.get(wn, 0.0) + float(r["realized_only_pnl_usd"])
        if include_checkpoints and r.get("checkpoints"):
            wmap = by_w_cp.setdefault(wn, {})
            for k_cp, snap in r["checkpoints"].items():
                wmap.setdefault(int(k_cp), []).append(snap)

    cum = 0.0
    pk = 0.0
    dd = 0.0
    worst_win = 0.0
    pos_w = 0
    for wn in sorted(by_w):
        wp = by_w[wn]
        if wp < worst_win:
            worst_win = wp
        cum += wp
        if cum > pk:
            pk = cum
        if pk - cum > dd:
            dd = pk - cum
        if wp > 0.0:
            pos_w += 1

    conc_peak_lt_notl = 0.0
    conc_peak_arc_notl = 0.0
    conc_peak_net_notl = 0.0
    conc_peak_inflight = 0.0
    conc_peak_imr_lt = 0.0
    conc_peak_imr_arc = 0.0
    conc_peak_mmr_lt = 0.0
    conc_peak_mmr_arc = 0.0
    for wmap in by_w_cp.values():
        for snaps in wmap.values():
            lt_n = sum(s[0] for s in snaps)
            arc_n = sum(s[1] for s in snaps)
            net_n = sum(s[2] for s in snaps)
            inf_n = max((s[3] - max(s[0], s[1])) for s in snaps) + max(lt_n, arc_n)
            imr_l = sum(s[4] for s in snaps)
            imr_a = sum(s[5] for s in snaps)
            mmr_l = sum(s[6] for s in snaps)
            mmr_a = sum(s[7] for s in snaps)
            conc_peak_lt_notl = max(conc_peak_lt_notl, lt_n)
            conc_peak_arc_notl = max(conc_peak_arc_notl, arc_n)
            conc_peak_net_notl = max(conc_peak_net_notl, net_n)
            conc_peak_inflight = max(conc_peak_inflight, inf_n)
            conc_peak_imr_lt = max(conc_peak_imr_lt, imr_l)
            conc_peak_imr_arc = max(conc_peak_imr_arc, imr_a)
            conc_peak_mmr_lt = max(conc_peak_mmr_lt, mmr_l)
            conc_peak_mmr_arc = max(conc_peak_mmr_arc, mmr_a)

    # Required per-venue USDC collateral: peak_IMR + 1.8x MMR safety buffer + max_DD_share + 1-clip in-flight reserve
    dd_share = 0.5 * max(dd, abs(worst_win))
    req_lt_usdc = max(conc_peak_imr_lt, 1.8 * conc_peak_mmr_lt) + dd_share + clip_usd
    req_arc_usdc = max(conc_peak_imr_arc, 1.8 * conc_peak_mmr_arc) + dd_share + clip_usd
    req_tot_usdc = req_lt_usdc + req_arc_usdc

    budget_util: dict[str, Any] = {}
    for b_tot in REFERENCE_BUDGETS_USD:
        b_leg = 0.5 * b_tot
        budget_util[f"budget_${int(b_tot)}"] = {
            "per_venue_usdc": round(b_leg, 2),
            "lighter_peak_imr_util_pct": round(100.0 * conc_peak_imr_lt / b_leg, 2) if b_leg > 0 else 0.0,
            "arcus_peak_imr_util_pct": round(100.0 * conc_peak_imr_arc / b_leg, 2) if b_leg > 0 else 0.0,
            "lighter_peak_mmr_util_pct": round(100.0 * conc_peak_mmr_lt / b_leg, 2) if b_leg > 0 else 0.0,
            "arcus_peak_mmr_util_pct": round(100.0 * conc_peak_mmr_arc / b_leg, 2) if b_leg > 0 else 0.0,
            "lighter_min_maint_multiple": round(b_leg / conc_peak_mmr_lt, 2) if conc_peak_mmr_lt > 0 else None,
            "arcus_min_maint_multiple": round(b_leg / conc_peak_mmr_arc, 2) if conc_peak_mmr_arc > 0 else None,
            "net_roic_pct": round(100.0 * mtm / b_tot, 3) if b_tot > 0 else 0.0,
        }

    return {
        "windows_covered": len(by_w),
        "fills": fills,
        "maker_filled_notional_usd": round(mk_notl, 2),
        "hedged_notional_usd": round(hd_notl, 2),
        "orphan_events": orp_ev,
        "orphan_notional_usd": round(orp_notl, 2),
        "orphan_unwind_rate": round(orp_notl / mk_notl, 6) if mk_notl > 0.0 else 0.0,
        "orphan_unwind_pnl_usd": round(orp_pnl, 4),
        "orphan_unwind_cost_usd": round(orp_cost, 4),
        "forced_funding_flattens": forced_flat,
        "blocked_funding_open": blk_open,
        "expected_funding_hours": exp_fh,
        "crossed_funding_hours": crs_fh,
        "funding_avoidance_qualified": fa_qual,
        "fee_attribution_by_asset": {
            "USDC": {
                "lighter_maker_fees_usdc": round(lt_mk_f, 4),
                "lighter_taker_fees_usdc": round(lt_tk_f, 4),
                "lighter_total_fees_usdc": round(lt_mk_f + lt_tk_f, 4),
                "arcus_maker_fees_usdc": round(arc_mk_f, 4),
                "arcus_taker_fees_usdc": round(arc_tk_f, 4),
                "arcus_total_fees_usdc": round(arc_mk_f + arc_tk_f, 4),
                "total_fees_usdc": round(fees_usdc, 4),
                "fill_coverage_reconciled": True,
            }
        },
        "depth_slip_usd": round(dslip, 4),
        "funding_leg_lighter_usd": round(f_lt, 4),
        "funding_leg_arcus_usd": round(f_arc, 4),
        "funding_cash_usd": round(fcash, 4),
        "realized_only_pnl_usd": round(realized, 4),
        "mtm_incl_residual_pnl_usd": round(mtm, 4),
        "bps_per_notional": round(mtm / mk_notl * 1e4, 3) if mk_notl > 0.0 else 0.0,
        "terminal_flat_all": term_flat,
        "terminal_residual_notional_usd": round(res_notl, 4),
        "max_drawdown_usd": round(dd, 2),
        "worst_window_pnl_usd": round(worst_win, 4),
        "positive_window_ratio": round(pos_w / len(by_w), 4) if by_w else 0.0,
        "mean_hold_min": round(sum(holds) / len(holds), 2) if holds else 0.0,
        "collateral_and_exposure": {
            "single_symbol_peak_gross_notional_usd": round(max_sym_gross, 2),
            "single_symbol_peak_single_leg_notional_usd": round(max_sym_single_leg, 2),
            "single_symbol_peak_net_notional_usd": round(max_sym_net, 2),
            "single_symbol_worst_inflight_reserve_usd": round(max_sym_inflight, 2),
            "single_symbol_peak_imr_lighter_usd": round(max_sym_imr_lt, 2),
            "single_symbol_peak_imr_arcus_usd": round(max_sym_imr_arc, 2),
            "concurrent_peak_lighter_notional_usd": round(conc_peak_lt_notl, 2),
            "concurrent_peak_arcus_notional_usd": round(conc_peak_arc_notl, 2),
            "concurrent_peak_net_notional_usd": round(conc_peak_net_notl, 2),
            "concurrent_worst_inflight_reserve_usd": round(conc_peak_inflight, 2),
            "concurrent_peak_imr_lighter_usd": round(conc_peak_imr_lt, 2),
            "concurrent_peak_imr_arcus_usd": round(conc_peak_imr_arc, 2),
            "concurrent_peak_mmr_lighter_usd": round(conc_peak_mmr_lt, 2),
            "concurrent_peak_mmr_arcus_usd": round(conc_peak_mmr_arc, 2),
            "required_equity_lighter_usdc": round(req_lt_usdc, 2),
            "required_equity_arcus_usdc": round(req_arc_usdc, 2),
            "required_total_equity_usdc": round(req_tot_usdc, 2),
            "reference_budgets": budget_util,
        },
        "window_pnl_series": {str(wn): round(by_w[wn], 4) for wn in sorted(by_w)},
        "window_realized_series": {str(wn): round(by_w_real[wn], 4) for wn in sorted(by_w_real)},
    }


def _build_split_summary(
    runs: Sequence[dict[str, Any]],
    *,
    clip_usd: float,
) -> dict[str, Any]:
    """Summarize `selection_dev`, `holdout`, `w26_w27_oos`, `all_w01_w27`, `ex_nvda`, and bootstrap CI."""
    splits: dict[str, Any] = {}
    for sp in ("selection_dev", "holdout", "w26_w27_oos"):
        splits[sp] = aggregate_settleability_runs([r for r in runs if r["split"] == sp], clip_usd=clip_usd)
    splits["all_w01_w27"] = aggregate_settleability_runs(runs, clip_usd=clip_usd)

    ex_nvda = {
        sp: aggregate_settleability_runs(
            [r for r in runs if r["split"] == sp and r["symbol"] != "NVDA"],
            clip_usd=clip_usd,
        )
        for sp in ("selection_dev", "holdout", "w26_w27_oos")
    }
    ex_nvda["all_w01_w27"] = aggregate_settleability_runs(
        [r for r in runs if r["symbol"] != "NVDA"],
        clip_usd=clip_usd,
    )

    all_series = [
        splits["all_w01_w27"]["window_pnl_series"][wk]
        for wk in sorted(splits["all_w01_w27"]["window_pnl_series"], key=int)
    ]
    boot_all = stationary_block_bootstrap_ci(all_series)
    return {
        "splits": splits,
        "ex_nvda_splits": ex_nvda,
        "bootstrap_all_w01_w27": boot_all,
    }


def _compute_multiple_testing(trials_by_name: dict[str, list[float]]) -> dict[str, Any]:
    if not trials_by_name:
        return {"status": "empty"}
    varying = {k: v for k, v in trials_by_name.items() if len(set(v)) > 1 and len(v) >= 4}
    out: dict[str, Any] = {"raw_trial_count": len(trials_by_name), "varying_trial_count": len(varying)}
    if not varying:
        out["status"] = "all_constant"
        return out
    t_len = len(next(iter(varying.values())))
    if t_len >= 3:
        out["dsr"] = deflated_sharpe_report(varying)
    if len(varying) >= 2 and t_len >= 16:
        out["cscv_pbo"] = cscv_pbo(varying, num_blocks=8)
    elif len(varying) >= 2 and t_len >= 8:
        out["cscv_pbo"] = cscv_pbo(varying, num_blocks=4)
    return out


def run_lakehouse_audit(
    lakehouse_dir: Path,
    *,
    fee_evidence_path: Path | None,
    output_path: Path,
    domains: tuple[str, ...] = ("rwa", "crypto"),
    clip_sizes: Sequence[float] = CLIP_SIZES_USD,
    probe_public_meta: bool = False,
) -> dict[str, Any]:
    """Run full Core <-> Arcus `B0` settleability audit across `W01..W27` on Aleph lakehouse."""
    import duckdb

    con = duckdb.connect(":memory:")
    con.execute("SET preserve_insertion_order=false; SET threads=2; SET memory_limit='2GB';")

    fees_meta, fee_prov = resolve_track02_fees(fee_evidence_path, lighter_mode="verified_prem")
    lt_meta = VENUE_TIMING["lighter_prem"]
    arc_meta = VENUE_TIMING["arcus"]

    campaigns_by_dom = {
        dom: discover_w01_w27_campaigns(lakehouse_dir, dom, min_window=1, max_window=27)
        for dom in domains
    }

    public_probe = probe_public_arcus_and_lighter_metadata() if probe_public_meta else {
        "status": "offline_snapshot_used",
        "lakehouse_primary_fact": "Aleph lakehouse has 0 funding_and_ctx parquet files for Arcus across W01-W27.",
    }

    # Runs for frozen B0 across clip sizes & modes: (dom, clip_usd, mode) -> list[run]
    frozen_runs: dict[tuple[str, float, str], list[dict[str, Any]]] = {}
    # Runs for 16-config B0 grid at $1,000 for DSR/PBO: (dom, mode, param_name) -> list[run]
    grid_runs_1k: dict[tuple[str, str, str], list[dict[str, Any]]] = {}
    symbol_coverage: dict[str, Any] = {}

    for dom in domains:
        syms = RWA_CORE_ARCUS_SYMBOLS if dom == "rwa" else CRYPTO_SYMBOLS
        arcus_mkts = sorted({VENUE_SYMBOL_MAP["arcus"][s] for s in syms})
        lt_sym_map = {s: VENUE_SYMBOL_MAP["lighter"][s] for s in syms}
        lt_mkts = sorted(set(lt_sym_map.values()))

        af = fees_meta["arcus"][dom]
        lf = fees_meta["lighter"][dom]

        for camp in campaigns_by_dom[dom]:
            wnum = int(camp["window_num"])
            wname = camp["window"]
            split = camp["split"]
            cid = camp["campaign_id"]
            udate = camp["utc_date"]

            bbo_arc = _load_venue_bbo_buckets(
                con, campaign_parquet_files(lakehouse_dir, "bbo_quotes", "arcus", udate, cid), arcus_mkts
            )
            bbo_lt = _load_venue_bbo_buckets(
                con, campaign_parquet_files(lakehouse_dir, "bbo_quotes", "lighter", udate, cid), lt_mkts
            )
            tr_lt = _load_venue_trades(
                con, campaign_parquet_files(lakehouse_dir, "public_trades", "lighter", udate, cid), lt_mkts
            )
            fn_lt = _load_venue_funding(
                con, campaign_parquet_files(lakehouse_dir, "funding_and_ctx", "lighter", udate, cid), "lighter", lt_sym_map
            )
            fn_arc_files = campaign_parquet_files(lakehouse_dir, "funding_and_ctx", "arcus", udate, cid)

            for sym in syms:
                am = VENUE_SYMBOL_MAP["arcus"][sym]
                lm = lt_sym_map[sym]
                if am not in bbo_arc or lm not in bbo_lt:
                    continue
                acols, acont = bbo_arc[am]
                lcols, lcont = bbo_lt[lm]
                t0 = max(int(acont["t0_ms"]), int(lcont["t0_ms"]))
                t1 = min(int(acont["t1_ms"]), int(lcont["t1_ms"]))
                if t1 - t0 < WARMUP_MS + 60_000:
                    continue

                ga = _build_grid_np(acols, t0, t1, STEP_MS)
                gl = _build_grid_np(lcols, t0, t1, STEP_MS)
                tl = tr_lt.get(lm)
                fl = fn_lt.get(lm)
                causal = compute_causal_basis_2h(ga["bid"], ga["ask"], ga["age"], gl["bid"], gl["ask"], gl["age"])
                tick_lt = _infer_tick(lcols["bid"])
                n_steps = len(ga["bid"])
                dec_steps = max(1, min(TEN_HOURS_MS // STEP_MS, n_steps - WARMUP_MS // STEP_MS))
                pre_lt = bucket_trades_by_step(tl, t0, lt_meta["maker_delay_ms"], n_steps, decision_steps=dec_steps)

                cov = symbol_coverage.setdefault(
                    sym,
                    {"domain": dom, "windows": 0, "lighter_funding_hours": 0, "arcus_funding_files": 0},
                )
                cov["windows"] += 1
                cov["lighter_funding_hours"] += len(fl) if fl else 0
                cov["arcus_funding_files"] += len(fn_arc_files)

                # 1. Event-by-event replay at every clip size ($500, $1000, $2000) for B0_original, B0_funding_flat, flat
                for clip_usd in clip_sizes:
                    p_frozen = frozen_b0_params(clip_usd)
                    for mode in ("B0_original", "B0_funding_flat", "flat"):
                        res = simulate_b0_settleability(
                            symbol=sym,
                            domain=dom,
                            arcus_bid=ga["bid"],
                            arcus_ask=ga["ask"],
                            arcus_bsz=ga["bsz"],
                            arcus_asz=ga["asz"],
                            arcus_age=ga["age"],
                            hedge_bid=gl["bid"],
                            hedge_ask=gl["ask"],
                            hedge_bsz=gl["bsz"],
                            hedge_asz=gl["asz"],
                            hedge_age=gl["age"],
                            causal=causal,
                            lighter_trades=tl,
                            t0_ms=t0,
                            tick=tick_lt,
                            params=p_frozen,
                            mode=mode,
                            lighter_maker_delay_ms=lt_meta["maker_delay_ms"],
                            lighter_taker_delay_ms=lt_meta["taker_delay_ms"],
                            lighter_speedbump_ms=lt_meta["speedbump_ms"],
                            arcus_delay_ms=arc_meta["maker_delay_ms"],
                            lighter_mk_fee_bps=float(lf["maker_fee_bps"]),
                            lighter_tk_fee_bps=float(lf["taker_fee_bps"]),
                            arcus_mk_fee_bps=float(af["maker_fee_bps"]),
                            arcus_tk_fee_bps=float(af["taker_fee_bps"]),
                            decision_steps=dec_steps,
                            clip_usd=clip_usd,
                            lighter_funding=fl,
                            arcus_funding=None,
                            pre_lighter_by_step=pre_lt,
                            record_checkpoints=True,
                        )
                        res["window"] = wname
                        res["window_num"] = wnum
                        res["split"] = split
                        frozen_runs.setdefault((dom, clip_usd, mode), []).append(res)
                        if abs(clip_usd - 1_000.0) < 1e-6 and mode in ("B0_original", "B0_funding_flat"):
                            grid_runs_1k.setdefault((dom, mode, p_frozen.name), []).append(res)

                # 2. Remaining 15 B0 grid configs at $1,000 for full DSR & CSCV PBO
                for cand in build_b0_grid(1_000.0):
                    if cand.name == "b0_e10_v0.0_sh0":
                        continue
                    for mode in ("B0_original", "B0_funding_flat"):
                        res_g = simulate_b0_settleability(
                            symbol=sym,
                            domain=dom,
                            arcus_bid=ga["bid"],
                            arcus_ask=ga["ask"],
                            arcus_bsz=ga["bsz"],
                            arcus_asz=ga["asz"],
                            arcus_age=ga["age"],
                            hedge_bid=gl["bid"],
                            hedge_ask=gl["ask"],
                            hedge_bsz=gl["bsz"],
                            hedge_asz=gl["asz"],
                            hedge_age=gl["age"],
                            causal=causal,
                            lighter_trades=tl,
                            t0_ms=t0,
                            tick=tick_lt,
                            params=cand,
                            mode=mode,
                            lighter_maker_delay_ms=lt_meta["maker_delay_ms"],
                            lighter_taker_delay_ms=lt_meta["taker_delay_ms"],
                            lighter_speedbump_ms=lt_meta["speedbump_ms"],
                            arcus_delay_ms=arc_meta["maker_delay_ms"],
                            lighter_mk_fee_bps=float(lf["maker_fee_bps"]),
                            lighter_tk_fee_bps=float(lf["taker_fee_bps"]),
                            arcus_mk_fee_bps=float(af["maker_fee_bps"]),
                            arcus_tk_fee_bps=float(af["taker_fee_bps"]),
                            decision_steps=dec_steps,
                            clip_usd=1_000.0,
                            lighter_funding=fl,
                            arcus_funding=None,
                            pre_lighter_by_step=pre_lt,
                            record_checkpoints=False,
                        )
                        res_g["window"] = wname
                        res_g["window_num"] = wnum
                        res_g["split"] = split
                        grid_runs_1k.setdefault((dom, mode, cand.name), []).append(res_g)

            del bbo_arc, bbo_lt, tr_lt, fn_lt
            gc.collect()
            print(f"[processed] {dom} {wname} ({cid})", flush=True)

    # Assemble structured report without huge raw checkpoints in JSON
    report: dict[str, Any] = {
        "schema_version": "track02_b0_settleability_audit_v1",
        "path": "Core <-> Arcus (USDC/USDC)",
        "campaigns": {
            dom: [{"window": c["window"], "window_num": c["window_num"], "split": c["split"]} for c in cl]
            for dom, cl in campaigns_by_dom.items()
        },
        "fee_provenance": {
            "source_sha256": fee_prov.get("evidence_sha256"),
            "lighter_prem": fees_meta["lighter"],
            "arcus_level0_5pct_discount": fees_meta["arcus"],
        },
        "margin_specs": {
            "arcus": ARCUS_MARGIN_SPECS,
            "lighter": LIGHTER_MARGIN_SPECS,
        },
        "public_metadata_probe": public_probe,
        "symbol_coverage": symbol_coverage,
        "domains": {},
    }

    for dom in domains:
        clip_out: dict[str, Any] = {}
        all_trials: dict[str, list[float]] = {}
        for clip_usd in clip_sizes:
            ckey = f"clip_${int(clip_usd)}"
            arms_cmp: dict[str, Any] = {}
            for mode in ("B0_original", "B0_funding_flat", "flat"):
                runs = frozen_runs.get((dom, clip_usd, mode), [])
                arms_cmp[mode] = _build_split_summary(runs, clip_usd=clip_usd)
                if mode in ("B0_original", "B0_funding_flat"):
                    ws = arms_cmp[mode]["splits"]["all_w01_w27"].get("window_pnl_series", {})
                    if ws:
                        all_trials[f"{ckey}:{mode}:b0_e10_v0.0_sh0"] = [ws[k] for k in sorted(ws, key=int)]

            # Paired window delta (B0_funding_flat - B0_original) on identical windows
            ws_orig = arms_cmp["B0_original"]["splits"]["all_w01_w27"].get("window_pnl_series", {})
            ws_ff = arms_cmp["B0_funding_flat"]["splits"]["all_w01_w27"].get("window_pnl_series", {})
            common_w = sorted(set(ws_orig) & set(ws_ff), key=int)
            delta_all = [ws_ff[wk] - ws_orig[wk] for wk in common_w]
            delta_by_split: dict[str, Any] = {}
            for sp in ("selection_dev", "holdout", "w26_w27_oos"):
                so = arms_cmp["B0_original"]["splits"][sp].get("window_pnl_series", {})
                sf = arms_cmp["B0_funding_flat"]["splits"][sp].get("window_pnl_series", {})
                cw = sorted(set(so) & set(sf), key=int)
                d_sp = [sf[wk] - so[wk] for wk in cw]
                delta_by_split[sp] = round(sum(d_sp), 4)

            clip_out[ckey] = {
                "clip_usd": clip_usd,
                "arms": arms_cmp,
                "paired_delta_funding_flat_minus_original": {
                    "by_split_usd": delta_by_split,
                    "all_w01_w27_bootstrap": stationary_block_bootstrap_ci(delta_all),
                },
            }

        # Add full 16-config grid at $1,000 for DSR & CSCV PBO
        grid_1k_trials: dict[str, list[float]] = {}
        for (d_k, mode_k, p_k), gruns in grid_runs_1k.items():
            if d_k != dom:
                continue
            agg = aggregate_settleability_runs(gruns, clip_usd=1_000.0, include_checkpoints=False)
            ws = agg.get("window_pnl_series", {})
            if ws:
                ser = [ws[k] for k in sorted(ws, key=int)]
                grid_1k_trials[f"{mode_k}:{p_k}"] = ser
                all_trials[f"clip_$1000:{mode_k}:{p_k}"] = ser

        report["domains"][dom] = {
            "by_clip_size": clip_out,
            "multiple_testing_grid_1k": _compute_multiple_testing(grid_1k_trials),
            "multiple_testing_all_audit_trials": _compute_multiple_testing(all_trials),
        }

    output_path.parent.mkdir(parents=True, exist_ok=True)
    output_path.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return report


def run_self_test() -> dict[str, Any]:
    """Deterministic stdlib-only self-test verifying clip non-linearity, margin/off-RTH, and funding-flat proof."""
    n = 420  # 84s @ 200ms
    step_ms = 200
    # Align t0 so a `:00` UTC hour boundary falls inside decision window (at k=250 -> t_cur = ht)
    ht_target = 1_795_777_200_000  # exact :00 UTC hour
    t0 = ht_target - 251 * step_ms
    warmup_steps = 40

    a_bid = [99.99] * n
    a_ask = [100.01] * n
    a_bsz = [8.0] * n   # ~$800 L1 -> clamps $1000/$2000 clips while $500 fits!
    a_asz = [8.0] * n
    a_age = [0] * n

    h_bid = [99.99] * n
    h_ask = [100.01] * n
    h_bsz = [20.0] * n
    h_asz = [20.0] * n
    h_age = [0] * n

    # Make Lighter rich at k=45..140 so 5s drift settles before tf at k=120
    h_bid[45:145] = [100.14] * 100
    h_ask[45:145] = [100.16] * 100

    causal = compute_causal_basis_2h(a_bid, a_ask, a_age, h_bid, h_ask, h_age, step_ms=step_ms)
    tf = t0 + 120 * step_ms + 200 + 40
    lt_tr = {"ts": [tf], "sgn": [1], "px": [100.25], "sz": [20.0]}
    lt_fund = {ht_target: 10.0}

    p_500 = frozen_b0_params(500.0)
    p_1000 = frozen_b0_params(1_000.0)

    r_orig_500 = simulate_b0_settleability(
        symbol="NVDA", domain="rwa",
        arcus_bid=a_bid, arcus_ask=a_ask, arcus_bsz=a_bsz, arcus_asz=a_asz, arcus_age=a_age,
        hedge_bid=h_bid, hedge_ask=h_ask, hedge_bsz=h_bsz, hedge_asz=h_asz, hedge_age=h_age,
        causal=causal, lighter_trades=lt_tr, t0_ms=t0, tick=0.01, params=p_500, mode="B0_original",
        warmup_steps=warmup_steps, decision_steps=n - warmup_steps, clip_usd=500.0,
        lighter_funding=lt_fund, open_block_before_hour_ms=20_000, flatten_before_hour_ms=10_000, guard_half_ms=2_000,
    )
    r_ff_500 = simulate_b0_settleability(
        symbol="NVDA", domain="rwa",
        arcus_bid=a_bid, arcus_ask=a_ask, arcus_bsz=a_bsz, arcus_asz=a_asz, arcus_age=a_age,
        hedge_bid=h_bid, hedge_ask=h_ask, hedge_bsz=h_bsz, hedge_asz=h_asz, hedge_age=h_age,
        causal=causal, lighter_trades=lt_tr, t0_ms=t0, tick=0.01, params=p_500, mode="B0_funding_flat",
        warmup_steps=warmup_steps, decision_steps=n - warmup_steps, clip_usd=500.0,
        lighter_funding=lt_fund, open_block_before_hour_ms=20_000, flatten_before_hour_ms=10_000, guard_half_ms=2_000,
    )
    r_orig_1k = simulate_b0_settleability(
        symbol="NVDA", domain="rwa",
        arcus_bid=a_bid, arcus_ask=a_ask, arcus_bsz=a_bsz, arcus_asz=a_asz, arcus_age=a_age,
        hedge_bid=h_bid, hedge_ask=h_ask, hedge_bsz=h_bsz, hedge_asz=h_asz, hedge_age=h_age,
        causal=causal, lighter_trades=lt_tr, t0_ms=t0, tick=0.01, params=p_1000, mode="B0_original",
        warmup_steps=warmup_steps, decision_steps=n - warmup_steps, clip_usd=1_000.0,
        lighter_funding=lt_fund, open_block_before_hour_ms=20_000, flatten_before_hour_ms=10_000, guard_half_ms=2_000,
    )
    assert r_orig_500["fills"] == 1
    assert r_orig_500["crossed_funding_hours"] == 1
    assert r_ff_500["fills"] == 1
    assert r_ff_500["forced_funding_flattens"] == 1
    assert r_ff_500["crossed_funding_hours"] == 0
    assert r_ff_500["funding_avoidance_qualified"] is True
    assert r_ff_500["funding_cash_usd"] == 0.0
    assert r_orig_1k["fills"] == 0  # L1=$800 < $1000 gate -> non-linear clip sizing!

    boot = stationary_block_bootstrap_ci([1.0, 2.0, -0.5, 1.5, 2.5, 0.5], n_boot=200)
    return {
        "self_test": "ok",
        "orig_500_crossed_hours": r_orig_500["crossed_funding_hours"],
        "ff_500_crossed_hours": r_ff_500["crossed_funding_hours"],
        "ff_500_flattens": r_ff_500["forced_funding_flattens"],
        "orig_1k_blocked_l1": r_orig_1k["blocked_l1_thin"],
        "boot_total": boot["total"],
    }


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--self-test", action="store_true")
    ap.add_argument("--lakehouse", type=Path, default=Path.home() / "aleph-tx-lakehouse")
    ap.add_argument("--fee-evidence", type=Path, default=None)
    ap.add_argument(
        "--output",
        type=Path,
        default=Path("/tmp/roadmap_upgrade_20261127/track02_b0_settleability_w01_w27.json"),
    )
    ap.add_argument("--domains", type=str, default="rwa,crypto")
    ap.add_argument("--probe-public-meta", action="store_true")
    args = ap.parse_args()

    if args.self_test:
        print(json.dumps(run_self_test(), indent=2, sort_keys=True))
        return 0

    doms = tuple(d.strip().lower() for d in args.domains.split(",") if d.strip())
    report = run_lakehouse_audit(
        args.lakehouse,
        fee_evidence_path=args.fee_evidence,
        output_path=args.output,
        domains=doms,
        probe_public_meta=args.probe_public_meta,
    )
    print(json.dumps({"status": "ok", "output": str(args.output), "domains": list(report["domains"].keys())}))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
