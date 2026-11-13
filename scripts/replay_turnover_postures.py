#!/usr/bin/env python3
"""Compare three fixed turnover postures on one sealed public capture shard.

This is a turnover-only study. Every posture/size/cost arm has a separate
ledger; neither directional PnL nor other strategies subsidize a neutral arm.
The grid is fixed before replay, with no fitting or winner selection here.
"""

import argparse
from concurrent.futures import ThreadPoolExecutor
from decimal import Decimal
import os
from pathlib import Path

from replay_extended_lighter import build_inputs, compile_tape, profile
from replay_maker_campaign import load, save
from replay_maker_first_campaign import digest, run_case, summarize


POSTURES = ("neutral", "long_first", "short_first")


def policies(pair, period):
    start, end, _ = period
    for posture in POSTURES:
        for size in (50, 100):
            for cost_cap in (0, 4):
                # Cost tolerance is an experimental loss allowance, not a
                # point valuation. Points and unconfirmed rebates stay zero.
                label = f"{posture}-{size}usd-{cost_cap}bps"
                mode = {"kind": "turnover", "max_turnover_quote": "2000", "max_fees_quote": "1",
                        "max_realized_loss_quote": "0.50", "incentive": {"eligible_leg": 1,
                        "eligible_from_ms": start, "eligible_until_ms": end,
                        "max_cost_bps": str(cost_cap)}}
                yield label, {
                    "pair": pair | {"mode": mode}, "maker_leg": 1,
                    "transport_ms": 250, "fill_notice_ms": 100, "maker_cancel_delay_ms": 0,
                    "taker_delay_ms": [300, 0], "trade_clock_guard_ms": 100,
                    "trade_delivery_grace_ms": 750, "quote_lifetime_ms": 800, "reprice_ms": 100,
                    "hold_ms": 15_000, "unhedged_timeout_ms": 2_500, "drain_ms": 15_000,
                    "require_net_edge": True, "maker_fee_bps": "0", "quote_notional": str(size),
                    "latency_reserve_bps": "0.5",
                    "hedged_exit": {"max_hold_ms": 60_000, "min_saving_bps": "0",
                                    "max_adverse_basis_bps": "10"},
                    "turnover_feedback": {"min_reserve_bps": "0.25", "max_reserve_bps": "0.25",
                                          "alpha": "0", "overhead_bps": "0.25",
                                          "max_quote_distance_bps": "12"},
                    "turnover_posture": {"initial": posture, "max_control_ttl_ms": 300_000},
                }


def summarize_posture(rows):
    result = summarize(rows)
    complete = all(r.get("incomplete") is None and r.get("net_pnl") is not None for r in rows)
    costs = [r.get("turnover_feedback", {}) for r in rows]
    complete &= all(c.get("net_after_overhead") is not None for c in costs)
    net = sum(Decimal(str(c["net_after_overhead"])) for c in costs) if complete else None
    target = sum(Decimal(str(c.get("modeled_target_turnover_quote", "0"))) for c in costs)
    # Use every target fill, including forced exits and unfinished cycles.
    # Program-credited volume is always unknown without venue statements.
    attribution = [r.get("cycle_attribution", {}) for r in rows]
    return result | {
        "net_after_overhead_quote": str(net) if net is not None else None,
        "target_turnover_quote": str(target), "venue_credited_turnover_quote": None,
        "cost_bps_per_target_turnover": str(-net * 10_000 / target)
            if net is not None and target else None,
        "net_after_overhead_per_hour": float(net) / result["hours"] if net is not None else None,
        "paired_maker_closes": sum(a.get("paired_maker_closes", 0) for a in attribution),
        "forced_ioc_cycles": sum(a.get("forced_ioc_cycles", 0) for a in attribution),
        "funding_paid_quote": "0", "points_value_quote": "0", "unconfirmed_rebates_quote": "0",
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for arg in ("engine", "shard", "metadata", "output"):
        parser.add_argument(arg, type=Path)
    parser.add_argument("--symbol", choices=("BTC", "ETH"), required=True)
    args = parser.parse_args()
    os.umask(0o077)
    out, engine = args.output.resolve(), args.engine.resolve()
    out.mkdir()
    for sub in ("inputs", "runs", "configs"):
        (out / sub).mkdir()
    cfg = profile(args.symbol, {k: load(args.metadata / k) for k in ("extended", "lighter")},
                  Path(__file__).resolve().parent.parent / "examples/eth_capture.json")
    frames, coverage = compile_tape(engine, args.shard, cfg, out, args.symbol)
    route, periods, counts = build_inputs(args.shard, frames, cfg, coverage, out, args.symbol)
    periods = [p for p in periods if p[1] - p[0] > 92_000]
    error = None
    if not coverage["synchronized_book_pair_frames"]:
        error = "no_synchronized_books"
    elif not periods:
        error = "no_full_cycle_window"
    elif not counts["extended_trade"]:
        error = "no_extended_trade_prints"
    cases = {}
    if error is None:
        with ThreadPoolExecutor(max_workers=2) as pool:
            tasks = [(label, pool.submit(run_case, engine, out, route, label, policy, period))
                     for period in periods for label, policy in policies(cfg["pair"], period)]
            rows = {}
            for label, task in tasks:
                rows.setdefault(label, []).append(task.result()[2])
        cases = {name: summarize_posture(values) for name, values in rows.items()}
    save(out / "report.json", {"scenario": "turnover", "route": route, "venue_domain": "lighter_core",
         "windows": periods, "coverage": coverage, "events": counts, "coverage_error": error,
         "cases": cases, "source_sha256": digest(args.shard), "engine_sha256": digest(engine),
         "metadata_sha256": {k: digest(args.metadata / k)
                             for k in ("extended", "lighter", "extended_fees", "lighter_fees")}})


if __name__ == "__main__":
    main()
