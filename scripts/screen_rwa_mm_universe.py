#!/usr/bin/env python3
"""Offline RWA market-making universe & UTC-session microstructure screener (pure stdlib).

Screens `(venue, symbol)` pairs across either:
  1. Aleph Hive parquet lakehouse (`--lakehouse <dir>`), lazily importing `duckdb` inside
     the loader function so CI environments without DuckDB/NumPy/Pandas remain supported.
  2. Pre-computed `multivenue_three_tracks_*.json` summaries (`--multivenue <paths...>`).

Classifies timestamps into canonical UTC equity/crypto trading sessions (`us_rth`,
`us_extended`, `overnight_asia`) and applies frozen admission gates to emit an
`rwa_mm_whitelist_v1` JSON artifact compatible with downstream replay tools.
"""

from __future__ import annotations

import argparse
from collections import defaultdict
from dataclasses import asdict, dataclass
from datetime import date, datetime, timedelta, timezone
import glob
import json
import math
import os
from pathlib import Path
from statistics import median
import sys
from typing import Any, Sequence

TEN_HOURS_MS = 36_000_000
MAX_CONTINUOUS_GAP_MS = 60_000
SUBSESSION_ACTIVE_GAP_MS = 600_000  # 10m; separates distinct session segments across days/blocks

UTC_SESSIONS: tuple[str, ...] = ("all", "us_rth", "us_extended", "overnight_asia")
TRADING_SESSIONS: tuple[str, ...] = ("us_rth", "us_extended", "overnight_asia")
SESSION_LABELS: dict[str, str] = {
    "all": "00:00-24:00_utc",
    "us_rth": "13:30-20:00_utc",
    "us_extended": "11:00-13:30_and_20:00-24:00_utc",
    "overnight_asia": "00:00-11:00_utc",
}

DEFAULT_WINDOWS: tuple[str, ...] = ("w18", "w19", "w20", "w21")
DEFAULT_VENUES: tuple[str, ...] = ("arcus", "lighter", "lighter_robinhood")
DEFAULT_SYMBOLS: tuple[str, ...] = (
    "DRAM",
    "SNDK",
    "NVDA",
    "QQQ",
    "SPY",
    "AAPL",
    "TSLA",
    "AMZN",
    "MU",
)

CRYPTO_SYMBOLS: frozenset[str] = frozenset({"BTC", "ETH", "SOL"})

# Exact verified Aleph campaign map across W18-W25.
CANONICAL_CAMPAIGNS: dict[str, dict[str, dict[str, str]]] = {
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
    "w22": {
        "crypto": {"campaign_id": "tokyo-crypto-20261124T130005Z-w22", "utc_date": "2026-11-24"},
        "rwa": {"campaign_id": "tokyo-rwa-20261124T125953Z-w22", "utc_date": "2026-11-24"},
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
}

# Canonical symbol mapping matching scripts/eval_multivenue_three_tracks.py.
VENUE_SYMBOL_MAP: dict[str, dict[str, str]] = {
    "arcus": {
        "BTC": "BTC-USD",
        "ETH": "ETH-USD",
        "SOL": "SOL-USD",
        "NVDA": "NVDA-USD",
        "SNDK": "SNDK-USD",
        "SPY": "SPY-USD",
        "QQQ": "QQQ-USD",
        "AAPL": "AAPL-USD",
        "TSLA": "TSLA-USD",
        "AMD": "AMD-USD",
        "MU": "MU-USD",
        "AMZN": "AMZN-USD",
        "GOOGL": "GOOGL-USD",
        "META": "META-USD",
        "MSFT": "MSFT-USD",
        "COIN": "COIN-USD",
        "CRCL": "CRCL-USD",
        "MSTR": "MSTR-USD",
        "PLTR": "PLTR-USD",
        "DRAM": "DRAM-USD",
        "HOOD": "HOOD-USD",
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
        "MU": "164",
    },
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
        "PLTR": "34",
    },
    "trade_xyz": {
        "NVDA": "xyz:NVDA",
        "SNDK": "xyz:SNDK",
        "AAPL": "xyz:AAPL",
        "TSLA": "xyz:TSLA",
        "AMD": "xyz:AMD",
        "MU": "xyz:MU",
        "AMZN": "xyz:AMZN",
        "GOOGL": "xyz:GOOGL",
        "META": "xyz:META",
        "MSFT": "xyz:MSFT",
        "COIN": "xyz:COIN",
        "CRCL": "xyz:CRCL",
        "MSTR": "xyz:MSTR",
        "PLTR": "xyz:PLTR",
        "DRAM": "xyz:DRAM",
        "HOOD": "xyz:HOOD",
    },
    "entropy": {
        "NVDA": "io:NVDA",
        "SNDK": "io:SNDK",
        "AAPL": "io:AAPL",
        "CRCL": "io:CRCL",
        "DRAM": "io:DRAM",
    },
    "bulk": {
        "BTC": "BTC-USD",
        "ETH": "ETH-USD",
        "SOL": "SOL-USD",
        "NVDA": "NVDA-USD",
        "SNDK": "SNDK-USD",
        "QQQ": "QQQ-USD",
        "TSLA": "TSLA-USD",
        "MU": "MU-USD",
        "MSTR": "MSTR-USD",
        "CRCL": "CRCL-USD",
    },
    "extended": {
        "BTC": "BTC-USD",
        "ETH": "ETH-USD",
        "SOL": "SOL-USD",
    },
    "hyperliquid": {
        "BTC": "BTC",
        "ETH": "ETH",
        "SOL": "SOL",
    },
}

