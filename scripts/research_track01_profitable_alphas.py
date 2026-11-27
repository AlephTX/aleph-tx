#!/usr/bin/env python3
"""Track 01 (`01_quant_profit`) Profitable Directional Alphas Research (`W01–W27`).

100% pure-stdlib at module top level (`duckdb` is imported lazily inside lakehouse loader,
runnable under `python3 -S`).

Implements exact Python mirrors of `src/strategy/directional_alpha.rs`:
  1. `OracleDiscoveryMomentum`: Trades in the SAME direction as sustained
     `mark_px - oracle_px` (perp price discovery leading lagging equity/RWA oracles).
  2. `OverreactionReversion`: Fades extreme `15m` / `60m` return overreactions
     (`|ret| >= hurdle`) with spread/funding gating, reversion TP, and time/stop-loss exits.

Strict execution & statistical realism across `W01–W27` (`selection_dev w01–w20` -> frozen
`holdout w21–w25` -> `w26_w27_oos`; strictly excluding `w08` and `-w28`):
  - All 18 `lighter_robinhood` (`USDG`, `0.0/0.0 bps`) and 19 `lighter` (`USDC`,
    verified `0.40/2.80 bps` Premium vs `0.0/0.0 bps` Standard counterfactual) markets.
  - `taker`: real L1 `bid/ask` crossing at `t_decision + 330 ms`
  - `maker_postonly`: resting `PostOnly` at `t_decision + 200 ms`, strictly print-verified
    against `public_trades` over `10 s` (zero fabricated fills) with `330 ms` IOC fallback.
  - Displayed L1 depth clamping (`<= 0.50 * L1_depth`) on entry and iterative slice unwinds.
  - Causal hourly `funding_and_ctx` accrual, explicit realized-only vs residual MTM accounting,
    `+0.5 / +1.0 / +2.0 bps` slippage stress, capacity scaling (`$500` vs `$1,000` clip),
    top-1 concentration checks, and `PSR / N_eff / DSR / CSCV PBO`.
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
DEFAULT_MAX_QUOTE_AGE_MS = 60_000
DEFAULT_MAX_CTX_AGE_MS = 60_000
DEFAULT_MAX_GAP_MS = 60_000
BPS_SCALE = 10_000.0

# Verified full market ID maps across W01-W27
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

CRYPTO_SET: frozenset[str] = frozenset({"BTC", "ETH", "SOL"})

# Frozen Family 1 grid (`OracleDiscoveryMomentum`, 24 arms)
MOM_MIN_DISLOC_GRID: tuple[float, ...] = (12.0, 15.0, 20.0, 25.0)
MOM_MAX_HOLD_MS_GRID: tuple[int, ...] = (300_000, 900_000, 1_800_000)
MOM_FUNDING_ALIGN_GRID: tuple[bool, ...] = (False, True)
MOM_SPREAD_MULT: float = 1.5
MOM_TAKE_PROFIT_BPS: float = 15.0
MOM_STOP_LOSS_BPS: float = 25.0
MOM_MAX_DISLOC_BPS: float = 250.0
MOM_MAX_ENTRY_SPREAD_BPS: float = 20.0
MOM_MAX_ADVERSE_FUNDING_BPS: float = 1.0
MOM_COOLDOWN_MS: int = 60_000
MOM_WARMUP_MS: int = 60_000
MOM_MIN_SAMPLES: int = 5

# Frozen Family 2 grid (`OverreactionReversion`, 32 arms)
REV_LOOKBACK_BARS_GRID: tuple[int, ...] = (15, 60)
REV_MIN_RETURN_GRID: tuple[float, ...] = (25.0, 30.0, 45.0, 60.0)
REV_TARGET_FRAC_GRID: tuple[float, ...] = (0.0, 0.40)
REV_FUNDING_ALIGN_GRID: tuple[bool, ...] = (False, True)
REV_BAR_MS: int = 60_000
REV_SPREAD_MULT: float = 2.0
REV_MAX_RETURN_BPS: float = 500.0
REV_STOP_LOSS_BPS: float = 45.0
REV_MAX_ENTRY_SPREAD_BPS: float = 20.0
REV_MAX_ADVERSE_FUNDING_BPS: float = 1.0
REV_COOLDOWN_MS: int = 300_000


def _load_local_module(mod_name: str, filename: str) -> Any:
    """Import sibling pure-stdlib module from the same directory."""
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
    """Resolve redacted fee models without exposing any account identifiers."""
    if evidence_path is not None and evidence_path.is_file():
        tfe = _load_local_module("track_fee_evidence", "track_fee_evidence.py")
        ev = tfe.load_track_fee_evidence(evidence_path)
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
    else:
        rh_mk, rh_tk, rh_status, rh_src = 0.0, 0.0, "verified_standard_zero", "lighter_rh_usdg_0_0"
        lt_mk, lt_tk, lt_status, lt_src = 0.4, 2.8, "verified_account_tier", "lighter_premium_0.40_2.80"

    return {
        "lighter_robinhood": {
            "settlement": "USDG",
            "primary_0_0": {
                "maker_bps": rh_mk,
                "taker_bps": rh_tk,
                "status": rh_status,
                "source": rh_src,
            },
        },
        "lighter": {
            "settlement": "USDC",
            "standard_0_0_counterfactual": {
                "maker_bps": 0.0,
                "taker_bps": 0.0,
                "status": "counterfactual_standard_zero",
                "source": "lighter_standard_0_0_counterfactual",
            },
            "premium_verified": {
                "maker_bps": lt_mk,
                "taker_bps": lt_tk,
                "status": lt_status,
                "source": lt_src,
            },
        },
    }


def compute_multiple_testing_metrics(
    trial_series: dict[str, Sequence[float]],
    num_blocks: int = 8,
) -> dict[str, Any]:
    """Compute PSR / N_eff / DSR / MinTRL / CSCV PBO via `audit_multiple_testing_overfitting.py`."""
    if not trial_series:
        return {"status": "empty"}
    amto = _load_local_module("audit_multiple_testing_overfitting", "audit_multiple_testing_overfitting.py")
    names = sorted(trial_series.keys())
    t_len = len(trial_series[names[0]])
    if t_len < 3:
        return {"status": "insufficient_windows", "windows": t_len}
    varying = {k: [float(x) for x in v] for k, v in trial_series.items() if len(set(v)) > 1}
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
# Causal Signal Filters (Exact Python Mirrors of `directional_alpha.rs`)
# ---------------------------------------------------------------------------


@dataclasses.dataclass
class OracleMomStatePy:
    warmup_ms: int = MOM_WARMUP_MS
    min_samples: int = MOM_MIN_SAMPLES
    max_perp_age_ms: int = DEFAULT_MAX_QUOTE_AGE_MS
    max_ctx_age_ms: int = DEFAULT_MAX_CTX_AGE_MS
    max_gap_ms: int = DEFAULT_MAX_GAP_MS
    initialized: bool = False
    first_obs_ms: int | None = None
    last_obs_ms: int | None = None
    sample_count: int = 0

    def reset(self) -> None:
        self.initialized = False
        self.first_obs_ms = None
        self.last_obs_ms = None
        self.sample_count = 0

    def step(
        self,
        now_ms: int,
        quote: tuple[int, float, float, float, float] | None,
        ctx: tuple[int, float, float, float] | None,
    ) -> tuple[int, float, float, float]:
        """Return `(status_code, signal_bps, spread_bps, funding_bps)` (`1=ready`, `0=blocked`)."""
        if self.last_obs_ms is not None and now_ms < self.last_obs_ms:
            return 0, 0.0, 0.0, 0.0
        if quote is None or ctx is None:
            return 0, 0.0, 0.0, 0.0
        q_ts, bp, bs, ap, aks = quote
        c_ts, mark_px, oracle_px, fnd_bps = ctx
        if q_ts > now_ms or c_ts > now_ms:
            return 0, 0.0, 0.0, 0.0
        if not (
            math.isfinite(bp)
            and math.isfinite(ap)
            and math.isfinite(mark_px)
            and math.isfinite(oracle_px)
            and math.isfinite(fnd_bps)
            and bp > 0.0
            and ap > bp
            and mark_px > 0.0
            and oracle_px > 0.0
        ):
            return 0, 0.0, 0.0, 0.0
        mid = 0.5 * (bp + ap)
        spread_bps = ((ap - bp) / mid) * BPS_SCALE
        if (now_ms - q_ts) > self.max_perp_age_ms or (now_ms - c_ts) > self.max_ctx_age_ms:
            return 0, 0.0, spread_bps, fnd_bps
        if self.last_obs_ms is not None and (now_ms - self.last_obs_ms) > self.max_gap_ms:
            self.reset()

        if not self.initialized:
            self.initialized = True
            self.first_obs_ms = now_ms
            self.last_obs_ms = now_ms
            self.sample_count = 1
        elif now_ms > self.last_obs_ms:
            self.last_obs_ms = now_ms
            self.sample_count += 1

        signal_bps = ((mark_px - oracle_px) / oracle_px) * BPS_SCALE
        warmed = (
            self.first_obs_ms is not None
            and (now_ms - self.first_obs_ms) >= self.warmup_ms
            and self.sample_count >= self.min_samples
        )
        return (1 if warmed else 0), signal_bps, spread_bps, fnd_bps


@dataclasses.dataclass
class OverreactionBarRingPy:
    bar_ms: int = REV_BAR_MS
    max_perp_age_ms: int = DEFAULT_MAX_QUOTE_AGE_MS
    max_gap_ms: int = 180_000
    max_bars: int = 256
    last_obs_ms: int | None = None
    current_bar: tuple[int, float] | None = None
    closes: list[float] = dataclasses.field(default_factory=list)

    def reset(self) -> None:
        self.last_obs_ms = None
        self.current_bar = None
        self.closes.clear()

    def step(
        self,
        now_ms: int,
        quote: tuple[int, float, float, float, float] | None,
        ctx: tuple[int, float, float, float] | None,
    ) -> tuple[int, float, float, float, float]:
        """Return `(ready_flag, ret_15m_bps, ret_60m_bps, spread_bps, funding_bps)`."""
        if self.last_obs_ms is not None and now_ms < self.last_obs_ms:
            return 0, 0.0, 0.0, 0.0, 0.0
        if quote is None:
            return 0, 0.0, 0.0, 0.0, 0.0
        q_ts, bp, _, ap, _ = quote
        fnd_bps = 0.0
        if ctx is not None:
            c_ts, _, _, f_val = ctx
            if c_ts <= now_ms and (now_ms - c_ts) <= DEFAULT_MAX_CTX_AGE_MS and math.isfinite(f_val):
                fnd_bps = f_val
        if q_ts > now_ms or not (math.isfinite(bp) and math.isfinite(ap) and bp > 0.0 and ap > bp):
            return 0, 0.0, 0.0, 0.0, fnd_bps
        mid = 0.5 * (bp + ap)
        spread_bps = ((ap - bp) / mid) * BPS_SCALE
        if (now_ms - q_ts) > self.max_perp_age_ms:
            return 0, 0.0, 0.0, spread_bps, fnd_bps
        if self.last_obs_ms is not None and (now_ms - self.last_obs_ms) > self.max_gap_ms:
            self.reset()
        self.last_obs_ms = now_ms

        bar_idx = now_ms // self.bar_ms
        if self.current_bar is None:
            self.current_bar = (bar_idx, mid)
        elif bar_idx > self.current_bar[0]:
            self.closes.append(self.current_bar[1])
            if len(self.closes) > self.max_bars:
                del self.closes[0]
            self.current_bar = (bar_idx, mid)
        else:
            self.current_bar = (bar_idx, mid)

        r15 = ((mid / self.closes[-15]) - 1.0) * BPS_SCALE if len(self.closes) >= 15 else math.nan
        r60 = ((mid / self.closes[-60]) - 1.0) * BPS_SCALE if len(self.closes) >= 60 else math.nan
        ready = 1 if (len(self.closes) >= 15) else 0
        return ready, r15, r60, spread_bps, fnd_bps


# ---------------------------------------------------------------------------
# Lakehouse Discovery & DuckDB Loader (W01-W27 ONLY; w08 & -w28 EXCLUDED)
# ---------------------------------------------------------------------------


def discover_windows_w01_w27(lakehouse_dir: Path) -> dict[str, dict[str, Any]]:
    """Discover `w01..w27` partitions (strictly excluding `w08` and `-w28` / `w28+`)."""
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
            if cid.endswith("-w28"):
                continue
            m = re.match(r"^tokyo-(crypto|rwa)-.*-w(\d+)$", cid)
            if not m:
                continue
            dom = m.group(1)
            w_num = int(m.group(2))
            if w_num < 1 or w_num > 27 or w_num == 8:
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
        split = "selection_dev" if w_num <= 20 else ("holdout" if w_num <= 25 else "w26_w27_oos")
        out[wid] = {
            "win": wid,
            "w_num": w_num,
            "split": split,
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
        if cid.endswith("-w28"):
            continue
        m = re.search(r"-w(\d+)$", cid)
        if not m or not (1 <= int(m.group(1)) <= 27):
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
    """Load BBO (500ms buckets), public_trades, and funding_and_ctx (`mark_px, oracle_px, funding_bps`)."""
    import duckdb

    sym_to_mkt = VENUE_MARKETS[venue]
    mkt_to_sym = {m: s for s, m in sym_to_mkt.items()}
    con = duckdb.connect(":memory:")
    con.execute("SET preserve_insertion_order=false; SET threads=2; SET memory_limit='2GB';")

    bbo_by_sym: dict[str, list[tuple[int, float, float, float, float]]] = {}
    bbo_ts_by_sym: dict[str, list[int]] = {}
    cont_by_sym: dict[str, dict[str, Any]] = {}
    trades_by_sym: dict[str, tuple[list[int], list[int], list[float], list[float]]] = {}
    ctx_by_sym: dict[str, list[tuple[int, float, float, float]]] = {}
    ctx_ts_by_sym: dict[str, list[int]] = {}

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
                   arg_max(mark_px_f64, available_at_ms) AS mark_px,
                   arg_max(oracle_px_f64, available_at_ms) AS oracle_px,
                   arg_max(COALESCE(funding_rate_bps_f64, 0.0), available_at_ms) AS fnd_bps
            FROM read_parquet({ff_sql})
            WHERE market IN ({syms_sql}) AND oracle_px_f64 > 0 AND mark_px_f64 > 0
            GROUP BY market, (available_at_ms // {STEP_MS})
            ORDER BY market, ts_ms ASC
            """
        ).fetchall()
        grp_c: dict[str, list[tuple[int, float, float, float]]] = defaultdict(list)
        grp_cts: dict[str, list[int]] = defaultdict(list)
        for sym, ts_ms, mpx, opx, fnd in f_rows:
            ssym = str(sym)
            if ssym in sym_to_mkt:
                its = int(ts_ms)
                grp_c[ssym].append((its, float(mpx), float(opx), float(fnd)))
                grp_cts[ssym].append(its)
        ctx_by_sym = dict(grp_c)
        ctx_ts_by_sym = dict(grp_cts)

    con.close()
    return {
        "bbo": bbo_by_sym,
        "bbo_ts": bbo_ts_by_sym,
        "cont": cont_by_sym,
        "trades": trades_by_sym,
        "ctx": ctx_by_sym,
        "ctx_ts": ctx_ts_by_sym,
    }


