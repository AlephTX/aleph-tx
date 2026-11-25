#!/usr/bin/env python3
"""Offline causal Backpack Depth-Multiplier Guardian simulator (Section 6.2 Step 2).

Evaluates three strictly causal quoting/hedging arms on identical chronological
market frames with a 100 ms non-PostOnly hedge speed bump and zero assumed
subsidies (`subsidy_usd_assumed = 0.0`):
  1. naive_cancel_heavy_side
  2. outer_band_dm_guardian
  3. outer_band_dm_guardian_cascade_guard
"""
from __future__ import annotations

import argparse
from collections import deque
from dataclasses import asdict, dataclass
import json
import math
from pathlib import Path
import sys
from typing import Any, Iterable

BPS_DIV = 10_000.0
MONTHLY_POOL_USD = 250_000.0
HOURS_PER_MONTH = 720.0
VALID_TRADE_SIDES = ("buy", "sell", "none")
REQUIRED_FRAME_KEYS = (
    "timestamp_ms",
    "bid_px",
    "ask_px",
    "mid_px",
    "trade_side",
    "trade_notional_usd",
)


@dataclass(frozen=True)
class TierPreset:
    tier: str
    max_bandwidth_bps: float
    min_side_notional_usd: float
    outer_band_bps: float
    dm_floor: float
    non_post_only_speed_bump_ms: int
    default_inner_band_bps: float
    default_depth_per_bp_usd: float
    default_max_loss_usd: float
    default_max_inventory_usd: float
    default_cascade_trigger_bps: float


TIER_PRESETS: dict[str, TierPreset] = {
    "tier3_alt": TierPreset(
        tier="tier3_alt",
        max_bandwidth_bps=25.0,
        min_side_notional_usd=5_000.0,
        outer_band_bps=23.0,
        dm_floor=0.25,
        non_post_only_speed_bump_ms=100,
        default_inner_band_bps=8.0,
        default_depth_per_bp_usd=450.0,
        default_max_loss_usd=150.0,
        default_max_inventory_usd=15_000.0,
        default_cascade_trigger_bps=10.0,
    ),
    "tier4_equity": TierPreset(
        tier="tier4_equity",
        max_bandwidth_bps=20.0,
        min_side_notional_usd=10_000.0,
        outer_band_bps=18.0,
        dm_floor=0.25,
        non_post_only_speed_bump_ms=100,
        default_inner_band_bps=6.0,
        default_depth_per_bp_usd=900.0,
        default_max_loss_usd=250.0,
        default_max_inventory_usd=30_000.0,
        default_cascade_trigger_bps=8.0,
    ),
}


@dataclass(frozen=True)
class SimConfig:
    preset: TierPreset
    inner_band_bps: float
    soft_limit_usd: float
    hedge_trigger_usd: float
    max_inventory_usd: float
    max_loss_usd: float
    maker_fee_bps: float = 0.0
    hedge_fee_bps: float = 1.5
    hedge_slippage_bps: float = 0.0
    depth_per_bp_usd: float = 450.0
    cascade_window_ms: int = 500
    cascade_trigger_bps: float = 10.0

    @classmethod
    def for_tier(cls, tier: str, **overrides: Any) -> SimConfig:
        if tier not in TIER_PRESETS:
            raise ValueError(f"unknown tier {tier!r}; expected one of {sorted(TIER_PRESETS)}")
        p = TIER_PRESETS[tier]
        cfg = cls(
            preset=p,
            inner_band_bps=p.default_inner_band_bps,
            soft_limit_usd=0.6 * p.min_side_notional_usd,
            hedge_trigger_usd=0.9 * p.min_side_notional_usd,
            max_inventory_usd=p.default_max_inventory_usd,
            max_loss_usd=p.default_max_loss_usd,
            depth_per_bp_usd=p.default_depth_per_bp_usd,
            cascade_trigger_bps=p.default_cascade_trigger_bps,
        )
        if overrides:
            data = asdict(cfg)
            data.pop("preset", None)
            data.update(overrides)
            cfg = cls(preset=p, **data)
        cfg.validate()
        return cfg

    def validate(self) -> None:
        max_bandwidth = _require_finite_float(
            self.preset.max_bandwidth_bps, "preset.max_bandwidth_bps"
        )
        min_side_notional = _require_finite_float(
            self.preset.min_side_notional_usd, "preset.min_side_notional_usd"
        )
        outer_band = _require_finite_float(
            self.preset.outer_band_bps, "preset.outer_band_bps"
        )
        dm_floor = _require_finite_float(self.preset.dm_floor, "preset.dm_floor")
        _require_int_field(
            self.preset.non_post_only_speed_bump_ms,
            "preset.non_post_only_speed_bump_ms",
            min_value=0,
        )
        if max_bandwidth <= 0.0:
            raise ValueError("preset.max_bandwidth_bps must be > 0")
        if min_side_notional <= 0.0:
            raise ValueError("preset.min_side_notional_usd must be > 0")
        if outer_band <= 0.0 or outer_band > max_bandwidth:
            raise ValueError("outer_band_bps must be in (0, max_bandwidth_bps]")
        if not (0.0 < dm_floor <= 1.0):
            raise ValueError("preset.dm_floor must be in (0, 1]")

        inner_band = _require_finite_float(self.inner_band_bps, "inner_band_bps")
        if inner_band <= 0.0 or inner_band >= outer_band:
            raise ValueError("inner_band_bps must be finite and in (0, outer_band_bps)")

        for field_name in (
            "soft_limit_usd",
            "hedge_trigger_usd",
            "max_inventory_usd",
            "max_loss_usd",
            "depth_per_bp_usd",
            "cascade_trigger_bps",
        ):
            val = _require_finite_float(getattr(self, field_name), field_name)
            if val <= 0.0:
                raise ValueError(f"{field_name} must be finite and > 0")

        _require_int_field(self.cascade_window_ms, "cascade_window_ms", min_value=1)

        maker_fee = _require_finite_float(self.maker_fee_bps, "maker_fee_bps")
        if not (0.0 <= maker_fee < BPS_DIV):
            raise ValueError("maker_fee_bps must be in [0, 10000) (no assumed rebate subsidy)")
        hedge_fee = _require_finite_float(self.hedge_fee_bps, "hedge_fee_bps")
        if not (0.0 <= hedge_fee < BPS_DIV):
            raise ValueError("hedge_fee_bps must be in [0, 10000)")
        hedge_slippage = _require_finite_float(self.hedge_slippage_bps, "hedge_slippage_bps")
        if not (0.0 <= hedge_slippage < BPS_DIV):
            raise ValueError("hedge_slippage_bps must be in [0, 10000)")


