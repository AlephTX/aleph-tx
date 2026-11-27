#!/usr/bin/env python3
"""Track 03 Single-Factor Core 4 Hold Ablation & W28+ Pre-Registration (`W01-W27`, `NOT -w28`).

Strictly stdlib-only module top level (`duckdb` imported lazily inside lakehouse runner).
Reuses P0-fixed simulator & discovery primitives from:
  - `research_track03_full_universe_speedbump_mm.py` (`SpeedbumpArmConfig`, `PreparedStream`,
    `simulate_prepared_arm`, `declared_end_ms`, `stale_exit_blocks`, `330 ms` re-queue)
  - `research_track03_patient_profit_frontier.py` (`discover_campaigns_w27`, `assert_no_w28_leak`)
  - `audit_multiple_testing_overfitting.py` (`PSR`, `N_eff`, `DSR`, `CSCV PBO`)
  - `track_fee_evidence.py` (`OPT-01` fee provenance)

Implements Roadmap §2 & §4 accounting:
  - True ledger `$/M = sum(net_cash_cost) / sum(target_executed_fill_notional_usd) * 1e6`
    (alongside clearly labeled `historical_double_sided_*_usd_per_m` cycle approximation).
  - `realized_only` vs `mtm_incl_residual`, `zero_rebate`, `maker_volume_usd`, taker share `s`.
  - Zero-fill windows retained in coverage/risk denominators with `unit_cost = None`.
  - Per-closed-lot hold duration quantiles (`p50`, `p95`, `p99`, `max`), max gross exposure
    (per-symbol & simultaneous portfolio), `stale_exit_blocks`, `terminal_residual_notional_usd`.
  - Pre-registered `ex-META` sensitivity & per-symbol PnL/volume contribution shares.
  - Politis-Romano stationary block bootstrap 95% CI on paired window delta vs `S4`.
  - Both raw `N_trials` DSR (in-study & cumulative `M=18`) & `N_eff` DSR + `CSCV PBO`.
"""

from __future__ import annotations

import argparse
import bisect
import concurrent.futures
import dataclasses
import gc
import hashlib
import json
import math
import os
import random
import re
import sys
from collections import defaultdict, deque
from pathlib import Path
from typing import Any, Sequence

SCRIPT_DIR = Path(__file__).resolve().parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

from audit_multiple_testing_overfitting import (  # noqa: E402
    cscv_pbo,
    deflated_sharpe_report,
    expected_max_sharpe,
    probabilistic_sharpe_ratio,
    series_moments,
)
from eval_volume_farming_cost import compute_volume_farming_cost  # noqa: E402
from research_track03_full_universe_speedbump_mm import (  # noqa: E402
    FROZEN_STAGE_B_PARAMS,
    VENUE_SYMBOL_MAP,
    SpeedbumpArmConfig,
    _OpenLot,
    _RefLeadState,
    _RestingOrder,
    _campaign_parquet_files,
    _infer_tick_from_prices,
    _round_tick,
    _window_sort_key,
    prepare_replay_stream,
    simulate_prepared_arm,
)
from research_track03_patient_profit_frontier import (  # noqa: E402
    POSITIVE_MARKOUT_CORE_4,
    assert_no_w28_leak,
    discover_campaigns_w27,
)
from track_fee_evidence import (  # noqa: E402
    VENUE_CAPABILITY_SNAPSHOT_VERSION,
    build_fee_provenance,
    load_track_fee_evidence,
    resolve_venue_fee,
)

CORE_4_SYMBOLS: tuple[str, ...] = POSITIVE_MARKOUT_CORE_4  # ("AAPL", "AMZN", "META", "MSFT")
EX_META_SYMBOLS: tuple[str, ...] = ("AAPL", "AMZN", "MSFT")
TARGET_VENUE = "lighter_robinhood"
TARGET_SETTLEMENT = "USDG"
PRIMARY_LATENCY_TRIPLE: tuple[int, int, int] = (200, 300, 330)
EXPECTED_FEE_EVIDENCE_SHA256 = "4d3a9c1aeae5358bb9c55ab448db392ac3edb0a89244f74c23d89817c3359d92"
DEFAULT_PREREG_BOOTSTRAP_SEED = 20261127
DEFAULT_PREREG_BLOCK_LEN = 3.0
DEFAULT_PREREG_N_BOOT = 10_000
TEN_HOURS_MS = 36_000_000
MAX_CONTINUOUS_GAP_MS = 60_000

_S4_BASE: dict[str, Any] = dict(FROZEN_STAGE_B_PARAMS["S4_speedbump_combined"])

HOLD_LADDER_ARMS: tuple[str, ...] = (
    "S4_speedbump_combined",
    "S4_hold60s",
    "S4_hold180s",
    "S4_hold300s",
    "S4_hold600s_only",
)

COMPONENT_ATTRIBUTION_ARMS: tuple[str, ...] = (
    "S4_hold600s_only",
    "S4_hold600s_penny1",
    "S4_hold600s_skew5",
    "P3_patient_600s",
    "W5_nopenn_15bp_180s",
)

