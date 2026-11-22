#!/usr/bin/env python3
"""Attribute simulated activity to the documented scope of Lighter's two incentives."""
from decimal import Decimal
from pathlib import Path
import re

RH_POINTS_RULE = "https://docs.lighter.xyz/programs/lighter-on-robinhood-chain-points.md"
LPP_RULE = "https://docs.lighter.xyz/programs/liquidity-partner-program.md"
ARCUS_TERMS_RULE = "https://arcus.xyz/legal/protocol-terms"
ARCUS_FEES_RULE = "https://docs.arcus.xyz/concepts/perpetuals/fees.md"
HYPERLIQUID_FEES_URL = "https://hyperliquid.gitbook.io/hyperliquid-docs/trading/fees"
HIP3_GROWTH_MODE_URL = "https://hyperliquid.gitbook.io/hyperliquid-docs/hyperliquid-improvement-proposals-hips/hip-3-builder-deployed-perpetuals"


def _venue_capability_snapshot_version() -> str:
    venues_rs = (Path(__file__).resolve().parent.parent / "src/venues.rs").read_text(encoding="utf-8")
    match = re.search(r'pub const VENUE_CAPABILITY_SNAPSHOT_VERSION:\s*&str\s*=\s*"([^"]+)";', venues_rs)
    if not match:
        raise AssertionError("Missing VENUE_CAPABILITY_SNAPSHOT_VERSION in src/venues.rs")
    return match.group(1)


VENUE_CAPABILITY_SNAPSHOT_VERSION = _venue_capability_snapshot_version()


def _nonnegative_amount(value, *, allow_none=False):
    if value is None:
        if allow_none:
            return None
        raise ValueError("amount cannot be None")
    result = Decimal(str(value))
    if not result.is_finite() or result < 0:
        raise ValueError("amount must be finite and nonnegative")
    return result


def cost_per_million_eligible_maker(net_cash, eligible_maker_turnover):
    """Return cost ($ per $1M confirmed/scoped eligible target maker volume) or None when undefined."""
    if net_cash is None or eligible_maker_turnover is None:
        return None
    cash = Decimal(str(net_cash))
    elig = _nonnegative_amount(eligible_maker_turnover)
    if not cash.is_finite() or elig <= 0:
        return None
    return str(-cash * Decimal(1_000_000) / elig)


def _fmt_bps(value: Decimal) -> str:
    two_dp = value.quantize(Decimal("0.01"))
    return str(two_dp if value == two_dp else value.normalize())


