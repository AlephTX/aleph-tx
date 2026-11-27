#!/usr/bin/env python3
"""Track 02 v2 (`02_hedge_arbitrage`): Joint Multi-Hour Funding Carry + Live Executable Basis-Timing Portfolio.

Evaluates ALL overlapping RWA & Crypto pairs across Aleph Tokyo `W01..W26` (~256h crypto /
~205h RWA, explicitly excluding `-w27` and `-w28`) joining real hourly `funding_and_ctx`
payments with ACTUAL L1 executable `bbo_quotes` `bid/ask` & displayed depth, `330 ms`
taker execution latency, and `public_trades` strict trade-through print verification for
`200 ms`-delayed passive entries.

Strictly enforces:
- Pure stdlib top-level imports (`duckdb` and `numpy` lazy-imported inside loaders only).
- Segregation of `USDC/USDC` distinct-domain, `USDG/USDC` distinct-domain (`{0, 1, 3}` bps
  FX drag stress), and `same_risk_domain` (`lighter <-> lighter_robinhood`,
  `trade_xyz <-> entropy`).
- Comparison of verified `lighter` Prem (`0.40/2.80`) vs `lighter` Std (`0/0`) counterfactual,
  `lighter_robinhood` (`0/0`), `trade_xyz` growthMode (`0.30/0.90`), `hyperliquid` (`1.50/4.50`),
  and unaudited `entropy` (`0.30/0.90`).
- Zero cross-subsidy between Crypto & RWA cohorts.
- Strict `selection_dev (w01-w20)` -> frozen `holdout (w21-w25)` -> `w26_oos` discipline,
  plus PSR / N_eff / DSR / MinTRL / CSCV PBO via `audit_multiple_testing_overfitting.py`.
"""

from __future__ import annotations

import argparse
from bisect import bisect_left, bisect_right
from dataclasses import asdict, dataclass
from datetime import date, timedelta
import gc
import glob
import hashlib
import json
import math
import os
from pathlib import Path
import re
import sys
from typing import Any, Sequence

SCRIPT_DIR = Path(__file__).resolve().parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

from audit_multiple_testing_overfitting import (  # noqa: E402
    cscv_pbo,
    deflated_sharpe_report,
    min_track_record_length,
    norm_ppf,
    probabilistic_sharpe_ratio,
    series_moments,
)
from track_fee_evidence import (  # noqa: E402
    VENUE_CAPABILITY_SNAPSHOT_VERSION,
    build_fee_provenance,
    load_track_fee_evidence,
    resolve_venue_fee,
)

HOUR_MS = 3_600_000
STEP_MS = 30_000
TAKER_LATENCY_MS = 330
PASSIVE_LATENCY_MS = 200
MAX_PASSIVE_WAIT_MS = 2_000
MAX_RECOVERY_MS = 5_000
MAX_GAP_MS = 3 * HOUR_MS
TARGET_PAIR_NOTIONAL_USD = 1_000.0
MIN_ENTRY_NOTIONAL_USD = 100.0
PORTFOLIO_CAPITAL_USD = 10_000.0
SINGLE_SYMBOL_WEIGHT_CAP = 0.40
FX_STRESS_BPS: tuple[float, ...] = (0.0, 1.0, 3.0)
PORTFOLIO_KS: tuple[int, ...] = (3, 5)
CRYPTO_SYMBOLS = frozenset({"BTC", "ETH", "SOL"})

# Verified full market ID map (confirmed on Aleph w26).
VENUE_SYMBOL_MAP: dict[str, dict[str, str]] = {
    "lighter_robinhood": {
        "ETH": "0",
        "BTC": "1",
        "SOL": "3",
        "AAPL": "10",
        "AMZN": "11",
        "GOOGL": "12",
        "META": "13",
        "MSFT": "14",
        "NVDA": "15",
        "TSLA": "16",
        "COIN": "23",
        "CRCL": "24",
        "QQQ": "25",
        "SPY": "26",
        "AMD": "29",
        "MU": "31",
        "SNDK": "32",
        "PLTR": "34",
    },
    "lighter": {
        "ETH": "0",
        "BTC": "1",
        "SOL": "2",
        "HOOD": "108",
        "COIN": "109",
        "NVDA": "110",
        "PLTR": "111",
        "TSLA": "112",
        "AAPL": "113",
        "AMZN": "114",
        "MSFT": "115",
        "GOOGL": "116",
        "META": "117",
        "MSTR": "122",
        "SPY": "128",
        "QQQ": "129",
        "AMD": "138",
        "SNDK": "139",
        "MU": "164",
    },
    "trade_xyz": {
        "SPY": "xyz:SP500",
        "QQQ": "xyz:XYZ100",
        **{
            sym: f"xyz:{sym}"
            for sym in (
                "AAPL",
                "AMD",
                "AMZN",
                "COIN",
                "CRCL",
                "DRAM",
                "GOOGL",
                "HOOD",
                "META",
                "MSFT",
                "MSTR",
                "MU",
                "NVDA",
                "PLTR",
                "SNDK",
                "TSLA",
            )
        },
    },
    "hyperliquid": {
        "BTC": "BTC",
        "ETH": "ETH",
        "SOL": "SOL",
    },
    "entropy": {
        sym: f"io:{sym}" for sym in ("AAPL", "CRCL", "DRAM", "NBIS", "NVDA", "SNDK")
    },
}

VENUE_META: dict[str, dict[str, Any]] = {
    "lighter_robinhood": {
        "settlement": "USDG",
        "risk_group": "lighter_l2",
        "max_book_age_ms": 5_000,
        "bbo_cadence": "ws_50ms",
        "maker_fee_bps": 0.0,
        "taker_fee_bps": 0.0,
        "fee_status": "verified_account_tier",
    },
    "lighter": {
        "settlement": "USDC",
        "risk_group": "lighter_l2",
        "max_book_age_ms": 5_000,
        "bbo_cadence": "ws_50ms",
        "maker_fee_bps": 0.40,
        "taker_fee_bps": 2.80,
        "fee_status": "verified_account_tier",
    },
    "trade_xyz": {
        "settlement": "USDC",
        "risk_group": "hl_hip3",
        "max_book_age_ms": 20_000,
        "bbo_cadence": "snapshot_12s",
        "maker_fee_bps": 0.30,
        "taker_fee_bps": 0.90,
        "fee_status": "verified_account_tier_growth_mode",
    },
    "hyperliquid": {
        "settlement": "USDC",
        "risk_group": "hl_core",
        "max_book_age_ms": 20_000,
        "bbo_cadence": "snapshot_12s",
        "maker_fee_bps": 1.50,
        "taker_fee_bps": 4.50,
        "fee_status": "verified_account_tier",
    },
    "entropy": {
        "settlement": "USDC",
        "risk_group": "hl_hip3",
        "max_book_age_ms": 20_000,
        "bbo_cadence": "snapshot_12s",
        "maker_fee_bps": 0.30,
        "taker_fee_bps": 0.90,
        "fee_status": "account_evidence_required",
    },
}


@dataclass(frozen=True)
class CarryPairSpec:
    symbol: str
    domain: str  # "crypto" or "rwa"
    venue_a: str
    venue_b: str
    cohort: str  # "USDC/USDC" or "USDG/USDC"
    same_risk_domain: bool
    label: str
    bbo_cadence_pair: str

    @property
    def cross_stablecoin(self) -> bool:
        return self.cohort == "USDG/USDC"


@dataclass(frozen=True)
class CarryArmParams:
    name: str
    arm: str  # C0_naive_carry, C1_basis_timed_taker, C2_basis_timed_passive_first
    funding_tau_h: float = 12.0
    basis_fair_tau_h: float = 6.0
    basis_vol_tau_h: float = 2.0
    horizon_h: float = 12.0
    hurdle_mult: float = 1.5
    min_expected_carry_bps: float = 1.0
    min_basis_zscore: float = 0.0
    basis_vol_floor_bps: float = 1.0
    basis_timing_weight: float = 1.0
    basis_timing_cap_bps: float = 25.0
    take_profit_basis_bps: float = 0.0
    take_profit_zscore: float = 0.0
    adverse_funding_exit_bps_hr: float = 0.0
    max_basis_stop_bps: float = 25.0
    max_hold_h: float = 48.0
    passive_first: bool = False
    max_passive_wait_ms: int = MAX_PASSIVE_WAIT_MS
    hazard_adverse_bps: float = 3.0
    min_l1_notional_usd: float = MIN_ENTRY_NOTIONAL_USD


def split_for_window(w_num: int) -> str:
    if w_num <= 20:
        return "selection_dev"
    if w_num <= 25:
        return "holdout"
    return "w26_oos"


def build_pair_catalog() -> list[CarryPairSpec]:
    """Enumerate all overlapping venue pairs across all ~19+ RWA & Crypto symbols."""
    by_sym: dict[str, list[str]] = {}
    for venue, mp in VENUE_SYMBOL_MAP.items():
        for sym in mp:
            by_sym.setdefault(sym, []).append(venue)

    pairs: list[CarryPairSpec] = []
    # Deterministic ordering of venue pairs
    venue_order = ("lighter", "lighter_robinhood", "trade_xyz", "hyperliquid", "entropy")
    for sym in sorted(by_sym):
        dom = "crypto" if sym in CRYPTO_SYMBOLS else "rwa"
        vs = [v for v in venue_order if v in by_sym[sym]]
        for i in range(len(vs)):
            for j in range(i + 1, len(vs)):
                va, vb = vs[i], vs[j]
                sa = VENUE_META[va]["settlement"]
                sb = VENUE_META[vb]["settlement"]
                cohort = "USDC/USDC" if sa == sb == "USDC" else "USDG/USDC"
                same_dom = VENUE_META[va]["risk_group"] == VENUE_META[vb]["risk_group"]
                ca = VENUE_META[va]["bbo_cadence"]
                cb = VENUE_META[vb]["bbo_cadence"]
                cad_pair = "ws_both" if ca == cb == "ws_50ms" else "mixed_ws_snapshot12s"
                pairs.append(
                    CarryPairSpec(
                        symbol=sym,
                        domain=dom,
                        venue_a=va,
                        venue_b=vb,
                        cohort=cohort,
                        same_risk_domain=same_dom,
                        label=f"{va}<->{vb} {sym}",
                        bbo_cadence_pair=cad_pair,
                    )
                )
    return pairs


def resolve_venue_fees(
    fee_evidence_path: Path | None = None,
    *,
    lighter_fee_mode: str = "verified_prem",
) -> tuple[dict[str, dict[str, Any]], dict[str, Any]]:
    """Resolve venue fee schedules for Lighter Prem (0.40/2.80) vs Std (0/0) counterfactual."""
    ev = load_track_fee_evidence(fee_evidence_path)
    resolved: dict[str, dict[str, Any]] = {}
    for venue, meta in VENUE_META.items():
        if venue == "lighter":
            if lighter_fee_mode == "standard_zero":
                mk, tk, st, src = 0.0, 0.0, "lighter_standard_zero_counterfactual", "counterfactual_public_0_0"
            else:
                mk, tk, st, src = resolve_venue_fee(
                    "lighter",
                    "crypto",
                    default_maker_bps=0.40,
                    default_taker_bps=2.80,
                    default_status="verified_account_tier",
                    evidence=ev,
                )
        elif venue == "entropy":
            mk, tk, st, src = (
                float(meta["maker_fee_bps"]),
                float(meta["taker_fee_bps"]),
                "account_evidence_required",
                "hip3_growth_mode_unaudited",
            )
        else:
            mk, tk, st, src = resolve_venue_fee(
                venue,
                "crypto",
                default_maker_bps=float(meta["maker_fee_bps"]),
                default_taker_bps=float(meta["taker_fee_bps"]),
                default_status=str(meta["fee_status"]),
                evidence=ev,
                trade_xyz_fee_mode="growth_mode",
            )
        resolved[venue] = {
            **meta,
            "maker_fee_bps": mk,
            "taker_fee_bps": tk,
            "fee_status": st,
            "fee_source": src,
        }
    prov = build_fee_provenance(
        ev,
        trade_xyz_fee_mode="growth_mode",
        venues_in_scope=("lighter", "lighter_robinhood", "hyperliquid", "trade_xyz"),
    )
    prov["lighter_fee_mode"] = lighter_fee_mode
    prov["entropy_fee_status"] = resolved["entropy"]["fee_status"]
    return resolved, prov


def build_arm_candidate_grid() -> dict[str, list[CarryArmParams]]:
    """Pre-registered causal candidate grid selected strictly on `selection_dev (w01..w20)`."""
    c0_list = [
        CarryArmParams(
            name=f"c0_tau{int(tau)}_h{int(hor)}_m{hm}",
            arm="C0_naive_carry",
            funding_tau_h=tau,
            horizon_h=hor,
            hurdle_mult=hm,
            min_expected_carry_bps=2.0,
            min_basis_zscore=-999.0,  # no basis timing gate
            basis_timing_weight=0.0,
            take_profit_basis_bps=0.0,
            max_basis_stop_bps=999.0,
            passive_first=False,
        )
        for tau in (6.0, 12.0, 24.0)
        for hor in (12.0, 24.0)
        for hm in (1.2, 1.8)
    ]

    c1_list = [
        CarryArmParams(
            name=f"c1_tau{int(tau)}_h{int(hor)}_z{z}_tp{int(tp)}_m{hm}",
            arm="C1_basis_timed_taker",
            funding_tau_h=tau,
            basis_fair_tau_h=6.0,
            basis_vol_tau_h=2.0,
            horizon_h=hor,
            hurdle_mult=hm,
            min_expected_carry_bps=2.0,
            min_basis_zscore=z,
            basis_vol_floor_bps=1.0,
            basis_timing_weight=1.0,
            basis_timing_cap_bps=15.0,
            take_profit_basis_bps=tp,
            take_profit_zscore=0.0,
            adverse_funding_exit_bps_hr=0.0,
            max_basis_stop_bps=25.0,
            passive_first=False,
        )
        for tau in (6.0, 12.0)
        for hor in (12.0, 24.0)
        for z in (0.75, 1.5)
        for tp in (5.0,)
        for hm in (1.2, 1.6)
    ]

    c2_list = [
        CarryArmParams(
            name=f"c2_tau{int(tau)}_h{int(hor)}_z{z}_tp{int(tp)}_m{hm}",
            arm="C2_basis_timed_passive_first",
            funding_tau_h=tau,
            basis_fair_tau_h=6.0,
            basis_vol_tau_h=2.0,
            horizon_h=hor,
            hurdle_mult=hm,
            min_expected_carry_bps=2.0,
            min_basis_zscore=z,
            basis_vol_floor_bps=1.0,
            basis_timing_weight=1.0,
            basis_timing_cap_bps=15.0,
            take_profit_basis_bps=tp,
            take_profit_zscore=0.0,
            adverse_funding_exit_bps_hr=0.0,
            max_basis_stop_bps=25.0,
            passive_first=True,
            max_passive_wait_ms=MAX_PASSIVE_WAIT_MS,
            hazard_adverse_bps=3.0,
        )
        for tau in (6.0, 12.0)
        for hor in (12.0, 24.0)
        for z in (0.75, 1.5)
        for tp in (5.0,)
        for hm in (1.2, 1.6)
    ]

    return {
        "C0_naive_carry": c0_list,
        "C1_basis_timed_taker": c1_list,
        "C2_basis_timed_passive_first": c2_list,
    }


