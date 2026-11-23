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
import numbers
from pathlib import Path
from statistics import median

try:
    from audit_lighter_program_scope import multi_venue_program_scope
except ImportError:
    from scripts.audit_lighter_program_scope import multi_venue_program_scope


def _validate_finite_real(value, name: str) -> float:
    if isinstance(value, bool) or not isinstance(value, (numbers.Real, Decimal)):
        raise ValueError(f"{name} must be a real finite number, got {type(value).__name__}")
    try:
        numeric = float(value)
    except (TypeError, ValueError, OverflowError) as exc:
        raise ValueError(f"{name} must be convertible to finite float") from exc
    if not math.isfinite(numeric):
        raise ValueError(f"{name} must be finite")
    return numeric


def evaluate_lighter_premium_upgrade(
    *,
    median_spread_bps: float,
    realized_1s_vol_bps: float,
    informed_flow_prob: float = 0.35,
    informed_sigma_mult: float = 2.5,
    non_latency_as_bps: float = 0.20,
    local_book_ms: float = 2.0,
    standard_cancel_ms: float = 300.0,
    premium_maker_fee_bps: float = 0.40,
) -> dict:
    """Evaluate the BARBELL §2.1 & §6.3 dual mathematical gate for Lighter Premium (0 LIT)."""
    spread = _validate_finite_real(median_spread_bps, "median_spread_bps")
    vol_1s = _validate_finite_real(realized_1s_vol_bps, "realized_1s_vol_bps")
    flow_prob = _validate_finite_real(informed_flow_prob, "informed_flow_prob")
    sigma_mult = _validate_finite_real(informed_sigma_mult, "informed_sigma_mult")
    non_latency_as = _validate_finite_real(non_latency_as_bps, "non_latency_as_bps")
    book_ms = _validate_finite_real(local_book_ms, "local_book_ms")
    cancel_ms = _validate_finite_real(standard_cancel_ms, "standard_cancel_ms")
    premium_fee = _validate_finite_real(premium_maker_fee_bps, "premium_maker_fee_bps")

    if spread <= 0.0:
        raise ValueError("median_spread_bps must be > 0")
    if vol_1s < 0.0:
        raise ValueError("realized_1s_vol_bps must be >= 0")
    if not (0.0 <= flow_prob <= 1.0):
        raise ValueError("informed_flow_prob must be in [0, 1]")
    if sigma_mult < 0.0:
        raise ValueError("informed_sigma_mult must be >= 0")
    if non_latency_as < 0.0:
        raise ValueError("non_latency_as_bps must be >= 0")
    if book_ms < 0.0:
        raise ValueError("local_book_ms must be >= 0")
    if cancel_ms < 0.0:
        raise ValueError("standard_cancel_ms must be >= 0")
    if premium_fee < 0.0:
        raise ValueError("premium_maker_fee_bps must be >= 0")

    half_spread_bps = 0.5 * spread
    standard_delta_t_ms = book_ms + cancel_ms
    premium_delta_t_ms = book_ms
    as_factor = flow_prob * sigma_mult * vol_1s
    standard_latency_as_bps = as_factor * math.sqrt(standard_delta_t_ms / 1000.0)
    premium_latency_as_bps = as_factor * math.sqrt(premium_delta_t_ms / 1000.0)
    delta_as_300ms_to_0ms_bps = standard_latency_as_bps - premium_latency_as_bps
    standard_net_edge_bps = half_spread_bps - non_latency_as - standard_latency_as_bps
    premium_net_edge_bps = half_spread_bps - premium_fee - non_latency_as - premium_latency_as_bps

    for computed_name, computed_val in (
        ("standard_latency_as_bps", standard_latency_as_bps),
        ("premium_latency_as_bps", premium_latency_as_bps),
        ("delta_as_300ms_to_0ms_bps", delta_as_300ms_to_0ms_bps),
        ("standard_net_edge_bps", standard_net_edge_bps),
        ("premium_net_edge_bps", premium_net_edge_bps),
    ):
        if not math.isfinite(computed_val):
            raise ValueError(f"{computed_name} overflowed to non-finite float")

    delta_as_exceeds_fee = delta_as_300ms_to_0ms_bps > premium_fee
    premium_positive_net_edge = premium_net_edge_bps > 0.0
    upgrade_admitted = delta_as_exceeds_fee and premium_positive_net_edge

    return {
        "median_spread_bps": round(spread, 6),
        "half_spread_bps": round(half_spread_bps, 6),
        "realized_1s_vol_bps": round(vol_1s, 6),
        "informed_flow_prob": round(flow_prob, 6),
        "informed_sigma_mult": round(sigma_mult, 6),
        "non_latency_as_bps": round(non_latency_as, 6),
        "local_book_ms": round(book_ms, 6),
        "standard_cancel_ms": round(cancel_ms, 6),
        "premium_maker_fee_bps": round(premium_fee, 6),
        "standard_delta_t_ms": round(standard_delta_t_ms, 6),
        "premium_delta_t_ms": round(premium_delta_t_ms, 6),
        "standard_latency_as_bps": round(standard_latency_as_bps, 6),
        "premium_latency_as_bps": round(premium_latency_as_bps, 6),
        "delta_as_300ms_to_0ms_bps": round(delta_as_300ms_to_0ms_bps, 6),
        "standard_net_edge_bps": round(standard_net_edge_bps, 6),
        "premium_net_edge_bps": round(premium_net_edge_bps, 6),
        "delta_as_exceeds_fee": delta_as_exceeds_fee,
        "premium_positive_net_edge": premium_positive_net_edge,
        "upgrade_admitted": upgrade_admitted,
        "operational_guardrails": {
            "lit_stake_required": 0,
            "subaccounts_irreversible": True,
            "downgrade_cooldown_hours": 24,
        },
    }


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
    abs_scaled_rets = []
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
            abs_scaled_rets.append(abs(ret_bps) / math.sqrt(dt_s))
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
            "estimated_1s_abs_drift_bps": None,
            "lighter_premium_gate": None,
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
        "estimated_1s_abs_drift_bps": round(median(abs_scaled_rets), 6) if abs_scaled_rets else None,
        "lighter_premium_gate": evaluate_lighter_premium_upgrade(
            median_spread_bps=med_spread,
            realized_1s_vol_bps=vol_1s,
        ),
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


