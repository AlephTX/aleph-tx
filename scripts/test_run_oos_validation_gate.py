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
    start_ms: int = MON_RTH_START_MS,
    end_ms: int = MON_RTH_END_MS,
    first_rx_ns: int | None = None,
    last_rx_ns: int | None = None,
    decision_bounds_ns: list[int] | None = None,
    arc_events: int = 100,
    xyz_events: int = 100,
    fund_events: int = 20,
    arc_max_gap_ns: int = 30_000_000_000,
    xyz_max_gap_ns: int = 30_000_000_000,
    fund_max_gap_ns: int = 1_800_000_000_000,
    arc_first_rx_ns: int | None = None,
    arc_last_rx_ns: int | None = None,
    xyz_first_rx_ns: int | None = None,
    xyz_last_rx_ns: int | None = None,
    gaps: list[dict] | None = None,
) -> dict:
    f_ns = start_ms * 1_000_000 if first_rx_ns is None else first_rx_ns
    l_ns = end_ms * 1_000_000 if last_rx_ns is None else last_rx_ns
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
        "decision_bounds_ns": [start_ms * 1_000_000, end_ms * 1_000_000] if decision_bounds_ns is None else decision_bounds_ns,
        "funding_hours_witnessed": witnessed,
        "funding_hours_missing_witness": missing,
        "first_rx_ns": f_ns,
        "last_rx_ns": l_ns,
        "gaps": [] if gaps is None else gaps,
        "source_continuity": {
            "arcus_book": {
                "events": arc_events,
                "first_rx_ns": f_ns if arc_first_rx_ns is None else arc_first_rx_ns,
                "last_rx_ns": l_ns if arc_last_rx_ns is None else arc_last_rx_ns,
                "max_gap_ns": arc_max_gap_ns,
            },
            "xyz_book": {
                "events": xyz_events,
                "first_rx_ns": f_ns if xyz_first_rx_ns is None else xyz_first_rx_ns,
                "last_rx_ns": l_ns if xyz_last_rx_ns is None else xyz_last_rx_ns,
                "max_gap_ns": xyz_max_gap_ns,
            },
            "xyz_funding": {
                "events": fund_events,
                "first_rx_ns": f_ns,
                "last_rx_ns": l_ns,
                "max_gap_ns": fund_max_gap_ns,
            },
            "warmup_lines_excluded": 0,
            "post_window_lines_excluded": 0,
        },
        "variants": variants,
    }


def _sample_seg_digests() -> dict[str, str]:
    return {"tokyo-research-mon.seg000.ndjson.gz": "a" * 64}


def _sample_eng_digests() -> dict[str, str]:
    return {rel: hashlib.sha256(rel.encode()).hexdigest() for rel in FROZEN_ANT_ENGINE_FILES}


def _sample_prereg_sha(prereg: dict) -> str:
    return hashlib.sha256(json.dumps(prereg, sort_keys=True).encode()).hexdigest()


def _sample_ant_prereg(
    *,
    classification: str = "monday_rth_oos",
    frozen_at_ms: int = MON_RTH_START_MS - 60_000,
    decision_start_ms: int = MON_RTH_START_MS,
    decision_end_ms: int = MON_RTH_END_MS,
    expected_rth_date_et: str = "2026-11-23",
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
        "frozen_engine_sha256": _sample_eng_digests() if frozen_engine_sha256 is None else frozen_engine_sha256,
    }


