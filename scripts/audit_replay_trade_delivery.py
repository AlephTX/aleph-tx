#!/usr/bin/env python3
"""Link a frozen Lighter replay to sealed public prints and separate its clocks.

This read-only audit supplements replay_trade_preflight, which remains the
admission rule. No source timestamp, delivery grace, replay or cash is changed.
Only aggregate evidence is returned; public identities and trade bodies stay in
the source archive. Cross-clock differences are not calibrated network delays.
"""
import argparse
from decimal import Decimal
from hashlib import sha256
from itertools import zip_longest
import json
import os
from pathlib import Path

from archive_public_capture import digest, records, rows
from audit_lighter_trade_clock import SOURCES, distribution, observe, write_report
from capture_timing import precise_available_ms
from replay_trade_preflight import audit as preflight, timing_policy

COMPONENTS = ("availability_minus_block_us", "execution_minus_block_us",
              "receipt_minus_execution_us", "availability_minus_receipt_us",
              "projection_rounding_us")


def positive(value):
    return type(value) is int and value > 0


def components(row, trade, at):
    """Use floored integer microseconds consistently so the bridge is exact."""
    stamp = row["capture_timing"]
    receipt = stamp["receipt_wall_ns"] // 1000
    available = stamp["available_wall_ns"] // 1000
    execution = trade.get("transaction_time")
    block = trade["timestamp"] * 1000
    out = {"availability_minus_block_us": at * 1000 - block,
           "availability_minus_receipt_us": available - receipt,
           "projection_rounding_us": at * 1000 - available,
           "execution_minus_block_us": None, "receipt_minus_execution_us": None}
    if positive(execution):
        out["execution_minus_block_us"] = execution - block
        out["receipt_minus_execution_us"] = receipt - execution
        assert sum(out[k] for k in COMPONENTS[1:]) == out[COMPONENTS[0]]
    return out


def annotated_prints(archive, venue, market, leg, start, end, counts):
    """Independent projection with extra clocks, checked against every tape print."""
    source = SOURCES[venue]
    seen = {}
    online = False
    previous = 0
    for raw in records(archive, source=source):
        row = json.loads(raw)
        if row["source"] != source or row.get("market") not in (None, market):
            continue
        at = precise_available_ms(row)
        if at < previous:
            raise ValueError("public availability clock regressed")
        previous = at
        event = row["event"]
        if event in ("disconnect", "clock_refresh"):
            online = False
            continue
        if event not in ("message", "subscribed"):
            continue
        data = row["payload"]
        if data["channel"].replace("/", ":") != f"trade:{market}":
            continue
        selected = start <= at < end
        counts["market_channel_messages_in_window"] += int(selected)
        snapshot = data["type"] == "subscribed/trade"
        if snapshot:
            online = True
        for trade in data.get("trades") or []:
            if str(trade["market_id"]) != market or type(trade["is_maker_ask"]) is not bool:
                continue
            source_ms = trade["timestamp"]
            identity = trade["trade_id"]
            if (not positive(source_ms) or type(identity) not in (int, str)
                    or not str(identity).isascii() or not str(identity).isdecimal()):
                raise ValueError("invalid public clock or identity")
            if not online:
                raise ValueError("public print outside acknowledged stream")
            value = (source_ms, str(trade["price"]), str(trade["size"]), trade["is_maker_ask"])
            metadata = (trade.get("transaction_time"), trade.get("block_height"), trade.get("sequence_index"))
            # Keep the same identity type as public_replay_sources.trade_events.
            if identity in seen:
                old, old_metadata = seen[identity]
                if old != value:
                    raise ValueError("conflicting duplicate public projection")
                counts["duplicate_records"] += int(selected)
                counts["conflicting_clock_metadata"] += int(selected and old_metadata != metadata)
                continue
            seen[identity] = value, metadata
            if snapshot:
                counts["snapshot_records"] += int(selected)
                continue
            if Decimal(value[1]) == 0:
                if Decimal(value[2]) <= 0:
                    raise ValueError("invalid zero-price public transfer")
                counts["excluded_zero_price_transfers"] += int(selected)
                continue
            if not selected:
                continue
            projected = dict(kind="trade", at_ms=at, source_ms=source_ms, leg=leg,
                             aggressor="buy" if value[3] else "sell", price=value[1], contracts=value[2])
            yield projected, trade, components(row, trade, at)


def tape_prints(tape, leg, policy, proof):
    previous = policy["start_ms"]
    ended = False
    h = sha256()
    signature = lambda s: (s.st_dev, s.st_ino, s.st_size, s.st_mtime_ns, s.st_ctime_ns)
    with Path(tape).open("rb") as f:
        before = os.fstat(f.fileno())
        for raw in rows(f):
            h.update(raw)
            row = json.loads(raw)
            at, kind = row["at_ms"], row["kind"]
            if type(at) is not int or not previous <= at <= policy["end_ms"] or ended:
                raise ValueError("invalid replay time order")
            previous = at
            if kind == "end":
                if at != policy["end_ms"]:
                    raise ValueError("invalid replay end")
                ended = True
            elif kind == "trade":
                if type(row["leg"]) is not int or row["leg"] not in (0, 1) or not positive(row["source_ms"]):
                    raise ValueError("invalid replay trade identity or source clock")
                if row["leg"] == leg:
                    yield row
            elif kind not in ("book", "trade_feed", "turnover_control"):
                raise ValueError("unsupported replay event")
        after = os.fstat(f.fileno())
    if (not ended or signature(before) != signature(after)
            or signature(after) != signature(Path(tape).stat())):
        raise ValueError("incomplete or changed replay input")
    proof.update(input_sha256=h.hexdigest(), input_bytes=before.st_size)