@dataclass(frozen=True)
class Frame:
    """Chronological L1 snapshot with same-book BBO valuation.

    `mid_px` is treated strictly as a same-book BBO valuation (midpoint or
    microprice), not an independent external oracle, so it must lie within
    `[bid_px, ask_px]`.
    """

    timestamp_ms: int
    bid_px: float
    ask_px: float
    mid_px: float
    trade_side: str
    trade_notional_usd: float


@dataclass(frozen=True)
class QuoteLevel:
    price: float
    notional_usd: float


@dataclass(frozen=True)
class PendingHedge:
    submit_ts_ms: int
    eligible_ts_ms: int
    side: str  # "buy" or "sell"
    qty_base: float


def _require_finite_float(value: Any, field_name: str) -> float:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise ValueError(f"field {field_name!r} must be numeric, got {type(value).__name__}")
    out = float(value)
    if not math.isfinite(out):
        raise ValueError(f"field {field_name!r} must be finite, got {value!r}")
    return out


def _require_int_field(value: Any, field_name: str, *, min_value: int = 0) -> int:
    if isinstance(value, bool) or not isinstance(value, int):
        raise ValueError(f"field {field_name!r} must be an int, got {type(value).__name__}")
    if value < min_value:
        raise ValueError(f"field {field_name!r} must be >= {min_value}, got {value}")
    return value


def _require_int_ts(value: Any) -> int:
    return _require_int_field(value, "timestamp_ms", min_value=0)


def validate_frame(raw: Frame | dict[str, Any], prev_ts_ms: int | None) -> Frame:
    """Strictly fail-closed validation for a single chronological frame."""
    if isinstance(raw, Frame):
        data: dict[str, Any] = {
            "timestamp_ms": raw.timestamp_ms,
            "bid_px": raw.bid_px,
            "ask_px": raw.ask_px,
            "mid_px": raw.mid_px,
            "trade_side": raw.trade_side,
            "trade_notional_usd": raw.trade_notional_usd,
        }
    elif isinstance(raw, dict):
        missing = [k for k in REQUIRED_FRAME_KEYS if k not in raw]
        if missing:
            raise ValueError(f"frame missing required keys: {missing}")
        data = raw
    else:
        raise ValueError(f"frame must be Frame or dict, got {type(raw).__name__}")

    ts = _require_int_ts(data["timestamp_ms"])
    if prev_ts_ms is not None and ts <= prev_ts_ms:
        raise ValueError(f"non-monotonic timestamp_ms: {ts} <= {prev_ts_ms}")

    bid_px = _require_finite_float(data["bid_px"], "bid_px")
    ask_px = _require_finite_float(data["ask_px"], "ask_px")
    mid_px = _require_finite_float(data["mid_px"], "mid_px")
    if bid_px <= 0.0:
        raise ValueError(f"bid_px must be > 0, got {bid_px}")
    if ask_px <= bid_px:
        raise ValueError(f"crossed or locked book: ask_px ({ask_px}) <= bid_px ({bid_px})")
    if mid_px <= 0.0:
        raise ValueError(f"mid_px must be > 0, got {mid_px}")
    if not (bid_px <= mid_px <= ask_px):
        raise ValueError(
            f"mid_px ({mid_px}) must lie within same-book BBO [{bid_px}, {ask_px}]"
        )

    trade_side = data["trade_side"]
    if not isinstance(trade_side, str) or trade_side not in VALID_TRADE_SIDES:
        raise ValueError(f"invalid trade_side {trade_side!r}; expected one of {VALID_TRADE_SIDES}")

    trade_notional = _require_finite_float(data["trade_notional_usd"], "trade_notional_usd")
    if trade_notional < 0.0:
        raise ValueError(f"trade_notional_usd must be >= 0, got {trade_notional}")
    if trade_side == "none" and trade_notional > 0.0:
        raise ValueError("trade_notional_usd > 0 is invalid when trade_side == 'none'")

    return Frame(
        timestamp_ms=ts,
        bid_px=bid_px,
        ask_px=ask_px,
        mid_px=mid_px,
        trade_side=trade_side,
        trade_notional_usd=trade_notional,
    )


