#!/usr/bin/env python3
"""Offline queue-priority MM simulator v2 and live Arcus/Entropy frontier scanner.

Compares 4 quoting architectures on captured L2 + trade streams:
  A_baseline_back_of_queue   : touch-chasing join (cancels & rejoins back of queue)
  B_queue_priority_ladder    : retains resting FIFO queue within band + pennies >=2-tick spreads
  C_qprio_lead_pull          : B + cross-venue EWMA lead-lag asymmetric pull + trade-sweep pull
  D_qprio_lead_min_edge_skew : C + fee-adjusted min-edge entry gate + inventory skew + optional 0-fee hedge

And `--scan-live-frontier` queries public Arcus, Hyperliquid (io/xyz), and Lighter Core/RH
REST endpoints (zero credentials) to rank spread-to-L1-depth opportunities and estimate T5/T6 maker-share thresholds.
"""

from __future__ import annotations

import argparse
import dataclasses
import gzip
import json
import math
import subprocess
import sys
from collections import defaultdict
from pathlib import Path
from typing import Any


@dataclasses.dataclass
class SimArmConfig:
    name: str
    quote_usd: float = 200.0
    max_pos_usd: float = 1000.0
    hedge_band_usd: float | None = None
    hedge_fee_bps: float = 0.0
    hedge_delay_ms: int = 310
    maker_fee_bps: float = 0.0
    order_delay_ms: int = 10
    cancel_delay_ms: int = 10
    keep_queue_ticks: int = 0
    penny_min_ticks: int = 9999
    lead_pull_bps: float | None = None
    lead_spread_mult: float = 0.0
    require_validated_lead_ref: bool = False
    lead_validation_horizon_ms: int = 2_000
    lead_validation_min_move_bps: float = 0.50
    lead_validation_min_samples: int = 12
    lead_validation_min_score_bps: float = 0.02
    sweep_pull_ratio: float | None = None
    sweep_min_notional_usd: float = 0.0
    sweep_cooldown_ms: int = 1500
    alo_reject_cooldown_ms: int = 0
    markout_shield_horizon_ms: int = 1_000
    markout_shield_adverse_threshold_bps: float = 0.0
    markout_shield_cooldown_ms: int = 1_200
    markout_shield_size_scale: float = 0.50
    obi_entry_veto_threshold: float | None = None
    alo_flight_guard_sigma_bps: float = 0.0
    alo_flight_guard_obi_threshold: float | None = None
    infer_book_diff_sweeps: bool = False
    disable_bbo_sweep_fills: bool = False
    min_edge_bps: float = -100.0
    skew_bps_at_cap: float = 0.0
    ewma_tau_ms: float = 120_000.0


SimParams = SimArmConfig


@dataclasses.dataclass
class RestingOrder:
    side: str  # "BID" or "ASK"
    price: float
    qty: float
    remaining: float
    queue_ahead: float
    placed_at_ms: int
    active_at_ms: int
    activated: bool = False
    cancel_effective_ms: int | None = None


def round_tick(px: float, tick: float, up: bool) -> float:
    units = px / tick
    return (math.ceil(units - 1e-9) if up else math.floor(units + 1e-9)) * tick


def default_arms(
    maker_fee_bps: float = 0.0,
    order_delay_ms: int = 10,
    cancel_delay_ms: int = 10,
) -> list[SimArmConfig]:
    return [
        SimArmConfig(
            name="A_baseline_back_of_queue",
            maker_fee_bps=maker_fee_bps,
            order_delay_ms=order_delay_ms,
            cancel_delay_ms=cancel_delay_ms,
            keep_queue_ticks=0,
            penny_min_ticks=9999,
            lead_pull_bps=None,
            sweep_pull_ratio=None,
            min_edge_bps=-100.0,
            skew_bps_at_cap=0.0,
            hedge_band_usd=1000.0,
            hedge_fee_bps=0.0,
        ),
        SimArmConfig(
            name="B_queue_priority_ladder",
            maker_fee_bps=maker_fee_bps,
            order_delay_ms=order_delay_ms,
            cancel_delay_ms=cancel_delay_ms,
            keep_queue_ticks=2,
            penny_min_ticks=3,
            lead_pull_bps=None,
            sweep_pull_ratio=None,
            min_edge_bps=-100.0,
            skew_bps_at_cap=0.5,
            hedge_band_usd=1000.0,
            hedge_fee_bps=0.0,
        ),
        SimArmConfig(
            name="C_qprio_lead_pull",
            maker_fee_bps=maker_fee_bps,
            order_delay_ms=order_delay_ms,
            cancel_delay_ms=cancel_delay_ms,
            keep_queue_ticks=2,
            penny_min_ticks=3,
            lead_pull_bps=0.30,
            sweep_pull_ratio=0.60,
            min_edge_bps=-100.0,
            skew_bps_at_cap=1.0,
            hedge_band_usd=1000.0,
            hedge_fee_bps=0.0,
        ),
        SimArmConfig(
            name="D_qprio_lead_min_edge_skew",
            maker_fee_bps=maker_fee_bps,
            order_delay_ms=order_delay_ms,
            cancel_delay_ms=cancel_delay_ms,
            keep_queue_ticks=2,
            penny_min_ticks=3,
            lead_pull_bps=0.30,
            sweep_pull_ratio=0.60,
            min_edge_bps=0.10,
            skew_bps_at_cap=2.0,
            hedge_band_usd=500.0,
            hedge_fee_bps=0.0,
        ),
        SimArmConfig(
            name="E_v3_validated_lead_qprio",
            maker_fee_bps=maker_fee_bps,
            order_delay_ms=order_delay_ms,
            cancel_delay_ms=cancel_delay_ms,
            keep_queue_ticks=4,
            penny_min_ticks=4,
            lead_pull_bps=0.80,
            lead_spread_mult=1.5,
            require_validated_lead_ref=True,
            lead_validation_horizon_ms=2_000,
            lead_validation_min_move_bps=0.50,
            lead_validation_min_samples=12,
            lead_validation_min_score_bps=0.02,
            sweep_pull_ratio=0.75,
            sweep_min_notional_usd=200.0,
            sweep_cooldown_ms=1_000,
            alo_reject_cooldown_ms=350,
            min_edge_bps=0.10,
            skew_bps_at_cap=1.8,
            hedge_band_usd=1000.0,
            hedge_fee_bps=0.0,
        ),
        SimArmConfig(
            name="F_v4_adaptive_shield",
            maker_fee_bps=maker_fee_bps,
            order_delay_ms=order_delay_ms,
            cancel_delay_ms=cancel_delay_ms,
            keep_queue_ticks=4,
            penny_min_ticks=4,
            lead_pull_bps=0.80,
            lead_spread_mult=1.5,
            require_validated_lead_ref=True,
            lead_validation_horizon_ms=1_500,
            lead_validation_min_move_bps=0.35,
            lead_validation_min_samples=5,
            lead_validation_min_score_bps=0.02,
            sweep_pull_ratio=0.75,
            sweep_min_notional_usd=200.0,
            sweep_cooldown_ms=1_000,
            alo_reject_cooldown_ms=350,
            markout_shield_horizon_ms=1_000,
            markout_shield_adverse_threshold_bps=1.50,
            markout_shield_cooldown_ms=1_200,
            markout_shield_size_scale=0.50,
            obi_entry_veto_threshold=0.75,
            alo_flight_guard_sigma_bps=0.25,
            alo_flight_guard_obi_threshold=0.45,
            min_edge_bps=0.10,
            skew_bps_at_cap=1.8,
            hedge_band_usd=1000.0,
            hedge_fee_bps=0.0,
        ),
    ]


