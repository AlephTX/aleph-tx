#!/usr/bin/env python3
"""Unit tests for scripts/eval_multivenue_three_tracks.py."""

from __future__ import annotations

import json
import re
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

import eval_multivenue_three_tracks as ev  # noqa: E402
import track_fee_evidence as tfe  # noqa: E402


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

    def test_opt01_snapshot_defaults_and_fee_evidence_resolution(self) -> None:
        venues_rs = (SCRIPT_DIR.parent / "src" / "venues.rs").read_text(encoding="utf-8")
        m = re.search(r'VENUE_CAPABILITY_SNAPSHOT_VERSION[^"]*"([^"]+)"', venues_rs)
        self.assertIsNotNone(m)
        self.assertEqual(ev.VENUE_CAPABILITY_SNAPSHOT_VERSION, m.group(1))

        # Default profiles must stay conservative (no unverified Level 4 or audited_standard_zero).
        self.assertEqual(ev.VENUE_PROFILES["arcus"]["fee_audit_status"], "public_tier0_schedule_only")
        self.assertEqual(ev.VENUE_PROFILES["arcus"]["maker_fee_bps_crypto"], 1.50)
        self.assertEqual(ev.VENUE_PROFILES["arcus"]["taker_fee_bps_crypto"], 4.50)
        self.assertEqual(ev.VENUE_PROFILES["arcus"]["maker_fee_bps_equity"], 0.0)
        self.assertEqual(ev.VENUE_PROFILES["arcus"]["taker_fee_bps_equity"], 2.25)
        self.assertEqual(ev.VENUE_PROFILES["lighter"]["fee_audit_status"], "public_tier0_unverified")
        self.assertEqual(ev.VENUE_PROFILES["lighter_robinhood"]["fee_audit_status"], "public_tier0_unverified")

        ev_payload = {
            "schema_version": "aleph_account_fee_evidence_v1",
            "audited_at_utc": "2026-11-22T17:26:28+00:00",
            "venues": {
                "arcus": {
                    "account_fee_status": "public_tier0_schedule_only",
                    "maker_fee_bps": None,
                    "taker_fee_bps": None,
                    "public_tier0_schedule_summary": {
                        "tier0_fee_profiles": [
                            {"maker_fee_bps": "0", "taker_fee_bps": "1.12"},
                            {"maker_fee_bps": "0", "taker_fee_bps": "2.25"},
                            {"maker_fee_bps": "1.5", "taker_fee_bps": "4.5"},
                        ],
                        "custom_schedule_tier0_bps": {
                            "ANT-USD": {"maker_fee_bps": "0", "taker_fee_bps": "1.12"},
                            "NVDA-USD": {"maker_fee_bps": "0", "taker_fee_bps": "2.25"},
                        },
                    },
                },
                "lighter": {
                    "account_fee_status": "public_schedule_only_unverified_account_tier",
                    "maker_fee_bps": None,
                    "taker_fee_bps": None,
                    "public_schedule_maker_fee_bps": "0",
                    "public_schedule_taker_fee_bps": "0",
                },
                "extended": {
                    "account_fee_status": "unknown_requires_account_evidence",
                    "maker_fee_bps": None,
                    "taker_fee_bps": None,
                },
                "bulk": {
                    "account_fee_status": "unknown_requires_account_evidence",
                    "maker_fee_bps": None,
                    "taker_fee_bps": None,
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
        with tempfile.TemporaryDirectory() as tmp:
            ev_path = Path(tmp) / "fee_ev.json"
            ev_path.write_text(json.dumps(ev_payload), encoding="utf-8")

            prof, fee_ev, prov = ev.resolve_venue_profiles(
                ev_path, arcus_fee_tier="level_0", trade_xyz_fee_mode="growth_mode"
            )
            self.assertEqual(prov["venue_capability_snapshot_version"], m.group(1))
            self.assertEqual(prof["lighter"]["fee_audit_status"], "public_schedule_only_unverified_account_tier")
            self.assertEqual(prof["lighter_robinhood"]["fee_audit_status"], "public_tier0_unverified")
            self.assertEqual(prof["extended"]["fee_audit_status"], "unknown_requires_account_evidence")
            self.assertEqual(prof["bulk"]["fee_audit_status"], "unknown_requires_account_evidence")
            self.assertEqual(prof["trade_xyz"]["fee_audit_status"], "verified_account_tier")
            self.assertEqual(prof["trade_xyz"]["taker_fee_bps_equity"], 0.9)
            # Global profiles stay unmutated.
            self.assertEqual(ev.VENUE_PROFILES["lighter"]["fee_audit_status"], "public_tier0_unverified")

            mk, tk, st, src = ev.resolve_market_fees("arcus", "rwa", "ANT-USD", prof, fee_ev, arcus_fee_tier="level_0")
            self.assertEqual((mk, tk, st, src), (0.0, 1.12, "public_tier0_schedule_only", "evidence_public_tier0_custom_market"))
            mk2, tk2, _, _ = ev.resolve_market_fees("arcus", "rwa", "NVDA-USD", prof, fee_ev, arcus_fee_tier="level_0")
            self.assertEqual((mk2, tk2), (0.0, 2.25))

            prof_l4, fee_ev_l4, _ = ev.resolve_venue_profiles(
                ev_path, arcus_fee_tier="level_4", trade_xyz_fee_mode="standard"
            )
            mk_l4, tk_l4, st_l4, _ = ev.resolve_market_fees(
                "arcus", "rwa", "NVDA-USD", prof_l4, fee_ev_l4, arcus_fee_tier="level_4"
            )
            self.assertEqual((mk_l4, tk_l4, st_l4), (0.0, 1.15, "account_evidence_required"))
            mk_xyz, tk_xyz, st_xyz, _ = ev.resolve_market_fees(
                "trade_xyz", "rwa", "xyz:NVDA", prof_l4, fee_ev_l4, trade_xyz_fee_mode="standard"
            )
            self.assertEqual((mk_xyz, tk_xyz, st_xyz), (3.0, 9.0, "verified_account_tier"))

    def test_unverified_account_override_rejected_and_verified_accepted(self) -> None:
        unverified_cases = [
            ("arcus", "rwa", "NVDA-USD", "public_tier0_schedule_only", 0.0, 2.25),
            ("trade_xyz", "rwa", "xyz:NVDA", "public_schedule_only_unverified_account_tier", 0.3, 0.9),
            ("extended", "crypto", "BTC-USD", "unknown_requires_account_evidence", 0.0, 5.0),
        ]
        with tempfile.TemporaryDirectory() as tmp:
            for venue, domain, raw_market, bad_status, def_mk, def_tk in unverified_cases:
                payload = {
                    "schema_version": "aleph_account_fee_evidence_v1",
                    "venues": {
                        venue: {
                            "account_fee_status": bad_status,
                            "maker_fee_bps": 0.0,
                            "taker_fee_bps": 0.0,
                        }
                    },
                }
                ev_path = Path(tmp) / f"{venue}_bad.json"
                ev_path.write_text(json.dumps(payload), encoding="utf-8")
                with self.assertRaisesRegex(ValueError, "verified_account_tier"):
                    tfe.load_track_fee_evidence(ev_path)
                with self.assertRaisesRegex(ValueError, "verified_account_tier"):
                    tfe.resolve_venue_fee(
                        venue,
                        domain,
                        default_maker_bps=def_mk,
                        default_taker_bps=def_tk,
                        default_status="public_tier0_unverified",
                        evidence={"loaded": True, "venues": {venue: payload["venues"][venue]}},
                        raw_market=raw_market,
                    )

            verified_payload = {
                "schema_version": "aleph_account_fee_evidence_v1",
                "venues": {
                    "arcus": {"account_fee_status": "verified_account_tier", "maker_fee_bps": 0.25, "taker_fee_bps": 1.25},
                    "trade_xyz": {"account_fee_status": "verified_account_tier", "maker_fee_bps": 0.25, "taker_fee_bps": 1.25},
                    "extended": {"account_fee_status": "verified_account_tier", "maker_fee_bps": 0.25, "taker_fee_bps": 1.25},
                },
            }
            good_path = Path(tmp) / "verified.json"
            good_path.write_text(json.dumps(verified_payload), encoding="utf-8")
            loaded = tfe.load_track_fee_evidence(good_path)
            for venue, domain, raw_market, _, def_mk, def_tk in unverified_cases:
                res = tfe.resolve_venue_fee(
                    venue,
                    domain,
                    default_maker_bps=def_mk,
                    default_taker_bps=def_tk,
                    default_status="unknown",
                    evidence=loaded,
                    raw_market=raw_market,
                    arcus_fee_tier="level_4" if venue == "arcus" else "level_0",
                )
                self.assertEqual(res, (0.25, 1.25, "verified_account_tier", "evidence_account_tier"))

            xyz_mode_payload = {
                "schema_version": "aleph_account_fee_evidence_v1",
                "venues": {
                    "trade_xyz": {
                        "account_fee_status": "verified_account_tier",
                        "maker_fee_bps": None,
                        "taker_fee_bps": None,
                        "growth_mode_maker_fee_bps": 0.3,
                        "growth_mode_taker_fee_bps": 0.9,
                        "standard_maker_fee_bps": 3.0,
                        "standard_taker_fee_bps": 9.0,
                    }
                },
            }
            xyz_path = Path(tmp) / "xyz_modes.json"
            xyz_path.write_text(json.dumps(xyz_mode_payload), encoding="utf-8")
            xyz_loaded = tfe.load_track_fee_evidence(xyz_path)
            self.assertEqual(
                tfe.resolve_venue_fee(
                    "trade_xyz",
                    "rwa",
                    default_maker_bps=0.3,
                    default_taker_bps=0.9,
                    default_status="verified_account_tier",
                    evidence=xyz_loaded,
                    trade_xyz_fee_mode="growth_mode",
                ),
                (0.3, 0.9, "verified_account_tier", "evidence_trade_xyz_growth_mode"),
            )
            self.assertEqual(
                tfe.resolve_venue_fee(
                    "trade_xyz",
                    "rwa",
                    default_maker_bps=0.3,
                    default_taker_bps=0.9,
                    default_status="verified_account_tier",
                    evidence=xyz_loaded,
                    trade_xyz_fee_mode="standard",
                ),
                (3.0, 9.0, "verified_account_tier", "evidence_trade_xyz_standard"),
            )

    def test_stdlib_only_isolated_import_without_scientific_stack(self) -> None:
        code = "\n".join(
            [
                "import sys",
                "[sys.modules.__setitem__(m, None) for m in ('numpy','duckdb','pyarrow','pandas','scipy')]",
                f"sys.path.insert(0, {str(SCRIPT_DIR)!r})",
                "import eval_multivenue_three_tracks as ev",
                "prof, _, prov = ev.resolve_venue_profiles()",
                "assert prof['arcus']['taker_fee_bps_equity'] == 2.25",
                "assert prov['venue_capability_snapshot_version'] == ev.VENUE_CAPABILITY_SNAPSHOT_VERSION",
            ]
        )
        proc = subprocess.run([sys.executable, "-c", code], check=False, capture_output=True, text=True)
        self.assertEqual(proc.returncode, 0, proc.stderr or proc.stdout)


if __name__ == "__main__":
    unittest.main()
