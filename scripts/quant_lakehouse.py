#!/usr/bin/env python3
"""Quantitative Market Data Lakehouse pipeline for AlephTX.

Implements the Bronze -> Silver storage and normalization contract across:
  - Cloud collectors (e.g., oasis-tokyo): seal immutable public captures, emit
    cryptographic manifests, and safely evict multi-GB local files once remote
    Google Drive upload receipts match SHA-256 and byte length.
  - Local research workstations (e.g., WSL2 Ubuntu NVMe): normalize sealed
    public captures across Arcus, Entropy (io:*), TradeXYZ (xyz:*), Lighter
    Robinhood, Lighter Core, Binance USD-M, BULK, Extended, Hyperliquid Core,
    and Coinbase into four receipt-causal tables, bundle/unpack Parquet shards
    for Google Drive transport without small-file API throttling, and compile
    Zstd Parquet + DuckDB views.

Safety invariants:
  - Public market data only: symlinks, active .partial/.tmp/.next files, and
    filenames matching private/account/order/fill/telemetry/credential patterns
    are unconditionally rejected.
  - Causal ordering: rows are indexed and sorted by
    (venue, domain, market, available_at_ms, sequence). Missing source clocks
    remain NULL and are never substituted with local receipt timestamps.
"""

from __future__ import annotations

import argparse
from collections import Counter
from contextlib import ExitStack
from datetime import datetime, timezone
from decimal import Decimal, InvalidOperation
import gzip
from hashlib import sha256
import io
import json
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import tarfile
from tempfile import TemporaryDirectory

from archive_public_capture import MAX_ROW, open_capture, records as archive_records, rows as stream_rows
from capture_timing import ClockAudit, precise_available_ms
from inventory_public_sources import PRIVATE_NAME


TEN_HOUR_MS = 10 * 3_600_000
EPOCH_MS_MIN = 1_000_000_000_000
EPOCH_MS_MAX = 10_000_000_000_000
MAX_PRICE_DEC = Decimal("1000000000")
MAX_LIGHTER_LEVELS = 50_000
TABLES = ("bbo_quotes", "public_trades", "funding_and_ctx", "feed_lifecycle")
VALID_REGIONS = ("tokyo", "taiwan", "wsl", "local", "other")
VALID_ROLES = ("selection_dev", "holdout_vault", "legacy_diagnostic")

RAW_SOURCE_MAP = {
    "binance": ("binance", "usdm"),
    "lighter": ("lighter", "core"),
    "lighter_trades": ("lighter", "core"),
    "lighter_robinhood": ("lighter_robinhood", "robinhood"),
    "lighter_robinhood_trades": ("lighter_robinhood", "robinhood"),
    "extended": ("extended", "mainnet"),
    "extended_trades": ("extended", "mainnet"),
    "extended_index": ("extended", "mainnet"),
    "arcus": ("arcus", "mainnet"),
    "arcus_snapshot": ("arcus", "mainnet"),
    "arcus_trades": ("arcus", "mainnet"),
    "bulk": ("bulk", "mainnet"),
    "bulk_snapshot": ("bulk", "mainnet"),
    "coinbase": ("coinbase", "spot"),
    "collector": ("collector", "session"),
    "hyperliquid": ("hyperliquid", "core"),
    "hyperliquid_snapshot": ("hyperliquid", "core"),
    "hyperliquid_trades": ("hyperliquid", "core"),
    "hyperliquid_funding": ("hyperliquid", "core"),
}


def reject_unsafe_path(
    path: Path,
    *,
    allow_directory: bool = False,
    must_be_directory: bool = False,
) -> Path:
    """Reject symlinks, incomplete captures, and private-indicating names."""
    raw = Path(path)
    if raw.is_symlink():
        raise ValueError(f"symlink rejected: {raw}")
    for part in (raw.name, raw.parent.name):
        if ".partial" in part or part.endswith((".tmp", ".next")):
            raise ValueError(f"active or incomplete artifact rejected: {raw}")
        if PRIVATE_NAME.search(part):
            raise ValueError(f"private-indicating path rejected: {raw}")
    resolved = raw.resolve(strict=True)
    if resolved != raw.absolute() and raw.is_symlink():
        raise ValueError(f"symlink rejected: {raw}")
    if must_be_directory:
        if not resolved.is_dir():
            raise ValueError(f"expected directory: {raw}")
    elif allow_directory:
        if not (resolved.is_dir() or resolved.is_file()):
            raise ValueError(f"expected directory or regular file: {raw}")
    elif not resolved.is_file():
        raise ValueError(f"not a regular file: {raw}")
    return resolved


def digest_unchanged(path: Path) -> tuple[str, int]:
    """Compute SHA-256 while verifying the file did not mutate during reading."""
    if path.is_symlink():
        raise ValueError(f"symlink rejected: {path}")
    fd = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
    with os.fdopen(fd, "rb") as handle:
        before = os.fstat(handle.fileno())
        if not stat.S_ISREG(before.st_mode):
            raise ValueError(f"not a regular file: {path}")
        hasher = sha256()
        while chunk := handle.read(1024 * 1024):
            hasher.update(chunk)
        after = os.fstat(handle.fileno())
    current = path.lstat()
    sig = lambda st: (st.st_dev, st.st_ino, st.st_size, st.st_mtime_ns)
    if sig(before) != sig(after) or sig(after) != sig(current):
        raise ValueError(f"artifact mutated during hashing: {path}")
    return hasher.hexdigest(), after.st_size


def write_immutable_json(path: Path, payload: dict) -> None:
    """Write a new read-only JSON artifact without overwriting existing files."""
    if path.exists() or path.is_symlink():
        raise FileExistsError(f"refusing to overwrite immutable artifact: {path}")
    if PRIVATE_NAME.search(path.name):
        raise ValueError(f"private-indicating output name rejected: {path.name}")
    path.parent.mkdir(parents=True, exist_ok=True)
    encoded = json.dumps(payload, indent=2, sort_keys=True, allow_nan=False) + "\n"
    old_umask = os.umask(0o077)
    tmp_path = path.with_name(f".{path.name}.tmp.{os.getpid()}")
    try:
        with tmp_path.open("x", encoding="utf-8") as handle:
            handle.write(encoded)
            handle.flush()
            os.fsync(handle.fileno())
        os.link(tmp_path, path)
    finally:
        if tmp_path.exists():
            tmp_path.unlink()
        os.umask(old_umask)


def utc_date_str(ms: int) -> str:
    return datetime.fromtimestamp(ms / 1000.0, tz=timezone.utc).strftime("%Y-%m-%d")


def classify_venue_domain(raw_source: str, market: str | None) -> tuple[str, str]:
    if raw_source not in RAW_SOURCE_MAP:
        raise ValueError(f"unsupported public source: {raw_source}")
    venue, domain = RAW_SOURCE_MAP[raw_source]
    if venue == "hyperliquid":
        if market and ":" in market:
            prefix = market.split(":", 1)[0]
            if not re.fullmatch(r"[A-Za-z0-9_-]{1,30}", prefix):
                raise ValueError(f"invalid Hyperliquid HIP-3 domain prefix: {market}")
            domain = f"hip3-{prefix}"
            if prefix == "io":
                venue = "entropy"
            elif prefix == "xyz":
                venue = "trade_xyz"
            else:
                venue = f"hyperliquid_{prefix}"
        elif not market:
            domain = "shared"
    return venue, domain


def parse_positive_dec(raw_val, *, allow_zero: bool = False) -> Decimal:
    text = str(raw_val)
    try:
        dec = Decimal(text)
    except InvalidOperation as exc:
        raise ValueError(f"invalid decimal {raw_val!r}") from exc
    if not dec.is_finite() or dec > MAX_PRICE_DEC:
        raise ValueError(f"out-of-range decimal {raw_val!r}")
    if dec < 0 or (dec == 0 and not allow_zero):
        raise ValueError(f"non-positive decimal {raw_val!r}")
    return dec


def format_dec(dec: Decimal) -> str:
    text = format(dec, "f")
    if "." in text:
        text = text.rstrip("0").rstrip(".")
    return text or "0"


def parse_rfc3339_ms(value: str) -> int:
    normalized = value[:-1] + "+00:00" if value.endswith("Z") else value
    dt = datetime.fromisoformat(normalized)
    if dt.tzinfo is None:
        raise ValueError("Coinbase timestamp missing timezone")
    return int(dt.timestamp() * 1000)


def bulk_timestamp_ms(raw_ts: int) -> int:
    if EPOCH_MS_MIN <= raw_ts < EPOCH_MS_MAX:
        return raw_ts
    converted = raw_ts // 1_000_000
    if EPOCH_MS_MIN <= converted < EPOCH_MS_MAX:
        return converted
    raise ValueError(f"invalid BULK timestamp: {raw_ts}")


class StatefulOrderBook:
    """Maintains L2 price levels for delta-stream venues (Lighter, Extended)."""

    def __init__(self) -> None:
        self.nonce: int | None = None
        self.time_ms: int = 0
        self.bids: dict[Decimal, Decimal] = {}
        self.asks: dict[Decimal, Decimal] = {}

    def reset(self) -> None:
        self.nonce = None
        self.time_ms = 0
        self.bids.clear()
        self.asks.clear()

    def apply(
        self,
        *,
        nonce: int,
        begin_nonce: int,
        time_ms: int,
        bids: list[tuple[str, str]],
        asks: list[tuple[str, str]],
        snapshot: bool,
    ) -> str:
        if nonce <= 0:
            raise ValueError("book sequence/nonce must be positive")
        if not snapshot:
            if self.nonce is None:
                return "ignored_pre_snapshot"
            if nonce <= self.nonce:
                return "ignored_stale_nonce"
            if begin_nonce != self.nonce or time_ms < self.time_ms:
                self.reset()
                return "gap"
        else:
            self.reset()

        for book_side, updates in ((self.bids, bids), (self.asks, asks)):
            for raw_px, raw_sz in updates:
                px = parse_positive_dec(raw_px, allow_zero=False)
                sz = parse_positive_dec(raw_sz, allow_zero=True)
                if sz == 0:
                    book_side.pop(px, None)
                else:
                    book_side[px] = sz
            if len(book_side) > MAX_LIGHTER_LEVELS:
                raise ValueError("reconstructed L2 depth exceeds safety bound")

        self.nonce = nonce
        self.time_ms = time_ms
        return "book"

    def top_of_book(self) -> tuple[Decimal | None, Decimal | None, Decimal | None, Decimal | None, int, int]:
        bid_px = max(self.bids) if self.bids else None
        ask_px = min(self.asks) if self.asks else None
        bid_sz = self.bids[bid_px] if bid_px is not None else None
        ask_sz = self.asks[ask_px] if ask_px is not None else None
        return bid_px, bid_sz, ask_px, ask_sz, len(self.bids), len(self.asks)


