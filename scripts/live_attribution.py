#!/usr/bin/env python3
"""Joint Post-Trade Attribution Engine (`telemetry.sqlite3` x Lakehouse `v_bbo_quotes`).

Performs causal `ASOF JOIN` alignment between private live session fills/orders
and public multi-venue BBO recordings to answer three institutional diagnostics:
  1. Cross-venue lead/lag at fill time (`available_at_ms <= fill_alignment_ms`):
     how many bps Binance, Arcus, Entropy, TradeXYZ, Lighter Robinhood, or
     Hyperliquid led our fill price and local DEX book when we got hit.
  2. Multi-horizon realized markout (`+100ms`, `+500ms`, `+1s`, `+5s`, `+30s`):
     signed post-fill mid-price drift (`available_at_ms >= fill_alignment_ms + h`)
     separating toxic informed flow from benign noise turnover.
  3. Resting queue lifetime (`fill.ts_ms - order.created_ms`) and paired hedge
     leg slippage/fee summary, keeping source, receipt, and telemetry clocks
     distinct and preserving unrated fees as `None` (`AGENTS.md` invariants).
"""

from __future__ import annotations

import argparse
from bisect import bisect_left, bisect_right
import gzip
import json
import math
from pathlib import Path
import shutil
import sqlite3
import subprocess
from tempfile import TemporaryDirectory

try:
    import duckdb as _duckdb_mod  # type: ignore
except ModuleNotFoundError:
    _duckdb_mod = None


DEFAULT_HORIZONS_MS = (100, 500, 1_000, 5_000, 30_000)
RATED_FEE_SOURCES = ("rate_derived", "explicit_zero")


def _open_sqlite_readonly(db_or_gz: Path) -> tuple[sqlite3.Connection, TemporaryDirectory | None]:
    if db_or_gz.is_symlink() or not db_or_gz.is_file():
        raise ValueError(f"SQLite path invalid or symlink: {db_or_gz}")
    if db_or_gz.suffix == ".gz":
        tmp = TemporaryDirectory()
        extracted = Path(tmp.name) / "telemetry.sqlite3"
        with gzip.open(db_or_gz, "rb") as src, extracted.open("wb") as dst:
            shutil.copyfileobj(src, dst)
        conn = sqlite3.connect(f"file:{extracted}?mode=ro", uri=True)
        conn.row_factory = sqlite3.Row
        return conn, tmp
    conn = sqlite3.connect(f"file:{db_or_gz.resolve()}?mode=ro", uri=True)
    conn.row_factory = sqlite3.Row
    return conn, None


def load_session_telemetry(db_path: Path, *, session_id: str | None = None) -> dict:
    """Load session metadata, fills, order creation times, and hedge fills."""
    conn, tmp = _open_sqlite_readonly(db_path)
    try:
        if session_id is None:
            row = conn.execute(
                "SELECT * FROM sessions ORDER BY started_ms DESC, session_id DESC LIMIT 1"
            ).fetchone()
        else:
            row = conn.execute(
                "SELECT * FROM sessions WHERE session_id = ?", (session_id,)
            ).fetchone()
        if row is None:
            raise ValueError("no matching session found in telemetry database")
        sess = dict(row)
        sid = sess["session_id"]

        fills = [
            dict(r)
            for r in conn.execute(
                "SELECT * FROM fills WHERE session_id = ? ORDER BY ts_ms ASC, offset ASC",
                (sid,),
            ).fetchall()
        ]
        orders = {
            int(r["client_order_id"]): dict(r)
            for r in conn.execute(
                "SELECT * FROM orders WHERE session_id = ?", (sid,)
            ).fetchall()
        }
        hedge_fills = [
            dict(r)
            for r in conn.execute(
                "SELECT * FROM hedge_fills WHERE session_id = ? ORDER BY ts_ms ASC, offset ASC",
                (sid,),
            ).fetchall()
        ]
        return {
            "session": sess,
            "fills": fills,
            "orders": orders,
            "hedge_fills": hedge_fills,
        }
    finally:
        conn.close()
        if tmp is not None:
            tmp.cleanup()


