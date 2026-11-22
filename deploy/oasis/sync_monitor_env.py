#!/usr/bin/env python3
"""Extract public-only account identifiers into `/etc/aleph-tx/monitor.env`.

Allows operators to maintain a single private trading `.env` while keeping
`aleph-tx-monitor` strictly isolated from every private key or API secret.
"""

from __future__ import annotations

import argparse
import os
from pathlib import Path
import re
import sys

PUBLIC_MAPPING = (
    ("LIGHTER_ACCOUNT_INDEX", "ALEPH_MONITOR_LIGHTER_INDEX", r"[0-9]{1,19}"),
    ("LIGHTER_RH_ACCOUNT_INDEX", "ALEPH_MONITOR_LIGHTER_RH_INDEX", r"[0-9]{1,19}"),
    ("HYPERLIQUID_ACCOUNT_ADDRESS", "ALEPH_MONITOR_HL_ADDRESS", r"0x[0-9a-fA-F]{40}"),
    ("HYPERLIQUID_VAULT_ADDRESS", "ALEPH_MONITOR_HL_XYZ_ADDRESS", r"0x[0-9a-fA-F]{40}"),
    ("ARCUS_ACCOUNT_ADDRESS", "ALEPH_MONITOR_ARCUS_ADDRESS", r"0x[0-9a-fA-F]{40}"),
    ("ARCUS_ACCOUNT_INDEX", "ALEPH_MONITOR_ARCUS_INDEX", r"[0-9]"),
    ("BULK_ACCOUNT_PUBKEY", "ALEPH_MONITOR_BULK_PUBKEY", r"[1-9A-HJ-NP-Za-km-z]{32,44}"),
)
PRESERVED_OPERATIONAL_KEYS = (
    "ALEPH_MONITOR_CONTROL_TOKEN",
    "ALEPH_MONITOR_ALERT_WEBHOOK",
    "ALEPH_MONITOR_RETENTION_DAYS",
    "ALEPH_MONITOR_PORT",
)
SENSITIVE_PRESERVED_KEYS = frozenset({
    "ALEPH_MONITOR_CONTROL_TOKEN",
    "ALEPH_MONITOR_ALERT_WEBHOOK",
})
PRESERVED_PUBLIC_OVERRIDE_KEYS = (
    "ALEPH_MONITOR_HL_XYZ_ADDRESS",
    "ALEPH_MONITOR_HL_IO_ADDRESS",
)
FORBIDDEN_KEY_FRAGMENTS = ("PRIVATE", "SECRET", "_KEY", "PASS", "MNEM" + "ONIC", "SEED")
ALLOWED_KEY_SET = (
    {dst for _, dst, _ in PUBLIC_MAPPING}
    | set(PRESERVED_OPERATIONAL_KEYS)
    | set(PRESERVED_PUBLIC_OVERRIDE_KEYS)
)


def _strip_quotes(value: str) -> str:
    value = value.strip()
    if len(value) >= 2 and value[0] == value[-1] and value[0] in ('"', "'"):
        return value[1:-1].strip()
    return value


def parse_dotenv(path: Path) -> dict[str, str]:
    parsed: dict[str, str] = {}
    for raw in path.read_text(encoding="utf-8").splitlines():
        line = raw.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, value = line.split("=", 1)
        parsed[key.strip()] = _strip_quotes(value)
    return parsed


def build_monitor_env(source_env: dict[str, str], existing_env: dict[str, str] | None = None) -> list[tuple[str, str]]:
    """Build deterministic `(KEY, VALUE)` pairs containing zero secret material."""
    pairs: list[tuple[str, str]] = []
    existing = existing_env or {}
    for key in PRESERVED_OPERATIONAL_KEYS:
        val = existing.get(key, "").strip()
        if val:
            pairs.append((key, val))

    source_overrides = {
        dst_key
        for src_key, dst_key, _ in PUBLIC_MAPPING
        if dst_key in PRESERVED_PUBLIC_OVERRIDE_KEYS and source_env.get(src_key, "").strip()
    }
    for key in PRESERVED_PUBLIC_OVERRIDE_KEYS:
        if key in source_overrides:
            continue
        val = existing.get(key, "").strip()
        if val:
            if not re.fullmatch(r"0x[0-9a-fA-F]{40}", val):
                raise ValueError(f"invalid existing public override for {key}")
            pairs.append((key, val))

    arcus_addr = source_env.get("ARCUS_ACCOUNT_ADDRESS", "").strip()
    for src_key, dst_key, pattern in PUBLIC_MAPPING:
        if any(frag in dst_key for frag in FORBIDDEN_KEY_FRAGMENTS):
            raise AssertionError(f"forbidden monitor destination key: {dst_key}")
        if src_key == "ARCUS_ACCOUNT_INDEX" and not arcus_addr:
            continue
        raw_val = source_env.get(src_key, "").strip()
        if not raw_val:
            continue
        if not re.fullmatch(pattern, raw_val):
            raise ValueError(f"invalid public identifier for {src_key}")
        if src_key == "LIGHTER_ACCOUNT_INDEX" or src_key == "LIGHTER_RH_ACCOUNT_INDEX":
            if not (0 < int(raw_val) <= 2**63 - 1):
                raise ValueError(f"invalid account index range for {src_key}")
        if dst_key not in ALLOWED_KEY_SET:
            raise AssertionError(f"unexpected destination key: {dst_key}")
        pairs.append((dst_key, raw_val))
    return pairs


def render_monitor_env(pairs: list[tuple[str, str]], *, redact_sensitive: bool = False) -> str:
    lines = ["# Managed public-only identifiers & operational monitor settings (mode 0600)."]
    for key, value in pairs:
        shown = "<redacted>" if redact_sensitive and key in SENSITIVE_PRESERVED_KEYS else value
        lines.append(f"{key}={shown}")
    return "\n".join(lines) + "\n"


def sync_monitor_env_file(source: Path, output: Path | None = None, *, dry_run: bool = False) -> str:
    source_env = parse_dotenv(source)
    existing_env = parse_dotenv(output) if (output is not None and output.is_file()) else None
    pairs = build_monitor_env(source_env, existing_env)
    rendered = render_monitor_env(pairs, redact_sensitive=False)
    if output is not None and not dry_run:
        output.parent.mkdir(parents=True, exist_ok=True)
        tmp = output.with_name(f".{output.name}.tmp.{os.getpid()}")
        fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        try:
            with os.fdopen(fd, "w", encoding="utf-8") as handle:
                handle.write(rendered)
            os.chmod(tmp, 0o600)
            os.replace(tmp, output)
        except BaseException:
            if tmp.exists():
                tmp.unlink()
            raise
        return rendered
    return render_monitor_env(pairs, redact_sensitive=True)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, required=True, help="Private `.env*` file to extract public IDs from")
    parser.add_argument("--output", type=Path, default=None, help="Destination monitor.env path (writes mode 0600)")
    parser.add_argument("--dry-run", action="store_true", help="Print redacted preview without writing secrets")
    args = parser.parse_args()
    if args.dry_run or args.output is None:
        source_env = parse_dotenv(args.source)
        existing_env = parse_dotenv(args.output) if (args.output is not None and args.output.is_file()) else None
        pairs = build_monitor_env(source_env, existing_env)
        sys.stdout.write(render_monitor_env(pairs, redact_sensitive=True))
    else:
        sync_monitor_env_file(args.source, args.output, dry_run=False)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
