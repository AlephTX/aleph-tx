#!/usr/bin/env python3
"""Freeze RH/Core turnover budgets and measure complete-cycle cost versus volume."""
import argparse
from concurrent.futures import ThreadPoolExecutor, as_completed
from decimal import Decimal
import math
import os
from pathlib import Path

from archive_public_capture import digest
from replay_maker_campaign import load, save
from replay_ten_hour_scenarios import TEN_HOURS, execute, turnover_policies
from replay_trade_preflight import audit

COST_CAPS = ("0", "0.5", "1", "2", "4", "8")


def decimal(value):
    return Decimal(str(value))


def serialized_sum_matches(values, total):
    """Rust reconciles exact cash; legacy JSON floats can differ by rounding.

    String-encoded decimals get no tolerance. For each legacy float allow
    at most one ULP, covering Decimal -> f64 -> shortest JSON conversion.
    This check never substitutes for the engine's exact cycle proof.
    """
    values = list(values)
    numbers = values + [total]
    bound = sum((Decimal(str(math.ulp(v))) for v in numbers if isinstance(v, float)), Decimal(0))
    return abs(sum(map(decimal, values), Decimal(0)) - decimal(total)) <= bound


def reward_eligibility(target, candidate, net):
    """Denominator-only scenarios, never a claim of awarded points or revenue.

    Public prints do not establish counterparty tier or program qualification.
    Even Premium is not a guarantee: venue exclusions/discretion still apply.
    """
    if candidate is not None and not Decimal(0) <= candidate <= target:
        raise ValueError("candidate turnover must be within target turnover")
    scenarios = {}
    for fraction in ("0", "0.2", "0.5", "0.8", "1"):
        volume = candidate * decimal(fraction) if candidate is not None else None
        scenarios[fraction] = {
            "assumed_eligible_turnover": str(volume) if volume is not None else None,
            "cost_bps_per_assumed_eligible_turnover": (
                str(-net / volume * 10_000) if net is not None and volume else None),
        }
    return {"status": "unknown_counterparty_tier_and_program_qualification",
            "confirmed_eligible_turnover": None,
            "rule_window_candidate_turnover": str(candidate) if candidate is not None else None,
            "cash_points_value": "0", "scope": "fixed_cash_ledger_denominator_only_no_order_replay",
            "standard_to_standard_rewarded": False, "premium_guarantees_rewards": False,
            "fraction_scenarios": scenarios}