CORE4_ABLATION_ARMS_SPEC: dict[str, dict[str, Any]] = {
    # Pure single-factor hold ladder (quotes, skew=2.0, penny=0, o200_c300_e330, queue, drain locked to S4)
    "S4_speedbump_combined": {
        **_S4_BASE,
        "max_hold_ms": 15_000,
    },
    "S4_hold60s": {
        **_S4_BASE,
        "max_hold_ms": 60_000,
    },
    "S4_hold180s": {
        **_S4_BASE,
        "max_hold_ms": 180_000,
    },
    "S4_hold300s": {
        **_S4_BASE,
        "max_hold_ms": 300_000,
    },
    "S4_hold600s_only": {
        **_S4_BASE,
        "max_hold_ms": 600_000,
        "reduce_only_penny_ticks": 0,
        "inventory_skew_bps_per_full_limit": 2.0,
    },
    # Component attribution arms isolating penny=1 and skew=5.0 vs P3_patient_600s & W5
    "S4_hold600s_penny1": {
        **_S4_BASE,
        "max_hold_ms": 600_000,
        "reduce_only_penny_ticks": 1,
        "inventory_skew_bps_per_full_limit": 2.0,
    },
    "S4_hold600s_skew5": {
        **_S4_BASE,
        "max_hold_ms": 600_000,
        "reduce_only_penny_ticks": 0,
        "inventory_skew_bps_per_full_limit": 5.0,
    },
    "P3_patient_600s": {
        **_S4_BASE,
        "max_hold_ms": 600_000,
        "reduce_only_penny_ticks": 1,
        "inventory_skew_bps_per_full_limit": 5.0,
    },
    "W5_nopenn_15bp_180s": {
        **_S4_BASE,
        "max_hold_ms": 180_000,
        "min_entry_edge_bps": 1.50,
        "latency_vol_spread_mult": 2.50,
        "vol_spread_mult": 1.00,
        "reduce_only_penny_ticks": 0,
        "inventory_skew_bps_per_full_limit": 4.0,
    },
}


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as fh:
        for chunk in iter(lambda: fh.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def canonical_json_sha256(obj: Any) -> str:
    encoded = json.dumps(obj, sort_keys=True, separators=(",", ":"), allow_nan=False).encode("utf-8")
    return hashlib.sha256(encoded).hexdigest()


def compute_quantiles(sorted_vals: Sequence[float | int], probs: Sequence[float]) -> dict[str, float | None]:
    """Compute exact linear-interpolated quantiles from an already sorted sequence."""
    n = len(sorted_vals)
    if n == 0:
        return {f"p{int(round(p * 100))}": None for p in probs}
    out: dict[str, float | None] = {}
    for p in probs:
        label = f"p{int(round(p * 100))}"
        if n == 1:
            out[label] = float(sorted_vals[0])
            continue
        pos = p * (n - 1)
        lo = math.floor(pos)
        hi = math.ceil(pos)
        frac = pos - lo
        val = float(sorted_vals[lo]) * (1.0 - frac) + float(sorted_vals[hi]) * frac
        out[label] = round(val, 2)
    return out


def build_core4_ablation_arms(
    latency_triple: tuple[int, int, int] = PRIMARY_LATENCY_TRIPLE,
    *,
    maker_fee_bps: float = 0.0,
    taker_fee_bps: float = 0.0,
    rebate_bps: float = 0.0,
    arm_names: Sequence[str] | None = None,
) -> list[SpeedbumpArmConfig]:
    """Instantiate SpeedbumpArmConfig instances for the Core 4 single-factor + attribution suite."""
    selected = list(arm_names) if arm_names is not None else list(CORE4_ABLATION_ARMS_SPEC.keys())
    unknown = sorted(set(selected) - set(CORE4_ABLATION_ARMS_SPEC.keys()))
    if unknown:
        raise ValueError(f"Unknown arm(s): {unknown}; valid: {sorted(CORE4_ABLATION_ARMS_SPEC.keys())}")
    o_ms, c_ms, e_ms = latency_triple
    arms: list[SpeedbumpArmConfig] = []
    for name in selected:
        spec = dict(CORE4_ABLATION_ARMS_SPEC[name])
        use_sb_cancel = bool(spec.pop("use_speed_bump_cancel_ms", False))
        if use_sb_cancel:
            spec["speed_bump_cancel_ms"] = float(c_ms)
        arms.append(
            SpeedbumpArmConfig(
                name=name,
                order_delay_ms=o_ms,
                cancel_delay_ms=c_ms,
                exit_delay_ms=e_ms,
                maker_fee_bps=maker_fee_bps,
                taker_fee_bps=taker_fee_bps,
                rebate_bps=rebate_bps,
                **spec,
            )
        )
    return arms


def resolve_core4_rh_fee_provenance(fee_evidence_path: Path | None) -> tuple[float, float, str, str, dict[str, Any]]:
    """Load OPT-01 fee evidence and enforce expected SHA-256 & 0/0 verified lighter_robinhood tier."""
    if fee_evidence_path is not None and not fee_evidence_path.is_file():
        raise FileNotFoundError(f"Missing fee evidence file: {fee_evidence_path}")
    ev = load_track_fee_evidence(fee_evidence_path)
    mk, tk, st, src = resolve_venue_fee(
        TARGET_VENUE,
        "rwa",
        default_maker_bps=0.0,
        default_taker_bps=0.0,
        default_status="verified_account_tier" if ev.get("loaded") else "public_tier0_unverified",
        evidence=ev,
    )
    prov = build_fee_provenance(ev, venues_in_scope=(TARGET_VENUE, "arcus", "trade_xyz"))
    if fee_evidence_path is not None:
        digest = sha256_file(fee_evidence_path)
        prov["fee_evidence_sha256"] = digest
        prov["expected_fee_evidence_sha256"] = EXPECTED_FEE_EVIDENCE_SHA256
        prov["fee_evidence_sha256_matches"] = digest == EXPECTED_FEE_EVIDENCE_SHA256
    prov["target_venue"] = TARGET_VENUE
    prov["target_settlement"] = TARGET_SETTLEMENT
    prov["target_maker_fee_bps"] = float(mk)
    prov["target_taker_fee_bps"] = float(tk)
    prov["target_account_fee_status"] = st
    prov["target_fee_source"] = src
    return float(mk), float(tk), st, src, prov


def simulate_core4_arm(
    stream: Any,
    tick: float,
    arm: SpeedbumpArmConfig,
    *,
    markout_horizons_ms: tuple[int, ...] = (1_000, 5_000, 10_000),
    declared_end_ms: int | None = None,
) -> dict[str, Any]:
    """Run exact P0-fixed simulator with additive hold-quantile, gross-exposure, and ledger telemetry.

    Trading logic, queue FIFO, fills, cash, fees, `declared_end_ms`, `stale_exit_blocks`, and
    `330 ms` IOC re-queueing match `research_track03_full_universe_speedbump_mm.simulate_prepared_arm`
    bit-for-bit (asserted by parity checks in `--self-test`, unit tests, and lakehouse validation).
    """
    if tick <= 0.0 or not math.isfinite(tick):
        raise ValueError(f"tick must be finite and > 0, got {tick}")
    if not (0.0 <= arm.book_cross_toxic_fill_frac <= 1.0) or not math.isfinite(arm.book_cross_toxic_fill_frac):
        raise ValueError(f"book_cross_toxic_fill_frac must be in [0, 1], got {arm.book_cross_toxic_fill_frac}")
    if arm.book_cross_mode not in ("strict_through", "opposite_touch", "level_evicted"):
        raise ValueError(f"Unsupported book_cross_mode {arm.book_cross_mode!r}")
    if declared_end_ms is not None:
        if isinstance(declared_end_ms, bool) or not isinstance(declared_end_ms, (int, float)) or not math.isfinite(declared_end_ms):
            raise ValueError(f"declared_end_ms must be finite integer ms or None, got {declared_end_ms!r}")
        declared_end_ms = int(declared_end_ms)

    tuples = stream.tuples
    mid_times = stream.mid_times
    mid_vals = stream.mid_vals
    has_trade_events = stream.has_trade_events
    has_ref_events = stream.has_ref_events
    evidence_mode = "public_trades" if has_trade_events else "no_trades_in_window"

    bisect_right = bisect.bisect_right
    cutoff_mid_idx = (
        (bisect_right(mid_times, declared_end_ms) - 1)
        if (declared_end_ms is not None and mid_times)
        else (len(mid_times) - 1)
    )

    if not tuples or not mid_times or cutoff_mid_idx < 0:
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
            "realized_net_usd": 0.0,
            "mtm_net_usd": 0.0,
            "spr": 0.0,
            "drft": 0.0,
            "pex": 0.0,
            "tslp": 0.0,
            "gross_usd": 0.0,
            "unrealized_mtm_usd": 0.0,
            "mtm_gross_usd": 0.0,
            "gross_ex_fee_bps": 0.0,
            "fee_usd": 0.0,
            "cyc": 0,
            "can": 0,
            "haz": 0,
            "qpres": 0,
            "ntl": 0.0,
            "maker_volume_usd": 0.0,
            "taker_volume_usd": 0.0,
            "target_executed_fill_notional_usd": 0.0,
            "taker_exit_share_s": 0.0,
            "ledger_mtm_usd_per_m": None,
            "ledger_realized_usd_per_m": None,
            "double_sided_usd_per_m": None,
            "realized_only_usd_per_m": None,
            "mtm_incl_residual_usd_per_m": None,
            "maker_volume_usd_per_m": None,
            "terminal_residual_notional_usd": 0.0,
            "max_gross_exposure_usd": 0.0,
            "depth_clipped_exits": 0,
            "stale_book_cancels": 0,
            "stale_exit_blocks": 0,
            "toxicity_pauses": 0,
            "avg_hold_ms": 0.0,
            "hold_weighted_sum_ms": 0.0,
            "max_hold_observed_ms": 0,
            "hold_p50_ms": None,
            "hold_p95_ms": None,
            "hold_p99_ms": None,
            "hold_max_ms": None,
            "closed_lot_count": 0,
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
            "unprinted_book_cross_fills": 0,
            "unprinted_book_cross_notional_usd": 0.0,
            "first_mid": None,
            "last_mid": None,
            "underlying_return_bps": None,
            "terminal_flat": True,
            "_hold_samples_ms": [],
            "_exposure_timeline": [],
            "_exact_cash_ex_fee_usd": 0.0,
            "_exact_realized_gross_usd": 0.0,
            "_exact_mtm_gross_usd": 0.0,
            "_exact_fee_usd": 0.0,
            "_exact_net_usd": 0.0,
        }

    last_mid_ts = mid_times[cutoff_mid_idx]
    if declared_end_ms is not None:
        effective_end_ms = declared_end_ms
    else:
        target_end_ts: int | None = None
        for t in reversed(tuples):
            if t[1] != 1:
                target_end_ts = t[0]
                break
        effective_end_ms = target_end_ts if target_end_ts is not None else tuples[-1][0]
    terminal_drain_start_ts = (
        effective_end_ms - arm.terminal_flatten_lead_ms if arm.terminal_flatten_lead_ms > 0 else effective_end_ms + 1
    )

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
    last_book_ts: int | None = None
    max_book_age_ms = arm.max_book_age_ms

    def book_is_fresh(now_ms: int) -> bool:
        return has_book and last_book_ts is not None and 0 <= (now_ms - last_book_ts) <= max_book_age_ms

    ref_books: dict[str, tuple[int, float]] = {}
    ref_lead_states: dict[str, _RefLeadState] = {}
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

    symbol_toxicity_ewma_bps = 0.0
    symbol_toxicity_fills = 0
    toxicity_pause_until_ms = 0
    toxicity_pauses = 0

    tfi_signed_vol = 0.0
    tfi_total_vol = 0.0
    last_tfi_ts: int | None = None

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

    depth_clipped_exits = 0
    stale_book_cancels = 0
    stale_exit_blocks = 0
    hold_weighted_sum_ms = 0.0
    hold_closed_notl = 0.0
    max_hold_observed_ms = 0
    hold_samples_ms: list[int] = []
    max_gross_exposure_usd = 0.0
    exposure_timeline: list[tuple[int, float]] = []
    last_recorded_exp_usd = -1.0

    cancel_in_flight_fills = 0
    cancel_in_flight_notional_usd = 0.0
    unprinted_book_cross_fills = 0
    unprinted_book_cross_notional_usd = 0.0
    first_mid: float | None = None
    last_mid: float | None = None
    last_maker_fill_ts_bid: int | None = None
    last_maker_fill_ts_ask: int | None = None
    maker_fills_log: list[tuple[int, str, float, float]] = []

    tol_tick_25 = tick * 0.25
    tol_tick_10 = tick * 0.10
    exp_fn = math.exp
    tfi_tau = arm.tfi_ewma_tau_ms
    sf_bps = arm.self_fill_stepback_bps
    sf_decay_ms = arm.self_fill_decay_ms
    latency_vol_half_bps_factor = (
        arm.latency_vol_spread_mult * math.sqrt(arm.speed_bump_cancel_ms / 1_000.0)
        if (arm.speed_bump_cancel_ms > 0.0 and arm.latency_vol_spread_mult > 0.0)
        else 0.0
    )

    def _record_exposure(ts_ms: int, exp_usd: float, *, force: bool = False) -> None:
        nonlocal max_gross_exposure_usd, last_recorded_exp_usd
        if exp_usd > max_gross_exposure_usd:
            max_gross_exposure_usd = exp_usd
        if force or abs(exp_usd - last_recorded_exp_usd) >= 0.50:
            if exposure_timeline and exposure_timeline[-1][0] == ts_ms:
                exposure_timeline[-1] = (ts_ms, exp_usd)
            else:
                exposure_timeline.append((ts_ms, exp_usd))
            last_recorded_exp_usd = exp_usd

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
        nonlocal last_maker_fill_ts_bid, last_maker_fill_ts_ask
        nonlocal hold_weighted_sum_ms, hold_closed_notl, max_hold_observed_ms
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
            closed_notl = closed_qty * fill_px
            ntl += closed_notl
            hold_ms = max(0, ts_ms - lot.entry_ts)
            hold_weighted_sum_ms += closed_notl * hold_ms
            hold_closed_notl += closed_notl
            hold_samples_ms.append(hold_ms)
            if hold_ms > max_hold_observed_ms:
                max_hold_observed_ms = hold_ms
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
        _record_exposure(ts_ms, abs(pos_qty) * mid_now, force=True)

    def activate_arrivals(now_ms: int) -> None:
        nonlocal bid_ord, ask_ord, alo_rejected
        if not book_is_fresh(now_ms):
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
        if not book_is_fresh(ts_ms):
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

    def cancel_resting_sides(now_ms: int, *, cancel_bid: bool = True, cancel_ask: bool = True) -> None:
        nonlocal can
        if cancel_bid and bid_ord is not None and bid_ord.cancel_effective_ms is None:
            bid_ord.cancel_requested_ms = now_ms
            bid_ord.cancel_effective_ms = now_ms + arm.cancel_delay_ms
            can += 1
        if cancel_ask and ask_ord is not None and ask_ord.cancel_effective_ms is None:
            ask_ord.cancel_requested_ms = now_ms
            ask_ord.cancel_effective_ms = now_ms + arm.cancel_delay_ms
            can += 1

    def decide(now_ms: int, sigma_bps: float) -> None:
        nonlocal ewma_basis, last_basis_ts, bid_ord, ask_ord, can, orders_placed, qpres
        nonlocal stale_book_cancels
        if not has_book:
            return
        if bid_ord is not None and bid_ord.cancel_effective_ms is not None and now_ms >= bid_ord.cancel_effective_ms:
            bid_ord = None
        if ask_ord is not None and ask_ord.cancel_effective_ms is not None and now_ms >= ask_ord.cancel_effective_ms:
            ask_ord = None

        if not book_is_fresh(now_ms):
            had_active = (bid_ord is not None and bid_ord.cancel_effective_ms is None) or (
                ask_ord is not None and ask_ord.cancel_effective_ms is None
            )
            cancel_resting_sides(now_ms)
            if had_active:
                stale_book_cancels += 1
            return

        bb = cur_bb
        ba = cur_ba
        venue_mid = 0.5 * (bb + ba)
        spread_bps = (ba - bb) / venue_mid * 1e4
        sp_ticks = max(1, round((ba - bb) / tick))
        in_terminal_drain = now_ms >= terminal_drain_start_ts

        l1_sum = cur_bsz + cur_asz
        if l1_sum > 0.0 and venue_mid > 0.0:
            microprice = (bb * cur_asz + ba * cur_bsz) / l1_sum
            microprice_offset_bps = ((microprice / venue_mid) - 1.0) * 1e4
        else:
            microprice_offset_bps = 0.0

        if tfi_tau > 0.0:
            decay_tfi_to(now_ms)
            tfi_norm = max(-1.0, min(1.0, tfi_signed_vol / tfi_total_vol)) if tfi_total_vol > 1e-12 else 0.0
        else:
            tfi_norm = 0.0

        fair = venue_mid
        stale_suppress_side: str | None = None
        if arm.use_oracle_anchor:
            fresh_ref_items = [
                (rv, m) for rv, (t, m) in ref_books.items() if 0 <= (now_ms - t) <= arm.oracle_max_age_ms
            ]
            if arm.cross_venue_fair_weight > 0.0:
                validated_mids: list[float] = []
                for rv, rm in fresh_ref_items:
                    st = ref_lead_states.get(rv)
                    if st is None:
                        st = _RefLeadState()
                        ref_lead_states[rv] = st
                    st.observe_and_mature(now_ms, rm, venue_mid)
                    if st.is_validated():
                        validated_mids.append(rm)
                candidate_mids = validated_mids if validated_mids else [m for _, m in fresh_ref_items]
            else:
                candidate_mids = [m for _, m in fresh_ref_items]

            if candidate_mids:
                if len(candidate_mids) == 1:
                    ref_mid = candidate_mids[0]
                else:
                    s_mids = sorted(candidate_mids)
                    mid_i = len(s_mids) // 2
                    ref_mid = 0.5 * (s_mids[mid_i - 1] + s_mids[mid_i]) if len(s_mids) % 2 == 0 else s_mids[mid_i]
                raw_basis = venue_mid - ref_mid
                if last_basis_ts is not None and 0 <= now_ms - last_basis_ts <= arm.oracle_max_age_ms and ewma_basis is not None:
                    alpha = 1.0 - exp_fn(-(now_ms - last_basis_ts) / arm.oracle_basis_tau_ms)
                    ewma_basis = ewma_basis + alpha * (raw_basis - ewma_basis)
                else:
                    ewma_basis = raw_basis
                last_basis_ts = now_ms
                ref_adj = ref_mid + ewma_basis
                if ref_adj > 0.0:
                    if arm.cross_venue_fair_weight > 0.0:
                        raw_shift_bps = math.log(ref_adj / venue_mid) * 1e4
                        weighted_shift = arm.cross_venue_fair_weight * raw_shift_bps
                        if arm.cross_venue_fair_cap_bps > 0.0:
                            clamped_shift = max(
                                -arm.cross_venue_fair_cap_bps, min(arm.cross_venue_fair_cap_bps, weighted_shift)
                            )
                        else:
                            clamped_shift = weighted_shift
                        fair = venue_mid * exp_fn(clamped_shift * 1e-4)
                    else:
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
        toxicity_paused = (
            arm.toxicity_pause_ewma_bps > 0.0
            and arm.toxicity_pause_ms > 0
            and now_ms < toxicity_pause_until_ms
        )

        quiet_blocked = False
        if arm.quiet_regime_only:
            ratio = spread_bps / max(sigma_bps, arm.quiet_sigma_floor_bps)
            quiet_blocked = (ratio < arm.quiet_min_spread_vol_ratio) or (sigma_bps > arm.quiet_max_sigma_bps)

        min_edge_total = arm.min_entry_edge_bps + arm.maker_fee_bps
        vol_half_bps = arm.vol_spread_mult * sigma_bps
        latency_vol_half_bps = latency_vol_half_bps_factor * sigma_bps
        max_micro_skew = arm.microprice_alpha_weight * (spread_bps * 0.5) if arm.microprice_alpha_weight > 0.0 else 0.0
        reducing_edge_floor = (
            arm.maker_fee_bps - arm.inventory_skew_bps_per_full_limit - arm.tfi_skew_bps - max_micro_skew
        )

        # BID
        is_red_bid = pos_usd < -10.0
        if not is_red_bid and (
            in_terminal_drain
            or pos_usd >= arm.max_pos_usd
            or now_ms < markout_cd_bid
            or quiet_blocked
            or toxicity_paused
            or tfi_bid_vetoed
            or stale_suppress_side == "BID"
        ):
            if bid_ord is not None and bid_ord.cancel_effective_ms is None:
                bid_ord.cancel_requested_ms = now_ms
                bid_ord.cancel_effective_ms = now_ms + arm.cancel_delay_ms
                can += 1
        else:
            asym_b = (
                min(arm.asymmetric_markout_max_bps, (-markout_ewma_bid) * arm.directional_markout_skew_mult)
                if (arm.directional_markout_skew_mult > 0.0 and markout_ewma_bid < 0.0)
                else 0.0
            )
            sf_b = decayed_sf("BID", now_ms)
            open_req_b = min_edge_total + vol_half_bps + latency_vol_half_bps + asym_b + sf_b

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
                ousd = abs(pos_usd) if is_red_bid else arm.quote_usd * sc
                q_b = abs(pos_qty) if is_red_bid else (ousd / px_b)
                bid_ord = _RestingOrder("BID", px_b, q_b, q_b, 0.0, now_ms, now_ms + arm.order_delay_ms, False)
                orders_placed += 1

        # ASK
        is_red_ask = pos_usd > 10.0
        if not is_red_ask and (
            in_terminal_drain
            or pos_usd <= -arm.max_pos_usd
            or now_ms < markout_cd_ask
            or quiet_blocked
            or toxicity_paused
            or tfi_ask_vetoed
            or stale_suppress_side == "ASK"
        ):
            if ask_ord is not None and ask_ord.cancel_effective_ms is None:
                ask_ord.cancel_requested_ms = now_ms
                ask_ord.cancel_effective_ms = now_ms + arm.cancel_delay_ms
                can += 1
        else:
            asym_a = (
                min(arm.asymmetric_markout_max_bps, (-markout_ewma_ask) * arm.directional_markout_skew_mult)
                if (arm.directional_markout_skew_mult > 0.0 and markout_ewma_ask < 0.0)
                else 0.0
            )
            sf_a = decayed_sf("ASK", now_ms)
            open_req_a = min_edge_total + vol_half_bps + latency_vol_half_bps + asym_a + sf_a

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
                ousd = abs(pos_usd) if is_red_ask else arm.quote_usd * sc
                q_a = abs(pos_qty) if is_red_ask else (ousd / px_a)
                ask_ord = _RestingOrder("ASK", px_a, q_a, q_a, 0.0, now_ms, now_ms + arm.order_delay_ms, False)
                orders_placed += 1

    def process_pending_iocs(now_ms: int) -> None:
        nonlocal cur_bsz, cur_asz, haz, depth_clipped_exits, stale_exit_blocks, pending_ioc_exits
        if not pending_ioc_exits or not has_book:
            return
        fresh_now = book_is_fresh(now_ms)
        mid_now_ioc = 0.5 * (cur_bb + cur_ba)
        still: list[tuple[int, str, float]] = []
        for exec_ts, iside, iqty in pending_ioc_exits:
            if now_ms < exec_ts:
                still.append((exec_ts, iside, iqty))
                continue
            if not fresh_now:
                stale_exit_blocks += 1
                still.append((now_ms + arm.exit_delay_ms, iside, iqty))
                continue
            if iside == "SELL" and pos_qty > 1e-12:
                wanted = min(iqty, pos_qty)
                avail = max(0.0, cur_bsz)
                fill_qty = min(wanted, avail)
                if fill_qty < wanted - 1e-12:
                    depth_clipped_exits += 1
                if fill_qty > 1e-12:
                    cur_bsz = max(0.0, cur_bsz - fill_qty)
                    record_fill(now_ms, "SELL", cur_bb, fill_qty, "taker", mid_now_ioc)
                    haz += 1
            elif iside == "BUY" and pos_qty < -1e-12:
                wanted = min(iqty, -pos_qty)
                avail = max(0.0, cur_asz)
                fill_qty = min(wanted, avail)
                if fill_qty < wanted - 1e-12:
                    depth_clipped_exits += 1
                if fill_qty > 1e-12:
                    cur_asz = max(0.0, cur_asz - fill_qty)
                    record_fill(now_ms, "BUY", cur_ba, fill_qty, "taker", mid_now_ioc)
                    haz += 1
        pending_ioc_exits = still

    def maybe_queue_exit_ioc(now_ms: int) -> None:
        if not open_lots or not book_is_fresh(now_ms):
            return
        should_flatten = ((now_ms - open_lots[0].entry_ts) >= arm.max_hold_ms) or (now_ms >= terminal_drain_start_ts)
        if not should_flatten:
            return
        tside = "SELL" if pos_qty > 1e-12 else ("BUY" if pos_qty < -1e-12 else None)
        if tside is not None:
            pend_q = sum(q for _, s, q in pending_ioc_exits if s == tside)
            needed = abs(pos_qty) - pend_q
            if needed > 1e-12:
                pending_ioc_exits.append((now_ms + arm.exit_delay_ms, tside, needed))

    mk_thresh = arm.markout_shield_adverse_threshold_bps
    mk_cd_ms = arm.markout_shield_cooldown_ms

    for ts, kind, c2, c3, c4, c5 in tuples:
        if declared_end_ms is not None and ts > declared_end_ms:
            break
        if bid_ord is not None and bid_ord.cancel_effective_ms is not None and ts >= bid_ord.cancel_effective_ms:
            bid_ord = None
        if ask_ord is not None and ask_ord.cancel_effective_ms is not None and ts >= ask_ord.cancel_effective_ms:
            ask_ord = None

        process_pending_iocs(ts)
        maybe_queue_exit_ioc(ts)

        if kind == 0:
            cur_bb, cur_bsz, cur_ba, cur_asz = c2, c3, c4, c5
            has_book = True
            last_book_ts = ts
            activate_arrivals(ts)
            if bid_ord is not None and bid_ord.activated and abs(bid_ord.price - cur_bb) <= tol_tick_25 and cur_bsz < bid_ord.queue_ahead:
                bid_ord.queue_ahead = cur_bsz
            if ask_ord is not None and ask_ord.activated and abs(ask_ord.price - cur_ba) <= tol_tick_25 and cur_asz < ask_ord.queue_ahead:
                ask_ord.queue_ahead = cur_asz

            mid_now = 0.5 * (cur_bb + cur_ba)
            if mid_now > 0.0:
                if first_mid is None:
                    first_mid = mid_now
                last_mid = mid_now

            if arm.book_cross_toxic_fill_frac > 0.0 and mid_now > 0.0:
                if bid_ord is not None and bid_ord.activated and not bid_ord.toxic_cross_handled:
                    if arm.book_cross_mode == "strict_through":
                        bid_crossed = cur_ba < bid_ord.price - tol_tick_10
                    elif arm.book_cross_mode == "opposite_touch":
                        bid_crossed = cur_ba <= bid_ord.price + tol_tick_10
                    else:  # level_evicted
                        bid_crossed = cur_bb < bid_ord.price - tol_tick_10
                    if bid_crossed:
                        fill_qty = bid_ord.remaining * arm.book_cross_toxic_fill_frac
                        bid_ord.toxic_cross_handled = True
                        if fill_qty > 1e-12:
                            cif = bid_ord.cancel_effective_ms is not None and ts < bid_ord.cancel_effective_ms
                            record_fill(
                                ts,
                                "BUY",
                                bid_ord.price,
                                fill_qty,
                                "maker",
                                mid_now,
                                was_cancel_in_flight=cif,
                            )
                            unprinted_book_cross_fills += 1
                            unprinted_book_cross_notional_usd += fill_qty * bid_ord.price
                            bid_ord.remaining -= fill_qty
                        if bid_ord.remaining <= 1e-12:
                            bid_ord = None
                if ask_ord is not None and ask_ord.activated and not ask_ord.toxic_cross_handled:
                    if arm.book_cross_mode == "strict_through":
                        ask_crossed = cur_bb > ask_ord.price + tol_tick_10
                    elif arm.book_cross_mode == "opposite_touch":
                        ask_crossed = cur_bb >= ask_ord.price - tol_tick_10
                    else:  # level_evicted
                        ask_crossed = cur_ba > ask_ord.price + tol_tick_10
                    if ask_crossed:
                        fill_qty = ask_ord.remaining * arm.book_cross_toxic_fill_frac
                        ask_ord.toxic_cross_handled = True
                        if fill_qty > 1e-12:
                            cif = ask_ord.cancel_effective_ms is not None and ts < ask_ord.cancel_effective_ms
                            record_fill(
                                ts,
                                "SELL",
                                ask_ord.price,
                                fill_qty,
                                "maker",
                                mid_now,
                                was_cancel_in_flight=cif,
                            )
                            unprinted_book_cross_fills += 1
                            unprinted_book_cross_notional_usd += fill_qty * ask_ord.price
                            ask_ord.remaining -= fill_qty
                        if ask_ord.remaining <= 1e-12:
                            ask_ord = None

            maybe_queue_exit_ioc(ts)

            if abs(pos_qty) > 1e-12:
                _record_exposure(ts, abs(pos_qty) * mid_now)
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

                    if math.isfinite(mk_bps):
                        if symbol_toxicity_fills == 0:
                            symbol_toxicity_ewma_bps = mk_bps
                        else:
                            symbol_toxicity_ewma_bps = 0.80 * symbol_toxicity_ewma_bps + 0.20 * mk_bps
                        symbol_toxicity_fills += 1
                        if (
                            arm.toxicity_pause_ewma_bps > 0.0
                            and arm.toxicity_pause_ms > 0
                            and symbol_toxicity_fills >= arm.toxicity_min_fills
                            and symbol_toxicity_ewma_bps < -arm.toxicity_pause_ewma_bps
                        ):
                            new_until = ts + arm.toxicity_pause_ms
                            if new_until > toxicity_pause_until_ms:
                                if ts >= toxicity_pause_until_ms:
                                    toxicity_pauses += 1
                                toxicity_pause_until_ms = new_until

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

            if not book_is_fresh(ts):
                continue
            activate_arrivals(ts)
            handle_trade(ts, c2, c3, c4)
            if tfi_tau > 0.0 and (arm.tfi_skew_bps > 0.0 or arm.tfi_veto_threshold > 0.0):
                decide(ts, last_sigma_bps)
            maybe_queue_exit_ioc(ts)

    mid_end = 0.5 * (cur_bb + cur_ba) if has_book else 0.0
    terminal_residual_notional_usd = abs(pos_qty) * mid_end if has_book else 0.0
    if has_book and abs(pos_qty) > 1e-12:
        _record_exposure(effective_end_ms, terminal_residual_notional_usd, force=True)
    unrealized_mtm_usd = (
        sum(lot.sgn * lot.qty * (mid_end - lot.entry_px) for lot in open_lots) if has_book else 0.0
    )
    terminal_flat = len(open_lots) == 0 and abs(pos_qty) <= 1e-12

    realized_gross_usd = spr + drft + pex + tslp
    realized_net_usd = realized_gross_usd - fee_usd
    mtm_gross_usd = realized_gross_usd + unrealized_mtm_usd
    mtm_net_usd = mtm_gross_usd - fee_usd

    realized_gross_bp = (realized_gross_usd / ntl) * 1e4 if ntl > 0.0 else 0.0
    mtm_gross_bp = (mtm_gross_usd / ntl) * 1e4 if ntl > 0.0 else 0.0
    net_per_ntl_bp = (mtm_net_usd / ntl) * 1e4 if ntl > 0.0 else 0.0
    taker_exit_share_s = min(1.0, max(0.0, taker_volume_usd / ntl)) if ntl > 0.0 else 0.0

    target_exec_notl = maker_volume_usd + taker_volume_usd
    ledger_mtm_usd_per_m = round((-mtm_net_usd / target_exec_notl) * 1e6, 4) if target_exec_notl > 0.0 else None
    ledger_realized_usd_per_m = (
        round((-realized_net_usd / target_exec_notl) * 1e6, 4) if target_exec_notl > 0.0 else None
    )

    ds_mtm_usd_per_m: float | None = None
    ds_realized_usd_per_m: float | None = None
    maker_vol_usd_per_m: float | None = None
    if ntl > 0.0:
        realized_cost_bd = compute_volume_farming_cost(
            realized_gross_bp,
            taker_exit_share_s,
            arm.taker_fee_bps,
            maker_fee_bps=arm.maker_fee_bps,
            rebate_bps=arm.rebate_bps,
        )
        mtm_cost_bd = compute_volume_farming_cost(
            mtm_gross_bp,
            taker_exit_share_s,
            arm.taker_fee_bps,
            maker_fee_bps=arm.maker_fee_bps,
            rebate_bps=arm.rebate_bps,
        )
        ds_mtm_usd_per_m = mtm_cost_bd.double_sided_usd_per_m
        ds_realized_usd_per_m = realized_cost_bd.double_sided_usd_per_m
        maker_vol_usd_per_m = mtm_cost_bd.maker_volume_usd_per_m

    hold_samples_sorted = sorted(hold_samples_ms)
    hold_q = compute_quantiles(hold_samples_sorted, (0.50, 0.95, 0.99))
    hold_max = int(hold_samples_sorted[-1]) if hold_samples_sorted else None

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

    cif_share = (cancel_in_flight_notional_usd / maker_volume_usd) if maker_volume_usd > 0.0 else 0.0

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
        "net": round(mtm_net_usd, 8),
        "realized_net_usd": round(realized_net_usd, 8),
        "mtm_net_usd": round(mtm_net_usd, 8),
        "spr": round(spr, 8),
        "drft": round(drft, 8),
        "pex": round(pex, 8),
        "tslp": round(tslp, 8),
        "gross_usd": round(realized_gross_usd, 8),
        "unrealized_mtm_usd": round(unrealized_mtm_usd, 8),
        "mtm_gross_usd": round(mtm_gross_usd, 8),
        "gross_ex_fee_bps": round(mtm_gross_bp, 6),
        "fee_usd": round(fee_usd, 8),
        "cyc": cyc,
        "can": can,
        "haz": haz,
        "qpres": qpres,
        "ntl": round(ntl, 6),
        "maker_volume_usd": round(maker_volume_usd, 6),
        "taker_volume_usd": round(taker_volume_usd, 6),
        "target_executed_fill_notional_usd": round(target_exec_notl, 6),
        "taker_exit_share_s": round(taker_exit_share_s, 6),
        "ledger_mtm_usd_per_m": ledger_mtm_usd_per_m,
        "ledger_realized_usd_per_m": ledger_realized_usd_per_m,
        "double_sided_usd_per_m": ds_mtm_usd_per_m,
        "realized_only_usd_per_m": ds_realized_usd_per_m,
        "mtm_incl_residual_usd_per_m": ds_mtm_usd_per_m,
        "maker_volume_usd_per_m": maker_vol_usd_per_m,
        "terminal_residual_notional_usd": round(terminal_residual_notional_usd, 6),
        "max_gross_exposure_usd": round(max_gross_exposure_usd, 4),
        "depth_clipped_exits": depth_clipped_exits,
        "stale_book_cancels": stale_book_cancels,
        "stale_exit_blocks": stale_exit_blocks,
        "toxicity_pauses": toxicity_pauses,
        "avg_hold_ms": round(hold_weighted_sum_ms / hold_closed_notl, 2) if hold_closed_notl > 0.0 else 0.0,
        "hold_weighted_sum_ms": round(hold_weighted_sum_ms, 2),
        "max_hold_observed_ms": max_hold_observed_ms,
        "hold_p50_ms": hold_q["p50"],
        "hold_p95_ms": hold_q["p95"],
        "hold_p99_ms": hold_q["p99"],
        "hold_max_ms": hold_max,
        "closed_lot_count": len(hold_samples_sorted),
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
        "unprinted_book_cross_fills": unprinted_book_cross_fills,
        "unprinted_book_cross_notional_usd": round(unprinted_book_cross_notional_usd, 6),
        "first_mid": round(first_mid, 8) if first_mid is not None else None,
        "last_mid": round(last_mid, 8) if last_mid is not None else None,
        "underlying_return_bps": (
            round(((last_mid / first_mid) - 1.0) * 1e4, 6)
            if (first_mid is not None and last_mid is not None and first_mid > 0.0)
            else None
        ),
        "terminal_flat": terminal_flat,
        "_hold_samples_ms": hold_samples_sorted,
        "_exposure_timeline": exposure_timeline,
        "_exact_cash_ex_fee_usd": cash_ex_fee_usd + (pos_qty * mid_end if has_book else 0.0),
        "_exact_realized_gross_usd": realized_gross_usd,
        "_exact_mtm_gross_usd": mtm_gross_usd,
        "_exact_fee_usd": fee_usd,
        "_exact_net_usd": mtm_net_usd,
    }


