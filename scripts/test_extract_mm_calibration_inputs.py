import hashlib
import json
import math
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from calibrate_mm_markout_and_as import calibrate_mm_parameters, histogram_quantile
from extract_mm_calibration_inputs import (
    aggregate_calibration_reports,
    build_calibrator_payload,
    compute_leave_one_window_stability,
    main,
    strategy_overlay,
    validate_non_overlapping_windows,
)

DIST_BINS = 41
MARK_BINS = 200


def _block(exposure_by_bin, fills_by_bin, markout_bins_1s, sum_sq, secs):
    bins = []
    for i in range(DIST_BINS):
        bins.append(
            {
                "lo_bps": i * 0.5,
                "hi_bps": (i + 1) * 0.5 if i + 1 < DIST_BINS else None,
                "exposure_secs": exposure_by_bin.get(i, 0.0),
                "fill_count": fills_by_bin.get(i, 0),
                "fill_turnover": 0.0,
            }
        )
    hists = []
    for horizon in (1_000, 5_000, 30_000):
        counts = [0] * MARK_BINS
        for idx, n in markout_bins_1s.items():
            counts[idx] += n
        hists.append({"horizon_ms": horizon, "counts": counts, "underflow": 0, "overflow": 0})
    return {
        "exposure_model": "book_event_piecewise_constant",
        "distance_bin_bps": 0.5,
        "distance_bins": bins,
        "unpriced_fills": 1,
        "markout_bin_bps": 0.5,
        "markout_min_bps": -50.0,
        "markout_histograms": hists,
        "sum_sq_log_return": sum_sq,
        "sigma_observed_secs": secs,
        "sigma_samples": 10,
        "sigma_gaps": 0,
        "sigma_per_sqrt_sec": math.sqrt(sum_sq / secs),
    }


