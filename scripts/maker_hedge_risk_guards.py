#!/usr/bin/env python3
"""Reusable offline simulator risk-guardrail and unit-cost auditing primitives.

Closes the W13/W14 maker-hedge simulator audit gaps without mutating frozen
research scripts or exercising any live exchange authority:
1. InFlightQuoteReservation: cancel-before-place lock & pending-cancel exposure.
2. UnhedgedExposureMonitor: continuous above-trigger duration & recovery halts.
3. GapAwareBasisTracker: inter-update gap / stream-reset EWMA re-warmup & lag skew.
4. UnitCostAuditor: exact Decimal dual-denominator costs, breakeven, flatness.
"""

from __future__ import annotations

from collections import Counter, defaultdict
from dataclasses import dataclass
from decimal import Decimal
from hashlib import sha256
import json
import math
from pathlib import Path
import sys
from typing import Any, Mapping, Optional, Sequence

D = Decimal
ZERO = D(0)
ONE = D(1)
SECOND_NS = 1_000_000_000
DEFAULT_RECOVERY_INTERVAL_NS = 5 * SECOND_NS
DEFAULT_FEE_GRID = (D(0), D(1), D("2.5"), D("3.5"))
EXPECTED_RWA_MARKETS = frozenset(f"{s}-USD" for s in ("SPY", "QQQ", "NVDA", "AMZN", "AAPL", "TSLA"))


def number(value: Any) -> Decimal:
    """Parse an exact numeric input into a finite Decimal (rejects float/bool)."""
    if isinstance(value, (bool, float)) or value is None:
        raise ValueError(f"exact Decimal/int/str required, got {type(value).__name__}")
    result = D(value)
    if not result.is_finite():
        raise ValueError("finite Decimal required")
    return result


def _positive_int(value: Any, name: str, *, allow_zero: bool = False) -> int:
    if isinstance(value, bool) or not isinstance(value, int):
        raise ValueError(f"{name} must be an integer")
    if value < 0 or (not allow_zero and value == 0):
        op = ">=" if allow_zero else ">"
        raise ValueError(f"{name} must be {op} 0, got {value}")
    return value


def _require_ns(value: Any, name: str = "now_ns") -> int:
    return _positive_int(value, name, allow_zero=True)


def _finite_float(value: Any, name: str, *, positive: bool = False) -> float:
    if isinstance(value, bool):
        raise ValueError(f"{name} must be numeric, not bool")
    out = float(value)
    if not math.isfinite(out) or (positive and out <= 0.0):
        raise ValueError(f"{name} must be finite{' and > 0' if positive else ''}, got {value}")
    return out


def _percentile(sorted_vals: Sequence[float], q: float) -> Optional[float]:
    if not sorted_vals:
        return None
    if len(sorted_vals) == 1:
        return sorted_vals[0]
    pos = q * (len(sorted_vals) - 1)
    lo = math.floor(pos)
    hi = math.ceil(pos)
    if lo == hi:
        return sorted_vals[lo]
    frac = pos - lo
    return sorted_vals[lo] * (1.0 - frac) + sorted_vals[hi] * frac


@dataclass
class TrackedOrder:
    order_id: str
    side: int
    quantity_base: Decimal
    remaining_base: Decimal
    limit_price: Decimal
    state: str
    created_at_ns: int
    cancel_requested_at_ns: Optional[int] = None
    cancel_eff_ns: Optional[int] = None
    unknown_reason: Optional[str] = None


