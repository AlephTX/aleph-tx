#!/usr/bin/env python3
"""Track 03 Full 18-Market Universe Discovery & Speed-Bump Quoting Evaluator (`W01-W26`).

Strictly stdlib-only module top-level (`duckdb` imported lazily inside lakehouse runner).
Reuses:
  - `eval_volume_farming_cost.py`: `compute_volume_farming_cost` (`rebate_bps = 0.0`)
  - `track_fee_evidence.py`: `OPT-01` capability & fee provenance
  - `audit_multiple_testing_overfitting.py`: `PSR`, `DSR`, `MinTRL`, and `CSCV PBO`

Implements Codex #140 & Gemini #140 Realism fixes:
  1. Displayed L1 depth clamping (`cur_bsz` / `cur_asz`) on both intraday IOC exits
     and terminal end-of-window flatten, decrementing remaining displayed size until
     the next book refresh and tracking `depth_clipped_exits`.
  2. Strict book freshness check (`<= max_book_age_ms`) on quoting, resting queue
     retention, intraday IOC exits, and terminal flatten.
  3. Explicit `330 ms` `exit_delay_ms` on BOTH intraday IOC exits AND terminal
     end-of-window flatten (never silently flattening at `last_ts` with 0 ms delay
     or infinite liquidity).
  4. Explicit reporting of `terminal_residual_notional_usd`, `depth_clipped_exits`,
     `realized_only_usd_per_m`, and `mtm_incl_residual_usd_per_m`.
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

from audit_multiple_testing_overfitting import (  # noqa: E402
    cscv_pbo,
    deflated_sharpe_report,
    probabilistic_sharpe_ratio,
    series_moments,
)
from eval_volume_farming_cost import compute_volume_farming_cost  # noqa: E402
from track_fee_evidence import (  # noqa: E402
    VENUE_CAPABILITY_SNAPSHOT_VERSION,
    build_fee_provenance,
    load_track_fee_evidence,
    resolve_venue_fee,
)

TEN_HOURS_MS = 36_000_000
MAX_CONTINUOUS_GAP_MS = 60_000
DEFAULT_EXIT_DELAY_MS = 330

CRYPTO_SYMBOLS: tuple[str, ...] = ("BTC", "ETH", "SOL")
LEGACY_5_RWA_SYMBOLS: tuple[str, ...] = ("AAPL", "SPY", "QQQ", "NVDA", "TSLA")
LEGACY_5_RWA_EX_NVDA: tuple[str, ...] = ("AAPL", "SPY", "QQQ", "TSLA")

STAGE_LATENCY_TRIPLES: tuple[tuple[int, int, int], ...] = (
    (200, 300, 330),
    (300, 300, 330),
)

_CAMPAIGN_RE = re.compile(r"^tokyo-(crypto|rwa)-.+-w(\d+)$")

# Verified full market ID map (confirmed on Aleph w26).
VENUE_SYMBOL_MAP: dict[str, dict[str, str]] = {
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
        "SNDK": "32",
        "PLTR": "34",
    },
    "lighter": {
        "ETH": "0",
        "BTC": "1",
        "SOL": "2",
        "HOOD": "108",
        "COIN": "109",
        "NVDA": "110",
        "PLTR": "111",
        "TSLA": "112",
        "AAPL": "113",
        "AMZN": "114",
        "MSFT": "115",
        "GOOGL": "116",
        "META": "117",
        "MSTR": "122",
        "SPY": "128",
        "QQQ": "129",
        "AMD": "138",
        "SNDK": "139",
        "MU": "164",
    },
    "arcus": {
        "BTC": "BTC-USD",
        "ETH": "ETH-USD",
        "SOL": "SOL-USD",
        "AAPL": "AAPL-USD",
        "AMZN": "AMZN-USD",
        "GOOGL": "GOOGL-USD",
        "META": "META-USD",
        "MSFT": "MSFT-USD",
        "NVDA": "NVDA-USD",
        "TSLA": "TSLA-USD",
        "COIN": "COIN-USD",
        "CRCL": "CRCL-USD",
        "QQQ": "QQQ-USD",
        "SPY": "SPY-USD",
        "AMD": "AMD-USD",
        "MU": "MU-USD",
        "SNDK": "SNDK-USD",
        "PLTR": "PLTR-USD",
        "HOOD": "HOOD-USD",
        "MSTR": "MSTR-USD",
    },
    "trade_xyz": {
        "AAPL": "xyz:AAPL",
        "AMZN": "xyz:AMZN",
        "GOOGL": "xyz:GOOGL",
        "META": "xyz:META",
        "MSFT": "xyz:MSFT",
        "NVDA": "xyz:NVDA",
        "TSLA": "xyz:TSLA",
        "COIN": "xyz:COIN",
        "CRCL": "xyz:CRCL",
        "QQQ": "xyz:XYZ100",
        "SPY": "xyz:SP500",
        "AMD": "xyz:AMD",
        "MU": "xyz:MU",
        "SNDK": "xyz:SNDK",
        "PLTR": "xyz:PLTR",
        "HOOD": "xyz:HOOD",
        "MSTR": "xyz:MSTR",
    },
    "binance": {
        "BTC": "BTCUSDT",
        "ETH": "ETHUSDT",
        "SOL": "SOLUSDT",
    },
}

VENUE_SETTLEMENT: dict[str, str] = {
    "lighter": "USDC",
    "lighter_robinhood": "USDG",
}

ALL_RWA_SYMBOLS: tuple[str, ...] = tuple(
    sorted(
        {
            sym
            for v in ("lighter_robinhood", "lighter")
            for sym in VENUE_SYMBOL_MAP[v]
            if sym not in CRYPTO_SYMBOLS
        }
    )
)

# G5 base microstructure params (preserved intact with reduce_only_penny_ticks=0).
G5_BASE_PARAMS: dict[str, Any] = {
    "microprice_alpha_weight": 0.25,
    "tfi_ewma_tau_ms": 2_000.0,
    "tfi_skew_bps": 0.0,
    "tfi_veto_threshold": 0.75,
    "reduce_only_penny_ticks": 0,
}

# Frozen Stage B speed-bump arm specifications (calibrated on selection_dev w01-w20 only).
FROZEN_STAGE_B_PARAMS: dict[str, dict[str, Any]] = {
    "G4_baseline": {
        "reduce_only_penny_ticks": 0,
    },
    "G5_microprice_tfi": dict(G5_BASE_PARAMS),
    "S1_latency_spread_floor": {
        **G5_BASE_PARAMS,
        "use_speed_bump_cancel_ms": True,
        "latency_vol_spread_mult": 1.25,
    },
    "S2_cross_venue_fair_shift": {
        **G5_BASE_PARAMS,
        "cross_venue_fair_weight": 0.75,
        "cross_venue_fair_cap_bps": 4.0,
    },
    "S3_toxicity_circuit_breaker": {
        **G5_BASE_PARAMS,
        "toxicity_pause_ewma_bps": 1.50,
        "toxicity_pause_ms": 60_000,
        "toxicity_min_fills": 6,
    },
    "S4_speedbump_combined": {
        **G5_BASE_PARAMS,
        "use_speed_bump_cancel_ms": True,
        "latency_vol_spread_mult": 1.25,
        "cross_venue_fair_weight": 0.75,
        "cross_venue_fair_cap_bps": 4.0,
        "toxicity_pause_ewma_bps": 1.50,
        "toxicity_pause_ms": 60_000,
        "toxicity_min_fills": 6,
    },
}


@dataclasses.dataclass(frozen=True)
class UniverseTarget:
    """Target `(venue, domain, symbol)` spec with explicit settlement, market ID, and OPT-01 fee provenance."""

    venue: str
    domain: str
    symbol: str
    raw_market: str
    settlement: str
    maker_fee_bps: float
    taker_fee_bps: float
    account_fee_status: str
    ref_venues: tuple[str, ...]
    fee_source: str


def build_full_universe_targets(
    *,
    fee_evidence_path: Path | None = None,
    domains: Sequence[str] = ("crypto", "rwa"),
    venues: Sequence[str] = ("lighter_robinhood", "lighter"),
    symbols: Sequence[str] | None = None,
) -> tuple[list[UniverseTarget], dict[str, Any]]:
    """Build full 18 `lighter_robinhood` + 19 `lighter` target matrix with OPT-01 fee provenance."""
    ev = load_track_fee_evidence(fee_evidence_path)
    domain_set = set(domains)
    sym_filter = {s.upper() for s in symbols} if symbols else None
    targets: list[UniverseTarget] = []

    for venue in venues:
        if venue not in VENUE_SETTLEMENT:
            raise ValueError(f"Unsupported venue {venue!r}; expected one of {sorted(VENUE_SETTLEMENT)}")
        settlement = VENUE_SETTLEMENT[venue]

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
                default_status="verified_account_tier" if ev.get("loaded") else "public_tier0_unverified",
                evidence=ev,
            )
            maker_bps, taker_bps, fee_status, fee_source = mk, tk, st, src

        for sym, raw_mkt in VENUE_SYMBOL_MAP[venue].items():
            if sym_filter and sym not in sym_filter:
                continue
            dom = "crypto" if sym in CRYPTO_SYMBOLS else "rwa"
            if dom not in domain_set:
                continue
            ref_vs: tuple[str, ...] = ("binance", "arcus") if dom == "crypto" else ("arcus", "trade_xyz")
            targets.append(
                UniverseTarget(
                    venue=venue,
                    domain=dom,
                    symbol=sym,
                    raw_market=raw_mkt,
                    settlement=settlement,
                    maker_fee_bps=maker_bps,
                    taker_fee_bps=taker_bps,
                    account_fee_status=fee_status,
                    ref_venues=ref_vs,
                    fee_source=fee_source,
                )
            )

    prov = build_fee_provenance(ev, venues_in_scope=tuple(venues))
    prov["lighter_standard_scenario"] = True
    prov["lighter_fee_status"] = "lighter_standard_zero_scenario"
    prov["lighter_premium_reference_bps"] = [0.40, 2.80]
    return targets, prov


@dataclasses.dataclass(frozen=True)
class SpeedbumpArmConfig:
    """Rust-aligned QueuePriorityMmConfig mirror with Codex #140 realism & speed-bump controls."""

    name: str
    order_delay_ms: int
    cancel_delay_ms: int
    exit_delay_ms: int = DEFAULT_EXIT_DELAY_MS
    quote_usd: float = 200.0
    max_pos_usd: float = 400.0
    max_hold_ms: int = 15_000
    max_book_age_ms: int = 5_000
    terminal_flatten_lead_ms: int = 1_000
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
    # G5 microstructure fields:
    microprice_alpha_weight: float = 0.0
    tfi_ewma_tau_ms: float = 0.0
    tfi_skew_bps: float = 0.0
    tfi_veto_threshold: float = 0.0
    self_fill_stepback_bps: float = 0.0
    self_fill_decay_ms: int = 0
    reduce_only_penny_ticks: int = 0
    # Speed-bump optimal quoting & toxicity circuit breaker fields (default-off):
    speed_bump_cancel_ms: float = 0.0
    latency_vol_spread_mult: float = 0.0
    cross_venue_fair_weight: float = 0.0
    cross_venue_fair_cap_bps: float = 0.0
    toxicity_pause_ewma_bps: float = 0.0
    toxicity_pause_ms: int = 0
    toxicity_min_fills: int = 0
    # Opt-in sensitivity for unprinted book-cross toxic fills (0.0 keeps default parity intact):
    book_cross_toxic_fill_frac: float = 0.0
    book_cross_mode: str = "strict_through"

    @property
    def latency_label(self) -> str:
        return f"o{self.order_delay_ms}_c{self.cancel_delay_ms}_e{self.exit_delay_ms}"


