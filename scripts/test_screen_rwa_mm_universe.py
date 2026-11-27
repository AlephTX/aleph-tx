#!/usr/bin/env python3
"""Stdlib-only unit tests for `scripts/screen_rwa_mm_universe.py`."""

from __future__ import annotations

from datetime import datetime, timezone
import json
import math
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import unittest

SCRIPT_DIR = Path(__file__).resolve().parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

import screen_rwa_mm_universe as scr  # noqa: E402


def _utc_ms(year: int, month: int, day: int, hour: int, minute: int = 0) -> int:
    return int(datetime(year, month, day, hour, minute, 0, tzinfo=timezone.utc).timestamp() * 1000)


class ScreenRwaMmUniverseTests(unittest.TestCase):
    def test_pure_stdlib_top_level_and_canonical_campaigns(self) -> None:
        probe = subprocess.run(
            [
                sys.executable,
                "-c",
                (
                    f"import sys; sys.path.insert(0, {str(SCRIPT_DIR)!r}); "
                    "import screen_rwa_mm_universe; "
                    "mods = [m for m in ('duckdb', 'numpy', 'pandas', 'pyarrow', 'scipy') if m in sys.modules]; "
                    "assert not mods, f'unexpected third-party imports: {mods}'"
                ),
            ],
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertEqual(probe.returncode, 0, probe.stderr or probe.stdout)
        for win in ("w18", "w19", "w20", "w21", "w22", "w23", "w24", "w25"):
            self.assertIn(win, scr.CANONICAL_CAMPAIGNS)
            self.assertIn("rwa", scr.CANONICAL_CAMPAIGNS[win])
            self.assertIn("crypto", scr.CANONICAL_CAMPAIGNS[win])
        self.assertEqual(scr.VENUE_PROFILES["arcus"]["settlement"], "USDC")
        self.assertEqual(scr.VENUE_PROFILES["lighter_robinhood"]["settlement"], "USDG")

    def test_classify_utc_session_boundaries(self) -> None:
        self.assertEqual(scr.classify_utc_session(_utc_ms(2026, 11, 25, 0, 0)), "overnight_asia")
        self.assertEqual(scr.classify_utc_session(_utc_ms(2026, 11, 25, 10, 59)), "overnight_asia")
        self.assertEqual(scr.classify_utc_session(_utc_ms(2026, 11, 25, 11, 0)), "us_extended")
        self.assertEqual(scr.classify_utc_session(_utc_ms(2026, 11, 25, 13, 29)), "us_extended")
        self.assertEqual(scr.classify_utc_session(_utc_ms(2026, 11, 25, 13, 30)), "us_rth")
        self.assertEqual(scr.classify_utc_session(_utc_ms(2026, 11, 25, 19, 59)), "us_rth")
        self.assertEqual(scr.classify_utc_session(_utc_ms(2026, 11, 25, 20, 0)), "us_extended")
        self.assertEqual(scr.classify_utc_session(_utc_ms(2026, 11, 25, 23, 59)), "us_extended")

    def test_half_spread_to_1s_move_ratio_and_session_metrics(self) -> None:
        # Constant alternating +/- 0.84 bps 1s step with 2.0 bps spread and 0 fee:
        # half_spread = 1.0 bps, median_1s_abs_move_bps = 0.84 -> ratio ~ 1.1905.
        t0 = _utc_ms(2026, 11, 25, 3, 0)
        books = scr._make_synthetic_bbo_stream(
            t0, 60, step_ms=1000, spread_bps=2.0, step_move_bps=0.84, l1_usd=1000.0
        )
        trades = [t0 + i * 2000 for i in range(30)]
        metrics = scr.compute_session_microstructure_metrics(
            books, trade_timestamps_ms=trades, maker_fee_bps=0.0, taker_fee_bps=1.15
        )
        asia = metrics["overnight_asia"]
        self.assertFalse(asia["insufficient_samples"])
        self.assertAlmostEqual(asia["half_spread_minus_fee_bps"], 1.0, places=2)
        self.assertAlmostEqual(asia["median_1s_abs_move_bps"], 0.84, places=2)
        self.assertAlmostEqual(asia["half_spread_to_1s_move_ratio"], 1.0 / 0.84, places=2)
        self.assertGreater(asia["trades_per_hour"], 500.0)
        self.assertEqual(metrics["us_rth"]["samples"], 0)
        self.assertTrue(metrics["us_rth"]["insufficient_samples"])

    def test_continuity_10h_flag_in_session_metrics(self) -> None:
        t0 = _utc_ms(2026, 11, 25, 0, 0)
        # 10h + 30s at 30s spacing -> continuous 10h
        books = scr._make_synthetic_bbo_stream(
            t0, 1202, step_ms=30_000, spread_bps=2.0, step_move_bps=0.5, l1_usd=800.0
        )
        res = scr.compute_slice_microstructure_metrics(books, maker_fee_bps=0.0)
        self.assertTrue(res["is_continuous_10h"])
        self.assertGreaterEqual(res["duration_ms"], 36_000_000)

    def test_stream_whitelist_admission_and_rejection_reasons(self) -> None:
        t_asia = _utc_ms(2026, 11, 25, 2, 0)
        t_rth = _utc_ms(2026, 11, 25, 14, 0)
        good_asia = scr._make_synthetic_bbo_stream(
            t_asia, 60, step_ms=1000, spread_bps=3.0, step_move_bps=0.6, l1_usd=1800.0
        )
        high_vol_rth = scr._make_synthetic_bbo_stream(
            t_rth, 60, step_ms=1000, spread_bps=3.0, step_move_bps=4.0, l1_usd=1800.0
        )
        thin_l1 = scr._make_synthetic_bbo_stream(
            t_asia, 60, step_ms=1000, spread_bps=0.3, step_move_bps=0.6, l1_usd=100.0
        )
        trades_asia = [t_asia + i * 1000 for i in range(60)]
        trades_rth = [t_rth + i * 1000 for i in range(60)]

        streams = [
            # Passes all gates across two windows; asia better than rth
            {
                "venue": "arcus",
                "symbol": "DRAM",
                "window": "w18",
                "books": good_asia + high_vol_rth[:20],
                "trade_timestamps_ms": trades_asia + trades_rth[:5],
            },
            {
                "venue": "arcus",
                "symbol": "DRAM",
                "window": "w19",
                "books": good_asia,
                "trade_timestamps_ms": trades_asia,
            },
            # Rejected: nonzero maker fee (trade_xyz default 0.30 bps)
            {
                "venue": "trade_xyz",
                "symbol": "NVDA",
                "window": "w18",
                "books": good_asia,
                "trade_timestamps_ms": trades_asia,
            },
            # Rejected: high vol + low spread_to_vol
            {
                "venue": "arcus",
                "symbol": "SNDK",
                "window": "w18",
                "books": high_vol_rth,
                "trade_timestamps_ms": trades_rth,
            },
            # Rejected: tight spread, low L1 depth, low trades/hr
            {
                "venue": "lighter_robinhood",
                "symbol": "SPY",
                "window": "w18",
                "books": thin_l1,
                "trade_timestamps_ms": [],
            },
        ]
        out = scr.screen_streams(streams, gates=scr.WhitelistGates())
        self.assertEqual(out["schema"], "rwa_mm_whitelist_v1")
        self.assertTrue(out["session_breakdown_available"])
        self.assertEqual([(r["venue"], r["symbol"]) for r in out["whitelist"]], [("arcus", "DRAM")])
        dram = out["whitelist"][0]
        self.assertEqual(dram["settlement"], "USDC")
        self.assertEqual(dram["preferred_sessions"][0], "overnight_asia")
        self.assertGreater(dram["whitelist_score"], 0.0)

        excl = {(r["venue"], r["symbol"]): r["reject_reasons"] for r in out["excluded"]}
        self.assertTrue(any(r.startswith("nonzero_maker_fee:") for r in excl[("trade_xyz", "NVDA")]))
        self.assertTrue(any(r.startswith("vol_above_max:") for r in excl[("arcus", "SNDK")]))
        self.assertTrue(any(r.startswith("half_spread_below_min:") for r in excl[("lighter_robinhood", "SPY")]))
        self.assertTrue(any(r.startswith("l1_depth_below_min:") for r in excl[("lighter_robinhood", "SPY")]))
        self.assertTrue(
            any(r.startswith("trades_per_hour_below_min:") for r in excl[("lighter_robinhood", "SPY")])
        )
        self.assertTrue(any(r["session"] == "overnight_asia" for r in out["session_matrix"]))

        # Allowing nonzero maker fee admits trade_xyz NVDA when microstructure passes
        relaxed = scr.screen_streams(streams, gates=scr.WhitelistGates(require_zero_maker_fee=False))
        self.assertIn(("trade_xyz", "NVDA"), [(r["venue"], r["symbol"]) for r in relaxed["whitelist"]])

    def test_multivenue_mode_aggregation_and_cli(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            root = Path(td)
            w19 = root / "mv_w19.json"
            w24 = root / "mv_w24.json"
            w19.write_text(
                json.dumps(
                    {
                        "split": "selection_dev",
                        "track1_quant_profit": [
                            {
                                "venue": "arcus",
                                "symbol": "DRAM",
                                "domain": "rwa",
                                "maker_fee_bps": 0.0,
                                "half_spread_minus_fee_bps": 1.38,
                                "spread_to_1s_vol_ratio": 1.085,
                                "median_l1_min_usd": 1628.3,
                                "realized_1s_vol_bps": 2.4,
                            },
                            {
                                "venue": "lighter_robinhood",
                                "symbol": "NVDA",
                                "domain": "rwa",
                                "maker_fee_bps": 0.0,
                                "half_spread_minus_fee_bps": 0.747,
                                "spread_to_1s_vol_ratio": 0.6,
                                "median_l1_min_usd": 402.9,
                                "realized_1s_vol_bps": 2.4,
                            },
                            {
                                "venue": "trade_xyz",
                                "symbol": "SNDK",
                                "domain": "rwa",
                                "maker_fee_bps": 0.3,
                                "half_spread_minus_fee_bps": 0.5,
                                "spread_to_1s_vol_ratio": 0.8,
                                "median_l1_min_usd": 500.0,
                                "realized_1s_vol_bps": 1.5,
                            },
                            {
                                "venue": "arcus",
                                "symbol": "BTC",
                                "domain": "crypto",
                                "maker_fee_bps": 0.0,
                                "half_spread_minus_fee_bps": 0.9,
                                "spread_to_1s_vol_ratio": 1.2,
                                "median_l1_min_usd": 2000.0,
                                "realized_1s_vol_bps": 1.5,
                            },
                        ],
                    }
                ),
                encoding="utf-8",
            )
            w24.write_text(
                json.dumps(
                    {
                        "split": "holdout",
                        "track1_quant_profit": [
                            {
                                "venue": "arcus",
                                "symbol": "DRAM",
                                "domain": "rwa",
                                "maker_fee_bps": 0.0,
                                "half_spread_minus_fee_bps": 1.49,
                                "spread_to_1s_vol_ratio": 1.175,
                                "median_l1_min_usd": 3958.2,
                                "realized_1s_vol_bps": 2.0,
                            },
                            {
                                "venue": "lighter_robinhood",
                                "symbol": "NVDA",
                                "domain": "rwa",
                                "maker_fee_bps": 0.0,
                                "half_spread_minus_fee_bps": 1.019,
                                "spread_to_1s_vol_ratio": 0.945,
                                "median_l1_min_usd": 750.0,
                                "realized_1s_vol_bps": 2.159,
                            },
                        ],
                    }
                ),
                encoding="utf-8",
            )

            out_path = root / "whitelist.json"
            rc = scr.main(["--multivenue", str(w19), str(w24), "--output", str(out_path)])
            self.assertEqual(rc, 0)
            payload = json.loads(out_path.read_text(encoding="utf-8"))

            self.assertEqual(payload["schema"], "rwa_mm_whitelist_v1")
            self.assertFalse(payload["session_breakdown_available"])
            self.assertEqual(payload["gates"]["trades_check_note"], "trades_check_skipped_multivenue_mode")
            # Crypto BTC excluded by default in RWA screener
            wl_pairs = [(r["venue"], r["symbol"]) for r in payload["whitelist"]]
            self.assertEqual(wl_pairs, [("arcus", "DRAM"), ("lighter_robinhood", "NVDA")])

            dram = payload["whitelist"][0]
            self.assertAlmostEqual(dram["worst_half_spread_minus_fee_bps"], 1.38, places=4)
            self.assertAlmostEqual(dram["min_l1_usd"], 1628.3, places=2)
            self.assertAlmostEqual(dram["mean_spread_to_vol_ratio"], 1.13, places=3)
            self.assertAlmostEqual(dram["whitelist_score"], 1.38 * 1.13, places=3)

            nvda_lr = payload["whitelist"][1]
            self.assertEqual(nvda_lr["settlement"], "USDG")

            excl = {(r["venue"], r["symbol"]): r["reject_reasons"] for r in payload["excluded"]}
            self.assertIn(("trade_xyz", "SNDK"), excl)
            self.assertTrue(any("nonzero_maker_fee" in x for x in excl[("trade_xyz", "SNDK")]))

    def test_self_test_cli_flag(self) -> None:
        proc = subprocess.run(
            [sys.executable, str(SCRIPT_DIR / "screen_rwa_mm_universe.py"), "--self-test"],
            capture_output=True,
            text=True,
            check=True,
        )
        parsed = json.loads(proc.stdout)
        self.assertEqual(parsed["self_test"], "ok")
        self.assertTrue(math.isclose(parsed["asia_half_spread_to_1s_move"], 3.0, abs_tol=0.1))

    def test_opt01_defaults_and_duration_weighted_session_aggregation(self) -> None:
        venues_rs = (SCRIPT_DIR.parent / "src" / "venues.rs").read_text(encoding="utf-8")
        m = re.search(r'VENUE_CAPABILITY_SNAPSHOT_VERSION[^"]*"([^"]+)"', venues_rs)
        self.assertIsNotNone(m)
        self.assertEqual(scr.VENUE_CAPABILITY_SNAPSHOT_VERSION, m.group(1))

        self.assertEqual(scr.VENUE_PROFILES["arcus"]["fee_audit_status"], "public_tier0_schedule_only")
        self.assertEqual(scr.VENUE_PROFILES["arcus"]["maker_fee_bps_crypto"], 1.50)
        self.assertEqual(scr.VENUE_PROFILES["arcus"]["taker_fee_bps_crypto"], 4.50)
        self.assertEqual(scr.VENUE_PROFILES["arcus"]["taker_fee_bps_equity"], 2.25)
        self.assertEqual(scr.VENUE_PROFILES["lighter"]["fee_audit_status"], "public_tier0_unverified")
        self.assertEqual(scr.VENUE_PROFILES["lighter_robinhood"]["fee_audit_status"], "public_tier0_unverified")

        # Short thin outlier window (0.1h, ratio=25.0) must not dominate a 6.0h window (ratio=1.3125).
        sess = scr._aggregate_session_across_windows(
            [
                {
                    "insufficient_samples": False,
                    "hours": 0.1,
                    "samples": 30,
                    "median_spread_bps": 1.0,
                    "realized_1s_vol_bps": 0.04,
                    "spread_to_vol_ratio": 25.0,
                    "half_spread_minus_fee_bps": 0.5,
                    "median_l1_min_usd": 1000.0,
                },
                {
                    "insufficient_samples": False,
                    "hours": 6.0,
                    "samples": 1800,
                    "median_spread_bps": 0.84,
                    "realized_1s_vol_bps": 0.64,
                    "spread_to_vol_ratio": 1.3125,
                    "half_spread_minus_fee_bps": 0.42,
                    "median_l1_min_usd": 1000.0,
                },
            ],
            "us_rth",
        )
        expected_w_stv = round((30 * 25.0 + 1800 * 1.3125) / 1830, 4)
        expected_pooled = round((30 * 1.0 + 1800 * 0.84) / (30 * 0.04 + 1800 * 0.64), 4)
        self.assertLess(sess["mean_spread_to_vol_ratio"], 2.0)
        self.assertAlmostEqual(sess["mean_spread_to_vol_ratio"], expected_w_stv, places=4)
        self.assertAlmostEqual(sess["pooled_spread_to_vol_ratio"], expected_pooled, places=4)

        # Synthetic zero-duration records emit snapshot version and evaluate cleanly.
        res = scr.evaluate_candidate_records(
            {
                ("lighter", "AAPL"): [
                    {
                        "window": "w23",
                        "raw_market": "113",
                        "settlement": "USDC",
                        "fee_audit_status": "public_tier0_unverified",
                        "maker_fee_bps": 0.0,
                        "taker_fee_bps": 0.0,
                        "all_summary": {
                            "insufficient_samples": False,
                            "half_spread_minus_fee_bps": 0.8,
                            "spread_to_vol_ratio": 1.1,
                            "median_l1_min_usd": 1200.0,
                            "realized_1s_vol_bps": 1.0,
                        },
                    }
                ]
            },
            gates=scr.WhitelistGates(),
            source_mode="multivenue_json",
            session_breakdown_available=False,
        )
        self.assertEqual(res["venue_capability_snapshot_version"], m.group(1))
        self.assertEqual(res["whitelist_count"], 1)

    def test_fee_evidence_delegates_to_track_fee_evidence_and_rejects_unverified_overrides(self) -> None:
        bad_cases = [
            ("arcus", "NVDA", "NVDA-USD", "rwa", "public_tier0_schedule_only"),
            ("trade_xyz", "NVDA", "xyz:NVDA", "rwa", "public_schedule_only_unverified_account_tier"),
            ("extended", "BTC", "BTC-USD", "crypto", "unknown_requires_account_evidence"),
        ]
        with tempfile.TemporaryDirectory() as td:
            root = Path(td)
            for venue, symbol, raw_mkt, domain, bad_status in bad_cases:
                bad_payload = {
                    "schema_version": "aleph_account_fee_evidence_v1",
                    "venues": {
                        venue: {
                            "account_fee_status": bad_status,
                            "maker_fee_bps": 0.0,
                            "taker_fee_bps": 0.0,
                        }
                    },
                }
                bad_path = root / f"{venue}_bad.json"
                bad_path.write_text(json.dumps(bad_payload), encoding="utf-8")
                with self.assertRaisesRegex(ValueError, "verified_account_tier"):
                    scr.load_fee_evidence_overrides(bad_path)
                with self.assertRaisesRegex(ValueError, "verified_account_tier"):
                    scr.screen_streams(
                        [
                            {
                                "venue": venue,
                                "symbol": symbol,
                                "raw_market": raw_mkt,
                                "domain": domain,
                                "books": scr._make_synthetic_bbo_stream(
                                    1_795_000_000_000, 12, spread_bps=3.0, step_move_bps=0.4, l1_usd=1000.0
                                ),
                            }
                        ],
                        gates=scr.WhitelistGates(),
                        fee_overrides={venue: bad_payload["venues"][venue]},
                    )

            verified_path = root / "good_verified.json"
            verified_path.write_text(
                json.dumps(
                    {
                        "schema_version": "aleph_account_fee_evidence_v1",
                        "venues": {
                            "arcus": {
                                "account_fee_status": "verified_account_tier",
                                "maker_fee_bps": 0.25,
                                "taker_fee_bps": 1.25,
                            }
                        },
                    }
                ),
                encoding="utf-8",
            )
            loaded = scr.load_fee_evidence_overrides(verified_path)
            meta = scr._resolve_venue_metadata("arcus", "NVDA", "NVDA-USD", "rwa", loaded)
            self.assertEqual((meta["maker_fee_bps"], meta["taker_fee_bps"], meta["fee_audit_status"]), (0.25, 1.25, "verified_account_tier"))

            ev_path = root / "real_like_ev.json"
            ev_path.write_text(
                json.dumps(
                    {
                        "schema_version": "aleph_account_fee_evidence_v1",
                        "venues": {
                            "arcus": {
                                "account_fee_status": "public_tier0_schedule_only",
                                "maker_fee_bps": None,
                                "taker_fee_bps": None,
                                "public_tier0_schedule_summary": {
                                    "tier0_fee_profiles": [
                                        {"maker_fee_bps": "0", "taker_fee_bps": "2.25"},
                                        {"maker_fee_bps": "1.5", "taker_fee_bps": "4.5"},
                                    ],
                                    "custom_schedule_tier0_bps": {
                                        "ANT-USD": {"maker_fee_bps": "0", "taker_fee_bps": "1.12"},
                                        "NVDA-USD": {"maker_fee_bps": "0", "taker_fee_bps": "2.25"},
                                    },
                                },
                            },
                            "trade_xyz": {
                                "account_fee_status": "verified_account_tier",
                                "maker_fee_bps": None,
                                "taker_fee_bps": None,
                                "growth_mode_maker_fee_bps": "0.3",
                                "growth_mode_taker_fee_bps": "0.9",
                                "standard_maker_fee_bps": "3",
                                "standard_taker_fee_bps": "9",
                            },
                        },
                    }
                ),
                encoding="utf-8",
            )
            mv_path = root / "mv.json"
            mv_path.write_text(
                json.dumps(
                    {
                        "split": "holdout_w25",
                        "track1_quant_profit": [
                            {
                                "venue": "arcus",
                                "symbol": "ANT",
                                "raw_market": "ANT-USD",
                                "domain": "rwa",
                                "maker_fee_bps": 0.0,
                                "median_spread_bps": 3.0,
                                "half_spread_minus_fee_bps": 1.5,
                                "spread_to_1s_vol_ratio": 1.2,
                                "median_l1_min_usd": 1200.0,
                                "realized_1s_vol_bps": 1.5,
                            },
                            {
                                "venue": "arcus",
                                "symbol": "NVDA",
                                "raw_market": "NVDA-USD",
                                "domain": "rwa",
                                "maker_fee_bps": 0.0,
                                "median_spread_bps": 3.0,
                                "half_spread_minus_fee_bps": 1.5,
                                "spread_to_1s_vol_ratio": 1.2,
                                "median_l1_min_usd": 1200.0,
                                "realized_1s_vol_bps": 1.5,
                            },
                            {
                                "venue": "trade_xyz",
                                "symbol": "NVDA",
                                "raw_market": "xyz:NVDA",
                                "domain": "rwa",
                                "maker_fee_bps": 0.3,
                                "median_spread_bps": 3.0,
                                "half_spread_minus_fee_bps": 1.2,
                                "spread_to_1s_vol_ratio": 1.2,
                                "median_l1_min_usd": 1200.0,
                                "realized_1s_vol_bps": 1.5,
                            },
                        ],
                    }
                ),
                encoding="utf-8",
            )
            out_path = root / "out_std.json"
            rc = scr.main(
                [
                    "--multivenue",
                    str(mv_path),
                    "--fee-evidence",
                    str(ev_path),
                    "--arcus-fee-tier",
                    "level_0",
                    "--trade-xyz-fee-mode",
                    "standard",
                    "--output",
                    str(out_path),
                ]
            )
            self.assertEqual(rc, 0)
            payload = json.loads(out_path.read_text(encoding="utf-8"))
            self.assertEqual(payload["venue_capability_snapshot_version"], scr.VENUE_CAPABILITY_SNAPSHOT_VERSION)
            self.assertEqual(payload["fee_provenance"]["arcus_fee_tier"], "level_0")
            self.assertEqual(payload["fee_provenance"]["trade_xyz_fee_mode"], "standard")
            by_pair = {
                (r["venue"], r["symbol"]): r
                for r in payload["whitelist"] + payload["excluded"]
            }
            self.assertEqual(by_pair[("arcus", "ANT")]["taker_fee_bps"], 1.12)
            self.assertEqual(by_pair[("arcus", "NVDA")]["taker_fee_bps"], 2.25)
            self.assertEqual(by_pair[("trade_xyz", "NVDA")]["maker_fee_bps"], 3.0)
            self.assertEqual(by_pair[("trade_xyz", "NVDA")]["taker_fee_bps"], 9.0)
            self.assertEqual(by_pair[("trade_xyz", "NVDA")]["fee_audit_status"], "verified_account_tier")
            self.assertAlmostEqual(by_pair[("trade_xyz", "NVDA")]["worst_half_spread_minus_fee_bps"], -1.5, places=4)


if __name__ == "__main__":
    unittest.main()
