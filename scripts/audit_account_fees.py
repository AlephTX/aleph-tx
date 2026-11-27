#!/usr/bin/env python3
"""Read-only 9-venue account fee-tier evidence auditor (`OPT-01`).

Strictly offline by default: refuses to run without `--live`, `--fixture`, or
`--self-test`. Never loads private keys or API secrets, never prints or writes
wallet addresses, pubkeys, account indices, balances, positions, or user volume
arrays, and enforces a hard redaction validator before emitting any JSON.
"""

from __future__ import annotations

import argparse
from datetime import datetime, timezone
from decimal import Decimal, InvalidOperation
import json
import os
from pathlib import Path
import re
import ssl
import sys
from typing import Any, Callable, Iterable
from urllib.error import HTTPError, URLError
from urllib.request import HTTPRedirectHandler, HTTPSHandler, Request, build_opener

REPO_ROOT = Path(__file__).resolve().parent.parent
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

try:
    from scripts.verify_venue_readiness import (
        BASE58_RE,
        EVM_ADDR_RE,
        extract_public_monitor_config,
    )
except ImportError:
    from verify_venue_readiness import (
        BASE58_RE,
        EVM_ADDR_RE,
        extract_public_monitor_config,
    )

SCHEMA_VERSION = "aleph_account_fee_evidence_v1"
ACCOUNT_FEE_STATUS_VERIFIED = "verified_account_tier"
ACCOUNT_FEE_STATUS_PUBLIC_TIER0 = "public_tier0_schedule_only"
ACCOUNT_FEE_STATUS_LIGHTER_PUBLIC_UNVERIFIED = "public_schedule_only_unverified_account_tier"
ACCOUNT_FEE_STATUS_UNKNOWN = "unknown_requires_account_evidence"

SUPPORTED_VENUES: tuple[str, ...] = (
    "arcus",
    "hyperliquid",
    "trade_xyz",
    "lighter",
    "backpack",
    "extended",
    "bulk",
    "okx",
    "binance",
)
_VENUE_INDEX: dict[str, int] = {v: i for i, v in enumerate(SUPPORTED_VENUES)}

MAX_RESPONSE_BYTES = 16 * 1024 * 1024
DEFAULT_TIMEOUT_SECONDS = 15.0
BPS_PER_UNIT = Decimal("10000")

HL_INFO_URL = "https://api.hyperliquid.xyz/info"
LIGHTER_ACCOUNT_URL = "https://mainnet.zklighter.elliot.ai/api/v1/account"
ARCUS_FEE_SCHEDULES_URL = "https://api.arcus.xyz/v1/markets/feeSchedules"

_HEX_KEY_64_RE = re.compile(r"(?<![0-9a-fA-F])(?:0x)?[0-9a-fA-F]{64}(?![0-9a-fA-F])")
_BASE58_TOKEN_RE = re.compile(r"(?<![A-Za-z0-9_])[1-9A-HJ-NP-Za-km-z]{32,44}(?![A-Za-z0-9_])")
_ISO_UTC_TS_RE = re.compile(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})$")

_PROHIBITED_EXACT_KEYS: frozenset[str] = frozenset(
    {
        "account_index",
        "accountindex",
        "master_account_index",
        "sub_account_index",
        "index",
        "l1_address",
        "l1address",
        "address",
        "wallet",
        "wallet_address",
        "walletaddress",
        "pubkey",
        "public_key",
        "publickey",
        "user",
        "username",
        "owner",
        "deployer",
        "fee_recipient",
        "feerecipient",
        "oracle_updater",
        "oracleupdater",
        "staking_user",
        "stakinguser",
        "staking_link",
        "stakinglink",
        "withdrawable",
        "withdrawable_usd",
        "account_value",
        "accountvalue",
        "account_value_usd",
        "margin_used",
        "marginused",
        "total_margin_used",
        "totalmarginused",
        "collateral",
        "available_balance",
        "availablebalance",
        "total_asset_value",
        "totalassetvalue",
        "cross_asset_value",
        "crossassetvalue",
        "positions",
        "asset_positions",
        "assetpositions",
        "open_orders",
        "openorders",
        "daily_user_vlm",
        "dailyuservlm",
        "total_raw_usd",
        "totalrawusd",
        "total_ntl_pos",
        "totalntlpos",
        "shares",
        "private_key",
        "privatekey",
        "api_key",
        "apikey",
        "secret",
        "api_secret",
        "apisecret",
        "token",
        "api_token",
        "auth_token",
        "control_token",
        "access_token",
        "bearer_token",
        "secret_token",
        "session_token",
        "mnemonic",
        "passphrase",
        "seed",
        "password",
        "signature",
    }
)

_PROHIBITED_KEY_FRAGMENTS: tuple[str, ...] = (
    "private_key",
    "privatekey",
    "api_key",
    "apikey",
    "api_secret",
    "apisecret",
    "client_secret",
    "webhook_secret",
    "secret_key",
    "mnemonic",
    "passphrase",
    "withdrawable",
    "accountvalue",
    "marginused",
    "dailyuservlm",
    "totalrawusd",
    "openorders",
    "l1_address",
    "l1address",
    "account_index",
    "accountindex",
)


def _is_prohibited_key(norm_key: str) -> bool:
    if norm_key in _PROHIBITED_EXACT_KEYS:
        return True
    if any(frag in norm_key for frag in _PROHIBITED_KEY_FRAGMENTS):
        return True
    if norm_key.startswith("secret_") or norm_key.endswith("_secret"):
        return True
    if norm_key.startswith("token_") or norm_key.endswith("_token"):
        return True
    return False

_BENIGN_SHORT_INT_KEYS: frozenset[str] = frozenset(
    {
        "total_venues",
        "error_venues",
        "market_count",
        "custom_schedule_market_count",
        "count",
        "account_type",
        "active_markets",
        "growth_mode_enabled_count",
        "growth_mode_disabled_count",
        "growth_mode_unspecified_count",
        "current_maker_fee_tick",
        "current_taker_fee_tick",
        "verified_level",
        "fee_discount_ppm",
    }
)

JsonRequester = Callable[[str, dict[str, Any] | None], Any]


class _NoRedirectHandler(HTTPRedirectHandler):
    """Fail closed rather than silently following HTTP redirects."""

    def redirect_request(self, req: Request, fp: Any, code: int, msg: str, headers: Any, newurl: str) -> None:
        raise HTTPError(req.full_url, code, f"unexpected redirect ({msg})", headers, fp)


def _utc_now_iso() -> str:
    return datetime.now(timezone.utc).isoformat()


def _build_ssl_context() -> ssl.SSLContext:
    # Only invoked in --live mode; honors SSL_CERT_FILE for macOS Python installs.
    cafile = os.environ.get("SSL_CERT_FILE") or None
    if cafile:
        return ssl.create_default_context(cafile=cafile)
    return ssl.create_default_context()


def _live_request_json(
    url: str,
    body: dict[str, Any] | None = None,
    *,
    timeout: float = DEFAULT_TIMEOUT_SECONDS,
) -> Any:
    if not url.startswith("https://"):
        raise ValueError(f"only HTTPS endpoints are allowed: {url}")
    headers = {
        "Accept": "application/json",
        "User-Agent": "AlephTX-AccountFeeAudit/1.0",
    }
    data: bytes | None = None
    method = "GET"
    if body is not None:
        data = json.dumps(body, separators=(",", ":")).encode("utf-8")
        headers["Content-Type"] = "application/json"
        method = "POST"
    req = Request(url, data=data, headers=headers, method=method)
    opener = build_opener(_NoRedirectHandler(), HTTPSHandler(context=_build_ssl_context()))
    with opener.open(req, timeout=timeout) as resp:
        if resp.status != 200:
            raise ValueError(f"unexpected HTTP status {resp.status} from {url.split('?', 1)[0]}")
        raw = resp.read(MAX_RESPONSE_BYTES + 1)
    if len(raw) > MAX_RESPONSE_BYTES:
        raise ValueError(f"response from {url.split('?', 1)[0]} exceeded {MAX_RESPONSE_BYTES} bytes")
    return json.loads(raw.decode("utf-8"))


