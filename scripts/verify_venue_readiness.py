#!/usr/bin/env python3
"""Read-only 7-venue public readiness and fee-tier auditor (`OPT-01` / pre-flight support).

Strictly avoids reading or transmitting any private signing keys. Accepts either
a public-only `--monitor-env` file or extracts public identifiers from `--env`
via `deploy/oasis/sync_monitor_env.py` before issuing read-only GET/POST `/info`
requests when `--live` is explicitly enabled.
"""

from __future__ import annotations

import argparse
from decimal import Decimal, InvalidOperation
import json
from pathlib import Path
import re
import sys
from typing import Any, Callable
from urllib.error import HTTPError
from urllib.parse import urlencode
from urllib.request import Request, urlopen

REPO_ROOT = Path(__file__).resolve().parent.parent
for _cand in (REPO_ROOT / "deploy" / "oasis", REPO_ROOT):
    if _cand.is_dir() and str(_cand) not in sys.path:
        sys.path.insert(0, str(_cand))

import sync_monitor_env  # type: ignore  # noqa: E402

EVM_ADDR_RE = re.compile(r"0x[0-9a-fA-F]{40}")
BASE58_RE = re.compile(r"[1-9A-HJ-NP-Za-km-z]{32,44}")
JsonRequester = Callable[[str, dict[str, Any] | None], Any]


def default_request_json(url: str, body: dict[str, Any] | None = None, timeout: float = 8.0) -> Any:
    headers = {"Accept": "application/json", "User-Agent": "AlephTX-Readiness/1.0"}
    data = None
    if body is not None:
        data = json.dumps(body).encode("utf-8")
        headers["Content-Type"] = "application/json"
    req = Request(url, data=data, headers=headers)
    with urlopen(req, timeout=timeout) as resp:
        return json.loads(resp.read(2_000_000).decode("utf-8"))


def _dec_or_none(raw: Any) -> Decimal | None:
    if raw is None or isinstance(raw, bool):
        return None
    try:
        val = Decimal(str(raw))
    except (InvalidOperation, ValueError):
        return None
    return val if val.is_finite() else None


def extract_public_monitor_config(
    *,
    monitor_env: Path | None = None,
    source_env: Path | None = None,
) -> dict[str, str]:
    """Load public-only identifiers, stripping every private key or API secret."""
    if monitor_env is not None and source_env is not None:
        raise ValueError("pass only one of --monitor-env or --env")
    if monitor_env is not None:
        raw = sync_monitor_env.parse_dotenv(monitor_env)
        return {
            k: v
            for k, v in raw.items()
            if k.startswith("ALEPH_MONITOR_")
            and not any(frag in k for frag in sync_monitor_env.FORBIDDEN_KEY_FRAGMENTS)
            and k != "ALEPH_MONITOR_CONTROL_TOKEN"
        }
    if source_env is not None:
        raw = sync_monitor_env.parse_dotenv(source_env)
        pairs = sync_monitor_env.build_monitor_env(raw, None)
        return {k: v for k, v in pairs if k != "ALEPH_MONITOR_CONTROL_TOKEN"}
    raise ValueError("one of --monitor-env or --env is required")


def _rows(value: Any) -> list[dict]:
    if not isinstance(value, list) or not all(isinstance(row, dict) for row in value):
        raise ValueError("expected venue record list")
    return value


def _required_number(record: dict, field: str) -> Decimal:
    value = _dec_or_none(record.get(field))
    if value is None:
        raise ValueError(f"missing or invalid {field}")
    return value


def _response(value: Any) -> dict:
    if not isinstance(value, dict) or value.get("error") or value.get("status") in ("err", "error", "failed"):
        raise ValueError("venue returned an invalid response")
    return value


def _probe_lighter(index: str, base_url: str, req: JsonRequester) -> dict[str, Any]:
    if not re.fullmatch(r"[0-9]{1,19}", index) or not (0 < int(index) <= 2**63 - 1):
        raise ValueError("invalid Lighter account index")
    data = req(f"{base_url.rstrip('/')}/api/v1/account?by=index&value={index}", None)
    if not isinstance(data, dict) or data.get("code") != 200:
        raise ValueError(f"Lighter account query code={data.get('code') if isinstance(data, dict) else 'invalid'}")
    matches = [a for a in _rows(data.get("accounts")) if a.get("index") == int(index)]
    if len(matches) != 1:
        raise ValueError("expected exactly one matching account")
    acct = matches[0]
    positions = _rows(acct.get("positions"))
    non_zero_pos = [p for p in positions if _required_number(p, "position") != 0]
    collateral = _required_number(acct, "collateral")
    avail = _required_number(acct, "available_balance")
    counts = [_required_number(p, "open_order_count") for p in positions]
    if any(c < 0 or c != int(c) for c in counts):
        raise ValueError("invalid open order count")
    open_orders = sum(map(int, counts))
    return {
        "configured": True,
        "status": "ok_flat" if (not non_zero_pos and open_orders == 0) else "ok_open_state",
        "collateral": str(collateral) if collateral is not None else None,
        "available_balance": str(avail) if avail is not None else None,
        "open_positions": len(non_zero_pos),
        "open_orders": open_orders,
    }


