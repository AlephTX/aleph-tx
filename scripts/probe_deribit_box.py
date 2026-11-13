#!/usr/bin/env python3
"""Screen native Deribit USDC option boxes using public combo depth.

Cash-flow bounds are conditional on full execution, the supplied index and
cost caps, and holding the matched legs to settlement. Account margin is
unavailable from these public endpoints.
"""

import argparse
from collections import Counter
from decimal import Decimal, InvalidOperation
import json
import time
from urllib.error import URLError
from urllib.parse import urlencode
from urllib.request import Request, urlopen

API = "https://www.deribit.com/api/v2/public/"
MAX_BYTES = 8 * 1024 * 1024
MAX_BOXES = 12
BOOK_DEPTH = 10
MAX_BOOK_AGE_MS = 3_000
MAX_CATALOG_AGE_MS = 30_000
ENTRY_FEE = Decimal("0.0003")
DELIVERY_FEE = Decimal("0.00015")
FEE_SCHEDULE = "https://support.deribit.com/hc/en-us/articles/25944746248989-Fees"


def decimal(value):
    try:
        result = Decimal(str(value))
    except InvalidOperation as error:
        raise ValueError("invalid numeric input") from error
    if not result.is_finite():
        raise ValueError("non-finite numeric input")
    return result


def public_get(method, **parameters):
    request = Request(API + method + "?" + urlencode(parameters), headers={
        "Accept": "application/json", "Cache-Control": "no-cache",
        "User-Agent": "AlephTX-public-research/1.0",
    })
    with urlopen(request, timeout=8) as response:
        raw = response.read(MAX_BYTES + 1)
    received = time.time_ns() // 1_000_000
    if len(raw) > MAX_BYTES:
        raise ValueError(f"oversize response for {method}")
    envelope = json.loads(raw, parse_float=Decimal)
    source = envelope.get("usOut", 0) // 1_000
    max_age_ms = MAX_BOOK_AGE_MS if method == "get_order_book" else MAX_CATALOG_AGE_MS
    if "result" not in envelope or not -1_000 <= received - source <= max_age_ms:
        raise ValueError(f"missing or stale public response for {method}")
    if method == "get_order_book" and not -1_000 <= received - envelope["result"].get("timestamp", 0) <= MAX_BOOK_AGE_MS:
        raise ValueError("stale public book")
    return envelope["result"], source


def market_identity(instrument):
    return tuple(instrument.get(k) for k in (
        "base_currency", "price_index", "index_id", "expiration_timestamp", "contract_size",
    ))


def usable(instrument, kind):
    return (instrument.get("kind") == kind and instrument.get("is_active") is True
            and instrument.get("state") == "open" and instrument.get("instrument_type") == "linear"
            and instrument.get("quote_currency") == instrument.get("settlement_currency") == "USDC")


def box_terms(combo, instruments):
    """Recognize both orientations by payoff, independent of the combo's label."""
    legs = combo.get("legs", [])
    if combo.get("state") != "active" or len(legs) != 4:
        return None
    names = [leg.get("instrument_name") for leg in legs]
    if len(set(names)) != 4 or any(name not in instruments for name in names):
        return None
    options = [instruments[name] for name in names]
    identity = market_identity(options[0])
    if not all(identity) or any(not usable(opt, "option") or market_identity(opt) != identity for opt in options):
        return None
    if len({opt.get("settlement_period") for opt in options}) != 1:
        return None
    strikes = sorted({decimal(opt["strike"]) for opt in options})
    if len(strikes) != 2 or strikes[0] <= 0:
        return None
    low, high = strikes
    signs = {(decimal(opt["strike"]), opt.get("option_type")): leg.get("amount")
             for opt, leg in zip(options, legs, strict=True)}
    orientation = signs.get((low, "call"))
    if orientation not in (-1, 1) or signs != {
        (low, "call"): orientation, (low, "put"): -orientation,
        (high, "call"): -orientation, (high, "put"): orientation,
    }:
        return None
    return orientation, strikes, options


def execution_debit(levels, quantity, orientation):
    remaining = quantity
    debit = Decimal(0)
    previous = None
    for price, size in levels[:BOOK_DEPTH]:
        debit_per_base = decimal(price) * orientation
        size = decimal(size)
        if size <= 0 or (previous is not None and debit_per_base <= previous):
            raise ValueError("invalid native combo depth")
        taken = min(remaining, size)
        debit += debit_per_base * taken
        remaining -= taken
        if remaining == 0:
            return debit, debit_per_base
        previous = debit_per_base
    return None


