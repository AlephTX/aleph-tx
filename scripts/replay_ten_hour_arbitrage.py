#!/usr/bin/env python3
"""Run one continuous basis ledger with 10 h decisions and a real tape drain.

The first minute is warm-up outside the decision interval. New cycles stop at
the decision cutoff; real later frames execute the terminal unwind. Missing
boundary evidence or remaining exposure fails the diagnostic. Fee and stream
quality verification remain external prerequisites for execution qualification.
"""
import argparse
from decimal import Decimal, InvalidOperation
from hashlib import file_digest
from itertools import chain
import json
import os
from pathlib import Path
import subprocess
import threading
import time

from archive_public_capture import verify_compiled
from replay_maker_campaign import save, load

TEN_HOURS = 36_000_000
ROUTE_FEES = {"lighter-hyperliquid": ("0", "4.5"), "lighter-extended": ("0", "2.5")}


def digest(path):
    with path.open("rb") as stream:
        return file_digest(stream, "sha256").hexdigest()


def stop(process):
    if process is None or process.poll() is not None:
        return
    try:
        process.terminate()
    except ProcessLookupError:
        pass
    try:
        process.wait(timeout=2)
    except subprocess.TimeoutExpired:
        try:
            process.kill()
        except ProcessLookupError:
            pass
        process.wait(timeout=2)


def write_all(stream, data):
    view = memoryview(data)
    while view:
        written = stream.write(view)
        if not written:
            raise BrokenPipeError("engine input closed")
        view = view[written:]


def terminal_report(report):
    value = report.get("terminal_drain")
    return value if isinstance(value, dict) else {}


def fee_pair(values):
    try:
        fees = tuple(Decimal(value) for value in values)
    except InvalidOperation as error:
        raise ValueError("invalid decimal taker fee") from error
    if len(fees) != 2 or any(not fee.is_finite() or not 0 <= fee < 10_000 for fee in fees):
        raise ValueError("two finite taker fees in [0, 10000) bps are required")
    return tuple(map(str, fees))