def validate_frames(raw_frames: Iterable[Frame | dict[str, Any]]) -> list[Frame]:
    """Validate an entire chronological frame sequence fail-closed."""
    frames: list[Frame] = []
    prev_ts: int | None = None
    for row in raw_frames:
        frame = validate_frame(row, prev_ts)
        prev_ts = frame.timestamp_ms
        frames.append(frame)
    if not frames:
        raise ValueError("frame sequence is empty")
    return frames


def _reject_json_constant(token: str) -> Any:
    raise ValueError(f"non-finite JSON constant rejected: {token!r}")


def load_tape_jsonl(path: Path) -> list[Frame]:
    """Load and fail-closed validate a JSONL frame tape."""
    if not path.is_file():
        raise FileNotFoundError(f"tape file not found or not a regular file: {path}")
    raw_rows: list[dict[str, Any]] = []
    with path.open("r", encoding="utf-8") as fh:
        for lineno, line in enumerate(fh, start=1):
            stripped = line.strip()
            if not stripped:
                continue
            try:
                obj = json.loads(stripped, parse_constant=_reject_json_constant)
            except Exception as exc:
                raise ValueError(f"invalid JSON on line {lineno} of {path}: {exc}") from exc
            if not isinstance(obj, dict):
                raise ValueError(f"line {lineno} of {path} must be a JSON object")
            raw_rows.append(obj)
    return validate_frames(raw_rows)


def compute_snapshot_dm(
    preset: TierPreset,
    mid_px: float,
    bid_quote: QuoteLevel | None,
    ask_quote: QuoteLevel | None,
) -> tuple[float, bool]:
    """Score a single snapshot Depth Multiplier (DM) under Backpack Tier rules.

    Both sides must retain qualifying notional >= preset.min_side_notional_usd
    within <= preset.max_bandwidth_bps of mid_px; otherwise DM drops to
    preset.dm_floor (0.25). Returns (dm, is_zero_side).
    """
    mid = _require_finite_float(mid_px, "mid_px")
    if mid <= 0.0:
        raise ValueError(f"mid_px must be finite and > 0, got {mid_px}")
    max_bandwidth = _require_finite_float(preset.max_bandwidth_bps, "preset.max_bandwidth_bps")
    min_side_notional = _require_finite_float(
        preset.min_side_notional_usd, "preset.min_side_notional_usd"
    )
    dm_floor = _require_finite_float(preset.dm_floor, "preset.dm_floor")
    if max_bandwidth <= 0.0 or min_side_notional <= 0.0 or not (0.0 < dm_floor <= 1.0):
        raise ValueError("preset bandwidth, notional, or dm_floor out of valid domain")

    def qualifying_notional(q: QuoteLevel | None, is_bid: bool) -> float:
        if q is None:
            return 0.0
        if (
            isinstance(q.price, bool)
            or isinstance(q.notional_usd, bool)
            or not isinstance(q.price, (int, float))
            or not isinstance(q.notional_usd, (int, float))
        ):
            return 0.0
        q_px = float(q.price)
        q_notional = float(q.notional_usd)
        if q_notional <= 0.0 or not math.isfinite(q_notional):
            return 0.0
        if q_px <= 0.0 or not math.isfinite(q_px):
            return 0.0
        dist_bps = (
            (mid - q_px) / mid * BPS_DIV
            if is_bid
            else (q_px - mid) / mid * BPS_DIV
        )
        if 0.0 <= dist_bps <= max_bandwidth + 1e-9:
            return q_notional
        return 0.0

    bid_qual = qualifying_notional(bid_quote, is_bid=True)
    ask_qual = qualifying_notional(ask_quote, is_bid=False)
    is_zero_side = bid_qual <= 0.0 or ask_qual <= 0.0
    if bid_qual >= min_side_notional and ask_qual >= min_side_notional:
        return 1.0, is_zero_side
    return dm_floor, is_zero_side


