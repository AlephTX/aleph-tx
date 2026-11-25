//! Deterministic cross-venue tail-risk supervisor (`ARB-IMP-10`).
//!
//! Evaluates per-venue margin utilization, liquidation buffer distance,
//! cross-venue equity concentration, ADL priority queue risk, and multi-source
//! stablecoin (`USDC`/`USDG`/`USDT`) peg integrity with fail-closed input
//! validation and strictly monotonic action escalation.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

const BPS_SCALE: f64 = 10_000.0;

/// Monotonic supervisory action emitted by [`evaluate_tail_risk`].
///
/// Ordering invariant: `Normal < ReduceOnly < HaltAndFlatten`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum TailRiskAction {
    Normal,
    ReduceOnly,
    HaltAndFlatten,
}

/// Venue-reported auto-deleveraging (ADL) priority queue severity.
///
/// Ordering invariant: `Low < Elevated < High < Critical`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum AdlRiskLevel {
    Low,
    Elevated,
    High,
    Critical,
}

/// Thresholds and oracle freshness bounds for [`evaluate_tail_risk`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TailRiskConfig {
    pub reduce_only_margin_util: f64,
    pub halt_margin_util: f64,
    pub min_liq_buffer_bps: f64,
    pub halt_liq_buffer_bps: f64,
    pub max_venue_equity_imbalance_ratio: f64,
    pub depeg_reduce_only_bps: f64,
    pub depeg_halt_bps: f64,
    pub max_peg_age_ms: u64,
    pub max_future_skew_ms: u64,
}

impl Default for TailRiskConfig {
    fn default() -> Self {
        Self {
            reduce_only_margin_util: 0.70,
            halt_margin_util: 0.85,
            min_liq_buffer_bps: 300.0,
            halt_liq_buffer_bps: 120.0,
            max_venue_equity_imbalance_ratio: 0.75,
            depeg_reduce_only_bps: 30.0,
            depeg_halt_bps: 75.0,
            max_peg_age_ms: 60_000,
            max_future_skew_ms: 5_000,
        }
    }
}

impl TailRiskConfig {
    /// Fail closed on non-finite thresholds or inconsistent ordering/domain bounds.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.reduce_only_margin_util.is_finite() && self.reduce_only_margin_util > 0.0,
            "reduce_only_margin_util must be finite and > 0.0"
        );
        ensure!(
            self.halt_margin_util.is_finite()
                && self.halt_margin_util > self.reduce_only_margin_util
                && self.halt_margin_util <= 1.0,
            "halt_margin_util must be finite, > reduce_only_margin_util, and <= 1.0"
        );
        ensure!(
            self.halt_liq_buffer_bps.is_finite() && self.halt_liq_buffer_bps > 0.0,
            "halt_liq_buffer_bps must be finite and > 0.0"
        );
        ensure!(
            self.min_liq_buffer_bps.is_finite()
                && self.min_liq_buffer_bps > self.halt_liq_buffer_bps,
            "min_liq_buffer_bps must be finite and > halt_liq_buffer_bps"
        );
        ensure!(
            self.max_venue_equity_imbalance_ratio.is_finite()
                && self.max_venue_equity_imbalance_ratio > 0.50
                && self.max_venue_equity_imbalance_ratio < 1.0,
            "max_venue_equity_imbalance_ratio must be finite and in (0.50, 1.0)"
        );
        ensure!(
            self.depeg_reduce_only_bps.is_finite() && self.depeg_reduce_only_bps > 0.0,
            "depeg_reduce_only_bps must be finite and > 0.0"
        );
        ensure!(
            self.depeg_halt_bps.is_finite()
                && self.depeg_halt_bps > self.depeg_reduce_only_bps
                && self.depeg_halt_bps < BPS_SCALE,
            "depeg_halt_bps must be finite, > depeg_reduce_only_bps, and < 10000.0"
        );
        ensure!(self.max_peg_age_ms > 0, "max_peg_age_ms must be > 0");
        Ok(())
    }
}

/// Point-in-time margin and clearing-domain risk snapshot for one venue.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VenueMarginSnapshot {
    pub venue: String,
    pub collateral_asset: String,
    pub equity_usd: f64,
    pub margin_used_usd: f64,
    pub liquidation_distance_bps: Option<f64>,
    pub adl_level: AdlRiskLevel,
}

/// Timestamped external stablecoin peg price observation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StablecoinPegSample {
    pub asset: String,
    pub price_usd: f64,
    pub available_at_ms: u64,
}

/// Deterministic supervisory verdict and summary metrics across all venues and collateral pegs.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TailRiskAssessment {
    pub action: TailRiskAction,
    pub reasons: Vec<String>,
    pub max_margin_util: f64,
    pub min_liq_buffer_bps: Option<f64>,
    pub equity_imbalance_ratio: f64,
    pub max_depeg_bps: f64,
    pub worst_adl_level: AdlRiskLevel,
    pub distinct_collateral_assets: Vec<String>,
}

#[derive(Debug)]
struct AssessmentState {
    action: TailRiskAction,
    reasons: BTreeSet<String>,
    max_margin_util: f64,
    min_liq_buffer_bps: Option<f64>,
    worst_adl_level: AdlRiskLevel,
    collateral_assets: BTreeSet<String>,
    all_positive_equity: bool,
}

impl Default for AssessmentState {
    fn default() -> Self {
        Self {
            action: TailRiskAction::Normal,
            reasons: BTreeSet::new(),
            max_margin_util: 0.0,
            min_liq_buffer_bps: None,
            worst_adl_level: AdlRiskLevel::Low,
            collateral_assets: BTreeSet::new(),
            all_positive_equity: true,
        }
    }
}