class InFlightQuoteReservation:
    """Tracks open, pending-cancel, and unknown quotes against worst-case caps."""

    def __init__(
        self,
        maker_cap_usd: Any,
        net_cap_usd: Any,
        *,
        allow_replace_overlap: bool = False,
        initial_maker_pos_base: Any = ZERO,
        initial_hedge_pos_base: Any = ZERO,
    ) -> None:
        self.maker_cap_usd = number(maker_cap_usd)
        self.net_cap_usd = number(net_cap_usd)
        if self.maker_cap_usd <= ZERO or self.net_cap_usd <= ZERO:
            raise ValueError("maker_cap_usd and net_cap_usd must be positive")
        self.allow_replace_overlap = bool(allow_replace_overlap)
        self.maker_pos_base = number(initial_maker_pos_base)
        self.hedge_pos_base = number(initial_hedge_pos_base)

        self.orders: dict[str, TrackedOrder] = {}
        self.used_ids: set[str] = set()
        self.closed_ids: set[str] = set()
        self.unknown_reasons: dict[str, str] = {}
        self.clock_ns: Optional[int] = None
        self.last_reject_reason: Optional[str] = None
        self.cancel_in_flight_fills: int = 0
        self.counts: Counter[str] = Counter()

    def _advance_clock(self, now_ns: int) -> int:
        ts = _require_ns(now_ns)
        if self.clock_ns is not None and ts < self.clock_ns:
            raise ValueError(f"clock regression: {ts} < {self.clock_ns}")
        self.clock_ns = ts
        return ts

    def has_unknown_orders(self) -> bool:
        return bool(self.unknown_reasons) or any(o.state == "unknown" for o in self.orders.values())

    def pending_bounds_base(self) -> tuple[Decimal, Decimal]:
        """Return (pending_buy_base, pending_sell_base) across ALL non-terminal orders."""
        pending_buy = ZERO
        pending_sell = ZERO
        for order in self.orders.values():
            if order.remaining_base <= ZERO:
                continue
            if order.side > 0:
                pending_buy += order.remaining_base
            else:
                pending_sell += order.remaining_base
        return pending_buy, pending_sell

    def pending_notional_bounds_usd(self, ref_mid: Any) -> dict[str, Decimal]:
        """Compute exact per-order worst-case buy/sell notional using outer limit price."""
        ref = number(ref_mid)
        if ref <= ZERO:
            raise ValueError("ref_mid must be positive")
        buy_base = ZERO
        sell_base = ZERO
        buy_notional_usd = ZERO
        sell_notional_usd = ZERO
        for order in self.orders.values():
            if order.remaining_base <= ZERO:
                continue
            outer_price = max(ref, order.limit_price)
            order_notional = order.remaining_base * outer_price
            if order.side > 0:
                buy_base += order.remaining_base
                buy_notional_usd += order_notional
            else:
                sell_base += order.remaining_base
                sell_notional_usd += order_notional
        return {
            "buy_base": buy_base,
            "sell_base": sell_base,
            "buy_notional_usd": buy_notional_usd,
            "sell_notional_usd": sell_notional_usd,
        }

    def exposure_bounds_base(self) -> dict[str, tuple[Decimal, Decimal]]:
        pending_buy, pending_sell = self.pending_bounds_base()
        net_pos = self.maker_pos_base + self.hedge_pos_base
        return {
            "maker": (self.maker_pos_base - pending_sell, self.maker_pos_base + pending_buy),
            "net": (net_pos - pending_sell, net_pos + pending_buy),
        }

    def can_place_quote(
        self,
        order_id: str,
        side: int,
        qty_base: Any,
        limit_price: Any,
        ref_mid: Any,
    ) -> tuple[bool, Optional[str]]:
        if not isinstance(order_id, str) or not order_id:
            raise ValueError("order_id must be a non-empty string")
        if order_id in self.used_ids:
            raise ValueError(f"order_id already used: {order_id}")
        if side not in (1, -1):
            raise ValueError(f"side must be +1 or -1, got {side}")

        qty = number(qty_base)
        limit = number(limit_price)
        ref = number(ref_mid)
        if qty <= ZERO or limit <= ZERO or ref <= ZERO:
            return False, "non_positive_order_parameter"
        if self.has_unknown_orders():
            return False, "unknown_order_halt"
        if not self.allow_replace_overlap:
            for order in self.orders.values():
                if order.side == side and order.state == "pending_cancel" and order.remaining_base > ZERO:
                    return False, "pending_cancel_on_side"

        pending_usd = self.pending_notional_bounds_usd(ref)
        cand_outer_notional = qty * max(ref, limit)
        total_buy_base = pending_usd["buy_base"] + (qty if side > 0 else ZERO)
        total_sell_base = pending_usd["sell_base"] + (qty if side < 0 else ZERO)
        total_buy_notional = pending_usd["buy_notional_usd"] + (cand_outer_notional if side > 0 else ZERO)
        total_sell_notional = pending_usd["sell_notional_usd"] + (cand_outer_notional if side < 0 else ZERO)

        def _worst_side_usd(pos_base: Decimal) -> Decimal:
            # Distinguish current fair-value exposure (pos * ref) from worst-case working
            # order notional summed per order at max(limit_price, ref).
            pos_usd = pos_base * ref
            worst_buy = max(abs(pos_base + total_buy_base) * ref, abs(pos_usd + total_buy_notional))
            worst_sell = max(abs(pos_base - total_sell_base) * ref, abs(pos_usd - total_sell_notional))
            return max(abs(pos_usd), worst_buy, worst_sell)

        if _worst_side_usd(self.maker_pos_base) > self.maker_cap_usd:
            return False, "maker_cap_exceeded"

        net_pos = self.maker_pos_base + self.hedge_pos_base
        if _worst_side_usd(net_pos) > self.net_cap_usd:
            return False, "net_cap_exceeded"

        return True, None

    def place_quote(
        self,
        order_id: str,
        side: int,
        qty_base: Any,
        limit_price: Any,
        ref_mid: Any,
        now_ns: int,
    ) -> bool:
        ts = self._advance_clock(now_ns)
        ok, reason = self.can_place_quote(order_id, side, qty_base, limit_price, ref_mid)
        if not ok:
            self.last_reject_reason = reason
            self.counts[f"reject_{reason}"] += 1
            return False
        qty = number(qty_base)
        limit = number(limit_price)
        self.used_ids.add(order_id)
        self.orders[order_id] = TrackedOrder(
            order_id=order_id,
            side=side,
            quantity_base=qty,
            remaining_base=qty,
            limit_price=limit,
            state="resting",
            created_at_ns=ts,
        )
        self.last_reject_reason = None
        self.counts["placed"] += 1
        return True

    def request_cancel(self, order_id: str, now_ns: int, cancel_eff_ns: int) -> bool:
        ts = self._advance_clock(now_ns)
        eff = _require_ns(cancel_eff_ns, "cancel_eff_ns")
        if eff < ts:
            raise ValueError(f"cancel_eff_ns ({eff}) cannot precede now_ns ({ts})")
        if order_id in self.closed_ids:
            return False
        order = self.orders.get(order_id)
        if order is None:
            raise ValueError(f"unknown order_id: {order_id}")
        if order.state == "unknown":
            raise ValueError(f"cannot request_cancel on unknown order {order_id}")
        if order.state == "pending_cancel":
            return False
        order.state = "pending_cancel"
        order.cancel_requested_at_ns = ts
        order.cancel_eff_ns = eff
        self.counts["cancel_requested"] += 1
        return True

    def record_fill(self, order_id: str, qty_base: Any, exec_price: Any, now_ns: int) -> bool:
        self._advance_clock(now_ns)
        order = self.orders.get(order_id)
        if order is None:
            raise ValueError(f"order_id not open: {order_id}")
        if order.state == "unknown":
            raise ValueError(f"cannot apply fill to unknown order {order_id}")
        qty = number(qty_base)
        px = number(exec_price)
        if qty <= ZERO or qty > order.remaining_base:
            raise ValueError(f"invalid fill qty {qty} for remaining {order.remaining_base}")
        if px <= ZERO or (px > order.limit_price if order.side > 0 else px < order.limit_price):
            raise ValueError(f"fill price {px} violates limit {order.limit_price} on side {order.side}")

        if order.state == "pending_cancel":
            self.cancel_in_flight_fills += 1
            self.counts["cancel_in_flight_fills"] += 1
        order.remaining_base -= qty
        self.maker_pos_base += qty if order.side > 0 else -qty
        self.counts["maker_fills"] += 1
        if order.remaining_base == ZERO:
            del self.orders[order_id]
            self.closed_ids.add(order_id)
            self.counts["filled_terminal"] += 1
        return True

    def confirm_terminal(self, order_id: str, now_ns: int) -> bool:
        ts = self._advance_clock(now_ns)
        if order_id in self.closed_ids:
            return False
        order = self.orders.get(order_id)
        if order is None:
            raise ValueError(f"unknown order_id: {order_id}")
        if order.state == "unknown":
            raise ValueError(f"cannot confirm terminal for unknown order {order_id}")
        if order.state == "pending_cancel":
            assert order.cancel_eff_ns is not None
            if ts < order.cancel_eff_ns:
                raise ValueError(
                    f"cannot confirm cancel for {order_id} at {ts} before cancel_eff_ns {order.cancel_eff_ns}"
                )
        del self.orders[order_id]
        self.closed_ids.add(order_id)
        self.counts["terminal_confirmed"] += 1
        return True

    def settle_modeled_cancels(self, now_ns: int) -> list[str]:
        """Replay helper: confirm all pending_cancel orders whose cancel_eff_ns <= now_ns.

        Never clears orders in state 'unknown'.
        """
        ts = self._advance_clock(now_ns)
        ready = [
            oid
            for oid, order in self.orders.items()
            if order.state == "pending_cancel" and order.cancel_eff_ns is not None and order.cancel_eff_ns <= ts
        ]
        for oid in ready:
            self.confirm_terminal(oid, ts)
        return sorted(ready)

    def mark_unknown(self, order_id: str, reason: str, now_ns: Optional[int] = None) -> None:
        if now_ns is not None:
            self._advance_clock(now_ns)
        if not reason or not isinstance(reason, str):
            raise ValueError("non-empty reason required")
        order = self.orders.get(order_id)
        if order is None:
            raise ValueError(f"cannot mark non-open order unknown: {order_id}")
        order.state = "unknown"
        order.unknown_reason = reason
        self.unknown_reasons[order_id] = reason
        self.counts["marked_unknown"] += 1

    def record_hedge_fill(self, signed_qty_base: Any, now_ns: int) -> None:
        self._advance_clock(now_ns)
        qty = number(signed_qty_base)
        if qty == ZERO:
            raise ValueError("hedge fill quantity must be non-zero")
        self.hedge_pos_base += qty
        self.counts["hedge_fills"] += 1

    def summary(self) -> dict[str, Any]:
        bounds = self.exposure_bounds_base()
        state_counts = Counter(o.state for o in self.orders.values())
        return {
            "maker_pos_base": self.maker_pos_base,
            "hedge_pos_base": self.hedge_pos_base,
            "net_pos_base": self.maker_pos_base + self.hedge_pos_base,
            "maker_bounds_base": bounds["maker"],
            "net_bounds_base": bounds["net"],
            "open_orders": len(self.orders),
            "resting_orders": state_counts["resting"],
            "pending_cancel_orders": state_counts["pending_cancel"],
            "unknown_orders": state_counts["unknown"],
            "unknown_reasons": dict(self.unknown_reasons),
            "cancel_in_flight_fills": self.cancel_in_flight_fills,
            "last_reject_reason": self.last_reject_reason,
            "counts": dict(self.counts),
        }