def multi_venue_program_scope(
    *,
    venue,
    liquidity_domain,
    asset_class,
    account_tier,
    maker_turnover,
    taker_turnover="0",
    counterparty_tier="unknown",
    strategy_posture="directional_or_unhedged",
    growth_mode=False,
    maker_14d_share=None,
    deployer_fee_scale="1",
):
    """Evaluate primary-rule program eligibility & fee-floor constraints across supported venues (`S8-2`)."""
    maker = _nonnegative_amount(maker_turnover)
    taker = _nonnegative_amount(taker_turnover)
    if asset_class not in ("crypto", "equity"):
        raise ValueError("unsupported asset_class")
    if counterparty_tier not in ("standard", "premium", "advanced", "unknown"):
        raise ValueError("unsupported counterparty_tier")

    disqualification_risks = []
    confirmed_eligible = None
    ceiling_eligible = None
    program_status = "unknown"
    maker_fee_bps = None
    taker_fee_bps = None
    volume_tier_credit_mult = "1"
    tier_req_usd = None
    rebate_bps = "0"
    source = RH_POINTS_RULE

    if venue == "lighter" and liquidity_domain == "lighter":
        source = LPP_RULE
        ceiling_eligible = "0"
        confirmed_eligible = "0"
        if account_tier == "standard":
            maker_fee_bps, taker_fee_bps = "0", "0"
            program_status = "ineligible_core_domain_and_lpp_standard_tier"
        elif account_tier == "plus":
            maker_fee_bps, taker_fee_bps = "0.50", "0.50"
            program_status = "ineligible_core_domain_for_rh_points_lpp_plus_unverified"
        elif account_tier in ("premium", "advanced"):
            maker_fee_bps, taker_fee_bps = "0.4", "2.8"
            program_status = "ineligible_core_domain_for_rh_points_lpp_snapshot_unverified"
        else:
            ceiling_eligible = None
            confirmed_eligible = None
            program_status = "unverified_lighter_account_tier"
    elif venue == "lighter_robinhood" and liquidity_domain == "lighter:robinhood":
        source = RH_POINTS_RULE
        if account_tier == "standard":
            maker_fee_bps, taker_fee_bps = "0", "0"
            if counterparty_tier == "standard":
                ceiling_eligible = "0"
                confirmed_eligible = "0"
                program_status = "ineligible_standard_to_standard_excluded"
                disqualification_risks.append("standard_to_standard_activity_unrewarded")
            else:
                ceiling_eligible = str(maker)
                program_status = "unverified_counterparty_tier_ceiling_only"
                disqualification_risks.append("standard_to_standard_activity_unrewarded")
        elif account_tier == "plus":
            # RH Plus base fee is 1.50/1.50 bps (not mainnet Plus 0.50/0.50 bps).
            maker_fee_bps, taker_fee_bps = "1.50", "1.50"
            ceiling_eligible = str(maker)
            program_status = "plus_tier_requires_program_statement"
        elif account_tier in ("premium", "advanced"):
            # RH base Premium (<$1M 14d volume) is 1.20/3.50 bps, scaling down to 0.84/2.45 bps.
            maker_fee_bps, taker_fee_bps = "1.20", "3.50"
            ceiling_eligible = str(maker)
            program_status = "premium_tier_requires_program_statement"
        else:
            ceiling_eligible = None
            confirmed_eligible = None
            program_status = "unverified_rh_account_tier"
    elif venue == "arcus" and liquidity_domain == "arcus":
        source = f"{ARCUS_FEES_RULE} | {ARCUS_TERMS_RULE}"
        if account_tier == "level_0":
            ceiling_eligible = str(maker)
            if asset_class == "equity":
                maker_fee_bps, taker_fee_bps = "0.00", "2.25"
                program_status = "eligible_rwa_l0_zero_maker_fee_subject_to_annex_c"
            else:
                maker_fee_bps, taker_fee_bps = "1.50", "4.50"
                program_status = "fee_dominated_crypto_l0_150usd_per_m"
        elif account_tier == "level_4":
            ceiling_eligible = str(maker)
            tier_req_usd = "200000000"
            maker_fee_bps = "0.00"
            taker_fee_bps = "1.15" if asset_class == "equity" else "2.30"
            program_status = "requires_200m_30d_volume_for_l4"
        else:
            ceiling_eligible = None
            confirmed_eligible = None
            program_status = "unverified_arcus_tier"
        if strategy_posture in ("delta_neutral_hedged", "looped_turnover", "wash_or_self_offset"):
            disqualification_risks.append("arcus_annex_c_neutral_hedged_or_looped_volume_disqualifiable")
            confirmed_eligible = None
    elif venue == "hyperliquid" and liquidity_domain in ("hyperliquid", "hyperliquid:core"):
        if growth_mode:
            raise ValueError("native hyperliquid does not use HIP-3 growth mode")
        source = HYPERLIQUID_FEES_URL
        ceiling_eligible = str(maker)
        maker_fee_bps, taker_fee_bps = "1.50", "4.50"
        share = Decimal(str(maker_14d_share)) if maker_14d_share is not None else Decimal(0)
        if share > Decimal("0.030"):
            rebate_bps = "0.3"
        elif share > Decimal("0.015"):
            rebate_bps = "0.2"
        elif share > Decimal("0.005"):
            rebate_bps = "0.1"
        else:
            disqualification_risks.append("hl_maker_rebate_requires_above_0_5pct_14d_maker_share")
        program_status = (
            "hl_maker_rebate_active"
            if rebate_bps != "0"
            else "fee_dominated_hl_tier0_150usd_per_m_without_0_5pct_share"
        )
    elif (
        (venue == "trade_xyz" and liquidity_domain in ("trade_xyz", "hip3-xyz", "hyperliquid:hip3-xyz"))
        or (venue == "entropy" and liquidity_domain in ("entropy", "hip3-io", "hyperliquid:hip3-io"))
    ):
        source = f"{HYPERLIQUID_FEES_URL} | {HIP3_GROWTH_MODE_URL}"
        fee_scale = _nonnegative_amount(deployer_fee_scale)
        if fee_scale > Decimal("5"):
            raise ValueError("HIP-3 deployer_fee_scale must lie in [0, 5]")
        scale_factor = 1 + fee_scale if fee_scale < 1 else 2 * fee_scale
        if growth_mode and asset_class == "crypto":
            ceiling_eligible = "0"
            confirmed_eligible = "0"
            program_status = "ineligible_hip3_growth_mode_excludes_core_crypto_assets"
            disqualification_risks.append("hip3_growth_mode_core_crypto_asset_disjointness")
        elif growth_mode:
            ceiling_eligible = str(maker)
            maker_fee_bps = _fmt_bps(Decimal("1.50") * scale_factor * Decimal("0.10"))
            taker_fee_bps = _fmt_bps(Decimal("4.50") * scale_factor * Decimal("0.10"))
            volume_tier_credit_mult = "0.10"
            program_status = "hip3_growth_mode_90pct_fee_and_volume_cut"
            disqualification_risks.append("hip3_growth_mode_cuts_tier_volume_and_rebates_90pct")
        else:
            ceiling_eligible = str(maker)
            maker_fee_bps = _fmt_bps(Decimal("1.50") * scale_factor)
            taker_fee_bps = _fmt_bps(Decimal("4.50") * scale_factor)
            program_status = "hip3_non_growth_deployer_fee_scale"
            disqualification_risks.append("hip3_maker_rebate_not_modeled")
    else:
        raise ValueError(f"unsupported venue/liquidity_domain: {venue}/{liquidity_domain}")

    return {
        "capability_snapshot_version": VENUE_CAPABILITY_SNAPSHOT_VERSION,
        "venue": venue,
        "liquidity_domain": liquidity_domain,
        "asset_class": asset_class,
        "account_tier": account_tier,
        "maker_turnover_quote": str(maker),
        "taker_turnover_quote": str(taker),
        "eligible_target_maker_ceiling_quote": ceiling_eligible,
        "confirmed_eligible_maker_quote": confirmed_eligible,
        "maker_fee_bps": maker_fee_bps,
        "taker_fee_bps": taker_fee_bps,
        "maker_rebate_bps": rebate_bps,
        "volume_tier_credit_multiplier": volume_tier_credit_mult,
        "tier_30d_volume_requirement_usd": tier_req_usd,
        "program_status": program_status,
        "disqualification_risks": disqualification_risks,
        "primary_source": source,
    }