def _parse_decimal(raw: Any, field_name: str) -> Decimal:
    if raw is None or isinstance(raw, bool):
        raise ValueError(f"missing or invalid decimal for {field_name}")
    try:
        val = Decimal(str(raw).strip())
    except (InvalidOperation, ValueError) as exc:
        raise ValueError(f"invalid decimal {raw!r} for {field_name}") from exc
    if not val.is_finite():
        raise ValueError(f"non-finite decimal {raw!r} for {field_name}")
    return val


def _parse_optional_decimal(raw: Any, field_name: str) -> Decimal | None:
    if raw is None or raw == "":
        return None
    return _parse_decimal(raw, field_name)


def _format_decimal(val: Decimal) -> str:
    if val == 0:
        return "0"
    norm = val.normalize()
    text = format(norm, "f")
    if "." in text:
        text = text.rstrip("0").rstrip(".")
    return text


def _reject_error_envelope(payload: Any, label: str) -> None:
    if isinstance(payload, dict):
        if payload.get("error"):
            raise ValueError(f"{label} returned error: {payload.get('error')}")
        status = payload.get("status")
        if isinstance(status, str) and status.lower() in ("err", "error", "failed"):
            raise ValueError(f"{label} returned error status: {status}")


def _sanitize_error_text(text: str, forbidden_literals: Iterable[str] = ()) -> str:
    cleaned = EVM_ADDR_RE.sub("<redacted_evm>", text)
    cleaned = _HEX_KEY_64_RE.sub("<redacted_hex>", cleaned)
    cleaned = _BASE58_TOKEN_RE.sub("<redacted_pubkey>", cleaned)
    cleaned = re.sub(r"(value=)[0-9]+", r"\1<redacted>", cleaned)
    for lit in forbidden_literals:
        item = str(lit).strip()
        if len(item) >= 2 and not item.isdigit():
            cleaned = re.sub(re.escape(item), "<redacted>", cleaned, flags=re.IGNORECASE)
        elif len(item) >= 5 and item.isdigit():
            cleaned = cleaned.replace(item, "<redacted>")
    return cleaned


def compute_hip3_execution_fees_bps(
    base_maker_bps: Decimal,
    base_taker_bps: Decimal,
    deployer_fee_scale: Decimal,
    growth_mode_enabled: bool,
    *,
    is_verified_core_crypto_asset: bool = False,
) -> dict[str, Any]:
    """Compute effective HIP-3 fee schedule matching `src/venues.rs::compute_hip3_execution_fees`."""
    if base_maker_bps < Decimal("0") or base_taker_bps < Decimal("0"):
        raise ValueError("HIP-3 base fees must be nonnegative")
    if deployer_fee_scale < Decimal("0") or deployer_fee_scale > Decimal("5"):
        raise ValueError("HIP-3 deployer_fee_scale must lie in [0, 5]")
    if growth_mode_enabled and is_verified_core_crypto_asset:
        raise ValueError("HIP-3 growth mode excludes verified core crypto assets")
    scale_factor = (
        Decimal("1") + deployer_fee_scale
        if deployer_fee_scale < Decimal("1")
        else Decimal("2") * deployer_fee_scale
    )
    growth_factor = Decimal("0.10") if growth_mode_enabled else Decimal("1")
    eff_maker = (base_maker_bps * scale_factor * growth_factor).normalize()
    eff_taker = (base_taker_bps * scale_factor * growth_factor).normalize()
    return {
        "scale_factor": _format_decimal(scale_factor),
        "growth_factor": _format_decimal(growth_factor),
        "effective_maker_fee_bps": _format_decimal(eff_maker),
        "effective_taker_fee_bps": _format_decimal(eff_taker),
        "tier_volume_credit_weight": _format_decimal(growth_factor),
    }


def _sanitize_hl_fee_schedule(raw_schedule: Any) -> dict[str, Any] | None:
    if raw_schedule is None:
        return None
    if not isinstance(raw_schedule, dict):
        raise ValueError("Hyperliquid feeSchedule must be an object")
    out: dict[str, Any] = {}
    for src_key, dst_key in (
        ("cross", "base_cross_bps"),
        ("add", "base_add_bps"),
        ("spotCross", "base_spot_cross_bps"),
        ("spotAdd", "base_spot_add_bps"),
    ):
        val = _parse_optional_decimal(raw_schedule.get(src_key), f"feeSchedule.{src_key}")
        if val is not None:
            out[dst_key] = _format_decimal(val * BPS_PER_UNIT)
    ref_disc = _parse_optional_decimal(raw_schedule.get("referralDiscount"), "feeSchedule.referralDiscount")
    if ref_disc is not None:
        out["referral_discount"] = _format_decimal(ref_disc)

    raw_tiers = raw_schedule.get("tiers")
    if isinstance(raw_tiers, dict):
        clean_tiers: dict[str, list[dict[str, str]]] = {}
        for tier_kind in ("vip", "mm"):
            rows = raw_tiers.get(tier_kind)
            if isinstance(rows, list):
                parsed_rows: list[dict[str, str]] = []
                for idx, row in enumerate(rows):
                    if not isinstance(row, dict):
                        continue
                    tier_entry: dict[str, str] = {}
                    for k in ("ntlCutoff", "makerFractionCutoff"):
                        v = _parse_optional_decimal(row.get(k), f"feeSchedule.tiers.{tier_kind}[{idx}].{k}")
                        if v is not None:
                            tier_entry[k] = _format_decimal(v)
                    for k, bps_k in (
                        ("cross", "cross_bps"),
                        ("add", "add_bps"),
                        ("spotCross", "spot_cross_bps"),
                        ("spotAdd", "spot_add_bps"),
                    ):
                        v = _parse_optional_decimal(row.get(k), f"feeSchedule.tiers.{tier_kind}[{idx}].{k}")
                        if v is not None:
                            tier_entry[bps_k] = _format_decimal(v * BPS_PER_UNIT)
                    if tier_entry:
                        parsed_rows.append(tier_entry)
                if parsed_rows:
                    clean_tiers[tier_kind] = parsed_rows
        if clean_tiers:
            out["tiers"] = clean_tiers

    raw_staking = raw_schedule.get("stakingDiscountTiers")
    if isinstance(raw_staking, list):
        clean_staking: list[dict[str, str]] = []
        for idx, row in enumerate(raw_staking):
            if not isinstance(row, dict):
                continue
            item: dict[str, str] = {}
            bps_max = _parse_optional_decimal(
                row.get("bpsOfMaxSupply"), f"feeSchedule.stakingDiscountTiers[{idx}].bpsOfMaxSupply"
            )
            disc = _parse_optional_decimal(row.get("discount"), f"feeSchedule.stakingDiscountTiers[{idx}].discount")
            if bps_max is not None:
                item["bps_of_max_supply"] = _format_decimal(bps_max)
            if disc is not None:
                item["discount"] = _format_decimal(disc)
            if item:
                clean_staking.append(item)
        if clean_staking:
            out["staking_discount_tiers"] = clean_staking
    return out or None


def _sanitize_hl_staking_discount(raw: Any) -> Any:
    if raw is None or raw == "":
        return None
    if isinstance(raw, dict):
        out: dict[str, str] = {}
        bps_max = _parse_optional_decimal(raw.get("bpsOfMaxSupply"), "activeStakingDiscount.bpsOfMaxSupply")
        disc = _parse_optional_decimal(raw.get("discount"), "activeStakingDiscount.discount")
        if bps_max is not None:
            out["bps_of_max_supply"] = _format_decimal(bps_max)
        if disc is not None:
            out["discount"] = _format_decimal(disc)
        return out or None
    val = _parse_decimal(raw, "activeStakingDiscount")
    return _format_decimal(val)


