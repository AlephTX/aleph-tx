import math
import unittest
from decimal import Decimal as D

from maker_hedge_risk_guards import (
    GapAwareBasisTracker,
    InFlightQuoteReservation,
    SECOND_NS,
    UnhedgedExposureMonitor,
    UnitCostAuditor,
    arm_strictly_cheaper_across_range,
    breakeven_hedge_fee_bp,
    evaluate_arm_costs,
    unit_cost_per_m,
)


class InFlightQuoteReservationTests(unittest.TestCase):
    def test_cancel_before_place_blocks_same_side_but_allows_other_side(self):
        res = InFlightQuoteReservation(maker_cap_usd="500", net_cap_usd="500")
        self.assertTrue(res.place_quote("bid-1", 1, "2", "100", "100", 1_000))
        self.assertTrue(res.request_cancel("bid-1", 2_000, 12_000))

        # Cannot place replacement bid while bid-1 is pending_cancel under default mode.
        self.assertFalse(res.place_quote("bid-2", 1, "2", "100", "100", 3_000))
        self.assertEqual(res.last_reject_reason, "pending_cancel_on_side")

        # Opposite side is independent and within cap.
        self.assertTrue(res.place_quote("ask-1", -1, "2", "100", "100", 4_000))

        # Cannot confirm terminal before cancel_eff_ns.
        with self.assertRaises(ValueError):
            res.confirm_terminal("bid-1", 11_999)

        self.assertEqual(res.settle_modeled_cancels(12_000), ["bid-1"])
        self.assertTrue(res.place_quote("bid-3", 1, "2", "100", "100", 13_000))

    def test_overlap_mode_still_reserves_pending_cancel_and_limit_worst_price(self):
        res = InFlightQuoteReservation(
            maker_cap_usd="500",
            net_cap_usd="500",
            allow_replace_overlap=True,
        )
        self.assertTrue(res.place_quote("bid-1", 1, "3", "100", "100", 1_000))
        self.assertTrue(res.request_cancel("bid-1", 2_000, 12_000))

        # Pending cancel buy of 3 + new buy of 3 = 6 * 100 = 600 > 500 -> blocked.
        self.assertFalse(res.place_quote("bid-2", 1, "3", "100", "100", 3_000))
        self.assertEqual(res.last_reject_reason, "maker_cap_exceeded")

        # Even if ref_mid=95, higher limit_price=101 drives max(ref_mid, limit_price) -> 5 * 101 > 500 -> blocked.
        self.assertFalse(res.place_quote("bid-3", 1, "2", "101", "95", 4_000))
        self.assertEqual(res.last_reject_reason, "maker_cap_exceeded")

        # 2 * 100 + pending 3 * 100 = 500 <= 500 -> allowed.
        self.assertTrue(res.place_quote("bid-4", 1, "2", "100", "99", 5_000))

        # Hedge short shifts net headroom without loosening maker_cap_usd.
        res.record_hedge_fill("-2", 6_000)
        bounds = res.exposure_bounds_base()
        self.assertEqual(bounds["maker"], (D("0"), D("5")))
        self.assertEqual(bounds["net"], (D("-2"), D("3")))

    def test_in_flight_fill_and_unknown_freeze(self):
        res = InFlightQuoteReservation(maker_cap_usd="1000", net_cap_usd="1000")
        self.assertTrue(res.place_quote("bid-1", 1, "4", "100", "100", 1_000))
        self.assertTrue(res.request_cancel("bid-1", 2_000, 12_000))

        # Partial fill arrives while cancel is in flight.
        self.assertTrue(res.record_fill("bid-1", "1.5", "99.5", 5_000))
        self.assertEqual(res.cancel_in_flight_fills, 1)
        self.assertEqual(res.maker_pos_base, D("1.5"))

        # Price worse than limit is rejected.
        with self.assertRaises(ValueError):
            res.record_fill("bid-1", "0.5", "100.5", 6_000)

        # Marking unknown freezes new quotes and resists replay settlement/terminal.
        res.mark_unknown("bid-1", "cancel_ack_timeout", 20_000)
        self.assertEqual(res.settle_modeled_cancels(30_000), [])
        with self.assertRaises(ValueError):
            res.confirm_terminal("bid-1", 31_000)
        with self.assertRaises(ValueError):
            res.record_fill("bid-1", "0.5", "100", 32_000)
        self.assertFalse(res.place_quote("ask-1", -1, "1", "100", "100", 33_000))
        self.assertEqual(res.last_reject_reason, "unknown_order_halt")

        summary = res.summary()
        self.assertEqual(summary["maker_bounds_base"], (D("1.5"), D("4.0")))
        self.assertEqual(summary["unknown_orders"], 1)

    def test_clock_regression_and_duplicate_id_rejected(self):
        res = InFlightQuoteReservation(maker_cap_usd="500", net_cap_usd="500")
        self.assertTrue(res.place_quote("q1", 1, "1", "100", "100", 10_000))
        with self.assertRaises(ValueError):
            res.place_quote("q1", 1, "1", "100", "100", 10_000)
        with self.assertRaises(ValueError):
            res.place_quote("q2", 1, "1", "100", "100", 9_999)


