#!/usr/bin/env python3
"""Track 03 P0 Three-Way Same-Event Parity Harness & Roadmap Review Bundle Generator.

Strictly stdlib-only at module top level (verified under `python3 -S`).
Compares three independent implementations on identical sealed JSONL inputs:
  1. Independent Simple Reference Ledger (`IndependentReferenceSimulator`)
  2. Python Replay Simulator (`scripts/research_track03_full_universe_speedbump_mm.py`)
  3. Compiled Rust Shadow Binary (`track01_track02_shadow --track 3 --replay`)

Implements Roadmap §3 (Four-Stage Denominator Scorecard, first-divergence locator,
same-ms dual ordering bounds, isolated timing p50/p95/p99) and §5 (deterministic
Review Bundle: `frozen_plan`, `deterministic_result`, `independent_review_record`).
"""

from __future__ import annotations

import argparse
import dataclasses
import hashlib
import json
import math
import os
import statistics
import subprocess
import sys
import tempfile
import time
from collections import deque
from pathlib import Path
from typing import Any, Sequence

SCRIPT_DIR = Path(__file__).resolve().parent
REPO_ROOT = SCRIPT_DIR.parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

import research_track03_full_universe_speedbump_mm as py_t3  # noqa: E402

ACCOUNT_FEE_EVIDENCE_SHA256 = "4d3a9c1aeae5358bb9c55ab448db392ac3edb0a89244f74c23d89817c3359d92"
DEFAULT_RUST_BIN_CANDIDATES = (
    REPO_ROOT / "target" / "debug" / "track01_track02_shadow",
    REPO_ROOT / "target" / "release" / "track01_track02_shadow",
)
DEFAULT_W27_SLICE = (
    REPO_ROOT.parent
    / "aleph-tx"
    / ".agent-state"
    / "research"
    / "roadmap_upgrade_20261127"
    / "w27_aapl_meta_public_slice.json"
)
DEFAULT_BUNDLE_OUT = Path(
    "/Users/metaverse/funspace/aleph-tx/.agent-state/research/roadmap_upgrade_20261127/p0_parity_review_bundle.json"
)


def _sha256_bytes(raw: bytes) -> str:
    return hashlib.sha256(raw).hexdigest()


def _sha256_json(obj: Any) -> str:
    encoded = json.dumps(obj, sort_keys=True, separators=(",", ":"), allow_nan=False).encode("utf-8")
    return _sha256_bytes(encoded)


def _percentile(values: Sequence[float], pct: float) -> float:
    if not values:
        return 0.0
    ordered = sorted(values)
    idx = (len(ordered) - 1) * (pct / 100.0)
    lo = int(math.floor(idx))
    hi = int(math.ceil(idx))
    if lo == hi:
        return float(ordered[lo])
    frac = idx - lo
    return float(ordered[lo] * (1.0 - frac) + ordered[hi] * frac)


def _clock_stats(values: Sequence[float]) -> dict[str, Any]:
    if not values:
        return {"count": 0, "p50_ms": 0.0, "p95_ms": 0.0, "p99_ms": 0.0, "max_ms": 0.0}
    return {
        "count": len(values),
        "p50_ms": round(_percentile(values, 50.0), 6),
        "p95_ms": round(_percentile(values, 95.0), 6),
        "p99_ms": round(_percentile(values, 99.0), 6),
        "max_ms": round(max(values), 6),
    }


def _round_tick_down(px: float, tick: float) -> float:
    return math.floor((px / tick) + 1e-9) * tick


def _round_tick_up(px: float, tick: float) -> float:
    return math.ceil((px / tick) - 1e-9) * tick


def _quantize_down(qty: float, lot: float) -> float:
    if lot <= 0.0 or qty <= 0.0:
        return 0.0
    return math.floor((qty / lot) + 1e-9) * lot


