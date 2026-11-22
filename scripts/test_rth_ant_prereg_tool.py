#!/usr/bin/env python3
"""Unit and end-to-end integration tests for `scripts/rth_ant_prereg_tool.py`."""

from __future__ import annotations

from datetime import datetime, timedelta, timezone
import gzip
import hashlib
import io
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch
from zoneinfo import ZoneInfo

from scripts.rth_ant_prereg_tool import (
    create_prereg_manifest,
    create_seal_receipt,
    create_witness_query,
    main as prereg_cli_main,
    wrap_witness_receipt,
)
from scripts.run_oos_validation_gate import FROZEN_ANT_ENGINE_FILES, TEN_HOURS_MS, main as gate_cli_main, run_rth_ant_cli

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

            diagnostic = root / "diagnostic.json"
            create_prereg_manifest(rth_date_str="2026-11-24", output=diagnostic, repo_root=REPO_ROOT,
                                   allow_non_monday=True, now_ms=start_ms-60_000)
            self.assertEqual(json.loads(diagnostic.read_text())["classification"], "weekday_rth_diagnostic")

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

    def test_end_to_end_prereg_seal_requires_independent_time_witness(self) -> None:
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
            self.assertEqual(rc, 1)
            rep = json.loads(gate_out.read_text(encoding="utf-8"))
            self.assertEqual(rep["verdict"], "NO_GO")
            self.assertTrue(rep["passed_metrics"])
            self.assertTrue(rep["provenance"]["structural_checks_passed"])
            self.assertFalse(rep["provenance"]["qualified"])
            self.assertEqual([k for k, v in rep["provenance"]["checks"].items() if not v],
                             ["independent_preregistration_witness_verified"])

    def test_cli_rejects_manual_now_ms_override(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            for argv in (
                ["prereg", "--rth-date", "2026-11-23", "--output", str(root / "m.json"), "--now-ms", "1"],
                [
                    "seal",
                    "--prereg-manifest",
                    str(root / "m.json"),
                    "--capture-dirs",
                    str(root),
                    "--output",
                    str(root / "s.json"),
                    "--now-ms",
                    "1",
                ],
            ):
                with self.subTest(cmd=argv[0]), self.assertRaises(SystemExit) as ctx:
                    prereg_cli_main(argv)
                self.assertEqual(ctx.exception.code, 2)

    def test_witness_query_and_wrap_witness_enable_verified_go_via_openssl_tsa(self) -> None:
        now_et = datetime.now(timezone.utc).astimezone(ET_TZ)
        next_mon = (now_et + timedelta(days=(7 - now_et.weekday()) % 7 or 7)).date()
        start_ms = int(datetime(next_mon.year, next_mon.month, next_mon.day, 7, 0, tzinfo=ET_TZ).timestamp() * 1000)
        end_ms = start_ms + TEN_HOURS_MS
        ms = 1_000_000
        sec = 1_000_000_000
        start_ns = start_ms * ms
        end_ns = end_ms * ms

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            ca_key = root / "ca.key"
            ca_cert = root / "ca.pem"
            leaf_key = root / "leaf.key"
            leaf_csr = root / "leaf.csr"
            leaf_cert = root / "leaf.pem"
            ext_cnf = root / "ext.cnf"
            ts_cnf = root / "ts.cnf"
            serial = root / "serial"
            serial.write_text("01\n", encoding="utf-8")
            ext_cnf.write_text(
                "[tsa_ext]\n"
                "basicConstraints = critical,CA:FALSE\n"
                "keyUsage = critical,digitalSignature\n"
                "extendedKeyUsage = critical,timeStamping\n",
                encoding="utf-8",
            )
            ts_cnf.write_text(
                "[tsa]\ndefault_tsa = tsa_config1\n\n[tsa_config1]\n"
                f"dir = {root}\nserial = {serial}\nsigner_cert = {leaf_cert}\nsigner_key = {leaf_key}\n"
                "default_policy = 1.2.3.4.1\ndigests = sha256\naccuracy = millisecs:1\nclock_precision_digits = 0\nordering = yes\nsigner_digest = sha256\n",
                encoding="utf-8",
            )
            subprocess.run(
                ["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", str(ca_key), "-out", str(ca_cert), "-days", "30", "-subj", "/CN=ToolTestCA"],
                check=True,
                capture_output=True,
            )
            subprocess.run(
                ["openssl", "req", "-new", "-newkey", "rsa:2048", "-nodes", "-keyout", str(leaf_key), "-out", str(leaf_csr), "-subj", "/CN=ToolTestTSA"],
                check=True,
                capture_output=True,
            )
            subprocess.run(
                ["openssl", "x509", "-req", "-in", str(leaf_csr), "-CA", str(ca_cert), "-CAkey", str(ca_key), "-CAcreateserial", "-out", str(leaf_cert), "-days", "30", "-extfile", str(ext_cnf), "-extensions", "tsa_ext"],
                check=True,
                capture_output=True,
            )

            pre = root / "prereg.json"
            with patch("sys.stdout", new_callable=io.StringIO):
                self.assertEqual(
                    prereg_cli_main(["prereg", "--rth-date", next_mon.isoformat(), "--window-start-et", "07:00", "--output", str(pre)]),
                    0,
                )

            tsq = root / "prereg.tsq"
            tsr = root / "prereg.tsr"
            witness_json = root / "witness.json"
            with patch("sys.stdout", new_callable=io.StringIO):
                self.assertEqual(
                    prereg_cli_main(["witness-query", "--prereg-manifest", str(pre), "--output", str(tsq)]),
                    0,
                )
            with self.assertRaises(FileExistsError):
                create_witness_query(prereg_manifest_path=pre, output=tsq)

            subprocess.run(
                ["openssl", "ts", "-reply", "-config", str(ts_cnf), "-queryfile", str(tsq), "-out", str(tsr)],
                check=True,
                capture_output=True,
            )
            with patch("sys.stdout", new_callable=io.StringIO):
                self.assertEqual(
                    prereg_cli_main(["wrap-witness", "--prereg-manifest", str(pre), "--tsr", str(tsr), "--output", str(witness_json)]),
                    0,
                )
            with self.assertRaises(FileExistsError):
                wrap_witness_receipt(prereg_manifest_path=pre, tsr_path=tsr, output=witness_json)

            wrapped = json.loads(witness_json.read_text(encoding="utf-8"))
            self.assertEqual(wrapped["schema"], "aleph_tx_external_prereg_witness_v1")
            self.assertEqual(wrapped["provider"], "rfc3161_tsa")
            self.assertEqual(wrapped["prereg_manifest_sha256"], hashlib.sha256(pre.read_bytes()).hexdigest())

            cap = root / "rth_mon"
            cap.mkdir()
            rows: list[dict] = []
            for t in range(start_ns, end_ns + 1, 30 * sec):
                rows.append(_rate(t))
                rows.extend(_pair(t + ms, "99.99", "100.01"))
            rth_open_ns = int(datetime(next_mon.year, next_mon.month, next_mon.day, 9, 35, tzinfo=ET_TZ).timestamp() * 1000) * ms
            for i in range(4):
                t = rth_open_ns + i * 20 * sec
                rows.append(_rate(t - 2 * ms))
                for j in range(12):
                    rows.extend(_pair(t + j * 100 * ms, "99.90", "99.92"))
                for j in range(12):
                    rows.extend(_pair(t + (20 + j) * 100 * ms, "100.13", "100.15"))
            rows.sort(key=lambda r: r["capture_timing"]["available_wall_ns"])
            seg = cap / f"tokyo-research-{next_mon.strftime('%Y%m%d')}T120000Z.seg000.ndjson.gz"
            with gzip.open(seg, "wt", encoding="utf-8") as handle:
                for row in rows:
                    handle.write(json.dumps(row) + "\n")

            seal = root / "seal.json"
            create_seal_receipt(prereg_manifest_path=pre, capture_dirs=[cap], output=seal, now_ms=end_ms + 30_000)

            gate_out = root / "gate_go.json"
            with patch("sys.stdout", new_callable=io.StringIO):
                rc_go = run_rth_ant_cli(
                    [cap],
                    "monday_rth_oos",
                    gate_out,
                    REPO_ROOT,
                    prereg_manifest_path=pre,
                    seal_receipt_path=seal,
                    external_witness_path=witness_json,
                    trusted_tsa_ca_path=ca_cert,
                )
            self.assertEqual(rc_go, 0)
            rep = json.loads(gate_out.read_text(encoding="utf-8"))
            self.assertEqual(rep["verdict"], "GO")
            self.assertTrue(rep["provenance"]["qualified"])
            self.assertTrue(rep["provenance"]["checks"]["independent_preregistration_witness_verified"])
            self.assertEqual(rep["provenance"]["pre_registration_clock_trust"], "verified_rfc3161_tsa_witness")
            self.assertLessEqual(rep["provenance"]["witness_time_ms"], start_ms)

            # Verify CLI flag wiring (--external-witness / --trusted-tsa-ca) end-to-end.
            cli_out = root / "gate_cli_go.json"
            with patch.object(
                sys,
                "argv",
                [
                    "run_oos_validation_gate.py",
                    "rth-ant",
                    "--capture-dirs",
                    str(cap),
                    "--role",
                    "monday_rth_oos",
                    "--prereg-manifest",
                    str(pre),
                    "--seal-receipt",
                    str(seal),
                    "--external-witness",
                    str(witness_json),
                    "--trusted-tsa-ca",
                    str(ca_cert),
                    "--output",
                    str(cli_out),
                ],
            ), patch("sys.stdout", new_callable=io.StringIO):
                self.assertEqual(gate_cli_main(), 0)
            self.assertEqual(json.loads(cli_out.read_text(encoding="utf-8"))["verdict"], "GO")


if __name__ == "__main__":
    unittest.main()
