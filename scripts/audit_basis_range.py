#!/usr/bin/env python3
"""Read public books only: receipt-causal basis-range distribution, never orders/PnL.

This is a top-of-book diagnostic, not the engine's full-depth/funding admission.
Time weights expire at book-age or observation-gap limits; missing time is explicit.
"""
import argparse
from collections import Counter, deque
from hashlib import sha256
import json
import math
import os
from pathlib import Path

from archive_public_capture import digest, rows


def bucket(value):
    if value <= 1e-9:
        return "zero_within_1e-9_bps"
    for bound in (0.5, 1, 2, 3):
        if value < bound:
            return f"below_{bound}_bps"
    return "at_least_3_bps"


class RangeAudit:
    def __init__(self, start, end, lookback=15000, sample=100, gap=1000):
        self.start, self.end = start, end
        self.lookback, self.sample, self.gap = lookback, sample, gap
        self.points, self.minimum, self.maximum = deque(), deque(), deque()
        self.identity, self.last_observed = None, None
        self.current, self.expires, self.accounted = None, start, start
        self.weight = Counter()
        self.samples = []
        self.events, self.cleared = Counter(), Counter()

    def account(self, at):
        at = min(at, self.end)
        if self.current is not None:
            dt = max(0, min(at, self.expires) - self.accounted)
            self.weight[bucket(self.current)] += dt
        self.accounted = max(at, self.accounted)

    def reset(self, reason, at):
        self.account(at)
        self.events[reason] += 1
        self.cleared[reason] += bool(self.points)
        self.points.clear()
        self.minimum.clear()
        self.maximum.clear()
        self.current, self.identity, self.last_observed = None, None, None

    def observe(self, at, basis, identity, valid_until):
        self.account(at)
        if self.last_observed is not None and at - self.last_observed > self.gap:
            self.reset("observation_gap", at)
        if identity == self.identity:
            return
        self.identity, self.last_observed = identity, at
        self.expires = min(at + self.gap, valid_until, self.end)
        if self.points and at - self.points[-1][0] < self.sample:
            return
        self.points.append((at, basis))
        while len(self.points) > 1 and self.points[1][0] <= at - self.lookback:
            self.points.popleft()
        first = self.points[0][0]
        for queue, remove in ((self.minimum, lambda v: v >= basis),
                              (self.maximum, lambda v: v <= basis)):
            while queue and queue[0][0] < first:
                queue.popleft()
            while queue and remove(queue[-1][1]):
                queue.pop()
            queue.append((at, basis))
        self.current = (self.maximum[0][1] - self.minimum[0][1]
                        if at - first >= self.lookback else None)
        if self.current is not None:
            self.samples.append(self.current)

    def report(self):
        self.account(self.end)
        values = sorted(self.samples)
        ready = sum(self.weight.values())
        interior = sum(v for k, v in self.weight.items() if k.startswith("below_"))
        return {"ready_time_ms": ready, "unavailable_time_ms": self.end - self.start - ready,
                "ready_fraction_of_window": ready / (self.end - self.start),
                "interior_0_to_3_bps_fraction_of_ready_time": interior / ready if ready else None,
                "range_time_ms": dict(self.weight), "ready_samples": len(values),
                "sample_quantiles_bps": {str(q): values[round((len(values)-1)*q)] if values else None
                                         for q in (0, .1, .25, .5, .75, .9, .99, 1)},
                "reset_events_including_empty_history": dict(self.events),
                "resets_that_cleared_samples": dict(self.cleared)}


def audit(tape, pair, window):
    start, end = window["start_ms"], window["end_ms"]
    state = RangeAudit(start, end)
    online, previous, ended = False, start, False
    last_source = [0, 0]
    max_age = pair["planner"]["max_quote_age_ms"]
    max_skew = pair["planner"]["max_quote_skew_ms"]
    payload = sha256()
    with tape.open("rb") as stream:
        initial = os.fstat(stream.fileno())
        for raw in rows(stream):
            payload.update(raw)
            event = json.loads(raw)
            at, kind = event["at_ms"], event["kind"]
            if ended or not previous <= at <= window["drain_until_ms"]:
                raise ValueError("unordered or out-of-window tape")
            previous = at
            state.account(at)
            if kind == "end":
                ended = at == window["drain_until_ms"]
            if at >= end:
                continue
            if kind == "trade_feed" and event["leg"] == 1:
                online = event["online"]
                if not online:
                    state.reset("trade_feed_offline", at)
            elif kind == "book":
                books = event["books"]
                stamps, mids, valid = [], [], online
                for i, book in enumerate(books):
                    ts = book["timestamp_ms"]
                    if ts > at:
                        raise ValueError("future book")
                    good = (book["market"] == pair["legs"][i]["market"]
                            and book["tradable"] and ts > 0 and ts >= last_source[i]
                            and at - ts <= max_age and bool(book["bids"]) and bool(book["asks"]))
                    if good:
                        bid, ask = (float(book[s][0]["price"]) for s in ("bids", "asks"))
                        good = math.isfinite(bid) and math.isfinite(ask) and 0 < bid < ask
                    if good:
                        last_source[i] = ts
                        mids.append((bid + ask) / 2)
                    valid = valid and good
                    stamps.append(ts)
                valid = valid and len(stamps) == 2 and abs(stamps[0] - stamps[1]) <= max_skew
                if valid:
                    # Target RH is leg 1; positive means target relatively dear.
                    state.observe(at, (mids[1] / mids[0] - 1) * 10000, tuple(stamps), min(stamps) + max_age)
                else:
                    state.reset("invalid_or_unavailable_top_of_book", at)
        final = os.fstat(stream.fileno())
        signature = lambda s: (s.st_ino, s.st_size, s.st_mtime_ns, s.st_ctime_ns)
        if not ended or signature(initial) != signature(final):
            raise ValueError("incomplete or changing input")
    return state.report() | {"input_sha256": payload.hexdigest(),
                            "scope": "top_of_book_float_diagnostic_no_full_depth_or_funding_admission_no_orders"}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("prepared", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    os.umask(0o077)
    manifest = args.prepared / "manifest.json"
    identity = digest(manifest)
    source = json.loads(manifest.read_text())
    if source["sample"] != "historical_selection":
        raise ValueError("this audit may only describe the reused selection sample")
    result = {"sample": "historical_selection", "source_manifest_sha256": identity,
              "runner_sha256": digest(Path(__file__)), "markets": {},
              "basis_convention": "10000*(target_mid/hedge_mid-1); positive_target_dear",
              "parameters": {"lookback_ms": 15000, "sample_ms": 100, "max_gap_ms": 1000}}
    args.output.mkdir()
    for symbol in ("BTC", "ETH"):
        spec = source["markets"][symbol]
        pair_path = args.prepared / spec["pair"]
        if digest(pair_path) != spec["pair_sha256"]:
            raise ValueError("pair changed")
        pair = json.loads(pair_path.read_text())
        if [p["venue"] for p in pair["legs"]] != ["lighter", "lighter_robinhood"]:
            raise ValueError("unexpected basis direction")
        if spec["window"]["end_ms"] - spec["window"]["start_ms"] != 36_000_000:
            raise ValueError("ten-hour decision window required")
        row = audit(args.prepared / spec["input"], pair, spec["window"])
        if row["input_sha256"] != spec["input_sha256"] or digest(manifest) != identity:
            raise ValueError("source identity mismatch")
        result["markets"][symbol] = row
    with (args.output / "report.json").open("x") as out:
        json.dump(result, out, indent=2)


if __name__ == "__main__":
    main()