def load_bbo_series_from_ndjson(bbo_ndjson_path: Path) -> dict[str, tuple[list[int], list[dict]]]:
    """Load tradable BBO series keyed by `venue:domain:market`, sorted by `available_at_ms`."""
    opener = gzip.open if bbo_ndjson_path.suffix == ".gz" else open
    grouped: dict[str, list[dict]] = {}
    with opener(bbo_ndjson_path, "rt", encoding="utf-8") as handle:
        for line in handle:
            if not line.strip():
                continue
            row = json.loads(line)
            if not row.get("tradable"):
                continue
            mid = row.get("mid_px_f64")
            avail = row.get("available_at_ms")
            if mid is None or not math.isfinite(mid) or mid <= 0 or type(avail) is not int:
                continue
            key = f"{row['venue']}:{row['domain']}:{row['market']}"
            grouped.setdefault(key, []).append(
                {
                    "available_at_ms": avail,
                    "sequence": int(row.get("sequence", 0)),
                    "source_time_ms": row.get("source_time_ms"),
                    "bid_px_f64": float(row["bid_px_f64"]),
                    "ask_px_f64": float(row["ask_px_f64"]),
                    "mid_px_f64": float(mid),
                    "spread_bps_f64": float(row["spread_bps_f64"]),
                }
            )
    indexed: dict[str, tuple[list[int], list[dict]]] = {}
    for key, rows in grouped.items():
        rows.sort(key=lambda r: (r["available_at_ms"], r["sequence"]))
        indexed[key] = ([r["available_at_ms"] for r in rows], rows)
    return indexed


def generate_duckdb_asof_sql(
    *,
    fills_json_path: Path,
    bbo_source_expr: str,
    venue_key: str,
    max_quote_age_ms: int,
    horizons_ms: tuple[int, ...] = DEFAULT_HORIZONS_MS,
) -> str:
    """Generate exact DuckDB `ASOF JOIN` SQL for pre-fill lead/lag and post-fill markouts."""
    venue, domain, market = venue_key.split(":", 2)
    esc = lambda s: s.replace("'", "''")
    fills_esc = esc(str(fills_json_path.resolve()))

    horizon_joins = []
    horizon_selects = []
    for h in horizons_ms:
        alias = f"q_h{h}"
        horizon_selects.append(
            f"""CASE
                WHEN {alias}.available_at_ms IS NOT NULL
                     AND {alias}.available_at_ms <= f.fill_alignment_ms + {h + max_quote_age_ms}
                THEN f.sign * (({alias}.mid_px_f64 - f.fill_price) / f.fill_price) * 10000.0
                ELSE NULL
            END AS markout_{h}ms_bps"""
        )
        horizon_joins.append(
            f"""ASOF LEFT JOIN quotes AS {alias}
              ON {alias}.available_at_ms >= f.fill_alignment_ms + {h}"""
        )

    joins_sql = "\n".join(horizon_joins)
    selects_sql = ",\n        ".join(horizon_selects)

    return f"""WITH quotes AS (
    SELECT
        CAST(available_at_ms AS BIGINT) AS available_at_ms,
        CAST(sequence AS BIGINT) AS sequence,
        CAST(source_time_ms AS BIGINT) AS source_time_ms,
        CAST(bid_px_f64 AS DOUBLE) AS bid_px_f64,
        CAST(ask_px_f64 AS DOUBLE) AS ask_px_f64,
        CAST(mid_px_f64 AS DOUBLE) AS mid_px_f64
    FROM {bbo_source_expr}
    WHERE tradable = true
      AND venue = '{esc(venue)}'
      AND domain = '{esc(domain)}'
      AND market = '{esc(market)}'
    ORDER BY available_at_ms ASC, sequence ASC
),
fills_in AS (
    SELECT
        CAST(fill_idx AS INTEGER) AS fill_idx,
        CAST(fill_alignment_ms AS BIGINT) AS fill_alignment_ms,
        CAST(sign AS DOUBLE) AS sign,
        CAST(fill_price AS DOUBLE) AS fill_price
    FROM read_json('{fills_esc}', format='newline_delimited', columns={{
        fill_idx: 'INTEGER',
        fill_alignment_ms: 'BIGINT',
        sign: 'DOUBLE',
        fill_price: 'DOUBLE'
    }})
)
SELECT
    f.fill_idx,
    CASE
        WHEN q0.available_at_ms IS NOT NULL
             AND f.fill_alignment_ms - q0.available_at_ms <= {max_quote_age_ms}
        THEN q0.mid_px_f64
        ELSE NULL
    END AS at_fill_mid,
    CASE
        WHEN q0.available_at_ms IS NOT NULL
             AND f.fill_alignment_ms - q0.available_at_ms <= {max_quote_age_ms}
        THEN f.fill_alignment_ms - q0.available_at_ms
        ELSE NULL
    END AS at_fill_age_ms,
    CASE
        WHEN q0.available_at_ms IS NOT NULL
             AND f.fill_alignment_ms - q0.available_at_ms <= {max_quote_age_ms}
        THEN f.sign * ((q0.mid_px_f64 - f.fill_price) / f.fill_price) * 10000.0
        ELSE NULL
    END AS vs_fill_bps,
    {selects_sql}
FROM fills_in AS f
ASOF LEFT JOIN quotes AS q0
  ON q0.available_at_ms <= f.fill_alignment_ms
{joins_sql}
ORDER BY f.fill_idx ASC;
"""