def _parse_hl_user_fees(payload: Any, *, venue_label: str) -> dict[str, Any]:
    _reject_error_envelope(payload, venue_label)
    if not isinstance(payload, dict):
        raise ValueError(f"{venue_label} userFees must be a JSON object")
    cross_rate = _parse_decimal(payload.get("userCrossRate"), f"{venue_label}.userCrossRate")
    add_rate = _parse_decimal(payload.get("userAddRate"), f"{venue_label}.userAddRate")
    taker_bps = (cross_rate * BPS_PER_UNIT).normalize()
    maker_bps = (add_rate * BPS_PER_UNIT).normalize()
    ref_disc = _parse_optional_decimal(payload.get("activeReferralDiscount"), f"{venue_label}.activeReferralDiscount")
    staking_disc = _sanitize_hl_staking_discount(payload.get("activeStakingDiscount"))
    fee_schedule = _sanitize_hl_fee_schedule(payload.get("feeSchedule"))
    return {
        "cross_rate_dec": cross_rate,
        "add_rate_dec": add_rate,
        "taker_bps_dec": taker_bps,
        "maker_bps_dec": maker_bps,
        "active_referral_discount": _format_decimal(ref_disc) if ref_disc is not None else None,
        "active_staking_discount": staking_disc,
        "fee_schedule": fee_schedule,
    }


def _probe_hyperliquid(
    address: str | None,
    req: JsonRequester | None,
    *,
    fixture_payload: Any = None,
) -> dict[str, Any]:
    if fixture_payload is not None:
        raw_fees = fixture_payload["userFees"] if isinstance(fixture_payload, dict) and "userFees" in fixture_payload else fixture_payload
    else:
        if not address:
            return {
                "venue": "hyperliquid",
                "configured": False,
                "status": "unconfigured",
                "account_fee_status": ACCOUNT_FEE_STATUS_UNKNOWN,
                "maker_fee_bps": None,
                "taker_fee_bps": None,
            }
        if not EVM_ADDR_RE.fullmatch(address):
            raise ValueError("invalid Hyperliquid EVM address")
        assert req is not None
        raw_fees = req(HL_INFO_URL, {"type": "userFees", "user": address})

    parsed = _parse_hl_user_fees(raw_fees, venue_label="hyperliquid")
    return {
        "venue": "hyperliquid",
        "configured": True,
        "status": "ok",
        "account_fee_status": ACCOUNT_FEE_STATUS_VERIFIED,
        "source_endpoints": [f"{HL_INFO_URL}#type=userFees"],
        "maker_fee_bps": _format_decimal(parsed["maker_bps_dec"]),
        "taker_fee_bps": _format_decimal(parsed["taker_bps_dec"]),
        "user_add_rate_decimal": _format_decimal(parsed["add_rate_dec"]),
        "user_cross_rate_decimal": _format_decimal(parsed["cross_rate_dec"]),
        "active_referral_discount": parsed["active_referral_discount"],
        "active_staking_discount": parsed["active_staking_discount"],
        "fee_schedule": parsed["fee_schedule"],
    }


def _extract_xyz_dex_scale(perp_dexs: Any) -> Decimal | None:
    _reject_error_envelope(perp_dexs, "trade_xyz.perpDexs")
    if not isinstance(perp_dexs, list) or not perp_dexs or perp_dexs[0] is not None:
        raise ValueError("trade_xyz perpDexs must be a list starting with null")
    matches = [item for item in perp_dexs if isinstance(item, dict) and item.get("name") == "xyz"]
    if len(matches) != 1:
        raise ValueError("trade_xyz DEX 'xyz' missing or duplicated in perpDexs")
    raw_scale = matches[0].get("deployerFeeScale")
    if raw_scale is None or raw_scale == "":
        return None
    scale = _parse_decimal(raw_scale, "trade_xyz.perpDexs.deployerFeeScale")
    if scale < Decimal("0") or scale > Decimal("5"):
        raise ValueError("trade_xyz deployerFeeScale must lie in [0, 5]")
    return scale


def _summarize_xyz_meta_ctxs(
    meta_ctxs: Any,
    *,
    dex_scale: Decimal | None,
    base_maker: Decimal,
    base_taker: Decimal,
) -> dict[str, Any]:
    _reject_error_envelope(meta_ctxs, "trade_xyz.metaAndAssetCtxs")
    if not isinstance(meta_ctxs, list) or len(meta_ctxs) != 2:
        raise ValueError("trade_xyz metaAndAssetCtxs must be [meta, assetCtxs]")
    meta, ctxs = meta_ctxs
    if not isinstance(meta, dict) or not isinstance(meta.get("universe"), list) or not isinstance(ctxs, list):
        raise ValueError("trade_xyz metaAndAssetCtxs missing universe or ctxs")
    if len(meta["universe"]) != len(ctxs):
        raise ValueError("trade_xyz universe/ctxs length mismatch")

    active_count = 0
    enabled_count = 0
    disabled_count = 0
    unspecified_count = 0
    observed_scales: dict[str, Decimal] = {}
    asset_entries: list[tuple[str, str | None, bool, Decimal]] = []
    effective_pairs: set[tuple[str, str]] = set()

    for asset in meta["universe"]:
        if not isinstance(asset, dict):
            raise ValueError("trade_xyz universe entry must be an object")
        if asset.get("isDelisted") is True:
            continue
        symbol = asset.get("name")
        if not isinstance(symbol, str) or not symbol.startswith("xyz:"):
            raise ValueError(f"unexpected trade_xyz asset name {symbol!r}")
        active_count += 1
        growth_mode = asset.get("growthMode")
        if growth_mode == "enabled":
            enabled_count += 1
            growth_enabled = True
        elif growth_mode == "disabled":
            disabled_count += 1
            growth_enabled = False
        elif growth_mode is None:
            unspecified_count += 1
            growth_enabled = False
        else:
            raise ValueError(f"unknown growthMode {growth_mode!r} for {symbol}")

        asset_scale_raw = asset.get("deployerFeeScale")
        if asset_scale_raw is not None and asset_scale_raw != "":
            eff_scale = _parse_decimal(asset_scale_raw, f"{symbol}.deployerFeeScale")
        elif dex_scale is not None:
            eff_scale = dex_scale
        else:
            raise ValueError(f"{symbol} missing deployerFeeScale and perpDexs has no dex deployerFeeScale")
        if eff_scale < Decimal("0") or eff_scale > Decimal("5"):
            raise ValueError(f"{symbol} deployerFeeScale out of [0, 5]")
        eff_scale_str = _format_decimal(eff_scale)
        observed_scales[eff_scale_str] = eff_scale
        asset_entries.append((symbol, growth_mode, growth_enabled, eff_scale))
        sched = compute_hip3_execution_fees_bps(base_maker, base_taker, eff_scale, growth_enabled)
        effective_pairs.add((sched["effective_maker_fee_bps"], sched["effective_taker_fee_bps"]))

    if active_count == 0:
        raise ValueError("trade_xyz has zero active assets in metaAndAssetCtxs")

    reference_scale = dex_scale if dex_scale is not None else (
        next(iter(observed_scales.values())) if len(observed_scales) == 1 else None
    )
    overrides: list[dict[str, Any]] = []
    for symbol, growth_mode, growth_enabled, eff_scale in asset_entries:
        if reference_scale is None or eff_scale != reference_scale:
            sched = compute_hip3_execution_fees_bps(base_maker, base_taker, eff_scale, growth_enabled)
            overrides.append(
                {
                    "symbol": symbol,
                    "deployer_fee_scale": _format_decimal(eff_scale),
                    "growth_mode": growth_mode or "disabled",
                    "effective_maker_fee_bps": sched["effective_maker_fee_bps"],
                    "effective_taker_fee_bps": sched["effective_taker_fee_bps"],
                }
            )

    unambiguous_pair: tuple[str, str] | None = next(iter(effective_pairs)) if len(effective_pairs) == 1 else None
    return {
        "effective_dex_scale": reference_scale,
        "unambiguous_pair": unambiguous_pair,
        "summary": {
            "active_markets": active_count,
            "growth_mode_enabled_count": enabled_count,
            "growth_mode_disabled_count": disabled_count,
            "growth_mode_unspecified_count": unspecified_count,
            "observed_deployer_fee_scales": sorted(observed_scales.keys()),
            "per_market_scale_overrides": overrides,
        },
    }


