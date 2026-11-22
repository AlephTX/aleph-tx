#!/usr/bin/env python3
"""Capture public rules, audit ANT metadata and price four-leg fee scenarios.

No account fields, keys, orders or trading calls. A passing audit is public-rule
consistency only; it cannot authorize a live strategy or certify incentive income.
"""
import argparse
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime, timezone
from decimal import Decimal, InvalidOperation
import hashlib
import json
from pathlib import Path
import subprocess
import sys

SOURCES = {
    "markets.json": ("https://api.arcus.xyz/v1/markets", None),
    "fees.json": ("https://api.arcus.xyz/v1/markets/feeSchedules", None),
    "xyz_meta.json": ("https://api.hyperliquid.xyz/info", {"type": "metaAndAssetCtxs", "dex": "xyz"}),
}


def decimal(value):
    if isinstance(value, (bool, float)):
        raise ValueError("decimal requires exact string or integer")
    result = Decimal(value)
    if not result.is_finite():
        raise ValueError("non-finite decimal")
    return result


def require(condition, reason):
    if not condition:
        raise ValueError(reason)


def utc(value):
    result = datetime.fromisoformat(value.replace("Z", "+00:00"))
    require(result.tzinfo is not None, "timezone required")
    return result.astimezone(timezone.utc)


def one(rows, predicate, name):
    matches = [row for row in rows if predicate(row)]
    require(len(matches) == 1, "missing or ambiguous " + name)
    return matches[0]


def capture(directory):
    directory.mkdir(parents=True, exist_ok=False)  # Preserve prior receipts/failures.

    def fetch(item):
        name, (url, body) = item
        started = datetime.now(timezone.utc)
        destination = directory / name
        receipt = {"file": name, "url": url, "request_body": body,
                   "requested_at": started.isoformat()}
        args = ["curl", "--silent", "--show-error", "--fail", "--max-time", "20",
                "--max-filesize", "4000000", "--output", str(destination),
                "--write-out", "%{json}"]
        if body is not None:
            args += ["--header", "Content-Type: application/json", "--data-binary", json.dumps(body)]
        try:
            result = subprocess.run(args + [url], capture_output=True, text=True, timeout=25)
            received = datetime.now(timezone.utc)
            receipt["received_at"] = received.isoformat()
            require(result.returncode == 0, "public fetch failed")
            meta = json.loads(result.stdout)
            require(meta["http_code"] == 200 and meta["ssl_verify_result"] == 0
                    and meta["url_effective"] == url, "unexpected HTTP/TLS/redirect")
            raw = destination.read_bytes()
            receipt.update(status=200, tls_verified=True, bytes=len(raw),
                           sha256=hashlib.sha256(raw).hexdigest())
        except (OSError, ValueError, subprocess.TimeoutExpired) as error:
            receipt.update(status="failed", reason=type(error).__name__)
        return receipt

    with ThreadPoolExecutor(max_workers=3) as pool:
        receipts = list(pool.map(fetch, SOURCES.items()))
    (directory / "receipts.json").write_text(json.dumps(receipts, indent=2) + "\n")
    return all(row["status"] == 200 for row in receipts)


def load_evidence(directory, as_of, max_age_seconds=60):
    require(0 < max_age_seconds <= 300, "freshness policy must be 1..300 seconds")
    receipts = json.loads((directory / "receipts.json").read_text())
    results, provenance, start_times, end_times = {}, [], [], []
    for name, (url, body) in SOURCES.items():
        row = one(receipts, lambda r: r["file"] == name, name + " receipt")
        require(row["status"] == 200 and row["tls_verified"] is True, "unsuccessful public receipt")
        require(row["url"] == url and row["request_body"] == body, "wrong public source/request")
        started, received = utc(row["requested_at"]), utc(row["received_at"])
        require(started <= received <= as_of, "invalid receipt clock order")
        # Use request start as conservative data-age bound, never mislabel it
        # as receipt time. Public endpoints do not certify source event age.
        require((as_of - started).total_seconds() <= max_age_seconds, "stale public rules")
        raw = (directory / name).read_bytes()
        digest = hashlib.sha256(raw).hexdigest()
        require(digest == row["sha256"] and len(raw) == row["bytes"], "public source hash/size mismatch")
        results[name] = json.loads(raw)
        start_times.append(started)
        end_times.append(received)
        provenance.append({"file": name, "sha256": digest,
                           "requested_at": started.isoformat(), "received_at": received.isoformat()})
    # Offline policy, not an exchange guarantee of atomic cross-venue state.
    require((max(end_times) - min(start_times)).total_seconds() <= 10, "public rule capture span exceeds 10s")
    return results, provenance


