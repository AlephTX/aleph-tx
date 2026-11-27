#!/usr/bin/env python3
"""Strictly read-only OPT-01 multi-venue account fee-tier verifier.

100% pure Python stdlib at top level. Offline by default: refuses to execute
without `--live`, `--fixture`, or `--self-test`. Loads `--env-file` strictly in
memory, passes Lighter/Backpack credentials to `go run ./cmd/fee-audit` solely via
child process environment variables, and enforces `audit_account_fees.validate_redacted_output`
against EVERY non-empty `--env-file` value before writing or printing JSON.
"""

from __future__ import annotations

import argparse
import base64
from datetime import datetime, timezone
from decimal import Decimal, InvalidOperation
import hashlib
import hmac
import json
import os
from pathlib import Path
import re
import ssl
import subprocess
import sys
import time
from typing import Any, Callable
from urllib.error import HTTPError, URLError
from urllib.parse import urlencode
from urllib.request import HTTPRedirectHandler, HTTPSHandler, Request, build_opener

REPO_ROOT = Path(__file__).resolve().parent.parent
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

try:
    from scripts import audit_account_fees as aaf
except ImportError:
    import audit_account_fees as aaf  # type: ignore[no-redef]

SCHEMA_VERSION = aaf.SCHEMA_VERSION
BPS_PER_UNIT = Decimal("10000")
MAX_RESPONSE_BYTES = 16 * 1024 * 1024
DEFAULT_TIMEOUT_SECONDS = 15.0

SUPPORTED_VENUES: tuple[str, ...] = (
    "lighter",
    "lighter_robinhood",
    "hyperliquid",
    "trade_xyz",
    "arcus",
    "extended",
    "backpack",
    "okx",
    "binance",
    "bulk",
)
_VENUE_SET: frozenset[str] = frozenset(SUPPORTED_VENUES)

ARCUS_ACCOUNT_URL = "https://api.arcus.xyz/v1/account"
ARCUS_ACCOUNT_STATS_URL = "https://api.arcus.xyz/v1/account/stats"
ARCUS_FEE_SCHEDULES_URL = "https://api.arcus.xyz/v1/markets/feeSchedules"
EXTENDED_BASE_URL = "https://api.starknet.extended.exchange"
OKX_TRADE_FEE_URL = "https://www.okx.com/api/v5/account/trade-fee?instType=SWAP"
BINANCE_COMMISSION_URL = "https://fapi.binance.com/fapi/v1/commissionRate"
DEFAULT_BULK_API_URL = "https://mainnet-api1.bulk.trade/api/v1"

HttpRequester = Callable[[str, dict[str, Any] | None, dict[str, str] | None], Any]
GoAuditRunner = Callable[[list[str], dict[str, str]], dict[str, Any]]


class _NoRedirectHandler(HTTPRedirectHandler):
    """Fail closed rather than silently following HTTP redirects."""

    def redirect_request(self, req: Request, fp: Any, code: int, msg: str, headers: Any, newurl: str) -> None:
        raise HTTPError(req.full_url, code, f"unexpected redirect ({msg})", headers, fp)


def _utc_now_iso() -> str:
    return datetime.now(timezone.utc).isoformat()


def _build_ssl_context() -> ssl.SSLContext:
    cafile = os.environ.get("SSL_CERT_FILE") or None
    if cafile:
        return ssl.create_default_context(cafile=cafile)
    default_paths = ssl.get_default_verify_paths()
    if default_paths.cafile and Path(default_paths.cafile).is_file():
        return ssl.create_default_context()
    for candidate in ("/etc/ssl/cert.pem", "/opt/homebrew/etc/ca-certificates/cert.pem"):
        if Path(candidate).is_file():
            return ssl.create_default_context(cafile=candidate)
    return ssl.create_default_context()


def _live_http_json(
    url: str,
    body: dict[str, Any] | None = None,
    headers: dict[str, str] | None = None,
    *,
    timeout: float = DEFAULT_TIMEOUT_SECONDS,
) -> Any:
    if not url.startswith("https://"):
        raise ValueError("only HTTPS endpoints are allowed")
    req_headers: dict[str, str] = {
        "Accept": "application/json",
        "User-Agent": "AlephTX-ReadOnlyFeeVerify/1.0",
    }
    if headers:
        req_headers.update(headers)
    data: bytes | None = None
    method = "GET"
    if body is not None:
        data = json.dumps(body, separators=(",", ":")).encode("utf-8")
        req_headers["Content-Type"] = "application/json"
        method = "POST"
    req = Request(url, data=data, headers=req_headers, method=method)
    opener = build_opener(_NoRedirectHandler(), HTTPSHandler(context=_build_ssl_context()))
    with opener.open(req, timeout=timeout) as resp:
        if resp.status != 200:
            raise ValueError(f"unexpected HTTP status {resp.status}")
        raw = resp.read(MAX_RESPONSE_BYTES + 1)
    if len(raw) > MAX_RESPONSE_BYTES:
        raise ValueError(f"response exceeded {MAX_RESPONSE_BYTES} bytes")
    return json.loads(raw.decode("utf-8"))


def parse_env_file_in_memory(env_path: Path) -> dict[str, str]:
    """Parse a dotenv file strictly in memory without mutating `os.environ`."""
    text = env_path.read_text(encoding="utf-8")
    env_map: dict[str, str] = {}
    for raw_line in text.splitlines():
        line = raw_line.strip()
        if not line or line.startswith("#"):
            continue
        if line.startswith("export "):
            line = line[len("export "):].strip()
        if "=" not in line:
            continue
        key, val = line.split("=", 1)
        key = key.strip()
        val = val.strip()
        if not key:
            continue
        if len(val) >= 2 and ((val[0] == '"' and val[-1] == '"') or (val[0] == "'" and val[-1] == "'")):
            val = val[1:-1]
        env_map[key] = val
    return env_map


def collect_all_env_literals(env_map: dict[str, str]) -> list[str]:
    """Collect EVERY non-empty value from `--env-file` as forbidden redaction literals."""
    literals: list[str] = []
    seen: set[str] = set()
    for val in env_map.values():
        stripped = str(val).strip()
        if stripped and stripped not in seen:
            seen.add(stripped)
            literals.append(stripped)
    return literals


def _format_decimal(val: Decimal) -> str:
    return aaf._format_decimal(val)


def _parse_decimal(raw: Any, field: str) -> Decimal:
    return aaf._parse_decimal(raw, field)


def _parse_optional_decimal(raw: Any, field: str) -> Decimal | None:
    return aaf._parse_optional_decimal(raw, field)


