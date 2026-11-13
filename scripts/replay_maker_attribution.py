#!/usr/bin/env python3
"""Run paired ETH Standard quote and latency diagnostics on recorded maker inputs."""

import argparse
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
import subprocess

from replay_maker_campaign import HOUR, load, save
from replay_maker_first_campaign import digest


ARMS = {
    "baseline_250ms": {},
    "no_signal": {},
    "placement_only_500ms": {"order_entry_ms": 500},
    "cancel_only_500ms": {"cancel_transport_ms": 500},
    "taker_only_500ms": {"taker_transport_ms": 500},
    "response_plus_250ms": {"order_response_ms": 350},
    "market_data_plus_250ms": {"market_data_delay_ms": 250},
    "all_transport_500ms": {"order_entry_ms": 500, "cancel_transport_ms": 500, "taker_transport_ms": 500},
    "queue_retention": {},
}


def run_case(engine, inputs, output, base_path, arm):
    cfg = load(base_path) | {
        "diagnose_cross_market": True, "order_entry_ms": 250, "cancel_transport_ms": 250,
        "taker_transport_ms": 250, "order_response_ms": 100, "market_data_delay_ms": 0,
    } | ARMS[arm]
    strategy = cfg["strategy"]
    if strategy["maker_fee_bps"] != 0 or cfg["taker_fee_bps"] != 0 or strategy["min_profit_bps"] != 12:
        raise ValueError("expected the ETH Standard 12 bps profile")
    if cfg["venue_cancel_delay_ms"] != 300 or cfg["venue_taker_delay_ms"] != 300:
        raise ValueError("expected Standard venue action delays")
    if "cross_market_reserve" not in strategy:
        raise ValueError("the base profile must include the cross-market reserve")
    if arm == "no_signal":
        del strategy["cross_market_reserve"]
    elif arm == "queue_retention":
        strategy["requote_threshold_bps"] *= 2
        strategy["order_ttl_secs"] *= 2
    # Fix signal-side delay across attribution arms to isolate the simulated
    # action delay from a second policy change in forecast attenuation.
    if arm != "no_signal":
        strategy["cross_market_reserve"]["execution_delay_ms"] = 250
    start = cfg["start_ms"]
    case = f"ETH-{start}-{arm}"
    config = output / "configs" / f"{case}.json"
    result = output / "runs" / f"{case}.json"
    save(config, cfg)
    with result.open("x") as stdout, result.with_suffix(".stderr").open("x") as stderr:
        run = subprocess.run([str(engine), str(config), str(inputs / f"ETH-{start}.ndjson")],
                             stdout=stdout, stderr=stderr, timeout=180, check=False)
    if run.returncode:
        raise RuntimeError(f"maker replay failed in {case} ({run.returncode})")
    row = load(result)
    return start, arm, row | {"hours": (cfg["drain_until_ms"] - start) / HOUR}


def summarize(rows, baseline):
    complete = all(r["net_liquidation_pnl"] is not None and r["halted"] is None for r in rows)
    hours = sum(r["hours"] for r in rows)
    turnover = sum(r["maker_turnover"] + r["taker_turnover"] for r in rows)
    net = sum(r["net_liquidation_pnl"] for r in rows) if complete else None
    paired = [r["net_liquidation_pnl"] - b["net_liquidation_pnl"] if r["net_liquidation_pnl"] is not None
              and b["net_liquidation_pnl"] is not None and r["halted"] is None and b["halted"] is None else None
              for r, b in zip(rows, baseline, strict=True)]
    attrs = [r["cross_market_attribution"] for r in rows]
    quality = {}
    for group in ("all_maker_fills", "changed_side_maker_fills"):
        sums = {field: sum(a[group][field] for a in attrs) for field in attrs[0][group]}
        marked = sums["marked_turnover_1s"]
        sums["markout_bps_1s"] = 10_000 * sums["markout_quote_1s"] / marked if marked else None
        sums["mean_queue_age_ms"] = sums["total_queue_age_ms"] / sums["fills"] if sums["fills"] else None
        quality[group] = sums
    attribution = {field: sum(a[field] for a in attrs) for field in (
        "evaluated", "signal_nonzero", "signal_at_least_0_01_bps", "reserve_dominates",
        "snapped_quote_changed", "maker_submitted", "maker_accepted", "maker_cancel_requests",
        "changed_side_maker_submitted", "changed_side_maker_accepted", "ioc_cost_quote_vs_submit_mid",
        "ioc_turnover_with_submit_mid",
    )}
    ioc_marked = attribution["ioc_turnover_with_submit_mid"]
    attribution["ioc_cost_bps_vs_submit_mid"] = (
        10_000 * attribution["ioc_cost_quote_vs_submit_mid"] / ioc_marked if ioc_marked else None
    )
    return {"windows": len(rows), "complete_windows": sum(r["net_liquidation_pnl"] is not None and r["halted"] is None for r in rows),
            "hours": hours, "turnover_quote": turnover, "turnover_quote_per_hour": turnover / hours,
            "net_quote": net, "net_quote_per_hour": net / hours if net is not None else None,
            "net_bps_per_gross_turnover": 10_000 * net / turnover if net is not None and turnover else None,
            "paired_net_delta_quote": sum(paired) if all(x is not None for x in paired) else None,
            "taker_fills": sum(r["taker_fills"] for r in rows), "taker_turnover": sum(r["taker_turnover"] for r in rows),
            "post_only_rejections": sum(r["post_only_rejections"] for r in rows),
            "mean_inventory_notional": sum(r["mean_inventory_notional"] * r["hours"] for r in rows) / hours,
            "max_inventory_notional": max(r["max_inventory_notional"] for r in rows),
            "attribution": attribution | quality}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("maker_replay", type=Path)
    parser.add_argument("inputs", type=Path, help="prior study maker/inputs directory")
    parser.add_argument("output", type=Path, help="new private output directory")
    parser.add_argument("base_configs", type=Path, nargs="+", help="existing ETH Standard 12 bps 250 ms cross-market replay configs")
    args = parser.parse_args()
    starts = sorted(load(p)["start_ms"] for p in args.base_configs)
    if len(starts) != len(set(starts)):
        parser.error("base configurations must have distinct start times")
    args.output.mkdir(mode=0o700)
    for folder in ("configs", "runs"):
        (args.output / folder).mkdir(mode=0o700)
    with ThreadPoolExecutor(max_workers=2) as pool:
        jobs = [pool.submit(run_case, args.maker_replay.resolve(), args.inputs, args.output, p, arm)
                for arm in ARMS for p in args.base_configs]
        runs = {arm: {} for arm in ARMS}
        for job in jobs:
            start, arm, row = job.result()
            runs[arm][start] = row
    baseline = [runs["baseline_250ms"][start] for start in starts]
    save(args.output / "report.json", {
        "maker_replay_sha256": digest(args.maker_replay),
        "input_sha256": {str(start): digest(args.inputs / f"ETH-{start}.ndjson") for start in starts},
        "base_config_sha256": {str(load(p)["start_ms"]): digest(p) for p in args.base_configs},
        "scenarios": {arm: summarize([runs[arm][start] for start in starts], baseline) for arm in ARMS},
    })


if __name__ == "__main__":
    main()