def program_scope(*, core_turnover, rh_turnover, account_tier):
    """Report what gross-domain totals alone establish, before any account attestation.

    Both amounts must come from the same strategy and window. A known zero means
    that this strategy had no activity on the domain. None denotes missing evidence.
    The public RH hedge multiplier concerns opposing positions, not Core turnover.
    """
    def amount(value):
        if value is None:
            return None
        result = Decimal(str(value))
        if not result.is_finite() or result < 0:
            raise ValueError("turnover must be finite and nonnegative")
        return result

    core = amount(core_turnover)
    rh = amount(rh_turnover)
    if account_tier not in ("standard", "plus", "premium", "advanced", "unknown"):
        raise ValueError("unsupported account tier")
    if account_tier == "standard":
        lpp_status = "ineligible_account_tier"
    elif account_tier == "plus":
        lpp_status = "plus_tier_lpp_eligibility_unverified"
    elif account_tier == "unknown":
        lpp_status = "account_tier_unknown"
    else:
        lpp_status = "requires_eligible_asset_and_resting_order_snapshots"

    return {
        "core_turnover_quote": None if core is None else str(core),
        "rh_turnover_quote": None if rh is None else str(rh),
        "rh_direct_eligible_volume_ceiling_quote": None if rh is None else str(rh),
        "rh_direct_rule_eligible_turnover_quote": "0" if rh == 0 else None,
        "lpp_status": lpp_status,
        "lpp_measure": "resting_liquidity_at_snapshots",
        "venue_confirmed_eligible_turnover_quote": None,
        "points_cash_value_assumed_quote": "0",
    }


def _signed_decimal(value, default="0"):
    if value is None:
        return Decimal(str(default))
    result = Decimal(str(value))
    if not result.is_finite():
        raise ValueError("numeric value must be finite")
    return result