def _run_go_fee_audit(
    venues: list[str],
    env_map: dict[str, str],
    *,
    timeout_sec: float = DEFAULT_TIMEOUT_SECONDS,
) -> dict[str, Any]:
    """Invoke `go run ./cmd/fee-audit` in `feeder/` passing credentials ONLY via child `env`."""
    allowed_prefixes = ("LIGHTER_", "BACKPACK_")
    child_env: dict[str, str] = {}
    for sys_key in (
        "PATH",
        "HOME",
        "TMPDIR",
        "GOPATH",
        "GOCACHE",
        "GOMODCACHE",
        "GOROOT",
        "GOTOOLCHAIN",
        "SSL_CERT_FILE",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "NO_PROXY",
    ):
        val = os.environ.get(sys_key)
        if val is not None:
            child_env[sys_key] = val

    for k, v in env_map.items():
        if k.startswith(allowed_prefixes) or k == "API_KEY_PRIVATE_KEY":
            child_env[k] = v

    cmd = ["go", "run", "./cmd/fee-audit", "--venues", ",".join(venues), "--timeout", f"{int(max(1, timeout_sec))}s"]
    proc = subprocess.run(
        cmd,
        cwd=str(REPO_ROOT / "feeder"),
        env=child_env,
        capture_output=True,
        text=True,
        timeout=max(30.0, timeout_sec * (len(venues) + 2) + 30.0),
        check=False,
    )
    if proc.returncode != 0:
        err_msg = aaf._sanitize_error_text(proc.stderr.strip() or f"exit {proc.returncode}", collect_all_env_literals(env_map))
        raise ValueError(f"go fee-audit failed: {err_msg}")
    stdout_text = proc.stdout
    brace_idx = stdout_text.find("{")
    if brace_idx < 0:
        raise ValueError("go fee-audit did not emit JSON")
    prefix_lines = [ln.strip() for ln in stdout_text[:brace_idx].splitlines() if ln.strip()]
    if any(not ln.startswith("shm:") for ln in prefix_lines):
        raise ValueError("go fee-audit emitted unexpected non-JSON stdout prefix")
    parsed, end_idx = json.JSONDecoder().raw_decode(stdout_text[brace_idx:])
    if stdout_text[brace_idx + end_idx :].strip():
        raise ValueError("go fee-audit emitted trailing non-JSON stdout")
    if not isinstance(parsed, dict) or not isinstance(parsed.get("venues"), dict):
        raise ValueError("go fee-audit returned malformed JSON")
    return parsed


