#!/usr/bin/env python3
"""Multi-venue Crypto + RWA Turnover, UTC Session-Gated & Full W01-W25 Regime Evaluator (stdlib-only top level).

Evaluates Track 03 (03_incentive_turnover) across BOTH:
  - Crypto: BTC, ETH, SOL
  - RWA:    AAPL, SPY, QQQ, NVDA, TSLA, DRAM, SNDK
across all continuous WS venues in the Aleph lakehouse:
  - lighter            (USDC, public_tier0_unverified, 0.0 / 0.0 bps)
  - lighter_robinhood  (USDG, public_tier0_unverified, 0.0 / 0.0 bps)
  - extended           (USDC, public_tier0_unverified, 0.0 / 2.50 bps, Crypto)
  - arcus              (USDC, public_tier0_schedule_only, RWA 0.0 / 2.25 bps, Crypto 1.50 / 4.50 bps)
  - bulk               (USDC, public_tier0_unverified, 0.0 / 2.00 bps, tagged `book_diff_diagnostic`)

Supports dynamic Hive campaign discovery across full Aleph W01-W25 history:
  - `full_dev`: Crypto `w01..w20 ex w08` (19w ~195h, includes `w03`), RWA `w01..w20` (15w ~154h), BULK `w01..w20` (20w)
  - `holdout`:  Crypto `w21..w25` (5w ~51h), RWA `w21, w23..w25` (4w ~41h), BULK `w21..w25` (5w)
  - `selection_dev`: `w18..w21`

Includes UTC Session-Gated Ablation (`all_hours` vs `quiet_sessions_only`: `overnight_asia + us_extended`
vs `us_rth` for RWA; causal trailing low-vol quartile Q25 gating for Crypto), window-level regime
stratification (`calm` / `normal` / `extreme_crash` with `w03` highlight), and tail-risk outputs
(`worst_10h_window`, `max_drawdown_usd`, `positive_window_ratio`, cumulative curve).

Reuses `compute_volume_farming_cost` from `eval_volume_farming_cost.py` with `rebate_bps = 0.0` default.
Strict stdlib-only module top level: `duckdb` is imported lazily inside the lakehouse loader only.
"""

from __future__ import annotations

import argparse
import bisect
import dataclasses
import gc
import glob
import hashlib
import json
import math
import os
import re
import sys
from collections import defaultdict, deque
from datetime import date, datetime, timedelta, timezone
from pathlib import Path
from typing import Any, Sequence

SCRIPT_DIR = Path(__file__).resolve().parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

from eval_volume_farming_cost import compute_volume_farming_cost  # noqa: E402

TEN_HOURS_MS = 36_000_000
MAX_CONTINUOUS_GAP_MS = 60_000

CRYPTO_SYMBOLS: tuple[str, ...] = ("BTC", "ETH", "SOL")
RWA_SYMBOLS: tuple[str, ...] = ("AAPL", "SPY", "QQQ", "NVDA", "TSLA", "DRAM", "SNDK")

UTC_SESSIONS: tuple[str, ...] = ("overnight_asia", "us_extended", "us_rth")
QUIET_RWA_SESSIONS: tuple[str, ...] = ("overnight_asia", "us_extended")
REGIMES: tuple[str, ...] = ("calm", "normal", "extreme_crash")

DEFAULT_REGIME_CUTOFFS: dict[str, tuple[float, float]] = {
    # (calm_max_vol_bps, extreme_min_vol_bps) fallback if no frozen full_dev thresholds provided
    "crypto": (0.85, 1.20),
    "rwa": (0.30, 0.60),
}

_CAMPAIGN_RE = re.compile(r"^tokyo-(crypto|rwa|bulk)-.+-w(\d+)$")

# Canonical fallback map for offline/test lookups; live runs use dynamic `discover_campaigns()`.
CANONICAL_WINDOWS: dict[str, dict[str, dict[str, dict[str, str]]]] = {
    "selection_dev": {
        "w18": {
            "crypto": {"campaign_id": "tokyo-crypto-20261122T200004Z-w18", "utc_date": "2026-11-22"},
            "rwa": {"campaign_id": "tokyo-rwa-20261122T195951Z-w18", "utc_date": "2026-11-22"},
            "bulk": {"campaign_id": "tokyo-bulk-20261122T195948Z-w18", "utc_date": "2026-11-22"},
        },
        "w19": {
            "crypto": {"campaign_id": "tokyo-crypto-20261123T061504Z-w19", "utc_date": "2026-11-23"},
            "rwa": {"campaign_id": "tokyo-rwa-20261123T061452Z-w19", "utc_date": "2026-11-23"},
            "bulk": {"campaign_id": "tokyo-bulk-20261123T061449Z-w19", "utc_date": "2026-11-23"},
        },
        "w20": {
            "crypto": {"campaign_id": "tokyo-crypto-20261123T163004Z-w20", "utc_date": "2026-11-23"},
            "rwa": {"campaign_id": "tokyo-rwa-20261123T162953Z-w20", "utc_date": "2026-11-23"},
            "bulk": {"campaign_id": "tokyo-bulk-20261123T162949Z-w20", "utc_date": "2026-11-23"},
        },
        "w21": {
            "crypto": {"campaign_id": "tokyo-crypto-20261124T024504Z-w21", "utc_date": "2026-11-24"},
            "rwa": {"campaign_id": "tokyo-rwa-20261124T024453Z-w21", "utc_date": "2026-11-24"},
            "bulk": {"campaign_id": "tokyo-bulk-20261124T024449Z-w21", "utc_date": "2026-11-24"},
        },
    },
    "holdout": {
        "w21": {
            "crypto": {"campaign_id": "tokyo-crypto-20261124T024504Z-w21", "utc_date": "2026-11-24"},
            "rwa": {"campaign_id": "tokyo-rwa-20261124T024453Z-w21", "utc_date": "2026-11-24"},
            "bulk": {"campaign_id": "tokyo-bulk-20261124T024449Z-w21", "utc_date": "2026-11-24"},
        },
        "w22": {
            "crypto": {"campaign_id": "tokyo-crypto-20261124T130005Z-w22", "utc_date": "2026-11-24"},
            "bulk": {"campaign_id": "tokyo-bulk-20261124T125949Z-w22", "utc_date": "2026-11-24"},
        },
        "w23": {
            "crypto": {"campaign_id": "tokyo-crypto-20261124T231506Z-w23", "utc_date": "2026-11-24"},
            "rwa": {"campaign_id": "tokyo-rwa-20261124T231453Z-w23", "utc_date": "2026-11-24"},
            "bulk": {"campaign_id": "tokyo-bulk-20261124T231449Z-w23", "utc_date": "2026-11-24"},
        },
        "w24": {
            "crypto": {"campaign_id": "tokyo-crypto-20261125T093007Z-w24", "utc_date": "2026-11-25"},
            "rwa": {"campaign_id": "tokyo-rwa-20261125T092953Z-w24", "utc_date": "2026-11-25"},
            "bulk": {"campaign_id": "tokyo-bulk-20261125T092949Z-w24", "utc_date": "2026-11-25"},
        },
        "w25": {
            "crypto": {"campaign_id": "tokyo-crypto-20261125T194507Z-w25", "utc_date": "2026-11-25"},
            "rwa": {"campaign_id": "tokyo-rwa-20261125T194453Z-w25", "utc_date": "2026-11-25"},
            "bulk": {"campaign_id": "tokyo-bulk-20261125T194449Z-w25", "utc_date": "2026-11-25"},
        },
    },
}

