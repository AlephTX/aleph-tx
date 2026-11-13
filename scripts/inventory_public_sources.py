#!/usr/bin/env python3
"""Inventory sealed public research archives without exporting their contents.

Only known campaign prefixes, archive locations and exact public market/fee
metadata names are inspected. Compiled results, evidence files outside this
allowlist and all unknown directories remain excluded; this is not an index
of every possible research artifact. The caller
must designate the public research root; filename hints do not verify stream
identity or coverage. All coverage remains unknown until a separate quality
audit establishes per-stream availability and gaps. Output never replaces an
existing catalog. Identical bytes retain path aliases but count only once.
"""

import argparse
from collections import Counter
from hashlib import sha256
import json
import os
from pathlib import Path
import re
import stat


CAMPAIGN_PREFIXES = ("2026-", "extended-lighter-", "robinhood-")
PRIVATE_NAME = re.compile(
    r"(?:^|[-_.])(?:private|account|accounts|orders|fills|balance|balances|telemetry|"
    r"recovery|debug|auth|authenticated|signed|credential|credentials|secret|secrets|"
    r"mnemonic|wallet|password|token|keys?)(?:$|[-_.])", re.IGNORECASE)
PUBLIC_METADATA_LEAVES = {
    "lighter": ("lighter", "market_terms"), "extended": ("extended", "market_terms"),
    "lighter_fees": ("lighter", "fees"), "extended_fees": ("extended", "fees"),
}


def public_metadata_hint(path, location, campaign_name):
    if re.fullmatch(r"metadata-\d{3}", location):
        return PUBLIC_METADATA_LEAVES.get(path.name)
    if location != "root":
        return None
    direct = {"lighter-metadata.json": "lighter", "lighter_robinhood-metadata.json": "lighter_robinhood",
              "extended-metadata.json": "extended"}
    if path.name in direct:
        return direct[path.name], "market_terms"
    match = re.fullmatch(r"metadata-\d{3}-(core|robinhood)\.json", path.name)
    if match:
        return ("lighter" if match[1] == "core" else "lighter_robinhood"), "market_terms"
    if path.name == "markets.json" and campaign_name.startswith("robinhood-public-"):
        return "lighter_robinhood", "market_terms"
    return None


def source_family_hint(name):
    lower = name.lower()
    for prefix, family in (
        ("lighter_robinhood", "lighter_robinhood"), ("lighter-robinhood", "lighter_robinhood"),
        ("hyperliquid", "hyperliquid"), ("coinbase", "coinbase"), ("lighter", "lighter"),
        ("extended", "extended"), ("entropy", "entropy"), ("tradexyz", "trade_xyz"),
        ("trade_xyz", "trade_xyz"), ("arcus", "arcus"), ("bulk", "bulk"),
    ):
        if lower.startswith(prefix + "-") or lower.startswith(prefix + "_"):
            return family
    return "mixed_or_unknown"


def archive_format(name):
    for suffix, kind in ((".jsonl.gz", "jsonl+gzip"), (".ndjson.gz", "ndjson+gzip"),
                         (".json.gz", "json+gzip"), (".ndjson.zst", "ndjson+zstd"),
                         (".jsonl.zst", "jsonl+zstd"), (".zst", "zstd")):
        if name.endswith(suffix):
            return kind
    return None


def _digest_unchanged(path):
    """Hash a regular file, rejecting replacement or append during the read."""
    descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
    with os.fdopen(descriptor, "rb") as stream:
        before = os.fstat(stream.fileno())
        if not stat.S_ISREG(before.st_mode):
            raise ValueError("not a regular archive")
        digest = sha256()
        while chunk := stream.read(1024 * 1024):
            digest.update(chunk)
        after = os.fstat(stream.fileno())
    current = path.lstat()
    signature = lambda s: (s.st_dev, s.st_ino, s.st_size, s.st_mtime_ns, s.st_ctime_ns)
    if signature(before) != signature(after) or signature(after) != signature(current):
        raise ValueError("archive changed while hashing")
    return digest.hexdigest(), after.st_size


