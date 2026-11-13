#!/usr/bin/env python3
"""Audit declared provenance and state continuity for independent 10 h studies.

This metadata gate does not inspect private records or infer profitable execution.
Artifact envelopes alone are not verified stream coverage. A producer must supply
the hash of its stream-quality audit and explicit gaps. Execution state hashes
must cover cash, inventory, pending orders, queue and strategy warm-up state.

CLI: research_windows.py CATALOG.json RUNS.json --as-of-ms INTEGER
CATALOG is a map from immutable source IDs to source descriptors. RUNS is a list
of independent scenario configurations, not raw order or fill records.
"""

import argparse
from collections import Counter
from decimal import Decimal, InvalidOperation
from hashlib import sha256
import json
from pathlib import Path
import re
from urllib.parse import parse_qsl, urlsplit


TEN_HOURS_MS = 36_000_000
SCENARIOS = {"quant", "arbitrage", "turnover"}
STREAM_KINDS = {"order_book", "trades", "funding_settlements", "funding_schedule", "reference"}
SHA256 = re.compile(r"[0-9a-f]{64}\Z")


def _hash(value):
    return isinstance(value, str) and SHA256.fullmatch(value) is not None


def _time(value):
    return isinstance(value, int) and not isinstance(value, bool) and value >= 0


def _interval(value):
    return (isinstance(value, (list, tuple)) and len(value) == 2
            and all(_time(t) for t in value) and value[0] < value[1])


def _merge(intervals):
    result = []
    for start, end in sorted(intervals):
        if result and start <= result[-1][1]:
            result[-1][1] = max(result[-1][1], end)
        else:
            result.append([start, end])
    return result


def _gaps(start, end, intervals):
    gaps, cursor = [], start
    for left, right in _merge(intervals):
        left, right = max(start, left), min(end, right)
        if right <= left:
            continue
        if left > cursor:
            gaps.append([cursor, left])
        cursor = max(cursor, right)
    if cursor < end:
        gaps.append([cursor, end])
    return gaps


def _origin_safe(origin):
    if not isinstance(origin, str) or not origin:
        return False
    try:
        url = urlsplit(origin)
    except ValueError:
        return False
    sensitive = {"token", "api_key", "apikey", "secret", "password", "signature", "authorization"}
    return (url.scheme in {"https", "wss"} and bool(url.hostname)
            and url.username is None and url.password is None and not url.fragment
            and not any(k.lower() in sensitive for k, _ in parse_qsl(url.query)))


def funding_schedule_coverage(source, start_ms, end_ms, as_of_ms):
    """A reviewed public rule has an effective period, not a stream envelope."""
    issues = set()
    schedule = source.get("funding_schedule", {})
    if not isinstance(schedule, dict):
        schedule = {}
    if source.get("visibility") != "public" or not _origin_safe(source.get("origin")):
        issues.add("public_provenance_unverified")
    if not all(isinstance(source.get(k), str) and source[k] for k in ("venue", "domain", "instrument")):
        issues.add("source_identity_missing")
    interval, offset = schedule.get("interval_ms"), schedule.get("offset_ms")
    if not (_time(interval) and interval >= 60_000 and _time(offset) and offset < interval):
        issues.add("funding_schedule_unknown")
    span = [schedule.get("valid_from_ms"), schedule.get("valid_until_ms")]
    if not _interval(span) or span[0] > start_ms or span[1] < end_ms:
        issues.add("funding_schedule_does_not_cover_window")
    reviewed = schedule.get("reviewed_at_ms")
    if (schedule.get("reviewed") is not True or not _hash(schedule.get("review_sha256"))
            or not _time(reviewed) or reviewed > as_of_ms):
        issues.add("funding_schedule_unreviewed")
    metadata = schedule.get("metadata_sha256")
    matched = False
    for artifact in source.get("artifacts", []):
        captured, sealed = artifact.get("captured_at_ms"), artifact.get("sealed_at_ms")
        if (_hash(metadata) and artifact.get("sha256") == metadata
                and _time(captured) and _time(sealed) and _time(reviewed)
                and captured <= start_ms and captured <= sealed <= reviewed <= as_of_ms):
            matched = True
    if not matched:
        issues.add("funding_schedule_metadata_unverified")
    return {"scope": "public_schedule_validity_not_stream_coverage",
            "qualified": not issues, "issues": sorted(issues),
            "validity_ms": span, "metadata_sha256": metadata}


