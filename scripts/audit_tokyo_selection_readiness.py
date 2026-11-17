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
TEN_HOUR_MS = 10 * 3_600_000
TEN_MIN_BUCKET_MS = 10 * 60_000
REQUIRED_10M_BUCKETS = 60
BOOK_SOURCES = frozenset(
    {
        "lighter",
        "lighter_robinhood",
        "binance",
        "coinbase",
        "hyperliquid",
        "arcus",
        "extended",
        "bulk",
    }
)

STRATEGY_PROFILES: dict[str, dict[str, tuple[str, ...]]] = {
    "arcus_single_venue": {
        "required": ("arcus", "arcus_trades"),
        "optional_reference": ("binance",),
    },
    "lighter_rh_single_venue": {
        "required": ("lighter_robinhood", "lighter_robinhood_trades"),
        "optional_reference": ("binance",),
    },
    "lighter_core_single_venue": {
        "required": ("lighter", "lighter_trades"),
        "optional_reference": ("binance", "hyperliquid"),
    },
    "arcus_lighter_rh_pair": {
        "required": ("arcus", "arcus_trades", "lighter_robinhood", "lighter_robinhood_trades"),
        "optional_reference": ("binance",),
    },
    "arcus_hip3_pair": {
        "required": ("arcus", "arcus_trades", "hyperliquid", "hyperliquid_trades"),
        "optional_reference": (),
    },
    "lighter_core_hl_pair": {
        "required": ("lighter", "lighter_trades", "hyperliquid", "hyperliquid_trades"),
        "optional_reference": (),
    },
    "bulk_isolated": {
        "required": ("bulk", "binance"),
        "optional_reference": (),
    },
}


def _is_subscription_confirm(source: str, event: str, payload: dict | None) -> bool:
    if event == "subscribed":
        return True
    if event != "message" or not isinstance(payload, dict):
        return False
    ptype = payload.get("type")
    if source in ("lighter", "lighter_robinhood"):
        return ptype in ("subscribed/order_book", "subscribed/market_stats")
    if source in ("lighter_trades", "lighter_robinhood_trades"):
        return ptype == "subscribed/trade"
    if source == "arcus":
        return ptype == "subscribed"
    if source in ("extended", "extended_trades"):
        return payload.get("seq") == 1
    if source == "binance":
        return payload.get("e") == "bookTicker"
    if source == "bulk":
        return ptype == "l2Snapshot"
    return False


