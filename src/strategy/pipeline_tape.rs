//! Venue-agnostic compiled-tape adapter driving [`run_continuous_pipeline_replay`]
//! from recorded/compiled `PerpFrame` NDJSON tapes or JSON scenarios.
//!
//! Contains ZERO dependencies on `crate::exchanges::*`: enforces strictly monotonic
//! causality across lookback momentum, leader return signals, oracle timestamps,
//! rolling volatility regime classification, and strictly warm-up-only
//! [`MarkovMicropriceTable`] calibration (no evaluation-window lookahead).

use crate::strategy::glft::VolRegime;
use crate::strategy::microprice::{
    MAX_PRODUCTION_MICROPRICE_RESIDUAL_TICKS, MarkovBookObservation, MarkovMicropriceConfig,
    MarkovMicropriceTable,
};
use crate::strategy::pipeline_driver::{
    PipelineReplayConfig, PipelineReplayReport, ReplayTakerSweep, ReplayTick,
    run_continuous_pipeline_replay,
};
use crate::strategy::quote_pipeline::{
    CausalRealizedVolEstimator, ExternalLeaderSignal, GenericMarketState,
};
use crate::strategy::relative_value::{Frame as PerpFrame, Level, Oracle, PaperScenario};
use anyhow::{Context, Result, ensure};
use rust_decimal::prelude::ToPrimitive;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::Path;

fn default_base_per_contract() -> f64 {
    1.0
}
fn default_momentum_window_ms() -> u64 {
    3_000
}
fn default_leader_horizon_ms() -> u64 {
    1_000
}
fn default_true() -> bool {
    true
}
fn default_high_vol_enter() -> f64 {
    2.5
}
fn default_high_vol_exit() -> f64 {
    1.8
}
fn default_vol_window_samples() -> usize {
    60
}
fn default_trade_fraction() -> f64 {
    0.35
}
fn default_dominance_ratio() -> f64 {
    1.5
}
fn default_max_frame_gap_ms() -> u64 {
    2_000
}

/// Opt-in causal taker sweep inference from consecutive L2 book snapshots. Default `enabled = false`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BookDiffSweepConfig {
    pub enabled: bool,
    /// Conservative fraction `(0, 1]` of unilateral depth depletion attributed to taker flow.
    pub trade_fraction: f64,
    /// Minimum one-sided dominance ratio (`>= 1.0`) required when both sides shrink slightly.
    pub dominance_ratio: f64,
    /// Maximum inter-frame interval (`ms > 0`) allowed for attributing a book diff to a sweep.
    pub max_frame_gap_ms: u64,
    /// Minimum scaled base sweep volume (`>= 0`) required to emit a `ReplayTakerSweep`.
    pub min_sweep_size_base: f64,
}

impl Default for BookDiffSweepConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            trade_fraction: default_trade_fraction(),
            dominance_ratio: default_dominance_ratio(),
            max_frame_gap_ms: default_max_frame_gap_ms(),
            min_sweep_size_base: 0.0,
        }
    }
}

impl BookDiffSweepConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.trade_fraction.is_finite()
                && self.trade_fraction > 0.0
                && self.trade_fraction <= 1.0,
            "book_diff_sweep.trade_fraction must be finite and in (0, 1]"
        );
        ensure!(
            self.dominance_ratio.is_finite() && self.dominance_ratio >= 1.0,
            "book_diff_sweep.dominance_ratio must be finite and >= 1.0"
        );
        ensure!(
            self.max_frame_gap_ms > 0,
            "book_diff_sweep.max_frame_gap_ms must be > 0"
        );
        ensure!(
            self.min_sweep_size_base.is_finite() && self.min_sweep_size_base >= 0.0,
            "book_diff_sweep.min_sweep_size_base must be finite and >= 0"
        );
        Ok(())
    }
}

/// Configuration for translating `PerpFrame` tapes into continuous `QuotePipeline` replays.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PipelineTapeConfig {
    /// Index (`0` or `1`) in `Frame.legs` of the quoted venue leg; the other leg acts as
    /// optional external leader reference when `replay.pipeline.lead_lag.enabled == true`.
    pub target_leg: usize,
    /// Minimum tick size in quote currency for target leg.
    pub tick_size: f64,
    #[serde(default = "default_base_per_contract")]
    pub base_per_contract: f64,
    #[serde(default = "default_momentum_window_ms")]
    pub momentum_window_ms: u64,
    #[serde(default = "default_leader_horizon_ms")]
    pub leader_return_horizon_ms: u64,
    #[serde(default = "default_true")]
    pub fit_microprice_on_warmup: bool,
    #[serde(default)]
    pub microprice: MarkovMicropriceConfig,
    #[serde(default = "default_high_vol_enter")]
    pub high_vol_enter_sigma_bps: f64,
    #[serde(default = "default_high_vol_exit")]
    pub high_vol_exit_sigma_bps: f64,
    #[serde(default = "default_vol_window_samples")]
    pub vol_window_samples: usize,
    /// Target leg funding settlement cadence in ms (required to convert per-period rate to bps/hour).
    /// When `None`, `run_pipeline_tape_file` auto-populates it from the tape header if present;
    /// otherwise funding rate extraction fails closed to `None`.
    #[serde(default)]
    pub funding_interval_ms: Option<u64>,
    #[serde(default)]
    pub book_diff_sweep: BookDiffSweepConfig,
    pub replay: PipelineReplayConfig,
}

