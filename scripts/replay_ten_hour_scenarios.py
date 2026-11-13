#!/usr/bin/env python3
"""Independent continuous quant/turnover diagnostics on a sealed public campaign.

Fixed policies, no parameter search. Every asset/capital/posture/cost arm gets
its own process and ledger. A completed engine run is not a coverage, account
fee, incentive-eligibility or production-readiness certificate.
"""
import argparse
import gzip
from decimal import Decimal
from heapq import merge
import json
import os
from pathlib import Path
import subprocess
import tomllib

from archive_public_capture import verify_compiled
from replay_maker_campaign import books, load, save, strategy_config, unseal
from replay_maker_first_campaign import digest
from public_replay_sources import trade_events

TEN_HOURS = 36_000_000
CAPITAL_SIZES = ((500, 50), (1000, 100))
POSTURES = ("neutral", "long_first", "short_first")



def strict_trade_rows(files, market, leg, end):
    """Preserve bad source times for engine rejection; never infer reconnects."""
    seen, previous = {}, None
    for path in files:
        with gzip.open(path, "rt") as stream:
            for line in stream:
                row = json.loads(line)
                at = row["received_at_ms"]
                if not isinstance(at, int) or isinstance(at, bool) or at < 0:
                    raise ValueError("invalid public receipt time")
                if previous is not None and at < previous:
                    raise ValueError("noncausal public trade archive")
                previous = at
                if row["kind"] == "feed_state":
                    if row["state"] not in ("connected", "disconnected"):
                        raise ValueError("unknown public feed state")
                    if at < end:
                        yield {"kind": "trade_feed", "at_ms": at, "leg": leg,
                               "online": row["state"] == "connected"}
                    continue
                if row.get("kind") != "trade" or row.get("market") != market:
                    continue
                source, side = row["source_time_ms"], row["side"]
                if side not in ("buy", "sell"):
                    raise ValueError("invalid public aggressor side")
                value = (source, side, str(row["price"]), str(row["size"]))
                identity = row.get("trade_id") or value
                if identity in seen:
                    if seen[identity] != value:
                        raise ValueError("conflicting duplicate public trade")
                    continue
                seen[identity] = value
                if at < end:
                    yield {"kind": "trade", "at_ms": at, "source_ms": source, "leg": leg,
                           "aggressor": side, "price": str(row["price"]), "contracts": str(row["size"])}


def time_window(campaign, symbol, compiled=None, start_ms=None):
    source = compiled or campaign / "results/final" / f"{symbol}-freshest.ndjson.zst"
    with unseal(source) as stream:
        next(stream)
        first = json.loads(next(stream))["now_ms"]
        # Read to EOF to verify the compressed archive, even during discovery.
        last = first
        for line in stream:
            last = json.loads(line)["now_ms"]
    begin = start_ms if start_ms is not None else first + 600_000
    if begin - first < 60_000:
        raise ValueError("requested window lacks sixty seconds of warm-up")
    end = begin + TEN_HOURS
    if last < end + 15_000:
        raise ValueError("source lacks ten decision hours and terminal drain")
    return {"warmup_start_ms": begin - 60_000, "start_ms": begin,
            "end_ms": end, "drain_until_ms": end + 15_000}


