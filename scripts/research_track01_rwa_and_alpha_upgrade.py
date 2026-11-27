#!/usr/bin/env python3
"""Track 01 (`01_quant_profit`) Deep Microstructure & RWA Alpha Upgrade Evaluator.

100% pure-stdlib at module top level (`duckdb` is imported lazily inside lakehouse functions).

Implements:
  - Stage A: Root-Cause Diagnosis on `selection_dev (w01-w20)` for Crypto (`lighter` 0/0 Standard
    & `lighter_robinhood` 0/0 Standard) and first-ever RWA Cross-Venue Lead-Lag Discovery
    (`NVDA, AAPL, SPY, QQQ, TSLA, AMZN, MU` on `lighter` 0/0 & `lighter_robinhood` 0/0 led by
    `arcus`, plus cross-domain `lighter <-> lighter_robinhood`), attributing PnL by entry
    `theta` bucket, reference OBI alignment, target spread width, 1s/3s/10s MFE/MAE trajectory,
    leader-reversal snapbacks, and latency decay across `taker_delay_ms in {230, 300, 400, 500}`.
  - Stage B: Exact Python mirror of upgraded Rust `LeadLagTaker` (`src/strategy/lead_lag_taker.rs`):
      * `A_baseline`: fixed `theta_bps`
      * `B_vol_adaptive`: causal EWMA 1s log-return threshold `max(theta, mult * sigma_1s)`
      * `M1_microprice_obi`: L1 microprice fair value + reference OBI directional confirmation
      * `M2_consensus_leader`: 3-leader stack median (`consensus_ref_mid` over `binance+coinbase+extended`
        for Crypto; `arcus+cross-venue` / multi-leader where applicable)
      * `M3_ref_reversal_spread_gate`: target spread-cost gate + leader-reversal early stop
      * `M4_combined`: combined microstructure upgrade (`M1 + M2 + M3` / best selection-dev combo)
    Evaluates Round-2 ablation strictly on `selection_dev (w01-w20)`, freezes parameters, and
    validates ONCE on untouched `holdout (w21-w25)` and `w26_oos` across `{230, 300, 400, 500} ms`
    for BOTH Crypto and RWA (`0.5*L1`-capped `$500` / `$1000`, quote + strict `public_trades` PV).
"""

from __future__ import annotations

import argparse
import bisect
import concurrent.futures
import dataclasses
import gc
import glob
import json
import math
import os
import re
import statistics
import sys
from collections import defaultdict
from datetime import date, timedelta
from pathlib import Path
from typing import Any, Sequence

TEN_HOURS_MS = 36_000_000
MAX_CONTINUOUS_GAP_MS = 60_000
STEP_MS = 250
DEFAULT_WARMUP_MS = 15 * 60_000
DEFAULT_MAX_AGE_MS = 5_000
DEFAULT_MAX_DEV_BPS = 60.0
DEFAULT_MAX_GAP_MS = 30_000
DEFAULT_VOL_EWMA_TAU_MS = 15_000
DEFAULT_PRINT_WINDOW_MS = 1_000
DEFAULT_L1_DEPTH_CAP_FRAC = 0.5
CLIPS_USD: tuple[float, ...] = (500.0, 1_000.0)
LATENCY_GRID_MS: tuple[int, ...] = (230, 300, 400, 500)

CRYPTO_SYMBOLS: tuple[str, ...] = ("BTC", "ETH", "SOL")
RWA_SYMBOLS: tuple[str, ...] = ("NVDA", "AAPL", "SPY", "QQQ", "TSLA", "AMZN", "MU")

VENUE_SYMBOL_MAP: dict[str, dict[str, str]] = {
    "binance": {"BTC": "BTCUSDT", "ETH": "ETHUSDT", "SOL": "SOLUSDT"},
    "coinbase": {"BTC": "BTC-USD", "ETH": "ETH-USD", "SOL": "SOL-USD"},
    "extended": {"BTC": "BTC-USD", "ETH": "ETH-USD", "SOL": "SOL-USD"},
    "hyperliquid": {"BTC": "BTC", "ETH": "ETH", "SOL": "SOL"},
    "arcus": {
        "BTC": "BTC-USD",
        "ETH": "ETH-USD",
        "SOL": "SOL-USD",
        "NVDA": "NVDA-USD",
        "AAPL": "AAPL-USD",
        "SPY": "SPY-USD",
        "QQQ": "QQQ-USD",
        "TSLA": "TSLA-USD",
        "AMZN": "AMZN-USD",
        "MU": "MU-USD",
    },
    "lighter": {
        "ETH": "0",
        "BTC": "1",
        "SOL": "2",
        "NVDA": "110",
        "TSLA": "112",
        "AAPL": "113",
        "AMZN": "114",
        "SPY": "128",
        "QQQ": "129",
        "MU": "164",
    },
    "lighter_robinhood": {
        "ETH": "0",
        "BTC": "1",
        "SOL": "3",
        "AAPL": "10",
        "AMZN": "11",
        "NVDA": "15",
        "TSLA": "16",
        "QQQ": "25",
        "SPY": "26",
        "MU": "31",
    },
}

VENUE_SETTLEMENT: dict[str, str] = {
    "lighter": "USDC",
    "lighter_robinhood": "USDG",
    "arcus": "USDC",
    "extended": "USDC",
    "binance": "USDT",
    "coinbase": "USD",
    "hyperliquid": "USDC",
}

# Explicit Standard 0/0 bps target modeling per Track 01 research brief.
# Note: `lighter` live account is audited Premium 0.4/2.8 bps (OPT-01); this study explicitly
# models Lighter Standard 0/0 bps sensitivity alongside verified Lighter Robinhood USDG 0/0 bps.
STANDARD_ZERO_FEE_PROFILES: dict[str, dict[str, Any]] = {
    "lighter": {
        "settlement": "USDC",
        "maker_fee_bps": 0.0,
        "taker_fee_bps": 0.0,
        "fee_model": "lighter_standard_zero_scenario_0_0",
    },
    "lighter_robinhood": {
        "settlement": "USDG",
        "maker_fee_bps": 0.0,
        "taker_fee_bps": 0.0,
        "fee_model": "verified_standard_zero_0_0",
    },
}


def compute_l1_microprice_and_obi(
    bid_px: float,
    bid_sz: float,
    ask_px: float,
    ask_sz: float,
) -> tuple[float, float, float, float]:
    """Return `(mid, microprice, obi, spread_bps)` for a valid L1 quote."""
    mid = 0.5 * (bid_px + ask_px)
    spread_bps = ((ask_px - bid_px) / mid) * 1e4 if mid > 0.0 else 0.0
    if (
        math.isfinite(bid_sz)
        and math.isfinite(ask_sz)
        and bid_sz > 0.0
        and ask_sz > 0.0
    ):
        denom = bid_sz + ask_sz
        micro = (bid_px * ask_sz + ask_px * bid_sz) / denom
        obi = (bid_sz - ask_sz) / denom
        return mid, micro, obi, spread_bps
    if math.isfinite(bid_sz) and math.isfinite(ask_sz) and (bid_sz + ask_sz) > 0.0:
        obi = (max(0.0, bid_sz) - max(0.0, ask_sz)) / (max(0.0, bid_sz) + max(0.0, ask_sz))
        return mid, mid, obi, spread_bps
    return mid, mid, 0.0, spread_bps


def consensus_ref_mid(
    now_ms: int,
    max_ref_age_ms: int,
    leaders: Sequence[tuple[float, int] | None],
) -> tuple[float, int] | None:
    """Pure-stdlib mirror of Rust `consensus_ref_mid` over up to 3 fresh leaders.

    Filters invalid/non-finite/non-positive prices, future timestamps (`recv_ms > now_ms`),
    and stale feeds (`now_ms - recv_ms > max_ref_age_ms`).
    Returns median for 3 valid leaders, mean for 2, single price for 1, or None for 0.
    """
    vals: list[float] = []
    newest_ts = 0
    for item in leaders[:3]:
        if item is None:
            continue
        px, ts = float(item[0]), int(item[1])
        if not (math.isfinite(px) and px > 0.0):
            continue
        if ts > now_ms or (now_ms - ts) > max_ref_age_ms:
            continue
        vals.append(px)
        if ts > newest_ts:
            newest_ts = ts
    n = len(vals)
    if n == 0:
        return None
    if n == 1:
        return (vals[0], newest_ts)
    if n == 2:
        return (0.5 * (vals[0] + vals[1]), newest_ts)
    vals.sort()
    return (vals[1], newest_ts)


@dataclasses.dataclass(frozen=True)
class LeadLagPair:
    domain: str  # "crypto" or "rwa"
    tgt_venue: str
    symbol: str
    ref_venue: str
    confirm_venue: str | None = None
    tertiary_venue: str | None = None

    @property
    def pair_id(self) -> str:
        return f"{self.domain}:{self.tgt_venue}_{self.symbol}_from_{self.ref_venue}"


def build_crypto_pairs() -> list[LeadLagPair]:
    pairs: list[LeadLagPair] = []
    for tgt in ("lighter", "lighter_robinhood"):
        for sym in CRYPTO_SYMBOLS:
            pairs.append(
                LeadLagPair(
                    domain="crypto",
                    tgt_venue=tgt,
                    symbol=sym,
                    ref_venue="binance",
                    confirm_venue="coinbase",
                    tertiary_venue="extended",
                )
            )
            pairs.append(
                LeadLagPair(
                    domain="crypto",
                    tgt_venue=tgt,
                    symbol=sym,
                    ref_venue="coinbase",
                    confirm_venue="binance",
                    tertiary_venue="extended",
                )
            )
    return pairs


def build_rwa_pairs() -> list[LeadLagPair]:
    pairs: list[LeadLagPair] = []
    for sym in RWA_SYMBOLS:
        # Primary discovery: fast tight-spread arcus leading speed-bumped lighter / lighter_robinhood
        pairs.append(
            LeadLagPair(
                domain="rwa",
                tgt_venue="lighter",
                symbol=sym,
                ref_venue="arcus",
                confirm_venue="lighter_robinhood",
            )
        )
        pairs.append(
            LeadLagPair(
                domain="rwa",
                tgt_venue="lighter_robinhood",
                symbol=sym,
                ref_venue="arcus",
                confirm_venue="lighter",
            )
        )
        # Cross-domain USDC <-> USDG lead-lag
        pairs.append(
            LeadLagPair(
                domain="rwa",
                tgt_venue="lighter",
                symbol=sym,
                ref_venue="lighter_robinhood",
                confirm_venue="arcus",
            )
        )
        pairs.append(
            LeadLagPair(
                domain="rwa",
                tgt_venue="lighter_robinhood",
                symbol=sym,
                ref_venue="lighter",
                confirm_venue="arcus",
            )
        )
    return pairs


@dataclasses.dataclass(frozen=True)
class LeadLagUpgradeConfig:
    theta_bps: float = 6.0
    tau_ms: int = 120_000
    hold_ms: int = 10_000
    taker_delay_ms: int = 230
    max_ref_age_ms: int = DEFAULT_MAX_AGE_MS
    max_tgt_age_ms: int = DEFAULT_MAX_AGE_MS
    max_dev_bps: float = DEFAULT_MAX_DEV_BPS
    warmup_ms: int = DEFAULT_WARMUP_MS
    max_gap_ms: int = DEFAULT_MAX_GAP_MS
    vol_theta_mult: float = 0.0
    vol_ewma_tau_ms: int = DEFAULT_VOL_EWMA_TAU_MS
    require_dual_leader_confirm: bool = False
    exit_on_catchup_frac: float | None = None
    # Stage B microstructure upgrades (default-off)
    use_microprice_fair: bool = False
    ref_obi_confirm_min: float = 0.0
    max_entry_tgt_spread_bps: float | None = None
    min_edge_over_spread_bps: float | None = None
    ref_reversal_exit_bps: float | None = None
    use_consensus_leader: bool = False
    # Execution / replay controls
    taker_fee_bps: float = 0.0
    l1_depth_cap_frac: float = DEFAULT_L1_DEPTH_CAP_FRAC
    step_ms: int = STEP_MS
    print_window_ms: int = DEFAULT_PRINT_WINDOW_MS


@dataclasses.dataclass
class _OpenPos:
    side: int
    entry_filled_est_ms: int
    exit_due_ms: int
    entry_tgt_mid: float
    entry_ref_px: float
    entry_gap_bps: float
    exit_triggered: bool = False


