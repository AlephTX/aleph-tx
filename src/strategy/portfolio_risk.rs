//! Deterministic portfolio risk-budgeting and crash stress helpers (`CRASH-IMP-03`).
//!
//! Pure Rust port of `scripts/portfolio_risk_budget.py` with fail-closed validation:
//! - Inverse-volatility weights and water-filling single-leg weight caps
//! - Crisis correlation floor (`stress_rho_floor`) & Euler risk contributions
//! - Non-normal empirical Value-at-Risk (VaR) and Expected Shortfall (ES / CVaR)
//! - Strictly capped fractional Kelly sizing (`<= max_scale <= 1.0`, `0.0` if `mean <= 0`)
//! - Real-time drawdown brake mirror of [`crate::daily_loss::DailyLossGuard`]
//! - 5-dimension perp stress evaluator and carry capital budget allocator

use serde::{Deserialize, Serialize};
use std::fmt;

/// Validation and computation errors returned fail-closed by portfolio risk helpers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortfolioRiskError {
    /// Input slice was empty where at least one element is required.
    EmptyInput(&'static str),
    /// Two inputs had incompatible lengths.
    LengthMismatch {
        context: &'static str,
        expected: usize,
        actual: usize,
    },
    /// Scalar input was non-finite or outside its valid domain.
    InvalidValue {
        field: &'static str,
        reason: &'static str,
    },
    /// Element at `index` in a slice was non-finite or outside its valid domain.
    InvalidElement {
        field: &'static str,
        index: usize,
        reason: &'static str,
    },
    /// Correlation matrix failed squareness, finiteness, range, diagonal, or symmetry checks.
    InvalidCorrelationMatrix {
        row: usize,
        col: usize,
        reason: String,
    },
    /// Portfolio variance is non-positive or non-finite (e.g., all weights are zero).
    NonPositivePortfolioVariance,
    /// Per-leg weight cap cannot be satisfied for the given leg count.
    InfeasibleWeightCap { n_legs: usize, detail: String },
}

impl fmt::Display for PortfolioRiskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyInput(field) => write!(f, "{field} must be non-empty"),
            Self::LengthMismatch {
                context,
                expected,
                actual,
            } => write!(f, "{context}: expected length {expected}, got {actual}"),
            Self::InvalidValue { field, reason } => write!(f, "invalid {field}: {reason}"),
            Self::InvalidElement {
                field,
                index,
                reason,
            } => write!(f, "invalid {field}[{index}]: {reason}"),
            Self::InvalidCorrelationMatrix { row, col, reason } => {
                write!(f, "invalid correlation matrix at ({row}, {col}): {reason}")
            }
            Self::NonPositivePortfolioVariance => {
                write!(f, "portfolio variance must be positive and finite")
            }
            Self::InfeasibleWeightCap { n_legs, detail } => {
                write!(f, "infeasible weight cap for {n_legs} legs: {detail}")
            }
        }
    }
}

impl std::error::Error for PortfolioRiskError {}

/// Parameters controlling the 5-dimension perp crash stress evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PerpStressParams {
    /// Adverse basis widening shock in basis points (`>= 0.0`).
    pub basis_shock_bps: f64,
    /// Base hedge-lag execution slippage in basis points (`>= 0.0`).
    pub hedge_lag_slip_bps: f64,
    /// Fraction of resting L1-L3 book depth lost during stress (`[0.0, 1.0)`).
    pub depth_drop_frac: f64,
    /// Adverse 10-hour funding flip shock in basis points (`>= 0.0`).
    pub funding_flip_bps_10h: f64,
    /// Minimum off-diagonal correlation imposed under stress (`[0.0, 1.0]`).
    pub stress_rho_floor: f64,
}

impl Default for PerpStressParams {
    fn default() -> Self {
        Self {
            basis_shock_bps: 40.0,
            hedge_lag_slip_bps: 15.0,
            depth_drop_frac: 0.80,
            funding_flip_bps_10h: 10.0,
            stress_rho_floor: 0.70,
        }
    }
}

/// Output metrics from [`evaluate_perp_stress_scenarios`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PerpStressReport {
    pub base_portfolio_vol: f64,
    pub stressed_portfolio_vol: f64,
    pub vol_stress_multiplier: f64,
    pub max_stressed_rc: f64,
    pub basis_shock_loss_usd: f64,
    pub thin_hedge_lag_loss_usd: f64,
    pub funding_flip_10h_loss_usd: f64,
    pub combined_crash_loss_usd: f64,
}

/// Configuration for [`allocate_carry_budgets`].
///
/// [`Default`] populates standard risk-budgeting parameters while leaving capital and loss
/// limits at `0.0` so unconfigured deployments fail closed until explicitly sized.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CarryBudgetConfig {
    pub max_gross_notional_usd: f64,
    pub max_single_leg_weight: f64,
    pub max_es_loss_usd: f64,
    pub es_alpha: f64,
    pub stress_rho_floor: f64,
    pub kelly_fraction: f64,
    pub max_kelly_scale: f64,
    pub brake_scale: f64,
}

impl Default for CarryBudgetConfig {
    fn default() -> Self {
        Self {
            max_gross_notional_usd: 0.0,
            max_single_leg_weight: 1.0,
            max_es_loss_usd: 0.0,
            es_alpha: 0.975,
            stress_rho_floor: 0.70,
            kelly_fraction: 0.25,
            max_kelly_scale: 1.0,
            brake_scale: 1.0,
        }
    }
}

/// Resulting portfolio weights, risk contributions, and USD notionals from
/// [`allocate_carry_budgets`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CarryBudgetAllocation {
    pub weights: Vec<f64>,
    pub leg_notionals_usd: Vec<f64>,
    pub allocated_gross_notional_usd: f64,
    pub base_portfolio_vol: f64,
    pub stressed_portfolio_vol: f64,
    pub stressed_risk_contributions: Vec<f64>,
    pub empirical_var_frac: f64,
    pub empirical_es_frac: f64,
    pub kelly_scale: f64,
    pub es_budget_scale: f64,
    pub effective_scale: f64,
}

/// JSON-deserializable scenario input for [`evaluate_portfolio_risk_scenario`].
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortfolioRiskScenario {
    pub vols: Vec<f64>,
    pub corr: Vec<Vec<f64>>,
    pub historical_portfolio_returns: Vec<f64>,
    pub mean_return: f64,
    pub variance: f64,
    pub config: CarryBudgetConfig,
    #[serde(default)]
    pub stress_params: PerpStressParams,
}