class UnhedgedExposureMonitorTests(unittest.TestCase):
    def test_event_driven_sub_minute_integration_and_duration_halt(self):
        mon = UnhedgedExposureMonitor(
            hedge_trigger_usd="1000",
            trigger_tolerance_usd="1",
            max_unhedged_duration_ns=10 * SECOND_NS,
        )
        # Sub-minute 15s spike between minute marks (t=5s..20s) is captured exactly.
        mon.observe(0, "5", "0", "100")  # $500 <= $1001
        mon.observe(5 * SECOND_NS, "15", "0", "100")  # $1500 > $1001 (excess $500)
        self.assertEqual(mon.should_halt_new_risk(15 * SECOND_NS), (False, None))
        self.assertEqual(mon.should_halt_new_risk(16 * SECOND_NS), (True, "unhedged_duration_exceeded"))

        mon.observe(20 * SECOND_NS, "15", "-10", "100")  # hedged back to $500
        self.assertEqual(mon.should_halt_new_risk(20 * SECOND_NS), (False, None))

        # Open episode at end of window integrated by finalize().
        mon.observe(40 * SECOND_NS, "18", "-10", "120")  # net=8 * 120 = $960 <= 1001
        mon.observe(50 * SECOND_NS, "21", "-10", "120")  # net=11 * 120 = $1320 (excess $320)
        out = mon.finalize(60 * SECOND_NS)

        self.assertEqual(out["max_continuous_above_trigger_ns"], 15 * SECOND_NS)
        self.assertEqual(out["total_above_trigger_ns"], 25 * SECOND_NS)
        # 15s * $500 + 10s * $320 = $10,700 USD*s
        self.assertEqual(out["time_weighted_excess_usd_sec"], D("10700"))
        self.assertEqual(out["max_abs_net_usd"], D("1500"))
        self.assertEqual(len(out["episodes"]), 2)
        self.assertTrue(out["episodes"][0]["closed"])
        self.assertTrue(out["episodes"][1]["open_at_end"])

    def test_hedge_exhaustion_halt_and_recovery_requires_within_trigger(self):
        mon = UnhedgedExposureMonitor(hedge_trigger_usd="1000")
        mon.observe(0, "14", "0", "100")  # $1400 > $1000
        mon.record_hedge_failure(1 * SECOND_NS, "-4", "stale_rh_book", retries_exhausted=False)
        self.assertEqual(mon.retry_events, 1)
        self.assertEqual(mon.exhausted_events, 0)
        self.assertEqual(mon.should_halt_new_risk(1 * SECOND_NS), (False, None))

        mon.record_hedge_failure(3 * SECOND_NS, "-4", "stale_rh_book", retries_exhausted=True)
        self.assertEqual(mon.exhausted_events, 1)
        self.assertEqual(mon.next_recovery_check_ns, 8 * SECOND_NS)
        self.assertFalse(mon.recovery_due(7 * SECOND_NS))
        self.assertTrue(mon.recovery_due(8 * SECOND_NS))
        self.assertEqual(
            mon.should_halt_new_risk(4 * SECOND_NS),
            (True, "hedge_retries_exhausted:stale_rh_book"),
        )

        # Partial hedge leaves net at $1100 (> $1000): halt MUST stay active.
        mon.record_hedge_success(9 * SECOND_NS, "14", "-3", "100")
        self.assertTrue(mon.hedge_unrecovered)
        self.assertEqual(
            mon.should_halt_new_risk(9 * SECOND_NS),
            (True, "hedge_retries_exhausted:stale_rh_book"),
        )

        # Final hedge returns confirmed net to $900 (<= $1000): halt clears cleanly.
        mon.record_hedge_success(10 * SECOND_NS, "14", "-5", "100")
        self.assertFalse(mon.hedge_unrecovered)
        self.assertEqual(mon.should_halt_new_risk(10 * SECOND_NS), (False, None))

    def test_clock_regression_rejected(self):
        mon = UnhedgedExposureMonitor(hedge_trigger_usd="1000")
        mon.observe(100, "1", "0", "100")
        with self.assertRaises(ValueError):
            mon.observe(99, "1", "0", "100")
        with self.assertRaises(ValueError):
            mon.record_hedge_failure(99, "1", "x", retries_exhausted=True)
        with self.assertRaises(ValueError):
            mon.should_halt_new_risk(99)

    def test_codex_repro_recovery_requires_fresh_position_and_matching_generation(self):
        mon = UnhedgedExposureMonitor(hedge_trigger_usd="1000")
        mon.observe(0, "0", "0", "100")
        mon.record_hedge_failure(10 * SECOND_NS, "-10", "synthetic_gap", retries_exhausted=True)
        gen1 = mon.recovery_generation
        self.assertEqual(gen1, 1)
        self.assertEqual(mon.exhausted_at_ns, 10 * SECOND_NS)

        # Success notification without fresh position MUST keep halt active (no stale last_obs clearance).
        mon.record_hedge_success(11 * SECOND_NS)
        halted, reason = mon.should_halt_new_risk(11 * SECOND_NS)
        self.assertTrue(halted)
        self.assertEqual(reason, "hedge_retries_exhausted:synthetic_gap")
        self.assertEqual(mon.last_obs_ns, 0)

        # Stale recovery_generation from an older failure cycle cannot clear gen2 halt.
        mon.record_hedge_failure(12 * SECOND_NS, "-5", "second_gap", retries_exhausted=True)
        self.assertEqual(mon.recovery_generation, 2)
        mon.record_hedge_success(13 * SECOND_NS, "0", "0", "100", recovery_generation=gen1)
        self.assertTrue(mon.hedge_unrecovered)

        # Matching current recovery_generation + flat authoritative position clears cleanly.
        mon.record_hedge_success(14 * SECOND_NS, "0", "0", "100", recovery_generation=2)
        self.assertFalse(mon.hedge_unrecovered)
        self.assertEqual(mon.should_halt_new_risk(14 * SECOND_NS), (False, None))

        # Untagged observe at exact exhausted_at_ns must NOT clear exhaustion triggered at that instant.
        mon.record_hedge_failure(15 * SECOND_NS, "-5", "same_ns_gap", retries_exhausted=True)
        mon.observe(15 * SECOND_NS, "0", "0", "100")
        self.assertTrue(mon.hedge_unrecovered)
        mon.observe(15 * SECOND_NS, "0", "0", "100", recovery_generation=mon.recovery_generation)
        self.assertFalse(mon.hedge_unrecovered)

        # Test InFlightQuoteReservation mixed-price outer notional reservation (Codex repro).
        quot = InFlightQuoteReservation(maker_cap_usd="500", net_cap_usd="500", allow_replace_overlap=True)
        self.assertTrue(quot.place_quote("known-sell", -1, "3", "120", "120", 10))
        self.assertTrue(quot.request_cancel("known-sell", 11, 90))
        # 3 * max(120, 100) + 2 * max(100, 100) = 360 + 200 = 560 > 500 -> MUST reject!
        self.assertFalse(quot.place_quote("candidate-sell", -1, "2", "100", "100", 12))
        self.assertEqual(quot.last_reject_reason, "maker_cap_exceeded")
        # 1 * 100 + 360 = 460 <= 500 -> allowed.
        self.assertTrue(quot.place_quote("smaller-sell", -1, "1", "100", "100", 13))