def compute_portfolio_max_gross_exposure(timelines: Sequence[Sequence[tuple[int, float]]]) -> float:
    """Compute exact simultaneous max sum(|pos_usd_sym|) across symbol exposure step timelines."""
    events: list[tuple[int, int, float]] = []
    for idx, tl in enumerate(timelines):
        for ts, exp_usd in tl:
            events.append((int(ts), idx, float(exp_usd)))
    if not events:
        return 0.0
    events.sort(key=lambda item: item[0])
    cur = [0.0] * len(timelines)
    cur_sum = 0.0
    peak = 0.0
    i = 0
    n = len(events)
    while i < n:
        ts = events[i][0]
        while i < n and events[i][0] == ts:
            _, s_idx, val = events[i]
            cur_sum += val - cur[s_idx]
            cur[s_idx] = val
            i += 1
        if cur_sum > peak:
            peak = cur_sum
    return round(peak, 4)


def aggregate_core4_bucket(
    grp: Sequence[dict[str, Any]],
    *,
    include_symbol: bool = False,
) -> dict[str, Any]:
    """Aggregate simulation rows under Roadmap §2/§4 ledger + double-sided + tail-hold + exposure accounting."""
    first = grp[0]
    spr = sum(float(r["spr"]) for r in grp)
    drft = sum(float(r["drft"]) for r in grp)
    pex = sum(float(r["pex"]) for r in grp)
    tslp = sum(float(r["tslp"]) for r in grp)
    realized_gross_usd = spr + drft + pex + tslp
    unrealized_mtm_usd = sum(float(r.get("unrealized_mtm_usd", 0.0)) for r in grp)
    mtm_gross_usd = realized_gross_usd + unrealized_mtm_usd
    fee_usd = sum(float(r["fee_usd"]) for r in grp)
    realized_net_usd = realized_gross_usd - fee_usd
    mtm_net_usd = mtm_gross_usd - fee_usd

    realized_cash_cost_usd = -realized_net_usd
    mtm_cash_cost_usd = -mtm_net_usd

    ntl = sum(float(r["ntl"]) for r in grp)
    maker_vol = sum(float(r["maker_volume_usd"]) for r in grp)
    taker_vol = sum(float(r["taker_volume_usd"]) for r in grp)
    exec_notl = sum(float(r.get("target_executed_fill_notional_usd", r["maker_volume_usd"] + r["taker_volume_usd"])) for r in grp)
    res_notl = sum(float(r.get("terminal_residual_notional_usd", 0.0)) for r in grp)
    depth_clipped = sum(int(r.get("depth_clipped_exits", 0)) for r in grp)
    stale_cancels = sum(int(r.get("stale_book_cancels", 0)) for r in grp)
    stale_blocks = sum(int(r.get("stale_exit_blocks", 0)) for r in grp)
    tox_pauses = sum(int(r.get("toxicity_pauses", 0)) for r in grp)
    hold_wsum = sum(float(r.get("hold_weighted_sum_ms", 0.0)) for r in grp)
    max_hold_obs = max((int(r.get("max_hold_observed_ms", 0)) for r in grp), default=0)

    mk_fee = float(first["maker_fee_bps"])
    tk_fee = float(first["taker_fee_bps"])
    reb_bp = float(first["rebate_bps"])

    # Zero-rebate net cash cost adds back any maker rebate credit included in fee_usd.
    rebate_credit_usd = maker_vol * (reb_bp * 1e-4)
    zero_rebate_mtm_cash_cost_usd = mtm_cash_cost_usd + rebate_credit_usd
    zero_rebate_realized_cash_cost_usd = realized_cash_cost_usd + rebate_credit_usd

    # Roadmap §2 Primary Ledger $/M: sum(net_cash_cost) / sum(target_executed_fill_notional_usd) * 1e6
    ledger_mtm_usd_per_m = round((mtm_cash_cost_usd / exec_notl) * 1e6, 4) if exec_notl > 0.0 else None
    ledger_realized_usd_per_m = round((realized_cash_cost_usd / exec_notl) * 1e6, 4) if exec_notl > 0.0 else None
    zero_rebate_ledger_mtm_usd_per_m = (
        round((zero_rebate_mtm_cash_cost_usd / exec_notl) * 1e6, 4) if exec_notl > 0.0 else None
    )
    zero_rebate_ledger_realized_usd_per_m = (
        round((zero_rebate_realized_cash_cost_usd / exec_notl) * 1e6, 4) if exec_notl > 0.0 else None
    )

    # Historical double-sided cycle-model convention (2 * ntl), clearly labeled.
    realized_gbp = (realized_gross_usd / ntl) * 1e4 if ntl > 0.0 else 0.0
    mtm_gbp = (mtm_gross_usd / ntl) * 1e4 if ntl > 0.0 else 0.0
    net_bp = (mtm_net_usd / ntl) * 1e4 if ntl > 0.0 else 0.0
    s_share = min(1.0, max(0.0, taker_vol / ntl)) if ntl > 0.0 else 0.0
    hist_ds_realized: float | None = None
    hist_ds_mtm: float | None = None
    hist_ds_zero_rebate: float | None = None
    hist_maker_vol_per_m: float | None = None
    if ntl > 0.0:
        hist_ds_realized = compute_volume_farming_cost(
            realized_gbp, s_share, tk_fee, maker_fee_bps=mk_fee, rebate_bps=reb_bp
        ).double_sided_usd_per_m
        mtm_bd = compute_volume_farming_cost(
            mtm_gbp, s_share, tk_fee, maker_fee_bps=mk_fee, rebate_bps=reb_bp
        )
        hist_ds_mtm = mtm_bd.double_sided_usd_per_m
        hist_maker_vol_per_m = mtm_bd.maker_volume_usd_per_m
        hist_ds_zero_rebate = compute_volume_farming_cost(
            mtm_gbp, s_share, tk_fee, maker_fee_bps=mk_fee, rebate_bps=0.0
        ).double_sided_usd_per_m

    pooled_hold_samples: list[int] = []
    for r in grp:
        hs = r.get("_hold_samples_ms")
        if hs:
            pooled_hold_samples.extend(int(v) for v in hs)
    pooled_hold_samples.sort()
    hold_q = compute_quantiles(pooled_hold_samples, (0.50, 0.95, 0.99))
    hold_max_ms = int(pooled_hold_samples[-1]) if pooled_hold_samples else (max_hold_obs if ntl > 0.0 else None)

    by_win: dict[str, list[dict[str, Any]]] = defaultdict(list)
    for r in grp:
        by_win[str(r["win"])].append(r)

    cum_net = 0.0
    peak_net = 0.0
    max_dd_usd = 0.0
    pos_wins = 0
    zero_fill_wins = 0
    total_hours = 0.0
    win_net_series: list[float] = []
    win_records: list[dict[str, Any]] = []
    worst_win: dict[str, Any] | None = None
    max_port_gross_exp = 0.0
    max_sym_gross_exp = max((float(r.get("max_gross_exposure_usd", 0.0)) for r in grp), default=0.0)
    sum_sym_max_gross_exp = 0.0

    for win_id in sorted(by_win.keys(), key=_window_sort_key):
        wgrp = by_win[win_id]
        w_mtm_net = sum(float(x["mtm_net_usd"]) for x in wgrp)
        w_real_net = sum(float(x["realized_net_usd"]) for x in wgrp)
        w_ntl = sum(float(x["ntl"]) for x in wgrp)
        w_mvol = sum(float(x["maker_volume_usd"]) for x in wgrp)
        w_tvol = sum(float(x["taker_volume_usd"]) for x in wgrp)
        w_exec_notl = sum(
            float(x.get("target_executed_fill_notional_usd", x["maker_volume_usd"] + x["taker_volume_usd"]))
            for x in wgrp
        )
        w_fills = sum(int(x.get("fills", 0)) for x in wgrp)
        w_res = sum(float(x.get("terminal_residual_notional_usd", 0.0)) for x in wgrp)
        w_s = min(1.0, max(0.0, w_tvol / w_ntl)) if w_ntl > 0.0 else 0.0

        # Exact simultaneous portfolio max gross exposure across symbols in this window
        timelines = [x.get("_exposure_timeline") or [] for x in wgrp]
        if any(timelines):
            w_port_exp = compute_portfolio_max_gross_exposure(timelines)
        else:
            w_port_exp = sum(float(x.get("max_gross_exposure_usd", 0.0)) for x in wgrp)
        max_port_gross_exp = max(max_port_gross_exp, w_port_exp)
        sum_sym_max_gross_exp = max(
            sum_sym_max_gross_exp,
            sum(float(x.get("max_gross_exposure_usd", 0.0)) for x in wgrp),
        )

        # Zero-fill windows stay in coverage denominator with unit_cost = None!
        w_ledger_mtm = round((-w_mtm_net / w_exec_notl) * 1e6, 4) if w_exec_notl > 0.0 else None
        w_ledger_real = round((-w_real_net / w_exec_notl) * 1e6, 4) if w_exec_notl > 0.0 else None
        w_ds_mtm = round((-w_mtm_net / (2.0 * w_ntl)) * 1e6, 4) if w_ntl > 0.0 else None
        if w_exec_notl <= 0.0 or w_fills == 0:
            zero_fill_wins += 1

        total_hours += max(float((x.get("continuity") or {}).get("hours", 0.0)) for x in wgrp)
        win_net_series.append(w_mtm_net)
        cum_net += w_mtm_net
        if cum_net > peak_net:
            peak_net = cum_net
        max_dd_usd = max(max_dd_usd, peak_net - cum_net)
        if w_mtm_net > 0.0:
            pos_wins += 1

        w_rec = {
            "win": win_id,
            "fills": w_fills,
            "mtm_net_usd": round(w_mtm_net, 6),
            "realized_net_usd": round(w_real_net, 6),
            "net_cash_cost_usd": round(-w_mtm_net, 6),
            "ntl": round(w_ntl, 4),
            "maker_volume_usd": round(w_mvol, 4),
            "taker_volume_usd": round(w_tvol, 4),
            "target_executed_fill_notional_usd": round(w_exec_notl, 4),
            "taker_exit_share_s": round(w_s, 6),
            "ledger_mtm_usd_per_m": w_ledger_mtm,
            "ledger_realized_usd_per_m": w_ledger_real,
            "historical_double_sided_mtm_usd_per_m": w_ds_mtm,
            "terminal_residual_notional_usd": round(w_res, 4),
            "portfolio_max_gross_exposure_usd": w_port_exp,
        }
        win_records.append(w_rec)
        if worst_win is None or w_mtm_net < worst_win["mtm_net_usd"]:
            worst_win = w_rec

    psr_zero: float | None = None
    if len(win_net_series) >= 3:
        mom = series_moments(win_net_series)
        psr_zero = round(probabilistic_sharpe_ratio(mom, 0.0), 6)

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

    cif_fills = sum(int(r.get("cancel_in_flight_fills", 0)) for r in grp)
    cif_notl = sum(float(r.get("cancel_in_flight_notional_usd", 0.0)) for r in grp)
    cif_share = (cif_notl / maker_vol) if maker_vol > 0.0 else 0.0
    ubx_fills = sum(int(r.get("unprinted_book_cross_fills", 0)) for r in grp)
    ubx_notl = sum(float(r.get("unprinted_book_cross_notional_usd", 0.0)) for r in grp)

    n_wins = len(by_win)
    out: dict[str, Any] = {
        "arm": first["arm"],
        "latency_label": first["latency_label"],
        "order_delay_ms": first["order_delay_ms"],
        "cancel_delay_ms": first["cancel_delay_ms"],
        "exit_delay_ms": first["exit_delay_ms"],
        "venue": first["venue"],
        "settlement": first["settlement"],
        "maker_fee_bps": mk_fee,
        "taker_fee_bps": tk_fee,
        "rebate_bps": reb_bp,
        "account_fee_status": first["account_fee_status"],
        "windows_covered": n_wins,
        "zero_fill_windows": zero_fill_wins,
        "active_fill_windows": n_wins - zero_fill_wins,
        "hours_covered": round(total_hours, 2),
        "rows": len(grp),
        "cont_rows": sum(1 for r in grp if r.get("cont")),
        "cyc": sum(int(r["cyc"]) for r in grp),
        "orders_placed": sum(int(r.get("orders_placed", 0)) for r in grp),
        "fills": sum(int(r["fills"]) for r in grp),
        "maker_fills": sum(int(r["maker_fills"]) for r in grp),
        "taker_exits": sum(int(r["taker_exits"]) for r in grp),
        "spr": round(spr, 6),
        "drft": round(drft, 6),
        "pex": round(pex, 6),
        "tslp": round(tslp, 6),
        "realized_gross_usd": round(realized_gross_usd, 6),
        "unrealized_mtm_usd": round(unrealized_mtm_usd, 6),
        "mtm_gross_usd": round(mtm_gross_usd, 6),
        "fee_usd": round(fee_usd, 6),
        "realized_net_usd": round(realized_net_usd, 6),
        "mtm_net_usd": round(mtm_net_usd, 6),
        "realized_net_cash_cost_usd": round(realized_cash_cost_usd, 6),
        "mtm_net_cash_cost_usd": round(mtm_cash_cost_usd, 6),
        "ntl": round(ntl, 4),
        "maker_volume_usd": round(maker_vol, 4),
        "taker_volume_usd": round(taker_vol, 4),
        "target_executed_fill_notional_usd": round(exec_notl, 4),
        "target_executed_fill_notional_per_window_usd": round(exec_notl / n_wins, 2) if n_wins > 0 else 0.0,
        "maker_volume_per_window_usd": round(maker_vol / n_wins, 2) if n_wins > 0 else 0.0,
        "gross_ex_fee_bps": round(mtm_gbp, 6),
        "net_per_ntl_bp": round(net_bp, 6),
        "taker_exit_share_s": round(s_share, 6),
        "natural_passive_exit_share": round(1.0 - s_share, 6),
        # Primary Roadmap §2 true ledger $/M
        "ledger_mtm_usd_per_m": ledger_mtm_usd_per_m,
        "ledger_realized_usd_per_m": ledger_realized_usd_per_m,
        "zero_rebate_ledger_mtm_usd_per_m": zero_rebate_ledger_mtm_usd_per_m,
        "zero_rebate_ledger_realized_usd_per_m": zero_rebate_ledger_realized_usd_per_m,
        # Clearly labeled historical double-sided cycle-model $/M
        "historical_double_sided_mtm_usd_per_m": hist_ds_mtm,
        "historical_double_sided_realized_usd_per_m": hist_ds_realized,
        "historical_double_sided_zero_rebate_usd_per_m": hist_ds_zero_rebate,
        "historical_maker_volume_usd_per_m": hist_maker_vol_per_m,
        # Risk, exposure, hold tail, and P0 safety counters
        "terminal_residual_notional_usd": round(res_notl, 4),
        "terminal_residual_per_window_usd": round(res_notl / n_wins, 4) if n_wins > 0 else 0.0,
        "max_gross_exposure_usd": round(max_port_gross_exp, 4),
        "max_symbol_gross_exposure_usd": round(max_sym_gross_exp, 4),
        "sum_symbol_max_gross_exposure_usd": round(sum_sym_max_gross_exp, 4),
        "avg_hold_ms": round(hold_wsum / ntl, 2) if ntl > 0.0 else 0.0,
        "avg_hold_s": round((hold_wsum / ntl) / 1_000.0, 3) if ntl > 0.0 else 0.0,
        "hold_p50_ms": hold_q["p50"],
        "hold_p95_ms": hold_q["p95"],
        "hold_p99_ms": hold_q["p99"],
        "hold_max_ms": hold_max_ms,
        "closed_lot_count": len(pooled_hold_samples),
        "depth_clipped_exits": depth_clipped,
        "stale_book_cancels": stale_cancels,
        "stale_exit_blocks": stale_blocks,
        "toxicity_pauses": tox_pauses,
        "markout_1s_bps": _agg_mk("markout_1s_bps", "markout_1s_den"),
        "markout_5s_bps": _agg_mk("markout_5s_bps", "markout_5s_den"),
        "markout_10s_bps": _agg_mk("markout_10s_bps", "markout_10s_den"),
        "cancel_in_flight_fills": cif_fills,
        "cancel_in_flight_share": round(cif_share, 6),
        "unprinted_book_cross_fills": ubx_fills,
        "unprinted_book_cross_notional_usd": round(ubx_notl, 4),
        "positive_window_ratio": round(pos_wins / n_wins, 4) if n_wins > 0 else 0.0,
        "psr_zero": psr_zero,
        "max_drawdown_usd": round(max_dd_usd, 4),
        "worst_10h_window": worst_win,
        "by_window": win_records,
    }
    if include_symbol:
        out["symbol"] = first["symbol"]
        out["domain"] = first["domain"]
    return out