def build_synthetic_tape(tier: str) -> list[Frame]:
    """Build deterministic calm + cascade waterfall + recovery synthetic frames."""
    if tier not in TIER_PRESETS:
        raise ValueError(f"unknown tier {tier!r}")
    p = TIER_PRESETS[tier]
    unit = p.min_side_notional_usd
    frames: list[Frame] = []
    ts = 1_795_200_000_000
    mid = 100.0
    half_spread_bps = 2.0

    def push_frame(step_ms: int, new_mid: float, side: str, notional: float) -> None:
        nonlocal ts, mid
        ts += step_ms
        mid = new_mid
        bid = round(mid * (1.0 - half_spread_bps / BPS_DIV), 6)
        ask = round(mid * (1.0 + half_spread_bps / BPS_DIV), 6)
        frames.append(
            Frame(
                timestamp_ms=ts,
                bid_px=bid,
                ask_px=ask,
                mid_px=round(mid, 6),
                trade_side=side,
                trade_notional_usd=round(notional, 4),
            )
        )

    # Phase 1: Calm clustered two-sided flow (120 frames, 100 ms spacing = 12 s).
    # Pairs of same-side 0.44x fills push inventory to ~0.88x (> 0.60x soft_limit_usd,
    # < 0.90x hedge_trigger_usd), causing naive_cancel_heavy_side to cancel to $0 (DM=0.25)
    # while outer_band_dm_guardian retains DM=1.0 via outer_band_bps.
    calm_pattern = (
        ("buy", 2.0, 0.44 * unit),
        ("buy", 1.5, 0.44 * unit),
        ("sell", -2.0, 0.44 * unit),
        ("sell", -1.5, 0.44 * unit),
        ("sell", -1.5, 0.44 * unit),
        ("buy", 2.0, 0.44 * unit),
    )
    for i in range(120):
        side, delta_bps, notional = calm_pattern[i % len(calm_pattern)]
        push_frame(100, mid * (1.0 + delta_bps / BPS_DIV), side, notional)

    # Phase 2: Waterfall cascade onset (3 frames * -4 bps = -12 bps over 150 ms)
    # triggering cascade_guard early, followed by 8 heavy toxic sell waves
    # (~15.5 bps total sweep reach: hits inner_band_bps, misses outer_band_bps).
    for _ in range(3):
        push_frame(50, mid * (1.0 - 4.0 / BPS_DIV), "sell", 0.30 * unit)
    for _ in range(8):
        push_frame(50, mid * (1.0 - 4.5 / BPS_DIV), "sell", 0.80 * unit)
    for _ in range(4):
        push_frame(50, mid * (1.0 - 1.5 / BPS_DIV), "sell", 0.35 * unit)

    # Phase 3: Post-cascade calm recovery (80 frames, 100 ms spacing = 8 s).
    for i in range(80):
        side, delta_bps, notional = calm_pattern[i % len(calm_pattern)]
        push_frame(100, mid * (1.0 + delta_bps / BPS_DIV), side, notional)

    return validate_frames(frames)


def _clamp_post_only_quotes(
    frame: Frame,
    bid_offset_bps: float | None,
    ask_offset_bps: float | None,
    notional_usd: float,
) -> tuple[QuoteLevel | None, QuoteLevel | None]:
    bid_q: QuoteLevel | None = None
    ask_q: QuoteLevel | None = None
    if bid_offset_bps is not None:
        px = min(frame.mid_px * (1.0 - bid_offset_bps / BPS_DIV), frame.bid_px)
        if px > 0.0 and px < frame.ask_px:
            bid_q = QuoteLevel(price=px, notional_usd=notional_usd)
    if ask_offset_bps is not None:
        px = max(frame.mid_px * (1.0 + ask_offset_bps / BPS_DIV), frame.ask_px)
        if px > frame.bid_px:
            ask_q = QuoteLevel(price=px, notional_usd=notional_usd)
    return bid_q, ask_q