def funding_run_binding_sha256(run):
    """Digest the exact run identity/state chain referenced by a funding audit."""
    value = {key: run.get(key) for key in ("ledger_id", "engine_sha256", "config_sha256",
             "fee_model_sha256", "instrument", "venue_domains", "window")}
    ids = run.get("source_ids", [])
    value["source_ids"] = sorted(ids) if isinstance(ids, list) and all(isinstance(s, str) for s in ids) else ids
    value["segments"] = run.get("execution", {}).get("segments", [])
    return sha256(json.dumps(value, sort_keys=True, separators=(",", ":"), allow_nan=False).encode()).hexdigest()


def _zero_positions(value, venues):
    if not isinstance(value, dict) or set(value) != set(venues):
        return False
    try:
        return all(isinstance(v, str) and Decimal(v).is_finite() and Decimal(v) == 0
                   for v in value.values())
    except InvalidOperation:
        return False


def _zero_counts(value, venues):
    return (isinstance(value, dict) and set(value) == set(venues)
            and all(_time(v) and v == 0 for v in value.values()))


def validate_funding_avoidance(run, catalog, as_of_ms):
    """Validate individually audited flat guards, never a boolean/count waiver.

    Slots follow the engine's (start_ms, end_ms] convention. Each guard is
    [slot - guard_ms, slot + guard_ms + 1), and source quality must also cover
    any guard time outside the decision window. Version-changing schedules
    must first be independently reconciled; a partial rule cannot cover a run.
    """
    issues, expected, proof_hashes = set(), {}, {}
    execution = run.get("execution", {})
    evidence = execution.get("funding_avoidance", {})
    if not isinstance(evidence, dict):
        evidence = {}
    venues = run.get("venue_domains", {})
    if not isinstance(venues, dict) or not venues:
        issues.add("funding_avoidance_schedules_missing")
        venues = {}
    start, end = run["window"]["start_ms"], run["window"]["end_ms"]
    ids = run.get("source_ids", [])
    if not isinstance(ids, list) or not all(isinstance(s, str) for s in ids):
        ids = []
    audit_at = evidence.get("audited_at_ms")
    if (not _hash(evidence.get("audit_sha256")) or not _time(audit_at)
            or not end <= audit_at <= as_of_ms
            or evidence.get("run_binding_sha256") != funding_run_binding_sha256(run)):
        issues.add("funding_avoidance_audit_unverified")
    guard = evidence.get("settlement_guard_ms")
    if not _time(guard) or not 1 <= guard <= 60_000:
        issues.add("funding_avoidance_guard_unknown")
        guard = None
    schedules = evidence.get("schedule_source_ids", {})
    proofs = evidence.get("verified_slots_by_venue", {})
    if not isinstance(schedules, dict) or set(schedules) != set(venues):
        issues.add("funding_avoidance_schedules_missing")
        schedules = {}
    if not isinstance(proofs, dict) or set(proofs) != set(venues):
        issues.add("funding_avoidance_slots_missing")
        proofs = {}
    streams = {source_id: catalog[source_id] for source_id in ids
               if isinstance(catalog.get(source_id), dict)
               and catalog[source_id].get("kind") in {"order_book", "trades"}}
    for venue, domain in venues.items():
        source_id = schedules.get(venue)
        source = catalog.get(source_id) if isinstance(source_id, str) else None
        if (not isinstance(source, dict) or source_id not in ids
                or source.get("kind") != "funding_schedule" or source.get("venue") != venue
                or source.get("domain") != domain or source.get("instrument") != run.get("instrument")
                or not funding_schedule_coverage(source, start, end, as_of_ms)["qualified"]):
            issues.add("funding_avoidance_schedule_unverified")
            continue
        schedule = source["funding_schedule"]
        interval, offset = schedule["interval_ms"], schedule["offset_ms"]
        first = ((start - offset) // interval + 1) * interval + offset
        expected[venue] = list(range(first, end + 1, interval))
        rows = proofs.get(venue)
        if not isinstance(rows, list):
            issues.add("funding_avoidance_slots_missing")
            continue
        actual = [row.get("at_ms") if isinstance(row, dict) else None for row in rows]
        if not all(_time(at) for at in actual) or actual != expected[venue]:
            issues.add("funding_avoidance_slot_mismatch")
        for row in rows:
            if not isinstance(row, dict) or not _time(row.get("at_ms")) or guard is None:
                issues.add("funding_avoidance_slot_proof_missing")
                continue
            at = row["at_ms"]
            left, right = at - guard, at + guard + 1
            digest, observed = row.get("proof_sha256"), row.get("observed_at_ms")
            if (left < 0 or guard * 2 >= interval or row.get("guard_start_ms") != left
                    or row.get("guard_end_ms") != right or not _hash(digest)
                    or not _hash(row.get("state_sha256")) or not _time(observed)
                    or not _time(audit_at) or not right <= observed <= audit_at):
                issues.add("funding_avoidance_slot_proof_missing")
            if _hash(digest):
                if digest in proof_hashes and proof_hashes[digest] != at:
                    issues.add("funding_avoidance_proof_reused_for_another_slot")
                proof_hashes[digest] = at
            # These are maxima over the entire guard, not a flat snapshot
            # captured after a funding-bearing position has been closed.
            if (not _zero_positions(row.get("max_abs_positions_base"), venues)
                    or not _zero_counts(row.get("max_open_order_counts"), venues)
                    or not _zero_counts(row.get("max_pending_ioc_counts"), venues)
                    or not _zero_counts(row.get("max_pending_fill_notice_counts"), venues)
                    or not _time(row.get("max_active_cycle_count")) or row["max_active_cycle_count"] != 0):
                issues.add("funding_avoidance_residual_or_unknown_risk")
            watermarks = row.get("source_watermarks_ms")
            if (not isinstance(watermarks, dict) or set(watermarks) != set(streams)
                    or not _time(observed) or not all(_time(v) and right <= v <= observed
                                                     for v in watermarks.values())):
                issues.add("funding_avoidance_watermark_unverified")
            if left < 0 or any(not source_coverage(s, left, right, as_of_ms)["qualified"]
                               for s in streams.values()):
                issues.add("funding_avoidance_guard_data_incomplete")
    return {"qualified": not issues, "issues": sorted(issues),
            "expected_slots_by_venue": expected,
            "scope": "reviewed flat-guard evidence; no funding payment imputed"}


def source_coverage(source, start_ms, end_ms, as_of_ms):
    """Return exact missing/unverified intervals; never fill a gap by rounding."""
    if source.get("kind") == "funding_schedule":
        return funding_schedule_coverage(source, start_ms, end_ms, as_of_ms)
    issues, recorded, verified = set(), [], []
    if source.get("visibility") != "public" or not _origin_safe(source.get("origin")):
        issues.add("public_provenance_unverified")
    if source.get("kind") not in STREAM_KINDS:
        issues.add("not_a_specific_market_stream")
    if not all(isinstance(source.get(k), str) and source[k] for k in ("venue", "domain", "instrument")):
        issues.add("source_identity_missing")
    seen = {}
    for artifact in source.get("artifacts", []):
        span = [artifact.get("start_ms"), artifact.get("end_ms")]
        digest = artifact.get("sha256")
        sealed = artifact.get("sealed_at_ms")
        if not _interval(span) or not _hash(digest) or not _time(sealed):
            issues.add("artifact_provenance_missing")
            continue
        if digest in seen:
            issues.add("duplicate_artifact" if seen[digest] == span else "conflicting_artifact_interval")
            continue
        seen[digest] = span
        if sealed < span[1] or sealed > as_of_ms or span[1] > as_of_ms:
            issues.add("artifact_not_sealed_at_audit_time")
            continue
        if span[1] <= start_ms or span[0] >= end_ms:
            continue
        recorded.append(span)
        gaps = artifact.get("gaps")
        if (artifact.get("quality_verified") is not True or not _hash(artifact.get("quality_sha256"))
                or not isinstance(gaps, list)):
            issues.add("stream_quality_unknown")
            continue
        if any(not _interval(gap) or gap[0] < span[0] or gap[1] > span[1] for gap in gaps):
            issues.add("invalid_gap_evidence")
            continue
        # Complement declared source gaps inside this immutable artifact.
        verified.extend(_gaps(*span, gaps))
    missing = _gaps(start_ms, end_ms, verified)
    envelope_gaps = _gaps(start_ms, end_ms, recorded)
    if missing:
        issues.add("missing_or_unverified_stream_time")
    return {
        "recorded_envelope_ms": end_ms - start_ms - sum(b - a for a, b in envelope_gaps),
        "verified_coverage_ms": end_ms - start_ms - sum(b - a for a, b in missing),
        "unrecorded_intervals": envelope_gaps,
        "missing_or_unverified_intervals": missing,
        "issues": sorted(issues),
        "qualified": not issues,
    }


def validate_run(run, catalog, as_of_ms):
    """Classify data coverage separately from executable 10 h evidence.

    Missing attestations yield explicit ineligible reasons; malformed temporal
    coordinates raise ValueError. This never returns or combines a PnL total.
    """
    window = run.get("window", {})
    start, end = window.get("start_ms"), window.get("end_ms")
    if not _interval([start, end]) or not _time(as_of_ms):
        raise ValueError("invalid window or audit clock")
    data_issues, execution_issues = set(), set()
    if end - start != TEN_HOURS_MS:
        data_issues.add("window_not_exactly_10h")
    if end > as_of_ms:
        data_issues.add("window_not_finished")
    if run.get("scenario") not in SCENARIOS:
        execution_issues.add("unknown_scenario")
    for key in ("ledger_id", "experiment_id", "comparison_group", "independent_sample_id", "instrument"):
        if not isinstance(run.get(key), str) or not run[key]:
            execution_issues.add(f"missing_{key}")
    domains = run.get("venue_domains", {})
    if (not isinstance(domains, dict) or not domains
            or not all(isinstance(k, str) and k and isinstance(v, str) and v for k, v in domains.items())):
        data_issues.add("venue_domains_missing")
        domains = {}
    ids = run.get("source_ids", [])
    if not isinstance(ids, list) or not ids or not all(isinstance(s, str) for s in ids):
        data_issues.add("source_requirements_missing")
        ids = []
    if len(set(ids)) != len(ids):
        data_issues.add("duplicate_source_requirement")
    coverage, kinds = {}, set()
    for source_id in sorted(set(ids)):
        source = catalog.get(source_id)
        if not isinstance(source, dict):
            data_issues.add("unknown_source_id")
            continue
        c = source_coverage(source, start, end, as_of_ms)
        coverage[source_id] = c
        if not c["qualified"]:
            data_issues.add("source_coverage_incomplete")
        venue = source.get("venue")
        # Reference venues are distinct, explicit inputs, not execution domains.
        if source.get("kind") != "reference" and domains.get(venue) != source.get("domain"):
            data_issues.add("source_domain_mismatch")
        if source.get("instrument") != run.get("instrument"):
            data_issues.add("source_instrument_mismatch")
        kinds.add((venue, source.get("kind")))
    for venue in domains:
        if (venue, "order_book") not in kinds:
            data_issues.add("required_order_book_missing")
    if run.get("scenario") in {"quant", "turnover"}:
        if run.get("maker_venue") not in domains or (run.get("maker_venue"), "trades") not in kinds:
            data_issues.add("required_maker_trade_stream_missing")
    for key in ("engine_sha256", "config_sha256", "fee_model_sha256"):
        if not _hash(run.get(key)):
            execution_issues.add(f"missing_{key}")
    if run.get("fee_model_verified") is not True:
        execution_issues.add("fee_model_unverified")
    frozen = run.get("frozen_at_ms")
    if not _time(frozen) or frozen > as_of_ms:
        execution_issues.add("configuration_freeze_unverified")
    purpose = run.get("purpose")
    if purpose not in {"historical_validation", "out_of_sample"}:
        execution_issues.add("unknown_evaluation_purpose")
    if purpose == "out_of_sample" and (_time(frozen) and frozen > start):
        execution_issues.add("configuration_frozen_after_evaluation_started")
    train_end, embargo = run.get("train_end_ms"), run.get("embargo_ms", 0)
    if not _time(embargo) or (train_end is not None and not _time(train_end)):
        execution_issues.add("invalid_training_boundary")
    elif train_end is not None and train_end + embargo > start:
        execution_issues.add("training_evaluation_overlap")
    execution = run.get("execution", {})
    if execution.get("supports_funding_boundaries") is not True:
        execution_issues.add("engine_not_continuous_across_funding")
    funding_mode = execution.get("funding_mode", "settlements")
    avoidance = None
    if funding_mode == "funding_avoidance":
        avoidance = validate_funding_avoidance(run, catalog, as_of_ms)
        execution_issues.update(avoidance["issues"])
    elif funding_mode == "settlements":
        if execution.get("funding_verified") is not True or any((v, "funding_settlements") not in kinds for v in domains):
            execution_issues.add("missing_funding_settlement_evidence")
    else:
        execution_issues.add("unknown_funding_mode")
    warmup = execution.get("warmup_complete_at_ms")
    if not _time(warmup) or warmup > start:
        execution_issues.add("warmup_not_complete_at_start")
    segments = execution.get("segments", [])
    cursor, previous_hash = start, None
    if not segments:
        execution_issues.add("execution_continuity_missing")
    for i, segment in enumerate(segments):
        a, b = segment.get("start_ms"), segment.get("end_ms")
        if not _interval([a, b]) or a != cursor or b > end:
            execution_issues.add("execution_segments_not_contiguous")
            continue
        if segment.get("state_reset") is not False and i > 0:
            execution_issues.add("pooled_restarted_executions")
        initial, final = segment.get("initial_state_sha256"), segment.get("final_state_sha256")
        if not _hash(initial) or not _hash(final):
            execution_issues.add("state_continuity_unverified")
        elif previous_hash is not None and initial != previous_hash:
            execution_issues.add("state_continuity_mismatch")
        cursor, previous_hash = b, final
    if cursor != end:
        execution_issues.add("execution_does_not_cover_window")
    if execution.get("final_exit_included") is not True:
        execution_issues.add("final_exit_missing")
    if execution.get("flat_at_end") is not True:
        execution_issues.add("residual_or_unknown_position")
    return {
        "scenario": run.get("scenario"), "ledger_id": run.get("ledger_id"),
        "window": {"start_ms": start, "end_ms": end}, "coverage": coverage,
        "funding_mode": funding_mode, "funding_avoidance": avoidance,
        "data_coverage_qualified": not data_issues,
        "execution_qualified": not data_issues and not execution_issues,
        "data_issues": sorted(data_issues), "execution_issues": sorted(execution_issues),
        "scope": "declared evidence validation; not a profitability or live-readiness certificate",
    }


def validate_independent_runs(runs, catalog, as_of_ms):
    """Keep scenario ledgers separate and reject inflated independent samples."""
    reports = [validate_run(run, catalog, as_of_ms) for run in runs]
    issues, seen_ledgers, artifacts = [], set(), {}
    # Renaming a source ID cannot turn the same bytes into another market period.
    # Several streams may legitimately be views of the same immutable archive.
    for source_id, source in catalog.items():
        identity = tuple(source.get(k) for k in ("venue", "domain", "kind", "instrument"))
        for artifact in source.get("artifacts", []):
            digest = artifact.get("sha256")
            if not _hash(digest):
                continue
            key, span = (identity, digest), [artifact.get("start_ms"), artifact.get("end_ms")]
            if key in artifacts and artifacts[key][1] != span:
                issues.append({"source_id": source_id, "other_source_id": artifacts[key][0],
                               "reason": "artifact_reused_with_conflicting_interval"})
            artifacts[key] = (source_id, span)
    observed = set()
    for i, run in enumerate(runs):
        ledger = run.get("ledger_id")
        if ledger in seen_ledgers:
            issues.append({"run": i, "reason": "ledger_reused"})
        seen_ledgers.add(ledger)
        window = reports[i]["window"]
        domains = tuple(sorted(run.get("venue_domains", {}).items()))
        identity = (run.get("instrument"), domains, window["start_ms"], window["end_ms"])
        observed.add(identity)
        for j, other in enumerate(runs[:i]):
            if run.get("experiment_id") != other.get("experiment_id") or run.get("scenario") != other.get("scenario"):
                continue
            same_market = run.get("instrument") == other.get("instrument") and domains == tuple(sorted(other.get("venue_domains", {}).items()))
            overlap = max(window["start_ms"], reports[j]["window"]["start_ms"]) < min(window["end_ms"], reports[j]["window"]["end_ms"])
            if not same_market or not overlap:
                continue
            paired = (run.get("comparison_group") == other.get("comparison_group")
                      and run.get("independent_sample_id") == other.get("independent_sample_id")
                      and window == reports[j]["window"])
            if not paired:
                issues.append({"run": i, "other_run": j, "reason": "overlap_claimed_as_independent"})
            elif (run.get("fee_model_sha256") != other.get("fee_model_sha256")
                  or sorted(run.get("source_ids", [])) != sorted(other.get("source_ids", []))):
                issues.append({"run": i, "other_run": j, "reason": "paired_evidence_or_fee_model_mismatch"})
    return {"runs": reports, "scenario_counts": dict(Counter(r.get("scenario") for r in runs)),
            "run_count": len(runs), "distinct_market_time_windows": len(observed),
            "independence_issues": issues,
            "all_execution_qualified": bool(reports) and not issues and all(r["execution_qualified"] for r in reports)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("catalog", type=Path)
    parser.add_argument("runs", type=Path)
    parser.add_argument("--as-of-ms", type=int, required=True)
    args = parser.parse_args()
    result = validate_independent_runs(json.loads(args.runs.read_text()),
                                       json.loads(args.catalog.read_text()), args.as_of_ms)
    print(json.dumps(result, sort_keys=True, indent=2, allow_nan=False))
    return 0 if result["all_execution_qualified"] else 2


if __name__ == "__main__":
    raise SystemExit(main())