# ---------------------------------------------------------------------------
# Execution & Funding Realism Helpers
# ---------------------------------------------------------------------------


def _latest_at(rows: Sequence[Any], ts_arr: Sequence[int], ts_query: int) -> Any | None:
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
    """Strict `public_trades` print verification for a resting PostOnly limit order."""
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
    ctx_rows: Sequence[tuple[int, float, float, float]],
    ctx_ts: Sequence[int],
    t_start_ms: int,
    t_end_ms: int,
    signed_notional_usd: float,
) -> float:
    """Integrate causal hourly `funding_and_ctx` cash over `[t_start_ms, t_end_ms]`."""
    if t_end_ms <= t_start_ms or not ctx_rows or abs(signed_notional_usd) <= 0.0:
        return 0.0
    i0 = bisect.bisect_right(ctx_ts, t_start_ms) - 1
    if i0 < 0:
        i0 = 0
    i1 = bisect.bisect_right(ctx_ts, t_end_ms) - 1
    if i1 < i0:
        return 0.0

    cash = 0.0
    cur_t = t_start_ms
    idx = i0
    while cur_t < t_end_ms:
        rate_bps = ctx_rows[idx][3]
        nxt_t = ctx_ts[idx + 1] if (idx + 1) < len(ctx_ts) else t_end_ms
        seg_end = min(t_end_ms, nxt_t)
        if seg_end > cur_t and math.isfinite(rate_bps):
            dt_hours = (seg_end - cur_t) / 3_600_000.0
            cash += -signed_notional_usd * (rate_bps * 1e-4) * dt_hours
        cur_t = seg_end
        if idx + 1 < len(ctx_ts) and ctx_ts[idx + 1] <= cur_t:
            idx += 1
        elif seg_end >= t_end_ms:
            break
        else:
            idx += 1
            if idx >= len(ctx_ts):
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
# Precomputed Signal Data & Simulators
# ---------------------------------------------------------------------------


