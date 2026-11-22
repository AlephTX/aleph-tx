#!/usr/bin/env python3
"""Read-only 9-venue cross-venue pair & multi-day carry universe screener.

Ingests an `opt01_venue_spec_audit_v1` JSON snapshot (such as the output of
`scripts/audit_venue_specs.py`), canonicalizes market symbols and clearing
domains across all 9 venues, computes exact Decimal lot-step LCMs, enforces
speed-bump and fee-transparency invariants, and optionally attaches read-only
public funding/mark probes via `--live-funding-probe`.

Strictly offline by default: never reads `.env*` files or secret/credential
environment variables, and never issues HTTP requests without `--live-funding-probe`.
"""

from __future__ import annotations

import argparse
from datetime import datetime, timezone
from decimal import Decimal, InvalidOperation
import json
import math
import os
from pathlib import Path
import re
import ssl
import sys
from typing import Any, Callable
from urllib.error import HTTPError, URLError
from urllib.request import HTTPRedirectHandler, HTTPSHandler, Request, build_opener

SCHEMA_VERSION = "cross_venue_pair_screener_v1"
EXPECTED_INPUT_SCHEMA = "opt01_venue_spec_audit_v1"
EXPECTED_ACCOUNT_FEE_EVIDENCE_SCHEMA = "aleph_account_fee_evidence_v1"
ACCOUNT_FEE_STATUS_UNKNOWN = "unknown_requires_account_evidence"
ACCOUNT_FEE_STATUS_VERIFIED = "verified_account_tier"
ACCOUNT_FEE_STATUS_PUBLIC_TIER0 = "public_tier0_schedule_only"
ACCOUNT_FEE_STATUS_PUBLIC_UNVERIFIED = "public_schedule_only_unverified_account_tier"
MAX_RESPONSE_BYTES = 16 * 1024 * 1024
DEFAULT_TIMEOUT_SECONDS = 15.0
DEFAULT_TOP_N = 50
HOURS_PER_YEAR = Decimal("8760")  # 24 * 365
BPS_PER_UNIT = Decimal("10000")
STEP_COMPAT_AMPLIFICATION_LIMIT = 1000

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
_VENUE_ORDER: dict[str, int] = {v: idx for idx, v in enumerate(SUPPORTED_VENUES)}

DOMAIN_USD_USDC = "USD_USDC_GROUP"
DOMAIN_USDT = "USDT_GROUP"
DOMAIN_COIN_M = "COIN_M_GROUP"
DOMAIN_USDG = "USDG_GROUP"
DOMAIN_USDH = "USDH_GROUP"
DOMAIN_USD1 = "USD1_GROUP"
DOMAIN_U = "U_GROUP"
DOMAIN_CRYPTO_QUOTE = "CRYPTO_QUOTE_GROUP"

_K_MEME_BASES: frozenset[str] = frozenset(
    {
        "PEPE",
        "BONK",
        "SHIB",
        "FLOKI",
        "LUNC",
        "DOGS",
        "NEIRO",
        "NOT",
        "SATS",
        "RATS",
        "CAT",
        "XEC",
        "CHEEMS",
        "WHY",
        "TOSHI",
    }
)

_KNOWN_EQUITY_BASES: frozenset[str] = frozenset(
    {
        "AAPL",
        "AAOI",
        "AMAT",
        "AMD",
        "AMZN",
        "ANT",
        "ARM",
        "ASML",
        "AVGO",
        "BABA",
        "BAC",
        "BB",
        "BE",
        "BMNR",
        "BOT",
        "BX",
        "CAMBRICON",
        "CBRS",
        "CCL",
        "COIN",
        "COST",
        "CRCL",
        "CRWD",
        "CRWV",
        "CVX",
        "CXMT",
        "DELL",
        "DKNG",
        "DRAM",
        "EBAY",
        "F",
        "GEV",
        "GIGADEV",
        "GLW",
        "GME",
        "GOOG",
        "GOOGL",
        "HIMS",
        "HOOD",
        "HYUNDAI",
        "IBM",
        "INTC",
        "IREN",
        "JIO",
        "KIOXIA",
        "KORU",
        "KSTR",
        "LITE",
        "LLY",
        "LRCX",
        "LYTE",
        "MAGS",
        "META",
        "MINIMAX",
        "MRNA",
        "MRVL",
        "MSFT",
        "MSTR",
        "MU",
        "MUU",
        "NBIS",
        "NCLD",
        "NET",
        "NFLX",
        "NOK",
        "NOW",
        "NVDA",
        "ORCL",
        "OURA",
        "PANW",
        "PLTR",
        "QCOM",
        "QQQ",
        "RDDT",
        "RELIANCE",
        "RIVN",
        "RKLB",
        "SAMSUNG",
        "SHAZ",
        "SHEIN",
        "SKHX",
        "SKHY",
        "SKHYNIX",
        "SMH",
        "SMIC",
        "SNDK",
        "SNXX",
        "SOFTBANK",
        "SOXL",
        "SPACEX",
        "SPCX",
        "SPY",
        "STRC",
        "TSLA",
        "TSM",
        "TTWO",
        "TWST",
        "UMC",
        "UNITREE",
        "URNM",
        "USAR",
        "VT",
        "WDC",
        "XBI",
        "XLE",
        "XIAOMI",
        "ZHIPU",
        "ZM",
        "ANTHROPIC",
        "OPENAI",
    }
)

_KNOWN_RWA_MACRO_BASES: frozenset[str] = frozenset(
    {
        "BRENTOIL",
        "CL",
        "COPPER",
        "CPER",
        "EUR",
        "EURUSD",
        "EWJ",
        "EWT",
        "EWY",
        "EWZ",
        "GBP",
        "GBPUSD",
        "GLD",
        "GOLD",
        "JP225",
        "JPY",
        "KR200",
        "NATGAS",
        "NIFTY",
        "NZDUSD",
        "PALLADIUM",
        "PLATINUM",
        "RVI",
        "SGOV",
        "SILVER",
        "SLV",
        "SP500",
        "SPX500M",
        "TECH100M",
        "TLT",
        "US100",
        "US10Y",
        "US500",
        "USDCAD",
        "USDCHF",
        "USDJPY",
        "USDKRW",
        "USO",
        "WTI",
        "XAG",
        "XAU",
        "XBR",
        "XCU",
        "XNG",
        "XPD",
        "XPT",
        "XYZ100",
    }
)

_AMBIGUOUS_TICKERS: frozenset[str] = frozenset({"QNT", "BOT", "LIT", "S", "AI", "MET", "CC", "IP", "W"})

_TOP_MEGA_CRYPTO: frozenset[str] = frozenset({"BTC", "ETH", "SOL", "XRP", "BNB", "DOGE", "ADA", "AVAX", "LINK", "SUI"})

_AUDITED_NO_SPEEDBUMP_VENUES: frozenset[str] = frozenset({"arcus", "hyperliquid"})
_SPEEDBUMP_VENUES: frozenset[str] = frozenset({"lighter", "lighter_robinhood"})
_RE_LOWER_K_PREFIX = re.compile(r"^k([A-Z0-9]+)$")
_RE_1M_PREFIX = re.compile(r"^1M([A-Z]{2,}[A-Z0-9]*)$")
_RE_1000000_PREFIX = re.compile(r"^1000000([A-Z][A-Z0-9]*)$")
_RE_1000_PREFIX = re.compile(r"^1000([A-Z][A-Z0-9]*)$")

LIVE_PROBE_ENDPOINTS: dict[str, dict[str, Any]] = {
    "arcus": {
        "url": "https://api.arcus.xyz/v1/markets",
        "method": "GET",
        "body": None,
    },
    "hyperliquid": {
        "url": "https://api.hyperliquid.xyz/info",
        "method": "POST",
        "body": {"type": "metaAndAssetCtxs"},
    },
    "trade_xyz": {
        "url": "https://api.hyperliquid.xyz/info",
        "method": "POST",
        "body": {"type": "metaAndAssetCtxs", "dex": "xyz"},
    },
    "lighter_funding": {
        "url": "https://mainnet.zklighter.elliot.ai/api/v1/funding-rates",
        "method": "GET",
        "body": None,
    },
    "lighter_details": {
        "url": "https://mainnet.zklighter.elliot.ai/api/v1/orderBookDetails",
        "method": "GET",
        "body": None,
    },
}


class _NoRedirectHandler(HTTPRedirectHandler):
    """Fail closed rather than silently following HTTP redirects."""

    def redirect_request(self, req: Request, fp: Any, code: int, msg: str, headers: Any, newurl: str) -> None:
        raise HTTPError(req.full_url, code, f"unexpected redirect ({msg})", headers, fp)


def _utc_now_iso() -> str:
    return datetime.now(timezone.utc).isoformat()


def _build_ssl_context() -> ssl.SSLContext:
    # Only invoked in --live-funding-probe mode; honors SSL_CERT_FILE for macOS Python installs.
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
        "User-Agent": "AlephTX-PairScreener/1.0",
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


def _parse_positive_decimal(raw: Any) -> Decimal | None:
    if raw is None or raw == "" or isinstance(raw, bool):
        return None
    try:
        val = Decimal(str(raw).strip())
    except (InvalidOperation, ValueError):
        return None
    if not val.is_finite() or val <= 0:
        return None
    return val


def _parse_optional_decimal(raw: Any) -> Decimal | None:
    if raw is None or raw == "" or isinstance(raw, bool):
        return None
    try:
        val = Decimal(str(raw).strip())
    except (InvalidOperation, ValueError):
        return None
    if not val.is_finite():
        return None
    return val


def _format_decimal(val: Decimal) -> str:
    if val == 0:
        return "0"
    norm = val.normalize()
    text = format(norm, "f")
    if "." in text:
        text = text.rstrip("0").rstrip(".")
    return text


def _normalize_scaled_base(raw_base: str, *, allow_uppercase_k: bool = False) -> tuple[str, str, int]:
    """Normalize 1000x / 1Mx / k-prefixed meme tickers to canonical base + multiplier."""
    m_1m = _RE_1000000_PREFIX.match(raw_base)
    if m_1m:
        underlying = m_1m.group(1)
        return f"1000000{underlying}", underlying, 1_000_000
    m_1m_short = _RE_1M_PREFIX.match(raw_base)
    if m_1m_short:
        underlying = m_1m_short.group(1)
        return f"1000000{underlying}", underlying, 1_000_000
    m_1000 = _RE_1000_PREFIX.match(raw_base)
    if m_1000:
        underlying = m_1000.group(1)
        return f"1000{underlying}", underlying, 1000
    m_k = _RE_LOWER_K_PREFIX.match(raw_base)
    if m_k:
        underlying = m_k.group(1)
        return f"1000{underlying}", underlying, 1000
    if allow_uppercase_k and raw_base.startswith("K") and len(raw_base) > 1:
        underlying = raw_base[1:]
        if underlying in _K_MEME_BASES:
            return f"1000{underlying}", underlying, 1000
    return raw_base, raw_base, 1


def _infer_asset_class_hint(
    venue: str,
    base: str,
    *,
    explicit_hint: str | None = None,
    fee_evidence: dict[str, Any] | None = None,
) -> str:
    if explicit_hint is not None:
        return explicit_hint
    upper = base.upper()
    if venue == "arcus" and isinstance(fee_evidence, dict):
        tiers = fee_evidence.get("tiers")
        if isinstance(tiers, list) and tiers and isinstance(tiers[0], dict):
            maker_ppm = tiers[0].get("maker_fee_ppm")
            maker_bps = _parse_optional_decimal(tiers[0].get("maker_fee_bps"))
            if maker_ppm == 0 or maker_bps == Decimal("0"):
                if upper in _KNOWN_RWA_MACRO_BASES:
                    return "rwa"
                return "equity"
        return "crypto"
    if upper in _KNOWN_EQUITY_BASES:
        return "equity"
    if upper in _KNOWN_RWA_MACRO_BASES:
        return "rwa"
    if venue == "trade_xyz":
        return "equity"
    return "crypto"


