#!/usr/bin/env python3
"""Validate unified multi-exchange `.env` files and emit least-privilege splits.

Never prints or logs secret values. Emits strictly scoped runtime env files
(`arcus_xyz_basis.env`, `live.env`, and public-only `monitor.env`) with mode
`0600` inside a `0700` output directory.
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass
import json
import os
from pathlib import Path
import re
import sys
from typing import Any

REPO_ROOT = Path(__file__).resolve().parent.parent
for _cand in (REPO_ROOT / "deploy" / "oasis", REPO_ROOT):
    if _cand.is_dir() and str(_cand) not in sys.path:
        sys.path.insert(0, str(_cand))

import sync_monitor_env  # type: ignore  # noqa: E402

RE_EVM_ADDR = re.compile(r"0x[0-9a-fA-F]{40}")
RE_HEX32 = re.compile(r"(?:0x)?[0-9a-fA-F]{64}")
RE_HEX40 = re.compile(r"(?:0x)?[0-9a-fA-F]{80}")
RE_BASE58 = re.compile(r"[1-9A-HJ-NP-Za-km-z]{32,44}")
RE_INT = re.compile(r"[0-9]{1,20}")

ARCUS_ENV_WHITELIST = (
    "ARCUS_ACCOUNT_ADDRESS",
    "ARCUS_ACCOUNT_INDEX",
    "ARCUS_API_PRIVATE_KEY",
    "HYPERLIQUID_ACCOUNT_ADDRESS",
    "HYPERLIQUID_VAULT_ADDRESS",
    "HYPERLIQUID_AGENT_PRIVATE_KEY",
)

LIGHTER_CORE_FEEDER_ENV = (
    "LIGHTER_PRIVATE_KEY",
    "API_KEY_PRIVATE_KEY",
    "LIGHTER_ACCOUNT_INDEX",
    "LIGHTER_API_KEY_INDEX",
    "LIGHTER_BASE_URL",
    "LIGHTER_CHAIN_ID",
)
LIGHTER_EXTRA_LANE_ENV = (
    "LIGHTER_CANCEL_PRIVATE_KEY",
    "LIGHTER_CANCEL_API_KEY_INDEX",
    "LIGHTER_TAKER_PRIVATE_KEY",
    "LIGHTER_TAKER_API_KEY_INDEX",
)
LIGHTER_HEDGE_ENV = (
    "HYPERLIQUID_ACCOUNT_ADDRESS",
    "HYPERLIQUID_AGENT_PRIVATE_KEY",
)


@dataclass(frozen=True)
class VenueSpec:
    name: str
    required: tuple[tuple[str, ...], ...]
    optional: tuple[str, ...] = ()
    prefixes: tuple[str, ...] = ()


VENUE_SPECS: tuple[VenueSpec, ...] = (
    VenueSpec(
        name="lighter_core",
        required=(
            ("LIGHTER_ACCOUNT_INDEX",),
            ("LIGHTER_API_KEY_INDEX",),
            ("LIGHTER_PRIVATE_KEY", "API_KEY_PRIVATE_KEY"),
        ),
        optional=(
            "LIGHTER_BASE_URL",
            "LIGHTER_CHAIN_ID",
            "LIGHTER_CANCEL_PRIVATE_KEY",
            "LIGHTER_CANCEL_API_KEY_INDEX",
            "LIGHTER_TAKER_PRIVATE_KEY",
            "LIGHTER_TAKER_API_KEY_INDEX",
        ),
    ),
    VenueSpec(
        name="lighter_rh",
        required=(("LIGHTER_RH_ACCOUNT_INDEX",),),
        optional=(
            "LIGHTER_RH_API_KEY_INDEX",
            "LIGHTER_RH_PRIVATE_KEY",
            "LIGHTER_RH_CANCEL_API_KEY_INDEX",
            "LIGHTER_RH_CANCEL_PRIVATE_KEY",
            "LIGHTER_RH_TAKER_API_KEY_INDEX",
            "LIGHTER_RH_TAKER_PRIVATE_KEY",
            "LIGHTER_RH_BASE_URL",
            "LIGHTER_RH_CHAIN_ID",
        ),
        prefixes=("LIGHTER_RH_",),
    ),
    VenueSpec(
        name="lighter_testnet",
        required=(
            ("LIGHTER_TESTNET_ACCOUNT_INDEX",),
            ("LIGHTER_TESTNET_API_KEY_INDEX",),
            ("LIGHTER_TESTNET_PRIVATE_KEY",),
        ),
        optional=(
            "LIGHTER_TESTNET_BASE_URL",
            "LIGHTER_TESTNET_CHAIN_ID",
            "LIGHTER_TESTNET_CANCEL_PRIVATE_KEY",
            "LIGHTER_TESTNET_CANCEL_API_KEY_INDEX",
            "LIGHTER_TESTNET_TAKER_PRIVATE_KEY",
            "LIGHTER_TESTNET_TAKER_API_KEY_INDEX",
        ),
        prefixes=("LIGHTER_TESTNET_",),
    ),
    VenueSpec(
        name="hyperliquid",
        required=(
            ("HYPERLIQUID_ACCOUNT_ADDRESS",),
            ("HYPERLIQUID_AGENT_PRIVATE_KEY",),
        ),
        optional=("HYPERLIQUID_VAULT_ADDRESS",),
        prefixes=("HYPERLIQUID_",),
    ),
    VenueSpec(
        name="arcus",
        required=(
            ("ARCUS_ACCOUNT_ADDRESS",),
            ("ARCUS_API_PRIVATE_KEY",),
        ),
        optional=("ARCUS_ACCOUNT_INDEX",),
        prefixes=("ARCUS_",),
    ),
    VenueSpec(
        name="bulk",
        required=(("BULK_ACCOUNT_PUBKEY", "BULK_PUBKEY"),),
        optional=("BULK_PRIVATE_KEY", "BULK_API_PRIVATE_KEY", "BULK_SIGNER_PRIVATE_KEY"),
        prefixes=("BULK_",),
    ),
    VenueSpec(
        name="extended",
        required=(
            ("EXTENDED_API_KEY",),
            ("EXTENDED_STARK_PRIVATE_KEY",),
        ),
        optional=("EXTENDED_PUBLIC_KEY", "EXTENDED_VAULT_ID", "EXTENDED_ACCOUNT_ADDRESS"),
        prefixes=("EXTENDED_",),
    ),
    VenueSpec(
        name="variational",
        required=(
            ("VARIATIONAL_ACCOUNT_ADDRESS", "VARIATIONAL_ADDRESS"),
            ("VARIATIONAL_VR_TOKEN", "VARIATIONAL_API_KEY", "VARIATIONAL_COOKIE"),
        ),
        optional=("VARIATIONAL_API_SECRET", "VARIATIONAL_PRIVATE_KEY"),
        prefixes=("VARIATIONAL_",),
    ),
    VenueSpec(
        name="backpack",
        required=(
            ("BACKPACK_PUBLIC_KEY",),
            ("BACKPACK_SECRET_KEY",),
        ),
        optional=("BACKPACK_ENV_PATH",),
        prefixes=("BACKPACK_",),
    ),
    VenueSpec(
        name="edgex",
        required=(
            ("EDGEX_ACCOUNT_ID",),
            ("EDGEX_STARK_PRIVATE_KEY",),
        ),
        optional=("EDGEX_L2_PRIVATE_KEY", "EDGEX_ENV_PATH"),
        prefixes=("EDGEX_",),
    ),
    VenueSpec(
        name="binance",
        required=(
            ("BINANCE_API_KEY",),
            ("BINANCE_API_SECRET", "BINANCE_SECRET_KEY"),
        ),
        optional=(),
        prefixes=("BINANCE_",),
    ),
    VenueSpec(
        name="okx",
        required=(
            ("OKX_API_KEY",),
            ("OKX_API_SECRET",),
            ("OKX_PASSPHRASE", "OKX_API_PASSPHRASE"),
        ),
        optional=(),
        prefixes=("OKX_",),
    ),
    VenueSpec(
        name="coinbase",
        required=(
            ("COINBASE_API_KEY", "COINBASE_KEY_NAME"),
            ("COINBASE_API_SECRET", "COINBASE_PRIVATE_KEY"),
        ),
        optional=(),
        prefixes=("COINBASE_",),
    ),
    VenueSpec(
        name="zeroone",
        required=(
            ("ZEROONE_ACCOUNT_PUBKEY", "ZEROONE_PUBKEY", "O1_PUBLIC_KEY"),
            ("ZEROONE_PRIVATE_KEY", "O1_PRIVATE_KEY"),
        ),
        optional=("ZEROONE_API_KEY", "O1_ACCOUNT_INDEX"),
        prefixes=("ZEROONE_", "O1_"),
    ),
)

_PLACEHOLDER_RE = re.compile(r"^(?:<[^>]+>|YOUR_[A-Z0-9_]+|CHANGEME|REPLACE_ME)$", re.IGNORECASE)


def _strip_quotes(value: str) -> str:
    value = value.strip()
    if len(value) >= 2 and value[0] == value[-1] and value[0] in ('"', "'"):
        return value[1:-1].strip()
    return value


def parse_dotenv_with_duplicates(path: Path) -> tuple[dict[str, str], list[str]]:
    parsed: dict[str, str] = {}
    duplicates: list[str] = []
    for raw in path.read_text(encoding="utf-8").splitlines():
        line = raw.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, value = line.split("=", 1)
        key = key.strip()
        val = _strip_quotes(value)
        if key in parsed and key not in duplicates:
            duplicates.append(key)
        parsed[key] = val
    return parsed, duplicates


def _valid_value(key: str, val: str) -> bool:
    if not val or _PLACEHOLDER_RE.fullmatch(val) or any(ord(c) < 32 for c in val):
        return False
    if key in (
        "HYPERLIQUID_ACCOUNT_ADDRESS",
        "HYPERLIQUID_VAULT_ADDRESS",
        "ARCUS_ACCOUNT_ADDRESS",
        "VARIATIONAL_ACCOUNT_ADDRESS",
        "VARIATIONAL_ADDRESS",
        "EXTENDED_ACCOUNT_ADDRESS",
    ):
        return RE_EVM_ADDR.fullmatch(val) is not None
    if key in (
        "ARCUS_API_PRIVATE_KEY",
        "HYPERLIQUID_AGENT_PRIVATE_KEY",
        "EXTENDED_STARK_PRIVATE_KEY",
        "EDGEX_STARK_PRIVATE_KEY",
        "EDGEX_L2_PRIVATE_KEY",
        "ZEROONE_PRIVATE_KEY",
        "O1_PRIVATE_KEY",
        "BULK_PRIVATE_KEY",
        "BULK_API_PRIVATE_KEY",
        "BULK_SIGNER_PRIVATE_KEY",
    ):
        return RE_HEX32.fullmatch(val) is not None
    if key in (
        "LIGHTER_PRIVATE_KEY",
        "API_KEY_PRIVATE_KEY",
        "LIGHTER_CANCEL_PRIVATE_KEY",
        "LIGHTER_TAKER_PRIVATE_KEY",
        "LIGHTER_RH_PRIVATE_KEY",
        "LIGHTER_RH_CANCEL_PRIVATE_KEY",
        "LIGHTER_RH_TAKER_PRIVATE_KEY",
        "LIGHTER_TESTNET_PRIVATE_KEY",
        "LIGHTER_TESTNET_CANCEL_PRIVATE_KEY",
        "LIGHTER_TESTNET_TAKER_PRIVATE_KEY",
    ):
        return RE_HEX40.fullmatch(val) is not None
    if key in (
        "BULK_ACCOUNT_PUBKEY",
        "BULK_PUBKEY",
        "ZEROONE_ACCOUNT_PUBKEY",
        "ZEROONE_PUBKEY",
        "O1_PUBLIC_KEY",
    ):
        return RE_BASE58.fullmatch(val) is not None
    if key == "ARCUS_ACCOUNT_INDEX":
        return len(val) == 1 and val.isdigit()
    if key in (
        "LIGHTER_ACCOUNT_INDEX",
        "LIGHTER_RH_ACCOUNT_INDEX",
        "LIGHTER_TESTNET_ACCOUNT_INDEX",
        "EDGEX_ACCOUNT_ID",
        "EXTENDED_VAULT_ID",
    ):
        return RE_INT.fullmatch(val) is not None and 0 < int(val) <= 2**63 - 1
    if key in (
        "LIGHTER_API_KEY_INDEX",
        "LIGHTER_CANCEL_API_KEY_INDEX",
        "LIGHTER_TAKER_API_KEY_INDEX",
        "LIGHTER_RH_API_KEY_INDEX",
        "LIGHTER_RH_CANCEL_API_KEY_INDEX",
        "LIGHTER_RH_TAKER_API_KEY_INDEX",
        "LIGHTER_TESTNET_API_KEY_INDEX",
        "LIGHTER_TESTNET_CANCEL_API_KEY_INDEX",
        "LIGHTER_TESTNET_TAKER_API_KEY_INDEX",
    ):
        return val.isdigit() and 0 <= int(val) <= 255
    if key.endswith(("_BASE_URL", "_URL")):
        return val.startswith("https://") or val.startswith("http://")
    if key.endswith("_CHAIN_ID") or key == "O1_ACCOUNT_INDEX":
        return val.isdigit() and int(val) >= 0
    return not any(ch.isspace() for ch in val)


def _known_keys_for_spec(spec: VenueSpec, env: dict[str, str]) -> list[str]:
    ordered: list[str] = []
    for alts in spec.required:
        ordered.extend(alts)
    ordered.extend(spec.optional)
    for k in sorted(env):
        if k not in ordered and any(k.startswith(p) for p in spec.prefixes):
            ordered.append(k)
    seen: set[str] = set()
    return [k for k in ordered if not (k in seen or seen.add(k))]


def audit_env_dict(env: dict[str, str], duplicates: list[str] | None = None, *, env_path: str = "<memory>") -> dict[str, Any]:
    """Audit all venue sections without returning any secret values."""
    dup_set = set(duplicates or ())
    venues_report: dict[str, dict[str, Any]] = {}
    summary: dict[str, list[str]] = {"ready": [], "partial": [], "empty": [], "invalid": []}

    for spec in VENUE_SPECS:
        keys = _known_keys_for_spec(spec, env)
        present = [k for k in keys if env.get(k, "").strip()]
        invalid = [k for k in present if not _valid_value(k, env[k].strip())]
        dup_in_venue = [k for k in keys if k in dup_set]
        missing_required = [
            "/".join(alts)
            for alts in spec.required
            if not any(env.get(k, "").strip() for k in alts)
        ]

        if invalid or dup_in_venue:
            status = "invalid"
        elif not present:
            status = "empty"
        elif missing_required:
            status = "partial"
        else:
            status = "ready"

        summary[status].append(spec.name)
        venues_report[spec.name] = {
            "status": status,
            "present_keys": present,
            "missing_required": missing_required,
            "invalid_keys": invalid,
            "duplicate_keys": dup_in_venue,
        }

    arcus_ok = venues_report["arcus"]["status"] == "ready"
    hl_ok = venues_report["hyperliquid"]["status"] == "ready"
    lighter_ok = venues_report["lighter_core"]["status"] == "ready"
    monitor_ok = bool(sync_monitor_env.build_monitor_env(env, None)) if not any(
        venues_report[v]["status"] == "invalid"
        for v in ("lighter_core", "lighter_rh", "hyperliquid", "arcus", "bulk")
    ) else False

    return {
        "env_path": env_path,
        "duplicate_keys": sorted(dup_set),
        "venues": venues_report,
        "summary": summary,
        "split_eligible": {
            "arcus_xyz_basis.env": arcus_ok and hl_ok,
            "live.env": lighter_ok,
            "monitor.env": monitor_ok,
        },
    }


def _write_mode_600(path: Path, lines: list[str]) -> None:
    content = "\n".join(lines) + "\n"
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    os.chmod(path.parent, 0o700)
    tmp = path.with_name(f".{path.name}.tmp.{os.getpid()}")
    fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            handle.write(content)
        os.chmod(tmp, 0o600)
        os.replace(tmp, path)
    except BaseException:
        if tmp.exists():
            tmp.unlink()
        raise


def split_env_file(source: Path, out_dir: Path) -> dict[str, Any]:
    """Emit strictly isolated `0600` runtime files from a unified `.env`."""
    env, duplicates = parse_dotenv_with_duplicates(source)
    report = audit_env_dict(env, duplicates, env_path=str(source))
    venues = report["venues"]
    written: dict[str, dict[str, Any]] = {}
    skipped: dict[str, str] = {}

    out_dir.mkdir(parents=True, exist_ok=True, mode=0o700)
    os.chmod(out_dir, 0o700)

    # 1. arcus_xyz_basis.env (Arcus + Hyperliquid)
    if venues["arcus"]["status"] == "ready" and venues["hyperliquid"]["status"] == "ready":
        lines = ["# Scoped runtime env for aleph-tx-arcus-xyz.service (mode 0600)."]
        emitted_keys: list[str] = []
        for k in ARCUS_ENV_WHITELIST:
            val = env.get(k, "").strip()
            if val:
                lines.append(f"{k}={val}")
                emitted_keys.append(k)
        target = out_dir / "arcus_xyz_basis.env"
        _write_mode_600(target, lines)
        written["arcus_xyz_basis.env"] = {"path": str(target), "keys": emitted_keys, "mode": "0600"}
    else:
        skipped["arcus_xyz_basis.env"] = (
            f"requires ready arcus ({venues['arcus']['status']}) & hyperliquid ({venues['hyperliquid']['status']})"
        )

    # 2. live.env (Lighter Core + optional Hyperliquid hedge)
    if venues["lighter_core"]["status"] == "ready":
        lines = ["# Scoped runtime env for aleph-tx-live.service (mode 0600)."]
        emitted_keys = []
        allowed = list(LIGHTER_CORE_FEEDER_ENV) + list(LIGHTER_EXTRA_LANE_ENV)
        if venues["hyperliquid"]["status"] == "ready":
            allowed.extend(LIGHTER_HEDGE_ENV)
        for k in allowed:
            if k == "API_KEY_PRIVATE_KEY" and env.get("LIGHTER_PRIVATE_KEY", "").strip():
                continue
            val = env.get(k, "").strip()
            if val:
                lines.append(f"{k}={val}")
                emitted_keys.append(k)
        target = out_dir / "live.env"
        _write_mode_600(target, lines)
        written["live.env"] = {"path": str(target), "keys": emitted_keys, "mode": "0600"}
    else:
        skipped["live.env"] = f"requires ready lighter_core ({venues['lighter_core']['status']})"

    # 3. monitor.env (public IDs only, preserves operational monitor tokens/webhooks)
    mon_target = out_dir / "monitor.env"
    existing_mon = sync_monitor_env.parse_dotenv(mon_target) if mon_target.is_file() else None
    mon_pairs = sync_monitor_env.build_monitor_env(env, existing_mon)
    if mon_pairs:
        sync_monitor_env.sync_monitor_env_file(source, mon_target)
        written["monitor.env"] = {
            "path": str(mon_target),
            "keys": [k for k, _ in mon_pairs],
            "mode": "0600",
        }
    else:
        skipped["monitor.env"] = "no valid public monitor identifiers present"

    return {
        "source": str(source),
        "out_dir": str(out_dir),
        "written": written,
        "skipped": skipped,
        "audit_summary": report["summary"],
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)

    p_audit = sub.add_parser("audit", help="Audit all venue credentials without printing any secret values")
    p_audit.add_argument("--env", type=Path, required=True)

    p_split = sub.add_parser("split", help="Emit isolated 0600 env files from unified env")
    p_split.add_argument("--env", type=Path, required=True)
    p_split.add_argument("--out-dir", type=Path, required=True)

    args = parser.parse_args(argv)
    if not args.env.is_file():
        print(json.dumps({"error": f"missing env file: {args.env}"}))
        return 2

    if args.command == "audit":
        env, duplicates = parse_dotenv_with_duplicates(args.env)
        print(json.dumps(audit_env_dict(env, duplicates, env_path=str(args.env)), indent=2))
        return 0
    if args.command == "split":
        print(json.dumps(split_env_file(args.env, args.out_dir), indent=2))
        return 0
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