impl PipelineTapeConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.target_leg <= 1,
            "target_leg must be 0 or 1 (got {})",
            self.target_leg
        );
        ensure!(
            self.tick_size.is_finite() && self.tick_size > 0.0,
            "tick_size must be finite and > 0"
        );
        ensure!(
            self.base_per_contract.is_finite() && self.base_per_contract > 0.0,
            "base_per_contract must be finite and > 0"
        );
        ensure!(
            self.momentum_window_ms > 0 && self.leader_return_horizon_ms > 0,
            "momentum_window_ms and leader_return_horizon_ms must be > 0"
        );
        ensure!(
            self.high_vol_exit_sigma_bps.is_finite()
                && self.high_vol_enter_sigma_bps.is_finite()
                && self.high_vol_exit_sigma_bps > 0.0
                && self.high_vol_enter_sigma_bps > self.high_vol_exit_sigma_bps,
            "require finite 0 < high_vol_exit_sigma_bps < high_vol_enter_sigma_bps"
        );
        ensure!(
            self.vol_window_samples >= 4,
            "vol_window_samples must be >= 4"
        );
        ensure!(
            self.funding_interval_ms.is_none_or(|ms| ms > 0),
            "funding_interval_ms must be > 0 when specified"
        );
        self.book_diff_sweep.validate()?;
        self.microprice.validate()?;
        self.replay.validate()?;
        Ok(())
    }
}

/// Output of causal `PerpFrame` -> `ReplayTick` translation.
#[derive(Debug, Clone)]
pub struct TranslatedTape {
    pub ticks: Vec<ReplayTick>,
    pub warmup_observations: Vec<MarkovBookObservation>,
    pub source_frames_read: u64,
    pub skipped_frames: u64,
    pub calm_ticks: u64,
    pub high_vol_ticks: u64,
    pub regime_transitions: u64,
    pub max_observed_sigma_bps_per_sqrt_s: f64,
    pub inferred_buy_sweeps: u64,
    pub inferred_sell_sweeps: u64,
}

/// Complete continuous tape replay report including warm-up calibration & volatility diagnostics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PipelineTapeReplayReport {
    pub source_frames_read: u64,
    pub translated_ticks: u64,
    pub skipped_frames: u64,
    pub microprice_fitted_from_warmup: bool,
    pub microprice_residual_inf_norm: Option<f64>,
    pub microprice_fallback_reason: Option<String>,
    pub calm_ticks: u64,
    pub high_vol_ticks: u64,
    pub regime_transitions: u64,
    pub max_observed_sigma_bps_per_sqrt_s: f64,
    #[serde(default)]
    pub inferred_buy_sweeps: u64,
    #[serde(default)]
    pub inferred_sell_sweeps: u64,
    pub replay: PipelineReplayReport,
}

/// Loaded `PerpFrame` tape plus optional leg funding settlement intervals from the header/scenario.
#[derive(Debug, Clone)]
pub struct LoadedPerpTape {
    pub funding_intervals_ms: Option<[u64; 2]>,
    pub frames: Vec<PerpFrame>,
}

fn causal_return_bps(
    history: &mut VecDeque<(u64, f64)>,
    now_ms: u64,
    mid_px: f64,
    horizon_ms: u64,
) -> f64 {
    let cutoff = now_ms.saturating_sub(horizon_ms);
    while history.len() > 1 && history.get(1).is_some_and(|(ts, _)| *ts <= cutoff) {
        history.pop_front();
    }
    let ret_bps = if let Some(&(_, base_mid)) = history.front()
        && base_mid > 0.0
    {
        ((mid_px - base_mid) / base_mid) * 10_000.0
    } else {
        0.0
    };
    history.push_back((now_ms, mid_px));
    if ret_bps.is_finite() { ret_bps } else { 0.0 }
}

fn causal_oracle_px(now_ms: u64, oracle: Oracle) -> Option<f64> {
    if oracle.timestamp_ms == 0 || oracle.timestamp_ms > now_ms {
        return None;
    }
    oracle.price.to_f64().filter(|v| v.is_finite() && *v > 0.0)
}

fn causal_funding_rate_bps_per_hour(
    now_ms: u64,
    forecast: Option<&crate::strategy::relative_value::FundingForecast>,
    funding_interval_ms: Option<u64>,
) -> Option<f64> {
    let interval_ms = funding_interval_ms.filter(|&ms| ms > 0)?;
    let fc = forecast?;
    if fc.observed_at_ms == 0 || fc.observed_at_ms > now_ms {
        return None;
    }
    let pt = fc
        .payments
        .iter()
        .filter(|pt| pt.at_ms >= now_ms)
        .min_by_key(|pt| pt.at_ms)?;
    let interval_hours = (interval_ms as f64) / 3_600_000.0;
    let bps_per_hour = (pt.rate.to_f64()? * 10_000.0) / interval_hours;
    (bps_per_hour.is_finite() && bps_per_hour.abs() <= 500.0).then_some(bps_per_hour)
}

#[derive(Debug, Clone)]
struct PrevTargetBook {
    now_ms: u64,
    bid_px: f64,
    bid_sz: f64,
    ask_px: f64,
    ask_sz: f64,
    bids: Vec<(f64, f64)>,
    asks: Vec<(f64, f64)>,
}

fn extract_levels(levels: &[Level], base_per_contract: f64) -> Vec<(f64, f64)> {
    let mut out = Vec::with_capacity(levels.len());
    for lvl in levels {
        if let (Some(px), Some(sz)) = (
            lvl.price.to_f64().filter(|v| v.is_finite() && *v > 0.0),
            lvl.contracts
                .to_f64()
                .filter(|v| v.is_finite() && *v > 0.0)
                .map(|c| c * base_per_contract),
        ) {
            out.push((px, sz));
        }
    }
    out
}

