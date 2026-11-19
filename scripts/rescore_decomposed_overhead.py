#!/usr/bin/env python3
"""Decompose modeled 0.25 bps/leg gross overhead into variable execution costs + fixed $/day infra.

Reads existing local `.agent-state/forward_ops_20261118/` score files STRICTLY READ-ONLY
without altering any frozen ledgers. Re-scores cohorts under:
  1. Legacy model: Net_after_0.25bp_gross (as originally scored in scripts/rescore)
  2. Zero variable overhead ($0/M variable assumed): Net_before_modeled_overhead = Net_0.25 + 0.25bps * Gross
  3. Decomposed all-in: Zero variable overhead MINUS prorated fixed infrastructure ($50/30d = $1.6667/d = $0.6944/10h),
     reported at both observed turnover pace and amortized $50M/30d target pace ($1.00/M fixed).
"""
import argparse
from decimal import Decimal
import json
from pathlib import Path

from audit_lighter_program_scope import score_program_cost_effectiveness

DEFAULT_FIXED_INFRA_USD_PER_DAY = Decimal("50") / Decimal("30")
DEFAULT_HOURS_PER_WINDOW = Decimal("10")


def _d(val):
    return Decimal(str(val))


def rescore_windows(
    rows,
    *,
    legacy_overhead_bps=Decimal("0.25"),
    fixed_overhead_usd_per_day=DEFAULT_FIXED_INFRA_USD_PER_DAY,
    hours_per_window=DEFAULT_HOURS_PER_WINDOW,
    fixed_amortized_30d_millions=Decimal("50"),
):
    """Aggregate legacy vs decomposed variable + fixed-infra economics over settled windows."""
    if fixed_overhead_usd_per_day < 0 or legacy_overhead_bps < 0 or hours_per_window <= 0:
        raise ValueError("overhead and window parameters must be valid")
    fixed_per_win = fixed_overhead_usd_per_day * hours_per_window / Decimal(24)
    n = len(rows)
    maker = sum((_d(r["maker"]) for r in rows), Decimal(0))
    taker = sum((_d(r["taker"]) for r in rows), Decimal(0))
    gross = maker + taker
    legacy_net = sum((_d(r["net"]) for r in rows), Decimal(0))
    legacy_stress = sum((_d(r.get("stress", r["net"])) for r in rows), Decimal(0))
    modeled_tax = gross * legacy_overhead_bps / Decimal(10_000)
    variable_net = legacy_net + modeled_tax
    variable_stress = legacy_stress + modeled_tax
    fixed_total = fixed_per_win * Decimal(n)
    all_in_net_obs = variable_net - fixed_total
    all_in_stress_obs = variable_stress - fixed_total

    def cpm(net_val, denom):
        if denom <= 0:
            return None
        return float(-net_val * Decimal(1_000_000) / denom)

    fixed_at_target_pace = float(fixed_overhead_usd_per_day * Decimal(30) / fixed_amortized_30d_millions)
    var_cpm_maker = cpm(variable_net, maker)
    return {
        "windows": n,
        "maker_turnover_quote": float(maker),
        "taker_turnover_quote": float(taker),
        "gross_turnover_quote": float(gross),
        "legacy_overhead_bps_assumed": float(legacy_overhead_bps),
        "modeled_gross_tax_stripped_usd": float(modeled_tax),
        "fixed_infra_total_usd": float(fixed_total),
        "legacy_net_usd": float(legacy_net),
        "legacy_stress_usd": float(legacy_stress),
        "variable_net_usd": float(variable_net),
        "variable_stress_usd": float(variable_stress),
        "all_in_net_observed_pace_usd": float(all_in_net_obs),
        "all_in_stress_observed_pace_usd": float(all_in_stress_obs),
        "legacy_cost_per_million_maker": cpm(legacy_net, maker),
        "variable_cost_per_million_maker": var_cpm_maker,
        "all_in_cost_per_million_maker_observed_pace": cpm(all_in_net_obs, maker),
        "all_in_cost_per_million_maker_50m_30d_pace": (
            None if var_cpm_maker is None else var_cpm_maker + fixed_at_target_pace
        ),
        "legacy_cost_per_million_gross": cpm(legacy_net, gross),
        "variable_cost_per_million_gross": cpm(variable_net, gross),
    }