def politis_romano_stationary_bootstrap_paired(
    arm_windows: Sequence[dict[str, Any]],
    baseline_windows: Sequence[dict[str, Any]],
    *,
    expected_block_len: float = DEFAULT_PREREG_BLOCK_LEN,
    n_boot: int = DEFAULT_PREREG_N_BOOT,
    seed: int = DEFAULT_PREREG_BOOTSTRAP_SEED,
) -> dict[str, Any]:
    """Politis-Romano (1994) circular stationary block bootstrap on paired windows (`arm - baseline`).

    Reports 95% CI for:
      1. Paired per-window MTM net cash PnL delta (`arm - baseline`, USD; >0 = arm better).
      2. Paired ratio-of-sums true ledger `$/M` delta (`arm - baseline`, $/M; <0 = lower cost / more profit).
      3. Paired ratio-of-sums historical double-sided `$/M` delta (`arm - baseline`, $/M).
    """
    if expected_block_len < 1.0:
        raise ValueError("expected_block_len must be >= 1.0")
    if n_boot < 100:
        raise ValueError("n_boot must be >= 100")

    base_map = {str(w["win"]): w for w in baseline_windows}
    arm_map = {str(w["win"]): w for w in arm_windows}
    common_wins = sorted(set(base_map.keys()) & set(arm_map.keys()), key=_window_sort_key)
    t_len = len(common_wins)
    if t_len < 3:
        raise ValueError(f"Need at least 3 aligned windows for stationary bootstrap, got {t_len}")

    arm_nets = [float(arm_map[w]["mtm_net_usd"]) for w in common_wins]
    base_nets = [float(base_map[w]["mtm_net_usd"]) for w in common_wins]
    delta_nets = [a - b for a, b in zip(arm_nets, base_nets)]

    arm_costs = [float(arm_map[w]["net_cash_cost_usd"]) for w in common_wins]
    base_costs = [float(base_map[w]["net_cash_cost_usd"]) for w in common_wins]
    arm_vols = [float(arm_map[w]["target_executed_fill_notional_usd"]) for w in common_wins]
    base_vols = [float(base_map[w]["target_executed_fill_notional_usd"]) for w in common_wins]
    arm_2ntls = [2.0 * float(arm_map[w]["ntl"]) for w in common_wins]
    base_2ntls = [2.0 * float(base_map[w]["ntl"]) for w in common_wins]

    obs_mean_delta_net = sum(delta_nets) / t_len
    obs_total_delta_net = sum(delta_nets)
    obs_arm_ledger = (sum(arm_costs) / sum(arm_vols)) * 1e6 if sum(arm_vols) > 0.0 else None
    obs_base_ledger = (sum(base_costs) / sum(base_vols)) * 1e6 if sum(base_vols) > 0.0 else None
    obs_delta_ledger = (
        (obs_arm_ledger - obs_base_ledger)
        if (obs_arm_ledger is not None and obs_base_ledger is not None)
        else None
    )
    obs_arm_ds = (sum(arm_costs) / sum(arm_2ntls)) * 1e6 if sum(arm_2ntls) > 0.0 else None
    obs_base_ds = (sum(base_costs) / sum(base_2ntls)) * 1e6 if sum(base_2ntls) > 0.0 else None
    obs_delta_ds = (obs_arm_ds - obs_base_ds) if (obs_arm_ds is not None and obs_base_ds is not None) else None

    rng = random.Random(seed)
    p_restart = 1.0 / float(expected_block_len)

    boot_mean_net_deltas: list[float] = []
    boot_ledger_deltas: list[float] = []
    boot_ds_deltas: list[float] = []

    for _ in range(n_boot):
        idx = rng.randrange(t_len)
        sum_dn = 0.0
        sum_ac = 0.0
        sum_bc = 0.0
        sum_av = 0.0
        sum_bv = 0.0
        sum_a2n = 0.0
        sum_b2n = 0.0
        for step in range(t_len):
            if step > 0:
                if rng.random() < p_restart:
                    idx = rng.randrange(t_len)
                else:
                    idx = (idx + 1) % t_len
            sum_dn += delta_nets[idx]
            sum_ac += arm_costs[idx]
            sum_bc += base_costs[idx]
            sum_av += arm_vols[idx]
            sum_bv += base_vols[idx]
            sum_a2n += arm_2ntls[idx]
            sum_b2n += base_2ntls[idx]
        boot_mean_net_deltas.append(sum_dn / t_len)
        if sum_av > 0.0 and sum_bv > 0.0:
            boot_ledger_deltas.append((sum_ac / sum_av) * 1e6 - (sum_bc / sum_bv) * 1e6)
        if sum_a2n > 0.0 and sum_b2n > 0.0:
            boot_ds_deltas.append((sum_ac / sum_a2n) * 1e6 - (sum_bc / sum_b2n) * 1e6)

    def _ci_summary(samples: Sequence[float], *, favor_positive: bool) -> dict[str, Any] | None:
        if not samples:
            return None
        s = sorted(samples)
        m = len(s)
        lo_idx = int(0.025 * (m - 1))
        hi_idx = int(0.975 * (m - 1))
        med_idx = int(0.500 * (m - 1))
        if favor_positive:
            p_non_improve = sum(1 for x in s if x <= 0.0) / m
        else:
            p_non_improve = sum(1 for x in s if x >= 0.0) / m
        return {
            "boot_median": round(s[med_idx], 6),
            "ci95_low": round(s[lo_idx], 6),
            "ci95_high": round(s[hi_idx], 6),
            "p_non_improvement": round(p_non_improve, 6),
        }

    return {
        "method": "politis_romano_stationary_block_bootstrap",
        "windows_aligned": t_len,
        "window_ids": common_wins,
        "expected_block_len": expected_block_len,
        "restart_prob_p": round(p_restart, 6),
        "n_boot": n_boot,
        "seed": seed,
        "observed_mean_delta_net_usd_per_win": round(obs_mean_delta_net, 6),
        "observed_total_delta_net_usd": round(obs_total_delta_net, 6),
        "delta_net_usd_per_win": _ci_summary(boot_mean_net_deltas, favor_positive=True),
        "observed_delta_ledger_usd_per_m": round(obs_delta_ledger, 4) if obs_delta_ledger is not None else None,
        "delta_ledger_usd_per_m": _ci_summary(boot_ledger_deltas, favor_positive=False),
        "observed_delta_historical_double_sided_usd_per_m": (
            round(obs_delta_ds, 4) if obs_delta_ds is not None else None
        ),
        "delta_historical_double_sided_usd_per_m": _ci_summary(boot_ds_deltas, favor_positive=False),
    }


def compute_core4_multiple_testing(
    rows: Sequence[dict[str, Any]],
    *,
    symbols: Sequence[str] = CORE_4_SYMBOLS,
    arm_names: Sequence[str] | None = None,
    cumulative_n_trials_sensitivity: int = 18,
) -> dict[str, Any]:
    """Compute PSR, N_eff, DSR(M_raw), DSR(N_eff), DSR(M_cum), and CSCV PBO over aligned windows."""
    allowed_syms = set(symbols)
    allowed_arms = set(arm_names) if arm_names is not None else None
    arm_win_net: dict[str, dict[str, float]] = defaultdict(dict)
    for r in rows:
        if r.get("skipped"):
            continue
        if r.get("symbol") not in allowed_syms:
            continue
        arm = str(r["arm"])
        if allowed_arms is not None and arm not in allowed_arms:
            continue
        win = str(r["win"])
        arm_win_net[arm][win] = arm_win_net[arm].get(win, 0.0) + float(r["mtm_net_usd"])

    if len(arm_win_net) < 2:
        return {"error": "need_at_least_2_arms"}
    arms_sorted = sorted(arm_win_net.keys())
    common_wins = sorted(set.intersection(*(set(arm_win_net[a].keys()) for a in arms_sorted)), key=_window_sort_key)
    if len(common_wins) < 4:
        return {"error": "need_at_least_4_aligned_windows"}

    series = {a: [arm_win_net[a][w] for w in common_wins] for a in arms_sorted}
    varying = {a: v for a, v in series.items() if len(set(v)) > 1}
    if len(varying) < 2:
        return {"error": "fewer_than_2_varying_arms"}

    dsr_rep = deflated_sharpe_report(varying)
    n_blocks = 8 if len(common_wins) >= 16 else 4
    pbo_rep = cscv_pbo(varying, num_blocks=n_blocks) if len(common_wins) >= n_blocks * 2 else None

    # Cumulative search-record sensitivity (18 = 12 frontier + 6 ablation; 50/100 = +symbol screen + latency grid)
    sharpes = [dsr_rep["trials"][a]["sr"] for a in sorted(varying.keys())]
    sr_thresh_cum = expected_max_sharpe(sharpes, float(cumulative_n_trials_sensitivity))
    dsr_cum = probabilistic_sharpe_ratio(dsr_rep["best_trial_moments"], sr_thresh_cum)
    cum_grid: dict[str, Any] = {}
    for m_cum in sorted({int(cumulative_n_trials_sensitivity), 18, 50, 100}):
        sr_t = expected_max_sharpe(sharpes, float(m_cum))
        cum_grid[f"M_{m_cum}"] = {
            "n_trials": m_cum,
            "sr_threshold": round(sr_t, 6),
            "dsr": round(probabilistic_sharpe_ratio(dsr_rep["best_trial_moments"], sr_t), 6),
        }

    n_eff_val = float(dsr_rep.get("n_eff", 1.0))
    dsr_n_eff_warning = (
        f"N_eff={n_eff_val:.4f} (< 2.0 across correlated hold rungs) clips E[max SR(N_eff)] near 0.0, "
        "making DSR(N_eff) near-trivially ~1.0; use DSR(M_raw), DSR(M_cum>=50), and out-of-sample tests instead."
    )

    return {
        "windows_aligned": len(common_wins),
        "window_ids": common_wins,
        "symbols": sorted(allowed_syms),
        "arms_included": sorted(varying.keys()),
        "dsr": dsr_rep,
        "dsr_n_eff_triviality_note": dsr_n_eff_warning,
        "cumulative_n_trials_sensitivity": cumulative_n_trials_sensitivity,
        "sr_threshold_cumulative": sr_thresh_cum,
        "dsr_cumulative": dsr_cum,
        "cumulative_sensitivity_grid": cum_grid,
        "cscv_pbo": pbo_rep,
    }


