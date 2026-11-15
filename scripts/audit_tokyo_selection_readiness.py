#!/usr/bin/env python3
"""Audit Tokyo (`asia-northeast1`) capture readiness: cross-domain latency, zeroPrice transfers, and Small-Clamp carry.

Accepts either a sealed `archive_public_capture` directory (containing `manifest.json`)
or a raw `.ndjson` / `.ndjson.gz` file recorded by `perp-capture`.

Outputs aggregate diagnostics only (no private account or order data):
1. Per-source `receipt_minus_source_ms` quantiles (p50/p95/p99), negative clock-skew counts,
   disconnect counts, and `<10ms` / `<25ms` / `>50ms` execution regime classification
   (directly comparing `lighter` Core vs `lighter_robinhood` RH sequencer latency).
2. Public trade tape hygiene on `lighter_trades` and `lighter_robinhood_trades`, isolating
   `price == 0` LLP `zeroPrice` liquidation transfers from regular execution flow.
3. Matched-symbol hourly funding spread screener (`bps / hour` and `bps / margin-hour`)
   comparing Lighter (`current_funding_rate` in %/hour, `raw_rate_scale = 0.01`) against
   Hyperliquid (`activeAssetCtx.funding` in fraction/hour, `raw_rate_scale = 1.0`).
"""
from __future__ import annotations

import argparse
from decimal import Decimal, InvalidOperation
import gzip
import json
import math
import os
from pathlib import Path
from tempfile import NamedTemporaryFile

from archive_public_capture import digest, records


LIGHTER_RATE_TO_BPS = Decimal("100")       # 1% / hour = 100 bps / hour
HYPERLIQUID_RATE_TO_BPS = Decimal("10000") # 1.0 / hour = 10000 bps / hour


def _iter_raw_lines(target: Path):
    if target.is_dir():
        for line in records(target):
            yield line
        return
    opener = gzip.open if target.name.endswith(".gz") else open
    with opener(target, "rt", encoding="utf-8") as handle:
        for line in handle:
            if line.strip():
                yield line


def _quantile(sorted_vals: list[float], q: float) -> float | None:
    if not sorted_vals:
        return None
    if len(sorted_vals) == 1:
        return round(sorted_vals[0], 3)
    idx = (len(sorted_vals) - 1) * q
    lo = math.floor(idx)
    hi = math.ceil(idx)
    if lo == hi:
        return round(sorted_vals[lo], 3)
    val = sorted_vals[lo] + (sorted_vals[hi] - sorted_vals[lo]) * (idx - lo)
    return round(val, 3)


def _classify_latency_regime(p50_ms: float | None, p95_ms: float | None) -> str:
    if p50_ms is None or p95_ms is None:
        return "insufficient_samples"
    if p50_ms <= 10.0 and p95_ms <= 25.0:
        return "sub_10ms_colocated_ready"
    if p50_ms <= 25.0 and p95_ms <= 60.0:
        return "regional_10_to_25ms"
    return "remote_sequencer_over_25ms"


def _extract_source_ms(source: str, payload: dict) -> float | None:
    if source in ("lighter", "lighter_robinhood"):
        ts = payload.get("timestamp")
        if type(ts) is int and ts > 0:
            return float(ts)
        ob = payload.get("order_book")
        if isinstance(ob, dict):
            lu = ob.get("last_updated_at")
            if type(lu) is int and lu > 0:
                return lu / 1000.0
    elif source in ("lighter_trades", "lighter_robinhood_trades"):
        if payload.get("type") == "update/trade":
            trades = payload.get("trades") or []
            exec_times = [
                t["transaction_time"] / 1000.0
                for t in trades
                if isinstance(t, dict)
                and type(t.get("transaction_time")) is int
                and t["transaction_time"] > 0
            ]
            if exec_times:
                return max(exec_times)
    elif source == "binance":
        data = payload.get("data") if isinstance(payload.get("data"), dict) else payload
        for key in ("E", "T"):
            ts = data.get(key)
            if type(ts) is int and ts > 0:
                return float(ts)
    elif source == "hyperliquid":
        if payload.get("channel") == "l2Book" and isinstance(payload.get("data"), dict):
            ts = payload["data"].get("time")
            if type(ts) is int and ts > 0:
                return float(ts)
    elif source == "arcus":
        if payload.get("channel") == "l2Orderbook" and isinstance(payload.get("contents"), dict):
            ts = payload["contents"].get("timestamp")
            if type(ts) is int and ts > 0:
                return ts / 1000.0
    elif source == "arcus_trades":
        contents = payload.get("contents")
        if isinstance(contents, list) and contents:
            stamps = [
                t["timestamp"] / 1000.0
                for t in contents
                if isinstance(t, dict) and type(t.get("timestamp")) is int and t["timestamp"] > 0
            ]
            if stamps:
                return max(stamps)
    return None