def tiers(rows):
    table = {}
    require(isinstance(rows, list) and bool(rows), "empty fee tiers")
    for row in rows:
        level = row["level"]
        require(type(level) is int and 0 <= level <= 6 and level not in table, "duplicate/invalid fee tier")
        maker, taker = row["makerFeePpm"], row["takerFeePpm"]
        require(type(maker) is int and type(taker) is int and taker >= 0, "invalid fee ppm")
        table[level] = (maker, taker)
    require(0 in table, "missing base fee tier")
    return table


def fee_scenarios(arcus_maker_bps, arcus_taker_bps, hl_taker_bps):
    result = {}
    for name, arcus_entry, arcus_exit in [
        ("arcus_maker_entry_and_exit", arcus_maker_bps, arcus_maker_bps),
        ("arcus_maker_entry_taker_exit", arcus_maker_bps, arcus_taker_bps),
        ("both_venues_taker_entry_and_exit", arcus_taker_bps, arcus_taker_bps),
    ]:
        cost = arcus_entry + arcus_exit + 2 * hl_taker_bps
        result[name] = {"four_leg_fees_bps_of_one_leg_principal": str(cost),
                        "fees_usd_per_million_one_leg_principal": str(cost * 100),
                        "fees_usd_per_million_all_four_leg_turnover": str(cost * 25),
                        "hours_at_hypothetical_12pct_net_simple_apr_365d": str(cost / 10000 / Decimal("0.12") * 8760)}
    return result