def _bisect_right(arr: Any, val: int) -> int:
    if hasattr(arr, "searchsorted"):
        return int(arr.searchsorted(val, side="right"))
    return bisect_right(arr, val)


def _bisect_left(arr: Any, val: int) -> int:
    if hasattr(arr, "searchsorted"):
        return int(arr.searchsorted(val, side="left"))
    return bisect_left(arr, val)


def _lookup_bbo_at(
    bbo: dict[str, Sequence[Any]],
    t_ms: int,
    max_age_ms: int,
) -> tuple[float, float, float, float, int] | None:
    """Lookup latest valid L1 BBO with `ts <= t_ms` and age `<= max_age_ms`."""
    ts_arr = bbo["ts"]
    idx = _bisect_right(ts_arr, t_ms) - 1
    if idx < 0:
        return None
    obs_ms = int(ts_arr[idx])
    if t_ms - obs_ms > max_age_ms:
        return None
    bid = float(bbo["bid"][idx])
    ask = float(bbo["ask"][idx])
    bsz = float(bbo["bsz"][idx])
    asz = float(bbo["asz"][idx])
    if not (math.isfinite(bid) and math.isfinite(ask) and bid > 0.0 and ask > bid):
        return None
    if not (math.isfinite(bsz) and math.isfinite(asz) and bsz >= 0.0 and asz >= 0.0):
        return None
    return bid, ask, bsz, asz, obs_ms


def _sync_quote_with_tape(
    quote: tuple[float, float, float, float, int] | None,
    trades: dict[str, Sequence[Any]] | None,
    t_ms: int,
    sync_threshold_ms: int = 500,
) -> tuple[float, float, float, float, int] | None:
    """For >500ms snapshot BBOs, re-center mid on newer public_trades prints to avoid stale-book illusion."""
    if quote is None or trades is None or len(trades["ts"]) == 0:
        return quote
    bid, ask, bsz, asz, obs_ms = quote
    if t_ms - obs_ms <= sync_threshold_ms:
        return quote
    ts_arr = trades["ts"]
    idx = _bisect_right(ts_arr, t_ms) - 1
    if idx < 0:
        return quote
    tr_ms = int(ts_arr[idx])
    if tr_ms <= obs_ms:
        return quote
    tr_px = float(trades["px"][idx])
    if not (tr_px > 0.0 and math.isfinite(tr_px)):
        return quote
    half_spr = max(0.5 * (ask - bid), 1e-9)
    new_bid = max(tr_px - half_spr, 1e-9)
    new_ask = max(tr_px + half_spr, new_bid + 2e-9)
    return new_bid, new_ask, bsz, asz, tr_ms


def _lookup_bbo_with_recovery(
    bbo: dict[str, Sequence[Any]],
    t_ms: int,
    max_age_ms: int,
    max_recovery_ms: int = MAX_RECOVERY_MS,
    *,
    retry_latency_ms: int = TAKER_LATENCY_MS,
    cutoff_ms: int | None = None,
    trades: dict[str, Sequence[Any]] | None = None,
) -> tuple[float, float, float, float, int] | None:
    """Lookup L1 BBO at `t_ms`; on miss, schedule delayed retry `recovery_ts + retry_latency_ms` within `cutoff_ms`."""
    if cutoff_ms is not None and t_ms > cutoff_ms:
        return None
    hit = _lookup_bbo_at(bbo, t_ms, max_age_ms)
    if hit is not None:
        return _sync_quote_with_tape(hit, trades, t_ms) if trades is not None else hit
    ts_arr = bbo["ts"]
    idx = _bisect_right(ts_arr, t_ms)
    n = len(ts_arr)
    while idx < n:
        cand_ms = int(ts_arr[idx])
        if cand_ms - t_ms > max_recovery_ms:
            return None
        if cutoff_ms is not None and cand_ms > cutoff_ms:
            return None
        bid = float(bbo["bid"][idx])
        ask = float(bbo["ask"][idx])
        bsz = float(bbo["bsz"][idx])
        asz = float(bbo["asz"][idx])
        if (
            math.isfinite(bid)
            and math.isfinite(ask)
            and bid > 0.0
            and ask > bid
            and math.isfinite(bsz)
            and math.isfinite(asz)
            and bsz >= 0.0
            and asz >= 0.0
        ):
            t_retry = cand_ms + max(0, int(retry_latency_ms))
            if cutoff_ms is not None and t_retry > cutoff_ms:
                return None
            retry_hit = _lookup_bbo_at(bbo, t_retry, max_age_ms)
            if retry_hit is None:
                return None
            if trades is not None:
                retry_hit = _sync_quote_with_tape(retry_hit, trades, t_retry)
                if retry_hit is None:
                    return None
            r_bid, r_ask, r_bsz, r_asz, _ = retry_hit
            return r_bid, r_ask, r_bsz, r_asz, t_retry
        idx += 1
    return None


def _normalize_settled_funding(
    funding_input: Any,
) -> tuple[list[tuple[int, int, float]], dict[int, list[tuple[int, float]]]]:
    """Normalize settled funding inputs into causal `(available_at_ms, pay_ms, rate_bps)` lists.

    Records with missing (`None`) or non-finite `available_at_ms` / `rate_bps` are strictly ignored.
    """
    events: list[tuple[int, int, float]] = []
    if not funding_input:
        return [], {}
    if isinstance(funding_input, dict):
        for ht_raw, val in funding_input.items():
            ht = int(ht_raw)
            if isinstance(val, dict):
                av_raw = val.get("available_at_ms")
                rate_raw = val.get("rate_bps")
            elif isinstance(val, (tuple, list)):
                if len(val) < 2:
                    continue
                rate_raw, av_raw = val[0], val[1]
            else:
                rate_raw, av_raw = val, ht
            if av_raw is None or rate_raw is None:
                continue
            try:
                av = int(av_raw)
                rate = float(rate_raw)
            except (TypeError, ValueError):
                continue
            if math.isfinite(rate):
                events.append((av, ht, rate))
    else:
        for item in funding_input:
            if not isinstance(item, dict):
                continue
            ht_raw = item.get("pay_ms")
            av_raw = item.get("available_at_ms")
            rate_raw = item.get("rate_bps")
            if ht_raw is None or av_raw is None or rate_raw is None:
                continue
            try:
                ht = int(ht_raw)
                av = int(av_raw)
                rate = float(rate_raw)
            except (TypeError, ValueError):
                continue
            if math.isfinite(rate):
                events.append((av, ht, rate))

    events.sort(key=lambda x: (x[0], x[1]))
    by_pay: dict[int, list[tuple[int, float]]] = {}
    for av, ht, rate in events:
        by_pay.setdefault(ht, []).append((av, rate))
    return events, by_pay


def _normalize_live_ctx(live_ctx_input: Any) -> tuple[Sequence[int], Sequence[float]] | None:
    """Normalize live/predicted funding context into time-ordered `(ts_arr, rate_arr)` keyed by `available_at_ms`."""
    if not live_ctx_input:
        return None
    if isinstance(live_ctx_input, dict) and "ts" in live_ctx_input and "rate" in live_ctx_input:
        ts_arr = live_ctx_input["ts"]
        rate_arr = live_ctx_input["rate"]
        if len(ts_arr) == 0 or len(ts_arr) != len(rate_arr):
            return None
        return ts_arr, rate_arr
    if isinstance(live_ctx_input, dict):
        pairs: list[tuple[int, float]] = []
        for k, v in live_ctx_input.items():
            if v is None:
                continue
            try:
                ts_v = int(k)
                r_v = float(v)
            except (TypeError, ValueError):
                continue
            if math.isfinite(r_v):
                pairs.append((ts_v, r_v))
        if not pairs:
            return None
        pairs.sort(key=lambda x: x[0])
        return [p[0] for p in pairs], [p[1] for p in pairs]
    pairs_seq: list[tuple[int, float]] = []
    for item in live_ctx_input:
        if isinstance(item, dict):
            av_raw = item.get("available_at_ms")
            r_raw = item.get("rate_bps")
        elif isinstance(item, (tuple, list)) and len(item) >= 2:
            av_raw, r_raw = item[0], item[1]
        else:
            continue
        if av_raw is None or r_raw is None:
            continue
        try:
            ts_v = int(av_raw)
            r_v = float(r_raw)
        except (TypeError, ValueError):
            continue
        if math.isfinite(r_v):
            pairs_seq.append((ts_v, r_v))
    if not pairs_seq:
        return None
    pairs_seq.sort(key=lambda x: x[0])
    return [p[0] for p in pairs_seq], [p[1] for p in pairs_seq]


def _lookup_live_ctx_at(
    live_ctx: tuple[Sequence[int], Sequence[float]] | None,
    t_dec: int,
    max_age_ms: int = HOUR_MS,
) -> float | None:
    """Strictly causal live/predicted funding lookup for `available_at_ms <= t_dec`."""
    if live_ctx is None:
        return None
    ts_arr, rate_arr = live_ctx
    idx = _bisect_right(ts_arr, t_dec) - 1
    if idx < 0:
        return None
    avail_ms = int(ts_arr[idx])
    if t_dec - avail_ms > max_age_ms:
        return None
    rate = float(rate_arr[idx])
    return rate if math.isfinite(rate) else None


def _find_passive_fill(
    trades: dict[str, Sequence[Any]] | None,
    t_start_ms: int,
    t_end_ms: int,
    side_buy: bool,
    rest_px: float,
    target_notional_usd: float,
) -> tuple[int, float] | None:
    """Strictly print-verified passive limit fill via `public_trades` trade-through."""
    if trades is None or len(trades["ts"]) == 0:
        return None
    ts_arr = trades["ts"]
    sgn_arr = trades["sgn"]
    px_arr = trades["px"]
    sz_arr = trades["sz"]
    lo = _bisect_left(ts_arr, t_start_ms)
    hi = _bisect_right(ts_arr, t_end_ms)
    if hi <= lo:
        return None
    through_notl = 0.0
    first_fill_ms: int | None = None
    for idx in range(lo, hi):
        sg = int(sgn_arr[idx])
        px = float(px_arr[idx])
        sz = float(sz_arr[idx])
        if not (px > 0.0 and sz > 0.0 and math.isfinite(px) and math.isfinite(sz)):
            continue
        if (side_buy and sg == -1 and px < rest_px) or ((not side_buy) and sg == 1 and px > rest_px):
            if first_fill_ms is None:
                first_fill_ms = int(ts_arr[idx])
            through_notl += px * sz
            if through_notl >= target_notional_usd:
                return first_fill_ms, target_notional_usd
    if first_fill_ms is not None and through_notl >= MIN_ENTRY_NOTIONAL_USD:
        return first_fill_ms, min(target_notional_usd, through_notl)
    return None


def precompute_pair_timeline(
    *,
    bbo_a: dict[str, Sequence[Any]],
    bbo_b: dict[str, Sequence[Any]],
    trades_a: dict[str, Sequence[Any]] | None = None,
    trades_b: dict[str, Sequence[Any]] | None = None,
    windows: Sequence[dict[str, Any]],
    max_age_a_ms: int,
    max_age_b_ms: int,
    basis_fair_tau_h: float = 6.0,
    basis_vol_tau_h: float = 2.0,
    step_ms: int = STEP_MS,
) -> dict[int, list[tuple[int, float, float, float, float, float, float, float, float, float, float | None, float]]]:
    """Precompute aligned valid step tuples and causal basis EMA/sigma per window once per pair."""
    basis_fair_ema: float | None = None
    basis_var_bps2: float | None = None
    last_book_ms: int | None = None
    by_win: dict[int, list[tuple[int, float, float, float, float, float, float, float, float, float, float | None, float]]] = {}

    for w in sorted(windows, key=lambda x: int(x["window_num"])):
        wn = int(w["window_num"])
        t_start = int(w["t0_ms"])
        t_end = int(w["t1_ms"])
        rows: list[tuple[int, float, float, float, float, float, float, float, float, float, float | None, float]] = []
        t_cur = t_start
        while t_cur <= t_end:
            qa = _sync_quote_with_tape(_lookup_bbo_at(bbo_a, t_cur, max_age_a_ms), trades_a, t_cur)
            qb = _sync_quote_with_tape(_lookup_bbo_at(bbo_b, t_cur, max_age_b_ms), trades_b, t_cur)
            if qa is None or qb is None:
                t_cur += step_ms
                continue
            abid, aask, absz, aasz, _ = qa
            bbid, bask, bbsz, basz, _ = qb
            mid_a = 0.5 * (abid + aask)
            mid_b = 0.5 * (bbid + bask)
            ref_mid = 0.5 * (mid_a + mid_b)
            if not (ref_mid > 0.0 and math.isfinite(ref_mid)):
                t_cur += step_ms
                continue
            raw_mid_basis_ab = ((mid_a - mid_b) / ref_mid) * 1e4
            prior_fair = basis_fair_ema
            prior_sigma = (
                math.sqrt(basis_var_bps2)
                if (basis_var_bps2 is not None and basis_var_bps2 > 0.0)
                else 0.0
            )
            if basis_fair_ema is not None and last_book_ms is not None and (t_cur - last_book_ms) <= MAX_GAP_MS:
                dt_h = max(0.0, (t_cur - last_book_ms) / HOUR_MS)
                if dt_h > 0.0:
                    af = 1.0 - math.exp(-dt_h / max(basis_fair_tau_h, 0.05))
                    av = 1.0 - math.exp(-dt_h / max(basis_vol_tau_h, 0.05))
                    dev = raw_mid_basis_ab - basis_fair_ema
                    basis_var_bps2 = (
                        dev * dev
                        if basis_var_bps2 is None
                        else (1.0 - av) * basis_var_bps2 + av * dev * dev
                    )
                    basis_fair_ema = basis_fair_ema + af * dev
            else:
                basis_fair_ema = raw_mid_basis_ab
                basis_var_bps2 = None
            last_book_ms = t_cur
            rows.append(
                (
                    t_cur,
                    abid,
                    aask,
                    absz,
                    aasz,
                    bbid,
                    bask,
                    bbsz,
                    basz,
                    raw_mid_basis_ab,
                    prior_fair,
                    prior_sigma,
                )
            )
            t_cur += step_ms
        by_win[wn] = rows
    return by_win