def _run_duckdb_query_json(sql: str, *, db_path: Path | None = None) -> list[dict]:
    if _duckdb_mod is not None:
        conn = _duckdb_mod.connect(str(db_path) if db_path else ":memory:", read_only=bool(db_path))
        try:
            cur = conn.execute(sql)
            cols = [d[0] for d in cur.description]
            return [dict(zip(cols, row)) for row in cur.fetchall()]
        finally:
            conn.close()
    duck_bin = shutil.which("duckdb") or str(Path.home() / ".local" / "bin" / "duckdb")
    if not Path(duck_bin).is_file():
        raise RuntimeError("neither python duckdb module nor duckdb CLI binary is installed")
    cmd = [duck_bin, "-json"]
    if db_path is not None:
        cmd.extend(["-readonly", str(db_path)])
    cmd.extend(["-c", sql])
    proc = subprocess.run(cmd, capture_output=True, text=True, check=True)
    out = proc.stdout.strip()
    return json.loads(out) if out else []


def _asof_at_or_before(
    times: list[int], rows: list[dict], target_ms: int, max_age_ms: int
) -> dict | None:
    idx = bisect_right(times, target_ms) - 1
    if idx < 0:
        return None
    row = rows[idx]
    if target_ms - row["available_at_ms"] > max_age_ms:
        return None
    return row


def _asof_at_or_after(
    times: list[int], rows: list[dict], target_ms: int, max_window_ms: int
) -> dict | None:
    idx = bisect_left(times, target_ms)
    if idx >= len(rows):
        return None
    row = rows[idx]
    if row["available_at_ms"] > target_ms + max_window_ms:
        return None
    return row


def _mean_or_none(values: list[float]) -> float | None:
    return sum(values) / len(values) if values else None