/// Combined risk-budget allocation and perp crash stress output from
/// [`evaluate_portfolio_risk_scenario`].
#[derive(Debug, Clone, Serialize)]
pub struct PortfolioRiskEvaluation {
    pub allocation: CarryBudgetAllocation,
    pub stress_on_max_notional: PerpStressReport,
    pub stress_on_allocated_notional: Option<PerpStressReport>,
}

fn check_positive_finite(val: f64, field: &'static str) -> Result<(), PortfolioRiskError> {
    if !val.is_finite() || val <= 0.0 {
        return Err(PortfolioRiskError::InvalidValue {
            field,
            reason: "must be positive and finite",
        });
    }
    Ok(())
}

fn check_non_negative_finite(val: f64, field: &'static str) -> Result<(), PortfolioRiskError> {
    if !val.is_finite() || val < 0.0 {
        return Err(PortfolioRiskError::InvalidValue {
            field,
            reason: "must be non-negative and finite",
        });
    }
    Ok(())
}

fn check_unit_closed(val: f64, field: &'static str) -> Result<(), PortfolioRiskError> {
    if !val.is_finite() || !(0.0..=1.0).contains(&val) {
        return Err(PortfolioRiskError::InvalidValue {
            field,
            reason: "must be finite and in [0.0, 1.0]",
        });
    }
    Ok(())
}

fn check_unit_half_open_left(val: f64, field: &'static str) -> Result<(), PortfolioRiskError> {
    if !val.is_finite() || val <= 0.0 || val > 1.0 {
        return Err(PortfolioRiskError::InvalidValue {
            field,
            reason: "must be finite and in (0.0, 1.0]",
        });
    }
    Ok(())
}

fn validate_positive_finite_slice(
    values: &[f64],
    field: &'static str,
) -> Result<(), PortfolioRiskError> {
    if values.is_empty() {
        return Err(PortfolioRiskError::EmptyInput(field));
    }
    for (index, &val) in values.iter().enumerate() {
        if !val.is_finite() || val <= 0.0 {
            return Err(PortfolioRiskError::InvalidElement {
                field,
                index,
                reason: "must be positive and finite",
            });
        }
    }
    Ok(())
}

fn validate_corr_matrix(corr: &[Vec<f64>], expected_n: usize) -> Result<(), PortfolioRiskError> {
    if corr.is_empty() {
        return Err(PortfolioRiskError::EmptyInput("corr"));
    }
    if corr.len() != expected_n {
        return Err(PortfolioRiskError::LengthMismatch {
            context: "correlation matrix row count",
            expected: expected_n,
            actual: corr.len(),
        });
    }
    for (row_idx, row) in corr.iter().enumerate() {
        if row.len() != expected_n {
            return Err(PortfolioRiskError::InvalidCorrelationMatrix {
                row: row_idx,
                col: row.len(),
                reason: format!("row length {} != {expected_n}", row.len()),
            });
        }
    }
    for (i, row) in corr.iter().enumerate() {
        let diag = row[i];
        if !diag.is_finite() || (diag - 1.0).abs() > 1e-6 {
            return Err(PortfolioRiskError::InvalidCorrelationMatrix {
                row: i,
                col: i,
                reason: "diagonal entry must equal 1.0 within 1e-6".to_string(),
            });
        }
        for (j, &val) in row.iter().enumerate() {
            if !val.is_finite() || val < -1.0 - 1e-9 || val > 1.0 + 1e-9 {
                return Err(PortfolioRiskError::InvalidCorrelationMatrix {
                    row: i,
                    col: j,
                    reason: "entry must be finite and within [-1.0, 1.0]".to_string(),
                });
            }
            if (val - corr[j][i]).abs() > 1e-6 {
                return Err(PortfolioRiskError::InvalidCorrelationMatrix {
                    row: i,
                    col: j,
                    reason: "matrix must be symmetric within 1e-6".to_string(),
                });
            }
        }
    }
    Ok(())
}

/// Compute normalized inverse-volatility weights summing to `1.0`.
pub fn inverse_vol_weights(vols: &[f64]) -> Result<Vec<f64>, PortfolioRiskError> {
    validate_positive_finite_slice(vols, "vols")?;
    let inv: Vec<f64> = vols.iter().map(|&v| 1.0 / v).collect();
    let denom: f64 = inv.iter().sum();
    if !denom.is_finite() || denom <= 0.0 {
        return Err(PortfolioRiskError::InvalidValue {
            field: "vols",
            reason: "inverse volatility sum must be positive and finite",
        });
    }
    Ok(inv.into_iter().map(|x| x / denom).collect())
}

/// Apply an iterative water-filling single-leg cap so all weights are `<= max_single_leg_weight`
/// and sum to `1.0`.
///
/// Fails closed if `weights.len() as f64 * max_single_leg_weight < 1.0 - 1e-12`, since weights
/// summing to `1.0` cannot satisfy an infeasible cap.
pub fn cap_and_renormalize_weights(
    weights: &[f64],
    max_single_leg_weight: f64,
) -> Result<Vec<f64>, PortfolioRiskError> {
    if weights.is_empty() {
        return Err(PortfolioRiskError::EmptyInput("weights"));
    }
    check_unit_half_open_left(max_single_leg_weight, "max_single_leg_weight")?;
    for (index, &w) in weights.iter().enumerate() {
        if !w.is_finite() || w < 0.0 {
            return Err(PortfolioRiskError::InvalidElement {
                field: "weights",
                index,
                reason: "must be non-negative and finite",
            });
        }
    }

    let n = weights.len();
    let cap_sum = (n as f64) * max_single_leg_weight;
    if cap_sum < 1.0 - 1e-12 {
        return Err(PortfolioRiskError::InfeasibleWeightCap {
            n_legs: n,
            detail: format!("n * max_single_leg_weight ({cap_sum}) < 1.0"),
        });
    }

    let init_sum: f64 = weights.iter().sum();
    if !init_sum.is_finite() || init_sum <= 0.0 {
        return Err(PortfolioRiskError::InvalidValue {
            field: "weights",
            reason: "sum of weights must be positive and finite",
        });
    }

    let mut capped_weights: Vec<f64> = weights.iter().map(|&w| w / init_sum).collect();
    let mut is_capped = vec![false; n];

    for _ in 0..=n {
        let mut newly_capped = false;
        for (weight, capped) in capped_weights.iter_mut().zip(is_capped.iter_mut()) {
            if !*capped && *weight > max_single_leg_weight {
                *weight = max_single_leg_weight;
                *capped = true;
                newly_capped = true;
            }
        }
        if !newly_capped {
            break;
        }

        let capped_count = is_capped.iter().filter(|&&c| c).count();
        if capped_count == n {
            let equal = 1.0 / (n as f64);
            return Ok(vec![equal; n]);
        }

        let remaining = (1.0 - (capped_count as f64) * max_single_leg_weight).max(0.0);
        let uncapped_sum: f64 = capped_weights
            .iter()
            .zip(is_capped.iter())
            .filter(|(_, c)| !**c)
            .map(|(&w, _)| w)
            .sum();

        if !uncapped_sum.is_finite() || uncapped_sum <= 0.0 {
            let uncapped_count = n - capped_count;
            let share = remaining / (uncapped_count as f64);
            for (weight, &capped) in capped_weights.iter_mut().zip(is_capped.iter()) {
                if !capped {
                    *weight = share;
                }
            }
        } else {
            let scale = remaining / uncapped_sum;
            for (weight, &capped) in capped_weights.iter_mut().zip(is_capped.iter()) {
                if !capped {
                    *weight *= scale;
                }
            }
        }
    }

    Ok(capped_weights)
}