impl AssessmentState {
    fn escalate(&mut self, action: TailRiskAction, reason: String) {
        self.action = self.action.max(action);
        self.reasons.insert(reason);
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct AssetPegSummary {
    has_any_sample: bool,
    fresh_samples: usize,
    has_stale_or_skewed: bool,
    worst_fresh_depeg_bps: f64,
}

fn normalize_asset_symbol(raw: &str) -> String {
    raw.trim().to_ascii_uppercase()
}

fn normalize_zero(value: f64) -> f64 {
    if value == 0.0 { 0.0 } else { value }
}

fn compute_depeg_bps(price_usd: f64) -> f64 {
    let bps = (price_usd - 1.0).abs() * BPS_SCALE;
    if bps.is_finite() {
        normalize_zero(bps)
    } else {
        f64::MAX
    }
}

fn assess_venues(
    venues: &[VenueMarginSnapshot],
    cfg: &TailRiskConfig,
    state: &mut AssessmentState,
) -> Result<()> {
    ensure!(!venues.is_empty(), "venues must not be empty");

    for (idx, snapshot) in venues.iter().enumerate() {
        let venue = snapshot.venue.trim();
        ensure!(!venue.is_empty(), "venues[{idx}].venue must not be empty");

        let collateral = normalize_asset_symbol(&snapshot.collateral_asset);
        ensure!(
            !collateral.is_empty(),
            "venues[{idx}].collateral_asset must not be empty"
        );
        state.collateral_assets.insert(collateral);

        ensure!(
            snapshot.equity_usd.is_finite(),
            "venues[{idx}].equity_usd must be finite"
        );
        ensure!(
            snapshot.margin_used_usd.is_finite() && snapshot.margin_used_usd >= 0.0,
            "venues[{idx}].margin_used_usd must be finite and >= 0.0"
        );

        if let Some(liq_bps) = snapshot.liquidation_distance_bps {
            ensure!(
                liq_bps.is_finite() && liq_bps >= 0.0,
                "venues[{idx}].liquidation_distance_bps must be finite and >= 0.0"
            );
            let liq_bps = normalize_zero(liq_bps);
            state.min_liq_buffer_bps = Some(
                state
                    .min_liq_buffer_bps
                    .map_or(liq_bps, |cur| cur.min(liq_bps)),
            );
            if liq_bps <= cfg.halt_liq_buffer_bps {
                state.escalate(
                    TailRiskAction::HaltAndFlatten,
                    format!("liq_buffer_halt:{venue}"),
                );
            } else if liq_bps <= cfg.min_liq_buffer_bps {
                state.escalate(
                    TailRiskAction::ReduceOnly,
                    format!("liq_buffer_reduce_only:{venue}"),
                );
            }
        }

        if snapshot.equity_usd <= 0.0 {
            state.all_positive_equity = false;
            let util = if snapshot.margin_used_usd == 0.0 {
                0.0
            } else {
                1.0
            };
            state.max_margin_util = state.max_margin_util.max(util);
            state.escalate(
                TailRiskAction::HaltAndFlatten,
                format!("venue_non_positive_equity:{venue}"),
            );
        } else {
            let raw_util = snapshot.margin_used_usd / snapshot.equity_usd;
            let util = normalize_zero(if raw_util.is_finite() { raw_util } else { 1.0 });
            state.max_margin_util = state.max_margin_util.max(util);
            if !raw_util.is_finite() || util >= cfg.halt_margin_util {
                state.escalate(
                    TailRiskAction::HaltAndFlatten,
                    format!("margin_util_halt:{venue}"),
                );
            } else if util >= cfg.reduce_only_margin_util {
                state.escalate(
                    TailRiskAction::ReduceOnly,
                    format!("margin_util_reduce_only:{venue}"),
                );
            }
        }

        state.worst_adl_level = state.worst_adl_level.max(snapshot.adl_level);
        match snapshot.adl_level {
            AdlRiskLevel::Critical => state.escalate(
                TailRiskAction::HaltAndFlatten,
                format!("adl_critical:{venue}"),
            ),
            AdlRiskLevel::High => {
                state.escalate(TailRiskAction::ReduceOnly, format!("adl_high:{venue}"))
            }
            AdlRiskLevel::Low | AdlRiskLevel::Elevated => {}
        }
    }

    Ok(())
}

fn assess_equity_imbalance(
    venues: &[VenueMarginSnapshot],
    cfg: &TailRiskConfig,
    state: &mut AssessmentState,
) -> f64 {
    if venues.len() < 2 || !state.all_positive_equity {
        return 0.0;
    }

    let max_equity = venues.iter().map(|v| v.equity_usd).fold(0.0_f64, f64::max);
    let sum_equity: f64 = venues.iter().map(|v| v.equity_usd).sum();
    let ratio = if sum_equity.is_finite() && sum_equity > 0.0 {
        (max_equity / sum_equity).clamp(0.0, 1.0)
    } else {
        let sum_scaled: f64 = venues.iter().map(|v| v.equity_usd / max_equity).sum();
        (1.0 / sum_scaled).clamp(0.0, 1.0)
    };

    if ratio >= cfg.max_venue_equity_imbalance_ratio {
        state.escalate(
            TailRiskAction::ReduceOnly,
            "cross_venue_equity_imbalance".to_string(),
        );
    }

    normalize_zero(ratio)
}

fn assess_stablecoin_pegs(
    now_ms: u64,
    pegs: &[StablecoinPegSample],
    cfg: &TailRiskConfig,
    state: &mut AssessmentState,
) -> Result<f64> {
    let future_limit_ms = now_ms.saturating_add(cfg.max_future_skew_ms);
    let mut peg_summaries: BTreeMap<String, AssetPegSummary> = BTreeMap::new();

    for (idx, sample) in pegs.iter().enumerate() {
        let asset = normalize_asset_symbol(&sample.asset);
        ensure!(!asset.is_empty(), "pegs[{idx}].asset must not be empty");
        ensure!(
            sample.price_usd.is_finite() && sample.price_usd > 0.0,
            "pegs[{idx}].price_usd must be finite and > 0.0"
        );

        let entry = peg_summaries.entry(asset).or_default();
        entry.has_any_sample = true;

        let is_future_skewed = sample.available_at_ms > future_limit_ms;
        let is_stale = now_ms.saturating_sub(sample.available_at_ms) > cfg.max_peg_age_ms;
        if is_future_skewed || is_stale {
            entry.has_stale_or_skewed = true;
        } else {
            entry.fresh_samples += 1;
            let depeg_bps = compute_depeg_bps(sample.price_usd);
            entry.worst_fresh_depeg_bps = entry.worst_fresh_depeg_bps.max(depeg_bps);
        }
    }

    let mixed_collateral = state.collateral_assets.len() > 1;
    let active_assets: Vec<String> = state.collateral_assets.iter().cloned().collect();
    for asset in active_assets {
        let summary = peg_summaries.get(&asset).copied().unwrap_or_default();
        if (summary.has_any_sample || mixed_collateral) && summary.fresh_samples == 0 {
            state.escalate(
                TailRiskAction::ReduceOnly,
                format!("missing_peg_coverage:{asset}"),
            );
        }
    }

    let mut max_depeg_bps = 0.0_f64;
    for (asset, summary) in peg_summaries {
        if summary.has_stale_or_skewed {
            state.escalate(
                TailRiskAction::ReduceOnly,
                format!("stale_or_skewed_peg:{asset}"),
            );
        }
        if summary.fresh_samples > 0 {
            max_depeg_bps = max_depeg_bps.max(summary.worst_fresh_depeg_bps);
            if summary.worst_fresh_depeg_bps >= cfg.depeg_halt_bps {
                state.escalate(
                    TailRiskAction::HaltAndFlatten,
                    format!("stablecoin_depeg_halt:{asset}"),
                );
            } else if summary.worst_fresh_depeg_bps >= cfg.depeg_reduce_only_bps {
                state.escalate(
                    TailRiskAction::ReduceOnly,
                    format!("stablecoin_depeg_reduce_only:{asset}"),
                );
            }
        }
    }

    Ok(normalize_zero(max_depeg_bps))
}

/// Evaluate cross-venue tail risk deterministically and fail closed on invalid inputs.
///
/// Escalation across all checks is strictly monotonic (`Normal < ReduceOnly < HaltAndFlatten`)
/// and emits canonically sorted, credential-free machine-readable reason codes.
pub fn evaluate_tail_risk(
    now_ms: u64,
    venues: &[VenueMarginSnapshot],
    pegs: &[StablecoinPegSample],
    cfg: &TailRiskConfig,
) -> Result<TailRiskAssessment> {
    cfg.validate()?;

    let mut state = AssessmentState::default();
    assess_venues(venues, cfg, &mut state)?;
    let equity_imbalance_ratio = assess_equity_imbalance(venues, cfg, &mut state);
    let max_depeg_bps = assess_stablecoin_pegs(now_ms, pegs, cfg, &mut state)?;

    Ok(TailRiskAssessment {
        action: state.action,
        reasons: state.reasons.into_iter().collect(),
        max_margin_util: state.max_margin_util,
        min_liq_buffer_bps: state.min_liq_buffer_bps,
        equity_imbalance_ratio,
        max_depeg_bps,
        worst_adl_level: state.worst_adl_level,
        distinct_collateral_assets: state.collateral_assets.into_iter().collect(),
    })
}

/// Configuration for the 100 ms ADL / unplanned net-delta circuit breaker (`S2-2B`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AdlDeltaBreakerConfig {
    /// Maximum tolerated unplanned net delta fraction of leg notional/base (`default 0.01` = 1%).
    pub max_unplanned_delta_ratio: f64,
    /// Base-unit dust tolerance below which imbalance is ignored (`default 1e-6`).
    pub dust_base: f64,
    /// Target recovery dispatch SLA in milliseconds (`default 100 ms`).
    pub response_sla_ms: u64,
    /// Hard SLA ceiling beyond which unresolved delta escalates to `HaltAndFlatten` (`default 200 ms`).
    pub halt_sla_ms: u64,
}

impl Default for AdlDeltaBreakerConfig {
    fn default() -> Self {
        Self {
            max_unplanned_delta_ratio: 0.01,
            dust_base: 1e-6,
            response_sla_ms: 100,
            halt_sla_ms: 200,
        }
    }
}

impl AdlDeltaBreakerConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.max_unplanned_delta_ratio.is_finite()
                && self.max_unplanned_delta_ratio > 0.0
                && self.max_unplanned_delta_ratio <= 1.0,
            "max_unplanned_delta_ratio must be finite and in (0, 1]"
        );
        ensure!(
            self.dust_base.is_finite() && self.dust_base >= 0.0,
            "dust_base must be finite and >= 0"
        );
        ensure!(
            self.response_sla_ms > 0 && self.halt_sla_ms >= self.response_sla_ms,
            "require 0 < response_sla_ms <= halt_sla_ms"
        );
        Ok(())
    }
}