class UnhedgedExposureMonitor:
    """Event-driven continuous exposure integration and hedge-failure recovery guard."""

    def __init__(
        self,
        hedge_trigger_usd: Any,
        *,
        trigger_tolerance_usd: Any = ZERO,
        max_unhedged_duration_ns: Optional[int] = None,
        default_recovery_interval_ns: int = DEFAULT_RECOVERY_INTERVAL_NS,
    ) -> None:
        self.hedge_trigger_usd = number(hedge_trigger_usd)
        self.trigger_tolerance_usd = number(trigger_tolerance_usd)
        if self.hedge_trigger_usd <= ZERO or self.trigger_tolerance_usd < ZERO:
            raise ValueError("hedge_trigger_usd > 0 and trigger_tolerance_usd >= 0 required")
        self.max_unhedged_duration_ns = (
            None if max_unhedged_duration_ns is None else _positive_int(max_unhedged_duration_ns, "max_unhedged_duration_ns")
        )
        self.default_recovery_interval_ns = _positive_int(
            default_recovery_interval_ns, "default_recovery_interval_ns"
        )

        self.clock_ns: Optional[int] = None
        self.last_obs_ns: Optional[int] = None
        self.last_above: bool = False
        self.last_abs_net_usd: Decimal = ZERO
        self.last_excess_usd: Decimal = ZERO
        self.episode_start_ns: Optional[int] = None
        self.episode_peak_usd: Decimal = ZERO
        self.current_continuous_ns: int = 0
        self.max_continuous_above_trigger_ns: int = 0
        self.total_above_trigger_ns: int = 0
        self.time_weighted_excess_usd_sec: Decimal = ZERO
        self.max_abs_net_usd: Decimal = ZERO
        self.max_abs_net_including_pending_usd: Decimal = ZERO
        self.episodes: list[dict[str, Any]] = []

        self.observation_count: int = 0
        self.retry_events: int = 0
        self.exhausted_events: int = 0
        self.hedge_success_events: int = 0
        self.hedge_unrecovered: bool = False
        self.unrecovered_reason: Optional[str] = None
        self.next_recovery_check_ns: Optional[int] = None
        self.exhausted_at_ns: Optional[int] = None
        self.recovery_generation: int = 0

    @property
    def threshold_usd(self) -> Decimal:
        return self.hedge_trigger_usd + self.trigger_tolerance_usd

    def _advance_clock(self, now_ns: int) -> int:
        ts = _require_ns(now_ns)
        if self.clock_ns is not None and ts < self.clock_ns:
            raise ValueError(f"clock regression: {ts} < {self.clock_ns}")
        self.clock_ns = ts
        return ts

    def _integrate_until(self, now_ns: int) -> None:
        if self.last_obs_ns is None or not self.last_above:
            return
        dt_ns = now_ns - self.last_obs_ns
        if dt_ns > 0:
            self.total_above_trigger_ns += dt_ns
            self.time_weighted_excess_usd_sec += self.last_excess_usd * Decimal(dt_ns) / Decimal(SECOND_NS)
            assert self.episode_start_ns is not None
            self.current_continuous_ns = now_ns - self.episode_start_ns
            self.max_continuous_above_trigger_ns = max(
                self.max_continuous_above_trigger_ns, self.current_continuous_ns
            )

    def observe(
        self,
        now_ns: int,
        maker_pos_base: Any,
        hedge_pos_base: Any,
        ref_mid: Any,
        pending_hedge_base: Any = ZERO,
        *,
        recovery_generation: Optional[int] = None,
    ) -> dict[str, Any]:
        ts = self._advance_clock(now_ns)
        if recovery_generation is not None:
            _positive_int(recovery_generation, "recovery_generation", allow_zero=True)
        maker = number(maker_pos_base)
        hedge = number(hedge_pos_base)
        mid = number(ref_mid)
        pending = number(pending_hedge_base)
        if mid <= ZERO:
            raise ValueError("ref_mid must be positive")

        self._integrate_until(ts)
        net_base = maker + hedge
        abs_net_usd = abs(net_base) * mid
        abs_with_pending = abs(net_base + pending) * mid
        self.max_abs_net_usd = max(self.max_abs_net_usd, abs_net_usd)
        self.max_abs_net_including_pending_usd = max(self.max_abs_net_including_pending_usd, abs_with_pending)

        is_above = abs_net_usd > self.threshold_usd
        excess_usd = max(ZERO, abs_net_usd - self.hedge_trigger_usd) if is_above else ZERO

        if is_above:
            if not self.last_above:
                self.episode_start_ns = ts
                self.episode_peak_usd = abs_net_usd
                self.current_continuous_ns = 0
            else:
                self.episode_peak_usd = max(self.episode_peak_usd, abs_net_usd)
                assert self.episode_start_ns is not None
                self.current_continuous_ns = ts - self.episode_start_ns
                self.max_continuous_above_trigger_ns = max(
                    self.max_continuous_above_trigger_ns, self.current_continuous_ns
                )
        else:
            if self.last_above and self.episode_start_ns is not None:
                self.episodes.append(
                    {
                        "start_ns": self.episode_start_ns,
                        "end_ns": ts,
                        "duration_ns": ts - self.episode_start_ns,
                        "peak_abs_net_usd": self.episode_peak_usd,
                        "closed": True,
                    }
                )
                self.episode_start_ns = None
                self.episode_peak_usd = ZERO
                self.current_continuous_ns = 0
            gen_matches = recovery_generation is None or recovery_generation == self.recovery_generation
            after_exhaust = (
                self.exhausted_at_ns is None
                or ts > self.exhausted_at_ns
                or (recovery_generation == self.recovery_generation and ts >= self.exhausted_at_ns)
            )
            if self.hedge_unrecovered and gen_matches and after_exhaust:
                self.hedge_unrecovered = False
                self.unrecovered_reason = None
                self.next_recovery_check_ns = None

        self.last_obs_ns = ts
        self.last_above = is_above
        self.last_abs_net_usd = abs_net_usd
        self.last_excess_usd = excess_usd
        self.observation_count += 1
        return {
            "abs_net_usd": abs_net_usd,
            "above_trigger": is_above,
            "excess_usd": excess_usd,
            "current_continuous_ns": self.current_continuous_ns,
        }

    def record_hedge_failure(
        self,
        now_ns: int,
        qty_base: Any,
        reason: str,
        *,
        retries_exhausted: bool = False,
        recovery_interval_ns: Optional[int] = None,
    ) -> None:
        ts = self._advance_clock(now_ns)
        qty = number(qty_base)
        if qty == ZERO:
            raise ValueError("qty_base must be non-zero")
        if not reason or not isinstance(reason, str):
            raise ValueError("non-empty reason required")
        if not retries_exhausted:
            self.retry_events += 1
            return
        interval = (
            self.default_recovery_interval_ns
            if recovery_interval_ns is None
            else _positive_int(recovery_interval_ns, "recovery_interval_ns")
        )
        self.exhausted_events += 1
        self.recovery_generation += 1
        self.exhausted_at_ns = ts
        self.hedge_unrecovered = True
        self.unrecovered_reason = reason
        self.next_recovery_check_ns = ts + interval

    def record_hedge_success(
        self,
        now_ns: int,
        maker_pos_base: Any = None,
        hedge_pos_base: Any = None,
        ref_mid: Any = None,
        pending_hedge_base: Any = ZERO,
        *,
        recovery_generation: Optional[int] = None,
    ) -> None:
        ts = self._advance_clock(now_ns)
        self.hedge_success_events += 1
        pos_args = (maker_pos_base, hedge_pos_base, ref_mid)
        if all(arg is not None for arg in pos_args):
            self.observe(
                ts,
                maker_pos_base,
                hedge_pos_base,
                ref_mid,
                pending_hedge_base,
                recovery_generation=recovery_generation,
            )
        elif any(arg is not None for arg in pos_args):
            raise ValueError("maker_pos_base, hedge_pos_base, and ref_mid must all be provided together")
        # Intentionally never clear `hedge_unrecovered` without fresh authoritative positions.

    def recovery_due(self, now_ns: int) -> bool:
        ts = _require_ns(now_ns)
        if self.clock_ns is not None and ts < self.clock_ns:
            raise ValueError(f"clock regression: {ts} < {self.clock_ns}")
        return (
            self.hedge_unrecovered
            and self.next_recovery_check_ns is not None
            and ts >= self.next_recovery_check_ns
        )

    def should_halt_new_risk(self, now_ns: int) -> tuple[bool, Optional[str]]:
        ts = _require_ns(now_ns)
        if self.clock_ns is not None and ts < self.clock_ns:
            raise ValueError(f"clock regression: {ts} < {self.clock_ns}")
        if self.hedge_unrecovered:
            return True, f"hedge_retries_exhausted:{self.unrecovered_reason}"
        if (
            self.max_unhedged_duration_ns is not None
            and self.last_above
            and self.episode_start_ns is not None
            and (ts - self.episode_start_ns) > self.max_unhedged_duration_ns
        ):
            return True, "unhedged_duration_exceeded"
        return False, None

    def finalize(self, end_ns: int) -> dict[str, Any]:
        ts = self._advance_clock(end_ns)
        self._integrate_until(ts)
        if self.last_above and self.episode_start_ns is not None:
            self.episodes.append(
                {
                    "start_ns": self.episode_start_ns,
                    "end_ns": ts,
                    "duration_ns": ts - self.episode_start_ns,
                    "peak_abs_net_usd": self.episode_peak_usd,
                    "closed": False,
                    "open_at_end": True,
                }
            )
            self.episode_start_ns = None
            self.last_above = False
        self.last_obs_ns = ts
        return self.summary()

    def summary(self) -> dict[str, Any]:
        return {
            "observation_count": self.observation_count,
            "max_abs_net_usd": self.max_abs_net_usd,
            "max_abs_net_including_pending_usd": self.max_abs_net_including_pending_usd,
            "max_continuous_above_trigger_ns": self.max_continuous_above_trigger_ns,
            "total_above_trigger_ns": self.total_above_trigger_ns,
            "time_weighted_excess_usd_sec": self.time_weighted_excess_usd_sec,
            "episodes": list(self.episodes),
            "retry_events": self.retry_events,
            "exhausted_events": self.exhausted_events,
            "hedge_success_events": self.hedge_success_events,
            "hedge_unrecovered": self.hedge_unrecovered,
            "unrecovered_reason": self.unrecovered_reason,
            "next_recovery_check_ns": self.next_recovery_check_ns,
            "exhausted_at_ns": self.exhausted_at_ns,
            "recovery_generation": self.recovery_generation,
        }


