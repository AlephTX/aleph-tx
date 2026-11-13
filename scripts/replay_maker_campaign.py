#!/usr/bin/env python3
"""Replay the production Lighter quoting loop on an Oasis public-data campaign."""

import argparse
from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager
import gzip
import hashlib
from heapq import merge
import json
import math
import os
from pathlib import Path
import subprocess
import tomllib

HOUR = 3_600_000
DAY = 24 * HOUR
RESPONSE_MS = 100
SCENARIOS = {
    "premium_baseline": (0.38, 4.0, {}),
    "premium_markout": (0.38, 4.0, {"use_markout_spread": True}),
    "premium_inventory": (0.38, 4.0, {
        "use_markout_spread": True,
        "min_profit_bps": 2.0,
        "max_position_notional_usd": 100.0,
        "inventory_urgency_notional_usd": 45.0,
        "position_timeout_secs": 30,
    }),
    "premium_patient_inventory": (0.38, 4.0, {
        "use_markout_spread": True,
        "min_profit_bps": 2.0,
        "max_position_notional_usd": 100.0,
        "inventory_urgency_notional_usd": 45.0,
        "position_timeout_secs": 120,
    }),
    "standard_baseline": (0.0, 0.0, {}),
    "standard_turnover": (0.0, 0.0, {
        "use_markout_spread": True,
        "min_profit_bps": 0.5,
        "max_position_notional_usd": 100.0,
        "inventory_urgency_notional_usd": 45.0,
        "position_timeout_secs": 30,
    }),
}
FEED_LIMITS = {"BTC": 1001, "ETH": 1002}
LATENCY = (250, 500)


def load(path):
    return json.loads(path.read_text())


def save(path, value):
    path.write_text(json.dumps(value, indent=2, allow_nan=False) + "\n")


@contextmanager
def unseal(path):
    with subprocess.Popen(["zstd", "-qdc", str(path)], stdout=subprocess.PIPE) as process:
        try:
            yield process.stdout
        finally:
            process.stdout.close()
            code = process.wait()
        if code:
            raise RuntimeError(f"zstd returned {code} for {path.name}")