def _compute_hip3_fees_bps(
    base_maker_bps: Decimal,
    base_taker_bps: Decimal,
    deployer_fee_scale: Decimal,
    growth_mode_enabled: bool,
) -> tuple[Decimal, Decimal]:
    """Derive effective HIP-3 maker/taker bps matching `src/venues.rs::compute_hip3_execution_fees`."""
    if base_maker_bps < Decimal("0") or base_taker_bps < Decimal("0"):
        raise ValueError("HIP-3 base fees must be nonnegative")
    if deployer_fee_scale < Decimal("0") or deployer_fee_scale > Decimal("5"):
        raise ValueError("HIP-3 deployer_fee_scale must lie in [0, 5]")
    scale_factor = (
        Decimal("1") + deployer_fee_scale
        if deployer_fee_scale < Decimal("1")
        else Decimal("2") * deployer_fee_scale
    )
    growth_factor = Decimal("0.10") if growth_mode_enabled else Decimal("1")
    return (
        (base_maker_bps * scale_factor * growth_factor).normalize(),
        (base_taker_bps * scale_factor * growth_factor).normalize(),
    )


def _validate_account_fee_evidence(evidence: Any) -> dict[str, dict[str, Any]]:
    """Validate an `aleph_account_fee_evidence_v1` payload and return its venues map."""
    if not isinstance(evidence, dict):
        raise ValueError("account_fee_evidence must be a JSON object")
    if evidence.get("schema_version") != EXPECTED_ACCOUNT_FEE_EVIDENCE_SCHEMA:
        raise ValueError(
            f"unsupported account_fee_evidence schema_version {evidence.get('schema_version')!r}; "
            f"expected {EXPECTED_ACCOUNT_FEE_EVIDENCE_SCHEMA!r}"
        )
    venues_obj = evidence.get("venues")
    if not isinstance(venues_obj, dict) or not venues_obj:
        raise ValueError("account_fee_evidence missing non-empty 'venues' mapping")

    validated: dict[str, dict[str, Any]] = {}
    for venue_name, row in venues_obj.items():
        if not isinstance(venue_name, str) or not isinstance(row, dict):
            raise ValueError(f"invalid venue record in account_fee_evidence for {venue_name!r}")
        status = row.get("account_fee_status")
        if not isinstance(status, str) or not status.strip():
            raise ValueError(f"missing account_fee_status for {venue_name!r} in account_fee_evidence")
        if status == ACCOUNT_FEE_STATUS_VERIFIED:
            if row.get("status") == "error":
                raise ValueError(f"inconsistent error status with verified_account_tier for {venue_name!r}")
            if venue_name == "trade_xyz":
                base_m = _parse_optional_decimal(row.get("base_maker_fee_bps"))
                base_t = _parse_optional_decimal(row.get("base_taker_fee_bps"))
                has_base = base_m is not None and base_t is not None
                if (base_m is None) != (base_t is None):
                    raise ValueError("trade_xyz verified evidence requires both base_maker_fee_bps and base_taker_fee_bps")
                if has_base and (base_m < Decimal("0") or base_t < Decimal("0")):  # type: ignore[operator]
                    raise ValueError("trade_xyz base fees must be nonnegative")
                if row.get("deployer_fee_scale") is not None:
                    scale_dec = _parse_optional_decimal(row.get("deployer_fee_scale"))
                    if scale_dec is None or scale_dec < Decimal("0") or scale_dec > Decimal("5"):
                        raise ValueError("trade_xyz deployer_fee_scale must lie in [0, 5]")
                std_m = _parse_optional_decimal(row.get("standard_maker_fee_bps"))
                std_t = _parse_optional_decimal(row.get("standard_taker_fee_bps"))
                gm_m = _parse_optional_decimal(row.get("growth_mode_maker_fee_bps"))
                gm_t = _parse_optional_decimal(row.get("growth_mode_taker_fee_bps"))
                def_m = _parse_optional_decimal(row.get("maker_fee_bps"))
                def_t = _parse_optional_decimal(row.get("taker_fee_bps"))
                has_split = all(v is not None for v in (std_m, std_t, gm_m, gm_t))
                has_default = def_m is not None and def_t is not None
                if not (has_base or has_split or has_default):
                    raise ValueError(
                        "trade_xyz verified account_fee_evidence requires base_* or standard_*/growth_mode_* or maker/taker fees"
                    )
            else:
                m_dec = _parse_optional_decimal(row.get("maker_fee_bps"))
                t_dec = _parse_optional_decimal(row.get("taker_fee_bps"))
                if m_dec is None or t_dec is None:
                    raise ValueError(
                        f"verified_account_tier for {venue_name!r} requires numeric maker_fee_bps and taker_fee_bps"
                    )
        validated[venue_name] = row
    return validated


def _apply_account_fee_evidence(canon: dict[str, Any], venue_evidence: dict[str, Any] | None) -> None:
    """Apply verified account fee-tier overrides or fail closed unverified account tiers."""
    venue = canon["venue"]
    ev = venue_evidence if isinstance(venue_evidence, dict) else {}
    ev_status = ev.get("account_fee_status")

    if ev_status == ACCOUNT_FEE_STATUS_VERIFIED:
        if venue == "trade_xyz":
            fe = canon.get("public_fee_evidence") if isinstance(canon.get("public_fee_evidence"), dict) else {}
            growth_mode = fe.get("growth_mode")
            if growth_mode not in (None, "enabled", "disabled"):
                canon["maker_fee_bps"] = None
                canon["taker_fee_bps"] = None
                canon["fee_basis"] = "invalid_growth_mode"
                canon["account_fee_status"] = ACCOUNT_FEE_STATUS_UNKNOWN
                canon["fee_provenance"] = "failed_closed_invalid_growth_mode"
                return
            growth_enabled = growth_mode == "enabled"
            raw_scale = fe.get("deployer_fee_scale")
            if raw_scale is None:
                raw_scale = fe.get("dex_deployer_fee_scale")
            if raw_scale is None:
                raw_scale = ev.get("deployer_fee_scale")
            market_scale = _parse_optional_decimal(raw_scale)
            base_maker = _parse_optional_decimal(ev.get("base_maker_fee_bps"))
            base_taker = _parse_optional_decimal(ev.get("base_taker_fee_bps"))

            if (
                base_maker is not None
                and base_taker is not None
                and base_maker >= Decimal("0")
                and base_taker >= Decimal("0")
                and market_scale is not None
                and Decimal("0") <= market_scale <= Decimal("5")
            ):
                m_bps, t_bps = _compute_hip3_fees_bps(base_maker, base_taker, market_scale, growth_enabled)
                canon["maker_fee_bps"] = _format_decimal(m_bps)
                canon["taker_fee_bps"] = _format_decimal(t_bps)
                canon["fee_basis"] = (
                    "hip3_growth_mode_verified_account_tier"
                    if growth_enabled
                    else "hip3_standard_verified_account_tier"
                )
                canon["account_fee_status"] = ACCOUNT_FEE_STATUS_VERIFIED
                canon["fee_provenance"] = "verified_account_fee_evidence"
                return

            ev_scale = _parse_optional_decimal(ev.get("deployer_fee_scale"))
            if market_scale is None or ev_scale is None or market_scale == ev_scale:
                if growth_enabled:
                    m_bps = _parse_optional_decimal(ev.get("growth_mode_maker_fee_bps"))
                    t_bps = _parse_optional_decimal(ev.get("growth_mode_taker_fee_bps"))
                    basis = "hip3_growth_mode_verified_account_tier"
                else:
                    m_bps = _parse_optional_decimal(ev.get("standard_maker_fee_bps"))
                    t_bps = _parse_optional_decimal(ev.get("standard_taker_fee_bps"))
                    basis = "hip3_standard_verified_account_tier"
                if m_bps is None or t_bps is None:
                    m_bps = _parse_optional_decimal(ev.get("maker_fee_bps"))
                    t_bps = _parse_optional_decimal(ev.get("taker_fee_bps"))
                    basis = "verified_account_fee_evidence"
                if m_bps is not None and t_bps is not None:
                    canon["maker_fee_bps"] = _format_decimal(m_bps)
                    canon["taker_fee_bps"] = _format_decimal(t_bps)
                    canon["fee_basis"] = basis
                    canon["account_fee_status"] = ACCOUNT_FEE_STATUS_VERIFIED
                    canon["fee_provenance"] = "verified_account_fee_evidence"
                    return

            canon["maker_fee_bps"] = None
            canon["taker_fee_bps"] = None
            canon["fee_basis"] = "unverified_hip3_scale_or_missing_fees"
            canon["account_fee_status"] = ACCOUNT_FEE_STATUS_UNKNOWN
            canon["fee_provenance"] = "failed_closed_unverified_hip3_evidence"
            return

        m_bps = _parse_optional_decimal(ev.get("maker_fee_bps"))
        t_bps = _parse_optional_decimal(ev.get("taker_fee_bps"))
        if m_bps is not None and t_bps is not None:
            canon["maker_fee_bps"] = _format_decimal(m_bps)
            canon["taker_fee_bps"] = _format_decimal(t_bps)
            canon["fee_basis"] = "verified_account_fee_evidence"
            canon["account_fee_status"] = ACCOUNT_FEE_STATUS_VERIFIED
            canon["fee_provenance"] = "verified_account_fee_evidence"
            return

        canon["maker_fee_bps"] = None
        canon["taker_fee_bps"] = None
        canon["fee_basis"] = "missing_verified_fee_fields"
        canon["account_fee_status"] = ACCOUNT_FEE_STATUS_UNKNOWN
        canon["fee_provenance"] = "failed_closed_missing_verified_fees"
        return

    if venue == "arcus" and (ev_status == ACCOUNT_FEE_STATUS_PUBLIC_TIER0 or venue_evidence is None):
        canon["account_fee_status"] = ACCOUNT_FEE_STATUS_UNKNOWN
        canon["fee_provenance"] = ev_status or ACCOUNT_FEE_STATUS_PUBLIC_TIER0
        return

    if venue == "trade_xyz" and venue_evidence is None:
        canon["account_fee_status"] = ACCOUNT_FEE_STATUS_UNKNOWN
        canon["fee_provenance"] = "public_hip3_l0_derived_only"
        return

    # Unverified venues (including Lighter when public_schedule_only_unverified_account_tier) fail closed.
    canon["maker_fee_bps"] = None
    canon["taker_fee_bps"] = None
    canon["account_fee_status"] = ACCOUNT_FEE_STATUS_UNKNOWN
    canon["fee_basis"] = ev_status or ACCOUNT_FEE_STATUS_UNKNOWN
    canon["fee_provenance"] = ev_status or ACCOUNT_FEE_STATUS_UNKNOWN


