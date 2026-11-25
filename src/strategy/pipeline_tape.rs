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
    PipelineReplayConfig, PipelineReplayReport, ReplayTick, run_continuous_pipeline_replay,
};
use crate::strategy::quote_pipeline::{ExternalLeaderSignal, GenericMarketState};
use crate::strategy::relative_value::{Frame as PerpFrame, Oracle, PaperScenario};
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
    pub replay: PipelineReplayReport,
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

fn sample_std_dev(samples: &VecDeque<f64>) -> Option<f64> {
    if samples.len() < 2 {
        return None;
    }
    let n = samples.len() as f64;
    let mean = samples.iter().sum::<f64>() / n;
    let var = samples
        .iter()
        .map(|x| {
            let d = *x - mean;
            d * d
        })
        .sum::<f64>()
        / (n - 1.0);
    if var.is_finite() && var >= 0.0 {
        Some(var.sqrt())
    } else {
        None
    }
}

fn causal_oracle_px(now_ms: u64, oracle: Oracle) -> Option<f64> {
    if oracle.timestamp_ms == 0 || oracle.timestamp_ms > now_ms {
        return None;
    }
    oracle.price.to_f64().filter(|v| v.is_finite() && *v > 0.0)
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
    let mut vol_returns: VecDeque<f64> = VecDeque::with_capacity(cfg.vol_window_samples + 1);
    let mut prev_valid_target: Option<(u64, f64)> = None;
    let mut regime = VolRegime::Calm;
    let mut calm_ticks = 0_u64;
    let mut high_vol_ticks = 0_u64;
    let mut regime_transitions = 0_u64;
    let mut max_sigma = 0.0_f64;

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
            continue;
        }
        let Some(top_bid) = target_book.bids.first() else {
            skipped_frames += 1;
            continue;
        };
        let Some(top_ask) = target_book.asks.first() else {
            skipped_frames += 1;
            continue;
        };
        let Some(bid_px) = top_bid.price.to_f64().filter(|v| v.is_finite() && *v > 0.0) else {
            skipped_frames += 1;
            continue;
        };
        let Some(ask_px) = top_ask.price.to_f64().filter(|v| v.is_finite() && *v > 0.0) else {
            skipped_frames += 1;
            continue;
        };
        if ask_px <= bid_px {
            skipped_frames += 1;
            continue;
        }
        let Some(bid_sz) = top_bid
            .contracts
            .to_f64()
            .filter(|v| v.is_finite() && *v > 0.0)
            .map(|c| c * cfg.base_per_contract)
        else {
            skipped_frames += 1;
            continue;
        };
        let Some(ask_sz) = top_ask
            .contracts
            .to_f64()
            .filter(|v| v.is_finite() && *v > 0.0)
            .map(|c| c * cfg.base_per_contract)
        else {
            skipped_frames += 1;
            continue;
        };

        let mid = 0.5 * (bid_px + ask_px);
        let momentum_3s_bps =
            causal_return_bps(&mut target_mid_hist, now_ms, mid, cfg.momentum_window_ms);

        if let Some((prev_t, prev_m)) = prev_valid_target
            && now_ms > prev_t
            && prev_m > 0.0
        {
            let dt_s = ((now_ms - prev_t) as f64) / 1_000.0;
            let scaled_ret = ((mid / prev_m - 1.0) * 10_000.0) / dt_s.sqrt();
            if scaled_ret.is_finite() {
                if vol_returns.len() == cfg.vol_window_samples {
                    vol_returns.pop_front();
                }
                vol_returns.push_back(scaled_ret);
                if let Some(sigma) = sample_std_dev(&vol_returns) {
                    max_sigma = max_sigma.max(sigma);
                    let next_regime = match regime {
                        VolRegime::Calm if sigma >= cfg.high_vol_enter_sigma_bps => {
                            VolRegime::HighVol
                        }
                        VolRegime::HighVol if sigma <= cfg.high_vol_exit_sigma_bps => {
                            VolRegime::Calm
                        }
                        other => other,
                    };
                    if next_regime != regime {
                        regime = next_regime;
                        regime_transitions += 1;
                    }
                }
            }
        }
        prev_valid_target = Some((now_ms, mid));
        match regime {
            VolRegime::Calm => calm_ticks += 1,
            VolRegime::HighVol => high_vol_ticks += 1,
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
                funding_rate_bps_per_hour: None,
                exit_queue_ahead_base: None,
            },
            leader,
            taker_sweep: None,
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
        replay,
    })
}

/// Load `Frame`s from either a `PaperScenario` JSON file or a `perp-compile` NDJSON tape
/// (auto-skipping optional line-1 `pair` scenario header).
pub fn load_perp_frames_from_path(path: &Path) -> Result<Vec<PerpFrame>> {
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
        return Ok(scenario.frames);
    }

    let file =
        File::open(path).with_context(|| format!("open NDJSON tape '{}'", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut line_no = 0_usize;
    let mut frames = Vec::new();
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
        if frames.is_empty()
            && line_no == 1
            && let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed)
            && val.get("pair").is_some()
            && val.get("legs").is_none()
        {
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
    Ok(frames)
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
    let frames = load_perp_frames_from_path(tape_path)?;
    run_perp_frames_pipeline_replay(&cfg, &frames)
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
                    max_oracle_divergence_bps: 50.0,
                    base_order_qty: 0.5,
                },
                slot_diff: SlotDiffConfig {
                    reprice_deadband_ticks: 1,
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
}