def quant_input(campaign, symbol, window, output, *, compiled=None, public_archive=None, lighter_id=None):
    files = sorted((campaign / "raw").glob("lighter-trades-*.jsonl.gz"))
    if public_archive is None and not files:
        raise ValueError("missing Lighter public trade files")
    first, stop = window["warmup_start_ms"], window["drain_until_ms"]
    prints = (strict_trade_rows(files, symbol, 0, stop) if public_archive is None else
              trade_events(public_archive, "lighter", str(lighter_id), 0))
    timeline = merge(books(campaign, symbol, compiled), prints,
                     key=lambda row: (row["at_ms"], row["kind"] == "book"))
    online = False
    states = 0
    counts = {"book": 0, "trade": 0, "trade_feed": 0}
    with output.open("x") as target:
        def emit(row):
            counts[row["kind"]] += 1
            target.write(json.dumps(row, separators=(",", ":"), allow_nan=False) + "\n")
        # A false initial state is explicit unknown connectivity, never guessed
        # from a book or a previous print. Only source feed_state changes it.
        initialized = False
        for row in timeline:
            at = row["at_ms"]
            if at < first:
                if row["kind"] == "trade_feed":
                    online = row["online"]
                    states += 1
                continue
            if not initialized:
                emit({"kind": "trade_feed", "at_ms": first, "online": online})
                initialized = True
            if at >= stop:
                continue
            if row["kind"] == "trade_feed":
                online = row["online"]
                states += 1
                emit({"kind": "trade_feed", "at_ms": at, "online": online})
            elif row["kind"] == "trade":
                emit({"kind": "trade", "at_ms": at, "source_ms": row["source_ms"],
                      "price": float(row["price"]), "size": float(row["contracts"]),
                      "aggressor": row["aggressor"]})
            else:
                emit(row)
    return {"events": counts, "explicit_feed_states": states,
            "compiled_sha256": digest(compiled or campaign / "results/final" / f"{symbol}-freshest.ndjson.zst"),
            "public_trade_files_sha256": ({path.name: digest(path) for path in files} if public_archive is None else
                                          {"manifest.json": digest(public_archive / "manifest.json")}),
            "input_sha256": digest(output)}



def robinhood_market_id(config, lineage, symbol, core_market_id):
    """Bind both domains to the exact profile that produced the paired tape."""
    if digest(config) != lineage["config_sha256"]:
        raise ValueError("Robinhood market mapping differs from compilation profile")
    profile = load(config)
    if ([(leg["venue"], leg["market"]) for leg in profile["pair"]["legs"]]
            != [("lighter", symbol), ("lighter_robinhood", symbol)]
            or profile["legs"][0]["lighter_market_id"] != core_market_id):
        raise ValueError("unexpected Robinhood or Core trading domain mapping")
    identifier = profile["legs"][1]["lighter_market_id"]
    if type(identifier) is not int or not 0 <= identifier <= 65535:
        raise ValueError("invalid Robinhood native market ID")
    return identifier


