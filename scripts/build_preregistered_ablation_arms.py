#!/usr/bin/env python3
"""Build pre-registered Sprint 9/10 ablation arms from a frozen base config (`S11-2`).

`mm`  : RecordedMakerConfig base + `extract_mm_calibration_inputs.py --calibrate`
        output -> four arms: control / quantile_floor / risk_budget / both.
`arb` : MakerFirstConfig base -> four arms: control / passive_loop_only /
        entry_markout_gate / both.

Every arm is written as canonical JSON with its SHA-256 in `manifest.json`, so
the holdout registration (owned by whoever holds the freeze-script lock) can
hash exactly what will be replayed. Arms differ from the base only in the keys
listed in `ablated_keys`; everything else is byte-for-byte inherited.
"""
import argparse
import copy
import hashlib
import json
from pathlib import Path

MM_ARMS = ("control", "quantile_floor", "risk_budget", "both")
ARB_ARMS = ("control", "passive_loop_only", "entry_markout_gate", "both")
CRASH_PAUSE_ARMS = ("arm_a_baseline", "arm_b_vol_pause", "arm_c_crash_pause", "arm_c_shadow_control")
CRASH_PAUSE_ARM_B_TOXICITY = {
    "vol_pause_bps": 15.0,
    "withdraw_cooldown_ms": 1500,
    "withdraw_clear_ratio": 0.6,
}
CRASH_PAUSE_ARM_C_POLICY_V1 = {
    "window_ms": 2000,
    "max_age_ms": 1000,
    "local_ret_shock_bps": -6.0,
    "sell_imbalance_max": -0.65,
    "min_sell_notional_usd": 15000.0,
    "ext_common_shock_bps": -4.0,
    "min_divergence_bps": -2.5,
    "resume_calm_window_ms": 2000,
    "resume_min_pause_ms": 1000,
    "resume_imbalance_abs_max": 0.35,
    "resume_l1_bid_depth_ratio_min": 0.5,
    "resume_external_reprice_min_bps": -2.5,
    "max_pause_ttl_ms": 15000,
}
CRASH_PAUSE_TTL_RULE = "handoff_to_common_info_shock_retaining_depth_flow_and_2s_external_stabilization"


def _canonical(obj):
    return json.dumps(obj, sort_keys=True, separators=(",", ":"))


def crash_pause_method_spec():
    return {
        "schema_version": 1,
        "card_id": "CORE-ETH-BUY-CRASH-PAUSE-RESUME-V1",
        "arms": list(CRASH_PAUSE_ARMS),
        "arm_b_local_toxicity": dict(CRASH_PAUSE_ARM_B_TOXICITY),
        "arm_c_crash_pause_filter": dict(CRASH_PAUSE_ARM_C_POLICY_V1),
        "ttl_expiry_rule": CRASH_PAUSE_TTL_RULE,
    }


def crash_pause_method_sha256():
    return hashlib.sha256(_canonical(crash_pause_method_spec()).encode("utf-8")).hexdigest()


def build_crash_pause_arms(base):
    if base.get("core_only") is not True:
        raise ValueError("crash_pause A/B/C requires core_only=true")
    strategy = base.get("strategy")
    if not isinstance(strategy, dict):
        raise ValueError("base config has no strategy section")
    clean = copy.deepcopy(base)
    clean["diagnose_cross_market"] = True
    clean["calibration_telemetry"] = True
    for key in (
        "local_reversal_filter",
        "binance_risk_filter",
        "hyperliquid_obi_filter",
        "local_obi_filter",
        "crash_pause_filter",
    ):
        clean.pop(key, None)
    tox_base = copy.deepcopy(clean["strategy"].get("local_toxicity") or {})
    for key in ("ofi_withdraw_threshold", "vol_pause_bps", "withdraw_cooldown_ms", "withdraw_clear_ratio"):
        tox_base.pop(key, None)
    if tox_base:
        clean["strategy"]["local_toxicity"] = tox_base
    else:
        clean["strategy"].pop("local_toxicity", None)

    arms = {}
    arms["arm_a_baseline"] = copy.deepcopy(clean)

    arm_b = copy.deepcopy(clean)
    arm_b["strategy"]["local_toxicity"] = copy.deepcopy(tox_base) | dict(CRASH_PAUSE_ARM_B_TOXICITY)
    arms["arm_b_vol_pause"] = arm_b

    arm_c = copy.deepcopy(clean)
    arm_c["crash_pause_filter"] = dict(CRASH_PAUSE_ARM_C_POLICY_V1) | {"enabled": True}
    arms["arm_c_crash_pause"] = arm_c

    arm_c_shadow = copy.deepcopy(clean)
    arm_c_shadow["crash_pause_filter"] = dict(CRASH_PAUSE_ARM_C_POLICY_V1) | {"enabled": False}
    arms["arm_c_shadow_control"] = arm_c_shadow

    ablated = [
        "strategy.local_toxicity.vol_pause_bps",
        "strategy.local_toxicity.withdraw_cooldown_ms",
        "strategy.local_toxicity.withdraw_clear_ratio",
        "crash_pause_filter",
        "diagnose_cross_market",
        "calibration_telemetry",
    ]
    return arms, ablated