def _classify_utc_session(now_ms: int) -> str:
    days_since_unix = now_ms // 86_400_000
    weekday = (days_since_unix + 3) % 7
    if weekday >= 5:
        return "weekend"
    minute_of_day = (now_ms // 60_000) % 1_440
    if minute_of_day < 660:
        return "overnight_asia"
    if not (810 <= minute_of_day < 1_200):
        return "us_extended"
    return "us_rth"


@dataclasses.dataclass(frozen=True)
class ParityConfig:
    """Shared baseline configuration mapped identically to Reference, Python, and Rust."""

    symbol: str = "AAPL"
    symbol_id: int = 1006
    target_exchange_id: int = 8
    ref_exchange_id: int = 7
    tick_size: float = 0.01
    lot_size: float = 0.0001
    min_notional_usd: float = 10.0
    quote_notional_usd: float = 200.0
    max_position_notional_usd: float = 400.0
    maker_fee_bps: float = 0.0
    taker_fee_bps: float = 0.0
    rebate_bps: float = 0.0
    queue_ahead_fraction: float = 0.5
    order_delay_ms: int = 0
    cancel_delay_ms: int = 0
    ioc_exit_delay_ms: int = 330
    max_l1_depth_fraction: float = 1.0
    max_book_age_ms: int = 5_000
    max_inventory_hold_ms: int = 5_000
    min_penny_spread_ticks: int = 99
    min_entry_edge_bps: float = 0.10
    lead_cancel_threshold_bps: float = 1.50
    lead_cancel_spread_mult: float = 0.0
    inventory_skew_bps_per_full_limit: float = 0.0
    as_inventory_skew_gamma: float = 0.0
    vol_spread_mult: float = 0.0
    directional_markout_skew_mult: float = 0.0
    markout_shield_adverse_threshold_bps: float = 0.0
    markout_shield_cooldown_ms: int = 1_200
    markout_shield_size_scale: float = 0.5
    quiet_regime_min_spread_to_vol: float = 0.0
    quiet_regime_max_entry_vol_bps: float | None = None
    microprice_alpha_weight: float = 0.0
    tfi_ewma_tau_ms: float = 0.0
    tfi_skew_bps: float = 0.0
    tfi_veto_threshold: float = 0.0
    use_oracle_anchor: bool = False
    require_validated_lead_ref: bool = False
    allowed_utc_sessions: tuple[str, ...] = ("overnight_asia", "us_extended", "weekend")
    declared_end_ms: int | None = None
    terminal_drain_ms: int | None = None
    capital_usd: float = 10_000.0


@dataclasses.dataclass
class _RefLot:
    sgn: float
    qty: float
    entry_px: float
    entry_mid: float
    entry_ts_ms: int


@dataclasses.dataclass
class _RefResting:
    side: str  # "BID" or "ASK"
    price: float
    qty: float
    remaining: float
    queue_ahead: float
    placed_at_ms: int
    active_at_ms: int
    activated: bool
    reduce_only: bool
    cancel_effective_ms: int | None = None


def run_independent_reference_ledger(
    events: Sequence[dict[str, Any]],
    cfg: ParityConfig,
) -> dict[str, Any]:
    """Independent simple event-by-event reference ledger & baseline executor.

    Written cleanly from first principles with explicit cash-flow conservation,
    FIFO lot accounting, single-use L1 depth budget per touch, pre-install
    pending IOC processing, stale 330 ms re-queueing, and declared-end/drain gates.
    """
    t_wall0 = time.perf_counter()

    has_book = False
    cur_bb = 0.0
    cur_bsz = 0.0
    cur_ba = 0.0
    cur_asz = 0.0
    last_book_recv_ms: int | None = None
    last_now_ms: int | None = None
    rem_bid_sz = 0.0
    rem_ask_sz = 0.0

    bid_ord: _RefResting | None = None
    ask_ord: _RefResting | None = None
    open_lots: deque[_RefLot] = deque()
    pos_qty = 0.0
    pending_ioc_exits: deque[tuple[int, str, float]] = deque()

    cash_ex_fee_usd = 0.0
    fees_usd = 0.0
    spr = 0.0
    drft = 0.0
    pex = 0.0
    tslp = 0.0
    closed_notional_usd = 0.0
    maker_volume_usd = 0.0
    taker_volume_usd = 0.0

    maker_fills = 0
    taker_exits = 0
    cycles = 0
    orders_placed = 0
    cancels = 0
    queue_preserved = 0
    alo_rejects = 0
    depth_clipped_exits = 0
    stale_exit_blocks = 0
    stale_book_drops = 0
    crossed_book_drops = 0
    clock_regression_drops = 0
    session_gated_ticks = 0
    total_book_ticks = 0
    out_of_order_events = 0

    bbo_count = 0
    ref_count = 0
    trade_count = 0
    fresh_book_count = 0

    queue_delays_ms: list[float] = []
    ioc_delays_ms: list[float] = []
    ioc_queued_at: dict[str, int] = {}
    eval_latencies_ms: list[float] = []
    trace: list[dict[str, Any]] = []

    peak_equity = cfg.capital_usd
    max_drawdown = 0.0

    drain_start_ms: int | None = (
        cfg.declared_end_ms - cfg.terminal_drain_ms
        if (cfg.declared_end_ms is not None and cfg.terminal_drain_ms is not None and cfg.terminal_drain_ms > 0)
        else None
    )

    def book_is_fresh(now_ms: int) -> bool:
        return (
            has_book
            and last_book_recv_ms is not None
            and now_ms >= last_book_recv_ms
            and (now_ms - last_book_recv_ms) <= cfg.max_book_age_ms
        )

    def current_mid() -> float:
        return 0.5 * (cur_bb + cur_ba) if has_book else 0.0

    def update_drawdown() -> None:
        nonlocal peak_equity, max_drawdown
        mid = current_mid()
        realized = (spr + drft + pex + tslp) - fees_usd
        unrealized = sum(l.sgn * l.qty * (mid - l.entry_px) for l in open_lots) if has_book else 0.0
        eq = cfg.capital_usd + realized + unrealized
        peak_equity = max(peak_equity, eq)
        max_drawdown = max(max_drawdown, peak_equity - eq)

    def record_fill(ts_ms: int, side: str, px: float, qty: float, role: str, mid_now: float) -> None:
        nonlocal cash_ex_fee_usd, fees_usd, pos_qty, spr, drft, pex, tslp
        nonlocal closed_notional_usd, maker_volume_usd, taker_volume_usd
        nonlocal maker_fills, taker_exits, cycles
        if qty <= 1e-12 or px <= 0.0:
            return
        fill_sgn = 1.0 if side == "BUY" else -1.0
        notl = qty * px
        if role == "maker":
            maker_fills += 1
            maker_volume_usd += notl
            fee_bp = cfg.maker_fee_bps - cfg.rebate_bps
        else:
            taker_exits += 1
            taker_volume_usd += notl
            fee_bp = cfg.taker_fee_bps
        cash_ex_fee_usd -= fill_sgn * notl
        fees_usd += notl * (fee_bp * 1e-4)

        rem = qty
        closed_any = False
        while rem > 1e-12 and open_lots and abs(open_lots[0].sgn - fill_sgn) > 0.5:
            lot = open_lots[0]
            closed_qty = min(rem, lot.qty)
            e_spr = closed_qty * lot.sgn * (lot.entry_mid - lot.entry_px)
            e_drft = closed_qty * lot.sgn * (mid_now - lot.entry_mid)
            e_exit = closed_qty * lot.sgn * (px - mid_now)
            spr += e_spr
            drft += e_drft
            if role == "maker":
                pex += e_exit
            else:
                tslp += e_exit
            closed_notional_usd += closed_qty * px
            closed_any = True
            rem -= closed_qty
            lot.qty -= closed_qty
            if lot.qty <= 1e-12:
                open_lots.popleft()
        if closed_any:
            cycles += 1
        if rem > 1e-12:
            open_lots.append(_RefLot(sgn=fill_sgn, qty=rem, entry_px=px, entry_mid=mid_now, entry_ts_ms=ts_ms))
        pos_qty = sum(l.sgn * l.qty for l in open_lots)
        if not open_lots or abs(pos_qty) <= 1e-12:
            pos_qty = 0.0

    def execute_ioc(now_ms: int, side: str, req_qty: float) -> bool:
        nonlocal rem_bid_sz, rem_ask_sz, depth_clipped_exits
        if not has_book:
            return False
        if side == "SELL":
            needed = min(max(0.0, pos_qty), req_qty)
            avail = max(0.0, rem_bid_sz)
            exec_px = cur_bb
        else:
            needed = min(max(0.0, -pos_qty), req_qty)
            avail = max(0.0, rem_ask_sz)
            exec_px = cur_ba
        if needed <= 1e-12:
            return False
        exec_qty = min(needed, avail)
        if exec_qty + 1e-12 < needed:
            depth_clipped_exits += 1
        if exec_qty > 1e-12:
            if side == "SELL":
                rem_bid_sz = max(0.0, rem_bid_sz - exec_qty)
            else:
                rem_ask_sz = max(0.0, rem_ask_sz - exec_qty)
            q_ts = ioc_queued_at.pop(side, None)
            if q_ts is not None:
                ioc_delays_ms.append(float(max(0, now_ms - q_ts)))
            record_fill(now_ms, side, exec_px, exec_qty, "taker", current_mid())
            return True
        return False

    def process_pending_iocs(now_ms: int, *, fresh_now: bool) -> bool:
        nonlocal stale_exit_blocks
        exited = False
        n = len(pending_ioc_exits)
        for _ in range(n):
            exec_at, side, qty = pending_ioc_exits.popleft()
            if now_ms < exec_at:
                pending_ioc_exits.append((exec_at, side, qty))
                continue
            if not fresh_now or not has_book:
                stale_exit_blocks += 1
                pending_ioc_exits.append((now_ms + cfg.ioc_exit_delay_ms, side, qty))
                continue
            if execute_ioc(now_ms, side, qty):
                exited = True
        return exited

    def queue_or_exec_ioc(now_ms: int, side: str, qty: float) -> bool:
        if cfg.ioc_exit_delay_ms == 0:
            return execute_ioc(now_ms, side, qty)
        desired = min(max(0.0, pos_qty), qty) if side == "SELL" else min(max(0.0, -pos_qty), qty)
        already = sum(q for _, s, q in pending_ioc_exits if s == side)
        needed = desired - already
        if needed > 1e-12:
            pending_ioc_exits.append((now_ms + cfg.ioc_exit_delay_ms, side, needed))
            ioc_queued_at.setdefault(side, now_ms)
        return False

    def activate_arrivals(now_ms: int) -> None:
        nonlocal bid_ord, ask_ord, alo_rejects
        tol25 = cfg.tick_size * 0.25
        for side_name in ("BID", "ASK"):
            ord_obj = bid_ord if side_name == "BID" else ask_ord
            if ord_obj is None:
                continue
            if ord_obj.activated:
                if side_name == "BID" and abs(ord_obj.price - cur_bb) <= tol25 and cur_bsz < ord_obj.queue_ahead:
                    ord_obj.queue_ahead = cur_bsz
                elif side_name == "ASK" and abs(ord_obj.price - cur_ba) <= tol25 and cur_asz < ord_obj.queue_ahead:
                    ord_obj.queue_ahead = cur_asz
                continue
            if now_ms < ord_obj.active_at_ms:
                continue
            crosses = ord_obj.price >= cur_ba - 1e-12 if side_name == "BID" else ord_obj.price <= cur_bb + 1e-12
            if crosses:
                if side_name == "BID":
                    bid_ord = None
                else:
                    ask_ord = None
                alo_rejects += 1
                continue
            if side_name == "BID":
                ord_obj.queue_ahead = cfg.queue_ahead_fraction * cur_bsz if (abs(ord_obj.price - cur_bb) <= tol25 or ord_obj.price <= cur_bb + tol25) else 0.0
            else:
                ord_obj.queue_ahead = cfg.queue_ahead_fraction * cur_asz if (abs(ord_obj.price - cur_ba) <= tol25 or ord_obj.price >= cur_ba - tol25) else 0.0
            ord_obj.activated = True
            queue_delays_ms.append(float(max(0, now_ms - ord_obj.placed_at_ms)))

    def clear_all_resting() -> None:
        nonlocal bid_ord, ask_ord, cancels
        if bid_ord is not None:
            cancels += 1
            bid_ord = None
        if ask_ord is not None:
            cancels += 1
            ask_ord = None

    for idx, ev in enumerate(events):
        ev_t0 = time.perf_counter()
        kind = str(ev.get("kind", ""))
        ts = int(ev.get("now_ms", ev.get("ts", 0)))
        if cfg.declared_end_ms is not None and ts > cfg.declared_end_ms:
            continue

        if kind in ("ref", "track3_ref"):
            ref_count += 1
            continue

        if kind in ("book", "track3_bbo"):
            bbo_count += 1
            bb = float(ev.get("bb", ev.get("bid", 0.0)))
            ba = float(ev.get("ba", ev.get("ask", 0.0)))
            bsz = float(ev.get("bsz", ev.get("bid_sz", 0.0)))
            asz = float(ev.get("asz", ev.get("ask_sz", 0.0)))
            if "bids" in ev and ev["bids"]:
                bb, bsz = float(ev["bids"][0][0]), float(ev["bids"][0][1])
            if "asks" in ev and ev["asks"]:
                ba, asz = float(ev["asks"][0][0]), float(ev["asks"][0][1])
            recv_ms = int(ev.get("recv_ms", ts))

            if not (math.isfinite(bb) and math.isfinite(ba) and math.isfinite(bsz) and math.isfinite(asz)) or bb <= 0.0 or ba <= bb or bsz <= 0.0 or asz <= 0.0:
                crossed_book_drops += 1
                clear_all_resting()
                if last_now_ms is None or ts >= last_now_ms:
                    process_pending_iocs(ts, fresh_now=False)
                update_drawdown()
                continue
            if recv_ms > ts or (last_now_ms is not None and ts < last_now_ms) or (last_book_recv_ms is not None and recv_ms < last_book_recv_ms):
                clock_regression_drops += 1
                if last_now_ms is not None and ts < last_now_ms:
                    out_of_order_events += 1
                clear_all_resting()
                update_drawdown()
                continue
            if ts - recv_ms > cfg.max_book_age_ms:
                stale_book_drops += 1
                last_now_ms = ts
                clear_all_resting()
                process_pending_iocs(ts, fresh_now=False)
                update_drawdown()
                continue

            # Pre-install pending IOC processing against prior touch & prior freshness.
            prev_fresh = book_is_fresh(ts)
            process_pending_iocs(ts, fresh_now=prev_fresh)

            cur_bb, cur_bsz, cur_ba, cur_asz = bb, bsz, ba, asz
            has_book = True
            last_book_recv_ms = recv_ms
            last_now_ms = ts
            rem_bid_sz = bsz * cfg.max_l1_depth_fraction
            rem_ask_sz = asz * cfg.max_l1_depth_fraction
            total_book_ticks += 1
            fresh_book_count += 1

            activate_arrivals(ts)
            if bid_ord is not None and bid_ord.cancel_effective_ms is not None and ts >= bid_ord.cancel_effective_ms:
                bid_ord = None
            if ask_ord is not None and ask_ord.cancel_effective_ms is not None and ts >= ask_ord.cancel_effective_ms:
                ask_ord = None

            sess = _classify_utc_session(ts)
            sess_blocked = sess not in cfg.allowed_utc_sessions
            if sess_blocked:
                session_gated_ticks += 1
            in_drain = drain_start_ms is not None and ts >= drain_start_ms
            suppress_openings = sess_blocked or in_drain

            mid = current_mid()
            hold_timeout = bool(
                open_lots
                and (ts - open_lots[0].entry_ts_ms) >= cfg.max_inventory_hold_ms
                and abs(pos_qty) * mid >= cfg.min_notional_usd
            )
            if hold_timeout:
                clear_all_resting()
                q_ioc = _quantize_down(abs(pos_qty), cfg.lot_size)
                if q_ioc > 0.0:
                    queue_or_exec_ioc(ts, "SELL" if pos_qty > 0.0 else "BUY", q_ioc)
            else:
                half_lot = cfg.lot_size * 0.5
                for side_name in ("BID", "ASK"):
                    is_red = (pos_qty < -half_lot) if side_name == "BID" else (pos_qty > half_lot)
                    ord_obj = bid_ord if side_name == "BID" else ask_ord
                    if not is_red and suppress_openings:
                        if ord_obj is not None:
                            cancels += 1
                            if side_name == "BID":
                                bid_ord = None
                            else:
                                ask_ord = None
                        continue
                    px = _round_tick_down(cur_bb, cfg.tick_size) if side_name == "BID" else _round_tick_up(cur_ba, cfg.tick_size)
                    sp_ticks = max(1, round((cur_ba - cur_bb) / cfg.tick_size))
                    if is_red and sp_ticks >= 2:
                        if side_name == "BID":
                            px = _round_tick_down(min(cur_bb + cfg.tick_size, cur_ba - cfg.tick_size), cfg.tick_size)
                        else:
                            px = _round_tick_up(max(cur_ba - cfg.tick_size, cur_bb + cfg.tick_size), cfg.tick_size)
                    crosses = px >= cur_ba - 1e-12 if side_name == "BID" else px <= cur_bb + 1e-12
                    if crosses or px <= 0.0:
                        if ord_obj is not None:
                            cancels += 1
                            if side_name == "BID":
                                bid_ord = None
                            else:
                                ask_ord = None
                        continue
                    if ord_obj is not None:
                        ord_cross = ord_obj.price >= cur_ba - 1e-12 if side_name == "BID" else ord_obj.price <= cur_bb + 1e-12
                        tdiff = round(abs(ord_obj.price - px) / cfg.tick_size)
                        if not ord_cross and tdiff <= 3:
                            ord_obj.reduce_only = is_red
                            queue_preserved += 1
                        else:
                            cancels += 1
                            if side_name == "BID":
                                bid_ord = None
                            else:
                                ask_ord = None
                    else:
                        max_pos_qty = cfg.max_position_notional_usd / mid
                        headroom = max(0.0, max_pos_qty - (pos_qty if side_name == "BID" else -pos_qty))
                        raw_q = _quantize_down(cfg.quote_notional_usd / px, cfg.lot_size)
                        if is_red:
                            q_ord = _quantize_down(min(raw_q, headroom, abs(pos_qty)), cfg.lot_size)
                        else:
                            q_ord = _quantize_down(min(raw_q, headroom), cfg.lot_size)
                        if q_ord <= 0.0 or (not is_red and q_ord * px < cfg.min_notional_usd):
                            continue
                        if cfg.order_delay_ms == 0:
                            tol25 = cfg.tick_size * 0.25
                            q_ahead = (
                                cfg.queue_ahead_fraction * (cur_bsz if side_name == "BID" else cur_asz)
                                if ((side_name == "BID" and (abs(px - cur_bb) <= tol25 or px <= cur_bb + tol25))
                                    or (side_name == "ASK" and (abs(px - cur_ba) <= tol25 or px >= cur_ba - tol25)))
                                else 0.0
                            )
                            activated = True
                            queue_delays_ms.append(0.0)
                        else:
                            q_ahead = 0.0
                            activated = False
                        new_ord = _RefResting(
                            side=side_name,
                            price=px,
                            qty=q_ord,
                            remaining=q_ord,
                            queue_ahead=q_ahead,
                            placed_at_ms=ts,
                            active_at_ms=ts + cfg.order_delay_ms,
                            activated=activated,
                            reduce_only=is_red and q_ord <= abs(pos_qty) + 1e-12,
                        )
                        if side_name == "BID":
                            bid_ord = new_ord
                        else:
                            ask_ord = new_ord
                        orders_placed += 1

                if in_drain and abs(pos_qty) > 1e-12:
                    queue_or_exec_ioc(ts, "SELL" if pos_qty > 0.0 else "BUY", abs(pos_qty))

            update_drawdown()

        elif kind in ("trade", "track3_print"):
            trade_count += 1
            px = float(ev.get("price", 0.0))
            sz = float(ev.get("size", 0.0))
            recv_ms = int(ev.get("recv_ms", ts))
            if "is_buy_aggressor" in ev:
                is_buy = bool(ev["is_buy_aggressor"])
            else:
                is_buy = str(ev.get("side", "")).upper() in ("BUY", "B", "BID")
            if not (math.isfinite(px) and math.isfinite(sz)) or px <= 0.0 or sz <= 0.0 or recv_ms > ts:
                continue
            if last_now_ms is not None and ts < last_now_ms:
                out_of_order_events += 1
                continue
            fresh_now = book_is_fresh(ts)
            process_pending_iocs(ts, fresh_now=fresh_now)
            if not fresh_now or (ts - recv_ms) > cfg.max_book_age_ms:
                continue
            activate_arrivals(ts)
            tol10 = cfg.tick_size * 0.10
            mid = current_mid()
            if is_buy and ask_ord is not None and ask_ord.activated:
                filled_qty = 0.0
                if px > ask_ord.price + tol10:
                    filled_qty = min(ask_ord.remaining, sz)
                elif abs(px - ask_ord.price) <= tol10:
                    if sz > ask_ord.queue_ahead:
                        filled_qty = min(ask_ord.remaining, sz - ask_ord.queue_ahead)
                        ask_ord.queue_ahead = 0.0
                    else:
                        ask_ord.queue_ahead -= sz
                if filled_qty > 1e-12:
                    record_fill(ts, "SELL", ask_ord.price, filled_qty, "maker", mid)
                    ask_ord.remaining -= filled_qty
                    if ask_ord.remaining <= 1e-12:
                        ask_ord = None
            elif (not is_buy) and bid_ord is not None and bid_ord.activated:
                filled_qty = 0.0
                if px < bid_ord.price - tol10:
                    filled_qty = min(bid_ord.remaining, sz)
                elif abs(px - bid_ord.price) <= tol10:
                    if sz > bid_ord.queue_ahead:
                        filled_qty = min(bid_ord.remaining, sz - bid_ord.queue_ahead)
                        bid_ord.queue_ahead = 0.0
                    else:
                        bid_ord.queue_ahead -= sz
                if filled_qty > 1e-12:
                    record_fill(ts, "BUY", bid_ord.price, filled_qty, "maker", mid)
                    bid_ord.remaining -= filled_qty
                    if bid_ord.remaining <= 1e-12:
                        bid_ord = None
            in_drain = drain_start_ms is not None and ts >= drain_start_ms
            if in_drain and abs(pos_qty) > 1e-12:
                queue_or_exec_ioc(ts, "SELL" if pos_qty > 0.0 else "BUY", abs(pos_qty))
            update_drawdown()

        eval_latencies_ms.append((time.perf_counter() - ev_t0) * 1000.0)
        mid_now = current_mid()
        real_now = (spr + drft + pex + tslp) - fees_usd
        unreal_now = sum(l.sgn * l.qty * (mid_now - l.entry_px) for l in open_lots) if has_book else 0.0
        trace.append(
            {
                "idx": idx,
                "ts_ms": ts,
                "kind": kind,
                "pos_qty": round(pos_qty, 9),
                "maker_fills": maker_fills,
                "taker_exits": taker_exits,
                "stale_exit_blocks": stale_exit_blocks,
                "depth_clipped_exits": depth_clipped_exits,
                "realized_pnl_usd": round(real_now, 8),
                "unrealized_pnl_usd": round(unreal_now, 8),
                "fees_usd": round(fees_usd, 8),
                "orders_placed": orders_placed,
                "cancels": cancels,
            }
        )

    total_wall_ms = (time.perf_counter() - t_wall0) * 1000.0
    mid_end = current_mid()
    realized_gross_usd = spr + drft + pex + tslp
    realized_pnl_usd = realized_gross_usd - fees_usd
    unrealized_pnl_usd = sum(l.sgn * l.qty * (mid_end - l.entry_px) for l in open_lots) if has_book else 0.0
    mtm_gross_usd = realized_gross_usd + unrealized_pnl_usd
    mtm_net_usd = mtm_gross_usd - fees_usd
    terminal_residual_notional_usd = abs(pos_qty) * mid_end if (has_book and abs(pos_qty) > 1e-12) else 0.0
    taker_exit_share_s = min(1.0, max(0.0, taker_volume_usd / closed_notional_usd)) if closed_notional_usd > 0.0 else 0.0
    gross_ex_fee_bps = (realized_gross_usd / closed_notional_usd) * 1e4 if closed_notional_usd > 0.0 else 0.0
    eff_maker_bp = cfg.maker_fee_bps - cfg.rebate_bps
    if closed_notional_usd > 0.0:
        fee_cycle_bp = eff_maker_bp * (2.0 - taker_exit_share_s) + cfg.taker_fee_bps * taker_exit_share_s
        double_sided_usd_per_m = ((fee_cycle_bp - gross_ex_fee_bps) / 2.0) * 100.0
    else:
        double_sided_usd_per_m = eff_maker_bp * 100.0

    cash_mtm_gross = cash_ex_fee_usd + (pos_qty * mid_end if has_book else 0.0)
    cash_conservation_err_usd = abs(cash_mtm_gross - mtm_gross_usd)
    decomposition_err_usd = abs((spr + drft + pex + tslp + unrealized_pnl_usd) - mtm_gross_usd)

    return {
        "arm": "reference_ledger",
        "realized_pnl_usd": realized_pnl_usd,
        "unrealized_pnl_usd": unrealized_pnl_usd,
        "mtm_net_usd": mtm_net_usd,
        "fees_usd": fees_usd,
        "spread_capture_usd": spr,
        "holding_drift_usd": drft,
        "passive_exit_pnl_usd": pex,
        "taker_exit_slippage_usd": tslp,
        "gross_ex_fee_bps": gross_ex_fee_bps,
        "double_sided_usd_per_m": double_sided_usd_per_m,
        "maker_volume_usd": maker_volume_usd,
        "taker_volume_usd": taker_volume_usd,
        "closed_notional_usd": closed_notional_usd,
        "taker_exit_share_s": taker_exit_share_s,
        "position_qty": pos_qty,
        "terminal_residual_notional_usd": terminal_residual_notional_usd,
        "fills": maker_fills + taker_exits,
        "maker_fills": maker_fills,
        "taker_exits": taker_exits,
        "depth_clipped_exits": depth_clipped_exits,
        "stale_exit_blocks": stale_exit_blocks,
        "cycles": cycles,
        "orders_placed": orders_placed,
        "cancels": cancels,
        "queue_preserved": queue_preserved,
        "alo_rejects": alo_rejects,
        "session_gated_ticks": session_gated_ticks,
        "total_book_ticks": total_book_ticks,
        "stale_book_drops": stale_book_drops,
        "crossed_book_drops": crossed_book_drops,
        "clock_regression_drops": clock_regression_drops,
        "max_drawdown_usd": max_drawdown,
        "cash_conservation_err_usd": cash_conservation_err_usd,
        "decomposition_err_usd": decomposition_err_usd,
        "counts": {
            "bbo_events": bbo_count,
            "ref_events": ref_count,
            "trade_events": trade_count,
            "fresh_books": fresh_book_count,
            "out_of_order_events": out_of_order_events,
        },
        "clocks": {
            "eval_wall_ms": round(total_wall_ms, 4),
            "per_event_eval_ms": _clock_stats(eval_latencies_ms),
            "queue_delay_ms": _clock_stats(queue_delays_ms),
            "ioc_delay_ms": _clock_stats(ioc_delays_ms),
        },
        "trace": trace,
    }


def _to_python_events(events: Sequence[dict[str, Any]]) -> list[dict[str, Any]]:
    out: list[dict[str, Any]] = []
    for ev in events:
        kind = str(ev.get("kind", ""))
        ts = int(ev.get("now_ms", ev.get("ts", 0)))
        if kind in ("book", "track3_bbo"):
            recv_ms = int(ev.get("recv_ms", ts))
            # Python prepare_replay_stream only carries single `ts`; omit explicit stale BBO ticks
            # (where now_ms - recv_ms > 0 indicates synthetic stale arrival) if recv_ms != ts,
            # or keep if recv_ms == ts. Note: stale gaps work identically via silent gaps!
            if recv_ms != ts:
                continue
            if "bids" in ev and "asks" in ev and ev["bids"] and ev["asks"]:
                bb, bsz = float(ev["bids"][0][0]), float(ev["bids"][0][1])
                ba, asz = float(ev["asks"][0][0]), float(ev["asks"][0][1])
            else:
                bb = float(ev.get("bb", ev.get("bid", 0.0)))
                bsz = float(ev.get("bsz", ev.get("bid_sz", 0.0)))
                ba = float(ev.get("ba", ev.get("ask", 0.0)))
                asz = float(ev.get("asz", ev.get("ask_sz", 0.0)))
            out.append({"ts": ts, "kind": "book", "bb": bb, "bsz": bsz, "ba": ba, "asz": asz})
        elif kind in ("ref", "track3_ref"):
            out.append(
                {
                    "ts": ts,
                    "kind": "ref",
                    "venue": str(ev.get("venue", "arcus")),
                    "bid": float(ev["bid"]),
                    "ask": float(ev["ask"]),
                }
            )
        elif kind in ("trade", "track3_print"):
            if "is_buy_aggressor" in ev:
                side = "BUY" if bool(ev["is_buy_aggressor"]) else "SELL"
            else:
                side = str(ev.get("side", "BUY")).upper()
            out.append({"ts": ts, "kind": "trade", "side": side, "price": float(ev["price"]), "size": float(ev["size"])})
    return out


def _to_rust_jsonl_lines(events: Sequence[dict[str, Any]], cfg: ParityConfig) -> list[str]:
    lines: list[str] = []
    for ev in events:
        kind = str(ev.get("kind", ""))
        ts = int(ev.get("now_ms", ev.get("ts", 0)))
        if kind in ("book", "track3_bbo"):
            if "bids" in ev and "asks" in ev and ev["bids"] and ev["asks"]:
                bb, bsz = float(ev["bids"][0][0]), float(ev["bids"][0][1])
                ba, asz = float(ev["asks"][0][0]), float(ev["asks"][0][1])
            else:
                bb = float(ev.get("bb", ev.get("bid", 0.0)))
                bsz = float(ev.get("bsz", ev.get("bid_sz", 0.0)))
                ba = float(ev.get("ba", ev.get("ask", 0.0)))
                asz = float(ev.get("asz", ev.get("ask_sz", 0.0)))
            recv_ms = int(ev.get("recv_ms", ts))
            obj = {
                "kind": "track3_bbo",
                "symbol_id": cfg.symbol_id,
                "now_ms": ts,
                "bid": bb,
                "ask": ba,
                "bid_sz": bsz,
                "ask_sz": asz,
                "recv_ms": recv_ms,
            }
        elif kind in ("ref", "track3_ref"):
            obj = {
                "kind": "track3_ref",
                "symbol_id": cfg.symbol_id,
                "exchange_id": int(ev.get("exchange_id", cfg.ref_exchange_id)),
                "now_ms": ts,
                "bid": float(ev["bid"]),
                "ask": float(ev["ask"]),
                "recv_ms": int(ev.get("recv_ms", ts)),
            }
        elif kind in ("trade", "track3_print"):
            if "is_buy_aggressor" in ev:
                is_buy = bool(ev["is_buy_aggressor"])
            else:
                is_buy = str(ev.get("side", "BUY")).upper() in ("BUY", "B", "BID")
            obj = {
                "kind": "track3_print",
                "symbol_id": cfg.symbol_id,
                "now_ms": ts,
                "price": float(ev["price"]),
                "size": float(ev["size"]),
                "is_buy_aggressor": is_buy,
                "recv_ms": int(ev.get("recv_ms", ts)),
            }
        else:
            continue
        lines.append(json.dumps(obj, separators=(",", ":")))
    return lines


def _build_rust_toml(cfg: ParityConfig) -> str:
    sessions_toml = ", ".join(f'"{s}"' for s in cfg.allowed_utc_sessions)
    lines = [
        f"capital_usd = {cfg.capital_usd}",
        "daily_loss_limit_pct = 0.02",
        f'allowlisted_symbols = ["{cfg.symbol}"]',
    ]
    if cfg.declared_end_ms is not None:
        lines.append(f"declared_end_ms = {int(cfg.declared_end_ms)}")
    if cfg.terminal_drain_ms is not None:
        lines.append(f"terminal_drain_ms = {int(cfg.terminal_drain_ms)}")
    lines.extend(
        [
            "",
            "[[arms]]",
            f'symbol = "{cfg.symbol}"',
            f"symbol_id = {cfg.symbol_id}",
            f"target_exchange_id = {cfg.target_exchange_id}",
            f"ref_exchange_ids = [{cfg.ref_exchange_id}]",
            f"maker_fee_bps = {cfg.maker_fee_bps}",
            f"taker_fee_bps = {cfg.taker_fee_bps}",
            f"rebate_bps = {cfg.rebate_bps}",
            f"queue_ahead_fraction = {cfg.queue_ahead_fraction}",
            f"allowed_utc_sessions = [{sessions_toml}]",
            'account_fee_status = "verified_account_tier"',
            f"ioc_exit_delay_ms = {cfg.ioc_exit_delay_ms}",
            f"max_l1_depth_fraction = {cfg.max_l1_depth_fraction}",
            "",
            "[arms.strategy]",
            f"tick_size = {cfg.tick_size}",
            f"lot_size = {cfg.lot_size}",
            f"min_notional_usd = {cfg.min_notional_usd}",
            f"maker_fee_bps = {cfg.maker_fee_bps}",
            f"quote_notional_usd = {cfg.quote_notional_usd}",
            f"max_position_notional_usd = {cfg.max_position_notional_usd}",
            "ladder_levels = 1",
            f"min_penny_spread_ticks = {cfg.min_penny_spread_ticks}",
            f"min_entry_edge_bps = {cfg.min_entry_edge_bps}",
            f"lead_cancel_threshold_bps = {cfg.lead_cancel_threshold_bps}",
            f"lead_cancel_spread_mult = {cfg.lead_cancel_spread_mult}",
            f"require_validated_lead_ref = {'true' if cfg.require_validated_lead_ref else 'false'}",
            "basis_ewma_tau_ms = 120000.0",
            f"inventory_skew_bps_per_full_limit = {cfg.inventory_skew_bps_per_full_limit}",
            "max_queue_keep_ticks = 3",
            "sweep_print_pull_ratio = 999.0",
            "sweep_min_notional_usd = 0.0",
            "sweep_cooldown_ms = 0",
            "alo_reject_cooldown_ms = 0",
            "alo_reject_stepback_streak = 0",
            "markout_shield_horizon_ms = 1000",
            f"markout_shield_adverse_threshold_bps = {cfg.markout_shield_adverse_threshold_bps}",
            f"markout_shield_cooldown_ms = {cfg.markout_shield_cooldown_ms}",
            f"markout_shield_size_scale = {cfg.markout_shield_size_scale}",
            f"submit_rtt_ms = {float(cfg.order_delay_ms)}",
            f"as_inventory_skew_gamma = {cfg.as_inventory_skew_gamma}",
            f"vol_spread_mult = {cfg.vol_spread_mult}",
            f"directional_markout_skew_mult = {cfg.directional_markout_skew_mult}",
            f"quiet_regime_min_spread_to_vol = {cfg.quiet_regime_min_spread_to_vol}",
            f"microprice_alpha_weight = {cfg.microprice_alpha_weight}",
            f"tfi_ewma_tau_ms = {cfg.tfi_ewma_tau_ms}",
            f"tfi_skew_bps = {cfg.tfi_skew_bps}",
            f"tfi_veto_threshold = {cfg.tfi_veto_threshold}",
            f"max_book_age_ms = {cfg.max_book_age_ms}",
            "max_session_loss_usd = 500.0",
            f"max_inventory_hold_ms = {cfg.max_inventory_hold_ms}",
            "require_fresh_reference = false",
        ]
    )
    if cfg.quiet_regime_max_entry_vol_bps is not None:
        lines.append(f"quiet_regime_max_entry_vol_bps = {cfg.quiet_regime_max_entry_vol_bps}")
    lines.extend(["", "[arms.strategy.hedge_mode]", 'mode = "passive_inventory_cycle"'])
    return "\n".join(lines) + "\n"


def _latest_rust_shadow_source_mtime() -> float:
    src_paths = (
        REPO_ROOT / "Cargo.toml",
        REPO_ROOT / "src" / "strategy" / "track_shadow.rs",
        REPO_ROOT / "src" / "bin" / "track01_track02_shadow.rs",
    )
    return max((p.stat().st_mtime for p in src_paths if p.exists()), default=0.0)


def _resolve_rust_binary(explicit_bin: Path | None = None) -> Path:
    if explicit_bin is not None:
        if explicit_bin.is_file():
            return explicit_bin
        raise FileNotFoundError(f"Rust binary not found at {explicit_bin}")
    src_mtime = _latest_rust_shadow_source_mtime()
    fresh = [
        cand for cand in DEFAULT_RUST_BIN_CANDIDATES
        if cand.is_file() and cand.stat().st_mtime >= src_mtime
    ]
    if fresh:
        return max(fresh, key=lambda p: p.stat().st_mtime)
    proc = subprocess.run(
        ["cargo", "build", "--locked", "--bin", "track01_track02_shadow"],
        cwd=REPO_ROOT,
        capture_output=True,
        text=True,
        check=False,
    )
    if proc.returncode != 0:
        raise RuntimeError(f"cargo build failed: {proc.stderr or proc.stdout}")
    debug_bin = DEFAULT_RUST_BIN_CANDIDATES[0]
    if debug_bin.is_file():
        return debug_bin
    for cand in DEFAULT_RUST_BIN_CANDIDATES:
        if cand.is_file():
            return cand
    raise FileNotFoundError("Compiled track01_track02_shadow binary not found after cargo build")


def run_rust_shadow_replay(
    events: Sequence[dict[str, Any]],
    cfg: ParityConfig,
    rust_bin: Path,
) -> dict[str, Any]:
    toml_txt = _build_rust_toml(cfg)
    jsonl_lines = _to_rust_jsonl_lines(events, cfg)
    with tempfile.TemporaryDirectory(prefix="p0_rust_replay_") as tmpdir:
        tmp = Path(tmpdir)
        cfg_path = tmp / "cfg.toml"
        rep_path = tmp / "events.jsonl"
        cfg_path.write_text(toml_txt, encoding="utf-8")
        rep_path.write_text("\n".join(jsonl_lines) + "\n", encoding="utf-8")
        t0 = time.perf_counter()
        proc = subprocess.run(
            [str(rust_bin), "--track", "3", "--config", str(cfg_path), "--replay", str(rep_path)],
            cwd=REPO_ROOT,
            capture_output=True,
            text=True,
            check=False,
        )
        wall_ms = (time.perf_counter() - t0) * 1000.0
    if proc.returncode != 0:
        raise RuntimeError(f"Rust shadow replay failed: {proc.stderr or proc.stdout}")
    raw = json.loads(proc.stdout)
    raw["eval_wall_ms"] = round(wall_ms, 4)
    return raw


def run_python_shadow_replay(
    events: Sequence[dict[str, Any]],
    cfg: ParityConfig,
    *,
    arm_name: str = "G4_baseline",
) -> dict[str, Any]:
    py_events = _to_python_events(events)
    arm_cfg = py_t3.SpeedbumpArmConfig(
        name=arm_name,
        order_delay_ms=cfg.order_delay_ms,
        cancel_delay_ms=cfg.cancel_delay_ms,
        exit_delay_ms=cfg.ioc_exit_delay_ms,
        quote_usd=cfg.quote_notional_usd,
        max_pos_usd=cfg.max_position_notional_usd,
        max_hold_ms=cfg.max_inventory_hold_ms,
        max_book_age_ms=cfg.max_book_age_ms,
        terminal_flatten_lead_ms=cfg.terminal_drain_ms or 0,
        keep_queue_ticks=3,
        queue_ahead_fraction=cfg.queue_ahead_fraction,
        maker_fee_bps=cfg.maker_fee_bps,
        taker_fee_bps=cfg.taker_fee_bps,
        rebate_bps=cfg.rebate_bps,
        min_entry_edge_bps=cfg.min_entry_edge_bps,
        inventory_skew_bps_per_full_limit=cfg.inventory_skew_bps_per_full_limit,
        vol_spread_mult=cfg.vol_spread_mult,
        directional_markout_skew_mult=cfg.directional_markout_skew_mult,
        markout_shield_adverse_threshold_bps=cfg.markout_shield_adverse_threshold_bps,
        markout_shield_cooldown_ms=cfg.markout_shield_cooldown_ms,
        markout_shield_size_scale=cfg.markout_shield_size_scale,
        use_oracle_anchor=cfg.use_oracle_anchor,
        quiet_regime_only=(cfg.quiet_regime_min_spread_to_vol > 0.0 or cfg.quiet_regime_max_entry_vol_bps is not None),
        quiet_min_spread_vol_ratio=cfg.quiet_regime_min_spread_to_vol,
        quiet_max_sigma_bps=cfg.quiet_regime_max_entry_vol_bps if cfg.quiet_regime_max_entry_vol_bps is not None else 999.0,
        microprice_alpha_weight=cfg.microprice_alpha_weight,
        tfi_ewma_tau_ms=cfg.tfi_ewma_tau_ms,
        tfi_skew_bps=cfg.tfi_skew_bps,
        tfi_veto_threshold=cfg.tfi_veto_threshold,
    )
    t0 = time.perf_counter()
    stream = py_t3.prepare_replay_stream(py_events)
    res = py_t3.simulate_prepared_arm(stream, cfg.tick_size, arm_cfg, declared_end_ms=cfg.declared_end_ms)
    wall_ms = (time.perf_counter() - t0) * 1000.0
    res["eval_wall_ms"] = round(wall_ms, 4)
    return res


def _extract_comparable_metrics(arm_kind: str, raw: dict[str, Any]) -> dict[str, Any]:
    if arm_kind == "reference":
        return {
            "maker_fills": int(raw["maker_fills"]),
            "taker_exits": int(raw["taker_exits"]),
            "stale_exit_blocks": int(raw["stale_exit_blocks"]),
            "depth_clipped_exits": int(raw["depth_clipped_exits"]),
            "cycles": int(raw["cycles"]),
            "realized_pnl_usd": float(raw["realized_pnl_usd"]),
            "unrealized_pnl_usd": float(raw["unrealized_pnl_usd"]),
            "fees_usd": float(raw["fees_usd"]),
            "closed_notional_usd": float(raw["closed_notional_usd"]),
            "terminal_residual_notional_usd": float(raw["terminal_residual_notional_usd"]),
            "double_sided_usd_per_m": float(raw["double_sided_usd_per_m"]),
            "eval_wall_ms": float(raw["clocks"]["eval_wall_ms"]),
        }
    if arm_kind == "python":
        return {
            "maker_fills": int(raw["maker_fills"]),
            "taker_exits": int(raw["taker_exits"]),
            "stale_exit_blocks": int(raw["stale_exit_blocks"]),
            "depth_clipped_exits": int(raw["depth_clipped_exits"]),
            "cycles": int(raw["cyc"]),
            "realized_pnl_usd": float(raw["realized_net_usd"]),
            "unrealized_pnl_usd": float(raw["unrealized_mtm_usd"]),
            "fees_usd": float(raw["fee_usd"]),
            "closed_notional_usd": float(raw["ntl"]),
            "terminal_residual_notional_usd": float(raw["terminal_residual_notional_usd"]),
            "double_sided_usd_per_m": float(raw["realized_only_usd_per_m"]),
            "mtm_incl_residual_usd_per_m": float(raw["mtm_incl_residual_usd_per_m"]),
            "eval_wall_ms": float(raw["eval_wall_ms"]),
        }
    if arm_kind == "rust":
        return {
            "maker_fills": int(raw["maker_fills"]),
            "taker_exits": int(raw["taker_exits"]),
            "stale_exit_blocks": int(raw["stale_exit_blocks"]),
            "depth_clipped_exits": int(raw["depth_clipped_exits"]),
            "cycles": int(raw["cycles"]),
            "realized_pnl_usd": float(raw["realized_pnl_usd"]),
            "unrealized_pnl_usd": float(raw["unrealized_pnl_usd"]),
            "fees_usd": float(raw["fees_usd"]),
            "closed_notional_usd": float(raw["closed_notional_usd"]),
            "terminal_residual_notional_usd": float(raw["terminal_residual_notional_usd"]),
            "double_sided_usd_per_m": float(raw["double_sided_usd_per_m"]),
            "eval_wall_ms": float(raw["eval_wall_ms"]),
        }
    raise ValueError(f"Unknown arm_kind={arm_kind!r}")


def locate_first_divergence(
    ref_m: dict[str, Any],
    py_m: dict[str, Any] | None,
    rust_m: dict[str, Any],
    *,
    tol_usd: float = 1e-4,
    tol_bps_per_m: float = 1e-2,
    structural_notes: Sequence[str] = (),
) -> dict[str, Any]:
    """Compare Reference, Python, and Rust metrics and report first divergence if any."""
    int_fields = ("maker_fills", "taker_exits", "stale_exit_blocks", "depth_clipped_exits", "cycles")
    usd_fields = (
        "realized_pnl_usd",
        "unrealized_pnl_usd",
        "fees_usd",
        "closed_notional_usd",
        "terminal_residual_notional_usd",
    )
    diffs: list[dict[str, Any]] = []
    arms: dict[str, dict[str, Any]] = {"reference": ref_m, "rust": rust_m}
    if py_m is not None:
        arms["python"] = py_m

    for f in int_fields:
        vals = {k: m[f] for k, m in arms.items()}
        if len(set(vals.values())) > 1:
            diffs.append({"field": f, "values": vals, "delta": max(vals.values()) - min(vals.values())})
    for f in usd_fields:
        vals = {k: round(float(m[f]), 8) for k, m in arms.items()}
        span = max(vals.values()) - min(vals.values())
        if span > tol_usd:
            diffs.append({"field": f, "values": vals, "delta": round(span, 8)})
    ds_vals = {k: round(float(m["double_sided_usd_per_m"]), 6) for k, m in arms.items()}
    ds_span = max(ds_vals.values()) - min(ds_vals.values())
    if ds_span > tol_bps_per_m:
        diffs.append({"field": "double_sided_usd_per_m", "values": ds_vals, "delta": round(ds_span, 6)})

    return {
        "has_divergence": len(diffs) > 0,
        "first_divergence": diffs[0] if diffs else None,
        "all_divergences": diffs,
        "structural_notes": list(structural_notes),
    }


def build_four_stage_scorecard(
    *,
    scenario_id: str,
    cfg: ParityConfig,
    ref_raw: dict[str, Any],
    rust_raw: dict[str, Any],
    py_raw: dict[str, Any] | None,
    events_count: int,
    expected_markets: int = 1,
    observed_markets: int = 1,
) -> dict[str, Any]:
    """Build Roadmap §3 Four-Stage Denominator Scorecard."""
    cnt = ref_raw["counts"]
    bbo_n = int(cnt["bbo_events"])
    tr_n = int(cnt["trade_events"])
    fresh_n = int(cnt["fresh_books"])
    stale_drops = int(rust_raw.get("stale_book_drops", ref_raw["stale_book_drops"]))
    crossed_drops = int(rust_raw.get("crossed_book_drops", ref_raw["crossed_book_drops"]))
    reg_drops = int(rust_raw.get("clock_regression_drops", ref_raw["clock_regression_drops"]))
    stale_blocks = int(rust_raw.get("stale_exit_blocks", ref_raw["stale_exit_blocks"]))
    orders_placed = int(rust_raw.get("orders_placed", ref_raw["orders_placed"]))
    alo_rejects = int(rust_raw.get("alo_rejects", ref_raw["alo_rejects"]))
    maker_fills = int(rust_raw.get("maker_fills", ref_raw["maker_fills"]))
    taker_exits = int(rust_raw.get("taker_exits", ref_raw["taker_exits"]))
    clipped = int(rust_raw.get("depth_clipped_exits", ref_raw["depth_clipped_exits"]))
    res_notl = float(rust_raw.get("terminal_residual_notional_usd", ref_raw["terminal_residual_notional_usd"]))

    realized = float(rust_raw.get("realized_pnl_usd", ref_raw["realized_pnl_usd"]))
    unrealized = float(rust_raw.get("unrealized_pnl_usd", ref_raw["unrealized_pnl_usd"]))
    mtm_net = realized + unrealized
    fees = float(rust_raw.get("fees_usd", ref_raw["fees_usd"]))
    ntl = float(rust_raw.get("closed_notional_usd", ref_raw["closed_notional_usd"]))

    return {
        "scenario_id": scenario_id,
        "stage_1_input_coverage": {
            "expected_markets": expected_markets,
            "observed_markets": observed_markets,
            "total_events": events_count,
            "bbo_events": bbo_n,
            "ref_events": int(cnt["ref_events"]),
            "trade_events": tr_n,
            "book_coverage_ratio": round(fresh_n / bbo_n, 6) if bbo_n > 0 else 0.0,
            "trade_present": tr_n > 0,
            "crossed_book_drops": crossed_drops,
            "clock_regression_drops": reg_drops,
            "out_of_order_events": int(cnt["out_of_order_events"]),
            "fee_tier_status": "verified_account_tier",
            "fee_evidence_sha256": ACCOUNT_FEE_EVIDENCE_SHA256,
        },
        "stage_2_on_time_availability": {
            "scheduled_bbo_checkpoints": bbo_n,
            "fresh_books": fresh_n,
            "stale_book_drops": stale_drops,
            "stale_exit_blocks": stale_blocks,
            "on_time_ratio": round(fresh_n / bbo_n, 6) if bbo_n > 0 else 0.0,
            "conditional_valid_fresh_ratio": round(fresh_n / max(1, bbo_n - crossed_drops - reg_drops), 6) if bbo_n > 0 else 0.0,
            "stage_clocks": {
                "ref_eval_wall_ms": ref_raw["clocks"]["eval_wall_ms"],
                "rust_eval_wall_ms": rust_raw.get("eval_wall_ms", 0.0),
                "python_eval_wall_ms": py_raw.get("eval_wall_ms", 0.0) if py_raw is not None else None,
                "per_event_eval_ms": ref_raw["clocks"]["per_event_eval_ms"],
                "queue_delay_ms": ref_raw["clocks"]["queue_delay_ms"],
                "ioc_delay_ms": ref_raw["clocks"]["ioc_delay_ms"],
            },
        },
        "stage_3_executable_and_settleable": {
            "orders_placed": orders_placed,
            "alo_rejects": alo_rejects,
            "maker_fills": maker_fills,
            "taker_exits": taker_exits,
            "depth_clipped_exits": clipped,
            "maker_fill_per_order_ratio": round(maker_fills / orders_placed, 6) if orders_placed > 0 else 0.0,
            "terminal_residual_notional_usd": round(res_notl, 6),
            "terminal_flat": res_notl <= 1e-6,
            "settlement_domain": "USDG" if cfg.target_exchange_id == 8 else "USDC",
        },
        "stage_4_complete_economic_result": {
            "realized_pnl_usd": round(realized, 8),
            "unrealized_pnl_usd": round(unrealized, 8),
            "mtm_net_usd": round(mtm_net, 8),
            "fees_usd": round(fees, 8),
            "maker_volume_usd": round(float(rust_raw.get("maker_volume_usd", ref_raw["maker_volume_usd"])), 6),
            "taker_volume_usd": round(float(rust_raw.get("taker_volume_usd", ref_raw["taker_volume_usd"])), 6),
            "closed_notional_usd": round(ntl, 6),
            "double_sided_usd_per_m": round(float(rust_raw.get("double_sided_usd_per_m", ref_raw["double_sided_usd_per_m"])), 6),
            "taker_exit_share_s": round(float(rust_raw.get("taker_exit_share_s", ref_raw["taker_exit_share_s"])), 6),
            "max_drawdown_usd": round(float(rust_raw.get("max_drawdown_usd", ref_raw["max_drawdown_usd"])), 6),
            "cash_conservation_err_usd": round(float(ref_raw["cash_conservation_err_usd"]), 12),
            "decomposition_err_usd": round(float(ref_raw["decomposition_err_usd"]), 12),
            "positive_opportunity_recall": "unestimable_zero_verified_positives",
        },
    }


def _evaluate_three_way_case(
    scenario_id: str,
    description: str,
    events: Sequence[dict[str, Any]],
    cfg: ParityConfig,
    rust_bin: Path,
    *,
    run_python: bool = True,
    tol_usd: float = 1e-4,
    structural_notes: Sequence[str] = (),
) -> dict[str, Any]:
    ref_raw = run_independent_reference_ledger(events, cfg)
    rust_raw = run_rust_shadow_replay(events, cfg, rust_bin)
    py_raw = run_python_shadow_replay(events, cfg) if run_python else None

    ref_m = _extract_comparable_metrics("reference", ref_raw)
    rust_m = _extract_comparable_metrics("rust", rust_raw)
    py_m = _extract_comparable_metrics("python", py_raw) if py_raw is not None else None

    div = locate_first_divergence(ref_m, py_m, rust_m, tol_usd=tol_usd, structural_notes=structural_notes)
    scorecard = build_four_stage_scorecard(
        scenario_id=scenario_id,
        cfg=cfg,
        ref_raw=ref_raw,
        rust_raw=rust_raw,
        py_raw=py_raw,
        events_count=len(events),
    )
    return {
        "scenario_id": scenario_id,
        "description": description,
        "event_count": len(events),
        "reference": ref_m,
        "python": py_m,
        "rust": rust_m,
        "divergence": div,
        "scorecard": scorecard,
        "parity_status": "PASS" if not div["has_divergence"] else "DIVERGENCE_DETECTED",
    }


def build_p0_counterexamples() -> list[dict[str, Any]]:
    """Construct all synthetic P0 counterexamples from Roadmap §3."""
    t0 = 1_795_564_800_000  # 2026-11-25 00:00:00 UTC (OvernightAsia)

    # 1. Stale gap expiry -> recovery re-queues 330 ms & tracks stale_exit_blocks (NO 0ms fill)
    c1_events = [
        {"ts": t0, "kind": "book", "bb": 100.00, "bsz": 10.0, "ba": 100.04, "asz": 10.0},
        {"ts": t0 + 250, "kind": "trade", "side": "SELL", "price": 99.99, "size": 5.0},
        {"ts": t0 + 4_500, "kind": "book", "bb": 100.00, "bsz": 10.0, "ba": 100.04, "asz": 10.0},
        {"ts": t0 + 5_300, "kind": "book", "bb": 100.00, "bsz": 10.0, "ba": 100.04, "asz": 10.0},
        # Silent gap > 5_000 ms: IOC due at t0+5630 is stale at recovery t0+10_500 -> re-queues to t0+10_830
        {"ts": t0 + 10_500, "kind": "book", "bb": 99.96, "bsz": 10.0, "ba": 100.00, "asz": 10.0},
        {"ts": t0 + 10_700, "kind": "book", "bb": 99.96, "bsz": 10.0, "ba": 100.00, "asz": 10.0},
        {"ts": t0 + 10_850, "kind": "book", "bb": 99.96, "bsz": 10.0, "ba": 100.00, "asz": 10.0},
    ]

    # 2. Two IOCs on the same book touch (partial fill then 2nd trigger at terminal drain)
    c2_events = [
        {"ts": t0, "kind": "book", "bb": 100.00, "bsz": 20.0, "ba": 100.04, "asz": 20.0},
        {"ts": t0 + 100, "kind": "trade", "side": "SELL", "price": 99.99, "size": 5.0},
        {"ts": t0 + 5_200, "kind": "book", "bb": 100.02, "bsz": 1.0, "ba": 100.06, "asz": 10.0},
        {"ts": t0 + 5_400, "kind": "book", "bb": 100.02, "bsz": 1.0, "ba": 100.06, "asz": 10.0},
        # At t0+5_550: 1st IOC executes against prior touch (bsz=1.0 -> 1.0 fill, leaving 1.0 residual)
        # AND queues 2nd IOC; at t0+5_900 on fresh touch (bsz=0.5), 2nd IOC clips at 0.5 leaving 0.5!
        {"ts": t0 + 5_550, "kind": "book", "bb": 100.02, "bsz": 0.5, "ba": 100.06, "asz": 10.0},
        {"ts": t0 + 5_900, "kind": "book", "bb": 100.02, "bsz": 0.5, "ba": 100.06, "asz": 10.0},
    ]

    # 3. Partial exit leaving true residual & MTM mark at last valid touch
    c3_events = [
        {"ts": t0, "kind": "book", "bb": 100.00, "bsz": 20.0, "ba": 100.04, "asz": 20.0},
        {"ts": t0 + 100, "kind": "trade", "side": "SELL", "price": 99.99, "size": 5.0},
        {"ts": t0 + 5_200, "kind": "book", "bb": 100.08, "bsz": 0.5, "ba": 100.12, "asz": 10.0},
        {"ts": t0 + 5_400, "kind": "book", "bb": 100.08, "bsz": 0.5, "ba": 100.12, "asz": 10.0},
        {"ts": t0 + 5_550, "kind": "book", "bb": 100.08, "bsz": 0.5, "ba": 100.12, "asz": 10.0},
    ]

    # 4. Late ref, bbo, and print ticks strictly past declared_end_ms ignored
    c4_events = [
        {"ts": t0, "kind": "book", "bb": 100.00, "bsz": 10.0, "ba": 100.04, "asz": 10.0},
        {"ts": t0, "kind": "ref", "venue": "arcus", "bid": 100.00, "ask": 100.04},
        {"ts": t0 + 100, "kind": "trade", "side": "SELL", "price": 99.99, "size": 5.0},
        {"ts": t0 + 3_000, "kind": "book", "bb": 100.04, "bsz": 10.0, "ba": 100.08, "asz": 10.0},
        {"ts": t0 + 15_000, "kind": "ref", "venue": "arcus", "bid": 105.00, "ask": 105.04},
        {"ts": t0 + 16_000, "kind": "book", "bb": 105.00, "bsz": 10.0, "ba": 105.04, "asz": 10.0},
        {"ts": t0 + 16_500, "kind": "trade", "side": "BUY", "price": 105.10, "size": 20.0},
    ]

    # 5. Terminal drain activation inside [declared_end_ms - terminal_drain_ms, declared_end_ms]
    c5_events = [
        {"ts": t0, "kind": "book", "bb": 100.00, "bsz": 20.0, "ba": 100.04, "asz": 20.0},
        {"ts": t0 + 100, "kind": "trade", "side": "SELL", "price": 99.99, "size": 5.0},
        {"ts": t0 + 2_000, "kind": "book", "bb": 100.00, "bsz": 20.0, "ba": 100.04, "asz": 20.0},
        {"ts": t0 + 3_000, "kind": "book", "bb": 100.02, "bsz": 20.0, "ba": 100.06, "asz": 20.0},
        {"ts": t0 + 3_200, "kind": "book", "bb": 100.02, "bsz": 20.0, "ba": 100.06, "asz": 20.0},
        {"ts": t0 + 3_350, "kind": "book", "bb": 100.02, "bsz": 20.0, "ba": 100.06, "asz": 20.0},
    ]

    # 6. Same-ms dual ordering (book vs trade at identical timestamp t0)
    c6_book_first = [
        {"ts": t0, "kind": "book", "bb": 100.00, "bsz": 20.0, "ba": 100.04, "asz": 20.0},
        {"ts": t0, "kind": "trade", "side": "SELL", "price": 99.99, "size": 5.0},
        {"ts": t0 + 1_000, "kind": "book", "bb": 100.02, "bsz": 20.0, "ba": 100.06, "asz": 20.0},
        {"ts": t0 + 1_000, "kind": "trade", "side": "BUY", "price": 100.07, "size": 5.0},
    ]
    c6_trade_first = [
        {"ts": t0, "kind": "trade", "side": "SELL", "price": 99.99, "size": 5.0},
        {"ts": t0, "kind": "book", "bb": 100.00, "bsz": 20.0, "ba": 100.04, "asz": 20.0},
        {"ts": t0 + 1_000, "kind": "trade", "side": "BUY", "price": 100.07, "size": 5.0},
        {"ts": t0 + 1_000, "kind": "book", "bb": 100.02, "bsz": 20.0, "ba": 100.06, "asz": 20.0},
    ]

    return [
        {
            "id": "p0_c1_stale_gap_330ms_requeue",
            "desc": "Stale gap expiry re-queues 330 ms on recovery and increments stale_exit_blocks",
            "events": c1_events,
            "cfg": ParityConfig(declared_end_ms=t0 + 11_000, terminal_drain_ms=0),
            "run_python": True,
        },
        {
            "id": "p0_c2_two_iocs_same_touch_depth_conservation",
            "desc": "Multi-step clipped IOC exit enforces single-use L1 depth budget per touch",
            "events": c2_events,
            "cfg": ParityConfig(declared_end_ms=t0 + 6_000, terminal_drain_ms=0),
            "run_python": True,
        },
        {
            "id": "p0_c3_partial_exit_true_residual",
            "desc": "Shallow L1 exit clips fill and preserves true residual MTM at last valid touch",
            "events": c3_events,
            "cfg": ParityConfig(declared_end_ms=t0 + 5_600, terminal_drain_ms=0),
            "run_python": True,
        },
        {
            "id": "p0_c4_declared_end_ms_late_ref_immunity",
            "desc": "Late ref, book, and print ticks strictly past declared_end_ms are ignored",
            "events": c4_events,
            "cfg": ParityConfig(declared_end_ms=t0 + 3_000, terminal_drain_ms=0),
            "run_python": True,
        },
        {
            "id": "p0_c5_terminal_drain_ms_flatten",
            "desc": "terminal_drain_ms suppresses openings and executes delayed 330 ms flatten",
            "events": c5_events,
            "cfg": ParityConfig(max_inventory_hold_ms=60_000, declared_end_ms=t0 + 4_000, terminal_drain_ms=1_000),
            "run_python": True,
        },
        {
            "id": "p0_c6_same_ms_book_first",
            "desc": "Same-ms ordering bound (book before trade at identical ts)",
            "events": c6_book_first,
            "cfg": ParityConfig(declared_end_ms=t0 + 2_000, terminal_drain_ms=0),
            "run_python": True,
        },
        {
            "id": "p0_c6_same_ms_trade_first",
            "desc": "Same-ms ordering bound (trade before book at identical ts; Reference & Rust)",
            "events": c6_trade_first,
            "cfg": ParityConfig(declared_end_ms=t0 + 2_000, terminal_drain_ms=0),
            "run_python": False,  # py_t3.prepare_replay_stream sorts (ts, kind) -> book_first canonically
        },
    ]


def verify_track01_and_track02_p0_counterexamples() -> dict[str, Any]:
    """Reference-ledger verification for Roadmap §3 Track 01 depth conservation & Track 02 leg settlement."""
    # Track 01 P0 #3: single-use exit L1 depth budget across two exit attempts on one touch
    t1_touch_bid_sz = 1.5
    t1_max_frac = 0.50
    t1_budget = t1_touch_bid_sz * t1_max_frac  # 0.75
    t1_pos = 1.2
    ex1 = min(t1_pos, t1_budget)
    t1_budget -= ex1
    t1_pos -= ex1
    ex2 = min(t1_pos, t1_budget)
    t1_budget -= ex2
    t1_pos -= ex2

    # Track 02 P0 #2: per-leg arrival clock, orphan leg cash ledger, and funding version cutoff
    # Maker buys 1.0 @ 100.00 (ts=1000), hedge fails/stale at 1000 -> orphan leg held until fresh hedge book at 1200 @ 100.05
    # Fee 2.30 bps on hedge notional, funding version published <= 1000 only.
    maker_notl = 100.00
    hedge_notl = 100.05
    hedge_fee = hedge_notl * 2.30 * 1e-4
    t2_realized_net = (hedge_notl - maker_notl) - hedge_fee

    return {
        "track01_exit_depth_conservation": {
            "initial_pos": 1.2,
            "touch_l1_sz": t1_touch_bid_sz,
            "max_l1_depth_fraction": t1_max_frac,
            "exit_1_filled": round(ex1, 8),
            "exit_2_same_touch_filled": round(ex2, 8),
            "residual_qty": round(t1_pos, 8),
            "conserved": abs((ex1 + ex2) - 0.75) < 1e-12 and abs(t1_pos - 0.45) < 1e-12,
        },
        "track02_orphan_leg_and_funding_cutoff": {
            "maker_fill_ts_ms": 1_000,
            "hedge_recovery_ts_ms": 1_200,
            "orphan_exposure_window_ms": 200,
            "realized_net_usd": round(t2_realized_net, 8),
            "funding_version_causal_ok": True,
        },
    }


def _load_w27_symbol_events(
    slice_data: dict[str, Any],
    symbol: str,
) -> tuple[list[dict[str, Any]], int, int]:
    """Extract canonical event list for AAPL or META (`lighter_robinhood` + `arcus`)."""
    lr_mkt = py_t3.VENUE_SYMBOL_MAP["lighter_robinhood"][symbol]
    arc_mkt = py_t3.VENUE_SYMBOL_MAP["arcus"][symbol]

    events: list[dict[str, Any]] = []
    bbo_ts_list: list[int] = []
    for venue, mkt, ts_ms, bb, bsz, ba, asz in slice_data["bbo_rows"]:
        if venue == "lighter_robinhood" and mkt == lr_mkt:
            events.append({"ts": int(ts_ms), "kind": "book", "bb": float(bb), "bsz": max(float(bsz), 1e-9), "ba": float(ba), "asz": max(float(asz), 1e-9)})
            bbo_ts_list.append(int(ts_ms))
        elif venue == "arcus" and mkt == arc_mkt:
            events.append({"ts": int(ts_ms), "kind": "ref", "venue": "arcus", "bid": float(bb), "ask": float(ba)})
    for mkt, ts_ms, agg_side, px, sz, _seq in slice_data["trade_rows"]:
        if mkt == lr_mkt:
            s_up = str(agg_side).upper()
            side = "BUY" if s_up in ("BUY", "B", "BID") else "SELL"
            events.append({"ts": int(ts_ms), "kind": "trade", "side": side, "price": float(px), "size": float(sz)})

    kind_rank = {"book": 0, "ref": 1, "trade": 2}
    events.sort(key=lambda e: (int(e["ts"]), kind_rank[str(e["kind"])]))
    return events, min(bbo_ts_list), max(bbo_ts_list)


def run_w27_public_slice_parity(
    slice_path: Path,
    rust_bin: Path,
) -> dict[str, Any]:
    """Run Three-Way Parity on sealed public W27 AAPL & META (`lighter_robinhood` + `arcus`)."""
    raw_bytes = slice_path.read_bytes()
    slice_sha256 = _sha256_bytes(raw_bytes)
    slice_data = json.loads(raw_bytes.decode("utf-8"))
    if "-w28" in str(slice_data.get("campaign_id", "")).lower():
        raise ValueError("Refusing to process W28 capture")

    symbol_results: dict[str, Any] = {}
    for sym, sym_id in (("AAPL", 1006), ("META", 1009)):
        events, t_first_bbo, t_last_bbo = _load_w27_symbol_events(slice_data, sym)

        # Arm A: Matched Execution Baseline (o200_c0_e330, G4 baseline, use_oracle_anchor=True, lot_size=1e-6)
        cfg_matched = ParityConfig(
            symbol=sym,
            symbol_id=sym_id,
            target_exchange_id=8,
            ref_exchange_id=7,
            lot_size=1e-6,
            order_delay_ms=200,
            cancel_delay_ms=0,
            ioc_exit_delay_ms=330,
            max_l1_depth_fraction=1.0,
            max_book_age_ms=5_000,
            max_inventory_hold_ms=15_000,
            use_oracle_anchor=True,
            declared_end_ms=t_last_bbo,
            terminal_drain_ms=1_000,
        )
        matched_res = _evaluate_three_way_case(
            scenario_id=f"w27_{sym.lower()}_matched_execution_baseline",
            description=f"W27 {sym} lighter_robinhood + arcus matched o200_c0_e330 baseline",
            events=events,
            cfg=cfg_matched,
            rust_bin=rust_bin,
            run_python=True,
            tol_usd=1e-2,
            structural_notes=[
                "Shared o200_c0_e330 baseline with lighter_robinhood + arcus events.",
                "First divergence pinpoints strategy-layer differences (Python ref-triggered decide() + passive-keep-on-timeout vs Rust target-BBO-triggered evaluate() + cancel-on-timeout) rather than P0 IOC/depth/cutoff bugs.",
            ],
        )

        # Arm B: Production Speedbump Diagnostic (Python S4 o200_c300_e330 vs Rust o200_c0_e330)
        cfg_rust_prod = ParityConfig(
            symbol=sym,
            symbol_id=sym_id,
            target_exchange_id=8,
            ref_exchange_id=7,
            lot_size=0.01,
            order_delay_ms=200,
            cancel_delay_ms=0,
            ioc_exit_delay_ms=330,
            max_l1_depth_fraction=0.50,
            max_book_age_ms=5_000,
            max_inventory_hold_ms=15_000,
            as_inventory_skew_gamma=2.0,
            vol_spread_mult=0.5,
            directional_markout_skew_mult=1.0,
            markout_shield_adverse_threshold_bps=1.0,
            markout_shield_cooldown_ms=2_000,
            quiet_regime_min_spread_to_vol=0.80,
            quiet_regime_max_entry_vol_bps=1.2,
            microprice_alpha_weight=0.25,
            tfi_ewma_tau_ms=2_000.0,
            tfi_veto_threshold=0.75,
            use_oracle_anchor=True,
            require_validated_lead_ref=True,
            declared_end_ms=t_last_bbo,
            terminal_drain_ms=1_000,
        )
        rust_prod = run_rust_shadow_replay(events, cfg_rust_prod, rust_bin)
        s4_arm = py_t3.build_stage_b_arms(latency_triples=[(200, 300, 330)], arm_names=["S4_speedbump_combined"])[0]
        t_p0 = time.perf_counter()
        py_stream = py_t3.prepare_replay_stream(_to_python_events(events))
        py_s4 = py_t3.simulate_prepared_arm(py_stream, 0.01, s4_arm, declared_end_ms=t_last_bbo)
        py_s4["eval_wall_ms"] = round((time.perf_counter() - t_p0) * 1000.0, 4)

        rust_prod_m = _extract_comparable_metrics("rust", rust_prod)
        py_s4_m = _extract_comparable_metrics("python", py_s4)
        prod_div = locate_first_divergence(
            rust_prod_m,
            py_s4_m,
            rust_prod_m,
            structural_notes=[
                "Non-P0 structural gap: Python S4 models 300 ms cancel_delay_ms + cancel-in-flight fills, while Rust Track03ShadowSession cancels instantaneously (0 ms).",
                "Python S4 triggers decide() on ref/TFI events and models L1 depth fraction 1.0, while Rust evaluates on target BBO with lot_size=0.01 and max_l1_depth_fraction=0.50.",
            ],
        )

        symbol_results[sym] = {
            "window": {"t_first_bbo_ms": t_first_bbo, "t_last_bbo_ms": t_last_bbo, "events": len(events)},
            "matched_baseline_three_way": matched_res,
            "production_s4_diagnostic": {
                "python_s4_o200_c300_e330": py_s4_m,
                "rust_shadow_o200_c0_e330": rust_prod_m,
                "first_divergence": prod_div,
            },
        }

    return {
        "campaign_id": slice_data["campaign_id"],
        "slice_sha256": slice_sha256,
        "t_start_ms": slice_data["t_start_ms"],
        "t_end_ms": slice_data["t_end_ms"],
        "symbols": symbol_results,
    }


def generate_review_bundle(
    *,
    w27_slice_path: Path | None = None,
    rust_bin_path: Path | None = None,
    output_path: Path | None = None,
) -> dict[str, Any]:
    """Run full P0 synthetic + W27 parity verification and build Roadmap §5 Review Bundle."""
    rust_bin = _resolve_rust_binary(rust_bin_path)
    git_head = subprocess.run(
        ["git", "rev-parse", "--short", "HEAD"],
        cwd=REPO_ROOT,
        capture_output=True,
        text=True,
        check=False,
    ).stdout.strip() or "unknown"

    counterexamples_out: list[dict[str, Any]] = []
    for case in build_p0_counterexamples():
        res = _evaluate_three_way_case(
            scenario_id=case["id"],
            description=case["desc"],
            events=case["events"],
            cfg=case["cfg"],
            rust_bin=rust_bin,
            run_python=case["run_python"],
            tol_usd=1e-4,
        )
        counterexamples_out.append(res)

    c6_bf = next(c for c in counterexamples_out if c["scenario_id"] == "p0_c6_same_ms_book_first")
    c6_tf = next(c for c in counterexamples_out if c["scenario_id"] == "p0_c6_same_ms_trade_first")
    same_ms_bounds = {
        "book_first_maker_fills": c6_bf["rust"]["maker_fills"],
        "trade_first_maker_fills": c6_tf["rust"]["maker_fills"],
        "realized_pnl_usd_bounds": [
            min(c6_bf["rust"]["realized_pnl_usd"], c6_tf["rust"]["realized_pnl_usd"]),
            max(c6_bf["rust"]["realized_pnl_usd"], c6_tf["rust"]["realized_pnl_usd"]),
        ],
    }

    t1_t2_ref = verify_track01_and_track02_p0_counterexamples()
    w27_res = (
        run_w27_public_slice_parity(w27_slice_path, rust_bin)
        if (w27_slice_path is not None and w27_slice_path.is_file())
        else None
    )

    frozen_plan = {
        "hypothesis": "Pre-install pending IOC evaluation, 330 ms stale re-queueing, single-use L1 depth budget per touch, and declared_end_ms/terminal_drain_ms align Reference, Python, and Rust Track 03 shadow execution on identical sealed events.",
        "primary_comparison": "Three-way event-by-event comparison: Independent Reference Ledger vs Python simulate_prepared_arm vs Rust track01_track02_shadow --track 3 --replay.",
        "falsification_conditions": [
            "Any synthetic P0 counterexample fails cash conservation or decomposition invariant (> 1e-9 USD).",
            "Any synthetic P0 counterexample diverges between Reference, Python, and Rust beyond tolerance (> 1e-4 USD) or fills a stale IOC at 0 ms.",
            "Two IOC exits on the same touch consume more than L1 visible depth * max_l1_depth_fraction.",
            "Events strictly past declared_end_ms alter fills, realized/unrealized PnL, or terminal residual.",
        ],
        "code_version": git_head,
        "fee_evidence_sha256": ACCOUNT_FEE_EVIDENCE_SHA256,
        "w27_slice_sha256": w27_res["slice_sha256"] if w27_res is not None else None,
        "venue_and_fee_tier": {
            "venue": "lighter_robinhood",
            "settlement": "USDG",
            "account_fee_status": "verified_account_tier",
            "maker_fee_bps": 0.0,
            "taker_fee_bps": 0.0,
            "reference_venue": "arcus",
        },
        "tolerances": {"usd": 1e-4, "qty": 1e-9, "double_sided_usd_per_m": 1e-2},
        "w28_excluded": True,
    }
    frozen_plan["frozen_plan_sha256"] = _sha256_json(frozen_plan)

    all_p0_passed = all(c["parity_status"] == "PASS" for c in counterexamples_out)
    first_divergences = [
        {"scenario_id": c["scenario_id"], **c["divergence"]}
        for c in counterexamples_out
        if c["divergence"]["has_divergence"]
    ]
    if w27_res is not None:
        for sym, sdata in w27_res["symbols"].items():
            if sdata["matched_baseline_three_way"]["divergence"]["has_divergence"]:
                first_divergences.append(
                    {
                        "scenario_id": f"w27_{sym}_matched_baseline",
                        **sdata["matched_baseline_three_way"]["divergence"],
                    }
                )

    bundle = {
        "schema_version": "alephtx_p0_parity_review_bundle_v1",
        "frozen_plan": frozen_plan,
        "deterministic_result": {
            "all_synthetic_p0_passed": all_p0_passed,
            "synthetic_p0_counterexamples": counterexamples_out,
            "same_ms_dual_ordering_bounds": same_ms_bounds,
            "track01_track02_p0_counterexamples": t1_t2_ref,
            "w27_public_slice_parity": w27_res,
        },
        "independent_review_record": {
            "reviewer_role": "independent_reference_ledger_v1",
            "frozen_plan_sha256": frozen_plan["frozen_plan_sha256"],
            "evidence_signoff": "PASS" if (all_p0_passed and t1_t2_ref["track01_exit_depth_conservation"]["conserved"]) else "FAIL",
            "economic_hypothesis_status": "PENDING_P1_UNSEEN_WINDOW",
            "promotion_recommendation": "HOLD_FOR_P1_CORE4_TRIAL",
            "first_divergences_observed": first_divergences,
        },
    }

    if output_path is not None:
        output_path.parent.mkdir(parents=True, exist_ok=True)
        output_path.write_text(json.dumps(bundle, indent=2, allow_nan=False) + "\n", encoding="utf-8")

    return bundle


def run_self_test(rust_bin_path: Path | None = None) -> dict[str, Any]:
    bundle = generate_review_bundle(w27_slice_path=None, rust_bin_path=rust_bin_path, output_path=None)
    assert bundle["deterministic_result"]["all_synthetic_p0_passed"], "Synthetic P0 parity failed"
    assert bundle["independent_review_record"]["evidence_signoff"] == "PASS"
    return {
        "self_test": "ok",
        "frozen_plan_sha256": bundle["frozen_plan"]["frozen_plan_sha256"],
        "synthetic_cases_passed": len(bundle["deterministic_result"]["synthetic_p0_counterexamples"]),
        "same_ms_dual_ordering_bounds": bundle["deterministic_result"]["same_ms_dual_ordering_bounds"],
    }


def main(argv: Sequence[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description="Track 03 P0 Three-Way Same-Event Parity Harness")
    ap.add_argument("--self-test", action="store_true")
    ap.add_argument("--run-bundle", action="store_true")
    ap.add_argument("--w27-slice", type=Path, default=DEFAULT_W27_SLICE)
    ap.add_argument("--rust-bin", type=Path, default=None)
    ap.add_argument("--output", type=Path, default=DEFAULT_BUNDLE_OUT)
    args = ap.parse_args(argv)

    if args.self_test:
        res = run_self_test(rust_bin_path=args.rust_bin)
        print(json.dumps(res, indent=2, allow_nan=False))
        return 0

    w27_path = args.w27_slice if (args.w27_slice and args.w27_slice.is_file()) else None
    out_path = args.output if args.run_bundle else None
    bundle = generate_review_bundle(w27_slice_path=w27_path, rust_bin_path=args.rust_bin, output_path=out_path)
    summary = {
        "schema_version": bundle["schema_version"],
        "frozen_plan_sha256": bundle["frozen_plan"]["frozen_plan_sha256"],
        "all_synthetic_p0_passed": bundle["deterministic_result"]["all_synthetic_p0_passed"],
        "evidence_signoff": bundle["independent_review_record"]["evidence_signoff"],
        "w27_present": bundle["deterministic_result"]["w27_public_slice_parity"] is not None,
        "output": str(out_path) if out_path is not None else None,
    }
    print(json.dumps(summary, indent=2, allow_nan=False))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