def _probe_arcus_full(
    env_map: dict[str, str],
    req: HttpRequester | None,
    *,
    fixture_payload: Any = None,
    discovered_secrets: list[str] | None = None,
) -> dict[str, Any]:
    stats_raw: Any = None
    if fixture_payload is not None:
        sched_raw = fixture_payload.get("feeSchedules") if isinstance(fixture_payload, dict) else fixture_payload
        acct_raw = fixture_payload.get("account") if isinstance(fixture_payload, dict) else None
        stats_raw = fixture_payload.get("stats") if isinstance(fixture_payload, dict) else None
        acct_configured = acct_raw is not None or stats_raw is not None
    else:
        assert req is not None
        sched_raw = req(ARCUS_FEE_SCHEDULES_URL, None, None)
        addr = env_map.get("ARCUS_ACCOUNT_ADDRESS", "").strip()
        idx = env_map.get("ARCUS_ACCOUNT_INDEX", "").strip()
        acct_configured = bool(addr)
        acct_raw = None
        if addr:
            if not aaf.EVM_ADDR_RE.fullmatch(addr):
                raise ValueError("invalid Arcus EVM address")
            query_idx = idx if idx else "0"
            if not re.fullmatch(r"[0-9]", query_idx):
                raise ValueError("invalid Arcus account index")
            query = urlencode({"address": addr.lower(), "accountIndex": query_idx})
            acct_raw = req(f"{ARCUS_ACCOUNT_URL}?{query}", None, None)
            stats_query = urlencode({"address": addr.lower(), "include": "feeTier"})
            try:
                stats_raw = req(f"{ARCUS_ACCOUNT_STATS_URL}?{stats_query}", None, None)
            except HTTPError as exc:
                if exc.code not in (400, 404):
                    raise

    base = aaf._probe_arcus(
        account_identifier_configured=acct_configured,
        req=None,
        fixture_payload={"feeSchedules": sched_raw} if sched_raw is not None else None,
    )
    base["settlement"] = "USDC"
    base["source_endpoints"] = ["GET https://api.arcus.xyz/v1/markets/feeSchedules"]

    if acct_raw is None and stats_raw is None:
        return base

    endpoints = list(base["source_endpoints"])
    if acct_raw is not None:
        aaf._reject_error_envelope(acct_raw, "arcus.account")
        if not isinstance(acct_raw, dict):
            raise ValueError("Arcus /v1/account response must be a JSON object")
        if discovered_secrets is not None:
            for k in ("address", "accountIndex", "netQuoteBalance", "equity", "freeCollateral", "sequenceNumber"):
                v = acct_raw.get(k)
                if v is not None and not isinstance(v, bool):
                    s = str(v).strip()
                    if s:
                        discovered_secrets.append(s)
        endpoints.insert(0, "GET https://api.arcus.xyz/v1/account")
        base["account_probe_ok"] = True

    if stats_raw is not None:
        aaf._reject_error_envelope(stats_raw, "arcus.account_stats")
        if not isinstance(stats_raw, dict):
            raise ValueError("Arcus /v1/account/stats response must be a JSON object")
        if discovered_secrets is not None and isinstance(stats_raw.get("address"), str):
            s = stats_raw["address"].strip()
            if s:
                discovered_secrets.append(s)
        endpoints.insert(0, "GET https://api.arcus.xyz/v1/account/stats?include=feeTier")
        base["account_probe_ok"] = True

    base["source_endpoints"] = endpoints

    # Check `/v1/account/stats` first, then `/v1/account` for `tradingFeeTier` / explicit fee fields.
    tier_obj: dict[str, Any] | None = None
    if isinstance(stats_raw, dict) and isinstance(stats_raw.get("tradingFeeTier"), dict):
        tier_obj = stats_raw["tradingFeeTier"]
    elif isinstance(acct_raw, dict) and isinstance(acct_raw.get("tradingFeeTier"), dict):
        tier_obj = acct_raw["tradingFeeTier"]

    acct_dict = acct_raw if isinstance(acct_raw, dict) else {}
    raw_level = (
        tier_obj.get("level")
        if tier_obj is not None and tier_obj.get("level") is not None
        else acct_dict.get("feeTier", acct_dict.get("feeTierLevel"))
    )
    raw_discount_ppm = (
        tier_obj.get("feeDiscountPpm")
        if tier_obj is not None and tier_obj.get("feeDiscountPpm") is not None
        else acct_dict.get("feeDiscountPpm", 0)
    )
    discount_ppm = int(raw_discount_ppm) if raw_discount_ppm is not None else 0
    if discount_ppm < 0 or discount_ppm >= 1_000_000:
        raise ValueError(f"invalid Arcus feeDiscountPpm: {discount_ppm}")
    discount_factor = Decimal("1") - (Decimal(discount_ppm) / Decimal("1000000"))

    maker_ppm = _parse_optional_decimal(
        tier_obj.get("makerFeePpm") if tier_obj else acct_dict.get("makerFeePpm"),
        "arcus.account.makerFeePpm",
    )
    taker_ppm = _parse_optional_decimal(
        tier_obj.get("takerFeePpm") if tier_obj else acct_dict.get("takerFeePpm"),
        "arcus.account.takerFeePpm",
    )
    maker_bps = _parse_optional_decimal(acct_dict.get("makerFeeBps"), "arcus.account.makerFeeBps")
    taker_bps = _parse_optional_decimal(acct_dict.get("takerFeeBps"), "arcus.account.takerFeeBps")

    if maker_ppm is not None and taker_ppm is not None:
        maker_bps = maker_ppm / Decimal(100)
        taker_bps = taker_ppm / Decimal(100)

    def _apply_discount_bps(bps_str: str) -> str:
        val = Decimal(bps_str)
        if val > 0:
            val = val * discount_factor
        return _format_decimal(val)

    if raw_level is not None and type(raw_level) is int and 0 <= raw_level <= 32:
        # Resolve exact crypto/rwa/custom-market fees at the verified tier level + feeDiscountPpm.
        sched_rows = (sched_raw or {}).get("feeSchedules") if isinstance(sched_raw, dict) else []
        crypto_pairs: set[tuple[str, str]] = set()
        rwa_pairs: set[tuple[str, str]] = set()
        custom_verified: dict[str, dict[str, str]] = {}
        for row in sched_rows or []:
            if not isinstance(row, dict):
                continue
            sym = str(row.get("marketDisplayName") or "").strip()
            tiers = aaf._parse_arcus_tiers(row.get("tiers"), f"feeSchedules[{sym}].tiers")
            match = [t for t in tiers if t["level"] == raw_level]
            if not match:
                continue
            t_entry = match[0]
            pair = (_apply_discount_bps(t_entry["maker_fee_bps"]), _apply_discount_bps(t_entry["taker_fee_bps"]))
            if row.get("hasCustomSchedule"):
                custom_verified[sym] = {"maker_fee_bps": pair[0], "taker_fee_bps": pair[1]}
                if sym != "ANT-USD":
                    rwa_pairs.add(pair)
            else:
                crypto_pairs.add(pair)

        base["account_fee_status"] = aaf.ACCOUNT_FEE_STATUS_VERIFIED
        base["verified_level"] = raw_level
        base["fee_discount_ppm"] = discount_ppm
        base["fee_fields_exposed"] = True
        if len(crypto_pairs) == 1:
            cmk, ctk = next(iter(crypto_pairs))
            base["crypto_maker_fee_bps"] = cmk
            base["crypto_taker_fee_bps"] = ctk
        if len(rwa_pairs) == 1:
            rmk, rtk = next(iter(rwa_pairs))
            base["rwa_maker_fee_bps"] = rmk
            base["rwa_taker_fee_bps"] = rtk
        if custom_verified:
            base["verified_custom_schedule_bps"] = dict(sorted(custom_verified.items()))
        base["note"] = (
            f"Verified Arcus tradingFeeTier level={raw_level}, feeDiscountPpm={discount_ppm} "
            "via read-only GET /v1/account/stats?include=feeTier + GET /v1/markets/feeSchedules."
        )
        return base

    if maker_bps is not None and taker_bps is not None:
        base["account_fee_status"] = aaf.ACCOUNT_FEE_STATUS_VERIFIED
        base["fee_fields_exposed"] = True
        base["maker_fee_bps"] = _format_decimal(maker_bps * discount_factor if maker_bps > 0 else maker_bps)
        base["taker_fee_bps"] = _format_decimal(taker_bps * discount_factor if taker_bps > 0 else taker_bps)
        base["note"] = "Verified Arcus per-account fees from read-only GET /v1/account."
        return base

    base["fee_fields_exposed"] = False
    base["note"] = (
        "Read-only GET /v1/account succeeded, but endpoint omits per-account fee tier fields; "
        "retaining public_tier0_schedule_only from GET /v1/markets/feeSchedules."
    )
    return base