def run_replay(engine, source, out, timeout=180, decompressor=None, drain_ms=60_000,
               start_ms=None, archive_sha256=None, route="lighter-hyperliquid", taker_fees_bps=None):
    """Retain a manifest even on failure; no source record body is exported."""
    if not 0 < timeout <= 3600:
        raise ValueError("timeout must be positive and no more than one hour")
    if not isinstance(drain_ms, int) or not 0 < drain_ms <= 3_600_000:
        raise ValueError("drain must be a positive integer and no more than one hour")
    if route not in ROUTE_FEES:
        raise ValueError("unsupported research route")
    fees = fee_pair(ROUTE_FEES[route] if taker_fees_bps is None else taker_fees_bps)
    os.umask(0o077)
    engine, source, out = Path(engine).resolve(), Path(source).resolve(), Path(out).resolve()
    out.mkdir()
    link = out / "input.ndjson"
    link.symlink_to("/dev/stdin")
    first = begin = end = previous = last_replayed = first_decision = boundary = None
    drain_end = drain_boundary = last_decision = None
    frames = maximum_gap = 0
    witnessed, reduced_budget_frames, zero_budget_frames = [0, 0], [0, 0], [0, 0]
    failures, children = [], []
    engine_code = archive_code = None
    source_hash = engine_hash = config_hash = None
    report = {}
    archive_eof = False
    child = archive = None
    timed_out = threading.Event()
    child_lock = threading.Lock()

    def expire():
        timed_out.set()
        with child_lock:
            for process in children:
                if process.poll() is None:
                    try:
                        process.kill()
                    except ProcessLookupError:
                        pass

    def spawn(*args, **kwargs):
        with child_lock:
            process = subprocess.Popen(*args, **kwargs)
            children.append(process)
            if timed_out.is_set():
                process.kill()
            return process

    timer = threading.Timer(timeout, expire)
    timer.daemon = True
    deadline = time.monotonic() + timeout
    timer.start()
    try:
        source_before = source.stat()
        source_hash, engine_hash = digest(source), digest(engine)
        with (out / "engine-report.json").open("x") as result, \
                (out / "engine.stderr").open("x") as errors, \
                (out / "archive.stderr").open("x") as archive_errors:
            archive = spawn(decompressor or ["zstd", "-qdc", str(source)],
                            stdout=subprocess.PIPE, stderr=archive_errors)
            stream = archive.stdout
            try:
                header = json.loads(stream.readline())
                initial_line = stream.readline()
                first = json.loads(initial_line)["now_ms"]
                begin = start_ms if start_ms is not None else first + 60_000
                if begin - first < 60_000:
                    raise ValueError("requested window lacks sixty seconds of warm-up")
                end = begin + TEN_HOURS
                drain_end = end + drain_ms
                pair = header["pair"]
                if [leg["venue"] for leg in pair["legs"]] != route.split("-"):
                    raise ValueError("tape legs do not match the requested route")
                pair["mode"] = {"kind": "pair_hedge"}
                pair["strategy"] = "basis_reversion"
                pair["basis"].update(entry_z=1.75, max_sample_gap_ms=15_000)
                for key in ("max_leg_notional_usd", "max_unhedged_notional_usd"):
                    pair["planner"][key] = min(pair["planner"][key], 100)
                for leg, fee in zip(pair["legs"], fees, strict=True):
                    leg["taker_fee_bps"] = fee
                header.update(one_way_latency_ms=550, trade_from_ms=begin, funding_guard_ms=1000,
                              trade_until_ms=end, drain_until_ms=drain_end)
                save(out / "header.json", header)
                config_hash = digest(out / "header.json")
                child = spawn([str(engine), "perp-replay", str(link)], stdin=subprocess.PIPE,
                              stdout=result, stderr=errors, bufsize=0)
                write_all(child.stdin, (json.dumps(header) + "\n").encode())
                for line in chain((initial_line,), stream):
                    row = json.loads(line)
                    now = row["now_ms"]
                    if previous is not None and now <= previous:
                        raise ValueError("noncausal input")
                    if previous is not None:
                        maximum_gap = max(maximum_gap, now - previous)
                    if now >= end and boundary is None:
                        boundary = now
                    previous = now
                    if len(row["legs"]) != 2:
                        raise ValueError("unexpected leg count")
                    for i, leg in enumerate(row["legs"]):
                        allowance = Decimal(str(leg["book"]["available_notional"]))
                        if not allowance.is_finite() or allowance < 0:
                            raise ValueError("invalid capacity evidence")
                        capped = min(allowance, Decimal(100))
                        reduced_budget_frames[i] += int(capped < allowance)
                        zero_budget_frames[i] += int(capped == 0)
                        # Preserve bids/asks, timestamps and tradability. This
                        # cap may only reduce an existing allowance, never add it.
                        leg["book"]["available_notional"] = format(capped, "f")
                        witnessed[i] += len(leg.get("settled", [])) + len(leg.get("witnessed_settlements", []))
                    write_all(child.stdin, (json.dumps(row, separators=(",", ":")) + "\n").encode())
                    frames += 1
                    last_replayed = now
                    if begin <= now < end:
                        last_decision = now
                        if first_decision is None:
                            first_decision = now
                    if now >= drain_end:
                        drain_boundary = now
                        break
            except Exception as error:
                failures.append("input_or_stream_failure:" + type(error).__name__)
            finally:
                if child is not None and child.stdin is not None:
                    try:
                        child.stdin.close()
                    except OSError:
                        failures.append("engine_input_close_failed")
                # Even a malformed tape / failed engine must not turn an
                # unchecked truncated compressed archive into valid evidence.
                for _ in iter(lambda: stream.read(1024 * 1024), b""):
                    pass
                archive_eof = True
                stream.close()
            archive_code = archive.wait(timeout=max(0.01, deadline - time.monotonic()))
            if child is not None:
                engine_code = child.wait(timeout=max(0.01, deadline - time.monotonic()))
        source_after = source.stat()
        if (source_before.st_size, source_before.st_mtime_ns, source_before.st_ino) != \
                (source_after.st_size, source_after.st_mtime_ns, source_after.st_ino):
            failures.append("source_changed_during_replay")
        if archive_code != 0:
            failures.append("compressed_archive_verification_failed")
        if engine_code != 0:
            failures.append("engine_exit_failure")
        if engine_code == 0:
            loaded = load(out / "engine-report.json")
            if not isinstance(loaded, dict):
                raise ValueError("engine report is not an object")
            report = loaded
            if report.get("frames") != frames:
                failures.append("engine_did_not_consume_all_submitted_frames")
            if report.get("halted") is not None:
                failures.append("engine_halted")
        if boundary is None:
            failures.append("source_does_not_reach_cutoff")
        if drain_boundary is None:
            failures.append("source_does_not_reach_drain_cutoff")
        if not terminal_report(report).get("complete", False):
            failures.append("terminal_drain_incomplete")
    except Exception as error:
        failures.append("runner_failure:" + type(error).__name__)
    finally:
        timer.cancel()
        for process in reversed(children):
            try:
                stop(process)
            except (OSError, subprocess.TimeoutExpired):
                failures.append("subprocess_cleanup_failed")
            for pipe in (process.stdin, process.stdout):
                if pipe is not None:
                    try:
                        pipe.close()
                    except OSError:
                        failures.append("subprocess_pipe_close_failed")
        timer.join(timeout=2)
        if timed_out.is_set():
            failures.append("runner_deadline_expired")
        if child is not None:
            engine_code = child.poll()
        if archive is not None:
            archive_code = archive.poll()
        link.unlink(missing_ok=True)
    last_submitted = last_replayed
    # Pipe writes prove submission, not execution. Only a successful engine
    # report consuming every frame can attest to the last replayed frame.
    if engine_code != 0 or report.get("frames") != frames:
        last_replayed = None
    else:
        last_replayed = terminal_report(report).get("last_execution_frame_ms")
    terminal_gap = drain_end - last_replayed if drain_end is not None and last_replayed is not None else None
    terminal_complete = terminal_report(report).get("complete", False)
    manifest = {
        "scenario": "arbitrage", "sample": "historical_reuse", "ledger_restarts": 0,
        "decision_window": {"start_ms": begin, "end_ms": end}, "warmup_start_ms": first,
        "drain_window": {"start_ms": end, "end_ms": drain_end},
        "first_decision_frame_ms": first_decision, "last_replayed_frame_ms": last_replayed,
        "last_submitted_frame_ms": last_submitted,
        "first_frame_at_or_after_cutoff_ms": boundary, "terminal_gap_ms": terminal_gap,
        "first_frame_at_or_after_drain_until_ms": drain_boundary,
        "decision_cutoff_gap_ms": end - last_decision if last_decision is not None else None,
        "source_covers_cutoff": boundary is not None,
        "source_covers_drain_cutoff": drain_boundary is not None,
        "leading_decision_gap_ms": first_decision - begin if first_decision is not None else None,
        "frames": frames, "max_observed_frame_gap_ms": maximum_gap,
        "funding_evidence_events": witnessed, "engine_exit_code": engine_code,
        "archive_exit_code": archive_code, "source_sha256": source_hash, "engine_sha256": engine_hash,
        "archive_manifest_sha256": archive_sha256,
        "compressed_archive_verified": archive_eof and archive_code == 0,
        "runner_sha256": digest(Path(__file__)), "config_sha256": config_hash,
        "fees_account_verified": False, "coverage_quality_verified": False,
        "terminal_execution_complete": terminal_complete,
        "execution_qualified": False, "execution_blockers": ["fees_and_stream_quality_unverified"] +
            ([] if terminal_complete else ["terminal_drain_incomplete"]),
        "production_qualified": False, "diagnostic_run_succeeded": not failures,
        "failures": sorted(set(failures)),
        "assumptions": {"route": route, "one_way_latency_ms": 550, "taker_fees_bps": list(fees),
            "fee_scenario": "public_reference" if taker_fees_bps is None else "explicit_sensitivity",
            "strategy": "basis_reversion", "mode": "pair_hedge", "entry_z": 1.75,
            "warmup_ms": begin - first if begin is not None and first is not None else None,
            "max_sample_gap_ms": 15_000, "funding_guard_ms": 1000,
            "available_notional_rule": "min(original,100)", "planner_limits_rule": "min(original,100)",
            "book_depth": "unchanged", "end_policy": "real post-cutoff IOC drain", "drain_ms": drain_ms,
            "zero_allowance_frames": zero_budget_frames, "allowance_reduced_frames": reduced_budget_frames},
        "result": {k: report.get(k) for k in ("frames", "entry_signals", "entries_submitted", "paired_entries",
            "gross_turnover", "fees", "funding", "open_positions_base", "realized_pnl",
            "final_liquidation_pnl", "max_unhedged_ms", "opening_execution", "closing_execution",
            "halted", "open_phase", "entry_diagnostics", "terminal_drain")},
    }
    save(out / "manifest.json", manifest)
    return manifest


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("engine", "source", "output"):
        parser.add_argument(name, type=Path)
    parser.add_argument("--timeout-seconds", type=float, default=180)
    parser.add_argument("--drain-seconds", type=int, default=60)
    parser.add_argument("--start-ms", type=int, help="UTC decision start shared with the other research scenarios")
    parser.add_argument("--public-archive", type=Path, help="verify shared archive provenance before replay")
    parser.add_argument("--route", choices=ROUTE_FEES, default="lighter-hyperliquid")
    parser.add_argument("--taker-fees-bps", nargs=2, metavar=("LIGHTER", "OTHER"),
                        help="fee sensitivity scenario in tape leg order")
    args = parser.parse_args()
    if args.taker_fees_bps is not None:
        try:
            args.taker_fees_bps = fee_pair(args.taker_fees_bps)
        except ValueError as error:
            parser.error(str(error))
    archive_sha256 = None
    if args.public_archive:
        try:
            archive_sha256 = verify_compiled(args.public_archive, args.source)["archive_manifest_sha256"]
        except (OSError, ValueError, KeyError) as error:
            parser.error(str(error))
    manifest = run_replay(args.engine, args.source, args.output, args.timeout_seconds,
                          drain_ms=args.drain_seconds * 1000, start_ms=args.start_ms,
                          archive_sha256=archive_sha256, route=args.route, taker_fees_bps=args.taker_fees_bps)
    return 0 if manifest["diagnostic_run_succeeded"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
