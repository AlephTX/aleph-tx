#!/usr/bin/env python3
"""Unit and end-to-end integration tests for `scripts/rth_ant_prereg_tool.py`."""

from __future__ import annotations

from datetime import datetime
import gzip
import json
from pathlib import Path
import tempfile
import unittest
from zoneinfo import ZoneInfo

from scripts.rth_ant_prereg_tool import create_prereg_manifest, create_seal_receipt
from scripts.run_oos_validation_gate import FROZEN_ANT_ENGINE_FILES, TEN_HOURS_MS, run_rth_ant_cli

ET_TZ = ZoneInfo("America/New_York")
REPO_ROOT = Path(__file__).resolve().parent.parent


def _msg(t_ns: int, source: str, market: str, payload: dict) -> dict:
    return {
        "event": "message",
        "source": source,
        "market": market,
        "capture_timing": {"available_wall_ns": t_ns},
        "payload": payload,
    }


def _pair(t_ns: int, ab: str, aa: str, xb: str = "100.00", xa: str = "100.02") -> list[dict]:
    ms = 1_000_000
    return [
        _msg(
            t_ns,
            "arcus",
            "ANT-USD",
            {
                "channel": "l2Orderbook",
                "id": "ANT-USD",
                "contents": {"timestamp": t_ns // 1000, "bids": [[ab, "1000"]], "asks": [[aa, "1000"]]},
            },
        ),
        _msg(
            t_ns + ms,
            "hyperliquid",
            "xyz:ANT",
            {
                "channel": "l2Book",
                "data": {"levels": [[{"px": xb, "sz": "1000"}], [{"px": xa, "sz": "1000"}]]},
            },
        ),
    ]


def _rate(t_ns: int) -> dict:
    return _msg(t_ns, "hyperliquid", "xyz:ANT", {"channel": "activeAssetCtx", "data": {"ctx": {"funding": "0"}}})


class RthAntPreregToolTests(unittest.TestCase):
    def test_prereg_enforces_monday_rth_bounds_future_start_and_no_overwrite(self) -> None:
        start_ms = int(datetime(2026, 11, 23, 7, 0, tzinfo=ET_TZ).timestamp() * 1000)
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            out = root / "manifest.json"
            with self.assertRaises(ValueError):
                create_prereg_manifest(
                    rth_date_str="2026-11-24",
                    output=out,
                    repo_root=REPO_ROOT,
                    now_ms=start_ms - 60_000,
                )
            with self.assertRaises(ValueError):
                create_prereg_manifest(
                    rth_date_str="2026-11-23",
                    output=out,
                    repo_root=REPO_ROOT,
                    now_ms=start_ms + 1,
                )
            with self.assertRaises(ValueError):
                create_prereg_manifest(
                    rth_date_str="2026-11-23",
                    window_start_et="05:00",
                    output=out,
                    repo_root=REPO_ROOT,
                    now_ms=start_ms - 10_000_000,
                )

            res = create_prereg_manifest(
                rth_date_str="2026-11-23",
                window_start_et="07:00",
                output=out,
                repo_root=REPO_ROOT,
                now_ms=start_ms - 60_000,
            )
            payload = json.loads(out.read_text(encoding="utf-8"))
            self.assertEqual(payload["classification"], "monday_rth_oos")
            self.assertEqual(payload["decision_end_ms"] - payload["decision_start_ms"], TEN_HOURS_MS)
            self.assertNotIn("segment_sha256", payload)
            self.assertEqual(set(payload["frozen_engine_sha256"]), set(FROZEN_ANT_ENGINE_FILES))
            self.assertEqual(res["decision_start_ms"], start_ms)

            with self.assertRaises(FileExistsError):
                create_prereg_manifest(
                    rth_date_str="2026-11-23",
                    window_start_et="07:00",
                    output=out,
                    repo_root=REPO_ROOT,
                    now_ms=start_ms - 59_000,
                )

    def test_seal_rejects_early_call_and_empty_capture_dirs(self) -> None:
        start_ms = int(datetime(2026, 11, 23, 7, 0, tzinfo=ET_TZ).timestamp() * 1000)
        end_ms = start_ms + TEN_HOURS_MS
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            pre = root / "prereg.json"
            create_prereg_manifest(
                rth_date_str="2026-11-23",
                window_start_et="07:00",
                output=pre,
                repo_root=REPO_ROOT,
                now_ms=start_ms - 60_000,
            )
            cap = root / "cap"
            cap.mkdir()
            seal_out = root / "seal.json"
            with self.assertRaises(ValueError):
                create_seal_receipt(
                    prereg_manifest_path=pre,
                    capture_dirs=[cap],
                    output=seal_out,
                    now_ms=end_ms - 1,
                )
            with self.assertRaises(FileNotFoundError):
                create_seal_receipt(
                    prereg_manifest_path=pre,
                    capture_dirs=[cap],
                    output=seal_out,
                    now_ms=end_ms,
                )

    def test_end_to_end_prereg_seal_passes_run_oos_validation_gate_monday_rth_go(self) -> None:
        start_ms = int(datetime(2026, 11, 23, 7, 0, tzinfo=ET_TZ).timestamp() * 1000)
        end_ms = start_ms + TEN_HOURS_MS
        ms = 1_000_000
        sec = 1_000_000_000
        start_ns = start_ms * ms
        end_ns = end_ms * ms

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            pre = root / "prereg.json"
            create_prereg_manifest(
                rth_date_str="2026-11-23",
                window_start_et="07:00",
                output=pre,
                repo_root=REPO_ROOT,
                now_ms=start_ms - 120_000,
            )

            cap = root / "rth_mon"
            cap.mkdir()
            rows: list[dict] = []
            # Pre-window warmup record and post-window trailing record to verify bounds isolation.
            rows.extend(_pair(start_ns - 60 * sec, "99.99", "100.01"))
            for t in range(start_ns, end_ns + 1, 30 * sec):
                rows.append(_rate(t))
                rows.extend(_pair(t + ms, "99.99", "100.01"))
            rows.extend(_pair(end_ns + 10 * sec, "99.99", "100.01"))

            rth_open_ns = int(datetime(2026, 11, 23, 9, 35, tzinfo=ET_TZ).timestamp() * 1000) * ms
            for i in range(4):
                t = rth_open_ns + i * 20 * sec
                rows.append(_rate(t - 2 * ms))
                for j in range(12):
                    rows.extend(_pair(t + j * 100 * ms, "99.90", "99.92"))
                for j in range(12):
                    rows.extend(_pair(t + (20 + j) * 100 * ms, "100.13", "100.15"))

            rows.sort(key=lambda r: r["capture_timing"]["available_wall_ns"])
            seg = cap / "tokyo-research-20261123T120000Z.seg000.ndjson.gz"
            with gzip.open(seg, "wt", encoding="utf-8") as handle:
                for row in rows:
                    handle.write(json.dumps(row) + "\n")

            seal = root / "seal.json"
            create_seal_receipt(
                prereg_manifest_path=pre,
                capture_dirs=[cap],
                output=seal,
                now_ms=end_ms + 30_000,
            )

            gate_out = root / "gate.json"
            rc = run_rth_ant_cli(
                [cap],
                "monday_rth_oos",
                gate_out,
                REPO_ROOT,
                prereg_manifest_path=pre,
                seal_receipt_path=seal,
            )
            self.assertEqual(rc, 0)
            rep = json.loads(gate_out.read_text(encoding="utf-8"))
            self.assertEqual(rep["verdict"], "GO")
            self.assertTrue(rep["provenance"]["qualified"])
            self.assertTrue(all(rep["provenance"]["checks"].values()), rep["provenance"]["checks"])


if __name__ == "__main__":
    unittest.main()