def default_arms_for_profile(
    profile_name: str,
    maker_fee_bps: float | None = None,
    infer_book_diff_sweeps: bool = False,
) -> list[SimArmConfig]:
    """Return calibrated 5-arm comparison suite for a physical venue execution profile."""
    presets = {
        "fast_zero_fee_alo": (0.0, 36, 33, 0.25, 0.45, 0.10),
        "speedbump_zero_fee_wide": (0.0, 200, 200, 0.35, 0.55, 0.25),
        "fast_fee_rebate_clob": (0.0, 40, 40, 0.25, 0.45, 0.20),
        "slow_l1_wide_spread": (0.30, 670, 695, 0.0, None, 0.50),
    }
    if profile_name not in presets:
        raise ValueError(f"unknown profile {profile_name!r}")
    base_fee, o_ms, c_ms, flight_sig, flight_obi, min_edge = presets[profile_name]
    fee = base_fee if maker_fee_bps is None else maker_fee_bps
    arms = default_arms(maker_fee_bps=fee, order_delay_ms=o_ms, cancel_delay_ms=c_ms)
    for arm in arms:
        arm.infer_book_diff_sweeps = infer_book_diff_sweeps
        if arm.name == "F_v4_adaptive_shield":
            arm.alo_flight_guard_sigma_bps = flight_sig
            arm.alo_flight_guard_obi_threshold = flight_obi
            arm.min_edge_bps = min_edge
        elif arm.name == "E_v3_validated_lead_qprio":
            arm.min_edge_bps = min_edge
    return arms


def load_capture_events(
    path: Path,
    target_symbol: str,
    ref_sources: set[str],
    ref_channel_ids: set[str],
) -> tuple[list[dict[str, Any]], float]:
    """Load chronological book/trade/reference events for a target Arcus market."""
    events: list[dict[str, Any]] = []
    opener = gzip.open if path.suffix == ".gz" else open
    # Infer tick size from observed Arcus prices
    tick_candidates: set[int] = set()
    with opener(path, "rt", encoding="utf-8") as fh:
        for line in fh:
            try:
                msg = json.loads(line)
            except json.JSONDecodeError:
                continue
            src = msg.get("source")
            ts = msg.get("available_at_ms") or msg.get("received_at_ms")
            if not isinstance(ts, (int, float)):
                continue
            ts_ms = int(ts)
            payload = msg.get("payload") or {}
            if src == "arcus" and payload.get("channel") == "l2Orderbook" and payload.get("id") == target_symbol:
                c = payload.get("contents") or {}
                bids_raw = c.get("bids") or []
                asks_raw = c.get("asks") or []
                if not bids_raw or not asks_raw:
                    continue
                bids = [(float(p), float(s)) for p, s in bids_raw[:20] if float(s) > 0]
                asks = [(float(p), float(s)) for p, s in asks_raw[:20] if float(s) > 0]
                if not bids or not asks or bids[0][0] >= asks[0][0]:
                    continue
                for p_str, _ in bids_raw[:2]:
                    if "." in p_str:
                        tick_candidates.add(len(p_str.split(".")[1].rstrip("0")))
                events.append({"ts": ts_ms, "kind": "book", "bids": bids, "asks": asks})
            elif src == "arcus_trades" and payload.get("contents"):
                for tr in payload["contents"]:
                    if tr.get("marketDisplayName") == target_symbol:
                        events.append(
                            {
                                "ts": ts_ms,
                                "kind": "trade",
                                "side": tr["side"],
                                "price": float(tr["price"]),
                                "size": float(tr["size"]),
                            }
                        )
            elif src in ref_sources:
                if "order_book" in payload:
                    ch = str(payload.get("channel") or "")
                    cid = ch.split(":")[1] if ":" in ch else ch
                    if cid in ref_channel_ids:
                        ob = payload["order_book"]
                        bs = ob.get("bids") or []
                        aks = ob.get("asks") or []
                        if bs and aks:
                            bb = float(bs[0]["price"])
                            ba = float(aks[0]["price"])
                            if 0 < bb < ba:
                                events.append(
                                    {
                                        "ts": ts_ms,
                                        "kind": "ref",
                                        "venue": src,
                                        "bid": bb,
                                        "ask": ba,
                                    }
                                )
    events.sort(key=lambda e: e["ts"])
    max_dec = max(tick_candidates) if tick_candidates else 2
    tick = 10.0 ** (-max_dec)
    return events, tick


