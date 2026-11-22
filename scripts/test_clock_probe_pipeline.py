#!/usr/bin/env python3
"""Cross-language verification: Lighter clock_probe across Python Bronze/Archive/Silver -> Rust perp-compile."""

from __future__ import annotations

from copy import deepcopy
import gzip
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))

from archive_public_capture import ingest, records as archive_records
from quant_lakehouse import generate_duckdb_sql, normalize_capture, seal_bronze_capture

REPO_ROOT = Path(__file__).resolve().parent.parent
RUN_ID = "0123456789abcdef0123456789abcdef"
T0_MS = 1_800_000_000_000


def _read_gz_ndjson(path: Path) -> list[dict]:
    with gzip.open(path, "rt", encoding="utf-8") as handle:
        return [json.loads(line) for line in handle if line.strip()]


class ClockProbeCrossLanguageTests(unittest.TestCase):
    _cross_venue_bin: Path

    @classmethod
    def _resolve_cross_venue_binary(cls) -> Path:
        override = os.environ.get("ALEPH_CROSS_VENUE_BIN")
        if override:
            candidate = Path(override)
            if not (candidate.is_file() and os.access(candidate, os.X_OK)):
                raise RuntimeError(f"ALEPH_CROSS_VENUE_BIN is not executable: {candidate}")
            return candidate

        target_dir = Path(os.environ.get("CARGO_TARGET_DIR", REPO_ROOT / "target"))
        watched_sources = [
            REPO_ROOT / "src" / "bin" / "cross_venue.rs",
            REPO_ROOT / "src" / "capture.rs",
            REPO_ROOT / "src" / "capture" / "clock.rs",
            REPO_ROOT / "src" / "capture" / "feeds.rs",
            REPO_ROOT / "src" / "capture" / "timing.rs",
            REPO_ROOT / "src" / "lib.rs",
        ]
        newest_src_mtime = max(p.stat().st_mtime for p in watched_sources if p.exists())
        candidates = [target_dir / "debug" / "cross_venue", target_dir / "release" / "cross_venue"]
        fresh = [
            p
            for p in candidates
            if p.is_file() and os.access(p, os.X_OK) and p.stat().st_mtime >= newest_src_mtime
        ]
        if fresh:
            return max(fresh, key=lambda p: p.stat().st_mtime)

        if os.environ.get("ALEPH_SKIP_CARGO_BUILD") == "1" or shutil.which("cargo") is None:
            raise unittest.SkipTest(
                "cross_venue binary not built/current and cargo build skipped or unavailable"
            )

        subprocess.run(
            ["cargo", "build", "--locked", "--bin", "cross_venue"],
            cwd=REPO_ROOT,
            check=True,
        )
        built = target_dir / "debug" / "cross_venue"
        if not (built.is_file() and os.access(built, os.X_OK)):
            raise RuntimeError(f"cargo build did not produce {built}")
        return built

    @classmethod
    def setUpClass(cls) -> None:
        cls._cross_venue_bin = cls._resolve_cross_venue_binary()

    def _row(
        self,
        seq: int,
        source: str,
        event: str,
        dt_ms: int,
        *,
        epoch: int = 1,
        request_sent_at_ms: int | None = None,
        payload: dict | None = None,
    ) -> dict:
        at = T0_MS + dt_ms
        item: dict = {
            "capture_timing": {
                "run_id": RUN_ID,
                "sequence": seq,
                "connection_epoch": 0 if source == "collector" else epoch,
                "receipt_wall_ns": at * 1_000_000 + 100_000,
                "receipt_monotonic_ns": dt_ms * 1_000_000 + 10_100_000,
                "available_wall_ns": at * 1_000_000 + 200_000,
                "available_monotonic_ns": dt_ms * 1_000_000 + 10_200_000,
            },
            "source": source,
            "event": event,
            "received_at_ms": at,
            "available_at_ms": at,
        }
        if request_sent_at_ms is not None:
            item["request_sent_at_ms"] = request_sent_at_ms
        if payload is not None:
            item["payload"] = payload
        return item

    def _book(self, dt_ms: int, nonce: int, begin_nonce: int) -> dict:
        at = T0_MS + dt_ms
        return {
            "type": "subscribed/order_book" if begin_nonce == 0 else "update/order_book",
            "channel": "order_book:0",
            "timestamp": at + 20,
            "order_book": {
                "nonce": nonce,
                "begin_nonce": begin_nonce,
                "last_updated_at": (at + 10) * 1_000,
                "bids": [{"price": "1999.90", "size": "4"}],
                "asks": [{"price": "2000.10", "size": "5"}],
            },
        }

    def _probe(self, published_ms: int) -> dict:
        return {
            "type": "subscribed/market_stats",
            "channel": "market_stats:0",
            "timestamp": published_ms,
            "market_stats": {"market_id": 0, "symbol": "ETH", "index_price": "2000.0"},
        }

    def _hl_book(self, dt_ms: int) -> dict:
        return {
            "channel": "l2Book",
            "data": {
                "coin": "ETH",
                "time": T0_MS + dt_ms,
                "levels": [
                    [{"px": "1999.8", "sz": "3", "n": 1}],
                    [{"px": "2000.2", "sz": "3", "n": 1}],
                ],
            },
        }

    def _compile_config(self, path: Path) -> Path:
        cfg = deepcopy(json.loads((REPO_ROOT / "examples" / "eth_capture.json").read_text(encoding="utf-8")))
        cfg["sample_ms"] = 100
        cfg["legs"][0]["source_clock_ahead_bound_ms"] = 0
        cfg["legs"][0]["lighter_clock"] = {
            "max_rtt_ms": 50,
            "valid_for_ms": 250,
            "drift_margin_ms": 2,
        }
        path.write_text(json.dumps(cfg, indent=2) + "\n", encoding="utf-8")
        return path

    def test_clock_probe_pipeline_go_wire_to_python_lakehouse_to_rust_compiler(self) -> None:
        # Match actual Go perp-capture wire semantics: Lighter does not emit a
        # synthetic "connected" row (only Hyperliquid/Binance/Arcus trades do).
        rows = [
            self._row(1, "lighter", "message", 0, epoch=1, request_sent_at_ms=T0_MS - 20, payload=self._book(0, 10, 0)),
            self._row(2, "hyperliquid", "connected", 10, epoch=1),
            self._row(3, "hyperliquid", "message", 20, epoch=1, payload=self._hl_book(20)),
            self._row(4, "lighter", "message", 100, epoch=1, payload=self._book(100, 11, 10)),
            self._row(5, "lighter", "clock_probe", 200, epoch=1, request_sent_at_ms=T0_MS + 180, payload=self._probe(T0_MS + 220)),
            self._row(6, "lighter_robinhood", "clock_probe", 220, epoch=1, request_sent_at_ms=T0_MS + 210, payload=self._probe(T0_MS + 230)),
            self._row(7, "lighter", "clock_probe", 250, epoch=2, request_sent_at_ms=T0_MS + 240, payload=self._probe(T0_MS + 270)),
            self._row(8, "lighter", "message", 300, epoch=1, payload=self._book(300, 12, 11)),
            self._row(9, "lighter", "message", 400, epoch=1, payload=self._book(400, 13, 12)),
            self._row(10, "collector", "end", 500, epoch=0),
        ]
        raw_bytes = "".join(json.dumps(r, separators=(",", ":")) + "\n" for r in rows).encode("utf-8")

        with tempfile.TemporaryDirectory(prefix="aleph-clock-probe-") as tmp:
            root = Path(tmp)
            source_gz = root / "capture.ndjson.gz"
            with gzip.open(source_gz, "wb") as handle:
                handle.write(raw_bytes)

            meta = {"campaign_id": "cross-lang-probe", "collector_region": "tokyo"}
            bronze = seal_bronze_capture(source_gz, split_role="selection_dev", **meta)
            self.assertEqual(bronze["records"], 10)
            self.assertEqual(bronze["disconnect_counts"], {})

            archive_dir = root / "archive"
            archived = ingest(source_gz, archive_dir)
            self.assertEqual(archived["records"], 10)
            reconstructed = b"".join(archive_records(archive_dir))
            self.assertEqual(reconstructed, raw_bytes)

            silver_dir = root / "silver"
            silver = normalize_capture(archive_dir, silver_dir, precise_times=True, **meta)
            self.assertEqual(silver["tables"]["bbo_quotes"]["rows"], 5)
            lifecycle = _read_gz_ndjson(silver_dir / "feed_lifecycle.ndjson.gz")
            probes = [item for item in lifecycle if item["event"] == "clock_probe"]
            self.assertEqual(
                [(item["venue"], item["connection_epoch"], item["request_sent_at_ms"]) for item in probes],
                [
                    ("lighter", 1, T0_MS + 180),
                    ("lighter_robinhood", 1, T0_MS + 210),
                    ("lighter", 2, T0_MS + 240),
                ],
            )
            sql = generate_duckdb_sql(silver_dir, root / "parquet")
            self.assertIn("WHERE event <> 'clock_probe'", sql)

            cfg_path = self._compile_config(root / "compile_cfg.json")
            rec_path = root / "reconstructed.ndjson"
            rec_path.write_bytes(reconstructed)
            compiled_path = root / "compiled.ndjson"
            proc = subprocess.run(
                [
                    str(self._cross_venue_bin),
                    "perp-compile",
                    "--precise-times",
                    str(cfg_path),
                    str(rec_path),
                    str(compiled_path),
                ],
                cwd=REPO_ROOT,
                check=True,
                capture_output=True,
                text=True,
            )
            cov = json.loads(proc.stdout)
            self.assertEqual(cov["availability_rule"], "ceil_available_wall_ns_to_ms")
            self.assertEqual(cov["capture_run_id"], RUN_ID)
            self.assertEqual(cov["precise_records"], 10)
            self.assertEqual(cov["lighter_clock_refreshes"], 3)
            self.assertEqual(cov["clock_unavailable"], [0, 0])
            self.assertEqual(cov["lighter_nonce_gaps"], [0, 0])
            self.assertEqual(cov["disconnects"], {})
            audit = cov["lighter_clock"][0]
            self.assertFalse(audit["conflicting"])
            self.assertEqual(audit["probes"], 3)

            frames = [json.loads(line) for line in compiled_path.read_text(encoding="utf-8").splitlines()[1:]]
            self.assertEqual(len(frames), 5)
            for frame in frames:
                self.assertGreater(frame["legs"][0]["book"]["timestamp_ms"], 0)
                self.assertTrue(frame["legs"][0]["book"]["tradable"])

    def test_conflicting_clock_probe_pipeline_fails_closed_in_rust_compiler(self) -> None:
        rows = [
            self._row(1, "lighter", "message", 0, epoch=1, request_sent_at_ms=T0_MS - 20, payload=self._book(0, 10, 0)),
            self._row(2, "lighter", "clock_probe", 100, epoch=1, request_sent_at_ms=T0_MS + 80, payload=self._probe(T0_MS + 160)),
            self._row(3, "lighter", "message", 200, epoch=1, payload=self._book(200, 11, 10)),
            self._row(4, "collector", "end", 300, epoch=0),
        ]
        with tempfile.TemporaryDirectory(prefix="aleph-clock-conflict-") as tmp:
            root = Path(tmp)
            source_gz = root / "conflict.ndjson.gz"
            with gzip.open(source_gz, "wt", encoding="utf-8") as handle:
                for row in rows:
                    handle.write(json.dumps(row, separators=(",", ":")) + "\n")
            archive_dir = root / "archive"
            ingest(source_gz, archive_dir)
            rec_path = root / "reconstructed.ndjson"
            rec_path.write_bytes(b"".join(archive_records(archive_dir)))
            cfg_path = self._compile_config(root / "compile_cfg.json")
            compiled_path = root / "compiled.ndjson"
            proc = subprocess.run(
                [
                    str(self._cross_venue_bin),
                    "perp-compile",
                    "--precise-times",
                    str(cfg_path),
                    str(rec_path),
                    str(compiled_path),
                ],
                cwd=REPO_ROOT,
                check=True,
                capture_output=True,
                text=True,
            )
            cov = json.loads(proc.stdout)
            self.assertEqual(cov["lighter_clock_refreshes"], 1)
            self.assertEqual(cov["clock_unavailable"], [1, 0])
            self.assertTrue(cov["lighter_clock"][0]["conflicting"])
            frames = [json.loads(line) for line in compiled_path.read_text(encoding="utf-8").splitlines()[1:]]
            self.assertGreater(frames[0]["legs"][0]["book"]["timestamp_ms"], 0)
            self.assertEqual(frames[1]["legs"][0]["book"]["timestamp_ms"], 0)
            self.assertFalse(frames[1]["legs"][0]["book"]["tradable"])


if __name__ == "__main__":
    unittest.main()
