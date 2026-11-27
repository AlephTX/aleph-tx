#!/usr/bin/env python3
"""Causal event-driven RWA volume-farming replay & cycle-accounting evaluator (stdlib-only top level).

Replays captured L1/L2 BBO + unbucketed public_trades streams for RWA markets on Arcus,
Lighter, and Lighter-Robinhood across canonical selection_dev (w18-w21) and holdout (w23-w25)
10h windows. Keeps USDC (arcus, lighter) and USDG (lighter_robinhood) settlement domains and
account_fee_status explicit, never fabricates fills on trade-less feeds, and enforces exact
additive cycle accounting:

    gross_usd == spr + drft + pex + tslp
    net_usd   == gross_usd - fee_usd
    terminal_flat == True

Reuses `eval_volume_farming_cost` (`compute_volume_farming_cost`, `load_fee_profiles_from_evidence`)
with `rebate_bps=0.0` default. Imports `duckdb` lazily inside the lakehouse loader only.
"""

from __future__ import annotations

import argparse
import bisect
import dataclasses
import glob
import json
import math
import os
import sys
from collections import defaultdict, deque
from datetime import date, timedelta
from pathlib import Path
from typing import Any, Sequence

SCRIPT_DIR = Path(__file__).resolve().parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

from eval_volume_farming_cost import (  # noqa: E402
    VenueFeeProfile,
    compute_volume_farming_cost,
    load_fee_profiles_from_evidence,
)

TEN_HOURS_MS = 36_000_000
MAX_CONTINUOUS_GAP_MS = 60_000

CANONICAL_RWA_WINDOWS: dict[str, dict[str, dict[str, str]]] = {
    "selection_dev": {
        "w18": {"campaign_id": "tokyo-rwa-20261122T195951Z-w18", "utc_date": "2026-11-22"},
        "w19": {"campaign_id": "tokyo-rwa-20261123T061452Z-w19", "utc_date": "2026-11-23"},
        "w20": {"campaign_id": "tokyo-rwa-20261123T162953Z-w20", "utc_date": "2026-11-23"},
        "w21": {"campaign_id": "tokyo-rwa-20261124T024453Z-w21", "utc_date": "2026-11-24"},
    },
    "holdout": {
        "w23": {"campaign_id": "tokyo-rwa-20261124T231453Z-w23", "utc_date": "2026-11-24"},
        "w24": {"campaign_id": "tokyo-rwa-20261125T092953Z-w24", "utc_date": "2026-11-25"},
        "w25": {"campaign_id": "tokyo-rwa-20261125T194453Z-w25", "utc_date": "2026-11-25"},
    },
}

VENUE_SYMBOL_MAP: dict[str, dict[str, str]] = {
    "arcus": {
        "DRAM": "DRAM-USD",
        "SNDK": "SNDK-USD",
        "NVDA": "NVDA-USD",
        "QQQ": "QQQ-USD",
        "SPY": "SPY-USD",
        "AAPL": "AAPL-USD",
        "TSLA": "TSLA-USD",
    },
    "lighter": {
        "NVDA": "110",
        "TSLA": "112",
        "AAPL": "113",
        "SPY": "128",
        "QQQ": "129",
    },
    "lighter_robinhood": {
        "AAPL": "10",
        "NVDA": "15",
        "TSLA": "16",
        "QQQ": "25",
        "SPY": "26",
    },
    "trade_xyz": {
        "DRAM": "xyz:DRAM",
        "SNDK": "xyz:SNDK",
        "NVDA": "xyz:NVDA",
        "AAPL": "xyz:AAPL",
        "TSLA": "xyz:TSLA",
    },
}

VENUE_LATENCY_MS: dict[str, tuple[int, int, int]] = {
    # (order_delay_ms, cancel_delay_ms, exit_delay_ms)
    "arcus": (36, 33, 40),
    "lighter": (200, 200, 220),
    "lighter_robinhood": (200, 200, 220),
}

VENUE_SETTLEMENT_DEFAULTS: dict[str, dict[str, Any]] = {
    "arcus": {
        "settlement": "USDC",
        "maker_fee_bps": 0.0,
        "taker_fee_bps": 2.25,
        "account_fee_status": "public_tier0_schedule_only",
    },
    "lighter": {
        "settlement": "USDC",
        "maker_fee_bps": 0.0,
        "taker_fee_bps": 0.0,
        "account_fee_status": "public_tier0_unverified",
    },
    "lighter_robinhood": {
        "settlement": "USDG",
        "maker_fee_bps": 0.0,
        "taker_fee_bps": 0.0,
        "account_fee_status": "public_tier0_unverified",
    },
}


@dataclasses.dataclass(frozen=True)
class ReplayTarget:
    """Target (venue, symbol) replay specification with explicit settlement & ref candidates."""

    venue: str
    symbol: str
    settlement: str
    maker_fee_bps: float
    taker_fee_bps: float
    account_fee_status: str
    ref_venues: tuple[str, ...]


DEFAULT_TARGET_SPECS: tuple[tuple[str, str, tuple[str, ...]], ...] = (
    ("arcus", "DRAM", ("trade_xyz",)),
    ("arcus", "SNDK", ("trade_xyz",)),
    ("arcus", "NVDA", ("lighter_robinhood", "lighter", "trade_xyz")),
    ("arcus", "QQQ", ("lighter_robinhood", "lighter")),
    ("arcus", "SPY", ("lighter_robinhood", "lighter")),
    ("arcus", "AAPL", ("lighter_robinhood", "lighter", "trade_xyz")),
    ("arcus", "TSLA", ("lighter_robinhood", "lighter", "trade_xyz")),
    ("lighter", "NVDA", ("arcus",)),
    ("lighter", "QQQ", ("arcus",)),
    ("lighter", "SPY", ("arcus",)),
    ("lighter", "AAPL", ("arcus",)),
    ("lighter", "TSLA", ("arcus",)),
    ("lighter_robinhood", "NVDA", ("arcus",)),
    ("lighter_robinhood", "QQQ", ("arcus",)),
    ("lighter_robinhood", "SPY", ("arcus",)),
    ("lighter_robinhood", "AAPL", ("arcus",)),
    ("lighter_robinhood", "TSLA", ("arcus",)),
)


def build_default_targets(
    fee_profiles: Sequence[VenueFeeProfile] | None = None,
    *,
    assume_lighter_standard_zero: bool = False,
) -> list[ReplayTarget]:
    """Construct canonical RWA target matrix with explicit settlement and fee provenance."""
    profile_by_key = {p.key: p for p in (fee_profiles or ())}
    targets: list[ReplayTarget] = []
    for venue, symbol, ref_venues in DEFAULT_TARGET_SPECS:
        defaults = VENUE_SETTLEMENT_DEFAULTS[venue]
        maker_bps = float(defaults["maker_fee_bps"])
        taker_bps = float(defaults["taker_fee_bps"])
        fee_status = str(defaults["account_fee_status"])
        if venue == "arcus":
            p = profile_by_key.get("arcus_rwa_verified") or profile_by_key.get("arcus_rwa_tier0")
            if p is not None:
                maker_bps = p.maker_fee_bps
                taker_bps = p.taker_fee_bps
                fee_status = p.account_fee_status
        elif venue == "lighter":
            if assume_lighter_standard_zero:
                maker_bps = 0.0
                taker_bps = 0.0
                fee_status = "lighter_standard_zero_scenario"
            else:
                p = profile_by_key.get("lighter_verified") or profile_by_key.get("lighter_public_tier0")
                if p is not None:
                    maker_bps = p.maker_fee_bps
                    taker_bps = p.taker_fee_bps
                    fee_status = p.account_fee_status
        elif venue == "lighter_robinhood":
            if assume_lighter_standard_zero:
                maker_bps = 0.0
                taker_bps = 0.0
                fee_status = "lighter_standard_zero_scenario"
            else:
                p = (
                    profile_by_key.get("lighter_robinhood_verified")
                    or profile_by_key.get("lighter_robinhood_public_tier0")
                    or profile_by_key.get("lighter_public_tier0")
                )
                if p is not None:
                    maker_bps = p.maker_fee_bps
                    taker_bps = p.taker_fee_bps
                    fee_status = p.account_fee_status
        targets.append(
            ReplayTarget(
                venue=venue,
                symbol=symbol,
                settlement=str(defaults["settlement"]),
                maker_fee_bps=maker_bps,
                taker_fee_bps=taker_bps,
                account_fee_status=fee_status,
                ref_venues=ref_venues,
            )
        )
    return targets


