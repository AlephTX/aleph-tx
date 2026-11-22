#!/usr/bin/env python3
"""Exact accounting diagnostics, not an execution or profitability simulator.

Inputs must be public research aggregates or synthetic fixtures. Unsettled
wallets remain unknown. Fee removal holds every original decision fixed.
Inventory netting assumes supplied fills and instantaneous continuous hedges;
it says nothing about achieving those fills, hedge prices, or adverse selection.
"""

import argparse
import json
from decimal import Decimal
from pathlib import Path

D = Decimal
ZERO = D(0)
BPS = D(10000)
MILLION = D(1000000)


def wallet_frontier(wallet):
    """Only fully settled ledgers have a gross/net fee break-even frontier."""
    terminal = wallet["terminal_pnl_usd"]
    volumes = {k: D(v) for k, v in wallet["known_turnover_usd"].items()}
    fees = {k: D(v) for k, v in wallet["venue_known_fees"].items()}
    result = {"status": "unresolved", "turnover": volumes, "known_fees": fees,
              "net": None, "gross_before_fees": None,
              "break_even_fee_bps_all_turnover": None}
    if terminal is None:
        return result
    if any(D(v) != 0 for bounds in wallet["position_bounds"].values() for v in bounds):
        raise ValueError("terminal cash with nonflat position bounds")
    volume = sum(volumes.values(), ZERO)
    net = D(terminal)
    gross = net + sum(fees.values(), ZERO)
    result.update(status="settled" if volume else "idle", net=net,
                  gross_before_fees=gross,
                  break_even_fee_bps_all_turnover=gross / volume * BPS if volume else None)
    return result


def fee_routes(arcus_maker_bps="0", arcus_taker_bps="1.12", xyz_taker_bps=".9"):
    """Equal per-leg notional; all four legs counted for cross-venue cycles."""
    am, at, xt = map(D, (arcus_maker_bps, arcus_taker_bps, xyz_taker_bps))
    routes = [("arcus_MM", 2 * am, 2), ("arcus_MT", am + at, 2),
              ("arcus_TT", 2 * at, 2), ("arcus_MM_xyz_TT", 2 * am + 2 * xt, 4),
              ("arcus_MT_xyz_TT", am + at + 2 * xt, 4),
              ("arcus_TT_xyz_TT", 2 * at + 2 * xt, 4)]
    return {name: {"cycle_bps_one_leg": bps, "usd_per_million_all_venues": bps * 100 / legs,
                   "usd_per_million_arcus": bps * 100 / 2}
            for name, bps, legs in routes}


def minimum_hedge_quantity(deltas, band):
    """Minimum L1 hedge quantity with fixed inventory band and terminal flat.

    Projection onto the allowed interval minimizes total variation: delaying a
    hedge within the interval cannot increase the later minimum movement, by
    the triangle inequality. This is a fill-path conditional quantity bound,
    allowing instantaneous hedges and no lot/minimum-size constraints.
    """
    band = D(band)
    if not band.is_finite() or band < 0:
        raise ValueError("band must be finite and nonnegative")
    inventory = hedged = total = unhedged = peak = ZERO
    for delta in deltas:
        delta = D(delta)
        if not delta.is_finite():
            raise ValueError("nonfinite fill")
        total += abs(delta)
        unhedged += delta
        peak = max(peak, abs(unhedged))
        wanted = inventory + delta
        inventory = min(band, max(-band, wanted))
        hedged += abs(wanted - inventory)
    drain = abs(inventory)
    return {"maker_absolute_quantity_after_batch_netting": total,
            "net_maker_quantity": unhedged, "unhedged_peak_quantity": peak,
            "hedge_quantity_before_drain": hedged, "terminal_flatten_quantity": drain,
            "total_hedge_quantity": hedged + drain}


def max_hedge_ratio(cost_per_million, overhead_bps="0", maker_bps="0", hedge_bps=".9"):
    """No spread capture/alpha credit; negative headroom stays infeasible."""
    c, overhead, maker, hedge = map(D, (cost_per_million, overhead_bps, maker_bps, hedge_bps))
    if hedge <= 0:
        raise ValueError("positive external hedge fee required")
    remaining = c / 100 - overhead - maker
    return None if remaining < 0 else remaining / hedge


def paired_taker_cash(arcus_entry, xyz_entry, arcus_exit, xyz_exit, long_arcus,
                      arcus_fee=".000112", xyz_fee=".00009"):
    """Four positive *notionals*, same quantity, flat two-venue cycle."""
    notionals = list(map(D, (arcus_entry, xyz_entry, arcus_exit, xyz_exit)))
    if any(not v.is_finite() or v <= 0 for v in notionals):
        raise ValueError("positive finite leg notionals required")
    ae, xe, ax, xx = notionals
    gross = (ax-ae + xe-xx) * (1 if long_arcus else -1)
    fees = (ae+ax)*D(arcus_fee) + (xe+xx)*D(xyz_fee)
    return {"gross": gross, "fees": fees, "net": gross-fees}