def _probe_trade_xyz(
    address: str | None,
    req: JsonRequester | None,
    *,
    uses_fallback_hl_address: bool | None = None,
    fixture_payload: Any = None,
) -> dict[str, Any]:
    meta_ctxs: Any = None
    if fixture_payload is not None:
        if not isinstance(fixture_payload, dict) or "userFees" not in fixture_payload or "perpDexs" not in fixture_payload:
            raise ValueError("trade_xyz fixture must provide 'userFees' and 'perpDexs'")
        raw_fees = fixture_payload["userFees"]
        perp_dexs = fixture_payload["perpDexs"]
        meta_ctxs = fixture_payload.get("metaAndAssetCtxs")
    else:
        if not address:
            return {
                "venue": "trade_xyz",
                "configured": False,
                "status": "unconfigured",
                "account_fee_status": ACCOUNT_FEE_STATUS_UNKNOWN,
                "maker_fee_bps": None,
                "taker_fee_bps": None,
            }
        if not EVM_ADDR_RE.fullmatch(address):
            raise ValueError("invalid TradeXYZ EVM address")
        assert req is not None
        raw_fees = req(HL_INFO_URL, {"type": "userFees", "user": address})
        perp_dexs = req(HL_INFO_URL, {"type": "perpDexs"})
        meta_ctxs = req(HL_INFO_URL, {"type": "metaAndAssetCtxs", "dex": "xyz"})

    parsed = _parse_hl_user_fees(raw_fees, venue_label="trade_xyz")
    base_maker = parsed["maker_bps_dec"]
    base_taker = parsed["taker_bps_dec"]
    dex_scale = _extract_xyz_dex_scale(perp_dexs)

    source_endpoints = [f"{HL_INFO_URL}#type=userFees", f"{HL_INFO_URL}#type=perpDexs"]
    default_maker: str | None = None
    default_taker: str | None = None
    meta_summary: dict[str, Any] | None = None
    default_note: str
    effective_scale = dex_scale
    if meta_ctxs is not None:
        source_endpoints.append(f"{HL_INFO_URL}#type=metaAndAssetCtxs&dex=xyz")
        meta_res = _summarize_xyz_meta_ctxs(
            meta_ctxs,
            dex_scale=dex_scale,
            base_maker=base_maker,
            base_taker=base_taker,
        )
        effective_scale = meta_res["effective_dex_scale"]
        meta_summary = meta_res["summary"]
        if meta_res["unambiguous_pair"] is not None:
            default_maker, default_taker = meta_res["unambiguous_pair"]
            default_note = "All active xyz markets share identical deployer_fee_scale and growthMode."
        else:
            default_note = "Per-market fee varies across active xyz markets by growthMode and/or deployerFeeScale."
    else:
        if effective_scale is None:
            raise ValueError("trade_xyz deployerFeeScale missing from both perpDexs and metaAndAssetCtxs")
        default_note = "Per-market growthMode not inspected; use standard_* vs growth_mode_* per asset."

    std = compute_hip3_execution_fees_bps(base_maker, base_taker, effective_scale, False) if effective_scale is not None else None
    growth = compute_hip3_execution_fees_bps(base_maker, base_taker, effective_scale, True) if effective_scale is not None else None

    out: dict[str, Any] = {
        "venue": "trade_xyz",
        "configured": True,
        "status": "ok",
        "account_fee_status": ACCOUNT_FEE_STATUS_VERIFIED,
        "source_endpoints": source_endpoints,
        "base_maker_fee_bps": _format_decimal(base_maker),
        "base_taker_fee_bps": _format_decimal(base_taker),
        "deployer_fee_scale": _format_decimal(effective_scale) if effective_scale is not None else None,
        "scale_factor": std["scale_factor"] if std is not None else None,
        "standard_maker_fee_bps": std["effective_maker_fee_bps"] if std is not None else None,
        "standard_taker_fee_bps": std["effective_taker_fee_bps"] if std is not None else None,
        "growth_mode_discount_multiplier": growth["growth_factor"] if growth is not None else "0.1",
        "growth_mode_maker_fee_bps": growth["effective_maker_fee_bps"] if growth is not None else None,
        "growth_mode_taker_fee_bps": growth["effective_taker_fee_bps"] if growth is not None else None,
        "maker_fee_bps": default_maker,
        "taker_fee_bps": default_taker,
        "default_fee_note": default_note,
        "active_referral_discount": parsed["active_referral_discount"],
        "active_staking_discount": parsed["active_staking_discount"],
        "fee_schedule": parsed["fee_schedule"],
        "formula_provenance": (
            "src/venues.rs::compute_hip3_execution_fees: "
            "scale_factor = (1 + deployer_fee_scale) if deployer_fee_scale < 1 else (2 * deployer_fee_scale); "
            "growth_factor = 0.10 if growth_mode == 'enabled' else 1; "
            "effective_bps = base_hl_bps * scale_factor * growth_factor (nonnegative base_hl_bps required)"
        ),
    }
    if uses_fallback_hl_address is not None:
        out["uses_fallback_hl_address"] = uses_fallback_hl_address
    if meta_summary is not None:
        out["market_growth_mode_summary"] = meta_summary
    return out


def _probe_lighter(
    index: str | None,
    req: JsonRequester | None,
    *,
    fixture_payload: Any = None,
    discovered_secrets: list[str] | None = None,
) -> dict[str, Any]:
    expected_index: int | None = None
    if fixture_payload is not None:
        if isinstance(fixture_payload, dict) and "response" in fixture_payload:
            raw = fixture_payload["response"]
            if fixture_payload.get("expected_index") is not None:
                expected_index = int(fixture_payload["expected_index"])
        else:
            raw = fixture_payload
    else:
        if not index:
            return {
                "venue": "lighter",
                "configured": False,
                "status": "unconfigured",
                "account_fee_status": ACCOUNT_FEE_STATUS_UNKNOWN,
                "maker_fee_bps": None,
                "taker_fee_bps": None,
            }
        if not re.fullmatch(r"[0-9]{1,19}", index) or not (0 < int(index) <= 2**63 - 1):
            raise ValueError("invalid Lighter account index")
        expected_index = int(index)
        assert req is not None
        raw = req(f"{LIGHTER_ACCOUNT_URL}?by=index&value={index}", None)

    _reject_error_envelope(raw, "lighter")
    if not isinstance(raw, dict) or raw.get("code") != 200:
        raise ValueError(f"Lighter account query returned code={raw.get('code') if isinstance(raw, dict) else 'invalid'}")
    accounts = raw.get("accounts")
    if not isinstance(accounts, list) or not all(isinstance(a, dict) for a in accounts):
        raise ValueError("Lighter account response missing accounts list")

    if expected_index is not None:
        matches = [a for a in accounts if a.get("index") == expected_index]
        if len(matches) != 1:
            raise ValueError("Lighter account response missing unique matching account")
        acct = matches[0]
    else:
        if len(accounts) != 1:
            raise ValueError("Lighter fixture must contain exactly one account when expected_index is omitted")
        acct = accounts[0]

    if discovered_secrets is not None:
        if acct.get("index") is not None:
            discovered_secrets.append(str(acct["index"]))
        if isinstance(acct.get("l1_address"), str) and acct["l1_address"].strip():
            discovered_secrets.append(acct["l1_address"].strip())

    acct_type = acct.get("account_type")
    if acct_type is not None and type(acct_type) is not int:
        raise ValueError("Lighter account_type must be an integer when present")
    if acct_type == 0:
        acct_type_label: str | None = "standard_type_0"
    elif acct_type is not None:
        acct_type_label = f"account_type_{acct_type}"
    else:
        acct_type_label = None

    # Only mark verified_account_tier if explicit per-account maker/taker fee fields exist.
    explicit_maker_bps = _parse_optional_decimal(acct.get("maker_fee_bps"), "lighter.account.maker_fee_bps")
    explicit_taker_bps = _parse_optional_decimal(acct.get("taker_fee_bps"), "lighter.account.taker_fee_bps")
    if explicit_maker_bps is None and explicit_taker_bps is None:
        pct_maker = _parse_optional_decimal(acct.get("maker_fee"), "lighter.account.maker_fee")
        pct_taker = _parse_optional_decimal(acct.get("taker_fee"), "lighter.account.taker_fee")
        if pct_maker is not None and pct_taker is not None:
            explicit_maker_bps = (pct_maker * Decimal("100")).normalize()
            explicit_taker_bps = (pct_taker * Decimal("100")).normalize()

    if explicit_maker_bps is not None and explicit_taker_bps is not None:
        return {
            "venue": "lighter",
            "configured": True,
            "status": "ok",
            "account_fee_status": ACCOUNT_FEE_STATUS_VERIFIED,
            "source_endpoints": [LIGHTER_ACCOUNT_URL],
            "account_type": acct_type,
            "account_type_label": acct_type_label,
            "maker_fee_bps": _format_decimal(explicit_maker_bps),
            "taker_fee_bps": _format_decimal(explicit_taker_bps),
            "public_schedule_maker_fee_bps": "0",
            "public_schedule_taker_fee_bps": "0",
            "note": "Verified from explicit per-account fee fields on Lighter account response.",
        }

    return {
        "venue": "lighter",
        "configured": True,
        "status": "ok",
        "account_fee_status": ACCOUNT_FEE_STATUS_LIGHTER_PUBLIC_UNVERIFIED,
        "source_endpoints": [LIGHTER_ACCOUNT_URL],
        "account_type": acct_type,
        "account_type_label": acct_type_label,
        "maker_fee_bps": None,
        "taker_fee_bps": None,
        "public_schedule_maker_fee_bps": "0",
        "public_schedule_taker_fee_bps": "0",
        "note": (
            "Public /api/v1/account does not expose account fee ticks; signed /api/v1/accountLimits "
            "is required to verify Standard (0/0 bps) vs Plus/Premium fee overrides per src/venues.rs."
        ),
    }


