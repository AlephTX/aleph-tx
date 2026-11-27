#!/usr/bin/env python3
"""Shared pure-stdlib fee-evidence loader and OPT-01 capability resolver for Track 01-03 scripts.

Synchronized with `VENUE_CAPABILITY_SNAPSHOT_VERSION = "2026-11-22.v2"` in `src/venues.rs`.
"""

from __future__ import annotations

import hashlib
import json
import math
from pathlib import Path
from typing import Any, Sequence

VENUE_CAPABILITY_SNAPSHOT_VERSION = "2026-11-22.v2"

ARCUS_FEE_TIERS: dict[str, dict[str, Any]] = {
    "level_0": {
        "crypto": (1.50, 4.50),
        "rwa": (0.0, 2.25),
        "status": "public_tier0_schedule_only",
    },
    "level_4": {
        "crypto": (0.0, 2.30),
        "rwa": (0.0, 1.15),
        "status": "account_evidence_required",
    },
}

TRADE_XYZ_FEE_MODES: dict[str, tuple[float, float]] = {
    "growth_mode": (0.30, 0.90),
    "standard": (3.00, 9.00),
}


def _opt_finite(value: Any, field: str) -> float | None:
    """Parse an optional non-negative finite float, failing closed on booleans/NaN/Inf/negatives."""
    if value is None:
        return None
    if isinstance(value, bool):
        raise ValueError(f"{field} must be numeric, got bool")
    try:
        parsed = float(value)
    except (TypeError, ValueError) as exc:
        raise ValueError(f"{field} must be numeric, got {value!r}") from exc
    if not math.isfinite(parsed) or parsed < 0.0:
        raise ValueError(f"{field} must be finite and >= 0, got {value!r}")
    return parsed


def _extract_pair(
    maker_raw: Any,
    taker_raw: Any,
    prefix: str,
) -> tuple[float, float] | None:
    """Parse an optional `(maker_fee_bps, taker_fee_bps)` pair, failing closed on half-pairs."""
    maker = _opt_finite(maker_raw, f"{prefix}.maker_fee_bps")
    taker = _opt_finite(taker_raw, f"{prefix}.taker_fee_bps")
    if (maker is None) != (taker is None):
        raise ValueError(f"{prefix}: maker_fee_bps and taker_fee_bps must both be set or both null")
    if maker is None or taker is None:
        return None
    return (maker, taker)


def _confirmed_account_pair(payload: dict[str, Any], venue: str) -> tuple[float, float] | None:
    pair = _extract_pair(payload.get("maker_fee_bps"), payload.get("taker_fee_bps"), venue)
    if pair is not None and payload.get("account_fee_status") != "verified_account_tier":
        raise ValueError(f"{venue}: fee override requires verified_account_tier evidence")
    return pair