def _probe_hyperliquid_dex(address: str, dex: str, req: JsonRequester, *, include_fees: bool = False) -> dict[str, Any]:
    if not EVM_ADDR_RE.fullmatch(address):
        raise ValueError("invalid EVM address")
    state = req("https://api.hyperliquid.xyz/info", {"type": "clearinghouseState", "user": address, "dex": dex})
    state = _response(state)
    acct_val = _required_number(_response(state.get("marginSummary")), "accountValue")
    withdrawable = _required_number(state, "withdrawable")
    positions = [item for item in _rows(state.get("assetPositions"))
                 if _required_number(_response(item.get("position")), "szi") != 0]
    orders_payload: dict[str, Any] = {"type": "frontendOpenOrders", "user": address}
    if dex:
        orders_payload["dex"] = dex
    open_orders = req("https://api.hyperliquid.xyz/info", orders_payload)
    open_cnt = len(_rows(open_orders))
    out: dict[str, Any] = {
        "configured": True,
        "status": "ok_flat" if (not positions and open_cnt == 0) else "ok_open_state",
        "account_value_usd": str(acct_val) if acct_val is not None else None,
        "withdrawable_usd": str(withdrawable) if withdrawable is not None else None,
        "open_positions": len(positions),
        "open_orders": open_cnt,
    }
    if include_fees:
        fees = req("https://api.hyperliquid.xyz/info", {"type": "userFees", "user": address})
        cross = _dec_or_none((fees or {}).get("userCrossRate"))
        add = _dec_or_none((fees or {}).get("userAddRate"))
        out["fees"] = {
            "user_cross_rate_bps": round(float(cross) * 10_000.0, 4) if cross is not None else None,
            "user_add_rate_bps": round(float(add) * 10_000.0, 4) if add is not None else None,
            "active_referral_discount": (fees or {}).get("activeReferralDiscount"),
        }
    return out


def _probe_arcus(address: str, index: str, req: JsonRequester) -> dict[str, Any]:
    if not EVM_ADDR_RE.fullmatch(address):
        raise ValueError("invalid Arcus EVM address")
    if not re.fullmatch(r"[0-9]", index):
        raise ValueError("invalid Arcus account index")
    query = urlencode({"address": address.lower(), "accountIndex": index})
    pos_data = req(f"https://api.arcus.xyz/v1/positions?{query}", None)
    raw_pos = _response(pos_data).get("positions")
    items = _rows(list(raw_pos.values()) if isinstance(raw_pos, dict) else raw_pos)
    non_zero_pos = [it for it in items if _required_number(it, "size") != 0]
    orders_data = req(f"https://api.arcus.xyz/v1/openOrders?{query}", None)
    orders = _response(orders_data).get("orders") if isinstance(orders_data, dict) else orders_data
    open_cnt = len(_rows(orders))
    return {
        "configured": True,
        "status": "ok_flat" if (not non_zero_pos and open_cnt == 0) else "ok_open_state",
        "open_positions": len(non_zero_pos),
        "open_orders": open_cnt,
    }


def _probe_bulk(pubkey: str, req: JsonRequester) -> dict[str, Any]:
    if not BASE58_RE.fullmatch(pubkey):
        raise ValueError("invalid BULK Base58 pubkey")
    data = req("https://mainnet-api1.bulk.trade/api/v1/account", {"type": "fullAccount", "user": pubkey})
    entries = data if isinstance(data, list) else [data]
    pos_cnt = 0
    open_cnt = 0
    if not entries:
        raise ValueError("missing BULK account")
    for entry in _rows(entries):
        entry = _response(entry)
        acct = _response(entry.get("fullAccount", entry))
        pos_cnt += sum(_required_number(it, "size") != 0 for it in _rows(acct.get("positions")))
        open_cnt += len(_rows(acct.get("openOrders")))
    return {
        "configured": True,
        "status": "ok_flat" if (pos_cnt == 0 and open_cnt == 0) else "ok_open_state",
        "open_positions": pos_cnt,
        "open_orders": open_cnt,
    }