def turnover_input(campaign, symbol, window, output, *, compiled=None, public_archive=None, lighter_id=None,
                   target="arcus", target_market_id=None):
    if target == "lighter_robinhood" and (public_archive is None or type(target_market_id) is not int
                                          or not 0 <= target_market_id <= 65535):
        raise ValueError("Robinhood requires its archived native market ID and public archive")
    start, stop = window["start_ms"], window["drain_until_ms"]
    route = f"{symbol}-lighter-{target}"
    source = compiled or campaign / "results/final" / f"{route}.ndjson.zst"
    spans = [[None, None], [None, None]]
    book_span = [None, None]
    info, counts = {}, {"book": 0, "trade": 0, "trade_feed": 0}
    explicit_feed_states = [0, 0]
    with unseal(source) as stream:
        pair = json.loads(next(stream))["pair"]
        if [leg["venue"] for leg in pair["legs"]] != ["lighter", target]:
            raise ValueError("unexpected turnover source domains")
        def book_events():
            previous = None
            for line in stream:
                frame = json.loads(line)
                at = frame["now_ms"]
                if previous is not None and at <= previous:
                    raise ValueError("noncausal turnover book tape")
                previous = at
                if book_span[0] is None:
                    book_span[0] = at
                book_span[1] = at
                # This is an explicit paper capital allowance, not displayed
                # depth. Every bid/ask level, time and tradability flag survives.
                books = [leg["book"] | {"available_notional": "150"} for leg in frame["legs"]]
                yield {"kind": "book", "at_ms": at, "books": books}
        def prints(leg):
            terms = pair["legs"][leg]
            venue = terms["venue"]
            if public_archive is None:
                paths = sorted((campaign / "raw").glob(f"{venue}-trades-*.jsonl.gz"))
                if not paths:
                    raise ValueError("missing turnover public trade source")
                info[venue] = {p.name: digest(p) for p in paths}
                events = strict_trade_rows(paths, terms["market"], leg, 2**63)
            else:
                info[venue] = {"manifest.json": digest(public_archive / "manifest.json")}
                market = str(lighter_id) if leg == 0 else (
                    str(target_market_id) if venue == "lighter_robinhood" else terms["market"])
                events = trade_events(public_archive, venue, market, leg)
            for event in events:
                if event["kind"] == "trade_feed":
                    explicit_feed_states[leg] += 1
                if spans[leg][0] is None:
                    spans[leg][0] = event["at_ms"]
                spans[leg][1] = event["at_ms"]
                yield event
        timeline = merge(prints(0), prints(1), book_events(),
                         key=lambda event: (event["at_ms"], event["kind"] == "book"))
        online, initialized = [False, False], False
        with output.open("x") as target:
            def emit(row):
                if row["kind"] in counts:
                    counts[row["kind"]] += 1
                target.write(json.dumps(row, separators=(",", ":"), allow_nan=False) + "\n")
            for event in timeline:
                at = event["at_ms"]
                if at < start:
                    if event["kind"] == "trade_feed":
                        online[event["leg"]] = event["online"]
                    continue
                if not initialized:
                    for leg, active in enumerate(online):
                        emit({"kind": "trade_feed", "at_ms": start, "leg": leg, "online": active})
                    initialized = True
                if at >= stop:
                    continue
                # A cached print following a disconnect is not reconnection
                # evidence. Only original feed_state controls online status.
                if event["kind"] == "trade_feed":
                    online[event["leg"]] = event["online"]
                emit(event)
            if (book_span[0] is None or book_span[0] > start or book_span[1] < stop
                    or any(a is None or a > start or b < stop for a, b in spans)):
                raise ValueError("turnover source lacks requested book/trade envelope")
            emit({"kind": "end", "at_ms": stop})
    return pair, {"public_trade_files_sha256": info, "compiled_sha256": digest(source),
                  "input_sha256": digest(output), "book_envelope_ms": book_span,
                  "trade_envelopes_ms": spans, "events": counts,
                  "explicit_feed_states_by_leg": explicit_feed_states,
                  "availability_assumption": "150 quote units per leg; raw depth unchanged",
                  "stream_quality_verified": False}


def quant_policy(base, market, symbol, window, capital, size):
    return {"strategy": strategy_config(base, market, symbol, capital, 0.0, {
                "min_profit_bps": 12.0, "base_order_notional_usd": float(size)}),
            "start_ms": window["warmup_start_ms"], "trade_from_ms": window["start_ms"],
            "end_ms": window["end_ms"], "drain_until_ms": window["drain_until_ms"],
            "initial_equity": capital, "order_entry_ms": 250, "order_response_ms": 100,
            "venue_cancel_delay_ms": 300, "venue_taker_delay_ms": 300,
            "trade_clock_guard_ms": 100, "trade_delivery_grace_ms": 2000,
            "taker_fee_bps": 0.0, "depth_participation": 0.25,
            "min_contracts": float(market["min_base_amount"]),
            "min_notional": float(market["min_quote_amount"]),
            "flat_at_funding": {"interval_ms": 3_600_000, "offset_ms": 0,
                "drain_before_ms": 15_000, "guard_ms": 1000, "max_event_gap_ms": 5000}}


