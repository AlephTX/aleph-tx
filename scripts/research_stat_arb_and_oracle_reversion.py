#!/usr/bin/env python3
"""Speed-Bump-Immune 1m-4h Statistical Arbitrage & Perp-vs-Oracle Dislocation Research.

100% pure-stdlib at module top level (`duckdb` is imported lazily inside lakehouse loader,
runnable under `python3 -S`).

Implements exact Python mirrors of `src/strategy/stat_arb.rs`:
  - Family A (`PairsStatArbPy`): causal online EWMA OLS hedge ratio `beta_t`, rolling
    spread Z-score, causal AR(1) persistence `phi` / half-life stationarity gate,
    configurable entry/exit/stop Z-thresholds, and 4h max hold time stop.
    Evaluates 15 multi-asset pairs across `{15m, 60m, 240m}` lookbacks x `{2.0, 2.5, 3.0}` Z-entry.
  - Family B (`OracleDislocationPy`): causal perp-vs-oracle dislocation (demeaned by
    slow EWMA equilibrium) vs spread-adjusted hurdle `max(min_disloc, spread_mult * spread)`,
    funding-alignment filter, convergence exit, and 1h time stop across ALL 18
    `lighter_robinhood` and 19 `lighter` markets.

Strict execution realism across `W01-W26` (`selection_dev w01-w20` -> untouched `holdout w21-w25`
-> `w26_oos`, explicitly excluding `w27+`):
  - Mode 1 (`taker`): real L1 `bid/ask` crossing at `t_decision + 330 ms`
  - Mode 2 (`maker_postonly`): strict `public_trades` print-verified `PostOnly` at `200 ms` delay
    (with bounded IOC fallback only on exit / orphan hedge leg)
  - Displayed L1 depth clamping (`<= 0.5 * L1_depth`) on both entry & exit
  - Hourly `funding_and_ctx` cash accrual (`-pos_usd * funding_rate_bps * 1e-4 * dt_hours`)
  - Explicit terminal flatten + residual accounting (`terminal_flat_all`, `terminal_residual_notional_usd`)
  - Fee models via `track_fee_evidence.py`: `lighter_robinhood` (`0/0` USDG verified),
    `lighter` (`0/0` Standard counterfactual + `0.40/2.80` Premium verified comparison),
    plus `+1/+2 bps` fee stress, top-1 exclusion concentration checks, and PSR/N_eff/DSR/PBO.
"""

from __future__ import annotations

import argparse
import bisect
import dataclasses
import gc
import glob
import importlib.util
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
BBO_BUCKET_MS = 500
STEP_MS = 1_000
TAKER_DELAY_MS = 330
MAKER_DELAY_MS = 200
MAKER_CHASE_WINDOW_MS = 10_000
L1_DEPTH_CAP_FRAC = 0.50
CLIP_USD_DEFAULT = 500.0
DEFAULT_WARMUP_MS = 15 * 60_000
DEFAULT_MAX_QUOTE_AGE_MS = 60_000
DEFAULT_MAX_ORACLE_AGE_MS = 60_000
DEFAULT_MAX_GAP_MS = 60_000

# Verified full market ID maps (confirmed on Aleph w01-w26)
LIGHTER_ROBINHOOD_MARKETS: dict[str, str] = {
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
}

LIGHTER_MARKETS: dict[str, str] = {
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
}

VENUE_MARKETS: dict[str, dict[str, str]] = {
    "lighter_robinhood": LIGHTER_ROBINHOOD_MARKETS,
    "lighter": LIGHTER_MARKETS,
}

# Family A: Multi-Asset Stat-Arb Pairs (Y, X, cluster)
STAT_ARB_PAIRS: tuple[tuple[str, str, str], ...] = (
    # Crypto-Equity
    ("MSTR", "BTC", "crypto_equity"),
    ("COIN", "BTC", "crypto_equity"),
    ("CRCL", "ETH", "crypto_equity"),
    # Semi / AI
    ("NVDA", "AMD", "semi_ai"),
    ("NVDA", "MU", "semi_ai"),
    ("AMD", "QQQ", "semi_ai"),
    ("NVDA", "QQQ", "semi_ai"),
    # Tech / Index
    ("AAPL", "QQQ", "tech_index"),
    ("MSFT", "QQQ", "tech_index"),
    ("GOOGL", "QQQ", "tech_index"),
    ("META", "AMZN", "tech_index"),
    ("SPY", "QQQ", "tech_index"),
    # Crypto
    ("ETH", "BTC", "crypto"),
    ("SOL", "BTC", "crypto"),
    ("SOL", "ETH", "crypto"),
)

# Family A parameter grid (swept strictly on selection_dev w01-w20)
PAIRS_LOOKBACK_GRID_MS: tuple[int, ...] = (15 * 60_000, 60 * 60_000, 240 * 60_000)
PAIRS_Z_ENTRY_GRID: tuple[float, ...] = (2.0, 2.5, 3.0)

# Family B parameter grid (swept strictly on selection_dev w01-w20)
ORACLE_MIN_DISLOC_GRID: tuple[float, ...] = (3.0, 5.0, 8.0, 12.0)
ORACLE_SPREAD_MULT_GRID: tuple[float, ...] = (1.0, 1.5, 2.0)
ORACLE_FUNDING_FILTER_GRID: tuple[bool, ...] = (False, True)


def _load_local_module(mod_name: str, filename: str) -> Any:
    """Import sibling pure-stdlib research module cleanly under any cwd."""
    if mod_name in sys.modules:
        return sys.modules[mod_name]
    path = Path(__file__).resolve().parent / filename
    spec = importlib.util.spec_from_file_location(mod_name, path)
    if spec is None or spec.loader is None:
        raise ImportError(f"Unable to load {path}")
    mod = importlib.util.module_from_spec(spec)
    sys.modules[mod_name] = mod
    spec.loader.exec_module(mod)
    return mod


def resolve_all_fee_models(evidence_path: Path | None) -> dict[str, Any]:
    """Load OPT-01 fee evidence via `track_fee_evidence.py` with counterfactuals."""
    tfe = _load_local_module("track_fee_evidence", "track_fee_evidence.py")
    ev = tfe.load_track_fee_evidence(evidence_path if (evidence_path and evidence_path.is_file()) else None)
    rh_mk, rh_tk, rh_status, rh_src = tfe.resolve_venue_fee(
        "lighter_robinhood",
        "rwa",
        default_maker_bps=0.0,
        default_taker_bps=0.0,
        default_status="verified_standard_zero",
        evidence=ev,
    )
    lt_mk, lt_tk, lt_status, lt_src = tfe.resolve_venue_fee(
        "lighter",
        "rwa",
        default_maker_bps=0.4,
        default_taker_bps=2.8,
        default_status="verified_account_tier",
        evidence=ev,
    )
    prov = tfe.build_fee_provenance(ev, venues_in_scope=("lighter_robinhood", "lighter"))
    return {
        "provenance": prov,
        "lighter_robinhood": {
            "primary_0_0": {"maker_bps": rh_mk, "taker_bps": rh_tk, "status": rh_status, "source": rh_src},
        },
        "lighter": {
            "standard_0_0_counterfactual": {
                "maker_bps": 0.0,
                "taker_bps": 0.0,
                "status": "counterfactual_standard_zero",
                "source": "lighter_standard_0_0_scenario",
            },
            "premium_verified": {
                "maker_bps": lt_mk,
                "taker_bps": lt_tk,
                "status": lt_status,
                "source": lt_src,
            },
        },
    }


