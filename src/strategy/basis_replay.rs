//! Venue-agnostic cross-venue basis continuous replay & ADL/Oracle stress evaluator (`S2 / S3`).
//!
//! Contains ZERO dependencies on `crate::exchanges::*`: drives [`BasisCoreEngine`] across
//! normalized two-leg [`BasisReplayFrame`] streams (including direct conversion from compiled
//! [`PerpFrame`] tapes) under `#694` continuous-coverage accounting, marketable-touch IOC
//! simulation, and deterministic ADL surviving-leg reduction checks.

use crate::daily_loss::DailyLossGuard;
use crate::strategy::basis_core::{
    BasisCoreAction, BasisCoreConfig, BasisCoreEngine, BasisLegConstraints, BasisMarketContext,
    FundingClockSample, NormalizedLegBook, ResidualRecoveryPolicy, quantize_shared_step_qty,
    quantize_step_qty,
};
use crate::strategy::kalman_basis::SessionRegime;
use crate::strategy::pipeline_driver::{
    DEFAULT_MAX_CONTINUOUS_EVAL_GAP_MS, FULL_10H_CONTINUOUS_MS, ReplayWindowSpec,
};
use crate::strategy::pipeline_tape::load_perp_frames_from_path;
use crate::strategy::relative_value::Frame as PerpFrame;
use crate::strategy::tail_risk::{AdlDeltaBreakerConfig, PairedLeg};
use anyhow::{Context, Result, ensure};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

fn sim_utc_day(now_ms: u64) -> i32 {
    let days_since_unix = (now_ms / 86_400_000).min(i32::MAX as u64 - 719_163) as i32;
    719_163 + days_since_unix
}

fn decimal_from_f64(value: f64) -> Option<Decimal> {
    if !value.is_finite() {
        return None;
    }
    Decimal::from_f64_retain(value)
}

/// Synthetic or replayed forced exchange Auto-Deleveraging position shrinkage on one leg.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AdlShockEvent {
    pub leg: PairedLeg,
    /// Fraction of open position forcibly closed (`(0, 1]`).
    pub cut_fraction: f64,
}

/// One chronological two-leg snapshot for [`run_continuous_basis_replay`].
#[derive(Debug, Clone, PartialEq)]
pub struct BasisReplayFrame {
    pub now_ms: u64,
    pub primary: NormalizedLegBook,
    pub hedge: NormalizedLegBook,
    pub reference_oracle_px: Option<Decimal>,
    pub regime: SessionRegime,
    pub primary_lease_ok: bool,
    pub adl_shock: Option<AdlShockEvent>,
    pub funding_clock: Option<FundingClockSample>,
    pub hedge_rtt_ms: Option<f64>,
    pub realized_vol_bps_per_sqrt_s: Option<f64>,
}

fn default_tape_session_regime() -> SessionRegime {
    SessionRegime::OffHours
}

/// Configuration for [`run_continuous_basis_replay`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BasisReplayConfig {
    pub core: BasisCoreConfig,
    pub constraints: BasisLegConstraints,
    pub window: ReplayWindowSpec,
    pub initial_capital_usd: f64,
    pub daily_loss_limit_pct: f64,
    pub primary_taker_fee_bps: f64,
    pub hedge_taker_fee_bps: f64,
    #[serde(default = "default_tape_session_regime")]
    pub tape_session_regime: SessionRegime,
}

impl BasisReplayConfig {
    pub fn validate(&self) -> Result<()> {
        self.core.validate()?;
        self.constraints.validate()?;
        self.window.validate()?;
        ensure!(
            self.initial_capital_usd.is_finite() && self.initial_capital_usd > 0.0,
            "initial_capital_usd must be finite and > 0"
        );
        ensure!(
            self.daily_loss_limit_pct.is_finite()
                && self.daily_loss_limit_pct > 0.0
                && self.daily_loss_limit_pct < 1.0,
            "daily_loss_limit_pct must be finite and in (0, 1)"
        );
        ensure!(
            self.primary_taker_fee_bps.is_finite()
                && self.hedge_taker_fee_bps.is_finite()
                && self.primary_taker_fee_bps >= 0.0
                && self.hedge_taker_fee_bps >= 0.0,
            "taker fees must be finite and >= 0"
        );
        Ok(())
    }
}

/// Full report emitted by [`run_continuous_basis_replay`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BasisReplayReport {
    pub eval_duration_ms: u64,
    pub observed_eval_span_ms: u64,
    pub max_eval_gap_ms: u64,
    pub is_full_10h_continuous: bool,
    pub warmup_ticks: u64,
    pub eval_ticks: u64,
    pub drain_ticks: u64,
    pub entries_filled: u64,
    pub exits_filled: u64,
    pub primary_adl_reduce_only_fills: u64,
    pub hedge_residual_fills: u64,
    pub non_reduce_only_residual_orders: u64,
    pub adl_halt_count: u64,
    pub oracle_blocked_ticks: u64,
    pub max_unplanned_residual_ms: u64,
    pub realized_pnl_usd: f64,
    pub fees_usd: f64,
    pub net_cash_usd: f64,
    pub peak_gross_notional_usd: f64,
    pub terminal_inv_primary: f64,
    pub terminal_inv_hedge: f64,
    pub daily_loss_risk_scale_end: f64,
}

struct ReplayCashLedger {
    cash_usd: f64,
    realized_pnl_usd: f64,
    fees_usd: f64,
}

struct LegTouchFill {
    is_buy: bool,
    qty: Decimal,
    exec_px: Decimal,
    fee_bps: f64,
}

