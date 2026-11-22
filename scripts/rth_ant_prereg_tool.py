#!/usr/bin/env python3
"""Pre-capture RTH preregistration manifest and post-capture seal receipt tool (`Step 2`).

Produces the split two-receipt provenance chain required by
`scripts/run_oos_validation_gate.py rth-ant --role monday_rth_oos`:
  1. `prereg`: Freezes `--prereg-manifest` BEFORE `decision_start_ms`, binding the
     exact 10-hour `[decision_start_ms, decision_end_ms)` US equities RTH window and
     the frozen ANT simulation engine SHA-256 digests (with zero `segment_sha256`).
  2. `seal`: Seals `--seal-receipt` AFTER `decision_end_ms`, linking the exact
     `prereg_manifest_sha256`, verifying every sealed segment SHA-256, and recording
     `measured_source_coverage` across `arcus_book`, `xyz_book`, and `xyz_funding`.
"""

from __future__ import annotations

import argparse
import base64
from datetime import date, datetime, time as dt_time
import gzip
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import time
from typing import Any
from zoneinfo import ZoneInfo

try:
    from scripts.run_oos_validation_gate import (
        ANT_STRATEGY_CONTRACT,
        FROZEN_ANT_ENGINE_FILES,
        TEN_HOURS_MS,
        _covered_us_rth_dates,
    )
except ModuleNotFoundError:  # pragma: no cover - direct CLI invocation
    sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
    from scripts.run_oos_validation_gate import (
        ANT_STRATEGY_CONTRACT,
        FROZEN_ANT_ENGINE_FILES,
        TEN_HOURS_MS,
        _covered_us_rth_dates,
    )

ET_TZ = ZoneInfo("America/New_York")
DEFAULT_REPO_ROOT = Path(__file__).resolve().parent.parent


def _sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def _parse_hhmm(raw: str) -> dt_time:
    parts = raw.strip().split(":")
    if len(parts) != 2:
        raise ValueError(f"invalid HH:MM time: {raw!r}")
    hour, minute = int(parts[0]), int(parts[1])
    return dt_time(hour, minute)


def _valid_positive_book(levels_bid: list[Any], levels_ask: list[Any], *, is_hl: bool) -> bool:
    bids: list[tuple[float, float]] = []
    asks: list[tuple[float, float]] = []
    try:
        if is_hl:
            bids = [(float(x["px"]), float(x["sz"])) for x in levels_bid if float(x["sz"]) > 0]
            asks = [(float(x["px"]), float(x["sz"])) for x in levels_ask if float(x["sz"]) > 0]
        else:
            bids = [(float(x[0]), float(x[1])) for x in levels_bid if float(x[1]) > 0]
            asks = [(float(x[0]), float(x[1])) for x in levels_ask if float(x[1]) > 0]
    except (KeyError, IndexError, TypeError, ValueError):
        return False
    return bool(bids and asks and bids[0][0] > 0 and asks[0][0] > 0 and bids[0][0] < asks[0][0])


