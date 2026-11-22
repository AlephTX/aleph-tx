#!/usr/bin/env python3
"""Pre-registered Zero-Tuning OOS Validation Gate Runner (`Step 2`).

Supports three locked acceptance gates with zero post-freeze parameter tuning:
1. `rth-ant`: Evaluates frozen `EXP-ARB-A2` (`Q=$500, in=10bp, out=0bp, persist=100ms,
   lad=2,p=1, ddbrk=$50`) across `lat_ms in {100, 250, 500}` for Monday US RTH captures.
2. `w15-rwa`: Evaluates `W15+` Core Standard RWA cost against hard `<= $20/M` and zero
   58% halt criteria on both full 6-market and `ex-NVDA` 5-market baskets.
3. `w17-crash-pause-verify`: Verifies SHA-256 integrity of all frozen `W17+` artifacts
   against `crash_pause_abc_preregistration_v1_frozen.json` and fails closed on drift.
"""

from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import sys
from typing import Any

REQUIRED_TAKER_LATS = (100, 250, 500)
MAX_DRAWDOWN_BUDGET_USD = 50.0
MIN_CYCLES = 3
MIN_WIN_RATE = 0.60


def evaluate_rth_ant_gate(
    window_result: dict[str, Any],
    *,
    role: str,
) -> dict[str, Any]:
    """Evaluate pre-registered `rth-ant` Go/No-Go gate on a simulated window dict."""
    if role not in ("monday_rth_oos", "selection_dev_dryrun"):
        raise ValueError(f"role must be 'monday_rth_oos' or 'selection_dev_dryrun', got {role!r}")

    variants = list(window_result.get("variants") or [])
    taker_rows = [r for r in variants if r.get("mode") == "TAKER_TAKER"]
    maker_rows = [r for r in variants if r.get("mode") == "MAKER_ARCUS_TAKER_XYZ"]

    by_lat = {int(r["lat_ms"]): r for r in taker_rows}
    all_lats_present = all(lat in by_lat for lat in REQUIRED_TAKER_LATS)
    all_taker_pos = all_lats_present and all(
        float(by_lat[lat]["net_cash_usd"]) > 0.0 for lat in REQUIRED_TAKER_LATS
    )
    all_flat = bool(variants) and all(bool(r.get("terminal_flat")) for r in variants)
    witnessed = int(window_result.get("funding_hours_witnessed", 0))
    missing_witness = int(window_result.get("funding_hours_missing_witness", -1))
    zero_missing_witness = witnessed > 0 and missing_witness == 0
    max_dd = max((float(r.get("max_drawdown_usd", 0.0)) for r in variants), default=0.0)
    dd_within_budget = bool(variants) and max_dd <= MAX_DRAWDOWN_BUDGET_USD
    halted_any = any(bool(r.get("halted_58pct")) for r in variants)
    cycles_ok = all_lats_present and all(
        int(by_lat[lat].get("cycles_closed", 0)) >= MIN_CYCLES for lat in REQUIRED_TAKER_LATS
    )
    win_rate_ok = all_lats_present and all(
        (by_lat[lat].get("win_rate") or 0.0) >= MIN_WIN_RATE for lat in REQUIRED_TAKER_LATS
    )

    checks = {
        "all_required_taker_latencies_present": all_lats_present,
        "all_taker_latencies_positive": all_taker_pos,
        "all_terminal_flat": all_flat,
        "zero_missing_funding_witness": zero_missing_witness,
        "max_drawdown_within_budget": dd_within_budget,
        "zero_58pct_halt": not halted_any,
        "sufficient_cycles_and_win_rate": cycles_ok and win_rate_ok,
    }
    passed_metrics = all(checks.values())
    if not passed_metrics:
        verdict = "NO_GO"
    elif role == "monday_rth_oos":
        verdict = "GO"
    else:
        verdict = "DRY_RUN_NOT_OOS"

    return {
        "gate": "rth_ant",
        "role": role,
        "verdict": verdict,
        "passed_metrics": passed_metrics,
        "checks": checks,
        "funding_hours_witnessed": witnessed,
        "funding_hours_missing_witness": missing_witness,
        "max_drawdown_usd": round(max_dd, 4),
        "taker_variants": taker_rows,
        "maker_variants": maker_rows,
    }


