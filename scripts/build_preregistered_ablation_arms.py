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


def _canonical(obj):
    return json.dumps(obj, sort_keys=True, separators=(",", ":"))


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


def write_arms(arms, ablated, out_dir, *, kind, base_path, extra_inputs):
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
    args = parser.parse_args()

    base = json.loads(args.base_config.read_text())
    if args.kind == "mm":
        cal = json.loads(args.calibration.read_text())
        if cal.get("split_label") not in ("selection_dev", "historical_regression"):
            raise SystemExit("calibration must come from a selection or regression split")
        arms, ablated = build_mm_arms(base, cal["strategy_overlay"])
        extra = [args.calibration]
    else:
        arms, ablated = build_arb_arms(base, json.loads(args.gate.read_text()))
        extra = [args.gate]
    manifest = write_arms(arms, ablated, args.out_dir, kind=args.kind, base_path=args.base_config, extra_inputs=extra)
    print(json.dumps(manifest, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