def simulate_carry_pair_timeline(
    *,
    pair: CarryPairSpec,
    bbo_a: dict[str, Sequence[Any]],
    bbo_b: dict[str, Sequence[Any]],
    trades_a: dict[str, Sequence[Any]] | None,
    trades_b: dict[str, Sequence[Any]] | None,
    funding_a: Any,
    funding_b: Any,
    live_ctx_a: Any,
    live_ctx_b: Any,
    windows: Sequence[dict[str, Any]],
    fee_a_mk_bps: float,
    fee_a_tk_bps: float,
    fee_b_mk_bps: float,
    fee_b_tk_bps: float,
    max_age_a_ms: int,
    max_age_b_ms: int,
    params: CarryArmParams,
    fx_drag_bps: float = 0.0,
    target_notional_usd: float = TARGET_PAIR_NOTIONAL_USD,
    step_ms: int = STEP_MS,
    precomputed_by_win: dict[int, list[tuple[int, float, float, float, float, float, float, float, float, float, float | None, float]]] | None = None,
    record_trades: bool = False,
) -> dict[str, Any]:
    """Simulate a single pair causally across W01..W26 windows with real L1 books & funding."""
    fx_eff = fx_drag_bps if pair.cross_stablecoin else 0.0
    avail_ev_a, pay_ev_a = _normalize_settled_funding(funding_a)
    avail_ev_b, pay_ev_b = _normalize_settled_funding(funding_b)
    hours_a = sorted(pay_ev_a)
    hours_b = sorted(pay_ev_b)
    live_norm_a = _normalize_live_ctx(live_ctx_a)
    live_norm_b = _normalize_live_ctx(live_ctx_b)

    # Build causal funding state tracker filtered strictly by `available_at_ms <= t_dec`.
    ev_idx_a = 0
    ev_idx_b = 0
    seen_a_by_pay: dict[int, float] = {}
    seen_b_by_pay: dict[int, float] = {}
    visible_pair_diffs: dict[int, float] = {}
    funding_ema: float | None = None
    last_funding_ms: int | None = None

    pos_dir = 0  # +1 = Short A / Long B; -1 = Long A / Short B; 0 = flat
    pos_qty_base = 0.0
    pos_notional_usd = 0.0
    entry_ms = 0
    leg_a_fill_ms = 0
    leg_b_fill_ms = 0
    entry_mid_a = 0.0
    entry_mid_b = 0.0
    entry_exec_a = 0.0
    entry_exec_b = 0.0
    entry_mid_basis_bps = 0.0
    entry_passive_is_a: bool | None = None
    leg_a_settled_cash: dict[int, float] = {}
    leg_b_settled_cash: dict[int, float] = {}
    cur_leg_a_funding_usd = 0.0
    cur_leg_b_funding_usd = 0.0

    # Explicit tracking of any unclosed orphan single-leg exposures awaiting book recovery.
    open_orphans: list[dict[str, Any]] = []
    trade_events: list[dict[str, Any]] = []

    per_window_rows: list[dict[str, Any]] = []
    terminal_flat_all = True
    terminal_residual_notional_usd = 0.0

    # Group windows by split so split boundaries force clean terminal flat & zero cross-split leakage.
    split_order = ("selection_dev", "holdout", "w26_oos")
    windows_by_split: dict[str, list[dict[str, Any]]] = {s: [] for s in split_order}
    for w in sorted(windows, key=lambda x: int(x["window_num"])):
        windows_by_split[w["split"]].append(w)

    def _recompute_funding_ema() -> None:
        nonlocal funding_ema, last_funding_ms
        f_ema: float | None = None
        last_ms: int | None = None
        for ht in sorted(visible_pair_diffs):
            diff = visible_pair_diffs[ht]
            if f_ema is not None and last_ms is not None and (ht - last_ms) <= MAX_GAP_MS:
                dt_h = max(0.0, (ht - last_ms) / HOUR_MS)
                alpha_f = 1.0 - math.exp(-dt_h / max(params.funding_tau_h, 0.25))
                f_ema = f_ema + alpha_f * (diff - f_ema)
            else:
                f_ema = diff
            last_ms = ht
        funding_ema = f_ema
        last_funding_ms = last_ms

    def _advance_funding(up_to_ms: int) -> None:
        nonlocal ev_idx_a, ev_idx_b, funding_ema, last_funding_ms
        changed_pays: set[int] = set()
        while ev_idx_a < len(avail_ev_a) and avail_ev_a[ev_idx_a][0] <= up_to_ms:
            _, ht, rate = avail_ev_a[ev_idx_a]
            seen_a_by_pay[ht] = rate
            if ht in seen_b_by_pay:
                changed_pays.add(ht)
            ev_idx_a += 1
        while ev_idx_b < len(avail_ev_b) and avail_ev_b[ev_idx_b][0] <= up_to_ms:
            _, ht, rate = avail_ev_b[ev_idx_b]
            seen_b_by_pay[ht] = rate
            if ht in seen_a_by_pay:
                changed_pays.add(ht)
            ev_idx_b += 1
        if not changed_pays:
            return
        need_recompute = False
        for ht in sorted(changed_pays):
            diff = seen_a_by_pay[ht] - seen_b_by_pay[ht]
            was_present = ht in visible_pair_diffs
            visible_pair_diffs[ht] = diff
            if was_present or (last_funding_ms is not None and ht <= last_funding_ms):
                need_recompute = True
            elif not need_recompute:
                if funding_ema is not None and last_funding_ms is not None and (ht - last_funding_ms) <= MAX_GAP_MS:
                    dt_h = max(0.0, (ht - last_funding_ms) / HOUR_MS)
                    alpha_f = 1.0 - math.exp(-dt_h / max(params.funding_tau_h, 0.25))
                    funding_ema = funding_ema + alpha_f * (diff - funding_ema)
                else:
                    funding_ema = diff
                last_funding_ms = ht
        if need_recompute:
            _recompute_funding_ema()

    def _settle_single_leg_funding(
        *,
        is_leg_a: bool,
        receipt_sign: int,
        qty_base: float,
        fallback_notl: float,
        fill_ms: int,
        up_to_ms: int,
        settled_cash_by_pay: dict[int, float],
        win_acc: dict[str, Any],
        settled_set: set[int],
        avail_cap_ms: int | None = None,
    ) -> float:
        """Settle single-leg funding strictly for `fill_ms < ht <= up_to_ms` with known `available_at_ms`."""
        hours = hours_a if is_leg_a else hours_b
        pay_map = pay_ev_a if is_leg_a else pay_ev_b
        bbo = bbo_a if is_leg_a else bbo_b
        max_age = max_age_a_ms if is_leg_a else max_age_b_ms
        acc_cash = 0.0
        lo = bisect_right(hours, fill_ms)
        hi = bisect_right(hours, up_to_ms)
        for idx in range(lo, hi):
            ht = hours[idx]
            if ht <= fill_ms:
                continue
            revs = pay_map.get(ht)
            if not revs:
                continue
            chosen_rate: float | None = None
            for av_ms, r_bps in revs:
                if avail_cap_ms is None or av_ms <= avail_cap_ms:
                    chosen_rate = r_bps
                else:
                    break
            if chosen_rate is None:
                continue
            q = _lookup_bbo_at(bbo, ht, max(max_age, HOUR_MS))
            if q is not None:
                notl_t = qty_base * (0.5 * (q[0] + q[1]))
            else:
                notl_t = fallback_notl
            cash_f = receipt_sign * notl_t * chosen_rate * 1e-4
            is_new = ht not in settled_cash_by_pay
            delta = cash_f - settled_cash_by_pay.get(ht, 0.0)
            settled_cash_by_pay[ht] = cash_f
            win_acc["funding_cash_usd"] += delta
            acc_cash += delta
            if is_leg_a:
                win_acc["leg_a_funding_cash_usd"] += delta
                if is_new:
                    win_acc["leg_a_funding_payments"] += 1
            else:
                win_acc["leg_b_funding_cash_usd"] += delta
                if is_new:
                    win_acc["leg_b_funding_payments"] += 1
            if ht not in settled_set:
                settled_set.add(ht)
                win_acc["funding_payments"] += 1
        return acc_cash

    def _settle_open_position_funding(
        up_to_a_ms: int,
        up_to_b_ms: int,
        win_acc: dict[str, Any],
        settled_set: set[int],
        *,
        avail_cap_ms: int | None = None,
    ) -> None:
        nonlocal cur_leg_a_funding_usd, cur_leg_b_funding_usd
        if pos_dir == 0:
            return
        ca = _settle_single_leg_funding(
            is_leg_a=True,
            receipt_sign=pos_dir,
            qty_base=pos_qty_base,
            fallback_notl=pos_notional_usd,
            fill_ms=leg_a_fill_ms,
            up_to_ms=up_to_a_ms,
            settled_cash_by_pay=leg_a_settled_cash,
            win_acc=win_acc,
            settled_set=settled_set,
            avail_cap_ms=avail_cap_ms,
        )
        cb = _settle_single_leg_funding(
            is_leg_a=False,
            receipt_sign=-pos_dir,
            qty_base=pos_qty_base,
            fallback_notl=pos_notional_usd,
            fill_ms=leg_b_fill_ms,
            up_to_ms=up_to_b_ms,
            settled_cash_by_pay=leg_b_settled_cash,
            win_acc=win_acc,
            settled_set=settled_set,
            avail_cap_ms=avail_cap_ms,
        )
        cur_leg_a_funding_usd += ca
        cur_leg_b_funding_usd += cb

    def _try_unwind_orphan(
        orp: dict[str, Any],
        t_attempt: int,
        win_acc: dict[str, Any],
        settled_set: set[int],
        *,
        is_split_end: bool = False,
        split_end_ms: int | None = None,
    ) -> bool:
        """Attempt to unwind an open orphan single-leg exposure and book exact cash PnL."""
        is_a = bool(orp["is_a"])
        bbo = bbo_a if is_a else bbo_b
        trades = trades_a if is_a else trades_b
        max_age = max_age_a_ms if is_a else max_age_b_ms
        tk_bps = fee_a_tk_bps if is_a else fee_b_tk_bps
        if split_end_ms is not None and t_attempt > split_end_ms:
            cf = _settle_single_leg_funding(
                is_leg_a=is_a,
                receipt_sign=orp["receipt_sign"],
                qty_base=orp["qty_base"],
                fallback_notl=orp["notional_usd"],
                fill_ms=orp["fill_ms"],
                up_to_ms=split_end_ms,
                settled_cash_by_pay=orp["settled_cash"],
                win_acc=win_acc,
                settled_set=settled_set,
                avail_cap_ms=split_end_ms,
            )
            orp["funding_usd"] += cf
            return False
        uq = _lookup_bbo_with_recovery(
            bbo,
            t_attempt,
            max_age,
            cutoff_ms=split_end_ms,
            trades=trades,
        )
        if uq is None:
            settle_cap = min(t_attempt, split_end_ms) if split_end_ms is not None else t_attempt
            cf = _settle_single_leg_funding(
                is_leg_a=is_a,
                receipt_sign=orp["receipt_sign"],
                qty_base=orp["qty_base"],
                fallback_notl=orp["notional_usd"],
                fill_ms=orp["fill_ms"],
                up_to_ms=settle_cap,
                settled_cash_by_pay=orp["settled_cash"],
                win_acc=win_acc,
                settled_set=settled_set,
                avail_cap_ms=split_end_ms,
            )
            orp["funding_usd"] += cf
            return False

        u_bid, u_ask, u_bsz, u_asz, u_obs_ms = uq
        u_exit_ms = max(t_attempt, u_obs_ms)
        if split_end_ms is not None and u_exit_ms > split_end_ms:
            return False
        cf = _settle_single_leg_funding(
            is_leg_a=is_a,
            receipt_sign=orp["receipt_sign"],
            qty_base=orp["qty_base"],
            fallback_notl=orp["notional_usd"],
            fill_ms=orp["fill_ms"],
            up_to_ms=u_exit_ms,
            settled_cash_by_pay=orp["settled_cash"],
            win_acc=win_acc,
            settled_set=settled_set,
            avail_cap_ms=split_end_ms,
        )
        orp["funding_usd"] += cf

        is_short = bool(orp["is_short"])
        u_exec = u_ask if is_short else u_bid
        u_l1 = (u_ask * u_asz) if is_short else (u_bid * u_bsz)
        u_mid = 0.5 * (u_bid + u_ask)
        sgn_pos = -1 if is_short else 1
        qty = float(orp["qty_base"])
        orp_mid_pnl = sgn_pos * qty * (u_mid - float(orp["entry_mid"]))
        orp_exec_pnl = sgn_pos * qty * (u_exec - float(orp["entry_exec"]))
        orp_half_spr = orp_mid_pnl - orp_exec_pnl

        unwind_notl = qty * u_exec
        depth_sweep_usd = 0.0
        if unwind_notl > u_l1:
            if u_l1 <= 0.0:
                sweep_bps = 15.0
            else:
                sweep_bps = min(2.0 * (unwind_notl / u_l1 - 1.0), 25.0)
            depth_sweep_usd = unwind_notl * sweep_bps * 1e-4

        unwind_fee = unwind_notl * tk_bps * 1e-4
        total_slip = orp_half_spr + depth_sweep_usd
        entry_fee = float(orp["entry_fee_usd"])
        orp_total_pnl = float(orp["funding_usd"]) + orp_mid_pnl - total_slip - entry_fee - unwind_fee

        win_acc["basis_pnl_usd"] += orp_mid_pnl
        win_acc["spread_slip_usd"] += total_slip
        win_acc["fees_usd"] += unwind_fee
        win_acc["notional_traded_usd"] += unwind_notl
        win_acc["orphan_unwound_count"] += 1
        win_acc["orphan_unwind_pnl_usd"] += orp_total_pnl
        win_acc["orphan_unwind_cost_usd"] += total_slip + entry_fee + unwind_fee
        if record_trades:
            trade_events.append(
                {
                    "kind": "orphan_unwind",
                    "leg": "A" if is_a else "B",
                    "fill_ms": int(orp["fill_ms"]),
                    "exit_ms": int(u_exit_ms),
                    "orphan_notional_usd": round(float(orp["notional_usd"]), 4),
                    "unwind_notional_usd": round(unwind_notl, 4),
                    "funding_usd": round(float(orp["funding_usd"]), 6),
                    "mid_pnl_usd": round(orp_mid_pnl, 6),
                    "slip_usd": round(total_slip, 6),
                    "fees_usd": round(entry_fee + unwind_fee, 6),
                    "net_pnl_usd": round(orp_total_pnl, 6),
                    "reason": str(orp["reason"]),
                }
            )
        return True

    def _book_orphan_fill(
        *,
        is_a: bool,
        is_short: bool,
        orphan_notl: float,
        fill_ms: int,
        rest_px: float,
        p_mid: float,
        mk_bps: float,
        t_hedge: int,
        reason: str,
        win_acc: dict[str, Any],
        settled_set: set[int],
        split_end_ms: int | None = None,
    ) -> None:
        qty = orphan_notl / rest_px
        entry_fee = orphan_notl * mk_bps * 1e-4
        win_acc["fees_usd"] += entry_fee
        win_acc["notional_traded_usd"] += orphan_notl
        win_acc["orphan_entries"] += 1
        win_acc["orphan_notional_usd"] += orphan_notl
        orp = {
            "is_a": is_a,
            "is_short": is_short,
            "receipt_sign": 1 if is_short else -1,
            "qty_base": qty,
            "notional_usd": orphan_notl,
            "fill_ms": fill_ms,
            "entry_exec": rest_px,
            "entry_mid": p_mid,
            "entry_fee_usd": entry_fee,
            "earliest_unwind_ms": t_hedge + TAKER_LATENCY_MS,
            "settled_cash": {},
            "funding_usd": 0.0,
            "reason": reason,
        }
        if not _try_unwind_orphan(
            orp,
            orp["earliest_unwind_ms"],
            win_acc,
            settled_set,
            split_end_ms=split_end_ms,
        ):
            open_orphans.append(orp)

    def _close_position(
        t_dec: int,
        win_acc: dict[str, Any],
        settled_set: set[int],
        reason: str,
        *,
        is_split_end: bool = False,
        split_end_ms: int | None = None,
    ) -> bool:
        nonlocal pos_dir, pos_qty_base, pos_notional_usd, entry_passive_is_a
        nonlocal leg_a_fill_ms, leg_b_fill_ms, cur_leg_a_funding_usd, cur_leg_b_funding_usd
        if pos_dir == 0:
            return True
        t_exec = t_dec if is_split_end else (t_dec + TAKER_LATENCY_MS)
        if split_end_ms is not None and t_exec > split_end_ms:
            return False
        qa = _lookup_bbo_with_recovery(
            bbo_a,
            t_exec,
            max_age_a_ms,
            cutoff_ms=split_end_ms,
            trades=trades_a,
        )
        qb = _lookup_bbo_with_recovery(
            bbo_b,
            t_exec,
            max_age_b_ms,
            cutoff_ms=split_end_ms,
            trades=trades_b,
        )
        if qa is None or qb is None:
            return False

        leg_a_exit_ms = max(t_exec, qa[4])
        leg_b_exit_ms = max(t_exec, qb[4])
        if split_end_ms is not None and (leg_a_exit_ms > split_end_ms or leg_b_exit_ms > split_end_ms):
            return False

        # Settle per-leg hourly funding strictly in `(leg_fill_ms, leg_exit_ms]`
        _settle_open_position_funding(
            leg_a_exit_ms,
            leg_b_exit_ms,
            win_acc,
            settled_set,
            avail_cap_ms=split_end_ms,
        )

        abid, aask, absz, aasz, _ = qa
        bbid, bask, bbsz, basz, _ = qb
        exit_mid_a = 0.5 * (abid + aask)
        exit_mid_b = 0.5 * (bbid + bask)

        # If pos_dir == +1 (Short A / Long B): exit buys A at ask, sells B at bid.
        # If pos_dir == -1 (Long A / Short B): exit sells A at bid, buys B at ask.
        if pos_dir == 1:
            exit_exec_a = aask
            exit_exec_b = bbid
            l1_a_usd = aask * aasz
            l1_b_usd = bbid * bbsz
        else:
            exit_exec_a = abid
            exit_exec_b = bask
            l1_a_usd = abid * absz
            l1_b_usd = bask * basz

        # Exact mid-to-mid basis PnL across entry -> exit:
        # pos_dir == +1 (short A, long B): (entry_mid_a - exit_mid_a)*q + (exit_mid_b - entry_mid_b)*q
        basis_mid_pnl = pos_dir * pos_qty_base * (
            (entry_mid_a - exit_mid_a) + (exit_mid_b - entry_mid_b)
        )

        # Half-spread crossing slip on entry + exit (exact: mid_pnl - spread_slip == actual exec price pnl):
        entry_exec_pnl = pos_dir * pos_qty_base * (
            (entry_exec_a - exit_exec_a) + (exit_exec_b - entry_exec_b)
        )
        spread_slip_usd = basis_mid_pnl - entry_exec_pnl

        # Actual L1 depth sweep impact on exit if position exceeds displayed L1 size:
        exit_notl_a = pos_qty_base * exit_exec_a
        exit_notl_b = pos_qty_base * exit_exec_b
        depth_sweep_usd = 0.0
        for leg_notl, l1_notl in ((exit_notl_a, l1_a_usd), (exit_notl_b, l1_b_usd)):
            if leg_notl > l1_notl:
                if l1_notl <= 0.0:
                    sweep_bps = 15.0
                else:
                    sweep_bps = min(2.0 * (leg_notl / l1_notl - 1.0), 25.0)
                depth_sweep_usd += leg_notl * sweep_bps * 1e-4
                win_acc["depth_clipped_exits"] += 1

        spread_slip_usd += depth_sweep_usd

        # Exact entry + exit fees
        entry_notl_a = pos_qty_base * entry_exec_a
        entry_notl_b = pos_qty_base * entry_exec_b
        if entry_passive_is_a is True:
            entry_fee = entry_notl_a * fee_a_mk_bps * 1e-4 + entry_notl_b * fee_b_tk_bps * 1e-4
        elif entry_passive_is_a is False:
            entry_fee = entry_notl_a * fee_a_tk_bps * 1e-4 + entry_notl_b * fee_b_mk_bps * 1e-4
        else:
            entry_fee = entry_notl_a * fee_a_tk_bps * 1e-4 + entry_notl_b * fee_b_tk_bps * 1e-4
        exit_fee = exit_notl_a * fee_a_tk_bps * 1e-4 + exit_notl_b * fee_b_tk_bps * 1e-4
        fees = entry_fee + exit_fee

        # FX drag on cross-stablecoin pairs (fx_eff bps round-trip on traded leg notional)
        avg_notl = 0.5 * (pos_notional_usd + 0.5 * (exit_notl_a + exit_notl_b))
        fx_cost = avg_notl * fx_eff * 1e-4 if fx_eff > 0.0 else 0.0

        win_acc["basis_pnl_usd"] += basis_mid_pnl
        win_acc["spread_slip_usd"] += spread_slip_usd
        win_acc["fees_usd"] += fees
        win_acc["fx_drag_usd"] += fx_cost
        win_acc["notional_traded_usd"] += pos_notional_usd + 0.5 * (exit_notl_a + exit_notl_b)
        win_acc["trades_closed"] += 1
        win_acc["exit_reasons"][reason] = win_acc["exit_reasons"].get(reason, 0) + 1

        if record_trades:
            trade_events.append(
                {
                    "kind": "matched_pair",
                    "pos_dir": pos_dir,
                    "leg_a_fill_ms": leg_a_fill_ms,
                    "leg_b_fill_ms": leg_b_fill_ms,
                    "leg_a_exit_ms": leg_a_exit_ms,
                    "leg_b_exit_ms": leg_b_exit_ms,
                    "qty_base": round(pos_qty_base, 8),
                    "hedged_notional_usd": round(pos_notional_usd, 4),
                    "leg_a_funding_usd": round(cur_leg_a_funding_usd, 6),
                    "leg_b_funding_usd": round(cur_leg_b_funding_usd, 6),
                    "basis_mid_pnl_usd": round(basis_mid_pnl, 6),
                    "spread_slip_usd": round(spread_slip_usd, 6),
                    "fees_usd": round(fees, 6),
                    "fx_cost_usd": round(fx_cost, 6),
                    "reason": reason,
                }
            )

        pos_dir = 0
        pos_qty_base = 0.0
        pos_notional_usd = 0.0
        leg_a_fill_ms = 0
        leg_b_fill_ms = 0
        leg_a_settled_cash = {}
        leg_b_settled_cash = {}
        cur_leg_a_funding_usd = 0.0
        cur_leg_b_funding_usd = 0.0
        entry_passive_is_a = None
        return True

    for split_name in split_order:
        split_wins = windows_by_split[split_name]
        if not split_wins:
            continue
        # Reset open position at split start (never carry state across dev -> holdout -> w26_oos)
        pos_dir = 0
        pos_qty_base = 0.0
        pos_notional_usd = 0.0
        leg_a_fill_ms = 0
        leg_b_fill_ms = 0
        leg_a_settled_cash = {}
        leg_b_settled_cash = {}
        cur_leg_a_funding_usd = 0.0
        cur_leg_b_funding_usd = 0.0
        entry_passive_is_a = None
        open_orphans = []
        last_prev_t: int | None = None
        split_end_cutoff_ms = int(split_wins[-1]["t1_ms"])

        for w_idx, w in enumerate(split_wins):
            t_start = int(w["t0_ms"])
            t_end = int(w["t1_ms"])
            settled_in_win: set[int] = set()
            win_acc: dict[str, Any] = {
                "window": w["window"],
                "window_num": int(w["window_num"]),
                "split": split_name,
                "funding_cash_usd": 0.0,
                "leg_a_funding_cash_usd": 0.0,
                "leg_b_funding_cash_usd": 0.0,
                "basis_pnl_usd": 0.0,
                "spread_slip_usd": 0.0,
                "fees_usd": 0.0,
                "fx_drag_usd": 0.0,
                "net_pnl_usd": 0.0,
                "notional_traded_usd": 0.0,
                "trades_entered": 0,
                "trades_closed": 0,
                "funding_payments": 0,
                "leg_a_funding_payments": 0,
                "leg_b_funding_payments": 0,
                "passive_attempts": 0,
                "passive_fills": 0,
                "depth_clipped_entries": 0,
                "depth_clipped_exits": 0,
                "orphan_entries": 0,
                "orphan_unwound_count": 0,
                "orphan_notional_usd": 0.0,
                "orphan_unwind_pnl_usd": 0.0,
                "orphan_unwind_cost_usd": 0.0,
                "orphan_open_residual_usd": 0.0,
                "position_open_residual_usd": 0.0,
                "terminal_residual_mtm_usd": 0.0,
                "terminal_residual_mtm_unknown": False,
                "exit_reasons": {},
            }

            if last_prev_t is not None and (t_start - last_prev_t) > MAX_GAP_MS:
                if pos_dir != 0:
                    _close_position(
                        last_prev_t,
                        win_acc,
                        settled_in_win,
                        "gap_force_flat",
                        is_split_end=True,
                        split_end_ms=last_prev_t,
                    )
                if open_orphans:
                    rem_orphans: list[dict[str, Any]] = []
                    for orp in open_orphans:
                        if not _try_unwind_orphan(
                            orp,
                            max(last_prev_t, int(orp["earliest_unwind_ms"])),
                            win_acc,
                            settled_in_win,
                            is_split_end=True,
                            split_end_ms=last_prev_t,
                        ):
                            rem_orphans.append(orp)
                    open_orphans = rem_orphans

            win_steps = (
                precomputed_by_win[int(w["window_num"])]
                if precomputed_by_win is not None
                else precompute_pair_timeline(
                    bbo_a=bbo_a,
                    bbo_b=bbo_b,
                    windows=[w],
                    max_age_a_ms=max_age_a_ms,
                    max_age_b_ms=max_age_b_ms,
                    basis_fair_tau_h=params.basis_fair_tau_h,
                    basis_vol_tau_h=params.basis_vol_tau_h,
                    step_ms=step_ms,
                )[int(w["window_num"])]
            )
            for (
                t_cur,
                abid,
                aask,
                absz,
                aasz,
                bbid,
                bask,
                bbsz,
                basz,
                raw_mid_basis_ab,
                prior_fair,
                prior_sigma,
            ) in win_steps:
                _advance_funding(t_cur)
                if open_orphans:
                    rem_orphans = []
                    for orp in open_orphans:
                        t_try = max(t_cur, int(orp["earliest_unwind_ms"]))
                        if t_try > t_cur or not _try_unwind_orphan(
                            orp,
                            t_try,
                            win_acc,
                            settled_in_win,
                            split_end_ms=split_end_cutoff_ms,
                        ):
                            rem_orphans.append(orp)
                    open_orphans = rem_orphans
                if pos_dir != 0:
                    _settle_open_position_funding(
                        t_cur,
                        t_cur,
                        win_acc,
                        settled_in_win,
                        avail_cap_ms=t_cur,
                    )
                mid_a = 0.5 * (abid + aask)
                mid_b = 0.5 * (bbid + bask)

                # Separate settled signal EMA from live/predicted funding rate (`available_at_ms <= t_cur`).
                eff_f_ab = funding_ema
                if eff_f_ab is None and live_norm_a is not None and live_norm_b is not None:
                    ca_t = _lookup_live_ctx_at(live_norm_a, t_cur)
                    cb_t = _lookup_live_ctx_at(live_norm_b, t_cur)
                    if ca_t is not None and cb_t is not None:
                        eff_f_ab = ca_t - cb_t

                if pos_dir != 0:
                    if t_cur <= entry_ms:
                        continue
                    dir_mid = pos_dir * raw_mid_basis_ab
                    dir_fair = pos_dir * (prior_fair if prior_fair is not None else raw_mid_basis_ab)
                    denom = max(prior_sigma, params.basis_vol_floor_bps)
                    dir_z = (dir_mid - dir_fair) / denom
                    conv_bps = entry_mid_basis_bps - dir_mid
                    dir_fund = pos_dir * (eff_f_ab if eff_f_ab is not None else 0.0)
                    held_h = (t_cur - entry_ms) / HOUR_MS

                    exit_reason: str | None = None
                    if eff_f_ab is None or dir_fund <= params.adverse_funding_exit_bps_hr:
                        exit_reason = "adverse_funding_flip"
                    elif -conv_bps >= params.max_basis_stop_bps:
                        exit_reason = "basis_stop_loss"
                    elif (
                        params.take_profit_basis_bps > 0.0
                        and conv_bps >= params.take_profit_basis_bps
                        and dir_z <= params.take_profit_zscore
                    ):
                        exit_reason = "basis_take_profit"
                    elif params.max_hold_h > 0.0 and held_h >= params.max_hold_h:
                        exit_reason = "max_hold_expired"

                    if exit_reason is not None:
                        _close_position(
                            t_cur,
                            win_acc,
                            settled_in_win,
                            exit_reason,
                            split_end_ms=split_end_cutoff_ms,
                        )

                elif (
                    not open_orphans
                    and eff_f_ab is not None
                    and prior_fair is not None
                    and last_funding_ms is not None
                    and (t_cur - last_funding_ms) <= MAX_GAP_MS
                    and t_cur + HOUR_MS <= t_end
                ):
                    cand_dir = 1 if eff_f_ab >= 0.0 else -1
                    expected_carry_bps = (cand_dir * eff_f_ab) * params.horizon_h
                    dir_mid = cand_dir * raw_mid_basis_ab
                    dir_fair = cand_dir * prior_fair
                    denom = max(prior_sigma, params.basis_vol_floor_bps)
                    basis_z = (dir_mid - dir_fair) / denom
                    timing_adv = max(
                        -params.basis_timing_cap_bps,
                        min(params.basis_timing_cap_bps, (dir_mid - dir_fair) * params.basis_timing_weight),
                    )

                    spr_a = ((aask - abid) / mid_a) * 1e4
                    spr_b = ((bask - bbid) / mid_b) * 1e4
                    passive_is_a = spr_a >= spr_b

                    if params.passive_first:
                        entry_fee_bps = (fee_a_mk_bps + fee_b_tk_bps) if passive_is_a else (fee_b_mk_bps + fee_a_tk_bps)
                        rt_fee_bps = entry_fee_bps + (fee_a_tk_bps + fee_b_tk_bps)
                        rt_spread_bps = 0.5 * max(spr_a, spr_b) + 1.0 * min(spr_a, spr_b)
                    else:
                        rt_fee_bps = 2.0 * (fee_a_tk_bps + fee_b_tk_bps)
                        rt_spread_bps = spr_a + spr_b

                    hurdle_bps = params.hurdle_mult * (rt_fee_bps + rt_spread_bps + fx_eff)

                    if (
                        expected_carry_bps >= params.min_expected_carry_bps
                        and basis_z >= params.min_basis_zscore
                        and (expected_carry_bps + timing_adv) >= hurdle_bps
                    ):
                        # Check L1 depth at decision time
                        l1_a = (abid * absz) if cand_dir == 1 else (aask * aasz)
                        l1_b = (bask * basz) if cand_dir == 1 else (bbid * bbsz)
                        if l1_a >= params.min_l1_notional_usd and l1_b >= params.min_l1_notional_usd:
                            if params.passive_first:
                                win_acc["passive_attempts"] += 1
                                t_post = t_cur + PASSIVE_LATENCY_MS
                                post_book = _sync_quote_with_tape(
                                    _lookup_bbo_at(
                                        bbo_a if passive_is_a else bbo_b,
                                        t_post,
                                        max_age_a_ms if passive_is_a else max_age_b_ms,
                                    ),
                                    trades_a if passive_is_a else trades_b,
                                    t_post,
                                )
                                opp_anchor = _sync_quote_with_tape(
                                    _lookup_bbo_at(
                                        bbo_b if passive_is_a else bbo_a,
                                        t_post,
                                        max_age_b_ms if passive_is_a else max_age_a_ms,
                                    ),
                                    trades_b if passive_is_a else trades_a,
                                    t_post,
                                )
                                if post_book is not None and opp_anchor is not None:
                                    # Sell passive at ask if passive leg is short, buy passive at bid if long
                                    passive_is_short = (cand_dir == 1 and passive_is_a) or (cand_dir == -1 and not passive_is_a)
                                    rest_px = post_book[1] if passive_is_short else post_book[0]
                                    fill_res = _find_passive_fill(
                                        trades_a if passive_is_a else trades_b,
                                        t_post,
                                        t_post + params.max_passive_wait_ms,
                                        side_buy=(not passive_is_short),
                                        rest_px=rest_px,
                                        target_notional_usd=target_notional_usd,
                                    )
                                    if fill_res is not None:
                                        fill_ms, filled_notl = fill_res
                                        win_acc["passive_fills"] += 1
                                        p_mid = 0.5 * (post_book[0] + post_book[1])
                                        p_mk_bps = fee_a_mk_bps if passive_is_a else fee_b_mk_bps
                                        # Hedge opposite leg at fill_ms + 330 ms
                                        t_hedge = fill_ms + TAKER_LATENCY_MS
                                        hq = _lookup_bbo_with_recovery(
                                            bbo_b if passive_is_a else bbo_a,
                                            t_hedge,
                                            max_age_b_ms if passive_is_a else max_age_a_ms,
                                            cutoff_ms=split_end_cutoff_ms,
                                            trades=trades_b if passive_is_a else trades_a,
                                        )
                                        hedged_notl = 0.0
                                        orphan_reason = "hedge_book_missing"
                                        h_px = 0.0
                                        h_mid = 0.0
                                        h_fill_ms = t_hedge
                                        if hq is not None:
                                            h_bid, h_ask, h_bsz, h_asz, h_obs_ms = hq
                                            h_px = h_ask if passive_is_short else h_bid
                                            h_l1 = (h_ask * h_asz) if passive_is_short else (h_bid * h_bsz)
                                            h_mid = 0.5 * (h_bid + h_ask)
                                            h_fill_ms = max(t_hedge, h_obs_ms)
                                            opp_mid0 = 0.5 * (opp_anchor[0] + opp_anchor[1])
                                            adv_move = ((h_mid / opp_mid0 - 1.0) * 1e4) if passive_is_short else ((1.0 - h_mid / opp_mid0) * 1e4)
                                            if adv_move >= params.hazard_adverse_bps * 5.0:
                                                orphan_reason = "hedge_adverse_abort"
                                            elif h_l1 < MIN_ENTRY_NOTIONAL_USD:
                                                orphan_reason = "hedge_l1_below_min"
                                            else:
                                                # P0 Defect 1 fix: clamp hedged notional to available hedge L1 depth!
                                                hedged_notl = min(filled_notl, h_l1)
                                                if hedged_notl < filled_notl:
                                                    orphan_reason = "hedge_partial_l1_clip"

                                        orphan_notl = max(0.0, filled_notl - hedged_notl)
                                        if orphan_notl > 1e-9:
                                            # P0 Defect 2 fix: explicitly book & unwind orphan passive-leg exposure!
                                            _book_orphan_fill(
                                                is_a=passive_is_a,
                                                is_short=passive_is_short,
                                                orphan_notl=orphan_notl,
                                                fill_ms=fill_ms,
                                                rest_px=rest_px,
                                                p_mid=p_mid,
                                                mk_bps=p_mk_bps,
                                                t_hedge=t_hedge,
                                                reason=orphan_reason,
                                                win_acc=win_acc,
                                                settled_set=settled_in_win,
                                                split_end_ms=split_end_cutoff_ms,
                                            )

                                        if hedged_notl >= MIN_ENTRY_NOTIONAL_USD:
                                            if hedged_notl < target_notional_usd:
                                                win_acc["depth_clipped_entries"] += 1
                                            pos_dir = cand_dir
                                            entry_passive_is_a = passive_is_a
                                            if passive_is_a:
                                                leg_a_fill_ms = fill_ms
                                                leg_b_fill_ms = h_fill_ms
                                                entry_exec_a, entry_exec_b = rest_px, h_px
                                                entry_mid_a, entry_mid_b = p_mid, h_mid
                                            else:
                                                leg_a_fill_ms = h_fill_ms
                                                leg_b_fill_ms = fill_ms
                                                entry_exec_a, entry_exec_b = h_px, rest_px
                                                entry_mid_a, entry_mid_b = h_mid, p_mid
                                            entry_ms = max(leg_a_fill_ms, leg_b_fill_ms)
                                            leg_a_settled_cash = {}
                                            leg_b_settled_cash = {}
                                            cur_leg_a_funding_usd = 0.0
                                            cur_leg_b_funding_usd = 0.0
                                            ref_e = 0.5 * (entry_mid_a + entry_mid_b)
                                            pos_qty_base = hedged_notl / ref_e
                                            pos_notional_usd = hedged_notl
                                            entry_mid_basis_bps = pos_dir * ((entry_mid_a - entry_mid_b) / ref_e) * 1e4
                                            win_acc["trades_entered"] += 1
                            else:
                                # Immediate dual-taker entry at t_cur + 330 ms
                                t_exec = t_cur + TAKER_LATENCY_MS
                                ea = _lookup_bbo_with_recovery(
                                    bbo_a,
                                    t_exec,
                                    max_age_a_ms,
                                    cutoff_ms=split_end_cutoff_ms,
                                    trades=trades_a,
                                )
                                eb = _lookup_bbo_with_recovery(
                                    bbo_b,
                                    t_exec,
                                    max_age_b_ms,
                                    cutoff_ms=split_end_cutoff_ms,
                                    trades=trades_b,
                                )
                                if ea is not None and eb is not None:
                                    eabid, eaask, eabsz, eaasz, ea_obs_ms = ea
                                    ebbid, ebask, ebbsz, ebasz, eb_obs_ms = eb
                                    ex_a = eabid if cand_dir == 1 else eaask
                                    ex_b = ebask if cand_dir == 1 else ebbid
                                    l1_ea = (eabid * eabsz) if cand_dir == 1 else (eaask * eaasz)
                                    l1_eb = (ebask * ebasz) if cand_dir == 1 else (ebbid * ebbsz)
                                    cap_notl = min(target_notional_usd, l1_ea, l1_eb)
                                    if cap_notl >= MIN_ENTRY_NOTIONAL_USD:
                                        if cap_notl < target_notional_usd:
                                            win_acc["depth_clipped_entries"] += 1
                                        pos_dir = cand_dir
                                        leg_a_fill_ms = max(t_exec, ea_obs_ms)
                                        leg_b_fill_ms = max(t_exec, eb_obs_ms)
                                        entry_ms = max(leg_a_fill_ms, leg_b_fill_ms)
                                        leg_a_settled_cash = {}
                                        leg_b_settled_cash = {}
                                        cur_leg_a_funding_usd = 0.0
                                        cur_leg_b_funding_usd = 0.0
                                        entry_passive_is_a = None
                                        entry_exec_a = ex_a
                                        entry_exec_b = ex_b
                                        entry_mid_a = 0.5 * (eabid + eaask)
                                        entry_mid_b = 0.5 * (ebbid + ebask)
                                        ref_e = 0.5 * (entry_mid_a + entry_mid_b)
                                        pos_qty_base = cap_notl / ref_e
                                        pos_notional_usd = cap_notl
                                        entry_mid_basis_bps = pos_dir * ((entry_mid_a - entry_mid_b) / ref_e) * 1e4
                                        win_acc["trades_entered"] += 1

            if pos_dir != 0:
                _settle_open_position_funding(
                    t_end,
                    t_end,
                    win_acc,
                    settled_in_win,
                    avail_cap_ms=split_end_cutoff_ms,
                )
            if open_orphans:
                rem_orphans = []
                for orp in open_orphans:
                    t_try = max(t_end, int(orp["earliest_unwind_ms"]))
                    if t_try > t_end or not _try_unwind_orphan(
                        orp,
                        t_try,
                        win_acc,
                        settled_in_win,
                        split_end_ms=split_end_cutoff_ms,
                    ):
                        rem_orphans.append(orp)
                open_orphans = rem_orphans

            # Force terminal flat at end of split (w20, w25, w26)
            is_last_in_split = (w_idx == len(split_wins) - 1)
            if is_last_in_split:
                if pos_dir != 0:
                    closed = _close_position(
                        t_end,
                        win_acc,
                        settled_in_win,
                        "split_terminal_drain",
                        is_split_end=True,
                        split_end_ms=t_end,
                    )
                    if not closed:
                        terminal_flat_all = False
                        terminal_residual_notional_usd += pos_notional_usd
                        win_acc["position_open_residual_usd"] += pos_notional_usd
                        qa_mtm = _lookup_bbo_at(bbo_a, t_end, MAX_GAP_MS)
                        qb_mtm = _lookup_bbo_at(bbo_b, t_end, MAX_GAP_MS)
                        if qa_mtm is not None and qb_mtm is not None:
                            mid_a_end = 0.5 * (qa_mtm[0] + qa_mtm[1])
                            mid_b_end = 0.5 * (qb_mtm[0] + qb_mtm[1])
                            res_mtm = pos_dir * pos_qty_base * ((entry_mid_a - mid_a_end) + (mid_b_end - entry_mid_b))
                            win_acc["terminal_residual_mtm_usd"] += round(res_mtm, 6)
                        else:
                            win_acc["terminal_residual_mtm_unknown"] = True
                        pos_dir = 0
                        pos_qty_base = 0.0
                        pos_notional_usd = 0.0
                        leg_a_fill_ms = 0
                        leg_b_fill_ms = 0
                        leg_a_settled_cash = {}
                        leg_b_settled_cash = {}
                if open_orphans:
                    rem_orphans = []
                    for orp in open_orphans:
                        t_try_end = max(t_end, int(orp["earliest_unwind_ms"]))
                        if t_try_end > t_end or not _try_unwind_orphan(
                            orp,
                            t_try_end,
                            win_acc,
                            settled_in_win,
                            is_split_end=True,
                            split_end_ms=t_end,
                        ):
                            rem_orphans.append(orp)
                    if rem_orphans:
                        terminal_flat_all = False
                        for orp in rem_orphans:
                            res_notl = float(orp["notional_usd"])
                            terminal_residual_notional_usd += res_notl
                            win_acc["orphan_open_residual_usd"] += res_notl
                            bbo_orp = bbo_a if orp["is_a"] else bbo_b
                            q_orp_mtm = _lookup_bbo_at(bbo_orp, t_end, MAX_GAP_MS)
                            if q_orp_mtm is not None:
                                mid_orp_end = 0.5 * (q_orp_mtm[0] + q_orp_mtm[1])
                                sgn_orp = -1 if orp["is_short"] else 1
                                res_orp_mtm = sgn_orp * float(orp["qty_base"]) * (mid_orp_end - float(orp["entry_mid"])) - float(orp["entry_fee_usd"])
                                win_acc["terminal_residual_mtm_usd"] += round(res_orp_mtm, 6)
                            else:
                                win_acc["terminal_residual_mtm_unknown"] = True
                    open_orphans = []

            win_acc["net_pnl_usd"] = (
                win_acc["funding_cash_usd"]
                + win_acc["basis_pnl_usd"]
                - win_acc["spread_slip_usd"]
                - win_acc["fees_usd"]
                - win_acc["fx_drag_usd"]
            )
            per_window_rows.append(win_acc)
            last_prev_t = t_end

    out = {
        "pair": pair.label,
        "symbol": pair.symbol,
        "domain": pair.domain,
        "cohort": pair.cohort,
        "same_risk_domain": pair.same_risk_domain,
        "bbo_cadence_pair": pair.bbo_cadence_pair,
        "arm": params.arm,
        "param_name": params.name,
        "fx_drag_bps": fx_eff,
        "terminal_flat": terminal_flat_all,
        "terminal_residual_notional_usd": round(terminal_residual_notional_usd, 4),
        "windows": per_window_rows,
    }
    if record_trades:
        out["trade_events"] = trade_events
    return out


