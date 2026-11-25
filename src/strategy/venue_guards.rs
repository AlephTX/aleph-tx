//! Venue-agnostic market metadata preflight & reference-oracle divergence gates.
//!
//! Contains ZERO imports from `crate::exchanges::*`; venue adapters convert native
//! market structs into [`VenueMarketMetadata`] at the boundary.

use crate::strategy::tail_risk::TailRiskAction;
use anyhow::{Context, Result, ensure};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct U320([u64; 5]);

impl U320 {
    fn from_u128(v: u128) -> Self {
        Self([v as u64, (v >> 64) as u64, 0, 0, 0])
    }

    fn mul_u64(self, rhs: u64) -> Option<Self> {
        let mut out = [0_u64; 5];
        let mut carry = 0_u128;
        let r = u128::from(rhs);
        for (dst, &src) in out.iter_mut().zip(self.0.iter()) {
            let prod = u128::from(src) * r + carry;
            *dst = prod as u64;
            carry = prod >> 64;
        }
        (carry == 0).then_some(Self(out))
    }

    fn mul_u128(self, rhs: u128) -> Option<Self> {
        let mut out = [0_u128; 6];
        let r = [rhs as u64, (rhs >> 64) as u64];
        for (i, &a) in self.0.iter().enumerate() {
            let mut carry = 0_u128;
            for (j, &b) in r.iter().enumerate() {
                let sum = out[i + j] + u128::from(a) * u128::from(b) + carry;
                out[i + j] = sum & u128::from(u64::MAX);
                carry = sum >> 64;
            }
            if carry > 0 {
                if i + 2 >= out.len() {
                    return None;
                }
                out[i + 2] += carry;
            }
        }
        let mut limbs = [0_u64; 5];
        let mut carry = 0_u128;
        for (dst, &acc) in limbs.iter_mut().zip(out[..5].iter()) {
            let sum = acc + carry;
            *dst = sum as u64;
            carry = sum >> 64;
        }
        (carry == 0 && out[5] == 0).then_some(Self(limbs))
    }

    fn mul_pow10(mut self, mut exp: u32) -> Option<Self> {
        while exp > 0 {
            let step = exp.min(19);
            let pow = 10_u64.pow(step);
            self = self.mul_u64(pow)?;
            exp -= step;
        }
        Some(self)
    }
}

impl Ord for U320 {
    fn cmp(&self, other: &Self) -> Ordering {
        for idx in (0..5).rev() {
            match self.0[idx].cmp(&other.0[idx]) {
                Ordering::Equal => continue,
                ord => return ord,
            }
        }
        Ordering::Equal
    }
}

impl PartialOrd for U320 {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn exact_decimal_divergence_exceeds(
    diff: Decimal,
    oracle_px: Decimal,
    max_div_dec: Decimal,
) -> Result<bool> {
    if diff.is_zero() {
        return Ok(false);
    }
    let m_d = u128::try_from(diff.mantissa()).context("negative diff mantissa")?;
    let m_o = u128::try_from(oracle_px.mantissa()).context("negative oracle mantissa")?;
    let m_l = u128::try_from(max_div_dec.mantissa()).context("negative limit mantissa")?;
    let s_lhs = diff.scale();
    let s_rhs = oracle_px.scale().saturating_add(max_div_dec.scale());
    let lhs = U320::from_u128(m_d)
        .mul_u64(10_000)
        .and_then(|v| v.mul_pow10(s_rhs.saturating_sub(s_lhs)))
        .context("exact oracle divergence LHS overflow")?;
    let rhs = U320::from_u128(m_l)
        .mul_u128(m_o)
        .and_then(|v| v.mul_pow10(s_lhs.saturating_sub(s_rhs)))
        .context("exact oracle divergence RHS overflow")?;
    Ok(lhs > rhs)
}

/// Normalized venue market metadata for preflight eligibility & fee-tier enforcement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VenueMarketMetadata {
    pub venue_id: String,
    pub symbol: String,
    pub is_delisted: bool,
    pub fee_discount_mode: Option<String>,
    pub fee_scale: Option<f64>,
    pub oracle_price: Option<Decimal>,
}