def _extract_public_fees(venue: str, fee_evidence: dict[str, Any] | None) -> dict[str, Any]:
    """Extract strictly verified public fees or preserve `null` with unknown account status."""
    fe = fee_evidence if isinstance(fee_evidence, dict) else {}
    if fe.get("status") == "not_exposed_in_public_endpoint":
        return {
            "maker_fee_bps": None,
            "taker_fee_bps": None,
            "fee_basis": "not_exposed_in_public_endpoint",
            "account_fee_status": ACCOUNT_FEE_STATUS_UNKNOWN,
        }

    if venue == "arcus":
        tiers = fe.get("tiers")
        if isinstance(tiers, list) and tiers:
            l0 = next((t for t in tiers if isinstance(t, dict) and t.get("level") == 0), tiers[0])
            if isinstance(l0, dict):
                m_bps = _parse_optional_decimal(l0.get("maker_fee_bps"))
                t_bps = _parse_optional_decimal(l0.get("taker_fee_bps"))
                if m_bps is None and l0.get("maker_fee_ppm") is not None:
                    m_bps = Decimal(l0["maker_fee_ppm"]) / Decimal(100)
                if t_bps is None and l0.get("taker_fee_ppm") is not None:
                    t_bps = Decimal(l0["taker_fee_ppm"]) / Decimal(100)
                if m_bps is not None and t_bps is not None:
                    return {
                        "maker_fee_bps": _format_decimal(m_bps),
                        "taker_fee_bps": _format_decimal(t_bps),
                        "fee_basis": "public_l0_schedule",
                        "account_fee_status": ACCOUNT_FEE_STATUS_UNKNOWN,
                    }

    elif venue == "lighter":
        m_raw = fe.get("maker_fee")
        t_raw = fe.get("taker_fee")
        m_val = _parse_optional_decimal(m_raw)
        t_val = _parse_optional_decimal(t_raw)
        if m_val is not None and t_val is not None:
            # Lighter /orderBookDetails reports fee rates as percentages (e.g. "0.0000", "1.0000" for 1%).
            m_bps = (m_val * Decimal("100")).normalize()
            t_bps = (t_val * Decimal("100")).normalize()
            return {
                "maker_fee_bps": _format_decimal(m_bps),
                "taker_fee_bps": _format_decimal(t_bps),
                "fee_basis": "public_standard_schedule",
                "account_fee_status": ACCOUNT_FEE_STATUS_UNKNOWN,
            }

    elif venue == "trade_xyz":
        scale_raw = fe.get("deployer_fee_scale")
        if scale_raw is None:
            scale_raw = fe.get("dex_deployer_fee_scale")
        scale = _parse_optional_decimal(scale_raw)
        if scale is not None and Decimal("0") <= scale <= Decimal("5"):
            base_maker = Decimal("1.50")
            base_taker = Decimal("4.50")
            scale_factor = (Decimal("1") + scale) if scale < Decimal("1") else (Decimal("2") * scale)
            growth_mode = fe.get("growth_mode")
            growth_factor = Decimal("0.10") if growth_mode == "enabled" else Decimal("1")
            m_bps = (base_maker * scale_factor * growth_factor).normalize()
            t_bps = (base_taker * scale_factor * growth_factor).normalize()
            return {
                "maker_fee_bps": _format_decimal(m_bps),
                "taker_fee_bps": _format_decimal(t_bps),
                "fee_basis": (
                    "hip3_growth_mode_derived_l0"
                    if growth_mode == "enabled"
                    else "hip3_regular_derived_l0"
                ),
                "account_fee_status": ACCOUNT_FEE_STATUS_UNKNOWN,
            }

    # Explicit non-defaulted fallback for unexposed fees (e.g. OKX groupId-only, HL core, backpack, extended, bulk, binance)
    return {
        "maker_fee_bps": None,
        "taker_fee_bps": None,
        "fee_basis": "not_exposed_in_public_endpoint",
        "account_fee_status": ACCOUNT_FEE_STATUS_UNKNOWN,
    }


def canonicalize_market(venue: str, market_dict: dict[str, Any]) -> dict[str, Any] | None:
    """Canonicalize one normalized market entry from `opt01_venue_spec_audit_v1`.

    Returns `None` when the contract is inactive, delisted, disambiguated with `#`,
    dated delivery, placeholder/staging, or non-perpetual.
    """
    if not isinstance(market_dict, dict):
        return None
    raw_symbol = market_dict.get("symbol")
    if not isinstance(raw_symbol, str) or not raw_symbol.strip():
        return None
    symbol = raw_symbol.strip()

    fe = market_dict.get("public_fee_evidence")
    fe_dict = fe if isinstance(fe, dict) else {}

    # Fail-closed exclusion of inactive, delisted, or legacy disambiguated contracts
    if fe_dict.get("is_delisted") is True:
        return None
    market_status = str(fe_dict.get("market_status", "")).strip().lower()
    if market_status == "inactive":
        return None
    if "#" in symbol or symbol.upper().endswith("_DELISTED"):
        return None

    venue_key = venue.strip().lower()
    flags: list[str] = []
    explicit_hint: str | None = None
    session_schedule = "24_7"
    quote_asset: str
    quote_domain: str
    collateral_asset: str
    raw_base: str

    if venue_key == "arcus":
        if not symbol.endswith("-USD"):
            return None
        raw_base = symbol[:-4]
        quote_asset = "USD"
        quote_domain = DOMAIN_USD_USDC
        collateral_asset = "USDC"
        canonical_base, underlying_base, unit_multiplier = _normalize_scaled_base(
            raw_base, allow_uppercase_k=True
        )

    elif venue_key == "trade_xyz":
        if ":" in symbol:
            _, raw_base = symbol.split(":", 1)
        else:
            raw_base = symbol
        if not raw_base:
            return None
        collat_tok = fe_dict.get("collateral_token")
        if collat_tok is None or collat_tok == 0:
            collateral_asset = "USDC"
            quote_asset = "USD"
            quote_domain = DOMAIN_USD_USDC
        elif collat_tok == 3:
            collateral_asset = "USDH"
            quote_asset = "USDH"
            quote_domain = DOMAIN_USDH
            flags.append("non_usdc_collateral_usdh")
        else:
            collateral_asset = f"HL_TOKEN_{collat_tok}"
            quote_asset = collateral_asset
            quote_domain = f"{collateral_asset}_GROUP"
            flags.append(f"non_usdc_collateral_{collateral_asset.lower()}")
        canonical_base, underlying_base, unit_multiplier = _normalize_scaled_base(raw_base)

    elif venue_key == "hyperliquid":
        if ":" in symbol:
            return None
        raw_base = symbol
        quote_asset = "USD"
        quote_domain = DOMAIN_USD_USDC
        collateral_asset = "USDC"
        canonical_base, underlying_base, unit_multiplier = _normalize_scaled_base(raw_base)
        explicit_hint = "crypto"

    elif venue_key == "lighter":
        raw_base = symbol
        explicit_collateral = str(
            fe_dict.get("settlement_currency") or fe_dict.get("collateral_asset") or ""
        ).upper()
        explicit_domain = str(fe_dict.get("liquidity_domain") or "").lower()
        if (
            explicit_collateral == "USDG"
            or "robinhood" in explicit_domain
            or symbol.endswith("_USDG")
            or symbol.endswith("-USDG")
        ):
            collateral_asset = "USDG"
            quote_asset = "USDG"
            quote_domain = DOMAIN_USDG
            if raw_base.endswith("_USDG") or raw_base.endswith("-USDG"):
                raw_base = raw_base[:-5]
            flags.append("lighter_robinhood_usdg_domain")
        else:
            collateral_asset = "USDC"
            quote_asset = "USD"
            quote_domain = DOMAIN_USD_USDC
        canonical_base, underlying_base, unit_multiplier = _normalize_scaled_base(raw_base)

    elif venue_key == "backpack":
        if not symbol.endswith("_USDC_PERP"):
            return None
        raw_base = symbol[: -len("_USDC_PERP")]
        if raw_base.endswith(".US"):
            raw_base = raw_base[:-3]
            explicit_hint = "equity"
        quote_asset = "USDC"
        quote_domain = DOMAIN_USD_USDC
        collateral_asset = "USDC"
        canonical_base, underlying_base, unit_multiplier = _normalize_scaled_base(raw_base)

    elif venue_key == "extended":
        if not symbol.endswith("-USD"):
            return None
        raw_base = symbol[:-4]
        if raw_base.startswith("PLACE_"):
            return None
        if raw_base.endswith("_24_5"):
            raw_base = raw_base[:-5]
            explicit_hint = "equity"
            session_schedule = "24_5"
            flags.append("extended_24_5_equity_hours")
        quote_asset = "USD"
        quote_domain = DOMAIN_USD_USDC
        collateral_asset = "USDC"
        canonical_base, underlying_base, unit_multiplier = _normalize_scaled_base(raw_base)

    elif venue_key == "bulk":
        if not symbol.endswith("-USD"):
            return None
        raw_base = symbol[:-4]
        quote_asset = "USD"
        quote_domain = DOMAIN_USD_USDC
        collateral_asset = "USDC"
        canonical_base, underlying_base, unit_multiplier = _normalize_scaled_base(raw_base)

    elif venue_key == "okx":
        if symbol.endswith("-USDT-SWAP"):
            raw_base = symbol[: -len("-USDT-SWAP")]
            quote_asset = "USDT"
            quote_domain = DOMAIN_USDT
            collateral_asset = "USDT"
        elif symbol.endswith("-USDC-SWAP"):
            raw_base = symbol[: -len("-USDC-SWAP")]
            quote_asset = "USDC"
            quote_domain = DOMAIN_USD_USDC
            collateral_asset = "USDC"
        elif symbol.endswith("-USD-SWAP"):
            raw_base = symbol[: -len("-USD-SWAP")]
            quote_asset = "USD"
            quote_domain = DOMAIN_COIN_M
            collateral_asset = raw_base
            flags.append("coin_m_inverse_swap")
        else:
            return None
        flags.append("okx_step_size_is_contract_lot")
        canonical_base, underlying_base, unit_multiplier = _normalize_scaled_base(raw_base)

    elif venue_key == "binance":
        if "_" in symbol:
            return None
        if symbol.endswith("USDT"):
            raw_base = symbol[:-4]
            quote_asset = "USDT"
            quote_domain = DOMAIN_USDT
            collateral_asset = "USDT"
        elif symbol.endswith("USDC"):
            raw_base = symbol[:-4]
            quote_asset = "USDC"
            quote_domain = DOMAIN_USD_USDC
            collateral_asset = "USDC"
        elif symbol.endswith("USD1"):
            raw_base = symbol[:-4]
            quote_asset = "USD1"
            quote_domain = DOMAIN_USD1
            collateral_asset = "USD1"
            flags.append("non_usdc_usdt_quote_usd1")
        elif symbol.endswith("BTC") and len(symbol) > 3:
            raw_base = symbol[:-3]
            quote_asset = "BTC"
            quote_domain = DOMAIN_CRYPTO_QUOTE
            collateral_asset = "BTC"
            flags.append("coin_quoted_perp")
        elif symbol in ("BTCU", "ETHU") or (symbol.endswith("U") and len(symbol) >= 4 and symbol[:-1] in ("BTC", "ETH")):
            raw_base = symbol[:-1]
            quote_asset = "U"
            quote_domain = DOMAIN_U
            collateral_asset = "U"
            flags.append("non_usdc_usdt_quote_u")
        else:
            return None
        canonical_base, underlying_base, unit_multiplier = _normalize_scaled_base(raw_base)

    else:
        raise ValueError(f"unsupported venue {venue!r}")

    if not canonical_base:
        return None

    if canonical_base.upper() in _AMBIGUOUS_TICKERS:
        flags.append("ambiguous_ticker_verify_underlying")

    asset_class_hint = _infer_asset_class_hint(
        venue_key,
        underlying_base,
        explicit_hint=explicit_hint,
        fee_evidence=fe_dict,
    )
    fees = _extract_public_fees(venue_key, fe_dict)

    return {
        "venue": venue_key,
        "symbol": symbol,
        "base": canonical_base,
        "underlying_base": underlying_base,
        "unit_multiplier": unit_multiplier,
        "quote_asset": quote_asset,
        "quote_domain": quote_domain,
        "collateral_asset": collateral_asset,
        "asset_class_hint": asset_class_hint,
        "session_schedule": session_schedule,
        "tick_size": market_dict.get("tick_size"),
        "step_size": market_dict.get("step_size"),
        "min_notional": market_dict.get("min_notional"),
        "max_leverage": market_dict.get("max_leverage"),
        "maker_fee_bps": fees["maker_fee_bps"],
        "taker_fee_bps": fees["taker_fee_bps"],
        "fee_basis": fees["fee_basis"],
        "account_fee_status": fees["account_fee_status"],
        "public_fee_evidence": fe_dict,
        "flags": flags,
    }


def compute_step_lcm(step_a: Decimal, step_b: Decimal) -> tuple[Decimal, int, int]:
    """Compute exact Decimal step LCM using scaled integer `math.lcm`.

    Returns `(step_lcm, step_ratio_a, step_ratio_b)` where `step_ratio_a = step_lcm / step_a`
    and `step_ratio_b = step_lcm / step_b` are exact positive integers.
    """
    if not step_a.is_finite() or not step_b.is_finite() or step_a <= 0 or step_b <= 0:
        raise ValueError("step_a and step_b must be positive finite Decimals")
    a_norm = step_a.normalize()
    b_norm = step_b.normalize()
    exp_a = max(-int(a_norm.as_tuple().exponent), 0)
    exp_b = max(-int(b_norm.as_tuple().exponent), 0)
    scale = max(exp_a, exp_b)
    factor = Decimal(10) ** scale
    int_a = int((a_norm * factor).to_integral_exact())
    int_b = int((b_norm * factor).to_integral_exact())
    lcm_int = math.lcm(int_a, int_b)
    lcm_dec = (Decimal(lcm_int) / factor).normalize()
    ratio_a = lcm_int // int_a
    ratio_b = lcm_int // int_b
    return lcm_dec, ratio_a, ratio_b


