#!/usr/bin/env python3
"""Audit public fee signatures and optional local-book markouts, never eligibility.

No account identifiers, private API, order submission, or reward attribution.
Schedule matches are hypotheses against the supplied dated review, not account
tier attestations. Historical archives that stripped fees remain unknown.
"""
import argparse
from bisect import bisect_left, bisect_right
from collections import Counter
from decimal import Decimal
import json
import os
from pathlib import Path

from archive_public_capture import digest, records, verify_compiled
from audit_lighter_trade_clock import SOURCES, write_report
from capture_timing import ClockAudit
from replay_maker_campaign import unseal


def positive(value):
    return type(value) is int and value > 0


def amount(value):
    if not isinstance(value, str) or len(value) > 80:
        raise ValueError("invalid public decimal")
    result = Decimal(value)
    if not result.is_finite() or result <= 0:
        raise ValueError("invalid public decimal")
    return result


def load_policy(path):
    policy = json.loads(path.read_text())
    if (policy.get("schema_version") != 1 or policy.get("qualification_verified") is not False
            or not policy.get("observed_date") or not policy.get("sources")
            or set(policy.get("domains", {})) != set(SOURCES)):
        raise ValueError("explicit dated fee-signature policy required")
    for domain in policy["domains"].values():
        for side in ("maker", "taker"):
            seen = set()
            for label, values in domain[side].items():
                if (label not in ("premium_schedule_match", "plus_schedule_match",
                                  "standard_special_order_compatible")
                        or any(not positive(v) or v in seen for v in values)):
                    raise ValueError("ambiguous fee signature policy")
                seen.update(values)
    return policy


def classify(trade, side, version, schedule):
    key = side + "_fee"
    if key not in trade or trade[key] is None:
        return "wire_omitted_zero_or_empty" if type(version) is int and version == 1 else "legacy_or_unverified_missing"
    value = trade[key]
    if type(value) is not int or not -(2**63) <= value < 2**63:
        return "invalid_integer"
    if value == 0:
        return "explicit_zero_standard_compatible"
    for label, values in schedule[side].items():
        if value in values:
            return label
    return "unmapped_integer"


class BookMarks:
    """As-of available book marks; stale/untradable frames split continuity.

    This measures market-print markouts, not simulated fills. It does not
    calibrate exchange clocks or move trades back to their source timestamps.
    """
    def __init__(self, frames, max_age_ms=500):
        self.times, self.values, self.epochs = [], [], []
        self.max_age_ms = max_age_ms
        epoch, previous, last_valid = 0, 0, None
        for at, book in frames:
            if not positive(at) or at < previous:
                raise ValueError("book availability regressed")
            previous = at
            stamp = book.get("timestamp_ms")
            valid = (book.get("tradable") is True and positive(stamp)
                     and 0 <= at - stamp <= max_age_ms and book.get("bids") and book.get("asks"))
            mid = None
            if valid:
                bid, ask = amount(book["bids"][0]["price"]), amount(book["asks"][0]["price"])
                if bid < ask:
                    mid = (bid + ask) / 2
            if mid is None or last_valid is None or at - last_valid > max_age_ms:
                epoch += 1
            last_valid = at if mid is not None else None
            self.times.append(at)
            self.values.append((mid, stamp))
            self.epochs.append(epoch)

    def mark(self, at, horizon, price, maker_ask):
        # A trade and a compiled book at the same millisecond have no proven
        # cross-stream ordering. Entry uses strictly earlier books.
        i = bisect_left(self.times, at) - 1
        j = bisect_right(self.times, at + horizon) - 1
        if i < 0 or j < 0 or not self.times or at + horizon > self.times[-1]:
            return None
        left, right = self.values[i], self.values[j]
        if (left[0] is None or right[0] is None or self.epochs[i] != self.epochs[j]
                or not 0 <= at - left[1] <= self.max_age_ms
                or not 0 <= at + horizon - right[1] <= self.max_age_ms):
            return None
        sign = Decimal(1 if maker_ask else -1)
        return ((right[0] / price - 1) * 10_000 * sign,
                (right[0] / left[0] - 1) * 10_000 * sign)


