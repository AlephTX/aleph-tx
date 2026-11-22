#!/usr/bin/env python3
"""Unit tests for `scripts/run_oos_validation_gate.py` (`Step 2`)."""

from __future__ import annotations

from datetime import datetime
import gzip
import hashlib
import io
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch
from zoneinfo import ZoneInfo

from scripts.run_oos_validation_gate import (
    FROZEN_ANT_ENGINE_FILES,
    evaluate_rth_ant_gate,
    evaluate_w15_rwa_gate,
    main as cli_main,
    verify_w17_crash_pause_prereg,
)

ET_TZ = ZoneInfo("America/New_York")
MON_RTH_START_MS = int(datetime(2026, 11, 23, 9, 0, tzinfo=ET_TZ).timestamp() * 1000)
MON_RTH_END_MS = MON_RTH_START_MS + 10 * 3_600_000


def _sample_ant_window(
    *,
    net: float = 5.0,
    missing: int = 0,
    dd: float = 4.0,
    witnessed: int = 10,
    first_rx_ns: int = MON_RTH_START_MS * 1_000_000,
    last_rx_ns: int = MON_RTH_END_MS * 1_000_000,
) -> dict:
    variants = []
    for mode in ("TAKER_TAKER", "MAKER_ARCUS_TAKER_XYZ"):
        for lat in (100, 250, 500):
            variants.append(
                {
                    "mode": mode,
                    "lat_ms": lat,
                    "net_cash_usd": net,
                    "terminal_flat": True,
                    "max_drawdown_usd": dd,
                    "halted_58pct": False,
                    "cycles_closed": 5,
                    "win_rate": 0.80,
                }
            )
    return {
        "funding_hours_witnessed": witnessed,
        "funding_hours_missing_witness": missing,
        "first_rx_ns": first_rx_ns,
        "last_rx_ns": last_rx_ns,
        "variants": variants,
    }


def _sample_seg_digests() -> dict[str, str]:
    return {"tokyo-research-mon.seg000.ndjson.gz": "a" * 64}


def _sample_eng_digests() -> dict[str, str]:
    return {rel: hashlib.sha256(rel.encode()).hexdigest() for rel in FROZEN_ANT_ENGINE_FILES}


def _sample_ant_prereg(
    *,
    classification: str = "monday_rth_oos",
    frozen_at_ms: int = MON_RTH_START_MS - 60_000,
    decision_start_ms: int = MON_RTH_START_MS,
    decision_end_ms: int = MON_RTH_END_MS,
    expected_rth_date_et: str = "2026-11-23",
    segment_sha256: dict[str, str] | None = None,
    frozen_engine_sha256: dict[str, str] | None = None,
) -> dict:
    return {
        "classification": classification,
        "frozen_at_ms": frozen_at_ms,
        "decision_start_ms": decision_start_ms,
        "decision_end_ms": decision_end_ms,
        "market_session": "us_equities_rth",
        "require_monday": True,
        "expected_rth_date_et": expected_rth_date_et,
        "segment_sha256": _sample_seg_digests() if segment_sha256 is None else segment_sha256,
        "frozen_engine_sha256": _sample_eng_digests() if frozen_engine_sha256 is None else frozen_engine_sha256,
    }


def _sample_rwa_summary(*, nvda_cash: float, other_cash: float, other_dd: float = 12.0) -> dict:
    markets = {}
    for sym in ("NVDA", "SPY", "QQQ", "AMZN", "AAPL", "TSLA"):
        cash = nvda_cash if sym == "NVDA" else other_cash
        markets[sym] = {
            "arms": {
                "CORE_PURE_PULL05_C310": {
                    "core_maker_usd": 500_000.0,
                    "dual_usd": 500_000.0,
                    "terminal_liq_cash_usd": cash,
                    "terminal_flat_liq_ok": True,
                    "max_drawdown_usd": other_dd,
                    "halt_blocked": 50,
                },
                "CORE_HEDGE_SELF_PULL05_C310": {
                    "core_maker_usd": 500_000.0,
                    "dual_usd": 1_000_000.0,
                    "terminal_liq_cash_usd": cash - 5.0,
                    "terminal_flat_liq_ok": True,
                    "max_drawdown_usd": other_dd,
                    "halt_blocked": 1490,
                },
            }
        }
    total_core = 3_000_000.0
    total_pure = nvda_cash + 5 * other_cash
    total_hedge = (nvda_cash - 5.0) + 5 * (other_cash - 5.0)
    return {
        "aggregate_6markets": {
            "CORE_PURE_PULL05_C310": {
                "core_maker_usd": total_core,
                "dual_usd": total_core,
                "terminal_liq_cash_usd": total_pure,
                "single_venue_core_usd_per_m_liq": round(-total_pure / (total_core / 1e6), 2),
                "terminal_flat_all": True,
                "drawdown_58_breached": False,
            },
            "CORE_HEDGE_SELF_PULL05_C310": {
                "core_maker_usd": total_core,
                "dual_usd": 6_000_000.0,
                "terminal_liq_cash_usd": total_hedge,
                "single_venue_core_usd_per_m_liq": round(-total_hedge / (total_core / 1e6), 2),
                "terminal_flat_all": True,
                "drawdown_58_breached": False,
            },
        },
        "markets": markets,
    }