def turnover_policies(pair, window, fees):
    start, end, stop = (window[k] for k in ("start_ms", "end_ms", "drain_until_ms"))
    if pair["legs"][0]["venue"] != "lighter" or pair["legs"][1]["venue"] not in ("arcus", "extended", "hyperliquid", "lighter_robinhood"):
        raise ValueError("expected Core Lighter hedge and a supported target domain")
    target_delay = 300 if pair["legs"][1]["venue"] == "lighter_robinhood" else 0
    for capital, size in CAPITAL_SIZES:
        for posture in POSTURES:
            for cost in (0, 4):
                label = f"{posture}-{capital}capital-{size}size-{cost}bps"
                legs = [dict(leg) for leg in pair["legs"]]
                legs[0]["taker_fee_bps"], legs[1]["taker_fee_bps"] = "0", fees[1]
                mode = {"kind": "turnover", "max_turnover_quote": str(capital * 20),
                        "max_fees_quote": str(Decimal(capital) / 50),
                        "max_realized_loss_quote": str(Decimal(capital) / 200),
                        "incentive": {"eligible_leg": 1, "eligible_from_ms": start,
                                      "eligible_until_ms": end, "max_cost_bps": str(cost)}}
                yield label, {"pair": pair | {"legs": legs, "mode": mode},
                    "maker_leg": 1, "start_ms": start, "end_ms": stop,
                    "transport_ms": 250, "fill_notice_ms": 100, "maker_cancel_delay_ms": target_delay,
                    "taker_delay_ms": [300, target_delay], "trade_clock_guard_ms": 100,
                    "trade_delivery_grace_ms": 750, "quote_lifetime_ms": 800,
                    "reprice_ms": 100, "hold_ms": 15_000, "unhedged_timeout_ms": 2500,
                    "drain_ms": 15_000, "require_net_edge": True,
                    "maker_fee_bps": fees[0], "quote_notional": str(size), "latency_reserve_bps": "0.5",
                    "hedged_exit": {"max_hold_ms": 60_000, "min_saving_bps": "0", "max_adverse_basis_bps": "10"},
                    "turnover_feedback": {"min_reserve_bps": "0.25", "max_reserve_bps": "0.25",
                        "alpha": "0", "overhead_bps": "0.25", "max_quote_distance_bps": "12"},
                    "turnover_posture": {"initial": posture, "max_control_ttl_ms": 300_000},
                    "flat_at_funding": {"settlement_guard_ms": 1000}}


def execute(engine, config, tape, output, maker_first=False):
    command = [str(engine)] + (["maker-first-replay"] if maker_first else []) + [str(config), str(tape)]
    with output.open("x") as stdout, output.with_suffix(".stderr").open("x") as stderr:
        try:
            proc = subprocess.run(command, stdout=stdout, stderr=stderr, timeout=600)
            code = proc.returncode
        except subprocess.TimeoutExpired:
            return {"incomplete": "execution_timeout", "engine_exit_code": None}
    if code:
        return {"incomplete": "engine_exit_failure", "engine_exit_code": code}
    return load(output) | {"engine_exit_code": code}


def summarize_quant(row):
    keys = ("book_frames", "tape_trades", "maker_fills", "taker_fills", "maker_turnover", "taker_turnover",
            "maker_fees", "taker_fees", "net_liquidation_pnl", "halted", "incomplete", "engine_exit_code",
            "funding_mode", "funding_boundaries_verified", "late_cancel_fills", "historical_trades",
            "strategy_startups", "funding_rejections", "warmup_rejections", "max_drawdown",
            "trade_feed_gaps", "trade_feed_continuity_verified", "final_position", "resting_orders", "needs_reconciliation")
    result = {key: row.get(key) for key in keys}
    flat = (row.get("final_position") == 0 and row.get("resting_orders") == 0
            and row.get("needs_reconciliation") is False and row.get("halted") is None
            and row.get("trade_feed_continuity_verified") is True)
    net = row.get("net_liquidation_pnl") if flat else None
    result["terminal_flat_and_feed_verified"] = flat
    volume = row.get("maker_turnover", 0) + row.get("taker_turnover", 0)
    result["net_after_overhead"] = None if net is None else net - volume * 0.25 / 10_000
    result["modeled_overhead_bps_of_gross_turnover"] = 0.25
    return result


