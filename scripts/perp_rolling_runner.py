#!/usr/bin/env python3
"""24x7 Perpetual 10h15m Rolling Capture Supervisor (`w01 -> w02 -> w03 -> ...`).

Runs an isolated `perp-capture` stream in an unbroken 24/7 loop of fixed
`10h15m` (`36900s`) validation windows:
  1. Writes each active window to `<campaign_id>.ndjson.gz.partial`.
  2. When a full `10h15m` window finishes naturally (`returncode == 0`),
     atomically renames `.partial` -> `.ndjson.gz` (`< 1 ms`), launches an
     asynchronous `SCHED_IDLE` + `ionice -c 3` `flywheel_sync.py ship-public`
     job (`seal-bronze` + optional `rclone` upload + `< 16 GiB` free-space
     eviction guard), and immediately starts `w{N+1}` (`< 50 ms` gap).
  3. On `SIGTERM`/`SIGINT` or unexpected non-zero exit, moves any incomplete or
     short-duration `.partial` file into `quarantine/` with an immutable
     `.quarantine_manifest.json` so `selection_dev` only ever contains genuine
     completed `10h15m` windows.
"""

from __future__ import annotations

import argparse
from datetime import datetime, timezone
from hashlib import sha256
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import time

sys.path.insert(0, str(Path(__file__).resolve().parent))

from flywheel_sync import build_idle_command
from quant_lakehouse import VALID_REGIONS, VALID_ROLES, write_immutable_json


WINDOW_RE = re.compile(r"-w([0-9]{2,})(?:\.ndjson\.gz|\.ndjson)")


def _sha256_and_size(path: Path) -> tuple[str, int]:
    h = sha256()
    total = 0
    with path.open("rb") as fh:
        while chunk := fh.read(256 * 1024):
            h.update(chunk)
            total += len(chunk)
    return h.hexdigest(), total


def next_window_index(output_dir: Path, quarantine_dir: Path, stream_name: str) -> int:
    """Return strictly monotonic next `wNN` index across active, evicted, and quarantined files."""
    max_idx = 0
    prefix = f"-{stream_name}-"
    for folder in (output_dir, quarantine_dir):
        if not folder.is_dir():
            continue
        for entry in folder.iterdir():
            if prefix not in entry.name:
                continue
            m = WINDOW_RE.search(entry.name)
            if m:
                max_idx = max(max_idx, int(m.group(1)))
    return max_idx + 1