def seal_bronze_capture(
    capture_path: Path,
    *,
    campaign_id: str,
    collector_region: str,
    split_role: str,
    manifest_out: Path | None = None,
) -> dict:
    """Verify a completed raw capture and emit an immutable Bronze manifest."""
    if collector_region not in VALID_REGIONS:
        raise ValueError(f"invalid collector_region {collector_region!r}; expected {VALID_REGIONS}")
    if split_role not in VALID_ROLES:
        raise ValueError(f"invalid split_role {split_role!r}; expected {VALID_ROLES}")
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]{1,120}", campaign_id) or PRIVATE_NAME.search(campaign_id):
        raise ValueError(f"invalid or private-indicating campaign_id: {campaign_id!r}")

    resolved = reject_unsafe_path(capture_path)
    before_sha, before_bytes = digest_unchanged(resolved)

    stream_hash = sha256()
    record_count = 0
    previous_at = 0
    first_at: int | None = None
    last_at: int | None = None
    ended = False
    segment_index: int | None = None
    segment_continued: bool | None = None
    prev_segment_name: str | None = None
    precise_clock: ClockAudit | None = None
    precise_records = 0
    source_counts: Counter[str] = Counter()
    disconnect_counts: Counter[str] = Counter()
    markets_seen: set[str] = set()

    with open_capture(resolved) as raw_stream:
        for raw_line in stream_rows(raw_stream):
            row = json.loads(raw_line)
            at = row.get("available_at_ms")
            received = row.get("received_at_ms")
            if type(at) is not int or type(received) is not int or not (0 < received <= at) or at < previous_at or ended:
                raise ValueError("capture violated receipt/availability order or wrote past collector/end")
            raw_source = row.get("source", "")
            event = row.get("event", "")
            if event not in ("message", "connected", "subscribed", "disconnect", "clock_refresh", "segment_start", "segment_end", "end"):
                raise ValueError(f"unsupported public event: {event!r}")
            if (raw_source == "collector") != (event in ("segment_start", "segment_end", "end")):
                raise ValueError("invalid collector lifecycle/footer record")

            if event == "segment_start":
                payload = row.get("payload")
                if record_count != 0 or not isinstance(payload, dict) or type(payload.get("segment_index")) is not int or payload["segment_index"] < 0:
                    raise ValueError("invalid collector/segment_start header")
                segment_index = payload["segment_index"]
                prev_segment_name = payload.get("prev_segment")
                if (segment_index > 0) != bool(prev_segment_name):
                    raise ValueError("segment_start prev_segment mismatch")
            elif event == "segment_end":
                payload = row.get("payload")
                if not isinstance(payload, dict) or payload.get("segment_index") != segment_index or type(payload.get("continued")) is not bool:
                    raise ValueError("invalid collector/segment_end footer")
                segment_continued = payload["continued"]
                if segment_continued:
                    ended = True

            if "capture_timing" in row:
                if precise_clock is None:
                    precise_clock = ClockAudit(allow_segment_offset=(segment_index is not None and segment_index > 0))
                precise_clock.observe(row)
                precise_records += 1

            market = row.get("market")
            if not market and event == "message" and isinstance(row.get("payload"), dict):
                payload = row["payload"]
                if raw_source in ("hyperliquid", "hyperliquid_snapshot") and isinstance(payload.get("data"), dict):
                    market = payload["data"].get("coin")
                elif raw_source in ("arcus", "arcus_trades"):
                    market = payload.get("id")
                elif raw_source in ("arcus_snapshot", "bulk_snapshot"):
                    market = payload.get("market")
                elif raw_source == "bulk" and isinstance(payload.get("data"), dict):
                    market = payload["data"].get("book", {}).get("symbol")
                elif raw_source in ("lighter", "lighter_robinhood", "lighter_trades", "lighter_robinhood_trades"):
                    chan = str(payload.get("channel", ""))
                    if ":" in chan or "/" in chan:
                        market = re.split(r"[:/]", chan, maxsplit=1)[1]

            venue, domain = classify_venue_domain(raw_source, market)
            if venue != "collector":
                source_counts[f"{venue}:{domain}:{raw_source}"] += 1
                if market:
                    markets_seen.add(f"{venue}:{domain}:{market}")
            if event == "disconnect":
                disconnect_counts[raw_source] += 1

            if first_at is None:
                first_at = at
            last_at = at
            previous_at = at
            if event == "end":
                if segment_index is not None and segment_continued is not False:
                    raise ValueError("rolling terminal segment lacked segment_end(continued=false) before end")
                ended = True
            record_count += 1
            stream_hash.update(raw_line)

    after_sha, after_bytes = digest_unchanged(resolved)
    if (before_sha, before_bytes) != (after_sha, after_bytes):
        raise ValueError("capture mutated while sealing")
    if not ended or record_count <= 1 or first_at is None or last_at is None:
        raise ValueError("capture missing valid publications or collector/end footer")

    is_rolling_segment = segment_index is not None
    span_ms = last_at - first_at
    # An individual rolling segment never certifies a 10h validation unit on its own;
    # contiguous chains are certified via seal_bronze_segment_chain.
    meets_ten_hour = (not is_rolling_segment) and (span_ms >= TEN_HOUR_MS)
    if is_rolling_segment:
        coverage_class = "rolling_segment"
    elif meets_ten_hour:
        coverage_class = "ten_hour_candidate"
    else:
        coverage_class = "diagnostic_short_window"

    manifest = {
        "schema_version": 1,
        "artifact_kind": "bronze_public_segment" if is_rolling_segment else "bronze_public_capture",
        "campaign_id": campaign_id,
        "collector_region": collector_region,
        "split_role": split_role,
        "capture_filename": resolved.name,
        "capture_sha256": before_sha,
        "capture_bytes": before_bytes,
        "stream_sha256": stream_hash.hexdigest(),
        "records": record_count,
        "precise_timing_records": precise_records,
        "capture_run_id": precise_clock.run_id if precise_clock is not None else None,
        "first_sequence": precise_clock.first_sequence if precise_clock is not None else None,
        "last_sequence": precise_clock.sequence if precise_clock is not None else None,
        "segment_index": segment_index,
        "segment_continued": segment_continued,
        "prev_segment": prev_segment_name,
        "first_available_ms": first_at,
        "last_available_ms": last_at,
        "span_ms": span_ms,
        "meets_ten_hour_validation_unit": meets_ten_hour,
        "coverage_class": coverage_class,
        "stream_record_counts": dict(sorted(source_counts.items())),
        "disconnect_counts": dict(sorted(disconnect_counts.items())),
        "markets_observed": sorted(markets_seen),
    }
    if manifest_out is not None:
        write_immutable_json(manifest_out, manifest)
    return manifest


def seal_bronze_segment_chain(
    segment_paths: list[Path],
    *,
    campaign_id: str,
    collector_region: str,
    split_role: str,
    manifest_out: Path | None = None,
) -> dict:
    """Verify an unbroken in-process rolling segment chain (e.g. 10 x 1h -> 10h unit)."""
    if not segment_paths:
        raise ValueError("segment_paths must not be empty")
    if collector_region not in VALID_REGIONS:
        raise ValueError(f"invalid collector_region {collector_region!r}")
    if split_role not in VALID_ROLES:
        raise ValueError(f"invalid split_role {split_role!r}")
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]{1,120}", campaign_id) or PRIVATE_NAME.search(campaign_id):
        raise ValueError(f"invalid or private-indicating campaign_id: {campaign_id!r}")

    seg_manifests = []
    expected_run_id: str | None = None
    prev_last_seq: int | None = None
    prev_last_at: int | None = None
    prev_filename: str | None = None
    total_records = 0
    total_bytes = 0
    source_counts: Counter[str] = Counter()
    disconnect_counts: Counter[str] = Counter()
    markets_seen: set[str] = set()

    for expected_idx, seg_path in enumerate(segment_paths):
        seg_meta = seal_bronze_capture(
            seg_path,
            campaign_id=campaign_id,
            collector_region=collector_region,
            split_role=split_role,
        )
        if seg_meta["artifact_kind"] != "bronze_public_segment" or seg_meta["segment_index"] != expected_idx:
            raise ValueError(f"segment {seg_path} has segment_index={seg_meta.get('segment_index')}, expected {expected_idx}")
        run_id = seg_meta["capture_run_id"]
        first_seq = seg_meta["first_sequence"]
        last_seq = seg_meta["last_sequence"]
        if not run_id or first_seq is None or last_seq is None:
            raise ValueError(f"segment {seg_path} lacks precise capture_timing run_id/sequence")
        if expected_idx == 0:
            if first_seq != 1:
                raise ValueError(f"segment 0 must start at sequence 1, got {first_seq}")
            expected_run_id = run_id
        else:
            if run_id != expected_run_id:
                raise ValueError(f"run_id changed across segments ({expected_run_id} -> {run_id}); state reset forbidden")
            if first_seq != (prev_last_seq or 0) + 1:
                raise ValueError(f"sequence gap between segments {expected_idx - 1} and {expected_idx}: {prev_last_seq} -> {first_seq}")
            if seg_meta["prev_segment"] != prev_filename:
                raise ValueError(f"prev_segment pointer mismatch on segment {expected_idx}")
            if seg_meta["first_available_ms"] < (prev_last_at or 0):
                raise ValueError(f"available_at_ms regressed across segment boundary at {expected_idx}")
        is_last = expected_idx == len(segment_paths) - 1
        if not is_last and seg_meta["segment_continued"] is not True:
            raise ValueError(f"non-final segment {expected_idx} was already marked terminal")
        if is_last and seg_meta["segment_continued"] is not False:
            raise ValueError(f"final segment {expected_idx} lacks terminal collector/end")

        prev_last_seq = last_seq
        prev_last_at = seg_meta["last_available_ms"]
        prev_filename = seg_meta["capture_filename"]
        total_records += seg_meta["records"]
        total_bytes += seg_meta["capture_bytes"]
        source_counts.update(seg_meta["stream_record_counts"])
        disconnect_counts.update(seg_meta["disconnect_counts"])
        markets_seen.update(seg_meta["markets_observed"])
        seg_manifests.append({
            "segment_index": expected_idx,
            "capture_filename": seg_meta["capture_filename"],
            "capture_sha256": seg_meta["capture_sha256"],
            "capture_bytes": seg_meta["capture_bytes"],
            "records": seg_meta["records"],
            "first_sequence": first_seq,
            "last_sequence": last_seq,
            "first_available_ms": seg_meta["first_available_ms"],
            "last_available_ms": seg_meta["last_available_ms"],
        })

    first_ms = seg_manifests[0]["first_available_ms"]
    last_ms = seg_manifests[-1]["last_available_ms"]
    span_ms = last_ms - first_ms
    chain_manifest = {
        "schema_version": 1,
        "artifact_kind": "bronze_public_segment_chain",
        "campaign_id": campaign_id,
        "collector_region": collector_region,
        "split_role": split_role,
        "capture_run_id": expected_run_id,
        "segment_count": len(seg_manifests),
        "segments": seg_manifests,
        "total_records": total_records,
        "total_capture_bytes": total_bytes,
        "first_sequence": seg_manifests[0]["first_sequence"],
        "last_sequence": seg_manifests[-1]["last_sequence"],
        "first_available_ms": first_ms,
        "last_available_ms": last_ms,
        "span_ms": span_ms,
        "meets_ten_hour_validation_unit": span_ms >= TEN_HOUR_MS,
        "coverage_class": "ten_hour_candidate" if span_ms >= TEN_HOUR_MS else "diagnostic_short_window",
        "stream_record_counts": dict(sorted(source_counts.items())),
        "disconnect_counts": dict(sorted(disconnect_counts.items())),
        "markets_observed": sorted(markets_seen),
    }
    if manifest_out is not None:
        write_immutable_json(manifest_out, chain_manifest)
    return chain_manifest