@dataclasses.dataclass
class PrecomputedDirectionalSeries:
    t0_ms: int
    t1_ms: int
    ts_ms: list[int]
    mom_ready: list[int]
    mom_sig_bps: list[float]
    rev_ready: list[int]
    rev_r15_bps: list[float]
    rev_r60_bps: list[float]
    spread_bps: list[float]
    fnd_bps: list[float]
    bid_px: list[float]
    ask_px: list[float]
    bbo: Sequence[tuple[int, float, float, float, float]]
    bbo_ts: Sequence[int]
    tr: tuple[list[int], list[int], list[float], list[float]] | None
    ctx: Sequence[tuple[int, float, float, float]]
    ctx_ts: Sequence[int]


def precompute_directional_series(
    bbo: Sequence[tuple[int, float, float, float, float]],
    bbo_ts: Sequence[int],
    ctx: Sequence[tuple[int, float, float, float]],
    ctx_ts: Sequence[int],
    *,
    tr: tuple[list[int], list[int], list[float], list[float]] | None = None,
    step_ms: int = STEP_MS,
) -> PrecomputedDirectionalSeries | None:
    """Precompute both `OracleDiscoveryMomentum` and `OverreactionReversion` causal timelines."""
    if len(bbo) < 100:
        return None
    t0_ms = int(bbo_ts[0])
    if ctx_ts:
        t0_ms = max(t0_ms, int(ctx_ts[0]))
    t1_ms = int(bbo_ts[-1])
    if ctx_ts:
        t1_ms = min(t1_ms, int(ctx_ts[-1]))
    if t1_ms <= t0_ms + 180_000:
        return None

    mom_flt = OracleMomStatePy()
    rev_flt = OverreactionBarRingPy()

    ts_list: list[int] = []
    m_rdy: list[int] = []
    m_sig: list[float] = []
    r_rdy: list[int] = []
    r_15: list[float] = []
    r_60: list[float] = []
    spr_l: list[float] = []
    fnd_l: list[float] = []
    bid_l: list[float] = []
    ask_l: list[float] = []

    ib = 0
    ic = 0
    nb = len(bbo)
    nc = len(ctx)
    cur_b: tuple[int, float, float, float, float] | None = None
    cur_c: tuple[int, float, float, float] | None = None

    t = t0_ms
    while t <= t1_ms:
        while ib < nb and bbo_ts[ib] <= t:
            cur_b = bbo[ib]
            ib += 1
        while ic < nc and ctx_ts[ic] <= t:
            cur_c = ctx[ic]
            ic += 1
        mr, msig, spr, fnd = mom_flt.step(t, cur_b, cur_c)
        rr, r15, r60, spr2, fnd2 = rev_flt.step(t, cur_b, cur_c)
        ts_list.append(t)
        m_rdy.append(mr)
        m_sig.append(msig)
        r_rdy.append(rr)
        r_15.append(r15)
        r_60.append(r60)
        spr_l.append(spr if mr else spr2)
        fnd_l.append(fnd if mr else fnd2)
        if cur_b is not None:
            bid_l.append(cur_b[1])
            ask_l.append(cur_b[3])
        else:
            bid_l.append(0.0)
            ask_l.append(0.0)
        t += step_ms

    return PrecomputedDirectionalSeries(
        t0_ms=t0_ms,
        t1_ms=t1_ms,
        ts_ms=ts_list,
        mom_ready=m_rdy,
        mom_sig_bps=m_sig,
        rev_ready=r_rdy,
        rev_r15_bps=r_15,
        rev_r60_bps=r_60,
        spread_bps=spr_l,
        fnd_bps=fnd_l,
        bid_px=bid_l,
        ask_px=ask_l,
        bbo=bbo,
        bbo_ts=bbo_ts,
        tr=tr,
        ctx=ctx,
        ctx_ts=ctx_ts,
    )


def _execute_entry(
    sig: PrecomputedDirectionalSeries,
    t_dec: int,
    t_stop_entries: int,
    side: int,
    exec_mode: str,
    clip_usd: float,
) -> tuple[bool, float, float, int, bool, bool, bool]:
    """Execute entry with 330ms taker or 200ms print-verified PostOnly.

    Returns `(ok, px_in, notl, t_fill_in, is_maker, depth_clipped, pv_unverifiable)`.
    """
    if exec_mode == "taker":
        t_ent = t_dec + TAKER_DELAY_MS
        f_in = _taker_fill(sig.bbo, sig.bbo_ts, t_ent, side, DEFAULT_MAX_QUOTE_AGE_MS)
        if f_in is None:
            return False, 0.0, 0.0, t_dec, False, False, False
        px_in, l1_in, _ = f_in
        notl = min(clip_usd, L1_DEPTH_CAP_FRAC * l1_in)
        if notl <= 5.0:
            return False, 0.0, 0.0, t_dec, False, False, False
        return True, px_in, notl, t_ent, False, (notl < clip_usd), False

    t_rest = t_dec + MAKER_DELAY_MS
    q_in = _latest_at(sig.bbo, sig.bbo_ts, t_rest)
    if q_in is None or (t_rest - q_in[0]) > DEFAULT_MAX_QUOTE_AGE_MS:
        return False, 0.0, 0.0, t_dec, True, False, False
    rest_px = q_in[1] if side > 0 else q_in[3]
    rest_sz = q_in[2] if side > 0 else q_in[4]
    if rest_px <= 0.0 or rest_sz <= 0.0:
        return False, 0.0, 0.0, t_dec, True, False, False
    l1_in = rest_px * rest_sz
    want_notl = min(clip_usd, L1_DEPTH_CAP_FRAC * l1_in)
    if want_notl <= 5.0:
        return False, 0.0, 0.0, t_dec, True, False, False
    t_chase_end = min(t_rest + MAKER_CHASE_WINDOW_MS, t_stop_entries)
    ok_in, fill_notl, fin_ts = _check_print_fill(
        sig.tr, t_rest, t_chase_end, side, rest_px, want_notl, l1_in
    )
    if not ok_in or fill_notl <= 5.0:
        return False, 0.0, 0.0, t_rest + 1_000, True, False, True
    return True, rest_px, fill_notl, fin_ts, True, (fill_notl < clip_usd), False


def _execute_exit(
    sig: PrecomputedDirectionalSeries,
    j_exit: int,
    side: int,
    px_in: float,
    notl: float,
    t_fill_in: int,
    exit_reason: str,
    passive_exit_reasons: tuple[str, ...],
    exec_mode: str,
    *,
    conserve_exit_depth: bool = True,
) -> tuple[float, float, float, float, float, float, int, bool, float, int]:
    """Execute exit with optional PostOnly chase + iterative 0.5*L1 IOC unwinds + MTM residual.

    When `conserve_exit_depth=True` (default P0 fix):
      - Tracks `rem_avail_by_quote` keyed by displayed quote timestamp so a single book snapshot
        is never reused across subsequent `j_cur` steps or double-counted across maker partial
        exit + taker fallback on the same touch.
      - Enforces strict declared cutoff `ts_ms[j_cur] + TAKER_DELAY_MS <= sig.t1_ms` so exits
        arriving past `t1_ms` stay explicit terminal residual MTM/exposure.

    Returns `(realized_px_pnl, realized_fnd, res_mtm_px_pnl, res_fnd, maker_vol, taker_vol,
              last_fill_out, clipped_here, rem_residual, j_next)`.
    """
    ts_ms = sig.ts_ms
    n = len(ts_ms)
    t_exit_dec = ts_ms[j_exit]
    rem = notl
    last_fill_out = min(sig.t1_ms, t_exit_dec + TAKER_DELAY_MS)
    clipped_here = False
    realized_px_pnl = 0.0
    realized_fnd = 0.0
    maker_vol = 0.0
    taker_vol = 0.0
    rem_avail_by_quote: dict[int, float] = {}

    if exec_mode == "maker_postonly" and exit_reason in passive_exit_reasons:
        t_xr = t_exit_dec + MAKER_DELAY_MS
        t_xc = min(t_xr + MAKER_CHASE_WINDOW_MS, sig.t1_ms - 1_000)
        q_o = _latest_at(sig.bbo, sig.bbo_ts, t_xr)
        if q_o is not None and (t_xr - q_o[0]) <= DEFAULT_MAX_QUOTE_AGE_MS:
            rp = q_o[1] if (-side) > 0 else q_o[3]
            rs = q_o[2] if (-side) > 0 else q_o[4]
            if rp > 0.0 and rs > 0.0:
                q_ts_m = int(q_o[0])
                cap_m = L1_DEPTH_CAP_FRAC * rp * rs
                avail_m = rem_avail_by_quote.get(q_ts_m, cap_m) if conserve_exit_depth else cap_m
                ok_o, fill_o, fo_ts = _check_print_fill(
                    sig.tr, t_xr, t_xc, -side, rp, min(rem, avail_m), rp * rs
                )
                if ok_o and fill_o > 0.0:
                    sl = min(rem, avail_m, fill_o) if conserve_exit_depth else min(rem, fill_o)
                    if conserve_exit_depth:
                        rem_avail_by_quote[q_ts_m] = max(0.0, avail_m - sl)
                    if sl > 0.0:
                        realized_px_pnl += side * (rp / px_in - 1.0) * sl
                        realized_fnd += _funding_cash_usd(
                            sig.ctx, sig.ctx_ts, t_fill_in, fo_ts, side * sl
                        )
                        maker_vol += sl
                        rem -= sl
                        last_fill_out = max(last_fill_out, fo_ts)
        j_cur = max(j_exit, bisect.bisect_left(ts_ms, t_xc))
    else:
        j_cur = j_exit

    while rem > 1e-6 and j_cur < n:
        if conserve_exit_depth and ts_ms[j_cur] + TAKER_DELAY_MS > sig.t1_ms:
            break
        t_ex = (
            ts_ms[j_cur] + TAKER_DELAY_MS
            if conserve_exit_depth
            else min(sig.t1_ms, ts_ms[j_cur] + TAKER_DELAY_MS)
        )
        f_out = _taker_fill(sig.bbo, sig.bbo_ts, t_ex, -side, DEFAULT_MAX_QUOTE_AGE_MS)
        if f_out is not None:
            q_ts = int(f_out[2])
            cap_raw = L1_DEPTH_CAP_FRAC * f_out[1]
            if conserve_exit_depth:
                avail = (
                    min(cap_raw, rem_avail_by_quote[q_ts])
                    if q_ts in rem_avail_by_quote
                    else cap_raw
                )
            else:
                avail = cap_raw
            sl = min(rem, avail)
            if sl < rem - 1e-6:
                clipped_here = True
            if sl > 0.0:
                if conserve_exit_depth:
                    rem_avail_by_quote[q_ts] = max(0.0, avail - sl)
                realized_px_pnl += side * (f_out[0] / px_in - 1.0) * sl
                realized_fnd += _funding_cash_usd(
                    sig.ctx, sig.ctx_ts, t_fill_in, t_ex, side * sl
                )
                taker_vol += sl
                rem -= sl
                last_fill_out = max(last_fill_out, t_ex)
        if rem <= 1e-6:
            break
        j_cur += 1

    res_mtm_px_pnl = 0.0
    res_fnd = 0.0
    if rem > 1e-6:
        last_q = (_latest_at(sig.bbo, sig.bbo_ts, sig.t1_ms) or sig.bbo[-1]) if conserve_exit_depth else sig.bbo[-1]
        px_mtm = last_q[1] if side > 0 else last_q[3]
        res_mtm_px_pnl = side * (px_mtm / px_in - 1.0) * rem
        res_fnd = _funding_cash_usd(sig.ctx, sig.ctx_ts, t_fill_in, sig.t1_ms, side * rem)
    else:
        rem = 0.0

    return (
        realized_px_pnl,
        realized_fnd,
        res_mtm_px_pnl,
        res_fnd,
        maker_vol,
        taker_vol,
        last_fill_out,
        clipped_here,
        rem,
        j_cur,
    )