def build_stage_a_arms(
    latency_triples: Sequence[tuple[int, int, int]] = STAGE_LATENCY_TRIPLES,
    *,
    maker_fee_bps: float = 0.0,
    taker_fee_bps: float = 0.0,
    rebate_bps: float = 0.0,
) -> list[SpeedbumpArmConfig]:
    """Build Stage A full-universe discovery arms (`G5_microprice_tfi` at 200/300/330 & 300/300/330)."""
    return [
        SpeedbumpArmConfig(
            name="G5_microprice_tfi",
            order_delay_ms=o_ms,
            cancel_delay_ms=c_ms,
            exit_delay_ms=e_ms,
            maker_fee_bps=maker_fee_bps,
            taker_fee_bps=taker_fee_bps,
            rebate_bps=rebate_bps,
            **G5_BASE_PARAMS,
        )
        for o_ms, c_ms, e_ms in latency_triples
    ]


def build_stage_b_arms(
    latency_triples: Sequence[tuple[int, int, int]] = STAGE_LATENCY_TRIPLES,
    *,
    maker_fee_bps: float = 0.0,
    taker_fee_bps: float = 0.0,
    rebate_bps: float = 0.0,
    arm_names: Sequence[str] | None = None,
    param_overrides: dict[str, dict[str, Any]] | None = None,
) -> list[SpeedbumpArmConfig]:
    """Build Stage B speed-bump ablation arms (`G4`, `G5`, `S1`..`S4`)."""
    spec_map = param_overrides if param_overrides is not None else FROZEN_STAGE_B_PARAMS
    selected_names = list(arm_names) if arm_names is not None else list(spec_map.keys())
    unknown = sorted(set(selected_names) - set(spec_map.keys()))
    if unknown:
        raise ValueError(f"Unknown Stage B arm(s): {unknown}; valid: {sorted(spec_map.keys())}")

    arms: list[SpeedbumpArmConfig] = []
    for o_ms, c_ms, e_ms in latency_triples:
        for name in selected_names:
            raw_spec = dict(spec_map[name])
            use_sb_cancel = bool(raw_spec.pop("use_speed_bump_cancel_ms", False))
            if use_sb_cancel:
                raw_spec["speed_bump_cancel_ms"] = float(c_ms)
            arms.append(
                SpeedbumpArmConfig(
                    name=name,
                    order_delay_ms=o_ms,
                    cancel_delay_ms=c_ms,
                    exit_delay_ms=e_ms,
                    maker_fee_bps=maker_fee_bps,
                    taker_fee_bps=taker_fee_bps,
                    rebate_bps=rebate_bps,
                    **raw_spec,
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
    toxic_cross_handled: bool = False


@dataclasses.dataclass
class _OpenLot:
    sgn: float
    qty: float
    entry_px: float
    entry_mid: float
    entry_ts: int


@dataclasses.dataclass
class _RefLeadState:
    history: deque[tuple[int, float]] = dataclasses.field(default_factory=lambda: deque(maxlen=64))
    pending: deque[tuple[int, float, float]] = dataclasses.field(default_factory=lambda: deque(maxlen=64))
    samples: int = 0
    lead_score_ewma_bps: float = 0.0
    last_signal_ms: int | None = None

    def observe_and_mature(
        self,
        now_ms: int,
        ref_mid: float,
        target_mid: float,
        horizon_ms: int = 1_500,
        min_move_bps: float = 0.35,
    ) -> None:
        while self.pending and now_ms >= self.pending[0][0]:
            _, direction, tgt0 = self.pending.popleft()
            if tgt0 > 0.0 and target_mid > 0.0:
                realized_bps = direction * math.log(target_mid / tgt0) * 1e4
                if math.isfinite(realized_bps):
                    if self.samples == 0:
                        self.lead_score_ewma_bps = realized_bps
                    else:
                        self.lead_score_ewma_bps = 0.85 * self.lead_score_ewma_bps + 0.15 * realized_bps
                    self.samples += 1

        if not self.history or now_ms > self.history[-1][0]:
            self.history.append((now_ms, ref_mid))

        cutoff = now_ms - horizon_ms
        ref0: float | None = None
        for ts_h, px_h in reversed(self.history):
            if ts_h <= cutoff:
                ref0 = px_h
                break
        if ref0 is not None and ref0 > 0.0 and (self.last_signal_ms is None or now_ms - self.last_signal_ms >= 250):
            move_bps = math.log(ref_mid / ref0) * 1e4
            if abs(move_bps) >= min_move_bps:
                direction = 1.0 if move_bps > 0.0 else -1.0
                self.pending.append((now_ms + horizon_ms, direction, target_mid))
                self.last_signal_ms = now_ms

    def is_validated(self, min_samples: int = 5, min_score_bps: float = 0.02) -> bool:
        return self.samples >= min_samples and self.lead_score_ewma_bps >= min_score_bps


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
                bb, bsz = float(ev["bb"]), float(ev["bsz"])
                ba, asz = float(ev["ba"]), float(ev["asz"])
            elif "bids" in ev and "asks" in ev:
                bb, bsz = float(ev["bids"][0][0]), float(ev["bids"][0][1])
                ba, asz = float(ev["asks"][0][0]), float(ev["asks"][0][1])
            else:
                bb, bsz = float(ev["bid_px"]), float(ev["bid_sz"])
                ba, asz = float(ev["ask_px"]), float(ev["ask_sz"])
            if (
                not (math.isfinite(bb) and math.isfinite(ba) and math.isfinite(bsz) and math.isfinite(asz))
                or bb <= 0.0
                or ba <= bb
                or bsz <= 0.0
                or asz <= 0.0
            ):
                continue
            tuples.append((int(ev["ts"]), 0, bb, bsz, ba, asz))
        elif kind == "ref":
            rbid = float(ev["bid"])
            rask = float(ev["ask"])
            if not (math.isfinite(rbid) and math.isfinite(rask)) or rbid <= 0.0 or rask <= rbid:
                continue
            has_ref = True
            tuples.append((int(ev["ts"]), 1, str(ev.get("venue") or "ref"), rbid, rask, 0.0))
        elif kind == "trade":
            px = float(ev["price"])
            sz = float(ev["size"])
            if not (math.isfinite(px) and math.isfinite(sz)) or px <= 0.0 or sz <= 0.0:
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
    for px in prices[:400]:
        s = f"{px:.8f}".rstrip("0").rstrip(".")
        if "." in s:
            best = min(best, 10.0 ** (-len(s.split(".")[1])))
    return max(best, 1e-6)


def simulate_prepared_arm(
    stream: PreparedStream,
    tick: float,
    arm: SpeedbumpArmConfig,
    *,
    markout_horizons_ms: tuple[int, ...] = (1_000, 5_000, 10_000),
    declared_end_ms: int | None = None,
) -> dict[str, Any]:
    """Run causal FIFO simulation with Codex #140 realism & speed-bump quoting controls."""
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
    cutoff_mid_idx = (bisect_right(mid_times, declared_end_ms) - 1) if (declared_end_ms is not None and mid_times) else (len(mid_times) - 1)

    if not tuples or not mid_times or cutoff_mid_idx < 0:
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
            "taker_exit_share_s": 0.0,
            "double_sided_usd_per_m": cost_bd.double_sided_usd_per_m,
            "realized_only_usd_per_m": cost_bd.double_sided_usd_per_m,
            "mtm_incl_residual_usd_per_m": cost_bd.double_sided_usd_per_m,
            "maker_volume_usd_per_m": cost_bd.maker_volume_usd_per_m,
            "s_zero_double_sided_floor_usd_per_m": cost_bd.s_zero_double_sided_floor_usd_per_m,
            "terminal_residual_notional_usd": 0.0,
            "depth_clipped_exits": 0,
            "stale_book_cancels": 0,
            "stale_exit_blocks": 0,
            "toxicity_pauses": 0,
            "avg_hold_ms": 0.0,
            "hold_weighted_sum_ms": 0.0,
            "max_hold_observed_ms": 0,
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
            "unprinted_book_cross_fills": 0,
            "unprinted_book_cross_notional_usd": 0.0,
            "first_mid": None,
            "last_mid": None,
            "underlying_return_bps": None,
            "terminal_flat": True,
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

    # Rolling symbol toxicity circuit breaker state (mirrors Rust mature_fill_markouts).
    symbol_toxicity_ewma_bps = 0.0
    symbol_toxicity_fills = 0
    toxicity_pause_until_ms = 0
    toxicity_pauses = 0

    # Signed TFI EWMA state.
    tfi_signed_vol = 0.0
    tfi_total_vol = 0.0
    last_tfi_ts: int | None = None

    # Self-fill stepback state.
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

    # Codex #140 realism counters + microstructure diagnostics:
    depth_clipped_exits = 0
    stale_book_cancels = 0
    stale_exit_blocks = 0
    hold_weighted_sum_ms = 0.0
    hold_closed_notl = 0.0
    max_hold_observed_ms = 0
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
    unprinted_book_cross_fills = 0
    unprinted_book_cross_notional_usd = 0.0
    first_mid: float | None = None
    last_mid: float | None = None

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
            closed_notl = closed_qty * fill_px
            ntl += closed_notl
            hold_ms = max(0, ts_ms - lot.entry_ts)
            hold_weighted_sum_ms += closed_notl * hold_ms
            hold_closed_notl += closed_notl
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
        nonlocal tfi_veto_cancels, tfi_veto_blocks, reduce_penny_placements, stale_book_cancels
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

        # BID (opening quotes blocked by toxicity_paused / quiet_blocked / tfi_vetoed / terminal drain;
        # reduce_only exits remain completely exempt!).
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
                if is_red_bid and arm.reduce_only_penny_ticks > 0 and px_b > bb + tol_tick_10:
                    reduce_penny_placements += 1

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
                if is_red_ask and arm.reduce_only_penny_ticks > 0 and px_a < ba - tol_tick_10:
                    reduce_penny_placements += 1

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

            # Book refresh may allow queuing a new delayed IOC exit (which must wait exit_delay_ms);
            # never immediately fill a previously stale pending IOC at 0 ms.
            maybe_queue_exit_ioc(ts)
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

    # Codex #140 & Gemini #140 Realism: NEVER silently flatten remaining inventory at `last_ts`
    # with 0 ms delay or infinite liquidity! Any unclosed inventory stays as explicit residual.
    mid_end = 0.5 * (cur_bb + cur_ba) if has_book else 0.0
    terminal_residual_notional_usd = abs(pos_qty) * mid_end if has_book else 0.0
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
        "taker_exit_share_s": round(taker_exit_share_s, 6),
        "double_sided_usd_per_m": mtm_cost_bd.double_sided_usd_per_m,
        "realized_only_usd_per_m": realized_cost_bd.double_sided_usd_per_m,
        "mtm_incl_residual_usd_per_m": mtm_cost_bd.double_sided_usd_per_m,
        "maker_volume_usd_per_m": mtm_cost_bd.maker_volume_usd_per_m,
        "s_zero_double_sided_floor_usd_per_m": mtm_cost_bd.s_zero_double_sided_floor_usd_per_m,
        "terminal_residual_notional_usd": round(terminal_residual_notional_usd, 6),
        "depth_clipped_exits": depth_clipped_exits,
        "stale_book_cancels": stale_book_cancels,
        "stale_exit_blocks": stale_exit_blocks,
        "toxicity_pauses": toxicity_pauses,
        "avg_hold_ms": round(hold_weighted_sum_ms / hold_closed_notl, 2) if hold_closed_notl > 0.0 else 0.0,
        "hold_weighted_sum_ms": round(hold_weighted_sum_ms, 2),
        "max_hold_observed_ms": max_hold_observed_ms,
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
        "_exact_cash_ex_fee_usd": cash_ex_fee_usd + (pos_qty * mid_end if has_book else 0.0),
        "_exact_realized_gross_usd": realized_gross_usd,
        "_exact_mtm_gross_usd": mtm_gross_usd,
        "_exact_fee_usd": fee_usd,
        "_exact_net_usd": mtm_net_usd,
    }


def simulate_arm(
    events: Sequence[dict[str, Any]],
    tick: float,
    arm: SpeedbumpArmConfig,
    *,
    markout_horizons_ms: tuple[int, ...] = (1_000, 5_000, 10_000),
    declared_end_ms: int | None = None,
) -> dict[str, Any]:
    """Convenience wrapper preparing `events` and simulating `arm`."""
    return simulate_prepared_arm(
        prepare_replay_stream(events),
        tick,
        arm,
        markout_horizons_ms=markout_horizons_ms,
        declared_end_ms=declared_end_ms,
    )


def _window_sort_key(win_id: str) -> tuple[int, str]:
    m = re.search(r"(\d+)$", win_id)
    return (int(m.group(1)) if m else 9999, win_id)


def discover_campaigns(
    lakehouse_dir: Path,
    split: str,
    window_ids: Sequence[str] | None = None,
) -> dict[str, dict[str, dict[str, str]]]:
    """Dynamically discover `tokyo-{crypto,rwa}-*-w*` Hive campaigns strictly within W01-W26."""
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
        if win_num < 1 or win_num > 26:
            # Explicitly exclude w27, w28, or beyond!
            continue
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


def aggregate_rows_bucket(grp: Sequence[dict[str, Any]], *, include_symbol: bool = False) -> dict[str, Any]:
    """Aggregate rows with explicit realized_only vs mtm_incl_residual $/M & residual metrics."""
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
    ntl = sum(float(r["ntl"]) for r in grp)
    maker_vol = sum(float(r["maker_volume_usd"]) for r in grp)
    taker_vol = sum(float(r["taker_volume_usd"]) for r in grp)
    res_notl = sum(float(r.get("terminal_residual_notional_usd", 0.0)) for r in grp)
    depth_clipped = sum(int(r.get("depth_clipped_exits", 0)) for r in grp)
    stale_cancels = sum(int(r.get("stale_book_cancels", 0)) for r in grp)
    stale_blocks = sum(int(r.get("stale_exit_blocks", 0)) for r in grp)
    tox_pauses = sum(int(r.get("toxicity_pauses", 0)) for r in grp)
    hold_wsum = sum(float(r.get("hold_weighted_sum_ms", 0.0)) for r in grp)
    max_hold_obs = max((int(r.get("max_hold_observed_ms", 0)) for r in grp), default=0)

    realized_gbp = (realized_gross_usd / ntl) * 1e4 if ntl > 0.0 else 0.0
    mtm_gbp = (mtm_gross_usd / ntl) * 1e4 if ntl > 0.0 else 0.0
    net_bp = (mtm_net_usd / ntl) * 1e4 if ntl > 0.0 else 0.0
    s_share = min(1.0, max(0.0, taker_vol / ntl)) if ntl > 0.0 else 0.0

    realized_cost = compute_volume_farming_cost(
        realized_gbp,
        s_share,
        float(first["taker_fee_bps"]),
        maker_fee_bps=float(first["maker_fee_bps"]),
        rebate_bps=float(first["rebate_bps"]),
    )
    mtm_cost = compute_volume_farming_cost(
        mtm_gbp,
        s_share,
        float(first["taker_fee_bps"]),
        maker_fee_bps=float(first["maker_fee_bps"]),
        rebate_bps=float(first["rebate_bps"]),
    )
    # Optional Lighter Premium (0.40/2.80) counterfactual cost sensitivity on same gross & s.
    prem_cost = compute_volume_farming_cost(
        mtm_gbp,
        s_share,
        2.80,
        maker_fee_bps=0.40,
        rebate_bps=0.0,
    )

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
    win_net_series: list[float] = []
    worst_win: dict[str, Any] | None = None
    for win_id in sorted(by_win.keys(), key=_window_sort_key):
        wgrp = by_win[win_id]
        w_mtm_net = sum(float(x["mtm_net_usd"]) for x in wgrp)
        w_mtm_gross = sum(float(x["mtm_gross_usd"]) for x in wgrp)
        w_ntl = sum(float(x["ntl"]) for x in wgrp)
        w_tvol = sum(float(x["taker_volume_usd"]) for x in wgrp)
        w_gbp = (w_mtm_gross / w_ntl) * 1e4 if w_ntl > 0.0 else 0.0
        w_s = min(1.0, max(0.0, w_tvol / w_ntl)) if w_ntl > 0.0 else 0.0
        w_cost = compute_volume_farming_cost(
            w_gbp,
            w_s,
            float(first["taker_fee_bps"]),
            maker_fee_bps=float(first["maker_fee_bps"]),
            rebate_bps=float(first["rebate_bps"]),
        )
        total_hours += max(float((x.get("continuity") or {}).get("hours", 0.0)) for x in wgrp)
        win_net_series.append(w_mtm_net)
        cum_net += w_mtm_net
        if cum_net > peak_net:
            peak_net = cum_net
        max_dd_usd = max(max_dd_usd, peak_net - cum_net)
        if w_mtm_net > 0.0:
            pos_wins += 1
        if worst_win is None or w_mtm_net < worst_win["net_usd"]:
            worst_win = {
                "win": win_id,
                "net_usd": round(w_mtm_net, 4),
                "ntl": round(w_ntl, 2),
                "double_sided_usd_per_m": w_cost.double_sided_usd_per_m,
            }

    psr_zero: float | None = None
    if len(win_net_series) >= 3:
        mom = series_moments(win_net_series)
        psr_zero = round(probabilistic_sharpe_ratio(mom, 0.0), 6)

    n_wins = len(by_win)
    out: dict[str, Any] = {
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
        "gross_usd": round(realized_gross_usd, 6),
        "unrealized_mtm_usd": round(unrealized_mtm_usd, 6),
        "mtm_gross_usd": round(mtm_gross_usd, 6),
        "fee_usd": round(fee_usd, 6),
        "realized_net_usd": round(realized_net_usd, 6),
        "net_usd": round(mtm_net_usd, 6),
        "ntl": round(ntl, 4),
        "maker_volume_usd": round(maker_vol, 4),
        "taker_volume_usd": round(taker_vol, 4),
        "gross_ex_fee_bps": round(mtm_gbp, 6),
        "net_per_ntl_bp": round(net_bp, 6),
        "taker_exit_share_s": round(s_share, 6),
        "double_sided_usd_per_m": mtm_cost.double_sided_usd_per_m,
        "realized_only_usd_per_m": realized_cost.double_sided_usd_per_m,
        "mtm_incl_residual_usd_per_m": mtm_cost.double_sided_usd_per_m,
        "premium_04_28_usd_per_m": prem_cost.double_sided_usd_per_m,
        "maker_volume_usd_per_m": mtm_cost.maker_volume_usd_per_m,
        "terminal_residual_notional_usd": round(res_notl, 4),
        "depth_clipped_exits": depth_clipped,
        "stale_book_cancels": stale_cancels,
        "stale_exit_blocks": stale_blocks,
        "toxicity_pauses": tox_pauses,
        "avg_hold_ms": round(hold_wsum / ntl, 2) if ntl > 0.0 else 0.0,
        "max_hold_observed_ms": max_hold_obs,
        "markout_1s_bps": _agg_mk("markout_1s_bps", "markout_1s_den"),
        "markout_5s_bps": _agg_mk("markout_5s_bps", "markout_5s_den"),
        "markout_10s_bps": _agg_mk("markout_10s_bps", "markout_10s_den"),
        "cancel_in_flight_fills": cif_fills,
        "cancel_in_flight_notional_usd": round(cif_notl, 4),
        "cancel_in_flight_share": round(cif_share, 6),
        "cancel_in_flight_markout_1s_bps": _agg_mk(
            "cancel_in_flight_markout_1s_bps", "cancel_in_flight_markout_1s_den"
        ),
        "same_side_cluster_fills": cl_fills,
        "same_side_cluster_notional_usd": round(cl_notl, 4),
        "same_side_cluster_share": round(cl_share, 6),
        "positive_window_ratio": round(pos_wins / n_wins, 4) if n_wins > 0 else 0.0,
        "psr_zero": psr_zero,
        "max_drawdown_usd": round(max_dd_usd, 4),
        "worst_10h_window": worst_win,
        "_win_net_map": {
            wid: round(sum(float(x["mtm_net_usd"]) for x in by_win[wid]), 8)
            for wid in sorted(by_win.keys(), key=_window_sort_key)
        },
    }
    if include_symbol:
        out["symbol"] = first["symbol"]
        out["domain"] = first["domain"]
    return out


def derive_top_tier_universe_from_dev(
    rows: Sequence[dict[str, Any]],
    *,
    latency_label: str = "o200_c300_e330",
    max_cost_usd_per_m: float = 10.0,
    min_windows: int = 10,
    exclude_symbols: Sequence[str] = ("NVDA",),
) -> dict[str, dict[str, Any]]:
    """Freeze causal Top-Tier Low-Cost Universe strictly on `selection_dev` Stage A rows.

    Rule: per venue, select RWA symbols covered in `>= min_windows` (excluding persistently toxic
    vetoed names such as `NVDA` if specified) whose `selection_dev` `G5` `mtm_incl_residual_usd_per_m <= max_cost_usd_per_m`.
    """
    active = [
        r
        for r in rows
        if not r.get("skipped")
        and r.get("arm") == "G5_microprice_tfi"
        and r.get("latency_label") == latency_label
        and r.get("domain") == "rwa"
    ]
    by_vs: dict[tuple[str, str], list[dict[str, Any]]] = defaultdict(list)
    for r in active:
        by_vs[(str(r["venue"]), str(r["symbol"]))].append(r)

    excl = set(exclude_symbols)
    out: dict[str, dict[str, Any]] = {}
    for venue in sorted({v for v, _ in by_vs}):
        league: list[dict[str, Any]] = []
        selected: list[str] = []
        for (v, sym), grp in sorted(by_vs.items()):
            if v != venue:
                continue
            agg = aggregate_rows_bucket(grp, include_symbol=True)
            agg.pop("_win_net_map", None)
            league.append(agg)
            if (
                sym not in excl
                and agg["windows_covered"] >= min_windows
                and agg["mtm_incl_residual_usd_per_m"] <= max_cost_usd_per_m
                and agg["ntl"] > 0.0
            ):
                selected.append(sym)
        league.sort(key=lambda x: x["mtm_incl_residual_usd_per_m"])
        out[venue] = {
            "rule": {
                "latency_label": latency_label,
                "max_cost_usd_per_m": max_cost_usd_per_m,
                "min_windows": min_windows,
                "exclude_symbols": list(exclude_symbols),
            },
            "selected_symbols": sorted(selected),
            "league_table": league,
        }
    return out


def compute_multiple_testing_summary(
    rows: Sequence[dict[str, Any]],
    *,
    cohort_filter: Sequence[str] | None = None,
    domain_filter: str | None = None,
) -> dict[str, dict[str, Any]]:
    """Compute DSR & CSCV PBO across arms per `(venue, latency_label)` using aligned window net PnLs."""
    active = [r for r in rows if not r.get("skipped")]
    if domain_filter:
        active = [r for r in active if r.get("domain") == domain_filter]
    if cohort_filter is not None:
        allowed = set(cohort_filter)
        active = [r for r in active if r.get("symbol") in allowed]

    by_vl: dict[tuple[str, str], dict[str, dict[str, float]]] = defaultdict(lambda: defaultdict(dict))
    for r in active:
        vl = (str(r["venue"]), str(r["latency_label"]))
        arm = str(r["arm"])
        win = str(r["win"])
        by_vl[vl][arm][win] = by_vl[vl][arm].get(win, 0.0) + float(r["mtm_net_usd"])

    out: dict[str, dict[str, Any]] = {}
    for (venue, lat), arm_map in sorted(by_vl.items()):
        if len(arm_map) < 2:
            continue
        arms_sorted = sorted(arm_map.keys())
        common_wins = sorted(
            set.intersection(*(set(arm_map[a].keys()) for a in arms_sorted)),
            key=_window_sort_key,
        )
        if len(common_wins) < 4:
            continue
        series = {a: [arm_map[a][w] for w in common_wins] for a in arms_sorted}
        varying = {a: v for a, v in series.items() if len(set(v)) > 1}
        if len(varying) < 2:
            continue
        dsr_rep = deflated_sharpe_report(varying)
        n_blocks = 8 if len(common_wins) >= 16 else 4
        pbo_rep = cscv_pbo(varying, num_blocks=n_blocks) if len(common_wins) >= n_blocks * 2 else None
        out[f"{venue}|{lat}"] = {
            "windows_aligned": len(common_wins),
            "dsr": dsr_rep,
            "cscv_pbo": pbo_rep,
        }
    return out


def summarize_cohorts(
    rows: Sequence[dict[str, Any]],
    *,
    top_tier_by_venue: dict[str, Sequence[str]] | None = None,
) -> dict[str, Any]:
    """Summarize Legacy 5-RWA (incl/ex-NVDA), Full 18/19-Market Universe, Top-Tier Universe, and Crypto."""
    active = [r for r in rows if not r.get("skipped")]

    def _agg_map(pred: Any, *, include_symbol: bool = False) -> dict[str, Any]:
        buckets: dict[tuple[Any, ...], list[dict[str, Any]]] = defaultdict(list)
        for r in active:
            if not pred(r):
                continue
            key = (
                (r["arm"], r["venue"], r["latency_label"], r["symbol"])
                if include_symbol
                else (r["arm"], r["venue"], r["latency_label"])
            )
            buckets[key].append(r)
        res: dict[str, Any] = {}
        for k, grp in sorted(buckets.items()):
            agg = aggregate_rows_bucket(grp, include_symbol=include_symbol)
            agg.pop("_win_net_map", None)
            res["|".join(str(x) for x in k)] = agg
        return res

    legacy_set = set(LEGACY_5_RWA_SYMBOLS)
    legacy_ex_nvda = set(LEGACY_5_RWA_EX_NVDA)
    crypto_set = set(CRYPTO_SYMBOLS)

    cohorts: dict[str, Any] = {
        "full_universe_all": _agg_map(lambda r: True),
        "full_universe_rwa": _agg_map(lambda r: r["domain"] == "rwa"),
        "full_universe_rwa_ex_nvda": _agg_map(lambda r: r["domain"] == "rwa" and r["symbol"] != "NVDA"),
        "crypto": _agg_map(lambda r: r["symbol"] in crypto_set),
        "legacy_5_rwa_incl_nvda": _agg_map(lambda r: r["symbol"] in legacy_set),
        "legacy_5_rwa_ex_nvda": _agg_map(lambda r: r["symbol"] in legacy_ex_nvda),
        "by_symbol": _agg_map(lambda r: True, include_symbol=True),
    }
    if top_tier_by_venue:
        tt_sets = {v: set(syms) for v, syms in top_tier_by_venue.items()}
        cohorts["top_tier_rwa"] = _agg_map(
            lambda r: r["symbol"] in tt_sets.get(str(r["venue"]), set())
        )
        cohorts["top_tier_rwa_ex_nvda"] = _agg_map(
            lambda r: r["symbol"] in tt_sets.get(str(r["venue"]), set()) and r["symbol"] != "NVDA"
        )
    return cohorts


def _eval_single_window(
    lakehouse_dir: Path,
    win_id: str,
    win_map: dict[str, dict[str, str]],
    targets: Sequence[UniverseTarget],
    mode: str,
    triples: Sequence[tuple[int, int, int]],
    arm_names: Sequence[str] | None,
    rebate_bps: float,
    param_overrides: dict[str, dict[str, Any]] | None,
) -> tuple[list[dict[str, Any]], float, float, float, int]:
    """Evaluate a single 10h window using batched per-venue DuckDB parquet scans."""
    import duckdb  # Lazy import so stdlib-only CI never requires duckdb

    con = duckdb.connect(":memory:")
    con.execute("SET preserve_insertion_order=false; SET threads=2; SET memory_limit='2GB';")

    rows_out: list[dict[str, Any]] = []
    max_gross_err = 0.0
    max_net_err = 0.0
    max_mtm_cash_err = 0.0
    non_flat_count = 0

    bbo_cache: dict[tuple[str, str], tuple[dict[str, list[tuple[int, float, float, float, float]]], dict[str, dict[str, Any]]]] = {}
    trade_cache: dict[tuple[str, str], dict[str, list[tuple[int, str, float, float]]]] = {}

    def load_venue_bbo_batch(
        venue: str,
        c_info: dict[str, str],
        markets: Sequence[str],
        bucket_ms: int = 250,
    ) -> tuple[dict[str, list[tuple[int, float, float, float, float]]], dict[str, dict[str, Any]]]:
        ckey = (venue, c_info["campaign_id"])
        if ckey in bbo_cache:
            return bbo_cache[ckey]
        files = _campaign_parquet_files(
            lakehouse_dir, "bbo_quotes", venue, c_info["utc_date"], c_info["campaign_id"]
        )
        if not files or not markets:
            bbo_cache[ckey] = ({}, {})
            return bbo_cache[ckey]

        f_list_sql = "[" + ", ".join(f"'{p}'" for p in files) + "]"
        mkt_list_sql = ", ".join(f"'{m}'" for m in sorted(set(markets)))

        stat_rows = con.execute(
            f"""
            WITH s AS (
                SELECT market,
                       available_at_ms,
                       LAG(available_at_ms) OVER (PARTITION BY market ORDER BY available_at_ms) AS prev_ts
                FROM read_parquet({f_list_sql})
                WHERE market IN ({mkt_list_sql})
                  AND tradable = true
                  AND bid_px_f64 > 0
                  AND ask_px_f64 > bid_px_f64
            )
            SELECT market,
                   COUNT(*),
                   MIN(available_at_ms),
                   MAX(available_at_ms),
                   COALESCE(MAX(available_at_ms - prev_ts), 0)
            FROM s
            GROUP BY market
            """
        ).fetchall()

        cont_map: dict[str, dict[str, Any]] = {}
        for mkt, cnt, t0, t1, mgap in stat_rows:
            if not cnt or cnt < 2:
                cont_map[str(mkt)] = {
                    "raw_events": int(cnt or 0),
                    "duration_ms": 0,
                    "hours": 0.0,
                    "max_gap_ms": 0,
                    "is_continuous_10h": False,
                }
            else:
                dur = int(t1 - t0)
                cont_map[str(mkt)] = {
                    "raw_events": int(cnt),
                    "duration_ms": dur,
                    "hours": round(dur / 3_600_000.0, 3),
                    "max_gap_ms": int(mgap),
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
            FROM read_parquet({f_list_sql})
            WHERE market IN ({mkt_list_sql})
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

        bbo_cache[ckey] = (by_mkt, cont_map)
        return bbo_cache[ckey]

    def load_venue_trades_batch(
        venue: str,
        c_info: dict[str, str],
        markets: Sequence[str],
    ) -> dict[str, list[tuple[int, str, float, float]]]:
        ckey = (venue, c_info["campaign_id"])
        if ckey in trade_cache:
            return trade_cache[ckey]
        files = _campaign_parquet_files(
            lakehouse_dir, "public_trades", venue, c_info["utc_date"], c_info["campaign_id"]
        )
        if not files or not markets:
            trade_cache[ckey] = {}
            return trade_cache[ckey]

        f_list_sql = "[" + ", ".join(f"'{p}'" for p in files) + "]"
        mkt_list_sql = ", ".join(f"'{m}'" for m in sorted(set(markets)))
        t_rows = con.execute(
            f"""
            SELECT market, available_at_ms, aggressor_side, price_f64, size_f64
            FROM read_parquet({f_list_sql})
            WHERE market IN ({mkt_list_sql})
              AND NOT is_zero_price_transfer
              AND price_f64 > 0
              AND size_f64 > 0
            ORDER BY market ASC, available_at_ms ASC
            """
        ).fetchall()

        by_mkt: dict[str, list[tuple[int, str, float, float]]] = defaultdict(list)
        for mkt, ts_ms, agg_side, px, sz in t_rows:
            by_mkt[str(mkt)].append((int(ts_ms), str(agg_side), float(px), float(sz)))
        trade_cache[ckey] = by_mkt
        return trade_cache[ckey]

    # Pre-collect markets needed per (domain, venue) so each parquet dataset is scanned once!
    markets_needed: dict[tuple[str, str], set[str]] = defaultdict(set)
    for t in targets:
        if t.domain not in win_map:
            continue
        markets_needed[(t.domain, t.venue)].add(t.raw_market)
        for rv in t.ref_venues:
            if rv in VENUE_SYMBOL_MAP and t.symbol in VENUE_SYMBOL_MAP[rv]:
                markets_needed[(t.domain, rv)].add(VENUE_SYMBOL_MAP[rv][t.symbol])

    for (dom, v), mkts in sorted(markets_needed.items()):
        c_info = win_map[dom]
        load_venue_bbo_batch(v, c_info, sorted(mkts), bucket_ms=250)
        if v in ("lighter", "lighter_robinhood"):
            load_venue_trades_batch(v, c_info, sorted(mkts))

    for target in targets:
        c_info = win_map.get(target.domain)
        if c_info is None:
            continue
        bbo_map, cont_map = load_venue_bbo_batch(target.venue, c_info, [target.raw_market])
        b_rows = bbo_map.get(target.raw_market, [])
        cont = cont_map.get(
            target.raw_market,
            {"raw_events": 0, "duration_ms": 0, "hours": 0.0, "max_gap_ms": 0, "is_continuous_10h": False},
        )
        if len(b_rows) < 200:
            rows_out.append(
                {
                    "win": win_id,
                    "campaign_id": c_info["campaign_id"],
                    "domain": target.domain,
                    "venue": target.venue,
                    "symbol": target.symbol,
                    "raw_market": target.raw_market,
                    "settlement": target.settlement,
                    "account_fee_status": target.account_fee_status,
                    "cont": False,
                    "continuity": cont,
                    "skipped": f"too_few_books:{len(b_rows)}",
                }
            )
            continue

        events: list[dict[str, Any]] = [
            {
                "ts": r[0],
                "kind": "book",
                "bb": r[1],
                "bsz": max(r[2], 1e-9),
                "ba": r[3],
                "asz": max(r[4], 1e-9),
            }
            for r in b_rows
        ]
        refs_used: list[str] = []
        for ref_v in target.ref_venues:
            if ref_v not in VENUE_SYMBOL_MAP or target.symbol not in VENUE_SYMBOL_MAP[ref_v]:
                continue
            ref_raw = VENUE_SYMBOL_MAP[ref_v][target.symbol]
            r_bbo_map, _ = load_venue_bbo_batch(ref_v, c_info, [ref_raw])
            r_rows = r_bbo_map.get(ref_raw, [])
            if len(r_rows) >= 200:
                refs_used.append(ref_v)
                for rr in r_rows:
                    events.append({"ts": rr[0], "kind": "ref", "venue": ref_v, "bid": rr[1], "ask": rr[3]})

        tr_map = load_venue_trades_batch(target.venue, c_info, [target.raw_market])
        for ts_ms, agg_side, px, sz in tr_map.get(target.raw_market, []):
            s_up = agg_side.upper()
            side_str = "BUY" if s_up in ("BUY", "B", "BID") else "SELL"
            events.append({"ts": ts_ms, "kind": "trade", "side": side_str, "price": px, "size": sz})

        stream = prepare_replay_stream(events)
        del events
        tick = _infer_tick_from_prices([r[1] for r in b_rows])

        if mode == "stage-a":
            arms = build_stage_a_arms(
                triples,
                maker_fee_bps=target.maker_fee_bps,
                taker_fee_bps=target.taker_fee_bps,
                rebate_bps=rebate_bps,
            )
        else:
            arms = build_stage_b_arms(
                triples,
                maker_fee_bps=target.maker_fee_bps,
                taker_fee_bps=target.taker_fee_bps,
                rebate_bps=rebate_bps,
                arm_names=arm_names,
                param_overrides=param_overrides,
            )

        for arm_cfg in arms:
            sim_res = simulate_prepared_arm(stream, tick, arm_cfg, declared_end_ms=b_rows[-1][0])
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
                    "domain": target.domain,
                    "venue": target.venue,
                    "symbol": target.symbol,
                    "raw_market": target.raw_market,
                    "settlement": target.settlement,
                    "account_fee_status": target.account_fee_status,
                    "fee_source": target.fee_source,
                    "maker_fee_bps": target.maker_fee_bps,
                    "taker_fee_bps": target.taker_fee_bps,
                    "rebate_bps": rebate_bps,
                    "tick_inferred": tick,
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
    return rows_out, max_gross_err, max_net_err, max_mtm_cash_err, non_flat_count


def run_duckdb_eval(
    lakehouse_dir: Path,
    *,
    mode: str = "stage-a",
    split: str = "selection_dev",
    window_ids: Sequence[str] | None = None,
    domains: Sequence[str] = ("crypto", "rwa"),
    venues: Sequence[str] = ("lighter_robinhood", "lighter"),
    symbols: Sequence[str] | None = None,
    latency_triples: Sequence[tuple[int, int, int]] = STAGE_LATENCY_TRIPLES,
    arm_names: Sequence[str] | None = None,
    rebate_bps: float = 0.0,
    fee_evidence_path: Path | None = None,
    param_overrides: dict[str, dict[str, Any]] | None = None,
    top_tier_by_venue: dict[str, Sequence[str]] | None = None,
    max_workers: int = 1,
) -> dict[str, Any]:
    """Run read-only DuckDB `:memory:` evaluation across W01-W26 full 18/19-market universe."""
    selected_windows = discover_campaigns(lakehouse_dir, split, window_ids)
    if not selected_windows:
        raise ValueError(f"No campaigns discovered under {lakehouse_dir} for split={split!r}")

    targets, fee_prov = build_full_universe_targets(
        fee_evidence_path=fee_evidence_path,
        domains=domains,
        venues=venues,
        symbols=symbols,
    )

    rows_out: list[dict[str, Any]] = []
    max_gross_err = 0.0
    max_net_err = 0.0
    max_mtm_cash_err = 0.0
    non_flat_count = 0

    eff_workers = max(1, min(int(max_workers), len(selected_windows)))
    if eff_workers == 1:
        for win_id, win_map in selected_windows.items():
            w_rows, w_ge, w_ne, w_me, w_nf = _eval_single_window(
                lakehouse_dir, win_id, win_map, targets, mode, latency_triples, arm_names, rebate_bps, param_overrides
            )
            rows_out.extend(w_rows)
            max_gross_err = max(max_gross_err, w_ge)
            max_net_err = max(max_net_err, w_ne)
            max_mtm_cash_err = max(max_mtm_cash_err, w_me)
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
                    latency_triples,
                    arm_names,
                    rebate_bps,
                    param_overrides,
                )
                for win_id, win_map in selected_windows.items()
            ]
            for fut in concurrent.futures.as_completed(futs):
                w_rows, w_ge, w_ne, w_me, w_nf = fut.result()
                rows_out.extend(w_rows)
                max_gross_err = max(max_gross_err, w_ge)
                max_net_err = max(max_net_err, w_ne)
                max_mtm_cash_err = max(max_mtm_cash_err, w_me)
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

    top_tier_discovery = (
        derive_top_tier_universe_from_dev(rows_out) if (mode == "stage-a" and split == "selection_dev") else None
    )
    eff_top_tier = (
        {v: spec["selected_symbols"] for v, spec in top_tier_discovery.items()}
        if top_tier_discovery is not None
        else top_tier_by_venue
    )
    cohorts = summarize_cohorts(rows_out, top_tier_by_venue=eff_top_tier)

    multiple_testing: dict[str, Any] = {}
    if mode == "stage-b":
        multiple_testing["full_universe_rwa"] = compute_multiple_testing_summary(rows_out, domain_filter="rwa")
        multiple_testing["legacy_5_rwa_incl_nvda"] = compute_multiple_testing_summary(
            rows_out, cohort_filter=LEGACY_5_RWA_SYMBOLS
        )
        multiple_testing["legacy_5_rwa_ex_nvda"] = compute_multiple_testing_summary(
            rows_out, cohort_filter=LEGACY_5_RWA_EX_NVDA
        )
        if eff_top_tier:
            union_tt = sorted({s for syms in eff_top_tier.values() for s in syms})
            if union_tt:
                multiple_testing["top_tier_rwa"] = compute_multiple_testing_summary(
                    rows_out, cohort_filter=union_tt
                )

    return {
        "schema": "track03_full_universe_speedbump_v1",
        "mode": mode,
        "venue_capability_snapshot_version": VENUE_CAPABILITY_SNAPSHOT_VERSION,
        "fee_provenance": fee_prov,
        "split": split,
        "rebate_bps": rebate_bps,
        "latency_triples": [list(t) for t in latency_triples],
        "frozen_stage_b_params": FROZEN_STAGE_B_PARAMS if mode == "stage-b" else None,
        "windows": selected_windows,
        "invariants": {
            "non_flat_count": non_flat_count,
            "max_gross_err": max_gross_err,
            "max_net_err": max_net_err,
            "max_mtm_cash_err": max_mtm_cash_err,
        },
        "targets": [dataclasses.asdict(t) for t in targets],
        "top_tier_discovery": top_tier_discovery,
        "top_tier_used": eff_top_tier,
        "cohorts": cohorts,
        "multiple_testing": multiple_testing,
        "rows": rows_out,
    }