def _extract_venues_map(snapshot: dict[str, Any]) -> dict[str, dict[str, Any]]:
    if not isinstance(snapshot, dict):
        raise ValueError("snapshot must be a JSON object")
    if "venues" in snapshot:
        venues = snapshot["venues"]
        if isinstance(venues, dict):
            return venues
        if isinstance(venues, list):
            return {row["venue"]: row for row in venues if isinstance(row, dict) and "venue" in row}
        raise ValueError("snapshot['venues'] must be an object or list")
    if "venue" in snapshot and "markets" in snapshot:
        return {snapshot["venue"]: snapshot}
    raise ValueError("snapshot missing 'venues' mapping")


def _parse_venues_arg(raw: str | None) -> tuple[str, ...]:
    if raw is None or not raw.strip():
        return SUPPORTED_VENUES
    items = [part.strip().lower() for part in raw.split(",") if part.strip()]
    if not items:
        raise ValueError("--venues must list at least one supported venue")
    unknown = [v for v in items if v not in _VENUE_ORDER]
    if unknown:
        raise ValueError(
            f"unsupported venue(s): {', '.join(unknown)}; supported: {', '.join(SUPPORTED_VENUES)}"
        )
    return tuple(dict.fromkeys(items))


def _assess_pair_topology_and_costs(leg_a: dict[str, Any], leg_b: dict[str, Any]) -> dict[str, Any]:
    va = leg_a["venue"]
    vb = leg_b["venue"]
    maker_a = _parse_optional_decimal(leg_a["maker_fee_bps"])
    taker_a = _parse_optional_decimal(leg_a["taker_fee_bps"])
    maker_b = _parse_optional_decimal(leg_b["maker_fee_bps"])
    taker_b = _parse_optional_decimal(leg_b["taker_fee_bps"])
    both_fees_known = all(v is not None for v in (maker_a, taker_a, maker_b, taker_b))

    reasons: list[str] = []
    a_is_slow = va in _SPEEDBUMP_VENUES
    b_is_slow = vb in _SPEEDBUMP_VENUES
    concurrent_allowed = not (a_is_slow or b_is_slow)

    if a_is_slow and b_is_slow:
        recommended_topology = "dual_slow_infeasible"
        maker_leg_venue: str | None = None
        hedge_leg_venue: str | None = None
        reasons.append("both_legs_have_speed_bump")
    elif a_is_slow or b_is_slow:
        slow_leg = leg_a if a_is_slow else leg_b
        fast_leg = leg_b if a_is_slow else leg_a
        maker_leg_venue = slow_leg["venue"]
        hedge_leg_venue = fast_leg["venue"]
        recommended_topology = f"maker_first_{maker_leg_venue}"
        reasons.append("lighter_300ms_taker_cancel_speed_bump_0ms_post_only")
        if hedge_leg_venue not in _AUDITED_NO_SPEEDBUMP_VENUES:
            reasons.append(f"hedge_leg_{hedge_leg_venue}_latency_unverified")
    else:
        # Both legs have no known speed-bump (though non-audited venues carry latency uncertainty).
        if both_fees_known:
            assert maker_a is not None and taker_a is not None and maker_b is not None and taker_b is not None
            cost_a_maker = maker_a + taker_b
            cost_b_maker = maker_b + taker_a
            cost_dual_taker = taker_a + taker_b
            if cost_a_maker <= cost_b_maker and cost_a_maker < cost_dual_taker:
                maker_leg_venue = va
                hedge_leg_venue = vb
                recommended_topology = f"maker_first_{va}"
                reasons.append(f"lower_maker_taker_cost_making_on_{va}")
            elif cost_b_maker < cost_a_maker and cost_b_maker < cost_dual_taker:
                maker_leg_venue = vb
                hedge_leg_venue = va
                recommended_topology = f"maker_first_{vb}"
                reasons.append(f"lower_maker_taker_cost_making_on_{vb}")
            elif maker_a < maker_b:
                maker_leg_venue = va
                hedge_leg_venue = vb
                recommended_topology = f"maker_first_{va}"
                reasons.append(f"lower_maker_fee_on_{va}")
            elif maker_b < maker_a:
                maker_leg_venue = vb
                hedge_leg_venue = va
                recommended_topology = f"maker_first_{vb}"
                reasons.append(f"lower_maker_fee_on_{vb}")
            else:
                maker_leg_venue = None
                hedge_leg_venue = None
                recommended_topology = "concurrent_protected_ioc"
                reasons.append("fast_fast_equal_fees_concurrent_ioc_feasible")
        elif va == "arcus" and maker_a == Decimal("0"):
            maker_leg_venue = "arcus"
            hedge_leg_venue = vb
            recommended_topology = "maker_first_arcus"
            reasons.append("arcus_0bp_rwa_maker_partner_fees_unknown")
        elif vb == "arcus" and maker_b == Decimal("0"):
            maker_leg_venue = "arcus"
            hedge_leg_venue = va
            recommended_topology = "maker_first_arcus"
            reasons.append("arcus_0bp_rwa_maker_partner_fees_unknown")
        else:
            maker_leg_venue = None
            hedge_leg_venue = None
            recommended_topology = "post_only_carry_or_maker_first_unverified_fees"
            reasons.append("fee_tier_unverified_cannot_optimize_maker_leg")

        for v in (va, vb):
            if v not in _AUDITED_NO_SPEEDBUMP_VENUES:
                leg = leg_a if va == v else leg_b
                if leg["account_fee_status"] == ACCOUNT_FEE_STATUS_VERIFIED:
                    reasons.append(f"{v}_latency_unverified")
                else:
                    reasons.append(f"{v}_latency_and_account_tier_unverified")

    # Compute exact round-trip fees ONLY when BOTH legs have verified public/derived L0 or verified account fees.
    one_way_mt: Decimal | None = None
    rt_mt: Decimal | None = None
    one_way_dt: Decimal | None = None
    rt_dt: Decimal | None = None

    if both_fees_known:
        assert maker_a is not None and taker_a is not None and maker_b is not None and taker_b is not None
        one_way_dt = taker_a + taker_b
        rt_dt = one_way_dt * Decimal("2")
        if maker_leg_venue == va:
            one_way_mt = maker_a + taker_b
        elif maker_leg_venue == vb:
            one_way_mt = maker_b + taker_a
        else:
            one_way_mt = min(maker_a + taker_b, maker_b + taker_a)
        rt_mt = one_way_mt * Decimal("2")
        status_a = leg_a["account_fee_status"]
        status_b = leg_b["account_fee_status"]
        if status_a == ACCOUNT_FEE_STATUS_VERIFIED and status_b == ACCOUNT_FEE_STATUS_VERIFIED:
            fee_completeness = "verified_account_tier_both_legs"
        elif ACCOUNT_FEE_STATUS_VERIFIED in (status_a, status_b):
            fee_completeness = "mixed_verified_account_and_public_l0"
        else:
            fee_completeness = "public_l0_both_legs"
    elif (maker_a is not None and taker_a is not None) or (maker_b is not None and taker_b is not None):
        fee_completeness = "partial_unknown_requires_account_evidence"
    else:
        fee_completeness = ACCOUNT_FEE_STATUS_UNKNOWN

    return {
        "recommended_topology": recommended_topology,
        "concurrent_protected_ioc_allowed": concurrent_allowed,
        "maker_leg_venue": maker_leg_venue,
        "hedge_leg_venue": hedge_leg_venue,
        "fee_completeness": fee_completeness,
        "one_way_maker_taker_bps": round(float(one_way_mt), 6) if one_way_mt is not None else None,
        "round_trip_maker_taker_bps": round(float(rt_mt), 6) if rt_mt is not None else None,
        "one_way_dual_taker_bps": round(float(one_way_dt), 6) if one_way_dt is not None else None,
        "round_trip_dual_taker_bps": round(float(rt_dt), 6) if rt_dt is not None else None,
        "topology_reasons": reasons,
    }


def _parse_arcus_live_markets(payload: Any) -> dict[str, dict[str, Any]]:
    if not isinstance(payload, dict) or not isinstance(payload.get("markets"), list):
        raise ValueError("Arcus live markets payload must contain 'markets' list")
    out: dict[str, dict[str, Any]] = {}
    for row in payload["markets"]:
        if not isinstance(row, dict):
            continue
        sym = row.get("marketDisplayName")
        if not isinstance(sym, str) or not sym.strip():
            continue
        sym = sym.strip()
        mid = _parse_positive_decimal(row.get("mid"))
        if mid is None:
            bid = _parse_positive_decimal(row.get("bid"))
            ask = _parse_positive_decimal(row.get("ask"))
            if bid is not None and ask is not None:
                mid = (bid + ask) / Decimal("2")
        if mid is None:
            mid = _parse_positive_decimal(row.get("markPrice"))
        if mid is None:
            mid = _parse_positive_decimal(row.get("lastTradePrice"))
        if mid is None:
            mid = _parse_positive_decimal(row.get("indexPrice"))
        if mid is None:
            mid = _parse_positive_decimal(row.get("oraclePrice"))

        rate_dec = _parse_optional_decimal(row.get("fundingRate"))
        if rate_dec is None:
            rate_dec = _parse_optional_decimal(row.get("nextFundingRate"))
        funding_hourly_bps = (rate_dec * BPS_PER_UNIT) if rate_dec is not None else None

        vol_24h = _parse_positive_decimal(row.get("volume24hNotional"))
        if vol_24h is None:
            vol_base = _parse_positive_decimal(row.get("volume24h"))
            if vol_base is not None and mid is not None:
                vol_24h = vol_base * mid
        oi_usd = _parse_positive_decimal(row.get("openInterestNotional"))
        if oi_usd is None:
            oi_base = _parse_positive_decimal(row.get("openInterest"))
            if oi_base is not None and mid is not None:
                oi_usd = oi_base * mid

        out[sym] = {
            "mid": mid,
            "funding_hourly_bps": funding_hourly_bps,
            "vol_24h_usd": vol_24h,
            "oi_usd": oi_usd,
        }
    return out


def _parse_hl_live_meta_ctxs(payload: Any, label: str) -> dict[str, dict[str, Any]]:
    if not isinstance(payload, list) or len(payload) != 2:
        raise ValueError(f"{label} metaAndAssetCtxs must be [meta, assetCtxs]")
    meta, ctxs = payload
    if not isinstance(meta, dict) or not isinstance(meta.get("universe"), list) or not isinstance(ctxs, list):
        raise ValueError(f"{label} metaAndAssetCtxs missing universe or ctxs")
    if len(meta["universe"]) != len(ctxs):
        raise ValueError(f"{label} universe/ctxs length mismatch")
    out: dict[str, dict[str, Any]] = {}
    for asset, ctx in zip(meta["universe"], ctxs):
        if not isinstance(asset, dict) or not isinstance(ctx, dict):
            continue
        if asset.get("isDelisted") is True:
            continue
        name = asset.get("name")
        if not isinstance(name, str) or not name.strip():
            continue
        name = name.strip()
        mid = _parse_positive_decimal(ctx.get("midPx"))
        if mid is None:
            mid = _parse_positive_decimal(ctx.get("markPx"))
        if mid is None:
            mid = _parse_positive_decimal(ctx.get("oraclePx"))

        rate_dec = _parse_optional_decimal(ctx.get("funding"))
        funding_hourly_bps = (rate_dec * BPS_PER_UNIT) if rate_dec is not None else None

        vol_24h = _parse_positive_decimal(ctx.get("dayNtlVlm"))
        oi_usd = None
        oi_base = _parse_positive_decimal(ctx.get("openInterest"))
        if oi_base is not None and mid is not None:
            oi_usd = oi_base * mid

        out[name] = {
            "mid": mid,
            "funding_hourly_bps": funding_hourly_bps,
            "vol_24h_usd": vol_24h,
            "oi_usd": oi_usd,
        }
    return out