def _parse_decimal(raw) -> Decimal | None:
    if raw is None or isinstance(raw, bool):
        return None
    try:
        val = Decimal(str(raw))
    except InvalidOperation:
        return None
    return val if val.is_finite() else None


def audit_readiness(
    target: Path,
    *,
    lighter_leverage: Decimal = Decimal("5"),
    hedge_leverage: Decimal = Decimal("5"),
) -> dict:
    if lighter_leverage <= 0 or hedge_leverage <= 0:
        raise ValueError("leverages must be positive")
    target = target.resolve(strict=True)
    manifest_sha256 = (
        digest(target / "manifest.json")
        if target.is_dir() and (target / "manifest.json").is_file()
        else None
    )

    latencies: dict[str, list[float]] = {}
    source_counts: dict[str, dict[str, int]] = {}
    zero_price_tape: dict[str, dict[str, int]] = {
        "lighter_trades": {"live_trades": 0, "zero_price_liquidations": 0},
        "lighter_robinhood_trades": {"live_trades": 0, "zero_price_liquidations": 0},
    }
    seen_trade_ids: dict[str, set[str]] = {
        "lighter_trades": set(),
        "lighter_robinhood_trades": set(),
    }

    # Latest and cumulative funding rate observations (in bps / hour) keyed by (venue, symbol)
    funding_obs: dict[tuple[str, str], list[Decimal]] = {}
    first_receipt_ms: int | None = None
    last_receipt_ms: int | None = None

    for raw in _iter_raw_lines(target):
        row = json.loads(raw)
        source = row.get("source")
        event = row.get("event")
        receipt = row.get("received_at_ms")
        if not isinstance(source, str) or type(receipt) is not int or receipt <= 0:
            continue
        if source == "collector":
            continue

        first_receipt_ms = receipt if first_receipt_ms is None else min(first_receipt_ms, receipt)
        last_receipt_ms = receipt if last_receipt_ms is None else max(last_receipt_ms, receipt)

        counters = source_counts.setdefault(
            source,
            {"messages": 0, "disconnects": 0, "clock_ahead_samples": 0, "latency_samples": 0},
        )
        if event in ("disconnect", "clock_refresh"):
            counters["disconnects"] += 1
            continue
        if event not in ("message", "subscribed"):
            continue
        counters["messages"] += 1

        payload = row.get("payload")
        if not isinstance(payload, dict):
            continue

        src_ms = _extract_source_ms(source, payload)
        if src_ms is not None:
            delta_ms = receipt - src_ms
            if delta_ms < 0:
                counters["clock_ahead_samples"] += 1
            else:
                counters["latency_samples"] += 1
                latencies.setdefault(source, []).append(delta_ms)

        # Lighter / Robinhood zeroPrice transfer accounting
        if source in zero_price_tape and payload.get("type") == "update/trade":
            seen = seen_trade_ids[source]
            bucket = zero_price_tape[source]
            for trade in payload.get("trades") or []:
                if not isinstance(trade, dict):
                    continue
                tid = str(trade.get("trade_id", ""))
                if not tid or tid in seen:
                    continue
                seen.add(tid)
                px = _parse_decimal(trade.get("price"))
                if px is None:
                    continue
                if px == 0:
                    bucket["zero_price_liquidations"] += 1
                elif px > 0:
                    bucket["live_trades"] += 1

        # Funding observations
        if source in ("lighter", "lighter_robinhood") and payload.get("type") in (
            "subscribed/market_stats",
            "update/market_stats",
        ):
            ms = payload.get("market_stats")
            if isinstance(ms, dict):
                sym = ms.get("symbol")
                rate_pct = _parse_decimal(ms.get("current_funding_rate"))
                if isinstance(sym, str) and sym and rate_pct is not None:
                    funding_obs.setdefault((source, sym), []).append(
                        rate_pct * LIGHTER_RATE_TO_BPS
                    )
        elif source == "hyperliquid" and payload.get("channel") == "activeAssetCtx":
            data = payload.get("data")
            if isinstance(data, dict) and isinstance(data.get("ctx"), dict):
                coin = data.get("coin")
                rate_frac = _parse_decimal(data["ctx"].get("funding"))
                if isinstance(coin, str) and coin and rate_frac is not None:
                    funding_obs.setdefault(("hyperliquid", coin), []).append(
                        rate_frac * HYPERLIQUID_RATE_TO_BPS
                    )

    source_summaries = {}
    for source in sorted(source_counts):
        vals = sorted(latencies.get(source, []))
        p50 = _quantile(vals, 0.50)
        p95 = _quantile(vals, 0.95)
        p99 = _quantile(vals, 0.99)
        source_summaries[source] = {
            **source_counts[source],
            "p50_ms": p50,
            "p95_ms": p95,
            "p99_ms": p99,
            "min_ms": round(vals[0], 3) if vals else None,
            "max_ms": round(vals[-1], 3) if vals else None,
            "regime": _classify_latency_regime(p50, p95),
        }

    # Compare Lighter Core vs Lighter Robinhood sequencer latency directly if both exist
    core_p50 = source_summaries.get("lighter", {}).get("p50_ms")
    rh_p50 = source_summaries.get("lighter_robinhood", {}).get("p50_ms")
    domain_latency_delta_ms = (
        round(rh_p50 - core_p50, 3)
        if core_p50 is not None and rh_p50 is not None
        else None
    )

    # Screen matched funding pairs against Hyperliquid
    margin_per_notional = (Decimal("1") / lighter_leverage) + (Decimal("1") / hedge_leverage)
    carry_candidates = []
    hl_symbols = {sym for (venue, sym) in funding_obs if venue == "hyperliquid"}
    for lighter_venue in ("lighter", "lighter_robinhood"):
        for sym in sorted(hl_symbols):
            lt_samples = funding_obs.get((lighter_venue, sym))
            hl_samples = funding_obs.get(("hyperliquid", sym))
            if not lt_samples or not hl_samples:
                continue
            lt_mean_bps = sum(lt_samples) / Decimal(len(lt_samples))
            hl_mean_bps = sum(hl_samples) / Decimal(len(hl_samples))
            short_lighter_spread_bps = lt_mean_bps - hl_mean_bps
            direction = (
                "short_lighter_long_hyperliquid"
                if short_lighter_spread_bps >= 0
                else "long_lighter_short_hyperliquid"
            )
            gross_notional_bps_hr = abs(short_lighter_spread_bps)
            carry_bps_margin_hr = gross_notional_bps_hr / margin_per_notional
            carry_candidates.append(
                {
                    "lighter_domain": lighter_venue,
                    "symbol": sym,
                    "direction": direction,
                    "lighter_mean_hourly_bps": str(round(lt_mean_bps, 4)),
                    "hyperliquid_mean_hourly_bps": str(round(hl_mean_bps, 4)),
                    "gross_spread_bps_per_notional_hr": str(round(gross_notional_bps_hr, 4)),
                    "gross_carry_bps_per_margin_hr": str(round(carry_bps_margin_hr, 4)),
                    "lighter_samples": len(lt_samples),
                    "hyperliquid_samples": len(hl_samples),
                }
            )
    carry_candidates.sort(
        key=lambda row: Decimal(row["gross_carry_bps_per_margin_hr"]),
        reverse=True,
    )

    duration_hours = (
        round((last_receipt_ms - first_receipt_ms) / 3_600_000.0, 4)
        if first_receipt_ms is not None and last_receipt_ms is not None
        else 0.0
    )

    return {
        "schema": "tokyo_selection_readiness_v1",
        "manifest_sha256": manifest_sha256,
        "window": {
            "first_receipt_ms": first_receipt_ms,
            "last_receipt_ms": last_receipt_ms,
            "duration_hours": duration_hours,
            "continuous_ten_hour_unit_satisfied": duration_hours >= 10.0,
        },
        "domain_comparison": {
            "lighter_core_p50_ms": core_p50,
            "lighter_robinhood_p50_ms": rh_p50,
            "robinhood_minus_core_p50_ms": domain_latency_delta_ms,
        },
        "sources": source_summaries,
        "zero_price_transfers": zero_price_tape,
        "small_clamp_carry_candidates": carry_candidates,
    }


def write_report(report: dict, output: Path) -> None:
    output.parent.mkdir(parents=True, exist_ok=True)
    with NamedTemporaryFile("w", dir=output.parent, delete=False, encoding="utf-8") as tmp:
        json.dump(report, tmp, indent=2, sort_keys=True)
        tmp.write("\n")
        tmp.flush()
        os.fsync(tmp.fileno())
    os.replace(tmp.name, output)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("capture", type=Path, help="Archive directory or .ndjson[.gz] file")
    parser.add_argument("--output", type=Path, help="Optional JSON output path")
    parser.add_argument("--lighter-leverage", type=Decimal, default=Decimal("5"))
    parser.add_argument("--hedge-leverage", type=Decimal, default=Decimal("5"))
    args = parser.parse_args()

    report = audit_readiness(
        args.capture,
        lighter_leverage=args.lighter_leverage,
        hedge_leverage=args.hedge_leverage,
    )
    if args.output:
        write_report(report, args.output)
    else:
        print(json.dumps(report, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
