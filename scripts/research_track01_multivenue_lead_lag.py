#!/usr/bin/env python3
"""Track 01 (`01_quant_profit`) Multi-Venue 10h Crypto Lead-Lag Evaluator (stdlib-only top level).

Evaluates causal cross-venue crypto lead-lag (`BTC`, `ETH`, `SOL`) across target venues
`lighter`, `lighter_robinhood`, `extended`, and `arcus` with primary leaders `binance`
and `coinbase` over the full Aleph W01-W25 history (~246h across 24 continuous 10h units).

Implements the exact `LeadLagTaker` decision mechanics from `src/strategy/lead_lag_taker.rs`:
  - `A_baseline_fixed_theta`: fixed `theta_bps`
  - `B_vol_adaptive_theta`: causal EWMA 1s log-return threshold `max(theta, mult * sigma_1s)`
  - `C_dual_leader`: directional consensus confirmation (`binance` <-> `coinbase`)
  - `D_catchup_exit`: early profit-protecting catch-up exit (`exit_on_catchup_frac`)
  - `E_combined_all`: vol-adaptive + dual-leader + catch-up exit
  - `F_vol_plus_dual`: vol-adaptive + dual-leader (ablation without catch-up exit)

Protocol (`AGENTS.md` strategy research contract):
  - Dynamic campaign discovery (`tokyo-crypto-*-w*` + cross-midnight `utc_date` + `utc_date+1`).
  - `full_dev` (`w01..w20 ex w08`, 19 windows ~195h including `w03` crash) vs untouched
    `holdout` (`w21..w25`, 5 windows ~51h). (`selection_dev` = `w18..w21` also supported.)
  - Parameters and regime volatility cutoffs (`calm` / `normal` / `extreme_crash`) are
    selected and frozen strictly on `full_dev`, then replayed ONCE untouched on `holdout`.
  - Reports both quote-based and strict `public_trades` print-verified PnL with `$500`
    and `$1000` L1 depth caps (`min(clip_usd, 0.5 * entry_l1_usd)`), exact venue fee
    schedules (`lighter` and `lighter_robinhood` marked `public_tier0_unverified`),
    `cont=True` (`>=10h, max_gap<=60s`), regime stratification, `worst_10h_window`,
    `max_drawdown_usd`, `positive_window_ratio`, and cumulative multi-window equity curves.

`duckdb` is imported lazily inside `run_duckdb_crypto_lead_lag()` only so unit tests and
`--self-test` run in 100% stdlib-only environments without `numpy` or `duckdb`.
"""

from __future__ import annotations

import argparse
import bisect
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
DEFAULT_WARMUP_MS = 15 * 60_000  # 15 min warm-up before 10h decision horizon
DEFAULT_MAX_AGE_MS = 5_000
DEFAULT_MAX_DEV_BPS = 60.0
DEFAULT_MAX_GAP_MS = 30_000
DEFAULT_VOL_EWMA_TAU_MS = 15_000
DEFAULT_PRINT_WINDOW_MS = 1_000
DEFAULT_L1_DEPTH_CAP_FRAC = 0.5
CLIPS_USD: tuple[float, ...] = (500.0, 1_000.0)

CANONICAL_CRYPTO_WINDOWS: dict[str, dict[str, dict[str, str]]] = {
    "selection_dev": {
        "w18": {"campaign_id": "tokyo-crypto-20261122T200004Z-w18", "utc_date": "2026-11-22"},
        "w19": {"campaign_id": "tokyo-crypto-20261123T061504Z-w19", "utc_date": "2026-11-23"},
        "w20": {"campaign_id": "tokyo-crypto-20261123T163004Z-w20", "utc_date": "2026-11-23"},
        "w21": {"campaign_id": "tokyo-crypto-20261124T024504Z-w21", "utc_date": "2026-11-24"},
    },
    "holdout": {
        "w21": {"campaign_id": "tokyo-crypto-20261124T024504Z-w21", "utc_date": "2026-11-24"},
        "w22": {"campaign_id": "tokyo-crypto-20261124T130005Z-w22", "utc_date": "2026-11-24"},
        "w23": {"campaign_id": "tokyo-crypto-20261124T231506Z-w23", "utc_date": "2026-11-24"},
        "w24": {"campaign_id": "tokyo-crypto-20261125T093007Z-w24", "utc_date": "2026-11-25"},
        "w25": {"campaign_id": "tokyo-crypto-20261125T194507Z-w25", "utc_date": "2026-11-25"},
    },
}

SYMBOLS: tuple[str, ...] = ("BTC", "ETH", "SOL")
TARGET_VENUES: tuple[str, ...] = ("lighter", "lighter_robinhood", "extended", "arcus")
LEADERS: tuple[tuple[str, str], ...] = (
    ("binance", "coinbase"),
    ("coinbase", "binance"),
)

VENUE_SYMBOL_MAP: dict[str, dict[str, str]] = {
    "binance": {"BTC": "BTCUSDT", "ETH": "ETHUSDT", "SOL": "SOLUSDT"},
    "coinbase": {"BTC": "BTC-USD", "ETH": "ETH-USD", "SOL": "SOL-USD"},
    "lighter": {"ETH": "0", "BTC": "1", "SOL": "2"},
    "lighter_robinhood": {"ETH": "0", "BTC": "1", "SOL": "3"},
    "extended": {"BTC": "BTC-USD", "ETH": "ETH-USD", "SOL": "SOL-USD"},
    "arcus": {"BTC": "BTC-USD", "ETH": "ETH-USD", "SOL": "SOL-USD"},
}

VENUE_PROFILES: dict[str, dict[str, Any]] = {
    "lighter": {
        "settlement": "USDC",
        "maker_fee_bps": 0.0,
        "taker_fee_bps": 0.0,
        "taker_delay_ms": 330,
        "account_fee_status": "public_tier0_unverified",
        "fee_note": "Standard public tier 0/0 bps; signed /api/v1/accountLimits not audited.",
    },
    "lighter_robinhood": {
        "settlement": "USDG",
        "maker_fee_bps": 0.0,
        "taker_fee_bps": 0.0,
        "taker_delay_ms": 230,
        "account_fee_status": "public_tier0_unverified",
        "fee_note": "Standard public tier 0/0 bps on USDG settlement; unverified account limits.",
    },
    "extended": {
        "settlement": "USDC",
        "maker_fee_bps": 0.0,
        "taker_fee_bps": 2.50,
        "taker_delay_ms": 60,
        "account_fee_status": "account_evidence_required",
        "fee_note": "Public schedule 0.0 maker / 2.50 taker bps; account-level fee audit pending.",
    },
    "arcus": {
        "settlement": "USDC",
        "maker_fee_bps": 1.50,
        "taker_fee_bps": 4.50,
        "taker_delay_ms": 40,
        "account_fee_status": "public_tier0_schedule_only",
        "fee_note": "Public Level 0 Base crypto schedule 1.50 maker / 4.50 taker bps (Level 4 Platinum 2.30 taker).",
    },
}

# Pre-registered selection grids (evaluated strictly on full_dev / selection_dev only).
BASELINE_THETAS_BPS: tuple[float, ...] = (4.0, 6.0, 8.0)
BASELINE_HOLD_MS: tuple[int, ...] = (3_000, 10_000)
BASELINE_TAU_MS: tuple[int, ...] = (30_000, 120_000)
VOL_THETA_MULT_GRID: tuple[float, ...] = (1.0, 1.5, 2.0)
CATCHUP_FRAC_GRID: tuple[float, ...] = (0.50, 0.70, 0.85)
MIN_SELECTION_TRADES = 40

ARM_NAMES: tuple[str, ...] = (
    "A_baseline_fixed_theta",
    "B_vol_adaptive_theta",
    "C_dual_leader",
    "D_catchup_exit",
    "E_combined_all",
    "F_vol_plus_dual",
)


@dataclasses.dataclass(frozen=True)
class CryptoLeadLagPair:
    """Canonical target-leader evaluation pair."""

    tgt_venue: str
    symbol: str
    ref_venue: str
    confirm_venue: str

    @property
    def pair_id(self) -> str:
        return f"{self.tgt_venue}_{self.symbol}_from_{self.ref_venue}"

    @property
    def label(self) -> str:
        return f"{self.tgt_venue} {self.symbol} <- {self.ref_venue} (confirm:{self.confirm_venue})"


def build_default_pairs() -> list[CryptoLeadLagPair]:
    """Return all 24 canonical crypto target-leader pairs."""
    pairs: list[CryptoLeadLagPair] = []
    for tgt_venue in TARGET_VENUES:
        for symbol in SYMBOLS:
            for ref_venue, confirm_venue in LEADERS:
                pairs.append(
                    CryptoLeadLagPair(
                        tgt_venue=tgt_venue,
                        symbol=symbol,
                        ref_venue=ref_venue,
                        confirm_venue=confirm_venue,
                    )
                )
    return pairs


@dataclasses.dataclass(frozen=True)
class LeadLagArmParams:
    """Frozen parameters matching Rust `LeadLagTakerConfig` + execution/fee settings."""

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
    taker_fee_bps: float = 0.0
    l1_depth_cap_frac: float = DEFAULT_L1_DEPTH_CAP_FRAC
    step_ms: int = STEP_MS
    print_window_ms: int = DEFAULT_PRINT_WINDOW_MS


@dataclasses.dataclass
class _OpenPos:
    side: int  # +1 Long, -1 Short
    entry_filled_est_ms: int
    exit_due_ms: int
    entry_tgt_mid: float
    entry_gap_bps: float
    exit_triggered: bool = False


