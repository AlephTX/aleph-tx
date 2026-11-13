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

from replay_maker_campaign import books, load, save, strategy_config, unseal
from replay_maker_first_campaign import digest

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


def time_window(campaign, symbol):
    with unseal(campaign / "results/final" / f"{symbol}-freshest.ndjson.zst") as stream:
        next(stream)
        first = json.loads(next(stream))["now_ms"]
        # Read to EOF to verify the compressed archive, even during discovery.
        last = first
        for line in stream:
            last = json.loads(line)["now_ms"]
    begin = first + 600_000
    end = begin + TEN_HOURS
    if last < end + 15_000:
        raise ValueError("source lacks ten decision hours and terminal drain")
    return {"warmup_start_ms": begin - 60_000, "start_ms": begin,
            "end_ms": end, "drain_until_ms": end + 15_000}


def quant_input(campaign, symbol, window, output):
    files = sorted((campaign / "raw").glob("lighter-trades-*.jsonl.gz"))
    if not files:
        raise ValueError("missing Lighter public trade files")
    first, stop = window["warmup_start_ms"], window["drain_until_ms"]
    timeline = merge(books(campaign, symbol), strict_trade_rows(files, symbol, 0, stop),
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
            "compiled_sha256": digest(campaign / "results/final" / f"{symbol}-freshest.ndjson.zst"),
            "public_trade_files_sha256": {path.name: digest(path) for path in files},
            "input_sha256": digest(output)}



def turnover_input(campaign, symbol, window, output):
    start, stop = window["start_ms"], window["drain_until_ms"]
    route = f"{symbol}-lighter-arcus"
    source = campaign / "results/final" / f"{route}.ndjson.zst"
    spans = [[None, None], [None, None]]
    book_span = [None, None]
    info, counts = {}, {"book": 0, "trade": 0, "trade_feed": 0}
    explicit_feed_states = [0, 0]
    with unseal(source) as stream:
        pair = json.loads(next(stream))["pair"]
        if [leg["venue"] for leg in pair["legs"]] != ["lighter", "arcus"]:
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
            paths = sorted((campaign / "raw").glob(f'{terms["venue"]}-trades-*.jsonl.gz'))
            if not paths:
                raise ValueError("missing turnover public trade source")
            info[terms["venue"]] = {p.name: digest(p) for p in paths}
            for event in strict_trade_rows(paths, terms["market"], leg, 2**63):
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
    if [leg["venue"] for leg in pair["legs"]] != ["lighter", "arcus"]:
        raise ValueError("expected Core Lighter hedge and Arcus target domains")
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
                    "transport_ms": 250, "fill_notice_ms": 100, "maker_cancel_delay_ms": 0,
                    "taker_delay_ms": [300, 0], "trade_clock_guard_ms": 100,
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
    args = parser.parse_args()
    os.umask(0o077)
    out, campaign = args.output.resolve(), args.campaign.resolve()
    out.mkdir()
    for name in ("inputs", "configs", "runs"):
        (out / name).mkdir()
    with args.base_config.open("rb") as stream:
        base = tomllib.load(stream)["inventory_neutral_mm"]
    campaign_info, terms = load(campaign / "campaign.json"), load(args.market_terms)
    engines = {"quant": args.quant_engine.resolve(), "turnover": args.pair_engine.resolve()}
    report = {"schema_version": 1, "sample": "historical_reuse", "ledger_restarts": 0,
              "runner_sha256": digest(Path(__file__)), "engine_sha256": {k: digest(v) for k, v in engines.items()},
              "market_terms_sha256": digest(args.market_terms), "capital_sizes": CAPITAL_SIZES,
              "source_evidence": {}, "quant": {}, "turnover": {}, "execution_qualified": False,
              "qualification_blockers": ["account_fees_unverified", "stream_quality_requires_separate_audit"],
              "points_value_quote": "0", "unconfirmed_rebates_quote": "0", "venue_credited_turnover_quote": None}
    save(out / "policy.json", report | {"frozen_grid": "two assets; two capital/size cases; three turnover postures; 0/4bps loss allowances"})
    for symbol in ("BTC", "ETH"):
        window = time_window(campaign, symbol)
        report["source_evidence"][symbol] = {"window": window}
        try:
            tape = out / "inputs" / f"quant-{symbol}.ndjson"
            report["source_evidence"][symbol]["quant"] = quant_input(campaign, symbol, window, tape)
            if not report["source_evidence"][symbol]["quant"]["explicit_feed_states"]:
                raise ValueError("missing_public_trade_feed_lifecycle")
            market = campaign_info["selected"]["lighter"][symbol]
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
            route = f"{symbol}-lighter-arcus"
            period = (window["start_ms"], window["drain_until_ms"], "historical_reuse")
            pair, provenance = turnover_input(campaign, symbol, window, out / "inputs" / f"{route}-{period[0]}.ndjson")
            report["source_evidence"][symbol]["turnover"] = provenance
            if not provenance["explicit_feed_states_by_leg"][1]:
                raise ValueError("missing_public_trade_feed_lifecycle")
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