def evict_local_capture(
    capture_path: Path,
    *,
    manifest_path: Path,
    remote_receipt_path: Path,
) -> dict:
    """Safely evict a multi-GB local capture only after Drive SHA-256 verification."""
    resolved_capture = reject_unsafe_path(capture_path)
    resolved_manifest = reject_unsafe_path(manifest_path)
    resolved_receipt = reject_unsafe_path(remote_receipt_path)

    manifest = json.loads(resolved_manifest.read_text(encoding="utf-8"))
    receipt = json.loads(resolved_receipt.read_text(encoding="utf-8"))

    local_sha, local_bytes = digest_unchanged(resolved_capture)
    if local_sha != manifest.get("capture_sha256") or local_bytes != manifest.get("capture_bytes"):
        raise ValueError("local capture does not match sealed Bronze manifest; refusing eviction")

    remote_sha = receipt.get("sha256")
    remote_bytes = receipt.get("bytes")
    remote_uri = receipt.get("remote_uri", "")
    if (
        remote_sha != local_sha
        or remote_bytes != local_bytes
        or not isinstance(remote_uri, str)
        or not remote_uri.startswith(("gdrive://", "rclone://"))
    ):
        raise ValueError("remote Drive receipt failed SHA-256, size, or URI verification; refusing eviction")

    tombstone_path = resolved_capture.with_name(f"{resolved_capture.name}.evicted.json")
    tombstone = {
        "schema_version": 1,
        "campaign_id": manifest["campaign_id"],
        "evicted_filename": resolved_capture.name,
        "capture_sha256": local_sha,
        "capture_bytes": local_bytes,
        "manifest_path": resolved_manifest.name,
        "remote_uri": remote_uri,
        "evicted_at_utc": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
    }
    try:
        write_immutable_json(tombstone_path, tombstone)
    except FileExistsError:
        # A previous eviction may have recorded the tombstone before unlinking.
        existing = json.loads(reject_unsafe_path(tombstone_path).read_text(encoding="utf-8"))
        if not isinstance(existing, dict):
            raise ValueError("invalid eviction tombstone; refusing eviction")
        recorded_at = existing.get("evicted_at_utc")
        if not isinstance(recorded_at, str) or not recorded_at:
            raise ValueError("invalid eviction tombstone timestamp; refusing eviction")
        if existing != {**tombstone, "evicted_at_utc": recorded_at}:
            raise ValueError("conflicting eviction tombstone; refusing eviction")
        tombstone = existing
    resolved_capture.unlink(missing_ok=True)
    return tombstone


def _iter_source_lines(input_path: Path):
    resolved = reject_unsafe_path(input_path, allow_directory=True)
    if resolved.is_dir():
        yield from archive_records(resolved)
        return
    if resolved.suffix == ".json":
        doc = json.loads(resolved.read_text(encoding="utf-8"))
        if doc.get("artifact_kind") != "bronze_public_segment_chain":
            raise ValueError(f"unsupported JSON input artifact_kind: {doc.get('artifact_kind')!r}")
        parent = resolved.parent
        for entry in doc["segments"]:
            seg_file = reject_unsafe_path(parent / entry["capture_filename"])
            sha, size = digest_unchanged(seg_file)
            if sha != entry["capture_sha256"] or size != entry["capture_bytes"]:
                raise ValueError(f"segment {seg_file} digest/size mismatch against chain manifest")
            with open_capture(seg_file) as handle:
                yield from stream_rows(handle)
        return
    with open_capture(resolved) as handle:
        yield from stream_rows(handle)


def _emit_bbo_row(
    emit,
    *,
    campaign_id: str,
    collector_region: str,
    venue: str,
    domain: str,
    raw_source: str,
    market: str,
    transport: str,
    sequence: int,
    available_at_ms: int,
    received_at_ms: int,
    available_wall_ns: int | None,
    receipt_wall_ns: int | None,
    source_time_ms: int | None,
    raw_source_time: int | None,
    venue_seq: int | None,
    bid_px: Decimal | None,
    bid_sz: Decimal | None,
    ask_px: Decimal | None,
    ask_sz: Decimal | None,
    depth_levels_bid: int,
    depth_levels_ask: int,
) -> None:
    tradable = (
        bid_px is not None
        and ask_px is not None
        and bid_sz is not None
        and ask_sz is not None
        and bid_px > 0
        and ask_px > bid_px
        and bid_sz > 0
        and ask_sz > 0
    )
    bid_f = float(bid_px) if bid_px is not None else None
    ask_f = float(ask_px) if ask_px is not None else None
    mid_f = ((bid_f + ask_f) / 2.0) if (tradable and bid_f is not None and ask_f is not None) else None
    spread_bps = (
        ((ask_f - bid_f) / mid_f) * 10_000.0
        if (tradable and mid_f and bid_f is not None and ask_f is not None)
        else None
    )
    emit(
        "bbo_quotes",
        {
            "campaign_id": campaign_id,
            "collector_region": collector_region,
            "utc_date": utc_date_str(available_at_ms),
            "venue": venue,
            "domain": domain,
            "raw_source": raw_source,
            "market": market,
            "transport": transport,
            "sequence": sequence,
            "available_at_ms": available_at_ms,
            "received_at_ms": received_at_ms,
            "available_wall_ns": available_wall_ns,
            "receipt_wall_ns": receipt_wall_ns,
            "source_time_ms": source_time_ms,
            "raw_source_time": raw_source_time,
            "venue_seq": venue_seq,
            "bid_px": format_dec(bid_px) if bid_px is not None else None,
            "bid_sz": format_dec(bid_sz) if bid_sz is not None else None,
            "ask_px": format_dec(ask_px) if ask_px is not None else None,
            "ask_sz": format_dec(ask_sz) if ask_sz is not None else None,
            "bid_px_f64": bid_f,
            "ask_px_f64": ask_f,
            "mid_px_f64": mid_f,
            "spread_bps_f64": spread_bps,
            "tradable": tradable,
            "depth_levels_bid": depth_levels_bid,
            "depth_levels_ask": depth_levels_ask,
        },
    )


