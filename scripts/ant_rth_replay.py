"""Causal ANT two-venue replay for the pre-registered RTH validation gate."""
from __future__ import annotations

from dataclasses import dataclass
import gzip
import heapq
import json
import math
from pathlib import Path
from typing import Any, Optional

MS_NS = 1_000_000
SEC_NS = 1_000_000_000
HOUR_NS = 3_600 * SEC_NS
MAX_ARC_AGE_NS = 250 * MS_NS
STALE_XYZ_NS = 30 * SEC_NS
ARC_PASSIVE_RTT_NS = 100 * MS_NS
PASSIVE_ENTRY_TTL_NS = 10 * SEC_NS
PASSIVE_EXIT_WAIT_NS = 120 * SEC_NS
WATERMARK_NS = 3 * SEC_NS
BIG = 1 << 62

ARC_TAKER_BP = 1.12
ARC_MAKER_BP = 0.0
XYZ_TAKER_BP = 0.90
ARC_SOFR_HOURLY_FRAC = 0.000005104166666666
CAP_USD = 2000.0
ALLOCATED_CAPITAL_USD = 1000.0
DD_PAUSE_FRAC = 0.58


def walk_book(levels: list[tuple[float, float]], qty_base: float) -> Optional[float]:
    if qty_base <= 1e-12 or not levels:
        return None
    need = qty_base
    cost = 0.0
    for px, sz in levels:
        if px <= 0 or sz <= 0:
            continue
        take = sz if sz < need else need
        cost += take * px
        need -= take
        if need <= 1e-12:
            return cost / qty_base
    return None


def walk_book_partial(levels: list[tuple[float, float]], qty_base: float) -> tuple[float, float]:
    """Consume available depth up to `qty_base` without inventing nonexistent L1 size."""
    if qty_base <= 1e-12 or not levels:
        return 0.0, 0.0
    need = qty_base
    filled = 0.0
    cost = 0.0
    for px, sz in levels:
        if px <= 0 or sz <= 0:
            continue
        take = sz if sz < need else need
        cost += take * px
        filled += take
        need -= take
        if need <= 1e-12:
            break
    if filled <= 1e-12:
        return 0.0, 0.0
    return filled, cost / filled


@dataclass
class PassiveOrder:
    purpose: str  # "entry" or "exit"
    side: int  # +1 buy Arcus, -1 sell Arcus
    px: float
    qty: float
    rem: float
    ahead: float
    last_disp: float
    land_ns: int
    expire_ns: int
    cancel_eff_ns: int = BIG
    xyz_trigger_mid: float = 0.0
    t_sig_ns: int = 0
    done: bool = False


def crossable_marginal_base(
    buy_levels: list[tuple[float, float]],
    sell_levels: list[tuple[float, float]],
    min_raw_edge_bp: float,
    take_fraction: float,
    max_qty_base: float,
) -> float:
    """Two-pointer marginal crossable base quantity across both L2 ladders (`ARB-IMP-06`)."""
    if max_qty_base <= 1e-12 or not buy_levels or not sell_levels:
        return 0.0
    bi = 0
    si = 0
    b_rem = buy_levels[0][1]
    s_rem = sell_levels[0][1]
    q_cross = 0.0
    cap_gross = max_qty_base / max(1e-6, take_fraction)
    while bi < len(buy_levels) and si < len(sell_levels):
        b_px, _ = buy_levels[bi]
        s_px, _ = sell_levels[si]
        if b_px <= 0.0 or s_px <= 0.0:
            break
        slice_edge_bp = (s_px / b_px - 1.0) * 1e4
        if slice_edge_bp < min_raw_edge_bp:
            break
        take = b_rem if b_rem < s_rem else s_rem
        if take > 0.0:
            q_cross += take
            if q_cross >= cap_gross:
                q_cross = cap_gross
                break
        b_rem -= take
        s_rem -= take
        if b_rem <= 1e-12:
            bi += 1
            b_rem = buy_levels[bi][1] if bi < len(buy_levels) else 0.0
        if s_rem <= 1e-12:
            si += 1
            s_rem = sell_levels[si][1] if si < len(sell_levels) else 0.0
    return min(max_qty_base, q_cross * take_fraction)


@dataclass(frozen=True)
class AblationSpec:
    arm_family: str  # A0, A1, A2, A3
    mode: str
    lat_ms: int
    q_usd: float
    entry_bp: float
    exit_bp: float = 0.0
    max_hold_s: int = 7200
    persist_ms: int = 0
    post_trade_refresh_lock: bool = False
    allow_batch_add: bool = False
    floor_frac: float = 0.25
    scale_bps: float = 0.0
    convex_power: float = 1.0
    use_crossable_book: bool = False
    take_fraction: float = 0.5
    use_kalman_leung_li: bool = False
    max_entry_bp: float = 35.0
    use_adverse_carry_penalty: bool = False
    carry_hold_hours: float = 0.5
    dd_brake_budget_usd: float = 0.0

    @property
    def key(self) -> str:
        parts = [
            self.arm_family,
            self.mode,
            f"lat={self.lat_ms}ms",
            f"Q={int(self.q_usd)}",
            f"in={self.entry_bp:g}",
        ]
        if self.persist_ms > 0 or self.post_trade_refresh_lock:
            parts.append(f"p={self.persist_ms}ms")
        if self.allow_batch_add:
            parts.append(f"lad={self.scale_bps:g},p={self.convex_power:g}")
        if self.use_crossable_book:
            parts.append(f"cb={self.take_fraction:g}")
        if self.use_kalman_leung_li:
            parts.append(f"kl={self.max_entry_bp:g}")
        if self.use_adverse_carry_penalty:
            parts.append(f"carry={self.carry_hold_hours:g}h")
        if self.dd_brake_budget_usd > 0.0:
            parts.append(f"ddbrk=${self.dd_brake_budget_usd:g}")
        return "|".join(parts)


def _next_drawdown_brake(
    peak_equity: float,
    equity: float,
    budget_usd: float,
    prev_tier: int,
    day_start_equity: float,
) -> tuple[int, float]:
    """CRASH-IMP-01 progressive soft-brake (`{1.0, 0.75, 0.50, 0.25}` with `0.05` hysteresis)."""
    if prev_tier == 4 or equity <= 0 or day_start_equity - equity > budget_usd:
        return 4, 0.0
    r_dd = max(0.0, peak_equity - equity) / budget_usd if budget_usd > 0.0 else 0.0
    if r_dd >= 0.75:
        escalated = 3
    elif r_dd >= 0.50:
        escalated = 2
    elif r_dd >= 0.25:
        escalated = 1
    else:
        escalated = 0
    tier = max(0, min(3, prev_tier))
    if escalated >= tier:
        tier = escalated
    else:
        if tier == 3 and r_dd < 0.70:
            tier = 2
        if tier == 2 and r_dd < 0.45:
            tier = 1
        if tier == 1 and r_dd < 0.20:
            tier = 0
    return tier, {0: 1.0, 1: 0.75, 2: 0.50, 3: 0.25}[tier]