def compiled_marks(archive, tape, config_path, venue, market, symbol, max_age_ms):
    lineage = verify_compiled(archive, tape)
    config = json.loads(config_path.read_text())
    if (lineage.get("availability_rule") != "ceil_available_wall_ns_to_ms"
            or digest(config_path) != lineage["config_sha256"]):
        raise ValueError("book input must bind this archive and precise compiler config")
    with unseal(tape) as stream:
        header = json.loads(next(stream))
        if header.get("independent_books") is not True:
            raise ValueError("independent local books required")
        matches = [i for i, leg in enumerate(header["pair"]["legs"])
                   if leg["venue"] == venue and leg["market"] == symbol]
        if len(matches) != 1:
            raise ValueError("ambiguous book domain or native market mapping")
        index = matches[0]
        identity_keys = ("venue", "market", "quote", "settlement", "base_per_contract")
        if (config["legs"][index]["lighter_market_id"] != int(market)
                or any(config["pair"]["legs"][index][k] != header["pair"]["legs"][index][k]
                       for k in identity_keys)):
            raise ValueError("ambiguous book domain or native market mapping")
        def frames():
            for line in stream:
                frame = json.loads(line)
                yield frame["now_ms"], frame["legs"][index]["book"]
        return BookMarks(frames(), max_age_ms), lineage


