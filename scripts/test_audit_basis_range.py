import unittest
from audit_basis_range import RangeAudit


class BasisRangeTests(unittest.TestCase):
    def test_time_weighting_expires_and_does_not_weight_bursts_as_time(self):
        s = RangeAudit(1000, 4000, lookback=1000, gap=250)
        for t in range(1000, 2001, 100):
            s.observe(t, 0, (t, t), t + 2200)
        # Ready at 2000; next change at 2100, so 100 ms at the floor.
        s.observe(2100, 2, (2100, 2100), 4300)
        # Same-timestamp bursts create no time or artificial samples.
        for i in range(10):
            s.observe(2100, 2, (2100, 2100+i), 4300)
        r = s.report()
        self.assertEqual(r["ready_time_ms"], 350)
        self.assertEqual(r["range_time_ms"]["zero_within_1e-9_bps"], 100)
        self.assertEqual(r["range_time_ms"]["below_3_bps"], 250)
        self.assertAlmostEqual(r["interior_0_to_3_bps_fraction_of_ready_time"], 250/350)

    def test_empty_resets_are_counted_and_duplicates_do_not_warm_up(self):
        s = RangeAudit(1000, 4000, lookback=1000, gap=250)
        s.reset("offline", 1000)
        s.reset("offline", 1100)
        for t in range(1200, 3000, 100):
            s.observe(t, 1, (1000, 1000), 3200)
        r = s.report()
        self.assertEqual(r["reset_events_including_empty_history"]["offline"], 2)
        self.assertEqual(r["resets_that_cleared_samples"]["offline"], 0)
        self.assertEqual(r["ready_time_ms"], 0)


if __name__ == "__main__":
    unittest.main()