def _sample_seal_receipt(
    prereg_sha256: str,
    *,
    sealed_at_ms: int = MON_RTH_END_MS + 60_000,
    segment_sha256: dict[str, str] | None = None,
    archive_first_rx_ns: int = MON_RTH_START_MS * 1_000_000,
    archive_last_rx_ns: int = MON_RTH_END_MS * 1_000_000,
    arcus_book_events: int = 100,
    xyz_book_events: int = 100,
) -> dict:
    return {
        "prereg_manifest_sha256": prereg_sha256,
        "sealed_at_ms": sealed_at_ms,
        "segment_sha256": _sample_seg_digests() if segment_sha256 is None else segment_sha256,
        "measured_source_coverage": {
            "archive_first_rx_ns": archive_first_rx_ns,
            "archive_last_rx_ns": archive_last_rx_ns,
            "arcus_book_events": arcus_book_events,
            "xyz_book_events": xyz_book_events,
        },
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
        prereg_sha = _sample_prereg_sha(prereg)
        seal = _sample_seal_receipt(prereg_sha)
        segs = _sample_seg_digests()
        engs = _sample_eng_digests()

        go = evaluate_rth_ant_gate(
            good,
            role="monday_rth_oos",
            prereg_manifest=prereg,
            seal_receipt=seal,
            verified_prereg_sha256=prereg_sha,
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
        for bad_win in (_sample_ant_window(missing=1), _sample_ant_window(net=-0.5), _sample_ant_window(dd=60.0)):
            self.assertEqual(
                evaluate_rth_ant_gate(
                    bad_win,
                    role="monday_rth_oos",
                    prereg_manifest=prereg,
                    seal_receipt=seal,
                    verified_prereg_sha256=prereg_sha,
                    verified_segment_digests=segs,
                    verified_engine_digests=engs,
                )["verdict"],
                "NO_GO",
            )

    def test_rth_ant_gate_fails_closed_on_missing_or_invalid_oos_provenance(self) -> None:
        good = _sample_ant_window()
        prereg = _sample_ant_prereg()
        prereg_sha = _sample_prereg_sha(prereg)
        seal = _sample_seal_receipt(prereg_sha)
        segs = _sample_seg_digests()
        engs = _sample_eng_digests()

        # 1. Missing manifest -> NO_GO even when metrics pass.
        no_manifest = evaluate_rth_ant_gate(good, role="monday_rth_oos")
        self.assertEqual(no_manifest["verdict"], "NO_GO")
        self.assertTrue(no_manifest["passed_metrics"])
        self.assertFalse(no_manifest["provenance"]["checks"]["prereg_manifest_supplied"])

        # 1b. Single merged post-hoc manifest containing segment_sha256 -> NO_GO.
        merged_prereg = {**prereg, "segment_sha256": segs}
        merged = evaluate_rth_ant_gate(
            good,
            role="monday_rth_oos",
            prereg_manifest=merged_prereg,
            seal_receipt=seal,
            verified_prereg_sha256=_sample_prereg_sha(merged_prereg),
            verified_segment_digests=segs,
            verified_engine_digests=engs,
        )
        self.assertEqual(merged["verdict"], "NO_GO")
        self.assertFalse(merged["provenance"]["checks"]["prereg_manifest_supplied"])

        # 1c. Missing seal receipt, wrong prereg SHA-256 link, or premature seal timestamp -> NO_GO.
        no_seal = evaluate_rth_ant_gate(
            good,
            role="monday_rth_oos",
            prereg_manifest=prereg,
            verified_prereg_sha256=prereg_sha,
            verified_segment_digests=segs,
            verified_engine_digests=engs,
        )
        self.assertEqual(no_seal["verdict"], "NO_GO")
        self.assertFalse(no_seal["provenance"]["checks"]["seal_receipt_supplied"])

        bad_link = evaluate_rth_ant_gate(
            good,
            role="monday_rth_oos",
            prereg_manifest=prereg,
            seal_receipt=_sample_seal_receipt("f" * 64),
            verified_prereg_sha256=prereg_sha,
            verified_segment_digests=segs,
            verified_engine_digests=engs,
        )
        self.assertEqual(bad_link["verdict"], "NO_GO")
        self.assertFalse(bad_link["provenance"]["checks"]["seal_links_prereg_sha256"])

        early_seal = evaluate_rth_ant_gate(
            good,
            role="monday_rth_oos",
            prereg_manifest=prereg,
            seal_receipt=_sample_seal_receipt(prereg_sha, sealed_at_ms=MON_RTH_END_MS - 1_000),
            verified_prereg_sha256=prereg_sha,
            verified_segment_digests=segs,
            verified_engine_digests=engs,
        )
        self.assertEqual(early_seal["verdict"], "NO_GO")
        self.assertFalse(early_seal["provenance"]["checks"]["sealed_after_decision_end"])

        # 2. Selection role masquerading in manifest -> NO_GO.
        p_wrong = _sample_ant_prereg(classification="selection_dev")
        p_wrong_sha = _sample_prereg_sha(p_wrong)
        wrong_cls = evaluate_rth_ant_gate(
            good,
            role="monday_rth_oos",
            prereg_manifest=p_wrong,
            seal_receipt=_sample_seal_receipt(p_wrong_sha),
            verified_prereg_sha256=p_wrong_sha,
            verified_segment_digests=segs,
            verified_engine_digests=engs,
        )
        self.assertEqual(wrong_cls["verdict"], "NO_GO")
        self.assertFalse(wrong_cls["provenance"]["checks"]["classification_is_monday_rth_oos"])

        # 3. Post-hoc freeze (frozen_at_ms > decision_start_ms) -> NO_GO.
        p_post = _sample_ant_prereg(frozen_at_ms=MON_RTH_START_MS + 1_000)
        p_post_sha = _sample_prereg_sha(p_post)
        post_hoc = evaluate_rth_ant_gate(
            good,
            role="monday_rth_oos",
            prereg_manifest=p_post,
            seal_receipt=_sample_seal_receipt(p_post_sha),
            verified_prereg_sha256=p_post_sha,
            verified_segment_digests=segs,
            verified_engine_digests=engs,
        )
        self.assertEqual(post_hoc["verdict"], "NO_GO")
        self.assertFalse(post_hoc["provenance"]["checks"]["pre_registered_before_decision_start"])

        # 4. Weekend (Saturday/Sunday) capture -> NO_GO.
        sun_start = int(datetime(2026, 11, 22, 9, 0, tzinfo=ET_TZ).timestamp() * 1000)
        sun_end = sun_start + 10 * 3_600_000
        p_wk = _sample_ant_prereg(
            frozen_at_ms=sun_start - 60_000,
            decision_start_ms=sun_start,
            decision_end_ms=sun_end,
            expected_rth_date_et="2026-11-22",
        )
        p_wk_sha = _sample_prereg_sha(p_wk)
        weekend = evaluate_rth_ant_gate(
            _sample_ant_window(start_ms=sun_start, end_ms=sun_end),
            role="monday_rth_oos",
            prereg_manifest=p_wk,
            seal_receipt=_sample_seal_receipt(
                p_wk_sha,
                sealed_at_ms=sun_end + 60_000,
                archive_first_rx_ns=sun_start * 1_000_000,
                archive_last_rx_ns=sun_end * 1_000_000,
            ),
            verified_prereg_sha256=p_wk_sha,
            verified_segment_digests=segs,
            verified_engine_digests=engs,
        )
        self.assertEqual(weekend["verdict"], "NO_GO")
        self.assertFalse(weekend["provenance"]["checks"]["us_rth_weekday_session_covered"])

        # 5. != 10h or missing/mismatched engine decision_bounds_ns -> NO_GO.
        short_end = MON_RTH_START_MS + 9 * 3_600_000
        p_short = _sample_ant_prereg(decision_end_ms=short_end)
        p_short_sha = _sample_prereg_sha(p_short)
        short_span = evaluate_rth_ant_gate(
            _sample_ant_window(end_ms=short_end),
            role="monday_rth_oos",
            prereg_manifest=p_short,
            seal_receipt=_sample_seal_receipt(p_short_sha, archive_last_rx_ns=short_end * 1_000_000),
            verified_prereg_sha256=p_short_sha,
            verified_segment_digests=segs,
            verified_engine_digests=engs,
        )
        self.assertEqual(short_span["verdict"], "NO_GO")
        self.assertFalse(short_span["provenance"]["checks"]["exact_10h_bounded_decision_window"])

        unbounded_res = evaluate_rth_ant_gate(
            _sample_ant_window(decision_bounds_ns=[]),
            role="monday_rth_oos",
            prereg_manifest=prereg,
            seal_receipt=seal,
            verified_prereg_sha256=prereg_sha,
            verified_segment_digests=segs,
            verified_engine_digests=engs,
        )
        self.assertEqual(unbounded_res["verdict"], "NO_GO")
        self.assertFalse(unbounded_res["provenance"]["checks"]["exact_10h_bounded_decision_window"])

        # 6. Insufficient witnessed funding hours -> NO_GO.
        low_funding = evaluate_rth_ant_gate(
            _sample_ant_window(witnessed=8),
            role="monday_rth_oos",
            prereg_manifest=prereg,
            seal_receipt=seal,
            verified_prereg_sha256=prereg_sha,
            verified_segment_digests=segs,
            verified_engine_digests=engs,
        )
        self.assertEqual(low_funding["verdict"], "NO_GO")
        self.assertFalse(low_funding["provenance"]["checks"]["funding_hours_cover_decision_window"])

        # 7. Dual-source continuity failures (single-source dropout, >5m book gap, inconsistent seal coverage) -> NO_GO.
        xyz_dropout = evaluate_rth_ant_gate(
            _sample_ant_window(xyz_last_rx_ns=(MON_RTH_END_MS - 120_000) * 1_000_000),
            role="monday_rth_oos",
            prereg_manifest=prereg,
            seal_receipt=seal,
            verified_prereg_sha256=prereg_sha,
            verified_segment_digests=segs,
            verified_engine_digests=engs,
        )
        self.assertEqual(xyz_dropout["verdict"], "NO_GO")
        self.assertFalse(xyz_dropout["provenance"]["checks"]["dual_source_continuity_verified"])

        large_gap = evaluate_rth_ant_gate(
            _sample_ant_window(arc_max_gap_ns=301_000 * 1_000_000),
            role="monday_rth_oos",
            prereg_manifest=prereg,
            seal_receipt=seal,
            verified_prereg_sha256=prereg_sha,
            verified_segment_digests=segs,
            verified_engine_digests=engs,
        )
        self.assertEqual(large_gap["verdict"], "NO_GO")
        self.assertFalse(large_gap["provenance"]["checks"]["dual_source_continuity_verified"])

        mismatch_cov = evaluate_rth_ant_gate(
            good,
            role="monday_rth_oos",
            prereg_manifest=prereg,
            seal_receipt=_sample_seal_receipt(prereg_sha, arcus_book_events=50),
            verified_prereg_sha256=prereg_sha,
            verified_segment_digests=segs,
            verified_engine_digests=engs,
        )
        self.assertEqual(mismatch_cov["verdict"], "NO_GO")
        self.assertFalse(mismatch_cov["provenance"]["checks"]["dual_source_continuity_verified"])

        # 8. Segment mismatch or extra unsealed segment -> NO_GO.
        bad_seg = evaluate_rth_ant_gate(
            good,
            role="monday_rth_oos",
            prereg_manifest=prereg,
            seal_receipt=seal,
            verified_prereg_sha256=prereg_sha,
            verified_segment_digests={**segs, "tokyo-research-mon.seg001.ndjson.gz": "b" * 64},
            verified_engine_digests=engs,
        )
        self.assertEqual(bad_seg["verdict"], "NO_GO")
        self.assertFalse(bad_seg["provenance"]["checks"]["sealed_segment_hashes_verified"])

        # 9. Frozen engine hash mismatch -> NO_GO.
        bad_eng = evaluate_rth_ant_gate(
            good,
            role="monday_rth_oos",
            prereg_manifest=prereg,
            seal_receipt=seal,
            verified_prereg_sha256=prereg_sha,
            verified_segment_digests=segs,
            verified_engine_digests={**engs, FROZEN_ANT_ENGINE_FILES[0]: "0" * 64},
        )
        self.assertEqual(bad_eng["verdict"], "NO_GO")
        self.assertFalse(bad_eng["provenance"]["checks"]["frozen_engine_hashes_verified"])

    def test_rth_ant_cli_isolates_decision_window_and_requires_split_receipts(self) -> None:
        ms = 1_000_000
        sec = 1_000_000_000
        start_ns = MON_RTH_START_MS * ms
        end_ns = MON_RTH_END_MS * ms
        repo_root = Path(__file__).resolve().parent.parent
        eng_shas = {
            rel: hashlib.sha256((repo_root / rel).read_bytes()).hexdigest()
            for rel in FROZEN_ANT_ENGINE_FILES
        }

        def _msg(t_ns: int, source: str, market: str, payload: dict) -> dict:
            return {
                "event": "message",
                "source": source,
                "market": market,
                "capture_timing": {"available_wall_ns": t_ns},
                "payload": payload,
            }

        def _pair(t_ns: int, ab: str, aa: str, xb: str = "100.05", xa: str = "100.07") -> list[dict]:
            return [
                _msg(
                    t_ns,
                    "arcus",
                    "ANT-USD",
                    {
                        "channel": "l2Orderbook",
                        "id": "ANT-USD",
                        "contents": {"timestamp": t_ns // 1000, "bids": [[ab, "1000"]], "asks": [[aa, "1000"]]},
                    },
                ),
                _msg(
                    t_ns + ms,
                    "hyperliquid",
                    "xyz:ANT",
                    {
                        "channel": "l2Book",
                        "data": {"levels": [[{"px": xb, "sz": "1000"}], [{"px": xa, "sz": "1000"}]]},
                    },
                ),
            ]

        def _rate(t_ns: int) -> dict:
            return _msg(t_ns, "hyperliquid", "xyz:ANT", {"channel": "activeAssetCtx", "data": {"ctx": {"funding": "0"}}})

        def _opportunities(q_ns: int) -> list[dict]:
            rows: list[dict] = []
            for i in range(4):
                t = q_ns + i * 20 * sec
                rows.append(_rate(t - 2 * ms))
                for j in range(12):
                    rows.extend(_pair(t + j * 100 * ms, "99.90", "99.92"))
                for j in range(12):
                    rows.extend(_pair(t + (20 + j) * 100 * ms, "100.13", "100.15"))
            return rows

        quiet_rows: list[dict] = []
        for t in range(start_ns, end_ns + 1, 30 * sec):
            quiet_rows.append(_rate(t))
            quiet_rows.extend(_pair(t + ms, "99.99", "100.01", "99.99", "100.01"))

        def _run_case(root: Path, name: str, rows: list[dict], *, supply_seal: bool = True) -> tuple[int, dict]:
            cap = root / name
            cap.mkdir()
            seg = cap / "tokyo-research-mon.seg000.ndjson.gz"
            sorted_rows = sorted(rows, key=lambda r: r["capture_timing"]["available_wall_ns"])
            with gzip.open(seg, "wt") as f:
                for row in sorted_rows:
                    f.write(json.dumps(row) + "\n")
            prereg = _sample_ant_prereg(frozen_engine_sha256=eng_shas)
            prereg_path = root / f"{name}_prereg.json"
            prereg_path.write_text(json.dumps(prereg))
            prereg_sha = hashlib.sha256(prereg_path.read_bytes()).hexdigest()
            seg_sha = hashlib.sha256(seg.read_bytes()).hexdigest()
            arc_ev = sum(1 for r in sorted_rows if r["source"] == "arcus" and start_ns <= r["capture_timing"]["available_wall_ns"] < end_ns)
            xyz_ev = sum(
                1
                for r in sorted_rows
                if r["source"] == "hyperliquid"
                and r["payload"].get("channel") == "l2Book"
                and start_ns <= r["capture_timing"]["available_wall_ns"] < end_ns
            )
            seal = _sample_seal_receipt(
                prereg_sha,
                segment_sha256={seg.name: seg_sha},
                archive_first_rx_ns=sorted_rows[0]["capture_timing"]["available_wall_ns"],
                archive_last_rx_ns=sorted_rows[-1]["capture_timing"]["available_wall_ns"],
                arcus_book_events=arc_ev,
                xyz_book_events=xyz_ev,
            )
            seal_path = root / f"{name}_seal.json"
            seal_path.write_text(json.dumps(seal))
            out_path = root / f"{name}_out.json"
            argv = [
                "run_oos_validation_gate.py",
                "rth-ant",
                "--capture-dirs",
                str(cap),
                "--role",
                "monday_rth_oos",
                "--prereg-manifest",
                str(prereg_path),
                "--output",
                str(out_path),
            ]
            if supply_seal:
                argv.extend(["--seal-receipt", str(seal_path)])
            with patch.object(sys, "argv", argv), patch("sys.stdout", new_callable=io.StringIO):
                rc = cli_main()
            return rc, json.loads(out_path.read_text())

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            # Without --seal-receipt -> fail-closed NO_GO even on positive opportunities.
            rc_no_seal, rep_no_seal = _run_case(root, "no_seal", [*_opportunities(start_ns + 103 * sec), *quiet_rows], supply_seal=False)
            self.assertEqual(rc_no_seal, 1)
            self.assertEqual(rep_no_seal["verdict"], "NO_GO")
            self.assertFalse(rep_no_seal["provenance"]["checks"]["seal_receipt_supplied"])

            # Codex 14:17 boundary test: quiet decision window vs added 4 pre-window cycles (-360s).
            rc_inside, rep_inside = _run_case(root, "inside", quiet_rows)
            rc_pre, rep_pre = _run_case(root, "pre_warmup", [*_opportunities(start_ns - 360 * sec), *quiet_rows])
            self.assertEqual(rc_inside, 1)
            self.assertEqual(rep_inside["verdict"], "NO_GO")
            self.assertEqual({v["cycles_closed"] for v in rep_inside["taker_variants"]}, {0})
            self.assertEqual(rc_pre, 1)
            self.assertEqual(rep_pre["verdict"], "NO_GO")
            self.assertEqual({v["cycles_closed"] for v in rep_pre["taker_variants"]}, {0})
            self.assertTrue(rep_pre["provenance"]["qualified"])

            # Positive in-window control -> GO across all latencies with full provenance verified.
            rc_pos, rep_pos = _run_case(root, "positive", [*_opportunities(start_ns + 103 * sec), *quiet_rows])
            self.assertEqual(rc_pos, 0)
            self.assertEqual(rep_pos["verdict"], "GO")
            self.assertEqual({v["cycles_closed"] for v in rep_pos["taker_variants"]}, {4})
            self.assertTrue(rep_pos["provenance"]["qualified"])

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

    def test_w15_rwa_gate_fails_closed_on_missing_fields_incomplete_markets_and_no_go_exit_code(self) -> None:
        missing_flat = _sample_rwa_summary(nvda_cash=-80.0, other_cash=-5.0)
        del missing_flat["markets"]["SPY"]["arms"]["CORE_HEDGE_SELF_PULL05_C310"]["terminal_flat_liq_ok"]
        with self.assertRaises(ValueError):
            evaluate_w15_rwa_gate(missing_flat)

        partial_markets = _sample_rwa_summary(nvda_cash=-80.0, other_cash=-5.0)
        del partial_markets["markets"]["TSLA"]
        with self.assertRaises(ValueError):
            evaluate_w15_rwa_gate(partial_markets)

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            go_json = root / "go.json"
            no_go_json = root / "nogo.json"
            out_json = root / "out.json"
            go_json.write_text(json.dumps(_sample_rwa_summary(nvda_cash=-80.0, other_cash=-5.0)))
            no_go_json.write_text(json.dumps(_sample_rwa_summary(nvda_cash=-80.0, other_cash=-15.0)))

            with patch.object(sys, "argv", ["run_oos_validation_gate.py", "w15-rwa", "--rwa-summary-json", str(go_json), "--output", str(out_json)]):
                self.assertEqual(cli_main(), 0)
            with patch.object(sys, "argv", ["run_oos_validation_gate.py", "w15-rwa", "--rwa-summary-json", str(no_go_json), "--output", str(out_json)]):
                self.assertEqual(cli_main(), 1)

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
