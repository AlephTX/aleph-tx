#!/usr/bin/env python3
"""Separate Standard, Premium fee/latency and TTL hypotheses on Core-only ETH."""
import argparse
from copy import deepcopy
import os
from pathlib import Path

from archive_public_capture import digest
from replay_maker_campaign import load, save
from replay_local_participation import summarize_local, control_matches
from replay_ten_hour_scenarios import execute, TEN_HOURS


def variants(config):
    if (config.get("core_only") is not True or config.get("local_reversal_filter") is not None
            or config["strategy"]["maker_fee_bps"] != 0 or config["taker_fee_bps"] != 0
            or config["venue_cancel_delay_ms"] != 300 or config["venue_taker_delay_ms"] != 300
            or config["end_ms"] - config["trade_from_ms"] != TEN_HOURS):
        raise ValueError("expected a frozen Core-only Standard control")
    yield "standard-control", deepcopy(config)
    premium = deepcopy(config)
    premium["venue_cancel_delay_ms"], premium["venue_taker_delay_ms"] = 0, 140
    premium["strategy"]["maker_fee_bps"], premium["taker_fee_bps"] = 0.4, 2.8
    yield "premium-counterfactual", premium
    for seconds in (15, 30):
        ttl = deepcopy(config)
        ttl["strategy"]["order_ttl_secs"] = seconds
        yield f"standard-ttl-{seconds}s", ttl


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("engine", "parent", "tape", "output"):
        parser.add_argument(name, type=Path)
    args = parser.parse_args()
    engine, parent, tape, out = (v.resolve() for v in (args.engine, args.parent, args.tape, args.output))
    os.umask(0o077)
    old_path = parent / "report.json"
    old_hash = digest(old_path)
    old = load(old_path)
    if not old["comparison_complete"] or not old["core_only"] or digest(tape) != old["input_sha256"]:
        raise ValueError("complete parent with matching Core-only source required")
    out.mkdir()
    frozen = {}
    for capital in (500, 1000):
        arm = f"{capital}-core-control"
        p = parent / (arm + ".config.json")
        if digest(p) != old["config_sha256"][arm]:
            raise ValueError("parent configuration changed")
        for name, cfg in variants(load(p)):
            label = f"{capital}-{name}"
            path = out / (label + ".config.json")
            save(path, cfg)
            frozen[label] = digest(path)
    policy = {"sample": "historical_selection", "execution_qualified": False, "holdout": False,
              "engine_sha256": digest(engine), "input_sha256": digest(tape),
              "parent_report_sha256": old_hash, "config_sha256": frozen,
              "runner_sha256": digest(Path(__file__)), "overhead_bps": 0.25,
              "premium": "zero-stake public fee/latency counterfactual; 250ms transport retained; no account change",
              "ttl": "existing replacement-lifetime knob; risk withdrawals and exits remain exempt"}
    save(out / "policy.json", policy)
    runs = {}
    for label in frozen:
        runs[label] = summarize_local(execute(engine, out / (label + ".config.json"), tape, out / (label + ".run.json")))
        save(out / "report.json", policy | {"runs": runs, "comparison_complete": False})
    if (digest(engine) != policy["engine_sha256"] or digest(tape) != policy["input_sha256"]
            or digest(old_path) != old_hash or any(digest(out / (n + ".config.json")) != h for n, h in frozen.items())):
        raise ValueError("frozen experiment changed")
    controls = {str(c): control_matches(old["runs"][f"{c}-core-control"], runs[f"{c}-standard-control"]) for c in (500, 1000)}
    complete = all(controls.values()) and all(r["net_after_overhead"] is not None for r in runs.values())
    save(out / "report.json", policy | {"runs": runs, "comparison_complete": complete, "controls_match": controls})
    return 0 if complete else 1


if __name__ == "__main__":
    raise SystemExit(main())
