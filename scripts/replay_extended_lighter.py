#!/usr/bin/env python3
"""Compile and replay one sealed Extended–Lighter public capture shard."""

import argparse
from collections import Counter
from concurrent.futures import ThreadPoolExecutor
from decimal import Decimal
from heapq import merge
import json
import os
from pathlib import Path
import subprocess

from replay_maker_campaign import HOUR, load, save, unseal
from replay_maker_first_campaign import book_rows, digest, run_case, summarize


def profile(symbol, terms, template, *, basis_entries=False):
    lighter = next(r for r in terms["lighter"]["order_book_details"] if r["symbol"] == symbol)
    extended = next(r for r in terms["extended"]["data"] if r["name"] == f"{symbol}-USD")
    if extended.get("isRfq") is not False or extended["type"] != "PERPETUAL" or not extended["active"]:
        raise ValueError("Extended market must have verified perpetual CLOB metadata")
    spec = extended["tradingConfig"]
    config = load(template)
    a, b = config["pair"]["legs"]
    for leg in (a, b):
        leg.update(underlying_id=symbol, payoff_id=f"RESEARCH-USD-LINEAR:{symbol}")
    a.update(market=symbol, tick=str(Decimal(1).scaleb(-lighter["price_decimals"])),
             lot=str(Decimal(1).scaleb(-lighter["size_decimals"])),
             min_contracts=lighter["min_base_amount"], min_notional=lighter["min_quote_amount"], taker_fee_bps="0")
    b.update(venue="extended", market=extended["name"], tick=spec["minPriceChange"],
             lot=spec["minOrderSizeChange"], min_contracts=spec["minOrderSize"], min_notional="0",
             taker_fee_bps="2.5")
    config["legs"] = [
        {"lighter_market_id": lighter["market_id"], "available_notional": "150",
         "lighter_clock": {"max_rtt_ms": 600, "valid_for_ms": 300_000, "drift_margin_ms": 5}},
        {"available_notional": "150", "book_mode": "websocket"},
    ]
    config["pair"]["planner"].update(max_quote_skew_ms=750, max_leg_notional_usd=150,
                                      max_unhedged_notional_usd=150)
    config.update(sample_ms=100, paper_entries=basis_entries,
                  reference={"kind": "coinbase", "product": f"{symbol}-USD"})
    if basis_entries:
        for leg in config["legs"]:
            leg["external_oracle"] = True
    else:
        config["pair"]["planner"]["min_net_edge_bps"] = 0.5
    return config


def public_trades(shard, symbol, lighter_id):
    seen = set()
    with unseal(shard) as tape:
        for line in tape:
            event = json.loads(line)
            source = event["source"]
            if source not in ("lighter_trades", "extended_trades"):
                continue
            leg = int(source == "extended_trades")
            at, received = event["available_at_ms"], event["received_at_ms"]
            if leg and event.get("market") != f"{symbol}-USD":
                continue
            if event["event"] == "disconnect":
                yield {"kind": "trade_feed", "at_ms": at, "leg": leg, "online": False}
                continue
            row = event["payload"]
            if leg:
                trades = [(t["i"], t["T"], t["p"], t["q"], t["S"].lower())
                          for t in row["data"] if t["m"] == f"{symbol}-USD" and t["tT"] == "TRADE"]
            else:
                if row["channel"].replace("/", ":") != f"trade:{lighter_id}":
                    continue
                trades = [(t["trade_id"], t["timestamp"], t["price"], t["size"],
                           "buy" if t["is_maker_ask"] else "sell") for t in row.get("trades") or []
                          if t["market_id"] == lighter_id and isinstance(t["is_maker_ask"], bool)]
            # Subscription history establishes connectivity and deduplication only.
            snapshot = (row["seq"] == 1) if leg else (row["type"] == "subscribed/trade")
            if snapshot:
                yield {"kind": "trade_feed", "at_ms": at, "leg": leg, "online": True}
            for ident, ts, price, size, side in trades:
                if (leg, ident) in seen:
                    continue
                seen.add((leg, ident))
                if not snapshot and 0 < ts <= received and side in ("buy", "sell"):
                    yield {"kind": "trade", "at_ms": at, "leg": leg, "source_ms": ts,
                           "aggressor": side, "price": str(price), "contracts": str(size)}


def compile_tape(engine, shard, config, out, symbol):
    config_path, frame_path = out / f"{symbol}-capture.json", out / f"{symbol}-books.ndjson"
    save(config_path, config)
    with unseal(shard) as feed:
        completed = subprocess.run([str(engine), "perp-compile", str(config_path), "/dev/stdin", str(frame_path)],
                                   stdin=feed, capture_output=True, check=True, timeout=180)
    return frame_path, json.loads(completed.stdout)