class GapAwareBasisTrackerTests(unittest.TestCase):
    def test_gap_reset_stream_reset_and_warmup_suppression(self):
        tracker = GapAwareBasisTracker(
            tau_s=120.0,
            max_stale_ns=2 * SECOND_NS,
            max_gap_reset_ns=10 * SECOND_NS,
            warmup_samples=2,
            warmup_span_ns=2 * SECOND_NS,
        )
        # Warmup takes >=2 samples and >=2s span.
        self.assertIsNone(tracker.update(0, 100.0, 100.0, 0, 0))
        self.assertIsNone(tracker.update(1 * SECOND_NS, 100.0, 100.0, 1 * SECOND_NS, 1 * SECOND_NS))
        gap = tracker.update(2 * SECOND_NS, 100.0, 100.1, 2 * SECOND_NS, 2 * SECOND_NS)
        self.assertIsNotNone(gap)
        self.assertAlmostEqual(gap, math.log(100.1 / 100.0) * 1e4, places=6)

        # Inter-update gap > 10s resets EWMA and suppresses spike when permanent basis shifts +50 bp.
        t_after_gap = 20 * SECOND_NS
        self.assertIsNone(tracker.update(t_after_gap, 100.0, 100.5, t_after_gap, t_after_gap))
        self.assertEqual(tracker.gap_resets, 1)
        self.assertIsNone(
            tracker.update(t_after_gap + SECOND_NS, 100.0, 100.5, t_after_gap + SECOND_NS, t_after_gap + SECOND_NS)
        )
        post_gap = tracker.update(
            t_after_gap + 2 * SECOND_NS,
            100.0,
            100.5,
            t_after_gap + 2 * SECOND_NS,
            t_after_gap + 2 * SECOND_NS,
        )
        self.assertIsNotNone(post_gap)
        self.assertAlmostEqual(post_gap, 0.0, places=6)

        # Explicit stream reset also triggers full re-warmup.
        tracker.on_stream_reset("rh", 30 * SECOND_NS)
        self.assertEqual(tracker.stream_resets["rh"], 1)
        self.assertIsNone(tracker.update(31 * SECOND_NS, 100.0, 100.5, 31 * SECOND_NS, 31 * SECOND_NS))
        self.assertIsNone(tracker.update(32 * SECOND_NS, 100.0, 100.5, 32 * SECOND_NS, 32 * SECOND_NS))
        self.assertIsNotNone(tracker.update(33 * SECOND_NS, 100.0, 100.5, 33 * SECOND_NS, 33 * SECOND_NS))

    def test_stale_and_future_timestamps_rejected_and_channel_skew_detected(self):
        tracker = GapAwareBasisTracker(max_stale_ns=2 * SECOND_NS, warmup_samples=1, channel_skew_alert_ms=50.0)
        # Future rx timestamp on either leg -> clock anomaly, not counted as valid sample.
        self.assertIsNone(tracker.update(5 * SECOND_NS, 100.0, 100.0, 5 * SECOND_NS + 1, 5 * SECOND_NS))
        self.assertIsNone(tracker.update(6 * SECOND_NS, 100.0, 100.0, 6 * SECOND_NS, 6 * SECOND_NS + 10))
        self.assertEqual(tracker.clock_anomalies, 2)

        # Stale rx timestamp (>2s) on either leg -> skipped cleanly.
        self.assertIsNone(tracker.update(10 * SECOND_NS, 100.0, 100.0, 7 * SECOND_NS, 10 * SECOND_NS))
        self.assertIsNone(tracker.update(11 * SECOND_NS, 100.0, 100.0, 11 * SECOND_NS, 8 * SECOND_NS))
        self.assertEqual(tracker.stale_skips, 2)

        # Channel lag tracking (~82 ms offset between l2_snapshot and print streams).
        for ms in (92, 94, 96):
            tracker.record_channel_lag("l2_snapshot", ex_ns=0, rx_ns=ms * 1_000_000)
        for ms in (11, 12, 13):
            tracker.record_channel_lag("print", ex_ns=0, rx_ns=ms * 1_000_000)
        lag = tracker.channel_lag_summary()
        self.assertAlmostEqual(lag["max_p50_diff_ms"], 82.0, places=6)
        self.assertTrue(lag["skew_warning"])

    def test_codex_repro_post_reset_floor_and_distinct_source_warmup(self):
        guard = GapAwareBasisTracker(max_stale_ns=2 * SECOND_NS, warmup_samples=2)
        self.assertIsNone(guard.update(0, "100", "100", 0, 0))
        guard.on_stream_reset("hedge", 100_000_000)

        # Pre-reset hedge rx_ns=0 (< reset_ts=100ms) MUST NOT warm up or emit signals!
        calls = [guard.update(t, "100", "100", t, 0) for t in (200_000_000, 300_000_000, 400_000_000)]
        self.assertEqual(calls, [None, None, None])
        self.assertEqual(guard.emitted_count, 0)
        self.assertEqual(guard.reset_waiting_skips, 3)

        # Post-reset quote at t=500ms enters warmup sample 1; repeating identical (500ms, 500ms) skips!
        self.assertIsNone(guard.update(500_000_000, "100", "100", 500_000_000, 500_000_000))
        self.assertIsNone(guard.update(600_000_000, "100", "100", 500_000_000, 500_000_000))
        self.assertEqual(guard.duplicate_rx_skips, 1)
        self.assertEqual(guard.warmup_count, 1)

        # Second distinct post-reset pair completes warmup; third distinct pair emits!
        self.assertIsNone(guard.update(700_000_000, "100", "100", 700_000_000, 700_000_000))
        # Out-of-order per-leg receipt regression (e.g. stale maker snapshot replayed out of order) is rejected.
        self.assertIsNone(guard.update(750_000_000, "100", "100.5", 650_000_000, 700_000_000))
        self.assertEqual(guard.out_of_order_rx_skips, 1)
        emitted = guard.update(800_000_000, "100", "100.1", 800_000_000, 800_000_000)
        self.assertIsNotNone(emitted)
        self.assertEqual(guard.emitted_count, 1)