class GapAwareBasisTracker:
    """Staleness-, anomaly-, and inter-update-gap-safe EWMA basis tracker."""

    def __init__(
        self,
        *,
        tau_s: float = 120.0,
        max_stale_ns: int = 2 * SECOND_NS,
        max_gap_reset_ns: int = 600 * SECOND_NS,
        warmup_samples: int = 2,
        warmup_span_ns: int = 0,
        channel_skew_alert_ms: float = 50.0,
    ) -> None:
        self.tau_s = _finite_float(tau_s, "tau_s", positive=True)
        self.max_stale_ns = _positive_int(max_stale_ns, "max_stale_ns")
        self.max_gap_reset_ns = _positive_int(max_gap_reset_ns, "max_gap_reset_ns")
        self.warmup_samples = _positive_int(warmup_samples, "warmup_samples")
        self.warmup_span_ns = _positive_int(warmup_span_ns, "warmup_span_ns", allow_zero=True)
        self.channel_skew_alert_ms = _finite_float(channel_skew_alert_ms, "channel_skew_alert_ms")
        if self.channel_skew_alert_ms < 0.0:
            raise ValueError("channel_skew_alert_ms must be >= 0")

        self.clock_ns: Optional[int] = None
        self.basis: Optional[float] = None
        self.basis_t_ns: Optional[int] = None
        self.last_valid_ns: Optional[int] = None
        self.warmup_start_ns: Optional[int] = None
        self.warmup_count: int = 0
        self.maker_min_rx_ns: Optional[int] = None
        self.hedge_min_rx_ns: Optional[int] = None
        self.maker_reset_pending: bool = False
        self.hedge_reset_pending: bool = False
        self.last_maker_rx_ns: Optional[int] = None
        self.last_hedge_rx_ns: Optional[int] = None
        self.pre_reset_maker_rx_ns: Optional[int] = None
        self.pre_reset_hedge_rx_ns: Optional[int] = None
        self.last_valid_rx_pair: Optional[tuple[int, int]] = None

        self.gap_resets: int = 0
        self.stream_resets: Counter[str] = Counter()
        self.stale_skips: int = 0
        self.clock_anomalies: int = 0
        self.reset_waiting_skips: int = 0
        self.out_of_order_rx_skips: int = 0
        self.duplicate_rx_skips: int = 0
        self.warmup_skips: int = 0
        self.emitted_count: int = 0
        self.channel_lags_ms: dict[str, list[float]] = defaultdict(list)
        self.negative_channel_lags: Counter[str] = Counter()

    def _advance_clock(self, now_ns: int) -> int:
        ts = _require_ns(now_ns)
        if self.clock_ns is not None and ts < self.clock_ns:
            raise ValueError(f"clock regression: {ts} < {self.clock_ns}")
        self.clock_ns = ts
        return ts

    def _clear_basis(self) -> None:
        self.basis = None
        self.basis_t_ns = None
        self.warmup_start_ns = None
        self.warmup_count = 0
        self.last_valid_rx_pair = None

    @staticmethod
    def _resolve_reset_leg(venue: str, leg: Optional[str]) -> str:
        if leg is not None:
            if leg not in ("maker", "hedge", "both"):
                raise ValueError(f"unsupported leg override: {leg}")
            return leg
        v = venue.strip().lower()
        if v in ("maker", "arcus"):
            return "maker"
        if v in ("hedge", "rh", "lighter_rh", "lighter_robinhood", "lighter", "hl", "hyperliquid"):
            return "hedge"
        return "both"

    def on_stream_reset(self, venue: str, now_ns: int, *, leg: Optional[str] = None) -> None:
        ts = self._advance_clock(now_ns)
        if not venue or not isinstance(venue, str):
            raise ValueError("non-empty venue name required")
        target = self._resolve_reset_leg(venue, leg)
        if target in ("maker", "both"):
            self.maker_min_rx_ns = ts if self.maker_min_rx_ns is None else max(self.maker_min_rx_ns, ts)
            self.maker_reset_pending = True
            self.pre_reset_maker_rx_ns = self.last_maker_rx_ns
        if target in ("hedge", "both"):
            self.hedge_min_rx_ns = ts if self.hedge_min_rx_ns is None else max(self.hedge_min_rx_ns, ts)
            self.hedge_reset_pending = True
            self.pre_reset_hedge_rx_ns = self.last_hedge_rx_ns
        self.stream_resets[venue] += 1
        self._clear_basis()

    def update(
        self,
        now_ns: int,
        maker_mid: Any,
        hedge_mid: Any,
        maker_rx_ns: int,
        hedge_rx_ns: int,
    ) -> Optional[float]:
        ts = self._advance_clock(now_ns)
        m_rx = _require_ns(maker_rx_ns, "maker_rx_ns")
        h_rx = _require_ns(hedge_rx_ns, "hedge_rx_ns")

        maker_age = ts - m_rx
        hedge_age = ts - h_rx
        if maker_age < 0 or hedge_age < 0:
            self.clock_anomalies += 1
            return None
        if maker_age > self.max_stale_ns or hedge_age > self.max_stale_ns:
            self.stale_skips += 1
            return None

        if (self.maker_min_rx_ns is not None and m_rx < self.maker_min_rx_ns) or (
            self.hedge_min_rx_ns is not None and h_rx < self.hedge_min_rx_ns
        ):
            self.reset_waiting_skips += 1
            return None
        if (
            self.maker_reset_pending
            and self.pre_reset_maker_rx_ns is not None
            and m_rx <= self.pre_reset_maker_rx_ns
        ) or (
            self.hedge_reset_pending
            and self.pre_reset_hedge_rx_ns is not None
            and h_rx <= self.pre_reset_hedge_rx_ns
        ):
            self.reset_waiting_skips += 1
            return None

        if self.last_valid_rx_pair == (m_rx, h_rx):
            self.duplicate_rx_skips += 1
            return None
        if (self.last_maker_rx_ns is not None and m_rx < self.last_maker_rx_ns) or (
            self.last_hedge_rx_ns is not None and h_rx < self.last_hedge_rx_ns
        ):
            self.out_of_order_rx_skips += 1
            return None

        m_mid = _finite_float(maker_mid, "maker_mid", positive=True)
        h_mid = _finite_float(hedge_mid, "hedge_mid", positive=True)

        if self.last_valid_ns is not None and (ts - self.last_valid_ns) > self.max_gap_reset_ns:
            if self.basis is not None:
                self.gap_resets += 1
                self._clear_basis()

        self.maker_reset_pending = False
        self.hedge_reset_pending = False
        self.last_maker_rx_ns = m_rx
        self.last_hedge_rx_ns = h_rx
        self.last_valid_rx_pair = (m_rx, h_rx)

        x = math.log(h_mid) - math.log(m_mid)
        if self.basis is None:
            self.basis = x
            self.basis_t_ns = ts
            self.warmup_start_ns = ts
            self.warmup_count = 1
            self.last_valid_ns = ts
            self.warmup_skips += 1
            return None

        assert self.basis_t_ns is not None and self.warmup_start_ns is not None
        dt_s = max(0, ts - self.basis_t_ns) / SECOND_NS
        alpha = 1.0 - math.exp(-dt_s / self.tau_s)

        if self.warmup_count < self.warmup_samples or (ts - self.warmup_start_ns) < self.warmup_span_ns:
            self.basis += alpha * (x - self.basis)
            self.basis_t_ns = ts
            self.last_valid_ns = ts
            self.warmup_count += 1
            self.warmup_skips += 1
            return None

        lead_gap_bps = (x - self.basis) * 1e4
        self.basis += alpha * (x - self.basis)
        self.basis_t_ns = ts
        self.last_valid_ns = ts
        self.emitted_count += 1
        return lead_gap_bps

    def record_channel_lag(self, channel: str, rx_ns: int, ex_ns: int) -> float:
        if not channel or not isinstance(channel, str):
            raise ValueError("non-empty channel name required")
        rx = _require_ns(rx_ns, "rx_ns")
        ex = _require_ns(ex_ns, "ex_ns")
        lag_ms = (rx - ex) / 1_000_000.0
        if rx < ex:
            self.negative_channel_lags[channel] += 1
        self.channel_lags_ms[channel].append(lag_ms)
        return lag_ms

    def channel_lag_summary(self) -> dict[str, Any]:
        per_channel: dict[str, dict[str, Any]] = {}
        p50_values: list[float] = []
        for channel, vals in sorted(self.channel_lags_ms.items()):
            ordered = sorted(vals)
            p50 = _percentile(ordered, 0.50)
            p99 = _percentile(ordered, 0.99)
            if p50 is not None:
                p50_values.append(p50)
            per_channel[channel] = {
                "count": len(ordered),
                "p50_ms": p50,
                "p99_ms": p99,
                "negative_count": self.negative_channel_lags[channel],
            }
        max_p50_diff_ms = (max(p50_values) - min(p50_values)) if len(p50_values) >= 2 else 0.0
        return {
            "channels": per_channel,
            "max_p50_diff_ms": max_p50_diff_ms,
            "skew_warning": len(p50_values) >= 2 and max_p50_diff_ms >= self.channel_skew_alert_ms,
        }


