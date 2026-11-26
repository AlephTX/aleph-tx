#!/usr/bin/env python3
"""Unified offline three-track, multi-venue 10h continuous evaluator on Aleph lakehouse.

Evaluates Track 01 (Quant Profit), Track 02 (Hedge Arbitrage), and Track 03 (Incentive Turnover)
strictly independently (zero cross-subsidy, separate ledgers & metrics) across all venues in
`~/aleph-tx-lakehouse/parquet/` using in-memory DuckDB read-only queries with exact Hive path
pruning and SQL-side 250ms bucketing (< 1 GB RAM, zero contention with `lakehouse.duckdb`).
"""

from __future__ import annotations

import argparse
import glob
import json
import math
import os
from pathlib import Path
import sys
from typing import Any

SCRIPT_DIR = Path(__file__).resolve().parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

import sim_queue_priority_mm as sim

TEN_HOURS_MS = 36_000_000
MAX_CONTINUOUS_GAP_MS = 60_000

CANONICAL_CAMPAIGNS: dict[str, dict[str, dict[str, str]]] = {
    "selection_dev": {
        "crypto": {"campaign_id": "tokyo-crypto-20261123T061504Z-w19", "utc_date": "2026-11-23"},
        "rwa": {"campaign_id": "tokyo-rwa-20261123T061452Z-w19", "utc_date": "2026-11-23"},
        "bulk": {"campaign_id": "tokyo-bulk-20261123T061449Z-w19", "utc_date": "2026-11-23"},
    },
    "holdout": {
        "crypto": {"campaign_id": "tokyo-crypto-20261125T093007Z-w24", "utc_date": "2026-11-25"},
        "rwa": {"campaign_id": "tokyo-rwa-20261125T092953Z-w24", "utc_date": "2026-11-25"},
        "bulk": {"campaign_id": "tokyo-bulk-20261125T092949Z-w24", "utc_date": "2026-11-25"},
    },
    "holdout_w25": {
        "crypto": {"campaign_id": "tokyo-crypto-20261125T194507Z-w25", "utc_date": "2026-11-25"},
        "rwa": {"campaign_id": "tokyo-rwa-20261125T194453Z-w25", "utc_date": "2026-11-25"},
        "bulk": {"campaign_id": "tokyo-bulk-20261125T194449Z-w25", "utc_date": "2026-11-25"},
    },
}

# Canonical symbol mapping verified across Aleph DuckDB + public venue specs (2026-11-25/26).
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
    "binance": {
        "BTC": "BTCUSDT",
        "ETH": "ETHUSDT",
        "SOL": "SOLUSDT",
    },
}