VENUE_PROFILES: dict[str, dict[str, Any]] = {
    "arcus": {
        "maker_fee_bps_crypto": 0.0,
        "maker_fee_bps_equity": 0.0,
        "taker_fee_bps_crypto": 2.30,
        "taker_fee_bps_equity": 1.15,
        "settlement": "USDC",
        "fee_audit_status": "public_tier0_schedule_only",
        "bbo_cadence": "ws_event",
        "has_trades": True,
    },
    "lighter_robinhood": {
        "maker_fee_bps_crypto": 0.0,
        "maker_fee_bps_equity": 0.0,
        "taker_fee_bps_crypto": 0.0,
        "taker_fee_bps_equity": 0.0,
        "settlement": "USDG",
        "fee_audit_status": "audited_standard_zero",
        "bbo_cadence": "ws_event",
        "has_trades": True,
    },
    "lighter": {
        "maker_fee_bps_crypto": 0.0,
        "maker_fee_bps_equity": 0.0,
        "taker_fee_bps_crypto": 0.0,
        "taker_fee_bps_equity": 0.0,
        "settlement": "USDC",
        "fee_audit_status": "audited_standard_zero",
        "bbo_cadence": "ws_event",
        "has_trades": True,
    },
    "extended": {
        "maker_fee_bps_crypto": 0.0,
        "maker_fee_bps_equity": 0.0,
        "taker_fee_bps_crypto": 2.50,
        "taker_fee_bps_equity": 2.50,
        "settlement": "USDC",
        "fee_audit_status": "account_evidence_required",
        "bbo_cadence": "ws_event",
        "has_trades": True,
    },
    "hyperliquid": {
        "maker_fee_bps_crypto": 1.50,
        "maker_fee_bps_equity": 1.50,
        "taker_fee_bps_crypto": 4.50,
        "taker_fee_bps_equity": 4.50,
        "settlement": "USDC",
        "fee_audit_status": "audited",
        "bbo_cadence": "polled_15s_diagnostic",
        "has_trades": True,
    },
    "trade_xyz": {
        "maker_fee_bps_crypto": 0.30,
        "maker_fee_bps_equity": 0.30,
        "taker_fee_bps_crypto": 0.90,
        "taker_fee_bps_equity": 0.90,
        "settlement": "USDC",
        "fee_audit_status": "account_evidence_required",
        "bbo_cadence": "polled_15s_diagnostic",
        "has_trades": True,
    },
    "entropy": {
        "maker_fee_bps_crypto": 0.30,
        "maker_fee_bps_equity": 0.30,
        "taker_fee_bps_crypto": 0.90,
        "taker_fee_bps_equity": 0.90,
        "settlement": "USDC",
        "fee_audit_status": "account_evidence_required",
        "bbo_cadence": "polled_15s_diagnostic",
        "has_trades": True,
    },
    "bulk": {
        "maker_fee_bps_crypto": 0.0,
        "maker_fee_bps_equity": 0.0,
        "taker_fee_bps_crypto": 2.00,
        "taker_fee_bps_equity": 2.00,
        "settlement": "USDC",
        "fee_audit_status": "account_evidence_required",
        "bbo_cadence": "ws_event",
        "has_trades": False,
    },
}


@dataclass(frozen=True)
class WhitelistGates:
    """Frozen admission thresholds for RWA market-making universe screening."""

    min_half_spread_bps: float = 0.25
    min_spread_to_vol: float = 0.45
    min_l1_usd: float = 300.0
    max_vol_bps: float = 2.5
    min_trades_per_hour: float = 50.0
    require_zero_maker_fee: bool = True


def _require_finite_number(val: Any, name: str) -> float:
    if isinstance(val, bool) or val is None:
        raise ValueError(f"{name} must be a finite number, got {val!r}")
    try:
        out = float(val)
    except (TypeError, ValueError) as exc:
        raise ValueError(f"{name} must be numeric, got {val!r}") from exc
    if not math.isfinite(out):
        raise ValueError(f"{name} must be finite, got {val!r}")
    return out


def _opt_finite(val: Any) -> float | None:
    if isinstance(val, bool) or val is None:
        return None
    try:
        out = float(val)
    except (TypeError, ValueError):
        return None
    return out if math.isfinite(out) else None


def classify_utc_session(ts_ms: int | float) -> str:
    """Classify epoch `ts_ms` into canonical UTC trading session (`us_rth`, `us_extended`, `overnight_asia`).

    - `us_rth`:         `13:30 <= UTC < 20:00` (canonical US regular session, `13:30-20:00_utc`)
    - `us_extended`:    `11:00 <= UTC < 13:30` or `20:00 <= UTC < 24:00`
    - `overnight_asia`: `00:00 <= UTC < 11:00`
    """
    ts = _require_finite_number(ts_ms, "ts_ms")
    dt = datetime.fromtimestamp(ts / 1000.0, tz=timezone.utc)
    minute_of_day = dt.hour * 60 + dt.minute
    if minute_of_day < 11 * 60:
        return "overnight_asia"
    if minute_of_day < 13 * 60 + 30 or minute_of_day >= 20 * 60:
        return "us_extended"
    return "us_rth"


def _compute_continuity_stats(
    timestamps_ms: Sequence[int],
    *,
    is_subsession: bool = False,
) -> dict[str, Any]:
    if len(timestamps_ms) < 2:
        return {
            "duration_ms": 0,
            "hours": 0.0,
            "max_gap_ms": 0,
            "is_continuous_10h": False,
        }
    dur = int(timestamps_ms[-1] - timestamps_ms[0])
    gaps = [int(timestamps_ms[i] - timestamps_ms[i - 1]) for i in range(1, len(timestamps_ms))]
    max_gap = max(gaps)
    if is_subsession:
        # Avoid inflating session duration across 6.5h/17.5h off-session gaps when computing trades/hr.
        active_ms = sum(g for g in gaps if 0 < g <= SUBSESSION_ACTIVE_GAP_MS)
        hours = active_ms / 3_600_000.0
    else:
        hours = dur / 3_600_000.0
    return {
        "duration_ms": dur,
        "hours": round(hours, 3),
        "max_gap_ms": max_gap,
        "is_continuous_10h": dur >= TEN_HOURS_MS and max_gap <= MAX_CONTINUOUS_GAP_MS,
    }