class LeadLagTakerPy:
    """Pure-stdlib Python mirror of Rust `LeadLagTaker` for deterministic replay & testing."""

    def __init__(self, cfg: LeadLagArmParams) -> None:
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
            frac = cfg.exit_on_catchup_frac
            if not (math.isfinite(frac) and 0.0 < frac <= 1.0):
                raise ValueError("exit_on_catchup_frac must be in (0.0, 1.0]")

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

    def observe_with_confirm(
        self,
        now_ms: int,
        ref_obs: tuple[float, int],
        confirm_ref_obs: tuple[float, int] | None,
        tgt_obs: tuple[float, int],
    ) -> tuple[str, int | None, float | None]:
        """Return `(kind, side_sgn, dev_bps)` where kind is Enter/Exit/Idle/Blocked:*."""
        ref_mid, ref_recv_ms = ref_obs
        tgt_mid, tgt_recv_ms = tgt_obs
        if (
            not (math.isfinite(ref_mid) and ref_mid > 0.0 and math.isfinite(tgt_mid) and tgt_mid > 0.0)
            or ref_recv_ms > now_ms
            or tgt_recv_ms > now_ms
        ):
            return ("Blocked:InvalidInput", None, None)

        if confirm_ref_obs is not None:
            c_mid, c_recv_ms = confirm_ref_obs
            if not (math.isfinite(c_mid) and c_mid > 0.0) or c_recv_ms > now_ms:
                return ("Blocked:InvalidInput", None, None)

        if (
            (self.last_obs_ms is not None and now_ms < self.last_obs_ms)
            or (self.tgt_vol_last is not None and now_ms < self.tgt_vol_last[0])
            or (self.confirm_last_obs_ms is not None and now_ms < self.confirm_last_obs_ms)
        ):
            return ("Blocked:InvalidInput", None, None)

        ref_stale = (now_ms - ref_recv_ms) > self.cfg.max_ref_age_ms
        tgt_stale = (now_ms - tgt_recv_ms) > self.cfg.max_tgt_age_ms
        confirm_fresh_obs: tuple[float, int] | None = None
        if confirm_ref_obs is not None and (now_ms - confirm_ref_obs[1]) <= self.cfg.max_ref_age_ms:
            confirm_fresh_obs = confirm_ref_obs

        # Update causal EWMA 1s log-return variance on fresh target observations only.
        if not tgt_stale:
            if self.tgt_vol_last is not None:
                prev_ms, prev_mid = self.tgt_vol_last
                dt_ms = now_ms - prev_ms
                if dt_ms > self.cfg.max_gap_ms:
                    self.tgt_var_bps2_per_s = 0.0
                elif dt_ms > 0 and prev_mid > 0.0:
                    dt_s = dt_ms / 1000.0
                    ret_bps = math.log(tgt_mid / prev_mid) * 1e4
                    inst_var = (ret_bps * ret_bps) / dt_s
                    if math.isfinite(inst_var):
                        alpha_v = 1.0 - math.exp(-dt_ms / float(self.cfg.vol_ewma_tau_ms))
                        self.tgt_var_bps2_per_s = (1.0 - alpha_v) * self.tgt_var_bps2_per_s + alpha_v * inst_var
            self.tgt_vol_last = (now_ms, tgt_mid)

        dev_bps: float | None = None
        if not ref_stale and not tgt_stale:
            x = math.log(ref_mid) - math.log(tgt_mid)
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
        if not tgt_stale and confirm_fresh_obs is not None:
            c_mid = confirm_fresh_obs[0]
            x_c = math.log(c_mid) - math.log(tgt_mid)
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
                    return ("Blocked:StaleTarget", None, None)
                self.pos.exit_triggered = True
                return ("Exit", self.pos.side, None)
            frac = self.cfg.exit_on_catchup_frac
            if (
                frac is not None
                and not tgt_stale
                and now_ms >= self.pos.entry_filled_est_ms
                and self.pos.entry_tgt_mid > 0.0
                and self.pos.entry_gap_bps > 0.0
            ):
                catchup_bps = self.pos.side * math.log(tgt_mid / self.pos.entry_tgt_mid) * 1e4
                if math.isfinite(catchup_bps) and catchup_bps >= frac * self.pos.entry_gap_bps:
                    self.pos.exit_triggered = True
                    return ("Exit", self.pos.side, None)
            return ("Idle", None, None)

        if ref_stale:
            return ("Blocked:StaleReference", None, None)
        if tgt_stale:
            return ("Blocked:StaleTarget", None, None)
        if dev_bps is None:
            return ("Blocked:WarmingUp", None, None)
        warm_since = self.first_obs_ms if self.first_obs_ms is not None else now_ms
        if (now_ms - warm_since) < self.cfg.warmup_ms:
            return ("Blocked:WarmingUp", None, None)
        if abs(dev_bps) >= self.cfg.max_dev_bps:
            return ("Blocked:StructuralBreak", None, None)
        if abs(dev_bps) < self.effective_theta_bps():
            return ("Idle", None, None)

        side = 1 if dev_bps > 0.0 else -1
        if self.cfg.require_dual_leader_confirm and confirm_fresh_obs is not None:
            if confirm_dev_bps is None or abs(confirm_dev_bps) >= self.cfg.max_dev_bps or (side * confirm_dev_bps) <= 0.0:
                return ("Idle", None, None)

        entry_filled_est_ms = now_ms + self.cfg.taker_delay_ms
        self.pos = _OpenPos(
            side=side,
            entry_filled_est_ms=entry_filled_est_ms,
            exit_due_ms=entry_filled_est_ms + self.cfg.hold_ms,
            entry_tgt_mid=tgt_mid,
            entry_gap_bps=abs(dev_bps),
            exit_triggered=False,
        )
        return ("Enter", side, dev_bps)


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


def _empty_sim_result() -> dict[str, Any]:
    return {
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
        "avg_hold_actual_ms": 0.0,
    }


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
    print_unverifiable: int,
) -> dict[str, Any]:
    k = len(quote_nets)
    if k == 0:
        out = _empty_sim_result()
        out["print_unverifiable"] = print_unverifiable
        out["stale_entry_skips"] = stale_entry_skips
        out["deferred_exits"] = deferred_exits
        out["unsettled_exits"] = unsettled_exits
        out["early_catchup_exits"] = early_catchup_exits
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
        "avg_hold_actual_ms": round(sum(hold_actual_ms_list) / len(hold_actual_ms_list), 1)
        if hold_actual_ms_list
        else 0.0,
    }


def simulate_lead_lag_streams(
    ref_books: Sequence[tuple[int, float, float, float, float]],
    tgt_books: Sequence[tuple[int, float, float, float, float]],
    cfg: LeadLagArmParams,
    *,
    confirm_books: Sequence[tuple[int, float, float, float, float]] | None = None,
    trades: Sequence[tuple[int, int, float, float]] | None = None,
) -> dict[str, Any]:
    """Replay causal LeadLagTaker strategy on chronological BBO & public_trades streams.

    Each book tuple is `(ts_ms, bid_px, bid_sz, ask_px, ask_sz)`.
    Each trade tuple is `(ts_ms, aggressor_sgn [+1 buy / -1 sell], price, size)`.
    """
    if len(ref_books) < 2 or len(tgt_books) < 2:
        return _empty_sim_result()

    t0_ms = max(int(ref_books[0][0]), int(tgt_books[0][0]))
    t1_ms = min(int(ref_books[-1][0]), int(tgt_books[-1][0]))
    if t1_ms <= t0_ms + cfg.step_ms:
        return _empty_sim_result()

    tgt_ts_arr = [int(b[0]) for b in tgt_books]
    n_ref = len(ref_books)
    n_tgt = len(tgt_books)
    n_conf = len(confirm_books) if confirm_books else 0

    if trades:
        tr_ts = [int(tr[0]) for tr in trades]
        tr_sgn = [int(tr[1]) for tr in trades]
        tr_px = [float(tr[2]) for tr in trades]
    else:
        tr_ts = []
        tr_sgn = []
        tr_px = []

    engine = LeadLagTakerPy(cfg)

    i_ref = 0
    i_tgt = 0
    i_conf = 0
    has_ref = False
    has_tgt = False
    has_conf = False

    active_trade: dict[str, Any] | None = None
    pending_exit_settle: dict[str, Any] | None = None

    quote_nets: list[float] = []
    pv_nets: list[float] = []
    entry_l1_list: list[float] = []
    hold_actual_ms_list: list[int] = []
    quote_clip_usd = {c: 0.0 for c in CLIPS_USD}
    pv_clip_usd = {c: 0.0 for c in CLIPS_USD}

    longs = 0
    stale_entry_skips = 0
    deferred_exits = 0
    unsettled_exits = 0
    early_catchup_exits = 0
    print_unverifiable = 0

    def latest_tgt_at(ts_query: int) -> tuple[int, tuple[int, float, float, float, float]] | None:
        idx = bisect.bisect_right(tgt_ts_arr, ts_query) - 1
        if idx < 0:
            return None
        return idx, tgt_books[idx]

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

            if tr_ts:
                pe = _mean_print_price(tr_ts, tr_sgn, tr_px, tr["t_entry_fill"], cfg.print_window_ms, side)
                px_ = _mean_print_price(tr_ts, tr_sgn, tr_px, tr["t_exit_actual"], cfg.print_window_ms, -side)
                if pe is not None and px_ is not None and pe > 0.0 and px_ > 0.0:
                    pv_net = side * (px_ / pe - 1.0) * 1e4 - 2.0 * cfg.taker_fee_bps
                    pv_nets.append(pv_net)
                    for clip in CLIPS_USD:
                        notl = min(clip, cfg.l1_depth_cap_frac * entry_l1)
                        pv_clip_usd[clip] += notl * pv_net * 1e-4
                else:
                    print_unverifiable += 1

            engine.notify_exit_filled()
            pending_exit_settle = None
            active_trade = None

        if has_ref and has_tgt:
            rb = ref_books[i_ref - 1]
            tb = tgt_books[i_tgt - 1]
            ref_ts, r_bid, r_ask = int(rb[0]), float(rb[1]), float(rb[3])
            tgt_ts, t_bid, t_ask = int(tb[0]), float(tb[1]), float(tb[3])
            ref_mid = 0.5 * (r_bid + r_ask)
            tgt_mid = 0.5 * (t_bid + t_ask)
            conf_obs: tuple[float, int] | None = None
            if confirm_books and has_conf:
                cb = confirm_books[i_conf - 1]
                conf_obs = (0.5 * (float(cb[1]) + float(cb[3])), int(cb[0]))

            kind, side_sgn, _ = engine.observe_with_confirm(
                now_ms,
                (ref_mid, ref_ts),
                conf_obs,
                (tgt_mid, tgt_ts),
            )

            if kind == "Enter" and side_sgn is not None:
                t_entry_fill = now_ms + cfg.taker_delay_ms
                if t_entry_fill > t1_ms:
                    engine.clear_position()
                else:
                    found = latest_tgt_at(t_entry_fill)
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
                            active_trade = {
                                "side": side_sgn,
                                "t_entry_dec": now_ms,
                                "t_entry_fill": t_entry_fill,
                                "entry_px": entry_px,
                                "entry_l1_usd": l1_usd,
                                "exit_due_ms": t_entry_fill + cfg.hold_ms,
                            }

            elif kind == "Exit" and active_trade is not None and pending_exit_settle is None:
                if now_ms < active_trade["exit_due_ms"]:
                    early_catchup_exits += 1
                t_exit_target = now_ms + cfg.taker_delay_ms
                found = latest_tgt_at(t_exit_target)
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
                    }

        now_ms += cfg.step_ms

    if active_trade is not None or pending_exit_settle is not None:
        unsettled_exits += 1

    return _finalize_sim_result(
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
        print_unverifiable,
    )


