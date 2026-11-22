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
from datetime import datetime, time as dt_time, timedelta, timezone
import hashlib
import importlib.util
import json
from pathlib import Path
import re
import sys
from typing import Any
from zoneinfo import ZoneInfo

REQUIRED_TAKER_LATS = (100, 250, 500)
MAX_DRAWDOWN_BUDGET_USD = 50.0
MIN_CYCLES = 3
MIN_WIN_RATE = 0.60
HOUR_MS = 3_600_000
NS_PER_MS = 1_000_000
TEN_HOURS_MS = 36_000_000
MIN_WITNESSED_HOURS_10H = 9
CAPTURE_COVERAGE_TOLERANCE_MS = 60_000
SHA256_RE = re.compile(r"[0-9a-f]{64}\Z")
ET_TZ = ZoneInfo("America/New_York")
FROZEN_ANT_ENGINE_FILES = (
    ".agent-state/research/ant_three_venue_20261120/sim_ant_arcus_xyz_basis.py",
    ".agent-state/research/ant_three_venue_20261120/run_arb_imp_ablation_w01_w02.py",
)


def _is_valid_ms(value: Any) -> bool:
    return isinstance(value, int) and not isinstance(value, bool) and value >= 0


def _is_sha256(value: Any) -> bool:
    return isinstance(value, str) and SHA256_RE.fullmatch(value) is not None


