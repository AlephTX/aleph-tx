#!/usr/bin/env python3
"""Unit tests for `validate_wallet_toxicity.py`."""

from __future__ import annotations

import json
import math
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))

import validate_wallet_toxicity as vwt  # noqa: E402


class ValidateWalletToxicityTests(unittest.TestCase):
    def test_non_overlapping_10_day_windows_never_overlap_and_exclude_twap_sub(self) -> None:
        samples = vwt.generate_synthetic_cohort(persistent=True, num_windows=3)
        windows = vwt.build_non_overlapping_windows(samples)
        self.assertEqual(len(windows), 3)
        for idx in range(len(windows) - 1):
            self.assertLessEqual(windows[idx][1], windows[idx + 1][0])
            self.assertEqual(windows[idx][1] - windows[idx][0], vwt.DEFAULT_IS_WINDOW_MS)
        for _, _, rows in windows:
            self.assertTrue(all(not r.is_twap_sub for r in rows))
            self.assertTrue(all(r.wallet_key != "twap_sub_001" for r in rows))

    def test_persistent_cohort_passes_statistical_gate(self) -> None:
        samples = vwt.generate_synthetic_cohort(persistent=True, num_wallets=50, num_windows=3)
        report = vwt.evaluate_wallet_toxicity_persistence(samples)
        self.assertTrue(report["passed"])
        self.assertEqual(report["valid_window_pairs"], 2)
        self.assertGreaterEqual(report["mean_spearman_rho"], vwt.DEFAULT_MIN_SPEARMAN_RHO)
        self.assertGreaterEqual(
            report["mean_oos_d10_minus_d5_bps"],
            vwt.DEFAULT_MIN_OOS_D10_MINUS_D5_BPS,
        )
        json.dumps(report, allow_nan=False)

    def test_shuffled_cohort_fails_statistical_gate(self) -> None:
        samples = vwt.generate_synthetic_cohort(persistent=False, num_wallets=50, num_windows=3)
        report = vwt.evaluate_wallet_toxicity_persistence(samples)
        self.assertFalse(report["passed"])

    def test_non_adjacent_windows_are_not_paired_across_a_gap(self) -> None:
        # Two dense, internally-valid 10-day windows separated by an empty 10-day bucket
        # (bucket indices 0 and 2) must NOT be paired: pairing across the gap would measure
        # "persistence" over a discontinuity and violate the strict adjacent-split contract.
        window_ms = vwt.DEFAULT_IS_WINDOW_MS
        base_ts = 1_700_000_000_000
        num_wallets = 50
        fills_per_wallet = 4
        latents = [(i / max(1, num_wallets - 1)) * 2.4 - 0.8 for i in range(num_wallets)]
        samples: list[vwt.WalletMarkoutSample] = []
        # Bucket index 0 and bucket index 2 (index 1 left intentionally empty -> a 10-day gap).
        for bucket_idx in (0, 2):
            win_start = base_ts + bucket_idx * window_ms
            for wid in range(num_wallets):
                for k in range(fills_per_wallet):
                    ts = win_start + (wid * fills_per_wallet + k + 1) * 1_000
                    samples.append(
                        vwt.WalletMarkoutSample(
                            wallet_key=f"w_{wid:03d}",
                            ts_ms=ts,
                            adverse_selection_bps=latents[wid],
                            is_twap_sub=False,
                            notional_usd=1_000.0,
                        )
                    )

        report = vwt.evaluate_wallet_toxicity_persistence(samples)
        # Two non-empty windows exist, but they are not adjacent, so zero valid pairs and
        # the gated pair is explicitly accounted for rather than silently consumed.
        self.assertEqual(report["non_overlapping_windows"], 2)
        self.assertEqual(report["valid_window_pairs"], 0)
        self.assertEqual(report["skipped_non_adjacent_pairs"], 1)
        self.assertFalse(report["passed"])

    def test_dense_adjacent_windows_still_pair(self) -> None:
        # Sanity: the adjacency guard does not reject genuinely consecutive windows.
        samples = vwt.generate_synthetic_cohort(persistent=True, num_wallets=50, num_windows=3)
        report = vwt.evaluate_wallet_toxicity_persistence(samples)
        self.assertEqual(report["valid_window_pairs"], 2)
        self.assertEqual(report["skipped_non_adjacent_pairs"], 0)
        self.assertTrue(report["passed"])

    def test_volume_weighted_wallet_means_prioritize_notional_size(self) -> None:
        means = vwt._wallet_means(
            [
                vwt.WalletMarkoutSample("w_1", 1000, 0.0, notional_usd=100.0),
                vwt.WalletMarkoutSample("w_1", 2000, 4.0, notional_usd=900.0),
            ]
        )
        self.assertAlmostEqual(means["w_1"], 3.6, places=9)

    def test_rejects_nan_bool_directories_and_conflicting_cli_flags(self) -> None:
        with self.assertRaises(ValueError):
            vwt.WalletMarkoutSample("w_001", 1000, math.nan)
        with self.assertRaises(ValueError):
            vwt.WalletMarkoutSample("w_001", True, 1.0)  # type: ignore[arg-type]
        with self.assertRaises(ValueError):
            vwt.WalletMarkoutSample("w_001", 1000, False)  # type: ignore[arg-type]
        with self.assertRaises(ValueError):
            vwt.evaluate_wallet_toxicity_persistence([], min_overlap_wallets=5)
        with self.assertRaises(SystemExit):
            vwt.main(["--self-test", "--input", "foo.json"])
        with tempfile.TemporaryDirectory() as tmpdir:
            with self.assertRaises(ValueError):
                vwt.load_samples_from_json(Path(tmpdir))
            p = Path(tmpdir) / "samples.json"
            samples = vwt.generate_synthetic_cohort(persistent=True, num_windows=2)
            p.write_text(
                json.dumps(
                    [
                        {
                            "wallet_key": s.wallet_key,
                            "ts_ms": s.ts_ms,
                            "adverse_selection_bps": s.adverse_selection_bps,
                            "is_twap_sub": s.is_twap_sub,
                            "notional_usd": s.notional_usd,
                        }
                        for s in samples
                    ]
                ),
                encoding="utf-8",
            )
            rc = vwt.main(["--input", str(p)])
            self.assertEqual(rc, 0)


if __name__ == "__main__":
    unittest.main()
