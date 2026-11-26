#!/usr/bin/env python3
"""Unit tests for scripts/sim_queue_priority_mm.py."""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import sim_queue_priority_mm as sim


class QueuePrioritySimTests(unittest.TestCase):
    def test_baseline_loses_queue_while_qprio_keeps_and_captures_spread(self) -> None:
        # Synthetic wide-spread market with benign two-way flow and slight touch jitter.
        events = []
        t = 1_000
        # Initial book: 100.00 / 100.04 (4 ticks, 4 bp) with $10k ahead at 100.00 / 100.04
        events.append({"ts": t, "kind": "book", "bids": [(100.00, 100.0)], "asks": [(100.04, 100.0)]})
        events.append({"ts": t, "kind": "ref", "venue": "lighter", "bid": 100.01, "ask": 100.03})

        # Touch moves 1 tick outward (99.99 / 100.05) then back: baseline cancels & loses queue,
        # whereas B/C/D penny inside or keep resting orders with 0 queue ahead!
        t += 50
        events.append({"ts": t, "kind": "book", "bids": [(99.99, 100.0)], "asks": [(100.05, 100.0)]})
        t += 50
        # Small retail buy/sell prints at 100.03 / 100.01 (inside old touch)
        events.append({"ts": t, "kind": "trade", "side": "BUY", "price": 100.03, "size": 2.0})
        t += 200
        events.append({"ts": t, "kind": "trade", "side": "SELL", "price": 100.01, "size": 2.0})
        t += 1_500
        events.append({"ts": t, "kind": "book", "bids": [(100.00, 100.0)], "asks": [(100.04, 100.0)]})

        arms = {a.name: a for a in sim.default_arms(0.0)}
        res_a = sim.simulate_arm(events, 0.01, arms["A_baseline_back_of_queue"])
        res_d = sim.simulate_arm(events, 0.01, arms["D_qprio_lead_min_edge_skew"])

        self.assertEqual(res_a["fills"], 0)
        self.assertGreaterEqual(res_d["fills"], 2)
        self.assertGreater(res_d["equity_usd"], 0.0)
        self.assertGreater(res_d["orders_kept"], 0)

    def test_lead_lag_pull_avoids_toxic_sweep_on_endangered_side(self) -> None:
        events = [
            {"ts": 1_000, "kind": "book", "bids": [(100.00, 50.0)], "asks": [(100.04, 50.0)]},
            {"ts": 1_000, "kind": "ref", "venue": "lighter", "bid": 100.01, "ask": 100.03},
            # Reference jumps +3 bp UP at t=1_050 -> Ask side should cancel before toxic sweep at t=1_100!
            {"ts": 1_050, "kind": "ref", "venue": "lighter", "bid": 100.04, "ask": 100.06},
            {"ts": 1_100, "kind": "trade", "side": "BUY", "price": 100.05, "size": 100.0},
            {"ts": 1_100, "kind": "book", "bids": [(100.04, 50.0)], "asks": [(100.06, 50.0)]},
            {"ts": 12_000, "kind": "book", "bids": [(100.04, 50.0)], "asks": [(100.06, 50.0)]},
        ]
        arms = {a.name: a for a in sim.default_arms(0.0)}
        res_b = sim.simulate_arm(events, 0.01, arms["B_queue_priority_ladder"])
        res_c = sim.simulate_arm(events, 0.01, arms["C_qprio_lead_pull"])

        self.assertGreater(res_b["fills"], 0)
        self.assertLess(res_b["equity_usd"], 0.0)
        self.assertEqual(res_c["fills"], 0)
        self.assertGreaterEqual(res_c["equity_usd"], res_b["equity_usd"])

    def test_mid_at_markout_never_extrapolates_past_capture_end(self) -> None:
        events = [
            {"ts": 1_000, "kind": "book", "bids": [(100.00, 1.0)], "asks": [(100.04, 1.0)]},
            {"ts": 1_050, "kind": "trade", "side": "BUY", "price": 100.04, "size": 5.0},
            {"ts": 5_000, "kind": "book", "bids": [(100.00, 1.0)], "asks": [(100.04, 1.0)]},
        ]
        res = sim.simulate_arm(
            events,
            0.01,
            sim.SimArmConfig(name="bounded_markout", keep_queue_ticks=2, penny_min_ticks=99),
            markout_horizons_ms=(1_000, 10_000, 30_000),
        )
        self.assertEqual(res["fills"], 1)
        self.assertIsNotNone(res["markouts"]["1s_bps"])
        self.assertIsNone(res["markouts"]["10s_bps"])
        self.assertIsNone(res["markouts"]["30s_bps"])

    def test_in_flight_alo_rejects_when_touch_crosses_before_arrival(self) -> None:
        events = [
            {"ts": 1_000, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.04, 10.0)]},
            # Price moves DOWN sharply while 36ms ALO Bid@100.00 is in flight (arrives at 1036ms)
            {"ts": 1_020, "kind": "book", "bids": [(99.96, 10.0)], "asks": [(100.00, 10.0)]},
            {"ts": 1_040, "kind": "trade", "side": "SELL", "price": 99.95, "size": 2.0},
            {"ts": 1_500, "kind": "book", "bids": [(99.96, 10.0)], "asks": [(100.00, 10.0)]},
        ]
        arm = sim.SimArmConfig(
            name="alo_flight",
            order_delay_ms=36,
            cancel_delay_ms=33,
            keep_queue_ticks=4,
            penny_min_ticks=99,
            alo_reject_cooldown_ms=350,
        )
        res = sim.simulate_arm(events, 0.01, arm)
        self.assertEqual(res["fills"], 0)
        self.assertGreaterEqual(res["alo_rejected"], 1)

    def test_strict_through_sweep_ignores_normal_touch_trade(self) -> None:
        events = [
            {"ts": 1_000, "kind": "book", "bids": [(100.00, 50.0)], "asks": [(100.04, 50.0)]},
            # Small retail trade right at touch (100.04, size=1.0) after order activates at 1036ms
            {"ts": 1_050, "kind": "trade", "side": "BUY", "price": 100.04, "size": 1.0},
            {"ts": 1_100, "kind": "book", "bids": [(100.00, 50.0)], "asks": [(100.04, 50.0)]},
        ]
        arm = sim.SimArmConfig(
            name="strict_sweep",
            order_delay_ms=36,
            cancel_delay_ms=33,
            keep_queue_ticks=4,
            penny_min_ticks=4,
            sweep_pull_ratio=0.75,
            sweep_min_notional_usd=200.0,
        )
        res = sim.simulate_arm(events, 0.01, arm)
        self.assertEqual(res["orders_canceled"], 0)

    def test_validated_lead_validator_mutes_lagging_ref(self) -> None:
        events = []
        t = 1_000
        # Target moves down while lagging ref moves up -> ref earns negative lead score and is muted.
        for step in range(8):
            tp = 100.00 - step * 0.05
            rp = 100.00 + step * 0.05
            events.append({"ts": t, "kind": "book", "bids": [(tp - 0.02, 5.0)], "asks": [(tp + 0.02, 5.0)]})
            events.append({"ts": t, "kind": "ref", "venue": "lagging", "bid": rp - 0.01, "ask": rp + 0.01})
            t += 500
        arm = sim.SimArmConfig(
            name="v3_lag_mute",
            order_delay_ms=36,
            cancel_delay_ms=33,
            keep_queue_ticks=4,
            penny_min_ticks=4,
            lead_pull_bps=0.50,
            require_validated_lead_ref=True,
            lead_validation_horizon_ms=500,
            lead_validation_min_move_bps=1.0,
            lead_validation_min_samples=3,
            lead_validation_min_score_bps=0.05,
        )
        res = sim.simulate_arm(events, 0.01, arm)
        self.assertEqual(res["validated_lead_refs_end"], 0)

    def test_v4_adaptive_shield_and_combined_mtm_and_book_diff_mode(self) -> None:
        # 1. combined_mtm_usd == equity_usd + terminal_basis_usd explicitly reported
        events = [
            {"ts": 1_000, "kind": "book", "bids": [(100.00, 50.0)], "asks": [(100.04, 50.0)]},
            {"ts": 1_000, "kind": "ref", "venue": "lighter", "bid": 100.01, "ask": 100.03},
            {"ts": 1_100, "kind": "trade", "side": "BUY", "price": 100.04, "size": 5.0},
            {"ts": 1_500, "kind": "ref", "venue": "lighter", "bid": 100.06, "ask": 100.08},
            {"ts": 2_000, "kind": "book", "bids": [(100.00, 50.0)], "asks": [(100.04, 50.0)]},
        ]
        arms = {a.name: a for a in sim.default_arms_for_profile("fast_zero_fee_alo")}
        self.assertIn("F_v4_adaptive_shield", arms)
        self.assertEqual(arms["F_v4_adaptive_shield"].lead_validation_min_samples, 5)
        res = sim.simulate_arm(events, 0.01, arms["E_v3_validated_lead_qprio"])
        self.assertAlmostEqual(
            res["combined_mtm_usd"],
            round(res["equity_usd"] + res["terminal_basis_usd"], 4),
            places=3,
        )

        # 2. ALO flight OBI guard blocks back-of-queue opening chase on narrow skewed book
        skewed = [
            {"ts": 1_000, "kind": "book", "bids": [(100.00, 1.0)], "asks": [(100.01, 20.0)]},
            {"ts": 2_000, "kind": "book", "bids": [(100.00, 1.0)], "asks": [(100.01, 20.0)]},
        ]
        f_arm = sim.SimArmConfig(
            name="flight_obi",
            order_delay_ms=40,
            cancel_delay_ms=40,
            keep_queue_ticks=4,
            penny_min_ticks=99,
            min_edge_bps=0.1,
            alo_flight_guard_obi_threshold=0.5,
        )
        res_f = sim.simulate_arm(skewed, 0.01, f_arm)
        # Only 1 side (Ask) placed initially; Bid skipped due to -0.90 OBI -> places <= 2 total across both ticks
        self.assertLessEqual(res_f["orders_placed"], 2)

        # 3. Book-diff diagnostic mode infers sweeps on trade-less streams (e.g. Bulk BBO)
        tradeless = [
            {"ts": 1_000, "kind": "book", "bids": [(100.00, 1.0)], "asks": [(100.04, 1.0)]},
            {"ts": 1_200, "kind": "book", "bids": [(100.00, 1.0)], "asks": [(100.04, 5.0)]},
            {"ts": 1_400, "kind": "book", "bids": [(100.04, 3.0)], "asks": [(100.06, 5.0)]},
            {"ts": 3_000, "kind": "book", "bids": [(100.04, 3.0)], "asks": [(100.06, 5.0)]},
        ]
        bd_arm = sim.SimArmConfig(
            name="bd",
            order_delay_ms=40,
            cancel_delay_ms=40,
            keep_queue_ticks=4,
            penny_min_ticks=4,
            min_edge_bps=0.1,
            infer_book_diff_sweeps=True,
        )
        res_bd = sim.simulate_arm(tradeless, 0.01, bd_arm)
        self.assertEqual(res_bd["fill_evidence_mode"], "book_diff_diagnostic")
        self.assertGreaterEqual(res_bd["inferred_book_diff_sweeps"], 1)

    def test_disable_bbo_sweep_fills_blocks_bbo_sweeps_and_only_fills_on_trades(self) -> None:
        self.assertIs(sim.SimParams, sim.SimArmConfig)
        bbo_crossed_only = [
            {"ts": 1_000, "kind": "book", "bids": [(100.00, 1.0)], "asks": [(100.04, 1.0)]},
            {"ts": 1_200, "kind": "book", "bids": [(100.00, 1.0)], "asks": [(100.04, 5.0)]},
            {"ts": 1_400, "kind": "book", "bids": [(100.04, 3.0)], "asks": [(100.06, 5.0)]},
            {"ts": 3_000, "kind": "book", "bids": [(100.04, 3.0)], "asks": [(100.06, 5.0)]},
        ]
        bd_disabled = sim.SimParams(
            name="bd_disabled",
            order_delay_ms=40,
            cancel_delay_ms=40,
            keep_queue_ticks=4,
            penny_min_ticks=4,
            min_edge_bps=0.1,
            infer_book_diff_sweeps=True,
            disable_bbo_sweep_fills=True,
        )
        res_no_tr = sim.simulate_arm(bbo_crossed_only, 0.01, bd_disabled)
        self.assertEqual(res_no_tr["fill_evidence_mode"], "public_trades")
        self.assertEqual(res_no_tr["inferred_book_diff_sweeps"], 0)
        self.assertEqual(res_no_tr["fills"], 0)

        # With a real trade-through print present, fills ONLY via public print and computes markouts cleanly
        with_real_print = [
            {"ts": 1_000, "kind": "book", "bids": [(100.00, 1.0)], "asks": [(100.04, 1.0)]},
            {"ts": 1_100, "kind": "trade", "side": "BUY", "price": 100.04, "size": 5.0},
            {"ts": 1_400, "kind": "book", "bids": [(100.04, 3.0)], "asks": [(100.06, 5.0)]},
            {"ts": 3_000, "kind": "book", "bids": [(100.04, 3.0)], "asks": [(100.06, 5.0)]},
        ]
        res_tr = sim.simulate_arm(with_real_print, 0.01, bd_disabled, markout_horizons_ms=(1_000,))
        self.assertEqual(res_tr["fill_evidence_mode"], "public_trades")
        self.assertEqual(res_tr["inferred_book_diff_sweeps"], 0)
        self.assertEqual(res_tr["fills"], 1)
        self.assertIsNotNone(res_tr["markouts"]["1s_bps"])


if __name__ == "__main__":
    unittest.main()
