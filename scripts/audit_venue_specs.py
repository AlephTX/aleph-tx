#!/usr/bin/env python3
"""Read-only multi-venue public specification & fee-schedule auditor (`OPT-01`).

Strictly offline by default: never reads `.env*` files or secret/credential
environment variables, and never issues HTTP requests without explicit `--live`.
Every venue always reports `account_fee_status = "unknown_requires_account_evidence"`
because public endpoints cannot establish account-specific tiers, VIP rebates,
or fee overrides.
"""

from __future__ import annotations

import argparse
from datetime import datetime, timezone
from decimal import Decimal, InvalidOperation
import hashlib
import json
import os
from pathlib import Path
import ssl
import sys
from typing import Any, Callable
from urllib.error import HTTPError, URLError
from urllib.request import HTTPRedirectHandler, HTTPSHandler, Request, build_opener

SCHEMA_VERSION = "opt01_venue_spec_audit_v1"
ACCOUNT_FEE_STATUS_UNKNOWN = "unknown_requires_account_evidence"
NOT_EXPOSED_FEE_EVIDENCE: dict[str, str] = {
    "status": "not_exposed_in_public_endpoint",
}
MAX_RESPONSE_BYTES = 16 * 1024 * 1024
DEFAULT_TIMEOUT_SECONDS = 15.0

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

VENUE_ENDPOINTS: dict[str, tuple[dict[str, Any], ...]] = {
    "arcus": (
        {
            "role": "markets",
            "key": "https://api.arcus.xyz/v1/markets",
            "url": "https://api.arcus.xyz/v1/markets",
            "method": "GET",
            "body": None,
        },
        {
            "role": "feeSchedules",
            "key": "https://api.arcus.xyz/v1/markets/feeSchedules",
            "url": "https://api.arcus.xyz/v1/markets/feeSchedules",
            "method": "GET",
            "body": None,
        },
    ),
    "hyperliquid": (
        {
            "role": "perpDexs",
            "key": "https://api.hyperliquid.xyz/info#type=perpDexs",
            "url": "https://api.hyperliquid.xyz/info",
            "method": "POST",
            "body": {"type": "perpDexs"},
        },
        {
            "role": "metaAndAssetCtxs",
            "key": "https://api.hyperliquid.xyz/info#type=metaAndAssetCtxs",
            "url": "https://api.hyperliquid.xyz/info",
            "method": "POST",
            "body": {"type": "metaAndAssetCtxs"},
        },
    ),
    "trade_xyz": (
        {
            "role": "perpDexs",
            "key": "https://api.hyperliquid.xyz/info#type=perpDexs",
            "url": "https://api.hyperliquid.xyz/info",
            "method": "POST",
            "body": {"type": "perpDexs"},
        },
        {
            "role": "metaAndAssetCtxs",
            "key": "https://api.hyperliquid.xyz/info#type=metaAndAssetCtxs&dex=xyz",
            "url": "https://api.hyperliquid.xyz/info",
            "method": "POST",
            "body": {"type": "metaAndAssetCtxs", "dex": "xyz"},
        },
    ),
    "lighter": (
        {
            "role": "orderBookDetails",
            "key": "https://mainnet.zklighter.elliot.ai/api/v1/orderBookDetails",
            "url": "https://mainnet.zklighter.elliot.ai/api/v1/orderBookDetails",
            "method": "GET",
            "body": None,
        },
    ),
    "backpack": (
        {
            "role": "markets",
            "key": "https://api.backpack.exchange/api/v1/markets",
            "url": "https://api.backpack.exchange/api/v1/markets",
            "method": "GET",
            "body": None,
        },
    ),
    "extended": (
        {
            "role": "markets",
            "key": "https://api.starknet.extended.exchange/api/v1/info/markets",
            "url": "https://api.starknet.extended.exchange/api/v1/info/markets",
            "method": "GET",
            "body": None,
        },
    ),
    "bulk": (
        {
            "role": "exchangeInfo",
            "key": "https://mainnet-api1.bulk.trade/api/v1/exchangeInfo",
            "url": "https://mainnet-api1.bulk.trade/api/v1/exchangeInfo",
            "method": "GET",
            "body": None,
        },
    ),
    "okx": (
        {
            "role": "instruments",
            "key": "https://www.okx.com/api/v5/public/instruments?instType=SWAP",
            "url": "https://www.okx.com/api/v5/public/instruments?instType=SWAP",
            "method": "GET",
            "body": None,
        },
    ),
    "binance": (
        {
            "role": "exchangeInfo",
            "key": "https://fapi.binance.com/fapi/v1/exchangeInfo",
            "url": "https://fapi.binance.com/fapi/v1/exchangeInfo",
            "method": "GET",
            "body": None,
        },
    ),
}


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


def _live_fetch_bytes(
    url: str,
    *,
    method: str = "GET",
    body: dict[str, Any] | None = None,
    timeout: float = DEFAULT_TIMEOUT_SECONDS,
) -> bytes:
    if not url.startswith("https://"):
        raise ValueError(f"only HTTPS public endpoints are allowed: {url}")
    headers = {
        "Accept": "application/json",
        "User-Agent": "AlephTX-SpecAudit/1.0",
    }
    data: bytes | None = None
    if body is not None:
        data = json.dumps(body, separators=(",", ":")).encode("utf-8")
        headers["Content-Type"] = "application/json"
    req = Request(url, data=data, headers=headers, method=method)
    https_handler = HTTPSHandler(context=_build_ssl_context())
    opener = build_opener(_NoRedirectHandler(), https_handler)
    with opener.open(req, timeout=timeout) as resp:
        if resp.status != 200:
            raise ValueError(f"unexpected HTTP status {resp.status} from {url}")
        raw = resp.read(MAX_RESPONSE_BYTES + 1)
    if len(raw) > MAX_RESPONSE_BYTES:
        raise ValueError(f"response from {url} exceeded {MAX_RESPONSE_BYTES} bytes")
    return raw


def _canonical_raw_bytes(payload: Any) -> bytes:
    if isinstance(payload, bytes):
        return payload
    if isinstance(payload, str):
        return payload.encode("utf-8")
    return json.dumps(
        payload,
        sort_keys=True,
        separators=(",", ":"),
        ensure_ascii=False,
        allow_nan=False,
    ).encode("utf-8")


def compute_response_sha256(raw_bytes: bytes) -> str:
    """Compute SHA-256 hex digest over exact raw endpoint/fixture bytes."""
    return hashlib.sha256(raw_bytes).hexdigest()


