import gzip
from hashlib import sha256
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "deploy" / "oasis"))

import flywheel_sync
import quant_lakehouse
from flywheel_sync import (
    backup_private_telemetry,
    build_idle_command,
    ship_public_bronze,
    validate_private_destination,
)
from telemetry_store import TelemetryStore


class FlywheelSyncTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.t0 = 1_800_000_000_000

    def _write_closed_segment(self, directory: Path, name: str) -> Path:
        out = directory / name
        run_id = "f1e2d3c4b5a697887766554433221100"
        origin_ns = self.t0 * 1_000_000 - 10_000_000

        def row(seq: int, source: str, event: str, off_ms: int, payload=None, market=None) -> dict:
            recv = self.t0 + off_ms
            avail = recv + 1
            rw = recv * 1_000_000 + 100_000
            aw = avail * 1_000_000 + 200_000
            rec = {
                "source": source,
                "event": event,
                "received_at_ms": recv,
                "available_at_ms": avail,
                "capture_timing": {
                    "run_id": run_id,
                    "sequence": seq,
                    "connection_epoch": 0 if source == "collector" else 1,
                    "receipt_wall_ns": rw,
                    "receipt_monotonic_ns": rw - origin_ns,
                    "available_wall_ns": aw,
                    "available_monotonic_ns": aw - origin_ns,
                },
            }
            if market is not None:
                rec["market"] = market
            if payload is not None:
                rec["payload"] = payload
            return rec

        rows = [
            row(1, "collector", "segment_start", 0, payload={"segment_index": 0}),
            row(
                2,
                "binance",
                "message",
                10,
                market="ETHUSDT",
                payload={"e": "bookTicker", "s": "ETHUSDT", "b": "2500.0", "B": "5", "a": "2500.1", "A": "6"},
            ),
            row(3, "collector", "segment_end", 50, payload={"segment_index": 0, "continued": True}),
        ]
        with gzip.open(out, "wt", encoding="utf-8", compresslevel=3) as fh:
            for item in rows:
                fh.write(json.dumps(item, separators=(",", ":")) + "\n")
        return out

    def test_build_idle_command_prefixes_linux_idle_schedulers(self) -> None:
        all_present = lambda tool: f"/usr/bin/{tool}"
        cmd = build_idle_command(["rclone", "copyto", "a", "b"], which=all_present)
        self.assertEqual(
            cmd,
            ["chrt", "-i", "0", "nice", "-n", "19", "ionice", "-c", "3", "rclone", "copyto", "a", "b"],
        )

        only_nice = lambda tool: "/usr/bin/nice" if tool == "nice" else None
        self.assertEqual(
            build_idle_command(["rclone", "version"], which=only_nice),
            ["nice", "-n", "19", "rclone", "version"],
        )

    def test_ship_public_bronze_skips_partial_and_private_verifies_sha256_and_evicts(self) -> None:
        cap_dir = self.root / "captures"
        cap_dir.mkdir()
        seg0 = self._write_closed_segment(cap_dir, "tokyo-flywheel-20261116.seg0000.ndjson.gz")
        active_partial = cap_dir / "tokyo-flywheel-20261116.seg0001.ndjson.gz.partial"
        active_partial.write_bytes(b"active-unsealed-slice")
        private_leak = cap_dir / "private-account-fills.ndjson.gz"
        private_leak.write_bytes(b"never-touch")

        remote_store: dict[str, bytes] = {}
        seen_cmds: list[list[str]] = []

        def fake_runner(cmd: list[str], **_kwargs) -> subprocess.CompletedProcess:
            seen_cmds.append(cmd)
            rclone_idx = cmd.index("rclone")
            sub = cmd[rclone_idx + 1]
            if sub == "copyto":
                sep = cmd.index("--")
                src = Path(cmd[sep + 1])
                dst = cmd[sep + 2]
                remote_store[dst] = src.read_bytes()
                return subprocess.CompletedProcess(cmd, 0, stdout="", stderr="")
            if sub == "lsjson":
                target = cmd[-1]
                blob = remote_store[target]
                out = json.dumps([{"Name": Path(target).name, "Size": len(blob)}])
                return subprocess.CompletedProcess(cmd, 0, stdout=out, stderr="")
            if sub == "hashsum":
                target = cmd[-1]
                digest = sha256(remote_store[target]).hexdigest()
                out = f"{digest}  {Path(target).name}\n"
                return subprocess.CompletedProcess(cmd, 0, stdout=out, stderr="")
            raise AssertionError(f"unexpected command: {cmd}")

        all_idle = lambda tool: f"/usr/bin/{tool}"
        mtime = seg0.stat().st_mtime

        # First pass: age < min_evict_age_secs -> seals, uploads, verifies, but retains local file.
        res1 = ship_public_bronze(
            cap_dir,
            remote_dir="gdrive:AlephTX-Quant-Lakehouse/10_bronze_raw/tokyo_selection_dev",
            collector_region="tokyo",
            split_role="selection_dev",
            bwlimit="3M",
            drive_chunk_size="64M",
            evict_after_verify=True,
            min_evict_age_secs=300.0,
            now_epoch=mtime + 60.0,
            which=all_idle,
            runner=fake_runner,
        )
        self.assertEqual(res1["shipped_count"], 1)
        self.assertEqual(res1["evicted_count"], 0)
        self.assertEqual(res1["shipped"][0]["campaign_id"], "tokyo-flywheel-20261116")
        self.assertTrue(seg0.exists())
        self.assertTrue(active_partial.exists())
        self.assertTrue(private_leak.exists())
        self.assertEqual(len(remote_store), 2)  # segment + .bronze_manifest.json only
        for cmd in seen_cmds:
            self.assertEqual(cmd[:9], ["chrt", "-i", "0", "nice", "-n", "19", "ionice", "-c", "3"])

        # Second pass after min_evict_age_secs -> reuses receipt without re-uploading and evicts cleanly.
        cmds_before = len(seen_cmds)
        res2 = ship_public_bronze(
            cap_dir,
            remote_dir="gdrive:AlephTX-Quant-Lakehouse/10_bronze_raw/tokyo_selection_dev",
            collector_region="tokyo",
            split_role="selection_dev",
            evict_after_verify=True,
            min_evict_age_secs=300.0,
            now_epoch=mtime + 600.0,
            which=all_idle,
            runner=fake_runner,
        )
        self.assertEqual(len(seen_cmds), cmds_before)
        self.assertEqual(res2["evicted_count"], 1)
        self.assertFalse(seg0.exists())
        self.assertTrue((cap_dir / f"{seg0.name}.evicted.json").exists())

        # Corrupted remote SHA-256 on a new segment must fail closed without evicting.
        seg2 = self._write_closed_segment(cap_dir, "tokyo-flywheel-bad.seg0000.ndjson.gz")

        def corrupt_hash_runner(cmd: list[str], **kwargs) -> subprocess.CompletedProcess:
            rclone_idx = cmd.index("rclone")
            if cmd[rclone_idx + 1] == "hashsum":
                return subprocess.CompletedProcess(cmd, 0, stdout=f"{'0' * 64}  bad\n", stderr="")
            return fake_runner(cmd, **kwargs)

        with self.assertRaisesRegex(ValueError, "SHA-256 mismatch"):
            ship_public_bronze(
                cap_dir,
                remote_dir="gdrive:AlephTX-Quant-Lakehouse/10_bronze_raw/tokyo_selection_dev",
                collector_region="tokyo",
                split_role="selection_dev",
                evict_after_verify=True,
                min_evict_age_secs=0.0,
                which=all_idle,
                runner=corrupt_hash_runner,
            )
        self.assertTrue(seg2.exists())
        self.assertFalse((cap_dir / f"{seg2.name}.upload_receipt.json").exists())

    def test_backup_private_telemetry_isolates_paths_online_wal_and_completed_sessions(self) -> None:
        for forbidden in (
            "gdrive:AlephTX-Quant-Lakehouse/10_bronze_raw/tokyo",
            "/mnt/e/AlephTX-Lakehouse/20_silver_normalized",
            "crypt:public/telemetry",
            "/var/lib/public_bronze/out",
        ):
            with self.assertRaisesRegex(ValueError, "public Lakehouse"):
                validate_private_destination(forbidden)

        telem_dir = self.root / "telemetry_raw"
        telem_dir.mkdir()
        db_path = self.root / "telemetry.sqlite3"

        # Session 1 is completed (`stopped`); Session 2 is still live (`snapshot`).
        s1 = telem_dir / "sess-done.jsonl"
        s1.write_text(
            "\n".join(
                [
                    json.dumps({"type": "session", "ts_ms": 1000, "exchange": "lighter", "symbol": "ETH"}),
                    json.dumps({"type": "stopped", "ts_ms": 2000}),
                ]
            )
            + "\n",
            encoding="utf-8",
        )
        s2 = telem_dir / "sess-active.jsonl"
        s2.write_text(
            "\n".join(
                [
                    json.dumps({"type": "session", "ts_ms": 3000, "exchange": "lighter", "symbol": "BTC"}),
                    json.dumps({"type": "snapshot", "ts_ms": 3500, "equity": 500.0, "dropped": 0}),
                ]
            )
            + "\n",
            encoding="utf-8",
        )

        store = TelemetryStore(db_path, telem_dir)
        self.addCleanup(store.db.close)
        self.assertEqual(store.ingest(), 4)

        uploaded: list[tuple[str, str]] = []

        def fake_runner(cmd: list[str], **_kwargs) -> subprocess.CompletedProcess:
            sep = cmd.index("--")
            uploaded.append((cmd[sep + 1], cmd[sep + 2]))
            return subprocess.CompletedProcess(cmd, 0, stdout="", stderr="")

        staging = self.root / "private_telemetry"
        summary = backup_private_telemetry(
            db_path=db_path,
            telemetry_dir=telem_dir,
            staging_dir=staging,
            private_remote="gdrive-crypt:aleph-private-vault",
            snapshot_tag="20261116T010000Z",
            which=lambda tool: f"/usr/bin/{tool}",
            runner=fake_runner,
        )
        self.assertEqual([c["filename"] for c in summary["completed_sessions"]], ["sess-done.jsonl.gz"])
        self.assertEqual(len(uploaded), 2)  # sqlite3.gz + sess-done.jsonl.gz only
        self.assertEqual(os.stat(staging).st_mode & 0o777, 0o700)

        gz_db = staging / summary["sqlite_backup"]["filename"]
        self.assertEqual(os.stat(gz_db).st_mode & 0o777, 0o600)
        restored = self.root / "restored.sqlite3"
        with gzip.open(gz_db, "rb") as src, restored.open("wb") as dst:
            dst.write(src.read())
        with sqlite3.connect(restored) as check_conn:
            self.assertEqual(check_conn.execute("PRAGMA integrity_check").fetchone()[0], "ok")
            count = check_conn.execute("SELECT COUNT(*) FROM sessions").fetchone()[0]
            self.assertEqual(count, 2)

    def test_evict_below_free_gib_evicts_oldest_verified_until_above_threshold(self) -> None:
        cap_dir = self.root / "rolling_guard"
        crypto_sub = cap_dir / "crypto"
        crypto_sub.mkdir(parents=True)
        w1 = self._write_closed_segment(crypto_sub, "tokyo-crypto-20261116T000000Z-w01.ndjson.gz")
        w2 = self._write_closed_segment(crypto_sub, "tokyo-crypto-20261116T101500Z-w02.ndjson.gz")
        os.utime(w1, (1_700_000_100, 1_700_000_100))
        os.utime(w2, (1_700_037_000, 1_700_037_000))

        remote_store: dict[str, bytes] = {}

        def fake_runner(cmd: list[str], **_kwargs) -> subprocess.CompletedProcess:
            rclone_idx = cmd.index("rclone")
            sub = cmd[rclone_idx + 1]
            if sub == "copyto":
                sep = cmd.index("--")
                remote_store[cmd[sep + 2]] = Path(cmd[sep + 1]).read_bytes()
                return subprocess.CompletedProcess(cmd, 0, stdout="", stderr="")
            if sub == "lsjson":
                target = cmd[-1]
                blob = remote_store[target]
                return subprocess.CompletedProcess(
                    cmd, 0, stdout=json.dumps([{"Name": Path(target).name, "Size": len(blob)}]), stderr=""
                )
            if sub == "hashsum":
                target = cmd[-1]
                return subprocess.CompletedProcess(
                    cmd, 0, stdout=f"{sha256(remote_store[target]).hexdigest()}  f\n", stderr=""
                )
            raise AssertionError(cmd)

        # Simulate 14 GiB free initially; once w01 is evicted, free space rises to 17 GiB (>= 16 GiB) so w02 stays!
        def fake_free(_p: Path) -> int:
            return (14 << 30) if w1.exists() else (17 << 30)

        res = ship_public_bronze(
            cap_dir,
            remote_dir="gdrive:AlephTX-Quant-Lakehouse/10_bronze_raw/tokyo_selection_dev",
            collector_region="tokyo",
            split_role="selection_dev",
            evict_below_free_gib=16.0,
            min_evict_age_secs=0.0,
            now_epoch=1_700_100_000.0,
            disk_free_bytes_fn=fake_free,
            which=lambda tool: f"/usr/bin/{tool}",
            runner=fake_runner,
        )
        self.assertEqual(res["shipped_count"], 2)
        self.assertEqual(res["evicted"], ["tokyo-crypto-20261116T000000Z-w01.ndjson.gz"])
        self.assertFalse(w1.exists())
        self.assertTrue(w2.exists())

    def test_evict_below_free_gib_honors_gdrive_receipt_when_remote_dir_empty(self) -> None:
        cap_dir = self.root / "gdrive_receipt_guard"
        rwa_sub = cap_dir / "rwa"
        rwa_sub.mkdir(parents=True)
        w1 = self._write_closed_segment(rwa_sub, "tokyo-rwa-20261115T134910Z-w01.ndjson.gz")
        w2 = self._write_closed_segment(rwa_sub, "tokyo-rwa-20261116T000410Z-w02.ndjson.gz")
        os.utime(w1, (1_700_000_100, 1_700_000_100))
        os.utime(w2, (1_700_037_000, 1_700_037_000))

        # Initial pass seals both manifests while disk has 19 GiB free (>= 16 GiB).
        res_seal = ship_public_bronze(
            cap_dir,
            remote_dir="",
            collector_region="tokyo",
            split_role="selection_dev",
            evict_below_free_gib=16.0,
            min_evict_age_secs=0.0,
            now_epoch=1_700_100_000.0,
            disk_free_bytes_fn=lambda _p: 19 << 30,
        )
        self.assertEqual(res_seal["sealed_count"], 2)
        self.assertEqual(res_seal["evicted"], [])

        # Write a verified Google Drive upload receipt ONLY for w01 (w02 not backed up yet).
        w1_manifest = json.loads(
            w1.with_name(f"{w1.name}.bronze_manifest.json").read_text(encoding="utf-8")
        )
        w1_receipt = w1.with_name(f"{w1.name}.upload_receipt.json")
        w1_receipt.write_text(
            json.dumps(
                {
                    "schema_version": 1,
                    "remote_uri": "gdrive://test-fixture",
                    "sha256": w1_manifest["capture_sha256"],
                    "bytes": w1_manifest["capture_bytes"],
                    "verified_at_utc": "2026-11-16T16:30:00Z",
                }
            ),
            encoding="utf-8",
        )

        # Steady-state idle pass (19 GiB >= 16 GiB) must NOT re-hash multi-GB blobs or evict.
        with mock.patch.object(
            flywheel_sync, "digest_unchanged", wraps=flywheel_sync.digest_unchanged
        ) as spy_digest:
            res_idle = ship_public_bronze(
                cap_dir,
                remote_dir="",
                collector_region="tokyo",
                split_role="selection_dev",
                evict_below_free_gib=16.0,
                min_evict_age_secs=0.0,
                now_epoch=1_700_100_000.0,
                disk_free_bytes_fn=lambda _p: 19 << 30,
            )
            self.assertEqual(spy_digest.call_count, 0)
            self.assertEqual(res_idle["evicted"], [])
            self.assertTrue(w1.exists())

        # Size mutation on disk is still caught immediately even on an idle pass.
        orig_w2_bytes = w2.read_bytes()
        w2.write_bytes(orig_w2_bytes + b"x")
        with self.assertRaisesRegex(ValueError, "sealed capture mutated on disk"):
            ship_public_bronze(
                cap_dir,
                remote_dir="",
                collector_region="tokyo",
                split_role="selection_dev",
                evict_below_free_gib=16.0,
                min_evict_age_secs=0.0,
                now_epoch=1_700_100_000.0,
                disk_free_bytes_fn=lambda _p: 19 << 30,
            )
        w2.write_bytes(orig_w2_bytes)
        os.utime(w2, (1_700_037_000, 1_700_037_000))

        # When free disk stays < 16 GiB (e.g. 12 GiB -> 15 GiB), w01 (Drive-verified) is evicted
        # with full SHA-256 verification, while unbacked w02 is strictly preserved!
        with mock.patch.object(
            quant_lakehouse, "digest_unchanged", wraps=quant_lakehouse.digest_unchanged
        ) as spy_evict_digest, mock.patch.object(
            flywheel_sync, "digest_unchanged", wraps=flywheel_sync.digest_unchanged
        ) as spy_preflight_digest:
            res_low_disk = ship_public_bronze(
                cap_dir,
                remote_dir="",
                collector_region="tokyo",
                split_role="selection_dev",
                evict_below_free_gib=16.0,
                min_evict_age_secs=0.0,
                now_epoch=1_700_100_000.0,
                disk_free_bytes_fn=lambda _p: (12 << 30) if w1.exists() else (15 << 30),
            )
            spy_preflight_digest.assert_not_called()
            spy_evict_digest.assert_called_once_with(w1)
        self.assertEqual(res_low_disk["evicted"], [w1.name])
        self.assertFalse(w1.exists())
        self.assertTrue(w1.with_name(f"{w1.name}.evicted.json").exists())
        self.assertTrue(w2.exists())


if __name__ == "__main__":
    unittest.main()