def windows(start, split, stop):
    boundaries = sorted({start, split, stop, *range((start // HOUR + 1) * HOUR, stop, HOUR)})
    result = []
    for left, right in zip(boundaries, boundaries[1:]):
        drain = min(right, (left // HOUR + 1) * HOUR - 1)
        if drain - left <= 15_000:
            continue
        result.append({
            "start_ms": left,
            "end_ms": drain - 10_000,
            "drain_until_ms": drain,
            "phase": "train" if left < split else "holdout",
        })
    return result


def compile_book(book):
    if not book.get("tradable") or not book["timestamp_ms"]:
        return {"timestamp_ms": 0, "bids": [], "asks": []}
    depth = {side: [[float(level["price"]), float(level["contracts"])] for level in book[side]]
             for side in ("bids", "asks")}
    if any(not side for side in depth.values()):
        return {"timestamp_ms": 0, "bids": [], "asks": []}
    return {"timestamp_ms": book["timestamp_ms"], **depth}


def books(campaign, symbol):
    file = campaign / "results" / "final" / f"{symbol}-freshest.ndjson.zst"
    with unseal(file) as stream:
        header = json.loads(stream.readline())
        assert [leg["venue"] for leg in header["pair"]["legs"]] == ["lighter", "hyperliquid"]
        for line in stream:
            frame = json.loads(line)
            left, right = (compile_book(leg["book"]) for leg in frame["legs"])
            yield {"kind": "book", "at_ms": frame["now_ms"], "lighter": left, "hyperliquid": right}


def trades(campaign, symbol):
    seen = set()
    previous = 0
    for path in sorted((campaign / "raw").glob("lighter-trades-*.jsonl.gz")):
        with gzip.open(path, "rb") as stream:
            for line in stream:
                trade = json.loads(line)
                if trade.get("kind") != "trade" or trade["market"] != symbol or trade.get("side") not in ("buy", "sell"):
                    continue
                identifier = trade.get("trade_id")
                if identifier is None or identifier in seen:
                    continue
                at = trade["received_at_ms"]
                if at < previous:
                    raise ValueError(f"public trades regress in {path.name}")
                previous = at
                seen.add(identifier)
                yield {"kind": "trade", "at_ms": at, "source_ms": trade["source_time_ms"],
                       "price": float(trade["price"]), "size": float(trade["size"]), "aggressor": trade["side"]}


def write_inputs(campaign, symbol, periods, out):
    timeline = merge(books(campaign, symbol), trades(campaign, symbol),
                     key=lambda event: (event["at_ms"], event["kind"] != "trade"))
    event = next(timeline, None)
    for period in periods:
        start, drain = period["start_ms"], period["drain_until_ms"]
        path = out / f"{symbol}-{start}.ndjson"
        with path.open("x") as file:
            while event is not None and event["at_ms"] < drain:
                if start <= event["at_ms"]:
                    file.write(json.dumps(event, separators=(",", ":"), allow_nan=False) + "\n")
                event = next(timeline, None)
    # Finish the decoder so corruption in the last compressed block is visible.
    for _ in timeline:
        pass


def strategy_config(base, market, symbol, capital, maker, overrides):
    return base | {
        "symbol_id": FEED_LIMITS[symbol], "market_id": market["id"],
        "tick_size": 10.0 ** -market["price_decimals"], "step_size": 10.0 ** -market["size_decimals"],
        "base_order_notional_usd": 50.0, "max_position_notional_usd": 150.0,
        "inventory_urgency_notional_usd": 75.0, "use_depth_pricing": False,
        "reference_portfolio_value": capital, "min_portfolio_scale": 1.0, "max_portfolio_scale": 1.0,
        "target_resting_margin_utilization": 0.02, "max_leverage": 2.0,
        "local_staleness_ms": 2200, "external_staleness_ms": 2200,
        "maker_fee_bps": maker, "use_post_only": True,
    } | overrides


def run_case(engine, out, base, market, capital, item):
    symbol, period, name, delay = item
    maker, taker, change = SCENARIOS[name]
    label = f"{symbol}-{period['start_ms']}-{name}-{delay}ms"
    config = out / "configs" / f"{label}.json"
    result = out / "runs" / f"{label}.json"
    options = {
        "strategy": strategy_config(base, market, symbol, capital, maker, change),
        **{k: period[k] for k in ("start_ms", "end_ms", "drain_until_ms")},
        "initial_equity": capital, "order_entry_ms": delay, "order_response_ms": RESPONSE_MS,
        "trade_clock_guard_ms": 100, "taker_fee_bps": taker, "depth_participation": 0.25,
        "min_contracts": float(market["min_base_amount"]), "min_notional": float(market["min_quote_amount"]),
    }
    save(config, options)
    inp = out / "inputs" / f"{symbol}-{period['start_ms']}.ndjson"
    with result.open("x") as result_file, (out / "runs" / f"{label}.stderr").open("x") as errors:
        code = subprocess.run([str(engine), str(config), str(inp)], stdout=result_file, stderr=errors,
                              timeout=180, check=False).returncode
    row = load(result) if not code else {"error": f"maker_replay_exit_{code}"}
    return symbol, period["phase"], name, delay, row


def aggregate(rows, capital):
    valid = [r for r in rows if r.get("net_liquidation_pnl") is not None and r["book_frames"] > 0]
    pnl = sum(r["net_liquidation_pnl"] for r in valid) if len(valid) == len(rows) and rows else None
    duration = sum(r["drain_until_ms"] - r["start_ms"] for r in rows)
    maker_volume = sum(r["maker_turnover"] for r in valid)
    taker_volume = sum(r["taker_turnover"] for r in valid)
    turnover = maker_volume + taker_volume
    maker_fee = sum(r["maker_fees"] for r in valid)
    taker_fee = sum(r["taker_fees"] for r in valid)
    maker_fills = sum(r["maker_fills"] for r in valid)
    taker_fills = sum(r["taker_fills"] for r in valid)
    hours = duration / HOUR
    mean_inventory = None
    if pnl is not None and duration and all(r.get("mean_inventory_notional") is not None for r in valid):
        mean_inventory = sum(
            r["mean_inventory_notional"] * (r["drain_until_ms"] - r["start_ms"]) for r in valid
        ) / duration
    daily = None if pnl is None or not duration else pnl / capital * DAY / duration * 100
    return {
        "windows": len(rows), "closed_windows": len(valid), "halted_windows": sum(bool(r.get("halted")) for r in rows),
        "errors": sorted({r["error"] for r in rows if "error" in r}),
        "hours": hours, "maker_fills": maker_fills, "taker_fills": taker_fills, "turnover": turnover,
        "fills_per_hour": (maker_fills + taker_fills) / hours if hours and pnl is not None else None,
        "accepted_per_hour": (
            sum(r["accepted_orders"] for r in valid) / hours
            if hours and pnl is not None and all("accepted_orders" in r for r in valid)
            else None
        ),
        "mean_inventory_notional": mean_inventory,
        "maker_fees": maker_fee, "taker_fees": taker_fee, "pnl": pnl,
        "pnl_before_fees": None if pnl is None else pnl + maker_fee + taker_fee,
        "break_even_maker_fee_bps": None if pnl is None or not maker_volume else (pnl + maker_fee) * 1e4 / maker_volume,
        "net_cost_bps": None if pnl is None or not turnover else -pnl * 1e4 / turnover,
        "linear_daily_pct": daily, "linear_annual_pct": None if daily is None else daily * 365,
        "max_drawdown": max((r["max_drawdown"] for r in valid), default=None),
        "post_only_rejections": sum(r["post_only_rejections"] for r in valid),
        "precision_rejections": sum(r["precision_rejections"] for r in valid),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("campaign", type=Path)
    parser.add_argument("engine", type=Path)
    parser.add_argument("base_config", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--capital", type=float, default=1000.0)
    parser.add_argument("--revision", required=True)
    parser.add_argument("--workers", type=int, default=2, choices=(1, 2))
    args = parser.parse_args()
    if not math.isfinite(args.capital) or args.capital < 500:
        parser.error("capital must be finite and at least 500 quote units")
    os.umask(0o077)
    root = args.campaign.resolve()
    out = args.output.resolve()
    out.mkdir(mode=0o700)
    for name in ("inputs", "configs", "runs"):
        (out / name).mkdir()
    with args.base_config.open("rb") as file:
        base = tomllib.load(file)["inventory_neutral_mm"]
    campaign = load(root / "campaign.json")
    original = load(root / "results" / "final" / "report.json")
    periods = windows(original["capture_from_ms"], original["crypto_cut_ms"], original["capture_through_ms"])
    save(out / "policy.json", {"revision": args.revision, "capital": args.capital,
                              "capture_commit": campaign["commit"], "cases": SCENARIOS,
                              "periods": periods, "latency_ms": LATENCY, "response_ms": RESPONSE_MS})
    for symbol in FEED_LIMITS:
        write_inputs(root, symbol, periods, out / "inputs")
    cases = [(symbol, period, name, delay) for symbol in FEED_LIMITS for period in periods
             for name in SCENARIOS for delay in LATENCY]
    results = {}
    with ThreadPoolExecutor(max_workers=args.workers) as workers:
        jobs = [workers.submit(run_case, args.engine.resolve(), out, base, campaign["selected"]["lighter"][case[0]],
                               args.capital, case) for case in cases]
        for case, job in zip(cases, jobs, strict=True):
            symbol, phase, name, delay, row = job.result()
            period = case[1]
            row |= {key: period[key] for key in ("start_ms", "drain_until_ms")}
            results.setdefault((symbol, phase, name, delay), []).append(row)
    summary = {s: {} for s in FEED_LIMITS}
    for (symbol, phase, name, delay), rows in results.items():
        summary[symbol].setdefault(phase, {})[f"{name}-{delay}ms"] = aggregate(rows, args.capital)
    selected = {}
    for symbol in FEED_LIMITS:
        train = summary[symbol]["train"]
        scores = []
        for name in SCENARIOS:
            latency_cases = [train[f"{name}-{d}ms"] for d in LATENCY]
            if all(r["pnl"] is not None and r["pnl"] > 0 and not r["halted_windows"] and r["maker_fills"] >= 12
                   for r in latency_cases):
                scores.append((min(r["pnl"] for r in latency_cases), name))
        selected[symbol] = max(scores)[1] if scores else None
    engine = args.engine.resolve()
    with engine.open("rb") as file:
        binary_sha = hashlib.file_digest(file, "sha256").hexdigest()
    save(out / "report.json", {"revision": args.revision, "capital": args.capital,
                              "maker_binary_sha256": binary_sha, "training_selected": selected, "cases": summary})
    print(json.dumps({"report_ready": True, "cases": len(cases), "training_selected": selected}))


if __name__ == "__main__":
    main()
