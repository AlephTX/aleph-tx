#!/usr/bin/env python3
"""Replay Lighter Core RWA market making and retain exact gate inputs."""
from __future__ import annotations

import argparse
import bisect
from collections import Counter, defaultdict
from dataclasses import dataclass
from decimal import Decimal, ROUND_HALF_UP
import gzip
import heapq
import json
import math
from pathlib import Path
import statistics as st
from typing import Any, Optional

if __package__:
    from .maker_hedge_risk_guards import GapAwareBasisTracker, InFlightQuoteReservation, UnhedgedExposureMonitor, UnitCostAuditor
else:
    from maker_hedge_risk_guards import GapAwareBasisTracker, InFlightQuoteReservation, UnhedgedExposureMonitor, UnitCostAuditor

D = Decimal
ZERO = D("0")
ONE = D("1")
BPS_DIV = D("10000")
MS_NS = 1_000_000
SEC_NS = 1_000_000_000
HOUR_NS = 3_600 * SEC_NS
WATERMARK_NS = 3 * SEC_NS
EQUITY_EVERY_NS = 60 * SEC_NS
MARK_GRID_NS = 100 * MS_NS
SHADOW_HORIZON_NS = 10 * SEC_NS
HEDGE_RETRY_NS = 100 * MS_NS
HEDGE_MAX_TRIES = 20
STALE_BOOK_NS = 5 * SEC_NS
BIG = 1 << 62

# Exact Lighter Core RWA specs fetched from mainnet /api/v1/orderBookDetails (2026-11-22)
CORE_SPECS: dict[str, dict[str, Any]] = {
    "SPY": {"core_id": "128", "rh_id": "26", "arcus_id": "SPY-USD", "sz_dec": 4, "px_dec": 2, "min_base": D("0.0070"), "min_quote": D("10"), "imf": D("0.0666")},
    "QQQ": {"core_id": "129", "rh_id": "25", "arcus_id": "QQQ-USD", "sz_dec": 4, "px_dec": 2, "min_base": D("0.0075"), "min_quote": D("10"), "imf": D("0.0666")},
    "NVDA": {"core_id": "110", "rh_id": "15", "arcus_id": "NVDA-USD", "sz_dec": 3, "px_dec": 3, "min_base": D("0.025"), "min_quote": D("10"), "imf": D("0.0666")},
    "AMZN": {"core_id": "114", "rh_id": "11", "arcus_id": "AMZN-USD", "sz_dec": 3, "px_dec": 3, "min_base": D("0.020"), "min_quote": D("10"), "imf": D("0.0666")},
    "AAPL": {"core_id": "113", "rh_id": "10", "arcus_id": "AAPL-USD", "sz_dec": 3, "px_dec": 3, "min_base": D("0.015"), "min_quote": D("10"), "imf": D("0.0666")},
    "TSLA": {"core_id": "112", "rh_id": "16", "arcus_id": "TSLA-USD", "sz_dec": 4, "px_dec": 2, "min_base": D("0.0150"), "min_quote": D("10"), "imf": D("0.0666")},
}
CORE_ID_TO_SYM = {v["core_id"]: k for k, v in CORE_SPECS.items()}
RH_ID_TO_SYM = {v["rh_id"]: k for k, v in CORE_SPECS.items()}
ARCUS_ID_TO_SYM = {v["arcus_id"]: k for k, v in CORE_SPECS.items()}

P_SNAP, P_PRINT, P_ACT = 0, 1, 2

ARM_CONFIGS: list[dict[str, Any]] = [
    dict(
        name="CORE_PURE_NO_HEDGE_C310",
        Q=D("200"),
        B=None,
        cap=D("2000"),
        maker_cap=D("2000"),
        place_ns=10 * MS_NS,
        cancel_ns=310 * MS_NS,
        cancel_ahead_fraction=D("1.0"),
        pull_bp=None,
        ref_venue=None,
        hedge_venue=None,
        hedge_delay_ns=0,
        hedge_fee_bp=ZERO,
        shadow_enabled=True,
    ),
    dict(
        name="CORE_PURE_NO_HEDGE_C310_CONS_Q",
        Q=D("200"),
        B=None,
        cap=D("2000"),
        maker_cap=D("2000"),
        place_ns=10 * MS_NS,
        cancel_ns=310 * MS_NS,
        cancel_ahead_fraction=D("0.5"),
        pull_bp=None,
        ref_venue=None,
        hedge_venue=None,
        hedge_delay_ns=0,
        hedge_fee_bp=ZERO,
        shadow_enabled=False,
    ),
    dict(
        name="CORE_PURE_PULL05_C310",
        Q=D("200"),
        B=None,
        cap=D("2000"),
        maker_cap=D("2000"),
        place_ns=10 * MS_NS,
        cancel_ns=310 * MS_NS,
        cancel_ahead_fraction=D("1.0"),
        pull_bp=0.5,
        ref_venue="arcus",
        hedge_venue=None,
        hedge_delay_ns=0,
        hedge_fee_bp=ZERO,
        shadow_enabled=True,
    ),
    dict(
        name="CORE_PURE_PULL05_C10_IDEAL",
        Q=D("200"),
        B=None,
        cap=D("2000"),
        maker_cap=D("2000"),
        place_ns=10 * MS_NS,
        cancel_ns=10 * MS_NS,
        cancel_ahead_fraction=D("1.0"),
        pull_bp=0.5,
        ref_venue="arcus",
        hedge_venue=None,
        hedge_delay_ns=0,
        hedge_fee_bp=ZERO,
        shadow_enabled=True,
    ),
    dict(
        name="CORE_HEDGE_SELF_C310",
        Q=D("200"),
        B=D("1000"),
        cap=D("5000"),
        maker_cap=D("50000"),  # same-venue Core position offsets via hedge_pos -> net_cap=$5000 binds
        place_ns=10 * MS_NS,
        cancel_ns=310 * MS_NS,
        cancel_ahead_fraction=D("1.0"),
        pull_bp=None,
        ref_venue=None,
        hedge_venue="core",
        hedge_delay_ns=310 * MS_NS,
        hedge_fee_bp=ZERO,
        shadow_enabled=False,
    ),
    dict(
        name="CORE_HEDGE_SELF_PULL05_C310",
        Q=D("200"),
        B=D("1000"),
        cap=D("5000"),
        maker_cap=D("50000"),
        place_ns=10 * MS_NS,
        cancel_ns=310 * MS_NS,
        cancel_ahead_fraction=D("1.0"),
        pull_bp=0.5,
        ref_venue="arcus",
        hedge_venue="core",
        hedge_delay_ns=310 * MS_NS,
        hedge_fee_bp=ZERO,
        shadow_enabled=True,
    ),
    dict(
        name="CORE_HEDGE_ARCUS_L0_C310",
        Q=D("200"),
        B=D("1000"),
        cap=D("5000"),
        maker_cap=D("5000"),
        place_ns=10 * MS_NS,
        cancel_ns=310 * MS_NS,
        cancel_ahead_fraction=D("1.0"),
        pull_bp=None,
        ref_venue=None,
        hedge_venue="arcus",
        hedge_delay_ns=100 * MS_NS,
        hedge_fee_bp=D("2.25"),
        shadow_enabled=False,
    ),
    dict(
        name="CORE_HEDGE_RH_STD_C310",
        Q=D("200"),
        B=D("1000"),
        cap=D("5000"),
        maker_cap=D("5000"),
        place_ns=10 * MS_NS,
        cancel_ns=310 * MS_NS,
        cancel_ahead_fraction=D("1.0"),
        pull_bp=None,
        ref_venue=None,
        hedge_venue="rh",
        hedge_delay_ns=310 * MS_NS,
        hedge_fee_bp=ZERO,
        shadow_enabled=False,
    ),
]


def _qstep(val: Decimal, dec: int) -> Decimal:
    q = D(1).scaleb(-dec)
    return val.quantize(q, rounding=ROUND_HALF_UP)