def _empty_trade_result() -> dict[str, Any]:
    return {
        "trades": 0,
        "long_trades": 0,
        "pv_trades": 0,
        "pv_unverifiable": 0,
        "realized_price_pnl_usd": 0.0,
        "realized_funding_usd": 0.0,
        "realized_only_gross_usd": 0.0,
        "residual_mtm_price_pnl_usd": 0.0,
        "residual_funding_usd": 0.0,
        "mtm_incl_residual_gross_usd": 0.0,
        "maker_vol_usd": 0.0,
        "taker_vol_usd": 0.0,
        "total_vol_usd": 0.0,
        "total_entry_notl_usd": 0.0,
        "avg_hold_ms": 0.0,
        "depth_clipped_entries": 0,
        "depth_clipped_exits": 0,
        "residual_trades": 0,
        "terminal_flat_all": True,
        "terminal_residual_notional_usd": 0.0,
        "exits_by_reason": {},
    }


def _finalize_trade_result(
    *,
    trades: int,
    long_trades: int,
    pv_trades: int,
    pv_unverifiable: int,
    realized_px_pnl: float,
    realized_fnd: float,
    res_mtm_px_pnl: float,
    res_fnd: float,
    maker_vol: float,
    taker_vol: float,
    total_entry_notl: float,
    hold_ms_list: Sequence[int],
    depth_clipped_entries: int,
    depth_clipped_exits: int,
    residual_trades: int,
    terminal_residual_notl: float,
    exits_by_reason: dict[str, int],
) -> dict[str, Any]:
    realized_gross = realized_px_pnl + realized_fnd
    mtm_gross = realized_gross + res_mtm_px_pnl + res_fnd
    return {
        "trades": trades,
        "long_trades": long_trades,
        "pv_trades": pv_trades,
        "pv_unverifiable": pv_unverifiable,
        "realized_price_pnl_usd": round(realized_px_pnl, 6),
        "realized_funding_usd": round(realized_fnd, 6),
        "realized_only_gross_usd": round(realized_gross, 6),
        "residual_mtm_price_pnl_usd": round(res_mtm_px_pnl, 6),
        "residual_funding_usd": round(res_fnd, 6),
        "mtm_incl_residual_gross_usd": round(mtm_gross, 6),
        "maker_vol_usd": round(maker_vol, 4),
        "taker_vol_usd": round(taker_vol, 4),
        "total_vol_usd": round(maker_vol + taker_vol, 4),
        "total_entry_notl_usd": round(total_entry_notl, 4),
        "avg_hold_ms": round(sum(hold_ms_list) / len(hold_ms_list), 1) if hold_ms_list else 0.0,
        "depth_clipped_entries": depth_clipped_entries,
        "depth_clipped_exits": depth_clipped_exits,
        "residual_trades": residual_trades,
        "terminal_flat_all": terminal_residual_notl <= 1e-6,
        "terminal_residual_notional_usd": round(terminal_residual_notl, 4),
        "exits_by_reason": exits_by_reason,
    }