def _parse_lighter_live_probe(
    funding_payload: Any | None,
    details_payload: Any | None,
) -> dict[str, dict[str, Any]]:
    by_market_id: dict[int, dict[str, Any]] = {}
    by_symbol: dict[str, dict[str, Any]] = {}

    if details_payload is not None:
        if not isinstance(details_payload, dict) or not isinstance(details_payload.get("order_book_details"), list):
            raise ValueError("Lighter orderBookDetails missing order_book_details list")
        for row in details_payload["order_book_details"]:
            if not isinstance(row, dict):
                continue
            if row.get("market_type", "perp") != "perp":
                continue
            status = str(row.get("status", "")).strip().lower()
            if status == "inactive":
                continue
            sym = row.get("symbol")
            if not isinstance(sym, str) or not sym.strip() or "#" in sym:
                continue
            sym = sym.strip()
            mid = _parse_positive_decimal(row.get("last_trade_price"))
            if mid is None:
                mid = _parse_positive_decimal(row.get("mark_price"))
            if mid is None:
                mid = _parse_positive_decimal(row.get("index_price"))
            vol_24h = _parse_positive_decimal(row.get("daily_quote_token_volume"))
            oi_usd = _parse_positive_decimal(row.get("open_interest_usd"))
            if oi_usd is None:
                oi_base = _parse_positive_decimal(row.get("open_interest"))
                if oi_base is not None and mid is not None:
                    oi_usd = oi_base * mid
            entry = {
                "mid": mid,
                "funding_hourly_bps": None,
                "vol_24h_usd": vol_24h,
                "oi_usd": oi_usd,
            }
            by_symbol[sym] = entry
            mid_id = row.get("market_id")
            if isinstance(mid_id, int) and not isinstance(mid_id, bool):
                by_market_id[mid_id] = entry

    if funding_payload is not None:
        if not isinstance(funding_payload, dict) or not isinstance(funding_payload.get("funding_rates"), list):
            raise ValueError("Lighter funding-rates missing funding_rates list")
        for row in funding_payload["funding_rates"]:
            if not isinstance(row, dict):
                continue
            if row.get("exchange") != "lighter":
                continue
            rate_dec = _parse_optional_decimal(row.get("rate"))
            if rate_dec is None:
                continue
            # Lighter /api/v1/funding-rates normalizes rates as 8-hour decimal fractions.
            hourly_bps = (rate_dec / Decimal("8")) * BPS_PER_UNIT
            market_id = row.get("market_id")
            sym = row.get("symbol")
            target: dict[str, Any] | None = None
            if isinstance(market_id, int) and not isinstance(market_id, bool) and market_id in by_market_id:
                target = by_market_id[market_id]
            elif isinstance(sym, str) and sym.strip() in by_symbol:
                target = by_symbol[sym.strip()]
            elif isinstance(sym, str) and sym.strip() and "#" not in sym:
                target = {
                    "mid": None,
                    "funding_hourly_bps": hourly_bps,
                    "vol_24h_usd": None,
                    "oi_usd": None,
                }
                by_symbol[sym.strip()] = target
            if target is not None:
                target["funding_hourly_bps"] = hourly_bps

    return by_symbol


def fetch_live_funding_probe(
    venues: tuple[str, ...] | list[str],
    *,
    fetcher: Callable[..., bytes] | None = None,
) -> tuple[dict[str, dict[str, dict[str, Any]]], dict[str, dict[str, Any]]]:
    """Query strictly public endpoints for Arcus, Hyperliquid Core, TradeXYZ, and Lighter.

    Isolates HTTP/JSON/schema errors per venue without failing the overall screen.
    """
    fn = fetcher or _live_fetch_bytes
    selected = set(venues)
    market_data: dict[str, dict[str, dict[str, Any]]] = {}
    probe_status: dict[str, dict[str, Any]] = {}

    def _fetch_json(spec: dict[str, Any]) -> Any:
        raw = fn(spec["url"], method=spec["method"], body=spec["body"])
        return json.loads(raw.decode("utf-8"))

    if "arcus" in selected:
        try:
            payload = _fetch_json(LIVE_PROBE_ENDPOINTS["arcus"])
            market_data["arcus"] = _parse_arcus_live_markets(payload)
            probe_status["arcus"] = {"status": "ok", "markets_probed": len(market_data["arcus"])}
        except (OSError, ValueError, KeyError, TypeError, InvalidOperation, URLError, json.JSONDecodeError) as exc:
            probe_status["arcus"] = {"status": "error", "error": f"{type(exc).__name__}: {exc}"}

    if "hyperliquid" in selected:
        try:
            payload = _fetch_json(LIVE_PROBE_ENDPOINTS["hyperliquid"])
            market_data["hyperliquid"] = _parse_hl_live_meta_ctxs(payload, "hyperliquid")
            probe_status["hyperliquid"] = {
                "status": "ok",
                "markets_probed": len(market_data["hyperliquid"]),
            }
        except (OSError, ValueError, KeyError, TypeError, InvalidOperation, URLError, json.JSONDecodeError) as exc:
            probe_status["hyperliquid"] = {"status": "error", "error": f"{type(exc).__name__}: {exc}"}

    if "trade_xyz" in selected:
        try:
            payload = _fetch_json(LIVE_PROBE_ENDPOINTS["trade_xyz"])
            market_data["trade_xyz"] = _parse_hl_live_meta_ctxs(payload, "trade_xyz")
            probe_status["trade_xyz"] = {
                "status": "ok",
                "markets_probed": len(market_data["trade_xyz"]),
            }
        except (OSError, ValueError, KeyError, TypeError, InvalidOperation, URLError, json.JSONDecodeError) as exc:
            probe_status["trade_xyz"] = {"status": "error", "error": f"{type(exc).__name__}: {exc}"}

    if "lighter" in selected:
        funding_payload: Any | None = None
        details_payload: Any | None = None
        errors: list[str] = []
        try:
            funding_payload = _fetch_json(LIVE_PROBE_ENDPOINTS["lighter_funding"])
        except (OSError, ValueError, KeyError, TypeError, InvalidOperation, URLError, json.JSONDecodeError) as exc:
            errors.append(f"funding-rates: {type(exc).__name__}: {exc}")
        try:
            details_payload = _fetch_json(LIVE_PROBE_ENDPOINTS["lighter_details"])
        except (OSError, ValueError, KeyError, TypeError, InvalidOperation, URLError, json.JSONDecodeError) as exc:
            errors.append(f"orderBookDetails: {type(exc).__name__}: {exc}")

        if funding_payload is not None or details_payload is not None:
            try:
                market_data["lighter"] = _parse_lighter_live_probe(funding_payload, details_payload)
                probe_status["lighter"] = {
                    "status": "ok" if not errors else "partial_error",
                    "markets_probed": len(market_data["lighter"]),
                    **({"error": "; ".join(errors)} if errors else {}),
                }
            except (ValueError, KeyError, TypeError, InvalidOperation) as exc:
                errors.append(f"{type(exc).__name__}: {exc}")
                probe_status["lighter"] = {"status": "error", "error": "; ".join(errors)}
        else:
            probe_status["lighter"] = {"status": "error", "error": "; ".join(errors)}

    return market_data, probe_status


def _build_live_probe_entry(
    leg_a: dict[str, Any],
    leg_b: dict[str, Any],
    live_data: dict[str, dict[str, dict[str, Any]]],
    *,
    round_trip_maker_taker_bps: Any = None,
    round_trip_dual_taker_bps: Any = None,
) -> dict[str, Any]:
    probe_a = live_data.get(leg_a["venue"], {}).get(leg_a["symbol"]) or {}
    probe_b = live_data.get(leg_b["venue"], {}).get(leg_b["symbol"]) or {}

    mid_a: Decimal | None = probe_a.get("mid")
    mid_b: Decimal | None = probe_b.get("mid")
    f_a: Decimal | None = probe_a.get("funding_hourly_bps")
    f_b: Decimal | None = probe_b.get("funding_hourly_bps")

    mid_dislocation_bps: float | None = None
    if mid_a is not None and mid_b is not None and mid_a > 0 and mid_b > 0:
        mid_ref = (mid_a + mid_b) / Decimal("2")
        disloc = (abs(mid_a - mid_b) / mid_ref) * BPS_PER_UNIT
        mid_dislocation_bps = round(float(disloc), 4)

    f_diff: Decimal | None = None
    annualized_pct: float | None = None
    if f_a is not None and f_b is not None:
        f_diff = abs(f_a - f_b)
        annualized = (f_diff * HOURS_PER_YEAR) / Decimal("100")
        annualized_pct = round(float(annualized), 4)

    rt_mt_dec = _parse_optional_decimal(round_trip_maker_taker_bps)
    rt_dt_dec = _parse_optional_decimal(round_trip_dual_taker_bps)
    net_7d_mt: float | None = None
    net_7d_dt: float | None = None
    be_hours_mt: float | None = None
    be_hours_dt: float | None = None
    if f_diff is not None:
        carry_7d_bps = f_diff * Decimal("168")
        if rt_mt_dec is not None:
            net_7d_mt = round(float(carry_7d_bps - rt_mt_dec), 6)
            if rt_mt_dec <= Decimal("0"):
                be_hours_mt = 0.0
            elif f_diff > Decimal("0"):
                be_hours_mt = round(float(rt_mt_dec / f_diff), 4)
        if rt_dt_dec is not None:
            net_7d_dt = round(float(carry_7d_bps - rt_dt_dec), 6)
            if rt_dt_dec <= Decimal("0"):
                be_hours_dt = 0.0
            elif f_diff > Decimal("0"):
                be_hours_dt = round(float(rt_dt_dec / f_diff), 4)

    def _activity_usd(probe: dict[str, Any]) -> float | None:
        val = probe.get("vol_24h_usd")
        if val is None:
            val = probe.get("oi_usd")
        return round(float(val), 2) if val is not None else None

    act_a = _activity_usd(probe_a)
    act_b = _activity_usd(probe_b)
    activity_obj: dict[str, Any] | None
    if act_a is None and act_b is None:
        activity_obj = None
    else:
        avail = [v for v in (act_a, act_b) if v is not None]
        activity_obj = {
            "leg_a_usd": act_a,
            "leg_b_usd": act_b,
            "min_usd": min(avail) if len(avail) == 2 else None,
        }

    return {
        "mid_a": round(float(mid_a), 8) if mid_a is not None else None,
        "mid_b": round(float(mid_b), 8) if mid_b is not None else None,
        "mid_dislocation_bps": mid_dislocation_bps,
        "funding_a_hourly_bps": round(float(f_a), 6) if f_a is not None else None,
        "funding_b_hourly_bps": round(float(f_b), 6) if f_b is not None else None,
        "funding_diff_hourly_bps": round(float(f_diff), 6) if f_diff is not None else None,
        "funding_annualized_pct": annualized_pct,
        "net_7d_carry_maker_taker_bps": net_7d_mt,
        "net_7d_carry_dual_taker_bps": net_7d_dt,
        "break_even_hours_maker_taker": be_hours_mt,
        "break_even_hours_dual_taker": be_hours_dt,
        "open_interest_or_vol_24h": activity_obj,
    }