def screen_quote(combo, terms, get, quantity, entry_index_cap, delivery_index_cap, financing):
    orientation, (low, high), options = terms
    name = combo["id"]
    spec, _ = get("get_instrument", instrument_name=name)
    book, source = get("get_order_book", instrument_name=name, depth=BOOK_DEPTH)
    result = {"combo": name, "side": "buy" if orientation == 1 else "sell", "observed_at_ms": source}
    if (not usable(spec, "option_combo") or market_identity(spec) != market_identity(options[0])
            or spec.get("instrument_id") != combo.get("instrument_id") or spec.get("instrument_name") != name
            or book.get("instrument_name") != name or book.get("state") != "open"
            or spec["expiration_timestamp"] <= source
            or not 0 <= source - book.get("timestamp", 0) <= MAX_BOOK_AGE_MS):
        return result | {"status": "book_or_instrument_unavailable"}
    minima = [decimal(row["min_trade_amount"]) for row in [spec, *options]]
    minimum_base = max(minima)
    quantity = minimum_base if quantity is None else quantity
    result["minimum_base"] = minimum_base
    if any(minimum <= 0 or quantity < minimum or quantity % minimum != 0 for minimum in minima):
        return result | {"status": "quantity_outside_screen"}
    levels = book["asks" if orientation == 1 else "bids"]
    execution = execution_debit(levels, quantity, orientation)
    if execution is None:
        return result | {"status": "insufficient_native_depth", "quantity_base": quantity}
    debit, worst_debit_per_base = execution
    limit_debit = worst_debit_per_base * quantity
    limit_price = worst_debit_per_base * orientation

    index = decimal(book["index_price"])
    if index <= 0:
        raise ValueError("nonpositive entry index")
    payoff = quantity * (high - low)
    # Screen on the undiscounted standard taker rate on all four legs.
    fee_estimate = 4 * ENTRY_FEE * index * quantity
    fee_bound = (4 * ENTRY_FEE * entry_index_cap * quantity
                 if entry_index_cap is not None and entry_index_cap >= index else None)
    # At most two options are in the money at any fixing. The fee cap on
    # each leg only lowers this bound. With no fixing cap, call fees can grow.
    delivery_bound = None
    expiry = spec["expiration_timestamp"]
    if options[0].get("settlement_period") == "day" and time.gmtime(expiry / 1000).tm_wday != 4:
        delivery_bound = Decimal(0)
    elif delivery_index_cap is not None:
        delivery_bound = 2 * DELIVERY_FEE * delivery_index_cap * quantity
    # Every fill of a native FOK limit is bounded by its limit, even if
    # displayed depth changes before the order reaches the matching engine.
    bounded_net = (payoff - limit_debit - fee_bound - delivery_bound - financing
                   if all(x is not None for x in (fee_bound, delivery_bound, financing)) else None)
    return result | {
        "status": "native_depth_observed", "book_timestamp_ms": book["timestamp"], "quantity_base": quantity,
        "expiry_ms": expiry, "index_at_snapshot": index,
        "gross_terminal_usdc": payoff, "entry_debit_usdc": debit,
        "native_fok_limit_price": limit_price, "limit_debit_usdc": limit_debit,
        "standard_fee_at_snapshot_usdc": fee_estimate, "entry_fee_bound_usdc": fee_bound,
        "delivery_fee_bound_usdc": delivery_bound, "conditional_net_usdc": bounded_net,
    }


def scan(get=public_get, quantity=None, entry_index_cap=None, delivery_index_cap=None, financing=None):
    combos, observed = get("get_combos", currency="USDC")
    active = [combo for combo in combos if combo.get("state") == "active"]
    four_leg = [combo for combo in active if len(combo.get("legs", [])) == 4]
    options, catalog_at = get("get_instruments", currency="USDC", kind="option", expired="false")
    instruments = {opt["instrument_name"]: opt for opt in options}
    missing_option_metadata = sum(
        any(leg.get("instrument_name") not in instruments for leg in combo["legs"]) for combo in four_leg
    )
    structures = Counter()
    for opt in options:
        if usable(opt, "option"):
            structures[opt["base_currency"]] += 1
    boxes = [(combo, terms) for combo in four_leg if (terms := box_terms(combo, instruments)) is not None]
    rows = [screen_quote(combo, terms, get, quantity, entry_index_cap, delivery_index_cap, financing)
            for combo, terms in boxes[:MAX_BOXES]]
    return {
        "venue": "deribit", "observed_at_ms": observed, "option_catalog_at_ms": catalog_at,
        "active_usdc_combos": len(active),
        "active_four_leg_usdc_combos": len(four_leg), "four_leg_missing_option_metadata": missing_option_metadata,
        "open_linear_usdc_options_by_base": dict(sorted(structures.items())),
        "native_boxes": len(boxes), "unqueried_native_boxes": max(0, len(boxes) - MAX_BOXES), "quotes": rows,
        "positive_conditional_quotes": sum(row.get("conditional_net_usdc") is not None
                                           and row["conditional_net_usdc"] > 0 for row in rows),
        "account_margin_verified": None, "verified_positive_executable_candidates": 0,
        "entry_index_cap": entry_index_cap, "delivery_index_cap": delivery_index_cap,
        "financing_and_other_cost_usdc": financing, "fee_schedule": FEE_SCHEDULE,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--quantity-base", type=decimal, help="largest native/leg minimum when omitted")
    parser.add_argument("--entry-index-cap", type=decimal, help="assumed upper index at matching")
    parser.add_argument("--delivery-index-cap", type=decimal, help="assumed upper fixing for non-daily delivery")
    parser.add_argument("--financing-and-other-cost-usdc", type=decimal, help="supplied full-cycle non-fee cost bound")
    args = parser.parse_args()
    if any(x is not None and x <= 0 for x in (args.quantity_base, args.entry_index_cap, args.delivery_index_cap)):
        parser.error("quantity and index caps must be positive")
    if args.financing_and_other_cost_usdc is not None and args.financing_and_other_cost_usdc < 0:
        parser.error("financing and other cost must be nonnegative")
    try:
        report = scan(quantity=args.quantity_base, entry_index_cap=args.entry_index_cap,
                      delivery_index_cap=args.delivery_index_cap, financing=args.financing_and_other_cost_usdc)
    except (URLError, TimeoutError, ValueError, KeyError, TypeError, InvalidOperation) as error:
        parser.exit(1, f"Deribit public probe failed ({type(error).__name__})\n")
    print(json.dumps(report, default=str, indent=2, allow_nan=False))


if __name__ == "__main__":
    main()