def simulate_oracle_momentum(
    sig: PrecomputedDirectionalSeries,
    *,
    min_dislocation_bps: float = 15.0,
    spread_mult: float = MOM_SPREAD_MULT,
    max_dislocation_bps: float = MOM_MAX_DISLOC_BPS,
    max_entry_spread_bps: float = MOM_MAX_ENTRY_SPREAD_BPS,
    require_funding_alignment: bool = True,
    max_adverse_funding_bps: float = MOM_MAX_ADVERSE_FUNDING_BPS,
    take_profit_bps: float = MOM_TAKE_PROFIT_BPS,
    stop_loss_bps: float = MOM_STOP_LOSS_BPS,
    max_hold_ms: int = 900_000,
    cooldown_ms: int = MOM_COOLDOWN_MS,
    exec_mode: str = "taker",
    clip_usd: float = CLIP_USD_DEFAULT,
    conserve_exit_depth: bool = True,
) -> dict[str, Any]:
    """Simulate `OracleDiscoveryMomentum` on a precomputed symbol-window timeline."""
    ts_ms = sig.ts_ms
    n = len(ts_ms)
    if n < 10:
        return _empty_trade_result()

    t_stop_entries = sig.t1_ms - 10_000
    rdy = sig.mom_ready
    sig_arr = sig.mom_sig_bps
    spr_arr = sig.spread_bps
    fnd_arr = sig.fnd_bps
    bid_arr = sig.bid_px
    ask_arr = sig.ask_px

    trades = long_trades = pv_trades = pv_unverifiable = 0
    depth_clipped_entries = depth_clipped_exits = residual_trades = 0
    exits_by_reason: dict[str, int] = defaultdict(int)
    realized_px_pnl = realized_fnd = res_mtm_px_pnl = res_fnd = 0.0
    maker_vol = taker_vol = total_entry_notl = terminal_residual_notl = 0.0
    hold_ms_list: list[int] = []
    last_exit_ms = -1

    k = 0
    while k < n:
        t_dec = ts_ms[k]
        if t_dec >= t_stop_entries:
            break
        if last_exit_ms >= 0 and (t_dec - last_exit_ms) < cooldown_ms:
            k += 1
            continue
        if rdy[k] == 0:
            k += 1
            continue
        sval = sig_arr[k]
        abs_s = abs(sval)
        if abs_s < min_dislocation_bps or abs_s >= max_dislocation_bps:
            k += 1
            continue
        spr = spr_arr[k]
        if spr > max_entry_spread_bps:
            k += 1
            continue
        hurdle = max(min_dislocation_bps, spread_mult * spr)
        if abs_s < hurdle:
            k += 1
            continue

        side = 1 if sval >= hurdle else -1
        if require_funding_alignment:
            fnd = fnd_arr[k]
            if (side > 0 and fnd > max_adverse_funding_bps) or (
                side < 0 and fnd < -max_adverse_funding_bps
            ):
                k += 1
                continue

        ok_e, px_in, notl, t_fill_in, is_mk, d_clip, pv_miss = _execute_entry(
            sig, t_dec, t_stop_entries, side, exec_mode, clip_usd
        )
        if not ok_e:
            if pv_miss:
                pv_unverifiable += 1
                k = max(k + 1, bisect.bisect_left(ts_ms, t_fill_in))
            else:
                k += 1
            continue
        if is_mk:
            maker_vol += notl
            pv_trades += 1
        else:
            taker_vol += notl
        if d_clip:
            depth_clipped_entries += 1

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
            ex_px = bid_arr[j] if side > 0 else ask_arr[j]
            if ex_px > 0.0:
                unr_bps = (
                    ((ex_px / px_in) - 1.0) * BPS_SCALE
                    if side > 0
                    else ((px_in - ex_px) / px_in) * BPS_SCALE
                )
                if stop_loss_bps > 0.0 and unr_bps <= -stop_loss_bps:
                    j_exit = j
                    exit_reason = "stop_loss"
                    break
                if take_profit_bps > 0.0 and unr_bps >= take_profit_bps:
                    j_exit = j
                    exit_reason = "take_profit"
                    break
            j += 1

        (
            rpx,
            rfnd,
            rmtm,
            rfnd_res,
            mv,
            tv,
            last_fill_out,
            ex_clip,
            rem_res,
            j_cur,
        ) = _execute_exit(
            sig,
            j_exit,
            side,
            px_in,
            notl,
            t_fill_in,
            exit_reason,
            ("take_profit",),
            exec_mode,
            conserve_exit_depth=conserve_exit_depth,
        )
        realized_px_pnl += rpx
        realized_fnd += rfnd
        res_mtm_px_pnl += rmtm
        res_fnd += rfnd_res
        maker_vol += mv
        taker_vol += tv
        if ex_clip:
            depth_clipped_exits += 1
        if rem_res > 1e-6:
            residual_trades += 1
        terminal_residual_notl += rem_res

        total_entry_notl += notl
        trades += 1
        if side > 0:
            long_trades += 1
        exits_by_reason[exit_reason] += 1
        hold_ms_list.append(max(0, last_fill_out - t_fill_in))
        last_exit_ms = last_fill_out
        k = max(j_cur + 1, bisect.bisect_left(ts_ms, last_fill_out))

    return _finalize_trade_result(
        trades=trades,
        long_trades=long_trades,
        pv_trades=pv_trades if exec_mode == "maker_postonly" else trades,
        pv_unverifiable=pv_unverifiable,
        realized_px_pnl=realized_px_pnl,
        realized_fnd=realized_fnd,
        res_mtm_px_pnl=res_mtm_px_pnl,
        res_fnd=res_fnd,
        maker_vol=maker_vol,
        taker_vol=taker_vol,
        total_entry_notl=total_entry_notl,
        hold_ms_list=hold_ms_list,
        depth_clipped_entries=depth_clipped_entries,
        depth_clipped_exits=depth_clipped_exits,
        residual_trades=residual_trades,
        terminal_residual_notl=terminal_residual_notl,
        exits_by_reason=dict(exits_by_reason),
    )


def simulate_overreaction_reversion(
    sig: PrecomputedDirectionalSeries,
    *,
    lookback_bars: int = 60,
    min_return_bps: float = 30.0,
    spread_mult: float = REV_SPREAD_MULT,
    max_return_bps: float = REV_MAX_RETURN_BPS,
    max_entry_spread_bps: float = REV_MAX_ENTRY_SPREAD_BPS,
    require_funding_alignment: bool = True,
    max_adverse_funding_bps: float = REV_MAX_ADVERSE_FUNDING_BPS,
    reversion_target_fraction: float = 0.40,
    stop_loss_bps: float = REV_STOP_LOSS_BPS,
    cooldown_ms: int = REV_COOLDOWN_MS,
    exec_mode: str = "taker",
    clip_usd: float = CLIP_USD_DEFAULT,
    conserve_exit_depth: bool = True,
) -> dict[str, Any]:
    """Simulate `OverreactionReversion` on a precomputed symbol-window timeline."""
    ts_ms = sig.ts_ms
    n = len(ts_ms)
    if n < 10:
        return _empty_trade_result()

    max_hold_ms = lookback_bars * REV_BAR_MS
    t_stop_entries = sig.t1_ms - 10_000
    rdy = sig.rev_ready
    ret_arr = sig.rev_r60_bps if lookback_bars >= 60 else sig.rev_r15_bps
    spr_arr = sig.spread_bps
    fnd_arr = sig.fnd_bps
    bid_arr = sig.bid_px
    ask_arr = sig.ask_px

    trades = long_trades = pv_trades = pv_unverifiable = 0
    depth_clipped_entries = depth_clipped_exits = residual_trades = 0
    exits_by_reason: dict[str, int] = defaultdict(int)
    realized_px_pnl = realized_fnd = res_mtm_px_pnl = res_fnd = 0.0
    maker_vol = taker_vol = total_entry_notl = terminal_residual_notl = 0.0
    hold_ms_list: list[int] = []
    last_exit_ms = -1

    k = 0
    while k < n:
        t_dec = ts_ms[k]
        if t_dec >= t_stop_entries:
            break
        if last_exit_ms >= 0 and (t_dec - last_exit_ms) < cooldown_ms:
            k += 1
            continue
        if rdy[k] == 0:
            k += 1
            continue
        rval = ret_arr[k]
        if not math.isfinite(rval):
            k += 1
            continue
        abs_r = abs(rval)
        if abs_r < min_return_bps or abs_r >= max_return_bps:
            k += 1
            continue
        spr = spr_arr[k]
        if spr > max_entry_spread_bps:
            k += 1
            continue
        hurdle = max(min_return_bps, spread_mult * spr)
        if abs_r < hurdle:
            k += 1
            continue

        # REVERSION: fade extreme move
        side = 1 if rval <= -hurdle else -1
        if require_funding_alignment:
            fnd = fnd_arr[k]
            if (side > 0 and fnd > max_adverse_funding_bps) or (
                side < 0 and fnd < -max_adverse_funding_bps
            ):
                k += 1
                continue

        ok_e, px_in, notl, t_fill_in, is_mk, d_clip, pv_miss = _execute_entry(
            sig, t_dec, t_stop_entries, side, exec_mode, clip_usd
        )
        if not ok_e:
            if pv_miss:
                pv_unverifiable += 1
                k = max(k + 1, bisect.bisect_left(ts_ms, t_fill_in))
            else:
                k += 1
            continue
        if is_mk:
            maker_vol += notl
            pv_trades += 1
        else:
            taker_vol += notl
        if d_clip:
            depth_clipped_entries += 1

        rev_target_bps = reversion_target_fraction * abs_r
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
            ex_px = bid_arr[j] if side > 0 else ask_arr[j]
            if ex_px > 0.0:
                unr_bps = (
                    ((ex_px / px_in) - 1.0) * BPS_SCALE
                    if side > 0
                    else ((px_in - ex_px) / px_in) * BPS_SCALE
                )
                if stop_loss_bps > 0.0 and unr_bps <= -stop_loss_bps:
                    j_exit = j
                    exit_reason = "stop_loss"
                    break
                if reversion_target_fraction > 0.0 and unr_bps >= rev_target_bps:
                    j_exit = j
                    exit_reason = "mean_reversion"
                    break
            j += 1

        (
            rpx,
            rfnd,
            rmtm,
            rfnd_res,
            mv,
            tv,
            last_fill_out,
            ex_clip,
            rem_res,
            j_cur,
        ) = _execute_exit(
            sig,
            j_exit,
            side,
            px_in,
            notl,
            t_fill_in,
            exit_reason,
            ("mean_reversion",),
            exec_mode,
            conserve_exit_depth=conserve_exit_depth,
        )
        realized_px_pnl += rpx
        realized_fnd += rfnd
        res_mtm_px_pnl += rmtm
        res_fnd += rfnd_res
        maker_vol += mv
        taker_vol += tv
        if ex_clip:
            depth_clipped_exits += 1
        if rem_res > 1e-6:
            residual_trades += 1
        terminal_residual_notl += rem_res

        total_entry_notl += notl
        trades += 1
        if side > 0:
            long_trades += 1
        exits_by_reason[exit_reason] += 1
        hold_ms_list.append(max(0, last_fill_out - t_fill_in))
        last_exit_ms = last_fill_out
        k = max(j_cur + 1, bisect.bisect_left(ts_ms, last_fill_out))

    return _finalize_trade_result(
        trades=trades,
        long_trades=long_trades,
        pv_trades=pv_trades if exec_mode == "maker_postonly" else trades,
        pv_unverifiable=pv_unverifiable,
        realized_px_pnl=realized_px_pnl,
        realized_fnd=realized_fnd,
        res_mtm_px_pnl=res_mtm_px_pnl,
        res_fnd=res_fnd,
        maker_vol=maker_vol,
        taker_vol=taker_vol,
        total_entry_notl=total_entry_notl,
        hold_ms_list=hold_ms_list,
        depth_clipped_entries=depth_clipped_entries,
        depth_clipped_exits=depth_clipped_exits,
        residual_trades=residual_trades,
        terminal_residual_notl=terminal_residual_notl,
        exits_by_reason=dict(exits_by_reason),
    )