/// Venue-agnostic preflight policy governing fee tier requirements and delisting checks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VenuePreflightPolicy {
    /// Required fee discount mode value (e.g., `Some("enabled")` for HIP-3 Growth Mode).
    pub require_fee_discount_mode: Option<String>,
    /// Upper bound on optional deployer/fee scale (`>= 0.0`).
    pub max_fee_scale: f64,
}

impl Default for VenuePreflightPolicy {
    fn default() -> Self {
        Self {
            require_fee_discount_mode: None,
            max_fee_scale: 1.0,
        }
    }
}

impl VenuePreflightPolicy {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.max_fee_scale.is_finite() && self.max_fee_scale >= 0.0,
            "max_fee_scale must be finite and >= 0"
        );
        if let Some(mode) = &self.require_fee_discount_mode {
            ensure!(
                !mode.trim().is_empty(),
                "require_fee_discount_mode must be non-empty when specified"
            );
        }
        Ok(())
    }
}

/// Verify a normalized venue market satisfies delisting, discount-mode, and fee-scale policy.
pub fn validate_venue_market_preflight(
    market: &VenueMarketMetadata,
    policy: &VenuePreflightPolicy,
) -> Result<()> {
    policy.validate()?;
    ensure!(
        !market.is_delisted,
        "venue '{}' market '{}' is delisted",
        market.venue_id,
        market.symbol
    );
    if let Some(required_mode) = policy.require_fee_discount_mode.as_deref() {
        ensure!(
            market
                .fee_discount_mode
                .as_deref()
                .is_some_and(|mode| mode.trim().eq_ignore_ascii_case(required_mode.trim())),
            "venue '{}' market '{}' missing required fee_discount_mode='{}' (got {:?})",
            market.venue_id,
            market.symbol,
            required_mode,
            market.fee_discount_mode
        );
    }
    if let Some(scale) = market.fee_scale {
        ensure!(
            scale.is_finite() && scale >= 0.0 && scale <= policy.max_fee_scale,
            "venue '{}' market '{}' fee_scale ({scale:.4}) exceeds max_fee_scale ({:.4})",
            market.venue_id,
            market.symbol,
            policy.max_fee_scale
        );
    }
    Ok(())
}

/// Evaluate cross-venue or maker mid divergence against an external/oracle reference price (`Decimal`).
///
/// Uses exact `Decimal` comparison against `max_divergence_bps` so exact equality boundaries
/// (such as `99.24750` vs `99` at `25.0` bps) are never false-rejected by binary `f64` rounding drift,
/// while preserving the continuous `f64` divergence telemetry output.
pub fn evaluate_reference_oracle_divergence_bps(
    reference_mid_px: Decimal,
    oracle_px: Decimal,
    max_divergence_bps: f64,
) -> Result<(f64, TailRiskAction)> {
    ensure!(
        reference_mid_px > Decimal::ZERO,
        "invalid non-positive reference mid price"
    );
    ensure!(
        oracle_px > Decimal::ZERO,
        "invalid non-positive oracle price"
    );
    ensure!(
        max_divergence_bps.is_finite() && max_divergence_bps > 0.0,
        "max_divergence_bps must be finite and > 0"
    );
    let max_div_dec = Decimal::try_from(max_divergence_bps)
        .ok()
        .or_else(|| Decimal::from_f64_retain(max_divergence_bps))
        .filter(|d| *d > Decimal::ZERO)
        .context("invalid or non-positive max_divergence_bps")?;

    let diff = (reference_mid_px - oracle_px).abs();
    let ten_k = Decimal::from(10_000_u32);
    let exceeded = exact_decimal_divergence_exceeds(diff, oracle_px, max_div_dec)?;
    let action = if exceeded {
        TailRiskAction::ReduceOnly
    } else {
        TailRiskAction::Normal
    };

    let mid_f = reference_mid_px
        .to_f64()
        .filter(|v| v.is_finite() && *v > 0.0)
        .context("invalid non-positive or non-finite reference mid price")?;
    let oracle_f = oracle_px
        .to_f64()
        .filter(|v| v.is_finite() && *v > 0.0)
        .context("invalid non-positive or non-finite oracle price")?;
    let div_bps = diff
        .checked_div(oracle_px)
        .and_then(|r| r.checked_mul(ten_k))
        .and_then(|d| d.to_f64())
        .filter(|v| v.is_finite())
        .unwrap_or_else(|| ((mid_f - oracle_f).abs() / oracle_f) * 1e4);
    ensure!(div_bps.is_finite(), "non-finite oracle divergence");
    Ok((div_bps, action))
}

