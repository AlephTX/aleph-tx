#!/usr/bin/env python3
"""Run directional maker/hedge comparisons from compiled books and public prints."""

import argparse
from collections import Counter
from concurrent.futures import ThreadPoolExecutor
from decimal import Decimal
from hashlib import file_digest
from heapq import merge
import gzip
from itertools import product
import json
import os
from pathlib import Path
import subprocess

from replay_maker_campaign import HOUR, load, save, unseal

MAKER_AND_TAKER_BPS = {
    "standard": ("0", "0"),
    "premium": ("0.4", "2.8"),
    "hyperliquid": ("1.5", "4.5"),
}


def digest(path):
    with path.open("rb") as file:
        return file_digest(file, "sha256").hexdigest()


def book_rows(stream, start, end):
    for line in stream:
        frame = json.loads(line)
        when = frame["now_ms"]
        if start <= when < end:
            # Simulation uses a fixed per-leg allowance. Coverage-only
            # compiler output carries no trading capacity.
            books = [leg["book"] | {"available_notional": "150"} for leg in frame["legs"]]
            yield {"kind": "book", "at_ms": when, "books": books}


def trade_rows(files, market, leg, end):
    seen = set()
    previous = 0
    for path in files:
        with gzip.open(path, "rb") as stream:
            for line in stream:
                row = json.loads(line)
                when = row["received_at_ms"]
                if when >= end:
                    continue
                if when < previous:
                    raise ValueError("out-of-order public tape")
                previous = when
                if row["kind"] == "feed_state":
                    if row["state"] not in ("connected", "disconnected"):
                        raise ValueError("unknown public feed state")
                    yield {"kind": "trade_feed", "at_ms": when, "leg": leg, "online": row["state"] == "connected"}
                    continue
                if row.get("kind") != "trade" or row.get("market") != market:
                    continue
                # Deduplicate before the replay window as subscriptions may resend recent prints.
                source = row["source_time_ms"]
                identity = row.get("trade_id") or (
                    source, row.get("side"), row["price"], row["size"]
                )
                if identity in seen:
                    continue
                seen.add(identity)
                if row["side"] in ("buy", "sell") and 0 < source <= when:
                    yield {"kind": "trade", "at_ms": when, "source_ms": source, "leg": leg,
                           "aggressor": row["side"], "price": row["price"], "contracts": row["size"]}


def windows(begin, holdout, end):
    for slot in range(begin, end, HOUR):
        yield (slot + 2_000, slot + HOUR - 2_000, "comparison" if slot < holdout else "holdout")


def write_inputs(route, capture, compiled, out, periods, until, *, trade_files=None):
    source_path = compiled / f"{route}.ndjson.zst"
    start = periods[0][0]
    info = {}
    with unseal(source_path) as stream:
        pair = json.loads(stream.readline())["pair"]
        def tape(venue, market, leg):
            source = "hyperliquid" if venue in ("entropy", "trade_xyz") else venue
            files = (trade_files[source] if trade_files is not None
                     else sorted((capture / "raw").glob(f"{source}-trades-*.jsonl.gz")))
            if not files:
                raise ValueError(f"missing public trade archive for {source}")
            info[source] = {p.name: digest(p) for p in files}
            return trade_rows(files, market, leg, until)
        feeds = [tape(t["venue"], t["market"], i) for i, t in enumerate(pair["legs"])]
        # Public prints at equal receipt times run before a new quote can use the book.
        timeline = merge(*feeds, book_rows(stream, start, until),
                         key=lambda event: (event["at_ms"], event["kind"] == "book"))
        event = next(timeline, None)
        online = [False, False]
        counts = Counter()
        for begin, end, _ in periods:
            while event is not None and event["at_ms"] < begin:
                if event["kind"] == "trade_feed":
                    online[event["leg"]] = event["online"]
                elif event["kind"] == "trade":
                    online[event["leg"]] = True
                event = next(timeline, None)
            path = out / f"{route}-{begin}.ndjson"
            with path.open("x") as file:
                def emit(row):
                    file.write(json.dumps(row, separators=(",", ":"), allow_nan=False) + "\n")
                for i, active in enumerate(online):
                    emit({"kind": "trade_feed", "at_ms": begin, "leg": i, "online": active})
                while event is not None and event["at_ms"] < end:
                    kind = event["kind"]
                    if kind == "trade_feed":
                        online[event["leg"]] = event["online"]
                    elif kind == "trade":
                        counts[event["leg"]] += 1
                        if not online[event["leg"]]:
                            online[event["leg"]] = True
                            emit({"kind": "trade_feed", "at_ms": event["at_ms"], "leg": event["leg"], "online": True})
                    emit(event)
                    event = next(timeline, None)
                emit({"kind": "end", "at_ms": end})
        for _ in timeline:
            pass
    return pair, {"public_trade_files_sha256": info, "public_trades": [counts[0], counts[1]],
                  "compiled_sha256": digest(source_path)}