def simulate_arm(
    arm_name: str,
    frames: list[Frame],
    cfg: SimConfig,
) -> dict[str, Any]:
    """Run one quoting/hedging arm strictly causally over validated frames."""
    if arm_name not in (
        "naive_cancel_heavy_side",
        "outer_band_dm_guardian",
        "outer_band_dm_guardian_cascade_guard",
    ):
        raise ValueError(f"unknown arm {arm_name!r}")
    cfg.validate()
    frames = validate_frames(frames)

    preset = cfg.preset
    cash_usd = 0.0
    inventory_base = 0.0
    maker_fees_usd = 0.0
    hedge_fees_usd = 0.0
    maker_volume_usd = 0.0
    hedge_volume_usd = 0.0
    terminal_unwind_volume_usd = 0.0
    maker_fills = 0
    hedge_fills = 0

    peak_equity_usd = 0.0
    max_drawdown_usd = 0.0

    bid_quote: QuoteLevel | None = None
    ask_quote: QuoteLevel | None = None
    pending_hedges: deque[PendingHedge] = deque()
    rolling_mids: deque[tuple[int, float]] = deque()

    dm_values: list[float] = []
    zero_side_snapshots = 0

    halted = False
    circuit_breaker_triggered = False
    circuit_breaker_ts_ms: int | None = None
    circuit_breaker_reason: str | None = None

    def execute_hedge_fill(side: str, qty_base: float, frame: Frame) -> float:
        nonlocal cash_usd, inventory_base, hedge_fees_usd, hedge_volume_usd, hedge_fills
        if qty_base <= 1e-12:
            return 0.0
        if side == "sell":
            exec_px = frame.bid_px * (1.0 - cfg.hedge_slippage_bps / BPS_DIV)
            notional = qty_base * exec_px
            fee = notional * cfg.hedge_fee_bps / BPS_DIV
            inventory_base -= qty_base
            cash_usd += notional - fee
        else:
            exec_px = frame.ask_px * (1.0 + cfg.hedge_slippage_bps / BPS_DIV)
            notional = qty_base * exec_px
            fee = notional * cfg.hedge_fee_bps / BPS_DIV
            inventory_base += qty_base
            cash_usd -= notional + fee
        hedge_fees_usd += fee
        hedge_volume_usd += notional
        hedge_fills += 1
        return notional

    def pending_signed_base() -> float:
        return sum(h.qty_base if h.side == "buy" else -h.qty_base for h in pending_hedges)

    for frame in frames:
        # 1. Execute any mature non-PostOnly hedges whose 100 ms speed bump has elapsed.
        while pending_hedges and frame.timestamp_ms >= pending_hedges[0].eligible_ts_ms:
            h = pending_hedges.popleft()
            execute_hedge_fill(h.side, h.qty_base, frame)

        # 2. Match incoming aggressor trade against standing PostOnly quotes from prior frame.
        if frame.trade_notional_usd > 0.0 and frame.trade_side == "sell" and bid_quote is not None:
            sweep_bps = frame.trade_notional_usd / cfg.depth_per_bp_usd
            sweep_low_px = frame.bid_px * (1.0 - sweep_bps / BPS_DIV)
            if bid_quote.price >= sweep_low_px:
                fill_notional = min(bid_quote.notional_usd, frame.trade_notional_usd)
                fill_base = fill_notional / bid_quote.price
                fee = fill_notional * cfg.maker_fee_bps / BPS_DIV
                inventory_base += fill_base
                cash_usd -= fill_notional + fee
                maker_fees_usd += fee
                maker_volume_usd += fill_notional
                maker_fills += 1
        elif frame.trade_notional_usd > 0.0 and frame.trade_side == "buy" and ask_quote is not None:
            sweep_bps = frame.trade_notional_usd / cfg.depth_per_bp_usd
            sweep_high_px = frame.ask_px * (1.0 + sweep_bps / BPS_DIV)
            if ask_quote.price <= sweep_high_px:
                fill_notional = min(ask_quote.notional_usd, frame.trade_notional_usd)
                fill_base = fill_notional / ask_quote.price
                fee = fill_notional * cfg.maker_fee_bps / BPS_DIV
                inventory_base -= fill_base
                cash_usd += fill_notional - fee
                maker_fees_usd += fee
                maker_volume_usd += fill_notional
                maker_fills += 1

        # 3. Causal rolling waterfall state + mark-to-market equity / drawdown tracking.
        rolling_mids.append((frame.timestamp_ms, frame.mid_px))
        cutoff = frame.timestamp_ms - cfg.cascade_window_ms
        while rolling_mids and rolling_mids[0][0] < cutoff:
            rolling_mids.popleft()

        equity_usd = cash_usd + inventory_base * frame.mid_px
        if equity_usd > peak_equity_usd:
            peak_equity_usd = equity_usd
        drawdown_usd = max(0.0, peak_equity_usd - equity_usd)
        if drawdown_usd > max_drawdown_usd:
            max_drawdown_usd = drawdown_usd

        inventory_usd = inventory_base * frame.mid_px

        # 4. Arm-specific risk & circuit-breaker / hedge / quote updates.
        if arm_name == "outer_band_dm_guardian_cascade_guard" and not halted:
            breach_reason: str | None = None
            if drawdown_usd >= cfg.max_loss_usd:
                breach_reason = "max_drawdown_breached"
            elif abs(inventory_usd) > cfg.max_inventory_usd:
                breach_reason = "max_inventory_breached"
            if breach_reason is not None:
                halted = True
                circuit_breaker_triggered = True
                circuit_breaker_ts_ms = frame.timestamp_ms
                circuit_breaker_reason = breach_reason
                bid_quote = None
                ask_quote = None
                eff_base = inventory_base + pending_signed_base()
                if abs(eff_base) > 1e-12:
                    pending_hedges.append(
                        PendingHedge(
                            submit_ts_ms=frame.timestamp_ms,
                            eligible_ts_ms=frame.timestamp_ms + preset.non_post_only_speed_bump_ms,
                            side="sell" if eff_base > 0.0 else "buy",
                            qty_base=abs(eff_base),
                        )
                    )

        if not halted:
            eff_base = inventory_base + pending_signed_base()
            if abs(eff_base * frame.mid_px) >= cfg.hedge_trigger_usd:
                pending_hedges.append(
                    PendingHedge(
                        submit_ts_ms=frame.timestamp_ms,
                        eligible_ts_ms=frame.timestamp_ms + preset.non_post_only_speed_bump_ms,
                        side="sell" if eff_base > 0.0 else "buy",
                        qty_base=abs(eff_base),
                    )
                )

            if arm_name == "naive_cancel_heavy_side":
                bid_off = None if inventory_usd > cfg.soft_limit_usd else cfg.inner_band_bps
                ask_off = None if inventory_usd < -cfg.soft_limit_usd else cfg.inner_band_bps
            elif arm_name == "outer_band_dm_guardian":
                bid_off = (
                    preset.outer_band_bps
                    if inventory_usd > cfg.soft_limit_usd
                    else cfg.inner_band_bps
                )
                ask_off = (
                    preset.outer_band_bps
                    if inventory_usd < -cfg.soft_limit_usd
                    else cfg.inner_band_bps
                )
            else:
                roll_max = max(m for _, m in rolling_mids)
                roll_min = min(m for _, m in rolling_mids)
                drop_bps = (roll_max - frame.mid_px) / roll_max * BPS_DIV
                rally_bps = (frame.mid_px - roll_min) / roll_min * BPS_DIV
                bid_off = (
                    preset.outer_band_bps
                    if (inventory_usd > cfg.soft_limit_usd or drop_bps >= cfg.cascade_trigger_bps)
                    else cfg.inner_band_bps
                )
                ask_off = (
                    preset.outer_band_bps
                    if (inventory_usd < -cfg.soft_limit_usd or rally_bps >= cfg.cascade_trigger_bps)
                    else cfg.inner_band_bps
                )

            bid_quote, ask_quote = _clamp_post_only_quotes(
                frame, bid_off, ask_off, preset.min_side_notional_usd
            )
        else:
            bid_quote = None
            ask_quote = None

        dm, is_zero_side = compute_snapshot_dm(preset, frame.mid_px, bid_quote, ask_quote)
        dm_values.append(dm)
        if is_zero_side:
            zero_side_snapshots += 1

    # 5. Terminal unwind at final frame only (no lookahead beyond tape end).
    final_frame = frames[-1]
    pending_hedges.clear()
    if abs(inventory_base) > 1e-12:
        unwind_side = "sell" if inventory_base > 0.0 else "buy"
        terminal_unwind_volume_usd = execute_hedge_fill(
            unwind_side, abs(inventory_base), final_frame
        )
    final_drawdown = max(0.0, peak_equity_usd - cash_usd)
    if final_drawdown > max_drawdown_usd:
        max_drawdown_usd = final_drawdown

    if not (math.isfinite(cash_usd) and math.isfinite(max_drawdown_usd)):
        raise ValueError("simulation accounting overflowed to non-finite float")

    total_snapshots = len(dm_values)
    mean_dm = sum(dm_values) / total_snapshots
    min_dm = min(dm_values)
    ge_090 = sum(1 for v in dm_values if v >= 0.90)
    pct_ge_090 = 100.0 * ge_090 / total_snapshots

    duration_ms = frames[-1].timestamp_ms - frames[0].timestamp_ms
    duration_hours = duration_ms / 3_600_000.0 if duration_ms > 0 else 0.0
    if cash_usd >= 0.0:
        breakeven_share: float | None = 0.0
    elif duration_hours > 0.0:
        monthly_loss_usd = (-cash_usd) * (HOURS_PER_MONTH / duration_hours)
        breakeven_share = round(monthly_loss_usd / MONTHLY_POOL_USD, 8)
    else:
        breakeven_share = None

    return {
        "arm": arm_name,
        "subsidy_usd_assumed": 0.0,
        "ex_subsidy_cash_pnl_usd": round(cash_usd, 6),
        "maker_fees_usd": round(maker_fees_usd, 6),
        "hedge_fees_usd": round(hedge_fees_usd, 6),
        "maker_volume_usd": round(maker_volume_usd, 4),
        "hedge_volume_usd": round(hedge_volume_usd, 4),
        "terminal_unwind_volume_usd": round(terminal_unwind_volume_usd, 4),
        "maker_fills": maker_fills,
        "hedge_fills": hedge_fills,
        "dm_metrics": {
            "mean_dm": round(mean_dm, 6),
            "min_dm": round(min_dm, 6),
            "pct_snapshots_dm_ge_090": round(pct_ge_090, 4),
            "zero_side_snapshots": zero_side_snapshots,
            "total_snapshots": total_snapshots,
        },
        "max_drawdown_usd": round(max_drawdown_usd, 6),
        "circuit_breaker_triggered": circuit_breaker_triggered,
        "circuit_breaker_ts_ms": circuit_breaker_ts_ms,
        "circuit_breaker_reason": circuit_breaker_reason,
        "breakeven_monthly_pool_share_for_250k_pool": breakeven_share,
    }


