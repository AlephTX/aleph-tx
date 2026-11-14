#!/usr/bin/env python3
"""Audit timing fields in a sealed Lighter public archive without changing replay.

Trade.timestamp is block-open milliseconds; transaction_time is sequencer
execution microseconds. Cross-clock differences below are observations, never
calibrated network latency. Only aggregate diagnostics are exported.
"""
import argparse
import json
import os
from pathlib import Path
from tempfile import NamedTemporaryFile

from archive_public_capture import digest, records

SOURCES = {"lighter": "lighter_trades", "lighter_robinhood": "lighter_robinhood_trades"}
TIMING_FIELDS = ("timestamp", "transaction_time", "block_height", "sequence_index")


def distribution():
    return {"count": 0, "sum_us": 0, "min_us": None, "max_us": None}


def observe(stats, value):
    stats["count"] += 1
    stats["sum_us"] += value
    stats["min_us"] = value if stats["min_us"] is None else min(stats["min_us"], value)
    stats["max_us"] = value if stats["max_us"] is None else max(stats["max_us"], value)


def positive_integer(value):
    return type(value) is int and value > 0


def audit(archive, venue, market, *, start_ms=None, end_ms=None, clock_guard_ms=100):
    if (venue not in SOURCES or not isinstance(market, str) or not market.isascii()
            or not market.isdecimal() or not 0 <= int(market) <= 65535
            or str(int(market)) != market
            or type(clock_guard_ms) is not int or not 0 <= clock_guard_ms <= 1000
            or (start_ms is None) != (end_ms is None)
            or (start_ms is not None and (not positive_integer(start_ms)
                                          or not positive_integer(end_ms) or start_ms >= end_ms))):
        raise ValueError("explicit domain, native market ID and valid timing window required")
    archive = archive.resolve(strict=True)
    manifest_path = archive / "manifest.json"
    manifest_hash = digest(manifest_path)
    manifest = json.loads(manifest_path.read_text())
    source = SOURCES[venue]
    shards = [s for s in manifest["shards"] if Path(s["path"]).parts[2] == source]
    counts = dict.fromkeys(("subscription_acks", "feed_offline_events", "snapshot_trade_records",
                           "duplicate_trade_records", "conflicting_duplicate_records",
                           "unacknowledged_trade_records", "live_unique_trades",
                           "missing_execution_time", "invalid_execution_time", "invalid_block_time",
                           "execution_before_block_time", "execution_after_local_receipt",
                           "block_execution_delta_exceeds_guard", "usable_execution_pairs"), 0)
    distributions = {name: distribution() for name in ("execution_minus_block_us",
                     "receipt_minus_block_us", "receipt_minus_execution_us", "availability_minus_receipt_us")}
    fields = {name: {"present": 0, "missing_or_zero": 0, "invalid": 0} for name in TIMING_FIELDS}
    first, last, previous, online, seen = None, None, 0, False, {}
    for raw in records(archive, source=source):
        row = json.loads(raw)
        at, receipt = row["available_at_ms"], row["received_at_ms"]
        if (not positive_integer(at) or not positive_integer(receipt)
                or at < previous or receipt > at or row["source"] != source):
            raise ValueError("invalid public receipt order")
        previous = at
        if row.get("market") not in (None, market):
            continue
        selected = start_ms is None or start_ms <= at < end_ms
        event = row["event"]
        if event in ("disconnect", "clock_refresh"):
            online = False
            counts["feed_offline_events"] += int(selected)
            continue
        if event not in ("message", "subscribed"):
            continue
        data = row["payload"]
        if data["channel"].replace("/", ":") != f"trade:{market}":
            continue
        subscribed = data["type"] == "subscribed/trade"
        if subscribed:
            online = True
            counts["subscription_acks"] += int(selected)
        for trade in data.get("trades") or []:
            if str(trade["market_id"]) != market:
                continue
            identity = trade["trade_id"]
            if (type(identity) not in (int, str) or isinstance(identity, bool)
                    or not str(identity).isascii() or not str(identity).isdecimal()):
                raise ValueError("invalid public trade identity")
            identity = str(identity)
            value = tuple(trade.get(key) for key in (*TIMING_FIELDS, "price", "size", "is_maker_ask"))
            if identity in seen:
                counts["duplicate_trade_records"] += int(selected)
                counts["conflicting_duplicate_records"] += int(selected and seen[identity] != value)
                continue
            seen[identity] = value
            if subscribed:
                counts["snapshot_trade_records"] += int(selected)
                continue
            if not online:
                counts["unacknowledged_trade_records"] += int(selected)
                continue
            if not selected:
                continue
            counts["live_unique_trades"] += 1
            first = at if first is None else first
            last = at
            for name in TIMING_FIELDS:
                val = trade.get(name)
                # Zero timestamps are unavailable. A zero sequence index or
                # block height is preserved as an integer identifier.
                is_identifier = name in ("block_height", "sequence_index")
                present = (type(val) is int and val >= 0) if is_identifier else positive_integer(val)
                state = ("present" if present else "missing_or_zero"
                         if val is None or type(val) is int and val == 0 else "invalid")
                fields[name][state] += 1
            observe(distributions["availability_minus_receipt_us"], (at - receipt) * 1000)
            block, execution = trade.get("timestamp"), trade.get("transaction_time")
            if not positive_integer(block):
                counts["invalid_block_time"] += 1
            else:
                observe(distributions["receipt_minus_block_us"], receipt * 1000 - block * 1000)
            if execution is None or type(execution) is int and execution == 0:
                counts["missing_execution_time"] += 1
                continue
            if not positive_integer(execution):
                counts["invalid_execution_time"] += 1
                continue
            delta_receipt = receipt * 1000 - execution
            observe(distributions["receipt_minus_execution_us"], delta_receipt)
            counts["execution_after_local_receipt"] += int(delta_receipt < 0)
            if not positive_integer(block):
                continue
            delta_block = execution - block * 1000
            observe(distributions["execution_minus_block_us"], delta_block)
            counts["execution_before_block_time"] += int(delta_block < 0)
            counts["block_execution_delta_exceeds_guard"] += int(abs(delta_block) > clock_guard_ms * 1000)
            counts["usable_execution_pairs"] += int(delta_block >= 0)
    # Recheck all consumed source shards; do not certify data changed in flight.
    if (digest(manifest_path) != manifest_hash
            or any(digest(archive / s["path"]) != s["sha256"] for s in shards)):
        raise ValueError("public archive changed during timing audit")
    total = counts["live_unique_trades"]
    complete = (total > 0 and counts["usable_execution_pairs"] == total
                and not counts["conflicting_duplicate_records"]
                and not counts["unacknowledged_trade_records"])
    return {"schema_version": 1, "scope": "public_execution_clock_fields_only", "venue": venue,
            "market": market, "selected_availability_window_ms": [start_ms, end_ms],
            "first_live_available_ms": first, "last_live_available_ms": last,
            "archive_manifest_sha256": manifest_hash,
            "consumed_shards": [{k: s[k] for k in ("path", "sha256", "bytes")} for s in shards],
            "auditor_sha256": digest(Path(__file__)),
            "archive_reader_sha256": digest(Path(__file__).with_name("archive_public_capture.py")),
            "counts": counts, "timing_fields": fields, "distributions": distributions,
            "execution_field_coverage": counts["usable_execution_pairs"] / total if total else None,
            "execution_fields_complete": complete, "clock_guard_ms_for_comparison": clock_guard_ms,
            "clock_guard_calibrated": False, "client_sequencer_offset_calibrated": False,
            "source_semantics": {"timestamp": "sequencer_block_open_ms",
                                 "transaction_time": "sequencer_execution_us",
                                 "received_at_ms": "collector_local_wall_clock_ms",
                                 "available_at_ms": "collector_availability_ms"},
            "source_timestamp_substituted": False, "execution_qualified": False,
            "stream_quality_verified": False,
            "limitations": ["Field coverage does not establish client/sequencer clock alignment.",
                            "Cross-clock differences are not measured network latency.",
                            "No missing execution time is inferred from block time.",
                            "Existing replay source_ms and all original archives remain unchanged."]}


