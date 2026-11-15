#!/usr/bin/env python3
"""Idle-priority Live-as-Data-Flywheel shipper and private telemetry backup.

Implements two strictly isolated background pipelines for cloud nodes (oasis-tokyo):
  1. Public Bronze Shipper (`ship-public`):
     - Discovers only closed/sealed `.ndjson.gz` / `.ndjson.zst` files (skipping
       `.partial`, `.tmp`, `.next`, symlinks, and private-indicating names).
     - Emits `.bronze_manifest.json` via `quant_lakehouse.seal_bronze_capture`.
     - Uploads under Linux idle scheduling (`chrt -i 0 nice -n 19 ionice -c 3`)
       with `rclone copyto --bwlimit 3M --drive-chunk-size 64M`.
     - Verifies remote byte length and SHA-256 before writing `.upload_receipt.json`
       and optionally evicts local files (`quant_lakehouse.evict_local_capture`).
  2. Private Telemetry Hot-Backup (`backup-private`):
     - Hard-blocks any destination path resembling public Bronze/Silver lakehouse
       trees (`10_bronze_raw`, `20_silver`, `public`, etc.).
     - Uses SQLite's online `Connection.backup()` API (`pages=256, sleep=0.005`)
       so `aleph-tx-monitor` WAL writes are never corrupted or stalled.
     - Compresses only completed `<session_id>.jsonl` files (containing a
       terminal `{"type":"stopped",...}` record) to `0600` `.jsonl.gz` archives.
"""

from __future__ import annotations

import argparse
from datetime import datetime, timezone
import gzip
from hashlib import sha256
import json
import os
from pathlib import Path
import re
import shutil
import sqlite3
import subprocess
import time
from typing import Callable

from quant_lakehouse import (
    PRIVATE_NAME,
    VALID_REGIONS,
    VALID_ROLES,
    digest_unchanged,
    evict_local_capture,
    reject_unsafe_path,
    seal_bronze_capture,
    write_immutable_json,
)


FORBIDDEN_PRIVATE_DEST_PATTERNS = re.compile(
    r"(?:^|/|:)(?:00_catalog|10_bronze_raw|20_silver_normalized|30_external_evidence|40_gold_experiments|public_bronze|public)(?:/|$)",
    re.IGNORECASE,
)


def build_idle_command(
    cmd: list[str],
    *,
    which: Callable[[str], str | None] = shutil.which,
) -> list[str]:
    """Prefix subprocess commands with Linux idle CPU & disk I/O schedulers when available."""
    prefix: list[str] = []
    if which("chrt") is not None:
        prefix.extend(["chrt", "-i", "0"])
    if which("nice") is not None:
        prefix.extend(["nice", "-n", "19"])
    if which("ionice") is not None:
        prefix.extend(["ionice", "-c", "3"])
    return [*prefix, *cmd]


def validate_private_destination(target: str) -> str:
    """Reject destinations that could leak private telemetry into public Lakehouse folders."""
    cleaned = target.strip()
    if not cleaned:
        raise ValueError("private destination must not be empty")
    if FORBIDDEN_PRIVATE_DEST_PATTERNS.search(cleaned):
        raise ValueError(
            f"refusing to route private telemetry into public Lakehouse path: {cleaned!r}"
        )
    return cleaned


def _infer_campaign_id(filename: str) -> str:
    stem = filename
    for suffix in (".ndjson.gz", ".ndjson.zst", ".ndjson", ".gz", ".zst"):
        if stem.endswith(suffix):
            stem = stem[: -len(suffix)]
            break
    # Strip `.segNNNN` suffix so all segments of one rolling run share campaign_id.
    stem = re.sub(r"\.seg[0-9]{4,}$", "", stem)
    return stem