def attribute_session(
    telemetry: dict,
    *,
    bbo_ndjson_path: Path | None = None,
    duckdb_path: Path | None = None,
    parquet_bbo_glob: str | None = None,
    local_market_key: str,
    reference_market_keys: list[str],
    max_quote_age_ms: int = 5_000,
    horizons_ms: tuple[int, ...] = DEFAULT_HORIZONS_MS,
    prefer_duckdb: bool = True,
) -> dict:
    """Attribute every fill in `telemetry` against local and external BBO streams."""
    session = telemetry["session"]
    fills = telemetry["fills"]
    orders = telemetry["orders"]
    hedge_fills = telemetry["hedge_fills"]

    all_keys = [local_market_key, *[k for k in reference_market_keys if k != local_market_key]]
    fill_inputs = []
    for idx, f in enumerate(fills):
        exch_ns = f.get("exchange_ts_ns")
        has_exch_ns = isinstance(exch_ns, int) and exch_ns > 0
        align_ms = (exch_ns // 1_000_000) if has_exch_ns else int(f["ts_ms"])
        sign = 1.0 if f["side"] == "buy" else -1.0
        fill_inputs.append(
            {
                "fill_idx": idx,
                "fill_alignment_ms": align_ms,
                "alignment_clock": "exchange_ts_ns" if has_exch_ns else "telemetry_ts_ms",
                "sign": sign,
                "fill_price": float(f["price"]),
            }
        )

    use_duck = False
    bbo_expr = ""
    if prefer_duckdb and (_duckdb_mod is not None or shutil.which("duckdb") or (Path.home() / ".local" / "bin" / "duckdb").is_file()):
        if duckdb_path is not None:
            use_duck = True
            bbo_expr = "v_bbo_quotes"
        elif parquet_bbo_glob is not None:
            use_duck = True
            esc_glob = parquet_bbo_glob.replace("'", "''")
            bbo_expr = f"read_parquet('{esc_glob}', hive_partitioning=true)"
        elif bbo_ndjson_path is not None:
            use_duck = True
            esc_nd = str(bbo_ndjson_path.resolve()).replace("'", "''")
            bbo_expr = (
                f"read_json('{esc_nd}', format='newline_delimited', columns={{"
                "venue:'VARCHAR', domain:'VARCHAR', market:'VARCHAR', sequence:'BIGINT', "
                "available_at_ms:'BIGINT', source_time_ms:'BIGINT', bid_px_f64:'DOUBLE', "
                "ask_px_f64:'DOUBLE', mid_px_f64:'DOUBLE', tradable:'BOOLEAN'})"
            )

    per_venue_metrics: dict[str, list[dict]] = {}
    if use_duck and fill_inputs:
        with TemporaryDirectory() as tmp:
            fills_nd = Path(tmp) / "fills_in.ndjson"
            with fills_nd.open("w", encoding="utf-8") as fh:
                for item in fill_inputs:
                    fh.write(json.dumps(item) + "\n")
            for key in all_keys:
                sql = generate_duckdb_asof_sql(
                    fills_json_path=fills_nd,
                    bbo_source_expr=bbo_expr,
                    venue_key=key,
                    max_quote_age_ms=max_quote_age_ms,
                    horizons_ms=horizons_ms,
                )
                per_venue_metrics[key] = _run_duckdb_query_json(sql, db_path=duckdb_path)
    else:
        if bbo_ndjson_path is None:
            raise ValueError("bbo_ndjson_path is required when DuckDB is unavailable")
        series_map = load_bbo_series_from_ndjson(bbo_ndjson_path)
        for key in all_keys:
            times, rows = series_map.get(key, ([], []))
            venue_rows = []
            for item in fill_inputs:
                align_ms = item["fill_alignment_ms"]
                sign = item["sign"]
                px = item["fill_price"]
                q0 = _asof_at_or_before(times, rows, align_ms, max_quote_age_ms)
                rec: dict = {
                    "fill_idx": item["fill_idx"],
                    "at_fill_mid": q0["mid_px_f64"] if q0 else None,
                    "at_fill_age_ms": (align_ms - q0["available_at_ms"]) if q0 else None,
                    "vs_fill_bps": (
                        sign * ((q0["mid_px_f64"] - px) / px) * 10_000.0 if q0 else None
                    ),
                }
                for h in horizons_ms:
                    qh = _asof_at_or_after(times, rows, align_ms + h, max_quote_age_ms)
                    rec[f"markout_{h}ms_bps"] = (
                        sign * ((qh["mid_px_f64"] - px) / px) * 10_000.0 if qh else None
                    )
                venue_rows.append(rec)
            per_venue_metrics[key] = venue_rows

    primary_ref_key = reference_market_keys[0] if reference_market_keys else local_market_key
    fill_reports = []
    sniped_count = 0
    toxic_500ms_count = 0
    rated_fee_bps_list: list[float] = []
    resting_lifetimes_ms: list[int] = []

    for idx, f in enumerate(fills):
        meta = fill_inputs[idx]
        coid = f.get("client_order_id")
        order_row = orders.get(int(coid)) if isinstance(coid, int) else None
        created_ms = order_row.get("created_ms") if order_row else None
        resting_ms = (
            int(f["ts_ms"]) - int(created_ms)
            if isinstance(created_ms, int) and int(f["ts_ms"]) >= int(created_ms)
            else None
        )
        if resting_ms is not None:
            resting_lifetimes_ms.append(resting_ms)
            if resting_ms < 250:
                sniped_count += 1

        notional = float(f["notional"])
        fee_source = f.get("fee_source")
        fee_bps = (
            (float(f["fee"]) / notional) * 10_000.0
            if fee_source in RATED_FEE_SOURCES and notional > 0
            else None
        )
        if fee_bps is not None:
            rated_fee_bps_list.append(fee_bps)

        local_m = per_venue_metrics[local_market_key][idx] if fill_inputs else {}
        local_mid = local_m.get("at_fill_mid")

        refs_out = {}
        for ref_key in reference_market_keys:
            rm = per_venue_metrics[ref_key][idx]
            ref_mid = rm.get("at_fill_mid")
            lead_vs_local = (
                meta["sign"] * ((ref_mid - local_mid) / local_mid) * 10_000.0
                if (ref_mid is not None and local_mid is not None and local_mid > 0)
                else None
            )
            refs_out[ref_key] = {
                "at_fill_mid": ref_mid,
                "at_fill_age_ms": rm.get("at_fill_age_ms"),
                "vs_fill_bps": rm.get("vs_fill_bps"),
                "lead_vs_local_bps": lead_vs_local,
                "markouts_bps": {
                    f"{h}ms": rm.get(f"markout_{h}ms_bps") for h in horizons_ms
                },
            }

        primary_m = per_venue_metrics[primary_ref_key][idx]
        m500 = primary_m.get("markout_500ms_bps")
        if m500 is not None and m500 < -2.0:
            toxic_500ms_count += 1

        local_markouts = {f"{h}ms": local_m.get(f"markout_{h}ms_bps") for h in horizons_ms}
        net_primary_markouts = {
            f"{h}ms": (
                primary_m.get(f"markout_{h}ms_bps") - fee_bps
                if (primary_m.get(f"markout_{h}ms_bps") is not None and fee_bps is not None)
                else None
            )
            for h in horizons_ms
        }

        fill_reports.append(
            {
                "trade_id": f["trade_id"],
                "client_order_id": coid,
                "side": f["side"],
                "price": float(f["price"]),
                "size": float(f["size"]),
                "notional": notional,
                "telemetry_ts_ms": int(f["ts_ms"]),
                "exchange_ts_ns": f.get("exchange_ts_ns"),
                "fill_alignment_ms": meta["fill_alignment_ms"],
                "alignment_clock": meta["alignment_clock"],
                "resting_lifetime_ms": resting_ms,
                "fee_source": fee_source,
                "rated_fee_bps": fee_bps,
                "local_book": {
                    "market_key": local_market_key,
                    "at_fill_mid": local_mid,
                    "at_fill_age_ms": local_m.get("at_fill_age_ms"),
                    "vs_fill_bps": local_m.get("vs_fill_bps"),
                    "markouts_bps": local_markouts,
                },
                "references": refs_out,
                "net_primary_markouts_after_fee_bps": net_primary_markouts,
            }
        )

    ref_summaries = {}
    for ref_key in reference_market_keys:
        vs_fill_vals = [
            r["references"][ref_key]["vs_fill_bps"]
            for r in fill_reports
            if r["references"][ref_key]["vs_fill_bps"] is not None
        ]
        lead_vals = [
            r["references"][ref_key]["lead_vs_local_bps"]
            for r in fill_reports
            if r["references"][ref_key]["lead_vs_local_bps"] is not None
        ]
        horizon_means = {}
        for h in horizons_ms:
            vals = [
                r["references"][ref_key]["markouts_bps"][f"{h}ms"]
                for r in fill_reports
                if r["references"][ref_key]["markouts_bps"][f"{h}ms"] is not None
            ]
            horizon_means[f"{h}ms"] = _mean_or_none(vals)
        ref_summaries[ref_key] = {
            "matched_fills": len(vs_fill_vals),
            "mean_vs_fill_bps": _mean_or_none(vs_fill_vals),
            "mean_lead_vs_local_bps": _mean_or_none(lead_vals),
            "mean_markouts_bps": horizon_means,
        }

    hedge_reported = sum(1 for hf in hedge_fills if hf.get("price_source") == "fill")
    hedge_limit = sum(1 for hf in hedge_fills if hf.get("price_source") == "limit")

    return {
        "schema_version": 1,
        "engine": "duckdb_asof" if use_duck else "python_causal_bisect",
        "session_id": session["session_id"],
        "exchange": session.get("exchange"),
        "symbol": session.get("symbol"),
        "fill_count": len(fill_reports),
        "fast_sniped_fills_lt_250ms": sniped_count,
        "toxic_fills_500ms_lt_minus_2bps": toxic_500ms_count,
        "mean_resting_lifetime_ms": _mean_or_none([float(x) for x in resting_lifetimes_ms]),
        "all_fees_rated": len(rated_fee_bps_list) == len(fill_reports),
        "mean_rated_fee_bps": _mean_or_none(rated_fee_bps_list)
        if len(rated_fee_bps_list) == len(fill_reports)
        else None,
        "hedge_leg": {
            "hedge_fill_count": len(hedge_fills),
            "reported_price_fills": hedge_reported,
            "limit_bound_fills": hedge_limit,
        },
        "reference_summary": ref_summaries,
        "fills": fill_reports,
    }


def render_markdown_summary(report: dict) -> str:
    """Format a concise GitHub-flavored Markdown attribution table."""
    lines = [
        f"# Live-vs-Lakehouse Post-Trade Attribution (`{report['session_id']}`)",
        "",
        f"- **Engine**: `{report['engine']}`",
        f"- **Symbol / Venue**: `{report.get('symbol')}` on `{report.get('exchange')}`",
        f"- **Total Fills**: `{report['fill_count']}` (fast `<250ms` sniped: `{report['fast_sniped_fills_lt_250ms']}`, `500ms < -2bps` toxic: `{report['toxic_fills_500ms_lt_minus_2bps']}`)",
        "",
        "## Cross-Venue Lead/Lag & Realized Markouts",
        "",
        "| Reference Market | Matched | At-Fill Edge (bps) | Lead vs Local (bps) | +100ms (bps) | +500ms (bps) | +5s (bps) |",
        "| --- | ---: | ---: | ---: | ---: | ---: | ---: |",
    ]
    fmt = lambda v: "n/a" if v is None else f"{v:+.2f}"
    for key, s in report["reference_summary"].items():
        m = s["mean_markouts_bps"]
        lines.append(
            f"| `{key}` | {s['matched_fills']} | {fmt(s['mean_vs_fill_bps'])} | {fmt(s['mean_lead_vs_local_bps'])} | {fmt(m.get('100ms'))} | {fmt(m.get('500ms'))} | {fmt(m.get('5000ms'))} |"
        )
    return "\n".join(lines) + "\n"


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--telemetry-db", type=Path, required=True, help="Path to telemetry.sqlite3 or .sqlite3.gz")
    parser.add_argument("--session-id", default=None, help="Specific session_id (defaults to latest)")
    parser.add_argument("--duckdb", type=Path, default=None, help="Path to lakehouse.duckdb containing v_bbo_quotes")
    parser.add_argument("--parquet-bbo-glob", default=None, help="Optional Parquet glob for bbo_quotes")
    parser.add_argument("--bbo-ndjson", type=Path, default=None, help="Optional Silver staging bbo_quotes.ndjson.gz")
    parser.add_argument("--local-market", required=True, help="venue:domain:market, e.g. lighter:core:0")
    parser.add_argument("--reference-markets", nargs="+", required=True, help="Reference venue:domain:market keys")
    parser.add_argument("--max-quote-age-ms", type=int, default=5_000)
    parser.add_argument("--markdown-out", type=Path, default=None)
    args = parser.parse_args()

    telemetry = load_session_telemetry(args.telemetry_db, session_id=args.session_id)
    report = attribute_session(
        telemetry,
        bbo_ndjson_path=args.bbo_ndjson,
        duckdb_path=args.duckdb,
        parquet_bbo_glob=args.parquet_bbo_glob,
        local_market_key=args.local_market,
        reference_market_keys=args.reference_markets,
        max_quote_age_ms=args.max_quote_age_ms,
    )
    if args.markdown_out is not None:
        args.markdown_out.write_text(render_markdown_summary(report), encoding="utf-8")
    print(json.dumps(report, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