def aggregate_split_metrics(
    pair_results: Sequence[dict[str, Any]],
    *,
    split: str,
    domain: str | None = None,
    cohort: str | None = None,
    same_risk_domain: bool | None = None,
    exclude_unaudited: bool = False,
) -> dict[str, Any]:
    """Aggregate cash decomposition, trades, passive share, and terminal flat checks."""
    funding = 0.0
    basis = 0.0
    spread_slip = 0.0
    fees = 0.0
    fx = 0.0
    net = 0.0
    notional = 0.0
    trades_entered = 0
    trades_closed = 0
    funding_payments = 0
    passive_attempts = 0
    passive_fills = 0
    depth_clipped_entries = 0
    depth_clipped_exits = 0
    orphan_entries = 0
    orphan_unwound_count = 0
    orphan_notional_usd = 0.0
    orphan_unwind_pnl_usd = 0.0
    orphan_unwind_cost_usd = 0.0
    orphan_open_residual_usd = 0.0
    terminal_flat_all = True
    terminal_residual = 0.0
    win_pnl_by_w: dict[int, float] = {}
    pair_pnl: dict[str, float] = {}

    for pr in pair_results:
        if domain is not None and pr["domain"] != domain:
            continue
        if cohort is not None and pr["cohort"] != cohort:
            continue
        if same_risk_domain is not None and pr["same_risk_domain"] != same_risk_domain:
            continue
        if exclude_unaudited and "entropy" in pr["pair"]:
            continue
        if not pr["terminal_flat"]:
            terminal_flat_all = False
        terminal_residual += float(pr["terminal_residual_notional_usd"])
        for w in pr["windows"]:
            if w["split"] != split:
                continue
            wnum = int(w["window_num"])
            funding += float(w["funding_cash_usd"])
            basis += float(w["basis_pnl_usd"])
            spread_slip += float(w["spread_slip_usd"])
            fees += float(w["fees_usd"])
            fx += float(w["fx_drag_usd"])
            wnet = float(w["net_pnl_usd"])
            net += wnet
            notional += float(w["notional_traded_usd"])
            trades_entered += int(w["trades_entered"])
            trades_closed += int(w["trades_closed"])
            funding_payments += int(w["funding_payments"])
            passive_attempts += int(w["passive_attempts"])
            passive_fills += int(w["passive_fills"])
            depth_clipped_entries += int(w["depth_clipped_entries"])
            depth_clipped_exits += int(w["depth_clipped_exits"])
            orphan_entries += int(w.get("orphan_entries", 0))
            orphan_unwound_count += int(w.get("orphan_unwound_count", 0))
            orphan_notional_usd += float(w.get("orphan_notional_usd", 0.0))
            orphan_unwind_pnl_usd += float(w.get("orphan_unwind_pnl_usd", 0.0))
            orphan_unwind_cost_usd += float(w.get("orphan_unwind_cost_usd", 0.0))
            orphan_open_residual_usd += float(w.get("orphan_open_residual_usd", 0.0))
            win_pnl_by_w[wnum] = win_pnl_by_w.get(wnum, 0.0) + wnet
            pair_pnl[pr["pair"]] = pair_pnl.get(pr["pair"], 0.0) + wnet

    max_pair_abs = max((abs(v) for v in pair_pnl.values()), default=0.0)
    sum_pair_abs = sum(abs(v) for v in pair_pnl.values())
    return {
        "split": split,
        "funding_cash_usd": round(funding, 4),
        "basis_pnl_usd": round(basis, 4),
        "spread_slip_usd": round(spread_slip, 4),
        "fees_usd": round(fees, 4),
        "fx_drag_usd": round(fx, 4),
        "net_pnl_usd": round(net, 4),
        "notional_traded_usd": round(notional, 2),
        "trades_entered": trades_entered,
        "trades_closed": trades_closed,
        "funding_payments": funding_payments,
        "passive_attempts": passive_attempts,
        "passive_fills": passive_fills,
        "passive_fill_pct": round(100.0 * passive_fills / passive_attempts, 2) if passive_attempts else 0.0,
        "depth_clipped_entries": depth_clipped_entries,
        "depth_clipped_exits": depth_clipped_exits,
        "orphan_entries": orphan_entries,
        "orphan_unwound_count": orphan_unwound_count,
        "orphan_notional_usd": round(orphan_notional_usd, 4),
        "orphan_unwind_pnl_usd": round(orphan_unwind_pnl_usd, 4),
        "orphan_unwind_cost_usd": round(orphan_unwind_cost_usd, 4),
        "orphan_open_residual_usd": round(orphan_open_residual_usd, 4),
        "terminal_flat_all": terminal_flat_all,
        "terminal_residual_notional_usd": round(terminal_residual, 4),
        "window_pnl_series": {str(k): round(win_pnl_by_w[k], 4) for k in sorted(win_pnl_by_w)},
        "top_pair_share_abs_pct": round(100.0 * max_pair_abs / sum_pair_abs, 2) if sum_pair_abs > 0.0 else 0.0,
        "pair_net_pnl": {k: round(v, 4) for k, v in sorted(pair_pnl.items(), key=lambda kv: -kv[1])},
    }