def _discover_closed_public_captures(capture_dir: Path) -> list[Path]:
    resolved_dir = reject_unsafe_path(capture_dir, allow_directory=True)
    if not resolved_dir.is_dir():
        raise ValueError(f"capture_dir is not a directory: {capture_dir}")
    candidates: list[Path] = []

    def _scan_one_dir(folder: Path) -> None:
        for entry in sorted(folder.iterdir(), key=lambda p: p.name):
            name = entry.name
            if entry.is_symlink() or not entry.is_file():
                continue
            if (
                ".partial" in name
                or name.endswith((".tmp", ".next", ".json"))
                or PRIVATE_NAME.search(name)
            ):
                continue
            if name.endswith((".ndjson.gz", ".ndjson.zst", ".ndjson")):
                candidates.append(entry)

    _scan_one_dir(resolved_dir)
    for sub in sorted(resolved_dir.iterdir(), key=lambda p: p.name):
        if sub.is_symlink() or not sub.is_dir():
            continue
        if sub.name.lower().startswith("quarantine") or PRIVATE_NAME.search(sub.name):
            continue
        _scan_one_dir(sub)
    return candidates


def _verify_remote_rclone_object(
    remote_object: str,
    *,
    expected_sha256: str,
    expected_bytes: int,
    which: Callable[[str], str | None] = shutil.which,
    runner: Callable[..., subprocess.CompletedProcess] = subprocess.run,
) -> dict:
    ls_cmd = build_idle_command(["rclone", "lsjson", "--", remote_object], which=which)
    ls_proc = runner(ls_cmd, capture_output=True, text=True, check=True)
    items = json.loads(ls_proc.stdout)
    if len(items) != 1 or int(items[0].get("Size", -1)) != expected_bytes:
        raise ValueError(
            f"remote size mismatch for {remote_object}: got {items}, want {expected_bytes}"
        )

    hash_cmd = build_idle_command(
        ["rclone", "hashsum", "SHA-256", "--", remote_object],
        which=which,
    )
    hash_proc = runner(hash_cmd, capture_output=True, text=True, check=True)
    remote_sha = ""
    for line in hash_proc.stdout.splitlines():
        parts = line.strip().split()
        if parts and re.fullmatch(r"[0-9a-fA-F]{64}", parts[0]):
            remote_sha = parts[0].lower()
            break
    if remote_sha != expected_sha256.lower():
        raise ValueError(
            f"remote SHA-256 mismatch for {remote_object}: got {remote_sha!r}, want {expected_sha256!r}"
        )
    return {
        "schema_version": 1,
        "remote_uri": f"rclone://{remote_object}",
        "sha256": remote_sha,
        "bytes": expected_bytes,
        "verified_at_utc": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
    }