def _probe_extended(
    env_map: dict[str, str],
    req: HttpRequester | None,
    *,
    fixture_payload: Any = None,
) -> dict[str, Any]:
    if fixture_payload is not None:
        raw = fixture_payload
        used_endpoint = "GET https://api.starknet.extended.exchange/api/v1/user/fees"
    else:
        api_key = env_map.get("EXTENDED_API_KEY", "").strip()
        if not api_key:
            return {
                "venue": "extended",
                "settlement": "USDC",
                "configured": False,
                "status": "unconfigured",
                "account_fee_status": aaf.ACCOUNT_FEE_STATUS_UNKNOWN,
                "maker_fee_bps": None,
                "taker_fee_bps": None,
            }
        base_url = env_map.get("EXTENDED_API_URL", "").strip().rstrip("/") or EXTENDED_BASE_URL
        if not base_url.startswith("https://"):
            raise ValueError("EXTENDED_API_URL must use HTTPS")
        headers = {"X-Api-Key": api_key}
        assert req is not None
        used_endpoint = "GET https://api.starknet.extended.exchange/api/v1/user/fees"
        try:
            raw = req(f"{base_url}/api/v1/user/fees", None, headers)
        except HTTPError as exc:
            if exc.code in (400, 404, 422):
                used_endpoint = "GET https://api.starknet.extended.exchange/api/v1/user/fees?market=BTC-USD"
                raw = req(f"{base_url}/api/v1/user/fees?market=BTC-USD", None, headers)
            else:
                raise

    aaf._reject_error_envelope(raw, "extended")
    if not isinstance(raw, dict):
        raise ValueError("Extended /api/v1/user/fees must return a JSON object")
    status_str = str(raw.get("status") or "").upper()
    if status_str and status_str != "OK":
        raise ValueError(f"Extended /api/v1/user/fees returned status={status_str}")

    data = raw.get("data")
    rows: list[dict[str, Any]]
    if isinstance(data, list):
        rows = [r for r in data if isinstance(r, dict)]
    elif isinstance(data, dict):
        rows = [data]
    else:
        raise ValueError("Extended /api/v1/user/fees missing data")
    if not rows:
        raise ValueError("Extended /api/v1/user/fees returned empty fee list")

    pairs: set[tuple[str, str]] = set()
    per_market: dict[str, dict[str, str]] = {}
    for row in rows:
        mk_raw = row.get("makerFeeRate", row.get("makerFee"))
        tk_raw = row.get("takerFeeRate", row.get("takerFee"))
        mk = _parse_decimal(mk_raw, "extended.makerFeeRate") * BPS_PER_UNIT
        tk = _parse_decimal(tk_raw, "extended.takerFeeRate") * BPS_PER_UNIT
        mk_str = _format_decimal(mk)
        tk_str = _format_decimal(tk)
        pairs.add((mk_str, tk_str))
        mkt = row.get("market")
        if isinstance(mkt, str) and mkt.strip():
            entry: dict[str, str] = {"maker_fee_bps": mk_str, "taker_fee_bps": tk_str}
            bld_raw = row.get("builderFeeRate")
            bld = _parse_optional_decimal(bld_raw, "extended.builderFeeRate")
            if bld is not None:
                entry["builder_fee_bps"] = _format_decimal(bld * BPS_PER_UNIT)
            per_market[mkt.strip()] = entry

    if len(pairs) == 1:
        maker_bps, taker_bps = next(iter(pairs))
        out: dict[str, Any] = {
            "venue": "extended",
            "settlement": "USDC",
            "configured": True,
            "status": "ok",
            "account_fee_status": aaf.ACCOUNT_FEE_STATUS_VERIFIED,
            "source_endpoints": [used_endpoint],
            "maker_fee_bps": maker_bps,
            "taker_fee_bps": taker_bps,
            "authenticated_probe_ok": True,
            "note": "Verified via read-only X-Api-Key GET /api/v1/user/fees.",
        }
        if per_market:
            out["market_count"] = len(per_market)
        return out

    # Multiple markets with distinct rates -> summarize crypto vs equity/24_5 domain rates + per-market map.
    crypto_pairs = {
        (per_market[m]["maker_fee_bps"], per_market[m]["taker_fee_bps"])
        for m in ("BTC-USD", "ETH-USD", "SOL-USD")
        if m in per_market
    }
    rwa_pairs = {
        (v["maker_fee_bps"], v["taker_fee_bps"])
        for m, v in per_market.items()
        if "_24_5-" in m
    }
    out_multi: dict[str, Any] = {
        "venue": "extended",
        "settlement": "USDC",
        "configured": True,
        "status": "ok",
        "account_fee_status": aaf.ACCOUNT_FEE_STATUS_VERIFIED,
        "source_endpoints": [used_endpoint],
        "maker_fee_bps": None,
        "taker_fee_bps": None,
        "authenticated_probe_ok": True,
        "market_count": len(per_market),
        "per_market_fees_bps": dict(sorted(per_market.items())),
        "note": "Verified via read-only X-Api-Key GET /api/v1/user/fees (varies by market).",
    }
    if len(crypto_pairs) == 1:
        cmk, ctk = next(iter(crypto_pairs))
        out_multi["crypto_maker_fee_bps"] = cmk
        out_multi["crypto_taker_fee_bps"] = ctk
    if len(rwa_pairs) == 1:
        rmk, rtk = next(iter(rwa_pairs))
        out_multi["rwa_maker_fee_bps"] = rmk
        out_multi["rwa_taker_fee_bps"] = rtk
    return out_multi


def _probe_okx(
    env_map: dict[str, str],
    req: HttpRequester | None,
    *,
    fixture_payload: Any = None,
    now_utc: datetime | None = None,
) -> dict[str, Any]:
    if fixture_payload is not None:
        raw = fixture_payload
    else:
        api_key = env_map.get("OKX_API_KEY", "").strip()
        api_secret = env_map.get("OKX_API_SECRET", "").strip()
        passphrase = env_map.get("OKX_PASSPHRASE", "").strip()
        if not api_key and not api_secret and not passphrase:
            return {
                "venue": "okx",
                "settlement": "USDT",
                "configured": False,
                "status": "unconfigured",
                "account_fee_status": aaf.ACCOUNT_FEE_STATUS_UNKNOWN,
                "maker_fee_bps": None,
                "taker_fee_bps": None,
            }
        if not (api_key and api_secret and passphrase):
            raise ValueError("incomplete OKX credentials")
        ts = (now_utc or datetime.now(timezone.utc)).strftime("%Y-%m-%dT%H:%M:%S.000Z")
        request_path = "/api/v5/account/trade-fee?instType=SWAP"
        prehash = f"{ts}GET{request_path}"
        mac = hmac.new(api_secret.encode("utf-8"), prehash.encode("utf-8"), hashlib.sha256).digest()
        sig = base64.b64encode(mac).decode("ascii")
        headers = {
            "OK-ACCESS-KEY": api_key,
            "OK-ACCESS-SIGN": sig,
            "OK-ACCESS-TIMESTAMP": ts,
            "OK-ACCESS-PASSPHRASE": passphrase,
        }
        assert req is not None
        raw = req(OKX_TRADE_FEE_URL, None, headers)

    if not isinstance(raw, dict) or str(raw.get("code")) != "0":
        code_val = raw.get("code") if isinstance(raw, dict) else "invalid"
        raise ValueError(f"OKX trade-fee returned code={code_val}")
    rows = raw.get("data")
    if not isinstance(rows, list) or not rows or not isinstance(rows[0], dict):
        raise ValueError("OKX trade-fee missing data row")
    row = rows[0]

    # OKX sign convention: negative = fee paid by user, positive = rebate.
    mk_raw = row.get("makerU") if row.get("makerU") not in (None, "") else row.get("maker")
    tk_raw = row.get("takerU") if row.get("takerU") not in (None, "") else row.get("taker")
    maker_bps = -_parse_decimal(mk_raw, "okx.maker") * BPS_PER_UNIT
    taker_bps = -_parse_decimal(tk_raw, "okx.taker") * BPS_PER_UNIT

    out: dict[str, Any] = {
        "venue": "okx",
        "settlement": "USDT",
        "configured": True,
        "status": "ok",
        "account_fee_status": aaf.ACCOUNT_FEE_STATUS_VERIFIED,
        "source_endpoints": [f"GET {OKX_TRADE_FEE_URL}"],
        "maker_fee_bps": _format_decimal(maker_bps),
        "taker_fee_bps": _format_decimal(taker_bps),
        "authenticated_probe_ok": True,
        "note": "Verified via read-only HMAC-SHA256 GET /api/v5/account/trade-fee?instType=SWAP (OKX negative sign negated to fee bps).",
    }
    if isinstance(row.get("level"), str) and row["level"].strip():
        out["fee_tier"] = row["level"].strip()
    if isinstance(row.get("category"), str) and row["category"].strip():
        out["fee_category"] = row["category"].strip()
    coin_mk = _parse_optional_decimal(row.get("maker"), "okx.coin_maker")
    coin_tk = _parse_optional_decimal(row.get("taker"), "okx.coin_taker")
    if coin_mk is not None and coin_tk is not None:
        out["coin_margined_maker_fee_bps"] = _format_decimal(-coin_mk * BPS_PER_UNIT)
        out["coin_margined_taker_fee_bps"] = _format_decimal(-coin_tk * BPS_PER_UNIT)
    return out