def compute_slice_microstructure_metrics(
    books: Sequence[tuple[int, float, float, float, float]],
    *,
    trade_timestamps_ms: Sequence[int] | None = None,
    maker_fee_bps: float = 0.0,
    taker_fee_bps: float = 0.0,
    is_subsession: bool = False,
) -> dict[str, Any]:
    """Compute microstructure & trading activity metrics for one chronological BBO slice."""
    maker_fee = _require_finite_number(maker_fee_bps, "maker_fee_bps")
    taker_fee = _require_finite_number(taker_fee_bps, "taker_fee_bps")

    valid_books: list[tuple[int, float, float, float, float]] = []
    for row in books:
        ts_raw, bid, bid_sz, ask, ask_sz = row
        ts_ms = int(ts_raw)
        if (
            math.isfinite(bid)
            and math.isfinite(ask)
            and math.isfinite(bid_sz)
            and math.isfinite(ask_sz)
            and bid > 0.0
            and ask > bid
        ):
            valid_books.append((ts_ms, float(bid), float(bid_sz), float(ask), float(ask_sz)))

    ts_list = [b[0] for b in valid_books]
    cont = _compute_continuity_stats(ts_list, is_subsession=is_subsession)
    trades_count = len(trade_timestamps_ms) if trade_timestamps_ms is not None else None
    trades_per_hour: float | None = None
    if trades_count is not None and cont["hours"] > 0.0:
        trades_per_hour = round(trades_count / cont["hours"], 3)

    if len(valid_books) < 20:
        return {
            "samples": len(valid_books),
            "insufficient_samples": True,
            **cont,
            "maker_fee_bps": round(maker_fee, 6),
            "taker_fee_bps": round(taker_fee, 6),
            "median_spread_bps": None,
            "half_spread_minus_fee_bps": None,
            "realized_1s_vol_bps": None,
            "median_1s_abs_move_bps": None,
            "spread_to_vol_ratio": None,
            "half_spread_to_1s_move_ratio": None,
            "median_l1_min_usd": None,
            "trades_count": trades_count,
            "trades_per_hour": trades_per_hour,
        }

    mids = [0.5 * (b[1] + b[3]) for b in valid_books]
    spreads = [(b[3] - b[1]) / mid * 1e4 for b, mid in zip(valid_books, mids)]
    l1_min_usd = [min(b[1] * max(b[2], 0.0), b[3] * max(b[4], 0.0)) for b in valid_books]

    # Causal 1s-normalized log-returns matching eval_multivenue_three_tracks.py:290-309.
    ret_1s_bps: list[float] = []
    last_bucket_ts = ts_list[0]
    last_bucket_mid = mids[0]
    for ts, mid in zip(ts_list[1:], mids[1:]):
        dt = ts - last_bucket_ts
        if 1_000 <= dt <= 10_000 and last_bucket_mid > 0.0:
            r_bps = math.log(mid / last_bucket_mid) * 1e4 / math.sqrt(dt / 1000.0)
            if math.isfinite(r_bps):
                ret_1s_bps.append(r_bps)
            last_bucket_ts = ts
            last_bucket_mid = mid
        elif dt > 10_000:
            last_bucket_ts = ts
            last_bucket_mid = mid

    vol_1s = 0.0
    if len(ret_1s_bps) >= 5:
        mean_r = sum(ret_1s_bps) / len(ret_1s_bps)
        vol_1s = math.sqrt(sum((x - mean_r) ** 2 for x in ret_1s_bps) / (len(ret_1s_bps) - 1))

    med_abs_move_1s: float | None = None
    if ret_1s_bps:
        med_abs_move_1s = float(median(abs(x) for x in ret_1s_bps))

    med_sp = float(median(spreads))
    med_l1 = float(median(l1_min_usd))
    half_spread_net = 0.5 * med_sp - maker_fee

    spread_to_vol = round(med_sp / vol_1s, 4) if vol_1s > 1e-6 else None
    half_spread_to_move = (
        round(half_spread_net / med_abs_move_1s, 4)
        if med_abs_move_1s is not None and med_abs_move_1s > 1e-6
        else None
    )

    return {
        "samples": len(valid_books),
        "insufficient_samples": False,
        **cont,
        "maker_fee_bps": round(maker_fee, 6),
        "taker_fee_bps": round(taker_fee, 6),
        "median_spread_bps": round(med_sp, 4),
        "half_spread_minus_fee_bps": round(half_spread_net, 4),
        "realized_1s_vol_bps": round(vol_1s, 4),
        "median_1s_abs_move_bps": round(med_abs_move_1s, 4) if med_abs_move_1s is not None else None,
        "spread_to_vol_ratio": spread_to_vol,
        "half_spread_to_1s_move_ratio": half_spread_to_move,
        "median_l1_min_usd": round(med_l1, 2),
        "trades_count": trades_count,
        "trades_per_hour": trades_per_hour,
    }


def compute_session_microstructure_metrics(
    books: Sequence[tuple[int, float, float, float, float]],
    *,
    trade_timestamps_ms: Sequence[int] | None = None,
    maker_fee_bps: float = 0.0,
    taker_fee_bps: float = 0.0,
) -> dict[str, dict[str, Any]]:
    """Compute overall (`all`) and per-UTC-session (`us_rth`, `us_extended`, `overnight_asia`) metrics."""
    buckets_bbo: dict[str, list[tuple[int, float, float, float, float]]] = {s: [] for s in UTC_SESSIONS}
    for row in books:
        sess = classify_utc_session(row[0])
        buckets_bbo["all"].append(row)
        buckets_bbo[sess].append(row)

    buckets_trades: dict[str, list[int] | None]
    if trade_timestamps_ms is None:
        buckets_trades = {s: None for s in UTC_SESSIONS}
    else:
        buckets_trades = {s: [] for s in UTC_SESSIONS}
        for ts_ms in trade_timestamps_ms:
            sess = classify_utc_session(ts_ms)
            buckets_trades["all"].append(int(ts_ms))  # type: ignore[union-attr]
            buckets_trades[sess].append(int(ts_ms))  # type: ignore[union-attr]

    out: dict[str, dict[str, Any]] = {}
    for sess in UTC_SESSIONS:
        res = compute_slice_microstructure_metrics(
            buckets_bbo[sess],
            trade_timestamps_ms=buckets_trades[sess],
            maker_fee_bps=maker_fee_bps,
            taker_fee_bps=taker_fee_bps,
            is_subsession=(sess != "all"),
        )
        res["session"] = sess
        res["session_label"] = SESSION_LABELS[sess]
        out[sess] = res
    return out


def load_fee_evidence_overrides(evidence_path: Path | None) -> dict[str, dict[str, Any]]:
    """Parse optional `account_fee_evidence_*.json` for venue audit status and fee tiers."""
    if evidence_path is None:
        return {}
    data = json.loads(evidence_path.read_text(encoding="utf-8"))
    venues = data.get("venues")
    if not isinstance(venues, dict):
        raise ValueError(f"Expected dict 'venues' in {evidence_path}")

    overrides: dict[str, dict[str, Any]] = {}
    for venue_name, payload in venues.items():
        if not isinstance(payload, dict):
            continue
        entry: dict[str, Any] = {}
        if payload.get("account_fee_status"):
            entry["fee_audit_status"] = str(payload["account_fee_status"])

        maker = _opt_finite(payload.get("maker_fee_bps"))
        taker = _opt_finite(payload.get("taker_fee_bps"))
        if maker is not None:
            entry["maker_fee_bps"] = maker
        if taker is not None:
            entry["taker_fee_bps"] = taker

        if venue_name == "arcus":
            pub = payload.get("public_tier0_schedule_summary") or {}
            custom = pub.get("custom_schedule_tier0_bps") or {}
            per_market: dict[str, tuple[float, float]] = {}
            for mkt, fees in custom.items():
                if isinstance(fees, dict):
                    mk = _opt_finite(fees.get("maker_fee_bps"))
                    tk = _opt_finite(fees.get("taker_fee_bps"))
                    if mk is not None and tk is not None:
                        per_market[str(mkt)] = (mk, tk)
            if per_market:
                entry["per_market_fees"] = per_market
            if maker is None:
                entry.setdefault("maker_fee_bps_equity", 0.0)
                entry.setdefault("taker_fee_bps_equity", 2.25)

        if venue_name == "lighter" and maker is None:
            pub_mk = _opt_finite(payload.get("public_schedule_maker_fee_bps"))
            pub_tk = _opt_finite(payload.get("public_schedule_taker_fee_bps"))
            if pub_mk is not None:
                entry["maker_fee_bps"] = pub_mk
            if pub_tk is not None:
                entry["taker_fee_bps"] = pub_tk

        if venue_name == "trade_xyz" and maker is None:
            gm_mk = _opt_finite(payload.get("growth_mode_maker_fee_bps"))
            gm_tk = _opt_finite(payload.get("growth_mode_taker_fee_bps"))
            if gm_mk is not None:
                entry["maker_fee_bps"] = gm_mk
            if gm_tk is not None:
                entry["taker_fee_bps"] = gm_tk

        overrides[str(venue_name)] = entry
    return overrides