/// Lift off-diagonal correlations to at least `stress_rho_floor` for crisis sizing.
pub fn stressed_corr_matrix(
    corr: &[Vec<f64>],
    stress_rho_floor: f64,
) -> Result<Vec<Vec<f64>>, PortfolioRiskError> {
    check_unit_closed(stress_rho_floor, "stress_rho_floor")?;
    validate_corr_matrix(corr, corr.len())?;

    let stressed = corr
        .iter()
        .enumerate()
        .map(|(i, row)| {
            row.iter()
                .enumerate()
                .map(|(j, &val)| {
                    if i == j {
                        1.0
                    } else {
                        val.max(stress_rho_floor)
                    }
                })
                .collect()
        })
        .collect();
    Ok(stressed)
}

/// Compute `(portfolio_vol, fractional_risk_contributions)` under correlation matrix `corr`.
pub fn portfolio_vol_and_rc(
    weights: &[f64],
    vols: &[f64],
    corr: &[Vec<f64>],
) -> Result<(f64, Vec<f64>), PortfolioRiskError> {
    validate_positive_finite_slice(vols, "vols")?;
    let n = vols.len();
    if weights.len() != n {
        return Err(PortfolioRiskError::LengthMismatch {
            context: "weights and vols",
            expected: n,
            actual: weights.len(),
        });
    }
    for (index, &w) in weights.iter().enumerate() {
        if !w.is_finite() || w < 0.0 {
            return Err(PortfolioRiskError::InvalidElement {
                field: "weights",
                index,
                reason: "must be non-negative and finite",
            });
        }
    }
    validate_corr_matrix(corr, n)?;

    let mut cov_w = Vec::with_capacity(n);
    for (&vol_i, corr_row) in vols.iter().zip(corr.iter()) {
        let acc: f64 = vols
            .iter()
            .zip(corr_row.iter())
            .zip(weights.iter())
            .map(|((&vol_j, &rho_ij), &w_j)| vol_i * vol_j * rho_ij * w_j)
            .sum();
        cov_w.push(acc);
    }

    let port_var: f64 = weights
        .iter()
        .zip(cov_w.iter())
        .map(|(&w, &cw)| w * cw)
        .sum();
    if !port_var.is_finite() || port_var <= 0.0 {
        return Err(PortfolioRiskError::NonPositivePortfolioVariance);
    }

    let port_vol = port_var.sqrt();
    let rcs = weights
        .iter()
        .zip(&cov_w)
        .map(|(&w, &cw)| (w * cw) / port_var)
        .collect();
    Ok((port_vol, rcs))
}

/// Compute empirical left-tail `(var_loss, es_loss)` as positive loss magnitudes.
pub fn empirical_var_es(returns: &[f64], alpha: f64) -> Result<(f64, f64), PortfolioRiskError> {
    if returns.is_empty() {
        return Err(PortfolioRiskError::EmptyInput("returns"));
    }
    if !alpha.is_finite() || alpha <= 0.5 || alpha >= 1.0 {
        return Err(PortfolioRiskError::InvalidValue {
            field: "alpha",
            reason: "must be finite and strictly in (0.5, 1.0)",
        });
    }

    let mut losses = Vec::with_capacity(returns.len());
    for (index, &r) in returns.iter().enumerate() {
        if !r.is_finite() {
            return Err(PortfolioRiskError::InvalidElement {
                field: "returns",
                index,
                reason: "must be finite",
            });
        }
        losses.push(-r);
    }

    losses.sort_by(|a, b| b.total_cmp(a));
    let tail_fraction = ((1.0 - alpha) * returns.len() as f64 * 1e12).round() / 1e12;
    let tail_count = (tail_fraction.ceil() as usize).clamp(1, losses.len());
    let var_loss = losses[tail_count - 1];
    let es_loss = losses[..tail_count].iter().sum::<f64>() / (tail_count as f64);
    Ok((var_loss, es_loss))
}

/// Compute fractional Kelly scaling strictly capped in `[0.0, max_scale]` (`max_scale <= 1.0`).
///
/// Returns `0.0` when `mean_return <= 0.0` or `variance <= 0.0`.
pub fn capped_fractional_kelly(
    mean_return: f64,
    variance: f64,
    fraction: f64,
    max_scale: f64,
) -> Result<f64, PortfolioRiskError> {
    if !fraction.is_finite() || fraction <= 0.0 || fraction > 0.5 {
        return Err(PortfolioRiskError::InvalidValue {
            field: "fraction",
            reason: "must be finite and in (0.0, 0.5]",
        });
    }
    check_unit_half_open_left(max_scale, "max_scale")?;
    if !mean_return.is_finite() {
        return Err(PortfolioRiskError::InvalidValue {
            field: "mean_return",
            reason: "must be finite",
        });
    }
    if !variance.is_finite() {
        return Err(PortfolioRiskError::InvalidValue {
            field: "variance",
            reason: "must be finite",
        });
    }
    if mean_return <= 0.0 || variance <= 0.0 {
        return Ok(0.0);
    }

    let raw = fraction * (mean_return / variance);
    Ok(raw.clamp(0.0, max_scale))
}