def run_comparison(
    tier: str,
    frames: list[Frame] | None = None,
    config: SimConfig | None = None,
    tape_source: str | None = None,
) -> dict[str, Any]:
    """Validate frames and evaluate all three arms on the identical sequence."""
    cfg = config if config is not None else SimConfig.for_tier(tier)
    if cfg.preset.tier != tier:
        raise ValueError(f"config tier {cfg.preset.tier!r} does not match tier {tier!r}")
    validated = validate_frames(frames if frames is not None else build_synthetic_tape(tier))
    duration_ms = validated[-1].timestamp_ms - validated[0].timestamp_ms
    duration_hours = duration_ms / 3_600_000.0 if duration_ms > 0 else 0.0

    arms = {
        name: simulate_arm(name, validated, cfg)
        for name in (
            "naive_cancel_heavy_side",
            "outer_band_dm_guardian",
            "outer_band_dm_guardian_cascade_guard",
        )
    }
    return {
        "schema": "backpack_dm_guardian_sim.v1",
        "tier": cfg.preset.tier,
        "preset": {
            "max_bandwidth_bps": cfg.preset.max_bandwidth_bps,
            "min_side_notional_usd": cfg.preset.min_side_notional_usd,
            "outer_band_bps": cfg.preset.outer_band_bps,
            "dm_floor": cfg.preset.dm_floor,
            "non_post_only_speed_bump_ms": cfg.preset.non_post_only_speed_bump_ms,
        },
        "subsidy_usd_assumed": 0.0,
        "monthly_pool_usd": MONTHLY_POOL_USD,
        "tape_source": tape_source or "builtin_synthetic_calm_cascade",
        "frames": len(validated),
        "duration_ms": duration_ms,
        "duration_hours": round(duration_hours, 8),
        "arms": arms,
    }