def _run_safe(fn: Callable[[], dict[str, Any]]) -> dict[str, Any]:
    try:
        return fn()
    except HTTPError as exc:
        return {"configured": True, "status": "error", "error": f"HTTP {exc.code}"}
    except Exception as exc:
        return {"configured": True, "status": "error", "error": f"{type(exc).__name__}: {exc}"}


def evaluate_readiness(pub_cfg: dict[str, str], *, requester: JsonRequester | None = None) -> dict[str, Any]:
    """Query configured public endpoints and summarize per-venue readiness."""
    req = requester or default_request_json
    hl_addr = pub_cfg.get("ALEPH_MONITOR_HL_ADDRESS", "").strip()
    hl_xyz = pub_cfg.get("ALEPH_MONITOR_HL_XYZ_ADDRESS", "").strip() or hl_addr
    hl_io = pub_cfg.get("ALEPH_MONITOR_HL_IO_ADDRESS", "").strip() or hl_addr
    arc_addr = pub_cfg.get("ALEPH_MONITOR_ARCUS_ADDRESS", "").strip()
    arc_idx = pub_cfg.get("ALEPH_MONITOR_ARCUS_INDEX", "").strip() or "0"
    lgt_idx = pub_cfg.get("ALEPH_MONITOR_LIGHTER_INDEX", "").strip()
    lgt_rh = pub_cfg.get("ALEPH_MONITOR_LIGHTER_RH_INDEX", "").strip()
    bulk_pk = pub_cfg.get("ALEPH_MONITOR_BULK_PUBKEY", "").strip()

    venues: dict[str, dict[str, Any]] = {
        "Lighter": _run_safe(lambda: _probe_lighter(lgt_idx, "https://mainnet.zklighter.elliot.ai", req))
        if lgt_idx else {"configured": False, "status": "unconfigured"},
        "LighterRH": _run_safe(lambda: _probe_lighter(lgt_rh, "https://api.rh.lighter.xyz", req))
        if lgt_rh else {"configured": False, "status": "unconfigured"},
        "Hyperliquid": _run_safe(lambda: _probe_hyperliquid_dex(hl_addr, "", req, include_fees=True))
        if hl_addr else {"configured": False, "status": "unconfigured"},
        "TradeXYZ": _run_safe(lambda: _probe_hyperliquid_dex(hl_xyz, "xyz", req))
        if hl_xyz else {"configured": False, "status": "unconfigured"},
        "Entropy": _run_safe(lambda: _probe_hyperliquid_dex(hl_io, "io", req))
        if hl_io else {"configured": False, "status": "unconfigured"},
        "Arcus": _run_safe(lambda: _probe_arcus(arc_addr, arc_idx, req))
        if arc_addr else {"configured": False, "status": "unconfigured"},
        "BULK": _run_safe(lambda: _probe_bulk(bulk_pk, req))
        if bulk_pk else {"configured": False, "status": "unconfigured"},
    }

    return {
        "venues": venues,
        "readiness": {
            "arcus_xyz_basis_ready_flat": all(
                venues[v]["status"] == "ok_flat" for v in ("Arcus", "TradeXYZ")
            ) and venues["Hyperliquid"]["status"] in ("ok_flat", "ok_open_state"),
            "lighter_mm_ready_flat": all(
                venues[v]["status"] == "ok_flat" for v in ("Lighter", "Hyperliquid")
            ),
            "configured_count": sum(1 for v in venues.values() if v["configured"]),
            "error_venues": [name for name, row in venues.items() if row["status"] == "error"],
        },
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    src_group = parser.add_mutually_exclusive_group(required=True)
    src_group.add_argument("--monitor-env", type=Path, help="Public-only monitor.env path")
    src_group.add_argument("--env", type=Path, help="Unified .env path (only public identifiers are extracted)")
    parser.add_argument("--live", action="store_true", help="Issue read-only HTTP queries to public venue endpoints")
    args = parser.parse_args(argv)

    if not args.live:
        print(json.dumps({"error": "pass --live to execute read-only public network queries"}))
        return 2

    pub_cfg = extract_public_monitor_config(monitor_env=args.monitor_env, source_env=args.env)
    rep = evaluate_readiness(pub_cfg)
    print(json.dumps(rep, indent=2))
    return 0 if not rep["readiness"]["error_venues"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