fn infer_book_diff_sweep(
    cfg: &BookDiffSweepConfig,
    prev: &PrevTargetBook,
    cur_now_ms: u64,
    cur_bid_px: f64,
    cur_bid_sz: f64,
    cur_ask_px: f64,
    cur_ask_sz: f64,
) -> Option<ReplayTakerSweep> {
    if !cfg.enabled {
        return None;
    }
    let dt = cur_now_ms.saturating_sub(prev.now_ms);
    if dt == 0 || dt > cfg.max_frame_gap_ms {
        return None;
    }

    const PX_TOL: f64 = 1e-9;
    let (ask_cleared, ask_touch) = if cur_bid_px >= prev.bid_px - PX_TOL {
        if cur_ask_px > prev.ask_px + PX_TOL {
            let cleared: f64 = prev
                .asks
                .iter()
                .filter(|&&(px, _)| px < cur_ask_px - PX_TOL)
                .map(|&(_, sz)| sz)
                .sum();
            let prev_at_cur = prev
                .asks
                .iter()
                .find(|&&(px, _)| (px - cur_ask_px).abs() <= PX_TOL)
                .map_or(0.0, |&(_, sz)| sz);
            (cleared, (prev_at_cur - cur_ask_sz).max(0.0))
        } else if (cur_ask_px - prev.ask_px).abs() <= PX_TOL {
            (0.0, (prev.ask_sz - cur_ask_sz).max(0.0))
        } else {
            (0.0, 0.0)
        }
    } else {
        (0.0, 0.0)
    };

    let (bid_cleared, bid_touch) = if cur_ask_px <= prev.ask_px + PX_TOL {
        if cur_bid_px < prev.bid_px - PX_TOL {
            let cleared: f64 = prev
                .bids
                .iter()
                .filter(|&&(px, _)| px > cur_bid_px + PX_TOL)
                .map(|&(_, sz)| sz)
                .sum();
            let prev_at_cur = prev
                .bids
                .iter()
                .find(|&&(px, _)| (px - cur_bid_px).abs() <= PX_TOL)
                .map_or(0.0, |&(_, sz)| sz);
            (cleared, (prev_at_cur - cur_bid_sz).max(0.0))
        } else if (cur_bid_px - prev.bid_px).abs() <= PX_TOL {
            (0.0, (prev.bid_sz - cur_bid_sz).max(0.0))
        } else {
            (0.0, 0.0)
        }
    } else {
        (0.0, 0.0)
    };

    let ask_tot = ask_cleared + ask_touch;
    let bid_tot = bid_cleared + bid_touch;
    let (taker_is_buy, cleared, touch, limit_px) = if ask_tot > 1e-12 && bid_tot <= 1e-12 {
        (true, ask_cleared, ask_touch, cur_ask_px)
    } else if bid_tot > 1e-12 && ask_tot <= 1e-12 {
        (false, bid_cleared, bid_touch, cur_bid_px)
    } else if ask_tot > 1e-12 && bid_tot > 1e-12 {
        if ask_tot >= cfg.dominance_ratio * bid_tot {
            (true, ask_cleared, ask_touch, cur_ask_px)
        } else if bid_tot >= cfg.dominance_ratio * ask_tot {
            (false, bid_cleared, bid_touch, cur_bid_px)
        } else {
            return None;
        }
    } else {
        return None;
    };

    let eff_cleared = cleared * cfg.trade_fraction;
    let eff_touch = touch * cfg.trade_fraction;
    let eff_total = eff_cleared + eff_touch;
    if !eff_total.is_finite() || eff_total <= 1e-12 || eff_total < cfg.min_sweep_size_base {
        return None;
    }
    let sweep_size_base = if eff_cleared > 1e-12 {
        eff_cleared
    } else {
        eff_touch
    };
    Some(ReplayTakerSweep {
        wallet_key: 0,
        taker_is_buy,
        is_twap_sub: false,
        sweep_size_base,
        sweep_limit_px: Some(limit_px),
        touch_depleted_base: Some(eff_touch),
    })
}