def _run_self_test() -> dict:
    wide_s1 = evaluate_lighter_premium_upgrade(
        median_spread_bps=2.30,
        realized_1s_vol_bps=1.336,
    )
    if not wide_s1["upgrade_admitted"]:
        raise AssertionError("BARBELL §2.1 wide S1 benchmark failed admission")
    narrow_core = evaluate_lighter_premium_upgrade(
        median_spread_bps=0.30,
        realized_1s_vol_bps=1.336,
    )
    if narrow_core["upgrade_admitted"]:
        raise AssertionError("Narrow Core benchmark unexpectedly admitted")

    mid0 = 100.0
    half0 = mid0 * (1.15 / 10_000.0)
    mid1 = mid0 * (1.0 + 1.336 / 10_000.0)
    half1 = mid1 * (1.15 / 10_000.0)
    synthetic_samples = [
        {"timestamp_ms": 1000, "bids": [[mid0 - half0, 50.0]], "asks": [[mid0 + half0, 50.0]]},
        {"timestamp_ms": 2000, "bids": [[mid1 - half1, 50.0]], "asks": [[mid1 + half1, 50.0]]},
    ]
    screened = score_market_book_series(
        synthetic_samples,
        venue="lighter",
        liquidity_domain="lighter",
        symbol="S1_WIDE",
        asset_class="crypto",
        account_tier="standard",
    )
    if screened["estimated_1s_abs_drift_bps"] is None or screened["lighter_premium_gate"] is None:
        raise AssertionError("Expected non-null drift and premium gate on synthetic frames")
    return {
        "self_test": "ok",
        "wide_s1_benchmark": {
            "delta_as_300ms_to_0ms_bps": wide_s1["delta_as_300ms_to_0ms_bps"],
            "premium_net_edge_bps": wide_s1["premium_net_edge_bps"],
            "upgrade_admitted": wide_s1["upgrade_admitted"],
            "operational_guardrails": wide_s1["operational_guardrails"],
        },
        "narrow_core_benchmark": {
            "premium_net_edge_bps": narrow_core["premium_net_edge_bps"],
            "upgrade_admitted": narrow_core["upgrade_admitted"],
        },
        "synthetic_screen_sample": {
            "symbol": screened["symbol"],
            "median_spread_bps": screened["median_spread_bps"],
            "realized_1s_vol_bps": screened["realized_1s_vol_bps"],
            "estimated_1s_abs_drift_bps": screened["estimated_1s_abs_drift_bps"],
            "upgrade_admitted": screened["lighter_premium_gate"]["upgrade_admitted"],
        },
    }


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("manifest", nargs="?", type=Path, help="JSON list of market tape specs")
    parser.add_argument(
        "--self-test",
        action="store_true",
        help="Run embedded BARBELL §2.1 & §6.3 Lighter Premium gate self-test and exit 0",
    )
    args = parser.parse_args(argv)
    if args.self_test and args.manifest is not None:
        parser.error("cannot specify both manifest and --self-test")
    if args.self_test:
        print(json.dumps(_run_self_test(), indent=2, allow_nan=False))
        return 0
    if args.manifest is None:
        parser.error("manifest is required unless --self-test is passed")
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