def unit_cost_per_m(cost_usd: Any, volume_usd: Any) -> Optional[Decimal]:
    """Compute exact USD cost per $1M volume, or None when volume <= 0."""
    cost = number(cost_usd)
    vol = number(volume_usd)
    if vol <= ZERO:
        return None
    return cost / vol * D(1_000_000)


def evaluate_arm_costs(
    marked_equity_usd: Any,
    maker_volume_usd: Any,
    hedge_volume_usd: Any,
    *,
    terminal_maker_pos_base: Any = None,
    terminal_hedge_pos_base: Any = None,
    terminal_flat_verified: bool = False,
    hedge_fee_bps_grid: Sequence[Any] = DEFAULT_FEE_GRID,
    extra_metrics: Optional[Mapping[str, Any]] = None,
) -> dict[str, Any]:
    """Compute dual-denominator unit costs and explicit terminal-flatness disclosure."""
    equity = number(marked_equity_usd)
    maker_vol = number(maker_volume_usd)
    hedge_vol = number(hedge_volume_usd)
    dual_vol = maker_vol + hedge_vol
    fee_per_bp_usd = hedge_vol / D(10_000)

    maker_intercept = unit_cost_per_m(-equity, maker_vol)
    maker_slope = unit_cost_per_m(fee_per_bp_usd, maker_vol)
    dual_intercept = unit_cost_per_m(-equity, dual_vol)
    dual_slope = unit_cost_per_m(fee_per_bp_usd, dual_vol)

    maker_by_fee: dict[str, Optional[Decimal]] = {}
    dual_by_fee: dict[str, Optional[Decimal]] = {}
    for raw_fee in hedge_fee_bps_grid:
        fee_bp = number(raw_fee)
        key = str(fee_bp)
        total_cost = -equity + fee_per_bp_usd * fee_bp
        maker_by_fee[key] = unit_cost_per_m(total_cost, maker_vol)
        dual_by_fee[key] = unit_cost_per_m(total_cost, dual_vol)

    m_pos = None if terminal_maker_pos_base is None else number(terminal_maker_pos_base)
    h_pos = None if terminal_hedge_pos_base is None else number(terminal_hedge_pos_base)
    if (m_pos is not None and m_pos != ZERO) or (h_pos is not None and h_pos != ZERO):
        is_flat: Optional[bool] = False
    elif terminal_flat_verified is True and m_pos == ZERO and h_pos == ZERO:
        is_flat = True
    else:
        is_flat = None

    result: dict[str, Any] = {
        "marked_equity_usd": equity,
        "maker_volume_usd": maker_vol,
        "hedge_volume_usd": hedge_vol,
        "dual_volume_usd": dual_vol,
        "maker_only_intercept_per_m": maker_intercept,
        "maker_only_slope_per_m_per_bp": maker_slope,
        "maker_only_by_fee_bp": maker_by_fee,
        "dual_venue_intercept_per_m": dual_intercept,
        "dual_venue_slope_per_m_per_bp": dual_slope,
        "dual_venue_by_fee_bp": dual_by_fee,
        "terminal_flat": is_flat,
        "terminal_flat_cash_usd": equity if is_flat is True else None,
    }
    if extra_metrics:
        result.update(extra_metrics)
    return result


