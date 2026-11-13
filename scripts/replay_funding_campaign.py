#!/usr/bin/env python3
"""Price funding windows on public settlement rates and delayed recorded depth."""

import argparse
from bisect import bisect_left
from collections import Counter
from decimal import Decimal
import gzip
import hashlib
import json
import os
from pathlib import Path
import subprocess

from replay_pair_campaign import (
    CAPITAL, DAY, HOUR, LATENCY, LEG_CAP, MAX_AGE, common_size, depth_cost,
    fee_cases, load, market_terms, quote_pair, reference_at, save, stock_reference, unseal,
)

RESERVE_BPS = 5.0
FORECAST_BARS = 3


def rate_archive(root):
    first = {}
    conflicts = {}
    for path in sorted((root / "history" / "funding").glob("*.json.gz")):
        with gzip.open(path, "rt") as file:
            sample = json.load(file)
        at = sample["available_at_ms"]
        for item in sample["rates"]:
            slot = item["slot_ms"]
            if slot % HOUR or not slot <= item["source_time_ms"] <= at:
                continue
            rate = Decimal(item["rate_bps"])
            if not rate.is_finite():
                continue
            key = (sample["venue"], sample["market"], slot)
            older = first.get(key)
            if older is None:
                first[key] = (at, rate)
            elif rate != older[1]:
                conflict_at = max(older[0], at)
                conflicts[key] = min(conflicts.get(key, conflict_at), conflict_at)
                if at < older[0]:
                    first[key] = (at, rate)
            elif at < older[0]:
                first[key] = (at, rate)
    return first, conflicts


def identifier(venue, symbol):
    if venue in ("lighter", "hyperliquid"):
        return venue, symbol
    return "hyperliquid", ("io:" if venue == "entropy" else "xyz:") + symbol


def observations_at(terms, slot, visible, first, conflicts):
    keys = [(*term, slot) for term in terms]
    seen = [first.get(key) for key in keys]
    if any(x is None or x[0] > visible or conflicts.get(key, visible + 1) <= visible
           for key, x in zip(keys, seen, strict=True)):
        return None
    return seen


def rates_at(terms, slot, visible, first, conflicts):
    seen = observations_at(terms, slot, visible, first, conflicts)
    return None if seen is None else [float(rate) for _, rate in seen]


