#!/usr/bin/env python3
"""Ex-ante spread-to-short-horizon-volatility market screener (`S8-4`).

Ranks candidate markets across venues by:
  spread_to_1s_vol_ratio = median_spread_bps / realized_1s_vol_bps
  net_half_spread_after_maker_fee_bps = 0.5 * median_spread_bps - maker_fee_bps
alongside primary-rule program eligibility from `audit_lighter_program_scope`.
Helps shift market making away from crowded 0-fee BTC/ETH books toward high spread/vol
niches (e.g., Arcus RWA 0-bp maker or long-tail markets).
"""
import argparse
from decimal import Decimal, InvalidOperation
import json
import math
from pathlib import Path
from statistics import median

from audit_lighter_program_scope import multi_venue_program_scope


def _is_nonpositive_fee(raw_fee) -> bool:
    if raw_fee is None:
        return False
    try:
        fee = Decimal(str(raw_fee))
        return fee.is_finite() and fee <= 0
    except (InvalidOperation, ValueError):
        return False


def score_market_book_series(
    samples,
    *,
    venue,
    liquidity_domain,
    symbol,
    asset_class,
    account_tier,
    strategy_posture="directional_or_unhedged",
    growth_mode=False,
    deployer_fee_scale="1",
    max_gap_ms=10_000,
    expected_1s_adverse_selection_bps=None,
):
    """Score chronological `{timestamp_ms, bids: [[px, sz], ...], asks: [[px, sz], ...]}` frames."""
    spreads_bps = []
    l1_notionals = []
    var_per_sec = []
    prev_mid = None
    prev_ts = None

    for frame in samples:
        ts = frame.get("timestamp_ms")
        bids = frame.get("bids") or []
        asks = frame.get("asks") or []
        if (type(ts) not in (int, float) or not math.isfinite(ts) or ts < 0
                or (prev_ts is not None and ts < prev_ts) or not bids or not asks):
            continue
        bid_px, bid_sz = float(bids[0][0]), float(bids[0][1])
        ask_px, ask_sz = float(asks[0][0]), float(asks[0][1])
        if (not all(math.isfinite(x) for x in (bid_px, ask_px, bid_sz, ask_sz))
                or not (0 < bid_px < ask_px and bid_sz > 0 and ask_sz > 0)):
            continue
        mid = bid_px + (ask_px - bid_px) / 2
        l1_notional = min(bid_px * bid_sz, ask_px * ask_sz)
        if not math.isfinite(l1_notional):
            continue
        spreads_bps.append((ask_px - bid_px) / mid * 10_000.0)
        l1_notionals.append(l1_notional)
        if prev_mid is not None and prev_ts is not None and 0 < ts - prev_ts <= max_gap_ms:
            ret_bps = (mid - prev_mid) / prev_mid * 10_000.0
            dt_s = (ts - prev_ts) / 1_000.0
            var_per_sec.append((ret_bps * ret_bps) / dt_s)
        prev_mid = mid
        prev_ts = ts

    scope = multi_venue_program_scope(
        venue=venue,
        liquidity_domain=liquidity_domain,
        asset_class=asset_class,
        account_tier=account_tier,
        maker_turnover="0",
        strategy_posture=strategy_posture,
        growth_mode=growth_mode,
        deployer_fee_scale=deployer_fee_scale,
    )
    maker_fee_bps = float(scope["maker_fee_bps"]) if scope["maker_fee_bps"] is not None else math.inf
    if not spreads_bps or not var_per_sec:
        return {
            "capability_snapshot_version": scope["capability_snapshot_version"],
            "venue": venue,
            "symbol": symbol,
            "asset_class": asset_class,
            "account_tier": account_tier,
            "samples": len(spreads_bps),
            "vol_intervals": len(var_per_sec),
            "median_spread_bps": None,
            "realized_1s_vol_bps": None,
            "spread_to_1s_vol_ratio": None,
            "maker_fee_bps": scope["maker_fee_bps"],
            "net_half_spread_after_maker_fee_bps": None,
            "adverse_selection_margin_ok": None,
            "median_min_l1_notional_usd": None,
            "program_status": scope["program_status"],
            "disqualification_risks": scope["disqualification_risks"],
        }

    med_spread = median(spreads_bps)
    vol_1s = math.sqrt(sum(var_per_sec) / len(var_per_sec))
    ratio = med_spread / vol_1s if vol_1s > 1e-12 else None
    net_half_spread = 0.5 * med_spread - maker_fee_bps if math.isfinite(maker_fee_bps) else None
    margin_ok = None
    if expected_1s_adverse_selection_bps is not None and net_half_spread is not None:
        adv = float(expected_1s_adverse_selection_bps)
        if not math.isfinite(adv) or adv < 0:
            raise ValueError("expected_1s_adverse_selection_bps must be finite and nonnegative")
        margin_ok = net_half_spread >= 2.0 * adv
    return {
        "capability_snapshot_version": scope["capability_snapshot_version"],
        "venue": venue,
        "symbol": symbol,
        "asset_class": asset_class,
        "account_tier": account_tier,
        "samples": len(spreads_bps),
        "vol_intervals": len(var_per_sec),
        "median_spread_bps": round(med_spread, 6),
        "realized_1s_vol_bps": round(vol_1s, 6),
        "spread_to_1s_vol_ratio": None if ratio is None else round(ratio, 6),
        "maker_fee_bps": scope["maker_fee_bps"],
        "net_half_spread_after_maker_fee_bps": (
            None if net_half_spread is None else round(net_half_spread, 6)
        ),
        "adverse_selection_margin_ok": margin_ok,
        "median_min_l1_notional_usd": round(median(l1_notionals), 4),
        "program_status": scope["program_status"],
        "disqualification_risks": scope["disqualification_risks"],
    }


def rank_markets(metrics):
    """Rank screened markets placing 0-fee positive net half-spread & highest spread/vol first."""
    return sorted(
        metrics,
        key=lambda m: (
            m["spread_to_1s_vol_ratio"] is not None,
            m.get("adverse_selection_margin_ok") is not False,
            m["net_half_spread_after_maker_fee_bps"] is not None
            and m["net_half_spread_after_maker_fee_bps"] > 0,
            _is_nonpositive_fee(m["maker_fee_bps"]),
            m["spread_to_1s_vol_ratio"] or -1.0,
            m["net_half_spread_after_maker_fee_bps"] or -1e9,
        ),
        reverse=True,
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("manifest", type=Path, help="JSON list of market tape specs")
    args = parser.parse_args()
    specs = json.loads(args.manifest.read_text())
    results = []
    for spec in specs:
        samples = [json.loads(line) for line in Path(spec["jsonl"]).read_text().splitlines() if line.strip()]
        results.append(
            score_market_book_series(
                samples,
                venue=spec["venue"],
                liquidity_domain=spec["liquidity_domain"],
                symbol=spec["symbol"],
                asset_class=spec["asset_class"],
                account_tier=spec["account_tier"],
                growth_mode=spec.get("growth_mode", False),
                deployer_fee_scale=spec.get("deployer_fee_scale", "1"),
                expected_1s_adverse_selection_bps=spec.get("expected_1s_adverse_selection_bps"),
            )
        )
    print(json.dumps(rank_markets(results), indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