def _linear_terms(arm: Mapping[str, Any], denominator: str) -> tuple[Optional[Decimal], Optional[Decimal]]:
    if denominator not in ("maker_only", "dual_venue"):
        raise ValueError(f"unsupported denominator: {denominator}")
    return arm[f"{denominator}_intercept_per_m"], arm[f"{denominator}_slope_per_m_per_bp"]


def breakeven_hedge_fee_bp(
    arm_a: Mapping[str, Any],
    arm_b: Mapping[str, Any],
    *,
    denominator: str = "maker_only",
) -> Optional[Decimal]:
    """Return exact hedge fee (bp) where arm_a and arm_b have equal unit cost."""
    ia, sa = _linear_terms(arm_a, denominator)
    ib, sb = _linear_terms(arm_b, denominator)
    if ia is None or sa is None or ib is None or sb is None:
        return None
    diff_slope = sa - sb
    if diff_slope == ZERO:
        return None
    return (ib - ia) / diff_slope


def arm_strictly_cheaper_across_range(
    arm_a: Mapping[str, Any],
    arm_b: Mapping[str, Any],
    f_low: Any,
    f_high: Any,
    *,
    denominator: str = "maker_only",
) -> bool:
    """Return True iff linear unit cost of arm_a < arm_b over all f in [f_low, f_high]."""
    lo = number(f_low)
    hi = number(f_high)
    if lo > hi:
        raise ValueError(f"f_low ({lo}) must be <= f_high ({hi})")
    ia, sa = _linear_terms(arm_a, denominator)
    ib, sb = _linear_terms(arm_b, denominator)
    if ia is None or sa is None or ib is None or sb is None:
        return False
    return (ia + sa * lo) < (ib + sb * lo) and (ia + sa * hi) < (ib + sb * hi)