class SlowKalmanEquilibrium:
    """Spread-adaptive 1D scalar Kalman intercept for slow structural basis drift."""

    def __init__(self, q_var: float = 0.0025, r_base: float = 4.0) -> None:
        self.q_var = q_var
        self.r_base = r_base
        self.alpha: float = 0.0
        self.p_var: float = 25.0
        self.initialized = False
        self.samples = 0

    def update(self, obs_bp: float, combined_spread_bp: float) -> float:
        if not self.initialized:
            self.alpha = obs_bp
            self.p_var = 4.0
            self.initialized = True
            self.samples = 1
            return self.alpha
        r_var = self.r_base * (1.0 + max(0.0, combined_spread_bp) ** 2 * 0.1)
        p_pred = self.p_var + self.q_var
        k = p_pred / (p_pred + r_var)
        self.alpha = self.alpha + k * (obs_bp - self.alpha)
        self.p_var = (1.0 - k) * p_pred
        self.samples += 1
        return self.alpha


class AblationArmState:
    def __init__(self, spec: AblationSpec) -> None:
        self.spec = spec
        self.lat_ns = spec.lat_ms * MS_NS
        self.inv_arc = 0.0
        self.inv_xyz = 0.0
        self.cash = 0.0
        self.fees_usd = 0.0
        self.funding_usd = 0.0
        self.vol_arc = 0.0
        self.vol_xyz = 0.0
        self.lat_slip_usd = 0.0

        self.entry_open_ns: Optional[int] = None
        self.cycles_opened = 0
        self.cycles_closed = 0
        self.wins = 0
        self.cycle_pnls: list[float] = []
        self.hold_times_s: list[float] = []
        self._cycle_start_eq = 0.0

        self.pend_action = False
        self.pend_hedges = 0
        self.passive: Optional[PassiveOrder] = None

        self.last_trade_arc_rx_ns = -1
        self.last_trade_xyz_rx_ns = -1
        self.armed_since_ns: dict[int, int] = {}
        self.kalman = SlowKalmanEquilibrium()

        self.signals_seen = 0
        self.persist_armed_count = 0
        self.persist_blocked_count = 0
        self.leung_li_capped = 0
        self.kalman_filtered = 0
        self.adverse_carry_filtered = 0
        self.crossable_capped = 0
        self.skipped_thin = 0
        self.skipped_stale = 0
        self.thin_hedge_events = 0
        self.stale_hedge_events = 0
        self.maker_fills = 0
        self.maker_timeouts = 0
        self.forced_time_exits = 0
        self.missing_funding_hours_while_open = 0
        self.brake_tier = 0
        self.brake_scale = 1.0
        self.brake_tier_transitions = 0
        self.brake_scaled_entries = 0

        self.guard_day: int | None = None
        self.day_start = ALLOCATED_CAPITAL_USD
        self.day_peak = self.day_start
        self.eq_peak = 0.0
        self.max_dd_usd = 0.0
        self.halted_58 = False

    def equity_mtm(self, arc_mid: float, xyz_mid: float) -> float:
        return self.cash - self.fees_usd + self.funding_usd + self.inv_arc * arc_mid + self.inv_xyz * xyz_mid

    def update_dd(self, t: int, arc_mid: float, xyz_mid: float) -> float:
        eq = self.equity_mtm(arc_mid, xyz_mid)
        self.eq_peak = max(self.eq_peak, eq)
        self.max_dd_usd = max(self.max_dd_usd, self.eq_peak - eq)
        if self.max_dd_usd >= DD_PAUSE_FRAC * ALLOCATED_CAPITAL_USD:
            self.halted_58 = True
        if self.spec.dd_brake_budget_usd > 0.0:
            day = t // (24 * HOUR_NS)
            equity = ALLOCATED_CAPITAL_USD + eq
            if self.guard_day != day:
                self.guard_day = day
                self.day_start = self.day_peak = equity
                self.brake_tier, self.brake_scale = 0, 1.0
            self.day_peak = max(self.day_peak, equity)
            new_tier, new_scale = _next_drawdown_brake(
                self.day_peak, equity, self.spec.dd_brake_budget_usd, self.brake_tier, self.day_start
            )
            if new_tier != self.brake_tier:
                self.brake_tier = new_tier
                self.brake_scale = new_scale
                self.brake_tier_transitions += 1
        return eq

    def accrue_funding(self, arc_mid: float, xyz_mid: float, xyz_hourly_rate: float) -> None:
        if abs(self.inv_arc) <= 1e-12 and abs(self.inv_xyz) <= 1e-12:
            return
        self.funding_usd -= self.inv_arc * arc_mid * ARC_SOFR_HOURLY_FRAC
        self.funding_usd -= self.inv_xyz * xyz_mid * xyz_hourly_rate

    def ladder_add_penalty_bp(self, abs_pos_usd: float) -> float:
        if not self.spec.allow_batch_add or self.spec.scale_bps <= 0.0:
            return 0.0
        u = min(1.0, abs_pos_usd / CAP_USD)
        if u <= self.spec.floor_frac:
            return 0.0
        frac = (u - self.spec.floor_frac) / max(1e-9, 1.0 - self.spec.floor_frac)
        return self.spec.scale_bps * (frac ** self.spec.convex_power)