def summarize_carry_cohort(rows, arm="l6_d2", split="temporal"):
    """Summarize dual-domain Core/RH carry rate screen before vs after 0.25bp/fill assumption."""
    complete = [r for r in rows if r.get("split") == split and r.get("missing") == 0]
    active = [r["arms"][arm] for r in complete if r.get("arms", {}).get(arm, {}).get("active")]
    return {
        "split": split,
        "arm": arm,
        "complete_windows": len(complete),
        "active_windows": len(active),
        "gross_sum_bps_zero_fee_postonly": float(sum((_d(x["gross_bps"]) for x in active), Decimal(0))),
        "after_1bp_legacy_025_x4_bps": float(sum((_d(x["after_1bp"]) for x in active), Decimal(0))),
        "after_5bp_stress_125_x4_bps": float(sum((_d(x["after_5bp"]) for x in active), Decimal(0))),
        "positive_windows_zero_overhead": sum(1 for x in active if _d(x["gross_bps"]) > 0),
        "positive_windows_legacy_1bp": sum(1 for x in active if _d(x["after_1bp"]) > 0),
    }


def collect_local_rescoring(base_dir: Path):
    """Build read-only decomposed rescoring report across available local Sprint 7 / Taiwan / Carry summaries."""
    out = {
        "fixed_infra_usd_per_day": float(DEFAULT_FIXED_INFRA_USD_PER_DAY),
        "fixed_infra_per_10h_window_usd": float(DEFAULT_FIXED_INFRA_USD_PER_DAY * Decimal(10) / Decimal(24)),
        "cohorts": {},
        "carry_dual_domain": {},
        "binance_2025_carry_reference": {
            "unconditional_breakeven_bps_per_fill": 0.104,
            "positive_funding_filter_breakeven_bps_per_fill": 0.134,
            "legacy_025_per_fill_net_per_window_usd": -0.00759,
            "zero_fee_postonly_light_std_breakeven_feasible_before_spread": True,
        },
    }

    s7_path = base_dir / "sprint7_cash_v1" / "scores.json"
    if s7_path.is_file():
        rows = json.loads(s7_path.read_text())["rows"]
        for arm in ("reentry_ref", "reentry_depth1", "both_ref", "both_depth1"):
            sub = [r for r in rows if r["track"] == "market_making" and r["arm"] == arm and not r.get("blockers") and r.get("net") is not None]
            if sub:
                out["cohorts"][f"s7_mm_{arm}_14w"] = rescore_windows(sub)
        for arm in ("edgeNone_queueNone", "edge0.0_queueNone", "edge0.0_queue5.0"):
            sub = [r for r in rows if r["track"] == "arb" and r["arm"] == arm and r.get("settled") and r.get("net") is not None]
            if sub:
                out["cohorts"][f"s7_arb_{arm}_settled"] = rescore_windows(sub)

    lm_path = base_dir / "sprint7_local_markout_v1" / "summary.json"
    if lm_path.is_file():
        rows = json.loads(lm_path.read_text())["rows"]
        for arm in ("control", "1s_close125"):
            sub = [r for r in rows if r["cohort"] == "monthly" and r["arm"] == arm and not r.get("blockers") and r.get("net") is not None]
            if sub:
                out["cohorts"][f"s7_lm_monthly_{arm}_14w"] = rescore_windows(sub)
            for sym in ("BTC", "ETH"):
                sub_reg = [r for r in rows if r["cohort"] == "region" and r["market"] == sym and r["arm"] == arm and not r.get("blockers") and r.get("net") is not None]
                if sub_reg:
                    out["cohorts"][f"s7_lm_region_{sym}_{arm}"] = rescore_windows(sub_reg)

    tw_path = base_dir / "taiwan_eligibility.json"
    if tw_path.is_file():
        arms = json.loads(tw_path.read_text())["arms"]
        for key in ("turnB_500_d25", "turnB_5000_d25"):
            paired = []
            for w in ("W1", "W2"):
                entry = arms.get(f"{w}/{key}")
                if entry and entry.get("modeled_net_after_overhead") is not None:
                    gross = _d(entry["core_turnover_quote"])
                    paired.append({
                        "maker": Decimal(0),
                        "taker": gross,
                        "net": _d(entry["modeled_net_after_overhead"]),
                    })
            if paired:
                res = rescore_windows(paired)
                # `taiwan_eligibility.json` stores combined Core gross turnover without maker/taker split.
                res["maker_turnover_quote"] = None
                res["taker_turnover_quote"] = None
                res["rh_eligible_turnover_quote"] = 0.0
                var_net = _d(res["variable_net_usd"])
                gross_q = _d(res["gross_turnover_quote"])
                all_in_obs = _d(res["all_in_net_observed_pace_usd"])
                var_gross_cpm = float(-var_net * Decimal(1_000_000) / gross_q) if gross_q > 0 else None
                res["all_in_cost_per_million_gross_observed_pace"] = (
                    float(-all_in_obs * Decimal(1_000_000) / gross_q) if gross_q > 0 else None
                )
                res["all_in_cost_per_million_gross_50m_30d_pace"] = (
                    None
                    if var_gross_cpm is None
                    else var_gross_cpm + float(DEFAULT_FIXED_INFRA_USD_PER_DAY * Decimal(30) / Decimal(50))
                )
                out["cohorts"][f"taiwan_core_btc_{key}_2w_core_gross_rh_zero"] = res

    carry_path = base_dir / "carry_results.json"
    if carry_path.is_file():
        c_rows = json.loads(carry_path.read_text())
        for sym in ("BTC", "ETH", "SOL", "XRP"):
            for sp in ("early", "temporal"):
                out["carry_dual_domain"][f"{sym}_{sp}"] = summarize_carry_cohort(
                    [r for r in c_rows if r.get("market") == sym],
                    split=sp,
                )

    # Codex Sprint 8 Core BTC $1,000 / 25x capital transfer validation (strictly read-only).
    cap_dir = base_dir / "sprint8_capital_transfer_v2"
    if cap_dir.is_dir():
        existing_files = sorted(cap_dir.glob("existing-*.json"))
        fresh_files = sorted(cap_dir.glob("fresh-*.json"))
        cap_rows = []
        withheld_days = []
        if existing_files:
            cap_rows.extend(json.loads(existing_files[-1].read_text()).get("rows", []))
        if fresh_files:
            cap_rows.extend(json.loads(fresh_files[-1].read_text()).get("rows", []))
        settled_by_cohort = {}
        for row in cap_rows:
            if row.get("status") != "settled" or row.get("net") is None:
                if row.get("status"):
                    withheld_days.append({"day": row.get("day"), "status": row.get("status"), "cohort": row.get("cohort")})
                continue
            settled_by_cohort.setdefault(row.get("cohort", "unknown"), []).append(row)
        cap_summary = {}
        for cohort, sub in sorted(settled_by_cohort.items()):
            stats = rescore_windows(sub)
            stats["program_score_rh"] = score_program_cost_effectiveness(
                program="rh_points",
                net_variable_pnl_quote=str(stats["variable_net_usd"]),
                fixed_infra_quote=str(stats["fixed_infra_total_usd"]),
                eligible_maker_turnover_quote="0",
            )
            cap_summary[cohort] = stats
        if cap_rows:
            all_settled = [r for r in cap_rows if r.get("status") == "settled" and r.get("net") is not None]
            out["sprint8_core_btc_1000_25x"] = {
                "cohorts": cap_summary,
                "all_settled": rescore_windows(all_settled) if all_settled else None,
                "withheld": withheld_days,
            }
    _stamp_evidence_classes(out)
    return out


