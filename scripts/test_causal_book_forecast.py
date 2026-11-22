import json
from pathlib import Path
import tempfile
import unittest

from causal_book_forecast import Sample, combined, extract, fit, score, touch, write_once


def book(at, mid=100.0, *, source=None, bq=3.0, aq=1.0):
    return {"kind": "book", "at_ms": at, "lighter": {
        "timestamp_ms": at if source is None else source,
        "bids": [[mid-.01, bq]], "asks": [[mid+.01, aq]]}}


def missing(at):
    return {"kind": "book", "at_ms": at,
            "lighter": {"timestamp_ms": 0, "bids": [], "asks": []}}


class CausalBookForecastTests(unittest.TestCase):
    def test_opposite_size_microprice_and_invalid_book(self):
        _, mid, signal = touch(book(1000)["lighter"], 1000)
        self.assertEqual(mid, 100)
        self.assertAlmostEqual(signal, .5)
        self.assertAlmostEqual(touch(book(1000, bq=1, aq=3)["lighter"], 1000)[2], -.5)
        self.assertIsNone(touch(missing(1000)["lighter"], 1000))
        for bad in [book(1000, source=1001), book(1000, bq=float("nan")),
                    book(1000, aq=0), book(1000, source=0)]:
            with self.assertRaises(ValueError):
                touch(bad["lighter"], 1000)

    def test_asof_features_and_first_eligible_label(self):
        events = [book(t, 100+t/100000) for t in range(1000, 8001, 250)]
        rows, coverage = extract(events, 2000, 7000)
        r = rows[0]
        self.assertEqual((r.at_ms, r.label_at_ms), (2000, 3000))
        self.assertAlmostEqual(r.features[1], (100.02/100.01-1)*10000)
        self.assertAlmostEqual(r.target_bps, (100.03/100.02-1)*10000)
        self.assertEqual(coverage[0]["grid_points"], 5)
        self.assertEqual(len(rows), 4)

    def test_future_tail_cannot_change_past_features(self):
        events = [book(t, 100+t/100000) for t in range(1000, 8001, 250)]
        changed = [e if e["at_ms"] < 4000 else book(e["at_ms"], 200) for e in events]
        a, _ = extract(events, 2000, 7000)
        b, _ = extract(changed, 2000, 7000)
        self.assertEqual(a[0], b[0])
        self.assertEqual(a[1].features, b[1].features)
        self.assertNotEqual(a[1].target_bps, b[1].target_bps)

    def test_market_delay_and_source_time_label_guard(self):
        events = [book(t, 100+t/100000) for t in range(500, 9001, 250)]
        rows, _ = extract(events, 2000, 7000, delay_ms=250)
        r = rows[0]
        self.assertEqual(r.label_at_ms, 3250)
        self.assertAlmostEqual(r.features[1], (100.0175/100.0075-1)*10000)
        self.assertAlmostEqual(r.target_bps, (100.03/100.0175-1)*10000)

    def test_subgrid_gap_withholds_cross_gap_labels_and_momentum(self):
        events = [book(t) for t in range(1000, 7001, 250)]
        events.insert(5, missing(2050))
        events.insert(6, book(2100))
        rows, coverage = extract(events, 2000, 7000)
        self.assertNotIn(2000, [r.at_ms for r in rows])
        self.assertNotIn(3000, [r.at_ms for r in rows])
        self.assertEqual(coverage[0]["label_gap"], 1)
        self.assertEqual(coverage[0]["feature_history_unavailable"], 1)

    def test_stale_source_never_labels_or_bridges(self):
        events = [book(1000), book(2000), book(2500, source=2000),
                  book(3000, source=2000), book(3250, source=2000),
                  book(3500), book(4000), book(4500), book(5000), book(5500)]
        rows, coverage = extract(events, 2000, 6000)
        self.assertNotIn(2000, [r.at_ms for r in rows])
        self.assertGreater(coverage[0].get("label_gap", 0), 0)

    def test_split_withholds_due_and_arrival_at_boundary(self):
        events = [book(t, 100+t/100000) for t in range(1000, 9001, 250)]
        rows, coverage = extract(events, 2000, 8000, split_ms=5000)
        self.assertTrue(all(r.label_at_ms < 5000 for r in rows if r.partition == 0))
        self.assertNotIn(4000, [r.at_ms for r in rows])
        self.assertEqual(coverage[0]["split_boundary_withheld"], 1)
        # A delayed label that would arrive at the boundary is also withheld.
        delayed, cov = extract(events, 2000, 8000, split_ms=5000,
                               delay_ms=1000, max_age_ms=2000)
        self.assertTrue(all(r.label_at_ms < 5000 for r in delayed if r.partition == 0))
        self.assertEqual(cov[0]["label_after_partition"], 1)
        with self.assertRaisesRegex(ValueError, "partition boundary"):
            extract(events, 2000, 8000, split_ms=4250)

    def test_nonmonotone_receipts_rejected(self):
        with self.assertRaisesRegex(ValueError, "receipt times decrease"):
            extract([book(1000), book(3000), book(2000)], 2000, 7000)

    def test_empty_and_cutoff_are_not_zero_returns(self):
        rows, cov = extract([], 2000, 7000)
        self.assertEqual(rows, [])
        self.assertEqual(cov[0]["feature_book_unavailable"], 5)
        rows, cov = extract([book(1000), book(2000), book(2500)], 2000, 4000)
        self.assertEqual(rows, [])
        self.assertGreater(cov[0].get("label_unavailable_at_cutoff", 0), 0)

    @staticmethod
    def synthetic():
        return [Sample(i*1000, i*1000+1000, 0, (x, y), 2+3*x-4*y)
                for i, (x, y) in enumerate(( (x, y) for x in range(-3, 4)
                                             for y in range(-2, 3)))]

    def test_fit_has_unpenalized_intercept_and_training_only_transform(self):
        training = self.synthetic()
        model = fit(training, ridge=1e-10)
        self.assertAlmostEqual(model.mean_y, 2)
        self.assertAlmostEqual(model.predict((.5, -.75)), 6.5, places=7)
        before = model.predict((1, 1))
        score([Sample(100000, 101000, 1, (1e6, -1e6), -1e9)], model)
        self.assertEqual(before, model.predict((1, 1)))
        self.assertLess(score(training, model)["metrics"]["frozen_linear"]["mse_bps2"], 1e-10)

    def test_degenerate_features_fail_without_fallback(self):
        same = [Sample(i, i+1, 0, (0, i), i) for i in range(10)]
        with self.assertRaisesRegex(ValueError, "unidentified"):
            fit(same)
        with self.assertRaises(ValueError):
            fit(self.synthetic(), ridge=float("nan"))
        with self.assertRaises(ValueError):
            fit(self.synthetic()).predict((1, float("inf")))

    def test_pooling_uses_squared_error_sums_not_average_window_mse(self):
        model = fit(self.synthetic())
        a = score(self.synthetic()[:2], model)
        b = score(self.synthetic()[2:], model)
        pooled = combined([a, b])
        all_rows = score(self.synthetic(), model)
        self.assertEqual(pooled["rows"], all_rows["rows"])
        self.assertAlmostEqual(pooled["mse_bps2"]["zero"],
                               all_rows["metrics"]["zero"]["mse_bps2"])
        self.assertIsNone(combined([])["mse_bps2"]["zero"])

    def test_freeze_preserves_original_and_rejects_different_output(self):
        with tempfile.TemporaryDirectory() as tmp:
            p = Path(tmp)/"model.json"
            write_once(p, {"beta": [1, 2]})
            before = p.read_bytes()
            write_once(p, {"beta": [1, 2]})
            self.assertEqual(before, p.read_bytes())
            with self.assertRaises(ValueError):
                write_once(p, {"beta": [3, 4]})
            self.assertEqual(json.loads(p.read_text()), {"beta": [1, 2]})


if __name__ == "__main__":
    unittest.main()