def _classify_and_rank_pair(pair: dict[str, Any]) -> tuple[list[str], float, list[str]]:
    leg_a = pair["leg_a"]
    leg_b = pair["leg_b"]
    va = leg_a["venue"]
    vb = leg_b["venue"]
    base = pair["canonical_base"]
    underlying = pair["underlying_base"]
    asset_class = pair["asset_class_hint"]
    flags = pair["risk_flags"]
    reasons: list[str] = []
    suitability: list[str] = []

    step_compat = pair["step_compatible"]
    quote_compat = pair["quote_domain_compatible"]
    rt_mt = pair["round_trip_maker_taker_bps"]
    rt_dt = pair["round_trip_dual_taker_bps"]
    eff_lev = pair["effective_max_leverage"]
    is_niche_asset = asset_class in ("equity", "rwa") or underlying not in _TOP_MEGA_CRYPTO
    has_niche_venue = bool({va, vb} & {"arcus", "trade_xyz", "lighter", "backpack"})

    if step_compat and (asset_class in ("equity", "rwa") or ({"arcus", "trade_xyz"} <= {va, vb}) or (rt_mt is not None and rt_mt <= 5.0 and is_niche_asset and has_niche_venue)):
        suitability.append("basis_niche_candidate")
        reasons.append("niche_or_low_fee_basis_candidate")

    if step_compat and (eff_lev is None or eff_lev >= 2.0):
        suitability.append("carry_universe_candidate")
        reasons.append("linear_perp_clean_step_multi_day_postonly_carry_eligible")

    score = 0.0
    if step_compat:
        score += 100.0
    else:
        score -= 200.0
        reasons.append("step_lcm_amplification_exceeds_1000")

    if quote_compat:
        score += 40.0
    else:
        score -= 25.0
        reasons.append("cross_quote_usdc_usdt_fx_risk")

    if pair["step_lcm_amplification"] == 1:
        score += 5.0

    if rt_mt is not None:
        # Reward explicit known L0/standard or verified account fees and penalize higher round-trip fees
        score += 60.0 - (rt_mt * 3.0)
        if pair.get("fee_completeness") in (
            "verified_account_tier_both_legs",
            "mixed_verified_account_and_public_l0",
        ):
            reasons.append(f"known_verified_round_trip_maker_taker_bps={rt_mt}")
        else:
            reasons.append(f"known_l0_round_trip_maker_taker_bps={rt_mt}")
        if rt_dt is not None:
            score -= rt_dt * 0.25
    else:
        reasons.append("fee_schedule_requires_account_evidence")

    if "basis_niche_candidate" in suitability:
        score += 15.0
    if asset_class in ("equity", "rwa"):
        score += 10.0

    # Mild bonus when both legs are audited or HIP-3 growth-mode / zero-fee lighter
    audited_or_public = sum(1 for v in (va, vb) if v in ("arcus", "lighter", "hyperliquid", "trade_xyz"))
    score += audited_or_public * 3.0
    if pair.get("concurrent_protected_ioc_allowed"):
        score += 2.0
        reasons.append("zero_speed_bump_dual_fast_agility")

    if "shared_hyperliquid_risk_domain" in flags:
        score -= 10.0
    if "extended_24_5_equity_hours" in flags:
        score -= 5.0
    if "ambiguous_ticker_verify_underlying" in flags:
        score -= 15.0
    if "okx_step_size_is_contract_lot" in flags:
        score -= 5.0

    live_probe = pair.get("live_probe")
    if isinstance(live_probe, dict):
        f_diff = live_probe.get("funding_diff_hourly_bps")
        disloc = live_probe.get("mid_dislocation_bps")
        if f_diff is not None:
            # 7-day gross carry (168 hours) minus known round-trip fee floor (if known)
            carry_7d = f_diff * 168.0
            fee_drag = rt_mt if rt_mt is not None else 0.0
            score += max(carry_7d - fee_drag, 0.0) * 0.5 + f_diff * 20.0
            reasons.append(f"live_funding_diff_hourly_bps={f_diff}")
        if disloc is not None:
            if disloc > 1000.0:
                flags.append("extreme_price_dislocation_verify_contract_parity")
                score -= 50.0
                reasons.append("extreme_price_dislocation_gt_1000bps")
            else:
                fee_drag = rt_mt if rt_mt is not None else 0.0
                net_disloc = max(disloc - fee_drag, 0.0)
                score += net_disloc * 0.4
                reasons.append(f"live_mid_dislocation_bps={disloc}")

    return suitability, round(score, 6), reasons


def screen_pairs(
    snapshot: dict[str, Any],
    *,
    venues: tuple[str, ...] | list[str] | None = None,
    allow_usdt_cross_quote: bool = False,
    live_funding_probe: bool = False,
    top: int | None = None,
    probe_fetcher: Callable[..., bytes] | None = None,
    account_fee_evidence: dict[str, Any] | None = None,
) -> dict[str, Any]:
    """Screen all eligible cross-venue pairs from an `opt01_venue_spec_audit_v1` snapshot."""
    venues_map = _extract_venues_map(snapshot)
    selected_venues = tuple(dict.fromkeys(venues)) if venues is not None else SUPPORTED_VENUES
    unknown = [v for v in selected_venues if v not in _VENUE_ORDER]
    if unknown:
        raise ValueError(f"unsupported venue(s): {', '.join(unknown)}")
    if top is not None and top <= 0:
        raise ValueError("--top must be a positive integer")

    evidence_venues: dict[str, dict[str, Any]] | None = None
    if account_fee_evidence is not None:
        evidence_venues = _validate_account_fee_evidence(account_fee_evidence)

    excluded_counts = {
        "inactive_or_delisted_or_disambiguated": 0,
        "missing_or_non_positive_tick_or_step": 0,
        "non_linear_or_excluded_quote_domain": 0,
    }
    skipped_venues: dict[str, str] = {}
    canonical_by_key: dict[tuple[str, int], list[dict[str, Any]]] = {}
    per_venue_active_counts: dict[str, int] = {}

    for venue in selected_venues:
        entry = venues_map.get(venue)
        if not isinstance(entry, dict):
            skipped_venues[venue] = "missing_from_snapshot"
            continue
        if entry.get("status") != "ok":
            skipped_venues[venue] = f"venue_status_{entry.get('status', 'unknown')}"
            continue
        markets = entry.get("markets")
        if not isinstance(markets, list):
            skipped_venues[venue] = "invalid_markets_list"
            continue

        active_count = 0
        for market in markets:
            canon = canonicalize_market(venue, market)
            if canon is None:
                excluded_counts["inactive_or_delisted_or_disambiguated"] += 1
                continue
            if evidence_venues is not None:
                _apply_account_fee_evidence(canon, evidence_venues.get(venue))
            tick_dec = _parse_positive_decimal(canon.get("tick_size"))
            step_dec = _parse_positive_decimal(canon.get("step_size"))
            if tick_dec is None or step_dec is None:
                excluded_counts["missing_or_non_positive_tick_or_step"] += 1
                continue

            qd = canon["quote_domain"]
            if not allow_usdt_cross_quote:
                if qd != DOMAIN_USD_USDC:
                    excluded_counts["non_linear_or_excluded_quote_domain"] += 1
                    continue
            else:
                if qd not in (DOMAIN_USD_USDC, DOMAIN_USDT):
                    excluded_counts["non_linear_or_excluded_quote_domain"] += 1
                    continue

            canon["_tick_dec"] = tick_dec
            canon["_step_dec"] = step_dec
            key = (canon["base"], int(canon["unit_multiplier"]))
            canonical_by_key.setdefault(key, []).append(canon)
            active_count += 1
        per_venue_active_counts[venue] = active_count

    live_data: dict[str, dict[str, dict[str, Any]]] = {}
    live_probe_status: dict[str, dict[str, Any]] | None = None
    if live_funding_probe:
        live_data, live_probe_status = fetch_live_funding_probe(selected_venues, fetcher=probe_fetcher)

    pairs: list[dict[str, Any]] = []
    for (base, unit_multiplier), legs in canonical_by_key.items():
        if len(legs) < 2:
            continue
        legs_sorted = sorted(
            legs,
            key=lambda m: (_VENUE_ORDER.get(m["venue"], 999), m["venue"], m["symbol"]),
        )
        for i in range(len(legs_sorted)):
            for j in range(i + 1, len(legs_sorted)):
                leg_a = legs_sorted[i]
                leg_b = legs_sorted[j]
                if leg_a["venue"] == leg_b["venue"]:
                    continue

                qd_a = leg_a["quote_domain"]
                qd_b = leg_b["quote_domain"]
                # Hard clearing guard: never merge non-compatible domains like USDG/USDC or USDH/USDC
                if {qd_a, qd_b} - {DOMAIN_USD_USDC, DOMAIN_USDT}:
                    continue
                quote_compat = qd_a == qd_b
                cross_quote_fx_risk = not quote_compat
                if cross_quote_fx_risk and not allow_usdt_cross_quote:
                    continue

                step_lcm_dec, ratio_a, ratio_b = compute_step_lcm(leg_a["_step_dec"], leg_b["_step_dec"])
                min_ratio = min(ratio_a, ratio_b)
                max_ratio = max(ratio_a, ratio_b)
                step_compatible = min_ratio <= STEP_COMPAT_AMPLIFICATION_LIMIT

                min_not_a = _parse_optional_decimal(leg_a["min_notional"])
                min_not_b = _parse_optional_decimal(leg_b["min_notional"])
                floor_a = float(min_not_a) if min_not_a is not None else 0.0
                floor_b = float(min_not_b) if min_not_b is not None else 0.0
                hl_xyz_lighter_floor = (
                    10.0
                    if any(v in ("hyperliquid", "trade_xyz", "lighter") for v in (leg_a["venue"], leg_b["venue"]))
                    else 0.0
                )
                min_notional_floor_usd = max(floor_a, floor_b, hl_xyz_lighter_floor)

                lev_a = leg_a["max_leverage"]
                lev_b = leg_b["max_leverage"]
                if lev_a is not None and lev_b is not None:
                    effective_max_leverage: float | None = round(min(float(lev_a), float(lev_b)), 6)
                else:
                    effective_max_leverage = None

                risk_flags = list(dict.fromkeys(leg_a["flags"] + leg_b["flags"]))
                if cross_quote_fx_risk:
                    risk_flags.append("cross_quote_fx_risk")
                if {leg_a["venue"], leg_b["venue"]} <= {"hyperliquid", "trade_xyz"}:
                    risk_flags.append("shared_hyperliquid_risk_domain")
                if max_ratio > STEP_COMPAT_AMPLIFICATION_LIMIT:
                    risk_flags.append("fine_to_coarse_step_ratio_gt_1000")
                if not step_compatible:
                    risk_flags.append("step_lcm_amplification_gt_1000")
                if leg_a["asset_class_hint"] != leg_b["asset_class_hint"]:
                    if "crypto" in (leg_a["asset_class_hint"], leg_b["asset_class_hint"]):
                        risk_flags.append("asset_class_mismatch_verify_underlying")

                # Promote pair asset_class_hint if either leg identifies equity or rwa
                hints = {leg_a["asset_class_hint"], leg_b["asset_class_hint"]}
                if "equity" in hints:
                    pair_asset_hint = "equity"
                elif "rwa" in hints:
                    pair_asset_hint = "rwa"
                else:
                    pair_asset_hint = "crypto"

                topo_cost = _assess_pair_topology_and_costs(leg_a, leg_b)

                def _public_leg_view(m: dict[str, Any]) -> dict[str, Any]:
                    view = {
                        "venue": m["venue"],
                        "symbol": m["symbol"],
                        "quote_asset": m["quote_asset"],
                        "quote_domain": m["quote_domain"],
                        "collateral_asset": m["collateral_asset"],
                        "asset_class_hint": m["asset_class_hint"],
                        "session_schedule": m["session_schedule"],
                        "tick_size": m["tick_size"],
                        "step_size": m["step_size"],
                        "min_notional": m["min_notional"],
                        "max_leverage": m["max_leverage"],
                        "maker_fee_bps": m["maker_fee_bps"],
                        "taker_fee_bps": m["taker_fee_bps"],
                        "fee_basis": m["fee_basis"],
                        "account_fee_status": m["account_fee_status"],
                    }
                    if "fee_provenance" in m:
                        view["fee_provenance"] = m["fee_provenance"]
                    return view

                pair_doc: dict[str, Any] = {
                    "pair_id": f"{leg_a['venue']}:{leg_a['symbol']}__vs__{leg_b['venue']}:{leg_b['symbol']}",
                    "canonical_base": base,
                    "underlying_base": leg_a["underlying_base"],
                    "unit_multiplier": unit_multiplier,
                    "asset_class_hint": pair_asset_hint,
                    "leg_a": _public_leg_view(leg_a),
                    "leg_b": _public_leg_view(leg_b),
                    "quote_domain_compatible": quote_compat,
                    "cross_quote_fx_risk": cross_quote_fx_risk,
                    "step_lcm": _format_decimal(step_lcm_dec),
                    "step_ratio_a": ratio_a,
                    "step_ratio_b": ratio_b,
                    "step_lcm_amplification": min_ratio,
                    "step_fine_to_coarse_ratio": max_ratio,
                    "step_compatible": step_compatible,
                    "min_notional_floor_usd": min_notional_floor_usd,
                    "effective_max_leverage": effective_max_leverage,
                    **topo_cost,
                    "risk_flags": risk_flags,
                }
                if live_funding_probe:
                    pair_doc["live_probe"] = _build_live_probe_entry(
                        leg_a,
                        leg_b,
                        live_data,
                        round_trip_maker_taker_bps=topo_cost["round_trip_maker_taker_bps"],
                        round_trip_dual_taker_bps=topo_cost["round_trip_dual_taker_bps"],
                    )

                suitability, score, reasons = _classify_and_rank_pair(pair_doc)
                pair_doc["suitability"] = suitability
                pair_doc["rank_score"] = score
                pair_doc["reasons"] = reasons
                pairs.append(pair_doc)

    pairs.sort(
        key=lambda p: (
            -p["rank_score"],
            p["round_trip_maker_taker_bps"] if p["round_trip_maker_taker_bps"] is not None else float("inf"),
            p["pair_id"],
        )
    )
    total_pairs = len(pairs)
    displayed_pairs = pairs[:top] if top is not None else pairs

    basis_count = sum(1 for p in pairs if "basis_niche_candidate" in p["suitability"])
    carry_count = sum(1 for p in pairs if "carry_universe_candidate" in p["suitability"])
    both_fees_known_count = sum(1 for p in pairs if p["fee_completeness"] == "public_l0_both_legs")
    both_verified_count = sum(1 for p in pairs if p["fee_completeness"] == "verified_account_tier_both_legs")
    all_known_fees_count = sum(1 for p in pairs if p["round_trip_maker_taker_bps"] is not None)

    if evidence_venues is None:
        summary_account_fee_status = ACCOUNT_FEE_STATUS_UNKNOWN
        verified_venues_list: list[str] = []
    else:
        verified_venues_list = [
            v
            for v in selected_venues
            if evidence_venues.get(v, {}).get("account_fee_status") == ACCOUNT_FEE_STATUS_VERIFIED
        ]
        active_venues = [v for v, cnt in per_venue_active_counts.items() if cnt > 0]
        if active_venues and all(
            evidence_venues.get(v, {}).get("account_fee_status") == ACCOUNT_FEE_STATUS_VERIFIED
            for v in active_venues
        ):
            summary_account_fee_status = ACCOUNT_FEE_STATUS_VERIFIED
        elif verified_venues_list:
            summary_account_fee_status = "partial_verified_account_tier"
        else:
            summary_account_fee_status = ACCOUNT_FEE_STATUS_UNKNOWN

    result: dict[str, Any] = {
        "schema_version": SCHEMA_VERSION,
        "generated_at_utc": _utc_now_iso(),
        "input_schema_version": snapshot.get("schema_version"),
        "input_audited_at_utc": snapshot.get("audited_at_utc"),
        "options": {
            "venues": list(selected_venues),
            "allow_usdt_cross_quote": allow_usdt_cross_quote,
            "live_funding_probe": live_funding_probe,
            "top": top,
            "account_fee_evidence_schema": (
                account_fee_evidence.get("schema_version") if account_fee_evidence is not None else None
            ),
        },
        "summary": {
            "requested_venues": len(selected_venues),
            "active_markets_by_venue": per_venue_active_counts,
            "skipped_venues": skipped_venues,
            "excluded_markets": excluded_counts,
            "total_matched_pairs": total_pairs,
            "returned_pairs": len(displayed_pairs),
            "basis_niche_candidates": basis_count,
            "carry_universe_candidates": carry_count,
            "both_legs_public_l0_fees_exposed": both_fees_known_count,
            "both_legs_verified_account_fees": both_verified_count,
            "both_legs_fees_known": all_known_fees_count,
            "account_fee_status": summary_account_fee_status,
            "verified_account_fee_venues": verified_venues_list,
        },
        "pairs": displayed_pairs,
    }
    if live_probe_status is not None:
        result["live_probe_status"] = live_probe_status
    return result