/// Causally translate compiled `PerpFrame` snapshots into `ReplayTick`s & warm-up microprice samples.
pub fn translate_perp_frames(
    cfg: &PipelineTapeConfig,
    frames: &[PerpFrame],
) -> Result<TranslatedTape> {
    cfg.validate()?;
    ensure!(!frames.is_empty(), "perp frames input must not be empty");

    let leader_leg = 1 - cfg.target_leg;
    let mut prev_ms = 0_u64;
    let mut ticks = Vec::with_capacity(frames.len());
    let mut warmup_observations = Vec::new();
    let mut skipped_frames = 0_u64;

    let mut target_mid_hist: VecDeque<(u64, f64)> = VecDeque::new();
    let mut leader_mid_hist: VecDeque<(u64, f64)> = VecDeque::new();
    let mut vol_est = CausalRealizedVolEstimator::new(cfg.vol_window_samples)?;
    let mut prev_target_book: Option<PrevTargetBook> = None;
    let mut regime = VolRegime::Calm;
    let mut calm_ticks = 0_u64;
    let mut high_vol_ticks = 0_u64;
    let mut regime_transitions = 0_u64;
    let mut max_sigma = 0.0_f64;
    let mut inferred_buy_sweeps = 0_u64;
    let mut inferred_sell_sweeps = 0_u64;

    for (idx, frame) in frames.iter().enumerate() {
        let now_ms = frame.now_ms;
        ensure!(now_ms > 0, "frame {idx}: now_ms must be > 0");
        if prev_ms > 0 {
            ensure!(
                now_ms > prev_ms,
                "frame {idx}: non-increasing timestamp {now_ms} <= {prev_ms}"
            );
        }
        prev_ms = now_ms;

        if now_ms < cfg.replay.window.warmup_start_ms || now_ms > cfg.replay.window.drain_end_ms {
            skipped_frames += 1;
            continue;
        }

        let target_book = &frame.legs[cfg.target_leg].book;
        if !target_book.tradable
            || target_book.timestamp_ms == 0
            || target_book.timestamp_ms > now_ms
        {
            skipped_frames += 1;
            prev_target_book = None;
            continue;
        }
        let Some(top_bid) = target_book.bids.first() else {
            skipped_frames += 1;
            prev_target_book = None;
            continue;
        };
        let Some(top_ask) = target_book.asks.first() else {
            skipped_frames += 1;
            prev_target_book = None;
            continue;
        };
        let Some(bid_px) = top_bid.price.to_f64().filter(|v| v.is_finite() && *v > 0.0) else {
            skipped_frames += 1;
            prev_target_book = None;
            continue;
        };
        let Some(ask_px) = top_ask.price.to_f64().filter(|v| v.is_finite() && *v > 0.0) else {
            skipped_frames += 1;
            prev_target_book = None;
            continue;
        };
        if ask_px <= bid_px {
            skipped_frames += 1;
            prev_target_book = None;
            continue;
        }
        let Some(bid_sz) = top_bid
            .contracts
            .to_f64()
            .filter(|v| v.is_finite() && *v > 0.0)
            .map(|c| c * cfg.base_per_contract)
        else {
            skipped_frames += 1;
            prev_target_book = None;
            continue;
        };
        let Some(ask_sz) = top_ask
            .contracts
            .to_f64()
            .filter(|v| v.is_finite() && *v > 0.0)
            .map(|c| c * cfg.base_per_contract)
        else {
            skipped_frames += 1;
            prev_target_book = None;
            continue;
        };

        let mid = 0.5 * (bid_px + ask_px);
        let momentum_3s_bps =
            causal_return_bps(&mut target_mid_hist, now_ms, mid, cfg.momentum_window_ms);

        let current_vol_bps = vol_est.observe(now_ms, mid);
        if let Some(sigma) = current_vol_bps {
            max_sigma = max_sigma.max(sigma);
            let next_regime = match regime {
                VolRegime::Calm if sigma >= cfg.high_vol_enter_sigma_bps => VolRegime::HighVol,
                VolRegime::HighVol if sigma <= cfg.high_vol_exit_sigma_bps => VolRegime::Calm,
                other => other,
            };
            if next_regime != regime {
                regime = next_regime;
                regime_transitions += 1;
            }
        }
        match regime {
            VolRegime::Calm => calm_ticks += 1,
            VolRegime::HighVol => high_vol_ticks += 1,
        }

        let taker_sweep = prev_target_book.as_ref().and_then(|prev| {
            infer_book_diff_sweep(
                &cfg.book_diff_sweep,
                prev,
                now_ms,
                bid_px,
                bid_sz,
                ask_px,
                ask_sz,
            )
        });
        if let Some(s) = taker_sweep {
            if s.taker_is_buy {
                inferred_buy_sweeps += 1;
            } else {
                inferred_sell_sweeps += 1;
            }
        }
        if cfg.book_diff_sweep.enabled {
            prev_target_book = Some(PrevTargetBook {
                now_ms,
                bid_px,
                bid_sz,
                ask_px,
                ask_sz,
                bids: extract_levels(&target_book.bids, cfg.base_per_contract),
                asks: extract_levels(&target_book.asks, cfg.base_per_contract),
            });
        }

        // Strict causal leader lookup (never leaks future prices or unacknowledged books).
        let leader_book = &frame.legs[leader_leg].book;
        let leader = if leader_book.tradable
            && leader_book.timestamp_ms > 0
            && leader_book.timestamp_ms <= now_ms
            && let (Some(lb), Some(la)) = (leader_book.bids.first(), leader_book.asks.first())
            && let (Some(l_bid), Some(l_ask)) = (
                lb.price.to_f64().filter(|v| v.is_finite() && *v > 0.0),
                la.price.to_f64().filter(|v| v.is_finite() && *v > 0.0),
            )
            && l_ask > l_bid
        {
            let l_mid = 0.5 * (l_bid + l_ask);
            let l_ret = causal_return_bps(
                &mut leader_mid_hist,
                now_ms,
                l_mid,
                cfg.leader_return_horizon_ms,
            );
            Some(ExternalLeaderSignal {
                leader_venue_id: u8::try_from(leader_leg).unwrap_or(1),
                reference_mid_px: l_mid,
                leader_return_bps: l_ret,
                source_ms: leader_book.timestamp_ms,
                receipt_ms: leader_book.timestamp_ms,
            })
        } else {
            None
        };

        let oracle_px = causal_oracle_px(now_ms, frame.legs[cfg.target_leg].oracle)
            .or_else(|| causal_oracle_px(now_ms, frame.reference));
        let funding_rate_bps_per_hour = causal_funding_rate_bps_per_hour(
            now_ms,
            frame.legs[cfg.target_leg].forecast.as_ref(),
            cfg.funding_interval_ms,
        );

        // Warm-up microprice fit observes strictly `now_ms < eval_start_ms` frames.
        if now_ms < cfg.replay.window.eval_start_ms {
            warmup_observations.push(MarkovBookObservation {
                timestamp_ms: now_ms,
                bid_px,
                ask_px,
                bid_sz,
                ask_sz,
                tick_size: cfg.tick_size,
            });
        }

        ticks.push(ReplayTick {
            market: GenericMarketState {
                now_ms,
                bid_px,
                ask_px,
                bid_sz,
                ask_sz,
                tick_size: cfg.tick_size,
                momentum_3s_bps,
                oracle_px,
                funding_rate_bps_per_hour,
                exit_queue_ahead_base: None,
                realized_vol_bps_per_sqrt_s: current_vol_bps,
            },
            leader,
            taker_sweep,
        });
    }

    Ok(TranslatedTape {
        ticks,
        warmup_observations,
        source_frames_read: frames.len() as u64,
        skipped_frames,
        calm_ticks,
        high_vol_ticks,
        regime_transitions,
        max_observed_sigma_bps_per_sqrt_s: max_sigma,
        inferred_buy_sweeps,
        inferred_sell_sweeps,
    })
}

