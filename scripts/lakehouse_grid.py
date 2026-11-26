#!/usr/bin/env python3
"""Shared read-only lakehouse helpers: receipt-causal fixed-step BBO grids and trades.

Grid semantics (causal, conservative): value at index ``i`` is the last tradable quote
whose ``available_at_ms`` is strictly before ``t0 + (i + 1) * step_ms``. A decision made
"at index i" therefore happens at wall time ``t0 + (i + 1) * step_ms`` and can only see
quotes that had already arrived. ``age_steps[i]`` counts steps since the last update so
callers can refuse to act on stale books.

Never opens ``lakehouse.duckdb`` (background pipelines may hold its lock); reads Hive
parquet partitions with an in-memory DuckDB capped at 2 GB / 2 threads.
"""

from __future__ import annotations

import glob
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Sequence

TEN_HOURS_MS = 36_000_000

# Windows: W19 = selection_dev, W24 = holdout (untouched by these new strategies before
# parameter freeze), W23 = second holdout. Campaign ids verified on aleph 2026-11-26.
WINDOWS: dict[str, dict[str, str]] = {
    "w19": {
        "crypto": "tokyo-crypto-20261123T061504Z-w19",
        "rwa": "tokyo-rwa-20261123T061452Z-w19",
        "split": "selection_dev",
    },
    "w23": {
        "crypto": "tokyo-crypto-20261124T231506Z-w23",
        "rwa": "tokyo-rwa-20261124T231453Z-w23",
        "split": "holdout_2",
    },
    "w24": {
        "crypto": "tokyo-crypto-20261125T093007Z-w24",
        "rwa": "tokyo-rwa-20261125T092953Z-w24",
        "split": "holdout",
    },
}


@dataclass
class Grid:
    t0_ms: int
    step_ms: int
    bid: Any  # numpy arrays
    ask: Any
    bid_sz: Any
    ask_sz: Any
    age_steps: Any
    raw_events: int
    max_gap_ms: int
    cadence: dict | None = None

    @property
    def n(self) -> int:
        return int(len(self.bid))

    def mid(self):
        return 0.5 * (self.bid + self.ask)


def campaign_files(lakehouse: Path, dataset: str, venue: str, campaign_id: str) -> list[str]:
    pattern = (
        Path(lakehouse)
        / "parquet"
        / dataset
        / "collector_region=tokyo"
        / f"venue={venue}"
        / "utc_date=*"
        / f"campaign_id={campaign_id}"
        / "*.parquet"
    )
    return sorted(glob.glob(str(pattern)))


def connect():
    import duckdb

    con = duckdb.connect(":memory:")
    con.execute("SET preserve_insertion_order=false; SET threads=2; SET memory_limit='2GB';")
    return con


def _sql_list(files: Sequence[str]) -> str:
    return "[" + ", ".join("'" + f.replace("'", "''") + "'" for f in files) + "]"


def load_bucket_rows(con, files: Sequence[str], market: str, step_ms: int):
    """Return (bucket, bid, ask, bid_sz, ask_sz) numpy columns plus raw stats."""
    if not files:
        return None, {"raw_events": 0, "max_gap_ms": 0}
    src = _sql_list(files)
    stats = con.execute(
        f"""
        WITH s AS (
          SELECT available_at_ms, LAG(available_at_ms) OVER (ORDER BY available_at_ms) AS p
          FROM read_parquet({src})
          WHERE market = ? AND tradable AND bid_px_f64 > 0 AND ask_px_f64 > bid_px_f64
        )
        SELECT COUNT(*), COALESCE(MAX(available_at_ms - p), 0),
               COALESCE(quantile_cont(available_at_ms - p, 0.5), 0),
               COALESCE(quantile_cont(available_at_ms - p, 0.9), 0),
               (SELECT string_agg(DISTINCT transport, ',') FROM read_parquet({src}) WHERE market = ?)
        FROM s
        """,
        [market, market],
    ).fetchone()
    cols = con.execute(
        f"""
        SELECT available_at_ms // {int(step_ms)} AS b,
               arg_max(bid_px_f64, available_at_ms) AS bid,
               arg_max(ask_px_f64, available_at_ms) AS ask,
               arg_max(CAST(bid_sz AS DOUBLE), available_at_ms) AS bsz,
               arg_max(CAST(ask_sz AS DOUBLE), available_at_ms) AS asz
        FROM read_parquet({src})
        WHERE market = ? AND tradable AND bid_px_f64 > 0 AND ask_px_f64 > bid_px_f64
        GROUP BY 1 ORDER BY 1
        """,
        [market],
    ).fetchnumpy()
    return cols, {
        "raw_events": int(stats[0] or 0),
        "max_gap_ms": int(stats[1] or 0),
        "p50_interarrival_ms": float(stats[2] or 0),
        "p90_interarrival_ms": float(stats[3] or 0),
        "transport": stats[4],
    }


