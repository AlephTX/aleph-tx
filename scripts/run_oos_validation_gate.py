#!/usr/bin/env python3
"""Pre-registered Zero-Tuning OOS Validation Gate Runner (`Step 2`).

Supports three locked acceptance gates with zero post-freeze parameter tuning:
1. `rth-ant`: Evaluates frozen `EXP-ARB-A2` (`Q=$500, in=10bp, out=0bp, persist=100ms,
   lad=2,p=1, ddbrk=$50`) across `lat_ms in {100, 250, 500}` for Monday US RTH captures.
2. `w15-rwa`: Diagnoses Core Standard RWA cost on full and `ex-NVDA` baskets.
   Formal admission remains withheld until the producer supplies frozen-source evidence.
3. `w17-crash-pause-verify`: Verifies SHA-256 integrity of all frozen `W17+` artifacts
   against `crash_pause_abc_preregistration_v1_frozen.json` and fails closed on drift.
"""

from __future__ import annotations

import argparse
import base64
import binascii
from datetime import datetime, time as dt_time, timedelta, timezone
from decimal import Decimal, InvalidOperation
from fractions import Fraction
import hashlib
from types import ModuleType
import json
import math
from pathlib import Path
import re
import subprocess
import sys
import tempfile
from typing import Any
from zoneinfo import ZoneInfo

REQUIRED_TAKER_LATS = (100, 250, 500)
MAX_DRAWDOWN_BUDGET_USD = 50.0
MIN_CYCLES = 3
MIN_WIN_RATE = 0.60
HOUR_MS = 3_600_000
NS_PER_MS = 1_000_000
TEN_HOURS_MS = 36_000_000
MIN_WITNESSED_HOURS_10H = 9
CAPTURE_COVERAGE_TOLERANCE_MS = 60_000
MAX_SOURCE_BOOK_GAP_MS = 300_000
# Rust replay continuity bound (`DEFAULT_MAX_CONTINUOUS_EVAL_GAP_MS` in
# `src/strategy/pipeline_driver.rs`). New pre-registrations may opt into this tighter
# ceiling by declaring `continuity_schema_version >= 2`; frozen pre-registrations that
# predate the field keep the historical 300s ceiling so their verdicts never change.
RUST_ALIGNED_SOURCE_BOOK_GAP_MS = 60_000
CONTINUITY_SCHEMA_V2 = 2
SHA256_RE = re.compile(r"[0-9a-f]{64}\Z")
ET_TZ = ZoneInfo("America/New_York")
REPO_ROOT = Path(__file__).resolve().parent.parent
FROZEN_ANT_ENGINE_FILES = ("scripts/ant_rth_replay.py",)
EXTERNAL_PREREG_WITNESS_SCHEMA = "aleph_tx_external_prereg_witness_v1"
EXTERNAL_PREREG_WITNESS_PROVIDER = "rfc3161_tsa"
W15_RWA_REPLAY_RECEIPT_SCHEMA = "aleph_tx_w15_rwa_replay_receipt_v1"
FROZEN_RWA_METHOD_FILES = (
    "scripts/sim_lighter_core_rwa_maker.py",
    "scripts/maker_hedge_risk_guards.py",
)
_MONTH_MAP = {
    "Jan": 1, "Feb": 2, "Mar": 3, "Apr": 4, "May": 5, "Jun": 6,
    "Jul": 7, "Aug": 8, "Sep": 9, "Oct": 10, "Nov": 11, "Dec": 12,
}
_OPENSSL_TS_RE = re.compile(
    r"^(Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec)\s+(\d{1,2})\s+"
    r"(\d{2}):(\d{2}):(\d{2})(?:\.(\d+))?\s+(\d{4})\s+(GMT|UTC)\Z"
)
ANT_STRATEGY_CONTRACT = {
    "arm_family": "EXP-CRASH-A2B", "q_usd": 500.0, "entry_bp": 10.0, "exit_bp": 0.0,
    "max_entry_bp": 35.0, "persist_ms": 100, "post_trade_refresh_lock": True,
    "allow_batch_add": True, "floor_frac": 0.25, "scale_bps": 2.0, "convex_power": 1.0,
    "dd_brake_budget_usd": MAX_DRAWDOWN_BUDGET_USD, "max_hold_s": 7200,
    "required_taker_latencies_ms": list(REQUIRED_TAKER_LATS),
}


def _contract_matches(value: Any) -> bool:
    return json.dumps(value, sort_keys=True) == json.dumps(ANT_STRATEGY_CONTRACT, sort_keys=True)


def _is_valid_ms(value: Any) -> bool:
    return isinstance(value, int) and not isinstance(value, bool) and value >= 0


def _is_pos_int(value: Any) -> bool:
    return isinstance(value, int) and not isinstance(value, bool) and value > 0


def _is_sha256(value: Any) -> bool:
    return isinstance(value, str) and SHA256_RE.fullmatch(value) is not None