class UnitCostAuditor:
    """Stateless wrapper for arm-level cost evaluation and cross-arm dominance checks."""

    def __init__(self, hedge_fee_bps_grid: Sequence[Any] = DEFAULT_FEE_GRID) -> None:
        self.hedge_fee_bps_grid = tuple(number(f) for f in hedge_fee_bps_grid)

    def evaluate_arm(self, **kwargs: Any) -> dict[str, Any]:
        kwargs.setdefault("hedge_fee_bps_grid", self.hedge_fee_bps_grid)
        return evaluate_arm_costs(**kwargs)

    def compare_arms(
        self,
        arms: Mapping[str, Mapping[str, Any]],
        *,
        baseline_key: str,
        reference_keys: Sequence[str] = (),
        fee_range: tuple[Any, Any] = (ZERO, D("3.5")),
    ) -> dict[str, Any]:
        if baseline_key not in arms:
            raise ValueError(f"baseline_key not found: {baseline_key}")
        baseline = arms[baseline_key]
        f_low, f_high = fee_range
        comparisons: dict[str, Any] = {}
        for name, arm in arms.items():
            dv = baseline["maker_volume_usd"] - arm["maker_volume_usd"]
            saved = arm["marked_equity_usd"] - baseline["marked_equity_usd"]
            entry: dict[str, Any] = {
                "removed_maker_volume_usd": dv,
                "marginal_removed_cost_per_m": unit_cost_per_m(saved, dv) if dv > ZERO else None,
                "breakeven_vs_baseline_maker_only_bp": breakeven_hedge_fee_bp(
                    arm, baseline, denominator="maker_only"
                )
                if name != baseline_key
                else None,
                "breakeven_vs_baseline_dual_venue_bp": breakeven_hedge_fee_bp(
                    arm, baseline, denominator="dual_venue"
                )
                if name != baseline_key
                else None,
                "strictly_cheaper_than_vs": {},
            }
            for ref_key in reference_keys:
                if ref_key not in arms or ref_key == name:
                    continue
                ref_arm = arms[ref_key]
                entry["strictly_cheaper_than_vs"][ref_key] = {
                    "maker_only": arm_strictly_cheaper_across_range(
                        arm, ref_arm, f_low, f_high, denominator="maker_only"
                    ),
                    "dual_venue": arm_strictly_cheaper_across_range(
                        arm, ref_arm, f_low, f_high, denominator="dual_venue"
                    ),
                }
            comparisons[name] = entry
        return comparisons