def decompose_execution_friction(
    report,
    *,
    fixed_overhead_usd_per_day="0",
    window_hours="10",
):
    """Break execution friction into 3 auditable parts (slippage, reject/retry, partial residual).

    Supports both `MakerFirstReport` (cross-venue dual-leg) and `RecordedMakerReport`
    (single-venue MM) without charging any unverified `0.25 bp` modeled tax.
    """
    fixed_day = _nonnegative_amount(fixed_overhead_usd_per_day)
    hours = _nonnegative_amount(window_hours)
    if hours <= 0:
        raise ValueError("window_hours must be positive")
    fixed_infra = fixed_day * hours / Decimal(24)

    # 1. Simulated slippage vs submit-time touch / arrival mid.
    attr = report.get("cross_market_attribution") or {}
    if "ioc_slippage" in report or "ioc_depth_impact" in report:
        simulated_slippage = _signed_decimal(report.get("ioc_slippage"))
        slippage_source = "ioc_slippage_vs_submit_touch"
    elif "ioc_cost_quote_vs_submit_mid" in attr:
        simulated_slippage = _signed_decimal(attr.get("ioc_cost_quote_vs_submit_mid"))
        slippage_source = "ioc_cost_quote_vs_submit_mid"
    else:
        simulated_slippage = Decimal(0)
        slippage_source = "none_reported"

    # 2. Rejection & retry counts / friction (explicit counters preserved; no fabricated cost).
    post_only_rejects = int(
        report.get("post_only_rejected", report.get("post_only_rejections", 0)) or 0
    )
    stale_rejects = int(report.get("stale_rejections", 0) or 0)
    precision_rejects = int(report.get("precision_rejections", 0) or 0)
    ioc_misses = (
        int(report.get("ioc_expired", 0) or 0)
        + int(report.get("ioc_price_misses", 0) or 0)
        + int(report.get("ioc_depth_misses", 0) or 0)
    )
    cycle_attr = report.get("cycle_attribution") or {}
    retry_hedge_fees = _signed_decimal(
        cycle_attr.get("forced_ioc_fees", [0, 0])[1]
        if isinstance(cycle_attr.get("forced_ioc_fees"), list)
        and len(cycle_attr.get("forced_ioc_fees", [])) >= 2
        else 0
    ) + _signed_decimal(
        cycle_attr.get("forced_ioc_fees", [0, 0])[0]
        if isinstance(cycle_attr.get("forced_ioc_fees"), list)
        and len(cycle_attr.get("forced_ioc_fees", [])) >= 2
        else 0
    )

    # 3. Partial-fill residual / forced unwind friction.
    forced_closes = int(report.get("forced_closes", 0) or 0)
    forced_close_pnl = _signed_decimal(report.get("forced_close_pnl", 0))
    dust_fills = int(cycle_attr.get("reduce_ioc_dust_fills", 0) or 0)
    sub_min_top_ups = int(cycle_attr.get("sub_min_top_up_unwinds", 0) or 0)
    sub_min_recovery_turnover = Decimal(0)
    if isinstance(cycle_attr.get("sub_min_recovery_turnover"), list):
        sub_min_recovery_turnover = sum(
            (_signed_decimal(x) for x in cycle_attr["sub_min_recovery_turnover"]),
            Decimal(0),
        )
    forced_ioc_slippage = Decimal(0)
    if isinstance(cycle_attr.get("forced_ioc_slippage"), list):
        forced_ioc_slippage = sum(
            (_signed_decimal(x) for x in cycle_attr["forced_ioc_slippage"]),
            Decimal(0),
        )
    final_position = _signed_decimal(report.get("final_position", 0))

    if "maker_fees" in report and "taker_fees" in report:
        venue_fees = _signed_decimal(report.get("maker_fees")) + _signed_decimal(report.get("taker_fees"))
    else:
        venue_fees = _signed_decimal(report.get("fees", 0))

    partial_residual_loss = (-forced_close_pnl if forced_close_pnl < 0 else Decimal(0)) + (
        forced_ioc_slippage if forced_ioc_slippage > 0 else Decimal(0)
    )

    return {
        "simulated_slippage": {
            "source": slippage_source,
            "slippage_quote": str(simulated_slippage),
            "depth_impact_quote": str(_signed_decimal(report.get("ioc_depth_impact", 0))),
        },
        "rejection_and_retry": {
            "post_only_rejections": post_only_rejects,
            "stale_rejections": stale_rejects,
            "precision_rejections": precision_rejects,
            "ioc_misses_or_expired": ioc_misses,
            "forced_ioc_fee_quote": str(retry_hedge_fees),
            "reconciled_cancel_requests": int(
                report.get("quote_cancels", attr.get("maker_cancel_requests", 0)) or 0
            ),
        },
        "partial_fill_residual": {
            "forced_closes": forced_closes,
            "forced_close_pnl_quote": str(forced_close_pnl),
            "forced_ioc_slippage_quote": str(forced_ioc_slippage),
            "partial_residual_cost_quote": str(partial_residual_loss),
            "reduce_ioc_dust_fills": dust_fills,
            "sub_min_top_up_unwinds": sub_min_top_ups,
            "sub_min_recovery_turnover_quote": str(sub_min_recovery_turnover),
            "terminal_open_position_base": str(final_position),
        },
        "venue_fees_quote": str(venue_fees),
        "fixed_infra_quote": str(fixed_infra),
        "modeled_extra_overhead_bps": "0",
    }


