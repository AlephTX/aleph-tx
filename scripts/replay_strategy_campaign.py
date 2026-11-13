#!/usr/bin/env python3
"""Compare quant, maker-first hedge and turnover policies on sealed public inputs."""

import argparse
from concurrent.futures import ThreadPoolExecutor
from decimal import Decimal
import gzip
from itertools import product
import json
import math
import os
from pathlib import Path
import subprocess
from tempfile import TemporaryDirectory
import tomllib

import replay_maker_campaign as maker
import replay_maker_first_campaign as paired

HOUR = maker.HOUR
CAPITAL = 1_000.0
TIERS = {"standard": (0.0, 0.0, 300, 300), "premium": (0.4, 2.8, 0, 140)}
LATENCIES = (250, 500)
ROUTES = ("lighter-hyperliquid", "lighter-arcus")
SYMBOLS = ("BTC", "ETH")


def sealed_trades(capture, end):
    files = {}
    coverage = {}
    for venue in ("lighter", "hyperliquid", "arcus"):
        paths = sorted((capture / "raw").glob(f"{venue}-trades-*.jsonl.gz"))
        last = 0
        selected = []
        for path in paths:
            selected.append(path)
            # A sealed shard containing a receipt beyond the cutoff proves
            # that the study did not silently truncate the public tape.
            with gzip.open(path, "rb") as stream:
                for line in stream:
                    last = json.loads(line)["received_at_ms"]
            if last >= end:
                break
        if last < end:
            raise ValueError(f"sealed public trade coverage ends before study cutoff on {venue}")
        files[venue] = selected
        coverage[venue] = last
    return files, coverage


def pair_inputs(capture, inputs, terms, periods, output, trade_files):
    (output / "inputs").mkdir()
    plans = {}
    evidence = {}
    for symbol, route in product(SYMBOLS, ROUTES):
        name = f"{symbol}-{route}"
        pair = None
        for lo, hi, directory in inputs:
            windows = [p for p in periods if lo <= p[0] < hi]
            found, info = paired.write_inputs(name, capture, directory, output / "inputs", windows, hi,
                                              trade_files=trade_files)
            if pair is not None and pair != found:
                raise ValueError(f"contract terms change between inputs for {name}")
            pair = found
            evidence.setdefault(name, []).append({"start_ms": lo, "end_ms": hi, **info})
        arcus = next(m for m in terms["arcus"] if m["baseAsset"] == symbol)
        arcus_fees = tuple(str(Decimal(arcus[k]) / 100)
                           for k in ("effectiveMakerFeePpm", "effectiveTakerFeePpm"))
        plans[name] = {label: cfg for label, cfg in paired.cases(pair, arcus_fees)
                       if label.endswith("-edge") and "-premium-" not in label}
    return plans, evidence


def maker_inputs(source, target, periods):
    for symbol, (start, end, _) in product(SYMBOLS, periods):
        tape = source / f"{symbol}-lighter-hyperliquid-{start}.ndjson"
        with tape.open() as stream, (target / f"{symbol}-{start}.ndjson").open("x") as out:
            for line in stream:
                event = json.loads(line)
                if event["kind"] == "book":
                    a, b = map(maker.compile_book, event["books"])
                    row = {"kind": "book", "at_ms": event["at_ms"], "lighter": a, "hyperliquid": b}
                elif event["kind"] == "trade" and event["leg"] == 0:
                    row = {k: event[k] for k in ("kind", "at_ms", "source_ms", "aggressor")}
                    row |= {"price": float(event["price"]), "size": float(event["contracts"])}
                else:
                    continue
                out.write(json.dumps(row, separators=(",", ":"), allow_nan=False) + "\n")


def maker_results(engine, output, base, capture, periods, pool):
    common = {"use_markout_spread": True, "max_position_notional_usd": 100.0,
              "inventory_urgency_notional_usd": 45.0, "position_timeout_secs": 120}
    regimes = {"control": {}, "minute_range": {"volatility_regime": {
        "range_trigger_bps": 8.0, "max_widen_bps": 3.0, "min_size_scale": 0.5}}}
    maker.SCENARIOS = {
        f"{tier}-{floor}bps-{label}": (m, t, common | {"min_profit_bps": float(floor), **regime})
        for (tier, (m, t, _, _)), floor, (label, regime) in product(TIERS.items(), (2, 12), regimes.items())
    }
    market = capture["selected"]["lighter"]
    jobs = []
    for symbol, (start, end, phase), (tier, (_, _, cancel, taker)), floor, regime, delay in product(
        SYMBOLS, periods, TIERS.items(), (2, 12), regimes, LATENCIES,
    ):
        period = {"start_ms": start, "end_ms": end - 10_000, "drain_until_ms": end, "phase": phase}
        name = f"{tier}-{floor}bps-{regime}"
        args = (symbol, period, name, delay)
        jobs.append((period, pool.submit(maker.run_case, engine, output, base, market[symbol], CAPITAL,
                                         args, venue_cancel_delay=cancel, venue_taker_delay=taker)))
    groups = {}
    for period, job in jobs:
        symbol, phase, name, delay, row = job.result()
        row |= {k: period[k] for k in ("start_ms", "drain_until_ms")}
        groups.setdefault((symbol, phase, f"{name}-{delay}ms"), []).append(row)
    result = {s: {"comparison": {}, "holdout": {}} for s in SYMBOLS}
    for (symbol, phase, name), rows in groups.items():
        result[symbol][phase][name] = maker.aggregate(rows, CAPITAL)
    return result