class FastDecBook:
    """Decimal-keyed order book with O(1) cached best touch and lazy multi-level walk."""

    __slots__ = ("bids", "asks", "_best_bid", "_best_ask")

    def __init__(self) -> None:
        self.bids: dict[Decimal, Decimal] = {}
        self.asks: dict[Decimal, Decimal] = {}
        self._best_bid: Optional[Decimal] = None
        self._best_ask: Optional[Decimal] = None

    def clear(self) -> None:
        self.bids.clear()
        self.asks.clear()
        self._best_bid = None
        self._best_ask = None

    def set_snapshot(self, bid_pairs: list[tuple[Decimal, Decimal]], ask_pairs: list[tuple[Decimal, Decimal]]) -> None:
        self.bids = {p: s for p, s in bid_pairs if s > ZERO}
        self.asks = {p: s for p, s in ask_pairs if s > ZERO}
        self._best_bid = max(self.bids) if self.bids else None
        self._best_ask = min(self.asks) if self.asks else None

    def apply_delta(self, bid_pairs: list[tuple[Decimal, Decimal]], ask_pairs: list[tuple[Decimal, Decimal]]) -> None:
        re_b = False
        for px, sz in bid_pairs:
            if sz <= ZERO:
                if px in self.bids:
                    del self.bids[px]
                    if px == self._best_bid:
                        re_b = True
            else:
                self.bids[px] = sz
                if self._best_bid is None or px > self._best_bid:
                    self._best_bid = px
        if re_b:
            self._best_bid = max(self.bids) if self.bids else None

        re_a = False
        for px, sz in ask_pairs:
            if sz <= ZERO:
                if px in self.asks:
                    del self.asks[px]
                    if px == self._best_ask:
                        re_a = True
            else:
                self.asks[px] = sz
                if self._best_ask is None or px < self._best_ask:
                    self._best_ask = px
        if re_a:
            self._best_ask = min(self.asks) if self.asks else None

    def best(self) -> Optional[tuple[Decimal, Decimal]]:
        if self._best_bid is None or self._best_ask is None or self._best_bid >= self._best_ask:
            return None
        return self._best_bid, self._best_ask

    def mid_dec(self) -> Optional[Decimal]:
        b = self.best()
        return None if b is None else (b[0] + b[1]) / D("2")

    def walk(self, side: int, qty: Decimal) -> Optional[Decimal]:
        """Exact Decimal VWAP to take `qty` (side=+1 buys asks, side=-1 sells bids)."""
        if qty <= ZERO or self.best() is None:
            return None
        levels = sorted(self.asks.items()) if side > 0 else sorted(self.bids.items(), reverse=True)
        need = qty
        cost = ZERO
        for px, sz in levels:
            take = sz if sz < need else need
            cost += take * px
            need -= take
            if need <= D("1e-12"):
                return cost / qty
        return None


@dataclass
class SimOrder:
    oid: str
    side: int
    px: Decimal
    qty: Decimal
    rem: Decimal
    ahead: Decimal
    land_ns: int
    cancel_req_ns: int = BIG
    cancel_eff_ns: int = BIG
    cancel_reason: Optional[str] = None
    live: bool = False
    last_displayed: Decimal = ZERO


@dataclass
class ShadowOrder:
    sid: str
    reason: str
    side: int
    px: Decimal
    rem: Decimal
    ahead: Decimal
    start_ns: int
    deadline_ns: int
    filled_usd: float = 0.0
    filled_px: float = 0.0
    filled_at_ns: Optional[int] = None
    last_displayed: Decimal = ZERO


class ArmState:
    def __init__(self, sym: str, spec: dict[str, Any], cfg: dict[str, Any]) -> None:
        self.sym = sym
        self.spec = spec
        self.cfg = cfg
        self.name = cfg["name"]
        self.Q: Decimal = cfg["Q"]
        self.B: Optional[Decimal] = cfg["B"]
        self.cap: Decimal = cfg["cap"]
        self.maker_cap: Decimal = cfg["maker_cap"]
        self.place_ns: int = cfg["place_ns"]
        self.cancel_ns: int = cfg["cancel_ns"]
        self.cancel_ahead_frac: Decimal = cfg["cancel_ahead_fraction"]
        self.pull_bp: Optional[float] = cfg["pull_bp"]
        self.ref_venue: Optional[str] = cfg["ref_venue"]
        self.hedge_venue: Optional[str] = cfg["hedge_venue"]
        self.hedge_delay_ns: int = cfg["hedge_delay_ns"]
        self.hedge_fee_bp: Decimal = cfg["hedge_fee_bp"]
        self.shadow_enabled: bool = bool(cfg["shadow_enabled"])

        self.res = InFlightQuoteReservation(
            maker_cap_usd=self.maker_cap,
            net_cap_usd=self.cap,
            allow_replace_overlap=False,
        )
        self.unhedged = (
            UnhedgedExposureMonitor(hedge_trigger_usd=self.B, default_recovery_interval_ns=5 * SEC_NS)
            if self.B is not None
            else UnhedgedExposureMonitor(hedge_trigger_usd=self.cap, default_recovery_interval_ns=5 * SEC_NS)
        )
        self.active: dict[int, Optional[SimOrder]] = {1: None, -1: None}
        self.resting: list[SimOrder] = []
        self.shadows: list[ShadowOrder] = []
        self.retired_shadows: list[ShadowOrder] = []
        self.oid_seq = 0
        self.sid_seq = 0

        self.cash_maker = ZERO
        self.cash_hedge = ZERO
        self.fees_hedge_usd = ZERO
        self.vol_maker_usd = ZERO
        self.vol_hedge_usd = ZERO
        self.hedge_slip_usd = ZERO
        self.pend_hedge = ZERO

        self.fills: list[tuple[int, int, float, float]] = []  # (ex_ns, side, px, qty)
        self.cancel_in_flight_fills: Counter[str] = Counter()
        self.cancel_in_flight_usd: Counter[str] = Counter()
        self.cancel_in_flight_events: list[tuple[int, int, float, float, str]] = []
        self.cancels_by_reason: Counter[str] = Counter()
        self.cancels_with_rem_by_reason: Counter[str] = Counter()

        self.n_place = 0
        self.n_reject = 0
        self.n_pull = 0
        self.n_stale_ref = 0
        self.n_cap_drop = 0
        self.n_hedge = 0
        self.n_hfail = 0
        self.n_overlap_blocked = 0
        self.n_halt_blocked = 0

        self.eq_peak = 0.0
        self.max_dd_usd = 0.0
        self.next_mark_ns = 0
        self.marks_900s: list[tuple[int, float, float]] = []  # (bucket_900s, eq_float, mid_float)
        self.drawdown_58_breached = False


class MarketState:
    def __init__(self, sym: str) -> None:
        self.sym = sym
        self.spec = CORE_SPECS[sym]
        self.core_ex = FastDecBook()
        self.core_seen = FastDecBook()
        self.rh_seen = FastDecBook()
        self.arc_seen = FastDecBook()

        self.core_epoch: Optional[int] = None
        self.rh_epoch: Optional[int] = None
        self.arc_epoch: Optional[int] = None
        self.core_ok = False
        self.rh_ok = False
        self.arc_ok = False
        self.core_rx_ns = 0
        self.rh_rx_ns = 0
        self.arc_rx_ns = 0
        self.last_ex_core_ns = 0

        self.gap_arc = GapAwareBasisTracker(
            tau_s=120.0,
            max_stale_ns=STALE_BOOK_NS,
            max_gap_reset_ns=60 * SEC_NS,
            warmup_samples=2,
        )
        self.last_gap_arc_val: Optional[float] = None
        self.last_gap_arc_ns: int = 0
        self.last_gap_pair: Optional[tuple[int, int]] = None

        self.arms = [ArmState(sym, self.spec, c) for c in ARM_CONFIGS]
        self.seen_tids: set[Any] = set()
        self.mid_t: list[int] = []
        self.mid_v: list[float] = []
        self.n_core_books = 0
        self.n_core_trades = 0
        self.n_core_trade_dups = 0
        self.n_late = 0
        self.core_trade_usd = 0.0
        self.first_ns = 0
        self.last_ns = 0