def _probe_binance(
    env_map: dict[str, str],
    req: HttpRequester | None,
    *,
    fixture_payload: Any = None,
    now_ms: int | None = None,
) -> dict[str, Any]:
    if fixture_payload is not None:
        raw = fixture_payload
    else:
        api_key = env_map.get("BINANCE_API_KEY", "").strip()
        api_secret = env_map.get("BINANCE_API_SECRET", "").strip()
        if not api_key and not api_secret:
            return {
                "venue": "binance",
                "settlement": "USDT",
                "configured": False,
                "status": "unconfigured",
                "account_fee_status": aaf.ACCOUNT_FEE_STATUS_UNKNOWN,
                "maker_fee_bps": None,
                "taker_fee_bps": None,
            }
        if not (api_key and api_secret):
            raise ValueError("incomplete Binance credentials")
        ts_ms = int(now_ms if now_ms is not None else time.time() * 1000)
        query = urlencode({"symbol": "BTCUSDT", "recvWindow": 5000, "timestamp": ts_ms})
        sig = hmac.new(api_secret.encode("utf-8"), query.encode("utf-8"), hashlib.sha256).hexdigest()
        url = f"{BINANCE_COMMISSION_URL}?{query}&signature={sig}"
        headers = {"X-MBX-APIKEY": api_key}
        assert req is not None
        raw = req(url, None, headers)

    aaf._reject_error_envelope(raw, "binance")
    if isinstance(raw, list):
        if not raw or not isinstance(raw[0], dict):
            raise ValueError("Binance commissionRate returned empty list")
        row = raw[0]
    elif isinstance(raw, dict):
        if "code" in raw and "msg" in raw and "makerCommissionRate" not in raw:
            raise ValueError(f"Binance commissionRate error code={raw.get('code')}")
        row = raw
    else:
        raise ValueError("Binance commissionRate invalid JSON")

    maker_bps = _parse_decimal(row.get("makerCommissionRate"), "binance.makerCommissionRate") * BPS_PER_UNIT
    taker_bps = _parse_decimal(row.get("takerCommissionRate"), "binance.takerCommissionRate") * BPS_PER_UNIT

    return {
        "venue": "binance",
        "settlement": "USDT",
        "configured": True,
        "status": "ok",
        "account_fee_status": aaf.ACCOUNT_FEE_STATUS_VERIFIED,
        "source_endpoints": [f"GET {BINANCE_COMMISSION_URL}?symbol=BTCUSDT"],
        "symbol": str(row.get("symbol") or "BTCUSDT"),
        "maker_fee_bps": _format_decimal(maker_bps),
        "taker_fee_bps": _format_decimal(taker_bps),
        "authenticated_probe_ok": True,
        "note": "Verified via read-only HMAC-SHA256 GET /fapi/v1/commissionRate?symbol=BTCUSDT.",
    }


def _probe_bulk(
    env_map: dict[str, str],
    req: HttpRequester | None,
    *,
    fixture_payload: Any = None,
) -> dict[str, Any]:
    if fixture_payload is not None:
        raw = fixture_payload
    else:
        base_url = env_map.get("BULK_API_URL", "").strip().rstrip("/") or DEFAULT_BULK_API_URL
        if not base_url.startswith("https://"):
            raise ValueError("BULK_API_URL must use HTTPS")
        assert req is not None
        raw = req(f"{base_url}/exchangeInfo", None, None)

    aaf._reject_error_envelope(raw, "bulk")
    if isinstance(raw, list):
        markets = [m for m in raw if isinstance(m, dict)]
    elif isinstance(raw, dict) and isinstance(raw.get("markets"), list):
        markets = [m for m in raw["markets"] if isinstance(m, dict)]
    elif isinstance(raw, dict) and isinstance(raw.get("symbols"), list):
        markets = [m for m in raw["symbols"] if isinstance(m, dict)]
    else:
        raise ValueError("Bulk /exchangeInfo returned unexpected shape")

    fee_pairs: set[tuple[str, str]] = set()
    for m in markets:
        mk_raw = m.get("makerFee", m.get("makerFeeRate"))
        tk_raw = m.get("takerFee", m.get("takerFeeRate"))
        if mk_raw is not None and tk_raw is not None:
            mk = _format_decimal(_parse_decimal(mk_raw, "bulk.makerFee") * BPS_PER_UNIT)
            tk = _format_decimal(_parse_decimal(tk_raw, "bulk.takerFee") * BPS_PER_UNIT)
            fee_pairs.add((mk, tk))

    if len(fee_pairs) == 1:
        mk_bps, tk_bps = next(iter(fee_pairs))
        return {
            "venue": "bulk",
            "settlement": "USDC",
            "configured": True,
            "status": "ok",
            "account_fee_status": aaf.ACCOUNT_FEE_STATUS_PUBLIC_TIER0,
            "source_endpoints": ["GET /exchangeInfo"],
            "market_count": len(markets),
            "maker_fee_bps": None,
            "taker_fee_bps": None,
            "public_schedule_maker_fee_bps": mk_bps,
            "public_schedule_taker_fee_bps": tk_bps,
            "fee_fields_exposed": True,
            "note": "Public /exchangeInfo exposed schedule rates only; per-account tier unverified.",
        }

    return {
        "venue": "bulk",
        "settlement": "USDC",
        "configured": True,
        "status": "ok_no_fee_fields_exposed",
        "account_fee_status": aaf.ACCOUNT_FEE_STATUS_UNKNOWN,
        "source_endpoints": ["GET /exchangeInfo"],
        "market_count": len(markets),
        "maker_fee_bps": None,
        "taker_fee_bps": None,
        "fee_fields_exposed": False,
        "note": "Public GET /exchangeInfo exposes market metadata only and omits fee-tier schedules.",
    }