def cases(pair, arcus_fees):
    venues = [leg["venue"] for leg in pair["legs"]]
    tiers = ("standard", "premium") if "lighter" in venues else ("base",)
    for tier, maker, delay, gated in product(tiers, (0, 1), (250, 500), (False, True)):
        legs = []
        fees = {}
        for terms in pair["legs"]:
            venue = terms["venue"]
            fees[venue] = arcus_fees if venue == "arcus" else MAKER_AND_TAKER_BPS[tier if venue == "lighter" else venue]
            legs.append(terms | {"taker_fee_bps": fees[venue][1]})
        maker_venue = venues[maker]
        taker_delays = [((300 if tier == "standard" else 140) if v == "lighter" else 0) for v in venues]
        yield f"{maker_venue}-{tier}-{delay}ms-{'edge' if gated else 'touch'}", {
            "pair": pair | {"legs": legs},
            "maker_leg": maker, "transport_ms": delay, "fill_notice_ms": 100,
            "maker_cancel_delay_ms": 300 if maker_venue == "lighter" and tier == "standard" else 0,
            "taker_delay_ms": taker_delays, "trade_clock_guard_ms": 100,
            "trade_delivery_grace_ms": 750, "quote_lifetime_ms": 800,
            "reprice_ms": 100, "hold_ms": 15_000, "unhedged_timeout_ms": 2_500,
            "drain_ms": 15_000, "require_net_edge": gated,
            "maker_fee_bps": fees[maker_venue][0], "quote_notional": "100",
            "latency_reserve_bps": "1",
        }


def summarize(rows):
    fields = ("maker_fills", "maker_buy_fills", "maker_sell_fills", "passive_offset_fills", "passive_only_cycles",
              "ioc_fills", "ioc_submitted", "ioc_partial", "ioc_expired", "ioc_price_misses",
              "ioc_depth_misses", "closed_cycles", "forced_closes", "exposure_stops", "post_only_rejected",
              "quotes_submitted", "quotes_accepted", "late_trade_prints", "below_minimum_hedge",
              "quote_expired_before_activation", "quote_cancelled_before_activation", "maker_prints_touching_quote",
              "below_minimum_unwind", "synchronized_book_frames", "book_frames", "total_unhedged_ms",
              "unhedged_episodes")
    valid = [r for r in rows if r.get("net_pnl") is not None]
    duration = sum(row["end_ms"]-row["start_ms"] for row in rows)
    hours = duration / HOUR
    totals = {name: sum(row.get(name, 0) for row in rows) for name in fields}
    numerics = ("fees", "maker_fees", "maker_turnover", "taker_turnover", "maker_spread_capture",
                "ioc_slippage", "ioc_depth_impact", "ioc_submit_touch_turnover", "forced_close_pnl", "realized_pnl",
                "passive_only_pnl", "passive_only_maker_turnover")
    totals |= {name: sum(float(row.get(name, 0)) for row in rows) for name in numerics}
    reasons = Counter()
    blocked = Counter()
    for row in rows:
        if row.get("incomplete"):
            reasons[row["incomplete"]] += 1
        blocked.update(row.get("entry_blocked", {}))
    net = sum(float(row["net_pnl"]) for row in valid) if len(valid)==len(rows) else None
    gross = sum(float(row["pnl_before_fees"]) for row in valid) if net is not None else None
    best = [Decimal(row["best_modeled_edge_bps"]) for row in rows if row.get("best_modeled_edge_bps") is not None]
    costs = [Decimal(row["best_projected_turnover_cost_bps"]) for row in rows
             if row.get("best_projected_turnover_cost_bps") is not None]
    modeled_eligible = sum(float(row.get("incentive_turnover", {}).get("filled_turnover_quote", 0)) for row in rows)
    fills = totals["maker_fills"] + totals["ioc_fills"]
    return totals | {
        "windows": len(rows), "complete_windows": len(valid), "hours": hours,
        "incomplete_reasons": reasons, "entry_blocked": blocked,
        "best_modeled_edge_bps": str(max(best)) if best else None,
        "best_projected_turnover_cost_bps": str(min(costs)) if costs else None,
        "modeled_eligible_turnover": modeled_eligible,
        "eligible_cost_bps": -net * 1e4 / modeled_eligible if net is not None and modeled_eligible else None,
        "passive_cycles_started": [sum(row.get("passive_cycles_started", [0, 0])[i] for row in rows)
                                   for i in (0, 1)],
        "passive_cycles_completed": [sum(row.get("passive_cycles_completed", [0, 0])[i] for row in rows)
                                     for i in (0, 1)],
        "max_unhedged_ms": max(row.get("max_unhedged_ms", 0) for row in rows),
        "max_net_notional": max(float(row.get("max_net_notional", 0)) for row in rows),
        "mean_unhedged_ms": totals["total_unhedged_ms"]/totals["unhedged_episodes"] if totals["unhedged_episodes"] else None,
        "maker_fills_per_hour": totals["maker_fills"]/hours, "total_fills_per_hour": fills/hours,
        "ioc_slippage_bps": 1e4*totals["ioc_slippage"]/totals["ioc_submit_touch_turnover"]
            if totals["ioc_submit_touch_turnover"] else None,
        "pnl_before_fees": gross, "net_pnl": net, "net_pnl_per_hour": net/hours if net is not None else None,
        "complete_windows_net_pnl": sum(float(row["net_pnl"]) for row in valid) if valid else None,
        "positive_windows": sum(Decimal(row["net_pnl"]) > 0 for row in valid),
        "residual_windows": sum(any(Decimal(v) for v in row.get("open_positions_base", [])) for row in rows),
    }