def audit(archive, venue, market, policy_path, *, start_ms=None, end_ms=None,
          books=None, book_config=None, symbol=None, max_book_age_ms=500):
    if (venue not in SOURCES or not isinstance(market, str) or not market.isascii()
            or not market.isdecimal() or str(int(market)) != market or int(market) > 65535
            or (start_ms is None) != (end_ms is None)
            or (start_ms is not None and (not positive(start_ms) or not positive(end_ms) or start_ms >= end_ms))
            or type(max_book_age_ms) is not int or not 1 <= max_book_age_ms <= 2000
            or (books is not None) != (book_config is not None and symbol is not None)):
        raise ValueError("explicit domain, market and valid audit policy required")
    archive = archive.resolve(strict=True)
    identity, policy_hash = digest(archive / "manifest.json"), digest(policy_path)
    policy = load_policy(policy_path)
    marks, lineage = (compiled_marks(archive, books, book_config, venue, market, symbol,
                                    max_book_age_ms) if books is not None else (None, None))
    counts, groups, seen, fee_histogram = Counter(), {}, {}, {s: Counter() for s in ("maker", "taker")}
    total, online, previous, first, last = Decimal(0), False, 0, None, None
    clock, modes = ClockAudit(), set()
    for raw in records(archive):
        row = json.loads(raw)
        precise = "capture_timing" in row
        modes.add(precise)
        at = clock.observe(row) if precise else row["available_at_ms"]
        if not positive(at) or at < previous:
            raise ValueError("capture availability regressed")
        previous = at
        if row["source"] != SOURCES[venue] or row.get("market") not in (None, market):
            continue
        selected = start_ms is None or start_ms <= at < end_ms
        if row["event"] in ("disconnect", "clock_refresh"):
            online = False
            counts["feed_offline_events"] += int(selected)
            continue
        if row["event"] not in ("message", "subscribed"):
            continue
        payload = row["payload"]
        if payload.get("type") not in ("subscribed/trade", "update/trade"):
            continue
        if payload["channel"].replace("/", ":") != f"trade:{market}":
            continue
        snapshot = payload["type"] == "subscribed/trade"
        if snapshot:
            online = True
            counts["subscription_acks"] += int(selected)
        version = payload.get("fee_fields_version")
        for trade in payload.get("trades") or []:
            if str(trade["market_id"]) != market:
                continue
            identifier = trade["trade_id"]
            if type(identifier) not in (int, str) or not str(identifier).isascii() or not str(identifier).isdecimal():
                raise ValueError("invalid public trade identity")
            identifier = str(identifier)
            value = tuple(trade.get(k) for k in ("timestamp", "transaction_time", "price", "size",
                                                "is_maker_ask", "maker_fee", "taker_fee")) + (version,)
            if identifier in seen:
                counts["duplicate_records"] += int(selected)
                counts["conflicting_duplicates"] += int(selected and seen[identifier] != value)
                continue
            seen[identifier] = value
            if snapshot or not online:
                counts["snapshot_records" if snapshot else "unacknowledged_records"] += int(selected)
                continue
            if not selected:
                continue
            if not positive(trade.get("timestamp")) or type(trade.get("is_maker_ask")) is not bool:
                raise ValueError("invalid public trade time or side")
            price, size = amount(trade["price"]), amount(trade["size"])
            notional = price * size
            total += notional
            first, last = first if first is not None else at, at
            counts["live_unique_trades"] += 1
            counts["fee_retaining_collector_records"] += int(type(version) is int and version == 1)
            for side in ("maker", "taker"):
                label = classify(trade, side, version, policy["domains"][venue])
                counts["invalid_fee_fields"] += int(label == "invalid_integer")
                if type(trade.get(side + "_fee")) is int:
                    fee_histogram[side][str(trade[side + "_fee"])] += 1
                group = groups.setdefault(side + "/" + label, {"count": 0, "notional": Decimal(0), "markouts": {}})
                group["count"] += 1
                group["notional"] += notional
                if side == "taker" and marks is not None:
                    for horizon in (1000, 5000):
                        stats = group["markouts"].setdefault(str(horizon), {
                            "count": 0, "missing_count": 0, "notional": Decimal(0),
                            "price_markout_bps_notional_sum": Decimal(0),
                            "mid_move_bps_notional_sum": Decimal(0)})
                        result = marks.mark(at, horizon, price, trade["is_maker_ask"])
                        if result is None:
                            stats["missing_count"] += 1
                        else:
                            stats["count"] += 1
                            stats["notional"] += notional
                            stats["price_markout_bps_notional_sum"] += result[0] * notional
                            stats["mid_move_bps_notional_sum"] += result[1] * notional
    if len(modes) > 1 or books is not None and modes != {True}:
        raise ValueError("mixed clocks or non-precise trades cannot join precise books")
    if digest(archive / "manifest.json") != identity or digest(policy_path) != policy_hash:
        raise ValueError("audit input changed")
    manifest = json.loads((archive / "manifest.json").read_text())
    if any(digest(archive / s["path"]) != s["sha256"] for s in manifest["shards"]):
        raise ValueError("archive changed during audit")
    if books is not None:
        if verify_compiled(archive, books) != lineage or digest(book_config) != lineage["config_sha256"]:
            raise ValueError("book lineage changed")
    for group in groups.values():
        group["notional_fraction"] = str(group["notional"] / total) if total else None
        for stats in group["markouts"].values():
            for metric in ("price_markout", "mid_move"):
                stats[metric + "_bps"] = (str(stats[metric + "_bps_notional_sum"] / stats["notional"])
                                          if stats["notional"] else None)
    # Only aggregates leave the auditor. There are no account or order mappings.
    result = {"schema_version": 1, "scope": "public_flow_fee_signatures_not_strategy_fills",
              "venue": venue, "market": market, "symbol": symbol,
              "window_ms": [start_ms, end_ms], "first_live_ms": first, "last_live_ms": last,
              "archive_manifest_sha256": identity, "fee_policy_sha256": policy_hash,
              "fee_policy": policy,
              "auditor_sha256": digest(Path(__file__)), "counts": dict(counts),
              "total_public_notional": total, "groups": groups, "wire_fee_histograms": fee_histogram,
              "book_lineage": lineage, "max_book_age_ms": max_book_age_ms,
              "markout_clock": "local_availability_plus_horizon", "positive_markout": "adverse_to_passive_side",
              "fee_schedule_effective_period_verified": False, "account_tier_confirmed": False,
              "fee_signature_audit_valid": bool(counts["live_unique_trades"]
                  and not counts["conflicting_duplicates"] and not counts["unacknowledged_records"]
                  and not counts["invalid_fee_fields"]),
              "stream_continuity_verified": False,
              "confirmed_eligible_volume": None, "strategy_fill_eligible_fraction": None,
              "points_cash_value": "0", "execution_qualified": False,
              "limitations": ["Public flow mix is not the counterparty mix of a simulated maker's fills.",
                              "Fee signatures can match a published schedule without proving account tier or points.",
                              "Missing legacy fees cannot be reconstructed; wire omission means zero or empty.",
                              "A 100 fee signature is compatible with Standard special orders, not proof of Premium.",
                              "Markouts are descriptive public-print observations, not cash PnL or calibrated execution latency.",
                              "This audit does not certify continuous ten-hour execution or funding settlement."]}
    return json.loads(json.dumps(result, default=str, allow_nan=False))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("archive", type=Path)
    parser.add_argument("venue", choices=tuple(SOURCES))
    parser.add_argument("market")
    parser.add_argument("policy", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--start-ms", type=int)
    parser.add_argument("--end-ms", type=int)
    parser.add_argument("--books", type=Path)
    parser.add_argument("--book-config", type=Path)
    parser.add_argument("--symbol")
    args = parser.parse_args()
    if args.output.exists():
        parser.error("output already exists")
    os.umask(0o077)
    result = audit(args.archive, args.venue, args.market, args.policy, start_ms=args.start_ms,
                   end_ms=args.end_ms, books=args.books, book_config=args.book_config, symbol=args.symbol)
    write_report(args.output, result)
    print(json.dumps({"live_unique_trades": result["counts"].get("live_unique_trades", 0),
                      "confirmed_eligible_volume": None, "archive_manifest_sha256": result["archive_manifest_sha256"]}))


if __name__ == "__main__":
    main()
