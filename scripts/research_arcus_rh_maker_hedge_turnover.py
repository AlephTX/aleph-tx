#!/usr/bin/env python3
"""Method 2 (Arcus Passive Maker x Lighter-RH Taker Hedge) W01-W27 P0 Turnover Benchmark.

Strictly stdlib-only at module top level (`duckdb` and `numpy` imported lazily inside
lakehouse query helpers ONLY; verified under `python3 -S`).

Evaluates Method 2 (`Arcus 0.00 bps RWA maker` x `Lighter-RH 0.00 bps taker`) across all
overlapping Tokyo `W01-W27` windows (strictly excluding `-w28`), with:
  1. Causal anchor to Lighter-RH BBO (`microprice`, causal EWMA fair basis, `lead_beta`),
     sweeping `entry_edge_bps` over `[-1.0, -0.5, 0.0, 0.25, 0.5, 1.0, 2.0]` to trace
     the Volume vs $/M Pareto frontier under `zero_points` (points = 0).
  2. Arcus `public_trades` FIFO queue depletion (`queue_ahead_fraction = 0.5`),
     maker submit/cancel latency (`submit_rtt_ms = 50/100/200`, `cancel_ms = 50/150/300`)
     with cancel-in-flight fills, plus unprinted toxic-fill stress (`strict_through`
     25/50/100% and `opposite_touch` 25/50%).
  3. Hedge execution after `hedge_delay_ms` (`330 ms` baseline, `150/330/500 ms` sweep),
     pre-install pending hedge evaluation, single-use L1 depth conservation, `+330 ms`
     orphan retries (never `0 ms`), `declared_end_ms` cutoff immunity, and exact
     6-term cash PnL conservation:
     `arcus_spread_capture + arcus_adverse + rh_hedge_slip + fee_cost + fx_drag + residual_drain`.
  4. Multi-venue empirical comparison on `W01-W27` against TradeXYZ (`trade_xyz`),
     Entropy (`entropy`), Bulk (`bulk`), Backpack (`backpack`), and Extended (`extended`).
"""

from __future__ import annotations

import argparse
import concurrent.futures
import dataclasses
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

from track_fee_evidence import (  # noqa: E402
    build_fee_provenance,
    load_track_fee_evidence,
    resolve_venue_fee,
)

HOUR_MS = 3_600_000
TEN_HOURS_MS = 36_000_000
DEFAULT_MAX_GAP_MS = 60_000
DEFAULT_MAX_BOOK_AGE_MS = 5_000
DEFAULT_WARMUP_MS = 1_800_000
DEFAULT_TERMINAL_DRAIN_MS = 60_000
USD_PER_MILLION = 1_000_000.0

RWA_ARCUS_RH_SYMBOLS: tuple[str, ...] = (
    "AAPL",
    "AMD",
    "AMZN",
    "COIN",
    "CRCL",
    "GOOGL",
    "META",
    "MSFT",
    "MU",
    "NVDA",
    "PLTR",
    "QQQ",
    "SPY",
    "TSLA",
)

# 16 exact 1:1 RWA stock-perp symbols overlapping Arcus & TradeXYZ
# (excludes xyz:XYZ100 ~26,150 vs QQQ ~636 and xyz:SP500 ~7,208 vs SPY ~719 index scale mismatches).
RWA_ARCUS_TRADEXYZ_SYMBOLS: tuple[str, ...] = (
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

RWA_ARCUS_RH_STOCK12_SYMBOLS: tuple[str, ...] = (
    "AAPL",
    "AMD",
    "AMZN",
    "COIN",
    "CRCL",
    "GOOGL",
    "META",
    "MSFT",
    "MU",
    "NVDA",
    "PLTR",
    "TSLA",
)

RWA_ARCUS_ENTROPY_SYMBOLS: tuple[str, ...] = (
    "AAPL",
    "CRCL",
    "DRAM",
    "NVDA",
    "SNDK",
)

CRYPTO_SYMBOLS: tuple[str, ...] = ("BTC", "ETH", "SOL")

VENUE_SYMBOL_MAP: dict[str, dict[str, str]] = {
    "arcus": {
        **{s: f"{s}-USD" for s in RWA_ARCUS_TRADEXYZ_SYMBOLS},
        "QQQ": "QQQ-USD",
        "SPY": "SPY-USD",
        **{s: f"{s}-USD" for s in CRYPTO_SYMBOLS},
    },
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
        "PLTR": "34",
    },
    "trade_xyz": {
        "AAPL": "xyz:AAPL",
        "AMD": "xyz:AMD",
        "AMZN": "xyz:AMZN",
        "COIN": "xyz:COIN",
        "CRCL": "xyz:CRCL",
        "DRAM": "xyz:DRAM",
        "GOOGL": "xyz:GOOGL",
        "HOOD": "xyz:HOOD",
        "META": "xyz:META",
        "MSFT": "xyz:MSFT",
        "MSTR": "xyz:MSTR",
        "MU": "xyz:MU",
        "NVDA": "xyz:NVDA",
        "PLTR": "xyz:PLTR",
        "QQQ": "xyz:XYZ100",
        "SNDK": "xyz:SNDK",
        "SPY": "xyz:SP500",
        "TSLA": "xyz:TSLA",
    },
    "entropy": {
        "AAPL": "io:AAPL",
        "CRCL": "io:CRCL",
        "DRAM": "io:DRAM",
        "NVDA": "io:NVDA",
        "SNDK": "io:SNDK",
    },
    "bulk": {
        "BTC": "BTC-USD",
        "ETH": "ETH-USD",
        "SOL": "SOL-USD",
        "CRCL": "CRCL-USD",
        "MSTR": "MSTR-USD",
        "MU": "MU-USD",
        "NVDA": "NVDA-USD",
        "QQQ": "QQQ-USD",
        "SNDK": "SNDK-USD",
        "SPY": "US500-USD",
        "TSLA": "TSLA-USD",
    },
    "extended": {
        "BTC": "BTC-USD",
        "ETH": "ETH-USD",
        "SOL": "SOL-USD",
    },
}

ENTRY_EDGE_PARETO_BPS: tuple[float, ...] = (-1.0, -0.5, 0.0, 0.25, 0.5, 1.0, 2.0)
FX_DRAG_BPS_GRID: tuple[float, ...] = (0.0, 1.0, 3.0)
CLIP_USD_GRID: tuple[float, ...] = (50.0, 100.0, 200.0, 500.0)
MAKER_LATENCY_GRID: tuple[tuple[int, int], ...] = (
    (50, 50),
    (50, 150),
    (100, 50),
    (100, 150),
    (100, 300),
    (200, 150),
    (200, 300),
)
HEDGE_DELAY_GRID_MS: tuple[int, ...] = (150, 330, 500)
TOXIC_STRESS_GRID: tuple[tuple[str, str, float], ...] = (
    ("none_0", "none", 0.0),
    ("strict_through_25", "strict_through", 0.25),
    ("strict_through_50", "strict_through", 0.50),
    ("strict_through_100", "strict_through", 1.00),
    ("opposite_touch_25", "opposite_touch", 0.25),
    ("opposite_touch_50", "opposite_touch", 0.50),
)


def split_for_window(w_num: int) -> str:
    if w_num <= 20:
        return "selection_dev"
    if w_num <= 25:
        return "holdout"
    return "w26_w27_oos"