def _waterfill_capped_weights(raw_weights: dict[str, float], symbol_map: dict[str, str], cap: float) -> dict[str, float]:
    """Normalize inverse-vol weights so sum <= 1.0 and per-symbol total weight <= cap."""
    if not raw_weights:
        return {}
    tot = sum(max(0.0, v) for v in raw_weights.values())
    if tot <= 0.0:
        return {}
    w = {k: max(0.0, v) / tot for k, v in raw_weights.items()}
    # Iteratively enforce per-symbol cap and redistribute headroom up to 1.0
    for _ in range(16):
        by_sym: dict[str, float] = {}
        for k, wk in w.items():
            s = symbol_map[k]
            by_sym[s] = by_sym.get(s, 0.0) + wk
        over = {s for s, tot_s in by_sym.items() if tot_s > cap + 1e-12}
        if not over:
            break
        for s in over:
            scale = cap / by_sym[s]
            for k in w:
                if symbol_map[k] == s:
                    w[k] *= scale
        used = sum(w.values())
        rem = 1.0 - used
        if rem <= 1e-12:
            break
        under_keys = [k for k in w if by_sym[symbol_map[k]] < cap - 1e-12]
        under_sum = sum(w[k] for k in under_keys)
        if under_sum <= 1e-12:
            break
        for k in under_keys:
            w[k] += rem * (w[k] / under_sum)
    # Final hard clamp check
    by_sym_final: dict[str, float] = {}
    for k, wk in w.items():
        s = symbol_map[k]
        by_sym_final[s] = by_sym_final.get(s, 0.0) + wk
    for s, tot_s in by_sym_final.items():
        if tot_s > cap:
            sc = cap / tot_s
            for k in w:
                if symbol_map[k] == s:
                    w[k] *= sc
    return w