class LeadLagTakerUpgradePy:
    """Pure-stdlib exact mirror of upgraded Rust `LeadLagTaker`."""

    def __init__(self, cfg: LeadLagUpgradeConfig) -> None:
        if not (math.isfinite(cfg.theta_bps) and cfg.theta_bps > 0.0):
            raise ValueError("theta_bps must be finite and > 0")
        if not (math.isfinite(cfg.max_dev_bps) and cfg.max_dev_bps > cfg.theta_bps):
            raise ValueError("max_dev_bps must be finite and > theta_bps")
        if cfg.tau_ms <= 0 or cfg.hold_ms <= 0 or cfg.max_gap_ms <= 0 or cfg.vol_ewma_tau_ms <= 0:
            raise ValueError("tau_ms, hold_ms, max_gap_ms, vol_ewma_tau_ms must be > 0")
        if cfg.max_ref_age_ms <= 0 or cfg.max_tgt_age_ms <= 0:
            raise ValueError("book age limits must be > 0")
        if not (math.isfinite(cfg.vol_theta_mult) and cfg.vol_theta_mult >= 0.0):
            raise ValueError("vol_theta_mult must be finite and >= 0")
        if cfg.exit_on_catchup_frac is not None:
            f = cfg.exit_on_catchup_frac
            if not (math.isfinite(f) and 0.0 < f <= 1.0):
                raise ValueError("exit_on_catchup_frac must be in (0.0, 1.0]")
        if not (math.isfinite(cfg.ref_obi_confirm_min) and 0.0 <= cfg.ref_obi_confirm_min <= 1.0):
            raise ValueError("ref_obi_confirm_min must be in [0.0, 1.0]")
        if cfg.max_entry_tgt_spread_bps is not None:
            if not (math.isfinite(cfg.max_entry_tgt_spread_bps) and cfg.max_entry_tgt_spread_bps > 0.0):
                raise ValueError("max_entry_tgt_spread_bps must be > 0")
        if cfg.min_edge_over_spread_bps is not None:
            if not (math.isfinite(cfg.min_edge_over_spread_bps) and cfg.min_edge_over_spread_bps >= 0.0):
                raise ValueError("min_edge_over_spread_bps must be >= 0")
        if cfg.ref_reversal_exit_bps is not None:
            if not (math.isfinite(cfg.ref_reversal_exit_bps) and cfg.ref_reversal_exit_bps > 0.0):
                raise ValueError("ref_reversal_exit_bps must be > 0")

        self.cfg = cfg
        self.ema: float | None = None
        self.first_obs_ms: int | None = None
        self.last_obs_ms: int | None = None
        self.confirm_ema: float | None = None
        self.confirm_last_obs_ms: int | None = None
        self.tgt_vol_last: tuple[int, float] | None = None
        self.tgt_var_bps2_per_s: float = 0.0
        self.pos: _OpenPos | None = None
        self.last_dev_bps: float | None = None

    def sigma_1s_bps(self) -> float:
        return math.sqrt(max(0.0, self.tgt_var_bps2_per_s))

    def effective_theta_bps(self) -> float:
        if self.cfg.vol_theta_mult > 0.0:
            return max(self.cfg.theta_bps, self.cfg.vol_theta_mult * self.sigma_1s_bps())
        return self.cfg.theta_bps

    def clear_position(self) -> None:
        self.pos = None

    def notify_exit_filled(self) -> None:
        self.pos = None

    def observe_l1_books(
        self,
        now_ms: int,
        ref_book: tuple[int, float, float, float, float],
        confirm_book: tuple[int, float, float, float, float] | None,
        tgt_book: tuple[int, float, float, float, float],
    ) -> tuple[str, int | None, float | None, str | None]:
        """Process L1 `(ts_ms, bid_px, bid_sz, ask_px, ask_sz)` observations.

        Returns `(kind, side_sgn, dev_bps, exit_reason)`.
        """
        r_ts, r_bid, r_bsz, r_ask, r_asz = int(ref_book[0]), float(ref_book[1]), float(ref_book[2]), float(ref_book[3]), float(ref_book[4])
        t_ts, t_bid, t_bsz, t_ask, t_asz = int(tgt_book[0]), float(tgt_book[1]), float(tgt_book[2]), float(tgt_book[3]), float(tgt_book[4])

        if (
            not (math.isfinite(r_bid) and math.isfinite(r_ask) and r_bid > 0.0 and r_ask > r_bid)
            or not (math.isfinite(t_bid) and math.isfinite(t_ask) and t_bid > 0.0 and t_ask > t_bid)
            or r_ts > now_ms
            or t_ts > now_ms
        ):
            return ("Blocked:InvalidInput", None, None, None)

        c_mid = 0.0
        c_ts = 0
        if confirm_book is not None:
            c_ts, c_bid, c_bsz, c_ask, c_asz = int(confirm_book[0]), float(confirm_book[1]), float(confirm_book[2]), float(confirm_book[3]), float(confirm_book[4])
            if not (math.isfinite(c_bid) and math.isfinite(c_ask) and c_bid > 0.0 and c_ask > c_bid) or c_ts > now_ms:
                return ("Blocked:InvalidInput", None, None, None)
            c_mid_raw, c_micro, _, _ = compute_l1_microprice_and_obi(c_bid, c_bsz, c_ask, c_asz)
            c_mid = c_micro if self.cfg.use_microprice_fair else c_mid_raw

        if (
            (self.last_obs_ms is not None and now_ms < self.last_obs_ms)
            or (self.tgt_vol_last is not None and now_ms < self.tgt_vol_last[0])
            or (self.confirm_last_obs_ms is not None and now_ms < self.confirm_last_obs_ms)
        ):
            return ("Blocked:InvalidInput", None, None, None)

        ref_stale = (now_ms - r_ts) > self.cfg.max_ref_age_ms
        tgt_stale = (now_ms - t_ts) > self.cfg.max_tgt_age_ms
        confirm_fresh = confirm_book is not None and (now_ms - c_ts) <= self.cfg.max_ref_age_ms

        r_mid, r_micro, r_obi, _ = compute_l1_microprice_and_obi(r_bid, r_bsz, r_ask, r_asz)
        t_mid, t_micro, _, t_spread_bps = compute_l1_microprice_and_obi(t_bid, t_bsz, t_ask, t_asz)

        ref_px = r_micro if self.cfg.use_microprice_fair else r_mid
        tgt_px = t_micro if self.cfg.use_microprice_fair else t_mid

        if not tgt_stale:
            if self.tgt_vol_last is not None:
                prev_ms, prev_mid = self.tgt_vol_last
                dt_ms = now_ms - prev_ms
                if dt_ms > self.cfg.max_gap_ms:
                    self.tgt_var_bps2_per_s = 0.0
                elif dt_ms > 0 and prev_mid > 0.0:
                    dt_s = dt_ms / 1000.0
                    ret_bps = math.log(tgt_px / prev_mid) * 1e4
                    inst_var = (ret_bps * ret_bps) / dt_s
                    if math.isfinite(inst_var):
                        alpha_v = 1.0 - math.exp(-dt_ms / float(self.cfg.vol_ewma_tau_ms))
                        self.tgt_var_bps2_per_s = (1.0 - alpha_v) * self.tgt_var_bps2_per_s + alpha_v * inst_var
            self.tgt_vol_last = (now_ms, tgt_px)

        dev_bps: float | None = None
        if not ref_stale and not tgt_stale:
            x = math.log(ref_px) - math.log(tgt_px)
            gap = (now_ms - self.last_obs_ms) if self.last_obs_ms is not None else None
            prior = self.ema if (self.ema is not None and gap is not None and gap <= self.cfg.max_gap_ms) else None
            dev_bps = (x - prior) * 1e4 if prior is not None else None
            if prior is not None and gap is not None:
                alpha = 1.0 - math.exp(-gap / float(self.cfg.tau_ms))
                self.ema = prior + alpha * (x - prior)
            else:
                self.first_obs_ms = now_ms
                self.ema = x
            if self.first_obs_ms is None:
                self.first_obs_ms = now_ms
            self.last_obs_ms = now_ms
            self.last_dev_bps = dev_bps

        confirm_dev_bps: float | None = None
        if not tgt_stale and confirm_fresh:
            x_c = math.log(c_mid) - math.log(tgt_px)
            gap_c = (now_ms - self.confirm_last_obs_ms) if self.confirm_last_obs_ms is not None else None
            prior_c = (
                self.confirm_ema
                if (self.confirm_ema is not None and gap_c is not None and gap_c <= self.cfg.max_gap_ms)
                else None
            )
            confirm_dev_bps = (x_c - prior_c) * 1e4 if prior_c is not None else None
            if prior_c is not None and gap_c is not None:
                alpha_c = 1.0 - math.exp(-gap_c / float(self.cfg.tau_ms))
                self.confirm_ema = prior_c + alpha_c * (x_c - prior_c)
            else:
                self.confirm_ema = x_c
            self.confirm_last_obs_ms = now_ms

        if self.pos is not None:
            if self.pos.exit_triggered or now_ms >= self.pos.exit_due_ms:
                if tgt_stale:
                    return ("Blocked:StaleTarget", None, None, None)
                self.pos.exit_triggered = True
                return ("Exit", self.pos.side, None, "timeout")
            if (
                self.cfg.ref_reversal_exit_bps is not None
                and not ref_stale
                and not tgt_stale
                and now_ms >= self.pos.entry_filled_est_ms
                and self.pos.entry_ref_px > 0.0
            ):
                ref_move_bps = self.pos.side * math.log(ref_px / self.pos.entry_ref_px) * 1e4
                if math.isfinite(ref_move_bps) and ref_move_bps <= -self.cfg.ref_reversal_exit_bps:
                    self.pos.exit_triggered = True
                    return ("Exit", self.pos.side, None, "ref_reversal")
            if (
                self.cfg.exit_on_catchup_frac is not None
                and not tgt_stale
                and now_ms >= self.pos.entry_filled_est_ms
                and self.pos.entry_tgt_mid > 0.0
                and self.pos.entry_gap_bps > 0.0
            ):
                catchup_bps = self.pos.side * math.log(tgt_px / self.pos.entry_tgt_mid) * 1e4
                if math.isfinite(catchup_bps) and catchup_bps >= self.cfg.exit_on_catchup_frac * self.pos.entry_gap_bps:
                    self.pos.exit_triggered = True
                    return ("Exit", self.pos.side, None, "catchup")
            return ("Idle", None, None, None)

        if ref_stale:
            return ("Blocked:StaleReference", None, None, None)
        if tgt_stale:
            return ("Blocked:StaleTarget", None, None, None)
        if dev_bps is None:
            return ("Blocked:WarmingUp", None, None, None)
        warm_since = self.first_obs_ms if self.first_obs_ms is not None else now_ms
        if (now_ms - warm_since) < self.cfg.warmup_ms:
            return ("Blocked:WarmingUp", None, None, None)
        if abs(dev_bps) >= self.cfg.max_dev_bps:
            return ("Blocked:StructuralBreak", None, None, None)
        if abs(dev_bps) < self.effective_theta_bps():
            return ("Idle", None, None, None)

        if self.cfg.max_entry_tgt_spread_bps is not None and t_spread_bps > self.cfg.max_entry_tgt_spread_bps:
            return ("Idle", None, None, None)
        if self.cfg.min_edge_over_spread_bps is not None:
            if (abs(dev_bps) - 0.5 * t_spread_bps) < self.cfg.min_edge_over_spread_bps:
                return ("Idle", None, None, None)

        side = 1 if dev_bps > 0.0 else -1
        if self.cfg.ref_obi_confirm_min > 0.0 and (side * r_obi) < self.cfg.ref_obi_confirm_min:
            return ("Idle", None, None, None)

        if self.cfg.require_dual_leader_confirm:
            if (
                not confirm_fresh
                or confirm_dev_bps is None
                or abs(confirm_dev_bps) >= self.cfg.max_dev_bps
                or (side * confirm_dev_bps) <= 0.0
            ):
                return ("Idle", None, None, None)

        entry_filled_est_ms = now_ms + self.cfg.taker_delay_ms
        self.pos = _OpenPos(
            side=side,
            entry_filled_est_ms=entry_filled_est_ms,
            exit_due_ms=entry_filled_est_ms + self.cfg.hold_ms,
            entry_tgt_mid=tgt_px,
            entry_ref_px=ref_px,
            entry_gap_bps=abs(dev_bps),
            exit_triggered=False,
        )
        return ("Enter", side, dev_bps, None)


def _mean_print_price(
    trade_ts: Sequence[int],
    trade_sgn: Sequence[int],
    trade_px: Sequence[float],
    t_start: int,
    window_ms: int,
    sgn: int,
) -> float | None:
    if not trade_ts:
        return None
    lo = bisect.bisect_left(trade_ts, t_start)
    hi = bisect.bisect_left(trade_ts, t_start + window_ms)
    if hi <= lo:
        return None
    total = 0.0
    cnt = 0
    for idx in range(lo, hi):
        if trade_sgn[idx] == sgn:
            total += trade_px[idx]
            cnt += 1
    return (total / cnt) if cnt > 0 else None


def _compute_t_stat(vals: Sequence[float]) -> float:
    n = len(vals)
    if n < 2:
        return 0.0
    mean_v = sum(vals) / n
    sd = statistics.stdev(vals)
    if sd <= 0.0:
        return 0.0
    return mean_v / (sd / math.sqrt(n))


def simulate_upgrade_streams(
    ref_books: Sequence[tuple[int, float, float, float, float]],
    tgt_books: Sequence[tuple[int, float, float, float, float]],
    cfg: LeadLagUpgradeConfig,
    *,
    confirm_books: Sequence[tuple[int, float, float, float, float]] | None = None,
    tertiary_books: Sequence[tuple[int, float, float, float, float]] | None = None,
    trades: Sequence[tuple[int, int, float, float]] | None = None,
    collect_trade_details: bool = False,
) -> dict[str, Any]:
    """Causal replay with optional 3-leader consensus, L1 microprice/OBI, spread gate, ref-reversal exit,
    and optional Stage A trade-level attribution telemetry."""
    if len(ref_books) < 2 or len(tgt_books) < 2:
        return _empty_sim_result(collect_trade_details)

    t0_ms = max(int(ref_books[0][0]), int(tgt_books[0][0]))
    t1_ms = min(int(ref_books[-1][0]), int(tgt_books[-1][0]))
    if t1_ms <= t0_ms + cfg.step_ms:
        return _empty_sim_result(collect_trade_details)

    tgt_ts_arr = [int(b[0]) for b in tgt_books]
    ref_ts_arr = [int(b[0]) for b in ref_books]
    n_ref = len(ref_books)
    n_tgt = len(tgt_books)
    n_conf = len(confirm_books) if confirm_books else 0
    n_tert = len(tertiary_books) if tertiary_books else 0

    if trades:
        tr_ts = [int(tr[0]) for tr in trades]
        tr_sgn = [int(tr[1]) for tr in trades]
        tr_px = [float(tr[2]) for tr in trades]
    else:
        tr_ts, tr_sgn, tr_px = [], [], []

    engine = LeadLagTakerUpgradePy(cfg)
    i_ref = i_tgt = i_conf = i_tert = 0
    has_ref = has_tgt = has_conf = has_tert = False

    active_trade: dict[str, Any] | None = None
    pending_exit_settle: dict[str, Any] | None = None

    quote_nets: list[float] = []
    pv_nets: list[float] = []
    entry_l1_list: list[float] = []
    hold_actual_ms_list: list[int] = []
    quote_clip_usd = {c: 0.0 for c in CLIPS_USD}
    pv_clip_usd = {c: 0.0 for c in CLIPS_USD}
    trade_details: list[dict[str, Any]] = []

    longs = 0
    stale_entry_skips = 0
    deferred_exits = 0
    unsettled_exits = 0
    early_catchup_exits = 0
    early_ref_reversal_exits = 0
    print_unverifiable = 0

    def latest_book_at(
        books: Sequence[tuple[int, float, float, float, float]],
        ts_arr: Sequence[int],
        ts_query: int,
    ) -> tuple[int, tuple[int, float, float, float, float]] | None:
        idx = bisect.bisect_right(ts_arr, ts_query) - 1
        if idx < 0:
            return None
        return idx, books[idx]

    def horizon_trajectory(
        side: int,
        t_fill: int,
        entry_px: float,
        entry_ref_px: float,
    ) -> dict[str, float | None]:
        out_traj: dict[str, float | None] = {}
        i0 = bisect.bisect_right(tgt_ts_arr, t_fill) - 1
        if i0 < 0:
            i0 = 0
        for h_s in (1, 3, 10):
            t_h = t_fill + h_s * 1_000
            found_h = latest_book_at(tgt_books, tgt_ts_arr, t_h)
            if found_h is None or t_h > t1_ms:
                out_traj[f"markout_{h_s}s_bps"] = None
                out_traj[f"mfe_{h_s}s_bps"] = None
                out_traj[f"mae_{h_s}s_bps"] = None
                continue
            i_end, hb = found_h
            h_mid = 0.5 * (float(hb[1]) + float(hb[3]))
            out_traj[f"markout_{h_s}s_bps"] = side * (h_mid / entry_px - 1.0) * 1e4
            best_mfe = -1e9
            worst_mae = 1e9
            for idx in range(i0, i_end + 1):
                bb = tgt_books[idx]
                exit_touch = float(bb[1]) if side > 0 else float(bb[3])
                ret = side * (exit_touch / entry_px - 1.0) * 1e4
                if ret > best_mfe:
                    best_mfe = ret
                if ret < worst_mae:
                    worst_mae = ret
            out_traj[f"mfe_{h_s}s_bps"] = best_mfe if best_mfe > -1e8 else None
            out_traj[f"mae_{h_s}s_bps"] = worst_mae if worst_mae < 1e8 else None

        # Reference reversal over [t_fill, t_fill + hold_ms]
        r0 = bisect.bisect_right(ref_ts_arr, t_fill) - 1
        r1 = bisect.bisect_right(ref_ts_arr, min(t1_ms, t_fill + cfg.hold_ms)) - 1
        min_ref_move = 0.0
        if r0 >= 0 and r1 >= r0 and entry_ref_px > 0.0:
            worst_r = 1e9
            for idx in range(r0, r1 + 1):
                rb_ = ref_books[idx]
                rm = 0.5 * (float(rb_[1]) + float(rb_[3]))
                if rm > 0.0:
                    mv = side * math.log(rm / entry_ref_px) * 1e4
                    if mv < worst_r:
                        worst_r = mv
            if worst_r < 1e8:
                min_ref_move = worst_r
        out_traj["min_ref_move_bps"] = min_ref_move
        return out_traj

    now_ms = t0_ms + cfg.step_ms
    while now_ms <= t1_ms:
        while i_ref < n_ref and int(ref_books[i_ref][0]) <= now_ms:
            has_ref = True
            i_ref += 1
        while i_tgt < n_tgt and int(tgt_books[i_tgt][0]) <= now_ms:
            has_tgt = True
            i_tgt += 1
        if confirm_books:
            while i_conf < n_conf and int(confirm_books[i_conf][0]) <= now_ms:
                has_conf = True
                i_conf += 1
        if tertiary_books:
            while i_tert < n_tert and int(tertiary_books[i_tert][0]) <= now_ms:
                has_tert = True
                i_tert += 1

        if pending_exit_settle is not None and now_ms >= pending_exit_settle["t_exit_actual"]:
            tr = pending_exit_settle
            side = tr["side"]
            entry_px = tr["entry_px"]
            exit_px = tr["exit_px"]
            entry_l1 = tr["entry_l1_usd"]
            q_net = side * (exit_px / entry_px - 1.0) * 1e4 - 2.0 * cfg.taker_fee_bps
            quote_nets.append(q_net)
            entry_l1_list.append(entry_l1)
            hold_actual_ms_list.append(tr["t_exit_actual"] - tr["t_entry_fill"])
            if side > 0:
                longs += 1
            for clip in CLIPS_USD:
                notl = min(clip, cfg.l1_depth_cap_frac * entry_l1)
                quote_clip_usd[clip] += notl * q_net * 1e-4

            pv_net_val: float | None = None
            if tr_ts:
                pe = _mean_print_price(tr_ts, tr_sgn, tr_px, tr["t_entry_fill"], cfg.print_window_ms, side)
                px_ = _mean_print_price(tr_ts, tr_sgn, tr_px, tr["t_exit_actual"], cfg.print_window_ms, -side)
                if pe is not None and px_ is not None and pe > 0.0 and px_ > 0.0:
                    pv_net_val = side * (px_ / pe - 1.0) * 1e4 - 2.0 * cfg.taker_fee_bps
                    pv_nets.append(pv_net_val)
                    for clip in CLIPS_USD:
                        notl = min(clip, cfg.l1_depth_cap_frac * entry_l1)
                        pv_clip_usd[clip] += notl * pv_net_val * 1e-4
                else:
                    print_unverifiable += 1

            if collect_trade_details:
                traj = horizon_trajectory(side, tr["t_entry_fill"], entry_px, tr["entry_ref_px"])
                trade_details.append(
                    {
                        "t_dec": tr["t_entry_dec"],
                        "side": side,
                        "abs_dev_bps": tr["abs_dev_bps"],
                        "ref_obi_aligned": tr["ref_obi_aligned"],
                        "tgt_spread_bps": tr["tgt_spread_bps"],
                        "edge_over_half_spread_bps": tr["abs_dev_bps"] - 0.5 * tr["tgt_spread_bps"],
                        "entry_l1_usd": entry_l1,
                        "quote_net_bps": q_net,
                        "pv_net_bps": pv_net_val,
                        "exit_reason": tr.get("exit_reason", "timeout"),
                        **traj,
                    }
                )

            engine.notify_exit_filled()
            pending_exit_settle = None
            active_trade = None

        if has_ref and has_tgt:
            rb = ref_books[i_ref - 1]
            tb = tgt_books[i_tgt - 1]
            cb = confirm_books[i_conf - 1] if (confirm_books and has_conf) else None
            tb3 = tertiary_books[i_tert - 1] if (tertiary_books and has_tert) else None

            if cfg.use_consensus_leader:
                # Build synthetic L1 consensus book using consensus_ref_mid across up to 3 leaders
                l1_candidates: list[tuple[float, int] | None] = [
                    (0.5 * (float(rb[1]) + float(rb[3])), int(rb[0])),
                    (0.5 * (float(cb[1]) + float(cb[3])), int(cb[0])) if cb is not None else None,
                    (0.5 * (float(tb3[1]) + float(tb3[3])), int(tb3[0])) if tb3 is not None else None,
                ]
                cons = consensus_ref_mid(now_ms, cfg.max_ref_age_ms, l1_candidates)
                if cons is not None:
                    c_px, c_ts = cons
                    # Preserve primary ref relative spread/sizes for OBI if primary is fresh
                    r_mid_raw = 0.5 * (float(rb[1]) + float(rb[3]))
                    half_spr = 0.5 * (float(rb[3]) - float(rb[1])) if r_mid_raw > 0.0 else 1e-6
                    ref_book_eff = (c_ts, c_px - max(half_spr, 1e-9), float(rb[2]), c_px + max(half_spr, 1e-9), float(rb[4]))
                else:
                    ref_book_eff = rb
                conf_book_eff = None
            else:
                ref_book_eff = rb
                conf_book_eff = cb

            kind, side_sgn, dev_bps, exit_reason = engine.observe_l1_books(now_ms, ref_book_eff, conf_book_eff, tb)

            if kind == "Enter" and side_sgn is not None and dev_bps is not None:
                t_entry_fill = now_ms + cfg.taker_delay_ms
                if t_entry_fill > t1_ms:
                    engine.clear_position()
                else:
                    found = latest_book_at(tgt_books, tgt_ts_arr, t_entry_fill)
                    if found is None:
                        stale_entry_skips += 1
                        engine.clear_position()
                    else:
                        _, eb = found
                        e_ts, e_bid, e_bsz, e_ask, e_asz = (
                            int(eb[0]),
                            float(eb[1]),
                            float(eb[2]),
                            float(eb[3]),
                            float(eb[4]),
                        )
                        if (t_entry_fill - e_ts) > cfg.max_tgt_age_ms or e_bid <= 0.0 or e_ask <= e_bid:
                            stale_entry_skips += 1
                            engine.clear_position()
                        else:
                            entry_px = e_ask if side_sgn > 0 else e_bid
                            entry_sz = e_asz if side_sgn > 0 else e_bsz
                            l1_usd = entry_px * entry_sz if (math.isfinite(entry_sz) and entry_sz > 0.0) else 0.0
                            r_mid_dec, _, r_obi_dec, _ = compute_l1_microprice_and_obi(
                                float(ref_book_eff[1]), float(ref_book_eff[2]), float(ref_book_eff[3]), float(ref_book_eff[4])
                            )
                            _, _, _, t_spr_dec = compute_l1_microprice_and_obi(
                                float(tb[1]), float(tb[2]), float(tb[3]), float(tb[4])
                            )
                            active_trade = {
                                "side": side_sgn,
                                "t_entry_dec": now_ms,
                                "t_entry_fill": t_entry_fill,
                                "entry_px": entry_px,
                                "entry_l1_usd": l1_usd,
                                "exit_due_ms": t_entry_fill + cfg.hold_ms,
                                "abs_dev_bps": abs(dev_bps),
                                "ref_obi_aligned": side_sgn * r_obi_dec,
                                "tgt_spread_bps": t_spr_dec,
                                "entry_ref_px": r_mid_dec,
                            }

            elif kind == "Exit" and active_trade is not None and pending_exit_settle is None:
                if exit_reason == "catchup":
                    early_catchup_exits += 1
                elif exit_reason == "ref_reversal":
                    early_ref_reversal_exits += 1
                t_exit_target = now_ms + cfg.taker_delay_ms
                found = latest_book_at(tgt_books, tgt_ts_arr, t_exit_target)
                exit_settled_px: float | None = None
                t_exit_actual = t_exit_target
                if found is not None:
                    f_idx, xb = found
                    x_ts, x_bid, x_ask = int(xb[0]), float(xb[1]), float(xb[3])
                    if (t_exit_target - x_ts) <= cfg.max_tgt_age_ms and x_bid > 0.0 and x_ask > x_bid:
                        exit_settled_px = x_bid if active_trade["side"] > 0 else x_ask
                    else:
                        scan = f_idx + 1
                        while scan < n_tgt:
                            sb = tgt_books[scan]
                            s_ts, s_bid, s_ask = int(sb[0]), float(sb[1]), float(sb[3])
                            if s_ts >= t_exit_target and s_bid > 0.0 and s_ask > s_bid:
                                exit_settled_px = s_bid if active_trade["side"] > 0 else s_ask
                                t_exit_actual = s_ts
                                deferred_exits += 1
                                break
                            scan += 1

                if exit_settled_px is None or t_exit_actual > t1_ms:
                    unsettled_exits += 1
                    engine.clear_position()
                    active_trade = None
                else:
                    pending_exit_settle = {
                        **active_trade,
                        "exit_px": exit_settled_px,
                        "t_exit_actual": t_exit_actual,
                        "exit_reason": exit_reason or "timeout",
                    }

        now_ms += cfg.step_ms

    if active_trade is not None or pending_exit_settle is not None:
        unsettled_exits += 1

    out = _finalize_sim_result(
        quote_nets,
        pv_nets,
        entry_l1_list,
        hold_actual_ms_list,
        quote_clip_usd,
        pv_clip_usd,
        longs,
        stale_entry_skips,
        deferred_exits,
        unsettled_exits,
        early_catchup_exits,
        early_ref_reversal_exits,
        print_unverifiable,
    )
    if collect_trade_details:
        out["trade_details"] = trade_details
    return out