def summarize_turnover(row):
    keys = ("maker_fills", "ioc_fills", "closed_cycles", "fees", "maker_fees", "maker_turnover", "taker_turnover",
            "net_pnl", "incomplete", "engine_exit_code", "open_positions_base", "funding_flat",
            "cycle_attribution", "turnover_feedback", "turnover_posture", "entry_blocked", "max_unhedged_ms")
    return {key: row.get(key) for key in keys}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("campaign", "quant_engine", "pair_engine", "base_config", "market_terms", "output"):
        parser.add_argument(name, type=Path)
    parser.add_argument("--public-archive", type=Path)
    parser.add_argument("--compiled-dir", type=Path)
    parser.add_argument("--start-ms", type=int, help="UTC decision start shared with arbitrage research")
    parser.add_argument("--turnover-venue", choices=("arcus", "extended", "hyperliquid", "lighter_robinhood"), default="arcus")
    parser.add_argument("--turnover-fees-bps", nargs=2, metavar=("MAKER", "TAKER"))
    args = parser.parse_args()
    if args.public_archive and not args.compiled_dir:
        parser.error("--public-archive requires --compiled-dir")
    if args.turnover_venue != "arcus" and not args.turnover_fees_bps:
        parser.error("provide explicit fees for the target trading domain")
    if args.turnover_venue == "lighter_robinhood" and not args.public_archive:
        parser.error("Robinhood requires a public archive and its hashed compilation profile")
    os.umask(0o077)
    out, campaign = args.output.resolve(), args.campaign.resolve()
    out.mkdir()
    for name in ("inputs", "configs", "runs"):
        (out / name).mkdir()
    with args.base_config.open("rb") as stream:
        base = tomllib.load(stream)["inventory_neutral_mm"]
    campaign_info = None if args.public_archive else load(campaign / "campaign.json")
    terms = load(args.market_terms)
    engines = {"quant": args.quant_engine.resolve(), "turnover": args.pair_engine.resolve()}
    report = {"schema_version": 1, "sample": "historical_reuse", "ledger_restarts": 0,
              "runner_sha256": digest(Path(__file__)), "engine_sha256": {k: digest(v) for k, v in engines.items()},
              "market_terms_sha256": digest(args.market_terms), "capital_sizes": CAPITAL_SIZES,
              "source_evidence": {}, "quant": {}, "turnover": {}, "execution_qualified": False,
              "qualification_blockers": ["account_fees_unverified", "stream_quality_requires_separate_audit"],
              "points_value_quote": "0", "unconfirmed_rebates_quote": "0", "venue_credited_turnover_quote": None}
    save(out / "policy.json", report | {"frozen_grid": "two assets; two capital/size cases; three turnover postures; 0/4bps loss allowances"})
    for symbol in ("BTC", "ETH"):
        compiled = args.compiled_dir / f"{symbol}-freshest.ndjson.zst" if args.compiled_dir else None
        window = time_window(campaign, symbol, compiled, args.start_ms)
        if args.public_archive:
            # Read the market mapping and terms from this tape's archived
            # compilation profile, rather than an unrelated old campaign.
            capture_config = args.compiled_dir / f"{symbol}-capture.json"
            provenance = verify_compiled(args.public_archive, compiled)
            if provenance["config_sha256"] != digest(capture_config):
                raise ValueError("compiled market terms do not match the archive compilation profile")
            profile = load(capture_config)
            spec = profile["pair"]["legs"][0]
            market = {"id": profile["legs"][0]["lighter_market_id"],
                      "price_decimals": -Decimal(spec["tick"]).normalize().as_tuple().exponent,
                      "size_decimals": -Decimal(spec["lot"]).normalize().as_tuple().exponent,
                      "min_base_amount": spec["min_contracts"], "min_quote_amount": spec["min_notional"]}
            if spec["venue"] != "lighter" or spec["market"] != symbol or any(
                    Decimal(spec[key]).normalize().as_tuple().digits != (1,) for key in ("tick", "lot")):
                raise ValueError("unsupported Lighter market terms")
        else:
            market = campaign_info["selected"]["lighter"][symbol]
        report["source_evidence"][symbol] = {"window": window}
        try:
            tape = out / "inputs" / f"quant-{symbol}.ndjson"
            report["source_evidence"][symbol]["quant"] = quant_input(campaign, symbol, window, tape,
                compiled=compiled, public_archive=args.public_archive, lighter_id=market["id"])
            if not report["source_evidence"][symbol]["quant"]["explicit_feed_states"]:
                raise ValueError("missing_public_trade_feed_lifecycle")
            report["quant"][symbol] = {}
            for capital, size in CAPITAL_SIZES:
                label = f"quant-{symbol}-{capital}capital-{size}size"
                cfg = out / "configs" / f"{label}.json"
                save(cfg, quant_policy(base, market, symbol, window, capital, size))
                row = execute(engines["quant"], cfg, tape, out / "runs" / f"{label}.json")
                report["quant"][symbol][label] = summarize_quant(row) | {"config_sha256": digest(cfg), "ledger_id": label}
                save(out / "report.json", report)
        except Exception as error:
            (out / f"quant-{symbol}.prepare-error").write_text(str(error))
            report["quant"].setdefault(symbol, {})["preparation"] = {"incomplete": "input_preparation_failed", "error_type": type(error).__name__,
                "source_blocker": "missing_public_trade_feed_lifecycle" if str(error) == "missing_public_trade_feed_lifecycle" else "other_preparation_failure"}
            save(out / "report.json", report)
        try:
            route = f"{symbol}-lighter-{args.turnover_venue}"
            period = (window["start_ms"], window["drain_until_ms"], "historical_reuse")
            turnover_book = compiled if args.turnover_venue == "hyperliquid" else (
                args.compiled_dir / f"{route}.ndjson.zst" if args.compiled_dir else None)
            target_market_id = None
            if args.public_archive and turnover_book != compiled:
                lineage = verify_compiled(args.public_archive, turnover_book)
                if args.turnover_venue == "lighter_robinhood":
                    target_config = args.compiled_dir / f"{route}.config.json"
                    target_market_id = robinhood_market_id(target_config, lineage, symbol, market["id"])
            pair, provenance = turnover_input(campaign, symbol, window, out / "inputs" / f"{route}-{period[0]}.ndjson",
                compiled=turnover_book, public_archive=args.public_archive, lighter_id=market["id"],
                target=args.turnover_venue, target_market_id=target_market_id)
            report["source_evidence"][symbol]["turnover"] = provenance
            if not provenance["explicit_feed_states_by_leg"][1]:
                raise ValueError("missing_public_trade_feed_lifecycle")
            if args.turnover_fees_bps:
                fees = tuple(str(Decimal(value)) for value in args.turnover_fees_bps)
            else:
                target = next(t for t in terms["arcus"] if t["baseAsset"] == symbol)
                fees = tuple(str(Decimal(target[key]) / 100) for key in ("effectiveMakerFeePpm", "effectiveTakerFeePpm"))
            report["turnover"][symbol] = {}
            for label, policy in turnover_policies(pair, window, fees):
                label = f"turnover-{symbol}-{label}"
                cfg = out / "configs" / f"{label}.json"
                save(cfg, policy)
                row = execute(engines["turnover"], cfg, out / "inputs" / f"{route}-{period[0]}.ndjson", out / "runs" / f"{label}.json", True)
                report["turnover"][symbol][label] = summarize_turnover(row) | {"config_sha256": digest(cfg), "ledger_id": label}
                save(out / "report.json", report)
        except Exception as error:
            (out / f"turnover-{symbol}.prepare-error").write_text(str(error))
            report["turnover"].setdefault(symbol, {})["preparation"] = {"incomplete": "input_preparation_failed", "error_type": type(error).__name__,
                "source_blocker": "missing_public_trade_feed_lifecycle" if str(error) == "missing_public_trade_feed_lifecycle" else "other_preparation_failure"}
            save(out / "report.json", report)
    print(json.dumps({"report_ready": True, "quant_runs": sum(map(len, report["quant"].values())),
                      "turnover_runs": sum(map(len, report["turnover"].values()))}))


if __name__ == "__main__":
    main()