def _decode_json_bytes(raw_bytes: bytes, label: str) -> Any:
    try:
        return json.loads(raw_bytes.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise ValueError(f"invalid JSON payload for {label}: {exc}") from exc


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


def _format_decimal(val: Decimal) -> str:
    if val == 0:
        return "0"
    norm = val.normalize()
    text = format(norm, "f")
    if "." in text:
        text = text.rstrip("0").rstrip(".")
    return text


def _format_decimal_str(raw: Any, field_name: str, *, allow_zero: bool = False) -> str:
    val = _parse_decimal(raw, field_name)
    if allow_zero:
        if val < 0:
            raise ValueError(f"{field_name} must be non-negative, got {val}")
    elif val <= 0:
        raise ValueError(f"{field_name} must be positive, got {val}")
    return _format_decimal(val)


def _parse_optional_decimal_str(raw: Any, field_name: str, *, allow_zero: bool = False) -> str | None:
    if raw is None or raw == "":
        return None
    return _format_decimal_str(raw, field_name, allow_zero=allow_zero)


def _parse_optional_leverage(raw: Any, field_name: str) -> float | None:
    if raw is None or raw == "":
        return None
    val = _parse_decimal(raw, field_name)
    if val <= 0:
        raise ValueError(f"{field_name} must be positive, got {val}")
    return round(float(val), 6)


def _leverage_from_fraction(raw: Any, field_name: str, *, scale: Decimal = Decimal("1")) -> float | None:
    if raw is None or raw == "":
        return None
    val = _parse_decimal(raw, field_name)
    if val <= 0 or val > scale:
        raise ValueError(f"{field_name} must be in (0, {scale}], got {val}")
    return round(float(scale / val), 6)


def _require_non_empty_str(raw: Any, field_name: str) -> str:
    if not isinstance(raw, str) or not raw.strip():
        raise ValueError(f"missing or invalid {field_name}")
    return raw.strip()


def _require_int(raw: Any, field_name: str, *, min_value: int | None = None, max_value: int | None = None) -> int:
    if type(raw) is not int:
        raise ValueError(f"{field_name} must be an integer")
    if min_value is not None and raw < min_value:
        raise ValueError(f"{field_name} must be >= {min_value}")
    if max_value is not None and raw > max_value:
        raise ValueError(f"{field_name} must be <= {max_value}")
    return raw


def _make_market(
    *,
    symbol: str,
    tick_size: str | None,
    step_size: str | None,
    min_notional: str | None,
    max_leverage: float | None,
    public_fee_evidence: dict[str, Any] | None,
) -> dict[str, Any]:
    return {
        "symbol": symbol,
        "tick_size": tick_size,
        "step_size": step_size,
        "min_notional": min_notional,
        "max_leverage": max_leverage,
        "public_fee_evidence": (
            public_fee_evidence if public_fee_evidence is not None else dict(NOT_EXPOSED_FEE_EVIDENCE)
        ),
    }


def _collect_fee_fields(
    row: dict[str, Any],
    fields: tuple[str, ...],
    *,
    source: str,
) -> dict[str, Any]:
    extracted: dict[str, Any] = {}
    for key in fields:
        if key in row and row[key] is not None:
            val = row[key]
            if isinstance(val, bool):
                raise ValueError(f"invalid boolean fee field {key}")
            if isinstance(val, (int, float, Decimal)):
                extracted[key] = _format_decimal(_parse_decimal(val, key))
            elif isinstance(val, str):
                text = val.strip()
                if not text:
                    raise ValueError(f"empty fee field {key}")
                if key in ("feeGroup", "groupId", "feeTier"):
                    extracted[key] = text
                else:
                    extracted[key] = _format_decimal(_parse_decimal(text, key))
            elif isinstance(val, dict):
                extracted[key] = val
            else:
                raise ValueError(f"unsupported fee field type for {key}")
    if not extracted:
        return dict(NOT_EXPOSED_FEE_EVIDENCE)
    return {"source": source, **extracted}


def _reject_error_envelope(payload: Any, venue: str) -> None:
    if isinstance(payload, dict):
        if payload.get("error"):
            raise ValueError(f"{venue} error response: {payload.get('error')}")
        status = payload.get("status")
        if isinstance(status, str) and status.lower() in ("err", "error", "failed"):
            raise ValueError(f"{venue} error status: {status}")


def _parse_arcus_tiers(rows: Any, label: str) -> list[dict[str, Any]]:
    if not isinstance(rows, list) or not rows:
        raise ValueError(f"Arcus {label} must be a non-empty list")
    seen_levels: set[int] = set()
    parsed: list[dict[str, Any]] = []
    for row in rows:
        if not isinstance(row, dict):
            raise ValueError(f"Arcus {label} tier entry must be an object")
        level = _require_int(row.get("level"), f"{label}.level", min_value=0, max_value=32)
        if level in seen_levels:
            raise ValueError(f"duplicate Arcus tier level {level}")
        seen_levels.add(level)
        maker_ppm = _require_int(row.get("makerFeePpm"), f"{label}.makerFeePpm")
        taker_ppm = _require_int(row.get("takerFeePpm"), f"{label}.takerFeePpm")
        parsed.append(
            {
                "level": level,
                "maker_fee_ppm": maker_ppm,
                "taker_fee_ppm": taker_ppm,
                "maker_fee_bps": _format_decimal(Decimal(maker_ppm) / Decimal(100)),
                "taker_fee_bps": _format_decimal(Decimal(taker_ppm) / Decimal(100)),
            }
        )
    if 0 not in seen_levels:
        raise ValueError(f"Arcus {label} missing base level 0")
    parsed.sort(key=lambda item: item["level"])
    return parsed


def _parse_arcus(payloads: dict[str, Any]) -> list[dict[str, Any]]:
    markets_doc = payloads["markets"]
    fees_doc = payloads["feeSchedules"]
    _reject_error_envelope(markets_doc, "arcus")
    _reject_error_envelope(fees_doc, "arcus")
    if not isinstance(markets_doc, dict) or not isinstance(markets_doc.get("markets"), list):
        raise ValueError("Arcus /v1/markets must be an object containing a markets list")
    if not isinstance(fees_doc, dict) or not isinstance(fees_doc.get("feeSchedules"), list):
        raise ValueError("Arcus /v1/markets/feeSchedules must be an object containing a feeSchedules list")

    raw_markets = markets_doc["markets"]
    if not raw_markets:
        raise ValueError("Arcus markets list is empty")

    schedules_by_id: dict[int, dict[str, Any]] = {}
    schedules_by_name: dict[str, dict[str, Any]] = {}
    for row in fees_doc["feeSchedules"]:
        if not isinstance(row, dict):
            raise ValueError("Arcus feeSchedules entry must be an object")
        market_id = _require_int(row.get("marketId"), "feeSchedules.marketId", min_value=0, max_value=65535)
        symbol = _require_non_empty_str(row.get("marketDisplayName"), "feeSchedules.marketDisplayName")
        if market_id in schedules_by_id or symbol in schedules_by_name:
            raise ValueError(f"duplicate Arcus feeSchedule for {symbol} ({market_id})")
        parsed_tiers = _parse_arcus_tiers(row.get("tiers"), f"feeSchedules[{symbol}].tiers")
        has_custom = row.get("hasCustomSchedule")
        if has_custom is not None and type(has_custom) is not bool:
            raise ValueError(f"invalid hasCustomSchedule for {symbol}")
        entry = {
            "market_id": market_id,
            "symbol": symbol,
            "has_custom_schedule": has_custom,
            "tiers": parsed_tiers,
        }
        schedules_by_id[market_id] = entry
        schedules_by_name[symbol] = entry

    seen_symbols: set[str] = set()
    markets: list[dict[str, Any]] = []
    for row in raw_markets:
        if not isinstance(row, dict):
            raise ValueError("Arcus market entry must be an object")
        symbol = _require_non_empty_str(row.get("marketDisplayName"), "marketDisplayName")
        if symbol in seen_symbols:
            raise ValueError(f"duplicate Arcus market symbol {symbol}")
        seen_symbols.add(symbol)
        market_id = _require_int(row.get("marketId"), f"{symbol}.marketId", min_value=0, max_value=65535)
        contract_type = row.get("type", "PERPETUAL")
        if contract_type != "PERPETUAL":
            continue
        tick_size = _format_decimal_str(row.get("tickSize"), f"{symbol}.tickSize")
        step_size = _format_decimal_str(row.get("stepSize"), f"{symbol}.stepSize")
        min_notional = _parse_optional_decimal_str(row.get("minOrderNotional"), f"{symbol}.minOrderNotional", allow_zero=True)
        max_leverage = _parse_optional_leverage(row.get("maxLeverage"), f"{symbol}.maxLeverage")
        if max_leverage is None and row.get("initialMarginFraction") is not None:
            max_leverage = _leverage_from_fraction(row.get("initialMarginFraction"), f"{symbol}.initialMarginFraction")

        schedule = schedules_by_id.get(market_id)
        if schedule is not None and schedule["symbol"] != symbol:
            raise ValueError(f"Arcus feeSchedules marketId {market_id} symbol mismatch: {schedule['symbol']} != {symbol}")
        if schedule is None:
            schedule = schedules_by_name.get(symbol)
            if schedule is not None and schedule["market_id"] != market_id:
                raise ValueError(f"Arcus feeSchedules symbol {symbol} marketId mismatch")

        fee_evidence: dict[str, Any]
        if schedule is not None:
            if "feeTiers" in row and row["feeTiers"] is not None:
                market_tiers = _parse_arcus_tiers(row["feeTiers"], f"markets[{symbol}].feeTiers")
                if market_tiers != schedule["tiers"]:
                    raise ValueError(f"Arcus feeTiers disagree between /v1/markets and /v1/markets/feeSchedules for {symbol}")
            if "hasCustomFeeSchedule" in row and row["hasCustomFeeSchedule"] is not None:
                if type(row["hasCustomFeeSchedule"]) is not bool:
                    raise ValueError(f"invalid hasCustomFeeSchedule for {symbol}")
                if schedule["has_custom_schedule"] is not None and row["hasCustomFeeSchedule"] != schedule["has_custom_schedule"]:
                    raise ValueError(f"Arcus hasCustomFeeSchedule mismatch for {symbol}")
            fee_evidence = {
                "source": "/v1/markets/feeSchedules",
                "market_id": market_id,
                "has_custom_schedule": schedule["has_custom_schedule"],
                "tiers": schedule["tiers"],
            }
            if row.get("effectiveMakerFeePpm") is not None:
                eff_maker = _require_int(row.get("effectiveMakerFeePpm"), f"{symbol}.effectiveMakerFeePpm")
                if eff_maker != schedule["tiers"][0]["maker_fee_ppm"]:
                    raise ValueError(f"Arcus effectiveMakerFeePpm mismatch for {symbol}")
                fee_evidence["effective_maker_fee_ppm"] = eff_maker
            if row.get("effectiveTakerFeePpm") is not None:
                eff_taker = _require_int(row.get("effectiveTakerFeePpm"), f"{symbol}.effectiveTakerFeePpm")
                if eff_taker != schedule["tiers"][0]["taker_fee_ppm"]:
                    raise ValueError(f"Arcus effectiveTakerFeePpm mismatch for {symbol}")
                fee_evidence["effective_taker_fee_ppm"] = eff_taker
        elif row.get("feeTiers") is not None:
            fee_evidence = {
                "source": "/v1/markets",
                "market_id": market_id,
                "has_custom_schedule": row.get("hasCustomFeeSchedule"),
                "tiers": _parse_arcus_tiers(row["feeTiers"], f"markets[{symbol}].feeTiers"),
            }
        else:
            fee_evidence = dict(NOT_EXPOSED_FEE_EVIDENCE)

        markets.append(
            _make_market(
                symbol=symbol,
                tick_size=tick_size,
                step_size=step_size,
                min_notional=min_notional,
                max_leverage=max_leverage,
                public_fee_evidence=fee_evidence,
            )
        )

    if not markets:
        raise ValueError("Arcus perpetual markets list is empty")
    return markets


def _parse_hyperliquid_dex(payloads: dict[str, Any], *, dex: str) -> list[dict[str, Any]]:
    venue_label = "hyperliquid" if not dex else f"trade_xyz({dex})"
    perp_dexs = payloads["perpDexs"]
    meta_and_ctxs = payloads["metaAndAssetCtxs"]
    _reject_error_envelope(perp_dexs, venue_label)
    _reject_error_envelope(meta_and_ctxs, venue_label)

    if not isinstance(perp_dexs, list) or not perp_dexs or perp_dexs[0] is not None:
        raise ValueError(f"{venue_label} perpDexs must be a list starting with null")

    dex_entry: dict[str, Any] | None = None
    dex_index = 0
    asset_offset = 0
    if dex:
        matches = [
            (idx, item)
            for idx, item in enumerate(perp_dexs)
            if isinstance(item, dict) and item.get("name") == dex
        ]
        if len(matches) != 1:
            raise ValueError(f"{venue_label} DEX {dex!r} missing or duplicated in perpDexs")
        dex_index, dex_entry = matches[0]
        asset_offset = 100_000 + dex_index * 10_000

    if not isinstance(meta_and_ctxs, list) or len(meta_and_ctxs) != 2:
        raise ValueError(f"{venue_label} metaAndAssetCtxs must be a 2-element [meta, assetCtxs] list")
    meta, contexts = meta_and_ctxs
    if not isinstance(meta, dict) or not isinstance(meta.get("universe"), list) or not isinstance(contexts, list):
        raise ValueError(f"{venue_label} metaAndAssetCtxs missing universe or contexts list")
    universe = meta["universe"]
    if len(universe) != len(contexts):
        raise ValueError(f"{venue_label} universe/contexts length mismatch")
    if not universe:
        raise ValueError(f"{venue_label} universe is empty")

    collateral_token = meta.get("collateralToken")
    if collateral_token is not None:
        _require_int(collateral_token, f"{venue_label}.collateralToken", min_value=0)

    dex_fee_scale: str | None = None
    if dex_entry is not None and dex_entry.get("deployerFeeScale") is not None:
        dex_fee_scale = _format_decimal_str(
            dex_entry.get("deployerFeeScale"),
            f"{venue_label}.perpDexs.deployerFeeScale",
            allow_zero=True,
        )

    seen_symbols: set[str] = set()
    markets: list[dict[str, Any]] = []
    for asset in universe:
        if not isinstance(asset, dict):
            raise ValueError(f"{venue_label} asset entry must be an object")
        symbol = _require_non_empty_str(asset.get("name"), f"{venue_label}.universe.name")
        if dex:
            if not symbol.startswith(f"{dex}:") or len(symbol) <= len(dex) + 1:
                raise ValueError(f"{venue_label} asset {symbol!r} outside {dex}: namespace")
        elif ":" in symbol:
            raise ValueError(f"{venue_label} core asset {symbol!r} must not be namespaced")
        if symbol in seen_symbols:
            raise ValueError(f"duplicate {venue_label} symbol {symbol}")
        seen_symbols.add(symbol)

        sz_decimals = _require_int(asset.get("szDecimals"), f"{symbol}.szDecimals", min_value=0, max_value=8)
        step_size = _format_decimal(Decimal(1).scaleb(-sz_decimals))
        tick_size = _format_decimal(Decimal(1).scaleb(-(6 - sz_decimals))) if sz_decimals <= 6 else None
        min_notional = _parse_optional_decimal_str(asset.get("minNotional"), f"{symbol}.minNotional", allow_zero=True)
        max_leverage = _parse_optional_leverage(asset.get("maxLeverage"), f"{symbol}.maxLeverage")

        is_delisted = asset.get("isDelisted", False)
        if type(is_delisted) is not bool:
            raise ValueError(f"invalid isDelisted flag for {symbol}")

        growth_mode = asset.get("growthMode")
        if growth_mode is not None and growth_mode not in ("enabled", "disabled"):
            raise ValueError(f"unknown growthMode {growth_mode!r} for {symbol}")

        asset_fee_scale: str | None = None
        if asset.get("deployerFeeScale") is not None:
            asset_fee_scale = _format_decimal_str(
                asset.get("deployerFeeScale"),
                f"{symbol}.deployerFeeScale",
                allow_zero=True,
            )

        if growth_mode is not None or asset_fee_scale is not None or dex_fee_scale is not None:
            fee_evidence: dict[str, Any] = {
                "source": "perpDexs+metaAndAssetCtxs",
                "dex": dex or "core",
            }
            if dex:
                fee_evidence["dex_index"] = dex_index
                fee_evidence["asset_offset"] = asset_offset
            if collateral_token is not None:
                fee_evidence["collateral_token"] = collateral_token
            if dex_fee_scale is not None:
                fee_evidence["dex_deployer_fee_scale"] = dex_fee_scale
            if asset_fee_scale is not None:
                fee_evidence["deployer_fee_scale"] = asset_fee_scale
            if growth_mode is not None:
                fee_evidence["growth_mode"] = growth_mode
            fee_evidence["is_delisted"] = is_delisted
        else:
            fee_evidence = dict(NOT_EXPOSED_FEE_EVIDENCE)

        markets.append(
            _make_market(
                symbol=symbol,
                tick_size=tick_size,
                step_size=step_size,
                min_notional=min_notional,
                max_leverage=max_leverage,
                public_fee_evidence=fee_evidence,
            )
        )

    return markets


def _parse_lighter_margin_leverage(
    row: dict[str, Any],
    *,
    symbol: str,
    status: str | None,
) -> float | None:
    """Resolve Lighter max leverage, preferring min_initial_margin_fraction and allowing 0 only when inactive."""
    max_leverage = _parse_optional_leverage(row.get("max_leverage"), f"{symbol}.max_leverage")
    if max_leverage is not None:
        return max_leverage

    candidates: list[tuple[str, Any]] = []
    for field_name in ("min_initial_margin_fraction", "default_initial_margin_fraction"):
        raw = row.get(field_name)
        if raw is not None and raw != "":
            candidates.append((field_name, raw))

    for field_name, imf_raw in candidates:
        full_field = f"{symbol}.{field_name}"
        imf_val = _parse_decimal(imf_raw, full_field)
        if imf_val < 0:
            raise ValueError(f"{full_field} must be positive, got {imf_val}")
        if imf_val == 0:
            # Lighter sets initial margin fractions to 0 on inactive/delisted markets (e.g. MKR).
            if status == "inactive":
                continue
            raise ValueError(f"{full_field} must be positive unless market status is 'inactive', got {imf_val}")
        scale = Decimal("1") if imf_val <= Decimal("1") else Decimal("10000")
        return _leverage_from_fraction(imf_raw, full_field, scale=scale)

    return None


def _parse_lighter(payload: Any) -> list[dict[str, Any]]:
    _reject_error_envelope(payload, "lighter")
    if not isinstance(payload, dict):
        raise ValueError("Lighter orderBookDetails response must be an object")
    if "code" in payload and payload["code"] != 200:
        raise ValueError(f"Lighter orderBookDetails returned code={payload['code']!r}")
    rows = payload.get("order_book_details")
    if not isinstance(rows, list):
        raise ValueError("Lighter response missing order_book_details list")

    perp_rows: list[tuple[dict[str, Any], str, str | None, int | None]] = []
    seen_market_ids: set[int] = set()
    active_symbols: set[str] = set()
    symbol_counts: dict[str, int] = {}

    for row in rows:
        if not isinstance(row, dict):
            raise ValueError("Lighter market entry must be an object")
        if row.get("market_type", "perp") != "perp":
            continue
        symbol = _require_non_empty_str(row.get("symbol"), "lighter.symbol")
        status_raw = row.get("status")
        status: str | None = (
            _require_non_empty_str(status_raw, f"{symbol}.status").lower()
            if status_raw is not None
            else None
        )
        market_id: int | None = (
            _require_int(row.get("market_id"), f"{symbol}.market_id", min_value=0)
            if row.get("market_id") is not None
            else None
        )
        if market_id is not None:
            if market_id in seen_market_ids:
                raise ValueError(f"duplicate Lighter perp market_id {market_id}")
            seen_market_ids.add(market_id)

        symbol_counts[symbol] = symbol_counts.get(symbol, 0) + 1
        if status != "inactive":
            if symbol in active_symbols:
                raise ValueError(f"duplicate active Lighter perp symbol {symbol}")
            active_symbols.add(symbol)

        perp_rows.append((row, symbol, status, market_id))

    seen_symbols: set[str] = set()
    markets: list[dict[str, Any]] = []
    for row, symbol, status, market_id in perp_rows:
        if symbol_counts[symbol] > 1 and status == "inactive":
            if market_id is None:
                raise ValueError(f"duplicate Lighter perp symbol {symbol} without market_id")
            market_symbol = f"{symbol}#{market_id}"
        else:
            if symbol_counts[symbol] > 1 and symbol in seen_symbols:
                raise ValueError(f"duplicate Lighter perp symbol {symbol}")
            market_symbol = symbol

        if market_symbol in seen_symbols:
            raise ValueError(f"duplicate Lighter perp symbol {market_symbol}")
        seen_symbols.add(market_symbol)

        price_dec_raw = row.get("price_decimals") if row.get("price_decimals") is not None else row.get("supported_price_decimals")
        size_dec_raw = row.get("size_decimals") if row.get("size_decimals") is not None else row.get("supported_size_decimals")
        price_decimals = _require_int(price_dec_raw, f"{market_symbol}.price_decimals", min_value=0, max_value=12)
        size_decimals = _require_int(size_dec_raw, f"{market_symbol}.size_decimals", min_value=0, max_value=12)
        tick_size = _format_decimal(Decimal(1).scaleb(-price_decimals))
        step_size = _format_decimal(Decimal(1).scaleb(-size_decimals))
        min_notional = _parse_optional_decimal_str(row.get("min_quote_amount"), f"{market_symbol}.min_quote_amount", allow_zero=True)

        max_leverage = _parse_lighter_margin_leverage(row, symbol=market_symbol, status=status)

        fee_evidence = _collect_fee_fields(
            row,
            ("maker_fee", "taker_fee", "liquidation_fee"),
            source="/api/v1/orderBookDetails",
        )
        if "source" not in fee_evidence and status is not None:
            fee_evidence = {"source": "/api/v1/orderBookDetails"}
        if "source" in fee_evidence:
            if market_id is not None:
                fee_evidence["market_id"] = market_id
            if status is not None:
                fee_evidence["market_status"] = status

        markets.append(
            _make_market(
                symbol=market_symbol,
                tick_size=tick_size,
                step_size=step_size,
                min_notional=min_notional,
                max_leverage=max_leverage,
                public_fee_evidence=fee_evidence,
            )
        )

    if not markets:
        raise ValueError("Lighter perpetual market catalog is empty")
    return markets


def _parse_backpack(payload: Any) -> list[dict[str, Any]]:
    _reject_error_envelope(payload, "backpack")
    if isinstance(payload, dict) and isinstance(payload.get("markets"), list):
        rows = payload["markets"]
    elif isinstance(payload, list):
        rows = payload
    else:
        raise ValueError("Backpack /api/v1/markets response must be a list of markets")

    seen_symbols: set[str] = set()
    markets: list[dict[str, Any]] = []
    for row in rows:
        if not isinstance(row, dict):
            raise ValueError("Backpack market entry must be an object")
        market_type = row.get("marketType", "PERP")
        if market_type != "PERP":
            continue
        symbol = _require_non_empty_str(row.get("symbol"), "backpack.symbol")
        if symbol in seen_symbols:
            raise ValueError(f"duplicate Backpack symbol {symbol}")
        seen_symbols.add(symbol)

        filters = row.get("filters")
        if not isinstance(filters, dict):
            raise ValueError(f"Backpack {symbol} missing filters object")
        price_filter = filters.get("price") or {}
        qty_filter = filters.get("quantity") or {}
        notional_filter = filters.get("notional") or {}
        leverage_filter = filters.get("leverage") or {}
        if not isinstance(price_filter, dict) or not isinstance(qty_filter, dict):
            raise ValueError(f"Backpack {symbol} invalid price/quantity filters")

        tick_raw = price_filter.get("tickSize", row.get("tickSize"))
        step_raw = qty_filter.get("stepSize", row.get("stepSize"))
        tick_size = _format_decimal_str(tick_raw, f"{symbol}.tickSize")
        step_size = _format_decimal_str(step_raw, f"{symbol}.stepSize")

        min_notional_raw = notional_filter.get("minNotional") if isinstance(notional_filter, dict) else None
        if min_notional_raw is None:
            min_notional_raw = row.get("minNotional")
        min_notional = _parse_optional_decimal_str(min_notional_raw, f"{symbol}.minNotional", allow_zero=True)

        max_lev_raw = row.get("maxLeverage")
        if max_lev_raw is None and isinstance(leverage_filter, dict):
            max_lev_raw = leverage_filter.get("maxLeverage")
        max_leverage = _parse_optional_leverage(max_lev_raw, f"{symbol}.maxLeverage")
        if max_leverage is None and isinstance(row.get("imfFunction"), dict):
            base_imf = row["imfFunction"].get("base")
            if base_imf is not None:
                max_leverage = _leverage_from_fraction(base_imf, f"{symbol}.imfFunction.base")

        fee_evidence = _collect_fee_fields(
            row,
            ("makerFee", "takerFee", "feeGroup", "fee"),
            source="/api/v1/markets",
        )
        markets.append(
            _make_market(
                symbol=symbol,
                tick_size=tick_size,
                step_size=step_size,
                min_notional=min_notional,
                max_leverage=max_leverage,
                public_fee_evidence=fee_evidence,
            )
        )

    if not markets:
        raise ValueError("Backpack perpetual market catalog is empty")
    return markets


def _parse_extended(payload: Any) -> list[dict[str, Any]]:
    _reject_error_envelope(payload, "extended")
    if not isinstance(payload, dict):
        raise ValueError("Extended /api/v1/info/markets response must be an object")
    if "status" in payload and str(payload["status"]).upper() != "OK":
        raise ValueError(f"Extended metadata returned status={payload['status']!r}")
    rows = payload.get("data")
    if not isinstance(rows, list):
        raise ValueError("Extended response missing data list")

    seen_symbols: set[str] = set()
    markets: list[dict[str, Any]] = []
    for row in rows:
        if not isinstance(row, dict):
            raise ValueError("Extended market entry must be an object")
        if row.get("type", "PERPETUAL") != "PERPETUAL":
            continue
        if row.get("isRfq") is True:
            continue
        symbol = _require_non_empty_str(row.get("name"), "extended.name")
        if symbol in seen_symbols:
            raise ValueError(f"duplicate Extended symbol {symbol}")
        seen_symbols.add(symbol)

        cfg = row.get("tradingConfig")
        if not isinstance(cfg, dict):
            raise ValueError(f"Extended {symbol} missing tradingConfig")
        tick_size = _format_decimal_str(cfg.get("minPriceChange"), f"{symbol}.minPriceChange")
        step_size = _format_decimal_str(cfg.get("minOrderSizeChange"), f"{symbol}.minOrderSizeChange")
        min_notional = _parse_optional_decimal_str(
            cfg.get("minOrderNotional") if cfg.get("minOrderNotional") is not None else cfg.get("minNotional"),
            f"{symbol}.minOrderNotional",
            allow_zero=True,
        )

        max_lev_raw = cfg.get("maxLeverage")
        if max_lev_raw is None and isinstance(cfg.get("riskFactorConfig"), list) and cfg["riskFactorConfig"]:
            first_rf = cfg["riskFactorConfig"][0]
            if isinstance(first_rf, dict):
                max_lev_raw = first_rf.get("maxLeverage")
        max_leverage = _parse_optional_leverage(max_lev_raw, f"{symbol}.maxLeverage")

        fee_evidence = _collect_fee_fields(
            {**cfg, **row},
            ("makerFee", "takerFee", "feeRate", "fees"),
            source="/api/v1/info/markets",
        )
        markets.append(
            _make_market(
                symbol=symbol,
                tick_size=tick_size,
                step_size=step_size,
                min_notional=min_notional,
                max_leverage=max_leverage,
                public_fee_evidence=fee_evidence,
            )
        )

    if not markets:
        raise ValueError("Extended perpetual market catalog is empty")
    return markets


def _parse_bulk(payload: Any) -> list[dict[str, Any]]:
    _reject_error_envelope(payload, "bulk")
    if isinstance(payload, dict) and isinstance(payload.get("markets"), list):
        rows = payload["markets"]
    elif isinstance(payload, list):
        rows = payload
    else:
        raise ValueError("BULK /api/v1/exchangeInfo response must be a list of markets")

    seen_symbols: set[str] = set()
    markets: list[dict[str, Any]] = []
    for row in rows:
        if not isinstance(row, dict):
            raise ValueError("BULK market entry must be an object")
        symbol = _require_non_empty_str(row.get("symbol"), "bulk.symbol")
        if symbol in seen_symbols:
            raise ValueError(f"duplicate BULK symbol {symbol}")
        seen_symbols.add(symbol)

        tick_size = _format_decimal_str(row.get("tickSize"), f"{symbol}.tickSize")
        step_raw = row.get("sizeIncrement") if row.get("sizeIncrement") is not None else row.get("lotSize")
        if step_raw is not None:
            step_size = _format_decimal_str(step_raw, f"{symbol}.sizeIncrement")
        else:
            dec_raw = row.get("sizeDecimals") if row.get("sizeDecimals") is not None else row.get("sizePrecision")
            size_decimals = _require_int(dec_raw, f"{symbol}.sizeDecimals", min_value=0, max_value=12)
            step_size = _format_decimal(Decimal(1).scaleb(-size_decimals))

        min_notional = _parse_optional_decimal_str(row.get("minNotional"), f"{symbol}.minNotional", allow_zero=True)
        max_leverage = _parse_optional_leverage(row.get("maxLeverage"), f"{symbol}.maxLeverage")
        fee_evidence = _collect_fee_fields(
            row,
            ("makerFee", "takerFee", "makerFeeBps", "takerFeeBps", "fees"),
            source="/api/v1/exchangeInfo",
        )
        markets.append(
            _make_market(
                symbol=symbol,
                tick_size=tick_size,
                step_size=step_size,
                min_notional=min_notional,
                max_leverage=max_leverage,
                public_fee_evidence=fee_evidence,
            )
        )

    if not markets:
        raise ValueError("BULK market catalog is empty")
    return markets


def _parse_okx(payload: Any) -> list[dict[str, Any]]:
    _reject_error_envelope(payload, "okx")
    if not isinstance(payload, dict):
        raise ValueError("OKX /api/v5/public/instruments response must be an object")
    if "code" in payload and str(payload["code"]) != "0":
        raise ValueError(f"OKX instruments returned code={payload['code']!r}")
    rows = payload.get("data")
    if not isinstance(rows, list):
        raise ValueError("OKX response missing data list")

    seen_symbols: set[str] = set()
    markets: list[dict[str, Any]] = []
    for row in rows:
        if not isinstance(row, dict):
            raise ValueError("OKX instrument entry must be an object")
        if row.get("instType", "SWAP") != "SWAP":
            continue
        symbol = _require_non_empty_str(row.get("instId"), "okx.instId")
        if symbol in seen_symbols:
            raise ValueError(f"duplicate OKX instId {symbol}")
        seen_symbols.add(symbol)

        tick_size = _format_decimal_str(row.get("tickSz"), f"{symbol}.tickSz")
        step_size = _format_decimal_str(row.get("lotSz"), f"{symbol}.lotSz")
        min_notional = _parse_optional_decimal_str(row.get("minNotional"), f"{symbol}.minNotional", allow_zero=True)
        max_leverage = _parse_optional_leverage(row.get("lever"), f"{symbol}.lever")
        fee_evidence = _collect_fee_fields(
            row,
            ("makerFee", "takerFee", "feeGroup", "groupId"),
            source="/api/v5/public/instruments",
        )
        markets.append(
            _make_market(
                symbol=symbol,
                tick_size=tick_size,
                step_size=step_size,
                min_notional=min_notional,
                max_leverage=max_leverage,
                public_fee_evidence=fee_evidence,
            )
        )

    if not markets:
        raise ValueError("OKX SWAP instrument catalog is empty")
    return markets


def _parse_binance(payload: Any) -> list[dict[str, Any]]:
    _reject_error_envelope(payload, "binance")
    if not isinstance(payload, dict):
        raise ValueError("Binance /fapi/v1/exchangeInfo response must be an object")
    if "code" in payload and payload.get("code") not in (None, 0, 200):
        raise ValueError(f"Binance exchangeInfo error code={payload.get('code')!r}")
    rows = payload.get("symbols")
    if not isinstance(rows, list):
        raise ValueError("Binance response missing symbols list")

    seen_symbols: set[str] = set()
    markets: list[dict[str, Any]] = []
    for row in rows:
        if not isinstance(row, dict):
            raise ValueError("Binance symbol entry must be an object")
        contract_type = row.get("contractType", "PERPETUAL")
        if contract_type not in ("PERPETUAL", "TRADIFI_PERPETUAL", ""):
            continue
        symbol = _require_non_empty_str(row.get("symbol"), "binance.symbol")
        if symbol in seen_symbols:
            raise ValueError(f"duplicate Binance symbol {symbol}")
        seen_symbols.add(symbol)

        filters_raw = row.get("filters")
        if not isinstance(filters_raw, list):
            raise ValueError(f"Binance {symbol} missing filters list")
        by_type: dict[str, dict[str, Any]] = {}
        for flt in filters_raw:
            if isinstance(flt, dict) and isinstance(flt.get("filterType"), str):
                by_type[flt["filterType"]] = flt

        price_flt = by_type.get("PRICE_FILTER")
        lot_flt = by_type.get("LOT_SIZE")
        if price_flt is None or lot_flt is None:
            raise ValueError(f"Binance {symbol} missing PRICE_FILTER or LOT_SIZE")
        tick_size = _format_decimal_str(price_flt.get("tickSize"), f"{symbol}.PRICE_FILTER.tickSize")
        step_size = _format_decimal_str(lot_flt.get("stepSize"), f"{symbol}.LOT_SIZE.stepSize")

        min_notional_flt = by_type.get("MIN_NOTIONAL") or {}
        min_notional_raw = min_notional_flt.get("notional") if min_notional_flt.get("notional") is not None else min_notional_flt.get("minNotional")
        min_notional = _parse_optional_decimal_str(min_notional_raw, f"{symbol}.MIN_NOTIONAL", allow_zero=True)
        max_leverage = _parse_optional_leverage(row.get("maxLeverage"), f"{symbol}.maxLeverage")
        fee_evidence = _collect_fee_fields(
            row,
            ("makerFee", "takerFee", "feeTier"),
            source="/fapi/v1/exchangeInfo",
        )
        markets.append(
            _make_market(
                symbol=symbol,
                tick_size=tick_size,
                step_size=step_size,
                min_notional=min_notional,
                max_leverage=max_leverage,
                public_fee_evidence=fee_evidence,
            )
        )

    if not markets:
        raise ValueError("Binance perpetual symbol catalog is empty")
    return markets


VENUE_PARSERS: dict[str, Callable[[dict[str, Any]], list[dict[str, Any]]]] = {
    "arcus": _parse_arcus,
    "hyperliquid": lambda payloads: _parse_hyperliquid_dex(payloads, dex=""),
    "trade_xyz": lambda payloads: _parse_hyperliquid_dex(payloads, dex="xyz"),
    "lighter": lambda payloads: _parse_lighter(payloads["orderBookDetails"]),
    "backpack": lambda payloads: _parse_backpack(payloads["markets"]),
    "extended": lambda payloads: _parse_extended(payloads["markets"]),
    "bulk": lambda payloads: _parse_bulk(payloads["exchangeInfo"]),
    "okx": lambda payloads: _parse_okx(payloads["instruments"]),
    "binance": lambda payloads: _parse_binance(payloads["exchangeInfo"]),
}


def _extract_fixture_endpoint_raw(
    venue: str,
    fixtures: dict[str, Any],
    spec: dict[str, Any],
) -> bytes:
    if venue not in fixtures:
        raise ValueError(f"missing fixture payload for venue {venue}")
    venue_fixture = fixtures[venue]
    specs = VENUE_ENDPOINTS[venue]

    selected: Any
    if len(specs) == 1:
        if isinstance(venue_fixture, dict):
            if spec["key"] in venue_fixture:
                selected = venue_fixture[spec["key"]]
            elif spec["role"] in venue_fixture:
                selected = venue_fixture[spec["role"]]
            elif set(venue_fixture.keys()) == {"raw"}:
                selected = venue_fixture["raw"]
            else:
                selected = venue_fixture
        else:
            selected = venue_fixture
    else:
        if not isinstance(venue_fixture, dict):
            raise ValueError(f"fixture for multi-endpoint venue {venue} must be an object")
        if spec["key"] in venue_fixture:
            selected = venue_fixture[spec["key"]]
        elif spec["role"] in venue_fixture:
            selected = venue_fixture[spec["role"]]
        else:
            raise ValueError(f"fixture for {venue} missing endpoint {spec['role']!r}")
        if isinstance(selected, dict) and set(selected.keys()) == {"raw"}:
            selected = selected["raw"]

    return _canonical_raw_bytes(selected)


def audit_venue(
    venue: str,
    *,
    live: bool = False,
    fixtures: dict[str, Any] | None = None,
    fetcher: Callable[..., bytes] | None = None,
    fetched_at_utc: str | None = None,
) -> dict[str, Any]:
    """Audit one venue from live public endpoints or offline fixtures.

    Isolates network, JSON, and schema failures to `status = "error"` while
    preserving `account_fee_status = "unknown_requires_account_evidence"`.
    """
    if venue not in VENUE_ENDPOINTS:
        raise ValueError(f"unsupported venue {venue!r}")
    specs = VENUE_ENDPOINTS[venue]
    endpoints = [spec["key"] for spec in specs]
    timestamp = fetched_at_utc or _utc_now_iso()
    response_sha256: dict[str, str] = {}

    try:
        decoded_payloads: dict[str, Any] = {}
        for spec in specs:
            if live:
                fn = fetcher or _live_fetch_bytes
                raw_bytes = fn(spec["url"], method=spec["method"], body=spec["body"])
            else:
                if fixtures is None:
                    raise ValueError("offline audit requires fixture payloads")
                raw_bytes = _extract_fixture_endpoint_raw(venue, fixtures, spec)
            response_sha256[spec["key"]] = compute_response_sha256(raw_bytes)
            decoded_payloads[spec["role"]] = _decode_json_bytes(raw_bytes, f"{venue}:{spec['role']}")

        markets = VENUE_PARSERS[venue](decoded_payloads)
        return {
            "venue": venue,
            "status": "ok",
            "endpoints": endpoints,
            "fetched_at_utc": timestamp,
            "response_sha256": response_sha256,
            "account_fee_status": ACCOUNT_FEE_STATUS_UNKNOWN,
            "market_count": len(markets),
            "markets": markets,
        }
    except (OSError, ValueError, KeyError, TypeError, InvalidOperation, URLError) as exc:
        return {
            "venue": venue,
            "status": "error",
            "error": f"{type(exc).__name__}: {exc}",
            "endpoints": endpoints,
            "fetched_at_utc": timestamp,
            "response_sha256": response_sha256,
            "account_fee_status": ACCOUNT_FEE_STATUS_UNKNOWN,
            "market_count": 0,
            "markets": [],
        }


def audit_venues(
    venues: tuple[str, ...] | list[str] = SUPPORTED_VENUES,
    *,
    live: bool = False,
    fixtures: dict[str, Any] | None = None,
    fetcher: Callable[..., bytes] | None = None,
    fetched_at_utc: str | None = None,
) -> dict[str, Any]:
    """Audit all requested venues and return a structured snapshot document."""
    if not venues:
        raise ValueError("at least one venue must be selected")
    unknown = [v for v in venues if v not in VENUE_ENDPOINTS]
    if unknown:
        raise ValueError(f"unsupported venue(s): {', '.join(unknown)}")
    if not live and fixtures is None:
        raise ValueError("refusing to audit without --live or offline fixture payloads")

    ordered_venues = list(dict.fromkeys(venues))
    timestamp = fetched_at_utc or _utc_now_iso()
    results: dict[str, dict[str, Any]] = {}
    for venue in ordered_venues:
        results[venue] = audit_venue(
            venue,
            live=live,
            fixtures=fixtures,
            fetcher=fetcher,
            fetched_at_utc=timestamp,
        )

    ok_count = sum(1 for row in results.values() if row["status"] == "ok")
    err_count = len(results) - ok_count
    total_markets = sum(row["market_count"] for row in results.values())
    all_unknown = all(
        row.get("account_fee_status") == ACCOUNT_FEE_STATUS_UNKNOWN
        for row in results.values()
    )
    return {
        "schema_version": SCHEMA_VERSION,
        "mode": "live" if live else "fixture",
        "audited_at_utc": timestamp,
        "requested_venues": ordered_venues,
        "summary": {
            "total_venues": len(results),
            "ok_venues": ok_count,
            "error_venues": err_count,
            "total_markets": total_markets,
            "all_account_fee_status_unknown": all_unknown,
        },
        "venues": results,
    }


def _extract_venues_map(snapshot: dict[str, Any]) -> dict[str, dict[str, Any]]:
    if not isinstance(snapshot, dict):
        raise ValueError("snapshot must be a JSON object")
    if "venues" in snapshot:
        venues = snapshot["venues"]
        if isinstance(venues, dict):
            return venues
        if isinstance(venues, list):
            return {row["venue"]: row for row in venues}
        raise ValueError("snapshot['venues'] must be an object or list")
    if "venue" in snapshot and "markets" in snapshot:
        return {snapshot["venue"]: snapshot}
    raise ValueError("snapshot missing 'venues' mapping")


def _spec_values_equal(field: str, prev: Any, curr: Any) -> bool:
    if prev is None and curr is None:
        return True
    if prev is None or curr is None:
        return False
    if field in ("tick_size", "step_size", "min_notional", "max_leverage"):
        try:
            return _parse_decimal(prev, field) == _parse_decimal(curr, field)
        except ValueError:
            return prev == curr
    return prev == curr


def diff_spec_snapshots(previous: dict[str, Any], current: dict[str, Any]) -> dict[str, Any]:
    """Compare two normalized spec snapshots for symbol, parameter, fee, or status drift."""
    prev_venues = _extract_venues_map(previous)
    curr_venues = _extract_venues_map(current)
    filtered_requested = (
        set(current["requested_venues"])
        if isinstance(current.get("requested_venues"), list)
        else None
    )

    venue_names = sorted(set(prev_venues) | set(curr_venues))
    venue_diffs: dict[str, dict[str, Any]] = {}
    drifted_venues = 0
    status_regressions = 0
    total_added = 0
    total_removed = 0
    total_changed = 0

    for venue in venue_names:
        if venue not in curr_venues:
            if filtered_requested is not None and venue not in filtered_requested:
                continue
            venue_diffs[venue] = {
                "drift_detected": True,
                "previous_status": prev_venues[venue].get("status"),
                "current_status": "missing",
                "status_regression": True,
                "venue_removed": True,
                "added_symbols": [],
                "removed_symbols": [],
                "changed_markets": [],
            }
            drifted_venues += 1
            status_regressions += 1
            continue

        if venue not in prev_venues:
            curr_entry = curr_venues[venue]
            curr_syms = sorted(m["symbol"] for m in curr_entry.get("markets", []))
            venue_diffs[venue] = {
                "drift_detected": True,
                "previous_status": "missing",
                "current_status": curr_entry.get("status"),
                "status_regression": curr_entry.get("status") != "ok",
                "venue_added": True,
                "added_symbols": curr_syms,
                "removed_symbols": [],
                "changed_markets": [],
            }
            drifted_venues += 1
            if curr_entry.get("status") != "ok":
                status_regressions += 1
            total_added += len(curr_syms)
            continue

        prev_entry = prev_venues[venue]
        curr_entry = curr_venues[venue]
        prev_status = prev_entry.get("status")
        curr_status = curr_entry.get("status")
        status_changed = prev_status != curr_status
        is_regression = prev_status == "ok" and curr_status != "ok"
        error_changed = (
            prev_status != "ok"
            and curr_status != "ok"
            and prev_entry.get("error") != curr_entry.get("error")
        )

        added_symbols: list[str] = []
        removed_symbols: list[str] = []
        changed_markets: list[dict[str, Any]] = []

        if prev_status == "ok" and curr_status == "ok":
            prev_by_sym = {m["symbol"]: m for m in prev_entry.get("markets", [])}
            curr_by_sym = {m["symbol"]: m for m in curr_entry.get("markets", [])}
            added_symbols = sorted(set(curr_by_sym) - set(prev_by_sym))
            removed_symbols = sorted(set(prev_by_sym) - set(curr_by_sym))

            for sym in sorted(set(prev_by_sym) & set(curr_by_sym)):
                pm = prev_by_sym[sym]
                cm = curr_by_sym[sym]
                field_changes: dict[str, dict[str, Any]] = {}
                for field in ("tick_size", "step_size", "min_notional", "max_leverage", "public_fee_evidence"):
                    pv = pm.get(field)
                    cv = cm.get(field)
                    if not _spec_values_equal(field, pv, cv):
                        field_changes[field] = {"previous": pv, "current": cv}
                if field_changes:
                    changed_markets.append({"symbol": sym, "changes": field_changes})

        venue_drifted = bool(
            status_changed
            or is_regression
            or error_changed
            or added_symbols
            or removed_symbols
            or changed_markets
        )
        if venue_drifted:
            drifted_venues += 1
        if is_regression:
            status_regressions += 1
        total_added += len(added_symbols)
        total_removed += len(removed_symbols)
        total_changed += len(changed_markets)

        venue_diffs[venue] = {
            "drift_detected": venue_drifted,
            "previous_status": prev_status,
            "current_status": curr_status,
            "status_changed": status_changed,
            "status_regression": is_regression,
            "added_symbols": added_symbols,
            "removed_symbols": removed_symbols,
            "changed_markets": changed_markets,
        }

    return {
        "drift_detected": drifted_venues > 0,
        "summary": {
            "compared_venues": len(venue_diffs),
            "drifted_venues": drifted_venues,
            "status_regressions": status_regressions,
            "added_markets": total_added,
            "removed_markets": total_removed,
            "changed_markets": total_changed,
        },
        "venues": venue_diffs,
    }


def build_embedded_self_test_fixtures() -> dict[str, Any]:
    """Return realistic offline synthetic fixtures covering all 9 supported venues."""
    return {
        "arcus": {
            "markets": {
                "markets": [
                    {
                        "marketId": 1,
                        "marketDisplayName": "BTC-USD",
                        "status": "ONLINE",
                        "type": "PERPETUAL",
                        "baseAsset": "BTC",
                        "quoteAsset": "USD",
                        "category": "CRYPTO",
                        "tickSize": "0.1",
                        "stepSize": "0.001",
                        "minOrderSize": "0.001",
                        "maxOrderSize": "10",
                        "minOrderNotional": "5",
                        "initialMarginFraction": "0.02",
                        "hasCustomFeeSchedule": False,
                        "effectiveMakerFeePpm": 150,
                        "effectiveTakerFeePpm": 450,
                        "feeTiers": [
                            {"level": 0, "makerFeePpm": 150, "takerFeePpm": 450},
                            {"level": 4, "makerFeePpm": 0, "takerFeePpm": 230},
                        ],
                    },
                    {
                        "marketId": 88,
                        "marketDisplayName": "ANT-USD",
                        "status": "ONLINE",
                        "type": "PERPETUAL",
                        "baseAsset": "ANT",
                        "quoteAsset": "USD",
                        "category": "EQUITIES",
                        "tickSize": "0.001",
                        "stepSize": "0.000001",
                        "minOrderSize": "0.000001",
                        "maxOrderSize": "500",
                        "minOrderNotional": "5",
                        "initialMarginFraction": "0.1",
                        "hasCustomFeeSchedule": True,
                        "effectiveMakerFeePpm": 0,
                        "effectiveTakerFeePpm": 112,
                        "feeTiers": [
                            {"level": 0, "makerFeePpm": 0, "takerFeePpm": 112},
                        ],
                    },
                ]
            },
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
            },
        },
        "hyperliquid": {
            "perpDexs": [
                None,
                {"name": "xyz", "fullName": "TradeXYZ", "deployerFeeScale": "1.0"},
            ],
            "metaAndAssetCtxs": [
                {
                    "universe": [
                        {"name": "BTC", "szDecimals": 5, "maxLeverage": 50, "isDelisted": False},
                        {"name": "ETH", "szDecimals": 4, "maxLeverage": 50, "isDelisted": False},
                    ],
                    "collateralToken": 0,
                },
                [
                    {"funding": "0.0000125", "markPx": "90000", "oraclePx": "90005", "openInterest": "1000"},
                    {"funding": "0.0000125", "markPx": "3000", "oraclePx": "3001", "openInterest": "5000"},
                ],
            ],
        },
        "trade_xyz": {
            "perpDexs": [
                None,
                {"name": "xyz", "fullName": "TradeXYZ", "deployerFeeScale": "1.0"},
            ],
            "metaAndAssetCtxs": [
                {
                    "universe": [
                        {
                            "name": "xyz:ANT",
                            "szDecimals": 2,
                            "maxLeverage": 10,
                            "isDelisted": False,
                            "growthMode": "enabled",
                            "deployerFeeScale": "1.0",
                        },
                    ],
                    "collateralToken": 0,
                },
                [
                    {"funding": "0.00001", "markPx": "100", "oraclePx": "100", "openInterest": "250"},
                ],
            ],
        },
        "lighter": {
            "code": 200,
            "order_book_details": [
                {
                    "symbol": "ETH",
                    "market_id": 0,
                    "market_type": "perp",
                    "status": "active",
                    "taker_fee": "0.0000",
                    "maker_fee": "0.0000",
                    "liquidation_fee": "1.0000",
                    "min_base_amount": "0.005",
                    "min_quote_amount": "10",
                    "size_decimals": 4,
                    "price_decimals": 2,
                    "default_initial_margin_fraction": 200,
                },
                {
                    "symbol": "ETH_SPOT",
                    "market_id": 2048,
                    "market_type": "spot",
                    "status": "active",
                    "min_base_amount": "0.005",
                    "min_quote_amount": "10",
                    "size_decimals": 4,
                    "price_decimals": 2,
                },
            ],
        },
        "backpack": [
            {
                "symbol": "BTC_USDC_PERP",
                "baseSymbol": "BTC",
                "quoteSymbol": "USDC",
                "marketType": "PERP",
                "orderBookState": "Open",
                "filters": {
                    "price": {"tickSize": "0.1", "minPrice": "0.1"},
                    "quantity": {"stepSize": "0.001", "minQuantity": "0.001"},
                },
                "imfFunction": {"type": "sqrt", "base": "0.02", "factor": "0"},
            },
            {
                "symbol": "BTC_USDC",
                "baseSymbol": "BTC",
                "quoteSymbol": "USDC",
                "marketType": "SPOT",
                "orderBookState": "Open",
                "filters": {
                    "price": {"tickSize": "0.01"},
                    "quantity": {"stepSize": "0.0001"},
                },
            },
        ],
        "extended": {
            "status": "OK",
            "data": [
                {
                    "name": "BTC-USD",
                    "active": True,
                    "status": "ACTIVE",
                    "type": "PERPETUAL",
                    "isRfq": False,
                    "tradingConfig": {
                        "minOrderSize": "0.0001",
                        "minOrderSizeChange": "0.0001",
                        "minPriceChange": "1",
                        "maxLeverage": "50",
                    },
                }
            ],
        },
        "bulk": [
            {
                "symbol": "NVDA-USD",
                "baseAsset": "NVDA",
                "quoteAsset": "USD",
                "status": "TRADING",
                "tickSize": "0.01",
                "sizeDecimals": 3,
                "sizeIncrement": "0.001",
                "minNotional": "10",
            }
        ],
        "okx": {
            "code": "0",
            "msg": "",
            "data": [
                {
                    "instType": "SWAP",
                    "instId": "BTC-USDT-SWAP",
                    "ctVal": "0.01",
                    "ctMult": "1",
                    "ctValCcy": "BTC",
                    "ctType": "linear",
                    "settleCcy": "USDT",
                    "tickSz": "0.1",
                    "lotSz": "0.01",
                    "minSz": "0.01",
                    "lever": "100",
                    "state": "live",
                    "groupId": "1",
                }
            ],
        },
        "binance": {
            "timezone": "UTC",
            "symbols": [
                {
                    "symbol": "BTCUSDT",
                    "contractType": "PERPETUAL",
                    "status": "TRADING",
                    "baseAsset": "BTC",
                    "quoteAsset": "USDT",
                    "filters": [
                        {"filterType": "PRICE_FILTER", "tickSize": "0.10"},
                        {"filterType": "LOT_SIZE", "stepSize": "0.001", "minQty": "0.001"},
                        {"filterType": "MIN_NOTIONAL", "notional": "5"},
                    ],
                },
                {
                    "symbol": "NVDAUSDT",
                    "contractType": "TRADIFI_PERPETUAL",
                    "status": "TRADING",
                    "baseAsset": "NVDA",
                    "quoteAsset": "USDT",
                    "filters": [
                        {"filterType": "PRICE_FILTER", "tickSize": "0.01"},
                        {"filterType": "LOT_SIZE", "stepSize": "0.01", "minQty": "0.01"},
                        {"filterType": "MIN_NOTIONAL", "notional": "5"},
                    ],
                },
            ],
        },
    }