def build_embedded_self_test_snapshot() -> dict[str, Any]:
    """Build a deterministic offline `opt01_venue_spec_audit_v1` snapshot covering all 9 venues."""
    return {
        "schema_version": EXPECTED_INPUT_SCHEMA,
        "mode": "fixture",
        "audited_at_utc": "2026-11-22T00:00:00+00:00",
        "requested_venues": list(SUPPORTED_VENUES),
        "summary": {
            "total_venues": 9,
            "ok_venues": 9,
            "error_venues": 0,
            "all_account_fee_status_unknown": True,
        },
        "venues": {
            "arcus": {
                "venue": "arcus",
                "status": "ok",
                "account_fee_status": ACCOUNT_FEE_STATUS_UNKNOWN,
                "markets": [
                    {
                        "symbol": "ANT-USD",
                        "tick_size": "0.001",
                        "step_size": "0.000001",
                        "min_notional": "5",
                        "max_leverage": 10.0,
                        "public_fee_evidence": {
                            "source": "/v1/markets/feeSchedules",
                            "market_id": 88,
                            "has_custom_schedule": True,
                            "tiers": [
                                {
                                    "level": 0,
                                    "maker_fee_ppm": 0,
                                    "taker_fee_ppm": 112,
                                    "maker_fee_bps": "0",
                                    "taker_fee_bps": "1.12",
                                }
                            ],
                        },
                    },
                    {
                        "symbol": "BTC-USD",
                        "tick_size": "0.1",
                        "step_size": "0.00000001",
                        "min_notional": "5",
                        "max_leverage": 40.0,
                        "public_fee_evidence": {
                            "source": "/v1/markets/feeSchedules",
                            "market_id": 1,
                            "has_custom_schedule": False,
                            "tiers": [
                                {
                                    "level": 0,
                                    "maker_fee_ppm": 150,
                                    "taker_fee_ppm": 450,
                                    "maker_fee_bps": "1.5",
                                    "taker_fee_bps": "4.5",
                                }
                            ],
                        },
                    },
                    {
                        "symbol": "KPEPE-USD",
                        "tick_size": "0.000001",
                        "step_size": "0.001",
                        "min_notional": "5",
                        "max_leverage": 20.0,
                        "public_fee_evidence": {
                            "source": "/v1/markets/feeSchedules",
                            "market_id": 75,
                            "has_custom_schedule": False,
                            "tiers": [
                                {
                                    "level": 0,
                                    "maker_fee_ppm": 150,
                                    "taker_fee_ppm": 450,
                                    "maker_fee_bps": "1.5",
                                    "taker_fee_bps": "4.5",
                                }
                            ],
                        },
                    },
                ],
            },
            "hyperliquid": {
                "venue": "hyperliquid",
                "status": "ok",
                "account_fee_status": ACCOUNT_FEE_STATUS_UNKNOWN,
                "markets": [
                    {
                        "symbol": "BTC",
                        "tick_size": "0.1",
                        "step_size": "0.00001",
                        "min_notional": None,
                        "max_leverage": 40.0,
                        "public_fee_evidence": {"status": "not_exposed_in_public_endpoint"},
                    },
                    {
                        "symbol": "kPEPE",
                        "tick_size": "0.000001",
                        "step_size": "1",
                        "min_notional": None,
                        "max_leverage": 10.0,
                        "public_fee_evidence": {"status": "not_exposed_in_public_endpoint"},
                    },
                ],
            },
            "trade_xyz": {
                "venue": "trade_xyz",
                "status": "ok",
                "account_fee_status": ACCOUNT_FEE_STATUS_UNKNOWN,
                "markets": [
                    {
                        "symbol": "xyz:ANT",
                        "tick_size": "0.0001",
                        "step_size": "0.01",
                        "min_notional": None,
                        "max_leverage": 10.0,
                        "public_fee_evidence": {
                            "source": "perpDexs+metaAndAssetCtxs",
                            "dex": "xyz",
                            "collateral_token": 0,
                            "deployer_fee_scale": "1",
                            "growth_mode": "enabled",
                            "is_delisted": False,
                        },
                    },
                    {
                        "symbol": "xyz:SNOW",
                        "tick_size": "0.001",
                        "step_size": "0.001",
                        "min_notional": None,
                        "max_leverage": 10.0,
                        "public_fee_evidence": {
                            "source": "perpDexs+metaAndAssetCtxs",
                            "dex": "xyz",
                            "collateral_token": 0,
                            "deployer_fee_scale": "1",
                            "is_delisted": True,
                        },
                    },
                ],
            },
            "lighter": {
                "venue": "lighter",
                "status": "ok",
                "account_fee_status": ACCOUNT_FEE_STATUS_UNKNOWN,
                "markets": [
                    {
                        "symbol": "ANT",
                        "tick_size": "0.01",
                        "step_size": "0.0001",
                        "min_notional": "10",
                        "max_leverage": 10.0,
                        "public_fee_evidence": {
                            "source": "/api/v1/orderBookDetails",
                            "maker_fee": "0",
                            "taker_fee": "0",
                            "market_id": 150,
                            "market_status": "active",
                        },
                    },
                    {
                        "symbol": "1000PEPE",
                        "tick_size": "0.000001",
                        "step_size": "1",
                        "min_notional": "10",
                        "max_leverage": 20.0,
                        "public_fee_evidence": {
                            "source": "/api/v1/orderBookDetails",
                            "maker_fee": "0",
                            "taker_fee": "0",
                            "market_id": 18,
                            "market_status": "active",
                        },
                    },
                    {
                        "symbol": "ASELS#8286",
                        "tick_size": "0.0001",
                        "step_size": "0.01",
                        "min_notional": "10",
                        "max_leverage": 3.0,
                        "public_fee_evidence": {
                            "source": "/api/v1/orderBookDetails",
                            "maker_fee": "0",
                            "taker_fee": "0",
                            "market_id": 8286,
                            "market_status": "inactive",
                        },
                    },
                ],
            },
            "backpack": {
                "venue": "backpack",
                "status": "ok",
                "account_fee_status": ACCOUNT_FEE_STATUS_UNKNOWN,
                "markets": [
                    {
                        "symbol": "ANT.US_USDC_PERP",
                        "tick_size": "0.01",
                        "step_size": "0.01",
                        "min_notional": None,
                        "max_leverage": 10.0,
                        "public_fee_evidence": {"status": "not_exposed_in_public_endpoint"},
                    },
                    {
                        "symbol": "kPEPE_USDC_PERP",
                        "tick_size": "0.0000001",
                        "step_size": "100",
                        "min_notional": None,
                        "max_leverage": 15.0,
                        "public_fee_evidence": {"status": "not_exposed_in_public_endpoint"},
                    },
                ],
            },
            "extended": {
                "venue": "extended",
                "status": "ok",
                "account_fee_status": ACCOUNT_FEE_STATUS_UNKNOWN,
                "markets": [
                    {
                        "symbol": "BTC-USD",
                        "tick_size": "1",
                        "step_size": "0.00001",
                        "min_notional": None,
                        "max_leverage": 50.0,
                        "public_fee_evidence": {"status": "not_exposed_in_public_endpoint"},
                    },
                ],
            },
            "bulk": {
                "venue": "bulk",
                "status": "ok",
                "account_fee_status": ACCOUNT_FEE_STATUS_UNKNOWN,
                "markets": [
                    {
                        "symbol": "ANT-USD",
                        "tick_size": "0.001",
                        "step_size": "0.00000001",
                        "min_notional": "50",
                        "max_leverage": 10.0,
                        "public_fee_evidence": {"status": "not_exposed_in_public_endpoint"},
                    },
                ],
            },
            "okx": {
                "venue": "okx",
                "status": "ok",
                "account_fee_status": ACCOUNT_FEE_STATUS_UNKNOWN,
                "markets": [
                    {
                        "symbol": "BTC-USDT-SWAP",
                        "tick_size": "0.1",
                        "step_size": "0.01",
                        "min_notional": None,
                        "max_leverage": 100.0,
                        "public_fee_evidence": {"source": "/api/v5/public/instruments", "groupId": "4"},
                    },
                    {
                        "symbol": "BTC-USD-SWAP",
                        "tick_size": "0.1",
                        "step_size": "0.1",
                        "min_notional": None,
                        "max_leverage": 100.0,
                        "public_fee_evidence": {"source": "/api/v5/public/instruments", "groupId": "4"},
                    },
                ],
            },
            "binance": {
                "venue": "binance",
                "status": "ok",
                "account_fee_status": ACCOUNT_FEE_STATUS_UNKNOWN,
                "markets": [
                    {
                        "symbol": "BTCUSDC",
                        "tick_size": "0.1",
                        "step_size": "0.001",
                        "min_notional": "50",
                        "max_leverage": None,
                        "public_fee_evidence": {"status": "not_exposed_in_public_endpoint"},
                    },
                    {
                        "symbol": "ANTUSDT",
                        "tick_size": "0.01",
                        "step_size": "0.01",
                        "min_notional": "5",
                        "max_leverage": None,
                        "public_fee_evidence": {"status": "not_exposed_in_public_endpoint"},
                    },
                ],
            },
        },
    }


