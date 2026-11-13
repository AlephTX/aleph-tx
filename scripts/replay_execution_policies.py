#!/usr/bin/env python3
"""Compare opt-in execution policies on a sealed replay campaign's inputs."""

import argparse
from concurrent.futures import ThreadPoolExecutor
from decimal import Decimal
from itertools import product
import os
from pathlib import Path
import tomllib

import replay_maker_campaign as maker
import replay_maker_first_campaign as paired
from replay_strategy_campaign import CAPITAL, LATENCIES, SYMBOLS, TIERS


def turnover_case(engine, old, out, symbol, period, delay, cap, variant):
    route = f"{symbol}-lighter-arcus"
    start, _, _ = period
    label = f"arcus-standard-{delay}ms-edge-turnover-{cap}bps-immediate"
    cfg = maker.load(old / "paired/configs" / f"{route}-{start}-{label}.json")
    if variant != "touch":
        cfg["turnover_feedback"] = {
            "min_reserve_bps": "0.25", "max_reserve_bps": "4",
            "alpha": "0" if variant == "fixed" else "0.25",
            "overhead_bps": "0.25", "max_quote_distance_bps": "12",
        }
    return paired.run_case(engine, out, route, f"{label}-{variant}", cfg, period)[2]


def turnover_summary(rows):
    summary = paired.summarize(rows)
    # Compare all policies at the same modeled overhead, including the touch control.
    gross = sum(Decimal(r.get("maker_turnover", 0)) + Decimal(r.get("taker_turnover", 0)) for r in rows)
    overhead = gross * Decimal("0.25") / 10_000
    eligible = sum(Decimal(r.get("incentive_turnover", {}).get("filled_turnover_quote", 0)) for r in rows)
    net = (sum(Decimal(r["net_pnl"]) for r in rows) - overhead
           if all(r.get("net_pnl") is not None for r in rows) else None)
    return summary | {"modeled_overhead_quote": str(overhead),
                      "net_after_overhead_quote": str(net) if net is not None else None,
                      "cost_after_overhead_bps": str(-net * 10_000 / eligible) if net is not None and eligible else None,
                      "eligible_turnover_at_nonpositive_cost": str(eligible) if net is not None and net >= 0 else None,
                      "feedback": [r["turnover_feedback"] for r in rows if "turnover_feedback" in r]}


def quant_cases(engine, out, base, markets, periods, pool):
    common = {"use_markout_spread": True, "max_position_notional_usd": 100.0,
              "inventory_urgency_notional_usd": 45.0, "position_timeout_secs": 120,
              "volatility_regime": {"range_trigger_bps": 8.0, "max_widen_bps": 3.0, "min_size_scale": 0.5}}
    maker.SCENARIOS = {}
    for tier, floor, delay, arm in product(TIERS, (2, 12), LATENCIES, ("control", "cross-market")):
        maker_fee, taker_fee, _, _ = TIERS[tier]
        settings = common | {"min_profit_bps": float(floor)}
        if arm == "cross-market":
            settings["cross_market_reserve"] = {"half_life_ms": 500, "max_quote_skew_ms": 750,
                                                "max_widen_bps": 3.0, "execution_delay_ms": delay}
        maker.SCENARIOS[f"{tier}-{floor}bps-{delay}ms-{arm}"] = (maker_fee, taker_fee, settings)
    pending = []
    for symbol, (start, end, phase), name in product(SYMBOLS, periods, maker.SCENARIOS):
        tier, _, transport, _ = name.split("-", 3)
        delay = int(transport.removesuffix("ms"))
        _, _, cancel, taker = TIERS[tier]
        window = {"start_ms": start, "end_ms": end - 10_000, "drain_until_ms": end, "phase": phase}
        case = (symbol, window, name, delay)
        job = pool.submit(maker.run_case, engine, out, base, markets[symbol], CAPITAL, case,
                          venue_cancel_delay=cancel, venue_taker_delay=taker)
        pending.append((symbol, window, name, job))
    rows = {}
    for symbol, window, name, job in pending:
        *_, row = job.result()
        rows.setdefault(symbol, {}).setdefault(name, []).append(row | window)
    return {symbol: {name: maker.aggregate(cases, CAPITAL) for name, cases in data.items()}
            for symbol, data in rows.items()}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("study", type=Path, help="existing replay_strategy_campaign.py output")
    parser.add_argument("capture", type=Path, help="public capture directory with campaign.json")
    parser.add_argument("config", type=Path, help="credential-free example strategy profile")
    parser.add_argument("cross_venue", type=Path)
    parser.add_argument("maker_replay", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    os.umask(0o077)
    study = args.study.resolve()
    evidence = maker.load(study / "evidence.json")
    periods = [tuple(p) for p in evidence["periods"] if p[2] == "holdout"]
    if not periods:
        parser.error("the source study has no holdout periods")
    output = args.output.resolve()
    output.mkdir()
    for name in ("maker", "paired"):
        dest = output / name
        dest.mkdir()
        (dest / "configs").mkdir()
        (dest / "runs").mkdir()
        (dest / "inputs").symlink_to(study / name / "inputs", target_is_directory=True)
    with args.config.open("rb") as file:
        base = tomllib.load(file)["inventory_neutral_mm"]
    markets = maker.load(args.capture / "campaign.json")["selected"]["lighter"]
    with ThreadPoolExecutor(max_workers=2) as pool:
        pending = {}
        for symbol, delay, cap, variant in product(SYMBOLS, LATENCIES, (0, 4, 8), ("touch", "fixed", "feedback")):
            pending.setdefault(symbol, {})[f"{delay}ms-{cap}bps-{variant}"] = [
                pool.submit(turnover_case, args.cross_venue.resolve(), study, output / "paired", symbol,
                            window, delay, cap, variant) for window in periods]
        maker.save(output / "turnover.json", {symbol: {label: turnover_summary([r.result() for r in runs])
                                                       for label, runs in data.items()}
                                               for symbol, data in pending.items()})
        maker.save(output / "quant.json", quant_cases(args.maker_replay.resolve(), output / "maker",
                                                     base, markets, periods, pool))
    maker.save(output / "evidence.json", {"periods": periods, "input_manifest_sha256": paired.digest(study / "evidence.json"),
               "engine_sha256": {k: paired.digest(getattr(args, k)) for k in ("cross_venue", "maker_replay")}})


if __name__ == "__main__":
    main()