# ---------------------------------------------------------------------------
# Aggregation, Stress, & Portfolio Evaluation
# ---------------------------------------------------------------------------


def net_pnl_for_fee_bps(
    row: dict[str, Any],
    maker_fee_bps: float,
    taker_fee_bps: float,
    stress_bps: float = 0.0,
    *,
    realized_only: bool = False,
) -> float:
    gross = float(row["realized_only_gross_usd"] if realized_only else row["mtm_incl_residual_gross_usd"])
    fee = (
        float(row["maker_vol_usd"]) * (maker_fee_bps + stress_bps) * 1e-4
        + float(row["taker_vol_usd"]) * (taker_fee_bps + stress_bps) * 1e-4
    )
    return gross - fee


def aggregate_rows(rows: Sequence[dict[str, Any]], *, venue: str) -> dict[str, Any]:
    """Aggregate rows with realized vs MTM, Prem vs Std, +0.5/+1/+2 bps stress, Sharpe, PSR, and capacity."""
    prem_mk, prem_tk = (0.40, 2.80) if venue == "lighter" else (0.0, 0.0)
    trades = sum(int(r["trades"]) for r in rows)
    pv_trades = sum(int(r["pv_trades"]) for r in rows)
    pv_unver = sum(int(r["pv_unverifiable"]) for r in rows)
    realized_px = sum(float(r["realized_price_pnl_usd"]) for r in rows)
    realized_fnd = sum(float(r["realized_funding_usd"]) for r in rows)
    realized_gross = sum(float(r["realized_only_gross_usd"]) for r in rows)
    res_mtm_px = sum(float(r["residual_mtm_price_pnl_usd"]) for r in rows)
    res_fnd = sum(float(r["residual_funding_usd"]) for r in rows)
    mtm_gross = sum(float(r["mtm_incl_residual_gross_usd"]) for r in rows)

    mk_vol = sum(float(r["maker_vol_usd"]) for r in rows)
    tk_vol = sum(float(r["taker_vol_usd"]) for r in rows)
    tot_vol = mk_vol + tk_vol
    tot_ent_notl = sum(float(r["total_entry_notl_usd"]) for r in rows)
    res_notl = sum(float(r["terminal_residual_notional_usd"]) for r in rows)
    clipped_ent = sum(int(r["depth_clipped_entries"]) for r in rows)
    clipped_ex = sum(int(r["depth_clipped_exits"]) for r in rows)
    res_trades = sum(int(r.get("residual_trades", 0)) for r in rows)

    net_prem_realized = sum(net_pnl_for_fee_bps(r, prem_mk, prem_tk, 0.0, realized_only=True) for r in rows)
    net_prem_mtm = sum(net_pnl_for_fee_bps(r, prem_mk, prem_tk, 0.0, realized_only=False) for r in rows)
    net_stress_0_5 = sum(net_pnl_for_fee_bps(r, 0.0, 0.0, 0.5, realized_only=False) for r in rows)
    net_stress_1 = sum(net_pnl_for_fee_bps(r, 0.0, 0.0, 1.0, realized_only=False) for r in rows)
    net_stress_2 = sum(net_pnl_for_fee_bps(r, 0.0, 0.0, 2.0, realized_only=False) for r in rows)

    by_win: dict[str, float] = defaultdict(float)
    by_sym: dict[str, float] = defaultdict(float)
    for r in rows:
        by_win[str(r["win"])] += float(r["mtm_incl_residual_gross_usd"])
        by_sym[str(r["symbol"])] += float(r["mtm_incl_residual_gross_usd"])

    win_vals = [by_win[w] for w in sorted(by_win)]
    pos_wins = sum(1 for v in win_vals if v > 0.0)
    cum = peak = max_dd = 0.0
    for v in win_vals:
        cum += v
        peak = max(peak, cum)
        max_dd = max(max_dd, peak - cum)

    win_sr = 0.0
    psr_zero = 0.0
    if len(win_vals) >= 3:
        amto = _load_local_module("audit_multiple_testing_overfitting", "audit_multiple_testing_overfitting.py")
        mom = amto.series_moments(win_vals)
        win_sr = mom["sr"]
        psr_zero = amto.probabilistic_sharpe_ratio(mom, 0.0)

    top_sym = max(by_sym.items(), key=lambda kv: kv[1])[0] if by_sym else None
    ex_top1_usd = (mtm_gross - by_sym[top_sym]) if top_sym is not None else 0.0

    weighted_hold_num = sum(float(r["avg_hold_ms"]) * int(r["trades"]) for r in rows)
    avg_hold_ms = weighted_hold_num / trades if trades > 0 else 0.0

    return {
        "rows": len(rows),
        "unique_windows": len(win_vals),
        "trades": trades,
        "pv_trades": pv_trades,
        "pv_unverifiable": pv_unver,
        "realized_price_pnl_usd": round(realized_px, 4),
        "realized_funding_usd": round(realized_fnd, 4),
        "realized_only_net_0_0_usd": round(realized_gross, 4),
        "residual_mtm_price_pnl_usd": round(res_mtm_px, 4),
        "residual_funding_usd": round(res_fnd, 4),
        "mtm_incl_residual_net_0_0_usd": round(mtm_gross, 4),
        "net_prem_realized_usd": round(net_prem_realized, 4),
        "net_prem_mtm_usd": round(net_prem_mtm, 4),
        "net_0_0_plus_0_5bp_usd": round(net_stress_0_5, 4),
        "net_0_0_plus_1bp_usd": round(net_stress_1, 4),
        "net_0_0_plus_2bp_usd": round(net_stress_2, 4),
        "net_0_0_bps_per_vol": round((mtm_gross / tot_vol) * 1e4, 4) if tot_vol > 0.0 else 0.0,
        "win_sharpe_0_0": round(win_sr, 4),
        "annualized_sharpe_0_0": round(win_sr * math.sqrt(876.0), 4),
        "psr_zero_0_0": round(psr_zero, 4),
        "maker_vol_usd": round(mk_vol, 2),
        "taker_vol_usd": round(tk_vol, 2),
        "total_vol_usd": round(tot_vol, 2),
        "avg_entry_notl_usd": round(tot_ent_notl / trades, 2) if trades > 0 else 0.0,
        "avg_hold_ms": round(avg_hold_ms, 1),
        "positive_windows": pos_wins,
        "positive_win_rate": round(pos_wins / len(win_vals), 4) if win_vals else 0.0,
        "max_dd_0_0_usd": round(max_dd, 4),
        "top_contributor": top_sym,
        "top_contributor_usd": round(by_sym[top_sym], 4) if top_sym is not None else 0.0,
        "ex_top1_net_0_0_usd": round(ex_top1_usd, 4),
        "depth_clipped_entries": clipped_ent,
        "depth_clipped_exits": clipped_ex,
        "residual_trades": res_trades,
        "terminal_flat_all": res_notl <= 1e-6,
        "terminal_residual_notional_usd": round(res_notl, 4),
    }