def lower_probability(successes, attempts):
    """One-sided Wilson lower estimate with a minimum sample count."""
    if attempts < 20:
        return "0"
    z = 1.645
    p = successes / attempts
    lower = (p + z*z/(2*attempts) - z*math.sqrt((p*(1-p) + z*z/(4*attempts))/attempts)) / (1+z*z/attempts)
    return f"{max(0.0, lower):.6f}"


def paired_results(engine, output, plans, periods, pool):
    train = [p for p in periods if p[2] == "comparison"]
    offsets = {"max_net_notional": "105", "max_wait_ms": 5_000}
    model = {}
    jobs = []
    result = {}
    def run(route, label, cfg, windows):
        return [pool.submit(paired.run_case, engine, output, route, label, cfg, p) for p in windows]
    def collect(tasks):
        groups = {}
        for task in tasks:
            name, phase, row = task.result()
            groups.setdefault(phase, {}).setdefault(name, []).append(row)
        return {phase: {name: paired.summarize(rows) for name, rows in cases.items()}
                for phase, cases in groups.items()}
    for route, cases in plans.items():
        for name, cfg in cases.items():
            common = cfg | {"unhedged_timeout_ms": 7_000, "latency_reserve_bps": "0.5"}
            for side in ("buy", "sell"):
                label = f"{name}-calibration-{side}"
                policy = common | {"entry_side": side, "passive_offset": offsets}
                jobs.append((route, name, side, run(route, label, policy, train)))
    for route, name, side, tasks in jobs:
        summary = next(iter(collect(tasks)["comparison"].values()))
        i = int(side == "sell")
        trials, closed = (summary[k][i] for k in ("passive_cycles_started", "passive_cycles_completed"))
        estimate = lower_probability(closed, trials)
        model.setdefault(route, {}).setdefault(name, {})[side] = {"attempts": trials, "closed": closed,
                                                                   "lower_probability": estimate}
    maker.save(output / "calibration.json", model)
    jobs = {}
    for route, cases in plans.items():
        for name, cfg in cases.items():
            common = cfg | {"unhedged_timeout_ms": 7_000, "latency_reserve_bps": "0.5"}
            probability = [model[route][name][side]["lower_probability"] for side in ("buy", "sell")]
            policies = {"immediate": common, "displayed": common | {"passive_offset": offsets},
                        "calibrated": common | {"passive_offset": offsets | {"close_probability": probability}}}
            for variant, policy in policies.items():
                jobs.setdefault(route, []).extend(run(route, f"{name}-{variant}", policy, periods))
            # Arcus maker turnover is the target. Points have zero cash value.
            if common["pair"]["legs"][common["maker_leg"]]["venue"] != "arcus":
                continue
            for cap, period in product((0, 4, 8), periods):
                start, end, _ = period
                mode = {"kind": "turnover", "max_turnover_quote": "2000", "max_fees_quote": "1",
                        "max_realized_loss_quote": "0.50", "incentive": {
                            "eligible_leg": common["maker_leg"], "max_cost_bps": str(cap),
                            "eligible_from_ms": start, "eligible_until_ms": end}}
                for variant in ("immediate", "calibrated"):
                    policy = policies[variant] | {"pair": common["pair"] | {"mode": mode}}
                    jobs[route].extend(run(route, f"{name}-turnover-{cap}bps-{variant}", policy, [period]))
    for route, tasks in jobs.items():
        result[route] = collect(tasks)
    return result


