#!/usr/bin/env python3
"""Pair an ETH Standard participation filter with its continuous ten-hour baseline."""

import argparse
import os
from pathlib import Path

from archive_public_capture import digest
from replay_maker_campaign import FEED_LIMITS, HOUR, load, save
from replay_ten_hour_scenarios import TEN_HOURS, execute, summarize_quant

FILTER = {"threshold": 0.60, "max_age_ms": 250}


def validate_baseline(config, window):
    strategy = config["strategy"]
    expected = {"start_ms": window["warmup_start_ms"], "trade_from_ms": window["start_ms"],
                "end_ms": window["end_ms"], "drain_until_ms": window["drain_until_ms"],
                "order_entry_ms": 250, "order_response_ms": 100, "venue_cancel_delay_ms": 300,
                "venue_taker_delay_ms": 300, "taker_fee_bps": 0.0}
    if (any(config.get(key) != value for key, value in expected.items())
            or window["end_ms"] - window["start_ms"] != TEN_HOURS
            or window["start_ms"] - window["warmup_start_ms"] != 60_000
            or window["drain_until_ms"] - window["end_ms"] != 15_000
            or not config.get("flat_at_funding")
            or config.get("hyperliquid_obi_filter") is not None
            or config.get("market_data_delay_ms", 0) != 0
            or config.get("cancel_transport_ms", 250) != 250
            or config.get("taker_transport_ms", 250) != 250
            or strategy["symbol_id"] != FEED_LIMITS["ETH"]
            or strategy.get("cross_market_reserve") is not None
            or strategy["min_profit_bps"] != 12.0
            or strategy["maker_fee_bps"] != 0.0):
        raise ValueError("expected the ten-hour ETH Standard 12 bps baseline")


def summarize(row):
    summary = summarize_quant(row)
    net = summary["net_after_overhead"]
    turnover = row.get("maker_turnover", 0) + row.get("taker_turnover", 0)
    attribution = row.get("cross_market_attribution", {})
    marks = attribution.get("all_maker_fills", {})
    marked_turnover = marks.get("marked_turnover_1s", 0)
    return summary | {
        "net_quote_per_hour": None if net is None else net / (TEN_HOURS / HOUR),
        "net_bps_of_gross_turnover": None if net is None or not turnover else net / turnover * 10_000,
        "maker_submitted": attribution.get("maker_submitted"),
        "maker_accepted": attribution.get("maker_accepted"),
        "maker_cancel_requests": attribution.get("maker_cancel_requests"),
        "maker_cancel_pending_fills": marks.get("cancel_pending_fills"),
        "marked_maker_fills_1s": marks.get("marked_fills_1s"),
        "maker_markout_bps_1s": (marks["markout_quote_1s"] / marked_turnover * 10_000
                                if marked_turnover else None),
        "quote_participation": row.get("quote_participation"),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("engine", type=Path)
    parser.add_argument("ten_hour_output", type=Path, help="output of replay_ten_hour_scenarios.py")
    parser.add_argument("output", type=Path)
    parser.add_argument("--capital", choices=(500, 1000), type=int, default=500)
    args = parser.parse_args()
    os.umask(0o077)
    source = args.ten_hour_output.resolve()
    report_path = source / "report.json"
    source_report_hash = digest(report_path)
    source_report = load(report_path)
    if digest(report_path) != source_report_hash:
        raise ValueError("source campaign report changed during read")
    evidence = source_report["source_evidence"]["ETH"]
    label = f"quant-ETH-{args.capital}capital-{args.capital // 10}size"
    baseline = source / "configs" / f"{label}.json"
    tape = source / "inputs" / "quant-ETH.ndjson"
    baseline_hash, tape_hash = digest(baseline), digest(tape)
    if (source_report["quant"]["ETH"][label]["config_sha256"] != baseline_hash
            or evidence["quant"]["input_sha256"] != tape_hash
            or not evidence["quant"]["explicit_feed_states"]):
        raise ValueError("baseline provenance or public trade-feed lifecycle is missing")
    config = load(baseline)
    validate_baseline(config, evidence["window"])
    if (config["initial_equity"] != args.capital
            or config["strategy"]["base_order_notional_usd"] != args.capital / 10):
        raise ValueError("baseline capital or target order size does not match the selected case")
    engine = args.engine.resolve()
    out = args.output.resolve()
    out.mkdir()
    arms = {"control": config | {"diagnose_cross_market": True},
            "hyperliquid_obi_filter": config | {"diagnose_cross_market": True,
                                              "hyperliquid_obi_filter": FILTER}}
    configs = {arm: out / f"{arm}.config.json" for arm in arms}
    for arm, frozen in arms.items():
        save(configs[arm], frozen)
    policy = {"strategy": "quant_profit", "sample": source_report["sample"],
              "source_report_sha256": source_report_hash, "baseline_config_sha256": baseline_hash,
              "input_sha256": tape_hash, "engine_sha256": digest(engine),
              "runner_sha256": digest(Path(__file__)), "source_evidence": evidence,
              "config_sha256": {arm: digest(path) for arm, path in configs.items()},
              "execution_qualified": False}
    save(out / "policy.json", policy)
    runs = {}
    for arm, path in configs.items():
        runs[arm] = summarize(execute(engine, path, tape, out / f"{arm}.run.json"))
        save(out / "report.json", policy | {"runs": runs, "comparison_complete": False})
    if (digest(tape) != tape_hash or digest(engine) != policy["engine_sha256"]
            or any(digest(path) != policy["config_sha256"][arm] for arm, path in configs.items())):
        raise ValueError("engine, source input, or frozen configuration changed during replay")
    control, filtered = (runs[arm]["net_quote_per_hour"] for arm in arms)
    complete = control is not None and filtered is not None
    save(out / "report.json", policy | {"runs": runs, "comparison_complete": complete,
                                      "paired_delta_net_quote_per_hour": filtered - control if complete else None})
    return 0 if complete else 1


if __name__ == "__main__":
    raise SystemExit(main())
