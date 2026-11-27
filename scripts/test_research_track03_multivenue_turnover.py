#!/usr/bin/env python3
"""Stdlib-only unit tests for `scripts/research_track03_multivenue_turnover.py`."""

from __future__ import annotations

import json
import re
import subprocess
import sys
import tempfile
import unittest
from datetime import datetime, timezone
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

import eval_volume_farming_cost as vf  # noqa: E402
import research_track03_multivenue_turnover as t3  # noqa: E402


class ResearchTrack03MultivenueTurnoverTests(unittest.TestCase):
    def test_stdlib_only_isolated_subprocess_blocking_third_party(self) -> None:
        code = "\n".join(
            [
                "import builtins, sys",
                "blocked = {'numpy', 'duckdb', 'pyarrow', 'pandas', 'scipy'}",
                "orig_import = builtins.__import__",
                "def guarded(name, globals=None, locals=None, fromlist=(), level=0):",
                "    if name.split('.')[0] in blocked:",
                "        raise ImportError(f'blocked third-party import: {name}')",
                "    return orig_import(name, globals, locals, fromlist, level)",
                "builtins.__import__ = guarded",
                f"sys.path.insert(0, {str(SCRIPT_DIR)!r})",
                "import research_track03_multivenue_turnover as m",
                "res = m.run_self_test()",
                "assert res['self_test'] == 'ok'",
            ]
        )
        proc = subprocess.run(
            [sys.executable, "-c", code],
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertEqual(proc.returncode, 0, proc.stderr or proc.stdout)

    def test_dynamic_campaign_discovery_splits_and_cross_midnight_glob(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            # Populate synthetic W01..W25 Hive dirs: crypto skips w08, rwa omits w06/w08/w10..w12/w22, bulk has w01..w25
            for w in range(1, 26):
                cid_bulk = f"tokyo-bulk-20261120T000000Z-w{w:02d}"
                (root / "parquet/bbo_quotes/collector_region=tokyo/venue=bulk/utc_date=2026-11-20" / f"campaign_id={cid_bulk}").mkdir(parents=True)
                if w != 8:
                    cid_cr = f"tokyo-crypto-20261120T000000Z-w{w:02d}"
                    (root / "parquet/bbo_quotes/collector_region=tokyo/venue=lighter/utc_date=2026-11-20" / f"campaign_id={cid_cr}").mkdir(parents=True)
                if w in (1, 2, 3, 4, 5, 7, 9, 13, 14, 15, 16, 17, 18, 19, 20, 21, 23, 24, 25):
                    cid_rw = f"tokyo-rwa-20261120T000000Z-w{w:02d}"
                    (root / "parquet/bbo_quotes/collector_region=tokyo/venue=arcus/utc_date=2026-11-20" / f"campaign_id={cid_rw}").mkdir(parents=True)

            full_dev = t3.discover_campaigns(root, "full_dev")
            holdout = t3.discover_campaigns(root, "holdout")
            crypto_dev = [w for w, d in full_dev.items() if "crypto" in d]
            rwa_dev = [w for w, d in full_dev.items() if "rwa" in d]
            bulk_dev = [w for w, d in full_dev.items() if "bulk" in d]
            self.assertEqual(len(crypto_dev), 19)
            self.assertNotIn("w08", crypto_dev)
            self.assertEqual(len(rwa_dev), 15)
            self.assertEqual(len(bulk_dev), 20)

            crypto_ho = [w for w, d in holdout.items() if "crypto" in d]
            rwa_ho = [w for w, d in holdout.items() if "rwa" in d]
            bulk_ho = [w for w, d in holdout.items() if "bulk" in d]
            self.assertEqual(crypto_ho, ["w21", "w22", "w23", "w24", "w25"])
            self.assertEqual(rwa_ho, ["w21", "w23", "w24", "w25"])
            self.assertEqual(bulk_ho, ["w21", "w22", "w23", "w24", "w25"])

            # Cross-midnight parquet globbing
            cid = "tokyo-crypto-20261124T231506Z-w23"
            d0 = root / "parquet/bbo_quotes/collector_region=tokyo/venue=lighter/utc_date=2026-11-24" / f"campaign_id={cid}"
            d1 = root / "parquet/bbo_quotes/collector_region=tokyo/venue=lighter/utc_date=2026-11-25" / f"campaign_id={cid}"
            d2 = root / "parquet/bbo_quotes/collector_region=tokyo/venue=lighter/utc_date=2026-11-26" / f"campaign_id={cid}"
            for d in (d0, d1, d2):
                d.mkdir(parents=True, exist_ok=True)
            f0, f1, f2 = d0 / "a.parquet", d1 / "b.parquet", d2 / "c.parquet"
            for f in (f0, f1, f2):
                f.write_bytes(b"PAR1")
            found = t3._campaign_parquet_files(root, "bbo_quotes", "lighter", "2026-11-24", cid)
            self.assertEqual(found, sorted([str(f0), str(f1)]))

    def test_targets_settlements_and_utc_session_boundaries(self) -> None:
        targets = t3.build_default_targets(include_bulk_diagnostic=True)
        self.assertEqual({t.symbol for t in targets if t.domain == "crypto"}, {"BTC", "ETH", "SOL"})
        self.assertEqual(
            {t.symbol for t in targets if t.domain == "rwa"},
            {"AAPL", "SPY", "QQQ", "NVDA", "TSLA", "DRAM", "SNDK"},
        )
        by_venue = {(t.venue, t.settlement, t.account_fee_status, t.has_public_trades) for t in targets}
        self.assertIn(("lighter", "USDC", "public_tier0_unverified", True), by_venue)
        self.assertIn(("lighter_robinhood", "USDG", "public_tier0_unverified", True), by_venue)
        self.assertIn(("extended", "USDC", "account_evidence_required", True), by_venue)
        self.assertIn(("arcus", "USDC", "public_tier0_schedule_only", True), by_venue)
        self.assertIn(("bulk", "USDC", "account_evidence_required", False), by_venue)

        t_asia = int(datetime(2026, 11, 24, 10, 59, 0, tzinfo=timezone.utc).timestamp() * 1000)
        t_ext1 = int(datetime(2026, 11, 24, 11, 0, 0, tzinfo=timezone.utc).timestamp() * 1000)
        t_rth0 = int(datetime(2026, 11, 24, 13, 30, 0, tzinfo=timezone.utc).timestamp() * 1000)
        t_ext2 = int(datetime(2026, 11, 24, 20, 0, 0, tzinfo=timezone.utc).timestamp() * 1000)
        self.assertEqual(t3.classify_utc_session(t_asia), "overnight_asia")
        self.assertEqual(t3.classify_utc_session(t_ext1), "us_extended")
        self.assertEqual(t3.classify_utc_session(t_rth0), "us_rth")
        self.assertEqual(t3.classify_utc_session(t_ext2), "us_extended")

    def test_rwa_session_gating_blocks_us_rth_openings_but_keeps_reducing_exits(self) -> None:
        t_ext = int(datetime(2026, 11, 24, 13, 29, 55, tzinfo=timezone.utc).timestamp() * 1000)
        t_rth = int(datetime(2026, 11, 24, 13, 30, 2, tzinfo=timezone.utc).timestamp() * 1000)
        events = [
            {"ts": t_ext, "kind": "book", "bb": 100.00, "bsz": 10.0, "ba": 100.04, "asz": 10.0},
            {"ts": t_ext + 300, "kind": "trade", "side": "SELL", "price": 99.99, "size": 8.0},
            {"ts": t_rth, "kind": "book", "bb": 100.02, "bsz": 10.0, "ba": 100.06, "asz": 10.0},
            {"ts": t_rth + 250, "kind": "book", "bb": 100.02, "bsz": 10.0, "ba": 100.06, "asz": 10.0},
            {"ts": t_rth + 600, "kind": "trade", "side": "BUY", "price": 100.07, "size": 8.0},
            {"ts": t_rth + 700, "kind": "trade", "side": "SELL", "price": 100.01, "size": 8.0},
            {"ts": t_rth + 2_000, "kind": "book", "bb": 100.02, "bsz": 10.0, "ba": 100.06, "asz": 10.0},
        ]
        g4_quiet = t3.build_ablation_arms("lighter", "rwa", arm_names=["G4_quiet_sessions_only"])[0]
        res = t3.simulate_turnover_arm(events, 0.01, g4_quiet)
        self.assertTrue(res["terminal_flat"])
        self.assertEqual(res["maker_fills"], 2)
        self.assertLess(res["taker_exit_share_s"], 0.005)
        self.assertGreater(res["session_gated_ticks"], 0)
        self.assertAlmostEqual(res["_exact_gross_usd"], res["_exact_cash_ex_fee_usd"], places=9)

    def test_crypto_causal_vol_quartile_gate_and_exact_cost_identity(self) -> None:
        t0 = int(datetime(2026, 11, 24, 1, 0, 0, tzinfo=timezone.utc).timestamp() * 1000)
        events = []
        for i in range(20):
            ts = t0 + i * 5_000
            events.append({"ts": ts, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.04, 10.0)]})
        t_q = t0 + 20 * 5_000
        events.append({"ts": t_q, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.04, 10.0)]})
        events.append({"ts": t_q + 300, "kind": "trade", "side": "SELL", "price": 99.99, "size": 8.0})
        events.append({"ts": t_q + 1_000, "kind": "book", "bids": [(100.02, 10.0)], "asks": [(100.06, 10.0)]})
        events.append({"ts": t_q + 1_060, "kind": "book", "bids": [(100.02, 10.0)], "asks": [(100.06, 10.0)]})
        events.append({"ts": t_q + 1_300, "kind": "trade", "side": "BUY", "price": 100.07, "size": 8.0})

        t_v = t_q + 5_000
        events.append({"ts": t_v, "kind": "book", "bids": [(100.20, 10.0)], "asks": [(100.24, 10.0)]})
        events.append({"ts": t_v + 300, "kind": "trade", "side": "SELL", "price": 100.19, "size": 8.0})
        events.append({"ts": t_v + 2_000, "kind": "book", "bids": [(100.20, 10.0)], "asks": [(100.24, 10.0)]})

        arm = t3.TurnoverArmConfig(
            name="G4_low_vol_q25",
            ablation_group="quiet_sessions_only",
            order_delay_ms=40,
            cancel_delay_ms=40,
            exit_delay_ms=45,
            maker_fee_bps=0.0,
            taker_fee_bps=2.50,
            rebate_bps=0.0,
            use_as_skew=True,
            quiet_regime_only=True,
            quiet_max_sigma_bps=1.2,
            causal_vol_quantile_max=0.25,
            causal_vol_min_samples=10,
        )
        res = t3.simulate_turnover_arm(events, 0.01, arm)
        self.assertTrue(res["terminal_flat"])
        self.assertEqual(res["maker_fills"], 2)
        self.assertGreater(res["vol_quantile_gated_ticks"], 0)
        self.assertAlmostEqual(res["_exact_gross_usd"], res["_exact_cash_ex_fee_usd"], places=9)
        self.assertAlmostEqual(res["net"], res["gross_usd"] - res["fee_usd"], places=6)
        bd = vf.compute_volume_farming_cost(
            res["gross_ex_fee_bps"],
            res["taker_exit_share_s"],
            arm.taker_fee_bps,
            maker_fee_bps=arm.maker_fee_bps,
            rebate_bps=0.0,
        )
        self.assertAlmostEqual(res["double_sided_usd_per_m"], bd.double_sided_usd_per_m, places=3)

    def test_regime_stratification_tail_risk_and_bulk_separation(self) -> None:
        def _row(win: str, venue: str, ev_mode: str, vol: float, spr: float, drft: float, fee: float) -> dict:
            return {
                "win": win,
                "domain": "crypto",
                "arm": "G4_all_hours",
                "ablation_group": "all_hours",
                "venue": venue,
                "symbol": "BTC",
                "settlement": "USDC",
                "account_fee_status": "public_tier0_unverified",
                "fill_evidence_mode": ev_mode,
                "continuity": {"hours": 10.25},
                "cont": True,
                "realized_1s_vol_bps": vol,
                "p99_1s_abs_ret_bps": vol * 3.0,
                "cyc": 10,
                "fills": 20,
                "spr": spr,
                "drft": drft,
                "pex": 0.0,
                "tslp": 0.0,
                "fee_usd": fee,
                "net": spr + drft - fee,
                "double_sided_usd_per_m": 0.0,
                "ntl": 2000.0,
                "maker_volume_usd": 3800.0,
                "taker_volume_usd": 200.0,
                "maker_fee_bps": 0.0,
                "taker_fee_bps": 0.0,
                "rebate_bps": 0.0,
                "markout_1s_bps": 0.1,
                "markout_1s_den": 2000.0,
                "markout_5s_bps": None,
                "markout_5s_den": 0.0,
                "markout_10s_bps": None,
                "markout_10s_den": 0.0,
            }

        rows = [
            _row("w01", "lighter", "public_trades", 0.50, 2.0, -0.5, 0.0),   # calm: +1.5
            _row("w03", "lighter", "public_trades", 1.00, 1.0, -4.0, 0.0),   # extreme_crash (w03): -3.0
            _row("w04", "lighter", "public_trades", 0.90, 2.0, -1.0, 0.0),   # normal: +1.0
            _row("w01", "bulk", "book_diff_diagnostic", 0.50, 1.0, -0.5, 0.2),
        ]
        summary = t3.annotate_and_aggregate_turnover(rows)
        lt = summary["by_domain_arm_venue"]["crypto|G4_all_hours|lighter|public_trades"]
        self.assertEqual(lt["windows_covered"], 3)
        self.assertAlmostEqual(lt["hours_covered"], 30.75, places=2)
        self.assertAlmostEqual(lt["positive_window_ratio"], 2.0 / 3.0, places=3)
        self.assertAlmostEqual(lt["max_drawdown_usd"], 3.0, places=4)
        self.assertEqual(lt["worst_10h_window"]["win"], "w03")
        self.assertEqual(lt["worst_10h_window"]["regime"], "extreme_crash")
        self.assertIn("calm", lt["regime_breakdown"])
        self.assertIn("normal", lt["regime_breakdown"])
        self.assertIn("extreme_crash", lt["regime_breakdown"])
        self.assertEqual(len(lt["cumulative_curve"]), 3)
        self.assertIn("crypto|G4_all_hours|bulk|book_diff_diagnostic", summary["by_domain_arm_venue"])
        self.assertEqual(len(summary["w03_extreme_highlight"]), 1)

    def test_cli_self_test(self) -> None:
        proc = subprocess.run(
            [sys.executable, str(SCRIPT_DIR / "research_track03_multivenue_turnover.py"), "--self-test"],
            check=True,
            capture_output=True,
            text=True,
        )
        payload = json.loads(proc.stdout)
        self.assertEqual(payload["self_test"], "ok")
        self.assertEqual(payload["bulk_mode"], "book_diff_diagnostic")

    def test_opt01_snapshot_and_fee_evidence_overrides(self) -> None:
        venues_rs = (SCRIPT_DIR.parent / "src" / "venues.rs").read_text(encoding="utf-8")
        m = re.search(r'VENUE_CAPABILITY_SNAPSHOT_VERSION[^"]*"([^"]+)"', venues_rs)
        self.assertIsNotNone(m)
        self.assertEqual(t3.VENUE_CAPABILITY_SNAPSHOT_VERSION, m.group(1))

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
                            {"maker_fee_bps": "0", "taker_fee_bps": "2.25"},
                            {"maker_fee_bps": "1.5", "taker_fee_bps": "4.5"},
                        ],
                        "custom_schedule_tier0_bps": {
                            "AAPL-USD": {"maker_fee_bps": "0", "taker_fee_bps": "2.25"},
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
            },
        }
        with tempfile.TemporaryDirectory() as tmp:
            ev_path = Path(tmp) / "fee_ev.json"
            ev_path.write_text(json.dumps(ev_payload), encoding="utf-8")

            targets, prov = t3.build_targets_with_provenance(
                include_bulk_diagnostic=True, fee_evidence_path=ev_path, arcus_fee_tier="level_0"
            )
            self.assertTrue(prov["fee_evidence_loaded"])
            self.assertEqual(prov["venue_capability_snapshot_version"], m.group(1))
            by_key = {(t.venue, t.domain, t.symbol): t for t in targets}
            self.assertEqual(by_key[("lighter", "crypto", "BTC")].account_fee_status, "public_schedule_only_unverified_account_tier")
            self.assertEqual(by_key[("lighter_robinhood", "crypto", "BTC")].account_fee_status, "public_tier0_unverified")
            self.assertEqual(by_key[("extended", "crypto", "BTC")].account_fee_status, "unknown_requires_account_evidence")
            self.assertEqual(by_key[("bulk", "crypto", "BTC")].account_fee_status, "unknown_requires_account_evidence")
            self.assertEqual(by_key[("arcus", "crypto", "BTC")].maker_fee_bps, 1.50)
            self.assertEqual(by_key[("arcus", "crypto", "BTC")].taker_fee_bps, 4.50)
            self.assertEqual(by_key[("arcus", "rwa", "AAPL")].fee_source, "evidence_public_tier0_custom_market")
            self.assertEqual(by_key[("arcus", "rwa", "AAPL")].taker_fee_bps, 2.25)

            sens_targets, _ = t3.build_targets_with_provenance(
                include_bulk_diagnostic=False, fee_evidence_path=ev_path, arcus_fee_tier="level_4"
            )
            sens_map = {(t.venue, t.domain, t.symbol): t for t in sens_targets}
            self.assertEqual(sens_map[("arcus", "crypto", "BTC")].taker_fee_bps, 2.30)
            self.assertEqual(sens_map[("arcus", "rwa", "AAPL")].taker_fee_bps, 1.15)
            self.assertEqual(sens_map[("arcus", "rwa", "AAPL")].account_fee_status, "account_evidence_required")

            bad_path = Path(tmp) / "bad.json"
            bad_path.write_text(json.dumps({"venues": {"lighter": {"maker_fee_bps": True, "taker_fee_bps": "0"}}}), encoding="utf-8")
            with self.assertRaises(ValueError):
                t3.build_targets_with_provenance(fee_evidence_path=bad_path)


if __name__ == "__main__":
    unittest.main()