def linked_audit(archive, tape, config, venue, market, leg, *, expected_tape_sha256):
    policy = timing_policy(config)
    if (venue not in SOURCES or type(leg) is not int or leg not in (0, 1)
            or not isinstance(market, str) or not market.isascii() or not market.isdecimal()
            or str(int(market)) != market or not 0 <= int(market) <= 65535
            or config["pair"]["legs"][leg]["venue"] != venue):
        raise ValueError("explicit matching venue, leg and native market required")
    archive = Path(archive).resolve(strict=True)
    manifest_path = archive / "manifest.json"
    manifest_sha = digest(manifest_path)
    manifest = json.loads(manifest_path.read_text())
    shards = [s for s in manifest["shards"] if Path(s["path"]).parts[2] == SOURCES[venue]]
    counts = dict.fromkeys(("matched_prints", "market_channel_messages_in_window", "ignored_prestart_prints", "snapshot_records",
        "duplicate_records", "conflicting_clock_metadata", "excluded_zero_price_transfers",
        "missing_execution_time", "invalid_execution_time", "execution_before_block",
        "execution_after_receipt", "legacy_bound_violations", "execution_bound_violations",
        "legacy_violation_execution_within_bound", "prestart_block_execution_after_start"), 0)
    cohorts = {name: {key: distribution() for key in COMPONENTS}
               for name in ("all_nonprestart", "legacy_bound_violations", "first_violation")}
    proof = {}
    first = None
    originals = annotated_prints(archive, venue, market, leg, policy["start_ms"], policy["end_ms"], counts)
    replay = tape_prints(tape, leg, policy, proof)
    for original, actual in zip_longest(originals, replay):
        if original is None or actual is None or original[0] != actual:
            raise ValueError("sealed archive and frozen replay projection differ")
        event, trade, parts = original
        counts["matched_prints"] += 1
        execution = trade.get("transaction_time")
        valid_execution = positive(execution)
        if execution is None or type(execution) is int and execution == 0:
            counts["missing_execution_time"] += 1
        elif not valid_execution:
            counts["invalid_execution_time"] += 1
        if event["source_ms"] < policy["start_ms"]:
            counts["ignored_prestart_prints"] += 1
            counts["prestart_block_execution_after_start"] += int(valid_execution and execution >= policy["start_ms"] * 1000)
            continue
        bound = policy["trade_delivery_grace_ms"] * 1000
        bad = parts["availability_minus_block_us"] < 0 or parts["availability_minus_block_us"] > bound
        counts["legacy_bound_violations"] += int(bad)
        if valid_execution:
            delay = event["at_ms"] * 1000 - execution
            counts["execution_before_block"] += int(parts["execution_minus_block_us"] < 0)
            counts["execution_after_receipt"] += int(parts["receipt_minus_execution_us"] < 0)
            counts["execution_bound_violations"] += int(delay < 0 or delay > bound)
            counts["legacy_violation_execution_within_bound"] += int(bad and 0 <= delay <= bound)
        selected = ["all_nonprestart"]
        if bad:
            selected.append("legacy_bound_violations")
            if first is None:
                first = event["at_ms"]
                selected.append("first_violation")
        for cohort in selected:
            for key, value in parts.items():
                if value is not None:
                    observe(cohorts[cohort][key], value)
    if proof.get("input_sha256") != expected_tape_sha256:
        raise ValueError("frozen tape hash differs")
    if (digest(manifest_path) != manifest_sha
            or any(digest(archive / s["path"]) != s["sha256"] for s in shards)):
        raise ValueError("archive changed during linked clock audit")
    if counts["matched_prints"]:
        projection_status = "matched_prints"
    elif counts["market_channel_messages_in_window"]:
        projection_status = "market_messages_without_projected_prints"
    elif shards:
        projection_status = "no_market_messages_in_window"
    else:
        projection_status = "no_venue_trade_shards"
    return dict(schema_version=2, scope="sealed_public_trade_to_frozen_replay_clock_link",
        venue=venue, native_market=market, leg=leg, timing_policy=policy, **proof,
        archive_manifest_sha256=manifest_sha, consumed_shards=[{k:s[k] for k in ("path","sha256","bytes")} for s in shards],
        counts=counts, components_us=cohorts, first_violation_at_ms=first,
        auditor_sha256=digest(Path(__file__)), projection_status=projection_status,
        projection_identity_verified=bool(counts["matched_prints"]),
        source_timestamp_substituted=False, guard_calibrated=False, execution_qualified=False,
        limitations=["This verifies the existing projection, not the truth of its block timestamp as execution time.",
            "Execution/receipt differences include unknown clock offsets; they are not network latency.",
            "Observed tails do not bound future messages or certify a shorter cancellation barrier.",
            "Execution-time reclassification is diagnostic only; original funding failures and cash remain unchanged."])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("config", type=Path)
    parser.add_argument("tape", type=Path)
    parser.add_argument("archive", type=Path)
    parser.add_argument("venue", choices=tuple(SOURCES))
    parser.add_argument("market")
    parser.add_argument("leg", type=int, choices=(0, 1))
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    config_sha = digest(args.config)
    config = json.loads(args.config.read_text())
    prior = preflight(args.tape, config)
    linked = linked_audit(args.archive, args.tape, config, args.venue, args.market, args.leg,
                          expected_tape_sha256=prior["input_sha256"])
    if digest(args.config) != config_sha:
        raise ValueError("config changed during audit")
    os.umask(0o077)
    write_report(args.output, dict(config_sha256=config_sha, preflight=prior, linked=linked))
    print(json.dumps(dict(projection_identity_verified=linked["projection_identity_verified"],
        projection_status=linked["projection_status"],
        legacy_bound_violations=linked["counts"]["legacy_bound_violations"],
        execution_qualified=False)))


if __name__ == "__main__":
    main()
