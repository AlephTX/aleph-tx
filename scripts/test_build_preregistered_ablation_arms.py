import hashlib
import json
import tempfile
import unittest
from pathlib import Path

from build_preregistered_ablation_arms import build_arb_arms, build_mm_arms, write_arms

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


if __name__ == "__main__":
    unittest.main()