def classify_utc_session(ts_ms: int) -> str:
    days_since_unix = ts_ms // 86_400_000
    weekday = (days_since_unix + 3) % 7
    if weekday >= 5:
        return "weekend"
    minute_of_day = (ts_ms // 60_000) % 1_440
    if minute_of_day < 660:
        return "overnight_asia"
    if 810 <= minute_of_day < 1_200:
        return "us_rth"
    return "us_extended"


@dataclasses.dataclass(frozen=True)
class ArcusHedgeTurnoverConfig:
    """P0 configuration for Arcus passive maker + cross-venue taker hedge turnover."""

    name: str = "m2_default"
    symbol: str = "NVDA"
    tick_size: float = 0.01
    lot_size: float = 0.0001
    min_notional_usd: float = 10.0
    clip_usd: float = 100.0
    max_inventory_usd: float = 300.0
    entry_edge_bps: float = 0.0
    skew_bps_per_clip: float = 0.5
    nonlinear_skew_power: float = 1.5
    lead_beta: float = 0.5
    use_microprice: bool = True
    fair_basis_tau_ms: float = 1_800_000.0
    max_gap_ms: int = DEFAULT_MAX_GAP_MS
    max_book_age_ms: int = DEFAULT_MAX_BOOK_AGE_MS
    queue_ahead_fraction: float = 0.5
    submit_rtt_ms: int = 100
    cancel_ms: int = 150
    hedge_delay_ms: int = 330
    orphan_retry_delay_ms: int = 330
    max_orphan_retries: int = 5
    max_l1_depth_fraction: float = 1.0
    keep_queue_ticks: int = 1
    improve_one_tick: bool = False
    maker_fee_bps: float = 0.0
    hedge_taker_fee_bps: float = 0.0
    fx_drag_bps: float = 0.0
    toxic_mode: str = "none"  # "none", "strict_through", "opposite_touch"
    toxic_fill_frac: float = 0.0
    warmup_ms: int = DEFAULT_WARMUP_MS
    terminal_drain_ms: int = DEFAULT_TERMINAL_DRAIN_MS
    declared_end_ms: int | None = None

    def __post_init__(self) -> None:
        if self.tick_size <= 0.0 or not math.isfinite(self.tick_size):
            raise ValueError(f"tick_size must be > 0, got {self.tick_size}")
        if self.lot_size <= 0.0 or not math.isfinite(self.lot_size):
            raise ValueError(f"lot_size must be > 0, got {self.lot_size}")
        if self.hedge_delay_ms <= 0 or self.orphan_retry_delay_ms <= 0:
            raise ValueError("hedge_delay_ms and orphan_retry_delay_ms must be > 0 (never 0 ms)")
        if not (0.0 <= self.queue_ahead_fraction <= 1.0):
            raise ValueError(f"queue_ahead_fraction must be in [0, 1], got {self.queue_ahead_fraction}")
        if not (0.0 < self.max_l1_depth_fraction <= 1.0):
            raise ValueError(f"max_l1_depth_fraction must be in (0, 1], got {self.max_l1_depth_fraction}")
        if self.toxic_mode not in ("none", "strict_through", "opposite_touch"):
            raise ValueError(f"Unsupported toxic_mode {self.toxic_mode!r}")
        if not (0.0 <= self.toxic_fill_frac <= 1.0):
            raise ValueError(f"toxic_fill_frac must be in [0, 1], got {self.toxic_fill_frac}")


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
    cancel_requested_ms: int | None = None
    cancel_effective_ms: int | None = None
    toxic_cross_handled: bool = False


@dataclasses.dataclass
class _PendingHedge:
    exec_at_ms: int
    hedge_side: str  # "SELL" or "BUY"
    sgn: int  # +1 => Arcus bought (sell hedge); -1 => Arcus sold (buy hedge)
    rem_qty: float
    qp: float
    a_mid_f: float
    fill_ms: int
    session: str
    attempt: int = 0


def _round_tick(px: float, tick: float, *, up: bool) -> float:
    units = px / tick
    return (math.ceil(units - 1e-9) if up else math.floor(units + 1e-9)) * tick


def _quantize_qty(qty: float, lot: float) -> float:
    if qty <= 0.0 or lot <= 0.0:
        return 0.0
    return math.floor((qty / lot) + 1e-9) * lot


def _microprice(bid: float, bsz: float, ask: float, asz: float) -> float:
    tot = bsz + asz
    if tot > 0.0 and math.isfinite(tot) and bsz > 0.0 and asz > 0.0:
        return (bid * asz + ask * bsz) / tot
    return 0.5 * (bid + ask)


def normalize_events(events: Sequence[dict[str, Any]]) -> list[tuple[int, int, Any, float, float, float]]:
    """Normalize dict events into `(ts_ms, kind, c2, c3, c4, c5)` sorted deterministically.

    Kind order at identical timestamps:
      0 = `arcus_book`, 1 = `hedge_book`, 2 = `arcus_trade`.
    """
    tuples: list[tuple[int, int, Any, float, float, float]] = []
    for ev in events:
        kind = ev.get("kind")
        ts = int(ev["ts"])
        if kind == "arcus_book":
            bb, bsz = float(ev["bid"]), float(ev["bsz"])
            ba, asz = float(ev["ask"]), float(ev["asz"])
            if bb > 0.0 and ba > bb and bsz > 0.0 and asz > 0.0 and math.isfinite(bb + ba + bsz + asz):
                tuples.append((ts, 0, bb, bsz, ba, asz))
        elif kind == "hedge_book":
            hb, hbsz = float(ev["bid"]), float(ev["bsz"])
            ha, hasz = float(ev["ask"]), float(ev["asz"])
            if hb > 0.0 and ha > hb and hbsz > 0.0 and hasz > 0.0 and math.isfinite(hb + ha + hbsz + hasz):
                tuples.append((ts, 1, hb, hbsz, ha, hasz))
        elif kind == "arcus_trade":
            px, sz = float(ev["price"]), float(ev["size"])
            s_up = str(ev["side"]).upper()
            side = "BUY" if s_up in ("BUY", "B", "BID") else "SELL"
            if px > 0.0 and sz > 0.0 and math.isfinite(px + sz):
                tuples.append((ts, 2, side, px, sz, 0.0))
        else:
            raise ValueError(f"Unknown event kind: {kind!r}")
    tuples.sort(key=lambda t: (t[0], t[1]))
    return tuples


def simulate_arcus_hedge_turnover(
    events: Sequence[dict[str, Any]] | Sequence[tuple[int, int, Any, float, float, float]],
    cfg: ArcusHedgeTurnoverConfig,
    *,
    already_normalized: bool = False,
) -> dict[str, Any]:
    """Run causal P0 event-driven Arcus-Maker x Cross-Venue-Taker-Hedge simulation."""
    tuples: list[tuple[int, int, Any, float, float, float]]
    if already_normalized:
        tuples = list(events)  # type: ignore[arg-type]
    else:
        tuples = normalize_events(events)  # type: ignore[arg-type]

    if not tuples:
        return _empty_result(cfg)

    declared_end_ms = cfg.declared_end_ms
    if declared_end_ms is not None:
        drain_start_ms = (
            declared_end_ms - cfg.terminal_drain_ms if cfg.terminal_drain_ms > 0 else declared_end_ms + 1
        )
    else:
        last_ts = tuples[-1][0]
        drain_start_ms = (
            last_ts - cfg.terminal_drain_ms if cfg.terminal_drain_ms > 0 else last_ts + 1
        )

    has_arcus = False
    cur_abb = 0.0
    cur_absz = 0.0
    cur_aba = 0.0
    cur_aasz = 0.0
    last_arcus_ts: int | None = None

    has_hedge = False
    cur_hb = 0.0
    cur_hbsz = 0.0
    cur_ha = 0.0
    cur_hasz = 0.0
    rem_hb_sz = 0.0
    rem_ha_sz = 0.0
    last_hedge_ts: int | None = None

    first_pair_ts: int | None = None
    last_basis_ts: int | None = None
    ewma_fair: float | None = None
    prior_fair: float | None = None

    bid_ord: _RestingOrder | None = None
    ask_ord: _RestingOrder | None = None
    pending_hedges: list[_PendingHedge] = []
    unhedged_orphans: list[tuple[int, float, float, float, str]] = []  # (sgn, qty, qp, a_mid_f, session)

    arcus_pos_qty = 0.0
    hedge_pos_qty = 0.0
    arcus_cash_usd = 0.0
    hedge_cash_usd = 0.0
    arcus_volume_usd = 0.0
    hedge_volume_usd = 0.0
    hedged_arcus_notional_usd = 0.0

    arcus_spread_capture = 0.0
    arcus_adverse = 0.0
    rh_hedge_slip = 0.0
    fee_cost = 0.0
    fx_drag = 0.0

    arcus_fills = 0
    hedge_fills = 0
    orders_placed = 0
    cancels_sent = 0
    alo_rejects = 0
    cancel_in_flight_fills = 0
    unprinted_toxic_fills = 0
    unprinted_toxic_notional_usd = 0.0
    orphan_events = 0
    orphan_triggered_notional_usd = 0.0
    orphan_retries = 0
    depth_clipped_hedges = 0
    stale_hedge_blocks = 0
    peak_arcus_gross_usd = 0.0

    by_session: dict[str, dict[str, float]] = {}

    def _sess_bucket(sess: str) -> dict[str, float]:
        b = by_session.get(sess)
        if b is None:
            b = {
                "arcus_fills": 0.0,
                "arcus_volume_usd": 0.0,
                "hedge_volume_usd": 0.0,
                "arcus_spread_capture": 0.0,
                "arcus_adverse": 0.0,
                "rh_hedge_slip": 0.0,
                "fee_cost": 0.0,
                "fx_drag": 0.0,
            }
            by_session[sess] = b
        return b

    tol_tick_25 = 0.25 * cfg.tick_size
    tol_tick_10 = 0.10 * cfg.tick_size
    exp_fn = math.exp
    log_fn = math.log

    def arcus_fresh(now_ms: int) -> bool:
        return has_arcus and last_arcus_ts is not None and 0 <= (now_ms - last_arcus_ts) <= cfg.max_book_age_ms

    def hedge_fresh(now_ms: int) -> bool:
        return has_hedge and last_hedge_ts is not None and 0 <= (now_ms - last_hedge_ts) <= cfg.max_book_age_ms

    def update_basis(now_ms: int) -> None:
        nonlocal ewma_fair, prior_fair, last_basis_ts, first_pair_ts
        if not (arcus_fresh(now_ms) and hedge_fresh(now_ms)):
            return
        if first_pair_ts is None:
            first_pair_ts = now_ms
        if cfg.use_microprice:
            ma = _microprice(cur_abb, cur_absz, cur_aba, cur_aasz)
            ha = _microprice(cur_hb, cur_hbsz, cur_ha, cur_hasz)
        else:
            ma = 0.5 * (cur_abb + cur_aba)
            ha = 0.5 * (cur_hb + cur_ha)
        b_now = log_fn(ma) - log_fn(ha)
        if ewma_fair is not None and last_basis_ts is not None and 0 <= (now_ms - last_basis_ts) <= cfg.max_gap_ms:
            prior_fair = ewma_fair
            dt = now_ms - last_basis_ts
            if dt > 0:
                alpha = 1.0 - exp_fn(-dt / max(cfg.fair_basis_tau_ms, 1.0))
                ewma_fair = ewma_fair + alpha * (b_now - ewma_fair)
        else:
            ewma_fair = b_now
            prior_fair = b_now
        last_basis_ts = now_ms

    def cancel_side(now_ms: int, ord_obj: _RestingOrder | None) -> None:
        nonlocal cancels_sent
        if ord_obj is not None and ord_obj.cancel_effective_ms is None:
            ord_obj.cancel_requested_ms = now_ms
            ord_obj.cancel_effective_ms = now_ms + cfg.cancel_ms
            cancels_sent += 1

    def expire_cancels(now_ms: int) -> None:
        nonlocal bid_ord, ask_ord
        if bid_ord is not None and bid_ord.cancel_effective_ms is not None and now_ms >= bid_ord.cancel_effective_ms:
            bid_ord = None
        if ask_ord is not None and ask_ord.cancel_effective_ms is not None and now_ms >= ask_ord.cancel_effective_ms:
            ask_ord = None

    def activate_arrivals(now_ms: int) -> None:
        nonlocal bid_ord, ask_ord, alo_rejects
        if not arcus_fresh(now_ms):
            return
        if bid_ord is not None and not bid_ord.activated and now_ms >= bid_ord.active_at_ms:
            if bid_ord.price >= cur_aba - 1e-12:
                bid_ord = None
                alo_rejects += 1
            else:
                if abs(bid_ord.price - cur_abb) <= tol_tick_25:
                    bid_ord.queue_ahead = cfg.queue_ahead_fraction * cur_absz
                elif bid_ord.price > cur_abb + tol_tick_25:
                    bid_ord.queue_ahead = 0.0
                else:
                    bid_ord.queue_ahead = cur_absz
                bid_ord.activated = True
        if ask_ord is not None and not ask_ord.activated and now_ms >= ask_ord.active_at_ms:
            if ask_ord.price <= cur_abb + 1e-12:
                ask_ord = None
                alo_rejects += 1
            else:
                if abs(ask_ord.price - cur_aba) <= tol_tick_25:
                    ask_ord.queue_ahead = cfg.queue_ahead_fraction * cur_aasz
                elif ask_ord.price < cur_aba - tol_tick_25:
                    ask_ord.queue_ahead = 0.0
                else:
                    ask_ord.queue_ahead = cur_aasz
                ask_ord.activated = True

    def record_arcus_fill(
        ts_ms: int,
        sgn: int,
        qty: float,
        qp: float,
        *,
        was_cif: bool,
        is_unprinted_toxic: bool,
    ) -> None:
        nonlocal arcus_fills, arcus_volume_usd, arcus_cash_usd, arcus_pos_qty, fee_cost
        nonlocal cancel_in_flight_fills, unprinted_toxic_fills, unprinted_toxic_notional_usd
        nonlocal peak_arcus_gross_usd
        if qty <= 1e-12:
            return
        notl = qty * qp
        arcus_fills += 1
        arcus_volume_usd += notl
        arcus_cash_usd -= sgn * notl
        arcus_pos_qty += sgn * qty
        mk_f = notl * cfg.maker_fee_bps * 1e-4
        fee_cost -= mk_f

        a_mid_f = 0.5 * (cur_abb + cur_aba)
        gross_now = abs(arcus_pos_qty) * a_mid_f
        if gross_now > peak_arcus_gross_usd:
            peak_arcus_gross_usd = gross_now

        if was_cif:
            cancel_in_flight_fills += 1
        if is_unprinted_toxic:
            unprinted_toxic_fills += 1
            unprinted_toxic_notional_usd += notl

        sess = classify_utc_session(ts_ms)
        sb = _sess_bucket(sess)
        sb["arcus_fills"] += 1.0
        sb["arcus_volume_usd"] += notl
        sb["fee_cost"] -= mk_f

        pending_hedges.append(
            _PendingHedge(
                exec_at_ms=ts_ms + cfg.hedge_delay_ms,
                hedge_side="SELL" if sgn == 1 else "BUY",
                sgn=sgn,
                rem_qty=qty,
                qp=qp,
                a_mid_f=a_mid_f,
                fill_ms=ts_ms,
                session=sess,
                attempt=0,
            )
        )

    def process_pending_hedges(now_ms: int) -> None:
        nonlocal rem_hb_sz, rem_ha_sz, hedge_fills, hedge_volume_usd, hedge_cash_usd, hedge_pos_qty
        nonlocal hedged_arcus_notional_usd, arcus_spread_capture, arcus_adverse, rh_hedge_slip
        nonlocal fee_cost, fx_drag, orphan_events, orphan_triggered_notional_usd, orphan_retries
        nonlocal depth_clipped_hedges, stale_hedge_blocks, pending_hedges
        if not pending_hedges:
            return
        still: list[_PendingHedge] = []
        a_mid_now = 0.5 * (cur_abb + cur_aba) if has_arcus else 0.0
        for ph in pending_hedges:
            if now_ms < ph.exec_at_ms:
                still.append(ph)
                continue
            if not hedge_fresh(now_ms) or not has_arcus:
                stale_hedge_blocks += 1
                if ph.attempt < cfg.max_orphan_retries:
                    orphan_retries += 1
                    ph.attempt += 1
                    ph.exec_at_ms = now_ms + cfg.orphan_retry_delay_ms
                    still.append(ph)
                else:
                    unhedged_orphans.append((ph.sgn, ph.rem_qty, ph.qp, ph.a_mid_f, ph.session))
                continue

            if ph.hedge_side == "SELL":
                avail = max(0.0, rem_hb_sz)
                hp = cur_hb
            else:
                avail = max(0.0, rem_ha_sz)
                hp = cur_ha

            exec_qty = min(ph.rem_qty, avail)
            if exec_qty > 1e-12:
                if ph.hedge_side == "SELL":
                    rem_hb_sz = max(0.0, rem_hb_sz - exec_qty)
                else:
                    rem_ha_sz = max(0.0, rem_ha_sz - exec_qty)

                h_notl = exec_qty * hp
                a_notl = exec_qty * ph.qp
                hedge_fills += 1
                hedge_volume_usd += h_notl
                hedged_arcus_notional_usd += a_notl
                hedge_cash_usd += ph.sgn * h_notl
                hedge_pos_qty -= ph.sgn * exec_qty

                hd_f = h_notl * cfg.hedge_taker_fee_bps * 1e-4
                fx_c = a_notl * cfg.fx_drag_bps * 1e-4
                fee_cost -= hd_f
                fx_drag -= fx_c

                sc = ph.sgn * exec_qty * (ph.a_mid_f - ph.qp)
                adv = ph.sgn * exec_qty * (a_mid_now - ph.a_mid_f)
                hslip = ph.sgn * exec_qty * (hp - a_mid_now)
                arcus_spread_capture += sc
                arcus_adverse += adv
                rh_hedge_slip += hslip

                sb = _sess_bucket(ph.session)
                sb["hedge_volume_usd"] += h_notl
                sb["arcus_spread_capture"] += sc
                sb["arcus_adverse"] += adv
                sb["rh_hedge_slip"] += hslip
                sb["fee_cost"] -= hd_f
                sb["fx_drag"] -= fx_c

            rem = ph.rem_qty - exec_qty
            if rem > 1e-12:
                depth_clipped_hedges += 1
                if ph.attempt == 0:
                    orphan_events += 1
                    orphan_triggered_notional_usd += rem * ph.qp
                if ph.attempt < cfg.max_orphan_retries:
                    orphan_retries += 1
                    ph.rem_qty = rem
                    ph.attempt += 1
                    ph.exec_at_ms = now_ms + cfg.orphan_retry_delay_ms
                    still.append(ph)
                else:
                    unhedged_orphans.append((ph.sgn, rem, ph.qp, ph.a_mid_f, ph.session))
        pending_hedges = still

    def decide(now_ms: int) -> None:
        nonlocal bid_ord, ask_ord, orders_placed
        if not (arcus_fresh(now_ms) and hedge_fresh(now_ms)):
            cancel_side(now_ms, bid_ord)
            cancel_side(now_ms, ask_ord)
            return
        if prior_fair is None or first_pair_ts is None or (now_ms - first_pair_ts) < cfg.warmup_ms:
            return

        if cfg.use_microprice:
            ma = _microprice(cur_abb, cur_absz, cur_aba, cur_aasz)
            ha = _microprice(cur_hb, cur_hbsz, cur_ha, cur_hasz)
        else:
            ma = 0.5 * (cur_abb + cur_aba)
            ha = 0.5 * (cur_hb + cur_ha)
        b_now = log_fn(ma) - log_fn(ha)
        shift = exp_fn(cfg.lead_beta * (b_now - prior_fair))
        hb_eff = cur_hb * shift
        ha_eff = cur_ha * shift

        a_mid = 0.5 * (cur_abb + cur_aba)
        pos_usd = arcus_pos_qty * a_mid
        max_inv = max(cfg.max_inventory_usd, cfg.clip_usd)
        q = pos_usd / max(cfg.clip_usd, 1e-9)
        q_max = max_inv / max(cfg.clip_usd, 1e-9)
        if cfg.nonlinear_skew_power > 1.0 and q != 0.0 and q_max > 0.0:
            u = min(abs(q) / q_max, 1.0)
            sk_q = q * (1.0 + (cfg.nonlinear_skew_power - 1.0) * (u ** cfg.nonlinear_skew_power))
        else:
            sk_q = q

        in_drain = now_ms >= drain_start_ms
        sp_ticks = max(1, round((cur_aba - cur_abb) / cfg.tick_size))
        can_imp = cfg.improve_one_tick and sp_ticks >= 2

        # --- BID side ---
        is_red_bid = pos_usd < -cfg.min_notional_usd
        if in_drain and not is_red_bid:
            cancel_side(now_ms, bid_ord)
        elif pos_usd >= max_inv:
            cancel_side(now_ms, bid_ord)
        else:
            e_bid = cfg.entry_edge_bps + cfg.skew_bps_per_clip * sk_q
            cap_bid = hb_eff * exp_fn(prior_fair - e_bid * 1e-4)
            join_bid = (cur_abb + cfg.tick_size) if can_imp else cur_abb
            px_b = _round_tick(min(join_bid, cap_bid), cfg.tick_size, up=False)
            if px_b >= cur_aba - 1e-12:
                px_b = _round_tick(cur_aba - cfg.tick_size, cfg.tick_size, up=False)
            if px_b <= 0.0 or px_b >= cur_aba - 1e-12:
                cancel_side(now_ms, bid_ord)
            elif bid_ord is not None:
                if bid_ord.cancel_effective_ms is None:
                    tdiff = round(abs(bid_ord.price - px_b) / cfg.tick_size)
                    if bid_ord.price >= cur_aba - 1e-12 or bid_ord.price > cap_bid + tol_tick_25 or tdiff > cfg.keep_queue_ticks:
                        cancel_side(now_ms, bid_ord)
            else:
                target_usd = min(abs(pos_usd), cfg.clip_usd) if is_red_bid else cfg.clip_usd
                q_b = _quantize_qty(target_usd / px_b, cfg.lot_size)
                if q_b * px_b >= cfg.min_notional_usd:
                    bid_ord = _RestingOrder(
                        side="BID",
                        price=px_b,
                        qty=q_b,
                        remaining=q_b,
                        queue_ahead=0.0,
                        placed_at_ms=now_ms,
                        active_at_ms=now_ms + cfg.submit_rtt_ms,
                    )
                    orders_placed += 1

        # --- ASK side ---
        is_red_ask = pos_usd > cfg.min_notional_usd
        if in_drain and not is_red_ask:
            cancel_side(now_ms, ask_ord)
        elif pos_usd <= -max_inv:
            cancel_side(now_ms, ask_ord)
        else:
            e_ask = cfg.entry_edge_bps - cfg.skew_bps_per_clip * sk_q
            floor_ask = ha_eff * exp_fn(prior_fair + e_ask * 1e-4)
            join_ask = (cur_aba - cfg.tick_size) if can_imp else cur_aba
            px_a = _round_tick(max(join_ask, floor_ask), cfg.tick_size, up=True)
            if px_a <= cur_abb + 1e-12:
                px_a = _round_tick(cur_abb + cfg.tick_size, cfg.tick_size, up=True)
            if px_a <= 0.0 or px_a <= cur_abb + 1e-12:
                cancel_side(now_ms, ask_ord)
            elif ask_ord is not None:
                if ask_ord.cancel_effective_ms is None:
                    tdiff = round(abs(ask_ord.price - px_a) / cfg.tick_size)
                    if ask_ord.price <= cur_abb + 1e-12 or ask_ord.price < floor_ask - tol_tick_25 or tdiff > cfg.keep_queue_ticks:
                        cancel_side(now_ms, ask_ord)
            else:
                target_usd = min(abs(pos_usd), cfg.clip_usd) if is_red_ask else cfg.clip_usd
                q_a = _quantize_qty(target_usd / px_a, cfg.lot_size)
                if q_a * px_a >= cfg.min_notional_usd:
                    ask_ord = _RestingOrder(
                        side="ASK",
                        price=px_a,
                        qty=q_a,
                        remaining=q_a,
                        queue_ahead=0.0,
                        placed_at_ms=now_ms,
                        active_at_ms=now_ms + cfg.submit_rtt_ms,
                    )
                    orders_placed += 1

    for ts, kind, c2, c3, c4, c5 in tuples:
        if declared_end_ms is not None and ts > declared_end_ms:
            break
        expire_cancels(ts)
        activate_arrivals(ts)
        process_pending_hedges(ts)

        if kind == 0:
            cur_abb, cur_absz, cur_aba, cur_aasz = float(c2), float(c3), float(c4), float(c5)
            has_arcus = True
            last_arcus_ts = ts
            activate_arrivals(ts)
            if bid_ord is not None and bid_ord.activated and abs(bid_ord.price - cur_abb) <= tol_tick_25:
                if cur_absz < bid_ord.queue_ahead:
                    bid_ord.queue_ahead = cur_absz
            if ask_ord is not None and ask_ord.activated and abs(ask_ord.price - cur_aba) <= tol_tick_25:
                if cur_aasz < ask_ord.queue_ahead:
                    ask_ord.queue_ahead = cur_aasz

            if cfg.toxic_mode != "none" and cfg.toxic_fill_frac > 0.0:
                if bid_ord is not None and bid_ord.activated and not bid_ord.toxic_cross_handled:
                    crossed = (
                        cur_aba < bid_ord.price - tol_tick_10
                        if cfg.toxic_mode == "strict_through"
                        else cur_aba <= bid_ord.price + tol_tick_10
                    )
                    if crossed:
                        bid_ord.toxic_cross_handled = True
                        f_qty = bid_ord.remaining * cfg.toxic_fill_frac
                        if f_qty > 1e-12:
                            cif = bid_ord.cancel_effective_ms is not None and ts < bid_ord.cancel_effective_ms
                            record_arcus_fill(ts, 1, f_qty, bid_ord.price, was_cif=cif, is_unprinted_toxic=True)
                            bid_ord.remaining -= f_qty
                        if bid_ord.remaining <= 1e-12:
                            bid_ord = None
                if ask_ord is not None and ask_ord.activated and not ask_ord.toxic_cross_handled:
                    crossed = (
                        cur_abb > ask_ord.price + tol_tick_10
                        if cfg.toxic_mode == "strict_through"
                        else cur_abb >= ask_ord.price - tol_tick_10
                    )
                    if crossed:
                        ask_ord.toxic_cross_handled = True
                        f_qty = ask_ord.remaining * cfg.toxic_fill_frac
                        if f_qty > 1e-12:
                            cif = ask_ord.cancel_effective_ms is not None and ts < ask_ord.cancel_effective_ms
                            record_arcus_fill(ts, -1, f_qty, ask_ord.price, was_cif=cif, is_unprinted_toxic=True)
                            ask_ord.remaining -= f_qty
                        if ask_ord.remaining <= 1e-12:
                            ask_ord = None

            update_basis(ts)
            decide(ts)

        elif kind == 1:
            cur_hb, cur_hbsz, cur_ha, cur_hasz = float(c2), float(c3), float(c4), float(c5)
            has_hedge = True
            last_hedge_ts = ts
            rem_hb_sz = cur_hbsz * cfg.max_l1_depth_fraction
            rem_ha_sz = cur_hasz * cfg.max_l1_depth_fraction
            update_basis(ts)
            decide(ts)

        else:
            if not arcus_fresh(ts):
                continue
            tr_side = str(c2)
            tr_px = float(c3)
            tr_sz = float(c4)
            if tr_side == "BUY" and ask_ord is not None and ask_ord.activated:
                f_qty = 0.0
                if tr_px > ask_ord.price + tol_tick_10:
                    f_qty = min(ask_ord.remaining, tr_sz)
                elif abs(tr_px - ask_ord.price) <= tol_tick_10:
                    if tr_sz > ask_ord.queue_ahead:
                        overflow = tr_sz - ask_ord.queue_ahead
                        ask_ord.queue_ahead = 0.0
                        f_qty = min(ask_ord.remaining, overflow)
                    else:
                        ask_ord.queue_ahead -= tr_sz
                if f_qty > 1e-12:
                    cif = ask_ord.cancel_effective_ms is not None and ts < ask_ord.cancel_effective_ms
                    record_arcus_fill(ts, -1, f_qty, ask_ord.price, was_cif=cif, is_unprinted_toxic=False)
                    ask_ord.remaining -= f_qty
                    if ask_ord.remaining <= 1e-12:
                        ask_ord = None
                    decide(ts)
            elif tr_side == "SELL" and bid_ord is not None and bid_ord.activated:
                f_qty = 0.0
                if tr_px < bid_ord.price - tol_tick_10:
                    f_qty = min(bid_ord.remaining, tr_sz)
                elif abs(tr_px - bid_ord.price) <= tol_tick_10:
                    if tr_sz > bid_ord.queue_ahead:
                        overflow = tr_sz - bid_ord.queue_ahead
                        bid_ord.queue_ahead = 0.0
                        f_qty = min(bid_ord.remaining, overflow)
                    else:
                        bid_ord.queue_ahead -= tr_sz
                if f_qty > 1e-12:
                    cif = bid_ord.cancel_effective_ms is not None and ts < bid_ord.cancel_effective_ms
                    record_arcus_fill(ts, 1, f_qty, bid_ord.price, was_cif=cif, is_unprinted_toxic=False)
                    bid_ord.remaining -= f_qty
                    if bid_ord.remaining <= 1e-12:
                        bid_ord = None
                    decide(ts)

    # Terminal reconciliation at declared_end_ms (NEVER 0ms flatten with infinite depth!)
    a_mid_end = 0.5 * (cur_abb + cur_aba) if has_arcus else 0.0
    h_mid_end = 0.5 * (cur_hb + cur_ha) if has_hedge else 0.0

    for ph in pending_hedges:
        unhedged_orphans.append((ph.sgn, ph.rem_qty, ph.qp, ph.a_mid_f, ph.session))
    for sgn, uqty, qp, a_mid_f, sess in unhedged_orphans:
        sc = sgn * uqty * (a_mid_f - qp)
        adv = sgn * uqty * (a_mid_end - a_mid_f)
        arcus_spread_capture += sc
        arcus_adverse += adv
        sb = _sess_bucket(sess)
        sb["arcus_spread_capture"] += sc
        sb["arcus_adverse"] += adv

    residual_drain = (-hedge_pos_qty) * (a_mid_end - h_mid_end)
    terminal_residual_notional_usd = abs(arcus_pos_qty) * a_mid_end
    terminal_unhedged_notional_usd = abs(arcus_pos_qty + hedge_pos_qty) * a_mid_end
    terminal_half_spread_unwind_usd = 0.0
    if has_arcus and abs(arcus_pos_qty) > 1e-12:
        terminal_half_spread_unwind_usd += abs(arcus_pos_qty) * 0.5 * (cur_aba - cur_abb)
    if has_hedge and abs(hedge_pos_qty) > 1e-12:
        terminal_half_spread_unwind_usd += abs(hedge_pos_qty) * 0.5 * (cur_ha - cur_hb)

    total_net_pnl_usd = (
        arcus_cash_usd + hedge_cash_usd + arcus_pos_qty * a_mid_end + hedge_pos_qty * h_mid_end + fee_cost + fx_drag
    )
    decomp_sum = (
        arcus_spread_capture
        + arcus_adverse
        + rh_hedge_slip
        + fee_cost
        + fx_drag
        + residual_drain
    )
    conservation_error_usd = abs(decomp_sum - total_net_pnl_usd)
    if conservation_error_usd > 1e-6:
        raise AssertionError(
            f"PnL conservation violation: decomp={decomp_sum} total={total_net_pnl_usd} err={conservation_error_usd}"
        )

    combined_volume_usd = arcus_volume_usd + hedge_volume_usd
    arcus_net_usd_per_m = (total_net_pnl_usd / arcus_volume_usd * USD_PER_MILLION) if arcus_volume_usd > 0.0 else 0.0
    combined_net_usd_per_m = (
        total_net_pnl_usd / combined_volume_usd * USD_PER_MILLION if combined_volume_usd > 0.0 else 0.0
    )

    return {
        "name": cfg.name,
        "symbol": cfg.symbol,
        "entry_edge_bps": cfg.entry_edge_bps,
        "arcus_fills": arcus_fills,
        "hedge_fills": hedge_fills,
        "orders_placed": orders_placed,
        "cancels_sent": cancels_sent,
        "alo_rejects": alo_rejects,
        "cancel_in_flight_fills": cancel_in_flight_fills,
        "unprinted_toxic_fills": unprinted_toxic_fills,
        "unprinted_toxic_notional_usd": round(unprinted_toxic_notional_usd, 4),
        "arcus_volume_usd": round(arcus_volume_usd, 4),
        "hedge_volume_usd": round(hedge_volume_usd, 4),
        "combined_volume_usd": round(combined_volume_usd, 4),
        "hedged_arcus_notional_usd": round(hedged_arcus_notional_usd, 4),
        "orphan_events": orphan_events,
        "orphan_triggered_notional_usd": round(orphan_triggered_notional_usd, 4),
        "orphan_rate": round(orphan_triggered_notional_usd / arcus_volume_usd, 6) if arcus_volume_usd > 0.0 else 0.0,
        "orphan_retries": orphan_retries,
        "depth_clipped_hedges": depth_clipped_hedges,
        "stale_hedge_blocks": stale_hedge_blocks,
        "terminal_pending_hedges": len(pending_hedges),
        "terminal_unhedged_orphans": len(unhedged_orphans),
        "terminal_residual_notional_usd": round(terminal_residual_notional_usd, 4),
        "terminal_unhedged_notional_usd": round(terminal_unhedged_notional_usd, 4),
        "terminal_half_spread_unwind_usd": round(terminal_half_spread_unwind_usd, 6),
        "peak_arcus_gross_usd": round(peak_arcus_gross_usd, 4),
        "arcus_spread_capture": round(arcus_spread_capture, 6),
        "arcus_adverse": round(arcus_adverse, 6),
        "rh_hedge_slip": round(rh_hedge_slip, 6),
        "fee_cost": round(fee_cost, 6),
        "fx_drag": round(fx_drag, 6),
        "residual_drain": round(residual_drain, 6),
        "total_net_pnl_usd": round(total_net_pnl_usd, 6),
        "conservation_error_usd": round(conservation_error_usd, 10),
        "arcus_single_sided_net_usd_per_m": round(arcus_net_usd_per_m, 4),
        "arcus_single_sided_cost_usd_per_m": round(-arcus_net_usd_per_m, 4),
        "combined_two_leg_net_usd_per_m": round(combined_net_usd_per_m, 4),
        "combined_two_leg_cost_usd_per_m": round(-combined_net_usd_per_m, 4),
        "by_session": {
            k: {sk: round(sv, 6) for sk, sv in v.items()} for k, v in sorted(by_session.items())
        },
    }


def _empty_result(cfg: ArcusHedgeTurnoverConfig) -> dict[str, Any]:
    return {
        "name": cfg.name,
        "symbol": cfg.symbol,
        "entry_edge_bps": cfg.entry_edge_bps,
        "arcus_fills": 0,
        "hedge_fills": 0,
        "orders_placed": 0,
        "cancels_sent": 0,
        "alo_rejects": 0,
        "cancel_in_flight_fills": 0,
        "unprinted_toxic_fills": 0,
        "unprinted_toxic_notional_usd": 0.0,
        "arcus_volume_usd": 0.0,
        "hedge_volume_usd": 0.0,
        "combined_volume_usd": 0.0,
        "hedged_arcus_notional_usd": 0.0,
        "orphan_events": 0,
        "orphan_triggered_notional_usd": 0.0,
        "orphan_rate": 0.0,
        "orphan_retries": 0,
        "depth_clipped_hedges": 0,
        "stale_hedge_blocks": 0,
        "terminal_pending_hedges": 0,
        "terminal_unhedged_orphans": 0,
        "terminal_residual_notional_usd": 0.0,
        "terminal_unhedged_notional_usd": 0.0,
        "terminal_half_spread_unwind_usd": 0.0,
        "peak_arcus_gross_usd": 0.0,
        "arcus_spread_capture": 0.0,
        "arcus_adverse": 0.0,
        "rh_hedge_slip": 0.0,
        "fee_cost": 0.0,
        "fx_drag": 0.0,
        "residual_drain": 0.0,
        "total_net_pnl_usd": 0.0,
        "conservation_error_usd": 0.0,
        "arcus_single_sided_net_usd_per_m": 0.0,
        "arcus_single_sided_cost_usd_per_m": 0.0,
        "combined_two_leg_net_usd_per_m": 0.0,
        "combined_two_leg_cost_usd_per_m": 0.0,
        "by_session": {},
    }


def clone_result_with_fx(res_0bp: dict[str, Any], fx_bps: float) -> dict[str, Any]:
    """Derive exact `fx_bps` metrics analytically from a `0.0 bps` run."""
    out = dict(res_0bp)
    if fx_bps <= 0.0:
        return out
    hedged_a = float(res_0bp["hedged_arcus_notional_usd"])
    extra_fx = -hedged_a * fx_bps * 1e-4
    new_fx = float(res_0bp["fx_drag"]) + extra_fx
    new_net = float(res_0bp["total_net_pnl_usd"]) + extra_fx
    av = float(res_0bp["arcus_volume_usd"])
    cv = float(res_0bp["combined_volume_usd"])
    a_net_pm = (new_net / av * USD_PER_MILLION) if av > 0.0 else 0.0
    c_net_pm = (new_net / cv * USD_PER_MILLION) if cv > 0.0 else 0.0
    out["fx_drag"] = round(new_fx, 6)
    out["total_net_pnl_usd"] = round(new_net, 6)
    out["arcus_single_sided_net_usd_per_m"] = round(a_net_pm, 4)
    out["arcus_single_sided_cost_usd_per_m"] = round(-a_net_pm, 4)
    out["combined_two_leg_net_usd_per_m"] = round(c_net_pm, 4)
    out["combined_two_leg_cost_usd_per_m"] = round(-c_net_pm, 4)
    sess_out: dict[str, dict[str, float]] = {}
    for sk, sv in (res_0bp.get("by_session") or {}).items():
        sc = dict(sv)
        sc["fx_drag"] = round(float(sv["fx_drag"]) - float(sv["arcus_volume_usd"]) * fx_bps * 1e-4, 6)
        sess_out[sk] = sc
    out["by_session"] = sess_out
    return out


def aggregate_runs(runs: Sequence[dict[str, Any]]) -> dict[str, Any]:
    """Aggregate per-(window, symbol) runs into summary metrics and drawdown."""
    if not runs:
        return {
            "runs": 0,
            "windows_covered": 0,
            "arcus_fills": 0,
            "hedge_fills": 0,
            "arcus_volume_usd": 0.0,
            "hedge_volume_usd": 0.0,
            "combined_volume_usd": 0.0,
            "hedged_arcus_notional_usd": 0.0,
            "orphan_events": 0,
            "orphan_rate": 0.0,
            "orphan_retries": 0,
            "depth_clipped_hedges": 0,
            "stale_hedge_blocks": 0,
            "unprinted_toxic_fills": 0,
            "cancel_in_flight_fills": 0,
            "terminal_residual_notional_usd": 0.0,
            "terminal_unhedged_notional_usd": 0.0,
            "arcus_spread_capture": 0.0,
            "arcus_adverse": 0.0,
            "rh_hedge_slip": 0.0,
            "fee_cost": 0.0,
            "fx_drag": 0.0,
            "residual_drain": 0.0,
            "total_net_pnl_usd": 0.0,
            "arcus_single_sided_net_usd_per_m": 0.0,
            "arcus_single_sided_cost_usd_per_m": 0.0,
            "combined_two_leg_net_usd_per_m": 0.0,
            "combined_two_leg_cost_usd_per_m": 0.0,
            "max_drawdown_usd": 0.0,
            "positive_window_ratio": 0.0,
            "by_session": {},
        }

    av = sum(float(r["arcus_volume_usd"]) for r in runs)
    hv = sum(float(r["hedge_volume_usd"]) for r in runs)
    cv = av + hv
    hedged_a = sum(float(r["hedged_arcus_notional_usd"]) for r in runs)
    orp_notl = sum(float(r["orphan_triggered_notional_usd"]) for r in runs)

    sc = sum(float(r["arcus_spread_capture"]) for r in runs)
    adv = sum(float(r["arcus_adverse"]) for r in runs)
    hslip = sum(float(r["rh_hedge_slip"]) for r in runs)
    fee = sum(float(r["fee_cost"]) for r in runs)
    fx = sum(float(r["fx_drag"]) for r in runs)
    res_d = sum(float(r["residual_drain"]) for r in runs)
    net = sum(float(r["total_net_pnl_usd"]) for r in runs)

    a_net_pm = (net / av * USD_PER_MILLION) if av > 0.0 else 0.0
    c_net_pm = (net / cv * USD_PER_MILLION) if cv > 0.0 else 0.0

    by_w: dict[int, float] = {}
    for r in runs:
        wn = int(r.get("window_num", 0))
        by_w[wn] = by_w.get(wn, 0.0) + float(r["total_net_pnl_usd"])
    cum = 0.0
    pk = 0.0
    dd = 0.0
    pos_w = 0
    for wn in sorted(by_w):
        cum += by_w[wn]
        if cum > pk:
            pk = cum
        if pk - cum > dd:
            dd = pk - cum
        if by_w[wn] > 0.0:
            pos_w += 1

    sess_agg: dict[str, dict[str, float]] = {}
    for r in runs:
        for sk, sv in (r.get("by_session") or {}).items():
            dst = sess_agg.setdefault(
                sk,
                {
                    "arcus_fills": 0.0,
                    "arcus_volume_usd": 0.0,
                    "hedge_volume_usd": 0.0,
                    "arcus_spread_capture": 0.0,
                    "arcus_adverse": 0.0,
                    "rh_hedge_slip": 0.0,
                    "fee_cost": 0.0,
                    "fx_drag": 0.0,
                },
            )
            for fk, fv in sv.items():
                dst[fk] = dst.get(fk, 0.0) + float(fv)
    for sk, sv in sess_agg.items():
        sav = sv["arcus_volume_usd"]
        scv = sav + sv["hedge_volume_usd"]
        snet = sv["arcus_spread_capture"] + sv["arcus_adverse"] + sv["rh_hedge_slip"] + sv["fee_cost"] + sv["fx_drag"]
        sv["intraday_net_pnl_usd"] = round(snet, 6)
        sv["arcus_single_sided_cost_usd_per_m"] = round((-snet / sav * USD_PER_MILLION) if sav > 0.0 else 0.0, 4)
        sv["combined_two_leg_cost_usd_per_m"] = round((-snet / scv * USD_PER_MILLION) if scv > 0.0 else 0.0, 4)
        for fk in list(sv.keys()):
            sv[fk] = round(sv[fk], 6)

    return {
        "runs": len(runs),
        "windows_covered": len(by_w),
        "arcus_fills": sum(int(r["arcus_fills"]) for r in runs),
        "hedge_fills": sum(int(r["hedge_fills"]) for r in runs),
        "arcus_volume_usd": round(av, 2),
        "hedge_volume_usd": round(hv, 2),
        "combined_volume_usd": round(cv, 2),
        "hedged_arcus_notional_usd": round(hedged_a, 2),
        "orphan_events": sum(int(r["orphan_events"]) for r in runs),
        "orphan_rate": round(orp_notl / av, 6) if av > 0.0 else 0.0,
        "orphan_retries": sum(int(r["orphan_retries"]) for r in runs),
        "depth_clipped_hedges": sum(int(r["depth_clipped_hedges"]) for r in runs),
        "stale_hedge_blocks": sum(int(r["stale_hedge_blocks"]) for r in runs),
        "unprinted_toxic_fills": sum(int(r["unprinted_toxic_fills"]) for r in runs),
        "cancel_in_flight_fills": sum(int(r["cancel_in_flight_fills"]) for r in runs),
        "terminal_residual_notional_usd": round(sum(float(r["terminal_residual_notional_usd"]) for r in runs), 2),
        "terminal_unhedged_notional_usd": round(sum(float(r["terminal_unhedged_notional_usd"]) for r in runs), 2),
        "arcus_spread_capture": round(sc, 4),
        "arcus_adverse": round(adv, 4),
        "rh_hedge_slip": round(hslip, 4),
        "fee_cost": round(fee, 4),
        "fx_drag": round(fx, 4),
        "residual_drain": round(res_d, 4),
        "total_net_pnl_usd": round(net, 4),
        "arcus_single_sided_net_usd_per_m": round(a_net_pm, 4),
        "arcus_single_sided_cost_usd_per_m": round(-a_net_pm, 4),
        "combined_two_leg_net_usd_per_m": round(c_net_pm, 4),
        "combined_two_leg_cost_usd_per_m": round(-c_net_pm, 4),
        "max_drawdown_usd": round(dd, 4),
        "positive_window_ratio": round(pos_w / len(by_w), 4) if by_w else 0.0,
        "by_session": sess_agg,
    }


def build_synthetic_parity_fixture() -> tuple[ArcusHedgeTurnoverConfig, list[dict[str, Any]], dict[str, Any]]:
    """Return a deterministic 24-event synthetic tape fixture and expected P0 output for Rust parity."""
    t0 = 1_700_000_000_000
    cfg = ArcusHedgeTurnoverConfig(
        name="p0_synthetic_parity_24",
        symbol="NVDA",
        tick_size=0.01,
        lot_size=0.01,
        min_notional_usd=10.0,
        clip_usd=100.0,
        max_inventory_usd=300.0,
        entry_edge_bps=-0.5,
        skew_bps_per_clip=0.0,
        nonlinear_skew_power=1.0,
        lead_beta=0.0,
        use_microprice=False,
        fair_basis_tau_ms=3_600_000.0,
        max_gap_ms=60_000,
        max_book_age_ms=5_000,
        queue_ahead_fraction=0.5,
        submit_rtt_ms=50,
        cancel_ms=100,
        hedge_delay_ms=330,
        orphan_retry_delay_ms=330,
        max_orphan_retries=5,
        max_l1_depth_fraction=1.0,
        keep_queue_ticks=0,
        maker_fee_bps=0.0,
        hedge_taker_fee_bps=0.0,
        fx_drag_bps=0.0,
        toxic_mode="none",
        toxic_fill_frac=0.0,
        warmup_ms=100,
        terminal_drain_ms=500,
        declared_end_ms=t0 + 10_000,
    )
    # 24 deterministic events exercising warmup, FIFO queue, 330ms hedge, single-use L1 depth + orphan retry,
    # stale hedge block (+330ms requeue), cancel-in-flight fill, terminal_drain_ms, and declared_end_ms cutoff.
    events: list[dict[str, Any]] = [
        # 1-2: t0 seed books (warmup starts)
        {"ts": t0 + 0, "kind": "arcus_book", "bid": 99.99, "bsz": 4.0, "ask": 100.01, "asz": 4.0},
        {"ts": t0 + 0, "kind": "hedge_book", "bid": 99.99, "bsz": 4.0, "ask": 100.01, "asz": 4.0},
        # 3-4: t0+200 warmup done -> places Bid@99.99 (1.0 qty, active at +250, queue_ahead=2.0) and Ask@100.01
        {"ts": t0 + 200, "kind": "arcus_book", "bid": 99.99, "bsz": 4.0, "ask": 100.01, "asz": 4.0},
        {"ts": t0 + 200, "kind": "hedge_book", "bid": 99.99, "bsz": 4.0, "ask": 100.01, "asz": 4.0},
        # 5: t0+300 SELL trade @ 99.99 sz 2.5 -> clears 2.0 queue_ahead, fills 0.50 qty! Hedge queued @ +630
        {"ts": t0 + 300, "kind": "arcus_trade", "side": "SELL", "price": 99.99, "size": 2.5},
        # 6: t0+400 RH book with thin bid depth 0.30 (< 0.50 needed)
        {"ts": t0 + 400, "kind": "hedge_book", "bid": 99.98, "bsz": 0.30, "ask": 100.00, "asz": 4.0},
        # 7: t0+650 Arcus book ticks down (bid 99.97, ask 99.99) + triggers pending hedge @ +630!
        #    Hedges 0.30 @ 99.98, exhausts single-use rem_hb_sz=0, retries 0.20 orphan @ t0+650+330 = t0+980!
        #    Also remaining Bid@99.99 is crossed, and decide() requests cancel on Bid@99.99 (effective t0+750)
        #    and Ask@100.01 (effective t0+750).
        {"ts": t0 + 650, "kind": "arcus_book", "bid": 99.97, "bsz": 4.0, "ask": 99.99, "asz": 4.0},
        # 8: t0+700 BUY trade @ 100.01 sz 3.0 during cancel-in-flight (< t0+750) -> clears queue_ahead=2.0,
        #    fills 1.00 Ask@100.01! Hedge BUY queued @ t0+1030.
        {"ts": t0 + 700, "kind": "arcus_trade", "side": "BUY", "price": 100.01, "size": 3.0},
        # 9: t0+1000 Arcus book refreshes -> triggers retry1 (0.20 SELL) @ +980 against RH from t0+400 (rem_hb_sz=0)!
        #    Clipped to 0 -> retries @ t0+1000+330 = t0+1330.
        {"ts": t0 + 1000, "kind": "arcus_book", "bid": 99.97, "bsz": 4.0, "ask": 99.99, "asz": 4.0},
        # 10: t0+1100 RH book refreshes (bid 99.98 sz 2.0, ask 100.00 sz 2.0) -> processes BUY hedge @ +1030 BEFORE
        #     installing new RH touch (against old ask 100.00 sz 4.0 -> fills 1.00 @ 100.00!), THEN installs new touch!
        {"ts": t0 + 1100, "kind": "hedge_book", "bid": 99.98, "bsz": 2.0, "ask": 100.00, "asz": 2.0},
        # 11: t0+1400 Arcus book -> processes retry2 (0.20 SELL) @ +1330 against RH bid 99.98 sz 2.0 -> fills 0.20 @ 99.98!
        {"ts": t0 + 1400, "kind": "arcus_book", "bid": 99.97, "bsz": 4.0, "ask": 99.99, "asz": 4.0},
        # 12: t0+2000 RH book refreshes
        {"ts": t0 + 2000, "kind": "hedge_book", "bid": 99.97, "bsz": 5.0, "ask": 99.99, "asz": 5.0},
        # 13: t0+2000 Arcus book (Bid@99.97, Ask@99.99 placed, active at +2050)
        {"ts": t0 + 2000, "kind": "arcus_book", "bid": 99.97, "bsz": 2.0, "ask": 99.99, "asz": 2.0},
        # 14: t0+2100 SELL trade @ 99.97 sz 3.0 -> clears queue_ahead=2.0, fills 1.00 Bid@99.97! Hedge SELL @ +2430
        {"ts": t0 + 2100, "kind": "arcus_trade", "side": "SELL", "price": 99.97, "size": 3.0},
        # 15: t0+7500 Arcus book ONLY (RH is stale: 7500 - 2000 = 5500 > 5000 max_book_age_ms!)
        #     Triggers pending SELL hedge @ +2430 -> STALE HEDGE BLOCK! Re-queues @ t0+7500+330 = t0+7830
        {"ts": t0 + 7500, "kind": "arcus_book", "bid": 99.98, "bsz": 4.0, "ask": 100.00, "asz": 4.0},
        # 16: t0+7900 RH book refreshes -> processes SELL hedge @ +7830 BEFORE install, STILL stale (7900-2000 > 5000)!
        #     STALE HEDGE BLOCK #2! Re-queues @ t0+7900+330 = t0+8230, THEN installs fresh RH touch!
        {"ts": t0 + 7900, "kind": "hedge_book", "bid": 99.98, "bsz": 5.0, "ask": 100.00, "asz": 5.0},
        # 17: t0+8300 Arcus book -> processes SELL hedge @ +8230 against fresh RH touch -> fills 1.00 @ 99.98!
        {"ts": t0 + 8300, "kind": "arcus_book", "bid": 99.98, "bsz": 4.0, "ask": 100.00, "asz": 4.0},
        # 18-19: t0+9600 inside terminal_drain_ms (9500..10000) -> net Arcus=+0.51 long -> only ASK@100.00 allowed!
        {"ts": t0 + 9600, "kind": "hedge_book", "bid": 99.98, "bsz": 5.0, "ask": 100.00, "asz": 5.0},
        {"ts": t0 + 9600, "kind": "arcus_book", "bid": 99.98, "bsz": 2.0, "ask": 100.00, "asz": 2.0},
        # 20: t0+9700 BUY trade @ 100.00 sz 3.0 -> clears queue_ahead=2.0, fills 0.51 Ask@100.00! Hedge BUY @ t0+10030 (> declared_end_ms!)
        {"ts": t0 + 9700, "kind": "arcus_trade", "side": "BUY", "price": 100.00, "size": 3.0},
        # 21-22: t0+9950 final books inside declared_end_ms
        {"ts": t0 + 9950, "kind": "arcus_book", "bid": 99.99, "bsz": 4.0, "ask": 100.01, "asz": 4.0},
        {"ts": t0 + 9950, "kind": "hedge_book", "bid": 99.99, "bsz": 4.0, "ask": 100.01, "asz": 4.0},
        # 23-24: t0+10050 strictly AFTER declared_end_ms (must be ignored completely!)
        {"ts": t0 + 10050, "kind": "arcus_book", "bid": 105.00, "bsz": 10.0, "ask": 105.02, "asz": 10.0},
        {"ts": t0 + 10050, "kind": "arcus_trade", "side": "BUY", "price": 105.02, "size": 5.0},
    ]
    assert len(events) == 24
    expected = simulate_arcus_hedge_turnover(events, cfg)
    return cfg, events, expected


def discover_w01_w27_campaigns(
    lakehouse_dir: Path,
    domain: str,
    *,
    min_window: int = 1,
    max_window: int = 27,
) -> list[dict[str, Any]]:
    """Discover Tokyo W01..W27 campaigns dynamically, strictly excluding `-w28`."""
    venue_dir = "bulk" if domain == "bulk" else "arcus"
    pat = str(
        lakehouse_dir
        / "parquet"
        / "bbo_quotes"
        / "collector_region=tokyo"
        / f"venue={venue_dir}"
        / "utc_date=*"
        / f"campaign_id=tokyo-{domain}-*-w*"
    )
    by_w: dict[int, dict[str, Any]] = {}
    for d in glob.glob(pat):
        cid = d.rsplit("campaign_id=", 1)[1]
        if cid.endswith("-w28"):
            continue
        m = re.search(rf"^tokyo-{re.escape(domain)}-(\d{{8}})T\d+Z-w(\d+)$", cid)
        if not m:
            continue
        ymd, w_num = m.group(1), int(m.group(2))
        if w_num < min_window or w_num > max_window:
            continue
        if domain == "crypto" and w_num == 8:
            continue
        utc_date = f"{ymd[:4]}-{ymd[4:6]}-{ymd[6:8]}"
        if w_num not in by_w:
            by_w[w_num] = {
                "window_num": w_num,
                "window": f"w{w_num:02d}",
                "campaign_id": cid,
                "utc_date": utc_date,
                "domain": domain,
                "split": split_for_window(w_num),
            }
    return [by_w[w] for w in sorted(by_w)]


def campaign_parquet_files(
    lakehouse_dir: Path, dataset: str, venue: str, utc_date: str, campaign_id: str
) -> list[str]:
    """Cross-midnight Hive parquet globbing over `utc_date` and `utc_date + 1`, never `-w28`."""
    if campaign_id.endswith("-w28"):
        return []
    date_dirs = [utc_date]
    try:
        date_dirs.append((date.fromisoformat(utc_date) + timedelta(days=1)).isoformat())
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
    return sorted({f for f in files if "-w28/" not in f and "-w28\\" not in f})


def _sql_list(files: Sequence[str]) -> str:
    return "[" + ", ".join("'" + f.replace("'", "''") + "'" for f in files) + "]"


def _infer_tick(prices: Sequence[float]) -> float:
    best = 1.0
    for px in prices[:400]:
        if px > 0.0 and math.isfinite(px):
            s = f"{px:.8f}".rstrip("0").rstrip(".")
            if "." in s:
                best = min(best, 10.0 ** (-len(s.split(".")[1])))
    return max(best, 1e-6)


def _load_venue_bbo_events(
    con: Any,
    files: Sequence[str],
    markets: Sequence[str],
    *,
    heartbeat_ms: int = 1_000,
) -> dict[str, tuple[list[tuple[int, float, float, float, float]], int, int]]:
    """Load exact-timestamp BBO events (keeping every BBO change + <=1s heartbeat) excluding `-w28`."""
    if not files or not markets:
        return {}
    src = _sql_list(files)
    ph = ", ".join("?" for _ in markets)
    cols = con.execute(
        f"""
        WITH raw AS (
            SELECT market,
                   CAST(available_at_ms AS BIGINT) AS ts,
                   bid_px_f64 AS bb,
                   CAST(bid_sz AS DOUBLE) AS bsz,
                   ask_px_f64 AS ba,
                   CAST(ask_sz AS DOUBLE) AS asz
            FROM read_parquet({src})
            WHERE market IN ({ph})
              AND tradable
              AND bid_px_f64 > 0
              AND ask_px_f64 > bid_px_f64
              AND bid_sz > 0
              AND ask_sz > 0
              AND NOT regexp_matches(campaign_id, '-w28$')
        ),
        lagged AS (
            SELECT market, ts, bb, bsz, ba, asz,
                   LAG(bb) OVER (PARTITION BY market ORDER BY ts) AS p_bb,
                   LAG(bsz) OVER (PARTITION BY market ORDER BY ts) AS p_bsz,
                   LAG(ba) OVER (PARTITION BY market ORDER BY ts) AS p_ba,
                   LAG(asz) OVER (PARTITION BY market ORDER BY ts) AS p_asz,
                   LAG(ts) OVER (PARTITION BY market ORDER BY ts) AS p_ts
            FROM raw
        )
        SELECT market, ts, bb, bsz, ba, asz
        FROM lagged
        WHERE p_bb IS NULL
           OR bb != p_bb OR ba != p_ba OR bsz != p_bsz OR asz != p_asz
           OR (ts // {int(heartbeat_ms)}) != (p_ts // {int(heartbeat_ms)})
        ORDER BY 1, 2
        """,
        list(markets),
    ).fetchnumpy()
    mkt_arr = cols["market"]
    ts_arr = cols["ts"]
    bb_arr = cols["bb"]
    bsz_arr = cols["bsz"]
    ba_arr = cols["ba"]
    asz_arr = cols["asz"]

    out: dict[str, tuple[list[tuple[int, float, float, float, float]], int, int]] = {}
    n_tot = len(mkt_arr)
    start = 0
    while start < n_tot:
        cur_m = str(mkt_arr[start])
        end = start + 1
        while end < n_tot and str(mkt_arr[end]) == cur_m:
            end += 1
        rows = [
            (int(ts_arr[i]), float(bb_arr[i]), float(bsz_arr[i]), float(ba_arr[i]), float(asz_arr[i]))
            for i in range(start, end)
        ]
        if len(rows) >= 2:
            out[cur_m] = (rows, rows[0][0], rows[-1][0])
        start = end
    return out


def _load_venue_trades_events(
    con: Any,
    files: Sequence[str],
    markets: Sequence[str],
) -> dict[str, list[tuple[int, str, float, float]]]:
    if not files or not markets:
        return {}
    src = _sql_list(files)
    ph = ", ".join("?" for _ in markets)
    cols = con.execute(
        f"""
        SELECT market,
               CAST(available_at_ms AS BIGINT) AS ts,
               CASE WHEN lower(aggressor_side) IN ('buy','b','bid') THEN 'BUY'
                    WHEN lower(aggressor_side) IN ('sell','s','ask') THEN 'SELL'
                    ELSE '' END AS side,
               price_f64 AS px,
               size_f64 AS sz
        FROM read_parquet({src})
        WHERE market IN ({ph})
          AND NOT is_zero_price_transfer
          AND price_f64 > 0
          AND size_f64 > 0
          AND NOT regexp_matches(campaign_id, '-w28$')
        ORDER BY 1, 2
        """,
        list(markets),
    ).fetchnumpy()
    mkt_arr = cols["market"]
    ts_arr = cols["ts"]
    sd_arr = cols["side"]
    px_arr = cols["px"]
    sz_arr = cols["sz"]
    out: dict[str, list[tuple[int, str, float, float]]] = {}
    n_tot = len(mkt_arr)
    start = 0
    while start < n_tot:
        cur_m = str(mkt_arr[start])
        end = start + 1
        while end < n_tot and str(mkt_arr[end]) == cur_m:
            end += 1
        rows = [
            (int(ts_arr[i]), str(sd_arr[i]), float(px_arr[i]), float(sz_arr[i]))
            for i in range(start, end)
            if str(sd_arr[i]) in ("BUY", "SELL")
        ]
        if rows:
            out[cur_m] = rows
        start = end
    return out


def _build_merged_stream(
    arcus_bbo: Sequence[tuple[int, float, float, float, float]],
    hedge_bbo: Sequence[tuple[int, float, float, float, float]],
    arcus_trades: Sequence[tuple[int, str, float, float]],
) -> tuple[list[tuple[int, int, Any, float, float, float]], int, int, float]:
    t0 = max(arcus_bbo[0][0], hedge_bbo[0][0])
    t1 = min(arcus_bbo[-1][0], hedge_bbo[-1][0])
    stream: list[tuple[int, int, Any, float, float, float]] = []
    for ts, bb, bsz, ba, asz in arcus_bbo:
        if t0 <= ts <= t1:
            stream.append((ts, 0, bb, bsz, ba, asz))
    for ts, hb, hbsz, ha, hasz in hedge_bbo:
        if t0 <= ts <= t1:
            stream.append((ts, 1, hb, hbsz, ha, hasz))
    for ts, sd, px, sz in arcus_trades:
        if t0 <= ts <= t1:
            stream.append((ts, 2, sd, px, sz, 0.0))
    stream.sort(key=lambda t: (t[0], t[1]))
    tick = _infer_tick([r[1] for r in arcus_bbo])
    return stream, t0, t1, tick


def _run_window_worker(args: tuple[str, dict[str, Any], dict[str, Any]]) -> dict[str, Any]:
    """Process one W01-W27 window across Method 2 sweeps + multi-venue comparisons."""
    import duckdb

    lakehouse_str, camp, fee_cfg = args
    lakehouse_dir = Path(lakehouse_str)
    dom = str(camp["domain"])
    wnum = int(camp["window_num"])
    wname = str(camp["window"])
    split = str(camp["split"])
    cid = str(camp["campaign_id"])
    udate = str(camp["utc_date"])

    con = duckdb.connect(":memory:")
    con.execute("SET preserve_insertion_order=false; SET threads=2; SET memory_limit='1500MB';")

    arcus_mkts = sorted(
        {
            VENUE_SYMBOL_MAP["arcus"][s]
            for s in ((RWA_ARCUS_TRADEXYZ_SYMBOLS + ("QQQ", "SPY")) if dom == "rwa" else CRYPTO_SYMBOLS)
        }
    )
    bbo_arcus = _load_venue_bbo_events(
        con, campaign_parquet_files(lakehouse_dir, "bbo_quotes", "arcus", udate, cid), arcus_mkts
    )
    tr_arcus = _load_venue_trades_events(
        con, campaign_parquet_files(lakehouse_dir, "public_trades", "arcus", udate, cid), arcus_mkts
    )

    out_rows: dict[str, list[dict[str, Any]]] = {}

    def _add(bucket_key: str, res: dict[str, Any], sym: str) -> None:
        r = dict(res)
        r["window"] = wname
        r["window_num"] = wnum
        r["split"] = split
        r["symbol"] = sym
        out_rows.setdefault(bucket_key, []).append(r)

    if dom == "rwa":
        rh_mkts = [VENUE_SYMBOL_MAP["lighter_robinhood"][s] for s in RWA_ARCUS_RH_SYMBOLS]
        xyz_mkts = [VENUE_SYMBOL_MAP["trade_xyz"][s] for s in RWA_ARCUS_TRADEXYZ_SYMBOLS]
        ent_mkts = [VENUE_SYMBOL_MAP["entropy"][s] for s in RWA_ARCUS_ENTROPY_SYMBOLS]

        bbo_rh = _load_venue_bbo_events(
            con, campaign_parquet_files(lakehouse_dir, "bbo_quotes", "lighter_robinhood", udate, cid), rh_mkts
        )
        bbo_xyz = _load_venue_bbo_events(
            con, campaign_parquet_files(lakehouse_dir, "bbo_quotes", "trade_xyz", udate, cid), xyz_mkts
        )
        bbo_ent = _load_venue_bbo_events(
            con, campaign_parquet_files(lakehouse_dir, "bbo_quotes", "entropy", udate, cid), ent_mkts
        )

        # 1) Method 2: Arcus x Lighter-RH across 14 RWA symbols
        for sym in RWA_ARCUS_RH_SYMBOLS:
            am = VENUE_SYMBOL_MAP["arcus"][sym]
            hm = VENUE_SYMBOL_MAP["lighter_robinhood"][sym]
            if am not in bbo_arcus or hm not in bbo_rh or am not in tr_arcus:
                continue
            stream, t0, t1, tick = _build_merged_stream(bbo_arcus[am][0], bbo_rh[hm][0], tr_arcus[am])
            if t1 - t0 < DEFAULT_WARMUP_MS + 120_000 or not stream:
                continue
            dec_end = min(t1, t0 + TEN_HOURS_MS)
            base_kw = dict(
                symbol=sym,
                tick_size=tick,
                maker_fee_bps=float(fee_cfg["arcus_rwa_mk"]),
                hedge_taker_fee_bps=float(fee_cfg["rh_tk"]),
                declared_end_ms=dec_end,
            )

            # Pareto frontier over entry_edge_bps
            for edge in ENTRY_EDGE_PARETO_BPS:
                cfg = ArcusHedgeTurnoverConfig(name=f"pareto_e{edge}", entry_edge_bps=edge, **base_kw)
                _add(f"m2_pareto|e={edge}", simulate_arcus_hedge_turnover(stream, cfg, already_normalized=True), sym)

            # lead_beta ablation at edge=0.0 and 0.5
            for edge in (0.0, 0.5):
                for lb in (0.0, 0.5, 1.0):
                    cfg = ArcusHedgeTurnoverConfig(
                        name=f"lb{lb}_e{edge}", entry_edge_bps=edge, lead_beta=lb, **base_kw
                    )
                    _add(
                        f"m2_lead_beta|lb={lb}|e={edge}",
                        simulate_arcus_hedge_turnover(stream, cfg, already_normalized=True),
                        sym,
                    )

            # Maker submit_rtt_ms / cancel_ms sensitivity at edge=0.0 and 0.5
            for edge in (0.0, 0.5):
                for sub_ms, can_ms in MAKER_LATENCY_GRID:
                    cfg = ArcusHedgeTurnoverConfig(
                        name=f"lat_s{sub_ms}_c{can_ms}_e{edge}",
                        entry_edge_bps=edge,
                        submit_rtt_ms=sub_ms,
                        cancel_ms=can_ms,
                        **base_kw,
                    )
                    _add(
                        f"m2_maker_lat|s={sub_ms}|c={can_ms}|e={edge}",
                        simulate_arcus_hedge_turnover(stream, cfg, already_normalized=True),
                        sym,
                    )

            # Hedge flight delay sensitivity (150 / 330 / 500 ms) at edge=0.0 and 0.5
            for edge in (0.0, 0.5):
                for hd_ms in HEDGE_DELAY_GRID_MS:
                    cfg = ArcusHedgeTurnoverConfig(
                        name=f"hd{hd_ms}_e{edge}",
                        entry_edge_bps=edge,
                        hedge_delay_ms=hd_ms,
                        orphan_retry_delay_ms=hd_ms,
                        **base_kw,
                    )
                    _add(
                        f"m2_hedge_delay|hd={hd_ms}|e={edge}",
                        simulate_arcus_hedge_turnover(stream, cfg, already_normalized=True),
                        sym,
                    )

            # Mandatory unprinted toxic-fill stress at edge=0.0 and 0.5
            for edge in (0.0, 0.5):
                for tlabel, tmode, tfrac in TOXIC_STRESS_GRID:
                    if tmode == "none":
                        continue
                    cfg = ArcusHedgeTurnoverConfig(
                        name=f"tox_{tlabel}_e{edge}",
                        entry_edge_bps=edge,
                        toxic_mode=tmode,
                        toxic_fill_frac=tfrac,
                        **base_kw,
                    )
                    _add(
                        f"m2_toxic|{tlabel}|e={edge}",
                        simulate_arcus_hedge_turnover(stream, cfg, already_normalized=True),
                        sym,
                    )

            # Clip size sensitivity ($50 / $100 / $200 / $500) at edge=0.0 and 0.5
            for edge in (0.0, 0.5):
                for clip in CLIP_USD_GRID:
                    cfg = ArcusHedgeTurnoverConfig(
                        name=f"clip{int(clip)}_e{edge}",
                        entry_edge_bps=edge,
                        clip_usd=clip,
                        max_inventory_usd=3.0 * clip,
                        **base_kw,
                    )
                    _add(
                        f"m2_clip|clip={int(clip)}|e={edge}",
                        simulate_arcus_hedge_turnover(stream, cfg, already_normalized=True),
                        sym,
                    )

        # 2) Method 4: Arcus x TradeXYZ (real W01-W27 trade_xyz BBO! xyz:MSTR lacks growthMode => 9.0 bps!)
        for sym in RWA_ARCUS_TRADEXYZ_SYMBOLS:
            am = VENUE_SYMBOL_MAP["arcus"][sym]
            xm = VENUE_SYMBOL_MAP["trade_xyz"][sym]
            if am not in bbo_arcus or xm not in bbo_xyz or am not in tr_arcus:
                continue
            stream, t0, t1, tick = _build_merged_stream(bbo_arcus[am][0], bbo_xyz[xm][0], tr_arcus[am])
            if t1 - t0 < DEFAULT_WARMUP_MS + 120_000 or not stream:
                continue
            dec_end = min(t1, t0 + TEN_HOURS_MS)
            xyz_tk = float(fee_cfg["xyz_std_tk"] if sym == "MSTR" else fee_cfg["xyz_gm_tk"])
            for edge in (0.0, 0.5):
                cfg = ArcusHedgeTurnoverConfig(
                    name=f"xyz_gm_e{edge}",
                    symbol=sym,
                    tick_size=tick,
                    entry_edge_bps=edge,
                    hedge_delay_ms=200,
                    orphan_retry_delay_ms=200,
                    maker_fee_bps=float(fee_cfg["arcus_rwa_mk"]),
                    hedge_taker_fee_bps=xyz_tk,
                    declared_end_ms=dec_end,
                )
                res = simulate_arcus_hedge_turnover(stream, cfg, already_normalized=True)
                _add(f"m4_xyz_gm|e={edge}", res, sym)
                if sym != "MSTR":
                    _add(f"m4_xyz_gm_ex_mstr|e={edge}", res, sym)
                if sym in RWA_ARCUS_RH_STOCK12_SYMBOLS:
                    _add(f"m4_xyz_gm_common12|e={edge}", res, sym)

                cfg_330 = ArcusHedgeTurnoverConfig(
                    name=f"xyz_gm330_e{edge}",
                    symbol=sym,
                    tick_size=tick,
                    entry_edge_bps=edge,
                    hedge_delay_ms=330,
                    orphan_retry_delay_ms=330,
                    maker_fee_bps=float(fee_cfg["arcus_rwa_mk"]),
                    hedge_taker_fee_bps=xyz_tk,
                    declared_end_ms=dec_end,
                )
                res_330 = simulate_arcus_hedge_turnover(stream, cfg_330, already_normalized=True)
                if sym in RWA_ARCUS_RH_STOCK12_SYMBOLS:
                    _add(f"m4_xyz_gm330_common12|e={edge}", res_330, sym)

        # 3) Method 3: Arcus x Entropy (real W01-W27 entropy BBO on 5 overlapping markets)
        for sym in RWA_ARCUS_ENTROPY_SYMBOLS:
            am = VENUE_SYMBOL_MAP["arcus"][sym]
            em = VENUE_SYMBOL_MAP["entropy"][sym]
            if am not in bbo_arcus or em not in bbo_ent or am not in tr_arcus:
                continue
            stream, t0, t1, tick = _build_merged_stream(bbo_arcus[am][0], bbo_ent[em][0], tr_arcus[am])
            if t1 - t0 < DEFAULT_WARMUP_MS + 120_000 or not stream:
                continue
            dec_end = min(t1, t0 + TEN_HOURS_MS)
            for edge in (0.0, 0.5):
                cfg = ArcusHedgeTurnoverConfig(
                    name=f"ent_e{edge}",
                    symbol=sym,
                    tick_size=tick,
                    entry_edge_bps=edge,
                    hedge_delay_ms=200,
                    orphan_retry_delay_ms=200,
                    maker_fee_bps=float(fee_cfg["arcus_rwa_mk"]),
                    hedge_taker_fee_bps=float(fee_cfg["entropy_tk"]),
                    declared_end_ms=dec_end,
                )
                _add(f"m3_entropy|e={edge}", simulate_arcus_hedge_turnover(stream, cfg, already_normalized=True), sym)

    elif dom == "crypto":
        ext_mkts = [VENUE_SYMBOL_MAP["extended"][s] for s in CRYPTO_SYMBOLS]
        bbo_ext = _load_venue_bbo_events(
            con, campaign_parquet_files(lakehouse_dir, "bbo_quotes", "extended", udate, cid), ext_mkts
        )
        for sym in CRYPTO_SYMBOLS:
            am = VENUE_SYMBOL_MAP["arcus"][sym]
            xm = VENUE_SYMBOL_MAP["extended"][sym]
            if am not in bbo_arcus or xm not in bbo_ext or am not in tr_arcus:
                continue
            stream, t0, t1, tick = _build_merged_stream(bbo_arcus[am][0], bbo_ext[xm][0], tr_arcus[am])
            if t1 - t0 < DEFAULT_WARMUP_MS + 120_000 or not stream:
                continue
            dec_end = min(t1, t0 + TEN_HOURS_MS)
            for edge in (0.0, 0.5):
                cfg = ArcusHedgeTurnoverConfig(
                    name=f"ext_crypto_e{edge}",
                    symbol=sym,
                    tick_size=tick,
                    entry_edge_bps=edge,
                    hedge_delay_ms=150,
                    orphan_retry_delay_ms=150,
                    maker_fee_bps=float(fee_cfg["arcus_crypto_mk"]),
                    hedge_taker_fee_bps=float(fee_cfg["extended_crypto_tk"]),
                    declared_end_ms=dec_end,
                )
                _add(f"bonus_extended_crypto|e={edge}", simulate_arcus_hedge_turnover(stream, cfg, already_normalized=True), sym)

    con.close()
    gc.collect()
    return {"window": wname, "domain": dom, "campaign_id": cid, "rows": out_rows}


def _run_bulk_diagnostic_worker(args: tuple[str, dict[str, Any], dict[str, Any]]) -> dict[str, Any]:
    """Evaluate Arcus x Bulk diagnostic on Bulk's 200ms snapshot BBOs (excluding -w28)."""
    import duckdb

    lakehouse_str, bulk_camp, fee_cfg = args
    lakehouse_dir = Path(lakehouse_str)
    wnum = int(bulk_camp["window_num"])
    wname = str(bulk_camp["window"])
    split = str(bulk_camp["split"])
    bulk_cid = str(bulk_camp["campaign_id"])
    bulk_udate = str(bulk_camp["utc_date"])

    # Match Arcus crypto & rwa campaigns for same window_num
    con = duckdb.connect(":memory:")
    con.execute("SET preserve_insertion_order=false; SET threads=2; SET memory_limit='1500MB';")
    rows_out: dict[str, list[dict[str, Any]]] = {}

    bulk_mkts = sorted(set(VENUE_SYMBOL_MAP["bulk"].values()))
    bbo_bulk = _load_venue_bbo_events(
        con, campaign_parquet_files(lakehouse_dir, "bbo_quotes", "bulk", bulk_udate, bulk_cid), bulk_mkts
    )

    for dom, syms in (("crypto", CRYPTO_SYMBOLS), ("rwa", ("CRCL", "MSTR", "MU", "NVDA", "QQQ", "SNDK", "TSLA"))):
        arcus_camps = [
            c for c in discover_w01_w27_campaigns(lakehouse_dir, dom, min_window=wnum, max_window=wnum)
        ]
        if not arcus_camps:
            continue
        acamp = arcus_camps[0]
        amkts = [VENUE_SYMBOL_MAP["arcus"][s] for s in syms]
        bbo_a = _load_venue_bbo_events(
            con, campaign_parquet_files(lakehouse_dir, "bbo_quotes", "arcus", acamp["utc_date"], acamp["campaign_id"]), amkts
        )
        tr_a = _load_venue_trades_events(
            con, campaign_parquet_files(lakehouse_dir, "public_trades", "arcus", acamp["utc_date"], acamp["campaign_id"]), amkts
        )
        for sym in syms:
            am = VENUE_SYMBOL_MAP["arcus"][sym]
            bm = VENUE_SYMBOL_MAP["bulk"][sym]
            if am not in bbo_a or bm not in bbo_bulk or am not in tr_a:
                continue
            stream, t0, t1, tick = _build_merged_stream(bbo_a[am][0], bbo_bulk[bm][0], tr_a[am])
            if t1 - t0 < DEFAULT_WARMUP_MS + 120_000 or not stream:
                continue
            dec_end = min(t1, t0 + TEN_HOURS_MS)
            mk_fee = float(fee_cfg["arcus_crypto_mk"] if dom == "crypto" else fee_cfg["arcus_rwa_mk"])
            tk_fee = float(fee_cfg["bulk_crypto_tk"] if dom == "crypto" else fee_cfg["bulk_rwa_tk"])
            for edge in (0.0, 0.5):
                cfg = ArcusHedgeTurnoverConfig(
                    name=f"bulk_{dom}_e{edge}",
                    symbol=sym,
                    tick_size=tick,
                    entry_edge_bps=edge,
                    hedge_delay_ms=150,
                    orphan_retry_delay_ms=150,
                    maker_fee_bps=mk_fee,
                    hedge_taker_fee_bps=tk_fee,
                    declared_end_ms=dec_end,
                )
                r = simulate_arcus_hedge_turnover(stream, cfg, already_normalized=True)
                r["window"] = wname
                r["window_num"] = wnum
                r["split"] = split
                r["symbol"] = sym
                rows_out.setdefault(f"m5_bulk_{dom}|e={edge}", []).append(r)

    con.close()
    return {"window": wname, "rows": rows_out}


def resolve_benchmark_fees(
    fee_evidence_path: Path | None,
    fee_probe_path: Path | None,
) -> tuple[dict[str, float], dict[str, Any]]:
    """Resolve verified fees from `account_fee_evidence_20261127.json` and `@fee-probe` JSON."""
    ev = load_track_fee_evidence(fee_evidence_path)
    prov = build_fee_provenance(ev, venues_in_scope=("arcus", "lighter_robinhood", "trade_xyz", "extended"))

    amk_rwa, atk_rwa, _, _ = resolve_venue_fee(
        "arcus", "rwa", default_maker_bps=0.0, default_taker_bps=2.1375,
        default_status="verified_account_tier", evidence=ev, raw_market="NVDA-USD",
    )
    amk_cry, atk_cry, _, _ = resolve_venue_fee(
        "arcus", "crypto", default_maker_bps=1.425, default_taker_bps=4.275,
        default_status="verified_account_tier", evidence=ev, raw_market="BTC-USD",
    )
    rmk, rtk, _, _ = resolve_venue_fee(
        "lighter_robinhood", "rwa", default_maker_bps=0.0, default_taker_bps=0.0,
        default_status="verified_account_tier", evidence=ev,
    )
    xmk, xtk, _, _ = resolve_venue_fee(
        "trade_xyz", "rwa", default_maker_bps=0.30, default_taker_bps=0.90,
        default_status="verified_account_tier", evidence=ev, trade_xyz_fee_mode="growth_mode",
    )
    emk_rwa, etk_rwa, _, _ = resolve_venue_fee(
        "extended", "rwa", default_maker_bps=0.0, default_taker_bps=0.90,
        default_status="verified_account_tier", evidence=ev,
    )
    emk_cry, etk_cry, _, _ = resolve_venue_fee(
        "extended", "crypto", default_maker_bps=0.0, default_taker_bps=2.25,
        default_status="verified_account_tier", evidence=ev,
    )
    bmk, btk, _, _ = resolve_venue_fee(
        "backpack", "crypto", default_maker_bps=1.90, default_taker_bps=4.50,
        default_status="verified_account_tier", evidence=ev,
    )

    fees: dict[str, float] = {
        "arcus_rwa_mk": amk_rwa if ev.get("loaded") else 0.0,
        "arcus_rwa_tk": atk_rwa if ev.get("loaded") else 2.1375,
        "arcus_crypto_mk": amk_cry if ev.get("loaded") else 1.425,
        "arcus_crypto_tk": atk_cry if ev.get("loaded") else 4.275,
        "rh_mk": rmk,
        "rh_tk": rtk,
        "xyz_gm_mk": xmk,
        "xyz_gm_tk": xtk,
        "xyz_std_tk": 9.0,
        "extended_rwa_mk": emk_rwa,
        "extended_rwa_tk": etk_rwa,
        "extended_crypto_mk": emk_cry,
        "extended_crypto_tk": etk_cry,
        "backpack_mk": bmk,
        "backpack_tk": btk,
        "entropy_mk": 0.30,
        "entropy_tk": 0.90,
        "bulk_mk": 0.0,
        "bulk_rwa_tk": 1.50,
        "bulk_crypto_tk": 3.50,
    }
    if fee_probe_path is not None and fee_probe_path.exists():
        raw_bytes = fee_probe_path.read_bytes()
        probe = json.loads(raw_bytes.decode("utf-8"))
        prov["fee_probe_sha256"] = hashlib.sha256(raw_bytes).hexdigest()
        prov["fee_probe_name"] = fee_probe_path.name
        venues_p = probe.get("venues") or {}
        if isinstance(venues_p.get("bulk"), dict):
            bp = venues_p["bulk"]
            rwa_sched = bp.get("tradfi_rwa_fee_schedule_bps") or {}
            if rwa_sched.get("taker_fee_bps") is not None:
                fees["bulk_rwa_tk"] = float(rwa_sched["taker_fee_bps"])
            if rwa_sched.get("maker_fee_bps") is not None:
                fees["bulk_mk"] = float(rwa_sched["maker_fee_bps"])
            cry_sched = (bp.get("crypto_fee_schedule_bps") or {}).get("tiers") or []
            if cry_sched and cry_sched[0].get("taker_fee_bps") is not None:
                fees["bulk_crypto_tk"] = float(cry_sched[0]["taker_fee_bps"])
        six_m = probe.get("six_methods_roundtrip_fee_floor_bps") or {}
        m3 = six_m.get("m3_arcus_rwa_maker_x_entropy_io_taker") or {}
        if m3.get("leg2_entropy_growth_mode_taker_bps") is not None:
            fees["entropy_tk"] = float(m3["leg2_entropy_growth_mode_taker_bps"])
        m4 = six_m.get("m4_arcus_rwa_maker_x_tradexyz_taker") or {}
        if m4.get("leg2_xyz_growth_mode_taker_bps") is not None:
            fees["xyz_gm_tk"] = float(m4["leg2_xyz_growth_mode_taker_bps"])
        if m4.get("leg2_xyz_non_growth_taker_bps") is not None:
            fees["xyz_std_tk"] = float(m4["leg2_xyz_non_growth_taker_bps"])
    return fees, prov


def run_lakehouse_benchmark(
    lakehouse_dir: Path,
    *,
    fee_evidence_path: Path | None,
    fee_probe_path: Path | None,
    output_path: Path,
    workers: int = 6,
) -> dict[str, Any]:
    """Run full W01-W27 Method 2 + multi-venue benchmark on Aleph lakehouse."""
    fee_cfg, fee_prov = resolve_benchmark_fees(fee_evidence_path, fee_probe_path)
    rwa_camps = discover_w01_w27_campaigns(lakehouse_dir, "rwa", min_window=1, max_window=27)
    cry_camps = discover_w01_w27_campaigns(lakehouse_dir, "crypto", min_window=1, max_window=27)
    bulk_camps = discover_w01_w27_campaigns(lakehouse_dir, "bulk", min_window=1, max_window=27)

    tasks = [(str(lakehouse_dir), c, fee_cfg) for c in rwa_camps + cry_camps]
    store: dict[str, list[dict[str, Any]]] = {}

    if workers > 1:
        with concurrent.futures.ProcessPoolExecutor(max_workers=workers) as ex:
            for res in ex.map(_run_window_worker, tasks):
                print(f"[done] {res['domain']} {res['window']} ({res['campaign_id']})", flush=True)
                for bk, rows in res["rows"].items():
                    store.setdefault(bk, []).extend(rows)
            if bulk_camps:
                btasks = [(str(lakehouse_dir), bc, fee_cfg) for bc in bulk_camps]
                for bres in ex.map(_run_bulk_diagnostic_worker, btasks):
                    print(f"[done] bulk {bres['window']}", flush=True)
                    for bk, rows in bres["rows"].items():
                        store.setdefault(bk, []).extend(rows)
    else:
        for t in tasks:
            res = _run_window_worker(t)
            for bk, rows in res["rows"].items():
                store.setdefault(bk, []).extend(rows)
        for bc in bulk_camps:
            bres = _run_bulk_diagnostic_worker((str(lakehouse_dir), bc, fee_cfg))
            for bk, rows in bres["rows"].items():
                store.setdefault(bk, []).extend(rows)

    def _agg_splits(rows: Sequence[dict[str, Any]]) -> dict[str, Any]:
        return {
            "all_w01_w27": aggregate_runs(rows),
            "selection_dev": aggregate_runs([r for r in rows if r["split"] == "selection_dev"]),
            "holdout": aggregate_runs([r for r in rows if r["split"] == "holdout"]),
            "w26_w27_oos": aggregate_runs([r for r in rows if r["split"] == "w26_w27_oos"]),
        }

    # Summarize FX drag grid on pareto frontier
    pareto_summary: dict[str, Any] = {}
    for edge in ENTRY_EDGE_PARETO_BPS:
        raw_rows = store.get(f"m2_pareto|e={edge}", [])
        fx_out: dict[str, Any] = {}
        for fx_bp in FX_DRAG_BPS_GRID:
            cloned = [clone_result_with_fx(r, fx_bp) for r in raw_rows]
            fx_out[f"fx_{int(fx_bp)}bp"] = _agg_splits(cloned)
        pareto_summary[f"e={edge}"] = fx_out

    # Per-symbol league table at edge=0.0 and edge=0.5 (FX 0bp & 1bp)
    per_symbol: dict[str, Any] = {}
    for edge in (0.0, 0.5):
        raw_rows = store.get(f"m2_pareto|e={edge}", [])
        sym_map: dict[str, Any] = {}
        for sym in sorted({r["symbol"] for r in raw_rows}):
            srows = [r for r in raw_rows if r["symbol"] == sym]
            sym_map[sym] = {
                "fx_0bp": _agg_splits(srows),
                "fx_1bp": _agg_splits([clone_result_with_fx(r, 1.0) for r in srows]),
            }
        per_symbol[f"e={edge}"] = sym_map

    # Sensitivity tables
    def _group_prefix(prefix: str) -> dict[str, Any]:
        out: dict[str, Any] = {}
        for k, rows in sorted(store.items()):
            if k.startswith(prefix):
                out[k] = {
                    "fx_0bp": _agg_splits(rows),
                    "fx_1bp": _agg_splits([clone_result_with_fx(r, 1.0) for r in rows]),
                }
        return out

    report: dict[str, Any] = {
        "schema_version": "arcus_m2_rh_hedge_w01_w27_v1",
        "w28_excluded": True,
        "campaigns": {
            "rwa": [{"window": c["window"], "window_num": c["window_num"], "split": c["split"]} for c in rwa_camps],
            "crypto": [{"window": c["window"], "window_num": c["window_num"], "split": c["split"]} for c in cry_camps],
            "bulk": [{"window": c["window"], "window_num": c["window_num"], "split": c["split"]} for c in bulk_camps],
        },
        "fee_schedule": fee_cfg,
        "fee_provenance": fee_prov,
        "method2_pareto_frontier": pareto_summary,
        "method2_per_symbol": per_symbol,
        "method2_lead_beta_ablation": _group_prefix("m2_lead_beta|"),
        "method2_maker_latency_sensitivity": _group_prefix("m2_maker_lat|"),
        "method2_hedge_delay_sensitivity": _group_prefix("m2_hedge_delay|"),
        "method2_toxic_fill_stress": _group_prefix("m2_toxic|"),
        "method2_clip_size_sensitivity": _group_prefix("m2_clip|"),
        "head_to_head_venues": {
            "m4_trade_xyz": {k: _agg_splits(v) for k, v in sorted(store.items()) if k.startswith("m4_xyz_gm")},
            "m3_entropy": {k: _agg_splits(v) for k, v in sorted(store.items()) if k.startswith("m3_entropy")},
            "m5_bulk_diagnostic": {k: _agg_splits(v) for k, v in sorted(store.items()) if k.startswith("m5_bulk_")},
            "bonus_extended_crypto": {k: _agg_splits(v) for k, v in sorted(store.items()) if k.startswith("bonus_extended_")},
        },
    }

    output_path.parent.mkdir(parents=True, exist_ok=True)
    output_path.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return report


def run_self_test() -> dict[str, Any]:
    """Run deterministic stdlib-only self-test on the 24-event synthetic parity fixture."""
    cfg, events, res = build_synthetic_parity_fixture()
    assert res["arcus_fills"] == 4, f"expected 4 arcus fills, got {res['arcus_fills']}"
    assert res["hedge_fills"] == 4, f"expected 4 hedge fills, got {res['hedge_fills']}"
    assert res["cancel_in_flight_fills"] == 1
    assert res["orphan_events"] >= 1
    assert res["orphan_retries"] >= 2
    assert res["stale_hedge_blocks"] >= 2
    assert res["depth_clipped_hedges"] >= 1
    assert res["terminal_pending_hedges"] == 1
    assert res["conservation_error_usd"] < 1e-8
    return {
        "self_test": "ok",
        "fixture_events": len(events),
        "config_name": cfg.name,
        "result": res,
    }


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--self-test", action="store_true")
    ap.add_argument("--lakehouse", type=Path, default=Path.home() / "aleph-tx-lakehouse")
    ap.add_argument("--fee-evidence", type=Path, default=None)
    ap.add_argument("--fee-probe", type=Path, default=None)
    ap.add_argument("--workers", type=int, default=6)
    ap.add_argument(
        "--output",
        type=Path,
        default=Path("/tmp/arcus_m2_rh_hedge_w01_w27.json"),
    )
    args = ap.parse_args()

    if args.self_test:
        print(json.dumps(run_self_test(), indent=2, sort_keys=True))
        return 0

    report = run_lakehouse_benchmark(
        args.lakehouse,
        fee_evidence_path=args.fee_evidence,
        fee_probe_path=args.fee_probe,
        output_path=args.output,
        workers=args.workers,
    )
    print(json.dumps({"status": "ok", "output": str(args.output), "rwa_windows": len(report["campaigns"]["rwa"])}))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