def _evaluate_family_cohort(
    rows: Sequence[dict[str, Any]],
    *,
    venue: str,
    exec_mode: str,
    cohort: str,
    cfg_keys: Sequence[tuple[Any, ...]],
    cfg_label_fn: Any,
    row_match_fn: Any,
    dev_wins: Sequence[str],
    all_wins: Sequence[str],
) -> dict[str, Any]:
    sub = [
        r
        for r in rows
        if r["venue"] == venue
        and r["exec_mode"] == exec_mode
        and (
            cohort == "all_symbols"
            or (cohort == "crypto_only" and r["symbol"] in CRYPTO_SET)
            or (cohort == "rwa_only" and r["symbol"] not in CRYPTO_SET)
        )
    ]
    dev_sub = [r for r in sub if r["split"] == "selection_dev"]

    cfg_grid_dev: dict[tuple[Any, ...], dict[str, Any]] = {}
    trial_dev: dict[str, list[float]] = {}
    trial_all: dict[str, list[float]] = {}
    for cfg in cfg_keys:
        c_dev = [r for r in dev_sub if row_match_fn(r, cfg)]
        c_all = [r for r in sub if row_match_fn(r, cfg)]
        agg_dev = aggregate_rows(c_dev, venue=venue)
        cfg_grid_dev[cfg] = agg_dev
        lbl = cfg_label_fn(cfg)
        wd: dict[str, float] = defaultdict(float)
        for r in c_dev:
            wd[str(r["win"])] += float(r["mtm_incl_residual_gross_usd"])
        trial_dev[lbl] = [wd[w] for w in dev_wins]
        wa: dict[str, float] = defaultdict(float)
        for r in c_all:
            wa[str(r["win"])] += float(r["mtm_incl_residual_gross_usd"])
        trial_all[lbl] = [wa[w] for w in all_wins]

    elig = [(k, v) for k, v in cfg_grid_dev.items() if v["trades"] >= 20]
    if not elig:
        elig = list(cfg_grid_dev.items())
    best_cfg = max(
        elig,
        key=lambda kv: (kv[1]["realized_only_net_0_0_usd"], kv[1]["win_sharpe_0_0"], kv[1]["trades"]),
    )[0]

    # Per-symbol positive-dev freeze strictly on selection_dev
    sym_league_dev: dict[str, Any] = {}
    frozen_pos_syms: dict[str, tuple[Any, ...]] = {}
    for sym in sorted({str(r["symbol"]) for r in dev_sub}):
        s_dev = [r for r in dev_sub if r["symbol"] == sym]
        best_sc = None
        best_sa = None
        for cfg in cfg_keys:
            sc_rows = [r for r in s_dev if row_match_fn(r, cfg)]
            if not sc_rows:
                continue
            agg = aggregate_rows(sc_rows, venue=venue)
            if best_sa is None or (agg["realized_only_net_0_0_usd"], agg["trades"]) > (
                best_sa["realized_only_net_0_0_usd"],
                best_sa["trades"],
            ):
                best_sa = agg
                best_sc = cfg
        if best_sc is not None and best_sa is not None:
            sym_league_dev[sym] = {"best_cfg": cfg_label_fn(best_sc), **best_sa}
            if best_sa["realized_only_net_0_0_usd"] > 0.0 and best_sa["trades"] >= 5:
                frozen_pos_syms[sym] = best_sc

    # Positive-dev symbols under the frozen best global cfg
    frozen_global_pos_syms = sorted(
        sym
        for sym in sorted({str(r["symbol"]) for r in dev_sub})
        if aggregate_rows([r for r in dev_sub if r["symbol"] == sym and row_match_fn(r, best_cfg)], venue=venue)[
            "realized_only_net_0_0_usd"
        ]
        > 0.0
    )

    splits_eval: dict[str, Any] = {}
    for sp in ("selection_dev", "holdout", "w26_w27_oos"):
        sp_global = [r for r in sub if r["split"] == sp and row_match_fn(r, best_cfg)]
        sp_pos_global = [r for r in sp_global if r["symbol"] in frozen_global_pos_syms]
        sp_pos_persym = [
            r
            for r in sub
            if r["split"] == sp
            and r["symbol"] in frozen_pos_syms
            and row_match_fn(r, frozen_pos_syms[str(r["symbol"])])
        ]
        splits_eval[sp] = {
            "all_symbols_best_global": aggregate_rows(sp_global, venue=venue),
            "frozen_global_pos_dev_symbols": aggregate_rows(sp_pos_global, venue=venue),
            "frozen_persym_pos_dev_symbols": aggregate_rows(sp_pos_persym, venue=venue),
        }

    # Per-window breakdown for best_global across all splits
    per_window_best: dict[str, Any] = {}
    for w in all_wins:
        wr = [r for r in sub if r["win"] == w and row_match_fn(r, best_cfg)]
        if wr:
            per_window_best[w] = {
                "split": wr[0]["split"],
                "trades": sum(int(x["trades"]) for x in wr),
                "realized_net_0_0_usd": round(sum(float(x["realized_only_gross_usd"]) for x in wr), 4),
                "mtm_net_0_0_usd": round(sum(float(x["mtm_incl_residual_gross_usd"]) for x in wr), 4),
                "net_prem_mtm_usd": round(sum(net_pnl_for_fee_bps(x, 0.40 if venue == "lighter" else 0.0, 2.80 if venue == "lighter" else 0.0) for x in wr), 4),
            }

    return {
        "best_global_cfg": cfg_label_fn(best_cfg),
        "frozen_global_pos_dev_symbols": frozen_global_pos_syms,
        "frozen_persym_pos_dev_symbols": {s: cfg_label_fn(c) for s, c in sorted(frozen_pos_syms.items())},
        "selection_dev_grid": {cfg_label_fn(k): v for k, v in sorted(cfg_grid_dev.items())},
        "selection_dev_symbol_league": sym_league_dev,
        "splits": splits_eval,
        "per_window_best_global": per_window_best,
        "multiple_testing_dev": compute_multiple_testing_metrics(trial_dev),
        "multiple_testing_w01_w27": compute_multiple_testing_metrics(trial_all),
    }


# ---------------------------------------------------------------------------
# Full W01-W27 Pipeline
# ---------------------------------------------------------------------------