def _resolve_venue_metadata(
    venue: str,
    symbol: str,
    raw_market: str | None,
    domain: str,
    fee_overrides: dict[str, dict[str, Any]],
) -> dict[str, Any]:
    vp = VENUE_PROFILES.get(venue, {})
    settlement = str(vp.get("settlement") or "USDC")
    fee_status = str(vp.get("fee_audit_status") or "unknown")
    is_crypto = domain == "crypto" or symbol in CRYPTO_SYMBOLS
    maker_fee = float(vp.get("maker_fee_bps_crypto" if is_crypto else "maker_fee_bps_equity", 0.0))
    taker_fee = float(vp.get("taker_fee_bps_crypto" if is_crypto else "taker_fee_bps_equity", 0.0))
    bbo_cadence = str(vp.get("bbo_cadence") or "unknown")
    has_trades = bool(vp.get("has_trades", True))

    ov = fee_overrides.get(venue)
    if ov:
        if "fee_audit_status" in ov:
            fee_status = str(ov["fee_audit_status"])
        if "maker_fee_bps" in ov:
            maker_fee = float(ov["maker_fee_bps"])
        elif not is_crypto and "maker_fee_bps_equity" in ov:
            maker_fee = float(ov["maker_fee_bps_equity"])
        if "taker_fee_bps" in ov:
            taker_fee = float(ov["taker_fee_bps"])
        elif not is_crypto and "taker_fee_bps_equity" in ov:
            taker_fee = float(ov["taker_fee_bps_equity"])
        per_mkt = ov.get("per_market_fees") or {}
        if raw_market and raw_market in per_mkt:
            maker_fee, taker_fee = per_mkt[raw_market]

    resolved_market = raw_market or VENUE_SYMBOL_MAP.get(venue, {}).get(symbol, symbol)
    return {
        "raw_market": resolved_market,
        "settlement": settlement,
        "fee_audit_status": fee_status,
        "maker_fee_bps": round(maker_fee, 6),
        "taker_fee_bps": round(taker_fee, 6),
        "bbo_cadence": bbo_cadence,
        "has_trades": has_trades,
    }


def _aggregate_session_across_windows(
    session_rows: Sequence[dict[str, Any]],
    session: str,
) -> dict[str, Any]:
    valid = [
        r
        for r in session_rows
        if not r.get("insufficient_samples")
        and r.get("half_spread_minus_fee_bps") is not None
        and r.get("median_l1_min_usd") is not None
    ]
    samples_total = sum(int(r.get("samples") or 0) for r in session_rows)
    hours_total = round(sum(float(r.get("hours") or 0.0) for r in session_rows), 3)
    if not valid:
        return {
            "session": session,
            "session_label": SESSION_LABELS.get(session, session),
            "windows_observed": 0,
            "samples_total": samples_total,
            "hours_total": hours_total,
            "worst_half_spread_minus_fee_bps": None,
            "mean_half_spread_minus_fee_bps": None,
            "mean_spread_bps": None,
            "mean_vol_1s_bps": None,
            "mean_1s_abs_move_bps": None,
            "mean_spread_to_vol_ratio": None,
            "mean_half_spread_to_1s_move_ratio": None,
            "min_l1_usd": None,
            "mean_trades_per_hour": None,
        }

    hs_vals = [float(r["half_spread_minus_fee_bps"]) for r in valid]
    sp_vals = [float(r["median_spread_bps"]) for r in valid if r.get("median_spread_bps") is not None]
    vol_vals = [float(r["realized_1s_vol_bps"]) for r in valid if r.get("realized_1s_vol_bps") is not None]
    mv_vals = [float(r["median_1s_abs_move_bps"]) for r in valid if r.get("median_1s_abs_move_bps") is not None]
    stv_vals = [float(r["spread_to_vol_ratio"]) for r in valid if r.get("spread_to_vol_ratio") is not None]
    hsm_vals = [
        float(r["half_spread_to_1s_move_ratio"])
        for r in valid
        if r.get("half_spread_to_1s_move_ratio") is not None
    ]
    l1_vals = [float(r["median_l1_min_usd"]) for r in valid]
    tph_vals = [float(r["trades_per_hour"]) for r in valid if r.get("trades_per_hour") is not None]

    return {
        "session": session,
        "session_label": SESSION_LABELS.get(session, session),
        "windows_observed": len(valid),
        "samples_total": samples_total,
        "hours_total": hours_total,
        "worst_half_spread_minus_fee_bps": round(min(hs_vals), 4),
        "mean_half_spread_minus_fee_bps": round(sum(hs_vals) / len(hs_vals), 4),
        "mean_spread_bps": round(sum(sp_vals) / len(sp_vals), 4) if sp_vals else None,
        "mean_vol_1s_bps": round(sum(vol_vals) / len(vol_vals), 4) if vol_vals else None,
        "mean_1s_abs_move_bps": round(sum(mv_vals) / len(mv_vals), 4) if mv_vals else None,
        "mean_spread_to_vol_ratio": round(sum(stv_vals) / len(stv_vals), 4) if stv_vals else None,
        "mean_half_spread_to_1s_move_ratio": round(sum(hsm_vals) / len(hsm_vals), 4) if hsm_vals else None,
        "min_l1_usd": round(min(l1_vals), 2),
        "mean_trades_per_hour": round(sum(tph_vals) / len(tph_vals), 3) if tph_vals else None,
    }


def _rank_preferred_sessions(session_summary: dict[str, dict[str, Any]]) -> list[str]:
    ranked: list[tuple[float, float, str]] = []
    for sess in TRADING_SESSIONS:
        row = session_summary.get(sess) or {}
        if not row.get("windows_observed"):
            continue
        ratio = row.get("mean_half_spread_to_1s_move_ratio")
        vol = row.get("mean_vol_1s_bps")
        hs = row.get("mean_half_spread_minus_fee_bps")
        if ratio is None or hs is None:
            continue
        # Higher half_spread_to_1s_move_ratio and lower 1s vol are preferred (mitigates corr(vol, net) < 0).
        ranked.append((float(ratio), -(float(vol) if vol is not None else 0.0), sess))
    ranked.sort(key=lambda item: (item[0], item[1]), reverse=True)
    return [item[2] for item in ranked]