def _run_venue_safe(
    venue: str,
    fn: Callable[[], dict[str, Any]],
    forbidden_literals: list[str],
) -> dict[str, Any]:
    try:
        return fn()
    except HTTPError as exc:
        return {
            "venue": venue,
            "configured": True,
            "status": "error",
            "account_fee_status": aaf.ACCOUNT_FEE_STATUS_UNKNOWN,
            "maker_fee_bps": None,
            "taker_fee_bps": None,
            "error": aaf._sanitize_error_text(f"HTTP {exc.code}", forbidden_literals),
        }
    except Exception as exc:  # noqa: BLE001
        return {
            "venue": venue,
            "configured": True,
            "status": "error",
            "account_fee_status": aaf.ACCOUNT_FEE_STATUS_UNKNOWN,
            "maker_fee_bps": None,
            "taker_fee_bps": None,
            "error": aaf._sanitize_error_text(f"{type(exc).__name__}: {exc}", forbidden_literals),
        }


def run_fee_tier_audit(
    venues: tuple[str, ...] | list[str] = SUPPORTED_VENUES,
    *,
    live: bool = False,
    env_map: dict[str, str] | None = None,
    fixtures: dict[str, Any] | None = None,
    requester: HttpRequester | None = None,
    go_runner: GoAuditRunner | None = None,
    mode_label: str | None = None,
    audited_at_utc: str | None = None,
) -> dict[str, Any]:
    """Execute read-only fee-tier verification across requested venues and validate redaction."""
    if not live and fixtures is None:
        raise ValueError("run_fee_tier_audit requires either live=True or fixtures")

    selected = tuple(dict.fromkeys(venues))
    unknown = [v for v in selected if v not in _VENUE_SET]
    if unknown:
        raise ValueError(f"unsupported venue(s): {', '.join(unknown)}")

    cfg = env_map or {}
    forbidden_literals = collect_all_env_literals(cfg)
    if fixtures is not None:
        forbidden_literals.extend(aaf._collect_forbidden_literals_from_fixtures(fixtures))

    req: HttpRequester | None = requester or (_live_http_json if live else None)

    def _aaf_req(url: str, body: dict[str, Any] | None) -> Any:
        assert req is not None
        return req(url, body, None)

    venue_rows: dict[str, dict[str, Any]] = {}

    go_venues = [v for v in selected if v in ("lighter", "lighter_robinhood", "backpack")]
    if go_venues:
        if fixtures is not None and "go_fee_audit" in fixtures:
            go_doc = fixtures["go_fee_audit"]
            go_rows = go_doc.get("venues", {}) if isinstance(go_doc, dict) else {}
            for gv in go_venues:
                if gv in go_rows:
                    venue_rows[gv] = dict(go_rows[gv])
                else:
                    venue_rows[gv] = {
                        "venue": gv,
                        "configured": False,
                        "status": "unconfigured",
                        "account_fee_status": aaf.ACCOUNT_FEE_STATUS_UNKNOWN,
                        "maker_fee_bps": None,
                        "taker_fee_bps": None,
                    }
        elif fixtures is not None:
            for gv in go_venues:
                if gv in fixtures and isinstance(fixtures[gv], dict):
                    venue_rows[gv] = dict(fixtures[gv])
                else:
                    venue_rows[gv] = {
                        "venue": gv,
                        "configured": False,
                        "status": "unconfigured",
                        "account_fee_status": aaf.ACCOUNT_FEE_STATUS_UNKNOWN,
                        "maker_fee_bps": None,
                        "taker_fee_bps": None,
                    }
        else:
            runner = go_runner or _run_go_fee_audit
            try:
                go_doc = runner(go_venues, cfg)
                go_rows = go_doc.get("venues", {})
                for gv in go_venues:
                    if gv in go_rows and isinstance(go_rows[gv], dict):
                        venue_rows[gv] = dict(go_rows[gv])
                    else:
                        venue_rows[gv] = {
                            "venue": gv,
                            "configured": True,
                            "status": "error",
                            "account_fee_status": aaf.ACCOUNT_FEE_STATUS_UNKNOWN,
                            "maker_fee_bps": None,
                            "taker_fee_bps": None,
                            "error": "missing venue result from go fee-audit",
                        }
            except Exception as exc:  # noqa: BLE001
                err_txt = aaf._sanitize_error_text(f"{type(exc).__name__}: {exc}", forbidden_literals)
                for gv in go_venues:
                    venue_rows[gv] = {
                        "venue": gv,
                        "configured": True,
                        "status": "error",
                        "account_fee_status": aaf.ACCOUNT_FEE_STATUS_UNKNOWN,
                        "maker_fee_bps": None,
                        "taker_fee_bps": None,
                        "error": err_txt,
                    }

    hl_addr = cfg.get("HYPERLIQUID_ACCOUNT_ADDRESS", "").strip() or cfg.get("ALEPH_MONITOR_HL_ADDRESS", "").strip()
    if hl_addr:
        forbidden_literals.append(hl_addr)

    for venue in selected:
        if venue in ("lighter", "lighter_robinhood", "backpack"):
            continue
        elif venue == "hyperliquid":
            if fixtures is not None:
                if "hyperliquid" in fixtures:
                    venue_rows[venue] = _run_venue_safe(
                        venue,
                        lambda: aaf._probe_hyperliquid(None, None, fixture_payload=fixtures["hyperliquid"]),
                        forbidden_literals,
                    )
                else:
                    venue_rows[venue] = {
                        "venue": venue,
                        "configured": False,
                        "status": "unconfigured",
                        "account_fee_status": aaf.ACCOUNT_FEE_STATUS_UNKNOWN,
                        "maker_fee_bps": None,
                        "taker_fee_bps": None,
                    }
            else:
                venue_rows[venue] = _run_venue_safe(
                    venue,
                    lambda: aaf._probe_hyperliquid(hl_addr or None, _aaf_req),
                    forbidden_literals,
                )
        elif venue == "trade_xyz":
            if fixtures is not None:
                if "trade_xyz" in fixtures:
                    venue_rows[venue] = _run_venue_safe(
                        venue,
                        lambda: aaf._probe_trade_xyz(None, None, fixture_payload=fixtures["trade_xyz"]),
                        forbidden_literals,
                    )
                else:
                    venue_rows[venue] = {
                        "venue": venue,
                        "configured": False,
                        "status": "unconfigured",
                        "account_fee_status": aaf.ACCOUNT_FEE_STATUS_UNKNOWN,
                        "maker_fee_bps": None,
                        "taker_fee_bps": None,
                    }
            else:
                venue_rows[venue] = _run_venue_safe(
                    venue,
                    lambda: aaf._probe_trade_xyz(hl_addr or None, _aaf_req, uses_fallback_hl_address=bool(hl_addr)),
                    forbidden_literals,
                )
        elif venue == "arcus":
            if fixtures is not None:
                if "arcus" in fixtures:
                    venue_rows[venue] = _run_venue_safe(
                        venue,
                        lambda: _probe_arcus_full(
                            cfg, None, fixture_payload=fixtures["arcus"], discovered_secrets=forbidden_literals
                        ),
                        forbidden_literals,
                    )
                else:
                    venue_rows[venue] = {
                        "venue": venue,
                        "configured": False,
                        "status": "unconfigured",
                        "account_fee_status": aaf.ACCOUNT_FEE_STATUS_UNKNOWN,
                        "maker_fee_bps": None,
                        "taker_fee_bps": None,
                    }
            else:
                venue_rows[venue] = _run_venue_safe(
                    venue,
                    lambda: _probe_arcus_full(cfg, req, discovered_secrets=forbidden_literals),
                    forbidden_literals,
                )
        elif venue == "extended":
            if fixtures is not None:
                if "extended" in fixtures:
                    venue_rows[venue] = _run_venue_safe(
                        venue,
                        lambda: _probe_extended(cfg, None, fixture_payload=fixtures["extended"]),
                        forbidden_literals,
                    )
                else:
                    venue_rows[venue] = {
                        "venue": venue,
                        "configured": False,
                        "status": "unconfigured",
                        "account_fee_status": aaf.ACCOUNT_FEE_STATUS_UNKNOWN,
                        "maker_fee_bps": None,
                        "taker_fee_bps": None,
                    }
            else:
                venue_rows[venue] = _run_venue_safe(
                    venue, lambda: _probe_extended(cfg, req), forbidden_literals
                )
        elif venue == "okx":
            if fixtures is not None:
                if "okx" in fixtures:
                    venue_rows[venue] = _run_venue_safe(
                        venue,
                        lambda: _probe_okx(cfg, None, fixture_payload=fixtures["okx"]),
                        forbidden_literals,
                    )
                else:
                    venue_rows[venue] = {
                        "venue": venue,
                        "configured": False,
                        "status": "unconfigured",
                        "account_fee_status": aaf.ACCOUNT_FEE_STATUS_UNKNOWN,
                        "maker_fee_bps": None,
                        "taker_fee_bps": None,
                    }
            else:
                venue_rows[venue] = _run_venue_safe(
                    venue, lambda: _probe_okx(cfg, req), forbidden_literals
                )
        elif venue == "binance":
            if fixtures is not None:
                if "binance" in fixtures:
                    venue_rows[venue] = _run_venue_safe(
                        venue,
                        lambda: _probe_binance(cfg, None, fixture_payload=fixtures["binance"]),
                        forbidden_literals,
                    )
                else:
                    venue_rows[venue] = {
                        "venue": venue,
                        "configured": False,
                        "status": "unconfigured",
                        "account_fee_status": aaf.ACCOUNT_FEE_STATUS_UNKNOWN,
                        "maker_fee_bps": None,
                        "taker_fee_bps": None,
                    }
            else:
                venue_rows[venue] = _run_venue_safe(
                    venue, lambda: _probe_binance(cfg, req), forbidden_literals
                )
        elif venue == "bulk":
            if fixtures is not None:
                if "bulk" in fixtures:
                    venue_rows[venue] = _run_venue_safe(
                        venue,
                        lambda: _probe_bulk(cfg, None, fixture_payload=fixtures["bulk"]),
                        forbidden_literals,
                    )
                else:
                    venue_rows[venue] = {
                        "venue": venue,
                        "configured": False,
                        "status": "unconfigured",
                        "account_fee_status": aaf.ACCOUNT_FEE_STATUS_UNKNOWN,
                        "maker_fee_bps": None,
                        "taker_fee_bps": None,
                    }
            else:
                venue_rows[venue] = _run_venue_safe(
                    venue, lambda: _probe_bulk(cfg, req), forbidden_literals
                )

    ordered_venues = {v: venue_rows[v] for v in selected}
    verified = [v for v, row in ordered_venues.items() if row["account_fee_status"] == aaf.ACCOUNT_FEE_STATUS_VERIFIED]
    public_only = [
        v
        for v, row in ordered_venues.items()
        if row["account_fee_status"]
        in (aaf.ACCOUNT_FEE_STATUS_PUBLIC_TIER0, aaf.ACCOUNT_FEE_STATUS_LIGHTER_PUBLIC_UNVERIFIED)
    ]
    unknown_venues = [
        v for v, row in ordered_venues.items() if row["account_fee_status"] == aaf.ACCOUNT_FEE_STATUS_UNKNOWN
    ]
    error_count = sum(1 for row in ordered_venues.values() if row.get("status") == "error")

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
        "venues": ordered_venues,
    }
    aaf.validate_redacted_output(report, forbidden_literals=forbidden_literals)
    return report