@dataclasses.dataclass
class PrecomputedUpgradeSignals:
    """Window-level precomputed causal signals for fast bit-for-bit replay."""

    t0_ms: int
    t1_ms: int
    step_ms: int
    ts_ms: list[int]
    ref_books: Sequence[tuple[int, float, float, float, float]]
    ref_ts_arr: list[int]
    tgt_books: Sequence[tuple[int, float, float, float, float]]
    tgt_ts_arr: list[int]
    tr_ts: list[int]
    tr_sgn: list[int]
    tr_px: list[float]
    ref_stale: list[bool]
    tgt_stale: list[bool]
    ref_px: list[float]
    ref_obi: list[float]
    tgt_px: list[float]
    tgt_spread_bps: list[float]
    sigma_1s: list[float]
    conf_fresh: list[bool]
    can_enter_by_tau: dict[int, list[bool]]
    dev_by_tau: dict[int, list[float]]
    cdev_by_tau: dict[int, list[float | None]]
    cand_indices_by_tau: dict[int, list[int]]


def precompute_upgrade_signals(
    ref_books: Sequence[tuple[int, float, float, float, float]],
    tgt_books: Sequence[tuple[int, float, float, float, float]],
    *,
    confirm_books: Sequence[tuple[int, float, float, float, float]] | None = None,
    tertiary_books: Sequence[tuple[int, float, float, float, float]] | None = None,
    trades: Sequence[tuple[int, int, float, float]] | None = None,
    use_microprice_fair: bool = False,
    use_consensus_leader: bool = False,
    tau_values_ms: Sequence[int] = (30_000, 120_000),
    step_ms: int = STEP_MS,
    warmup_ms: int = DEFAULT_WARMUP_MS,
    max_ref_age_ms: int = DEFAULT_MAX_AGE_MS,
    max_tgt_age_ms: int = DEFAULT_MAX_AGE_MS,
    max_dev_bps: float = DEFAULT_MAX_DEV_BPS,
    max_gap_ms: int = DEFAULT_MAX_GAP_MS,
    vol_ewma_tau_ms: int = DEFAULT_VOL_EWMA_TAU_MS,
    min_cand_theta_bps: float = 4.0,
) -> PrecomputedUpgradeSignals | None:
    """Precompute causal EMA deviations, microprice/OBI, spreads, and sigma_1s once per window."""
    if len(ref_books) < 2 or len(tgt_books) < 2:
        return None
    t0_ms = max(int(ref_books[0][0]), int(tgt_books[0][0]))
    t1_ms = min(int(ref_books[-1][0]), int(tgt_books[-1][0]))
    if t1_ms <= t0_ms + step_ms:
        return None

    n_ref = len(ref_books)
    n_tgt = len(tgt_books)
    n_conf = len(confirm_books) if confirm_books else 0
    n_tert = len(tertiary_books) if tertiary_books else 0

    ts_list: list[int] = []
    ref_stale_list: list[bool] = []
    tgt_stale_list: list[bool] = []
    ref_px_list: list[float] = []
    ref_obi_list: list[float] = []
    tgt_px_list: list[float] = []
    tgt_spr_list: list[float] = []
    sigma_list: list[float] = []
    conf_fresh_list: list[bool] = []

    taus = tuple(sorted(set(int(t) for t in tau_values_ms)))
    ema_state: dict[int, float | None] = {tau: None for tau in taus}
    c_ema_state: dict[int, float | None] = {tau: None for tau in taus}
    can_enter_by_tau: dict[int, list[bool]] = {tau: [] for tau in taus}
    dev_by_tau: dict[int, list[float]] = {tau: [] for tau in taus}
    cdev_by_tau: dict[int, list[float | None]] = {tau: [] for tau in taus}
    cand_indices_by_tau: dict[int, list[int]] = {tau: [] for tau in taus}
    alpha_by_tau: dict[int, dict[int, float]] = {tau: {} for tau in taus}

    first_obs_ms: int | None = None
    last_obs_ms: int | None = None
    confirm_last_obs_ms: int | None = None
    tgt_vol_last: tuple[int, float] | None = None
    tgt_var = 0.0

    i_ref = i_tgt = i_conf = i_tert = 0
    has_ref = has_tgt = has_conf = has_tert = False
    k = 0
    now_ms = t0_ms + step_ms

    while now_ms <= t1_ms:
        while i_ref < n_ref and int(ref_books[i_ref][0]) <= now_ms:
            has_ref = True
            i_ref += 1
        while i_tgt < n_tgt and int(tgt_books[i_tgt][0]) <= now_ms:
            has_tgt = True
            i_tgt += 1
        if confirm_books:
            while i_conf < n_conf and int(confirm_books[i_conf][0]) <= now_ms:
                has_conf = True
                i_conf += 1
        if tertiary_books:
            while i_tert < n_tert and int(tertiary_books[i_tert][0]) <= now_ms:
                has_tert = True
                i_tert += 1

        ts_list.append(now_ms)
        if not (has_ref and has_tgt):
            ref_stale_list.append(True)
            tgt_stale_list.append(True)
            ref_px_list.append(0.0)
            ref_obi_list.append(0.0)
            tgt_px_list.append(0.0)
            tgt_spr_list.append(0.0)
            sigma_list.append(0.0)
            conf_fresh_list.append(False)
            for tau in taus:
                can_enter_by_tau[tau].append(False)
                dev_by_tau[tau].append(0.0)
                cdev_by_tau[tau].append(None)
            now_ms += step_ms
            k += 1
            continue

        rb = ref_books[i_ref - 1]
        tb = tgt_books[i_tgt - 1]
        cb = confirm_books[i_conf - 1] if (confirm_books and has_conf) else None
        tb3 = tertiary_books[i_tert - 1] if (tertiary_books and has_tert) else None

        if use_consensus_leader:
            l1_cands: list[tuple[float, int] | None] = [
                (0.5 * (float(rb[1]) + float(rb[3])), int(rb[0])),
                (0.5 * (float(cb[1]) + float(cb[3])), int(cb[0])) if cb is not None else None,
                (0.5 * (float(tb3[1]) + float(tb3[3])), int(tb3[0])) if tb3 is not None else None,
            ]
            cons = consensus_ref_mid(now_ms, max_ref_age_ms, l1_cands)
            if cons is not None:
                c_px, c_ts = cons
                r_mid_raw = 0.5 * (float(rb[1]) + float(rb[3]))
                half_spr = 0.5 * (float(rb[3]) - float(rb[1])) if r_mid_raw > 0.0 else 1e-6
                rb_eff = (c_ts, c_px - max(half_spr, 1e-9), float(rb[2]), c_px + max(half_spr, 1e-9), float(rb[4]))
            else:
                rb_eff = rb
            cb_eff = None
        else:
            rb_eff = rb
            cb_eff = cb

        r_ts, r_bid, r_bsz, r_ask, r_asz = int(rb_eff[0]), float(rb_eff[1]), float(rb_eff[2]), float(rb_eff[3]), float(rb_eff[4])
        t_ts, t_bid, t_bsz, t_ask, t_asz = int(tb[0]), float(tb[1]), float(tb[2]), float(tb[3]), float(tb[4])
        valid_px = (
            math.isfinite(r_bid) and math.isfinite(r_ask) and r_bid > 0.0 and r_ask > r_bid
            and math.isfinite(t_bid) and math.isfinite(t_ask) and t_bid > 0.0 and t_ask > t_bid
        )
        if not valid_px:
            ref_stale_list.append(True)
            tgt_stale_list.append(True)
            ref_px_list.append(0.0)
            ref_obi_list.append(0.0)
            tgt_px_list.append(0.0)
            tgt_spr_list.append(0.0)
            sigma_list.append(math.sqrt(max(0.0, tgt_var)))
            conf_fresh_list.append(False)
            for tau in taus:
                can_enter_by_tau[tau].append(False)
                dev_by_tau[tau].append(0.0)
                cdev_by_tau[tau].append(None)
            now_ms += step_ms
            k += 1
            continue

        r_mid, r_micro, r_obi, _ = compute_l1_microprice_and_obi(r_bid, r_bsz, r_ask, r_asz)
        t_mid, t_micro, _, t_spr_bps = compute_l1_microprice_and_obi(t_bid, t_bsz, t_ask, t_asz)
        ref_px = r_micro if use_microprice_fair else r_mid
        tgt_px = t_micro if use_microprice_fair else t_mid

        ref_stale = (now_ms - r_ts) > max_ref_age_ms
        tgt_stale = (now_ms - t_ts) > max_tgt_age_ms

        c_px_val = 0.0
        c_fresh = False
        if cb_eff is not None:
            c_ts, c_bid, c_bsz, c_ask, c_asz = int(cb_eff[0]), float(cb_eff[1]), float(cb_eff[2]), float(cb_eff[3]), float(cb_eff[4])
            if math.isfinite(c_bid) and math.isfinite(c_ask) and c_bid > 0.0 and c_ask > c_bid and (now_ms - c_ts) <= max_ref_age_ms:
                cm, cmic, _, _ = compute_l1_microprice_and_obi(c_bid, c_bsz, c_ask, c_asz)
                c_px_val = cmic if use_microprice_fair else cm
                c_fresh = True

        if not tgt_stale:
            if tgt_vol_last is not None:
                prev_ms, prev_mid = tgt_vol_last
                dt_ms = now_ms - prev_ms
                if dt_ms > max_gap_ms:
                    tgt_var = 0.0
                elif dt_ms > 0 and prev_mid > 0.0:
                    dt_s = dt_ms / 1000.0
                    ret_bps = math.log(tgt_px / prev_mid) * 1e4
                    inst_var = (ret_bps * ret_bps) / dt_s
                    if math.isfinite(inst_var):
                        alpha_v = 1.0 - math.exp(-dt_ms / float(vol_ewma_tau_ms))
                        tgt_var = (1.0 - alpha_v) * tgt_var + alpha_v * inst_var
            tgt_vol_last = (now_ms, tgt_px)

        ref_stale_list.append(ref_stale)
        tgt_stale_list.append(tgt_stale)
        ref_px_list.append(ref_px)
        ref_obi_list.append(r_obi)
        tgt_px_list.append(tgt_px)
        tgt_spr_list.append(t_spr_bps)
        sigma_list.append(math.sqrt(max(0.0, tgt_var)))
        conf_fresh_list.append(c_fresh)

        if not ref_stale and not tgt_stale:
            x = math.log(ref_px) - math.log(tgt_px)
            gap = (now_ms - last_obs_ms) if last_obs_ms is not None else None
            in_gap = gap is not None and gap <= max_gap_ms
            if not in_gap or first_obs_ms is None:
                first_obs_ms = now_ms
            warmed = (now_ms - first_obs_ms) >= warmup_ms
            for tau in taus:
                prior = ema_state[tau] if in_gap else None
                if prior is not None and gap is not None:
                    dev = (x - prior) * 1e4
                    a = alpha_by_tau[tau].get(gap)
                    if a is None:
                        a = 1.0 - math.exp(-gap / float(tau))
                        alpha_by_tau[tau][gap] = a
                    ema_state[tau] = prior + a * (x - prior)
                    ok = warmed and abs(dev) < max_dev_bps
                    can_enter_by_tau[tau].append(ok)
                    dev_by_tau[tau].append(dev)
                    if ok and abs(dev) >= min_cand_theta_bps:
                        cand_indices_by_tau[tau].append(k)
                else:
                    ema_state[tau] = x
                    can_enter_by_tau[tau].append(False)
                    dev_by_tau[tau].append(0.0)
            last_obs_ms = now_ms
        else:
            for tau in taus:
                can_enter_by_tau[tau].append(False)
                dev_by_tau[tau].append(0.0)

        if not tgt_stale and c_fresh:
            x_c = math.log(c_px_val) - math.log(tgt_px)
            gap_c = (now_ms - confirm_last_obs_ms) if confirm_last_obs_ms is not None else None
            in_gap_c = gap_c is not None and gap_c <= max_gap_ms
            for tau in taus:
                prior_c = c_ema_state[tau] if in_gap_c else None
                if prior_c is not None and gap_c is not None:
                    c_dev = (x_c - prior_c) * 1e4
                    a_c = alpha_by_tau[tau].get(gap_c)
                    if a_c is None:
                        a_c = 1.0 - math.exp(-gap_c / float(tau))
                        alpha_by_tau[tau][gap_c] = a_c
                    c_ema_state[tau] = prior_c + a_c * (x_c - prior_c)
                    cdev_by_tau[tau].append(c_dev)
                else:
                    c_ema_state[tau] = x_c
                    cdev_by_tau[tau].append(None)
            confirm_last_obs_ms = now_ms
        else:
            for tau in taus:
                cdev_by_tau[tau].append(None)

        now_ms += step_ms
        k += 1

    if trades:
        tr_ts = [int(tr[0]) for tr in trades]
        tr_sgn = [int(tr[1]) for tr in trades]
        tr_px = [float(tr[2]) for tr in trades]
    else:
        tr_ts, tr_sgn, tr_px = [], [], []

    return PrecomputedUpgradeSignals(
        t0_ms=t0_ms,
        t1_ms=t1_ms,
        step_ms=step_ms,
        ts_ms=ts_list,
        ref_books=ref_books,
        ref_ts_arr=[int(b[0]) for b in ref_books],
        tgt_books=tgt_books,
        tgt_ts_arr=[int(b[0]) for b in tgt_books],
        tr_ts=tr_ts,
        tr_sgn=tr_sgn,
        tr_px=tr_px,
        ref_stale=ref_stale_list,
        tgt_stale=tgt_stale_list,
        ref_px=ref_px_list,
        ref_obi=ref_obi_list,
        tgt_px=tgt_px_list,
        tgt_spread_bps=tgt_spr_list,
        sigma_1s=sigma_list,
        conf_fresh=conf_fresh_list,
        can_enter_by_tau=can_enter_by_tau,
        dev_by_tau=dev_by_tau,
        cdev_by_tau=cdev_by_tau,
        cand_indices_by_tau=cand_indices_by_tau,
    )