def run_research(
    lakehouse_dir: Path,
    *,
    fee_evidence_path: Path | None = None,
    output_path: Path | None = None,
    clip_usd: float = CLIP_USD_DEFAULT,
    capacity_clip_usd: float = 1_000.0,
) -> dict[str, Any]:
    windows = discover_windows_w01_w27(lakehouse_dir)
    if not windows:
        raise RuntimeError(f"No w01..w27 partitions discovered under {lakehouse_dir}")

    fee_meta = resolve_all_fee_models(fee_evidence_path)
    mom_rows: list[dict[str, Any]] = []
    rev_rows: list[dict[str, Any]] = []
    cap_rows: list[dict[str, Any]] = []
    market_coverage: dict[str, dict[str, int]] = {"lighter_robinhood": defaultdict(int), "lighter": defaultdict(int)}

    mom_grid = [
        (md, mh, fa)
        for md in MOM_MIN_DISLOC_GRID
        for mh in MOM_MAX_HOLD_MS_GRID
        for fa in MOM_FUNDING_ALIGN_GRID
    ]
    rev_grid = [
        (lb, mr, rf, fa)
        for lb in REV_LOOKBACK_BARS_GRID
        for mr in REV_MIN_RETURN_GRID
        for rf in REV_TARGET_FRAC_GRID
        for fa in REV_FUNDING_ALIGN_GRID
    ]

    for wid in sorted(windows.keys()):
        winfo = windows[wid]
        split_name = winfo["split"]
        campaigns = winfo["campaigns"]
        for venue in ("lighter_robinhood", "lighter"):
            bundle = load_window_venue_bundle(lakehouse_dir, venue, campaigns)
            bbo = bundle["bbo"]
            bbo_ts = bundle["bbo_ts"]
            cont = bundle["cont"]
            trades_map = bundle["trades"]
            ctx = bundle["ctx"]
            ctx_ts = bundle["ctx_ts"]

            for sym, cinfo in cont.items():
                if cinfo.get("is_continuous_10h"):
                    market_coverage[venue][sym] += 1

            for sym in sorted(VENUE_MARKETS[venue].keys()):
                if sym not in bbo or sym not in ctx:
                    continue
                if not cont[sym]["is_continuous_10h"]:
                    continue
                sig = precompute_directional_series(
                    bbo[sym], bbo_ts[sym], ctx[sym], ctx_ts[sym], tr=trades_map.get(sym)
                )
                if sig is None:
                    continue

                # Family 1: OracleDiscoveryMomentum
                for md, mh, fa in mom_grid:
                    for exec_mode in ("taker", "maker_postonly"):
                        res_m = simulate_oracle_momentum(
                            sig,
                            min_dislocation_bps=md,
                            max_hold_ms=mh,
                            require_funding_alignment=fa,
                            exec_mode=exec_mode,
                            clip_usd=clip_usd,
                        )
                        mom_rows.append(
                            {
                                "venue": venue,
                                "split": split_name,
                                "win": wid,
                                "symbol": sym,
                                "min_disloc_bps": md,
                                "max_hold_ms": mh,
                                "funding_align": fa,
                                "exec_mode": exec_mode,
                                **res_m,
                            }
                        )

                # Family 2: OverreactionReversion
                for lb, mr, rf, fa in rev_grid:
                    for exec_mode in ("taker", "maker_postonly"):
                        res_r = simulate_overreaction_reversion(
                            sig,
                            lookback_bars=lb,
                            min_return_bps=mr,
                            reversion_target_fraction=rf,
                            require_funding_alignment=fa,
                            exec_mode=exec_mode,
                            clip_usd=clip_usd,
                        )
                        rev_rows.append(
                            {
                                "venue": venue,
                                "split": split_name,
                                "win": wid,
                                "symbol": sym,
                                "lookback_bars": lb,
                                "min_return_bps": mr,
                                "reversion_frac": rf,
                                "funding_align": fa,
                                "exec_mode": exec_mode,
                                **res_r,
                            }
                        )

                # Capacity check at $1,000 clip on canonical representative configs
                for fam, res_cap in (
                    (
                        "mom_d15_h15m_f1",
                        simulate_oracle_momentum(
                            sig,
                            min_dislocation_bps=15.0,
                            max_hold_ms=900_000,
                            require_funding_alignment=True,
                            exec_mode="taker",
                            clip_usd=capacity_clip_usd,
                        ),
                    ),
                    (
                        "rev_60m_r30_rf40_f1",
                        simulate_overreaction_reversion(
                            sig,
                            lookback_bars=60,
                            min_return_bps=30.0,
                            reversion_target_fraction=0.40,
                            require_funding_alignment=True,
                            exec_mode="taker",
                            clip_usd=capacity_clip_usd,
                        ),
                    ),
                ):
                    cap_rows.append(
                        {
                            "venue": venue,
                            "split": split_name,
                            "win": wid,
                            "symbol": sym,
                            "arm": fam,
                            **res_cap,
                        }
                    )

            del bundle
            gc.collect()

    dev_wins = sorted([w for w, i in windows.items() if i["split"] == "selection_dev"])
    all_wins = sorted(windows.keys())

    mom_out: dict[str, Any] = {}
    rev_out: dict[str, Any] = {}
    cap_out: dict[str, Any] = {}

    for venue in ("lighter_robinhood", "lighter"):
        mom_out[venue] = {}
        rev_out[venue] = {}
        for exec_mode in ("taker", "maker_postonly"):
            mom_out[venue][exec_mode] = {
                cohort: _evaluate_family_cohort(
                    mom_rows,
                    venue=venue,
                    exec_mode=exec_mode,
                    cohort=cohort,
                    cfg_keys=mom_grid,
                    cfg_label_fn=lambda k: f"d{int(k[0])}_h{k[1] // 60000}m_f{int(k[2])}",
                    row_match_fn=lambda r, k: (
                        r["min_disloc_bps"] == k[0]
                        and r["max_hold_ms"] == k[1]
                        and r["funding_align"] == k[2]
                    ),
                    dev_wins=dev_wins,
                    all_wins=all_wins,
                )
                for cohort in ("rwa_only", "crypto_only", "all_symbols")
            }
            rev_out[venue][exec_mode] = {
                cohort: _evaluate_family_cohort(
                    rev_rows,
                    venue=venue,
                    exec_mode=exec_mode,
                    cohort=cohort,
                    cfg_keys=rev_grid,
                    cfg_label_fn=lambda k: f"lb{k[0]}m_r{int(k[1])}_rf{int(round(k[2] * 100))}_f{int(k[3])}",
                    row_match_fn=lambda r, k: (
                        r["lookback_bars"] == k[0]
                        and r["min_return_bps"] == k[1]
                        and r["reversion_frac"] == k[2]
                        and r["funding_align"] == k[3]
                    ),
                    dev_wins=dev_wins,
                    all_wins=all_wins,
                )
                for cohort in ("crypto_only", "rwa_only", "all_symbols")
            }

        cap_out[venue] = {}
        for arm in ("mom_d15_h15m_f1", "rev_60m_r30_rf40_f1"):
            cap_out[venue][arm] = {
                sp: aggregate_rows(
                    [r for r in cap_rows if r["venue"] == venue and r["arm"] == arm and r["split"] == sp],
                    venue=venue,
                )
                for sp in ("selection_dev", "holdout", "w26_w27_oos")
            }

    out = {
        "schema": "track01_directional_alphas_w01_w27_v1",
        "protocol": {
            "selection_dev": [w for w, i in sorted(windows.items()) if i["split"] == "selection_dev"],
            "holdout": [w for w, i in sorted(windows.items()) if i["split"] == "holdout"],
            "w26_w27_oos": [w for w, i in sorted(windows.items()) if i["split"] == "w26_w27_oos"],
            "excluded_windows": ["w08", "w28+"],
            "clip_usd": clip_usd,
            "capacity_clip_usd": capacity_clip_usd,
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
        "family_1_oracle_discovery_momentum": mom_out,
        "family_2_overreaction_reversion": rev_out,
        "capacity_1000_usd_stress": cap_out,
    }

    if output_path is not None:
        output_path.parent.mkdir(parents=True, exist_ok=True)
        output_path.write_text(json.dumps(out, indent=2, allow_nan=False) + "\n", encoding="utf-8")
    return out


# ---------------------------------------------------------------------------
# Pure-Stdlib Self-Test
# ---------------------------------------------------------------------------


def run_self_test() -> dict[str, Any]:
    """Deterministic synthetic verification of both engines, PV fills, funding, and PSR/DSR/PBO."""
    t0 = 1_000_000

    # 1. OracleDiscoveryMomentum synthetic: mark leads oracle upward (+20 bps), perp rallies +20 bps
    bbo_m: list[tuple[int, float, float, float, float]] = []
    ctx_m: list[tuple[int, float, float, float]] = []
    tr_t: list[int] = []
    tr_s: list[int] = []
    tr_p: list[float] = []
    tr_sz: list[float] = []
    for i in range(600):
        t = t0 + i * 1_000
        if i < 120:
            px = 100.0
            ora = 100.0
        elif i < 200:
            # Mark leads oracle up by +20 bps -> entry at i=120, continues up +25 bps by i=180
            ramp = (i - 120) * 0.0005
            px = 100.20 + ramp
            ora = 100.00
        else:
            px = 100.25
            ora = 100.25
        bbo_m.append((t, px - 0.01, 20.0, px + 0.01, 20.0))
        ctx_m.append((t, px, ora, -0.5))
        tr_t.append(t + 300)
        tr_s.append(-1 if i < 160 else 1)
        tr_p.append((px - 0.01) if i < 160 else (px + 0.01))
        tr_sz.append(10.0)

    sig_m = precompute_directional_series(
        bbo_m, [b[0] for b in bbo_m], ctx_m, [c[0] for c in ctx_m], tr=(tr_t, tr_s, tr_p, tr_sz)
    )
    assert sig_m is not None
    res_m = simulate_oracle_momentum(
        sig_m, min_dislocation_bps=15.0, take_profit_bps=12.0, exec_mode="taker", clip_usd=500.0
    )
    assert res_m["trades"] >= 1, f"expected >=1 momentum trade, got {res_m}"
    assert res_m["realized_only_gross_usd"] > 0.0, f"expected positive momentum pnl, got {res_m}"
    assert res_m["realized_funding_usd"] > 0.0, f"expected positive funding on long with -0.5bp/hr: {res_m}"
    assert res_m["terminal_flat_all"] is True

    # 2. OverreactionReversion synthetic: 90 minutes, flat 65m then crash -50 bps over 10m then bounce +30 bps
    bbo_r: list[tuple[int, float, float, float, float]] = []
    ctx_r: list[tuple[int, float, float, float]] = []
    for i in range(5_400):
        t = t0 + i * 1_000
        m = i // 60
        if m < 65:
            px = 100.0
        elif m < 70:
            px = 99.50
        else:
            px = 99.85
        bbo_r.append((t, px - 0.01, 20.0, px + 0.01, 20.0))
        ctx_r.append((t, px, px, 0.0))

    sig_r = precompute_directional_series(bbo_r, [b[0] for b in bbo_r], ctx_r, [c[0] for c in ctx_r])
    assert sig_r is not None
    res_r = simulate_overreaction_reversion(
        sig_r,
        lookback_bars=60,
        min_return_bps=30.0,
        reversion_target_fraction=0.40,
        exec_mode="taker",
        clip_usd=500.0,
    )
    assert res_r["trades"] >= 1, f"expected >=1 overreaction trade, got {res_r}"
    assert res_r["realized_only_gross_usd"] > 0.0, f"expected positive reversion pnl, got {res_r}"
    assert res_r["terminal_flat_all"] is True

    # 3. Multiple testing smoke test
    mt = compute_multiple_testing_metrics(
        {
            "arm1": [5.0 + (i % 3) for i in range(20)],
            "arm2": [-1.0 + (i % 2) for i in range(20)],
        },
        num_blocks=8,
    )
    assert mt["status"] == "ok" and "dsr" in mt and "cscv" in mt

    return {
        "self_test": "ok",
        "mom_trades": res_m["trades"],
        "mom_realized_gross_usd": res_m["realized_only_gross_usd"],
        "rev_trades": res_r["trades"],
        "rev_realized_gross_usd": res_r["realized_only_gross_usd"],
        "dsr_effective": mt["dsr"]["dsr_effective"],
        "pbo": mt["cscv"]["pbo"],
    }


def main(argv: Sequence[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--lakehouse", type=Path, default=Path(os.path.expanduser("~/aleph-tx-lakehouse")))
    ap.add_argument("--fee-evidence", type=Path, default=None)
    ap.add_argument(
        "--output",
        type=Path,
        default=Path("/tmp/profitability_upgrade_20261127/t1/track01_directional_alphas_w01_w27.json"),
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
                "w26_w27_oos_windows": len(res["protocol"]["w26_w27_oos"]),
            },
            indent=2,
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