def build_grid(cols, stats, t0_ms: int, t1_ms: int, step_ms: int) -> Grid | None:
    """Forward-fill bucketed last quotes onto [t0, t1) with causal age tracking."""
    import numpy as np

    if cols is None or len(cols["b"]) == 0:
        return None
    n = int((t1_ms - t0_ms) // step_ms)
    b0 = t0_ms // step_ms
    idx = np.asarray(cols["b"], dtype=np.int64) - b0
    keep = (idx >= 0) & (idx < n)
    # Quotes before t0 seed the first value (carry forward the last pre-window quote).
    pre = idx < 0
    out = {}
    for k in ("bid", "ask", "bsz", "asz"):
        arr = np.full(n, np.nan)
        src = np.asarray(cols[k], dtype=np.float64)
        arr[idx[keep]] = src[keep]
        if pre.any() and np.isnan(arr[0]):
            arr[0] = src[pre][-1]
        out[k] = arr
    updated = np.zeros(n, dtype=bool)
    updated[idx[keep]] = True
    # forward fill
    valid = ~np.isnan(out["bid"])
    pos = np.where(valid, np.arange(n), 0)
    np.maximum.accumulate(pos, out=pos)
    for k in out:
        out[k] = out[k][pos]  # leading entries map to index 0, which stays NaN if unseeded
    upos = np.where(updated, np.arange(n), -1)
    np.maximum.accumulate(upos, out=upos)
    age = np.where(upos >= 0, np.arange(n) - upos, n).astype(np.int64)
    return Grid(
        t0_ms=int(t0_ms),
        step_ms=int(step_ms),
        bid=out["bid"],
        ask=out["ask"],
        bid_sz=out["bsz"],
        ask_sz=out["asz"],
        age_steps=age,
        raw_events=stats["raw_events"],
        max_gap_ms=stats["max_gap_ms"],
        cadence={k: stats.get(k) for k in ("p50_interarrival_ms", "p90_interarrival_ms", "transport")},
    )


def campaign_span(con, files: Sequence[str], market: str) -> tuple[int, int] | None:
    if not files:
        return None
    r = con.execute(
        f"SELECT MIN(available_at_ms), MAX(available_at_ms) FROM read_parquet({_sql_list(files)}) WHERE market = ?",
        [market],
    ).fetchone()
    if r is None or r[0] is None:
        return None
    return int(r[0]), int(r[1])


def load_trades(con, files: Sequence[str], market: str, t0_ms: int, t1_ms: int):
    """Return numpy columns (ts, side_sign, px, sz) for public prints; side_sign=+1 buy aggressor."""
    import numpy as np

    if not files:
        return None
    cols = con.execute(
        f"""
        SELECT available_at_ms AS ts,
               CASE WHEN lower(aggressor_side) IN ('buy','b','bid') THEN 1
                    WHEN lower(aggressor_side) IN ('sell','s','ask') THEN -1 ELSE 0 END AS sgn,
               price_f64 AS px, size_f64 AS sz
        FROM read_parquet({_sql_list(files)})
        WHERE market = ? AND NOT is_zero_price_transfer AND price_f64 > 0 AND size_f64 > 0
          AND available_at_ms >= ? AND available_at_ms < ?
        ORDER BY available_at_ms
        """,
        [market, int(t0_ms), int(t1_ms)],
    ).fetchnumpy()
    return {k: np.asarray(v) for k, v in cols.items()}


def ema_causal(x, alpha: float):
    """ema[i] uses x[0..i-1] only (strictly causal); ema[0] = x[0]."""
    import numpy as np

    out = np.empty_like(x)
    acc = x[0]
    for i in range(len(x)):
        out[i] = acc
        xi = x[i]
        if xi == xi:  # not NaN
            acc = acc + alpha * (xi - acc) if acc == acc else xi
    return out