def summarize_core4_split(
    rows: Sequence[dict[str, Any]],
    *,
    baseline_arm: str = "S4_speedbump_combined",
    include_bootstrap: bool = True,
) -> dict[str, Any]:
    """Summarize Core 4, ex-META, per-symbol, contribution shares, attribution, and paired bootstrap."""
    active = [r for r in rows if not r.get("skipped")]

    def _agg_by_arm(pred: Any, *, include_symbol: bool = False) -> dict[str, Any]:
        buckets: dict[tuple[Any, ...], list[dict[str, Any]]] = defaultdict(list)
        for r in active:
            if not pred(r):
                continue
            key = (r["arm"], r["symbol"]) if include_symbol else (r["arm"],)
            buckets[key].append(r)
        return {
            "|".join(str(x) for x in k): aggregate_core4_bucket(grp, include_symbol=include_symbol)
            for k, grp in sorted(buckets.items())
        }

    core4_set = set(CORE_4_SYMBOLS)
    ex_meta_set = set(EX_META_SYMBOLS)

    core4_agg = _agg_by_arm(lambda r: r["symbol"] in core4_set)
    ex_meta_agg = _agg_by_arm(lambda r: r["symbol"] in ex_meta_set)
    by_symbol_agg = _agg_by_arm(lambda r: r["symbol"] in core4_set, include_symbol=True)

    # Per-symbol contribution shares per arm
    contrib_by_arm: dict[str, Any] = {}
    for arm_name, c_stats in sorted(core4_agg.items()):
        total_net = float(c_stats["mtm_net_usd"])
        total_exec = float(c_stats["target_executed_fill_notional_usd"])
        per_sym: dict[str, Any] = {}
        max_pos_share = 0.0
        top_sym: str | None = None
        top_sym_net = -1e18
        for sym in CORE_4_SYMBOLS:
            s_stats = by_symbol_agg.get(f"{arm_name}|{sym}")
            if not s_stats:
                continue
            s_net = float(s_stats["mtm_net_usd"])
            s_exec = float(s_stats["target_executed_fill_notional_usd"])
            pnl_share = round(s_net / total_net, 6) if abs(total_net) > 1e-9 else None
            vol_share = round(s_exec / total_exec, 6) if total_exec > 0.0 else None
            if pnl_share is not None and total_net > 0.0:
                max_pos_share = max(max_pos_share, pnl_share)
            if s_net > top_sym_net:
                top_sym_net = s_net
                top_sym = sym
            per_sym[sym] = {
                "mtm_net_usd": s_stats["mtm_net_usd"],
                "realized_net_usd": s_stats["realized_net_usd"],
                "target_executed_fill_notional_usd": s_stats["target_executed_fill_notional_usd"],
                "ledger_mtm_usd_per_m": s_stats["ledger_mtm_usd_per_m"],
                "historical_double_sided_mtm_usd_per_m": s_stats["historical_double_sided_mtm_usd_per_m"],
                "taker_exit_share_s": s_stats["taker_exit_share_s"],
                "hold_p50_ms": s_stats["hold_p50_ms"],
                "hold_p95_ms": s_stats["hold_p95_ms"],
                "hold_max_ms": s_stats["hold_max_ms"],
                "pnl_share_of_core4": pnl_share,
                "volume_share_of_core4": vol_share,
            }
        contrib_by_arm[arm_name] = {
            "core4_mtm_net_usd": total_net,
            "ex_meta_mtm_net_usd": ex_meta_agg[arm_name]["mtm_net_usd"] if arm_name in ex_meta_agg else 0.0,
            "ex_meta_ledger_mtm_usd_per_m": (
                ex_meta_agg[arm_name]["ledger_mtm_usd_per_m"] if arm_name in ex_meta_agg else None
            ),
            "top_symbol_by_pnl": top_sym,
            "max_single_symbol_positive_pnl_share": round(max_pos_share, 6) if total_net > 0.0 else None,
            "symbols": per_sym,
        }

    # Component attribution at 600s hold (all vs ex-META)
    def _build_component_attribution(cohort_map: dict[str, Any]) -> dict[str, Any]:
        req = ("S4_speedbump_combined", "S4_hold600s_only", "S4_hold600s_penny1", "S4_hold600s_skew5", "P3_patient_600s")
        if not all(k in cohort_map for k in req):
            return {}
        s4 = cohort_map["S4_speedbump_combined"]
        h600 = cohort_map["S4_hold600s_only"]
        pen = cohort_map["S4_hold600s_penny1"]
        skw = cohort_map["S4_hold600s_skew5"]
        p3 = cohort_map["P3_patient_600s"]

        def _delta(a: dict[str, Any], b: dict[str, Any]) -> dict[str, Any]:
            la, lb = a["ledger_mtm_usd_per_m"], b["ledger_mtm_usd_per_m"]
            da, db = a["historical_double_sided_mtm_usd_per_m"], b["historical_double_sided_mtm_usd_per_m"]
            return {
                "delta_mtm_net_usd": round(a["mtm_net_usd"] - b["mtm_net_usd"], 4),
                "delta_ledger_usd_per_m": round(la - lb, 4) if (la is not None and lb is not None) else None,
                "delta_historical_ds_usd_per_m": round(da - db, 4) if (da is not None and db is not None) else None,
                "delta_taker_share_s": round(a["taker_exit_share_s"] - b["taker_exit_share_s"], 6),
                "delta_exec_notional_usd": round(
                    a["target_executed_fill_notional_usd"] - b["target_executed_fill_notional_usd"], 2
                ),
            }

        inter_net = (p3["mtm_net_usd"] - h600["mtm_net_usd"]) - (
            (pen["mtm_net_usd"] - h600["mtm_net_usd"]) + (skw["mtm_net_usd"] - h600["mtm_net_usd"])
        )
        return {
            "pure_hold_15s_to_600s": _delta(h600, s4),
            "pure_penny_0_to_1_at_600s": _delta(pen, h600),
            "pure_skew_2_to_5_at_600s": _delta(skw, h600),
            "combined_penny1_skew5_at_600s": _delta(p3, h600),
            "total_p3_vs_s4": _delta(p3, s4),
            "interaction_net_usd_penny_x_skew": round(inter_net, 4),
        }

    # Politis-Romano paired stationary bootstrap vs baseline S4 (when >= 3 windows)
    boot_core4: dict[str, Any] = {}
    boot_ex_meta: dict[str, Any] = {}
    if include_bootstrap and baseline_arm in core4_agg and core4_agg[baseline_arm]["windows_covered"] >= 3:
        base_c4_wins = core4_agg[baseline_arm]["by_window"]
        for arm_name, a_stats in sorted(core4_agg.items()):
            if arm_name == baseline_arm:
                continue
            if a_stats["windows_covered"] >= 3:
                boot_core4[arm_name] = politis_romano_stationary_bootstrap_paired(
                    a_stats["by_window"], base_c4_wins
                )
    if include_bootstrap and baseline_arm in ex_meta_agg and ex_meta_agg[baseline_arm]["windows_covered"] >= 3:
        base_em_wins = ex_meta_agg[baseline_arm]["by_window"]
        for arm_name, a_stats in sorted(ex_meta_agg.items()):
            if arm_name == baseline_arm:
                continue
            if a_stats["windows_covered"] >= 3:
                boot_ex_meta[arm_name] = politis_romano_stationary_bootstrap_paired(
                    a_stats["by_window"], base_em_wins
                )

    return {
        "core4": core4_agg,
        "ex_meta": ex_meta_agg,
        "by_symbol": by_symbol_agg,
        "symbol_contributions": contrib_by_arm,
        "component_attribution": {
            "core4": _build_component_attribution(core4_agg),
            "ex_meta": _build_component_attribution(ex_meta_agg),
        },
        "paired_bootstrap_vs_s4": {
            "core4": boot_core4,
            "ex_meta": boot_ex_meta,
        },
    }


def _eval_core4_window(
    lakehouse_dir: Path,
    win_id: str,
    win_map: dict[str, dict[str, str]],
    maker_fee_bps: float,
    taker_fee_bps: float,
    account_fee_status: str,
    fee_source: str,
    arm_names: Sequence[str] | None,
    rebate_bps: float,
    verify_parity: bool,
) -> tuple[list[dict[str, Any]], dict[str, Any], float, float, float, float, int]:
    """Evaluate a single 10h RWA window across Core 4 symbols and ablation arms."""
    if "rwa" not in win_map:
        return [], {}, 0.0, 0.0, 0.0, 0.0, 0

    import duckdb  # Lazy import so stdlib-only CI never requires duckdb

    con = duckdb.connect(":memory:")
    con.execute("SET preserve_insertion_order=false; SET threads=2; SET memory_limit='2GB';")

    c_info = win_map["rwa"]
    rh_map = VENUE_SYMBOL_MAP[TARGET_VENUE]
    ref_venues = ("arcus", "trade_xyz")

    bbo_cache: dict[str, tuple[dict[str, list[tuple[int, float, float, float, float]]], dict[str, dict[str, Any]]]] = {}
    trade_cache: dict[str, list[tuple[int, str, float, float]]] = {}

    def load_venue_bbo(venue: str, mkts: Sequence[str], bucket_ms: int = 250) -> tuple[dict[str, list[tuple[int, float, float, float, float]]], dict[str, dict[str, Any]]]:
        if venue in bbo_cache:
            return bbo_cache[venue]
        files = _campaign_parquet_files(
            lakehouse_dir, "bbo_quotes", venue, c_info["utc_date"], c_info["campaign_id"]
        )
        if not files or not mkts:
            bbo_cache[venue] = ({}, {})
            return bbo_cache[venue]
        f_sql = "[" + ", ".join(f"'{p}'" for p in files) + "]"
        m_sql = ", ".join(f"'{m}'" for m in sorted(set(mkts)))

        stat_rows = con.execute(
            f"""
            WITH s AS (
                SELECT market,
                       available_at_ms,
                       available_at_ms - LAG(available_at_ms) OVER (PARTITION BY market ORDER BY available_at_ms) AS dt_ms
                FROM read_parquet({f_sql})
                WHERE market IN ({m_sql})
                  AND tradable = true
                  AND bid_px_f64 > 0
                  AND ask_px_f64 > bid_px_f64
            )
            SELECT market,
                   COUNT(*) AS cnt,
                   MIN(available_at_ms) AS t0,
                   MAX(available_at_ms) AS t1,
                   COALESCE(MAX(dt_ms), 0) AS max_gap_ms,
                   COALESCE(APPROX_QUANTILE(dt_ms, 0.50), 0) AS med_dt_ms,
                   COALESCE(APPROX_QUANTILE(dt_ms, 0.95), 0) AS p95_dt_ms,
                   COALESCE(SUM(CASE WHEN dt_ms > 5000 THEN 1 ELSE 0 END), 0) AS gaps_gt_5s,
                   COALESCE(SUM(CASE WHEN dt_ms > 5000 THEN dt_ms - 5000 ELSE 0 END), 0) AS stale_excess_ms
            FROM s
            GROUP BY market
            """
        ).fetchall()

        cont_map: dict[str, dict[str, Any]] = {}
        for mkt, cnt, t0, t1, mgap, med_dt, p95_dt, gt5s, stale_ms in stat_rows:
            if not cnt or cnt < 2:
                cont_map[str(mkt)] = {
                    "raw_events": int(cnt or 0),
                    "duration_ms": 0,
                    "hours": 0.0,
                    "max_gap_ms": 0,
                    "median_dt_ms": 0.0,
                    "p95_dt_ms": 0.0,
                    "gaps_gt_5s": 0,
                    "stale_time_share": 1.0,
                    "is_continuous_10h": False,
                }
            else:
                dur = int(t1 - t0)
                cont_map[str(mkt)] = {
                    "raw_events": int(cnt),
                    "duration_ms": dur,
                    "hours": round(dur / 3_600_000.0, 3),
                    "max_gap_ms": int(mgap),
                    "median_dt_ms": round(float(med_dt), 2),
                    "p95_dt_ms": round(float(p95_dt), 2),
                    "gaps_gt_5s": int(gt5s),
                    "stale_time_share": round(float(stale_ms) / max(1.0, float(dur)), 6),
                    "is_continuous_10h": dur >= TEN_HOURS_MS and int(mgap) <= MAX_CONTINUOUS_GAP_MS,
                }

        bucket_rows = con.execute(
            f"""
            SELECT market,
                   MIN(available_at_ms) AS ts_ms,
                   arg_min(bid_px_f64, available_at_ms) AS bid_px,
                   arg_min(CAST(bid_sz AS DOUBLE), available_at_ms) AS bid_sz,
                   arg_min(ask_px_f64, available_at_ms) AS ask_px,
                   arg_min(CAST(ask_sz AS DOUBLE), available_at_ms) AS ask_sz
            FROM read_parquet({f_sql})
            WHERE market IN ({m_sql})
              AND tradable = true
              AND bid_px_f64 > 0
              AND ask_px_f64 > bid_px_f64
            GROUP BY market, (available_at_ms // {int(bucket_ms)})
            ORDER BY market ASC, ts_ms ASC
            """
        ).fetchall()

        by_mkt: dict[str, list[tuple[int, float, float, float, float]]] = defaultdict(list)
        for mkt, ts_ms, bb, bsz, ba, asz in bucket_rows:
            by_mkt[str(mkt)].append((int(ts_ms), float(bb), float(bsz), float(ba), float(asz)))
        bbo_cache[venue] = (by_mkt, cont_map)
        return bbo_cache[venue]

    def load_target_trades(mkts: Sequence[str]) -> dict[str, list[tuple[int, str, float, float]]]:
        nonlocal trade_cache
        if trade_cache:
            return trade_cache
        files = _campaign_parquet_files(
            lakehouse_dir, "public_trades", TARGET_VENUE, c_info["utc_date"], c_info["campaign_id"]
        )
        if not files or not mkts:
            return {}
        f_sql = "[" + ", ".join(f"'{p}'" for p in files) + "]"
        m_sql = ", ".join(f"'{m}'" for m in sorted(set(mkts)))
        t_rows = con.execute(
            f"""
            SELECT market, available_at_ms, aggressor_side, price_f64, size_f64
            FROM read_parquet({f_sql})
            WHERE market IN ({m_sql})
              AND NOT is_zero_price_transfer
              AND price_f64 > 0
              AND size_f64 > 0
            ORDER BY market ASC, available_at_ms ASC
            """
        ).fetchall()
        out: dict[str, list[tuple[int, str, float, float]]] = defaultdict(list)
        for mkt, ts_ms, agg_side, px, sz in t_rows:
            out[str(mkt)].append((int(ts_ms), str(agg_side), float(px), float(sz)))
        trade_cache = out
        return trade_cache

    tgt_mkts = [rh_map[s] for s in CORE_4_SYMBOLS]
    bbo_map, cont_map = load_venue_bbo(TARGET_VENUE, tgt_mkts)
    for rv in ref_venues:
        rv_mkts = [VENUE_SYMBOL_MAP[rv][s] for s in CORE_4_SYMBOLS if s in VENUE_SYMBOL_MAP[rv]]
        load_venue_bbo(rv, rv_mkts)
    tr_map = load_target_trades(tgt_mkts)

    freshness_records: dict[str, Any] = {}
    for mkt, cs in cont_map.items():
        freshness_records[f"{win_id}|rwa|{TARGET_VENUE}|{mkt}"] = cs

    arms = build_core4_ablation_arms(
        PRIMARY_LATENCY_TRIPLE,
        maker_fee_bps=maker_fee_bps,
        taker_fee_bps=taker_fee_bps,
        rebate_bps=rebate_bps,
        arm_names=arm_names,
    )

    rows_out: list[dict[str, Any]] = []
    max_gross_err = 0.0
    max_net_err = 0.0
    max_mtm_cash_err = 0.0
    max_parity_err = 0.0
    non_flat_count = 0

    for sym in CORE_4_SYMBOLS:
        raw_mkt = rh_map[sym]
        b_rows = bbo_map.get(raw_mkt, [])
        cont = cont_map.get(
            raw_mkt,
            {
                "raw_events": 0,
                "duration_ms": 0,
                "hours": 0.0,
                "max_gap_ms": 0,
                "median_dt_ms": 0.0,
                "p95_dt_ms": 0.0,
                "gaps_gt_5s": 0,
                "stale_time_share": 1.0,
                "is_continuous_10h": False,
            },
        )
        if len(b_rows) < 200:
            rows_out.append(
                {
                    "win": win_id,
                    "campaign_id": c_info["campaign_id"],
                    "domain": "rwa",
                    "venue": TARGET_VENUE,
                    "symbol": sym,
                    "raw_market": raw_mkt,
                    "settlement": TARGET_SETTLEMENT,
                    "account_fee_status": account_fee_status,
                    "cont": False,
                    "continuity": cont,
                    "skipped": f"too_few_books:{cont.get('raw_events', 0)}",
                }
            )
            continue

        events: list[dict[str, Any]] = [
            {"ts": r[0], "kind": "book", "bb": r[1], "bsz": max(r[2], 1e-9), "ba": r[3], "asz": max(r[4], 1e-9)}
            for r in b_rows
        ]
        refs_used: list[str] = []
        for rv in ref_venues:
            if sym not in VENUE_SYMBOL_MAP[rv]:
                continue
            ref_raw = VENUE_SYMBOL_MAP[rv][sym]
            r_bbo_map, _ = bbo_cache[rv]
            r_rows = r_bbo_map.get(ref_raw, [])
            if len(r_rows) >= 200:
                refs_used.append(rv)
                for rr in r_rows:
                    events.append({"ts": rr[0], "kind": "ref", "venue": rv, "bid": rr[1], "ask": rr[3]})

        for ts_ms, agg_side, px, sz in tr_map.get(raw_mkt, []):
            s_up = agg_side.upper()
            side_str = "BUY" if s_up in ("BUY", "B", "BID") else "SELL"
            events.append({"ts": ts_ms, "kind": "trade", "side": side_str, "price": px, "size": sz})

        stream = prepare_replay_stream(events)
        tick = _infer_tick_from_prices([r[1] for r in b_rows])
        declared_end = int(b_rows[-1][0])
        del events

        for arm_cfg in arms:
            sim_res = simulate_core4_arm(stream, tick, arm_cfg, declared_end_ms=declared_end)
            if verify_parity:
                ref_res = simulate_prepared_arm(stream, tick, arm_cfg, declared_end_ms=declared_end)
                for pk in ("net", "ntl", "maker_volume_usd", "taker_volume_usd", "stale_exit_blocks", "fills", "cyc"):
                    max_parity_err = max(max_parity_err, abs(float(sim_res[pk]) - float(ref_res[pk])))

            ex_cash = float(sim_res.pop("_exact_cash_ex_fee_usd"))
            ex_real_gross = float(sim_res.pop("_exact_realized_gross_usd"))
            ex_mtm_gross = float(sim_res.pop("_exact_mtm_gross_usd"))
            ex_fee = float(sim_res.pop("_exact_fee_usd"))
            ex_net = float(sim_res.pop("_exact_net_usd"))
            max_mtm_cash_err = max(max_mtm_cash_err, abs(ex_mtm_gross - ex_cash))
            if sim_res["terminal_flat"]:
                max_gross_err = max(max_gross_err, abs(ex_real_gross - ex_cash))
            else:
                non_flat_count += 1
            max_net_err = max(max_net_err, abs(ex_net - (ex_mtm_gross - ex_fee)))

            rows_out.append(
                {
                    "win": win_id,
                    "campaign_id": c_info["campaign_id"],
                    "domain": "rwa",
                    "venue": TARGET_VENUE,
                    "symbol": sym,
                    "raw_market": raw_mkt,
                    "settlement": TARGET_SETTLEMENT,
                    "account_fee_status": account_fee_status,
                    "fee_source": fee_source,
                    "maker_fee_bps": maker_fee_bps,
                    "taker_fee_bps": taker_fee_bps,
                    "rebate_bps": rebate_bps,
                    "tick_inferred": tick,
                    "declared_end_ms": declared_end,
                    "cont": bool(cont["is_continuous_10h"]),
                    "continuity": cont,
                    "refs_used": refs_used,
                    **sim_res,
                }
            )
        del stream

    bbo_cache.clear()
    trade_cache.clear()
    con.close()
    gc.collect()
    return rows_out, freshness_records, max_gross_err, max_net_err, max_mtm_cash_err, max_parity_err, non_flat_count