def build_self_test_fixtures() -> dict[str, Any]:
    """Deterministic offline fixtures across all 10 venues (including synthetic secrets to test redaction)."""
    base = aaf.build_embedded_self_test_fixtures()
    base["go_fee_audit"] = {
        "schema_version": "aleph_go_fee_audit_v1",
        "venues": {
            "lighter": {
                "venue": "lighter",
                "settlement": "USDC",
                "configured": True,
                "status": "ok",
                "account_fee_status": aaf.ACCOUNT_FEE_STATUS_VERIFIED,
                "source_endpoints": ["GET /api/v1/accountLimits", "GET /api/v1/account"],
                "maker_fee_bps": "0",
                "taker_fee_bps": "0",
                "public_schedule_maker_fee_bps": "0",
                "public_schedule_taker_fee_bps": "0",
                "current_maker_fee_tick": 0,
                "current_taker_fee_tick": 0,
                "account_type": 0,
                "account_type_label": "standard_type_0",
                "standard_zero_confirmed": True,
                "custom_fee_override_detected": False,
                "authenticated_probe_ok": True,
            },
            "lighter_robinhood": {
                "venue": "lighter_robinhood",
                "settlement": "USDG",
                "configured": True,
                "status": "ok",
                "account_fee_status": aaf.ACCOUNT_FEE_STATUS_VERIFIED,
                "source_endpoints": ["GET /api/v1/accountLimits", "GET /api/v1/account"],
                "maker_fee_bps": "0.5",
                "taker_fee_bps": "1.5",
                "public_schedule_maker_fee_bps": "0",
                "public_schedule_taker_fee_bps": "0",
                "current_maker_fee_tick": 50,
                "current_taker_fee_tick": 150,
                "account_type": 0,
                "account_type_label": "standard_type_0",
                "standard_zero_confirmed": False,
                "custom_fee_override_detected": True,
                "authenticated_probe_ok": True,
            },
            "backpack": {
                "venue": "backpack",
                "settlement": "USDC",
                "configured": True,
                "status": "ok_no_fee_fields_exposed",
                "account_fee_status": aaf.ACCOUNT_FEE_STATUS_UNKNOWN,
                "source_endpoints": ["GET /api/v1/account (instruction=accountQuery)"],
                "maker_fee_bps": None,
                "taker_fee_bps": None,
                "authenticated_probe_ok": True,
                "fee_fields_exposed": False,
            },
        },
    }
    base["arcus"]["account"] = {
        "accountIndex": 1,
        "address": "0x3333333333333333333333333333333333333333",
        "netQuoteBalance": "1234.56",
        "equity": "1234.56",
        "freeCollateral": "1000.00",
        "sequenceNumber": 99,
    }
    base["extended"] = {
        "status": "OK",
        "data": [
            {"market": "BTC-USD", "makerFeeRate": "0", "takerFeeRate": "0.00025", "builderFeeRate": "0"},
        ],
    }
    base["okx"] = {
        "code": "0",
        "msg": "",
        "data": [{"level": "Lv1", "category": "1", "makerU": "-0.0002", "takerU": "-0.0005", "maker": "-0.0002", "taker": "-0.0005"}],
    }
    base["binance"] = {
        "symbol": "BTCUSDT",
        "makerCommissionRate": "0.0002",
        "takerCommissionRate": "0.0005",
    }
    base["bulk"] = [
        {"symbol": "BTC-USD", "baseAsset": "BTC", "quoteAsset": "USD", "status": "ACTIVE"},
        {"symbol": "ETH-USD", "baseAsset": "ETH", "quoteAsset": "USD", "status": "ACTIVE"},
    ]
    return base