def ship_public_bronze(
    capture_dir: Path,
    *,
    remote_dir: str | None = None,
    collector_region: str,
    split_role: str,
    campaign_id: str | None = None,
    bwlimit: str = "3M",
    drive_chunk_size: str = "64M",
    evict_after_verify: bool = False,
    evict_below_free_gib: float | None = None,
    min_evict_age_secs: float = 0.0,
    now_epoch: float | None = None,
    disk_free_bytes_fn: Callable[[Path], int] = lambda p: shutil.disk_usage(p).free,
    which: Callable[[str], str | None] = shutil.which,
    runner: Callable[..., subprocess.CompletedProcess] = subprocess.run,
) -> dict:
    """Seal, idle-upload, hash-verify, and optionally evict closed Bronze segments."""
    if collector_region not in VALID_REGIONS:
        raise ValueError(f"invalid collector_region: {collector_region!r}")
    if split_role not in VALID_ROLES:
        raise ValueError(f"invalid split_role: {split_role!r}")
    remote_base = (remote_dir or "").strip().rstrip("/")
    if remote_base and ":" not in remote_base:
        raise ValueError(f"remote_dir must be empty or an rclone remote path (e.g. gdrive:...), got {remote_dir!r}")

    resolved_root = reject_unsafe_path(capture_dir, allow_directory=True)
    now_ts = time.time() if now_epoch is None else now_epoch
    sealed = []
    shipped = []
    evicted = []
    eviction_candidates: list[tuple[float, Path, Path, Path]] = []

    for cap_path in _discover_closed_public_captures(resolved_root):
        cid = campaign_id or _infer_campaign_id(cap_path.name)
        manifest_path = cap_path.with_name(f"{cap_path.name}.bronze_manifest.json")
        receipt_path = cap_path.with_name(f"{cap_path.name}.upload_receipt.json")

        if manifest_path.exists():
            manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
            cur_sha, cur_bytes = digest_unchanged(cap_path)
            if cur_sha != manifest.get("capture_sha256") or cur_bytes != manifest.get("capture_bytes"):
                raise ValueError(f"sealed capture mutated on disk: {cap_path}")
        else:
            try:
                manifest = seal_bronze_capture(
                    cap_path,
                    campaign_id=cid,
                    collector_region=collector_region,
                    split_role=split_role,
                    manifest_out=manifest_path,
                )
            except FileExistsError:
                manifest = json.loads(manifest_path.read_text(encoding="utf-8"))

        sealed.append(cap_path.name)
        if not remote_base:
            continue

        rel_subdir = (
            cap_path.parent.relative_to(resolved_root).as_posix()
            if cap_path.parent != resolved_root
            else ""
        )
        remote_folder = f"{remote_base}/{rel_subdir}" if rel_subdir else remote_base
        remote_cap = f"{remote_folder}/{cap_path.name}"
        remote_manifest = f"{remote_folder}/{manifest_path.name}"

        if not receipt_path.exists():
            for local_src, remote_dst in ((cap_path, remote_cap), (manifest_path, remote_manifest)):
                copy_cmd = build_idle_command(
                    [
                        "rclone",
                        "copyto",
                        "--bwlimit",
                        bwlimit,
                        "--drive-chunk-size",
                        drive_chunk_size,
                        "--",
                        str(local_src),
                        remote_dst,
                    ],
                    which=which,
                )
                runner(copy_cmd, capture_output=True, text=True, check=True)

            receipt = _verify_remote_rclone_object(
                remote_cap,
                expected_sha256=manifest["capture_sha256"],
                expected_bytes=manifest["capture_bytes"],
                which=which,
                runner=runner,
            )
            try:
                write_immutable_json(receipt_path, receipt)
            except FileExistsError:
                pass

        shipped.append(
            {
                "capture": cap_path.name,
                "campaign_id": manifest["campaign_id"],
                "sha256": manifest["capture_sha256"],
                "bytes": manifest["capture_bytes"],
                "remote_uri": f"rclone://{remote_cap}",
            }
        )

        mtime = cap_path.stat().st_mtime
        age_secs = max(0.0, now_ts - mtime)
        if age_secs >= min_evict_age_secs and receipt_path.exists():
            if evict_after_verify:
                tombstone = evict_local_capture(
                    cap_path,
                    manifest_path=manifest_path,
                    remote_receipt_path=receipt_path,
                )
                evicted.append(tombstone["evicted_filename"])
            else:
                eviction_candidates.append((mtime, cap_path, manifest_path, receipt_path))

    if (
        not evict_after_verify
        and evict_below_free_gib is not None
        and evict_below_free_gib > 0
        and eviction_candidates
    ):
        min_free_bytes = int(evict_below_free_gib * (1 << 30))
        eviction_candidates.sort(key=lambda item: (item[0], item[1].name))
        for _mtime, cap_path, manifest_path, receipt_path in eviction_candidates:
            if disk_free_bytes_fn(resolved_root) >= min_free_bytes:
                break
            tombstone = evict_local_capture(
                cap_path,
                manifest_path=manifest_path,
                remote_receipt_path=receipt_path,
            )
            evicted.append(tombstone["evicted_filename"])

    return {
        "schema_version": 1,
        "sealed_count": len(sealed),
        "shipped_count": len(shipped),
        "evicted_count": len(evicted),
        "shipped": shipped,
        "evicted": evicted,
    }


def _session_jsonl_is_completed(jsonl_path: Path) -> bool:
    """Only completed sessions with a terminal `stopped` record are archived."""
    if jsonl_path.is_symlink() or not jsonl_path.is_file():
        return False
    try:
        with jsonl_path.open("rb") as handle:
            handle.seek(0, os.SEEK_END)
            size = handle.tell()
            if size == 0:
                return False
            read_from = max(0, size - 65_536)
            handle.seek(read_from)
            tail = handle.read()
    except OSError:
        return False
    if not tail.endswith(b"\n"):
        return False
    lines = [ln for ln in tail.splitlines() if ln.strip()]
    if not lines:
        return False
    try:
        last_obj = json.loads(lines[-1].decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError):
        return False
    return isinstance(last_obj, dict) and last_obj.get("type") == "stopped"