/// Evaluate cross-venue or maker mid divergence against an oracle price (`f64`), returning
/// `TailRiskAction::ReduceOnly` when `|mid - oracle| / oracle * 1e4 > max_divergence_bps`.
pub fn evaluate_reference_oracle_divergence_f64(
    reference_mid_px: f64,
    oracle_px: f64,
    max_divergence_bps: f64,
) -> Result<(f64, TailRiskAction)> {
    ensure!(
        max_divergence_bps.is_finite() && max_divergence_bps > 0.0,
        "max_divergence_bps must be finite and > 0"
    );
    ensure!(
        reference_mid_px.is_finite() && reference_mid_px > 0.0,
        "reference_mid_px must be finite and > 0"
    );
    ensure!(
        oracle_px.is_finite() && oracle_px > 0.0,
        "oracle_px must be finite and > 0"
    );
    let div_bps = ((reference_mid_px - oracle_px).abs() / oracle_px) * 1e4;
    ensure!(div_bps.is_finite(), "non-finite oracle divergence");
    let action = if div_bps > max_divergence_bps {
        TailRiskAction::ReduceOnly
    } else {
        TailRiskAction::Normal
    };
    Ok((div_bps, action))
}

/// Independent reference oracle price observation carrying distinct source & receipt clocks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferenceOracleSample {
    pub source_id: String,
    pub price: Decimal,
    pub source_ms: u64,
    pub receipt_ms: u64,
}

/// Multi-source oracle consensus and staleness policy.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MultiOraclePolicy {
    pub min_sources: usize,
    pub max_oracle_age_ms: u64,
    pub max_future_skew_ms: u64,
    pub max_inter_source_spread_bps: f64,
}

impl Default for MultiOraclePolicy {
    fn default() -> Self {
        Self {
            min_sources: 2,
            max_oracle_age_ms: 2_000,
            max_future_skew_ms: 2_000,
            max_inter_source_spread_bps: 35.0,
        }
    }
}

impl MultiOraclePolicy {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.min_sources >= 1,
            "multi_oracle.min_sources must be >= 1"
        );
        ensure!(
            self.max_oracle_age_ms > 0 && self.max_future_skew_ms <= 30_000,
            "multi_oracle age > 0 and future skew <= 30_000 ms required"
        );
        ensure!(
            self.max_inter_source_spread_bps.is_finite() && self.max_inter_source_spread_bps > 0.0,
            "multi_oracle.max_inter_source_spread_bps must be finite and > 0"
        );
        Ok(())
    }
}

/// Verified median consensus computed across fresh independent oracle sources.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MultiOracleConsensus {
    pub median_price: Decimal,
    pub valid_sources: usize,
    pub inter_source_spread_bps: f64,
    pub newest_receipt_ms: u64,
}

