import gzip
import json
import os
from pathlib import Path
import stat
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))

from perp_rolling_runner import run_rolling_loop


class PerpRollingRunnerTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()

    def _make_fake_perp_capture(self, script_path: Path, *, fail_first: bool = False) -> None:
        marker = self.root / "failed_once.flag"
        code = f"""#!{sys.executable}
import gzip, json, pathlib, sys

args = sys.argv[1:]
out_idx = args.index("-output") + 1
out_path = pathlib.Path(args[out_idx])

fail_first = {fail_first!r}
marker = pathlib.Path({str(marker)!r})
if fail_first and not marker.exists():
    marker.write_text("1")
    out_path.write_bytes(b"corrupt-partial")
    sys.exit(2)

t0 = 1_800_000_000_000
origin = t0 * 1_000_000 - 10_000_000
run_id = "11223344556677889900aabbccddeeff"
rows = [
    {{
        "source": "binance",
        "event": "message",
        "market": "BTCUSDT",
        "received_at_ms": t0 + 10,
        "available_at_ms": t0 + 11,
        "capture_timing": {{
            "run_id": run_id,
            "sequence": 1,
            "connection_epoch": 1,
            "receipt_wall_ns": (t0 + 10) * 1_000_000 + 100,
            "receipt_monotonic_ns": (t0 + 10) * 1_000_000 + 100 - origin,
            "available_wall_ns": (t0 + 11) * 1_000_000 + 200,
            "available_monotonic_ns": (t0 + 11) * 1_000_000 + 200 - origin,
        }},
        "payload": {{"e": "bookTicker", "s": "BTCUSDT", "b": "90000.0", "B": "1", "a": "90000.5", "A": "2"}},
    }},
    {{
        "source": "collector",
        "event": "end",
        "received_at_ms": t0 + 50,
        "available_at_ms": t0 + 51,
        "capture_timing": {{
            "run_id": run_id,
            "sequence": 2,
            "connection_epoch": 0,
            "receipt_wall_ns": (t0 + 50) * 1_000_000 + 100,
            "receipt_monotonic_ns": (t0 + 50) * 1_000_000 + 100 - origin,
            "available_wall_ns": (t0 + 51) * 1_000_000 + 200,
            "available_monotonic_ns": (t0 + 51) * 1_000_000 + 200 - origin,
        }},
    }},
]
with gzip.open(out_path, "wt", encoding="utf-8") as fh:
    for r in rows:
        fh.write(json.dumps(r) + "\\n")
"""
        script_path.write_text(code, encoding="utf-8")
        os.chmod(script_path, stat.S_IRWXU)

    def test_rolling_loop_sweeps_orphans_quarantines_failures_and_seals_monotonic_windows(self) -> None:
        out_dir = self.root / "crypto"
        quar_dir = self.root / "quarantine"
        out_dir.mkdir()
        quar_dir.mkdir()
        # Simulate a prior SIGKILL during quarantine hashing: .partial in quarantine_dir with no manifest yet!
        unmanifested = quar_dir / "tokyo-crypto-20261114T120000Z-w00.ndjson.gz.partial"
        unmanifested.write_bytes(b"sigkill-interrupted-quarantine-bytes")
        # Leftover orphan from prior crash at w01 -> swept to quarantine, next window starts at w02!
        orphan = out_dir / "tokyo-crypto-20261115T000000Z-w01.ndjson.gz.partial"
        orphan.write_bytes(b"leftover-crash-bytes")

        fake_bin = self.root / "fake_perp_capture.py"
        self._make_fake_perp_capture(fake_bin, fail_first=True)
        flywheel_py = Path(__file__).resolve().parent / "flywheel_sync.py"

        summary = run_rolling_loop(
            stream_name="crypto",
            output_dir=out_dir,
            quarantine_dir=quar_dir,
            perp_capture_bin=fake_bin,
            flywheel_script=flywheel_py,
            capture_args=["-binance", "BTCUSDT"],
            window_duration="5s",
            collector_region="tokyo",
            split_role="selection_dev",
            remote_dir="",
            max_windows=2,
            failure_backoff_secs=0.05,
        )
        # 1 unmanifested quarantine backfilled (w00), 1 orphan swept (w01), 1 failed attempt quarantined (w02), then w03 completed cleanly!
        self.assertEqual(
            summary["quarantine_manifests_backfilled"],
            ["tokyo-crypto-20261114T120000Z-w00.quarantine_manifest.json"],
        )
        self.assertTrue(
            (quar_dir / "tokyo-crypto-20261114T120000Z-w00.quarantine_manifest.json").exists()
        )
        self.assertEqual(len(summary["orphaned_swept"]), 1)
        self.assertEqual(len(summary["quarantined_windows"]), 1)
        self.assertTrue(summary["quarantined_windows"][0].endswith("-w02.ndjson.gz.partial"))
        self.assertEqual(len(summary["completed_windows"]), 1)
        w3_name = summary["completed_windows"][0]
        self.assertTrue(w3_name.endswith("-w03.ndjson.gz"))
        self.assertTrue((out_dir / w3_name).exists())
        self.assertTrue((out_dir / f"{w3_name}.bronze_manifest.json").exists())

    def test_oasis_installed_script_subset_is_self_contained(self):
        import re
        import shutil
        import subprocess
        import sys

        repo_root = Path(__file__).resolve().parents[1]
        install_sh = (repo_root / "deploy" / "oasis" / "install.sh").read_text(encoding="utf-8")
        match = re.search(r"scripts/\{([^}]+)\}\.py", install_sh)
        self.assertIsNotNone(match)
        installed = [name.strip() for name in match.group(1).split(",")]
        with tempfile.TemporaryDirectory() as tmpdir:
            isolated = Path(tmpdir) / "isolated_scripts"
            isolated.mkdir()
            for name in installed:
                shutil.copy2(repo_root / "scripts" / f"{name}.py", isolated / f"{name}.py")
            proc = subprocess.run(
                [sys.executable, str(isolated / "perp_rolling_runner.py"), "--help"],
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(proc.returncode, 0, msg=proc.stderr)

    def test_oasis_capture_units_guard_against_needrestart_and_short_stop_timeouts(self) -> None:
        repo_root = Path(__file__).resolve().parents[1]
        nr_conf = (repo_root / "deploy" / "oasis" / "50-aleph-tx-needrestart.conf").read_text(encoding="utf-8")
        self.assertIn("perp-capture-", nr_conf)
        self.assertIn("$nrconf{override_rc}", nr_conf)
        for stream in ("bulk", "crypto", "rwa"):
            unit_txt = (
                repo_root / "deploy" / "oasis" / f"aleph-perp-capture-{stream}.service"
            ).read_text(encoding="utf-8")
            self.assertIn("TimeoutStopSec=120", unit_txt)

    def test_backfill_quarantine_is_stream_scoped_and_ignores_concurrent_manifest(self) -> None:
        from perp_rolling_runner import WINDOW_RE, backfill_missing_quarantine_manifests

        self.assertEqual(
            WINDOW_RE.search("tokyo-rwa-20261116T000000Z-w07.01.jsonl.zst").group(1),
            "07",
        )
        quar_dir = self.root / "shared_quarantine"
        quar_dir.mkdir()
        crypto_partial = quar_dir / "tokyo-crypto-20261116T000000Z-w01.ndjson.gz.partial"
        rwa_partial = quar_dir / "tokyo-rwa-20261116T000000Z-w01.ndjson.gz.partial"
        crypto_partial.write_bytes(b"crypto-bytes")
        rwa_partial.write_bytes(b"rwa-bytes")

        created = backfill_missing_quarantine_manifests(
            quar_dir, collector_region="tokyo", stream_name="crypto"
        )
        self.assertEqual(created, ["tokyo-crypto-20261116T000000Z-w01.quarantine_manifest.json"])
        self.assertFalse((quar_dir / "tokyo-rwa-20261116T000000Z-w01.quarantine_manifest.json").exists())

        # Re-running (or racing with another process after exists() check) stays idempotent.
        self.assertEqual(
            backfill_missing_quarantine_manifests(
                quar_dir, collector_region="tokyo", stream_name="crypto"
            ),
            [],
        )


if __name__ == "__main__":
    unittest.main()