def evaluate_candidate_records(
    records_by_pair: dict[tuple[str, str], list[dict[str, Any]]],
    *,
    gates: WhitelistGates,
    source_mode: str,
    session_breakdown_available: bool,
    extra_meta: dict[str, Any] | None = None,
) -> dict[str, Any]:
    """Aggregate per-window observations, enforce frozen whitelist gates, and rank candidates."""
    whitelist: list[dict[str, Any]] = []
    excluded: list[dict[str, Any]] = []
    session_matrix: list[dict[str, Any]] = []
    trades_skipped_any = False

    for (venue, symbol), win_rows in sorted(records_by_pair.items()):
        first = win_rows[0]
        raw_market = str(first.get("raw_market") or symbol)
        settlement = str(first.get("settlement") or "USDC")
        fee_audit_status = str(first.get("fee_audit_status") or "unknown")
        maker_fee_bps = float(first.get("maker_fee_bps") or 0.0)
        taker_fee_bps = float(first.get("taker_fee_bps") or 0.0)
        windows_list = [str(r.get("window") or f"win_{i}") for i, r in enumerate(win_rows)]

        session_summary: dict[str, dict[str, Any]] = {}
        if session_breakdown_available:
            for sess in UTC_SESSIONS:
                sess_slices = [r["sessions"][sess] for r in win_rows if sess in (r.get("sessions") or {})]
                agg_sess = _aggregate_session_across_windows(sess_slices, sess)
                session_summary[sess] = agg_sess
                if sess != "all" and agg_sess["windows_observed"] > 0:
                    session_matrix.append(
                        {
                            "venue": venue,
                            "symbol": symbol,
                            "raw_market": raw_market,
                            **agg_sess,
                        }
                    )
            all_agg = session_summary["all"]
            preferred_sessions = _rank_preferred_sessions(session_summary)
        else:
            all_slices = [r["all_summary"] for r in win_rows if r.get("all_summary")]
            all_agg = _aggregate_session_across_windows(all_slices, "all")
            session_summary = {"all": all_agg}
            preferred_sessions = []

        worst_hs = all_agg["worst_half_spread_minus_fee_bps"]
        mean_stv = all_agg["mean_spread_to_vol_ratio"]
        min_l1 = all_agg["min_l1_usd"]
        mean_vol = all_agg["mean_vol_1s_bps"]
        mean_tph = all_agg["mean_trades_per_hour"]
        windows_valid = int(all_agg["windows_observed"])

        reject_reasons: list[str] = []
        if gates.require_zero_maker_fee and maker_fee_bps > 1e-9:
            reject_reasons.append(f"nonzero_maker_fee:{maker_fee_bps:.4f}>0")

        if (
            windows_valid == 0
            or worst_hs is None
            or mean_stv is None
            or min_l1 is None
            or mean_vol is None
        ):
            reject_reasons.append("insufficient_data")
        else:
            if worst_hs < gates.min_half_spread_bps:
                reject_reasons.append(
                    f"half_spread_below_min:{worst_hs:.4f}<{gates.min_half_spread_bps:.4f}"
                )
            if mean_stv < gates.min_spread_to_vol:
                reject_reasons.append(
                    f"spread_to_vol_below_min:{mean_stv:.4f}<{gates.min_spread_to_vol:.4f}"
                )
            if min_l1 < gates.min_l1_usd:
                reject_reasons.append(f"l1_depth_below_min:{min_l1:.2f}<{gates.min_l1_usd:.2f}")
            if mean_vol > gates.max_vol_bps:
                reject_reasons.append(f"vol_above_max:{mean_vol:.4f}>{gates.max_vol_bps:.4f}")

        if mean_tph is not None:
            if mean_tph < gates.min_trades_per_hour:
                reject_reasons.append(
                    f"trades_per_hour_below_min:{mean_tph:.2f}<{gates.min_trades_per_hour:.2f}"
                )
        else:
            trades_skipped_any = True

        score: float | None = None
        if worst_hs is not None and mean_stv is not None:
            score = round(worst_hs * min(mean_stv, 2.0), 4)

        candidate_entry: dict[str, Any] = {
            "venue": venue,
            "symbol": symbol,
            "raw_market": raw_market,
            "settlement": settlement,
            "fee_audit_status": fee_audit_status,
            "maker_fee_bps": round(maker_fee_bps, 6),
            "taker_fee_bps": round(taker_fee_bps, 6),
            "windows_checked": len(win_rows),
            "windows_valid": windows_valid,
            "windows": windows_list,
            "worst_half_spread_minus_fee_bps": worst_hs,
            "mean_spread_to_vol_ratio": mean_stv,
            "min_l1_usd": min_l1,
            "mean_vol_1s_bps": mean_vol,
            "mean_trades_per_hour": mean_tph,
            "preferred_sessions": preferred_sessions,
            "whitelist_score": score,
            "session_summary": session_summary,
        }

        if reject_reasons:
            candidate_entry["reject_reasons"] = reject_reasons
            excluded.append(candidate_entry)
        else:
            whitelist.append(candidate_entry)

    whitelist.sort(
        key=lambda r: (
            -(float(r["whitelist_score"]) if r["whitelist_score"] is not None else -1e18),
            r["venue"],
            r["symbol"],
        )
    )
    excluded.sort(key=lambda r: (r["venue"], r["symbol"]))
    session_matrix.sort(
        key=lambda r: (
            r["venue"],
            r["symbol"],
            TRADING_SESSIONS.index(r["session"]) if r["session"] in TRADING_SESSIONS else 99,
        )
    )

    gates_dict = asdict(gates)
    if trades_skipped_any:
        gates_dict["trades_check_note"] = (
            "trades_check_skipped_multivenue_mode"
            if source_mode == "multivenue_json"
            else "trades_check_skipped_unavailable_for_some_venues"
        )

    out: dict[str, Any] = {
        "schema": "rwa_mm_whitelist_v1",
        "source_mode": source_mode,
        "session_breakdown_available": session_breakdown_available,
        "session_labels": SESSION_LABELS,
        "gates": gates_dict,
        "whitelist_count": len(whitelist),
        "excluded_count": len(excluded),
        "whitelist": whitelist,
        "excluded": excluded,
        "session_matrix": session_matrix,
    }
    if extra_meta:
        out.update(extra_meta)
    return out