def create_prereg_manifest(
    *,
    rth_date_str: str,
    window_start_et: str = "07:00",
    output: Path,
    repo_root: Path = DEFAULT_REPO_ROOT,
    allow_non_monday: bool = False,
    now_ms: int | None = None,
    force: bool = False,
) -> dict[str, Any]:
    """Create and write a pre-capture RTH preregistration manifest."""
    if output.exists() and not force:
        raise FileExistsError(f"refusing to overwrite existing manifest {output} without --force")

    target_date = date.fromisoformat(rth_date_str)
    if not allow_non_monday and target_date.weekday() != 0:
        raise ValueError(f"{rth_date_str} is weekday {target_date.weekday()}, expected Monday (pass --allow-non-monday to override)")

    start_time = _parse_hhmm(window_start_et)
    start_dt = datetime.combine(target_date, start_time, tzinfo=ET_TZ)
    decision_start_ms = int(start_dt.timestamp() * 1000)
    decision_end_ms = decision_start_ms + TEN_HOURS_MS
    frozen_at_ms = int(time.time() * 1000) if now_ms is None else int(now_ms)
    if frozen_at_ms > decision_start_ms:
        raise ValueError(
            f"preregistration frozen_at_ms={frozen_at_ms} exceeds decision_start_ms={decision_start_ms}; cannot pre-register after start"
        )

    covered = _covered_us_rth_dates(
        decision_start_ms,
        decision_end_ms,
        require_monday=not allow_non_monday,
        expected_rth_date_et=target_date.isoformat(),
    )
    if covered != [target_date.isoformat()]:
        raise ValueError(
            f"10h window starting {window_start_et} ET does not cover full US equities RTH 09:30-16:00 ET on {target_date.isoformat()}"
        )

    engine_hashes: dict[str, str] = {}
    for rel in FROZEN_ANT_ENGINE_FILES:
        engine_path = (repo_root / rel).resolve()
        if not engine_path.is_file():
            raise FileNotFoundError(f"missing frozen ANT engine file: {engine_path}")
        engine_hashes[rel] = _sha256_file(engine_path)

    manifest = {
        "schema": "aleph_tx_rth_ant_prereg_manifest_v1",
        "classification": "weekday_rth_diagnostic" if allow_non_monday else "monday_rth_oos",
        "market_session": "us_equities_rth",
        "require_monday": not allow_non_monday,
        "expected_rth_date_et": target_date.isoformat(),
        "window_start_et": window_start_et,
        "frozen_at_ms": frozen_at_ms,
        "decision_start_ms": decision_start_ms,
        "decision_end_ms": decision_end_ms,
        "max_source_book_gap_ms": 300_000,
        "frozen_engine_sha256": engine_hashes,
        "strategy_contract": ANT_STRATEGY_CONTRACT,
    }
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return {
        "output": str(output),
        "prereg_manifest_sha256": _sha256_file(output),
        "expected_rth_date_et": target_date.isoformat(),
        "frozen_at_ms": frozen_at_ms,
        "decision_start_ms": decision_start_ms,
        "decision_end_ms": decision_end_ms,
        "decision_start_utc": datetime.fromtimestamp(decision_start_ms / 1000.0, tz=datetime.now().astimezone().tzinfo).astimezone(ZoneInfo("UTC")).isoformat(),
        "decision_end_utc": datetime.fromtimestamp(decision_end_ms / 1000.0, tz=ZoneInfo("UTC")).isoformat(),
    }


def _scan_segment_metrics(path: Path) -> tuple[str, int, int, int, int, int]:
    digest = _sha256_file(path)
    first_rx = 0
    last_rx = 0
    arc_events = 0
    xyz_events = 0
    fund_events = 0
    with gzip.open(path, "rt", encoding="utf-8") as handle:
        for line in handle:
            if '"ANT-USD"' not in line and '"xyz:ANT"' not in line:
                continue
            try:
                rec = json.loads(line)
            except ValueError:
                continue
            if rec.get("event") != "message":
                continue
            t_rx = (rec.get("capture_timing") or {}).get("available_wall_ns")
            if not isinstance(t_rx, int) or isinstance(t_rx, bool) or t_rx <= 0:
                continue
            first_rx = t_rx if first_rx == 0 else min(first_rx, t_rx)
            last_rx = max(last_rx, t_rx)
            src = rec.get("source")
            mkt = rec.get("market")
            pl = rec.get("payload") or {}
            if src == "arcus" and pl.get("channel") == "l2Orderbook" and pl.get("id") == "ANT-USD":
                contents = pl.get("contents") or {}
                if _valid_positive_book(contents.get("bids") or [], contents.get("asks") or [], is_hl=False):
                    arc_events += 1
            elif src == "hyperliquid" and mkt == "xyz:ANT":
                ch = pl.get("channel")
                if ch == "l2Book" and "data" in pl:
                    lv = pl["data"].get("levels") or [[], []]
                    if len(lv) >= 2 and _valid_positive_book(lv[0], lv[1], is_hl=True):
                        xyz_events += 1
                elif ch == "activeAssetCtx" and "data" in pl:
                    ctx = pl["data"].get("ctx") or {}
                    if ctx.get("funding") is not None:
                        try:
                            float(ctx["funding"])
                        except (TypeError, ValueError):
                            pass
                        else:
                            fund_events += 1
    return digest, first_rx, last_rx, arc_events, xyz_events, fund_events