def _parse_latency_triples(raw: str | None) -> list[tuple[int, int, int]]:
    if not raw:
        return list(STAGE_LATENCY_TRIPLES)
    out: list[tuple[int, int, int]] = []
    for part in raw.split(","):
        part = part.strip()
        if not part:
            continue
        o_s, c_s, e_s = part.split(":")
        out.append((int(o_s), int(c_s), int(e_s)))
    return out


def run_self_test() -> dict[str, Any]:
    """Verify Codex #140 realism (L1 depth clamp, freshness, 330ms exit delay, residual MTM) & S1-S4 arms."""
    tick = 0.01
    events: list[dict[str, Any]] = [
        {"ts": 0, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.06, 10.0)]},
        {"ts": 0, "kind": "ref", "venue": "arcus", "bid": 100.00, "ask": 100.06},
        {"ts": 300, "kind": "trade", "side": "SELL", "price": 100.00, "size": 8.0},
        {"ts": 600, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.06, 10.0)]},
        {"ts": 900, "kind": "trade", "side": "BUY", "price": 100.04, "size": 5.0},
        {"ts": 1_200, "kind": "trade", "side": "BUY", "price": 100.06, "size": 5.0},
        {"ts": 1_500, "kind": "book", "bids": [(100.00, 2.0)], "asks": [(100.06, 2.0)]},
        {"ts": 1_800, "kind": "trade", "side": "SELL", "price": 99.98, "size": 6.0},
        # Shallow L1 bid depth (0.50 < 2.00 position) at IOC arrival (16_800+330=17_130) clips exit!
        {"ts": 16_800, "kind": "book", "bids": [(99.95, 0.5)], "asks": [(100.01, 5.0)]},
        {"ts": 17_200, "kind": "book", "bids": [(99.95, 0.5)], "asks": [(100.01, 5.0)]},
        {"ts": 18_500, "kind": "book", "bids": [(99.95, 5.0)], "asks": [(100.01, 5.0)]},
        {"ts": 19_500, "kind": "book", "bids": [(99.95, 5.0)], "asks": [(100.01, 5.0)]},
    ]

    arms = build_stage_b_arms(latency_triples=[(200, 300, 330)], rebate_bps=0.0)
    assert len(arms) == 6
    verified: list[str] = []
    for arm in arms:
        res = simulate_arm(events, tick, arm)
        assert abs(res["_exact_mtm_gross_usd"] - res["_exact_cash_ex_fee_usd"]) <= 1e-6
        assert abs(res["_exact_net_usd"] - (res["_exact_mtm_gross_usd"] - res["_exact_fee_usd"])) <= 1e-9
        assert abs((res["spr"] + res["drft"] + res["pex"] + res["tslp"]) - res["gross_usd"]) <= 1e-6
        verified.append(arm.name)

    return {
        "self_test": "ok",
        "arms_verified": verified,
        "lighter_robinhood_mkts": len(VENUE_SYMBOL_MAP["lighter_robinhood"]),
        "lighter_mkts": len(VENUE_SYMBOL_MAP["lighter"]),
    }