def build_portfolio_inv_vol_topk(
    pair_results: Sequence[dict[str, Any]],
    *,
    top_k: int,
    capital_usd: float = PORTFOLIO_CAPITAL_USD,
    pair_ref_notional_usd: float = TARGET_PAIR_NOTIONAL_USD,
    symbol_cap: float = SINGLE_SYMBOL_WEIGHT_CAP,
) -> dict[str, Any]:
    """Causal inverse-vol Top-K portfolio allocator (`<= 1.0x` gross, per-symbol cap, zero cross-subsidy)."""
    if not pair_results:
        return {"top_k": top_k, "splits": {}, "window_pnl_series": {}}

    # Index windows across all pairs in this strictly segregated cohort
    win_nums = sorted({int(w["window_num"]) for pr in pair_results for w in pr["windows"]})
    pair_map = {pr["pair"]: pr for pr in pair_results}
    sym_map = {pr["pair"]: pr["symbol"] for pr in pair_results}
    by_pw: dict[tuple[str, int], dict[str, Any]] = {}
    for pr in pair_results:
        for w in pr["windows"]:
            by_pw[(pr["pair"], int(w["window_num"]))] = w

    split_acc: dict[str, dict[str, Any]] = {
        sp: {
            "funding_cash_usd": 0.0,
            "basis_pnl_usd": 0.0,
            "spread_slip_usd": 0.0,
            "fees_usd": 0.0,
            "fx_drag_usd": 0.0,
            "net_pnl_usd": 0.0,
            "notional_traded_usd": 0.0,
            "max_gross_leverage": 0.0,
            "max_symbol_weight": 0.0,
            "terminal_flat_all": all(pr["terminal_flat"] for pr in pair_results),
            "terminal_residual_notional_usd": round(sum(float(pr["terminal_residual_notional_usd"]) for pr in pair_results), 4),
        }
        for sp in ("selection_dev", "holdout", "w26_oos")
    }
    window_pnl_series: dict[str, float] = {}
    history: dict[str, list[float]] = {p: [] for p in pair_map}

    for wn in win_nums:
        sp = split_for_window(wn)
        # Causal trailing stats strictly prior to window wn
        scores: list[tuple[float, str, float]] = []
        for p in sorted(pair_map):
            hist = history[p]
            if len(hist) >= 2:
                mean_h = sum(hist) / len(hist)
                var_h = sum((x - mean_h) ** 2 for x in hist) / (len(hist) - 1)
                vol_h = max(math.sqrt(var_h), 0.50)
                score = mean_h / vol_h
            elif len(hist) == 1:
                vol_h = max(abs(hist[0]), 1.0)
                score = hist[0] / vol_h
            else:
                vol_h = 1.0
                score = 0.0
            scores.append((score, p, vol_h))

        # Rank strictly by causal trailing Sharpe-like score (break ties deterministically by pair name)
        scores.sort(key=lambda x: (-x[0], x[1]))
        chosen = [x for x in scores if x[0] > 0.0][:top_k]
        if not chosen:
            chosen = scores[: min(top_k, len(scores))]

        raw_w = {p: 1.0 / vol_h for _, p, vol_h in chosen}
        weights = _waterfill_capped_weights(raw_w, sym_map, symbol_cap)
        gross_w = sum(weights.values())
        sym_totals: dict[str, float] = {}
        for p, wk in weights.items():
            sym_totals[sym_map[p]] = sym_totals.get(sym_map[p], 0.0) + wk
        max_sym_w = max(sym_totals.values(), default=0.0)
        split_acc[sp]["max_gross_leverage"] = max(split_acc[sp]["max_gross_leverage"], round(gross_w, 6))
        split_acc[sp]["max_symbol_weight"] = max(split_acc[sp]["max_symbol_weight"], round(max_sym_w, 6))

        w_net = 0.0
        for p, wk in weights.items():
            row = by_pw.get((p, wn))
            if row is None:
                continue
            alloc_notl = wk * capital_usd
            scale = alloc_notl / pair_ref_notional_usd
            split_acc[sp]["funding_cash_usd"] += float(row["funding_cash_usd"]) * scale
            split_acc[sp]["basis_pnl_usd"] += float(row["basis_pnl_usd"]) * scale
            split_acc[sp]["spread_slip_usd"] += float(row["spread_slip_usd"]) * scale
            split_acc[sp]["fees_usd"] += float(row["fees_usd"]) * scale
            split_acc[sp]["fx_drag_usd"] += float(row["fx_drag_usd"]) * scale
            pn = float(row["net_pnl_usd"]) * scale
            split_acc[sp]["net_pnl_usd"] += pn
            split_acc[sp]["notional_traded_usd"] += float(row["notional_traded_usd"]) * scale
            w_net += pn

        window_pnl_series[str(wn)] = round(w_net, 4)
        for p in pair_map:
            row = by_pw.get((p, wn))
            history[p].append(float(row["net_pnl_usd"]) if row is not None else 0.0)

    for sp in split_acc:
        for k in ("funding_cash_usd", "basis_pnl_usd", "spread_slip_usd", "fees_usd", "fx_drag_usd", "net_pnl_usd"):
            split_acc[sp][k] = round(split_acc[sp][k], 4)
        split_acc[sp]["notional_traded_usd"] = round(split_acc[sp]["notional_traded_usd"], 2)

    return {
        "top_k": top_k,
        "capital_usd": capital_usd,
        "symbol_cap": symbol_cap,
        "splits": split_acc,
        "window_pnl_series": window_pnl_series,
    }


def compute_multiple_testing_stats(trials_by_name: dict[str, list[float]]) -> dict[str, Any]:
    """Compute PSR / N_eff / DSR / MinTRL / CSCV PBO over aligned window PnL series."""
    if not trials_by_name:
        return {"status": "empty_trials"}
    # Filter out constant zero series before DSR/CSCV where needed, while tracking total raw trials
    raw_count = len(trials_by_name)
    varying = {k: v for k, v in trials_by_name.items() if len(set(v)) > 1 and len(v) >= 4}
    out: dict[str, Any] = {"raw_trial_count": raw_count, "varying_trial_count": len(varying)}
    if not varying:
        out["status"] = "all_trials_constant"
        return out
    t_len = len(next(iter(varying.values())))
    if t_len >= 3:
        dsr = deflated_sharpe_report(varying)
        out["dsr"] = dsr
    if len(varying) >= 2 and t_len >= 16:
        out["cscv_pbo"] = cscv_pbo(varying, num_blocks=8)
    elif len(varying) >= 2 and t_len >= 8:
        out["cscv_pbo"] = cscv_pbo(varying, num_blocks=4)
    return out