def run_self_test() -> dict[str, Any]:
    """Run deterministic internal checks covering DM, causality, CB, and fail-closed validation."""
    reports: dict[str, Any] = {}
    for tier in ("tier3_alt", "tier4_equity"):
        rep = run_comparison(tier)
        assert rep["subsidy_usd_assumed"] == 0.0
        naive = rep["arms"]["naive_cancel_heavy_side"]
        guard = rep["arms"]["outer_band_dm_guardian"]
        cascade = rep["arms"]["outer_band_dm_guardian_cascade_guard"]
        assert naive["subsidy_usd_assumed"] == 0.0
        assert naive["dm_metrics"]["zero_side_snapshots"] > 0, naive
        assert naive["dm_metrics"]["mean_dm"] < 0.90, naive
        assert guard["dm_metrics"]["zero_side_snapshots"] == 0, guard
        assert guard["dm_metrics"]["mean_dm"] >= 0.90, guard
        assert cascade["dm_metrics"]["zero_side_snapshots"] == 0, cascade
        assert cascade["dm_metrics"]["mean_dm"] >= 0.90, cascade
        reports[tier] = rep

    # Verify causal 100 ms anti-lookahead invariant.
    cfg3 = SimConfig.for_tier("tier3_alt")
    base_frames = [
        {"timestamp_ms": 1000, "bid_px": 99.98, "ask_px": 100.02, "mid_px": 100.0, "trade_side": "none", "trade_notional_usd": 0.0},
        {"timestamp_ms": 1100, "bid_px": 99.85, "ask_px": 99.89, "mid_px": 99.87, "trade_side": "sell", "trade_notional_usd": 6000.0},
        {"timestamp_ms": 1150, "bid_px": 99.50, "ask_px": 99.54, "mid_px": 99.52, "trade_side": "none", "trade_notional_usd": 0.0},
        {"timestamp_ms": 1199, "bid_px": 100.40, "ask_px": 100.44, "mid_px": 100.42, "trade_side": "none", "trade_notional_usd": 0.0},
        {"timestamp_ms": 1200, "bid_px": 99.70, "ask_px": 99.74, "mid_px": 99.72, "trade_side": "none", "trade_notional_usd": 0.0},
    ]
    perturbed_frames = [dict(r) for r in base_frames]
    perturbed_frames[2]["bid_px"] = 97.00
    perturbed_frames[2]["ask_px"] = 97.04
    perturbed_frames[2]["mid_px"] = 97.02
    perturbed_frames[3]["bid_px"] = 104.00
    perturbed_frames[3]["ask_px"] = 104.04
    perturbed_frames[3]["mid_px"] = 104.02
    r_base = simulate_arm("outer_band_dm_guardian", validate_frames(base_frames), cfg3)
    r_pert = simulate_arm("outer_band_dm_guardian", validate_frames(perturbed_frames), cfg3)
    assert r_base["ex_subsidy_cash_pnl_usd"] == r_pert["ex_subsidy_cash_pnl_usd"]
    assert r_base["hedge_volume_usd"] == r_pert["hedge_volume_usd"]

    # Verify fail-closed validation rejects NaN / crossed book / out-of-spread mid / non-monotonic ts / bad side.
    for bad in (
        [{"timestamp_ms": 100, "bid_px": float("nan"), "ask_px": 101.0, "mid_px": 100.5, "trade_side": "none", "trade_notional_usd": 0.0}],
        [{"timestamp_ms": 100, "bid_px": 101.0, "ask_px": 100.0, "mid_px": 100.5, "trade_side": "none", "trade_notional_usd": 0.0}],
        [{"timestamp_ms": 100, "bid_px": 99.50, "ask_px": 99.54, "mid_px": 90.0, "trade_side": "none", "trade_notional_usd": 0.0}],
        [{"timestamp_ms": 200, "bid_px": 99.0, "ask_px": 101.0, "mid_px": 100.0, "trade_side": "none", "trade_notional_usd": 0.0},
         {"timestamp_ms": 150, "bid_px": 99.0, "ask_px": 101.0, "mid_px": 100.0, "trade_side": "none", "trade_notional_usd": 0.0}],
        [{"timestamp_ms": 100, "bid_px": 99.0, "ask_px": 101.0, "mid_px": 100.0, "trade_side": "invalid", "trade_notional_usd": 0.0}],
    ):
        try:
            validate_frames(bad)
        except ValueError:
            pass
        else:
            raise AssertionError(f"expected ValueError for invalid frame {bad}")

    return {"self_test": "passed", "tiers": reports}