def point(row):
    feedback = row.get("turnover_feedback") or {}
    target = decimal(feedback.get("modeled_target_turnover_quote", 0))
    complete = (row.get("engine_exit_code") == 0 and row.get("incomplete") is None
                and row.get("net_pnl") is not None and feedback.get("net_after_overhead") is not None
                and all(decimal(p) == 0 for p in row.get("open_positions_base", [None])))
    diagnostics = row.get("execution_costs") or {}
    cycles = diagnostics.get("cycles", [])
    exact_proof = "whole_ledger_cash_reconciled" in diagnostics
    full_net = diagnostics.get("whole_ledger_net_before_overhead") if exact_proof else row.get("net_pnl")
    cash_complete = (complete and len(cycles) == row.get("closed_cycles")
                     and all(c["completed"] and c["cash_reconciled"] for c in cycles)
                     and (not exact_proof or diagnostics["whole_ledger_cash_reconciled"] is True)
                     and full_net is not None
                     and serialized_sum_matches((c["net_before_overhead"] for c in cycles), full_net))
    price_complete = cash_complete and all(c["price_decomposition_reconciled"] for c in cycles)
    net = decimal(feedback["net_after_overhead"]) if cash_complete else None
    costs = None if net is None or not target else -net / target * 10_000
    candidate = feedback.get("modeled_eligible_turnover_quote") if cash_complete else None
    eligibility = reward_eligibility(target, decimal(candidate) if candidate is not None else None, net)
    stages = {}
    if price_complete:
        for phase in ("entry_maker", "entry_hedge", "close_maker", "close_hedge", "forced_close"):
            stages[phase] = str(sum((decimal(s["execution_cost_quote"]) for c in cycles
                                     for s in c["stages"] if s["phase"] == phase), Decimal(0)))
    forced_cost = decimal(stages["forced_close"]) if price_complete else None
    forced_stages = [s for c in cycles for s in c["stages"] if s["phase"] == "forced_close"]
    marks = {}
    for horizon in (1000, 5000):
        rows = [m for c in cycles for m in c["entry_markouts"] if m["horizon_ms"] == horizon]
        volume = sum((decimal(m["marked_turnover"]) for m in rows), Decimal(0))
        marks[str(horizon)] = {k: sum(m[k] for m in rows) for k in
                              ("marked_fills", "expired_fills", "missing_entry_mid", "pending_at_cutoff")}
        marks[str(horizon)] |= {"marked_turnover": str(volume),
            "mid_move_cost_bps": (str(sum((decimal(m["mid_move_cost_quote"]) for m in rows), Decimal(0))
                                       / volume * 10_000) if volume else None)}
    return {
        "engine_exit_code": row.get("engine_exit_code"), "incomplete": row.get("incomplete"),
        "conservative_cancel_guard": row.get("conservative_cancel_guard", False),
        "cancel_guard_overlap_prints": row.get("cancel_guard_overlap_prints"),
        "cancel_guard_overlap_fills": row.get("cancel_guard_overlap_fills"),
        "cancel_guard_overlap_turnover": row.get("cancel_guard_overlap_turnover"),
        "maker_fills_after_cancel_request": row.get("maker_fills_after_cancel_request"),
        "ledger_complete": complete, "cash_attribution_reconciled": cash_complete,
        "cash_proof": "engine_exact_full_ledger" if exact_proof else "legacy_json_rounding",
        "net_after_overhead_exact": diagnostics.get("whole_ledger_net_after_overhead") if cash_complete else None,
        "price_decomposition_reconciled": price_complete,
        "closed_cycles": row.get("closed_cycles", 0), "observed_target_turnover": str(target),
        "net_after_overhead": str(net) if net is not None else None,
        "realized_cost_bps_per_target_turnover": str(costs) if costs is not None else None,
        "cost_quote_per_million_target_turnover": str(costs * 100) if costs is not None else None,
        "price_loss_quote": (str(-decimal(row["pnl_before_fees"])) if cash_complete else None),
        "explicit_fees_quote": str(row["fees"]) if cash_complete else None,
        "modeled_overhead_quote": str(feedback["overhead_quote"]) if cash_complete else None,
        "execution_cost_by_stage": stages if price_complete else None,
        "reward_eligibility": eligibility,
        "forced_close_fills": sum(s["fills"] for s in forced_stages) if cash_complete else None,
        "forced_close_cycles": sum(any(s["phase"] == "forced_close" and s["fills"] > 0
                                       for s in c["stages"]) for c in cycles) if cash_complete else None,
        "forced_close_execution_cost_quote": str(forced_cost) if forced_cost is not None else None,
        "forced_close_signed_fraction_of_total_cost": (str(forced_cost / -net)
            if forced_cost is not None and net is not None and net < 0 else None),
        "forced_share_basis": "signed_execution_cost_over_positive_total_cost_including_drift_fees_overhead_not_bounded",
        "price_drift_both_legs_quote": (str(sum((decimal(c["price_drift_cost_quote"]) for c in cycles), Decimal(0)))
                                        if price_complete else None),
        "opening_hedge_slippage_vs_submit_touch_nonadditive": (
            str(sum((decimal(c["opening_hedge_slippage_vs_submit_touch"]) for c in cycles), Decimal(0)))
            if cash_complete else None),
        "entry_markouts_nonadditive": marks,
        "max_unhedged_ms": row.get("max_unhedged_ms"), "max_net_notional": row.get("max_net_notional"),
        "funding_flat": row.get("funding_flat"),
        "sample_scope": "single_selected_window_not_a_population_cost_estimate",
        "sparse_cycles": row.get("closed_cycles", 0) < 30,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("engine", type=Path)
    parser.add_argument("prepared", type=Path, help="hash-bound RH/Core inputs and public provenance")
    parser.add_argument("output", type=Path)
    parser.add_argument("--workers", type=int, choices=(1, 2), default=2)
    args = parser.parse_args()
    os.umask(0o077)
    engine, prepared, out = (p.resolve() for p in (args.engine, args.prepared, args.output))
    source_path = prepared / "manifest.json"
    source_hash = digest(source_path)
    source = load(source_path)
    if (source_hash != digest(source_path) or source["sample"] != "historical_selection"
            or source["numeraire_assumption"] != "USDG_USDC_parity_unverified"
            or source["execution_qualified"] is not False):
        raise ValueError("explicit selection and unverified currency assumptions are required")
    out.mkdir()
    for name in ("configs", "runs"):
        (out / name).mkdir()
    frozen, inputs, admissions = {}, {}, {}
    for symbol in ("BTC", "ETH"):
        spec = source["markets"][symbol]
        window = spec["window"]
        if window["end_ms"] - window["start_ms"] != TEN_HOURS:
            raise ValueError("expected one continuous ten-hour window")
        tape, pair_file = prepared / spec["input"], prepared / spec["pair"]
        if digest(tape) != spec["input_sha256"] or digest(pair_file) != spec["pair_sha256"]:
            raise ValueError("prepared input identity mismatch")
        pair = load(pair_file)
        if [p["venue"] for p in pair["legs"]] != ["lighter", "lighter_robinhood"]:
            raise ValueError("this curve prioritizes the RH/Core route")
        inputs[symbol] = tape
        policies = list(turnover_policies(pair, window, ("0", "0"), cost_caps=COST_CAPS))
        for label, config in policies:
            label = symbol + "-" + label
            config["diagnose_cycles"] = True
            path = out / "configs" / (label + ".json")
            save(path, config)
            frozen[label] = {"symbol": symbol, "config": path, "sha256": digest(path),
                             "cost_cap_bps": config["pair"]["mode"]["incentive"]["max_cost_bps"]}
        admissions[symbol] = audit(tape, policies[0][1])
        save(out / (symbol + ".admission.json"), admissions[symbol])
    # Every asset, budget and posture is frozen before any engine is invoked.
    policy = {"sample": "historical_selection", "program": "turnover", "route": "RH_Core",
              "execution_qualified": False, "source_manifest_sha256": source_hash,
              "source_evidence": source, "cost_caps_bps": COST_CAPS,
              "cost_cap_quote_per_million": {v: str(decimal(v) * 100) for v in COST_CAPS},
              "engine_sha256": digest(engine), "runner_sha256": digest(Path(__file__)),
              "config_sha256": {n: f["sha256"] for n, f in frozen.items()},
              "modeled_overhead_source": "uncalibrated_research_assumption_not_exchange_fee",
              "reward_eligibility": "unknown; admission budgets use target-volume assumptions, not confirmed points eligibility",
              "max_workers": args.workers, "observed_volume_is_not_a_capacity_guarantee": True}
    save(out / "policy.json", policy)
    results = {}
    def run(label, spec):
        path = spec["config"]
        if digest(path) != spec["sha256"]:
            raise ValueError("frozen configuration changed")
        if not admissions[spec["symbol"]]["trade_timing_admitted"]:
            return {"ledger_complete": False, "incomplete": "input_trade_timing_rejected",
                    "blockers": admissions[spec["symbol"]]["blockers"]}
        row = execute(engine, path, inputs[spec["symbol"]], out / "runs" / (label + ".json"), True)
        result = point(row)
        if digest(path) != spec["sha256"]:
            raise ValueError("frozen configuration changed during replay")
        result["cost_cap_bps"] = spec["cost_cap_bps"]
        result["cost_cap_quote_per_million"] = str(decimal(spec["cost_cap_bps"]) * 100)
        return result
    with ThreadPoolExecutor(max_workers=args.workers) as pool:
        jobs = {pool.submit(run, label, spec): label for label, spec in frozen.items()}
        for future in as_completed(jobs):
            label = jobs[future]
            try:
                results[label] = future.result()
            except Exception as error:
                results[label] = {"ledger_complete": False, "incomplete": "runner_failure",
                                  "error_type": type(error).__name__}
            save(out / "report.json", policy | {"runs": results, "matrix_finished": False})
    if (digest(engine) != policy["engine_sha256"] or digest(source_path) != source_hash
            or any(digest(p) != source["markets"][s]["input_sha256"] for s, p in inputs.items())):
        raise ValueError("frozen engine or inputs changed during matrix")
    complete = sum(r.get("ledger_complete") is True and r.get("cash_attribution_reconciled") is True
                   for r in results.values())
    save(out / "report.json", policy | {"runs": results, "matrix_finished": True,
                                       "complete_ledgers": complete, "total_arms": len(frozen)})
    return 0 if complete == len(frozen) else 1


if __name__ == "__main__":
    raise SystemExit(main())