def simulate_precomputed_upgrade(
    sig: PrecomputedUpgradeSignals,
    cfg: LeadLagUpgradeConfig,
    *,
    collect_trade_details: bool = False,
) -> dict[str, Any]:
    """Fast exact replay over precomputed signals (bit-for-bit identical to `simulate_upgrade_streams`)."""
    n_steps = len(sig.ts_ms)
    if n_steps == 0:
        return _empty_sim_result(collect_trade_details)

    ts_ms = sig.ts_ms
    tgt_books = sig.tgt_books
    tgt_ts_arr = sig.tgt_ts_arr
    ref_books = sig.ref_books
    ref_ts_arr = sig.ref_ts_arr
    n_tgt = len(tgt_books)
    t1_ms = sig.t1_ms
    tr_ts, tr_sgn, tr_px = sig.tr_ts, sig.tr_sgn, sig.tr_px

    can_enter = sig.can_enter_by_tau[cfg.tau_ms]
    dev_arr = sig.dev_by_tau[cfg.tau_ms]
    cdev_arr = sig.cdev_by_tau[cfg.tau_ms]
    cand_indices = sig.cand_indices_by_tau[cfg.tau_ms]
    ref_stale = sig.ref_stale
    tgt_stale = sig.tgt_stale
    ref_px_arr = sig.ref_px
    ref_obi_arr = sig.ref_obi
    tgt_px_arr = sig.tgt_px
    tgt_spr_arr = sig.tgt_spread_bps
    sigma_1s = sig.sigma_1s
    conf_fresh = sig.conf_fresh

    quote_nets: list[float] = []
    pv_nets: list[float] = []
    entry_l1_list: list[float] = []
    hold_actual_ms_list: list[int] = []
    quote_clip_usd = {c: 0.0 for c in CLIPS_USD}
    pv_clip_usd = {c: 0.0 for c in CLIPS_USD}
    trade_details: list[dict[str, Any]] = []

    longs = 0
    stale_entry_skips = 0
    deferred_exits = 0
    unsettled_exits = 0
    early_catchup_exits = 0
    early_ref_reversal_exits = 0
    print_unverifiable = 0

    def latest_book_at(
        books: Sequence[tuple[int, float, float, float, float]],
        ts_arr: Sequence[int],
        ts_query: int,
    ) -> tuple[int, tuple[int, float, float, float, float]] | None:
        idx = bisect.bisect_right(ts_arr, ts_query) - 1
        if idx < 0:
            return None
        return idx, books[idx]

    def horizon_trajectory(side: int, t_fill: int, entry_px: float, entry_ref_px: float) -> dict[str, float | None]:
        out_traj: dict[str, float | None] = {}
        i0 = bisect.bisect_right(tgt_ts_arr, t_fill) - 1
        if i0 < 0:
            i0 = 0
        for h_s in (1, 3, 10):
            t_h = t_fill + h_s * 1_000
            found_h = latest_book_at(tgt_books, tgt_ts_arr, t_h)
            if found_h is None or t_h > t1_ms:
                out_traj[f"markout_{h_s}s_bps"] = None
                out_traj[f"mfe_{h_s}s_bps"] = None
                out_traj[f"mae_{h_s}s_bps"] = None
                continue
            i_end, hb = found_h
            h_mid = 0.5 * (float(hb[1]) + float(hb[3]))
            out_traj[f"markout_{h_s}s_bps"] = side * (h_mid / entry_px - 1.0) * 1e4
            best_mfe = -1e9
            worst_mae = 1e9
            for idx in range(i0, i_end + 1):
                bb = tgt_books[idx]
                exit_touch = float(bb[1]) if side > 0 else float(bb[3])
                ret = side * (exit_touch / entry_px - 1.0) * 1e4
                if ret > best_mfe:
                    best_mfe = ret
                if ret < worst_mae:
                    worst_mae = ret
            out_traj[f"mfe_{h_s}s_bps"] = best_mfe if best_mfe > -1e8 else None
            out_traj[f"mae_{h_s}s_bps"] = worst_mae if worst_mae < 1e8 else None

        r0 = bisect.bisect_right(ref_ts_arr, t_fill) - 1
        r1 = bisect.bisect_right(ref_ts_arr, min(t1_ms, t_fill + cfg.hold_ms)) - 1
        min_ref_move = 0.0
        if r0 >= 0 and r1 >= r0 and entry_ref_px > 0.0:
            worst_r = 1e9
            for idx in range(r0, r1 + 1):
                rb_ = ref_books[idx]
                rm = 0.5 * (float(rb_[1]) + float(rb_[3]))
                if rm > 0.0:
                    mv = side * math.log(rm / entry_ref_px) * 1e4
                    if mv < worst_r:
                        worst_r = mv
            if worst_r < 1e8:
                min_ref_move = worst_r
        out_traj["min_ref_move_bps"] = min_ref_move
        return out_traj

    c_pos = 0
    n_cands = len(cand_indices)
    min_k = 0
    rev_bps = cfg.ref_reversal_exit_bps
    frac = cfg.exit_on_catchup_frac

    while c_pos < n_cands:
        k = cand_indices[c_pos]
        if k < min_k:
            c_pos = bisect.bisect_left(cand_indices, min_k, c_pos)
            continue
        c_pos += 1
        if not can_enter[k]:
            continue
        d = dev_arr[k]
        eff_theta = max(cfg.theta_bps, cfg.vol_theta_mult * sigma_1s[k]) if cfg.vol_theta_mult > 0.0 else cfg.theta_bps
        if abs(d) < eff_theta:
            continue
        t_spr = tgt_spr_arr[k]
        if cfg.max_entry_tgt_spread_bps is not None and t_spr > cfg.max_entry_tgt_spread_bps:
            continue
        if cfg.min_edge_over_spread_bps is not None and (abs(d) - 0.5 * t_spr) < cfg.min_edge_over_spread_bps:
            continue
        side = 1 if d > 0.0 else -1
        if cfg.ref_obi_confirm_min > 0.0 and (side * ref_obi_arr[k]) < cfg.ref_obi_confirm_min:
            continue
        if cfg.require_dual_leader_confirm:
            cd = cdev_arr[k]
            if not conf_fresh[k] or cd is None or abs(cd) >= cfg.max_dev_bps or (side * cd) <= 0.0:
                continue

        now_ms = ts_ms[k]
        t_entry_fill = now_ms + cfg.taker_delay_ms
        if t_entry_fill > t1_ms:
            continue
        found = latest_book_at(tgt_books, tgt_ts_arr, t_entry_fill)
        if found is None:
            stale_entry_skips += 1
            continue
        _, eb = found
        e_ts, e_bid, e_bsz, e_ask, e_asz = int(eb[0]), float(eb[1]), float(eb[2]), float(eb[3]), float(eb[4])
        if (t_entry_fill - e_ts) > cfg.max_tgt_age_ms or e_bid <= 0.0 or e_ask <= e_bid:
            stale_entry_skips += 1
            continue

        entry_px = e_ask if side > 0 else e_bid
        entry_sz = e_asz if side > 0 else e_bsz
        entry_l1 = entry_px * entry_sz if (math.isfinite(entry_sz) and entry_sz > 0.0) else 0.0
        exit_due_ms = t_entry_fill + cfg.hold_ms
        entry_tgt_mid = tgt_px_arr[k]
        entry_ref_px = ref_px_arr[k]
        entry_gap_bps = abs(d)

        j_exit_dec: int | None = None
        exit_reason = "timeout"
        if frac is None and rev_bps is None:
            j = bisect.bisect_left(ts_ms, exit_due_ms, k + 1)
            while j < n_steps:
                if not tgt_stale[j]:
                    j_exit_dec = j
                    break
                j += 1
        else:
            j = k + 1
            while j < n_steps:
                t_j = ts_ms[j]
                if t_j >= exit_due_ms:
                    if not tgt_stale[j]:
                        j_exit_dec = j
                        exit_reason = "timeout"
                        break
                elif t_j >= t_entry_fill and not tgt_stale[j]:
                    if rev_bps is not None and not ref_stale[j] and entry_ref_px > 0.0:
                        ref_move = side * math.log(ref_px_arr[j] / entry_ref_px) * 1e4
                        if math.isfinite(ref_move) and ref_move <= -rev_bps:
                            j_exit_dec = j
                            exit_reason = "ref_reversal"
                            early_ref_reversal_exits += 1
                            break
                    if frac is not None and entry_tgt_mid > 0.0 and entry_gap_bps > 0.0:
                        catchup_bps = side * math.log(tgt_px_arr[j] / entry_tgt_mid) * 1e4
                        if math.isfinite(catchup_bps) and catchup_bps >= frac * entry_gap_bps:
                            j_exit_dec = j
                            exit_reason = "catchup"
                            early_catchup_exits += 1
                            break
                j += 1

        if j_exit_dec is None:
            unsettled_exits += 1
            break

        t_exit_target = ts_ms[j_exit_dec] + cfg.taker_delay_ms
        found_x = latest_book_at(tgt_books, tgt_ts_arr, t_exit_target)
        exit_settled_px: float | None = None
        t_exit_actual = t_exit_target
        if found_x is not None:
            f_idx, xb = found_x
            x_ts, x_bid, x_ask = int(xb[0]), float(xb[1]), float(xb[3])
            if (t_exit_target - x_ts) <= cfg.max_tgt_age_ms and x_bid > 0.0 and x_ask > x_bid:
                exit_settled_px = x_bid if side > 0 else x_ask
            else:
                scan = f_idx + 1
                while scan < n_tgt:
                    sb = tgt_books[scan]
                    s_ts, s_bid, s_ask = int(sb[0]), float(sb[1]), float(sb[3])
                    if s_ts >= t_exit_target and s_bid > 0.0 and s_ask > s_bid:
                        exit_settled_px = s_bid if side > 0 else s_ask
                        t_exit_actual = s_ts
                        deferred_exits += 1
                        break
                    scan += 1

        if exit_settled_px is None or t_exit_actual > t1_ms:
            unsettled_exits += 1
            break

        k_settle = bisect.bisect_left(ts_ms, t_exit_actual, j_exit_dec)
        if k_settle >= n_steps:
            unsettled_exits += 1
            break

        q_net = side * (exit_settled_px / entry_px - 1.0) * 1e4 - 2.0 * cfg.taker_fee_bps
        quote_nets.append(q_net)
        entry_l1_list.append(entry_l1)
        hold_actual_ms_list.append(t_exit_actual - t_entry_fill)
        if side > 0:
            longs += 1
        for clip in CLIPS_USD:
            notl = min(clip, cfg.l1_depth_cap_frac * entry_l1)
            quote_clip_usd[clip] += notl * q_net * 1e-4

        pv_net_val: float | None = None
        if tr_ts:
            pe = _mean_print_price(tr_ts, tr_sgn, tr_px, t_entry_fill, cfg.print_window_ms, side)
            px_ = _mean_print_price(tr_ts, tr_sgn, tr_px, t_exit_actual, cfg.print_window_ms, -side)
            if pe is not None and px_ is not None and pe > 0.0 and px_ > 0.0:
                pv_net_val = side * (px_ / pe - 1.0) * 1e4 - 2.0 * cfg.taker_fee_bps
                pv_nets.append(pv_net_val)
                for clip in CLIPS_USD:
                    notl = min(clip, cfg.l1_depth_cap_frac * entry_l1)
                    pv_clip_usd[clip] += notl * pv_net_val * 1e-4
            else:
                print_unverifiable += 1

        if collect_trade_details:
            traj = horizon_trajectory(side, t_entry_fill, entry_px, entry_ref_px)
            trade_details.append(
                {
                    "t_dec": now_ms,
                    "side": side,
                    "abs_dev_bps": entry_gap_bps,
                    "ref_obi_aligned": side * ref_obi_arr[k],
                    "tgt_spread_bps": t_spr,
                    "edge_over_half_spread_bps": entry_gap_bps - 0.5 * t_spr,
                    "entry_l1_usd": entry_l1,
                    "quote_net_bps": q_net,
                    "pv_net_bps": pv_net_val,
                    "exit_reason": exit_reason,
                    **traj,
                }
            )

        min_k = k_settle

    out = _finalize_sim_result(
        quote_nets,
        pv_nets,
        entry_l1_list,
        hold_actual_ms_list,
        quote_clip_usd,
        pv_clip_usd,
        longs,
        stale_entry_skips,
        deferred_exits,
        unsettled_exits,
        early_catchup_exits,
        early_ref_reversal_exits,
        print_unverifiable,
    )
    if collect_trade_details:
        out["trade_details"] = trade_details
    return out