def _parse_arcus_tiers(rows: Any, label: str) -> list[dict[str, Any]]:
    if not isinstance(rows, list) or not rows:
        raise ValueError(f"Arcus {label} must be a non-empty list")
    seen: set[int] = set()
    parsed: list[dict[str, Any]] = []
    for row in rows:
        if not isinstance(row, dict):
            raise ValueError(f"Arcus {label} tier entry must be an object")
        level = row.get("level")
        if type(level) is not int or level < 0 or level > 32:
            raise ValueError(f"invalid Arcus tier level in {label}")
        if level in seen:
            raise ValueError(f"duplicate Arcus tier level {level} in {label}")
        seen.add(level)
        maker_ppm = row.get("makerFeePpm")
        taker_ppm = row.get("takerFeePpm")
        if type(maker_ppm) is not int or type(taker_ppm) is not int:
            raise ValueError(f"invalid Arcus fee ppm in {label}")
        parsed.append(
            {
                "level": level,
                "maker_fee_bps": _format_decimal(Decimal(maker_ppm) / Decimal(100)),
                "taker_fee_bps": _format_decimal(Decimal(taker_ppm) / Decimal(100)),
            }
        )
    if 0 not in seen:
        raise ValueError(f"Arcus {label} missing tier 0")
    parsed.sort(key=lambda item: item["level"])
    return parsed


def _probe_arcus(
    *,
    account_identifier_configured: bool,
    req: JsonRequester | None,
    fixture_payload: Any = None,
) -> dict[str, Any]:
    if fixture_payload is not None:
        raw = fixture_payload["feeSchedules"] if isinstance(fixture_payload, dict) and isinstance(fixture_payload.get("feeSchedules"), dict) else fixture_payload
    else:
        assert req is not None
        raw = req(ARCUS_FEE_SCHEDULES_URL, None)

    _reject_error_envelope(raw, "arcus")
    if not isinstance(raw, dict) or not isinstance(raw.get("feeSchedules"), list) or not raw["feeSchedules"]:
        raise ValueError("Arcus /v1/markets/feeSchedules must contain a non-empty feeSchedules list")

    seen_ids: set[int] = set()
    seen_names: set[str] = set()
    profile_counts: dict[tuple[str, str], int] = {}
    custom_markets: dict[str, dict[str, str]] = {}
    custom_count = 0

    for row in raw["feeSchedules"]:
        if not isinstance(row, dict):
            raise ValueError("Arcus feeSchedules entry must be an object")
        market_id = row.get("marketId")
        if type(market_id) is not int or market_id < 0 or market_id > 65535:
            raise ValueError("invalid Arcus feeSchedules marketId")
        sym = row.get("marketDisplayName")
        if not isinstance(sym, str) or not sym.strip():
            raise ValueError("missing Arcus feeSchedules marketDisplayName")
        sym = sym.strip()
        if market_id in seen_ids or sym in seen_names:
            raise ValueError(f"duplicate Arcus feeSchedule for {sym}")
        seen_ids.add(market_id)
        seen_names.add(sym)

        tiers = _parse_arcus_tiers(row.get("tiers"), f"feeSchedules[{sym}].tiers")
        l0 = tiers[0]
        pair = (l0["maker_fee_bps"], l0["taker_fee_bps"])
        profile_counts[pair] = profile_counts.get(pair, 0) + 1
        has_custom = row.get("hasCustomSchedule")
        if has_custom is not None and type(has_custom) is not bool:
            raise ValueError(f"invalid hasCustomSchedule for {sym}")
        if has_custom:
            custom_count += 1
            custom_markets[sym] = {
                "maker_fee_bps": l0["maker_fee_bps"],
                "taker_fee_bps": l0["taker_fee_bps"],
            }

    profiles = [
        {"maker_fee_bps": m, "taker_fee_bps": t, "market_count": count}
        for (m, t), count in sorted(profile_counts.items())
    ]
    return {
        "venue": "arcus",
        "configured": True,
        "account_identifier_configured": account_identifier_configured,
        "status": "ok",
        "account_fee_status": ACCOUNT_FEE_STATUS_PUBLIC_TIER0,
        "source_endpoints": [ARCUS_FEE_SCHEDULES_URL],
        "maker_fee_bps": None,
        "taker_fee_bps": None,
        "public_tier0_schedule_summary": {
            "market_count": len(seen_names),
            "custom_schedule_market_count": custom_count,
            "tier0_fee_profiles": profiles,
            "custom_schedule_tier0_bps": dict(sorted(custom_markets.items())),
        },
        "note": (
            "Public /v1/markets/feeSchedules exposes Tier 0 schedules only; "
            "account-level VIP/volume fee tiers require authenticated queries not issued here."
        ),
    }


def _unprobed_unknown_venue(venue: str) -> dict[str, Any]:
    return {
        "venue": venue,
        "configured": False,
        "status": "unprobed_no_public_account_fee_endpoint",
        "account_fee_status": ACCOUNT_FEE_STATUS_UNKNOWN,
        "maker_fee_bps": None,
        "taker_fee_bps": None,
    }


def _check_string_for_leaks(
    text: str,
    *,
    path: str,
    long_literals: list[str],
    short_digit_literals: list[str],
    allow_iso_ts: bool = False,
) -> None:
    if EVM_ADDR_RE.search(text):
        raise ValueError(f"redaction guard rejected EVM address at {path}")
    if _HEX_KEY_64_RE.search(text):
        raise ValueError(f"redaction guard rejected 64-hex secret/key material at {path}")
    if _BASE58_TOKEN_RE.search(text):
        raise ValueError(f"redaction guard rejected Base58 pubkey token at {path}")

    if allow_iso_ts and _ISO_UTC_TS_RE.fullmatch(text):
        return

    lowered = text.lower()
    for lit in long_literals:
        if lit.lower() in lowered:
            raise ValueError(f"redaction guard rejected forbidden literal at {path}")
    for lit in short_digit_literals:
        if re.search(rf"(?<![0-9.]){re.escape(lit)}(?![0-9.])", text):
            raise ValueError(f"redaction guard rejected forbidden short numeric identifier at {path}")


def _walk_and_validate_redaction(
    node: Any,
    *,
    path: str,
    long_literals: list[str],
    short_digit_literals: list[str],
) -> None:
    if isinstance(node, dict):
        for raw_key, val in node.items():
            key_str = str(raw_key)
            norm_key = key_str.lower()
            if _is_prohibited_key(norm_key):
                raise ValueError(f"redaction guard rejected prohibited key {key_str!r} at {path}")
            _check_string_for_leaks(
                key_str,
                path=f"{path}.{key_str}(key)",
                long_literals=long_literals,
                short_digit_literals=short_digit_literals,
            )
            child_path = f"{path}.{key_str}"
            if isinstance(val, int) and not isinstance(val, bool):
                val_str = str(val)
                if val_str in long_literals:
                    raise ValueError(f"redaction guard rejected forbidden numeric literal at {child_path}")
                if val_str in short_digit_literals and norm_key not in _BENIGN_SHORT_INT_KEYS:
                    raise ValueError(f"redaction guard rejected forbidden short numeric identifier at {child_path}")
            else:
                _walk_and_validate_redaction(
                    val,
                    path=child_path,
                    long_literals=long_literals,
                    short_digit_literals=short_digit_literals,
                )
    elif isinstance(node, list):
        for idx, item in enumerate(node):
            _walk_and_validate_redaction(
                item,
                path=f"{path}[{idx}]",
                long_literals=long_literals,
                short_digit_literals=short_digit_literals,
            )
    elif isinstance(node, str):
        _check_string_for_leaks(
            node,
            path=path,
            long_literals=long_literals,
            short_digit_literals=short_digit_literals,
            allow_iso_ts=(path == "$.audited_at_utc"),
        )