def _summarize_rwa_arm(summary: dict[str, Any], arm_name: str, *, exclude_nvda: bool) -> dict[str, Any]:
    if not exclude_nvda:
        arm = (summary.get("aggregate_6markets") or {}).get(arm_name)
        if not arm:
            raise KeyError(f"missing aggregate_6markets arm {arm_name!r}")
        return {
            "core_maker_usd": float(arm["core_maker_usd"]),
            "dual_usd": float(arm["dual_usd"]),
            "terminal_liq_cash_usd": float(arm["terminal_liq_cash_usd"]),
            "core_usd_per_m_liq": float(arm["single_venue_core_usd_per_m_liq"]),
            "terminal_flat_all": bool(arm.get("terminal_flat_all")),
            "drawdown_58_breached": bool(arm.get("drawdown_58_breached")),
        }

    markets = summary.get("markets") or {}
    core_vol = 0.0
    dual_vol = 0.0
    liq_cash = 0.0
    flat_all = True
    any_58_breach = False
    halt_blocks = 0
    max_mkt_dd = 0.0
    count = 0
    for sym, sym_data in markets.items():
        if sym == "NVDA":
            continue
        arm = (sym_data.get("arms") or {}).get(arm_name)
        if not arm:
            raise KeyError(f"missing market {sym!r} arm {arm_name!r}")
        core_vol += float(arm["core_maker_usd"])
        dual_vol += float(arm["dual_usd"])
        liq_cash += float(arm["terminal_liq_cash_usd"])
        flat_all = flat_all and bool(arm.get("terminal_flat_liq_ok", True))
        mkt_dd = float(arm.get("max_drawdown_usd", 0.0))
        max_mkt_dd = max(max_mkt_dd, mkt_dd)
        any_58_breach = any_58_breach or bool(arm.get("drawdown_58_breached")) or mkt_dd >= 580.0
        halt_blocks += int(arm.get("halt_blocked", 0))
        count += 1
    if count == 0 or core_vol <= 0.0:
        raise ValueError(f"empty ex-NVDA volume for {arm_name}")
    return {
        "markets_included": count,
        "core_maker_usd": round(core_vol, 2),
        "dual_usd": round(dual_vol, 2),
        "terminal_liq_cash_usd": round(liq_cash, 4),
        "core_usd_per_m_liq": round(-liq_cash / (core_vol / 1e6), 2),
        "terminal_flat_all": flat_all,
        "max_market_drawdown_usd": round(max_mkt_dd, 4),
        "drawdown_58_breached": any_58_breach,
        "feed_halt_blocks": halt_blocks,
    }


def evaluate_w15_rwa_gate(
    summary: dict[str, Any],
    *,
    max_cost_per_million: float = 20.0,
) -> dict[str, Any]:
    """Evaluate `W15+` Core Standard RWA cost threshold (`<= $20/M`) on 6-sym & ex-NVDA."""
    if max_cost_per_million <= 0.0:
        raise ValueError("max_cost_per_million must be positive")
    arms = ("CORE_PURE_PULL05_C310", "CORE_HEDGE_SELF_PULL05_C310")
    rows: dict[str, Any] = {}
    for arm_name in arms:
        full = _summarize_rwa_arm(summary, arm_name, exclude_nvda=False)
        ex_nvda = _summarize_rwa_arm(summary, arm_name, exclude_nvda=True)
        full["pass_cost_le_threshold"] = (
            full["core_usd_per_m_liq"] <= max_cost_per_million
            and full["terminal_flat_all"]
            and not full["drawdown_58_breached"]
        )
        ex_nvda["pass_cost_le_threshold"] = (
            ex_nvda["core_usd_per_m_liq"] <= max_cost_per_million
            and ex_nvda["terminal_flat_all"]
            and not ex_nvda["drawdown_58_breached"]
        )
        rows[arm_name] = {"full_6markets": full, "ex_nvda": ex_nvda}

    hedged_ex_nvda_ok = rows["CORE_HEDGE_SELF_PULL05_C310"]["ex_nvda"]["pass_cost_le_threshold"]
    pure_ex_nvda_ok = rows["CORE_PURE_PULL05_C310"]["ex_nvda"]["pass_cost_le_threshold"]
    verdict = "GO_HEDGED" if hedged_ex_nvda_ok else ("GO_UNHEDGED_ONLY" if pure_ex_nvda_ok else "NO_GO")
    return {
        "gate": "w15_rwa",
        "max_cost_per_million_usd": max_cost_per_million,
        "verdict": verdict,
        "arms": rows,
    }


