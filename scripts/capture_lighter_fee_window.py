#!/usr/bin/env python3
"""Capture a separate, budgeted public fee-observation window on Oasis.

Never replaces a frozen collector. Only this driver's child process group can
be stopped. No account access, private stream, orders or cancellations.
"""
import argparse
from collections import Counter
import gzip
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import time

from capture_binance_window import digest, save_event
from capture_timing import ClockAudit


def validate(plan):
    for key in ("capture_from_ms", "capture_until_ms", "validation_start_ms", "validation_end_ms",
                "disk_reserve_bytes", "max_capture_bytes"):
        if type(plan.get(key)) is not int or plan[key] <= 0:
            raise ValueError("invalid fee capture plan")
    if (plan["validation_end_ms"] - plan["validation_start_ms"] != 36_000_000
            or not 60_000 <= plan["validation_start_ms"] - plan["capture_from_ms"] <= 900_000
            or not 15_000 <= plan["capture_until_ms"] - plan["validation_end_ms"] <= 900_000
            or plan["disk_reserve_bytes"] < 16 * 2**30
            or plan["max_capture_bytes"] > 512 * 2**20
            or not plan.get("protected_artifacts")):
        raise ValueError("invalid continuous window or resource boundary")
    for domain in ("lighter", "lighter_robinhood"):
        markets = plan["markets"][domain]
        if (not 1 <= len(markets) <= 2 or len(set(markets)) != len(markets)
                or any(type(m) is not int or not 0 <= m <= 65535 for m in markets)):
            raise ValueError("invalid independently resolved markets")


def protected_unchanged(plan):
    return all(digest(Path(p)) == expected for p, expected in plan["protected_artifacts"].items())


def seal(root, plan, plan_hash):
    partial = root / "public.ndjson.gz.partial"
    counts, clock = Counter(), ClockAudit()
    first, last = None, None
    with gzip.open(partial, "rb") as stream:
        for line in stream:
            row = json.loads(line)
            at = clock.observe(row)
            if last is not None and last["event"] == "end":
                raise ValueError("records after capture footer")
            first = at if first is None else first
            last = row
            counts[row["source"] + "/" + row["event"]] += 1
            if row["source"].endswith("_trades") and row["event"] == "message":
                if row["payload"].get("fee_fields_version") != 1:
                    raise ValueError("collector did not preserve fee evidence")
    if (not last or last["source"] != "collector" or last["event"] != "end"
            or first > plan["validation_start_ms"] - 60_000
            or last["available_at_ms"] < plan["validation_end_ms"] + 15_000
            or not protected_unchanged(plan)):
        raise ValueError("incomplete window, footer or protected artifact changed")
    sealed = root / "public.ndjson.gz"
    os.link(partial, sealed)
    report = {"state": "sealed_pending_continuity_and_fee_audit", "plan_sha256": plan_hash,
              "capture_sha256": digest(sealed), "bytes": sealed.stat().st_size,
              "first_ms": first, "last_ms": last["available_at_ms"], "counts": dict(counts),
              "protected_artifacts_unchanged": True, "strategy_validation": False,
              "confirmed_eligible_volume": None}
    with (root / "capture-summary.json").open("x") as stream:
        json.dump(report, stream, indent=2)
    save_event(root, report)
    return report


def run(root):
    plan_path, binary = root / "capture-plan.json", root / "perp-capture"
    plan_hash = digest(plan_path)
    plan = json.loads(plan_path.read_text())
    validate(plan)
    if digest(binary) != plan["collector_sha256"] or not protected_unchanged(plan):
        raise ValueError("collector or protected artifact changed")
    with (root / "capture-driver.lock").open("x") as stream:
        stream.write(str(os.getpid()))
    save_event(root, {"state": "scheduled", "plan_sha256": plan_hash})
    while time.time_ns() // 1_000_000 < plan["capture_from_ms"]:
        time.sleep(min(30, max(.001, (plan["capture_from_ms"] - time.time_ns() // 1_000_000) / 1000)))
    now = time.time_ns() // 1_000_000
    if (now > plan["validation_start_ms"] - 60_000 or digest(plan_path) != plan_hash
            or digest(binary) != plan["collector_sha256"] or not protected_unchanged(plan)):
        raise ValueError("missed warmup or changed frozen identity; no window reset")
    if shutil.disk_usage(root).free < plan["disk_reserve_bytes"] + 256 * 2**20:
        raise ValueError("insufficient reserve for independent capture")
    partial = root / "public.ndjson.gz.partial"
    command = [str(binary), "-lighter", ",".join(map(str, plan["markets"]["lighter"])),
               "-lighter-trades", "-lighter-robinhood", ",".join(map(str, plan["markets"]["lighter_robinhood"])),
               "-lighter-robinhood-trades", "-lighter-clock-refresh", "4m", "-precise-times",
               "-duration", str(plan["capture_until_ms"] - now) + "ms", "-gzip", "-output", str(partial)]
    reason = None
    with (root / "capture.log").open("xb") as log:
        process = subprocess.Popen(["nice", "-n", "15", "ionice", "-c", "3", *command],
                                   stdout=log, stderr=log, env=dict(os.environ, GOMAXPROCS="1"), start_new_session=True)
        save_event(root, {"state": "capturing", "pid": process.pid})
        while process.poll() is None:
            if shutil.disk_usage(root).free < plan["disk_reserve_bytes"]:
                reason = "disk_reserve"
            elif partial.exists() and partial.stat().st_size > plan["max_capture_bytes"]:
                reason = "capture_budget"
            if reason:
                os.killpg(process.pid, signal.SIGINT)
                try:
                    process.wait(timeout=15)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGTERM)
                break
            time.sleep(5)
        code = process.wait()
    if reason or code:
        save_event(root, {"state": "incomplete", "reason": reason, "exit_code": code})
        return 1
    if digest(plan_path) != plan_hash or digest(binary) != plan["collector_sha256"]:
        raise ValueError("capture identity changed")
    seal(root, plan, plan_hash)
    return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("campaign", type=Path)
    args = parser.parse_args()
    os.umask(0o077)
    root = args.campaign.resolve(strict=True)
    try:
        return run(root)
    except Exception as error:
        save_event(root, {"state": "failed", "error_type": type(error).__name__})
        raise


if __name__ == "__main__":
    raise SystemExit(main())
