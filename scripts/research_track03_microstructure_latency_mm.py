#!/usr/bin/env python3
"""Track 03 Microstructure & Lighter Standard (0/0 bps) Latency Diagnostic & Upgrade Evaluator.

Strictly stdlib-only module top-level (`duckdb` imported lazily inside lakehouse runner).
Reuses `compute_volume_farming_cost` (`eval_volume_farming_cost.py`) with `rebate_bps = 0.0`
and `track_fee_evidence.py` (`OPT-01` capability provenance).

Supports:
  - Stage A (`--mode stage-a`): Root-cause cash & latency diagnostics on `selection_dev` (`w01-w20`)
    sweeping `(order_ms, cancel_ms) in {(36,33), (100,200), (200,300), (300,300), (300,400)}`
    across `lighter` (`USDC`, `lighter_standard_zero_scenario` `0/0`) and `lighter_robinhood`
    (`USDG`, `verified_account_tier` `0/0`), decomposing `gross = spr + drft + pex + tslp`,
    cancel-in-flight toxic fill share, consecutive same-side sweep clustering, and forced IOC `tslp`.
  - Stage B (`--mode stage-b`): Round-2 iterative ablation (`G4_baseline`, `G5_microprice_tfi`,
    `G6_self_fill_stepback`, `G7_patient_passive_unwind`, `G8_combined`) at Standard latencies
    `(200, 300)` & `(300, 300)` on `selection_dev`, untouched `holdout` (`w21-w25`), and `w26_oos`.
"""

from __future__ import annotations

import argparse
import bisect
import concurrent.futures
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
from datetime import date, timedelta
from pathlib import Path
from typing import Any, Sequence

SCRIPT_DIR = Path(__file__).resolve().parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

from eval_volume_farming_cost import compute_volume_farming_cost  # noqa: E402
from track_fee_evidence import (  # noqa: E402
    VENUE_CAPABILITY_SNAPSHOT_VERSION,
    build_fee_provenance,
    load_track_fee_evidence,
    resolve_venue_fee,
)

TEN_HOURS_MS = 36_000_000
MAX_CONTINUOUS_GAP_MS = 60_000

CRYPTO_SYMBOLS: tuple[str, ...] = ("BTC", "ETH", "SOL")
RWA_SYMBOLS: tuple[str, ...] = ("AAPL", "SPY", "QQQ", "NVDA", "TSLA")

STAGE_A_LATENCY_PAIRS: tuple[tuple[int, int], ...] = (
    (36, 33),
    (100, 200),
    (200, 300),
    (300, 300),
    (300, 400),
)
STAGE_B_LATENCY_PAIRS: tuple[tuple[int, int], ...] = (
    (200, 300),
    (300, 300),
)

DEFAULT_REGIME_CUTOFFS: dict[str, tuple[float, float]] = {
    "crypto": (0.85, 1.20),
    "rwa": (0.30, 0.60),
}
REGIMES: tuple[str, ...] = ("calm", "normal", "extreme_crash")

_CAMPAIGN_RE = re.compile(r"^tokyo-(crypto|rwa)-.+-w(\d+)$")

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
    "arcus": {
        "BTC": "BTC-USD",
        "ETH": "ETH-USD",
        "SOL": "SOL-USD",
        "AAPL": "AAPL-USD",
        "SPY": "SPY-USD",
        "QQQ": "QQQ-USD",
        "NVDA": "NVDA-USD",
        "TSLA": "TSLA-USD",
    },
    "binance": {
        "BTC": "BTCUSDT",
        "ETH": "ETHUSDT",
        "SOL": "SOLUSDT",
    },
    "trade_xyz": {
        "NVDA": "xyz:NVDA",
        "AAPL": "xyz:AAPL",
        "TSLA": "xyz:TSLA",
    },
}

VENUE_SETTLEMENT: dict[str, str] = {
    "lighter": "USDC",
    "lighter_robinhood": "USDG",
}

# Frozen Round-2 parameters (selected on selection_dev w01-w20 only; untouched on holdout/w26).
FROZEN_STAGE_B_PARAMS: dict[str, dict[str, Any]] = {
    "G4_baseline": {},
    "G5_microprice_tfi": {
        "microprice_alpha_weight": 0.25,
        "tfi_ewma_tau_ms": 2_000.0,
        "tfi_skew_bps": 0.0,
        "tfi_veto_threshold": 0.75,
    },
    "G6_self_fill_stepback": {
        "self_fill_stepback_bps": 0.50,
        "self_fill_decay_ms": 1_500,
    },
    "G7_patient_passive_unwind": {
        "reduce_only_penny_ticks": 1,
    },
    "G8_combined": {
        "microprice_alpha_weight": 0.25,
        "tfi_ewma_tau_ms": 2_000.0,
        "tfi_skew_bps": 0.0,
        "tfi_veto_threshold": 0.75,
        "self_fill_stepback_bps": 0.50,
        "self_fill_decay_ms": 1_500,
        "reduce_only_penny_ticks": 1,
    },
}


@dataclasses.dataclass(frozen=True)
class LatencyTarget:
    """Target `(venue, domain, symbol)` evaluation spec with explicit settlement & fee provenance."""

    venue: str
    domain: str
    symbol: str
    settlement: str
    maker_fee_bps: float
    taker_fee_bps: float
    account_fee_status: str
    ref_venues: tuple[str, ...]
    fee_source: str


def build_standard_targets(
    *,
    fee_evidence_path: Path | None = None,
    domains: Sequence[str] = ("crypto", "rwa"),
    venues: Sequence[str] = ("lighter", "lighter_robinhood"),
    symbols: Sequence[str] | None = None,
) -> tuple[list[LatencyTarget], dict[str, Any]]:
    """Build Lighter Standard (0/0 bps) target matrix preserving USDC vs USDG provenance."""
    ev = load_track_fee_evidence(fee_evidence_path)
    domain_set = set(domains)
    sym_filter = {s.upper() for s in symbols} if symbols else None
    targets: list[LatencyTarget] = []

    for venue in venues:
        if venue not in VENUE_SETTLEMENT:
            raise ValueError(f"Unsupported venue {venue!r}; expected one of {sorted(VENUE_SETTLEMENT)}")
        settlement = VENUE_SETTLEMENT[venue]

        # Lighter Robinhood is verified 0/0 Standard; Lighter Core is modeled under explicit
        # 0/0 Standard scenario (`lighter_standard_zero_scenario`) rather than Premium 0.4/2.8.
        if venue == "lighter":
            maker_bps, taker_bps = 0.0, 0.0
            fee_status = "lighter_standard_zero_scenario"
            fee_source = "counterfactual_lighter_standard_zero"
        else:
            mk, tk, st, src = resolve_venue_fee(
                venue,
                "crypto",
                default_maker_bps=0.0,
                default_taker_bps=0.0,
                default_status="public_tier0_unverified",
                evidence=ev,
            )
            maker_bps, taker_bps, fee_status, fee_source = mk, tk, st, src

        if "crypto" in domain_set:
            for sym in CRYPTO_SYMBOLS:
                if sym_filter and sym not in sym_filter:
                    continue
                if sym not in VENUE_SYMBOL_MAP[venue]:
                    continue
                targets.append(
                    LatencyTarget(
                        venue=venue,
                        domain="crypto",
                        symbol=sym,
                        settlement=settlement,
                        maker_fee_bps=maker_bps,
                        taker_fee_bps=taker_bps,
                        account_fee_status=fee_status,
                        ref_venues=("binance",),
                        fee_source=fee_source,
                    )
                )

        if "rwa" in domain_set:
            for sym in RWA_SYMBOLS:
                if sym_filter and sym not in sym_filter:
                    continue
                if sym not in VENUE_SYMBOL_MAP[venue]:
                    continue
                targets.append(
                    LatencyTarget(
                        venue=venue,
                        domain="rwa",
                        symbol=sym,
                        settlement=settlement,
                        maker_fee_bps=maker_bps,
                        taker_fee_bps=taker_bps,
                        account_fee_status=fee_status,
                        ref_venues=("arcus",),
                        fee_source=fee_source,
                    )
                )

    prov = build_fee_provenance(ev, venues_in_scope=tuple(venues))
    prov["lighter_standard_scenario"] = True
    prov["lighter_fee_status"] = "lighter_standard_zero_scenario"
    return targets, prov