DEFAULT_TARGETS: list[ReplayTarget] = build_default_targets()


@dataclasses.dataclass(frozen=True)
class RwaArmConfig:
    """Frozen strategy arm parameters for RWA volume-farming replay."""

    name: str
    quote_usd: float = 200.0
    max_pos_usd: float = 400.0
    max_hold_ms: int = 15_000
    keep_queue_ticks: int = 0
    order_delay_ms: int = 36
    cancel_delay_ms: int = 33
    exit_delay_ms: int = 40
    queue_ahead_fraction: float = 0.5
    maker_fee_bps: float = 0.0
    taker_fee_bps: float = 0.0
    rebate_bps: float = 0.0
    # S1 markout shield
    markout_shield_horizon_ms: int = 1_000
    markout_shield_adverse_threshold_bps: float = 0.0
    markout_shield_cooldown_ms: int = 0
    markout_shield_size_scale: float = 1.0
    # G1 Avellaneda-Stoikov inventory skew + vol-adaptive spread
    use_as_skew: bool = False
    gamma: float = 0.5
    as_skew_bps: float = 2.0
    base_extra_bps: float = 0.0
    vol_spread_mult: float = 0.0
    passive_exit_priority: bool = False
    # G2 asymmetric markout skew + high-vol pause
    asymmetric_markout_skew: bool = False
    asymmetric_markout_max_bps: float = 3.0
    high_vol_pause_sigma_bps: float | None = None
    # G3 cross-venue oracle fair anchor + stale-side suppression
    use_oracle_anchor: bool = False
    oracle_basis_tau_ms: float = 120_000.0
    oracle_fair_weight: float = 0.5
    oracle_stale_suppress_bps: float = 2.5
    oracle_max_age_ms: int = 5_000
    # G4 quiet-regime admission gate
    quiet_regime_only: bool = False
    quiet_min_spread_vol_ratio: float = 0.80
    quiet_max_sigma_bps: float = 1.2
    quiet_sigma_floor_bps: float = 0.05


def build_arms(
    venue: str,
    *,
    maker_fee_bps: float = 0.0,
    taker_fee_bps: float = 0.0,
    rebate_bps: float = 0.0,
    arm_names: Sequence[str] | None = None,
) -> list[RwaArmConfig]:
    """Return the 6 frozen strategy arms parameterized with venue latencies and fees."""
    if venue not in VENUE_LATENCY_MS:
        raise ValueError(f"Unsupported venue {venue!r}; expected one of {sorted(VENUE_LATENCY_MS)}")
    order_ms, cancel_ms, exit_ms = VENUE_LATENCY_MS[venue]
    common = dict(
        quote_usd=200.0,
        max_pos_usd=400.0,
        max_hold_ms=15_000,
        order_delay_ms=order_ms,
        cancel_delay_ms=cancel_ms,
        exit_delay_ms=exit_ms,
        queue_ahead_fraction=0.5,
        maker_fee_bps=maker_fee_bps,
        taker_fee_bps=taker_fee_bps,
        rebate_bps=rebate_bps,
    )
    arms = [
        RwaArmConfig(
            name="S0_baseline_touch",
            keep_queue_ticks=0,
            **common,
        ),
        RwaArmConfig(
            name="S1_markout_shield",
            keep_queue_ticks=3,
            markout_shield_horizon_ms=1_000,
            markout_shield_adverse_threshold_bps=1.0,
            markout_shield_cooldown_ms=2_000,
            markout_shield_size_scale=0.5,
            **common,
        ),
        RwaArmConfig(
            name="G1_as_inv_skew",
            keep_queue_ticks=3,
            use_as_skew=True,
            gamma=0.5,
            as_skew_bps=2.0,
            base_extra_bps=0.1,
            vol_spread_mult=0.5,
            passive_exit_priority=True,
            **common,
        ),
        RwaArmConfig(
            name="G2_markout_skew_pause",
            keep_queue_ticks=3,
            use_as_skew=True,
            gamma=0.5,
            as_skew_bps=2.0,
            base_extra_bps=0.1,
            vol_spread_mult=0.5,
            passive_exit_priority=True,
            asymmetric_markout_skew=True,
            asymmetric_markout_max_bps=3.0,
            high_vol_pause_sigma_bps=1.5,
            **common,
        ),
        RwaArmConfig(
            name="G3_markout_skew_oracle",
            keep_queue_ticks=3,
            use_as_skew=True,
            gamma=0.5,
            as_skew_bps=2.0,
            base_extra_bps=0.1,
            vol_spread_mult=0.5,
            passive_exit_priority=True,
            asymmetric_markout_skew=True,
            asymmetric_markout_max_bps=3.0,
            high_vol_pause_sigma_bps=1.5,
            use_oracle_anchor=True,
            oracle_basis_tau_ms=120_000.0,
            oracle_fair_weight=0.5,
            oracle_stale_suppress_bps=2.5,
            oracle_max_age_ms=5_000,
            **common,
        ),
        RwaArmConfig(
            name="G4_quiet_regime_only",
            keep_queue_ticks=3,
            use_as_skew=True,
            gamma=0.5,
            as_skew_bps=2.0,
            base_extra_bps=0.1,
            vol_spread_mult=0.5,
            passive_exit_priority=True,
            asymmetric_markout_skew=True,
            asymmetric_markout_max_bps=3.0,
            high_vol_pause_sigma_bps=1.5,
            use_oracle_anchor=True,
            oracle_basis_tau_ms=120_000.0,
            oracle_fair_weight=0.5,
            oracle_stale_suppress_bps=2.5,
            oracle_max_age_ms=5_000,
            quiet_regime_only=True,
            quiet_min_spread_vol_ratio=0.80,
            quiet_max_sigma_bps=1.2,
            quiet_sigma_floor_bps=0.05,
            **common,
        ),
    ]
    if arm_names is not None:
        wanted = set(arm_names)
        known = {a.name for a in arms}
        unknown = sorted(wanted - known)
        if unknown:
            raise ValueError(f"Unknown arm name(s): {unknown}; valid: {sorted(known)}")
        arms = [a for a in arms if a.name in wanted]
    return arms


@dataclasses.dataclass
class _RestingOrder:
    side: str  # "BID" or "ASK"
    price: float
    qty: float
    remaining: float
    queue_ahead: float
    placed_at_ms: int
    active_at_ms: int
    activated: bool = False
    cancel_effective_ms: int | None = None


@dataclasses.dataclass
class _OpenLot:
    sgn: float  # +1.0 for long (BID entry), -1.0 for short (ASK entry)
    qty: float
    entry_px: float
    entry_mid: float
    entry_ts: int


def _round_tick(px: float, tick: float, *, up: bool) -> float:
    units = px / tick
    return (math.ceil(units - 1e-9) if up else math.floor(units + 1e-9)) * tick


def _infer_tick_from_prices(prices: Sequence[float]) -> float:
    best = 1.0
    for px in prices[:200]:
        s = f"{px:.8f}".rstrip("0").rstrip(".")
        if "." in s:
            best = min(best, 10.0 ** (-len(s.split(".")[1])))
    return max(best, 1e-6)


def _normalize_book_event(ev: dict[str, Any]) -> dict[str, Any]:
    if "bids" in ev and "asks" in ev:
        return ev
    return {
        "ts": int(ev["ts"]),
        "kind": "book",
        "bids": [(float(ev["bid_px"]), max(float(ev["bid_sz"]), 1e-9))],
        "asks": [(float(ev["ask_px"]), max(float(ev["ask_sz"]), 1e-9))],
    }