/// Paired leg position snapshot evaluated by [`evaluate_adl_delta_breaker`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct NetDeltaSnapshot {
    pub now_ms: u64,
    pub unplanned_since_ms: Option<u64>,
    pub target_leg_base: f64,
    pub hedge_leg_base: f64,
    pub reference_gross_base: f64,
}

/// Identifies which paired leg should execute a reduce-only recovery trim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PairedLeg {
    Primary,
    Hedge,
}

/// Deterministic ADL / unplanned net-delta breaker decision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AdlDeltaAction {
    Balanced {
        net_delta_base: f64,
        delta_ratio: f64,
    },
    EmergencyReduceLeg {
        leg: PairedLeg,
        is_buy: bool,
        qty_base: f64,
        reduce_only: bool,
        elapsed_ms: u64,
        within_response_sla: bool,
    },
    HaltAndFlatten {
        net_delta_base: f64,
        elapsed_ms: u64,
        reason: String,
    },
}

/// Evaluate unplanned delta across paired venues (such as a profitable leg being shrunk
/// by venue Auto-Deleveraging) and force sub-100ms recovery or 200ms halt escalation.
///
/// Always selects a surviving leg whose sign matches `net_delta_base` and clamps `qty_base`
/// to `|leg_pos|`, ensuring the recovery order can safely execute with `reduce_only = true`.
pub fn evaluate_adl_delta_breaker(
    cfg: &AdlDeltaBreakerConfig,
    snap: &NetDeltaSnapshot,
) -> Result<AdlDeltaAction> {
    cfg.validate()?;
    ensure!(snap.now_ms > 0, "now_ms must be > 0");
    ensure!(
        snap.target_leg_base.is_finite()
            && snap.hedge_leg_base.is_finite()
            && snap.reference_gross_base.is_finite()
            && snap.reference_gross_base >= 0.0,
        "position inputs must be finite and reference_gross_base >= 0"
    );
    if let Some(since_ms) = snap.unplanned_since_ms {
        ensure!(
            since_ms > 0 && since_ms <= snap.now_ms,
            "invalid or future unplanned_since_ms ({since_ms} vs {})",
            snap.now_ms
        );
    }

    let net_delta_base = snap.target_leg_base + snap.hedge_leg_base;
    let abs_net = net_delta_base.abs();
    let scale = snap
        .reference_gross_base
        .max(snap.target_leg_base.abs())
        .max(snap.hedge_leg_base.abs());

    if abs_net <= cfg.dust_base || scale <= cfg.dust_base {
        return Ok(AdlDeltaAction::Balanced {
            net_delta_base,
            delta_ratio: 0.0,
        });
    }

    let delta_ratio = abs_net / scale;
    if delta_ratio <= cfg.max_unplanned_delta_ratio {
        return Ok(AdlDeltaAction::Balanced {
            net_delta_base,
            delta_ratio,
        });
    }

    let elapsed_ms = snap
        .unplanned_since_ms
        .map_or(0, |since| snap.now_ms.saturating_sub(since));
    if elapsed_ms > cfg.halt_sla_ms {
        return Ok(AdlDeltaAction::HaltAndFlatten {
            net_delta_base,
            elapsed_ms,
            reason: format!(
                "adl_delta_sla_breached: elapsed={}ms > {}ms (ratio={:.4})",
                elapsed_ms, cfg.halt_sla_ms, delta_ratio
            ),
        });
    }

    let same_sign_as_net =
        |pos: f64| (net_delta_base > 0.0 && pos > 0.0) || (net_delta_base < 0.0 && pos < 0.0);
    let primary_eligible = same_sign_as_net(snap.target_leg_base);
    let hedge_eligible = same_sign_as_net(snap.hedge_leg_base);
    let (leg, leg_pos) = match (primary_eligible, hedge_eligible) {
        (true, true) => {
            if snap.target_leg_base.abs() >= snap.hedge_leg_base.abs() {
                (PairedLeg::Primary, snap.target_leg_base)
            } else {
                (PairedLeg::Hedge, snap.hedge_leg_base)
            }
        }
        (true, false) => (PairedLeg::Primary, snap.target_leg_base),
        (false, true) => (PairedLeg::Hedge, snap.hedge_leg_base),
        (false, false) => {
            return Ok(AdlDeltaAction::HaltAndFlatten {
                net_delta_base,
                elapsed_ms,
                reason: "no_reduce_only_leg_available".into(),
            });
        }
    };
    let qty_base = abs_net.min(leg_pos.abs());
    let is_buy = leg_pos < 0.0;

    Ok(AdlDeltaAction::EmergencyReduceLeg {
        leg,
        is_buy,
        qty_base,
        reduce_only: true,
        elapsed_ms,
        within_response_sla: elapsed_ms <= cfg.response_sla_ms,
    })
}

/// Thresholds for [`evaluate_collateral_rebalance`] live free-collateral & in-flight transfer checks.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CollateralRebalanceConfig {
    /// Minimum free collateral buffer ratio `effective_free / (required_margin + max(effective_free, 0))`
    /// before escalating to `ReduceOnly`.
    pub min_free_buffer_ratio: f64,
    /// Critical free buffer ratio at or below which [`evaluate_collateral_rebalance`] escalates to `HaltAndFlatten`.
    pub halt_free_buffer_ratio: f64,
    /// Maximum tolerated age (`ms`) of an unsettled in-flight transfer before escalating to `ReduceOnly`.
    pub max_in_flight_age_ms: u64,
}

impl Default for CollateralRebalanceConfig {
    fn default() -> Self {
        Self {
            min_free_buffer_ratio: 0.25,
            halt_free_buffer_ratio: 0.10,
            max_in_flight_age_ms: 300_000,
        }
    }
}

