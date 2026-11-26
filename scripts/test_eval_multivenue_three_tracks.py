#!/usr/bin/env python3
"""Unit tests for scripts/eval_multivenue_three_tracks.py."""

from __future__ import annotations

import json
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import eval_multivenue_three_tracks as ev


class MultiVenueThreeTracksTests(unittest.TestCase):
    def test_continuity_stats_enforces_exact_10h_and_max_gap(self) -> None:
        short = ev.compute_continuity_stats([0, 35_999_000])
        self.assertFalse(short["is_continuous_10h"])

        gapped = ev.compute_continuity_stats([0, 120_000, 36_120_000])
        self.assertFalse(gapped["is_continuous_10h"])

        valid = ev.compute_continuity_stats([i * 30_000 for i in range(1202)])
        self.assertTrue(valid["is_continuous_10h"])
        self.assertGreaterEqual(valid["duration_ms"], 36_000_000)

    def test_track1_microstructure_computes_spread_vol_and_fee_adjusted_edge(self) -> None:
        books = []
        t = 1_000_000
        for i in range(40):
            mid = 100.0 + (0.02 if i % 2 else -0.02)
            books.append((t + i * 1_000, mid - 0.05, 10.0, mid + 0.05, 10.0))
        res = ev.compute_track1_microstructure_metrics(books, maker_fee_bps=0.3)
        self.assertEqual(res["samples"], 40)
        self.assertAlmostEqual(res["median_spread_bps"], 10.0, delta=0.1)
        self.assertGreater(res["realized_1s_vol_bps"], 0.0)
        self.assertGreater(res["half_spread_minus_fee_bps"], 4.0)

    def test_track2_basis_pair_metrics_is_strictly_causal_and_checks_hurdles(self) -> None:
        # books_a at t=1000, 2000; books_b updates at t=900, 1900 (lag 100ms <= 500ms)
        # Dislocation spikes to +12 bps at t=2000
        books_a = [(1_000 + i * 1_000, 99.99, 100.01) for i in range(30)]
        books_b = [(900 + i * 1_000, 99.99, 100.01) for i in range(30)]
        books_a[24] = (25_000, 100.11, 100.13)
        books_a[25] = (26_000, 100.11, 100.13)
        books_a[26] = (27_000, 100.11, 100.13)
        res = ev.compute_track2_basis_pair_metrics(
            books_a,
            books_b,
            max_sync_skew_ms=500,
            dual_taker_fee_bps=3.0,
            maker_first_fee_bps=1.15,
        )
        self.assertEqual(res["paired_samples"], 30)
        self.assertGreater(res["p95_abs_disloc_bps"], 0.0)
        self.assertGreaterEqual(res["maker_first_exceed_pct"], res["dual_taker_exceed_pct"])
        rendered = json.dumps(res, allow_nan=False)
        self.assertIn("paired_samples", rendered)

    def test_holdout_w25_campaign_and_midnight_utc_globbing(self) -> None:
        import tempfile

        self.assertIn("holdout_w25", ev.CANONICAL_CAMPAIGNS)
        w25 = ev.CANONICAL_CAMPAIGNS["holdout_w25"]
        self.assertEqual(
            w25["crypto"],
            {"campaign_id": "tokyo-crypto-20261125T194507Z-w25", "utc_date": "2026-11-25"},
        )
        self.assertEqual(
            w25["rwa"],
            {"campaign_id": "tokyo-rwa-20261125T194453Z-w25", "utc_date": "2026-11-25"},
        )
        self.assertEqual(
            w25["bulk"],
            {"campaign_id": "tokyo-bulk-20261125T194449Z-w25", "utc_date": "2026-11-25"},
        )

        with tempfile.TemporaryDirectory() as td:
            root = Path(td)
            cid = w25["crypto"]["campaign_id"]
            p1 = root / f"parquet/bbo_quotes/collector_region=tokyo/venue=arcus/utc_date=2026-11-25/campaign_id={cid}/part-0.parquet"
            p2 = root / f"parquet/bbo_quotes/collector_region=tokyo/venue=arcus/utc_date=2026-11-26/campaign_id={cid}/part-1.parquet"
            p3 = root / f"parquet/bbo_quotes/collector_region=tokyo/venue=arcus/utc_date=2026-11-24/campaign_id={cid}/old.parquet"
            for p in (p1, p2, p3):
                p.parent.mkdir(parents=True, exist_ok=True)
                p.write_bytes(b"")
            files = ev._campaign_parquet_files(root, "bbo_quotes", "arcus", "2026-11-25", cid)
            self.assertEqual(files, [str(p1), str(p2)])


if __name__ == "__main__":
    unittest.main()