def run_case(engine, out, route, label, policy, period):
    begin, end, phase = period
    name = f"{route}-{begin}-{label}"
    config = out / "configs" / f"{name}.json"
    output = out / "runs" / f"{name}.json"
    tape = out / "inputs" / f"{route}-{begin}.ndjson"
    save(config, policy | {"start_ms": begin, "end_ms": end})
    with output.open("x") as stdout, output.with_suffix(".stderr").open("x") as stderr:
        proc = subprocess.run([str(engine), "maker-first-replay", str(config), str(tape)],
                              stdout=stdout, stderr=stderr, timeout=90)
    row = load(output) if proc.returncode == 0 else {
        "start_ms": begin, "end_ms": end, "incomplete": f"replay_exit_{proc.returncode}",
    }
    return label, phase, row


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("engine", type=Path)
    parser.add_argument("capture", type=Path)
    parser.add_argument("compiled", type=Path)
    parser.add_argument("market_terms", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--begin-ms", type=int, required=True)
    parser.add_argument("--holdout-ms", type=int, required=True)
    parser.add_argument("--end-ms", type=int, required=True)
    parser.add_argument("--workers", type=int, choices=(1, 2), default=2)
    args = parser.parse_args()
    if not (0 < args.begin_ms < args.holdout_ms < args.end_ms
            and all(v % HOUR == 0 for v in (args.begin_ms, args.holdout_ms, args.end_ms))):
        parser.error("chronological comparison and holdout require aligned UTC hour boundaries")
    os.umask(0o077)
    out = args.output.resolve()
    out.mkdir()
    for folder in ("inputs", "runs", "configs"):
        (out / folder).mkdir()
    engine = args.engine.resolve()
    periods = list(windows(args.begin_ms, args.holdout_ms, args.end_ms))
    terms = load(args.market_terms)
    result = {"periods": periods, "engine_sha256": digest(engine), "market_terms_sha256": digest(args.market_terms),
              "routes": {}}
    for path in sorted(args.compiled.glob("*.ndjson.zst")):
        route = path.name.removesuffix(".ndjson.zst")
        if route.startswith("capture-"):
            continue
        pair, coverage = write_inputs(route, args.capture, args.compiled, out / "inputs", periods, args.end_ms)
        symbol = pair["legs"][0]["underlying_id"]
        arcus = next(row for row in terms["arcus"] if row["baseAsset"] == symbol)
        fees = tuple(str(Decimal(arcus[key])/100) for key in ("effectiveMakerFeePpm", "effectiveTakerFeePpm"))
        policies = list(cases(pair, fees))
        jobs = [(label, policy, period) for label, policy in policies for period in periods]
        rows = {}
        with ThreadPoolExecutor(max_workers=args.workers) as executor:
            tasks = [executor.submit(run_case, engine, out, route, *job) for job in jobs]
            for future in tasks:
                label, phase, row = future.result()
                rows.setdefault(phase, {}).setdefault(label, []).append(row)
        result["routes"][route] = coverage | {"cases": {
            phase: {label: summarize(r) for label, r in labels.items()} for phase, labels in rows.items()
        }}
        tmp = out / "report.next"
        save(tmp, result)
        tmp.replace(out / "report.json")
    if not result["routes"]:
        raise ValueError("no compiled replay tapes")


if __name__ == "__main__":
    main()