def assert_redacted_evidence(payload: Any, forbidden_literals: Iterable[str] = ()) -> None:
    """Enforce zero PII, zero balances/positions/dailyUserVlm, and zero forbidden literals."""
    try:
        serialized = json.dumps(payload, sort_keys=True, ensure_ascii=False, allow_nan=False)
    except (TypeError, ValueError) as exc:
        raise ValueError(f"payload is not strictly JSON-serializable: {exc}") from exc

    if EVM_ADDR_RE.search(serialized):
        raise ValueError("redaction guard rejected EVM address in serialized payload")
    if _HEX_KEY_64_RE.search(serialized):
        raise ValueError("redaction guard rejected 64-hex secret/key material in serialized payload")

    long_literals: list[str] = []
    short_digit_literals: list[str] = []
    for raw_lit in forbidden_literals:
        lit = str(raw_lit).strip()
        if not lit:
            continue
        if lit.isdigit():
            if len(lit) >= 5:
                long_literals.append(lit)
            elif len(lit) >= 2:
                short_digit_literals.append(lit)
            # 1-digit indices ("0", "1", ...) are guarded via prohibited key enforcement in tree walk.
        elif len(lit) >= 2:
            long_literals.append(lit)

    _walk_and_validate_redaction(
        payload,
        path="$",
        long_literals=long_literals,
        short_digit_literals=short_digit_literals,
    )


def validate_redacted_output(payload: Any, forbidden_literals: Iterable[str] = ()) -> None:
    """Public alias for `assert_redacted_evidence` used by read-only OPT-01 fee auditors."""
    assert_redacted_evidence(payload, forbidden_literals=forbidden_literals)


def _collect_forbidden_literals_from_pub_cfg(pub_cfg: dict[str, str]) -> list[str]:
    literals: list[str] = []
    for key in (
        "ALEPH_MONITOR_HL_ADDRESS",
        "ALEPH_MONITOR_HL_XYZ_ADDRESS",
        "ALEPH_MONITOR_HL_IO_ADDRESS",
        "ALEPH_MONITOR_ARCUS_ADDRESS",
        "ALEPH_MONITOR_ARCUS_INDEX",
        "ALEPH_MONITOR_LIGHTER_INDEX",
        "ALEPH_MONITOR_LIGHTER_RH_INDEX",
        "ALEPH_MONITOR_BULK_PUBKEY",
    ):
        val = pub_cfg.get(key, "").strip()
        if val:
            literals.append(val)
    return literals


def _collect_forbidden_literals_from_fixtures(fixtures: dict[str, Any]) -> list[str]:
    found: list[str] = []

    def _scan(obj: Any) -> None:
        if isinstance(obj, dict):
            for k, v in obj.items():
                kl = str(k).lower()
                if kl in ("user", "address", "l1_address", "pubkey", "expected_index") and isinstance(v, (str, int)):
                    if not isinstance(v, bool):
                        found.append(str(v))
                elif kl == "accounts" and isinstance(v, list):
                    for item in v:
                        if isinstance(item, dict) and item.get("index") is not None and not isinstance(item.get("index"), bool):
                            found.append(str(item["index"]))
                        _scan(item)
                    continue
                _scan(v)
        elif isinstance(obj, list):
            for item in obj:
                _scan(item)

    _scan(fixtures)
    return found


def _run_venue_safe(
    venue: str,
    fn: Callable[[], dict[str, Any]],
    forbidden_literals: Iterable[str],
) -> dict[str, Any]:
    try:
        return fn()
    except HTTPError as exc:
        return {
            "venue": venue,
            "configured": True,
            "status": "error",
            "account_fee_status": ACCOUNT_FEE_STATUS_UNKNOWN,
            "maker_fee_bps": None,
            "taker_fee_bps": None,
            "error": _sanitize_error_text(f"HTTP {exc.code}", forbidden_literals),
        }
    except Exception as exc:  # noqa: BLE001
        return {
            "venue": venue,
            "configured": True,
            "status": "error",
            "account_fee_status": ACCOUNT_FEE_STATUS_UNKNOWN,
            "maker_fee_bps": None,
            "taker_fee_bps": None,
            "error": _sanitize_error_text(f"{type(exc).__name__}: {exc}", forbidden_literals),
        }


