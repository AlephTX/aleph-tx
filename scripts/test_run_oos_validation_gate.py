#!/usr/bin/env python3
"""Unit tests for `scripts/run_oos_validation_gate.py` (`Step 2`)."""

from __future__ import annotations

import base64
from datetime import datetime, timedelta, timezone
from decimal import Decimal
import gzip
import hashlib
import io
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch
from zoneinfo import ZoneInfo

from scripts.rth_ant_prereg_tool import create_witness_query, wrap_witness_receipt
from scripts.run_oos_validation_gate import (
    ANT_STRATEGY_CONTRACT,
    FROZEN_ANT_ENGINE_FILES,
    TEN_HOURS_MS,
    evaluate_rth_ant_gate,
    evaluate_w15_rwa_gate,
    main as cli_main,
    verify_rfc3161_witness,
    verify_w17_crash_pause_prereg,
)
from scripts.sim_lighter_core_rwa_maker import (
    build_formal_replay_receipt,
    main as sim_rwa_cli_main,
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
        "strategy_contract": dict(ANT_STRATEGY_CONTRACT),
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
    for market in markets.values():
        for arm in market["arms"].values():
            for field in ("core_maker_usd", "terminal_liq_cash_usd"):
                arm[field + "_exact"] = str(arm[field])
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
    def test_oos_does_not_execute_engine_with_mismatched_or_missing_pin(self):
        from scripts.run_oos_validation_gate import run_rth_ant_cli
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            path = root / FROZEN_ANT_ENGINE_FILES[0]
            path.parent.mkdir()
            plan = root / "plan.json"
            plan.write_text(json.dumps(_sample_ant_prereg()))
            for present in (False, True):
                if present:
                    path.write_text("raise AssertionError('unverified engine executed')")
                out = root / "result.json"
                self.assertEqual(run_rth_ant_cli([], "monday_rth_oos", out, root, plan), 1)
                self.assertEqual(json.loads(out.read_text())["verdict"], "NO_GO")

    def test_replay_uses_executable_cap_at_arm_and_persistence_wakeup(self):
        from scripts.ant_rth_replay import AblationSim, AblationSpec, MS_NS
        for bp, admitted in [(34, True), (36, False), (48, False)]:
            sim = AblationSim([AblationSpec("synthetic", "TAKER_TAKER", 100, 500, 10)])
            t = 1000 * MS_NS
            sim.on_arc_seen_book(t, [(99.99, 10)], [(100, 10)])
            sim.on_xyz_book(t, [(100 + bp / 100, 10)], [(100.01 + bp / 100, 10)])
            self.assertEqual(sim.arms[0].pend_action, admitted)
        sim = AblationSim([AblationSpec("synthetic", "TAKER_TAKER", 100, 500, 10, persist_ms=100)])
        sim.on_arc_seen_book(t, [(99.99, 10)], [(100, 10)])
        sim.on_xyz_book(t, [(100.34, 10)], [(100.35, 10)])
        self.assertTrue(sim.arms[0].armed_since_ns)
        sim.on_arc_seen_book(t + 50 * MS_NS, [(99.5, 10)], [(99.9, 10)])
        sim.drain(t + 100 * MS_NS)
        self.assertFalse(sim.arms[0].armed_since_ns)
        self.assertFalse(sim.arms[0].pend_action)

    def test_monday_role_rejects_changed_contract_and_weekday_override(self):
        for offset in (0, 86_400_000):
            start, end = MON_RTH_START_MS + offset, MON_RTH_END_MS + offset
            plan = _sample_ant_prereg(frozen_at_ms=start-1000, decision_start_ms=start,
                decision_end_ms=end, expected_rth_date_et="2026-11-24" if offset else "2026-11-23")
            if offset:
                plan["require_monday"] = False
            else:
                plan["strategy_contract"]["entry_bp"] = 999
            digest = _sample_prereg_sha(plan)
            report = evaluate_rth_ant_gate(_sample_ant_window(start_ms=start,end_ms=end), role="monday_rth_oos",
                prereg_manifest=plan, verified_prereg_sha256=digest, verified_segment_digests=_sample_seg_digests(),
                verified_engine_digests=_sample_eng_digests(), seal_receipt=_sample_seal_receipt(digest,
                    sealed_at_ms=end+1000, archive_first_rx_ns=start*1_000_000, archive_last_rx_ns=end*1_000_000))
            self.assertEqual(report["verdict"], "NO_GO")
            self.assertFalse(report["provenance"]["checks"]["us_rth_weekday_session_covered" if offset else "strategy_contract_matches"])

    def test_current_rwa_producer_preserves_pre_rounding_decimals(self):
        from decimal import Decimal
        from scripts.sim_lighter_core_rwa_maker import CoreMakerSim, CORE_SPECS, summarize_sim
        sim = CoreMakerSim(tuple(CORE_SPECS))
        for market in sim.mk.values():
            for arm in market.arms:
                arm.cash_maker = Decimal("-0.20004")
                arm.vol_maker_usd = Decimal("10000")
        summary = summarize_sim(sim, Path("synthetic"), 0, 0)
        row = summary["markets"]["SPY"]["arms"]["CORE_HEDGE_SELF_PULL05_C310"]
        self.assertEqual(row["terminal_liq_cash_usd"], -0.2)
        self.assertEqual(Decimal(row["terminal_liq_cash_usd_exact"]), Decimal("-0.20004"))
        self.assertEqual(evaluate_w15_rwa_gate(summary)["verdict"], "NO_GO")

    def test_w15_rounding_can_hide_over_budget_cost(self):
        summary = _sample_rwa_summary(nvda_cash=-20,other_cash=-10)
        for market in summary["markets"].values():
            for arm in market["arms"].values():
                arm.update(core_maker_usd=10000, terminal_liq_cash_usd=-0.2)
                arm.pop("core_maker_usd_exact")
                arm.pop("terminal_liq_cash_usd_exact")
        report = evaluate_w15_rwa_gate(summary)
        self.assertEqual(report["verdict"], "NO_GO")
        self.assertEqual(report["arms"]["CORE_HEDGE_SELF_PULL05_C310"]["ex_nvda"]["cost_evidence"], "insufficient_precision")
        for market in summary["markets"].values():
            for arm in market["arms"].values():
                arm.update(core_maker_usd_exact="10000", terminal_liq_cash_usd_exact="-0.20004")
        report = evaluate_w15_rwa_gate(summary)
        self.assertEqual(report["verdict"], "NO_GO")
        self.assertEqual(report["arms"]["CORE_HEDGE_SELF_PULL05_C310"]["ex_nvda"]["cost_evidence"], "above_or_no_volume")

    def test_rth_ant_gate_enforces_oos_role_and_failure_conditions(self) -> None:
        good = _sample_ant_window()
        prereg = _sample_ant_prereg()
        prereg_sha = _sample_prereg_sha(prereg)
        seal = _sample_seal_receipt(prereg_sha)
        segs = _sample_seg_digests()
        engs = _sample_eng_digests()

        no_witness = evaluate_rth_ant_gate(
            good,
            role="monday_rth_oos",
            prereg_manifest=prereg,
            seal_receipt=seal,
            verified_prereg_sha256=prereg_sha,
            verified_segment_digests=segs,
            verified_engine_digests=engs,
        )
        self.assertEqual(no_witness["verdict"], "NO_GO")
        self.assertTrue(no_witness["passed_metrics"])
        self.assertTrue(no_witness["provenance"]["structural_checks_passed"])
        self.assertFalse(no_witness["provenance"]["qualified"])
        self.assertFalse(no_witness["provenance"]["checks"]["independent_preregistration_witness_verified"])
        self.assertEqual(
            no_witness["provenance"]["pre_registration_clock_trust"],
            "self_asserted_json_only_requires_external_witness",
        )

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
        self.assertEqual(
            no_manifest["provenance"]["pre_registration_clock_trust"],
            "self_asserted_json_only_requires_external_witness",
        )

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
            self.assertTrue(rep_pre["provenance"]["structural_checks_passed"])

            # Positive in-window control has complete structure and passes economics.
            rc_pos, rep_pos = _run_case(root, "positive", [*_opportunities(start_ns + 103 * sec), *quiet_rows])
            self.assertEqual(rc_pos, 1)
            self.assertEqual(rep_pos["verdict"], "NO_GO")
            self.assertEqual({v["cycles_closed"] for v in rep_pos["taker_variants"]}, {4})
            self.assertTrue(rep_pos["passed_metrics"])
            self.assertTrue(rep_pos["provenance"]["structural_checks_passed"])
            self.assertFalse(rep_pos["provenance"]["qualified"])

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

            with patch.object(sys, "argv", ["run_oos_validation_gate.py", "w15-rwa", "--role", "diagnostic", "--rwa-summary-json", str(go_json), "--output", str(out_json)]):
                self.assertEqual(cli_main(), 0)
                self.assertEqual(json.loads(out_json.read_text())["verdict"], "DIAGNOSTIC_ONLY")
            with patch.object(sys, "argv", ["run_oos_validation_gate.py", "w15-rwa", "--role", "diagnostic", "--rwa-summary-json", str(no_go_json), "--output", str(out_json)]):
                self.assertEqual(cli_main(), 1)

    def test_w15_rwa_never_promotes_legacy_summary_to_formal_admission(self) -> None:
        summary = _sample_rwa_summary(nvda_cash=-80.0, other_cash=-5.0)
        summary.update(role="historical_diagnostic", window="w14", decision_start_ms=0,
                       decision_end_ms=600_000, segments_sha256={})
        with tempfile.TemporaryDirectory() as tmp:
            source, output = Path(tmp) / "summary.json", Path(tmp) / "report.json"
            source.write_text(json.dumps(summary))
            common = ["run_oos_validation_gate.py", "w15-rwa", "--rwa-summary-json", str(source), "--output", str(output)]
            for extra, code, verdict, threshold in (
                ([], 2, "FORMAL_WITHHELD", 20.0),
                (["--role", "formal", "--max-cost-per-million", "20.01"], 2, "FORMAL_WITHHELD", 20.01),
                (["--role", "diagnostic", "--max-cost-per-million", "20.01"], 0, "DIAGNOSTIC_ONLY", 20.01),
            ):
                with self.subTest(extra=extra), patch.object(sys, "argv", common + extra), patch("builtins.print") as printer:
                    self.assertEqual(cli_main(), code)
                    result = json.loads(output.read_text())
                    self.assertEqual(result["verdict"], verdict)
                    self.assertIs(result["formal_admission"], False)
                    self.assertEqual(result["max_cost_per_million_usd"], threshold)
                    self.assertEqual(result["input_sha256"], hashlib.sha256(source.read_bytes()).hexdigest())
                    self.assertEqual(json.loads(printer.call_args.args[0]), {k: v for k, v in result.items() if k != "input_json"})

    def test_w15_rwa_applies_exact_cost_threshold_before_display_rounding(self) -> None:
        for market_cash, expected in (
            (-9.999, "GO_UNHEDGED_ONLY"),
            (-10.0, "GO_UNHEDGED_ONLY"),
            (-10.002, "NO_GO"),
        ):
            with self.subTest(market_cash=market_cash):
                result = evaluate_w15_rwa_gate(_sample_rwa_summary(nvda_cash=-80.0, other_cash=market_cash))
                self.assertEqual(result["verdict"], expected)
                self.assertEqual(result["arms"]["CORE_PURE_PULL05_C310"]["ex_nvda"]["core_usd_per_m_liq"], 20.0)

    def test_w15_rwa_requires_finite_and_plausible_evidence(self) -> None:
        for field, value in (
            ("max_drawdown_usd", float("nan")),
            ("max_drawdown_usd", -1.0),
            ("core_maker_usd", float("inf")),
            ("dual_usd", -1.0),
            ("terminal_liq_cash_usd", float("nan")),
        ):
            with self.subTest(field=field, value=value):
                summary = _sample_rwa_summary(nvda_cash=-80.0, other_cash=-3.0)
                summary["markets"]["SPY"]["arms"]["CORE_HEDGE_SELF_PULL05_C310"][field] = value
                with self.assertRaises(ValueError):
                    evaluate_w15_rwa_gate(summary)
        for bad_max in (True, float("nan"), 0.0, -5.0):
            with self.subTest(bad_max=bad_max):
                with self.assertRaises(ValueError):
                    evaluate_w15_rwa_gate(_sample_rwa_summary(nvda_cash=-80.0, other_cash=-3.0), max_cost_per_million=bad_max)

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

    def _create_ephemeral_tsa(self, root: Path, *, name: str = "tsa") -> tuple[Path, Path]:
        ca_key = root / f"{name}_ca.key"
        ca_cert = root / f"{name}_ca.pem"
        leaf_key = root / f"{name}_leaf.key"
        leaf_csr = root / f"{name}_leaf.csr"
        leaf_cert = root / f"{name}_leaf.pem"
        ext_cnf = root / f"{name}_ext.cnf"
        ts_cnf = root / f"{name}_ts.cnf"
        serial_file = root / f"{name}_serial"
        serial_file.write_text("01\n", encoding="utf-8")

        ext_cnf.write_text(
            "[tsa_ext]\n"
            "basicConstraints = critical,CA:FALSE\n"
            "keyUsage = critical,digitalSignature\n"
            "extendedKeyUsage = critical,timeStamping\n",
            encoding="utf-8",
        )
        ts_cnf.write_text(
            "[tsa]\n"
            "default_tsa = tsa_config1\n\n"
            "[tsa_config1]\n"
            f"dir = {root}\n"
            f"serial = {serial_file}\n"
            f"signer_cert = {leaf_cert}\n"
            f"signer_key = {leaf_key}\n"
            "default_policy = 1.2.3.4.1\n"
            "digests = sha256\n"
            "clock_precision_digits = 0\n"
            "ordering = yes\n"
            "signer_digest = sha256\n",
            encoding="utf-8",
        )
        subprocess.run(
            [
                "openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes",
                "-keyout", str(ca_key), "-out", str(ca_cert), "-days", "30",
                "-subj", f"/CN=AlephTestCA-{name}",
            ],
            check=True,
            capture_output=True,
        )
        subprocess.run(
            [
                "openssl", "req", "-new", "-newkey", "rsa:2048", "-nodes",
                "-keyout", str(leaf_key), "-out", str(leaf_csr),
                "-subj", f"/CN=AlephTestTSA-{name}",
            ],
            check=True,
            capture_output=True,
        )
        subprocess.run(
            [
                "openssl", "x509", "-req", "-in", str(leaf_csr),
                "-CA", str(ca_cert), "-CAkey", str(ca_key), "-CAcreateserial",
                "-out", str(leaf_cert), "-days", "30",
                "-extfile", str(ext_cnf), "-extensions", "tsa_ext",
            ],
            check=True,
            capture_output=True,
        )
        return ca_cert, ts_cnf

    def _issue_tsr(self, ts_cnf: Path, tsq_path: Path, tsr_path: Path) -> None:
        subprocess.run(
            [
                "openssl", "ts", "-reply",
                "-config", str(ts_cnf),
                "-queryfile", str(tsq_path),
                "-out", str(tsr_path),
            ],
            check=True,
            capture_output=True,
        )

    def test_verify_rfc3161_witness_enforces_ca_digest_base64_and_timestamp_bounds(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            ca_cert, ts_cnf = self._create_ephemeral_tsa(root, name="primary")
            wrong_ca_cert, _ = self._create_ephemeral_tsa(root, name="wrong")

            manifest = _sample_ant_prereg()
            manifest_path = root / "prereg.json"
            manifest_path.write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8")
            manifest_sha = hashlib.sha256(manifest_path.read_bytes()).hexdigest()

            tsq_path = root / "req.tsq"
            tsr_path = root / "resp.tsr"
            witness_path = root / "witness.json"
            create_witness_query(prereg_manifest_path=manifest_path, output=tsq_path)
            self._issue_tsr(ts_cnf, tsq_path, tsr_path)
            wrap_witness_receipt(prereg_manifest_path=manifest_path, tsr_path=tsr_path, output=witness_path)
            witness = json.loads(witness_path.read_text(encoding="utf-8"))

            ok, ts_ms, detail = verify_rfc3161_witness(
                witness,
                expected_prereg_sha256=manifest_sha,
                decision_start_ms=4_102_444_800_000,  # 2100-01-01 UTC
                trusted_ca_path=ca_cert,
            )
            self.assertTrue(ok, detail)
            self.assertIsInstance(ts_ms, int)
            self.assertEqual(detail, "verified_rfc3161_tsa_witness")

            # Boundary equality witness_time_ms == decision_start_ms passes; -1 fails.
            ok_eq, ts_eq, _ = verify_rfc3161_witness(
                witness,
                expected_prereg_sha256=manifest_sha,
                decision_start_ms=ts_ms,
                trusted_ca_path=ca_cert,
            )
            self.assertTrue(ok_eq)
            self.assertEqual(ts_eq, ts_ms)

            ok_late, ts_late, late_reason = verify_rfc3161_witness(
                witness,
                expected_prereg_sha256=manifest_sha,
                decision_start_ms=ts_ms - 1,
                trusted_ca_path=ca_cert,
            )
            self.assertFalse(ok_late)
            self.assertIsNone(ts_late)
            self.assertIn("witness_after_decision_start", late_reason)

            # Fail closed on wrong CA, missing CA, embedded CA bypass attempt, tampered digest, bad base64/schema.
            for label, w_arg, exp_sha, ca_arg in (
                ("wrong_ca", witness, manifest_sha, wrong_ca_cert),
                ("missing_ca", witness, manifest_sha, None),
                ("nonexistent_ca", witness, manifest_sha, root / "does_not_exist.pem"),
                ("embedded_ca_ignored", {**witness, "trusted_ca_pem": ca_cert.read_text()}, manifest_sha, wrong_ca_cert),
                ("tampered_expected_sha", witness, "f" * 64, ca_cert),
                ("tampered_witness_sha", {**witness, "prereg_manifest_sha256": "f" * 64}, manifest_sha, ca_cert),
                ("bad_schema", {**witness, "schema": "wrong_schema"}, manifest_sha, ca_cert),
                ("bad_provider", {**witness, "provider": "git_commit"}, manifest_sha, ca_cert),
                ("bad_base64_whitespace", {**witness, "tsr_base64": witness["tsr_base64"] + "\n"}, manifest_sha, ca_cert),
                ("empty_base64", {**witness, "tsr_base64": ""}, manifest_sha, ca_cert),
                ("corrupt_tsr", {**witness, "tsr_base64": base64.b64encode(b"not-a-tsr").decode("ascii")}, manifest_sha, ca_cert),
                ("none_witness", None, manifest_sha, ca_cert),
            ):
                with self.subTest(case=label):
                    res_ok, res_ts, _ = verify_rfc3161_witness(
                        w_arg,
                        expected_prereg_sha256=exp_sha,
                        decision_start_ms=4_102_444_800_000,
                        trusted_ca_path=ca_arg,
                    )
                    self.assertFalse(res_ok)
                    self.assertIsNone(res_ts)

    def test_rth_ant_gate_rfc3161_witness_go_and_fail_closed_variants(self) -> None:
        now_et = datetime.now(timezone.utc).astimezone(ET_TZ)
        next_mon = (now_et + timedelta(days=(7 - now_et.weekday()) % 7 or 7)).date()
        prev_mon = (now_et - timedelta(days=now_et.weekday() or 7)).date()
        fut_start_ms = int(datetime(next_mon.year, next_mon.month, next_mon.day, 7, 0, tzinfo=ET_TZ).timestamp() * 1000)
        fut_end_ms = fut_start_ms + TEN_HOURS_MS
        past_start_ms = int(datetime(prev_mon.year, prev_mon.month, prev_mon.day, 7, 0, tzinfo=ET_TZ).timestamp() * 1000)
        past_end_ms = past_start_ms + TEN_HOURS_MS

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            ca_cert, ts_cnf = self._create_ephemeral_tsa(root, name="gate")
            wrong_ca_cert, _ = self._create_ephemeral_tsa(root, name="gate_wrong")

            fut_prereg = _sample_ant_prereg(
                frozen_at_ms=fut_start_ms - 60_000,
                decision_start_ms=fut_start_ms,
                decision_end_ms=fut_end_ms,
                expected_rth_date_et=next_mon.isoformat(),
            )
            fut_path = root / "fut_prereg.json"
            fut_path.write_text(json.dumps(fut_prereg, sort_keys=True) + "\n", encoding="utf-8")
            fut_sha = hashlib.sha256(fut_path.read_bytes()).hexdigest()
            fut_tsq = root / "fut.tsq"
            fut_tsr = root / "fut.tsr"
            fut_wit_path = root / "fut_witness.json"
            create_witness_query(prereg_manifest_path=fut_path, output=fut_tsq)
            self._issue_tsr(ts_cnf, fut_tsq, fut_tsr)
            wrap_witness_receipt(prereg_manifest_path=fut_path, tsr_path=fut_tsr, output=fut_wit_path)
            fut_wit = json.loads(fut_wit_path.read_text(encoding="utf-8"))

            good_fut = _sample_ant_window(start_ms=fut_start_ms, end_ms=fut_end_ms)
            fut_seal = _sample_seal_receipt(
                fut_sha,
                sealed_at_ms=fut_end_ms + 60_000,
                archive_first_rx_ns=fut_start_ms * 1_000_000,
                archive_last_rx_ns=fut_end_ms * 1_000_000,
            )

            go_rep = evaluate_rth_ant_gate(
                good_fut,
                role="monday_rth_oos",
                prereg_manifest=fut_prereg,
                seal_receipt=fut_seal,
                verified_prereg_sha256=fut_sha,
                verified_segment_digests=_sample_seg_digests(),
                verified_engine_digests=_sample_eng_digests(),
                external_witness=fut_wit,
                trusted_tsa_ca_path=ca_cert,
            )
            self.assertEqual(go_rep["verdict"], "GO")
            self.assertTrue(go_rep["provenance"]["qualified"])
            self.assertTrue(go_rep["provenance"]["checks"]["independent_preregistration_witness_verified"])
            self.assertEqual(go_rep["provenance"]["pre_registration_clock_trust"], "verified_rfc3161_tsa_witness")
            self.assertIsNotNone(go_rep["provenance"]["witness_time_ms"])
            self.assertLessEqual(go_rep["provenance"]["witness_time_ms"], fut_start_ms)

            # Wrong CA -> NO_GO
            bad_ca_rep = evaluate_rth_ant_gate(
                good_fut,
                role="monday_rth_oos",
                prereg_manifest=fut_prereg,
                seal_receipt=fut_seal,
                verified_prereg_sha256=fut_sha,
                verified_segment_digests=_sample_seg_digests(),
                verified_engine_digests=_sample_eng_digests(),
                external_witness=fut_wit,
                trusted_tsa_ca_path=wrong_ca_cert,
            )
            self.assertEqual(bad_ca_rep["verdict"], "NO_GO")
            self.assertFalse(bad_ca_rep["provenance"]["qualified"])
            self.assertFalse(bad_ca_rep["provenance"]["checks"]["independent_preregistration_witness_verified"])
            self.assertIsNone(bad_ca_rep["provenance"]["witness_time_ms"])
            self.assertEqual(
                bad_ca_rep["provenance"]["pre_registration_clock_trust"],
                "self_asserted_json_only_requires_external_witness",
            )

            # Tampered manifest digest -> NO_GO
            tampered_sha = hashlib.sha256(b"tampered").hexdigest()
            tampered_rep = evaluate_rth_ant_gate(
                good_fut,
                role="monday_rth_oos",
                prereg_manifest=fut_prereg,
                seal_receipt=_sample_seal_receipt(
                    tampered_sha,
                    sealed_at_ms=fut_end_ms + 60_000,
                    archive_first_rx_ns=fut_start_ms * 1_000_000,
                    archive_last_rx_ns=fut_end_ms * 1_000_000,
                ),
                verified_prereg_sha256=tampered_sha,
                verified_segment_digests=_sample_seg_digests(),
                verified_engine_digests=_sample_eng_digests(),
                external_witness=fut_wit,
                trusted_tsa_ca_path=ca_cert,
            )
            self.assertEqual(tampered_rep["verdict"], "NO_GO")
            self.assertFalse(tampered_rep["provenance"]["checks"]["independent_preregistration_witness_verified"])
            self.assertIsNone(tampered_rep["provenance"]["witness_time_ms"])

            # Post-window TSA timestamp (witness_time > decision_start) -> NO_GO
            past_prereg = _sample_ant_prereg(
                frozen_at_ms=past_start_ms - 60_000,
                decision_start_ms=past_start_ms,
                decision_end_ms=past_end_ms,
                expected_rth_date_et=prev_mon.isoformat(),
            )
            past_path = root / "past_prereg.json"
            past_path.write_text(json.dumps(past_prereg, sort_keys=True) + "\n", encoding="utf-8")
            past_sha = hashlib.sha256(past_path.read_bytes()).hexdigest()
            past_tsq = root / "past.tsq"
            past_tsr = root / "past.tsr"
            past_wit_path = root / "past_witness.json"
            create_witness_query(prereg_manifest_path=past_path, output=past_tsq)
            self._issue_tsr(ts_cnf, past_tsq, past_tsr)
            wrap_witness_receipt(prereg_manifest_path=past_path, tsr_path=past_tsr, output=past_wit_path)
            past_wit = json.loads(past_wit_path.read_text(encoding="utf-8"))

            late_rep = evaluate_rth_ant_gate(
                _sample_ant_window(start_ms=past_start_ms, end_ms=past_end_ms),
                role="monday_rth_oos",
                prereg_manifest=past_prereg,
                seal_receipt=_sample_seal_receipt(
                    past_sha,
                    sealed_at_ms=past_end_ms + 60_000,
                    archive_first_rx_ns=past_start_ms * 1_000_000,
                    archive_last_rx_ns=past_end_ms * 1_000_000,
                ),
                verified_prereg_sha256=past_sha,
                verified_segment_digests=_sample_seg_digests(),
                verified_engine_digests=_sample_eng_digests(),
                external_witness=past_wit,
                trusted_tsa_ca_path=ca_cert,
            )
            self.assertEqual(late_rep["verdict"], "NO_GO")
            self.assertTrue(late_rep["provenance"]["structural_checks_passed"])
            self.assertFalse(late_rep["provenance"]["qualified"])
            self.assertFalse(late_rep["provenance"]["checks"]["independent_preregistration_witness_verified"])
            self.assertIsNone(late_rep["provenance"]["witness_time_ms"])

    def test_w15_rwa_formal_replay_receipt_end_to_end_and_tamper_checks(self) -> None:
        repo_root = Path(__file__).resolve().parent.parent
        start_ms = MON_RTH_START_MS
        end_ms = MON_RTH_END_MS
        ms_ns = 1_000_000
        start_ns = start_ms * ms_ns
        end_ns = end_ms * ms_ns

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            cap_dir = root / "capture_w15"
            cap_dir.mkdir()
            seg_path = cap_dir / "tokyo-research-w15.seg000.ndjson.gz"
            events = [
                # Pre-window SPY book that must be filtered out
                {
                    "event": "message",
                    "source": "lighter",
                    "capture_timing": {"available_wall_ns": start_ns - ms_ns, "connection_epoch": 1},
                    "payload": {
                        "channel": "order_book:128",
                        "type": "subscribed/order_book",
                        "order_book": {"bids": [{"price": "500.00", "size": "10"}], "asks": [{"price": "500.10", "size": "10"}]},
                    },
                },
                # In-window SPY book
                {
                    "event": "message",
                    "source": "lighter",
                    "capture_timing": {"available_wall_ns": start_ns, "connection_epoch": 1},
                    "payload": {
                        "channel": "order_book:128",
                        "type": "subscribed/order_book",
                        "order_book": {"bids": [{"price": "500.00", "size": "10"}], "asks": [{"price": "500.10", "size": "10"}]},
                    },
                },
                # Post-window boundary event at exact end_ns (half-open excludes end_ns)
                {
                    "event": "message",
                    "source": "lighter",
                    "capture_timing": {"available_wall_ns": end_ns, "connection_epoch": 1},
                    "payload": {
                        "channel": "order_book:128",
                        "type": "update/order_book",
                        "order_book": {"bids": [{"price": "500.01", "size": "10"}], "asks": [{"price": "500.11", "size": "10"}]},
                    },
                },
            ]
            with gzip.open(seg_path, "wt", encoding="utf-8") as handle:
                for ev in events:
                    handle.write(json.dumps(ev) + "\n")

            sim_out = root / "sim_bounded.json"
            with patch("sys.stdout", new_callable=io.StringIO):
                sim_rc = sim_rwa_cli_main([
                    "--input", str(seg_path),
                    "--output", str(sim_out),
                    "--decision-start-ms", str(start_ms),
                    "--decision-end-ms", str(end_ms),
                    "--emit-formal-receipt",
                ])
            self.assertEqual(sim_rc, 0)
            sim_res = json.loads(sim_out.read_text(encoding="utf-8"))
            self.assertEqual(sim_res["markets"]["SPY"]["core_books"], 1)
            self.assertIn("formal_replay_receipt", sim_res)
            self.assertEqual(sim_res["formal_replay_receipt"]["schema"], "aleph_tx_w15_rwa_replay_receipt_v1")

            # Build gate-passing 6-market summary with real receipt bound to seg_path + seal_receipt
            seg_sha = hashlib.sha256(seg_path.read_bytes()).hexdigest()
            seal_path = root / "seal_receipt.json"
            seal_path.write_text(
                json.dumps({"segment_sha256": {f"{cap_dir.name}/{seg_path.name}": seg_sha}}, indent=2),
                encoding="utf-8",
            )

            summary = _sample_rwa_summary(nvda_cash=-80.0, other_cash=-5.0)
            summary["formal_replay_receipt"] = build_formal_replay_receipt(
                summary,
                input_path=seg_path,
                decision_start_ms=start_ms,
                decision_end_ms=end_ms,
                repo_root=repo_root,
            )
            summary_path = root / "rwa_formal_summary.json"
            summary_path.write_text(json.dumps(summary), encoding="utf-8")
            gate_out = root / "gate_out.json"

            with patch.object(
                sys,
                "argv",
                [
                    "run_oos_validation_gate.py",
                    "w15-rwa",
                    "--role",
                    "formal",
                    "--rwa-summary-json",
                    str(summary_path),
                    "--seal-receipt",
                    str(seal_path),
                    "--output",
                    str(gate_out),
                ],
            ), patch("sys.stdout", new_callable=io.StringIO):
                rc_ok = cli_main()
            self.assertEqual(rc_ok, 0)
            rep_ok = json.loads(gate_out.read_text(encoding="utf-8"))
            self.assertEqual(rep_ok["verdict"], "GO_HEDGED")
            self.assertIs(rep_ok["formal_admission"], True)
            self.assertIs(rep_ok["receipt_verified"], True)
            self.assertTrue(all(rep_ok["receipt_checks"].values()))

            # Tamper payload, method hash, segment hash, and 10h window -> NO_GO (exit 1)
            for label, mutator, failed_check in (
                (
                    "tampered_payload",
                    lambda s: s["markets"]["SPY"]["arms"]["CORE_HEDGE_SELF_PULL05_C310"].update({"terminal_liq_cash_usd": 1.0}),
                    "payload_sha256_verified",
                ),
                (
                    "tampered_method_hash",
                    lambda s: s["formal_replay_receipt"]["frozen_method_sha256"].update({"scripts/sim_lighter_core_rwa_maker.py": "0" * 64}),
                    "frozen_method_hashes_verified",
                ),
                (
                    "tampered_segment_hash",
                    lambda s: s["formal_replay_receipt"]["sealed_input_sha256"].update({seg_path.name: "0" * 64}),
                    "sealed_input_hashes_verified",
                ),
                (
                    "tampered_window_span",
                    lambda s: s["formal_replay_receipt"].update({"decision_end_ms": start_ms + 3_600_000}),
                    "exact_10h_decision_window",
                ),
            ):
                with self.subTest(case=label):
                    tampered = json.loads(json.dumps(summary))
                    mutator(tampered)
                    t_path = root / f"{label}.json"
                    t_path.write_text(json.dumps(tampered), encoding="utf-8")
                    with patch.object(
                        sys,
                        "argv",
                        [
                            "run_oos_validation_gate.py",
                            "w15-rwa",
                            "--role",
                            "formal",
                            "--rwa-summary-json",
                            str(t_path),
                            "--seal-receipt",
                            str(seal_path),
                            "--output",
                            str(gate_out),
                        ],
                    ), patch("sys.stdout", new_callable=io.StringIO):
                        rc_bad = cli_main()
                    self.assertEqual(rc_bad, 1)
                    rep_bad = json.loads(gate_out.read_text(encoding="utf-8"))
                    self.assertEqual(rep_bad["verdict"], "NO_GO")
                    self.assertIs(rep_bad["formal_admission"], False)
                    self.assertEqual(rep_bad["reason"], "invalid_formal_replay_receipt")
                    self.assertFalse(rep_bad["receipt_checks"][failed_check])

            # Legacy summary without formal_replay_receipt -> FORMAL_WITHHELD (exit 2)
            legacy = _sample_rwa_summary(nvda_cash=-80.0, other_cash=-5.0)
            legacy_path = root / "legacy.json"
            legacy_path.write_text(json.dumps(legacy), encoding="utf-8")
            with patch.object(
                sys,
                "argv",
                [
                    "run_oos_validation_gate.py",
                    "w15-rwa",
                    "--role",
                    "formal",
                    "--rwa-summary-json",
                    str(legacy_path),
                    "--seal-receipt",
                    str(seal_path),
                    "--output",
                    str(gate_out),
                ],
            ), patch("sys.stdout", new_callable=io.StringIO):
                rc_leg = cli_main()
            self.assertEqual(rc_leg, 2)
            rep_leg = json.loads(gate_out.read_text(encoding="utf-8"))
            self.assertEqual(rep_leg["verdict"], "FORMAL_WITHHELD")
            self.assertIs(rep_leg["formal_admission"], False)
            self.assertEqual(rep_leg["reason"], "missing_registered_cost_replay_receipt")

            # Valid formal receipt but threshold != 20.0 -> FORMAL_WITHHELD (exit 2)
            with patch.object(
                sys,
                "argv",
                [
                    "run_oos_validation_gate.py",
                    "w15-rwa",
                    "--role",
                    "formal",
                    "--max-cost-per-million",
                    "25.0",
                    "--rwa-summary-json",
                    str(summary_path),
                    "--seal-receipt",
                    str(seal_path),
                    "--output",
                    str(gate_out),
                ],
            ), patch("sys.stdout", new_callable=io.StringIO):
                rc_thr = cli_main()
            self.assertEqual(rc_thr, 2)
            self.assertEqual(json.loads(gate_out.read_text(encoding="utf-8"))["reason"], "formal_threshold_must_be_20")

            # Valid formal receipt with failing economics -> formal_admission=True, verdict=NO_GO, exit 1
            nogo_summary = _sample_rwa_summary(nvda_cash=-80.0, other_cash=-15.0)
            nogo_summary["formal_replay_receipt"] = build_formal_replay_receipt(
                nogo_summary,
                input_path=seg_path,
                decision_start_ms=start_ms,
                decision_end_ms=end_ms,
                repo_root=repo_root,
            )
            nogo_path = root / "rwa_formal_nogo.json"
            nogo_path.write_text(json.dumps(nogo_summary), encoding="utf-8")
            with patch.object(
                sys,
                "argv",
                [
                    "run_oos_validation_gate.py",
                    "w15-rwa",
                    "--role",
                    "formal",
                    "--rwa-summary-json",
                    str(nogo_path),
                    "--seal-receipt",
                    str(seal_path),
                    "--output",
                    str(gate_out),
                ],
            ), patch("sys.stdout", new_callable=io.StringIO):
                rc_nogo = cli_main()
            self.assertEqual(rc_nogo, 1)
            rep_nogo = json.loads(gate_out.read_text(encoding="utf-8"))
            self.assertEqual(rep_nogo["verdict"], "NO_GO")
            self.assertIs(rep_nogo["formal_admission"], True)
            self.assertIs(rep_nogo["receipt_verified"], True)

            # Ambiguous basename across two sealed dirs -> invalid_formal_replay_receipt (exit 1)
            ambig_seal = root / "ambig_seal.json"
            ambig_seal.write_text(
                json.dumps(
                    {
                        "segment_sha256": {
                            f"dir_a/{seg_path.name}": seg_sha,
                            f"dir_b/{seg_path.name}": seg_sha,
                        }
                    }
                ),
                encoding="utf-8",
            )
            with patch.object(
                sys,
                "argv",
                [
                    "run_oos_validation_gate.py",
                    "w15-rwa",
                    "--role",
                    "formal",
                    "--rwa-summary-json",
                    str(summary_path),
                    "--seal-receipt",
                    str(ambig_seal),
                    "--output",
                    str(gate_out),
                ],
            ), patch("sys.stdout", new_callable=io.StringIO):
                rc_ambig = cli_main()
            self.assertEqual(rc_ambig, 1)
            rep_ambig = json.loads(gate_out.read_text(encoding="utf-8"))
            self.assertEqual(rep_ambig["reason"], "invalid_formal_replay_receipt")
            self.assertFalse(rep_ambig["receipt_checks"]["sealed_input_hashes_verified"])

            # --emit-formal-receipt without bounds fails CLI argument validation
            for bad_args in (
                ["--input", str(seg_path), "--output", str(sim_out), "--emit-formal-receipt"],
                ["--input", str(seg_path), "--output", str(sim_out), "--decision-start-ms", str(start_ms)],
            ):
                with self.subTest(bad_args=bad_args), patch("sys.stderr", new_callable=io.StringIO), self.assertRaises(SystemExit) as ctx:
                    sim_rwa_cli_main(bad_args)
                self.assertEqual(ctx.exception.code, 2)


if __name__ == "__main__":
    unittest.main()