def window_grid(start, stop, equity):
    start = max(start, start // HOUR * HOUR + 8 * 60_000)
    for hours in ((1,) if equity else (1, 2, 4, 8)):
        for decision in range(start, stop, hours * HOUR):
            anchor = decision // HOUR * HOUR
            close = anchor + hours * HOUR + 2 * 60_000
            if close + 120_000 < stop:
                yield decision, close, anchor, hours


def capture_frames(stream, past, arrivals, ref_tape):
    past, arrivals = sorted(past), sorted(arrivals)
    found_past, found_arrivals = {}, {}
    for line in stream:
        frame = json.loads(line)
        at = frame["now_ms"]
        i = bisect_left(past, at)
        j = bisect_left(arrivals, at - MAX_AGE)
        if (i == len(past) or past[i] > at + MAX_AGE) and (j == len(arrivals) or arrivals[j] > at):
            continue
        reference = reference_at(frame, ref_tape)
        row = quote_pair(frame, reference)
        if row is None:
            continue
        books, top = row
        observation = (at, books, top, reference)
        while i < len(past) and past[i] <= at + MAX_AGE:
            found_past[past[i]] = observation
            i += 1
        oldest = min(book["timestamp_ms"] for book in books)
        while j < len(arrivals) and arrivals[j] <= oldest:
            found_arrivals.setdefault(arrivals[j], observation)
            j += 1
    return found_past, found_arrivals


def values(observation, qty, terms, direction):
    books = observation[1]
    traded = [depth_cost(book["asks" if sign > 0 else "bids"], qty, float(terms[i][0]), sign > 0)
              for i, (sign, book) in enumerate(zip(direction, books, strict=True))]
    if any(v is None or not terms[i][2] <= v <= LEG_CAP or qty + 1e-10 < terms[i][1]
           for i, v in enumerate(traded)):
        return None
    return traded


def cash(values, direction, fees):
    return sum(-sign * val for sign, val in zip(direction, values, strict=True)), sum(
        val * rate / 10_000 for val, rate in zip(values, fees, strict=True)
    )


def analyze(campaign, source, symbol, metadata, previous, rates):
    equity = symbol not in ("BTC", "ETH")
    start = previous["equity_split"]["holdout_from_ms"] if equity else previous["crypto_cut_ms"]
    stop = previous["equity_split"]["close_ms"] if equity else previous["capture_through_ms"]
    windows = list(window_grid(start, stop, equity))
    past, future = set(), set()
    for decision, close, anchor, hours in windows:
        past.update([decision, *(anchor + n * HOUR for n in range(1, hours + 1))])
        future.update(t + delay for delay in LATENCY for t in (decision, close))
    reference = stock_reference(campaign, symbol) if equity else None
    with unseal(source) as stream:
        header = json.loads(stream.readline())
        venues = [leg["venue"] for leg in header["pair"]["legs"]]
        assert "arcus" not in venues
        specs = [market_terms(metadata, v, symbol) for v in venues]
        snapshots, filled = capture_frames(stream, past, future, reference)
    markets = [identifier(v, symbol) for v in venues]
    first, conflicts = rates
    fees_by_case = fee_cases(metadata, symbol)
    results = {}
    for name in ("public_schedule", "higher_fee"):
        fees = [fees_by_case[name][v] for v in venues]
        for delay in LATENCY:
            checks = []
            conditions = Counter()
            for decision, close, anchor, hours in windows:
                history = [rates_at(markets, anchor - n * HOUR, decision, first, conflicts) for n in (2, 1, 0)]
                slots = [anchor + n * HOUR for n in range(1, hours + 1)]
                settlements = [rates_at(markets, slot, stop, first, conflicts) for slot in slots]
                before, entry, exit_ = snapshots.get(decision), filled.get(decision + delay), filled.get(close + delay)
                marks = [snapshots.get(slot) for slot in slots]
                if any(x is None for x in history + settlements):
                    conditions["missing_public_rates"] += 1
                    continue
                if before is None or entry is None or exit_ is None or any(x is None for x in marks):
                    conditions["missing_delayed_depth_or_mark"] += 1
                    continue
                diffs = [b - a for a, b in history]
                sign = 1 if sum(diffs) >= 0 else -1
                direction, reverse = (sign, -sign), (-sign, sign)
                qty = common_size(specs[0][0], specs[1][0], max(q[1] for q in before[2]) * 1.02)
                candidates = (values(before, qty, specs, direction), values(before, qty, specs, reverse),
                              values(entry, qty, specs, direction), values(exit_, qty, specs, reverse))
                if not qty or any(x is None for x in candidates):
                    conditions["insufficient_common_lot"] += 1
                    continue
                reserve = qty * before[3] * RESERVE_BPS / 10_000
                pairs = [cash(v, direction if i % 2 == 0 else reverse, fees) for i, v in enumerate(candidates)]
                expected = qty * before[3] * hours * min(sign * d for d in diffs) / 10_000
                projected = expected + sum(x - fee for x, fee in pairs[:2]) - reserve
                funding = sum(-direction[i] * rate * qty * (sum(mark[2][i]) / 2) / 10_000
                              for mark, payment in zip(marks, settlements, strict=True) for i, rate in enumerate(payment))
                price_pnl = sum(x for x, _ in pairs[2:])
                fees_paid = sum(fee for _, fee in pairs[2:])
                net = funding + price_pnl - fees_paid - reserve
                checks.append({"net": net, "projected": projected, "funding": funding, "fees": fees_paid,
                               "basis_and_execution_pnl": price_pnl, "reserve": reserve, "hours": hours,
                               "hold_ms": exit_[0] - entry[0]})
                conditions["estimated"] += 1
            best = max(checks, key=lambda x: x["net"], default=None)
            results[f"{name}-{delay}ms"] = {
                "conditions": dict(conditions), "positive": sum(x["net"] > 0 for x in checks),
                "pre_trade_cost_gate_passed": sum(x["projected"] > 0 for x in checks),
                "best_window": best,
                "best_window_linear_daily_pct": None if best is None else
                    100 * best["net"] / CAPITAL * (6.5 * HOUR if equity else DAY) / best["hold_ms"],
            }
    return {"symbol": symbol, "venues": venues, "candidate_windows": len(windows), "results": results}


def carry_forecasts(terms, now, last_payment, rates):
    anchor = now // HOUR * HOUR
    history = [observations_at(terms, anchor - n * HOUR, now, *rates) for n in range(FORECAST_BARS)]
    if any(row is None for row in history):
        return None
    payments = range(anchor + HOUR, last_payment + 1, HOUR)
    forecasts = []
    for observations in zip(*history, strict=True):
        values = [rate for _, rate in observations]
        low, high = min(values), max(values)
        forecasts.append((max(at for at, _ in observations), [{
            "at_ms": at,
            "rate": str((high + low) / 20_000),
            "uncertainty_bps": str((high - low) / 2),
        } for at in payments]))
    return forecasts


def native_input(source, dest, metadata, previous, rates):
    symbol = source.name.split("-")[0]
    equity = symbol in ("NVDA", "TSLA")
    session = previous["equity_split"]
    stop = session["close_ms"] if equity else previous["capture_through_ms"]
    horizon = HOUR if equity else 8 * HOUR
    with unseal(source) as stream, dest.open("x") as output:
        header = json.loads(stream.readline())
        pair = header["pair"]
        legs = pair["legs"]
        fees = fee_cases(metadata, symbol)["public_schedule"]
        terms = [identifier(leg["venue"], symbol) for leg in legs]
        for leg in legs:
            if leg["funding"] != {"interval_ms": HOUR, "offset_ms": 0}:
                raise ValueError("carry forecast requires the verified hourly UTC schedule")
            leg["taker_fee_bps"] = str(fees[leg["venue"]])
        pair.update(strategy="funding_carry", optimize_funding_horizon=True, mode={"kind": "pair_hedge"})
        pair["planner"].update(max_funding_age_ms=HOUR, funding_horizon_ms=horizon,
                               exit_basis_reserve_bps=RESERVE_BPS, depth_fraction=0.20,
                               max_leg_notional_usd=LEG_CAP, max_unhedged_notional_usd=LEG_CAP)
        header.update(funding_guard_ms=1000,
                      trade_from_ms=session["holdout_from_ms"] if equity else previous["crypto_cut_ms"])
        json.dump(header, output, separators=(",", ":"))
        output.write("\n")
        ready = 0
        last_payment = stop - max(120_000, pair["equity_close_buffer_ms"]) - pair["execution_window_ms"]
        for line in stream:
            frame = json.loads(line)
            now = frame["now_ms"]
            if now >= stop:
                break
            forecasts = None
            if now >= header["trade_from_ms"]:
                forecasts = carry_forecasts(terms, now, min(last_payment, now + horizon), rates)
            if forecasts and all(payments for _, payments in forecasts):
                ready += 1
            for i, leg in enumerate(frame["legs"]):
                leg["forecast"] = None
                if forecasts and forecasts[i][1]:
                    observed, payments = forecasts[i]
                    leg["forecast"] = {
                        "observed_at_ms": observed,
                        "payments": [point | {"oracle_price": leg["oracle"]["price"]} for point in payments],
                    }
            # A held leg crossing a slot still requires verified settlement evidence.
            json.dump(frame, output, separators=(",", ":"))
            output.write("\n")
        # Drain the decoder even when the evaluation cutoff precedes the recording end.
        for _ in stream:
            pass
    return ready


def native_run(engine, tape, out, delay):
    output = out / f"{tape.stem}-{delay}ms.json"
    input_link = output.with_suffix(".ndjson")
    input_link.symlink_to("/dev/stdin")
    error = output.with_suffix(".stderr")
    try:
        with tape.open("rb") as stream, output.open("xb") as stdout, error.open("xb") as stderr:
            header = json.loads(stream.readline())
            header["one_way_latency_ms"] = delay
            with subprocess.Popen([str(engine), "perp-replay", str(input_link)], stdin=subprocess.PIPE,
                                  stdout=stdout, stderr=stderr) as proc:
                try:
                    with proc.stdin:
                        proc.stdin.write((json.dumps(header) + "\n").encode())
                        while block := stream.read(1 << 20):
                            proc.stdin.write(block)
                except BrokenPipeError:
                    pass
                code = proc.wait(timeout=180)
    finally:
        input_link.unlink()
    if code:
        reason = "missing_realized_funding" if b"missing realized funding" in error.read_bytes() else f"perp_replay_exit_{code}"
        return {"error": reason}
    report = load(output)
    keys = ("training_frames", "holdout_basis_ready_frames", "entry_signals", "entries_submitted",
            "paired_entries", "ioc_filled", "ioc_partial", "fees", "funding", "entry_diagnostics",
            "max_unhedged_base", "max_unhedged_ms", "open_positions_base", "final_liquidation_pnl")
    return {key: report[key] for key in keys}


def native_carry(sources, engine, equity_results, out, metadata, previous, rates, revision):
    out.mkdir(mode=0o700)
    reports = {}
    for source in sources:
        label = source.name.removesuffix(".ndjson.zst")
        if label.startswith(("NVDA-", "TSLA-")):
            source = equity_results / source.name
        tape = out / f"{label}.ndjson"
        ready = native_input(source, tape, metadata, previous, rates)
        cases = {f"{delay}ms": native_run(engine, tape, out, delay) for delay in LATENCY}
        subprocess.run(["zstd", "-q", "-T1", "-3", "--rm", str(tape)], check=True, timeout=180)
        reports[label] = {"frames_with_rate_history": ready, "cases": cases}
    with engine.open("rb") as stream:
        engine_sha = hashlib.file_digest(stream, "sha256").hexdigest()
    save(out / "report.json", {"revision": revision, "engine_sha256": engine_sha,
                               "reserve_bps": RESERVE_BPS, "pair_capital": CAPITAL,
                               "rate_history_payments": FORECAST_BARS, "routes": reports})


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("campaign", type=Path)
    parser.add_argument("metadata", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--engine", type=Path)
    parser.add_argument("--equity-results", type=Path)
    parser.add_argument("--revision")
    args = parser.parse_args()
    if any((args.engine, args.equity_results, args.revision)) and not all(
        (args.engine, args.equity_results, args.revision)
    ):
        parser.error("native carry requires --engine, --equity-results and --revision")
    os.umask(0o077)
    root = args.campaign.resolve()
    final = root / "results" / "final"
    previous = load(final / "report.json")
    metadata = load(args.metadata)
    sources = [final / f"{s}-freshest.ndjson.zst" for s in ("BTC", "ETH")]
    sources += [final / f"{r}.ndjson.zst" for r in sorted(previous["routes"])
                if set(r.split("-")[1:]) <= {"lighter", "entropy", "trade_xyz"}]
    rates = rate_archive(root)
    if args.engine:
        native_carry(sources, args.engine.resolve(), args.equity_results.resolve(),
                     args.output.resolve(), metadata, previous, rates, args.revision)
        return
    result = {p.name.removesuffix(".ndjson.zst"): analyze(root, p, p.name.split("-")[0], metadata, previous, rates)
              for p in sources}
    save(args.output, {"funding_mark": "last fresh public book midpoint before settlement", "risk_reserve_bps": RESERVE_BPS,
                       "fee_snapshot": metadata["snapshot_stamp"], "pair_capital": CAPITAL, "routes": result})
    print(json.dumps({"funding_routes": len(result)}))


if __name__ == "__main__":
    main()