def discover_all_campaigns(lakehouse_dir: Path, *, min_w: int = 1, max_w: int = 26) -> dict[str, list[dict[str, Any]]]:
    """Discover all Tokyo W01..W26 campaigns per domain (`crypto`, `rwa`), excluding w27/w28."""
    out: dict[str, dict[int, dict[str, Any]]] = {"crypto": {}, "rwa": {}}
    pat = str(
        lakehouse_dir
        / "parquet"
        / "bbo_quotes"
        / "collector_region=tokyo"
        / "venue=*"
        / "utc_date=*"
        / "campaign_id=tokyo-*"
    )
    for d in glob.glob(pat):
        cid = d.rsplit("campaign_id=", 1)[1]
        if "-w27" in cid or "-w28" in cid:
            continue
        m = re.search(r"^tokyo-(crypto|rwa)-(\d{8})T\d+Z-w(\d+)$", cid)
        if not m:
            continue
        dom, ymd, w_num = m.group(1), m.group(2), int(m.group(3))
        if w_num < min_w or w_num > max_w or (dom == "crypto" and w_num == 8):
            continue
        utc_date = f"{ymd[:4]}-{ymd[4:6]}-{ymd[6:8]}"
        if w_num not in out[dom]:
            out[dom][w_num] = {
                "window_num": w_num,
                "window": f"w{w_num:02d}",
                "campaign_id": cid,
                "utc_date": utc_date,
                "domain": dom,
                "split": split_for_window(w_num),
            }
    return {dom: [mp[k] for k in sorted(mp)] for dom, mp in out.items()}


def _sql_list(files: Sequence[str]) -> str:
    return "[" + ", ".join("'" + f.replace("'", "''") + "'" for f in files) + "]"


def _venue_w01_w26_files(lakehouse_dir: Path, dataset: str, venue: str, domain: str) -> list[str]:
    pat = str(
        lakehouse_dir
        / "parquet"
        / dataset
        / "collector_region=tokyo"
        / f"venue={venue}"
        / "utc_date=*"
        / f"campaign_id=tokyo-{domain}-*-w*"
        / "*.parquet"
    )
    files = sorted(glob.glob(pat))
    return [f for f in files if "-w27" not in f and "-w28" not in f]


def _load_symbol_bbo(con: Any, files: Sequence[str], raw_market: str) -> dict[str, Any] | None:
    import numpy as np

    if not files:
        return None
    src = _sql_list(files)
    cols = con.execute(
        f"""
        SELECT available_at_ms AS ts,
               bid_px_f64 AS bid,
               ask_px_f64 AS ask,
               CAST(bid_sz AS DOUBLE) AS bsz,
               CAST(ask_sz AS DOUBLE) AS asz
        FROM read_parquet({src})
        WHERE market = ? AND tradable AND bid_px_f64 > 0 AND ask_px_f64 > bid_px_f64
        ORDER BY available_at_ms ASC
        """,
        [raw_market],
    ).fetchnumpy()
    if len(cols["ts"]) < 2:
        return None
    return {
        "ts": np.asarray(cols["ts"], dtype=np.int64),
        "bid": np.asarray(cols["bid"], dtype=np.float64),
        "ask": np.asarray(cols["ask"], dtype=np.float64),
        "bsz": np.asarray(cols["bsz"], dtype=np.float64),
        "asz": np.asarray(cols["asz"], dtype=np.float64),
    }


def _load_symbol_trades(con: Any, files: Sequence[str], raw_market: str) -> dict[str, Any] | None:
    import numpy as np

    if not files:
        return None
    src = _sql_list(files)
    cols = con.execute(
        f"""
        SELECT available_at_ms AS ts,
               CASE WHEN lower(aggressor_side) IN ('buy','b','bid') THEN 1
                    WHEN lower(aggressor_side) IN ('sell','s','ask') THEN -1 ELSE 0 END AS sgn,
               price_f64 AS px,
               size_f64 AS sz
        FROM read_parquet({src})
        WHERE market = ? AND NOT is_zero_price_transfer AND price_f64 > 0 AND size_f64 > 0
        ORDER BY available_at_ms ASC
        """,
        [raw_market],
    ).fetchnumpy()
    if len(cols["ts"]) == 0:
        return None
    return {
        "ts": np.asarray(cols["ts"], dtype=np.int64),
        "sgn": np.asarray(cols["sgn"], dtype=np.int64),
        "px": np.asarray(cols["px"], dtype=np.float64),
        "sz": np.asarray(cols["sz"], dtype=np.float64),
    }


def _load_symbol_funding(
    con: Any,
    files: Sequence[str],
    venue: str,
    symbol: str,
    raw_market: str,
) -> tuple[list[dict[str, Any]], dict[str, Any] | None]:
    """Return `(settled_events_with_avail, live_ctx_series_by_avail)` without global future leakage."""
    import numpy as np

    if not files:
        return [], None
    src = _sql_list(files)
    candidates = list(dict.fromkeys([symbol, raw_market]))
    ph = ", ".join("?" for _ in candidates)

    settled: list[dict[str, Any]] = []
    live_ctx: dict[str, Any] | None = None

    if venue.startswith("lighter"):
        rows = con.execute(
            f"""
            SELECT ((funding_slot_ms // {HOUR_MS}) * {HOUR_MS}) + {HOUR_MS} AS pay_ms,
                   GREATEST(
                       MAX(available_at_ms),
                       ((funding_slot_ms // {HOUR_MS}) * {HOUR_MS}) + {HOUR_MS}
                   ) AS avail_ms,
                   avg(funding_rate_bps_f64) AS rate_bps
            FROM read_parquet({src})
            WHERE market IN ({ph}) AND ctx_kind = 'market_stats'
              AND funding_slot_ms IS NOT NULL
              AND available_at_ms IS NOT NULL
              AND funding_rate_bps_f64 IS NOT NULL
              AND available_at_ms >= funding_slot_ms AND available_at_ms < funding_slot_ms + {HOUR_MS}
            GROUP BY 1
            ORDER BY 2 ASC, 1 ASC
            """,
            candidates,
        ).fetchall()
        for pay_ms, avail_ms, rate_bps in rows:
            if (
                pay_ms is not None
                and avail_ms is not None
                and rate_bps is not None
                and math.isfinite(float(rate_bps))
            ):
                settled.append(
                    {
                        "pay_ms": int(pay_ms),
                        "available_at_ms": int(avail_ms),
                        "rate_bps": float(rate_bps),
                    }
                )
        lcols = con.execute(
            f"""
            SELECT available_at_ms AS ts,
                   funding_rate_bps_f64 AS rate_bps
            FROM read_parquet({src})
            WHERE market IN ({ph}) AND ctx_kind = 'market_stats'
              AND available_at_ms IS NOT NULL
              AND funding_rate_bps_f64 IS NOT NULL
            ORDER BY available_at_ms ASC
            """,
            candidates,
        ).fetchnumpy()
        if len(lcols["ts"]) > 0:
            live_ctx = {
                "ts": np.asarray(lcols["ts"], dtype=np.int64),
                "rate": np.asarray(lcols["rate_bps"], dtype=np.float64),
            }
    else:
        rows = con.execute(
            f"""
            SELECT (funding_slot_ms // {HOUR_MS}) * {HOUR_MS} AS pay_ms,
                   available_at_ms AS avail_ms,
                   funding_rate_bps_f64 AS rate_bps
            FROM read_parquet({src})
            WHERE market IN ({ph}) AND ctx_kind = 'hourly_funding_history'
              AND funding_slot_ms IS NOT NULL
              AND available_at_ms IS NOT NULL
              AND funding_rate_bps_f64 IS NOT NULL
            ORDER BY available_at_ms ASC, 1 ASC
            """,
            candidates,
        ).fetchall()
        for pay_ms, avail_ms, rate_bps in rows:
            if (
                pay_ms is not None
                and avail_ms is not None
                and rate_bps is not None
                and math.isfinite(float(rate_bps))
            ):
                settled.append(
                    {
                        "pay_ms": int(pay_ms),
                        "available_at_ms": int(avail_ms),
                        "rate_bps": float(rate_bps),
                    }
                )
        lcols = con.execute(
            f"""
            SELECT available_at_ms AS ts,
                   funding_rate_bps_f64 AS rate_bps
            FROM read_parquet({src})
            WHERE market IN ({ph}) AND ctx_kind = 'active_asset_ctx'
              AND available_at_ms IS NOT NULL
              AND funding_rate_bps_f64 IS NOT NULL
            ORDER BY available_at_ms ASC
            """,
            candidates,
        ).fetchnumpy()
        if len(lcols["ts"]) > 0:
            live_ctx = {
                "ts": np.asarray(lcols["ts"], dtype=np.int64),
                "rate": np.asarray(lcols["rate_bps"], dtype=np.float64),
            }
    return settled, live_ctx


def _campaign_time_bounds(con: Any, lakehouse_dir: Path, campaigns: dict[str, list[dict[str, Any]]]) -> None:
    """Populate exact `[t0_ms, t1_ms]` bounds for every discovered campaign from `lighter` BBOs."""
    for dom, clist in campaigns.items():
        files = _venue_w01_w26_files(lakehouse_dir, "bbo_quotes", "lighter", dom)
        if not files:
            continue
        src = _sql_list(files)
        rows = con.execute(
            f"""
            SELECT campaign_id, MIN(available_at_ms), MAX(available_at_ms)
            FROM read_parquet({src})
            GROUP BY 1
            """
        ).fetchall()
        bounds = {str(cid): (int(t0), int(t1)) for cid, t0, t1 in rows if t0 is not None and t1 is not None}
        for c in clist:
            t0, t1 = bounds.get(c["campaign_id"], (0, 0))
            c["t0_ms"] = t0
            c["t1_ms"] = t1