def score_program_cost_effectiveness(
    *,
    program,
    net_variable_pnl_quote,
    fixed_infra_quote="0",
    eligible_maker_turnover_quote=None,
    resting_snapshot_score=None,
    annex_c_hedged_or_looped=False,
    hip3_growth_mode=False,
):
    """Score per-program 'eligible score per dollar cost' rather than cost per raw volume (`S9-1`).

    Programs:
      - `rh_points`: Lighter RH Points (`1M` eligible maker turnover per unit; excludes Standard-vs-Standard)
      - `lighter_lpp`: Lighter Liquidity Partner Program (`resting_snapshot_score`, never raw turnover)
      - `arcus_rwa`: Arcus RWA Points (`1M` eligible maker turnover per unit; disqualifies neutral/looped if flagged)
      - `hip3_tier_volume`: HIP-3 fee-tier credit (`0.10x` weight when `growth_mode` is active)
      - `zero_reward`: Self-funded proprietary trading (no incentive score denominator)
    """
    var_pnl = _signed_decimal(net_variable_pnl_quote)
    fixed = _nonnegative_amount(fixed_infra_quote)
    all_in_pnl = var_pnl - fixed
    net_cost_usd = -all_in_pnl

    valid_programs = (
        "rh_points",
        "lighter_lpp",
        "arcus_rwa",
        "hip3_tier_volume",
        "zero_reward",
    )
    if program not in valid_programs:
        raise ValueError(f"unsupported program: {program}")

    eligible_units = None
    unit_name = "none"
    disqualified_reason = None

    if program == "rh_points":
        unit_name = "million_eligible_rh_maker_usd"
        elig = _nonnegative_amount(eligible_maker_turnover_quote, allow_none=True)
        if elig is not None and elig > 0:
            eligible_units = elig / Decimal(1_000_000)
        elif elig == 0:
            disqualified_reason = "zero_eligible_rh_maker_turnover"
    elif program == "lighter_lpp":
        unit_name = "resting_liquidity_snapshot_score"
        if resting_snapshot_score is not None:
            snap = _nonnegative_amount(resting_snapshot_score)
            if snap > 0:
                eligible_units = snap
            else:
                disqualified_reason = "zero_resting_snapshot_score_or_ineligible_tier"
        else:
            disqualified_reason = "resting_snapshot_score_unmeasured_or_ineligible_tier"
    elif program == "arcus_rwa":
        unit_name = "million_eligible_arcus_rwa_maker_usd"
        if annex_c_hedged_or_looped:
            disqualified_reason = "arcus_annex_c_hedged_or_looped_disqualifiable"
        else:
            elig = _nonnegative_amount(eligible_maker_turnover_quote, allow_none=True)
            if elig is not None and elig > 0:
                eligible_units = elig / Decimal(1_000_000)
    elif program == "hip3_tier_volume":
        unit_name = "million_tier_credited_volume_usd"
        elig = _nonnegative_amount(eligible_maker_turnover_quote, allow_none=True)
        if elig is not None and elig > 0:
            mult = Decimal("0.10") if hip3_growth_mode else Decimal("1")
            eligible_units = elig * mult / Decimal(1_000_000)
    else:
        unit_name = "zero_reward_prop"
        disqualified_reason = "zero_reward_no_incentive_score"

    usd_per_unit = None
    score_per_usd_cost = None
    if eligible_units is not None and eligible_units > 0:
        usd_per_unit = str(net_cost_usd / eligible_units)
        if net_cost_usd > 0:
            score_per_usd_cost = str(eligible_units / net_cost_usd)

    return {
        "program": program,
        "unit_name": unit_name,
        "net_variable_pnl_quote": str(var_pnl),
        "fixed_infra_quote": str(fixed),
        "all_in_pnl_quote": str(all_in_pnl),
        "net_cost_usd": str(net_cost_usd),
        "eligible_score_units": None if eligible_units is None else str(eligible_units),
        "usd_cost_per_eligible_score_unit": usd_per_unit,
        "eligible_score_per_usd_cost": score_per_usd_cost,
        "surplus_with_positive_pnl": all_in_pnl > 0,
        "disqualified_or_undefined_reason": disqualified_reason,
    }