def simulate_rwa_cycle_arm(
    events: Sequence[dict[str, Any]],
    tick: float,
    arm: RwaArmConfig,
    *,
    markout_horizons_ms: tuple[int, ...] = (1_000, 5_000, 10_000),
    high_vol_threshold_bps: float = 1.5,
) -> dict[str, Any]:
    """Run causal event-driven FIFO queue simulation with exact cycle-accounting decomposition."""
    if tick <= 0.0 or not math.isfinite(tick):
        raise ValueError(f"tick must be finite and > 0, got {tick}")

    norm_events: list[dict[str, Any]] = []
    mid_times: list[int] = []
    mid_vals: list[float] = []
    has_trade_events = False
    has_ref_events = False

    for ev in events:
        kind = ev.get("kind")
        if kind == "book":
            bev = _normalize_book_event(ev)
            bb, bsz = bev["bids"][0]
            ba, asz = bev["asks"][0]
            if bb <= 0.0 or ba <= bb or bsz <= 0.0 or asz <= 0.0:
                continue
            norm_events.append(bev)
            mid_times.append(int(bev["ts"]))
            mid_vals.append(0.5 * (bb + ba))
        elif kind == "trade":
            px = float(ev["price"])
            sz = float(ev["size"])
            if px <= 0.0 or sz <= 0.0:
                continue
            s_up = str(ev["side"]).upper()
            side_norm = "BUY" if s_up in ("BUY", "B", "BID") else "SELL"
            has_trade_events = True
            norm_events.append({"ts": int(ev["ts"]), "kind": "trade", "side": side_norm, "price": px, "size": sz})
        elif kind == "ref":
            rbid = float(ev["bid"])
            rask = float(ev["ask"])
            if rbid <= 0.0 or rask <= rbid:
                continue
            has_ref_events = True
            norm_events.append(
                {
                    "ts": int(ev["ts"]),
                    "kind": "ref",
                    "venue": str(ev.get("venue") or "ref"),
                    "bid": rbid,
                    "ask": rask,
                }
            )

    if not norm_events or not mid_times:
        cost_bd = compute_volume_farming_cost(
            0.0,
            0.0,
            arm.taker_fee_bps,
            maker_fee_bps=arm.maker_fee_bps,
            rebate_bps=arm.rebate_bps,
        )
        return {
            "arm": arm.name,
            "fill_evidence_mode": "public_trades" if has_trade_events else "no_trades_in_window",
            "ref_available": has_ref_events,
            "net": 0.0,
            "spr": 0.0,
            "drft": 0.0,
            "pex": 0.0,
            "tslp": 0.0,
            "gross_usd": 0.0,
            "gross_ex_fee_bps": 0.0,
            "fee_usd": 0.0,
            "cyc": 0,
            "can": 0,
            "haz": 0,
            "qpres": 0,
            "ntl": 0.0,
            "maker_volume_usd": 0.0,
            "taker_volume_usd": 0.0,
            "taker_exit_share_s": 0.0,
            "double_sided_usd_per_m": cost_bd.double_sided_usd_per_m,
            "maker_volume_usd_per_m": cost_bd.maker_volume_usd_per_m,
            "s_zero_double_sided_floor_usd_per_m": cost_bd.s_zero_double_sided_floor_usd_per_m,
            "price_through_fills": 0,
            "sweep_cleared_fills": 0,
            "sweep_touch_fills": 0,
            "stride_touch_fills": 0,
            "markout_1s_usd": 0.0,
            "markout_1s_bps": None,
            "markout_5s_bps": None,
            "markout_10s_bps": None,
            "markout_1s_den": 0.0,
            "markout_5s_den": 0.0,
            "markout_10s_den": 0.0,
            "net_per_ntl_bp": 0.0,
            "high_vol_ticks": 0,
            "fills": 0,
            "maker_fills": 0,
            "taker_exits": 0,
            "orders_placed": 0,
            "alo_rejected": 0,
            "terminal_flat": True,
        }

    def mid_at(ts_ms: int) -> float | None:
        if not mid_times or ts_ms > mid_times[-1]:
            return None
        idx = bisect.bisect_right(mid_times, ts_ms) - 1
        return mid_vals[idx] if idx >= 0 else None

    cur_book: dict[str, Any] | None = None
    ref_books: dict[str, tuple[int, float]] = {}
    ewma_basis: float | None = None
    last_basis_ts: int | None = None

    resting: dict[str, _RestingOrder | None] = {"BID": None, "ASK": None}
    open_lots: deque[_OpenLot] = deque()
    pos_qty = 0.0

    # Pending markout shield evaluations: (eval_ts_ms, maker_side, fill_px)
    pending_markouts: deque[tuple[int, str, float]] = deque()
    markout_ewma_bps = {"BID": 0.0, "ASK": 0.0}
    markout_cooldown_until = {"BID": 0, "ASK": 0}

    # Causal 1s realized volatility estimator
    last_mid_for_vol: tuple[int, float] | None = None
    var_bps2_per_s = 0.0
    high_vol_ticks = 0

    # Pending IOC exits: list of (exec_ts_ms, side, qty)
    pending_ioc_exits: list[tuple[int, str, float]] = []

    # Accounting accumulators
    cash_ex_fee_usd = 0.0
    fee_usd = 0.0
    spr = 0.0
    drft = 0.0
    pex = 0.0
    tslp = 0.0
    ntl = 0.0
    maker_volume_usd = 0.0
    taker_volume_usd = 0.0

    cyc = 0
    can = 0
    haz = 0
    qpres = 0
    orders_placed = 0
    alo_rejected = 0
    price_through_fills = 0
    sweep_cleared_fills = 0
    sweep_touch_fills = 0
    maker_fills = 0
    taker_exits = 0
    maker_fills_log: list[dict[str, Any]] = []

    def level_size(levels: Sequence[tuple[float, float]], price: float) -> float | None:
        for p, s in levels:
            if abs(p - price) <= tick * 0.25:
                return s
        return None

    def update_realized_vol(now_ms: int, mid: float) -> float:
        nonlocal last_mid_for_vol, var_bps2_per_s
        if last_mid_for_vol is not None:
            prev_ms, prev_mid = last_mid_for_vol
            dt_s = (now_ms - prev_ms) / 1000.0
            if 0.0 < dt_s <= 10.0 and prev_mid > 0.0 and mid > 0.0:
                ret_bps = math.log(mid / prev_mid) * 1e4
                inst_var = min(400.0, (ret_bps * ret_bps) / dt_s)
                alpha = 1.0 - math.exp(-dt_s / 15.0)
                var_bps2_per_s = (1.0 - alpha) * var_bps2_per_s + alpha * inst_var
        last_mid_for_vol = (now_ms, mid)
        return math.sqrt(var_bps2_per_s)

    def mature_markouts(now_ms: int, target_mid: float) -> None:
        if target_mid <= 0.0 or not pending_markouts:
            return
        thresh = arm.markout_shield_adverse_threshold_bps
        while pending_markouts and now_ms >= pending_markouts[0][0]:
            _, fside, fpx = pending_markouts.popleft()
            if fpx <= 0.0:
                continue
            sgn = 1.0 if fside == "BID" else -1.0
            mk_bps = sgn * (target_mid - fpx) / fpx * 1e4
            markout_ewma_bps[fside] = 0.75 * markout_ewma_bps[fside] + 0.25 * mk_bps
            if thresh > 0.0 and markout_ewma_bps[fside] < -2.0 * thresh and arm.markout_shield_cooldown_ms > 0:
                markout_cooldown_until[fside] = max(
                    markout_cooldown_until[fside], now_ms + arm.markout_shield_cooldown_ms
                )

    def record_fill(
        ts_ms: int,
        fill_side: str,  # "BUY" (BID) or "SELL" (ASK)
        fill_px: float,
        fill_qty: float,
        role: str,  # "maker" or "taker"
        mid_now: float,
    ) -> None:
        nonlocal cash_ex_fee_usd, fee_usd, pos_qty, spr, drft, pex, tslp, ntl
        nonlocal maker_volume_usd, taker_volume_usd, cyc, maker_fills, taker_exits
        if fill_qty <= 1e-12 or fill_px <= 0.0:
            return

        fill_sgn = 1.0 if fill_side == "BUY" else -1.0
        notl = fill_qty * fill_px
        if role == "maker":
            fee_bp = arm.maker_fee_bps - arm.rebate_bps
            maker_volume_usd += notl
            maker_fills += 1
            maker_side = "BID" if fill_side == "BUY" else "ASK"
            maker_fills_log.append(
                {"ts": ts_ms, "side": maker_side, "px": fill_px, "qty": fill_qty, "notl": notl}
            )
            pending_markouts.append((ts_ms + arm.markout_shield_horizon_ms, maker_side, fill_px))
            if len(pending_markouts) > 256:
                pending_markouts.popleft()
        else:
            fee_bp = arm.taker_fee_bps
            taker_volume_usd += notl
            taker_exits += 1

        cash_ex_fee_usd -= fill_sgn * notl
        fee_usd += notl * (fee_bp * 1e-4)
        pos_qty += fill_sgn * fill_qty

        rem = fill_qty
        closed_any = False
        while rem > 1e-12 and open_lots and open_lots[0].sgn != fill_sgn:
            lot = open_lots[0]
            closed_qty = min(rem, lot.qty)
            entry_spr = closed_qty * lot.sgn * (lot.entry_mid - lot.entry_px)
            drft_part = closed_qty * lot.sgn * (mid_now - lot.entry_mid)
            exit_edge = closed_qty * lot.sgn * (fill_px - mid_now)

            spr += entry_spr
            drft += drft_part
            if role == "maker":
                pex += exit_edge
            else:
                tslp += exit_edge

            ntl += closed_qty * fill_px
            closed_any = True
            rem -= closed_qty
            lot.qty -= closed_qty
            if lot.qty <= 1e-12:
                open_lots.popleft()

        if closed_any:
            cyc += 1

        if rem > 1e-12:
            open_lots.append(
                _OpenLot(
                    sgn=fill_sgn,
                    qty=rem,
                    entry_px=fill_px,
                    entry_mid=mid_now,
                    entry_ts=ts_ms,
                )
            )
        if not open_lots:
            pos_qty = 0.0

    def expire_cancels(now_ms: int) -> None:
        for side in ("BID", "ASK"):
            o = resting[side]
            if o is not None and o.cancel_effective_ms is not None and now_ms >= o.cancel_effective_ms:
                resting[side] = None

    def activate_arrivals(now_ms: int) -> None:
        nonlocal alo_rejected
        if cur_book is None:
            return
        bb, bsz = cur_book["bids"][0]
        ba, asz = cur_book["asks"][0]
        for side in ("BID", "ASK"):
            o = resting[side]
            if o is None or o.activated or now_ms < o.active_at_ms:
                continue
            crosses = (o.price >= ba - 1e-12) if side == "BID" else (o.price <= bb + 1e-12)
            if crosses:
                resting[side] = None
                alo_rejected += 1
                continue
            lvls = cur_book["bids"] if side == "BID" else cur_book["asks"]
            vis = level_size(lvls, o.price)
            if vis is not None:
                o.queue_ahead = arm.queue_ahead_fraction * vis
            elif (side == "BID" and o.price > bb + tick * 0.25) or (side == "ASK" and o.price < ba - tick * 0.25):
                # Inside spread (pennying): front of new price level
                o.queue_ahead = 0.0
            else:
                # Behind L1 when deeper book levels aren't captured: conservative L1 fraction floor
                l1_sz = bsz if side == "BID" else asz
                o.queue_ahead = arm.queue_ahead_fraction * l1_sz
            o.activated = True

    def process_pending_ioc_exits(now_ms: int) -> None:
        nonlocal pending_ioc_exits, haz
        if not pending_ioc_exits or cur_book is None:
            return
        bb = cur_book["bids"][0][0]
        ba = cur_book["asks"][0][0]
        mid_now = 0.5 * (bb + ba)
        still: list[tuple[int, str, float]] = []
        for exec_ts, side, qty in pending_ioc_exits:
            if now_ms < exec_ts:
                still.append((exec_ts, side, qty))
                continue
            if side == "SELL" and pos_qty > 1e-12:
                exec_qty = min(qty, pos_qty)
                record_fill(now_ms, "SELL", bb, exec_qty, "taker", mid_now)
                haz += 1
            elif side == "BUY" and pos_qty < -1e-12:
                exec_qty = min(qty, -pos_qty)
                record_fill(now_ms, "BUY", ba, exec_qty, "taker", mid_now)
                haz += 1
        pending_ioc_exits = still

    def maybe_schedule_timeout_exit(now_ms: int) -> None:
        if not open_lots or cur_book is None:
            return
        oldest = open_lots[0]
        if (now_ms - oldest.entry_ts) < arm.max_hold_ms:
            return
        target_side = "SELL" if pos_qty > 1e-12 else ("BUY" if pos_qty < -1e-12 else None)
        if target_side is None:
            return
        already_pending = sum(q for _, s, q in pending_ioc_exits if s == target_side)
        needed = abs(pos_qty) - already_pending
        if needed > 1e-12:
            pending_ioc_exits.append((now_ms + arm.exit_delay_ms, target_side, needed))

    def handle_trade(ts_ms: int, tr_side: str, tr_px: float, tr_sz: float) -> None:
        nonlocal price_through_fills, sweep_cleared_fills, sweep_touch_fills
        if cur_book is None:
            return
        maker_side = "ASK" if tr_side == "BUY" else "BID"
        o = resting[maker_side]
        if o is None or not o.activated:
            return

        tol = tick * 0.1
        through = (tr_px > o.price + tol) if maker_side == "ASK" else (tr_px < o.price - tol)
        at_touch = abs(tr_px - o.price) <= tol

        filled_qty = 0.0
        if through:
            filled_qty = min(o.remaining, tr_sz)
            if filled_qty > 1e-12:
                price_through_fills += 1
        elif at_touch:
            if tr_sz > o.queue_ahead:
                overflow_sz = tr_sz - o.queue_ahead
                o.queue_ahead = 0.0
                filled_qty = min(o.remaining, overflow_sz)
                if filled_qty > 1e-12:
                    if filled_qty < o.remaining - 1e-12:
                        sweep_touch_fills += 1
                    else:
                        sweep_cleared_fills += 1
            else:
                o.queue_ahead -= tr_sz

        if filled_qty > 1e-12:
            bb = cur_book["bids"][0][0]
            ba = cur_book["asks"][0][0]
            mid_now = 0.5 * (bb + ba)
            fill_side = "BUY" if maker_side == "BID" else "SELL"
            record_fill(ts_ms, fill_side, o.price, filled_qty, "maker", mid_now)
            o.remaining -= filled_qty
            if o.remaining <= 1e-12:
                resting[maker_side] = None

    def cancel_order(side: str, now_ms: int) -> None:
        nonlocal can
        o = resting[side]
        if o is not None and o.cancel_effective_ms is None:
            o.cancel_effective_ms = now_ms + arm.cancel_delay_ms
            can += 1

    def decide(now_ms: int, *, sigma_bps: float) -> None:
        nonlocal ewma_basis, last_basis_ts, orders_placed, qpres
        if cur_book is None:
            return
        expire_cancels(now_ms)

        bb = cur_book["bids"][0][0]
        ba = cur_book["asks"][0][0]
        venue_mid = 0.5 * (bb + ba)
        spread_bps = (ba - bb) / venue_mid * 1e4
        sp_ticks = max(1, round((ba - bb) / tick))

        # Causal oracle fair anchor (G3/G4)
        fair = venue_mid
        stale_suppress_side: str | None = None
        if arm.use_oracle_anchor:
            fresh_refs = [m for t, m in ref_books.values() if now_ms - t <= arm.oracle_max_age_ms]
            if fresh_refs:
                ref_mid = sorted(fresh_refs)[len(fresh_refs) // 2]
                raw_basis = venue_mid - ref_mid
                if last_basis_ts is not None and 0 <= now_ms - last_basis_ts <= arm.oracle_max_age_ms and ewma_basis is not None:
                    alpha = 1.0 - math.exp(-(now_ms - last_basis_ts) / arm.oracle_basis_tau_ms)
                    ewma_basis = ewma_basis + alpha * (raw_basis - ewma_basis)
                else:
                    ewma_basis = raw_basis
                last_basis_ts = now_ms
                ref_adj = ref_mid + ewma_basis
                if ref_adj > 0.0:
                    w = arm.oracle_fair_weight
                    fair = (1.0 - w) * venue_mid + w * ref_adj
                    div_bps = (venue_mid - ref_adj) / ref_adj * 1e4
                    if div_bps > arm.oracle_stale_suppress_bps:
                        # Local bid is stale-high vs ref_adj
                        stale_suppress_side = "BID"
                    elif div_bps < -arm.oracle_stale_suppress_bps:
                        # Local ask is stale-low vs ref_adj
                        stale_suppress_side = "ASK"
            elif last_basis_ts is not None and now_ms - last_basis_ts > arm.oracle_max_age_ms:
                ewma_basis = None
                last_basis_ts = None

        pos_usd = pos_qty * venue_mid
        q_norm = max(-1.5, min(1.5, pos_usd / arm.max_pos_usd)) if arm.max_pos_usd > 0 else 0.0
        if arm.use_as_skew:
            r_px = fair - q_norm * (arm.as_skew_bps * 1e-4) * fair
            half_extra_bps = arm.base_extra_bps + arm.vol_spread_mult * sigma_bps
        else:
            r_px = fair
            half_extra_bps = 0.0

        high_vol_paused = (
            arm.high_vol_pause_sigma_bps is not None and sigma_bps > arm.high_vol_pause_sigma_bps
        )
        quiet_blocked = False
        if arm.quiet_regime_only:
            ratio = spread_bps / max(sigma_bps, arm.quiet_sigma_floor_bps)
            quiet_blocked = (ratio < arm.quiet_min_spread_vol_ratio) or (sigma_bps > arm.quiet_max_sigma_bps)

        for side in ("BID", "ASK"):
            o = resting[side]
            is_reducing = (side == "BID" and pos_usd < -10.0) or (side == "ASK" and pos_usd > 10.0)
            cap_blocked = (side == "BID" and pos_usd >= arm.max_pos_usd) or (
                side == "ASK" and pos_usd <= -arm.max_pos_usd
            )

            # Opening gates (inventory-reducing exits are always permitted!)
            if not is_reducing:
                if cap_blocked:
                    cancel_order(side, now_ms)
                    continue
                if now_ms < markout_cooldown_until[side]:
                    cancel_order(side, now_ms)
                    continue
                if high_vol_paused or quiet_blocked:
                    cancel_order(side, now_ms)
                    continue
                if stale_suppress_side == side:
                    cancel_order(side, now_ms)
                    continue

            # Desired quote price
            if is_reducing and arm.passive_exit_priority:
                if sp_ticks >= 2:
                    desired_raw = min(bb + tick, ba - tick) if side == "BID" else max(ba - tick, bb + tick)
                else:
                    desired_raw = bb if side == "BID" else ba
            elif arm.use_as_skew:
                asym_bps = 0.0
                if arm.asymmetric_markout_skew and markout_ewma_bps[side] < 0.0:
                    asym_bps = min(arm.asymmetric_markout_max_bps, -markout_ewma_bps[side])
                req_half_bps = half_extra_bps + asym_bps
                if side == "BID":
                    desired_raw = min(bb, r_px * (1.0 - req_half_bps * 1e-4))
                else:
                    desired_raw = max(ba, r_px * (1.0 + req_half_bps * 1e-4))
            else:
                desired_raw = bb if side == "BID" else ba

            desired_px = _round_tick(desired_raw, tick, up=(side == "ASK"))
            # Ensure ALO never crosses opposite BBO
            if side == "BID" and desired_px >= ba - 1e-12:
                desired_px = _round_tick(ba - tick, tick, up=False)
            elif side == "ASK" and desired_px <= bb + 1e-12:
                desired_px = _round_tick(bb + tick, tick, up=True)

            crosses = (desired_px >= ba - 1e-12) if side == "BID" else (desired_px <= bb + 1e-12)
            if crosses or desired_px <= 0.0:
                cancel_order(side, now_ms)
                continue

            if o is not None:
                if o.cancel_effective_ms is not None:
                    continue
                ord_crosses = (o.price >= ba - 1e-12) if side == "BID" else (o.price <= bb + 1e-12)
                tick_diff = round(abs(o.price - desired_px) / tick)
                best_ref = bb if side == "BID" else ba
                touch_diff = round(abs(o.price - best_ref) / tick)
                within_keep = tick_diff <= arm.keep_queue_ticks or (
                    arm.keep_queue_ticks > 0 and touch_diff <= arm.keep_queue_ticks
                )
                if not ord_crosses and within_keep:
                    qpres += 1
                    continue
                cancel_order(side, now_ms)
                continue

            # Place new passive order
            size_scale = 1.0
            if (
                not is_reducing
                and arm.markout_shield_adverse_threshold_bps > 0.0
                and markout_ewma_bps[side] < -arm.markout_shield_adverse_threshold_bps
            ):
                size_scale = arm.markout_shield_size_scale

            order_usd = arm.quote_usd * size_scale
            if is_reducing:
                order_usd = min(max(abs(pos_usd), arm.quote_usd), max(arm.quote_usd, abs(pos_usd)))
            qty = order_usd / desired_px
            resting[side] = _RestingOrder(
                side=side,
                price=desired_px,
                qty=qty,
                remaining=qty,
                queue_ahead=0.0,
                placed_at_ms=now_ms,
                active_at_ms=now_ms + arm.order_delay_ms,
                activated=False,
            )
            orders_placed += 1

    last_sigma_bps = 0.0
    for ev in norm_events:
        ts = ev["ts"]
        expire_cancels(ts)
        process_pending_ioc_exits(ts)
        maybe_schedule_timeout_exit(ts)
        process_pending_ioc_exits(ts)

        if ev["kind"] == "book":
            cur_book = ev
            activate_arrivals(ts)
            for side, lvls in (("BID", ev["bids"]), ("ASK", ev["asks"])):
                o = resting[side]
                if o is not None and o.activated:
                    vis = level_size(lvls, o.price)
                    if vis is not None and vis < o.queue_ahead:
                        o.queue_ahead = vis
            bb = ev["bids"][0][0]
            ba = ev["asks"][0][0]
            mid_now = 0.5 * (bb + ba)
            last_sigma_bps = update_realized_vol(ts, mid_now)
            if last_sigma_bps > high_vol_threshold_bps:
                high_vol_ticks += 1
            mature_markouts(ts, mid_now)
            decide(ts, sigma_bps=last_sigma_bps)

        elif ev["kind"] == "ref":
            activate_arrivals(ts)
            ref_books[ev["venue"]] = (ts, 0.5 * (ev["bid"] + ev["ask"]))
            if arm.use_oracle_anchor and cur_book is not None:
                decide(ts, sigma_bps=last_sigma_bps)

        elif ev["kind"] == "trade":
            if cur_book is None:
                continue
            activate_arrivals(ts)
            handle_trade(ts, ev["side"], ev["price"], ev["size"])
            maybe_schedule_timeout_exit(ts)

    # Terminal liquidation: flatten any remaining open inventory via terminal IOC at terminal BBO
    last_ts = norm_events[-1]["ts"]
    if cur_book is not None:
        bb = cur_book["bids"][0][0]
        ba = cur_book["asks"][0][0]
        mid_end = 0.5 * (bb + ba)
        if pos_qty > 1e-12:
            record_fill(last_ts, "SELL", bb, pos_qty, "taker", mid_end)
            haz += 1
        elif pos_qty < -1e-12:
            record_fill(last_ts, "BUY", ba, -pos_qty, "taker", mid_end)
            haz += 1

    terminal_flat = len(open_lots) == 0 and abs(pos_qty) <= 1e-12
    gross_usd = spr + drft + pex + tslp
    net_usd = gross_usd - fee_usd
    gross_ex_fee_bps = (gross_usd / ntl) * 1e4 if ntl > 0.0 else 0.0
    net_per_ntl_bp = (net_usd / ntl) * 1e4 if ntl > 0.0 else 0.0
    taker_exit_share_s = min(1.0, max(0.0, taker_volume_usd / ntl)) if ntl > 0.0 else 0.0

    cost_bd = compute_volume_farming_cost(
        gross_ex_fee_bps,
        taker_exit_share_s,
        arm.taker_fee_bps,
        maker_fee_bps=arm.maker_fee_bps,
        rebate_bps=arm.rebate_bps,
    )

    markout_bps: dict[str, float | None] = {}
    markout_dens: dict[str, float] = {}
    markout_1s_usd = 0.0
    for h_ms in markout_horizons_ms:
        num = 0.0
        den = 0.0
        for fl in maker_fills_log:
            fm = mid_at(fl["ts"] + h_ms)
            if fm is None:
                continue
            sgn = 1.0 if fl["side"] == "BID" else -1.0
            mk_bps = sgn * (fm - fl["px"]) / fl["px"] * 1e4
            num += fl["notl"] * mk_bps
            den += fl["notl"]
            if h_ms == 1_000:
                markout_1s_usd += sgn * fl["qty"] * (fm - fl["px"])
        key = f"{h_ms // 1000}s"
        markout_bps[key] = round(num / den, 6) if den > 0.0 else None
        markout_dens[key] = den

    return {
        "arm": arm.name,
        "fill_evidence_mode": "public_trades" if has_trade_events else "no_trades_in_window",
        "ref_available": has_ref_events,
        "net": round(net_usd, 8),
        "spr": round(spr, 8),
        "drft": round(drft, 8),
        "pex": round(pex, 8),
        "tslp": round(tslp, 8),
        "gross_usd": round(gross_usd, 8),
        "gross_ex_fee_bps": round(gross_ex_fee_bps, 6),
        "fee_usd": round(fee_usd, 8),
        "cyc": cyc,
        "can": can,
        "haz": haz,
        "qpres": qpres,
        "ntl": round(ntl, 6),
        "maker_volume_usd": round(maker_volume_usd, 6),
        "taker_volume_usd": round(taker_volume_usd, 6),
        "taker_exit_share_s": round(taker_exit_share_s, 6),
        "double_sided_usd_per_m": cost_bd.double_sided_usd_per_m,
        "maker_volume_usd_per_m": cost_bd.maker_volume_usd_per_m,
        "s_zero_double_sided_floor_usd_per_m": cost_bd.s_zero_double_sided_floor_usd_per_m,
        "price_through_fills": price_through_fills,
        "sweep_cleared_fills": sweep_cleared_fills,
        "sweep_touch_fills": sweep_touch_fills,
        "stride_touch_fills": 0,
        "markout_1s_usd": round(markout_1s_usd, 6),
        "markout_1s_bps": markout_bps.get("1s"),
        "markout_5s_bps": markout_bps.get("5s"),
        "markout_10s_bps": markout_bps.get("10s"),
        "markout_1s_den": round(markout_dens.get("1s", 0.0), 6),
        "markout_5s_den": round(markout_dens.get("5s", 0.0), 6),
        "markout_10s_den": round(markout_dens.get("10s", 0.0), 6),
        "net_per_ntl_bp": round(net_per_ntl_bp, 6),
        "high_vol_ticks": high_vol_ticks,
        "fills": maker_fills + taker_exits,
        "maker_fills": maker_fills,
        "taker_exits": taker_exits,
        "orders_placed": orders_placed,
        "alo_rejected": alo_rejected,
        "terminal_flat": terminal_flat,
        # Exact unrounded audit fields for invariant verification
        "_exact_cash_ex_fee_usd": cash_ex_fee_usd,
        "_exact_gross_usd": gross_usd,
        "_exact_fee_usd": fee_usd,
        "_exact_net_usd": net_usd,
    }


def _campaign_parquet_files(
    lakehouse_dir: Path,
    dataset: str,
    venue: str,
    utc_date: str,
    campaign_id: str,
) -> list[str]:
    """Glob Hive parquet files across both `utc_date` and `utc_date + 1 day` for cross-midnight runs."""
    date_dirs: list[str]
    if not utc_date or utc_date == "*":
        date_dirs = ["*"]
    else:
        date_dirs = [utc_date]
        try:
            next_day = (date.fromisoformat(utc_date) + timedelta(days=1)).isoformat()
            date_dirs.append(next_day)
        except ValueError:
            pass
    files: list[str] = []
    for d in date_dirs:
        base = (
            lakehouse_dir
            / "parquet"
            / dataset
            / "collector_region=tokyo"
            / f"venue={venue}"
            / f"utc_date={d}"
            / f"campaign_id={campaign_id}"
        )
        files.extend(glob.glob(str(base / "*.parquet")))
    return sorted(set(files))


def load_whitelist_filter(whitelist_path: Path) -> set[tuple[str | None, str]]:
    """Parse a market whitelist JSON file into `(venue_or_none, symbol)` filter pairs."""
    data = json.loads(whitelist_path.read_text(encoding="utf-8"))
    allowed: set[tuple[str | None, str]] = set()

    def _add_entry(item: Any) -> None:
        if isinstance(item, str) and item.strip():
            allowed.add((None, item.strip().upper()))
        elif isinstance(item, dict):
            sym = item.get("symbol") or item.get("sym")
            ven = item.get("venue")
            if isinstance(sym, str) and sym.strip():
                v_str = str(ven).strip() if ven else None
                allowed.add((v_str, sym.strip().upper()))

    if isinstance(data, list):
        for item in data:
            _add_entry(item)
    elif isinstance(data, dict):
        matched_list_key = False
        for key in ("targets", "whitelist", "markets", "rows", "allowed"):
            if isinstance(data.get(key), list):
                matched_list_key = True
                for item in data[key]:
                    _add_entry(item)
        if not matched_list_key:
            for k, v in data.items():
                if isinstance(v, list):
                    for sym in v:
                        if isinstance(sym, str) and sym.strip():
                            allowed.add((str(k), sym.strip().upper()))
    else:
        raise ValueError(f"Unsupported whitelist JSON structure in {whitelist_path}")

    if not allowed:
        raise ValueError(f"No valid whitelist entries found in {whitelist_path}")
    return allowed


def filter_targets_by_whitelist(
    targets: Sequence[ReplayTarget],
    whitelist: set[tuple[str | None, str]] | None,
) -> list[ReplayTarget]:
    """Filter targets using parsed whitelist pairs."""
    if whitelist is None:
        return list(targets)
    out: list[ReplayTarget] = []
    for t in targets:
        sym_up = t.symbol.upper()
        if (t.venue, sym_up) in whitelist or (None, sym_up) in whitelist:
            out.append(t)
    return out


def aggregate_arm_summary(rows: Sequence[dict[str, Any]]) -> dict[str, Any]:
    """Aggregate replay rows cleanly by (arm, venue) and (arm, venue, symbol) without mixing domains."""

    def _agg_bucket(group_rows: Sequence[dict[str, Any]], *, include_symbol: bool) -> dict[str, Any]:
        first = group_rows[0]
        spr = sum(float(r["spr"]) for r in group_rows)
        drft = sum(float(r["drft"]) for r in group_rows)
        pex = sum(float(r["pex"]) for r in group_rows)
        tslp = sum(float(r["tslp"]) for r in group_rows)
        gross_usd = spr + drft + pex + tslp
        fee_usd = sum(float(r["fee_usd"]) for r in group_rows)
        net_usd = gross_usd - fee_usd
        ntl = sum(float(r["ntl"]) for r in group_rows)
        maker_vol = sum(float(r["maker_volume_usd"]) for r in group_rows)
        taker_vol = sum(float(r["taker_volume_usd"]) for r in group_rows)
        gross_bp = (gross_usd / ntl) * 1e4 if ntl > 0.0 else 0.0
        s_share = min(1.0, max(0.0, taker_vol / ntl)) if ntl > 0.0 else 0.0

        cost_bd = compute_volume_farming_cost(
            gross_bp,
            s_share,
            float(first["taker_fee_bps"]),
            maker_fee_bps=float(first["maker_fee_bps"]),
            rebate_bps=float(first["rebate_bps"]),
        )

        def _agg_mk(bps_key: str, den_key: str) -> float | None:
            num = 0.0
            den = 0.0
            for r in group_rows:
                val = r.get(bps_key)
                d = float(r.get(den_key) or 0.0)
                if val is not None and d > 0.0:
                    num += float(val) * d
                    den += d
            return round(num / den, 6) if den > 0.0 else None

        summary: dict[str, Any] = {
            "arm": first["arm"],
            "venue": first["venue"],
            "settlement": first["settlement"],
            "account_fee_status": first["account_fee_status"],
            "rows": len(group_rows),
            "cont_rows": sum(1 for r in group_rows if r.get("cont")),
            "cyc": sum(int(r["cyc"]) for r in group_rows),
            "fills": sum(int(r["fills"]) for r in group_rows),
            "spr": round(spr, 6),
            "drft": round(drft, 6),
            "pex": round(pex, 6),
            "tslp": round(tslp, 6),
            "gross_usd": round(gross_usd, 6),
            "fee_usd": round(fee_usd, 6),
            "net_usd": round(net_usd, 6),
            "ntl": round(ntl, 4),
            "maker_volume_usd": round(maker_vol, 4),
            "taker_volume_usd": round(taker_vol, 4),
            "gross_ex_fee_bps": round(gross_bp, 6),
            "taker_exit_share_s": round(s_share, 6),
            "double_sided_usd_per_m": cost_bd.double_sided_usd_per_m,
            "maker_volume_usd_per_m": cost_bd.maker_volume_usd_per_m,
            "s_zero_double_sided_floor_usd_per_m": cost_bd.s_zero_double_sided_floor_usd_per_m,
            "markout_1s_bps": _agg_mk("markout_1s_bps", "markout_1s_den"),
            "markout_5s_bps": _agg_mk("markout_5s_bps", "markout_5s_den"),
            "markout_10s_bps": _agg_mk("markout_10s_bps", "markout_10s_den"),
            "high_vol_ticks": sum(int(r["high_vol_ticks"]) for r in group_rows),
        }
        if include_symbol:
            summary["symbol"] = first["symbol"]
        return summary

    by_arm_venue_groups: dict[tuple[str, str], list[dict[str, Any]]] = defaultdict(list)
    by_arm_venue_sym_groups: dict[tuple[str, str, str], list[dict[str, Any]]] = defaultdict(list)
    for row in rows:
        if row.get("skipped"):
            continue
        by_arm_venue_groups[(row["arm"], row["venue"])].append(row)
        by_arm_venue_sym_groups[(row["arm"], row["venue"], row["symbol"])].append(row)

    by_arm_venue = {
        f"{arm}|{venue}": _agg_bucket(grp, include_symbol=False)
        for (arm, venue), grp in sorted(by_arm_venue_groups.items())
    }
    by_arm_venue_symbol = {
        f"{arm}|{venue}|{sym}": _agg_bucket(grp, include_symbol=True)
        for (arm, venue, sym), grp in sorted(by_arm_venue_sym_groups.items())
    }
    return {
        "by_arm_venue": by_arm_venue,
        "by_arm_venue_symbol": by_arm_venue_symbol,
    }


def run_duckdb_rwa_replay(
    lakehouse_dir: Path,
    *,
    split: str = "holdout",
    window_ids: Sequence[str] | None = None,
    targets: Sequence[ReplayTarget] | None = None,
    arm_names: Sequence[str] | None = None,
    rebate_bps: float = 0.0,
    assume_lighter_standard_zero: bool = False,
) -> dict[str, Any]:
    """Load Hive parquet BBO + public_trades via in-memory DuckDB and run causal RWA replay."""
    import duckdb  # Lazy import so stdlib-only CI never needs duckdb at module import time

    split_windows = CANONICAL_RWA_WINDOWS[split]
    if window_ids:
        unknown = [w for w in window_ids if w not in split_windows]
        if unknown:
            raise ValueError(f"Unknown window(s) {unknown} for split {split!r}; valid: {sorted(split_windows)}")
        selected_windows = {w: split_windows[w] for w in window_ids}
    else:
        selected_windows = dict(split_windows)

    active_targets = (
        list(targets)
        if targets is not None
        else build_default_targets(assume_lighter_standard_zero=assume_lighter_standard_zero)
    )
    con = duckdb.connect(":memory:")
    con.execute("SET preserve_insertion_order=false; SET threads=2; SET memory_limit='2GB';")

    bucket_cache: dict[
        tuple[str, str, str], tuple[list[tuple[int, float, float, float, float]], dict[str, Any]]
    ] = {}

    def fetch_books_bucketed(
        win_id: str,
        venue: str,
        market: str,
        bucket_ms: int = 250,
    ) -> tuple[list[tuple[int, float, float, float, float]], dict[str, Any]]:
        key = (win_id, venue, market)
        if key in bucket_cache:
            return bucket_cache[key]
        c_info = selected_windows[win_id]
        files = _campaign_parquet_files(
            lakehouse_dir, "bbo_quotes", venue, c_info["utc_date"], c_info["campaign_id"]
        )
        if not files:
            empty_cont = {
                "raw_events": 0,
                "duration_ms": 0,
                "hours": 0.0,
                "max_gap_ms": 0,
                "is_continuous_10h": False,
            }
            bucket_cache[key] = ([], empty_cont)
            return bucket_cache[key]

        f_list_sql = "[" + ", ".join(f"'{p}'" for p in files) + "]"
        raw_stats = con.execute(
            f"""
            WITH s AS (
                SELECT available_at_ms,
                       LAG(available_at_ms) OVER (ORDER BY available_at_ms) AS prev_ts
                FROM read_parquet({f_list_sql})
                WHERE market = ?
                  AND tradable = true
                  AND bid_px_f64 > 0 AND ask_px_f64 > bid_px_f64
            )
            SELECT COUNT(*), MIN(available_at_ms), MAX(available_at_ms), COALESCE(MAX(available_at_ms - prev_ts), 0)
            FROM s
            """,
            [market],
        ).fetchone()
        cnt, t0, t1, mgap = raw_stats
        if not cnt or cnt < 2:
            empty_cont = {
                "raw_events": int(cnt or 0),
                "duration_ms": 0,
                "hours": 0.0,
                "max_gap_ms": 0,
                "is_continuous_10h": False,
            }
            bucket_cache[key] = ([], empty_cont)
            return bucket_cache[key]

        dur = int(t1 - t0)
        cont = {
            "raw_events": int(cnt),
            "duration_ms": dur,
            "hours": round(dur / 3_600_000.0, 3),
            "max_gap_ms": int(mgap),
            "is_continuous_10h": dur >= TEN_HOURS_MS and int(mgap) <= MAX_CONTINUOUS_GAP_MS,
        }
        rows = con.execute(
            f"""
            SELECT MIN(available_at_ms) AS ts_ms,
                   arg_min(bid_px_f64, available_at_ms) AS bid_px,
                   arg_min(CAST(bid_sz AS DOUBLE), available_at_ms) AS bid_sz,
                   arg_min(ask_px_f64, available_at_ms) AS ask_px,
                   arg_min(CAST(ask_sz AS DOUBLE), available_at_ms) AS ask_sz
            FROM read_parquet({f_list_sql})
            WHERE market = ?
              AND tradable = true
              AND bid_px_f64 > 0 AND ask_px_f64 > bid_px_f64
            GROUP BY (available_at_ms // {int(bucket_ms)})
            ORDER BY ts_ms ASC
            """,
            [market],
        ).fetchall()
        bucket_cache[key] = (rows, cont)
        return bucket_cache[key]

    def fetch_trades(win_id: str, venue: str, market: str) -> list[tuple[int, str, float, float]]:
        c_info = selected_windows[win_id]
        files = _campaign_parquet_files(
            lakehouse_dir, "public_trades", venue, c_info["utc_date"], c_info["campaign_id"]
        )
        if not files:
            return []
        f_list_sql = "[" + ", ".join(f"'{p}'" for p in files) + "]"
        return con.execute(
            f"""
            SELECT available_at_ms, aggressor_side, price_f64, size_f64
            FROM read_parquet({f_list_sql})
            WHERE market = ?
              AND NOT is_zero_price_transfer
              AND price_f64 > 0 AND size_f64 > 0
            ORDER BY available_at_ms ASC
            """,
            [market],
        ).fetchall()

    rows_out: list[dict[str, Any]] = []
    for win_id, c_info in selected_windows.items():
        for target in active_targets:
            raw_mkt = VENUE_SYMBOL_MAP[target.venue][target.symbol]
            b_rows, cont = fetch_books_bucketed(win_id, target.venue, raw_mkt, bucket_ms=250)
            if len(b_rows) < 200:
                rows_out.append(
                    {
                        "win": win_id,
                        "campaign_id": c_info["campaign_id"],
                        "sym": f"{target.symbol}_{target.venue}",
                        "venue": target.venue,
                        "symbol": target.symbol,
                        "raw_market": raw_mkt,
                        "settlement": target.settlement,
                        "account_fee_status": target.account_fee_status,
                        "cont": False,
                        "continuity": cont,
                        "skipped": f"too_few_books:{len(b_rows)}",
                    }
                )
                continue

            events: list[dict[str, Any]] = [
                {
                    "ts": int(r[0]),
                    "kind": "book",
                    "bids": [(float(r[1]), max(float(r[2]), 1e-9))],
                    "asks": [(float(r[3]), max(float(r[4]), 1e-9))],
                }
                for r in b_rows
            ]

            ref_venue_used: str | None = None
            for ref_v in target.ref_venues:
                if ref_v not in VENUE_SYMBOL_MAP or target.symbol not in VENUE_SYMBOL_MAP[ref_v]:
                    continue
                ref_raw = VENUE_SYMBOL_MAP[ref_v][target.symbol]
                r_rows, _ = fetch_books_bucketed(win_id, ref_v, ref_raw, bucket_ms=250)
                if len(r_rows) >= 200:
                    ref_venue_used = ref_v
                    for rr in r_rows:
                        events.append(
                            {
                                "ts": int(rr[0]),
                                "kind": "ref",
                                "venue": ref_v,
                                "bid": float(rr[1]),
                                "ask": float(rr[3]),
                            }
                        )
                    break

            for ts_ms, agg_side, px, sz in fetch_trades(win_id, target.venue, raw_mkt):
                s_up = str(agg_side).upper()
                side_str = "BUY" if s_up in ("BUY", "B", "BID") else "SELL"
                events.append(
                    {
                        "ts": int(ts_ms),
                        "kind": "trade",
                        "side": side_str,
                        "price": float(px),
                        "size": float(sz),
                    }
                )

            events.sort(key=lambda e: e["ts"])
            tick = _infer_tick_from_prices([float(r[1]) for r in b_rows])
            arms = build_arms(
                target.venue,
                maker_fee_bps=target.maker_fee_bps,
                taker_fee_bps=target.taker_fee_bps,
                rebate_bps=rebate_bps,
                arm_names=arm_names,
            )
            for arm_cfg in arms:
                sim_res = simulate_rwa_cycle_arm(events, tick, arm_cfg)
                sim_res.pop("_exact_cash_ex_fee_usd", None)
                sim_res.pop("_exact_gross_usd", None)
                sim_res.pop("_exact_fee_usd", None)
                sim_res.pop("_exact_net_usd", None)
                rows_out.append(
                    {
                        "win": win_id,
                        "campaign_id": c_info["campaign_id"],
                        "sym": f"{target.symbol}_{target.venue}",
                        "venue": target.venue,
                        "symbol": target.symbol,
                        "raw_market": raw_mkt,
                        "settlement": target.settlement,
                        "account_fee_status": target.account_fee_status,
                        "maker_fee_bps": target.maker_fee_bps,
                        "taker_fee_bps": target.taker_fee_bps,
                        "rebate_bps": rebate_bps,
                        "tick_inferred": tick,
                        "cont": bool(cont["is_continuous_10h"]),
                        "continuity": cont,
                        "ref_venue_used": ref_venue_used,
                        "ref_available": ref_venue_used is not None,
                        **sim_res,
                    }
                )

    con.close()
    return {
        "schema": "rwa_volume_farming_replay_v1",
        "split": split,
        "rebate_bps": rebate_bps,
        "assume_lighter_standard_zero": bool(assume_lighter_standard_zero),
        "fee_scenario": (
            "lighter_standard_zero_scenario" if assume_lighter_standard_zero else "verified_or_default"
        ),
        "windows": selected_windows,
        "targets": [dataclasses.asdict(t) for t in active_targets],
        "rows": rows_out,
        "arm_summary": aggregate_arm_summary(rows_out),
    }


def run_self_test() -> dict[str, Any]:
    """Verify exact accounting, queue-ahead, IOC exit, and arm invariants on synthetic streams."""
    # 1. Synthetic multi-fill stream with passive entry, passive exit, and IOC timeout exit
    tick = 0.01
    events: list[dict[str, Any]] = [
        {"ts": 0, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.04, 10.0)]},
        {"ts": 0, "kind": "ref", "venue": "trade_xyz", "bid": 100.00, "ask": 100.04},
        # At ts=50, order placed at ts=0 is active (arcus order_delay=36ms). queue_ahead = 0.5*10 = 5.0
        # Sell trade of 7.0 at 100.00 clears 5.0 queue_ahead and fills 2.0 (capped by remaining=2.0)
        {"ts": 100, "kind": "trade", "side": "SELL", "price": 100.00, "size": 7.0},
        # Book drifts up slightly, then buy trade hits ask to close passively
        {"ts": 500, "kind": "book", "bids": [(100.02, 4.0)], "asks": [(100.06, 4.0)]},
        {"ts": 500, "kind": "ref", "venue": "trade_xyz", "bid": 100.02, "ask": 100.06},
        {"ts": 600, "kind": "trade", "side": "BUY", "price": 100.06, "size": 10.0},
        # Second fill that hits 15s max_hold timeout -> IOC exit
        {"ts": 1_000, "kind": "book", "bids": [(100.00, 2.0)], "asks": [(100.04, 2.0)]},
        {"ts": 1_100, "kind": "trade", "side": "SELL", "price": 99.98, "size": 5.0},
        {"ts": 17_000, "kind": "book", "bids": [(99.96, 5.0)], "asks": [(100.02, 5.0)]},
        {"ts": 18_500, "kind": "book", "bids": [(99.97, 5.0)], "asks": [(100.03, 5.0)]},
    ]

    checked_arms = []
    for arm_cfg in build_arms("arcus", maker_fee_bps=0.0, taker_fee_bps=2.25, rebate_bps=0.0):
        res = simulate_rwa_cycle_arm(events, tick, arm_cfg)
        assert res["terminal_flat"] is True, f"{arm_cfg.name} not terminal_flat"
        gross_diff = abs(res["_exact_gross_usd"] - res["_exact_cash_ex_fee_usd"])
        assert gross_diff < 1e-9, f"{arm_cfg.name} gross mismatch: {gross_diff}"
        net_diff = abs(res["_exact_net_usd"] - (res["_exact_gross_usd"] - res["_exact_fee_usd"]))
        assert net_diff < 1e-9, f"{arm_cfg.name} net mismatch: {net_diff}"
        if res["ntl"] > 0:
            expected_s = res["taker_volume_usd"] / res["ntl"]
            assert abs(res["taker_exit_share_s"] - expected_s) < 1e-5
        checked_arms.append(arm_cfg.name)

    # 2. Trade-less feed must yield strictly 0 fills and no_trades_in_window
    tradeless_events = [
        {"ts": 0, "kind": "book", "bids": [(100.00, 5.0)], "asks": [(100.05, 5.0)]},
        {"ts": 250, "kind": "book", "bids": [(100.10, 5.0)], "asks": [(100.15, 5.0)]},
    ]
    tl_res = simulate_rwa_cycle_arm(
        tradeless_events, tick, build_arms("lighter", maker_fee_bps=0.0, taker_fee_bps=0.0)[0]
    )
    assert tl_res["fills"] == 0 and tl_res["ntl"] == 0.0
    assert tl_res["fill_evidence_mode"] == "no_trades_in_window"

    # 3. Settlement separation check
    settlements = {(t.venue, t.settlement) for t in DEFAULT_TARGETS}
    assert ("arcus", "USDC") in settlements
    assert ("lighter", "USDC") in settlements
    assert ("lighter_robinhood", "USDG") in settlements

    return {
        "self_test": "ok",
        "arms_verified": checked_arms,
        "settlements_verified": sorted(f"{v}:{s}" for v, s in settlements),
        "tradeless_fills": tl_res["fills"],
    }