@dataclasses.dataclass
class PrecomputedPairSignals:
    """Window-level precomputed causal signals across `BASELINE_TAU_MS` for fast exact replay."""

    t0_ms: int
    t1_ms: int
    step_ms: int
    ts_ms: list[int]
    tgt_books: Sequence[tuple[int, float, float, float, float]]
    tgt_ts_arr: list[int]
    tr_ts: list[int]
    tr_sgn: list[int]
    tr_px: list[float]
    tgt_stale: list[bool]
    tgt_mid: list[float]
    sigma_1s: list[float]
    conf_fresh: list[bool]
    can_enter_by_tau: dict[int, list[bool]]
    dev_by_tau: dict[int, list[float]]
    cdev_by_tau: dict[int, list[float | None]]
    cand_indices_by_tau: dict[int, list[int]]
    tgt_realized_1s_vol_bps: float
    ref_realized_1s_vol_bps: float


def compute_window_realized_1s_vol_bps(books: Sequence[tuple[int, float, float, float, float]]) -> float:
    """Compute realized 1s log-return volatility (bps/sqrt(s)) from a BBO stream."""
    if len(books) < 20:
        return 0.0
    ret_1s: list[float] = []
    last_ts = int(books[0][0])
    last_mid = 0.5 * (float(books[0][1]) + float(books[0][3]))
    for b in books[1:]:
        ts = int(b[0])
        mid = 0.5 * (float(b[1]) + float(b[3]))
        dt = ts - last_ts
        if 1_000 <= dt <= 10_000 and last_mid > 0.0 and mid > 0.0:
            r = math.log(mid / last_mid) * 1e4 / math.sqrt(dt / 1000.0)
            if math.isfinite(r):
                ret_1s.append(r)
            last_ts = ts
            last_mid = mid
        elif dt > 10_000 and mid > 0.0:
            last_ts = ts
            last_mid = mid
    if len(ret_1s) < 5:
        return 0.0
    return round(statistics.stdev(ret_1s), 4)


def precompute_pair_signals(
    ref_books: Sequence[tuple[int, float, float, float, float]],
    tgt_books: Sequence[tuple[int, float, float, float, float]],
    *,
    confirm_books: Sequence[tuple[int, float, float, float, float]] | None = None,
    trades: Sequence[tuple[int, int, float, float]] | None = None,
    tau_values_ms: Sequence[int] = BASELINE_TAU_MS,
    step_ms: int = STEP_MS,
    warmup_ms: int = DEFAULT_WARMUP_MS,
    max_ref_age_ms: int = DEFAULT_MAX_AGE_MS,
    max_tgt_age_ms: int = DEFAULT_MAX_AGE_MS,
    max_dev_bps: float = DEFAULT_MAX_DEV_BPS,
    max_gap_ms: int = DEFAULT_MAX_GAP_MS,
    vol_ewma_tau_ms: int = DEFAULT_VOL_EWMA_TAU_MS,
    min_cand_theta_bps: float = min(BASELINE_THETAS_BPS),
) -> PrecomputedPairSignals | None:
    """Precompute causal EMA deviations, confirm deviations, and tgt sigma_1s once per window."""
    if len(ref_books) < 2 or len(tgt_books) < 2:
        return None
    t0_ms = max(int(ref_books[0][0]), int(tgt_books[0][0]))
    t1_ms = min(int(ref_books[-1][0]), int(tgt_books[-1][0]))
    if t1_ms <= t0_ms + step_ms:
        return None

    n_ref = len(ref_books)
    n_tgt = len(tgt_books)
    n_conf = len(confirm_books) if confirm_books else 0

    ts_list: list[int] = []
    tgt_stale_list: list[bool] = []
    tgt_mid_list: list[float] = []
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

    i_ref = 0
    i_tgt = 0
    i_conf = 0
    has_ref = False
    has_tgt = False
    has_conf = False

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

        ts_list.append(now_ms)

        if not (has_ref and has_tgt):
            tgt_stale_list.append(True)
            tgt_mid_list.append(0.0)
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
        ref_ts, r_bid, r_ask = int(rb[0]), float(rb[1]), float(rb[3])
        tgt_ts, t_bid, t_ask = int(tb[0]), float(tb[1]), float(tb[3])
        ref_mid = 0.5 * (r_bid + r_ask)
        tgt_mid = 0.5 * (t_bid + t_ask)
        valid_px = ref_mid > 0.0 and tgt_mid > 0.0 and math.isfinite(ref_mid) and math.isfinite(tgt_mid)

        ref_stale = (not valid_px) or (now_ms - ref_ts) > max_ref_age_ms
        tgt_stale = (not valid_px) or (now_ms - tgt_ts) > max_tgt_age_ms

        c_mid = 0.0
        c_fresh = False
        if confirm_books and has_conf:
            cb = confirm_books[i_conf - 1]
            c_ts = int(cb[0])
            c_m = 0.5 * (float(cb[1]) + float(cb[3]))
            if math.isfinite(c_m) and c_m > 0.0 and (now_ms - c_ts) <= max_ref_age_ms:
                c_mid = c_m
                c_fresh = True

        if not tgt_stale:
            if tgt_vol_last is not None:
                prev_ms, prev_mid = tgt_vol_last
                dt_ms = now_ms - prev_ms
                if dt_ms > max_gap_ms:
                    tgt_var = 0.0
                elif dt_ms > 0 and prev_mid > 0.0:
                    dt_s = dt_ms / 1000.0
                    ret_bps = math.log(tgt_mid / prev_mid) * 1e4
                    inst_var = (ret_bps * ret_bps) / dt_s
                    if math.isfinite(inst_var):
                        alpha_v = 1.0 - math.exp(-dt_ms / float(vol_ewma_tau_ms))
                        tgt_var = (1.0 - alpha_v) * tgt_var + alpha_v * inst_var
            tgt_vol_last = (now_ms, tgt_mid)

        tgt_stale_list.append(tgt_stale)
        tgt_mid_list.append(tgt_mid)
        sigma_list.append(math.sqrt(max(0.0, tgt_var)))
        conf_fresh_list.append(c_fresh)

        if not ref_stale and not tgt_stale:
            x = math.log(ref_mid) - math.log(tgt_mid)
            gap = (now_ms - last_obs_ms) if last_obs_ms is not None else None
            in_gap = gap is not None and gap <= max_gap_ms
            if not in_gap:
                first_obs_ms = now_ms
            elif first_obs_ms is None:
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
            x_c = math.log(c_mid) - math.log(tgt_mid)
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
        tr_ts = []
        tr_sgn = []
        tr_px = []

    return PrecomputedPairSignals(
        t0_ms=t0_ms,
        t1_ms=t1_ms,
        step_ms=step_ms,
        ts_ms=ts_list,
        tgt_books=tgt_books,
        tgt_ts_arr=[int(b[0]) for b in tgt_books],
        tr_ts=tr_ts,
        tr_sgn=tr_sgn,
        tr_px=tr_px,
        tgt_stale=tgt_stale_list,
        tgt_mid=tgt_mid_list,
        sigma_1s=sigma_list,
        conf_fresh=conf_fresh_list,
        can_enter_by_tau=can_enter_by_tau,
        dev_by_tau=dev_by_tau,
        cdev_by_tau=cdev_by_tau,
        cand_indices_by_tau=cand_indices_by_tau,
        tgt_realized_1s_vol_bps=compute_window_realized_1s_vol_bps(tgt_books),
        ref_realized_1s_vol_bps=compute_window_realized_1s_vol_bps(ref_books),
    )


def simulate_precomputed_arm(sig: PrecomputedPairSignals, cfg: LeadLagArmParams) -> dict[str, Any]:
    """Fast exact replay over precomputed signals (bit-for-bit identical to `simulate_lead_lag_streams`)."""
    n_steps = len(sig.ts_ms)
    if n_steps == 0:
        return _empty_sim_result()

    ts_ms = sig.ts_ms
    tgt_books = sig.tgt_books
    tgt_ts_arr = sig.tgt_ts_arr
    n_tgt = len(tgt_books)
    t1_ms = sig.t1_ms
    tr_ts, tr_sgn, tr_px = sig.tr_ts, sig.tr_sgn, sig.tr_px

    can_enter = sig.can_enter_by_tau[cfg.tau_ms]
    dev_arr = sig.dev_by_tau[cfg.tau_ms]
    cdev_arr = sig.cdev_by_tau[cfg.tau_ms]
    cand_indices = sig.cand_indices_by_tau[cfg.tau_ms]
    tgt_stale = sig.tgt_stale
    tgt_mid = sig.tgt_mid
    sigma_1s = sig.sigma_1s
    conf_fresh = sig.conf_fresh

    quote_nets: list[float] = []
    pv_nets: list[float] = []
    entry_l1_list: list[float] = []
    hold_actual_ms_list: list[int] = []
    quote_clip_usd = {c: 0.0 for c in CLIPS_USD}
    pv_clip_usd = {c: 0.0 for c in CLIPS_USD}

    longs = 0
    stale_entry_skips = 0
    deferred_exits = 0
    unsettled_exits = 0
    early_catchup_exits = 0
    print_unverifiable = 0

    def latest_tgt_at(ts_query: int) -> tuple[int, tuple[int, float, float, float, float]] | None:
        idx = bisect.bisect_right(tgt_ts_arr, ts_query) - 1
        if idx < 0:
            return None
        return idx, tgt_books[idx]

    c_pos = 0
    n_cands = len(cand_indices)
    min_k = 0

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
        side = 1 if d > 0.0 else -1
        if cfg.require_dual_leader_confirm and conf_fresh[k]:
            cd = cdev_arr[k]
            if cd is None or abs(cd) >= cfg.max_dev_bps or (side * cd) <= 0.0:
                continue

        now_ms = ts_ms[k]
        t_entry_fill = now_ms + cfg.taker_delay_ms
        if t_entry_fill > t1_ms:
            continue
        found = latest_tgt_at(t_entry_fill)
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
        entry_tgt_mid = tgt_mid[k]
        entry_gap_bps = abs(d)
        frac = cfg.exit_on_catchup_frac

        # Find exit trigger tick j > k
        j_exit_dec: int | None = None
        if frac is None:
            # Jump directly to first tick >= exit_due_ms
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
                        break
                elif not tgt_stale[j] and t_j >= t_entry_fill and entry_tgt_mid > 0.0 and entry_gap_bps > 0.0:
                    catchup_bps = side * math.log(tgt_mid[j] / entry_tgt_mid) * 1e4
                    if math.isfinite(catchup_bps) and catchup_bps >= frac * entry_gap_bps:
                        j_exit_dec = j
                        early_catchup_exits += 1
                        break
                j += 1

        if j_exit_dec is None:
            unsettled_exits += 1
            break

        t_exit_target = ts_ms[j_exit_dec] + cfg.taker_delay_ms
        found_x = latest_tgt_at(t_exit_target)
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

        # Find settlement tick k_settle >= j_exit_dec where ts_ms[k_settle] >= t_exit_actual
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

        if tr_ts:
            pe = _mean_print_price(tr_ts, tr_sgn, tr_px, t_entry_fill, cfg.print_window_ms, side)
            px_ = _mean_print_price(tr_ts, tr_sgn, tr_px, t_exit_actual, cfg.print_window_ms, -side)
            if pe is not None and px_ is not None and pe > 0.0 and px_ > 0.0:
                pv_net = side * (px_ / pe - 1.0) * 1e4 - 2.0 * cfg.taker_fee_bps
                pv_nets.append(pv_net)
                for clip in CLIPS_USD:
                    notl = min(clip, cfg.l1_depth_cap_frac * entry_l1)
                    pv_clip_usd[clip] += notl * pv_net * 1e-4
            else:
                print_unverifiable += 1

        min_k = k_settle

    return _finalize_sim_result(
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
        print_unverifiable,
    )