def inventory_public_sources(root):
    root = Path(root).resolve(strict=True)
    if not root.is_dir():
        raise ValueError("public research root must be a directory")
    skipped, directories, candidates, campaigns = Counter(), [], [], []

    def skip_directory(path, reason):
        directories.append({"relative_path": str(path.relative_to(root)), "reason": reason})
        skipped[reason] += 1

    def inspect_file(path, location, campaign_name):
        name = path.name
        if path.is_symlink():
            skipped["symlink"] += 1
        elif ".partial" in name or name.endswith((".tmp", ".next")):
            skipped["incomplete_archive"] += 1
        elif PRIVATE_NAME.search(name):
            skipped["private_indicating_name"] += 1
        elif not path.is_file():
            skipped["not_regular_file"] += 1
        else:
            metadata = public_metadata_hint(path, location, campaign_name)
            if metadata is not None:
                candidates.append((path, {"artifact_type": "public_metadata", "format": "public_metadata",
                                          "source_family_hint": metadata[0], "metadata_kind": metadata[1]}))
                return
            allowed = (location == "raw" and archive_format(name) is not None)
            allowed |= location in {"root", "segments"} and name.startswith("capture-") and name.endswith(".ndjson.zst")
            allowed |= location == "root" and name in {"public.ndjson.gz", "public.ndjson.zst"}
            if allowed:
                candidates.append((path, {"artifact_type": "market_data_archive", "format": archive_format(name),
                                          "source_family_hint": source_family_hint(name)}))
            else:
                skipped["outside_archive_allowlist"] += 1

    for campaign in sorted(root.iterdir()):
        if campaign.is_symlink():
            skipped["symlink"] += 1
            continue
        if not campaign.is_dir():
            skipped["root_non_campaign_file"] += 1
            continue
        if not campaign.name.startswith(CAMPAIGN_PREFIXES) or PRIVATE_NAME.search(campaign.name):
            skip_directory(campaign, "outside_campaign_allowlist")
            continue
        campaigns.append(campaign.name)
        for child in sorted(campaign.iterdir()):
            if child.is_symlink():
                skipped["symlink"] += 1
            elif child.is_dir():
                if child.name not in {"raw", "segments"} and not re.fullmatch(r"metadata-\d{3}", child.name):
                    skip_directory(child, "excluded_campaign_directory")
                    continue
                for path in sorted(child.iterdir()):
                    if path.is_dir() and not path.is_symlink():
                        skip_directory(path, "nested_archive_directory")
                    else:
                        inspect_file(path, child.name, campaign.name)
            else:
                inspect_file(child, "root", campaign.name)

    by_digest = {}
    for path, hint in candidates:
        try:
            digest, size = _digest_unchanged(path)
        except (OSError, ValueError):
            skipped["unreadable_or_unstable_archive"] += 1
            continue
        location = {"relative_path": str(path.relative_to(root)), **hint}
        if digest in by_digest:
            by_digest[digest]["aliases"].append(location)
            by_digest[digest]["artifact_types"] = sorted(set(by_digest[digest]["artifact_types"]) | {hint["artifact_type"]})
            if by_digest[digest]["source_family_hint"] != location["source_family_hint"]:
                by_digest[digest]["source_family_hint"] = "mixed_or_unknown"
        else:
            by_digest[digest] = {"sha256": digest, "bytes": size, **location,
                                 "artifact_types": [hint["artifact_type"]],
                                 "aliases": [], "coverage": "unknown", "stream_identity": "unverified",
                                 "seal_evidence": "allowlisted_name_and_stable_read_only"}
    artifacts = sorted(by_digest.values(), key=lambda item: item["relative_path"])
    alias_count = sum(len(a["aliases"]) for a in artifacts)
    archives = [a for a in artifacts if "market_data_archive" in a["artifact_types"]]
    metadata = [a for a in artifacts if "public_metadata" in a["artifact_types"]]
    locations = [location for a in artifacts for location in [a, *a["aliases"]]]
    report = {"schema_version": 2, "scope": "allowlisted public archives and market/fee snapshots; not a coverage or fee eligibility certificate",
              "campaigns": campaigns, "artifacts": artifacts,
              "unique_artifacts": len(artifacts), "unique_archives": len(archives), "unique_metadata": len(metadata),
              "archive_path_count": sum(a["artifact_type"] == "market_data_archive" for a in locations),
              "metadata_path_count": sum(a["artifact_type"] == "public_metadata" for a in locations),
              "unique_archive_bytes": sum(a["bytes"] for a in archives),
              "unique_metadata_bytes": sum(a["bytes"] for a in metadata),
              "duplicate_aliases": alias_count, "unique_bytes": sum(a["bytes"] for a in artifacts),
              "skipped_counts": dict(sorted(skipped.items())), "skipped_directories": directories}
    report["inventory_sha256"] = sha256(json.dumps(report, sort_keys=True, separators=(",", ":")).encode()).hexdigest()
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root", type=Path)
    parser.add_argument("--output", type=Path, help="new private catalog file; existing files are never replaced")
    args = parser.parse_args()
    if args.output is not None and (args.output.exists() or args.output.is_symlink()):
        parser.error("output already exists; choose a new immutable catalog path")
    report = inventory_public_sources(args.root)
    encoded = json.dumps(report, indent=2, sort_keys=True, allow_nan=False) + "\n"
    if args.output is None:
        print(encoded, end="")
    else:
        os.umask(0o077)
        with args.output.open("x") as stream:
            stream.write(encoded)
        fields = ("inventory_sha256", "unique_artifacts", "unique_archives", "archive_path_count",
                  "unique_metadata", "metadata_path_count", "duplicate_aliases", "unique_bytes", "skipped_counts")
        print(json.dumps({key: report[key] for key in fields}, sort_keys=True))


if __name__ == "__main__":
    main()
