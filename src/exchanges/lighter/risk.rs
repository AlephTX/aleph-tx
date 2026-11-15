//! Exact Lighter fair-mark, Small-Clamp funding, and `zeroPrice` margin health math.
//!
//! All calculations use exact [`rust_decimal::Decimal`] arithmetic with explicit
//! validation so offline research, pre-trade checks, and live risk guards share
//! one deterministic specification without float drift.

use anyhow::{Result, ensure};
use rust_decimal::Decimal;

/// Lighter's default `±0.5%` (`50 bps`) clamp around `index_price` for impact bid/ask.
pub const DEFAULT_IMPACT_CLAMP_RATIO: Decimal = Decimal::from_parts(5, 0, 0, false, 3);

/// Lighter's `0.05%` (`5 bps`) Small-Clamp parameter inside the 8h funding premium damper.
pub const DEFAULT_SMALL_CLAMP_ABS: Decimal = Decimal::from_parts(5, 0, 0, false, 4);

/// Standard `0.01%` (`1 bp`) 8-hour base interest rate component.
pub const DEFAULT_INTEREST_RATE_8H: Decimal = Decimal::from_parts(1, 0, 0, false, 4);

/// Lighter's `±0.5%` (`50 bps`) maximum hourly funding rate cap.
pub const DEFAULT_MAX_HOURLY_FUNDING_ABS: Decimal = Decimal::from_parts(5, 0, 0, false, 3);