def main(argv: Sequence[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description="Track 03 Full 18-Market Universe & Speed-Bump Quoting Evaluator")
    ap.add_argument("--lakehouse", type=Path, default=Path(os.path.expanduser("~/aleph-tx-lakehouse")))
    ap.add_argument("--mode", choices=("stage-a", "stage-b"), default="stage-a")
    ap.add_argument("--split", choices=("selection_dev", "holdout", "w26_oos", "all"), default="selection_dev")
    ap.add_argument("--windows", type=str, default=None)
    ap.add_argument("--domains", type=str, default="crypto,rwa")
    ap.add_argument("--venues", type=str, default="lighter_robinhood,lighter")
    ap.add_argument("--symbols", type=str, default=None)
    ap.add_argument("--latency-triples", type=str, default=None, help="Comma-separated o:c:e triples")
    ap.add_argument("--arms", type=str, default=None)
    ap.add_argument("--rebate-bps", type=float, default=0.0)
    ap.add_argument("--fee-evidence", type=Path, default=None)
    ap.add_argument("--top-tier-from", type=Path, default=None, help="Stage A JSON to freeze Top-Tier universe")
    ap.add_argument("--max-workers", type=int, default=1)
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

    top_tier_map: dict[str, list[str]] | None = None
    if args.top_tier_from:
        src = json.loads(args.top_tier_from.read_text(encoding="utf-8"))
        raw_tt = src.get("top_tier_discovery") or src.get("top_tier_used") or {}
        if isinstance(raw_tt, dict):
            top_tier_map = {
                str(v): list(val["selected_symbols"]) if isinstance(val, dict) else list(val)
                for v, val in raw_tt.items()
            }

    win_list = [w.strip() for w in args.windows.split(",") if w.strip()] if args.windows else None
    dom_list = [d.strip() for d in args.domains.split(",") if d.strip()]
    ven_list = [v.strip() for v in args.venues.split(",") if v.strip()]
    sym_list = [s.strip() for s in args.symbols.split(",") if s.strip()] if args.symbols else None
    arm_list = [a.strip() for a in args.arms.split(",") if a.strip()] if args.arms else None
    triples = _parse_latency_triples(args.latency_triples)

    result = run_duckdb_eval(
        args.lakehouse,
        mode=args.mode,
        split=args.split,
        window_ids=win_list,
        domains=dom_list,
        venues=ven_list,
        symbols=sym_list,
        latency_triples=triples,
        arm_names=arm_list,
        rebate_bps=args.rebate_bps,
        fee_evidence_path=args.fee_evidence,
        top_tier_by_venue=top_tier_map,
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