def audit_rules(data):
    rows = data["markets.json"]["markets"]
    market = one(rows, lambda x: x["marketDisplayName"] == "ANT-USD", "Arcus ANT")
    market_id = market["marketId"]
    require(type(market_id) is int and 0 <= market_id <= 65535, "invalid Arcus market id")
    one(rows, lambda x: x["marketId"] == market_id, "Arcus market id")
    for key, expected in {"status": "ONLINE", "type": "PERPETUAL", "baseAsset": "ANT",
                          "quoteAsset": "USD", "category": "EQUITIES"}.items():
        require(market[key] == expected, "unexpected Arcus " + key)
    fee = one(data["fees.json"]["feeSchedules"], lambda x: x["marketId"] == market_id, "ANT fee schedule")
    require(fee["marketDisplayName"] == "ANT-USD", "fee schedule symbol mismatch")
    table = tiers(fee["tiers"])
    require(table == tiers(market["feeTiers"]), "fee endpoints disagree")
    require(table[0] == (market["effectiveMakerFeePpm"], market["effectiveTakerFeePpm"]), "effective base fee mismatch")
    require(type(market["hasCustomFeeSchedule"]) is bool
            and market["hasCustomFeeSchedule"] == fee["hasCustomSchedule"], "fee schedule flags disagree")
    values = {key: decimal(market[key]) for key in ["tickSize", "stepSize", "minOrderSize",
              "maxOrderSize", "minOrderNotional", "initialMarginFraction", "offHoursInitialMarginFraction"]}
    require(all(v > 0 for v in values.values()), "nonpositive market limit")
    require(values["minOrderSize"] <= values["maxOrderSize"], "inverted size limits")
    require(values["initialMarginFraction"] <= values["offHoursInitialMarginFraction"] <= 1, "invalid margin fractions")
    require(type(market["isOutsideRth"]) is bool, "unknown trading session")
    require(market["markTier"] in ("FAST", "STANDARD"), "unknown mark tier")
    lower, upper = market["lowerTradingBound"], market["upperTradingBound"]
    if market["isOutsideRth"]:
        require(lower is not None and upper is not None, "missing off-hours bounds")
        require(0 < decimal(lower) < decimal(upper), "invalid off-hours bounds")
    else:
        require(lower is None and upper is None, "stale off-hours bounds in regular session")
    meta, contexts = data["xyz_meta.json"]
    require(len(meta["universe"]) == len(contexts), "misaligned HL contexts")
    asset = one(meta["universe"], lambda x: x["name"] == "xyz:ANT", "HL xyz:ANT")
    require(asset.get("isDelisted", False) is False, "HL ANT delisted")
    require(asset["growthMode"] in ("enabled", "disabled"), "unknown HL growth mode")
    require(type(asset["szDecimals"]) is int and 0 <= asset["szDecimals"] <= 8, "invalid HL lot precision")
    require(type(meta["collateralToken"]) is int and meta["collateralToken"] >= 0, "unknown HL collateral index")
    scale = decimal(asset["deployerFeeScale"])
    require(0 <= scale <= 10, "invalid HL fee scale")
    factor = 1 + scale if scale < 1 else 2 * scale
    if asset["growthMode"] == "enabled":
        factor *= Decimal("0.1")
    maker, taker = (Decimal(v) / 100 for v in table[0])
    hl_taker = Decimal("4.5") * factor
    return {
        "public_rules_consistent": True, "live_ready": False,
        "arcus_market_id": market_id, "arcus_base_tier_maker_bps": str(maker),
        "arcus_base_tier_taker_bps": str(taker),
        "arcus_is_outside_rth": market["isOutsideRth"],
        "arcus_trading_bounds": {"lower": lower, "upper": upper},
        "arcus_mark_tier": market["markTier"],
        "arcus_expansion_pressure_seconds": 600 if market["markTier"] == "FAST" else 3600,
        "arcus_initial_band_fraction_of_settlement": str(values["initialMarginFraction"] / 2),
        "hl_growth_mode": asset["growthMode"], "hl_deployer_fee_scale": str(scale),
        "hl_collateral_token_index": meta["collateralToken"],
        "hl_tier0_unstaked_unreferred_unaligned_taker_bps_assumption": str(hl_taker),
        "fee_scenarios": fee_scenarios(maker, taker, hl_taker),
        "points_cash_credit_usd": "0", "campaign_exact_end_utc": None,
        "missing_live_evidence": ["account_eligibility_and_actual_fees", "incentive_eligibility",
            "campaign_exact_end", "contract_and_collateral_equivalence", "fresh_executable_depth",
            "durable_arcus_xyz_execution_recovery", "continuous_10h_full_cycle_cash_validation",
            "explicit_capital_loss_and_live_action_authorization"],
        "scope": "public snapshot diagnostic; excludes spread/slippage/funding/borrow/FX and fee-state changes",
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["capture", "audit"])
    parser.add_argument("directory", type=Path)
    parser.add_argument("--as-of", help="explicit historical audit time; default current UTC")
    parser.add_argument("--max-age-seconds", type=int, default=60)
    args = parser.parse_args()
    try:
        if args.command == "capture":
            return 0 if capture(args.directory) else 2
        as_of = utc(args.as_of) if args.as_of else datetime.now(timezone.utc)
        data, provenance = load_evidence(args.directory, as_of, args.max_age_seconds)
        result = audit_rules(data)
        result.update(as_of=as_of.isoformat(), provenance=provenance,
                      historical_audit=args.as_of is not None)
    except (OSError, ValueError, KeyError, TypeError, InvalidOperation) as error:
        # Never echo malformed source bodies or account material to stdout.
        print(json.dumps({"public_rules_consistent": False, "live_ready": False,
                          "failure_class": type(error).__name__}))
        return 2
    print(json.dumps(result, indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main())