class ExtractMmCalibrationInputsTest(unittest.TestCase):
    def test_separate_quote_lifetime_and_fill_clock_models(self):
        baseline = _block({}, {}, {}, 0.0, 1.0)
        current = dict(baseline, exposure_model="resting_lifetime_fresh_book",
                       fill_distance_model="pre_execution_received_mid")
        for previous in (baseline, dict(current, fill_distance_model=None)):
            with self.assertRaisesRegex(ValueError, "incompatible resting-exposure"):
                aggregate_calibration_reports([current, previous])
        pooled = aggregate_calibration_reports([current, current])
        self.assertEqual(pooled["fill_distance_model"], "pre_execution_received_mid")
        self.assertIsNone(aggregate_calibration_reports([baseline])["fill_distance_model"])

    def test_aggregates_reports_and_recovers_kappa_sigma_and_markout_floor(self):
        # True lambda(d) = 0.5 * exp(-0.4 * d_bps) at bin centres 0.75, 1.75, 2.75 bps.
        expo = 2_000.0
        fills = {i: round(expo * 0.5 * math.exp(-0.4 * (i * 0.5 + 0.25))) for i in (1, 3, 5)}
        # Split exposure and fills evenly across two windows.
        half_fills = {i: n // 2 for i, n in fills.items()}
        rest_fills = {i: n - n // 2 for i, n in fills.items()}
        # Median 1s markout in bin 94 ([-3.0, -2.5)) -> -2.75 bps at p=0.5 interpolation.
        marks = {94: 10}
        a = _block({1: expo / 2, 3: expo / 2, 5: expo / 2, 7: 5.0}, half_fills, marks, 1e-8 * 100, 100.0)
        b = _block({1: expo / 2, 3: expo / 2, 5: expo / 2}, rest_fills, marks, 4e-8 * 300, 300.0)
        agg = aggregate_calibration_reports([a, b])
        self.assertEqual(agg["unpriced_fills"], 2)
        self.assertAlmostEqual(agg["sigma_per_sqrt_sec"], math.sqrt((1e-6 + 1.2e-5) / 400.0))
        self.assertEqual(sum(agg["markout_histograms_by_horizon"]["1s"]["counts"]), 20)

        payload, dropped = build_calibrator_payload(
            agg,
            min_bin_exposure_secs=60.0,
            time_horizon_sec=1.0,
            max_pos_for_as=1.0,
            target_risk_budget_bps_at_max_pos=6.0,
            maker_fee_bps=0.0,
            min_profit_bps=1.0,
            quantile_p=0.5,
        )
        # Bin 7 (5 s exposure) and the open overflow bin are dropped; empty bins too.
        self.assertGreaterEqual(dropped, 2)
        self.assertEqual([round(d["distance_bps"], 2) for d in payload["depth_bins"]], [0.75, 1.75, 2.75])
        out = calibrate_mm_parameters(**payload)
        self.assertAlmostEqual(out["kappa_fit"]["kappa_per_bps"], 0.4, delta=0.01)
        rec = out["recommended_config"]
        self.assertAlmostEqual(rec["markout_half_spread_floor_bps"], 2.75 + 0.5, places=6)
        overlay = strategy_overlay(out)
        self.assertEqual(overlay["markout_reserve"], {"quantile_floor_p": 0.5})
        self.assertEqual(overlay["as_risk_budget_bps_at_max_pos"], 6.0)
        self.assertAlmostEqual(overlay["as_kappa"], out["kappa_fit"]["as_kappa_fractional"])

    def test_rejects_incompatible_binning(self):
        a = _block({1: 100.0}, {1: 1}, {94: 1}, 1e-8, 1.0)
        b = _block({1: 100.0}, {1: 1}, {94: 1}, 1e-8, 1.0)
        b["markout_bin_bps"] = 1.0
        with self.assertRaises(ValueError):
            aggregate_calibration_reports([a, b])

    def test_rejects_misaligned_distance_bins_of_equal_width(self):
        a = _block({}, {}, {}, 0.0, 1.0)
        b = _block({}, {}, {}, 0.0, 1.0)
        b["distance_bins"][1:3] = reversed(b["distance_bins"][1:3])
        with self.assertRaisesRegex(ValueError, "distance bin edges"):
            aggregate_calibration_reports([a, b])

    def test_cli_counts_distinct_reports_even_with_identical_calibration(self):
        block = _block({2: 100}, {2: 12}, {90: 12}, 0.0, 1.0)
        with tempfile.TemporaryDirectory() as tmp:
            first, second, out = (Path(tmp) / n for n in ("first", "second", "out"))
            first.write_text(json.dumps({"start_ms": 0, "calibration": block}))
            second.write_bytes(first.read_bytes())
            args = ["extract", str(first), str(second), "--split-label", "diagnostic",
                    "--max-pos-for-as", "1", "--target-risk-budget-bps", "6", "--output", str(out)]
            with patch.object(sys, "argv", args):
                with self.assertRaisesRegex(SystemExit, "duplicate calibration report"):
                    main()
                self.assertFalse(out.exists())
                second.write_text(json.dumps({"start_ms": 36_000_000, "calibration": block}))
                self.assertEqual(main(), 0)
            result = json.loads(out.read_bytes())
            self.assertEqual([r["sha256"] for r in result["inputs"]],
                             [hashlib.sha256(p.read_bytes()).hexdigest() for p in (first, second)])
            self.assertEqual(result["calibrator_payload"]["depth_bins"][0]["fill_count"], 24)

    def test_rejects_overlapping_windows_across_distinct_files_and_flags_concentration(self):
        # Distinct JSON files (different SHA-256) with overlapping [start_ms, end_ms) for same market.
        block_a = _block({1: 500.0, 3: 500.0}, {1: 18, 3: 6}, {94: 24}, 1e-6, 100.0)
        block_b = _block({1: 500.0, 3: 500.0}, {1: 4, 3: 2}, {94: 6}, 1e-6, 100.0)
        with tempfile.TemporaryDirectory() as tmp:
            fa, fb, out = (Path(tmp) / n for n in ("a.json", "b.json", "out.json"))
            fa.write_text(json.dumps({"market": "BTC", "start_ms": 1_000, "end_ms": 37_000_000, "calibration": block_a}))
            fb.write_text(json.dumps({"market": "BTC", "start_ms": 18_000_000, "end_ms": 54_000_000, "calibration": block_b}))
            argv = [
                "extract", str(fa), str(fb), "--split-label", "selection_dev",
                "--max-pos-for-as", "1", "--target-risk-budget-bps", "6", "--output", str(out),
            ]
            with patch.object(sys, "argv", argv):
                with self.assertRaisesRegex(SystemExit, "overlapping calibration windows"):
                    main()
                self.assertFalse(out.exists())

            # Non-overlapping windows succeed and report single-window concentration + LOO stability.
            fb.write_text(json.dumps({"market": "BTC", "start_ms": 37_000_000, "end_ms": 73_000_000, "calibration": block_b}))
            with patch.object(sys, "argv", argv + ["--max-single-window-fill-share", "0.50"]):
                with self.assertRaisesRegex(SystemExit, "single-window fill concentration"):
                    main()
            with patch.object(sys, "argv", argv):
                self.assertEqual(main(), 0)
            rep = json.loads(out.read_text())
            stab = rep["stability"]
            self.assertEqual(stab["total_admitted_fills"], 30)
            self.assertAlmostEqual(stab["max_single_window_fill_share"], 24 / 30, places=6)
            # Dropping dominant window a leaves 6 fills (< 20 -> withheld); dropping b leaves 24 fills (fit).
            self.assertEqual(stab["leave_one_window_fit_count"], 1)
            self.assertEqual(stab["leave_one_window_withheld_count"], 1)

        # Overlapping timestamps across DIFFERENT markets remain permitted when market is tagged.
        validate_non_overlapping_windows([
            ("btc.json", "BTC", 1_000, 37_000_000),
            ("eth.json", "ETH", 1_000, 37_000_000),
        ])
        stab_direct = compute_leave_one_window_stability([("a", block_a), ("b", block_b)])
        self.assertEqual(stab_direct["windows_total"], 2)

    def test_histogram_quantile_flags_censored_tails(self):
        hist = {"bin_bps": 0.5, "min_bps": -50.0, "counts": [0] * MARK_BINS, "underflow": 6, "overflow": 0}
        hist["counts"][100] = 4
        value, censored = histogram_quantile(hist, 0.5)
        self.assertTrue(censored)
        self.assertEqual(value, -50.0)
        value, censored = histogram_quantile(hist, 0.9)
        self.assertFalse(censored)
        self.assertAlmostEqual(value, 0.0 + 0.75 * 0.5)


if __name__ == "__main__":
    unittest.main()