class _ForbiddenEnvGuard:
    """Block any os.environ read during offline audits/self-tests."""

    def __init__(self) -> None:
        self._orig = None

    def __enter__(self) -> "_ForbiddenEnvGuard":
        class _DeniedEnv:
            def __getitem__(self, key: str) -> str:
                raise AssertionError(f"offline audit must not read environment variable {key!r}")

            def get(self, key: str, default: Any = None) -> Any:
                raise AssertionError(f"offline audit must not read environment variable {key!r}")

            def __contains__(self, key: object) -> bool:
                raise AssertionError(f"offline audit must not inspect environment variable {key!r}")

            def keys(self) -> Any:
                raise AssertionError("offline audit must not enumerate environment variables")

            def items(self) -> Any:
                raise AssertionError("offline audit must not enumerate environment variables")

            def __iter__(self) -> Any:
                raise AssertionError("offline audit must not iterate environment variables")

            def __len__(self) -> int:
                raise AssertionError("offline audit must not inspect environment variable count")

            def copy(self) -> Any:
                raise AssertionError("offline audit must not copy environment variables")

        self._orig = os.environ
        os.environ = _DeniedEnv()  # type: ignore[assignment]
        return self

    def __exit__(self, exc_type: Any, exc: Any, tb: Any) -> None:
        if self._orig is not None:
            os.environ = self._orig