impl CollateralRebalanceConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.halt_free_buffer_ratio.is_finite() && self.halt_free_buffer_ratio > 0.0,
            "halt_free_buffer_ratio must be finite and > 0.0"
        );
        ensure!(
            self.min_free_buffer_ratio.is_finite()
                && self.min_free_buffer_ratio > self.halt_free_buffer_ratio
                && self.min_free_buffer_ratio < 1.0,
            "min_free_buffer_ratio must be finite, > halt_free_buffer_ratio, and < 1.0"
        );
        ensure!(
            self.max_in_flight_age_ms > 0,
            "max_in_flight_age_ms must be > 0"
        );
        Ok(())
    }
}

/// Per-venue live collateral & in-flight transfer snapshot for [`evaluate_collateral_rebalance`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CollateralRebalanceSnapshot {
    pub venue: String,
    pub free_collateral_usd: f64,
    pub required_margin_usd: f64,
    /// Pending inbound bridge/transfer USD (NEVER credited to effective free collateral before settlement).
    pub in_flight_inbound_usd: f64,
    /// Pending outbound bridge/transfer USD (deducted immediately from free collateral).
    pub in_flight_outbound_usd: f64,
    /// Elapsed age (`ms`) of the oldest unsettled transfer on this venue, if any transfer is in flight.
    pub in_flight_age_ms: Option<u64>,
}

/// Pessimistic collateral & in-flight transfer verdict across active venues.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CollateralRebalanceAssessment {
    pub action: TailRiskAction,
    pub reasons: Vec<String>,
    pub min_effective_free_buffer_ratio: Option<f64>,
    pub total_in_flight_inbound_usd: f64,
    pub total_in_flight_outbound_usd: f64,
}

