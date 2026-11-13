#!/usr/bin/env python3
"""Measure a fixed-size pair opportunity ceiling and run the Rust paper engine."""

import argparse
from bisect import bisect_right
from collections import deque
from concurrent.futures import ThreadPoolExecutor
from decimal import Decimal
import gzip
from itertools import combinations
import json
import math
import os
from pathlib import Path
import shutil
import subprocess
from typing import NamedTuple

from replay_maker_campaign import DAY, HOUR, LATENCY, load, save, unseal

VENUES = ("lighter", "hyperliquid", "arcus", "entropy", "trade_xyz")
SYMBOLS = ("BTC", "ETH", "NVDA", "TSLA")
CAPITAL = 200
LEG_CAP = CAPITAL / 2
MAX_AGE = 2200
MAX_SKEW = 750
MIN_HOLD = 1000
MAX_HOLD = 60_000
PARTICIPATION = 0.20
FEE_CASES = {
    "public_schedule": {"lighter": 0.0, "hyperliquid": 4.5, "entropy": 0.9, "trade_xyz": 0.9},
    "arcus_zero_sensitivity": {"lighter": 0.0, "hyperliquid": 4.5, "arcus": 0.0, "entropy": 0.9, "trade_xyz": 0.9},
    "higher_fee": {"lighter": 4.0, "hyperliquid": 4.5, "arcus": 4.5, "entropy": 9.0, "trade_xyz": 9.0},
}


class Cash(NamedTuple):
    pnl: float = 0.0
    turnover: float = 0.0
    fees: float = 0.0
    cycles: int = 0

    def fill(self, gross, turnover, fees, closed=False):
        return Cash(self.pnl + gross - fees, self.turnover + turnover, self.fees + fees, self.cycles + int(closed))


class Ceiling:
    """Best flat cash after nonoverlapping, fixed-size, hindsight round trips."""

    def __init__(self):
        self.flat_times = [0]
        self.flat = [Cash()]
        self.pending = [deque(), deque()]
        self.open = [deque(), deque()]
        self.hour = None

    def step(self, at, decision, actions):
        hour = at // HOUR
        if hour != self.hour:
            for queue in (*self.pending, *self.open):
                queue.clear()
            self.hour = hour
        if decision // HOUR != hour:
            return
        best = self.flat[-1]
        before_open = self.flat[bisect_right(self.flat_times, decision) - 1]
        for side, (pending, active) in enumerate(zip(self.pending, self.open, strict=True)):
            while pending and pending[0][0] <= decision - MIN_HOLD:
                time, opened = pending.popleft()
                while active and active[-1][1].pnl <= opened.pnl:
                    active.pop()
                active.append((time, opened))
            while active and active[0][0] < at - MAX_HOLD:
                active.popleft()
            if active:
                settled = active[0][1].fill(*actions[1 - side], closed=True)
                if settled.pnl > best.pnl + 1e-9:
                    best = settled
            pending.append((at, before_open.fill(*actions[side])))
        self.flat_times.append(at)
        self.flat.append(best)

    def result(self, duration, equity):
        row = self.flat[-1]
        day = 6.5 * HOUR if equity else DAY
        return row._asdict() | {"linear_daily_pct": 100 * row.pnl / CAPITAL * day / duration}


def market_terms(metadata, venue, symbol):
    if venue == "lighter":
        r = metadata["captured_lighter"][symbol]
        return Decimal(1).scaleb(-r["size_decimals"]), float(r["min_base_amount"]), float(r["min_quote_amount"])
    if venue == "arcus":
        r = next(r for r in metadata["arcus"] if r["baseAsset"] == symbol)
        return Decimal(r["stepSize"]), float(r["minOrderSize"]), float(r["minOrderNotional"])
    market = {"hyperliquid": symbol, "entropy": f"io:{symbol}", "trade_xyz": f"xyz:{symbol}"}[venue]
    key = {"hyperliquid": "native", "entropy": "entropy", "trade_xyz": "tradexyz"}[venue]
    r = next(r for r in metadata["hyperliquid"][key]["assets"] if r["name"] == market)
    return Decimal(1).scaleb(-r["szDecimals"]), 0.0, 10.0


