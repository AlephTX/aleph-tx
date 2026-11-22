#!/usr/bin/env python3
"""Cross-process Go -> Rust SHM ABI and seqlock verification tests (`OPT-03`)."""

from __future__ import annotations

import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

REPO_ROOT = Path(__file__).resolve().parent.parent

FIXTURES: dict[tuple[int, int], dict[str, object]] = {
    (1, 1001): {
        "timestamp_ns": 1_763_800_000_001_001_000,
        "bid_price": 84123.5,
        "bid_size": 2.25,
        "ask_price": 84124.0,
        "ask_size": 1.75,
        "bids": [
            (84123.5, 2.25),
            (84123.0, 4.5),
            (84122.5, 6.75),
            (84122.0, 8.0),
            (84121.5, 10.5),
        ],
        "asks": [
            (84124.0, 1.75),
            (84124.5, 3.5),
            (84125.0, 5.25),
            (84125.5, 7.0),
            (84126.0, 9.25),
        ],
    },
    (10, 1003): {
        "timestamp_ns": 1_763_800_000_010_003_000,
        "bid_price": 145.10,
        "bid_size": 12.5,
        "ask_price": 145.15,
        "ask_size": 9.0,
        "bids": [
            (145.10, 12.5),
            (145.05, 25.0),
            (145.00, 37.5),
            (144.95, 50.0),
            (144.90, 62.5),
        ],
        "asks": [
            (145.15, 9.0),
            (145.20, 18.0),
            (145.25, 27.0),
            (145.30, 36.0),
            (145.35, 45.0),
        ],
    },
}