/// Compute a manipulation-resistant exact Decimal median over independent oracle feeds,
/// failing closed on duplicate/future/stale feeds, insufficient quorum, or wide spread.
pub fn evaluate_multi_source_oracle_median(
    now_ms: u64,
    samples: &[ReferenceOracleSample],
    policy: &MultiOraclePolicy,
) -> Result<MultiOracleConsensus> {
    ensure!(now_ms > 0, "now_ms must be > 0");
    policy.validate()?;

    let mut seen_sources = HashSet::with_capacity(samples.len());
    let mut fresh_prices = Vec::with_capacity(samples.len());
    let mut newest_receipt_ms = 0_u64;

    for sample in samples {
        let id_norm = sample.source_id.trim().to_ascii_lowercase();
        ensure!(
            !id_norm.is_empty(),
            "reference oracle source_id must be non-empty"
        );
        ensure!(
            seen_sources.insert(id_norm),
            "duplicate reference oracle source_id '{}'",
            sample.source_id
        );
        ensure!(
            sample.price > Decimal::ZERO,
            "reference oracle '{}' price must be > 0",
            sample.source_id
        );
        ensure!(
            sample.receipt_ms > 0 && sample.receipt_ms <= now_ms,
            "reference oracle '{}' invalid or future receipt_ms ({} vs {})",
            sample.source_id,
            sample.receipt_ms,
            now_ms
        );
        ensure!(
            sample.source_ms > 0
                && sample.source_ms <= now_ms.saturating_add(policy.max_future_skew_ms),
            "reference oracle '{}' invalid or future source_ms ({} vs {})",
            sample.source_id,
            sample.source_ms,
            now_ms
        );

        let receipt_stale = now_ms.saturating_sub(sample.receipt_ms) > policy.max_oracle_age_ms;
        let source_stale = now_ms.saturating_sub(sample.source_ms)
            > policy
                .max_oracle_age_ms
                .saturating_add(policy.max_future_skew_ms);
        if receipt_stale || source_stale {
            continue;
        }
        newest_receipt_ms = newest_receipt_ms.max(sample.receipt_ms);
        fresh_prices.push(sample.price);
    }

    ensure!(
        fresh_prices.len() >= policy.min_sources,
        "insufficient fresh reference oracle sources: {} < {}",
        fresh_prices.len(),
        policy.min_sources
    );

    fresh_prices.sort();
    let mid = fresh_prices.len() / 2;
    let median_price = if fresh_prices.len() % 2 == 1 {
        fresh_prices[mid]
    } else {
        (fresh_prices[mid - 1] + fresh_prices[mid]) * Decimal::new(5, 1)
    };
    ensure!(median_price > Decimal::ZERO, "oracle median must be > 0");

    let spread = fresh_prices[fresh_prices.len() - 1] - fresh_prices[0];
    let spread_limit_dec = Decimal::try_from(policy.max_inter_source_spread_bps)
        .context("max_inter_source_spread_bps exceeds Decimal precision")?;
    ensure!(
        !exact_decimal_divergence_exceeds(spread, median_price, spread_limit_dec)?,
        "inter-source oracle spread exceeds {:.2} bps across {} sources",
        policy.max_inter_source_spread_bps,
        fresh_prices.len()
    );

    let inter_source_spread_bps = spread
        .checked_div(median_price)
        .and_then(|r| r.checked_mul(Decimal::from(10_000)))
        .and_then(|v| v.to_f64())
        .filter(|v| v.is_finite())
        .unwrap_or_else(|| {
            let s = spread.to_f64().unwrap_or(0.0);
            let m = median_price.to_f64().unwrap_or(1.0).max(1e-12);
            (s / m) * 1e4
        });

    Ok(MultiOracleConsensus {
        median_price,
        valid_sources: fresh_prices.len(),
        inter_source_spread_bps,
        newest_receipt_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generic_preflight_enforces_delisting_discount_mode_and_fee_scale() {
        let policy = VenuePreflightPolicy {
            require_fee_discount_mode: Some("enabled".into()),
            max_fee_scale: 1.0,
        };
        let ok = VenueMarketMetadata {
            venue_id: "venue_a".into(),
            symbol: "SYM-PERP".into(),
            is_delisted: false,
            fee_discount_mode: Some("enabled".into()),
            fee_scale: Some(1.0),
            oracle_price: Some(Decimal::from(100)),
        };
        assert!(validate_venue_market_preflight(&ok, &policy).is_ok());

        let mut delisted = ok.clone();
        delisted.is_delisted = true;
        assert!(validate_venue_market_preflight(&delisted, &policy).is_err());

        let mut wrong_mode = ok.clone();
        wrong_mode.fee_discount_mode = Some("disabled".into());
        assert!(validate_venue_market_preflight(&wrong_mode, &policy).is_err());

        let mut high_scale = ok;
        high_scale.fee_scale = Some(1.5);
        assert!(validate_venue_market_preflight(&high_scale, &policy).is_err());
    }

    #[test]
    fn reference_oracle_divergence_blocks_entries_and_fails_closed() {
        let (ok_bps, ok_act) =
            evaluate_reference_oracle_divergence_f64(100.10, 100.0, 25.0).unwrap();
        assert!((ok_bps - 10.0).abs() < 1e-6);
        assert_eq!(ok_act, TailRiskAction::Normal);

        let (bad_bps, bad_act) =
            evaluate_reference_oracle_divergence_f64(100.30, 100.0, 25.0).unwrap();
        assert!((bad_bps - 30.0).abs() < 1e-6);
        assert_eq!(bad_act, TailRiskAction::ReduceOnly);

        assert!(evaluate_reference_oracle_divergence_f64(0.0, 100.0, 25.0).is_err());
        assert!(evaluate_reference_oracle_divergence_f64(100.0, f64::NAN, 25.0).is_err());
        assert!(
            evaluate_reference_oracle_divergence_bps(Decimal::ZERO, Decimal::from(100), 25.0)
                .is_err()
        );
        assert!(
            evaluate_reference_oracle_divergence_bps(
                Decimal::from(100),
                Decimal::from(100),
                f64::NAN
            )
            .is_err()
        );
    }

    #[test]
    fn exact_decimal_oracle_divergence_boundary_avoids_f64_false_reject() {
        let oracle = Decimal::from(99);
        let exact_bound = Decimal::from_str_exact("99.24750").expect("valid decimal literal");
        let (div_eq, act_eq) =
            evaluate_reference_oracle_divergence_bps(exact_bound, oracle, 25.0).unwrap();
        assert!((div_eq - 25.0).abs() < 1e-12);
        assert_eq!(act_eq, TailRiskAction::Normal);

        let just_over = Decimal::from_str_exact("99.24751").expect("valid decimal literal");
        let (div_over, act_over) =
            evaluate_reference_oracle_divergence_bps(just_over, oracle, 25.0).unwrap();
        assert!(div_over > 25.0);
        assert_eq!(act_over, TailRiskAction::ReduceOnly);

        let lower_exact = Decimal::from_str_exact("98.75250").expect("valid decimal literal");
        let (div_low_eq, act_low_eq) =
            evaluate_reference_oracle_divergence_bps(lower_exact, oracle, 25.0).unwrap();
        assert!((div_low_eq - 25.0).abs() < 1e-12);
        assert_eq!(act_low_eq, TailRiskAction::Normal);

        let lower_just_over = Decimal::from_str_exact("98.75249").expect("valid decimal literal");
        let (div_low_over, act_low_over) =
            evaluate_reference_oracle_divergence_bps(lower_just_over, oracle, 25.0).unwrap();
        assert!(div_low_over > 25.0);
        assert_eq!(act_low_over, TailRiskAction::ReduceOnly);

        let upper_exact = Decimal::from_str_exact("100.25").expect("valid decimal literal");
        let (_, act_upper_eq) =
            evaluate_reference_oracle_divergence_bps(upper_exact, Decimal::from(100), 25.0)
                .unwrap();
        assert_eq!(act_upper_eq, TailRiskAction::Normal);

        let upper_over = Decimal::from_str_exact("100.25001").expect("valid decimal literal");
        let (_, act_upper_over) =
            evaluate_reference_oracle_divergence_bps(upper_over, Decimal::from(100), 25.0).unwrap();
        assert_eq!(act_upper_over, TailRiskAction::ReduceOnly);

        let non_dyadic_eq = Decimal::from_str_exact("100.003").expect("valid decimal literal");
        let (_, act_nd_eq) =
            evaluate_reference_oracle_divergence_bps(non_dyadic_eq, Decimal::from(100), 0.3)
                .unwrap();
        assert_eq!(act_nd_eq, TailRiskAction::Normal);

        // #692 counterexample: extreme 4e28 magnitudes where checked_mul overflows and lossy
        // checked_div rounded +1 unit breach to equality -> U320 exact cross-product preserves +/-1.
        let huge_oracle =
            Decimal::from_str_exact("40000000000000000000000000000").expect("valid 4e28 decimal");
        let huge_eq =
            Decimal::from_str_exact("40100000000000000000000000000").expect("valid eq decimal");
        let huge_under =
            Decimal::from_str_exact("40099999999999999999999999999").expect("valid -1 decimal");
        let huge_over =
            Decimal::from_str_exact("40100000000000000000000000001").expect("valid +1 decimal");
        assert_eq!(
            evaluate_reference_oracle_divergence_bps(huge_eq, huge_oracle, 25.0)
                .unwrap()
                .1,
            TailRiskAction::Normal
        );
        assert_eq!(
            evaluate_reference_oracle_divergence_bps(huge_under, huge_oracle, 25.0)
                .unwrap()
                .1,
            TailRiskAction::Normal
        );
        assert_eq!(
            evaluate_reference_oracle_divergence_bps(huge_over, huge_oracle, 25.0)
                .unwrap()
                .1,
            TailRiskAction::ReduceOnly
        );
    }

    #[test]
    fn multi_source_oracle_median_resists_single_outlier_and_rejects_stale_or_split() {
        let now_ms = 100_000_u64;
        let policy = MultiOraclePolicy {
            min_sources: 2,
            max_oracle_age_ms: 2_000,
            max_future_skew_ms: 2_000,
            max_inter_source_spread_bps: 30.0,
        };
        let d = |s: &str| Decimal::from_str_exact(s).expect("valid decimal");

        // 3 healthy fresh sources -> exact median 100.05
        let consensus = evaluate_multi_source_oracle_median(
            now_ms,
            &[
                ReferenceOracleSample {
                    source_id: "src_a".into(),
                    price: d("100.00"),
                    source_ms: 99_500,
                    receipt_ms: 99_600,
                },
                ReferenceOracleSample {
                    source_id: "src_b".into(),
                    price: d("100.10"),
                    source_ms: 99_550,
                    receipt_ms: 99_700,
                },
                ReferenceOracleSample {
                    source_id: "src_c".into(),
                    price: d("100.05"),
                    source_ms: 99_520,
                    receipt_ms: 99_650,
                },
            ],
            &policy,
        )
        .unwrap();
        assert_eq!(consensus.median_price, d("100.05"));
        assert_eq!(consensus.valid_sources, 3);
        assert!(consensus.inter_source_spread_bps < 11.0);

        // Stale outlier ignored while 2 remaining fresh sources still form quorum.
        let filtered = evaluate_multi_source_oracle_median(
            now_ms,
            &[
                ReferenceOracleSample {
                    source_id: "src_a".into(),
                    price: d("100.00"),
                    source_ms: 99_500,
                    receipt_ms: 99_600,
                },
                ReferenceOracleSample {
                    source_id: "src_b".into(),
                    price: d("100.10"),
                    source_ms: 99_550,
                    receipt_ms: 99_700,
                },
                ReferenceOracleSample {
                    source_id: "src_stale".into(),
                    price: d("150.00"),
                    source_ms: 90_000,
                    receipt_ms: 95_000, // age 5s > 2s
                },
            ],
            &policy,
        )
        .unwrap();
        assert_eq!(filtered.median_price, d("100.05"));
        assert_eq!(filtered.valid_sources, 2);

        // Manipulated wide spread between fresh sources fails closed.
        assert!(
            evaluate_multi_source_oracle_median(
                now_ms,
                &[
                    ReferenceOracleSample {
                        source_id: "src_a".into(),
                        price: d("100.00"),
                        source_ms: 99_500,
                        receipt_ms: 99_600,
                    },
                    ReferenceOracleSample {
                        source_id: "src_b".into(),
                        price: d("101.00"),
                        source_ms: 99_500,
                        receipt_ms: 99_600,
                    },
                ],
                &policy,
            )
            .is_err()
        );

        // Duplicate source IDs or future receipt clock fail closed.
        assert!(
            evaluate_multi_source_oracle_median(
                now_ms,
                &[
                    ReferenceOracleSample {
                        source_id: "src_a".into(),
                        price: d("100.00"),
                        source_ms: 99_500,
                        receipt_ms: 99_600,
                    },
                    ReferenceOracleSample {
                        source_id: "SRC_A".into(),
                        price: d("100.00"),
                        source_ms: 99_500,
                        receipt_ms: 99_600,
                    },
                ],
                &policy,
            )
            .is_err()
        );
    }
}