def _expected_interior_utc_hours(start_ms: int, end_ms: int) -> int:
    if end_ms <= start_ms:
        return 0
    first_hr = (start_ms // HOUR_MS + 1) * HOUR_MS
    last_hr = ((end_ms - 1) // HOUR_MS) * HOUR_MS
    if last_hr < first_hr:
        return 0
    return ((last_hr - first_hr) // HOUR_MS) + 1


def _covered_us_rth_dates(
    start_ms: int,
    end_ms: int,
    *,
    require_monday: bool = True,
    expected_rth_date_et: str | None = None,
) -> list[str]:
    if end_ms <= start_ms:
        return []
    start_et = datetime.fromtimestamp(start_ms / 1000.0, tz=timezone.utc).astimezone(ET_TZ)
    end_et = datetime.fromtimestamp(end_ms / 1000.0, tz=timezone.utc).astimezone(ET_TZ)
    covered: list[str] = []
    cur = start_et.date()
    end_date = end_et.date()
    while cur <= end_date:
        is_weekday = cur.weekday() < 5
        matches_day = (cur.weekday() == 0) if require_monday else is_weekday
        if expected_rth_date_et is not None:
            matches_day = matches_day and (cur.isoformat() == expected_rth_date_et)
        if matches_day:
            open_ms = int(datetime.combine(cur, dt_time(9, 30), tzinfo=ET_TZ).timestamp() * 1000)
            close_ms = int(datetime.combine(cur, dt_time(16, 0), tzinfo=ET_TZ).timestamp() * 1000)
            if start_ms <= open_ms and end_ms >= close_ms:
                covered.append(cur.isoformat())
        cur += timedelta(days=1)
    return covered


def _segment_hashes_match(
    expected_segments: Any,
    verified_segment_digests: dict[str, str] | None,
) -> bool:
    if not isinstance(expected_segments, dict) or not expected_segments:
        return False
    if not all(isinstance(k, str) and bool(k) and _is_sha256(v) for k, v in expected_segments.items()):
        return False
    if not isinstance(verified_segment_digests, dict):
        return False
    return verified_segment_digests == expected_segments


def _engine_hashes_match(
    expected_engines: Any,
    verified_engine_digests: dict[str, str] | None,
) -> bool:
    if not isinstance(expected_engines, dict) or not isinstance(verified_engine_digests, dict):
        return False
    if set(expected_engines) != set(FROZEN_ANT_ENGINE_FILES) or set(verified_engine_digests) != set(FROZEN_ANT_ENGINE_FILES):
        return False
    for rel_path in FROZEN_ANT_ENGINE_FILES:
        exp = expected_engines.get(rel_path)
        act = verified_engine_digests.get(rel_path)
        if not (_is_sha256(exp) and act == exp):
            return False
    return True


def _book_stream_ok(stream: Any, start_ms: int, end_ms: int, max_gap_ms: int) -> bool:
    if not isinstance(stream, dict):
        return False
    events = stream.get("events")
    first_rx_ns = stream.get("first_rx_ns")
    last_rx_ns = stream.get("last_rx_ns")
    max_gap_ns = stream.get("max_gap_ns")
    if not (_is_pos_int(events) and _is_pos_int(first_rx_ns) and _is_pos_int(last_rx_ns) and first_rx_ns <= last_rx_ns):
        return False
    if not (_is_valid_ms(max_gap_ns) and max_gap_ns <= max_gap_ms * NS_PER_MS):
        return False
    return (
        (first_rx_ns // NS_PER_MS) <= start_ms + CAPTURE_COVERAGE_TOLERANCE_MS
        and (last_rx_ns // NS_PER_MS) >= end_ms - CAPTURE_COVERAGE_TOLERANCE_MS
    )


def _verify_dual_source_continuity(
    prereg_manifest: dict[str, Any],
    *,
    window_result: dict[str, Any],
    seal_receipt: dict[str, Any] | None,
    start_ms: int,
    end_ms: int,
) -> bool:
    # Opt-in tightened ceiling for NEW pre-registrations only. A manifest that declares
    # `continuity_schema_version >= 2` is held to the Rust-aligned 60s bound; manifests
    # without the field (every historical frozen contract) keep the legacy 300s ceiling so
    # their acceptance is bit-for-bit unchanged. An unknown/invalid schema version fails closed.
    schema_version = prereg_manifest.get("continuity_schema_version")
    if schema_version is None:
        ceiling_ms = MAX_SOURCE_BOOK_GAP_MS
    elif schema_version == CONTINUITY_SCHEMA_V2:
        ceiling_ms = RUST_ALIGNED_SOURCE_BOOK_GAP_MS
    else:
        return False

    raw_cap = prereg_manifest.get("max_source_book_gap_ms")
    if raw_cap is None:
        max_gap_ms = ceiling_ms
    elif _is_pos_int(raw_cap) and raw_cap <= ceiling_ms:
        max_gap_ms = raw_cap
    else:
        return False

    continuity = window_result.get("source_continuity")
    if not isinstance(continuity, dict):
        return False
    arc_stream = continuity.get("arcus_book")
    xyz_stream = continuity.get("xyz_book")
    fund_stream = continuity.get("xyz_funding")
    if not (_book_stream_ok(arc_stream, start_ms, end_ms, max_gap_ms) and _book_stream_ok(xyz_stream, start_ms, end_ms, max_gap_ms)):
        return False
    if not (
        isinstance(fund_stream, dict)
        and _is_pos_int(fund_stream.get("events"))
        and _is_valid_ms(fund_stream.get("max_gap_ns"))
        and fund_stream["max_gap_ns"] <= HOUR_MS * NS_PER_MS
    ):
        return False

    gaps = window_result.get("gaps")
    if gaps is not None:
        if not isinstance(gaps, list):
            return False
        for item in gaps:
            if not isinstance(item, dict):
                return False
            gap_s = item.get("gap_s")
            if not isinstance(gap_s, (int, float)) or isinstance(gap_s, bool) or not (0.0 <= float(gap_s) * 1000.0 <= max_gap_ms):
                return False

    obs_first_ns = window_result.get("first_rx_ns")
    obs_last_ns = window_result.get("last_rx_ns")
    if not (_is_pos_int(obs_first_ns) and _is_pos_int(obs_last_ns) and obs_first_ns <= obs_last_ns):
        return False
    if (obs_first_ns // NS_PER_MS) > start_ms + CAPTURE_COVERAGE_TOLERANCE_MS or (obs_last_ns // NS_PER_MS) < end_ms - CAPTURE_COVERAGE_TOLERANCE_MS:
        return False

    if not isinstance(seal_receipt, dict):
        return False
    cov = seal_receipt.get("measured_source_coverage")
    if not isinstance(cov, dict):
        return False
    arch_first_ns = cov.get("archive_first_rx_ns")
    arch_last_ns = cov.get("archive_last_rx_ns")
    cov_arc = cov.get("arcus_book_events")
    cov_xyz = cov.get("xyz_book_events")
    if not (_is_pos_int(arch_first_ns) and _is_pos_int(arch_last_ns) and arch_first_ns <= arch_last_ns):
        return False
    if not (_is_pos_int(cov_arc) and _is_pos_int(cov_xyz)):
        return False
    return (
        arch_first_ns <= (start_ms + CAPTURE_COVERAGE_TOLERANCE_MS) * NS_PER_MS
        and arch_last_ns >= (end_ms - CAPTURE_COVERAGE_TOLERANCE_MS) * NS_PER_MS
        and arch_first_ns <= obs_first_ns
        and arch_last_ns >= obs_last_ns
        and cov_arc >= arc_stream["events"]
        and cov_xyz >= xyz_stream["events"]
    )


def _openssl_witness_bounds_ms(reply_text: str) -> tuple[int, int] | None:
    status_lines = [line.split("Status:", 1)[1].strip() for line in reply_text.splitlines() if line.startswith("Status:")]
    if len(status_lines) != 1 or not status_lines[0].startswith("Granted"):
        return None
    ts_lines = [line.split("Time stamp:", 1)[1].strip() for line in reply_text.splitlines() if line.startswith("Time stamp:")]
    if len(ts_lines) != 1:
        return None
    match = _OPENSSL_TS_RE.fullmatch(ts_lines[0])
    if match is None:
        return None
    mon_s, day_s, hr_s, min_s, sec_s, frac_s, year_s, _tz = match.groups()
    try:
        dt = datetime(
            int(year_s),
            _MONTH_MAP[mon_s],
            int(day_s),
            int(hr_s),
            int(min_s),
            int(sec_s),
            tzinfo=timezone.utc,
        )
    except ValueError:
        return None
    # RFC 3161 §2.4.2 defines the latest creation time as genTime + accuracy.
    # OpenSSL displays absent components as "unspecified" and present ones in hex.
    accuracy_rows = [line.removeprefix("Accuracy:").strip() for line in reply_text.splitlines()
                     if line.startswith("Accuracy:")]
    if len(accuracy_rows) != 1:
        return None
    component = r"(unspecified|0x[0-9a-fA-F]+)"
    match = re.fullmatch(rf"{component} seconds, {component} millis, {component} micros", accuracy_rows[0])
    if match is None or all(part == "unspecified" for part in match.groups()):
        return None
    sec, ms, us = (0 if part == "unspecified" else int(part, 16) for part in match.groups())
    if any(value > 999 or (value == 0 and part != "unspecified")
           for value, part in zip((ms, us), match.groups()[1:])):
        return None
    frac = frac_s or ""
    frac_us = int(frac[:6].ljust(6, "0"))
    # Preserve sub-microsecond precision at an integer-millisecond decision boundary.
    round_up_us = any(digit != "0" for digit in frac[6:])
    base_ms = int(dt.timestamp()) * 1000
    upper_us = frac_us + round_up_us + sec * 1_000_000 + ms * 1000 + us
    return base_ms + frac_us // 1000, base_ms + (upper_us + 999) // 1000


def verify_rfc3161_witness(
    witness: dict[str, Any] | None,
    *,
    expected_prereg_sha256: str,
    decision_start_ms: int,
    trusted_ca_path: Path | None,
) -> tuple[bool, int | None, str]:
    """Fail-closed RFC 3161 TSA verification against an explicit trusted CA file."""
    if not isinstance(witness, dict):
        return (False, None, "external_witness_not_supplied" if witness is None else "invalid_witness_object")
    if witness.get("schema") != EXTERNAL_PREREG_WITNESS_SCHEMA:
        return (False, None, "invalid_witness_schema")
    if witness.get("provider") != EXTERNAL_PREREG_WITNESS_PROVIDER:
        return (False, None, "invalid_witness_provider")
    if not _is_sha256(expected_prereg_sha256):
        return (False, None, "invalid_expected_prereg_sha256")
    if witness.get("prereg_manifest_sha256") != expected_prereg_sha256:
        return (False, None, "prereg_manifest_sha256_mismatch")
    if not _is_valid_ms(decision_start_ms):
        return (False, None, "invalid_decision_start_ms")
    if trusted_ca_path is None:
        return (False, None, "missing_trusted_tsa_ca")
    ca_path = Path(trusted_ca_path)
    if not ca_path.is_file():
        return (False, None, "missing_trusted_tsa_ca_file")

    tsr_b64 = witness.get("tsr_base64")
    if not isinstance(tsr_b64, str) or not tsr_b64:
        return (False, None, "missing_tsr_base64")
    try:
        tsr_bytes = base64.b64decode(tsr_b64, validate=True)
    except (binascii.Error, ValueError):
        return (False, None, "invalid_tsr_base64")
    if not tsr_bytes:
        return (False, None, "empty_tsr_bytes")

    with tempfile.TemporaryDirectory() as tmp_dir:
        tsr_path = Path(tmp_dir) / "witness.tsr"
        tsr_path.write_bytes(tsr_bytes)
        try:
            verify_proc = subprocess.run(
                [
                    "openssl",
                    "ts",
                    "-verify",
                    "-digest",
                    expected_prereg_sha256,
                    "-in",
                    str(tsr_path),
                    "-CAfile",
                    str(ca_path),
                ],
                capture_output=True,
                text=True,
                check=False,
                timeout=10,
            )
            if verify_proc.returncode != 0:
                return (False, None, "openssl_ts_verify_failed")
            text_proc = subprocess.run(
                ["openssl", "ts", "-reply", "-in", str(tsr_path), "-text"],
                capture_output=True,
                text=True,
                check=False,
                timeout=10,
            )
        except (subprocess.TimeoutExpired, OSError):
            return (False, None, "openssl_ts_exec_error")

    if text_proc.returncode != 0:
        return (False, None, "openssl_ts_reply_text_failed")
    bounds = _openssl_witness_bounds_ms(text_proc.stdout)
    if bounds is None:
        return (False, None, "unresolved_witness_timestamp_or_accuracy")
    witness_time_ms, upper_ms = bounds
    if upper_ms > decision_start_ms:
        return (False, None, f"witness_after_decision_start:{upper_ms}>{decision_start_ms}")
    return (True, witness_time_ms, "verified_rfc3161_tsa_witness")


def verify_rth_ant_provenance(
    prereg_manifest: dict[str, Any] | None,
    *,
    window_result: dict[str, Any],
    seal_receipt: dict[str, Any] | None = None,
    verified_prereg_sha256: str | None = None,
    verified_segment_digests: dict[str, str] | None = None,
    verified_engine_digests: dict[str, str] | None = None,
    external_witness: dict[str, Any] | None = None,
    trusted_tsa_ca_path: Path | None = None,
) -> dict[str, Any]:
    """Verify fail-closed split pre-capture/post-seal receipts, bounded 10h RTH replay, and continuity."""
    if not isinstance(prereg_manifest, dict):
        checks = {
            "prereg_manifest_supplied": False,
            "strategy_contract_matches": False,
            "seal_receipt_supplied": isinstance(seal_receipt, dict),
            "seal_links_prereg_sha256": False,
            "sealed_after_decision_end": False,
            "classification_is_monday_rth_oos": False,
            "pre_registered_before_decision_start": False,
            "independent_preregistration_witness_verified": False,
            "exact_10h_bounded_decision_window": False,
            "funding_hours_cover_decision_window": False,
            "us_rth_weekday_session_covered": False,
            "dual_source_continuity_verified": False,
            "sealed_segment_hashes_verified": False,
            "frozen_engine_hashes_verified": False,
        }
        return {
            "qualified": False,
            "structural_checks_passed": False,
            "pre_registration_clock_trust": "self_asserted_json_only_requires_external_witness",
            "witness_time_ms": None,
            "witness_verification_detail": "prereg_manifest_missing",
            "checks": checks,
            "covered_rth_dates_et": [],
            "decision_start_ms": None,
            "decision_end_ms": None,
            "frozen_at_ms": None,
            "sealed_at_ms": None,
        }

    prereg_supplied_ok = "segment_sha256" not in prereg_manifest
    seal_supplied_ok = isinstance(seal_receipt, dict)
    seal_links_ok = bool(
        seal_supplied_ok
        and _is_sha256(verified_prereg_sha256)
        and seal_receipt.get("prereg_manifest_sha256") == verified_prereg_sha256
    )

    frozen_at_ms = prereg_manifest.get("frozen_at_ms")
    start_ms = prereg_manifest.get("decision_start_ms")
    end_ms = prereg_manifest.get("decision_end_ms")
    valid_times = _is_valid_ms(frozen_at_ms) and _is_valid_ms(start_ms) and _is_valid_ms(end_ms)
    sealed_at_ms = seal_receipt.get("sealed_at_ms") if seal_supplied_ok else None
    sealed_after_end_ok = bool(valid_times and _is_valid_ms(sealed_at_ms) and sealed_at_ms >= end_ms)

    pre_reg_ok = bool(valid_times and frozen_at_ms <= start_ms < end_ms)
    expected_bounds = (start_ms * NS_PER_MS, end_ms * NS_PER_MS) if valid_times else None
    actual_bounds = window_result.get("decision_bounds_ns")
    bounds_matched = bool(
        expected_bounds is not None
        and (actual_bounds == list(expected_bounds) or actual_bounds == expected_bounds)
    )
    span_ok = bool(valid_times and (end_ms - start_ms) == TEN_HOURS_MS and bounds_matched)

    witnessed = int(window_result.get("funding_hours_witnessed", 0))
    missing = int(window_result.get("funding_hours_missing_witness", -1))
    min_req_hours = max(MIN_WITNESSED_HOURS_10H, _expected_interior_utc_hours(start_ms, end_ms) if valid_times else MIN_WITNESSED_HOURS_10H)
    funding_cov_ok = bool(valid_times and missing == 0 and witnessed >= min_req_hours)

    expected_date = prereg_manifest.get("expected_rth_date_et")
    rth_dates = (
        _covered_us_rth_dates(
            start_ms,
            end_ms,
            require_monday=True,
            expected_rth_date_et=expected_date if isinstance(expected_date, str) else None,
        )
        if valid_times
        else []
    )
    session_decl = prereg_manifest.get("market_session")
    session_ok = expected_date in rth_dates and session_decl == "us_equities_rth" and prereg_manifest.get("require_monday") is True

    continuity_ok = (
        _verify_dual_source_continuity(
            prereg_manifest,
            window_result=window_result,
            seal_receipt=seal_receipt,
            start_ms=start_ms,
            end_ms=end_ms,
        )
        if valid_times
        else False
    )

    checks = {
        "prereg_manifest_supplied": prereg_supplied_ok,
        "strategy_contract_matches": _contract_matches(prereg_manifest.get("strategy_contract")),
        "seal_receipt_supplied": seal_supplied_ok,
        "seal_links_prereg_sha256": seal_links_ok,
        "sealed_after_decision_end": sealed_after_end_ok,
        "classification_is_monday_rth_oos": prereg_manifest.get("classification") == "monday_rth_oos",
        "pre_registered_before_decision_start": pre_reg_ok,
        "exact_10h_bounded_decision_window": span_ok,
        "funding_hours_cover_decision_window": funding_cov_ok,
        "us_rth_weekday_session_covered": session_ok,
        "dual_source_continuity_verified": continuity_ok,
        "sealed_segment_hashes_verified": bool(
            seal_supplied_ok and _segment_hashes_match(seal_receipt.get("segment_sha256"), verified_segment_digests)
        ),
        "frozen_engine_hashes_verified": _engine_hashes_match(prereg_manifest.get("frozen_engine_sha256"), verified_engine_digests),
    }
    structural_checks_passed = all(checks.values())
    if external_witness is None:
        witness_ok, witness_time_ms, witness_detail = False, None, "external_witness_not_supplied"
    elif not (valid_times and _is_sha256(verified_prereg_sha256)):
        witness_ok, witness_time_ms, witness_detail = False, None, "missing_prereg_sha256_or_decision_start_ms"
    else:
        witness_ok, witness_time_ms, witness_detail = verify_rfc3161_witness(
            external_witness,
            expected_prereg_sha256=verified_prereg_sha256,
            decision_start_ms=start_ms,
            trusted_ca_path=trusted_tsa_ca_path,
        )
    checks["independent_preregistration_witness_verified"] = witness_ok
    clock_trust = (
        "verified_rfc3161_tsa_witness"
        if witness_ok
        else "self_asserted_json_only_requires_external_witness"
    )
    return {
        "qualified": all(checks.values()),
        "structural_checks_passed": structural_checks_passed,
        "pre_registration_clock_trust": clock_trust,
        "witness_time_ms": witness_time_ms if witness_ok else None,
        "witness_verification_detail": witness_detail,
        "checks": checks,
        "covered_rth_dates_et": rth_dates,
        "decision_start_ms": start_ms if _is_valid_ms(start_ms) else None,
        "decision_end_ms": end_ms if _is_valid_ms(end_ms) else None,
        "frozen_at_ms": frozen_at_ms if _is_valid_ms(frozen_at_ms) else None,
        "sealed_at_ms": sealed_at_ms if _is_valid_ms(sealed_at_ms) else None,
    }


def evaluate_rth_ant_gate(
    window_result: dict[str, Any],
    *,
    role: str,
    prereg_manifest: dict[str, Any] | None = None,
    seal_receipt: dict[str, Any] | None = None,
    verified_prereg_sha256: str | None = None,
    verified_segment_digests: dict[str, str] | None = None,
    verified_engine_digests: dict[str, str] | None = None,
    external_witness: dict[str, Any] | None = None,
    trusted_tsa_ca_path: Path | None = None,
) -> dict[str, Any]:
    """Evaluate pre-registered `rth-ant` Go/No-Go gate on a simulated window dict."""
    if role not in ("monday_rth_oos", "selection_dev_dryrun"):
        raise ValueError(f"role must be 'monday_rth_oos' or 'selection_dev_dryrun', got {role!r}")

    variants = list(window_result.get("variants") or [])
    taker_rows = [r for r in variants if r.get("mode") == "TAKER_TAKER"]
    maker_rows = [r for r in variants if r.get("mode") == "MAKER_ARCUS_TAKER_XYZ"]

    by_lat = {int(r["lat_ms"]): r for r in taker_rows}
    all_lats_present = all(lat in by_lat for lat in REQUIRED_TAKER_LATS)
    all_taker_pos = all_lats_present and all(
        float(by_lat[lat]["net_cash_usd"]) > 0.0 for lat in REQUIRED_TAKER_LATS
    )
    all_flat = bool(variants) and all(bool(r.get("terminal_flat")) for r in variants)
    witnessed = int(window_result.get("funding_hours_witnessed", 0))
    missing_witness = int(window_result.get("funding_hours_missing_witness", -1))
    zero_missing_witness = witnessed > 0 and missing_witness == 0
    max_dd = max((float(r.get("max_drawdown_usd", 0.0)) for r in variants), default=0.0)
    dd_within_budget = bool(variants) and max_dd <= MAX_DRAWDOWN_BUDGET_USD
    halted_any = any(bool(r.get("halted_58pct")) for r in variants)
    cycles_ok = all_lats_present and all(
        int(by_lat[lat].get("cycles_closed", 0)) >= MIN_CYCLES for lat in REQUIRED_TAKER_LATS
    )
    win_rate_ok = all_lats_present and all(
        (by_lat[lat].get("win_rate") or 0.0) >= MIN_WIN_RATE for lat in REQUIRED_TAKER_LATS
    )

    checks = {
        "all_required_taker_latencies_present": all_lats_present,
        "all_taker_latencies_positive": all_taker_pos,
        "all_terminal_flat": all_flat,
        "zero_missing_funding_witness": zero_missing_witness,
        "max_drawdown_within_budget": dd_within_budget,
        "zero_58pct_halt": not halted_any,
        "sufficient_cycles_and_win_rate": cycles_ok and win_rate_ok,
    }
    passed_metrics = all(checks.values())
    provenance: dict[str, Any] | None = None
    if role == "monday_rth_oos":
        provenance = verify_rth_ant_provenance(
            prereg_manifest,
            window_result=window_result,
            seal_receipt=seal_receipt,
            verified_prereg_sha256=verified_prereg_sha256,
            verified_segment_digests=verified_segment_digests,
            verified_engine_digests=verified_engine_digests,
            external_witness=external_witness,
            trusted_tsa_ca_path=trusted_tsa_ca_path,
        )
        verdict = "GO" if (passed_metrics and provenance["qualified"]) else "NO_GO"
    else:
        if prereg_manifest is not None or seal_receipt is not None or external_witness is not None:
            provenance = verify_rth_ant_provenance(
                prereg_manifest,
                window_result=window_result,
                seal_receipt=seal_receipt,
                verified_prereg_sha256=verified_prereg_sha256,
                verified_segment_digests=verified_segment_digests,
                verified_engine_digests=verified_engine_digests,
                external_witness=external_witness,
                trusted_tsa_ca_path=trusted_tsa_ca_path,
            )
        verdict = "NO_GO" if not passed_metrics else "DRY_RUN_NOT_OOS"

    return {
        "gate": "rth_ant",
        "role": role,
        "verdict": verdict,
        "passed_metrics": passed_metrics,
        "checks": checks,
        "provenance": provenance,
        "funding_hours_witnessed": witnessed,
        "funding_hours_missing_witness": missing_witness,
        "max_drawdown_usd": round(max_dd, 4),
        "taker_variants": taker_rows,
        "maker_variants": maker_rows,
    }


EXPECTED_RWA_FULL_MARKETS = frozenset({"NVDA", "SPY", "QQQ", "AMZN", "AAPL", "TSLA"})
EXPECTED_RWA_EX_NVDA_MARKETS = EXPECTED_RWA_FULL_MARKETS - {"NVDA"}


def _require_bool(mapping: dict[str, Any], key: str, *, ctx: str) -> bool:
    if key not in mapping:
        raise ValueError(f"missing required boolean field {key!r} in {ctx}")
    val = mapping[key]
    if not isinstance(val, bool):
        raise ValueError(f"field {key!r} must be boolean in {ctx}")
    return val


def _require_finite_number(
    mapping: dict[str, Any], key: str, *, ctx: str, nonnegative: bool = False,
) -> float:
    value = mapping.get(key)
    if (
        isinstance(value, bool)
        or not isinstance(value, (int, float))
        or not math.isfinite(value)
        or (nonnegative and value < 0.0)
    ):
        raise ValueError(f"invalid numeric field {key!r} in {ctx}")
    return float(value)


def _rwa_cost_per_million(cash: float, core_volume: float) -> float:
    if not math.isfinite(core_volume) or core_volume <= 0.0:
        raise ValueError("RWA gate requires positive finite Core volume")
    cost = -cash / (core_volume / 1e6)
    if not math.isfinite(cost):
        raise ValueError("RWA gate requires finite cost")
    return cost


def _summarize_rwa_arm(summary: dict[str, Any], arm_name: str, *, exclude_nvda: bool) -> dict[str, Any]:
    markets = summary.get("markets")
    if not isinstance(markets, dict):
        raise ValueError("missing required 'markets' mapping in W15 RWA summary")
    if not exclude_nvda:
        actual_full = frozenset(markets.keys())
        if actual_full != EXPECTED_RWA_FULL_MARKETS:
            raise ValueError(
                f"W15 RWA full_6markets set mismatch: expected {sorted(EXPECTED_RWA_FULL_MARKETS)}, got {sorted(actual_full)}"
            )
        arm = (summary.get("aggregate_6markets") or {}).get(arm_name)
        if not arm:
            raise KeyError(f"missing aggregate_6markets arm {arm_name!r}")
        ctx = f"aggregate_6markets.{arm_name}"
        return {
            "core_maker_usd": _require_finite_number(arm, "core_maker_usd", ctx=ctx, nonnegative=True),
            "dual_usd": _require_finite_number(arm, "dual_usd", ctx=ctx, nonnegative=True),
            "terminal_liq_cash_usd": _require_finite_number(arm, "terminal_liq_cash_usd", ctx=ctx),
            "core_usd_per_m_liq": _require_finite_number(arm, "single_venue_core_usd_per_m_liq", ctx=ctx),
            "terminal_flat_all": _require_bool(arm, "terminal_flat_all", ctx=ctx),
            "drawdown_58_breached": _require_bool(arm, "drawdown_58_breached", ctx=ctx),
        }

    actual_ex_nvda = frozenset(sym for sym in markets if sym != "NVDA")
    if actual_ex_nvda != EXPECTED_RWA_EX_NVDA_MARKETS:
        raise ValueError(
            f"W15 RWA ex-NVDA market set mismatch: expected {sorted(EXPECTED_RWA_EX_NVDA_MARKETS)}, got {sorted(actual_ex_nvda)}"
        )

    core_vol = 0.0
    dual_vol = 0.0
    liq_cash = 0.0
    flat_all = True
    any_58_breach = False
    halt_blocks = 0
    max_mkt_dd = 0.0
    count = 0
    for sym in sorted(EXPECTED_RWA_EX_NVDA_MARKETS):
        sym_data = markets[sym]
        arm = (sym_data.get("arms") or {}).get(arm_name)
        if not arm:
            raise KeyError(f"missing market {sym!r} arm {arm_name!r}")
        ctx = f"markets.{sym}.arms.{arm_name}"
        core_vol += _require_finite_number(arm, "core_maker_usd", ctx=ctx, nonnegative=True)
        dual_vol += _require_finite_number(arm, "dual_usd", ctx=ctx, nonnegative=True)
        liq_cash += _require_finite_number(arm, "terminal_liq_cash_usd", ctx=ctx)
        market_flat = _require_bool(arm, "terminal_flat_liq_ok", ctx=ctx)
        flat_all = flat_all and market_flat
        mkt_dd = _require_finite_number(arm, "max_drawdown_usd", ctx=ctx, nonnegative=True)
        max_mkt_dd = max(max_mkt_dd, mkt_dd)
        reported_breach = (
            _require_bool(arm, "drawdown_58_breached", ctx=ctx)
            if "drawdown_58_breached" in arm else False
        )
        any_58_breach = any_58_breach or reported_breach or mkt_dd >= 580.0
        halt_blocks += int(arm.get("halt_blocked", 0))
        count += 1
    if not math.isfinite(dual_vol):
        raise ValueError(f"non-finite ex-NVDA dual-venue volume for {arm_name}")
    cost = _rwa_cost_per_million(liq_cash, core_vol)
    return {
        "markets_included": count,
        "core_maker_usd": core_vol,
        "dual_usd": dual_vol,
        "terminal_liq_cash_usd": liq_cash,
        "core_usd_per_m_liq": cost,
        "terminal_flat_all": flat_all,
        "max_market_drawdown_usd": round(max_mkt_dd, 4),
        "drawdown_58_breached": any_58_breach,
        "feed_halt_blocks": halt_blocks,
    }


def _rwa_cost_evidence(arms: list[dict], threshold: float) -> str:
    # A legacy cash value represents a rounded 4dp interval, turnover a 2dp interval.
    # Compare cross products exactly, including at equality.
    low = high = Fraction(0)
    min_volume = max_volume = Fraction(0)
    target = Fraction(str(threshold))
    for arm in arms:
        bounds = []
        for field, half_unit in (("terminal_liq_cash_usd", "0.00005"), ("core_maker_usd", "0.005")):
            shown = Fraction(str(arm[field]))
            error = Fraction(half_unit)
            key = field + "_exact"
            if key in arm:
                raw = arm[key]
                try:
                    value = Decimal(raw) if isinstance(raw, str) else None
                except InvalidOperation:
                    value = None
                if value is None or not value.is_finite() or abs(Fraction(value) - shown) > error:
                    raise ValueError(f"invalid or inconsistent {key}")
                bounds.append((Fraction(value), Fraction(value)))
            else:
                bounds.append((shown - error, shown + error))
        (cash_lo, cash_hi), (vol_lo, vol_hi) = bounds
        if vol_hi < 0 or ("core_maker_usd_exact" in arm and vol_lo < 0):
            raise ValueError("negative maker turnover")
        min_volume += max(0, vol_lo)
        max_volume += max(0, vol_hi)
        low += -cash_hi * 1_000_000 - target * vol_hi
        high += -cash_lo * 1_000_000 - target * max(0, vol_lo)
    if max_volume == 0 or low > 0:
        return "above_or_no_volume"
    if min_volume > 0 and high <= 0:
        return "within_threshold"
    return "insufficient_precision"


def evaluate_w15_rwa_gate(
    summary: dict[str, Any],
    *,
    max_cost_per_million: float = 20.0,
) -> dict[str, Any]:
    """Evaluate `W15+` Core Standard RWA cost threshold (`<= $20/M`) on 6-sym & ex-NVDA."""
    if (
        isinstance(max_cost_per_million, bool)
        or not isinstance(max_cost_per_million, (int, float))
        or not math.isfinite(max_cost_per_million)
        or max_cost_per_million <= 0.0
    ):
        raise ValueError("max_cost_per_million must be positive finite")
    arms = ("CORE_PURE_PULL05_C310", "CORE_HEDGE_SELF_PULL05_C310")
    rows: dict[str, Any] = {}
    for arm_name in arms:
        full = _summarize_rwa_arm(summary, arm_name, exclude_nvda=False)
        ex_nvda = _summarize_rwa_arm(summary, arm_name, exclude_nvda=True)
        market_arms = [summary["markets"][sym]["arms"][arm_name] for sym in sorted(EXPECTED_RWA_EX_NVDA_MARKETS)]
        for row, inputs in ((full, [summary["aggregate_6markets"][arm_name]]), (ex_nvda, market_arms)):
            evidence = _rwa_cost_evidence(inputs, max_cost_per_million)
            row["cost_evidence"] = evidence
            row["pass_cost_le_threshold"] = evidence == "within_threshold" and row["terminal_flat_all"] and not row["drawdown_58_breached"]
        ex_nvda["core_maker_usd"] = round(ex_nvda["core_maker_usd"], 2)
        ex_nvda["dual_usd"] = round(ex_nvda["dual_usd"], 2)
        ex_nvda["terminal_liq_cash_usd"] = round(ex_nvda["terminal_liq_cash_usd"], 4)
        ex_nvda["core_usd_per_m_liq"] = round(ex_nvda["core_usd_per_m_liq"], 2)
        rows[arm_name] = {"full_6markets": full, "ex_nvda": ex_nvda}

    hedged_ex_nvda_ok = rows["CORE_HEDGE_SELF_PULL05_C310"]["ex_nvda"]["pass_cost_le_threshold"]
    pure_ex_nvda_ok = rows["CORE_PURE_PULL05_C310"]["ex_nvda"]["pass_cost_le_threshold"]
    verdict = "GO_HEDGED" if hedged_ex_nvda_ok else ("GO_UNHEDGED_ONLY" if pure_ex_nvda_ok else "NO_GO")
    return {
        "gate": "w15_rwa",
        "max_cost_per_million_usd": max_cost_per_million,
        "verdict": verdict,
        "arms": rows,
    }


def _sealed_inputs_match_seal_segments(sealed_inputs: Any, seal_segments: Any) -> bool:
    for mapping in (sealed_inputs, seal_segments):
        if not isinstance(mapping, dict) or not mapping:
            return False
        for name, digest in mapping.items():
            if (not isinstance(name, str) or any(p in ("", ".", "..") for p in name.split("/"))
                    or not _is_sha256(digest)):
                return False

    if len(sealed_inputs) != len(seal_segments):
        return False
    matched = set()
    for name, digest in sealed_inputs.items():
        if name in seal_segments:
            key = name
        else:
            matches = [key for key in seal_segments
                       if key.endswith("/" + name) or name.endswith("/" + key)]
            if len(matches) != 1:
                return False
            key = matches[0]
        if key in matched or seal_segments[key] != digest:
            return False
        matched.add(key)
    return True


def _verify_w15_rwa_formal_receipt(
    summary: dict[str, Any],
    *,
    seal_receipt: dict[str, Any] | None,
    repo_root: Path,
) -> tuple[bool, dict[str, bool]]:
    receipt = summary.get("formal_replay_receipt") if isinstance(summary, dict) else None
    if not isinstance(receipt, dict):
        return False, {
            "formal_replay_receipt_schema_ok": False,
            "exact_10h_decision_window": False,
            "frozen_method_hashes_verified": False,
            "sealed_input_hashes_verified": False,
            "payload_sha256_verified": False,
        }

    schema_ok = receipt.get("schema") == W15_RWA_REPLAY_RECEIPT_SCHEMA
    d_start = receipt.get("decision_start_ms")
    d_end = receipt.get("decision_end_ms")
    exact_10h = bool(_is_pos_int(d_start) and _is_pos_int(d_end) and (d_end - d_start) == TEN_HOURS_MS)

    # Some seal formats also record their scoring boundaries. Compare both when supplied.
    seal = seal_receipt if isinstance(seal_receipt, dict) else {}
    bounds_in_seal = "decision_start_ms" in seal or "decision_end_ms" in seal
    seal_bounds_ok = not bounds_in_seal or (
        _is_pos_int(seal.get("decision_start_ms")) and _is_pos_int(seal.get("decision_end_ms"))
        and (seal["decision_start_ms"], seal["decision_end_ms"]) == (d_start, d_end)
    )

    frozen_map = receipt.get("frozen_method_sha256")
    method_ok = isinstance(frozen_map, dict) and set(frozen_map.keys()) == set(FROZEN_RWA_METHOD_FILES)
    if method_ok:
        for rel_path in FROZEN_RWA_METHOD_FILES:
            target = (repo_root / rel_path).resolve()
            exp_sha = frozen_map.get(rel_path)
            if not (target.is_file() and _is_sha256(exp_sha) and _sha256_file(target) == exp_sha):
                method_ok = False
                break

    seal_segments = seal_receipt.get("segment_sha256") if isinstance(seal_receipt, dict) else None
    sealed_input_ok = _sealed_inputs_match_seal_segments(receipt.get("sealed_input_sha256"), seal_segments)

    payload_ok = False
    markets = summary.get("markets")
    agg6 = summary.get("aggregate_6markets")
    exp_payload_sha = receipt.get("payload_sha256")
    if isinstance(markets, dict) and isinstance(agg6, dict) and _is_sha256(exp_payload_sha):
        try:
            canonical = json.dumps(
                {"markets": markets, "aggregate_6markets": agg6},
                sort_keys=True,
                separators=(",", ":"),
                allow_nan=False,
            ).encode("utf-8")
        except (TypeError, ValueError):
            canonical = None
        if canonical is not None:
            payload_ok = hashlib.sha256(canonical).hexdigest() == exp_payload_sha

    checks = {
        "formal_replay_receipt_schema_ok": schema_ok,
        "exact_10h_decision_window": exact_10h,
        "supplied_seal_bounds_consistent": seal_bounds_ok,
        "frozen_method_hashes_verified": method_ok,
        "sealed_input_hashes_verified": sealed_input_ok,
        "payload_sha256_verified": payload_ok,
    }
    return all(checks.values()), checks


def run_w15_rwa_cli(
    summary_path: Path,
    output: Path,
    role: str,
    threshold: float,
    *,
    seal_receipt_path: Path | None = None,
    repo_root: Path = REPO_ROOT,
) -> int:
    source = summary_path.read_bytes()
    report: dict[str, Any] = {
        "gate": "w15_rwa",
        "role": role,
        "formal_admission": False,
        "input_json": str(summary_path),
        "input_sha256": hashlib.sha256(source).hexdigest(),
        "max_cost_per_million_usd": threshold if math.isfinite(threshold) else None,
    }
    summary = json.loads(source)
    if role == "formal":
        if threshold != 20.0:
            report.update(verdict="FORMAL_WITHHELD", reason="formal_threshold_must_be_20")
            exit_code = 2
        elif not isinstance(summary, dict) or summary.get("formal_replay_receipt") is None or seal_receipt_path is None:
            report.update(verdict="FORMAL_WITHHELD", reason="missing_registered_cost_replay_receipt")
            exit_code = 2
        else:
            seal_receipt: dict[str, Any] | None = None
            try:
                loaded_seal = json.loads(seal_receipt_path.read_text(encoding="utf-8"))
                if isinstance(loaded_seal, dict):
                    seal_receipt = loaded_seal
            except (OSError, ValueError):
                seal_receipt = None
            receipt_ok, receipt_checks = _verify_w15_rwa_formal_receipt(
                summary,
                seal_receipt=seal_receipt,
                repo_root=repo_root,
            )
            if not receipt_ok:
                report.update(
                    verdict="NO_GO",
                    formal_admission=False,
                    reason="invalid_formal_replay_receipt",
                    receipt_verified=False,
                    receipt_checks=receipt_checks,
                )
                exit_code = 1
            else:
                try:
                    economics = evaluate_w15_rwa_gate(summary, max_cost_per_million=20.0)
                except (KeyError, ValueError):
                    report.update(
                        verdict="NO_GO",
                        formal_admission=False,
                        reason="invalid_rwa_summary_metrics",
                        receipt_verified=False,
                        receipt_checks=receipt_checks,
                    )
                    exit_code = 1
                else:
                    # The v1 receipt is generated after replay. It can establish internal
                    # consistency, but it carries no independently registered window or method.
                    report.update(
                        verdict="FORMAL_WITHHELD",
                        reason="missing_independent_registered_replay_contract",
                        diagnostic_verdict=economics["verdict"],
                        arms=economics["arms"],
                        receipt_verified=True,
                        receipt_checks=receipt_checks,
                    )
                    exit_code = 2
    else:
        economics = evaluate_w15_rwa_gate(summary, max_cost_per_million=threshold)
        report.update(verdict="DIAGNOSTIC_ONLY", diagnostic_verdict=economics["verdict"], arms=economics["arms"])
        exit_code = 1 if economics["verdict"] == "NO_GO" else 0
    output.write_text(json.dumps(report, indent=2, allow_nan=False) + "\n")
    print(json.dumps({k: v for k, v in report.items() if k != "input_json"}, indent=2, allow_nan=False))
    return exit_code


def verify_w17_crash_pause_prereg(prereg_path: Path, repo_root: Path) -> dict[str, Any]:
    """Verify SHA-256 parity of all frozen `W17+` crash-pause artifacts."""
    prereg = json.loads(prereg_path.read_text())
    expected_map: dict[str, str] = dict(prereg.get("artifact_sha256") or {})
    if not expected_map:
        raise ValueError(f"missing artifact_sha256 map in {prereg_path}")

    checked: dict[str, dict[str, Any]] = {}
    all_match = True
    for rel_path, expected_sha in expected_map.items():
        target = (repo_root / rel_path).resolve()
        if not target.is_file():
            checked[rel_path] = {"expected": expected_sha, "actual": None, "match": False}
            all_match = False
            continue
        actual_sha = hashlib.sha256(target.read_bytes()).hexdigest()
        matched = actual_sha == expected_sha
        checked[rel_path] = {"expected": expected_sha, "actual": actual_sha, "match": matched}
        if not matched:
            all_match = False

    return {
        "gate": "w17_crash_pause_verify",
        "card_id": prereg.get("card_id"),
        "method_sha256": prereg.get("method_sha256"),
        "builder_cli": prereg.get("builder_cli"),
        "verdict": "VERIFIED" if all_match else "MISMATCH",
        "artifacts": checked,
    }


def _sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def _collect_segment_hashes(capture_dirs: list[Path], seal_receipt: dict[str, Any] | None) -> dict[str, str]:
    expected = seal_receipt.get("segment_sha256") if isinstance(seal_receipt, dict) else None
    use_dir_prefix = (
        isinstance(expected, dict)
        and bool(expected)
        and all(isinstance(k, str) and k.count("/") == 1 for k in expected)
    )
    digests: dict[str, str] = {}
    for d in capture_dirs:
        if not d.is_dir():
            continue
        for f in sorted(d.glob("tokyo-research-*.seg*.ndjson.gz")):
            key = f"{d.name}/{f.name}" if use_dir_prefix else f.name
            if key in digests:
                key = f"{d.name}/{f.name}"
                if key in digests:
                    key = str(f)
            digests[key] = _sha256_file(f)
    return digests


def _run_pinned_replay(source: bytes, path: Path, captures: list[Path], contract: dict, bounds: tuple[int, int] | None) -> dict:
    # Compile exactly the bytes fingerprinted by the caller. Dataclasses need
    # a module registration while their types are built.
    name = "_ant_rth_" + hashlib.sha256(source).hexdigest()
    mod = ModuleType(name)
    mod.__file__ = str(path)
    prior = sys.modules.get(name)
    sys.modules[name] = mod
    try:
        exec(compile(source, str(path), "exec"), mod.__dict__)
        params = dict(contract)
        latencies = params.pop("required_taker_latencies_ms")
        specs = [mod.AblationSpec(mode=mode, lat_ms=lat, **params)
                 for mode in ("TAKER_TAKER", "MAKER_ARCUS_TAKER_XYZ") for lat in latencies]
        return mod.run_ablation(captures, specs, decision_bounds_ns=bounds)
    finally:
        if prior is None:
            del sys.modules[name]
        else:
            sys.modules[name] = prior


def _load_external_witness(external_witness_path: Path | None) -> Any:
    if external_witness_path is None:
        return None
    try:
        loaded_witness = json.loads(external_witness_path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return {"_load_error": True}
    return {"_null": True} if loaded_witness is None else loaded_witness


def _load_prereg_manifest(
    prereg_manifest_path: Path | None,
) -> tuple[dict[str, Any] | None, str | None, str | None]:
    if prereg_manifest_path is None:
        return (None, None, "prereg_manifest_not_supplied")
    try:
        frozen_bytes = prereg_manifest_path.read_bytes()
        digest = hashlib.sha256(frozen_bytes).hexdigest()
        loaded = json.loads(frozen_bytes)
    except (OSError, ValueError):
        return (None, None, "prereg_manifest_load_error")
    if not isinstance(loaded, dict):
        return ({}, digest, "invalid_prereg_manifest_object")
    return (loaded, digest, None)


def preflight_rth_ant_witness(
    prereg_manifest_path: Path | None,
    external_witness_path: Path | None,
    trusted_tsa_ca_path: Path | None,
) -> dict[str, Any]:
    """Fail-closed RFC 3161 preregistration witness check prior to replay or archive hashing."""
    manifest, verified_prereg_sha256, load_error = _load_prereg_manifest(prereg_manifest_path)
    external_witness = _load_external_witness(external_witness_path)
    decision_start_ms = manifest.get("decision_start_ms") if isinstance(manifest, dict) else None
    if load_error is not None:
        ok, witness_time_ms, detail = False, None, load_error
    elif not (_is_valid_ms(decision_start_ms) and decision_start_ms > 0 and _is_sha256(verified_prereg_sha256)):
        ok, witness_time_ms, detail = False, None, "invalid_decision_start_ms"
    else:
        ok, witness_time_ms, detail = verify_rfc3161_witness(
            external_witness,
            expected_prereg_sha256=verified_prereg_sha256,
            decision_start_ms=decision_start_ms,
            trusted_ca_path=trusted_tsa_ca_path,
        )
    return {
        "gate": "rth_ant_witness_preflight",
        "verdict": "VERIFIED" if ok else "NO_GO",
        "independent_preregistration_witness_verified": ok,
        "pre_registration_clock_trust": (
            "verified_rfc3161_tsa_witness"
            if ok
            else "self_asserted_json_only_requires_external_witness"
        ),
        "witness_time_ms": witness_time_ms if ok else None,
        "witness_verification_detail": detail,
        "prereg_manifest": str(prereg_manifest_path) if prereg_manifest_path is not None else None,
        "prereg_manifest_sha256": verified_prereg_sha256,
        "decision_start_ms": decision_start_ms if _is_valid_ms(decision_start_ms) else None,
        "external_witness": str(external_witness_path) if external_witness_path is not None else None,
        "trusted_tsa_ca": str(trusted_tsa_ca_path) if trusted_tsa_ca_path is not None else None,
    }


def run_rth_ant_witness_preflight_cli(
    prereg_manifest_path: Path | None,
    external_witness_path: Path | None,
    trusted_tsa_ca_path: Path | None,
    output: Path | None = None,
) -> int:
    report = preflight_rth_ant_witness(
        prereg_manifest_path=prereg_manifest_path,
        external_witness_path=external_witness_path,
        trusted_tsa_ca_path=trusted_tsa_ca_path,
    )
    text = json.dumps(report, indent=2) + "\n"
    if output is not None:
        output.write_text(text, encoding="utf-8")
    print(text, end="")
    return 0 if report["verdict"] == "VERIFIED" else 1


def _emit_rth_ant_output(
    report: dict[str, Any],
    *,
    capture_dirs: list[Path],
    role: str,
    output: Path,
    prereg_manifest_path: Path | None,
    verified_prereg_sha256: str | None,
    seal_receipt_path: Path | None,
    external_witness_path: Path | None,
    trusted_tsa_ca_path: Path | None,
) -> int:
    report["capture_dirs"] = [str(p) for p in capture_dirs]
    report["prereg_manifest"] = str(prereg_manifest_path) if prereg_manifest_path is not None else None
    report["prereg_manifest_sha256"] = verified_prereg_sha256
    report["seal_receipt"] = str(seal_receipt_path) if seal_receipt_path is not None else None
    report["external_witness"] = str(external_witness_path) if external_witness_path is not None else None
    report["trusted_tsa_ca"] = str(trusted_tsa_ca_path) if trusted_tsa_ca_path is not None else None
    output.write_text(json.dumps(report, indent=2) + "\n")
    summary = {
        "gate": report["gate"],
        "role": role,
        "verdict": report["verdict"],
        "checks": report["checks"],
        "provenance": report["provenance"],
        "max_drawdown_usd": report["max_drawdown_usd"],
    }
    if "witness_preflight_blocked" in report:
        summary["witness_preflight_blocked"] = report["witness_preflight_blocked"]
    print(json.dumps(summary, indent=2))
    if role == "monday_rth_oos" and report["verdict"] != "GO":
        return 1
    return 0


def run_rth_ant_cli(
    capture_dirs: list[Path],
    role: str,
    output: Path,
    repo_root: Path,
    prereg_manifest_path: Path | None = None,
    seal_receipt_path: Path | None = None,
    external_witness_path: Path | None = None,
    trusted_tsa_ca_path: Path | None = None,
    *,
    require_witness_preflight: bool = False,
) -> int:
    external_witness = _load_external_witness(external_witness_path)
    if require_witness_preflight and role == "monday_rth_oos":
        preflight = preflight_rth_ant_witness(
            prereg_manifest_path=prereg_manifest_path,
            external_witness_path=external_witness_path,
            trusted_tsa_ca_path=trusted_tsa_ca_path,
        )
        if not preflight["independent_preregistration_witness_verified"]:
            manifest_pre, digest_pre, _ = _load_prereg_manifest(prereg_manifest_path)
            report = evaluate_rth_ant_gate(
                {},
                role=role,
                prereg_manifest=manifest_pre,
                seal_receipt=None,
                verified_prereg_sha256=digest_pre,
                verified_segment_digests={},
                verified_engine_digests={},
                external_witness=external_witness,
                trusted_tsa_ca_path=trusted_tsa_ca_path,
            )
            if manifest_pre is None:
                report["provenance"]["witness_verification_detail"] = preflight["witness_verification_detail"]
            report["witness_preflight_blocked"] = True
            return _emit_rth_ant_output(
                report,
                capture_dirs=capture_dirs,
                role=role,
                output=output,
                prereg_manifest_path=prereg_manifest_path,
                verified_prereg_sha256=digest_pre,
                seal_receipt_path=seal_receipt_path,
                external_witness_path=external_witness_path,
                trusted_tsa_ca_path=trusted_tsa_ca_path,
            )

    manifest: dict[str, Any] | None = None
    verified_prereg_sha256: str | None = None
    if prereg_manifest_path is not None:
        frozen_bytes = prereg_manifest_path.read_bytes()
        verified_prereg_sha256 = hashlib.sha256(frozen_bytes).hexdigest()
        loaded = json.loads(frozen_bytes)
        manifest = loaded if isinstance(loaded, dict) else {}

    seal_receipt: dict[str, Any] | None = None
    if seal_receipt_path is not None:
        loaded_seal = json.loads(seal_receipt_path.read_text())
        seal_receipt = loaded_seal if isinstance(loaded_seal, dict) else {}

    declared = manifest.get("strategy_contract") if manifest else None
    decision_bounds_ns: tuple[int, int] | None = None
    if isinstance(manifest, dict):
        start_ms = manifest.get("decision_start_ms")
        end_ms = manifest.get("decision_end_ms")
        if _is_valid_ms(start_ms) and _is_valid_ms(end_ms) and 0 < start_ms < end_ms:
            decision_bounds_ns = (start_ms * NS_PER_MS, end_ms * NS_PER_MS)

    engine_key, = FROZEN_ANT_ENGINE_FILES
    path = repo_root / engine_key
    try:
        source = path.read_bytes()
    except FileNotFoundError:
        source = None
    eng_digests = {engine_key: hashlib.sha256(source).hexdigest()} if source is not None else {}
    qualified_engine = _contract_matches(declared) and _engine_hashes_match((manifest or {}).get("frozen_engine_sha256"), eng_digests)
    raw = {}
    if source is not None and (role == "selection_dev_dryrun" or qualified_engine):
        raw = _run_pinned_replay(source, path, capture_dirs, declared if qualified_engine else ANT_STRATEGY_CONTRACT, decision_bounds_ns)
    seg_digests = _collect_segment_hashes(capture_dirs, seal_receipt)
    report = evaluate_rth_ant_gate(
        raw,
        role=role,
        prereg_manifest=manifest,
        seal_receipt=seal_receipt,
        verified_prereg_sha256=verified_prereg_sha256,
        verified_segment_digests=seg_digests,
        verified_engine_digests=eng_digests,
        external_witness=external_witness,
        trusted_tsa_ca_path=trusted_tsa_ca_path,
    )
    return _emit_rth_ant_output(
        report,
        capture_dirs=capture_dirs,
        role=role,
        output=output,
        prereg_manifest_path=prereg_manifest_path,
        verified_prereg_sha256=verified_prereg_sha256,
        seal_receipt_path=seal_receipt_path,
        external_witness_path=external_witness_path,
        trusted_tsa_ca_path=trusted_tsa_ca_path,
    )


def main() -> int:
    repo_root = Path(__file__).resolve().parent.parent
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)

    p_ant = sub.add_parser("rth-ant", help="Run frozen EXP-ARB-A2 ANT gate on sealed captures")
    p_ant.add_argument("--capture-dirs", nargs="+", type=Path, required=True)
    p_ant.add_argument("--role", choices=("monday_rth_oos", "selection_dev_dryrun"), required=True)
    p_ant.add_argument("--prereg-manifest", type=Path, default=None, help="Pre-capture frozen plan JSON (required for monday_rth_oos GO)")
    p_ant.add_argument("--seal-receipt", type=Path, default=None, help="Post-capture seal receipt JSON referencing prereg SHA-256")
    p_ant.add_argument("--external-witness", type=Path, default=None, help="External RFC 3161 TSA preregistration witness JSON")
    p_ant.add_argument("--trusted-tsa-ca", type=Path, default=None, help="Explicit trusted TSA CA PEM file (never embedded)")
    p_ant.add_argument(
        "--require-witness-preflight",
        action="store_true",
        help="Verify external RFC 3161 witness before running formal replay or archive hashing",
    )
    p_ant.add_argument("--output", type=Path, required=True)

    p_pre = sub.add_parser(
        "rth-ant-witness-preflight",
        help="Verify external RFC 3161 TSA preregistration witness before dataset reads",
    )
    p_pre.add_argument("--prereg-manifest", type=Path, required=True, help="Pre-capture frozen plan JSON")
    p_pre.add_argument("--external-witness", type=Path, default=None, help="External RFC 3161 TSA preregistration witness JSON")
    p_pre.add_argument("--trusted-tsa-ca", type=Path, default=None, help="Explicit trusted TSA CA PEM file")
    p_pre.add_argument("--output", type=Path, default=None)

    p_rwa = sub.add_parser("w15-rwa", help="Diagnose Core RWA cost or report formal admission status")
    p_rwa.add_argument("--role", choices=("formal", "diagnostic"), default="formal")
    p_rwa.add_argument("--rwa-summary-json", type=Path, required=True)
    p_rwa.add_argument("--max-cost-per-million", type=float, default=20.0)
    p_rwa.add_argument("--seal-receipt", type=Path, default=None, help="Post-capture seal receipt JSON verifying sealed RWA input segments")
    p_rwa.add_argument("--output", type=Path, required=True)

    p_w17 = sub.add_parser("w17-crash-pause-verify", help="Verify frozen W17+ artifact SHA-256")
    p_w17.add_argument("--prereg", type=Path, required=True)

    args = parser.parse_args()
    if args.command == "rth-ant":
        return run_rth_ant_cli(
            args.capture_dirs,
            args.role,
            args.output,
            repo_root,
            args.prereg_manifest,
            args.seal_receipt,
            args.external_witness,
            args.trusted_tsa_ca,
            require_witness_preflight=args.require_witness_preflight,
        )
    if args.command == "rth-ant-witness-preflight":
        return run_rth_ant_witness_preflight_cli(
            args.prereg_manifest,
            args.external_witness,
            args.trusted_tsa_ca,
            args.output,
        )
    if args.command == "w15-rwa":
        return run_w15_rwa_cli(
            args.rwa_summary_json,
            args.output,
            args.role,
            args.max_cost_per_million,
            seal_receipt_path=args.seal_receipt,
            repo_root=repo_root,
        )
    if args.command == "w17-crash-pause-verify":
        res = verify_w17_crash_pause_prereg(args.prereg, repo_root)
        print(json.dumps(res, indent=2))
        return 0 if res["verdict"] == "VERIFIED" else 1
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