fn fill_leg_at_touch(
    leg_pos: &mut Decimal,
    leg_cost_basis: &mut Decimal,
    ledger: &mut ReplayCashLedger,
    fill: LegTouchFill,
) -> Result<()> {
    let q_f = fill.qty.to_f64().context("qty overflow f64")?;
    let px_f = fill.exec_px.to_f64().context("price overflow f64")?;
    let notional = q_f * px_f;
    let fee = notional * (fill.fee_bps / 10_000.0);
    ensure!(
        fee.is_finite() && notional.is_finite(),
        "non-finite fill math"
    );
    ledger.fees_usd += fee;

    let signed_delta = if fill.is_buy { fill.qty } else { -fill.qty };
    let prev = *leg_pos;
    if prev.is_zero() || (prev > Decimal::ZERO) == fill.is_buy {
        let next_abs = prev.abs() + fill.qty;
        if next_abs > Decimal::ZERO {
            *leg_cost_basis = (prev.abs() * (*leg_cost_basis) + fill.qty * fill.exec_px) / next_abs;
        }
        *leg_pos = prev + signed_delta;
    } else {
        let close_qty = fill.qty.min(prev.abs());
        let close_sign = if prev > Decimal::ZERO {
            Decimal::ONE
        } else {
            -Decimal::ONE
        };
        let realized = (close_qty * (fill.exec_px - *leg_cost_basis) * close_sign)
            .to_f64()
            .context("realized PnL overflow")?;
        ledger.realized_pnl_usd += realized;
        *leg_pos = prev + signed_delta;
        if leg_pos.is_zero() {
            *leg_cost_basis = Decimal::ZERO;
        } else if (*leg_pos > Decimal::ZERO) != (prev > Decimal::ZERO) {
            *leg_cost_basis = fill.exec_px;
        }
    }

    let delta_cash = if fill.is_buy { -notional } else { notional };
    ledger.cash_usd += delta_cash - fee;
    Ok(())
}

fn reducible_qty(inv: Decimal, is_buy: bool) -> Decimal {
    if (is_buy && inv < Decimal::ZERO) || (!is_buy && inv > Decimal::ZERO) {
        inv.abs()
    } else {
        Decimal::ZERO
    }
}