@dataclasses.dataclass(frozen=True)
class MicrostructureArmConfig:
    """Rust-aligned QueuePriorityMmConfig mirror for Track 03 latency & microstructure evaluation."""

    name: str
    order_delay_ms: int
    cancel_delay_ms: int
    exit_delay_ms: int
    quote_usd: float = 200.0
    max_pos_usd: float = 400.0
    max_hold_ms: int = 15_000
    keep_queue_ticks: int = 3
    queue_ahead_fraction: float = 0.5
    maker_fee_bps: float = 0.0
    taker_fee_bps: float = 0.0
    rebate_bps: float = 0.0
    min_entry_edge_bps: float = 0.10
    inventory_skew_bps_per_full_limit: float = 2.0
    vol_spread_mult: float = 0.5
    directional_markout_skew_mult: float = 1.0
    asymmetric_markout_max_bps: float = 3.0
    markout_shield_horizon_ms: int = 1_000
    markout_shield_adverse_threshold_bps: float = 1.0
    markout_shield_cooldown_ms: int = 2_000
    markout_shield_size_scale: float = 0.5
    use_oracle_anchor: bool = True
    oracle_basis_tau_ms: float = 120_000.0
    oracle_stale_suppress_bps: float = 2.5
    oracle_max_age_ms: int = 5_000
    quiet_regime_only: bool = True
    quiet_min_spread_vol_ratio: float = 0.80
    quiet_max_sigma_bps: float = 1.2
    quiet_sigma_floor_bps: float = 0.05
    # New Rust microstructure upgrades (default-off):
    microprice_alpha_weight: float = 0.0
    tfi_ewma_tau_ms: float = 0.0
    tfi_skew_bps: float = 0.0
    tfi_veto_threshold: float = 0.0
    self_fill_stepback_bps: float = 0.0
    self_fill_decay_ms: int = 0
    reduce_only_penny_ticks: int = 0

    @property
    def latency_label(self) -> str:
        return f"o{self.order_delay_ms}_c{self.cancel_delay_ms}"


def exit_delay_for_pair(order_ms: int, cancel_ms: int) -> int:
    """Compute realistic taker-exit wire delay matching latency pair."""
    if order_ms == 36 and cancel_ms == 33:
        return 40
    return max(order_ms, cancel_ms) + 20


def build_stage_a_arms(
    latency_pairs: Sequence[tuple[int, int]] = STAGE_A_LATENCY_PAIRS,
    *,
    maker_fee_bps: float = 0.0,
    taker_fee_bps: float = 0.0,
    rebate_bps: float = 0.0,
) -> list[MicrostructureArmConfig]:
    """Build Stage A baseline (`G4_baseline`) arms across the latency sweep."""
    return [
        MicrostructureArmConfig(
            name="G4_baseline",
            order_delay_ms=o_ms,
            cancel_delay_ms=c_ms,
            exit_delay_ms=exit_delay_for_pair(o_ms, c_ms),
            maker_fee_bps=maker_fee_bps,
            taker_fee_bps=taker_fee_bps,
            rebate_bps=rebate_bps,
        )
        for o_ms, c_ms in latency_pairs
    ]


def build_stage_b_arms(
    latency_pairs: Sequence[tuple[int, int]] = STAGE_B_LATENCY_PAIRS,
    *,
    maker_fee_bps: float = 0.0,
    taker_fee_bps: float = 0.0,
    rebate_bps: float = 0.0,
    arm_names: Sequence[str] | None = None,
    param_overrides: dict[str, dict[str, Any]] | None = None,
) -> list[MicrostructureArmConfig]:
    """Build Stage B ablation arms (`G4`..`G8`) at realistic Standard latencies."""
    spec_map = param_overrides if param_overrides is not None else FROZEN_STAGE_B_PARAMS
    selected_names = list(arm_names) if arm_names is not None else list(spec_map.keys())
    unknown = sorted(set(selected_names) - set(spec_map.keys()))
    if unknown:
        raise ValueError(f"Unknown Stage B arm(s): {unknown}; valid: {sorted(spec_map.keys())}")

    arms: list[MicrostructureArmConfig] = []
    for o_ms, c_ms in latency_pairs:
        e_ms = exit_delay_for_pair(o_ms, c_ms)
        for name in selected_names:
            arms.append(
                MicrostructureArmConfig(
                    name=name,
                    order_delay_ms=o_ms,
                    cancel_delay_ms=c_ms,
                    exit_delay_ms=e_ms,
                    maker_fee_bps=maker_fee_bps,
                    taker_fee_bps=taker_fee_bps,
                    rebate_bps=rebate_bps,
                    **spec_map[name],
                )
            )
    return arms


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
    cancel_requested_ms: int | None = None
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
    """Pre-normalized flat tuple stream reused across all latency/arm evaluations."""

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