/// Pure mirror of [`crate::daily_loss::DailyLossGuard`] progressive drawdown brake
/// (`CRASH-IMP-01`).
///
/// Returns `(next_tier, scale)` with `0.05` recovery hysteresis and strict cap `<= 1.0`.
pub fn real_time_drawdown_brake_scale(
    peak_equity: f64,
    equity: f64,
    daily_budget_usd: f64,
    prev_tier: u8,
    day_start_equity: f64,
) -> Result<(u8, f64), PortfolioRiskError> {
    if !peak_equity.is_finite() {
        return Err(PortfolioRiskError::InvalidValue {
            field: "peak_equity",
            reason: "must be finite",
        });
    }
    if !equity.is_finite() {
        return Err(PortfolioRiskError::InvalidValue {
            field: "equity",
            reason: "must be finite",
        });
    }
    check_positive_finite(daily_budget_usd, "daily_budget_usd")?;
    check_positive_finite(day_start_equity, "day_start_equity")?;

    if prev_tier >= 4 || equity <= 0.0 || day_start_equity - equity > daily_budget_usd {
        return Ok((4, 0.0));
    }

    let r_dd = (peak_equity - equity).max(0.0) / daily_budget_usd;
    let escalated = if r_dd >= 0.75 {
        3
    } else if r_dd >= 0.50 {
        2
    } else if r_dd >= 0.25 {
        1
    } else {
        0
    };

    let mut tier = prev_tier.min(3);
    if escalated >= tier {
        tier = escalated;
    } else {
        if tier == 3 && r_dd < 0.70 {
            tier = 2;
        }
        if tier == 2 && r_dd < 0.45 {
            tier = 1;
        }
        if tier == 1 && r_dd < 0.20 {
            tier = 0;
        }
    }

    let scale = match tier {
        0 => 1.0,
        1 => 0.75,
        2 => 0.50,
        3 => 0.25,
        _ => 0.0,
    };
    Ok((tier, scale))
}

/// Evaluate 5-dimension perp stress loss and crisis volatility amplification.
pub fn evaluate_perp_stress_scenarios(
    gross_notional_usd: f64,
    leg_vols: &[f64],
    weights: &[f64],
    corr: &[Vec<f64>],
    params: PerpStressParams,
) -> Result<PerpStressReport, PortfolioRiskError> {
    check_positive_finite(gross_notional_usd, "gross_notional_usd")?;
    check_non_negative_finite(params.basis_shock_bps, "basis_shock_bps")?;
    check_non_negative_finite(params.hedge_lag_slip_bps, "hedge_lag_slip_bps")?;
    check_non_negative_finite(params.funding_flip_bps_10h, "funding_flip_bps_10h")?;
    if !params.depth_drop_frac.is_finite() || !(0.0..1.0).contains(&params.depth_drop_frac) {
        return Err(PortfolioRiskError::InvalidValue {
            field: "depth_drop_frac",
            reason: "must be finite and in [0.0, 1.0)",
        });
    }
    check_unit_closed(params.stress_rho_floor, "stress_rho_floor")?;

    let (base_portfolio_vol, _) = portfolio_vol_and_rc(weights, leg_vols, corr)?;
    let stress_corr = stressed_corr_matrix(corr, params.stress_rho_floor)?;
    let (stressed_portfolio_vol, stress_rc) =
        portfolio_vol_and_rc(weights, leg_vols, &stress_corr)?;

    let vol_stress_multiplier = stressed_portfolio_vol / base_portfolio_vol;
    let max_stressed_rc = stress_rc.iter().copied().fold(f64::NEG_INFINITY, f64::max);

    let basis_shock_loss_usd = gross_notional_usd * params.basis_shock_bps / 1e4;
    // Thin-depth multiplier penalizes hedge execution slippage as L1-L3 depth evaporates.
    let thin_slip_mult = 1.0 / (1.0 - params.depth_drop_frac).max(0.10);
    let thin_hedge_lag_loss_usd =
        gross_notional_usd * params.hedge_lag_slip_bps * thin_slip_mult / 1e4;
    let funding_flip_10h_loss_usd = gross_notional_usd * params.funding_flip_bps_10h / 1e4;
    let combined_crash_loss_usd =
        basis_shock_loss_usd + thin_hedge_lag_loss_usd + funding_flip_10h_loss_usd;

    if !vol_stress_multiplier.is_finite()
        || !max_stressed_rc.is_finite()
        || !combined_crash_loss_usd.is_finite()
    {
        return Err(PortfolioRiskError::InvalidValue {
            field: "stress_report",
            reason: "stress evaluation produced non-finite output",
        });
    }

    Ok(PerpStressReport {
        base_portfolio_vol,
        stressed_portfolio_vol,
        vol_stress_multiplier,
        max_stressed_rc,
        basis_shock_loss_usd,
        thin_hedge_lag_loss_usd,
        funding_flip_10h_loss_usd,
        combined_crash_loss_usd,
    })
}

/// Allocate multi-leg carry capital budgets using capped inverse-vol weights, stressed correlation
/// Euler risk contributions, empirical ES budget scaling, capped fractional Kelly, and a
/// drawdown brake multiplier.
pub fn allocate_carry_budgets(
    vols: &[f64],
    corr: &[Vec<f64>],
    historical_portfolio_returns: &[f64],
    mean_return: f64,
    variance: f64,
    cfg: &CarryBudgetConfig,
) -> Result<CarryBudgetAllocation, PortfolioRiskError> {
    check_positive_finite(cfg.max_gross_notional_usd, "max_gross_notional_usd")?;
    check_unit_half_open_left(cfg.max_single_leg_weight, "max_single_leg_weight")?;
    check_positive_finite(cfg.max_es_loss_usd, "max_es_loss_usd")?;
    check_unit_closed(cfg.stress_rho_floor, "stress_rho_floor")?;
    check_unit_closed(cfg.brake_scale, "brake_scale")?;

    let raw_weights = inverse_vol_weights(vols)?;
    let weights = cap_and_renormalize_weights(&raw_weights, cfg.max_single_leg_weight)?;
    let (base_portfolio_vol, _) = portfolio_vol_and_rc(&weights, vols, corr)?;
    let stressed_corr = stressed_corr_matrix(corr, cfg.stress_rho_floor)?;
    let (stressed_portfolio_vol, stressed_risk_contributions) =
        portfolio_vol_and_rc(&weights, vols, &stressed_corr)?;
    let (empirical_var_frac, empirical_es_frac) =
        empirical_var_es(historical_portfolio_returns, cfg.es_alpha)?;
    let kelly_scale = capped_fractional_kelly(
        mean_return,
        variance,
        cfg.kelly_fraction,
        cfg.max_kelly_scale,
    )?;

    let es_budget_scale = if empirical_es_frac <= 0.0 {
        1.0
    } else {
        (cfg.max_es_loss_usd / (cfg.max_gross_notional_usd * empirical_es_frac)).clamp(0.0, 1.0)
    };
    let min_scale = kelly_scale.min(es_budget_scale).min(cfg.brake_scale);
    let effective_scale = min_scale.clamp(0.0, 1.0);
    let allocated_gross_notional_usd = cfg.max_gross_notional_usd * effective_scale;
    if !allocated_gross_notional_usd.is_finite() {
        return Err(PortfolioRiskError::InvalidValue {
            field: "allocated_gross_notional_usd",
            reason: "allocated notional must be finite",
        });
    }
    let gross = allocated_gross_notional_usd;
    let leg_notionals_usd: Vec<f64> = weights.iter().map(|&w| w * gross).collect();

    Ok(CarryBudgetAllocation {
        weights,
        leg_notionals_usd,
        allocated_gross_notional_usd,
        base_portfolio_vol,
        stressed_portfolio_vol,
        stressed_risk_contributions,
        empirical_var_frac,
        empirical_es_frac,
        kelly_scale,
        es_budget_scale,
        effective_scale,
    })
}

