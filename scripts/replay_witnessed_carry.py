#!/usr/bin/env python3
"""Replay funding carry with receipt-time forecasts and witnessed cash settlements."""

import argparse
from collections import Counter
from decimal import Decimal
import json
import os
from pathlib import Path

from replay_funding_campaign import HOUR, carry_forecasts, identifier, native_run, rate_archive
from replay_maker_campaign import load, save, unseal
from replay_maker_first_campaign import digest

VENUES = ("lighter", "hyperliquid")


def witnesses(path):
    with unseal(path) as stream:
        legs = json.loads(stream.readline())["pair"]["legs"]
        if tuple(leg["venue"] for leg in legs) != VENUES:
            raise ValueError("expected the witnessed Lighter and Hyperliquid pair")
        slots = [{}, {}]
        for line in stream:
            for found, frame in zip(slots, json.loads(line)["legs"], strict=True):
                for receipt in frame.get("witnessed_settlements", []):
                    when = receipt["at_ms"]
                    if when in found:
                        raise ValueError("duplicate settlement evidence")
                    found[when] = receipt
    return slots


def make_tape(path, output, rates, begin, end, delay, premium):
    with unseal(path) as stream, output.open("x") as file:
        header = json.loads(stream.readline())
        pair = header["pair"]
        legs = pair["legs"]
        terms = [identifier(leg["venue"], leg["market"]) for leg in legs]
        pair.update(strategy="funding_carry", optimize_funding_horizon=True, recover_single_leg=True)
        pair["planner"].update(max_funding_age_ms=HOUR, funding_horizon_ms=4*HOUR,
                               exit_basis_reserve_bps=2.0, min_net_edge_bps=0.5,
                               max_leg_notional_usd=100, max_unhedged_notional_usd=100)
        pair["execution_window_ms"] = 3_000
        legs[0]["taker_fee_bps"] = "2.8" if premium else "0"
        legs[1]["taker_fee_bps"] = "4.5"
        header.update(funding_guard_ms=1_000, one_way_latency_ms=delay, trade_from_ms=begin)
        file.write(json.dumps(header, separators=(",", ":")) + "\n")
        ready = 0
        for line in stream:
            frame = json.loads(line)
            now = frame["now_ms"]
            if now >= end:
                continue
            last = min(end-120_000, now+4*HOUR)
            forecast = carry_forecasts(terms, now, last, rates) if now >= begin else None
            if forecast and all(payments for _, payments in forecast):
                ready += 1
            for i, leg in enumerate(frame["legs"]):
                leg["book"]["available_notional"] = "100"
                leg["forecast"] = None
                if forecast and forecast[i][1] and Decimal(leg["oracle"]["price"]) > 0:
                    received, payments = forecast[i]
                    leg["forecast"] = {"observed_at_ms":received,"payments":[
                        point | {"oracle_price":leg["oracle"]["price"]} for point in payments]}
            file.write(json.dumps(frame,separators=(",", ":")) + "\n")
    return ready


def per_slot(slots, begin, end):
    required = range((begin//HOUR+1)*HOUR, end+1, HOUR)
    result = {}
    for at in required:
        if any(at not in side for side in slots):
            result[str(at)] = None
            continue
        a,b=(side[at] for side in slots)
        low = Decimal(a["long_cash_low_per_base"])-Decimal(b["long_cash_high_per_base"])
        high = Decimal(a["long_cash_high_per_base"])-Decimal(b["long_cash_low_per_base"])
        result[str(at)] = {"long_lighter_short_hyperliquid_per_base":[str(low),str(high)],
                           "lighter_rate_bps":str(Decimal(a["rate"])*10_000),
                           "hyperliquid_rate_bps":str(Decimal(b["rate"])*10_000)}
    return result


def summarize_run(row):
    keep = ("entry_signals","entries_submitted","paired_entries","ioc_filled","ioc_partial","fees","funding",
            "max_unhedged_ms","open_positions_base","final_liquidation_pnl","error")
    summary = {key:row[key] for key in keep if key in row}
    if "entry_diagnostics" in row:
        detail = row["entry_diagnostics"]
        summary["best_net_edge_bps"] = detail["best_net_edge_bps"]
        summary["entry_blocked"] = Counter(detail["blocked_by_gate"]).most_common()
    return summary


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("engine", type=Path)
    parser.add_argument("capture", type=Path)
    parser.add_argument("compiled", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--start-ms", type=int, required=True)
    parser.add_argument("--end-ms", type=int, required=True)
    args = parser.parse_args()
    if not 0<args.start_ms<args.end_ms or args.start_ms%HOUR or args.end_ms%HOUR:
        parser.error("funding window must use ordered UTC hours")
    os.umask(0o077)
    out = args.output.resolve()
    out.mkdir()
    rates = rate_archive(args.capture)
    output = {"start_ms":args.start_ms,"end_ms":args.end_ms,"engine_sha256":digest(args.engine),"markets":{}}
    for symbol in ("BTC","ETH"):
        tape = args.compiled / f"{symbol}-lighter-hyperliquid.ndjson.zst"
        slots = witnesses(tape)
        results = {}
        for tier, delay in (("standard",550),("standard",800),("premium",390),("premium",640)):
            tag = f"{symbol}-{tier}-{delay}ms"
            dest = out / f"{tag}.ndjson"
            ready = make_tape(tape,dest,rates,args.start_ms,args.end_ms,delay,tier=="premium")
            results[tag] = {"frames_with_forecast":ready} | summarize_run(native_run(args.engine.resolve(),dest,out,delay))
            dest.unlink()
        output["markets"][symbol] = {"source_sha256":digest(tape),
            "settlements":per_slot(slots,args.start_ms,args.end_ms), "replay":results}
    save(out/"report.json",output)


if __name__=="__main__":
    main()