def compute_multiple_testing_metrics(trial_series: dict[str, Sequence[float]], num_blocks: int = 8) -> dict[str, Any]:
    """Run PSR / N_eff / DSR / MinTRL / CSCV PBO via `audit_multiple_testing_overfitting.py`."""
    if not trial_series:
        return {"status": "empty"}
    amto = _load_local_module("audit_multiple_testing_overfitting", "audit_multiple_testing_overfitting.py")
    names = sorted(trial_series.keys())
    t_len = len(trial_series[names[0]])
    if t_len < 3:
        return {"status": "insufficient_windows", "windows": t_len}
    # Filter out strictly constant zero series for CSCV stability while keeping count
    varying = {k: list(v) for k, v in trial_series.items() if len(set(v)) > 1}
    if not varying:
        return {"status": "all_constant_zero", "windows": t_len, "num_trials_raw": len(names)}
    dsr = amto.deflated_sharpe_report(varying)
    pbo_res: dict[str, Any] | None = None
    eff_blocks = num_blocks
    if t_len < eff_blocks * 2:
        eff_blocks = max(4, (t_len // 2) - ((t_len // 2) % 2))
    if len(varying) >= 2 and t_len >= eff_blocks * 2 and eff_blocks >= 4:
        pbo_res = amto.cscv_pbo(varying, num_blocks=eff_blocks)
    return {
        "status": "ok",
        "windows": t_len,
        "num_trials_total": len(names),
        "num_trials_varying": len(varying),
        "dsr": dsr,
        "cscv": pbo_res,
    }


# ---------------------------------------------------------------------------
# Causal Signal Engines (Exact Python Mirrors of `src/strategy/stat_arb.rs`)
# ---------------------------------------------------------------------------


@dataclasses.dataclass(frozen=True)
class PairsStatArbCfg:
    ewma_tau_ms: int = 3_600_000
    warmup_ms: int = DEFAULT_WARMUP_MS
    min_samples: int = 30
    ou_window: int = 64
    ar_sample_ms: int = 10_000
    max_quote_age_ms: int = DEFAULT_MAX_QUOTE_AGE_MS
    max_gap_ms: int = DEFAULT_MAX_GAP_MS
    beta_min: float = 0.10
    beta_max: float = 5.00
    z_entry: float = 2.50
    z_exit: float = 0.25
    z_stop: float = 4.50
    phi_max_stationary: float = 0.985
    max_hold_ms: int = 14_400_000
    min_spread_vol_bps: float = 2.0
    max_combined_spread_bps: float = 25.0
    use_phi_gate: bool = True


class _Ar1RingPy:
    def __init__(self, window: int) -> None:
        self.window = int(window)
        self.residuals: list[float] = []
        self.dt_secs: list[float] = []
        self.last_sample_ms: int | None = None

    def reset(self) -> None:
        self.residuals.clear()
        self.dt_secs.clear()
        self.last_sample_ms = None

    def maybe_push(self, now_ms: int, value: float, min_cadence_ms: int) -> None:
        if not math.isfinite(value):
            return
        if self.last_sample_ms is None:
            dt_sec = 0.0
        else:
            dt_ms = now_ms - self.last_sample_ms
            if dt_ms < min_cadence_ms:
                return
            dt_sec = dt_ms / 1000.0
        self.residuals.append(value)
        self.dt_secs.append(dt_sec)
        if len(self.residuals) > self.window:
            self.residuals.pop(0)
            self.dt_secs.pop(0)
        self.last_sample_ms = now_ms

    def estimate(self, min_samples: int, phi_max_stationary: float) -> tuple[float | None, float | None, str]:
        n = len(self.residuals)
        if n < min_samples or n < 2:
            return None, None, "warming_up"
        sum_xx = 0.0
        sum_xy = 0.0
        sum_dt = 0.0
        pair_count = 0
        for i in range(1, n):
            xp = self.residuals[i - 1]
            xc = self.residuals[i]
            dt = self.dt_secs[i]
            if dt > 0.0 and math.isfinite(xp) and math.isfinite(xc) and math.isfinite(dt):
                sum_xx += xp * xp
                sum_xy += xp * xc
                sum_dt += dt
                pair_count += 1
        if pair_count == 0 or sum_dt <= 0.0 or sum_xx <= 1e-12:
            return None, None, "warming_up"
        phi = sum_xy / sum_xx
        if not math.isfinite(phi) or phi <= 0.0 or phi >= phi_max_stationary:
            return (phi if math.isfinite(phi) else None), None, "non_stationary"
        mean_dt_sec = sum_dt / pair_count
        theta_per_sec = -math.log(phi) / mean_dt_sec
        if not math.isfinite(theta_per_sec) or theta_per_sec <= 0.0:
            return phi, None, "non_stationary"
        hl_ms = (math.log(2.0) / theta_per_sec) * 1000.0
        if not math.isfinite(hl_ms) or hl_ms <= 0.0:
            return phi, None, "non_stationary"
        return phi, hl_ms, "stationary"


class PairsSignalFilterPy:
    """Causal online EWMA OLS + AR(1) stationarity filter matching `PairsStatArb`."""

    def __init__(self, cfg: PairsStatArbCfg) -> None:
        self.cfg = cfg
        self.initialized = False
        self.first_obs_ms: int | None = None
        self.last_obs_ms: int | None = None
        self.mu_y = 0.0
        self.mu_x = 0.0
        self.var_x = 0.0
        self.cov_xy = 0.0
        self.beta = max(cfg.beta_min, min(cfg.beta_max, 1.0))
        self.spread_var_bps2 = 0.0
        self.ar_ring = _Ar1RingPy(cfg.ou_window)

    def reset(self) -> None:
        self.initialized = False
        self.first_obs_ms = None
        self.last_obs_ms = None
        self.mu_y = 0.0
        self.mu_x = 0.0
        self.var_x = 0.0
        self.cov_xy = 0.0
        self.beta = max(self.cfg.beta_min, min(self.cfg.beta_max, 1.0))
        self.spread_var_bps2 = 0.0
        self.ar_ring.reset()

    def step(
        self,
        now_ms: int,
        qy: tuple[int, float, float, float, float] | None,
        qx: tuple[int, float, float, float, float] | None,
    ) -> tuple[bool, str, float, float, float, float | None]:
        """Return `(valid, status, beta, z_score, combined_spread_bps, phi)`."""
        if self.last_obs_ms is not None and now_ms < self.last_obs_ms:
            return False, "clock_regression", self.beta, 0.0, 0.0, None
        if qy is None or qx is None:
            return False, "stale_quote", self.beta, 0.0, 0.0, None
        y_ts, y_bid, _, y_ask, _ = qy
        x_ts, x_bid, _, x_ask, _ = qx
        if y_ts > now_ms or x_ts > now_ms:
            return False, "invalid_input", self.beta, 0.0, 0.0, None
        if not (math.isfinite(y_bid) and math.isfinite(y_ask) and y_bid > 0.0 and y_ask > y_bid):
            return False, "invalid_input", self.beta, 0.0, 0.0, None
        if not (math.isfinite(x_bid) and math.isfinite(x_ask) and x_bid > 0.0 and x_ask > x_bid):
            return False, "invalid_input", self.beta, 0.0, 0.0, None
        if (now_ms - y_ts) > self.cfg.max_quote_age_ms or (now_ms - x_ts) > self.cfg.max_quote_age_ms:
            return False, "stale_quote", self.beta, 0.0, 0.0, None

        mid_y = 0.5 * (y_bid + y_ask)
        mid_x = 0.5 * (x_bid + x_ask)
        spr_y = ((y_ask - y_bid) / mid_y) * 1e4
        spr_x = ((x_ask - x_bid) / mid_x) * 1e4
        log_y = math.log(mid_y)
        log_x = math.log(mid_x)

        if self.last_obs_ms is not None and (now_ms - self.last_obs_ms) > self.cfg.max_gap_ms:
            self.reset()

        if not self.initialized:
            self.initialized = True
            self.first_obs_ms = now_ms
            self.last_obs_ms = now_ms
            self.mu_y = log_y
            self.mu_x = log_x
            spread_bps = 0.0
            self.ar_ring.maybe_push(now_ms, spread_bps, self.cfg.ar_sample_ms)
        else:
            dt_ms = now_ms - (self.last_obs_ms if self.last_obs_ms is not None else now_ms)
            dx = log_x - self.mu_x
            dy = log_y - self.mu_y
            spread_bps = (dy - self.beta * dx) * 1e4
            if dt_ms > 0:
                alpha = 1.0 - math.exp(-dt_ms / float(self.cfg.ewma_tau_ms))
                one_minus_a = 1.0 - alpha
                self.mu_x += alpha * dx
                self.mu_y += alpha * dy
                self.var_x = max(0.0, one_minus_a * (self.var_x + alpha * dx * dx))
                self.cov_xy = one_minus_a * (self.cov_xy + alpha * dx * dy)
                if self.var_x > 1e-12 and math.isfinite(self.cov_xy):
                    self.beta = max(self.cfg.beta_min, min(self.cfg.beta_max, self.cov_xy / self.var_x))
                self.spread_var_bps2 = max(
                    0.0, one_minus_a * self.spread_var_bps2 + alpha * spread_bps * spread_bps
                )
                self.ar_ring.maybe_push(now_ms, spread_bps, self.cfg.ar_sample_ms)
                self.last_obs_ms = now_ms

        vol_bps = max(math.sqrt(self.spread_var_bps2), self.cfg.min_spread_vol_bps)
        z_score = spread_bps / vol_bps
        comb_spr = spr_y + self.beta * spr_x
        phi, _, st = self.ar_ring.estimate(self.cfg.min_samples, self.cfg.phi_max_stationary)
        warmed = self.first_obs_ms is not None and (now_ms - self.first_obs_ms) >= self.cfg.warmup_ms
        if not warmed or st == "warming_up":
            return True, "warming_up", self.beta, z_score, comb_spr, phi
        if self.cfg.use_phi_gate and st != "stationary":
            return True, "non_stationary", self.beta, z_score, comb_spr, phi
        return True, "ready", self.beta, z_score, comb_spr, phi


@dataclasses.dataclass(frozen=True)
class OracleDislocCfg:
    eq_ewma_tau_ms: int = 3_600_000
    warmup_ms: int = DEFAULT_WARMUP_MS
    min_samples: int = 20
    max_perp_age_ms: int = DEFAULT_MAX_QUOTE_AGE_MS
    max_oracle_age_ms: int = DEFAULT_MAX_ORACLE_AGE_MS
    max_gap_ms: int = DEFAULT_MAX_GAP_MS
    min_dislocation_bps: float = 5.0
    spread_mult: float = 1.5
    max_dislocation_bps: float = 80.0
    exit_dislocation_bps: float = 0.5
    stop_dislocation_bps: float = 100.0
    max_hold_ms: int = 3_600_000
    require_funding_alignment: bool = False
    max_adverse_funding_bps: float = 0.5
    max_entry_spread_bps: float = 20.0


class OracleSignalFilterPy:
    """Causal slow-equilibrium oracle dislocation filter matching `OracleDislocationReversion`."""

    def __init__(self, cfg: OracleDislocCfg) -> None:
        self.cfg = cfg
        self.initialized = False
        self.first_obs_ms: int | None = None
        self.last_obs_ms: int | None = None
        self.eq_bps = 0.0
        self.sample_count = 0

    def reset(self) -> None:
        self.initialized = False
        self.first_obs_ms = None
        self.last_obs_ms = None
        self.eq_bps = 0.0
        self.sample_count = 0

    def step(
        self,
        now_ms: int,
        quote: tuple[int, float, float, float, float] | None,
        oracle: tuple[int, float, float] | None,
    ) -> tuple[bool, str, float, float, float]:
        """Return `(valid, status, disloc_bps, spread_bps, funding_rate_bps)`."""
        if self.last_obs_ms is not None and now_ms < self.last_obs_ms:
            return False, "clock_regression", 0.0, 0.0, 0.0
        if quote is None:
            return False, "stale_quote", 0.0, 0.0, 0.0
        if oracle is None:
            return False, "stale_oracle", 0.0, 0.0, 0.0

        q_ts, bid_px, _, ask_px, _ = quote
        o_ts, oracle_px, fnd_bps = oracle
        if q_ts > now_ms or o_ts > now_ms:
            return False, "invalid_input", 0.0, 0.0, 0.0
        if not (math.isfinite(bid_px) and math.isfinite(ask_px) and bid_px > 0.0 and ask_px > bid_px):
            return False, "invalid_input", 0.0, 0.0, 0.0
        if not (math.isfinite(oracle_px) and oracle_px > 0.0 and math.isfinite(fnd_bps)):
            return False, "invalid_input", 0.0, 0.0, 0.0
        if (now_ms - q_ts) > self.cfg.max_perp_age_ms:
            return False, "stale_quote", 0.0, 0.0, fnd_bps
        if (now_ms - o_ts) > self.cfg.max_oracle_age_ms:
            return False, "stale_oracle", 0.0, 0.0, fnd_bps

        mid_px = 0.5 * (bid_px + ask_px)
        spread_bps = ((ask_px - bid_px) / mid_px) * 1e4
        raw_disloc = ((mid_px - oracle_px) / oracle_px) * 1e4

        if self.last_obs_ms is not None and (now_ms - self.last_obs_ms) > self.cfg.max_gap_ms:
            self.reset()

        if not self.initialized:
            self.initialized = True
            self.first_obs_ms = now_ms
            self.last_obs_ms = now_ms
            self.eq_bps = raw_disloc
            self.sample_count = 1
            disloc_bps = 0.0
        else:
            dt_ms = now_ms - (self.last_obs_ms if self.last_obs_ms is not None else now_ms)
            disloc_bps = raw_disloc - self.eq_bps
            if dt_ms > 0:
                alpha = 1.0 - math.exp(-dt_ms / float(self.cfg.eq_ewma_tau_ms))
                self.eq_bps += alpha * (raw_disloc - self.eq_bps)
                self.last_obs_ms = now_ms
                self.sample_count += 1

        warmed = (
            self.first_obs_ms is not None
            and (now_ms - self.first_obs_ms) >= self.cfg.warmup_ms
            and self.sample_count >= self.cfg.min_samples
        )
        return True, ("ready" if warmed else "warming_up"), disloc_bps, spread_bps, fnd_bps


# ---------------------------------------------------------------------------
# Lakehouse Discovery & DuckDB Loader (W01..W26 ONLY; W27+ STRICTLY EXCLUDED)
# ---------------------------------------------------------------------------


def discover_windows_w01_w26(lakehouse_dir: Path) -> dict[str, dict[str, Any]]:
    """Discover all `w01..w26` window rosters across Tokyo venues (excluding `w08` and `w27+`)."""
    base = lakehouse_dir / "parquet" / "bbo_quotes" / "collector_region=tokyo"
    by_win: dict[int, dict[str, Any]] = {}
    for venue in ("lighter", "lighter_robinhood"):
        v_dir = base / f"venue={venue}"
        if not v_dir.is_dir():
            continue
        for cid_dir in glob.glob(str(v_dir / "utc_date=*" / "campaign_id=tokyo-*")):
            parts = Path(cid_dir).parts
            cid = parts[-1].split("=", 1)[1]
            utc_date = parts[-2].split("=", 1)[1]
            m = re.match(r"^tokyo-(crypto|rwa)-.*-w(\d+)$", cid)
            if not m:
                continue
            dom = m.group(1)
            w_num = int(m.group(2))
            if w_num < 1 or w_num > 26 or w_num == 8:
                continue
            slot = by_win.setdefault(w_num, {"win": f"w{w_num:02d}", "campaigns": {}})
            c_info = slot["campaigns"].setdefault(cid, {"domain": dom, "utc_dates": set()})
            c_info["utc_dates"].add(utc_date)

    out: dict[str, dict[str, Any]] = {}
    for w_num, slot in sorted(by_win.items()):
        wid = f"w{w_num:02d}"
        camps = [
            {"campaign_id": cid, "domain": info["domain"], "utc_date": min(info["utc_dates"])}
            for cid, info in sorted(slot["campaigns"].items())
        ]
        out[wid] = {
            "win": wid,
            "w_num": w_num,
            "split": ("selection_dev" if w_num <= 20 else ("holdout" if w_num <= 25 else "w26_oos")),
            "campaigns": camps,
        }
    return out


def _campaign_files(
    lakehouse_dir: Path,
    dataset: str,
    venue: str,
    campaigns: Sequence[dict[str, str]],
) -> list[str]:
    files: list[str] = []
    for c in campaigns:
        cid = c["campaign_id"]
        # Safety invariant: never touch w27+
        m = re.search(r"-w(\d+)$", cid)
        if not m or int(m.group(1)) > 26:
            continue
        ud = c["utc_date"]
        date_dirs = [ud]
        try:
            date_dirs.append((date.fromisoformat(ud) + timedelta(days=1)).isoformat())
        except ValueError:
            pass
        for d in date_dirs:
            pdir = (
                lakehouse_dir
                / "parquet"
                / dataset
                / "collector_region=tokyo"
                / f"venue={venue}"
                / f"utc_date={d}"
                / f"campaign_id={cid}"
            )
            files.extend(glob.glob(str(pdir / "*.parquet")))
    return sorted(set(files))


def load_window_venue_bundle(
    lakehouse_dir: Path,
    venue: str,
    campaigns: Sequence[dict[str, str]],
) -> dict[str, Any]:
    """Load BBO (500ms buckets), public_trades, and funding_and_ctx (1s buckets) for one (window, venue)."""
    import duckdb

    sym_to_mkt = VENUE_MARKETS[venue]
    mkt_to_sym = {m: s for s, m in sym_to_mkt.items()}
    con = duckdb.connect(":memory:")
    con.execute("SET preserve_insertion_order=false; SET threads=2; SET memory_limit='2GB';")

    bbo_by_sym: dict[str, list[tuple[int, float, float, float, float]]] = {}
    bbo_ts_by_sym: dict[str, list[int]] = {}
    cont_by_sym: dict[str, dict[str, Any]] = {}
    trades_by_sym: dict[str, tuple[list[int], list[int], list[float], list[float]]] = {}
    oracle_by_sym: dict[str, list[tuple[int, float, float]]] = {}
    oracle_ts_by_sym: dict[str, list[int]] = {}

    bbo_files = _campaign_files(lakehouse_dir, "bbo_quotes", venue, campaigns)
    if bbo_files:
        f_sql = "[" + ", ".join("'" + p.replace("'", "''") + "'" for p in bbo_files) + "]"
        mkts_sql = ", ".join("'" + m + "'" for m in mkt_to_sym)
        stats_rows = con.execute(
            f"""
            WITH b AS (
                SELECT market, (available_at_ms // {BBO_BUCKET_MS}) AS bucket,
                       MAX(available_at_ms) AS ts_ms, MIN(available_at_ms) AS min_ts, COUNT(*) AS cnt
                FROM read_parquet({f_sql})
                WHERE market IN ({mkts_sql}) AND tradable = true AND bid_px_f64 > 0 AND ask_px_f64 > bid_px_f64
                GROUP BY 1, 2
            ),
            g AS (
                SELECT market, ts_ms, min_ts, cnt,
                       LAG(ts_ms) OVER (PARTITION BY market ORDER BY bucket) AS prev_ts
                FROM b
            )
            SELECT market, COALESCE(SUM(cnt), 0), MIN(min_ts), MAX(ts_ms), COALESCE(MAX(ts_ms - prev_ts), 0)
            FROM g GROUP BY 1
            """
        ).fetchall()
        for mkt, cnt, t0, t1, mgap in stats_rows:
            sym = mkt_to_sym.get(str(mkt))
            if sym and cnt and int(cnt) >= 100 and t0 is not None and t1 is not None:
                dur = int(t1 - t0)
                cont_by_sym[sym] = {
                    "raw_events": int(cnt),
                    "t0_ms": int(t0),
                    "t1_ms": int(t1),
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
            GROUP BY market, (available_at_ms // {BBO_BUCKET_MS})
            ORDER BY market, ts_ms ASC
            """
        ).fetchall()
        grp_bbo: dict[str, list[tuple[int, float, float, float, float]]] = defaultdict(list)
        grp_ts: dict[str, list[int]] = defaultdict(list)
        for mkt, ts_ms, bp, bs, ap, aks in rows:
            sym = mkt_to_sym.get(str(mkt))
            if sym and sym in cont_by_sym:
                its = int(ts_ms)
                grp_bbo[sym].append((its, float(bp), float(bs), float(ap), float(aks)))
                grp_ts[sym].append(its)
        bbo_by_sym = dict(grp_bbo)
        bbo_ts_by_sym = dict(grp_ts)

    tr_files = _campaign_files(lakehouse_dir, "public_trades", venue, campaigns)
    if tr_files:
        tf_sql = "[" + ", ".join("'" + p.replace("'", "''") + "'" for p in tr_files) + "]"
        mkts_sql = ", ".join("'" + m + "'" for m in mkt_to_sym)
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
        tmp_tr: dict[str, tuple[list[int], list[int], list[float], list[float]]] = defaultdict(
            lambda: ([], [], [], [])
        )
        for mkt, ts_ms, sgn, px, sz in t_rows:
            isgn = int(sgn)
            if isgn == 0:
                continue
            sym = mkt_to_sym.get(str(mkt))
            if sym:
                buf = tmp_tr[sym]
                buf[0].append(int(ts_ms))
                buf[1].append(isgn)
                buf[2].append(float(px))
                buf[3].append(float(sz))
        trades_by_sym = dict(tmp_tr)

    fnd_files = _campaign_files(lakehouse_dir, "funding_and_ctx", venue, campaigns)
    if fnd_files:
        ff_sql = "[" + ", ".join("'" + p.replace("'", "''") + "'" for p in fnd_files) + "]"
        syms_sql = ", ".join("'" + s + "'" for s in sym_to_mkt)
        f_rows = con.execute(
            f"""
            SELECT market,
                   MAX(available_at_ms) AS ts_ms,
                   arg_max(oracle_px_f64, available_at_ms) AS oracle_px,
                   arg_max(COALESCE(funding_rate_bps_f64, 0.0), available_at_ms) AS fnd_bps
            FROM read_parquet({ff_sql})
            WHERE market IN ({syms_sql}) AND oracle_px_f64 > 0
            GROUP BY market, (available_at_ms // {STEP_MS})
            ORDER BY market, ts_ms ASC
            """
        ).fetchall()
        grp_o: dict[str, list[tuple[int, float, float]]] = defaultdict(list)
        grp_ots: dict[str, list[int]] = defaultdict(list)
        for sym, ts_ms, opx, fnd in f_rows:
            ssym = str(sym)
            if ssym in sym_to_mkt:
                its = int(ts_ms)
                grp_o[ssym].append((its, float(opx), float(fnd)))
                grp_ots[ssym].append(its)
        oracle_by_sym = dict(grp_o)
        oracle_ts_by_sym = dict(grp_ots)

    con.close()
    return {
        "bbo": bbo_by_sym,
        "bbo_ts": bbo_ts_by_sym,
        "cont": cont_by_sym,
        "trades": trades_by_sym,
        "oracle": oracle_by_sym,
        "oracle_ts": oracle_ts_by_sym,
    }


# ---------------------------------------------------------------------------
# Execution & Funding Realism Helpers
# ---------------------------------------------------------------------------


def _latest_at(
    rows: Sequence[Any],
    ts_arr: Sequence[int],
    ts_query: int,
) -> Any | None:
    idx = bisect.bisect_right(ts_arr, ts_query) - 1
    if idx < 0:
        return None
    return rows[idx]


def _check_print_fill(
    tr_tuple: tuple[list[int], list[int], list[float], list[float]] | None,
    t_start: int,
    t_end: int,
    side: int,
    rest_px: float,
    target_notional_usd: float,
    l1_depth_usd: float,
) -> tuple[bool, float, int]:
    """Strict `public_trades` print verification for a resting PostOnly limit order.

    Returns `(filled_any, filled_notional_usd, fill_ts_ms)`.
    - `side == +1` (Buy PostOnly resting at `rest_px` bid): requires sell-aggressor prints
      (`tr_sgn == -1`) at `px <= rest_px` (or any print strictly below `rest_px`).
    - `side == -1` (Sell PostOnly resting at `rest_px` ask): requires buy-aggressor prints
      (`tr_sgn == +1`) at `px >= rest_px` (or any print strictly above `rest_px`).
    - Filled notional is capped by BOTH `L1_DEPTH_CAP_FRAC * l1_depth_usd` and
      `L1_DEPTH_CAP_FRAC * matched_print_notional_usd`. Zero fabricated fills.
    """
    if tr_tuple is None or target_notional_usd <= 0.0 or l1_depth_usd <= 0.0:
        return False, 0.0, t_start
    tr_ts, tr_sgn, tr_px, tr_sz = tr_tuple
    if not tr_ts:
        return False, 0.0, t_start
    lo = bisect.bisect_left(tr_ts, t_start)
    hi = bisect.bisect_right(tr_ts, t_end)
    if hi <= lo:
        return False, 0.0, t_start

    cap_l1 = min(target_notional_usd, L1_DEPTH_CAP_FRAC * l1_depth_usd)
    if cap_l1 <= 0.0:
        return False, 0.0, t_start

    acc_print_notl = 0.0
    fill_ts = t_start
    for idx in range(lo, hi):
        px = tr_px[idx]
        sz = tr_sz[idx]
        sgn = tr_sgn[idx]
        if side > 0:
            matched = (sgn < 0 and px <= rest_px + 1e-12) or (px < rest_px - 1e-12)
        else:
            matched = (sgn > 0 and px >= rest_px - 1e-12) or (px > rest_px + 1e-12)
        if matched:
            acc_print_notl += px * sz
            fill_ts = tr_ts[idx]
            if L1_DEPTH_CAP_FRAC * acc_print_notl >= cap_l1:
                return True, cap_l1, fill_ts

    filled_notl = min(cap_l1, L1_DEPTH_CAP_FRAC * acc_print_notl)
    if filled_notl > 0.0:
        return True, filled_notl, fill_ts
    return False, 0.0, t_start


def _funding_cash_usd(
    oracle_rows: Sequence[tuple[int, float, float]],
    oracle_ts: Sequence[int],
    t_start_ms: int,
    t_end_ms: int,
    signed_notional_usd: float,
) -> float:
    """Integrate causal hourly `funding_and_ctx` cash over `[t_start_ms, t_end_ms]`.

    Matches `src/strategy/pipeline_driver.rs`:
    `funding_usd = -signed_notional_usd * (funding_rate_bps / 10_000) * dt_hours`.
    """
    if t_end_ms <= t_start_ms or not oracle_rows or abs(signed_notional_usd) <= 0.0:
        return 0.0
    i0 = bisect.bisect_right(oracle_ts, t_start_ms) - 1
    if i0 < 0:
        i0 = 0
    i1 = bisect.bisect_right(oracle_ts, t_end_ms) - 1
    if i1 < i0:
        return 0.0

    cash = 0.0
    cur_t = t_start_ms
    idx = i0
    while cur_t < t_end_ms:
        rate_bps = oracle_rows[idx][2]
        nxt_t = oracle_ts[idx + 1] if (idx + 1) < len(oracle_ts) else t_end_ms
        seg_end = min(t_end_ms, nxt_t)
        if seg_end > cur_t and math.isfinite(rate_bps):
            dt_hours = (seg_end - cur_t) / 3_600_000.0
            cash += -signed_notional_usd * (rate_bps * 1e-4) * dt_hours
        cur_t = seg_end
        if idx + 1 < len(oracle_ts) and oracle_ts[idx + 1] <= cur_t:
            idx += 1
        elif seg_end >= t_end_ms:
            break
        else:
            idx += 1
            if idx >= len(oracle_ts):
                break
    return cash


def _taker_fill(
    bbo_rows: Sequence[tuple[int, float, float, float, float]],
    bbo_ts: Sequence[int],
    t_target: int,
    side: int,
    max_age_ms: int,
) -> tuple[float, float, int] | None:
    """Return `(fill_px, l1_depth_usd, quote_ts_ms)` for L1 crossing at `t_target`."""
    q = _latest_at(bbo_rows, bbo_ts, t_target)
    if q is None:
        return None
    q_ts, bp, bs, ap, aks = q
    if (t_target - q_ts) > max_age_ms or bp <= 0.0 or ap <= bp:
        return None
    px = ap if side > 0 else bp
    sz = aks if side > 0 else bs
    if not (math.isfinite(sz) and sz > 0.0):
        return None
    return px, px * sz, q_ts


# ---------------------------------------------------------------------------
# Family A Precompute & Simulation (`PairsStatArbPy`)
# ---------------------------------------------------------------------------


@dataclasses.dataclass
class PrecomputedPairsSignal:
    t0_ms: int
    t1_ms: int
    ts_ms: list[int]
    status: list[str]
    beta: list[float]
    z_score: list[float]
    combined_spread_bps: list[float]
    y_bbo: Sequence[tuple[int, float, float, float, float]]
    y_bbo_ts: Sequence[int]
    x_bbo: Sequence[tuple[int, float, float, float, float]]
    x_bbo_ts: Sequence[int]
    y_tr: tuple[list[int], list[int], list[float], list[float]] | None
    x_tr: tuple[list[int], list[int], list[float], list[float]] | None
    y_ora: Sequence[tuple[int, float, float]]
    y_ora_ts: Sequence[int]
    x_ora: Sequence[tuple[int, float, float]]
    x_ora_ts: Sequence[int]


def precompute_pairs_signal(
    y_bbo: Sequence[tuple[int, float, float, float, float]],
    y_bbo_ts: Sequence[int],
    x_bbo: Sequence[tuple[int, float, float, float, float]],
    x_bbo_ts: Sequence[int],
    *,
    ewma_tau_ms: int,
    y_tr: tuple[list[int], list[int], list[float], list[float]] | None = None,
    x_tr: tuple[list[int], list[int], list[float], list[float]] | None = None,
    y_ora: Sequence[tuple[int, float, float]] = (),
    y_ora_ts: Sequence[int] = (),
    x_ora: Sequence[tuple[int, float, float]] = (),
    x_ora_ts: Sequence[int] = (),
    use_phi_gate: bool = True,
    step_ms: int = STEP_MS,
) -> PrecomputedPairsSignal | None:
    if len(y_bbo) < 100 or len(x_bbo) < 100:
        return None
    t0_ms = max(int(y_bbo_ts[0]), int(x_bbo_ts[0]))
    t1_ms = min(int(y_bbo_ts[-1]), int(x_bbo_ts[-1]))
    if t1_ms <= t0_ms + DEFAULT_WARMUP_MS + 60_000:
        return None

    cfg = PairsStatArbCfg(ewma_tau_ms=ewma_tau_ms, use_phi_gate=use_phi_gate)
    flt = PairsSignalFilterPy(cfg)

    ts_list: list[int] = []
    st_list: list[str] = []
    beta_list: list[float] = []
    z_list: list[float] = []
    spr_list: list[float] = []

    iy = 0
    ix = 0
    ny = len(y_bbo)
    nx = len(x_bbo)
    cur_y: tuple[int, float, float, float, float] | None = None
    cur_x: tuple[int, float, float, float, float] | None = None

    t = t0_ms
    while t <= t1_ms:
        while iy < ny and y_bbo_ts[iy] <= t:
            cur_y = y_bbo[iy]
            iy += 1
        while ix < nx and x_bbo_ts[ix] <= t:
            cur_x = x_bbo[ix]
            ix += 1
        _, status, beta, z_sc, comb_spr, _ = flt.step(t, cur_y, cur_x)
        ts_list.append(t)
        st_list.append(status)
        beta_list.append(beta)
        z_list.append(z_sc)
        spr_list.append(comb_spr)
        t += step_ms

    return PrecomputedPairsSignal(
        t0_ms=t0_ms,
        t1_ms=t1_ms,
        ts_ms=ts_list,
        status=st_list,
        beta=beta_list,
        z_score=z_list,
        combined_spread_bps=spr_list,
        y_bbo=y_bbo,
        y_bbo_ts=y_bbo_ts,
        x_bbo=x_bbo,
        x_bbo_ts=x_bbo_ts,
        y_tr=y_tr,
        x_tr=x_tr,
        y_ora=y_ora,
        y_ora_ts=y_ora_ts,
        x_ora=x_ora,
        x_ora_ts=x_ora_ts,
    )


def simulate_pairs_signal(
    sig: PrecomputedPairsSignal,
    *,
    z_entry: float = 2.5,
    z_exit: float = 0.25,
    z_stop: float = 4.5,
    max_hold_ms: int = 14_400_000,
    max_combined_spread_bps: float = 25.0,
    exec_mode: str = "taker",
    clip_usd: float = CLIP_USD_DEFAULT,
) -> dict[str, Any]:
    """Simulate causal PairsStatArb execution with L1 depth clamping, hourly funding, and terminal flatten."""
    ts_ms = sig.ts_ms
    n = len(ts_ms)
    if n < 10:
        return _empty_pair_or_oracle_result()

    t_stop_entries = sig.t1_ms - 10_000
    status = sig.status
    z_arr = sig.z_score
    beta_arr = sig.beta
    spr_arr = sig.combined_spread_bps

    trades = 0
    long_y_trades = 0
    pv_trades = 0
    pv_unverifiable = 0
    depth_clipped_entries = 0
    depth_clipped_exits = 0
    exits_by_reason: dict[str, int] = defaultdict(int)

    gross_price_pnl_usd = 0.0
    funding_cash_usd = 0.0
    maker_vol_usd = 0.0
    taker_vol_usd = 0.0
    total_entry_notl_usd = 0.0
    hold_ms_list: list[int] = []
    terminal_residual_notl_usd = 0.0

    k = 0
    while k < n:
        t_dec = ts_ms[k]
        if t_dec >= t_stop_entries:
            break
        if status[k] != "ready" or spr_arr[k] > max_combined_spread_bps:
            k += 1
            continue
        zk = z_arr[k]
        if abs(zk) >= z_stop or abs(zk) < z_entry:
            k += 1
            continue

        side_y = 1 if zk <= -z_entry else -1
        side_x = -side_y
        beta_k = max(0.25, min(4.0, beta_arr[k]))
        target_y = clip_usd
        target_x = clip_usd * beta_k

        # Execute Entry
        if exec_mode == "taker":
            t_ent = t_dec + TAKER_DELAY_MS
            fy = _taker_fill(sig.y_bbo, sig.y_bbo_ts, t_ent, side_y, DEFAULT_MAX_QUOTE_AGE_MS)
            fx = _taker_fill(sig.x_bbo, sig.x_bbo_ts, t_ent, side_x, DEFAULT_MAX_QUOTE_AGE_MS)
            if fy is None or fx is None:
                k += 1
                continue
            y_px_in, y_l1_in, _ = fy
            x_px_in, x_l1_in, _ = fx
            scale = min(1.0, (L1_DEPTH_CAP_FRAC * y_l1_in) / target_y, (L1_DEPTH_CAP_FRAC * x_l1_in) / target_x)
            if scale <= 0.01:
                k += 1
                continue
            if scale < 1.0:
                depth_clipped_entries += 1
            notl_y = target_y * scale
            notl_x = target_x * scale
            t_fill_in = t_ent
            taker_vol_usd += notl_y + notl_x
        else:
            # PostOnly at t_dec + 200ms with strict public_trades print verification
            t_rest = t_dec + MAKER_DELAY_MS
            qy = _latest_at(sig.y_bbo, sig.y_bbo_ts, t_rest)
            qx = _latest_at(sig.x_bbo, sig.x_bbo_ts, t_rest)
            if qy is None or qx is None:
                k += 1
                continue
            if (t_rest - qy[0]) > DEFAULT_MAX_QUOTE_AGE_MS or (t_rest - qx[0]) > DEFAULT_MAX_QUOTE_AGE_MS:
                k += 1
                continue
            y_rest_px = qy[1] if side_y > 0 else qy[3]
            y_rest_sz = qy[2] if side_y > 0 else qy[4]
            x_rest_px = qx[1] if side_x > 0 else qx[3]
            x_rest_sz = qx[2] if side_x > 0 else qx[4]
            if y_rest_px <= 0.0 or x_rest_px <= 0.0 or y_rest_sz <= 0.0 or x_rest_sz <= 0.0:
                k += 1
                continue
            y_l1_in = y_rest_px * y_rest_sz
            x_l1_in = x_rest_px * x_rest_sz
            scale = min(1.0, (L1_DEPTH_CAP_FRAC * y_l1_in) / target_y, (L1_DEPTH_CAP_FRAC * x_l1_in) / target_x)
            if scale <= 0.01:
                k += 1
                continue
            want_y = target_y * scale
            want_x = target_x * scale
            t_chase_end = min(t_rest + MAKER_CHASE_WINDOW_MS, t_stop_entries)
            ok_y, fill_y, fy_ts = _check_print_fill(sig.y_tr, t_rest, t_chase_end, side_y, y_rest_px, want_y, y_l1_in)
            ok_x, fill_x, fx_ts = _check_print_fill(sig.x_tr, t_rest, t_chase_end, side_x, x_rest_px, want_x, x_l1_in)
            if not ok_y and not ok_x:
                pv_unverifiable += 1
                k = max(k + 1, bisect.bisect_left(ts_ms, t_rest + 1_000))
                continue
            if ok_y and ok_x:
                # Matched passive fills on both legs (scale to matched ratio)
                s2 = min(1.0, fill_y / want_y, fill_x / want_x)
                notl_y = want_y * s2
                notl_x = want_x * s2
                y_px_in = y_rest_px
                x_px_in = x_rest_px
                t_fill_in = max(fy_ts, fx_ts)
                maker_vol_usd += notl_y + notl_x
                pv_trades += 1
            elif ok_y:
                # Y filled passively; hedge X via taker IOC at t_chase_end + 330ms
                fx = _taker_fill(sig.x_bbo, sig.x_bbo_ts, t_chase_end + TAKER_DELAY_MS, side_x, DEFAULT_MAX_QUOTE_AGE_MS)
                if fx is None:
                    # Unwind filled Y immediately via taker IOC
                    fy_unw = _taker_fill(sig.y_bbo, sig.y_bbo_ts, t_chase_end + TAKER_DELAY_MS, -side_y, DEFAULT_MAX_QUOTE_AGE_MS)
                    if fy_unw is not None:
                        gross_price_pnl_usd += side_y * (fy_unw[0] / y_rest_px - 1.0) * fill_y
                        maker_vol_usd += fill_y
                        taker_vol_usd += fill_y
                    k = max(k + 1, bisect.bisect_left(ts_ms, t_chase_end + TAKER_DELAY_MS))
                    continue
                x_px_in, x_l1_ioc, _ = fx
                notl_y = fill_y
                notl_x = min(fill_y * beta_k, L1_DEPTH_CAP_FRAC * x_l1_ioc)
                y_px_in = y_rest_px
                t_fill_in = t_chase_end + TAKER_DELAY_MS
                maker_vol_usd += notl_y
                taker_vol_usd += notl_x
                pv_trades += 1
            else:
                # X filled passively; hedge Y via taker IOC
                fy = _taker_fill(sig.y_bbo, sig.y_bbo_ts, t_chase_end + TAKER_DELAY_MS, side_y, DEFAULT_MAX_QUOTE_AGE_MS)
                if fy is None:
                    fx_unw = _taker_fill(sig.x_bbo, sig.x_bbo_ts, t_chase_end + TAKER_DELAY_MS, -side_x, DEFAULT_MAX_QUOTE_AGE_MS)
                    if fx_unw is not None:
                        gross_price_pnl_usd += side_x * (fx_unw[0] / x_rest_px - 1.0) * fill_x
                        maker_vol_usd += fill_x
                        taker_vol_usd += fill_x
                    k = max(k + 1, bisect.bisect_left(ts_ms, t_chase_end + TAKER_DELAY_MS))
                    continue
                assert fy is not None
                y_px_in, y_l1_ioc, _ = fy
                notl_x = fill_x
                notl_y = min(fill_x / beta_k, L1_DEPTH_CAP_FRAC * y_l1_ioc)
                x_px_in = x_rest_px
                t_fill_in = t_chase_end + TAKER_DELAY_MS
                maker_vol_usd += notl_x
                taker_vol_usd += notl_y
                pv_trades += 1

        # Scan forward for exit (mean reversion, stop loss, max hold, or terminal flatten)
        j = max(k + 1, bisect.bisect_left(ts_ms, t_fill_in))
        exit_reason = "terminal_flatten"
        j_exit = n - 1
        while j < n:
            tj = ts_ms[j]
            if tj >= t_stop_entries:
                j_exit = j
                exit_reason = "terminal_flatten"
                break
            if (tj - t_fill_in) >= max_hold_ms:
                j_exit = j
                exit_reason = "max_hold"
                break
            zj = z_arr[j]
            if status[j] in ("ready", "non_stationary"):
                if (side_y > 0 and zj <= -z_stop) or (side_y < 0 and zj >= z_stop):
                    j_exit = j
                    exit_reason = "stop_loss"
                    break
                if (side_y > 0 and zj >= -z_exit) or (side_y < 0 and zj <= z_exit):
                    j_exit = j
                    exit_reason = "mean_reversion"
                    break
            j += 1

        t_exit_dec = ts_ms[j_exit]
        # Execute Exit with strict 0.5 * L1_depth iterative slice unwind across ticks if depth-clipped.
        rem_y = notl_y
        rem_x = notl_x
        last_fnd_t = t_fill_in
        last_fill_out = min(sig.t1_ms, t_exit_dec + TAKER_DELAY_MS)
        clipped_here = False

        if exec_mode == "maker_postonly" and exit_reason == "mean_reversion":
            t_xr = t_exit_dec + MAKER_DELAY_MS
            t_xc = min(t_xr + MAKER_CHASE_WINDOW_MS, sig.t1_ms - 1_000)
            qy_o = _latest_at(sig.y_bbo, sig.y_bbo_ts, t_xr)
            qx_o = _latest_at(sig.x_bbo, sig.x_bbo_ts, t_xr)
            if qy_o is not None and (t_xr - qy_o[0]) <= DEFAULT_MAX_QUOTE_AGE_MS:
                yrp = qy_o[1] if (-side_y) > 0 else qy_o[3]
                yrs = qy_o[2] if (-side_y) > 0 else qy_o[4]
                if yrp > 0.0 and yrs > 0.0:
                    ok_yo, fill_yo, fyo_ts = _check_print_fill(sig.y_tr, t_xr, t_xc, -side_y, yrp, rem_y, yrp * yrs)
                    if ok_yo and fill_yo > 0.0:
                        sl_y = min(rem_y, fill_yo)
                        gross_price_pnl_usd += side_y * (yrp / y_px_in - 1.0) * sl_y
                        funding_cash_usd += _funding_cash_usd(sig.y_ora, sig.y_ora_ts, last_fnd_t, fyo_ts, side_y * sl_y)
                        maker_vol_usd += sl_y
                        rem_y -= sl_y
                        last_fill_out = max(last_fill_out, fyo_ts)
            if qx_o is not None and (t_xr - qx_o[0]) <= DEFAULT_MAX_QUOTE_AGE_MS:
                xrp = qx_o[1] if (-side_x) > 0 else qx_o[3]
                xrs = qx_o[2] if (-side_x) > 0 else qx_o[4]
                if xrp > 0.0 and xrs > 0.0:
                    ok_xo, fill_xo, fxo_ts = _check_print_fill(sig.x_tr, t_xr, t_xc, -side_x, xrp, rem_x, xrp * xrs)
                    if ok_xo and fill_xo > 0.0:
                        sl_x = min(rem_x, fill_xo)
                        gross_price_pnl_usd += side_x * (xrp / x_px_in - 1.0) * sl_x
                        funding_cash_usd += _funding_cash_usd(sig.x_ora, sig.x_ora_ts, last_fnd_t, fxo_ts, side_x * sl_x)
                        maker_vol_usd += sl_x
                        rem_x -= sl_x
                        last_fill_out = max(last_fill_out, fxo_ts)
            j_cur = max(j_exit, bisect.bisect_left(ts_ms, t_xc))
        else:
            j_cur = j_exit

        # Unwind any remaining Y/X notional via taker IOC at +330ms, stepping ticks if 0.5*L1_depth clips
        while (rem_y > 1e-6 or rem_x > 1e-6) and j_cur < n:
            t_ex = min(sig.t1_ms, ts_ms[j_cur] + TAKER_DELAY_MS)
            if rem_y > 1e-6:
                fy_o = _taker_fill(sig.y_bbo, sig.y_bbo_ts, t_ex, -side_y, DEFAULT_MAX_QUOTE_AGE_MS)
                if fy_o is not None:
                    cap_y = L1_DEPTH_CAP_FRAC * fy_o[1]
                    sl_y = min(rem_y, cap_y)
                    if sl_y < rem_y - 1e-6:
                        clipped_here = True
                    if sl_y > 0.0:
                        gross_price_pnl_usd += side_y * (fy_o[0] / y_px_in - 1.0) * sl_y
                        funding_cash_usd += _funding_cash_usd(sig.y_ora, sig.y_ora_ts, t_fill_in, t_ex, side_y * sl_y)
                        taker_vol_usd += sl_y
                        rem_y -= sl_y
                        last_fill_out = max(last_fill_out, t_ex)
            if rem_x > 1e-6:
                fx_o = _taker_fill(sig.x_bbo, sig.x_bbo_ts, t_ex, -side_x, DEFAULT_MAX_QUOTE_AGE_MS)
                if fx_o is not None:
                    cap_x = L1_DEPTH_CAP_FRAC * fx_o[1]
                    sl_x = min(rem_x, cap_x)
                    if sl_x < rem_x - 1e-6:
                        clipped_here = True
                    if sl_x > 0.0:
                        gross_price_pnl_usd += side_x * (fx_o[0] / x_px_in - 1.0) * sl_x
                        funding_cash_usd += _funding_cash_usd(sig.x_ora, sig.x_ora_ts, t_fill_in, t_ex, side_x * sl_x)
                        taker_vol_usd += sl_x
                        rem_x -= sl_x
                        last_fill_out = max(last_fill_out, t_ex)
            if rem_y <= 1e-6 and rem_x <= 1e-6:
                break
            j_cur += 1

        if clipped_here:
            depth_clipped_exits += 1
        if rem_y > 1e-6:
            last_y = sig.y_bbo[-1]
            y_mtm = last_y[1] if side_y > 0 else last_y[3]
            gross_price_pnl_usd += side_y * (y_mtm / y_px_in - 1.0) * rem_y
            funding_cash_usd += _funding_cash_usd(sig.y_ora, sig.y_ora_ts, t_fill_in, sig.t1_ms, side_y * rem_y)
            terminal_residual_notl_usd += rem_y
        if rem_x > 1e-6:
            last_x = sig.x_bbo[-1]
            x_mtm = last_x[1] if side_x > 0 else last_x[3]
            gross_price_pnl_usd += side_x * (x_mtm / x_px_in - 1.0) * rem_x
            funding_cash_usd += _funding_cash_usd(sig.x_ora, sig.x_ora_ts, t_fill_in, sig.t1_ms, side_x * rem_x)
            terminal_residual_notl_usd += rem_x

        total_entry_notl_usd += notl_y + notl_x
        trades += 1
        if side_y > 0:
            long_y_trades += 1
        exits_by_reason[exit_reason] += 1
        hold_ms_list.append(max(0, last_fill_out - t_fill_in))

        k = max(j_cur + 1, bisect.bisect_left(ts_ms, last_fill_out))

    return _finalize_trade_metrics(
        trades=trades,
        long_trades=long_y_trades,
        pv_trades=pv_trades if exec_mode == "maker_postonly" else trades,
        pv_unverifiable=pv_unverifiable,
        gross_price_pnl_usd=gross_price_pnl_usd,
        funding_cash_usd=funding_cash_usd,
        maker_vol_usd=maker_vol_usd,
        taker_vol_usd=taker_vol_usd,
        total_entry_notl_usd=total_entry_notl_usd,
        hold_ms_list=hold_ms_list,
        depth_clipped_entries=depth_clipped_entries,
        depth_clipped_exits=depth_clipped_exits,
        terminal_residual_notl_usd=terminal_residual_notl_usd,
        exits_by_reason=dict(exits_by_reason),
    )


# ---------------------------------------------------------------------------
# Family B Precompute & Simulation (`OracleDislocationPy`)
# ---------------------------------------------------------------------------


@dataclasses.dataclass
class PrecomputedOracleSignal:
    t0_ms: int
    t1_ms: int
    ts_ms: list[int]
    status: list[str]
    disloc_bps: list[float]
    spread_bps: list[float]
    funding_bps: list[float]
    bbo: Sequence[tuple[int, float, float, float, float]]
    bbo_ts: Sequence[int]
    tr: tuple[list[int], list[int], list[float], list[float]] | None
    ora: Sequence[tuple[int, float, float]]
    ora_ts: Sequence[int]


def precompute_oracle_signal(
    bbo: Sequence[tuple[int, float, float, float, float]],
    bbo_ts: Sequence[int],
    ora: Sequence[tuple[int, float, float]],
    ora_ts: Sequence[int],
    *,
    tr: tuple[list[int], list[int], list[float], list[float]] | None = None,
    eq_ewma_tau_ms: int = 3_600_000,
    step_ms: int = STEP_MS,
) -> PrecomputedOracleSignal | None:
    if len(bbo) < 100 or len(ora) < 50:
        return None
    t0_ms = max(int(bbo_ts[0]), int(ora_ts[0]))
    t1_ms = min(int(bbo_ts[-1]), int(ora_ts[-1]))
    if t1_ms <= t0_ms + DEFAULT_WARMUP_MS + 60_000:
        return None

    flt = OracleSignalFilterPy(OracleDislocCfg(eq_ewma_tau_ms=eq_ewma_tau_ms))
    ts_list: list[int] = []
    st_list: list[str] = []
    dis_list: list[float] = []
    spr_list: list[float] = []
    fnd_list: list[float] = []

    ib = 0
    io = 0
    nb = len(bbo)
    no = len(ora)
    cur_b: tuple[int, float, float, float, float] | None = None
    cur_o: tuple[int, float, float] | None = None

    t = t0_ms
    while t <= t1_ms:
        while ib < nb and bbo_ts[ib] <= t:
            cur_b = bbo[ib]
            ib += 1
        while io < no and ora_ts[io] <= t:
            cur_o = ora[io]
            io += 1
        _, status, dis, spr, fnd = flt.step(t, cur_b, cur_o)
        ts_list.append(t)
        st_list.append(status)
        dis_list.append(dis)
        spr_list.append(spr)
        fnd_list.append(fnd)
        t += step_ms

    return PrecomputedOracleSignal(
        t0_ms=t0_ms,
        t1_ms=t1_ms,
        ts_ms=ts_list,
        status=st_list,
        disloc_bps=dis_list,
        spread_bps=spr_list,
        funding_bps=fnd_list,
        bbo=bbo,
        bbo_ts=bbo_ts,
        tr=tr,
        ora=ora,
        ora_ts=ora_ts,
    )


def simulate_oracle_signal(
    sig: PrecomputedOracleSignal,
    *,
    min_dislocation_bps: float = 5.0,
    spread_mult: float = 1.5,
    require_funding_alignment: bool = False,
    max_adverse_funding_bps: float = 0.5,
    max_dislocation_bps: float = 80.0,
    exit_dislocation_bps: float = 0.5,
    stop_dislocation_bps: float = 100.0,
    max_hold_ms: int = 3_600_000,
    max_entry_spread_bps: float = 20.0,
    exec_mode: str = "taker",
    clip_usd: float = CLIP_USD_DEFAULT,
) -> dict[str, Any]:
    """Simulate causal OracleDislocationReversion with L1 depth clamping, hourly funding, and terminal flatten."""
    ts_ms = sig.ts_ms
    n = len(ts_ms)
    if n < 10:
        return _empty_pair_or_oracle_result()

    t_stop_entries = sig.t1_ms - 10_000
    status = sig.status
    dis_arr = sig.disloc_bps
    spr_arr = sig.spread_bps
    fnd_arr = sig.funding_bps

    trades = 0
    long_trades = 0
    pv_trades = 0
    pv_unverifiable = 0
    depth_clipped_entries = 0
    depth_clipped_exits = 0
    exits_by_reason: dict[str, int] = defaultdict(int)

    gross_price_pnl_usd = 0.0
    funding_cash_usd = 0.0
    maker_vol_usd = 0.0
    taker_vol_usd = 0.0
    total_entry_notl_usd = 0.0
    hold_ms_list: list[int] = []
    terminal_residual_notl_usd = 0.0

    k = 0
    while k < n:
        t_dec = ts_ms[k]
        if t_dec >= t_stop_entries:
            break
        if status[k] != "ready":
            k += 1
            continue
        spr = spr_arr[k]
        if spr > max_entry_spread_bps:
            k += 1
            continue
        dis = dis_arr[k]
        if abs(dis) >= max_dislocation_bps:
            k += 1
            continue
        hurdle = max(min_dislocation_bps, spread_mult * spr)
        if abs(dis) < hurdle:
            k += 1
            continue

        side = 1 if dis <= -hurdle else -1
        if require_funding_alignment:
            fnd = fnd_arr[k]
            if side > 0 and fnd > max_adverse_funding_bps:
                k += 1
                continue
            if side < 0 and fnd < -max_adverse_funding_bps:
                k += 1
                continue

        if exec_mode == "taker":
            t_ent = t_dec + TAKER_DELAY_MS
            f_in = _taker_fill(sig.bbo, sig.bbo_ts, t_ent, side, DEFAULT_MAX_QUOTE_AGE_MS)
            if f_in is None:
                k += 1
                continue
            px_in, l1_in, _ = f_in
            notl = min(clip_usd, L1_DEPTH_CAP_FRAC * l1_in)
            if notl <= 5.0:
                k += 1
                continue
            if notl < clip_usd:
                depth_clipped_entries += 1
            t_fill_in = t_ent
            taker_vol_usd += notl
        else:
            t_rest = t_dec + MAKER_DELAY_MS
            q_in = _latest_at(sig.bbo, sig.bbo_ts, t_rest)
            if q_in is None or (t_rest - q_in[0]) > DEFAULT_MAX_QUOTE_AGE_MS:
                k += 1
                continue
            rest_px = q_in[1] if side > 0 else q_in[3]
            rest_sz = q_in[2] if side > 0 else q_in[4]
            if rest_px <= 0.0 or rest_sz <= 0.0:
                k += 1
                continue
            l1_in = rest_px * rest_sz
            want_notl = min(clip_usd, L1_DEPTH_CAP_FRAC * l1_in)
            if want_notl <= 5.0:
                k += 1
                continue
            t_chase_end = min(t_rest + MAKER_CHASE_WINDOW_MS, t_stop_entries)
            ok_in, fill_notl, fin_ts = _check_print_fill(sig.tr, t_rest, t_chase_end, side, rest_px, want_notl, l1_in)
            if not ok_in or fill_notl <= 5.0:
                pv_unverifiable += 1
                k = max(k + 1, bisect.bisect_left(ts_ms, t_rest + 1_000))
                continue
            if fill_notl < clip_usd:
                depth_clipped_entries += 1
            px_in = rest_px
            notl = fill_notl
            t_fill_in = fin_ts
            maker_vol_usd += notl
            pv_trades += 1

        j = max(k + 1, bisect.bisect_left(ts_ms, t_fill_in))
        exit_reason = "terminal_flatten"
        j_exit = n - 1
        while j < n:
            tj = ts_ms[j]
            if tj >= t_stop_entries:
                j_exit = j
                exit_reason = "terminal_flatten"
                break
            if (tj - t_fill_in) >= max_hold_ms:
                j_exit = j
                exit_reason = "max_hold"
                break
            dj = dis_arr[j]
            if status[j] == "ready":
                if (side > 0 and dj <= -stop_dislocation_bps) or (side < 0 and dj >= stop_dislocation_bps):
                    j_exit = j
                    exit_reason = "stop_loss"
                    break
                if (side > 0 and dj >= -exit_dislocation_bps) or (side < 0 and dj <= exit_dislocation_bps):
                    j_exit = j
                    exit_reason = "mean_reversion"
                    break
            j += 1

        t_exit_dec = ts_ms[j_exit]
        rem = notl
        last_fill_out = min(sig.t1_ms, t_exit_dec + TAKER_DELAY_MS)
        clipped_here = False

        if exec_mode == "maker_postonly" and exit_reason == "mean_reversion":
            t_xr = t_exit_dec + MAKER_DELAY_MS
            t_xc = min(t_xr + MAKER_CHASE_WINDOW_MS, sig.t1_ms - 1_000)
            q_o = _latest_at(sig.bbo, sig.bbo_ts, t_xr)
            if q_o is not None and (t_xr - q_o[0]) <= DEFAULT_MAX_QUOTE_AGE_MS:
                rp = q_o[1] if (-side) > 0 else q_o[3]
                rs = q_o[2] if (-side) > 0 else q_o[4]
                if rp > 0.0 and rs > 0.0:
                    ok_o, fill_o, fo_ts = _check_print_fill(sig.tr, t_xr, t_xc, -side, rp, rem, rp * rs)
                    if ok_o and fill_o > 0.0:
                        sl = min(rem, fill_o)
                        gross_price_pnl_usd += side * (rp / px_in - 1.0) * sl
                        funding_cash_usd += _funding_cash_usd(sig.ora, sig.ora_ts, t_fill_in, fo_ts, side * sl)
                        maker_vol_usd += sl
                        rem -= sl
                        last_fill_out = max(last_fill_out, fo_ts)
            j_cur = max(j_exit, bisect.bisect_left(ts_ms, t_xc))
        else:
            j_cur = j_exit

        while rem > 1e-6 and j_cur < n:
            t_ex = min(sig.t1_ms, ts_ms[j_cur] + TAKER_DELAY_MS)
            f_out = _taker_fill(sig.bbo, sig.bbo_ts, t_ex, -side, DEFAULT_MAX_QUOTE_AGE_MS)
            if f_out is not None:
                cap = L1_DEPTH_CAP_FRAC * f_out[1]
                sl = min(rem, cap)
                if sl < rem - 1e-6:
                    clipped_here = True
                if sl > 0.0:
                    gross_price_pnl_usd += side * (f_out[0] / px_in - 1.0) * sl
                    funding_cash_usd += _funding_cash_usd(sig.ora, sig.ora_ts, t_fill_in, t_ex, side * sl)
                    taker_vol_usd += sl
                    rem -= sl
                    last_fill_out = max(last_fill_out, t_ex)
            if rem <= 1e-6:
                break
            j_cur += 1

        if clipped_here:
            depth_clipped_exits += 1
        if rem > 1e-6:
            last_q = sig.bbo[-1]
            px_mtm = last_q[1] if side > 0 else last_q[3]
            gross_price_pnl_usd += side * (px_mtm / px_in - 1.0) * rem
            funding_cash_usd += _funding_cash_usd(sig.ora, sig.ora_ts, t_fill_in, sig.t1_ms, side * rem)
            terminal_residual_notl_usd += rem

        total_entry_notl_usd += notl
        trades += 1
        if side > 0:
            long_trades += 1
        exits_by_reason[exit_reason] += 1
        hold_ms_list.append(max(0, last_fill_out - t_fill_in))

        k = max(j_cur + 1, bisect.bisect_left(ts_ms, last_fill_out))

    return _finalize_trade_metrics(
        trades=trades,
        long_trades=long_trades,
        pv_trades=pv_trades if exec_mode == "maker_postonly" else trades,
        pv_unverifiable=pv_unverifiable,
        gross_price_pnl_usd=gross_price_pnl_usd,
        funding_cash_usd=funding_cash_usd,
        maker_vol_usd=maker_vol_usd,
        taker_vol_usd=taker_vol_usd,
        total_entry_notl_usd=total_entry_notl_usd,
        hold_ms_list=hold_ms_list,
        depth_clipped_entries=depth_clipped_entries,
        depth_clipped_exits=depth_clipped_exits,
        terminal_residual_notl_usd=terminal_residual_notl_usd,
        exits_by_reason=dict(exits_by_reason),
    )


def _empty_pair_or_oracle_result() -> dict[str, Any]:
    return {
        "trades": 0,
        "long_trades": 0,
        "pv_trades": 0,
        "pv_unverifiable": 0,
        "gross_price_pnl_usd": 0.0,
        "funding_cash_usd": 0.0,
        "gross_total_usd": 0.0,
        "maker_vol_usd": 0.0,
        "taker_vol_usd": 0.0,
        "total_vol_usd": 0.0,
        "total_entry_notl_usd": 0.0,
        "avg_hold_ms": 0.0,
        "depth_clipped_entries": 0,
        "depth_clipped_exits": 0,
        "terminal_flat_all": True,
        "terminal_residual_notional_usd": 0.0,
        "exits_by_reason": {},
    }


def _finalize_trade_metrics(
    *,
    trades: int,
    long_trades: int,
    pv_trades: int,
    pv_unverifiable: int,
    gross_price_pnl_usd: float,
    funding_cash_usd: float,
    maker_vol_usd: float,
    taker_vol_usd: float,
    total_entry_notl_usd: float,
    hold_ms_list: Sequence[int],
    depth_clipped_entries: int,
    depth_clipped_exits: int,
    terminal_residual_notl_usd: float,
    exits_by_reason: dict[str, int],
) -> dict[str, Any]:
    gross_total = gross_price_pnl_usd + funding_cash_usd
    return {
        "trades": trades,
        "long_trades": long_trades,
        "pv_trades": pv_trades,
        "pv_unverifiable": pv_unverifiable,
        "gross_price_pnl_usd": round(gross_price_pnl_usd, 6),
        "funding_cash_usd": round(funding_cash_usd, 6),
        "gross_total_usd": round(gross_total, 6),
        "maker_vol_usd": round(maker_vol_usd, 4),
        "taker_vol_usd": round(taker_vol_usd, 4),
        "total_vol_usd": round(maker_vol_usd + taker_vol_usd, 4),
        "total_entry_notl_usd": round(total_entry_notl_usd, 4),
        "avg_hold_ms": round(sum(hold_ms_list) / len(hold_ms_list), 1) if hold_ms_list else 0.0,
        "depth_clipped_entries": depth_clipped_entries,
        "depth_clipped_exits": depth_clipped_exits,
        "terminal_flat_all": terminal_residual_notl_usd <= 1e-6,
        "terminal_residual_notional_usd": round(terminal_residual_notl_usd, 4),
        "exits_by_reason": exits_by_reason,
    }


def net_pnl_for_fee_bps(row: dict[str, Any], maker_fee_bps: float, taker_fee_bps: float, stress_bps: float = 0.0) -> float:
    fee = (
        float(row["maker_vol_usd"]) * (maker_fee_bps + stress_bps) * 1e-4
        + float(row["taker_vol_usd"]) * (taker_fee_bps + stress_bps) * 1e-4
    )
    return float(row["gross_total_usd"]) - fee


def aggregate_rows(
    rows: Sequence[dict[str, Any]],
    *,
    venue: str,
) -> dict[str, Any]:
    """Aggregate simulation rows with 0/0, Lighter Prem (0.40/2.80), +1/+2 bps stress, and concentration."""
    prem_mk, prem_tk = (0.40, 2.80) if venue == "lighter" else (0.0, 0.0)
    trades = sum(int(r["trades"]) for r in rows)
    pv_trades = sum(int(r["pv_trades"]) for r in rows)
    gross_px = sum(float(r["gross_price_pnl_usd"]) for r in rows)
    fnd_cash = sum(float(r["funding_cash_usd"]) for r in rows)
    gross_tot = sum(float(r["gross_total_usd"]) for r in rows)
    mk_vol = sum(float(r["maker_vol_usd"]) for r in rows)
    tk_vol = sum(float(r["taker_vol_usd"]) for r in rows)
    tot_vol = mk_vol + tk_vol
    res_notl = sum(float(r["terminal_residual_notional_usd"]) for r in rows)
    clipped_ent = sum(int(r["depth_clipped_entries"]) for r in rows)
    clipped_ex = sum(int(r["depth_clipped_exits"]) for r in rows)

    net_0_0 = gross_tot
    net_stress_1 = sum(net_pnl_for_fee_bps(r, 0.0, 0.0, 1.0) for r in rows)
    net_stress_2 = sum(net_pnl_for_fee_bps(r, 0.0, 0.0, 2.0) for r in rows)
    net_prem = sum(net_pnl_for_fee_bps(r, prem_mk, prem_tk, 0.0) for r in rows)

    by_win: dict[str, float] = defaultdict(float)
    by_entity: dict[str, float] = defaultdict(float)
    for r in rows:
        by_win[str(r["win"])] += float(r["gross_total_usd"])
        ent = str(r.get("pair_id") or r.get("symbol") or "unknown")
        by_entity[ent] += float(r["gross_total_usd"])

    win_vals = [by_win[w] for w in sorted(by_win)]
    pos_wins = sum(1 for v in win_vals if v > 0.0)
    cum = peak = max_dd = 0.0
    for v in win_vals:
        cum += v
        peak = max(peak, cum)
        max_dd = max(max_dd, peak - cum)

    top_ent = max(by_entity.items(), key=lambda kv: kv[1])[0] if by_entity else None
    ex_top1_usd = (net_0_0 - by_entity[top_ent]) if top_ent is not None else 0.0

    # Realized vs MTM: residual notional is small / tracked explicitly; realized_only deducts residual mark
    return {
        "rows": len(rows),
        "unique_windows": len(win_vals),
        "trades": trades,
        "pv_trades": pv_trades,
        "gross_price_pnl_usd": round(gross_px, 4),
        "funding_cash_usd": round(fnd_cash, 4),
        "net_0_0_usd": round(net_0_0, 4),
        "net_prem_usd": round(net_prem, 4),
        "net_0_0_plus_1bp_usd": round(net_stress_1, 4),
        "net_0_0_plus_2bp_usd": round(net_stress_2, 4),
        "net_0_0_bps_per_vol": round((net_0_0 / tot_vol) * 1e4, 4) if tot_vol > 0.0 else 0.0,
        "maker_vol_usd": round(mk_vol, 2),
        "taker_vol_usd": round(tk_vol, 2),
        "total_vol_usd": round(tot_vol, 2),
        "positive_windows": pos_wins,
        "positive_win_rate": round(pos_wins / len(win_vals), 4) if win_vals else 0.0,
        "max_dd_0_0_usd": round(max_dd, 4),
        "top_contributor": top_ent,
        "top_contributor_usd": round(by_entity[top_ent], 4) if top_ent is not None else 0.0,
        "ex_top1_net_0_0_usd": round(ex_top1_usd, 4),
        "depth_clipped_entries": clipped_ent,
        "depth_clipped_exits": clipped_ex,
        "terminal_flat_all": res_notl <= 1e-6,
        "terminal_residual_notional_usd": round(res_notl, 4),
    }


# ---------------------------------------------------------------------------
# Full W01-W26 Research Pipeline
# ---------------------------------------------------------------------------


def run_research(
    lakehouse_dir: Path,
    *,
    fee_evidence_path: Path | None = None,
    output_path: Path | None = None,
    clip_usd: float = CLIP_USD_DEFAULT,
) -> dict[str, Any]:
    windows = discover_windows_w01_w26(lakehouse_dir)
    if not windows:
        raise RuntimeError(f"No w01..w26 partitions discovered under {lakehouse_dir}")

    fee_meta = resolve_all_fee_models(fee_evidence_path)

    # Accumulators
    # Family A: raw rows keyed by (venue, exec_mode, tau_ms, z_entry, split, pair_id, win)
    fam_a_rows: list[dict[str, Any]] = []
    # Family B: raw rows keyed by (venue, exec_mode, min_disloc, spread_mult, fnd_align, split, symbol, win)
    fam_b_rows: list[dict[str, Any]] = []
    market_coverage: dict[str, dict[str, int]] = {"lighter_robinhood": defaultdict(int), "lighter": defaultdict(int)}

    win_order = sorted(windows.keys())
    for wid in win_order:
        winfo = windows[wid]
        split_name = winfo["split"]
        campaigns = winfo["campaigns"]
        for venue in ("lighter_robinhood", "lighter"):
            bundle = load_window_venue_bundle(lakehouse_dir, venue, campaigns)
            bbo = bundle["bbo"]
            bbo_ts = bundle["bbo_ts"]
            cont = bundle["cont"]
            trades_map = bundle["trades"]
            ora = bundle["oracle"]
            ora_ts = bundle["oracle_ts"]

            for sym, cinfo in cont.items():
                if cinfo.get("is_continuous_10h"):
                    market_coverage[venue][sym] += 1

            # --- Family A: Pairs Stat-Arb ---
            for sym_y, sym_x, cluster in STAT_ARB_PAIRS:
                if sym_y not in bbo or sym_x not in bbo:
                    continue
                if not (cont[sym_y]["is_continuous_10h"] and cont[sym_x]["is_continuous_10h"]):
                    continue
                pair_id = f"{sym_y}-{sym_x}"
                for tau_ms in PAIRS_LOOKBACK_GRID_MS:
                    sig = precompute_pairs_signal(
                        bbo[sym_y],
                        bbo_ts[sym_y],
                        bbo[sym_x],
                        bbo_ts[sym_x],
                        ewma_tau_ms=tau_ms,
                        y_tr=trades_map.get(sym_y),
                        x_tr=trades_map.get(sym_x),
                        y_ora=ora.get(sym_y, ()),
                        y_ora_ts=ora_ts.get(sym_y, ()),
                        x_ora=ora.get(sym_x, ()),
                        x_ora_ts=ora_ts.get(sym_x, ()),
                    )
                    if sig is None:
                        continue
                    for z_ent in PAIRS_Z_ENTRY_GRID:
                        for exec_mode in ("taker", "maker_postonly"):
                            res = simulate_pairs_signal(
                                sig,
                                z_entry=z_ent,
                                exec_mode=exec_mode,
                                clip_usd=clip_usd,
                            )
                            fam_a_rows.append(
                                {
                                    "venue": venue,
                                    "split": split_name,
                                    "win": wid,
                                    "pair_id": pair_id,
                                    "cluster": cluster,
                                    "tau_ms": tau_ms,
                                    "z_entry": z_ent,
                                    "exec_mode": exec_mode,
                                    **res,
                                }
                            )

            # --- Family B: Perp-vs-Oracle Dislocation Reversion ---
            for sym in sorted(VENUE_MARKETS[venue].keys()):
                if sym not in bbo or sym not in ora:
                    continue
                if not cont[sym]["is_continuous_10h"]:
                    continue
                sig_o = precompute_oracle_signal(
                    bbo[sym],
                    bbo_ts[sym],
                    ora[sym],
                    ora_ts[sym],
                    tr=trades_map.get(sym),
                )
                if sig_o is None:
                    continue
                for min_dis in ORACLE_MIN_DISLOC_GRID:
                    for smult in ORACLE_SPREAD_MULT_GRID:
                        for fnd_align in ORACLE_FUNDING_FILTER_GRID:
                            for exec_mode in ("taker", "maker_postonly"):
                                res_o = simulate_oracle_signal(
                                    sig_o,
                                    min_dislocation_bps=min_dis,
                                    spread_mult=smult,
                                    require_funding_alignment=fnd_align,
                                    exec_mode=exec_mode,
                                    clip_usd=clip_usd,
                                )
                                fam_b_rows.append(
                                    {
                                        "venue": venue,
                                        "split": split_name,
                                        "win": wid,
                                        "symbol": sym,
                                        "min_disloc_bps": min_dis,
                                        "spread_mult": smult,
                                        "funding_align": fnd_align,
                                        "exec_mode": exec_mode,
                                        **res_o,
                                    }
                                )

            del bundle
            gc.collect()

    # -----------------------------------------------------------------------
    # Causal Selection strictly on `selection_dev (w01-w20)` -> Frozen OOS
    # -----------------------------------------------------------------------
    dev_wins_all = sorted([w for w, info in windows.items() if info["split"] == "selection_dev"])
    all_wins_sorted = sorted(windows.keys())

    fam_a_out: dict[str, Any] = {}
    fam_b_out: dict[str, Any] = {}

    for venue in ("lighter_robinhood", "lighter"):
        fam_a_out[venue] = {}
        fam_b_out[venue] = {}

        for exec_mode in ("taker", "maker_postonly"):
            # --- Family A Selection ---
            va_rows = [r for r in fam_a_rows if r["venue"] == venue and r["exec_mode"] == exec_mode]
            dev_rows_a = [r for r in va_rows if r["split"] == "selection_dev"]

            # 1) Global config grid on selection_dev (across all available pairs)
            cfg_grid_a: dict[tuple[int, float], dict[str, Any]] = {}
            trial_series_dev_a: dict[str, list[float]] = {}
            trial_series_all_a: dict[str, list[float]] = {}
            for tau_ms in PAIRS_LOOKBACK_GRID_MS:
                for z_ent in PAIRS_Z_ENTRY_GRID:
                    sub_dev = [r for r in dev_rows_a if r["tau_ms"] == tau_ms and r["z_entry"] == z_ent]
                    sub_all = [r for r in va_rows if r["tau_ms"] == tau_ms and r["z_entry"] == z_ent]
                    agg_dev = aggregate_rows(sub_dev, venue=venue)
                    cfg_grid_a[(tau_ms, z_ent)] = agg_dev
                    tname = f"tau{tau_ms // 60000}m_z{z_ent}"
                    w_map_dev = defaultdict(float)
                    for r in sub_dev:
                        w_map_dev[r["win"]] += float(r["gross_total_usd"])
                    trial_series_dev_a[tname] = [w_map_dev[w] for w in dev_wins_all]
                    w_map_all = defaultdict(float)
                    for r in sub_all:
                        w_map_all[r["win"]] += float(r["gross_total_usd"])
                    trial_series_all_a[tname] = [w_map_all[w] for w in all_wins_sorted]

            # Best global config on selection_dev (requiring >= 20 trades)
            elig_cfg_a = [(k, v) for k, v in cfg_grid_a.items() if v["trades"] >= 20]
            if not elig_cfg_a:
                elig_cfg_a = list(cfg_grid_a.items())
            best_global_cfg_a = max(elig_cfg_a, key=lambda kv: (kv[1]["net_0_0_usd"], kv[1]["trades"]))[0]

            # 2) Per-pair best config & positive-dev pair freeze strictly on selection_dev
            frozen_pairs_a: dict[str, dict[str, Any]] = {}
            pair_league_dev_a: dict[str, Any] = {}
            for sym_y, sym_x, cluster in STAT_ARB_PAIRS:
                pid = f"{sym_y}-{sym_x}"
                p_dev = [r for r in dev_rows_a if r["pair_id"] == pid]
                if not p_dev:
                    continue
                best_p_cfg = None
                best_p_agg = None
                for tau_ms in PAIRS_LOOKBACK_GRID_MS:
                    for z_ent in PAIRS_Z_ENTRY_GRID:
                        sub = [r for r in p_dev if r["tau_ms"] == tau_ms and r["z_entry"] == z_ent]
                        if not sub:
                            continue
                        agg = aggregate_rows(sub, venue=venue)
                        if best_p_agg is None or (agg["net_0_0_usd"], agg["trades"]) > (
                            best_p_agg["net_0_0_usd"],
                            best_p_agg["trades"],
                        ):
                            best_p_agg = agg
                            best_p_cfg = (tau_ms, z_ent)
                if best_p_cfg is not None and best_p_agg is not None:
                    pair_league_dev_a[pid] = {
                        "cluster": cluster,
                        "best_tau_min": best_p_cfg[0] // 60_000,
                        "best_z_entry": best_p_cfg[1],
                        **best_p_agg,
                    }
                    if best_p_agg["net_0_0_usd"] > 0.0 and best_p_agg["trades"] >= 5:
                        frozen_pairs_a[pid] = {"tau_ms": best_p_cfg[0], "z_entry": best_p_cfg[1]}

            # Evaluate Frozen Portfolios across selection_dev, holdout, w26_oos:
            # (a) All pairs at best global config
            # (b) Frozen positive-dev pairs at their frozen configs (or top-5 if >5)
            top5_pairs_a = sorted(
                frozen_pairs_a.keys(),
                key=lambda p: pair_league_dev_a[p]["net_0_0_usd"],
                reverse=True,
            )[:5]

            splits_eval_a: dict[str, Any] = {}
            for sp in ("selection_dev", "holdout", "w26_oos"):
                sp_all_global = [
                    r
                    for r in va_rows
                    if r["split"] == sp and r["tau_ms"] == best_global_cfg_a[0] and r["z_entry"] == best_global_cfg_a[1]
                ]
                sp_pos_frozen = [
                    r
                    for r in va_rows
                    if r["split"] == sp
                    and r["pair_id"] in frozen_pairs_a
                    and r["tau_ms"] == frozen_pairs_a[r["pair_id"]]["tau_ms"]
                    and r["z_entry"] == frozen_pairs_a[r["pair_id"]]["z_entry"]
                ]
                sp_top5_frozen = [r for r in sp_pos_frozen if r["pair_id"] in top5_pairs_a]
                splits_eval_a[sp] = {
                    "all_pairs_best_global": aggregate_rows(sp_all_global, venue=venue),
                    "frozen_positive_dev_pairs": aggregate_rows(sp_pos_frozen, venue=venue),
                    "frozen_top5_pairs": aggregate_rows(sp_top5_frozen, venue=venue),
                }

            # Per-pair OOS breakdown on holdout + w26_oos for frozen pairs
            frozen_pair_oos_a: dict[str, Any] = {}
            for pid, pcfg in sorted(frozen_pairs_a.items()):
                h_rows = [
                    r
                    for r in va_rows
                    if r["split"] in ("holdout", "w26_oos")
                    and r["pair_id"] == pid
                    and r["tau_ms"] == pcfg["tau_ms"]
                    and r["z_entry"] == pcfg["z_entry"]
                ]
                frozen_pair_oos_a[pid] = aggregate_rows(h_rows, venue=venue)

            fam_a_out[venue][exec_mode] = {
                "best_global_cfg": {
                    "tau_min": best_global_cfg_a[0] // 60_000,
                    "z_entry": best_global_cfg_a[1],
                },
                "selection_dev_grid": {
                    f"tau{t // 60000}m_z{z}": agg for (t, z), agg in sorted(cfg_grid_a.items())
                },
                "selection_dev_pair_league": pair_league_dev_a,
                "frozen_positive_dev_pairs": {
                    p: {"tau_min": c["tau_ms"] // 60_000, "z_entry": c["z_entry"]}
                    for p, c in sorted(frozen_pairs_a.items())
                },
                "frozen_top5_pairs": top5_pairs_a,
                "splits": splits_eval_a,
                "frozen_pairs_oos_combined": frozen_pair_oos_a,
                "multiple_testing_dev": compute_multiple_testing_metrics(trial_series_dev_a),
                "multiple_testing_w01_w26": compute_multiple_testing_metrics(trial_series_all_a),
            }

            # --- Family B Selection ---
            vb_rows = [r for r in fam_b_rows if r["venue"] == venue and r["exec_mode"] == exec_mode]
            dev_rows_b = [r for r in vb_rows if r["split"] == "selection_dev"]

            cfg_grid_b: dict[tuple[float, float, bool], dict[str, Any]] = {}
            trial_series_dev_b: dict[str, list[float]] = {}
            trial_series_all_b: dict[str, list[float]] = {}
            for min_dis in ORACLE_MIN_DISLOC_GRID:
                for smult in ORACLE_SPREAD_MULT_GRID:
                    for fnd_align in ORACLE_FUNDING_FILTER_GRID:
                        sub_dev = [
                            r
                            for r in dev_rows_b
                            if r["min_disloc_bps"] == min_dis
                            and r["spread_mult"] == smult
                            and r["funding_align"] == fnd_align
                        ]
                        sub_all = [
                            r
                            for r in vb_rows
                            if r["min_disloc_bps"] == min_dis
                            and r["spread_mult"] == smult
                            and r["funding_align"] == fnd_align
                        ]
                        agg_dev = aggregate_rows(sub_dev, venue=venue)
                        cfg_grid_b[(min_dis, smult, fnd_align)] = agg_dev
                        tname = f"d{int(min_dis)}_s{smult}_f{int(fnd_align)}"
                        w_map_dev = defaultdict(float)
                        for r in sub_dev:
                            w_map_dev[r["win"]] += float(r["gross_total_usd"])
                        trial_series_dev_b[tname] = [w_map_dev[w] for w in dev_wins_all]
                        w_map_all = defaultdict(float)
                        for r in sub_all:
                            w_map_all[r["win"]] += float(r["gross_total_usd"])
                        trial_series_all_b[tname] = [w_map_all[w] for w in all_wins_sorted]

            elig_cfg_b = [(k, v) for k, v in cfg_grid_b.items() if v["trades"] >= 20]
            if not elig_cfg_b:
                elig_cfg_b = list(cfg_grid_b.items())
            best_global_cfg_b = max(elig_cfg_b, key=lambda kv: (kv[1]["net_0_0_usd"], kv[1]["trades"]))[0]

            # Per-symbol league & frozen positive-dev symbols strictly on selection_dev
            frozen_syms_b: dict[str, dict[str, Any]] = {}
            sym_league_dev_b: dict[str, Any] = {}
            for sym in sorted(VENUE_MARKETS[venue].keys()):
                s_dev = [r for r in dev_rows_b if r["symbol"] == sym]
                if not s_dev:
                    continue
                best_s_cfg = None
                best_s_agg = None
                for min_dis in ORACLE_MIN_DISLOC_GRID:
                    for smult in ORACLE_SPREAD_MULT_GRID:
                        for fnd_align in ORACLE_FUNDING_FILTER_GRID:
                            sub = [
                                r
                                for r in s_dev
                                if r["min_disloc_bps"] == min_dis
                                and r["spread_mult"] == smult
                                and r["funding_align"] == fnd_align
                            ]
                            if not sub:
                                continue
                            agg = aggregate_rows(sub, venue=venue)
                            if best_s_agg is None or (agg["net_0_0_usd"], agg["trades"]) > (
                                best_s_agg["net_0_0_usd"],
                                best_s_agg["trades"],
                            ):
                                best_s_agg = agg
                                best_s_cfg = (min_dis, smult, fnd_align)
                if best_s_cfg is not None and best_s_agg is not None:
                    sym_league_dev_b[sym] = {
                        "best_min_disloc_bps": best_s_cfg[0],
                        "best_spread_mult": best_s_cfg[1],
                        "best_funding_align": best_s_cfg[2],
                        **best_s_agg,
                    }
                    if best_s_agg["net_0_0_usd"] > 0.0 and best_s_agg["trades"] >= 5:
                        frozen_syms_b[sym] = {
                            "min_disloc_bps": best_s_cfg[0],
                            "spread_mult": best_s_cfg[1],
                            "funding_align": best_s_cfg[2],
                        }

            top6_syms_b = sorted(
                frozen_syms_b.keys(),
                key=lambda s: sym_league_dev_b[s]["net_0_0_usd"],
                reverse=True,
            )[:6]

            splits_eval_b: dict[str, Any] = {}
            for sp in ("selection_dev", "holdout", "w26_oos"):
                sp_all_global = [
                    r
                    for r in vb_rows
                    if r["split"] == sp
                    and r["min_disloc_bps"] == best_global_cfg_b[0]
                    and r["spread_mult"] == best_global_cfg_b[1]
                    and r["funding_align"] == best_global_cfg_b[2]
                ]
                sp_pos_frozen = [
                    r
                    for r in vb_rows
                    if r["split"] == sp
                    and r["symbol"] in frozen_syms_b
                    and r["min_disloc_bps"] == frozen_syms_b[r["symbol"]]["min_disloc_bps"]
                    and r["spread_mult"] == frozen_syms_b[r["symbol"]]["spread_mult"]
                    and r["funding_align"] == frozen_syms_b[r["symbol"]]["funding_align"]
                ]
                sp_top6_frozen = [r for r in sp_pos_frozen if r["symbol"] in top6_syms_b]
                splits_eval_b[sp] = {
                    "all_symbols_best_global": aggregate_rows(sp_all_global, venue=venue),
                    "frozen_positive_dev_symbols": aggregate_rows(sp_pos_frozen, venue=venue),
                    "frozen_top6_symbols": aggregate_rows(sp_top6_frozen, venue=venue),
                }

            # Also check global config ex-Crypto / RWA-only vs Crypto-only on all splits
            domain_breakdown_b: dict[str, Any] = {}
            for sp in ("selection_dev", "holdout", "w26_oos"):
                sp_gl = [
                    r
                    for r in vb_rows
                    if r["split"] == sp
                    and r["min_disloc_bps"] == best_global_cfg_b[0]
                    and r["spread_mult"] == best_global_cfg_b[1]
                    and r["funding_align"] == best_global_cfg_b[2]
                ]
                domain_breakdown_b[sp] = {
                    "crypto_only": aggregate_rows(
                        [r for r in sp_gl if r["symbol"] in ("BTC", "ETH", "SOL")], venue=venue
                    ),
                    "rwa_only": aggregate_rows(
                        [r for r in sp_gl if r["symbol"] not in ("BTC", "ETH", "SOL")], venue=venue
                    ),
                }

            frozen_sym_oos_b: dict[str, Any] = {}
            for sym, scfg in sorted(frozen_syms_b.items()):
                h_rows = [
                    r
                    for r in vb_rows
                    if r["split"] in ("holdout", "w26_oos")
                    and r["symbol"] == sym
                    and r["min_disloc_bps"] == scfg["min_disloc_bps"]
                    and r["spread_mult"] == scfg["spread_mult"]
                    and r["funding_align"] == scfg["funding_align"]
                ]
                frozen_sym_oos_b[sym] = aggregate_rows(h_rows, venue=venue)

            fam_b_out[venue][exec_mode] = {
                "best_global_cfg": {
                    "min_disloc_bps": best_global_cfg_b[0],
                    "spread_mult": best_global_cfg_b[1],
                    "funding_align": best_global_cfg_b[2],
                },
                "selection_dev_grid": {
                    f"d{int(d)}_s{s}_f{int(f)}": agg for (d, s, f), agg in sorted(cfg_grid_b.items())
                },
                "selection_dev_symbol_league": sym_league_dev_b,
                "frozen_positive_dev_symbols": frozen_syms_b,
                "frozen_top6_symbols": top6_syms_b,
                "splits": splits_eval_b,
                "domain_breakdown_best_global": domain_breakdown_b,
                "frozen_symbols_oos_combined": frozen_sym_oos_b,
                "multiple_testing_dev": compute_multiple_testing_metrics(trial_series_dev_b),
                "multiple_testing_w01_w26": compute_multiple_testing_metrics(trial_series_all_b),
            }

    out = {
        "schema": "stat_arb_and_oracle_reversion_w01_w26_v1",
        "protocol": {
            "selection_dev": [w for w, i in sorted(windows.items()) if i["split"] == "selection_dev"],
            "holdout": [w for w, i in sorted(windows.items()) if i["split"] == "holdout"],
            "w26_oos": [w for w, i in sorted(windows.items()) if i["split"] == "w26_oos"],
            "excluded_windows": ["w08", "w27+"],
            "clip_usd": clip_usd,
            "taker_delay_ms": TAKER_DELAY_MS,
            "maker_delay_ms": MAKER_DELAY_MS,
            "maker_chase_window_ms": MAKER_CHASE_WINDOW_MS,
            "l1_depth_cap_frac": L1_DEPTH_CAP_FRAC,
            "step_ms": STEP_MS,
            "bbo_bucket_ms": BBO_BUCKET_MS,
        },
        "fee_models": fee_meta,
        "market_coverage_continuous_10h_windows": {
            v: dict(sorted(m.items())) for v, m in market_coverage.items()
        },
        "family_a_pairs_stat_arb": fam_a_out,
        "family_b_oracle_dislocation": fam_b_out,
    }

    if output_path is not None:
        output_path.parent.mkdir(parents=True, exist_ok=True)
        output_path.write_text(json.dumps(out, indent=2, allow_nan=False) + "\n", encoding="utf-8")
    return out


# ---------------------------------------------------------------------------
# Pure-Stdlib Self-Test
# ---------------------------------------------------------------------------


def run_self_test() -> dict[str, Any]:
    """Deterministic pure-stdlib synthetic verification of Families A & B + realism checks."""
    # 1. PairsStatArb: synthetic cointegrated pair with shock & reversion
    y_bbo: list[tuple[int, float, float, float, float]] = []
    x_bbo: list[tuple[int, float, float, float, float]] = []
    y_ora: list[tuple[int, float, float]] = []
    x_ora: list[tuple[int, float, float]] = []
    y_tr_t: list[int] = []
    y_tr_s: list[int] = []
    y_tr_p: list[float] = []
    y_tr_sz: list[float] = []
    x_tr_t: list[int] = []
    x_tr_s: list[int] = []
    x_tr_p: list[float] = []
    x_tr_sz: list[float] = []

    t0 = 1_000_000
    for i in range(1_600):
        t = t0 + i * 1_000
        # Oscillating cointegrated series during warmup + reversion shock at i=1000..1150
        osc = 0.0002 * (1.0 if i % 2 == 0 else -1.0)
        shock = -0.0025 if (1000 <= i < 1100) else 0.0
        px_x = 100.0 * math.exp(0.001 * math.sin(i * 0.05))
        px_y = 200.0 * math.exp(0.001 * math.sin(i * 0.05) + osc + shock)
        y_bbo.append((t, px_y * 0.9999, 20.0, px_y * 1.0001, 20.0))
        x_bbo.append((t, px_x * 0.9999, 40.0, px_x * 1.0001, 40.0))
        y_ora.append((t, px_y, -0.5))
        x_ora.append((t, px_x, 0.2))
        # Prints on both sides for PostOnly test
        y_tr_t.append(t + 400)
        y_tr_s.append(-1 if i % 2 == 0 else 1)
        y_tr_p.append(px_y * 0.9999 if i % 2 == 0 else px_y * 1.0001)
        y_tr_sz.append(10.0)
        x_tr_t.append(t + 400)
        x_tr_s.append(1 if i % 2 == 0 else -1)
        x_tr_p.append(px_x * 1.0001 if i % 2 == 0 else px_x * 0.9999)
        x_tr_sz.append(20.0)

    y_ts = [b[0] for b in y_bbo]
    x_ts = [b[0] for b in x_bbo]
    yo_ts = [o[0] for o in y_ora]
    xo_ts = [o[0] for o in x_ora]

    sig_a = precompute_pairs_signal(
        y_bbo, y_ts, x_bbo, x_ts,
        ewma_tau_ms=300_000,
        y_tr=(y_tr_t, y_tr_s, y_tr_p, y_tr_sz),
        x_tr=(x_tr_t, x_tr_s, x_tr_p, x_tr_sz),
        y_ora=y_ora, y_ora_ts=yo_ts,
        x_ora=x_ora, x_ora_ts=xo_ts,
    )
    assert sig_a is not None
    res_a_taker = simulate_pairs_signal(sig_a, z_entry=2.0, exec_mode="taker", clip_usd=500.0)
    res_a_maker = simulate_pairs_signal(sig_a, z_entry=2.0, exec_mode="maker_postonly", clip_usd=500.0)
    assert res_a_taker["trades"] >= 1, f"expected >=1 pairs taker trade, got {res_a_taker}"
    assert res_a_taker["gross_total_usd"] > 0.0, f"expected positive pairs pnl, got {res_a_taker}"
    assert res_a_taker["terminal_flat_all"] is True
    assert res_a_maker["trades"] >= 1, f"expected >=1 pairs maker trade, got {res_a_maker}"

    # 2. OracleDislocationReversion: synthetic dislocation + convergence
    o_bbo: list[tuple[int, float, float, float, float]] = []
    o_ora: list[tuple[int, float, float]] = []
    for i in range(1_400):
        t = t0 + i * 1_000
        disloc = -0.0015 if (1000 <= i < 1080) else 0.0
        px = 100.0 * (1.0 + disloc)
        o_bbo.append((t, px * 0.9999, 30.0, px * 1.0001, 30.0))
        o_ora.append((t, 100.0, -0.5))  # favorable negative funding for long
    sig_b = precompute_oracle_signal(o_bbo, [b[0] for b in o_bbo], o_ora, [o[0] for o in o_ora])
    assert sig_b is not None
    res_b = simulate_oracle_signal(
        sig_b,
        min_dislocation_bps=5.0,
        spread_mult=1.5,
        require_funding_alignment=True,
        exec_mode="taker",
        clip_usd=500.0,
    )
    assert res_b["trades"] >= 1 and res_b["gross_total_usd"] > 0.0, f"expected positive oracle reversion: {res_b}"
    assert res_b["funding_cash_usd"] > 0.0, f"expected positive funding receipt on long with -0.5bps/hr: {res_b}"
    assert res_b["terminal_flat_all"] is True

    # 3. Multiple testing helper smoke test
    mt = compute_multiple_testing_metrics(
        {
            "arm1": [10.0 + (i % 3) for i in range(20)],
            "arm2": [-2.0 + (i % 2) for i in range(20)],
        },
        num_blocks=8,
    )
    assert mt["status"] == "ok" and "dsr" in mt and "cscv" in mt

    return {
        "self_test": "ok",
        "pairs_taker_trades": res_a_taker["trades"],
        "pairs_taker_gross_usd": res_a_taker["gross_total_usd"],
        "pairs_maker_trades": res_a_maker["trades"],
        "oracle_trades": res_b["trades"],
        "oracle_gross_usd": res_b["gross_total_usd"],
        "oracle_funding_usd": res_b["funding_cash_usd"],
        "dsr_effective": mt["dsr"]["dsr_effective"],
        "pbo": mt["cscv"]["pbo"],
    }


def main(argv: Sequence[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--lakehouse", type=Path, default=Path(os.path.expanduser("~/aleph-tx-lakehouse")))
    ap.add_argument(
        "--fee-evidence",
        type=Path,
        default=Path(".agent-state/research/account_fee_evidence_20261127.json"),
    )
    ap.add_argument(
        "--output",
        type=Path,
        default=Path("/tmp/quant_upgrade_20261127/alpha_statarb/stat_arb_oracle_w01_w26.json"),
    )
    ap.add_argument("--clip-usd", type=float, default=CLIP_USD_DEFAULT)
    ap.add_argument("--self-test", action="store_true")
    args = ap.parse_args(argv)

    if args.self_test:
        print(json.dumps(run_self_test(), indent=2, allow_nan=False))
        return 0

    res = run_research(
        args.lakehouse,
        fee_evidence_path=args.fee_evidence,
        output_path=args.output,
        clip_usd=float(args.clip_usd),
    )
    print(
        json.dumps(
            {
                "ok": True,
                "output": str(args.output),
                "selection_dev_windows": len(res["protocol"]["selection_dev"]),
                "holdout_windows": len(res["protocol"]["holdout"]),
                "w26_oos_windows": len(res["protocol"]["w26_oos"]),
            },
            indent=2,
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