def audit_account_fees(
    venues: tuple[str, ...] | list[str] = SUPPORTED_VENUES,
    *,
    live: bool = False,
    pub_cfg: dict[str, str] | None = None,
    fixtures: dict[str, Any] | None = None,
    requester: JsonRequester | None = None,
    mode_label: str | None = None,
    audited_at_utc: str | None = None,
) -> dict[str, Any]:
    """Audit account-level fee tiers across the requested subset of the 9 venues."""
    if not live and fixtures is None:
        raise ValueError("audit_account_fees requires either live=True or fixtures")

    selected = tuple(dict.fromkeys(venues))
    unknown = [v for v in selected if v not in _VENUE_INDEX]
    if unknown:
        raise ValueError(f"unsupported venue(s): {', '.join(unknown)}")

    cfg = pub_cfg or {}
    forbidden_literals = _collect_forbidden_literals_from_pub_cfg(cfg)
    if fixtures is not None:
        forbidden_literals.extend(_collect_forbidden_literals_from_fixtures(fixtures))

    req: JsonRequester | None = requester or (_live_request_json if live else None)

    hl_addr = cfg.get("ALEPH_MONITOR_HL_ADDRESS", "").strip()
    hl_xyz_explicit = cfg.get("ALEPH_MONITOR_HL_XYZ_ADDRESS", "").strip()
    hl_xyz = hl_xyz_explicit or hl_addr
    uses_xyz_fallback = bool(hl_addr) and not bool(hl_xyz_explicit)
    lgt_idx = cfg.get("ALEPH_MONITOR_LIGHTER_INDEX", "").strip()
    arc_configured = bool(cfg.get("ALEPH_MONITOR_ARCUS_ADDRESS", "").strip())

    venue_rows: dict[str, dict[str, Any]] = {}
    for venue in selected:
        if venue == "hyperliquid":
            if fixtures is not None:
                if "hyperliquid" in fixtures:
                    venue_rows[venue] = _run_venue_safe(
                        venue,
                        lambda: _probe_hyperliquid(None, None, fixture_payload=fixtures["hyperliquid"]),
                        forbidden_literals,
                    )
                else:
                    venue_rows[venue] = {
                        "venue": "hyperliquid",
                        "configured": False,
                        "status": "unconfigured",
                        "account_fee_status": ACCOUNT_FEE_STATUS_UNKNOWN,
                        "maker_fee_bps": None,
                        "taker_fee_bps": None,
                    }
            else:
                venue_rows[venue] = _run_venue_safe(
                    venue,
                    lambda: _probe_hyperliquid(hl_addr or None, req),
                    forbidden_literals,
                )

        elif venue == "trade_xyz":
            if fixtures is not None:
                if "trade_xyz" in fixtures:
                    venue_rows[venue] = _run_venue_safe(
                        venue,
                        lambda: _probe_trade_xyz(None, None, fixture_payload=fixtures["trade_xyz"]),
                        forbidden_literals,
                    )
                else:
                    venue_rows[venue] = {
                        "venue": "trade_xyz",
                        "configured": False,
                        "status": "unconfigured",
                        "account_fee_status": ACCOUNT_FEE_STATUS_UNKNOWN,
                        "maker_fee_bps": None,
                        "taker_fee_bps": None,
                    }
            else:
                venue_rows[venue] = _run_venue_safe(
                    venue,
                    lambda: _probe_trade_xyz(
                        hl_xyz or None,
                        req,
                        uses_fallback_hl_address=uses_xyz_fallback if hl_xyz else None,
                    ),
                    forbidden_literals,
                )

        elif venue == "lighter":
            if fixtures is not None:
                if "lighter" in fixtures:
                    venue_rows[venue] = _run_venue_safe(
                        venue,
                        lambda: _probe_lighter(
                            None,
                            None,
                            fixture_payload=fixtures["lighter"],
                            discovered_secrets=forbidden_literals,
                        ),
                        forbidden_literals,
                    )
                else:
                    venue_rows[venue] = {
                        "venue": "lighter",
                        "configured": False,
                        "status": "unconfigured",
                        "account_fee_status": ACCOUNT_FEE_STATUS_UNKNOWN,
                        "maker_fee_bps": None,
                        "taker_fee_bps": None,
                    }
            else:
                venue_rows[venue] = _run_venue_safe(
                    venue,
                    lambda: _probe_lighter(
                        lgt_idx or None,
                        req,
                        discovered_secrets=forbidden_literals,
                    ),
                    forbidden_literals,
                )

        elif venue == "arcus":
            if fixtures is not None:
                if "arcus" in fixtures:
                    venue_rows[venue] = _run_venue_safe(
                        venue,
                        lambda: _probe_arcus(
                            account_identifier_configured=False,
                            req=None,
                            fixture_payload=fixtures["arcus"],
                        ),
                        forbidden_literals,
                    )
                else:
                    venue_rows[venue] = {
                        "venue": "arcus",
                        "configured": False,
                        "status": "unconfigured",
                        "account_fee_status": ACCOUNT_FEE_STATUS_UNKNOWN,
                        "maker_fee_bps": None,
                        "taker_fee_bps": None,
                    }
            else:
                venue_rows[venue] = _run_venue_safe(
                    venue,
                    lambda: _probe_arcus(account_identifier_configured=arc_configured, req=req),
                    forbidden_literals,
                )

        else:
            venue_rows[venue] = _unprobed_unknown_venue(venue)

    verified = [v for v, row in venue_rows.items() if row["account_fee_status"] == ACCOUNT_FEE_STATUS_VERIFIED]
    public_only = [
        v
        for v, row in venue_rows.items()
        if row["account_fee_status"]
        in (ACCOUNT_FEE_STATUS_PUBLIC_TIER0, ACCOUNT_FEE_STATUS_LIGHTER_PUBLIC_UNVERIFIED)
    ]
    unknown_venues = [v for v, row in venue_rows.items() if row["account_fee_status"] == ACCOUNT_FEE_STATUS_UNKNOWN]
    error_count = sum(1 for row in venue_rows.values() if row["status"] == "error")

    report: dict[str, Any] = {
        "schema_version": SCHEMA_VERSION,
        "mode": mode_label or ("live" if live else "fixture"),
        "audited_at_utc": audited_at_utc or _utc_now_iso(),
        "requested_venues": list(selected),
        "summary": {
            "total_venues": len(selected),
            "verified_account_tier_venues": verified,
            "public_schedule_only_venues": public_only,
            "unknown_venues": unknown_venues,
            "error_venues": error_count,
            "redaction_verified": True,
        },
        "venues": venue_rows,
    }
    assert_redacted_evidence(report, forbidden_literals)
    return report


def build_embedded_self_test_fixtures() -> dict[str, Any]:
    """Build deterministic synthetic fixtures across probed venues (with intentionally sensitive fields to verify redaction)."""
    return {
        "arcus": {
            "feeSchedules": {
                "feeSchedules": [
                    {
                        "marketId": 1,
                        "marketDisplayName": "BTC-USD",
                        "hasCustomSchedule": False,
                        "tiers": [
                            {"level": 0, "makerFeePpm": 150, "takerFeePpm": 450},
                            {"level": 4, "makerFeePpm": 0, "takerFeePpm": 230},
                        ],
                    },
                    {
                        "marketId": 88,
                        "marketDisplayName": "ANT-USD",
                        "hasCustomSchedule": True,
                        "tiers": [
                            {"level": 0, "makerFeePpm": 0, "takerFeePpm": 112},
                        ],
                    },
                ]
            }
        },
        "hyperliquid": {
            "dailyUserVlm": [{"date": "2026-11-22", "userCross": "999999.12", "userAdd": "500000.00"}],
            "feeSchedule": {
                "cross": "0.00045",
                "add": "0.00015",
                "spotCross": "0.0007",
                "spotAdd": "0.0004",
                "tiers": {
                    "vip": [{"ntlCutoff": "5000000", "cross": "0.0004", "add": "0.00012"}],
                    "mm": [{"makerFractionCutoff": "0.005", "add": "-0.00001"}],
                },
                "referralDiscount": "0.04",
                "stakingDiscountTiers": [{"bpsOfMaxSupply": "0.1", "discount": "0.05"}],
            },
            "userCrossRate": "0.000432",
            "userAddRate": "0.000144",
            "activeReferralDiscount": "0.04",
            "activeStakingDiscount": {"bpsOfMaxSupply": "0.0", "discount": "0.0"},
            "stakingLink": {"type": "user", "stakingUser": "0x1111111111111111111111111111111111111111"},
        },
        "trade_xyz": {
            "userFees": {
                "dailyUserVlm": [{"date": "2026-11-22", "userCross": "123456.0"}],
                "userCrossRate": "0.00045",
                "userAddRate": "0.00015",
                "activeReferralDiscount": "0.0",
                "activeStakingDiscount": "0.0",
            },
            "perpDexs": [
                None,
                {
                    "name": "xyz",
                    "fullName": "TradeXYZ",
                    "deployer": "0x2222222222222222222222222222222222222222",
                    "feeRecipient": "0x3333333333333333333333333333333333333333",
                    "deployerFeeScale": "1.0",
                },
            ],
            "metaAndAssetCtxs": [
                {
                    "universe": [
                        {"name": "xyz:ANT", "growthMode": "enabled", "isDelisted": False},
                        {"name": "xyz:NVDA", "growthMode": "disabled", "isDelisted": False},
                    ]
                },
                [{}, {}],
            ],
        },
        "lighter": {
            "code": 200,
            "accounts": [
                {
                    "index": 77889,
                    "l1_address": "0x4444444444444444444444444444444444444444",
                    "account_type": 1,
                    "collateral": "1234.56",
                    "available_balance": "1234.56",
                    "positions": [{"market_id": 0, "symbol": "ETH", "position": "0"}],
                }
            ],
        },
    }


class _ForbiddenEnvGuard:
    """Block any os.environ read during offline audits/self-tests."""

    def __init__(self) -> None:
        self._orig: Any = None

    def __enter__(self) -> "_ForbiddenEnvGuard":
        class _DeniedEnv:
            def __getitem__(self, key: str) -> str:
                raise AssertionError(f"offline account fee audit must not read env var {key!r}")

            def get(self, key: str, default: Any = None) -> Any:
                raise AssertionError(f"offline account fee audit must not read env var {key!r}")

            def __contains__(self, key: object) -> bool:
                raise AssertionError(f"offline account fee audit must not inspect env var {key!r}")

            def keys(self) -> Any:
                raise AssertionError("offline account fee audit must not enumerate env vars")

            def items(self) -> Any:
                raise AssertionError("offline account fee audit must not enumerate env vars")

            def __iter__(self) -> Any:
                raise AssertionError("offline account fee audit must not iterate env vars")

            def __len__(self) -> int:
                raise AssertionError("offline account fee audit must not inspect env var count")

            def copy(self) -> Any:
                raise AssertionError("offline account fee audit must not copy env vars")

        self._orig = os.environ
        os.environ = _DeniedEnv()  # type: ignore[assignment]
        return self

    def __exit__(self, exc_type: Any, exc: Any, tb: Any) -> None:
        if self._orig is not None:
            os.environ = self._orig