def _empty_sim_result(include_details: bool = False) -> dict[str, Any]:
    d: dict[str, Any] = {
        "trades": 0,
        "long_share": 0.0,
        "quote_net_bps_total": 0.0,
        "quote_net_bps_mean": 0.0,
        "quote_t_stat": 0.0,
        "quote_hit_rate": 0.0,
        "print_verified_trades": 0,
        "print_unverifiable": 0,
        "print_verified_net_bps_total": 0.0,
        "print_verified_net_bps_mean": None,
        "print_verified_t_stat": None,
        "print_verified_hit_rate": None,
        "quote_clip_pnl_usd": {str(int(c)): 0.0 for c in CLIPS_USD},
        "pv_clip_pnl_usd": {str(int(c)): 0.0 for c in CLIPS_USD},
        "median_entry_l1_usd": 0.0,
        "stale_entry_skips": 0,
        "deferred_exits": 0,
        "unsettled_exits": 0,
        "early_catchup_exits": 0,
        "early_ref_reversal_exits": 0,
        "avg_hold_actual_ms": 0.0,
    }
    if include_details:
        d["trade_details"] = []
    return d


def _finalize_sim_result(
    quote_nets: Sequence[float],
    pv_nets: Sequence[float],
    entry_l1_list: Sequence[float],
    hold_actual_ms_list: Sequence[int],
    quote_clip_usd: dict[float, float],
    pv_clip_usd: dict[float, float],
    longs: int,
    stale_entry_skips: int,
    deferred_exits: int,
    unsettled_exits: int,
    early_catchup_exits: int,
    early_ref_reversal_exits: int,
    print_unverifiable: int,
) -> dict[str, Any]:
    k = len(quote_nets)
    if k == 0:
        out = _empty_sim_result(False)
        out["print_unverifiable"] = print_unverifiable
        out["stale_entry_skips"] = stale_entry_skips
        out["deferred_exits"] = deferred_exits
        out["unsettled_exits"] = unsettled_exits
        out["early_catchup_exits"] = early_catchup_exits
        out["early_ref_reversal_exits"] = early_ref_reversal_exits
        return out
    pv_k = len(pv_nets)
    return {
        "trades": k,
        "long_share": round(longs / k, 3),
        "quote_net_bps_total": round(sum(quote_nets), 4),
        "quote_net_bps_mean": round(sum(quote_nets) / k, 4),
        "quote_t_stat": round(_compute_t_stat(quote_nets), 3),
        "quote_hit_rate": round(sum(1 for v in quote_nets if v > 0.0) / k, 3),
        "print_verified_trades": pv_k,
        "print_unverifiable": print_unverifiable,
        "print_verified_net_bps_total": round(sum(pv_nets), 4) if pv_k else 0.0,
        "print_verified_net_bps_mean": round(sum(pv_nets) / pv_k, 4) if pv_k else None,
        "print_verified_t_stat": round(_compute_t_stat(pv_nets), 3) if pv_k >= 2 else None,
        "print_verified_hit_rate": round(sum(1 for v in pv_nets if v > 0.0) / pv_k, 3) if pv_k else None,
        "quote_clip_pnl_usd": {str(int(c)): round(quote_clip_usd[c], 4) for c in CLIPS_USD},
        "pv_clip_pnl_usd": {str(int(c)): round(pv_clip_usd[c], 4) for c in CLIPS_USD},
        "median_entry_l1_usd": round(statistics.median(entry_l1_list), 2) if entry_l1_list else 0.0,
        "stale_entry_skips": stale_entry_skips,
        "deferred_exits": deferred_exits,
        "unsettled_exits": unsettled_exits,
        "early_catchup_exits": early_catchup_exits,
        "early_ref_reversal_exits": early_ref_reversal_exits,
        "avg_hold_actual_ms": round(sum(hold_actual_ms_list) / len(hold_actual_ms_list), 1)
        if hold_actual_ms_list
        else 0.0,
    }


def summarize_stage_a_trades(trades: Sequence[dict[str, Any]]) -> dict[str, Any]:
    """Aggregate Stage A trade-level telemetry into diagnostic buckets."""
    def _bucket_stats(rows: Sequence[dict[str, Any]]) -> dict[str, Any]:
        if not rows:
            return {"trades": 0, "quote_mean_bps": None, "quote_total_bps": 0.0, "pv_trades": 0, "pv_mean_bps": None, "pv_total_bps": 0.0, "quote_hit_rate": None}
        q_vals = [float(r["quote_net_bps"]) for r in rows]
        pv_vals = [float(r["pv_net_bps"]) for r in rows if r["pv_net_bps"] is not None]
        return {
            "trades": len(rows),
            "quote_mean_bps": round(sum(q_vals) / len(q_vals), 4),
            "quote_total_bps": round(sum(q_vals), 2),
            "quote_hit_rate": round(sum(1 for v in q_vals if v > 0.0) / len(q_vals), 3),
            "pv_trades": len(pv_vals),
            "pv_mean_bps": round(sum(pv_vals) / len(pv_vals), 4) if pv_vals else None,
            "pv_total_bps": round(sum(pv_vals), 2),
        }

    theta_buckets = {
        "[4,6)": [r for r in trades if 4.0 <= r["abs_dev_bps"] < 6.0],
        "[6,8)": [r for r in trades if 6.0 <= r["abs_dev_bps"] < 8.0],
        "[8,12)": [r for r in trades if 8.0 <= r["abs_dev_bps"] < 12.0],
        "[12,20)": [r for r in trades if 12.0 <= r["abs_dev_bps"] < 20.0],
        "[20,60)": [r for r in trades if r["abs_dev_bps"] >= 20.0],
    }
    obi_buckets = {
        "aligned_strong(>=0.25)": [r for r in trades if r["ref_obi_aligned"] >= 0.25],
        "aligned_mild(0,0.25)": [r for r in trades if 0.0 < r["ref_obi_aligned"] < 0.25],
        "neutral_or_zero(==0)": [r for r in trades if r["ref_obi_aligned"] == 0.0],
        "opposed(<0)": [r for r in trades if r["ref_obi_aligned"] < 0.0],
        "opposed_strong(<=-0.25)": [r for r in trades if r["ref_obi_aligned"] <= -0.25],
    }
    spread_buckets = {
        "[0,0.5)bp": [r for r in trades if r["tgt_spread_bps"] < 0.5],
        "[0.5,1.0)bp": [r for r in trades if 0.5 <= r["tgt_spread_bps"] < 1.0],
        "[1.0,2.0)bp": [r for r in trades if 1.0 <= r["tgt_spread_bps"] < 2.0],
        "[2.0,4.0)bp": [r for r in trades if 2.0 <= r["tgt_spread_bps"] < 4.0],
        "[4.0,inf)bp": [r for r in trades if r["tgt_spread_bps"] >= 4.0],
    }
    edge_over_half_buckets = {
        "<3bp": [r for r in trades if r["edge_over_half_spread_bps"] < 3.0],
        "[3,5)bp": [r for r in trades if 3.0 <= r["edge_over_half_spread_bps"] < 5.0],
        "[5,8)bp": [r for r in trades if 5.0 <= r["edge_over_half_spread_bps"] < 8.0],
        ">=8bp": [r for r in trades if r["edge_over_half_spread_bps"] >= 8.0],
    }
    reversal_buckets = {
        "snapback<=-5bp": [r for r in trades if r["min_ref_move_bps"] <= -5.0],
        "snapback(-5,-3]bp": [r for r in trades if -5.0 < r["min_ref_move_bps"] <= -3.0],
        "snapback(-3,-2]bp": [r for r in trades if -3.0 < r["min_ref_move_bps"] <= -2.0],
        "no_snapback(>-2bp)": [r for r in trades if r["min_ref_move_bps"] > -2.0],
    }
    traj_summary: dict[str, Any] = {}
    for h_s in (1, 3, 10):
        mks = [float(r[f"markout_{h_s}s_bps"]) for r in trades if r.get(f"markout_{h_s}s_bps") is not None]
        mfes = [float(r[f"mfe_{h_s}s_bps"]) for r in trades if r.get(f"mfe_{h_s}s_bps") is not None]
        maes = [float(r[f"mae_{h_s}s_bps"]) for r in trades if r.get(f"mae_{h_s}s_bps") is not None]
        traj_summary[f"{h_s}s"] = {
            "n": len(mks),
            "markout_mean_bps": round(sum(mks) / len(mks), 4) if mks else None,
            "mfe_mean_bps": round(sum(mfes) / len(mfes), 4) if mfes else None,
            "mae_mean_bps": round(sum(maes) / len(maes), 4) if maes else None,
        }

    return {
        "total_trades": len(trades),
        "by_theta": {k: _bucket_stats(v) for k, v in theta_buckets.items()},
        "by_ref_obi": {k: _bucket_stats(v) for k, v in obi_buckets.items()},
        "by_tgt_spread": {k: _bucket_stats(v) for k, v in spread_buckets.items()},
        "by_edge_over_half_spread": {k: _bucket_stats(v) for k, v in edge_over_half_buckets.items()},
        "by_ref_reversal": {k: _bucket_stats(v) for k, v in reversal_buckets.items()},
        "trajectory_mfe_mae": traj_summary,
    }


def _campaign_parquet_files(
    lakehouse_dir: Path,
    dataset: str,
    venue: str,
    utc_date: str,
    campaign_id: str,
) -> list[str]:
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


def discover_campaigns(lakehouse_dir: Path, domain: str) -> dict[str, dict[str, str]]:
    prefix = "tokyo-crypto-" if domain == "crypto" else "tokyo-rwa-"
    base = lakehouse_dir / "parquet" / "bbo_quotes" / "collector_region=tokyo"
    found: dict[int, tuple[str, list[str]]] = {}
    for p in glob.glob(str(base / "venue=*" / "utc_date=*" / f"campaign_id={prefix}*")):
        parts = Path(p).parts
        cid = parts[-1].split("=", 1)[1]
        utc_date = parts[-2].split("=", 1)[1]
        m = re.search(r"-w(\d+)$", cid)
        if not m:
            continue
        w_num = int(m.group(1))
        if w_num not in found:
            found[w_num] = (cid, [utc_date])
        else:
            found[w_num][1].append(utc_date)
    return {
        f"w{w_num:02d}": {"campaign_id": cid, "utc_date": min(dates)}
        for w_num, (cid, dates) in sorted(found.items())
    }


def split_campaign_windows(discovered: dict[str, dict[str, str]]) -> dict[str, dict[str, dict[str, str]]]:
    def _wnum(wid: str) -> int:
        return int(wid[1:])

    return {
        "selection_dev": {w: info for w, info in sorted(discovered.items()) if _wnum(w) <= 20 and w != "w08"},
        "holdout": {w: info for w, info in sorted(discovered.items()) if 21 <= _wnum(w) <= 25},
        "w26_oos": {w: info for w, info in sorted(discovered.items()) if _wnum(w) == 26},
    }


def _load_window_venue_data(
    con: Any,
    lakehouse_dir: Path,
    win_info: dict[str, str],
    venue: str,
    symbols: Sequence[str],
    *,
    load_trades: bool,
) -> tuple[
    dict[str, tuple[list[tuple[int, float, float, float, float]], dict[str, Any]]],
    dict[str, list[tuple[int, int, float, float]]],
]:
    """Batch-load all requested symbols for one `(window, venue)` in a single DuckDB scan."""
    sym_to_mkt = {s: VENUE_SYMBOL_MAP[venue][s] for s in symbols if s in VENUE_SYMBOL_MAP.get(venue, {})}
    mkt_to_sym = {m: s for s, m in sym_to_mkt.items()}
    bbo_out: dict[str, tuple[list[tuple[int, float, float, float, float]], dict[str, Any]]] = {
        s: ([], {"raw_events": 0, "duration_ms": 0, "hours": 0.0, "max_gap_ms": 0, "is_continuous_10h": False})
        for s in symbols
    }
    tr_out: dict[str, list[tuple[int, int, float, float]]] = {s: [] for s in symbols}
    if not sym_to_mkt:
        return bbo_out, tr_out

    files = _campaign_parquet_files(lakehouse_dir, "bbo_quotes", venue, win_info["utc_date"], win_info["campaign_id"])
    if files:
        f_sql = "[" + ", ".join("'" + p.replace("'", "''") + "'" for p in files) + "]"
        mkts_sql = ", ".join("'" + m.replace("'", "''") + "'" for m in mkt_to_sym)
        if venue == "binance":
            stats_rows = con.execute(
                f"""
                WITH b AS (
                    SELECT market, (available_at_ms // {STEP_MS}) AS bucket,
                           MAX(available_at_ms) AS ts_ms, MIN(available_at_ms) AS min_ts, COUNT(*) AS cnt
                    FROM read_parquet({f_sql})
                    WHERE market IN ({mkts_sql}) AND tradable = true AND bid_px_f64 > 0 AND ask_px_f64 > bid_px_f64
                    GROUP BY 1, 2
                ),
                g AS (
                    SELECT market, ts_ms, min_ts, cnt, LAG(ts_ms) OVER (PARTITION BY market ORDER BY bucket) AS prev_ts FROM b
                )
                SELECT market, COALESCE(SUM(cnt), 0), MIN(min_ts), MAX(ts_ms), COALESCE(MAX(ts_ms - prev_ts), 0)
                FROM g GROUP BY 1
                """
            ).fetchall()
        else:
            stats_rows = con.execute(
                f"""
                WITH s AS (
                    SELECT market, available_at_ms, LAG(available_at_ms) OVER (PARTITION BY market ORDER BY available_at_ms) AS prev_ts
                    FROM read_parquet({f_sql})
                    WHERE market IN ({mkts_sql}) AND tradable = true AND bid_px_f64 > 0 AND ask_px_f64 > bid_px_f64
                )
                SELECT market, COUNT(*), MIN(available_at_ms), MAX(available_at_ms), COALESCE(MAX(available_at_ms - prev_ts), 0)
                FROM s GROUP BY 1
                """
            ).fetchall()
        cont_by_sym: dict[str, dict[str, Any]] = {}
        for mkt, cnt, t0, t1, mgap in stats_rows:
            sym = mkt_to_sym.get(str(mkt))
            if sym and cnt and int(cnt) >= 2 and t0 is not None and t1 is not None:
                dur = int(t1 - t0)
                cont_by_sym[sym] = {
                    "raw_events": int(cnt),
                    "duration_ms": dur,
                    "hours": round(dur / 3_600_000.0, 3),
                    "max_gap_ms": int(mgap),
                    "is_continuous_10h": dur >= TEN_HOURS_MS and int(mgap) <= MAX_CONTINUOUS_GAP_MS,
                }
        rows = con.execute(
            f"""
            SELECT market,
                   MAX(available_at_ms) AS ts_ms,
                   arg_max(bid_px_f64, available_at_ms) AS bid_px,
                   arg_max(COALESCE(CAST(bid_sz AS DOUBLE), 0.0), available_at_ms) AS bid_sz,
                   arg_max(ask_px_f64, available_at_ms) AS ask_px,
                   arg_max(COALESCE(CAST(ask_sz AS DOUBLE), 0.0), available_at_ms) AS ask_sz
            FROM read_parquet({f_sql})
            WHERE market IN ({mkts_sql}) AND tradable = true AND bid_px_f64 > 0 AND ask_px_f64 > bid_px_f64
            GROUP BY market, (available_at_ms // {STEP_MS})
            ORDER BY market, ts_ms ASC
            """
        ).fetchall()
        grouped: dict[str, list[tuple[int, float, float, float, float]]] = defaultdict(list)
        for mkt, ts_ms, bp, bs, ap, aks in rows:
            sym = mkt_to_sym.get(str(mkt))
            if sym:
                grouped[sym].append((int(ts_ms), float(bp), float(bs), float(ap), float(aks)))
        for sym, lst in grouped.items():
            if sym in cont_by_sym:
                bbo_out[sym] = (lst, cont_by_sym[sym])

    if load_trades:
        t_files = _campaign_parquet_files(lakehouse_dir, "public_trades", venue, win_info["utc_date"], win_info["campaign_id"])
        if t_files:
            tf_sql = "[" + ", ".join("'" + p.replace("'", "''") + "'" for p in t_files) + "]"
            mkts_sql = ", ".join("'" + m.replace("'", "''") + "'" for m in mkt_to_sym)
            t_rows = con.execute(
                f"""
                SELECT market,
                       available_at_ms AS ts_ms,
                       CASE WHEN lower(aggressor_side) IN ('buy', 'b', 'bid') THEN 1
                            WHEN lower(aggressor_side) IN ('sell', 's', 'ask') THEN -1
                            ELSE 0 END AS sgn,
                       price_f64 AS px,
                       size_f64 AS sz
                FROM read_parquet({tf_sql})
                WHERE market IN ({mkts_sql}) AND NOT is_zero_price_transfer AND price_f64 > 0 AND size_f64 > 0
                ORDER BY market, available_at_ms ASC
                """
            ).fetchall()
            for mkt, ts_ms, sgn, px, sz in t_rows:
                if int(sgn) != 0:
                    sym = mkt_to_sym.get(str(mkt))
                    if sym:
                        tr_out[sym].append((int(ts_ms), int(sgn), float(px), float(sz)))
    return bbo_out, tr_out


