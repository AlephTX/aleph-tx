#!/usr/bin/env python3
"""Freeze and compare a receipt-causal Core entry gate on a reused ten-hour tape."""

import argparse
import math
import os
from pathlib import Path

from archive_public_capture import digest
from replay_maker_campaign import load, save
from replay_quant_participation import summarize, validate_baseline
from replay_ten_hour_scenarios import execute

# One mechanism probe, fixed before execution; no fitted labels or parameter scan.
POLICY = {"lookback_ms": 2000, "recovery_ms": 500, "max_age_ms": 500,
          "shock_bps": 0.5, "recovery_bps": 0.1, "flow_imbalance": 0.6,
          "min_shock_notional": 1000.0}


def summarize_local(row):
    local = row.get("local_participation") or {}
    decisions, ready = local.get("decisions"), local.get("ready_decisions")
    marks = []
    for mark in row.get("cross_market_attribution", {}).get("maker_markouts", []):
        turnover = mark.get("marked_turnover")
        cancel_turnover = mark.get("cancel_pending_marked_turnover")
        marks.append(mark | {
            "markout_bps": mark["markout_quote"] / turnover * 10_000 if turnover else None,
            "cancel_pending_markout_bps": (mark["cancel_pending_markout_quote"] / cancel_turnover * 10_000
                                           if cancel_turnover else None),
        })
    return summarize(row) | {
        "local_participation": row.get("local_participation"),
        "local_ready_fraction": ready / decisions if decisions and ready is not None else None,
        "maker_markouts": marks,
        "markout_anchor": row.get("cross_market_attribution", {}).get("markout_anchor"),
        "fair_value_mode": row.get("fair_value_mode"),
    }


def control_matches(reference, current):
    """Diagnostics must not change the economic control; unknown is not zero."""
    fields = ("net_after_overhead", "maker_turnover", "taker_turnover")
    return (reference.get("terminal_flat_and_feed_verified") is True
            and current.get("terminal_flat_and_feed_verified") is True
            and all(isinstance(reference.get(k), (int, float))
                    and isinstance(current.get(k), (int, float))
                    and math.isclose(reference[k], current[k], rel_tol=1e-10, abs_tol=1e-9)
                    for k in fields))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("engine", type=Path)
    parser.add_argument("ten_hour_output", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--entry-hold-ms", choices=(0, 1000), type=int, default=0,
                        help="0 for the initial gate; 1000 for a separate fixed participation-hold sensitivity")
    parser.add_argument("--core-only", action="store_true",
                        help="add a Core-only fair-value control and run the gate without publishing external books")
    args = parser.parse_args()
    os.umask(0o077)
    engine, source, out = (p.resolve() for p in (args.engine, args.ten_hour_output, args.output))
    source_report_path = source / "report.json"
    report_hash = digest(source_report_path)
    source_report = load(source_report_path)
    evidence = source_report["source_evidence"]["ETH"]
    tape = source / "inputs/quant-ETH.ndjson"
    tape_hash = digest(tape)
    if (report_hash != digest(source_report_path)
            or evidence["quant"]["input_sha256"] != tape_hash
            or not evidence["quant"]["explicit_feed_states"]):
        raise ValueError("source report or tape provenance is missing or changed")
    arms, references, baseline_hashes = {}, {}, {}
    for capital in (500, 1000):
        label = f"quant-ETH-{capital}capital-{capital // 10}size"
        baseline = source / "configs" / f"{label}.json"
        baseline_hash = digest(baseline)
        reference = source_report["quant"]["ETH"][label]
        if reference["config_sha256"] != baseline_hash:
            raise ValueError("baseline config provenance mismatch")
        config = load(baseline)
        validate_baseline(config, evidence["window"])
        if (digest(baseline) != baseline_hash or config["initial_equity"] != capital
                or config["strategy"]["base_order_notional_usd"] != capital / 10):
            raise ValueError("baseline changed or capital does not match")
        baseline_hashes[str(capital)] = baseline_hash
        references[str(capital)] = reference
        arms[f"{capital}-control"] = config | {"diagnose_cross_market": True}
        if args.core_only:
            arms[f"{capital}-core-control"] = config | {"diagnose_cross_market": True, "core_only": True}
        arms[f"{capital}-local"] = config | {"diagnose_cross_market": True,
                                            "core_only": args.core_only,
                                            "local_reversal_filter": POLICY | {"entry_hold_ms": args.entry_hold_ms}}
    out.mkdir()
    configs = {arm: out / f"{arm}.config.json" for arm in arms}
    for arm, config in arms.items():
        save(configs[arm], config)
    policy = {"strategy": "quant_profit", "experiment": "historical_local_reversal_mechanism_probe",
              "entry_hold_ms": args.entry_hold_ms,
              "core_only": args.core_only,
              "sample": source_report["sample"], "selection_observed": True,
              "execution_qualified": False, "holdout": False, "fitted_model": False,
              "baseline_fair_value_and_risk_reference_unchanged": not args.core_only,
              "source_report_sha256": report_hash, "baseline_config_sha256": baseline_hashes,
              "input_sha256": tape_hash, "engine_sha256": digest(engine),
              "runner_sha256": digest(Path(__file__)), "source_evidence": evidence,
              "config_sha256": {arm: digest(p) for arm, p in configs.items()}}
    save(out / "policy.json", policy)
    runs = {}
    for arm, path in configs.items():
        runs[arm] = summarize_local(execute(engine, path, tape, out / f"{arm}.run.json"))
        save(out / "report.json", policy | {"runs": runs, "comparison_complete": False})
    if (digest(tape) != tape_hash or digest(source_report_path) != report_hash
            or digest(engine) != policy["engine_sha256"]
            or any(digest(p) != policy["config_sha256"][arm] for arm, p in configs.items())):
        raise ValueError("frozen experiment changed during replay")
    matched = {str(c): control_matches(references[str(c)], runs[f"{c}-control"]) for c in (500, 1000)}
    complete = all(matched.values()) and all(r["net_after_overhead"] is not None for r in runs.values())
    deltas = {str(c): (runs[f"{c}-local"]["net_after_overhead"] - runs[f"{c}-control"]["net_after_overhead"]
                       if complete else None) for c in (500, 1000)}
    gate_deltas = {str(c): (runs[f"{c}-local"]["net_after_overhead"]
                            - runs[f"{c}-{'core-control' if args.core_only else 'control'}"]["net_after_overhead"]
                            if complete else None) for c in (500, 1000)}
    save(out / "report.json", policy | {"runs": runs, "comparison_complete": complete,
                                       "control_matches_parent": matched, "paired_net_delta": deltas,
                                       "entry_gate_delta_against_same_fair_value": gate_deltas})
    return 0 if complete else 1


if __name__ == "__main__":
    raise SystemExit(main())