def create_seal_receipt(
    *,
    prereg_manifest_path: Path,
    capture_dirs: list[Path],
    output: Path,
    now_ms: int | None = None,
    force: bool = False,
) -> dict[str, Any]:
    """Verify sealed segments after `decision_end_ms` and emit a tamper-evident seal receipt."""
    if output.exists() and not force:
        raise FileExistsError(f"refusing to overwrite existing seal receipt {output} without --force")
    if not prereg_manifest_path.is_file():
        raise FileNotFoundError(f"missing prereg manifest: {prereg_manifest_path}")

    manifest = json.loads(prereg_manifest_path.read_text(encoding="utf-8"))
    if "segment_sha256" in manifest:
        raise ValueError("prereg manifest unexpectedly contains segment_sha256")
    decision_end_ms = manifest.get("decision_end_ms")
    if not isinstance(decision_end_ms, int) or isinstance(decision_end_ms, bool) or decision_end_ms <= 0:
        raise ValueError(f"invalid decision_end_ms in {prereg_manifest_path}")

    sealed_at_ms = int(time.time() * 1000) if now_ms is None else int(now_ms)
    if sealed_at_ms < decision_end_ms:
        raise ValueError(
            f"cannot seal before decision_end_ms: sealed_at_ms={sealed_at_ms} < decision_end_ms={decision_end_ms}"
        )

    files: list[tuple[Path, Path]] = []
    for d in capture_dirs:
        if not d.is_dir():
            raise FileNotFoundError(f"missing capture directory: {d}")
        for seg in sorted(d.glob("tokyo-research-*.seg*.ndjson.gz")):
            files.append((d, seg))
    if not files:
        raise FileNotFoundError("no tokyo-research-*.seg*.ndjson.gz segments found to seal")

    use_dir_prefix = len(capture_dirs) > 1
    if use_dir_prefix and len({d.name for d in capture_dirs}) != len(capture_dirs):
        raise ValueError("capture directories must have distinct basenames when sealing multiple directories")

    segment_sha256: dict[str, str] = {}
    archive_first_rx_ns = 0
    archive_last_rx_ns = 0
    arc_total = 0
    xyz_total = 0
    fund_total = 0
    for d, seg in files:
        key = f"{d.name}/{seg.name}" if use_dir_prefix else seg.name
        if key in segment_sha256:
            raise ValueError(f"duplicate segment key {key!r}")
        digest, f_ns, l_ns, a_cnt, x_cnt, fn_cnt = _scan_segment_metrics(seg)
        segment_sha256[key] = digest
        if f_ns > 0:
            archive_first_rx_ns = f_ns if archive_first_rx_ns == 0 else min(archive_first_rx_ns, f_ns)
            archive_last_rx_ns = max(archive_last_rx_ns, l_ns)
        arc_total += a_cnt
        xyz_total += x_cnt
        fund_total += fn_cnt

    if archive_first_rx_ns <= 0 or arc_total <= 0 or xyz_total <= 0:
        raise ValueError(
            f"incomplete ANT archive coverage: first_rx={archive_first_rx_ns}, arc={arc_total}, xyz={xyz_total}"
        )

    prereg_sha256 = _sha256_file(prereg_manifest_path)
    receipt = {
        "schema": "aleph_tx_rth_ant_seal_receipt_v1",
        "prereg_manifest_sha256": prereg_sha256,
        "sealed_at_ms": sealed_at_ms,
        "segment_sha256": segment_sha256,
        "measured_source_coverage": {
            "archive_first_rx_ns": archive_first_rx_ns,
            "archive_last_rx_ns": archive_last_rx_ns,
            "arcus_book_events": arc_total,
            "xyz_book_events": xyz_total,
            "xyz_funding_events": fund_total,
        },
    }
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(receipt, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return {
        "output": str(output),
        "seal_receipt_sha256": _sha256_file(output),
        "prereg_manifest_sha256": prereg_sha256,
        "sealed_at_ms": sealed_at_ms,
        "segment_count": len(segment_sha256),
        "measured_source_coverage": receipt["measured_source_coverage"],
    }


def _load_valid_prereg_manifest_sha256(prereg_manifest_path: Path) -> str:
    if not prereg_manifest_path.is_file():
        raise FileNotFoundError(f"missing prereg manifest: {prereg_manifest_path}")
    raw_bytes = prereg_manifest_path.read_bytes()
    manifest = json.loads(raw_bytes.decode("utf-8"))
    if not isinstance(manifest, dict):
        raise ValueError(f"prereg manifest must be a JSON object: {prereg_manifest_path}")
    if "segment_sha256" in manifest:
        raise ValueError("prereg manifest unexpectedly contains segment_sha256")
    return hashlib.sha256(raw_bytes).hexdigest()


def create_witness_query(
    *,
    prereg_manifest_path: Path,
    output: Path,
    force: bool = False,
) -> dict[str, Any]:
    """Create an RFC 3161 `.tsq` timestamp query bound to `--prereg-manifest` SHA-256."""
    if output.exists() and not force:
        raise FileExistsError(f"refusing to overwrite existing witness query {output} without --force")
    prereg_sha256 = _load_valid_prereg_manifest_sha256(prereg_manifest_path)
    output.parent.mkdir(parents=True, exist_ok=True)
    subprocess.run(
        [
            "openssl",
            "ts",
            "-query",
            "-digest",
            prereg_sha256,
            "-sha256",
            "-cert",
            "-out",
            str(output),
        ],
        capture_output=True,
        text=True,
        check=True,
        timeout=10,
    )
    return {
        "output": str(output),
        "prereg_manifest_sha256": prereg_sha256,
        "tsq_sha256": _sha256_file(output),
    }


def wrap_witness_receipt(
    *,
    prereg_manifest_path: Path,
    tsr_path: Path,
    output: Path,
    force: bool = False,
) -> dict[str, Any]:
    """Wrap a TSA `.tsr` response into `aleph_tx_external_prereg_witness_v1` JSON."""
    if output.exists() and not force:
        raise FileExistsError(f"refusing to overwrite existing witness receipt {output} without --force")
    if not tsr_path.is_file():
        raise FileNotFoundError(f"missing TSR reply file: {tsr_path}")
    tsr_bytes = tsr_path.read_bytes()
    if not tsr_bytes:
        raise ValueError(f"empty TSR reply file: {tsr_path}")
    prereg_sha256 = _load_valid_prereg_manifest_sha256(prereg_manifest_path)
    witness = {
        "schema": "aleph_tx_external_prereg_witness_v1",
        "provider": "rfc3161_tsa",
        "prereg_manifest_sha256": prereg_sha256,
        "tsr_base64": base64.b64encode(tsr_bytes).decode("ascii"),
    }
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(witness, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return {
        "output": str(output),
        "schema": witness["schema"],
        "provider": witness["provider"],
        "prereg_manifest_sha256": prereg_sha256,
        "witness_sha256": _sha256_file(output),
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)

    p_pre = sub.add_parser("prereg", help="Create pre-capture RTH preregistration manifest")
    p_pre.add_argument("--rth-date", required=True, help="Target ET session date (YYYY-MM-DD)")
    p_pre.add_argument("--window-start-et", default="07:00", help="10h decision window start in ET (default 07:00 -> 17:00 ET)")
    p_pre.add_argument("--output", type=Path, required=True)
    p_pre.add_argument("--allow-non-monday", action="store_true")
    p_pre.add_argument("--force", action="store_true")

    p_seal = sub.add_parser("seal", help="Seal completed capture directories against a frozen prereg manifest")
    p_seal.add_argument("--prereg-manifest", type=Path, required=True)
    p_seal.add_argument("--capture-dirs", nargs="+", type=Path, required=True)
    p_seal.add_argument("--output", type=Path, required=True)
    p_seal.add_argument("--force", action="store_true")

    p_wq = sub.add_parser("witness-query", help="Create RFC 3161 .tsq query for a preregistration manifest")
    p_wq.add_argument("--prereg-manifest", type=Path, required=True)
    p_wq.add_argument("--output", type=Path, required=True)
    p_wq.add_argument("--force", action="store_true")

    p_ww = sub.add_parser("wrap-witness", help="Wrap an RFC 3161 .tsr reply into external witness JSON")
    p_ww.add_argument("--prereg-manifest", type=Path, required=True)
    p_ww.add_argument("--tsr", type=Path, required=True)
    p_ww.add_argument("--output", type=Path, required=True)
    p_ww.add_argument("--force", action="store_true")

    args = parser.parse_args(argv)
    if args.command == "prereg":
        res = create_prereg_manifest(
            rth_date_str=args.rth_date,
            window_start_et=args.window_start_et,
            output=args.output,
            allow_non_monday=args.allow_non_monday,
            force=args.force,
        )
        print(json.dumps(res, indent=2))
        return 0
    if args.command == "seal":
        res = create_seal_receipt(
            prereg_manifest_path=args.prereg_manifest,
            capture_dirs=args.capture_dirs,
            output=args.output,
            force=args.force,
        )
        print(json.dumps(res, indent=2))
        return 0
    if args.command == "witness-query":
        res = create_witness_query(
            prereg_manifest_path=args.prereg_manifest,
            output=args.output,
            force=args.force,
        )
        print(json.dumps(res, indent=2))
        return 0
    if args.command == "wrap-witness":
        res = wrap_witness_receipt(
            prereg_manifest_path=args.prereg_manifest,
            tsr_path=args.tsr,
            output=args.output,
            force=args.force,
        )
        print(json.dumps(res, indent=2))
        return 0
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