def run_self_test(venues: tuple[str, ...] | list[str] = SUPPORTED_VENUES) -> dict[str, Any]:
    """Run strictly offline deterministic self-test under `_ForbiddenEnvGuard`."""
    fixtures = build_embedded_self_test_fixtures()
    fixed_ts = "2026-11-22T00:00:00+00:00"
    with _ForbiddenEnvGuard():
        report = audit_account_fees(
            venues,
            live=False,
            fixtures=fixtures,
            mode_label="self_test",
            audited_at_utc=fixed_ts,
        )
        assert report["schema_version"] == SCHEMA_VERSION
        assert report["summary"]["error_venues"] == 0
        if "hyperliquid" in report["venues"]:
            hl = report["venues"]["hyperliquid"]
            assert hl["account_fee_status"] == ACCOUNT_FEE_STATUS_VERIFIED
            assert hl["maker_fee_bps"] == "1.44"
            assert hl["taker_fee_bps"] == "4.32"
        if "trade_xyz" in report["venues"]:
            xyz = report["venues"]["trade_xyz"]
            assert xyz["account_fee_status"] == ACCOUNT_FEE_STATUS_VERIFIED
            assert xyz["standard_maker_fee_bps"] == "3"
            assert xyz["standard_taker_fee_bps"] == "9"
            assert xyz["growth_mode_maker_fee_bps"] == "0.3"
            assert xyz["growth_mode_taker_fee_bps"] == "0.9"
            assert xyz["maker_fee_bps"] is None
            assert xyz["taker_fee_bps"] is None
        if "lighter" in report["venues"]:
            lgt = report["venues"]["lighter"]
            assert lgt["account_fee_status"] == ACCOUNT_FEE_STATUS_LIGHTER_PUBLIC_UNVERIFIED
            assert lgt["account_type"] == 1
            assert lgt["maker_fee_bps"] is None
            assert lgt["taker_fee_bps"] is None
        if "arcus" in report["venues"]:
            arc = report["venues"]["arcus"]
            assert arc["account_fee_status"] == ACCOUNT_FEE_STATUS_PUBLIC_TIER0
            assert arc["maker_fee_bps"] is None
        for v in ("backpack", "extended", "bulk", "okx", "binance"):
            if v in report["venues"]:
                assert report["venues"][v]["account_fee_status"] == ACCOUNT_FEE_STATUS_UNKNOWN
                assert report["venues"][v]["maker_fee_bps"] is None
                assert report["venues"][v]["taker_fee_bps"] is None

        # Verify redaction guard rejects PII / balances / forbidden literals.
        bad_rejected = True
        for bad_payload, lits in (
            ({"schema_version": SCHEMA_VERSION, "leak": "0x1111111111111111111111111111111111111111"}, ()),
            ({"schema_version": SCHEMA_VERSION, "venues": {"lighter": {"account_index": 0}}}, ("0",)),
            ({"schema_version": SCHEMA_VERSION, "venues": {"hyperliquid": {"dailyUserVlm": []}}}, ()),
            ({"schema_version": SCHEMA_VERSION, "venues": {"lighter": {"withdrawable": "10"}}}, ()),
        ):
            try:
                assert_redacted_evidence(bad_payload, lits)
            except ValueError:
                continue
            bad_rejected = False
        assert bad_rejected, "redaction guard failed to reject forbidden payload"

    return {
        "self_test": "passed",
        "schema_version": SCHEMA_VERSION,
        "venues_tested": list(report["venues"].keys()),
        "verified_account_tier_venues": report["summary"]["verified_account_tier_venues"],
        "public_schedule_only_venues": report["summary"]["public_schedule_only_venues"],
        "unknown_venues": report["summary"]["unknown_venues"],
        "redaction_guard_verified": True,
        "hip3_derivation_verified": True,
        "zero_secret_env_reads_verified": True,
    }


def _parse_venues_arg(raw: str | None) -> tuple[str, ...]:
    if raw is None or not raw.strip():
        return SUPPORTED_VENUES
    items = [part.strip().lower() for part in raw.split(",") if part.strip()]
    if not items:
        raise ValueError("--venues must list at least one supported venue")
    unknown = [v for v in items if v not in _VENUE_INDEX]
    if unknown:
        raise ValueError(
            f"unsupported venue(s): {', '.join(unknown)}; supported: {', '.join(SUPPORTED_VENUES)}"
        )
    return tuple(dict.fromkeys(items))


def _write_json_output(path: Path, payload: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(payload, indent=2, allow_nan=False) + "\n", encoding="utf-8")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group(required=False)
    mode.add_argument(
        "--live",
        action="store_true",
        help="Explicitly issue read-only HTTPS queries using public identifiers from --env or --monitor-env",
    )
    mode.add_argument(
        "--fixture",
        type=Path,
        default=None,
        help="Path to offline JSON fixture mapping venue names to raw/parsed endpoint payloads",
    )
    mode.add_argument(
        "--self-test",
        action="store_true",
        help="Run strictly offline deterministic self-test across all 9 venues and exit",
    )
    env_src = parser.add_mutually_exclusive_group(required=False)
    env_src.add_argument(
        "--env",
        type=Path,
        default=None,
        help="Source dotenv path for --live (only non-secret public IDs extracted via sync_monitor_env)",
    )
    env_src.add_argument(
        "--monitor-env",
        type=Path,
        default=None,
        help="Public-only monitor.env path for --live",
    )
    parser.add_argument(
        "--venues",
        type=str,
        default=",".join(SUPPORTED_VENUES),
        help=f"Comma-separated venues to audit (default: {','.join(SUPPORTED_VENUES)})",
    )
    parser.add_argument(
        "--output",
        type=Path,
        default=None,
        help="Optional file path to write the redacted JSON evidence output",
    )
    args = parser.parse_args(argv)

    if not args.live and args.fixture is None and not args.self_test:
        print(
            "Error: refuse to run without an explicit mode; specify one of --live, --fixture <path.json>, or --self-test",
            file=sys.stderr,
        )
        return 2

    if not args.live and (args.env is not None or args.monitor_env is not None):
        print("Error: --env and --monitor-env are only valid with --live", file=sys.stderr)
        return 2

    if args.live and args.env is None and args.monitor_env is None:
        print("Error: --live requires either --env <path> or --monitor-env <path>", file=sys.stderr)
        return 2

    try:
        venues = _parse_venues_arg(args.venues)
    except ValueError as exc:
        print(f"Error: {exc}", file=sys.stderr)
        return 2

    if args.self_test:
        try:
            summary = run_self_test(venues)
        except Exception as exc:  # noqa: BLE001
            print(f"Self-test failed: {type(exc).__name__}: {exc}", file=sys.stderr)
            return 2
        assert_redacted_evidence(summary)
        if args.output is not None:
            try:
                _write_json_output(args.output, summary)
            except OSError as exc:
                print(f"Error writing --output: {exc}", file=sys.stderr)
                return 2
        print(json.dumps(summary, indent=2, allow_nan=False))
        return 0

    if args.fixture is not None:
        try:
            loaded = json.loads(args.fixture.read_text(encoding="utf-8"))
        except (OSError, UnicodeDecodeError, json.JSONDecodeError) as exc:
            print(f"Error reading --fixture {args.fixture}: {exc}", file=sys.stderr)
            return 2
        if not isinstance(loaded, dict):
            print("Error: --fixture JSON root must be an object", file=sys.stderr)
            return 2
        fixtures = (
            loaded["venues"]
            if isinstance(loaded.get("venues"), dict) and not any(v in loaded for v in _VENUE_INDEX)
            else loaded
        )
        try:
            report = audit_account_fees(venues, live=False, fixtures=fixtures)
        except ValueError as exc:
            print(f"Error auditing fixture: {exc}", file=sys.stderr)
            return 2
    else:
        try:
            pub_cfg = extract_public_monitor_config(monitor_env=args.monitor_env, source_env=args.env)
        except (OSError, ValueError) as exc:
            print(f"Error loading public monitor identifiers: {exc}", file=sys.stderr)
            return 2
        report = audit_account_fees(venues, live=True, pub_cfg=pub_cfg)

    if args.output is not None:
        try:
            _write_json_output(args.output, report)
        except OSError as exc:
            print(f"Error writing --output {args.output}: {exc}", file=sys.stderr)
            return 2

    print(json.dumps(report, indent=2, allow_nan=False))
    return 1 if report["summary"]["error_venues"] > 0 else 0


if __name__ == "__main__":
    raise SystemExit(main())