# Evidence classes (Codex 2026-11-19 01:18 CST request): positive results that
# lack proven two-leg execution are valuations and must never be reported as settled replay cash (and replay cash is not live evidence).
EVIDENCE_REPLAY_SETTLED = "replay_settled_cash"
EVIDENCE_REPLAY_MODELED = "replay_modeled_overhead_net"
EVIDENCE_VALUATION = "valuation_only_unproven_two_leg_execution"
EVIDENCE_REFERENCE = "valuation_only_reference"
SETTLED_REPLAY_EVIDENCE = frozenset({EVIDENCE_REPLAY_SETTLED})


def _stamp(entry, evidence):
    if isinstance(entry, dict):
        entry["evidence_class"] = evidence
        entry["counts_as_settled_replay_cash"] = evidence in SETTLED_REPLAY_EVIDENCE


def _stamp_evidence_classes(out):
    for key, cohort in out["cohorts"].items():
        _stamp(cohort, EVIDENCE_REPLAY_MODELED if key.startswith("taiwan_") else EVIDENCE_REPLAY_SETTLED)
    for cohort in out["carry_dual_domain"].values():
        _stamp(cohort, EVIDENCE_VALUATION)
    _stamp(out["binance_2025_carry_reference"], EVIDENCE_REFERENCE)
    cap = out.get("sprint8_core_btc_1000_25x")
    if cap:
        for cohort in cap["cohorts"].values():
            _stamp(cohort, EVIDENCE_REPLAY_SETTLED)
        _stamp(cap["all_settled"], EVIDENCE_REPLAY_SETTLED)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--summary-dir",
        type=Path,
        default=Path(".agent-state/forward_ops_20261118"),
    )
    parser.add_argument("--output", type=Path, default=None)
    args = parser.parse_args()
    report = collect_local_rescoring(args.summary_dir)
    text = json.dumps(report, indent=2, sort_keys=True)
    if args.output is not None:
        args.output.write_text(text + "\n")
    else:
        print(text)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