def _campaign_parquet_files(
    lakehouse_dir: Path,
    dataset: str,
    venue: str,
    utc_date: str,
    campaign_id: str,
) -> list[str]:
    """Resolve Hive parquet files across `utc_date` and cross-midnight `utc_date + 1`."""
    date_dirs: list[str]
    if not utc_date or utc_date == "*":
        date_dirs = ["*"]
    else:
        date_dirs = [utc_date]
        try:
            next_day = (date.fromisoformat(utc_date) + timedelta(days=1)).isoformat()
            date_dirs.append(next_day)
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


def discover_crypto_campaigns(lakehouse_dir: Path) -> dict[str, dict[str, str]]:
    """Dynamically discover all `tokyo-crypto-*-w*` campaigns and their earliest `utc_date`."""
    base = lakehouse_dir / "parquet" / "bbo_quotes" / "collector_region=tokyo"
    found: dict[int, tuple[str, list[str]]] = {}
    for p in glob.glob(str(base / "venue=*" / "utc_date=*" / "campaign_id=tokyo-crypto-*")):
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


def resolve_crypto_splits(lakehouse_dir: Path, split_mode: str) -> dict[str, dict[str, dict[str, str]]]:
    """Build window dicts for `full_dev` (w01..w20 ex w08), `selection_dev` (w18..w21), and `holdout` (w21..w25)."""
    discovered = discover_crypto_campaigns(lakehouse_dir)
    if not discovered:
        return {
            "full_dev": {},
            "selection_dev": dict(CANONICAL_CRYPTO_WINDOWS["selection_dev"]),
            "holdout": dict(CANONICAL_CRYPTO_WINDOWS["holdout"]),
        }

    def _w_num(wid: str) -> int:
        return int(wid[1:])

    full_dev = {w: info for w, info in sorted(discovered.items()) if _w_num(w) <= 20 and w != "w08"}
    selection_dev = {w: info for w, info in sorted(discovered.items()) if 18 <= _w_num(w) <= 21}
    holdout = {w: info for w, info in sorted(discovered.items()) if _w_num(w) >= 21}
    if split_mode == "selection_dev":
        return {"selection_dev": selection_dev, "holdout": holdout}
    return {"full_dev": full_dev, "holdout": holdout}


def _quantile_linear(sorted_vals: Sequence[float], q: float) -> float:
    if not sorted_vals:
        return 0.0
    if len(sorted_vals) == 1:
        return float(sorted_vals[0])
    pos = q * (len(sorted_vals) - 1)
    lo = int(math.floor(pos))
    hi = min(lo + 1, len(sorted_vals) - 1)
    frac = pos - lo
    return float(sorted_vals[lo]) * (1.0 - frac) + float(sorted_vals[hi]) * frac


def _compute_regime_cutoffs_from_dev(
    dev_rows: Sequence[dict[str, Any]],
) -> dict[str, dict[str, float]]:
    """Freeze per-symbol realized 1s vol tercile/tail cutoffs (`p33`, `p90`) strictly from dev rows."""
    by_sym_win: dict[tuple[str, str], float] = {}
    for r in dev_rows:
        if r.get("skipped") or not r.get("cont"):
            continue
        sym = r["symbol"]
        win = r["win"]
        vol = float(r.get("tgt_realized_1s_vol_bps", 0.0))
        if vol > 0.0 and (sym, win) not in by_sym_win:
            by_sym_win[(sym, win)] = vol

    cutoffs: dict[str, dict[str, float]] = {}
    for sym in SYMBOLS:
        vols = sorted(v for (s, _), v in by_sym_win.items() if s == sym)
        if vols:
            cutoffs[sym] = {
                "p33_bps": round(_quantile_linear(vols, 1.0 / 3.0), 4),
                "p90_bps": round(_quantile_linear(vols, 0.90), 4),
                "max_dev_vol_bps": round(max(vols), 4),
            }
        else:
            cutoffs[sym] = {"p33_bps": 1.0, "p90_bps": 3.0, "max_dev_vol_bps": 3.0}
    return cutoffs


def _classify_regime(symbol: str, tgt_vol_bps: float, cutoffs: dict[str, dict[str, float]]) -> str:
    c = cutoffs.get(symbol)
    if not c:
        return "normal"
    if tgt_vol_bps <= c["p33_bps"]:
        return "calm"
    if tgt_vol_bps > c["p90_bps"]:
        return "extreme_crash"
    return "normal"


def _tail_risk_and_curve(group: Sequence[dict[str, Any]]) -> dict[str, Any]:
    """Compute chronological cumulative equity curve, max_drawdown_usd, worst_10h_window, positive_window_ratio."""
    ordered = sorted(group, key=lambda x: x["win"])
    cum_q1000 = 0.0
    peak_q1000 = 0.0
    max_dd_q1000 = 0.0
    cum_pv1000 = 0.0
    peak_pv1000 = 0.0
    max_dd_pv1000 = 0.0

    worst_q_row = min(ordered, key=lambda x: float(x["quote_clip_pnl_usd"]["1000"]))
    worst_pv_row = min(ordered, key=lambda x: float(x["pv_clip_pnl_usd"]["1000"]))

    curve: list[dict[str, Any]] = []
    pos_q = 0
    pos_pv = 0
    pv_active_wins = 0
    for r in ordered:
        q1000 = float(r["quote_clip_pnl_usd"]["1000"])
        pv1000 = float(r["pv_clip_pnl_usd"]["1000"])
        cum_q1000 += q1000
        peak_q1000 = max(peak_q1000, cum_q1000)
        max_dd_q1000 = max(max_dd_q1000, peak_q1000 - cum_q1000)
        cum_pv1000 += pv1000
        peak_pv1000 = max(peak_pv1000, cum_pv1000)
        max_dd_pv1000 = max(max_dd_pv1000, peak_pv1000 - cum_pv1000)
        if int(r["trades"]) > 0 and q1000 > 0.0:
            pos_q += 1
        if int(r["print_verified_trades"]) > 0:
            pv_active_wins += 1
            if pv1000 > 0.0:
                pos_pv += 1
        curve.append(
            {
                "win": r["win"],
                "regime": r.get("regime", "normal"),
                "trades": int(r["trades"]),
                "pv_trades": int(r["print_verified_trades"]),
                "quote_1000_usd": round(q1000, 4),
                "cum_quote_1000_usd": round(cum_q1000, 4),
                "pv_1000_usd": round(pv1000, 4),
                "cum_pv_1000_usd": round(cum_pv1000, 4),
            }
        )

    n_wins = len(ordered)
    return {
        "positive_window_ratio_quote": round(pos_q / n_wins, 4) if n_wins else 0.0,
        "positive_window_ratio_pv": round(pos_pv / pv_active_wins, 4) if pv_active_wins else None,
        "max_drawdown_quote_1000_usd": round(max_dd_q1000, 4),
        "max_drawdown_pv_1000_usd": round(max_dd_pv1000, 4),
        "worst_10h_window_quote": {
            "win": worst_q_row["win"],
            "regime": worst_q_row.get("regime", "normal"),
            "quote_clip_1000_usd": round(float(worst_q_row["quote_clip_pnl_usd"]["1000"]), 4),
            "quote_net_bps_mean": worst_q_row["quote_net_bps_mean"],
        },
        "worst_10h_window_pv": {
            "win": worst_pv_row["win"],
            "regime": worst_pv_row.get("regime", "normal"),
            "pv_clip_1000_usd": round(float(worst_pv_row["pv_clip_pnl_usd"]["1000"]), 4),
            "pv_net_bps_mean": worst_pv_row["print_verified_net_bps_mean"],
        },
        "cumulative_curve": curve,
    }