/// Proactive deleverage warning threshold (`1.8x` maintenance margin) to exit via
/// 0 bps maker / controlled IOC before hitting Lighter's 1% LLP partial-liquidation fee.
pub const DEFAULT_PRE_LIQUIDATION_SAFETY_MULTIPLE: Decimal =
    Decimal::from_parts(18, 0, 0, false, 1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarginRegime {
    /// Flat or account equity is comfortably above `safety_multiple * maintenance_margin_req`.
    Healthy,
    /// Equity is above maintenance margin (`> 1.0x`) but below `safety_multiple` (e.g. `< 1.8x`):
    /// strategy should proactively deleverage before 1% LLP partial liquidation triggers.
    PreLiquidationDeleverage,
    /// Equity has breached maintenance margin (`<= 1.0x`) and is exposed to partial liquidation.
    PartialLiquidationZone,
    /// Equity has breached close-out margin and is exposed to `zeroPrice` LLP seizure.
    ZeroPriceBankruptcyZone,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AccountMarginHealth {
    pub account_value: Decimal,
    pub unrealized_pnl: Decimal,
    pub position_notional: Decimal,
    pub maintenance_margin_req: Decimal,
    pub close_out_margin_req: Decimal,
    /// `account_value / maintenance_margin_req`, or `None` when flat (`position == 0`).
    pub maintenance_multiple: Option<Decimal>,
    /// Exact mark price at which `account_value == maintenance_margin_req` (1% LLP fee zone).
    pub partial_liquidation_mark: Option<Decimal>,
    /// Exact mark price at which `account_value == close_out_margin_req` (`zeroPrice` zone).
    pub zero_price_trigger_mark: Option<Decimal>,
    pub regime: MarginRegime,
}

/// Computes Lighter's clamped impact midpoint:
/// `impact_bid` and `impact_ask` are each clamped to `index_price * (1 ± max_deviation_ratio)`.
pub fn clamped_impact_mid(
    index_price: Decimal,
    impact_bid: Decimal,
    impact_ask: Decimal,
    max_deviation_ratio: Decimal,
) -> Result<Decimal> {
    ensure!(
        index_price > Decimal::ZERO
            && impact_bid > Decimal::ZERO
            && impact_ask > Decimal::ZERO
            && impact_bid <= impact_ask,
        "Index and impact bid/ask prices must be positive and non-crossed"
    );
    ensure!(
        max_deviation_ratio >= Decimal::ZERO && max_deviation_ratio < Decimal::ONE,
        "Impact clamp ratio must lie in [0, 1)"
    );
    let lower = index_price * (Decimal::ONE - max_deviation_ratio);
    let upper = index_price * (Decimal::ONE + max_deviation_ratio);
    let bid = impact_bid.clamp(lower, upper);
    let ask = impact_ask.clamp(lower, upper);
    Ok((bid + ask) / Decimal::TWO)
}

/// Computes Lighter's 3-way median Fair Mark Price:
/// `Median(index_price, ema_basis_price, clamped_impact_mid)`.
pub fn fair_mark_price(
    index_price: Decimal,
    impact_bid: Decimal,
    impact_ask: Decimal,
    ema_basis_price: Decimal,
    max_deviation_ratio: Decimal,
) -> Result<Decimal> {
    ensure!(
        ema_basis_price > Decimal::ZERO,
        "EMA basis price must be positive"
    );
    let impact_mid = clamped_impact_mid(index_price, impact_bid, impact_ask, max_deviation_ratio)?;
    let mut trio = [index_price, ema_basis_price, impact_mid];
    trio.sort_unstable();
    Ok(trio[1])
}

/// Computes Lighter's 1-hour funding settlement rate under the `0.05%` Small-Clamp formula:
/// `clamp((premium_twap + clamp(interest_8h - premium_twap, -clamp_abs, +clamp_abs)) / 8, -max_hourly_abs, +max_hourly_abs)`.
///
/// Positive return means longs pay shorts.
pub fn small_clamp_hourly_funding_rate(
    premium_twap: Decimal,
    interest_rate_8h: Decimal,
    clamp_abs: Decimal,
    max_hourly_abs: Decimal,
) -> Result<Decimal> {
    ensure!(
        clamp_abs >= Decimal::ZERO && max_hourly_abs > Decimal::ZERO,
        "Funding clamp bounds must be non-negative with positive hourly cap"
    );
    let damper = (interest_rate_8h - premium_twap).clamp(-clamp_abs, clamp_abs);
    let eight = Decimal::from(8);
    let raw_hourly = (premium_twap + damper) / eight;
    Ok(raw_hourly.clamp(-max_hourly_abs, max_hourly_abs))
}

/// Computes capital-adjusted net hourly funding carry in `bps / total_margin_hour`
/// for a delta-neutral pair (`1` unit notional on Lighter hedged by `-1` unit on the hedge venue).
///
/// When `lighter_is_short` is `true`, Lighter is short (`+lighter_hourly_rate`) and the
/// hedge venue is long (`-hedge_hourly_rate`); when `false`, signs invert.
/// Total margin locked per unit notional is `(1 / lighter_leverage) + (1 / hedge_leverage)`.
pub fn net_carry_bps_per_margin_hour(
    lighter_hourly_rate: Decimal,
    hedge_hourly_rate: Decimal,
    lighter_is_short: bool,
    lighter_leverage: Decimal,
    hedge_leverage: Decimal,
) -> Result<Decimal> {
    ensure!(
        lighter_leverage > Decimal::ZERO && hedge_leverage > Decimal::ZERO,
        "Venue leverages must be positive"
    );
    let notional_spread = if lighter_is_short {
        lighter_hourly_rate - hedge_hourly_rate
    } else {
        hedge_hourly_rate - lighter_hourly_rate
    };
    let margin_per_notional = (Decimal::ONE / lighter_leverage) + (Decimal::ONE / hedge_leverage);
    Ok((notional_spread / margin_per_notional) * Decimal::from(10_000))
}

/// Evaluates Lighter account margin health, maintenance multiple, and exact mark-price
/// thresholds for partial liquidation (`maintenance_margin_fraction`) and `zeroPrice`
/// LLP close-out (`close_out_margin_fraction`).
pub fn evaluate_margin_health(
    collateral_usd: Decimal,
    signed_position: Decimal,
    entry_price: Decimal,
    mark_price: Decimal,
    maintenance_margin_fraction: Decimal,
    close_out_margin_fraction: Decimal,
    safety_multiple: Decimal,
) -> Result<AccountMarginHealth> {
    ensure!(
        collateral_usd.is_sign_positive() || collateral_usd.is_zero(),
        "Collateral cannot be negative; represent deficits via unrealized PnL"
    );
    ensure!(mark_price > Decimal::ZERO, "Mark price must be positive");
    ensure!(
        maintenance_margin_fraction > Decimal::ZERO
            && maintenance_margin_fraction < Decimal::ONE
            && close_out_margin_fraction > Decimal::ZERO
            && close_out_margin_fraction < maintenance_margin_fraction,
        "Margin fractions must satisfy 0 < close_out < maintenance < 1"
    );
    ensure!(
        safety_multiple > Decimal::ONE,
        "Pre-liquidation safety multiple must exceed 1.0"
    );

    if signed_position.is_zero() {
        return Ok(AccountMarginHealth {
            account_value: collateral_usd,
            unrealized_pnl: Decimal::ZERO,
            position_notional: Decimal::ZERO,
            maintenance_margin_req: Decimal::ZERO,
            close_out_margin_req: Decimal::ZERO,
            maintenance_multiple: None,
            partial_liquidation_mark: None,
            zero_price_trigger_mark: None,
            regime: MarginRegime::Healthy,
        });
    }

    ensure!(
        entry_price > Decimal::ZERO,
        "Entry price must be positive for a non-zero position"
    );
    let abs_pos = signed_position.abs();
    let unrealized_pnl = signed_position * (mark_price - entry_price);
    let account_value = collateral_usd + unrealized_pnl;
    let position_notional = abs_pos * mark_price;
    let maintenance_margin_req = position_notional * maintenance_margin_fraction;
    let close_out_margin_req = position_notional * close_out_margin_fraction;
    let maintenance_multiple = Some(account_value / maintenance_margin_req);

    let threshold_mark = |margin_frac: Decimal| -> Decimal {
        if signed_position > Decimal::ZERO {
            let numer = abs_pos * entry_price - collateral_usd;
            let denom = abs_pos * (Decimal::ONE - margin_frac);
            (numer / denom).max(Decimal::ZERO)
        } else {
            let numer = abs_pos * entry_price + collateral_usd;
            let denom = abs_pos * (Decimal::ONE + margin_frac);
            (numer / denom).max(Decimal::ZERO)
        }
    };

    let partial_liquidation_mark = Some(threshold_mark(maintenance_margin_fraction));
    let zero_price_trigger_mark = Some(threshold_mark(close_out_margin_fraction));

    let regime = if account_value <= close_out_margin_req {
        MarginRegime::ZeroPriceBankruptcyZone
    } else if account_value <= maintenance_margin_req {
        MarginRegime::PartialLiquidationZone
    } else if account_value < maintenance_margin_req * safety_multiple {
        MarginRegime::PreLiquidationDeleverage
    } else {
        MarginRegime::Healthy
    };

    Ok(AccountMarginHealth {
        account_value,
        unrealized_pnl,
        position_notional,
        maintenance_margin_req,
        close_out_margin_req,
        maintenance_multiple,
        partial_liquidation_mark,
        zero_price_trigger_mark,
        regime,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    macro_rules! dec {
        ($x:expr) => {
            Decimal::from_str_exact(&stringify!($x).replace(' ', "")).unwrap()
        };
    }

    #[test]
    fn fair_mark_clamps_extreme_impact_book_and_takes_median() {
        // Spoofed book at +3% ($2060/$2062) against $2000 index: clamped to +0.5% ($2010),
        // and 3-way median of ($2000 index, $2002 EMA, $2010 clamped impact) = $2002!
        let mark = fair_mark_price(
            dec!(2000),
            dec!(2060),
            dec!(2062),
            dec!(2002),
            DEFAULT_IMPACT_CLAMP_RATIO,
        )
        .unwrap();
        assert_eq!(mark, dec!(2002));

        // When EMA also tracks higher ($2015), clamped impact ($2010) caps the median at $2010.
        let capped = fair_mark_price(
            dec!(2000),
            dec!(2060),
            dec!(2062),
            dec!(2015),
            DEFAULT_IMPACT_CLAMP_RATIO,
        )
        .unwrap();
        assert_eq!(capped, dec!(2010));
    }

    #[test]
    fn small_clamp_hourly_funding_matches_lighter_005_pct_spec() {
        // Neutral premium (0): (0 + clamp(0.0001 - 0, -0.0005, 0.0005)) / 8 = 0.0000125 (0.125 bps/h)
        let neutral = small_clamp_hourly_funding_rate(
            dec!(0),
            DEFAULT_INTEREST_RATE_8H,
            DEFAULT_SMALL_CLAMP_ABS,
            DEFAULT_MAX_HOURLY_FUNDING_ABS,
        )
        .unwrap();
        assert_eq!(neutral, dec!(0.0000125));

        // Negative premium (-0.0012 = -12 bps):
        // interest - premium = 0.0001 - (-0.0012) = +0.0013 -> clamped to +0.0005 (Small-Clamp!)
        // hourly = (-0.0012 + 0.0005) / 8 = -0.0000875 (-0.875 bps/h)
        let neg = small_clamp_hourly_funding_rate(
            dec!(-0.0012),
            DEFAULT_INTEREST_RATE_8H,
            DEFAULT_SMALL_CLAMP_ABS,
            DEFAULT_MAX_HOURLY_FUNDING_ABS,
        )
        .unwrap();
        assert_eq!(neg, dec!(-0.0000875));

        // Net carry: Short Lighter (+2.0 bps/h) + Long Hyperliquid (+0.5 bps/h paid) at 5x/5x
        // Spread = 0.0002 - 0.00005 = 0.00015 per notional; total margin = 0.2 + 0.2 = 0.4
        // Carry = 0.00015 / 0.4 = 0.000375 = 3.75 bps / margin-hour.
        let carry =
            net_carry_bps_per_margin_hour(dec!(0.0002), dec!(0.00005), true, dec!(5), dec!(5))
                .unwrap();
        assert_eq!(carry, dec!(3.75));
    }

    #[test]
    fn margin_health_identifies_1_8x_deleverage_and_exact_zero_price_marks() {
        // Long 1 ETH @ $2000 with $100 collateral, MMF = 2%, Close-out = 1%.
        // Partial liquidation mark: (2000 - 100) / (1 - 0.02) = 1900 / 0.98
        // Zero-price mark: (2000 - 100) / (1 - 0.01) = 1900 / 0.99
        let healthy = evaluate_margin_health(
            dec!(100),
            dec!(1),
            dec!(2000),
            dec!(2000),
            dec!(0.02),
            dec!(0.01),
            DEFAULT_PRE_LIQUIDATION_SAFETY_MULTIPLE,
        )
        .unwrap();
        assert_eq!(healthy.regime, MarginRegime::Healthy);
        assert_eq!(healthy.maintenance_multiple, Some(dec!(2.5)));

        // Drop mark to $1960: PnL = -$40, equity = $60, MM = $1960 * 0.02 = $39.20
        // Multiple = 60 / 39.20 = 1.5306x (< 1.8x) -> PreLiquidationDeleverage!
        let warn = evaluate_margin_health(
            dec!(100),
            dec!(1),
            dec!(2000),
            dec!(1960),
            dec!(0.02),
            dec!(0.01),
            DEFAULT_PRE_LIQUIDATION_SAFETY_MULTIPLE,
        )
        .unwrap();
        assert_eq!(warn.regime, MarginRegime::PreLiquidationDeleverage);

        // Drop mark to $1910: PnL = -$90, equity = $10, close-out = $19.10 -> ZeroPriceBankruptcyZone!
        let zero_zone = evaluate_margin_health(
            dec!(100),
            dec!(1),
            dec!(2000),
            dec!(1910),
            dec!(0.02),
            dec!(0.01),
            DEFAULT_PRE_LIQUIDATION_SAFETY_MULTIPLE,
        )
        .unwrap();
        assert_eq!(zero_zone.regime, MarginRegime::ZeroPriceBankruptcyZone);
        assert!(
            zero_zone.zero_price_trigger_mark.unwrap()
                < zero_zone.partial_liquidation_mark.unwrap()
        );
    }
}
