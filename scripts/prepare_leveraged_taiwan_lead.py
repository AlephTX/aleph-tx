#!/usr/bin/env python3
"""Prepare and validate the frozen $1,000-equity + 25x-leverage Taiwan Core BTC Mode B config (`S8-4B`).

Isolates capital/margin scaling traps observed in naive $50k -> $1k downscaling:
  1. `reference_portfolio_value` mismatch (`$100` or `$50k` defaults) or `portfolio_scale` clamping
     silently shrinks a $5,000 clip 50x to $100 unless `reference_portfolio_value = initial_equity`
     and `min_portfolio_scale = max_portfolio_scale = 1.0`.
  2. Default `max_leverage = 10.0` blocks $5,000 clipping / $10,000 inventory on $1,000 initial equity
     under `evaluate_margin_health` (requires `max_leverage = 25.0`, `MMF = 0.012`, `1.8x` safety).
  3. Default `max_daily_loss_pct = 0.002` on $1,000 equity triggers a $2.00 premature halt before W1/W2
     verified drawdowns ($22.99 / $40.00); $1,000 leveraged spec fixes `max_daily_loss_pct = 0.05` ($50.00).

Also binds explicit Lighter RH / LPP eligibility accounting (`rh_direct_rule_eligible_turnover_quote = "0"`).
"""
import argparse
import hashlib
import json
from pathlib import Path

from audit_lighter_program_scope import multi_venue_program_scope


def build_leveraged_taiwan_core_btc_spec(
    *,
    start_ms,
    end_ms,
    drain_until_ms,
    trade_from_ms=None,
    initial_equity=1000.0,
    clip_notional_usd=5000.0,
    max_position_notional_usd=10000.0,
    max_leverage=25.0,
    min_profit_bps=16.0,
    depth_participation=0.25,
    max_daily_loss_pct=0.05,
    order_entry_ms=250,
    venue_cancel_delay_ms=300,
    venue_taker_delay_ms=300,
):
    """Build and fail-closed validate a $1,000-margin leveraged Core BTC Mode B Replay config."""
    if not (500.0 <= initial_equity <= 5000.0):
        raise ValueError("initial_equity must reflect the $500-$5000 constrained budget test range")
    if clip_notional_usd <= 0 or max_position_notional_usd < clip_notional_usd:
        raise ValueError("max_position_notional_usd must be >= clip_notional_usd")
    required_initial_margin = max_position_notional_usd / max_leverage
    if required_initial_margin > initial_equity * 0.85:
        raise ValueError(
            f"max_position initial margin ${required_initial_margin:.2f} exceeds 85% of ${initial_equity:.2f} equity at {max_leverage}x"
        )
    daily_loss_budget_usd = initial_equity * max_daily_loss_pct
    if daily_loss_budget_usd <= 40.0:
        raise ValueError(
            f"daily loss cap ${daily_loss_budget_usd:.2f} would prematurely halt inside historical W2 drawdown ($40.00)"
        )

    config = {
        "strategy": {
            "exchange_id": 2,
            "symbol_id": 1001,
            "market_id": 1,
            "tick_size": 0.1,
            "step_size": 0.0001,
            "maker_fee_bps": 0.0,
            "min_profit_bps": min_profit_bps,
            "penny_ticks": 1.0,
            "inventory_skew_bps": 3.0,
            "grid_levels": 1,
            "grid_spacing_bps": 2.0,
            "grid_size_decay": 0.7,
            "base_order_size": 0.05,
            "max_position": 0.20,
            "inventory_urgency_threshold": 0.10,
            "base_order_notional_usd": clip_notional_usd,
            "round_base_notional_nearest": True,
            "max_position_notional_usd": max_position_notional_usd,
            "inventory_urgency_notional_usd": clip_notional_usd,
            "min_inventory_notional_usd": 10.0,
            "reference_portfolio_value": initial_equity,
            "min_portfolio_scale": 1.0,
            "max_portfolio_scale": 1.0,
            "use_depth_pricing": False,
            "use_post_only": True,
            "require_external_fair_value": False,
            "adverse_selection_threshold": 2.0,
            "requote_threshold_bps": 1.5,
            "order_ttl_secs": 5,
            "max_leverage": max_leverage,
            "min_available_balance": 20.0,
            "max_daily_loss_pct": max_daily_loss_pct,
            "position_timeout_secs": 60,
            "local_staleness_ms": 5000,
            "stale_bbo_cancel_ms": 15000,
            "target_resting_margin_utilization": 0.35,
            "maintenance_margin_fraction": 0.012,
            "pre_liquidation_safety_multiple": 1.8,
            "cap_utilization_floor_to_base_notional": True,
        },
        "start_ms": start_ms,
        "trade_from_ms": start_ms if trade_from_ms is None else trade_from_ms,
        "end_ms": end_ms,
        "drain_until_ms": drain_until_ms,
        "initial_equity": initial_equity,
        "order_entry_ms": order_entry_ms,
        "venue_maker_delay_ms": 0,
        "venue_cancel_delay_ms": venue_cancel_delay_ms,
        "venue_taker_delay_ms": venue_taker_delay_ms,
        "market_data_delay_ms": 0,
        "order_response_ms": 100,
        "trade_clock_guard_ms": 100,
        "trade_delivery_grace_ms": 2000,
        "core_only": True,
        "flat_at_funding": {
            "interval_ms": 3_600_000,
            "offset_ms": 0,
            "drain_before_ms": 10_000,
            "guard_ms": 1_000,
            "max_event_gap_ms": 5_000,
        },
        "taker_fee_bps": 0.0,
        "min_contracts": 0.0001,
        "min_notional": 10.0,
        "depth_participation": depth_participation,
        "diagnose_cancel_attribution": True,
    }

    eligibility = multi_venue_program_scope(
        venue="lighter",
        liquidity_domain="lighter",
        asset_class="crypto",
        account_tier="standard",
        maker_turnover="0",
    )
    canonical = json.dumps(config, sort_keys=True, separators=(",", ":")).encode()
    return {
        "spec_version": "taiwan_core_btc_mode_b_1000_25x.v1",
        "config_sha256": hashlib.sha256(canonical).hexdigest(),
        "replay_config": config,
        "margin_audit": {
            "initial_equity_usd": initial_equity,
            "clip_notional_usd": clip_notional_usd,
            "max_position_notional_usd": max_position_notional_usd,
            "max_leverage": max_leverage,
            "initial_margin_at_max_pos_usd": required_initial_margin,
            "daily_loss_hard_stop_usd": daily_loss_budget_usd,
        },
        "eligibility_audit": eligibility,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--start-ms", type=int, default=1_800_000_000_000)
    parser.add_argument("--trade-from-ms", type=int, help="First decision time after any pre-window warm-up")
    parser.add_argument("--end-ms", type=int, default=1_800_036_000_000)
    parser.add_argument("--drain-until-ms", type=int, default=1_800_036_015_000)
    args = parser.parse_args()
    spec = build_leveraged_taiwan_core_btc_spec(
        start_ms=args.start_ms,
        trade_from_ms=args.trade_from_ms,
        end_ms=args.end_ms,
        drain_until_ms=args.drain_until_ms,
    )
    print(json.dumps(spec, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