def build_mm_arms(base, overlay):
    strategy = base.get("strategy")
    if not isinstance(strategy, dict):
        raise ValueError("base config has no strategy section")
    if not isinstance(strategy.get("markout_reserve"), dict):
        raise ValueError("base strategy.markout_reserve must exist; its widen/age limits stay frozen")
    if base.get("core_only") and not (strategy.get("local_toxicity") or {}).get("enable_local_markout"):
        raise ValueError("Core-only arms need local_toxicity.enable_local_markout")
    for key in ("as_kappa", "as_gamma", "as_risk_budget_bps_at_max_pos", "markout_reserve"):
        if key not in overlay:
            raise ValueError(f"overlay missing {key}")
    quantile_p = overlay["markout_reserve"]["quantile_floor_p"]
    arms = {}
    for arm in MM_ARMS:
        cfg = copy.deepcopy(base)
        cfg["diagnose_cross_market"] = True
        cfg["calibration_telemetry"] = True
        s = cfg["strategy"]
        s["markout_reserve"].pop("quantile_floor_p", None)
        s.pop("as_risk_budget_bps_at_max_pos", None)
        if arm in ("quantile_floor", "both"):
            s["markout_reserve"]["quantile_floor_p"] = quantile_p
        if arm in ("risk_budget", "both"):
            s["as_kappa"] = overlay["as_kappa"]
            s["as_gamma"] = overlay["as_gamma"]
            s["as_risk_budget_bps_at_max_pos"] = overlay["as_risk_budget_bps_at_max_pos"]
        arms[arm] = cfg
    ablated = [
        "strategy.markout_reserve.quantile_floor_p",
        "strategy.as_kappa",
        "strategy.as_gamma",
        "strategy.as_risk_budget_bps_at_max_pos",
        "diagnose_cross_market",
        "calibration_telemetry",
    ]
    return arms, ablated


def build_arb_arms(base, gate):
    passive = base.get("passive_offset")
    if not isinstance(passive, dict):
        raise ValueError("base config needs passive_offset for the passive-loop arm")
    for key in ("horizon_ms", "prior_adverse_markout_bps", "min_profit_bps"):
        if key not in gate:
            raise ValueError(f"entry markout gate missing {key}")
    arms = {}
    for arm in ARB_ARMS:
        cfg = copy.deepcopy(base)
        cfg["passive_offset"]["passive_loop_only"] = arm in ("passive_loop_only", "both")
        cfg.pop("entry_markout_gate", None)
        if arm in ("entry_markout_gate", "both"):
            cfg["entry_markout_gate"] = copy.deepcopy(gate)
        arms[arm] = cfg
    return arms, ["passive_offset.passive_loop_only", "entry_markout_gate"]


def write_arms(arms, ablated, out_dir, *, kind, base_path, extra_inputs, extra_manifest=None):
    out_dir.mkdir(parents=True, exist_ok=True)
    entries = []
    for name, cfg in arms.items():
        payload = (_canonical(cfg) + "\n").encode("utf-8")
        path = out_dir / f"{kind}-{name}.json"
        path.write_bytes(payload)
        entries.append({"arm": name, "path": path.name, "sha256": hashlib.sha256(payload).hexdigest()})
    inputs = [{"path": str(base_path), "sha256": hashlib.sha256(Path(base_path).read_bytes()).hexdigest()}]
    inputs += [{"path": str(p), "sha256": hashlib.sha256(Path(p).read_bytes()).hexdigest()} for p in extra_inputs]
    manifest = {"schema_version": 2, "kind": kind, "inputs": inputs, "ablated_keys": ablated, "arms": entries}
    if extra_manifest:
        manifest.update(extra_manifest)
    (out_dir / "manifest.json").write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
    return manifest


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="kind", required=True)
    mm = sub.add_parser("mm")
    mm.add_argument("base_config", type=Path)
    mm.add_argument("calibration", type=Path, help="extract_mm_calibration_inputs.py --calibrate output")
    mm.add_argument("out_dir", type=Path)
    arb = sub.add_parser("arb")
    arb.add_argument("base_config", type=Path)
    arb.add_argument("gate", type=Path, help="EntryMarkoutGate JSON object")
    arb.add_argument("out_dir", type=Path)
    cp = sub.add_parser("crash_pause")
    cp.add_argument("base_config", type=Path)
    cp.add_argument("out_dir", type=Path)
    args = parser.parse_args()

    base = json.loads(args.base_config.read_text())
    extra_manifest = None
    if args.kind == "mm":
        cal = json.loads(args.calibration.read_text())
        if cal.get("split_label") not in ("selection_dev", "historical_regression"):
            raise SystemExit("calibration must come from a selection or regression split")
        arms, ablated = build_mm_arms(base, cal["strategy_overlay"])
        extra = [args.calibration]
    elif args.kind == "arb":
        arms, ablated = build_arb_arms(base, json.loads(args.gate.read_text()))
        extra = [args.gate]
    else:
        arms, ablated = build_crash_pause_arms(base)
        extra = []
        extra_manifest = {
            "method_spec": crash_pause_method_spec(),
            "method_sha256": crash_pause_method_sha256(),
        }
    manifest = write_arms(
        arms,
        ablated,
        args.out_dir,
        kind=args.kind,
        base_path=args.base_config,
        extra_inputs=extra,
        extra_manifest=extra_manifest,
    )
    print(json.dumps(manifest, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