def build_inputs(shard, frames, config, coverage, out, symbol):
    first = coverage["first_frame_ms"]
    end = coverage["last_frame_ms"]
    # No execution window crosses either venue's hourly funding payment.
    windows = [(max(first, h + 2_000), min(end, h + HOUR - 2_000))
               for h in range(first // HOUR * HOUR, end, HOUR)]
    windows = [(a, b, "holdout") for a, b in windows if b - a > 45_000]
    route = f"{symbol}-lighter-extended"
    online = [False, False]
    counts = Counter()
    with frames.open() as stream:
        next(stream)
        timeline = merge(public_trades(shard, symbol, config["legs"][0]["lighter_market_id"]),
                         book_rows(stream, first, end), key=lambda x: (x["at_ms"], x["kind"] == "book"))
        event = next(timeline, None)
        for start, stop, _ in windows:
            while event is not None and event["at_ms"] < start:
                if event["kind"] == "trade_feed":
                    online[event["leg"]] = event["online"]
                event = next(timeline, None)
            with (out / "inputs" / f"{route}-{start}.ndjson").open("x") as file:
                def emit(row):
                    file.write(json.dumps(row, separators=(",", ":")) + "\n")
                for leg, up in enumerate(online):
                    emit({"kind": "trade_feed", "at_ms": start, "leg": leg, "online": up})
                while event is not None and event["at_ms"] < stop:
                    if event["kind"] == "trade_feed":
                        online[event["leg"]] = event["online"]
                    elif event["kind"] == "trade" and not online[event["leg"]]:
                        online[event["leg"]] = True
                        emit({"kind": "trade_feed", "at_ms": event["at_ms"], "leg": event["leg"], "online": True})
                    counts[event["kind"]] += 1
                    if event["kind"] == "trade":
                        counts["extended_trade" if event["leg"] else "lighter_trade"] += 1
                    emit(event)
                    event = next(timeline, None)
                emit({"kind": "end", "at_ms": stop})
        for _ in timeline:
            pass
    return route, windows, counts


def run_arms(engine, out, config, route, windows, pool):
    jobs = {}
    for cap in (0, 4, 8):
        for name, wait, anchored in (("legacy", None, False), ("legacy-clock", 15_000, True),
                                     ("paired-15s", 15_000, False), ("paired-60s", 60_000, False),
                                     ("paired-180s", 180_000, False)):
            name = f"{cap}bps-{name}"
            jobs[name] = []
            for period in windows:
                start, end, _ = period
                if wait is not None and end - start <= wait + (17_000 if anchored else 32_000):
                    continue
                mode = {"kind": "turnover", "max_turnover_quote": "2000", "max_fees_quote": "1",
                        "max_realized_loss_quote": "0.50", "incentive": {"eligible_leg": 1,
                        "eligible_from_ms": start, "eligible_until_ms": end, "max_cost_bps": str(cap)}}
                policy = {"pair": config["pair"] | {"mode": mode}, "maker_leg": 1, "transport_ms": 250,
                          "fill_notice_ms": 100, "maker_cancel_delay_ms": 0, "taker_delay_ms": [300, 0],
                          "trade_clock_guard_ms": 100, "trade_delivery_grace_ms": 750, "quote_lifetime_ms": 800,
                          "reprice_ms": 100, "hold_ms": 15_000, "unhedged_timeout_ms": 2_500,
                          "drain_ms": 15_000, "require_net_edge": True, "maker_fee_bps": "0",
                          "quote_notional": "100", "latency_reserve_bps": "0.5",
                          "turnover_feedback": {"min_reserve_bps": "0.25", "max_reserve_bps": "0.25",
                              "alpha": "0", "overhead_bps": "0.25", "max_quote_distance_bps": "12"}}
                if wait is not None:
                    policy["hedged_exit"] = {"max_hold_ms": wait, "min_saving_bps": "0",
                                             "max_adverse_basis_bps": "10", "use_cycle_deadline": anchored}
                jobs[name].append(pool.submit(run_case, engine, out, route, name, policy, period))
    result = {}
    for name, pending in jobs.items():
        if pending:
            rows = [task.result()[2] for task in pending]
            summary = summarize(rows)
            full_cost = [r["turnover_feedback"] for r in rows if r.get("turnover_feedback")]
            target = summary["modeled_eligible_turnover"]
            complete = len(full_cost) == len(rows) and all(r["net_after_overhead"] is not None for r in full_cost)
            net = sum(Decimal(r["net_after_overhead"]) for r in full_cost) if complete else None
            result[name] = summary | {
                "attribution": [r["cycle_attribution"] for r in rows if "cycle_attribution" in r],
                "full_cost": full_cost,
                "net_after_overhead_quote": str(net) if net is not None else None,
                "cost_bps_per_target_turnover": float(-net * 10_000 / Decimal(str(target)))
                    if net is not None and target else None,
            }
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("engine", type=Path)
    parser.add_argument("shard", type=Path)
    parser.add_argument("metadata", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--symbol", choices=("BTC", "ETH"), default="BTC")
    args = parser.parse_args()
    os.umask(0o077)
    out, engine = args.output.resolve(), args.engine.resolve()
    out.mkdir()
    for subdir in ("inputs", "runs", "configs"):
        (out / subdir).mkdir()
    cfg = profile(args.symbol, {k: load(args.metadata / k) for k in ("extended", "lighter")},
                  Path(__file__).resolve().parent.parent / "examples/eth_capture.json")
    frames, coverage = compile_tape(engine, args.shard, cfg, out, args.symbol)
    route, windows, counts = build_inputs(args.shard, frames, cfg, coverage, out, args.symbol)
    cases = {}
    coverage_error = None
    if not coverage["synchronized_book_pair_frames"]:
        coverage_error = "no_synchronized_books"
    elif not windows:
        coverage_error = "no_replayable_trade_window"
    elif not counts["extended_trade"]:
        coverage_error = "no_extended_trade_prints"
    else:
        with ThreadPoolExecutor(max_workers=2) as pool:
            cases = run_arms(engine, out, cfg, route, windows, pool)
    save(out / "report.json", {"route": route, "windows": windows, "coverage": coverage, "events": counts,
         "coverage_error": coverage_error,
         "source_sha256": digest(args.shard), "engine_sha256": digest(engine), "cases": cases,
         "metadata_sha256": {k: digest(args.metadata / k) for k in ("extended", "lighter", "extended_fees", "lighter_fees")}})


if __name__ == "__main__":
    main()
