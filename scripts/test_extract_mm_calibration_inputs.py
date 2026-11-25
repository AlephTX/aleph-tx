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

    def test_binds_bounds_to_selected_calibration_chain_and_enforces_positive_intervals(self):
        block = _block({1: 500.0, 3: 500.0}, {1: 18, 3: 6}, {94: 24}, 1e-6, 100.0)
        selected = {"market": "BTC", "start_ms": 36_000_000, "end_ms": 72_000_000, "calibration": block}
        same_window = dict(selected, label="distinct-byte-duplicate-window")

        with tempfile.TemporaryDirectory() as tmp:
            fa, fb, out = (Path(tmp) / n for n in ("a.json", "b.json", "out.json"))
            base_argv = [
                "extract", str(fa), str(fb), "--split-label", "diagnostic",
                "--max-pos-for-as", "1", "--target-risk-budget-bps", "6", "--output", str(out),
            ]
            # Unrelated sibling clocks or market must not mask overlapping windows on the selected chain.
            for sibling in ({"market": "BTC", "start_ms": 0, "end_ms": 36_000_000}, {"market": "ETH"}):
                fa.write_text(json.dumps({"report": sibling, "result": selected}))
                fb.write_text(json.dumps(same_window))
                out.unlink(missing_ok=True)
                with patch.object(sys, "argv", base_argv):
                    with self.assertRaisesRegex(SystemExit, "overlapping calibration windows for market BTC"):
                        main()
                    self.assertFalse(out.exists())

            # Explicit overlap opt-out still admits valid distinct-byte overlaps.
            with patch.object(sys, "argv", base_argv + ["--allow-overlapping-windows"]):
                self.assertEqual(main(), 0)

            # Root precedence + trade_from_ms over same-level start_ms + descendant fill along selected chain.
            fa.write_text(json.dumps({
                "symbol": "BTC",
                "trade_from_ms": 36_000_000,
                "start_ms": 35_940_000,
                "report": {"result": {"start_ms": 35_940_000, "end_ms": 72_000_000, "calibration": block}},
            }))
            fb.write_text(json.dumps({"recorded": {"market": "BTC", "start_ms": 72_000_000, "end_ms": 108_000_000, "calibration": block}}))
            with patch.object(sys, "argv", base_argv):
                self.assertEqual(main(), 0)
            res = json.loads(out.read_text())
            self.assertEqual(
                [(r["market"], r["start_ms"], r["end_ms"]) for r in res["inputs"]],
                [("BTC", 36_000_000, 72_000_000), ("BTC", 72_000_000, 108_000_000)],
            )

            # Top-level calibration owner ignores descendant report metadata.
            fa.write_text(json.dumps({"calibration": block, "report": selected}))
            single_argv = [
                "extract", str(fa), "--split-label", "diagnostic",
                "--max-pos-for-as", "1", "--target-risk-budget-bps", "6", "--output", str(out),
            ]
            with patch.object(sys, "argv", single_argv):
                self.assertEqual(main(), 0)
            top_prov = json.loads(out.read_text())["inputs"][0]
            self.assertFalse(any(k in top_prov for k in ("market", "start_ms", "end_ms")))

            # Empty (start == end) and inverted (end < start) windows fail closed even with overlap opt-out.
            for end_ms in (36_000_000, 35_999_999):
                fa.write_text(json.dumps({"market": "BTC", "start_ms": 36_000_000, "end_ms": end_ms, "calibration": block}))
                for extra in ([], ["--allow-overlapping-windows"]):
                    out.unlink(missing_ok=True)
                    with patch.object(sys, "argv", single_argv + extra):
                        with self.assertRaisesRegex(SystemExit, "invalid window bounds"):
                            main()
                        self.assertFalse(out.exists())

    def test_ancestor_window_bounds_conflict_and_inheritance(self):
        from extract_mm_calibration_inputs import _extract_window_bounds

        # Conflicting bounds across wrapper/owner chain (including market vs symbol).
        for ancestors, expected in (
            ([{"market": "BTC"}, {"market": "ETH"}], "market"),
            ([{"market": "BTC"}, {"symbol": "ETH"}], "market"),
            ([{"market": "BTC", "symbol": "ETH"}], "market"),
            ([{"trade_from_ms": 36_000_000}, {"trade_from_ms": 39_600_000}], "trade_from_ms"),
            ([{"start_ms": 35_940_000}, {"start_ms": 36_000_000}], "start_ms"),
            ([{"end_ms": 39_600_000}, {"end_ms": 43_200_000}], "end_ms"),
            ([{"trade_from_ms": 35_900_000}, {"start_ms": 36_000_000}], "start bounds"),
            ([{"start_ms": 36_000_000}, {"trade_from_ms": 35_900_000}], "start bounds"),
            ([{"start_ms": True}], "invalid calibration start_ms"),
        ):
            with self.assertRaisesRegex(ValueError, expected):
                _extract_window_bounds(ancestors)

        # Valid inheritance & warmup precedence regardless of wrapper/owner order.
        self.assertEqual(
            _extract_window_bounds([{"market": "BTC", "trade_from_ms": 36_000_000}, {"market": "BTC", "start_ms": 35_940_000, "end_ms": 39_600_000}]),
            ("BTC", 36_000_000, 39_600_000),
        )
        self.assertEqual(
            _extract_window_bounds([{"symbol": "BTC", "start_ms": 35_940_000}, {"market": "BTC", "trade_from_ms": 36_000_000, "end_ms": 39_600_000}]),
            ("BTC", 36_000_000, 39_600_000),
        )
        self.assertEqual(
            _extract_window_bounds([{}, {"market": "ETH", "start_ms": 1_000, "end_ms": 2_000}]),
            ("ETH", 1_000, 2_000),
        )
        self.assertEqual(
            _extract_window_bounds([{"market": "ETH", "trade_from_ms": 1_200, "end_ms": 2_000}, {}]),
            ("ETH", 1_200, 2_000),
        )

        # End-to-end main() rejects #609 wrapper-vs-owner conflicts and admits consistent wrappers.
        block = _block({1: 500.0, 3: 500.0}, {1: 18, 3: 6}, {94: 24}, 1e-6, 100.0)
        with tempfile.TemporaryDirectory() as td:
            root = Path(td)
            inp, out = root / "report.json", root / "out.json"
            cli_argv = [
                "extract",
                str(inp),
                "--split-label",
                "diagnostic",
                "--max-pos-for-as",
                "1",
                "--target-risk-budget-bps",
                "6",
                "--output",
                str(out),
            ]
            for doc in (
                {
                    "market": "BTC",
                    "trade_from_ms": 36_000_000,
                    "end_ms": 72_000_000,
                    "result": {"market": "BTC", "trade_from_ms": 72_000_000, "end_ms": 108_000_000, "calibration": block},
                },
                {
                    "market": "BTC",
                    "trade_from_ms": 36_000_000,
                    "end_ms": 72_000_000,
                    "result": {"symbol": "ETH", "trade_from_ms": 36_000_000, "end_ms": 72_000_000, "calibration": block},
                },
                {
                    "market": "BTC",
                    "trade_from_ms": 35_900_000,
                    "end_ms": 72_000_000,
                    "result": {"market": "BTC", "start_ms": 36_000_000, "calibration": block},
                },
            ):
                inp.write_text(json.dumps(doc))
                out.unlink(missing_ok=True)
                with patch.object(sys, "argv", cli_argv):
                    with self.assertRaisesRegex(SystemExit, "conflicting calibration"):
                        main()
                    self.assertFalse(out.exists())

            inp.write_text(json.dumps({
                "symbol": "BTC",
                "start_ms": 35_940_000,
                "result": {"market": "BTC", "trade_from_ms": 36_000_000, "end_ms": 72_000_000, "calibration": block},
            }))
            with patch.object(sys, "argv", cli_argv):
                self.assertEqual(main(), 0)
            prov = json.loads(out.read_text())["inputs"][0]
            self.assertEqual((prov["market"], prov["start_ms"], prov["end_ms"]), ("BTC", 36_000_000, 72_000_000))

            # #609 two-file --max-single-window-fill-share 0.60 regression & disjoint control.
            fa, fb = root / "a.json", root / "b.json"
            owner = {"market": "BTC", "trade_from_ms": 36_000_000, "end_ms": 72_000_000, "calibration": block}
            fa.write_text(json.dumps({"report": owner}))
            pair_argv = [
                "extract",
                str(fa),
                str(fb),
                "--split-label",
                "selection_dev",
                "--max-pos-for-as",
                "1",
                "--target-risk-budget-bps",
                "6",
                "--max-single-window-fill-share",
                "0.60",
                "--output",
                str(out),
            ]
            for second in (
                {"market": "BTC", "trade_from_ms": 72_000_000, "end_ms": 108_000_000, "result": owner},
                {"market": "ETH", "result": owner},
            ):
                fb.write_text(json.dumps(second))
                for extra in ([], ["--allow-overlapping-windows"]):
                    out.unlink(missing_ok=True)
                    with patch.object(sys, "argv", pair_argv + extra):
                        with self.assertRaisesRegex(SystemExit, "conflicting calibration"):
                            main()
                        self.assertFalse(out.exists())

            fb.write_text(json.dumps({"result": {**owner, "trade_from_ms": 72_000_000, "end_ms": 108_000_000}}))
            out.unlink(missing_ok=True)
            with patch.object(sys, "argv", pair_argv):
                self.assertEqual(main(), 0)
            stability = json.loads(out.read_text())["stability"]
            self.assertEqual(stability["windows_total"], 2)
            self.assertEqual(stability["total_admitted_fills"], 48)
            self.assertEqual(stability["max_single_window_fill_share"], 0.5)

    def test_rejects_fractional_and_non_finite_timestamps_while_allowing_integer_strings(self):
        from extract_mm_calibration_inputs import _extract_window_bounds

        bad_values = (
            72_000_000.9,
            72_000_000.2,
            0.5,
            float("nan"),
            float("inf"),
            float("-inf"),
            1e20,
            "72000000.9",
            "72000000.0",
            "1e7",
            "nan",
            "inf",
            "36_000_000",
            "",
            [],
            True,
        )
        for bad in bad_values:
            for key in ("trade_from_ms", "start_ms", "end_ms"):
                with self.assertRaisesRegex(ValueError, "invalid .*calibration"):
                    _extract_window_bounds([{"market": "BTC", key: bad}])
            with self.assertRaisesRegex(ValueError, "invalid .*calibration"):
                validate_non_overlapping_windows([("bad.json", "BTC", bad, None)])
            with self.assertRaisesRegex(ValueError, "invalid .*calibration"):
                validate_non_overlapping_windows([("bad.json", "BTC", None, bad)])

        # #612 CLI regression: fractional overlaps must fail closed without truncating to integers.
        block = _block({1: 500.0, 3: 500.0}, {1: 18, 3: 6}, {94: 24}, 1e-6, 100.0)
        owner = {"market": "BTC", "trade_from_ms": 36_000_000, "end_ms": 72_000_000, "calibration": block}
        with tempfile.TemporaryDirectory() as td:
            root = Path(td)
            fa, fb, out = (root / n for n in ("a.json", "b.json", "out.json"))
            argv = [
                "extract",
                str(fa),
                str(fb),
                "--split-label",
                "selection_dev",
                "--max-pos-for-as",
                "1",
                "--target-risk-budget-bps",
                "6",
                "--max-single-window-fill-share",
                "0.60",
                "--output",
                str(out),
            ]
            fa.write_text(json.dumps({"result": {**owner, "end_ms": 72_000_000.9}}))
            fb.write_text(json.dumps({"result": {**owner, "trade_from_ms": 72_000_000.2, "end_ms": 108_000_000}}))
            for extra in ([], ["--allow-overlapping-windows"]):
                out.unlink(missing_ok=True)
                with patch.object(sys, "argv", argv + extra):
                    with self.assertRaisesRegex(SystemExit, "invalid non-integer or non-finite calibration"):
                        main()
                    self.assertFalse(out.exists())

            # Historical integer strings and exact integer floats remain accepted as exact integers.
            fa.write_text(json.dumps({"result": {**owner, "trade_from_ms": "36000000", "end_ms": " 72000000 "}}))
            fb.write_text(json.dumps({"result": {**owner, "trade_from_ms": 72_000_000.0, "end_ms": 108_000_000.0}}))
            out.unlink(missing_ok=True)
            with patch.object(sys, "argv", argv):
                self.assertEqual(main(), 0)
            inputs = json.loads(out.read_text())["inputs"]
            self.assertEqual(
                [(r["market"], r["start_ms"], r["end_ms"]) for r in inputs],
                [("BTC", 36_000_000, 72_000_000), ("BTC", 72_000_000, 108_000_000)],
            )

    def test_rejects_binary64_rounded_json_literals_and_normalizes_decimal_bin_edges(self):
        from decimal import Decimal
        from extract_mm_calibration_inputs import _extract_window_bounds

        for bad in (
            Decimal("1e9999999"),
            Decimal("NaN"),
            Decimal("Infinity"),
            Decimal("-Infinity"),
            Decimal("72000000.000000003"),
            Decimal("72000000.00000000000000000000000000001"),
            Decimal("9007199254740993.0"),
        ):
            with self.assertRaisesRegex(ValueError, "invalid non-integer or non-finite calibration"):
                _extract_window_bounds([{"market": "BTC", "end_ms": bad}])

        # Decimal bin edges (e.g. from parse_float=Decimal on 0.1) normalize to float cleanly.
        dec_block = _block({1: 500.0}, {1: 10}, {94: 10}, 1e-6, 100.0)
        dec_block["distance_bins"][0]["hi_bps"] = Decimal("0.1")
        flt_block = _block({1: 500.0}, {1: 10}, {94: 10}, 1e-6, 100.0)
        flt_block["distance_bins"][0]["hi_bps"] = 0.1
        agg = aggregate_calibration_reports([dec_block, flt_block])
        self.assertIs(type(agg["distance_bins"][0]["hi_bps"]), float)
        self.assertEqual(agg["distance_bins"][0]["hi_bps"], 0.1)
        json.dumps(agg)

        # #615 CLI regressions: sub-ULP fractional overlap, >28-digit fractions, and >2^53 binary64 rounding fail closed.
        block = _block({1: 500.0, 3: 500.0}, {1: 18, 3: 6}, {94: 24}, 1e-6, 100.0)
        owner = {"market": "BTC", "trade_from_ms": 36_000_000, "end_ms": 72_000_000, "calibration": block}
        with tempfile.TemporaryDirectory() as td:
            root = Path(td)
            fa, fb, out = (root / n for n in ("a.json", "b.json", "out.json"))
            argv_pair = [
                "extract", str(fa), str(fb), "--split-label", "selection_dev",
                "--max-pos-for-as", "1", "--target-risk-budget-bps", "6",
                "--max-single-window-fill-share", "0.60", "--output", str(out),
            ]
            fa.write_text(json.dumps({"report": {**owner, "end_ms": "RAW_END"}}).replace('"RAW_END"', "72000000.000000003"))
            fb.write_text(json.dumps({"result": {**owner, "trade_from_ms": "RAW_START", "end_ms": 108_000_000}}).replace('"RAW_START"', "72000000.000000001"))
            for extra in ([], ["--allow-overlapping-windows"]):
                out.unlink(missing_ok=True)
                with patch.object(sys, "argv", argv_pair + extra):
                    with self.assertRaisesRegex(SystemExit, "invalid non-integer or non-finite calibration"):
                        main()
                    self.assertFalse(out.exists())

            argv_single = [
                "extract", str(fa), "--split-label", "selection_dev",
                "--max-pos-for-as", "1", "--target-risk-budget-bps", "6", "--output", str(out),
            ]
            high_owner = {**owner, "trade_from_ms": 1 << 53, "end_ms": (1 << 53) + 1000}
            for raw_doc in (
                json.dumps({"report": {**owner, "end_ms": "RAW"}}).replace('"RAW"', "72000000.00000000000000000000000000001"),
                json.dumps({"trade_from_ms": "RAW", "report": high_owner}).replace('"RAW"', "9007199254740993.0"),
            ):
                fa.write_text(raw_doc)
                out.unlink(missing_ok=True)
                with patch.object(sys, "argv", argv_single):
                    with self.assertRaisesRegex(SystemExit, "invalid non-integer or non-finite calibration"):
                        main()
                    self.assertFalse(out.exists())

            # Exact 2^53 integral float and legacy integer-string/integral-float warmup controls pass cleanly.
            fa.write_text(json.dumps({"trade_from_ms": "RAW", "report": high_owner}).replace('"RAW"', "9007199254740992.0"))
            out.unlink(missing_ok=True)
            with patch.object(sys, "argv", argv_single):
                self.assertEqual(main(), 0)
            self.assertEqual(json.loads(out.read_text())["inputs"][0]["start_ms"], 1 << 53)

            fa.write_text(
                json.dumps({"symbol": "BTC", "start_ms": " 35940000 ", "report": {**owner, "trade_from_ms": "+36000000", "end_ms": "RAW"}})
                .replace('"RAW"', "72000000.0")
            )
            out.unlink(missing_ok=True)
            with patch.object(sys, "argv", argv_single):
                self.assertEqual(main(), 0)
            prov = json.loads(out.read_text())["inputs"][0]
            self.assertEqual((prov["market"], prov["start_ms"], prov["end_ms"]), ("BTC", 36_000_000, 72_000_000))

    def test_decision_phase_sigma_scope_binds_bounds_and_rejects_mismatches(self):
        legacy_kw = dict(exposure_by_bin={1: 500.0, 3: 500.0}, fills_by_bin={1: 18, 3: 6}, markout_bins_1s={94: 24})
        block_a = _block(**legacy_kw, sum_sq=9.0e-4, secs=100.0)
        block_a["decision_phase_sigma"] = {
            "scope": "receipt_half_open",
            "trade_from_ms": 36_000_000,
            "end_ms": 72_000_000,
            "sum_sq_log_return": 1.0e-6,
            "sigma_observed_secs": 50.0,
            "sigma_samples": 100,
            "sigma_gaps": 0,
            "sigma_per_sqrt_sec": math.sqrt(1.0e-6 / 50.0),
        }
        block_b = _block(**legacy_kw, sum_sq=1.6e-3, secs=100.0)
        block_b["decision_phase_sigma"] = {
            "scope": "receipt_half_open",
            "trade_from_ms": 72_000_000,
            "end_ms": 108_000_000,
            "sum_sq_log_return": 3.0e-6,
            "sigma_observed_secs": 150.0,
            "sigma_samples": 300,
            "sigma_gaps": 1,
            "sigma_per_sqrt_sec": math.sqrt(3.0e-6 / 150.0),
        }

        with tempfile.TemporaryDirectory() as td:
            root = Path(td)
            fa, fb, out = (root / n for n in ("a.json", "b.json", "out.json"))
            rep_a = {"market": "BTC", "start_ms": 35_940_000, "trade_from_ms": 36_000_000, "end_ms": 72_000_000, "calibration": block_a}
            rep_b = {"market": "BTC", "start_ms": 71_940_000, "trade_from_ms": 72_000_000, "end_ms": 108_000_000, "calibration": block_b}
            fa.write_text(json.dumps(rep_a))
            fb.write_text(json.dumps(rep_b))
            base = [
                "extract", str(fa), str(fb), "--split-label", "selection_dev",
                "--max-pos-for-as", "1", "--target-risk-budget-bps", "6", "--output", str(out),
            ]

            with patch.object(sys, "argv", base):
                self.assertEqual(main(), 0)
            legacy_out = json.loads(out.read_text())
            self.assertEqual(legacy_out["aggregate"]["sigma_scope"], "legacy")
            self.assertEqual(legacy_out["aggregate"]["sigma_observed_secs"], 200.0)
            self.assertAlmostEqual(
                legacy_out["calibrator_payload"]["sigma_per_sqrt_sec"],
                math.sqrt(2.5e-3 / 200.0),
            )

            out.unlink()
            with patch.object(sys, "argv", base + ["--sigma-scope", "decision_phase"]):
                self.assertEqual(main(), 0)
            phase_out = json.loads(out.read_text())
            self.assertEqual(phase_out["aggregate"]["sigma_scope"], "decision_phase")
            self.assertEqual(phase_out["aggregate"]["sigma_observed_secs"], 200.0)
            self.assertEqual(phase_out["aggregate"]["sigma_samples"], 400)
            self.assertAlmostEqual(
                phase_out["calibrator_payload"]["sigma_per_sqrt_sec"],
                math.sqrt(4.0e-6 / 200.0),
            )

            # Coherent empty block (0 variance, 0s, 0 samples, None sigma) pools cleanly alongside a populated block.
            block_empty = {
                **block_b,
                "decision_phase_sigma": {
                    "scope": "receipt_half_open",
                    "trade_from_ms": 72_000_000,
                    "end_ms": 108_000_000,
                    "sum_sq_log_return": 0.0,
                    "sigma_observed_secs": 0.0,
                    "sigma_samples": 0,
                    "sigma_gaps": 0,
                    "sigma_per_sqrt_sec": None,
                },
            }
            fb.write_text(json.dumps({**rep_b, "calibration": block_empty}))
            out.unlink()
            with patch.object(sys, "argv", base + ["--sigma-scope", "decision_phase"]):
                self.assertEqual(main(), 0)
            empty_pooled = json.loads(out.read_text())
            self.assertEqual(empty_pooled["aggregate"]["sigma_observed_secs"], 50.0)
            self.assertEqual(empty_pooled["aggregate"]["sigma_samples"], 100)
            self.assertAlmostEqual(
                empty_pooled["calibrator_payload"]["sigma_per_sqrt_sec"],
                math.sqrt(1.0e-6 / 50.0),
            )

            # Fail closed on missing block, invalid scope, non-integral bounds, owner mismatch (warm-up start_ms only), missing owner bounds, incoherent duration/samples/variance/sigma (#648), and all-empty zero variance.
            bad_cases = [
                ({"market": "BTC", "start_ms": 35_940_000, "trade_from_ms": 36_000_000, "end_ms": 72_000_000, "calibration": _block(**legacy_kw, sum_sq=1e-6, secs=100.0)}, "lacks valid decision_phase_sigma"),
                ({"market": "BTC", "trade_from_ms": 36_000_000, "end_ms": 72_000_000, "calibration": {**block_a, "decision_phase_sigma": {**block_a["decision_phase_sigma"], "scope": "all_events"}}}, "lacks valid decision_phase_sigma"),
                ({"market": "BTC", "trade_from_ms": 36_000_000, "end_ms": 72_000_000, "calibration": {**block_a, "decision_phase_sigma": {**block_a["decision_phase_sigma"], "trade_from_ms": 36_000_000.5}}}, "invalid non-integer or non-finite calibration"),
                ({"market": "BTC", "start_ms": 35_940_000, "end_ms": 72_000_000, "calibration": block_a}, "owner decision bounds"),
                ({"market": "BTC", "calibration": block_a}, "requires bound owner/wrapper"),
                ({"market": "BTC", "trade_from_ms": 36_000_000, "end_ms": 72_000_000, "calibration": {**block_a, "decision_phase_sigma": {**block_a["decision_phase_sigma"], "sum_sq_log_return": 0.03, "sigma_observed_secs": 0.0, "sigma_samples": 0, "sigma_per_sqrt_sec": None}}}, "incoherent decision_phase_sigma zero duration"),
                ({"market": "BTC", "trade_from_ms": 36_000_000, "end_ms": 72_000_000, "calibration": {**block_a, "decision_phase_sigma": {**block_a["decision_phase_sigma"], "sigma_observed_secs": 36_001.0, "sigma_per_sqrt_sec": math.sqrt(1.0e-6 / 36_001.0)}}}, "exceeds scope span"),
                ({"market": "BTC", "trade_from_ms": 36_000_000, "end_ms": 72_000_000, "calibration": {**block_a, "decision_phase_sigma": {**block_a["decision_phase_sigma"], "sigma_samples": 0}}}, "incoherent decision_phase_sigma duration/sample"),
                ({"market": "BTC", "trade_from_ms": 36_000_000, "end_ms": 72_000_000, "calibration": {**block_a, "decision_phase_sigma": {**block_a["decision_phase_sigma"], "sigma_observed_secs": 0.0, "sigma_samples": 100, "sigma_per_sqrt_sec": None}}}, "incoherent decision_phase_sigma duration/sample"),
                ({"market": "BTC", "trade_from_ms": 36_000_000, "end_ms": 72_000_000, "calibration": {**block_a, "decision_phase_sigma": {**block_a["decision_phase_sigma"], "sigma_observed_secs": 0.05, "sigma_samples": 100, "sigma_per_sqrt_sec": math.sqrt(1.0e-6 / 0.05)}}}, "exceeds 1kHz"),
                ({"market": "BTC", "trade_from_ms": 36_000_000, "end_ms": 72_000_000, "calibration": {**block_a, "decision_phase_sigma": {**block_a["decision_phase_sigma"], "sigma_per_sqrt_sec": 0.5}}}, "incoherent decision_phase_sigma sigma_per_sqrt_sec"),
                ({"market": "BTC", "trade_from_ms": 36_000_000, "end_ms": 72_000_000, "calibration": {**block_a, "decision_phase_sigma": {**block_a["decision_phase_sigma"], "sum_sq_log_return": 0.0, "sigma_observed_secs": 0.0, "sigma_samples": 0, "sigma_per_sqrt_sec": None}}}, "no realized variance observed"),
            ]
            single_argv = [
                "extract", str(fa), "--split-label", "selection_dev",
                "--sigma-scope", "decision_phase",
                "--max-pos-for-as", "1", "--target-risk-budget-bps", "6", "--output", str(out),
            ]
            for bad_doc, pattern in bad_cases:
                fa.write_text(json.dumps(bad_doc))
                out.unlink(missing_ok=True)
                with patch.object(sys, "argv", single_argv):
                    with self.assertRaisesRegex(SystemExit, pattern):
                        main()
                    self.assertFalse(out.exists())


if __name__ == "__main__":
    unittest.main()