def run_stage_a_diagnosis(
    lakehouse_dir: Path,
    *,
    domain_filter: str = "both",
    output_dir: Path | None = None,
) -> dict[str, Any]:
    """Execute Stage A Root-Cause Diagnosis & RWA Cross-Venue Discovery on `selection_dev (w01-w20)`."""
    import duckdb

    con = duckdb.connect(":memory:")
    con.execute("SET preserve_insertion_order=false; SET threads=2; SET memory_limit='2GB';")

    out: dict[str, Any] = {
        "schema": "track01_stage_a_diagnosis_v1",
        "fee_scenario": STANDARD_ZERO_FEE_PROFILES,
        "latency_grid_ms": list(LATENCY_GRID_MS),
    }

    if domain_filter in ("crypto", "both"):
        c_wins = split_campaign_windows(discover_campaigns(lakehouse_dir, "crypto"))["selection_dev"]
        c_pairs = build_crypto_pairs()
        c_venues_bbo = ("binance", "coinbase", "lighter", "lighter_robinhood")
        c_venues_tr = ("lighter", "lighter_robinhood")

        # Per (tgt_venue, delay_ms) -> list of sim results & trade details
        diag_trades: dict[tuple[str, float, int], list[dict[str, Any]]] = defaultdict(list)
        lat_acc: dict[tuple[str, float, int], list[dict[str, Any]]] = defaultdict(list)

        for win_id, win_info in sorted(c_wins.items()):
            win_bbo: dict[str, dict[str, tuple[list[Any], dict[str, Any]]]] = {}
            win_tr: dict[str, dict[str, list[Any]]] = {}
            for v in c_venues_bbo:
                bbo_d, tr_d = _load_window_venue_data(
                    con, lakehouse_dir, win_info, v, CRYPTO_SYMBOLS, load_trades=(v in c_venues_tr)
                )
                win_bbo[v] = bbo_d
                if v in c_venues_tr:
                    win_tr[v] = tr_d

            for p in c_pairs:
                ref_rows, ref_cont = win_bbo[p.ref_venue][p.symbol]
                tgt_rows, tgt_cont = win_bbo[p.tgt_venue][p.symbol]
                tr_rows = win_tr[p.tgt_venue][p.symbol]
                if not (ref_cont["is_continuous_10h"] and tgt_cont["is_continuous_10h"] and len(ref_rows) >= 200 and len(tgt_rows) >= 200):
                    continue
                sig = precompute_upgrade_signals(
                    ref_rows,
                    tgt_rows,
                    trades=tr_rows,
                    tau_values_ms=(120_000,),
                    warmup_ms=DEFAULT_WARMUP_MS,
                    min_cand_theta_bps=4.0,
                )
                if sig is None:
                    continue
                for theta_bps in (4.0, 6.0):
                    for delay_ms in LATENCY_GRID_MS:
                        if theta_bps == 4.0 and delay_ms != 230:
                            continue
                        cfg = LeadLagUpgradeConfig(
                            theta_bps=theta_bps,
                            tau_ms=120_000,
                            hold_ms=10_000,
                            taker_delay_ms=delay_ms,
                            warmup_ms=DEFAULT_WARMUP_MS,
                            taker_fee_bps=0.0,
                        )
                        collect = (delay_ms == 230)
                        res = simulate_precomputed_upgrade(sig, cfg, collect_trade_details=collect)
                        if collect:
                            diag_trades[(p.tgt_venue, theta_bps, delay_ms)].extend(res.pop("trade_details", []))
                        lat_acc[(p.tgt_venue, theta_bps, delay_ms)].append(
                            {
                                "win": win_id,
                                "pair_id": p.pair_id,
                                "symbol": p.symbol,
                                "ref_venue": p.ref_venue,
                                **res,
                            }
                        )
            del win_bbo, win_tr
            gc.collect()

        out["crypto_selection_dev"] = {
            "windows": c_wins,
            "latency_decay": {
                f"{v}|theta={th}|delay={d}ms": _agg_simple(rows)
                for (v, th, d), rows in sorted(lat_acc.items())
            },
            "attribution_230ms": {
                f"{v}|theta={th}": summarize_stage_a_trades(tr_list)
                for (v, th, d), tr_list in sorted(diag_trades.items())
                if d == 230
            },
        }

    if domain_filter in ("rwa", "both"):
        r_wins = split_campaign_windows(discover_campaigns(lakehouse_dir, "rwa"))["selection_dev"]
        r_pairs = build_rwa_pairs()
        r_venues = ("arcus", "lighter", "lighter_robinhood")

        rwa_grid_acc: dict[tuple[str, float, int, int], list[dict[str, Any]]] = defaultdict(list)
        rwa_lat_acc: dict[tuple[str, str, int], list[dict[str, Any]]] = defaultdict(list)
        rwa_diag_trades: dict[tuple[str, str], list[dict[str, Any]]] = defaultdict(list)

        for win_id, win_info in sorted(r_wins.items()):
            win_bbo: dict[str, dict[str, tuple[list[Any], dict[str, Any]]]] = {}
            win_tr: dict[str, dict[str, list[Any]]] = {}
            for v in r_venues:
                bbo_d, tr_d = _load_window_venue_data(
                    con, lakehouse_dir, win_info, v, RWA_SYMBOLS, load_trades=(v in ("lighter", "lighter_robinhood"))
                )
                win_bbo[v] = bbo_d
                if v in ("lighter", "lighter_robinhood"):
                    win_tr[v] = tr_d

            for p in r_pairs:
                ref_rows, ref_cont = win_bbo[p.ref_venue][p.symbol]
                tgt_rows, tgt_cont = win_bbo[p.tgt_venue][p.symbol]
                tr_rows = win_tr[p.tgt_venue][p.symbol]
                if not (ref_cont["is_continuous_10h"] and tgt_cont["is_continuous_10h"] and len(ref_rows) >= 200 and len(tgt_rows) >= 200):
                    continue
                sig = precompute_upgrade_signals(
                    ref_rows,
                    tgt_rows,
                    trades=tr_rows,
                    tau_values_ms=(30_000, 120_000),
                    warmup_ms=DEFAULT_WARMUP_MS,
                    min_cand_theta_bps=4.0,
                )
                if sig is None:
                    continue
                # Discovery grid at 230ms
                for theta_bps in (4.0, 6.0, 8.0):
                    for hold_ms in (3_000, 10_000):
                        for tau_ms in (30_000, 120_000):
                            cfg = LeadLagUpgradeConfig(
                                theta_bps=theta_bps,
                                tau_ms=tau_ms,
                                hold_ms=hold_ms,
                                taker_delay_ms=230,
                                warmup_ms=DEFAULT_WARMUP_MS,
                                taker_fee_bps=0.0,
                            )
                            res = simulate_precomputed_upgrade(sig, cfg)
                            rwa_grid_acc[(p.pair_id, theta_bps, hold_ms, tau_ms)].append({"win": win_id, **res})
                # Latency sweep on canonical baseline (theta=6.0, hold=10_000, tau=120_000)
                for delay_ms in LATENCY_GRID_MS:
                    cfg = LeadLagUpgradeConfig(
                        theta_bps=6.0,
                        tau_ms=120_000,
                        hold_ms=10_000,
                        taker_delay_ms=delay_ms,
                        warmup_ms=DEFAULT_WARMUP_MS,
                        taker_fee_bps=0.0,
                    )
                    collect = (delay_ms == 230)
                    res = simulate_precomputed_upgrade(sig, cfg, collect_trade_details=collect)
                    if collect:
                        cohort = f"{p.tgt_venue}<-{p.ref_venue}"
                        dts = res.pop("trade_details", [])
                        rwa_diag_trades[(cohort, p.symbol)].extend(dts)
                        rwa_diag_trades[(cohort, "ALL")].extend(dts)
                    rwa_lat_acc[(f"{p.tgt_venue}<-{p.ref_venue}", p.symbol, delay_ms)].append({"win": win_id, **res})
                    rwa_lat_acc[(f"{p.tgt_venue}<-{p.ref_venue}", "ALL", delay_ms)].append({"win": win_id, **res})
            del win_bbo, win_tr
            gc.collect()

        # Best baseline per RWA pair
        rwa_pair_best: dict[str, Any] = {}
        for p in r_pairs:
            best_row = None
            best_key = -1e18
            for theta_bps in (4.0, 6.0, 8.0):
                for hold_ms in (3_000, 10_000):
                    for tau_ms in (30_000, 120_000):
                        rows = rwa_grid_acc.get((p.pair_id, theta_bps, hold_ms, tau_ms), [])
                        if not rows:
                            continue
                        agg = _agg_simple(rows)
                        score = agg["pv_clip_1000_usd"] if agg["pv_trades"] > 0 else agg["quote_clip_1000_usd"]
                        if agg["trades"] >= 20 and score > best_key:
                            best_key = score
                            best_row = {"theta_bps": theta_bps, "hold_ms": hold_ms, "tau_ms": tau_ms, **agg}
            if best_row is not None:
                rwa_pair_best[p.pair_id] = best_row

        out["rwa_selection_dev"] = {
            "windows": r_wins,
            "pair_best_230ms": rwa_pair_best,
            "latency_decay_by_cohort_symbol": {
                f"{cohort}|{sym}|delay={d}ms": _agg_simple(rows)
                for (cohort, sym, d), rows in sorted(rwa_lat_acc.items())
            },
            "attribution_230ms": {
                f"{cohort}|{sym}": summarize_stage_a_trades(tr_list)
                for (cohort, sym), tr_list in sorted(rwa_diag_trades.items())
                if sym == "ALL"
            },
        }

    con.close()
    if output_dir is not None:
        output_dir.mkdir(parents=True, exist_ok=True)
        out_path = output_dir / "track01_stage_a_diagnosis.json"
        out_path.write_text(json.dumps(out, indent=2, allow_nan=False) + "\n", encoding="utf-8")
    return out


def _agg_simple(rows: Sequence[dict[str, Any]]) -> dict[str, Any]:
    trades = sum(int(r["trades"]) for r in rows)
    pv_trades = sum(int(r["print_verified_trades"]) for r in rows)
    q_bps = sum(float(r["quote_net_bps_total"]) for r in rows)
    pv_bps = sum(float(r["print_verified_net_bps_total"]) for r in rows)
    q500 = sum(float(r["quote_clip_pnl_usd"]["500"]) for r in rows)
    q1000 = sum(float(r["quote_clip_pnl_usd"]["1000"]) for r in rows)
    pv500 = sum(float(r["pv_clip_pnl_usd"]["500"]) for r in rows)
    pv1000 = sum(float(r["pv_clip_pnl_usd"]["1000"]) for r in rows)
    pos_q = sum(1 for r in rows if int(r["trades"]) > 0 and float(r["quote_clip_pnl_usd"]["1000"]) > 0.0)
    pos_pv = sum(1 for r in rows if int(r["print_verified_trades"]) > 0 and float(r["pv_clip_pnl_usd"]["1000"]) > 0.0)
    return {
        "pair_windows": len(rows),
        "trades": trades,
        "pv_trades": pv_trades,
        "quote_net_bps_mean": round(q_bps / trades, 4) if trades else 0.0,
        "pv_net_bps_mean": round(pv_bps / pv_trades, 4) if pv_trades else None,
        "quote_clip_500_usd": round(q500, 4),
        "quote_clip_1000_usd": round(q1000, 4),
        "pv_clip_500_usd": round(pv500, 4),
        "pv_clip_1000_usd": round(pv1000, 4),
        "pos_q_pair_wins": pos_q,
        "pos_pv_pair_wins": pos_pv,
    }


def run_self_test() -> dict[str, Any]:
    """Pure-stdlib synthetic self-test verifying all new microstructure features."""
    # 1. Consensus median test (zero stale poisoning)
    c1 = consensus_ref_mid(10_000, 5_000, [(100.10, 9_900), (100.30, 9_950), (100.20, 9_800)])
    assert c1 == (100.20, 9_950), f"bad 3-leader median: {c1}"
    c2 = consensus_ref_mid(10_000, 5_000, [(100.10, 9_900), (150.0, 2_000), (100.30, 9_800)])
    assert c2 is not None and math.isclose(c2[0], 100.20, abs_tol=1e-9), f"stale poisoned consensus: {c2}"

    # 2. Microprice + OBI confirmation
    cfg_obi = LeadLagUpgradeConfig(
        theta_bps=3.0,
        tau_ms=2_000,
        hold_ms=1_000,
        taker_delay_ms=200,
        warmup_ms=1_000,
        use_microprice_fair=True,
        ref_obi_confirm_min=0.30,
    )
    e = LeadLagTakerUpgradePy(cfg_obi)
    t = 10_000
    for _ in range(10):
        e.observe_l1_books(t, (t, 99.995, 10.0, 100.005, 10.0), None, (t, 99.995, 10.0, 100.005, 10.0))
        t += 250
    # Ref jumps up +5bp in mid, but OBI is strongly negative (bid_sz=1, ask_sz=9 -> obi=-0.8) -> suppressed!
    act_opp = e.observe_l1_books(t, (t, 100.045, 1.0, 100.055, 9.0), None, (t, 99.995, 10.0, 100.005, 10.0))
    assert act_opp[0] == "Idle", f"expected OBI suppression, got {act_opp}"
    t += 250
    # Ref jumps up +5bp AND OBI is strongly positive (bid_sz=9, ask_sz=1 -> obi=+0.8) -> Enter Long!
    act_ok = e.observe_l1_books(t, (t, 100.045, 9.0, 100.055, 1.0), None, (t, 99.995, 10.0, 100.005, 10.0))
    assert act_ok[0] == "Enter" and act_ok[1] == 1, f"expected Enter Long, got {act_ok}"

    # 3. Leader-reversal early stop
    cfg_rev = LeadLagUpgradeConfig(
        theta_bps=3.0,
        tau_ms=2_000,
        hold_ms=5_000,
        taker_delay_ms=200,
        warmup_ms=1_000,
        ref_reversal_exit_bps=3.0,
    )
    e2 = LeadLagTakerUpgradePy(cfg_rev)
    t = 10_000
    for _ in range(10):
        e2.observe_l1_books(t, (t, 99.995, 10.0, 100.005, 10.0), None, (t, 99.995, 10.0, 100.005, 10.0))
        t += 250
    ent = e2.observe_l1_books(t, (t, 100.045, 10.0, 100.055, 10.0), None, (t, 99.995, 10.0, 100.005, 10.0))
    assert ent[0] == "Enter"
    # Snap back -4bp at t+250 (>= entry_filled_est_ms t+200, < exit_due t+5200) -> Exit with reason ref_reversal!
    ex = e2.observe_l1_books(t + 250, (t + 250, 100.005, 10.0, 100.015, 10.0), None, (t + 250, 99.995, 10.0, 100.005, 10.0))
    assert ex[0] == "Exit" and ex[3] == "ref_reversal", f"expected ref_reversal exit, got {ex}"
    assert e2.pos is not None, "position must remain retained until notify_exit_filled()"
    e2.notify_exit_filled()
    assert e2.pos is None

    # 4. Spread gate
    cfg_spr = LeadLagUpgradeConfig(
        theta_bps=3.0,
        tau_ms=2_000,
        hold_ms=1_000,
        taker_delay_ms=200,
        warmup_ms=1_000,
        max_entry_tgt_spread_bps=2.0,
        min_edge_over_spread_bps=3.0,
    )
    e3 = LeadLagTakerUpgradePy(cfg_spr)
    t = 10_000
    for _ in range(10):
        e3.observe_l1_books(t, (t, 99.995, 10.0, 100.005, 10.0), None, (t, 99.995, 10.0, 100.005, 10.0))
        t += 250
    # Wide target spread (4bp) -> blocked (Idle)
    wide = e3.observe_l1_books(t, (t, 100.045, 10.0, 100.055, 10.0), None, (t, 99.98, 10.0, 100.02, 10.0))
    assert wide[0] == "Idle", f"expected wide spread suppression, got {wide}"

    return {"self_test": "ok", "consensus_median": c1[0], "obi_gated": True, "ref_reversal_exit": True, "spread_gated": True}


STAGE_B_ARM_NAMES: tuple[str, ...] = (
    "A_baseline",
    "B_vol_adaptive",
    "M1_microprice_obi",
    "M2_consensus_leader",
    "M3_ref_reversal_spread_gate",
    "M4_combined",
)

STAGE_B_THETA_GRID: tuple[float, ...] = (4.0, 6.0, 8.0)
STAGE_B_HOLD_GRID: tuple[int, ...] = (3_000, 10_000)
STAGE_B_TAU_GRID: tuple[int, ...] = (30_000, 120_000)
STAGE_B_VOL_MULT_GRID: tuple[float, ...] = (1.0, 1.5, 2.0)
STAGE_B_OBI_MIN_GRID: tuple[float, ...] = (0.15, 0.25, 0.35)
STAGE_B_SPREAD_GATE_GRID: tuple[tuple[float, float], ...] = (
    (1.5, 3.0),
    (2.5, 4.0),
    (4.0, 5.0),
)
STAGE_B_REV_EXIT_GRID: tuple[float, ...] = (2.5, 3.5, 5.0)


