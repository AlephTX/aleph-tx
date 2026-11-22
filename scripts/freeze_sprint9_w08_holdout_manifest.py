#!/usr/bin/env python3
"""Pre-register Sprint 9 forward windows `W08-W16` (`S9-5`).

Strictly respects active `W07` frozen runs (`OPT-02`), enforces `frozen_at_ms < decision_start_ms`,
hashes all calibrated configurations, and blocks standalone cross-sectional directional promotion
unless retrospective `DSR >= 0.95` and `CSCV PBO <= 0.25`.

Tokyo sources are selection_dev. A pre-start freeze proves prospective timing,
not untouched source custody. The legacy filename/function are kept for callers;
new v2 manifests record that distinction. Existing frozen artifacts are not rewritten.
"""
import argparse
import hashlib
import json
import math
from pathlib import Path
import time

ELIGIBLE_HOLDOUT_WINDOWS = tuple(f"w{idx:02d}" for idx in range(8, 17))


def build_sprint9_holdout_manifest(
    *,
    window_id,
    frozen_at_ms,
    decision_start_ms,
    mm_calibrated_config,
    passive_arb_config,
    multiday_carry_config,
    quant_audit_dsr=None,
    quant_audit_pbo=None,
    admit_standalone_quant=False,
):
    """Validate a preregistered forward window, without claiming an untouched holdout."""
    if window_id not in ELIGIBLE_HOLDOUT_WINDOWS:
        raise ValueError(f"window_id {window_id!r} not in W08-W16 forward window set")
    if any(type(ms) is not int or ms <= 0 for ms in (frozen_at_ms, decision_start_ms)):
        raise ValueError("freeze and decision times must be positive integer milliseconds")
    if frozen_at_ms >= decision_start_ms:
        raise ValueError("OPT-02 prospective violation: frozen_at_ms must strictly precede decision_start_ms")
    for label, value in (("DSR", quant_audit_dsr), ("PBO", quant_audit_pbo)):
        if value is not None and (
            isinstance(value, bool) or not math.isfinite(float(value)) or not 0 <= float(value) <= 1
        ):
            raise ValueError(f"{label} must be a finite probability")
    if admit_standalone_quant:
        if quant_audit_dsr is None or quant_audit_pbo is None:
            raise ValueError("standalone quant admission requires DSR and PBO evidence")
        if float(quant_audit_dsr) < 0.95 or float(quant_audit_pbo) > 0.25:
            raise ValueError(
                f"refusing standalone quant admission: dsr={quant_audit_dsr} (<0.95) or pbo={quant_audit_pbo} (>0.25)"
            )

    def cfg_entry(name, cfg):
        canon = json.dumps(cfg, sort_keys=True, separators=(",", ":"), allow_nan=False).encode()
        return {
            "arm": name,
            "sha256": hashlib.sha256(canon).hexdigest(),
            "config": cfg,
        }

    arms = [
        cfg_entry("mm_markout_calibrated", mm_calibrated_config),
        cfg_entry("passive_arb_loop_and_entry_markout_gate", passive_arb_config),
        cfg_entry("multiday_carry_isolated", multiday_carry_config),
    ]
    payload = {
        "schema_version": "sprint9_forward_manifest.v2",
        "evidence_class": "preregistered_forward",
        "source_role": "selection_dev",
        "untouched_holdout": False,
        "window_id": window_id,
        "frozen_at_ms": frozen_at_ms,
        "decision_start_ms": decision_start_ms,
        "lead_time_ms": decision_start_ms - frozen_at_ms,
        "standalone_quant_admitted": bool(admit_standalone_quant),
        "quant_audit_gate": {
            "dsr": quant_audit_dsr,
            "pbo": quant_audit_pbo,
            "required_dsr_min": 0.95,
            "required_pbo_max": 0.25,
        },
        "arms": arms,
    }
    canon_manifest = json.dumps(payload, sort_keys=True, separators=(",", ":")).encode()
    payload["manifest_sha256"] = hashlib.sha256(canon_manifest).hexdigest()
    return payload


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--window", required=True, choices=ELIGIBLE_HOLDOUT_WINDOWS)
    parser.add_argument("--decision-start-ms", type=int, required=True)
    parser.add_argument("--mm-config", type=Path, required=True)
    parser.add_argument("--passive-arb-config", type=Path, required=True)
    parser.add_argument("--carry-config", type=Path, required=True)
    args = parser.parse_args()

    frozen_ms = int(time.time() * 1000)
    manifest = build_sprint9_holdout_manifest(
        window_id=args.window,
        frozen_at_ms=frozen_ms,
        decision_start_ms=args.decision_start_ms,
        mm_calibrated_config=json.loads(args.mm_config.read_text()),
        passive_arb_config=json.loads(args.passive_arb_config.read_text()),
        multiday_carry_config=json.loads(args.carry_config.read_text()),
    )
    print(json.dumps(manifest, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