def _write_gzip_0600(src_path: Path, dst_gz: Path) -> tuple[str, int]:
    old_umask = os.umask(0o077)
    tmp_path = dst_gz.with_name(f".{dst_gz.name}.tmp.{os.getpid()}")
    try:
        with src_path.open("rb") as reader, tmp_path.open("xb") as raw_out:
            with gzip.GzipFile(filename="", fileobj=raw_out, mode="wb", mtime=0, compresslevel=6) as gz:
                while chunk := reader.read(256 * 1024):
                    gz.write(chunk)
            raw_out.flush()
            os.fsync(raw_out.fileno())
        os.replace(tmp_path, dst_gz)
    finally:
        if tmp_path.exists():
            tmp_path.unlink()
        os.umask(old_umask)
    hasher = sha256()
    with dst_gz.open("rb") as check_in:
        while chunk := check_in.read(256 * 1024):
            hasher.update(chunk)
    return hasher.hexdigest(), dst_gz.stat().st_size


def atomic_sqlite_backup(db_path: Path, staging_dir: Path, *, snapshot_tag: str) -> dict:
    """Create a WAL-consistent online SQLite backup compressed with 0600 permissions."""
    if db_path.is_symlink() or not db_path.is_file():
        raise ValueError(f"SQLite telemetry DB not found or symlink: {db_path}")
    if not re.fullmatch(r"[A-Za-z0-9._-]{1,80}", snapshot_tag):
        raise ValueError(f"invalid snapshot_tag: {snapshot_tag!r}")

    old_umask = os.umask(0o077)
    staging_dir.mkdir(parents=True, exist_ok=True, mode=0o700)
    raw_backup = staging_dir / f".telemetry-{snapshot_tag}.sqlite3.tmp.{os.getpid()}"
    final_gz = staging_dir / f"telemetry-{snapshot_tag}.sqlite3.gz"
    try:
        src_uri = f"file:{db_path.resolve()}?mode=ro"
        with sqlite3.connect(src_uri, uri=True, timeout=10.0) as src_conn:
            with sqlite3.connect(raw_backup) as dst_conn:
                src_conn.backup(dst_conn, pages=256, sleep=0.005)
                integrity = dst_conn.execute("PRAGMA integrity_check").fetchone()
                if not integrity or integrity[0] != "ok":
                    raise ValueError(f"SQLite backup integrity_check failed: {integrity}")
        gz_sha, gz_bytes = _write_gzip_0600(raw_backup, final_gz)
    finally:
        if raw_backup.exists():
            raw_backup.unlink()
        os.umask(old_umask)

    return {
        "filename": final_gz.name,
        "path": str(final_gz),
        "sha256": gz_sha,
        "bytes": gz_bytes,
    }


