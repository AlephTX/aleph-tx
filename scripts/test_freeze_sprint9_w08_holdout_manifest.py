import hashlib
import json
import unittest

from freeze_sprint9_w08_holdout_manifest import build_sprint9_holdout_manifest


class FreezeSprint9HoldoutManifestTest(unittest.TestCase):
    def test_forward_evidence_label_is_hash_bound_and_does_not_relabel_source(self):
        config = {"as_kappa": 2000}
        m = build_sprint9_holdout_manifest(
            window_id="w10", frozen_at_ms=10, decision_start_ms=20,
            mm_calibrated_config=config, passive_arb_config={}, multiday_carry_config={},
        )
        self.assertEqual(m["schema_version"], "sprint9_forward_manifest.v2")
        self.assertEqual(m["evidence_class"], "preregistered_forward")
        self.assertEqual(m["source_role"], "selection_dev")
        self.assertIs(m["untouched_holdout"], False)
        digest = m.pop("manifest_sha256")
        canonical = lambda obj: json.dumps(obj, sort_keys=True, separators=(",", ":")).encode()
        self.assertEqual(digest, hashlib.sha256(canonical(m)).hexdigest())
        self.assertEqual(m["arms"][0]["sha256"], hashlib.sha256(canonical(config)).hexdigest())
        m["untouched_holdout"] = True
        self.assertNotEqual(digest, hashlib.sha256(canonical(m)).hexdigest())

    def test_probability_gate_and_manifest_require_finite_evidence(self):
        args = dict(window_id="w09", frozen_at_ms=10, decision_start_ms=20,
                    mm_calibrated_config={}, passive_arb_config={}, multiday_carry_config={},
                    quant_audit_dsr=0.95, quant_audit_pbo=0.25, admit_standalone_quant=True)
        self.assertTrue(build_sprint9_holdout_manifest(**args)["standalone_quant_admitted"])
        for key, invalid in [
            ("quant_audit_dsr", float("nan")),
            ("quant_audit_dsr", 1.1),
            ("quant_audit_pbo", float("-inf")),
            ("quant_audit_pbo", -0.1),
            ("quant_audit_pbo", False),
            ("frozen_at_ms", float("nan")),
            ("decision_start_ms", 0),
            ("mm_calibrated_config", {"floor": float("nan")}),
        ]:
            with self.subTest(key=key, invalid=invalid), self.assertRaises(ValueError):
                build_sprint9_holdout_manifest(**(args | {key: invalid}))

    def test_enforces_w08_w16_opt02_pre_freeze_and_blocks_overfit_quant(self):
        m = build_sprint9_holdout_manifest(
            window_id="w08",
            frozen_at_ms=1_800_000_000_000,
            decision_start_ms=1_800_003_600_000,
            mm_calibrated_config={"quantile_floor_p": 0.5, "as_risk_budget_bps_at_max_pos": 6.0},
            passive_arb_config={"passive_loop_only": True},
            multiday_carry_config={"market": "SOL"},
            quant_audit_dsr=0.41,
            quant_audit_pbo=0.63,
            admit_standalone_quant=False,
        )
        self.assertEqual(m["window_id"], "w08")
        self.assertFalse(m["standalone_quant_admitted"])
        self.assertEqual(len(m["arms"]), 3)
        self.assertEqual(len(m["manifest_sha256"]), 64)

        # Cannot touch active W07 or frozen historical W01..W06.
        with self.assertRaises(ValueError):
            build_sprint9_holdout_manifest(
                window_id="w07",
                frozen_at_ms=1_800_000_000_000,
                decision_start_ms=1_800_003_600_000,
                mm_calibrated_config={},
                passive_arb_config={},
                multiday_carry_config={},
            )

        # Must freeze strictly before decision_start_ms.
        with self.assertRaises(ValueError):
            build_sprint9_holdout_manifest(
                window_id="w09",
                frozen_at_ms=1_800_003_600_000,
                decision_start_ms=1_800_003_600_000,
                mm_calibrated_config={},
                passive_arb_config={},
                multiday_carry_config={},
            )

        # Rejects admitting standalone quant with DSR < 0.95 or PBO > 0.25.
        with self.assertRaises(ValueError):
            build_sprint9_holdout_manifest(
                window_id="w09",
                frozen_at_ms=1_800_000_000_000,
                decision_start_ms=1_800_003_600_000,
                mm_calibrated_config={},
                passive_arb_config={},
                multiday_carry_config={},
                quant_audit_dsr=0.41,
                quant_audit_pbo=0.63,
                admit_standalone_quant=True,
            )


if __name__ == "__main__":
    unittest.main()