/// Evaluate live available collateral across venues using pessimistic transfer accounting:
/// `in_flight_outbound_usd` is deducted immediately while `in_flight_inbound_usd` is NOT credited
/// until confirmed settled, and aged/undated in-flight transfers escalate to `ReduceOnly`.
pub fn evaluate_collateral_rebalance(
    cfg: &CollateralRebalanceConfig,
    snapshots: &[CollateralRebalanceSnapshot],
) -> Result<CollateralRebalanceAssessment> {
    cfg.validate()?;
    ensure!(
        !snapshots.is_empty(),
        "collateral snapshots must not be empty"
    );

    let mut action = TailRiskAction::Normal;
    let mut reasons = BTreeSet::new();
    let mut min_ratio: Option<f64> = None;
    let mut total_in = 0.0_f64;
    let mut total_out = 0.0_f64;
    let mut seen_venues = BTreeSet::new();

    for (idx, snap) in snapshots.iter().enumerate() {
        let venue = snap.venue.trim();
        ensure!(
            !venue.is_empty(),
            "snapshots[{idx}].venue must not be empty"
        );
        ensure!(
            seen_venues.insert(venue.to_ascii_lowercase()),
            "duplicate collateral venue '{venue}'"
        );
        ensure!(
            snap.free_collateral_usd.is_finite()
                && snap.free_collateral_usd >= 0.0
                && snap.required_margin_usd.is_finite()
                && snap.required_margin_usd >= 0.0
                && snap.in_flight_inbound_usd.is_finite()
                && snap.in_flight_inbound_usd >= 0.0
                && snap.in_flight_outbound_usd.is_finite()
                && snap.in_flight_outbound_usd >= 0.0,
            "snapshots[{idx}] collateral values must be finite and >= 0"
        );

        total_in += snap.in_flight_inbound_usd;
        total_out += snap.in_flight_outbound_usd;

        let effective_free = snap.free_collateral_usd - snap.in_flight_outbound_usd;
        if effective_free < 0.0 {
            action = action.max(TailRiskAction::HaltAndFlatten);
            reasons.insert(format!("negative_effective_free_collateral:{venue}"));
        }

        if snap.required_margin_usd > 0.0 {
            let denom = snap.required_margin_usd + effective_free.max(0.0);
            let ratio = if effective_free <= 0.0 {
                0.0
            } else {
                normalize_zero((effective_free / denom).clamp(0.0, 1.0))
            };
            min_ratio = Some(min_ratio.map_or(ratio, |cur| cur.min(ratio)));
            if ratio <= cfg.halt_free_buffer_ratio {
                action = action.max(TailRiskAction::HaltAndFlatten);
                reasons.insert(format!("free_collateral_halt:{venue}"));
            } else if ratio <= cfg.min_free_buffer_ratio {
                action = action.max(TailRiskAction::ReduceOnly);
                reasons.insert(format!("free_collateral_reduce_only:{venue}"));
            }
        }

        if snap.in_flight_inbound_usd > 0.0 || snap.in_flight_outbound_usd > 0.0 {
            match snap.in_flight_age_ms {
                None => {
                    action = action.max(TailRiskAction::ReduceOnly);
                    reasons.insert(format!("in_flight_transfer_age_missing:{venue}"));
                }
                Some(age_ms) if age_ms > cfg.max_in_flight_age_ms => {
                    action = action.max(TailRiskAction::ReduceOnly);
                    reasons.insert(format!("in_flight_transfer_stale:{venue}"));
                }
                Some(_) => {}
            }
        }
    }

    Ok(CollateralRebalanceAssessment {
        action,
        reasons: reasons.into_iter().collect(),
        min_effective_free_buffer_ratio: min_ratio,
        total_in_flight_inbound_usd: normalize_zero(total_in),
        total_in_flight_outbound_usd: normalize_zero(total_out),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW_MS: u64 = 1_700_000_000_000;

    fn venue(
        name: &str,
        collateral: &str,
        equity_usd: f64,
        margin_used_usd: f64,
        liq_bps: Option<f64>,
        adl_level: AdlRiskLevel,
    ) -> VenueMarginSnapshot {
        VenueMarginSnapshot {
            venue: name.to_string(),
            collateral_asset: collateral.to_string(),
            equity_usd,
            margin_used_usd,
            liquidation_distance_bps: liq_bps,
            adl_level,
        }
    }

    fn peg(asset: &str, price_usd: f64, available_at_ms: u64) -> StablecoinPegSample {
        StablecoinPegSample {
            asset: asset.to_string(),
            price_usd,
            available_at_ms,
        }
    }

    fn approx_eq(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn healthy_multi_venue_balanced_account_is_normal() {
        let cfg = TailRiskConfig::default();
        let venues = vec![
            venue(
                "lighter",
                "USDC",
                450.0,
                90.0,
                Some(520.0),
                AdlRiskLevel::Low,
            ),
            venue(
                "hyperliquid_core",
                "usdc",
                550.0,
                165.0,
                Some(450.0),
                AdlRiskLevel::Elevated,
            ),
        ];
        let pegs = vec![peg("USDC", 1.0002, NOW_MS - 1_000)];

        let out = evaluate_tail_risk(NOW_MS, &venues, &pegs, &cfg).expect("healthy evaluation");
        assert_eq!(out.action, TailRiskAction::Normal);
        assert!(out.reasons.is_empty());
        assert!(approx_eq(out.max_margin_util, 0.30));
        assert!(approx_eq(
            out.min_liq_buffer_bps.expect("liq buffer present"),
            450.0
        ));
        assert!(approx_eq(out.equity_imbalance_ratio, 0.55));
        assert!(approx_eq(out.max_depeg_bps, 2.0));
        assert_eq!(out.worst_adl_level, AdlRiskLevel::Elevated);
        assert_eq!(out.distinct_collateral_assets, vec!["USDC".to_string()]);
    }

    #[test]
    fn single_venue_single_collateral_without_pegs_is_normal() {
        let cfg = TailRiskConfig::default();
        let venues = vec![venue(
            "lighter",
            "USDC",
            1_000.0,
            250.0,
            None,
            AdlRiskLevel::Low,
        )];

        let out = evaluate_tail_risk(NOW_MS, &venues, &[], &cfg).expect("single venue normal");
        assert_eq!(out.action, TailRiskAction::Normal);
        assert!(out.reasons.is_empty());
        assert!(approx_eq(out.max_margin_util, 0.25));
        assert_eq!(out.min_liq_buffer_bps, None);
        assert!(approx_eq(out.equity_imbalance_ratio, 0.0));
        assert!(approx_eq(out.max_depeg_bps, 0.0));
    }

    #[test]
    fn margin_utilization_triggers_reduce_only_and_halt() {
        let cfg = TailRiskConfig::default();

        let reduce_venues = vec![
            venue(
                "lighter",
                "USDC",
                500.0,
                360.0,
                Some(600.0),
                AdlRiskLevel::Low,
            ),
            venue(
                "hyperliquid_core",
                "USDC",
                500.0,
                100.0,
                Some(600.0),
                AdlRiskLevel::Low,
            ),
        ];
        let reduce = evaluate_tail_risk(NOW_MS, &reduce_venues, &[], &cfg).expect("reduce_only");
        assert_eq!(reduce.action, TailRiskAction::ReduceOnly);
        assert!(approx_eq(reduce.max_margin_util, 0.72));
        assert_eq!(reduce.reasons, vec!["margin_util_reduce_only:lighter"]);

        let halt_venues = vec![
            venue(
                "lighter",
                "USDC",
                500.0,
                440.0,
                Some(600.0),
                AdlRiskLevel::Low,
            ),
            venue(
                "hyperliquid_core",
                "USDC",
                500.0,
                100.0,
                Some(600.0),
                AdlRiskLevel::Low,
            ),
        ];
        let halt = evaluate_tail_risk(NOW_MS, &halt_venues, &[], &cfg).expect("halt");
        assert_eq!(halt.action, TailRiskAction::HaltAndFlatten);
        assert!(approx_eq(halt.max_margin_util, 0.88));
        assert_eq!(halt.reasons, vec!["margin_util_halt:lighter"]);
    }

    #[test]
    fn liquidation_buffer_triggers_reduce_only_and_halt() {
        let cfg = TailRiskConfig::default();

        let reduce_venues = vec![
            venue(
                "arcus",
                "USDC",
                500.0,
                100.0,
                Some(250.0),
                AdlRiskLevel::Low,
            ),
            venue(
                "hyperliquid_core",
                "USDC",
                500.0,
                100.0,
                Some(500.0),
                AdlRiskLevel::Low,
            ),
        ];
        let reduce =
            evaluate_tail_risk(NOW_MS, &reduce_venues, &[], &cfg).expect("liq reduce_only");
        assert_eq!(reduce.action, TailRiskAction::ReduceOnly);
        assert!(approx_eq(
            reduce.min_liq_buffer_bps.expect("liq buffer"),
            250.0
        ));
        assert_eq!(reduce.reasons, vec!["liq_buffer_reduce_only:arcus"]);

        let halt_venues = vec![
            venue(
                "arcus",
                "USDC",
                500.0,
                100.0,
                Some(100.0),
                AdlRiskLevel::Low,
            ),
            venue(
                "hyperliquid_core",
                "USDC",
                500.0,
                100.0,
                Some(500.0),
                AdlRiskLevel::Low,
            ),
        ];
        let halt = evaluate_tail_risk(NOW_MS, &halt_venues, &[], &cfg).expect("liq halt");
        assert_eq!(halt.action, TailRiskAction::HaltAndFlatten);
        assert!(approx_eq(
            halt.min_liq_buffer_bps.expect("liq buffer"),
            100.0
        ));
        assert_eq!(halt.reasons, vec!["liq_buffer_halt:arcus"]);
    }

    #[test]
    fn exact_threshold_boundaries_trigger_conservatively() {
        let cfg = TailRiskConfig::default();

        // Exact reduce-only thresholds + exact freshness edges stay within their expected tiers.
        let boundary_reduce = evaluate_tail_risk(
            NOW_MS,
            &[
                venue(
                    "lighter",
                    "USDC",
                    1_000.0,
                    700.0,
                    Some(300.0),
                    AdlRiskLevel::Low,
                ),
                venue(
                    "hyperliquid_core",
                    "USDC",
                    1_000.0,
                    200.0,
                    Some(500.0),
                    AdlRiskLevel::Low,
                ),
            ],
            &[
                peg("USDC", 1.0, NOW_MS - cfg.max_peg_age_ms),
                peg("USDT", 1.0, NOW_MS + cfg.max_future_skew_ms),
            ],
            &cfg,
        )
        .expect("exact boundary reduce_only");
        assert_eq!(boundary_reduce.action, TailRiskAction::ReduceOnly);
        assert_eq!(
            boundary_reduce.reasons,
            vec![
                "liq_buffer_reduce_only:lighter",
                "margin_util_reduce_only:lighter",
            ]
        );

        // Exact imbalance 0.75 and exact halt thresholds (margin util 0.85, liq buffer 120 bps).
        let boundary_halt = evaluate_tail_risk(
            NOW_MS,
            &[
                venue(
                    "lighter",
                    "USDC",
                    750.0,
                    637.5,
                    Some(120.0),
                    AdlRiskLevel::Low,
                ),
                venue(
                    "hyperliquid_core",
                    "USDC",
                    250.0,
                    50.0,
                    Some(500.0),
                    AdlRiskLevel::Low,
                ),
            ],
            &[],
            &cfg,
        )
        .expect("exact boundary halt");
        assert_eq!(boundary_halt.action, TailRiskAction::HaltAndFlatten);
        assert!(approx_eq(boundary_halt.equity_imbalance_ratio, 0.75));
        assert_eq!(
            boundary_halt.reasons,
            vec![
                "cross_venue_equity_imbalance",
                "liq_buffer_halt:lighter",
                "margin_util_halt:lighter",
            ]
        );
    }

    #[test]
    fn cross_venue_equity_imbalance_triggers_reduce_only() {
        let cfg = TailRiskConfig::default();
        let venues = vec![
            venue(
                "hyperliquid_core",
                "USDC",
                850.0,
                100.0,
                Some(600.0),
                AdlRiskLevel::Low,
            ),
            venue(
                "lighter",
                "USDC",
                150.0,
                20.0,
                Some(600.0),
                AdlRiskLevel::Low,
            ),
        ];

        let out = evaluate_tail_risk(NOW_MS, &venues, &[], &cfg).expect("imbalance reduce_only");
        assert_eq!(out.action, TailRiskAction::ReduceOnly);
        assert!(approx_eq(out.equity_imbalance_ratio, 0.85));
        assert_eq!(out.reasons, vec!["cross_venue_equity_imbalance"]);
    }

    #[test]
    fn adl_high_and_critical_escalate_appropriately() {
        let cfg = TailRiskConfig::default();

        let high_venues = vec![venue(
            "lighter",
            "USDC",
            500.0,
            100.0,
            Some(500.0),
            AdlRiskLevel::High,
        )];
        let high = evaluate_tail_risk(NOW_MS, &high_venues, &[], &cfg).expect("adl high");
        assert_eq!(high.action, TailRiskAction::ReduceOnly);
        assert_eq!(high.worst_adl_level, AdlRiskLevel::High);
        assert_eq!(high.reasons, vec!["adl_high:lighter"]);

        let crit_venues = vec![venue(
            "lighter",
            "USDC",
            500.0,
            100.0,
            Some(500.0),
            AdlRiskLevel::Critical,
        )];
        let crit = evaluate_tail_risk(NOW_MS, &crit_venues, &[], &cfg).expect("adl critical");
        assert_eq!(crit.action, TailRiskAction::HaltAndFlatten);
        assert_eq!(crit.worst_adl_level, AdlRiskLevel::Critical);
        assert_eq!(crit.reasons, vec!["adl_critical:lighter"]);
    }

    #[test]
    fn mixed_collateral_and_multi_source_stablecoin_depeg_checks() {
        let cfg = TailRiskConfig::default();
        let mixed_venues = vec![
            venue(
                "hyperliquid_core",
                "usdc",
                500.0,
                100.0,
                Some(500.0),
                AdlRiskLevel::Low,
            ),
            venue(
                "hyperliquid_rh",
                "Usdg",
                500.0,
                100.0,
                Some(500.0),
                AdlRiskLevel::Low,
            ),
        ];

        // Mixed USDC + USDG without peg coverage -> ReduceOnly for both domains.
        let uncovered = evaluate_tail_risk(NOW_MS, &mixed_venues, &[], &cfg).expect("missing pegs");
        assert_eq!(uncovered.action, TailRiskAction::ReduceOnly);
        assert_eq!(
            uncovered.distinct_collateral_assets,
            vec!["USDC".to_string(), "USDG".to_string()]
        );
        assert_eq!(
            uncovered.reasons,
            vec!["missing_peg_coverage:USDC", "missing_peg_coverage:USDG"]
        );

        // Fresh USDC + 35 bp USDG depeg -> ReduceOnly.
        let depeg_35 = evaluate_tail_risk(
            NOW_MS,
            &mixed_venues,
            &[
                peg("USDC", 1.0000, NOW_MS - 500),
                peg("USDG", 0.9965, NOW_MS - 500),
            ],
            &cfg,
        )
        .expect("35 bp depeg");
        assert_eq!(depeg_35.action, TailRiskAction::ReduceOnly);
        assert!(approx_eq(depeg_35.max_depeg_bps, 35.0));
        assert_eq!(depeg_35.reasons, vec!["stablecoin_depeg_reduce_only:USDG"]);

        // Multi-source USDG oracle: one healthy source + one 80 bp depeg source -> HaltAndFlatten.
        let depeg_80 = evaluate_tail_risk(
            NOW_MS,
            &mixed_venues,
            &[
                peg("USDC", 1.0001, NOW_MS - 500),
                peg("USDG", 0.9999, NOW_MS - 500),
                peg("USDG", 0.9920, NOW_MS - 400),
                peg("usdt", 1.0000, NOW_MS - 400),
            ],
            &cfg,
        )
        .expect("80 bp multi-source depeg");
        assert_eq!(depeg_80.action, TailRiskAction::HaltAndFlatten);
        assert!(approx_eq(depeg_80.max_depeg_bps, 80.0));
        assert_eq!(depeg_80.reasons, vec!["stablecoin_depeg_halt:USDG"]);

        // Stale peg sample -> ReduceOnly (and missing_peg_coverage when no fresh sample exists).
        let stale = evaluate_tail_risk(
            NOW_MS,
            &mixed_venues,
            &[
                peg("USDC", 1.0000, NOW_MS - 500),
                peg("USDG", 1.0000, NOW_MS - cfg.max_peg_age_ms - 1),
            ],
            &cfg,
        )
        .expect("stale peg");
        assert_eq!(stale.action, TailRiskAction::ReduceOnly);
        assert!(approx_eq(stale.max_depeg_bps, 0.0));
        assert_eq!(
            stale.reasons,
            vec!["missing_peg_coverage:USDG", "stale_or_skewed_peg:USDG"]
        );

        // Future-skewed peg sample combined with a fresh 80 bp depeg never downgrades HaltAndFlatten.
        let skewed_plus_halt = evaluate_tail_risk(
            NOW_MS,
            &mixed_venues,
            &[
                peg("USDC", 1.0000, NOW_MS - 500),
                peg("USDG", 1.0000, NOW_MS + cfg.max_future_skew_ms + 1),
                peg("USDG", 1.0080, NOW_MS - 500),
            ],
            &cfg,
        )
        .expect("skewed plus halt");
        assert_eq!(skewed_plus_halt.action, TailRiskAction::HaltAndFlatten);
        assert!(approx_eq(skewed_plus_halt.max_depeg_bps, 80.0));
        assert_eq!(
            skewed_plus_halt.reasons,
            vec!["stablecoin_depeg_halt:USDG", "stale_or_skewed_peg:USDG"]
        );
    }

    #[test]
    fn non_positive_equity_halts_with_finite_margin_util() {
        let cfg = TailRiskConfig::default();

        let zero_eq_with_margin = vec![
            venue("lighter", "USDC", 0.0, 50.0, Some(0.0), AdlRiskLevel::Low),
            venue(
                "hyperliquid_core",
                "USDC",
                500.0,
                100.0,
                Some(500.0),
                AdlRiskLevel::Low,
            ),
        ];
        let out_nonzero_margin =
            evaluate_tail_risk(NOW_MS, &zero_eq_with_margin, &[], &cfg).expect("zero equity");
        assert_eq!(out_nonzero_margin.action, TailRiskAction::HaltAndFlatten);
        assert!(out_nonzero_margin.max_margin_util.is_finite());
        assert!(approx_eq(out_nonzero_margin.max_margin_util, 1.0));
        assert!(approx_eq(out_nonzero_margin.equity_imbalance_ratio, 0.0));
        assert_eq!(
            out_nonzero_margin.reasons,
            vec![
                "liq_buffer_halt:lighter",
                "venue_non_positive_equity:lighter",
            ]
        );

        let neg_eq_zero_margin = vec![venue("arcus", "USDC", -25.0, 0.0, None, AdlRiskLevel::Low)];
        let out_zero_margin =
            evaluate_tail_risk(NOW_MS, &neg_eq_zero_margin, &[], &cfg).expect("negative equity");
        assert_eq!(out_zero_margin.action, TailRiskAction::HaltAndFlatten);
        assert!(out_zero_margin.max_margin_util.is_finite());
        assert!(approx_eq(out_zero_margin.max_margin_util, 0.0));
        assert_eq!(
            out_zero_margin.reasons,
            vec!["venue_non_positive_equity:arcus"]
        );
    }

    #[test]
    fn fail_closed_on_invalid_inputs_and_config_thresholds() {
        let cfg = TailRiskConfig::default();
        let valid_venues = vec![venue(
            "lighter",
            "USDC",
            500.0,
            100.0,
            Some(500.0),
            AdlRiskLevel::Low,
        )];

        // Empty venues & blank venue/asset identifiers.
        assert!(evaluate_tail_risk(NOW_MS, &[], &[], &cfg).is_err());
        assert!(
            evaluate_tail_risk(
                NOW_MS,
                &[venue("  ", "USDC", 500.0, 100.0, None, AdlRiskLevel::Low)],
                &[],
                &cfg,
            )
            .is_err()
        );
        assert!(
            evaluate_tail_risk(
                NOW_MS,
                &[venue("lighter", " ", 500.0, 100.0, None, AdlRiskLevel::Low)],
                &[],
                &cfg,
            )
            .is_err()
        );

        // Non-finite / negative venue numbers.
        assert!(
            evaluate_tail_risk(
                NOW_MS,
                &[venue(
                    "lighter",
                    "USDC",
                    f64::NAN,
                    100.0,
                    None,
                    AdlRiskLevel::Low
                )],
                &[],
                &cfg,
            )
            .is_err()
        );
        assert!(
            evaluate_tail_risk(
                NOW_MS,
                &[venue(
                    "lighter",
                    "USDC",
                    f64::INFINITY,
                    100.0,
                    None,
                    AdlRiskLevel::Low
                )],
                &[],
                &cfg,
            )
            .is_err()
        );
        assert!(
            evaluate_tail_risk(
                NOW_MS,
                &[venue(
                    "lighter",
                    "USDC",
                    500.0,
                    -1.0,
                    None,
                    AdlRiskLevel::Low
                )],
                &[],
                &cfg,
            )
            .is_err()
        );
        assert!(
            evaluate_tail_risk(
                NOW_MS,
                &[venue(
                    "lighter",
                    "USDC",
                    500.0,
                    f64::INFINITY,
                    None,
                    AdlRiskLevel::Low
                )],
                &[],
                &cfg,
            )
            .is_err()
        );
        assert!(
            evaluate_tail_risk(
                NOW_MS,
                &[venue(
                    "lighter",
                    "USDC",
                    500.0,
                    100.0,
                    Some(-0.1),
                    AdlRiskLevel::Low
                )],
                &[],
                &cfg,
            )
            .is_err()
        );
        assert!(
            evaluate_tail_risk(
                NOW_MS,
                &[venue(
                    "lighter",
                    "USDC",
                    500.0,
                    100.0,
                    Some(f64::NAN),
                    AdlRiskLevel::Low
                )],
                &[],
                &cfg,
            )
            .is_err()
        );

        // Invalid peg samples.
        assert!(evaluate_tail_risk(NOW_MS, &valid_venues, &[peg(" ", 1.0, NOW_MS)], &cfg).is_err());
        assert!(
            evaluate_tail_risk(NOW_MS, &valid_venues, &[peg("USDC", 0.0, NOW_MS)], &cfg).is_err()
        );
        assert!(
            evaluate_tail_risk(NOW_MS, &valid_venues, &[peg("USDC", -1.0, NOW_MS)], &cfg).is_err()
        );
        assert!(
            evaluate_tail_risk(
                NOW_MS,
                &valid_venues,
                &[peg("USDC", f64::NAN, NOW_MS)],
                &cfg
            )
            .is_err()
        );
        assert!(
            evaluate_tail_risk(
                NOW_MS,
                &valid_venues,
                &[peg("USDC", f64::INFINITY, NOW_MS)],
                &cfg
            )
            .is_err()
        );

        // Invalid config bounds.
        let invalid_configs = [
            TailRiskConfig {
                reduce_only_margin_util: 0.0,
                ..cfg.clone()
            },
            TailRiskConfig {
                reduce_only_margin_util: 0.85,
                halt_margin_util: 0.85,
                ..cfg.clone()
            },
            TailRiskConfig {
                halt_margin_util: 1.01,
                ..cfg.clone()
            },
            TailRiskConfig {
                halt_margin_util: f64::NAN,
                ..cfg.clone()
            },
            TailRiskConfig {
                halt_liq_buffer_bps: 0.0,
                ..cfg.clone()
            },
            TailRiskConfig {
                min_liq_buffer_bps: f64::INFINITY,
                ..cfg.clone()
            },
            TailRiskConfig {
                halt_liq_buffer_bps: 300.0,
                min_liq_buffer_bps: 300.0,
                ..cfg.clone()
            },
            TailRiskConfig {
                max_venue_equity_imbalance_ratio: 0.50,
                ..cfg.clone()
            },
            TailRiskConfig {
                max_venue_equity_imbalance_ratio: 1.0,
                ..cfg.clone()
            },
            TailRiskConfig {
                depeg_reduce_only_bps: 0.0,
                ..cfg.clone()
            },
            TailRiskConfig {
                depeg_reduce_only_bps: 75.0,
                depeg_halt_bps: 75.0,
                ..cfg.clone()
            },
            TailRiskConfig {
                depeg_halt_bps: 10_000.0,
                ..cfg.clone()
            },
            TailRiskConfig {
                max_peg_age_ms: 0,
                ..cfg
            },
        ];

        for bad_cfg in &invalid_configs {
            assert!(bad_cfg.validate().is_err(), "expected invalid {bad_cfg:?}");
            assert!(evaluate_tail_risk(NOW_MS, &valid_venues, &[], bad_cfg).is_err());
        }
    }

    #[test]
    fn canonical_reason_ordering_and_serde_rejects_unknown_fields() {
        let cfg = TailRiskConfig::default();
        let venues_a = vec![
            venue(
                "lighter",
                "USDG",
                850.0,
                650.0,
                Some(200.0),
                AdlRiskLevel::High,
            ),
            venue(
                "arcus",
                "USDC",
                150.0,
                135.0,
                Some(80.0),
                AdlRiskLevel::Critical,
            ),
        ];
        let mut venues_b = venues_a.clone();
        venues_b.reverse();

        let pegs = vec![peg("USDC", 1.0, NOW_MS), peg("USDG", 0.9960, NOW_MS)];

        let out_a = evaluate_tail_risk(NOW_MS, &venues_a, &pegs, &cfg).expect("out_a");
        let out_b = evaluate_tail_risk(NOW_MS, &venues_b, &pegs, &cfg).expect("out_b");
        assert_eq!(out_a, out_b);
        assert_eq!(out_a.action, TailRiskAction::HaltAndFlatten);

        let mut sorted_reasons = out_a.reasons.clone();
        sorted_reasons.sort();
        sorted_reasons.dedup();
        assert_eq!(out_a.reasons, sorted_reasons);

        let parsed: TailRiskConfig = serde_json::from_str("{}").expect("default serde");
        assert_eq!(parsed, cfg);

        let bad_json = r#"{"reduce_only_margin_util":0.7,"unknown_field":1}"#;
        assert!(serde_json::from_str::<TailRiskConfig>(bad_json).is_err());
    }

    #[test]
    fn adl_delta_breaker_triggers_sub_100ms_reduce_only_and_escalates_past_200ms() {
        let cfg = AdlDeltaBreakerConfig::default();

        let balanced = evaluate_adl_delta_breaker(
            &cfg,
            &NetDeltaSnapshot {
                now_ms: NOW_MS,
                unplanned_since_ms: None,
                target_leg_base: 10.0,
                hedge_leg_base: -9.95,
                reference_gross_base: 10.0,
            },
        )
        .unwrap();
        assert!(matches!(balanced, AdlDeltaAction::Balanced { .. }));

        // Short hedge leg is ADL-trimmed by venue (-10 -> -6), leaving +4.0 net long.
        // Recovery MUST sell 4.0 on the surviving long Primary leg (`reduce_only = true`).
        let hedge_trimmed = evaluate_adl_delta_breaker(
            &cfg,
            &NetDeltaSnapshot {
                now_ms: NOW_MS,
                unplanned_since_ms: Some(NOW_MS - 45),
                target_leg_base: 10.0,
                hedge_leg_base: -6.0,
                reference_gross_base: 10.0,
            },
        )
        .unwrap();
        assert_eq!(
            hedge_trimmed,
            AdlDeltaAction::EmergencyReduceLeg {
                leg: PairedLeg::Primary,
                is_buy: false,
                qty_base: 4.0,
                reduce_only: true,
                elapsed_ms: 45,
                within_response_sla: true,
            }
        );

        // Primary long leg is ADL-trimmed (+10 -> +7), leaving -3.0 net short.
        // Recovery MUST buy 3.0 on the surviving short Hedge leg (`reduce_only = true`).
        let primary_trimmed = evaluate_adl_delta_breaker(
            &cfg,
            &NetDeltaSnapshot {
                now_ms: NOW_MS,
                unplanned_since_ms: Some(NOW_MS - 30),
                target_leg_base: 7.0,
                hedge_leg_base: -10.0,
                reference_gross_base: 10.0,
            },
        )
        .unwrap();
        assert_eq!(
            primary_trimmed,
            AdlDeltaAction::EmergencyReduceLeg {
                leg: PairedLeg::Hedge,
                is_buy: true,
                qty_base: 3.0,
                reduce_only: true,
                elapsed_ms: 30,
                within_response_sla: true,
            }
        );

        // Same-sign drift (+3, +2 => +5 net): qty_base clamps safely to surviving leg's |pos| (3.0).
        let same_sign = evaluate_adl_delta_breaker(
            &cfg,
            &NetDeltaSnapshot {
                now_ms: NOW_MS,
                unplanned_since_ms: Some(NOW_MS - 10),
                target_leg_base: 3.0,
                hedge_leg_base: 2.0,
                reference_gross_base: 5.0,
            },
        )
        .unwrap();
        assert_eq!(
            same_sign,
            AdlDeltaAction::EmergencyReduceLeg {
                leg: PairedLeg::Primary,
                is_buy: false,
                qty_base: 3.0,
                reduce_only: true,
                elapsed_ms: 10,
                within_response_sla: true,
            }
        );

        // Unresolved past 200 ms halt SLA escalates to HaltAndFlatten.
        let halt = evaluate_adl_delta_breaker(
            &cfg,
            &NetDeltaSnapshot {
                now_ms: NOW_MS,
                unplanned_since_ms: Some(NOW_MS - 250),
                target_leg_base: 10.0,
                hedge_leg_base: -6.0,
                reference_gross_base: 10.0,
            },
        )
        .unwrap();
        assert!(matches!(
            halt,
            AdlDeltaAction::HaltAndFlatten {
                elapsed_ms: 250,
                ..
            }
        ));

        // Future unplanned_since_ms fails closed.
        assert!(
            evaluate_adl_delta_breaker(
                &cfg,
                &NetDeltaSnapshot {
                    now_ms: NOW_MS,
                    unplanned_since_ms: Some(NOW_MS + 1),
                    target_leg_base: 10.0,
                    hedge_leg_base: -6.0,
                    reference_gross_base: 10.0,
                },
            )
            .is_err()
        );
    }

    #[test]
    fn collateral_rebalance_deducts_outbound_ignores_inbound_and_flags_stale_transfers() {
        let cfg = CollateralRebalanceConfig::default();

        // Outbound $250 leaves only $50 effective free against $300 required margin => 50/350 = 0.1428 <= 0.25 (ReduceOnly),
        // even though $500 inbound is in-flight (must NEVER be pre-credited!).
        let reduce = evaluate_collateral_rebalance(
            &cfg,
            &[CollateralRebalanceSnapshot {
                venue: "arcus".into(),
                free_collateral_usd: 300.0,
                required_margin_usd: 300.0,
                in_flight_inbound_usd: 500.0,
                in_flight_outbound_usd: 250.0,
                in_flight_age_ms: Some(15_000),
            }],
        )
        .unwrap();
        assert_eq!(reduce.action, TailRiskAction::ReduceOnly);
        assert_eq!(reduce.reasons, vec!["free_collateral_reduce_only:arcus"]);
        assert!(reduce.total_in_flight_inbound_usd > 499.0);

        // Stale in-flight transfer > max_in_flight_age_ms escalates healthy venue to ReduceOnly.
        let stale = evaluate_collateral_rebalance(
            &cfg,
            &[CollateralRebalanceSnapshot {
                venue: "hyperliquid_xyz".into(),
                free_collateral_usd: 500.0,
                required_margin_usd: 500.0,
                in_flight_inbound_usd: 100.0,
                in_flight_outbound_usd: 0.0,
                in_flight_age_ms: Some(400_000),
            }],
        )
        .unwrap();
        assert_eq!(stale.action, TailRiskAction::ReduceOnly);
        assert_eq!(
            stale.reasons,
            vec!["in_flight_transfer_stale:hyperliquid_xyz"]
        );

        // Over-committed outbound (> free) fails into HaltAndFlatten.
        let halt = evaluate_collateral_rebalance(
            &cfg,
            &[CollateralRebalanceSnapshot {
                venue: "lighter".into(),
                free_collateral_usd: 100.0,
                required_margin_usd: 200.0,
                in_flight_inbound_usd: 0.0,
                in_flight_outbound_usd: 150.0,
                in_flight_age_ms: Some(5_000),
            }],
        )
        .unwrap();
        assert_eq!(halt.action, TailRiskAction::HaltAndFlatten);

        // NaN / negative amounts fail closed with Err.
        assert!(
            evaluate_collateral_rebalance(
                &cfg,
                &[CollateralRebalanceSnapshot {
                    venue: "arcus".into(),
                    free_collateral_usd: -1.0,
                    required_margin_usd: 100.0,
                    in_flight_inbound_usd: 0.0,
                    in_flight_outbound_usd: 0.0,
                    in_flight_age_ms: None,
                }],
            )
            .is_err()
        );
    }

    #[test]
    fn collateral_rebalance_rejects_duplicate_canonical_venues() {
        let cfg = CollateralRebalanceConfig::default();
        let make_snap = |venue: &str, outbound: f64| CollateralRebalanceSnapshot {
            venue: venue.into(),
            free_collateral_usd: 100.0,
            required_margin_usd: 100.0,
            in_flight_inbound_usd: 0.0,
            in_flight_outbound_usd: outbound,
            in_flight_age_ms: Some(100),
        };

        let repeated = evaluate_collateral_rebalance(
            &cfg,
            &[make_snap(" Venue_A ", 60.0), make_snap("venue_a", 60.0)],
        );
        assert!(
            repeated
                .unwrap_err()
                .to_string()
                .contains("duplicate collateral venue")
        );

        let distinct = evaluate_collateral_rebalance(
            &cfg,
            &[make_snap(" Venue_A ", 60.0), make_snap("Venue_B", 60.0)],
        )
        .unwrap();
        assert_eq!(distinct.action, TailRiskAction::Normal);
        assert_eq!(distinct.total_in_flight_outbound_usd, 120.0);

        let consolidated =
            evaluate_collateral_rebalance(&cfg, &[make_snap("Venue_A", 120.0)]).unwrap();
        assert_eq!(consolidated.action, TailRiskAction::HaltAndFlatten);
    }
}