def screen_streams(
    streams: Sequence[dict[str, Any]],
    *,
    gates: WhitelistGates,
    fee_overrides: dict[str, dict[str, Any]] | None = None,
) -> dict[str, Any]:
    """Screen in-memory `(venue, symbol, window, books, trade_timestamps_ms)` records."""
    overrides = fee_overrides or {}
    records_by_pair: dict[tuple[str, str], list[dict[str, Any]]] = defaultdict(list)
    for item in streams:
        venue = str(item["venue"])
        symbol = str(item["symbol"])
        window = str(item.get("window") or "w_custom")
        domain = str(item.get("domain") or ("crypto" if symbol in CRYPTO_SYMBOLS else "rwa"))
        meta = _resolve_venue_metadata(
            venue,
            symbol,
            item.get("raw_market"),
            domain,
            overrides,
        )
        maker_fee = (
            float(item["maker_fee_bps"])
            if item.get("maker_fee_bps") is not None
            else float(meta["maker_fee_bps"])
        )
        taker_fee = (
            float(item["taker_fee_bps"])
            if item.get("taker_fee_bps") is not None
            else float(meta["taker_fee_bps"])
        )
        sess_metrics = compute_session_microstructure_metrics(
            item.get("books") or (),
            trade_timestamps_ms=item.get("trade_timestamps_ms"),
            maker_fee_bps=maker_fee,
            taker_fee_bps=taker_fee,
        )
        records_by_pair[(venue, symbol)].append(
            {
                "window": window,
                "raw_market": meta["raw_market"],
                "settlement": item.get("settlement") or meta["settlement"],
                "fee_audit_status": item.get("fee_audit_status") or meta["fee_audit_status"],
                "maker_fee_bps": maker_fee,
                "taker_fee_bps": taker_fee,
                "sessions": sess_metrics,
            }
        )
    return evaluate_candidate_records(
        records_by_pair,
        gates=gates,
        source_mode="stream_in_memory",
        session_breakdown_available=True,
    )


def screen_multivenue_json_files(
    mv_paths: Sequence[Path],
    *,
    gates: WhitelistGates,
    venues_filter: Sequence[str] | None = None,
    symbols_filter: Sequence[str] | None = None,
    include_crypto: bool = False,
    fee_overrides: dict[str, dict[str, Any]] | None = None,
) -> dict[str, Any]:
    """Aggregate `multivenue_three_tracks_*.json` Track 1 rows into `rwa_mm_whitelist_v1`."""
    overrides = fee_overrides or {}
    v_set = set(venues_filter) if venues_filter else None
    s_set = set(symbols_filter) if symbols_filter else None

    records_by_pair: dict[tuple[str, str], list[dict[str, Any]]] = defaultdict(list)
    loaded_files: list[str] = []

    for path in mv_paths:
        raw = json.loads(path.read_text(encoding="utf-8"))
        loaded_files.append(str(path))
        split_label = str(raw.get("split") or path.stem)
        t1_rows = raw.get("track1_quant_profit")
        if not isinstance(t1_rows, list):
            raise ValueError(f"Expected list 'track1_quant_profit' in {path}")

        for row in t1_rows:
            if not isinstance(row, dict):
                continue
            venue = str(row.get("venue") or "")
            symbol = str(row.get("symbol") or "")
            if not venue or not symbol:
                continue
            domain = str(row.get("domain") or ("crypto" if symbol in CRYPTO_SYMBOLS else "rwa"))
            if not include_crypto and domain == "crypto" and (s_set is None or symbol not in s_set):
                continue
            if v_set is not None and venue not in v_set:
                continue
            if s_set is not None and symbol not in s_set:
                continue

            meta = _resolve_venue_metadata(
                venue,
                symbol,
                row.get("raw_market"),
                domain,
                overrides,
            )
            row_maker_fee = _opt_finite(row.get("maker_fee_bps"))
            maker_fee = row_maker_fee if row_maker_fee is not None else float(meta["maker_fee_bps"])
            if venue in overrides and "maker_fee_bps" in overrides[venue]:
                maker_fee = float(overrides[venue]["maker_fee_bps"])

            fee_status = str(meta["fee_audit_status"])
            if row.get("fee_audit_status") and venue not in overrides:
                fee_status = str(row["fee_audit_status"])

            med_sp = _opt_finite(row.get("median_spread_bps"))
            hs_raw = _opt_finite(row.get("half_spread_minus_fee_bps"))
            # Re-derive half_spread_minus_fee_bps if fee override shifted maker_fee_bps.
            if med_sp is not None and (hs_raw is None or (row_maker_fee is not None and abs(maker_fee - row_maker_fee) > 1e-9)):
                hs_val: float | None = round(0.5 * med_sp - maker_fee, 4)
            else:
                hs_val = hs_raw

            insuff = bool(row.get("insufficient_samples")) or hs_val is None
            tph = _opt_finite(row.get("trades_per_hour"))
            all_summary = {
                "samples": int(row.get("samples") or 0),
                "hours": float(row.get("hours") or 0.0),
                "max_gap_ms": int(row.get("max_gap_ms") or 0),
                "is_continuous_10h": bool(row.get("is_continuous_10h", False)),
                "insufficient_samples": insuff,
                "median_spread_bps": med_sp,
                "half_spread_minus_fee_bps": hs_val,
                "realized_1s_vol_bps": _opt_finite(row.get("realized_1s_vol_bps")),
                "median_1s_abs_move_bps": _opt_finite(row.get("median_1s_abs_move_bps")),
                "spread_to_vol_ratio": _opt_finite(row.get("spread_to_1s_vol_ratio") or row.get("spread_to_vol_ratio")),
                "half_spread_to_1s_move_ratio": _opt_finite(row.get("half_spread_to_1s_move_ratio")),
                "median_l1_min_usd": _opt_finite(row.get("median_l1_min_usd")),
                "trades_count": row.get("trades_count"),
                "trades_per_hour": tph,
            }
            records_by_pair[(venue, symbol)].append(
                {
                    "window": split_label,
                    "raw_market": meta["raw_market"],
                    "settlement": row.get("settlement") or meta["settlement"],
                    "fee_audit_status": fee_status,
                    "maker_fee_bps": maker_fee,
                    "taker_fee_bps": meta["taker_fee_bps"],
                    "all_summary": all_summary,
                }
            )

    return evaluate_candidate_records(
        records_by_pair,
        gates=gates,
        source_mode="multivenue_json",
        session_breakdown_available=False,
        extra_meta={"multivenue_sources": loaded_files},
    )


def _campaign_parquet_files(
    lakehouse_dir: Path, dataset: str, venue: str, utc_date: str, campaign_id: str
) -> list[str]:
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


def _sql_list(files: Sequence[str]) -> str:
    return "[" + ", ".join("'" + f.replace("'", "''") + "'" for f in files) + "]"


