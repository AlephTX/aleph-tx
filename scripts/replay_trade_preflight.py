#!/usr/bin/env python3
"""Audit Maker-first trade timing once before an independent policy matrix.

This is a necessary input gate for flat-at-funding, not a stream-coverage or
execution certificate. It never drops prints, repairs clocks or widens bounds.
Only aggregate timing evidence is exported; no order or trade bodies are copied.
"""
import argparse
from hashlib import sha256
import json
import os
from pathlib import Path

from archive_public_capture import digest, rows


def timing_policy(config):
    policy = {key: config[key] for key in
              ("start_ms", "end_ms", "maker_leg", "trade_delivery_grace_ms")}
    if (any(type(value) is not int or value < 0 for value in policy.values())
            or policy["start_ms"] >= policy["end_ms"] or policy["maker_leg"] not in (0, 1)
            or not 0 < policy["trade_delivery_grace_ms"] <= 5000
            or not config.get("flat_at_funding")):
        raise ValueError("trade preflight requires an explicit flat-at-funding timing policy")
    return policy


def audit(tape, config):
    policy = timing_policy(config)
    start, end = policy["start_ms"], policy["end_ms"]
    legs = [{"trades": 0, "ignored_prestart_prints": 0, "future_source_times": 0,
             "over_delivery_bound": 0, "max_delivery_ms": 0, "min_delivery_ms": None,
             "first_violation_at_ms": None, "feed_online_events": 0,
             "feed_offline_events": 0, "delivery_buckets": dict.fromkeys(
                 ("future", "0_100", "101_250", "251_750", "751_2000", "2001_5000", "over_5000"), 0)}
            for _ in range(2)]
    payload_hash = sha256()
    previous, ended, count = start, False, 0
    signature = lambda s: (s.st_dev, s.st_ino, s.st_size, s.st_mtime_ns, s.st_ctime_ns)
    with Path(tape).open("rb") as stream:
        before = os.fstat(stream.fileno())
        for raw in rows(stream):
            payload_hash.update(raw)
            count += 1
            event = json.loads(raw)
            at, kind = event["at_ms"], event["kind"]
            if type(at) is not int or not previous <= at <= end or ended:
                raise ValueError(f"invalid replay order at line {count}")
            previous = at
            if kind == "end":
                if at != end:
                    raise ValueError("replay end does not match the timing policy")
                ended = True
                continue
            if kind not in ("book", "trade", "trade_feed", "turnover_control"):
                raise ValueError(f"unsupported replay event at line {count}")
            if kind not in ("trade", "trade_feed"):
                continue
            leg = event["leg"]
            if type(leg) is not int or leg not in (0, 1):
                raise ValueError(f"invalid replay leg at line {count}")
            stats = legs[leg]
            if kind == "trade_feed":
                if type(event["online"]) is not bool:
                    raise ValueError(f"invalid feed state at line {count}")
                stats["feed_online_events" if event["online"] else "feed_offline_events"] += 1
                continue
            source = event["source_ms"]
            if type(source) is not int or source <= 0:
                raise ValueError(f"invalid source time at line {count}")
            stats["trades"] += 1
            # Match Maker-first: this flat-start ledger cannot have orders
            # affected by prints with source time strictly before its start.
            if source < start:
                stats["ignored_prestart_prints"] += 1
                continue
            delay = at - source
            stats["max_delivery_ms"] = max(stats["max_delivery_ms"], delay)
            old = stats["min_delivery_ms"]
            stats["min_delivery_ms"] = delay if old is None else min(old, delay)
            bucket = ("future" if delay < 0 else "0_100" if delay <= 100 else
                      "101_250" if delay <= 250 else "251_750" if delay <= 750 else
                      "751_2000" if delay <= 2000 else "2001_5000" if delay <= 5000 else "over_5000")
            stats["delivery_buckets"][bucket] += 1
            stats["future_source_times"] += int(delay < 0)
            stats["over_delivery_bound"] += int(delay > policy["trade_delivery_grace_ms"])
            if (delay < 0 or delay > policy["trade_delivery_grace_ms"]) and stats["first_violation_at_ms"] is None:
                stats["first_violation_at_ms"] = at
        after = os.fstat(stream.fileno())
    if signature(before) != signature(after) or signature(after) != signature(Path(tape).stat()):
        raise ValueError("replay input changed during timing audit")
    if not ended:
        raise ValueError("replay input lacks its terminal end marker")
    maker = legs[policy["maker_leg"]]
    issues = [key for key in ("future_source_times", "over_delivery_bound") if maker[key]]
    return {"schema_version": 1, "scope": "maker_trade_timing_only",
            "policy": policy, "input_sha256": payload_hash.hexdigest(), "input_bytes": before.st_size,
            "auditor_sha256": digest(Path(__file__)), "events": count, "by_leg": legs,
            "trade_timing_admitted": not issues, "blockers": issues,
            "stream_quality_verified": False, "execution_qualified": False}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("config", type=Path)
    parser.add_argument("tape", type=Path)
    parser.add_argument("output", type=Path, help="new immutable aggregate report")
    args = parser.parse_args()
    if args.output.exists() or args.output.is_symlink():
        parser.error("output already exists")
    config_hash = digest(args.config)
    config = json.loads(args.config.read_text())
    if digest(args.config) != config_hash:
        raise ValueError("configuration changed during read")
    report = audit(args.tape, config) | {"config_sha256": config_hash}
    os.umask(0o077)
    with args.output.open("x") as stream:
        json.dump(report, stream, indent=2, allow_nan=False)
        stream.write("\n")
    print(json.dumps({key: report[key] for key in ("trade_timing_admitted", "blockers", "input_sha256")}))
    return 0 if report["trade_timing_admitted"] else 2


if __name__ == "__main__":
    raise SystemExit(main())