/// Fit causal warm-up microprice (if enabled & converged) and run continuous replay over `frames`.
pub fn run_perp_frames_pipeline_replay(
    cfg: &PipelineTapeConfig,
    frames: &[PerpFrame],
) -> Result<PipelineTapeReplayReport> {
    let translated = translate_perp_frames(cfg, frames)?;
    ensure!(
        !translated.ticks.is_empty(),
        "no tradable target frames inside configured replay window"
    );

    let (microprice, microprice_fitted_from_warmup, residual, fallback_reason) =
        if cfg.fit_microprice_on_warmup {
            if translated.warmup_observations.len() < 4 {
                (
                    None,
                    false,
                    None,
                    Some("insufficient_warmup_observations".to_string()),
                )
            } else {
                match MarkovMicropriceTable::fit(&cfg.microprice, &translated.warmup_observations)
                    .and_then(|tbl| {
                        tbl.validate_for_production(MAX_PRODUCTION_MICROPRICE_RESIDUAL_TICKS)?;
                        Ok(tbl)
                    }) {
                    Ok(tbl) => {
                        let res = tbl.residual_inf_norm();
                        (Some(tbl), true, Some(res), None)
                    }
                    Err(err) => (None, false, None, Some(err.to_string())),
                }
            }
        } else {
            (None, false, None, Some("disabled_by_config".to_string()))
        };

    let replay = run_continuous_pipeline_replay(&cfg.replay, microprice, &translated.ticks)?;
    Ok(PipelineTapeReplayReport {
        source_frames_read: translated.source_frames_read,
        translated_ticks: translated.ticks.len() as u64,
        skipped_frames: translated.skipped_frames,
        microprice_fitted_from_warmup,
        microprice_residual_inf_norm: residual,
        microprice_fallback_reason: fallback_reason,
        calm_ticks: translated.calm_ticks,
        high_vol_ticks: translated.high_vol_ticks,
        regime_transitions: translated.regime_transitions,
        max_observed_sigma_bps_per_sqrt_s: translated.max_observed_sigma_bps_per_sqrt_s,
        inferred_buy_sweeps: translated.inferred_buy_sweeps,
        inferred_sell_sweeps: translated.inferred_sell_sweeps,
        replay,
    })
}

/// Load `Frame`s and optional leg funding settlement intervals from a JSON scenario, NDJSON tape, or stdin (`-`).
pub fn load_perp_tape_from_path(path: &Path) -> Result<LoadedPerpTape> {
    let mut reader: Box<dyn BufRead> = if path == Path::new("-") {
        Box::new(BufReader::new(std::io::stdin().lock()))
    } else {
        ensure!(
            path.is_file(),
            "tape file '{}' does not exist or is not a file",
            path.display()
        );
        if path.extension().and_then(|e| e.to_str()) == Some("json") {
            let raw = fs::read_to_string(path)
                .with_context(|| format!("read JSON tape '{}'", path.display()))?;
            let scenario: PaperScenario = serde_json::from_str(&raw)
                .with_context(|| format!("parse PaperScenario JSON '{}'", path.display()))?;
            ensure!(
                !scenario.frames.is_empty(),
                "JSON tape '{}' contains zero frames",
                path.display()
            );
            let i0 = scenario.pair.legs[0].funding.interval_ms;
            let i1 = scenario.pair.legs[1].funding.interval_ms;
            ensure!(i0 > 0 && i1 > 0, "pair leg funding interval_ms must be > 0");
            return Ok(LoadedPerpTape {
                funding_intervals_ms: Some([i0, i1]),
                frames: scenario.frames,
            });
        }
        let file =
            File::open(path).with_context(|| format!("open NDJSON tape '{}'", path.display()))?;
        Box::new(BufReader::new(file))
    };

    let mut line = String::new();
    let mut line_no = 0_usize;
    let mut frames = Vec::new();
    let mut funding_intervals_ms = None;
    let mut seen_non_empty = false;
    loop {
        line.clear();
        let bytes = reader
            .read_line(&mut line)
            .with_context(|| format!("read line {} of '{}'", line_no + 1, path.display()))?;
        if bytes == 0 {
            break;
        }
        line_no += 1;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let is_first_non_empty = !seen_non_empty;
        seen_non_empty = true;
        if is_first_non_empty
            && let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed)
            && val.get("pair").is_some()
            && val.get("legs").is_none()
        {
            if let Some(legs) = val
                .get("pair")
                .and_then(|p| p.get("legs"))
                .and_then(|l| l.as_array())
                && legs.len() == 2
                && let (Some(i0), Some(i1)) = (
                    legs[0]
                        .get("funding")
                        .and_then(|f| f.get("interval_ms"))
                        .and_then(|v| v.as_u64()),
                    legs[1]
                        .get("funding")
                        .and_then(|f| f.get("interval_ms"))
                        .and_then(|v| v.as_u64()),
                )
            {
                ensure!(i0 > 0 && i1 > 0, "pair leg funding interval_ms must be > 0");
                funding_intervals_ms = Some([i0, i1]);
            }
            continue;
        }
        let frame: PerpFrame = serde_json::from_str(trimmed).with_context(|| {
            format!("parse PerpFrame on line {line_no} of '{}'", path.display())
        })?;
        frames.push(frame);
    }
    ensure!(
        !frames.is_empty(),
        "NDJSON tape '{}' contains zero frames",
        path.display()
    );
    Ok(LoadedPerpTape {
        funding_intervals_ms,
        frames,
    })
}