def _score_candidate(stats: dict[str, Any]) -> tuple[int, float]:
    """Rank candidates: jointly positive (quote & PV) first, then combined USD PnL."""
    q1k = float(stats["q1000"])
    pv1k = float(stats["pv1000"])
    pv_tr = int(stats["pv_trades"])
    joint_pos = 1 if (q1k > 0.0 and (pv_tr == 0 or pv1k > 0.0)) else 0
    primary = (pv1k if pv_tr > 0 else q1k) if joint_pos else (q1k + pv1k)
    return (joint_pos, primary)


def _arm_configs_stage_b(
    pair: LeadLagPair,
    frozen: dict[str, Any],
    *,
    taker_delay_ms: int,
    warmup_ms: int,
) -> dict[str, LeadLagUpgradeConfig]:
    common = dict(
        theta_bps=float(frozen["theta_bps"]),
        tau_ms=int(frozen["tau_ms"]),
        hold_ms=int(frozen["hold_ms"]),
        taker_delay_ms=int(taker_delay_ms),
        warmup_ms=warmup_ms,
        taker_fee_bps=0.0,
    )
    vol_mult = float(frozen["vol_theta_mult"])
    obi_min = float(frozen["ref_obi_confirm_min"])
    max_spr = float(frozen["max_entry_tgt_spread_bps"])
    min_edge = float(frozen["min_edge_over_spread_bps"])
    rev_bps = float(frozen["ref_reversal_exit_bps"])
    m4_mode = str(frozen.get("m4_mode", "vol_m1_m3"))

    m4_use_vol = vol_mult if "vol" in m4_mode else 0.0
    m4_use_cons = "cons" in m4_mode

    return {
        "A_baseline": LeadLagUpgradeConfig(**common),
        "B_vol_adaptive": LeadLagUpgradeConfig(**common, vol_theta_mult=vol_mult),
        "M1_microprice_obi": LeadLagUpgradeConfig(
            **common,
            use_microprice_fair=True,
            ref_obi_confirm_min=obi_min,
        ),
        "M2_consensus_leader": LeadLagUpgradeConfig(
            **common,
            use_consensus_leader=True,
        ),
        "M3_ref_reversal_spread_gate": LeadLagUpgradeConfig(
            **common,
            max_entry_tgt_spread_bps=max_spr,
            min_edge_over_spread_bps=min_edge,
            ref_reversal_exit_bps=rev_bps,
        ),
        "M4_combined": LeadLagUpgradeConfig(
            **common,
            vol_theta_mult=m4_use_vol,
            use_microprice_fair=True,
            ref_obi_confirm_min=obi_min,
            use_consensus_leader=m4_use_cons,
            max_entry_tgt_spread_bps=max_spr,
            min_edge_over_spread_bps=min_edge,
            ref_reversal_exit_bps=rev_bps,
        ),
    }


def _aggregate_stage_b_rows(rows: Sequence[dict[str, Any]]) -> dict[str, Any]:
    """Aggregate Stage B evaluation rows by arm|venue|delay, arm|domain|delay, and arm|pair|delay."""
    by_venue: dict[tuple[str, str, str, int], list[dict[str, Any]]] = defaultdict(list)
    by_domain: dict[tuple[str, str, int], list[dict[str, Any]]] = defaultdict(list)
    by_pair: dict[tuple[str, str, int], list[dict[str, Any]]] = defaultdict(list)

    for r in rows:
        if r.get("skipped") or not r.get("cont"):
            continue
        arm = r["arm"]
        dom = r["domain"]
        ven = r["tgt_venue"]
        pid = r["pair_id"]
        dly = int(r["taker_delay_ms"])
        by_venue[(arm, dom, ven, dly)].append(r)
        by_domain[(arm, dom, dly)].append(r)
        by_pair[(arm, pid, dly)].append(r)

    def _agg_grp(grp: Sequence[dict[str, Any]], *, include_curve: bool = False) -> dict[str, Any]:
        trades = sum(int(x["trades"]) for x in grp)
        pv_tr = sum(int(x["print_verified_trades"]) for x in grp)
        q_bps = sum(float(x["quote_net_bps_total"]) for x in grp)
        pv_bps = sum(float(x["print_verified_net_bps_total"]) for x in grp)
        q500 = sum(float(x["quote_clip_pnl_usd"]["500"]) for x in grp)
        q1000 = sum(float(x["quote_clip_pnl_usd"]["1000"]) for x in grp)
        pv500 = sum(float(x["pv_clip_pnl_usd"]["500"]) for x in grp)
        pv1000 = sum(float(x["pv_clip_pnl_usd"]["1000"]) for x in grp)
        out: dict[str, Any] = {
            "pair_windows": len(grp),
            "trades": trades,
            "pv_trades": pv_tr,
            "pv_coverage": round(pv_tr / trades, 3) if trades else 0.0,
            "quote_net_bps_mean": round(q_bps / trades, 4) if trades else 0.0,
            "pv_net_bps_mean": round(pv_bps / pv_tr, 4) if pv_tr else None,
            "quote_clip_500_usd": round(q500, 4),
            "quote_clip_1000_usd": round(q1000, 4),
            "pv_clip_500_usd": round(pv500, 4),
            "pv_clip_1000_usd": round(pv1000, 4),
            "early_ref_reversal_exits": sum(int(x.get("early_ref_reversal_exits", 0)) for x in grp),
        }
        if include_curve:
            by_win: dict[str, list[dict[str, Any]]] = defaultdict(list)
            for x in grp:
                by_win[x["win"]].append(x)
            cum_q = peak_q = dd_q = 0.0
            cum_pv = peak_pv = dd_pv = 0.0
            pos_q_w = pos_pv_w = pv_active_w = 0
            for _, wrs in sorted(by_win.items()):
                wq = sum(float(x["quote_clip_pnl_usd"]["1000"]) for x in wrs)
                wpv = sum(float(x["pv_clip_pnl_usd"]["1000"]) for x in wrs)
                wtr = sum(int(x["trades"]) for x in wrs)
                wpv_tr = sum(int(x["print_verified_trades"]) for x in wrs)
                cum_q += wq
                peak_q = max(peak_q, cum_q)
                dd_q = max(dd_q, peak_q - cum_q)
                cum_pv += wpv
                peak_pv = max(peak_pv, cum_pv)
                dd_pv = max(dd_pv, peak_pv - cum_pv)
                if wtr > 0 and wq > 0.0:
                    pos_q_w += 1
                if wpv_tr > 0:
                    pv_active_w += 1
                    if wpv > 0.0:
                        pos_pv_w += 1
            out["unique_windows"] = len(by_win)
            out["positive_quote_win_ratio"] = round(pos_q_w / len(by_win), 4) if by_win else 0.0
            out["positive_pv_win_ratio"] = round(pos_pv_w / pv_active_w, 4) if pv_active_w else None
            out["max_dd_quote_1000_usd"] = round(dd_q, 4)
            out["max_dd_pv_1000_usd"] = round(dd_pv, 4)
        return out

    return {
        "by_arm_domain_venue_delay": {
            f"{arm}|{dom}|{ven}|{dly}ms": _agg_grp(grp, include_curve=True)
            for (arm, dom, ven, dly), grp in sorted(by_venue.items())
        },
        "by_arm_domain_delay": {
            f"{arm}|{dom}|{dly}ms": _agg_grp(grp, include_curve=True)
            for (arm, dom, dly), grp in sorted(by_domain.items())
        },
        "by_arm_pair_delay": {
            f"{arm}|{pid}|{dly}ms": _agg_grp(grp, include_curve=False)
            for (arm, pid, dly), grp in sorted(by_pair.items())
        },
    }


def _load_dom_window_worker(
    lakehouse_dir: Path,
    win_info: dict[str, str],
    bbo_venues: Sequence[str],
    tr_venues: Sequence[str],
    symbols: Sequence[str],
) -> tuple[dict[str, Any], dict[str, Any]]:
    import duckdb

    con = duckdb.connect(":memory:")
    con.execute("SET preserve_insertion_order=false; SET threads=1; SET memory_limit='1GB';")
    wb: dict[str, Any] = {}
    wt: dict[str, Any] = {}
    for v in bbo_venues:
        bd, td = _load_window_venue_data(con, lakehouse_dir, win_info, v, symbols, load_trades=(v in tr_venues))
        wb[v] = bd
        if v in tr_venues:
            wt[v] = td
    con.close()
    return wb, wt


def _sb_pass1_window(task: tuple[Any, ...]) -> tuple[dict[str, dict[tuple[float, int, int], dict[str, float]]], dict[str, int]]:
    lakehouse_dir, win_info, pairs, bbo_venues, tr_venues, symbols, warmup_ms = task
    wb, wt = _load_dom_window_worker(lakehouse_dir, win_info, bbo_venues, tr_venues, symbols)
    acc: dict[str, dict[tuple[float, int, int], dict[str, float]]] = {}
    cnts: dict[str, int] = {}
    for p in pairs:
        ref_rows, ref_cont = wb[p.ref_venue][p.symbol]
        tgt_rows, tgt_cont = wb[p.tgt_venue][p.symbol]
        tr_rows = wt[p.tgt_venue][p.symbol]
        if not (ref_cont["is_continuous_10h"] and tgt_cont["is_continuous_10h"] and len(ref_rows) >= 200 and len(tgt_rows) >= 200):
            continue
        sig = precompute_upgrade_signals(
            ref_rows, tgt_rows, trades=tr_rows, tau_values_ms=STAGE_B_TAU_GRID, warmup_ms=warmup_ms, min_cand_theta_bps=4.0
        )
        if sig is None:
            continue
        cnts[p.pair_id] = 1
        p_acc: dict[tuple[float, int, int], dict[str, float]] = {}
        for tau_ms in STAGE_B_TAU_GRID:
            for hold_ms in STAGE_B_HOLD_GRID:
                for th in STAGE_B_THETA_GRID:
                    cfg = LeadLagUpgradeConfig(theta_bps=th, tau_ms=tau_ms, hold_ms=hold_ms, taker_delay_ms=230, warmup_ms=warmup_ms)
                    r = simulate_precomputed_upgrade(sig, cfg)
                    p_acc[(th, hold_ms, tau_ms)] = {
                        "trades": float(r["trades"]),
                        "pv_trades": float(r["print_verified_trades"]),
                        "q1000": float(r["quote_clip_pnl_usd"]["1000"]),
                        "pv1000": float(r["pv_clip_pnl_usd"]["1000"]),
                    }
        acc[p.pair_id] = p_acc
    del wb, wt
    gc.collect()
    return acc, cnts


def _sb_pass2_window(task: tuple[Any, ...]) -> tuple[dict[str, Any], dict[str, Any], dict[str, Any]]:
    lakehouse_dir, win_info, pairs, bbo_venues, tr_venues, symbols, chosen_base, warmup_ms = task
    wb, wt = _load_dom_window_worker(lakehouse_dir, win_info, bbo_venues, tr_venues, symbols)
    v_acc: dict[str, dict[float, dict[str, float]]] = {}
    o_acc: dict[str, dict[float, dict[str, float]]] = {}
    m_acc: dict[str, dict[tuple[float, float, float], dict[str, float]]] = {}
    for p in pairs:
        ref_rows, ref_cont = wb[p.ref_venue][p.symbol]
        tgt_rows, tgt_cont = wb[p.tgt_venue][p.symbol]
        tr_rows = wt[p.tgt_venue][p.symbol]
        if not (ref_cont["is_continuous_10h"] and tgt_cont["is_continuous_10h"] and len(ref_rows) >= 200 and len(tgt_rows) >= 200):
            continue
        b_th, b_h, b_tau, _ = chosen_base[p.pair_id]
        sig_mid = precompute_upgrade_signals(
            ref_rows, tgt_rows, trades=tr_rows, use_microprice_fair=False, tau_values_ms=(b_tau,), warmup_ms=warmup_ms, min_cand_theta_bps=b_th
        )
        sig_mic = precompute_upgrade_signals(
            ref_rows, tgt_rows, trades=tr_rows, use_microprice_fair=True, tau_values_ms=(b_tau,), warmup_ms=warmup_ms, min_cand_theta_bps=b_th
        )
        if sig_mid is None or sig_mic is None:
            continue
        v_acc[p.pair_id] = {}
        for mult in STAGE_B_VOL_MULT_GRID:
            r = simulate_precomputed_upgrade(
                sig_mid, LeadLagUpgradeConfig(theta_bps=b_th, tau_ms=b_tau, hold_ms=b_h, taker_delay_ms=230, warmup_ms=warmup_ms, vol_theta_mult=mult)
            )
            v_acc[p.pair_id][mult] = {"trades": float(r["trades"]), "pv_trades": float(r["print_verified_trades"]), "q1000": float(r["quote_clip_pnl_usd"]["1000"]), "pv1000": float(r["pv_clip_pnl_usd"]["1000"])}
        o_acc[p.pair_id] = {}
        for obi_min in STAGE_B_OBI_MIN_GRID:
            r = simulate_precomputed_upgrade(
                sig_mic, LeadLagUpgradeConfig(theta_bps=b_th, tau_ms=b_tau, hold_ms=b_h, taker_delay_ms=230, warmup_ms=warmup_ms, use_microprice_fair=True, ref_obi_confirm_min=obi_min)
            )
            o_acc[p.pair_id][obi_min] = {"trades": float(r["trades"]), "pv_trades": float(r["print_verified_trades"]), "q1000": float(r["quote_clip_pnl_usd"]["1000"]), "pv1000": float(r["pv_clip_pnl_usd"]["1000"])}
        m_acc[p.pair_id] = {}
        for spr, edge in STAGE_B_SPREAD_GATE_GRID:
            for rev in STAGE_B_REV_EXIT_GRID:
                r = simulate_precomputed_upgrade(
                    sig_mid, LeadLagUpgradeConfig(theta_bps=b_th, tau_ms=b_tau, hold_ms=b_h, taker_delay_ms=230, warmup_ms=warmup_ms, max_entry_tgt_spread_bps=spr, min_edge_over_spread_bps=edge, ref_reversal_exit_bps=rev)
                )
                m_acc[p.pair_id][(spr, edge, rev)] = {"trades": float(r["trades"]), "pv_trades": float(r["print_verified_trades"]), "q1000": float(r["quote_clip_pnl_usd"]["1000"]), "pv1000": float(r["pv_clip_pnl_usd"]["1000"])}
    del wb, wt
    gc.collect()
    return v_acc, o_acc, m_acc


def _sb_pass3_window(task: tuple[Any, ...]) -> dict[str, dict[str, dict[str, float]]]:
    lakehouse_dir, win_info, pairs, bbo_venues, tr_venues, symbols, chosen_base, chosen_sub, m4_modes, warmup_ms = task
    wb, wt = _load_dom_window_worker(lakehouse_dir, win_info, bbo_venues, tr_venues, symbols)
    out: dict[str, dict[str, dict[str, float]]] = {}
    for p in pairs:
        ref_rows, ref_cont = wb[p.ref_venue][p.symbol]
        tgt_rows, tgt_cont = wb[p.tgt_venue][p.symbol]
        conf_rows = wb[p.confirm_venue][p.symbol][0] if p.confirm_venue else None
        tert_rows = wb[p.tertiary_venue][p.symbol][0] if p.tertiary_venue else None
        tr_rows = wt[p.tgt_venue][p.symbol]
        if not (ref_cont["is_continuous_10h"] and tgt_cont["is_continuous_10h"] and len(ref_rows) >= 200 and len(tgt_rows) >= 200):
            continue
        b_th, b_h, b_tau, _ = chosen_base[p.pair_id]
        b_vol, b_obi, b_spr, b_edge, b_rev = chosen_sub[p.pair_id]
        sig_mic = precompute_upgrade_signals(
            ref_rows, tgt_rows, trades=tr_rows, use_microprice_fair=True, use_consensus_leader=False, tau_values_ms=(b_tau,), warmup_ms=warmup_ms, min_cand_theta_bps=b_th
        )
        sig_cons_mic = precompute_upgrade_signals(
            ref_rows, tgt_rows, confirm_books=conf_rows, tertiary_books=tert_rows, trades=tr_rows, use_microprice_fair=True, use_consensus_leader=True, tau_values_ms=(b_tau,), warmup_ms=warmup_ms, min_cand_theta_bps=b_th
        )
        if sig_mic is None or sig_cons_mic is None:
            continue
        out[p.pair_id] = {}
        for mode in m4_modes:
            use_vol = b_vol if "vol" in mode else 0.0
            use_cons = "cons" in mode
            sig_use = sig_cons_mic if use_cons else sig_mic
            cfg = LeadLagUpgradeConfig(
                theta_bps=b_th, tau_ms=b_tau, hold_ms=b_h, taker_delay_ms=230, warmup_ms=warmup_ms,
                vol_theta_mult=use_vol, use_microprice_fair=True, ref_obi_confirm_min=b_obi,
                use_consensus_leader=use_cons, max_entry_tgt_spread_bps=b_spr,
                min_edge_over_spread_bps=b_edge, ref_reversal_exit_bps=b_rev,
            )
            r = simulate_precomputed_upgrade(sig_use, cfg)
            out[p.pair_id][mode] = {"trades": float(r["trades"]), "pv_trades": float(r["print_verified_trades"]), "q1000": float(r["quote_clip_pnl_usd"]["1000"]), "pv1000": float(r["pv_clip_pnl_usd"]["1000"])}
    del wb, wt
    gc.collect()
    return out