/// Run a continuous cross-venue basis replay across `warmup -> eval -> drain` without
/// resetting inventory, equity, or `DailyLossGuard`.
pub fn run_continuous_basis_replay(
    cfg: &BasisReplayConfig,
    frames: &[BasisReplayFrame],
) -> Result<BasisReplayReport> {
    cfg.validate()?;
    ensure!(!frames.is_empty(), "basis replay frames must not be empty");

    let mut engine = BasisCoreEngine::new(cfg.core.clone(), cfg.constraints)?;
    let mut daily_loss = DailyLossGuard::open(
        None,
        cfg.daily_loss_limit_pct,
        cfg.initial_capital_usd,
        sim_utc_day(cfg.window.warmup_start_ms),
    )?;

    let dust = cfg
        .constraints
        .primary_step_size
        .min(cfg.constraints.hedge_step_size)
        * Decimal::new(5, 1);

    let mut inv_primary = Decimal::ZERO;
    let mut inv_hedge = Decimal::ZERO;
    let mut primary_cost = Decimal::ZERO;
    let mut hedge_cost = Decimal::ZERO;
    let mut ledger = ReplayCashLedger {
        cash_usd: cfg.initial_capital_usd,
        realized_pnl_usd: 0.0_f64,
        fees_usd: 0.0_f64,
    };

    let mut prev_ms = 0_u64;
    let mut first_eval_ms: Option<u64> = None;
    let mut last_eval_ms: Option<u64> = None;
    let mut prev_eval_ms: Option<u64> = None;
    let mut max_eval_gap_ms = 0_u64;

    let mut warmup_ticks = 0_u64;
    let mut eval_ticks = 0_u64;
    let mut drain_ticks = 0_u64;
    let mut entries_filled = 0_u64;
    let mut exits_filled = 0_u64;
    let mut primary_adl_reduce_only_fills = 0_u64;
    let mut hedge_residual_fills = 0_u64;
    let mut non_reduce_only_residual_orders = 0_u64;
    let mut adl_halt_count = 0_u64;
    let mut oracle_blocked_ticks = 0_u64;
    let mut residual_since_ms: Option<u64> = None;
    let mut max_unplanned_residual_ms = 0_u64;
    let mut peak_gross_notional_usd = 0.0_f64;

    for (idx, frame) in frames.iter().enumerate() {
        let now_ms = frame.now_ms;
        ensure!(now_ms > 0, "frame {idx}: now_ms must be > 0");
        ensure!(
            now_ms >= cfg.window.warmup_start_ms && now_ms <= cfg.window.drain_end_ms,
            "frame {idx}: timestamp {now_ms} outside [warmup_start_ms, drain_end_ms]"
        );
        if prev_ms > 0 {
            ensure!(
                now_ms > prev_ms,
                "frame {idx}: non-increasing timestamp {now_ms} <= {prev_ms}"
            );
        }
        prev_ms = now_ms;

        let in_warmup = now_ms < cfg.window.eval_start_ms;
        let in_drain = now_ms > cfg.window.eval_end_ms;
        if in_warmup {
            warmup_ticks += 1;
        } else if in_drain {
            drain_ticks += 1;
        } else {
            eval_ticks += 1;
            if first_eval_ms.is_none() {
                first_eval_ms = Some(now_ms);
            }
            if let Some(prev_eval) = prev_eval_ms {
                max_eval_gap_ms = max_eval_gap_ms.max(now_ms.saturating_sub(prev_eval));
            }
            prev_eval_ms = Some(now_ms);
            last_eval_ms = Some(now_ms);
        }

        ensure!(
            frame.primary.bid_px > Decimal::ZERO
                && frame.primary.ask_px > frame.primary.bid_px
                && frame.hedge.bid_px > Decimal::ZERO
                && frame.hedge.ask_px > frame.hedge.bid_px,
            "frame {idx}: primary and hedge books must have positive non-crossed top prices"
        );

        let pm = ((frame.primary.bid_px + frame.primary.ask_px) * Decimal::new(5, 1))
            .to_f64()
            .context("primary mid overflow")?;
        let hm = ((frame.hedge.bid_px + frame.hedge.ask_px) * Decimal::new(5, 1))
            .to_f64()
            .context("hedge mid overflow")?;

        if let Some(shock) = frame.adl_shock {
            ensure!(
                shock.cut_fraction.is_finite()
                    && shock.cut_fraction > 0.0
                    && shock.cut_fraction <= 1.0,
                "frame {idx}: ADL cut_fraction must be in (0, 1]"
            );
            let frac_dec =
                decimal_from_f64(shock.cut_fraction).context("non-finite ADL cut_fraction")?;
            match shock.leg {
                PairedLeg::Primary if inv_primary.abs() > dust => {
                    let cut_qty = quantize_step_qty(
                        inv_primary.abs() * frac_dec,
                        cfg.constraints.primary_step_size,
                    )?;
                    let bounded_cut = cut_qty.min(inv_primary.abs());
                    if bounded_cut > Decimal::ZERO {
                        let buy_to_cut = inv_primary < Decimal::ZERO;
                        let mid_dec =
                            (frame.primary.bid_px + frame.primary.ask_px) * Decimal::new(5, 1);
                        fill_leg_at_touch(
                            &mut inv_primary,
                            &mut primary_cost,
                            &mut ledger,
                            LegTouchFill {
                                is_buy: buy_to_cut,
                                qty: bounded_cut,
                                exec_px: mid_dec,
                                fee_bps: 0.0,
                            },
                        )?;
                    }
                }
                PairedLeg::Hedge if inv_hedge.abs() > dust => {
                    let cut_qty = quantize_step_qty(
                        inv_hedge.abs() * frac_dec,
                        cfg.constraints.hedge_step_size,
                    )?;
                    let bounded_cut = cut_qty.min(inv_hedge.abs());
                    if bounded_cut > Decimal::ZERO {
                        let buy_to_cut = inv_hedge < Decimal::ZERO;
                        let mid_dec =
                            (frame.hedge.bid_px + frame.hedge.ask_px) * Decimal::new(5, 1);
                        fill_leg_at_touch(
                            &mut inv_hedge,
                            &mut hedge_cost,
                            &mut ledger,
                            LegTouchFill {
                                is_buy: buy_to_cut,
                                qty: bounded_cut,
                                exec_px: mid_dec,
                                fee_bps: 0.0,
                            },
                        )?;
                    }
                }
                _ => {}
            }
            engine.reconcile_positions(
                inv_primary,
                inv_hedge,
                frame.primary.receipt_ms,
                frame.hedge.receipt_ms,
                now_ms,
            );
        }

        let gross_ntl = inv_primary.abs().to_f64().unwrap_or(0.0) * pm
            + inv_hedge.abs().to_f64().unwrap_or(0.0) * hm;
        peak_gross_notional_usd = peak_gross_notional_usd.max(gross_ntl);

        let mtm_unrealized_p = ((frame.primary.bid_px + frame.primary.ask_px) * Decimal::new(5, 1)
            - primary_cost)
            * inv_primary;
        let mtm_unrealized_h = ((frame.hedge.bid_px + frame.hedge.ask_px) * Decimal::new(5, 1)
            - hedge_cost)
            * inv_hedge;
        let eq_delta = ledger.realized_pnl_usd - ledger.fees_usd
            + mtm_unrealized_p.to_f64().unwrap_or(0.0)
            + mtm_unrealized_h.to_f64().unwrap_or(0.0);
        let equity_usd = cfg.initial_capital_usd + eq_delta;
        ensure!(
            equity_usd.is_finite(),
            "frame {idx}: equity became non-finite"
        );
        let _ = daily_loss.check(equity_usd, sim_utc_day(now_ms));

        let prev_oracle_blocks = engine.counters.oracle_blocked_entries;
        let action = engine.on_books_with_context(
            now_ms,
            &frame.primary,
            &frame.hedge,
            &daily_loss,
            frame.primary_lease_ok,
            frame.reference_oracle_px,
            frame.regime,
            BasisMarketContext {
                funding_clock: frame.funding_clock,
                reference_oracles: None,
                hedge_rtt_ms: frame.hedge_rtt_ms,
                realized_vol_bps_per_sqrt_s: frame.realized_vol_bps_per_sqrt_s,
            },
        );
        if engine.counters.oracle_blocked_entries > prev_oracle_blocks {
            oracle_blocked_ticks += 1;
        }

        match action {
            BasisCoreAction::Hold { .. } => {}
            BasisCoreAction::HaltAndFlatten { .. } => {
                adl_halt_count += 1;
            }
            BasisCoreAction::EmergencyReducePrimary {
                is_buy,
                qty,
                limit_price,
                reduce_only,
            } => {
                if !reduce_only {
                    non_reduce_only_residual_orders += 1;
                }
                let exec_px = if is_buy {
                    frame.primary.ask_px
                } else {
                    frame.primary.bid_px
                };
                let marketable = if is_buy {
                    limit_price >= exec_px
                } else {
                    limit_price <= exec_px
                };
                let vis = frame.primary.visible_size_within_limit(is_buy, limit_price);
                let exec_qty = if marketable {
                    let cap = if reduce_only {
                        qty.min(vis).min(reducible_qty(inv_primary, is_buy))
                    } else {
                        qty.min(vis)
                    };
                    quantize_step_qty(cap, cfg.constraints.primary_step_size)
                        .unwrap_or(Decimal::ZERO)
                } else {
                    Decimal::ZERO
                };
                if exec_qty > Decimal::ZERO {
                    fill_leg_at_touch(
                        &mut inv_primary,
                        &mut primary_cost,
                        &mut ledger,
                        LegTouchFill {
                            is_buy,
                            qty: exec_qty,
                            exec_px,
                            fee_bps: cfg.primary_taker_fee_bps,
                        },
                    )?;
                    if reduce_only {
                        primary_adl_reduce_only_fills += 1;
                    }
                }
                engine.reconcile_positions(
                    inv_primary,
                    inv_hedge,
                    frame.primary.receipt_ms,
                    frame.hedge.receipt_ms,
                    now_ms,
                );
            }
            BasisCoreAction::EmergencyReduceHedge {
                is_buy,
                qty,
                limit_price,
                reduce_only,
            } => {
                if !reduce_only {
                    non_reduce_only_residual_orders += 1;
                }
                let exec_px = if is_buy {
                    frame.hedge.ask_px
                } else {
                    frame.hedge.bid_px
                };
                let marketable = if is_buy {
                    limit_price >= exec_px
                } else {
                    limit_price <= exec_px
                };
                let vis = frame.hedge.visible_size_within_limit(is_buy, limit_price);
                let exec_qty = if marketable {
                    let cap = if reduce_only {
                        qty.min(vis).min(reducible_qty(inv_hedge, is_buy))
                    } else {
                        qty.min(vis)
                    };
                    quantize_step_qty(cap, cfg.constraints.hedge_step_size).unwrap_or(Decimal::ZERO)
                } else {
                    Decimal::ZERO
                };
                if exec_qty > Decimal::ZERO {
                    fill_leg_at_touch(
                        &mut inv_hedge,
                        &mut hedge_cost,
                        &mut ledger,
                        LegTouchFill {
                            is_buy,
                            qty: exec_qty,
                            exec_px,
                            fee_bps: cfg.hedge_taker_fee_bps,
                        },
                    )?;
                    hedge_residual_fills += 1;
                }
                engine.reconcile_positions(
                    inv_primary,
                    inv_hedge,
                    frame.primary.receipt_ms,
                    frame.hedge.receipt_ms,
                    now_ms,
                );
            }
            BasisCoreAction::ExitPairIoc {
                direction_primary_buy,
                qty,
                primary_limit_px,
                hedge_limit_px,
                ..
            } => {
                let p_exec = if direction_primary_buy {
                    frame.primary.ask_px
                } else {
                    frame.primary.bid_px
                };
                let h_exec = if !direction_primary_buy {
                    frame.hedge.ask_px
                } else {
                    frame.hedge.bid_px
                };
                let p_mkt = if direction_primary_buy {
                    primary_limit_px >= p_exec
                } else {
                    primary_limit_px <= p_exec
                };
                let h_mkt = if !direction_primary_buy {
                    hedge_limit_px >= h_exec
                } else {
                    hedge_limit_px <= h_exec
                };
                if p_mkt && h_mkt && qty > Decimal::ZERO {
                    let p_vis = frame
                        .primary
                        .visible_size_within_limit(direction_primary_buy, primary_limit_px);
                    let h_vis = frame
                        .hedge
                        .visible_size_within_limit(!direction_primary_buy, hedge_limit_px);
                    let raw_close = qty
                        .min(p_vis)
                        .min(h_vis)
                        .min(reducible_qty(inv_primary, direction_primary_buy))
                        .min(reducible_qty(inv_hedge, !direction_primary_buy));
                    let close_q = if raw_close > Decimal::ZERO {
                        quantize_shared_step_qty(
                            raw_close,
                            cfg.constraints.primary_step_size,
                            cfg.constraints.hedge_step_size,
                        )
                        .unwrap_or(Decimal::ZERO)
                    } else {
                        Decimal::ZERO
                    };
                    if close_q > Decimal::ZERO {
                        fill_leg_at_touch(
                            &mut inv_primary,
                            &mut primary_cost,
                            &mut ledger,
                            LegTouchFill {
                                is_buy: direction_primary_buy,
                                qty: close_q,
                                exec_px: p_exec,
                                fee_bps: cfg.primary_taker_fee_bps,
                            },
                        )?;
                        fill_leg_at_touch(
                            &mut inv_hedge,
                            &mut hedge_cost,
                            &mut ledger,
                            LegTouchFill {
                                is_buy: !direction_primary_buy,
                                qty: close_q,
                                exec_px: h_exec,
                                fee_bps: cfg.hedge_taker_fee_bps,
                            },
                        )?;
                        exits_filled += 1;
                    }
                }
                engine.reconcile_positions(
                    inv_primary,
                    inv_hedge,
                    frame.primary.receipt_ms,
                    frame.hedge.receipt_ms,
                    now_ms,
                );
            }
            BasisCoreAction::EnterPairIoc {
                direction_primary_buy,
                qty,
                primary_limit_px,
                hedge_limit_px,
                ..
            } => {
                if !in_warmup && !in_drain && qty > Decimal::ZERO {
                    let p_exec = if direction_primary_buy {
                        frame.primary.ask_px
                    } else {
                        frame.primary.bid_px
                    };
                    let h_exec = if !direction_primary_buy {
                        frame.hedge.ask_px
                    } else {
                        frame.hedge.bid_px
                    };
                    let p_mkt = if direction_primary_buy {
                        primary_limit_px >= p_exec
                    } else {
                        primary_limit_px <= p_exec
                    };
                    let h_mkt = if !direction_primary_buy {
                        hedge_limit_px >= h_exec
                    } else {
                        hedge_limit_px <= h_exec
                    };
                    if p_mkt && h_mkt {
                        let p_vis = frame
                            .primary
                            .visible_size_within_limit(direction_primary_buy, primary_limit_px);
                        let h_vis = frame
                            .hedge
                            .visible_size_within_limit(!direction_primary_buy, hedge_limit_px);
                        let raw_fill = qty.min(p_vis).min(h_vis);
                        let fill_q = if raw_fill > Decimal::ZERO {
                            quantize_shared_step_qty(
                                raw_fill,
                                cfg.constraints.primary_step_size,
                                cfg.constraints.hedge_step_size,
                            )
                            .unwrap_or(Decimal::ZERO)
                        } else {
                            Decimal::ZERO
                        };
                        if fill_q > Decimal::ZERO {
                            fill_leg_at_touch(
                                &mut inv_primary,
                                &mut primary_cost,
                                &mut ledger,
                                LegTouchFill {
                                    is_buy: direction_primary_buy,
                                    qty: fill_q,
                                    exec_px: p_exec,
                                    fee_bps: cfg.primary_taker_fee_bps,
                                },
                            )?;
                            fill_leg_at_touch(
                                &mut inv_hedge,
                                &mut hedge_cost,
                                &mut ledger,
                                LegTouchFill {
                                    is_buy: !direction_primary_buy,
                                    qty: fill_q,
                                    exec_px: h_exec,
                                    fee_bps: cfg.hedge_taker_fee_bps,
                                },
                            )?;
                            entries_filled += 1;
                        }
                    }
                }
                engine.reconcile_positions(
                    inv_primary,
                    inv_hedge,
                    frame.primary.receipt_ms,
                    frame.hedge.receipt_ms,
                    now_ms,
                );
            }
        }

        if (inv_primary + inv_hedge).abs() > dust {
            let start = *residual_since_ms.get_or_insert(now_ms);
            max_unplanned_residual_ms = max_unplanned_residual_ms.max(now_ms.saturating_sub(start));
        } else {
            residual_since_ms = None;
        }
    }

    let (observed_eval_span_ms, effective_max_eval_gap_ms) =
        if let (Some(first), Some(last)) = (first_eval_ms, last_eval_ms) {
            let lead_gap = first.saturating_sub(cfg.window.eval_start_ms);
            let tail_gap = cfg.window.eval_end_ms.saturating_sub(last);
            (
                last.saturating_sub(first),
                max_eval_gap_ms.max(lead_gap).max(tail_gap),
            )
        } else {
            (0, cfg.window.eval_duration_ms())
        };
    let is_full_10h_continuous = cfg.window.is_full_10h_continuous()
        && eval_ticks >= 2
        && observed_eval_span_ms >= FULL_10H_CONTINUOUS_MS
        && effective_max_eval_gap_ms <= DEFAULT_MAX_CONTINUOUS_EVAL_GAP_MS;

    Ok(BasisReplayReport {
        eval_duration_ms: cfg.window.eval_duration_ms(),
        observed_eval_span_ms,
        max_eval_gap_ms: effective_max_eval_gap_ms,
        is_full_10h_continuous,
        warmup_ticks,
        eval_ticks,
        drain_ticks,
        entries_filled,
        exits_filled,
        primary_adl_reduce_only_fills,
        hedge_residual_fills,
        non_reduce_only_residual_orders,
        adl_halt_count,
        oracle_blocked_ticks,
        max_unplanned_residual_ms,
        realized_pnl_usd: ledger.realized_pnl_usd,
        fees_usd: ledger.fees_usd,
        net_cash_usd: ledger.realized_pnl_usd - ledger.fees_usd,
        peak_gross_notional_usd,
        terminal_inv_primary: inv_primary.to_f64().unwrap_or(0.0),
        terminal_inv_hedge: inv_hedge.to_f64().unwrap_or(0.0),
        daily_loss_risk_scale_end: daily_loss.risk_scale(),
    })
}

