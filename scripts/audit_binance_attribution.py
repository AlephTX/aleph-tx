#!/usr/bin/env python3
"""Separate diagnostic replay of a completed frozen quant A/B; never rewrite it."""
import argparse
from copy import deepcopy
import hashlib
import json
import os
from pathlib import Path
import subprocess

ADDED_FIELDS = frozenset((
    "fresh_reference_fraction", "unavailable_reference_decisions", "shock_reference_decisions",
    "withdrawn_bid_targets_by_shock", "withdrawn_ask_targets_by_shock",
    "withdrawn_bid_targets_by_unavailable", "withdrawn_ask_targets_by_unavailable"))


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def load(path):
    return json.loads(path.read_text())


def save(path, value):
    with path.open("x") as stream:
        json.dump(value, stream, indent=2, allow_nan=False)


def verify(original, diagnostic):
    projected = deepcopy(diagnostic)
    gate = projected.get("binance_participation", {})
    if not ADDED_FIELDS.issubset(gate):
        return False, "missing_diagnostic_fields"
    for side in ("bid", "ask"):
        if gate[f"withdrawn_{side}_targets"] != (
                gate[f"withdrawn_{side}_targets_by_shock"]
                + gate[f"withdrawn_{side}_targets_by_unavailable"]):
            return False, "withdrawal_partition_mismatch"
    if gate["decisions"] != gate["fresh_reference_decisions"] + gate["unavailable_reference_decisions"]:
        return False, "reference_partition_mismatch"
    for key in ADDED_FIELDS:
        gate.pop(key)
    if projected != original:
        return False, "original_decisions_or_ledger_changed"
    return True, None


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("engine", type=Path)
    parser.add_argument("frozen", type=Path)
    parser.add_argument("original", type=Path, help="completed original replay output with native tapes")
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    os.umask(0o077)
    policy = load(args.frozen / "policy.json")
    original = load(args.original / "report.json")
    evidence = load(args.original / "input-evidence.json")
    if (policy.get("program") != "quant_profit" or len(policy["arms"]) != 8
            or original["policy_sha256"] != digest(args.frozen / "policy.json")
            or set(original["runs"]) != set(policy["arms"])):
        raise ValueError("completed independent quant A/B identity required")
    identities = {}
    for label, arm in policy["arms"].items():
        config = args.frozen / (label + ".config.json")
        tape = args.original / (arm["domain"] + ".ndjson")
        if digest(config) != arm["config_sha256"]:
            raise ValueError("original config changed")
        identities[config] = arm["config_sha256"]
        if tape not in identities:
            identities[tape] = digest(tape)
            if identities[tape] != evidence[arm["domain"]]["input_sha256"]:
                raise ValueError("original input changed")
        raw = args.original / (label + ".run.json")
        identities[raw] = digest(raw)
    identities[args.engine] = digest(args.engine)
    identities[args.frozen / "policy.json"] = digest(args.frozen / "policy.json")
    identities[args.original / "report.json"] = digest(args.original / "report.json")
    identities[Path(__file__)] = digest(Path(__file__))
    args.output.mkdir()
    save(args.output / "identity.json", {
        "experiment": "quant_binance_attribution_audit", "program": "quant_profit",
        "scope": "same_decisions_required_diagnostics_only_no_new_holdout",
        "files_sha256": {str(p.resolve()): h for p, h in identities.items()}})
    results = {}
    for label, arm in policy["arms"].items():
        path = args.output / (label + ".run.json")
        with path.open("x") as stdout, path.with_suffix(".stderr").open("x") as stderr:
            try:
                code = subprocess.run([str(args.engine.resolve()),
                    str(args.frozen / (label + ".config.json")),
                    str(args.original / (arm["domain"] + ".ndjson"))],
                    stdout=stdout, stderr=stderr, timeout=3600).returncode
            except subprocess.TimeoutExpired:
                code = None
        if code == 0:
            row = load(path)
            matched, reason = verify(load(args.original / (label + ".run.json")), row)
            results[label] = {"original_reproduced": matched, "failure": reason,
                              "binance_participation": row.get("binance_participation") if matched else None}
        else:
            results[label] = {"original_reproduced": False, "failure": "diagnostic_engine_failure",
                              "engine_exit_code": code}
    if any(digest(p) != h for p, h in identities.items()):
        raise ValueError("audit input changed")
    passed = all(v["original_reproduced"] for v in results.values())
    save(args.output / "report.json", {"program": "quant_profit", "original_reproduced": passed,
        "original_comparison_complete": original["comparison_complete"], "runs": results,
        "identifies_withdrawal_counts_not_causal_pnl": True, "new_holdout": False})
    return 0 if passed else 1


if __name__ == "__main__":
    raise SystemExit(main())