def _sb_eval_window(task: tuple[Any, ...]) -> list[dict[str, Any]]:
    lakehouse_dir, split_name, dom, win_id, win_info, pairs, bbo_venues, tr_venues, symbols, all_frozen, warmup_ms = task
    wb, wt = _load_dom_window_worker(lakehouse_dir, win_info, bbo_venues, tr_venues, symbols)
    rows: list[dict[str, Any]] = []
    for p in pairs:
        ref_rows, ref_cont = wb[p.ref_venue][p.symbol]
        tgt_rows, tgt_cont = wb[p.tgt_venue][p.symbol]
        conf_rows = wb[p.confirm_venue][p.symbol][0] if p.confirm_venue else None
        tert_rows = wb[p.tertiary_venue][p.symbol][0] if p.tertiary_venue else None
        tr_rows = wt[p.tgt_venue][p.symbol]
        cont_ok = bool(ref_cont["is_continuous_10h"] and tgt_cont["is_continuous_10h"] and len(ref_rows) >= 200 and len(tgt_rows) >= 200)
        if not cont_ok:
            continue
        fr = all_frozen[p.pair_id]
        b_tau = int(fr["tau_ms"])
        b_th = float(fr["theta_bps"])
        sig_mid = precompute_upgrade_signals(ref_rows, tgt_rows, trades=tr_rows, use_microprice_fair=False, use_consensus_leader=False, tau_values_ms=(b_tau,), warmup_ms=warmup_ms, min_cand_theta_bps=b_th)
        sig_mic = precompute_upgrade_signals(ref_rows, tgt_rows, trades=tr_rows, use_microprice_fair=True, use_consensus_leader=False, tau_values_ms=(b_tau,), warmup_ms=warmup_ms, min_cand_theta_bps=b_th)
        sig_cons = precompute_upgrade_signals(ref_rows, tgt_rows, confirm_books=conf_rows, tertiary_books=tert_rows, trades=tr_rows, use_microprice_fair=False, use_consensus_leader=True, tau_values_ms=(b_tau,), warmup_ms=warmup_ms, min_cand_theta_bps=b_th)
        need_cons_mic = "cons" in str(fr["m4_mode"])
        sig_cons_mic = (
            precompute_upgrade_signals(ref_rows, tgt_rows, confirm_books=conf_rows, tertiary_books=tert_rows, trades=tr_rows, use_microprice_fair=True, use_consensus_leader=True, tau_values_ms=(b_tau,), warmup_ms=warmup_ms, min_cand_theta_bps=b_th)
            if need_cons_mic else None
        )
        if sig_mid is None or sig_mic is None or sig_cons is None:
            continue
        for dly in LATENCY_GRID_MS:
            arm_cfgs = _arm_configs_stage_b(p, fr, taker_delay_ms=dly, warmup_ms=warmup_ms)
            for arm_name, cfg in arm_cfgs.items():
                if arm_name == "M1_microprice_obi":
                    sig_use = sig_mic
                elif arm_name == "M2_consensus_leader":
                    sig_use = sig_cons
                elif arm_name == "M4_combined":
                    sig_use = sig_cons_mic if (need_cons_mic and sig_cons_mic is not None) else sig_mic
                else:
                    sig_use = sig_mid
                sim_res = simulate_precomputed_upgrade(sig_use, cfg)
                rows.append(
                    {
                        "split": split_name,
                        "domain": dom,
                        "win": win_id,
                        "campaign_id": win_info["campaign_id"],
                        "arm": arm_name,
                        "pair_id": p.pair_id,
                        "tgt_venue": p.tgt_venue,
                        "symbol": p.symbol,
                        "ref_venue": p.ref_venue,
                        "taker_delay_ms": dly,
                        "cont": True,
                        **sim_res,
                    }
                )
    del wb, wt
    gc.collect()
    return rows


def _map_tasks(fn: Any, tasks: Sequence[tuple[Any, ...]], max_workers: int) -> list[Any]:
    if max_workers <= 1 or len(tasks) <= 1:
        return [fn(t) for t in tasks]
    with concurrent.futures.ProcessPoolExecutor(max_workers=min(max_workers, len(tasks))) as ex:
        return list(ex.map(fn, tasks))


def run_stage_b_upgrade(
    lakehouse_dir: Path,
    *,
    domain_filter: str = "both",
    output_dir: Path | None = None,
    warmup_ms: int = DEFAULT_WARMUP_MS,
    max_workers: int = 1,
) -> dict[str, Any]:
    """Run Round-2 iterative ablation on `selection_dev`, freeze, and validate ONCE on `holdout` + `w26_oos`."""
    domains: list[str] = []
    if domain_filter in ("crypto", "both"):
        domains.append("crypto")
    if domain_filter in ("rwa", "both"):
        domains.append("rwa")

    out: dict[str, Any] = {
        "schema": "track01_stage_b_upgrade_v1",
        "protocol": (
            "Select & freeze all parameters strictly on selection_dev (w01-w20) via Round-2 ablation; "
            "replay ONCE untouched on holdout (w21-w25) and w26_oos across taker_delay_ms in {230,300,400,500}."
        ),
        "fee_scenario": STANDARD_ZERO_FEE_PROFILES,
        "latency_grid_ms": list(LATENCY_GRID_MS),
        "arms": list(STAGE_B_ARM_NAMES),
        "frozen_params": {},
        "selection_diagnostics": {},
        "splits": {},
    }

    all_frozen: dict[str, dict[str, Any]] = {}
    all_sel_diags: dict[str, Any] = {}

    for dom in domains:
        pairs = build_crypto_pairs() if dom == "crypto" else build_rwa_pairs()
        symbols = CRYPTO_SYMBOLS if dom == "crypto" else RWA_SYMBOLS
        bbo_venues = (
            ("binance", "coinbase", "extended", "lighter", "lighter_robinhood")
            if dom == "crypto"
            else ("arcus", "lighter", "lighter_robinhood")
        )
        tr_venues = ("lighter", "lighter_robinhood")
        splits_map = split_campaign_windows(discover_campaigns(lakehouse_dir, dom))
        dev_wins = splits_map["selection_dev"]

        # Pass 1 on selection_dev
        base_acc: dict[str, dict[tuple[float, int, int], dict[str, float]]] = {
            p.pair_id: {
                (th, h, t): {"trades": 0.0, "pv_trades": 0.0, "q1000": 0.0, "pv1000": 0.0}
                for t in STAGE_B_TAU_GRID
                for h in STAGE_B_HOLD_GRID
                for th in STAGE_B_THETA_GRID
            }
            for p in pairs
        }
        cont_counts: dict[str, int] = {p.pair_id: 0 for p in pairs}
        p1_tasks = [(lakehouse_dir, wi, pairs, bbo_venues, tr_venues, symbols, warmup_ms) for _, wi in sorted(dev_wins.items())]
        for acc_w, cnt_w in _map_tasks(_sb_pass1_window, p1_tasks, max_workers):
            for pid, c_val in cnt_w.items():
                cont_counts[pid] += c_val
            for pid, mp in acc_w.items():
                for k, vals in mp.items():
                    for fld, fv in vals.items():
                        base_acc[pid][k][fld] += fv

        chosen_base: dict[str, tuple[float, int, int, str]] = {}
        for p in pairs:
            min_tr = min(40, max(10, 4 * cont_counts[p.pair_id]))
            elig = [(k, v) for k, v in base_acc[p.pair_id].items() if v["trades"] >= min_tr]
            joint = [(k, v) for k, v in elig if _score_candidate(v)[0] == 1]
            if joint:
                bk, _ = max(joint, key=lambda kv: _score_candidate(kv[1]))
                chosen_base[p.pair_id] = (bk[0], bk[1], bk[2], "selected_joint_positive")
            else:
                chosen_base[p.pair_id] = (6.0, 10_000, 120_000, "fallback_default_6bp_10s")

        # Pass 2 on selection_dev
        vol_acc: dict[str, dict[float, dict[str, float]]] = {
            p.pair_id: {m: {"trades": 0.0, "pv_trades": 0.0, "q1000": 0.0, "pv1000": 0.0} for m in STAGE_B_VOL_MULT_GRID}
            for p in pairs
        }
        obi_acc: dict[str, dict[float, dict[str, float]]] = {
            p.pair_id: {o: {"trades": 0.0, "pv_trades": 0.0, "q1000": 0.0, "pv1000": 0.0} for o in STAGE_B_OBI_MIN_GRID}
            for p in pairs
        }
        m3_acc: dict[str, dict[tuple[float, float, float], dict[str, float]]] = {
            p.pair_id: {
                (spr, edge, rev): {"trades": 0.0, "pv_trades": 0.0, "q1000": 0.0, "pv1000": 0.0}
                for spr, edge in STAGE_B_SPREAD_GATE_GRID
                for rev in STAGE_B_REV_EXIT_GRID
            }
            for p in pairs
        }
        p2_tasks = [(lakehouse_dir, wi, pairs, bbo_venues, tr_venues, symbols, chosen_base, warmup_ms) for _, wi in sorted(dev_wins.items())]
        for vw, ow, mw in _map_tasks(_sb_pass2_window, p2_tasks, max_workers):
            for pid, mp in vw.items():
                for k, vals in mp.items():
                    for fld, fv in vals.items():
                        vol_acc[pid][k][fld] += fv
            for pid, mp in ow.items():
                for k, vals in mp.items():
                    for fld, fv in vals.items():
                        obi_acc[pid][k][fld] += fv
            for pid, mp in mw.items():
                for k, vals in mp.items():
                    for fld, fv in vals.items():
                        m3_acc[pid][k][fld] += fv

        chosen_sub: dict[str, tuple[float, float, float, float, float]] = {}
        for p in pairs:
            min_sub = max(10, 2 * cont_counts[p.pair_id])
            v_elig = [(m, a) for m, a in vol_acc[p.pair_id].items() if a["trades"] >= min_sub]
            best_vol = max(v_elig, key=lambda kv: _score_candidate(kv[1]))[0] if v_elig else 1.5
            o_elig = [(o, a) for o, a in obi_acc[p.pair_id].items() if a["trades"] >= min_sub]
            best_obi = max(o_elig, key=lambda kv: _score_candidate(kv[1]))[0] if o_elig else 0.25
            m3_elig = [(k, a) for k, a in m3_acc[p.pair_id].items() if a["trades"] >= min_sub]
            best_m3 = max(m3_elig, key=lambda kv: _score_candidate(kv[1]))[0] if m3_elig else (2.5, 4.0, 3.5)
            chosen_sub[p.pair_id] = (float(best_vol), float(best_obi), float(best_m3[0]), float(best_m3[1]), float(best_m3[2]))

        # Pass 3 on selection_dev
        m4_modes = ("m1_m3", "vol_m1_m3", "vol_m1_cons_m3")
        m4_acc: dict[str, dict[str, dict[str, float]]] = {
            p.pair_id: {m: {"trades": 0.0, "pv_trades": 0.0, "q1000": 0.0, "pv1000": 0.0} for m in m4_modes}
            for p in pairs
        }
        p3_tasks = [(lakehouse_dir, wi, pairs, bbo_venues, tr_venues, symbols, chosen_base, chosen_sub, m4_modes, warmup_ms) for _, wi in sorted(dev_wins.items())]
        for m4w in _map_tasks(_sb_pass3_window, p3_tasks, max_workers):
            for pid, mp in m4w.items():
                for mode, vals in mp.items():
                    for fld, fv in vals.items():
                        m4_acc[pid][mode][fld] += fv

        for p in pairs:
            b_th, b_h, b_tau, b_stat = chosen_base[p.pair_id]
            b_vol, b_obi, b_spr, b_edge, b_rev = chosen_sub[p.pair_id]
            min_m4 = max(8, cont_counts[p.pair_id])
            m4_elig = [(m, a) for m, a in m4_acc[p.pair_id].items() if a["trades"] >= min_m4]
            best_m4 = max(m4_elig, key=lambda kv: _score_candidate(kv[1]))[0] if m4_elig else "vol_m1_m3"
            all_frozen[p.pair_id] = {
                "pair_id": p.pair_id,
                "domain": dom,
                "selection_status": b_stat,
                "theta_bps": b_th,
                "hold_ms": b_h,
                "tau_ms": b_tau,
                "vol_theta_mult": b_vol,
                "ref_obi_confirm_min": b_obi,
                "max_entry_tgt_spread_bps": b_spr,
                "min_edge_over_spread_bps": b_edge,
                "ref_reversal_exit_bps": b_rev,
                "m4_mode": best_m4,
                "variants_tested_on_selection": (
                    len(STAGE_B_THETA_GRID) * len(STAGE_B_HOLD_GRID) * len(STAGE_B_TAU_GRID)
                    + len(STAGE_B_VOL_MULT_GRID)
                    + len(STAGE_B_OBI_MIN_GRID)
                    + len(STAGE_B_SPREAD_GATE_GRID) * len(STAGE_B_REV_EXIT_GRID)
                    + len(m4_modes)
                ),
            }
            all_sel_diags[p.pair_id] = {
                "cont_windows_in_dev": cont_counts[p.pair_id],
                "m4_candidates": m4_acc[p.pair_id],
            }

        # Evaluate frozen arms across all 3 splits and all 4 latencies
        for split_name in ("selection_dev", "holdout", "w26_oos"):
            win_map = splits_map[split_name]
            eval_tasks = [
                (lakehouse_dir, split_name, dom, wid, wi, pairs, bbo_venues, tr_venues, symbols, all_frozen, warmup_ms)
                for wid, wi in sorted(win_map.items())
            ]
            split_rows: list[dict[str, Any]] = []
            for w_rows in _map_tasks(_sb_eval_window, eval_tasks, max_workers):
                split_rows.extend(w_rows)
            split_bucket = out["splits"].setdefault(split_name, {"windows": {}, "rows": []})
            split_bucket["windows"][dom] = win_map
            split_bucket["rows"].extend(split_rows)

    for split_name, sb in out["splits"].items():
        sb["summary"] = _aggregate_stage_b_rows(sb["rows"])

    out["frozen_params"] = all_frozen
    out["selection_diagnostics"] = all_sel_diags

    if output_dir is not None:
        output_dir.mkdir(parents=True, exist_ok=True)
        out_path = output_dir / "track01_stage_b_upgrade.json"
        out_path.write_text(json.dumps(out, indent=2, allow_nan=False) + "\n", encoding="utf-8")
    return out


def main(argv: Sequence[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description="Track 01 Microstructure & RWA Alpha Upgrade Evaluator")
    ap.add_argument("--lakehouse", type=Path, default=Path(os.path.expanduser("~/aleph-tx-lakehouse")))
    ap.add_argument("--mode", choices=("stage_a", "stage_b", "all", "self_test"), default="self_test")
    ap.add_argument("--domain", choices=("crypto", "rwa", "both"), default="both")
    ap.add_argument("--max-workers", type=int, default=1)
    ap.add_argument("--output-dir", type=Path, default=Path("/tmp/swarm_t1_upgrade"))
    ap.add_argument("--self-test", action="store_true")
    args = ap.parse_args(argv)

    if args.self_test or args.mode == "self_test":
        print(json.dumps(run_self_test(), indent=2, allow_nan=False))
        return 0

    summary: dict[str, Any] = {}
    if args.mode in ("stage_a", "all"):
        res_a = run_stage_a_diagnosis(args.lakehouse, domain_filter=args.domain, output_dir=args.output_dir)
        summary["stage_a_done"] = True
        summary["rwa_best_pairs"] = len(res_a.get("rwa_selection_dev", {}).get("pair_best_230ms", {}))
    if args.mode in ("stage_b", "all"):
        res_b = run_stage_b_upgrade(
            args.lakehouse, domain_filter=args.domain, output_dir=args.output_dir, max_workers=max(1, args.max_workers)
        )
        summary["stage_b_done"] = True
        summary["frozen_pairs"] = len(res_b.get("frozen_params", {}))
        summary["splits"] = sorted(res_b.get("splits", {}).keys())
    print(json.dumps(summary, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