def normalize_capture(
    input_path: Path,
    output_dir: Path,
    *,
    campaign_id: str,
    collector_region: str,
    precise_times: bool = False,
    withheld_arcus_trade_markets: frozenset[str] = frozenset(),
) -> dict:
    """Normalize a sealed capture or partitioned archive into 4 receipt-causal Silver tables."""
    if collector_region not in VALID_REGIONS:
        raise ValueError(f"invalid collector_region: {collector_region!r}")
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]{1,120}", campaign_id) or PRIVATE_NAME.search(campaign_id):
        raise ValueError(f"invalid or private-indicating campaign_id: {campaign_id!r}")
    if output_dir.exists():
        raise FileExistsError(f"Silver staging directory already exists: {output_dir}")
    output_dir.mkdir(parents=True, mode=0o700)

    table_counts = {name: 0 for name in TABLES}
    table_hashes = {name: sha256() for name in TABLES}
    books: dict[tuple[str, str], StatefulOrderBook] = {}
    trade_online_since: dict[tuple[str, str], int | None] = {}
    trade_seen: dict[tuple[str, str], dict] = {}
    withheld_messages = {market: 0 for market in sorted(withheld_arcus_trade_markets)}
    ended = False
    previous_at = 0
    clock_audit = ClockAudit() if precise_times else None

    with ExitStack() as stack:
        writers = {}
        for name in TABLES:
            file_path = output_dir / f"{name}.ndjson.gz"
            raw_file = stack.enter_context(file_path.open("xb"))
            gz = stack.enter_context(gzip.GzipFile(filename="", fileobj=raw_file, mode="wb", mtime=0, compresslevel=3))
            writers[name] = gz

        def emit(table: str, record: dict) -> None:
            encoded = (json.dumps(record, separators=(",", ":"), sort_keys=True, allow_nan=False) + "\n").encode("utf-8")
            writers[table].write(encoded)
            table_counts[table] += 1
            table_hashes[table].update(encoded)

        for seq, raw_line in enumerate(_iter_source_lines(input_path)):
            row = json.loads(raw_line)
            raw_source = row["source"]
            event = row["event"]
            received_ms = row["received_at_ms"]
            avail_ms = (
                clock_audit.observe(row)
                if (clock_audit is not None and "capture_timing" in row)
                else (precise_available_ms(row) if (precise_times and "capture_timing" in row) else row["available_at_ms"])
            )
            if type(avail_ms) is not int or type(received_ms) is not int or received_ms <= 0 or avail_ms < previous_at or ended:
                raise ValueError("capture receipt/availability invariant violated during normalization")
            previous_at = avail_ms

            timing = row.get("capture_timing") or {}
            avail_wall_ns = timing.get("available_wall_ns")
            receipt_wall_ns = timing.get("receipt_wall_ns")
            conn_epoch = timing.get("connection_epoch")
            req_sent_ms = row.get("request_sent_at_ms")
            payload = row.get("payload")
            market = row.get("market")

            # Keep the original sequence and audit every receipt clock. A projected
            # market has neither an online trades subscription nor usable prints.
            if withheld_messages and raw_source == "arcus_trades" and event in ("subscribed", "message"):
                envelope_market = payload.get("id") if isinstance(payload, dict) else None
                if market in withheld_messages or envelope_market in withheld_messages:
                    if (not envelope_market or (market and market != envelope_market)
                            or payload.get("channel") != "trades"):
                        raise ValueError("ambiguous Arcus market in withheld trade stream")
                    withheld_messages[envelope_market] += 1
                    continue

            if raw_source == "collector" and event in ("segment_start", "segment_end"):
                emit(
                    "feed_lifecycle",
                    {
                        "campaign_id": campaign_id,
                        "collector_region": collector_region,
                        "utc_date": utc_date_str(avail_ms),
                        "venue": "collector",
                        "domain": "session",
                        "raw_source": "collector",
                        "market": None,
                        "sequence": seq,
                        "available_at_ms": avail_ms,
                        "received_at_ms": received_ms,
                        "request_sent_at_ms": None,
                        "connection_epoch": conn_epoch,
                        "event": event,
                        "detail": str(payload.get("segment_index")) if isinstance(payload, dict) else None,
                    },
                )
                continue

            if raw_source == "collector" and event == "end":
                ended = True
                emit(
                    "feed_lifecycle",
                    {
                        "campaign_id": campaign_id,
                        "collector_region": collector_region,
                        "utc_date": utc_date_str(avail_ms),
                        "venue": "collector",
                        "domain": "session",
                        "raw_source": "collector",
                        "market": None,
                        "sequence": seq,
                        "available_at_ms": avail_ms,
                        "received_at_ms": received_ms,
                        "request_sent_at_ms": None,
                        "connection_epoch": conn_epoch,
                        "event": "end",
                        "detail": None,
                    },
                )
                continue

            if event in ("connected", "disconnect", "clock_refresh", "subscribed"):
                sub_market = market
                if event == "subscribed" and isinstance(payload, dict):
                    sub_market = sub_market or payload.get("coin") or payload.get("id")
                venue, domain = classify_venue_domain(raw_source, sub_market)
                if event in ("disconnect", "clock_refresh"):
                    for key in [k for k in books if k[0] == raw_source and (sub_market is None or k[1] == sub_market)]:
                        books[key].reset()
                    for key in [k for k in trade_online_since if k[0] == raw_source and (sub_market is None or k[1] == sub_market)]:
                        trade_online_since[key] = None
                if raw_source == "hyperliquid_trades" and event == "subscribed" and isinstance(payload, dict):
                    if payload.get("type") == "trades" and sub_market:
                        trade_online_since[(raw_source, sub_market)] = received_ms
                if raw_source == "arcus_trades" and event == "subscribed" and isinstance(payload, dict):
                    if payload.get("channel") == "trades" and sub_market:
                        trade_online_since[(raw_source, sub_market)] = received_ms
                emit(
                    "feed_lifecycle",
                    {
                        "campaign_id": campaign_id,
                        "collector_region": collector_region,
                        "utc_date": utc_date_str(avail_ms),
                        "venue": venue,
                        "domain": domain,
                        "raw_source": raw_source,
                        "market": sub_market,
                        "sequence": seq,
                        "available_at_ms": avail_ms,
                        "received_at_ms": received_ms,
                        "request_sent_at_ms": req_sent_ms,
                        "connection_epoch": conn_epoch,
                        "event": event,
                        "detail": payload.get("type") if isinstance(payload, dict) else None,
                    },
                )
                continue

            if event != "message" or not isinstance(payload, dict):
                continue

            # 1. Binance USD-M BBO
            if raw_source == "binance":
                mkt = market or payload["s"]
                venue, domain = classify_venue_domain(raw_source, mkt)
                bid_px = parse_positive_dec(payload["b"])
                bid_sz = parse_positive_dec(payload["B"])
                ask_px = parse_positive_dec(payload["a"])
                ask_sz = parse_positive_dec(payload["A"])
                src_ms = payload.get("T") or payload.get("E")
                _emit_bbo_row(
                    emit,
                    campaign_id=campaign_id,
                    collector_region=collector_region,
                    venue=venue,
                    domain=domain,
                    raw_source=raw_source,
                    market=mkt,
                    transport="ws",
                    sequence=seq,
                    available_at_ms=avail_ms,
                    received_at_ms=received_ms,
                    available_wall_ns=avail_wall_ns,
                    receipt_wall_ns=receipt_wall_ns,
                    source_time_ms=int(src_ms) if src_ms is not None else None,
                    raw_source_time=int(payload["E"]) if payload.get("E") is not None else None,
                    venue_seq=int(payload["u"]) if payload.get("u") is not None else None,
                    bid_px=bid_px,
                    bid_sz=bid_sz,
                    ask_px=ask_px,
                    ask_sz=ask_sz,
                    depth_levels_bid=1,
                    depth_levels_ask=1,
                )

            # 2. Coinbase Spot Ticker
            elif raw_source == "coinbase":
                if payload.get("type") != "ticker":
                    continue
                mkt = payload["product_id"]
                venue, domain = classify_venue_domain(raw_source, mkt)
                bid_px = parse_positive_dec(payload["best_bid"])
                ask_px = parse_positive_dec(payload["best_ask"])
                bid_sz = parse_positive_dec(payload.get("best_bid_size", "1"))
                ask_sz = parse_positive_dec(payload.get("best_ask_size", "1"))
                src_ms = parse_rfc3339_ms(payload["time"]) if payload.get("time") else None
                _emit_bbo_row(
                    emit,
                    campaign_id=campaign_id,
                    collector_region=collector_region,
                    venue=venue,
                    domain=domain,
                    raw_source=raw_source,
                    market=mkt,
                    transport="ws",
                    sequence=seq,
                    available_at_ms=avail_ms,
                    received_at_ms=received_ms,
                    available_wall_ns=avail_wall_ns,
                    receipt_wall_ns=receipt_wall_ns,
                    source_time_ms=src_ms,
                    raw_source_time=src_ms,
                    venue_seq=int(payload["sequence"]) if payload.get("sequence") is not None else None,
                    bid_px=bid_px,
                    bid_sz=bid_sz,
                    ask_px=ask_px,
                    ask_sz=ask_sz,
                    depth_levels_bid=1,
                    depth_levels_ask=1,
                )

            # 3. Arcus & BULK (WebSocket + REST Snapshots)
            elif raw_source in ("arcus", "arcus_snapshot", "bulk", "bulk_snapshot"):
                is_snap = raw_source.endswith("_snapshot")
                transport = "rest_snapshot" if is_snap else "ws"
                if raw_source == "arcus":
                    if payload.get("channel") != "l2Orderbook" or payload.get("type") not in ("subscribed", "channel_data"):
                        continue
                    mkt = payload["id"]
                    book_body = payload["contents"]
                elif raw_source == "arcus_snapshot":
                    mkt = payload["market"]
                    book_body = payload["data"]
                elif raw_source == "bulk":
                    if payload.get("type") != "l2Snapshot":
                        continue
                    book_body = payload["data"]["book"]
                    mkt = book_body["symbol"]
                else:
                    mkt = payload["market"]
                    book_body = payload["data"]
                    if book_body.get("symbol") != mkt:
                        continue

                venue, domain = classify_venue_domain(raw_source, mkt)
                if venue == "arcus":
                    raw_ts = int(book_body["timestamp"])
                    src_ms = raw_ts // 1_000
                    bids = [( parse_positive_dec(p), parse_positive_dec(s) ) for p, s in book_body.get("bids", [])]
                    asks = [( parse_positive_dec(p), parse_positive_dec(s) ) for p, s in book_body.get("asks", [])]
                else:
                    if book_body.get("updateType") != "snapshot":
                        continue
                    raw_ts = int(book_body["timestamp"])
                    src_ms = bulk_timestamp_ms(raw_ts)
                    lvls = book_body.get("levels", [[], []])
                    bids = [(parse_positive_dec(item["px"]), parse_positive_dec(item["sz"])) for item in lvls[0]]
                    asks = [(parse_positive_dec(item["px"]), parse_positive_dec(item["sz"])) for item in lvls[1]]

                bid_px, bid_sz = bids[0] if bids else (None, None)
                ask_px, ask_sz = asks[0] if asks else (None, None)
                _emit_bbo_row(
                    emit,
                    campaign_id=campaign_id,
                    collector_region=collector_region,
                    venue=venue,
                    domain=domain,
                    raw_source=raw_source,
                    market=mkt,
                    transport=transport,
                    sequence=seq,
                    available_at_ms=avail_ms,
                    received_at_ms=received_ms,
                    available_wall_ns=avail_wall_ns,
                    receipt_wall_ns=receipt_wall_ns,
                    source_time_ms=src_ms,
                    raw_source_time=raw_ts,
                    venue_seq=None,
                    bid_px=bid_px,
                    bid_sz=bid_sz,
                    ask_px=ask_px,
                    ask_sz=ask_sz,
                    depth_levels_bid=len(bids),
                    depth_levels_ask=len(asks),
                )

            # 4. Lighter Core & Lighter Robinhood Books + Market Stats
            elif raw_source in ("lighter", "lighter_robinhood"):
                chan = str(payload.get("channel", ""))
                if ":" not in chan and "/" not in chan:
                    continue
                _, mkt_id = re.split(r"[:/]", chan, maxsplit=1)
                venue, domain = classify_venue_domain(raw_source, mkt_id)
                msg_type = payload.get("type", "")
                if req_sent_ms is not None and msg_type in ("subscribed/order_book", "subscribed/market_stats"):
                    emit(
                        "feed_lifecycle",
                        {
                            "campaign_id": campaign_id,
                            "collector_region": collector_region,
                            "utc_date": utc_date_str(avail_ms),
                            "venue": venue,
                            "domain": domain,
                            "raw_source": raw_source,
                            "market": mkt_id,
                            "sequence": seq,
                            "available_at_ms": avail_ms,
                            "received_at_ms": received_ms,
                            "request_sent_at_ms": req_sent_ms,
                            "connection_epoch": conn_epoch,
                            "event": "subscribed",
                            "detail": msg_type,
                        },
                    )
                if msg_type in ("subscribed/order_book", "update/order_book"):
                    ob = payload["order_book"]
                    raw_us = int(ob["last_updated_at"])
                    src_ms = raw_us // 1_000
                    state = books.setdefault((raw_source, mkt_id), StatefulOrderBook())
                    outcome = state.apply(
                        nonce=int(ob["nonce"]),
                        begin_nonce=int(ob.get("begin_nonce", 0)),
                        time_ms=src_ms,
                        bids=[(lvl["price"], lvl["size"]) for lvl in ob.get("bids", [])],
                        asks=[(lvl["price"], lvl["size"]) for lvl in ob.get("asks", [])],
                        snapshot=(msg_type == "subscribed/order_book"),
                    )
                    if outcome == "gap":
                        emit(
                            "feed_lifecycle",
                            {
                                "campaign_id": campaign_id,
                                "collector_region": collector_region,
                                "utc_date": utc_date_str(avail_ms),
                                "venue": venue,
                                "domain": domain,
                                "raw_source": raw_source,
                                "market": mkt_id,
                                "sequence": seq,
                                "available_at_ms": avail_ms,
                                "received_at_ms": received_ms,
                                "request_sent_at_ms": None,
                                "connection_epoch": conn_epoch,
                                "event": "sequence_gap",
                                "detail": f"lighter_nonce:{ob.get('begin_nonce')}->{ob.get('nonce')}",
                            },
                        )
                    elif outcome == "book":
                        bid_px, bid_sz, ask_px, ask_sz, nb, na = state.top_of_book()
                        _emit_bbo_row(
                            emit,
                            campaign_id=campaign_id,
                            collector_region=collector_region,
                            venue=venue,
                            domain=domain,
                            raw_source=raw_source,
                            market=mkt_id,
                            transport="ws",
                            sequence=seq,
                            available_at_ms=avail_ms,
                            received_at_ms=received_ms,
                            available_wall_ns=avail_wall_ns,
                            receipt_wall_ns=receipt_wall_ns,
                            source_time_ms=src_ms,
                            raw_source_time=raw_us,
                            venue_seq=int(ob["nonce"]),
                            bid_px=bid_px,
                            bid_sz=bid_sz,
                            ask_px=ask_px,
                            ask_sz=ask_sz,
                            depth_levels_bid=nb,
                            depth_levels_ask=na,
                        )
                elif msg_type in ("subscribed/market_stats", "update/market_stats"):
                    stats = payload["market_stats"]
                    oracle_dec = parse_positive_dec(stats["index_price"])
                    mark_dec = parse_positive_dec(stats["mark_price"]) if stats.get("mark_price") else None
                    # Lighter current_funding_rate is reported in %/hour (e.g. "0.0012" = 0.0012%/h = 0.12 bps/h).
                    rate_dec = Decimal(str(stats["current_funding_rate"])) if stats.get("current_funding_rate") is not None else None
                    rate_bps = rate_dec * Decimal("100") if rate_dec is not None else None
                    pub_ms = int(payload["timestamp"]) if payload.get("timestamp") is not None else None
                    emit(
                        "funding_and_ctx",
                        {
                            "campaign_id": campaign_id,
                            "collector_region": collector_region,
                            "utc_date": utc_date_str(avail_ms),
                            "venue": venue,
                            "domain": domain,
                            "raw_source": raw_source,
                            "market": stats.get("symbol") or mkt_id,
                            "ctx_kind": "market_stats",
                            "sequence": seq,
                            "available_at_ms": avail_ms,
                            "received_at_ms": received_ms,
                            "source_time_ms": pub_ms,
                            "funding_slot_ms": stats.get("funding_timestamp"),
                            "oracle_px": format_dec(oracle_dec),
                            "mark_px": format_dec(mark_dec) if mark_dec is not None else None,
                            "funding_rate_raw": format_dec(rate_dec) if rate_dec is not None else None,
                            "funding_rate_bps": format_dec(rate_bps) if rate_bps is not None else None,
                            "oracle_px_f64": float(oracle_dec),
                            "mark_px_f64": float(mark_dec) if mark_dec is not None else None,
                            "funding_rate_bps_f64": float(rate_bps) if rate_bps is not None else None,
                        },
                    )

            # 5. Hyperliquid Core, Entropy (io:*), and TradeXYZ (xyz:*) Books + Contexts + Hourly Funding
            elif raw_source in ("hyperliquid", "hyperliquid_snapshot"):
                is_snap = raw_source == "hyperliquid_snapshot"
                data = payload["data"]
                mkt = data["coin"]
                venue, domain = classify_venue_domain(raw_source, mkt)
                chan = payload.get("channel", "")
                if chan == "l2Book":
                    lvls = data["levels"]
                    bids = [(parse_positive_dec(item["px"]), parse_positive_dec(item["sz"])) for item in lvls[0]]
                    asks = [(parse_positive_dec(item["px"]), parse_positive_dec(item["sz"])) for item in lvls[1]]
                    bid_px, bid_sz = bids[0] if bids else (None, None)
                    ask_px, ask_sz = asks[0] if asks else (None, None)
                    src_ms = int(data["time"])
                    _emit_bbo_row(
                        emit,
                        campaign_id=campaign_id,
                        collector_region=collector_region,
                        venue=venue,
                        domain=domain,
                        raw_source=raw_source,
                        market=mkt,
                        transport="rest_snapshot" if is_snap else "ws",
                        sequence=seq,
                        available_at_ms=avail_ms,
                        received_at_ms=received_ms,
                        available_wall_ns=avail_wall_ns,
                        receipt_wall_ns=receipt_wall_ns,
                        source_time_ms=src_ms,
                        raw_source_time=src_ms,
                        venue_seq=None,
                        bid_px=bid_px,
                        bid_sz=bid_sz,
                        ask_px=ask_px,
                        ask_sz=ask_sz,
                        depth_levels_bid=len(bids),
                        depth_levels_ask=len(asks),
                    )
                elif chan == "activeAssetCtx" and not is_snap:
                    ctx = data["ctx"]
                    oracle_dec = parse_positive_dec(ctx["oraclePx"])
                    mark_dec = parse_positive_dec(ctx["markPx"]) if ctx.get("markPx") else None
                    rate_dec = Decimal(str(ctx["funding"])) if ctx.get("funding") is not None else None
                    rate_bps = rate_dec * Decimal("10000") if rate_dec is not None else None
                    emit(
                        "funding_and_ctx",
                        {
                            "campaign_id": campaign_id,
                            "collector_region": collector_region,
                            "utc_date": utc_date_str(avail_ms),
                            "venue": venue,
                            "domain": domain,
                            "raw_source": raw_source,
                            "market": mkt,
                            "ctx_kind": "active_asset_ctx",
                            "sequence": seq,
                            "available_at_ms": avail_ms,
                            "received_at_ms": received_ms,
                            "source_time_ms": None,
                            "funding_slot_ms": None,
                            "oracle_px": format_dec(oracle_dec),
                            "mark_px": format_dec(mark_dec) if mark_dec is not None else None,
                            "funding_rate_raw": format_dec(rate_dec) if rate_dec is not None else None,
                            "funding_rate_bps": format_dec(rate_bps) if rate_bps is not None else None,
                            "oracle_px_f64": float(oracle_dec),
                            "mark_px_f64": float(mark_dec) if mark_dec is not None else None,
                            "funding_rate_bps_f64": float(rate_bps) if rate_bps is not None else None,
                        },
                    )

            elif raw_source == "hyperliquid_funding":
                mkt = market or ""
                venue, domain = classify_venue_domain(raw_source, mkt)
                for point in payload.get("data", []):
                    stamp = int(point["time"])
                    slot = (stamp // 3_600_000) * 3_600_000
                    if point.get("coin") != mkt or not (0 <= stamp - slot <= 10_000) or stamp > received_ms:
                        raise ValueError("invalid Hyperliquid hourly funding record")
                    rate_dec = Decimal(str(point["fundingRate"]))
                    rate_bps = rate_dec * Decimal("10000")
                    emit(
                        "funding_and_ctx",
                        {
                            "campaign_id": campaign_id,
                            "collector_region": collector_region,
                            "utc_date": utc_date_str(avail_ms),
                            "venue": venue,
                            "domain": domain,
                            "raw_source": raw_source,
                            "market": mkt,
                            "ctx_kind": "hourly_funding_history",
                            "sequence": seq,
                            "available_at_ms": avail_ms,
                            "received_at_ms": received_ms,
                            "source_time_ms": stamp,
                            "funding_slot_ms": slot,
                            "oracle_px": None,
                            "mark_px": None,
                            "funding_rate_raw": format_dec(rate_dec),
                            "funding_rate_bps": format_dec(rate_bps),
                            "oracle_px_f64": None,
                            "mark_px_f64": None,
                            "funding_rate_bps_f64": float(rate_bps),
                        },
                    )

            # 6. Extended Orderbook & Index Stream
            elif raw_source == "extended":
                data = payload["data"]
                mkt = data["m"]
                venue, domain = classify_venue_domain(raw_source, mkt)
                ext_seq = int(payload["seq"])
                src_ms = int(payload["ts"])
                is_snap = payload.get("type") == "SNAPSHOT"
                qty_field = "q" if is_snap else "c"
                state = books.setdefault((raw_source, mkt), StatefulOrderBook())
                outcome = state.apply(
                    nonce=ext_seq,
                    begin_nonce=ext_seq - 1,
                    time_ms=src_ms,
                    bids=[(lvl["p"], lvl[qty_field]) for lvl in data.get("b", [])],
                    asks=[(lvl["p"], lvl[qty_field]) for lvl in data.get("a", [])],
                    snapshot=is_snap,
                )
                if outcome == "gap":
                    emit(
                        "feed_lifecycle",
                        {
                            "campaign_id": campaign_id,
                            "collector_region": collector_region,
                            "utc_date": utc_date_str(avail_ms),
                            "venue": venue,
                            "domain": domain,
                            "raw_source": raw_source,
                            "market": mkt,
                            "sequence": seq,
                            "available_at_ms": avail_ms,
                            "received_at_ms": received_ms,
                            "request_sent_at_ms": None,
                            "connection_epoch": conn_epoch,
                            "event": "sequence_gap",
                            "detail": f"extended_seq:{ext_seq}",
                        },
                    )
                elif outcome == "book":
                    bid_px, bid_sz, ask_px, ask_sz, nb, na = state.top_of_book()
                    _emit_bbo_row(
                        emit,
                        campaign_id=campaign_id,
                        collector_region=collector_region,
                        venue=venue,
                        domain=domain,
                        raw_source=raw_source,
                        market=mkt,
                        transport="ws",
                        sequence=seq,
                        available_at_ms=avail_ms,
                        received_at_ms=received_ms,
                        available_wall_ns=avail_wall_ns,
                        receipt_wall_ns=receipt_wall_ns,
                        source_time_ms=src_ms,
                        raw_source_time=src_ms,
                        venue_seq=ext_seq,
                        bid_px=bid_px,
                        bid_sz=bid_sz,
                        ask_px=ask_px,
                        ask_sz=ask_sz,
                        depth_levels_bid=nb,
                        depth_levels_ask=na,
                    )

            elif raw_source == "extended_index":
                data = payload["data"]
                mkt = data["m"]
                venue, domain = classify_venue_domain(raw_source, mkt)
                oracle_dec = parse_positive_dec(data["p"])
                emit(
                    "funding_and_ctx",
                    {
                        "campaign_id": campaign_id,
                        "collector_region": collector_region,
                        "utc_date": utc_date_str(avail_ms),
                        "venue": venue,
                        "domain": domain,
                        "raw_source": raw_source,
                        "market": mkt,
                        "ctx_kind": "index_price",
                        "sequence": seq,
                        "available_at_ms": avail_ms,
                        "received_at_ms": received_ms,
                        "source_time_ms": int(data["ts"]),
                        "funding_slot_ms": None,
                        "oracle_px": format_dec(oracle_dec),
                        "mark_px": None,
                        "funding_rate_raw": None,
                        "funding_rate_bps": None,
                        "oracle_px_f64": float(oracle_dec),
                        "mark_px_f64": None,
                        "funding_rate_bps_f64": None,
                    },
                )

            # 7. Public Trade Tapes (Arcus, Hyperliquid/Entropy/TradeXYZ, Lighter Core & Robinhood, Extended)
            elif raw_source in (
                "arcus_trades",
                "hyperliquid_trades",
                "lighter_trades",
                "lighter_robinhood_trades",
                "extended_trades",
            ):
                if raw_source == "arcus_trades":
                    mkt = market or payload.get("id") or ""
                    venue, domain = classify_venue_domain(raw_source, mkt)
                    online_since = trade_online_since.get((raw_source, mkt))
                    if online_since is None:
                        raise ValueError(f"Arcus trade batch received before subscription ack on {mkt}")
                    if payload.get("channel") != "trades" or payload.get("id") != mkt:
                        raise ValueError(f"invalid Arcus trade channel or market envelope on {mkt}")
                    seen = trade_seen.setdefault((raw_source, mkt), {})
                    for trd in payload.get("contents") or []:
                        if trd.get("marketDisplayName") != mkt:
                            continue
                        tid = str(trd["tradeId"])
                        raw_us = int(trd["timestamp"])
                        if raw_us <= 0:
                            raise ValueError("non-positive Arcus trade timestamp")
                        stamp = raw_us // 1_000
                        side = {"BUY": "buy", "SELL": "sell"}[trd["side"]]
                        px_dec = parse_positive_dec(trd["price"])
                        sz_dec = parse_positive_dec(trd["size"])
                        dedup_val = (raw_us, format_dec(px_dec), format_dec(sz_dec), side)
                        if tid in seen:
                            if seen[tid] != dedup_val:
                                raise ValueError("conflicting duplicate Arcus trade")
                            continue
                        seen[tid] = dedup_val
                        emit(
                            "public_trades",
                            {
                                "campaign_id": campaign_id,
                                "collector_region": collector_region,
                                "utc_date": utc_date_str(avail_ms),
                                "venue": venue,
                                "domain": domain,
                                "raw_source": raw_source,
                                "market": mkt,
                                "sequence": seq,
                                "available_at_ms": avail_ms,
                                "received_at_ms": received_ms,
                                "source_time_ms": stamp,
                                "trade_id": tid,
                                "aggressor_side": side,
                                "price": format_dec(px_dec),
                                "size": format_dec(sz_dec),
                                "price_f64": float(px_dec),
                                "size_f64": float(sz_dec),
                                "notional_f64": float(px_dec * sz_dec),
                                "is_zero_price_transfer": False,
                                "fee_fields_version": None,
                                "maker_fee_raw": None,
                                "taker_fee_raw": None,
                                "transaction_time_us": raw_us,
                                "block_height": None,
                            },
                        )

                elif raw_source == "hyperliquid_trades":
                    mkt = market or ""
                    venue, domain = classify_venue_domain(raw_source, mkt)
                    online_since = trade_online_since.get((raw_source, mkt))
                    if online_since is None:
                        raise ValueError(f"Hyperliquid trade batch received before subscription ack on {mkt}")
                    seen = trade_seen.setdefault((raw_source, mkt), {})
                    for trd in payload.get("data", []):
                        if trd.get("coin") != mkt:
                            continue
                        tid = str(trd["tid"])
                        stamp = int(trd["time"])
                        side = {"B": "buy", "A": "sell"}[trd["side"]]
                        px_dec = parse_positive_dec(trd["px"])
                        sz_dec = parse_positive_dec(trd["sz"])
                        dedup_key = (stamp, tid)
                        dedup_val = (stamp, format_dec(px_dec), format_dec(sz_dec), side)
                        if dedup_key in seen:
                            if seen[dedup_key] != dedup_val:
                                raise ValueError("conflicting duplicate Hyperliquid trade")
                            continue
                        seen[dedup_key] = dedup_val
                        if stamp < online_since:
                            continue
                        emit(
                            "public_trades",
                            {
                                "campaign_id": campaign_id,
                                "collector_region": collector_region,
                                "utc_date": utc_date_str(avail_ms),
                                "venue": venue,
                                "domain": domain,
                                "raw_source": raw_source,
                                "market": mkt,
                                "sequence": seq,
                                "available_at_ms": avail_ms,
                                "received_at_ms": received_ms,
                                "source_time_ms": stamp,
                                "trade_id": tid,
                                "aggressor_side": side,
                                "price": format_dec(px_dec),
                                "size": format_dec(sz_dec),
                                "price_f64": float(px_dec),
                                "size_f64": float(sz_dec),
                                "notional_f64": float(px_dec * sz_dec),
                                "is_zero_price_transfer": False,
                                "fee_fields_version": None,
                                "maker_fee_raw": None,
                                "taker_fee_raw": None,
                                "transaction_time_us": None,
                                "block_height": None,
                            },
                        )

                elif raw_source == "extended_trades":
                    mkt = market or ""
                    venue, domain = classify_venue_domain(raw_source, mkt)
                    is_sub = int(payload["seq"]) == 1
                    if is_sub:
                        trade_online_since[(raw_source, mkt)] = received_ms
                    online_since = trade_online_since.get((raw_source, mkt))
                    if online_since is None:
                        raise ValueError(f"Extended trade received before subscription snapshot on {mkt}")
                    seen = trade_seen.setdefault((raw_source, mkt), {})
                    for trd in payload.get("data", []):
                        if trd.get("m") != mkt or trd.get("tT") != "TRADE":
                            continue
                        tid = str(trd["i"])
                        stamp = int(trd["T"])
                        side = str(trd["S"]).lower()
                        px_dec = parse_positive_dec(trd["p"])
                        sz_dec = parse_positive_dec(trd["q"])
                        dedup_val = (stamp, format_dec(px_dec), format_dec(sz_dec), side)
                        if tid in seen:
                            if seen[tid] != dedup_val:
                                raise ValueError("conflicting duplicate Extended trade")
                            continue
                        seen[tid] = dedup_val
                        if is_sub:
                            continue
                        emit(
                            "public_trades",
                            {
                                "campaign_id": campaign_id,
                                "collector_region": collector_region,
                                "utc_date": utc_date_str(avail_ms),
                                "venue": venue,
                                "domain": domain,
                                "raw_source": raw_source,
                                "market": mkt,
                                "sequence": seq,
                                "available_at_ms": avail_ms,
                                "received_at_ms": received_ms,
                                "source_time_ms": stamp,
                                "trade_id": tid,
                                "aggressor_side": side,
                                "price": format_dec(px_dec),
                                "size": format_dec(sz_dec),
                                "price_f64": float(px_dec),
                                "size_f64": float(sz_dec),
                                "notional_f64": float(px_dec * sz_dec),
                                "is_zero_price_transfer": False,
                                "fee_fields_version": None,
                                "maker_fee_raw": None,
                                "taker_fee_raw": None,
                                "transaction_time_us": None,
                                "block_height": None,
                            },
                        )

                else:
                    chan = str(payload.get("channel", "")).replace("/", ":")
                    if not chan.startswith("trade:"):
                        continue
                    mkt = chan.split(":", 1)[1]
                    venue, domain = classify_venue_domain(raw_source, mkt)
                    is_sub = payload.get("type") == "subscribed/trade"
                    if is_sub:
                        trade_online_since[(raw_source, mkt)] = received_ms
                    online_since = trade_online_since.get((raw_source, mkt))
                    if online_since is None:
                        raise ValueError(f"Lighter trade received before subscribed/trade on {mkt}")
                    seen = trade_seen.setdefault((raw_source, mkt), {})
                    fee_ver = payload.get("fee_fields_version")
                    for trd in payload.get("trades") or []:
                        if str(trd.get("market_id")) != mkt or type(trd.get("is_maker_ask")) is not bool:
                            continue
                        tid = str(trd["trade_id"])
                        stamp = int(trd["timestamp"])
                        side = "buy" if trd["is_maker_ask"] else "sell"
                        px_dec = parse_positive_dec(trd["price"], allow_zero=True)
                        sz_dec = parse_positive_dec(trd["size"], allow_zero=False)
                        dedup_val = (stamp, format_dec(px_dec), format_dec(sz_dec), side)
                        if tid in seen:
                            if seen[tid] != dedup_val:
                                raise ValueError("conflicting duplicate Lighter trade")
                            continue
                        seen[tid] = dedup_val
                        if is_sub:
                            continue
                        emit(
                            "public_trades",
                            {
                                "campaign_id": campaign_id,
                                "collector_region": collector_region,
                                "utc_date": utc_date_str(avail_ms),
                                "venue": venue,
                                "domain": domain,
                                "raw_source": raw_source,
                                "market": mkt,
                                "sequence": seq,
                                "available_at_ms": avail_ms,
                                "received_at_ms": received_ms,
                                "source_time_ms": stamp,
                                "trade_id": tid,
                                "aggressor_side": side,
                                "price": format_dec(px_dec),
                                "size": format_dec(sz_dec),
                                "price_f64": float(px_dec),
                                "size_f64": float(sz_dec),
                                "notional_f64": float(px_dec * sz_dec),
                                "is_zero_price_transfer": px_dec == 0,
                                "fee_fields_version": int(fee_ver) if fee_ver is not None else None,
                                "maker_fee_raw": int(trd["maker_fee"]) if trd.get("maker_fee") is not None else None,
                                "taker_fee_raw": int(trd["taker_fee"]) if trd.get("taker_fee") is not None else None,
                                "transaction_time_us": int(trd["transaction_time"]) if trd.get("transaction_time") is not None else None,
                                "block_height": int(trd["block_height"]) if trd.get("block_height") is not None else None,
                            },
                        )

    if not ended:
        shutil.rmtree(output_dir, ignore_errors=True)
        raise ValueError("capture stream lacked terminal collector/end footer")

    tables_meta = {}
    for name in TABLES:
        gz_path = output_dir / f"{name}.ndjson.gz"
        gz_sha, gz_bytes = digest_unchanged(gz_path)
        tables_meta[name] = {
            "file": gz_path.name,
            "rows": table_counts[name],
            "compressed_sha256": gz_sha,
            "compressed_bytes": gz_bytes,
            "uncompressed_sha256": table_hashes[name].hexdigest(),
        }

    summary = {
        "schema_version": 1,
        "artifact_kind": "silver_normalized_staging",
        "campaign_id": campaign_id,
        "collector_region": collector_region,
        "precise_times": precise_times,
        "causal_ordering_key": ["venue", "domain", "market", "available_at_ms", "sequence"],
        "tables": tables_meta,
    }
    if withheld_messages:
        summary["artifact_kind"] = "silver_partial_market_projection"
        summary["withheld_arcus_trade_messages"] = withheld_messages
    write_immutable_json(output_dir / "silver_manifest.json", summary)
    return summary


def generate_duckdb_sql(staging_dir: Path, parquet_root: Path) -> str:
    """Generate causal-sorted, Hive-partitioned Zstd Parquet compilation & view SQL."""
    s_dir = staging_dir.resolve()
    p_root = parquet_root.resolve()
    esc = lambda p: str(p).replace("'", "''")

    bbo_src = esc(s_dir / "bbo_quotes.ndjson.gz")
    trd_src = esc(s_dir / "public_trades.ndjson.gz")
    ctx_src = esc(s_dir / "funding_and_ctx.ndjson.gz")
    lfc_src = esc(s_dir / "feed_lifecycle.ndjson.gz")

    bbo_dst = esc(p_root / "bbo_quotes")
    trd_dst = esc(p_root / "public_trades")
    ctx_dst = esc(p_root / "funding_and_ctx")
    lfc_dst = esc(p_root / "feed_lifecycle")

    return f"""-- AlephTX Quantitative Market Data Lakehouse (DuckDB + Zstd Parquet)
-- Strictly ordered by receipt-causal availability: (venue, domain, market, available_at_ms, sequence)

COPY (
    SELECT
        CAST(campaign_id AS VARCHAR) AS campaign_id,
        CAST(collector_region AS VARCHAR) AS collector_region,
        CAST(utc_date AS VARCHAR) AS utc_date,
        CAST(venue AS VARCHAR) AS venue,
        CAST(domain AS VARCHAR) AS domain,
        CAST(raw_source AS VARCHAR) AS raw_source,
        CAST(market AS VARCHAR) AS market,
        CAST(transport AS VARCHAR) AS transport,
        CAST(sequence AS BIGINT) AS sequence,
        CAST(available_at_ms AS BIGINT) AS available_at_ms,
        CAST(received_at_ms AS BIGINT) AS received_at_ms,
        CAST(available_wall_ns AS BIGINT) AS available_wall_ns,
        CAST(receipt_wall_ns AS BIGINT) AS receipt_wall_ns,
        CAST(source_time_ms AS BIGINT) AS source_time_ms,
        CAST(raw_source_time AS BIGINT) AS raw_source_time,
        CAST(venue_seq AS UBIGINT) AS venue_seq,
        CAST(bid_px AS DECIMAL(38, 12)) AS bid_px,
        CAST(bid_sz AS DECIMAL(38, 12)) AS bid_sz,
        CAST(ask_px AS DECIMAL(38, 12)) AS ask_px,
        CAST(ask_sz AS DECIMAL(38, 12)) AS ask_sz,
        CAST(bid_px_f64 AS DOUBLE) AS bid_px_f64,
        CAST(ask_px_f64 AS DOUBLE) AS ask_px_f64,
        CAST(mid_px_f64 AS DOUBLE) AS mid_px_f64,
        CAST(spread_bps_f64 AS DOUBLE) AS spread_bps_f64,
        CAST(tradable AS BOOLEAN) AS tradable,
        CAST(depth_levels_bid AS INTEGER) AS depth_levels_bid,
        CAST(depth_levels_ask AS INTEGER) AS depth_levels_ask
    FROM read_json('{bbo_src}', format='newline_delimited', columns={{
        campaign_id: 'VARCHAR', collector_region: 'VARCHAR', utc_date: 'VARCHAR',
        venue: 'VARCHAR', domain: 'VARCHAR', raw_source: 'VARCHAR', market: 'VARCHAR',
        transport: 'VARCHAR', sequence: 'BIGINT', available_at_ms: 'BIGINT',
        received_at_ms: 'BIGINT', available_wall_ns: 'BIGINT', receipt_wall_ns: 'BIGINT',
        source_time_ms: 'BIGINT', raw_source_time: 'BIGINT', venue_seq: 'UBIGINT',
        bid_px: 'VARCHAR', bid_sz: 'VARCHAR', ask_px: 'VARCHAR', ask_sz: 'VARCHAR',
        bid_px_f64: 'DOUBLE', ask_px_f64: 'DOUBLE', mid_px_f64: 'DOUBLE',
        spread_bps_f64: 'DOUBLE', tradable: 'BOOLEAN',
        depth_levels_bid: 'INTEGER', depth_levels_ask: 'INTEGER'
    }})
    ORDER BY venue, domain, market, available_at_ms, sequence
) TO '{bbo_dst}' (
    FORMAT PARQUET,
    PARTITION_BY (collector_region, venue, utc_date, campaign_id),
    OVERWRITE_OR_IGNORE true,
    COMPRESSION ZSTD,
    ROW_GROUP_SIZE 250000
);

COPY (
    SELECT
        CAST(campaign_id AS VARCHAR) AS campaign_id,
        CAST(collector_region AS VARCHAR) AS collector_region,
        CAST(utc_date AS VARCHAR) AS utc_date,
        CAST(venue AS VARCHAR) AS venue,
        CAST(domain AS VARCHAR) AS domain,
        CAST(raw_source AS VARCHAR) AS raw_source,
        CAST(market AS VARCHAR) AS market,
        CAST(sequence AS BIGINT) AS sequence,
        CAST(available_at_ms AS BIGINT) AS available_at_ms,
        CAST(received_at_ms AS BIGINT) AS received_at_ms,
        CAST(source_time_ms AS BIGINT) AS source_time_ms,
        CAST(trade_id AS VARCHAR) AS trade_id,
        CAST(aggressor_side AS VARCHAR) AS aggressor_side,
        CAST(price AS DECIMAL(38, 12)) AS price,
        CAST(size AS DECIMAL(38, 12)) AS size,
        CAST(price_f64 AS DOUBLE) AS price_f64,
        CAST(size_f64 AS DOUBLE) AS size_f64,
        CAST(notional_f64 AS DOUBLE) AS notional_f64,
        CAST(is_zero_price_transfer AS BOOLEAN) AS is_zero_price_transfer,
        CAST(fee_fields_version AS INTEGER) AS fee_fields_version,
        CAST(maker_fee_raw AS BIGINT) AS maker_fee_raw,
        CAST(taker_fee_raw AS BIGINT) AS taker_fee_raw,
        CAST(transaction_time_us AS BIGINT) AS transaction_time_us,
        CAST(block_height AS BIGINT) AS block_height
    FROM read_json('{trd_src}', format='newline_delimited', columns={{
        campaign_id: 'VARCHAR', collector_region: 'VARCHAR', utc_date: 'VARCHAR',
        venue: 'VARCHAR', domain: 'VARCHAR', raw_source: 'VARCHAR', market: 'VARCHAR',
        sequence: 'BIGINT', available_at_ms: 'BIGINT', received_at_ms: 'BIGINT',
        source_time_ms: 'BIGINT', trade_id: 'VARCHAR', aggressor_side: 'VARCHAR',
        price: 'VARCHAR', size: 'VARCHAR', price_f64: 'DOUBLE', size_f64: 'DOUBLE',
        notional_f64: 'DOUBLE', is_zero_price_transfer: 'BOOLEAN',
        fee_fields_version: 'INTEGER', maker_fee_raw: 'BIGINT', taker_fee_raw: 'BIGINT',
        transaction_time_us: 'BIGINT', block_height: 'BIGINT'
    }})
    ORDER BY venue, domain, market, available_at_ms, sequence
) TO '{trd_dst}' (
    FORMAT PARQUET,
    PARTITION_BY (collector_region, venue, utc_date, campaign_id),
    OVERWRITE_OR_IGNORE true,
    COMPRESSION ZSTD,
    ROW_GROUP_SIZE 250000
);

COPY (
    SELECT
        CAST(campaign_id AS VARCHAR) AS campaign_id,
        CAST(collector_region AS VARCHAR) AS collector_region,
        CAST(utc_date AS VARCHAR) AS utc_date,
        CAST(venue AS VARCHAR) AS venue,
        CAST(domain AS VARCHAR) AS domain,
        CAST(raw_source AS VARCHAR) AS raw_source,
        CAST(market AS VARCHAR) AS market,
        CAST(ctx_kind AS VARCHAR) AS ctx_kind,
        CAST(sequence AS BIGINT) AS sequence,
        CAST(available_at_ms AS BIGINT) AS available_at_ms,
        CAST(received_at_ms AS BIGINT) AS received_at_ms,
        CAST(source_time_ms AS BIGINT) AS source_time_ms,
        CAST(funding_slot_ms AS BIGINT) AS funding_slot_ms,
        CAST(oracle_px AS DECIMAL(38, 12)) AS oracle_px,
        CAST(mark_px AS DECIMAL(38, 12)) AS mark_px,
        CAST(funding_rate_raw AS DECIMAL(38, 12)) AS funding_rate_raw,
        CAST(funding_rate_bps AS DECIMAL(38, 12)) AS funding_rate_bps,
        CAST(oracle_px_f64 AS DOUBLE) AS oracle_px_f64,
        CAST(mark_px_f64 AS DOUBLE) AS mark_px_f64,
        CAST(funding_rate_bps_f64 AS DOUBLE) AS funding_rate_bps_f64
    FROM read_json('{ctx_src}', format='newline_delimited', columns={{
        campaign_id: 'VARCHAR', collector_region: 'VARCHAR', utc_date: 'VARCHAR',
        venue: 'VARCHAR', domain: 'VARCHAR', raw_source: 'VARCHAR', market: 'VARCHAR',
        ctx_kind: 'VARCHAR', sequence: 'BIGINT', available_at_ms: 'BIGINT',
        received_at_ms: 'BIGINT', source_time_ms: 'BIGINT', funding_slot_ms: 'BIGINT',
        oracle_px: 'VARCHAR', mark_px: 'VARCHAR', funding_rate_raw: 'VARCHAR',
        funding_rate_bps: 'VARCHAR', oracle_px_f64: 'DOUBLE', mark_px_f64: 'DOUBLE',
        funding_rate_bps_f64: 'DOUBLE'
    }})
    ORDER BY venue, domain, market, available_at_ms, sequence
) TO '{ctx_dst}' (
    FORMAT PARQUET,
    PARTITION_BY (collector_region, venue, utc_date, campaign_id),
    OVERWRITE_OR_IGNORE true,
    COMPRESSION ZSTD,
    ROW_GROUP_SIZE 250000
);

COPY (
    SELECT
        CAST(campaign_id AS VARCHAR) AS campaign_id,
        CAST(collector_region AS VARCHAR) AS collector_region,
        CAST(utc_date AS VARCHAR) AS utc_date,
        CAST(venue AS VARCHAR) AS venue,
        CAST(domain AS VARCHAR) AS domain,
        CAST(raw_source AS VARCHAR) AS raw_source,
        CAST(market AS VARCHAR) AS market,
        CAST(sequence AS BIGINT) AS sequence,
        CAST(available_at_ms AS BIGINT) AS available_at_ms,
        CAST(received_at_ms AS BIGINT) AS received_at_ms,
        CAST(request_sent_at_ms AS BIGINT) AS request_sent_at_ms,
        CAST(connection_epoch AS BIGINT) AS connection_epoch,
        CAST(event AS VARCHAR) AS event,
        CAST(detail AS VARCHAR) AS detail
    FROM read_json('{lfc_src}', format='newline_delimited', columns={{
        campaign_id: 'VARCHAR', collector_region: 'VARCHAR', utc_date: 'VARCHAR',
        venue: 'VARCHAR', domain: 'VARCHAR', raw_source: 'VARCHAR', market: 'VARCHAR',
        sequence: 'BIGINT', available_at_ms: 'BIGINT', received_at_ms: 'BIGINT',
        request_sent_at_ms: 'BIGINT', connection_epoch: 'BIGINT',
        event: 'VARCHAR', detail: 'VARCHAR'
    }})
    ORDER BY venue, domain, market, available_at_ms, sequence
) TO '{lfc_dst}' (
    FORMAT PARQUET,
    PARTITION_BY (collector_region, venue, utc_date, campaign_id),
    OVERWRITE_OR_IGNORE true,
    COMPRESSION ZSTD,
    ROW_GROUP_SIZE 250000
);

CREATE OR REPLACE VIEW v_bbo_quotes AS
SELECT * FROM read_parquet('{bbo_dst}/**/*.parquet', hive_partitioning=true);

CREATE OR REPLACE VIEW v_public_trades AS
SELECT * FROM read_parquet('{trd_dst}/**/*.parquet', hive_partitioning=true);

CREATE OR REPLACE VIEW v_funding_and_ctx AS
SELECT * FROM read_parquet('{ctx_dst}/**/*.parquet', hive_partitioning=true);

CREATE OR REPLACE VIEW v_feed_lifecycle AS
SELECT * FROM read_parquet('{lfc_dst}/**/*.parquet', hive_partitioning=true);

CREATE OR REPLACE VIEW v_stream_health AS
SELECT
    campaign_id,
    collector_region,
    venue,
    domain,
    market,
    COUNT(*) FILTER (WHERE event = 'disconnect') AS disconnects,
    COUNT(*) FILTER (WHERE event = 'sequence_gap') AS sequence_gaps,
    MIN(available_at_ms) AS first_lifecycle_ms,
    MAX(available_at_ms) AS last_lifecycle_ms,
    COUNT(*) FILTER (WHERE raw_source = 'arcus_trades' AND event = 'subscribed') AS arcus_trade_subscriptions
FROM v_feed_lifecycle
GROUP BY campaign_id, collector_region, venue, domain, market;
"""


def build_duckdb_lakehouse(
    staging_dir: Path,
    parquet_root: Path,
    db_path: Path,
    *,
    sql_out: Path | None = None,
    require_duckdb: bool = False,
    allow_partial_projection: bool = False,
) -> dict:
    """Compile normalized Silver staging tables into Zstd Parquet + DuckDB catalog."""
    resolved_staging = reject_unsafe_path(staging_dir, must_be_directory=True)
    manifest = json.loads((resolved_staging / "silver_manifest.json").read_text(encoding="utf-8"))
    partial = manifest.get("artifact_kind") == "silver_partial_market_projection"
    if partial and not allow_partial_projection:
        raise ValueError("partial Silver requires --allow-partial-projection and separate Parquet and DuckDB destinations")
    if partial and not manifest.get("withheld_arcus_trade_messages"):
        raise ValueError("partial Silver manifest must identify its withheld Arcus markets")
    for name, info in manifest["tables"].items():
        sha, size = digest_unchanged(resolved_staging / info["file"])
        if sha != info["compressed_sha256"] or size != info["compressed_bytes"]:
            raise ValueError(f"Silver staging table {name} failed hash check")

    parquet_root.mkdir(parents=True, exist_ok=True)
    db_path.parent.mkdir(parents=True, exist_ok=True)

    # Filter COPY blocks to non-empty tables so DuckDB never fails on 0-row files.
    full_sql = generate_duckdb_sql(resolved_staging, parquet_root)
    blocks = [blk.strip() for blk in full_sql.split(";\n") if blk.strip()]
    executable_blocks = []
    for blk in blocks:
        skip = False
        for name, info in manifest["tables"].items():
            if info["rows"] == 0 and (f"{name}.ndjson.gz" in blk or f"v_{name}" in blk or (name == "feed_lifecycle" and "v_stream_health" in blk)):
                skip = True
                break
        if not skip:
            executable_blocks.append(blk + ";")
    exec_sql = "\n\n".join(executable_blocks) + "\n"

    target_sql = sql_out or (parquet_root / f"build_{manifest['campaign_id']}.sql")
    if not target_sql.exists():
        target_sql.write_text(full_sql, encoding="utf-8")

    engine_used = None
    try:
        import duckdb  # type: ignore

        conn = duckdb.connect(str(db_path))
        try:
            conn.execute(exec_sql)
        finally:
            conn.close()
        engine_used = "python_duckdb"
    except ImportError:
        duckdb_bin = shutil.which("duckdb")
        if duckdb_bin:
            subprocess.run([duckdb_bin, str(db_path)], input=exec_sql.encode("utf-8"), check=True)
            engine_used = "cli_duckdb"
        elif require_duckdb:
            raise RuntimeError("DuckDB is not installed (install `pip install duckdb` or `duckdb` CLI on WSL)")

    result = {
        "campaign_id": manifest["campaign_id"],
        "sql_path": str(target_sql),
        "parquet_root": str(parquet_root),
        "db_path": str(db_path),
        "executed": engine_used is not None,
        "engine": engine_used,
    }
    if partial:
        result["artifact_kind"] = manifest["artifact_kind"]
        result["withheld_arcus_trade_messages"] = manifest["withheld_arcus_trade_messages"]
    return result


def bundle_directory_tar(source_dir: Path, bundle_out: Path, *, campaign_id: str) -> dict:
    """Pack a Silver Parquet or staging directory into one deterministic .tar for Drive upload."""
    resolved_dir = reject_unsafe_path(source_dir, must_be_directory=True)
    if bundle_out.exists() or bundle_out.is_symlink():
        raise FileExistsError(f"bundle already exists: {bundle_out}")
    if not bundle_out.name.endswith(".tar"):
        raise ValueError("bundle output must end in .tar")

    entries = []
    files = sorted(p for p in resolved_dir.rglob("*") if not p.is_dir())
    for item in files:
        if item.is_symlink():
            raise ValueError(f"symlink inside bundle directory rejected: {item}")
        rel = item.relative_to(resolved_dir).as_posix()
        if PRIVATE_NAME.search(rel):
            raise ValueError(f"private-indicating member rejected: {rel}")
        file_sha, file_bytes = digest_unchanged(item)
        entries.append({"path": rel, "sha256": file_sha, "bytes": file_bytes})

    bundle_out.parent.mkdir(parents=True, exist_ok=True)
    with TemporaryDirectory(dir=bundle_out.parent) as tmp_dir:
        tmp_tar = Path(tmp_dir) / "bundle.tar"
        with tarfile.open(tmp_tar, "w", format=tarfile.PAX_FORMAT) as archive:
            for entry in entries:
                disk_path = resolved_dir / entry["path"]
                info = tarfile.TarInfo(name=entry["path"])
                info.size = entry["bytes"]
                info.mtime = 0
                info.mode = 0o600
                info.uid = 0
                info.gid = 0
                info.uname = ""
                info.gname = ""
                with disk_path.open("rb") as handle:
                    archive.addfile(info, handle)
        os.link(tmp_tar, bundle_out)

    tar_sha, tar_bytes = digest_unchanged(bundle_out)
    manifest = {
        "schema_version": 1,
        "artifact_kind": "silver_tar_bundle",
        "campaign_id": campaign_id,
        "bundle_filename": bundle_out.name,
        "bundle_sha256": tar_sha,
        "bundle_bytes": tar_bytes,
        "files": entries,
    }
    write_immutable_json(bundle_out.with_suffix(".manifest.json"), manifest)
    return manifest


def unpack_bundle_tar(bundle_path: Path, manifest_path: Path, target_dir: Path) -> dict:
    """Verify and safely extract a Silver .tar bundle onto local WSL NVMe storage."""
    resolved_bundle = reject_unsafe_path(bundle_path)
    resolved_manifest = reject_unsafe_path(manifest_path)
    manifest = json.loads(resolved_manifest.read_text(encoding="utf-8"))

    tar_sha, tar_bytes = digest_unchanged(resolved_bundle)
    if tar_sha != manifest.get("bundle_sha256") or tar_bytes != manifest.get("bundle_bytes"):
        raise ValueError("bundle SHA-256 or byte size mismatch")

    expected = {item["path"]: (item["sha256"], item["bytes"]) for item in manifest["files"]}
    target_dir.mkdir(parents=True, exist_ok=True)
    target_resolved = target_dir.resolve()

    extracted = 0
    with tarfile.open(resolved_bundle, "r") as archive:
        for member in archive.getmembers():
            if not member.isfile() or member.issym() or member.islnk():
                raise ValueError(f"non-regular tar member rejected: {member.name}")
            if member.name not in expected:
                raise ValueError(f"unexpected tar member: {member.name}")
            dest = (target_resolved / member.name).resolve()
            if not dest.is_relative_to(target_resolved):
                raise ValueError(f"tar path traversal rejected: {member.name}")
            dest.parent.mkdir(parents=True, exist_ok=True)
            src = archive.extractfile(member)
            if src is None:
                raise ValueError(f"unable to read tar member: {member.name}")
            hasher = sha256()
            written = 0
            with dest.open("wb") as out_handle:
                while chunk := src.read(1024 * 1024):
                    hasher.update(chunk)
                    out_handle.write(chunk)
                    written += len(chunk)
            exp_sha, exp_bytes = expected[member.name]
            if hasher.hexdigest() != exp_sha or written != exp_bytes:
                dest.unlink(missing_ok=True)
                raise ValueError(f"extracted member hash mismatch: {member.name}")
            extracted += 1

    if extracted != len(expected):
        raise ValueError("bundle missing expected members")
    return {"campaign_id": manifest["campaign_id"], "files_verified": extracted, "target_dir": str(target_resolved)}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)

    p_seal = sub.add_parser("seal-bronze", help="Verify a completed capture and write its Bronze manifest")
    p_seal.add_argument("capture", type=Path)
    p_seal.add_argument("--campaign-id", required=True)
    p_seal.add_argument("--collector-region", choices=VALID_REGIONS, required=True)
    p_seal.add_argument("--split-role", choices=VALID_ROLES, required=True)
    p_seal.add_argument("--manifest-out", type=Path, required=True)

    p_evict = sub.add_parser("evict-local", help="Evict local capture after remote Drive SHA-256 verification")
    p_evict.add_argument("--capture", type=Path, required=True)
    p_evict.add_argument("--manifest", type=Path, required=True)
    p_evict.add_argument("--remote-receipt", type=Path, required=True)

    p_norm = sub.add_parser("normalize", help="Normalize sealed capture into 4 receipt-causal Silver tables")
    p_norm.add_argument("input", type=Path)
    p_norm.add_argument("--output-dir", type=Path, required=True)
    p_norm.add_argument("--campaign-id", required=True)
    p_norm.add_argument("--collector-region", choices=VALID_REGIONS, required=True)
    p_norm.add_argument("--precise-times", action="store_true")
    p_norm.add_argument(
        "--withhold-arcus-trade-market", action="append", default=[],
        help="Produce a partial Silver projection without trade subscriptions or prints for this market",
    )

    p_duck = sub.add_parser("build-duckdb", help="Compile Silver staging tables into Zstd Parquet + DuckDB views")
    p_duck.add_argument("--staging-dir", type=Path, required=True)
    p_duck.add_argument("--parquet-root", type=Path, required=True)
    p_duck.add_argument("--db-path", type=Path, required=True)
    p_duck.add_argument("--sql-out", type=Path)
    p_duck.add_argument("--require-duckdb", action="store_true")
    p_duck.add_argument("--allow-partial-projection", action="store_true",
                        help="Build a partial market projection using dedicated Parquet and DuckDB destinations")

    p_bundle = sub.add_parser("bundle-parquet", help="Bundle Parquet shards into one deterministic .tar for Drive")
    p_bundle.add_argument("--source-dir", type=Path, required=True)
    p_bundle.add_argument("--bundle-out", type=Path, required=True)
    p_bundle.add_argument("--campaign-id", required=True)

    p_unpack = sub.add_parser("unpack-bundle", help="Verify and unpack a Silver .tar bundle onto local storage")
    p_unpack.add_argument("--bundle", type=Path, required=True)
    p_unpack.add_argument("--manifest", type=Path, required=True)
    p_unpack.add_argument("--target-dir", type=Path, required=True)

    p_chain = sub.add_parser("seal-segment-chain", help="Verify contiguous rolling segments into one unbroken Bronze chain manifest")
    p_chain.add_argument("--segments", type=Path, nargs="+", required=True)
    p_chain.add_argument("--campaign-id", required=True)
    p_chain.add_argument("--collector-region", choices=VALID_REGIONS, required=True)
    p_chain.add_argument("--split-role", choices=VALID_ROLES, required=True)
    p_chain.add_argument("--manifest-out", type=Path, required=True)

    args = parser.parse_args()
    if args.command == "seal-bronze":
        result = seal_bronze_capture(
            args.capture,
            campaign_id=args.campaign_id,
            collector_region=args.collector_region,
            split_role=args.split_role,
            manifest_out=args.manifest_out,
        )
    elif args.command == "seal-segment-chain":
        result = seal_bronze_segment_chain(
            args.segments,
            campaign_id=args.campaign_id,
            collector_region=args.collector_region,
            split_role=args.split_role,
            manifest_out=args.manifest_out,
        )
    elif args.command == "evict-local":
        result = evict_local_capture(
            args.capture,
            manifest_path=args.manifest,
            remote_receipt_path=args.remote_receipt,
        )
    elif args.command == "normalize":
        result = normalize_capture(
            args.input,
            args.output_dir,
            campaign_id=args.campaign_id,
            collector_region=args.collector_region,
            precise_times=args.precise_times,
            withheld_arcus_trade_markets=frozenset(args.withhold_arcus_trade_market),
        )
    elif args.command == "build-duckdb":
        result = build_duckdb_lakehouse(
            args.staging_dir,
            args.parquet_root,
            args.db_path,
            sql_out=args.sql_out,
            require_duckdb=args.require_duckdb,
            allow_partial_projection=args.allow_partial_projection,
        )
    elif args.command == "bundle-parquet":
        result = bundle_directory_tar(args.source_dir, args.bundle_out, campaign_id=args.campaign_id)
    else:
        result = unpack_bundle_tar(args.bundle, args.manifest, args.target_dir)

    print(json.dumps(result, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