def backup_private_telemetry(
    *,
    db_path: Path,
    telemetry_dir: Path,
    staging_dir: Path,
    private_remote: str | None = None,
    snapshot_tag: str | None = None,
    bwlimit: str = "1M",
    which: Callable[[str], str | None] = shutil.which,
    runner: Callable[..., subprocess.CompletedProcess] = subprocess.run,
) -> dict:
    """Snapshot SQLite + completed session JSONLs into an isolated private store."""
    validate_private_destination(str(staging_dir))
    if private_remote is not None:
        validate_private_destination(private_remote)
        if ":" not in private_remote:
            raise ValueError("private_remote must be an rclone remote (preferably rclone crypt:)")

    tag = snapshot_tag or datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    old_umask = os.umask(0o077)
    try:
        staging_dir.mkdir(parents=True, exist_ok=True, mode=0o700)
        os.chmod(staging_dir, 0o700)
    finally:
        os.umask(old_umask)

    db_meta = atomic_sqlite_backup(db_path, staging_dir, snapshot_tag=tag)
    sessions_stage = staging_dir / "sessions"
    old_umask = os.umask(0o077)
    try:
        sessions_stage.mkdir(parents=True, exist_ok=True, mode=0o700)
    finally:
        os.umask(old_umask)

    archived_sessions = []
    if telemetry_dir.is_dir() and not telemetry_dir.is_symlink():
        for entry in sorted(telemetry_dir.glob("*.jsonl")):
            if not _session_jsonl_is_completed(entry):
                continue
            out_gz = sessions_stage / f"{entry.name}.gz"
            if not out_gz.exists():
                gz_sha, gz_bytes = _write_gzip_0600(entry, out_gz)
            else:
                gz_sha, gz_bytes = digest_unchanged(out_gz)
            archived_sessions.append(
                {"filename": out_gz.name, "sha256": gz_sha, "bytes": gz_bytes}
            )

    uploaded = []
    if private_remote is not None:
        remote_base = private_remote.rstrip("/")
        targets = [(Path(db_meta["path"]), f"{remote_base}/sqlite/{db_meta['filename']}")]
        for sess in archived_sessions:
            targets.append(
                (sessions_stage / sess["filename"], f"{remote_base}/sessions/{sess['filename']}")
            )
        for local_file, remote_dst in targets:
            cmd = build_idle_command(
                ["rclone", "copyto", "--bwlimit", bwlimit, "--", str(local_file), remote_dst],
                which=which,
            )
            runner(cmd, capture_output=True, text=True, check=True)
            uploaded.append(remote_dst)

    return {
        "schema_version": 1,
        "snapshot_tag": tag,
        "sqlite_backup": {
            "filename": db_meta["filename"],
            "sha256": db_meta["sha256"],
            "bytes": db_meta["bytes"],
        },
        "completed_sessions": archived_sessions,
        "uploaded_targets": uploaded,
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)

    p_pub = sub.add_parser("ship-public", help="Seal, idle-upload, verify, and optionally evict Bronze captures")
    p_pub.add_argument("--capture-dir", type=Path, required=True)
    p_pub.add_argument("--remote-dir", default="", help="Optional rclone remote dir, e.g. gdrive:AlephTX-Quant-Lakehouse/10_bronze_raw/tokyo_selection_dev")
    p_pub.add_argument("--collector-region", choices=VALID_REGIONS, default="tokyo")
    p_pub.add_argument("--split-role", choices=VALID_ROLES, default="selection_dev")
    p_pub.add_argument("--campaign-id", default=None)
    p_pub.add_argument("--bwlimit", default="3M")
    p_pub.add_argument("--drive-chunk-size", default="64M")
    p_pub.add_argument("--evict-after-verify", action="store_true")
    p_pub.add_argument("--evict-below-free-gib", type=float, default=None, help="Evict oldest Drive-verified slices only when free disk drops below N GiB")
    p_pub.add_argument("--min-evict-age-secs", type=float, default=300.0)

    p_priv = sub.add_parser("backup-private", help="Hot-backup SQLite telemetry and completed session JSONLs")
    p_priv.add_argument("--db-path", type=Path, default=Path("/var/lib/aleph-tx-monitor/telemetry.sqlite3"))
    p_priv.add_argument("--telemetry-dir", type=Path, default=Path("/var/lib/aleph-tx-telemetry"))
    p_priv.add_argument("--staging-dir", type=Path, required=True)
    p_priv.add_argument("--private-remote", default=None, help="Optional encrypted rclone remote, e.g. gdrive-crypt:private_telemetry/tokyo")
    p_priv.add_argument("--snapshot-tag", default=None)
    p_priv.add_argument("--bwlimit", default="1M")

    args = parser.parse_args()
    if args.command == "ship-public":
        res = ship_public_bronze(
            args.capture_dir,
            remote_dir=args.remote_dir,
            collector_region=args.collector_region,
            split_role=args.split_role,
            campaign_id=args.campaign_id,
            bwlimit=args.bwlimit,
            drive_chunk_size=args.drive_chunk_size,
            evict_after_verify=args.evict_after_verify,
            evict_below_free_gib=args.evict_below_free_gib,
            min_evict_age_secs=args.min_evict_age_secs,
        )
    else:
        res = backup_private_telemetry(
            db_path=args.db_path,
            telemetry_dir=args.telemetry_dir,
            staging_dir=args.staging_dir,
            private_remote=args.private_remote,
            snapshot_tag=args.snapshot_tag,
            bwlimit=args.bwlimit,
        )
    print(json.dumps(res, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