def _aggregate_arm_rows(rows: Sequence[dict[str, Any]], *, cont_only: bool = False) -> dict[str, Any]:
    """Aggregate window-level rows by `(arm, pair_id)`, `(arm, tgt_venue)`, and `(arm, tgt_venue, regime)`."""
    filtered = [r for r in rows if not r.get("skipped") and (not cont_only or r.get("cont"))]
    by_pair_arm: dict[tuple[str, str], list[dict[str, Any]]] = defaultdict(list)
    by_venue_arm: dict[tuple[str, str], list[dict[str, Any]]] = defaultdict(list)
    by_regime_venue_arm: dict[tuple[str, str, str], list[dict[str, Any]]] = defaultdict(list)
    by_regime_arm: dict[tuple[str, str], list[dict[str, Any]]] = defaultdict(list)

    for r in filtered:
        by_pair_arm[(r["arm"], r["pair_id"])].append(r)
        by_venue_arm[(r["arm"], r["tgt_venue"])].append(r)
        reg = r.get("regime", "normal")
        by_regime_venue_arm[(r["arm"], r["tgt_venue"], reg)].append(r)
        by_regime_arm[(r["arm"], reg)].append(r)

    def _agg(group: Sequence[dict[str, Any]], *, include_pair_fields: bool, include_curve: bool) -> dict[str, Any]:
        first = group[0]
        trades = sum(int(x["trades"]) for x in group)
        pv_trades = sum(int(x["print_verified_trades"]) for x in group)
        q_bps_total = sum(float(x["quote_net_bps_total"]) for x in group)
        pv_bps_total = sum(float(x["print_verified_net_bps_total"]) for x in group)
        q_usd_500 = sum(float(x["quote_clip_pnl_usd"]["500"]) for x in group)
        q_usd_1000 = sum(float(x["quote_clip_pnl_usd"]["1000"]) for x in group)
        pv_usd_500 = sum(float(x["pv_clip_pnl_usd"]["500"]) for x in group)
        pv_usd_1000 = sum(float(x["pv_clip_pnl_usd"]["1000"]) for x in group)
        win_q_pos = sum(1 for x in group if int(x["trades"]) > 0 and float(x["quote_net_bps_total"]) > 0.0)
        win_pv_pos = sum(
            1 for x in group if int(x["print_verified_trades"]) > 0 and float(x["print_verified_net_bps_total"]) > 0.0
        )
        hours_covered = sum(float(x.get("hours_covered", 0.0)) for x in group)
        out: dict[str, Any] = {
            "arm": first["arm"],
            "tgt_venue": first["tgt_venue"],
            "settlement": first["settlement"],
            "account_fee_status": first["account_fee_status"],
            "taker_fee_bps": first["taker_fee_bps"],
            "windows_covered": len(group),
            "cont_windows": sum(1 for x in group if x.get("cont")),
            "hours_covered": round(hours_covered, 2),
            "trades": trades,
            "print_verified_trades": pv_trades,
            "print_verified_coverage": round(pv_trades / trades, 3) if trades else 0.0,
            "quote_net_bps_total": round(q_bps_total, 4),
            "quote_net_bps_mean": round(q_bps_total / trades, 4) if trades else 0.0,
            "print_verified_net_bps_total": round(pv_bps_total, 4),
            "print_verified_net_bps_mean": round(pv_bps_total / pv_trades, 4) if pv_trades else None,
            "quote_clip_500_usd": round(q_usd_500, 4),
            "quote_clip_1000_usd": round(q_usd_1000, 4),
            "pv_clip_500_usd": round(pv_usd_500, 4),
            "pv_clip_1000_usd": round(pv_usd_1000, 4),
            "positive_quote_windows": win_q_pos,
            "positive_pv_windows": win_pv_pos,
            "early_catchup_exits": sum(int(x["early_catchup_exits"]) for x in group),
        }
        if include_pair_fields:
            out["pair_id"] = first["pair_id"]
            out["symbol"] = first["symbol"]
            out["ref_venue"] = first["ref_venue"]
            out["confirm_venue"] = first["confirm_venue"]
            l1s = [float(x["median_entry_l1_usd"]) for x in group if float(x["median_entry_l1_usd"]) > 0.0]
            out["median_entry_l1_usd"] = round(statistics.median(l1s), 2) if l1s else 0.0
        if include_curve:
            out.update(_tail_risk_and_curve(group))
        return out

    # Venue-level curves aggregate per-window sums across pairs
    by_venue_arm_curve: dict[str, dict[str, Any]] = {}
    for (arm, venue), grp in sorted(by_venue_arm.items()):
        by_win: dict[str, list[dict[str, Any]]] = defaultdict(list)
        for r in grp:
            by_win[r["win"]].append(r)
        win_totals = []
        for win_id, wrs in sorted(by_win.items()):
            win_totals.append(
                {
                    "win": win_id,
                    "regime": wrs[0].get("regime", "normal"),
                    "trades": sum(int(x["trades"]) for x in wrs),
                    "print_verified_trades": sum(int(x["print_verified_trades"]) for x in wrs),
                    "quote_net_bps_mean": round(
                        sum(float(x["quote_net_bps_total"]) for x in wrs)
                        / max(1, sum(int(x["trades"]) for x in wrs)),
                        4,
                    ),
                    "print_verified_net_bps_mean": round(
                        sum(float(x["print_verified_net_bps_total"]) for x in wrs)
                        / sum(int(x["print_verified_trades"]) for x in wrs),
                        4,
                    )
                    if sum(int(x["print_verified_trades"]) for x in wrs) > 0
                    else None,
                    "quote_clip_pnl_usd": {"1000": sum(float(x["quote_clip_pnl_usd"]["1000"]) for x in wrs)},
                    "pv_clip_pnl_usd": {"1000": sum(float(x["pv_clip_pnl_usd"]["1000"]) for x in wrs)},
                }
            )
        base_agg = _agg(grp, include_pair_fields=False, include_curve=False)
        base_agg["unique_windows"] = len(win_totals)
        base_agg.update(_tail_risk_and_curve(win_totals))
        by_venue_arm_curve[f"{arm}|{venue}"] = base_agg

    return {
        "by_arm_pair": {
            f"{arm}|{pid}": _agg(grp, include_pair_fields=True, include_curve=True)
            for (arm, pid), grp in sorted(by_pair_arm.items())
        },
        "by_arm_venue": by_venue_arm_curve,
        "by_arm_venue_regime": {
            f"{arm}|{venue}|{reg}": _agg(grp, include_pair_fields=False, include_curve=False)
            for (arm, venue, reg), grp in sorted(by_regime_venue_arm.items())
        },
        "by_arm_regime": {
            f"{arm}|{reg}": _agg(grp, include_pair_fields=False, include_curve=False)
            for (arm, reg), grp in sorted(by_regime_arm.items())
        },
    }


def _arm_configs_from_frozen(pair: CryptoLeadLagPair, frozen: dict[str, Any], *, warmup_ms: int) -> dict[str, LeadLagArmParams]:
    vp = VENUE_PROFILES[pair.tgt_venue]
    common = dict(
        theta_bps=float(frozen["theta_bps"]),
        tau_ms=int(frozen["tau_ms"]),
        hold_ms=int(frozen["hold_ms"]),
        taker_delay_ms=int(vp["taker_delay_ms"]),
        warmup_ms=warmup_ms,
        taker_fee_bps=float(vp["taker_fee_bps"]),
    )
    vol_mult = float(frozen["vol_theta_mult"])
    catch_frac = float(frozen["exit_on_catchup_frac"])
    return {
        "A_baseline_fixed_theta": LeadLagArmParams(**common),
        "B_vol_adaptive_theta": LeadLagArmParams(**common, vol_theta_mult=vol_mult),
        "C_dual_leader": LeadLagArmParams(**common, require_dual_leader_confirm=True),
        "D_catchup_exit": LeadLagArmParams(**common, exit_on_catchup_frac=catch_frac),
        "E_combined_all": LeadLagArmParams(
            **common,
            vol_theta_mult=vol_mult,
            require_dual_leader_confirm=True,
            exit_on_catchup_frac=catch_frac,
        ),
        "F_vol_plus_dual": LeadLagArmParams(
            **common,
            vol_theta_mult=vol_mult,
            require_dual_leader_confirm=True,
        ),
    }


def _select_pair_frozen_params(
    pair: CryptoLeadLagPair,
    window_streams: dict[str, dict[str, Any]],
    *,
    warmup_ms: int = DEFAULT_WARMUP_MS,
) -> tuple[dict[str, Any], dict[str, Any]]:
    """In-memory selection helper (used in unit tests or small splits)."""
    pre_by_win: dict[str, PrecomputedPairSignals] = {}
    for win_id, st in sorted(window_streams.items()):
        if not st["cont"]:
            continue
        sig = precompute_pair_signals(
            st["ref"], st["tgt"], confirm_books=st["confirm"], trades=st["trades"], warmup_ms=warmup_ms
        )
        if sig is not None:
            pre_by_win[win_id] = sig
    return _select_from_precomputed(pair, pre_by_win, warmup_ms=warmup_ms)


