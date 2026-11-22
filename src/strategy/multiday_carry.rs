//! Independent multi-day funding-carry sub-track (`S9-3`).
//!
//! Enforces strict separation from market-making and arbitrage budgets:
//!   - Segregated capital, max loss budget, and fixed daily infra cost;
//!   - Pre-window causal frozen direction (no mid-campaign direction flips);
//!   - Entry quote distance `>= partner_fee_bps + conditional_markout_bps + min_entry_profit_bps`;
//!   - Continuous 10h (`36_000_000 ms`) checkpoints with exact witnessed funding reconciliation.

use anyhow::{Result, ensure};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

const BPS: Decimal = Decimal::from_parts(10_000, 0, 0, false, 0);
const UNIT_MS: u64 = 36_000_000;
const DAY_MS: u64 = 86_400_000;

fn hourly_interval_ms() -> u64 {
    3_600_000
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MultiDayCarryConfig {
    pub market: String,
    pub legs: [String; 2],
    pub decision_start_ms: u64,
    pub end_ms: u64,
    pub min_publication_lag_ms: u64,
    /// Both carry legs must share this known UTC funding grid.
    #[serde(default = "hourly_interval_ms")]
    pub funding_interval_ms: u64,
    #[serde(default)]
    pub funding_offset_ms: u64,
    #[serde(with = "rust_decimal::serde::str")]
    pub capital_usd: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub max_loss_budget_usd: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub target_leg_notional_usd: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub min_forecast_bps: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub trailing_exit_threshold_bps: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub partner_fee_bps: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub conditional_markout_bps: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub min_entry_profit_bps: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub max_basis_drawdown_bps: Decimal,
    #[serde(default, with = "rust_decimal::serde::str")]
    pub maker_entry_fee_bps: Decimal,
    #[serde(default, with = "rust_decimal::serde::str")]
    pub maker_exit_fee_bps: Decimal,
    /// Optional per-leg `[leg0, leg1]` entry fee rates in bps (positive = fee, negative = rebate).
    /// When absent, falls back symmetrically to `[maker_entry_fee_bps; 2]`.
    #[serde(default)]
    pub leg_entry_fee_bps: Option<[Decimal; 2]>,
    /// Optional per-leg `[leg0, leg1]` exit fee rates in bps (positive = fee, negative = rebate).
    /// When absent, falls back symmetrically to `[maker_exit_fee_bps; 2]`.
    #[serde(default)]
    pub leg_exit_fee_bps: Option<[Decimal; 2]>,
    #[serde(default, with = "rust_decimal::serde::str")]
    pub fixed_overhead_usd_per_day: Decimal,
    #[serde(default)]
    pub exit_on_missing_witness: bool,
}

impl MultiDayCarryConfig {
    pub fn resolved_entry_fee_bps(&self) -> [Decimal; 2] {
        self.leg_entry_fee_bps
            .unwrap_or([self.maker_entry_fee_bps; 2])
    }

    pub fn resolved_exit_fee_bps(&self) -> [Decimal; 2] {
        self.leg_exit_fee_bps
            .unwrap_or([self.maker_exit_fee_bps; 2])
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.market.is_empty()
                && !self.legs[0].is_empty()
                && !self.legs[1].is_empty()
                && self.legs[0] != self.legs[1],
            "multi-day carry requires distinct leg venues and market symbol"
        );
        ensure!(
            self.decision_start_ms > 0
                && self.end_ms > self.decision_start_ms
                && self.end_ms - self.decision_start_ms >= UNIT_MS,
            "multi-day carry campaign must span at least one continuous 10h unit"
        );
        ensure!(
            self.funding_interval_ms > 0 && self.funding_offset_ms < self.funding_interval_ms,
            "invalid common funding interval or UTC offset"
        );
        ensure!(
            self.capital_usd > Decimal::ZERO
                && self.max_loss_budget_usd > Decimal::ZERO
                && self.max_loss_budget_usd <= self.capital_usd
                && self.target_leg_notional_usd > Decimal::ZERO
                && self.target_leg_notional_usd <= self.capital_usd * Decimal::from(5)
                && self.min_forecast_bps > Decimal::ZERO
                && self.partner_fee_bps >= Decimal::ZERO
                && self.conditional_markout_bps >= Decimal::ZERO
                && self.max_basis_drawdown_bps > Decimal::ZERO
                && self.fixed_overhead_usd_per_day >= Decimal::ZERO,
            "invalid multi-day carry capital, loss budget, or distance parameters"
        );
        ensure!(
            !(self.leg_entry_fee_bps.is_some() && self.maker_entry_fee_bps != Decimal::ZERO),
            "specify either maker_entry_fee_bps or leg_entry_fee_bps, not both"
        );
        ensure!(
            !(self.leg_exit_fee_bps.is_some() && self.maker_exit_fee_bps != Decimal::ZERO),
            "specify either maker_exit_fee_bps or leg_exit_fee_bps, not both"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PriorSettlement {
    pub settled_at_ms: u64,
    /// Per-settlement funding rate differential `rate_leg0 - rate_leg1` (in fractional units, positive = leg0 longs pay more).
    #[serde(with = "rust_decimal::serde::str")]
    pub rate_diff: Decimal,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CarryTimelineFrame {
    pub at_ms: u64,
    #[serde(with = "rust_decimal::serde::str")]
    pub leg0_price: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub leg1_price: Decimal,
    #[serde(default)]
    pub funding_slot_expected: bool,
    /// Witnessed per-settlement rates `[rate_leg0, rate_leg1]` (positive = longs pay shorts) if both present at slot.
    #[serde(default)]
    pub witnessed_rates: Option<[Decimal; 2]>,
    #[serde(with = "rust_decimal::serde::str")]
    pub observed_entry_quote_distance_bps: Decimal,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MultiDayCarryScenario {
    pub config: MultiDayCarryConfig,
    pub prior_settlements: Vec<PriorSettlement>,
    pub frames: Vec<CarryTimelineFrame>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CarryExitReason {
    NoSignal,
    BlockedEntryDistance,
    TrailingFundingDecay,
    LossBudgetBreached,
    BasisDrawdownBreached,
    MissingFundingWitness,
    CampaignEnd,
}

#[derive(Debug, Clone, Serialize)]
pub struct TenHourCarryCheckpoint {
    pub unit_index: usize,
    pub at_ms: u64,
    #[serde(with = "rust_decimal::serde::str")]
    pub mark_equity_usd: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub cumulative_funding_usd: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub cumulative_fees_usd: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub cumulative_price_pnl_usd: Decimal,
    pub open: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct MultiDayCarryReport {
    pub subtrack: &'static str,
    pub market: String,
    pub legs: [String; 2],
    pub status: &'static str,
    pub direction_frozen_before_start: bool,
    pub frozen_leg0_sign: i32,
    #[serde(with = "rust_decimal::serde::str")]
    pub pre_window_ten_hour_forecast_bps: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub required_entry_distance_bps: Decimal,
    pub entered: bool,
    pub exit_reason: CarryExitReason,
    #[serde(with = "rust_decimal::serde::str")]
    pub base_quantity: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub capital_usd: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub price_pnl_usd: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub witnessed_funding_cash_usd: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub fees_usd: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub terminal_equity_usd: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub net_variable_pnl_usd: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub fixed_infra_usd: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub net_all_in_pnl_usd: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub max_drawdown_usd: Decimal,
    pub expected_funding_slots: usize,
    pub witnessed_funding_slots: usize,
    pub missing_witness_slots: usize,
    pub funding_witness_complete: bool,
    pub cash_identity_reconciled: bool,
    pub expected_checkpoints: u64,
    pub missing_checkpoints: u64,
    pub ten_hour_checkpoints: Vec<TenHourCarryCheckpoint>,
}

impl MultiDayCarryScenario {
    fn cash_components_reconcile(report: &MultiDayCarryReport) -> bool {
        if report.capital_usd <= Decimal::ZERO || report.fixed_infra_usd < Decimal::ZERO {
            return false;
        }
        let Some(magnitude) = report
            .capital_usd
            .abs()
            .checked_add(report.price_pnl_usd.abs())
            .and_then(|v| v.checked_add(report.witnessed_funding_cash_usd.abs()))
            .and_then(|v| v.checked_add(report.fees_usd.abs()))
            .and_then(|v| v.checked_add(report.fixed_infra_usd.abs()))
        else {
            return false;
        };
        let adjusted = if magnitude.is_zero() {
            0_i32
        } else {
            magnitude.mantissa().unsigned_abs().ilog10() as i32 - (magnitude.scale() as i32)
        };
        let raw_exp = (adjusted - 27).max(-28);
        let max_tol = Decimal::new(1, 12);
        let tolerance = if raw_exp > -12 {
            max_tol
        } else {
            Decimal::new(4, raw_exp.unsigned_abs()).min(max_tol)
        };
        let Some(expected_var) = report
            .price_pnl_usd
            .checked_add(report.witnessed_funding_cash_usd)
            .and_then(|v| v.checked_sub(report.fees_usd))
        else {
            return false;
        };
        let Some(expected_equity) = report.capital_usd.checked_add(report.net_variable_pnl_usd)
        else {
            return false;
        };
        let Some(expected_all_in) = report
            .net_variable_pnl_usd
            .checked_sub(report.fixed_infra_usd)
        else {
            return false;
        };
        let Some(var_diff) = expected_var.checked_sub(report.net_variable_pnl_usd) else {
            return false;
        };
        let Some(eq_diff) = expected_equity.checked_sub(report.terminal_equity_usd) else {
            return false;
        };
        let Some(all_in_diff) = expected_all_in.checked_sub(report.net_all_in_pnl_usd) else {
            return false;
        };
        if var_diff.abs() > tolerance || eq_diff.abs() > tolerance || all_in_diff.abs() > tolerance
        {
            return false;
        }
        if let Some(cp) = report.ten_hour_checkpoints.last().filter(|cp| !cp.open) {
            let checks = [
                cp.mark_equity_usd.checked_sub(report.terminal_equity_usd),
                cp.cumulative_funding_usd
                    .checked_sub(report.witnessed_funding_cash_usd),
                cp.cumulative_fees_usd.checked_sub(report.fees_usd),
                cp.cumulative_price_pnl_usd
                    .checked_sub(report.price_pnl_usd),
            ];
            if checks
                .into_iter()
                .any(|diff| diff.is_none_or(|d| d.abs() > tolerance))
            {
                return false;
            }
        }
        true
    }

    pub fn evaluate(&self) -> Result<MultiDayCarryReport> {
        self.config.validate()?;
        ensure!(!self.frames.is_empty(), "multi-day carry frames empty");

        let decision_cutoff = self
            .config
            .decision_start_ms
            .saturating_sub(self.config.min_publication_lag_ms);
        let mut causal_prior = BTreeMap::new();
        for settlement in self
            .prior_settlements
            .iter()
            .filter(|s| s.settled_at_ms <= decision_cutoff)
        {
            let rate = causal_prior
                .entry(settlement.settled_at_ms)
                .or_insert(settlement.rate_diff);
            ensure!(
                *rate == settlement.rate_diff,
                "conflicting rate differentials for a causal prior settlement"
            );
        }
        ensure!(
            !causal_prior.is_empty(),
            "need at least one causal pre-window funding settlement before decision_start_ms - lag"
        );
        let mean_diff =
            causal_prior.values().copied().sum::<Decimal>() / Decimal::from(causal_prior.len());
        let interval = self.config.funding_interval_ms;
        let offset = self.config.funding_offset_ms;
        // Difference counts every contractual settlement in (previous, current],
        // even when a whole source frame or its expected flag is absent.
        let slot_index =
            |ms: u64| (u128::from(ms) + u128::from(interval - offset)) / u128::from(interval);
        let start = self.config.decision_start_ms;
        let forecast_slots = u64::try_from(slot_index(start + UNIT_MS) - slot_index(start))?;
        let forecast_10h_bps = mean_diff * BPS * Decimal::from(forecast_slots);
        let required_distance_bps = self.config.partner_fee_bps
            + self.config.conditional_markout_bps
            + self.config.min_entry_profit_bps;
        let campaign_ms = self.config.end_ms - self.config.decision_start_ms;
        let fixed_infra_usd = self.config.fixed_overhead_usd_per_day * Decimal::from(campaign_ms)
            / Decimal::from(DAY_MS);

        let build_flat_report =
            |frozen_sign: i32, reason: CarryExitReason, status: &'static str| MultiDayCarryReport {
                subtrack: "multiday_funding_carry_isolated",
                market: self.config.market.clone(),
                legs: self.config.legs.clone(),
                status,
                direction_frozen_before_start: true,
                frozen_leg0_sign: frozen_sign,
                pre_window_ten_hour_forecast_bps: forecast_10h_bps,
                required_entry_distance_bps: required_distance_bps,
                entered: false,
                exit_reason: reason,
                base_quantity: Decimal::ZERO,
                capital_usd: self.config.capital_usd,
                price_pnl_usd: Decimal::ZERO,
                witnessed_funding_cash_usd: Decimal::ZERO,
                fees_usd: Decimal::ZERO,
                terminal_equity_usd: self.config.capital_usd,
                net_variable_pnl_usd: Decimal::ZERO,
                fixed_infra_usd,
                net_all_in_pnl_usd: -fixed_infra_usd,
                max_drawdown_usd: Decimal::ZERO,
                expected_funding_slots: 0,
                witnessed_funding_slots: 0,
                missing_witness_slots: 0,
                funding_witness_complete: true,
                cash_identity_reconciled: true,
                expected_checkpoints: 0,
                missing_checkpoints: 0,
                ten_hour_checkpoints: Vec::new(),
            };

        if forecast_10h_bps.abs() < self.config.min_forecast_bps {
            return Ok(build_flat_report(0, CarryExitReason::NoSignal, "no_signal"));
        }
        // Positive rate = longs pay shorts -> if rate[0] - rate[1] > 0, short leg0 (-1) & long leg1 (+1).
        let frozen_leg0_sign = if forecast_10h_bps > Decimal::ZERO {
            -1
        } else {
            1
        };

        let mut prev_ms = 0_u64;
        let mut entry_idx = None;
        for (idx, frame) in self.frames.iter().enumerate() {
            ensure!(
                frame.at_ms > prev_ms
                    && frame.leg0_price > Decimal::ZERO
                    && frame.leg1_price > Decimal::ZERO,
                "carry frames must increase monotonically with positive leg prices"
            );
            prev_ms = frame.at_ms;
            if entry_idx.is_none()
                && (self.config.decision_start_ms..self.config.end_ms).contains(&frame.at_ms)
            {
                entry_idx = Some(idx);
            }
        }
        let start_idx =
            entry_idx.ok_or_else(|| anyhow::anyhow!("no entry frame inside the carry campaign"))?;
        let entry = &self.frames[start_idx];
        if entry.observed_entry_quote_distance_bps < required_distance_bps {
            return Ok(build_flat_report(
                frozen_leg0_sign,
                CarryExitReason::BlockedEntryDistance,
                "blocked_entry_distance",
            ));
        }

        let avg_px = (entry.leg0_price + entry.leg1_price) / Decimal::from(2);
        let base_qty = self.config.target_leg_notional_usd / avg_px;
        let q0 = base_qty * Decimal::from(frozen_leg0_sign);
        let q1 = -q0;
        let entry_fee_rates = self.config.resolved_entry_fee_bps();
        let exit_fee_rates = self.config.resolved_exit_fee_bps();
        let entry_fee = (entry.leg0_price * entry_fee_rates[0]
            + entry.leg1_price * entry_fee_rates[1])
            * base_qty
            / BPS;
        let mut fees_usd = entry_fee;
        let mut funding_cash_usd = Decimal::ZERO;
        let mut realized_price_pnl_usd = Decimal::ZERO;
        let mut peak_equity = self.config.capital_usd;
        let mut max_drawdown_usd = entry_fee.max(Decimal::ZERO);
        let mut open = true;
        let mut exit_reason = CarryExitReason::CampaignEnd;
        let mut expected_slots = 0usize;
        let mut witnessed_slots = 0usize;
        let mut missing_slots = 0usize;
        let mut previous_slot = slot_index(entry.at_ms);
        let mut checkpoints = Vec::new();
        let entry_basis_bps = (entry.leg0_price - entry.leg1_price) / entry.leg1_price * BPS;

        for frame in &self.frames[start_idx + 1..] {
            if frame.at_ms > self.config.end_ms {
                break;
            }
            let was_open = open;
            let current_slot = slot_index(frame.at_ms);
            let due_here = frame.at_ms % interval == offset;
            ensure!(
                (!frame.funding_slot_expected || due_here)
                    && (frame.witnessed_rates.is_none() || frame.funding_slot_expected),
                "funding evidence does not match the configured settlement grid"
            );
            if open {
                let elapsed_slots = usize::try_from(current_slot - previous_slot)?;
                expected_slots += elapsed_slots;
                let rates = frame.witnessed_rates;
                missing_slots += elapsed_slots - usize::from(rates.is_some());
                if let Some([r0, r1]) = rates {
                    witnessed_slots += 1;
                    funding_cash_usd -= q0 * r0 * frame.leg0_price + q1 * r1 * frame.leg1_price;
                    let slot_carry_bps = -Decimal::from(frozen_leg0_sign) * (r0 - r1) * BPS;
                    if slot_carry_bps < self.config.trailing_exit_threshold_bps {
                        open = false;
                        exit_reason = CarryExitReason::TrailingFundingDecay;
                    }
                }
                if self.config.exit_on_missing_witness && missing_slots > 0 {
                    // The first available frame after a gap is the earliest observable exit.
                    open = false;
                    exit_reason = CarryExitReason::MissingFundingWitness;
                }
            }
            previous_slot = current_slot;

            let cur_price_pnl = if was_open {
                q0 * (frame.leg0_price - entry.leg0_price)
                    + q1 * (frame.leg1_price - entry.leg1_price)
            } else {
                realized_price_pnl_usd
            };
            let equity = self.config.capital_usd + cur_price_pnl + funding_cash_usd - fees_usd;
            peak_equity = peak_equity.max(equity);
            max_drawdown_usd = max_drawdown_usd.max(peak_equity - equity);

            if open {
                let basis_bps = (frame.leg0_price - frame.leg1_price) / frame.leg1_price * BPS;
                let adverse_basis = if frozen_leg0_sign < 0 {
                    basis_bps - entry_basis_bps
                } else {
                    entry_basis_bps - basis_bps
                };
                if max_drawdown_usd >= self.config.max_loss_budget_usd {
                    open = false;
                    exit_reason = CarryExitReason::LossBudgetBreached;
                } else if adverse_basis >= self.config.max_basis_drawdown_bps {
                    open = false;
                    exit_reason = CarryExitReason::BasisDrawdownBreached;
                } else if frame.at_ms == self.config.end_ms {
                    open = false;
                }
            }
            if was_open && !open {
                realized_price_pnl_usd = cur_price_pnl;
                fees_usd += (frame.leg0_price * exit_fee_rates[0]
                    + frame.leg1_price * exit_fee_rates[1])
                    * base_qty
                    / BPS;
            }
            let marked_equity =
                self.config.capital_usd + cur_price_pnl + funding_cash_usd - fees_usd;
            max_drawdown_usd = max_drawdown_usd.max(peak_equity - marked_equity);
            peak_equity = peak_equity.max(marked_equity);

            let elapsed = frame.at_ms - self.config.decision_start_ms;
            if elapsed.is_multiple_of(UNIT_MS) {
                checkpoints.push(TenHourCarryCheckpoint {
                    unit_index: usize::try_from(elapsed / UNIT_MS)?,
                    at_ms: frame.at_ms,
                    mark_equity_usd: marked_equity,
                    cumulative_funding_usd: funding_cash_usd,
                    cumulative_fees_usd: fees_usd,
                    cumulative_price_pnl_usd: cur_price_pnl,
                    open,
                });
            }
        }
        ensure!(
            !open,
            "missing carry terminal valuation at the campaign end"
        );

        let terminal_equity_usd =
            self.config.capital_usd + realized_price_pnl_usd + funding_cash_usd - fees_usd;
        let net_variable_pnl_usd = terminal_equity_usd - self.config.capital_usd;
        let net_all_in_pnl_usd = net_variable_pnl_usd - fixed_infra_usd;
        let funding_witness_complete = missing_slots == 0;
        let expected_checkpoints = campaign_ms / UNIT_MS;
        let missing_checkpoints = expected_checkpoints - u64::try_from(checkpoints.len())?;
        let status = if !funding_witness_complete {
            "incomplete_missing_witness"
        } else if missing_checkpoints > 0 {
            "incomplete_missing_checkpoint"
        } else {
            "settled_witnessed"
        };

        let mut report = MultiDayCarryReport {
            subtrack: "multiday_funding_carry_isolated",
            market: self.config.market.clone(),
            legs: self.config.legs.clone(),
            status,
            direction_frozen_before_start: true,
            frozen_leg0_sign,
            pre_window_ten_hour_forecast_bps: forecast_10h_bps,
            required_entry_distance_bps: required_distance_bps,
            entered: true,
            exit_reason,
            base_quantity: base_qty,
            capital_usd: self.config.capital_usd,
            price_pnl_usd: realized_price_pnl_usd,
            witnessed_funding_cash_usd: funding_cash_usd,
            fees_usd,
            terminal_equity_usd,
            net_variable_pnl_usd,
            fixed_infra_usd,
            net_all_in_pnl_usd,
            max_drawdown_usd,
            expected_funding_slots: expected_slots,
            witnessed_funding_slots: witnessed_slots,
            missing_witness_slots: missing_slots,
            funding_witness_complete,
            cash_identity_reconciled: false,
            expected_checkpoints,
            missing_checkpoints,
            ten_hour_checkpoints: checkpoints,
        };
        report.cash_identity_reconciled = Self::cash_components_reconcile(&report);
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    fn scenario(observed_dist: &str) -> MultiDayCarryScenario {
        let start = 1_800_000_000_000_u64;
        let hour = 3_600_000_u64;
        let end = start + 30 * hour; // Continuous 30h (3 x 10h checkpoints)
        let config = MultiDayCarryConfig {
            market: "SOL".into(),
            legs: ["lighter".into(), "lighter_robinhood".into()],
            decision_start_ms: start,
            end_ms: end,
            min_publication_lag_ms: 2 * hour,
            funding_interval_ms: hour,
            funding_offset_ms: 0,
            capital_usd: d("400"),
            max_loss_budget_usd: d("50"),
            target_leg_notional_usd: d("100"),
            min_forecast_bps: d("1.0"),
            trailing_exit_threshold_bps: d("-1.0"),
            partner_fee_bps: d("0"),
            conditional_markout_bps: d("0.5"),
            min_entry_profit_bps: d("0.5"),
            max_basis_drawdown_bps: d("25"),
            maker_entry_fee_bps: d("0"),
            maker_exit_fee_bps: d("0"),
            leg_entry_fee_bps: None,
            leg_exit_fee_bps: None,
            fixed_overhead_usd_per_day: d("1.6666666667"),
            exit_on_missing_witness: false,
        };
        let prior_settlements = vec![
            // Strictly before start - 2h -> causal; post-cutoff sample must be ignored!
            PriorSettlement {
                settled_at_ms: start - 3 * hour,
                rate_diff: d("0.0001"), // 1 bp/h -> +10 bps per 10h
            },
            PriorSettlement {
                settled_at_ms: start - hour, // within lag -> must NOT invert forecast!
                rate_diff: d("-0.0100"),
            },
        ];
        let mut frames = Vec::new();
        for h in 0..=30_u64 {
            let at_ms = start + h * hour;
            frames.push(CarryTimelineFrame {
                at_ms,
                leg0_price: d("100"),
                leg1_price: d("100"),
                funding_slot_expected: h > 0,
                witnessed_rates: (h > 0).then_some([d("0.0001"), d("0")]),
                observed_entry_quote_distance_bps: d(observed_dist),
            });
        }
        MultiDayCarryScenario {
            config,
            prior_settlements,
            frames,
        }
    }

    #[test]
    fn missing_frame_does_not_backdate_cash_or_invent_settlements() {
        let mut data = scenario("1.5");
        let hour = hourly_interval_ms();
        data.frames.remove(10);
        let report = data.evaluate().unwrap();
        assert_eq!(report.expected_funding_slots, 30);
        assert_eq!(report.witnessed_funding_slots, 29);
        assert_eq!(report.missing_witness_slots, 1);
        assert_eq!(report.expected_checkpoints, 3);
        assert_eq!(report.missing_checkpoints, 1);
        assert_eq!(report.ten_hour_checkpoints[0].unit_index, 2);
        assert_eq!(report.status, "incomplete_missing_witness");
        data.config.exit_on_missing_witness = true;
        let exited = data.evaluate().unwrap();
        assert_eq!(exited.exit_reason, CarryExitReason::MissingFundingWitness);
        assert_eq!(exited.expected_funding_slots, 11);
        assert_eq!(exited.witnessed_funding_slots, 10);

        // An alternative grid can have a missing valuation without missing funding.
        let mut shifted = scenario("1.5");
        shifted.config.funding_offset_ms = hour / 2;
        for frame in shifted.frames.iter_mut().skip(1) {
            frame.at_ms -= hour / 2;
        }
        let mut end = shifted.frames.last().unwrap().clone();
        end.at_ms = shifted.config.end_ms;
        end.funding_slot_expected = false;
        end.witnessed_rates = None;
        shifted.frames.push(end);
        let report = shifted.evaluate().unwrap();
        assert_eq!(report.expected_funding_slots, 30);
        assert!(report.funding_witness_complete);
        assert_eq!(report.missing_checkpoints, 2);
        assert_eq!(report.status, "incomplete_missing_checkpoint");
    }

    #[test]
    fn ten_hour_forecast_counts_the_actual_settlement_grid() {
        let mut input = scenario("10");
        let hour = hourly_interval_ms();
        let start = input.config.decision_start_ms;
        input.config.funding_interval_ms = 8 * hour;
        input.config.funding_offset_ms = (start + hour) % (8 * hour);
        input.config.min_forecast_bps = d("3");
        let report = input.evaluate().unwrap();
        assert_eq!(report.pre_window_ten_hour_forecast_bps, d("2"));
        assert_eq!(report.status, "no_signal");
    }

    #[test]
    fn repeated_prior_funding_slots_never_change_the_frozen_signal() {
        let mut input = scenario("1.5");
        let start = input.config.decision_start_ms;
        input.prior_settlements.push(PriorSettlement {
            settled_at_ms: start - 4 * hourly_interval_ms(),
            rate_diff: d("-0.0002"),
        });
        let baseline = input.evaluate().unwrap();
        assert_eq!(baseline.pre_window_ten_hour_forecast_bps, d("-5"));
        assert_eq!(baseline.frozen_leg0_sign, 1);

        input
            .prior_settlements
            .push(input.prior_settlements[0].clone());
        let repeated = input.evaluate().unwrap();
        assert_eq!(
            repeated.pre_window_ten_hour_forecast_bps,
            baseline.pre_window_ten_hour_forecast_bps
        );
        assert_eq!(repeated.frozen_leg0_sign, baseline.frozen_leg0_sign);

        input.prior_settlements.last_mut().unwrap().rate_diff = d("0.0003");
        assert!(input.evaluate().is_err());
    }

    #[test]
    fn terminal_close_and_grid_are_required_for_settled_carry() {
        let mut data = scenario("1.5");
        data.config.maker_exit_fee_bps = d("5");
        data.frames[5].funding_slot_expected = false;
        data.frames[5].witnessed_rates = None;
        let report = data.evaluate().unwrap();
        assert_eq!(report.missing_witness_slots, 1);
        let terminal = report.ten_hour_checkpoints.last().unwrap();
        assert!(!terminal.open);
        assert_eq!(terminal.cumulative_fees_usd, report.fees_usd);
        assert_eq!(terminal.mark_equity_usd, report.terminal_equity_usd);
        assert!(report.max_drawdown_usd >= d("0.10"));
        data.frames.pop();
        assert!(data.evaluate().is_err());
        let mut late = data.frames.pop().unwrap();
        late.at_ms = data.config.end_ms + 1;
        data.frames = vec![late];
        assert!(data.evaluate().is_err());
    }

    #[test]
    fn multiday_carry_freezes_causal_direction_tracks_10h_units_and_blocks_narrow_entry() {
        let rep = scenario("1.2").evaluate().unwrap();
        assert!(rep.direction_frozen_before_start);
        assert_eq!(rep.frozen_leg0_sign, -1); // Core > RH -> short Core, long RH (post-lag negative ignored!)
        assert!(rep.entered);
        assert_eq!(rep.ten_hour_checkpoints.len(), 3);
        assert_eq!(rep.expected_funding_slots, 30);
        assert_eq!(rep.witnessed_funding_slots, 30);
        assert!(rep.funding_witness_complete);
        assert!(rep.cash_identity_reconciled);
        // 30 hours * $100 * 1 bp/h = +$0.30 witnessed funding, 0 fees!
        assert_eq!(rep.witnessed_funding_cash_usd, d("0.3"));
        assert_eq!(rep.terminal_equity_usd, d("400.3"));

        // Entry distance 0.5 < required (0 + 0.5 + 0.5 = 1.0 bps) stays flat!
        let blocked = scenario("0.5").evaluate().unwrap();
        assert!(!blocked.entered);
        assert_eq!(blocked.exit_reason, CarryExitReason::BlockedEntryDistance);
        assert_eq!(blocked.terminal_equity_usd, d("400"));

        // Missing witness cannot be silently imputed as 0 -> flags incomplete_missing_witness.
        let mut gapped = scenario("1.5");
        gapped.frames[5].witnessed_rates = None;
        let gap_rep = gapped.evaluate().unwrap();
        assert_eq!(gap_rep.status, "incomplete_missing_witness");
        assert_eq!(gap_rep.missing_witness_slots, 1);
        assert!(!gap_rep.funding_witness_complete);
    }

    #[test]
    fn asymmetric_per_leg_wave2b_rwa_fees_and_dual_spec_validation() {
        let start = 1_800_000_000_000_u64;
        let hour = hourly_interval_ms();
        let frames: Vec<serde_json::Value> = (0..=30_u64)
            .map(|h| {
                serde_json::json!({
                    "at_ms": start + h * hour,
                    "leg0_price": "100",
                    "leg1_price": "100",
                    "funding_slot_expected": h > 0,
                    "witnessed_rates": if h > 0 { Some(["0.000005", "0.000070"]) } else { None },
                    "observed_entry_quote_distance_bps": "5.0"
                })
            })
            .collect();
        // Long Arcus RWA (0 maker entry, 2.25 taker exit) + Short Lighter Core RWA (0 maker / 0 taker).
        let json_payload = serde_json::json!({
            "config": {
                "market": "NVDA",
                "legs": ["arcus", "lighter"],
                "decision_start_ms": start,
                "end_ms": start + 30 * hour,
                "min_publication_lag_ms": hour,
                "capital_usd": "1000",
                "max_loss_budget_usd": "100",
                "target_leg_notional_usd": "500",
                "min_forecast_bps": "3.0",
                "trailing_exit_threshold_bps": "0.1",
                "partner_fee_bps": "2.25",
                "conditional_markout_bps": "1.0",
                "min_entry_profit_bps": "0.5",
                "max_basis_drawdown_bps": "25",
                "leg_entry_fee_bps": ["0", "0"],
                "leg_exit_fee_bps": ["2.25", "0"],
                "fixed_overhead_usd_per_day": "1.6"
            },
            "prior_settlements": [{
                "settled_at_ms": start - 2 * hour,
                "rate_diff": "-0.000065"
            }],
            "frames": frames
        });
        let scenario: MultiDayCarryScenario = serde_json::from_value(json_payload).unwrap();
        let rep = scenario.evaluate().unwrap();
        assert_eq!(rep.frozen_leg0_sign, 1); // Long Arcus (+1), Short Lighter Core (-1)
        assert_eq!(rep.status, "settled_witnessed");
        assert!(rep.cash_identity_reconciled);
        // 30h * $500 * (0.000070 - 0.000005) = $0.975 funding; exit fee on leg0 only: $500 * 2.25 bp = $0.1125.
        assert_eq!(rep.witnessed_funding_cash_usd, d("0.975"));
        assert_eq!(rep.fees_usd, d("0.1125"));
        assert_eq!(rep.net_variable_pnl_usd, d("0.8625"));
        assert_eq!(rep.terminal_equity_usd, d("1000.8625"));
        assert_eq!(rep.fixed_infra_usd, d("2"));
        assert_eq!(rep.net_all_in_pnl_usd, d("-1.1375"));

        let mut ambiguous = scenario;
        ambiguous.config.maker_entry_fee_bps = d("0.5");
        assert!(ambiguous.evaluate().is_err());
    }

    #[test]
    fn cash_components_reconcile_handles_decimal_rounding_and_detects_tampering() {
        let mut sc = scenario("2.0");
        sc.config.capital_usd = d("100000");
        sc.config.target_leg_notional_usd = d("333.3333");
        for f in &mut sc.frames {
            f.leg0_price = d("103.17");
            f.leg1_price = d("103.29");
        }
        let rep = sc.evaluate().unwrap();
        assert!(rep.cash_identity_reconciled);
        assert!(MultiDayCarryScenario::cash_components_reconcile(&rep));

        let mut rounded = rep.clone();
        rounded.witnessed_funding_cash_usd += d("0.000000000000000000000001");
        assert!(MultiDayCarryScenario::cash_components_reconcile(&rounded));

        for mutate in [
            |r: &mut MultiDayCarryReport| r.witnessed_funding_cash_usd -= d("0.01"),
            |r: &mut MultiDayCarryReport| r.terminal_equity_usd += d("0.01"),
            |r: &mut MultiDayCarryReport| r.net_all_in_pnl_usd += d("0.01"),
            |r: &mut MultiDayCarryReport| r.fixed_infra_usd = d("-1"),
            |r: &mut MultiDayCarryReport| {
                r.ten_hour_checkpoints
                    .last_mut()
                    .unwrap()
                    .cumulative_fees_usd += d("0.01");
            },
        ] {
            let mut tampered = rep.clone();
            mutate(&mut tampered);
            assert!(!MultiDayCarryScenario::cash_components_reconcile(&tampered));
        }
    }
}