def audit_mhmv_json(
    directory: Path,
    *,
    windows: Sequence[str] = ("w13", "w14"),
    auditor: Optional[UnitCostAuditor] = None,
) -> dict[str, Any]:
    """Read-only audit of preserved mhmv_<window>_out.json files."""
    auditor = auditor or UnitCostAuditor()
    arm_ids = ("Q200_B1000", "Q200_B1000_pull05", "Q200_B1000_mv30", "Q200_B1000_mv50")
    out: dict[str, Any] = {}

    for window in windows:
        path = directory / f"mhmv_{window}_out.json"
        raw = path.read_bytes()
        digest = sha256(raw).hexdigest()
        payload = json.loads(raw, parse_float=D)
        markets = payload["markets"]
        if set(markets) != EXPECTED_RWA_MARKETS:
            raise ValueError(f"unexpected markets in {path}: {sorted(markets)}")

        evaluated: dict[str, dict[str, Any]] = {}
        for arm_id in arm_ids:
            rows = [markets[m]["arms"][arm_id] for m in sorted(markets)]
            maker_vol = sum((r["arcus_maker_usd"] for r in rows), ZERO)
            hedge_vol = sum((r["rh_taker_usd"] for r in rows), ZERO)
            equity = sum((r["equity_usd_at_mids"] for r in rows), ZERO)
            term_arcus = sum((abs(r["terminal_inv_arcus_usd"]) for r in rows), ZERO)
            term_rh = sum((abs(r["terminal_inv_rh_usd"]) for r in rows), ZERO)
            # Mark-to-mid outputs only preserve cent-rounded USD estimates; without exact
            # base positions and settlement proof, terminal flatness remains unverified.
            evaluated[arm_id] = auditor.evaluate_arm(
                marked_equity_usd=equity,
                maker_volume_usd=maker_vol,
                hedge_volume_usd=hedge_vol,
                terminal_maker_pos_base=None,
                terminal_hedge_pos_base=None,
                terminal_flat_verified=False,
                extra_metrics={
                    "max_market_abs_net_usd": max(r["max_abs_net_usd"] for r in rows),
                    "max_market_drawdown_usd": max(r["max_drawdown_usd"] for r in rows),
                    "terminal_abs_arcus_usd": term_arcus,
                    "terminal_abs_rh_usd": term_rh,
                    "terminal_nonflat_usd_clue": term_arcus > ZERO or term_rh > ZERO,
                    "terminal_net_drain_est_usd": sum((r["terminal_net_drain_est_usd"] for r in rows), ZERO),
                    "hedge_retries": sum(int(r["hedge_retries"]) for r in rows),
                },
            )

        out[window] = {
            "source_path": str(path),
            "source_sha256": digest,
            "arms": evaluated,
            "comparisons": auditor.compare_arms(
                evaluated,
                baseline_key="Q200_B1000",
                reference_keys=("Q200_B1000", "Q200_B1000_pull05"),
            ),
        }
    return out


def _json_default(value: Any) -> Any:
    if isinstance(value, Decimal):
        return str(value)
    raise TypeError(f"not JSON serializable: {type(value).__name__}")


def main(argv: Optional[Sequence[str]] = None) -> int:
    args = list(sys.argv[1:] if argv is None else argv)
    if len(args) != 1:
        print("usage: maker_hedge_risk_guards.py <mhmv_output_dir>", file=sys.stderr)
        return 2
    report = audit_mhmv_json(Path(args[0]))
    print(json.dumps(report, indent=2, sort_keys=True, default=_json_default))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