def _strip_private_keys(rows: Sequence[dict[str, Any]]) -> list[dict[str, Any]]:
    return [{k: v for k, v in r.items() if not k.startswith("_")} for r in rows]


def run_core4_hold_ablation(
    lakehouse_dir: Path,
    *,
    split: str = "all",
    window_ids: Sequence[str] | None = None,
    arm_names: Sequence[str] | None = None,
    rebate_bps: float = 0.0,
    fee_evidence_path: Path | None = None,
    max_workers: int = 4,
    include_rows: bool = False,
    verify_parity: bool = True,
) -> dict[str, Any]:
    """Run Core 4 single-factor hold ablation & component attribution across W01-W27 (`NOT -w28`)."""
    all_discovered = discover_campaigns_w27(lakehouse_dir, split, window_ids)
    selected_windows = {w: m for w, m in all_discovered.items() if "rwa" in m}
    if not selected_windows:
        raise ValueError(f"No RWA campaigns discovered under {lakehouse_dir} for split={split!r}")

    mk_bps, tk_bps, fee_status, fee_src, fee_prov = resolve_core4_rh_fee_provenance(fee_evidence_path)

    rows_out: list[dict[str, Any]] = []
    freshness_all: dict[str, Any] = {}
    max_gross_err = 0.0
    max_net_err = 0.0
    max_mtm_cash_err = 0.0
    max_parity_err = 0.0
    non_flat_count = 0

    work_items = [(win_id, win_map) for win_id, win_map in selected_windows.items()]
    eff_workers = max(1, min(int(max_workers), len(work_items)))

    if eff_workers == 1:
        for win_id, win_map in work_items:
            w_rows, w_fresh, w_ge, w_ne, w_me, w_pe, w_nf = _eval_core4_window(
                lakehouse_dir,
                win_id,
                win_map,
                mk_bps,
                tk_bps,
                fee_status,
                fee_src,
                arm_names,
                rebate_bps,
                verify_parity,
            )
            rows_out.extend(w_rows)
            freshness_all.update(w_fresh)
            max_gross_err = max(max_gross_err, w_ge)
            max_net_err = max(max_net_err, w_ne)
            max_mtm_cash_err = max(max_mtm_cash_err, w_me)
            max_parity_err = max(max_parity_err, w_pe)
            non_flat_count += w_nf
    else:
        with concurrent.futures.ProcessPoolExecutor(max_workers=eff_workers) as pool:
            futs = [
                pool.submit(
                    _eval_core4_window,
                    lakehouse_dir,
                    win_id,
                    win_map,
                    mk_bps,
                    tk_bps,
                    fee_status,
                    fee_src,
                    arm_names,
                    rebate_bps,
                    verify_parity,
                )
                for win_id, win_map in work_items
            ]
            for fut in concurrent.futures.as_completed(futs):
                w_rows, w_fresh, w_ge, w_ne, w_me, w_pe, w_nf = fut.result()
                rows_out.extend(w_rows)
                freshness_all.update(w_fresh)
                max_gross_err = max(max_gross_err, w_ge)
                max_net_err = max(max_net_err, w_ne)
                max_mtm_cash_err = max(max_mtm_cash_err, w_me)
                max_parity_err = max(max_parity_err, w_pe)
                non_flat_count += w_nf
        rows_out.sort(
            key=lambda r: (
                _window_sort_key(str(r["win"])),
                str(r.get("symbol", "")),
                str(r.get("arm", "")),
            )
        )

    assert_no_w28_leak(list(selected_windows.keys()), rows_out)

    def _win_num(w: str) -> int:
        m = re.search(r"(\d+)$", w)
        return int(m.group(1)) if m else 0

    dev_rows = [r for r in rows_out if 1 <= _win_num(str(r["win"])) <= 20]
    hold_rows = [r for r in rows_out if 21 <= _win_num(str(r["win"])) <= 25]
    oos_rows = [r for r in rows_out if _win_num(str(r["win"])) in (26, 27)]

    splits_summary: dict[str, Any] = {}
    for sname, srows in (
        ("selection_dev_w01_w20", dev_rows),
        ("holdout_w21_w25", hold_rows),
        ("w26_w27_oos", oos_rows),
        ("all_w01_w27", rows_out),
    ):
        if srows:
            splits_summary[sname] = summarize_core4_split(
                srows,
                include_bootstrap=(sname in ("selection_dev_w01_w20", "holdout_w21_w25", "all_w01_w27")),
            )

    multiple_testing: dict[str, Any] = {}
    if rows_out:
        multiple_testing["core4_hold_ladder_w01_w27"] = compute_core4_multiple_testing(
            rows_out,
            symbols=CORE_4_SYMBOLS,
            arm_names=HOLD_LADDER_ARMS,
            cumulative_n_trials_sensitivity=18,
        )
        multiple_testing["core4_all_ablation_w01_w27"] = compute_core4_multiple_testing(
            rows_out,
            symbols=CORE_4_SYMBOLS,
            arm_names=None,
            cumulative_n_trials_sensitivity=18,
        )
        multiple_testing["ex_meta_all_ablation_w01_w27"] = compute_core4_multiple_testing(
            rows_out,
            symbols=EX_META_SYMBOLS,
            arm_names=None,
            cumulative_n_trials_sensitivity=18,
        )

    payload: dict[str, Any] = {
        "schema": "track03_core4_single_factor_hold_ablation_v1",
        "venue_capability_snapshot_version": VENUE_CAPABILITY_SNAPSHOT_VERSION,
        "w28_excluded": True,
        "target_venue": TARGET_VENUE,
        "target_settlement": TARGET_SETTLEMENT,
        "core4_symbols": list(CORE_4_SYMBOLS),
        "ex_meta_symbols": list(EX_META_SYMBOLS),
        "latency_triple": list(PRIMARY_LATENCY_TRIPLE),
        "latency_label": f"o{PRIMARY_LATENCY_TRIPLE[0]}_c{PRIMARY_LATENCY_TRIPLE[1]}_e{PRIMARY_LATENCY_TRIPLE[2]}",
        "rebate_bps": rebate_bps,
        "fee_provenance": fee_prov,
        "hold_ladder_arms": list(HOLD_LADDER_ARMS),
        "component_attribution_arms": list(COMPONENT_ATTRIBUTION_ARMS),
        "arm_specs": CORE4_ABLATION_ARMS_SPEC,
        "windows": selected_windows,
        "invariants": {
            "total_rows": len(rows_out),
            "windows_count": len(selected_windows),
            "non_flat_count": non_flat_count,
            "max_gross_err": max_gross_err,
            "max_net_err": max_net_err,
            "max_mtm_cash_err": max_mtm_cash_err,
            "parity_verified": bool(verify_parity),
            "max_parity_err": max_parity_err,
        },
        "splits": splits_summary,
        "multiple_testing": multiple_testing,
    }
    if include_rows:
        payload["rows"] = _strip_private_keys(rows_out)
    return payload


QUALITATIVE_TOXIC_CROSS_SCENARIOS: tuple[tuple[str, str, float], ...] = (
    ("baseline_prints_only", "strict_through", 0.0),
    ("strict_through_25", "strict_through", 0.25),
    ("strict_through_50", "strict_through", 0.50),
    ("strict_through_100", "strict_through", 1.00),
    ("opposite_touch_25", "opposite_touch", 0.25),
    ("opposite_touch_50", "opposite_touch", 0.50),
    ("opposite_touch_100", "opposite_touch", 1.00),
    ("level_evicted_25", "level_evicted", 0.25),
    ("level_evicted_50", "level_evicted", 0.50),
)

QUALITATIVE_TOXIC_ARMS: tuple[str, ...] = (
    "S4_speedbump_combined",
    "S4_hold60s",
    "S4_hold180s",
    "S4_hold600s_only",
)


def _student_t_two_sided_p(t_abs: float, df: int) -> float | None:
    """Pure-stdlib two-sided Student-t p-value via smooth cosine transformation on [0, pi/2]."""
    if df < 1 or not math.isfinite(t_abs):
        return None
    if t_abs == 0.0:
        return 1.0
    theta_hi = math.atan(abs(t_abs) / math.sqrt(float(df)))
    n_steps = 400
    h = theta_hi / n_steps
    power = df - 1
    acc = 0.0
    for i in range(n_steps + 1):
        theta = i * h
        w = 1.0 if (i == 0 or i == n_steps) else (4.0 if i % 2 == 1 else 2.0)
        val = (math.cos(theta) ** power) if power > 0 else 1.0
        acc += w * val
    integral = (h / 3.0) * acc
    coeff = 2.0 * math.exp(math.lgamma(0.5 * (df + 1)) - math.lgamma(0.5 * df)) / math.sqrt(math.pi)
    cdf_two_sided_inside = coeff * integral
    return max(0.0, min(1.0, 1.0 - cdf_two_sided_inside))


def compute_ols_beta_regression(xs: Sequence[float], ys: Sequence[float]) -> dict[str, Any]:
    """Pure-stdlib OLS regression `y = alpha + beta * x` with SE, t-stat, R^2, and Student-t p-value."""
    if len(xs) != len(ys):
        raise ValueError(f"Mismatched OLS lengths: {len(xs)} vs {len(ys)}")
    pairs = [(float(x), float(y)) for x, y in zip(xs, ys) if math.isfinite(float(x)) and math.isfinite(float(y))]
    n = len(pairs)
    if n < 3:
        return {
            "n": n,
            "alpha": None,
            "beta": None,
            "se_beta": None,
            "t_beta": None,
            "p_beta": None,
            "r_squared": None,
            "corr": None,
        }
    mean_x = sum(x for x, _ in pairs) / n
    mean_y = sum(y for _, y in pairs) / n
    sxx = sum((x - mean_x) ** 2 for x, _ in pairs)
    syy = sum((y - mean_y) ** 2 for _, y in pairs)
    sxy = sum((x - mean_x) * (y - mean_y) for x, y in pairs)
    if sxx <= 1e-18:
        return {
            "n": n,
            "alpha": round(mean_y, 6),
            "beta": None,
            "se_beta": None,
            "t_beta": None,
            "p_beta": None,
            "r_squared": 0.0,
            "corr": None,
        }
    beta = sxy / sxx
    alpha = mean_y - beta * mean_x
    rss = sum((y - (alpha + beta * x)) ** 2 for x, y in pairs)
    df = n - 2
    s2 = max(0.0, rss / df)
    se_beta = math.sqrt(s2 / sxx)
    t_beta = (beta / se_beta) if se_beta > 1e-15 else 0.0
    p_beta = _student_t_two_sided_p(abs(t_beta), df)
    r2 = (1.0 - (rss / syy)) if syy > 1e-18 else 0.0
    corr = (sxy / math.sqrt(sxx * syy)) if (sxx > 1e-18 and syy > 1e-18) else 0.0
    return {
        "n": n,
        "alpha": round(alpha, 6),
        "beta": round(beta, 6),
        "se_beta": round(se_beta, 6),
        "t_beta": round(t_beta, 4),
        "p_beta": round(p_beta, 6) if p_beta is not None else None,
        "r_squared": round(max(0.0, min(1.0, r2)), 6),
        "corr": round(max(-1.0, min(1.0, corr)), 6),
    }


def _summarize_regime_bucket(items: Sequence[dict[str, Any]]) -> dict[str, Any]:
    n = len(items)
    if n == 0:
        return {
            "n_units": 0,
            "mean_ret_bps": None,
            "mtm_net_usd": 0.0,
            "mean_net_usd": 0.0,
            "positive_ratio": 0.0,
            "target_executed_fill_notional_usd": 0.0,
            "ledger_mtm_usd_per_m": None,
            "spr": 0.0,
            "drft": 0.0,
            "pex": 0.0,
            "tslp": 0.0,
        }
    mtm = sum(float(x["mtm_net_usd"]) for x in items)
    exec_notl = sum(float(x["target_executed_fill_notional_usd"]) for x in items)
    rets = [float(x["ret_bps"]) for x in items if x.get("ret_bps") is not None]
    pos_n = sum(1 for x in items if float(x["mtm_net_usd"]) > 0.0)
    return {
        "n_units": n,
        "mean_ret_bps": round(sum(rets) / len(rets), 4) if rets else None,
        "mtm_net_usd": round(mtm, 6),
        "mean_net_usd": round(mtm / n, 6),
        "positive_ratio": round(pos_n / n, 4),
        "target_executed_fill_notional_usd": round(exec_notl, 4),
        "ledger_mtm_usd_per_m": round((-mtm / exec_notl) * 1e6, 4) if exec_notl > 0.0 else None,
        "spr": round(sum(float(x["spr"]) for x in items), 6),
        "drft": round(sum(float(x["drft"]) for x in items), 6),
        "pex": round(sum(float(x["pex"]) for x in items), 6),
        "tslp": round(sum(float(x["tslp"]) for x in items), 6),
    }


def compute_regime_symmetry(
    up_items: Sequence[dict[str, Any]],
    down_items: Sequence[dict[str, Any]],
) -> dict[str, Any]:
    """Compare up-window vs down-window strategy profitability and Welch two-sample t-test."""
    up_s = _summarize_regime_bucket(up_items)
    dn_s = _summarize_regime_bucket(down_items)
    up_vals = [float(x["mtm_net_usd"]) for x in up_items]
    dn_vals = [float(x["mtm_net_usd"]) for x in down_items]
    welch_t: float | None = None
    welch_p: float | None = None
    if len(up_vals) >= 2 and len(dn_vals) >= 2:
        mu_u = sum(up_vals) / len(up_vals)
        mu_d = sum(dn_vals) / len(dn_vals)
        v_u = sum((v - mu_u) ** 2 for v in up_vals) / (len(up_vals) - 1)
        v_d = sum((v - mu_d) ** 2 for v in dn_vals) / (len(dn_vals) - 1)
        se2 = (v_u / len(up_vals)) + (v_d / len(dn_vals))
        if se2 > 1e-18:
            welch_t = round((mu_u - mu_d) / math.sqrt(se2), 4)
            df_num = se2 * se2
            df_den = ((v_u / len(up_vals)) ** 2) / (len(up_vals) - 1) + ((v_d / len(dn_vals)) ** 2) / (len(dn_vals) - 1)
            df_est = max(1, int(round(df_num / df_den))) if df_den > 1e-18 else max(1, len(up_vals) + len(dn_vals) - 2)
            welch_p = round(_student_t_two_sided_p(abs(welch_t), df_est) or 1.0, 6)
    return {
        "up_windows": up_s,
        "down_windows": dn_s,
        "welch_t_up_minus_down_mean_usd": welch_t,
        "welch_p_two_sided": welch_p,
    }