class _ForbiddenEnvGuard:
    """Block any os.environ read during offline screens/self-tests."""

    def __init__(self) -> None:
        self._orig: Any = None

    def __enter__(self) -> "_ForbiddenEnvGuard":
        class _DeniedEnv:
            def __getitem__(self, key: str) -> str:
                raise AssertionError(f"offline screener must not read environment variable {key!r}")

            def get(self, key: str, default: Any = None) -> Any:
                raise AssertionError(f"offline screener must not read environment variable {key!r}")

            def __contains__(self, key: object) -> bool:
                raise AssertionError(f"offline screener must not inspect environment variable {key!r}")

            def keys(self) -> Any:
                raise AssertionError("offline screener must not enumerate environment variables")

            def items(self) -> Any:
                raise AssertionError("offline screener must not enumerate environment variables")

            def __iter__(self) -> Any:
                raise AssertionError("offline screener must not iterate environment variables")

            def __len__(self) -> int:
                raise AssertionError("offline screener must not inspect environment variable count")

            def copy(self) -> Any:
                raise AssertionError("offline screener must not copy environment variables")

        self._orig = os.environ
        os.environ = _DeniedEnv()  # type: ignore[assignment]
        return self

    def __exit__(self, exc_type: Any, exc: Any, tb: Any) -> None:
        if self._orig is not None:
            os.environ = self._orig


def run_self_test(venues: tuple[str, ...] | list[str] = SUPPORTED_VENUES) -> dict[str, Any]:
    """Run deterministic offline self-test across all 9 venues with zero env/network access."""
    snap = build_embedded_self_test_snapshot()
    with _ForbiddenEnvGuard():
        lcm_1, r1_a, r1_b = compute_step_lcm(Decimal("0.003"), Decimal("0.01"))
        assert lcm_1 == Decimal("0.03") and (r1_a, r1_b) == (10, 3)
        lcm_2, r2_a, r2_b = compute_step_lcm(Decimal("0.01"), Decimal("0.0001"))
        assert lcm_2 == Decimal("0.01") and (r2_a, r2_b) == (1, 100)

        usd_only = screen_pairs(snap, venues=venues, allow_usdt_cross_quote=False, live_funding_probe=False)
        with_usdt = screen_pairs(snap, venues=venues, allow_usdt_cross_quote=True, live_funding_probe=False)

        assert usd_only["summary"]["total_matched_pairs"] > 0
        for pair in usd_only["pairs"]:
            assert pair["leg_a"]["quote_domain"] == DOMAIN_USD_USDC
            assert pair["leg_b"]["quote_domain"] == DOMAIN_USD_USDC
            assert pair["quote_domain_compatible"] is True
            assert pair["cross_quote_fx_risk"] is False
            if "lighter" in (pair["leg_a"]["venue"], pair["leg_b"]["venue"]):
                assert pair["recommended_topology"] != "concurrent_protected_ioc"
                assert pair["concurrent_protected_ioc_allowed"] is False

        if set(venues) >= {"arcus", "trade_xyz"}:
            ant_arc_xyz = [
                p
                for p in usd_only["pairs"]
                if p["canonical_base"] == "ANT"
                and {p["leg_a"]["venue"], p["leg_b"]["venue"]} == {"arcus", "trade_xyz"}
            ]
            assert len(ant_arc_xyz) == 1
            assert ant_arc_xyz[0]["round_trip_maker_taker_bps"] == 1.8
            assert ant_arc_xyz[0]["round_trip_dual_taker_bps"] == 4.04

        if set(venues) >= {"arcus", "binance"}:
            assert any(p["cross_quote_fx_risk"] for p in with_usdt["pairs"])
            for p in with_usdt["pairs"]:
                if "binance" in (p["leg_a"]["venue"], p["leg_b"]["venue"]):
                    bin_leg = p["leg_a"] if p["leg_a"]["venue"] == "binance" else p["leg_b"]
                    assert bin_leg["maker_fee_bps"] is None
                    assert bin_leg["taker_fee_bps"] is None
                    assert bin_leg["account_fee_status"] == ACCOUNT_FEE_STATUS_UNKNOWN
                    assert p["round_trip_maker_taker_bps"] is None

        if set(venues) >= {"hyperliquid", "lighter", "arcus", "backpack"}:
            pepe_pairs = [p for p in usd_only["pairs"] if p["canonical_base"] == "1000PEPE"]
            assert len(pepe_pairs) >= 3

        # Verify optional aleph_account_fee_evidence_v1 ingestion + fail-closed unverified preservation
        sample_evidence = {
            "schema_version": EXPECTED_ACCOUNT_FEE_EVIDENCE_SCHEMA,
            "venues": {
                "arcus": {"account_fee_status": ACCOUNT_FEE_STATUS_PUBLIC_TIER0, "maker_fee_bps": None, "taker_fee_bps": None},
                "hyperliquid": {"account_fee_status": ACCOUNT_FEE_STATUS_VERIFIED, "maker_fee_bps": "1.44", "taker_fee_bps": "4.32"},
                "trade_xyz": {
                    "account_fee_status": ACCOUNT_FEE_STATUS_VERIFIED,
                    "base_maker_fee_bps": "1.44",
                    "base_taker_fee_bps": "4.32",
                    "deployer_fee_scale": "1",
                    "standard_maker_fee_bps": "2.88",
                    "standard_taker_fee_bps": "8.64",
                    "growth_mode_maker_fee_bps": "0.288",
                    "growth_mode_taker_fee_bps": "0.864",
                },
                "lighter": {"account_fee_status": ACCOUNT_FEE_STATUS_PUBLIC_UNVERIFIED, "maker_fee_bps": None, "taker_fee_bps": None},
                "binance": {"account_fee_status": ACCOUNT_FEE_STATUS_UNKNOWN, "maker_fee_bps": None, "taker_fee_bps": None},
            },
        }
        with_ev = screen_pairs(
            snap,
            venues=venues,
            allow_usdt_cross_quote=False,
            live_funding_probe=False,
            account_fee_evidence=sample_evidence,
        )
        if set(venues) >= {"arcus", "hyperliquid"}:
            btc_arc_hl = next(
                p
                for p in with_ev["pairs"]
                if p["canonical_base"] == "BTC" and {p["leg_a"]["venue"], p["leg_b"]["venue"]} == {"arcus", "hyperliquid"}
            )
            # Arcus BTC (1.5 / 4.5) + HL verified (1.44 / 4.32): min MT = 1.5 + 4.32 = 5.82 -> RT = 11.64
            assert btc_arc_hl["recommended_topology"] == "maker_first_arcus"
            assert btc_arc_hl["round_trip_maker_taker_bps"] == 11.64
            assert btc_arc_hl["round_trip_dual_taker_bps"] == 17.64
        if set(venues) >= {"arcus", "lighter"}:
            ant_arc_lgt = next(
                p
                for p in with_ev["pairs"]
                if p["canonical_base"] == "ANT" and {p["leg_a"]["venue"], p["leg_b"]["venue"]} == {"arcus", "lighter"}
            )
            assert ant_arc_lgt["round_trip_maker_taker_bps"] is None

    return {
        "self_test": "passed",
        "venues_tested": list(venues),
        "usd_only_pairs": usd_only["summary"]["total_matched_pairs"],
        "with_usdt_pairs": with_usdt["summary"]["total_matched_pairs"],
        "top_pair_id": usd_only["pairs"][0]["pair_id"] if usd_only["pairs"] else None,
        "step_lcm_verified": True,
        "lighter_speed_bump_guard_verified": True,
        "unknown_fee_preservation_verified": True,
        "account_fee_evidence_verified": True,
        "zero_secret_env_reads_verified": True,
    }


def _write_json_output(path: Path, payload: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(payload, indent=2, allow_nan=False) + "\n", encoding="utf-8")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group(required=False)
    mode.add_argument(
        "--snapshot",
        type=Path,
        default=None,
        help="Path to opt01_venue_spec_audit_v1 JSON snapshot",
    )
    mode.add_argument(
        "--self-test",
        action="store_true",
        help="Run offline deterministic self-test across all 9 venues and exit",
    )
    parser.add_argument(
        "--top",
        type=int,
        default=DEFAULT_TOP_N,
        help=f"Maximum number of ranked pairs to emit (default: {DEFAULT_TOP_N})",
    )
    parser.add_argument(
        "--venues",
        type=str,
        default=",".join(SUPPORTED_VENUES),
        help=f"Comma-separated venues to include (default: {','.join(SUPPORTED_VENUES)})",
    )
    parser.add_argument(
        "--allow-usdt-cross-quote",
        action="store_true",
        help="Allow pairing USDT-settled linear perps with USD/USDC perps (marks cross_quote_fx_risk=true)",
    )
    parser.add_argument(
        "--live-funding-probe",
        action="store_true",
        help="Opt-in: query public Arcus/Hyperliquid/TradeXYZ/Lighter endpoints for live mark/funding spreads",
    )
    parser.add_argument(
        "--account-fee-evidence",
        type=Path,
        default=None,
        help="Optional path to aleph_account_fee_evidence_v1 JSON output from audit_account_fees.py",
    )
    parser.add_argument(
        "--output",
        type=Path,
        default=None,
        help="Optional file path to write the JSON screening report",
    )
    args = parser.parse_args(argv)

    if args.snapshot is None and not args.self_test:
        print(
            "Error: refuse to run without --snapshot <path.json> or --self-test",
            file=sys.stderr,
        )
        return 2

    if args.self_test and args.live_funding_probe:
        print(
            "Error: --self-test is strictly offline and cannot be combined with --live-funding-probe",
            file=sys.stderr,
        )
        return 2

    if args.self_test and args.account_fee_evidence is not None:
        print(
            "Error: --self-test uses embedded fixtures and cannot be combined with --account-fee-evidence",
            file=sys.stderr,
        )
        return 2

    if args.top <= 0:
        print("Error: --top must be a positive integer", file=sys.stderr)
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

    assert args.snapshot is not None
    try:
        snapshot = json.loads(args.snapshot.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as exc:
        print(f"Error reading --snapshot {args.snapshot}: {exc}", file=sys.stderr)
        return 2

    account_fee_evidence: dict[str, Any] | None = None
    if args.account_fee_evidence is not None:
        try:
            account_fee_evidence = json.loads(args.account_fee_evidence.read_text(encoding="utf-8"))
        except (OSError, UnicodeDecodeError, json.JSONDecodeError) as exc:
            print(f"Error reading --account-fee-evidence {args.account_fee_evidence}: {exc}", file=sys.stderr)
            return 2

    try:
        report = screen_pairs(
            snapshot,
            venues=venues,
            allow_usdt_cross_quote=args.allow_usdt_cross_quote,
            live_funding_probe=args.live_funding_probe,
            top=args.top,
            account_fee_evidence=account_fee_evidence,
        )
    except (ValueError, KeyError, TypeError) as exc:
        print(f"Error screening snapshot: {exc}", file=sys.stderr)
        return 2

    if args.output is not None:
        try:
            _write_json_output(args.output, report)
        except OSError as exc:
            print(f"Error writing --output {args.output}: {exc}", file=sys.stderr)
            return 2

    try:
        print(json.dumps(report, indent=2, allow_nan=False))
    except BrokenPipeError:
        pass
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