def _print_human_summary(report: dict[str, Any]) -> None:
    print(
        f"Backpack DM Guardian Offline Sim | tier={report['tier']} | "
        f"frames={report['frames']} | subsidy_usd_assumed=${report['subsidy_usd_assumed']:.2f}"
    )
    for name, arm in report["arms"].items():
        dm = arm["dm_metrics"]
        print(
            f"  {name:38s} | pnl=${arm['ex_subsidy_cash_pnl_usd']:9.4f} | "
            f"mean_dm={dm['mean_dm']:.4f} | zero_side={dm['zero_side_snapshots']:3d} | "
            f"dd=${arm['max_drawdown_usd']:7.2f} | cb={arm['circuit_breaker_triggered']} | "
            f"be_share={arm['breakeven_monthly_pool_share_for_250k_pool']}"
        )


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--tier",
        choices=sorted(TIER_PRESETS.keys()),
        default="tier3_alt",
        help="Backpack MM tier preset (tier3_alt or tier4_equity).",
    )
    parser.add_argument(
        "--tape",
        type=Path,
        default=None,
        help="Optional chronological JSONL tape path.",
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="Emit structured JSON output.",
    )
    parser.add_argument(
        "--self-test",
        action="store_true",
        help="Run built-in causal, DM, circuit-breaker, and fail-closed self-tests.",
    )
    args = parser.parse_args(argv)
    if args.self_test and args.tape is not None:
        parser.error("cannot combine --self-test with --tape")

    try:
        if args.self_test:
            res = run_self_test()
            if args.json:
                print(json.dumps(res, indent=2, allow_nan=False))
            else:
                t3 = res["tiers"]["tier3_alt"]["arms"]
                t4 = res["tiers"]["tier4_equity"]["arms"]
                print(
                    "self-test passed: "
                    f"tier3(naive_dm={t3['naive_cancel_heavy_side']['dm_metrics']['mean_dm']:.4f}, "
                    f"guard_dm={t3['outer_band_dm_guardian']['dm_metrics']['mean_dm']:.4f}, "
                    f"cascade_pnl=${t3['outer_band_dm_guardian_cascade_guard']['ex_subsidy_cash_pnl_usd']:.4f}) | "
                    f"tier4(naive_dm={t4['naive_cancel_heavy_side']['dm_metrics']['mean_dm']:.4f}, "
                    f"guard_dm={t4['outer_band_dm_guardian']['dm_metrics']['mean_dm']:.4f}, "
                    f"cascade_pnl=${t4['outer_band_dm_guardian_cascade_guard']['ex_subsidy_cash_pnl_usd']:.4f})"
                )
            return 0

        frames = load_tape_jsonl(args.tape) if args.tape is not None else None
        tape_source = str(args.tape) if args.tape is not None else None
        report = run_comparison(args.tier, frames=frames, tape_source=tape_source)
        if args.json:
            print(json.dumps(report, indent=2, allow_nan=False))
        else:
            _print_human_summary(report)
        return 0
    except Exception as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