class OosValidationGateTests(unittest.TestCase):
    def test_rth_ant_gate_enforces_oos_role_and_failure_conditions(self) -> None:
        good = _sample_ant_window()
        prereg = _sample_ant_prereg()
        segs = _sample_seg_digests()
        engs = _sample_eng_digests()

        go = evaluate_rth_ant_gate(
            good,
            role="monday_rth_oos",
            prereg_manifest=prereg,
            verified_segment_digests=segs,
            verified_engine_digests=engs,
        )
        self.assertEqual(go["verdict"], "GO")
        self.assertTrue(go["provenance"]["qualified"])

        # Historical selection_dev can NEVER emit GO, even with strong metrics.
        dry = evaluate_rth_ant_gate(good, role="selection_dev_dryrun")
        self.assertEqual(dry["verdict"], "DRY_RUN_NOT_OOS")
        self.assertTrue(dry["passed_metrics"])

        # Missing funding witness, negative taker leg, or drawdown breach -> NO_GO.
        self.assertEqual(
            evaluate_rth_ant_gate(
                _sample_ant_window(missing=1),
                role="monday_rth_oos",
                prereg_manifest=prereg,
                verified_segment_digests=segs,
                verified_engine_digests=engs,
            )["verdict"],
            "NO_GO",
        )
        self.assertEqual(
            evaluate_rth_ant_gate(
                _sample_ant_window(net=-0.5),
                role="monday_rth_oos",
                prereg_manifest=prereg,
                verified_segment_digests=segs,
                verified_engine_digests=engs,
            )["verdict"],
            "NO_GO",
        )
        self.assertEqual(
            evaluate_rth_ant_gate(
                _sample_ant_window(dd=60.0),
                role="monday_rth_oos",
                prereg_manifest=prereg,
                verified_segment_digests=segs,
                verified_engine_digests=engs,
            )["verdict"],
            "NO_GO",
        )

    def test_rth_ant_gate_fails_closed_on_missing_or_invalid_oos_provenance(self) -> None:
        good = _sample_ant_window()
        segs = _sample_seg_digests()
        engs = _sample_eng_digests()

        # 1. Missing manifest -> NO_GO even when metrics pass.
        no_manifest = evaluate_rth_ant_gate(good, role="monday_rth_oos")
        self.assertEqual(no_manifest["verdict"], "NO_GO")
        self.assertTrue(no_manifest["passed_metrics"])
        self.assertFalse(no_manifest["provenance"]["checks"]["prereg_manifest_supplied"])

        # 2. Selection role masquerading in manifest -> NO_GO.
        wrong_cls = evaluate_rth_ant_gate(
            good,
            role="monday_rth_oos",
            prereg_manifest=_sample_ant_prereg(classification="selection_dev"),
            verified_segment_digests=segs,
            verified_engine_digests=engs,
        )
        self.assertEqual(wrong_cls["verdict"], "NO_GO")
        self.assertFalse(wrong_cls["provenance"]["checks"]["classification_is_monday_rth_oos"])

        # 3. Post-hoc freeze (frozen_at_ms > decision_start_ms) -> NO_GO.
        post_hoc = evaluate_rth_ant_gate(
            good,
            role="monday_rth_oos",
            prereg_manifest=_sample_ant_prereg(frozen_at_ms=MON_RTH_START_MS + 1_000),
            verified_segment_digests=segs,
            verified_engine_digests=engs,
        )
        self.assertEqual(post_hoc["verdict"], "NO_GO")
        self.assertFalse(post_hoc["provenance"]["checks"]["pre_registered_before_decision_start"])

        # 4. Weekend (Saturday/Sunday) capture -> NO_GO.
        sun_start = int(datetime(2026, 11, 22, 9, 0, tzinfo=ET_TZ).timestamp() * 1000)
        sun_end = sun_start + 10 * 3_600_000
        weekend = evaluate_rth_ant_gate(
            _sample_ant_window(first_rx_ns=sun_start * 1_000_000, last_rx_ns=sun_end * 1_000_000),
            role="monday_rth_oos",
            prereg_manifest=_sample_ant_prereg(
                frozen_at_ms=sun_start - 60_000,
                decision_start_ms=sun_start,
                decision_end_ms=sun_end,
                expected_rth_date_et="2026-11-22",
            ),
            verified_segment_digests=segs,
            verified_engine_digests=engs,
        )
        self.assertEqual(weekend["verdict"], "NO_GO")
        self.assertFalse(weekend["provenance"]["checks"]["us_rth_weekday_session_covered"])

        # 5. < 10h continuous decision span -> NO_GO.
        short_end = MON_RTH_START_MS + 9 * 3_600_000
        short_span = evaluate_rth_ant_gate(
            _sample_ant_window(last_rx_ns=short_end * 1_000_000),
            role="monday_rth_oos",
            prereg_manifest=_sample_ant_prereg(decision_end_ms=short_end),
            verified_segment_digests=segs,
            verified_engine_digests=engs,
        )
        self.assertEqual(short_span["verdict"], "NO_GO")
        self.assertFalse(short_span["provenance"]["checks"]["continuous_10h_span"])

        # 6. Insufficient witnessed funding hours -> NO_GO.
        low_funding = evaluate_rth_ant_gate(
            _sample_ant_window(witnessed=8),
            role="monday_rth_oos",
            prereg_manifest=_sample_ant_prereg(),
            verified_segment_digests=segs,
            verified_engine_digests=engs,
        )
        self.assertEqual(low_funding["verdict"], "NO_GO")
        self.assertFalse(low_funding["provenance"]["checks"]["funding_hours_cover_decision_window"])

        # 7. Missing or truncated observed capture coverage -> NO_GO.
        truncated_cov = evaluate_rth_ant_gate(
            _sample_ant_window(last_rx_ns=(MON_RTH_END_MS - 120_000) * 1_000_000),
            role="monday_rth_oos",
            prereg_manifest=_sample_ant_prereg(),
            verified_segment_digests=segs,
            verified_engine_digests=engs,
        )
        self.assertEqual(truncated_cov["verdict"], "NO_GO")
        self.assertFalse(truncated_cov["provenance"]["checks"]["observed_capture_covers_decision_window"])

        # 8. Segment mismatch or extra unsealed segment -> NO_GO.
        bad_seg = evaluate_rth_ant_gate(
            good,
            role="monday_rth_oos",
            prereg_manifest=_sample_ant_prereg(),
            verified_segment_digests={**segs, "tokyo-research-mon.seg001.ndjson.gz": "b" * 64},
            verified_engine_digests=engs,
        )
        self.assertEqual(bad_seg["verdict"], "NO_GO")
        self.assertFalse(bad_seg["provenance"]["checks"]["sealed_segment_hashes_verified"])

        # 9. Frozen engine hash mismatch -> NO_GO.
        bad_eng = evaluate_rth_ant_gate(
            good,
            role="monday_rth_oos",
            prereg_manifest=_sample_ant_prereg(),
            verified_segment_digests=segs,
            verified_engine_digests={**engs, FROZEN_ANT_ENGINE_FILES[0]: "0" * 64},
        )
        self.assertEqual(bad_eng["verdict"], "NO_GO")
        self.assertFalse(bad_eng["provenance"]["checks"]["frozen_engine_hashes_verified"])

    def test_rth_ant_cli_fails_closed_without_manifest_and_hashes_segments(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            cap = root / "cap_mon"
            cap.mkdir()
            seg = cap / "tokyo-research-mon.seg000.ndjson.gz"
            with gzip.open(seg, "wt") as f:
                f.write("")
            out_no_manifest = root / "out_no_manifest.json"
            with patch.object(
                sys,
                "argv",
                [
                    "run_oos_validation_gate.py",
                    "rth-ant",
                    "--capture-dirs",
                    str(cap),
                    "--role",
                    "monday_rth_oos",
                    "--output",
                    str(out_no_manifest),
                ],
            ), patch("sys.stdout", new_callable=io.StringIO):
                rc = cli_main()
            self.assertEqual(rc, 1)
            rep_no_manifest = json.loads(out_no_manifest.read_text())
            self.assertEqual(rep_no_manifest["verdict"], "NO_GO")
            self.assertFalse(rep_no_manifest["provenance"]["checks"]["prereg_manifest_supplied"])

            repo_root = Path(__file__).resolve().parent.parent
            seg_sha = hashlib.sha256(seg.read_bytes()).hexdigest()
            eng_shas = {
                rel: hashlib.sha256((repo_root / rel).read_bytes()).hexdigest()
                for rel in FROZEN_ANT_ENGINE_FILES
            }
            manifest_path = root / "manifest.json"
            manifest_path.write_text(
                json.dumps(
                    _sample_ant_prereg(
                        segment_sha256={"tokyo-research-mon.seg000.ndjson.gz": seg_sha},
                        frozen_engine_sha256=eng_shas,
                    )
                )
            )
            out_with_manifest = root / "out_with_manifest.json"
            with patch.object(
                sys,
                "argv",
                [
                    "run_oos_validation_gate.py",
                    "rth-ant",
                    "--capture-dirs",
                    str(cap),
                    "--role",
                    "monday_rth_oos",
                    "--prereg-manifest",
                    str(manifest_path),
                    "--output",
                    str(out_with_manifest),
                ],
            ), patch("sys.stdout", new_callable=io.StringIO):
                rc_with = cli_main()
            self.assertEqual(rc_with, 1)
            rep_with = json.loads(out_with_manifest.read_text())
            self.assertEqual(rep_with["verdict"], "NO_GO")
            self.assertTrue(rep_with["provenance"]["checks"]["sealed_segment_hashes_verified"])
            self.assertTrue(rep_with["provenance"]["checks"]["frozen_engine_hashes_verified"])
            self.assertFalse(rep_with["provenance"]["checks"]["observed_capture_covers_decision_window"])

    def test_w15_rwa_gate_distinguishes_hedged_unhedged_and_no_go(self) -> None:
        # Pure ex-NVDA = +$2.5M volume, -$25 cash = $10/M (<=20); Hedged ex-NVDA = -$50 = $20/M (<=20)
        hedged_ok = _sample_rwa_summary(nvda_cash=-80.0, other_cash=-5.0)
        res_hedged = evaluate_w15_rwa_gate(hedged_ok)
        self.assertEqual(res_hedged["verdict"], "GO_HEDGED")

        # Pure ex-NVDA = -$5/M (-$5 -> +$25), Hedged = -$30 -> $12/M? Wait: let pure=-7.0 ($14/M), hedge=-12.0 ($24/M)
        unhedged_only = _sample_rwa_summary(nvda_cash=-80.0, other_cash=-7.0)
        res_unhedged = evaluate_w15_rwa_gate(unhedged_only)
        self.assertEqual(res_unhedged["verdict"], "GO_UNHEDGED_ONLY")

        bad = _sample_rwa_summary(nvda_cash=-80.0, other_cash=-15.0)
        self.assertEqual(evaluate_w15_rwa_gate(bad)["verdict"], "NO_GO")

        dd_breach = _sample_rwa_summary(nvda_cash=-80.0, other_cash=-5.0, other_dd=600.0)
        self.assertEqual(evaluate_w15_rwa_gate(dd_breach)["verdict"], "NO_GO")

    def test_w17_crash_pause_verify_detects_tampering(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            target = root / "src" / "frozen.rs"
            target.parent.mkdir(parents=True)
            content = b"fn frozen() {}\n"
            target.write_bytes(content)
            prereg = root / "prereg.json"
            prereg.write_text(
                json.dumps(
                    {
                        "card_id": "TEST-CARD",
                        "method_sha256": "abc",
                        "builder_cli": "echo ok",
                        "artifact_sha256": {
                            "src/frozen.rs": hashlib.sha256(content).hexdigest()
                        },
                    }
                )
            )
            ok = verify_w17_crash_pause_prereg(prereg, root)
            self.assertEqual(ok["verdict"], "VERIFIED")

            target.write_bytes(b"fn tampered() {}\n")
            bad = verify_w17_crash_pause_prereg(prereg, root)
            self.assertEqual(bad["verdict"], "MISMATCH")


if __name__ == "__main__":
    unittest.main()