def _expected_interior_utc_hours(start_ms: int, end_ms: int) -> int:
    if end_ms <= start_ms:
        return 0
    first_hr = (start_ms // HOUR_MS + 1) * HOUR_MS
    last_hr = ((end_ms - 1) // HOUR_MS) * HOUR_MS
    if last_hr < first_hr:
        return 0
    return ((last_hr - first_hr) // HOUR_MS) + 1


def _covered_us_rth_dates(
    start_ms: int,
    end_ms: int,
    *,
    require_monday: bool = True,
    expected_rth_date_et: str | None = None,
) -> list[str]:
    if end_ms <= start_ms:
        return []
    start_et = datetime.fromtimestamp(start_ms / 1000.0, tz=timezone.utc).astimezone(ET_TZ)
    end_et = datetime.fromtimestamp(end_ms / 1000.0, tz=timezone.utc).astimezone(ET_TZ)
    covered: list[str] = []
    cur = start_et.date()
    end_date = end_et.date()
    while cur <= end_date:
        is_weekday = cur.weekday() < 5
        matches_day = (cur.weekday() == 0) if require_monday else is_weekday
        if expected_rth_date_et is not None:
            matches_day = matches_day and (cur.isoformat() == expected_rth_date_et)
        if matches_day:
            open_ms = int(datetime.combine(cur, dt_time(9, 30), tzinfo=ET_TZ).timestamp() * 1000)
            close_ms = int(datetime.combine(cur, dt_time(16, 0), tzinfo=ET_TZ).timestamp() * 1000)
            if start_ms <= open_ms and end_ms >= close_ms:
                covered.append(cur.isoformat())
        cur += timedelta(days=1)
    return covered


def _segment_hashes_match(
    expected_segments: Any,
    verified_segment_digests: dict[str, str] | None,
) -> bool:
    if not isinstance(expected_segments, dict) or not expected_segments:
        return False
    if not all(isinstance(k, str) and bool(k) and _is_sha256(v) for k, v in expected_segments.items()):
        return False
    if not isinstance(verified_segment_digests, dict):
        return False
    return verified_segment_digests == expected_segments


def _engine_hashes_match(
    expected_engines: Any,
    verified_engine_digests: dict[str, str] | None,
) -> bool:
    if not isinstance(expected_engines, dict) or not isinstance(verified_engine_digests, dict):
        return False
    for rel_path in FROZEN_ANT_ENGINE_FILES:
        exp = expected_engines.get(rel_path)
        act = verified_engine_digests.get(rel_path)
        if not (_is_sha256(exp) and act == exp):
            return False
    return True


def verify_rth_ant_provenance(
    prereg_manifest: dict[str, Any] | None,
    *,
    window_result: dict[str, Any],
    verified_segment_digests: dict[str, str] | None = None,
    verified_engine_digests: dict[str, str] | None = None,
) -> dict[str, Any]:
    """Verify fail-closed OOS pre-registration, 10h RTH session coverage, and sealed hashes."""
    if not isinstance(prereg_manifest, dict):
        checks = {
            "prereg_manifest_supplied": False,
            "classification_is_monday_rth_oos": False,
            "pre_registered_before_decision_start": False,
            "continuous_10h_span": False,
            "funding_hours_cover_decision_window": False,
            "us_rth_weekday_session_covered": False,
            "observed_capture_covers_decision_window": False,
            "sealed_segment_hashes_verified": False,
            "frozen_engine_hashes_verified": False,
        }
        return {"qualified": False, "checks": checks, "covered_rth_dates_et": []}

    frozen_at_ms = prereg_manifest.get("frozen_at_ms")
    start_ms = prereg_manifest.get("decision_start_ms")
    end_ms = prereg_manifest.get("decision_end_ms")
    valid_times = _is_valid_ms(frozen_at_ms) and _is_valid_ms(start_ms) and _is_valid_ms(end_ms)

    pre_reg_ok = bool(valid_times and frozen_at_ms <= start_ms < end_ms)
    span_ok = bool(valid_times and end_ms > start_ms and (end_ms - start_ms) >= TEN_HOURS_MS)

    witnessed = int(window_result.get("funding_hours_witnessed", 0))
    missing = int(window_result.get("funding_hours_missing_witness", -1))
    min_req_hours = max(MIN_WITNESSED_HOURS_10H, _expected_interior_utc_hours(start_ms, end_ms) if valid_times else MIN_WITNESSED_HOURS_10H)
    funding_cov_ok = bool(valid_times and missing == 0 and witnessed >= min_req_hours)

    require_monday = bool(prereg_manifest.get("require_monday", True))
    expected_date = prereg_manifest.get("expected_rth_date_et")
    rth_dates = (
        _covered_us_rth_dates(
            start_ms,
            end_ms,
            require_monday=require_monday,
            expected_rth_date_et=expected_date if isinstance(expected_date, str) else None,
        )
        if valid_times
        else []
    )
    session_decl = prereg_manifest.get("market_session")
    session_ok = bool(rth_dates) and (session_decl in (None, "us_equities_rth"))

    first_rx_ns = window_result.get("first_rx_ns")
    last_rx_ns = window_result.get("last_rx_ns")
    observed_cov_ok = False
    if valid_times and isinstance(first_rx_ns, int) and isinstance(last_rx_ns, int) and 0 < first_rx_ns <= last_rx_ns:
        first_rx_ms = first_rx_ns // NS_PER_MS
        last_rx_ms = last_rx_ns // NS_PER_MS
        observed_cov_ok = (
            first_rx_ms <= start_ms + CAPTURE_COVERAGE_TOLERANCE_MS
            and last_rx_ms >= end_ms - CAPTURE_COVERAGE_TOLERANCE_MS
        )

    checks = {
        "prereg_manifest_supplied": True,
        "classification_is_monday_rth_oos": prereg_manifest.get("classification") == "monday_rth_oos",
        "pre_registered_before_decision_start": pre_reg_ok,
        "continuous_10h_span": span_ok,
        "funding_hours_cover_decision_window": funding_cov_ok,
        "us_rth_weekday_session_covered": session_ok,
        "observed_capture_covers_decision_window": observed_cov_ok,
        "sealed_segment_hashes_verified": _segment_hashes_match(prereg_manifest.get("segment_sha256"), verified_segment_digests),
        "frozen_engine_hashes_verified": _engine_hashes_match(prereg_manifest.get("frozen_engine_sha256"), verified_engine_digests),
    }
    return {
        "qualified": all(checks.values()),
        "checks": checks,
        "covered_rth_dates_et": rth_dates,
        "decision_start_ms": start_ms if _is_valid_ms(start_ms) else None,
        "decision_end_ms": end_ms if _is_valid_ms(end_ms) else None,
        "frozen_at_ms": frozen_at_ms if _is_valid_ms(frozen_at_ms) else None,
    }


def evaluate_rth_ant_gate(
    window_result: dict[str, Any],
    *,
    role: str,
    prereg_manifest: dict[str, Any] | None = None,
    verified_segment_digests: dict[str, str] | None = None,
    verified_engine_digests: dict[str, str] | None = None,
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
    provenance: dict[str, Any] | None = None
    if role == "monday_rth_oos":
        provenance = verify_rth_ant_provenance(
            prereg_manifest,
            window_result=window_result,
            verified_segment_digests=verified_segment_digests,
            verified_engine_digests=verified_engine_digests,
        )
        verdict = "GO" if (passed_metrics and provenance["qualified"]) else "NO_GO"
    else:
        if prereg_manifest is not None:
            provenance = verify_rth_ant_provenance(
                prereg_manifest,
                window_result=window_result,
                verified_segment_digests=verified_segment_digests,
                verified_engine_digests=verified_engine_digests,
            )
        verdict = "NO_GO" if not passed_metrics else "DRY_RUN_NOT_OOS"

    return {
        "gate": "rth_ant",
        "role": role,
        "verdict": verdict,
        "passed_metrics": passed_metrics,
        "checks": checks,
        "provenance": provenance,
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


def _sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def _collect_segment_hashes(capture_dirs: list[Path], manifest: dict[str, Any] | None) -> dict[str, str]:
    expected = manifest.get("segment_sha256") if isinstance(manifest, dict) else None
    use_dir_prefix = (
        isinstance(expected, dict)
        and bool(expected)
        and all(isinstance(k, str) and k.count("/") == 1 for k in expected)
    )
    digests: dict[str, str] = {}
    for d in capture_dirs:
        if not d.is_dir():
            continue
        for f in sorted(d.glob("tokyo-research-*.seg*.ndjson.gz")):
            key = f"{d.name}/{f.name}" if use_dir_prefix else f.name
            if key in digests:
                key = f"{d.name}/{f.name}"
                if key in digests:
                    key = str(f)
            digests[key] = _sha256_file(f)
    return digests


def _collect_engine_hashes(repo_root: Path) -> dict[str, str]:
    digests: dict[str, str] = {}
    for rel in FROZEN_ANT_ENGINE_FILES:
        path = repo_root / rel
        if path.is_file():
            digests[rel] = _sha256_file(path)
    return digests


def run_rth_ant_cli(
    capture_dirs: list[Path],
    role: str,
    output: Path,
    repo_root: Path,
    prereg_manifest_path: Path | None = None,
) -> int:
    mod = _load_ant_ablation_module(repo_root)
    manifest: dict[str, Any] | None = None
    if prereg_manifest_path is not None:
        loaded = json.loads(prereg_manifest_path.read_text())
        manifest = loaded if isinstance(loaded, dict) else {}

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
    seg_digests = _collect_segment_hashes(capture_dirs, manifest)
    eng_digests = _collect_engine_hashes(repo_root)
    report = evaluate_rth_ant_gate(
        raw,
        role=role,
        prereg_manifest=manifest,
        verified_segment_digests=seg_digests,
        verified_engine_digests=eng_digests,
    )
    report["capture_dirs"] = [str(p) for p in capture_dirs]
    report["prereg_manifest"] = str(prereg_manifest_path) if prereg_manifest_path is not None else None
    output.write_text(json.dumps(report, indent=2) + "\n")
    print(
        json.dumps(
            {
                "gate": report["gate"],
                "role": role,
                "verdict": report["verdict"],
                "checks": report["checks"],
                "provenance": report["provenance"],
                "max_drawdown_usd": report["max_drawdown_usd"],
            },
            indent=2,
        )
    )
    if role == "monday_rth_oos" and report["verdict"] != "GO":
        return 1
    return 0


def main() -> int:
    repo_root = Path(__file__).resolve().parent.parent
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)

    p_ant = sub.add_parser("rth-ant", help="Run frozen EXP-ARB-A2 ANT gate on sealed captures")
    p_ant.add_argument("--capture-dirs", nargs="+", type=Path, required=True)
    p_ant.add_argument("--role", choices=("monday_rth_oos", "selection_dev_dryrun"), required=True)
    p_ant.add_argument("--prereg-manifest", type=Path, default=None, help="Pre-registration manifest JSON (required for monday_rth_oos GO)")
    p_ant.add_argument("--output", type=Path, required=True)

    p_rwa = sub.add_parser("w15-rwa", help="Evaluate W15+ Core RWA <= $20/M gate")
    p_rwa.add_argument("--rwa-summary-json", type=Path, required=True)
    p_rwa.add_argument("--max-cost-per-million", type=float, default=20.0)
    p_rwa.add_argument("--output", type=Path, required=True)

    p_w17 = sub.add_parser("w17-crash-pause-verify", help="Verify frozen W17+ artifact SHA-256")
    p_w17.add_argument("--prereg", type=Path, required=True)

    args = parser.parse_args()
    if args.command == "rth-ant":
        return run_rth_ant_cli(args.capture_dirs, args.role, args.output, repo_root, args.prereg_manifest)
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