def _eval_qualitative_toxic_window(
    lakehouse_dir: Path,
    win_id: str,
    win_map: dict[str, dict[str, str]],
    maker_fee_bps: float,
    taker_fee_bps: float,
    rebate_bps: float,
    arm_names: Sequence[str],
    scenarios: Sequence[tuple[str, str, float]],
) -> list[dict[str, Any]]:
    """Run book-cross toxic-fill sensitivity matrix for one window reusing loaded DuckDB parquet streams."""
    if "rwa" not in win_map:
        return []
    import duckdb  # Lazy import

    con = duckdb.connect(":memory:")
    con.execute("SET preserve_insertion_order=false; SET threads=2; SET memory_limit='2GB';")
    c_info = win_map["rwa"]
    rh_map = VENUE_SYMBOL_MAP[TARGET_VENUE]
    ref_venues = ("arcus", "trade_xyz")

    def _load_bbo(venue: str, mkts: Sequence[str], bucket_ms: int = 250) -> dict[str, list[tuple[int, float, float, float, float]]]:
        files = _campaign_parquet_files(lakehouse_dir, "bbo_quotes", venue, c_info["utc_date"], c_info["campaign_id"])
        if not files or not mkts:
            return {}
        f_sql = "[" + ", ".join(f"'{p}'" for p in files) + "]"
        m_sql = ", ".join(f"'{m}'" for m in sorted(set(mkts)))
        rows = con.execute(
            f"""
            SELECT market,
                   MIN(available_at_ms) AS ts_ms,
                   arg_min(bid_px_f64, available_at_ms) AS bid_px,
                   arg_min(CAST(bid_sz AS DOUBLE), available_at_ms) AS bid_sz,
                   arg_min(ask_px_f64, available_at_ms) AS ask_px,
                   arg_min(CAST(ask_sz AS DOUBLE), available_at_ms) AS ask_sz
            FROM read_parquet({f_sql})
            WHERE market IN ({m_sql})
              AND tradable = true
              AND bid_px_f64 > 0
              AND ask_px_f64 > bid_px_f64
            GROUP BY market, (available_at_ms // {int(bucket_ms)})
            ORDER BY market ASC, ts_ms ASC
            """
        ).fetchall()
        out: dict[str, list[tuple[int, float, float, float, float]]] = defaultdict(list)
        for mkt, ts_ms, bb, bsz, ba, asz in rows:
            out[str(mkt)].append((int(ts_ms), float(bb), float(bsz), float(ba), float(asz)))
        return out

    tgt_mkts = [rh_map[s] for s in CORE_4_SYMBOLS]
    bbo_tgt = _load_bbo(TARGET_VENUE, tgt_mkts)
    bbo_refs = {
        rv: _load_bbo(rv, [VENUE_SYMBOL_MAP[rv][s] for s in CORE_4_SYMBOLS if s in VENUE_SYMBOL_MAP[rv]])
        for rv in ref_venues
    }

    tr_files = _campaign_parquet_files(lakehouse_dir, "public_trades", TARGET_VENUE, c_info["utc_date"], c_info["campaign_id"])
    tr_map: dict[str, list[tuple[int, str, float, float]]] = defaultdict(list)
    if tr_files and tgt_mkts:
        tf_sql = "[" + ", ".join(f"'{p}'" for p in tr_files) + "]"
        tm_sql = ", ".join(f"'{m}'" for m in sorted(set(tgt_mkts)))
        for mkt, ts_ms, agg_side, px, sz in con.execute(
            f"""
            SELECT market, available_at_ms, aggressor_side, price_f64, size_f64
            FROM read_parquet({tf_sql})
            WHERE market IN ({tm_sql}) AND price_f64 > 0 AND size_f64 > 0
            ORDER BY available_at_ms ASC
            """
        ).fetchall():
            tr_map[str(mkt)].append((int(ts_ms), str(agg_side), float(px), float(sz)))
    con.close()

    base_arms = build_core4_ablation_arms(
        PRIMARY_LATENCY_TRIPLE,
        maker_fee_bps=maker_fee_bps,
        taker_fee_bps=taker_fee_bps,
        rebate_bps=rebate_bps,
        arm_names=arm_names,
    )

    out_rows: list[dict[str, Any]] = []
    for sym in CORE_4_SYMBOLS:
        raw_mkt = rh_map[sym]
        b_rows = bbo_tgt.get(raw_mkt, [])
        if len(b_rows) < 200:
            continue
        events: list[dict[str, Any]] = [
            {"ts": r[0], "kind": "book", "bb": r[1], "bsz": max(r[2], 1e-9), "ba": r[3], "asz": max(r[4], 1e-9)}
            for r in b_rows
        ]
        for rv in ref_venues:
            if sym in VENUE_SYMBOL_MAP[rv]:
                for rr in bbo_refs[rv].get(VENUE_SYMBOL_MAP[rv][sym], []):
                    events.append({"ts": rr[0], "kind": "ref", "venue": rv, "bid": rr[1], "ask": rr[3]})
        for ts_ms, agg_side, px, sz in tr_map.get(raw_mkt, []):
            s_up = agg_side.upper()
            side_str = "BUY" if s_up in ("BUY", "B", "BID") else "SELL"
            events.append({"ts": ts_ms, "kind": "trade", "side": side_str, "price": px, "size": sz})

        stream = prepare_replay_stream(events)
        tick = _infer_tick_from_prices([r[1] for r in b_rows])
        declared_end = int(b_rows[-1][0])
        del events

        for sc_id, mode, frac in scenarios:
            for base_arm in base_arms:
                arm_cfg = dataclasses.replace(
                    base_arm,
                    book_cross_mode=mode,
                    book_cross_toxic_fill_frac=frac,
                )
                sim_res = simulate_core4_arm(stream, tick, arm_cfg, declared_end_ms=declared_end)
                out_rows.append(
                    {
                        "scenario_id": sc_id,
                        "book_cross_mode": mode,
                        "book_cross_toxic_fill_frac": frac,
                        "win": win_id,
                        "symbol": sym,
                        "arm": base_arm.name,
                        "mtm_net_usd": float(sim_res["mtm_net_usd"]),
                        "realized_net_usd": float(sim_res["realized_net_usd"]),
                        "spr": float(sim_res["spr"]),
                        "drft": float(sim_res["drft"]),
                        "pex": float(sim_res["pex"]),
                        "tslp": float(sim_res["tslp"]),
                        "target_executed_fill_notional_usd": float(sim_res["target_executed_fill_notional_usd"]),
                        "maker_volume_usd": float(sim_res["maker_volume_usd"]),
                        "taker_volume_usd": float(sim_res["taker_volume_usd"]),
                        "taker_exit_share_s": float(sim_res["taker_exit_share_s"]),
                        "unprinted_book_cross_fills": int(sim_res["unprinted_book_cross_fills"]),
                        "unprinted_book_cross_notional_usd": float(sim_res["unprinted_book_cross_notional_usd"]),
                        "terminal_residual_notional_usd": float(sim_res["terminal_residual_notional_usd"]),
                    }
                )
        del stream
    gc.collect()
    return out_rows


def _build_qualitative_from_baseline_rows(
    rows_out: Sequence[dict[str, Any]],
) -> tuple[dict[str, Any], dict[str, Any], dict[str, Any]]:
    """Compute Audit ① (spr/drft/pex/tslp), ② (up vs down windows), and ④ (OLS beta) from baseline rows."""
    active = [r for r in rows_out if not r.get("skipped")]

    # Audit ①: Full spr/drft/pex/tslp decomposition table across all 9 arms (Core 4 & ex-META & S4_600 by symbol/split)
    decomp_core4: dict[str, Any] = {}
    decomp_ex_meta: dict[str, Any] = {}
    for cohort_name, allowed_syms, dst in (
        ("core4", set(CORE_4_SYMBOLS), decomp_core4),
        ("ex_meta", set(EX_META_SYMBOLS), decomp_ex_meta),
    ):
        for arm_id in CORE4_ABLATION_ARMS_SPEC:
            grp = [r for r in active if r["arm"] == arm_id and r["symbol"] in allowed_syms]
            if not grp:
                continue
            spr = sum(float(r["spr"]) for r in grp)
            drft = sum(float(r["drft"]) for r in grp)
            pex = sum(float(r["pex"]) for r in grp)
            tslp = sum(float(r["tslp"]) for r in grp)
            realized_gross = spr + drft + pex + tslp
            unreal = sum(float(r.get("unrealized_mtm_usd", 0.0)) for r in grp)
            fee = sum(float(r["fee_usd"]) for r in grp)
            exec_notl = sum(float(r["target_executed_fill_notional_usd"]) for r in grp)
            mtm_net = realized_gross + unreal - fee
            dst[arm_id] = {
                "cohort": cohort_name,
                "spr": round(spr, 6),
                "drft": round(drft, 6),
                "pex": round(pex, 6),
                "tslp": round(tslp, 6),
                "realized_gross_usd": round(realized_gross, 6),
                "unrealized_mtm_usd": round(unreal, 6),
                "fee_usd": round(fee, 6),
                "mtm_net_usd": round(mtm_net, 6),
                "ledger_mtm_usd_per_m": round((-mtm_net / exec_notl) * 1e6, 4) if exec_notl > 0.0 else None,
            }

    # Build window returns map from S4_hold600s_only (same underlying stream for every arm)
    sym_win_ret_bps: dict[tuple[str, str], float] = {}
    for r in active:
        if r["arm"] == "S4_hold600s_only" and r.get("underlying_return_bps") is not None:
            sym_win_ret_bps[(str(r["win"]), str(r["symbol"]))] = float(r["underlying_return_bps"])
    win_ids = sorted({str(r["win"]) for r in active}, key=_window_sort_key)
    win_ew_ret_bps: dict[str, float] = {}
    for w in win_ids:
        vals = [sym_win_ret_bps[(w, s)] for s in CORE_4_SYMBOLS if (w, s) in sym_win_ret_bps]
        if vals:
            win_ew_ret_bps[w] = sum(vals) / len(vals)

    # Audit ②: Up-market vs Down-market windows
    regime_symmetry: dict[str, Any] = {"portfolio_by_arm": {}, "core4_600s_per_symbol": {}, "core4_600s_pooled_symbol_windows": {}}
    for arm_id in ("S4_hold600s_only", "S4_hold180s", "S4_speedbump_combined", "P3_patient_600s"):
        up_list: list[dict[str, Any]] = []
        dn_list: list[dict[str, Any]] = []
        for w in win_ids:
            if w not in win_ew_ret_bps:
                continue
            wr = [r for r in active if r["arm"] == arm_id and str(r["win"]) == w and r["symbol"] in CORE_4_SYMBOLS]
            if not wr:
                continue
            rec = {
                "win": w,
                "ret_bps": win_ew_ret_bps[w],
                "mtm_net_usd": sum(float(x["mtm_net_usd"]) for x in wr),
                "target_executed_fill_notional_usd": sum(float(x["target_executed_fill_notional_usd"]) for x in wr),
                "spr": sum(float(x["spr"]) for x in wr),
                "drft": sum(float(x["drft"]) for x in wr),
                "pex": sum(float(x["pex"]) for x in wr),
                "tslp": sum(float(x["tslp"]) for x in wr),
            }
            if win_ew_ret_bps[w] > 0.0:
                up_list.append(rec)
            else:
                dn_list.append(rec)
        regime_symmetry["portfolio_by_arm"][arm_id] = compute_regime_symmetry(up_list, dn_list)

    pooled_up: list[dict[str, Any]] = []
    pooled_dn: list[dict[str, Any]] = []
    for sym in CORE_4_SYMBOLS:
        s_up: list[dict[str, Any]] = []
        s_dn: list[dict[str, Any]] = []
        for w in win_ids:
            if (w, sym) not in sym_win_ret_bps:
                continue
            matches = [r for r in active if r["arm"] == "S4_hold600s_only" and str(r["win"]) == w and r["symbol"] == sym]
            if not matches:
                continue
            r = matches[0]
            rec = {
                "win": w,
                "symbol": sym,
                "ret_bps": sym_win_ret_bps[(w, sym)],
                "mtm_net_usd": float(r["mtm_net_usd"]),
                "target_executed_fill_notional_usd": float(r["target_executed_fill_notional_usd"]),
                "spr": float(r["spr"]),
                "drft": float(r["drft"]),
                "pex": float(r["pex"]),
                "tslp": float(r["tslp"]),
            }
            if rec["ret_bps"] > 0.0:
                s_up.append(rec)
                pooled_up.append(rec)
            else:
                s_dn.append(rec)
                pooled_dn.append(rec)
        regime_symmetry["core4_600s_per_symbol"][sym] = compute_regime_symmetry(s_up, s_dn)
    regime_symmetry["core4_600s_pooled_symbol_windows"] = compute_regime_symmetry(pooled_up, pooled_dn)

    # Audit ④: OLS Beta Regressions (`pnl_usd ~ alpha + beta * underlying_ret_pct`)
    beta_regressions: dict[str, Any] = {"portfolio_by_arm": {}, "s4_hold600s_per_symbol": {}, "s4_hold600s_pooled_84": {}}
    for arm_id in ("S4_hold600s_only", "S4_hold180s", "S4_speedbump_combined", "P3_patient_600s"):
        xs_pct: list[float] = []
        ys_usd: list[float] = []
        for w in win_ids:
            if w not in win_ew_ret_bps:
                continue
            wr = [r for r in active if r["arm"] == arm_id and str(r["win"]) == w and r["symbol"] in CORE_4_SYMBOLS]
            if not wr:
                continue
            xs_pct.append(win_ew_ret_bps[w] / 100.0)
            ys_usd.append(sum(float(x["mtm_net_usd"]) for x in wr))
        beta_regressions["portfolio_by_arm"][arm_id] = {
            "x_unit": "equal_weight_core4_return_pct",
            "y_unit": "window_mtm_net_usd",
            **compute_ols_beta_regression(xs_pct, ys_usd),
        }

    pooled_x: list[float] = []
    pooled_y: list[float] = []
    for sym in CORE_4_SYMBOLS:
        sx: list[float] = []
        sy: list[float] = []
        for w in win_ids:
            if (w, sym) not in sym_win_ret_bps:
                continue
            matches = [r for r in active if r["arm"] == "S4_hold600s_only" and str(r["win"]) == w and r["symbol"] == sym]
            if not matches:
                continue
            ret_pct = sym_win_ret_bps[(w, sym)] / 100.0
            pnl_usd = float(matches[0]["mtm_net_usd"])
            sx.append(ret_pct)
            sy.append(pnl_usd)
            pooled_x.append(ret_pct)
            pooled_y.append(pnl_usd)
        beta_regressions["s4_hold600s_per_symbol"][sym] = {
            "x_unit": f"{sym}_window_return_pct",
            "y_unit": f"{sym}_window_mtm_net_usd",
            **compute_ols_beta_regression(sx, sy),
        }
    beta_regressions["s4_hold600s_pooled_84"] = {
        "x_unit": "symbol_window_return_pct",
        "y_unit": "symbol_window_mtm_net_usd",
        **compute_ols_beta_regression(pooled_x, pooled_y),
    }

    return (
        {"core4": decomp_core4, "ex_meta": decomp_ex_meta},
        regime_symmetry,
        beta_regressions,
    )


def run_core4_qualitative_audit(
    lakehouse_dir: Path,
    *,
    split: str = "all",
    window_ids: Sequence[str] | None = None,
    rebate_bps: float = 0.0,
    fee_evidence_path: Path | None = None,
    max_workers: int = 4,
) -> dict[str, Any]:
    """Run all 4 Track 03 qualitative verification audits (decomposition, up/down symmetry, book-cross toxic fills, beta)."""
    ablation_payload = run_core4_hold_ablation(
        lakehouse_dir,
        split=split,
        window_ids=window_ids,
        arm_names=None,
        rebate_bps=rebate_bps,
        fee_evidence_path=fee_evidence_path,
        max_workers=max_workers,
        include_rows=True,
        verify_parity=False,
    )
    rows_out = ablation_payload.pop("rows", [])
    decomp, regime_sym, beta_reg = _build_qualitative_from_baseline_rows(rows_out)

    # Audit ③: Unprinted book-cross toxic fill stress across W01-W27
    all_discovered = discover_campaigns_w27(lakehouse_dir, split, window_ids)
    selected_windows = {w: m for w, m in all_discovered.items() if "rwa" in m}
    mk_bps, tk_bps, _, _, _ = resolve_core4_rh_fee_provenance(fee_evidence_path)
    tox_rows: list[dict[str, Any]] = []
    work_items = list(selected_windows.items())
    eff_workers = max(1, min(int(max_workers), len(work_items)))
    if eff_workers == 1:
        for win_id, win_map in work_items:
            tox_rows.extend(
                _eval_qualitative_toxic_window(
                    lakehouse_dir,
                    win_id,
                    win_map,
                    mk_bps,
                    tk_bps,
                    rebate_bps,
                    QUALITATIVE_TOXIC_ARMS,
                    QUALITATIVE_TOXIC_CROSS_SCENARIOS,
                )
            )
    else:
        with concurrent.futures.ProcessPoolExecutor(max_workers=eff_workers) as pool:
            futs = [
                pool.submit(
                    _eval_qualitative_toxic_window,
                    lakehouse_dir,
                    win_id,
                    win_map,
                    mk_bps,
                    tk_bps,
                    rebate_bps,
                    QUALITATIVE_TOXIC_ARMS,
                    QUALITATIVE_TOXIC_CROSS_SCENARIOS,
                )
                for win_id, win_map in work_items
            ]
            for fut in concurrent.futures.as_completed(futs):
                tox_rows.extend(fut.result())

    assert_no_w28_leak(list(selected_windows.keys()), tox_rows)

    toxic_matrix: dict[str, Any] = {}
    for sc_id, mode, frac in QUALITATIVE_TOXIC_CROSS_SCENARIOS:
        sc_arms: dict[str, Any] = {}
        for arm_id in QUALITATIVE_TOXIC_ARMS:
            grp_c4 = [r for r in tox_rows if r["scenario_id"] == sc_id and r["arm"] == arm_id and r["symbol"] in CORE_4_SYMBOLS]
            grp_em = [r for r in tox_rows if r["scenario_id"] == sc_id and r["arm"] == arm_id and r["symbol"] in EX_META_SYMBOLS]
            if not grp_c4:
                continue

            def _agg_tox(g: Sequence[dict[str, Any]]) -> dict[str, Any]:
                by_w: dict[str, float] = defaultdict(float)
                for r in g:
                    by_w[str(r["win"])] += float(r["mtm_net_usd"])
                mtm = sum(by_w.values())
                exec_n = sum(float(r["target_executed_fill_notional_usd"]) for r in g)
                ubx_f = sum(int(r["unprinted_book_cross_fills"]) for r in g)
                ubx_n = sum(float(r["unprinted_book_cross_notional_usd"]) for r in g)
                spr = sum(float(r["spr"]) for r in g)
                drft = sum(float(r["drft"]) for r in g)
                pex = sum(float(r["pex"]) for r in g)
                tslp = sum(float(r["tslp"]) for r in g)
                pos_w = sum(1 for v in by_w.values() if v > 0.0)
                return {
                    "windows": len(by_w),
                    "positive_window_ratio": round(pos_w / len(by_w), 4) if by_w else 0.0,
                    "mtm_net_usd": round(mtm, 4),
                    "ledger_mtm_usd_per_m": round((-mtm / exec_n) * 1e6, 4) if exec_n > 0.0 else None,
                    "target_executed_fill_notional_usd": round(exec_n, 2),
                    "unprinted_book_cross_fills": ubx_f,
                    "unprinted_book_cross_notional_usd": round(ubx_n, 2),
                    "spr": round(spr, 4),
                    "drft": round(drft, 4),
                    "pex": round(pex, 4),
                    "tslp": round(tslp, 4),
                }

            sc_arms[arm_id] = {
                "core4": _agg_tox(grp_c4),
                "ex_meta": _agg_tox(grp_em),
            }
        toxic_matrix[sc_id] = {
            "book_cross_mode": mode,
            "book_cross_toxic_fill_frac": frac,
            "arms": sc_arms,
        }

    return {
        "schema": "track03_core4_qualitative_audit_w01_w27_v1",
        "w28_excluded": True,
        "target_venue": TARGET_VENUE,
        "windows_count": len(selected_windows),
        "audit_1_decomposition_spr_drft_pex_tslp": decomp,
        "audit_2_up_vs_down_symmetry": regime_sym,
        "audit_3_unprinted_book_cross_toxic_fills": toxic_matrix,
        "audit_4_ols_beta_regressions": beta_reg,
        "multiple_testing": ablation_payload["multiple_testing"],
    }