def run_self_test(venues: tuple[str, ...] | list[str] = SUPPORTED_VENUES) -> dict[str, Any]:
    """Run offline self-test over embedded synthetic fixtures for all 9 venues."""
    fixtures = build_embedded_self_test_fixtures()
    fixed_ts = "2026-11-22T00:00:00+00:00"

    with _ForbiddenEnvGuard():
        snap_a = audit_venues(venues, live=False, fixtures=fixtures, fetched_at_utc=fixed_ts)
        snap_b = audit_venues(venues, live=False, fixtures=fixtures, fetched_at_utc=fixed_ts)

        assert snap_a["summary"]["error_venues"] == 0, "embedded fixtures should all parse cleanly"
        assert snap_a["summary"]["all_account_fee_status_unknown"] is True
        for row in snap_a["venues"].values():
            assert row["status"] == "ok"
            assert row["account_fee_status"] == ACCOUNT_FEE_STATUS_UNKNOWN
            assert row["response_sha256"] and all(len(h) == 64 for h in row["response_sha256"].values())
            assert row["response_sha256"] == snap_b["venues"][row["venue"]]["response_sha256"]

        clean_diff = diff_spec_snapshots(snap_a, snap_b)
        assert clean_diff["drift_detected"] is False

        mutated_fixtures = json.loads(json.dumps(fixtures))
        if "arcus" in mutated_fixtures:
            mutated_fixtures["arcus"]["markets"]["markets"][0]["tickSize"] = "0.5"
        snap_mutated = audit_venues(venues, live=False, fixtures=mutated_fixtures, fetched_at_utc=fixed_ts)
        drift_diff = diff_spec_snapshots(snap_a, snap_mutated)
        if "arcus" in snap_a["venues"]:
            assert drift_diff["drift_detected"] is True

        broken_fixtures = json.loads(json.dumps(fixtures))
        broken_fixtures[venues[0]] = {"corrupt": "payload"}
        snap_err = audit_venues(venues, live=False, fixtures=broken_fixtures, fetched_at_utc=fixed_ts)
        assert snap_err["venues"][venues[0]]["status"] == "error"
        assert snap_err["venues"][venues[0]]["account_fee_status"] == ACCOUNT_FEE_STATUS_UNKNOWN
        if len(venues) > 1:
            assert snap_err["venues"][venues[1]]["status"] == "ok"

    return {
        "self_test": "passed",
        "venues_tested": list(snap_a["venues"].keys()),
        "total_markets": snap_a["summary"]["total_markets"],
        "all_account_fee_status_unknown": snap_a["summary"]["all_account_fee_status_unknown"],
        "provenance_verified": True,
        "zero_secret_env_reads_verified": True,
        "diff_clean_verified": not clean_diff["drift_detected"],
        "diff_drift_verified": drift_diff["drift_detected"] if "arcus" in snap_a["venues"] else True,
        "error_isolation_verified": snap_err["venues"][venues[0]]["status"] == "error",
        "sample_response_sha256": {
            venue: row["response_sha256"] for venue, row in snap_a["venues"].items()
        },
    }