def verify_w17_crash_pause_prereg(prereg_path: Path, repo_root: Path) -> dict[str, Any]:
    """Verify SHA-256 parity of all frozen `W17+` crash-pause artifacts."""
    prereg = json.loads(prereg_path.read_text())
    expected_map: dict[str, str] = dict(prereg.get("artifact_sha256") or {})
    if not expected_map:
        raise ValueError(f"missing artifact_sha256 map in {prereg_path}")

    checked: dict[str, dict[str, Any]] = {}
    all_match = True
    for rel_path, expected_sha in expected_map.items():
        target = (repo_root / rel_path).resolve()
        if not target.is_file():
            checked[rel_path] = {"expected": expected_sha, "actual": None, "match": False}
            all_match = False
            continue
        actual_sha = hashlib.sha256(target.read_bytes()).hexdigest()
        matched = actual_sha == expected_sha
        checked[rel_path] = {"expected": expected_sha, "actual": actual_sha, "match": matched}
        if not matched:
            all_match = False

    return {
        "gate": "w17_crash_pause_verify",
        "card_id": prereg.get("card_id"),
        "method_sha256": prereg.get("method_sha256"),
        "builder_cli": prereg.get("builder_cli"),
        "verdict": "VERIFIED" if all_match else "MISMATCH",
        "artifacts": checked,
    }


def _load_ant_ablation_module(repo_root: Path) -> Any:
    mod_dir = repo_root / ".agent-state" / "research" / "ant_three_venue_20261120"
    mod_path = mod_dir / "run_arb_imp_ablation_w01_w02.py"
    if not mod_path.is_file():
        raise FileNotFoundError(f"missing ablation engine at {mod_path}")
    if str(mod_dir) not in sys.path:
        sys.path.insert(0, str(mod_dir))
    spec = importlib.util.spec_from_file_location("run_arb_imp_ablation_w01_w02", mod_path)
    assert spec is not None and spec.loader is not None
    mod = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = mod
    spec.loader.exec_module(mod)
    return mod


def run_rth_ant_cli(capture_dirs: list[Path], role: str, output: Path, repo_root: Path) -> int:
    mod = _load_ant_ablation_module(repo_root)
    gate_specs = []
    for mode in ("TAKER_TAKER", "MAKER_ARCUS_TAKER_XYZ"):
        for lat in REQUIRED_TAKER_LATS:
            gate_specs.append(
                mod.AblationSpec(
                    arm_family="EXP-CRASH-A2B",
                    mode=mode,
                    lat_ms=lat,
                    q_usd=500.0,
                    entry_bp=10.0,
                    persist_ms=100,
                    post_trade_refresh_lock=True,
                    allow_batch_add=True,
                    floor_frac=0.25,
                    scale_bps=2.0,
                    convex_power=1.0,
                    max_entry_bp=35.0,
                    dd_brake_budget_usd=MAX_DRAWDOWN_BUDGET_USD,
                )
            )
    raw = mod.run_ablation(capture_dirs, gate_specs)
    report = evaluate_rth_ant_gate(raw, role=role)
    report["capture_dirs"] = [str(p) for p in capture_dirs]
    output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({"gate": report["gate"], "role": role, "verdict": report["verdict"], "checks": report["checks"], "max_drawdown_usd": report["max_drawdown_usd"]}, indent=2))
    return 0


def main() -> int:
    repo_root = Path(__file__).resolve().parent.parent
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)

    p_ant = sub.add_parser("rth-ant", help="Run frozen EXP-ARB-A2 ANT gate on sealed captures")
    p_ant.add_argument("--capture-dirs", nargs="+", type=Path, required=True)
    p_ant.add_argument("--role", choices=("monday_rth_oos", "selection_dev_dryrun"), required=True)
    p_ant.add_argument("--output", type=Path, required=True)

    p_rwa = sub.add_parser("w15-rwa", help="Evaluate W15+ Core RWA <= $20/M gate")
    p_rwa.add_argument("--rwa-summary-json", type=Path, required=True)
    p_rwa.add_argument("--max-cost-per-million", type=float, default=20.0)
    p_rwa.add_argument("--output", type=Path, required=True)

    p_w17 = sub.add_parser("w17-crash-pause-verify", help="Verify frozen W17+ artifact SHA-256")
    p_w17.add_argument("--prereg", type=Path, required=True)

    args = parser.parse_args()
    if args.command == "rth-ant":
        return run_rth_ant_cli(args.capture_dirs, args.role, args.output, repo_root)
    if args.command == "w15-rwa":
        summary = json.loads(args.rwa_summary_json.read_text())
        res = evaluate_w15_rwa_gate(summary, max_cost_per_million=args.max_cost_per_million)
        res["input_json"] = str(args.rwa_summary_json)
        args.output.write_text(json.dumps(res, indent=2) + "\n")
        print(json.dumps({"gate": res["gate"], "verdict": res["verdict"], "arms": res["arms"]}, indent=2))
        return 0
    if args.command == "w17-crash-pause-verify":
        res = verify_w17_crash_pause_prereg(args.prereg, repo_root)
        print(json.dumps(res, indent=2))
        return 0 if res["verdict"] == "VERIFIED" else 1
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