def ioc_case(engine, source, out, route, period, delay, cap):
    begin, end, phase = period
    label = f"{route}-{begin}-{delay}ms-" + ("basis" if cap is None else f"turnover-{cap}bps")
    result = out / "runs" / f"{label}.json"
    manifest = out / "configs" / f"{label}.json"
    with paired.unseal(source) as tape, result.open("x") as stdout, result.with_suffix(".stderr").open("x") as stderr:
        header = json.loads(tape.readline())
        config = header["pair"]
        config.update(recover_single_leg=True, execution_window_ms=1_800)
        config["basis"].update(entry_z=0.75, exit_z=0.35, min_exit_profit_bps="0.5")
        config["planner"].update(max_leg_notional_usd=100, max_unhedged_notional_usd=100)
        if cap is not None:
            eligible = next(i for i, leg in enumerate(config["legs"]) if leg["venue"] == "arcus")
            config["mode"] = {"kind": "turnover", "max_turnover_quote": "2000", "max_fees_quote": "1",
                              "max_realized_loss_quote": "0.50", "incentive": {
                                  "eligible_leg": eligible, "max_cost_bps": str(cap),
                                  "eligible_from_ms": begin, "eligible_until_ms": end - 30_000}}
        header.update(trade_from_ms=begin, one_way_latency_ms=delay)
        maker.save(manifest, header | {"input_sha256": paired.digest(source), "cutoff_ms": end})
        with TemporaryDirectory(dir=out) as temporary:
            input_link = Path(temporary) / "input.ndjson"
            input_link.symlink_to("/dev/stdin")
            with subprocess.Popen([str(engine), "perp-replay", str(input_link)], stdin=subprocess.PIPE,
                                  stdout=stdout, stderr=stderr) as proc:
                try:
                    with proc.stdin as stream:
                        stream.write((json.dumps(header) + "\n").encode())
                        for line in tape:
                            row = json.loads(line)
                            if begin <= row["now_ms"] < end:
                                for leg in row["legs"]:
                                    # Coverage-only inputs carry no simulated margin.
                                    leg["book"]["available_notional"] = "100"
                                stream.write((json.dumps(row, separators=(",", ":")) + "\n").encode())
                except BrokenPipeError:
                    for _ in tape:
                        pass
                code = proc.wait(timeout=180)
    if code:
        return phase, label, {"error": f"perp_replay_exit_{code}"}
    report = maker.load(result)
    keys = ("frames", "entry_signals", "entries_submitted", "paired_entries", "ioc_filled", "gross_turnover",
            "fees", "funding", "max_unhedged_ms", "open_positions_base", "final_liquidation_pnl", "halted",
            "incentive_turnover", "entry_diagnostics")
    return phase, label, {k: report[k] for k in keys if k in report}


def ioc_results(engine, compiled, out, periods, pool):
    cases = []
    for symbol, route, period, delay in product(SYMBOLS, ROUTES, periods, LATENCIES):
        if period[2] != "holdout":
            continue
        name = f"{symbol}-{route}"
        source = compiled / f"{name}.ndjson.zst"
        for cap in ((None, 0, 4, 8) if route.endswith("-arcus") else (None,)):
            cases.append(pool.submit(ioc_case, engine, source, out, name, period, delay, cap))
    groups = {}
    for case in cases:
        phase, label, row = case.result()
        groups.setdefault(phase, {})[label] = row
    return groups


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("capture", type=Path)
    parser.add_argument("market_terms", type=Path)
    parser.add_argument("comparison_compiled", type=Path)
    parser.add_argument("holdout_compiled", type=Path)
    parser.add_argument("cross_venue", type=Path)
    parser.add_argument("maker_replay", type=Path)
    parser.add_argument("config", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--begin-ms", type=int, required=True)
    parser.add_argument("--holdout-ms", type=int, required=True)
    parser.add_argument("--end-ms", type=int, required=True)
    args = parser.parse_args()
    begin, split, end = args.begin_ms, args.holdout_ms, args.end_ms
    if not (0 < begin < split < end and begin % HOUR == split % HOUR == 0 and end % 60_000 == 0):
        parser.error("start and holdout must be UTC hour boundaries and the cutoff a later UTC minute")
    os.umask(0o077)
    output = args.output.resolve()
    output.mkdir()
    capture = args.capture.resolve()
    manifest = maker.load(capture / "campaign.json")
    trade_files, coverage = sealed_trades(capture, end)
    pair_root, maker_root, ioc_root = (output / name for name in ("paired", "maker", "ioc"))
    for root in (pair_root, maker_root, ioc_root):
        for folder in ("configs", "runs"):
            (root / folder).mkdir(parents=True, exist_ok=True)
    inputs = ((begin, split, args.comparison_compiled.resolve()), (split, end, args.holdout_compiled.resolve()))
    periods = [(start + 2_000, min(start + HOUR, end) - 2_000, "comparison" if start < split else "holdout")
               for start in range(begin, end, HOUR)]
    engines = {"maker_replay": args.maker_replay.resolve(), "cross_venue": args.cross_venue.resolve()}
    def status(stage):
        temp = output / "status.next"
        maker.save(temp, {"stage": stage})
        temp.replace(output / "status.json")
    status("inputs")
    plans, evidence = pair_inputs(capture, inputs, maker.load(args.market_terms), periods, pair_root, trade_files)
    (maker_root / "inputs").mkdir()
    maker_inputs(pair_root / "inputs", maker_root / "inputs", periods)
    maker.save(output / "evidence.json", {"capture_commit": manifest["commit"], "periods": periods,
               "trade_receipt_coverage_end_ms": coverage,
               "engine_sha256": {k: paired.digest(v) for k, v in engines.items()}, "routes": evidence})
    with ThreadPoolExecutor(max_workers=2) as pool:
        status("maker_first")
        maker.save(output / "paired.json", paired_results(engines["cross_venue"], pair_root, plans, periods, pool))
        status("quant")
        with args.config.open("rb") as file:
            base = tomllib.load(file)["inventory_neutral_mm"]
        maker.save(output / "maker.json", maker_results(engines["maker_replay"], maker_root, base, manifest, periods, pool))
        status("ioc")
        maker.save(output / "ioc.json", ioc_results(engines["cross_venue"], inputs[1][2], ioc_root, periods, pool))
    status("complete")


if __name__ == "__main__":
    main()