def run_self_test(venues: tuple[str, ...] | list[str] = SUPPORTED_VENUES) -> dict[str, Any]:
    """Run strictly offline deterministic self-test across all 10 venues."""
    fixtures = build_self_test_fixtures()
    with aaf._ForbiddenEnvGuard():
        report = run_fee_tier_audit(
            venues,
            live=False,
            fixtures=fixtures,
            mode_label="self_test",
            audited_at_utc="2026-11-27T00:00:00+00:00",
        )
        assert report["summary"]["error_venues"] == 0
        assert report["summary"]["redaction_verified"] is True
        v = report["venues"]
        assert v["lighter"]["maker_fee_bps"] == "0"
        assert v["lighter_robinhood"]["maker_fee_bps"] == "0.5"
        assert v["extended"]["maker_fee_bps"] == "0"
        assert v["extended"]["taker_fee_bps"] == "2.5"
        assert v["okx"]["maker_fee_bps"] == "2"
        assert v["okx"]["taker_fee_bps"] == "5"
        assert v["binance"]["maker_fee_bps"] == "2"
        assert v["binance"]["taker_fee_bps"] == "5"
        assert v["arcus"]["account_fee_status"] == aaf.ACCOUNT_FEE_STATUS_PUBLIC_TIER0
        assert v["backpack"]["account_fee_status"] == aaf.ACCOUNT_FEE_STATUS_UNKNOWN
        assert v["bulk"]["account_fee_status"] == aaf.ACCOUNT_FEE_STATUS_UNKNOWN

    return {
        "self_test": "passed",
        "schema_version": SCHEMA_VERSION,
        "venues_tested": list(report["venues"].keys()),
        "verified_account_tier_venues": report["summary"]["verified_account_tier_venues"],
        "public_schedule_only_venues": report["summary"]["public_schedule_only_venues"],
        "unknown_venues": report["summary"]["unknown_venues"],
        "redaction_verified": True,
    }


def _parse_venues_arg(raw: str | None) -> tuple[str, ...]:
    if raw is None or not raw.strip():
        return SUPPORTED_VENUES
    items = [part.strip().lower() for part in raw.split(",") if part.strip()]
    if not items:
        raise ValueError("--venues must list at least one venue")
    unknown = [v for v in items if v not in _VENUE_SET]
    if unknown:
        raise ValueError(f"unsupported venue(s): {', '.join(unknown)}")
    return tuple(dict.fromkeys(items))


def _write_json_output(path: Path, payload: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(payload, indent=2, allow_nan=False) + "\n", encoding="utf-8")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group(required=False)
    mode.add_argument("--live", action="store_true", help="Issue read-only HTTPS queries using --env-file in memory")
    mode.add_argument("--fixture", type=Path, default=None, help="Offline JSON fixture file")
    mode.add_argument("--self-test", action="store_true", help="Run strictly offline deterministic self-test")
    parser.add_argument("--env-file", type=Path, default=None, help="Dotenv file loaded strictly in memory for --live")
    parser.add_argument("--venues", type=str, default=",".join(SUPPORTED_VENUES), help="Comma-separated venues")
    parser.add_argument("--out", "--output", dest="out", type=Path, default=None, help="Redacted JSON output path")
    args = parser.parse_args(argv)

    if not args.live and args.fixture is None and not args.self_test:
        print("Error: refuse to run without --live, --fixture, or --self-test", file=sys.stderr)
        return 2
    if args.live and args.env_file is None:
        print("Error: --live requires --env-file <path>", file=sys.stderr)
        return 2
    if not args.live and args.env_file is not None:
        print("Error: --env-file is only valid with --live", file=sys.stderr)
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
        aaf.validate_redacted_output(summary)
        if args.out is not None:
            _write_json_output(args.out, summary)
        print(json.dumps(summary, indent=2, allow_nan=False))
        return 0

    if args.fixture is not None:
        loaded = json.loads(args.fixture.read_text(encoding="utf-8"))
        if not isinstance(loaded, dict):
            print("Error: --fixture root must be a JSON object", file=sys.stderr)
            return 2
        fixtures = (
            loaded["venues"]
            if isinstance(loaded.get("venues"), dict) and not any(v in loaded for v in _VENUE_SET)
            else loaded
        )
        report = run_fee_tier_audit(venues, live=False, fixtures=fixtures)
    else:
        env_map = parse_env_file_in_memory(args.env_file)
        report = run_fee_tier_audit(venues, live=True, env_map=env_map)

    if args.out is not None:
        _write_json_output(args.out, report)
    print(json.dumps(report, indent=2, allow_nan=False))
    return 1 if report["summary"]["error_venues"] > 0 else 0


if __name__ == "__main__":
    raise SystemExit(main())
