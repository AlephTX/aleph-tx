#!/usr/bin/env python3
"""Frozen RH/Core cancellation-overlap sensitivity on the inspected cost curve."""
import argparse
from concurrent.futures import ThreadPoolExecutor, as_completed
import os
from pathlib import Path

from archive_public_capture import digest
from replay_maker_campaign import load, save
from replay_ten_hour_scenarios import execute, TEN_HOURS
from replay_trade_preflight import audit
from replay_turnover_cost_curve import point


def control_matches(old, new):
    return (old["ledger_complete"] == new["ledger_complete"]
            and old["incomplete"] == new["incomplete"]
            and old["closed_cycles"] == new["closed_cycles"]
            and all(old[k] == new[k] for k in ("observed_target_turnover", "net_after_overhead")))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("engine", "parent", "output"):
        parser.add_argument(name, type=Path)
    args = parser.parse_args()
    engine, parent, out = (v.resolve() for v in (args.engine, args.parent, args.output))
    os.umask(0o077)
    old_report = parent / "curve/report.json"
    report_hash = digest(old_report)
    old = load(old_report)
    prepared = parent / "prepared"
    manifest = load(prepared / "manifest.json")
    if not old["matrix_finished"] or old["source_manifest_sha256"] != digest(prepared / "manifest.json"):
        raise ValueError("complete hash-bound parent matrix required")
    out.mkdir()
    for directory in ("configs", "runs"):
        (out / directory).mkdir()
    frozen, references, inputs, admissions = {}, {}, {}, {}
    for symbol in ("BTC", "ETH"):
        spec = manifest["markets"][symbol]
        tape = prepared / spec["input"]
        if digest(tape) != spec["input_sha256"] or spec["window"]["end_ms"] - spec["window"]["start_ms"] != TEN_HOURS:
            raise ValueError("parent input identity or window mismatch")
        inputs[symbol] = tape
    for label, expected in sorted(old["config_sha256"].items()):
        path = parent / "curve/configs" / (label + ".json")
        if digest(path) != expected:
            raise ValueError("parent configuration changed")
        config = load(path)
        if config.get("conservative_cancel_guard", False) or not config.get("diagnose_cycles"):
            raise ValueError("expected historical strict guard with attribution")
        symbol = label.split("-", 1)[0]
        if symbol not in admissions:
            admissions[symbol] = audit(inputs[symbol], config)
            save(out / (symbol + ".admission.json"), admissions[symbol])
        raw_path = parent / "curve/runs" / (label + ".json")
        references[label] = {"report_sha256": digest(raw_path),
                             "result": point(load(raw_path) | {"engine_exit_code": 0})}
        modes = (False, True) if label.endswith("-2bps") else (True,)
        for conservative in modes:
            name = ("conservative-" if conservative else "strict-control-") + label
            path = out / "configs" / (name + ".json")
            save(path, config | {"conservative_cancel_guard": conservative})
            frozen[name] = {"config_sha256": digest(path), "parent_label": label,
                            "symbol": symbol, "conservative": conservative}
    policy = {"sample": "historical_selection", "execution_qualified": False,
              "parent_report_sha256": report_hash, "source_manifest_sha256": digest(prepared / "manifest.json"),
              "engine_sha256": digest(engine), "runner_sha256": digest(Path(__file__)),
              "frozen": frozen, "references": references,
              "scope": "all 72 conservative arms plus 12 strict 2bps controls; no risk-limit or reward changes"}
    save(out / "policy.json", policy)
    def run(name, spec):
        path = out / "configs" / (name + ".json")
        if digest(path) != spec["config_sha256"]:
            raise ValueError("frozen config changed")
        if not admissions[spec["symbol"]]["trade_timing_admitted"]:
            return {"incomplete": "input_trade_timing_rejected", "ledger_complete": False}
        result = point(execute(engine, path, inputs[spec["symbol"]], out / "runs" / (name + ".json"), True))
        if not spec["conservative"]:
            result["matches_parent"] = control_matches(references[spec["parent_label"]]["result"], result)
        return result
    results = {}
    with ThreadPoolExecutor(max_workers=2) as pool:
        jobs = {pool.submit(run, name, spec): name for name, spec in frozen.items()}
        for future in as_completed(jobs):
            name = jobs[future]
            try:
                results[name] = future.result()
            except Exception as error:
                results[name] = {"ledger_complete": False, "incomplete": "runner_failure", "error_type": type(error).__name__}
            save(out / "report.json", policy | {"runs": results, "matrix_finished": False})
    if (digest(engine) != policy["engine_sha256"] or digest(old_report) != report_hash
            or any(digest(out / "configs" / (n + ".json")) != s["config_sha256"] for n, s in frozen.items())
            or any(digest(p) != manifest["markets"][s]["input_sha256"] for s, p in inputs.items())):
        raise ValueError("frozen inputs changed during replay")
    controls = all(results[n].get("matches_parent") is True for n in frozen if n.startswith("strict-control-"))
    completed = sum(v.get("cash_attribution_reconciled") is True for v in results.values())
    save(out / "report.json", policy | {"runs": results, "matrix_finished": True,
                                       "controls_match": controls, "complete_ledgers": completed})
    return 0 if controls and completed == len(results) else 1


if __name__ == "__main__":
    raise SystemExit(main())