def _select_from_precomputed(
    pair: CryptoLeadLagPair,
    pre_by_win: dict[str, PrecomputedPairSignals],
    *,
    warmup_ms: int = DEFAULT_WARMUP_MS,
) -> tuple[dict[str, Any], dict[str, Any]]:
    vp = VENUE_PROFILES[pair.tgt_venue]
    taker_fee_bps = float(vp["taker_fee_bps"])
    taker_delay_ms = int(vp["taker_delay_ms"])

    baseline_candidates: list[dict[str, Any]] = []
    for tau_ms in BASELINE_TAU_MS:
        for hold_ms in BASELINE_HOLD_MS:
            for theta_bps in BASELINE_THETAS_BPS:
                cfg = LeadLagArmParams(
                    theta_bps=theta_bps,
                    tau_ms=tau_ms,
                    hold_ms=hold_ms,
                    taker_delay_ms=taker_delay_ms,
                    warmup_ms=warmup_ms,
                    taker_fee_bps=taker_fee_bps,
                )
                tot_tr = tot_pv = 0
                tot_qb = tot_pvb = tot_q1000 = tot_pv1000 = 0.0
                for sig in pre_by_win.values():
                    r = simulate_precomputed_arm(sig, cfg)
                    tot_tr += r["trades"]
                    tot_pv += r["print_verified_trades"]
                    tot_qb += r["quote_net_bps_total"]
                    tot_pvb += r["print_verified_net_bps_total"]
                    tot_q1000 += r["quote_clip_pnl_usd"]["1000"]
                    tot_pv1000 += r["pv_clip_pnl_usd"]["1000"]
                baseline_candidates.append(
                    {
                        "theta_bps": theta_bps,
                        "hold_ms": hold_ms,
                        "tau_ms": tau_ms,
                        "trades": tot_tr,
                        "pv_trades": tot_pv,
                        "quote_net_bps_total": round(tot_qb, 4),
                        "pv_net_bps_total": round(tot_pvb, 4),
                        "quote_clip_1000_usd": round(tot_q1000, 4),
                        "pv_clip_1000_usd": round(tot_pv1000, 4),
                    }
                )

    min_tr = min(MIN_SELECTION_TRADES, max(5, 5 * len(pre_by_win)))
    eligible_base = [
        c
        for c in baseline_candidates
        if c["trades"] >= min_tr
        and c["quote_net_bps_total"] > 0.0
        and (c["pv_trades"] == 0 or c["pv_net_bps_total"] > 0.0)
    ]
    if eligible_base:
        best_base = max(
            eligible_base,
            key=lambda c: c["pv_clip_1000_usd"] if c["pv_trades"] > 0 else c["quote_clip_1000_usd"],
        )
        selection_status = "selected_positive_baseline"
    else:
        best_base = {"theta_bps": 6.0, "hold_ms": 10_000, "tau_ms": 120_000}
        selection_status = "no_positive_baseline_fallback_default"

    base_theta = float(best_base["theta_bps"])
    base_hold = int(best_base["hold_ms"])
    base_tau = int(best_base["tau_ms"])

    vol_candidates: list[dict[str, Any]] = []
    for mult in VOL_THETA_MULT_GRID:
        cfg = LeadLagArmParams(
            theta_bps=base_theta,
            tau_ms=base_tau,
            hold_ms=base_hold,
            taker_delay_ms=taker_delay_ms,
            warmup_ms=warmup_ms,
            vol_theta_mult=mult,
            taker_fee_bps=taker_fee_bps,
        )
        tot_tr = tot_pv = 0
        tot_q1000 = tot_pv1000 = 0.0
        for sig in pre_by_win.values():
            r = simulate_precomputed_arm(sig, cfg)
            tot_tr += r["trades"]
            tot_pv += r["print_verified_trades"]
            tot_q1000 += r["quote_clip_pnl_usd"]["1000"]
            tot_pv1000 += r["pv_clip_pnl_usd"]["1000"]
        vol_candidates.append(
            {"vol_theta_mult": mult, "trades": tot_tr, "pv_trades": tot_pv, "q1000": tot_q1000, "pv1000": tot_pv1000}
        )
    elig_vol = [c for c in vol_candidates if c["trades"] >= max(5, min_tr // 2)]
    best_vol_mult = (
        max(elig_vol, key=lambda c: c["pv1000"] if c["pv_trades"] > 0 else c["q1000"])["vol_theta_mult"]
        if elig_vol
        else 1.5
    )

    catch_candidates: list[dict[str, Any]] = []
    for frac in CATCHUP_FRAC_GRID:
        cfg = LeadLagArmParams(
            theta_bps=base_theta,
            tau_ms=base_tau,
            hold_ms=base_hold,
            taker_delay_ms=taker_delay_ms,
            warmup_ms=warmup_ms,
            exit_on_catchup_frac=frac,
            taker_fee_bps=taker_fee_bps,
        )
        tot_tr = tot_pv = 0
        tot_q1000 = tot_pv1000 = 0.0
        for sig in pre_by_win.values():
            r = simulate_precomputed_arm(sig, cfg)
            tot_tr += r["trades"]
            tot_pv += r["print_verified_trades"]
            tot_q1000 += r["quote_clip_pnl_usd"]["1000"]
            tot_pv1000 += r["pv_clip_pnl_usd"]["1000"]
        catch_candidates.append(
            {
                "exit_on_catchup_frac": frac,
                "trades": tot_tr,
                "pv_trades": tot_pv,
                "q1000": tot_q1000,
                "pv1000": tot_pv1000,
            }
        )
    elig_catch = [c for c in catch_candidates if c["trades"] >= max(5, min_tr // 2)]
    best_catch_frac = (
        max(elig_catch, key=lambda c: c["pv1000"] if c["pv_trades"] > 0 else c["q1000"])["exit_on_catchup_frac"]
        if elig_catch
        else 0.70
    )

    frozen = {
        "pair_id": pair.pair_id,
        "selection_status": selection_status,
        "theta_bps": base_theta,
        "hold_ms": base_hold,
        "tau_ms": base_tau,
        "vol_theta_mult": float(best_vol_mult),
        "exit_on_catchup_frac": float(best_catch_frac),
        "variants_tested_on_selection": len(baseline_candidates) + len(vol_candidates) + len(catch_candidates),
    }
    diagnostics = {
        "baseline_grid_count": len(baseline_candidates),
        "baseline_positive_count": len(eligible_base),
        "vol_candidates": vol_candidates,
        "catchup_candidates": catch_candidates,
    }
    return frozen, diagnostics


def run_duckdb_crypto_lead_lag(
    lakehouse_dir: Path,
    *,
    split: str = "both",
    pairs: Sequence[CryptoLeadLagPair] | None = None,
    frozen_params_path: Path | None = None,
    warmup_ms: int = DEFAULT_WARMUP_MS,
) -> dict[str, Any]:
    """Run read-only Hive parquet evaluation window-by-window via in-memory DuckDB (`gc.collect()` per window)."""
    import duckdb  # Lazy import so stdlib-only CI never requires duckdb at import time

    active_pairs = list(pairs) if pairs is not None else build_default_pairs()
    splits_map = resolve_crypto_splits(lakehouse_dir, split)
    dev_split_name = "selection_dev" if split == "selection_dev" else "full_dev"

    con = duckdb.connect(":memory:")
    con.execute("SET preserve_insertion_order=false; SET threads=2; SET memory_limit='2GB';")

    def fetch_window_books(
        win_info: dict[str, str],
        venue: str,
        symbol: str,
    ) -> tuple[list[tuple[int, float, float, float, float]], dict[str, Any]]:
        raw_mkt = VENUE_SYMBOL_MAP[venue][symbol]
        files = _campaign_parquet_files(
            lakehouse_dir, "bbo_quotes", venue, win_info["utc_date"], win_info["campaign_id"]
        )
        if not files:
            return [], {"raw_events": 0, "duration_ms": 0, "hours": 0.0, "max_gap_ms": 0, "is_continuous_10h": False}
        f_sql = "[" + ", ".join("'" + p.replace("'", "''") + "'" for p in files) + "]"
        if venue == "binance":
            stats = con.execute(
                f"""
                WITH b AS (
                    SELECT (available_at_ms // {STEP_MS}) AS bucket,
                           MAX(available_at_ms) AS ts_ms,
                           MIN(available_at_ms) AS min_ts,
                           COUNT(*) AS cnt
                    FROM read_parquet({f_sql})
                    WHERE market = ? AND tradable = true AND bid_px_f64 > 0 AND ask_px_f64 > bid_px_f64
                    GROUP BY 1
                ),
                g AS (
                    SELECT ts_ms, min_ts, cnt, LAG(ts_ms) OVER (ORDER BY bucket) AS prev_ts FROM b
                )
                SELECT COALESCE(SUM(cnt), 0), MIN(min_ts), MAX(ts_ms), COALESCE(MAX(ts_ms - prev_ts), 0)
                FROM g
                """,
                [raw_mkt],
            ).fetchone()
        else:
            stats = con.execute(
                f"""
                WITH s AS (
                    SELECT available_at_ms, LAG(available_at_ms) OVER (ORDER BY available_at_ms) AS prev_ts
                    FROM read_parquet({f_sql})
                    WHERE market = ? AND tradable = true AND bid_px_f64 > 0 AND ask_px_f64 > bid_px_f64
                )
                SELECT COUNT(*), MIN(available_at_ms), MAX(available_at_ms), COALESCE(MAX(available_at_ms - prev_ts), 0)
                FROM s
                """,
                [raw_mkt],
            ).fetchone()
        cnt, t0, t1, mgap = stats
        if not cnt or int(cnt) < 2 or t0 is None or t1 is None:
            return [], {"raw_events": int(cnt or 0), "duration_ms": 0, "hours": 0.0, "max_gap_ms": 0, "is_continuous_10h": False}
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
            SELECT MAX(available_at_ms) AS ts_ms,
                   arg_max(bid_px_f64, available_at_ms) AS bid_px,
                   arg_max(CAST(bid_sz AS DOUBLE), available_at_ms) AS bid_sz,
                   arg_max(ask_px_f64, available_at_ms) AS ask_px,
                   arg_max(CAST(ask_sz AS DOUBLE), available_at_ms) AS ask_sz
            FROM read_parquet({f_sql})
            WHERE market = ? AND tradable = true AND bid_px_f64 > 0 AND ask_px_f64 > bid_px_f64
            GROUP BY (available_at_ms // {STEP_MS})
            ORDER BY ts_ms ASC
            """,
            [raw_mkt],
        ).fetchall()
        return rows, cont

    def fetch_window_trades(
        win_info: dict[str, str],
        venue: str,
        symbol: str,
    ) -> list[tuple[int, int, float, float]]:
        raw_mkt = VENUE_SYMBOL_MAP[venue][symbol]
        files = _campaign_parquet_files(
            lakehouse_dir, "public_trades", venue, win_info["utc_date"], win_info["campaign_id"]
        )
        if not files:
            return []
        f_sql = "[" + ", ".join("'" + p.replace("'", "''") + "'" for p in files) + "]"
        rows = con.execute(
            f"""
            SELECT available_at_ms AS ts_ms,
                   CASE WHEN lower(aggressor_side) IN ('buy', 'b', 'bid') THEN 1
                        WHEN lower(aggressor_side) IN ('sell', 's', 'ask') THEN -1
                        ELSE 0 END AS sgn,
                   price_f64 AS px,
                   size_f64 AS sz
            FROM read_parquet({f_sql})
            WHERE market = ? AND NOT is_zero_price_transfer AND price_f64 > 0 AND size_f64 > 0
            ORDER BY available_at_ms ASC
            """,
            [raw_mkt],
        ).fetchall()
        return [(int(r[0]), int(r[1]), float(r[2]), float(r[3])) for r in rows if int(r[1]) != 0]

    def load_one_window(
        win_info: dict[str, str],
    ) -> tuple[
        dict[tuple[str, str], tuple[list[tuple[int, float, float, float, float]], dict[str, Any]]],
        dict[tuple[str, str], list[tuple[int, int, float, float]]],
    ]:
        needed_bbo: set[tuple[str, str]] = set()
        needed_trades: set[tuple[str, str]] = set()
        for p in active_pairs:
            needed_bbo.add((p.ref_venue, p.symbol))
            needed_bbo.add((p.confirm_venue, p.symbol))
            needed_bbo.add((p.tgt_venue, p.symbol))
            needed_trades.add((p.tgt_venue, p.symbol))
        win_bbo = {k: fetch_window_books(win_info, k[0], k[1]) for k in sorted(needed_bbo)}
        win_trades = {k: fetch_window_trades(win_info, k[0], k[1]) for k in sorted(needed_trades)}
        return win_bbo, win_trades

    out: dict[str, Any] = {
        "schema": "track01_multivenue_crypto_lead_lag_v2",
        "protocol": (
            "Full W01-W25 dynamic discovery: select & freeze parameters + regime cutoffs strictly on "
            f"{dev_split_name}; replay ONCE untouched on holdout. Strict HOLD, read-only."
        ),
        "venue_profiles": VENUE_PROFILES,
        "step_ms": STEP_MS,
        "warmup_ms": warmup_ms,
        "l1_depth_cap_frac": DEFAULT_L1_DEPTH_CAP_FRAC,
        "clips_usd": list(CLIPS_USD),
    }

    frozen_map: dict[str, dict[str, Any]] = {}
    regime_cutoffs: dict[str, dict[str, float]] = {}

    if split in ("selection_dev", "full_dev", "both"):
        dev_windows = splits_map[dev_split_name]
        # Pass 1 (window-streaming): accumulate baseline 12-grid stats across all continuous dev windows
        base_grid_acc: dict[str, dict[tuple[float, int, int], dict[str, Any]]] = {
            p.pair_id: {
                (th, h, t): {"trades": 0, "pv_trades": 0, "q_bps": 0.0, "pv_bps": 0.0, "q1000": 0.0, "pv1000": 0.0}
                for t in BASELINE_TAU_MS
                for h in BASELINE_HOLD_MS
                for th in BASELINE_THETAS_BPS
            }
            for p in active_pairs
        }
        cont_win_count: dict[str, int] = {p.pair_id: 0 for p in active_pairs}

        for win_id, win_info in sorted(dev_windows.items()):
            win_bbo, win_trades = load_one_window(win_info)
            for p in active_pairs:
                ref_rows, ref_cont = win_bbo[(p.ref_venue, p.symbol)]
                tgt_rows, tgt_cont = win_bbo[(p.tgt_venue, p.symbol)]
                conf_rows, _ = win_bbo[(p.confirm_venue, p.symbol)]
                tr_rows = win_trades[(p.tgt_venue, p.symbol)]
                cont_ok = (
                    ref_cont["is_continuous_10h"]
                    and tgt_cont["is_continuous_10h"]
                    and len(ref_rows) >= 200
                    and len(tgt_rows) >= 200
                )
                if not cont_ok:
                    continue
                sig = precompute_pair_signals(
                    ref_rows,
                    tgt_rows,
                    confirm_books=conf_rows if len(conf_rows) >= 200 else None,
                    trades=tr_rows,
                    warmup_ms=warmup_ms,
                )
                if sig is None:
                    continue
                cont_win_count[p.pair_id] += 1
                vp = VENUE_PROFILES[p.tgt_venue]
                for tau_ms in BASELINE_TAU_MS:
                    for hold_ms in BASELINE_HOLD_MS:
                        for theta_bps in BASELINE_THETAS_BPS:
                            cfg = LeadLagArmParams(
                                theta_bps=theta_bps,
                                tau_ms=tau_ms,
                                hold_ms=hold_ms,
                                taker_delay_ms=int(vp["taker_delay_ms"]),
                                warmup_ms=warmup_ms,
                                taker_fee_bps=float(vp["taker_fee_bps"]),
                            )
                            r = simulate_precomputed_arm(sig, cfg)
                            acc = base_grid_acc[p.pair_id][(theta_bps, hold_ms, tau_ms)]
                            acc["trades"] += r["trades"]
                            acc["pv_trades"] += r["print_verified_trades"]
                            acc["q_bps"] += r["quote_net_bps_total"]
                            acc["pv_bps"] += r["print_verified_net_bps_total"]
                            acc["q1000"] += r["quote_clip_pnl_usd"]["1000"]
                            acc["pv1000"] += r["pv_clip_pnl_usd"]["1000"]
            del win_bbo, win_trades
            gc.collect()

        chosen_base: dict[str, tuple[float, int, int, str, int, int]] = {}
        for p in active_pairs:
            min_tr = min(MIN_SELECTION_TRADES, max(5, 5 * cont_win_count[p.pair_id]))
            cands = base_grid_acc[p.pair_id]
            elig = [
                (k, v)
                for k, v in cands.items()
                if v["trades"] >= min_tr
                and v["q_bps"] > 0.0
                and (v["pv_trades"] == 0 or v["pv_bps"] > 0.0)
            ]
            if elig:
                best_k, _ = max(elig, key=lambda kv: kv[1]["pv1000"] if kv[1]["pv_trades"] > 0 else kv[1]["q1000"])
                chosen_base[p.pair_id] = (best_k[0], best_k[1], best_k[2], "selected_positive_baseline", len(cands), len(elig))
            else:
                chosen_base[p.pair_id] = (6.0, 10_000, 120_000, "no_positive_baseline_fallback_default", len(cands), 0)

        # Pass 2 (window-streaming): select vol_theta_mult & exit_on_catchup_frac on chosen_base
        vol_acc: dict[str, dict[float, dict[str, float]]] = {
            p.pair_id: {m: {"trades": 0.0, "pv_trades": 0.0, "q1000": 0.0, "pv1000": 0.0} for m in VOL_THETA_MULT_GRID}
            for p in active_pairs
        }
        catch_acc: dict[str, dict[float, dict[str, float]]] = {
            p.pair_id: {f: {"trades": 0.0, "pv_trades": 0.0, "q1000": 0.0, "pv1000": 0.0} for f in CATCHUP_FRAC_GRID}
            for p in active_pairs
        }

        for win_id, win_info in sorted(dev_windows.items()):
            win_bbo, win_trades = load_one_window(win_info)
            for p in active_pairs:
                ref_rows, ref_cont = win_bbo[(p.ref_venue, p.symbol)]
                tgt_rows, tgt_cont = win_bbo[(p.tgt_venue, p.symbol)]
                conf_rows, _ = win_bbo[(p.confirm_venue, p.symbol)]
                tr_rows = win_trades[(p.tgt_venue, p.symbol)]
                if not (ref_cont["is_continuous_10h"] and tgt_cont["is_continuous_10h"] and len(ref_rows) >= 200 and len(tgt_rows) >= 200):
                    continue
                b_theta, b_hold, b_tau, _, _, _ = chosen_base[p.pair_id]
                sig = precompute_pair_signals(
                    ref_rows,
                    tgt_rows,
                    confirm_books=conf_rows if len(conf_rows) >= 200 else None,
                    trades=tr_rows,
                    tau_values_ms=(b_tau,),
                    warmup_ms=warmup_ms,
                    min_cand_theta_bps=b_theta,
                )
                if sig is None:
                    continue
                vp = VENUE_PROFILES[p.tgt_venue]
                for mult in VOL_THETA_MULT_GRID:
                    cfg = LeadLagArmParams(
                        theta_bps=b_theta,
                        tau_ms=b_tau,
                        hold_ms=b_hold,
                        taker_delay_ms=int(vp["taker_delay_ms"]),
                        warmup_ms=warmup_ms,
                        vol_theta_mult=mult,
                        taker_fee_bps=float(vp["taker_fee_bps"]),
                    )
                    r = simulate_precomputed_arm(sig, cfg)
                    a = vol_acc[p.pair_id][mult]
                    a["trades"] += r["trades"]
                    a["pv_trades"] += r["print_verified_trades"]
                    a["q1000"] += r["quote_clip_pnl_usd"]["1000"]
                    a["pv1000"] += r["pv_clip_pnl_usd"]["1000"]
                for frac in CATCHUP_FRAC_GRID:
                    cfg = LeadLagArmParams(
                        theta_bps=b_theta,
                        tau_ms=b_tau,
                        hold_ms=b_hold,
                        taker_delay_ms=int(vp["taker_delay_ms"]),
                        warmup_ms=warmup_ms,
                        exit_on_catchup_frac=frac,
                        taker_fee_bps=float(vp["taker_fee_bps"]),
                    )
                    r = simulate_precomputed_arm(sig, cfg)
                    a = catch_acc[p.pair_id][frac]
                    a["trades"] += r["trades"]
                    a["pv_trades"] += r["print_verified_trades"]
                    a["q1000"] += r["quote_clip_pnl_usd"]["1000"]
                    a["pv1000"] += r["pv_clip_pnl_usd"]["1000"]
            del win_bbo, win_trades
            gc.collect()

        sel_diags: dict[str, Any] = {}
        for p in active_pairs:
            b_theta, b_hold, b_tau, status, n_base, n_pos = chosen_base[p.pair_id]
            min_tr = min(MIN_SELECTION_TRADES, max(5, 5 * cont_win_count[p.pair_id]))
            v_elig = [(m, a) for m, a in vol_acc[p.pair_id].items() if a["trades"] >= max(5, min_tr // 2)]
            best_vol = (
                max(v_elig, key=lambda kv: kv[1]["pv1000"] if kv[1]["pv_trades"] > 0 else kv[1]["q1000"])[0]
                if v_elig
                else 1.5
            )
            c_elig = [(f, a) for f, a in catch_acc[p.pair_id].items() if a["trades"] >= max(5, min_tr // 2)]
            best_catch = (
                max(c_elig, key=lambda kv: kv[1]["pv1000"] if kv[1]["pv_trades"] > 0 else kv[1]["q1000"])[0]
                if c_elig
                else 0.70
            )
            frozen_map[p.pair_id] = {
                "pair_id": p.pair_id,
                "selection_status": status,
                "theta_bps": b_theta,
                "hold_ms": b_hold,
                "tau_ms": b_tau,
                "vol_theta_mult": float(best_vol),
                "exit_on_catchup_frac": float(best_catch),
                "variants_tested_on_selection": n_base + len(VOL_THETA_MULT_GRID) + len(CATCHUP_FRAC_GRID),
            }
            sel_diags[p.pair_id] = {
                "baseline_grid_count": n_base,
                "baseline_positive_count": n_pos,
                "cont_windows_in_dev": cont_win_count[p.pair_id],
            }
        out["frozen_params"] = frozen_map
        out["selection_diagnostics"] = sel_diags

    def evaluate_split_streaming(split_name: str, win_map: dict[str, dict[str, str]], cutoffs_in: dict[str, dict[str, float]] | None) -> tuple[dict[str, Any], dict[str, dict[str, float]]]:
        raw_rows: list[dict[str, Any]] = []
        for win_id, win_info in sorted(win_map.items()):
            win_bbo, win_trades = load_one_window(win_info)
            for p in active_pairs:
                vp = VENUE_PROFILES[p.tgt_venue]
                ref_rows, ref_cont = win_bbo[(p.ref_venue, p.symbol)]
                tgt_rows, tgt_cont = win_bbo[(p.tgt_venue, p.symbol)]
                conf_rows, conf_cont = win_bbo[(p.confirm_venue, p.symbol)]
                tr_rows = win_trades[(p.tgt_venue, p.symbol)]
                if len(ref_rows) < 200 or len(tgt_rows) < 200:
                    raw_rows.append(
                        {
                            "split": split_name,
                            "win": win_id,
                            "campaign_id": win_info["campaign_id"],
                            "pair_id": p.pair_id,
                            "tgt_venue": p.tgt_venue,
                            "symbol": p.symbol,
                            "ref_venue": p.ref_venue,
                            "confirm_venue": p.confirm_venue,
                            "settlement": vp["settlement"],
                            "account_fee_status": vp["account_fee_status"],
                            "taker_fee_bps": vp["taker_fee_bps"],
                            "cont": False,
                            "hours_covered": 0.0,
                            "continuity": {"ref": ref_cont, "tgt": tgt_cont, "confirm": conf_cont},
                            "skipped": "missing_books",
                        }
                    )
                    continue
                fr = frozen_map[p.pair_id]
                sig = precompute_pair_signals(
                    ref_rows,
                    tgt_rows,
                    confirm_books=conf_rows if len(conf_rows) >= 200 else None,
                    trades=tr_rows,
                    tau_values_ms=(int(fr["tau_ms"]),),
                    warmup_ms=warmup_ms,
                    min_cand_theta_bps=float(fr["theta_bps"]),
                )
                if sig is None:
                    raw_rows.append(
                        {
                            "split": split_name,
                            "win": win_id,
                            "campaign_id": win_info["campaign_id"],
                            "pair_id": p.pair_id,
                            "tgt_venue": p.tgt_venue,
                            "symbol": p.symbol,
                            "ref_venue": p.ref_venue,
                            "confirm_venue": p.confirm_venue,
                            "settlement": vp["settlement"],
                            "account_fee_status": vp["account_fee_status"],
                            "taker_fee_bps": vp["taker_fee_bps"],
                            "cont": False,
                            "hours_covered": 0.0,
                            "continuity": {"ref": ref_cont, "tgt": tgt_cont, "confirm": conf_cont},
                            "skipped": "insufficient_overlap",
                        }
                    )
                    continue
                cont_ok = bool(ref_cont["is_continuous_10h"] and tgt_cont["is_continuous_10h"])
                hours_cov = round((sig.t1_ms - sig.t0_ms) / 3_600_000.0, 3)
                arm_cfgs = _arm_configs_from_frozen(p, fr, warmup_ms=warmup_ms)
                for arm_name, cfg in arm_cfgs.items():
                    sim_res = simulate_precomputed_arm(sig, cfg)
                    raw_rows.append(
                        {
                            "split": split_name,
                            "win": win_id,
                            "campaign_id": win_info["campaign_id"],
                            "arm": arm_name,
                            "pair_id": p.pair_id,
                            "tgt_venue": p.tgt_venue,
                            "symbol": p.symbol,
                            "ref_venue": p.ref_venue,
                            "confirm_venue": p.confirm_venue,
                            "settlement": vp["settlement"],
                            "account_fee_status": vp["account_fee_status"],
                            "taker_fee_bps": vp["taker_fee_bps"],
                            "taker_delay_ms": vp["taker_delay_ms"],
                            "theta_bps": cfg.theta_bps,
                            "hold_ms": cfg.hold_ms,
                            "tau_ms": cfg.tau_ms,
                            "vol_theta_mult": cfg.vol_theta_mult,
                            "require_dual_leader_confirm": cfg.require_dual_leader_confirm,
                            "exit_on_catchup_frac": cfg.exit_on_catchup_frac,
                            "cont": cont_ok,
                            "hours_covered": hours_cov,
                            "tgt_realized_1s_vol_bps": sig.tgt_realized_1s_vol_bps,
                            "ref_realized_1s_vol_bps": sig.ref_realized_1s_vol_bps,
                            "continuity": {"ref": ref_cont, "tgt": tgt_cont, "confirm": conf_cont},
                            "public_trades_count": len(tr_rows),
                            **sim_res,
                        }
                    )
            del win_bbo, win_trades
            gc.collect()

        cutoffs_used = cutoffs_in if cutoffs_in is not None else _compute_regime_cutoffs_from_dev(raw_rows)
        for r in raw_rows:
            if not r.get("skipped"):
                r["regime"] = _classify_regime(r["symbol"], float(r.get("tgt_realized_1s_vol_bps", 0.0)), cutoffs_used)

        return (
            {
                "split": split_name,
                "windows": win_map,
                "regime_cutoffs_from_dev": cutoffs_used,
                "rows": raw_rows,
                "summary_all": _aggregate_arm_rows(raw_rows, cont_only=False),
                "summary_cont_only": _aggregate_arm_rows(raw_rows, cont_only=True),
            },
            cutoffs_used,
        )

    if split in ("selection_dev", "full_dev", "both"):
        dev_res, regime_cutoffs = evaluate_split_streaming(dev_split_name, splits_map[dev_split_name], None)
        out["regime_cutoffs"] = regime_cutoffs
        out[dev_split_name] = dev_res

    if split in ("holdout", "both"):
        if not frozen_map:
            if frozen_params_path is None:
                raise ValueError("--frozen-params is required when running --split holdout alone")
            raw_fr = json.loads(frozen_params_path.read_text(encoding="utf-8"))
            frozen_map = raw_fr["frozen_params"] if "frozen_params" in raw_fr else raw_fr
            regime_cutoffs = raw_fr.get("regime_cutoffs", {})
            out["frozen_params"] = frozen_map
            out["regime_cutoffs"] = regime_cutoffs
        hold_res, _ = evaluate_split_streaming("holdout", splits_map["holdout"], regime_cutoffs)
        out["holdout"] = hold_res

    con.close()
    return out


def run_self_test() -> dict[str, Any]:
    """Run synthetic pure-stdlib tests verifying causal mechanics, precomputed parity, and accounting invariants."""
    ref_books: list[tuple[int, float, float, float, float]] = []
    tgt_books: list[tuple[int, float, float, float, float]] = []
    conf_books: list[tuple[int, float, float, float, float]] = []
    trades: list[tuple[int, int, float, float]] = []
    for k in range(100):
        t = k * 250
        r_mid = 100.10 if 20 <= k < 36 else 100.00
        c_mid = 100.06 if 20 <= k < 36 else 100.00
        t_mid = 100.09 if 24 <= k < 36 else 100.00
        ref_books.append((t, r_mid - 0.005, 10.0, r_mid + 0.005, 10.0))
        conf_books.append((t, c_mid - 0.005, 10.0, c_mid + 0.005, 10.0))
        tgt_books.append((t, t_mid - 0.005, 4.0, t_mid + 0.005, 4.0))  # ~$400 L1 depth
        if k == 21:
            trades.append((t, 1, 100.005, 2.0))
        if k == 30:
            trades.append((t, -1, 100.085, 2.0))

    cfg_base = LeadLagArmParams(
        theta_bps=4.0,
        tau_ms=20_000,
        hold_ms=2_000,
        taker_delay_ms=200,
        warmup_ms=2_000,
        taker_fee_bps=0.0,
    )
    r_base = simulate_lead_lag_streams(ref_books, tgt_books, cfg_base, trades=trades)
    assert r_base["trades"] >= 1, f"expected >= 1 trade, got {r_base}"
    assert r_base["quote_net_bps_mean"] > 0.0, f"expected positive quote net, got {r_base}"

    sig = precompute_pair_signals(
        ref_books, tgt_books, confirm_books=conf_books, trades=trades, tau_values_ms=(20_000,), warmup_ms=2_000
    )
    assert sig is not None
    r_pre = simulate_precomputed_arm(sig, cfg_base)
    assert r_pre == r_base, f"precomputed parity mismatch: {r_pre} vs {r_base}"

    cfg_catch = dataclasses.replace(cfg_base, exit_on_catchup_frac=0.70, hold_ms=10_000)
    r_catch = simulate_lead_lag_streams(ref_books, tgt_books, cfg_catch, trades=trades)
    r_catch_pre = simulate_precomputed_arm(sig, cfg_catch)
    assert r_catch["early_catchup_exits"] >= 1, f"expected early catchup exit, got {r_catch}"
    assert r_catch_pre == r_catch, f"catchup precomputed mismatch: {r_catch_pre} vs {r_catch}"

    flat_conf = [(b[0], 99.995, 10.0, 100.005, 10.0) for b in ref_books]
    cfg_dual = dataclasses.replace(cfg_base, require_dual_leader_confirm=True)
    r_dual_reject = simulate_lead_lag_streams(ref_books, tgt_books, cfg_dual, confirm_books=flat_conf)
    assert r_dual_reject["trades"] == 0, f"expected dual leader rejection, got {r_dual_reject}"

    r_dual_agree = simulate_lead_lag_streams(ref_books, tgt_books, cfg_dual, confirm_books=conf_books, trades=trades)
    r_dual_agree_pre = simulate_precomputed_arm(sig, cfg_dual)
    assert r_dual_agree["trades"] >= 1
    assert r_dual_agree_pre == r_dual_agree

    assert math.isclose(
        r_base["quote_clip_pnl_usd"]["500"],
        r_base["quote_clip_pnl_usd"]["1000"],
        abs_tol=1e-6,
    )

    return {
        "self_test": "ok",
        "baseline_trades": r_base["trades"],
        "baseline_net_bps": r_base["quote_net_bps_mean"],
        "catchup_exits": r_catch["early_catchup_exits"],
        "dual_reject_trades": r_dual_reject["trades"],
        "dual_agree_trades": r_dual_agree["trades"],
        "precomputed_parity": "exact",
        "l1_capped_clip_500_usd": r_base["quote_clip_pnl_usd"]["500"],
        "l1_capped_clip_1000_usd": r_base["quote_clip_pnl_usd"]["1000"],
    }


def main(argv: Sequence[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description="Track 01 Multi-Venue 10h Crypto Lead-Lag Evaluator")
    ap.add_argument("--lakehouse", type=Path, default=Path(os.path.expanduser("~/aleph-tx-lakehouse")))
    ap.add_argument("--split", choices=("selection_dev", "full_dev", "holdout", "both"), default="both")
    ap.add_argument("--frozen-params", type=Path, default=None, help="Frozen params JSON for standalone --split holdout")
    ap.add_argument("--venues", type=str, default=None, help="Comma-separated subset of target venues")
    ap.add_argument("--symbols", type=str, default=None, help="Comma-separated subset of symbols (BTC,ETH,SOL)")
    ap.add_argument("--warmup-ms", type=int, default=DEFAULT_WARMUP_MS)
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

    pairs = build_default_pairs()
    if args.venues:
        vset = {v.strip() for v in args.venues.split(",") if v.strip()}
        pairs = [p for p in pairs if p.tgt_venue in vset]
    if args.symbols:
        sset = {s.strip().upper() for s in args.symbols.split(",") if s.strip()}
        pairs = [p for p in pairs if p.symbol in sset]

    res = run_duckdb_crypto_lead_lag(
        args.lakehouse,
        split=args.split,
        pairs=pairs,
        frozen_params_path=args.frozen_params,
        warmup_ms=args.warmup_ms,
    )
    rendered = json.dumps(res, indent=2, allow_nan=False)
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(rendered + "\n", encoding="utf-8")
    else:
        print(rendered)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
