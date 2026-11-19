import math
import unittest

from calibrate_mm_markout_and_as import (
    calibrate_mm_parameters,
    fit_exponential_kappa,
)


class CalibrateMmMarkoutAndAsTest(unittest.TestCase):
    def test_recovers_exponential_kappa_risk_gamma_and_multihorizon_floor(self):
        # True lambda(d) = 2.0 * exp(-0.4 * d_bps)
        bins = []
        for d_bps in (0.5, 1.5, 2.5, 4.0):
            exposure = 1000.0
            fills = exposure * 2.0 * math.exp(-0.4 * d_bps)
            bins.append({"distance_bps": d_bps, "fill_count": fills, "exposure_secs": exposure})

        kappa = fit_exponential_kappa(bins)
        self.assertAlmostEqual(kappa["kappa_per_bps"], 0.4, places=6)
        self.assertEqual(kappa["method"], "poisson_mle_exposure_offset")
        self.assertAlmostEqual(kappa["loglinear_positive_bins_kappa_per_bps"], 0.4, places=6)
        self.assertAlmostEqual(kappa["arrival_intensity_at_zero_per_sec"], 2.0, places=6)
        self.assertAlmostEqual(kappa["as_kappa_fractional"], 4000.0, places=3)

        # Simulated adverse markout worsens by 30s (-3.2 bps median), exceeding 1s (-1.2 bps).
        samples = {
            "1s": [-1.2, -1.0, -1.4, -1.1],
            "5s": [-2.0, -2.2, -2.5, -1.8],
            "30s": [-3.0, -3.4, -3.2, -3.2],
            "60s": [-2.8, -2.9, -3.1, -3.0],
        }
        out = calibrate_mm_parameters(
            depth_bins=bins,
            markout_samples_by_horizon=samples,
            sigma_per_sqrt_sec=1e-4,
            time_horizon_sec=1.0,
            max_pos_for_as=1.0,
            target_risk_budget_bps_at_max_pos=6.0,
            maker_fee_bps=0.2,
            min_profit_bps=1.0,
            quantile_p=0.50,
            min_markout_samples=4,
        )
        rec = out["recommended_config"]
        # gamma * (1e-4)^2 * 1 = 6 bps / 10_000 -> gamma = 60_000
        self.assertAlmostEqual(rec["as_gamma"], 60_000.0, places=2)
        # Fee(0.2) + 30s median loss(3.2) + half min_profit(0.5) = 3.9 bps
        self.assertAlmostEqual(rec["markout_half_spread_floor_bps"], 3.9, places=6)
        self.assertGreaterEqual(rec["recommended_half_spread_floor_bps"], 3.9)

    def test_rejects_non_decaying_or_degenerate_intensity(self):
        bad_bins = [
            {"distance_bps": 1.0, "fill_count": 10.0, "exposure_secs": 100.0},
            {"distance_bps": 3.0, "fill_count": 20.0, "exposure_secs": 100.0},
        ]
        with self.assertRaises(ValueError):
            fit_exponential_kappa(bad_bins)


    def test_exposed_zero_fill_bins_are_evidence_of_decay(self):
        bins = [
            {"distance_bps": 0.75, "fill_count": 100, "exposure_secs": 100.0},
            {"distance_bps": 1.75, "fill_count": 30, "exposure_secs": 100.0},
            {"distance_bps": 2.75, "fill_count": 0, "exposure_secs": 1000.0},
            {"distance_bps": 3.75, "fill_count": 0, "exposure_secs": 1000.0},
        ]
        fit = fit_exponential_kappa(bins)
        self.assertEqual(fit["zero_fill_bins"], 2)
        legacy = fit["loglinear_positive_bins_kappa_per_bps"]
        self.assertAlmostEqual(legacy, math.log(100 / 30), places=9)
        # Dropping the 2,000 exposed seconds with no fills understated the decay.
        self.assertGreater(fit["kappa_per_bps"], legacy)
        # Score equation holds at the MLE: fill-weighted mean distance == intensity-weighted mean.
        k = fit["kappa_per_bps"]
        w = [b["exposure_secs"] * math.exp(-k * b["distance_bps"]) for b in bins]
        m = sum(wi * b["distance_bps"] for wi, b in zip(w, bins)) / sum(w)
        self.assertAlmostEqual(m, (100 * 0.75 + 30 * 1.75) / 130, places=9)
        self.assertIsNotNone(fit["kappa_se_per_bps"])

    def test_unbounded_kappa_fails_closed(self):
        bins = [
            {"distance_bps": 0.5, "fill_count": 50, "exposure_secs": 100.0},
            {"distance_bps": 2.0, "fill_count": 0, "exposure_secs": 100.0},
        ]
        with self.assertRaises(ValueError):
            fit_exponential_kappa(bins)

    def test_empty_or_thin_markouts_cannot_emit_a_floor(self):
        bins = [{"distance_bps": d, "fill_count": 1000 * math.exp(-0.4 * d), "exposure_secs": 1000.0} for d in (0.5, 1.5, 2.5)]
        empty_hist = {"bin_bps": 0.5, "min_bps": -50.0, "counts": [0] * 200, "underflow": 0, "overflow": 0}
        kwargs = dict(depth_bins=bins, sigma_per_sqrt_sec=1e-4, max_pos_for_as=1.0)
        with self.assertRaises(ValueError):
            calibrate_mm_parameters(markout_histograms_by_horizon={"1s": empty_hist, "5s": empty_hist}, **kwargs)
        thin = dict(empty_hist, counts=[0] * 200)
        thin["counts"][95] = 5
        with self.assertRaises(ValueError):
            calibrate_mm_parameters(markout_histograms_by_horizon={"1s": thin}, **kwargs)
        out = calibrate_mm_parameters(markout_histograms_by_horizon={"1s": thin}, min_markout_samples=5, **kwargs)
        self.assertEqual(out["markout_curve"]["qualifying_horizons"], 1)
        with self.assertRaises(ValueError):
            calibrate_mm_parameters(markout_histograms_by_horizon={"1s": thin}, min_markout_samples=0, **kwargs)


if __name__ == "__main__":
    unittest.main()