fn perp_book_to_leg_book(
    now_ms: u64,
    book: &crate::strategy::relative_value::Book,
    base_per_contract: Decimal,
) -> Option<NormalizedLegBook> {
    if !book.tradable || book.timestamp_ms == 0 || book.timestamp_ms > now_ms {
        return None;
    }
    let mut bids = Vec::with_capacity(book.bids.len());
    for lvl in &book.bids {
        let sz = lvl.contracts * base_per_contract;
        if lvl.price > Decimal::ZERO && sz > Decimal::ZERO {
            bids.push((lvl.price, sz));
        }
    }
    let mut asks = Vec::with_capacity(book.asks.len());
    for lvl in &book.asks {
        let sz = lvl.contracts * base_per_contract;
        if lvl.price > Decimal::ZERO && sz > Decimal::ZERO {
            asks.push((lvl.price, sz));
        }
    }
    let (bid_px, bid_sz) = *bids.first()?;
    let (ask_px, ask_sz) = *asks.first()?;
    if ask_px <= bid_px {
        return None;
    }
    Some(NormalizedLegBook {
        source_ms: book.timestamp_ms,
        receipt_ms: book.timestamp_ms,
        bid_px,
        bid_sz,
        ask_px,
        ask_sz,
        bids,
        asks,
    })
}

