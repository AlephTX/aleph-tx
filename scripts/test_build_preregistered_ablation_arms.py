import hashlib
import json
import tempfile
import unittest
from pathlib import Path

from build_preregistered_ablation_arms import (
    CRASH_PAUSE_ARMS,
    CRASH_PAUSE_ARM_B_TOXICITY,
    CRASH_PAUSE_ARM_C_POLICY_V1,
    build_arb_arms,
    build_crash_pause_arms,
    build_mm_arms,
    crash_pause_method_sha256,
    crash_pause_method_spec,
    write_arms,
)

OVERLAY = {
    "as_kappa": 4000.0,
    "as_gamma": 60000.0,
    "as_risk_budget_bps_at_max_pos": 6.0,
    "markout_reserve": {"quantile_floor_p": 0.5},
}


def _mm_base():
    return {
        "start_ms": 1,
        "core_only": True,
        "strategy": {
            "as_kappa": 2000.0,
            "as_gamma": 5000.0,
            "markout_reserve": {"max_widen_bps": 15.0, "close_cost_bps": 0.0, "max_sample_age_ms": 120000},
            "local_toxicity": {"enable_local_markout": True},
        },
    }


class BuildArmsTest(unittest.TestCase):
    def test_mm_arms_ablate_only_declared_keys(self):
        arms, ablated = build_mm_arms(_mm_base(), OVERLAY)
        self.assertEqual(set(arms), {"control", "quantile_floor", "risk_budget", "both"})
        ctrl, qf, rb, both = (arms[k]["strategy"] for k in ("control", "quantile_floor", "risk_budget", "both"))
        self.assertNotIn("quantile_floor_p", ctrl["markout_reserve"])
        self.assertNotIn("as_risk_budget_bps_at_max_pos", ctrl)
        self.assertEqual(ctrl["as_kappa"], 2000.0)
        self.assertEqual(qf["markout_reserve"]["quantile_floor_p"], 0.5)
        self.assertEqual(qf["as_kappa"], 2000.0)
        self.assertEqual(rb["as_risk_budget_bps_at_max_pos"], 6.0)
        self.assertNotIn("quantile_floor_p", rb["markout_reserve"])
        self.assertEqual((both["as_gamma"], both["markout_reserve"]["quantile_floor_p"]), (60000.0, 0.5))
        for cfg in arms.values():
            self.assertTrue(cfg["diagnose_cross_market"] and cfg["calibration_telemetry"])
            self.assertEqual(cfg["strategy"]["markout_reserve"]["max_widen_bps"], 15.0)
        self.assertIn("strategy.as_risk_budget_bps_at_max_pos", ablated)

    def test_mm_requires_frozen_markout_reserve_and_local_marks(self):
        base = _mm_base()
        del base["strategy"]["markout_reserve"]
        with self.assertRaises(ValueError):
            build_mm_arms(base, OVERLAY)
        base = _mm_base()
        base["strategy"]["local_toxicity"]["enable_local_markout"] = False
        with self.assertRaises(ValueError):
            build_mm_arms(base, OVERLAY)

    def test_arb_arms_and_manifest_hashes(self):
        base = {"passive_offset": {"max_wait_ms": 1200}, "entry_markout_gate": {"stale": True}}
        gate = {"horizon_ms": 1000, "prior_adverse_markout_bps": "2.0", "min_profit_bps": "1.0", "max_sample_age_ms": 600000}
        arms, ablated = build_arb_arms(base, gate)
        self.assertNotIn("entry_markout_gate", arms["control"])
        self.assertFalse(arms["control"]["passive_offset"]["passive_loop_only"])
        self.assertTrue(arms["passive_loop_only"]["passive_offset"]["passive_loop_only"])
        self.assertEqual(arms["entry_markout_gate"]["entry_markout_gate"], gate)
        self.assertTrue(arms["both"]["passive_offset"]["passive_loop_only"])
        with tempfile.TemporaryDirectory() as tmp:
            base_path = Path(tmp) / "base.json"
            base_path.write_text(json.dumps(base))
            manifest = write_arms(arms, ablated, Path(tmp) / "out", kind="arb", base_path=base_path, extra_inputs=[])
            self.assertEqual(len(manifest["arms"]), 4)
            self.assertEqual(manifest["schema_version"], 2)
            disk_manifest = json.loads((Path(tmp) / "out" / "manifest.json").read_bytes())
            for entry in disk_manifest["arms"]:
                payload = (Path(tmp) / "out" / entry["path"]).read_bytes()
                self.assertEqual(hashlib.sha256(payload).hexdigest(), entry["sha256"])
        with self.assertRaises(ValueError):
            build_arb_arms({"x": 1}, gate)

    def test_crash_pause_abc_arms_and_frozen_method_hash(self):
        base = _mm_base()
        base["local_reversal_filter"] = {"stale": True}
        base["strategy"]["local_toxicity"]["vol_pause_bps"] = 99.0
        arms, ablated = build_crash_pause_arms(base)
        self.assertEqual(tuple(arms.keys()), CRASH_PAUSE_ARMS)
        a = arms["arm_a_baseline"]
        b = arms["arm_b_vol_pause"]
        c = arms["arm_c_crash_pause"]
        c_shadow = arms["arm_c_shadow_control"]
        for cfg in arms.values():
            self.assertTrue(cfg["core_only"])
            self.assertTrue(cfg["diagnose_cross_market"] and cfg["calibration_telemetry"])
            self.assertNotIn("local_reversal_filter", cfg)
            self.assertTrue(cfg["strategy"]["local_toxicity"]["enable_local_markout"])
        self.assertNotIn("crash_pause_filter", a)
        self.assertNotIn("vol_pause_bps", a["strategy"]["local_toxicity"])
        self.assertNotIn("crash_pause_filter", b)
        for key, val in CRASH_PAUSE_ARM_B_TOXICITY.items():
            self.assertEqual(b["strategy"]["local_toxicity"][key], val)
        self.assertNotIn("vol_pause_bps", c["strategy"]["local_toxicity"])
        self.assertEqual(c["crash_pause_filter"], dict(CRASH_PAUSE_ARM_C_POLICY_V1) | {"enabled": True})
        self.assertEqual(c_shadow["crash_pause_filter"], dict(CRASH_PAUSE_ARM_C_POLICY_V1) | {"enabled": False})
        self.assertIn("crash_pause_filter", ablated)
        with tempfile.TemporaryDirectory() as tmp:
            base_path = Path(tmp) / "base.json"
            base_path.write_text(json.dumps(base))
            manifest = write_arms(
                arms,
                ablated,
                Path(tmp) / "cp",
                kind="crash_pause",
                base_path=base_path,
                extra_inputs=[],
                extra_manifest={
                    "method_spec": crash_pause_method_spec(),
                    "method_sha256": crash_pause_method_sha256(),
                },
            )
            self.assertEqual(manifest["method_sha256"], crash_pause_method_sha256())
            for entry in manifest["arms"]:
                payload = (Path(tmp) / "cp" / entry["path"]).read_bytes()
                self.assertEqual(hashlib.sha256(payload).hexdigest(), entry["sha256"])
        non_core = _mm_base()
        non_core["core_only"] = False
        with self.assertRaises(ValueError):
            build_crash_pause_arms(non_core)


if __name__ == "__main__":
    unittest.main()