def funding_overlap(arcus, xyz, start_ms, end_ms):
    """Join actual applied hourly rates in (start,end]; no forward filling.

    Equal constant USD notional rate-spread diagnostic; actual base-quantity
    funding cash also needs each venue's oracle and position at every tick.
    """
    hour = 3600000
    if start_ms % hour or end_ms % hour or end_ms <= start_ms:
        raise ValueError("whole-hour nonempty interval required")
    offsets = {"arcus": [], "xyz": []}
    def index(rows, arc):
        out = {}
        for row in rows:
            raw = row["time"]
            if isinstance(raw, bool) or not isinstance(raw, int) or (arc and raw % 1000):
                raise ValueError("invalid source time")
            source_time = raw // 1000 if arc else raw
            # HL publishes an hourly tick milliseconds after the boundary.
            # Preserve that offset; never join an arbitrary nearby timestamp.
            offset = source_time % hour
            if offset > (0 if arc else 1000):
                raise ValueError("off-hour funding tick")
            t = source_time - offset
            if not start_ms < t <= end_ms:
                continue
            offsets["arcus" if arc else "xyz"].append(offset)
            if arc and row["marketId"] != 88:
                raise ValueError("wrong Arcus market")
            if not arc and row["coin"] != "xyz:ANT":
                raise ValueError("wrong xyz market")
            rate = D(row["fundingRate"])
            if not rate.is_finite():
                raise ValueError("nonfinite rate")
            if t in out and out[t] != rate:
                raise ValueError("conflicting hourly funding rate")
            out[t] = rate
        return out
    a, x = index(arcus, True), index(xyz, False)
    expected = list(range(start_ms + hour, end_ms + 1, hour))
    matched = [t for t in expected if t in a and t in x]
    diffs = [a[t] - x[t] for t in matched]
    full = len(matched) == len(expected)
    return {"expected_hours": len(expected), "matched_hours": len(matched),
            "missing_arcus_hours": [t for t in expected if t not in a],
            "missing_xyz_hours": [t for t in expected if t not in x],
            "complete": full, "source_offsets_ms": offsets,
            "short_arcus_long_xyz_positive_hours": sum(d > 0 for d in diffs),
            "short_arcus_long_xyz_negative_hours": sum(d < 0 for d in diffs),
            "observed_spread_sum_bps": sum(diffs, ZERO) * BPS,
            "full_interval_spread_bps": sum(diffs, ZERO) * BPS if full else None,
            "min_hour_spread_bps": min(diffs) * BPS if diffs else None,
            "max_hour_spread_bps": max(diffs) * BPS if diffs else None,
            "hours": [{"time_ms": t, "arcus": a[t], "xyz": x[t], "spread_bps": (a[t]-x[t])*BPS}
                      for t in matched]}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("wallets", type=Path, help="Public aggregate JSON with windows/modes/wallets")
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    source = json.loads(args.wallets.read_text())
    out = {"classification": "fixed-ledger accounting diagnostic; no new execution simulation",
           "fee_routes": fee_routes(), "wallets": {}}
    for window, modes in source["wallets"].items():
        out["wallets"][window] = {mode: {name: wallet_frontier(w) for name, w in wallets.items()}
                                  for mode, wallets in modes.items()}
    out["hedge_ratio_caps"] = {overhead: {cap: max_hedge_ratio(cap, overhead)
                                          for cap in ("2", "20", "50")}
                                 for overhead in ("0", ".25")}
    out["quant_fee_repricing"] = {}
    for window, modes in out["wallets"].items():
        out["quant_fee_repricing"][window] = {}
        for mode, wallets in modes.items():
            quant = wallets.get("quant")
            if quant is None:
                continue
            if quant["turnover"].get("xyz", ZERO) != 0:
                raise ValueError("quant fee repricing requires Arcus-only taker ledger")
            gross, turnover = quant["gross_before_fees"], sum(quant["turnover"].values(), ZERO)
            out["quant_fee_repricing"][window][mode] = {
                rate: None if gross is None else gross-turnover*D(rate)/BPS
                for rate in ("1.12", ".95", ".80", ".67", ".57", ".50", ".47")}
    out["fee_repricing_limit"] = "Accounting only: original decisions, stops and fills held fixed; no fee-tier eligibility claim."
    args.output.write_text(json.dumps(out, default=str, indent=2) + "\n")


if __name__ == "__main__":
    main()