def _is_usable_market_data(source: str, event: str, payload: dict | None) -> bool:
    if source not in BOOK_SOURCES:
        return _is_subscription_confirm(source, event, payload) or (
            event == "message" and isinstance(payload, dict)
        )
    if event != "message" or not isinstance(payload, dict):
        return False
    if source in ("lighter", "lighter_robinhood"):
        return payload.get("type") in ("subscribed/order_book", "update/order_book") and isinstance(
            payload.get("order_book"), dict
        )
    if source == "hyperliquid":
        return payload.get("channel") == "l2Book" and isinstance(payload.get("data"), dict)
    if source == "arcus":
        return (
            payload.get("channel") == "l2Orderbook"
            and payload.get("type") in ("subscribed", "channel_data")
            and isinstance(payload.get("contents"), dict)
        )
    if source == "binance":
        data = payload.get("data") if isinstance(payload.get("data"), dict) else payload
        return data.get("e") == "bookTicker" or ("b" in data and "a" in data)
    if source == "extended":
        return payload.get("type") in ("SNAPSHOT", "DELTA")
    if source == "bulk":
        return payload.get("type") == "l2Snapshot"
    return True


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
    max_overall_gap_ms: int = 15_000,
    max_source_gap_ms: int = 60_000,
    session_gated_sources: tuple[str, ...] = (),
    manifest_path: Path | None = None,
) -> dict:
    if lighter_leverage <= 0 or hedge_leverage <= 0:
        raise ValueError("leverages must be positive")
    if max_overall_gap_ms <= 0 or max_source_gap_ms <= 0:
        raise ValueError("gap limits must be positive")
    session_gated_set = {s.strip() for s in session_gated_sources if s and s.strip()}
    target = target.resolve(strict=True)
    resolved_manifest: Path | None = None
    if manifest_path is not None:
        resolved_manifest = manifest_path.resolve(strict=True)
    elif target.is_dir() and (target / "manifest.json").is_file():
        resolved_manifest = target / "manifest.json"
    elif target.is_file():
        sidecar = target.with_name(f"{target.name}.bronze_manifest.json")
        if sidecar.is_file():
            resolved_manifest = sidecar
    manifest_sha256: str | None = None
    if resolved_manifest is not None:
        if resolved_manifest.name.endswith(".bronze_manifest.json") and target.is_file():
            meta = json.loads(resolved_manifest.read_text(encoding="utf-8"))
            expected_bytes = meta.get("capture_bytes")
            if isinstance(expected_bytes, int) and target.stat().st_size != expected_bytes:
                raise ValueError(
                    f"capture size {target.stat().st_size} does not match bronze manifest capture_bytes {expected_bytes}"
                )
        manifest_sha256 = digest(resolved_manifest)

    latencies: dict[str, list[float]] = {}
    source_counts: dict[str, dict[str, int]] = {}
    source_first_ms: dict[str, int] = {}
    source_last_ms: dict[str, int] = {}
    source_max_seen_ms: dict[str, int] = {}
    source_max_inter_gap_ms: dict[str, int] = {}
    source_buckets: dict[str, set[int]] = {}
    overall_buckets: set[int] = set()
    overall_max_seen_ms: int | None = None
    max_overall_gap_observed: int = 0
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
    # Reconnect recovery tracking separated by cause ("disconnect" vs "clock_refresh")
    # and milestone ("confirm" vs "usable_data").
    pending_outage: dict[str, dict[str, int | str | bool]] = {}
    recovery_samples: dict[str, dict[str, list[float]]] = {}
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

        counters = source_counts.setdefault(
            source,
            {
                "messages": 0,
                "disconnects": 0,
                "clock_refreshes": 0,
                "clock_ahead_samples": 0,
                "latency_samples": 0,
            },
        )
        rec_lists = recovery_samples.setdefault(
            source,
            {
                "disconnect_confirm_ms": [],
                "disconnect_usable_ms": [],
                "clock_refresh_confirm_ms": [],
                "clock_refresh_usable_ms": [],
            },
        )
        if event == "disconnect":
            counters["disconnects"] += 1
            pending_outage[source] = {
                "kind": "disconnect",
                "at_ms": receipt,
                "confirmed": False,
                "usable": False,
            }
            first_receipt_ms = receipt if first_receipt_ms is None else min(first_receipt_ms, receipt)
            last_receipt_ms = receipt if last_receipt_ms is None else max(last_receipt_ms, receipt)
            continue
        if event == "clock_refresh":
            counters["clock_refreshes"] += 1
            pending_outage[source] = {
                "kind": "clock_refresh",
                "at_ms": receipt,
                "confirmed": False,
                "usable": False,
            }
            first_receipt_ms = receipt if first_receipt_ms is None else min(first_receipt_ms, receipt)
            last_receipt_ms = receipt if last_receipt_ms is None else max(last_receipt_ms, receipt)
            continue
        if event not in ("message", "subscribed"):
            continue
        counters["messages"] += 1

        payload_peek = row.get("payload") if isinstance(row.get("payload"), dict) else None
        outage = pending_outage.get(source)
        if outage is not None:
            elapsed_outage = float(max(0, receipt - int(outage["at_ms"])))
            prefix = str(outage["kind"])
            if not outage["confirmed"] and _is_subscription_confirm(source, event, payload_peek):
                rec_lists[f"{prefix}_confirm_ms"].append(elapsed_outage)
                outage["confirmed"] = True
            if not outage["usable"] and _is_usable_market_data(source, event, payload_peek):
                rec_lists[f"{prefix}_usable_ms"].append(elapsed_outage)
                outage["usable"] = True
            if outage["usable"]:
                if not outage["confirmed"]:
                    rec_lists[f"{prefix}_confirm_ms"].append(elapsed_outage)
                pending_outage.pop(source, None)

        first_receipt_ms = receipt if first_receipt_ms is None else min(first_receipt_ms, receipt)
        last_receipt_ms = receipt if last_receipt_ms is None else max(last_receipt_ms, receipt)
        if overall_max_seen_ms is not None and receipt > overall_max_seen_ms:
            max_overall_gap_observed = max(
                max_overall_gap_observed, receipt - overall_max_seen_ms
            )
        overall_max_seen_ms = (
            receipt if overall_max_seen_ms is None else max(overall_max_seen_ms, receipt)
        )
        bucket_idx = max(0, receipt - first_receipt_ms) // TEN_MIN_BUCKET_MS
        if bucket_idx < REQUIRED_10M_BUCKETS:
            overall_buckets.add(bucket_idx)
            source_buckets.setdefault(source, set()).add(bucket_idx)

        source_first_ms[source] = (
            receipt if source not in source_first_ms else min(source_first_ms[source], receipt)
        )
        source_last_ms[source] = (
            receipt if source not in source_last_ms else max(source_last_ms[source], receipt)
        )
        prev_src_max = source_max_seen_ms.get(source)
        if prev_src_max is not None and receipt > prev_src_max:
            source_max_inter_gap_ms[source] = max(
                source_max_inter_gap_ms.get(source, 0), receipt - prev_src_max
            )
        source_max_seen_ms[source] = (
            receipt if prev_src_max is None else max(prev_src_max, receipt)
        )

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
        s_first = source_first_ms.get(source)
        s_last = source_last_ms.get(source)
        s_span_hours = (
            round((s_last - s_first) / 3_600_000.0, 4)
            if s_first is not None and s_last is not None
            else 0.0
        )
        lead_gap = (
            max(0, s_first - first_receipt_ms)
            if s_first is not None and first_receipt_ms is not None
            else None
        )
        trail_gap = (
            max(0, last_receipt_ms - s_last)
            if s_last is not None and last_receipt_ms is not None
            else None
        )
        s_max_gap = (
            max(
                source_max_inter_gap_ms.get(source, 0),
                lead_gap or 0,
                trail_gap or 0,
            )
            if s_first is not None and s_last is not None
            else None
        )
        s_buckets = len(source_buckets.get(source, set()))
        rec = recovery_samples.get(source, {})
        dc_conf = sorted(rec.get("disconnect_confirm_ms", []))
        dc_use = sorted(rec.get("disconnect_usable_ms", []))
        cr_conf = sorted(rec.get("clock_refresh_confirm_ms", []))
        cr_use = sorted(rec.get("clock_refresh_usable_ms", []))
        unrecovered_dc = int(
            source in pending_outage and pending_outage[source].get("kind") == "disconnect"
        )
        source_summaries[source] = {
            **source_counts[source],
            "unrecovered_disconnects": unrecovered_dc,
            "reconnect_confirm_p50_ms": _quantile(dc_conf, 0.50),
            "reconnect_confirm_max_ms": round(dc_conf[-1], 3) if dc_conf else None,
            "reconnect_usable_p50_ms": _quantile(dc_use, 0.50),
            "reconnect_usable_max_ms": round(dc_use[-1], 3) if dc_use else None,
            "clock_refresh_confirm_p50_ms": _quantile(cr_conf, 0.50),
            "clock_refresh_usable_p50_ms": _quantile(cr_use, 0.50),
            "clock_refresh_usable_max_ms": round(cr_use[-1], 3) if cr_use else None,
            "first_receipt_ms": s_first,
            "last_receipt_ms": s_last,
            "span_hours": s_span_hours,
            "max_gap_ms": s_max_gap,
            "covered_10m_buckets": s_buckets,
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

    continuity_blockers: list[str] = []
    if not source_summaries or first_receipt_ms is None or last_receipt_ms is None:
        continuity_blockers.append("empty_capture")
    else:
        if duration_hours < 10.0:
            continuity_blockers.append(f"duration_hours_below_10h:{duration_hours}")
        if len(overall_buckets) < REQUIRED_10M_BUCKETS:
            continuity_blockers.append(
                f"covered_10m_buckets_incomplete:{len(overall_buckets)}/{REQUIRED_10M_BUCKETS}"
            )
        if max_overall_gap_observed > max_overall_gap_ms:
            continuity_blockers.append(
                f"max_overall_gap_ms_exceeded:{max_overall_gap_observed}>{max_overall_gap_ms}"
            )
        active_book_sources = 0
        for src, summary in source_summaries.items():
            if src in session_gated_set:
                continue
            if summary["disconnects"] > 0:
                continuity_blockers.append(f"source_disconnects:{src}:{summary['disconnects']}")
            if src in BOOK_SOURCES:
                active_book_sources += 1
                if summary["span_hours"] < 9.95:
                    continuity_blockers.append(
                        f"source_span_below_9.95h:{src}:{summary['span_hours']}"
                    )
                if summary["covered_10m_buckets"] < REQUIRED_10M_BUCKETS:
                    continuity_blockers.append(
                        f"source_10m_buckets_incomplete:{src}:{summary['covered_10m_buckets']}/{REQUIRED_10M_BUCKETS}"
                    )
                if summary["max_gap_ms"] is None or summary["max_gap_ms"] > max_source_gap_ms:
                    continuity_blockers.append(
                        f"source_max_gap_ms_exceeded:{src}:{summary['max_gap_ms']}>{max_source_gap_ms}"
                    )
        if active_book_sources == 0:
            continuity_blockers.append("no_uninterrupted_book_source")

    # Evaluate strategy-scoped readiness separately from station-wide connection health
    # so unrelated feed reconnects do not misclassify self-contained strategies.
    strategy_readiness: dict[str, dict] = {}
    for prof_name, deps in STRATEGY_PROFILES.items():
        req = deps["required"]
        opt = deps["optional_reference"]
        if any(s not in source_summaries for s in req):
            strategy_readiness[prof_name] = {
                "status": "not_in_capture",
                "required_sources": list(req),
                "missing_sources": [s for s in req if s not in source_summaries],
            }
            continue
        prof_blockers: list[str] = []
        reconnect_sources: dict[str, dict[str, float | int | None]] = {}
        if duration_hours < 10.0:
            prof_blockers.append(f"duration_hours_below_10h:{duration_hours}")
        if len(overall_buckets) < REQUIRED_10M_BUCKETS:
            prof_blockers.append(
                f"covered_10m_buckets_incomplete:{len(overall_buckets)}/{REQUIRED_10M_BUCKETS}"
            )
        for src in req:
            st = source_summaries[src]
            if st["unrecovered_disconnects"] > 0:
                prof_blockers.append(f"unrecovered_disconnect:{src}")
            if st["disconnects"] > 0:
                reconnect_sources[src] = {
                    "disconnects": st["disconnects"],
                    "reconnect_confirm_max_ms": st["reconnect_confirm_max_ms"],
                    "reconnect_usable_max_ms": st["reconnect_usable_max_ms"],
                }
                if (
                    st["reconnect_usable_max_ms"] is None
                    or st["reconnect_usable_max_ms"] > max_source_gap_ms
                ):
                    prof_blockers.append(
                        f"reconnect_usable_exceeded:{src}:{st['reconnect_usable_max_ms']}"
                    )
            if src in BOOK_SOURCES and src not in session_gated_set:
                if st["span_hours"] < 9.95:
                    prof_blockers.append(f"source_span_below_9.95h:{src}:{st['span_hours']}")
                if st["covered_10m_buckets"] < REQUIRED_10M_BUCKETS:
                    prof_blockers.append(
                        f"source_10m_buckets_incomplete:{src}:{st['covered_10m_buckets']}/{REQUIRED_10M_BUCKETS}"
                    )
                if st["max_gap_ms"] is None or st["max_gap_ms"] > max_source_gap_ms:
                    prof_blockers.append(
                        f"source_max_gap_ms_exceeded:{src}:{st['max_gap_ms']}>{max_source_gap_ms}"
                    )
        opt_alerts = [
            f"{src}:disconnects={source_summaries[src]['disconnects']}"
            for src in opt
            if src in source_summaries and source_summaries[src]["disconnects"] > 0
        ]
        if prof_blockers:
            prof_status = "blocked"
        elif reconnect_sources:
            prof_status = "ready_with_causal_reconnect_barriers"
        else:
            prof_status = "ready_strict_zero_disconnect"
        strategy_readiness[prof_name] = {
            "status": prof_status,
            "required_sources": list(req),
            "optional_reference_sources": [s for s in opt if s in source_summaries],
            "optional_reference_alerts": opt_alerts,
            "reconnect_sources": reconnect_sources,
            "blockers": prof_blockers,
        }

    return {
        "schema": "tokyo_selection_readiness_v1",
        "manifest_sha256": manifest_sha256,
        "window": {
            "first_receipt_ms": first_receipt_ms,
            "last_receipt_ms": last_receipt_ms,
            "duration_hours": duration_hours,
            "covered_10m_buckets": len(overall_buckets),
            "required_10m_buckets": REQUIRED_10M_BUCKETS,
            "max_overall_gap_ms": max_overall_gap_observed,
            "max_overall_gap_limit_ms": max_overall_gap_ms,
            "max_source_gap_limit_ms": max_source_gap_ms,
            "session_gated_sources": sorted(session_gated_set),
            "continuity_blockers": continuity_blockers,
            "continuous_ten_hour_unit_satisfied": len(continuity_blockers) == 0,
        },
        "strategy_readiness": strategy_readiness,
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
    parser.add_argument("--max-overall-gap-ms", type=int, default=15_000)
    parser.add_argument("--max-source-gap-ms", type=int, default=60_000)
    parser.add_argument(
        "--session-gated-sources",
        nargs="*",
        default=(),
        help="Optional sources with scheduled market-hours halts (recorded in window.session_gated_sources)",
    )
    parser.add_argument(
        "--manifest",
        type=Path,
        default=None,
        help="Optional parent .bronze_manifest.json or archive manifest.json path to bind manifest_sha256",
    )
    args = parser.parse_args()

    report = audit_readiness(
        args.capture,
        lighter_leverage=args.lighter_leverage,
        hedge_leverage=args.hedge_leverage,
        max_overall_gap_ms=args.max_overall_gap_ms,
        max_source_gap_ms=args.max_source_gap_ms,
        session_gated_sources=tuple(args.session_gated_sources),
        manifest_path=args.manifest,
    )
    if args.output:
        write_report(report, args.output)
    else:
        print(json.dumps(report, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
