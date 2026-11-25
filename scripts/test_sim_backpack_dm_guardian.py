#!/usr/bin/env python3
"""Offline unit tests for Backpack Depth-Multiplier Guardian simulator (Section 6.2 Step 2)."""
from __future__ import annotations

import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPTS_DIR = Path(__file__).resolve().parent
if str(SCRIPTS_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPTS_DIR))

from sim_backpack_dm_guardian import (
    Frame,
    QuoteLevel,
    SimConfig,
    TIER_PRESETS,
    compute_snapshot_dm,
    load_tape_jsonl,
    run_comparison,
    simulate_arm,
    validate_frames,
)

SCRIPT_PATH = SCRIPTS_DIR / "sim_backpack_dm_guardian.py"


class BackpackDmGuardianTests(unittest.TestCase):
    def test_tier3_and_tier4_dm_calculation_bandwidth_and_floor(self):
        t3 = TIER_PRESETS["tier3_alt"]
        t4 = TIER_PRESETS["tier4_equity"]
        mid = 100.0

        # Tier 3: $5k per side within 25 bps -> DM = 1.0
        dm, zero_side = compute_snapshot_dm(
            t3,
            mid,
            QuoteLevel(price=99.77, notional_usd=5000.0),   # 23 bps
            QuoteLevel(price=100.23, notional_usd=5000.0),  # 23 bps
        )
        self.assertEqual(dm, 1.0)
        self.assertFalse(zero_side)

        # Tier 3: exact 25 bps passes, 25.1 bps fails bandwidth gate -> 0.25 floor & zero_side=True
        dm_edge, zero_edge = compute_snapshot_dm(
            t3,
            mid,
            QuoteLevel(price=99.75, notional_usd=5000.0),   # exact 25.0 bps
            QuoteLevel(price=100.25, notional_usd=5000.0),  # exact 25.0 bps
        )
        self.assertEqual(dm_edge, 1.0)
        self.assertFalse(zero_edge)

        dm_oob, zero_oob = compute_snapshot_dm(
            t3,
            mid,
            QuoteLevel(price=99.749, notional_usd=5000.0),  # 25.1 bps > 25 bps
            QuoteLevel(price=100.10, notional_usd=5000.0),
        )
        self.assertEqual(dm_oob, 0.25)
        self.assertTrue(zero_oob)

        # Tier 3: under min_side_notional_usd ($4,999 < $5,000) -> 0.25 floor
        dm_under, zero_under = compute_snapshot_dm(
            t3,
            mid,
            QuoteLevel(price=99.90, notional_usd=4999.0),
            QuoteLevel(price=100.10, notional_usd=5000.0),
        )
        self.assertEqual(dm_under, 0.25)
        self.assertFalse(zero_under)

        # Tier 4: $10k per side within 20 bps -> DM = 1.0
        dm4, zero4 = compute_snapshot_dm(
            t4,
            mid,
            QuoteLevel(price=99.82, notional_usd=10000.0),  # 18 bps
            QuoteLevel(price=100.18, notional_usd=10000.0), # 18 bps
        )
        self.assertEqual(dm4, 1.0)
        self.assertFalse(zero4)

        # Tier 4: 20.5 bps fails 20 bps bandwidth gate -> 0.25 floor
        dm4_oob, zero4_oob = compute_snapshot_dm(
            t4,
            mid,
            QuoteLevel(price=99.90, notional_usd=10000.0),
            QuoteLevel(price=100.205, notional_usd=10000.0),  # 20.5 bps > 20 bps
        )
        self.assertEqual(dm4_oob, 0.25)
        self.assertTrue(zero4_oob)

        # Tier 4: $9,000 < $10,000 -> 0.25 floor
        dm4_under, zero4_under = compute_snapshot_dm(
            t4,
            mid,
            QuoteLevel(price=99.90, notional_usd=9000.0),
            QuoteLevel(price=100.10, notional_usd=10000.0),
        )
        self.assertEqual(dm4_under, 0.25)
        self.assertFalse(zero4_under)

    def test_naive_cancel_collapses_dm_while_outer_band_guardians_preserve_dm(self):
        for tier in ("tier3_alt", "tier4_equity"):
            report = run_comparison(tier)
            naive = report["arms"]["naive_cancel_heavy_side"]
            guard = report["arms"]["outer_band_dm_guardian"]
            cascade = report["arms"]["outer_band_dm_guardian_cascade_guard"]

            self.assertGreater(naive["dm_metrics"]["zero_side_snapshots"], 0)
            self.assertLess(naive["dm_metrics"]["mean_dm"], 0.90)
            self.assertEqual(naive["dm_metrics"]["min_dm"], 0.25)

            self.assertGreaterEqual(guard["dm_metrics"]["mean_dm"], 0.90)
            self.assertEqual(guard["dm_metrics"]["zero_side_snapshots"], 0)
            self.assertEqual(guard["dm_metrics"]["min_dm"], 1.0)

            self.assertGreaterEqual(cascade["dm_metrics"]["mean_dm"], 0.90)
            self.assertEqual(cascade["dm_metrics"]["zero_side_snapshots"], 0)
            self.assertGreaterEqual(
                cascade["ex_subsidy_cash_pnl_usd"],
                guard["ex_subsidy_cash_pnl_usd"],
            )

    def test_causal_100ms_hedge_delay_anti_lookahead(self):
        cfg = SimConfig.for_tier("tier3_alt")
        # Frame 0: initial quote placement
        # Frame 1 (t=1000): sell sweep fills $5,000 bid -> triggers sell hedge eligible at t=1100
        # Frames 2..3 (t=1040, t=1099): BEFORE t+100 speed bump, no trade
        # Frame 4 (t=1100): eligible hedge executes at Frame 4's bid_px = 99.60
        base_frames = [
            Frame(1000, 99.98, 100.02, 100.00, "none", 0.0),
            Frame(1100, 99.85, 99.89, 99.87, "sell", 6000.0),
            Frame(1140, 99.50, 99.54, 99.52, "none", 0.0),
            Frame(1199, 100.40, 100.44, 100.42, "none", 0.0),
            Frame(1200, 99.60, 99.64, 99.62, "none", 0.0),
        ]

        # Perturb prices strictly BEFORE submit_ts + 100 (frames at t=1140 and t=1199)
        perturbed_before = list(base_frames)
        perturbed_before[2] = Frame(1140, 92.00, 92.04, 92.02, "none", 0.0)
        perturbed_before[3] = Frame(1199, 109.00, 109.04, 109.02, "none", 0.0)

        res_base = simulate_arm("outer_band_dm_guardian", base_frames, cfg)
        res_pert = simulate_arm("outer_band_dm_guardian", perturbed_before, cfg)

        # Anti-lookahead invariant: pre-100ms price changes cannot affect hedge fill price or PnL
        self.assertEqual(res_base["hedge_fills"], 1)
        self.assertEqual(res_pert["hedge_fills"], 1)
        self.assertEqual(res_base["hedge_volume_usd"], res_pert["hedge_volume_usd"])
        self.assertEqual(res_base["ex_subsidy_cash_pnl_usd"], res_pert["ex_subsidy_cash_pnl_usd"])

        # Changing the frame AT t >= submit_ts + 100 DOES change hedge fill price & PnL
        perturbed_at_100ms = list(base_frames)
        perturbed_at_100ms[4] = Frame(1200, 98.60, 98.64, 98.62, "none", 0.0)
        res_after = simulate_arm("outer_band_dm_guardian", perturbed_at_100ms, cfg)
        self.assertLess(res_after["ex_subsidy_cash_pnl_usd"], res_base["ex_subsidy_cash_pnl_usd"])

        # If tape ends BEFORE t + 100 (last frame at t=1199), terminal unwind executes on final frame
        truncated = base_frames[:4]
        res_trunc = simulate_arm("outer_band_dm_guardian", truncated, cfg)
        self.assertGreater(res_trunc["terminal_unwind_volume_usd"], 0.0)
        self.assertEqual(res_trunc["hedge_fills"], 1)

    def test_cascade_guard_triggers_circuit_breaker_on_severe_drawdown_or_inventory(self):
        # Severe waterfall crash: -25 bps per 30 ms with $12k sell prints
        frames = []
        ts = 1_795_200_000_000
        mid = 100.0
        for _ in range(40):
            ts += 30
            mid *= 1.0 - 25.0 / 10_000.0
            frames.append(
                Frame(
                    timestamp_ms=ts,
                    bid_px=round(mid * 0.9998, 6),
                    ask_px=round(mid * 1.0002, 6),
                    mid_px=round(mid, 6),
                    trade_side="sell",
                    trade_notional_usd=12_000.0,
                )
            )
        report = run_comparison("tier3_alt", frames=frames)
        guard = report["arms"]["outer_band_dm_guardian"]
        cascade = report["arms"]["outer_band_dm_guardian_cascade_guard"]

        self.assertFalse(guard["circuit_breaker_triggered"])
        self.assertTrue(cascade["circuit_breaker_triggered"])
        self.assertIn(
            cascade["circuit_breaker_reason"],
            ("max_drawdown_breached", "max_inventory_breached"),
        )
        self.assertIsNotNone(cascade["circuit_breaker_ts_ms"])
        self.assertLess(cascade["max_drawdown_usd"], guard["max_drawdown_usd"])
        self.assertGreater(cascade["ex_subsidy_cash_pnl_usd"], guard["ex_subsidy_cash_pnl_usd"])

        # Explicitly verify max_drawdown_breached trigger path with large max_inventory_usd
        cfg_dd = SimConfig.for_tier("tier3_alt", max_inventory_usd=500_000.0, max_loss_usd=50.0)
        res_dd = simulate_arm("outer_band_dm_guardian_cascade_guard", frames, cfg_dd)
        self.assertTrue(res_dd["circuit_breaker_triggered"])
        self.assertEqual(res_dd["circuit_breaker_reason"], "max_drawdown_breached")

    def test_subsidy_free_accounting_and_breakeven_pool_share(self):
        report = run_comparison("tier3_alt")
        self.assertEqual(report["subsidy_usd_assumed"], 0.0)
        for arm in report["arms"].values():
            self.assertEqual(arm["subsidy_usd_assumed"], 0.0)
            self.assertIn("maker_volume_usd", arm)
            self.assertIn("hedge_volume_usd", arm)
            self.assertIn("breakeven_monthly_pool_share_for_250k_pool", arm)
            self.assertGreaterEqual(arm["breakeven_monthly_pool_share_for_250k_pool"], 0.0)

        # Losing 1-hour replay should yield positive breakeven share proportional to $250k pool
        cfg = SimConfig.for_tier("tier3_alt", hedge_fee_bps=10.0)
        frames = [
            Frame(0, 99.98, 100.02, 100.00, "none", 0.0),
            Frame(1_800_000, 99.50, 99.54, 99.52, "sell", 6000.0),
            Frame(3_600_000, 99.00, 99.04, 99.02, "none", 0.0),
        ]
        losing_arm = simulate_arm("outer_band_dm_guardian", frames, cfg)
        self.assertLess(losing_arm["ex_subsidy_cash_pnl_usd"], 0.0)
        expected_share = round(
            (-losing_arm["ex_subsidy_cash_pnl_usd"] * 720.0) / 250_000.0,
            8,
        )
        self.assertAlmostEqual(
            losing_arm["breakeven_monthly_pool_share_for_250k_pool"],
            expected_share,
            places=6,
        )

    def test_fail_closed_validation_rejects_malformed_frames_and_tapes(self):
        valid_base = {
            "timestamp_ms": 1000,
            "bid_px": 99.98,
            "ask_px": 100.02,
            "mid_px": 100.00,
            "trade_side": "buy",
            "trade_notional_usd": 1000.0,
        }
        bad_cases = [
            # NaN and Inf
            [dict(valid_base, bid_px=float("nan"))],
            [dict(valid_base, ask_px=float("inf"))],
            [dict(valid_base, mid_px=float("-inf"))],
            [dict(valid_base, trade_notional_usd=float("nan"))],
            # Crossed, locked, or non-positive book / mid
            [dict(valid_base, bid_px=0.0)],
            [dict(valid_base, bid_px=-1.0)],
            [dict(valid_base, bid_px=100.05, ask_px=100.00)],  # crossed
            [dict(valid_base, bid_px=100.00, ask_px=100.00)],  # locked
            [dict(valid_base, mid_px=0.0)],
            [dict(valid_base, mid_px=-5.0)],
            [dict(valid_base, bid_px=99.50, ask_px=99.54, mid_px=90.0)],
            [dict(valid_base, bid_px=99.50, ask_px=99.54, mid_px=100.0)],
            # Non-monotonic timestamps (backwards and duplicate)
            [dict(valid_base, timestamp_ms=1000), dict(valid_base, timestamp_ms=999)],
            [dict(valid_base, timestamp_ms=1000), dict(valid_base, timestamp_ms=1000)],
            # Invalid trade_side or negative trade_notional_usd
            [dict(valid_base, trade_side="bid")],
            [dict(valid_base, trade_side="none", trade_notional_usd=100.0)],
            [dict(valid_base, trade_notional_usd=-1.0)],
            # Boolean masquerading as int/float
            [dict(valid_base, timestamp_ms=True)],
        ]
        for bad in bad_cases:
            with self.subTest(bad=bad):
                with self.assertRaises(ValueError):
                    validate_frames(bad)

        with self.assertRaises(ValueError):
            validate_frames([])

        # Reject non-finite JSON constants via JSONL loader
        with tempfile.TemporaryDirectory() as td:
            tape = Path(td) / "bad.jsonl"
            tape.write_text(json.dumps(valid_base).replace("99.98", "NaN") + "\n", encoding="utf-8")
            with self.assertRaises(ValueError):
                load_tape_jsonl(tape)

    def test_cli_self_test_and_json_output(self):
        self_test_proc = subprocess.run(
            [sys.executable, str(SCRIPT_PATH), "--self-test"],
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(self_test_proc.returncode, 0, self_test_proc.stderr)

        for tier in ("tier3_alt", "tier4_equity"):
            proc = subprocess.run(
                [sys.executable, str(SCRIPT_PATH), "--tier", tier, "--json"],
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(proc.returncode, 0, proc.stderr)
            payload = json.loads(proc.stdout)
            self.assertEqual(payload["schema"], "backpack_dm_guardian_sim.v1")
            self.assertEqual(payload["tier"], tier)
            self.assertEqual(payload["subsidy_usd_assumed"], 0.0)
            self.assertEqual(
                set(payload["arms"].keys()),
                {
                    "naive_cancel_heavy_side",
                    "outer_band_dm_guardian",
                    "outer_band_dm_guardian_cascade_guard",
                },
            )
            for arm in payload["arms"].values():
                self.assertEqual(arm["subsidy_usd_assumed"], 0.0)
                for key in (
                    "ex_subsidy_cash_pnl_usd",
                    "maker_volume_usd",
                    "hedge_volume_usd",
                    "dm_metrics",
                    "max_drawdown_usd",
                    "circuit_breaker_triggered",
                    "breakeven_monthly_pool_share_for_250k_pool",
                ):
                    self.assertIn(key, arm)
                for dm_key in (
                    "mean_dm",
                    "min_dm",
                    "pct_snapshots_dm_ge_090",
                    "zero_side_snapshots",
                ):
                    self.assertIn(dm_key, arm["dm_metrics"])

        # CLI --tape roundtrip & fail-closed exit code 1 on invalid tape
        with tempfile.TemporaryDirectory() as td:
            good_tape = Path(td) / "good.jsonl"
            good_tape.write_text(
                "\n".join(
                    [
                        json.dumps({"timestamp_ms": 1000, "bid_px": 99.98, "ask_px": 100.02, "mid_px": 100.0, "trade_side": "none", "trade_notional_usd": 0.0}),
                        json.dumps({"timestamp_ms": 1100, "bid_px": 99.96, "ask_px": 100.00, "mid_px": 99.98, "trade_side": "sell", "trade_notional_usd": 3000.0}),
                        json.dumps({"timestamp_ms": 1200, "bid_px": 99.98, "ask_px": 100.02, "mid_px": 100.0, "trade_side": "buy", "trade_notional_usd": 3000.0}),
                    ]
                )
                + "\n",
                encoding="utf-8",
            )
            ok_proc = subprocess.run(
                [sys.executable, str(SCRIPT_PATH), "--tier", "tier3_alt", "--tape", str(good_tape), "--json"],
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(ok_proc.returncode, 0, ok_proc.stderr)
            ok_json = json.loads(ok_proc.stdout)
            self.assertEqual(ok_json["frames"], 3)

            bad_tape = Path(td) / "bad.jsonl"
            bad_tape.write_text(
                json.dumps({"timestamp_ms": 1000, "bid_px": 101.0, "ask_px": 100.0, "mid_px": 100.5, "trade_side": "none", "trade_notional_usd": 0.0}) + "\n",
                encoding="utf-8",
            )
            bad_proc = subprocess.run(
                [sys.executable, str(SCRIPT_PATH), "--tape", str(bad_tape), "--json"],
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertNotEqual(bad_proc.returncode, 0)

    def test_sim_config_and_compute_snapshot_dm_reject_bool_and_invalid_slippage_bounds(self):
        with self.assertRaises(ValueError):
            SimConfig.for_tier("tier3_alt", cascade_window_ms=True)
        with self.assertRaises(ValueError):
            SimConfig.for_tier("tier3_alt", maker_fee_bps=True)
        with self.assertRaises(ValueError):
            SimConfig.for_tier("tier3_alt", maker_fee_bps=10_000.0)
        with self.assertRaises(ValueError):
            SimConfig.for_tier("tier3_alt", hedge_fee_bps=10_000.0)
        with self.assertRaises(ValueError):
            SimConfig.for_tier("tier3_alt", hedge_slippage_bps=10_000.0)
        with self.assertRaises(ValueError):
            SimConfig.for_tier("tier3_alt", inner_band_bps=float("nan"))

        t3 = TIER_PRESETS["tier3_alt"]
        with self.assertRaises(ValueError):
            compute_snapshot_dm(
                t3,
                True,  # type: ignore[arg-type]
                QuoteLevel(price=99.9, notional_usd=5000.0),
                QuoteLevel(price=100.1, notional_usd=5000.0),
            )
        dm_bool_q, zero_bool_q = compute_snapshot_dm(
            t3,
            100.0,
            QuoteLevel(price=True, notional_usd=5000.0),  # type: ignore[arg-type]
            QuoteLevel(price=100.1, notional_usd=True),  # type: ignore[arg-type]
        )
        self.assertEqual(dm_bool_q, 0.25)
        self.assertTrue(zero_bool_q)

    def test_simulate_arm_direct_call_validates_config_and_frames_fail_closed(self):
        cfg_bad_slip = SimConfig(
            preset=TIER_PRESETS["tier3_alt"],
            inner_band_bps=8.0,
            soft_limit_usd=3000.0,
            hedge_trigger_usd=4500.0,
            max_inventory_usd=15_000.0,
            max_loss_usd=150.0,
            hedge_slippage_bps=10_000.0,
        )
        good_frames = [
            Frame(1000, 99.98, 100.02, 100.0, "none", 0.0),
            Frame(1100, 99.98, 100.02, 100.0, "none", 0.0),
        ]
        with self.assertRaises(ValueError):
            simulate_arm("outer_band_dm_guardian", good_frames, cfg_bad_slip)

        cfg_ok = SimConfig.for_tier("tier3_alt")
        for bad_frames in (
            [],
            [
                Frame(1100, 99.98, 100.02, 100.0, "none", 0.0),
                Frame(1000, 99.98, 100.02, 100.0, "none", 0.0),
            ],
            [Frame(1000, 100.05, 100.00, 100.02, "none", 0.0)],
        ):
            with self.subTest(bad_frames=bad_frames):
                with self.assertRaises(ValueError):
                    simulate_arm("outer_band_dm_guardian", bad_frames, cfg_ok)

    def test_load_tape_rejects_directory_and_cli_rejects_self_test_with_tape(self):
        with tempfile.TemporaryDirectory() as td:
            with self.assertRaises(FileNotFoundError):
                load_tape_jsonl(Path(td))
            tape = Path(td) / "good.jsonl"
            tape.write_text(
                json.dumps(
                    {
                        "timestamp_ms": 1000,
                        "bid_px": 99.98,
                        "ask_px": 100.02,
                        "mid_px": 100.0,
                        "trade_side": "none",
                        "trade_notional_usd": 0.0,
                    }
                )
                + "\n",
                encoding="utf-8",
            )
            proc = subprocess.run(
                [sys.executable, str(SCRIPT_PATH), "--self-test", "--tape", str(tape)],
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(proc.returncode, 2)

    def test_validate_frame_binds_mid_px_to_same_book_bbo(self):
        with self.assertRaises(ValueError):
            validate_frames([Frame(1000, 99.50, 99.54, 90.0, "none", 0.0)])
        with self.assertRaises(ValueError):
            validate_frames([Frame(1000, 99.50, 99.54, 100.0, "none", 0.0)])

        cfg = SimConfig.for_tier("tier3_alt")
        for mid_bad in (90.0, 100.0):
            with self.assertRaises(ValueError):
                simulate_arm(
                    "outer_band_dm_guardian",
                    [Frame(1000, 99.50, 99.54, mid_bad, "none", 0.0)],
                    cfg,
                )

        for mid_ok in (99.50, 99.51, 99.54):
            validated = validate_frames([Frame(1000, 99.50, 99.54, mid_ok, "none", 0.0)])
            self.assertEqual(validated[0].mid_px, mid_ok)


if __name__ == "__main__":
    unittest.main()