/// Convert compiled `PerpFrame` snapshots into paired `BasisReplayFrame`s for `BasisCoreEngine`.
pub fn basis_frames_from_perp_frames(
    frames: &[PerpFrame],
    primary_leg: usize,
    primary_base_per_contract: Decimal,
    hedge_base_per_contract: Decimal,
    default_regime: SessionRegime,
) -> Result<Vec<BasisReplayFrame>> {
    ensure!(primary_leg <= 1, "primary_leg must be 0 or 1");
    ensure!(
        primary_base_per_contract > Decimal::ZERO && hedge_base_per_contract > Decimal::ZERO,
        "base_per_contract multipliers must be > 0"
    );
    let hedge_leg = 1 - primary_leg;
    let mut out = Vec::with_capacity(frames.len());
    let mut prev_ms = 0_u64;
    for (idx, frame) in frames.iter().enumerate() {
        ensure!(frame.now_ms > 0, "frame {idx}: now_ms must be > 0");
        if prev_ms > 0 {
            ensure!(
                frame.now_ms > prev_ms,
                "frame {idx}: non-increasing timestamp"
            );
        }
        prev_ms = frame.now_ms;
        let Some(primary) = perp_book_to_leg_book(
            frame.now_ms,
            &frame.legs[primary_leg].book,
            primary_base_per_contract,
        ) else {
            continue;
        };
        let Some(hedge) = perp_book_to_leg_book(
            frame.now_ms,
            &frame.legs[hedge_leg].book,
            hedge_base_per_contract,
        ) else {
            continue;
        };
        let oracle_px = (frame.reference.timestamp_ms > 0
            && frame.reference.timestamp_ms <= frame.now_ms
            && frame.reference.price > Decimal::ZERO)
            .then_some(frame.reference.price);
        out.push(BasisReplayFrame {
            now_ms: frame.now_ms,
            primary,
            hedge,
            reference_oracle_px: oracle_px,
            regime: default_regime,
            primary_lease_ok: true,
            adl_shock: None,
            funding_clock: None,
            hedge_rtt_ms: None,
            realized_vol_bps_per_sqrt_s: None,
        });
    }
    ensure!(
        !out.is_empty(),
        "no tradable paired basis frames converted from perp frames"
    );
    Ok(out)
}