VENUE_SYMBOL_MAP: dict[str, dict[str, str]] = {
    "lighter": {
        "ETH": "0",
        "BTC": "1",
        "SOL": "2",
        "NVDA": "110",
        "TSLA": "112",
        "AAPL": "113",
        "SPY": "128",
        "QQQ": "129",
    },
    "lighter_robinhood": {
        "ETH": "0",
        "BTC": "1",
        "SOL": "3",
        "AAPL": "10",
        "NVDA": "15",
        "TSLA": "16",
        "QQQ": "25",
        "SPY": "26",
    },
    "extended": {
        "BTC": "BTC-USD",
        "ETH": "ETH-USD",
        "SOL": "SOL-USD",
    },
    "arcus": {
        "BTC": "BTC-USD",
        "ETH": "ETH-USD",
        "SOL": "SOL-USD",
        "AAPL": "AAPL-USD",
        "SPY": "SPY-USD",
        "QQQ": "QQQ-USD",
        "NVDA": "NVDA-USD",
        "TSLA": "TSLA-USD",
        "DRAM": "DRAM-USD",
        "SNDK": "SNDK-USD",
    },
    "bulk": {
        "BTC": "BTC-USD",
        "ETH": "ETH-USD",
        "SOL": "SOL-USD",
        "QQQ": "QQQ-USD",
        "NVDA": "NVDA-USD",
        "TSLA": "TSLA-USD",
        "SNDK": "SNDK-USD",
    },
    "binance": {
        "BTC": "BTCUSDT",
        "ETH": "ETHUSDT",
        "SOL": "SOLUSDT",
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
    "lighter": (200, 200, 220),
    "lighter_robinhood": (200, 200, 220),
    "extended": (40, 40, 45),
    "arcus": (36, 33, 40),
    "bulk": (40, 40, 45),
}

VENUE_METADATA: dict[str, dict[str, Any]] = {
    "lighter": {
        "settlement": "USDC",
        "account_fee_status": "public_tier0_unverified",
        "has_public_trades": True,
        "crypto_fees": (0.0, 0.0),
        "rwa_fees": (0.0, 0.0),
    },
    "lighter_robinhood": {
        "settlement": "USDG",
        "account_fee_status": "public_tier0_unverified",
        "has_public_trades": True,
        "crypto_fees": (0.0, 0.0),
        "rwa_fees": (0.0, 0.0),
    },
    "extended": {
        "settlement": "USDC",
        "account_fee_status": "public_tier0_unverified",
        "has_public_trades": True,
        "crypto_fees": (0.0, 2.50),
        "rwa_fees": (0.0, 2.50),
    },
    "arcus": {
        "settlement": "USDC",
        "account_fee_status": "public_tier0_schedule_only",
        "has_public_trades": True,
        "crypto_fees": (1.50, 4.50),
        "rwa_fees": (0.0, 2.25),
    },
    "bulk": {
        "settlement": "USDC",
        "account_fee_status": "public_tier0_unverified",
        "has_public_trades": False,
        "crypto_fees": (0.0, 2.00),
        "rwa_fees": (0.0, 2.00),
    },
}


@dataclasses.dataclass(frozen=True)
class TurnoverTarget:
    """Target `(venue, domain, symbol)` evaluation spec with explicit settlement & fee provenance."""

    venue: str
    domain: str
    symbol: str
    settlement: str
    maker_fee_bps: float
    taker_fee_bps: float
    account_fee_status: str
    has_public_trades: bool
    ref_venues: tuple[str, ...]


def build_default_targets(*, include_bulk_diagnostic: bool = True) -> list[TurnoverTarget]:
    """Build canonical Crypto + RWA multi-venue target matrix with explicit USDC vs USDG separation."""
    targets: list[TurnoverTarget] = []

    crypto_venues: tuple[str, ...] = ("lighter", "lighter_robinhood", "extended", "arcus")
    if include_bulk_diagnostic:
        crypto_venues = crypto_venues + ("bulk",)
    for venue in crypto_venues:
        meta = VENUE_METADATA[venue]
        maker_bp, taker_bp = meta["crypto_fees"]
        for sym in CRYPTO_SYMBOLS:
            if sym not in VENUE_SYMBOL_MAP[venue]:
                continue
            targets.append(
                TurnoverTarget(
                    venue=venue,
                    domain="crypto",
                    symbol=sym,
                    settlement=str(meta["settlement"]),
                    maker_fee_bps=float(maker_bp),
                    taker_fee_bps=float(taker_bp),
                    account_fee_status=str(meta["account_fee_status"]),
                    has_public_trades=bool(meta["has_public_trades"]),
                    ref_venues=("binance",),
                )
            )

    rwa_venues: tuple[str, ...] = ("lighter", "lighter_robinhood", "arcus")
    if include_bulk_diagnostic:
        rwa_venues = rwa_venues + ("bulk",)
    for venue in rwa_venues:
        meta = VENUE_METADATA[venue]
        maker_bp, taker_bp = meta["rwa_fees"]
        for sym in RWA_SYMBOLS:
            if sym not in VENUE_SYMBOL_MAP[venue]:
                continue
            if venue == "arcus":
                refs: tuple[str, ...] = (
                    ("trade_xyz",) if sym in ("DRAM", "SNDK") else ("lighter_robinhood", "lighter", "trade_xyz")
                )
            else:
                refs = ("arcus",)
            targets.append(
                TurnoverTarget(
                    venue=venue,
                    domain="rwa",
                    symbol=sym,
                    settlement=str(meta["settlement"]),
                    maker_fee_bps=float(maker_bp),
                    taker_fee_bps=float(taker_bp),
                    account_fee_status=str(meta["account_fee_status"]),
                    has_public_trades=bool(meta["has_public_trades"]),
                    ref_venues=refs,
                )
            )
    return targets


@dataclasses.dataclass(frozen=True)
class TurnoverArmConfig:
    """Frozen strategy + session-gate parameters for Track 03 multi-venue turnover ablation."""

    name: str
    ablation_group: str
    quote_usd: float = 200.0
    max_pos_usd: float = 400.0
    max_hold_ms: int = 15_000
    keep_queue_ticks: int = 0
    order_delay_ms: int = 200
    cancel_delay_ms: int = 200
    exit_delay_ms: int = 220
    queue_ahead_fraction: float = 0.5
    maker_fee_bps: float = 0.0
    taker_fee_bps: float = 0.0
    rebate_bps: float = 0.0
    use_as_skew: bool = False
    as_skew_bps: float = 2.0
    base_extra_bps: float = 0.1
    vol_spread_mult: float = 0.5
    passive_exit_priority: bool = True
    asymmetric_markout_skew: bool = False
    asymmetric_markout_max_bps: float = 3.0
    markout_shield_horizon_ms: int = 1_000
    markout_shield_adverse_threshold_bps: float = 1.0
    markout_shield_cooldown_ms: int = 2_000
    markout_shield_size_scale: float = 0.5
    high_vol_pause_sigma_bps: float | None = None
    use_oracle_anchor: bool = False
    oracle_basis_tau_ms: float = 120_000.0
    oracle_fair_weight: float = 0.5
    oracle_stale_suppress_bps: float = 2.5
    oracle_max_age_ms: int = 5_000
    quiet_regime_only: bool = False
    quiet_min_spread_vol_ratio: float = 0.80
    quiet_max_sigma_bps: float = 1.2
    quiet_sigma_floor_bps: float = 0.05
    allowed_utc_sessions: tuple[str, ...] | None = None
    causal_vol_quantile_max: float | None = None
    causal_vol_window_ms: int = 3_600_000
    causal_vol_min_samples: int = 60
    infer_book_diff_sweeps: bool = False


def build_ablation_arms(
    venue: str,
    domain: str,
    *,
    maker_fee_bps: float = 0.0,
    taker_fee_bps: float = 0.0,
    rebate_bps: float = 0.0,
    infer_book_diff_sweeps: bool = False,
    arm_names: Sequence[str] | None = None,
) -> list[TurnoverArmConfig]:
    """Construct the frozen Track 03 ablation suite for `(venue, domain)`."""
    if venue not in VENUE_LATENCY_MS:
        raise ValueError(f"Unsupported venue {venue!r}")
    o_ms, c_ms, e_ms = VENUE_LATENCY_MS[venue]
    common: dict[str, Any] = dict(
        quote_usd=200.0,
        max_pos_usd=400.0,
        max_hold_ms=15_000,
        order_delay_ms=o_ms,
        cancel_delay_ms=c_ms,
        exit_delay_ms=e_ms,
        queue_ahead_fraction=0.5,
        maker_fee_bps=maker_fee_bps,
        taker_fee_bps=taker_fee_bps,
        rebate_bps=rebate_bps,
        infer_book_diff_sweeps=infer_book_diff_sweeps,
    )
    g4_core: dict[str, Any] = dict(
        keep_queue_ticks=3,
        use_as_skew=True,
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
    )

    arms: list[TurnoverArmConfig] = [
        TurnoverArmConfig(
            name="S0_all_hours",
            ablation_group="all_hours",
            keep_queue_ticks=0,
            passive_exit_priority=False,
            **common,
        ),
        TurnoverArmConfig(
            name="G4_all_hours",
            ablation_group="all_hours",
            **g4_core,
            **common,
        ),
    ]

    if domain == "rwa":
        arms.extend(
            [
                TurnoverArmConfig(
                    name="S0_quiet_sessions_only",
                    ablation_group="quiet_sessions_only",
                    keep_queue_ticks=0,
                    passive_exit_priority=False,
                    allowed_utc_sessions=QUIET_RWA_SESSIONS,
                    **common,
                ),
                TurnoverArmConfig(
                    name="G4_quiet_sessions_only",
                    ablation_group="quiet_sessions_only",
                    allowed_utc_sessions=QUIET_RWA_SESSIONS,
                    **g4_core,
                    **common,
                ),
                TurnoverArmConfig(
                    name="G4_us_rth_only",
                    ablation_group="us_rth",
                    allowed_utc_sessions=("us_rth",),
                    **g4_core,
                    **common,
                ),
            ]
        )
    else:
        arms.extend(
            [
                TurnoverArmConfig(
                    name="S0_low_vol_q25",
                    ablation_group="quiet_sessions_only",
                    keep_queue_ticks=0,
                    passive_exit_priority=False,
                    causal_vol_quantile_max=0.25,
                    **common,
                ),
                TurnoverArmConfig(
                    name="G4_low_vol_q25",
                    ablation_group="quiet_sessions_only",
                    causal_vol_quantile_max=0.25,
                    **g4_core,
                    **common,
                ),
                TurnoverArmConfig(
                    name="G4_overnight_asia",
                    ablation_group="overnight_asia",
                    allowed_utc_sessions=("overnight_asia",),
                    **g4_core,
                    **common,
                ),
                TurnoverArmConfig(
                    name="G4_us_rth",
                    ablation_group="us_rth",
                    allowed_utc_sessions=("us_rth",),
                    **g4_core,
                    **common,
                ),
            ]
        )

    if arm_names is not None:
        wanted = set(arm_names)
        known = {a.name for a in arms}
        unknown = sorted(wanted - known)
        if unknown:
            raise ValueError(f"Unknown arm name(s): {unknown}; valid for {domain}: {sorted(known)}")
        arms = [a for a in arms if a.name in wanted]
    return arms


def classify_utc_session(ts_ms: int) -> str:
    """Classify epoch ms into canonical UTC trading session (`overnight_asia`, `us_extended`, `us_rth`)."""
    minute_of_day = (int(ts_ms) // 60_000) % 1_440
    if minute_of_day < 660:
        return "overnight_asia"
    if minute_of_day < 810 or minute_of_day >= 1_200:
        return "us_extended"
    return "us_rth"


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


@dataclasses.dataclass
class _RestingOrder:
    side: str
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
    sgn: float
    qty: float
    entry_px: float
    entry_mid: float
    entry_ts: int


@dataclasses.dataclass
class PreparedStream:
    """Pre-normalized flat tuple event stream reused across all ablation arms for speed."""

    # Tuple format:
    # - kind 0 (book):  (ts, 0, bb, bsz, ba, asz)
    # - kind 1 (ref):   (ts, 1, venue_str, bid, ask, 0.0)
    # - kind 2 (trade): (ts, 2, side_str, price, size, 0.0)
    tuples: list[tuple[int, int, Any, float, float, float]]
    mid_times: list[int]
    mid_vals: list[float]
    has_trade_events: bool
    has_ref_events: bool
    realized_1s_vol_bps: float
    p99_1s_abs_ret_bps: float
    median_spread_bps: float


def prepare_replay_stream(events: Sequence[dict[str, Any]]) -> PreparedStream:
    """Normalize and sort events once, extracting window-level realized 1s vol & spread stats."""
    tuples: list[tuple[int, int, Any, float, float, float]] = []
    has_trade = False
    has_ref = False

    for ev in events:
        kind = ev.get("kind")
        if kind == "book":
            if "bb" in ev and "ba" in ev:
                bb, bsz = float(ev["bb"]), max(float(ev["bsz"]), 1e-9)
                ba, asz = float(ev["ba"]), max(float(ev["asz"]), 1e-9)
            elif "bids" in ev and "asks" in ev:
                bb, bsz = float(ev["bids"][0][0]), max(float(ev["bids"][0][1]), 1e-9)
                ba, asz = float(ev["asks"][0][0]), max(float(ev["asks"][0][1]), 1e-9)
            else:
                bb, bsz = float(ev["bid_px"]), max(float(ev["bid_sz"]), 1e-9)
                ba, asz = float(ev["ask_px"]), max(float(ev["ask_sz"]), 1e-9)
            if bb <= 0.0 or ba <= bb or bsz <= 0.0 or asz <= 0.0:
                continue
            tuples.append((int(ev["ts"]), 0, bb, bsz, ba, asz))
        elif kind == "ref":
            rbid = float(ev["bid"])
            rask = float(ev["ask"])
            if rbid <= 0.0 or rask <= rbid:
                continue
            has_ref = True
            tuples.append((int(ev["ts"]), 1, str(ev.get("venue") or "ref"), rbid, rask, 0.0))
        elif kind == "trade":
            px = float(ev["price"])
            sz = float(ev["size"])
            if px <= 0.0 or sz <= 0.0:
                continue
            s_up = str(ev["side"]).upper()
            side_norm = "BUY" if s_up in ("BUY", "B", "BID") else "SELL"
            has_trade = True
            tuples.append((int(ev["ts"]), 2, side_norm, px, sz, 0.0))

    tuples.sort(key=lambda t: (t[0], t[1]))

    mid_times: list[int] = []
    mid_vals: list[float] = []
    spreads: list[float] = []
    ret_1s: list[float] = []
    last_1s_ts: int | None = None
    last_1s_mid = 0.0

    for item in tuples:
        if item[1] != 0:
            continue
        ts, _, bb, _, ba, _ = item
        mid = 0.5 * (bb + ba)
        mid_times.append(ts)
        mid_vals.append(mid)
        spreads.append((ba - bb) / mid * 1e4)
        if last_1s_ts is None:
            last_1s_ts = ts
            last_1s_mid = mid
        else:
            dt = ts - last_1s_ts
            if 1_000 <= dt <= 10_000 and last_1s_mid > 0.0:
                r_bp = math.log(mid / last_1s_mid) * 1e4 / math.sqrt(dt / 1000.0)
                if math.isfinite(r_bp):
                    ret_1s.append(r_bp)
                last_1s_ts = ts
                last_1s_mid = mid
            elif dt > 10_000:
                last_1s_ts = ts
                last_1s_mid = mid

    vol_1s = 0.0
    p99_ret = 0.0
    if len(ret_1s) >= 5:
        mean_r = sum(ret_1s) / len(ret_1s)
        vol_1s = math.sqrt(sum((x - mean_r) ** 2 for x in ret_1s) / (len(ret_1s) - 1))
        abs_sorted = sorted(abs(x) for x in ret_1s)
        p99_ret = abs_sorted[int(0.99 * (len(abs_sorted) - 1))]
    med_sp = sorted(spreads)[len(spreads) // 2] if spreads else 0.0

    return PreparedStream(
        tuples=tuples,
        mid_times=mid_times,
        mid_vals=mid_vals,
        has_trade_events=has_trade,
        has_ref_events=has_ref,
        realized_1s_vol_bps=round(vol_1s, 4),
        p99_1s_abs_ret_bps=round(p99_ret, 4),
        median_spread_bps=round(med_sp, 4),
    )


def simulate_prepared_arm(
    stream: PreparedStream,
    tick: float,
    arm: TurnoverArmConfig,
    *,
    markout_horizons_ms: tuple[int, ...] = (1_000, 5_000, 10_000),
) -> dict[str, Any]:
    """Run causal FIFO simulation on a `PreparedStream` with exact cash/fee decomposition."""
    if tick <= 0.0 or not math.isfinite(tick):
        raise ValueError(f"tick must be finite and > 0, got {tick}")

    tuples = stream.tuples
    mid_times = stream.mid_times
    mid_vals = stream.mid_vals
    has_trade_events = stream.has_trade_events
    has_ref_events = stream.has_ref_events

    if arm.infer_book_diff_sweeps and not has_trade_events:
        evidence_mode = "book_diff_diagnostic"
    elif has_trade_events:
        evidence_mode = "public_trades"
    else:
        evidence_mode = "no_trades_in_window"

    if not tuples or not mid_times:
        cost_bd = compute_volume_farming_cost(
            0.0, 0.0, arm.taker_fee_bps, maker_fee_bps=arm.maker_fee_bps, rebate_bps=arm.rebate_bps
        )
        return {
            "arm": arm.name,
            "ablation_group": arm.ablation_group,
            "fill_evidence_mode": evidence_mode,
            "ref_available": has_ref_events,
            "realized_1s_vol_bps": stream.realized_1s_vol_bps,
            "p99_1s_abs_ret_bps": stream.p99_1s_abs_ret_bps,
            "median_spread_bps": stream.median_spread_bps,
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
            "markout_1s_bps": None,
            "markout_5s_bps": None,
            "markout_10s_bps": None,
            "markout_1s_den": 0.0,
            "markout_5s_den": 0.0,
            "markout_10s_den": 0.0,
            "net_per_ntl_bp": 0.0,
            "fills": 0,
            "maker_fills": 0,
            "taker_exits": 0,
            "orders_placed": 0,
            "alo_rejected": 0,
            "session_gated_ticks": 0,
            "vol_quantile_gated_ticks": 0,
            "total_book_ticks": 0,
            "terminal_flat": True,
            "_exact_cash_ex_fee_usd": 0.0,
            "_exact_gross_usd": 0.0,
            "_exact_fee_usd": 0.0,
            "_exact_net_usd": 0.0,
        }

    bisect_right = bisect.bisect_right
    last_mid_ts = mid_times[-1]

    def mid_at(ts_ms: int) -> float | None:
        if ts_ms > last_mid_ts:
            return None
        idx = bisect_right(mid_times, ts_ms) - 1
        return mid_vals[idx] if idx >= 0 else None

    cur_ts = 0
    cur_bb = 0.0
    cur_bsz = 0.0
    cur_ba = 0.0
    cur_asz = 0.0
    has_book = False
    prev_ts = 0
    prev_bb = 0.0
    prev_bsz = 0.0
    prev_ba = 0.0
    prev_asz = 0.0
    has_prev_book = False

    ref_books: dict[str, tuple[int, float]] = {}
    ewma_basis: float | None = None
    last_basis_ts: int | None = None

    bid_ord: _RestingOrder | None = None
    ask_ord: _RestingOrder | None = None
    open_lots: deque[_OpenLot] = deque()
    pos_qty = 0.0

    pending_markouts: deque[tuple[int, str, float]] = deque()
    markout_ewma_bid = 0.0
    markout_ewma_ask = 0.0
    markout_cd_bid = 0
    markout_cd_ask = 0

    last_vol_ts = -1
    last_vol_mid = 0.0
    var_bps2_per_s = 0.0

    vol_ring: deque[tuple[int, float]] = deque()
    vol_sorted: list[float] = []
    last_vol_sample_ts = -1

    pending_ioc_exits: list[tuple[int, str, float]] = []

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
    maker_fills = 0
    taker_exits = 0
    session_gated_ticks = 0
    vol_quantile_gated_ticks = 0
    total_book_ticks = 0
    maker_fills_log: list[tuple[int, str, float, float]] = []  # (ts, side, px, notl)

    tol_tick_25 = tick * 0.25
    tol_tick_10 = tick * 0.10
    allowed_sessions_set = set(arm.allowed_utc_sessions) if arm.allowed_utc_sessions is not None else None
    causal_vq = arm.causal_vol_quantile_max
    causal_vw = arm.causal_vol_window_ms
    causal_vmin = arm.causal_vol_min_samples

    def record_fill(ts_ms: int, fill_side: str, fill_px: float, fill_qty: float, role: str, mid_now: float) -> None:
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
            mside = "BID" if fill_side == "BUY" else "ASK"
            maker_fills_log.append((ts_ms, mside, fill_px, notl))
            pending_markouts.append((ts_ms + arm.markout_shield_horizon_ms, mside, fill_px))
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
                _OpenLot(sgn=fill_sgn, qty=rem, entry_px=fill_px, entry_mid=mid_now, entry_ts=ts_ms)
            )
        if not open_lots:
            pos_qty = 0.0

    def activate_arrivals(now_ms: int) -> None:
        nonlocal bid_ord, ask_ord, alo_rejected
        if not has_book:
            return
        if bid_ord is not None and not bid_ord.activated and now_ms >= bid_ord.active_at_ms:
            if bid_ord.price >= cur_ba - 1e-12:
                bid_ord = None
                alo_rejected += 1
            else:
                if abs(bid_ord.price - cur_bb) <= tol_tick_25 or bid_ord.price <= cur_bb + tol_tick_25:
                    bid_ord.queue_ahead = arm.queue_ahead_fraction * cur_bsz
                else:
                    bid_ord.queue_ahead = 0.0
                bid_ord.activated = True
        if ask_ord is not None and not ask_ord.activated and now_ms >= ask_ord.active_at_ms:
            if ask_ord.price <= cur_bb + 1e-12:
                ask_ord = None
                alo_rejected += 1
            else:
                if abs(ask_ord.price - cur_ba) <= tol_tick_25 or ask_ord.price >= cur_ba - tol_tick_25:
                    ask_ord.queue_ahead = arm.queue_ahead_fraction * cur_asz
                else:
                    ask_ord.queue_ahead = 0.0
                ask_ord.activated = True

    def handle_trade(ts_ms: int, tr_side: str, tr_px: float, tr_sz: float) -> None:
        nonlocal bid_ord, ask_ord
        if not has_book:
            return
        if tr_side == "BUY":
            o = ask_ord
            if o is None or not o.activated:
                return
            filled_qty = 0.0
            if tr_px > o.price + tol_tick_10:
                filled_qty = min(o.remaining, tr_sz)
            elif abs(tr_px - o.price) <= tol_tick_10:
                if tr_sz > o.queue_ahead:
                    overflow = tr_sz - o.queue_ahead
                    o.queue_ahead = 0.0
                    filled_qty = min(o.remaining, overflow)
                else:
                    o.queue_ahead -= tr_sz
            if filled_qty > 1e-12:
                record_fill(ts_ms, "SELL", o.price, filled_qty, "maker", 0.5 * (cur_bb + cur_ba))
                o.remaining -= filled_qty
                if o.remaining <= 1e-12:
                    ask_ord = None
        else:
            o = bid_ord
            if o is None or not o.activated:
                return
            filled_qty = 0.0
            if tr_px < o.price - tol_tick_10:
                filled_qty = min(o.remaining, tr_sz)
            elif abs(tr_px - o.price) <= tol_tick_10:
                if tr_sz > o.queue_ahead:
                    overflow = tr_sz - o.queue_ahead
                    o.queue_ahead = 0.0
                    filled_qty = min(o.remaining, overflow)
                else:
                    o.queue_ahead -= tr_sz
            if filled_qty > 1e-12:
                record_fill(ts_ms, "BUY", o.price, filled_qty, "maker", 0.5 * (cur_bb + cur_ba))
                o.remaining -= filled_qty
                if o.remaining <= 1e-12:
                    bid_ord = None

    def decide(now_ms: int, sigma_bps: float, count_gate_tick: bool) -> None:
        nonlocal ewma_basis, last_basis_ts, bid_ord, ask_ord, can, orders_placed, qpres
        nonlocal session_gated_ticks, vol_quantile_gated_ticks
        if not has_book:
            return
        if bid_ord is not None and bid_ord.cancel_effective_ms is not None and now_ms >= bid_ord.cancel_effective_ms:
            bid_ord = None
        if ask_ord is not None and ask_ord.cancel_effective_ms is not None and now_ms >= ask_ord.cancel_effective_ms:
            ask_ord = None

        bb = cur_bb
        ba = cur_ba
        venue_mid = 0.5 * (bb + ba)
        spread_bps = (ba - bb) / venue_mid * 1e4
        sp_ticks = max(1, round((ba - bb) / tick))

        fair = venue_mid
        stale_suppress_side: str | None = None
        if arm.use_oracle_anchor:
            fresh_refs = [m for t, m in ref_books.values() if now_ms - t <= arm.oracle_max_age_ms]
            if fresh_refs:
                ref_mid = fresh_refs[0] if len(fresh_refs) == 1 else sorted(fresh_refs)[len(fresh_refs) // 2]
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
                        stale_suppress_side = "BID"
                    elif div_bps < -arm.oracle_stale_suppress_bps:
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

        high_vol_paused = arm.high_vol_pause_sigma_bps is not None and sigma_bps > arm.high_vol_pause_sigma_bps
        quiet_blocked = False
        if arm.quiet_regime_only:
            ratio = spread_bps / max(sigma_bps, arm.quiet_sigma_floor_bps)
            quiet_blocked = (ratio < arm.quiet_min_spread_vol_ratio) or (sigma_bps > arm.quiet_max_sigma_bps)

        sess_blocked = (
            allowed_sessions_set is not None
            and classify_utc_session(now_ms) not in allowed_sessions_set
        )
        vq_blocked = False
        if causal_vq is not None:
            if len(vol_sorted) < causal_vmin:
                vq_blocked = True
            else:
                q_idx = min(len(vol_sorted) - 1, max(0, int(causal_vq * (len(vol_sorted) - 1))))
                vq_blocked = sigma_bps > vol_sorted[q_idx]

        if count_gate_tick:
            if sess_blocked:
                session_gated_ticks += 1
            if vq_blocked:
                vol_quantile_gated_ticks += 1

        # BID
        is_red_bid = pos_usd < -10.0
        if not is_red_bid and (
            pos_usd >= arm.max_pos_usd
            or now_ms < markout_cd_bid
            or high_vol_paused
            or quiet_blocked
            or sess_blocked
            or vq_blocked
            or stale_suppress_side == "BID"
        ):
            if bid_ord is not None and bid_ord.cancel_effective_ms is None:
                bid_ord.cancel_effective_ms = now_ms + arm.cancel_delay_ms
                can += 1
        else:
            if is_red_bid and arm.passive_exit_priority:
                raw_b = min(bb + tick, ba - tick) if sp_ticks >= 2 else bb
            elif arm.use_as_skew:
                asym_b = min(arm.asymmetric_markout_max_bps, -markout_ewma_bid) if (arm.asymmetric_markout_skew and markout_ewma_bid < 0.0) else 0.0
                raw_b = min(bb, r_px * (1.0 - (half_extra_bps + asym_b) * 1e-4))
            else:
                raw_b = bb
            px_b = _round_tick(raw_b, tick, up=False)
            if px_b >= ba - 1e-12:
                px_b = _round_tick(ba - tick, tick, up=False)
            if px_b >= ba - 1e-12 or px_b <= 0.0:
                if bid_ord is not None and bid_ord.cancel_effective_ms is None:
                    bid_ord.cancel_effective_ms = now_ms + arm.cancel_delay_ms
                    can += 1
            elif bid_ord is not None:
                if bid_ord.cancel_effective_ms is None:
                    ord_cross = bid_ord.price >= ba - 1e-12
                    tdiff = round(abs(bid_ord.price - px_b) / tick)
                    tcdiff = round(abs(bid_ord.price - bb) / tick)
                    if not ord_cross and (tdiff <= arm.keep_queue_ticks or (arm.keep_queue_ticks > 0 and tcdiff <= arm.keep_queue_ticks)):
                        qpres += 1
                    else:
                        bid_ord.cancel_effective_ms = now_ms + arm.cancel_delay_ms
                        can += 1
            else:
                sc = (
                    arm.markout_shield_size_scale
                    if (not is_red_bid and arm.markout_shield_adverse_threshold_bps > 0.0 and markout_ewma_bid < -arm.markout_shield_adverse_threshold_bps)
                    else 1.0
                )
                ousd = max(arm.quote_usd, abs(pos_usd)) if is_red_bid else arm.quote_usd * sc
                q_b = ousd / px_b
                bid_ord = _RestingOrder("BID", px_b, q_b, q_b, 0.0, now_ms, now_ms + arm.order_delay_ms, False)
                orders_placed += 1

        # ASK
        is_red_ask = pos_usd > 10.0
        if not is_red_ask and (
            pos_usd <= -arm.max_pos_usd
            or now_ms < markout_cd_ask
            or high_vol_paused
            or quiet_blocked
            or sess_blocked
            or vq_blocked
            or stale_suppress_side == "ASK"
        ):
            if ask_ord is not None and ask_ord.cancel_effective_ms is None:
                ask_ord.cancel_effective_ms = now_ms + arm.cancel_delay_ms
                can += 1
        else:
            if is_red_ask and arm.passive_exit_priority:
                raw_a = max(ba - tick, bb + tick) if sp_ticks >= 2 else ba
            elif arm.use_as_skew:
                asym_a = min(arm.asymmetric_markout_max_bps, -markout_ewma_ask) if (arm.asymmetric_markout_skew and markout_ewma_ask < 0.0) else 0.0
                raw_a = max(ba, r_px * (1.0 + (half_extra_bps + asym_a) * 1e-4))
            else:
                raw_a = ba
            px_a = _round_tick(raw_a, tick, up=True)
            if px_a <= bb + 1e-12:
                px_a = _round_tick(bb + tick, tick, up=True)
            if px_a <= bb + 1e-12 or px_a <= 0.0:
                if ask_ord is not None and ask_ord.cancel_effective_ms is None:
                    ask_ord.cancel_effective_ms = now_ms + arm.cancel_delay_ms
                    can += 1
            elif ask_ord is not None:
                if ask_ord.cancel_effective_ms is None:
                    ord_cross = ask_ord.price <= bb + 1e-12
                    tdiff = round(abs(ask_ord.price - px_a) / tick)
                    tcdiff = round(abs(ask_ord.price - ba) / tick)
                    if not ord_cross and (tdiff <= arm.keep_queue_ticks or (arm.keep_queue_ticks > 0 and tcdiff <= arm.keep_queue_ticks)):
                        qpres += 1
                    else:
                        ask_ord.cancel_effective_ms = now_ms + arm.cancel_delay_ms
                        can += 1
            else:
                sc = (
                    arm.markout_shield_size_scale
                    if (not is_red_ask and arm.markout_shield_adverse_threshold_bps > 0.0 and markout_ewma_ask < -arm.markout_shield_adverse_threshold_bps)
                    else 1.0
                )
                ousd = max(arm.quote_usd, abs(pos_usd)) if is_red_ask else arm.quote_usd * sc
                q_a = ousd / px_a
                ask_ord = _RestingOrder("ASK", px_a, q_a, q_a, 0.0, now_ms, now_ms + arm.order_delay_ms, False)
                orders_placed += 1

    last_sigma_bps = 0.0
    mk_thresh = arm.markout_shield_adverse_threshold_bps
    mk_cd_ms = arm.markout_shield_cooldown_ms
    max_hold_ms = arm.max_hold_ms
    exit_delay_ms = arm.exit_delay_ms
    infer_bd = arm.infer_book_diff_sweeps and not has_trade_events

    for ts, kind, c2, c3, c4, c5 in tuples:
        if bid_ord is not None and bid_ord.cancel_effective_ms is not None and ts >= bid_ord.cancel_effective_ms:
            bid_ord = None
        if ask_ord is not None and ask_ord.cancel_effective_ms is not None and ts >= ask_ord.cancel_effective_ms:
            ask_ord = None

        if pending_ioc_exits and has_book:
            mid_now_ioc = 0.5 * (cur_bb + cur_ba)
            still: list[tuple[int, str, float]] = []
            for exec_ts, iside, iqty in pending_ioc_exits:
                if ts < exec_ts:
                    still.append((exec_ts, iside, iqty))
                elif iside == "SELL" and pos_qty > 1e-12:
                    record_fill(ts, "SELL", cur_bb, min(iqty, pos_qty), "taker", mid_now_ioc)
                    haz += 1
                elif iside == "BUY" and pos_qty < -1e-12:
                    record_fill(ts, "BUY", cur_ba, min(iqty, -pos_qty), "taker", mid_now_ioc)
                    haz += 1
            pending_ioc_exits = still

        if open_lots and has_book and (ts - open_lots[0].entry_ts) >= max_hold_ms:
            tside = "SELL" if pos_qty > 1e-12 else ("BUY" if pos_qty < -1e-12 else None)
            if tside is not None:
                pend_q = sum(q for _, s, q in pending_ioc_exits if s == tside)
                needed = abs(pos_qty) - pend_q
                if needed > 1e-12:
                    pending_ioc_exits.append((ts + exit_delay_ms, tside, needed))

        if kind == 0:
            prev_ts, prev_bb, prev_bsz, prev_ba, prev_asz = cur_ts, cur_bb, cur_bsz, cur_ba, cur_asz
            has_prev_book = has_book
            cur_ts, cur_bb, cur_bsz, cur_ba, cur_asz = ts, c2, c3, c4, c5
            has_book = True
            total_book_ticks += 1
            activate_arrivals(ts)
            if bid_ord is not None and bid_ord.activated and abs(bid_ord.price - cur_bb) <= tol_tick_25 and cur_bsz < bid_ord.queue_ahead:
                bid_ord.queue_ahead = cur_bsz
            if ask_ord is not None and ask_ord.activated and abs(ask_ord.price - cur_ba) <= tol_tick_25 and cur_asz < ask_ord.queue_ahead:
                ask_ord.queue_ahead = cur_asz

            if infer_bd and has_prev_book and 0 < (ts - prev_ts) <= 2_000:
                if cur_bb > prev_ba + tol_tick_10:
                    handle_trade(ts, "BUY", cur_ba, max(prev_asz * 0.35, 1e-6))
                elif cur_ba < prev_bb - tol_tick_10:
                    handle_trade(ts, "SELL", cur_bb, max(prev_bsz * 0.35, 1e-6))
                else:
                    ask_drop = prev_asz if cur_ba > prev_ba + tol_tick_10 else (max(0.0, prev_asz - cur_asz) if abs(cur_ba - prev_ba) <= tol_tick_10 else 0.0)
                    bid_drop = prev_bsz if cur_bb < prev_bb - tol_tick_10 else (max(0.0, prev_bsz - cur_bsz) if abs(cur_bb - prev_bb) <= tol_tick_10 else 0.0)
                    if ask_drop > 0.0 and ask_drop >= 1.5 * bid_drop and (ask_drop * 0.35) * prev_ba >= 50.0:
                        handle_trade(ts, "BUY", prev_ba if abs(cur_ba - prev_ba) <= tol_tick_10 else cur_ba, ask_drop * 0.35)
                    elif bid_drop > 0.0 and bid_drop >= 1.5 * ask_drop and (bid_drop * 0.35) * prev_bb >= 50.0:
                        handle_trade(ts, "SELL", prev_bb if abs(cur_bb - prev_bb) <= tol_tick_10 else cur_bb, bid_drop * 0.35)

            mid_now = 0.5 * (cur_bb + cur_ba)
            if last_vol_ts >= 0 and ts > last_vol_ts and last_vol_mid > 0.0 and mid_now > 0.0:
                dt_s = (ts - last_vol_ts) / 1000.0
                if 0.0 < dt_s <= 10.0:
                    ret_bps = math.log(mid_now / last_vol_mid) * 1e4
                    inst_var = min(400.0, (ret_bps * ret_bps) / dt_s)
                    alpha = 1.0 - math.exp(-dt_s / 15.0)
                    var_bps2_per_s = (1.0 - alpha) * var_bps2_per_s + alpha * inst_var
            last_vol_ts = ts
            last_vol_mid = mid_now
            last_sigma_bps = math.sqrt(var_bps2_per_s)
            if causal_vq is not None and (last_vol_sample_ts < 0 or (ts - last_vol_sample_ts) >= 5_000):
                cutoff = ts - causal_vw
                while vol_ring and vol_ring[0][0] < cutoff:
                    _, old_s = vol_ring.popleft()
                    idx = bisect.bisect_left(vol_sorted, old_s)
                    if idx < len(vol_sorted):
                        vol_sorted.pop(idx)
                vol_ring.append((ts, last_sigma_bps))
                bisect.insort(vol_sorted, last_sigma_bps)
                last_vol_sample_ts = ts

            if mid_now > 0.0 and pending_markouts:
                while pending_markouts and ts >= pending_markouts[0][0]:
                    _, fside, fpx = pending_markouts.popleft()
                    if fpx <= 0.0:
                        continue
                    if fside == "BID":
                        mk_bps = (mid_now - fpx) / fpx * 1e4
                        markout_ewma_bid = 0.75 * markout_ewma_bid + 0.25 * mk_bps
                        if mk_thresh > 0.0 and markout_ewma_bid < -2.0 * mk_thresh and mk_cd_ms > 0:
                            markout_cd_bid = max(markout_cd_bid, ts + mk_cd_ms)
                    else:
                        mk_bps = -(mid_now - fpx) / fpx * 1e4
                        markout_ewma_ask = 0.75 * markout_ewma_ask + 0.25 * mk_bps
                        if mk_thresh > 0.0 and markout_ewma_ask < -2.0 * mk_thresh and mk_cd_ms > 0:
                            markout_cd_ask = max(markout_cd_ask, ts + mk_cd_ms)

            decide(ts, last_sigma_bps, True)

        elif kind == 1:
            activate_arrivals(ts)
            ref_books[c2] = (ts, 0.5 * (c3 + c4))
            if arm.use_oracle_anchor and has_book:
                decide(ts, last_sigma_bps, False)

        else:
            if not has_book:
                continue
            activate_arrivals(ts)
            handle_trade(ts, c2, c3, c4)
            if open_lots and (ts - open_lots[0].entry_ts) >= max_hold_ms:
                tside = "SELL" if pos_qty > 1e-12 else ("BUY" if pos_qty < -1e-12 else None)
                if tside is not None:
                    pend_q = sum(q for _, s, q in pending_ioc_exits if s == tside)
                    needed = abs(pos_qty) - pend_q
                    if needed > 1e-12:
                        pending_ioc_exits.append((ts + exit_delay_ms, tside, needed))

    last_ts = tuples[-1][0]
    if has_book:
        mid_end = 0.5 * (cur_bb + cur_ba)
        if pos_qty > 1e-12:
            record_fill(last_ts, "SELL", cur_bb, pos_qty, "taker", mid_end)
            haz += 1
        elif pos_qty < -1e-12:
            record_fill(last_ts, "BUY", cur_ba, -pos_qty, "taker", mid_end)
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
    for h_ms in markout_horizons_ms:
        num = 0.0
        den = 0.0
        for fl_ts, fl_side, fl_px, fl_notl in maker_fills_log:
            fm = mid_at(fl_ts + h_ms)
            if fm is None:
                continue
            sgn = 1.0 if fl_side == "BID" else -1.0
            mk_bps = sgn * (fm - fl_px) / fl_px * 1e4
            num += fl_notl * mk_bps
            den += fl_notl
        key = f"{h_ms // 1000}s"
        markout_bps[key] = round(num / den, 6) if den > 0.0 else None
        markout_dens[key] = den

    return {
        "arm": arm.name,
        "ablation_group": arm.ablation_group,
        "fill_evidence_mode": evidence_mode,
        "ref_available": has_ref_events,
        "realized_1s_vol_bps": stream.realized_1s_vol_bps,
        "p99_1s_abs_ret_bps": stream.p99_1s_abs_ret_bps,
        "median_spread_bps": stream.median_spread_bps,
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
        "markout_1s_bps": markout_bps.get("1s"),
        "markout_5s_bps": markout_bps.get("5s"),
        "markout_10s_bps": markout_bps.get("10s"),
        "markout_1s_den": round(markout_dens.get("1s", 0.0), 6),
        "markout_5s_den": round(markout_dens.get("5s", 0.0), 6),
        "markout_10s_den": round(markout_dens.get("10s", 0.0), 6),
        "net_per_ntl_bp": round(net_per_ntl_bp, 6),
        "fills": maker_fills + taker_exits,
        "maker_fills": maker_fills,
        "taker_exits": taker_exits,
        "orders_placed": orders_placed,
        "alo_rejected": alo_rejected,
        "session_gated_ticks": session_gated_ticks,
        "vol_quantile_gated_ticks": vol_quantile_gated_ticks,
        "total_book_ticks": total_book_ticks,
        "terminal_flat": terminal_flat,
        "_exact_cash_ex_fee_usd": cash_ex_fee_usd,
        "_exact_gross_usd": gross_usd,
        "_exact_fee_usd": fee_usd,
        "_exact_net_usd": net_usd,
    }


def simulate_turnover_arm(
    events: Sequence[dict[str, Any]],
    tick: float,
    arm: TurnoverArmConfig,
    *,
    markout_horizons_ms: tuple[int, ...] = (1_000, 5_000, 10_000),
) -> dict[str, Any]:
    """Convenience wrapper that prepares `events` and simulates `arm`."""
    return simulate_prepared_arm(prepare_replay_stream(events), tick, arm, markout_horizons_ms=markout_horizons_ms)


def _window_sort_key(win_id: str) -> tuple[int, str]:
    m = re.search(r"(\d+)$", win_id)
    return (int(m.group(1)) if m else 9999, win_id)


def discover_campaigns(
    lakehouse_dir: Path,
    split: str,
    window_ids: Sequence[str] | None = None,
) -> dict[str, dict[str, dict[str, str]]]:
    """Dynamically discover `tokyo-{crypto,rwa,bulk}-*-w*` Hive campaigns across W01-W25."""
    root = lakehouse_dir / "parquet" / "bbo_quotes" / "collector_region=tokyo"
    discovered: dict[str, dict[str, dict[str, Any]]] = defaultdict(dict)
    for d_str in glob.glob(str(root / "venue=*" / "utc_date=*" / "campaign_id=tokyo-*")):
        p = Path(d_str)
        cid = p.name.split("campaign_id=", 1)[-1]
        m = _CAMPAIGN_RE.match(cid)
        if not m:
            continue
        stream_kind, win_num_str = m.group(1), m.group(2)
        win_num = int(win_num_str)
        win_id = f"w{win_num:02d}"
        utc_date = p.parent.name.split("utc_date=", 1)[-1]
        entry = discovered[win_id].get(stream_kind)
        if entry is None or utc_date < entry["utc_date"]:
            discovered[win_id][stream_kind] = {"campaign_id": cid, "utc_date": utc_date, "win_num": win_num}

    def _keep_win(win_id: str, stream_kind: str, win_num: int) -> bool:
        if window_ids:
            return win_id in set(window_ids)
        if split == "full_dev":
            return 1 <= win_num <= 20
        if split == "holdout":
            return 21 <= win_num <= 25
        if split == "selection_dev":
            return 18 <= win_num <= 21
        if split == "all":
            return 1 <= win_num <= 25
        raise ValueError(f"Unsupported split {split!r}")

    out: dict[str, dict[str, dict[str, str]]] = {}
    for win_id in sorted(discovered.keys(), key=_window_sort_key):
        kinds_out: dict[str, dict[str, str]] = {}
        for skind, info in discovered[win_id].items():
            if _keep_win(win_id, skind, int(info["win_num"])):
                kinds_out[skind] = {"campaign_id": info["campaign_id"], "utc_date": info["utc_date"]}
        if kinds_out:
            out[win_id] = kinds_out
    return out


def _campaign_parquet_files(
    lakehouse_dir: Path,
    dataset: str,
    venue: str,
    utc_date: str,
    campaign_id: str,
) -> list[str]:
    """Glob Hive parquet files across `utc_date` and `utc_date + 1 day` for cross-midnight runs."""
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
    return sorted(set(files))


def compute_regime_thresholds_from_rows(rows: Sequence[dict[str, Any]]) -> dict[str, tuple[float, float]]:
    """Derive frozen `(calm_max_vol_bps, extreme_min_vol_bps)` per `domain|symbol` from dev rows."""
    vols_by_key: dict[str, list[float]] = defaultdict(list)
    for r in rows:
        if r.get("skipped"):
            continue
        v = r.get("realized_1s_vol_bps")
        if v is not None and float(v) > 0.0:
            vols_by_key[f"{r['domain']}|{r['symbol']}"].append(float(v))
    out: dict[str, tuple[float, float]] = {}
    for key, vals in sorted(vols_by_key.items()):
        s = sorted(vals)
        calm_cut = s[int(0.33 * (len(s) - 1))]
        ext_cut = s[int(0.80 * (len(s) - 1))]
        if ext_cut <= calm_cut:
            ext_cut = calm_cut * 1.25 + 0.05
        out[key] = (round(calm_cut, 4), round(ext_cut, 4))
    return out


def classify_window_regime(
    domain: str,
    symbol: str,
    win_id: str,
    realized_1s_vol_bps: float,
    regime_thresholds: dict[str, tuple[float, float]] | None = None,
) -> str:
    """Classify a 10h window into `calm`, `normal`, or `extreme_crash` (highlighting `w03`)."""
    if win_id == "w03" and domain == "crypto":
        return "extreme_crash"
    key = f"{domain}|{symbol}"
    calm_max, ext_min = (
        regime_thresholds[key]
        if (regime_thresholds and key in regime_thresholds)
        else DEFAULT_REGIME_CUTOFFS.get(domain, (0.85, 1.20))
    )
    if realized_1s_vol_bps >= ext_min:
        return "extreme_crash"
    if realized_1s_vol_bps <= calm_max:
        return "calm"
    return "normal"


def annotate_and_aggregate_turnover(
    rows: Sequence[dict[str, Any]],
    *,
    regime_thresholds: dict[str, tuple[float, float]] | None = None,
) -> dict[str, Any]:
    """Classify window regimes and compute tail-risk, cumulative curves, and domain-separated summaries."""
    active_rows = [r for r in rows if not r.get("skipped")]
    for r in active_rows:
        if "regime" not in r:
            r["regime"] = classify_window_regime(
                str(r["domain"]),
                str(r["symbol"]),
                str(r["win"]),
                float(r.get("realized_1s_vol_bps") or 0.0),
                regime_thresholds,
            )

    def _agg_bucket(grp: Sequence[dict[str, Any]], *, include_symbol: bool) -> dict[str, Any]:
        first = grp[0]
        spr = sum(float(r["spr"]) for r in grp)
        drft = sum(float(r["drft"]) for r in grp)
        pex = sum(float(r["pex"]) for r in grp)
        tslp = sum(float(r["tslp"]) for r in grp)
        gross_usd = spr + drft + pex + tslp
        fee_usd = sum(float(r["fee_usd"]) for r in grp)
        net_usd = gross_usd - fee_usd
        ntl = sum(float(r["ntl"]) for r in grp)
        maker_vol = sum(float(r["maker_volume_usd"]) for r in grp)
        taker_vol = sum(float(r["taker_volume_usd"]) for r in grp)
        gross_bp = (gross_usd / ntl) * 1e4 if ntl > 0.0 else 0.0
        net_bp = (net_usd / ntl) * 1e4 if ntl > 0.0 else 0.0
        s_share = min(1.0, max(0.0, taker_vol / ntl)) if ntl > 0.0 else 0.0

        cost_bd = compute_volume_farming_cost(
            gross_bp,
            s_share,
            float(first["taker_fee_bps"]),
            maker_fee_bps=float(first["maker_fee_bps"]),
            rebate_bps=float(first["rebate_bps"]),
        )

        # Chronological per-window curve + drawdown + worst window
        by_win: dict[str, list[dict[str, Any]]] = defaultdict(list)
        for r in grp:
            by_win[str(r["win"])].append(r)

        cum_net = 0.0
        cum_ntl = 0.0
        peak_net = 0.0
        max_dd_usd = 0.0
        pos_wins = 0
        total_hours = 0.0
        curve: list[dict[str, Any]] = []
        worst_win: dict[str, Any] | None = None

        for win_id in sorted(by_win.keys(), key=_window_sort_key):
            wgrp = by_win[win_id]
            w_spr = sum(float(x["spr"]) for x in wgrp)
            w_drft = sum(float(x["drft"]) for x in wgrp)
            w_pex = sum(float(x["pex"]) for x in wgrp)
            w_tslp = sum(float(x["tslp"]) for x in wgrp)
            w_gross = w_spr + w_drft + w_pex + w_tslp
            w_fee = sum(float(x["fee_usd"]) for x in wgrp)
            w_net = w_gross - w_fee
            w_ntl = sum(float(x["ntl"]) for x in wgrp)
            w_tvol = sum(float(x["taker_volume_usd"]) for x in wgrp)
            w_gross_bp = (w_gross / w_ntl) * 1e4 if w_ntl > 0.0 else 0.0
            w_s = min(1.0, max(0.0, w_tvol / w_ntl)) if w_ntl > 0.0 else 0.0
            w_cost = compute_volume_farming_cost(
                w_gross_bp,
                w_s,
                float(first["taker_fee_bps"]),
                maker_fee_bps=float(first["maker_fee_bps"]),
                rebate_bps=float(first["rebate_bps"]),
            )
            w_hrs = max(float((x.get("continuity") or {}).get("hours", 0.0)) for x in wgrp)
            total_hours += w_hrs
            w_vol = sum(float(x.get("realized_1s_vol_bps") or 0.0) for x in wgrp) / len(wgrp)
            w_regimes = [str(x.get("regime", "normal")) for x in wgrp]
            w_reg = "extreme_crash" if "extreme_crash" in w_regimes else ("normal" if "normal" in w_regimes else "calm")

            cum_net += w_net
            cum_ntl += w_ntl
            if cum_net > peak_net:
                peak_net = cum_net
            dd = peak_net - cum_net
            if dd > max_dd_usd:
                max_dd_usd = dd
            if w_net > 0.0:
                pos_wins += 1

            pt = {
                "win": win_id,
                "hours": round(w_hrs, 3),
                "regime": w_reg,
                "realized_1s_vol_bps": round(w_vol, 4),
                "net_usd": round(w_net, 4),
                "cum_net_usd": round(cum_net, 4),
                "ntl": round(w_ntl, 2),
                "cum_ntl": round(cum_ntl, 2),
                "double_sided_usd_per_m": w_cost.double_sided_usd_per_m,
            }
            curve.append(pt)
            if worst_win is None or w_net < worst_win["net_usd"]:
                worst_win = {
                    "win": win_id,
                    "regime": w_reg,
                    "net_usd": round(w_net, 4),
                    "ntl": round(w_ntl, 2),
                    "double_sided_usd_per_m": w_cost.double_sided_usd_per_m,
                }

        # Regime stratification breakdown across rows in this bucket
        regime_breakdown: dict[str, Any] = {}
        for reg in REGIMES:
            rgrp = [r for r in grp if r.get("regime") == reg]
            if not rgrp:
                continue
            r_spr = sum(float(x["spr"]) for x in rgrp)
            r_drft = sum(float(x["drft"]) for x in rgrp)
            r_pex = sum(float(x["pex"]) for x in rgrp)
            r_tslp = sum(float(x["tslp"]) for x in rgrp)
            r_gross = r_spr + r_drft + r_pex + r_tslp
            r_fee = sum(float(x["fee_usd"]) for x in rgrp)
            r_net = r_gross - r_fee
            r_ntl = sum(float(x["ntl"]) for x in rgrp)
            r_tvol = sum(float(x["taker_volume_usd"]) for x in rgrp)
            r_gbp = (r_gross / r_ntl) * 1e4 if r_ntl > 0.0 else 0.0
            r_s = min(1.0, max(0.0, r_tvol / r_ntl)) if r_ntl > 0.0 else 0.0
            r_cost = compute_volume_farming_cost(
                r_gbp,
                r_s,
                float(first["taker_fee_bps"]),
                maker_fee_bps=float(first["maker_fee_bps"]),
                rebate_bps=float(first["rebate_bps"]),
            )
            r_wins = {str(x["win"]) for x in rgrp}
            regime_breakdown[reg] = {
                "rows": len(rgrp),
                "windows": len(r_wins),
                "ntl": round(r_ntl, 2),
                "net_usd": round(r_net, 4),
                "gross_ex_fee_bps": round(r_gbp, 6),
                "double_sided_usd_per_m": r_cost.double_sided_usd_per_m,
            }

        def _agg_mk(bps_key: str, den_key: str) -> float | None:
            num = 0.0
            den = 0.0
            for r in grp:
                val = r.get(bps_key)
                d = float(r.get(den_key) or 0.0)
                if val is not None and d > 0.0:
                    num += float(val) * d
                    den += d
            return round(num / den, 6) if den > 0.0 else None

        n_wins = len(by_win)
        out: dict[str, Any] = {
            "domain": first["domain"],
            "arm": first["arm"],
            "ablation_group": first["ablation_group"],
            "venue": first["venue"],
            "settlement": first["settlement"],
            "account_fee_status": first["account_fee_status"],
            "fill_evidence_mode": first["fill_evidence_mode"],
            "windows_covered": n_wins,
            "hours_covered": round(total_hours, 2),
            "rows": len(grp),
            "cont_rows": sum(1 for r in grp if r.get("cont")),
            "cyc": sum(int(r["cyc"]) for r in grp),
            "fills": sum(int(r["fills"]) for r in grp),
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
            "net_per_ntl_bp": round(net_bp, 6),
            "taker_exit_share_s": round(s_share, 6),
            "double_sided_usd_per_m": cost_bd.double_sided_usd_per_m,
            "maker_volume_usd_per_m": cost_bd.maker_volume_usd_per_m,
            "s_zero_double_sided_floor_usd_per_m": cost_bd.s_zero_double_sided_floor_usd_per_m,
            "markout_1s_bps": _agg_mk("markout_1s_bps", "markout_1s_den"),
            "markout_5s_bps": _agg_mk("markout_5s_bps", "markout_5s_den"),
            "markout_10s_bps": _agg_mk("markout_10s_bps", "markout_10s_den"),
            "positive_window_ratio": round(pos_wins / n_wins, 4) if n_wins > 0 else 0.0,
            "max_drawdown_usd": round(max_dd_usd, 4),
            "worst_10h_window": worst_win,
            "regime_breakdown": regime_breakdown,
            "cumulative_curve": curve,
        }
        if include_symbol:
            out["symbol"] = first["symbol"]
        return out

    by_domain_arm_venue: dict[tuple[str, str, str, str], list[dict[str, Any]]] = defaultdict(list)
    by_domain_arm_venue_sym: dict[tuple[str, str, str, str, str], list[dict[str, Any]]] = defaultdict(list)

    for row in active_rows:
        ev_mode = str(row["fill_evidence_mode"])
        by_domain_arm_venue[(row["domain"], row["arm"], row["venue"], ev_mode)].append(row)
        by_domain_arm_venue_sym[(row["domain"], row["arm"], row["venue"], row["symbol"], ev_mode)].append(row)

    w03_rows = [
        {
            "domain": r["domain"],
            "venue": r["venue"],
            "symbol": r["symbol"],
            "arm": r["arm"],
            "fill_evidence_mode": r["fill_evidence_mode"],
            "realized_1s_vol_bps": r.get("realized_1s_vol_bps"),
            "p99_1s_abs_ret_bps": r.get("p99_1s_abs_ret_bps"),
            "ntl": r["ntl"],
            "net_usd": r["net"],
            "double_sided_usd_per_m": r["double_sided_usd_per_m"],
        }
        for r in active_rows
        if r.get("win") == "w03"
    ]

    return {
        "by_domain_arm_venue": {
            f"{d}|{a}|{v}|{m}": _agg_bucket(grp, include_symbol=False)
            for (d, a, v, m), grp in sorted(by_domain_arm_venue.items())
        },
        "by_domain_arm_venue_symbol": {
            f"{d}|{a}|{v}|{s}|{m}": _agg_bucket(grp, include_symbol=True)
            for (d, a, v, s, m), grp in sorted(by_domain_arm_venue_sym.items())
        },
        "w03_extreme_highlight": w03_rows,
    }


def aggregate_turnover_summary(rows: Sequence[dict[str, Any]]) -> dict[str, Any]:
    """Backward-compatible wrapper around `annotate_and_aggregate_turnover`."""
    return annotate_and_aggregate_turnover(rows)


def run_duckdb_multivenue_eval(
    lakehouse_dir: Path,
    *,
    split: str = "holdout",
    window_ids: Sequence[str] | None = None,
    domains: Sequence[str] = ("crypto", "rwa"),
    venues: Sequence[str] | None = None,
    symbols: Sequence[str] | None = None,
    arm_names: Sequence[str] | None = None,
    include_bulk_diagnostic: bool = True,
    rebate_bps: float = 0.0,
    regime_thresholds: dict[str, tuple[float, float]] | None = None,
) -> dict[str, Any]:
    """Run read-only DuckDB `:memory:` multi-venue Crypto + RWA evaluation with per-window GC."""
    import duckdb  # Lazy import so stdlib-only CI never needs duckdb at module top level

    selected_windows = discover_campaigns(lakehouse_dir, split, window_ids)
    if not selected_windows:
        raise ValueError(f"No campaigns discovered under {lakehouse_dir} for split={split!r}")

    targets = build_default_targets(include_bulk_diagnostic=include_bulk_diagnostic)
    domain_set = set(domains)
    venue_set = set(venues) if venues else None
    sym_set = {s.upper() for s in symbols} if symbols else None
    active_targets = [
        t
        for t in targets
        if t.domain in domain_set
        and (venue_set is None or t.venue in venue_set)
        and (sym_set is None or t.symbol in sym_set)
    ]

    con = duckdb.connect(":memory:")
    con.execute("SET preserve_insertion_order=false; SET threads=2; SET memory_limit='2GB';")

    rows_out: list[dict[str, Any]] = []

    def _campaign_info(win_id: str, venue: str, domain: str) -> dict[str, str] | None:
        w_map = selected_windows.get(win_id) or {}
        if venue == "bulk":
            return w_map.get("bulk")
        return w_map.get("crypto" if domain == "crypto" else "rwa")

    for win_id in selected_windows:
        bucket_cache: dict[
            tuple[str, str, str], tuple[list[tuple[int, float, float, float, float]], dict[str, Any]]
        ] = {}

        def fetch_books_bucketed(
            venue: str,
            c_info: dict[str, str],
            market: str,
            bucket_ms: int = 250,
        ) -> tuple[list[tuple[int, float, float, float, float]], dict[str, Any]]:
            ckey = (venue, c_info["campaign_id"], market)
            if ckey in bucket_cache:
                return bucket_cache[ckey]
            files = _campaign_parquet_files(
                lakehouse_dir, "bbo_quotes", venue, c_info["utc_date"], c_info["campaign_id"]
            )
            if not files:
                empty = {"raw_events": 0, "duration_ms": 0, "hours": 0.0, "max_gap_ms": 0, "is_continuous_10h": False}
                bucket_cache[ckey] = ([], empty)
                return bucket_cache[ckey]

            f_list_sql = "[" + ", ".join(f"'{p}'" for p in files) + "]"
            cnt, t0, t1, mgap = con.execute(
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
            if not cnt or cnt < 2:
                empty = {"raw_events": int(cnt or 0), "duration_ms": 0, "hours": 0.0, "max_gap_ms": 0, "is_continuous_10h": False}
                bucket_cache[ckey] = ([], empty)
                return bucket_cache[ckey]

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
            bucket_cache[ckey] = (rows, cont)
            return bucket_cache[ckey]

        def fetch_trades(venue: str, c_info: dict[str, str], market: str) -> list[tuple[int, str, float, float]]:
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

        for target in active_targets:
            c_info = _campaign_info(win_id, target.venue, target.domain)
            if c_info is None:
                continue
            raw_mkt = VENUE_SYMBOL_MAP[target.venue][target.symbol]
            b_rows, cont = fetch_books_bucketed(target.venue, c_info, raw_mkt, bucket_ms=250)
            if len(b_rows) < 200:
                rows_out.append(
                    {
                        "win": win_id,
                        "campaign_id": c_info["campaign_id"],
                        "domain": target.domain,
                        "venue": target.venue,
                        "symbol": target.symbol,
                        "raw_market": raw_mkt,
                        "settlement": target.settlement,
                        "account_fee_status": target.account_fee_status,
                        "fill_evidence_mode": "public_trades" if target.has_public_trades else "book_diff_diagnostic",
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
                    "bb": float(r[1]),
                    "bsz": max(float(r[2]), 1e-9),
                    "ba": float(r[3]),
                    "asz": max(float(r[4]), 1e-9),
                }
                for r in b_rows
            ]

            ref_venue_used: str | None = None
            ref_c_info = _campaign_info(win_id, "arcus", target.domain)  # non-bulk domain campaign for refs
            if ref_c_info is not None:
                for ref_v in target.ref_venues:
                    if ref_v not in VENUE_SYMBOL_MAP or target.symbol not in VENUE_SYMBOL_MAP[ref_v]:
                        continue
                    ref_raw = VENUE_SYMBOL_MAP[ref_v][target.symbol]
                    r_rows, _ = fetch_books_bucketed(ref_v, ref_c_info, ref_raw, bucket_ms=250)
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

            if target.has_public_trades:
                for ts_ms, agg_side, px, sz in fetch_trades(target.venue, c_info, raw_mkt):
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

            stream = prepare_replay_stream(events)
            del events
            tick = _infer_tick_from_prices([float(r[1]) for r in b_rows])
            arms = build_ablation_arms(
                target.venue,
                target.domain,
                maker_fee_bps=target.maker_fee_bps,
                taker_fee_bps=target.taker_fee_bps,
                rebate_bps=rebate_bps,
                infer_book_diff_sweeps=(not target.has_public_trades),
                arm_names=arm_names,
            )
            for arm_cfg in arms:
                sim_res = simulate_prepared_arm(stream, tick, arm_cfg)
                sim_res.pop("_exact_cash_ex_fee_usd", None)
                sim_res.pop("_exact_gross_usd", None)
                sim_res.pop("_exact_fee_usd", None)
                sim_res.pop("_exact_net_usd", None)
                rows_out.append(
                    {
                        "win": win_id,
                        "campaign_id": c_info["campaign_id"],
                        "domain": target.domain,
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
                        **sim_res,
                    }
                )
            del stream

        bucket_cache.clear()
        gc.collect()

    con.close()

    eff_thresholds = (
        regime_thresholds
        if regime_thresholds is not None
        else (compute_regime_thresholds_from_rows(rows_out) if split in ("full_dev", "selection_dev") else None)
    )
    summary = annotate_and_aggregate_turnover(rows_out, regime_thresholds=eff_thresholds)

    return {
        "schema": "track03_multivenue_turnover_v2",
        "split": split,
        "rebate_bps": rebate_bps,
        "windows": selected_windows,
        "regime_thresholds": eff_thresholds,
        "targets": [dataclasses.asdict(t) for t in active_targets],
        "rows": rows_out,
        "summary": summary,
    }


def run_self_test() -> dict[str, Any]:
    """Verify exact cash/cost accounting, session gating, vol-quartile gating, regime/tail stats, and bulk isolation."""
    tick = 0.01
    ts_ext = int(datetime(2026, 11, 24, 13, 29, 55, tzinfo=timezone.utc).timestamp() * 1000)
    ts_rth = int(datetime(2026, 11, 24, 13, 30, 2, tzinfo=timezone.utc).timestamp() * 1000)
    assert classify_utc_session(ts_ext) == "us_extended"
    assert classify_utc_session(ts_rth) == "us_rth"

    events_rwa = [
        {"ts": ts_ext, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.04, 10.0)]},
        {"ts": ts_ext, "kind": "ref", "venue": "arcus", "bid": 100.00, "ask": 100.04},
        {"ts": ts_ext + 300, "kind": "trade", "side": "SELL", "price": 99.99, "size": 8.0},
        {"ts": ts_rth, "kind": "book", "bids": [(100.02, 10.0)], "asks": [(100.06, 10.0)]},
        {"ts": ts_rth, "kind": "ref", "venue": "arcus", "bid": 100.02, "ask": 100.06},
        {"ts": ts_rth + 250, "kind": "book", "bids": [(100.02, 10.0)], "asks": [(100.06, 10.0)]},
        {"ts": ts_rth + 600, "kind": "trade", "side": "BUY", "price": 100.07, "size": 8.0},
        {"ts": ts_rth + 700, "kind": "trade", "side": "SELL", "price": 100.01, "size": 8.0},
        {"ts": ts_rth + 2_000, "kind": "book", "bids": [(100.02, 10.0)], "asks": [(100.06, 10.0)]},
    ]

    verified_arms: list[str] = []
    for arm in build_ablation_arms("lighter", "rwa", maker_fee_bps=0.0, taker_fee_bps=0.0, rebate_bps=0.0):
        res = simulate_turnover_arm(events_rwa, tick, arm)
        assert res["terminal_flat"] is True
        assert abs(res["_exact_gross_usd"] - res["_exact_cash_ex_fee_usd"]) < 1e-9
        assert abs(res["_exact_net_usd"] - (res["_exact_gross_usd"] - res["_exact_fee_usd"])) < 1e-9
        verified_arms.append(arm.name)
        if arm.name == "G4_quiet_sessions_only":
            assert res["maker_fills"] == 2
            assert res["taker_exit_share_s"] < 0.005
            assert res["session_gated_ticks"] > 0

    tradeless = [
        {"ts": ts_ext, "kind": "book", "bids": [(100.00, 5.0)], "asks": [(100.05, 5.0)]},
        {"ts": ts_ext + 250, "kind": "book", "bids": [(100.10, 5.0)], "asks": [(100.15, 5.0)]},
    ]
    tl_arm = build_ablation_arms("lighter", "crypto", arm_names=["S0_all_hours"])[0]
    tl_res = simulate_turnover_arm(tradeless, tick, tl_arm)
    assert tl_res["fills"] == 0 and tl_res["fill_evidence_mode"] == "no_trades_in_window"

    bulk_arm = build_ablation_arms(
        "bulk", "crypto", maker_fee_bps=0.0, taker_fee_bps=2.0, infer_book_diff_sweeps=True, arm_names=["S0_all_hours"]
    )[0]
    bulk_res = simulate_turnover_arm(tradeless, tick, bulk_arm)
    assert bulk_res["fill_evidence_mode"] == "book_diff_diagnostic"

    assert classify_window_regime("crypto", "BTC", "w03", 0.5) == "extreme_crash"
    assert classify_window_regime("crypto", "BTC", "w04", 0.65) == "calm"

    targets = build_default_targets(include_bulk_diagnostic=True)
    settlements = {(t.venue, t.settlement, t.account_fee_status) for t in targets}
    assert ("lighter", "USDC", "public_tier0_unverified") in settlements
    assert ("lighter_robinhood", "USDG", "public_tier0_unverified") in settlements
    assert ("arcus", "USDC", "public_tier0_schedule_only") in settlements

    return {
        "self_test": "ok",
        "rwa_arms_verified": verified_arms,
        "tradeless_fills": tl_res["fills"],
        "bulk_mode": bulk_res["fill_evidence_mode"],
        "settlement_domains": sorted(f"{v}:{s}:{st}" for v, s, st in settlements),
    }


def main(argv: Sequence[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description="Multi-venue Crypto + RWA Track 03 turnover & regime evaluator")
    ap.add_argument(
        "--lakehouse",
        type=Path,
        default=Path(os.path.expanduser("~/aleph-tx-lakehouse")),
        help="Path to Aleph lakehouse root directory",
    )
    ap.add_argument(
        "--split",
        choices=("full_dev", "holdout", "selection_dev", "all"),
        default="holdout",
        help="Window split (`full_dev` w01-w20, `holdout` w21-w25, `selection_dev` w18-w21, `all` w01-w25)",
    )
    ap.add_argument("--windows", type=str, default=None, help="Comma-separated window IDs within --split")
    ap.add_argument("--domains", type=str, default="crypto,rwa", help="Comma-separated domains (crypto,rwa)")
    ap.add_argument("--venues", type=str, default=None, help="Optional comma-separated venue subset")
    ap.add_argument("--symbols", type=str, default=None, help="Optional comma-separated symbol subset")
    ap.add_argument("--arms", type=str, default=None, help="Optional comma-separated arm subset")
    ap.add_argument("--no-bulk", action="store_true", help="Exclude bulk book_diff_diagnostic targets")
    ap.add_argument("--rebate-bps", type=float, default=0.0, help="Explicit maker rebate bps (default 0.0)")
    ap.add_argument(
        "--regime-thresholds-from",
        type=Path,
        default=None,
        help="Optional full_dev JSON output whose frozen `regime_thresholds` are applied to holdout",
    )
    ap.add_argument("--output", type=Path, default=None, help="Optional JSON output file path")
    ap.add_argument("--self-test", action="store_true", help="Run built-in unit/invariant checks")
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

    frozen_thresholds: dict[str, tuple[float, float]] | None = None
    if args.regime_thresholds_from:
        src = json.loads(args.regime_thresholds_from.read_text(encoding="utf-8"))
        raw_thr = src.get("regime_thresholds") or {}
        frozen_thresholds = {str(k): (float(v[0]), float(v[1])) for k, v in raw_thr.items()}

    win_list = [w.strip() for w in args.windows.split(",") if w.strip()] if args.windows else None
    dom_list = [d.strip() for d in args.domains.split(",") if d.strip()]
    ven_list = [v.strip() for v in args.venues.split(",") if v.strip()] if args.venues else None
    sym_list = [s.strip() for s in args.symbols.split(",") if s.strip()] if args.symbols else None
    arm_list = [a.strip() for a in args.arms.split(",") if a.strip()] if args.arms else None

    result = run_duckdb_multivenue_eval(
        args.lakehouse,
        split=args.split,
        window_ids=win_list,
        domains=dom_list,
        venues=ven_list,
        symbols=sym_list,
        arm_names=arm_list,
        include_bulk_diagnostic=not args.no_bulk,
        rebate_bps=args.rebate_bps,
        regime_thresholds=frozen_thresholds,
    )
    rendered = json.dumps(result, indent=2, allow_nan=False)
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(rendered + "\n", encoding="utf-8")
        sha = hashlib.sha256(args.output.read_bytes()).hexdigest()
        print(json.dumps({"output": str(args.output), "sha256": sha, "rows": len(result["rows"])}))
    else:
        print(rendered)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
