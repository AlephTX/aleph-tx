import json
from pathlib import Path
import random
from tempfile import TemporaryDirectory
import unittest

from audit_multiple_testing_overfitting import (
    cscv_pbo,
    collect_local_multiple_testing_audit,
    deflated_sharpe_report,
    effective_num_trials,
    expected_max_sharpe,
)


class MultipleTestingOverfittingTest(unittest.TestCase):
    def test_joint_calendar_keeps_zero_cash_and_separates_missing_markets_and_tracks(self):
        with TemporaryDirectory() as tmp:
            root = Path(tmp)
            path = root / "basis_hours_v2"
            path.mkdir()
            rows = [
                dict(start=t, sym="X", arm=arm, track=track, split="late", net025=value)
                for t in range(24) for track in ("quant", "arb")
                for arm, value in (("a", t % 3), ("b", -(t % 5)))
            ]
            del rows[3]["net025"]
            (path / "units.json").write_text(json.dumps(rows))
            cohorts = collect_local_multiple_testing_audit(root)["cohorts"]
            pair = cohorts["basis_hours_v2_temporal"]
            self.assertEqual((pair["scheduled"], pair["common"]), (24, 23))
            self.assertEqual(pair["missing_by_trial"], {"X_a": 0, "X_b": 1})
            self.assertEqual(cohorts["basis_hours_v2_temporal_quant"]["common"], 24)

    def test_ties_are_label_invariant_and_bad_alignment_is_rejected(self):
        a = [float(i % 3) for i in range(32)]
        b = [float(i % 4) for i in range(32)]
        self.assertEqual(cscv_pbo({"a": a, "b": b, "c": a}), cscv_pbo({"c": a, "a": b, "b": a}))
        self.assertEqual(expected_max_sharpe([0.1, -0.1], 1.01), 0)
        with self.assertRaises(ValueError):
            cscv_pbo({"a": a, "b": b[:-1]})
        with self.assertRaises(ValueError):
            deflated_sharpe_report({"a": a, "b": [float("nan"), *b[1:]]})

    def test_mirrored_and_correlated_trials_collapse_effective_n_and_fail_dsr_pbo(self):
        rng = random.Random(42)
        t = 160
        # 6 zero-centered symmetric pairs (+x_i, -x_i) minus 0.02 transaction cost.
        trials = {}
        for i in range(6):
            raw = [rng.gauss(0.0, 1.0) for _ in range(t)]
            mu = sum(raw) / t
            centered = [x - mu for x in raw]
            trials[f"mom_{i}"] = [x - 0.02 for x in centered]
            trials[f"rev_{i}"] = [-x - 0.02 for x in centered]

        rep = deflated_sharpe_report(trials)
        self.assertEqual(rep["num_trials_raw"], 12)
        self.assertLess(rep["effective_independent_trials"], 10.0)
        self.assertLess(rep["dsr_raw"], 0.95)
        self.assertIsNotNone(rep["sr_threshold_raw"])

        pbo = cscv_pbo(trials, num_blocks=8)
        self.assertEqual(pbo["splits"], 70)
        self.assertGreaterEqual(pbo["pbo"], 0.8)
        self.assertGreaterEqual(pbo["prob_is_winner_oos_nonpositive"], 0.8)

    def test_identical_copies_collapse_n_eff_to_one(self):
        base = [(-1.0) ** i + 0.1 * i for i in range(20)]
        n_eff = effective_num_trials([base, list(base), list(base), list(base)])
        self.assertAlmostEqual(n_eff, 1.0, places=6)

    def test_genuine_strong_alpha_passes_dsr_and_low_pbo(self):
        rng = random.Random(7)
        t = 160
        trials = {
            "true_alpha": [0.45 + rng.gauss(0.0, 0.3) for _ in range(t)],
            "noise_a": [rng.gauss(-0.02, 0.5) for _ in range(t)],
            "noise_b": [rng.gauss(-0.02, 0.5) for _ in range(t)],
            "noise_c": [rng.gauss(-0.02, 0.5) for _ in range(t)],
        }
        rep = deflated_sharpe_report(trials)
        self.assertEqual(rep["best_trial"], "true_alpha")
        self.assertGreater(rep["dsr_raw"], 0.99)
        pbo = cscv_pbo(trials, num_blocks=8)
        self.assertEqual(pbo["pbo"], 0.0)
        self.assertEqual(pbo["prob_is_winner_oos_nonpositive"], 0.0)


if __name__ == "__main__":
    unittest.main()