VENUE_PROFILES: dict[str, dict[str, Any]] = {
    "arcus": {
        "profile": "fast_zero_fee_alo",
        "maker_fee_bps_crypto": 0.0,
        "maker_fee_bps_equity": 0.0,
        "taker_fee_bps_crypto": 2.30,
        "taker_fee_bps_equity": 1.15,
        "settlement": "USDC",
        "fee_audit_status": "audited",
        "bbo_cadence": "ws_event",
        "has_trades": True,
    },
    "lighter_robinhood": {
        "profile": "speedbump_zero_fee_wide",
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
        "profile": "speedbump_zero_fee_wide",
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
        "profile": "fast_fee_rebate_clob",
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
        "profile": "fast_fee_rebate_clob",
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
        "profile": "slow_l1_wide_spread",
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
        "profile": "slow_l1_wide_spread",
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
        "profile": "fast_fee_rebate_clob",
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


def compute_continuity_stats(timestamps_ms: list[int]) -> dict[str, Any]:
    """Check whether a chronological event stream satisfies continuous 10h unit requirements."""
    if len(timestamps_ms) < 2:
        return {
            "events": len(timestamps_ms),
            "duration_ms": 0,
            "max_gap_ms": 0,
            "is_continuous_10h": False,
        }
    dur = timestamps_ms[-1] - timestamps_ms[0]
    max_gap = max(timestamps_ms[i] - timestamps_ms[i - 1] for i in range(1, len(timestamps_ms)))
    return {
        "events": len(timestamps_ms),
        "duration_ms": dur,
        "hours": round(dur / 3_600_000.0, 3),
        "max_gap_ms": max_gap,
        "is_continuous_10h": dur >= TEN_HOURS_MS and max_gap <= MAX_CONTINUOUS_GAP_MS,
    }


def compute_track1_microstructure_metrics(
    books: list[tuple[int, float, float, float, float]],
    maker_fee_bps: float,
) -> dict[str, Any]:
    """Compute spread, causal 1s vol, spread/vol ratio, L1 depth, and half-spread after maker fee."""
    if len(books) < 20:
        return {"samples": len(books), "insufficient_samples": True}
    ts_list = [b[0] for b in books]
    mids = [0.5 * (b[1] + b[3]) for b in books]
    spreads = [(b[3] - b[1]) / (0.5 * (b[1] + b[3])) * 1e4 for b in books]
    l1_min_usd = [min(b[1] * b[2], b[3] * b[4]) for b in books]

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

    spreads_s = sorted(spreads)
    l1_s = sorted(l1_min_usd)
    med_sp = spreads_s[len(spreads_s) // 2]
    med_l1 = l1_s[len(l1_s) // 2]
    cont = compute_continuity_stats(ts_list)
    return {
        "samples": len(books),
        **cont,
        "median_spread_bps": round(med_sp, 3),
        "p90_spread_bps": round(spreads_s[int(0.90 * (len(spreads_s) - 1))], 3),
        "realized_1s_vol_bps": round(vol_1s, 3),
        "spread_to_1s_vol_ratio": round(med_sp / vol_1s, 3) if vol_1s > 1e-6 else None,
        "median_l1_min_usd": round(med_l1, 1),
        "half_spread_minus_fee_bps": round(0.5 * med_sp - maker_fee_bps, 3),
    }


def compute_track2_basis_pair_metrics(
    books_a: list[tuple[int, float, float]],
    books_b: list[tuple[int, float, float]],
    *,
    max_sync_skew_ms: int = 500,
    dual_taker_fee_bps: float = 0.0,
    maker_first_fee_bps: float = 0.0,
) -> dict[str, Any]:
    """Synchronize two venue `(ts_ms, bid, ask)` streams and quantify basis dislocation & hurdle hits."""
    if not books_a or not books_b:
        return {"paired_samples": 0}
    ts_b = [b[0] for b in books_b]
    aligned_basis_bps: list[float] = []
    aligned_spread_a: list[float] = []
    aligned_spread_b: list[float] = []
    j = 0
    nb = len(books_b)
    for ta, ba_bid, ba_ask in books_a:
        while j + 1 < nb and ts_b[j + 1] <= ta:
            j += 1
        tb, bb_bid, bb_ask = books_b[j]
        if tb > ta or (ta - tb) > max_sync_skew_ms:
            continue
        ma = 0.5 * (ba_bid + ba_ask)
        mb = 0.5 * (bb_bid + bb_ask)
        if ma <= 0.0 or mb <= 0.0:
            continue
        aligned_basis_bps.append((ma - mb) / (0.5 * (ma + mb)) * 1e4)
        aligned_spread_a.append((ba_ask - ba_bid) / ma * 1e4)
        aligned_spread_b.append((bb_ask - bb_bid) / mb * 1e4)

    if len(aligned_basis_bps) < 20:
        return {"paired_samples": len(aligned_basis_bps)}

    b_sorted = sorted(aligned_basis_bps)
    med_basis = b_sorted[len(b_sorted) // 2]
    disloc_abs = sorted(abs(x - med_basis) for x in aligned_basis_bps)
    med_sp_a = sorted(aligned_spread_a)[len(aligned_spread_a) // 2]
    med_sp_b = sorted(aligned_spread_b)[len(aligned_spread_b) // 2]

    dual_taker_hurdle = dual_taker_fee_bps + 0.5 * (med_sp_a + med_sp_b)
    maker_first_hurdle = maker_first_fee_bps + 0.5 * med_sp_b

    dt_hits = sum(1 for d in disloc_abs if d > dual_taker_hurdle)
    mf_hits = sum(1 for d in disloc_abs if d > maker_first_hurdle)
    return {
        "paired_samples": len(aligned_basis_bps),
        "median_raw_basis_bps": round(med_basis, 3),
        "med_spread_a_bps": round(med_sp_a, 3),
        "med_spread_b_bps": round(med_sp_b, 3),
        "p95_abs_disloc_bps": round(disloc_abs[int(0.95 * (len(disloc_abs) - 1))], 3),
        "p99_abs_disloc_bps": round(disloc_abs[int(0.99 * (len(disloc_abs) - 1))], 3),
        "dual_taker_hurdle_bps": round(dual_taker_hurdle, 3),
        "dual_taker_exceed_pct": round(dt_hits / len(disloc_abs) * 100.0, 3),
        "maker_first_hurdle_bps": round(maker_first_hurdle, 3),
        "maker_first_exceed_pct": round(mf_hits / len(disloc_abs) * 100.0, 3),
    }


def _infer_tick_from_prices(prices: list[float]) -> float:
    best = 1.0
    for px in prices[:200]:
        s = f"{px:.8f}".rstrip("0").rstrip(".")
        if "." in s:
            best = min(best, 10.0 ** (-len(s.split(".")[1])))
    return max(best, 1e-6)


def _campaign_parquet_files(
    lakehouse_dir: Path, dataset: str, venue: str, utc_date: str, campaign_id: str
) -> list[str]:
    from datetime import date, timedelta

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


def run_duckdb_lakehouse_eval(lakehouse_dir: Path, split: str = "holdout") -> dict[str, Any]:
    """Execute ultra-low-memory read-only 3-track evaluation directly on Hive parquet partitions."""
    import duckdb

    # Never open lakehouse.duckdb while background W25 pipelines may hold locks; use :memory: with strict cap.
    con = duckdb.connect(":memory:")
    con.execute("SET preserve_insertion_order=false; SET threads=2; SET memory_limit='2GB';")

    campaigns = CANONICAL_CAMPAIGNS[split]

    def _campaign_for(venue: str, domain: str) -> dict[str, str]:
        if venue == "bulk" and "bulk" in campaigns:
            return campaigns["bulk"]
        return campaigns["crypto" if domain == "crypto" else "rwa"]

    def fetch_books_bucketed(
        venue: str, market: str, domain: str, bucket_ms: int = 250
    ) -> tuple[list[tuple[int, float, float, float, float]], dict[str, Any]]:
        c_info = _campaign_for(venue, domain)
        files = _campaign_parquet_files(
            lakehouse_dir, "bbo_quotes", venue, c_info["utc_date"], c_info["campaign_id"]
        )
        if not files:
            return [], {"events": 0, "duration_ms": 0, "max_gap_ms": 0, "is_continuous_10h": False}
        f_list_sql = "[" + ", ".join(f"'{p}'" for p in files) + "]"
        raw_stats = con.execute(
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
        cnt, t0, t1, mgap = raw_stats
        if not cnt or cnt < 2:
            return [], {"events": int(cnt or 0), "duration_ms": 0, "max_gap_ms": 0, "is_continuous_10h": False}
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
        return rows, cont

    def fetch_trades(venue: str, market: str, domain: str) -> list[tuple[int, str, float, float]]:
        c_info = campaigns["crypto" if domain == "crypto" else "rwa"]
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

    # Keep only small 250ms-bucketed streams in a bounded cache (< 15MB each)
    bucket_cache: dict[tuple[str, str, str], tuple[list[tuple[int, float, float, float, float]], dict[str, Any]]] = {}

    def get_books(venue: str, canon: str, domain: str):
        key = (venue, canon, domain)
        if key not in bucket_cache:
            raw_mkt = VENUE_SYMBOL_MAP[venue][canon]
            bucket_cache[key] = fetch_books_bucketed(venue, raw_mkt, domain, bucket_ms=250)
        return bucket_cache[key]

    # === TRACK 01: 01_quant_profit Multi-Venue Microstructure Frontier ===
    track1_targets = [
        ("arcus", "NVDA", "rwa"),
        ("arcus", "SNDK", "rwa"),
        ("arcus", "DRAM", "rwa"),
        ("arcus", "SPY", "rwa"),
        ("arcus", "QQQ", "rwa"),
        ("arcus", "BTC", "crypto"),
        ("lighter_robinhood", "NVDA", "rwa"),
        ("lighter_robinhood", "QQQ", "rwa"),
        ("lighter_robinhood", "SPY", "rwa"),
        ("lighter_robinhood", "BTC", "crypto"),
        ("lighter_robinhood", "ETH", "crypto"),
        ("lighter", "NVDA", "rwa"),
        ("lighter", "QQQ", "rwa"),
        ("lighter", "BTC", "crypto"),
        ("extended", "BTC", "crypto"),
        ("extended", "ETH", "crypto"),
        ("bulk", "NVDA", "rwa"),
        ("bulk", "SNDK", "rwa"),
        ("bulk", "BTC", "crypto"),
        ("trade_xyz", "NVDA", "rwa"),
        ("trade_xyz", "SNDK", "rwa"),
        ("entropy", "NVDA", "rwa"),
        ("entropy", "SNDK", "rwa"),
    ]
    track1_rows: list[dict[str, Any]] = []
    for venue, canon, domain in track1_targets:
        rows, raw_cont = get_books(venue, canon, domain)
        vp = VENUE_PROFILES[venue]
        fee = vp["maker_fee_bps_crypto"] if domain == "crypto" else vp["maker_fee_bps_equity"]
        metrics = compute_track1_microstructure_metrics(rows, fee)
        metrics["raw_continuity"] = raw_cont
        metrics.update(
            {
                "venue": venue,
                "symbol": canon,
                "raw_market": VENUE_SYMBOL_MAP[venue][canon],
                "campaign_id": _campaign_for(venue, domain)["campaign_id"],
                "domain": domain,
                "maker_fee_bps": fee,
                "bbo_cadence": vp["bbo_cadence"],
                "fee_audit_status": vp["fee_audit_status"],
            }
        )
        track1_rows.append(metrics)

    # === TRACK 02: 02_hedge_arbitrage Synchronized Pair Basis & Funding Carry ===
    track2_pairs = [
        ("arcus", "lighter_robinhood", "NVDA", "rwa"),
        ("arcus", "lighter_robinhood", "QQQ", "rwa"),
        ("arcus", "lighter_robinhood", "SPY", "rwa"),
        ("arcus", "lighter", "QQQ", "rwa"),
        ("arcus", "trade_xyz", "NVDA", "rwa"),
        ("arcus", "trade_xyz", "SNDK", "rwa"),
        ("arcus", "bulk", "NVDA", "rwa"),
        ("arcus", "bulk", "BTC", "crypto"),
        ("lighter_robinhood", "lighter", "BTC", "crypto"),
        ("extended", "arcus", "BTC", "crypto"),
    ]
    track2_basis_rows: list[dict[str, Any]] = []
    for va, vb, canon, domain in track2_pairs:
        ba, _ = get_books(va, canon, domain)
        bb, _ = get_books(vb, canon, domain)
        pa, pb = VENUE_PROFILES[va], VENUE_PROFILES[vb]
        tf_a = pa["taker_fee_bps_crypto"] if domain == "crypto" else pa["taker_fee_bps_equity"]
        mf_a = pa["maker_fee_bps_crypto"] if domain == "crypto" else pa["maker_fee_bps_equity"]
        tf_b = pb["taker_fee_bps_crypto"] if domain == "crypto" else pb["taker_fee_bps_equity"]
        res = compute_track2_basis_pair_metrics(
            [(r[0], r[1], r[3]) for r in ba],
            [(r[0], r[1], r[3]) for r in bb],
            max_sync_skew_ms=500,
            dual_taker_fee_bps=tf_a + tf_b,
            maker_first_fee_bps=mf_a + tf_b,
        )
        res.update(
            {
                "leg_a_maker": va,
                "leg_b_hedge": vb,
                "symbol": canon,
                "domain": domain,
                "cross_stablecoin": pa["settlement"] != pb["settlement"],
                "settlement_pair": f"{pa['settlement']}/{pb['settlement']}",
            }
        )
        track2_basis_rows.append(res)

    funding_summary = []
    for venue in ("lighter", "lighter_robinhood", "trade_xyz", "entropy", "extended", "hyperliquid"):
        for domain in ("crypto", "rwa"):
            c_info = campaigns[domain]
            ffiles = _campaign_parquet_files(
                lakehouse_dir, "funding_and_ctx", venue, c_info["utc_date"], c_info["campaign_id"]
            )
            if not ffiles:
                continue
            f_sql = "[" + ", ".join(f"'{p}'" for p in ffiles) + "]"
            for r in con.execute(
                f"""
                SELECT market,
                       COUNT(*) AS n_obs,
                       ROUND(AVG(funding_rate_bps_f64), 4) AS mean_fr_bps,
                       ROUND(MIN(funding_rate_bps_f64), 4) AS min_fr_bps,
                       ROUND(MAX(funding_rate_bps_f64), 4) AS max_fr_bps
                FROM read_parquet({f_sql})
                WHERE funding_rate_bps_f64 IS NOT NULL
                GROUP BY 1
                HAVING COUNT(*) >= 10
                ORDER BY ABS(AVG(funding_rate_bps_f64)) DESC
                LIMIT 6
                """
            ).fetchall():
                funding_summary.append(
                    {
                        "venue": venue,
                        "domain": domain,
                        "market": r[0],
                        "n_obs": r[1],
                        "mean_fr_bps": r[2],
                        "min_fr_bps": r[3],
                        "max_fr_bps": r[4],
                    }
                )

    # === TRACK 03: 03_incentive_turnover Continuous 10h Multi-Venue Simulations ===
    track3_targets = [
        ("arcus", "SNDK", "rwa", "trade_xyz", "SNDK"),
        ("arcus", "NVDA", "rwa", "lighter_robinhood", "NVDA"),
        ("arcus", "QQQ", "rwa", "lighter_robinhood", "QQQ"),
        ("arcus", "SPY", "rwa", "lighter_robinhood", "SPY"),
        ("arcus", "BTC", "crypto", "binance", "BTC"),
        ("lighter_robinhood", "NVDA", "rwa", "arcus", "NVDA"),
        ("lighter_robinhood", "SPY", "rwa", "arcus", "SPY"),
        ("lighter_robinhood", "ETH", "crypto", "binance", "ETH"),
        ("lighter", "QQQ", "rwa", "arcus", "QQQ"),
        ("lighter", "SPY", "rwa", "arcus", "SPY"),
        ("extended", "BTC", "crypto", "binance", "BTC"),
        ("extended", "ETH", "crypto", "binance", "ETH"),
        ("trade_xyz", "SNDK", "rwa", "arcus", "SNDK"),
        ("entropy", "SNDK", "rwa", "arcus", "SNDK"),
        ("bulk", "NVDA", "rwa", "arcus", "NVDA"),
        ("bulk", "SNDK", "rwa", "arcus", "SNDK"),
    ]
    track3_rows: list[dict[str, Any]] = []
    for venue, canon, domain, ref_venue, ref_canon in track3_targets:
        b_rows, raw_cont = get_books(venue, canon, domain)
        if len(b_rows) < 200:
            track3_rows.append({"venue": venue, "symbol": canon, "skipped": f"too_few_books:{len(b_rows)}"})
            continue
        vp = VENUE_PROFILES[venue]
        raw_mkt = VENUE_SYMBOL_MAP[venue][canon]
        events: list[dict[str, Any]] = [
            {"ts": r[0], "kind": "book", "bids": [(r[1], max(r[2], 1e-6))], "asks": [(r[3], max(r[4], 1e-6))]}
            for r in b_rows
        ]
        if ref_venue in VENUE_SYMBOL_MAP and ref_canon in VENUE_SYMBOL_MAP[ref_venue]:
            r_rows, _ = get_books(ref_venue, ref_canon, domain)
            for rr in r_rows:
                events.append({"ts": rr[0], "kind": "ref", "venue": ref_venue, "bid": rr[1], "ask": rr[3]})

        if vp["has_trades"]:
            for ts_ms, agg_side, px, sz in fetch_trades(venue, raw_mkt, domain):
                s_up = str(agg_side).upper()
                side_str = "BUY" if s_up in ("BUY", "B", "BID") else "SELL"
                events.append({"ts": ts_ms, "kind": "trade", "side": side_str, "price": px, "size": sz})

        events.sort(key=lambda e: e["ts"])
        tick = _infer_tick_from_prices([r[1] for r in b_rows])
        fee = vp["maker_fee_bps_crypto"] if domain == "crypto" else vp["maker_fee_bps_equity"]
        infer_bd = not vp["has_trades"]
        arms_cfg = sim.default_arms_for_profile(vp["profile"], maker_fee_bps=fee, infer_book_diff_sweeps=infer_bd)
        arm_subset = [
            a
            for a in arms_cfg
            if a.name
            in (
                "A_baseline_back_of_queue",
                "B_queue_priority_ladder",
                "E_v3_validated_lead_qprio",
                "F_v4_adaptive_shield",
            )
        ]
        arm_results = [sim.simulate_arm(events, tick, a) for a in arm_subset]
        for res in arm_results:
            if not vp["has_trades"]:
                res["fill_evidence_mode"] = "book_diff_diagnostic"
            elif vp["bbo_cadence"] != "ws_event":
                res["fill_evidence_mode"] = "polled_bbo_diagnostic"
            else:
                res["fill_evidence_mode"] = "continuous_10h_ws_trades"
        row_out: dict[str, Any] = {
            "venue": venue,
            "symbol": canon,
            "raw_market": raw_mkt,
            "ref_venue": ref_venue,
            "campaign_id": _campaign_for(venue, domain)["campaign_id"],
            "profile": vp["profile"],
            "maker_fee_bps": fee,
            "tick_inferred": tick,
            "continuity": raw_cont,
            "fee_audit_status": vp["fee_audit_status"],
            "arms": arm_results,
        }
        if venue == "arcus":
            import dataclasses

            bbo_disabled = [
                sim.simulate_arm(events, tick, dataclasses.replace(a, disable_bbo_sweep_fills=True))
                for a in arm_subset
            ]
            for res in bbo_disabled:
                res["fill_evidence_mode"] = "public_trades_bbo_sweep_disabled"
            row_out["bbo_sweep_disabled_arms"] = bbo_disabled
        track3_rows.append(row_out)

    con.close()
    return {
        "schema": "multivenue_three_tracks_10h_v1",
        "split": split,
        "campaigns": campaigns,
        "backpack_status": {
            "in_lakehouse": False,
            "fee_audit_status": "account_evidence_required",
            "note": "No Aleph collector capture yet; see scripts/sim_backpack_dm_guardian.py for Depth-Multiplier rules (100ms non-PostOnly bump, Tier3 25bps/min $5k, Tier4 20bps/min $10k).",
        },
        "track1_quant_profit": track1_rows,
        "track2_hedge_arbitrage": {
            "pairs": track2_basis_rows,
            "top_funding_ctx": funding_summary,
        },
        "track3_incentive_turnover": track3_rows,
    }


def main() -> int:
    ap = argparse.ArgumentParser(description="Multi-venue three-track 10h Aleph lakehouse evaluator")
    ap.add_argument("--lakehouse", type=Path, default=Path(os.path.expanduser("~/aleph-tx-lakehouse")))
    ap.add_argument("--split", choices=tuple(CANONICAL_CAMPAIGNS.keys()), default="holdout")
    ap.add_argument("--output", type=Path)
    args = ap.parse_args()
    out = run_duckdb_lakehouse_eval(args.lakehouse, args.split)
    rendered = json.dumps(out, indent=2, allow_nan=False)
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(rendered + "\n", encoding="utf-8")
    else:
        print(rendered)
    return 0


if __name__ == "__main__":
    sys.exit(main())