def build_w28plus_prereg_plan(
    *,
    baseline_commit: str = "9c9eaf2",
    candidate_arm: str = "S4_hold600s_only",
    status: str = "PRE_REGISTERED_FROZEN_HOLD",
    shadow_config_path: str = "examples/track03_core4_patient_shadow.toml",
    shadow_config_sha256: str | None = None,
    fee_evidence_sha256: str = EXPECTED_FEE_EVIDENCE_SHA256,
    max_inventory_hold_ms: int = 600_000,
    reduce_only_penny_ticks: int = 0,
    inventory_skew_bps_per_full_limit: float = 2.0,
    min_w28plus_windows: int = 10,
) -> dict[str, Any]:
    """Build deterministic canonical W28+ pre-registered experiment manifest with Roadmap §4 & §5 + qualitative gates."""
    candidate_spec = CORE4_ABLATION_ARMS_SPEC[candidate_arm]
    baseline_spec = CORE4_ABLATION_ARMS_SPEC["S4_speedbump_combined"]
    plan_core: dict[str, Any] = {
        "schema": "aleph_tx_track03_w28plus_prereg_plan_v2",
        "track": "03_incentive_turnover",
        "status": status,
        "baseline_git_commit": baseline_commit,
        "target_venue": TARGET_VENUE,
        "target_exchange_id": 8,
        "ref_exchange_ids": [7, 10],
        "ref_venues": ["arcus", "trade_xyz"],
        "target_settlement": TARGET_SETTLEMENT,
        "fee_tier": {
            "maker_fee_bps": 0.0,
            "taker_fee_bps": 0.0,
            "rebate_bps": 0.0,
            "account_fee_status": "verified_account_tier",
            "fee_evidence_sha256": fee_evidence_sha256,
        },
        "universe": {
            "cohort": "Core_4_RWA",
            "symbols": list(CORE_4_SYMBOLS),
            "ex_meta_sensitivity_symbols": list(EX_META_SYMBOLS),
            "excluded_symbols": ["QQQ", "SPY", "NVDA", "TSLA", "GOOGL", "AMD", "MU", "COIN", "CRCL", "PLTR", "SNDK"],
        },
        "window_eligibility": {
            "min_window_index": 28,
            "regex_include": "^tokyo-rwa-.*-w(2[89]|[3-9][0-9]|[1-9][0-9]{2,})$",
            "continuous_10h_ms": TEN_HOURS_MS,
            "max_continuous_gap_ms": MAX_CONTINUOUS_GAP_MS,
            "terminal_flatten_lead_ms": 1_000,
            "cross_window_state_reset": False,
            "zero_fill_windows_policy": "retain_in_coverage_and_risk_denominators_with_unit_cost_null",
            "min_independent_windows": min_w28plus_windows,
        },
        "hypothesis": {
            "primary_comparison": f"{candidate_arm} vs frozen S4_speedbump_combined",
            "causal_mechanism": (
                "Extending max_hold_ms on Core 4 while locking quoting, latency, queue, and drain "
                "eliminates forced 330 ms IOC churn without adding penny-tick adverse selection, "
                "conditional on directional symmetry, market-neutral beta, and unprinted book-cross survival."
            ),
            "baseline_arm": {
                "name": "S4_speedbump_combined",
                "max_hold_ms": baseline_spec["max_hold_ms"],
                "reduce_only_penny_ticks": baseline_spec["reduce_only_penny_ticks"],
                "inventory_skew_bps_per_full_limit": 2.0,
            },
            "candidate_arm": {
                "name": candidate_arm,
                "max_hold_ms": max_inventory_hold_ms,
                "reduce_only_penny_ticks": reduce_only_penny_ticks,
                "inventory_skew_bps_per_full_limit": inventory_skew_bps_per_full_limit,
                "latency_triple_ms": list(PRIMARY_LATENCY_TRIPLE),
            },
            "secondary_attribution_arm": "P3_patient_600s",
            "pre_registered_trial_cap": 3,
            "multiple_testing_disclosure": (
                "Do not treat DSR(N_eff) near 1.0 as standalone robustness evidence when N_eff < 2 across correlated rungs; "
                "audit cumulative M >= 50 (including universe selection, patient frontier, and latency grid) plus Gates 10-12."
            ),
        },
        "accounting_contract": {
            "primary_unit_metric": "ledger_mtm_usd_per_m = sum(net_cash_cost_usd) / sum(target_executed_fill_notional_usd) * 1e6",
            "secondary_unit_metric": "historical_double_sided_mtm_usd_per_m = sum(net_cash_cost_usd) / (2 * sum(ntl)) * 1e6",
            "separate_realized_vs_mtm": True,
            "zero_rebate_baseline": True,
            "bootstrap_method": "politis_romano_stationary_block_bootstrap",
            "bootstrap_params": {
                "expected_block_len": DEFAULT_PREREG_BLOCK_LEN,
                "n_boot": DEFAULT_PREREG_N_BOOT,
                "seed": DEFAULT_PREREG_BOOTSTRAP_SEED,
            },
        },
        "falsification_thresholds": {
            "1_cost_reduction_vs_s4": {
                "require_ledger_mtm_usd_per_m_lt": 0.0,
                "require_delta_ledger_vs_s4_usd_per_m_le": -5.0,
                "require_bootstrap_ci95_delta_ledger_high_lt": 0.0,
            },
            "2_volume_floor": {
                "min_target_executed_fill_notional_per_window_usd": 400_000.0,
                "min_maker_volume_per_window_usd": 380_000.0,
            },
            "3_absolute_cash_cost": {
                "max_mtm_net_cash_cost_usd": 0.0,
                "max_realized_net_cash_cost_usd": 0.0,
                "min_positive_window_ratio": 0.70,
            },
            "4_max_gross_exposure": {
                "max_single_symbol_gross_exposure_usd": 650.0,
                "max_portfolio_gross_exposure_usd": 1600.0,
            },
            "5_hold_tail_bounds": {
                "max_hold_p95_ms": 180_000.0,
                "max_hold_observed_ms": max_inventory_hold_ms + 15_000,
            },
            "6_forced_ioc_share": {
                "max_taker_exit_share_s": 0.03,
            },
            "7_terminal_residual_cap": {
                "max_terminal_residual_per_window_usd": 25.0,
            },
            "8_ex_meta_stability_cap": {
                "require_ex_meta_ledger_mtm_usd_per_m_le": 0.0,
                "max_ex_meta_mtm_net_cash_cost_usd": 0.0,
            },
            "9_single_symbol_share_cap": {
                "max_single_symbol_positive_pnl_share": 0.80,
            },
            "10_directional_symmetry": {
                "min_windows_per_direction": 3,
                "max_up_windows_ledger_mtm_usd_per_m": 0.0,
                "max_down_windows_ledger_mtm_usd_per_m": 0.0,
                "min_positive_ratio_each_direction": 0.50,
            },
            "11_market_neutrality_beta": {
                "max_abs_t_beta": 2.0,
                "max_r_squared": 0.25,
                "min_alpha_usd_per_window": 0.0,
            },
            "12_toxic_book_cross_robustness": {
                "book_cross_mode": "strict_through",
                "book_cross_toxic_fill_frac": 0.50,
                "max_ledger_mtm_usd_per_m": 0.0,
            },
        },
        "capital_and_stop_conditions": {
            "isolated_capital_usd": 10_000.0,
            "daily_loss_limit_pct": 0.02,
            "max_session_loss_per_symbol_usd": 20.0,
            "operational_mode": "HOLD_SHADOW_REPLAY_ONLY",
            "shadow_config_path": shadow_config_path,
            "shadow_config_sha256": shadow_config_sha256,
        },
        "candidate_spec": candidate_spec,
    }
    plan_sha = canonical_json_sha256(plan_core)
    return {
        **plan_core,
        "plan_sha256": plan_sha,
    }


def run_self_test() -> dict[str, Any]:
    """Verify exact parity with `simulate_prepared_arm`, Roadmap §2 accounting, bootstrap, and prereg SHA-256."""
    tick = 0.01
    events: list[dict[str, Any]] = [
        {"ts": 0, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.06, 10.0)]},
        {"ts": 0, "kind": "ref", "venue": "arcus", "bid": 100.00, "ask": 100.06},
        {"ts": 300, "kind": "trade", "side": "SELL", "price": 100.00, "size": 8.0},
        {"ts": 4_000, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.06, 10.0)]},
        {"ts": 8_000, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.06, 10.0)]},
        {"ts": 12_000, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.06, 10.0)]},
        {"ts": 16_000, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.06, 10.0)]},
        {"ts": 16_500, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.06, 10.0)]},
        {"ts": 20_500, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.06, 10.0)]},
        {"ts": 25_000, "kind": "trade", "side": "BUY", "price": 100.06, "size": 8.0},
        {"ts": 29_000, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.06, 10.0)]},
        {"ts": 31_000, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.06, 10.0)]},
        {"ts": 90_000, "kind": "ref", "venue": "arcus", "bid": 101.00, "ask": 101.06},
    ]
    stream = prepare_replay_stream(events)
    arms = build_core4_ablation_arms(PRIMARY_LATENCY_TRIPLE, maker_fee_bps=0.0, taker_fee_bps=0.0)
    results: dict[str, Any] = {}
    max_parity_diff = 0.0
    for arm in arms:
        r_new = simulate_core4_arm(stream, tick, arm, declared_end_ms=31_000)
        r_ref = simulate_prepared_arm(stream, tick, arm, declared_end_ms=31_000)
        for key in ("net", "realized_net_usd", "ntl", "maker_volume_usd", "taker_volume_usd", "stale_exit_blocks", "fills", "cyc"):
            diff = abs(float(r_new[key]) - float(r_ref[key]))
            max_parity_diff = max(max_parity_diff, diff)
            assert diff <= 1e-12, f"Parity mismatch on {arm.name}.{key}: {r_new[key]} != {r_ref[key]}"
        results[arm.name] = r_new

    assert results["S4_speedbump_combined"]["taker_exits"] >= 1
    assert results["S4_hold600s_only"]["taker_exits"] == 0
    assert results["S4_hold600s_only"]["hold_p50_ms"] is not None
    assert results["S4_hold600s_only"]["max_gross_exposure_usd"] > 0.0

    p1 = build_w28plus_prereg_plan()
    p2 = build_w28plus_prereg_plan()
    assert p1["plan_sha256"] == p2["plan_sha256"]

    return {
        "self_test": "ok",
        "arms_count": len(CORE4_ABLATION_ARMS_SPEC),
        "max_parity_diff": max_parity_diff,
        "s4_net_usd": results["S4_speedbump_combined"]["mtm_net_usd"],
        "s4_hold600s_only_net_usd": results["S4_hold600s_only"]["mtm_net_usd"],
        "s4_hold600s_only_ledger_usd_per_m": results["S4_hold600s_only"]["ledger_mtm_usd_per_m"],
        "prereg_plan_sha256": p1["plan_sha256"],
    }


def main(argv: Sequence[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description="Track 03 Core 4 Single-Factor Hold Ablation & W28+ Preregistration")
    ap.add_argument("--lakehouse", type=Path, default=Path(os.path.expanduser("~/aleph-tx-lakehouse")))
    ap.add_argument(
        "--split",
        choices=("selection_dev", "holdout", "w26_w27_oos", "w26_oos", "w27_oos", "all"),
        default="all",
    )
    ap.add_argument("--windows", type=str, default=None)
    ap.add_argument("--arms", type=str, default=None)
    ap.add_argument("--rebate-bps", type=float, default=0.0)
    ap.add_argument("--fee-evidence", type=Path, default=None)
    ap.add_argument("--max-workers", type=int, default=4)
    ap.add_argument("--include-rows", action="store_true")
    ap.add_argument("--no-parity-check", action="store_true")
    ap.add_argument("--output", type=Path, default=None)
    ap.add_argument("--prereg-output", type=Path, default=None)
    ap.add_argument("--prereg-candidate-arm", type=str, default="S4_hold600s_only")
    ap.add_argument("--prereg-hold-ms", type=int, default=600_000)
    ap.add_argument("--prereg-penny-ticks", type=int, default=0)
    ap.add_argument("--prereg-skew-bps", type=float, default=2.0)
    ap.add_argument("--prereg-status", type=str, default="PRE_REGISTERED_FROZEN_HOLD")
    ap.add_argument("--qualitative-audit-output", type=Path, default=None)
    ap.add_argument("--shadow-config", type=Path, default=None)
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

    if args.prereg_output and not args.windows and not args.output and not args.qualitative_audit_output and not args.lakehouse.exists():
        sh_sha = sha256_file(args.shadow_config) if (args.shadow_config and args.shadow_config.is_file()) else None
        plan = build_w28plus_prereg_plan(
            candidate_arm=args.prereg_candidate_arm,
            status=args.prereg_status,
            max_inventory_hold_ms=args.prereg_hold_ms,
            reduce_only_penny_ticks=args.prereg_penny_ticks,
            inventory_skew_bps_per_full_limit=args.prereg_skew_bps,
            shadow_config_sha256=sh_sha,
        )
        args.prereg_output.parent.mkdir(parents=True, exist_ok=True)
        args.prereg_output.write_text(json.dumps(plan, indent=2, sort_keys=True, allow_nan=False) + "\n", encoding="utf-8")
        print(json.dumps({"prereg_output": str(args.prereg_output), "plan_sha256": plan["plan_sha256"]}))
        return 0

    if args.rebate_bps < 0.0 or not math.isfinite(args.rebate_bps):
        ap.error("--rebate-bps must be finite and >= 0")

    win_list = [w.strip() for w in args.windows.split(",") if w.strip()] if args.windows else None
    arm_list = [a.strip() for a in args.arms.split(",") if a.strip()] if args.arms else None

    if args.qualitative_audit_output:
        qual = run_core4_qualitative_audit(
            args.lakehouse,
            split=args.split,
            window_ids=win_list,
            rebate_bps=args.rebate_bps,
            fee_evidence_path=args.fee_evidence,
            max_workers=args.max_workers,
        )
        args.qualitative_audit_output.parent.mkdir(parents=True, exist_ok=True)
        args.qualitative_audit_output.write_text(json.dumps(qual, indent=2, allow_nan=False) + "\n", encoding="utf-8")
        print(
            json.dumps(
                {
                    "qualitative_audit_output": str(args.qualitative_audit_output),
                    "sha256": sha256_file(args.qualitative_audit_output),
                    "windows_count": qual["windows_count"],
                }
            )
        )
        if not args.output and not args.prereg_output:
            return 0

    result = run_core4_hold_ablation(
        args.lakehouse,
        split=args.split,
        window_ids=win_list,
        arm_names=arm_list,
        rebate_bps=args.rebate_bps,
        fee_evidence_path=args.fee_evidence,
        max_workers=args.max_workers,
        include_rows=args.include_rows,
        verify_parity=not args.no_parity_check,
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
                    "invariants": result["invariants"],
                    "windows": sorted(result["windows"].keys(), key=_window_sort_key),
                }
            )
        )
    else:
        print(rendered)

    if args.prereg_output:
        sh_sha = sha256_file(args.shadow_config) if (args.shadow_config and args.shadow_config.is_file()) else None
        plan = build_w28plus_prereg_plan(
            candidate_arm=args.prereg_candidate_arm,
            status=args.prereg_status,
            max_inventory_hold_ms=args.prereg_hold_ms,
            reduce_only_penny_ticks=args.prereg_penny_ticks,
            inventory_skew_bps_per_full_limit=args.prereg_skew_bps,
            shadow_config_sha256=sh_sha,
        )
        args.prereg_output.parent.mkdir(parents=True, exist_ok=True)
        args.prereg_output.write_text(json.dumps(plan, indent=2, sort_keys=True, allow_nan=False) + "\n", encoding="utf-8")
        print(json.dumps({"prereg_output": str(args.prereg_output), "plan_sha256": plan["plan_sha256"]}))

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