/// Evaluate carry capital budgets and perp crash stress on max and allocated gross notionals.
pub fn evaluate_portfolio_risk_scenario(
    scenario: &PortfolioRiskScenario,
) -> Result<PortfolioRiskEvaluation, PortfolioRiskError> {
    let allocation = allocate_carry_budgets(
        &scenario.vols,
        &scenario.corr,
        &scenario.historical_portfolio_returns,
        scenario.mean_return,
        scenario.variance,
        &scenario.config,
    )?;
    let stress_on_max_notional = evaluate_perp_stress_scenarios(
        scenario.config.max_gross_notional_usd,
        &scenario.vols,
        &allocation.weights,
        &scenario.corr,
        scenario.stress_params,
    )?;
    let stress_on_allocated_notional = if allocation.allocated_gross_notional_usd > 0.0 {
        Some(evaluate_perp_stress_scenarios(
            allocation.allocated_gross_notional_usd,
            &scenario.vols,
            &allocation.weights,
            &scenario.corr,
            scenario.stress_params,
        )?)
    } else {
        None
    };
    Ok(PortfolioRiskEvaluation {
        allocation,
        stress_on_max_notional,
        stress_on_allocated_notional,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_close(actual: f64, expected: f64, tol: f64) {
        assert!(
            actual.is_finite() && (actual - expected).abs() <= tol,
            "expected {expected} +/- {tol}, got {actual}"
        );
    }

    #[test]
    fn inverse_vol_weights_and_euler_risk_contributions() {
        let vols = [0.02, 0.04, 0.04];
        let w = inverse_vol_weights(&vols).expect("valid vols");
        assert_close(w.iter().sum(), 1.0, 1e-12);
        assert_close(w[0], 0.5, 1e-12);
        assert_close(w[1], 0.25, 1e-12);
        assert_close(w[2], 0.25, 1e-12);

        let eye = vec![
            vec![1.0, 0.0, 0.0],
            vec![0.0, 1.0, 0.0],
            vec![0.0, 0.0, 1.0],
        ];
        let (port_vol, rcs) = portfolio_vol_and_rc(&w, &vols, &eye).expect("valid portfolio");
        assert_close(port_vol, 0.017320508075689, 1e-12);
        assert_close(rcs.iter().sum(), 1.0, 1e-12);
        for rc in rcs {
            assert_close(rc, 1.0 / 3.0, 1e-12);
        }
    }

    #[test]
    fn crisis_correlation_floor_lifts_portfolio_vol() {
        let corr = vec![vec![1.0, 0.1], vec![0.1, 1.0]];
        let vols = [0.02, 0.02];
        let w = [0.5, 0.5];
        let (base_vol, _) = portfolio_vol_and_rc(&w, &vols, &corr).expect("base vol");
        let stressed = stressed_corr_matrix(&corr, 0.70).expect("stressed corr");
        assert_close(stressed[0][0], 1.0, 1e-12);
        assert_close(stressed[0][1], 0.70, 1e-12);
        assert_close(stressed[1][0], 0.70, 1e-12);
        let (stress_vol, rcs) = portfolio_vol_and_rc(&w, &vols, &stressed).expect("stress vol");
        assert!(stress_vol > base_vol);
        assert_close(rcs.iter().sum(), 1.0, 1e-12);
    }

    #[test]
    fn empirical_var_es_captures_fat_left_tail() {
        let mut returns = vec![0.001; 39];
        returns.push(-0.12);

        let (var_975, es_975) = empirical_var_es(&returns, 0.975).expect("97.5% VaR/ES");
        assert_close(var_975, 0.12, 1e-12);
        assert_close(es_975, 0.12, 1e-12);

        let (var_90, es_90) = empirical_var_es(&returns, 0.90).expect("90% VaR/ES");
        assert_close(var_90, -0.001, 1e-12);
        assert_close(es_90, 0.02925, 1e-12);
        assert!(es_90 >= var_90);

        let (single_var, single_es) = empirical_var_es(&[-0.03], 0.99).expect("single return");
        assert_close(single_var, 0.03, 1e-12);
        assert_close(single_es, 0.03, 1e-12);
    }

    #[test]
    fn capped_fractional_kelly_zero_on_non_positive_and_respects_ceiling() {
        let zero_mean = capped_fractional_kelly(0.0, 0.01, 0.25, 1.0).unwrap();
        assert_close(zero_mean, 0.0, 1e-12);

        let neg_mean = capped_fractional_kelly(-0.005, 0.01, 0.25, 1.0).unwrap();
        assert_close(neg_mean, 0.0, 1e-12);

        let zero_var = capped_fractional_kelly(0.01, 0.0, 0.25, 1.0).unwrap();
        assert_close(zero_var, 0.0, 1e-12);

        let neg_var = capped_fractional_kelly(0.01, -0.01, 0.25, 1.0).unwrap();
        assert_close(neg_var, 0.0, 1e-12);

        let capped_one = capped_fractional_kelly(0.20, 0.01, 0.25, 1.0).unwrap();
        assert_close(capped_one, 1.0, 1e-12);

        let capped_sub = capped_fractional_kelly(0.20, 0.01, 0.25, 0.6).unwrap();
        assert_close(capped_sub, 0.6, 1e-12);

        let interior = capped_fractional_kelly(0.01, 0.01, 0.25, 1.0).unwrap();
        assert_close(interior, 0.25, 1e-12);
    }

    #[test]
    fn drawdown_brake_parity_with_hysteresis_and_sticky_breach() {
        for (equity, prior, exp_tier, exp_scale) in [
            (500.0, 0, 3, 0.25),
            (475.0, 3, 3, 0.25),
            (474.0, 3, 4, 0.0),
            (510.0, 4, 4, 0.0),
        ] {
            let (tier, scale) = real_time_drawdown_brake_scale(530.0, equity, 25.0, prior, 500.0)
                .expect("valid brake args");
            assert_eq!(tier, exp_tier);
            assert_close(scale, exp_scale, 1e-12);
        }

        let (t1, s1) = real_time_drawdown_brake_scale(1000.0, 987.5, 50.0, 0, 1000.0).unwrap();
        assert_eq!(t1, 1);
        assert_close(s1, 0.75, 1e-12);

        let (t2, s2) = real_time_drawdown_brake_scale(1000.0, 989.0, 50.0, t1, 1000.0).unwrap();
        assert_eq!(t2, 1);
        assert_close(s2, 0.75, 1e-12);

        let (t3, s3) = real_time_drawdown_brake_scale(1000.0, 962.5, 50.0, t2, 1000.0).unwrap();
        assert_eq!(t3, 3);
        assert_close(s3, 0.25, 1e-12);

        let (t4, s4) = real_time_drawdown_brake_scale(1000.0, 1000.0, 50.0, t3, 1000.0).unwrap();
        assert_eq!(t4, 0);
        assert_close(s4, 1.0, 1e-12);
    }

    #[test]
    fn perp_stress_evaluator_matches_python_reference() {
        let corr = vec![vec![1.0, 0.1], vec![0.1, 1.0]];
        let report = evaluate_perp_stress_scenarios(
            1000.0,
            &[0.01, 0.02],
            &[0.6, 0.4],
            &corr,
            PerpStressParams::default(),
        )
        .expect("valid stress scenario");

        assert_close(report.base_portfolio_vol, 0.0104690018626419, 1e-12);
        assert_close(report.stressed_portfolio_vol, 0.0129305839001957, 1e-12);
        assert_close(report.vol_stress_multiplier, 1.23513053773902, 1e-12);
        assert_close(report.max_stressed_rc, 0.583732057416268, 1e-12);
        assert_close(report.basis_shock_loss_usd, 4.0, 1e-12);
        assert_close(report.thin_hedge_lag_loss_usd, 7.5, 1e-12);
        assert_close(report.funding_flip_10h_loss_usd, 1.0, 1e-12);
        assert_close(report.combined_crash_loss_usd, 12.5, 1e-12);
    }

    #[test]
    fn water_filling_weight_cap_redistributes_and_rejects_infeasible() {
        // Multi-round water-filling: [0.55, 0.35, 0.10] capped at 0.40 -> [0.40, 0.40, 0.20].
        let w = cap_and_renormalize_weights(&[0.55, 0.35, 0.10], 0.40).expect("feasible cap");
        assert_close(w.iter().sum(), 1.0, 1e-12);
        assert_close(w[0], 0.40, 1e-12);
        assert_close(w[1], 0.40, 1e-12);
        assert_close(w[2], 0.20, 1e-12);

        // Exact boundary 1.0 / 3.0 on 3 legs is feasible.
        let equal = cap_and_renormalize_weights(&[0.5, 0.3, 0.2], 1.0 / 3.0).unwrap();
        assert_close(equal.iter().sum(), 1.0, 1e-12);
        for weight in equal {
            assert_close(weight, 1.0 / 3.0, 1e-12);
        }

        // Infeasible cap: 3 * 0.30 = 0.90 < 1.0.
        let err = cap_and_renormalize_weights(&[0.5, 0.3, 0.2], 0.30).unwrap_err();
        assert!(matches!(
            err,
            PortfolioRiskError::InfeasibleWeightCap { .. }
        ));
    }

    #[test]
    fn allocate_carry_budgets_es_scaling_kelly_cap_and_brake_zero() {
        let vols = [0.01, 0.03, 0.06];
        let corr = vec![
            vec![1.0, 0.2, -0.1],
            vec![0.2, 1.0, 0.3],
            vec![-0.1, 0.3, 1.0],
        ];
        // 39 calm +0.1% returns + 1 crash -12.0% return -> empirical ES(97.5%) = 0.12.
        let mut hist = vec![0.001; 39];
        hist.push(-0.12);

        // ES budget caps at 600 / (10_000 * 0.12) = 0.50, while Kelly wants 1.0.
        let cfg = CarryBudgetConfig {
            max_gross_notional_usd: 10_000.0,
            max_single_leg_weight: 0.50,
            max_es_loss_usd: 600.0,
            ..CarryBudgetConfig::default()
        };
        let alloc =
            allocate_carry_budgets(&vols, &corr, &hist, 0.20, 0.01, &cfg).expect("valid alloc");

        assert_close(alloc.weights.iter().sum(), 1.0, 1e-12);
        assert_close(alloc.weights[0], 0.5, 1e-12);
        assert_close(alloc.weights[1], 1.0 / 3.0, 1e-12);
        assert_close(alloc.weights[2], 1.0 / 6.0, 1e-12);
        assert_close(alloc.base_portfolio_vol, 0.0171755640373177, 1e-12);
        assert_close(alloc.stressed_portfolio_vol, 0.0224722050542442, 1e-12);
        assert_close(alloc.stressed_risk_contributions.iter().sum(), 1.0, 1e-12);
        assert_close(alloc.empirical_var_frac, 0.12, 1e-12);
        assert_close(alloc.empirical_es_frac, 0.12, 1e-12);
        assert_close(alloc.kelly_scale, 1.0, 1e-12);
        assert_close(alloc.es_budget_scale, 0.5, 1e-12);
        assert_close(alloc.effective_scale, 0.5, 1e-12);
        assert_close(alloc.allocated_gross_notional_usd, 5_000.0, 1e-12);
        assert_close(alloc.leg_notionals_usd.iter().sum(), 5_000.0, 1e-12);
        assert_close(alloc.leg_notionals_usd[0], 2_500.0, 1e-12);

        // Brake scale == 0.0 forces zero allocation even when Kelly & ES allow exposure.
        let braked_cfg = CarryBudgetConfig {
            brake_scale: 0.0,
            ..cfg.clone()
        };
        let braked = allocate_carry_budgets(&vols, &corr, &hist, 0.20, 0.01, &braked_cfg)
            .expect("braked alloc");
        assert_close(braked.effective_scale, 0.0, 1e-12);
        assert_close(braked.allocated_gross_notional_usd, 0.0, 1e-12);
        for notional in braked.leg_notionals_usd {
            assert_close(notional, 0.0, 1e-12);
        }

        // Positive tail (ES <= 0.0) leaves es_budget_scale at 1.0; Kelly ceiling <= 1.0 governs.
        let positive_hist = vec![0.01, 0.02, 0.015, 0.03];
        let kelly_capped = allocate_carry_budgets(&vols, &corr, &positive_hist, 0.20, 0.01, &cfg)
            .expect("positive tail alloc");
        assert_close(kelly_capped.es_budget_scale, 1.0, 1e-12);
        assert_close(kelly_capped.kelly_scale, 1.0, 1e-12);
        assert_close(kelly_capped.allocated_gross_notional_usd, 10_000.0, 1e-12);
    }

    #[test]
    fn fail_closed_validation_rejects_invalid_inputs() {
        let eye2 = vec![vec![1.0, 0.0], vec![0.0, 1.0]];

        // Empty slices and length mismatches.
        assert_eq!(
            inverse_vol_weights(&[]),
            Err(PortfolioRiskError::EmptyInput("vols"))
        );
        assert_eq!(
            empirical_var_es(&[], 0.975),
            Err(PortfolioRiskError::EmptyInput("returns"))
        );
        assert_eq!(
            stressed_corr_matrix(&[], 0.70),
            Err(PortfolioRiskError::EmptyInput("corr"))
        );
        let len_err = portfolio_vol_and_rc(&[0.5], &[0.01, 0.02], &eye2).unwrap_err();
        assert!(matches!(len_err, PortfolioRiskError::LengthMismatch { .. }));

        // Non-finite and non-positive vols, zero weights.
        for bad in [0.0, -0.01, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(inverse_vol_weights(&[0.01, bad]).is_err());
        }
        assert!(portfolio_vol_and_rc(&[-0.1, 1.1], &[0.01, 0.02], &eye2).is_err());
        assert!(portfolio_vol_and_rc(&[f64::NAN, 0.5], &[0.01, 0.02], &eye2).is_err());
        assert_eq!(
            portfolio_vol_and_rc(&[0.0, 0.0], &[0.01, 0.02], &eye2),
            Err(PortfolioRiskError::NonPositivePortfolioVariance)
        );

        // Invalid correlation matrices: non-square, asymmetric, out-of-range, non-unit diagonal.
        assert!(stressed_corr_matrix(&[vec![1.0, 0.0], vec![0.0]], 0.70).is_err());
        assert!(stressed_corr_matrix(&[vec![1.0, 0.2], vec![0.1, 1.0]], 0.70).is_err());
        assert!(stressed_corr_matrix(&[vec![1.0, 1.1], vec![1.1, 1.0]], 0.70).is_err());
        assert!(stressed_corr_matrix(&[vec![1.0, -1.1], vec![-1.1, 1.0]], 0.70).is_err());
        assert!(stressed_corr_matrix(&[vec![0.99, 0.0], vec![0.0, 1.0]], 0.70).is_err());
        assert!(stressed_corr_matrix(&[vec![1.0, f64::NAN], vec![f64::NAN, 1.0]], 0.70).is_err());

        // Invalid scalar bounds.
        assert!(stressed_corr_matrix(&eye2, -0.01).is_err());
        assert!(stressed_corr_matrix(&eye2, 1.01).is_err());
        assert!(stressed_corr_matrix(&eye2, f64::NAN).is_err());

        assert!(empirical_var_es(&[0.01, -0.02], 0.5).is_err());
        assert!(empirical_var_es(&[0.01, -0.02], 1.0).is_err());
        assert!(empirical_var_es(&[0.01, f64::NAN], 0.95).is_err());

        assert!(capped_fractional_kelly(0.01, 0.01, 0.0, 1.0).is_err());
        assert!(capped_fractional_kelly(0.01, 0.01, 0.51, 1.0).is_err());
        assert!(capped_fractional_kelly(0.01, 0.01, 0.25, 0.0).is_err());
        assert!(capped_fractional_kelly(0.01, 0.01, 0.25, 1.01).is_err());
        assert!(capped_fractional_kelly(f64::NAN, 0.01, 0.25, 1.0).is_err());
        assert!(capped_fractional_kelly(0.01, f64::INFINITY, 0.25, 1.0).is_err());

        assert!(real_time_drawdown_brake_scale(1000.0, 990.0, 0.0, 0, 1000.0).is_err());
        assert!(real_time_drawdown_brake_scale(1000.0, 990.0, 50.0, 0, 0.0).is_err());
        assert!(real_time_drawdown_brake_scale(f64::NAN, 990.0, 50.0, 0, 1000.0).is_err());

        // Invalid stress params & gross notional.
        let bad_notional = evaluate_perp_stress_scenarios(
            0.0,
            &[0.01, 0.02],
            &[0.5, 0.5],
            &eye2,
            PerpStressParams::default(),
        );
        assert!(bad_notional.is_err());

        let bad_depth = evaluate_perp_stress_scenarios(
            1000.0,
            &[0.01, 0.02],
            &[0.5, 0.5],
            &eye2,
            PerpStressParams {
                depth_drop_frac: 1.0,
                ..PerpStressParams::default()
            },
        );
        assert!(bad_depth.is_err());

        let bad_bps = evaluate_perp_stress_scenarios(
            1000.0,
            &[0.01, 0.02],
            &[0.5, 0.5],
            &eye2,
            PerpStressParams {
                basis_shock_bps: -1.0,
                ..PerpStressParams::default()
            },
        );
        assert!(bad_bps.is_err());

        // Invalid carry budget config (including unconfigured Default).
        let unconfigured = allocate_carry_budgets(
            &[0.01, 0.02],
            &eye2,
            &[-0.01, 0.02],
            0.01,
            0.01,
            &CarryBudgetConfig::default(),
        );
        assert!(unconfigured.is_err());

        let vols2 = [0.01, 0.02];
        let rets2 = [-0.01, 0.02];
        let infeasible_cfg = CarryBudgetConfig {
            max_gross_notional_usd: 1000.0,
            max_single_leg_weight: 0.4,
            max_es_loss_usd: 100.0,
            ..CarryBudgetConfig::default()
        };
        let res_infeasible =
            allocate_carry_budgets(&vols2, &eye2, &rets2, 0.01, 0.01, &infeasible_cfg);
        assert!(res_infeasible.is_err());

        let bad_brake_cfg = CarryBudgetConfig {
            max_gross_notional_usd: 1000.0,
            max_single_leg_weight: 0.5,
            max_es_loss_usd: 100.0,
            brake_scale: 1.1,
            ..CarryBudgetConfig::default()
        };
        let res_brake = allocate_carry_budgets(&vols2, &eye2, &rets2, 0.01, 0.01, &bad_brake_cfg);
        assert!(res_brake.is_err());

        let zero_es_cfg = CarryBudgetConfig {
            max_gross_notional_usd: 1000.0,
            max_single_leg_weight: 0.5,
            max_es_loss_usd: 0.0,
            ..CarryBudgetConfig::default()
        };
        assert!(allocate_carry_budgets(&vols2, &eye2, &rets2, 0.01, 0.01, &zero_es_cfg).is_err());

        let zero_cap_cfg = CarryBudgetConfig {
            max_gross_notional_usd: 1000.0,
            max_single_leg_weight: 0.0,
            max_es_loss_usd: 100.0,
            ..CarryBudgetConfig::default()
        };
        assert!(allocate_carry_budgets(&vols2, &eye2, &rets2, 0.01, 0.01, &zero_cap_cfg).is_err());

        // Single-leg carry allocation and PortfolioRiskError Display/Error smoke check.
        let one_cfg = CarryBudgetConfig {
            max_gross_notional_usd: 2_000.0,
            max_single_leg_weight: 1.0,
            max_es_loss_usd: 100.0,
            ..CarryBudgetConfig::default()
        };
        let one_alloc =
            allocate_carry_budgets(&[0.02], &[vec![1.0]], &[-0.02, 0.01], 0.01, 0.01, &one_cfg)
                .expect("single-leg alloc");
        assert_close(one_alloc.weights[0], 1.0, 1e-12);
        assert_close(one_alloc.stressed_risk_contributions[0], 1.0, 1e-12);

        let err_msg = PortfolioRiskError::NonPositivePortfolioVariance.to_string();
        assert!(!err_msg.is_empty());
        let _: &dyn std::error::Error = &PortfolioRiskError::EmptyInput("vols");
    }

    #[test]
    fn serde_roundtrip_defaults_and_deny_unknown_fields() {
        let default_stress: PerpStressParams = serde_json::from_str("{}").expect("default stress");
        assert_eq!(default_stress, PerpStressParams::default());

        let custom_stress = PerpStressParams {
            basis_shock_bps: 55.0,
            hedge_lag_slip_bps: 20.0,
            depth_drop_frac: 0.75,
            funding_flip_bps_10h: 12.5,
            stress_rho_floor: 0.85,
        };
        let stress_json = serde_json::to_string(&custom_stress).expect("serialize stress");
        let parsed_stress: PerpStressParams =
            serde_json::from_str(&stress_json).expect("roundtrip stress");
        assert_eq!(parsed_stress, custom_stress);
        assert!(serde_json::from_str::<PerpStressParams>(r#"{"extra": 1.0}"#).is_err());

        let default_cfg: CarryBudgetConfig = serde_json::from_str("{}").expect("default cfg");
        assert_eq!(default_cfg, CarryBudgetConfig::default());

        let custom_cfg = CarryBudgetConfig {
            max_gross_notional_usd: 10_000.0,
            max_single_leg_weight: 0.5,
            max_es_loss_usd: 500.0,
            es_alpha: 0.95,
            stress_rho_floor: 0.75,
            kelly_fraction: 0.2,
            max_kelly_scale: 0.8,
            brake_scale: 0.75,
        };
        let cfg_json = serde_json::to_string(&custom_cfg).expect("serialize cfg");
        let parsed_cfg: CarryBudgetConfig = serde_json::from_str(&cfg_json).expect("roundtrip cfg");
        assert_eq!(parsed_cfg, custom_cfg);
        assert!(serde_json::from_str::<CarryBudgetConfig>(r#"{"unknown": 1.0}"#).is_err());

        let partial_scenario_json = r#"{
            "vols": [0.01, 0.02],
            "corr": [[1.0, 0.2], [0.2, 1.0]],
            "historical_portfolio_returns": [0.001, -0.05],
            "mean_return": 0.002,
            "variance": 0.0004,
            "config": {
                "max_gross_notional_usd": 5000.0,
                "max_single_leg_weight": 0.6,
                "max_es_loss_usd": 300.0
            }
        }"#;
        let parsed_scenario: PortfolioRiskScenario =
            serde_json::from_str(partial_scenario_json).expect("scenario with defaults");
        assert_eq!(parsed_scenario.stress_params, PerpStressParams::default());
        assert_close(parsed_scenario.config.kelly_fraction, 0.25, 1e-12);
        assert!(serde_json::from_str::<PortfolioRiskScenario>(r#"{"extra": 1}"#).is_err());
    }

    #[test]
    fn evaluate_portfolio_risk_scenario_positive_and_zero_allocation() {
        let mut hist = vec![0.001; 39];
        hist.push(-0.12);
        let scenario = PortfolioRiskScenario {
            vols: vec![0.01, 0.03, 0.06],
            corr: vec![
                vec![1.0, 0.2, -0.1],
                vec![0.2, 1.0, 0.3],
                vec![-0.1, 0.3, 1.0],
            ],
            historical_portfolio_returns: hist,
            mean_return: 0.20,
            variance: 0.01,
            config: CarryBudgetConfig {
                max_gross_notional_usd: 10_000.0,
                max_single_leg_weight: 0.50,
                max_es_loss_usd: 600.0,
                ..CarryBudgetConfig::default()
            },
            stress_params: PerpStressParams::default(),
        };

        let eval = evaluate_portfolio_risk_scenario(&scenario).expect("positive scenario");
        assert_close(eval.allocation.effective_scale, 0.5, 1e-12);
        assert_close(eval.allocation.allocated_gross_notional_usd, 5_000.0, 1e-12);
        let alloc_stress = eval
            .stress_on_allocated_notional
            .as_ref()
            .expect("allocated stress present");
        assert_close(
            alloc_stress.combined_crash_loss_usd,
            eval.stress_on_max_notional.combined_crash_loss_usd * 0.5,
            1e-12,
        );
        assert_close(
            alloc_stress.vol_stress_multiplier,
            eval.stress_on_max_notional.vol_stress_multiplier,
            1e-12,
        );
        let json_val = serde_json::to_value(&eval).expect("serialize eval");
        assert!(json_val["stress_on_allocated_notional"].is_object());

        // Zero allocation via brake_scale == 0.0 leaves stress_on_allocated_notional as None (null).
        let mut braked = scenario.clone();
        braked.config.brake_scale = 0.0;
        let braked_eval = evaluate_portfolio_risk_scenario(&braked).expect("braked scenario");
        assert_close(
            braked_eval.allocation.allocated_gross_notional_usd,
            0.0,
            1e-12,
        );
        assert!(braked_eval.stress_on_max_notional.combined_crash_loss_usd > 0.0);
        assert!(braked_eval.stress_on_allocated_notional.is_none());
        let braked_json = serde_json::to_value(&braked_eval).expect("serialize braked eval");
        assert!(braked_json["stress_on_allocated_notional"].is_null());

        // Zero allocation via non-positive mean return also yields None for allocated stress.
        let mut neg_mean = scenario;
        neg_mean.mean_return = -0.01;
        let neg_eval = evaluate_portfolio_risk_scenario(&neg_mean).expect("neg mean scenario");
        assert_close(neg_eval.allocation.allocated_gross_notional_usd, 0.0, 1e-12);
        assert!(neg_eval.stress_on_allocated_notional.is_none());

        // Invalid stress_params must still fail closed when brake_scale == 0.0.
        braked.stress_params.depth_drop_frac = 1.0;
        assert!(evaluate_portfolio_risk_scenario(&braked).is_err());
    }
}