def load_track_fee_evidence(evidence_path: Path | None) -> dict[str, Any]:
    """Load `aleph_account_fee_evidence_v1` JSON strictly via stdlib, preserving provenance hashes."""
    if evidence_path is None:
        return {
            "loaded": False,
            "evidence_name": None,
            "evidence_sha256": None,
            "schema_version": None,
            "audited_at_utc": None,
            "venues": {},
        }

    path = Path(evidence_path)
    raw_bytes = path.read_bytes()
    sha = hashlib.sha256(raw_bytes).hexdigest()
    data = json.loads(raw_bytes.decode("utf-8"))
    if not isinstance(data, dict) or not isinstance(data.get("venues"), dict):
        raise ValueError(f"Expected JSON object with dict 'venues' in {path}")

    parsed_venues: dict[str, dict[str, Any]] = {}
    for venue_name, payload in data["venues"].items():
        if not isinstance(payload, dict):
            continue
        vname = str(venue_name)
        account_pair = _confirmed_account_pair(payload, vname)
        entry: dict[str, Any] = {
            "account_fee_status": (
                str(payload["account_fee_status"]) if payload.get("account_fee_status") is not None else None
            ),
            "maker_fee_bps": account_pair[0] if account_pair is not None else None,
            "taker_fee_bps": account_pair[1] if account_pair is not None else None,
        }

        if vname == "arcus":
            pub = payload.get("public_tier0_schedule_summary") or {}
            if not isinstance(pub, dict):
                raise ValueError("arcus.public_tier0_schedule_summary must be a dict")
            tiers = pub.get("tier0_fee_profiles") or []
            if not isinstance(tiers, list):
                raise ValueError("arcus.public_tier0_schedule_summary.tier0_fee_profiles must be a list")
            tier0_crypto: tuple[float, float] | None = None
            tier0_rwa: tuple[float, float] | None = None
            for idx, item in enumerate(tiers):
                if not isinstance(item, dict):
                    raise ValueError(f"arcus.tier0_fee_profiles[{idx}] must be a dict")
                pair = _extract_pair(
                    item.get("maker_fee_bps"),
                    item.get("taker_fee_bps"),
                    f"arcus.tier0_fee_profiles[{idx}]",
                )
                if pair is None:
                    raise ValueError(f"arcus.tier0_fee_profiles[{idx}] missing maker/taker fee bps")
                if math.isclose(pair[0], 1.5, abs_tol=1e-9) and math.isclose(pair[1], 4.5, abs_tol=1e-9):
                    tier0_crypto = pair
                elif math.isclose(pair[0], 0.0, abs_tol=1e-9) and math.isclose(pair[1], 2.25, abs_tol=1e-9):
                    tier0_rwa = pair

            custom = pub.get("custom_schedule_tier0_bps") or {}
            if not isinstance(custom, dict):
                raise ValueError("arcus.public_tier0_schedule_summary.custom_schedule_tier0_bps must be a dict")
            per_market: dict[str, tuple[float, float]] = {}
            for mkt, fees in custom.items():
                if not isinstance(fees, dict):
                    raise ValueError(f"arcus.custom_schedule_tier0_bps[{mkt!r}] must be a dict")
                pair = _extract_pair(
                    fees.get("maker_fee_bps"),
                    fees.get("taker_fee_bps"),
                    f"arcus.custom.{mkt}",
                )
                if pair is None:
                    raise ValueError(f"arcus.custom.{mkt}: missing maker/taker fee bps")
                per_market[str(mkt)] = pair

            entry["tier0_crypto_fees"] = tier0_crypto
            entry["tier0_rwa_fees"] = tier0_rwa
            entry["per_market_fees"] = per_market

        if vname == "lighter":
            pub_pair = _extract_pair(
                payload.get("public_schedule_maker_fee_bps"),
                payload.get("public_schedule_taker_fee_bps"),
                "lighter.public_schedule",
            )
            if pub_pair is not None:
                entry["public_schedule_fees"] = pub_pair

        if vname == "trade_xyz":
            gm_pair = _extract_pair(
                payload.get("growth_mode_maker_fee_bps"),
                payload.get("growth_mode_taker_fee_bps"),
                "trade_xyz.growth_mode",
            )
            st_pair = _extract_pair(
                payload.get("standard_maker_fee_bps"),
                payload.get("standard_taker_fee_bps"),
                "trade_xyz.standard",
            )
            if gm_pair is not None:
                entry["growth_mode_fees"] = gm_pair
            if st_pair is not None:
                entry["standard_fees"] = st_pair
            gm_summary = payload.get("market_growth_mode_summary")
            if isinstance(gm_summary, dict):
                entry["market_growth_mode_summary"] = dict(gm_summary)

        parsed_venues[vname] = entry

    return {
        "loaded": True,
        "evidence_name": path.name,
        "evidence_sha256": sha,
        "schema_version": data.get("schema_version"),
        "audited_at_utc": data.get("audited_at_utc"),
        "venues": parsed_venues,
    }