/// Load `Frame`s from either a `PaperScenario` JSON file, a `perp-compile` NDJSON tape, or
/// stdin (`-`) streaming NDJSON lines (auto-skipping optional line-1 `pair` scenario header).
pub fn load_perp_frames_from_path(path: &Path) -> Result<Vec<PerpFrame>> {
    Ok(load_perp_tape_from_path(path)?.frames)
}

/// End-to-end offline runner for `cross_venue pipeline-replay <CONFIG> <TAPE>`.
pub fn run_pipeline_tape_file(
    config_path: &Path,
    tape_path: &Path,
) -> Result<PipelineTapeReplayReport> {
    ensure!(
        config_path.is_file(),
        "config file '{}' does not exist or is not a file",
        config_path.display()
    );
    let raw_cfg = fs::read_to_string(config_path)
        .with_context(|| format!("read config '{}'", config_path.display()))?;
    let cfg: PipelineTapeConfig = serde_json::from_str(&raw_cfg)
        .with_context(|| format!("parse PipelineTapeConfig '{}'", config_path.display()))?;
    cfg.validate()?;
    let tape = load_perp_tape_from_path(tape_path)?;
    let tape_interval = tape.funding_intervals_ms.map(|arr| arr[cfg.target_leg]);
    let mut effective_cfg = cfg;
    if let (Some(cfg_ms), Some(hdr_ms)) = (effective_cfg.funding_interval_ms, tape_interval) {
        ensure!(
            cfg_ms == hdr_ms,
            "PipelineTapeConfig funding_interval_ms ({cfg_ms}) mismatches tape header funding.interval_ms ({hdr_ms})"
        );
    } else if effective_cfg.funding_interval_ms.is_none() {
        effective_cfg.funding_interval_ms = tape_interval;
    }
    run_perp_frames_pipeline_replay(&effective_cfg, &tape.frames)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strategy::glft::GlftParams;
    use crate::strategy::hybrid_exit::HybridExitConfig;
    use crate::strategy::pipeline_driver::{ReplayWindowSpec, SlotDiffConfig};
    use crate::strategy::quote_pipeline::{LeadLagConfig, QuotePipelineConfig, StrategyMode};
    use crate::strategy::wallet_toxicity::WalletToxicityConfig;

    fn sample_tape_cfg(
        warmup_start_ms: u64,
        eval_start_ms: u64,
        eval_end_ms: u64,
        drain_end_ms: u64,
    ) -> PipelineTapeConfig {
        PipelineTapeConfig {
            target_leg: 0,
            tick_size: 0.1,
            base_per_contract: 1.0,
            momentum_window_ms: 3_000,
            leader_return_horizon_ms: 1_000,
            fit_microprice_on_warmup: true,
            microprice: MarkovMicropriceConfig {
                imbalance_buckets: 2,
                spread_buckets: 1,
                ..MarkovMicropriceConfig::default()
            },
            high_vol_enter_sigma_bps: 2.5,
            high_vol_exit_sigma_bps: 1.8,
            vol_window_samples: 8,
            funding_interval_ms: Some(3_600_000),
            book_diff_sweep: BookDiffSweepConfig::default(),
            replay: PipelineReplayConfig {
                pipeline: QuotePipelineConfig {
                    mode: StrategyMode::NeutralMarketMaking,
                    glft: GlftParams {
                        gamma: 0.08,
                        xi: 0.08,
                        delta_qty: 1.0,
                        intensity_a: 2.98,
                        decay_k: 0.85,
                        sigma_bps_per_sqrt_s: 1.335,
                        max_inventory_base: 5.0,
                        inventory_skew_scale: 1.0,
                        fee_floor_bps: 0.40,
                        max_half_spread_bps: 50.0,
                    },
                    wallet_toxicity: WalletToxicityConfig::default(),
                    hybrid_exit: HybridExitConfig::default(),
                    lead_lag: LeadLagConfig {
                        enabled: true,
                        ..LeadLagConfig::default()
                    },
                    markout_shield: crate::strategy::quote_pipeline::MarkoutShieldConfig::default(),
                    funding_bias: Default::default(),
                    inventory_clip: Default::default(),
                    vol_momentum_defense: Default::default(),
                    max_oracle_divergence_bps: 50.0,
                    base_order_qty: 0.5,
                },
                slot_diff: SlotDiffConfig {
                    reprice_deadband_ticks: 1,
                    reprice_deadband_bps: None,
                    min_size_change_frac: 0.1,
                    lot_step: 0.01,
                    min_order_qty: 0.01,
                },
                window: ReplayWindowSpec {
                    warmup_start_ms,
                    eval_start_ms,
                    eval_end_ms,
                    drain_end_ms,
                },
                initial_capital_usd: 1_000.0,
                daily_loss_limit_pct: 0.05,
                maker_fee_bps: 0.0,
                taker_fee_bps: 3.5,
                queue_fill_delay_ms: 100,
                passive_touch_fill_stride: 2,
                initial_queue_ahead_frac: 1.0,
                pre_registered_wallets: vec![],
            },
        }
    }

    #[test]
    fn pipeline_tape_loads_and_replays_crypto_funding_fixture_with_safe_fallback() {
        let frames =
            load_perp_frames_from_path(Path::new("examples/crypto_funding_paper.json")).unwrap();
        let first_ms = frames[0].now_ms;
        let last_ms = frames.last().unwrap().now_ms;
        let mid_ms = first_ms + (last_ms - first_ms) / 2;
        let cfg = sample_tape_cfg(first_ms, mid_ms, last_ms, last_ms);

        let report = run_perp_frames_pipeline_replay(&cfg, &frames).unwrap();
        assert_eq!(report.source_frames_read, frames.len() as u64);
        assert_eq!(report.translated_ticks, frames.len() as u64);
        assert!(!report.replay.is_full_10h_continuous);
        // Short fixture warmup falls back safely instead of loading unconverged microprice.
        assert!(!report.microprice_fitted_from_warmup);
        assert!(report.microprice_fallback_reason.is_some());
    }

    #[test]
    fn pipeline_tape_skips_nontradable_frames_and_rejects_regressed_clocks() {
        let mut frames =
            load_perp_frames_from_path(Path::new("examples/crypto_funding_paper.json")).unwrap();
        let first_ms = frames[0].now_ms;
        let last_ms = frames.last().unwrap().now_ms;
        let mid_ms = first_ms + (last_ms - first_ms) / 2;
        let cfg = sample_tape_cfg(first_ms, mid_ms, last_ms, last_ms);

        frames[1].legs[0].book.tradable = false;
        let report = run_perp_frames_pipeline_replay(&cfg, &frames).unwrap();
        assert_eq!(report.skipped_frames, 1);
        assert_eq!(report.translated_ticks, (frames.len() - 1) as u64);

        // Regressed timestamp must fail closed.
        frames[2].now_ms = frames[1].now_ms - 1;
        assert!(translate_perp_frames(&cfg, &frames).is_err());
    }

    #[test]
    fn pipeline_tape_extracts_causal_funding_rate_and_rejects_future_forecast() {
        let mut frames =
            load_perp_frames_from_path(Path::new("examples/crypto_funding_paper.json")).unwrap();
        let first_ms = frames[0].now_ms;
        let last_ms = frames.last().unwrap().now_ms;
        let mid_ms = first_ms + (last_ms - first_ms) / 2;
        let cfg = sample_tape_cfg(first_ms, mid_ms, last_ms, last_ms);

        // Inject future forecast timestamp on frame 0 (must ignore) and causal forecast on frame 1.
        let now0 = frames[0].now_ms;
        if let Some(fc0) = frames[0].legs[0].forecast.as_mut() {
            fc0.observed_at_ms = now0 + 1;
        }
        let now1 = frames[1].now_ms;
        if let Some(fc1) = frames[1].legs[0].forecast.as_mut() {
            fc1.observed_at_ms = now1;
            if let Some(mut next_pt) = fc1.payments.first().copied() {
                let past_pt = crate::strategy::relative_value::FundingPoint {
                    at_ms: now1 - 1,
                    rate: rust_decimal::Decimal::new(99, 5),
                    uncertainty_bps: next_pt.uncertainty_bps,
                    oracle_price: next_pt.oracle_price,
                };
                next_pt.at_ms = now1 + 60_000;
                next_pt.rate = rust_decimal::Decimal::new(15, 5); // 1.5 bps/h
                let later_pt = crate::strategy::relative_value::FundingPoint {
                    at_ms: now1 + 120_000,
                    rate: rust_decimal::Decimal::new(45, 5),
                    uncertainty_bps: next_pt.uncertainty_bps,
                    oracle_price: next_pt.oracle_price,
                };
                fc1.payments = vec![past_pt, later_pt, next_pt];
            }
        }
        let translated = translate_perp_frames(&cfg, &frames).unwrap();
        assert_eq!(translated.ticks[0].market.funding_rate_bps_per_hour, None);
        assert!(
            (translated.ticks[1]
                .market
                .funding_rate_bps_per_hour
                .unwrap()
                - 1.5)
                .abs()
                < 1e-9
        );

        // 8-hour funding cadence (`28_800_000 ms`, rate = 0.0008 -> 8 bps/period -> 1.0 bps/hour).
        let mut cfg_8h = cfg.clone();
        cfg_8h.funding_interval_ms = Some(28_800_000);
        if let Some(fc1) = frames[1].legs[0].forecast.as_mut() {
            fc1.payments[2].rate = rust_decimal::Decimal::new(8, 4); // 0.0008 = 8 bps per 8h
        }
        let translated_8h = translate_perp_frames(&cfg_8h, &frames).unwrap();
        assert!(
            (translated_8h.ticks[1]
                .market
                .funding_rate_bps_per_hour
                .unwrap()
                - 1.0)
                .abs()
                < 1e-9
        );

        // Unknown funding cadence fails closed to `None` instead of guessing 1h.
        let mut cfg_none = cfg;
        cfg_none.funding_interval_ms = None;
        let translated_none = translate_perp_frames(&cfg_none, &frames).unwrap();
        assert_eq!(
            translated_none.ticks[1].market.funding_rate_bps_per_hour,
            None
        );

        // Header mismatch in `run_pipeline_tape_file` must fail closed.
        let tmp_dir = std::env::temp_dir();
        let cfg_path = tmp_dir.join("alephtx_test_tape_cfg_mismatch.json");
        let mut bad_cfg = cfg_8h; // specifies 8h while examples/crypto_funding_paper.json has 1h
        bad_cfg.replay.window = ReplayWindowSpec {
            warmup_start_ms: first_ms,
            eval_start_ms: mid_ms,
            eval_end_ms: last_ms,
            drain_end_ms: last_ms,
        };
        std::fs::write(&cfg_path, serde_json::to_string(&bad_cfg).unwrap()).unwrap();
        let err =
            run_pipeline_tape_file(&cfg_path, Path::new("examples/crypto_funding_paper.json"))
                .unwrap_err()
                .to_string();
        let _ = std::fs::remove_file(&cfg_path);
        assert!(err.contains("mismatches tape header"));

        // Leading blank line before NDJSON `pair` header must still parse header intervals cleanly.
        let ndjson_path = tmp_dir.join("alephtx_test_tape_leading_blank.ndjson");
        let ndjson_body = format!(
            "\n{{\"pair\":{{\"legs\":[{{\"funding\":{{\"interval_ms\":3600000}}}},{{\"funding\":{{\"interval_ms\":28800000}}}}]}}}}\n{}\n",
            serde_json::to_string(&frames[0]).unwrap()
        );
        std::fs::write(&ndjson_path, ndjson_body).unwrap();
        let loaded = load_perp_tape_from_path(&ndjson_path).unwrap();
        let _ = std::fs::remove_file(&ndjson_path);
        assert_eq!(loaded.funding_intervals_ms, Some([3_600_000, 28_800_000]));
        assert_eq!(loaded.frames.len(), 1);
    }

    #[test]
    fn book_diff_sweep_infers_unilateral_sweeps_and_rejects_quote_pulls_and_gaps() {
        use crate::strategy::relative_value::{Book, Level};

        let frames_src =
            load_perp_frames_from_path(Path::new("examples/crypto_funding_paper.json")).unwrap();
        let template = frames_src[0].clone();
        let mk_frame = |now_ms: u64, bids: Vec<(&str, &str)>, asks: Vec<(&str, &str)>| {
            let mut f = template.clone();
            f.now_ms = now_ms;
            f.legs[0].book = Book {
                market: "ETH".into(),
                timestamp_ms: now_ms,
                tradable: true,
                bids: bids
                    .into_iter()
                    .map(|(p, s)| Level {
                        price: p.parse().unwrap(),
                        contracts: s.parse().unwrap(),
                    })
                    .collect(),
                asks: asks
                    .into_iter()
                    .map(|(p, s)| Level {
                        price: p.parse().unwrap(),
                        contracts: s.parse().unwrap(),
                    })
                    .collect(),
                bounds: None,
                available_notional: rust_decimal::Decimal::new(10_000, 0),
            };
            f
        };

        let frames = vec![
            // Frame 0: base book
            mk_frame(
                1_000,
                vec![("100.0", "10"), ("99.9", "10")],
                vec![("100.1", "5"), ("100.2", "5")],
            ),
            // Frame 1 (+100ms): buyer clears 100.1 (5) and consumes 2 at 100.2 -> cleared=5, touch=2, ask=100.2
            mk_frame(
                1_100,
                vec![("100.0", "10"), ("99.9", "10")],
                vec![("100.2", "3")],
            ),
            // Frame 2 (+100ms): seller depletes touch at 100.0 from 10 -> 4 while ask stays 100.2
            mk_frame(
                1_200,
                vec![("100.0", "4"), ("99.9", "10")],
                vec![("100.2", "3")],
            ),
            // Frame 3 (+100ms): symmetric quote pull (bid drops 100.0->99.9 AND ask jumps 100.2->100.3) -> None!
            mk_frame(1_300, vec![("99.9", "10")], vec![("100.3", "5")]),
            // Frame 4 (+100ms): ambiguous dual shrinkage at touch (bid 10->7 [-3], ask 5->2.5 [-2.5], ratio 1.2 < 1.5) -> None!
            mk_frame(1_400, vec![("99.9", "7")], vec![("100.3", "2.5")]),
            // Frame 5 (+5_000ms > max_frame_gap_ms): large gap -> None!
            mk_frame(6_400, vec![("99.9", "5")], vec![("100.4", "1")]),
        ];

        let mut cfg = sample_tape_cfg(1_000, 1_100, 6_400, 6_400);
        cfg.book_diff_sweep.enabled = false;
        let off = translate_perp_frames(&cfg, &frames).unwrap();
        assert_eq!(off.inferred_buy_sweeps, 0);
        assert_eq!(off.inferred_sell_sweeps, 0);
        assert!(off.ticks.iter().all(|t| t.taker_sweep.is_none()));

        cfg.book_diff_sweep.enabled = true;
        cfg.book_diff_sweep.trade_fraction = 0.5;
        cfg.book_diff_sweep.dominance_ratio = 1.5;
        cfg.book_diff_sweep.max_frame_gap_ms = 2_000;
        let on = translate_perp_frames(&cfg, &frames).unwrap();
        assert_eq!(on.inferred_buy_sweeps, 1);
        assert_eq!(on.inferred_sell_sweeps, 1);
        assert!(on.ticks[0].taker_sweep.is_none());

        let s1 = on.ticks[1].taker_sweep.unwrap();
        assert!(s1.taker_is_buy);
        assert_eq!(s1.wallet_key, 0);
        assert_eq!(s1.sweep_limit_px, Some(100.2));
        assert!((s1.sweep_size_base - 2.5).abs() < 1e-9);
        assert!((s1.touch_depleted_base.unwrap() - 1.0).abs() < 1e-9);

        let s2 = on.ticks[2].taker_sweep.unwrap();
        assert!(!s2.taker_is_buy);
        assert_eq!(s2.sweep_limit_px, Some(100.0));
        assert!((s2.sweep_size_base - 3.0).abs() < 1e-9);
        assert!((s2.touch_depleted_base.unwrap() - 3.0).abs() < 1e-9);

        assert!(on.ticks[3].taker_sweep.is_none());
        assert!(on.ticks[4].taker_sweep.is_none());
        assert!(on.ticks[5].taker_sweep.is_none());
    }
}