def main(argv: Sequence[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description="Causal RWA volume-farming replay & cycle-accounting evaluator")
    ap.add_argument(
        "--lakehouse",
        type=Path,
        default=Path(os.path.expanduser("~/aleph-tx-lakehouse")),
        help="Path to Aleph lakehouse root directory",
    )
    ap.add_argument(
        "--split",
        choices=tuple(CANONICAL_RWA_WINDOWS.keys()),
        default="holdout",
        help="Canonical window split (selection_dev or holdout)",
    )
    ap.add_argument(
        "--windows",
        type=str,
        default=None,
        help="Comma-separated subset of window IDs within --split (e.g. w18,w19)",
    )
    ap.add_argument("--whitelist", type=Path, default=None, help="Optional JSON market whitelist path")
    ap.add_argument("--fee-evidence", type=Path, default=None, help="Optional account_fee_evidence JSON path")
    ap.add_argument(
        "--assume-lighter-standard-zero",
        action="store_true",
        help="Explicit counterfactual scenario overriding Lighter fees to 0/0 bps with lighter_standard_zero_scenario provenance",
    )
    ap.add_argument("--rebate-bps", type=float, default=0.0, help="Explicit maker rebate bps (default 0.0)")
    ap.add_argument("--arms", type=str, default=None, help="Comma-separated subset of arm names to run")
    ap.add_argument("--output", type=Path, default=None, help="Optional JSON output file path")
    ap.add_argument("--self-test", action="store_true", help="Run built-in algebraic & replay invariant checks")
    args = ap.parse_args(argv)

    if args.self_test:
        rendered = json.dumps(run_self_test(), indent=2, allow_nan=False)
        if args.output:
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_text(rendered + "\n", encoding="utf-8")
        else:
            print(rendered)
        return 0

    if args.rebate_bps < 0.0 or not math.isfinite(args.rebate_bps):
        ap.error("--rebate-bps must be finite and >= 0")

    fee_profiles = load_fee_profiles_from_evidence(args.fee_evidence) if args.fee_evidence else None
    targets = build_default_targets(
        fee_profiles,
        assume_lighter_standard_zero=args.assume_lighter_standard_zero,
    )
    if args.whitelist:
        wl = load_whitelist_filter(args.whitelist)
        targets = filter_targets_by_whitelist(targets, wl)
        if not targets:
            ap.error(f"Whitelist {args.whitelist} matched 0 canonical targets")

    win_list = [w.strip() for w in args.windows.split(",") if w.strip()] if args.windows else None
    arm_list = [a.strip() for a in args.arms.split(",") if a.strip()] if args.arms else None

    result = run_duckdb_rwa_replay(
        args.lakehouse,
        split=args.split,
        window_ids=win_list,
        targets=targets,
        arm_names=arm_list,
        rebate_bps=args.rebate_bps,
        assume_lighter_standard_zero=args.assume_lighter_standard_zero,
    )
    rendered = json.dumps(result, indent=2, allow_nan=False)
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(rendered + "\n", encoding="utf-8")
    else:
        print(rendered)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