/// Offline CLI loader & evaluator for `cross_venue basis-replay <CONFIG> <TAPE>`.
pub fn run_basis_tape_file(config_path: &Path, tape_path: &Path) -> Result<BasisReplayReport> {
    ensure!(
        config_path.is_file(),
        "config file '{}' does not exist or is not a file",
        config_path.display()
    );
    let raw_cfg = fs::read_to_string(config_path)
        .with_context(|| format!("read basis config '{}'", config_path.display()))?;
    let cfg: BasisReplayConfig = serde_json::from_str(&raw_cfg)
        .with_context(|| format!("parse BasisReplayConfig '{}'", config_path.display()))?;
    cfg.validate()?;
    let perp_frames = load_perp_frames_from_path(tape_path)?;
    let mut basis_frames = basis_frames_from_perp_frames(
        &perp_frames,
        0,
        Decimal::ONE,
        Decimal::ONE,
        cfg.tape_session_regime,
    )?;
    basis_frames
        .retain(|f| f.now_ms >= cfg.window.warmup_start_ms && f.now_ms <= cfg.window.drain_end_ms);
    ensure!(
        !basis_frames.is_empty(),
        "no paired basis frames inside configured replay window"
    );
    run_continuous_basis_replay(&cfg, &basis_frames)
}

/// Paired synthetic diagnostic comparing classic hedge-leg fallback vs `PreferReduceOnlySurvivingLeg`
/// ADL protection & mandatory reference-oracle entry gating.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BasisBenchmarkReport {
    pub disclaimer: String,
    pub classic_hedge_arm: BasisReplayReport,
    pub adl_shielded_arm: BasisReplayReport,
}

fn dec_s(s: &str) -> Decimal {
    Decimal::from_str_exact(s).unwrap_or(Decimal::ONE)
}