def simulate_prepared_arm(
    stream: PreparedStream,
    tick: float,
    arm: MicrostructureArmConfig,
    *,
    markout_horizons_ms: tuple[int, ...] = (1_000, 5_000, 10_000),
) -> dict[str, Any]:
    """Run causal FIFO simulation mirroring Rust `QueuePriorityMmEngine` with latency & toxic-flow diagnostics."""
    if tick <= 0.0 or not math.isfinite(tick):
        raise ValueError(f"tick must be finite and > 0, got {tick}")

    tuples = stream.tuples
    mid_times = stream.mid_times
    mid_vals = stream.mid_vals
    has_trade_events = stream.has_trade_events
    has_ref_events = stream.has_ref_events
    evidence_mode = "public_trades" if has_trade_events else "no_trades_in_window"

    if not tuples or not mid_times:
        cost_bd = compute_volume_farming_cost(
            0.0, 0.0, arm.taker_fee_bps, maker_fee_bps=arm.maker_fee_bps, rebate_bps=arm.rebate_bps
        )
        return {
            "arm": arm.name,
            "latency_label": arm.latency_label,
            "order_delay_ms": arm.order_delay_ms,
            "cancel_delay_ms": arm.cancel_delay_ms,
            "exit_delay_ms": arm.exit_delay_ms,
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
            "cancel_in_flight_fills": 0,
            "cancel_in_flight_notional_usd": 0.0,
            "cancel_in_flight_share": 0.0,
            "cancel_in_flight_markout_1s_bps": None,
            "cancel_in_flight_markout_1s_den": 0.0,
            "same_side_cluster_fills": 0,
            "same_side_cluster_notional_usd": 0.0,
            "same_side_cluster_share": 0.0,
            "same_side_cluster_markout_1s_bps": None,
            "same_side_cluster_markout_1s_den": 0.0,
            "tfi_veto_cancels": 0,
            "tfi_veto_blocks": 0,
            "reduce_penny_placements": 0,
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

    cur_bb = 0.0
    cur_bsz = 0.0
    cur_ba = 0.0
    cur_asz = 0.0
    has_book = False

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

    # Signed TFI EWMA state
    tfi_signed_vol = 0.0
    tfi_total_vol = 0.0
    last_tfi_ts: int | None = None

    # Self-fill stepback state
    sf_stepback_bid = 0.0
    sf_stepback_ask = 0.0
    sf_last_ms_bid: int | None = None
    sf_last_ms_ask: int | None = None

    last_vol_ts = -1
    last_vol_mid = 0.0
    var_bps2_per_s = 0.0
    last_sigma_bps = 0.0

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

    # Diagnostic counters
    cancel_in_flight_fills = 0
    cancel_in_flight_notional_usd = 0.0
    cif_fills_log: list[tuple[int, str, float, float]] = []
    same_side_cluster_fills = 0
    same_side_cluster_notional_usd = 0.0
    cluster_fills_log: list[tuple[int, str, float, float]] = []
    last_maker_fill_ts_bid: int | None = None
    last_maker_fill_ts_ask: int | None = None
    tfi_veto_cancels = 0
    tfi_veto_blocks = 0
    reduce_penny_placements = 0

    maker_fills_log: list[tuple[int, str, float, float]] = []

    tol_tick_25 = tick * 0.25
    tol_tick_10 = tick * 0.10
    exp_fn = math.exp
    tfi_tau = arm.tfi_ewma_tau_ms
    sf_bps = arm.self_fill_stepback_bps
    sf_decay_ms = arm.self_fill_decay_ms
    max_book_age_ms = 5_000

    def decay_tfi_to(now_ms: int) -> None:
        nonlocal tfi_signed_vol, tfi_total_vol, last_tfi_ts
        if tfi_tau <= 0.0 or last_tfi_ts is None or now_ms <= last_tfi_ts:
            return
        dt = now_ms - last_tfi_ts
        if dt > max_book_age_ms:
            tfi_signed_vol = 0.0
            tfi_total_vol = 0.0
        else:
            d = exp_fn(-dt / tfi_tau)
            tfi_signed_vol *= d
            tfi_total_vol *= d
        last_tfi_ts = now_ms

    def decayed_sf(side: str, now_ms: int) -> float:
        if sf_bps <= 0.0 or sf_decay_ms <= 0:
            return 0.0
        if side == "BID":
            val, prev = sf_stepback_bid, sf_last_ms_bid
        else:
            val, prev = sf_stepback_ask, sf_last_ms_ask
        if prev is None:
            return 0.0
        if now_ms >= prev:
            return val * exp_fn(-(now_ms - prev) / sf_decay_ms)
        return val

    def record_fill(
        ts_ms: int,
        fill_side: str,
        fill_px: float,
        fill_qty: float,
        role: str,
        mid_now: float,
        *,
        was_cancel_in_flight: bool = False,
    ) -> None:
        nonlocal cash_ex_fee_usd, fee_usd, pos_qty, spr, drft, pex, tslp, ntl
        nonlocal maker_volume_usd, taker_volume_usd, cyc, maker_fills, taker_exits
        nonlocal sf_stepback_bid, sf_stepback_ask, sf_last_ms_bid, sf_last_ms_ask
        nonlocal cancel_in_flight_fills, cancel_in_flight_notional_usd
        nonlocal same_side_cluster_fills, same_side_cluster_notional_usd
        nonlocal last_maker_fill_ts_bid, last_maker_fill_ts_ask
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
            if was_cancel_in_flight:
                cancel_in_flight_fills += 1
                cancel_in_flight_notional_usd += notl
                cif_fills_log.append((ts_ms, mside, fill_px, notl))
            prev_same_ts = last_maker_fill_ts_bid if mside == "BID" else last_maker_fill_ts_ask
            if prev_same_ts is not None and (ts_ms - prev_same_ts) <= 2_000:
                same_side_cluster_fills += 1
                same_side_cluster_notional_usd += notl
                cluster_fills_log.append((ts_ms, mside, fill_px, notl))
            if mside == "BID":
                last_maker_fill_ts_bid = ts_ms
            else:
                last_maker_fill_ts_ask = ts_ms

            pending_markouts.append((ts_ms + arm.markout_shield_horizon_ms, mside, fill_px))
            if len(pending_markouts) > 256:
                pending_markouts.popleft()

            if sf_bps > 0.0 and sf_decay_ms > 0:
                cur_sf = decayed_sf(mside, ts_ms)
                upd_sf = min(3.0 * sf_bps, cur_sf + sf_bps)
                if mside == "BID":
                    sf_stepback_bid = upd_sf
                    sf_last_ms_bid = ts_ms if sf_last_ms_bid is None else max(sf_last_ms_bid, ts_ms)
                else:
                    sf_stepback_ask = upd_sf
                    sf_last_ms_ask = ts_ms if sf_last_ms_ask is None else max(sf_last_ms_ask, ts_ms)
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
                cif = o.cancel_effective_ms is not None and ts_ms < o.cancel_effective_ms
                record_fill(
                    ts_ms,
                    "SELL",
                    o.price,
                    filled_qty,
                    "maker",
                    0.5 * (cur_bb + cur_ba),
                    was_cancel_in_flight=cif,
                )
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
                cif = o.cancel_effective_ms is not None and ts_ms < o.cancel_effective_ms
                record_fill(
                    ts_ms,
                    "BUY",
                    o.price,
                    filled_qty,
                    "maker",
                    0.5 * (cur_bb + cur_ba),
                    was_cancel_in_flight=cif,
                )
                o.remaining -= filled_qty
                if o.remaining <= 1e-12:
                    bid_ord = None

    def decide(now_ms: int, sigma_bps: float) -> None:
        nonlocal ewma_basis, last_basis_ts, bid_ord, ask_ord, can, orders_placed, qpres
        nonlocal tfi_veto_cancels, tfi_veto_blocks, reduce_penny_placements
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

        # L1 Microprice offset (bps)
        l1_sum = cur_bsz + cur_asz
        if l1_sum > 0.0 and venue_mid > 0.0:
            microprice = (bb * cur_asz + ba * cur_bsz) / l1_sum
            microprice_offset_bps = ((microprice / venue_mid) - 1.0) * 1e4
        else:
            microprice_offset_bps = 0.0

        # Signed TFI normalized in [-1, 1]
        if tfi_tau > 0.0:
            decay_tfi_to(now_ms)
            tfi_norm = max(-1.0, min(1.0, tfi_signed_vol / tfi_total_vol)) if tfi_total_vol > 1e-12 else 0.0
        else:
            tfi_norm = 0.0

        fair = venue_mid
        stale_suppress_side: str | None = None
        if arm.use_oracle_anchor:
            fresh_refs = [m for t, m in ref_books.values() if now_ms - t <= arm.oracle_max_age_ms]
            if fresh_refs:
                ref_mid = fresh_refs[0] if len(fresh_refs) == 1 else sorted(fresh_refs)[len(fresh_refs) // 2]
                raw_basis = venue_mid - ref_mid
                if last_basis_ts is not None and 0 <= now_ms - last_basis_ts <= arm.oracle_max_age_ms and ewma_basis is not None:
                    alpha = 1.0 - exp_fn(-(now_ms - last_basis_ts) / arm.oracle_basis_tau_ms)
                    ewma_basis = ewma_basis + alpha * (raw_basis - ewma_basis)
                else:
                    ewma_basis = raw_basis
                last_basis_ts = now_ms
                ref_adj = ref_mid + ewma_basis
                if ref_adj > 0.0:
                    fair = 0.5 * venue_mid + 0.5 * ref_adj
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
        skew_factor = 0.5 * q_norm + 0.5 * q_norm * abs(q_norm)
        skew_bps = skew_factor * arm.inventory_skew_bps_per_full_limit

        pre_move_alpha_bps = arm.microprice_alpha_weight * microprice_offset_bps + arm.tfi_skew_bps * tfi_norm
        directional_fair = fair * exp_fn(pre_move_alpha_bps * 1e-4) if pre_move_alpha_bps != 0.0 else fair
        r_px = directional_fair * exp_fn(-skew_bps * 1e-4)

        tfi_bid_vetoed = arm.tfi_veto_threshold > 0.0 and (-tfi_norm) >= arm.tfi_veto_threshold
        tfi_ask_vetoed = arm.tfi_veto_threshold > 0.0 and tfi_norm >= arm.tfi_veto_threshold

        quiet_blocked = False
        if arm.quiet_regime_only:
            ratio = spread_bps / max(sigma_bps, arm.quiet_sigma_floor_bps)
            quiet_blocked = (ratio < arm.quiet_min_spread_vol_ratio) or (sigma_bps > arm.quiet_max_sigma_bps)

        min_edge_total = arm.min_entry_edge_bps + arm.maker_fee_bps
        vol_half_bps = arm.vol_spread_mult * sigma_bps
        max_micro_skew = arm.microprice_alpha_weight * (spread_bps * 0.5) if arm.microprice_alpha_weight > 0.0 else 0.0
        reducing_edge_floor = (
            arm.maker_fee_bps - arm.inventory_skew_bps_per_full_limit - arm.tfi_skew_bps - max_micro_skew
        )

        # BID
        is_red_bid = pos_usd < -10.0
        if not is_red_bid and (
            pos_usd >= arm.max_pos_usd
            or now_ms < markout_cd_bid
            or quiet_blocked
            or tfi_bid_vetoed
            or stale_suppress_side == "BID"
        ):
            if bid_ord is not None and bid_ord.cancel_effective_ms is None:
                bid_ord.cancel_requested_ms = now_ms
                bid_ord.cancel_effective_ms = now_ms + arm.cancel_delay_ms
                can += 1
                if tfi_bid_vetoed and not quiet_blocked:
                    tfi_veto_cancels += 1
            elif bid_ord is None and tfi_bid_vetoed and not quiet_blocked:
                tfi_veto_blocks += 1
        else:
            asym_b = (
                min(arm.asymmetric_markout_max_bps, (-markout_ewma_bid) * arm.directional_markout_skew_mult)
                if (arm.directional_markout_skew_mult > 0.0 and markout_ewma_bid < 0.0)
                else 0.0
            )
            sf_b = decayed_sf("BID", now_ms)
            open_req_b = min_edge_total + vol_half_bps + asym_b + sf_b

            if is_red_bid and arm.reduce_only_penny_ticks > 0 and sp_ticks >= 2:
                raw_b = min(bb + arm.reduce_only_penny_ticks * tick, ba - tick)
            elif is_red_bid and sp_ticks >= 2:
                raw_b = min(bb + tick, ba - tick)
            elif not is_red_bid:
                raw_b = min(bb, r_px * exp_fn(-open_req_b * 1e-4))
            else:
                raw_b = bb
            px_b = _round_tick(raw_b, tick, up=False)
            if px_b >= ba - 1e-12:
                px_b = _round_tick(ba - tick, tick, up=False)

            desired_edge_b = (r_px - px_b) / r_px * 1e4 if r_px > 0 else -1e9
            edge_ok_b = desired_edge_b >= (reducing_edge_floor if is_red_bid else open_req_b)
            if not edge_ok_b or px_b >= ba - 1e-12 or px_b <= 0.0:
                if bid_ord is not None and bid_ord.cancel_effective_ms is None:
                    bid_ord.cancel_requested_ms = now_ms
                    bid_ord.cancel_effective_ms = now_ms + arm.cancel_delay_ms
                    can += 1
            elif bid_ord is not None:
                if bid_ord.cancel_effective_ms is None:
                    ord_cross = bid_ord.price >= ba - 1e-12
                    tdiff = round(abs(bid_ord.price - px_b) / tick)
                    r_edge_b = (r_px - bid_ord.price) / r_px * 1e4 if r_px > 0 else -1e9
                    r_edge_ok_b = r_edge_b >= (reducing_edge_floor if is_red_bid else open_req_b)
                    # When patient reduce-only pennying wants strictly better touch inside wide spread, step inside once
                    better_red = is_red_bid and arm.reduce_only_penny_ticks > 0 and px_b > bid_ord.price + tol_tick_10
                    if not ord_cross and r_edge_ok_b and tdiff <= arm.keep_queue_ticks and not better_red:
                        qpres += 1
                    else:
                        bid_ord.cancel_requested_ms = now_ms
                        bid_ord.cancel_effective_ms = now_ms + arm.cancel_delay_ms
                        can += 1
            else:
                sc = (
                    arm.markout_shield_size_scale
                    if (not is_red_bid and arm.markout_shield_adverse_threshold_bps > 0.0 and markout_ewma_bid < -arm.markout_shield_adverse_threshold_bps)
                    else 1.0
                )
                ousd = max(arm.quote_usd, abs(pos_usd)) if is_red_bid else arm.quote_usd * sc
                q_b = max(ousd / px_b, abs(pos_qty)) if is_red_bid else (ousd / px_b)
                bid_ord = _RestingOrder("BID", px_b, q_b, q_b, 0.0, now_ms, now_ms + arm.order_delay_ms, False)
                orders_placed += 1
                if is_red_bid and arm.reduce_only_penny_ticks > 0 and px_b > bb + tol_tick_10:
                    reduce_penny_placements += 1

        # ASK
        is_red_ask = pos_usd > 10.0
        if not is_red_ask and (
            pos_usd <= -arm.max_pos_usd
            or now_ms < markout_cd_ask
            or quiet_blocked
            or tfi_ask_vetoed
            or stale_suppress_side == "ASK"
        ):
            if ask_ord is not None and ask_ord.cancel_effective_ms is None:
                ask_ord.cancel_requested_ms = now_ms
                ask_ord.cancel_effective_ms = now_ms + arm.cancel_delay_ms
                can += 1
                if tfi_ask_vetoed and not quiet_blocked:
                    tfi_veto_cancels += 1
            elif ask_ord is None and tfi_ask_vetoed and not quiet_blocked:
                tfi_veto_blocks += 1
        else:
            asym_a = (
                min(arm.asymmetric_markout_max_bps, (-markout_ewma_ask) * arm.directional_markout_skew_mult)
                if (arm.directional_markout_skew_mult > 0.0 and markout_ewma_ask < 0.0)
                else 0.0
            )
            sf_a = decayed_sf("ASK", now_ms)
            open_req_a = min_edge_total + vol_half_bps + asym_a + sf_a

            if is_red_ask and arm.reduce_only_penny_ticks > 0 and sp_ticks >= 2:
                raw_a = max(ba - arm.reduce_only_penny_ticks * tick, bb + tick)
            elif is_red_ask and sp_ticks >= 2:
                raw_a = max(ba - tick, bb + tick)
            elif not is_red_ask:
                raw_a = max(ba, r_px * exp_fn(open_req_a * 1e-4))
            else:
                raw_a = ba
            px_a = _round_tick(raw_a, tick, up=True)
            if px_a <= bb + 1e-12:
                px_a = _round_tick(bb + tick, tick, up=True)

            desired_edge_a = (px_a - r_px) / r_px * 1e4 if r_px > 0 else -1e9
            edge_ok_a = desired_edge_a >= (reducing_edge_floor if is_red_ask else open_req_a)
            if not edge_ok_a or px_a <= bb + 1e-12 or px_a <= 0.0:
                if ask_ord is not None and ask_ord.cancel_effective_ms is None:
                    ask_ord.cancel_requested_ms = now_ms
                    ask_ord.cancel_effective_ms = now_ms + arm.cancel_delay_ms
                    can += 1
            elif ask_ord is not None:
                if ask_ord.cancel_effective_ms is None:
                    ord_cross = ask_ord.price <= bb + 1e-12
                    tdiff = round(abs(ask_ord.price - px_a) / tick)
                    r_edge_a = (ask_ord.price - r_px) / r_px * 1e4 if r_px > 0 else -1e9
                    r_edge_ok_a = r_edge_a >= (reducing_edge_floor if is_red_ask else open_req_a)
                    better_red = is_red_ask and arm.reduce_only_penny_ticks > 0 and px_a < ask_ord.price - tol_tick_10
                    if not ord_cross and r_edge_ok_a and tdiff <= arm.keep_queue_ticks and not better_red:
                        qpres += 1
                    else:
                        ask_ord.cancel_requested_ms = now_ms
                        ask_ord.cancel_effective_ms = now_ms + arm.cancel_delay_ms
                        can += 1
            else:
                sc = (
                    arm.markout_shield_size_scale
                    if (not is_red_ask and arm.markout_shield_adverse_threshold_bps > 0.0 and markout_ewma_ask < -arm.markout_shield_adverse_threshold_bps)
                    else 1.0
                )
                ousd = max(arm.quote_usd, abs(pos_usd)) if is_red_ask else arm.quote_usd * sc
                q_a = max(ousd / px_a, abs(pos_qty)) if is_red_ask else (ousd / px_a)
                ask_ord = _RestingOrder("ASK", px_a, q_a, q_a, 0.0, now_ms, now_ms + arm.order_delay_ms, False)
                orders_placed += 1
                if is_red_ask and arm.reduce_only_penny_ticks > 0 and px_a < ba - tol_tick_10:
                    reduce_penny_placements += 1

    mk_thresh = arm.markout_shield_adverse_threshold_bps
    mk_cd_ms = arm.markout_shield_cooldown_ms
    max_hold_ms = arm.max_hold_ms
    exit_delay_ms = arm.exit_delay_ms

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
            cur_bb, cur_bsz, cur_ba, cur_asz = c2, c3, c4, c5
            has_book = True
            activate_arrivals(ts)
            if bid_ord is not None and bid_ord.activated and abs(bid_ord.price - cur_bb) <= tol_tick_25 and cur_bsz < bid_ord.queue_ahead:
                bid_ord.queue_ahead = cur_bsz
            if ask_ord is not None and ask_ord.activated and abs(ask_ord.price - cur_ba) <= tol_tick_25 and cur_asz < ask_ord.queue_ahead:
                ask_ord.queue_ahead = cur_asz

            mid_now = 0.5 * (cur_bb + cur_ba)
            if last_vol_ts >= 0 and ts > last_vol_ts and last_vol_mid > 0.0 and mid_now > 0.0:
                dt_s = (ts - last_vol_ts) / 1000.0
                if 0.0 < dt_s <= 10.0:
                    ret_bps = math.log(mid_now / last_vol_mid) * 1e4
                    inst_var = min(400.0, (ret_bps * ret_bps) / dt_s)
                    alpha = 1.0 - exp_fn(-dt_s / 15.0)
                    var_bps2_per_s = (1.0 - alpha) * var_bps2_per_s + alpha * inst_var
            last_vol_ts = ts
            last_vol_mid = mid_now
            last_sigma_bps = math.sqrt(var_bps2_per_s)

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

            decide(ts, last_sigma_bps)

        elif kind == 1:
            activate_arrivals(ts)
            ref_books[c2] = (ts, 0.5 * (c3 + c4))
            if arm.use_oracle_anchor and has_book:
                decide(ts, last_sigma_bps)

        else:
            if tfi_tau > 0.0:
                decay_tfi_to(ts)
                if last_tfi_ts is None or (last_tfi_ts - ts) <= max_book_age_ms:
                    tr_sgn = 1.0 if c2 == "BUY" else -1.0
                    tr_notl = c3 * c4
                    tfi_signed_vol += tr_sgn * tr_notl
                    tfi_total_vol += tr_notl
                    last_tfi_ts = ts if last_tfi_ts is None else max(last_tfi_ts, ts)

            if not has_book:
                continue
            activate_arrivals(ts)
            handle_trade(ts, c2, c3, c4)
            if tfi_tau > 0.0 and (arm.tfi_skew_bps > 0.0 or arm.tfi_veto_threshold > 0.0):
                decide(ts, last_sigma_bps)
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

    def _compute_log_markout(log: list[tuple[int, str, float, float]], h_ms: int) -> tuple[float | None, float]:
        num = 0.0
        den = 0.0
        for fl_ts, fl_side, fl_px, fl_notl in log:
            fm = mid_at(fl_ts + h_ms)
            if fm is None:
                continue
            sgn = 1.0 if fl_side == "BID" else -1.0
            mk_bps = sgn * (fm - fl_px) / fl_px * 1e4
            num += fl_notl * mk_bps
            den += fl_notl
        return (round(num / den, 6) if den > 0.0 else None), round(den, 6)

    markout_bps: dict[str, float | None] = {}
    markout_dens: dict[str, float] = {}
    for h_ms in markout_horizons_ms:
        val, den = _compute_log_markout(maker_fills_log, h_ms)
        key = f"{h_ms // 1000}s"
        markout_bps[key] = val
        markout_dens[key] = den

    cif_mk_1s, cif_mk_1s_den = _compute_log_markout(cif_fills_log, 1_000)
    cl_mk_1s, cl_mk_1s_den = _compute_log_markout(cluster_fills_log, 1_000)

    cif_share = (cancel_in_flight_notional_usd / maker_volume_usd) if maker_volume_usd > 0.0 else 0.0
    cl_share = (same_side_cluster_notional_usd / maker_volume_usd) if maker_volume_usd > 0.0 else 0.0

    return {
        "arm": arm.name,
        "latency_label": arm.latency_label,
        "order_delay_ms": arm.order_delay_ms,
        "cancel_delay_ms": arm.cancel_delay_ms,
        "exit_delay_ms": arm.exit_delay_ms,
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
        "markout_1s_den": markout_dens.get("1s", 0.0),
        "markout_5s_den": markout_dens.get("5s", 0.0),
        "markout_10s_den": markout_dens.get("10s", 0.0),
        "net_per_ntl_bp": round(net_per_ntl_bp, 6),
        "fills": maker_fills + taker_exits,
        "maker_fills": maker_fills,
        "taker_exits": taker_exits,
        "orders_placed": orders_placed,
        "alo_rejected": alo_rejected,
        "cancel_in_flight_fills": cancel_in_flight_fills,
        "cancel_in_flight_notional_usd": round(cancel_in_flight_notional_usd, 6),
        "cancel_in_flight_share": round(cif_share, 6),
        "cancel_in_flight_markout_1s_bps": cif_mk_1s,
        "cancel_in_flight_markout_1s_den": cif_mk_1s_den,
        "same_side_cluster_fills": same_side_cluster_fills,
        "same_side_cluster_notional_usd": round(same_side_cluster_notional_usd, 6),
        "same_side_cluster_share": round(cl_share, 6),
        "same_side_cluster_markout_1s_bps": cl_mk_1s,
        "same_side_cluster_markout_1s_den": cl_mk_1s_den,
        "tfi_veto_cancels": tfi_veto_cancels,
        "tfi_veto_blocks": tfi_veto_blocks,
        "reduce_penny_placements": reduce_penny_placements,
        "terminal_flat": terminal_flat,
        "_exact_cash_ex_fee_usd": cash_ex_fee_usd,
        "_exact_gross_usd": gross_usd,
        "_exact_fee_usd": fee_usd,
        "_exact_net_usd": net_usd,
    }


def simulate_arm(
    events: Sequence[dict[str, Any]],
    tick: float,
    arm: MicrostructureArmConfig,
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
    """Dynamically discover `tokyo-{crypto,rwa}-*-w*` Hive campaigns across W01-W26."""
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

    def _keep_win(win_num: int) -> bool:
        if window_ids:
            return f"w{win_num:02d}" in set(window_ids)
        if split == "selection_dev":
            return 1 <= win_num <= 20
        if split == "holdout":
            return 21 <= win_num <= 25
        if split == "w26_oos":
            return win_num == 26
        if split == "all":
            return 1 <= win_num <= 26
        raise ValueError(f"Unsupported split {split!r}")

    out: dict[str, dict[str, dict[str, str]]] = {}
    for win_id in sorted(discovered.keys(), key=_window_sort_key):
        kinds_out: dict[str, dict[str, str]] = {}
        for skind, info in discovered[win_id].items():
            if _keep_win(int(info["win_num"])):
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


def annotate_and_aggregate(
    rows: Sequence[dict[str, Any]],
    *,
    regime_thresholds: dict[str, tuple[float, float]] | None = None,
) -> dict[str, Any]:
    """Aggregate cash decomposition, latency diagnostics, regime stratification, and tail risk."""
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

    def _agg_mk(grp: Sequence[dict[str, Any]], bps_key: str, den_key: str) -> float | None:
        num = 0.0
        den = 0.0
        for r in grp:
            val = r.get(bps_key)
            d = float(r.get(den_key) or 0.0)
            if val is not None and d > 0.0:
                num += float(val) * d
                den += d
        return round(num / den, 6) if den > 0.0 else None

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

        cif_fills = sum(int(r.get("cancel_in_flight_fills", 0)) for r in grp)
        cif_notl = sum(float(r.get("cancel_in_flight_notional_usd", 0.0)) for r in grp)
        cif_share = (cif_notl / maker_vol) if maker_vol > 0.0 else 0.0
        cl_fills = sum(int(r.get("same_side_cluster_fills", 0)) for r in grp)
        cl_notl = sum(float(r.get("same_side_cluster_notional_usd", 0.0)) for r in grp)
        cl_share = (cl_notl / maker_vol) if maker_vol > 0.0 else 0.0

        by_win: dict[str, list[dict[str, Any]]] = defaultdict(list)
        for r in grp:
            by_win[str(r["win"])].append(r)

        cum_net = 0.0
        peak_net = 0.0
        max_dd_usd = 0.0
        pos_wins = 0
        total_hours = 0.0
        worst_win: dict[str, Any] | None = None
        for win_id in sorted(by_win.keys(), key=_window_sort_key):
            wgrp = by_win[win_id]
            w_gross = sum(float(x["spr"]) + float(x["drft"]) + float(x["pex"]) + float(x["tslp"]) for x in wgrp)
            w_fee = sum(float(x["fee_usd"]) for x in wgrp)
            w_net = w_gross - w_fee
            w_ntl = sum(float(x["ntl"]) for x in wgrp)
            w_tvol = sum(float(x["taker_volume_usd"]) for x in wgrp)
            w_gbp = (w_gross / w_ntl) * 1e4 if w_ntl > 0.0 else 0.0
            w_s = min(1.0, max(0.0, w_tvol / w_ntl)) if w_ntl > 0.0 else 0.0
            w_cost = compute_volume_farming_cost(
                w_gbp,
                w_s,
                float(first["taker_fee_bps"]),
                maker_fee_bps=float(first["maker_fee_bps"]),
                rebate_bps=float(first["rebate_bps"]),
            )
            total_hours += max(float((x.get("continuity") or {}).get("hours", 0.0)) for x in wgrp)
            cum_net += w_net
            if cum_net > peak_net:
                peak_net = cum_net
            max_dd_usd = max(max_dd_usd, peak_net - cum_net)
            if w_net > 0.0:
                pos_wins += 1
            if worst_win is None or w_net < worst_win["net_usd"]:
                worst_win = {
                    "win": win_id,
                    "net_usd": round(w_net, 4),
                    "ntl": round(w_ntl, 2),
                    "double_sided_usd_per_m": w_cost.double_sided_usd_per_m,
                }

        n_wins = len(by_win)
        out: dict[str, Any] = {
            "domain": first["domain"],
            "arm": first["arm"],
            "latency_label": first["latency_label"],
            "order_delay_ms": first["order_delay_ms"],
            "cancel_delay_ms": first["cancel_delay_ms"],
            "exit_delay_ms": first["exit_delay_ms"],
            "venue": first["venue"],
            "settlement": first["settlement"],
            "account_fee_status": first["account_fee_status"],
            "windows_covered": n_wins,
            "hours_covered": round(total_hours, 2),
            "rows": len(grp),
            "cont_rows": sum(1 for r in grp if r.get("cont")),
            "cyc": sum(int(r["cyc"]) for r in grp),
            "fills": sum(int(r["fills"]) for r in grp),
            "maker_fills": sum(int(r["maker_fills"]) for r in grp),
            "taker_exits": sum(int(r["taker_exits"]) for r in grp),
            "spr": round(spr, 6),
            "drft": round(drft, 6),
            "pex": round(pex, 6),
            "tslp": round(tslp, 6),
            "tslp_bps_per_ntl": round((tslp / ntl) * 1e4, 6) if ntl > 0.0 else 0.0,
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
            "markout_1s_bps": _agg_mk(grp, "markout_1s_bps", "markout_1s_den"),
            "markout_5s_bps": _agg_mk(grp, "markout_5s_bps", "markout_5s_den"),
            "markout_10s_bps": _agg_mk(grp, "markout_10s_bps", "markout_10s_den"),
            "cancel_in_flight_fills": cif_fills,
            "cancel_in_flight_notional_usd": round(cif_notl, 4),
            "cancel_in_flight_share": round(cif_share, 6),
            "cancel_in_flight_markout_1s_bps": _agg_mk(
                grp, "cancel_in_flight_markout_1s_bps", "cancel_in_flight_markout_1s_den"
            ),
            "same_side_cluster_fills": cl_fills,
            "same_side_cluster_notional_usd": round(cl_notl, 4),
            "same_side_cluster_share": round(cl_share, 6),
            "same_side_cluster_markout_1s_bps": _agg_mk(
                grp, "same_side_cluster_markout_1s_bps", "same_side_cluster_markout_1s_den"
            ),
            "tfi_veto_cancels": sum(int(r.get("tfi_veto_cancels", 0)) for r in grp),
            "tfi_veto_blocks": sum(int(r.get("tfi_veto_blocks", 0)) for r in grp),
            "reduce_penny_placements": sum(int(r.get("reduce_penny_placements", 0)) for r in grp),
            "positive_window_ratio": round(pos_wins / n_wins, 4) if n_wins > 0 else 0.0,
            "max_drawdown_usd": round(max_dd_usd, 4),
            "worst_10h_window": worst_win,
        }
        if include_symbol:
            out["symbol"] = first["symbol"]
        return out

    by_davl: dict[tuple[str, str, str, str], list[dict[str, Any]]] = defaultdict(list)
    by_davls: dict[tuple[str, str, str, str, str], list[dict[str, Any]]] = defaultdict(list)
    for r in active_rows:
        by_davl[(r["domain"], r["arm"], r["venue"], r["latency_label"])].append(r)
        by_davls[(r["domain"], r["arm"], r["venue"], r["latency_label"], r["symbol"])].append(r)

    return {
        "by_domain_arm_venue_latency": {
            f"{d}|{a}|{v}|{l}": _agg_bucket(grp, include_symbol=False)
            for (d, a, v, l), grp in sorted(by_davl.items())
        },
        "by_domain_arm_venue_latency_symbol": {
            f"{d}|{a}|{v}|{l}|{s}": _agg_bucket(grp, include_symbol=True)
            for (d, a, v, l, s), grp in sorted(by_davls.items())
        },
    }


def _parse_latency_pairs(raw: str | None, default_pairs: Sequence[tuple[int, int]]) -> list[tuple[int, int]]:
    if not raw:
        return list(default_pairs)
    out: list[tuple[int, int]] = []
    for part in raw.split(","):
        part = part.strip()
        if not part:
            continue
        o_s, c_s = part.split(":")
        out.append((int(o_s), int(c_s)))
    return out


def _eval_single_window(
    lakehouse_dir: Path,
    win_id: str,
    win_map: dict[str, dict[str, str]],
    targets: Sequence[LatencyTarget],
    mode: str,
    pairs: Sequence[tuple[int, int]],
    arm_names: Sequence[str] | None,
    rebate_bps: float,
    param_overrides: dict[str, dict[str, Any]] | None,
) -> tuple[list[dict[str, Any]], float, float, int]:
    """Evaluate a single 10h window in an isolated DuckDB `:memory:` connection."""
    import duckdb  # Lazy import so stdlib-only CI never requires duckdb

    con = duckdb.connect(":memory:")
    con.execute("SET preserve_insertion_order=false; SET threads=1; SET memory_limit='1GB';")
    rows_out: list[dict[str, Any]] = []
    max_gross_err = 0.0
    max_net_err = 0.0
    non_flat_count = 0
    bucket_cache: dict[tuple[str, str, str], tuple[list[tuple[int, float, float, float, float]], dict[str, Any]]] = {}

    def fetch_books_bucketed(
        venue: str, c_info: dict[str, str], market: str, bucket_ms: int = 250
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
                WHERE market = ? AND tradable = true AND bid_px_f64 > 0 AND ask_px_f64 > bid_px_f64
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
            WHERE market = ? AND tradable = true AND bid_px_f64 > 0 AND ask_px_f64 > bid_px_f64
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
            WHERE market = ? AND NOT is_zero_price_transfer AND price_f64 > 0 AND size_f64 > 0
            ORDER BY available_at_ms ASC
            """,
            [market],
        ).fetchall()

    for target in targets:
        c_info = win_map.get(target.domain)
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
                    "cont": False,
                    "continuity": cont,
                    "skipped": f"too_few_books:{len(b_rows)}",
                }
            )
            continue

        events: list[dict[str, Any]] = [
            {"ts": int(r[0]), "kind": "book", "bb": float(r[1]), "bsz": max(float(r[2]), 1e-9), "ba": float(r[3]), "asz": max(float(r[4]), 1e-9)}
            for r in b_rows
        ]
        ref_venue_used: str | None = None
        for ref_v in target.ref_venues:
            if ref_v not in VENUE_SYMBOL_MAP or target.symbol not in VENUE_SYMBOL_MAP[ref_v]:
                continue
            ref_raw = VENUE_SYMBOL_MAP[ref_v][target.symbol]
            r_rows, _ = fetch_books_bucketed(ref_v, c_info, ref_raw, bucket_ms=250)
            if len(r_rows) >= 200:
                ref_venue_used = ref_v
                for rr in r_rows:
                    events.append({"ts": int(rr[0]), "kind": "ref", "venue": ref_v, "bid": float(rr[1]), "ask": float(rr[3])})
                break

        for ts_ms, agg_side, px, sz in fetch_trades(target.venue, c_info, raw_mkt):
            s_up = str(agg_side).upper()
            side_str = "BUY" if s_up in ("BUY", "B", "BID") else "SELL"
            events.append({"ts": int(ts_ms), "kind": "trade", "side": side_str, "price": float(px), "size": float(sz)})

        stream = prepare_replay_stream(events)
        del events
        tick = _infer_tick_from_prices([float(r[1]) for r in b_rows])

        if mode == "stage-a":
            arms = build_stage_a_arms(
                pairs,
                maker_fee_bps=target.maker_fee_bps,
                taker_fee_bps=target.taker_fee_bps,
                rebate_bps=rebate_bps,
            )
        else:
            arms = build_stage_b_arms(
                pairs,
                maker_fee_bps=target.maker_fee_bps,
                taker_fee_bps=target.taker_fee_bps,
                rebate_bps=rebate_bps,
                arm_names=arm_names,
                param_overrides=param_overrides,
            )

        for arm_cfg in arms:
            sim_res = simulate_prepared_arm(stream, tick, arm_cfg)
            ex_cash = float(sim_res.pop("_exact_cash_ex_fee_usd"))
            ex_gross = float(sim_res.pop("_exact_gross_usd"))
            ex_fee = float(sim_res.pop("_exact_fee_usd"))
            ex_net = float(sim_res.pop("_exact_net_usd"))
            max_gross_err = max(max_gross_err, abs(ex_gross - ex_cash))
            max_net_err = max(max_net_err, abs(ex_net - (ex_gross - ex_fee)))
            if not sim_res["terminal_flat"]:
                non_flat_count += 1
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
                    "fee_source": target.fee_source,
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
    con.close()
    gc.collect()
    return rows_out, max_gross_err, max_net_err, non_flat_count


def run_duckdb_eval(
    lakehouse_dir: Path,
    *,
    mode: str = "stage-a",
    split: str = "selection_dev",
    window_ids: Sequence[str] | None = None,
    domains: Sequence[str] = ("crypto", "rwa"),
    venues: Sequence[str] = ("lighter", "lighter_robinhood"),
    symbols: Sequence[str] | None = None,
    latency_pairs: Sequence[tuple[int, int]] | None = None,
    arm_names: Sequence[str] | None = None,
    rebate_bps: float = 0.0,
    regime_thresholds: dict[str, tuple[float, float]] | None = None,
    fee_evidence_path: Path | None = None,
    param_overrides: dict[str, dict[str, Any]] | None = None,
    max_workers: int = 1,
) -> dict[str, Any]:
    """Run read-only DuckDB `:memory:` evaluation for Stage A or Stage B (optionally parallel per window)."""
    selected_windows = discover_campaigns(lakehouse_dir, split, window_ids)
    if not selected_windows:
        raise ValueError(f"No campaigns discovered under {lakehouse_dir} for split={split!r}")

    targets, fee_prov = build_standard_targets(
        fee_evidence_path=fee_evidence_path,
        domains=domains,
        venues=venues,
        symbols=symbols,
    )
    pairs = list(
        latency_pairs
        if latency_pairs is not None
        else (STAGE_A_LATENCY_PAIRS if mode == "stage-a" else STAGE_B_LATENCY_PAIRS)
    )

    rows_out: list[dict[str, Any]] = []
    max_gross_err = 0.0
    max_net_err = 0.0
    non_flat_count = 0

    eff_workers = max(1, min(int(max_workers), len(selected_windows)))
    if eff_workers == 1:
        for win_id, win_map in selected_windows.items():
            w_rows, w_ge, w_ne, w_nf = _eval_single_window(
                lakehouse_dir, win_id, win_map, targets, mode, pairs, arm_names, rebate_bps, param_overrides
            )
            rows_out.extend(w_rows)
            max_gross_err = max(max_gross_err, w_ge)
            max_net_err = max(max_net_err, w_ne)
            non_flat_count += w_nf
    else:
        with concurrent.futures.ProcessPoolExecutor(max_workers=eff_workers) as pool:
            futs = [
                pool.submit(
                    _eval_single_window,
                    lakehouse_dir,
                    win_id,
                    win_map,
                    targets,
                    mode,
                    pairs,
                    arm_names,
                    rebate_bps,
                    param_overrides,
                )
                for win_id, win_map in selected_windows.items()
            ]
            for fut in concurrent.futures.as_completed(futs):
                w_rows, w_ge, w_ne, w_nf = fut.result()
                rows_out.extend(w_rows)
                max_gross_err = max(max_gross_err, w_ge)
                max_net_err = max(max_net_err, w_ne)
                non_flat_count += w_nf
        rows_out.sort(
            key=lambda r: (
                _window_sort_key(str(r["win"])),
                str(r.get("domain", "")),
                str(r.get("venue", "")),
                str(r.get("symbol", "")),
                str(r.get("latency_label", "")),
                str(r.get("arm", "")),
            )
        )

    eff_thresholds = (
        regime_thresholds
        if regime_thresholds is not None
        else (compute_regime_thresholds_from_rows(rows_out) if split == "selection_dev" else None)
    )
    summary = annotate_and_aggregate(rows_out, regime_thresholds=eff_thresholds)

    return {
        "schema": "track03_microstructure_latency_v1",
        "mode": mode,
        "venue_capability_snapshot_version": VENUE_CAPABILITY_SNAPSHOT_VERSION,
        "fee_provenance": fee_prov,
        "split": split,
        "rebate_bps": rebate_bps,
        "latency_pairs": [list(p) for p in pairs],
        "frozen_stage_b_params": FROZEN_STAGE_B_PARAMS if mode == "stage-b" else None,
        "windows": selected_windows,
        "regime_thresholds": eff_thresholds,
        "invariants": {
            "non_flat_count": non_flat_count,
            "terminal_flat_all": non_flat_count == 0,
            "max_gross_err": max_gross_err,
            "max_net_err": max_net_err,
        },
        "targets": [dataclasses.asdict(t) for t in targets],
        "rows": rows_out,
        "summary": summary,
    }


def run_self_test() -> dict[str, Any]:
    """Verify exact accounting, cancel-in-flight diagnostics, and G5-G8 invariants on synthetic streams."""
    tick = 0.01
    events: list[dict[str, Any]] = [
        {"ts": 0, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.06, 10.0)]},
        {"ts": 0, "kind": "ref", "venue": "arcus", "bid": 100.00, "ask": 100.06},
        # At ts=300: order active (order_delay=200ms). Sell trade fills BID at 100.00 -> long!
        {"ts": 300, "kind": "trade", "side": "SELL", "price": 100.00, "size": 8.0},
        # Wide spread persists; G7/G8 patient reduce_only_penny_ticks=2 steps Ask inside to 100.04!
        {"ts": 600, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.06, 10.0)]},
        # Buy trade at 100.04 closes G7/G8 passively, while G4 at 100.05 also closes passively
        {"ts": 900, "kind": "trade", "side": "BUY", "price": 100.04, "size": 5.0},
        {"ts": 1_200, "kind": "trade", "side": "BUY", "price": 100.06, "size": 5.0},
        # Second fill + hold timeout IOC
        {"ts": 1_500, "kind": "book", "bids": [(100.00, 2.0)], "asks": [(100.06, 2.0)]},
        {"ts": 1_800, "kind": "trade", "side": "SELL", "price": 99.98, "size": 6.0},
        {"ts": 18_000, "kind": "book", "bids": [(99.95, 5.0)], "asks": [(100.01, 5.0)]},
        {"ts": 19_000, "kind": "book", "bids": [(99.95, 5.0)], "asks": [(100.01, 5.0)]},
    ]

    arms = build_stage_b_arms(latency_pairs=[(200, 300)], rebate_bps=0.0)
    assert len(arms) == 5
    verified: list[str] = []
    for arm in arms:
        res = simulate_arm(events, tick, arm)
        assert res["terminal_flat"] is True
        assert abs(res["_exact_gross_usd"] - res["_exact_cash_ex_fee_usd"]) <= 1e-9
        assert abs(res["_exact_net_usd"] - (res["_exact_gross_usd"] - res["_exact_fee_usd"])) <= 1e-9
        assert abs((res["spr"] + res["drft"] + res["pex"] + res["tslp"]) - res["gross_usd"]) <= 1e-6
        verified.append(arm.name)

    return {
        "self_test": "ok",
        "arms_verified": verified,
    }


def main(argv: Sequence[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description="Track 03 Microstructure & Lighter Standard Latency Evaluator")
    ap.add_argument("--lakehouse", type=Path, default=Path(os.path.expanduser("~/aleph-tx-lakehouse")))
    ap.add_argument("--mode", choices=("stage-a", "stage-b"), default="stage-a")
    ap.add_argument("--split", choices=("selection_dev", "holdout", "w26_oos", "all"), default="selection_dev")
    ap.add_argument("--windows", type=str, default=None)
    ap.add_argument("--domains", type=str, default="crypto,rwa")
    ap.add_argument("--venues", type=str, default="lighter,lighter_robinhood")
    ap.add_argument("--symbols", type=str, default=None)
    ap.add_argument("--latency-pairs", type=str, default=None, help="Comma-separated o:c pairs, e.g. 200:300,300:300")
    ap.add_argument("--arms", type=str, default=None)
    ap.add_argument("--rebate-bps", type=float, default=0.0)
    ap.add_argument("--fee-evidence", type=Path, default=None)
    ap.add_argument("--regime-thresholds-from", type=Path, default=None)
    ap.add_argument("--max-workers", type=int, default=1, help="Parallel per-window worker processes (default 1)")
    ap.add_argument("--output", type=Path, default=None)
    ap.add_argument("--self-test", action="store_true")
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
    ven_list = [v.strip() for v in args.venues.split(",") if v.strip()]
    sym_list = [s.strip() for s in args.symbols.split(",") if s.strip()] if args.symbols else None
    arm_list = [a.strip() for a in args.arms.split(",") if a.strip()] if args.arms else None
    pairs = _parse_latency_pairs(
        args.latency_pairs, STAGE_A_LATENCY_PAIRS if args.mode == "stage-a" else STAGE_B_LATENCY_PAIRS
    )

    result = run_duckdb_eval(
        args.lakehouse,
        mode=args.mode,
        split=args.split,
        window_ids=win_list,
        domains=dom_list,
        venues=ven_list,
        symbols=sym_list,
        latency_pairs=pairs,
        arm_names=arm_list,
        rebate_bps=args.rebate_bps,
        regime_thresholds=frozen_thresholds,
        fee_evidence_path=args.fee_evidence,
        max_workers=args.max_workers,
    )
    rendered = json.dumps(result, indent=2, allow_nan=False)
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(rendered + "\n", encoding="utf-8")
        sha = hashlib.sha256(args.output.read_bytes()).hexdigest()
        print(
            json.dumps(
                {
                    "output": str(args.output),
                    "sha256": sha,
                    "rows": len(result["rows"]),
                    "invariants": result["invariants"],
                }
            )
        )
    else:
        print(rendered)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