class ShmAbiCrossProcessTests(unittest.TestCase):
    _tmpdir: tempfile.TemporaryDirectory[str]
    _go_bin: Path
    _rust_bin: Path

    @classmethod
    def _resolve_rust_binary(cls) -> Path:
        override = os.environ.get("ALEPH_SHM_ABI_CHECK_BIN")
        if override:
            candidate = Path(override)
            if not (candidate.is_file() and os.access(candidate, os.X_OK)):
                raise RuntimeError(f"ALEPH_SHM_ABI_CHECK_BIN is not executable: {candidate}")
            return candidate

        target_dir = Path(os.environ.get("CARGO_TARGET_DIR", REPO_ROOT / "target"))
        watched_sources = [
            REPO_ROOT / "src" / "bin" / "shm_abi_check.rs",
            REPO_ROOT / "src" / "shm_reader.rs",
            REPO_ROOT / "src" / "shm_depth_reader.rs",
            REPO_ROOT / "src" / "lib.rs",
        ]
        newest_src_mtime = max(p.stat().st_mtime for p in watched_sources if p.exists())

        candidates = [
            target_dir / "debug" / "shm_abi_check",
            target_dir / "release" / "shm_abi_check",
        ]
        fresh = [
            p
            for p in candidates
            if p.is_file() and os.access(p, os.X_OK) and p.stat().st_mtime >= newest_src_mtime
        ]
        if fresh:
            return max(fresh, key=lambda p: p.stat().st_mtime)

        if os.environ.get("ALEPH_SKIP_CARGO_BUILD") == "1" or shutil.which("cargo") is None:
            raise unittest.SkipTest(
                "shm_abi_check binary not built/current and cargo build skipped or unavailable"
            )

        subprocess.run(
            ["cargo", "build", "--locked", "--bin", "shm_abi_check"],
            cwd=REPO_ROOT,
            check=True,
        )
        built = target_dir / "debug" / "shm_abi_check"
        if not (built.is_file() and os.access(built, os.X_OK)):
            raise RuntimeError(f"cargo build did not produce {built}")
        return built

    @classmethod
    def setUpClass(cls) -> None:
        if shutil.which("go") is None:
            raise unittest.SkipTest("go toolchain not available")

        cls._rust_bin = cls._resolve_rust_binary()
        cls._tmpdir = tempfile.TemporaryDirectory(prefix="aleph-shm-abi-")
        cls.addClassCleanup(cls._tmpdir.cleanup)
        cls._go_bin = Path(cls._tmpdir.name) / "shm-verify"
        subprocess.run(
            ["go", "build", "-o", str(cls._go_bin), "./cmd/shm-verify"],
            cwd=REPO_ROOT / "feeder",
            check=True,
            capture_output=True,
            text=True,
        )

    def _shm_paths(self, label: str) -> tuple[Path, Path]:
        workdir = Path(self._tmpdir.name) / label
        workdir.mkdir(parents=True, exist_ok=True)
        return workdir / "aleph-matrix", workdir / "aleph-depth"

    def _run_go(self, matrix: Path, depth: Path, mode: str) -> None:
        subprocess.run(
            [
                str(self._go_bin),
                "-matrix",
                str(matrix),
                "-depth",
                str(depth),
                "-mode",
                mode,
            ],
            check=True,
            capture_output=True,
            text=True,
        )

    def _run_rust(self, matrix: Path, depth: Path, symbol: int, exchange: int) -> dict[str, object]:
        proc = subprocess.run(
            [
                str(self._rust_bin),
                "--matrix",
                str(matrix),
                "--depth",
                str(depth),
                "--symbol",
                str(symbol),
                "--exchange",
                str(exchange),
            ],
            check=True,
            capture_output=True,
            text=True,
        )
        payload = json.loads(proc.stdout)
        self.assertEqual(set(payload.keys()), {"symbol_id", "exchange_id", "bbo", "depth"})
        self.assertEqual(payload["symbol_id"], symbol)
        self.assertEqual(payload["exchange_id"], exchange)
        return payload

    def _assert_bbo_matches(self, bbo: dict[str, object] | None, exchange: int, symbol: int) -> None:
        self.assertIsNotNone(bbo)
        assert bbo is not None
        spec = FIXTURES[(exchange, symbol)]
        self.assertGreater(bbo["seqlock"], 0)
        self.assertEqual(bbo["seqlock"] % 2, 0)
        self.assertEqual(bbo["msg_type"], 1)
        self.assertEqual(bbo["exchange_id"], exchange)
        self.assertEqual(bbo["symbol_id"], symbol)
        self.assertGreater(bbo["timestamp_ns"], 0)
        self.assertEqual(bbo["timestamp_ns"], spec["timestamp_ns"])
        self.assertEqual(bbo["bid_price"], spec["bid_price"])
        self.assertEqual(bbo["bid_size"], spec["bid_size"])
        self.assertEqual(bbo["ask_price"], spec["ask_price"])
        self.assertEqual(bbo["ask_size"], spec["ask_size"])

    def _assert_depth_matches(
        self, depth: dict[str, object] | None, exchange: int, symbol: int
    ) -> None:
        self.assertIsNotNone(depth)
        assert depth is not None
        spec = FIXTURES[(exchange, symbol)]
        self.assertGreater(depth["seqlock"], 0)
        self.assertEqual(depth["seqlock"] % 2, 0)
        self.assertEqual(depth["exchange_id"], exchange)
        self.assertEqual(depth["symbol_id"], symbol)
        self.assertGreater(depth["timestamp_ns"], 0)
        self.assertEqual(depth["timestamp_ns"], spec["timestamp_ns"])

        bids = depth["bids"]
        asks = depth["asks"]
        self.assertEqual(len(bids), 5)
        self.assertEqual(len(asks), 5)
        for idx, (exp_px, exp_sz) in enumerate(spec["bids"]):
            self.assertEqual(bids[idx]["price"], exp_px)
            self.assertEqual(bids[idx]["size"], exp_sz)
        for idx, (exp_px, exp_sz) in enumerate(spec["asks"]):
            self.assertEqual(asks[idx]["price"], exp_px)
            self.assertEqual(asks[idx]["size"], exp_sz)

        for idx in range(1, 5):
            self.assertLess(bids[idx]["price"], bids[idx - 1]["price"])
            self.assertGreater(asks[idx]["price"], asks[idx - 1]["price"])
        self.assertLess(bids[0]["price"], asks[0]["price"])

    def test_write_mode_roundtrips_bbo_and_five_level_depth_across_go_and_rust(self) -> None:
        matrix, depth = self._shm_paths("write")
        self._run_go(matrix, depth, "write")

        for exchange, symbol in FIXTURES:
            payload = self._run_rust(matrix, depth, symbol, exchange)
            self._assert_bbo_matches(payload["bbo"], exchange, symbol)
            self._assert_depth_matches(payload["depth"], exchange, symbol)

        unpopulated = self._run_rust(matrix, depth, 1001, 2)
        self.assertIsNone(unpopulated["bbo"])
        self.assertIsNone(unpopulated["depth"])

    def test_invalidate_bbo_clears_bbo_while_preserving_depth(self) -> None:
        matrix, depth = self._shm_paths("invalidate_bbo")
        self._run_go(matrix, depth, "invalidate-bbo")

        for exchange, symbol in FIXTURES:
            payload = self._run_rust(matrix, depth, symbol, exchange)
            self.assertIsNone(payload["bbo"])
            self._assert_depth_matches(payload["depth"], exchange, symbol)

    def test_invalidate_depth_clears_depth_while_preserving_bbo(self) -> None:
        matrix, depth = self._shm_paths("invalidate_depth")
        self._run_go(matrix, depth, "invalidate-depth")

        for exchange, symbol in FIXTURES:
            payload = self._run_rust(matrix, depth, symbol, exchange)
            self._assert_bbo_matches(payload["bbo"], exchange, symbol)
            self.assertIsNone(payload["depth"])

    def test_binaries_fail_closed_on_invalid_inputs(self) -> None:
        matrix, depth = self._shm_paths("fail_closed")
        bad_mode = subprocess.run(
            [
                str(self._go_bin),
                "-matrix",
                str(matrix),
                "-depth",
                str(depth),
                "-mode",
                "unknown-mode",
            ],
            capture_output=True,
            text=True,
        )
        self.assertNotEqual(bad_mode.returncode, 0)

        self._run_go(matrix, depth, "write")
        bad_exchange = subprocess.run(
            [
                str(self._rust_bin),
                "--matrix",
                str(matrix),
                "--depth",
                str(depth),
                "--symbol",
                "1001",
                "--exchange",
                "12",
            ],
            capture_output=True,
            text=True,
        )
        self.assertNotEqual(bad_exchange.returncode, 0)

        truncated = Path(self._tmpdir.name) / "truncated-matrix"
        truncated.write_bytes(b"\x00" * 64)
        bad_file = subprocess.run(
            [
                str(self._rust_bin),
                "--matrix",
                str(truncated),
                "--depth",
                str(depth),
                "--symbol",
                "1001",
                "--exchange",
                "1",
            ],
            capture_output=True,
            text=True,
        )
        self.assertNotEqual(bad_file.returncode, 0)


if __name__ == "__main__":
    unittest.main()