class AblationSim:
    def __init__(self, specs: list[AblationSpec]) -> None:
        self.arms = [AblationArmState(s) for s in specs]
        self.q: list[tuple[int, int, int, Any, tuple[Any, ...]]] = []
        self.seq = 0
        self.now = 0

        self.arc_ex_bids: list[tuple[float, float]] = []
        self.arc_ex_asks: list[tuple[float, float]] = []
        self.arc_seen_bids: list[tuple[float, float]] = []
        self.arc_seen_asks: list[tuple[float, float]] = []
        self.arc_rx_ns = 0

        self.xyz_bids: list[tuple[float, float]] = []
        self.xyz_asks: list[tuple[float, float]] = []
        self.xyz_rx_ns = 0
        self.xyz_funding_rate = 0.0
        self.xyz_funding_seen = False
        self.xyz_funding_rx_ns = 0
        self.next_hour_ns: Optional[int] = None
        self.funding_hours_witnessed = 0
        self.funding_hours_missing_witness = 0

        self.seen_tids: set[Any] = set()
        self.fresh_xyz_events = 0
        self.lines = 0
        self.gaps_detected: list[dict[str, Any]] = []

    def push(self, t: int, prio: int, fn: Any, *args: Any) -> None:
        self.seq += 1
        heapq.heappush(self.q, (t, prio, self.seq, fn, args))

    def drain(self, upto: int) -> None:
        while self.q and self.q[0][0] <= upto:
            t, _, _, fn, args = heapq.heappop(self.q)
            if t > self.now:
                if self.next_hour_ns is not None:
                    self._check_funding_tick(t)
                self.now = t
            fn(self.now, *args)

    def _arc_touch(self, seen: bool = True) -> Optional[tuple[float, float]]:
        b = self.arc_seen_bids if seen else self.arc_ex_bids
        a = self.arc_seen_asks if seen else self.arc_ex_asks
        if not b or not a or b[0][0] >= a[0][0]:
            return None
        return b[0][0], a[0][0]

    def _xyz_touch(self) -> Optional[tuple[float, float]]:
        if not self.xyz_bids or not self.xyz_asks or self.xyz_bids[0][0] >= self.xyz_asks[0][0]:
            return None
        return self.xyz_bids[0][0], self.xyz_asks[0][0]

    def on_arc_ex_book(self, t: int, bids: list[tuple[float, float]], asks: list[tuple[float, float]]) -> None:
        self.arc_ex_bids = bids
        self.arc_ex_asks = asks
        b_map = dict(bids)
        a_map = dict(asks)
        for arm in self.arms:
            po = arm.passive
            if po is None or po.done:
                continue
            if t >= po.expire_ns or t >= po.cancel_eff_ns:
                self._cancel_passive_cleanup(arm, po)
                continue
            d_sz = (b_map if po.side > 0 else a_map).get(po.px, 0.0)
            if d_sz < po.ahead:
                drop = max(0.0, po.last_disp - d_sz)
                po.ahead = min(d_sz, max(0.0, po.ahead - 0.5 * drop))
            po.last_disp = d_sz

    def on_arc_seen_book(self, t: int, bids: list[tuple[float, float]], asks: list[tuple[float, float]]) -> None:
        self._check_funding_tick(t)
        self.arc_seen_bids = bids
        self.arc_seen_asks = asks
        self.arc_rx_ns = t
        self._prune_armed_edges()

    def _raw_edges(self, ab: float, aa: float, xb: float, xa: float) -> tuple[float, float, float, float]:
        return (
            (xb / aa - 1.0) * 1e4,
            (ab / xa - 1.0) * 1e4,
            (xb / ab - 1.0) * 1e4,
            (aa / xa - 1.0) * 1e4,
        )

    def _effective_entry_requirements(self, arm: AblationArmState, direction: int, am: float, xm: float, raw_edge: float) -> tuple[bool, float, str]:
        """Return (admitted, min_raw_edge_bp, reject_tag) for entry direction +1 or -1."""
        pos_usd = abs(arm.inv_arc * am)
        eff_raw_entry_bp = arm.spec.entry_bp + arm.ladder_add_penalty_bp(pos_usd)
        if raw_edge > arm.spec.max_entry_bp:
            return False, eff_raw_entry_bp, "leung_li"
        mid_basis_bp = (am / xm - 1.0) * 1e4
        if arm.spec.use_kalman_leung_li:
            if arm.kalman.samples < 10:
                return False, eff_raw_entry_bp, "kalman"
            slow_eq = arm.kalman.alpha
            eq_disloc = (slow_eq - mid_basis_bp) if direction > 0 else (mid_basis_bp - slow_eq)
            if eq_disloc > arm.spec.max_entry_bp:
                return False, eff_raw_entry_bp, "leung_li"
            if eq_disloc < eff_raw_entry_bp:
                return False, eff_raw_entry_bp, "kalman"
        if arm.spec.use_adverse_carry_penalty:
            net_hourly_frac = direction * (self.xyz_funding_rate - ARC_SOFR_HOURLY_FRAC)
            if net_hourly_frac < 0.0:
                adverse_bp = (-net_hourly_frac) * 1e4 * arm.spec.carry_hold_hours
                eff_raw_entry_bp += adverse_bp
        return True, eff_raw_entry_bp, "ok"

    def _prune_armed_edges(self) -> None:
        at = self._arc_touch(True)
        xt = self._xyz_touch()
        if at is None or xt is None:
            for arm in self.arms:
                arm.armed_since_ns.clear()
            return
        ab, aa = at
        xb, xa = xt
        am = 0.5 * (ab + aa)
        xm = 0.5 * (xb + xa)
        ep_t, en_t, ep_m, en_m = self._raw_edges(ab, aa, xb, xa)
        for arm in self.arms:
            if not arm.armed_since_ns:
                continue
            for direction in list(arm.armed_since_ns.keys()):
                raw_edge = (ep_t if direction > 0 else en_t) if arm.spec.mode == "TAKER_TAKER" else (ep_m if direction > 0 else en_m)
                ok, req_bp, _ = self._effective_entry_requirements(arm, direction, am, xm, raw_edge)
                if not ok or raw_edge < req_bp:
                    arm.armed_since_ns.pop(direction, None)

    def on_arc_ex_print(self, t: int, t_rx: int, taker: int, px: float, sz: float) -> None:
        for arm in self.arms:
            po = arm.passive
            if po is None or po.done or not (po.land_ns <= t < min(po.expire_ns, po.cancel_eff_ns)):
                continue
            if po.side < 0 and taker > 0 and px >= po.px:
                through = px > po.px
            elif po.side > 0 and taker < 0 and px <= po.px:
                through = px < po.px
            else:
                continue
            if through:
                q = po.rem
            elif po.ahead >= sz:
                po.ahead -= sz
                continue
            else:
                q = min(po.rem, sz - po.ahead)
            if q <= 1e-12:
                continue
            po.ahead = 0.0
            po.rem -= q
            arm.inv_arc += po.side * q
            arm.cash -= po.side * q * po.px
            arm.vol_arc += q * po.px
            arm.maker_fills += 1
            arm.last_trade_arc_rx_ns = max(arm.last_trade_arc_rx_ns, self.arc_rx_ns)
            arm.last_trade_xyz_rx_ns = max(arm.last_trade_xyz_rx_ns, self.xyz_rx_ns)
            if po.rem <= 1e-12:
                po.done = True
                arm.passive = None
                arm.pend_action = False
            fill_rx = max(t_rx, t)
            arm.pend_hedges += 1
            self.push(fill_rx + arm.lat_ns, 2, self.exec_xyz_hedge, arm, -po.side, q, po.xyz_trigger_mid)

    def exec_xyz_hedge(self, t: int, arm: AblationArmState, side: int, qty: float, expected_mid: float) -> None:
        arm.pend_hedges = max(0, arm.pend_hedges - 1)
        unhedged = arm.inv_arc + arm.inv_xyz
        if abs(unhedged) <= 1e-9:
            return
        hedge_side = -1 if unhedged > 0 else 1
        target_qty = min(qty, abs(unhedged)) if side == hedge_side else abs(unhedged)
        if t - self.xyz_rx_ns > STALE_XYZ_NS:
            arm.skipped_stale += 1
            arm.stale_hedge_events += 1
            return
        levels = self.xyz_asks if hedge_side > 0 else self.xyz_bids
        filled, px = walk_book_partial(levels, target_qty)
        if filled <= 1e-12:
            arm.skipped_thin += 1
            arm.thin_hedge_events += 1
            return
        if filled + 1e-9 < target_qty:
            arm.thin_hedge_events += 1
        arm.inv_xyz += hedge_side * filled
        arm.cash -= hedge_side * filled * px
        notional = filled * px
        arm.vol_xyz += notional
        arm.fees_usd += notional * XYZ_TAKER_BP / 1e4
        if expected_mid > 0:
            arm.lat_slip_usd += hedge_side * (px - expected_mid) * filled
        arm.last_trade_xyz_rx_ns = max(arm.last_trade_xyz_rx_ns, self.xyz_rx_ns)
        self._after_position_update(t, arm)

    def _after_position_update(self, t: int, arm: AblationArmState) -> None:
        at = self._arc_touch(False) or self._arc_touch(True)
        xt = self._xyz_touch()
        if at and xt:
            eq = arm.update_dd(t, 0.5 * (at[0] + at[1]), 0.5 * (xt[0] + xt[1]))
            if abs(arm.inv_arc) <= 1e-9 and abs(arm.inv_xyz) <= 1e-9 and arm.entry_open_ns is not None:
                arm.cycles_closed += 1
                arm.hold_times_s.append((t - arm.entry_open_ns) / 1e9)
                cpnl = eq - arm._cycle_start_eq
                arm.cycle_pnls.append(cpnl)
                if cpnl > 0:
                    arm.wins += 1
                arm.entry_open_ns = None

    def _cancel_passive_cleanup(self, arm: AblationArmState, po: PassiveOrder) -> None:
        po.done = True
        arm.passive = None
        arm.pend_action = False
        arm.maker_timeouts += 1

    def _check_funding_tick(self, t: int) -> None:
        if self.next_hour_ns is None:
            self.next_hour_ns = ((t // HOUR_NS) + 1) * HOUR_NS
        while t >= self.next_hour_ns:
            h = self.next_hour_ns
            at = self._arc_touch(True)
            xt = self._xyz_touch()
            has_funding = self.xyz_funding_seen and 0 <= (h - self.xyz_funding_rx_ns) <= HOUR_NS
            books_fresh = (
                self.arc_rx_ns > 0
                and self.xyz_rx_ns > 0
                and 0 <= (h - self.arc_rx_ns) <= 300 * SEC_NS
                and 0 <= (h - self.xyz_rx_ns) <= 300 * SEC_NS
            )
            if at and xt and has_funding and books_fresh:
                am, xm = 0.5 * (at[0] + at[1]), 0.5 * (xt[0] + xt[1])
                self.funding_hours_witnessed += 1
                for arm in self.arms:
                    arm.accrue_funding(am, xm, self.xyz_funding_rate)
            else:
                self.funding_hours_missing_witness += 1
                for arm in self.arms:
                    if abs(arm.inv_arc) > 1e-12 or abs(arm.inv_xyz) > 1e-12:
                        arm.missing_funding_hours_while_open += 1
            self.next_hour_ns += HOUR_NS

    def on_xyz_funding(self, t: int, rate: float, rx_ns: int) -> None:
        self._check_funding_tick(t)
        self.xyz_funding_rate = rate
        self.xyz_funding_seen = True
        self.xyz_funding_rx_ns = rx_ns

    def _dual_refresh_ok(self, arm: AblationArmState) -> bool:
        if not arm.spec.post_trade_refresh_lock or arm.last_trade_xyz_rx_ns < 0:
            return True
        return self.arc_rx_ns > arm.last_trade_arc_rx_ns and self.xyz_rx_ns > arm.last_trade_xyz_rx_ns

    def _size_entry_qty(self, arm: AblationArmState, direction: int, eff_req_bp: float, ab: float, aa: float, am: float) -> float:
        remaining_cap_usd = max(0.0, CAP_USD - abs(arm.inv_arc * am))
        eff_q_usd = arm.spec.q_usd * (arm.brake_scale if arm.spec.dd_brake_budget_usd > 0.0 else 1.0)
        clip_usd = min(eff_q_usd, remaining_cap_usd)
        max_qty = clip_usd / am
        if max_qty * am < 5.0:
            return 0.0
        if not arm.spec.use_crossable_book:
            return max_qty
        if arm.spec.mode == "TAKER_TAKER":
            buy_lv = self.arc_seen_asks if direction > 0 else self.xyz_asks
            sell_lv = self.xyz_bids if direction > 0 else self.arc_seen_bids
        else:
            buy_lv = [(ab, max_qty / max(1e-6, arm.spec.take_fraction))] if direction > 0 else self.xyz_asks
            sell_lv = self.xyz_bids if direction > 0 else [(aa, max_qty / max(1e-6, arm.spec.take_fraction))]
        qty = crossable_marginal_base(buy_lv, sell_lv, eff_req_bp, arm.spec.take_fraction, max_qty)
        if qty * am < 5.0:
            arm.crossable_capped += 1
            return 0.0
        if qty + 1e-9 < max_qty:
            arm.crossable_capped += 1
        return qty

    def _try_trigger_entry(self, t: int, arm: AblationArmState, direction: int, eff_req_bp: float) -> None:
        if arm.pend_action or arm.pend_hedges > 0 or (arm.passive is not None and not arm.passive.done):
            return
        if arm.halted_58 or not self._dual_refresh_ok(arm):
            return
        at = self._arc_touch(True)
        xt = self._xyz_touch()
        if at is None or xt is None or (t - self.arc_rx_ns) > MAX_ARC_AGE_NS or (t - self.xyz_rx_ns) > STALE_XYZ_NS:
            return
        ab, aa = at
        xb, xa = xt
        am = 0.5 * (ab + aa)
        xm = 0.5 * (xb + xa)
        pos_usd = abs(arm.inv_arc * am)
        if pos_usd + 5.0 > CAP_USD:
            return
        if abs(arm.inv_arc) > 1e-9 and (not arm.spec.allow_batch_add or (arm.inv_arc > 0) != (direction > 0)):
            return
        ep_t, en_t, ep_m, en_m = self._raw_edges(ab, aa, xb, xa)
        raw_edge = (ep_t if direction > 0 else en_t) if arm.spec.mode == "TAKER_TAKER" else (ep_m if direction > 0 else en_m)
        ok, latest_req_bp, tag = self._effective_entry_requirements(arm, direction, am, xm, raw_edge)
        if not ok:
            if tag == "leung_li":
                arm.leung_li_capped += 1
            elif tag == "kalman":
                arm.kalman_filtered += 1
            return
        req_bp = max(eff_req_bp, latest_req_bp)
        if raw_edge < req_bp:
            return
        qty = self._size_entry_qty(arm, direction, req_bp, ab, aa, am)
        if qty <= 1e-12:
            return
        if arm.spec.dd_brake_budget_usd > 0.0 and arm.brake_scale < 1.0 - 1e-9:
            arm.brake_scaled_entries += 1
        arm.armed_since_ns.clear()
        if arm.spec.mode == "TAKER_TAKER":
            arm.pend_action = True
            exp_a = aa if direction > 0 else ab
            exp_x = xb if direction > 0 else xa
            self.push(t + arm.lat_ns, 2, self.exec_taker_pair, arm, direction, qty, exp_a, exp_x, True)
        else:
            px = ab if direction > 0 else aa
            xyz_ref = xb if direction > 0 else xa
            self._place_passive_arcus(t, arm, purpose="entry", side=direction, px=px, qty=qty, xyz_ref=xyz_ref, ttl_ns=PASSIVE_ENTRY_TTL_NS)

    def on_persist_wakeup(self, t: int, arm: AblationArmState, direction: int, arm_t_ns: int, req_bp: float) -> None:
        if arm.armed_since_ns.get(direction) != arm_t_ns:
            arm.persist_blocked_count += 1
            return
        self._try_trigger_entry(t, arm, direction, req_bp)

    def on_xyz_book(self, t: int, bids: list[tuple[float, float]], asks: list[tuple[float, float]]) -> None:
        self._check_funding_tick(t)
        self.xyz_bids = bids
        self.xyz_asks = asks
        self.xyz_rx_ns = t

        xt = self._xyz_touch()
        at = self._arc_touch(True)
        if xt is None or at is None or (t - self.arc_rx_ns) > MAX_ARC_AGE_NS:
            self._prune_armed_edges()
            return
        self.fresh_xyz_events += 1
        ab, aa = at
        xb, xa = xt
        am = 0.5 * (ab + aa)
        xm = 0.5 * (xb + xa)
        mid_basis_bp = (am / xm - 1.0) * 1e4
        combined_sp_bp = ((aa - ab) / am + (xa - xb) / xm) * 1e4
        ep_t, en_t, ep_m, en_m = self._raw_edges(ab, aa, xb, xa)

        for arm in self.arms:
            arm.signals_seen += 1
            arm.update_dd(t, am, xm)
            if arm.spec.use_kalman_leung_li:
                arm.kalman.update(mid_basis_bp, combined_sp_bp)
            if arm.pend_action or arm.pend_hedges > 0 or (arm.passive is not None and not arm.passive.done):
                continue

            unhedged = arm.inv_arc + arm.inv_xyz
            if abs(unhedged) > 1e-9:
                hedge_side = -1 if unhedged > 0 else 1
                arm.pend_hedges += 1
                self.push(t + arm.lat_ns, 2, self.exec_xyz_hedge, arm, hedge_side, abs(unhedged), xm)
                continue

            did_exit_or_hold = False
            if abs(arm.inv_arc) > 1e-9:
                pos_sign = 1 if arm.inv_arc > 0 else -1
                hold_s = (t - (arm.entry_open_ns or t)) / 1e9
                close_t_edge = en_t if pos_sign > 0 else ep_t
                close_m_edge = en_m if pos_sign > 0 else ep_m
                timed_out = hold_s >= arm.spec.max_hold_s
                if arm.spec.mode == "TAKER_TAKER":
                    if (close_t_edge >= arm.spec.exit_bp and self._dual_refresh_ok(arm)) or timed_out:
                        if timed_out:
                            arm.forced_time_exits += 1
                        arm.pend_action = True
                        self.push(
                            t + arm.lat_ns,
                            2,
                            self.exec_taker_pair,
                            arm,
                            -pos_sign,
                            abs(arm.inv_arc),
                            ab if pos_sign > 0 else aa,
                            xa if pos_sign > 0 else xb,
                            False,
                        )
                        did_exit_or_hold = True
                else:
                    if timed_out:
                        arm.forced_time_exits += 1
                        arm.pend_action = True
                        self.push(
                            t + arm.lat_ns,
                            2,
                            self.exec_taker_pair,
                            arm,
                            -pos_sign,
                            abs(arm.inv_arc),
                            ab if pos_sign > 0 else aa,
                            xa if pos_sign > 0 else xb,
                            False,
                        )
                        did_exit_or_hold = True
                    elif close_m_edge >= arm.spec.exit_bp and self._dual_refresh_ok(arm):
                        self._place_passive_arcus(
                            t,
                            arm,
                            purpose="exit",
                            side=-pos_sign,
                            px=aa if pos_sign > 0 else ab,
                            qty=abs(arm.inv_arc),
                            xyz_ref=xa if pos_sign > 0 else xb,
                            ttl_ns=min(PASSIVE_EXIT_WAIT_NS, max(SEC_NS, int((arm.spec.max_hold_s - hold_s) * 1e9))),
                        )
                        did_exit_or_hold = True
                if did_exit_or_hold or not arm.spec.allow_batch_add:
                    continue

            if arm.halted_58 or not self._dual_refresh_ok(arm):
                continue

            # Candidate entry (either flat or same-direction batch add under ladder)
            allowed_dirs = [1 if arm.inv_arc > 0 else -1] if abs(arm.inv_arc) > 1e-9 else [1, -1]
            best_dir = 0
            best_score = -1e18
            best_req = 0.0
            for direction in allowed_dirs:
                raw_edge = (ep_t if direction > 0 else en_t) if arm.spec.mode == "TAKER_TAKER" else (ep_m if direction > 0 else en_m)
                ok, req_bp, tag = self._effective_entry_requirements(arm, direction, am, xm, raw_edge)
                if not ok:
                    if raw_edge >= arm.spec.entry_bp:
                        if tag == "leung_li":
                            arm.leung_li_capped += 1
                        elif tag == "kalman":
                            arm.kalman_filtered += 1
                    continue
                if raw_edge < req_bp:
                    if raw_edge >= arm.spec.entry_bp and arm.spec.use_adverse_carry_penalty:
                        arm.adverse_carry_filtered += 1
                    continue
                if raw_edge > best_score:
                    best_score = raw_edge
                    best_dir = direction
                    best_req = req_bp

            for d_old in list(arm.armed_since_ns.keys()):
                if d_old != best_dir:
                    arm.armed_since_ns.pop(d_old, None)

            if best_dir == 0:
                arm.armed_since_ns.clear()
                continue

            if arm.spec.persist_ms <= 0:
                self._try_trigger_entry(t, arm, best_dir, best_req)
            else:
                if best_dir not in arm.armed_since_ns:
                    arm.armed_since_ns[best_dir] = t
                    arm.persist_armed_count += 1
                    self.push(t + arm.spec.persist_ms * MS_NS, 2, self.on_persist_wakeup, arm, best_dir, t, best_req)

    def _place_passive_arcus(
        self,
        t: int,
        arm: AblationArmState,
        *,
        purpose: str,
        side: int,
        px: float,
        qty: float,
        xyz_ref: float,
        ttl_ns: int,
    ) -> None:
        if arm.passive is not None and not arm.passive.done:
            return
        if purpose == "entry" and arm.entry_open_ns is None:
            at = self._arc_touch(True)
            xt = self._xyz_touch()
            if at and xt:
                arm._cycle_start_eq = arm.equity_mtm(0.5 * (at[0] + at[1]), 0.5 * (xt[0] + xt[1]))
        po = PassiveOrder(
            purpose=purpose,
            side=side,
            px=px,
            qty=qty,
            rem=qty,
            ahead=0.0,
            last_disp=0.0,
            land_ns=t + ARC_PASSIVE_RTT_NS,
            expire_ns=t + ARC_PASSIVE_RTT_NS + ttl_ns,
            xyz_trigger_mid=xyz_ref,
            t_sig_ns=t,
        )
        arm.passive = po
        self.push(po.land_ns, 2, self.on_passive_land, arm, po)
        self.push(po.expire_ns, 2, self.on_passive_expire, arm, po)

    def on_passive_land(self, t: int, arm: AblationArmState, po: PassiveOrder) -> None:
        if po.done or t >= po.expire_ns:
            return
        at = self._arc_touch(False)
        if at is None or (po.side > 0 and po.px >= at[1]) or (po.side < 0 and po.px <= at[0]):
            po.done = True
            if arm.passive is po:
                arm.passive = None
            return
        lv = dict(self.arc_ex_bids if po.side > 0 else self.arc_ex_asks)
        sz = lv.get(po.px, 0.0)
        po.ahead = sz
        po.last_disp = sz
        if po.purpose == "entry" and arm.entry_open_ns is None:
            arm.entry_open_ns = t
            arm.cycles_opened += 1

    def on_passive_expire(self, t: int, arm: AblationArmState, po: PassiveOrder) -> None:
        if po.done:
            return
        po.done = True
        if arm.passive is po:
            arm.passive = None
            arm.maker_timeouts += 1
            if po.purpose == "entry" and abs(arm.inv_arc) <= 1e-12:
                arm.entry_open_ns = None
                arm.cycles_opened = max(0, arm.cycles_opened - 1)

    def exec_taker_pair(
        self,
        t: int,
        arm: AblationArmState,
        side_arc: int,
        qty: float,
        exp_arc_px: float,
        exp_xyz_px: float,
        is_entry: bool,
    ) -> None:
        arm.pend_action = False
        arc_levels = self.arc_ex_asks if side_arc > 0 else self.arc_ex_bids
        xyz_levels = self.xyz_bids if side_arc > 0 else self.xyz_asks
        px_a = walk_book(arc_levels, qty)
        px_x = walk_book(xyz_levels, qty)
        if px_a is None or px_x is None:
            arm.skipped_thin += 1
            return
        if is_entry and abs(arm.inv_arc) <= 1e-12:
            at = self._arc_touch(False)
            xt = self._xyz_touch()
            if at and xt:
                arm._cycle_start_eq = arm.equity_mtm(0.5 * (at[0] + at[1]), 0.5 * (xt[0] + xt[1]))
            arm.entry_open_ns = t
            arm.cycles_opened += 1

        arm.inv_arc += side_arc * qty
        arm.cash -= side_arc * qty * px_a
        na = qty * px_a
        arm.vol_arc += na
        arm.fees_usd += na * ARC_TAKER_BP / 1e4

        side_xyz = -side_arc
        arm.inv_xyz += side_xyz * qty
        arm.cash -= side_xyz * qty * px_x
        nx = qty * px_x
        arm.vol_xyz += nx
        arm.fees_usd += nx * XYZ_TAKER_BP / 1e4

        arm.last_trade_arc_rx_ns = max(arm.last_trade_arc_rx_ns, self.arc_rx_ns)
        arm.last_trade_xyz_rx_ns = max(arm.last_trade_xyz_rx_ns, self.xyz_rx_ns)
        arm.lat_slip_usd += (side_arc * (px_a - exp_arc_px) + side_xyz * (px_x - exp_xyz_px)) * qty
        self._after_position_update(t, arm)


def _init_stream_stats() -> dict[str, int]:
    return {"events": 0, "first_rx_ns": 0, "last_rx_ns": 0, "max_gap_ns": 0}


def _record_stream_rx(stats: dict[str, int], t_rx: int) -> None:
    if stats["events"] == 0:
        stats["first_rx_ns"] = t_rx
        stats["last_rx_ns"] = t_rx
    else:
        if t_rx > stats["last_rx_ns"]:
            stats["max_gap_ns"] = max(stats["max_gap_ns"], t_rx - stats["last_rx_ns"])
            stats["last_rx_ns"] = t_rx
        elif t_rx < stats["first_rx_ns"]:
            stats["first_rx_ns"] = t_rx
    stats["events"] += 1


def _finalize_stream_stats(stats: dict[str, int]) -> dict[str, Any]:
    return {**stats, "max_gap_ms": round(stats["max_gap_ns"] / MS_NS, 3)}


def run_ablation(
    dirs: list[Path],
    specs: list[AblationSpec],
    *,
    decision_bounds_ns: tuple[int, int] | list[int] | None = None,
) -> dict[str, Any]:
    files: list[Path] = []
    for d in dirs:
        files.extend(sorted(d.glob("tokyo-research-*.seg*.ndjson.gz")))
    sim = AblationSim(specs)
    decision_start_ns = 0
    decision_end_ns = 0
    if decision_bounds_ns is not None:
        if (
            not isinstance(decision_bounds_ns, (tuple, list))
            or len(decision_bounds_ns) != 2
            or not all(isinstance(v, int) and not isinstance(v, bool) for v in decision_bounds_ns)
            or not (0 < decision_bounds_ns[0] < decision_bounds_ns[1])
        ):
            raise ValueError(f"invalid decision_bounds_ns: {decision_bounds_ns!r}")
        decision_start_ns = int(decision_bounds_ns[0])
        decision_end_ns = int(decision_bounds_ns[1])
        sim.next_hour_ns = ((decision_start_ns // HOUR_NS) + 1) * HOUR_NS

    first_rx = 0
    last_rx = 0
    prev_dir = None
    warmup_lines_excluded = 0
    post_window_lines_excluded = 0
    arc_book_stats = _init_stream_stats()
    xyz_book_stats = _init_stream_stats()
    xyz_funding_stats = _init_stream_stats()

    for fpath in files:
        cur_dir = fpath.parent
        with gzip.open(fpath, "rt") as f:
            for line in f:
                sim.lines += 1
                if '"ANT-USD"' not in line and '"xyz:ANT"' not in line:
                    continue
                try:
                    d = json.loads(line)
                except Exception:
                    continue
                if d.get("event") != "message":
                    continue
                t_rx = (d.get("capture_timing") or {}).get("available_wall_ns")
                if not isinstance(t_rx, int) or t_rx <= 0:
                    continue
                if decision_bounds_ns is not None:
                    if t_rx < decision_start_ns:
                        warmup_lines_excluded += 1
                        continue
                    if t_rx >= decision_end_ns:
                        post_window_lines_excluded += 1
                        continue
                if prev_dir is not None and cur_dir != prev_dir:
                    sim.gaps_detected.append({"from_ms": last_rx // MS_NS, "to_ms": t_rx // MS_NS, "gap_s": round((t_rx - last_rx) / 1e9, 2)})
                prev_dir = cur_dir
                first_rx = t_rx if first_rx == 0 else min(first_rx, t_rx)
                last_rx = max(last_rx, t_rx)

                src = d.get("source")
                mkt = d.get("market")
                pl = d.get("payload") or {}
                if src == "arcus" and pl.get("channel") == "l2Orderbook" and pl.get("id") == "ANT-USD":
                    c = pl.get("contents") or {}
                    bids = [(float(x[0]), float(x[1])) for x in c.get("bids") or [] if float(x[1]) > 0]
                    asks = [(float(x[0]), float(x[1])) for x in c.get("asks") or [] if float(x[1]) > 0]
                    if not (bids and asks and bids[0][0] < asks[0][0]):
                        continue
                    _record_stream_rx(arc_book_stats, t_rx)
                    ex_ns = max(int(c["timestamp"]) * 1000 if c.get("timestamp") else t_rx, sim.now)
                    sim.push(ex_ns, 0, sim.on_arc_ex_book, bids, asks)
                    sim.push(t_rx, 1, sim.on_arc_seen_book, bids, asks)
                elif src == "arcus_trades" and mkt == "ANT-USD":
                    for it in pl.get("contents") or []:
                        tid = it.get("tradeId")
                        if tid in sim.seen_tids:
                            continue
                        if tid is not None:
                            sim.seen_tids.add(tid)
                        px, sz = float(it["price"]), float(it["size"])
                        if px <= 0 or sz <= 0:
                            continue
                        sign = 1 if it.get("side") == "BUY" else -1
                        ex_ns = max(int(it["timestamp"]) * 1000 if it.get("timestamp") else t_rx, sim.now)
                        sim.push(ex_ns, 1, sim.on_arc_ex_print, t_rx, sign, px, sz)
                elif src == "hyperliquid" and mkt == "xyz:ANT":
                    ch = pl.get("channel")
                    if ch == "l2Book" and "data" in pl:
                        lv = pl["data"].get("levels") or [[], []]
                        bids = [(float(x["px"]), float(x["sz"])) for x in lv[0] if float(x["sz"]) > 0]
                        asks = [(float(x["px"]), float(x["sz"])) for x in lv[1] if float(x["sz"]) > 0]
                        if bids and asks and bids[0][0] < asks[0][0]:
                            _record_stream_rx(xyz_book_stats, t_rx)
                            sim.push(max(t_rx, sim.now), 1, sim.on_xyz_book, bids, asks)
                    elif ch == "activeAssetCtx" and "data" in pl:
                        ctx = pl["data"].get("ctx") or {}
                        if ctx.get("funding") is not None:
                            try:
                                rate_val = float(ctx["funding"])
                            except (TypeError, ValueError):
                                pass
                            else:
                                _record_stream_rx(xyz_funding_stats, t_rx)
                                sim.push(max(t_rx, sim.now), 0, sim.on_xyz_funding, rate_val, t_rx)
                if last_rx > WATERMARK_NS:
                    sim.drain(last_rx - WATERMARK_NS)

    if decision_bounds_ns is None:
        sim.drain(BIG)
        cutoff_ns = last_rx
    else:
        cutoff_ns = decision_end_ns - 1
        sim.drain(cutoff_ns)
        sim._check_funding_tick(cutoff_ns)

    results: list[dict[str, Any]] = []
    by_family: dict[str, list[dict[str, Any]]] = {}
    for arm in sim.arms:
        drain_arc_qty = abs(arm.inv_arc)
        drain_xyz_qty = abs(arm.inv_xyz)
        pending_inflight = bool(
            arm.pend_action
            or arm.pend_hedges > 0
            or (arm.passive is not None and not arm.passive.done)
        )
        drain_fees = 0.0
        flat_ok = True
        if abs(arm.inv_arc) > 1e-12:
            if cutoff_ns <= 0 or sim.arc_rx_ns <= 0 or (cutoff_ns - sim.arc_rx_ns) > 300 * SEC_NS:
                flat_ok = False
            sd_a = -1 if arm.inv_arc > 0 else 1
            lv_a = sim.arc_ex_asks if sd_a > 0 else sim.arc_ex_bids
            px_a = walk_book(lv_a, abs(arm.inv_arc))
            if px_a is None:
                flat_ok = False
            else:
                q_a = abs(arm.inv_arc)
                arm.cash -= sd_a * q_a * px_a
                arm.vol_arc += q_a * px_a
                f_a = q_a * px_a * ARC_TAKER_BP / 1e4
                arm.fees_usd += f_a
                drain_fees += f_a
                arm.inv_arc = 0.0
        if abs(arm.inv_xyz) > 1e-12:
            if cutoff_ns <= 0 or sim.xyz_rx_ns <= 0 or (cutoff_ns - sim.xyz_rx_ns) > 300 * SEC_NS:
                flat_ok = False
            sd_x = -1 if arm.inv_xyz > 0 else 1
            lv_x = sim.xyz_asks if sd_x > 0 else sim.xyz_bids
            px_x = walk_book(lv_x, abs(arm.inv_xyz))
            if px_x is None:
                flat_ok = False
            else:
                q_x = abs(arm.inv_xyz)
                arm.cash -= sd_x * q_x * px_x
                arm.vol_xyz += q_x * px_x
                f_x = q_x * px_x * XYZ_TAKER_BP / 1e4
                arm.fees_usd += f_x
                drain_fees += f_x
                arm.inv_xyz = 0.0
        at = sim._arc_touch(False) or sim._arc_touch(True)
        xt = sim._xyz_touch()
        am = 0.5 * (at[0] + at[1]) if at else 0.0
        xm = 0.5 * (xt[0] + xt[1]) if xt else 0.0
        eq = arm.equity_mtm(am, xm)
        row = {
            "key": arm.spec.key,
            "arm_family": arm.spec.arm_family,
            "mode": arm.spec.mode,
            "lat_ms": arm.spec.lat_ms,
            "q_usd": arm.spec.q_usd,
            "entry_bp": arm.spec.entry_bp,
            "persist_ms": arm.spec.persist_ms,
            "net_cash_usd": round(eq, 4),
            "terminal_flat": flat_ok,
            "terminal_drain_required": (drain_arc_qty > 1e-12 or drain_xyz_qty > 1e-12),
            "terminal_drain_arc_qty": round(drain_arc_qty, 6),
            "terminal_drain_xyz_qty": round(drain_xyz_qty, 6),
            "terminal_drain_fees_usd": round(drain_fees, 4),
            "pending_inflight_at_cutoff": pending_inflight,
            "gross_price_cash_usd": round(arm.cash, 4),
            "fees_usd": round(arm.fees_usd, 4),
            "funding_usd": round(arm.funding_usd, 4),
            "lat_slip_usd": round(arm.lat_slip_usd, 4),
            "vol_arc_usd": round(arm.vol_arc, 2),
            "vol_xyz_usd": round(arm.vol_xyz, 2),
            "cycles_opened": arm.cycles_opened,
            "cycles_closed": arm.cycles_closed,
            "win_rate": round(arm.wins / arm.cycles_closed, 4) if arm.cycles_closed else None,
            "avg_hold_s": round(sum(arm.hold_times_s) / len(arm.hold_times_s), 1) if arm.hold_times_s else None,
            "max_drawdown_usd": round(arm.max_dd_usd, 4),
            "halted_58pct": arm.halted_58,
            "persist_armed": arm.persist_armed_count,
            "persist_blocked": arm.persist_blocked_count,
            "leung_li_capped": arm.leung_li_capped,
            "kalman_filtered": arm.kalman_filtered,
            "adverse_carry_filtered": arm.adverse_carry_filtered,
            "crossable_capped": arm.crossable_capped,
            "thin_hedge_events": arm.thin_hedge_events,
            "stale_hedge_events": arm.stale_hedge_events,
            "skipped_thin": arm.skipped_thin,
            "maker_fills": arm.maker_fills,
            "maker_timeouts": arm.maker_timeouts,
            "forced_time_exits": arm.forced_time_exits,
            "missing_funding_hours_while_open": arm.missing_funding_hours_while_open,
            "brake_tier_transitions": arm.brake_tier_transitions,
            "brake_scaled_entries": arm.brake_scaled_entries,
        }
        results.append(row)
        by_family.setdefault(arm.spec.arm_family, []).append(row)

    family_summary: dict[str, Any] = {}
    for fam, rows in by_family.items():
        pos = [r for r in rows if r["net_cash_usd"] > 0 and r["terminal_flat"]]
        vals = sorted(r["net_cash_usd"] for r in rows)
        best_r = max(rows, key=lambda r: r["net_cash_usd"])
        worst_r = min(rows, key=lambda r: r["net_cash_usd"])
        family_summary[fam] = {
            "count": len(rows),
            "positive_count": len(pos),
            "median_net_cash_usd": vals[len(vals) // 2],
            "mean_net_cash_usd": round(sum(vals) / len(vals), 4),
            "best": best_r,
            "worst": worst_r,
        }

    return {
        "lines": sim.lines,
        "decision_bounds_ns": [decision_start_ns, decision_end_ns] if decision_bounds_ns is not None else None,
        "first_rx_ns": first_rx,
        "last_rx_ns": last_rx,
        "fresh_xyz_events": sim.fresh_xyz_events,
        "funding_hours_witnessed": sim.funding_hours_witnessed,
        "funding_hours_missing_witness": sim.funding_hours_missing_witness,
        "gaps": sim.gaps_detected,
        "source_continuity": {
            "arcus_book": _finalize_stream_stats(arc_book_stats),
            "xyz_book": _finalize_stream_stats(xyz_book_stats),
            "xyz_funding": _finalize_stream_stats(xyz_funding_stats),
            "warmup_lines_excluded": warmup_lines_excluded,
            "post_window_lines_excluded": post_window_lines_excluded,
        },
        "family_summary": family_summary,
        "variants_count": len(results),
        "variants": results,
    }