def screen_lakehouse(
    lakehouse_dir: Path,
    *,
    windows: Sequence[str],
    venues: Sequence[str],
    symbols: Sequence[str],
    gates: WhitelistGates,
    bucket_ms: int = 250,
    fee_overrides: dict[str, dict[str, Any]] | None = None,
) -> dict[str, Any]:
    """Load 250ms-bucketed BBO & public_trades via read-only in-memory DuckDB and screen universe."""
    import duckdb  # Lazy import so stdlib-only CI never needs duckdb installed.

    overrides = fee_overrides or {}
    con = duckdb.connect(":memory:")
    con.execute("SET preserve_insertion_order=false; SET threads=2; SET memory_limit='2GB';")

    records_by_pair: dict[tuple[str, str], list[dict[str, Any]]] = defaultdict(list)

    try:
        for win in windows:
            if win not in CANONICAL_CAMPAIGNS:
                raise ValueError(f"Unknown campaign window {win!r}; expected one of {sorted(CANONICAL_CAMPAIGNS)}")
            win_cfg = CANONICAL_CAMPAIGNS[win]
            for venue in venues:
                venue_map = VENUE_SYMBOL_MAP.get(venue, {})
                for symbol in symbols:
                    if symbol not in venue_map:
                        continue
                    domain = "crypto" if symbol in CRYPTO_SYMBOLS else "rwa"
                    c_info = win_cfg["bulk"] if (venue == "bulk" and "bulk" in win_cfg) else win_cfg[domain]
                    raw_market = venue_map[symbol]
                    meta = _resolve_venue_metadata(venue, symbol, raw_market, domain, overrides)

                    bbo_files = _campaign_parquet_files(
                        lakehouse_dir, "bbo_quotes", venue, c_info["utc_date"], c_info["campaign_id"]
                    )
                    books: list[tuple[int, float, float, float, float]] = []
                    if bbo_files:
                        bbo_sql = _sql_list(bbo_files)
                        books = con.execute(
                            f"""
                            SELECT MIN(available_at_ms) AS ts_ms,
                                   arg_min(bid_px_f64, available_at_ms) AS bid_px,
                                   arg_min(CAST(bid_sz AS DOUBLE), available_at_ms) AS bid_sz,
                                   arg_min(ask_px_f64, available_at_ms) AS ask_px,
                                   arg_min(CAST(ask_sz AS DOUBLE), available_at_ms) AS ask_sz
                            FROM read_parquet({bbo_sql})
                            WHERE market = ?
                              AND tradable = true
                              AND bid_px_f64 > 0 AND ask_px_f64 > bid_px_f64
                            GROUP BY (available_at_ms // {int(bucket_ms)})
                            ORDER BY ts_ms ASC
                            """,
                            [raw_market],
                        ).fetchall()

                    trade_ts: list[int] | None = None
                    if meta["has_trades"]:
                        trade_files = _campaign_parquet_files(
                            lakehouse_dir, "public_trades", venue, c_info["utc_date"], c_info["campaign_id"]
                        )
                        if trade_files:
                            tr_sql = _sql_list(trade_files)
                            trade_rows = con.execute(
                                f"""
                                SELECT available_at_ms
                                FROM read_parquet({tr_sql})
                                WHERE market = ?
                                  AND NOT is_zero_price_transfer
                                  AND price_f64 > 0 AND size_f64 > 0
                                ORDER BY available_at_ms ASC
                                """,
                                [raw_market],
                            ).fetchall()
                            trade_ts = [int(r[0]) for r in trade_rows]
                        else:
                            trade_ts = []

                    sess_metrics = compute_session_microstructure_metrics(
                        books,
                        trade_timestamps_ms=trade_ts,
                        maker_fee_bps=float(meta["maker_fee_bps"]),
                        taker_fee_bps=float(meta["taker_fee_bps"]),
                    )
                    records_by_pair[(venue, symbol)].append(
                        {
                            "window": win,
                            "raw_market": raw_market,
                            "settlement": meta["settlement"],
                            "fee_audit_status": meta["fee_audit_status"],
                            "maker_fee_bps": meta["maker_fee_bps"],
                            "taker_fee_bps": meta["taker_fee_bps"],
                            "sessions": sess_metrics,
                        }
                    )
    finally:
        con.close()

    return evaluate_candidate_records(
        records_by_pair,
        gates=gates,
        source_mode="lakehouse",
        session_breakdown_available=True,
        extra_meta={"lakehouse_dir": str(lakehouse_dir), "windows_requested": list(windows)},
    )


def _make_synthetic_bbo_stream(
    start_ts_ms: int,
    n_samples: int,
    *,
    step_ms: int = 1000,
    spread_bps: float = 2.0,
    step_move_bps: float = 0.5,
    l1_usd: float = 1200.0,
) -> list[tuple[int, float, float, float, float]]:
    books: list[tuple[int, float, float, float, float]] = []
    mid = 100.0
    half_frac = 0.5 * spread_bps * 1e-4
    for i in range(n_samples):
        if i > 0:
            sign = 1.0 if (i % 2 == 0) else -1.0
            mid *= math.exp(sign * step_move_bps * 1e-4)
        bid = mid * (1.0 - half_frac)
        ask = mid * (1.0 + half_frac)
        bid_sz = l1_usd / bid
        ask_sz = l1_usd / ask
        books.append((start_ts_ms + i * step_ms, bid, bid_sz, ask, ask_sz))
    return books