def resolve_venue_fee(
    venue: str,
    domain: str,
    *,
    default_maker_bps: float,
    default_taker_bps: float,
    default_status: str,
    evidence: dict[str, Any] | None = None,
    raw_market: str | None = None,
    arcus_fee_tier: str = "level_0",
    trade_xyz_fee_mode: str = "growth_mode",
) -> tuple[float, float, str, str]:
    """Resolve `(maker_fee_bps, taker_fee_bps, fee_status, provenance_source)` for a venue/market."""
    if domain not in ("crypto", "rwa"):
        raise ValueError(f"Unsupported domain {domain!r}; expected 'crypto' or 'rwa'")
    if arcus_fee_tier not in ARCUS_FEE_TIERS:
        raise ValueError(f"Unsupported arcus_fee_tier {arcus_fee_tier!r}")
    if trade_xyz_fee_mode not in TRADE_XYZ_FEE_MODES:
        raise ValueError(f"Unsupported trade_xyz_fee_mode {trade_xyz_fee_mode!r}")

    ev = evidence or {"loaded": False, "venues": {}}
    ev_loaded = bool(ev.get("loaded"))
    ev_venues = ev.get("venues") or {}
    entry = ev_venues.get(venue) if ev_loaded and isinstance(ev_venues, dict) else None

    if entry is not None:
        pair = _confirmed_account_pair(entry, venue)
        if pair is not None:
            return pair[0], pair[1], "verified_account_tier", "evidence_account_tier"

    if venue == "arcus":
        tier_cfg = ARCUS_FEE_TIERS[arcus_fee_tier]
        mk, tk = tier_cfg[domain]
        status = str(tier_cfg["status"])
        if entry is not None:
            ev_status = entry.get("account_fee_status") or status
            if arcus_fee_tier == "level_0":
                per_market = entry.get("per_market_fees") or {}
                if raw_market and raw_market in per_market:
                    pm_mk, pm_tk = per_market[raw_market]
                    return float(pm_mk), float(pm_tk), str(ev_status), "evidence_public_tier0_custom_market"
                tier0_pair = entry.get("tier0_crypto_fees" if domain == "crypto" else "tier0_rwa_fees")
                if tier0_pair is not None:
                    return float(tier0_pair[0]), float(tier0_pair[1]), str(ev_status), "evidence_public_tier0_schedule"
                return float(mk), float(tk), str(ev_status), "evidence_status_with_level_0_default"
            return float(mk), float(tk), "account_evidence_required", "sensitivity_level_4_unverified"
        return float(mk), float(tk), status, f"default_arcus_{arcus_fee_tier}"

    if venue == "trade_xyz":
        mk, tk = TRADE_XYZ_FEE_MODES[trade_xyz_fee_mode]
        status = str(default_status)
        if entry is not None:
            ev_status = str(entry.get("account_fee_status") or status)
            mode_key = "growth_mode_fees" if trade_xyz_fee_mode == "growth_mode" else "standard_fees"
            mode_pair = entry.get(mode_key)
            if mode_pair is not None:
                return float(mode_pair[0]), float(mode_pair[1]), ev_status, f"evidence_trade_xyz_{trade_xyz_fee_mode}"
            return float(mk), float(tk), ev_status, "evidence_status_only"
        return float(mk), float(tk), status, f"default_trade_xyz_{trade_xyz_fee_mode}"

    mk = float(default_maker_bps)
    tk = float(default_taker_bps)
    status = str(default_status)
    if entry is not None:
        ev_status = str(entry.get("account_fee_status") or status)
        if venue == "lighter" and entry.get("public_schedule_fees") is not None:
            pub_mk, pub_tk = entry["public_schedule_fees"]
            return float(pub_mk), float(pub_tk), ev_status, "evidence_public_schedule"
        return mk, tk, ev_status, "evidence_status_only"
    return mk, tk, status, "default"


def build_fee_provenance(
    evidence: dict[str, Any] | None,
    *,
    arcus_fee_tier: str = "level_0",
    trade_xyz_fee_mode: str | None = None,
    venues_in_scope: Sequence[str] | None = None,
) -> dict[str, Any]:
    """Build sanitized OPT-01 fee provenance metadata (no local absolute paths)."""
    ev = evidence or {}
    out: dict[str, Any] = {
        "venue_capability_snapshot_version": VENUE_CAPABILITY_SNAPSHOT_VERSION,
        "fee_evidence_loaded": bool(ev.get("loaded", False)),
        "fee_evidence_name": ev.get("evidence_name"),
        "fee_evidence_sha256": ev.get("evidence_sha256"),
        "fee_evidence_schema_version": ev.get("schema_version"),
        "fee_evidence_audited_at_utc": ev.get("audited_at_utc"),
        "arcus_fee_tier": arcus_fee_tier,
    }
    if trade_xyz_fee_mode is not None:
        out["trade_xyz_fee_mode"] = trade_xyz_fee_mode
    ev_venues = ev.get("venues") or {}
    include_xyz = "trade_xyz" in ev_venues and (
        venues_in_scope is None or "trade_xyz" in set(venues_in_scope) or trade_xyz_fee_mode is not None
    )
    if include_xyz and isinstance(ev_venues.get("trade_xyz"), dict):
        gm_summary = ev_venues["trade_xyz"].get("market_growth_mode_summary")
        if isinstance(gm_summary, dict):
            out["trade_xyz_market_growth_mode_summary"] = dict(gm_summary)
    return out