def fee_cases(metadata, symbol):
    arcus = next(r for r in metadata["arcus"] if r["baseAsset"] == symbol)
    return {name: {"arcus": float(arcus["effectiveTakerFeePpm"]) / 100.0} | rates
            for name, rates in FEE_CASES.items()}


def common_size(left, right, price):
    scale = max(-left.as_tuple().exponent, -right.as_tuple().exponent)
    step = Decimal(math.lcm(int(left.scaleb(scale)), int(right.scaleb(scale)))).scaleb(-scale)
    return float((Decimal(str(LEG_CAP / price)) // step) * step)


def depth_cost(levels, quantity, step, buy):
    cap = float(levels[0]["price"]) * (1.0015 if buy else 0.9985)
    value = 0.0
    remaining = quantity
    for level in levels[:20]:
        px = float(level["price"])
        if (buy and px > cap) or (not buy and px < cap):
            break
        lot = math.floor(float(level["contracts"]) * PARTICIPATION / step + 1e-9) * step
        take = min(remaining, lot)
        value += px * take
        remaining -= take
        if remaining < step * 1e-6:
            return value
    return None


def quote_pair(frame, ref):
    now = frame["now_ms"]
    if ref is None or ref <= 0:
        return None
    books = [leg["book"] for leg in frame["legs"]]
    if any(not b["tradable"] or not 0 < b["timestamp_ms"] <= now
           or now - b["timestamp_ms"] > MAX_AGE or not (b["bids"] and b["asks"]) for b in books):
        return None
    if abs(books[0]["timestamp_ms"] - books[1]["timestamp_ms"]) > MAX_SKEW:
        return None
    bid_ask = [[float(b[side][0]["price"]) for side in ("bids", "asks")] for b in books]
    if any(bid <= 0 or ask <= bid or abs((bid + ask) / (2 * ref) - 1) > 0.005 for bid, ask in bid_ask):
        return None
    return books, bid_ask


def stock_reference(campaign, symbol):
    data = []
    with gzip.open(campaign / "history" / "nasdaq-first-observed.jsonl.gz", "rt") as stream:
        for line in stream:
            r = json.loads(line)
            if r["symbol"] == symbol:
                data.append((r["observed_at_ms"], r["bar_end_ms"], float(r["close"])))
    data.sort()
    latest = []
    for item in data:
        if not latest or item[1] > latest[-1][1]:
            latest.append(item)
    return [row[0] for row in latest], latest


def reference_at(frame, ref_tape):
    now = frame["now_ms"]
    if ref_tape is None:
        ref = frame["reference"]
        return float(ref["price"]) if 0 < ref["timestamp_ms"] <= now and now - ref["timestamp_ms"] <= MAX_AGE else None
    avail, bars = ref_tape
    i = bisect_right(avail, now) - 1
    return bars[i][2] if i >= 0 and 0 <= now - bars[i][1] <= 180_000 else None


def screen(campaign, metadata, report, source):
    symbol = source.name.split("-")[0]
    equity = symbol in ("NVDA", "TSLA")
    start = report["equity_split"]["holdout_from_ms"] if equity else report["crypto_cut_ms"]
    stop = report["equity_split"]["close_ms"] if equity else report["capture_through_ms"]
    ref_tape = stock_reference(campaign, symbol) if equity else None
    fees = fee_cases(metadata, symbol)
    trials = {(name, delay): Ceiling() for name in FEE_CASES for delay in LATENCY}
    decisions = []
    last_decision = {d: 0 for d in LATENCY}
    counts = {d: 0 for d in LATENCY}
    qty = None
    synchronized = 0
    valid_depth = 0
    with unseal(source) as stream:
        header = json.loads(stream.readline())
        legs = tuple(leg["venue"] for leg in header["pair"]["legs"])
        terms = [market_terms(metadata, leg, symbol) for leg in legs]
        for line in stream:
            frame = json.loads(line)
            now = frame["now_ms"]
            valid = quote_pair(frame, reference_at(frame, ref_tape))
            if valid is None:
                continue
            books, top = valid
            if now < start and qty is None:
                qty = common_size(terms[0][0], terms[1][0], max(p[1] for p in top) * 1.02)
            if not start <= now < stop or not qty:
                continue
            synchronized += 1
            decisions.append(now)
            costs = [depth_cost(book[side], qty, float(terms[i][0]), side == "asks")
                     for i, book in enumerate(books) for side in ("bids", "asks")]
            if any(cost is None or not terms[i // 2][2] <= cost <= LEG_CAP
                   or qty + 1e-10 < terms[i // 2][1] for i, cost in enumerate(costs)):
                continue
            valid_depth += 1
            bid0, ask0, bid1, ask1 = costs
            for delay in LATENCY:
                latest = min(b["timestamp_ms"] for b in books) - delay
                i = bisect_right(decisions, latest) - 1
                if i < 0 or now - decisions[i] > MAX_AGE + delay or decisions[i] < last_decision[delay]:
                    continue
                last_decision[delay] = decisions[i]
                counts[delay] += 1
                for name, rates in fees.items():
                    f0, f1 = (rates[leg] / 10_000 for leg in legs)
                    actions = ((bid1 - ask0, bid1 + ask0, ask0 * f0 + bid1 * f1),
                               (bid0 - ask1, bid0 + ask1, bid0 * f0 + ask1 * f1))
                    trials[name, delay].step(now, decisions[i], actions)
    return {"symbol": symbol, "venues": legs, "holdout_hours": (stop - start) / HOUR, "size_base": qty,
            "synchronized_frames": synchronized, "size_and_depth_frames": valid_depth, "latency_frames": counts,
            "assumptions": "fixed matched linear base exposure at par; current public schedules; avoid hourly funding boundaries",
            "ceilings": {f"{name}-{delay}ms": trial.result(stop - start, equity)
                         for (name, delay), trial in trials.items()}}


def native_case(engine, out, source, report, fee_case, strategy, delay, train):
    symbol = source.name.split("-")[0]
    equity = symbol not in ("BTC", "ETH")
    session = report["equity_split"]
    cut = session["holdout_from_ms"] if equity else report["crypto_cut_ms"]
    key = source.name.removesuffix(".ndjson.zst")
    label = f"{key}-{fee_case}-{strategy}-{delay}ms-" + ("train" if train else "holdout")
    dest = out / "native"
    path = dest / f"{label}.ndjson"
    stdout = dest / f"{label}.json"
    path.symlink_to("/dev/stdin")
    try:
        with unseal(source) as stream, stdout.open("xb") as results, (dest / f"{label}.stderr").open("xb") as err:
            header = json.loads(stream.readline())
            header["one_way_latency_ms"] = delay
            header["trade_from_ms"] = None if train else cut
            header["funding_guard_ms"] = 1000
            pair = header["pair"]
            for leg in pair["legs"]:
                leg["taker_fee_bps"] = str(FEE_CASES[fee_case][leg["venue"]])
            if strategy == "cost_aware":
                pair["basis"].update(entry_z=1.75, max_sample_gap_ms=15_000)
            elif strategy == "turnover":
                pair["basis"].update(entry_z=1.75, max_sample_gap_ms=15_000)
                pair["mode"] = {"kind": "turnover", "max_turnover_quote": "2000", "max_fees_quote": "1",
                                "max_realized_loss_quote": "0.50"}
            save(dest / f"{label}.header.json", header)
            with subprocess.Popen([str(engine), "perp-replay", str(path)], stdin=subprocess.PIPE, stdout=results,
                                  stderr=err) as proc:
                try:
                    proc.stdin.write((json.dumps(header) + "\n").encode())
                    if train:
                        for line in stream:
                            if json.loads(line)["now_ms"] >= cut:
                                break
                            proc.stdin.write(line)
                        for _ in stream:
                            pass
                    else:
                        shutil.copyfileobj(stream, proc.stdin, length=1 << 20)
                except BrokenPipeError:
                    for _ in stream:
                        pass
                finally:
                    try:
                        proc.stdin.close()
                    except BrokenPipeError:
                        pass
                code = proc.wait(timeout=150)
        if code:
            return label, {"error": f"perp_replay_exit_{code}"}
    finally:
        path.unlink()
    r = load(stdout)
    fields = ("training_frames", "holdout_basis_ready_frames", "entry_signals", "entries_submitted", "paired_entries",
              "ioc_filled", "ioc_partial", "gross_turnover", "fees", "funding", "max_unhedged_base", "max_unhedged_ms",
              "max_observed_drawdown", "halted", "open_positions_base", "final_liquidation_pnl", "entry_diagnostics")
    net = r["final_liquidation_pnl"]
    comparable = net is not None and all(Decimal(p) == 0 for p in r["open_positions_base"])
    first = max(report["capture_from_ms"], session["open_ms"]) if equity else report["capture_from_ms"]
    last = session["close_ms"] if equity else report["capture_through_ms"]
    duration = cut - first if train else last - cut
    day = 6.5 * HOUR if equity else DAY
    volume = float(r["gross_turnover"])
    summary = {k: r[k] for k in fields}
    return label, summary | {
        "closed_pairs": len(r["trades"]),
        "linear_daily_pct": float(net) / CAPITAL * day / duration * 100 if comparable else None,
        "net_cost_bps": -float(net) * 1e4 / volume if volume and comparable else None,
    }


def equity_config(root, metadata, previous, template, label):
    symbol, *venues = label.split("-")
    evidence = previous["equity_evidence"]
    if not evidence["calendar"].get("calendar_page_verified") or any(
        evidence[k]["condition"] != "downloaded" or symbol in evidence[k].get("target_symbols", ())
        for k in ("halts", "splits", "dividends")
    ):
        raise ValueError(f"unclear regular-session evidence for {symbol}")
    cfg = load(template)
    pair = cfg["pair"]
    pair.update(equity_minute_reference_max_age_ms=180_000,
                allow_shared_risk_domain=set(venues) == {"entropy", "trade_xyz"})
    pair["planner"].update(max_leg_notional_usd=LEG_CAP, max_unhedged_notional_usd=LEG_CAP)
    terms = []
    configs = []
    fees = fee_cases(metadata, symbol)["public_schedule"]
    for venue in venues:
        step, minimum, min_notional = market_terms(metadata, venue, symbol)
        market = symbol if venue == "lighter" else ("io:" if venue == "entropy" else "xyz:") + symbol
        venue_cfg = {"available_notional": str(LEG_CAP), "external_oracle": True}
        if venue == "lighter":
            observed = metadata["captured_lighter"][symbol]
            tick = format(Decimal(1).scaleb(-observed["price_decimals"]), "f")
            venue_cfg.update(lighter_market_id=observed["id"],
                             lighter_clock={"max_rtt_ms": 600, "valid_for_ms": 300_000, "drift_margin_ms": 5})
        else:
            # The captured NVDA/TSLA prices are in the three-digit band.
            tick = "0.01"
            venue_cfg.update(accept_receipt_time_for_oracle=True, book_mode="freshest")
        terms.append(pair["legs"][0] | {
            "venue": venue, "market": market, "underlying_id": f"NASDAQ:{symbol}",
            "payoff_id": f"RESEARCH-ASSUMPTION:{symbol}", "tick": tick,
            "lot": format(step, "f"), "min_contracts": str(max(float(step), minimum)),
            "min_notional": str(min_notional), "taker_fee_bps": str(fees[venue]),
        })
        configs.append(venue_cfg)
    pair["legs"] = terms
    session = previous["equity_split"]
    open_ms, close_ms = session["open_ms"], session["close_ms"]
    _, tape = stock_reference(root, symbol)
    bars = [{"ends_at_ms": end, "available_at_ms": available, "close": str(price)}
            for available, end, price in tape if open_ms < end <= close_ms and available <= close_ms]
    cfg.update(legs=configs, paper_entries=True, funding_guard_ms=1000,
               reference={"kind": "observed_equity_minute_bars", "underlying_id": f"NASDAQ:{symbol}", "bars": bars},
               equity_sessions=[{"observed_at_ms": open_ms, "opens_at_ms": open_ms, "closes_at_ms": close_ms,
                                 "clear_until_ms": close_ms, "halted": False}])
    return cfg


def compile_equity(engine, root, metadata, previous, template, out, label):
    symbol = label.split("-")[0]
    config = out / f"{label}.config.json"
    replay = out / f"{label}.ndjson"
    sealed = out / f"{label}.ndjson.zst"
    save(config, equity_config(root, metadata, previous, template, label))
    with (out / f"{label}.coverage.json").open("xb") as coverage, (out / f"{label}.compile.stderr").open("xb") as err:
        archive = root / "results" / "final" / f"capture-{symbol}.ndjson.gz"
        with subprocess.Popen(["gzip", "-dc", str(archive)], stdout=subprocess.PIPE, stderr=err) as decoder:
            code = subprocess.run([str(engine), "perp-compile", str(config), "/dev/stdin", str(replay)],
                                  stdin=decoder.stdout, stdout=coverage, stderr=err, timeout=300).returncode
            decoder.stdout.close()
            if decoder.wait(timeout=30) or code:
                raise RuntimeError(f"equity compile failed for {label}")
    subprocess.run(["zstd", "-q", "-T1", "-3", str(replay), "-o", str(sealed)], check=True, timeout=120)
    replay.unlink()
    return sealed


def equity_study(engine, root, metadata, previous, template, out, revision):
    labels = sorted(label for label in previous["routes"] if label.startswith(("NVDA-", "TSLA-"))
                    and set(label.split("-")[1:]) <= {"lighter", "entropy", "trade_xyz"})
    sources = [compile_equity(engine, root, metadata, previous, template, out, label) for label in labels]
    result = {}
    with ThreadPoolExecutor(max_workers=2) as workers:
        jobs = [workers.submit(native_case, engine, out, source, previous, fee, strategy, delay, False)
                for source in sources for fee in ("public_schedule", "higher_fee")
                for strategy in ("baseline", "cost_aware", "turnover") for delay in LATENCY]
        for job in jobs:
            name, row = job.result()
            result[name] = row
    save(out / "report.json", {"revision": revision, "scope": "conditional linear equity regular-session simulation",
                              "routes": labels, "native_paper": result})
    print(json.dumps({"equity_routes": len(labels), "native_cases": len(result)}))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("campaign", type=Path)
    parser.add_argument("metadata", type=Path)
    parser.add_argument("engine", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--revision", required=True)
    parser.add_argument("--equity-template", type=Path)
    args = parser.parse_args()
    os.umask(0o077)
    root = args.campaign.resolve()
    out = args.output.resolve()
    out.mkdir(mode=0o700)
    (out / "native").mkdir()
    metadata = load(args.metadata)
    final = root / "results" / "final"
    previous = load(final / "report.json")
    if args.equity_template:
        equity_study(args.engine.resolve(), root, metadata, previous, args.equity_template, out, args.revision)
        return
    sources = [final / f"{symbol}-freshest.ndjson.zst" for symbol in ("BTC", "ETH")]
    routes = sorted(k for k in previous["routes"] if set(k.split("-")[1:]) <= set(VENUES))
    sources.extend(final / f"{label}.ndjson.zst" for label in routes)
    save(out / "policy.json", {"revision": args.revision, "capital_quote": CAPITAL, "source_commit": previous["commit"],
                              "fees_taker_bps": {s: fee_cases(metadata, s) for s in SYMBOLS},
                              "market_snapshot": metadata["snapshot_stamp"],
                              "latency_ms": LATENCY, "hold_ms": [MIN_HOLD, MAX_HOLD], "depth_fraction": PARTICIPATION})
    rows = {}
    with ThreadPoolExecutor(max_workers=2) as workers:
        futures = {source: workers.submit(screen, root, metadata, previous, source) for source in sources}
        for source, future in futures.items():
            rows[source.name.removesuffix(".ndjson.zst")] = future.result()
    save(out / "ceilings.json", rows)
    causal = {}
    with ThreadPoolExecutor(max_workers=2) as workers:
        jobs = [workers.submit(native_case, args.engine.resolve(), out, source, previous, fee, strategy, delay, train)
                for source in sources[:2] for fee in ("public_schedule", "higher_fee") for delay in LATENCY
                for strategy in ("baseline", "cost_aware", "turnover") for train in (True, False)]
        for job in jobs:
            name, row = job.result()
            causal[name] = row
    available = {tuple(sorted(r["venues"])) for r in rows.values()}
    missing = [list(pair) for pair in combinations(sorted(VENUES), 2) if pair not in available]
    save(out / "report.json", {"revision": args.revision, "venue_pairs_without_matched_symbol": missing,
                              "capital_quote": CAPITAL, "ceilings": rows, "native_paper": causal})
    print(json.dumps({"report_ready": True, "routes_with_data": len(rows), "causal_trials": len(causal)}))


if __name__ == "__main__":
    main()