class UnitCostAuditorTests(unittest.TestCase):
    def test_dual_denominator_breakeven_dominance_and_terminal_flatness(self):
        auditor = UnitCostAuditor()
        # Synthetic arms reproducing exact proportional slope/intercept breakeven behavior.
        base = auditor.evaluate_arm(
            marked_equity_usd="-338.7665",
            maker_volume_usd="1964502.27",
            hedge_volume_usd="570987.46",
            terminal_maker_pos_base="1",
            terminal_hedge_pos_base="-1",
        )
        pull = auditor.evaluate_arm(
            marked_equity_usd="-127.4563",
            maker_volume_usd="1095735.05",
            hedge_volume_usd="503677.02",
            terminal_maker_pos_base="0",
            terminal_hedge_pos_base="0",
            terminal_flat_verified=True,
        )
        gate = auditor.evaluate_arm(
            marked_equity_usd="-191.9070",
            maker_volume_usd="1150977.38",
            hedge_volume_usd="454417.21",
            terminal_maker_pos_base="0",
            terminal_hedge_pos_base="0",
            terminal_flat_verified=True,
        )

        # Non-flat inventory must keep terminal_flat_cash_usd=None even when marked equity is finite.
        self.assertFalse(base["terminal_flat"])
        self.assertIsNone(base["terminal_flat_cash_usd"])
        self.assertTrue(pull["terminal_flat"])
        self.assertEqual(pull["terminal_flat_cash_usd"], D("-127.4563"))

        # Omitted positions or unverified flatness must return terminal_flat=None & terminal_flat_cash_usd=None.
        omitted = evaluate_arm_costs("17", "1000", "300")
        self.assertIsNone(omitted["terminal_flat"])
        self.assertIsNone(omitted["terminal_flat_cash_usd"])
        unknown_flat = evaluate_arm_costs(
            "-10",
            "1000",
            "0",
            terminal_maker_pos_base="0",
            terminal_hedge_pos_base="0",
            terminal_flat_verified=False,
        )
        self.assertIsNone(unknown_flat["terminal_flat"])
        self.assertIsNone(unknown_flat["terminal_flat_cash_usd"])

        # Compare dual denominators and breakeven (~3.32057 bp on synthetic proportions).
        be = breakeven_hedge_fee_bp(pull, base, denominator="maker_only")
        self.assertIsNotNone(be)
        self.assertAlmostEqual(float(be), 3.32057, places=5)
        self.assertLess(pull["dual_venue_by_fee_bp"]["0"], pull["maker_only_by_fee_bp"]["0"])
        self.assertTrue(arm_strictly_cheaper_across_range(pull, gate, "0", "3.5", denominator="maker_only"))
        self.assertTrue(arm_strictly_cheaper_across_range(pull, gate, "0", "3.5", denominator="dual_venue"))
        self.assertFalse(arm_strictly_cheaper_across_range(pull, base, "0", "3.5", denominator="maker_only"))

        comp = auditor.compare_arms(
            {"base": base, "pull": pull, "gate": gate},
            baseline_key="base",
            reference_keys=("base", "pull"),
        )
        self.assertTrue(comp["gate"]["removed_maker_volume_usd"] > 0)
        self.assertFalse(comp["gate"]["strictly_cheaper_than_vs"]["pull"]["maker_only"])

    def test_zero_volume_and_float_rejection(self):
        self.assertIsNone(unit_cost_per_m("10", "0"))
        zero_arm = evaluate_arm_costs("0", "0", "0")
        self.assertIsNone(zero_arm["maker_only_intercept_per_m"])
        self.assertIsNone(zero_arm["dual_venue_by_fee_bp"]["0"])
        self.assertIsNone(breakeven_hedge_fee_bp(zero_arm, zero_arm))
        self.assertFalse(arm_strictly_cheaper_across_range(zero_arm, zero_arm, "0", "3.5"))
        with self.assertRaises(ValueError):
            unit_cost_per_m(10.0, "100")


if __name__ == "__main__":
    unittest.main()