def simulate_arm(
    events: list[dict[str, Any]],
    tick: float,
    arm: SimArmConfig,
    markout_horizons_ms: tuple[int, ...] = (1000, 10_000, 30_000),
) -> dict[str, Any]:
    """Run causal event-driven simulation on a chronological stream."""
    if not events:
        return {"arm": arm.name, "maker_volume_usd": 0.0, "fills": 0}

    # Pre-index target mids for post-trade markouts.
    mid_times: list[int] = []
    mid_vals: list[float] = []
    for ev in events:
        if ev["kind"] == "book":
            mid = 0.5 * (ev["bids"][0][0] + ev["asks"][0][0])
            mid_times.append(ev["ts"])
            mid_vals.append(mid)

    def mid_at(ts_ms: int) -> float | None:
        import bisect

        if not mid_times or ts_ms > mid_times[-1]:
            return None
        idx = bisect.bisect_right(mid_times, ts_ms) - 1
        return mid_vals[idx] if idx >= 0 else None

    cur_book: dict[str, Any] | None = None
    prev_book_ts: int | None = None
    ref_books: dict[str, tuple[int, float]] = {}
    ref_lead_states: dict[str, dict[str, Any]] = {}
    basis_ewma: float | None = None
    last_basis_ts: int | None = None

    resting: dict[str, RestingOrder | None] = {"BID": None, "ASK": None}
    bid_cooldown_until = 0
    ask_cooldown_until = 0
    alo_cooldown_until = {"BID": 0, "ASK": 0}

    # Causal post-fill markout shield + OBI + realized vol parity with Rust QueuePriorityMmEngine v4
    pending_fills: list[tuple[int, str, float]] = []  # (eval_ts_ms, side, fill_px)
    markout_ewma_bps = {"BID": 0.0, "ASK": 0.0}
    markout_cooldown_until = {"BID": 0, "ASK": 0}
    last_mid_for_vol: tuple[int, float] | None = None
    var_bps2_per_s = 0.0

    cash_usd = 0.0
    inv_target = 0.0
    inv_hedge = 0.0
    maker_vol_usd = 0.0
    hedge_vol_usd = 0.0
    fills_log: list[dict[str, Any]] = []
    pending_hedges: list[tuple[int, str, float]] = []  # (exec_ts, side, qty)
    orders_placed = 0
    orders_canceled = 0
    orders_kept = 0
    alo_rejected = 0
    spread_capture_usd = 0.0
    validated_lead_refs_end = 0
    inferred_book_diff_sweeps = 0

    def weighted_obi(book: dict[str, Any]) -> float:
        weights = (1.0, 0.5, 0.25)
        wb = 0.0
        wa = 0.0
        bids = book["bids"]
        asks = book["asks"]
        for idx, w in enumerate(weights):
            if idx < len(bids) and bids[idx][0] > 0.0 and bids[idx][1] > 0.0:
                wb += bids[idx][1] * w
            if idx < len(asks) and asks[idx][0] > 0.0 and asks[idx][1] > 0.0:
                wa += asks[idx][1] * w
        denom = wb + wa
        return max(-1.0, min(1.0, (wb - wa) / denom)) if denom > 0.0 else 0.0

    def update_realized_vol(now_ms: int, mid: float) -> None:
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

    def mature_fill_markouts(now_ms: int, target_mid: float) -> None:
        nonlocal pending_fills
        if target_mid <= 0.0 or not pending_fills:
            return
        still: list[tuple[int, str, float]] = []
        thresh = arm.markout_shield_adverse_threshold_bps
        for eval_ms, fside, fpx in pending_fills:
            if now_ms < eval_ms:
                still.append((eval_ms, fside, fpx))
                continue
            if fpx <= 0.0:
                continue
            sgn = 1.0 if fside == "BID" else -1.0
            mk_bps = sgn * (target_mid - fpx) / fpx * 1e4
            markout_ewma_bps[fside] = 0.75 * markout_ewma_bps[fside] + 0.25 * mk_bps
            if thresh > 0.0 and markout_ewma_bps[fside] < -2.0 * thresh:
                markout_cooldown_until[fside] = max(
                    markout_cooldown_until[fside], now_ms + arm.markout_shield_cooldown_ms
                )
        pending_fills = still

    def handle_trade(ts_ms: int, tr_side: str, tr_px: float, tr_sz: float) -> None:
        nonlocal cash_usd, inv_target, maker_vol_usd, spread_capture_usd
        nonlocal ask_cooldown_until, bid_cooldown_until
        if cur_book is None or tr_px <= 0.0 or tr_sz <= 0.0:
            return
        maker_side = "ASK" if tr_side == "BUY" else "BID"
        o = resting[maker_side]
        if o is not None and o.activated:
            through = (tr_px > o.price + tick * 0.1) if maker_side == "ASK" else (tr_px < o.price - tick * 0.1)
            at_px = abs(tr_px - o.price) <= tick * 0.1
            filled_qty = 0.0
            if through:
                filled_qty = o.remaining
            elif at_px:
                if tr_sz > o.queue_ahead:
                    avail = tr_sz - o.queue_ahead
                    o.queue_ahead = 0.0
                    filled_qty = min(o.remaining, avail)
                else:
                    o.queue_ahead -= tr_sz

            if filled_qty > 1e-12:
                mid_now = 0.5 * (cur_book["bids"][0][0] + cur_book["asks"][0][0])
                notl = filled_qty * o.price
                fee = notl * (arm.maker_fee_bps * 1e-4)
                if maker_side == "BID":
                    cash_usd -= notl + fee
                    inv_target += filled_qty
                    spread_capture_usd += (mid_now - o.price) * filled_qty
                else:
                    cash_usd += notl - fee
                    inv_target -= filled_qty
                    spread_capture_usd += (o.price - mid_now) * filled_qty
                maker_vol_usd += notl
                fills_log.append(
                    {"ts": ts_ms, "side": maker_side, "px": o.price, "qty": filled_qty, "notl": notl}
                )
                pending_fills.append((ts_ms + arm.markout_shield_horizon_ms, maker_side, o.price))
                if len(pending_fills) > 128:
                    pending_fills.pop(0)
                o.remaining -= filled_qty
                if o.remaining <= 1e-12:
                    resting[maker_side] = None
                maybe_trigger_hedge(ts_ms)

        if arm.sweep_pull_ratio is not None:
            tr_notl = tr_px * tr_sz
            tol = tick * 0.1
            if maker_side == "ASK":
                ask_px, ask_sz = cur_book["asks"][0]
                if tr_px > ask_px + tol or (tr_sz >= ask_sz * arm.sweep_pull_ratio and tr_notl >= arm.sweep_min_notional_usd):
                    ask_cooldown_until = max(ask_cooldown_until, ts_ms + arm.sweep_cooldown_ms)
            else:
                bid_px, bid_sz = cur_book["bids"][0]
                if tr_px < bid_px - tol or (tr_sz >= bid_sz * arm.sweep_pull_ratio and tr_notl >= arm.sweep_min_notional_usd):
                    bid_cooldown_until = max(bid_cooldown_until, ts_ms + arm.sweep_cooldown_ms)

    def level_size(levels: list[tuple[float, float]], price: float) -> float:
        for p, s in levels:
            if abs(p - price) < tick * 0.25:
                return s
        return 0.0

    def activate_arrivals(now_ms: int) -> None:
        nonlocal alo_rejected
        if cur_book is None:
            return
        bb, _ = cur_book["bids"][0]
        ba, _ = cur_book["asks"][0]
        for side in ("BID", "ASK"):
            o = resting[side]
            if o is None or o.activated or now_ms < o.active_at_ms:
                continue
            crosses = (o.price >= ba) if side == "BID" else (o.price <= bb)
            if crosses:
                resting[side] = None
                alo_rejected += 1
                if arm.alo_reject_cooldown_ms > 0:
                    alo_cooldown_until[side] = max(alo_cooldown_until[side], now_ms + arm.alo_reject_cooldown_ms)
            else:
                lvls = cur_book["bids"] if side == "BID" else cur_book["asks"]
                o.queue_ahead = level_size(lvls, o.price)
                o.activated = True

    def process_pending_hedges(now_ms: int) -> None:
        nonlocal cash_usd, inv_hedge, hedge_vol_usd, pending_hedges
        if not pending_hedges or cur_book is None:
            return
        still: list[tuple[int, str, float]] = []
        for exec_ts, side, qty in pending_hedges:
            if now_ms < exec_ts:
                still.append((exec_ts, side, qty))
                continue
            # Use current reference (or target) mid +/- conservative half-spread + hedge fee
            ref_mids = [m for t, m in ref_books.values() if now_ms - t <= 5000]
            px = (
                sorted(ref_mids)[len(ref_mids) // 2]
                if ref_mids
                else 0.5 * (cur_book["bids"][0][0] + cur_book["asks"][0][0])
            )
            half_sp_frac = 0.45e-4  # ~0.45 bp conservative half-spread on Lighter Core/RH
            fee_frac = arm.hedge_fee_bps * 1e-4
            if side == "BUY":
                exec_px = px * (1.0 + half_sp_frac)
                cash_usd -= qty * exec_px * (1.0 + fee_frac)
                inv_hedge += qty
            else:
                exec_px = px * (1.0 - half_sp_frac)
                cash_usd += qty * exec_px * (1.0 - fee_frac)
                inv_hedge -= qty
            hedge_vol_usd += qty * exec_px
        pending_hedges = still

    def maybe_trigger_hedge(now_ms: int) -> None:
        if arm.hedge_band_usd is None or cur_book is None:
            return
        mid = 0.5 * (cur_book["bids"][0][0] + cur_book["asks"][0][0])
        pending_qty = sum(q if s == "BUY" else -q for _, s, q in pending_hedges)
        net_qty = inv_target + inv_hedge + pending_qty
        net_usd = net_qty * mid
        if abs(net_usd) > arm.hedge_band_usd:
            excess = (abs(net_usd) - arm.hedge_band_usd) / mid
            if excess * mid >= 10.0:
                hside = "SELL" if net_qty > 0 else "BUY"
                pending_hedges.append((now_ms + arm.hedge_delay_ms, hside, excess))

    def decide(now_ms: int) -> None:
        nonlocal basis_ewma, last_basis_ts, orders_placed, orders_canceled, orders_kept, validated_lead_refs_end
        if cur_book is None:
            return
        bb, _ = cur_book["bids"][0]
        ba, _ = cur_book["asks"][0]
        t_mid = 0.5 * (bb + ba)
        sp_bps = (ba - bb) / t_mid * 1e4
        obi = weighted_obi(cur_book)
        update_realized_vol(now_ms, t_mid)
        mature_fill_markouts(now_ms, t_mid)
        sigma_bps = math.sqrt(var_bps2_per_s)

        # Update causal ReferenceLeadValidator & basis EWMA from fresh references
        fresh_items = [(v, m) for v, (t, m) in ref_books.items() if now_ms - t <= 5000]
        lead_mids: list[float] = []
        for venue_name, r_mid in fresh_items:
            st = ref_lead_states.setdefault(
                venue_name,
                {"history": [], "pending": [], "samples": 0, "score_ewma": 0.0, "last_sig_ms": None},
            )
            still_pending = []
            for eval_ms, direction, tgt0 in st["pending"]:
                if now_ms >= eval_ms:
                    if tgt0 > 0 and t_mid > 0:
                        realized = direction * math.log(t_mid / tgt0) * 1e4
                        if st["samples"] == 0:
                            st["score_ewma"] = realized
                        else:
                            st["score_ewma"] = 0.85 * st["score_ewma"] + 0.15 * realized
                        st["samples"] += 1
                else:
                    still_pending.append((eval_ms, direction, tgt0))
            st["pending"] = still_pending

            if not st["history"] or now_ms > st["history"][-1][0]:
                st["history"].append((now_ms, r_mid))
                if len(st["history"]) > 64:
                    st["history"].pop(0)

            cutoff = now_ms - arm.lead_validation_horizon_ms
            base_px = None
            for h_ts, h_px in reversed(st["history"]):
                if h_ts <= cutoff:
                    base_px = h_px
                    break
            if base_px is not None and base_px > 0:
                if st["last_sig_ms"] is None or (now_ms - st["last_sig_ms"]) >= 250:
                    mv = math.log(r_mid / base_px) * 1e4
                    if abs(mv) >= arm.lead_validation_min_move_bps:
                        st["pending"].append(
                            (now_ms + arm.lead_validation_horizon_ms, 1.0 if mv > 0 else -1.0, t_mid)
                        )
                        st["last_sig_ms"] = now_ms

            if (not arm.require_validated_lead_ref) or (
                st["samples"] >= arm.lead_validation_min_samples
                and st["score_ewma"] >= arm.lead_validation_min_score_bps
            ):
                lead_mids.append(r_mid)

        validated_lead_refs_end = len(lead_mids)
        lead_gap_bps = 0.0
        fv = t_mid
        if lead_mids:
            r_mid = sorted(lead_mids)[len(lead_mids) // 2]
            raw_bps = math.log(r_mid / t_mid) * 1e4
            if last_basis_ts is not None and 0 <= now_ms - last_basis_ts <= 5000 and basis_ewma is not None:
                alpha = 1.0 - math.exp(-(now_ms - last_basis_ts) / arm.ewma_tau_ms)
                basis_ewma = basis_ewma + alpha * (raw_bps - basis_ewma)
            else:
                basis_ewma = raw_bps
            last_basis_ts = now_ms
            lead_gap_bps = raw_bps - basis_ewma
            fv = r_mid * math.exp(-basis_ewma / 1e4)
        elif last_basis_ts is not None and now_ms - last_basis_ts > 5000:
            basis_ewma = None
            last_basis_ts = None

        inv_ratio = max(-1.5, min(1.5, (inv_target * t_mid) / arm.max_pos_usd))
        skew_bps = (0.5 * inv_ratio + 0.5 * inv_ratio * abs(inv_ratio)) * arm.skew_bps_at_cap
        res_px = fv * math.exp(-skew_bps / 1e4)

        sp_ticks = max(1, round((ba - bb) / tick))
        can_penny_open = sp_ticks >= max(3, arm.penny_min_ticks)
        eff_lead_thresh = (
            max(arm.lead_pull_bps, arm.lead_spread_mult * sp_bps)
            if arm.lead_pull_bps is not None
            else None
        )

        for side in ("BID", "ASK"):
            ord_cur = resting[side]
            # Retire orders whose cancel already took effect
            if ord_cur is not None and ord_cur.cancel_effective_ms is not None and now_ms >= ord_cur.cancel_effective_ms:
                resting[side] = None
                ord_cur = None

            lead_danger = False
            if eff_lead_thresh is not None:
                aligned = (-lead_gap_bps) if side == "BID" else lead_gap_bps
                lead_danger = aligned >= eff_lead_thresh
            sweep_danger = (now_ms < bid_cooldown_until) if side == "BID" else (now_ms < ask_cooldown_until)

            if lead_danger or sweep_danger:
                if ord_cur is not None and ord_cur.cancel_effective_ms is None:
                    ord_cur.cancel_effective_ms = now_ms + arm.cancel_delay_ms
                    orders_canceled += 1
                continue

            is_reducing = (side == "BID" and inv_target * t_mid < -10.0) or (
                side == "ASK" and inv_target * t_mid > 10.0
            )
            can_penny_side = can_penny_open or (is_reducing and sp_ticks >= 2)
            desired_px = (
                min(bb + tick, ba - tick) if (side == "BID" and can_penny_side) else
                max(ba - tick, bb + tick) if (side == "ASK" and can_penny_side) else
                (bb if side == "BID" else ba)
            )
            desired_px = round_tick(desired_px, tick, up=(side == "ASK"))
            crosses = (desired_px >= ba) if side == "BID" else (desired_px <= bb)

            edge_bps = (
                (res_px - desired_px) / res_px * 1e4
                if side == "BID"
                else (desired_px - res_px) / res_px * 1e4
            )
            edge_ok = is_reducing or (edge_bps - arm.maker_fee_bps >= arm.min_edge_bps)

            pos_usd = inv_target * t_mid
            cap_blocked = (side == "BID" and pos_usd >= arm.max_pos_usd) or (
                side == "ASK" and pos_usd <= -arm.max_pos_usd
            )

            if crosses or not edge_ok or cap_blocked:
                if ord_cur is not None and ord_cur.cancel_effective_ms is None:
                    ord_cur.cancel_effective_ms = now_ms + arm.cancel_delay_ms
                    orders_canceled += 1
                continue

            if ord_cur is not None:
                if ord_cur.cancel_effective_ms is not None:
                    continue
                tick_diff = round(abs(ord_cur.price - desired_px) / tick)
                ord_crosses = (ord_cur.price >= ba) if side == "BID" else (ord_cur.price <= bb)
                if not ord_crosses and tick_diff <= arm.keep_queue_ticks:
                    orders_kept += 1
                    continue
                # Need to reprice: initiate cancel
                ord_cur.cancel_effective_ms = now_ms + arm.cancel_delay_ms
                orders_canceled += 1
                continue

            if not is_reducing:
                if now_ms < alo_cooldown_until[side] or now_ms < markout_cooldown_until[side]:
                    continue
                aligned_adverse_obi = (-obi) if side == "BID" else obi
                if arm.obi_entry_veto_threshold is not None and aligned_adverse_obi >= arm.obi_entry_veto_threshold:
                    continue
                if sp_ticks <= 2:
                    flight_sigma_bps = sigma_bps * math.sqrt(arm.order_delay_ms / 1000.0)
                    if arm.alo_flight_guard_sigma_bps > 0.0 and flight_sigma_bps >= arm.alo_flight_guard_sigma_bps:
                        continue
                    if arm.alo_flight_guard_obi_threshold is not None and aligned_adverse_obi >= arm.alo_flight_guard_obi_threshold:
                        continue

            size_scale = 1.0
            if (
                not is_reducing
                and arm.markout_shield_adverse_threshold_bps > 0.0
                and markout_ewma_bps[side] < -arm.markout_shield_adverse_threshold_bps
            ):
                size_scale = arm.markout_shield_size_scale

            # Place new ALO order at target price (queue_ahead computed on wire arrival after order_delay_ms)
            qty = (arm.quote_usd * size_scale) / desired_px
            resting[side] = RestingOrder(
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

    for ev in events:
        ts = ev["ts"]
        process_pending_hedges(ts)

        # Expire finished cancels before processing event
        for side in ("BID", "ASK"):
            o = resting[side]
            if o is not None and o.cancel_effective_ms is not None and ts >= o.cancel_effective_ms:
                resting[side] = None

        if ev["kind"] == "book":
            prev_book = cur_book
            cur_book = ev
            activate_arrivals(ts)
            # Queue ahead shrinks conservatively only if visible level shrank below queue_ahead
            for side, lvls in (("BID", ev["bids"]), ("ASK", ev["asks"])):
                o = resting[side]
                if o is not None and o.activated:
                    vis = level_size(lvls, o.price)
                    if vis < o.queue_ahead:
                        o.queue_ahead = vis
            # Optional causal book-diff sweep inference for trade-less feeds (e.g. Bulk BBO diagnostics)
            if (
                arm.infer_book_diff_sweeps
                and not arm.disable_bbo_sweep_fills
                and prev_book is not None
                and prev_book_ts is not None
                and 0 < (ts - prev_book_ts) <= 2_000
            ):
                p_bb, p_bs = prev_book["bids"][0]
                p_ba, p_as = prev_book["asks"][0]
                c_bb, c_bs = ev["bids"][0]
                c_ba, c_as = ev["asks"][0]
                if c_bb > p_ba + tick * 0.1:
                    inferred_book_diff_sweeps += 1
                    handle_trade(ts, "BUY", c_ba, max(p_as * 0.35, 1e-6))
                elif c_ba < p_bb - tick * 0.1:
                    inferred_book_diff_sweeps += 1
                    handle_trade(ts, "SELL", c_bb, max(p_bs * 0.35, 1e-6))
                else:
                    tol = tick * 0.1
                    ask_drop = (
                        p_as
                        if c_ba > p_ba + tol
                        else (max(0.0, p_as - c_as) if abs(c_ba - p_ba) <= tol else 0.0)
                    )
                    bid_drop = (
                        p_bs
                        if c_bb < p_bb - tol
                        else (max(0.0, p_bs - c_bs) if abs(c_bb - p_bb) <= tol else 0.0)
                    )
                    if ask_drop > 0.0 and ask_drop >= 1.5 * bid_drop and (ask_drop * 0.35) * p_ba >= 50.0:
                        inferred_book_diff_sweeps += 1
                        handle_trade(ts, "BUY", p_ba if c_ba == p_ba else c_ba, ask_drop * 0.35)
                    elif bid_drop > 0.0 and bid_drop >= 1.5 * ask_drop and (bid_drop * 0.35) * p_bb >= 50.0:
                        inferred_book_diff_sweeps += 1
                        handle_trade(ts, "SELL", p_bb if c_bb == p_bb else c_bb, bid_drop * 0.35)
            prev_book_ts = ts
            decide(ts)

        elif ev["kind"] == "ref":
            activate_arrivals(ts)
            ref_books[ev["venue"]] = (ts, 0.5 * (ev["bid"] + ev["ask"]))
            if arm.lead_pull_bps is not None or arm.require_validated_lead_ref:
                decide(ts)

        elif ev["kind"] == "trade":
            if cur_book is None:
                continue
            activate_arrivals(ts)
            handle_trade(ts, ev["side"], ev["price"], ev["size"])
            if arm.sweep_pull_ratio is not None:
                decide(ts)

    # Flush pending hedges and mark final equity consistently to terminal target mid
    if cur_book is None:
        return {"arm": arm.name, "maker_volume_usd": 0.0, "fills": 0}
    last_ts = events[-1]["ts"]
    process_pending_hedges(last_ts + 10_000)
    t_mid_end = 0.5 * (cur_book["bids"][0][0] + cur_book["asks"][0][0])
    fresh_refs_end = [m for t, m in ref_books.values() if last_ts - t <= 5000]
    r_mid_end = sorted(fresh_refs_end)[len(fresh_refs_end) // 2] if fresh_refs_end else t_mid_end
    equity_usd = cash_usd + (inv_target + inv_hedge) * t_mid_end
    terminal_basis_usd = inv_hedge * (r_mid_end - t_mid_end)
    combined_mtm_usd = equity_usd + terminal_basis_usd
    cost_per_m = (-equity_usd / maker_vol_usd * 1e6) if maker_vol_usd > 0 else None
    combined_cost_per_m = (-combined_mtm_usd / maker_vol_usd * 1e6) if maker_vol_usd > 0 else None

    markouts: dict[str, float | None] = {}
    for h_ms in markout_horizons_ms:
        num = 0.0
        den = 0.0
        for fl in fills_log:
            fm = mid_at(fl["ts"] + h_ms)
            if fm is None:
                continue
            sgn = 1.0 if fl["side"] == "BID" else -1.0
            mk_bps = sgn * (fm - fl["px"]) / fl["px"] * 1e4
            num += fl["notl"] * mk_bps
            den += fl["notl"]
        markouts[f"{h_ms // 1000}s_bps"] = round(num / den, 4) if den > 0 else None

    return {
        "arm": arm.name,
        "fill_evidence_mode": "book_diff_diagnostic" if (arm.infer_book_diff_sweeps and not arm.disable_bbo_sweep_fills) else "public_trades",
        "inferred_book_diff_sweeps": inferred_book_diff_sweeps,
        "maker_volume_usd": round(maker_vol_usd, 2),
        "hedge_volume_usd": round(hedge_vol_usd, 2),
        "fills": len(fills_log),
        "orders_placed": orders_placed,
        "orders_canceled": orders_canceled,
        "orders_kept": orders_kept,
        "alo_rejected": alo_rejected,
        "validated_lead_refs_end": validated_lead_refs_end,
        "spread_capture_bps": round(spread_capture_usd / maker_vol_usd * 1e4, 4) if maker_vol_usd > 0 else None,
        "equity_usd": round(equity_usd, 4),
        "terminal_basis_usd": round(terminal_basis_usd, 4),
        "combined_mtm_usd": round(combined_mtm_usd, 4),
        "cost_per_m_usd": round(cost_per_m, 2) if cost_per_m is not None else None,
        "combined_cost_per_m_usd": round(combined_cost_per_m, 2) if combined_cost_per_m is not None else None,
        "markouts": markouts,
    }


def _curl_json(url: str, body: dict[str, Any] | None = None) -> Any:
    cmd = ["curl", "-sS", url]
    if body is not None:
        cmd = ["curl", "-sS", "-X", "POST", url, "-H", "Content-Type: application/json", "-d", json.dumps(body)]
    return json.loads(subprocess.check_output(cmd, timeout=15))


def scan_live_frontier() -> dict[str, Any]:
    """Read-only public scan of Arcus T5/T6 thresholds, Arcus RWA spreads, and Entropy (io:*) markets."""
    arc_markets_raw = _curl_json("https://api.arcus.xyz/v1/markets")["markets"]
    total_arc_24h = 0.0
    arc_rwa: list[dict[str, Any]] = []
    for m in arc_markets_raw:
        if m.get("status") != "ONLINE" or m.get("type") != "PERPETUAL":
            continue
        vol = float(m.get("volume24hNotional") or 0.0)
        total_arc_24h += vol
        if m.get("hasCustomFeeSchedule"):
            arc_rwa.append(
                {
                    "symbol": m["marketDisplayName"],
                    "vol24h_usd": vol,
                    "tick_size": float(m["tickSize"]),
                    "step_size": float(m["stepSize"]),
                    "min_notional": float(m["minOrderNotional"]),
                }
            )
    arc_rwa.sort(key=lambda r: r["vol24h_usd"], reverse=True)
    for r in arc_rwa[:15]:
        try:
            b = _curl_json(f"https://api.arcus.xyz/v1/bbo/{r['symbol']}")
            bb, bs = float(b["bestBid"]["price"]), float(b["bestBid"]["size"])
            ba, asz = float(b["bestAsk"]["price"]), float(b["bestAsk"]["size"])
            mid = 0.5 * (bb + ba)
            r["spread_bps"] = round((ba - bb) / mid * 1e4, 3)
            r["l1_bid_usd"] = round(bb * bs, 2)
            r["l1_ask_usd"] = round(ba * asz, 2)
            r["spread_ticks"] = round((ba - bb) / r["tick_size"])
        except Exception as exc:
            r["bbo_error"] = str(exc)

    io_meta, io_ctxs = _curl_json("https://api.hyperliquid.xyz/info", {"type": "metaAndAssetCtxs", "dex": "io"})
    io_rows: list[dict[str, Any]] = []
    for m, c in zip(io_meta["universe"], io_ctxs):
        if m.get("isDelisted"):
            continue
        vol = float(c.get("dayNtlVlm") or 0.0)
        gm = m.get("growthMode") == "enabled"
        maker_fee_bps = 0.30 if gm else 3.00
        row: dict[str, Any] = {
            "coin": m["name"],
            "vol24h_usd": vol,
            "growth_mode": gm,
            "maker_fee_bps": maker_fee_bps,
            "sz_decimals": m["szDecimals"],
            "max_leverage": m["maxLeverage"],
        }
        io_rows.append(row)
    io_rows.sort(key=lambda r: r["vol24h_usd"], reverse=True)
    for r in io_rows[:12]:
        try:
            b = _curl_json("https://api.hyperliquid.xyz/info", {"type": "l2Book", "coin": r["coin"]})
            lvls = b.get("levels") or [[], []]
            if lvls[0] and lvls[1]:
                bb, bs = float(lvls[0][0]["px"]), float(lvls[0][0]["sz"])
                ba, asz = float(lvls[1][0]["px"]), float(lvls[1][0]["sz"])
                mid = 0.5 * (bb + ba)
                sp = (ba - bb) / mid * 1e4
                r["spread_bps"] = round(sp, 3)
                r["half_spread_minus_fee_bps"] = round(0.5 * sp - r["maker_fee_bps"], 3)
                r["l1_bid_usd"] = round(bb * bs, 2)
                r["l1_ask_usd"] = round(ba * asz, 2)
        except Exception as exc:
            r["bbo_error"] = str(exc)

    return {
        "arcus_total_24h_vol_usd": round(total_arc_24h, 2),
        # Total maker volume equals 1x total trade volume (every trade has 1 maker side).
        "arcus_implied_t5_daily_maker_usd_1p5pct": round(total_arc_24h * 0.015, 2),
        "arcus_implied_t6_daily_maker_usd_4pct": round(total_arc_24h * 0.04, 2),
        "arcus_top15_rwa": arc_rwa[:15],
        "entropy_total_24h_vol_usd": round(sum(r["vol24h_usd"] for r in io_rows), 2),
        "entropy_top12": io_rows[:12],
    }


def main() -> int:
    ap = argparse.ArgumentParser(description="Queue-priority MM offline simulator v3 & live frontier scanner")
    ap.add_argument("--scan-live-frontier", action="store_true", help="Scan live public Arcus/Entropy APIs")
    ap.add_argument("--tokyo-rtt", action="store_true", help="Use realistic Tokyo GCP Arcus RTT (36ms submit / 33ms cancel)")
    ap.add_argument("--captures", nargs="*", help="Capture pairs as path:symbol:ref_sources:ref_ids")
    ap.add_argument("--output", type=Path, help="Optional JSON output path")
    args = ap.parse_args()

    order_ms, cancel_ms = (36, 33) if args.tokyo_rtt else (10, 10)
    out: dict[str, Any] = {"rtt_profile_ms": {"submit": order_ms, "cancel": cancel_ms}}
    if args.scan_live_frontier:
        out["live_frontier"] = scan_live_frontier()

    if args.captures:
        runs: list[dict[str, Any]] = []
        for spec in args.captures:
            path_s, sym, rsrcs_s, rids_s = spec.split(":", 3)
            events, tick = load_capture_events(
                Path(path_s),
                sym,
                set(rsrcs_s.split(",")) if rsrcs_s else set(),
                set(rids_s.split(",")) if rids_s else set(),
            )
            arm_results = [simulate_arm(events, tick, arm) for arm in default_arms(0.0, order_ms, cancel_ms)]
            runs.append({"capture": path_s, "symbol": sym, "tick": tick, "events": len(events), "arms": arm_results})
        out["simulations"] = runs

    rendered = json.dumps(out, indent=2)
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(rendered + "\n", encoding="utf-8")
    print(rendered)
    return 0


if __name__ == "__main__":
    sys.exit(main())