def quarantine_partial_file(
    partial_path: Path,
    quarantine_dir: Path,
    *,
    campaign_id: str,
    collector_region: str,
    abort_reason: str,
    started_epoch: float,
    ended_epoch: float,
) -> Path | None:
    """Move an interrupted or crashed `.partial` file into `quarantine_dir` with SHA-256 metadata."""
    if not partial_path.exists():
        return None
    quarantine_dir.mkdir(parents=True, exist_ok=True, mode=0o755)
    dest_raw = quarantine_dir / partial_path.name
    suffix = 1
    while dest_raw.exists():
        dest_raw = quarantine_dir / f"{partial_path.name}.{suffix}"
        suffix += 1
    os.replace(partial_path, dest_raw)
    digest, size_bytes = _sha256_and_size(dest_raw)
    manifest_path = quarantine_dir / f"{campaign_id}.quarantine_manifest.json"
    if not manifest_path.exists():
        manifest = {
            "schema_version": 1,
            "manifest_kind": "quarantine_partial",
            "campaign_id": campaign_id,
            "created_at_utc": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
            "collector_region": collector_region,
            "split_role": "quarantine_diagnostic",
            "observed_window_utc": {
                "start": datetime.fromtimestamp(started_epoch, tz=timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
                "end": datetime.fromtimestamp(ended_epoch, tz=timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
                "duration_seconds": max(0, int(round(ended_epoch - started_epoch))),
            },
            "raw_file": {
                "name": dest_raw.name,
                "size_bytes": size_bytes,
                "sha256": digest,
            },
            "sealed_cleanly": False,
            "abort_reason": abort_reason,
            "notes": "Short or interrupted capture isolated from 10h validation per AGENTS.md.",
        }
        write_immutable_json(manifest_path, manifest)
    return dest_raw


def sweep_orphaned_partials(
    output_dir: Path, quarantine_dir: Path, *, collector_region: str
) -> list[str]:
    """Quarantine any leftover `.partial` files found on supervisor startup."""
    moved: list[str] = []
    if not output_dir.is_dir():
        return moved
    now = time.time()
    for entry in sorted(output_dir.iterdir(), key=lambda p: p.name):
        if entry.is_file() and entry.name.endswith(".partial"):
            stem = entry.name
            for suf in (".ndjson.gz.partial", ".ndjson.partial", ".partial"):
                if stem.endswith(suf):
                    stem = stem[: -len(suf)]
                    break
            mtime = entry.stat().st_mtime
            dest = quarantine_partial_file(
                entry,
                quarantine_dir,
                campaign_id=f"{stem}-orphan",
                collector_region=collector_region,
                abort_reason="orphaned_partial_on_runner_startup",
                started_epoch=mtime,
                ended_epoch=now,
            )
            if dest is not None:
                moved.append(dest.name)
    return moved


def run_rolling_loop(
    *,
    stream_name: str,
    output_dir: Path,
    quarantine_dir: Path,
    perp_capture_bin: Path,
    flywheel_script: Path,
    capture_args: list[str],
    window_duration: str = "36900s",
    collector_region: str = "tokyo",
    split_role: str = "selection_dev",
    remote_dir: str = "",
    evict_below_free_gib: float = 16.0,
    max_windows: int = 0,
    failure_backoff_secs: float = 3.0,
) -> dict:
    if not re.fullmatch(r"[a-z0-9][a-z0-9_-]{0,31}", stream_name):
        raise ValueError(f"invalid stream_name: {stream_name!r}")
    if collector_region not in VALID_REGIONS:
        raise ValueError(f"invalid collector_region: {collector_region!r}")
    if split_role not in VALID_ROLES:
        raise ValueError(f"invalid split_role: {split_role!r}")

    output_dir.mkdir(parents=True, exist_ok=True, mode=0o755)
    quarantine_dir.mkdir(parents=True, exist_ok=True, mode=0o755)

    orphaned = sweep_orphaned_partials(
        output_dir, quarantine_dir, collector_region=collector_region
    )

    stop_requested = False
    active_proc: subprocess.Popen | None = None
    bg_shippers: list[subprocess.Popen] = []

    def _handle_stop(signum, _frame) -> None:
        nonlocal stop_requested
        stop_requested = True
        if active_proc is not None and active_proc.poll() is None:
            try:
                active_proc.send_signal(signum)
            except ProcessLookupError:
                pass

    prev_sigterm = signal.signal(signal.SIGTERM, _handle_stop)
    prev_sigint = signal.signal(signal.SIGINT, _handle_stop)

    completed_windows: list[str] = []
    quarantined_windows: list[str] = []

    try:
        while not stop_requested:
            # Reap finished background shippers so zero zombies accumulate.
            bg_shippers = [p for p in bg_shippers if p.poll() is None]

            win_idx = next_window_index(output_dir, quarantine_dir, stream_name)
            started_epoch = time.time()
            utc_stamp = datetime.fromtimestamp(started_epoch, tz=timezone.utc).strftime(
                "%Y%m%dT%H%M%SZ"
            )
            campaign_id = f"{collector_region}-{stream_name}-{utc_stamp}-w{win_idx:02d}"
            partial_path = output_dir / f"{campaign_id}.ndjson.gz.partial"
            final_path = output_dir / f"{campaign_id}.ndjson.gz"

            cmd = [
                str(perp_capture_bin),
                *capture_args,
                "-precise-times",
                "-gzip",
                "-duration",
                window_duration,
                "-output",
                str(partial_path),
            ]
            print(
                f"[rolling:{stream_name}] starting window w{win_idx:02d}: {campaign_id} ({window_duration})",
                flush=True,
            )
            active_proc = subprocess.Popen(cmd)
            rc = active_proc.wait()
            active_proc = None
            ended_epoch = time.time()

            if rc == 0 and not stop_requested and partial_path.exists():
                os.replace(partial_path, final_path)
                completed_windows.append(final_path.name)
                print(
                    f"[rolling:{stream_name}] sealed window w{win_idx:02d} -> {final_path.name}; spawning async Bronze shipper",
                    flush=True,
                )
                ship_cmd = build_idle_command(
                    [
                        sys.executable,
                        str(flywheel_script),
                        "ship-public",
                        "--capture-dir",
                        str(output_dir),
                        "--remote-dir",
                        remote_dir,
                        "--collector-region",
                        collector_region,
                        "--split-role",
                        split_role,
                        "--evict-below-free-gib",
                        str(evict_below_free_gib),
                        "--min-evict-age-secs",
                        "0",
                    ]
                )
                bg_shippers.append(subprocess.Popen(ship_cmd))

                if max_windows > 0 and (len(completed_windows) + len(quarantined_windows)) >= max_windows:
                    break
                continue

            reason = (
                "operator_signal_stop"
                if stop_requested
                else f"perp_capture_exit_{rc}"
            )
            q_dest = quarantine_partial_file(
                partial_path,
                quarantine_dir,
                campaign_id=campaign_id,
                collector_region=collector_region,
                abort_reason=reason,
                started_epoch=started_epoch,
                ended_epoch=ended_epoch,
            )
            if q_dest is not None:
                quarantined_windows.append(q_dest.name)
                print(
                    f"[rolling:{stream_name}] quarantined {q_dest.name} (reason={reason})",
                    flush=True,
                )

            if stop_requested:
                break
            if max_windows > 0 and (len(completed_windows) + len(quarantined_windows)) >= max_windows:
                break

            deadline = time.time() + failure_backoff_secs
            while not stop_requested and time.time() < deadline:
                time.sleep(min(0.1, max(0.01, deadline - time.time())))
    finally:
        signal.signal(signal.SIGTERM, prev_sigterm)
        signal.signal(signal.SIGINT, prev_sigint)
        for p in bg_shippers:
            try:
                p.wait(timeout=30)
            except subprocess.TimeoutExpired:
                pass

    return {
        "stream_name": stream_name,
        "orphaned_swept": orphaned,
        "completed_windows": completed_windows,
        "quarantined_windows": quarantined_windows,
    }


def main() -> None:
    argv = sys.argv[1:]
    if "--" in argv:
        sep = argv.index("--")
        own_argv, capture_args = argv[:sep], argv[sep + 1 :]
    else:
        own_argv, capture_args = argv, []

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--stream-name", required=True, help="Short stream slug: crypto, rwa, or bulk")
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--quarantine-dir", type=Path, required=True)
    parser.add_argument(
        "--perp-capture-bin",
        type=Path,
        default=Path("/opt/aleph-tx/current/bin/perp-capture"),
    )
    parser.add_argument(
        "--flywheel-script",
        type=Path,
        default=Path(__file__).resolve().parent / "flywheel_sync.py",
    )
    parser.add_argument("--window-duration", default="36900s")
    parser.add_argument("--collector-region", choices=VALID_REGIONS, default="tokyo")
    parser.add_argument("--split-role", choices=VALID_ROLES, default="selection_dev")
    parser.add_argument("--remote-dir", default=os.environ.get("ALEPH_PUBLIC_BRONZE_REMOTE", ""))
    parser.add_argument("--evict-below-free-gib", type=float, default=16.0)
    parser.add_argument("--max-windows", type=int, default=0)
    args = parser.parse_args(own_argv)

    if not capture_args:
        raise SystemExit("pass perp-capture arguments after `--`")

    run_rolling_loop(
        stream_name=args.stream_name,
        output_dir=args.output_dir,
        quarantine_dir=args.quarantine_dir,
        perp_capture_bin=args.perp_capture_bin,
        flywheel_script=args.flywheel_script,
        capture_args=capture_args,
        window_duration=args.window_duration,
        collector_region=args.collector_region,
        split_role=args.split_role,
        remote_dir=args.remote_dir,
        evict_below_free_gib=args.evict_below_free_gib,
        max_windows=args.max_windows,
    )


if __name__ == "__main__":
    main()