def run_lakehouse_evaluation(
    lakehouse_dir: Path,
    *,
    fee_evidence_path: Path | None,
    output_path: Path,
    symbols_filter: set[str] | None = None,
) -> dict[str, Any]:
    """Run full W01..W26 evaluation on Aleph lakehouse."""
    import duckdb

    con = duckdb.connect(":memory:")
    con.execute("SET preserve_insertion_order=false; SET threads=2; SET memory_limit='2GB';")

    campaigns = discover_all_campaigns(lakehouse_dir, min_w=1, max_w=26)
    _campaign_time_bounds(con, lakehouse_dir, campaigns)

    all_pairs = build_pair_catalog()
    if symbols_filter:
        all_pairs = [p for p in all_pairs if p.symbol in symbols_filter]

    pairs_by_sym: dict[str, list[CarryPairSpec]] = {}
    for p in all_pairs:
        pairs_by_sym.setdefault(p.symbol, []).append(p)

    fee_regimes = {}
    for mode_key in ("verified_prem", "standard_zero"):
        fees_meta, prov = resolve_venue_fees(fee_evidence_path, lighter_fee_mode=mode_key)
        fee_regimes[mode_key] = {"venues": fees_meta, "provenance": prov}

    grid = build_arm_candidate_grid()

    # Store raw per-pair-per-candidate results keyed by (mode_key, fx_bps, arm, cand_name)
    results_store: dict[tuple[str, float, str, str], list[dict[str, Any]]] = {}
    coverage_summary: list[dict[str, Any]] = []

    for sym in sorted(pairs_by_sym):
        dom = "crypto" if sym in CRYPTO_SYMBOLS else "rwa"
        sym_wins = [w for w in campaigns.get(dom, []) if w.get("t1_ms", 0) > w.get("t0_ms", 0)]
        if not sym_wins:
            continue
        sym_pairs = pairs_by_sym[sym]
        venues_needed = sorted({p.venue_a for p in sym_pairs} | {p.venue_b for p in sym_pairs})

        bbo_cache: dict[str, dict[str, Any] | None] = {}
        tr_cache: dict[str, dict[str, Any] | None] = {}
        fund_cache: dict[str, tuple[list[dict[str, Any]], dict[str, Any] | None]] = {}

        for v in venues_needed:
            mkt = VENUE_SYMBOL_MAP[v][sym]
            bbo_files = _venue_w01_w26_files(lakehouse_dir, "bbo_quotes", v, dom)
            tr_files = _venue_w01_w26_files(lakehouse_dir, "public_trades", v, dom)
            fn_files = _venue_w01_w26_files(lakehouse_dir, "funding_and_ctx", v, dom)
            bbo_cache[v] = _load_symbol_bbo(con, bbo_files, mkt)
            tr_cache[v] = _load_symbol_trades(con, tr_files, mkt)
            fund_cache[v] = _load_symbol_funding(con, fn_files, v, sym, mkt)

        for p in sym_pairs:
            ba = bbo_cache.get(p.venue_a)
            bb = bbo_cache.get(p.venue_b)
            fa_s, fa_l = fund_cache.get(p.venue_a, ([], None))
            fb_s, fb_l = fund_cache.get(p.venue_b, ([], None))
            overlap_hours = len({int(r["pay_ms"]) for r in fa_s} & {int(r["pay_ms"]) for r in fb_s})
            coverage_summary.append(
                {
                    "pair": p.label,
                    "symbol": p.symbol,
                    "domain": p.domain,
                    "cohort": p.cohort,
                    "same_risk_domain": p.same_risk_domain,
                    "bbo_rows_a": len(ba["ts"]) if ba else 0,
                    "bbo_rows_b": len(bb["ts"]) if bb else 0,
                    "settled_overlap_hours": overlap_hours,
                }
            )
            if ba is None or bb is None or overlap_hours < 5:
                continue

            ta = tr_cache.get(p.venue_a)
            tb = tr_cache.get(p.venue_b)
            max_age_a = int(VENUE_META[p.venue_a]["max_book_age_ms"])
            max_age_b = int(VENUE_META[p.venue_b]["max_book_age_ms"])
            pre_win = precompute_pair_timeline(
                bbo_a=ba,
                bbo_b=bb,
                trades_a=ta,
                trades_b=tb,
                windows=sym_wins,
                max_age_a_ms=max_age_a,
                max_age_b_ms=max_age_b,
            )

            fx_list = FX_STRESS_BPS if p.cross_stablecoin else (0.0,)
            for mode_key, reg in fee_regimes.items():
                vmeta = reg["venues"]
                famk = float(vmeta[p.venue_a]["maker_fee_bps"])
                fatk = float(vmeta[p.venue_a]["taker_fee_bps"])
                fbmk = float(vmeta[p.venue_b]["maker_fee_bps"])
                fbtk = float(vmeta[p.venue_b]["taker_fee_bps"])
                for fx_bps in fx_list:
                    for arm, cand_list in grid.items():
                        for cand in cand_list:
                            res = simulate_carry_pair_timeline(
                                pair=p,
                                bbo_a=ba,
                                bbo_b=bb,
                                trades_a=ta,
                                trades_b=tb,
                                funding_a=fa_s,
                                funding_b=fb_s,
                                live_ctx_a=fa_l,
                                live_ctx_b=fb_l,
                                windows=sym_wins,
                                fee_a_mk_bps=famk,
                                fee_a_tk_bps=fatk,
                                fee_b_mk_bps=fbmk,
                                fee_b_tk_bps=fbtk,
                                max_age_a_ms=max_age_a,
                                max_age_b_ms=max_age_b,
                                params=cand,
                                fx_drag_bps=fx_bps,
                                precomputed_by_win=pre_win,
                            )
                            results_store.setdefault((mode_key, fx_bps, arm, cand.name), []).append(res)

        del bbo_cache, tr_cache, fund_cache
        gc.collect()
        print(f"[processed] {sym} ({dom}): {len(sym_pairs)} pairs", flush=True)

    # Select frozen winner per (mode_key, cohort_key, domain, arm) strictly on selection_dev (w01..w20)
    cohort_defs = [
        ("USDC_USDC_distinct", "USDC/USDC", False, True),
        ("USDG_USDC_distinct", "USDG/USDC", False, True),
        ("same_risk_domain", None, True, False),
    ]
    report: dict[str, Any] = {
        "schema_version": "track02_funding_basis_carry_portfolio_v1",
        "campaigns": {
            dom: [{"window": c["window"], "split": c["split"], "t0_ms": c.get("t0_ms", 0), "t1_ms": c.get("t1_ms", 0)} for c in cl]
            for dom, cl in campaigns.items()
        },
        "fee_regimes": fee_regimes,
        "coverage_summary": coverage_summary,
        "cohorts": {},
    }

    for mode_key in ("verified_prem", "standard_zero"):
        mode_out: dict[str, Any] = {}
        for cname, coh_filter, same_dom, excl_unaud in cohort_defs:
            fx_values = FX_STRESS_BPS if cname == "USDG_USDC_distinct" else (0.0,)
            for fx_bps in fx_values:
                ckey = f"{cname}_fx{int(fx_bps)}bp" if cname == "USDG_USDC_distinct" else cname
                dom_out: dict[str, Any] = {}
                for dom in ("crypto", "rwa"):
                    arm_out: dict[str, Any] = {}
                    aligned_trials: dict[str, list[float]] = {}
                    frozen_winners: dict[str, str] = {}
                    for arm, cand_list in grid.items():
                        best_cand_name: str | None = None
                        best_sel_net = -1e18
                        sel_grid_rows: list[dict[str, Any]] = []
                        for cand in cand_list:
                            prs = results_store.get((mode_key, fx_bps, arm, cand.name), [])
                            sel_agg = aggregate_split_metrics(
                                prs,
                                split="selection_dev",
                                domain=dom,
                                cohort=coh_filter,
                                same_risk_domain=same_dom,
                                exclude_unaudited=excl_unaud,
                            )
                            sel_grid_rows.append({"param_name": cand.name, "net_pnl_usd": sel_agg["net_pnl_usd"], "trades_closed": sel_agg["trades_closed"]})
                            # Aligned full w01..w26 trial vector for multiple testing audit
                            all_w_series = {}
                            for sp in ("selection_dev", "holdout", "w26_oos"):
                                sp_agg = aggregate_split_metrics(
                                    prs,
                                    split=sp,
                                    domain=dom,
                                    cohort=coh_filter,
                                    same_risk_domain=same_dom,
                                    exclude_unaudited=excl_unaud,
                                )
                                all_w_series.update(sp_agg["window_pnl_series"])
                            if all_w_series:
                                aligned_trials[f"{arm}:{cand.name}"] = [all_w_series[k] for k in sorted(all_w_series, key=int)]
                            if sel_agg["net_pnl_usd"] > best_sel_net:
                                best_sel_net = sel_agg["net_pnl_usd"]
                                best_cand_name = cand.name

                        if best_cand_name is None:
                            continue
                        frozen_winners[arm] = best_cand_name
                        win_prs = results_store.get((mode_key, fx_bps, arm, best_cand_name), [])
                        splits_out = {
                            sp: aggregate_split_metrics(
                                win_prs,
                                split=sp,
                                domain=dom,
                                cohort=coh_filter,
                                same_risk_domain=same_dom,
                                exclude_unaudited=excl_unaud,
                            )
                            for sp in ("selection_dev", "holdout", "w26_oos")
                        }
                        arm_out[arm] = {
                            "frozen_param": best_cand_name,
                            "selection_grid_count": len(sel_grid_rows),
                            "splits": splits_out,
                        }

                    # C3_portfolio_inv_vol_topK built on the frozen C1/C2 winner (best selection_dev among C1/C2)
                    c12_cands = [a for a in ("C1_basis_timed_taker", "C2_basis_timed_passive_first") if a in arm_out]
                    port_out: dict[str, Any] = {}
                    if c12_cands:
                        best_arm_c12 = max(c12_cands, key=lambda a: arm_out[a]["splits"]["selection_dev"]["net_pnl_usd"])
                        best_param_c12 = arm_out[best_arm_c12]["frozen_param"]
                        base_prs = [
                            pr
                            for pr in results_store.get((mode_key, fx_bps, best_arm_c12, best_param_c12), [])
                            if pr["domain"] == dom
                            and (coh_filter is None or pr["cohort"] == coh_filter)
                            and (same_dom is None or pr["same_risk_domain"] == same_dom)
                            and (not excl_unaud or "entropy" not in pr["pair"])
                        ]
                        for k_top in PORTFOLIO_KS:
                            pk = build_portfolio_inv_vol_topk(base_prs, top_k=k_top)
                            aligned_trials[f"C3_portfolio_inv_vol_top{k_top}"] = [
                                pk["window_pnl_series"][wk] for wk in sorted(pk["window_pnl_series"], key=int)
                            ]
                            port_out[f"top_{k_top}"] = pk

                    # Multiple testing DSR / PBO audit
                    mt_audit = compute_multiple_testing_stats(aligned_trials)
                    dom_out[dom] = {
                        "arms": arm_out,
                        "C3_portfolio_inv_vol": port_out,
                        "multiple_testing": mt_audit,
                    }
                mode_out[ckey] = dom_out
        report["cohorts"][mode_key] = mode_out

    output_path.parent.mkdir(parents=True, exist_ok=True)
    output_path.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return report


def run_self_test() -> dict[str, Any]:
    """Offline synthetic self-test verifying carry + basis Z-score, passive fill, FX drag, and DSR/PBO."""
    pair_usdc = CarryPairSpec("NVDA", "rwa", "lighter", "trade_xyz", "USDC/USDC", False, "lighter<->trade_xyz NVDA", "mixed_ws_snapshot12s")
    pair_usdg = CarryPairSpec("NVDA", "rwa", "lighter_robinhood", "trade_xyz", "USDG/USDC", False, "rh<->trade_xyz NVDA", "mixed_ws_snapshot12s")

    # 30 windows (~10h each): 20 selection_dev, 5 holdout, 1 w26_oos (26 windows)
    t0_base = 1_700_000_000_000
    win_ms = 6 * HOUR_MS
    windows = [
        {
            "window_num": w,
            "window": f"w{w:02d}",
            "split": split_for_window(w),
            "t0_ms": t0_base + (w - 1) * win_ms,
            "t1_ms": t0_base + w * win_ms - 1_000,
        }
        for w in range(1, 27)
    ]

    ts_bbo: list[int] = []
    a_bid: list[float] = []
    a_ask: list[float] = []
    a_bsz: list[float] = []
    a_asz: list[float] = []
    b_bid: list[float] = []
    b_ask: list[float] = []
    b_bsz: list[float] = []
    b_asz: list[float] = []
    tr_ts: list[int] = []
    tr_sgn: list[int] = []
    tr_px: list[float] = []
    tr_sz: list[float] = []
    fa: dict[int, float] = {}
    fb: dict[int, float] = {}

    for w_i, w in enumerate(windows):
        t_cur = w["t0_ms"]
        while t_cur <= w["t1_ms"]:
            # Hour index within window
            hr = (t_cur - w["t0_ms"]) // HOUR_MS
            # Make Leg A intermittently rich (+12 bps) on even hours, mean-reverting on odd hours
            rich = 0.12 if (hr % 2 == 1) else -0.02
            ts_bbo.append(t_cur)
            a_bid.append(100.0 + rich - 0.02)
            a_ask.append(100.0 + rich + 0.02)
            a_bsz.append(50.0)
            a_asz.append(50.0)
            b_bid.append(99.99)
            b_ask.append(100.01)
            b_bsz.append(50.0)
            b_asz.append(50.0)
            if rich > 0.0:
                tr_ts.append(t_cur + 500)
                tr_sgn.append(1)
                tr_px.append(100.16)
                tr_sz.append(25.0)
            t_cur += 10_000
        # Hourly settled funding rates with deterministic per-window variation
        h0 = (w["t0_ms"] // HOUR_MS) * HOUR_MS
        for k in range(1, 7):
            ht = h0 + k * HOUR_MS
            fa[ht] = 2.0 + 0.3 * (w_i % 5)
            fb[ht] = 0.2

    bbo_a = {"ts": ts_bbo, "bid": a_bid, "ask": a_ask, "bsz": a_bsz, "asz": a_asz}
    bbo_b = {"ts": ts_bbo, "bid": b_bid, "ask": b_ask, "bsz": b_bsz, "asz": b_asz}
    trades_a = {"ts": tr_ts, "sgn": tr_sgn, "px": tr_px, "sz": tr_sz}

    p_c0 = CarryArmParams("c0", "C0_naive_carry", min_basis_zscore=-999.0, basis_timing_weight=0.0, take_profit_basis_bps=0.0, max_basis_stop_bps=999.0)
    p_c1 = CarryArmParams("c1", "C1_basis_timed_taker", min_basis_zscore=1.0, take_profit_basis_bps=5.0)
    p_c2 = CarryArmParams("c2", "C2_basis_timed_passive_first", min_basis_zscore=1.0, take_profit_basis_bps=5.0, passive_first=True)

    r0 = simulate_carry_pair_timeline(
        pair=pair_usdc, bbo_a=bbo_a, bbo_b=bbo_b, trades_a=trades_a, trades_b=None,
        funding_a=fa, funding_b=fb, live_ctx_a=None, live_ctx_b=None, windows=windows,
        fee_a_mk_bps=0.4, fee_a_tk_bps=2.8, fee_b_mk_bps=0.3, fee_b_tk_bps=0.9,
        max_age_a_ms=30_000, max_age_b_ms=30_000, params=p_c0,
    )
    r1 = simulate_carry_pair_timeline(
        pair=pair_usdc, bbo_a=bbo_a, bbo_b=bbo_b, trades_a=trades_a, trades_b=None,
        funding_a=fa, funding_b=fb, live_ctx_a=None, live_ctx_b=None, windows=windows,
        fee_a_mk_bps=0.4, fee_a_tk_bps=2.8, fee_b_mk_bps=0.3, fee_b_tk_bps=0.9,
        max_age_a_ms=30_000, max_age_b_ms=30_000, params=p_c1,
    )
    r2 = simulate_carry_pair_timeline(
        pair=pair_usdg, bbo_a=bbo_a, bbo_b=bbo_b, trades_a=trades_a, trades_b=None,
        funding_a=fa, funding_b=fb, live_ctx_a=None, live_ctx_b=None, windows=windows,
        fee_a_mk_bps=0.0, fee_a_tk_bps=0.0, fee_b_mk_bps=0.3, fee_b_tk_bps=0.9,
        max_age_a_ms=30_000, max_age_b_ms=30_000, params=p_c2, fx_drag_bps=1.0,
    )
    assert r0["terminal_flat"] and r1["terminal_flat"] and r2["terminal_flat"]
    assert r2["terminal_residual_notional_usd"] == 0.0

    trials = {
        "c0": [float(w["net_pnl_usd"]) for w in r0["windows"]],
        "c1": [float(w["net_pnl_usd"]) for w in r1["windows"]],
        "c2": [float(w["net_pnl_usd"]) for w in r2["windows"]],
    }
    mt = compute_multiple_testing_stats(trials)
    port = build_portfolio_inv_vol_topk([r1, r2], top_k=3)
    res = {
        "self_test": "ok",
        "c0_sel": aggregate_split_metrics([r0], split="selection_dev")["net_pnl_usd"],
        "c1_sel": aggregate_split_metrics([r1], split="selection_dev")["net_pnl_usd"],
        "c2_sel": aggregate_split_metrics([r2], split="selection_dev")["net_pnl_usd"],
        "c2_passive_fills": sum(w["passive_fills"] for w in r2["windows"]),
        "portfolio_holdout_net": port["splits"]["holdout"]["net_pnl_usd"],
        "has_dsr": "dsr" in mt,
        "has_cscv": "cscv_pbo" in mt,
    }
    return res


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--self-test", action="store_true")
    ap.add_argument("--lakehouse", type=Path, default=Path.home() / "aleph-tx-lakehouse")
    ap.add_argument("--fee-evidence", type=Path, default=None)
    ap.add_argument("--output", type=Path, default=Path("/tmp/quant_upgrade_20261127/arb_carry/track02_funding_basis_carry_w01_w26.json"))
    ap.add_argument("--symbols", type=str, default="")
    args = ap.parse_args()

    if args.self_test:
        res = run_self_test()
        print(json.dumps(res, indent=2, sort_keys=True))
        return 0

    syms = {s.strip().upper() for s in args.symbols.split(",") if s.strip()} if args.symbols else None
    report = run_lakehouse_evaluation(
        args.lakehouse,
        fee_evidence_path=args.fee_evidence,
        output_path=args.output,
        symbols_filter=syms,
    )
    print(json.dumps({"status": "ok", "output": str(args.output), "coverage_pairs": len(report["coverage_summary"])}))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