def run_self_test() -> dict[str, Any]:
    """Run synthetic stdlib-only self-tests covering sessions, metrics, gates, and multivenue mode."""
    # 1. UTC session classification boundaries.
    def ts_utc(hour: int, minute: int) -> int:
        return int(datetime(2026, 11, 25, hour, minute, 0, tzinfo=timezone.utc).timestamp() * 1000)

    assert classify_utc_session(ts_utc(0, 0)) == "overnight_asia"
    assert classify_utc_session(ts_utc(10, 59)) == "overnight_asia"
    assert classify_utc_session(ts_utc(11, 0)) == "us_extended"
    assert classify_utc_session(ts_utc(13, 29)) == "us_extended"
    assert classify_utc_session(ts_utc(13, 30)) == "us_rth"
    assert classify_utc_session(ts_utc(19, 59)) == "us_rth"
    assert classify_utc_session(ts_utc(20, 0)) == "us_extended"
    assert classify_utc_session(ts_utc(23, 59)) == "us_extended"

    # 2. Microstructure & half_spread_to_1s_move_ratio calculation.
    t_asia = ts_utc(2, 0)
    books_asia = _make_synthetic_bbo_stream(t_asia, 60, spread_bps=3.0, step_move_bps=0.5, l1_usd=1500.0)
    t_rth = ts_utc(14, 0)
    books_rth = _make_synthetic_bbo_stream(t_rth, 60, spread_bps=3.0, step_move_bps=3.5, l1_usd=1500.0)

    # Combined stream spanning overnight_asia (low vol) + us_rth (high vol)
    combined_books = books_asia + books_rth
    trades_all = [t_asia + i * 2_000 for i in range(30)] + [t_rth + i * 2_000 for i in range(30)]

    metrics = compute_session_microstructure_metrics(
        combined_books, trade_timestamps_ms=trades_all, maker_fee_bps=0.0
    )
    assert math.isclose(metrics["overnight_asia"]["half_spread_minus_fee_bps"], 1.5, abs_tol=1e-2)
    assert math.isclose(metrics["overnight_asia"]["median_1s_abs_move_bps"], 0.5, abs_tol=0.02)
    assert math.isclose(metrics["overnight_asia"]["half_spread_to_1s_move_ratio"], 3.0, abs_tol=0.1)
    assert metrics["us_rth"]["realized_1s_vol_bps"] > metrics["overnight_asia"]["realized_1s_vol_bps"]

    # 3. Stream screening with admission + nonzero maker fee rejection + high-vol rejection.
    streams = [
        {
            "venue": "arcus",
            "symbol": "DRAM",
            "window": "w19",
            "books": books_asia,
            "trade_timestamps_ms": [t_asia + i * 1_000 for i in range(60)],
        },
        {
            "venue": "trade_xyz",
            "symbol": "NVDA",
            "window": "w19",
            "books": books_asia,
            "trade_timestamps_ms": [t_asia + i * 1_000 for i in range(60)],
        },
        {
            "venue": "arcus",
            "symbol": "SNDK",
            "window": "w19",
            "books": books_rth,
            "trade_timestamps_ms": [t_rth + i * 1_000 for i in range(60)],
        },
    ]
    res = screen_streams(streams, gates=WhitelistGates())
    assert res["schema"] == "rwa_mm_whitelist_v1"
    assert [(r["venue"], r["symbol"]) for r in res["whitelist"]] == [("arcus", "DRAM")]
    excl = {(r["venue"], r["symbol"]): r["reject_reasons"] for r in res["excluded"]}
    assert any("nonzero_maker_fee" in reason for reason in excl[("trade_xyz", "NVDA")])
    assert any("vol_above_max" in reason for reason in excl[("arcus", "SNDK")])

    return {
        "self_test": "ok",
        "whitelist_passed": [(r["venue"], r["symbol"], r["whitelist_score"]) for r in res["whitelist"]],
        "excluded_reasons": {f"{k[0]}:{k[1]}": v for k, v in excl.items()},
        "asia_half_spread_to_1s_move": metrics["overnight_asia"]["half_spread_to_1s_move_ratio"],
    }


def _parse_csv_list(raw_values: Sequence[str] | None) -> list[str]:
    if not raw_values:
        return []
    out: list[str] = []
    for item in raw_values:
        for tok in str(item).split(","):
            tok_s = tok.strip()
            if tok_s:
                out.append(tok_s)
    return out


def build_arg_parser() -> argparse.ArgumentParser:
    ap = argparse.ArgumentParser(
        description="Screen RWA perpetual MM universe across UTC sessions and frozen gates"
    )
    ap.add_argument("--lakehouse", type=Path, help="Path to Aleph lakehouse root directory")
    ap.add_argument(
        "--multivenue",
        type=Path,
        nargs="+",
        help="Offline multivenue_three_tracks_*.json files (zero DuckDB needed)",
    )
    ap.add_argument("--fee-evidence", type=Path, help="Optional account_fee_evidence_*.json path")
    ap.add_argument(
        "--windows",
        nargs="+",
        default=list(DEFAULT_WINDOWS),
        help="Campaign windows for --lakehouse (default: w18,w19,w20,w21)",
    )
    ap.add_argument(
        "--venues",
        nargs="+",
        help="Venues to include (default in lakehouse: arcus,lighter,lighter_robinhood)",
    )
    ap.add_argument(
        "--symbols",
        nargs="+",
        help="Canonical symbols to include (default in lakehouse: DRAM,SNDK,NVDA,QQQ,SPY,AAPL,TSLA,AMZN,MU)",
    )
    ap.add_argument("--include-crypto", action="store_true", help="Include BTC/ETH/SOL in --multivenue")
    ap.add_argument("--min-half-spread-bps", type=float, default=0.25)
    ap.add_argument("--min-spread-to-vol", type=float, default=0.45)
    ap.add_argument("--min-l1-usd", type=float, default=300.0)
    ap.add_argument("--max-vol-bps", type=float, default=2.5)
    ap.add_argument("--min-trades-per-hour", type=float, default=50.0)
    ap.add_argument(
        "--require-zero-maker-fee",
        dest="require_zero_maker_fee",
        action="store_true",
        default=True,
        help="Require zero maker fee (default: enabled)",
    )
    ap.add_argument(
        "--allow-nonzero-maker-fee",
        dest="require_zero_maker_fee",
        action="store_false",
        help="Disable zero maker fee requirement",
    )
    ap.add_argument("--output", type=Path, help="Optional path to write JSON whitelist output")
    ap.add_argument("--self-test", action="store_true", help="Run built-in synthetic tests and exit")
    return ap


def main(argv: Sequence[str] | None = None) -> int:
    ap = build_arg_parser()
    args = ap.parse_args(argv)

    if args.self_test:
        print(json.dumps(run_self_test(), indent=2, allow_nan=False))
        return 0

    if not args.lakehouse and not args.multivenue:
        ap.error("Specify either --lakehouse <dir> or --multivenue <paths...> (or --self-test)")
    if args.lakehouse and args.multivenue:
        ap.error("Specify only one of --lakehouse or --multivenue")

    gates = WhitelistGates(
        min_half_spread_bps=args.min_half_spread_bps,
        min_spread_to_vol=args.min_spread_to_vol,
        min_l1_usd=args.min_l1_usd,
        max_vol_bps=args.max_vol_bps,
        min_trades_per_hour=args.min_trades_per_hour,
        require_zero_maker_fee=args.require_zero_maker_fee,
    )
    fee_overrides = load_fee_evidence_overrides(args.fee_evidence)
    venues_parsed = _parse_csv_list(args.venues)
    symbols_parsed = _parse_csv_list(args.symbols)

    if args.multivenue:
        result = screen_multivenue_json_files(
            args.multivenue,
            gates=gates,
            venues_filter=venues_parsed or None,
            symbols_filter=symbols_parsed or None,
            include_crypto=args.include_crypto,
            fee_overrides=fee_overrides,
        )
    else:
        windows_parsed = _parse_csv_list(args.windows) or list(DEFAULT_WINDOWS)
        result = screen_lakehouse(
            args.lakehouse,
            windows=windows_parsed,
            venues=venues_parsed or list(DEFAULT_VENUES),
            symbols=symbols_parsed or list(DEFAULT_SYMBOLS),
            gates=gates,
            fee_overrides=fee_overrides,
        )

    rendered = json.dumps(result, indent=2, allow_nan=False)
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(rendered + "\n", encoding="utf-8")
    else:
        print(rendered)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