def write_report(output, report):
    """Publish a complete report atomically, without replacing prior evidence."""
    temporary = None
    try:
        with NamedTemporaryFile(mode="w", encoding="utf-8", dir=output.parent,
                                prefix=f".{output.name}.", suffix=".tmp", delete=False) as stream:
            temporary = Path(stream.name)
            json.dump(report, stream, indent=2, allow_nan=False)
            stream.write("\n")
            stream.flush()
            os.fsync(stream.fileno())
        os.link(temporary, output)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("archive", type=Path)
    parser.add_argument("venue", choices=tuple(SOURCES))
    parser.add_argument("market", help="native numeric market ID, independently resolved for this domain")
    parser.add_argument("output", type=Path, help="new aggregate JSON report outside Git")
    parser.add_argument("--start-ms", type=int)
    parser.add_argument("--end-ms", type=int)
    parser.add_argument("--clock-guard-ms", type=int, default=100)
    args = parser.parse_args()
    if args.output.exists() or args.output.is_symlink():
        parser.error("output already exists")
    report = audit(args.archive, args.venue, args.market, start_ms=args.start_ms,
                   end_ms=args.end_ms, clock_guard_ms=args.clock_guard_ms)
    os.umask(0o077)
    write_report(args.output, report)
    print(json.dumps({k: report[k] for k in ("execution_fields_complete", "execution_field_coverage",
                                           "archive_manifest_sha256")}))
    return 0 if report["execution_fields_complete"] else 2


if __name__ == "__main__":
    raise SystemExit(main())