/// Deterministic offline benchmark for `cross_venue basis-bench`.
pub fn run_basis_benchmark() -> Result<BasisBenchmarkReport> {
    let window = ReplayWindowSpec {
        warmup_start_ms: 1_000_000,
        eval_start_ms: 1_002_000,
        eval_end_ms: 1_020_000,
        drain_end_ms: 1_025_000,
    };

    let mut frames = Vec::new();
    for step in 0..=25_u64 {
        let now_ms = 1_000_000 + step * 1_000;
        let (p_bid, p_ask, h_bid, h_ask, oracle, adl_shock) = if (3..=5).contains(&step) {
            // Valid +18 bp entry dislocation aligned with oracle.
            ("99.90", "99.92", "100.10", "100.12", "100.00", None)
        } else if step == 7 {
            // Sudden 40% ADL cut on the profitable short Hedge leg!
            (
                "100.00",
                "100.02",
                "100.00",
                "100.02",
                "100.00",
                Some(AdlShockEvent {
                    leg: PairedLeg::Hedge,
                    cut_fraction: 0.40,
                }),
            )
        } else if (11..=13).contains(&step) {
            // Dislocated oracle trap (+45 bp oracle skew while spread looks attractive).
            ("99.90", "99.92", "100.12", "100.14", "100.45", None)
        } else if step >= 15 {
            // Exit spread convergence.
            ("100.05", "100.07", "99.99", "100.01", "100.03", None)
        } else {
            ("99.99", "100.01", "99.99", "100.01", "100.00", None)
        };

        frames.push(BasisReplayFrame {
            now_ms,
            primary: NormalizedLegBook::from_l1(
                now_ms,
                now_ms,
                dec_s(p_bid),
                dec_s("20"),
                dec_s(p_ask),
                dec_s("20"),
            )?,
            hedge: NormalizedLegBook::from_l1(
                now_ms,
                now_ms,
                dec_s(h_bid),
                dec_s("20"),
                dec_s(h_ask),
                dec_s("20"),
            )?,
            reference_oracle_px: Some(dec_s(oracle)),
            regime: SessionRegime::UsRth,
            primary_lease_ok: true,
            adl_shock,
            funding_clock: None,
            hedge_rtt_ms: None,
            realized_vol_bps_per_sqrt_s: None,
        });
    }

    let base_core = BasisCoreConfig {
        primary_venue_id: "venue_a".into(),
        hedge_venue_id: "venue_b".into(),
        entry_bp: 10.0,
        exit_bp: 0.0,
        max_entry_bp: 35.0,
        persist_ms: 0,
        post_trade_refresh_lock: false,
        clip_usd: 200.0,
        max_notional_usd: 400.0,
        max_order_usd: 250.0,
        max_total_notional_usd: 400.0,
        ladder_floor_frac: 0.25,
        ladder_scale_bps: 2.0,
        ladder_convex_power: 1.0,
        max_hold_ms: 3_600_000,
        max_primary_book_age_ms: 250,
        max_hedge_book_age_ms: 2_000,
        max_venue_clock_skew_ms: 2_000,
        slippage_bps: 8.0,
        max_oracle_divergence_bps: 25.0,
        require_reference_oracle_for_entry: true,
        residual_policy: ResidualRecoveryPolicy::PreferReduceOnlySurvivingLeg,
        adl_delta_breaker: AdlDeltaBreakerConfig::default(),
        kalman_basis: None,
        funding_settlement: Default::default(),
        multi_oracle: None,
        latency_budget: Default::default(),
    };
    let constraints = BasisLegConstraints {
        primary_step_size: dec_s("0.01"),
        primary_min_order_size: dec_s("0.01"),
        primary_min_order_notional_usd: 5.0,
        hedge_step_size: dec_s("0.01"),
        hedge_min_order_size: dec_s("0.01"),
        hedge_min_order_notional_usd: 10.0,
    };

    let mut classic_core = base_core.clone();
    classic_core.residual_policy = ResidualRecoveryPolicy::PreferHedgeVenue;
    classic_core.max_oracle_divergence_bps = 100.0;
    classic_core.require_reference_oracle_for_entry = false;

    let classic_cfg = BasisReplayConfig {
        core: classic_core,
        constraints,
        window,
        initial_capital_usd: 1_000.0,
        daily_loss_limit_pct: 0.05,
        primary_taker_fee_bps: 2.0,
        hedge_taker_fee_bps: 2.0,
        tape_session_regime: default_tape_session_regime(),
    };
    let shielded_cfg = BasisReplayConfig {
        core: base_core,
        constraints,
        window,
        initial_capital_usd: 1_000.0,
        daily_loss_limit_pct: 0.05,
        primary_taker_fee_bps: 2.0,
        hedge_taker_fee_bps: 2.0,
        tape_session_regime: default_tape_session_regime(),
    };

    let classic_hedge_arm = run_continuous_basis_replay(&classic_cfg, &frames)?;
    let adl_shielded_arm = run_continuous_basis_replay(&shielded_cfg, &frames)?;

    Ok(BasisBenchmarkReport {
        disclaimer: "synthetic_diagnostic_not_10h_oos_evidence".into(),
        classic_hedge_arm,
        adl_shielded_arm,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basis_benchmark_uses_surviving_leg_reduce_only_and_blocks_oracle_trap() {
        let report = run_basis_benchmark().unwrap();
        assert!(!report.adl_shielded_arm.is_full_10h_continuous);
        assert_eq!(report.adl_shielded_arm.non_reduce_only_residual_orders, 0);
        assert!(report.adl_shielded_arm.primary_adl_reduce_only_fills > 0);
        assert!(report.adl_shielded_arm.oracle_blocked_ticks > 0);
        assert!(report.classic_hedge_arm.non_reduce_only_residual_orders > 0);
    }

    #[test]
    fn basis_replay_enforces_694_ten_hour_continuous_coverage_and_fail_closed_inputs() {
        let cfg = BasisReplayConfig {
            core: BasisCoreConfig {
                primary_venue_id: "a".into(),
                hedge_venue_id: "b".into(),
                entry_bp: 10.0,
                exit_bp: 0.0,
                max_entry_bp: 35.0,
                persist_ms: 0,
                post_trade_refresh_lock: false,
                clip_usd: 50.0,
                max_notional_usd: 100.0,
                max_order_usd: 100.0,
                max_total_notional_usd: 100.0,
                ladder_floor_frac: 0.25,
                ladder_scale_bps: 2.0,
                ladder_convex_power: 1.0,
                max_hold_ms: 3_600_000,
                max_primary_book_age_ms: 250,
                max_hedge_book_age_ms: 2_000,
                max_venue_clock_skew_ms: 2_000,
                slippage_bps: 8.0,
                max_oracle_divergence_bps: 25.0,
                require_reference_oracle_for_entry: true,
                residual_policy: ResidualRecoveryPolicy::PreferReduceOnlySurvivingLeg,
                adl_delta_breaker: AdlDeltaBreakerConfig::default(),
                kalman_basis: None,
                funding_settlement: Default::default(),
                multi_oracle: None,
                latency_budget: Default::default(),
            },
            constraints: BasisLegConstraints::default(),
            window: ReplayWindowSpec {
                warmup_start_ms: 1_000,
                eval_start_ms: 2_000,
                eval_end_ms: 2_000 + FULL_10H_CONTINUOUS_MS,
                drain_end_ms: 2_000 + FULL_10H_CONTINUOUS_MS,
            },
            initial_capital_usd: 1_000.0,
            daily_loss_limit_pct: 0.05,
            primary_taker_fee_bps: 2.0,
            hedge_taker_fee_bps: 2.0,
            tape_session_regime: SessionRegime::OffHours,
        };
        let make_frame = |now_ms: u64, adl_shock: Option<AdlShockEvent>| BasisReplayFrame {
            now_ms,
            primary: NormalizedLegBook::from_l1(
                now_ms,
                now_ms,
                dec_s("100.0"),
                dec_s("5"),
                dec_s("100.1"),
                dec_s("5"),
            )
            .unwrap(),
            hedge: NormalizedLegBook::from_l1(
                now_ms,
                now_ms,
                dec_s("100.0"),
                dec_s("5"),
                dec_s("100.1"),
                dec_s("5"),
            )
            .unwrap(),
            reference_oracle_px: Some(dec_s("100.05")),
            regime: SessionRegime::UsRth,
            primary_lease_ok: true,
            adl_shock,
            funding_clock: None,
            hedge_rtt_ms: None,
            realized_vol_bps_per_sqrt_s: None,
        };

        // Gapped endpoints spanning 10h MUST report `is_full_10h_continuous == false`.
        let gapped = vec![
            make_frame(2_000, None),
            make_frame(2_000 + FULL_10H_CONTINUOUS_MS, None),
        ];
        let rep_gapped = run_continuous_basis_replay(&cfg, &gapped).unwrap();
        assert!(!rep_gapped.is_full_10h_continuous);

        // Dense 60s cadence across full 10h reports `is_full_10h_continuous == true`.
        let dense: Vec<BasisReplayFrame> = (0..=600_u64)
            .map(|i| make_frame(2_000 + i * 60_000, None))
            .collect();
        let rep_dense = run_continuous_basis_replay(&cfg, &dense).unwrap();
        assert!(rep_dense.is_full_10h_continuous);

        // Regressed clock and invalid ADL cut_fraction fail closed.
        assert!(
            run_continuous_basis_replay(&cfg, &[make_frame(3_000, None), make_frame(2_500, None)])
                .is_err()
        );
        assert!(
            run_continuous_basis_replay(
                &cfg,
                &[make_frame(
                    2_500,
                    Some(AdlShockEvent {
                        leg: PairedLeg::Hedge,
                        cut_fraction: 1.5,
                    }),
                )]
            )
            .is_err()
        );
    }

    #[test]
    fn basis_replay_blocks_warmup_entries_enforces_reduce_only_direction_and_respects_regime() {
        let cfg = BasisReplayConfig {
            core: BasisCoreConfig {
                primary_venue_id: "a".into(),
                hedge_venue_id: "b".into(),
                entry_bp: 10.0,
                exit_bp: 0.0,
                max_entry_bp: 35.0,
                persist_ms: 0,
                post_trade_refresh_lock: false,
                clip_usd: 200.0,
                max_notional_usd: 400.0,
                max_order_usd: 250.0,
                max_total_notional_usd: 400.0,
                ladder_floor_frac: 0.25,
                ladder_scale_bps: 2.0,
                ladder_convex_power: 1.0,
                max_hold_ms: 3_600_000,
                max_primary_book_age_ms: 250,
                max_hedge_book_age_ms: 2_000,
                max_venue_clock_skew_ms: 2_000,
                slippage_bps: 8.0,
                max_oracle_divergence_bps: 25.0,
                require_reference_oracle_for_entry: true,
                residual_policy: ResidualRecoveryPolicy::PreferReduceOnlySurvivingLeg,
                adl_delta_breaker: AdlDeltaBreakerConfig::default(),
                kalman_basis: None,
                funding_settlement: Default::default(),
                multi_oracle: None,
                latency_budget: Default::default(),
            },
            constraints: BasisLegConstraints {
                primary_step_size: dec_s("0.01"),
                primary_min_order_size: dec_s("0.01"),
                primary_min_order_notional_usd: 5.0,
                hedge_step_size: dec_s("0.01"),
                hedge_min_order_size: dec_s("0.01"),
                hedge_min_order_notional_usd: 10.0,
            },
            window: ReplayWindowSpec {
                warmup_start_ms: 1_000,
                eval_start_ms: 2_000,
                eval_end_ms: 10_000,
                drain_end_ms: 12_000,
            },
            initial_capital_usd: 1_000.0,
            daily_loss_limit_pct: 0.05,
            primary_taker_fee_bps: 2.0,
            hedge_taker_fee_bps: 2.0,
            tape_session_regime: SessionRegime::OffHours,
        };

        // Strong >10 bp dislocation inside warmup window (now_ms < eval_start_ms) must NOT open.
        let warmup_frame = BasisReplayFrame {
            now_ms: 1_500,
            primary: NormalizedLegBook::from_l1(
                1_500,
                1_500,
                dec_s("99.90"),
                dec_s("20"),
                dec_s("99.92"),
                dec_s("20"),
            )
            .unwrap(),
            hedge: NormalizedLegBook::from_l1(
                1_500,
                1_500,
                dec_s("100.10"),
                dec_s("20"),
                dec_s("100.12"),
                dec_s("20"),
            )
            .unwrap(),
            reference_oracle_px: Some(dec_s("100.00")),
            regime: SessionRegime::UsRth,
            primary_lease_ok: true,
            adl_shock: None,
            funding_clock: None,
            hedge_rtt_ms: None,
            realized_vol_bps_per_sqrt_s: None,
        };
        let rep = run_continuous_basis_replay(&cfg, &[warmup_frame]).unwrap();
        assert_eq!(rep.warmup_ticks, 1);
        assert_eq!(rep.eval_ticks, 0);
        assert_eq!(rep.entries_filled, 0);
        assert_eq!(rep.fees_usd, 0.0);
        assert_eq!(rep.terminal_inv_primary, 0.0);
        assert_eq!(rep.terminal_inv_hedge, 0.0);

        // Reduce-only direction helper never allows same-sign expansion or flat increase.
        assert_eq!(reducible_qty(Decimal::ZERO, true), Decimal::ZERO);
        assert_eq!(reducible_qty(Decimal::ZERO, false), Decimal::ZERO);
        assert_eq!(reducible_qty(dec_s("2"), true), Decimal::ZERO);
        assert_eq!(reducible_qty(dec_s("-2"), false), Decimal::ZERO);
        assert_eq!(reducible_qty(dec_s("2"), false), dec_s("2"));
        assert_eq!(reducible_qty(dec_s("-2"), true), dec_s("2"));

        // Crossed/non-positive books fail closed.
        let crossed = BasisReplayFrame {
            now_ms: 2_500,
            primary: NormalizedLegBook {
                source_ms: 2_500,
                receipt_ms: 2_500,
                bid_px: dec_s("100.20"),
                bid_sz: dec_s("5"),
                ask_px: dec_s("100.10"),
                ask_sz: dec_s("5"),
                bids: vec![(dec_s("100.20"), dec_s("5"))],
                asks: vec![(dec_s("100.10"), dec_s("5"))],
            },
            hedge: NormalizedLegBook::from_l1(
                2_500,
                2_500,
                dec_s("100.00"),
                dec_s("5"),
                dec_s("100.10"),
                dec_s("5"),
            )
            .unwrap(),
            reference_oracle_px: Some(dec_s("100.05")),
            regime: SessionRegime::UsRth,
            primary_lease_ok: true,
            adl_shock: None,
            funding_clock: None,
            hedge_rtt_ms: None,
            realized_vol_bps_per_sqrt_s: None,
        };
        assert!(run_continuous_basis_replay(&cfg, &[crossed]).is_err());

        // Tape conversion respects caller-provided regime instead of fabricating UsRth.
        let perp_frames =
            load_perp_frames_from_path(Path::new("examples/crypto_funding_paper.json")).unwrap();
        let converted = basis_frames_from_perp_frames(
            &perp_frames,
            0,
            Decimal::ONE,
            Decimal::ONE,
            SessionRegime::OffHours,
        )
        .unwrap();
        assert!(
            converted
                .iter()
                .all(|f| f.regime == SessionRegime::OffHours)
        );
    }
}