def _parse_venues_arg(raw: str | None) -> tuple[str, ...]:
    if raw is None or not raw.strip():
        return SUPPORTED_VENUES
    items = [part.strip() for part in raw.split(",") if part.strip()]
    if not items:
        raise ValueError("--venues must list at least one supported venue")
    unknown = [v for v in items if v not in VENUE_ENDPOINTS]
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
        help="Explicitly issue read-only HTTPS GET/POST queries to public venue endpoints",
    )
    mode.add_argument(
        "--fixture",
        type=Path,
        help="Path to offline JSON fixture mapping venue names to raw/parsed public payloads",
    )
    mode.add_argument(
        "--self-test",
        action="store_true",
        help="Run embedded offline self-test over all 9 venues and exit",
    )
    parser.add_argument(
        "--venues",
        type=str,
        default=",".join(SUPPORTED_VENUES),
        help=f"Comma-separated venues to audit (default: {','.join(SUPPORTED_VENUES)})",
    )
    parser.add_argument(
        "--diff",
        type=Path,
        default=None,
        help="Compare current audit snapshot against a previous snapshot JSON file",
    )
    parser.add_argument(
        "--fail-on-drift",
        action="store_true",
        help="Exit with code 2 when --diff detects specification or status drift",
    )
    parser.add_argument(
        "--output",
        type=Path,
        default=None,
        help="Optional file path to write the JSON audit output",
    )
    args = parser.parse_args(argv)

    if not args.live and args.fixture is None and not args.self_test:
        print(
            "Error: refuse to run without an explicit mode; specify one of --live, --fixture <path.json>, or --self-test",
            file=sys.stderr,
        )
        return 2

    if args.fail_on_drift and args.diff is None and not args.self_test:
        print("Error: --fail-on-drift requires --diff <previous_snapshot.json>", file=sys.stderr)
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
        if args.output is not None:
            try:
                _write_json_output(args.output, summary)
            except OSError as exc:
                print(f"Error writing --output: {exc}", file=sys.stderr)
                return 2
        print(json.dumps(summary, indent=2, allow_nan=False))
        return 0

    fixtures: dict[str, Any] | None = None
    if args.fixture is not None:
        try:
            loaded = json.loads(args.fixture.read_text(encoding="utf-8"))
        except (OSError, UnicodeDecodeError, json.JSONDecodeError) as exc:
            print(f"Error reading --fixture {args.fixture}: {exc}", file=sys.stderr)
            return 2
        if not isinstance(loaded, dict):
            print("Error: --fixture JSON root must be an object", file=sys.stderr)
            return 2
        fixtures = loaded["venues"] if isinstance(loaded.get("venues"), dict) and not any(v in loaded for v in VENUE_ENDPOINTS) else loaded

    report = audit_venues(venues, live=args.live, fixtures=fixtures)

    diff_result: dict[str, Any] | None = None
    if args.diff is not None:
        try:
            previous_snapshot = json.loads(args.diff.read_text(encoding="utf-8"))
            diff_result = diff_spec_snapshots(previous_snapshot, report)
        except (OSError, UnicodeDecodeError, json.JSONDecodeError, ValueError, KeyError, TypeError) as exc:
            print(f"Error reading/diffing --diff {args.diff}: {exc}", file=sys.stderr)
            return 2
        report["diff"] = diff_result

    if args.output is not None:
        try:
            _write_json_output(args.output, report)
        except OSError as exc:
            print(f"Error writing --output {args.output}: {exc}", file=sys.stderr)
            return 2

    print(json.dumps(report, indent=2, allow_nan=False))

    if args.fail_on_drift and diff_result is not None and diff_result["drift_detected"]:
        return 2
    if report["summary"]["error_venues"] > 0:
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