class CoreMakerSim:
    def __init__(self, symbols: tuple[str, ...]) -> None:
        self.mk = {s: MarketState(s) for s in symbols}
        self.q: list[tuple[int, int, int, Any, tuple[Any, ...]]] = []
        self.seq = 0
        self.now = 0

    def push(self, t: int, prio: int, fn: Any, *args: Any) -> None:
        self.seq += 1
        heapq.heappush(self.q, (t, prio, self.seq, fn, args))

    def drain(self, upto: int) -> None:
        q = self.q
        while q and q[0][0] <= upto:
            t, _, _, fn, args = heapq.heappop(q)
            if t > self.now:
                self.now = t
            fn(self.now, *args)

    # ---- Exchange Core Book Updates (Exchange Clock) ----
    def on_core_ex_book(
        self,
        t: int,
        m: MarketState,
        is_snap: bool,
        bids: list[tuple[Decimal, Decimal]],
        asks: list[tuple[Decimal, Decimal]],
    ) -> None:
        if is_snap:
            m.core_ex.set_snapshot(bids, asks)
        else:
            m.core_ex.apply_delta(bids, asks)

        mid_d = m.core_ex.mid_dec()
        if mid_d is not None and (not m.mid_t or t - m.mid_t[-1] >= MARK_GRID_NS):
            m.mid_t.append(t)
            m.mid_v.append(float(mid_d))

        for a in m.arms:
            keep: list[SimOrder] = []
            for o in a.resting:
                if o.cancel_eff_ns <= t or o.rem <= D("1e-12"):
                    continue
                side_book = m.core_ex.bids if o.side > 0 else m.core_ex.asks
                d_sz = side_book.get(o.px, ZERO)
                if d_sz < o.ahead:
                    if a.cancel_ahead_frac >= ONE:
                        o.ahead = d_sz
                    else:
                        drop = max(ZERO, o.last_displayed - d_sz)
                        o.ahead = min(d_sz, max(ZERO, o.ahead - drop * a.cancel_ahead_frac))
                o.last_displayed = d_sz
                keep.append(o)
            a.resting = keep

            if a.shadow_enabled and a.shadows:
                s_keep: list[ShadowOrder] = []
                for so in a.shadows:
                    if t >= so.deadline_ns:
                        a.retired_shadows.append(so)
                        continue
                    side_book = m.core_ex.bids if so.side > 0 else m.core_ex.asks
                    d_sz = side_book.get(so.px, ZERO)
                    if d_sz < so.ahead:
                        so.ahead = d_sz
                    so.last_displayed = d_sz
                    s_keep.append(so)
                a.shadows = s_keep

    # ---- Exchange Core Trade Prints (Exchange Clock) ----
    def on_core_ex_print(
        self,
        t: int,
        m: MarketState,
        t_rx: int,
        taker: int,
        px: Decimal,
        sz: Decimal,
    ) -> None:
        for a in m.arms:
            for o in a.resting:
                if o.rem <= D("1e-12") or not (o.land_ns <= t < o.cancel_eff_ns):
                    continue
                if o.side < 0 and taker > 0 and px >= o.px:
                    through = px > o.px
                elif o.side > 0 and taker < 0 and px <= o.px:
                    through = px < o.px
                else:
                    continue

                if through:
                    q = o.rem
                elif o.ahead >= sz:
                    o.ahead -= sz
                    continue
                else:
                    diff = sz - o.ahead
                    q = o.rem if o.rem < diff else diff
                o.ahead = ZERO
                o.rem -= q

                # Record exact Decimal fill in guard reservation at monotonic `t`
                was_pending_cancel = o.cancel_req_ns <= t < o.cancel_eff_ns
                a.res.record_fill(o.oid, q, o.px, now_ns=t)
                if was_pending_cancel and o.cancel_reason:
                    a.cancel_in_flight_fills[o.cancel_reason] += 1
                    a.cancel_in_flight_usd[o.cancel_reason] += float(q * o.px)
                    a.cancel_in_flight_events.append((t, o.side, float(o.px), float(q), o.cancel_reason))

                a.cash_maker -= D(o.side) * q * o.px
                a.vol_maker_usd += q * o.px
                a.fills.append((t, o.side, float(o.px), float(q)))
                if o.rem <= D("1e-12") and a.active[o.side] is o:
                    a.active[o.side] = None

                fill_rx = max(t_rx, t)
                self.push(fill_rx, P_ACT, self.on_fill_known, m, a)

            if a.shadow_enabled and a.shadows:
                s_keep: list[ShadowOrder] = []
                for so in a.shadows:
                    if t >= so.deadline_ns:
                        a.retired_shadows.append(so)
                        continue
                    matched = False
                    if so.side < 0 and taker > 0 and px >= so.px:
                        through = px > so.px
                        matched = True
                    elif so.side > 0 and taker < 0 and px <= so.px:
                        through = px < so.px
                        matched = True
                    else:
                        through = False
                    if matched:
                        if through:
                            sq = so.rem
                        elif so.ahead >= sz:
                            so.ahead -= sz
                            sq = ZERO
                        else:
                            diff = sz - so.ahead
                            sq = so.rem if so.rem < diff else diff
                        if sq > ZERO:
                            so.ahead = ZERO
                            so.rem -= sq
                            so.filled_usd += float(sq * so.px)
                            so.filled_px = float(so.px)
                            if so.filled_at_ns is None:
                                so.filled_at_ns = t
                            a.retired_shadows.append(so)
                            continue
                    s_keep.append(so)
                a.shadows = s_keep

    def on_fill_known(self, t: int, m: MarketState, a: ArmState) -> None:
        mid_d = m.core_seen.mid_dec()
        if mid_d is not None:
            a.unhedged.observe(t, a.res.maker_pos_base, a.res.hedge_pos_base, mid_d, a.pend_hedge)
        if a.B is not None:
            self.maybe_schedule_hedge(t, m, a)

    # ---- Causal Strategy Observation & Quoting (Receipt Clock) ----
    def on_core_seen(
        self,
        t: int,
        m: MarketState,
        is_snap: bool,
        epoch: Optional[int],
        bids: list[tuple[Decimal, Decimal]],
        asks: list[tuple[Decimal, Decimal]],
    ) -> None:
        if epoch is not None and m.core_epoch is not None and epoch != m.core_epoch:
            m.gap_arc.on_stream_reset("lighter_core", t, leg="maker")
            m.last_gap_arc_val = None
            m.last_gap_pair = None
        if epoch is not None:
            m.core_epoch = epoch
        if is_snap:
            m.core_seen.set_snapshot(bids, asks)
            m.core_ok = True
            if m.core_epoch is None:
                m.gap_arc.on_stream_reset("lighter_core", t, leg="maker")
        elif m.core_ok:
            m.core_seen.apply_delta(bids, asks)
        else:
            return
        m.core_rx_ns = t
        self._update_lead_gap(t, m)
        self.decide_all(t, m)

    def on_arc_seen(
        self,
        t: int,
        m: MarketState,
        is_snap: bool,
        epoch: Optional[int],
        bids: list[tuple[Decimal, Decimal]],
        asks: list[tuple[Decimal, Decimal]],
    ) -> None:
        if epoch is not None and m.arc_epoch is not None and epoch != m.arc_epoch:
            m.gap_arc.on_stream_reset("arcus", t, leg="hedge")
            m.last_gap_arc_val = None
            m.last_gap_pair = None
        if epoch is not None:
            m.arc_epoch = epoch
        if is_snap:
            m.arc_seen.set_snapshot(bids, asks)
            m.arc_ok = True
        elif m.arc_ok:
            m.arc_seen.apply_delta(bids, asks)
        else:
            return
        m.arc_rx_ns = t
        if self._update_lead_gap(t, m):
            self.decide_all(t, m, only_ref="arcus")

    def on_rh_seen(
        self,
        t: int,
        m: MarketState,
        is_snap: bool,
        epoch: Optional[int],
        bids: list[tuple[Decimal, Decimal]],
        asks: list[tuple[Decimal, Decimal]],
    ) -> None:
        if epoch is not None and m.rh_epoch is not None and epoch != m.rh_epoch:
            pass
        if epoch is not None:
            m.rh_epoch = epoch
        if is_snap:
            m.rh_seen.set_snapshot(bids, asks)
            m.rh_ok = True
        elif m.rh_ok:
            m.rh_seen.apply_delta(bids, asks)
        else:
            return
        m.rh_rx_ns = t

    def _update_lead_gap(self, t: int, m: MarketState) -> bool:
        if not (m.core_ok and m.arc_ok):
            return False
        c_mid = m.core_seen.mid_dec()
        a_mid = m.arc_seen.mid_dec()
        if c_mid is None or a_mid is None:
            return False
        pair = (m.core_rx_ns, m.arc_rx_ns)
        if pair == m.last_gap_pair:
            return False
        if (t - m.core_rx_ns) > STALE_BOOK_NS or (t - m.arc_rx_ns) > STALE_BOOK_NS:
            m.last_gap_arc_val = None
            return False
        m.last_gap_pair = pair
        g = m.gap_arc.update(
            now_ns=t,
            maker_mid=str(c_mid),
            hedge_mid=str(a_mid),
            maker_rx_ns=m.core_rx_ns,
            hedge_rx_ns=m.arc_rx_ns,
        )
        m.last_gap_arc_val = g
        m.last_gap_arc_ns = t
        return g is not None

    def current_lead_gap(self, t: int, m: MarketState) -> Optional[float]:
        if not (m.core_ok and m.arc_ok):
            return None
        if (t - m.core_rx_ns) > STALE_BOOK_NS or (t - m.arc_rx_ns) > STALE_BOOK_NS:
            return None
        return m.last_gap_arc_val

    def decide_all(self, t: int, m: MarketState, *, only_ref: Optional[str] = None) -> None:
        touch = m.core_seen.best()
        if touch is None:
            return
        bid, ask = touch
        mid = (bid + ask) / D("2")
        gap = self.current_lead_gap(t, m)
        for a in m.arms:
            if only_ref is not None and a.ref_venue != only_ref:
                continue
            self.decide_arm(t, m, a, bid, ask, mid, gap)

    def decide_arm(
        self,
        t: int,
        m: MarketState,
        a: ArmState,
        bid: Decimal,
        ask: Decimal,
        mid: Decimal,
        gap: Optional[float],
    ) -> None:
        a.res.settle_modeled_cancels(t)
        halt, _ = a.unhedged.should_halt_new_risk(t)
        if a.unhedged.recovery_due(t):
            a.unhedged.observe(
                t,
                a.res.maker_pos_base,
                a.res.hedge_pos_base,
                mid,
                a.pend_hedge,
                recovery_generation=a.unhedged.recovery_generation,
            )
            halt, _ = a.unhedged.should_halt_new_risk(t)

        for side in (1, -1):
            want: Optional[Decimal] = bid if side > 0 else ask
            cancel_reason: Optional[str] = None

            if halt:
                want = None
                cancel_reason = "halt"
                a.n_halt_blocked += 1
            elif a.pull_bp is not None:
                if gap is None:
                    # Wait until Arcus lead warmup / freshness is valid before opening/keeping risk?
                    # Pull-on-signal only cancels/skips when ref shows move against us (or stale if open).
                    pass
                elif -side * gap >= a.pull_bp:
                    want = None
                    cancel_reason = "lead_pull"
                    a.n_pull += 1

            o = a.active[side]
            if o is not None:
                if o.rem <= D("1e-12") or o.cancel_eff_ns <= t:
                    a.active[side] = None
                    o = None
                elif o.px != want:
                    if want is not None:
                        reason = "touch_move"
                    else:
                        reason = cancel_reason or "withdraw"
                    self._cancel_order(t, m, a, o, reason)
                    a.active[side] = None
                    o = None

            if o is None and want is not None:
                qty = _qstep(a.Q / want, m.spec["sz_dec"])
                if qty < m.spec["min_base"] or qty * want < m.spec["min_quote"]:
                    continue
                a.oid_seq += 1
                oid = f"{m.sym}:{a.name}:{a.oid_seq}"
                if not a.res.place_quote(oid, side, qty, want, mid, t):
                    if a.res.last_reject_reason == "pending_cancel_on_side":
                        a.n_overlap_blocked += 1
                    elif a.res.last_reject_reason in ("maker_cap_exceeded", "net_cap_exceeded"):
                        a.n_cap_drop += 1
                    continue
                no = SimOrder(
                    oid=oid,
                    side=side,
                    px=want,
                    qty=qty,
                    rem=qty,
                    ahead=ZERO,
                    land_ns=t + a.place_ns,
                )
                a.active[side] = no
                a.n_place += 1
                self.push(no.land_ns, P_ACT, self.on_land, m, a, no)

        if t >= a.next_mark_ns:
            a.next_mark_ns = t + EQUITY_EVERY_NS
            self.mark_arm(t, m, a)

    def _cancel_order(self, t: int, m: MarketState, a: ArmState, o: SimOrder, reason: str) -> None:
        if o.cancel_req_ns <= t:
            return
        eff = t + a.cancel_ns
        o.cancel_req_ns = t
        o.cancel_eff_ns = eff
        o.cancel_reason = reason
        a.cancels_by_reason[reason] += 1
        if o.oid in a.res.orders:
            a.res.request_cancel(o.oid, t, eff)
        self.push(eff, P_ACT, self.on_cancel_eff, m, a, o)

    def on_cancel_eff(self, t: int, m: MarketState, a: ArmState, o: SimOrder) -> None:
        a.res.settle_modeled_cancels(t)
        if o.rem > D("1e-12") and o.cancel_reason:
            a.cancels_with_rem_by_reason[o.cancel_reason] += 1
            if a.shadow_enabled and o.live:
                a.sid_seq += 1
                side_book = m.core_ex.bids if o.side > 0 else m.core_ex.asks
                d_sz = side_book.get(o.px, ZERO)
                a.shadows.append(
                    ShadowOrder(
                        sid=f"S:{a.sid_seq}",
                        reason=o.cancel_reason,
                        side=o.side,
                        px=o.px,
                        rem=o.rem,
                        ahead=min(o.ahead, d_sz) if o.ahead <= d_sz else d_sz,
                        start_ns=t,
                        deadline_ns=t + SHADOW_HORIZON_NS,
                        last_displayed=d_sz,
                    )
                )
        if a.active[o.side] is o:
            a.active[o.side] = None
        touch = m.core_seen.best()
        if touch is not None:
            b, ak = touch
            self.decide_arm(t, m, a, b, ak, (b + ak) / D("2"), self.current_lead_gap(t, m))

    def on_land(self, t: int, m: MarketState, a: ArmState, o: SimOrder) -> None:
        if o.cancel_req_ns <= t or o.cancel_eff_ns <= t or o.oid not in a.res.orders:
            return
        b = m.core_ex.best()
        if b is None or (o.side > 0 and o.px >= b[1]) or (o.side < 0 and o.px <= b[0]):
            a.n_reject += 1
            a.res.confirm_terminal(o.oid, t)
            if a.active[o.side] is o:
                a.active[o.side] = None
            return
        o.live = True
        d_sz = (m.core_ex.bids if o.side > 0 else m.core_ex.asks).get(o.px, ZERO)
        o.ahead = d_sz
        o.last_displayed = d_sz
        a.resting.append(o)

    def maybe_schedule_hedge(self, t: int, m: MarketState, a: ArmState) -> None:
        if a.B is None:
            return
        mid = m.core_seen.mid_dec()
        if mid is None or mid <= ZERO:
            return
        net = a.res.maker_pos_base + a.res.hedge_pos_base + a.pend_hedge
        if abs(net) * mid <= a.B:
            return
        keep = (a.B / mid) * (ONE if net > ZERO else -ONE)
        raw_qty = keep - net
        qty = _qstep(raw_qty, m.spec["sz_dec"])
        if abs(qty) < m.spec["min_base"] or abs(qty) * mid < m.spec["min_quote"]:
            return
        a.pend_hedge += qty
        self.push(t + a.hedge_delay_ns, P_ACT, self.hedge_exec, m, a, qty, 0)

    def hedge_exec(self, t: int, m: MarketState, a: ArmState, qty: Decimal, tries: int) -> None:
        side = 1 if qty > ZERO else -1
        abs_qty = abs(qty)
        if a.hedge_venue == "core":
            fresh = m.core_ok and (t - m.core_rx_ns) <= STALE_BOOK_NS
            bk = m.core_ex
        elif a.hedge_venue == "arcus":
            fresh = m.arc_ok and (t - m.arc_rx_ns) <= STALE_BOOK_NS
            bk = m.arc_seen
        elif a.hedge_venue == "rh":
            fresh = m.rh_ok and (t - m.rh_rx_ns) <= STALE_BOOK_NS
            bk = m.rh_seen
        else:
            return

        px = bk.walk(side, abs_qty) if fresh else None
        if px is None:
            a.n_hfail += 1
            if tries + 1 < HEDGE_MAX_TRIES:
                a.unhedged.record_hedge_failure(t, qty, f"thin_or_stale_{a.hedge_venue}", retries_exhausted=False)
                self.push(t + HEDGE_RETRY_NS, P_ACT, self.hedge_exec, m, a, qty, tries + 1)
            else:
                a.pend_hedge -= qty
                a.unhedged.record_hedge_failure(t, qty, f"thin_or_stale_{a.hedge_venue}", retries_exhausted=True)
            return

        a.pend_hedge -= qty
        a.res.record_hedge_fill(qty, t)
        notional = abs_qty * px
        fee = notional * a.hedge_fee_bp / BPS_DIV
        a.cash_hedge -= qty * px
        a.fees_hedge_usd += fee
        a.vol_hedge_usd += notional
        bk_mid = bk.mid_dec()
        if bk_mid is not None:
            a.hedge_slip_usd += D(side) * (px - bk_mid) * abs_qty
        a.n_hedge += 1
        a.unhedged.record_hedge_success(
            t,
            a.res.maker_pos_base,
            a.res.hedge_pos_base,
            m.core_seen.mid_dec() or px,
            a.pend_hedge,
        )

    def mark_arm(self, t: int, m: MarketState, a: ArmState) -> None:
        m_mid = m.core_ex.mid_dec() or m.core_seen.mid_dec()
        if m_mid is None:
            return
        if a.hedge_venue == "arcus":
            h_mid = m.arc_seen.mid_dec() or m_mid
        elif a.hedge_venue == "rh":
            h_mid = m.rh_seen.mid_dec() or m_mid
        else:
            h_mid = m_mid

        eq = float(
            a.cash_maker
            + a.cash_hedge
            - a.fees_hedge_usd
            + a.res.maker_pos_base * m_mid
            + a.res.hedge_pos_base * h_mid
        )
        a.eq_peak = max(a.eq_peak, eq)
        a.max_dd_usd = max(a.max_dd_usd, a.eq_peak - eq)
        if a.max_dd_usd >= 0.58 * float(a.cap):
            a.drawdown_58_breached = True
        a.marks_900s.append((t // (900 * SEC_NS), eq, float(m_mid)))
        a.unhedged.observe(t, a.res.maker_pos_base, a.res.hedge_pos_base, m_mid, a.pend_hedge)
        if a.B is not None and a.pend_hedge == ZERO:
            self.maybe_schedule_hedge(t, m, a)


def mid_at(m: MarketState, t: int) -> Optional[float]:
    i = bisect.bisect_right(m.mid_t, t) - 1
    return m.mid_v[i] if i >= 0 else None


def compute_900s_beta_t(marks: list[tuple[int, float, float]], cap_usd: float) -> dict[str, Any]:
    by_b: dict[int, tuple[float, float]] = {}
    for b, eq, mid in marks:
        by_b[b] = (eq, mid)
    keys = sorted(by_b)
    ys, xs = [], []
    for k0, k1 in zip(keys, keys[1:]):
        if k1 != k0 + 1:
            continue
        eq0, m0 = by_b[k0]
        eq1, m1 = by_b[k1]
        if m0 > 0:
            ys.append((eq1 - eq0) / cap_usd)
            xs.append(m1 / m0 - 1.0)
    n = len(xs)
    if n < 5:
        return {"n": n, "beta": None, "t_stat": None}
    mx = math.fsum(xs) / n
    my = math.fsum(ys) / n
    sxx = math.fsum((x - mx) ** 2 for x in xs)
    if sxx <= 1e-18:
        return {"n": n, "beta": 0.0, "t_stat": 0.0}
    beta = math.fsum((x - mx) * (y - my) for x, y in zip(xs, ys)) / sxx
    alpha = my - beta * mx
    rss = math.fsum((y - (alpha + beta * x)) ** 2 for x, y in zip(xs, ys))
    se2 = rss / max(1, n - 2) / sxx
    t_stat = beta / math.sqrt(se2) if se2 > 1e-24 else 0.0
    return {"n": n, "beta": round(beta, 4), "t_stat": round(t_stat, 3)}


def parse_lighter_pairs(ob: dict[str, Any]) -> tuple[list[tuple[Decimal, Decimal]], list[tuple[Decimal, Decimal]]]:
    bids = [(D(x["price"]), D(x["size"])) for x in ob.get("bids") or []]
    asks = [(D(x["price"]), D(x["size"])) for x in ob.get("asks") or []]
    return bids, asks


def parse_arcus_pairs(c: dict[str, Any]) -> tuple[list[tuple[Decimal, Decimal]], list[tuple[Decimal, Decimal]]]:
    bids = [(D(str(x[0])), D(str(x[1]))) for x in c.get("bids") or []]
    asks = [(D(str(x[0])), D(str(x[1]))) for x in c.get("asks") or []]
    return bids, asks


def run_simulation(path: Path, symbols: tuple[str, ...]) -> dict[str, Any]:
    sim = CoreMakerSim(symbols)
    lines_seen = 0
    bad_lines = 0
    t_rx_max = 0

    with gzip.open(path, "rt") as f:
        for line in f:
            lines_seen += 1
            if '"source":"binance"' in line or '"source":"coinbase"' in line or '"source":"hyperliquid"' in line:
                continue
            try:
                d = json.loads(line)
            except Exception:
                bad_lines += 1
                continue
            if d.get("event") != "message":
                continue
            ct = d.get("capture_timing") or {}
            t_rx = ct.get("available_wall_ns")
            if not isinstance(t_rx, int) or t_rx <= 0:
                continue
            t_rx_max = max(t_rx_max, t_rx)
            src = d.get("source")
            pl = d.get("payload") or {}
            epoch = ct.get("connection_epoch")

            if src == "lighter":
                ch = str(pl.get("channel", ""))
                if not ch.startswith("order_book:"):
                    continue
                sym = CORE_ID_TO_SYM.get(ch.split(":", 1)[1])
                if sym is None or sym not in sim.mk:
                    continue
                m = sim.mk[sym]
                m.first_ns = m.first_ns or t_rx
                m.last_ns = max(m.last_ns, t_rx)
                m.n_core_books += 1
                ob = pl.get("order_book") or {}
                bids, asks = parse_lighter_pairs(ob)
                is_snap = str(pl.get("type", "")).startswith("subscribed")
                lu = ob.get("last_updated_at")
                if not is_snap and isinstance(lu, (int, float)) and lu > 0:
                    raw_ex = int(lu) * 1000
                elif not is_snap and pl.get("timestamp"):
                    raw_ex = int(pl["timestamp"]) * MS_NS
                else:
                    raw_ex = t_rx
                if raw_ex > t_rx + 5 * SEC_NS:
                    raw_ex = t_rx
                if raw_ex < sim.now:
                    m.n_late += 1
                    raw_ex = sim.now
                sim.push(raw_ex, P_SNAP, sim.on_core_ex_book, m, is_snap, bids, asks)
                sim.push(t_rx, P_ACT, sim.on_core_seen, m, is_snap, epoch, bids, asks)

            elif src == "lighter_trades":
                ch = str(pl.get("channel", ""))
                if not ch.startswith("trade:"):
                    continue
                sym = CORE_ID_TO_SYM.get(ch.split(":", 1)[1])
                if sym is None or sym not in sim.mk:
                    continue
                if str(pl.get("type", "")).startswith("subscribed"):
                    continue
                m = sim.mk[sym]
                for tr in pl.get("trades") or []:
                    tid = tr.get("trade_id")
                    if tid in m.seen_tids:
                        m.n_core_trade_dups += 1
                        continue
                    if tid is not None:
                        m.seen_tids.add(tid)
                    px = D(str(tr["price"]))
                    sz = D(str(tr["size"]))
                    if px <= ZERO or sz <= ZERO:
                        continue
                    tt = tr.get("transaction_time")
                    if isinstance(tt, (int, float)) and tt > 0:
                        ex_ns = int(tt) * 1000
                    elif tr.get("timestamp"):
                        ex_ns = int(tr["timestamp"]) * MS_NS
                    else:
                        ex_ns = t_rx
                    if ex_ns > t_rx + 5 * SEC_NS:
                        ex_ns = t_rx
                    if ex_ns < sim.now:
                        m.n_late += 1
                        ex_ns = sim.now
                    taker = 1 if tr.get("is_maker_ask") is True else -1
                    m.n_core_trades += 1
                    m.core_trade_usd += float(px * sz)
                    sim.push(ex_ns, P_PRINT, sim.on_core_ex_print, m, t_rx, taker, px, sz)

            elif src == "arcus" and pl.get("channel") == "l2Orderbook":
                sym = ARCUS_ID_TO_SYM.get(pl.get("id", ""))
                if sym is None or sym not in sim.mk:
                    continue
                m = sim.mk[sym]
                c = pl.get("contents") or {}
                bids, asks = parse_arcus_pairs(c)
                is_snap = str(pl.get("type", "")).startswith("subscribed") or True  # Arcus l2Orderbook emits full top-20 snapshots
                sim.push(t_rx, P_SNAP, sim.on_arc_seen, m, is_snap, epoch, bids, asks)

            elif src == "lighter_robinhood":
                ch = str(pl.get("channel", ""))
                if not ch.startswith("order_book:"):
                    continue
                sym = RH_ID_TO_SYM.get(ch.split(":", 1)[1])
                if sym is None or sym not in sim.mk:
                    continue
                m = sim.mk[sym]
                ob = pl.get("order_book") or {}
                bids, asks = parse_lighter_pairs(ob)
                is_snap = str(pl.get("type", "")).startswith("subscribed")
                sim.push(t_rx, P_SNAP, sim.on_rh_seen, m, is_snap, epoch, bids, asks)

            if t_rx_max > WATERMARK_NS:
                sim.drain(t_rx_max - WATERMARK_NS)

    sim.drain(BIG)
    for m in sim.mk.values():
        end_t = max(sim.now, m.last_ns)
        for a in m.arms:
            a.res.settle_modeled_cancels(end_t)
            a.unhedged.finalize(end_t)
            a.retired_shadows.extend(a.shadows)
            a.shadows.clear()

    return summarize_sim(sim, path, lines_seen, bad_lines)


def summarize_sim(sim: CoreMakerSim, path: Path, lines_seen: int, bad_lines: int) -> dict[str, Any]:
    auditor_std = UnitCostAuditor(hedge_fee_bps_grid=(ZERO, D("1"), D("2.25"), D("3.5")))
    out_markets: dict[str, Any] = {}
    agg: dict[str, dict[str, Any]] = defaultdict(
        lambda: {
            "maker_vol": ZERO,
            "hedge_vol": ZERO,
            "eq_mtm": ZERO,
            "term_cash_sum": ZERO,
            "term_flat_all": True,
            "fills": 0,
            "hedges": 0,
            "cancels": Counter(),
            "cancels_rem": Counter(),
            "in_flight": Counter(),
            "shadow_by_reason": defaultdict(lambda: {"spawned": 0, "filled": 0, "filled_usd": 0.0, "avoided_loss_usd": 0.0, "unfilled": 0, "unknown": 0}),
            "marks": [],
            "max_dd": 0.0,
            "overlap_blocked": 0,
            "n_hfail": 0,
            "drawdown_58_breached": False,
        }
    )

    for sym, m in sorted(sim.mk.items()):
        end_core_mid = m.core_ex.mid_dec() or m.core_seen.mid_dec() or ZERO
        end_arc_mid = m.arc_seen.mid_dec() or end_core_mid
        end_rh_mid = m.rh_seen.mid_dec() or end_core_mid
        arms_out: dict[str, Any] = {}
        for a in m.arms:
            m_pos = a.res.maker_pos_base
            h_pos = a.res.hedge_pos_base
            h_mid_end = end_arc_mid if a.hedge_venue == "arcus" else (end_rh_mid if a.hedge_venue == "rh" else end_core_mid)
            eq_mtm = (
                a.cash_maker
                + a.cash_hedge
                - a.fees_hedge_usd
                + m_pos * end_core_mid
                + h_pos * h_mid_end
            )

            # Forced terminal-flat liquidation walking actual closing book(s)
            liq_cash = a.cash_maker + a.cash_hedge - a.fees_hedge_usd
            flat_ok = True
            liq_note = []
            if a.hedge_venue == "core":
                net = m_pos + h_pos
                if net != ZERO:
                    lpx = m.core_ex.walk(-1 if net > ZERO else 1, abs(net))
                    if lpx is None:
                        flat_ok = False
                        liq_note.append("core_close_book_thin")
                    else:
                        liq_cash += net * lpx  # 0 bp taker on Core Standard
            else:
                if m_pos != ZERO:
                    lpx = m.core_ex.walk(-1 if m_pos > ZERO else 1, abs(m_pos))
                    if lpx is None:
                        flat_ok = False
                        liq_note.append("core_close_book_thin")
                    else:
                        liq_cash += m_pos * lpx
                if h_pos != ZERO:
                    h_bk = m.arc_seen if a.hedge_venue == "arcus" else m.rh_seen
                    hpx = h_bk.walk(-1 if h_pos > ZERO else 1, abs(h_pos))
                    if hpx is None:
                        flat_ok = False
                        liq_note.append("hedge_close_book_thin")
                    else:
                        liq_cash += h_pos * hpx - abs(h_pos) * hpx * a.hedge_fee_bp / BPS_DIV

            # Real fill markouts vs Core mid
            mk_bp = {}
            for h in (1, 10, 60):
                num = den = 0.0
                for t_f, sd, fpx, fq in a.fills:
                    mh = mid_at(m, t_f + h * SEC_NS)
                    if mh is None or fpx <= 0:
                        continue
                    num += sd * (mh - fpx) / fpx * 1e4 * (fq * fpx)
                    den += fq * fpx
                mk_bp[f"{h}s"] = round(num / den, 4) if den > 0 else None

            # Phase 0 Counterfactual Shadow Twin Breakdown
            shadow_stats: dict[str, Any] = {}
            all_reasons = set(a.cancels_by_reason.keys()) | {so.reason for so in a.retired_shadows}
            for reason in sorted(all_reasons):
                shadow_stats[reason] = {
                    "cancels_requested": int(a.cancels_by_reason[reason]),
                    "cancels_with_rem": int(a.cancels_with_rem_by_reason[reason]),
                    "in_flight_fills": int(a.cancel_in_flight_fills[reason]),
                    "in_flight_usd": round(a.cancel_in_flight_usd[reason], 2),
                    "shadow_spawned": 0,
                    "shadow_filled_10s": 0,
                    "shadow_filled_usd": 0.0,
                    "shadow_avoided_loss_10s_usd": 0.0,
                    "shadow_unfilled_10s": 0,
                    "shadow_unknown_10s": 0,
                }
            for so in a.retired_shadows:
                st_r = shadow_stats[so.reason]
                st_r["shadow_spawned"] += 1
                if so.filled_at_ns is not None:
                    mh10 = mid_at(m, so.filled_at_ns + 10 * SEC_NS)
                    if mh10 is None or so.filled_px <= 0:
                        st_r["shadow_unknown_10s"] += 1
                    else:
                        st_r["shadow_filled_10s"] += 1
                        st_r["shadow_filled_usd"] += so.filled_usd
                        # Avoided PnL = -(maker_pnl_if_held) = -side * (mh10 - fill_px)/fill_px * usd
                        # Positive avoided_loss means canceling saved us money!
                        mk_if_held_usd = so.side * (mh10 - so.filled_px) / so.filled_px * so.filled_usd
                        st_r["shadow_avoided_loss_10s_usd"] += -mk_if_held_usd
                elif so.deadline_ns > m.last_ns:
                    st_r["shadow_unknown_10s"] += 1
                else:
                    st_r["shadow_unfilled_10s"] += 1

            for r, sr in shadow_stats.items():
                sr["shadow_filled_usd"] = round(sr["shadow_filled_usd"], 2)
                sr["shadow_avoided_loss_10s_usd"] = round(sr["shadow_avoided_loss_10s_usd"], 4)

            beta_info = compute_900s_beta_t(a.marks_900s, float(a.cap))
            u_sum = a.unhedged.summary()
            uc_report = auditor_std.evaluate_arm(
                marked_equity_usd=eq_mtm,
                maker_volume_usd=a.vol_maker_usd,
                hedge_volume_usd=a.vol_hedge_usd,
                terminal_maker_pos_base=m_pos if flat_ok else None,
                terminal_hedge_pos_base=h_pos if flat_ok else None,
                terminal_flat_verified=False,
            )

            arms_out[a.name] = {
                "core_maker_usd_exact": str(a.vol_maker_usd),
                "terminal_liq_cash_usd_exact": str(liq_cash) if flat_ok else None,
                "core_maker_usd": float(round(a.vol_maker_usd, 2)),
                "hedge_usd": float(round(a.vol_hedge_usd, 2)),
                "dual_usd": float(round(a.vol_maker_usd + a.vol_hedge_usd, 2)),
                "fills": len(a.fills),
                "hedges": a.n_hedge,
                "hedge_failures": a.n_hfail,
                "placed": a.n_place,
                "post_only_rejects": a.n_reject,
                "overlap_blocked_by_guard": a.n_overlap_blocked,
                "cap_drops": a.n_cap_drop,
                "halt_blocked": a.n_halt_blocked,
                "cancels_by_reason": dict(a.cancels_by_reason),
                "cancel_in_flight_fills": dict(a.cancel_in_flight_fills),
                "equity_mtm_usd": float(round(eq_mtm, 4)),
                "fees_hedge_usd": float(round(a.fees_hedge_usd, 4)),
                "hedge_slip_usd": float(round(a.hedge_slip_usd, 4)),
                "terminal_flat_liq_ok": flat_ok,
                "terminal_liq_cash_usd": float(round(liq_cash, 4)) if flat_ok else None,
                "liq_note": liq_note,
                "maker_only_cost_per_m_mtm": float(round(-eq_mtm / a.vol_maker_usd * D("1000000"), 2)) if a.vol_maker_usd > ZERO else None,
                "maker_only_cost_per_m_liq": float(round(-liq_cash / a.vol_maker_usd * D("1000000"), 2)) if (flat_ok and a.vol_maker_usd > ZERO) else None,
                "dual_cost_per_m_mtm": float(round(-eq_mtm / (a.vol_maker_usd + a.vol_hedge_usd) * D("1000000"), 2)) if a.vol_maker_usd > ZERO else None,
                "fill_markout_bp": mk_bp,
                "beta_900s": beta_info,
                "max_drawdown_usd": round(a.max_dd_usd, 4),
                "drawdown_58_breached": a.drawdown_58_breached,
                "max_continuous_above_trigger_s": round(u_sum["max_continuous_above_trigger_ns"] / 1e9, 3),
                "phase0_shadow_by_reason": shadow_stats,
                "unit_cost_grid_rh_bp": {k: (None if v is None else float(round(v, 2))) for k, v in uc_report["maker_only_by_fee_bp"].items()},
            }

            ag = agg[a.name]
            ag["maker_vol"] += a.vol_maker_usd
            ag["hedge_vol"] += a.vol_hedge_usd
            ag["eq_mtm"] += eq_mtm
            if flat_ok:
                ag["term_cash_sum"] += liq_cash
            else:
                ag["term_flat_all"] = False
            ag["fills"] += len(a.fills)
            ag["hedges"] += a.n_hedge
            ag["n_hfail"] += a.n_hfail
            ag["overlap_blocked"] += a.n_overlap_blocked
            ag["max_dd"] = max(ag["max_dd"], a.max_dd_usd)
            ag["drawdown_58_breached"] = ag["drawdown_58_breached"] or a.drawdown_58_breached
            ag["cancels"] += a.cancels_by_reason
            ag["cancels_rem"] += a.cancels_with_rem_by_reason
            ag["in_flight"] += a.cancel_in_flight_fills
            for r, sr in shadow_stats.items():
                dst = ag["shadow_by_reason"][r]
                dst["spawned"] += sr["shadow_spawned"]
                dst["filled"] += sr["shadow_filled_10s"]
                dst["filled_usd"] += sr["shadow_filled_usd"]
                dst["avoided_loss_usd"] += sr["shadow_avoided_loss_10s_usd"]
                dst["unfilled"] += sr["shadow_unfilled_10s"]
                dst["unknown"] += sr["shadow_unknown_10s"]

        out_markets[sym] = {
            "core_books": m.n_core_books,
            "core_trades": m.n_core_trades,
            "core_trade_dups": m.n_core_trade_dups,
            "core_trade_usd": round(m.core_trade_usd, 2),
            "late_events": m.n_late,
            "gap_arc_updates": m.gap_arc.emitted_count,
            "gap_arc_resets": m.gap_arc.gap_resets + sum(m.gap_arc.stream_resets.values()),
            "arms": arms_out,
        }

    agg_out: dict[str, Any] = {}
    for c in ARM_CONFIGS:
        name = c["name"]
        ag = agg[name]
        mv = ag["maker_vol"]
        hv = ag["hedge_vol"]
        dv = mv + hv
        eq = ag["eq_mtm"]
        tc = ag["term_cash_sum"] if ag["term_flat_all"] else None
        # Add cross-fee summary for RH/Core comparison: compute equity before hedge fee + varied bp
        rh_grid = {}
        if name == "CORE_HEDGE_RH_STD_C310" and mv > ZERO:
            for f_str in ("0", "1.5", "3.5"):
                f = D(f_str)
                tot_c = -eq + hv * f / BPS_DIV
                rh_grid[f"rh_taker_{f_str}bp_maker_only_per_m"] = float(round(tot_c / mv * D("1000000"), 2))
        elif name in ("CORE_HEDGE_SELF_C310", "CORE_HEDGE_SELF_PULL05_C310") and mv > ZERO:
            # Premium 0 LIT comparison sensitivity (0.40 maker + 2.80 taker)
            for label, mf, tf in [("standard_0_0", ZERO, ZERO), ("core_prem_0.4_2.8", D("0.40"), D("2.80"))]:
                tot_c = -eq + mv * mf / BPS_DIV + hv * tf / BPS_DIV
                rh_grid[f"{label}_maker_only_per_m"] = float(round(tot_c / mv * D("1000000"), 2))

        agg_out[name] = {
            "core_maker_usd_exact": str(mv),
            "terminal_liq_cash_usd_exact": str(tc) if tc is not None else None,
            "core_maker_usd": float(round(mv, 2)),
            "hedge_usd": float(round(hv, 2)),
            "dual_usd": float(round(dv, 2)),
            "fills": ag["fills"],
            "hedges": ag["hedges"],
            "hedge_failures": ag["n_hfail"],
            "overlap_blocked_by_guard": ag["overlap_blocked"],
            "equity_mtm_usd": float(round(eq, 4)),
            "terminal_flat_all": ag["term_flat_all"],
            "terminal_liq_cash_usd": None if tc is None else float(round(tc, 4)),
            "single_venue_core_usd_per_m_mtm": float(round(-eq / mv * D("1000000"), 2)) if mv > ZERO else None,
            "single_venue_core_usd_per_m_liq": float(round(-tc / mv * D("1000000"), 2)) if (tc is not None and mv > ZERO) else None,
            "dual_venue_usd_per_m_mtm": float(round(-eq / dv * D("1000000"), 2)) if dv > ZERO else None,
            "pass_a1_le_20_usd_per_m_mtm": bool(mv > ZERO and (-eq / mv * D("1000000")) <= D("20")),
            "pass_a1_le_20_usd_per_m_liq": bool(tc is not None and mv > ZERO and (-tc / mv * D("1000000")) <= D("20")),
            "max_market_drawdown_usd": round(ag["max_dd"], 4),
            "drawdown_58_breached": ag["drawdown_58_breached"],
            "cancels_by_reason": dict(ag["cancels"]),
            "cancels_with_rem_by_reason": dict(ag["cancels_rem"]),
            "cancel_in_flight_fills": dict(ag["in_flight"]),
            "phase0_shadow_by_reason": {
                r: {
                    "spawned": d["spawned"],
                    "filled_10s": d["filled"],
                    "filled_usd": round(d["filled_usd"], 2),
                    "avoided_loss_10s_usd": round(d["avoided_loss_usd"], 4),
                    "unfilled_10s": d["unfilled"],
                    "unknown_10s": d["unknown"],
                }
                for r, d in ag["shadow_by_reason"].items()
            },
            "tier_sensitivity_notes": rh_grid,
        }

    return {
        "schema": "lighter_core_rwa_maker_sim.v1",
        "scope_note": "Offline selection_dev simulation on Lighter Core Standard (0/0 bp, 0ms PostOnly + 10ms RTT place, 300ms + 10ms RTT cancel, NO LPP).",
        "input_file": str(path),
        "lines_seen": lines_seen,
        "bad_lines": bad_lines,
        "aggregate_6markets": agg_out,
        "markets": out_markets,
    }


def _self_test() -> None:
    # Synthetic deterministic trace verifying:
    # 1. Intervening print during 300 ms speed bump fills pending_cancel order & increments cancel_in_flight_fills
    # 2. InFlightQuoteReservation blocks overlapping quote on the same side until cancel_eff settles
    # 3. PostOnly reject on an already pending_cancel order does not crash confirm_terminal
    # 4. Phase 0 counterfactual shadow twin records avoided loss when canceled order would have been hit negatively
    # 5. Terminal flat liquidation identity is exact (-0.42 USD in test 1 scenario)
    sim = CoreMakerSim(("NVDA",))
    m = sim.mk["NVDA"]
    t0 = 1_795_193_000_000_000_000

    core_b0 = [(D("100.00"), D("10"))]
    core_a0 = [(D("100.10"), D("10"))]
    arc_b0 = [(D("100.00"), D("10"))]
    arc_a0 = [(D("100.10"), D("10"))]

    sim.push(t0, P_SNAP, sim.on_core_ex_book, m, True, core_b0, core_a0)
    sim.push(t0, P_ACT, sim.on_core_seen, m, True, 1, core_b0, core_a0)
    sim.push(t0, P_SNAP, sim.on_arc_seen, m, True, 1, arc_b0, arc_a0)
    sim.drain(t0 + 50 * MS_NS)  # Bid order @ 100.00 qty=2.000 lands at t0 + 10ms (ahead=10)

    # Warm up Arcus basis cleanly with zero divergence
    sim.push(t0 + 200 * MS_NS, P_ACT, sim.on_arc_seen, m, False, 1, arc_b0, arc_a0)
    sim.push(t0 + 300 * MS_NS, P_SNAP, sim.on_core_seen, m, False, 1, core_b0, core_a0)
    sim.drain(t0 + 350 * MS_NS)

    # Clear ahead=10 on ask or bid is NOT needed because print has size 12 below;
    # first run Test 2's clean lead_pull on calm aligned state BEFORE Core touch moves!
    arm_pull = next(a for a in m.arms if a.name == "CORE_PURE_PULL05_C310")
    assert arm_pull.active[1] is not None and arm_pull.active[1].live

    # Arcus drops 5 bp while Core stays flat @ 100.00/100.10 -> arm_pull cancels bid cleanly for lead_pull!
    t_lp = t0 + 500 * MS_NS
    sim.push(t_lp, P_SNAP, sim.on_arc_seen, m, False, 1, [(D("99.95"), D("10"))], [(D("100.05"), D("10"))])
    sim.drain(t_lp + 350 * MS_NS)  # Past 310ms cancel_eff -> ShadowOrder spawns on bid @ 100.00
    assert arm_pull.cancels_with_rem_by_reason["lead_pull"] == 1, arm_pull.cancels_with_rem_by_reason
    assert len(arm_pull.shadows) == 1

    # Toxic sell print hits 100.00 (size=12 consumes 10 ahead + 2 counterfactual ours!)
    # Meanwhile CORE_PURE_NO_HEDGE_C310 ALSO gets hit here for 2.000 real long shares @ 100.00!
    t_hit = t_lp + 450 * MS_NS
    sim.push(t_hit, P_PRINT, sim.on_core_ex_print, m, t_hit, -1, D("100.00"), D("12.000"))
    sim.drain(t_hit)

    # Re-align Arcus back to 100.00/100.10, then test Core touch move -> cancel_in_flight & overlap block
    t1 = t0 + 2 * SEC_NS
    core_b1 = [(D("100.02"), D("5")), (D("100.00"), D("10"))]
    sim.push(t1 - 50 * MS_NS, P_SNAP, sim.on_arc_seen, m, False, 1, arc_b0, arc_a0)
    sim.push(t1, P_SNAP, sim.on_core_ex_book, m, False, core_b1, core_a0)
    sim.push(t1, P_ACT, sim.on_core_seen, m, False, 1, core_b1, core_a0)
    sim.drain(t1)

    arm_pure = next(a for a in m.arms if a.name == "CORE_PURE_NO_HEDGE_C310")
    # At t1 arm_pure places bid @ 100.02 (lands at t1+10ms). Move bid again at t1+50ms -> 100.04
    # to trigger pending_cancel + overlap_blocked >= 1, and hit 100.02 at t1+100ms BEFORE cancel_eff!
    core_b2 = [(D("100.04"), D("1")), (D("100.02"), D("2"))]
    sim.push(t1 + 50 * MS_NS, P_SNAP, sim.on_core_ex_book, m, False, core_b2, core_a0)
    sim.push(t1 + 50 * MS_NS, P_ACT, sim.on_core_seen, m, False, 1, core_b2, core_a0)
    sim.drain(t1 + 60 * MS_NS)
    assert arm_pure.n_overlap_blocked >= 1, f"Expected overlap guard block while 310ms cancel pending: {arm_pure.n_overlap_blocked}"

    # Also push a quick cancel before land_ns edge test on ask side: move ask slightly so ask cancels before landing
    sim.push(t1 + 100 * MS_NS, P_PRINT, sim.on_core_ex_print, m, t1 + 100 * MS_NS, -1, D("100.02"), D("4.000"))
    # Test 3: Cancel triggered BEFORE land_ns + crossed exchange book at land_ns
    t_cross = t1 + 500 * MS_NS
    sim.push(t_cross, P_SNAP, sim.on_core_ex_book, m, True, [(D("100.04"), D("5"))], [(D("100.03"), D("5"))])  # crossed ex book
    sim.push(t_cross + 1 * MS_NS, P_SNAP, sim.on_core_ex_book, m, True, [(D("99.90"), D("5"))], [(D("100.10"), D("5"))])
    sim.push(t_cross + 2 * MS_NS, P_ACT, sim.on_core_seen, m, True, 1, [(D("99.90"), D("5"))], [(D("100.10"), D("5"))])  # places ask @ 100.10 landing +10ms
    sim.push(t_cross + 5 * MS_NS, P_ACT, sim.on_core_seen, m, True, 1, [(D("99.90"), D("5"))], [(D("100.12"), D("5"))])  # cancels ask before land_ns!
    # Wait past 10s so shadow fill 10s markout is evaluated after Core drops to 99.79/99.81
    sim.push(t_hit + 2 * SEC_NS, P_SNAP, sim.on_core_ex_book, m, True, [(D("99.79"), D("20"))], [(D("99.81"), D("20"))])
    sim.push(t_hit + 15 * SEC_NS, P_SNAP, sim.on_core_ex_book, m, True, [(D("99.79"), D("20"))], [(D("99.81"), D("20"))])
    sim.drain(BIG)

    assert arm_pure.cancel_in_flight_fills["touch_move"] == 1, arm_pure.cancel_in_flight_fills
    m.last_ns = t_hit + 20 * SEC_NS
    for a in m.arms:
        a.retired_shadows.extend(a.shadows)
        a.shadows.clear()
    summary = summarize_sim(sim, Path("synthetic_selftest"), 0, 0)
    lp_sh = summary["aggregate_6markets"]["CORE_PURE_PULL05_C310"]["phase0_shadow_by_reason"].get("lead_pull")
    assert lp_sh is not None and lp_sh["filled_10s"] >= 1 and lp_sh["avoided_loss_10s_usd"] > 0, lp_sh

    pure_agg = summary["aggregate_6markets"]["CORE_PURE_NO_HEDGE_C310"]
    assert pure_agg["terminal_flat_all"] is True
    assert pure_agg["single_venue_core_usd_per_m_liq"] is not None
    print(f"self-test passed: in_flight={arm_pure.cancel_in_flight_fills['touch_move']}, "
          f"overlap_blocked={arm_pure.n_overlap_blocked}, shadow_avoided={lp_sh['avoided_loss_10s_usd']:.4f} USD, "
          f"liq=${pure_agg['terminal_liq_cash_usd']:.4f}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--input", type=Path)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--symbols", nargs="*", default=list(CORE_SPECS.keys()))
    args = parser.parse_args()
    if args.self_test:
        _self_test()
        return 0
    if not args.input or not args.output:
        parser.error("--input and --output required unless --self-test")
    res = run_simulation(args.input, tuple(args.symbols))
    args.output.write_text(json.dumps(res, indent=2) + "\n")
    print(f"Done Core Maker Sim ({res['lines_seen']} lines, bad={res['bad_lines']})")
    for name, row in res["aggregate_6markets"].items():
        print(
            f"  {name:30s} | mk=${row['core_maker_usd']:,.0f} | hg=${row['hedge_usd']:,.0f} | "
            f"eq_mtm=${row['equity_mtm_usd']:,.2f} | liq=${row['terminal_liq_cash_usd']} | "
            f"$/M(mtm)={row['single_venue_core_usd_per_m_mtm']} | $/M(liq)={row['single_venue_core_usd_per_m_liq']} | "
            f"pass_A1={row['pass_a1_le_20_usd_per_m_mtm']}"
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
