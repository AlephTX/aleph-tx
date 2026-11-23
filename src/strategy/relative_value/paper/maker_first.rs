//! Public-tape maker-first replay. The ledger and protected depth model are
//! shared with the sequential IOC simulator.

use super::{Fill, IncentiveTurnoverReport, Ledger, sweep};
use crate::strategy::kalman_basis::{
    KalmanBasisConfig, KalmanBasisFilter, KalmanBasisSnapshot, MakerEntrySide, SessionRegime,
    StationarityStatus,
};
use crate::strategy::relative_value::*;
use crate::strategy::replay_queue_depth::ReplayQueueDepth;
use crate::strategy::slow_cascade::{
    CascadeSide, SlowCascadeConfig, SlowCascadeDecision, SlowCascadeGuard,
};
use std::collections::{BTreeMap, VecDeque};

mod basis_reversion;
mod basis_risk;
mod costs;
mod entry_book;
mod entry_flow;
mod entry_guards;
mod funding;
mod paired_cash_exit;
mod posture;
use basis_reversion::BasisReversionState;
pub use basis_reversion::{BasisReversionReport, MakerBasisReversion};
use basis_risk::BasisRiskState;
pub use basis_risk::{BasisRiskReport, BasisRiskReserve};
use entry_book::EntryBookState;
pub use entry_book::{EntryBookFilter, EntryBookReport};
use entry_flow::EntryFlowState;
pub use entry_flow::{EntryFlowFilter, EntryFlowReport};
use entry_guards::{
    BookEvidence, EntryEdgePersistState, InventoryLadderState, PostFillRefreshState,
    fingerprint_leg_book,
};
pub use entry_guards::{
    EntryEdgePersistReport, EntryEdgePersistence, InventoryLadderConfig, InventoryLadderReport,
    PostFillRefreshMode, PostFillRefreshReport,
};
pub use funding::{
    CarryDriftAdjustment, CarryDriftReport, FlatAtFunding, FundingFlatReport,
    FundingLifecycleReport, FundingSettlementPolicy,
};
use funding::{CarryDriftState, FundingLifecycleState, FundingState, TimerKind};
pub use paired_cash_exit::{PairedCashExit, PairedCashExitReport};
use posture::PostureState;
pub use posture::{TurnoverControl, TurnoverPosture, TurnoverPosturePolicy, TurnoverPostureReport};

const MAX_FEEDBACK_LEVELS: usize = 8;

/// Configuration capability check, not validation of a tape or a profitability
/// claim. Funding avoidance still requires runtime proof at every boundary.
#[derive(Debug, Serialize)]
pub struct MakerFirstPreflight {
    pub scope: &'static str,
    pub start_ms: u64,
    pub end_ms: u64,
    pub duration_ms: u64,
    pub trade_from_ms: u64,
    pub execution_supported: bool,
    pub equity_session_gate_enforced: bool,
    pub funding_slots_by_leg: [u64; 2],
    pub first_funding_slot_ms: [Option<u64>; 2],
    pub blockers: Vec<&'static str>,
    pub required_engine_work: Vec<&'static str>,
    pub required_evidence: Vec<&'static str>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MakerFirstConfig {
    pub pair: PairConfig,
    pub maker_leg: usize,
    pub start_ms: u64,
    /// Feed and basis observations start at `start_ms`; opening orders wait for this receipt time.
    #[serde(default)]
    pub trade_from_ms: Option<u64>,
    pub end_ms: u64,
    pub transport_ms: u64,
    pub fill_notice_ms: u64,
    /// Venue-enforced speed-bump before a passive Maker order activates on the book
    /// (e.g. `200 ms` on Lighter Robinhood Standard, `0 ms` on Lighter Core Standard).
    #[serde(default)]
    pub maker_create_delay_ms: u64,
    pub maker_cancel_delay_ms: u64,
    pub taker_delay_ms: [u64; 2],
    pub trade_clock_guard_ms: u64,
    /// Offline sensitivity: include the clock-uncertain cancellation overlap.
    /// False preserves the historical strict-before-cancel matching model.
    #[serde(default)]
    pub conservative_cancel_guard: bool,
    pub trade_delivery_grace_ms: u64,
    pub quote_lifetime_ms: u64,
    pub reprice_ms: u64,
    pub hold_ms: u64,
    pub unhedged_timeout_ms: u64,
    pub drain_ms: u64,
    /// Research-only: prove flat through every funding guard; never impute a payment.
    #[serde(default)]
    pub flat_at_funding: Option<FlatAtFunding>,
    /// Touch control submits without a net edge gate.
    pub require_net_edge: bool,
    /// If omitted, compare both opening directions on every decision.
    #[serde(default)]
    pub entry_side: Option<HedgeSide>,
    /// Turnover-only entry direction; every posture still hedges confirmed fills fully.
    #[serde(default)]
    pub turnover_posture: Option<TurnoverPosturePolicy>,
    /// Allow small confirmed inventory to close passively before routing it out.
    #[serde(default)]
    pub passive_offset: Option<PassiveOffset>,
    /// Adjust the target-venue entry reserve using completed round trips.
    #[serde(default)]
    pub turnover_feedback: Option<TurnoverFeedback>,
    /// Separate waiting budget and economics for an already hedged position.
    #[serde(default)]
    pub hedged_exit: Option<HedgedExit>,
    /// Offline experiment: request the ordinary cancel-and-drain sequence when
    /// both displayed full exits cover this cycle's fees and explicit costs.
    /// A quote-based trigger is not a fill or a guaranteed profitable exit.
    #[serde(default)]
    pub paired_cash_exit: Option<PairedCashExit>,
    /// Offline diagnostics only; never changes admission or execution.
    #[serde(default)]
    pub diagnose_cycles: bool,
    /// Offline read-only diagnostics for the opening-maker-fill to hedge-complete phase
    /// (first available hedge touch, wait/miss breakdown, and exact ms split; `S14-1`).
    /// Never changes admission or execution; omitted from report JSON when `false`.
    #[serde(default)]
    pub diagnose_entry_hedge: bool,
    /// Optional execution policy for hedge/unwind IOCs (`submit_ioc`), decoupling protective
    /// limit-price cushion, max depth impact, and retry cadence from opening admission (`S14-1`).
    /// `None` preserves existing behavior byte-identically.
    #[serde(default)]
    pub hedge_ioc_policy: Option<HedgeIocPolicy>,
    #[serde(with = "rust_decimal::serde::str")]
    pub maker_fee_bps: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub quote_notional: Decimal,
    /// A reserve for changes in basis and hedge prices before matching.
    #[serde(with = "rust_decimal::serde::str")]
    pub latency_reserve_bps: Decimal,
    /// Optional decoupled adverse hedge-move tolerance (bps) before cancelling a resting
    /// opening or closing maker quote (`None` defaults to `latency_reserve_bps`).
    /// Prevents sub-bps hedge-book ticks from churning resting quotes on 500ms+ cancel paths.
    #[serde(default, with = "rust_decimal::serde::str_option")]
    pub hedge_protection_bps: Option<Decimal>,
    /// Optional adverse hedge-move tolerance (bps) applied specifically to resting
    /// `paired_close` maker quotes (`None` falls back to `hedge_protection_bps` / `latency_reserve_bps`).
    #[serde(default, with = "rust_decimal::serde::str_option")]
    pub closing_hedge_protection_bps: Option<Decimal>,
    /// Offline turnover entry reserve; never changes exits or hedge protection.
    #[serde(default)]
    pub basis_risk_reserve: Option<BasisRiskReserve>,
    /// Causal synchronized-book EWMA basis reversion policy for `PairHedge` mode.
    #[serde(default)]
    pub basis_reversion: Option<MakerBasisReversion>,
    /// Causal liquid-leg shock gate for opening maker quotes.
    #[serde(default)]
    pub entry_flow_filter: Option<EntryFlowFilter>,
    /// Ex-ante synchronized L1 spread, hedge touch depth, and imbalance gate for opening quotes (`S6-3`).
    #[serde(default)]
    pub entry_book_filter: Option<EntryBookFilter>,
    /// Maximum displayed book levels inspected on opening quotes (`1..=MAX_LEVELS`).
    #[serde(default = "default_closing_max_levels")]
    pub opening_max_levels: usize,
    /// Retain resting opening quotes across sub-minimum partial fills so queue-front
    /// residuals can accumulate above `min_notional` before externalizing.
    #[serde(default)]
    pub aggregate_sub_minimum_fills: bool,
    /// Explicitly permit a single budgeted two-step `FOK` top-up + `reduce_only` IOC
    /// unwind during `drain` when a venue enforces `min_contracts` on `reduce_only`.
    /// Kept separate from `aggregate_sub_minimum_fills` for independent ablation.
    #[serde(default)]
    pub allow_sub_minimum_top_up_unwind: bool,
    /// Enforce dynamic equity session, corporate-action window, and external reference
    /// freshness gates on `MakerFirstEvent::Book` frames (`RWA-01..03`).
    #[serde(default)]
    pub enforce_equity_session_gate: bool,
    /// Optional opt-in probabilistic queue-position sensitivity model (`hftbacktest` power-law
    /// cancellation depletion + initial queue fraction). `None` keeps the default conservative
    /// 100%-ahead / 0%-cancellation FIFO model byte-identical (`S8-3`).
    #[serde(default)]
    pub queue_model: Option<ProbabilisticQueueConfig>,
    /// Optional causal conditional-markout + hedge-fee entry distance gate (`S9-3`):
    /// requires signed opening distance from companion anchor mid `>= fee + conditional_markout + min_profit`.
    #[serde(default)]
    pub entry_markout_gate: Option<EntryMarkoutGate>,
    /// Research-only: reconcile continuous cash across funding slots using
    /// `source_time` fill watermarks and explicit `WitnessedSettlement` events (`ARB-IMP-05`).
    #[serde(default)]
    pub funding_settlement: Option<FundingSettlementPolicy>,
    /// Optional hourly carry-drift basis threshold adjustment (`ARB-IMP-05`).
    #[serde(default)]
    pub carry_drift_adjustment: Option<CarryDriftAdjustment>,
    /// Optional causal slow-cascade asymmetric opening entry throttle (`CRASH-IMP-02`).
    #[serde(default)]
    pub slow_cascade: Option<SlowCascadeConfig>,
    /// Optional causal Kalman + OU stationarity basis filter (`ARB-IMP-04`).
    #[serde(default)]
    pub kalman_basis: Option<KalmanBasisConfig>,
    /// Optional convex exposure-based opening threshold surcharge (`ARB-IMP-02`).
    #[serde(default)]
    pub inventory_ladder: Option<InventoryLadderConfig>,
    /// Optional causal opening-edge persistence gate (`ARB-IMP-03`).
    #[serde(default)]
    pub entry_edge_persistence: Option<EntryEdgePersistence>,
    /// Optional post-fill book refresh barrier before next opening quote (`ARB-IMP-03`).
    #[serde(default)]
    pub post_fill_book_refresh: Option<PostFillRefreshMode>,
}

fn default_entry_markout_quantile_p() -> f64 {
    0.5
}

fn default_entry_markout_max_samples() -> usize {
    32
}

fn default_true() -> bool {
    true
}

/// Ex-ante opening maker distance gate: `distance >= fee + conditional_markout + min_profit` (`S9-3`).
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EntryMarkoutGate {
    pub horizon_ms: u64,
    #[serde(with = "rust_decimal::serde::str")]
    pub prior_adverse_markout_bps: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub min_profit_bps: Decimal,
    #[serde(default = "default_entry_markout_quantile_p")]
    pub quantile_p: f64,
    #[serde(default = "default_entry_markout_max_samples")]
    pub max_samples: usize,
    #[serde(default = "default_true")]
    pub include_hedge_taker_fee: bool,
    /// Labels older than this stop counting (`S11-1`). Without expiry a burst
    /// of adverse fills can block entries for the rest of the window, because
    /// a blocked gate produces no new fills to relabel it.
    #[serde(default)]
    pub max_sample_age_ms: Option<u64>,
}

impl EntryMarkoutGate {
    fn validate(&self) -> Result<()> {
        let max_bps = Decimal::from(500);
        ensure!(
            (100..=60_000).contains(&self.horizon_ms)
                && self.prior_adverse_markout_bps >= Decimal::ZERO
                && self.prior_adverse_markout_bps <= max_bps
                && (-Decimal::from(50)..=max_bps).contains(&self.min_profit_bps)
                && self.quantile_p.is_finite()
                && self.quantile_p > 0.0
                && self.quantile_p <= 0.5
                && (4..=256).contains(&self.max_samples)
                && self
                    .max_sample_age_ms
                    .is_none_or(|ms| (1_000..=3_600_000).contains(&ms)),
            "invalid entry markout gate configuration"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct EntryMarkoutGateReport {
    pub horizon_ms: u64,
    pub samples: usize,
    #[serde(with = "rust_decimal::serde::str")]
    pub active_adverse_markout_bps: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub last_required_distance_bps: Decimal,
    pub blocked_quote_distance: usize,
    pub aged_out_samples: usize,
    pub unlabelled_expired: usize,
    pub labels_completed: usize,
    pub missing_hedge_anchor: usize,
    pub pending_labels: usize,
}

/// Optional execution policy scoped strictly to hedge/unwind IOC submissions (`submit_ioc`).
/// Never loosens opening/closing maker admission, `book.in_bounds`, `participation`,
/// `max_single_leg`, `min_order_base`, or venue `taker_delay_ms` (`S14-1`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HedgeIocPolicy {
    /// Protective limit-price cushion (bps) applied to hedge/unwind IOCs (`0..=100`).
    #[serde(with = "rust_decimal::serde::str")]
    pub limit_buffer_bps: Decimal,
    /// Optional max depth impact cap (bps) for hedge/unwind IOCs (`None` inherits
    /// `pair.max_depth_impact_bps`). Must be in `0.01..=1000` and strictly `> limit_buffer_bps`
    /// whenever `limit_buffer_bps > 0` (leaving headroom for tick alignment).
    #[serde(default, with = "rust_decimal::serde::str_option")]
    pub max_depth_impact_bps: Option<Decimal>,
    /// Optional IOC retry cadence (ms) after a miss/partial/wait while unhedged
    /// (`None` inherits `reprice_ms`). Bounded `1..=2_000`.
    #[serde(default)]
    pub retry_ms: Option<u64>,
    /// When `true`, apply the decoupled buffer/impact/retry policy ONLY to opening entry-hedge
    /// IOCs (`!unwind && !closing_hedge`), keeping paired closing hedges and forced drains on
    /// the conservative `pair.limit_buffer_bps` / `pair.max_depth_impact_bps` / `reprice_ms`
    /// (`S15-2`). Default `false` preserves Sprint 14 behavior byte-identically.
    #[serde(default)]
    pub entry_only: bool,
}

impl HedgeIocPolicy {
    fn validate(&self, pair: &PairConfig) -> Result<()> {
        let max_impact_bps = match self.max_depth_impact_bps {
            Some(bps) => bps,
            None => dec(pair.max_depth_impact_bps)?,
        };
        ensure!(
            (Decimal::ZERO..=Decimal::from(100)).contains(&self.limit_buffer_bps)
                && (Decimal::new(1, 2)..=Decimal::from(1_000)).contains(&max_impact_bps)
                && (self.limit_buffer_bps.is_zero() || self.limit_buffer_bps < max_impact_bps)
                && self.retry_ms.is_none_or(|ms| (1..=2_000).contains(&ms)),
            "invalid hedge IOC policy"
        );
        Ok(())
    }

    fn apply_to_rules(&self, mut rules: Rules) -> Rules {
        rules.limit_buffer = self.limit_buffer_bps / BPS;
        if let Some(max_impact_bps) = self.max_depth_impact_bps {
            rules.max_impact = max_impact_bps / BPS;
        }
        rules
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct EntryHedgeEpisode {
    pub cycle_id: usize,
    pub opening_maker_leg: usize,
    pub hedge_leg: usize,
    pub hedge_side: HedgeSide,
    pub opening_fill_ms: u64,
    pub first_notice_ms: Option<u64>,
    pub first_ioc_submit_ms: Option<u64>,
    pub first_ioc_arrival_ms: Option<u64>,
    pub first_hedge_fill_ms: Option<u64>,
    pub first_hedge_book_ms: Option<u64>,
    pub fully_hedged_ms: Option<u64>,
    pub forced_drain_triggered_ms: Option<u64>,
    pub risk_resolved_ms: Option<u64>,
    pub censored_unhedged_ms: Option<u64>,
    pub entry_unhedged_ms: Option<u64>,
    pub notice_wait_ms: Option<u64>,
    pub submit_wait_ms: Option<u64>,
    pub ioc_flight_and_retry_ms: Option<u64>,
    pub outcome: &'static str,
    pub opening_maker_fills: usize,
    #[serde(with = "rust_decimal::serde::str")]
    pub opening_maker_base: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub opening_maker_notional: Decimal,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub first_available_hedge_touch_at_fill: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub first_available_hedge_mid_at_fill: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub first_available_hedge_touch_at_notice: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub first_ioc_submit_touch: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub first_ioc_limit: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str")]
    pub entry_hedge_base: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub entry_hedge_notional: Decimal,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub entry_hedge_vwap: Option<Decimal>,
    pub ioc_submitted: usize,
    pub ioc_fills: usize,
    pub ioc_partial: usize,
    pub ioc_price_misses: usize,
    pub ioc_depth_misses: usize,
    pub ioc_expired: usize,
    pub stale_book_waits: usize,
    pub depth_unavailable_waits: usize,
    pub below_minimum_waits: usize,
    pub reprice_throttle_waits: usize,
}

#[derive(Debug, Default, Serialize)]
pub struct EntryHedgeDiagnostics {
    pub episodes: Vec<EntryHedgeEpisode>,
    pub total_entry_unhedged_ms: u64,
    pub total_censored_unhedged_ms: u64,
    pub total_notice_wait_ms: u64,
    pub total_submit_wait_ms: u64,
    pub total_ioc_flight_and_retry_ms: u64,
}

#[derive(Debug, Default)]
struct EntryHedgeDiagState {
    episodes: Vec<EntryHedgeEpisode>,
    active: Option<EntryHedgeEpisode>,
}

impl EntryHedgeDiagState {
    #[allow(clippy::too_many_arguments)]
    fn on_opening_fill(
        &mut self,
        now: u64,
        maker_leg: usize,
        hedge_side: HedgeSide,
        base: Decimal,
        notional: Decimal,
        hedge_touch_at_fill: Option<Decimal>,
        hedge_mid_at_fill: Option<Decimal>,
    ) {
        if let Some(ep) = &mut self.active {
            ep.opening_maker_fills += 1;
            ep.opening_maker_base += base;
            ep.opening_maker_notional += notional;
            return;
        }
        let cycle_id = self.episodes.len();
        self.active = Some(EntryHedgeEpisode {
            cycle_id,
            opening_maker_leg: maker_leg,
            hedge_leg: 1 - maker_leg,
            hedge_side,
            opening_fill_ms: now,
            first_notice_ms: None,
            first_ioc_submit_ms: None,
            first_ioc_arrival_ms: None,
            first_hedge_fill_ms: None,
            first_hedge_book_ms: None,
            fully_hedged_ms: None,
            forced_drain_triggered_ms: None,
            risk_resolved_ms: None,
            censored_unhedged_ms: None,
            entry_unhedged_ms: None,
            notice_wait_ms: None,
            submit_wait_ms: None,
            ioc_flight_and_retry_ms: None,
            outcome: "open",
            opening_maker_fills: 1,
            opening_maker_base: base,
            opening_maker_notional: notional,
            first_available_hedge_touch_at_fill: hedge_touch_at_fill,
            first_available_hedge_mid_at_fill: hedge_mid_at_fill,
            first_available_hedge_touch_at_notice: None,
            first_ioc_submit_touch: None,
            first_ioc_limit: None,
            entry_hedge_base: Decimal::ZERO,
            entry_hedge_notional: Decimal::ZERO,
            entry_hedge_vwap: None,
            ioc_submitted: 0,
            ioc_fills: 0,
            ioc_partial: 0,
            ioc_price_misses: 0,
            ioc_depth_misses: 0,
            ioc_expired: 0,
            stale_book_waits: 0,
            depth_unavailable_waits: 0,
            below_minimum_waits: 0,
            reprice_throttle_waits: 0,
        });
    }

    fn on_notice(&mut self, now: u64, hedge_touch_at_notice: Option<Decimal>) {
        if let Some(ep) = &mut self.active
            && ep.first_notice_ms.is_none()
        {
            ep.first_notice_ms = Some(now);
            ep.first_available_hedge_touch_at_notice = hedge_touch_at_notice;
        }
    }

    fn close_active(&mut self, now: u64, outcome: &'static str) {
        let Some(mut ep) = self.active.take() else {
            return;
        };
        ep.outcome = outcome;
        let elapsed = now - ep.opening_fill_ms;
        if outcome == "open_at_cutoff" {
            ep.censored_unhedged_ms = Some(elapsed);
        } else {
            ep.risk_resolved_ms = Some(now);
            ep.entry_unhedged_ms = Some(elapsed);
        }
        if outcome == "hedged" {
            ep.fully_hedged_ms = Some(now);
        }
        if let Some(notice) = ep.first_notice_ms {
            ep.notice_wait_ms = Some(notice - ep.opening_fill_ms);
            if let Some(submit) = ep.first_ioc_submit_ms {
                ep.submit_wait_ms = Some(submit.saturating_sub(notice));
                if matches!(outcome, "hedged" | "hedged_within_deadband") {
                    ep.ioc_flight_and_retry_ms = Some(now - submit);
                }
            }
        }
        if !ep.entry_hedge_base.is_zero() {
            ep.entry_hedge_vwap = Some(ep.entry_hedge_notional / ep.entry_hedge_base);
        }
        self.episodes.push(ep);
    }

    fn finish(mut self, cutoff: u64) -> EntryHedgeDiagnostics {
        self.close_active(cutoff, "open_at_cutoff");
        let total_censored_unhedged_ms = self
            .episodes
            .last()
            .and_then(|ep| ep.censored_unhedged_ms)
            .unwrap_or(0);
        let mut total_entry_unhedged_ms = 0u64;
        let mut total_notice_wait_ms = 0u64;
        let mut total_submit_wait_ms = 0u64;
        let mut total_ioc_flight_and_retry_ms = 0u64;
        for ep in &self.episodes {
            total_entry_unhedged_ms =
                total_entry_unhedged_ms.saturating_add(ep.entry_unhedged_ms.unwrap_or(0));
            total_notice_wait_ms =
                total_notice_wait_ms.saturating_add(ep.notice_wait_ms.unwrap_or(0));
            total_submit_wait_ms =
                total_submit_wait_ms.saturating_add(ep.submit_wait_ms.unwrap_or(0));
            total_ioc_flight_and_retry_ms = total_ioc_flight_and_retry_ms
                .saturating_add(ep.ioc_flight_and_retry_ms.unwrap_or(0));
        }
        EntryHedgeDiagnostics {
            episodes: self.episodes,
            total_entry_unhedged_ms,
            total_censored_unhedged_ms,
            total_notice_wait_ms,
            total_submit_wait_ms,
            total_ioc_flight_and_retry_ms,
        }
    }
}

fn default_initial_ahead_fraction() -> Decimal {
    Decimal::ONE
}

fn default_cancel_progress_power() -> f64 {
    1.0
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProbabilisticQueueConfig {
    /// Initial fraction `[0, 1]` of visible level size placed ahead of newly activated quotes.
    #[serde(
        default = "default_initial_ahead_fraction",
        with = "rust_decimal::serde::str"
    )]
    pub initial_ahead_fraction: Decimal,
    /// Power-law cancellation progress exponent `alpha` in `(0, 10]` (`x^a / (x^a + (1-x)^a)`).
    /// `1.0` = uniform cancellation along the queue; `> 1.0` shifts cancellations toward tail orders.
    #[serde(default = "default_cancel_progress_power")]
    pub cancel_progress_power: f64,
}

impl ProbabilisticQueueConfig {
    fn validate(&self) -> Result<()> {
        ensure!(
            (Decimal::ZERO..=Decimal::ONE).contains(&self.initial_ahead_fraction)
                && self.cancel_progress_power.is_finite()
                && self.cancel_progress_power > 0.0
                && self.cancel_progress_power <= 10.0,
            "invalid probabilistic queue model parameters"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct QueueModelReport {
    #[serde(with = "rust_decimal::serde::str")]
    pub initial_ahead_fraction: Decimal,
    pub cancel_progress_power: f64,
    pub cancel_depletion_events: usize,
    #[serde(with = "rust_decimal::serde::str")]
    pub cancel_depleted_ahead_base: Decimal,
}

fn default_closing_max_levels() -> usize {
    1
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HedgedExit {
    pub max_hold_ms: u64,
    #[serde(with = "rust_decimal::serde::str")]
    pub min_saving_bps: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub max_adverse_basis_bps: Decimal,
    /// Retain the original time limit from the first maker fill. Useful for
    /// comparing exit economics at the same opening horizon.
    #[serde(default)]
    pub use_cycle_deadline: bool,
    /// Optional separate lifetime for closing maker quotes so hedged exits
    /// preserve FIFO queue priority rather than expiring at entry `quote_lifetime_ms`.
    #[serde(default)]
    pub closing_quote_lifetime_ms: Option<u64>,
    /// Window from `paired_at_ms` during which closing quotes require
    /// `target_cycle_edge_bps` before falling back to `min_saving_bps`.
    #[serde(default)]
    pub patience_ms: u64,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    pub target_cycle_edge_bps: Option<Decimal>,
    #[serde(default = "default_closing_max_levels")]
    pub closing_max_levels: usize,
    /// Optional separate book-staleness horizon (ms) before forcing an emergency
    /// double-IOC unwind of a delta-neutral (`net == 0`) hedged cycle. Stale books
    /// (`> max_quote_age_ms`) still immediately cancel resting Maker quotes and block
    /// new quotes, without prematurely liquidating hedged positions across quiet books.
    /// Aborts immediately if both legs go stale simultaneously, if single-leg adverse
    /// drift exceeds `max_hedged_leg_adverse_bps` (default `150 bps`), or if isolated
    /// per-leg margin breaches `lighter::risk::evaluate_margin_health` `1.8x` safety.
    #[serde(default)]
    pub max_hedged_book_stale_ms: Option<u64>,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    pub max_hedged_leg_adverse_bps: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    pub hedged_leg_collateral_quote: Option<Decimal>,
    /// Treat cross-venue lot/min-order truncation dust (`|net| < partner min hedge`,
    /// bounded by `max_unhedged_notional_usd`) between opposite-signed legs as paired
    /// while sizing passive closing Maker quotes for the Maker leg's full native quantity.
    #[serde(default)]
    pub allow_partner_step_deadband: bool,
    /// Optional bounded grace (ms, `1..=5_000`, `<= max_hedged_book_stale_ms`) when BOTH
    /// legs simultaneously go briefly empty/unusable on a balanced (`net == 0`) hedged cycle
    /// (e.g., 100-200 ms simultaneous exchange reconnect refresh, `S12-2`). Resting Maker
    /// quotes still cancel immediately and no new quotes post while either leg is stale;
    /// emergency double-IOC fires once simultaneous blindness exceeds this grace.
    #[serde(default)]
    pub max_hedged_both_blind_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PassiveOffset {
    #[serde(with = "rust_decimal::serde::str")]
    pub max_net_notional: Decimal,
    pub max_wait_ms: u64,
    /// Out-of-sample lower-bound inputs ordered by opening buy, then sell.
    /// None retains the displayed round-trip control.
    #[serde(default)]
    pub close_probability: Option<[Decimal; 2]>,
    /// Optional minimum displayed Maker-Maker round-trip edge (bps) required to open.
    #[serde(default, with = "rust_decimal::serde::str_option")]
    pub min_passive_edge_bps: Option<Decimal>,
    /// Optional minimum realized round-trip edge (bps) required on the passive offset quote.
    #[serde(default, with = "rust_decimal::serde::str_option")]
    pub min_offset_edge_bps: Option<Decimal>,
    /// Optional adverse-move stop (bps) measured on the companion reference mid
    /// relative to cycle entry; triggers immediate `externalize` hedge when exceeded.
    #[serde(default, with = "rust_decimal::serde::str_option")]
    pub max_adverse_move_bps: Option<Decimal>,
    /// Maximum displayed levels inspected when posting a passive offset exit.
    #[serde(default = "default_closing_max_levels")]
    pub offset_max_levels: usize,
    /// Optional quote lifetime for passive offset exits to preserve FIFO priority.
    #[serde(default)]
    pub offset_quote_lifetime_ms: Option<u64>,
    /// Hold passive offset quotes until `hold_ms.min(unhedged_timeout_ms)` rather than
    /// triggering cross-venue taker `externalize` at `max_wait_ms` (`S9-3`).
    #[serde(default)]
    pub passive_loop_only: bool,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnoverFeedback {
    #[serde(with = "rust_decimal::serde::str")]
    pub min_reserve_bps: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub max_reserve_bps: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub alpha: Decimal,
    /// Additional modeled costs per unit of both venues' gross turnover.
    #[serde(with = "rust_decimal::serde::str")]
    pub overhead_bps: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub max_quote_distance_bps: Decimal,
    /// Optional fixed infrastructure cost ($/day) prorated over the active window (`S8-1`).
    /// Reported separately alongside variable slippage/fees without mutating `net_after_overhead`.
    #[serde(default, with = "rust_decimal::serde::str_option")]
    pub fixed_overhead_usd_per_day: Option<Decimal>,
}

#[derive(Debug, Serialize)]
pub struct TurnoverFeedbackReport {
    pub min_reserve_bps: Decimal,
    pub max_reserve_bps: Decimal,
    pub final_reserve_bps: Decimal,
    pub max_observed_reserve_bps: Decimal,
    pub cycles_used: usize,
    pub overhead_bps: Decimal,
    pub overhead_quote: Decimal,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixed_window_overhead_quote: Option<Decimal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net_after_fixed_window_overhead: Option<Decimal>,
    pub modeled_target_turnover_quote: Decimal,
    pub modeled_eligible_turnover_quote: Decimal,
    /// A public replay has no venue statement confirming program credit.
    pub venue_credited_turnover_quote: Option<Decimal>,
    pub net_after_overhead: Option<Decimal>,
    pub liquidation_pnl_after_overhead: Option<Decimal>,
    pub realized_cost_bps: Option<Decimal>,
    pub outward_quotes: usize,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[allow(clippy::large_enum_variant)] // Stream one event at a time without boxing each book pair.
pub enum MakerFirstEvent {
    Book {
        at_ms: u64,
        books: [Book; 2],
        #[serde(default)]
        reference: Option<Oracle>,
        #[serde(default)]
        equity_session: Option<EquitySession>,
        #[serde(default)]
        leg_oracles: Option<[Oracle; 2]>,
    },
    Trade {
        at_ms: u64,
        source_ms: u64,
        leg: usize,
        aggressor: HedgeSide,
        #[serde(with = "rust_decimal::serde::str")]
        price: Decimal,
        #[serde(with = "rust_decimal::serde::str")]
        contracts: Decimal,
    },
    TradeFeed {
        at_ms: u64,
        leg: usize,
        online: bool,
    },
    TurnoverControl {
        at_ms: u64,
        control: TurnoverControl,
    },
    FundingSettlement {
        at_ms: u64,
        leg: usize,
        settlement: WitnessedSettlement,
    },
    FundingRate {
        at_ms: u64,
        leg: usize,
        source_ms: u64,
        #[serde(with = "rust_decimal::serde::str")]
        rate_per_settlement: Decimal,
    },
    End {
        at_ms: u64,
    },
}

impl MakerFirstEvent {
    fn time(&self) -> u64 {
        match *self {
            Self::Book { at_ms, .. }
            | Self::Trade { at_ms, .. }
            | Self::TradeFeed { at_ms, .. }
            | Self::TurnoverControl { at_ms, .. }
            | Self::FundingSettlement { at_ms, .. }
            | Self::FundingRate { at_ms, .. }
            | Self::End { at_ms } => at_ms,
        }
    }
}

#[derive(Debug, Default, Serialize)]
pub struct MakerFirstReport {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub basis_risk_reserve: Option<BasisRiskReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub basis_reversion: Option<BasisReversionReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_flow_filter: Option<EntryFlowReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_book_filter: Option<EntryBookReport>,
    pub conservative_cancel_guard: bool,
    pub cancel_guard_overlap_prints: usize,
    pub cancel_guard_overlap_fills: usize,
    pub maker_fills_after_cancel_request: usize,
    #[serde(with = "rust_decimal::serde::str")]
    pub cancel_guard_overlap_turnover: Decimal,
    pub start_ms: u64,
    pub end_ms: u64,
    pub trade_from_ms: u64,
    pub book_frames: usize,
    pub synchronized_book_frames: usize,
    pub trade_prints: usize,
    pub entry_checks: usize,
    pub quotes_submitted: usize,
    pub quotes_accepted: usize,
    pub post_only_rejected: usize,
    pub post_only_rejected_crossed: usize,
    pub post_only_rejected_beyond_depth: usize,
    pub post_only_rejected_new_level: usize,
    pub quote_cancels: usize,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub opening_cancel_requested: BTreeMap<&'static str, usize>,
    pub late_trade_prints: usize,
    pub quote_expired_before_activation: usize,
    pub quote_cancelled_before_activation: usize,
    pub maker_prints_touching_quote: usize,
    pub maker_fills: usize,
    pub maker_buy_fills: usize,
    pub maker_sell_fills: usize,
    pub passive_offset_fills: usize,
    pub ioc_submitted: usize,
    pub ioc_fills: usize,
    pub ioc_partial: usize,
    pub ioc_expired: usize,
    pub ioc_price_misses: usize,
    pub ioc_depth_misses: usize,
    pub closed_cycles: usize,
    pub passive_only_cycles: usize,
    pub passive_only_pnl: Decimal,
    pub passive_only_maker_turnover: Decimal,
    /// Denominators include cycles still open at the cutoff, by opening side.
    pub passive_cycles_started: [usize; 2],
    pub passive_cycles_completed: [usize; 2],
    pub forced_closes: usize,
    pub exposure_stops: usize,
    pub unhedged_episodes: usize,
    pub total_unhedged_ms: u64,
    pub max_unhedged_ms: u64,
    pub max_net_notional: Decimal,
    pub maker_turnover: Decimal,
    pub taker_turnover: Decimal,
    pub fees: Decimal,
    pub maker_fees: Decimal,
    pub ioc_slippage: Decimal,
    pub ioc_depth_impact: Decimal,
    pub ioc_submit_touch_turnover: Decimal,
    pub maker_spread_capture: Decimal,
    pub forced_close_pnl: Decimal,
    pub cycle_attribution: CycleAttribution,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_costs: Option<costs::Costs>,
    pub turnover_posture: TurnoverPostureReport,
    pub realized_pnl: Decimal,
    pub pnl_before_fees: Option<Decimal>,
    pub net_pnl: Option<Decimal>,
    pub final_liquidation_pnl: Option<Decimal>,
    #[serde(serialize_with = "super::serialize_positions")]
    pub open_positions_base: [Decimal; 2],
    pub best_modeled_edge_bps: Option<Decimal>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub best_projected_turnover_cost_bps: Option<Decimal>,
    pub entry_blocked: BTreeMap<&'static str, usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub incentive_turnover: Option<IncentiveTurnoverReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turnover_feedback: Option<TurnoverFeedbackReport>,
    pub below_minimum_hedge: usize,
    pub below_minimum_unwind: usize,
    pub turnover_breakdown: TurnoverBreakdown,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rwa_session: Option<RwaSessionAttribution>,
    pub incomplete: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub funding_flat: Option<FundingFlatReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub funding_lifecycle: Option<FundingLifecycleReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub carry_drift: Option<CarryDriftReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queue_model: Option<QueueModelReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_markout_gate: Option<EntryMarkoutGateReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_hedge_diagnostics: Option<EntryHedgeDiagnostics>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paired_cash_exit: Option<PairedCashExitReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slow_cascade: Option<SlowCascadeReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kalman_basis: Option<KalmanBasisReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inventory_ladder: Option<InventoryLadderReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_edge_persistence: Option<EntryEdgePersistReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub post_fill_book_refresh: Option<PostFillRefreshReport>,
}

/// Telemetry emitted when [`MakerFirstConfig::slow_cascade`] is enabled (`CRASH-IMP-02`).
#[derive(Debug, Clone, Copy, Serialize)]
pub struct SlowCascadeReport {
    pub active_side: CascadeSide,
    pub completed_bars: usize,
    pub gap_resets: u64,
    pub rejected_inputs: u64,
    pub blocked_opening_entries: usize,
    pub scaled_opening_entries: usize,
    pub cancelled_resting_entries: usize,
}

/// Telemetry emitted when [`MakerFirstConfig::kalman_basis`] is enabled (`ARB-IMP-04`).
#[derive(Debug, Clone, Serialize)]
pub struct KalmanBasisReport {
    pub initialized: bool,
    pub current_regime: Option<SessionRegime>,
    pub alpha_bps: f64,
    pub variance_p: f64,
    pub last_stationarity: Option<StationarityStatus>,
    pub last_dislocation_bps: Option<f64>,
    pub last_half_life_ms: Option<f64>,
    pub regime_resets: u64,
    pub gap_resets: u64,
    pub rejected_inputs: u64,
    pub blocked_warmup_or_invalid: usize,
    pub blocked_non_stationary: usize,
    pub blocked_dislocation_band: usize,
    pub blocked_side_mismatch: usize,
    pub cancelled_resting_entries: usize,
    pub time_stop_triggers: usize,
}

/// Explicit four-denominator turnover separation (`VMM-01` / `OPT-01`) so target-venue,
/// dual-venue gross, rule-window candidate, and venue-confirmed eligible volume are never conflated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TurnoverBreakdown {
    pub capability_snapshot_version: &'static str,
    #[serde(with = "rust_decimal::serde::str")]
    pub target_venue_turnover_quote: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub dual_venue_gross_turnover_quote: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub rule_window_candidate_turnover_quote: Decimal,
    /// Public-tape replay has no authenticated venue points/rebate statement; always `None`.
    #[serde(with = "rust_decimal::serde::str_option")]
    pub confirmed_eligible_turnover_quote: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str")]
    pub excluded_recovery_turnover_quote: Decimal,
}

impl Default for TurnoverBreakdown {
    fn default() -> Self {
        Self {
            capability_snapshot_version: crate::venues::VENUE_CAPABILITY_SNAPSHOT_VERSION,
            target_venue_turnover_quote: Decimal::ZERO,
            dual_venue_gross_turnover_quote: Decimal::ZERO,
            rule_window_candidate_turnover_quote: Decimal::ZERO,
            confirmed_eligible_turnover_quote: None,
            excluded_recovery_turnover_quote: Decimal::ZERO,
        }
    }
}

/// RWA equity session and oracle regime attribution across `MakerFirstEvent::Book` frames (`RWA-01..03`).
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct RwaSessionAttribution {
    pub regular_cash_frames: usize,
    pub extended_external_frames: usize,
    pub off_hours_internal_or_close_frames: usize,
    pub halted_or_uncovered_frames: usize,
    pub missing_session_metadata_frames: usize,
    pub session_blocked_entries: usize,
}

#[derive(Debug, Default, Serialize)]
pub struct CycleAttribution {
    /// In pair leg order. Includes every simulated fill, even in unfinished cycles.
    #[serde(serialize_with = "super::serialize_positions")]
    pub opening_maker_fees: [Decimal; 2],
    #[serde(serialize_with = "super::serialize_positions")]
    pub opening_hedge_fees: [Decimal; 2],
    #[serde(serialize_with = "super::serialize_positions")]
    pub closing_maker_fees: [Decimal; 2],
    #[serde(serialize_with = "super::serialize_positions")]
    pub closing_hedge_fees: [Decimal; 2],
    #[serde(serialize_with = "super::serialize_positions")]
    pub forced_ioc_fees: [Decimal; 2],
    pub closing_quotes: ClosingQuoteAttribution,
    pub closing_maker_fills: usize,
    #[serde(with = "rust_decimal::serde::str")]
    pub closing_maker_turnover: Decimal,
    #[serde(serialize_with = "super::serialize_positions")]
    pub forced_ioc_turnover: [Decimal; 2],
    #[serde(serialize_with = "super::serialize_positions")]
    pub forced_ioc_slippage: [Decimal; 2],
    /// Lifecycle cohorts require flat positions and complete evidence at close.
    pub paired_closed_cycles: usize,
    pub paired_maker_closes: usize,
    #[serde(with = "rust_decimal::serde::str")]
    pub paired_maker_close_pnl: Decimal,
    pub forced_ioc_cycles: usize,
    #[serde(with = "rust_decimal::serde::str")]
    pub forced_ioc_cycle_pnl: Decimal,
    pub total_time_to_hedge_ms: u64,
    pub max_time_to_hedge_ms: u64,
    pub timed_paired_closed_cycles: usize,
    pub total_hedge_to_close_ms: u64,
    pub max_hedge_to_close_ms: u64,
    pub paired_cycles_open_at_cutoff: usize,
    #[serde(with = "rust_decimal::serde::str")]
    pub max_paired_gross_notional: Decimal,
    pub hedged_time_stops: usize,
    pub hedged_basis_stops: usize,
    pub hedged_book_stops: usize,
    pub reduce_ioc_dust_avoided: usize,
    pub reduce_ioc_dust_fills: usize,
    pub sub_min_top_up_attempts: usize,
    pub sub_min_top_up_failures: usize,
    pub sub_min_top_up_unwinds: usize,
    #[serde(serialize_with = "super::serialize_positions")]
    pub sub_min_recovery_turnover: [Decimal; 2],
    pub paired_drain_blocked: usize,
    pub partner_step_deadband_cycles: usize,
}

#[derive(Debug, Default, Serialize)]
pub struct ClosingQuoteAttribution {
    /// Each stage counts a quote once, including quotes in unfinished cycles.
    pub submitted: usize,
    pub accepted: usize,
    pub touched: usize,
    pub filled: usize,
    pub expired_before_activation: usize,
    pub post_only_rejected: usize,
    pub post_only_rejected_crossed: usize,
    pub post_only_rejected_beyond_depth: usize,
    pub post_only_rejected_new_level: usize,
    /// Counts the first cancellation request for each quote.
    pub cancel_requested: BTreeMap<&'static str, usize>,
}

impl MakerFirstReport {
    fn blocked(&mut self, reason: &'static str) {
        *self.entry_blocked.entry(reason).or_default() += 1;
    }
}

struct Quote {
    posture: TurnoverPosture,
    side: HedgeSide,
    price: Decimal,
    left: Decimal,
    ahead: Decimal,
    queue_depth: Option<Box<ReplayQueueDepth<Decimal>>>,
    arrival_ms: u64,
    activated_ms: Option<u64>,
    expires_ms: u64,
    cancelled_ms: Option<u64>,
    hedge_limit: Option<Decimal>,
    passive_entry: bool,
    closing: bool,
    closing_touched: bool,
    closing_filled: bool,
}

struct Candidate {
    edge_bps: Decimal,
    raw_edge_bps_before_carry: Option<Decimal>,
    cycle_edge_bps: Option<Decimal>,
    passive_entry: bool,
    queue_ahead: Decimal,
    price: Decimal,
    quantity: Decimal,
    side: HedgeSide,
    hedge_limit: Option<Decimal>,
    turnover: Option<TurnoverProjection>,
}

struct TurnoverProjection {
    gross: Decimal,
    fees: Decimal,
    fallback_cash: Decimal,
    eligible: Decimal,
}

struct Ioc {
    leg: usize,
    side: HedgeSide,
    quantity: Decimal,
    limit: Decimal,
    touch: Decimal,
    arrival_ms: u64,
    expires_ms: u64,
    unwind: bool,
    closing_hedge: bool,
    sub_min_top_up: bool,
}

#[derive(Clone, Copy, Default)]
struct SubMinSunk {
    base: Decimal,
    cash: Decimal,
    turnover: Decimal,
    fees: Decimal,
    eligible: Decimal,
}

struct Cycle {
    id: usize,
    posture: TurnoverPosture,
    started_ms: u64,
    initial_cash: Decimal,
    initial_turnover: Decimal,
    initial_fees: Decimal,
    draining: bool,
    externalizing: bool,
    maker_turnover: Decimal,
    passive_side: Option<usize>,
    entry_ref_mid: Option<Decimal>,
    paired_mids: [Option<Decimal>; 2],
    paired_at_ms: Option<u64>,
    hedge_fill_at_ms: Option<u64>,
    paired_basis_bps: Option<Decimal>,
    closing_maker_filled: bool,
    forced_ioc_filled: bool,
    sub_min_top_up_attempts: usize,
    sub_min_recovery_pending_base: Decimal,
    used_partner_step_deadband: bool,
    cash_exit_triggered: bool,
}

#[derive(Clone, Copy)]
struct ClosedCycleSummary {
    posture_idx: usize,
    draining: bool,
    externalizing: bool,
    paired_maker_close: bool,
    forced_ioc_filled: bool,
    cash_exit_triggered: bool,
    recorded_in_attribution: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct HedgeBookObservation {
    source_ms: u64,
    touch: [Decimal; 2],
}

struct SlowCascadeState {
    guard: SlowCascadeGuard,
    last_decision: Option<SlowCascadeDecision>,
    last_observation: Option<HedgeBookObservation>,
    blocked_opening_entries: usize,
    scaled_opening_entries: usize,
    cancelled_resting_entries: usize,
}

impl SlowCascadeState {
    fn new(config: SlowCascadeConfig) -> Result<Self> {
        Ok(Self {
            guard: SlowCascadeGuard::new(config)?,
            last_decision: None,
            last_observation: None,
            blocked_opening_entries: 0,
            scaled_opening_entries: 0,
            cancelled_resting_entries: 0,
        })
    }

    fn scale_for(&self, side: HedgeSide) -> f64 {
        let Some(decision) = self.last_decision.filter(|d| d.data_healthy) else {
            return 0.0;
        };
        match side {
            HedgeSide::Buy => decision.bid_entry_scale,
            HedgeSide::Sell => decision.ask_entry_scale,
        }
    }

    fn report(&self) -> SlowCascadeReport {
        SlowCascadeReport {
            active_side: self.guard.side(),
            completed_bars: self.guard.completed_bars(),
            gap_resets: self.guard.gap_resets(),
            rejected_inputs: self.guard.rejected_inputs(),
            blocked_opening_entries: self.blocked_opening_entries,
            scaled_opening_entries: self.scaled_opening_entries,
            cancelled_resting_entries: self.cancelled_resting_entries,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct KalmanBookObservation {
    source_ms: [u64; 2],
    touch: [[Decimal; 2]; 2],
    regime: SessionRegime,
}

struct KalmanBasisState {
    filter: KalmanBasisFilter,
    last_snapshot: Option<KalmanBasisSnapshot>,
    last_observation: Option<KalmanBookObservation>,
    blocked_warmup_or_invalid: usize,
    blocked_non_stationary: usize,
    blocked_dislocation_band: usize,
    blocked_side_mismatch: usize,
    cancelled_resting_entries: usize,
    time_stop_triggers: usize,
}

impl KalmanBasisState {
    fn new(config: KalmanBasisConfig) -> Result<Self> {
        Ok(Self {
            filter: KalmanBasisFilter::new(config)?,
            last_snapshot: None,
            last_observation: None,
            blocked_warmup_or_invalid: 0,
            blocked_non_stationary: 0,
            blocked_dislocation_band: 0,
            blocked_side_mismatch: 0,
            cancelled_resting_entries: 0,
            time_stop_triggers: 0,
        })
    }

    fn check_entry(&self, side: HedgeSide) -> std::result::Result<(), &'static str> {
        let Some(snap) = self.last_snapshot else {
            return Err("kalman_basis_warmup_or_invalid");
        };
        if !snap.valid_observation || snap.stationarity == StationarityStatus::WarmingUp {
            return Err("kalman_basis_warmup_or_invalid");
        }
        if snap.stationarity == StationarityStatus::NonStationary {
            return Err("kalman_basis_non_stationary");
        }
        let eval = self.filter.evaluate_entry(&snap);
        if !eval.allowed {
            return Err("kalman_basis_band");
        }
        let side_ok = matches!(
            (eval.preferred_side, side),
            (Some(MakerEntrySide::Buy), HedgeSide::Buy)
                | (Some(MakerEntrySide::Sell), HedgeSide::Sell)
        );
        if !side_ok {
            return Err("kalman_basis_side");
        }
        Ok(())
    }

    fn record_block(&mut self, reason: &'static str) {
        match reason {
            "kalman_basis_warmup_or_invalid" => self.blocked_warmup_or_invalid += 1,
            "kalman_basis_non_stationary" => self.blocked_non_stationary += 1,
            "kalman_basis_band" => self.blocked_dislocation_band += 1,
            "kalman_basis_side" => self.blocked_side_mismatch += 1,
            _ => {}
        }
    }

    fn report(&self) -> KalmanBasisReport {
        KalmanBasisReport {
            initialized: self.filter.is_initialized(),
            current_regime: self.filter.current_regime(),
            alpha_bps: self.filter.alpha_bps(),
            variance_p: self.filter.variance_p(),
            last_stationarity: self.last_snapshot.map(|s| s.stationarity),
            last_dislocation_bps: self.last_snapshot.map(|s| s.dislocation_bps),
            last_half_life_ms: self.last_snapshot.and_then(|s| s.half_life_ms),
            regime_resets: self.filter.regime_resets(),
            gap_resets: self.filter.gap_resets(),
            rejected_inputs: self.filter.rejected_inputs(),
            blocked_warmup_or_invalid: self.blocked_warmup_or_invalid,
            blocked_non_stationary: self.blocked_non_stationary,
            blocked_dislocation_band: self.blocked_dislocation_band,
            blocked_side_mismatch: self.blocked_side_mismatch,
            cancelled_resting_entries: self.cancelled_resting_entries,
            time_stop_triggers: self.time_stop_triggers,
        }
    }
}

struct Replay<'a> {
    config: &'a MakerFirstConfig,
    rules: Rules,
    common_step: Decimal,
    ledger: Ledger,
    leg_turnover: [Decimal; 2],
    books: Option<[Book; 2]>,
    last_reference: Option<Oracle>,
    last_equity_session: Option<EquitySession>,
    equity_session_uncertain: bool,
    rwa_session: RwaSessionAttribution,
    saw_rwa_metadata: bool,
    usable: [bool; 2],
    last_usable_at_ms: [Option<u64>; 2],
    consumed_ms: [u64; 2],
    last_source_ms: [u64; 2],
    feed_online: bool,
    quote: Option<Quote>,
    ioc: Option<Ioc>,
    cycle: Option<Cycle>,
    next_cycle_id: usize,
    closed_cycles_meta: BTreeMap<usize, ClosedCycleSummary>,
    next_quote_ms: u64,
    next_ioc_ms: u64,
    next_entry_ioc_ms: u64,
    notices: VecDeque<(u64, Decimal)>,
    unhedged_since: Option<u64>,
    cost_feedback: CostFeedback,
    basis_risk: BasisRiskState,
    basis_reversion: BasisReversionState,
    entry_flow: EntryFlowState,
    entry_book: EntryBookState,
    entry_markout: EntryMarkoutState,
    entry_hedge_diag: Option<EntryHedgeDiagState>,
    posture: PostureState,
    funding: Option<FundingState>,
    funding_lifecycle: Option<FundingLifecycleState>,
    carry_drift: CarryDriftState,
    slow_cascade: Option<SlowCascadeState>,
    kalman_basis: Option<KalmanBasisState>,
    inventory_ladder: InventoryLadderState,
    entry_edge_persist: EntryEdgePersistState,
    post_fill_refresh: PostFillRefreshState,
    report: MakerFirstReport,
}

const ENTRY_MARKOUT_MAX_SAMPLES: usize = 256;
const ENTRY_MARKOUT_MIN_SAMPLES: usize = 4;

#[derive(Default)]
struct EntryMarkoutState {
    pending: VecDeque<(u64, HedgeSide, Decimal)>,
    /// `(labelled_at_ms, nonnegative adverse move in bps)`.
    adverse_bps: VecDeque<(u64, Decimal)>,
    /// Upper-tail adverse quantile, recomputed only when labels change so the
    /// per-level quote check stays allocation- and sort-free.
    cached_quantile: Option<Decimal>,
    last_required_bps: std::cell::Cell<Option<Decimal>>,
    blocked_quote_distance: std::cell::Cell<usize>,
    aged_out_samples: usize,
    unlabelled_expired: usize,
    labels_completed: usize,
    missing_hedge_anchor: usize,
}

impl EntryMarkoutState {
    fn enqueue(&mut self, due_ms: u64, side: HedgeSide, hedge_mid: Option<Decimal>) {
        let Some(anchor_mid) = hedge_mid.filter(|m| *m > Decimal::ZERO) else {
            self.missing_hedge_anchor += 1;
            return;
        };
        if self.pending.len() >= ENTRY_MARKOUT_MAX_SAMPLES {
            self.pending.pop_front();
            self.unlabelled_expired += 1;
        }
        self.pending.push_back((due_ms, side, anchor_mid));
    }

    fn advance(&mut self, now: u64, hedge_mark: Option<(u64, Decimal)>, policy: EntryMarkoutGate) {
        const GRACE_MS: u64 = 1_000;
        let mut changed = false;
        while let Some(&(due_ms, side, entry_mid)) = self.pending.front() {
            if now < due_ms {
                break;
            }
            if now - due_ms > GRACE_MS {
                self.pending.pop_front();
                self.unlabelled_expired += 1;
                continue;
            }
            // A maker-side quote would mix venue basis into the hedge return.
            // A pre-horizon source quote cannot confirm a post-horizon observation.
            let Some((_, mid)) =
                hedge_mark.filter(|(at, mid)| (due_ms..=now).contains(at) && *mid > Decimal::ZERO)
            else {
                break;
            };
            self.pending.pop_front();
            let raw_move_bps = (mid - entry_mid) / entry_mid * BPS;
            let adverse = match side {
                HedgeSide::Buy => (-raw_move_bps).max(Decimal::ZERO),
                HedgeSide::Sell => raw_move_bps.max(Decimal::ZERO),
            };
            if self.adverse_bps.len() >= policy.max_samples {
                self.adverse_bps.pop_front();
            }
            self.adverse_bps.push_back((now, adverse));
            self.labels_completed += 1;
            changed = true;
        }
        if let Some(max_age) = policy.max_sample_age_ms {
            while self
                .adverse_bps
                .front()
                .is_some_and(|(at, _)| now.saturating_sub(*at) > max_age)
            {
                self.adverse_bps.pop_front();
                self.aged_out_samples += 1;
                changed = true;
            }
        }
        if changed {
            self.cached_quantile = self.upper_tail_quantile(policy);
        }
    }

    fn upper_tail_quantile(&self, policy: EntryMarkoutGate) -> Option<Decimal> {
        let n = self.adverse_bps.len();
        if n < ENTRY_MARKOUT_MIN_SAMPLES {
            return None;
        }
        let mut scratch = [Decimal::ZERO; ENTRY_MARKOUT_MAX_SAMPLES];
        for (slot, (_, value)) in scratch.iter_mut().zip(&self.adverse_bps) {
            *slot = *value;
        }
        let sorted = &mut scratch[..n];
        sorted.sort_unstable();
        // Nonnegative adverse magnitudes: the upper tail (1 - p) is the worst loss.
        let upper_frac = (1.0 - policy.quantile_p).clamp(0.0, 1.0);
        let idx = ((n - 1) as f64 * upper_frac).round() as usize;
        Some(sorted[idx.min(n - 1)])
    }

    fn active_markout_bps(&self, policy: EntryMarkoutGate) -> Decimal {
        self.cached_quantile
            .map_or(policy.prior_adverse_markout_bps, |q| {
                policy.prior_adverse_markout_bps.max(q)
            })
    }

    fn required_distance_bps(&self, policy: EntryMarkoutGate, fee_term_bps: Decimal) -> Decimal {
        let req = fee_term_bps + self.active_markout_bps(policy) + policy.min_profit_bps;
        self.last_required_bps.set(Some(req));
        req
    }

    fn record_block(&self) {
        self.blocked_quote_distance
            .set(self.blocked_quote_distance.get() + 1);
    }

    fn report(&self, policy: EntryMarkoutGate, default_fee_bps: Decimal) -> EntryMarkoutGateReport {
        let active = self.active_markout_bps(policy);
        let req = self
            .last_required_bps
            .get()
            .unwrap_or(default_fee_bps + active + policy.min_profit_bps);
        EntryMarkoutGateReport {
            horizon_ms: policy.horizon_ms,
            samples: self.adverse_bps.len(),
            active_adverse_markout_bps: active,
            last_required_distance_bps: req,
            blocked_quote_distance: self.blocked_quote_distance.get(),
            aged_out_samples: self.aged_out_samples,
            unlabelled_expired: self.unlabelled_expired,
            labels_completed: self.labels_completed,
            missing_hedge_anchor: self.missing_hedge_anchor,
            pending_labels: self.pending.len(),
        }
    }
}

#[derive(Default)]
struct CostFeedback {
    excess_reserve_bps: Decimal,
    max_excess_reserve_bps: Decimal,
    cycles_used: usize,
    outward_quotes: usize,
    target_turnover: Decimal,
}

impl TurnoverFeedback {
    fn overhead(self, gross: Decimal) -> Decimal {
        gross * self.overhead_bps / BPS
    }
}

impl CostFeedback {
    fn observe(&mut self, policy: TurnoverFeedback, net: Decimal, eligible: Decimal) {
        if policy.alpha.is_zero() {
            return;
        }
        let headroom = policy.max_reserve_bps - policy.min_reserve_bps;
        if eligible > Decimal::ZERO {
            let cap = headroom * eligible;
            let cost = (-net * BPS).clamp(-cap, cap) / eligible;
            self.excess_reserve_bps = (self.excess_reserve_bps * (Decimal::ONE - policy.alpha)
                + policy.alpha * cost)
                .clamp(Decimal::ZERO, headroom);
            self.cycles_used += 1;
        } else if net < Decimal::ZERO {
            // The loss belongs to the strategy even without target turnover.
            self.excess_reserve_bps = headroom;
        }
        self.max_excess_reserve_bps = self.max_excess_reserve_bps.max(self.excess_reserve_bps);
    }

    fn reserve(&self, policy: TurnoverFeedback) -> Decimal {
        policy.min_reserve_bps + self.excess_reserve_bps
    }
}

fn opposite(side: HedgeSide) -> HedgeSide {
    match side {
        HedgeSide::Buy => HedgeSide::Sell,
        HedgeSide::Sell => HedgeSide::Buy,
    }
}

fn side_index(side: HedgeSide) -> usize {
    usize::from(side == HedgeSide::Sell)
}

fn direction(quantity: Decimal) -> HedgeSide {
    if quantity > Decimal::ZERO {
        HedgeSide::Sell
    } else {
        HedgeSide::Buy
    }
}

fn signed_cost(side: HedgeSide, actual: Decimal, reference: Decimal) -> Decimal {
    if side == HedgeSide::Buy {
        actual - reference
    } else {
        reference - actual
    }
}

/// Power-law fraction `[0, 1]` of net level cancellations removed from `ahead`
/// (ahead probability complementary to hftbacktest's `PowerProbQueueFunc`).
fn power_queue_cancel_share(ahead: Decimal, prev_depth: Decimal, power: f64) -> Decimal {
    if ahead <= Decimal::ZERO || prev_depth <= Decimal::ZERO {
        return Decimal::ZERO;
    }
    if ahead >= prev_depth {
        return Decimal::ONE;
    }
    let Some(ratio) = (ahead / prev_depth).to_f64() else {
        return Decimal::ONE;
    };
    let x = ratio.clamp(0.0, 1.0);
    if x <= 0.0 {
        return Decimal::ZERO;
    }
    if x >= 1.0 {
        return Decimal::ONE;
    }
    let xa = x.powf(power);
    let one_minus_xa = (1.0 - x).powf(power);
    let denom = xa + one_minus_xa;
    let share = if denom > 0.0 && denom.is_finite() {
        (xa / denom).clamp(0.0, 1.0)
    } else {
        x
    };
    Decimal::try_from(share)
        .unwrap_or(Decimal::ONE)
        .clamp(Decimal::ZERO, Decimal::ONE)
}

fn cash(side: HedgeSide, notional: Decimal, fee_bps: Decimal) -> Decimal {
    signed_cost(side, Decimal::ZERO, notional) - notional * fee_bps / BPS
}

fn min_order_base(terms: &MarketTerms, limit: Decimal, reduces_leg: bool) -> Decimal {
    if reduces_leg && terms.venue == "arcus" {
        (terms.min_contracts * terms.base_per_contract).max(terms.base_step())
    } else {
        terms.min_base(limit)
    }
}

fn min_reduce_ioc_base(terms: &MarketTerms, limit: Decimal) -> Decimal {
    match terms.venue.as_str() {
        "lighter" | "lighter_robinhood" | "trade_xyz" | "entropy" => terms.base_step(),
        "arcus" | "extended" => {
            (terms.min_contracts * terms.base_per_contract).max(terms.base_step())
        }
        _ => terms.min_base(limit),
    }
}

impl MakerFirstConfig {
    #[inline]
    fn maker_arrival_delay_ms(&self) -> u64 {
        self.transport_ms.saturating_add(self.maker_create_delay_ms)
    }

    fn entry_horizon_ms(&self) -> u64 {
        let hold_ms = self.hedged_exit.map_or(self.hold_ms, |p| {
            if p.use_cycle_deadline {
                p.max_hold_ms.max(self.hold_ms)
            } else {
                p.max_hold_ms.saturating_add(self.hold_ms)
            }
        });
        self.drain_ms
            .saturating_add(hold_ms)
            .saturating_add(self.maker_create_delay_ms)
            .saturating_add(self.quote_lifetime_ms)
    }

    fn validate(&self, registry: &VenueRegistry) -> Result<()> {
        let capability = self.preflight(registry)?;
        ensure!(
            capability.execution_supported,
            "maker-first execution window crosses a funding slot"
        );
        Ok(())
    }

    pub fn preflight(&self, registry: &VenueRegistry) -> Result<MakerFirstPreflight> {
        self.pair.validate(registry)?;
        if let Some(policy) = self.basis_risk_reserve {
            policy.validate(self)?;
        }
        if let Some(policy) = self.basis_reversion {
            policy.validate(self)?;
        }
        if let Some(policy) = self.entry_flow_filter {
            policy.validate()?;
        }
        if let Some(policy) = self.entry_book_filter {
            policy.validate()?;
        }
        if let Some(qm) = self.queue_model {
            qm.validate()?;
        }
        if let Some(gate) = self.entry_markout_gate {
            gate.validate()?;
        }
        if let Some(policy) = self.hedge_ioc_policy {
            policy.validate(&self.pair)?;
        }
        ensure!(
            (self.pair.asset_class == AssetClass::Crypto
                || self.flat_at_funding.is_some()
                || self.funding_settlement.is_some())
                && (self.pair.mode.is_pair_hedge() || self.require_net_edge)
                && self.maker_leg < 2
                && (1..=DAY_MS).contains(&self.start_ms.abs_diff(self.end_ms))
                && self.start_ms > 0
                && self.start_ms < self.end_ms
                && (1..=2_000).contains(&self.transport_ms)
                && self.fill_notice_ms <= 2_000
                && self.maker_create_delay_ms <= 2_000
                && self.maker_cancel_delay_ms <= 2_000
                && self.taker_delay_ms.iter().all(|d| *d <= 2_000)
                && self.trade_clock_guard_ms <= 1_000
                && self.trade_delivery_grace_ms <= 5_000
                && self.end_ms < u64::MAX - DAY_MS
                && (100..=60_000).contains(&self.quote_lifetime_ms)
                && (1..=2_000).contains(&self.reprice_ms)
                && (1..=180_000).contains(&self.hold_ms)
                && (100..=30_000).contains(&self.unhedged_timeout_ms)
                && (1_000..=60_000).contains(&self.drain_ms)
                && (1..=MAX_LEVELS).contains(&self.opening_max_levels)
                && self
                    .trade_from_ms
                    .is_none_or(|at| (self.start_ms..self.end_ms).contains(&at))
                && self.entry_horizon_ms()
                    < self.end_ms - self.trade_from_ms.unwrap_or(self.start_ms)
                && (-Decimal::from(100)..=Decimal::from(1_000)).contains(&self.maker_fee_bps)
                && self.quote_notional > Decimal::ZERO
                && bounded(self.quote_notional)
                && (Decimal::ZERO..=Decimal::from(100)).contains(&self.latency_reserve_bps)
                && self
                    .hedge_protection_bps
                    .is_none_or(|bps| (Decimal::ZERO..=Decimal::from(100)).contains(&bps))
                && self
                    .closing_hedge_protection_bps
                    .is_none_or(|bps| (Decimal::ZERO..=Decimal::from(100)).contains(&bps)),
            "invalid maker-first execution policy"
        );
        if let Some(policy) = self.turnover_posture {
            ensure!(
                !self.pair.mode.is_pair_hedge()
                    && self.entry_side.is_none()
                    && self.passive_offset.is_none()
                    && self.hedged_exit.is_some()
                    && (1..=3_600_000).contains(&policy.max_control_ttl_ms),
                "invalid isolated turnover posture policy"
            );
        }
        if let Some(offset) = &self.passive_offset {
            let cancel_window = self.transport_ms
                + self.maker_cancel_delay_ms
                + self.trade_delivery_grace_ms
                + self.trade_clock_guard_ms;
            let max_bps = Decimal::from(1_000);
            ensure!(
                offset.max_net_notional > Decimal::ZERO
                    && offset.max_net_notional <= Rules::new(&self.pair)?.max_single_leg
                    && offset.max_wait_ms <= self.unhedged_timeout_ms
                    && offset.max_wait_ms < self.hold_ms
                    && offset.max_wait_ms
                        > 2 * cancel_window + self.maker_arrival_delay_ms() + self.fill_notice_ms
                    && (1..=MAX_LEVELS).contains(&offset.offset_max_levels)
                    && offset
                        .offset_quote_lifetime_ms
                        .is_none_or(|ms| (100..=30_000).contains(&ms))
                    && offset
                        .min_passive_edge_bps
                        .is_none_or(|bps| (-Decimal::from(100)..=max_bps).contains(&bps))
                    && offset
                        .min_offset_edge_bps
                        .is_none_or(|bps| (-Decimal::from(100)..=max_bps).contains(&bps))
                    && offset
                        .max_adverse_move_bps
                        .is_none_or(|bps| bps > Decimal::ZERO && bps <= max_bps)
                    && offset.close_probability.is_none_or(|p| {
                        self.require_net_edge
                            && p.iter().all(|v| (Decimal::ZERO..=Decimal::ONE).contains(v))
                    })
                    && (self.pair.mode.incentive().is_none() || offset.close_probability.is_some()),
                "invalid passive offset size or time budget"
            );
        }
        if let Some(policy) = self.turnover_feedback {
            let max_bps = Decimal::from(100);
            ensure!(
                self.pair.mode.incentive().is_some()
                    && policy.min_reserve_bps > Decimal::ZERO
                    && (policy.min_reserve_bps..=max_bps).contains(&policy.max_reserve_bps)
                    && (Decimal::ZERO..=Decimal::ONE).contains(&policy.alpha)
                    && (Decimal::ZERO..=max_bps).contains(&policy.overhead_bps)
                    && (Decimal::ZERO..=max_bps).contains(&policy.max_quote_distance_bps)
                    && policy
                        .fixed_overhead_usd_per_day
                        .is_none_or(|v| v >= Decimal::ZERO && v <= Decimal::from(10_000)),
                "invalid maker-first turnover feedback"
            );
        }
        if let Some(policy) = self.paired_cash_exit {
            policy.validate(self)?;
        }
        if let Some(policy) = self.hedged_exit {
            let max_bps = Decimal::from(1_000);
            ensure!(
                (1..=180_000).contains(&policy.max_hold_ms)
                    && (Decimal::ZERO..=max_bps).contains(&policy.min_saving_bps)
                    && policy.max_adverse_basis_bps > Decimal::ZERO
                    && policy.max_adverse_basis_bps <= max_bps
                    && policy
                        .closing_quote_lifetime_ms
                        .is_none_or(|ms| (100..=180_000).contains(&ms))
                    && policy
                        .max_hedged_book_stale_ms
                        .is_none_or(|ms| (100..=180_000).contains(&ms))
                    && policy.max_hedged_both_blind_ms.is_none_or(|ms| {
                        (1..=5_000).contains(&ms)
                            && policy
                                .max_hedged_book_stale_ms
                                .is_some_and(|stale_ms| ms <= stale_ms)
                    })
                    && policy
                        .max_hedged_leg_adverse_bps
                        .is_none_or(|bps| bps > Decimal::ZERO && bps <= max_bps)
                    && policy
                        .hedged_leg_collateral_quote
                        .is_none_or(|col| col > Decimal::ZERO && bounded(col))
                    && policy.patience_ms <= policy.max_hold_ms
                    && (1..=MAX_FEEDBACK_LEVELS).contains(&policy.closing_max_levels)
                    && policy
                        .target_cycle_edge_bps
                        .is_none_or(|bps| (-max_bps..=max_bps).contains(&bps))
                    && (policy.patience_ms == 0 || policy.target_cycle_edge_bps.is_some()),
                "invalid hedged exit policy"
            );
        }
        ensure!(
            self.flat_at_funding.is_none() || self.funding_settlement.is_none(),
            "cannot enable both flat_at_funding and funding_settlement"
        );
        if let Some(policy) = self.flat_at_funding {
            policy.validate(self)?;
        }
        if let Some(policy) = self.funding_settlement {
            policy.validate(self)?;
        }
        if let Some(policy) = self.carry_drift_adjustment {
            policy.validate(self)?;
        }
        if let Some(policy) = self.slow_cascade {
            policy.validate()?;
        }
        if let Some(policy) = self.kalman_basis {
            policy.validate()?;
            ensure!(
                self.pair.mode.is_pair_hedge(),
                "kalman_basis requires PairHedge mode"
            );
            ensure!(
                self.basis_reversion.is_none(),
                "cannot enable both basis_reversion and kalman_basis"
            );
        }
        if let Some(policy) = self.inventory_ladder {
            policy.validate(self)?;
        }
        if let Some(policy) = self.entry_edge_persistence {
            policy.validate(self)?;
        }
        let mut slots = [0; 2];
        let mut first = [None; 2];
        for (i, leg) in self.pair.legs.iter().enumerate() {
            let (at, count) = leg
                .funding
                .window(self.start_ms, self.end_ms)
                .ok_or_else(|| anyhow::anyhow!("funding schedule overflow"))?;
            slots[i] = count;
            first[i] = (count > 0).then_some(at);
        }
        let supported =
            slots == [0; 2] || self.flat_at_funding.is_some() || self.funding_settlement.is_some();
        let equity_session_gate_enforced =
            self.pair.asset_class != AssetClass::Equity || self.enforce_equity_session_gate;
        let mut required_evidence = if self.flat_at_funding.is_some() {
            vec![
                "verified_funding_schedule_and_settlement_guard",
                "bounded_source_to_receipt_trade_delivery",
                "public_trade_and_book_continuity_across_payment_boundaries",
                "flat_positions_and_resolved_orders_at_every_funding_guard",
            ]
        } else if self.funding_settlement.is_some() {
            vec![
                "verified_funding_schedule_and_settlement_guard",
                "bounded_source_to_receipt_trade_delivery",
                "witnessed_settlement_for_every_held_slot",
            ]
        } else if supported {
            vec![]
        } else {
            vec![
                "verified_realized_or_witnessed_payment_for_each_held_slot",
                "venue_market_currency_and_funding_schedule_provenance",
                "public_trade_and_book_continuity_across_payment_boundaries",
            ]
        };
        if !equity_session_gate_enforced {
            required_evidence.push("equity_session_and_reference_gate_enforced");
        }
        Ok(MakerFirstPreflight {
            scope: "configuration_only_no_tape_validated",
            start_ms: self.start_ms,
            end_ms: self.end_ms,
            duration_ms: self.end_ms - self.start_ms,
            trade_from_ms: self.trade_from_ms.unwrap_or(self.start_ms),
            execution_supported: supported,
            equity_session_gate_enforced,
            funding_slots_by_leg: slots,
            first_funding_slot_ms: first,
            blockers: if supported {
                vec![]
            } else {
                vec!["maker_first_funding_lifecycle_not_supported"]
            },
            // Maker prints arrive after their source timestamps. Booking funding
            // against receipt-time inventory would misprice a late pre-slot fill.
            // These are implementation requirements, not merely missing data.
            required_engine_work: if supported {
                vec![]
            } else {
                vec![
                    "source_time_position_watermark_and_late_fill_resolution",
                    "settlement_booking_in_continuous_cash_and_loss_ledger",
                    "funding_boundary_fill_and_order_guard",
                ]
            },
            required_evidence,
        })
    }
}

impl Replay<'_> {
    fn funding_paused(&self, now: u64) -> bool {
        self.funding.as_ref().is_some_and(|state| state.paused(now))
    }

    fn funding_failure(&mut self, reason: &'static str) {
        self.report.incomplete.get_or_insert(reason);
        if let Some(state) = &mut self.funding {
            state.report.failure.get_or_insert(reason);
        }
    }

    fn funding_lifecycle_failure(&mut self, reason: &'static str) {
        self.report.incomplete.get_or_insert(reason);
        if let Some(lifecycle) = &mut self.funding_lifecycle {
            lifecycle.report.failure.get_or_insert(reason);
        }
    }

    fn funding_lifecycle_guard_active(&self, now: u64) -> bool {
        self.funding_lifecycle
            .as_ref()
            .is_some_and(|s| s.guard_active(now))
    }

    fn funding_lifecycle_blocks_quote(&self, now: u64) -> bool {
        self.funding_lifecycle.as_ref().is_some_and(|s| {
            s.guard_active(now) || s.guard_active(now + self.config.maker_arrival_delay_ms())
        })
    }

    fn resolve_funding_settlements(&mut self, now: u64, at_cutoff: bool) {
        let Some(lifecycle) = &mut self.funding_lifecycle else {
            return;
        };
        let (payouts, err) = lifecycle.resolve_ready(now, at_cutoff);
        let active_id = self.cycle.as_ref().map(|c| c.id);
        for payout in payouts {
            let low = payout.low;
            let high = payout.high;
            self.ledger.cash += low;
            self.ledger.funding += low;
            self.ledger.funding_upper += high;
            self.ledger.witnessed_funding_payments += 1;
            let belongs_to_active = payout.cycle_id.is_some() && payout.cycle_id == active_id;
            if let Some(costs) = &mut self.report.execution_costs {
                costs.book_funding(payout.cycle_id, low);
            }
            if !belongs_to_active {
                // Keep currently active cycle PnL isolated from late funding belonging to an
                // earlier closed cycle or unattributed cross-cycle remainder.
                if let Some(active) = &mut self.cycle {
                    active.initial_cash += low;
                }
                self.report.realized_pnl += low;
                if let Some(closed) = payout
                    .cycle_id
                    .and_then(|id| self.closed_cycles_meta.get(&id).copied())
                {
                    self.report.turnover_posture.realized_pnl[closed.posture_idx] += low;
                    if closed.draining {
                        self.report.forced_close_pnl += low;
                    } else if !closed.externalizing {
                        self.report.passive_only_pnl += low;
                    }
                    if closed.recorded_in_attribution {
                        if closed.forced_ioc_filled {
                            self.report.cycle_attribution.forced_ioc_cycle_pnl += low;
                        }
                        if closed.paired_maker_close {
                            self.report.cycle_attribution.paired_maker_close_pnl += low;
                        }
                    }
                    if closed.cash_exit_triggered
                        && let Some(stats) = &mut self.report.paired_cash_exit
                    {
                        stats.realized_stress_cash += low;
                    }
                }
            }
        }
        if let Some(reason) = err {
            self.funding_lifecycle_failure(reason);
        }
    }

    fn record_funding_lifecycle_fill(
        &mut self,
        leg: usize,
        cycle_id: Option<usize>,
        source_ms: u64,
        receipt_ms: u64,
        signed_base: Decimal,
    ) {
        let Some(lifecycle) = &mut self.funding_lifecycle else {
            return;
        };
        if let Some(reason) = lifecycle.on_fill(leg, cycle_id, source_ms, receipt_ms, signed_base) {
            self.funding_lifecycle_failure(reason);
        }
    }

    fn next_carry_drift_stale_at(&self) -> Option<u64> {
        let policy = self.config.carry_drift_adjustment?;
        let quote = self
            .quote
            .as_ref()
            .filter(|q| !q.closing && q.cancelled_ms.is_none())?;
        let _ = quote;
        self.carry_drift.next_stale_at(policy)
    }

    fn advance_timers(&mut self, until: u64, mut min_now: u64) -> Result<()> {
        loop {
            let funding_at = self
                .funding
                .as_ref()
                .and_then(|s| s.timers.front().map(|(at, _)| *at))
                .filter(|at| *at <= until);
            let lifecycle_at = self
                .funding_lifecycle
                .as_ref()
                .and_then(|s| s.next_timer_due(until));
            let carry_stale_at = self
                .next_carry_drift_stale_at()
                .map(|at| at.max(min_now))
                .filter(|at| *at <= until);
            let next_at = [funding_at, lifecycle_at, carry_stale_at]
                .into_iter()
                .flatten()
                .min();
            let Some(fired_at) = next_at else {
                return Ok(());
            };
            min_now = min_now.max(fired_at);
            if funding_at == Some(fired_at) {
                let (now, kind) = self
                    .funding
                    .as_mut()
                    .and_then(|s| s.timers.pop_front())
                    .expect("funding timer inspected above");
                self.step_funding_timer(now, kind)?;
            } else if lifecycle_at == Some(fired_at) {
                let now = self
                    .funding_lifecycle
                    .as_mut()
                    .and_then(|s| s.pop_timer_due(until))
                    .expect("lifecycle timer inspected above");
                self.step_funding_lifecycle_timer(now)?;
            } else {
                if self.posture.expire(fired_at) {
                    self.report.turnover_posture.controls_expired += 1;
                    self.posture_changed(fired_at);
                }
                self.risk(fired_at);
                self.advance_quote(fired_at, false);
                self.advance(fired_at)?;
                self.risk(fired_at);
            }
        }
    }

    fn step_funding_lifecycle_timer(&mut self, now: u64) -> Result<()> {
        if self.posture.expire(now) {
            self.report.turnover_posture.controls_expired += 1;
            self.posture_changed(now);
        }
        self.resolve_funding_settlements(now, false);
        self.risk(now);
        self.advance_quote(now, false);
        self.advance(now)?;
        self.resolve_funding_settlements(now, false);
        self.risk(now);
        Ok(())
    }

    fn funding_flat(&self) -> bool {
        self.ledger.is_flat()
            && self.quote.is_none()
            && self.ioc.is_none()
            && self.notices.is_empty()
            && self.cycle.is_none()
    }

    fn step_funding_timer(&mut self, now: u64, kind: TimerKind) -> Result<()> {
        if self.posture.expire(now) {
            self.report.turnover_posture.controls_expired += 1;
            self.posture_changed(now);
        }
        if matches!(kind, TimerKind::Resume) {
            // A book received before the guard cannot authorize resumption.
            self.usable = [false; 2];
            self.last_usable_at_ms = [None; 2];
            self.funding
                .as_mut()
                .expect("funding timer owns state")
                .resume_after_ms = now;
        }
        self.risk(now);
        self.advance_quote(now, false);
        self.advance(now)?;
        self.risk(now);
        if matches!(kind, TimerKind::Check | TimerKind::Slot(_)) {
            if !self.funding_flat() {
                self.funding_failure("funding_boundary_not_flat_and_resolved");
            }
            let state = self.funding.as_mut().expect("funding timer owns state");
            state.report.boundary_checks += 1;
            if let TimerKind::Slot(legs) = kind
                && state.report.failure.is_none()
                && self.report.incomplete.is_none()
            {
                for (i, paid) in legs.into_iter().enumerate() {
                    state.report.verified_slots_by_leg[i] += u64::from(paid);
                }
            }
        }
        Ok(())
    }

    #[cfg(test)]
    fn funding_timers(&mut self, until: u64) -> Result<()> {
        loop {
            let timer = self.funding.as_mut().and_then(|state| {
                state.timers.front().filter(|(at, _)| *at <= until)?;
                state.timers.pop_front()
            });
            let Some((now, kind)) = timer else {
                return Ok(());
            };
            self.step_funding_timer(now, kind)?;
        }
    }

    fn maker(&self) -> usize {
        self.config.maker_leg
    }

    fn quote_lifetime_ms(&self, closing: bool) -> u64 {
        if closing {
            if self.ledger.positions[1 - self.maker()].is_zero()
                && let Some(ms) = self
                    .config
                    .passive_offset
                    .as_ref()
                    .and_then(|policy| policy.offset_quote_lifetime_ms)
            {
                return ms;
            }
            self.config
                .hedged_exit
                .and_then(|policy| policy.closing_quote_lifetime_ms)
                .unwrap_or(self.config.quote_lifetime_ms)
        } else {
            self.config.quote_lifetime_ms
        }
    }

    fn net(&self) -> Decimal {
        self.ledger.positions.iter().sum()
    }

    fn posture_changed(&mut self, now: u64) {
        self.report.turnover_posture.posture_changes += 1;
        self.cancel(now, "posture_change");
        if let Some(c) = &mut self.cycle {
            c.draining = true;
        }
    }

    fn fresh(&self, i: usize, now: u64) -> Option<&Book> {
        let book = self.books.as_ref()?.get(i)?;
        (self.usable[i] && now - book.timestamp_ms <= self.config.pair.planner.max_quote_age_ms)
            .then_some(book)
    }

    fn pair_books(&self, now: u64) -> Option<[&Book; 2]> {
        let (a, b) = (self.fresh(0, now)?, self.fresh(1, now)?);
        (a.timestamp_ms.abs_diff(b.timestamp_ms) <= self.config.pair.planner.max_quote_skew_ms)
            .then_some([a, b])
    }

    /// Latest time to request cancellation while preserving the full wait
    /// budget through modeled cancel latency and late-print delivery.
    fn passive_cancel_by(&self, now: u64) -> Option<u64> {
        let policy = self.config.passive_offset.as_ref()?;
        let cycle = self.cycle.as_ref()?;
        let maker = self.maker();
        let book = self.fresh(maker, now)?;
        if cycle.draining
            || cycle.externalizing
            || (policy.close_probability.is_some() && cycle.passive_side.is_none())
            || !self.feed_online
            || !self.ledger.positions[1 - maker].is_zero()
            || self.net().abs() * book.asks[0].price > policy.max_net_notional
        {
            return None;
        }
        if let Some(max_adverse) = policy.max_adverse_move_bps {
            let ref_book = self.fresh(1 - maker, now)?;
            let entry_ref = cycle.entry_ref_mid?;
            let current_ref = ref_book.mid();
            let pos = self.ledger.positions[maker];
            let adverse_bps = if pos > Decimal::ZERO {
                (entry_ref - current_ref) / entry_ref * BPS
            } else if pos < Decimal::ZERO {
                (current_ref - entry_ref) / entry_ref * BPS
            } else {
                Decimal::ZERO
            };
            if adverse_bps > max_adverse {
                return None;
            }
        }
        let wait_ms = if policy.passive_loop_only {
            self.config.hold_ms.min(self.config.unhedged_timeout_ms)
        } else {
            policy.max_wait_ms
        };
        let cancelled_by = cycle.started_ms + wait_ms;
        let cancel_window = self.config.transport_ms
            + self.config.maker_cancel_delay_ms
            + self.config.trade_delivery_grace_ms
            + self.config.trade_clock_guard_ms;
        cancelled_by
            .checked_sub(cancel_window)
            .filter(|at| now < *at)
    }

    fn set_books(&mut self, books: [Book; 2], now: u64) -> Result<()> {
        for (i, book) in books.iter().enumerate() {
            ensure!(
                book.timestamp_ms <= now,
                "maker-first book is from the future"
            );
            self.usable[i] = book.timestamp_ms > 0
                && book.timestamp_ms >= self.last_source_ms[i]
                && self
                    .funding
                    .as_ref()
                    .is_none_or(|s| book.timestamp_ms >= s.resume_after_ms)
                && book.valid_depth(&self.config.pair.legs[i]);
            if self.usable[i] {
                self.last_source_ms[i] = book.timestamp_ms;
                self.last_usable_at_ms[i] = Some(now);
            }
        }
        self.books = Some(books);
        if let Some(mode) = self.config.post_fill_book_refresh {
            self.post_fill_refresh
                .observe_books(mode, self.usable, self.last_source_ms);
        }
        if let Some(policy) = self.config.basis_risk_reserve {
            let observation = self.pair_books(now).filter(|_| self.feed_online).map(|b| {
                let maker = b[self.maker()].mid();
                let hedge = b[1 - self.maker()].mid();
                (
                    (maker - hedge) / hedge * BPS,
                    [b[0].timestamp_ms, b[1].timestamp_ms],
                )
            });
            self.basis_risk
                .observe(policy, self.config.latency_reserve_bps, now, observation);
        }
        if let Some(policy) = self.config.basis_reversion {
            if !self.feed_online || !self.usable[0] || !self.usable[1] {
                self.basis_reversion.observe(policy, now, None);
            } else if let Some(b) = self.pair_books(now) {
                let maker_book = b[self.maker()];
                let maker = maker_book.mid();
                let hedge = b[1 - self.maker()].mid();
                let quote_basis_bps = if policy.use_quote_level_dislocation && hedge > Decimal::ZERO
                {
                    let buy_touch = maker_book.side(opposite(HedgeSide::Buy))[0].price;
                    let sell_touch = maker_book.side(opposite(HedgeSide::Sell))[0].price;
                    Some([
                        (buy_touch - hedge) / hedge * BPS,
                        (sell_touch - hedge) / hedge * BPS,
                    ])
                } else {
                    None
                };
                self.basis_reversion.observe_with_quotes(
                    policy,
                    now,
                    Some((
                        (maker - hedge) / hedge * BPS,
                        [b[0].timestamp_ms, b[1].timestamp_ms],
                    )),
                    quote_basis_bps,
                );
            } else if self.basis_reversion.exceeds_gap(policy, now) {
                self.basis_reversion.observe(policy, now, None);
            }
        }
        if let Some(policy) = self.config.entry_flow_filter {
            let hedge_leg = 1 - self.maker();
            if !self.feed_online || !self.usable[hedge_leg] {
                self.entry_flow.observe(policy, now, None);
            } else if let Some(b) = self.fresh(hedge_leg, now) {
                self.entry_flow
                    .observe(policy, now, Some((b.mid(), b.timestamp_ms)));
            } else if self.entry_flow.exceeds_gap(policy, now) {
                self.entry_flow.observe(policy, now, None);
            }
        }
        if self.slow_cascade.is_some() {
            let hedge_leg = 1 - self.maker();
            let obs = if self.feed_online && self.usable[hedge_leg] {
                self.fresh(hedge_leg, now).and_then(|b| {
                    b.mid().to_f64().map(|px| {
                        (
                            HedgeBookObservation {
                                source_ms: b.timestamp_ms,
                                touch: [b.bids[0].price, b.asks[0].price],
                            },
                            px,
                        )
                    })
                })
            } else {
                None
            };
            if let Some(sc) = &mut self.slow_cascade {
                if let Some((observation, px)) = obs {
                    if let Some(previous) = sc.last_observation
                        && previous.source_ms == observation.source_ms
                    {
                        if previous != observation {
                            sc.last_decision = None;
                        }
                    } else {
                        sc.last_observation = Some(observation);
                        sc.last_decision = Some(sc.guard.observe(now, px));
                    }
                } else {
                    sc.last_decision = None;
                }
            }
        }
        let mark = self
            .fresh(self.maker(), now)
            .map(|b| (b.timestamp_ms, b.mid()));
        if let Some(costs) = &mut self.report.execution_costs {
            costs.book(now, mark);
        }
        self.report.book_frames += 1;
        self.report.synchronized_book_frames += usize::from(self.pair_books(now).is_some());
        Ok(())
    }

    fn cancel(&mut self, now: u64, reason: &'static str) {
        if let Some(quote) = self.quote.as_mut()
            && quote.cancelled_ms.is_none()
        {
            quote.cancelled_ms =
                Some(now + self.config.transport_ms + self.config.maker_cancel_delay_ms);
            self.report.quote_cancels += 1;
            if quote.closing {
                *self
                    .report
                    .cycle_attribution
                    .closing_quotes
                    .cancel_requested
                    .entry(reason)
                    .or_default() += 1;
            } else {
                *self
                    .report
                    .opening_cancel_requested
                    .entry(reason)
                    .or_default() += 1;
            }
        }
    }

    fn advance_quote(&mut self, now: u64, book_frame: bool) {
        let Some(mut quote) = self.quote.take() else {
            return;
        };
        if quote.cancelled_ms.is_some_and(|at| {
            let last_receipt =
                at + self.config.trade_delivery_grace_ms + self.config.trade_clock_guard_ms;
            if self.config.conservative_cancel_guard {
                now > last_receipt
            } else {
                now >= last_receipt
            }
        }) {
            return;
        }
        if quote.cancelled_ms.is_some_and(|at| now >= at) {
            if quote.activated_ms.is_some() {
                self.quote = Some(quote);
            } else {
                self.report.quote_cancelled_before_activation += 1;
            }
            return;
        }
        let newly_activated = quote.activated_ms.is_none();
        if newly_activated && now >= quote.arrival_ms {
            if now >= quote.expires_ms {
                self.report.quote_expired_before_activation += 1;
                self.report
                    .cycle_attribution
                    .closing_quotes
                    .expired_before_activation += usize::from(quote.closing);
                return;
            }
            if let Some(book) = self.fresh(self.maker(), now)
                && book.timestamp_ms >= quote.arrival_ms
            {
                let touch = book.side(quote.side)[0].price;
                if !book.in_bounds(quote.price)
                    || signed_cost(quote.side, quote.price, touch) >= Decimal::ZERO
                {
                    self.report.post_only_rejected += 1;
                    self.report.post_only_rejected_crossed += 1;
                    let closing_quotes = &mut self.report.cycle_attribution.closing_quotes;
                    closing_quotes.post_only_rejected += usize::from(quote.closing);
                    closing_quotes.post_only_rejected_crossed += usize::from(quote.closing);
                    return;
                }
                let same_side = book.side(opposite(quote.side));
                let worst = same_side.last().expect("valid book").price;
                let level = same_side.iter().find(|level| level.price == quote.price);
                let beyond_depth = signed_cost(quote.side, quote.price, worst) < Decimal::ZERO;
                let new_level_disallowed = !beyond_depth
                    && self.config.turnover_feedback.is_some()
                    && self.cycle.is_none()
                    && level.is_none();
                if beyond_depth || new_level_disallowed {
                    self.report.post_only_rejected += 1;
                    self.report.post_only_rejected_beyond_depth += usize::from(beyond_depth);
                    self.report.post_only_rejected_new_level += usize::from(new_level_disallowed);
                    let closing_quotes = &mut self.report.cycle_attribution.closing_quotes;
                    closing_quotes.post_only_rejected += usize::from(quote.closing);
                    closing_quotes.post_only_rejected_beyond_depth +=
                        usize::from(quote.closing && beyond_depth);
                    closing_quotes.post_only_rejected_new_level +=
                        usize::from(quote.closing && new_level_disallowed);
                    return;
                }
                let level_base = level.map_or(Decimal::ZERO, |level| level.contracts)
                    * self.config.pair.legs[self.maker()].base_per_contract;
                quote.ahead = self
                    .config
                    .queue_model
                    .map_or(level_base, |qm| level_base * qm.initial_ahead_fraction);
                quote.queue_depth = self
                    .config
                    .queue_model
                    .map(|_| Box::new(ReplayQueueDepth::new(now)));
                quote.activated_ms = Some(now);
                self.report.quotes_accepted += 1;
                self.report.cycle_attribution.closing_quotes.accepted += usize::from(quote.closing);
            }
        } else if book_frame
            && !newly_activated
            && quote.cancelled_ms.is_none_or(|at| now < at)
            && let Some(qm) = self.config.queue_model
            && let Some(history) = quote.queue_depth.as_mut()
        {
            let visible = self.fresh(self.maker(), now).and_then(|book| {
                let levels = book.side(opposite(quote.side));
                let worst = levels.last()?;
                if signed_cost(quote.side, quote.price, worst.price) < Decimal::ZERO {
                    return None;
                }
                let base = levels
                    .iter()
                    .find(|l| l.price == quote.price)
                    .map_or(Decimal::ZERO, |l| l.contracts)
                    * self.config.pair.legs[self.maker()].base_per_contract;
                Some((book.timestamp_ms, base))
            });
            if let Some((source, depth)) = visible.filter(|_| self.feed_online) {
                history.book(
                    source,
                    depth,
                    now,
                    (
                        self.config.trade_clock_guard_ms,
                        self.config.trade_delivery_grace_ms,
                        self.config.pair.planner.max_quote_age_ms,
                    ),
                    |before, after| {
                        let drop = before - after;
                        let share =
                            power_queue_cancel_share(quote.ahead, before, qm.cancel_progress_power);
                        let depleted = (share * drop)
                            .max(quote.ahead - after)
                            .min(drop)
                            .min(quote.ahead);
                        quote.ahead -= depleted;
                        if depleted > Decimal::ZERO
                            && let Some(rep) = self.report.queue_model.as_mut()
                        {
                            rep.cancel_depletion_events += 1;
                            rep.cancel_depleted_ahead_base += depleted;
                        }
                    },
                );
            } else {
                history.reset(now);
            }
        }
        let expired = now >= quote.expires_ms;
        let hedge_side = opposite(quote.side);
        let hedge_leg = 1 - self.maker();
        let hedgeable = match quote.hedge_limit {
            Some(previous) => self.pair_books(now).is_some_and(|books| {
                let paired_close = quote.closing && self.config.hedged_exit.is_some();
                if !self.config.require_net_edge && !paired_close {
                    return true;
                }
                let hedge_qty = if paired_close
                    && self
                        .config
                        .hedged_exit
                        .is_some_and(|p| p.allow_partner_step_deadband)
                {
                    floor_step(
                        quote.left.min(self.ledger.positions[hedge_leg].abs()),
                        self.config.pair.legs[hedge_leg].base_step(),
                    )
                    .unwrap_or(Decimal::ZERO)
                } else if quote.closing {
                    quote.left
                } else {
                    self.ledger.positions[self.maker()].abs() + quote.left
                };
                let limit = if paired_close {
                    self.reduce_limit(books[hedge_leg], hedge_leg, hedge_side, hedge_qty)
                } else {
                    self.limit(books[hedge_leg], hedge_leg, hedge_side, hedge_qty)
                };
                limit.is_some_and(|limit| {
                    let reference = previous * hedge_qty;
                    let default_tol = self
                        .config
                        .hedge_protection_bps
                        .unwrap_or(self.config.latency_reserve_bps);
                    let tol_bps = if paired_close {
                        self.config
                            .closing_hedge_protection_bps
                            .unwrap_or(default_tol)
                    } else {
                        default_tol
                    };
                    !self.config.require_net_edge
                        || signed_cost(hedge_side, limit * hedge_qty, reference)
                            <= reference * tol_bps / BPS
                })
            }),
            None => self.passive_cancel_by(now).is_some(),
        };
        let feedback_cancel = book_frame
            && self.config.turnover_feedback.is_some()
            && !quote.closing
            && !self.resting_turnover_within_budget(&quote, now);
        let basis_reversion_cancel = book_frame
            && !quote.closing
            && self.config.basis_reversion.is_some_and(|p| {
                let quote_basis_bps = self.pair_books(now).map(|b| {
                    let hedge_mid = b[1 - self.maker()].mid();
                    (quote.price - hedge_mid) / hedge_mid * BPS
                });
                !self
                    .basis_reversion
                    .resting_quote_valid(p, quote.side, quote_basis_bps, now)
            });
        let entry_flow_cancel = book_frame
            && !quote.closing
            && self.config.entry_flow_filter.is_some_and(|p| {
                p.cancel_resting_entries && !self.entry_flow.permits_resting(p, quote.side, now)
            });
        let carry_drift_cancel = if !quote.closing
            && let Some(policy) = self.config.carry_drift_adjustment
        {
            self.carry_drift
                .adjustment_bps(policy, self.maker(), quote.side, now)
                .err()
        } else {
            None
        };
        let slow_cascade_cancel = book_frame
            && !quote.closing
            && self
                .slow_cascade
                .as_ref()
                .is_some_and(|sc| sc.scale_for(quote.side) <= 0.0);
        let kalman_basis_cancel = book_frame
            && !quote.closing
            && self
                .kalman_basis
                .as_ref()
                .is_some_and(|kb| kb.check_entry(quote.side).is_err());
        let lifetime_ms = self.quote_lifetime_ms(quote.closing);
        let cancel_reason = if expired {
            Some(if quote.expires_ms < quote.arrival_ms + lifetime_ms {
                "close_deadline"
            } else {
                "quote_ttl"
            })
        } else if !hedgeable {
            Some("hedge_protection")
        } else if feedback_cancel {
            Some("turnover_budget")
        } else if basis_reversion_cancel {
            Some("basis_dislocation")
        } else if entry_flow_cancel {
            if quote.cancelled_ms.is_none() {
                self.entry_flow.cancelled_resting_entries += 1;
            }
            Some("entry_flow_toxicity")
        } else if slow_cascade_cancel {
            if quote.cancelled_ms.is_none()
                && let Some(sc) = &mut self.slow_cascade
            {
                sc.cancelled_resting_entries += 1;
            }
            Some("slow_cascade_halt")
        } else if kalman_basis_cancel {
            if quote.cancelled_ms.is_none()
                && let Some(kb) = &mut self.kalman_basis
            {
                kb.cancelled_resting_entries += 1;
            }
            Some("kalman_basis_regime")
        } else {
            carry_drift_cancel
        };
        self.quote = Some(quote);
        if let Some(reason) = cancel_reason {
            self.cancel(now, reason);
        }
    }

    fn trade(
        &mut self,
        now: u64,
        source: u64,
        side: HedgeSide,
        price: Decimal,
        contracts: Decimal,
    ) -> Result<()> {
        let i = self.maker();
        self.report.trade_prints += 1;
        if now.saturating_sub(source) > self.config.trade_delivery_grace_ms {
            self.report.late_trade_prints += 1;
        }
        let midpoint = self.fresh(i, now).map(Book::mid);
        let reference_mids = [
            self.fresh(0, now).map(Book::mid),
            self.fresh(1, now).map(Book::mid),
        ];
        let Some(quote) = self.quote.as_mut() else {
            return Ok(());
        };
        if let Some(depth) = quote.queue_depth.as_mut() {
            ensure!(
                source <= now && now - source <= self.config.trade_delivery_grace_ms,
                "queue model requires bounded trade delivery"
            );
            if side != quote.side && signed_cost(quote.side, price, quote.price) <= Decimal::ZERO {
                let volume = contracts * self.config.pair.legs[i].base_per_contract;
                depth.trade(
                    source,
                    volume,
                    now,
                    self.config.trade_clock_guard_ms,
                    self.config.pair.planner.max_quote_age_ms,
                );
            }
        }
        let guarded = source.saturating_sub(self.config.trade_clock_guard_ms);
        if quote.activated_ms.is_none_or(|at| at >= guarded)
            || source > now
            || quote.side == side
            || signed_cost(quote.side, price, quote.price) > Decimal::ZERO
        {
            return Ok(());
        }
        let strict_excluded = quote
            .cancelled_ms
            .is_some_and(|at| source.saturating_add(self.config.trade_clock_guard_ms) >= at);
        let conservative_excluded = quote.cancelled_ms.is_some_and(|at| {
            guarded > at
                || now > at + self.config.trade_delivery_grace_ms + self.config.trade_clock_guard_ms
        });
        let overlap = strict_excluded && !conservative_excluded;
        self.report.cancel_guard_overlap_prints += usize::from(overlap);
        if if self.config.conservative_cancel_guard {
            conservative_excluded
        } else {
            strict_excluded
        } {
            return Ok(());
        }
        self.report.maker_prints_touching_quote += 1;
        if quote.closing && !quote.closing_touched {
            quote.closing_touched = true;
            self.report.cycle_attribution.closing_quotes.touched += 1;
        }
        let terms = &self.config.pair.legs[i];
        let mut volume = contracts * terms.base_per_contract;
        if price == quote.price {
            let used = volume.min(quote.ahead);
            quote.ahead -= used;
            volume -= used;
        } else {
            // A through print has passed every better displayed price.
            quote.ahead = Decimal::ZERO;
        }
        let base = floor_step(volume.min(quote.left), terms.base_step())?;
        if base.is_zero() {
            return Ok(());
        }
        let side = quote.side;
        let closing = quote.closing;
        let passive_side = quote.passive_entry.then_some(side_index(side));
        let posture = quote.posture;
        let offsets_inventory = quote.hedge_limit.is_none();
        if offsets_inventory || (closing && self.config.hedged_exit.is_some()) {
            let position = self.ledger.positions[i];
            ensure!(
                !position.is_zero() && side == direction(position) && base <= position.abs(),
                "maker close would increase confirmed inventory"
            );
        }
        if offsets_inventory {
            self.report.passive_offset_fills += 1;
        }
        if closing && !quote.closing_filled {
            quote.closing_filled = true;
            self.report.cycle_attribution.closing_quotes.filled += 1;
        }
        let notional = base * quote.price;
        self.report.maker_fills_after_cancel_request += usize::from(quote.cancelled_ms.is_some());
        if overlap {
            self.report.cancel_guard_overlap_fills += 1;
            self.report.cancel_guard_overlap_turnover += notional;
        }
        quote.left -= base;
        if quote.left.is_zero() {
            self.quote = None;
        }
        let cycle = self.cycle.get_or_insert_with(|| {
            let id = self.next_cycle_id;
            self.next_cycle_id += 1;
            if let Some(costs) = &mut self.report.execution_costs {
                debug_assert_eq!(costs.cycles.len(), id);
                costs.begin(now, reference_mids);
            }
            if let Some(i) = passive_side {
                self.report.passive_cycles_started[i] += 1;
            }
            self.report.turnover_posture.cycles_started[posture.index()] += 1;
            Cycle {
                id,
                posture,
                started_ms: now,
                initial_cash: self.ledger.cash,
                initial_turnover: self.ledger.turnover,
                initial_fees: self.ledger.fees,
                draining: posture != self.posture.posture,
                externalizing: false,
                maker_turnover: Decimal::ZERO,
                passive_side,
                entry_ref_mid: reference_mids[1 - i],
                paired_mids: reference_mids,
                paired_at_ms: None,
                hedge_fill_at_ms: None,
                paired_basis_bps: None,
                closing_maker_filled: false,
                forced_ioc_filled: false,
                sub_min_top_up_attempts: 0,
                sub_min_recovery_pending_base: Decimal::ZERO,
                used_partner_step_deadband: false,
                cash_exit_triggered: false,
            }
        });
        cycle.maker_turnover += notional;
        cycle.closing_maker_filled |= closing && !offsets_inventory;
        if !closing {
            cycle.hedge_fill_at_ms = None;
        }
        let cycle_id = cycle.id;
        self.report.maker_fills += 1;
        self.report.maker_buy_fills += usize::from(side == HedgeSide::Buy);
        self.report.maker_sell_fills += usize::from(side == HedgeSide::Sell);
        self.report.maker_turnover += notional;
        self.leg_turnover[i] += notional;
        self.record_eligible_fill(i, source, notional);
        if let Some(mid) = midpoint {
            self.report.maker_spread_capture -= signed_cost(side, notional, base * mid);
        }
        let fee = self
            .ledger
            .trade(i, side, Fill { base, notional }, self.config.maker_fee_bps);
        let signed_base = if side == HedgeSide::Buy { base } else { -base };
        self.record_funding_lifecycle_fill(i, Some(cycle_id), source, now, signed_base);
        self.report.maker_fees += fee;
        if let Some(costs) = &mut self.report.execution_costs {
            costs.fill(costs::CostFill {
                now,
                leg: i,
                maker_leg: i,
                phase: if closing {
                    costs::Phase::CloseMaker
                } else {
                    costs::Phase::EntryMaker
                },
                side,
                base,
                notional,
                fee,
                mid: midpoint,
                submit_touch: None,
            });
        }
        let attribution = &mut self.report.cycle_attribution;
        if closing {
            attribution.closing_maker_fees[i] += fee;
            attribution.closing_maker_turnover += notional;
            attribution.closing_maker_fills += 1;
        } else {
            attribution.opening_maker_fees[i] += fee;
            if let Some(policy) = self.config.entry_markout_gate {
                self.entry_markout.enqueue(
                    now.saturating_add(policy.horizon_ms),
                    side,
                    reference_mids[1 - i],
                );
            }
        }
        let signed_base = if side == HedgeSide::Buy { base } else { -base };
        if !closing {
            let hedge_side = direction(signed_base);
            let hedge_touch_at_fill = self
                .fresh(1 - i, now)
                .and_then(|book| book.side(hedge_side).first())
                .map(|level| level.price);
            if let Some(diag) = &mut self.entry_hedge_diag {
                diag.on_opening_fill(
                    now,
                    i,
                    hedge_side,
                    base,
                    notional,
                    hedge_touch_at_fill,
                    reference_mids[1 - i],
                );
            }
        }
        self.notices
            .push_back((now + self.config.fill_notice_ms, signed_base));
        if self.config.post_fill_book_refresh.is_some() {
            self.post_fill_refresh.arm(self.last_source_ms);
        }
        Ok(())
    }

    fn poll_ioc(&mut self, now: u64) -> Result<()> {
        let Some(order) = self.ioc.take() else {
            return Ok(());
        };
        let is_entry_hedge = !order.unwind && !order.closing_hedge;
        if now >= order.expires_ms {
            self.report.ioc_expired += 1;
            if is_entry_hedge
                && let Some(ep) = self
                    .entry_hedge_diag
                    .as_mut()
                    .and_then(|d| d.active.as_mut())
            {
                ep.ioc_expired += 1;
            }
            self.report.cycle_attribution.sub_min_top_up_failures +=
                usize::from(order.sub_min_top_up);
            return Ok(());
        }
        let Some(book) = self.fresh(order.leg, now).filter(|book| {
            book.timestamp_ms >= order.arrival_ms && book.timestamp_ms > self.consumed_ms[order.leg]
        }) else {
            self.ioc = Some(order);
            return Ok(());
        };
        let Some(touch) = book.side(order.side).first().map(|l| l.price) else {
            self.report.ioc_depth_misses += 1;
            if is_entry_hedge
                && let Some(ep) = self
                    .entry_hedge_diag
                    .as_mut()
                    .and_then(|d| d.active.as_mut())
            {
                ep.ioc_depth_misses += 1;
            }
            self.report.cycle_attribution.sub_min_top_up_failures +=
                usize::from(order.sub_min_top_up);
            return Ok(());
        };
        if !book.in_bounds(order.limit)
            || signed_cost(order.side, touch, order.limit) > Decimal::ZERO
        {
            self.report.ioc_price_misses += 1;
            if is_entry_hedge
                && let Some(ep) = self
                    .entry_hedge_diag
                    .as_mut()
                    .and_then(|d| d.active.as_mut())
            {
                ep.ioc_price_misses += 1;
            }
            self.report.cycle_attribution.sub_min_top_up_failures +=
                usize::from(order.sub_min_top_up);
            return Ok(());
        }
        if order.sub_min_top_up && !self.notices.is_empty() {
            self.report.cycle_attribution.sub_min_top_up_failures += 1;
            return Ok(());
        }
        let terms = &self.config.pair.legs[order.leg];
        let exposure = if order.unwind {
            self.ledger.positions[order.leg]
        } else {
            self.net()
        };
        if order.sub_min_top_up {
            let max_unhedged =
                Decimal::try_from(self.config.pair.planner.max_unhedged_notional_usd)
                    .unwrap_or(Decimal::ZERO);
            ensure!(
                self.config.allow_sub_minimum_top_up_unwind
                    && order.unwind
                    && self.notices.is_empty()
                    && self
                        .cycle
                        .as_ref()
                        .is_some_and(|c| c.sub_min_top_up_attempts == 1)
                    && self.ledger.positions[1 - order.leg].is_zero()
                    && !exposure.is_zero()
                    && order.side == direction(exposure)
                    && exposure.abs() < min_reduce_ioc_base(terms, touch.min(order.limit))
                    && order.quantity >= min_order_base(terms, order.limit, false)
                    && (order.quantity - exposure.abs()) * order.limit
                        <= self.rules.max_single_leg.min(max_unhedged),
                "maker-first sub-minimum top-up IOC violated dust-unwind bounds"
            );
        } else {
            ensure!(
                !exposure.is_zero()
                    && order.side == direction(exposure)
                    && order.quantity <= exposure.abs(),
                "maker-first IOC would increase confirmed exposure"
            );
        }
        if order.closing_hedge && self.config.hedged_exit.is_some() {
            let leg = self.ledger.positions[order.leg];
            ensure!(
                !leg.is_zero() && order.side == direction(leg) && order.quantity <= leg.abs(),
                "maker-first closing hedge would increase leg exposure"
            );
        }
        let fill = sweep(
            book,
            terms,
            order.side,
            order.limit,
            order.quantity,
            self.rules.participation,
        )?;
        if fill.base.is_zero() || (order.sub_min_top_up && fill.base < order.quantity) {
            self.report.ioc_depth_misses += 1;
            if is_entry_hedge
                && let Some(ep) = self
                    .entry_hedge_diag
                    .as_mut()
                    .and_then(|d| d.active.as_mut())
            {
                ep.ioc_depth_misses += 1;
            }
            self.report.cycle_attribution.sub_min_top_up_failures +=
                usize::from(order.sub_min_top_up);
            return Ok(());
        }
        let fill_time = book.timestamp_ms;
        let fill_mid = book.mid();
        self.consumed_ms[order.leg] = fill_time;
        if self.config.post_fill_book_refresh.is_some() {
            self.post_fill_refresh.arm(self.last_source_ms);
        }
        self.report.ioc_fills += 1;
        self.report.ioc_partial += usize::from(fill.base < order.quantity);
        if is_entry_hedge
            && let Some(ep) = self
                .entry_hedge_diag
                .as_mut()
                .and_then(|d| d.active.as_mut())
        {
            ep.ioc_fills += 1;
            ep.ioc_partial += usize::from(fill.base < order.quantity);
            if ep.first_hedge_fill_ms.is_none() {
                ep.first_hedge_fill_ms = Some(now);
                ep.first_hedge_book_ms = Some(fill_time);
            }
            ep.entry_hedge_base += fill.base;
            ep.entry_hedge_notional += fill.notional;
        }
        self.report.ioc_slippage += signed_cost(order.side, fill.notional, fill.base * order.touch);
        self.report.ioc_submit_touch_turnover += fill.base * order.touch;
        self.report.ioc_depth_impact += signed_cost(order.side, fill.notional, fill.base * touch);
        self.report.taker_turnover += fill.notional;
        self.leg_turnover[order.leg] += fill.notional;
        let notional = fill.notional;
        let fill_base = fill.base;
        let leg_before = self.ledger.positions[order.leg];
        let mut recovery_notional = Decimal::ZERO;
        if order.sub_min_top_up {
            let cycle = self
                .cycle
                .as_mut()
                .expect("sub-minimum top-up has an active cycle");
            cycle.sub_min_recovery_pending_base = (fill_base - leg_before.abs()).max(Decimal::ZERO);
            recovery_notional = notional;
        } else if order.unwind
            && let Some(cycle) = self
                .cycle
                .as_mut()
                .filter(|c| c.sub_min_recovery_pending_base > Decimal::ZERO)
        {
            let rec_base = fill_base.min(cycle.sub_min_recovery_pending_base);
            cycle.sub_min_recovery_pending_base -= rec_base;
            recovery_notional = notional * rec_base / fill_base;
            let non_rec = (notional - recovery_notional).max(Decimal::ZERO);
            if non_rec > Decimal::ZERO {
                self.record_eligible_fill(order.leg, fill_time, non_rec);
            }
        } else {
            self.record_eligible_fill(order.leg, fill_time, notional);
        }
        let remainder = if order.sub_min_top_up {
            Decimal::ZERO
        } else {
            leg_before.abs() - fill_base
        };
        let dust = !order.sub_min_top_up
            && self.config.hedged_exit.is_some()
            && (order.unwind || order.closing_hedge)
            && remainder > Decimal::ZERO
            && remainder < min_reduce_ioc_base(terms, touch.min(order.limit));
        let fee = self
            .ledger
            .trade(order.leg, order.side, fill, terms.taker_fee_bps);
        let signed_ioc_base = if order.side == HedgeSide::Buy {
            fill_base
        } else {
            -fill_base
        };
        let active_cycle_id = self.cycle.as_ref().map(|c| c.id);
        self.record_funding_lifecycle_fill(
            order.leg,
            active_cycle_id,
            fill_time,
            now,
            signed_ioc_base,
        );
        if let Some(costs) = &mut self.report.execution_costs {
            costs.fill(costs::CostFill {
                now,
                leg: order.leg,
                maker_leg: self.config.maker_leg,
                phase: if order.unwind {
                    costs::Phase::ForcedClose
                } else if order.closing_hedge {
                    costs::Phase::CloseHedge
                } else {
                    costs::Phase::EntryHedge
                },
                side: order.side,
                base: fill_base,
                notional,
                fee,
                mid: Some(fill_mid),
                submit_touch: Some(order.touch),
            });
        }
        let attribution = &mut self.report.cycle_attribution;
        attribution.reduce_ioc_dust_fills += usize::from(dust);
        attribution.sub_min_top_up_unwinds += usize::from(order.sub_min_top_up);
        attribution.sub_min_recovery_turnover[order.leg] += recovery_notional;
        if order.unwind {
            self.cycle
                .as_mut()
                .expect("unwind has an active cycle")
                .forced_ioc_filled = true;
            attribution.forced_ioc_fees[order.leg] += fee;
            attribution.forced_ioc_turnover[order.leg] += notional;
            attribution.forced_ioc_slippage[order.leg] +=
                signed_cost(order.side, notional, fill_base * order.touch);
        } else if order.closing_hedge {
            attribution.closing_hedge_fees[order.leg] += fee;
        } else {
            attribution.opening_hedge_fees[order.leg] += fee;
        }
        if !order.unwind
            && !order.closing_hedge
            && (self.net().is_zero() || self.within_partner_hedge_deadband(now))
            && !self.ledger.is_flat()
        {
            self.cycle
                .as_mut()
                .expect("opening hedge has an active cycle")
                .hedge_fill_at_ms = Some(now);
            if let Some(diag) = &mut self.entry_hedge_diag {
                let outcome = if self.ledger.positions[0] == -self.ledger.positions[1] {
                    "hedged"
                } else {
                    "hedged_within_deadband"
                };
                diag.close_active(now, outcome);
            }
        }
        Ok(())
    }

    fn record_eligible_fill(&mut self, leg: usize, time: u64, notional: Decimal) {
        let is_target_leg = self
            .config
            .pair
            .mode
            .incentive()
            .is_some_and(|policy| policy.eligible_leg == leg);
        if self.config.turnover_feedback.is_some() && is_target_leg {
            self.cost_feedback.target_turnover += notional;
        }
        if self
            .config
            .pair
            .mode
            .incentive()
            .is_some_and(|policy| policy.eligible_leg == leg && policy.active_at(time))
        {
            self.ledger.cycle_eligible_turnover += notional;
        }
    }

    fn ioc_rules(&self, is_entry_hedge: bool) -> Rules {
        self.config
            .hedge_ioc_policy
            .filter(|policy| is_entry_hedge || !policy.entry_only)
            .map_or(self.rules, |policy| policy.apply_to_rules(self.rules))
    }

    fn hedge_retry_ms(&self, is_entry_hedge: bool) -> u64 {
        self.config
            .hedge_ioc_policy
            .filter(|policy| is_entry_hedge || !policy.entry_only)
            .and_then(|policy| policy.retry_ms)
            .unwrap_or(self.config.reprice_ms)
    }

    fn next_ioc_ready_ms(&self, is_entry_hedge: bool) -> u64 {
        if is_entry_hedge
            && self
                .config
                .hedge_ioc_policy
                .is_some_and(|policy| policy.entry_only)
        {
            self.next_entry_ioc_ms
        } else {
            self.next_ioc_ms
        }
    }

    fn record_ioc_pacing(&mut self, now: u64, is_entry_hedge: bool) {
        if self
            .config
            .hedge_ioc_policy
            .is_some_and(|policy| policy.entry_only)
        {
            // Keep entry retry pacing isolated from closing/forced-drain pacing (`S16-1`) while
            // still enforcing conservative `reprice_ms` spacing from every IOC submission.
            self.next_entry_ioc_ms = now + self.hedge_retry_ms(true);
            self.next_ioc_ms = now + self.config.reprice_ms;
        } else {
            let next = now + self.hedge_retry_ms(is_entry_hedge);
            self.next_ioc_ms = next;
            self.next_entry_ioc_ms = next;
        }
    }

    fn submit_ioc(&mut self, now: u64, i: usize, position: Decimal, unwind: bool) -> Result<()> {
        let closing_hedge = !unwind && self.cycle.as_ref().is_some_and(|c| c.closing_maker_filled);
        let is_entry_hedge = !unwind && !closing_hedge;
        if self.ioc.is_some() || position.is_zero() {
            return Ok(());
        }
        if now < self.next_ioc_ready_ms(is_entry_hedge) {
            if is_entry_hedge
                && let Some(ep) = self
                    .entry_hedge_diag
                    .as_mut()
                    .and_then(|d| d.active.as_mut())
            {
                ep.reprice_throttle_waits += 1;
            }
            return Ok(());
        }
        self.record_ioc_pacing(now, is_entry_hedge);
        let Some(book) = self.fresh(i, now) else {
            if is_entry_hedge
                && let Some(ep) = self
                    .entry_hedge_diag
                    .as_mut()
                    .and_then(|d| d.active.as_mut())
            {
                ep.stale_book_waits += 1;
            }
            return Ok(());
        };
        let available_notional = book.available_notional;
        let terms = &self.config.pair.legs[i];
        let side = direction(position);
        let ioc_rules = self.ioc_rules(is_entry_hedge);
        let curve = Depth::build(book, terms, side, ioc_rules);
        let Ok(last) = curve.last() else {
            if is_entry_hedge
                && let Some(ep) = self
                    .entry_hedge_diag
                    .as_mut()
                    .and_then(|d| d.active.as_mut())
            {
                ep.depth_unavailable_waits += 1;
            }
            return Ok(());
        };
        let mut target = position.abs();
        if closing_hedge && self.config.hedged_exit.is_some() {
            let leg = self.ledger.positions[i];
            if leg.is_zero() || direction(leg) != side {
                return Ok(());
            }
            target = target.min(leg.abs());
        }
        let mut quantity = floor_step(target.min(last.base), terms.base_step())?;
        let leg = self.ledger.positions[i];
        let reduces_leg = !leg.is_zero() && direction(leg) == side && quantity <= leg.abs();
        let Some(touch) = book.side(side).first().map(|l| l.price) else {
            if is_entry_hedge
                && let Some(ep) = self
                    .entry_hedge_diag
                    .as_mut()
                    .and_then(|d| d.active.as_mut())
            {
                ep.depth_unavailable_waits += 1;
            }
            return Ok(());
        };
        let mut dust_avoided = false;
        let recovering_top_up = unwind
            && self
                .cycle
                .as_ref()
                .is_some_and(|c| c.sub_min_recovery_pending_base > Decimal::ZERO);
        if reduces_leg
            && ((self.config.hedged_exit.is_some() && (unwind || closing_hedge))
                || recovering_top_up)
        {
            let adjusted =
                Self::retain_tradable_remainder(terms, &curve, touch, leg.abs(), quantity)?;
            dust_avoided = adjusted < quantity;
            quantity = adjusted;
        }
        let Ok(mut limit) = curve.limit(quantity) else {
            if is_entry_hedge
                && let Some(ep) = self
                    .entry_hedge_diag
                    .as_mut()
                    .and_then(|d| d.active.as_mut())
            {
                ep.depth_unavailable_waits += 1;
            }
            return Ok(());
        };
        let mut min_ioc_base = if reduces_leg {
            min_reduce_ioc_base(terms, limit)
        } else {
            min_order_base(terms, limit, false)
        };
        let mut sub_min_top_up = false;
        if unwind
            && self.config.allow_sub_minimum_top_up_unwind
            && self.notices.is_empty()
            && self
                .cycle
                .as_ref()
                .is_some_and(|c| c.sub_min_top_up_attempts < 1)
            && reduces_leg
            && self.ledger.positions[1 - i].is_zero()
            && leg.abs() < min_ioc_base
        {
            // Budgeted 1-attempt-per-cycle two-step FOK top-up + reduce-only IOC flatten.
            // Count the attempt immediately so depth misses, expirations, or tight
            // settlement deadlines consume the budget rather than retrying every tick.
            self.cycle
                .as_mut()
                .expect("sub-minimum top-up has an active cycle")
                .sub_min_top_up_attempts += 1;
            self.report.cycle_attribution.sub_min_top_up_attempts += 1;

            let leg1_arrival_ms = now + self.config.transport_ms + self.config.taker_delay_ms[i];
            let leg1_expires_ms =
                (leg1_arrival_ms + self.config.pair.execution_window_ms).min(self.config.end_ms);
            let leg2_submit_ms = leg1_arrival_ms.max(now + self.hedge_retry_ms(false));
            let leg2_arrival_ms =
                leg2_submit_ms + self.config.transport_ms + self.config.taker_delay_ms[i];
            let flat_deadline_ms = self
                .funding
                .as_ref()
                .and_then(|f| f.next_flat_deadline(now))
                .map_or(self.config.end_ms, |d| d.min(self.config.end_ms));

            let worst_price = last.limit.max(touch);
            let min_entry = min_order_base(terms, worst_price, false);
            let top_up_qty = floor_step(min_entry + leg.abs(), terms.base_step())?;
            let max_unhedged =
                Decimal::try_from(self.config.pair.planner.max_unhedged_notional_usd)
                    .unwrap_or(Decimal::ZERO);
            if leg1_arrival_ms < leg1_expires_ms
                && leg2_arrival_ms < flat_deadline_ms
                && top_up_qty >= min_entry + leg.abs()
                && top_up_qty <= last.base
                && let Ok(top_limit) = curve.limit(top_up_qty)
                && top_up_qty >= min_order_base(terms, top_limit, false)
                && top_up_qty * top_limit <= available_notional.min(self.rules.max_single_leg)
                && (top_up_qty - leg.abs()) * top_limit
                    <= self.rules.max_single_leg.min(max_unhedged)
            {
                quantity = top_up_qty;
                limit = top_limit;
                min_ioc_base = min_order_base(terms, top_limit, false);
                sub_min_top_up = true;
            } else {
                self.report.cycle_attribution.sub_min_top_up_failures += 1;
            }
        }
        if quantity.is_zero() || quantity < min_ioc_base {
            if unwind {
                self.report.below_minimum_unwind += 1;
            } else {
                self.report.below_minimum_hedge += 1;
                if is_entry_hedge
                    && let Some(ep) = self
                        .entry_hedge_diag
                        .as_mut()
                        .and_then(|d| d.active.as_mut())
                {
                    ep.below_minimum_waits += 1;
                }
            }
            self.report.cycle_attribution.reduce_ioc_dust_avoided += usize::from(dust_avoided);
            return Ok(());
        }
        let arrival_ms = now + self.config.transport_ms + self.config.taker_delay_ms[i];
        let expires_ms =
            (arrival_ms + self.config.pair.execution_window_ms).min(self.config.end_ms);
        if arrival_ms >= expires_ms {
            return Ok(());
        }
        self.ioc = Some(Ioc {
            leg: i,
            side,
            quantity,
            limit,
            touch,
            arrival_ms,
            expires_ms,
            unwind,
            closing_hedge,
            sub_min_top_up,
        });
        if is_entry_hedge
            && let Some(ep) = self
                .entry_hedge_diag
                .as_mut()
                .and_then(|d| d.active.as_mut())
        {
            ep.ioc_submitted += 1;
            if ep.first_ioc_submit_ms.is_none() {
                ep.first_ioc_submit_ms = Some(now);
                ep.first_ioc_arrival_ms = Some(arrival_ms);
                ep.first_ioc_submit_touch = Some(touch);
                ep.first_ioc_limit = Some(limit);
            }
        }
        self.report.cycle_attribution.reduce_ioc_dust_avoided += usize::from(dust_avoided);
        self.report.ioc_submitted += 1;
        Ok(())
    }

    fn retain_tradable_remainder(
        terms: &MarketTerms,
        curve: &Depth,
        touch: Decimal,
        position: Decimal,
        quantity: Decimal,
    ) -> Result<Decimal> {
        if quantity >= position || quantity.is_zero() {
            return Ok(quantity);
        }
        // Use the protected depth for the remaining order. The touch is a
        // conservative price for the notional floor of a buy to close.
        let price = curve.last().map_or(touch, |last| last.limit.min(touch));
        let minimum = min_reduce_ioc_base(terms, price);
        let ceiling = (position - minimum).max(Decimal::ZERO);
        floor_step(quantity.min(ceiling), terms.base_step())
    }

    fn limit(
        &self,
        book: &Book,
        leg: usize,
        side: HedgeSide,
        quantity: Decimal,
    ) -> Option<Decimal> {
        let terms = &self.config.pair.legs[leg];
        let limit = Depth::build(book, terms, side, self.rules)
            .limit(quantity)
            .ok()?;
        (quantity >= terms.min_base(limit)
            && quantity * limit <= book.available_notional.min(self.rules.max_single_leg))
        .then_some(limit)
    }

    fn reduce_limit(
        &self,
        book: &Book,
        leg: usize,
        side: HedgeSide,
        quantity: Decimal,
    ) -> Option<Decimal> {
        let terms = &self.config.pair.legs[leg];
        let position = self.ledger.positions[leg];
        if position.is_zero()
            || side != direction(position)
            || quantity > position.abs()
            || quantity % terms.base_step() != Decimal::ZERO
        {
            return None;
        }
        let limit = Depth::build(book, terms, side, self.rules)
            .limit(quantity)
            .ok()?;
        (quantity >= min_reduce_ioc_base(terms, limit)).then_some(limit)
    }

    fn settled_opening_ledger(&self) -> (Decimal, Decimal, Decimal) {
        let i = self.maker();
        match &self.cycle {
            Some(cycle)
                if !self.ledger.positions[i].is_zero()
                    && self.ledger.positions[1 - i].is_zero() =>
            {
                (
                    cycle.initial_turnover,
                    cycle.initial_fees,
                    cycle.initial_cash,
                )
            }
            _ => (self.ledger.turnover, self.ledger.fees, self.ledger.cash),
        }
    }

    fn sub_min_sunk(&self) -> SubMinSunk {
        let i = self.maker();
        let existing = self.ledger.positions[i].abs();
        match &self.cycle {
            Some(cycle) if !existing.is_zero() && self.ledger.positions[1 - i].is_zero() => {
                SubMinSunk {
                    base: existing,
                    cash: self.ledger.cash - cycle.initial_cash,
                    turnover: self.ledger.turnover - cycle.initial_turnover,
                    fees: self.ledger.fees - cycle.initial_fees,
                    eligible: self.ledger.cycle_eligible_turnover,
                }
            }
            _ => SubMinSunk::default(),
        }
    }

    fn worst_case_partial_liquidation_cash(&self, books: [&Book; 2]) -> Option<Decimal> {
        let i = self.maker();
        let pos = self.ledger.positions[i];
        let existing = pos.abs();
        if existing.is_zero() {
            return Some(self.ledger.cash);
        }
        let cycle = self.cycle.as_ref()?;
        let unwind_side = direction(pos);
        let unwind_px = Depth::build(books[i], &self.config.pair.legs[i], unwind_side, self.rules)
            .limit(existing)
            .ok()?;
        let sunk_cash = self.ledger.cash - cycle.initial_cash;
        let taker_fee = self.config.pair.legs[i].taker_fee_bps;
        Some(cycle.initial_cash + sunk_cash + cash(unwind_side, unwind_px * existing, taker_fee))
    }

    #[cfg(test)]
    fn candidate(
        &self,
        books: [&Book; 2],
        side: HedgeSide,
        closing: bool,
        level: &Level,
    ) -> Result<Option<Candidate>> {
        self.candidate_with_dislocation(books, side, closing, level, None, None, Decimal::ONE)
    }

    #[allow(clippy::too_many_arguments)]
    fn candidate_with_dislocation(
        &self,
        books: [&Book; 2],
        side: HedgeSide,
        closing: bool,
        level: &Level,
        dislocation_bps: Option<Decimal>,
        carry_adj_bps: Option<Decimal>,
        opening_scale: Decimal,
    ) -> Result<Option<Candidate>> {
        let i = self.maker();
        let j = 1 - i;
        let maker = &self.config.pair.legs[i];
        let hedge = &self.config.pair.legs[j];
        let passive = level.price;
        let mark = books[0].asks[0].price.max(books[1].asks[0].price);
        let quantity = if closing {
            self.ledger.positions[i].abs()
        } else {
            let scaled_notional = if opening_scale < Decimal::ONE {
                (self.config.quote_notional * opening_scale).min(self.rules.max_single_leg)
            } else {
                self.config.quote_notional.min(self.rules.max_single_leg)
            };
            let full_target = floor_step(
                (scaled_notional / mark).min(self.rules.max_base),
                self.common_step,
            )?;
            let existing = self.ledger.positions[i].abs();
            if existing > Decimal::ZERO {
                let rem = floor_step(
                    (full_target - existing).max(Decimal::ZERO),
                    maker.base_step(),
                )?;
                let min_entry = min_order_base(maker, passive, false);
                if rem >= min_entry {
                    rem
                } else if existing + min_entry <= self.rules.max_base
                    && (existing + min_entry) * mark <= self.rules.max_single_leg
                {
                    min_entry
                } else {
                    Decimal::ZERO
                }
            } else {
                full_target
            }
        };
        let hedged_exit = self.config.hedged_exit.filter(|_| closing);
        let min_size = min_order_base(maker, passive, hedged_exit.is_some());
        if quantity.is_zero() || quantity < min_size || !books[i].in_bounds(passive) {
            return Ok(None);
        }
        if !closing
            && self.config.turnover_feedback.is_some()
            && (self.ledger.positions[i].abs() + quantity) * passive
                > books[i].available_notional.min(self.rules.max_single_leg)
        {
            return Ok(None);
        }
        let hedge_eval_qty = if hedged_exit.is_some_and(|p| p.allow_partner_step_deadband) {
            floor_step(
                quantity.min(self.ledger.positions[j].abs()),
                hedge.base_step(),
            )?
        } else if closing {
            quantity
        } else {
            self.ledger.positions[i].abs() + quantity
        };
        let hedge_price = if hedged_exit.is_some() {
            self.reduce_limit(books[j], j, opposite(side), hedge_eval_qty)
        } else {
            self.limit(books[j], j, opposite(side), hedge_eval_qty)
        };
        let Some(hedge_price) = hedge_price else {
            return Ok(None);
        };
        let sunk = if closing {
            SubMinSunk::default()
        } else {
            self.sub_min_sunk()
        };
        let maker_cash = sunk.cash + cash(side, passive * quantity, self.config.maker_fee_bps);
        let hedge_cash_qty = if closing {
            hedge_eval_qty
        } else {
            sunk.base + quantity
        };
        let hedge_cash = cash(
            opposite(side),
            hedge_price * hedge_cash_qty,
            hedge.taker_fee_bps,
        );
        let cycle_edge_bps = self.cycle.as_ref().filter(|_| closing).map(|cycle| {
            let cycle_cash = (maker_cash + hedge_cash) + (self.ledger.cash - cycle.initial_cash);
            self.edge_bps(cycle_cash, quantity * mark)
        });
        let mut candidate = Candidate {
            edge_bps: Decimal::ZERO,
            raw_edge_bps_before_carry: None,
            cycle_edge_bps,
            passive_entry: false,
            queue_ahead: level.contracts,
            price: passive,
            quantity,
            side,
            hedge_limit: Some(hedge_price),
            turnover: None,
        };
        let (cash_edge, raw_cash_edge) = if hedged_exit.is_some() {
            // The paired hedge IOC is identical for both exits. Sunk cash
            // and that shared hedge cost cancel out of the comparison.
            let Some(ioc) = self.reduce_limit(books[i], i, side, quantity) else {
                return Ok(None);
            };
            (
                maker_cash - cash(side, ioc * quantity, maker.taker_fee_bps),
                None,
            )
        } else if let Some(cycle) = self.cycle.as_ref().filter(|_| closing) {
            (
                (maker_cash + hedge_cash) + (self.ledger.cash - cycle.initial_cash),
                None,
            )
        } else {
            let Some(edge) = self.opening_edge(
                books,
                &mut candidate,
                maker_cash,
                hedge_cash,
                dislocation_bps,
                carry_adj_bps,
                sunk,
            ) else {
                return Ok(None);
            };
            edge
        };
        let edge_qty = if closing {
            quantity
        } else {
            sunk.base + quantity
        };
        candidate.edge_bps = self.edge_bps(cash_edge, edge_qty * mark);
        candidate.raw_edge_bps_before_carry =
            raw_cash_edge.map(|raw| self.edge_bps(raw, edge_qty * mark));
        Ok(Some(candidate))
    }

    #[allow(clippy::too_many_arguments)]
    fn opening_edge(
        &self,
        books: [&Book; 2],
        candidate: &mut Candidate,
        maker_cash: Decimal,
        hedge_cash: Decimal,
        dislocation_bps: Option<Decimal>,
        carry_adj_bps: Option<Decimal>,
        sunk: SubMinSunk,
    ) -> Option<(Decimal, Option<Decimal>)> {
        let i = self.maker();
        let j = 1 - i;
        let maker = &self.config.pair.legs[i];
        let hedge = &self.config.pair.legs[j];
        let Candidate {
            side,
            quantity,
            price,
            ..
        } = *candidate;
        let total_qty = sunk.base + quantity;
        let reverse = books[i].side(side)[0].price;
        let maker_fee = self.config.maker_fee_bps;
        let reverse_cash = cash(opposite(side), reverse * total_qty, maker_fee);
        let passive_cash = maker_cash + reverse_cash;
        let offset = self
            .config
            .passive_offset
            .as_ref()
            .filter(|policy| total_qty * books[i].asks[0].price <= policy.max_net_notional);
        if let Some(min_pe) = offset.and_then(|policy| policy.min_passive_edge_bps) {
            let passive_edge_bps = self.edge_bps(passive_cash, total_qty * price);
            if passive_edge_bps < min_pe {
                return None;
            }
        }
        let probabilities = offset.and_then(|policy| policy.close_probability);
        let mode = self.config.pair.mode;
        let carry_cash = if mode.is_pair_hedge() {
            carry_adj_bps.map_or(Decimal::ZERO, |adj| {
                adj / BPS * (total_qty * books[j].mid())
            })
        } else {
            Decimal::ZERO
        };
        if offset.is_some() && probabilities.is_none() && mode.is_pair_hedge() {
            candidate.passive_entry = true;
            return Some((
                passive_cash + carry_cash,
                carry_adj_bps.map(|_| passive_cash),
            ));
        }
        let hedge_exit = self.limit(books[j], j, side, total_qty)?;
        let hedge_exit_cash = cash(side, hedge_exit * total_qty, hedge.taker_fee_bps);
        let paired_cash = (maker_cash + hedge_cash) + (reverse_cash + hedge_exit_cash);
        if probabilities.is_none() && mode.is_pair_hedge() {
            let reversion_cash = match (self.config.basis_reversion, dislocation_bps) {
                (Some(policy), Some(disloc)) => {
                    policy.reversion_fraction * disloc / BPS * (total_qty * books[j].mid())
                }
                _ => Decimal::ZERO,
            };
            let raw = paired_cash + reversion_cash;
            return Some((raw + carry_cash, carry_adj_bps.map(|_| raw)));
        }
        let maker_exit = self.limit(books[i], i, opposite(side), total_qty)?;
        let fallback_cash = maker_cash
            + hedge_cash
            + hedge_exit_cash
            + cash(opposite(side), maker_exit * total_qty, maker.taker_fee_bps);
        let mut edge = paired_cash;
        if let Some(p) = probabilities {
            // Lower success probability bounds the score when the passive
            // outcome pays more. Otherwise use its full adverse contribution.
            let improvement = passive_cash - fallback_cash;
            let expected = fallback_cash
                + p[side_index(side)] * improvement.max(Decimal::ZERO)
                + improvement.min(Decimal::ZERO);
            candidate.passive_entry = expected > paired_cash;
            edge = edge.max(expected);
        }
        if !mode.is_pair_hedge() {
            let hedge_round_trip = candidate.hedge_limit.expect("opening hedge") + hedge_exit;
            let hedge_turnover = total_qty * hedge_round_trip;
            let eligible = mode.incentive().map_or(Decimal::ZERO, |p| {
                if p.eligible_leg == i {
                    sunk.eligible + quantity * price + total_qty * reverse.min(maker_exit)
                } else if candidate.passive_entry {
                    Decimal::ZERO
                } else {
                    hedge_turnover
                }
            });
            let worst_cash = if candidate.passive_entry {
                paired_cash.min(fallback_cash).min(passive_cash)
            } else {
                paired_cash.min(fallback_cash)
            };
            let gross = sunk.turnover
                + quantity * price
                + total_qty * (reverse.max(maker_exit) + hedge_round_trip);
            let fees = sunk.fees
                + (quantity * price * maker_fee.max(Decimal::ZERO)
                    + total_qty
                        * ((reverse * maker_fee.max(Decimal::ZERO))
                            .max(maker_exit * maker.taker_fee_bps)
                            + hedge_round_trip * hedge.taker_fee_bps))
                    / BPS;
            candidate.turnover = Some(TurnoverProjection {
                gross,
                fees,
                fallback_cash: worst_cash,
                eligible,
            });
            return Some((edge, None));
        }
        Some((edge + carry_cash, carry_adj_bps.map(|_| edge)))
    }

    fn turnover_rank(
        &self,
        candidate: &Candidate,
        budget: Option<TurnoverBudget>,
        best_cost: &mut Option<Decimal>,
        now: u64,
    ) -> std::result::Result<Decimal, &'static str> {
        let Some(projection) = &candidate.turnover else {
            return Ok(candidate.edge_bps);
        };
        let budget = budget.expect("turnover mode has a budget");
        let latency_reserve = if let Some(policy) = self.config.basis_risk_reserve {
            self.basis_risk
                .value(policy, now)
                .ok_or("basis_risk_warmup_or_gap")?
        } else {
            self.config.latency_reserve_bps
        };
        let feedback = self.config.turnover_feedback;
        let overhead = feedback.map_or(Decimal::ZERO, |p| p.overhead(projection.gross));
        let loss = -projection.fallback_cash + latency_reserve / BPS * projection.gross + overhead;
        let reserve = feedback.map_or(Decimal::ZERO, |p| self.cost_feedback.reserve(p));
        let reserved_loss = loss + reserve * projection.eligible / BPS;
        if projection.gross > budget.turnover
            || projection.fees > budget.fees
            || budget
                .loss
                .is_some_and(|remaining| reserved_loss > remaining)
        {
            return Err("turnover_budget");
        }
        let Some(policy) = self.config.pair.mode.incentive() else {
            return Ok(candidate.edge_bps);
        };
        if projection.eligible <= Decimal::ZERO {
            return Err("turnover_cost");
        }
        let cost_bps = loss * BPS / projection.eligible;
        let value = best_cost.get_or_insert(cost_bps);
        *value = (*value).min(cost_bps);
        let (base_turnover, _, base_cash) = self.settled_opening_ledger();
        let cash = base_cash - feedback.map_or(Decimal::ZERO, |p| p.overhead(base_turnover));
        if !policy.within_cost(cash, self.ledger.eligible_turnover)
            || !policy.within_cost(-loss, projection.eligible)
        {
            return Err("turnover_cost");
        }
        if !policy.within_cost(-reserved_loss, projection.eligible) {
            return Err("turnover_feedback");
        }
        Ok(-cost_bps)
    }

    fn resting_turnover_within_budget(&self, quote: &Quote, now: u64) -> bool {
        let Some(books) = self.pair_books(now) else {
            return false;
        };
        let i = self.maker();
        let j = 1 - i;
        let side = quote.side;
        let existing = if quote.closing || !self.ledger.positions[j].is_zero() {
            Decimal::ZERO
        } else {
            self.ledger.positions[i].abs()
        };
        let quantity = existing + quote.left;
        let Some(hedge) = self.limit(books[j], j, opposite(side), quantity) else {
            return false;
        };
        if quantity * quote.price > books[i].available_notional.min(self.rules.max_single_leg)
            || !books[i].in_bounds(quote.price)
        {
            return false;
        }
        let sunk = if existing > Decimal::ZERO {
            self.sub_min_sunk()
        } else {
            SubMinSunk::default()
        };
        let maker_cash =
            sunk.cash + cash(side, quote.price * quote.left, self.config.maker_fee_bps);
        let hedge_cash = cash(
            opposite(side),
            hedge * quantity,
            self.config.pair.legs[j].taker_fee_bps,
        );
        let mut candidate = Candidate {
            edge_bps: Decimal::ZERO,
            raw_edge_bps_before_carry: None,
            cycle_edge_bps: None,
            passive_entry: false,
            queue_ahead: quote.ahead,
            price: quote.price,
            quantity: quote.left,
            side,
            hedge_limit: Some(hedge),
            turnover: None,
        };
        if self
            .opening_edge(
                books,
                &mut candidate,
                maker_cash,
                hedge_cash,
                None,
                None,
                sunk,
            )
            .is_none()
            || candidate.passive_entry != quote.passive_entry
        {
            return false;
        }
        let mode = self.config.pair.mode;
        let (base_turnover, base_fees, base_cash) = self.settled_opening_ledger();
        let overhead = self
            .config
            .turnover_feedback
            .map_or(Decimal::ZERO, |p| p.overhead(base_turnover));
        let budget = mode.remaining_budget(base_turnover, base_fees, base_cash - overhead);
        self.turnover_rank(&candidate, budget, &mut None, now)
            .is_ok()
    }

    fn classify_persistence_book(&mut self, now: u64) -> BookEvidence {
        if self.config.entry_edge_persistence.is_none() {
            return BookEvidence::None;
        }
        let legs =
            [self.fresh(0, now), self.fresh(1, now)].map(|book| book.map(fingerprint_leg_book));
        let pair_usable = self.feed_online && self.pair_books(now).is_some();
        self.entry_edge_persist
            .classify_fingerprints(legs, pair_usable)
    }

    #[cfg(test)]
    fn quote(&mut self, now: u64) -> Result<()> {
        self.evaluate_quote(now, BookEvidence::New)
    }

    fn evaluate_quote(&mut self, now: u64, book_evidence: BookEvidence) -> Result<()> {
        let sub_min_opening = self.sub_min_opening_active(now);
        let closing = self.cycle.is_some() && !sub_min_opening;
        let throttled = now < self.next_quote_ms;
        let active_opening_quote = self.quote.as_ref().is_some_and(|q| !q.closing);
        let observe_persistence_only = !closing
            && self.config.entry_edge_persistence.is_some()
            && self.ioc.is_none()
            && self.report.incomplete.is_none()
            && (throttled || active_opening_quote)
            && (self.quote.is_none() || active_opening_quote);
        if !observe_persistence_only
            && (self.quote.is_some()
                || self.ioc.is_some()
                || throttled
                || self.report.incomplete.is_some())
        {
            return Ok(());
        }
        if self.funding_paused(now) {
            if !closing {
                self.entry_edge_persist.reset_all();
            }
            return Ok(());
        }
        let record_stats = !observe_persistence_only;
        if !closing && self.config.trade_from_ms.is_some_and(|at| now < at) {
            return Ok(());
        }
        let horizon = self.config.entry_horizon_ms();
        if !closing
            && self
                .funding
                .as_ref()
                .and_then(|s| s.next_pause(now))
                .is_some_and(|at| now + horizon >= at)
        {
            self.entry_edge_persist.reset_all();
            if record_stats {
                self.report.blocked("funding_entry_horizon");
            }
            return Ok(());
        }
        if !closing && now + horizon >= self.config.end_ms {
            self.entry_edge_persist.reset_all();
            return Ok(());
        }
        if !closing && let Some(reason) = self.equity_entry_block_reason(now) {
            self.entry_edge_persist.reset_all();
            if record_stats {
                self.rwa_session.session_blocked_entries += 1;
                self.report.blocked(reason);
            }
            return Ok(());
        }
        if record_stats {
            self.report.entry_checks += usize::from(!closing);
        }
        if !self.feed_online {
            if !closing {
                self.entry_edge_persist.reset_all();
            }
            if record_stats {
                self.report.blocked("trade_feed");
            }
            return Ok(());
        }
        let Some(books) = self.pair_books(now) else {
            if !closing {
                self.entry_edge_persist.reset_all();
            }
            if record_stats {
                self.report.blocked("book_age_or_skew");
            }
            return Ok(());
        };
        if self.funding_lifecycle_blocks_quote(now) {
            if !closing {
                self.entry_edge_persist.reset_all();
            }
            if record_stats {
                if let Some(lifecycle) = &mut self.funding_lifecycle {
                    lifecycle.report.boundary_guard_blocks += 1;
                }
                self.report.blocked("funding_boundary_guard");
            }
            return Ok(());
        }
        if !closing
            && self.config.post_fill_book_refresh.is_some()
            && self.post_fill_refresh.is_locked()
        {
            self.entry_edge_persist.reset_all();
            if record_stats {
                self.post_fill_refresh.record_block();
                self.report.blocked("post_fill_book_refresh");
            }
            return Ok(());
        }
        let mode = self.config.pair.mode;
        let incentive = mode.incentive();
        let feedback = self.config.turnover_feedback.filter(|_| !closing);
        if !closing
            && incentive.is_some_and(|p| !p.active_at(now) || now + horizon >= p.eligible_until_ms)
        {
            self.entry_edge_persist.reset_all();
            if record_stats {
                self.report.blocked("turnover_eligibility");
            }
            return Ok(());
        }
        let (base_turnover, base_fees, base_cash) = self.settled_opening_ledger();
        let cash = base_cash - feedback.map_or(Decimal::ZERO, |p| p.overhead(base_turnover));
        let worst_partial_cash = if sub_min_opening {
            self.worst_case_partial_liquidation_cash(books)
                .map(|wc| wc - feedback.map_or(Decimal::ZERO, |p| p.overhead(self.ledger.turnover)))
        } else {
            None
        };
        if !closing
            && (mode.realized_loss_exhausted(cash)
                || worst_partial_cash.is_some_and(|wc| mode.realized_loss_exhausted(wc)))
        {
            self.entry_edge_persist.reset_all();
            if record_stats {
                self.report.blocked("turnover_loss_limit");
            }
            return Ok(());
        }
        let hedged_exit = self.config.hedged_exit.filter(|_| closing);
        let patient_target = hedged_exit.and_then(|policy| {
            policy.target_cycle_edge_bps.filter(|_| {
                self.cycle
                    .as_ref()
                    .and_then(|c| c.paired_at_ms)
                    .is_some_and(|at| now.saturating_sub(at) < policy.patience_ms)
            })
        });
        let budget = mode.remaining_budget(base_turnover, base_fees, cash);
        let mut best: Option<(Decimal, Decimal, Candidate, bool)> = None;
        let mut blocked = None;
        let mut best_edge = self.report.best_modeled_edge_bps;
        let mut best_cost = self.report.best_projected_turnover_cost_bps;
        let mut kalman_frame_block: Option<&'static str> = None;
        let mut kalman_side_blocks = 0usize;
        let mut slow_cascade_blocks = 0usize;
        let mut carry_observations: [Option<std::result::Result<Decimal, &'static str>>; 2] =
            [None; 2];
        for side in [HedgeSide::Buy, HedgeSide::Sell] {
            let permitted = if closing {
                side == direction(self.ledger.positions[self.maker()])
            } else if sub_min_opening {
                side == opposite(direction(self.ledger.positions[self.maker()]))
                    && self.posture.posture.permits(side)
                    && self.config.entry_side.is_none_or(|opening| side == opening)
            } else {
                self.posture.posture.permits(side)
                    && self.config.entry_side.is_none_or(|opening| side == opening)
            };
            if !permitted {
                continue;
            }
            if !closing && let Some(policy) = self.config.entry_flow_filter {
                let flow_res = if record_stats {
                    self.entry_flow.check_entry(policy, side, now)
                } else {
                    self.entry_flow.eval_entry(policy, side, now)
                };
                if let Err(reason) = flow_res {
                    blocked = Some(reason);
                    continue;
                }
            }
            if !closing && let Some(policy) = self.config.entry_book_filter {
                let i = self.maker();
                let j = 1 - i;
                let book_res = if record_stats {
                    self.entry_book.check_entry(
                        policy,
                        books[i],
                        &self.config.pair.legs[i],
                        books[j],
                        &self.config.pair.legs[j],
                        side,
                        self.config.quote_notional,
                        self.config.maker_fee_bps,
                    )
                } else {
                    self.entry_book.eval_entry(
                        policy,
                        books[i],
                        &self.config.pair.legs[i],
                        books[j],
                        &self.config.pair.legs[j],
                        side,
                        self.config.quote_notional,
                        self.config.maker_fee_bps,
                    )
                };
                if let Err(reason) = book_res {
                    blocked = Some(reason);
                    continue;
                }
            }
            if !closing
                && let Some(kb) = &self.kalman_basis
                && let Err(reason) = kb.check_entry(side)
            {
                if reason == "kalman_basis_side" {
                    kalman_side_blocks += 1;
                } else {
                    kalman_frame_block.get_or_insert(reason);
                }
                blocked = Some(reason);
                continue;
            }
            let (entry_scale, entry_scaled) = if !closing && let Some(sc) = &self.slow_cascade {
                let raw = sc.scale_for(side);
                let scaled_dec = if raw > 0.0 {
                    Decimal::from_f64_retain(raw)
                        .filter(|d| *d > Decimal::ZERO && *d <= Decimal::ONE)
                } else {
                    None
                };
                let Some(scaled_dec) = scaled_dec else {
                    slow_cascade_blocks += 1;
                    blocked = Some("slow_cascade_halt");
                    continue;
                };
                (scaled_dec, scaled_dec < Decimal::ONE)
            } else {
                (Decimal::ONE, false)
            };
            let mid_dislocation_bps = if !closing && let Some(policy) = self.config.basis_reversion
            {
                if policy.use_quote_level_dislocation {
                    let Some(mid_disloc) =
                        self.basis_reversion.mid_dislocation_bps(policy, side, now)
                    else {
                        blocked = Some("basis_reversion_warmup_or_gap");
                        continue;
                    };
                    Some(mid_disloc.max(Decimal::ZERO))
                } else {
                    match self
                        .basis_reversion
                        .favorable_dislocation_bps(policy, side, now)
                    {
                        Ok(disloc) => Some(disloc),
                        Err(reason) => {
                            blocked = Some(reason);
                            continue;
                        }
                    }
                }
            } else {
                None
            };
            let carry_adj_bps = if !closing && let Some(policy) = self.config.carry_drift_adjustment
            {
                match self
                    .carry_drift
                    .adjustment_bps(policy, self.maker(), side, now)
                {
                    Ok(adj) => {
                        carry_observations[side_index(side)] = Some(Ok(adj));
                        Some(adj)
                    }
                    Err(reason) => {
                        carry_observations[side_index(side)] = Some(Err(reason));
                        blocked = Some(reason);
                        continue;
                    }
                }
            } else {
                None
            };
            let levels = books[self.maker()].side(opposite(side));
            let hedge_mid = books[1 - self.maker()].mid();
            let touch = levels[0].price;
            let max_levels = if feedback.is_some() {
                MAX_FEEDBACK_LEVELS.max(self.config.opening_max_levels)
            } else if let Some(policy) = hedged_exit.filter(|_| patient_target.is_some()) {
                policy.closing_max_levels
            } else if closing {
                1
            } else {
                self.config.opening_max_levels
            };
            for level in levels.iter().take(max_levels) {
                let distance = (level.price - touch).abs() * BPS / touch;
                if feedback.is_some_and(|p| distance > p.max_quote_distance_bps) {
                    break;
                }
                if !closing
                    && let Some(policy) = self
                        .config
                        .basis_reversion
                        .filter(|p| p.use_quote_level_dislocation)
                {
                    let quote_basis_bps = (level.price - hedge_mid) / hedge_mid * BPS;
                    if let Err(reason) = self.basis_reversion.favorable_quote_dislocation_bps(
                        policy,
                        side,
                        Some(quote_basis_bps),
                        now,
                    ) {
                        blocked = Some(reason);
                        continue;
                    }
                }
                if !closing
                    && let Some(policy) = self.config.entry_markout_gate
                    && hedge_mid > Decimal::ZERO
                {
                    let quote_dist_bps = match side {
                        HedgeSide::Buy => (hedge_mid - level.price) / hedge_mid * BPS,
                        HedgeSide::Sell => (level.price - hedge_mid) / hedge_mid * BPS,
                    };
                    let fee_term = if policy.include_hedge_taker_fee {
                        self.config.pair.legs[1 - self.maker()].taker_fee_bps
                    } else {
                        self.config.maker_fee_bps
                    };
                    if quote_dist_bps < self.entry_markout.required_distance_bps(policy, fee_term) {
                        if record_stats {
                            self.entry_markout.record_block();
                        }
                        blocked = Some("entry_markout_quote_distance");
                        continue;
                    }
                }
                let Some(candidate) = self.candidate_with_dislocation(
                    books,
                    side,
                    closing,
                    level,
                    mid_dislocation_bps,
                    carry_adj_bps,
                    entry_scale,
                )?
                else {
                    continue;
                };
                if !closing {
                    let value = best_edge.get_or_insert(candidate.edge_bps);
                    *value = (*value).max(candidate.edge_bps);
                }
                let rank = match self.turnover_rank(&candidate, budget, &mut best_cost, now) {
                    Ok(rank) => rank,
                    Err(reason) => {
                        blocked = Some(reason);
                        continue;
                    }
                };
                if hedged_exit.is_some_and(|p| candidate.edge_bps < p.min_saving_bps) {
                    blocked = Some("paired_maker_saving");
                    continue;
                }
                if let Some(target_bps) = patient_target
                    && candidate.cycle_edge_bps.is_none_or(|ce| ce < target_bps)
                {
                    blocked = Some("paired_target_edge");
                    continue;
                }
                if best
                    .as_ref()
                    .is_none_or(|(prior_distance, prior_rank, prior, _)| {
                        (distance, -rank, candidate.queue_ahead)
                            < (*prior_distance, -*prior_rank, prior.queue_ahead)
                    })
                {
                    best = Some((distance, rank, candidate, entry_scaled));
                }
                // The nearest passing level preserves price priority.
                break;
            }
        }
        if record_stats {
            if let Some(kb) = &mut self.kalman_basis {
                if let Some(reason) = kalman_frame_block {
                    kb.record_block(reason);
                }
                kb.blocked_side_mismatch += kalman_side_blocks;
            }
            if let Some(sc) = &mut self.slow_cascade {
                sc.blocked_opening_entries += slow_cascade_blocks;
            }
            for obs in carry_observations.into_iter().flatten() {
                match obs {
                    Ok(adj) => self.carry_drift.record_adjustment(adj),
                    Err("carry_rate_conflict") => self.carry_drift.record_conflict_block(),
                    Err(_) => self.carry_drift.record_stale_block(),
                }
            }
            self.report.best_modeled_edge_bps = best_edge;
            self.report.best_projected_turnover_cost_bps = best_cost;
            self.next_quote_ms = now + self.config.reprice_ms;
        }
        let Some((distance, _, best, best_scaled)) = best else {
            if !closing {
                self.entry_edge_persist.reset_all();
            }
            if record_stats {
                self.report
                    .blocked(blocked.unwrap_or("size_or_hedge_depth"));
            }
            return Ok(());
        };
        let ladder_extra = if !closing && let Some(ladder) = self.config.inventory_ladder {
            let projected =
                (self.ledger.positions[self.maker()].abs() + best.quantity) * best.price;
            let extra = ladder.extra_edge_bps(projected, true);
            if record_stats {
                self.inventory_ladder.record_eval(extra);
            }
            extra
        } else {
            Decimal::ZERO
        };
        let required_edge = self.rules.min_edge + ladder_extra;
        if self.config.require_net_edge
            && incentive.is_none()
            && hedged_exit.is_none()
            && best.edge_bps < required_edge
        {
            if !closing {
                self.entry_edge_persist.reset_all();
            }
            if record_stats {
                if !closing && best.edge_bps >= self.rules.min_edge && ladder_extra > Decimal::ZERO
                {
                    self.inventory_ladder.record_block();
                    self.report.blocked("inventory_ladder_edge");
                    return Ok(());
                }
                if !closing
                    && best
                        .raw_edge_bps_before_carry
                        .is_some_and(|raw| raw >= self.rules.min_edge)
                {
                    self.carry_drift.record_adverse_block();
                }
                self.report.blocked(if closing {
                    "exit_net_edge"
                } else {
                    "entry_net_edge"
                });
            }
            return Ok(());
        }
        if !closing && let Some(persist) = self.config.entry_edge_persistence {
            let new_book = book_evidence == BookEvidence::New;
            if observe_persistence_only {
                if new_book {
                    self.entry_edge_persist
                        .observe_passing(persist, best.side, now);
                }
                return Ok(());
            }
            if !self
                .entry_edge_persist
                .check_and_advance(persist, best.side, now, new_book)
            {
                self.report.blocked("entry_edge_persist");
                return Ok(());
            }
        }
        if observe_persistence_only {
            return Ok(());
        }
        if feedback.is_some() && distance > Decimal::ZERO {
            self.cost_feedback.outward_quotes += 1;
        }
        let cancel_by = hedged_exit.map_or(u64::MAX, |policy| {
            let cycle = self
                .cycle
                .as_ref()
                .expect("paired close has an active cycle");
            let stop = self
                .paired_deadline(cycle, policy)
                .expect("paired close has a stable opening hedge");
            stop.saturating_sub(
                self.config.transport_ms
                    + self.config.maker_cancel_delay_ms
                    + self.config.trade_delivery_grace_ms
                    + self.config.trade_clock_guard_ms,
            )
        });
        if now + self.config.maker_arrival_delay_ms() < cancel_by {
            if best_scaled && let Some(sc) = &mut self.slow_cascade {
                sc.scaled_opening_entries += 1;
            }
            self.post(now, best, cancel_by);
        }
        Ok(())
    }

    fn edge_bps(&self, cash_edge: Decimal, notional: Decimal) -> Decimal {
        (cash_edge * BPS / notional - self.config.latency_reserve_bps).round_dp(8)
    }

    fn post(&mut self, now: u64, candidate: Candidate, cancel_by: u64) {
        let arrival_ms = now + self.config.maker_arrival_delay_ms();
        let closing = self.cycle.is_some() && !self.sub_min_opening_active(now);
        let lifetime_ms = self.quote_lifetime_ms(closing);
        self.quote = Some(Quote {
            posture: self.posture.posture,
            side: candidate.side,
            price: candidate.price,
            left: candidate.quantity,
            arrival_ms,
            ahead: Decimal::ZERO,
            queue_depth: None,
            activated_ms: None,
            expires_ms: (arrival_ms + lifetime_ms).min(cancel_by),
            cancelled_ms: None,
            hedge_limit: candidate.hedge_limit,
            passive_entry: candidate.passive_entry,
            closing,
            closing_touched: false,
            closing_filled: false,
        });
        self.report.quotes_submitted += 1;
        self.report.cycle_attribution.closing_quotes.submitted += usize::from(closing);
    }

    fn quote_passive_offset(&mut self, now: u64, cancel_by: u64) {
        if self.quote.is_some()
            || self.ioc.is_some()
            || !self.notices.is_empty()
            || now < self.next_quote_ms
            || now + self.config.maker_arrival_delay_ms() >= cancel_by
            || self.report.incomplete.is_some()
        {
            return;
        }
        self.next_quote_ms = now + self.config.reprice_ms;
        let i = self.maker();
        let Some(book) = self.fresh(i, now) else {
            return;
        };
        let Some(policy) = self.config.passive_offset.as_ref() else {
            return;
        };
        let position = self.ledger.positions[i];
        let side = direction(position);
        let quantity = position.abs();
        let cycle = self.cycle.as_ref().expect("passive inventory cycle");
        let max_levels = policy.offset_max_levels;
        let passive_relaxed =
            policy.passive_loop_only && now >= cycle.started_ms + policy.max_wait_ms;
        let base_floor = policy.min_offset_edge_bps.unwrap_or_else(|| {
            if self.config.require_net_edge {
                self.rules.min_edge
            } else {
                -Decimal::from(100_000)
            }
        });
        let min_edge_floor = if passive_relaxed {
            base_floor.min(Decimal::ZERO)
        } else {
            base_floor
        };
        let mut selected = None;
        let mut saw_valid_level = false;
        for level in book.side(opposite(side)).iter().take(max_levels) {
            let price = level.price;
            if quantity < min_order_base(&self.config.pair.legs[i], price, true)
                || !book.in_bounds(price)
            {
                continue;
            }
            saw_valid_level = true;
            let round_trip_cash = self.ledger.cash - cycle.initial_cash
                + cash(side, quantity * price, self.config.maker_fee_bps);
            let edge_bps = self.edge_bps(round_trip_cash, quantity * price);
            if (self.config.require_net_edge || policy.min_offset_edge_bps.is_some())
                && edge_bps < min_edge_floor
            {
                continue;
            }
            selected = Some((price, edge_bps, level.contracts));
            break;
        }
        let Some((price, edge_bps, queue_ahead)) = selected else {
            if saw_valid_level {
                self.report.blocked("passive_offset_edge");
            }
            return;
        };
        self.post(
            now,
            Candidate {
                side,
                price,
                quantity,
                edge_bps,
                raw_edge_bps_before_carry: None,
                cycle_edge_bps: None,
                queue_ahead,
                hedge_limit: None,
                passive_entry: false,
                turnover: None,
            },
            cancel_by,
        );
    }

    fn paired_basis_bps(&self, now: u64) -> Option<Decimal> {
        let books = self.pair_books(now)?;
        let maker_mid = books[self.maker()].mid();
        let hedge_mid = books[1 - self.maker()].mid();
        let basis = (maker_mid - hedge_mid) / hedge_mid * BPS;
        Some(if self.ledger.positions[self.maker()] > Decimal::ZERO {
            basis
        } else {
            -basis
        })
    }

    fn paired_deadline(&self, cycle: &Cycle, policy: HedgedExit) -> Option<u64> {
        let start = cycle.paired_at_ms?;
        let anchor = if policy.use_cycle_deadline {
            cycle.started_ms
        } else {
            start
        };
        Some((anchor + policy.max_hold_ms).min(self.config.end_ms - self.config.drain_ms))
    }

    fn within_partner_hedge_deadband(&self, now: u64) -> bool {
        if !self
            .config
            .hedged_exit
            .is_some_and(|p| p.allow_partner_step_deadband)
            || !self.notices.is_empty()
            || self.ioc.is_some()
        {
            return false;
        }
        let [a, b] = self.ledger.positions;
        if a.is_zero() || b.is_zero() || (a > Decimal::ZERO) == (b > Decimal::ZERO) {
            return false;
        }
        let net = a + b;
        if net.is_zero() {
            return false;
        }
        let j = 1 - self.maker();
        let hedge_terms = &self.config.pair.legs[j];
        let hedge_pos = self.ledger.positions[j];
        let side = direction(net);
        let closing_hedge = self.cycle.as_ref().is_some_and(|c| c.closing_maker_filled);
        let target = if closing_hedge {
            net.abs().min(hedge_pos.abs())
        } else {
            net.abs()
        };
        let Ok(qty) = floor_step(target, hedge_terms.base_step()) else {
            return false;
        };
        let max_unhedged = Decimal::try_from(self.config.pair.planner.max_unhedged_notional_usd)
            .unwrap_or(Decimal::ZERO);
        if let Some(book) = self.fresh(self.maker(), now).or_else(|| self.fresh(j, now))
            && net.abs() * book.mid() > self.rules.max_single_leg.min(max_unhedged)
        {
            return false;
        }
        if qty.is_zero() {
            return true;
        }
        let reduces_leg =
            !hedge_pos.is_zero() && direction(hedge_pos) == side && qty <= hedge_pos.abs();
        self.fresh(j, now)
            .and_then(|book| book.side(side).first())
            .is_some_and(|top| {
                let min_ioc = if reduces_leg {
                    min_reduce_ioc_base(hedge_terms, top.price)
                } else {
                    min_order_base(hedge_terms, top.price, false)
                };
                qty < min_ioc
            })
    }

    fn observe_kalman_basis(
        &mut self,
        now: u64,
        reference: Option<Oracle>,
        equity_session: Option<EquitySession>,
        leg_oracles: Option<[Oracle; 2]>,
    ) {
        if self.kalman_basis.is_none() {
            return;
        }
        let obs = if self.feed_online
            && self.usable[0]
            && self.usable[1]
            && let Some(b) = self.pair_books(now)
        {
            let maker_book = b[self.maker()];
            let hedge_book = b[1 - self.maker()];
            let maker_mid = maker_book.mid();
            let hedge_mid = hedge_book.mid();
            if maker_mid > Decimal::ZERO && hedge_mid > Decimal::ZERO {
                ((maker_mid - hedge_mid) / hedge_mid * BPS)
                    .to_f64()
                    .map(|basis_bps| {
                        let maker_spread_bps =
                            ((maker_book.asks[0].price - maker_book.bids[0].price) / maker_mid
                                * BPS)
                                .to_f64()
                                .unwrap_or(f64::NAN);
                        let hedge_spread_bps =
                            ((hedge_book.asks[0].price - hedge_book.bids[0].price) / hedge_mid
                                * BPS)
                                .to_f64()
                                .unwrap_or(f64::NAN);
                        (
                            b.map(|book| book.timestamp_ms),
                            b.map(|book| [book.bids[0].price, book.asks[0].price]),
                            basis_bps,
                            maker_spread_bps + hedge_spread_bps,
                        )
                    })
            } else {
                None
            }
        } else {
            None
        };
        let regime = if self.config.pair.asset_class != AssetClass::Equity {
            SessionRegime::UsRth
        } else {
            let any_internal = reference
                .is_some_and(|r| matches!(r.mode, OracleMode::Internal | OracleMode::LastClose))
                || leg_oracles.is_some_and(|oracles| {
                    oracles
                        .iter()
                        .any(|o| matches!(o.mode, OracleMode::Internal | OracleMode::LastClose))
                });
            match equity_session {
                Some(s)
                    if !s.halted
                        && now >= s.observed_at_ms
                        && now < s.clear_until_ms
                        && !any_internal =>
                {
                    if now < s.opens_at_ms {
                        SessionRegime::PreMarket
                    } else if now < s.closes_at_ms {
                        SessionRegime::UsRth
                    } else {
                        SessionRegime::PostMarket
                    }
                }
                _ => SessionRegime::OffHours,
            }
        };
        if let Some(kb) = &mut self.kalman_basis {
            let Some((source_ms, touch, basis_bps, combined_spread_bps)) = obs else {
                kb.last_snapshot = None;
                return;
            };
            let observation = KalmanBookObservation {
                source_ms,
                touch,
                regime,
            };
            // Fixed leg markets, freshness, and per-leg timestamp order were
            // checked above. A repeated frame contains no new observation.
            if let Some(previous) = kb.last_observation
                && previous.source_ms == observation.source_ms
            {
                // Same-millisecond price ambiguity or a session transition needs
                // another source update. A cached replay cannot clear this block.
                if previous != observation {
                    kb.last_snapshot = None;
                }
                return;
            }
            kb.last_observation = Some(observation);
            kb.last_snapshot = Some(
                kb.filter
                    .observe(now, basis_bps, combined_spread_bps, regime),
            );
        }
    }

    fn admit_equity_session(&mut self, now: u64, observation: EquitySession) -> bool {
        if !self.config.enforce_equity_session_gate
            || self.config.pair.asset_class != AssetClass::Equity
        {
            self.last_equity_session = Some(observation);
            return true;
        }
        if observation.observed_at_ms == 0
            || observation.observed_at_ms > now
            || observation.opens_at_ms >= observation.closes_at_ms
            || observation.clear_until_ms <= observation.opens_at_ms
        {
            self.equity_session_uncertain = true;
            return false;
        }
        if let Some(prev) = self.last_equity_session {
            if observation.observed_at_ms < prev.observed_at_ms {
                return false;
            }
            if observation.observed_at_ms == prev.observed_at_ms {
                if observation.opens_at_ms != prev.opens_at_ms
                    || observation.closes_at_ms != prev.closes_at_ms
                    || observation.clear_until_ms != prev.clear_until_ms
                    || observation.halted != prev.halted
                {
                    self.equity_session_uncertain = true;
                }
                return !self.equity_session_uncertain;
            }
        }
        self.last_equity_session = Some(observation);
        self.equity_session_uncertain = false;
        true
    }

    fn observe_rwa_frame(
        &mut self,
        now: u64,
        reference: Option<Oracle>,
        equity_session: Option<EquitySession>,
        leg_oracles: Option<[Oracle; 2]>,
    ) -> Option<EquitySession> {
        if reference.is_some()
            || equity_session.is_some()
            || leg_oracles.is_some()
            || self.config.pair.asset_class == AssetClass::Equity
        {
            self.saw_rwa_metadata = true;
        }
        if let Some(r) = reference {
            self.last_reference = Some(r);
        }
        let received_session = equity_session.is_some();
        let equity_session = equity_session.filter(|s| self.admit_equity_session(now, *s));
        if !self.saw_rwa_metadata {
            return if self.equity_session_uncertain {
                None
            } else {
                equity_session
            };
        }
        let any_internal = reference
            .is_some_and(|r| matches!(r.mode, OracleMode::Internal | OracleMode::LastClose))
            || leg_oracles.is_some_and(|oracles| {
                oracles
                    .iter()
                    .any(|o| matches!(o.mode, OracleMode::Internal | OracleMode::LastClose))
            });
        match equity_session {
            Some(s) if s.halted || now < s.observed_at_ms || now >= s.clear_until_ms => {
                self.rwa_session.halted_or_uncovered_frames += 1;
            }
            Some(_) if any_internal => {
                self.rwa_session.off_hours_internal_or_close_frames += 1;
            }
            Some(s) if now >= s.opens_at_ms && now < s.closes_at_ms => {
                self.rwa_session.regular_cash_frames += 1;
            }
            Some(_) => {
                self.rwa_session.extended_external_frames += 1;
            }
            None if received_session => {
                self.rwa_session.halted_or_uncovered_frames += 1;
            }
            None if any_internal => {
                self.rwa_session.off_hours_internal_or_close_frames += 1;
            }
            None => {
                self.rwa_session.missing_session_metadata_frames += 1;
            }
        }
        if self.quote.as_ref().is_some_and(|q| !q.closing)
            && let Some(reason) = self.equity_entry_block_reason(now)
        {
            self.cancel(now, reason);
        }
        if self.equity_session_uncertain {
            None
        } else {
            equity_session
        }
    }

    fn equity_entry_block_reason(&self, now: u64) -> Option<&'static str> {
        if self.equity_session_uncertain {
            return Some("equity_halt_or_ca_boundary");
        }
        if let Some(session) = self.last_equity_session
            && (session.halted || now < session.observed_at_ms || now >= session.clear_until_ms)
        {
            return Some("equity_halt_or_ca_boundary");
        }
        if !self.config.enforce_equity_session_gate
            || self.config.pair.asset_class != AssetClass::Equity
        {
            return None;
        }
        let Some(session) = self.last_equity_session else {
            return Some("equity_missing_session");
        };
        let end_needed = now
            .saturating_add(self.config.entry_horizon_ms())
            .saturating_add(self.config.pair.equity_close_buffer_ms);
        if end_needed > session.clear_until_ms {
            return Some("equity_halt_or_ca_boundary");
        }
        if now < session.opens_at_ms || end_needed > session.closes_at_ms {
            return Some("equity_outside_session");
        }
        let Some(reference) = self.last_reference else {
            return Some("equity_missing_reference");
        };
        if reference.timestamp_ms > now {
            return Some("equity_stale_reference");
        }
        match reference.mode {
            OracleMode::LiveExternal => {
                if now - reference.timestamp_ms > self.config.pair.oracle_max_age_ms {
                    return Some("equity_stale_reference");
                }
            }
            OracleMode::EquityMinuteClose => {
                if self
                    .config
                    .pair
                    .equity_minute_reference_max_age_ms
                    .is_none_or(|max_age| now - reference.timestamp_ms > max_age)
                {
                    return Some("equity_stale_reference");
                }
            }
            OracleMode::Internal | OracleMode::LastClose => {
                return Some("equity_non_external_reference");
            }
        }
        None
    }

    fn update_paired_state(&mut self, now: u64) {
        let within_deadband = self.within_partner_hedge_deadband(now);
        let stable = !self.ledger.is_flat()
            && (self.net().is_zero() || within_deadband)
            && self.quote.is_none()
            && self.ioc.is_none()
            && self.notices.is_empty();
        if !stable {
            return;
        }
        let basis = self
            .config
            .hedged_exit
            .and_then(|_| self.paired_basis_bps(now));
        let fresh_pair_mids = self
            .pair_books(now)
            .map(|b| [Some(b[0].mid()), Some(b[1].mid())]);
        let cycle = self
            .cycle
            .as_mut()
            .expect("paired inventory has an active cycle");
        if self.config.hedged_exit.is_some()
            && cycle.paired_at_ms.is_none()
            && now >= cycle.started_ms + self.config.hold_ms
        {
            return;
        }
        if !cycle.draining {
            if cycle.paired_at_ms.is_none()
                && let Some(mids) = fresh_pair_mids
            {
                cycle.paired_mids = mids;
            }
            cycle.paired_at_ms.get_or_insert(now);
            cycle.used_partner_step_deadband |= within_deadband;
            if cycle.paired_basis_bps.is_none() {
                cycle.paired_basis_bps = basis;
            }
        }
    }

    fn hedged_single_leg_margin_or_drift_breached(
        &self,
        now: u64,
        cycle: &Cycle,
        policy: HedgedExit,
    ) -> bool {
        use crate::exchanges::lighter::risk::{
            DEFAULT_PRE_LIQUIDATION_SAFETY_MULTIPLE, MarginRegime, evaluate_margin_health,
        };

        let fresh_mids = [
            self.fresh(0, now).map(Book::mid),
            self.fresh(1, now).map(Book::mid),
        ];
        // When BOTH legs are simultaneously stale/empty, trigger immediately by default
        // (`max_hedged_both_blind_ms = None`) or whenever net exposure is nonzero (even inside
        // partner-step deadband dust). Strict net-zero balanced hedges may ride out a brief
        // simultaneous refresh up to `max_hedged_both_blind_ms` (`<= 5_000 ms`, `S12-2`/`S13-2`),
        // measured on the receipt clock since at least one leg was usable (`max(last_usable_at_ms)`),
        // while `paired_stop` continues enforcing `max_hedged_book_stale_ms` on both receipt and
        // source clocks per leg.
        if fresh_mids[0].is_none() && fresh_mids[1].is_none() {
            if !self.net().is_zero() || self.last_source_ms[0] == 0 || self.last_source_ms[1] == 0 {
                return true;
            }
            let last_any_usable = self.last_usable_at_ms[0]
                .zip(self.last_usable_at_ms[1])
                .map(|(a, b)| a.max(b));
            return policy.max_hedged_both_blind_ms.is_none_or(|grace_ms| {
                last_any_usable.is_none_or(|last_ok| now.saturating_sub(last_ok) > grace_ms)
            });
        }

        let max_adverse_bps = policy
            .max_hedged_leg_adverse_bps
            .unwrap_or_else(|| Decimal::from(150));
        let mmf = Decimal::new(2, 2); // 2.0% isolated maintenance margin baseline
        let close_out = Decimal::new(1, 2); // 1.0% close-out baseline

        for leg in [0usize, 1usize] {
            let pos = self.ledger.positions[leg];
            if pos.is_zero() {
                continue;
            }
            let Some(entry_mid) = cycle.paired_mids[leg] else {
                continue;
            };
            // Observe direct fresh mid on `leg`, or infer implied same-underlying mark from
            // the fresh companion leg (`1 - leg`) when `leg`'s own book is temporarily stale.
            let implied_mid = fresh_mids[leg].or_else(|| {
                let comp_now = fresh_mids[1 - leg]?;
                let comp_entry = cycle.paired_mids[1 - leg]?;
                (comp_entry > Decimal::ZERO).then(|| entry_mid * comp_now / comp_entry)
            });
            let Some(mark_mid) = implied_mid else {
                continue;
            };
            let adverse_bps = if pos > Decimal::ZERO {
                (entry_mid - mark_mid) / entry_mid * BPS
            } else {
                (mark_mid - entry_mid) / entry_mid * BPS
            };
            if adverse_bps > max_adverse_bps {
                return true;
            }
            if let Some(collateral) = policy.hedged_leg_collateral_quote
                && let Ok(health) = evaluate_margin_health(
                    collateral,
                    pos,
                    entry_mid,
                    mark_mid,
                    mmf,
                    close_out,
                    DEFAULT_PRE_LIQUIDATION_SAFETY_MULTIPLE,
                )
                && health.regime != MarginRegime::Healthy
            {
                return true;
            }
        }
        false
    }

    fn paired_stop(&self, now: u64) -> (bool, bool, bool) {
        let Some(policy) = self.config.hedged_exit else {
            return (false, false, false);
        };
        let Some(cycle) = self.cycle.as_ref() else {
            return (false, false, false);
        };
        let Some(deadline) = self.paired_deadline(cycle, policy) else {
            return (false, false, false);
        };
        let time = now >= deadline;
        let basis = self
            .paired_basis_bps(now)
            .zip(cycle.paired_basis_bps)
            .is_some_and(|(current, initial)| initial - current > policy.max_adverse_basis_bps);
        // Stale books withdraw the maker. Temporary inter-venue skew or brief
        // RTT-probe `tradable=false` blips alone cannot generate new quotes or a
        // basis observation. When configured on a balanced (`net == 0`) hedged
        // cycle, `max_hedged_book_stale_ms` avoids forcing an irreversible
        // double-IOC drain unless book usability/freshness exceeds `max_stale_ms`,
        // both legs go blind simultaneously, or single-leg valuation/margin breaches.
        let book = if let Some(max_stale_ms) = policy
            .max_hedged_book_stale_ms
            .filter(|_| self.net().is_zero() || self.within_partner_hedge_deadband(now))
        {
            self.hedged_single_leg_margin_or_drift_breached(now, cycle, policy)
                || [0, 1].into_iter().any(|i| {
                    self.last_usable_at_ms[i]
                        .is_none_or(|last_ok| now.saturating_sub(last_ok) > max_stale_ms)
                        || self.last_source_ms[i] == 0
                        || now.saturating_sub(self.last_source_ms[i]) > max_stale_ms
                })
        } else {
            self.fresh(0, now).is_none() || self.fresh(1, now).is_none()
        };
        (time, basis, book)
    }

    fn record_completed_cycle(&mut self, cycle: &Cycle, now: u64, pnl: Decimal) {
        if self.report.incomplete.is_some() || !self.notices.is_empty() {
            return;
        }
        if let Some(costs) = &mut self.report.execution_costs {
            let overhead = self.config.turnover_feedback.map_or(Decimal::ZERO, |p| {
                p.overhead(self.ledger.turnover - cycle.initial_turnover)
            });
            costs.close(now, pnl, overhead);
        }
        let stats = &mut self.report.cycle_attribution;
        if cycle.forced_ioc_filled {
            stats.forced_ioc_cycles += 1;
            stats.forced_ioc_cycle_pnl += pnl;
        }
        if cycle.paired_at_ms.is_some() {
            stats.paired_closed_cycles += 1;
            stats.partner_step_deadband_cycles += usize::from(cycle.used_partner_step_deadband);
            if cycle.closing_maker_filled && !cycle.forced_ioc_filled {
                stats.paired_maker_closes += 1;
                stats.paired_maker_close_pnl += pnl;
            }
        }
        if let Some(hedge_at) = cycle.hedge_fill_at_ms {
            stats.timed_paired_closed_cycles += 1;
            let hedge_ms = hedge_at - cycle.started_ms;
            let close_ms = now - hedge_at;
            stats.total_time_to_hedge_ms += hedge_ms;
            stats.max_time_to_hedge_ms = stats.max_time_to_hedge_ms.max(hedge_ms);
            stats.total_hedge_to_close_ms += close_ms;
            stats.max_hedge_to_close_ms = stats.max_hedge_to_close_ms.max(close_ms);
        }
    }

    fn can_sub_min_top_up_unwind_after_companion_flat(
        &self,
        now: u64,
        leg: usize,
        pos: Decimal,
    ) -> bool {
        if !self.config.allow_sub_minimum_top_up_unwind
            || !self.notices.is_empty()
            || self
                .cycle
                .as_ref()
                .is_none_or(|c| c.sub_min_top_up_attempts >= 1)
            || pos.is_zero()
        {
            return false;
        }
        let Some(book) = self.fresh(leg, now) else {
            return false;
        };
        let terms = &self.config.pair.legs[leg];
        let side = direction(pos);
        let curve = Depth::build(book, terms, side, self.rules);
        let Ok(last) = curve.last() else {
            return false;
        };
        let Some(touch) = book.side(side).first().map(|l| l.price) else {
            return false;
        };
        if pos.abs() >= min_reduce_ioc_base(terms, touch) {
            return false;
        }
        let flat_deadline_ms = self
            .funding
            .as_ref()
            .and_then(|f| f.next_flat_deadline(now))
            .map_or(self.config.end_ms, |d| d.min(self.config.end_ms));
        let needed_ms = 3 * self.config.transport_ms
            + self.config.taker_delay_ms[1 - leg]
            + 2 * self.config.taker_delay_ms[leg]
            + 2 * self.config.reprice_ms;
        if now.saturating_add(needed_ms) >= flat_deadline_ms {
            return false;
        }
        let min_entry = min_order_base(terms, last.limit.max(touch), false);
        let Ok(top_up_qty) = floor_step(min_entry + pos.abs(), terms.base_step()) else {
            return false;
        };
        let max_unhedged = Decimal::try_from(self.config.pair.planner.max_unhedged_notional_usd)
            .unwrap_or(Decimal::ZERO);
        top_up_qty >= min_entry + pos.abs()
            && top_up_qty <= last.base
            && curve.limit(top_up_qty).is_ok_and(|top_limit| {
                top_up_qty >= min_order_base(terms, top_limit, false)
                    && top_up_qty * top_limit
                        <= book.available_notional.min(self.rules.max_single_leg)
                    && (top_up_qty - pos.abs()) * top_limit
                        <= self.rules.max_single_leg.min(max_unhedged)
            })
    }

    fn paired_drain_quantity(&self, now: u64, leg: usize, requested: Decimal) -> Result<Decimal> {
        let full = self.ledger.positions[leg];
        let companion = self.ledger.positions[1 - leg];
        if full.is_zero() || companion.is_zero() {
            return Ok(requested);
        }
        let Some(book) = self.fresh(leg, now) else {
            return Ok(Decimal::ZERO);
        };
        let side = direction(full);
        let terms = &self.config.pair.legs[leg];
        if requested.abs() < full.abs() {
            let curve = Depth::build(book, terms, side, self.rules);
            if let Ok(last) = curve.last() {
                let quantity = floor_step(requested.abs().min(last.base), terms.base_step())?;
                let touch = book.side(side)[0].price;
                let safe =
                    Self::retain_tradable_remainder(terms, &curve, touch, full.abs(), quantity)?;
                if curve.limit(safe).is_ok_and(|limit| {
                    safe > Decimal::ZERO && safe >= min_reduce_ioc_base(terms, limit)
                }) {
                    return Ok(requested);
                }
            }
        }
        // Require executable full reductions before intentionally increasing
        // net exposure. A full close that reduces net can still be used to
        // contain an existing mismatch, with the remaining leg kept on book.
        let full_closeable = self.reduce_limit(book, leg, side, full.abs()).is_some();
        let companion_closeable = companion.abs() <= (full + companion).abs()
            || self.fresh(1 - leg, now).is_some_and(|book| {
                self.reduce_limit(book, 1 - leg, direction(companion), companion.abs())
                    .is_some()
            })
            || (self
                .config
                .hedged_exit
                .is_some_and(|p| p.allow_partner_step_deadband)
                && self.can_sub_min_top_up_unwind_after_companion_flat(now, 1 - leg, companion));
        Ok(if full_closeable && companion_closeable {
            full
        } else {
            Decimal::ZERO
        })
    }

    fn net_below_minimum(&self, now: u64) -> bool {
        let net = self.net();
        if net.is_zero() {
            return false;
        }
        let side = direction(net);
        [0, 1].into_iter().all(|leg| {
            let terms = &self.config.pair.legs[leg];
            let Ok(base) = floor_step(net.abs(), terms.base_step()) else {
                return false;
            };
            let min_contracts_base =
                (terms.min_contracts * terms.base_per_contract).max(terms.base_step());
            if base < min_contracts_base {
                return true;
            }
            // Never evaluate price-dependent `min_notional` against stale or empty
            // disconnect frames (`bids = [], asks = []`): unknown notional stays false.
            self.fresh(leg, now)
                .and_then(|book| book.side(side).first())
                .is_some_and(|top| base < min_order_base(terms, top.price, false))
        })
    }

    fn sub_min_opening_active(&self, now: u64) -> bool {
        let maker = self.maker();
        let pos = self.ledger.positions[maker].abs();
        if !self.config.aggregate_sub_minimum_fills
            || !self.notices.is_empty()
            || self.cycle.as_ref().is_none_or(|c| c.draining)
            || !self.ledger.positions[1 - maker].is_zero()
            || pos.is_zero()
            || !self.net_below_minimum(now)
        {
            return false;
        }
        let terms = &self.config.pair.legs[maker];
        let min_reduce = self
            .fresh(maker, now)
            .and_then(|book| {
                let side = direction(self.ledger.positions[maker]);
                book.side(side)
                    .first()
                    .map(|top| min_reduce_ioc_base(terms, top.price))
            })
            .unwrap_or_else(|| {
                (terms.min_contracts * terms.base_per_contract).max(terms.base_step())
            });
        pos < min_reduce
    }

    fn advance(&mut self, now: u64) -> Result<()> {
        self.advance_with_frame(now, BookEvidence::None)
    }

    fn advance_with_frame(&mut self, now: u64, book_evidence: BookEvidence) -> Result<()> {
        if let Some(policy) = self.config.entry_markout_gate {
            let hedge_mark = self
                .fresh(1 - self.maker(), now)
                .map(|book| (book.timestamp_ms, book.mid()));
            self.entry_markout.advance(now, hedge_mark, policy);
        }
        let funding_pause = self.funding_paused(now);
        if funding_pause {
            self.cancel(now, "funding_guard");
        }
        if self.funding_lifecycle_guard_active(now) {
            self.cancel(now, "funding_boundary_guard");
        }
        self.poll_ioc(now)?;
        let mut confirmed = false;
        while self.notices.front().is_some_and(|(at, _)| *at <= now) {
            self.notices.pop_front();
            confirmed = true;
        }
        if confirmed
            && let Some((hedge_leg, hedge_side)) = self
                .entry_hedge_diag
                .as_ref()
                .and_then(|d| d.active.as_ref())
                .filter(|ep| {
                    ep.first_notice_ms.is_none()
                        && now - ep.opening_fill_ms >= self.config.fill_notice_ms
                })
                .map(|ep| (ep.hedge_leg, ep.hedge_side))
        {
            let hedge_touch_at_notice = self
                .fresh(hedge_leg, now)
                .and_then(|book| book.side(hedge_side).first())
                .map(|level| level.price);
            if let Some(diag) = &mut self.entry_hedge_diag {
                diag.on_notice(now, hedge_touch_at_notice);
            }
        }
        let retain_sub_min_opening = self.config.aggregate_sub_minimum_fills
            && self
                .quote
                .as_ref()
                .is_some_and(|quote| !quote.closing && quote.cancelled_ms.is_none())
            && self.net_below_minimum(now);
        if confirmed
            && !retain_sub_min_opening
            && self.quote.as_ref().is_some_and(|quote| {
                quote.hedge_limit.is_some()
                    || self
                        .config
                        .hedged_exit
                        .is_some_and(|policy| policy.closing_quote_lifetime_ms.is_some())
            })
        {
            self.cancel(now, "fill_confirmed");
        }
        self.update_paired_state(now);
        let cash_exit = self.paired_cash_exit_value(now);
        if let Some(value) = cash_exit {
            let stats = self
                .report
                .paired_cash_exit
                .as_mut()
                .expect("cash exit requires an enabled report");
            stats.triggers += 1;
            stats.with_pending_maker_quote += usize::from(self.quote.is_some());
            stats.first_trigger_ms.get_or_insert(now);
            stats.quoted_stress_cash += value;
        }
        let (paired_time, paired_basis, paired_book) = self.paired_stop(now);
        let kalman_time_stop = self
            .cycle
            .as_ref()
            .and_then(|c| c.paired_at_ms)
            .zip(self.kalman_basis.as_ref())
            .is_some_and(|(paired_at, kb)| {
                kb.last_snapshot.is_some_and(|snap| {
                    kb.filter
                        .should_time_stop(&snap, now.saturating_sub(paired_at))
                })
            });
        if let Some(cycle) = &mut self.cycle {
            let exposure_stop = self
                .unhedged_since
                .is_some_and(|at| now >= at + self.config.unhedged_timeout_ms);
            if !cycle.draining {
                self.report.exposure_stops += usize::from(exposure_stop);
                self.report.cycle_attribution.hedged_time_stops += usize::from(paired_time);
                self.report.cycle_attribution.hedged_basis_stops += usize::from(paired_basis);
                self.report.cycle_attribution.hedged_book_stops += usize::from(paired_book);
                if kalman_time_stop && let Some(kb) = &mut self.kalman_basis {
                    kb.time_stop_triggers += 1;
                }
            }
            let opening_timeout = (self.config.hedged_exit.is_none()
                || cycle.paired_at_ms.is_none())
                && now >= cycle.started_ms + self.config.hold_ms;
            cycle.draining |= opening_timeout
                || cash_exit.is_some()
                || funding_pause
                || paired_time
                || paired_basis
                || paired_book
                || kalman_time_stop
                || now >= self.config.end_ms - self.config.drain_ms
                || self.report.incomplete.is_some()
                || exposure_stop;
            cycle.cash_exit_triggered |= cash_exit.is_some();
            if cycle.draining
                && let Some(ep) = self
                    .entry_hedge_diag
                    .as_mut()
                    .and_then(|d| d.active.as_mut())
            {
                // A drain request can precede an unwind fill by several book frames.
                ep.forced_drain_triggered_ms.get_or_insert(now);
            }
        }
        if self.cycle.as_ref().is_some_and(|cycle| cycle.draining) {
            self.cancel(now, "drain");
            if self.quote.is_none() && self.notices.is_empty() {
                let [a, b] = self.ledger.positions;
                let net = a + b;
                // Trim the unmatched size before flattening the balanced part.
                let (i, quantity) = if net.is_zero() {
                    (self.maker(), self.ledger.positions[self.maker()])
                } else if (a > Decimal::ZERO) == (net > Decimal::ZERO) && !a.is_zero() {
                    (0, net)
                } else {
                    (1, net)
                };
                if self.config.hedged_exit.is_some() {
                    if now >= self.next_ioc_ms && self.ioc.is_none() && !quantity.is_zero() {
                        let mut target_leg = i;
                        let mut close = self.paired_drain_quantity(now, i, quantity)?;
                        if close.is_zero()
                            && self
                                .config
                                .hedged_exit
                                .is_some_and(|p| p.allow_partner_step_deadband)
                        {
                            let alt = 1 - i;
                            let alt_pos = self.ledger.positions[alt];
                            if !alt_pos.is_zero() {
                                let alt_close = self.paired_drain_quantity(now, alt, alt_pos)?;
                                if !alt_close.is_zero() {
                                    target_leg = alt;
                                    close = alt_close;
                                }
                            }
                        }
                        if close.is_zero() {
                            self.report.cycle_attribution.paired_drain_blocked += 1;
                            self.record_ioc_pacing(now, false);
                        } else {
                            self.submit_ioc(now, target_leg, close, true)?;
                        }
                    }
                } else {
                    let full = self.ledger.positions[i];
                    let below_minimum = self
                        .fresh(i, now)
                        .and_then(|book| book.side(direction(quantity)).first())
                        .is_some_and(|top| {
                            let terms = &self.config.pair.legs[i];
                            floor_step(quantity.abs(), terms.base_step())
                                .is_ok_and(|base| base < min_reduce_ioc_base(terms, top.price))
                        });
                    // If the residual cannot be traded, a protected full close
                    // can still clear the leg when its companion remains tradable.
                    let companion = self.ledger.positions[1 - i];
                    let companion_closeable = self
                        .fresh(1 - i, now)
                        .and_then(|book| book.side(direction(companion)).first())
                        .is_some_and(|top| {
                            let terms = &self.config.pair.legs[1 - i];
                            companion.abs() >= min_reduce_ioc_base(terms, top.price)
                        });
                    let close =
                        if below_minimum && full.abs() > quantity.abs() && companion_closeable {
                            full
                        } else {
                            quantity
                        };
                    self.submit_ioc(now, i, close, true)?;
                }
            }
        } else if !self.net().is_zero() && !self.within_partner_hedge_deadband(now) {
            let awaiting_sub_min_opening = (self.sub_min_opening_active(now)
                && self.quote.as_ref().is_none_or(|quote| !quote.closing))
                || (self.config.aggregate_sub_minimum_fills
                    && self.quote.as_ref().is_some_and(|quote| !quote.closing)
                    && self.net_below_minimum(now));
            if !awaiting_sub_min_opening {
                if let Some(cancel_by) = self
                    .passive_cancel_by(now)
                    .filter(|_| !self.funding_lifecycle_blocks_quote(now))
                {
                    self.quote_passive_offset(now, cancel_by);
                } else {
                    self.externalize(now)?;
                }
            }
        }
        if self.ledger.is_flat()
            && self.quote.is_none()
            && self.ioc.is_none()
            && (self.notices.is_empty()
                || (self.config.passive_offset.is_none() && self.config.hedged_exit.is_none()))
            && let Some(cycle) = self.cycle.take()
        {
            let pnl = self.ledger.cash - cycle.initial_cash;
            if cycle.cash_exit_triggered {
                let policy = self.config.paired_cash_exit.expect("cash exit policy");
                let turnover = self.ledger.turnover - cycle.initial_turnover;
                let stats = self
                    .report
                    .paired_cash_exit
                    .as_mut()
                    .expect("cash exit report");
                stats.settled_cycles += 1;
                stats.realized_stress_cash +=
                    pnl - policy.cost(turnover, turnover - cycle.maker_turnover);
            }
            self.record_completed_cycle(&cycle, now, pnl);
            let recorded_in_attribution =
                self.report.incomplete.is_none() && self.notices.is_empty();
            self.closed_cycles_meta.insert(
                cycle.id,
                ClosedCycleSummary {
                    posture_idx: cycle.posture.index(),
                    draining: cycle.draining,
                    externalizing: cycle.externalizing,
                    paired_maker_close: cycle.paired_at_ms.is_some()
                        && cycle.closing_maker_filled
                        && !cycle.forced_ioc_filled,
                    forced_ioc_filled: cycle.forced_ioc_filled,
                    cash_exit_triggered: cycle.cash_exit_triggered,
                    recorded_in_attribution,
                },
            );
            let eligible = std::mem::take(&mut self.ledger.cycle_eligible_turnover);
            if let Some(policy) = self.config.turnover_feedback {
                if self.report.incomplete.is_none() && self.notices.is_empty() {
                    self.ledger.eligible_turnover += eligible;
                    let cost = policy.overhead(self.ledger.turnover - cycle.initial_turnover);
                    self.cost_feedback.observe(policy, pnl - cost, eligible);
                }
            } else {
                self.ledger.eligible_turnover += eligible;
            }
            self.report.closed_cycles += 1;
            if cycle.draining {
                self.report.forced_closes += 1;
                self.report.forced_close_pnl += pnl;
            } else if !cycle.externalizing {
                self.report.passive_only_cycles += 1;
                self.report.passive_only_pnl += pnl;
                self.report.passive_only_maker_turnover += cycle.maker_turnover;
                if let Some(side) = cycle.passive_side {
                    self.report.passive_cycles_completed[side] += 1;
                }
            }
            self.report.turnover_posture.cycles_completed[cycle.posture.index()] += 1;
            self.report.turnover_posture.realized_pnl[cycle.posture.index()] += pnl;
            self.report.realized_pnl += pnl;
        }
        let sub_min_opening = self.sub_min_opening_active(now);
        let closing_ready = sub_min_opening
            || self.config.hedged_exit.is_none()
            || self
                .cycle
                .as_ref()
                .is_none_or(|cycle| cycle.paired_at_ms.is_some());
        if (self.net().is_zero() || self.within_partner_hedge_deadband(now) || sub_min_opening)
            && closing_ready
            && (self.notices.is_empty()
                || (self.config.passive_offset.is_none() && self.config.hedged_exit.is_none()))
            && self.cycle.as_ref().is_none_or(|cycle| !cycle.draining)
        {
            self.evaluate_quote(now, book_evidence)?;
        }
        Ok(())
    }

    fn externalize(&mut self, now: u64) -> Result<()> {
        if let Some(cycle) = &mut self.cycle {
            cycle.externalizing = true;
        }
        if self
            .quote
            .as_ref()
            .is_some_and(|quote| quote.hedge_limit.is_none())
        {
            // The reduce quote can still fill until cancellation and its
            // delivery window are complete. Keep the hedge waiting.
            self.cancel(now, "externalize");
        } else {
            let pending: Decimal = self.notices.iter().map(|(_, quantity)| quantity).sum();
            let confirmed = self.net() - pending;
            self.submit_ioc(now, 1 - self.maker(), confirmed, false)?;
        }
        Ok(())
    }

    fn risk(&mut self, now: u64) {
        let [a, b] = self.ledger.positions;
        if !a.is_zero()
            && !b.is_zero()
            && a.is_sign_negative() != b.is_sign_negative()
            && let [Some(a_book), Some(b_book)] = [self.fresh(0, now), self.fresh(1, now)]
        {
            let gross = a.abs().min(b.abs()) * (a_book.mid() + b_book.mid());
            let stats = &mut self.report.cycle_attribution;
            stats.max_paired_gross_notional = stats.max_paired_gross_notional.max(gross);
        }
        let net = self.net().abs();
        if !net.is_zero()
            && let Some(book) = self
                .fresh(self.maker(), now)
                .or_else(|| self.fresh(1 - self.maker(), now))
        {
            self.report.max_net_notional = self.report.max_net_notional.max(net * book.mid());
        }
        if net.is_zero() || self.within_partner_hedge_deadband(now) {
            if let Some(diag) = &mut self.entry_hedge_diag
                && let Some(ep) = &diag.active
            {
                let outcome = if ep.forced_drain_triggered_ms.is_some() {
                    "forced_drain"
                } else if a.is_zero() && b.is_zero() {
                    "passive_offset_flat"
                } else {
                    "net_within_risk_limit"
                };
                diag.close_active(now, outcome);
            }
            if let Some(start) = self.unhedged_since.take() {
                let elapsed = now - start;
                self.report.total_unhedged_ms += elapsed;
                self.report.max_unhedged_ms = self.report.max_unhedged_ms.max(elapsed);
            }
        } else if self.unhedged_since.is_none() {
            self.unhedged_since = Some(now);
            self.report.unhedged_episodes += 1;
        }
    }

    fn final_mark(&self) -> Result<Option<(Decimal, Decimal)>> {
        let mut value = self.ledger.cash;
        let mut turnover = self.ledger.turnover;
        for (i, position) in self.ledger.positions.into_iter().enumerate() {
            if position.is_zero() {
                continue;
            }
            let Some(book) = self.fresh(i, self.config.end_ms) else {
                return Ok(None);
            };
            if book.timestamp_ms <= self.consumed_ms[i] {
                return Ok(None);
            }
            let side = direction(position);
            let terms = &self.config.pair.legs[i];
            let depth = Depth::build(book, terms, side, self.rules);
            let Ok(limit) = depth.limit(position.abs()) else {
                return Ok(None);
            };
            let fill = sweep(
                book,
                terms,
                side,
                limit,
                position.abs(),
                self.rules.participation,
            )?;
            if fill.base != position.abs() || fill.base < min_reduce_ioc_base(terms, limit) {
                return Ok(None);
            }
            value += cash(side, fill.notional, terms.taker_fee_bps);
            turnover += fill.notional;
        }
        Ok(Some((value, turnover)))
    }

    fn run(
        mut self,
        events: impl IntoIterator<Item = Result<MakerFirstEvent>>,
    ) -> Result<MakerFirstReport> {
        let mut previous = self.config.start_ms;
        let mut ended = false;
        for event in events {
            let event = event?;
            let now = event.time();
            ensure!(
                !ended && (previous..=self.config.end_ms).contains(&now),
                "unordered maker-first tape"
            );
            let prev_event = previous;
            previous = now;
            self.advance_timers(now, prev_event)?;
            if self.posture.expire(now) {
                self.report.turnover_posture.controls_expired += 1;
                self.posture_changed(now);
            }
            let mut book_frame = false;
            let mut book_evidence = BookEvidence::None;
            match event {
                MakerFirstEvent::Book {
                    books,
                    reference,
                    equity_session,
                    leg_oracles,
                    ..
                } => {
                    self.set_books(books, now)?;
                    let admitted_session =
                        self.observe_rwa_frame(now, reference, equity_session, leg_oracles);
                    self.observe_kalman_basis(now, reference, admitted_session, leg_oracles);
                    book_evidence = self.classify_persistence_book(now);
                    book_frame = true;
                }
                MakerFirstEvent::Trade {
                    source_ms,
                    leg,
                    aggressor,
                    price: px,
                    contracts,
                    ..
                } => {
                    ensure!(
                        leg < 2
                            && price(px)
                            && bounded(contracts)
                            && contracts > Decimal::ZERO
                            && source_ms > 0,
                        "invalid maker-first print"
                    );
                    if leg == self.maker()
                        && (self.funding.is_some() || self.funding_lifecycle.is_some())
                    {
                        if source_ms < self.config.start_ms {
                            // The replay starts with no orders or inventory. A
                            // reconnect history print cannot fill our later order.
                            if let Some(state) = &mut self.funding {
                                state.report.ignored_prestart_prints += 1;
                            }
                            if let Some(lifecycle) = &mut self.funding_lifecycle {
                                lifecycle.report.ignored_prestart_prints += 1;
                            }
                            self.report.trade_prints += 1;
                            continue;
                        }
                        let delivery_ms = now.saturating_sub(source_ms);
                        if let Some(state) = &mut self.funding {
                            state.report.max_trade_delivery_ms =
                                state.report.max_trade_delivery_ms.max(delivery_ms);
                        }
                        if let Some(lifecycle) = &mut self.funding_lifecycle {
                            lifecycle.report.max_trade_delivery_ms =
                                lifecycle.report.max_trade_delivery_ms.max(delivery_ms);
                        }
                        if source_ms > now || now - source_ms > self.config.trade_delivery_grace_ms
                        {
                            self.report.late_trade_prints += 1;
                            self.report.trade_prints += 1;
                            self.funding_failure("funding_trade_delivery_bound_violated");
                            self.funding_lifecycle_failure("funding_trade_delivery_bound_violated");
                            self.cancel(now, "unbounded_trade_delivery");
                            continue;
                        }
                    }
                    if leg == self.maker() && self.feed_online {
                        self.trade(now, source_ms, aggressor, px, contracts)?;
                    }
                }
                MakerFirstEvent::TradeFeed { leg, online, .. } => {
                    ensure!(leg < 2, "invalid maker-first trade feed leg");
                    if leg == self.maker() {
                        self.feed_online = online;
                        if !online
                            && let Some(depth) =
                                self.quote.as_mut().and_then(|q| q.queue_depth.as_mut())
                        {
                            depth.reset(now);
                        }
                        if !online {
                            self.basis_risk.feed_offline();
                            self.basis_reversion.feed_offline();
                            self.entry_flow.feed_offline();
                            if let Some(sc) = &mut self.slow_cascade {
                                sc.last_decision = None;
                            }
                            if let Some(kb) = &mut self.kalman_basis {
                                kb.last_snapshot = None;
                            }
                            self.entry_edge_persist.reset_all();
                            if self
                                .quote
                                .as_ref()
                                .is_some_and(|quote| quote.activated_ms.is_some())
                                || (self.funding.is_some() && !self.funding_flat())
                            {
                                self.report.incomplete = Some("resting_order_during_trade_gap");
                            }
                            if self.funding_lifecycle.as_ref().is_some_and(|lifecycle| {
                                lifecycle.has_unwatermarked_slot(self.maker(), now)
                            }) {
                                self.funding_lifecycle_failure(
                                    "funding_trade_feed_gap_at_settlement",
                                );
                            }
                            self.cancel(now, "trade_feed_gap");
                        }
                    }
                }
                MakerFirstEvent::TurnoverControl { control, .. } => {
                    let previous = self.posture.posture;
                    if self
                        .config
                        .turnover_posture
                        .is_some_and(|p| self.posture.control(p, now, control))
                    {
                        self.report.turnover_posture.controls_accepted += 1;
                        if previous != self.posture.posture {
                            self.posture_changed(now);
                        }
                    } else {
                        self.report.turnover_posture.controls_rejected += 1;
                    }
                }
                MakerFirstEvent::FundingSettlement {
                    leg, settlement, ..
                } => {
                    let failure = match &mut self.funding_lifecycle {
                        Some(lifecycle) => lifecycle.on_witness(leg, now, settlement)?,
                        None => {
                            anyhow::bail!(
                                "unexpected funding settlement without funding_settlement policy"
                            );
                        }
                    };
                    if let Some(reason) = failure {
                        self.funding_lifecycle_failure(reason);
                    } else {
                        self.resolve_funding_settlements(now, false);
                    }
                }
                MakerFirstEvent::FundingRate {
                    leg,
                    source_ms,
                    rate_per_settlement,
                    ..
                } => {
                    ensure!(leg < 2, "invalid funding rate leg");
                    self.carry_drift.observe_rate(
                        leg,
                        source_ms,
                        now,
                        rate_per_settlement,
                        self.config.pair.legs[leg].funding.interval_ms,
                    )?;
                    if let Some(policy) = self.config.carry_drift_adjustment
                        && let Some(quote) = self.quote.as_ref().filter(|quote| !quote.closing)
                        && matches!(
                            self.carry_drift
                                .adjustment_bps(policy, self.maker(), quote.side, now),
                            Err("carry_rate_conflict")
                        )
                    {
                        self.cancel(now, "carry_rate_conflict");
                    }
                }
                MakerFirstEvent::End { .. } => {
                    ensure!(
                        now == self.config.end_ms,
                        "incorrect maker-first tape cutoff"
                    );
                    if let Some(policy) = self.config.entry_markout_gate {
                        self.entry_markout.advance(now, None, policy);
                    }
                    self.risk(now);
                    ended = true;
                    continue;
                }
            }
            self.risk(now);
            if book_frame
                || self
                    .quote
                    .as_ref()
                    .is_some_and(|q| q.activated_ms.is_some())
            {
                self.advance_quote(now, book_frame);
            }
            self.advance_with_frame(now, book_evidence)?;
            self.risk(now);
        }
        ensure!(
            ended && self.report.book_frames > 0,
            "incomplete maker-first tape"
        );
        if let Some(costs) = &mut self.report.execution_costs {
            costs.finish(self.config.end_ms);
        }
        if let Some(at) = self.unhedged_since {
            let elapsed = self.config.end_ms - at;
            self.report.total_unhedged_ms += elapsed;
            self.report.max_unhedged_ms = self.report.max_unhedged_ms.max(elapsed);
        }
        self.report.turnover_posture.final_posture = self.posture.posture;
        self.report.open_positions_base = self.ledger.positions;
        self.report.cycle_attribution.paired_cycles_open_at_cutoff = usize::from(
            !self.ledger.is_flat()
                && self
                    .cycle
                    .as_ref()
                    .is_some_and(|c| c.paired_at_ms.is_some()),
        );
        self.report.fees = self.ledger.fees;
        self.resolve_funding_settlements(self.config.end_ms, true);
        if let Some(lifecycle) = &self.funding_lifecycle
            && lifecycle.report.resolved_slots_by_leg != lifecycle.report.planned_slots_by_leg
        {
            self.funding_lifecycle_failure("unresolved_funding_slot_at_cutoff");
        }
        if let Some(state) = &self.funding
            && state.report.verified_slots_by_leg != state.report.planned_slots_by_leg
        {
            self.funding_failure("funding_slots_not_verified");
        }
        if !self.ledger.is_flat()
            || self.quote.is_some()
            || self.ioc.is_some()
            || !self.notices.is_empty()
        {
            self.report.incomplete.get_or_insert("residual_at_cutoff");
        }
        // A marked residual cannot repair an unproven historical funding cashflow.
        let final_mark = if (self.funding.is_some() || self.funding_lifecycle.is_some())
            && self.report.incomplete.is_some()
        {
            None
        } else {
            self.final_mark()?
        };
        self.report.final_liquidation_pnl = final_mark.map(|(cash, _)| cash);
        if let Some(policy) = self.config.pair.mode.incentive() {
            let turnover = self.ledger.eligible_turnover;
            self.report.incentive_turnover = Some(IncentiveTurnoverReport {
                venue: self.config.pair.legs[policy.eligible_leg].venue.clone(),
                filled_turnover_quote: turnover,
                realized_cost_bps: (turnover > Decimal::ZERO
                    && self.ledger.is_flat()
                    && self.cycle.is_none()
                    && self.report.incomplete.is_none())
                .then(|| -self.ledger.cash / turnover * BPS),
            });
        }
        if self.report.incomplete.is_none() {
            self.report.net_pnl = Some(self.ledger.cash);
            self.report.pnl_before_fees = Some(self.ledger.cash + self.ledger.fees);
        }
        if let Some(policy) = self.config.turnover_feedback {
            let overhead_quote = policy.overhead(self.ledger.turnover);
            let net_after_overhead = self.report.net_pnl.map(|cash| cash - overhead_quote);
            let fixed_window_overhead_quote = policy.fixed_overhead_usd_per_day.map(|per_day| {
                let window_ms = self
                    .config
                    .end_ms
                    .saturating_sub(self.config.trade_from_ms.unwrap_or(self.config.start_ms));
                per_day * Decimal::from(window_ms) / Decimal::from(DAY_MS)
            });
            let net_after_fixed_window_overhead = self
                .report
                .net_pnl
                .zip(fixed_window_overhead_quote)
                .map(|(cash, fixed)| cash - fixed);
            let turnover = self.ledger.eligible_turnover;
            self.report.turnover_feedback = Some(TurnoverFeedbackReport {
                min_reserve_bps: policy.min_reserve_bps,
                max_reserve_bps: policy.max_reserve_bps,
                final_reserve_bps: self.cost_feedback.reserve(policy),
                max_observed_reserve_bps: policy.min_reserve_bps
                    + self.cost_feedback.max_excess_reserve_bps,
                cycles_used: self.cost_feedback.cycles_used,
                overhead_bps: policy.overhead_bps,
                overhead_quote,
                fixed_window_overhead_quote,
                net_after_fixed_window_overhead,
                modeled_target_turnover_quote: self.cost_feedback.target_turnover,
                modeled_eligible_turnover_quote: turnover,
                venue_credited_turnover_quote: None,
                net_after_overhead,
                liquidation_pnl_after_overhead: final_mark
                    .map(|(cash, gross)| cash - policy.overhead(gross)),
                realized_cost_bps: net_after_overhead
                    .filter(|_| turnover > Decimal::ZERO)
                    .map(|cash| -cash * BPS / turnover),
                outward_quotes: self.cost_feedback.outward_quotes,
            });
        }
        if let Some(costs) = &mut self.report.execution_costs {
            let after = self
                .report
                .turnover_feedback
                .as_ref()
                .map_or(self.report.net_pnl, |r| r.net_after_overhead);
            costs.reconcile_ledger(self.report.net_pnl, after);
        }
        let recovery_by_leg = self.report.cycle_attribution.sub_min_recovery_turnover;
        let excluded_recovery = recovery_by_leg[0] + recovery_by_leg[1];
        let target_leg = self
            .config
            .pair
            .mode
            .incentive()
            .map_or(self.maker(), |p| p.eligible_leg);
        self.report.turnover_breakdown = TurnoverBreakdown {
            capability_snapshot_version: crate::venues::VENUE_CAPABILITY_SNAPSHOT_VERSION,
            target_venue_turnover_quote: (self.leg_turnover[target_leg]
                - recovery_by_leg[target_leg])
                .max(Decimal::ZERO),
            dual_venue_gross_turnover_quote: (self.ledger.turnover - excluded_recovery)
                .max(Decimal::ZERO),
            rule_window_candidate_turnover_quote: self.ledger.eligible_turnover,
            confirmed_eligible_turnover_quote: None,
            excluded_recovery_turnover_quote: excluded_recovery,
        };
        let default_hedge_fee = self.config.pair.legs[1 - self.maker()].taker_fee_bps;
        let default_maker_fee = self.config.maker_fee_bps;
        self.report.entry_markout_gate = self.config.entry_markout_gate.map(|policy| {
            let fee = if policy.include_hedge_taker_fee {
                default_hedge_fee
            } else {
                default_maker_fee
            };
            self.entry_markout.report(policy, fee)
        });
        self.report.rwa_session = self.saw_rwa_metadata.then_some(self.rwa_session);
        self.report.funding_flat = self.funding.map(|state| state.report);
        self.report.funding_lifecycle = self.funding_lifecycle.take().map(|mut lifecycle| {
            if let Some(net_pnl) = self.report.net_pnl {
                let price_pnl = net_pnl - self.ledger.funding + self.ledger.fees;
                lifecycle.report.price_pnl_usd = Some(price_pnl);
                lifecycle.report.cash_identity_reconciled =
                    price_pnl + lifecycle.report.witnessed_funding_cash_usd - self.ledger.fees
                        == net_pnl
                        && lifecycle.report.witnessed_funding_cash_usd == self.ledger.funding;
            }
            lifecycle.report
        });
        self.report.carry_drift = self
            .config
            .carry_drift_adjustment
            .map(|_| self.carry_drift.report);
        self.report.basis_risk_reserve = self.config.basis_risk_reserve.map(|policy| {
            self.basis_risk
                .report(policy, self.config.latency_reserve_bps)
        });
        self.report.basis_reversion = self
            .config
            .basis_reversion
            .map(|policy| self.basis_reversion.report(policy));
        self.report.entry_flow_filter = self
            .config
            .entry_flow_filter
            .map(|policy| self.entry_flow.report(policy));
        self.report.entry_book_filter = self
            .config
            .entry_book_filter
            .map(|policy| self.entry_book.report(policy));
        self.report.entry_hedge_diagnostics = self
            .entry_hedge_diag
            .take()
            .map(|d| d.finish(self.config.end_ms));
        self.report.slow_cascade = self.slow_cascade.as_ref().map(SlowCascadeState::report);
        self.report.kalman_basis = self.kalman_basis.as_ref().map(KalmanBasisState::report);
        self.report.inventory_ladder = self
            .config
            .inventory_ladder
            .map(|_| self.inventory_ladder.report());
        self.report.entry_edge_persistence = self
            .config
            .entry_edge_persistence
            .map(|policy| self.entry_edge_persist.report(policy));
        self.report.post_fill_book_refresh = self
            .config
            .post_fill_book_refresh
            .map(|mode| self.post_fill_refresh.report(mode));
        Ok(self.report)
    }
}

pub fn run_maker_first(
    config: &MakerFirstConfig,
    events: impl IntoIterator<Item = Result<MakerFirstEvent>>,
    registry: &VenueRegistry,
) -> Result<MakerFirstReport> {
    config.validate(registry)?;
    new_replay(config)?.run(events)
}

fn new_replay(config: &MakerFirstConfig) -> Result<Replay<'_>> {
    let [a, b] = &config.pair.legs;
    Ok(Replay {
        config,
        rules: Rules::new(&config.pair)?,
        common_step: common_quantity_step(a.base_step(), b.base_step())?,
        ledger: Ledger::default(),
        leg_turnover: [Decimal::ZERO; 2],
        books: None,
        last_reference: None,
        last_equity_session: None,
        equity_session_uncertain: false,
        rwa_session: RwaSessionAttribution::default(),
        saw_rwa_metadata: config.pair.asset_class == AssetClass::Equity,
        usable: [false; 2],
        last_usable_at_ms: [None; 2],
        consumed_ms: [0; 2],
        last_source_ms: [0; 2],
        feed_online: false,
        quote: None,
        ioc: None,
        cycle: None,
        next_cycle_id: 0,
        closed_cycles_meta: BTreeMap::new(),
        next_quote_ms: 0,
        next_ioc_ms: 0,
        next_entry_ioc_ms: 0,
        notices: VecDeque::new(),
        unhedged_since: None,
        cost_feedback: CostFeedback::default(),
        basis_risk: BasisRiskState::default(),
        basis_reversion: BasisReversionState::default(),
        entry_flow: EntryFlowState::default(),
        entry_book: EntryBookState::default(),
        entry_markout: EntryMarkoutState::default(),
        entry_hedge_diag: config
            .diagnose_entry_hedge
            .then(EntryHedgeDiagState::default),
        posture: PostureState::new(config.turnover_posture),
        funding: FundingState::new(config)?,
        funding_lifecycle: FundingLifecycleState::new(config)?,
        carry_drift: CarryDriftState::default(),
        slow_cascade: config.slow_cascade.map(SlowCascadeState::new).transpose()?,
        kalman_basis: config.kalman_basis.map(KalmanBasisState::new).transpose()?,
        inventory_ladder: InventoryLadderState::default(),
        entry_edge_persist: EntryEdgePersistState::default(),
        post_fill_refresh: PostFillRefreshState::default(),
        report: MakerFirstReport {
            conservative_cancel_guard: config.conservative_cancel_guard,
            start_ms: config.start_ms,
            trade_from_ms: config.trade_from_ms.unwrap_or(config.start_ms),
            end_ms: config.end_ms,
            execution_costs: config.diagnose_cycles.then(costs::Costs::new),
            paired_cash_exit: config
                .paired_cash_exit
                .map(|_| PairedCashExitReport::default()),
            queue_model: config.queue_model.map(|qm| QueueModelReport {
                initial_ahead_fraction: qm.initial_ahead_fraction,
                cancel_progress_power: qm.cancel_progress_power,
                ..Default::default()
            }),
            ..Default::default()
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    mod basis_risk_tests {
        include!("maker_first/basis_risk_tests.rs");
    }

    mod funding_tests {
        include!("maker_first/funding_tests.rs");
    }

    mod paired_cash_exit_tests {
        include!("maker_first/paired_cash_exit_tests.rs");
    }

    mod regime_guards_tests {
        include!("maker_first/regime_guards_tests.rs");
    }

    mod entry_guards_tests {
        include!("maker_first/entry_guards_tests.rs");
    }

    fn config() -> MakerFirstConfig {
        let pair = serde_json::from_str::<super::super::PaperScenario>(include_str!(
            "../../../../examples/crypto_funding_paper.json"
        ))
        .unwrap()
        .pair;
        MakerFirstConfig {
            pair,
            maker_leg: 0,
            start_ms: 1_000_000,
            trade_from_ms: None,
            end_ms: 1_025_000,
            transport_ms: 100,
            fill_notice_ms: 100,
            maker_create_delay_ms: 0,
            maker_cancel_delay_ms: 100,
            taker_delay_ms: [0; 2],
            trade_clock_guard_ms: 0,
            conservative_cancel_guard: false,
            trade_delivery_grace_ms: 150,
            quote_lifetime_ms: 800,
            reprice_ms: 100,
            hold_ms: 2_000,
            unhedged_timeout_ms: 1_000,
            drain_ms: 4_000,
            flat_at_funding: None,
            require_net_edge: false,
            entry_side: None,
            turnover_posture: None,
            passive_offset: None,
            turnover_feedback: None,
            hedged_exit: None,
            paired_cash_exit: None,
            diagnose_cycles: false,
            diagnose_entry_hedge: false,
            hedge_ioc_policy: None,
            maker_fee_bps: Decimal::ONE,
            quote_notional: Decimal::from(100),
            latency_reserve_bps: Decimal::ZERO,
            hedge_protection_bps: None,
            closing_hedge_protection_bps: None,
            basis_risk_reserve: None,
            basis_reversion: None,
            entry_flow_filter: None,
            entry_book_filter: None,
            opening_max_levels: 1,
            aggregate_sub_minimum_fills: false,
            allow_sub_minimum_top_up_unwind: false,
            enforce_equity_session_gate: false,
            queue_model: None,
            entry_markout_gate: None,
            funding_settlement: None,
            carry_drift_adjustment: None,
            slow_cascade: None,
            kalman_basis: None,
            inventory_ladder: None,
            entry_edge_persistence: None,
            post_fill_book_refresh: None,
        }
    }

    fn d(value: &str) -> Decimal {
        value.parse().unwrap()
    }

    fn feedback() -> TurnoverFeedback {
        TurnoverFeedback {
            min_reserve_bps: d("0.25"),
            max_reserve_bps: d("4"),
            alpha: d("0.25"),
            overhead_bps: d("0.5"),
            max_quote_distance_bps: d("12"),
            fixed_overhead_usd_per_day: None,
        }
    }

    fn books(config: &MakerFirstConfig, at: u64, maker_bid: &str) -> MakerFirstEvent {
        let level = |price: &str, contracts: &str| Level {
            price: d(price),
            contracts: d(contracts),
        };
        MakerFirstEvent::Book {
            at_ms: at,
            books: [0, 1].map(|i| Book {
                market: config.pair.legs[i].market.clone(),
                timestamp_ms: at,
                tradable: true,
                bids: vec![level(if i == 0 { maker_bid } else { "1000.2" }, "0.10")],
                asks: vec![level(if i == 0 { "1000.2" } else { "1000.4" }, "1")],
                bounds: None,
                available_notional: Decimal::from(150),
            }),
            reference: None,
            equity_session: None,
            leg_oracles: None,
        }
    }

    fn partial(at: u64, source: u64, contracts: &str) -> MakerFirstEvent {
        MakerFirstEvent::Trade {
            at_ms: at,
            source_ms: source,
            leg: 0,
            aggressor: HedgeSide::Sell,
            price: d("1000"),
            contracts: d(contracts),
        }
    }

    fn cancellation_quote(start: u64) -> Quote {
        Quote {
            posture: TurnoverPosture::Neutral,
            side: HedgeSide::Buy,
            price: d("1000"),
            left: d("0.1"),
            ahead: Decimal::ZERO,
            queue_depth: None,
            arrival_ms: start,
            activated_ms: Some(start),
            expires_ms: start + 2_000,
            cancelled_ms: Some(start + 1_000),
            hedge_limit: Some(d("1000")),
            passive_entry: false,
            closing: false,
            closing_touched: false,
            closing_filled: false,
        }
    }

    #[test]
    fn cancel_guard_sensitivity_bounds_clock_overlap_and_preserves_cash() {
        for (offset, strict_fill, conservative_fill) in [
            (-101, true, true),
            (-100, false, true),
            (0, false, true),
            (100, false, true),
            (101, false, false),
        ] {
            for (conservative, expected) in [(false, strict_fill), (true, conservative_fill)] {
                let mut cfg = config();
                cfg.trade_clock_guard_ms = 100;
                cfg.conservative_cancel_guard = conservative;
                let mut replay = new_replay(&cfg).unwrap();
                replay.quote = Some(cancellation_quote(cfg.start_ms));
                let source = (cfg.start_ms as i64 + 1_000 + offset) as u64;
                replay
                    .trade(source + 50, source, HedgeSide::Sell, d("1000"), d("0.01"))
                    .unwrap();
                assert_eq!(replay.report.maker_fills, usize::from(expected));
                assert_eq!(
                    replay.report.cancel_guard_overlap_fills,
                    usize::from(expected && conservative && offset >= -100)
                );
                assert_eq!(
                    replay.ledger.positions[0],
                    if expected { d("0.01") } else { Decimal::ZERO }
                );
                assert_eq!(
                    replay.ledger.cash,
                    if expected {
                        d("-10.001")
                    } else {
                        Decimal::ZERO
                    }
                );
            }
        }
    }

    #[test]
    fn conservative_tombstone_keeps_last_receipt_then_releases() {
        let mut cfg = config();
        cfg.trade_clock_guard_ms = 100;
        cfg.conservative_cancel_guard = true;
        let mut replay = new_replay(&cfg).unwrap();
        replay.quote = Some(cancellation_quote(cfg.start_ms));
        let end = cfg.start_ms + 1_000 + 100 + cfg.trade_delivery_grace_ms;
        replay.advance_quote(end, false);
        assert!(replay.quote.is_some());
        replay
            .trade(
                end,
                end - cfg.trade_delivery_grace_ms,
                HedgeSide::Sell,
                d("1000"),
                d("0.01"),
            )
            .unwrap();
        assert_eq!(replay.report.cancel_guard_overlap_fills, 1);
        replay
            .trade(
                end + 1,
                end - cfg.trade_delivery_grace_ms,
                HedgeSide::Sell,
                d("1000"),
                d("0.01"),
            )
            .unwrap();
        assert_eq!(replay.report.maker_fills, 1);
        replay.advance_quote(end + 1, false);
        assert!(replay.quote.is_none());
        assert_eq!(replay.ledger.positions[0], d("0.01"));
        assert!(!replay.notices.is_empty());
    }

    #[test]
    fn warmup_observes_books_but_only_admits_entries_at_trade_from() {
        let mut cfg = config();
        let start = cfg.start_ms;
        let boundary = start + 1_000;
        cfg.trade_from_ms = Some(boundary);
        let run = |last_book| {
            let events = [
                MakerFirstEvent::TradeFeed {
                    at_ms: start,
                    leg: 0,
                    online: true,
                },
                books(&cfg, start, "1000"),
                books(&cfg, last_book, "1000"),
                MakerFirstEvent::End { at_ms: cfg.end_ms },
            ];
            run_maker_first(&cfg, events.into_iter().map(Ok), &VenueRegistry::default()).unwrap()
        };
        let warm = run(boundary - 1);
        assert_eq!(warm.book_frames, 2);
        assert_eq!(warm.quotes_submitted, 0);
        assert_eq!(warm.entry_checks, 0);
        let active = run(boundary);
        assert!(active.entry_checks > 0);
        assert!(active.quotes_submitted > 0);
        cfg.trade_from_ms = Some(cfg.end_ms);
        assert!(cfg.preflight(&VenueRegistry::default()).is_err());
    }

    #[test]
    fn waits_for_queue_and_only_hedges_confirmed_partials() {
        let mut cfg = config();
        cfg.pair.planner.max_leg_notional_usd = 150.0;
        cfg.pair.planner.max_unhedged_notional_usd = 150.0;
        cfg.pair.planner.max_base_quantity = 0.1;
        cfg.pair.planner.max_quote_skew_ms = 750;
        cfg.maker_fee_bps = Decimal::ZERO;
        cfg.pair.legs[1].taker_fee_bps = Decimal::ZERO;
        cfg.pair.legs[1].tick = d("0.1");
        cfg.pair.limit_buffer_bps = 0.0;
        cfg.pair.planner.depth_fraction = 1.0;
        for leg in &mut cfg.pair.legs {
            leg.lot = d("0.01");
            leg.min_notional = Decimal::ZERO;
            leg.min_contracts = d("0.01");
        }
        let t = cfg.start_ms;
        let mut rows = vec![
            MakerFirstEvent::TradeFeed {
                at_ms: t,
                leg: 0,
                online: true,
            },
            books(&cfg, t, "1000"),
            books(&cfg, t + 100, "1000"),
            partial(t + 110, t + 99, "0.30"),  // Before activation.
            partial(t + 120, t + 115, "0.09"), // Still behind the queue.
            partial(t + 130, t + 125, "0.03"), // 0.02 confirmed at +230.
            partial(t + 180, t + 175, "0.01"), // 0.01 confirmed at +280.
            books(&cfg, t + 230, "1000"),
            books(&cfg, t + 280, "1000"),
            books(&cfg, t + 329, "1000"), // Before the first IOC arrives.
            books(&cfg, t + 330, "1000"),
            partial(t + 445, t + 420, "0.01"), // Executed before cancel effective at +430.
            partial(t + 460, t + 431, "0.09"), // Executed after the cancel.
            books(&cfg, t + 470, "1000"),
            books(&cfg, t + 550, "1000"),
            books(&cfg, t + 660, "1000"),
        ];
        rows.push(MakerFirstEvent::End { at_ms: cfg.end_ms });
        let r = run_maker_first(&cfg, rows.into_iter().map(Ok), &VenueRegistry::default()).unwrap();
        assert_eq!(r.maker_fills, 3, "{r:?}");
        assert_eq!(r.maker_turnover, d("40"));
        assert_eq!(r.ioc_fills, 3);
        assert_eq!(r.open_positions_base, [d("0.04"), d("-0.04")]);
        assert_eq!(r.total_unhedged_ms, 530);
        assert!(r.net_pnl.is_none());
    }

    #[test]
    fn refuses_a_settlement_boundary_in_execution_mode() {
        let mut cfg = config();
        cfg.start_ms = 3_590_000;
        cfg.end_ms = 3_615_000;
        assert!(cfg.validate(&VenueRegistry::default()).is_err());
    }

    #[test]
    fn ten_hour_preflight_reports_each_funding_schedule_without_authorizing_execution() {
        let mut cfg = config();
        cfg.end_ms = cfg.start_ms + 10 * 3_600_000;
        cfg.pair.legs[1].funding.interval_ms = 8 * 3_600_000;
        let preflight = cfg.preflight(&VenueRegistry::default()).unwrap();
        assert_eq!(preflight.duration_ms, 36_000_000);
        assert_eq!(preflight.funding_slots_by_leg, [10, 1]);
        assert_eq!(
            preflight.first_funding_slot_ms,
            [Some(3_600_000), Some(28_800_000)]
        );
        assert!(!preflight.execution_supported);
        assert!(
            preflight
                .required_engine_work
                .contains(&"source_time_position_watermark_and_late_fill_resolution")
        );
        assert!(cfg.validate(&VenueRegistry::default()).is_err());
        assert!(
            run_maker_first(
                &cfg,
                Vec::<Result<MakerFirstEvent>>::new(),
                &VenueRegistry::default()
            )
            .is_err()
        );
    }

    #[test]
    fn preflight_preserves_short_window_admission_and_rejects_invalid_risk_parameters() {
        let mut cfg = config();
        let short = cfg.preflight(&VenueRegistry::default()).unwrap();
        assert!(short.execution_supported);
        assert!(short.equity_session_gate_enforced);
        assert_eq!(short.funding_slots_by_leg, [0, 0]);
        assert!(short.required_engine_work.is_empty());
        assert_eq!(short.scope, "configuration_only_no_tape_validated");

        let mut equity_cfg = config();
        equity_cfg.pair.asset_class = AssetClass::Equity;
        equity_cfg.pair.oracle_max_age_ms = 5_000;
        equity_cfg.pair.equity_close_buffer_ms = 1_000;
        equity_cfg.flat_at_funding = Some(FlatAtFunding {
            settlement_guard_ms: 2_000,
        });
        equity_cfg.enforce_equity_session_gate = false;
        let eq_unprotected = equity_cfg.preflight(&VenueRegistry::default()).unwrap();
        assert!(!eq_unprotected.equity_session_gate_enforced);
        assert!(
            eq_unprotected
                .required_evidence
                .contains(&"equity_session_and_reference_gate_enforced")
        );
        equity_cfg.enforce_equity_session_gate = true;
        let eq_protected = equity_cfg.preflight(&VenueRegistry::default()).unwrap();
        assert!(eq_protected.equity_session_gate_enforced);
        assert!(
            !eq_protected
                .required_evidence
                .contains(&"equity_session_and_reference_gate_enforced")
        );

        cfg.end_ms = cfg.start_ms + 36_000_000;
        cfg.quote_notional = Decimal::ZERO;
        assert!(cfg.preflight(&VenueRegistry::default()).is_err());
    }

    #[test]
    fn paired_wait_preserves_opening_deadline_and_closes_by_maker_when_hedged() {
        let mut cfg = passive_config(HedgeSide::Buy);
        cfg.passive_offset = None;
        cfg.pair.limit_buffer_bps = 0.0;
        cfg.pair.strategy = Strategy::BasisReversion;
        cfg.pair.legs[0].venue = "arcus".into();
        cfg.pair.legs[0].taker_fee_bps = d("4.5");
        cfg.pair.legs[1].taker_fee_bps = Decimal::ZERO;
        cfg.pair.planner.min_net_edge_bps = 0.0;
        cfg.require_net_edge = false;
        cfg.maker_fee_bps = d("1.5");
        cfg.hedged_exit = Some(HedgedExit {
            max_hold_ms: 6_000,
            min_saving_bps: Decimal::ZERO,
            max_adverse_basis_bps: d("10"),
            use_cycle_deadline: false,
            closing_quote_lifetime_ms: None,
            patience_ms: 0,
            target_cycle_edge_bps: None,
            closing_max_levels: 1,
            max_hedged_book_stale_ms: None,
            max_hedged_leg_adverse_bps: None,
            hedged_leg_collateral_quote: None,
            allow_partner_step_deadband: false,
            max_hedged_both_blind_ms: None,
        });
        let start = cfg.start_ms;
        let run = |cfg: &MakerFirstConfig, opening_fill, hedge_fill| {
            let mut rows = vec![
                MakerFirstEvent::TradeFeed {
                    at_ms: start,
                    leg: 0,
                    online: true,
                },
                books(cfg, start, "999.8"),
                books(cfg, start + 100, "999.8"),
                print_against(start + 110, HedgeSide::Buy, "999.7", opening_fill),
                books(cfg, start + 210, "999.8"),
            ];
            if hedge_fill {
                rows.push(books(cfg, start + 310, "999.8"));
            }
            rows.extend([
                books(cfg, start + 1_800, "999.8"),
                books(cfg, start + 2_300, "999.8"),
                books(cfg, start + 2_400, "999.8"),
                print_against(start + 2_410, HedgeSide::Sell, "1000.3", opening_fill),
                books(cfg, start + 2_510, "999.8"),
                books(cfg, start + 2_610, "999.8"),
                MakerFirstEvent::TradeFeed {
                    at_ms: start + 2_611,
                    leg: 0,
                    online: false,
                },
                books(cfg, start + 3_050, "999.8"),
                MakerFirstEvent::End { at_ms: cfg.end_ms },
            ]);
            run_maker_first(cfg, rows.into_iter().map(Ok), &VenueRegistry::default()).unwrap()
        };
        let paired = run(&cfg, "0.04", true);
        let stats = &paired.cycle_attribution;
        assert_eq!(stats.paired_maker_closes, 1, "{paired:?}");
        assert_eq!(stats.forced_ioc_cycles, 0);
        assert_eq!(stats.total_time_to_hedge_ms, 200);
        assert_eq!(stats.total_hedge_to_close_ms, 2300);
        assert!(stats.closing_quotes.submitted >= stats.closing_quotes.accepted);
        assert!(stats.closing_quotes.accepted >= stats.closing_quotes.touched);
        assert_eq!(stats.closing_quotes.touched, 1);
        assert_eq!(stats.closing_quotes.filled, 1);
        assert_eq!(stats.closing_maker_fills, 1);
        assert_eq!(stats.opening_maker_fees[0], d("0.0059988"));
        assert_eq!(stats.closing_maker_fees[0], d("0.0060012"));
        assert_eq!(paired.fees, d("0.012"));
        assert_eq!(paired.net_pnl, Some(d("-0.004")));
        assert_eq!(stats.max_paired_gross_notional, d("80.012"));

        let unhedged = run(&cfg, "0.02", false);
        assert_eq!(unhedged.exposure_stops, 1, "{unhedged:?}");
        assert_eq!(unhedged.cycle_attribution.paired_maker_closes, 0);
        assert_eq!(unhedged.cycle_attribution.closing_quotes.submitted, 0);
        assert_eq!(unhedged.cycle_attribution.hedged_time_stops, 0);
        assert!(unhedged.cycle_attribution.forced_ioc_fees[0] > Decimal::ZERO);
    }

    #[test]
    fn paired_exit_recovers_a_partial_maker_close_without_stranding_a_leg() {
        let mut cfg = passive_config(HedgeSide::Buy);
        cfg.passive_offset = None;
        cfg.pair.limit_buffer_bps = 0.0;
        cfg.pair.strategy = Strategy::BasisReversion;
        cfg.pair.legs[0].venue = "arcus".into();
        cfg.pair.legs[0].taker_fee_bps = d("4.5");
        cfg.pair.legs[1].taker_fee_bps = Decimal::ZERO;
        cfg.pair.legs[1].min_notional = d("30");
        cfg.require_net_edge = false;
        cfg.maker_fee_bps = d("1.5");
        cfg.hedged_exit = Some(HedgedExit {
            max_hold_ms: 6_000,
            min_saving_bps: Decimal::ZERO,
            max_adverse_basis_bps: d("10"),
            use_cycle_deadline: false,
            closing_quote_lifetime_ms: None,
            patience_ms: 0,
            target_cycle_edge_bps: None,
            closing_max_levels: 1,
            max_hedged_book_stale_ms: None,
            max_hedged_leg_adverse_bps: None,
            hedged_leg_collateral_quote: None,
            allow_partner_step_deadband: false,
            max_hedged_both_blind_ms: None,
        });
        let t = cfg.start_ms;
        let registry = VenueRegistry::default();
        let opening = |cfg: &MakerFirstConfig| {
            vec![
                MakerFirstEvent::TradeFeed {
                    at_ms: t,
                    leg: 0,
                    online: true,
                },
                books(cfg, t, "999.8"),
                books(cfg, t + 100, "999.8"),
                print_against(t + 110, HedgeSide::Buy, "999.7", "0.04"),
                books(cfg, t + 210, "999.8"),
                books(cfg, t + 310, "999.8"),
            ]
        };
        let mut rows = opening(&cfg);
        rows.extend([
            books(&cfg, t + 1_800, "999.8"),
            books(&cfg, t + 2_300, "999.8"),
            books(&cfg, t + 2_400, "999.8"),
            print_against(t + 2_410, HedgeSide::Sell, "1000.3", "0.02"),
            books(&cfg, t + 2_510, "999.8"),
            books(&cfg, t + 2_610, "999.8"),
            books(&cfg, t + 4_120, "999.8"),
            books(&cfg, t + 4_220, "999.8"),
            MakerFirstEvent::TradeFeed {
                at_ms: t + 4_221,
                leg: 0,
                online: false,
            },
            books(&cfg, t + 4_320, "999.8"),
            MakerFirstEvent::End { at_ms: cfg.end_ms },
        ]);
        let recovered = run_maker_first(&cfg, rows.into_iter().map(Ok), &registry).unwrap();
        assert_eq!(
            recovered.cycle_attribution.closing_maker_fills, 1,
            "{recovered:?}"
        );
        assert!(recovered.cycle_attribution.reduce_ioc_dust_avoided > 0);
        assert_eq!(recovered.cycle_attribution.reduce_ioc_dust_fills, 0);
        assert_eq!(recovered.cycle_attribution.forced_ioc_cycles, 1);
        assert_eq!(recovered.open_positions_base, [Decimal::ZERO; 2]);
        assert!(recovered.net_pnl.is_some(), "{recovered:?}");

        let mut rows = opening(&cfg);
        for offset in [1_800, 2_300, 2_400, 6_310, 6_410] {
            let mut event = books(&cfg, t + offset, "999.8");
            if let MakerFirstEvent::Book { books, .. } = &mut event {
                books[1].bids[0].price = d("699.8");
                books[1].asks[0].price = d("700.0");
            }
            rows.push(event);
        }
        rows.push(MakerFirstEvent::End { at_ms: cfg.end_ms });
        let blocked = run_maker_first(&cfg, rows.into_iter().map(Ok), &registry).unwrap();
        assert!(
            blocked.cycle_attribution.paired_drain_blocked > 0,
            "{blocked:?}"
        );
        assert_eq!(blocked.open_positions_base, [d("0.04"), d("-0.04")]);
        assert_eq!(blocked.max_unhedged_ms, 200);
        assert!(blocked.net_pnl.is_none());

        cfg.pair.legs[0].min_contracts = d("0.02");
        let mut rows = opening(&cfg);
        rows.push(books(&cfg, t + 6_310, "999.8"));
        let mut arrival = books(&cfg, t + 6_410, "999.8");
        if let MakerFirstEvent::Book { books, .. } = &mut arrival {
            books[0].bids[0].contracts = d("0.03");
        }
        rows.extend([
            arrival,
            books(&cfg, t + 6_510, "999.8"),
            MakerFirstEvent::End { at_ms: cfg.end_ms },
        ]);
        let partial = run_maker_first(&cfg, rows.into_iter().map(Ok), &registry).unwrap();
        assert_eq!(
            partial.cycle_attribution.reduce_ioc_dust_fills, 1,
            "{partial:?}"
        );
        assert_eq!(partial.open_positions_base, [d("0.01"), Decimal::ZERO]);
        assert!(partial.net_pnl.is_none());
    }

    #[test]
    fn profit_floor_is_compared_in_basis_points() {
        let mut cfg = config();
        cfg.require_net_edge = true;
        cfg.maker_fee_bps = Decimal::ZERO;
        cfg.pair.legs[1].taker_fee_bps = Decimal::ZERO;
        cfg.pair.planner.max_quote_skew_ms = 750;
        let start = cfg.start_ms;
        let tape = |cfg: &MakerFirstConfig| {
            [
                MakerFirstEvent::TradeFeed {
                    at_ms: start,
                    leg: 0,
                    online: true,
                },
                books(cfg, start, "999.8"),
                books(cfg, start + 100, "999.8"),
                books(cfg, start + 1100, "999.8"),
                books(cfg, start + 1700, "999.8"),
                MakerFirstEvent::End { at_ms: cfg.end_ms },
            ]
            .into_iter()
            .map(Ok)
        };
        let registry = VenueRegistry::default();
        let accepted = run_maker_first(&cfg, tape(&cfg), &registry).unwrap();
        assert!(accepted.best_modeled_edge_bps.unwrap() > Decimal::ONE);
        assert!(accepted.quotes_accepted > 0);
        cfg.pair.planner.min_net_edge_bps = 3.0;
        let rejected = run_maker_first(&cfg, tape(&cfg), &registry).unwrap();
        assert_eq!(rejected.quotes_submitted, 0);
        assert!(rejected.entry_blocked.contains_key("entry_net_edge"));
    }

    fn passive_config(opening: HedgeSide) -> MakerFirstConfig {
        let mut cfg = config();
        cfg.entry_side = Some(opening);
        cfg.unhedged_timeout_ms = 1_700;
        cfg.passive_offset = Some(PassiveOffset {
            max_net_notional: d("100"),
            max_wait_ms: 1_200,
            close_probability: None,
            min_passive_edge_bps: None,
            min_offset_edge_bps: None,
            max_adverse_move_bps: None,
            offset_max_levels: 1,
            offset_quote_lifetime_ms: None,
            passive_loop_only: false,
        });
        cfg.pair.planner.max_base_quantity = 0.04;
        cfg.pair.planner.max_quote_skew_ms = 750;
        cfg.pair.planner.depth_fraction = 1.0;
        cfg.pair.legs.iter_mut().for_each(|terms| {
            terms.min_notional = Decimal::ZERO;
            terms.min_contracts = d("0.01");
            terms.lot = d("0.01");
        });
        cfg.require_net_edge = true;
        cfg.maker_fee_bps = Decimal::ZERO;
        cfg
    }

    fn print_against(
        at_ms: u64,
        resting: HedgeSide,
        price: &str,
        contracts: &str,
    ) -> MakerFirstEvent {
        MakerFirstEvent::Trade {
            at_ms,
            source_ms: at_ms,
            leg: 0,
            aggressor: opposite(resting),
            price: d(price),
            contracts: d(contracts),
        }
    }

    #[test]
    fn passive_offset_closes_a_long_or_short_without_a_taker_order() {
        for opening in [HedgeSide::Buy, HedgeSide::Sell] {
            let mut cfg = passive_config(opening);
            cfg.diagnose_entry_hedge = true;
            let t = cfg.start_ms;
            let (entry, exit) = match opening {
                HedgeSide::Buy => ("999.7", "1000.3"),
                HedgeSide::Sell => ("1000.3", "999.7"),
            };
            let events = [
                MakerFirstEvent::TradeFeed {
                    at_ms: t,
                    leg: 0,
                    online: true,
                },
                books(&cfg, t, "999.8"),
                books(&cfg, t + 100, "999.8"),
                print_against(t + 110, opening, entry, "0.04"),
                books(&cfg, t + 210, "999.8"),
                books(&cfg, t + 310, "999.8"),
                print_against(t + 320, opposite(opening), exit, "0.04"),
                books(&cfg, t + 420, "999.8"),
                MakerFirstEvent::End { at_ms: cfg.end_ms },
            ];
            let result =
                run_maker_first(&cfg, events.into_iter().map(Ok), &VenueRegistry::default())
                    .unwrap();
            assert_eq!(result.passive_only_cycles, 1, "{result:?}");
            assert_eq!(result.passive_only_pnl, d("0.016"));
            assert_eq!([result.maker_buy_fills, result.maker_sell_fills], [1, 1]);
            assert_eq!(result.ioc_submitted, 0);
            assert_eq!(result.max_unhedged_ms, 210);
            let ep = &result.entry_hedge_diagnostics.as_ref().unwrap().episodes[0];
            assert_eq!(ep.outcome, "passive_offset_flat");
            assert_eq!(ep.entry_unhedged_ms, Some(210));
            assert_eq!(ep.risk_resolved_ms, Some(ep.opening_fill_ms + 210));
            assert_eq!(ep.fully_hedged_ms, None);
            assert_eq!(result.passive_cycles_started[side_index(opening)], 1);
            assert_eq!(result.passive_cycles_completed[side_index(opening)], 1);
        }
    }

    #[test]
    fn calibrated_offset_prices_fallback_before_posting() {
        let mut cfg = passive_config(HedgeSide::Buy);
        cfg.pair.limit_buffer_bps = 0.0;
        cfg.pair.planner.min_net_edge_bps = 1.0;
        cfg.pair.legs[0].taker_fee_bps = d("10");
        cfg.pair.legs[1].taker_fee_bps = d("10");
        let run = |cfg: &MakerFirstConfig| {
            let t = cfg.start_ms;
            let events = [
                MakerFirstEvent::TradeFeed {
                    at_ms: t,
                    leg: 0,
                    online: true,
                },
                books(cfg, t, "999.8"),
                books(cfg, t + 100, "999.8"),
                print_against(t + 110, HedgeSide::Buy, "999.7", "0.04"),
                books(cfg, t + 210, "999.8"),
                books(cfg, t + 310, "999.8"),
                print_against(t + 320, HedgeSide::Sell, "1000.3", "0.04"),
                books(cfg, t + 420, "999.8"),
                MakerFirstEvent::TradeFeed {
                    at_ms: t + 421,
                    leg: 0,
                    online: false,
                },
                books(cfg, t + 1_000, "999.8"),
                MakerFirstEvent::End { at_ms: cfg.end_ms },
            ];
            run_maker_first(cfg, events.into_iter().map(Ok), &VenueRegistry::default()).unwrap()
        };
        cfg.passive_offset.as_mut().unwrap().close_probability = Some([d("0.1"), d("1")]);
        let no_edge = run(&cfg);
        assert_eq!(no_edge.quotes_submitted, 0);
        assert!(no_edge.entry_blocked.contains_key("entry_net_edge"));
        cfg.passive_offset.as_mut().unwrap().close_probability = Some([d("1"), d("0.1")]);
        let selected = run(&cfg);
        assert_eq!(selected.passive_cycles_completed, [1, 0]);
        assert_eq!(selected.net_pnl, Some(d("0.016")));
    }

    #[test]
    fn maker_turnover_requires_fallback_budget_and_credits_only_flat_cycles() {
        let mut cfg = passive_config(HedgeSide::Buy);
        cfg.pair.strategy = Strategy::BasisReversion;
        cfg.pair.legs[0].venue = "arcus".into();
        cfg.pair.legs[0].taker_fee_bps = d("10");
        cfg.pair.legs[1].taker_fee_bps = d("10");
        cfg.pair.limit_buffer_bps = 0.0;
        cfg.passive_offset.as_mut().unwrap().close_probability = Some([Decimal::ONE; 2]);
        let mode = |cap| PaperMode::Turnover {
            max_turnover_quote: d("500"),
            max_fees_quote: d("1"),
            max_realized_loss_quote: Some(d("0.50")),
            incentive: Some(IncentiveTurnover {
                eligible_leg: 0,
                eligible_from_ms: 1_000_000,
                eligible_until_ms: 1_025_000,
                max_cost_bps: d(cap),
            }),
        };
        let run = |cfg: &MakerFirstConfig, close| {
            let t = cfg.start_ms;
            let mut events = vec![
                MakerFirstEvent::TradeFeed {
                    at_ms: t,
                    leg: 0,
                    online: true,
                },
                books(cfg, t, "999.8"),
                books(cfg, t + 100, "999.8"),
                print_against(t + 110, HedgeSide::Buy, "999.7", "0.04"),
                books(cfg, t + 210, "999.8"),
                books(cfg, t + 310, "999.8"),
            ];
            if close {
                events.extend([
                    print_against(t + 320, HedgeSide::Sell, "1000.3", "0.04"),
                    books(cfg, t + 420, "999.8"),
                    MakerFirstEvent::TradeFeed {
                        at_ms: t + 421,
                        leg: 0,
                        online: false,
                    },
                    books(cfg, t + 1_500, "999.8"),
                    books(cfg, t + 2_000, "999.8"),
                ]);
            }
            events.push(MakerFirstEvent::End { at_ms: cfg.end_ms });
            run_maker_first(cfg, events.into_iter().map(Ok), &VenueRegistry::default()).unwrap()
        };
        cfg.pair.mode = mode("4");
        let rejected = run(&cfg, false);
        assert_eq!(rejected.quotes_submitted, 0);
        assert!(rejected.entry_blocked.contains_key("turnover_cost"));
        cfg.pair.mode = mode("30");
        let open = run(&cfg, false).incentive_turnover.unwrap();
        assert_eq!(open.filled_turnover_quote, Decimal::ZERO);
        assert!(open.realized_cost_bps.is_none());
        let flat = run(&cfg, true);
        assert_eq!(flat.passive_cycles_completed, [1, 0]);
        let measured = flat.incentive_turnover.unwrap();
        assert_eq!(measured.filled_turnover_quote, d("80"));
        assert_eq!(measured.realized_cost_bps, Some(d("-2")));
    }

    #[test]
    fn turnover_feedback_widens_after_loss_and_returns_to_the_floor() {
        let mut state = CostFeedback::default();
        let policy = feedback();
        assert_eq!(policy.overhead(d("400")), d("0.02"));
        assert_eq!(state.reserve(policy), d("0.25"));
        state.observe(policy, d("-0.06"), d("200"));
        assert_eq!(state.reserve(policy), d("1"));
        state.observe(policy, d("0.06"), d("200"));
        assert_eq!(state.reserve(policy), d("0.25"));
        state.observe(policy, d("-0.06"), Decimal::ZERO);
        assert_eq!(state.reserve(policy), d("4"));
        assert_eq!(state.cycles_used, 2);
        let fixed = TurnoverFeedback {
            alpha: Decimal::ZERO,
            ..policy
        };
        state = CostFeedback::default();
        state.observe(fixed, d("-0.06"), Decimal::ZERO);
        assert_eq!(state.reserve(fixed), d("0.25"));
    }

    #[test]
    fn zero_cost_quote_uses_observed_outward_depth_and_counts_only_flat_cycles() {
        let mut cfg = passive_config(HedgeSide::Buy);
        cfg.pair.strategy = Strategy::BasisReversion;
        cfg.pair.legs[0].venue = "arcus".into();
        cfg.pair.legs[0].taker_fee_bps = Decimal::ZERO;
        cfg.pair.legs[1].taker_fee_bps = Decimal::ZERO;
        cfg.pair.limit_buffer_bps = 0.0;
        cfg.passive_offset.as_mut().unwrap().close_probability = Some([Decimal::ONE; 2]);
        cfg.pair.mode = PaperMode::Turnover {
            max_turnover_quote: d("500"),
            max_fees_quote: d("1"),
            max_realized_loss_quote: Some(d("0.50")),
            incentive: Some(IncentiveTurnover {
                eligible_leg: 0,
                eligible_from_ms: cfg.start_ms,
                eligible_until_ms: cfg.end_ms,
                max_cost_bps: Decimal::ZERO,
            }),
        };
        let run = |cfg: &MakerFirstConfig, observed_at_arrival, close| {
            let t = cfg.start_ms;
            let two_levels = |at, deeper| {
                let mut event = books(cfg, at, "999.8");
                if let MakerFirstEvent::Book { books, .. } = &mut event
                    && deeper
                {
                    books[0].bids.push(Level {
                        price: d("999.0"),
                        contracts: d("0.10"),
                    });
                }
                event
            };
            let mut events = vec![
                MakerFirstEvent::TradeFeed {
                    at_ms: t,
                    leg: 0,
                    online: true,
                },
                two_levels(t, true),
                two_levels(t + 100, observed_at_arrival),
                print_against(t + 110, HedgeSide::Buy, "999.0", "0.04"),
                print_against(t + 120, HedgeSide::Buy, "998.9", "0.04"),
                two_levels(t + 220, true),
                two_levels(t + 320, true),
            ];
            if close {
                events.extend([
                    print_against(t + 330, HedgeSide::Sell, "1000.3", "0.04"),
                    two_levels(t + 430, true),
                    MakerFirstEvent::TradeFeed {
                        at_ms: t + 431,
                        leg: 0,
                        online: false,
                    },
                    two_levels(t + 1_500, true),
                    two_levels(t + 2_000, true),
                ]);
            }
            events.push(MakerFirstEvent::End { at_ms: cfg.end_ms });
            run_maker_first(cfg, events.into_iter().map(Ok), &VenueRegistry::default()).unwrap()
        };
        assert_eq!(run(&cfg, true, true).quotes_submitted, 0);
        cfg.turnover_feedback = Some(feedback());
        let missing = run(&cfg, false, false);
        assert_eq!(missing.maker_fills, 0);
        assert!(missing.post_only_rejected > 0);
        assert_eq!(
            missing.post_only_rejected_beyond_depth,
            missing.post_only_rejected
        );
        assert_eq!(missing.post_only_rejected_new_level, 0);
        let open = run(&cfg, true, false).turnover_feedback.unwrap();
        assert_eq!(open.cycles_used, 0);
        assert_eq!(open.modeled_target_turnover_quote, d("39.96"));
        assert_eq!(open.modeled_eligible_turnover_quote, Decimal::ZERO);
        assert!(open.net_after_overhead.is_none());
        let flat = run(&cfg, true, true);
        assert_eq!(flat.maker_fills, 2, "{flat:?}");
        let measured = flat.turnover_feedback.unwrap();
        assert!(measured.outward_quotes > 0);
        assert_eq!(measured.cycles_used, 1);
        assert_eq!(measured.modeled_eligible_turnover_quote, d("79.968"));
        assert_eq!(measured.net_after_overhead, Some(d("0.0440016")));
        assert!(measured.realized_cost_bps.unwrap() < Decimal::ZERO);
        assert!(measured.venue_credited_turnover_quote.is_none());
    }

    #[test]
    fn offset_timeout_waits_for_cancel_then_hedges_only_the_residual() {
        let cfg = passive_config(HedgeSide::Sell);
        let t = cfg.start_ms;
        let events = [
            MakerFirstEvent::TradeFeed {
                at_ms: t,
                leg: 0,
                online: true,
            },
            books(&cfg, t, "999.8"),
            books(&cfg, t + 100, "999.8"),
            print_against(t + 110, HedgeSide::Sell, "1000.3", "0.04"),
            books(&cfg, t + 210, "999.8"),
            books(&cfg, t + 310, "999.8"),
            books(&cfg, t + 960, "999.8"),
            print_against(t + 1_080, HedgeSide::Buy, "999.7", "0.01"),
            books(&cfg, t + 1_180, "999.8"),
            books(&cfg, t + 1_310, "999.8"),
            books(&cfg, t + 1_409, "999.8"),
            books(&cfg, t + 1_410, "999.8"),
            MakerFirstEvent::End { at_ms: cfg.end_ms },
        ];
        let result =
            run_maker_first(&cfg, events.into_iter().map(Ok), &VenueRegistry::default()).unwrap();
        assert_eq!(result.passive_offset_fills, 1);
        assert_eq!(result.ioc_fills, 1);
        assert_eq!(result.open_positions_base, [d("-0.03"), d("0.03")]);
        assert_eq!(result.max_unhedged_ms, 1_300);
        assert_eq!(result.passive_cycles_started, [0, 1]);
        assert_eq!(result.passive_cycles_completed, [0, 0]);
    }
    fn posture_config(posture: TurnoverPosture) -> MakerFirstConfig {
        let mut c = config();
        c.pair.strategy = Strategy::BasisReversion;
        c.pair.mode = PaperMode::Turnover {
            max_turnover_quote: d("100000"),
            max_fees_quote: d("100"),
            max_realized_loss_quote: Some(d("100")),
            incentive: None,
        };
        c.pair.legs[0].venue = "arcus".into();
        for leg in &mut c.pair.legs {
            leg.taker_fee_bps = Decimal::ZERO;
        }
        c.pair.planner.depth_fraction = 1.0;
        c.pair.planner.min_net_edge_bps = 0.0;
        c.maker_fee_bps = Decimal::ZERO;
        c.require_net_edge = true;
        c.hedged_exit = Some(HedgedExit {
            max_hold_ms: 6000,
            min_saving_bps: Decimal::ZERO,
            max_adverse_basis_bps: d("10"),
            use_cycle_deadline: false,
            closing_quote_lifetime_ms: None,
            patience_ms: 0,
            target_cycle_edge_bps: None,
            closing_max_levels: 1,
            max_hedged_book_stale_ms: None,
            max_hedged_leg_adverse_bps: None,
            hedged_leg_collateral_quote: None,
            allow_partner_step_deadband: false,
            max_hedged_both_blind_ms: None,
        });
        c.turnover_posture = Some(TurnoverPosturePolicy {
            initial: posture,
            max_control_ttl_ms: 10000,
        });
        c
    }

    fn posture_tape(
        c: &MakerFirstConfig,
        posture: TurnoverPosture,
        control: Option<(u64, TurnoverControl)>,
    ) -> Vec<Result<MakerFirstEvent>> {
        let t = c.start_ms;
        let mut events = vec![MakerFirstEvent::TradeFeed {
            at_ms: t,
            leg: 0,
            online: true,
        }];
        for offset in (0..=21000).step_by(100) {
            let mut b = books(c, t + offset, "999.8");
            if let MakerFirstEvent::Book { books, .. } = &mut b {
                for book in books {
                    for level in book.bids.iter_mut().chain(book.asks.iter_mut()) {
                        level.contracts = d("5");
                    }
                }
            }
            events.push(b);
        }
        let buy = posture != TurnoverPosture::ShortFirst;
        events.push(print_against(
            t + 110,
            if buy { HedgeSide::Buy } else { HedgeSide::Sell },
            if buy { "999.7" } else { "1000.3" },
            "0.08",
        ));
        events.push(print_against(
            t + 2410,
            if buy { HedgeSide::Sell } else { HedgeSide::Buy },
            if buy { "1000.3" } else { "999.7" },
            "0.08",
        ));
        if let Some((at_ms, control)) = control {
            events.push(MakerFirstEvent::TurnoverControl { at_ms, control });
        }
        events.push(MakerFirstEvent::End { at_ms: c.end_ms });
        events.sort_by_key(MakerFirstEvent::time);
        events.into_iter().map(Ok).collect()
    }

    #[test]
    fn cycle_cost_diagnostics_preserve_all_three_posture_ledgers() {
        for posture in [
            TurnoverPosture::Neutral,
            TurnoverPosture::LongFirst,
            TurnoverPosture::ShortFirst,
        ] {
            let mut cfg = posture_config(posture);
            let control = run_maker_first(
                &cfg,
                posture_tape(&cfg, posture, None),
                &VenueRegistry::default(),
            )
            .unwrap();
            cfg.diagnose_cycles = true;
            let measured = run_maker_first(
                &cfg,
                posture_tape(&cfg, posture, None),
                &VenueRegistry::default(),
            )
            .unwrap();
            assert_eq!(control.net_pnl, measured.net_pnl);
            assert_eq!(control.maker_turnover, measured.maker_turnover);
            assert_eq!(control.taker_turnover, measured.taker_turnover);
            assert_eq!(control.quotes_submitted, measured.quotes_submitted);
            assert_eq!(control.quote_cancels, measured.quote_cancels);
            let costs = measured.execution_costs.unwrap();
            assert!(costs.whole_ledger_cash_reconciled);
            assert_eq!(costs.whole_ledger_net_before_overhead, measured.net_pnl);
            assert_eq!(costs.cycles.len(), measured.closed_cycles);
            assert!(
                costs
                    .cycles
                    .iter()
                    .all(|c| c.completed && c.cash_reconciled && c.price_decomposition_reconciled)
            );
            assert_eq!(
                costs
                    .cycles
                    .iter()
                    .map(|c| c.net_before_overhead.unwrap())
                    .sum::<Decimal>(),
                measured.net_pnl.unwrap()
            );
        }
    }

    #[test]
    fn turnover_postures_fully_hedge_and_close_both_legs() {
        for posture in [
            TurnoverPosture::Neutral,
            TurnoverPosture::LongFirst,
            TurnoverPosture::ShortFirst,
        ] {
            let cfg = posture_config(posture);
            let r = run_maker_first(
                &cfg,
                posture_tape(&cfg, posture, None),
                &VenueRegistry::default(),
            )
            .unwrap();
            assert_eq!(r.incomplete, None, "{posture:?}: {r:?}");
            assert!(r.closed_cycles > 0, "{r:?}");
            assert_eq!(r.open_positions_base, [Decimal::ZERO; 2]);
            assert_eq!(r.exposure_stops, 0, "{r:?}");
            assert!(r.cycle_attribution.paired_closed_cycles > 0, "{r:?}");
            assert!(r.turnover_posture.cycles_started[posture.index()] > 0);
            assert_eq!(
                r.turnover_posture.cycles_completed[posture.index()],
                r.closed_cycles
            );
            assert_eq!(
                r.turnover_posture.realized_pnl[posture.index()],
                r.realized_pnl
            );
        }
    }

    #[test]
    fn turnover_control_drains_old_fills_and_expires_to_neutral() {
        let cfg = posture_config(TurnoverPosture::LongFirst);
        let t = cfg.start_ms;
        let control = TurnoverControl {
            revision: 1,
            issued_at_ms: t + 160,
            expires_at_ms: t + 1600,
            posture: TurnoverPosture::ShortFirst,
        };
        let r = run_maker_first(
            &cfg,
            posture_tape(&cfg, TurnoverPosture::LongFirst, Some((t + 160, control))),
            &VenueRegistry::default(),
        )
        .unwrap();
        assert_eq!(r.incomplete, None, "{r:?}");
        assert_eq!(r.turnover_posture.controls_accepted, 1);
        assert_eq!(r.turnover_posture.controls_expired, 1);
        assert_eq!(r.turnover_posture.final_posture, TurnoverPosture::Neutral);
        assert!(r.forced_closes > 0);
        assert_eq!(r.open_positions_base, [Decimal::ZERO; 2]);
    }

    #[test]
    fn turnover_control_keeps_late_fill_in_original_posture_cohort() {
        let cfg = posture_config(TurnoverPosture::LongFirst);
        let t = cfg.start_ms;
        // Change before the first fill; the old order can still execute while
        // cancellation travels. Its first late fill starts an old, draining cycle.
        let control = TurnoverControl {
            revision: 1,
            issued_at_ms: t + 60,
            expires_at_ms: t + 1600,
            posture: TurnoverPosture::ShortFirst,
        };
        let r = run_maker_first(
            &cfg,
            posture_tape(&cfg, TurnoverPosture::LongFirst, Some((t + 60, control))),
            &VenueRegistry::default(),
        )
        .unwrap();
        assert_eq!(r.incomplete, None, "{r:?}");
        assert_eq!(r.turnover_posture.cycles_started, [0, 1, 0]);
        assert_eq!(r.turnover_posture.cycles_completed, [0, 1, 0]);
        assert_eq!(r.open_positions_base, [Decimal::ZERO; 2]);
    }

    #[test]
    fn fixed_turnover_posture_preserves_legacy_entry_execution() {
        for (posture, side) in [
            (TurnoverPosture::Neutral, None),
            (TurnoverPosture::LongFirst, Some(HedgeSide::Buy)),
            (TurnoverPosture::ShortFirst, Some(HedgeSide::Sell)),
        ] {
            let cfg = posture_config(posture);
            let mut legacy = posture_config(posture);
            legacy.turnover_posture = None;
            legacy.entry_side = side;
            let execute = |c: &MakerFirstConfig| {
                run_maker_first(c, posture_tape(c, posture, None), &VenueRegistry::default())
                    .unwrap()
            };
            let (new, old) = (execute(&cfg), execute(&legacy));
            assert_eq!(new.net_pnl, old.net_pnl);
            assert_eq!(new.maker_turnover, old.maker_turnover);
            assert_eq!(new.taker_turnover, old.taker_turnover);
            assert_eq!(new.max_unhedged_ms, old.max_unhedged_ms);
            assert_eq!(new.open_positions_base, old.open_positions_base);
        }
    }

    #[test]
    fn turnover_control_rejects_replay_future_and_risk_mutation() {
        let policy = posture_config(TurnoverPosture::Neutral)
            .turnover_posture
            .unwrap();
        let mut state = PostureState::new(Some(policy));
        let c = TurnoverControl {
            revision: 1,
            issued_at_ms: 100,
            expires_at_ms: 500,
            posture: TurnoverPosture::LongFirst,
        };
        assert!(!state.control(policy, 99, c));
        assert!(state.control(policy, 100, c));
        assert!(!state.control(policy, 101, c));
        assert!(!state.control(policy, 500, TurnoverControl { revision: 2, ..c }));
        assert!(!state.control(
            policy,
            200,
            TurnoverControl {
                revision: 2,
                expires_at_ms: 20000,
                ..c
            }
        ));
        assert!(serde_json::from_str::<TurnoverControl>(r#"{"revision":2,"issued_at_ms":100,"expires_at_ms":500,"posture":"long_first","max_directional_notional":"999999"}"#).is_err());
        assert!(state.expire(500));
        assert_eq!(state.posture, TurnoverPosture::Neutral);
        let mut cfg = posture_config(TurnoverPosture::LongFirst);
        cfg.turnover_posture.as_mut().unwrap().max_control_ttl_ms = 0;
        assert!(cfg.validate(&VenueRegistry::default()).is_err());
        cfg.turnover_posture.as_mut().unwrap().max_control_ttl_ms = 10000;
        cfg.pair.mode = PaperMode::PairHedge;
        assert!(cfg.validate(&VenueRegistry::default()).is_err());
    }

    #[test]
    fn basis_reversion_and_patient_hedged_exit_capture_dislocation_blocked_by_static_control() {
        let mut control = config();
        control.end_ms = control.start_ms + 30_000;
        control.pair.mode = PaperMode::PairHedge;
        control.pair.strategy = Strategy::BasisReversion;
        control.pair.limit_buffer_bps = 0.0;
        control.pair.planner.min_net_edge_bps = 1.5;
        control.pair.planner.depth_fraction = 1.0;
        control.maker_fee_bps = Decimal::ZERO;
        control.pair.legs[0].taker_fee_bps = Decimal::ZERO;
        control.pair.legs[1].taker_fee_bps = Decimal::ZERO;
        control.require_net_edge = true;
        control.latency_reserve_bps = d("0.5");
        control.quote_lifetime_ms = 800;
        control.hedged_exit = Some(HedgedExit {
            max_hold_ms: 12_000,
            min_saving_bps: Decimal::ZERO,
            max_adverse_basis_bps: d("20"),
            use_cycle_deadline: false,
            closing_quote_lifetime_ms: Some(8_000),
            patience_ms: 8_000,
            target_cycle_edge_bps: Some(d("1.5")),
            closing_max_levels: 4,
            max_hedged_book_stale_ms: None,
            max_hedged_leg_adverse_bps: None,
            hedged_leg_collateral_quote: None,
            allow_partner_step_deadband: false,
            max_hedged_both_blind_ms: None,
        });

        let mut improved = config();
        improved.end_ms = control.end_ms;
        improved.pair = control.pair.clone();
        improved.maker_fee_bps = Decimal::ZERO;
        improved.require_net_edge = true;
        improved.latency_reserve_bps = d("0.5");
        improved.quote_lifetime_ms = 800;
        improved.hedged_exit = control.hedged_exit;
        improved.basis_reversion = Some(MakerBasisReversion {
            warmup_ms: 5_000,
            half_life_ms: 10_000,
            max_gap_ms: 1_000,
            min_dislocation_bps: d("4.0"),
            cancel_dislocation_bps: d("1.0"),
            reversion_fraction: d("0.75"),
            use_quote_level_dislocation: false,
            max_dislocation_bps: None,
            min_persist_ms: None,
        });
        improved.entry_flow_filter = Some(EntryFlowFilter {
            lookback_ms: 1_000,
            sample_ms: 100,
            max_gap_ms: 1_000,
            max_adverse_move_bps: d("5.0"),
            max_range_bps: d("10.0"),
            cancel_resting_entries: true,
        });

        let start = control.start_ms;
        let pair_book = |at: u64, m_bid: &str, m_ask: &str, m_ask2: &str| {
            let lvl = |p: &str, c: &str| Level {
                price: d(p),
                contracts: d(c),
            };
            MakerFirstEvent::Book {
                at_ms: at,
                books: [
                    Book {
                        market: control.pair.legs[0].market.clone(),
                        timestamp_ms: at,
                        tradable: true,
                        bids: vec![lvl(m_bid, "0.05")],
                        asks: vec![lvl(m_ask, "0.05"), lvl(m_ask2, "0.05")],
                        bounds: None,
                        available_notional: Decimal::from(150),
                    },
                    Book {
                        market: control.pair.legs[1].market.clone(),
                        timestamp_ms: at,
                        tradable: true,
                        bids: vec![lvl("1000.0", "1.0")],
                        asks: vec![lvl("1000.2", "1.0")],
                        bounds: None,
                        available_notional: Decimal::from(150),
                    },
                ],
                reference: None,
                equity_session: None,
                leg_oracles: None,
            }
        };
        let build_tape = || {
            let mut ev = vec![MakerFirstEvent::TradeFeed {
                at_ms: start,
                leg: 0,
                online: true,
            }];
            // 5.5s equilibrium warmup around 1000.1 mid (0 bps basis vs hedge mid 1000.1).
            for dt in (0..=5_500).step_by(500) {
                ev.push(pair_book(start + dt, "1000.0", "1000.2", "1000.5"));
            }
            // Maker dislocates -6 bps cheap (mid 999.5 vs hedge 1000.1) with same 2 bps spread.
            ev.push(pair_book(start + 6_000, "999.4", "999.6", "1000.5"));
            ev.push(pair_book(start + 6_150, "999.4", "999.6", "1000.5"));
            ev.push(MakerFirstEvent::Trade {
                at_ms: start + 6_200,
                source_ms: start + 6_200,
                leg: 0,
                aggressor: HedgeSide::Sell,
                price: d("999.4"),
                contracts: d("0.5"),
            });
            ev.push(pair_book(start + 6_350, "999.4", "999.6", "1000.5"));
            ev.push(pair_book(start + 6_500, "999.4", "999.6", "1000.5"));
            // Deeper closing ask at level[1] = 1000.5 rests across >800ms without TTL churn,
            // then maker reverts and buyer sweeps 1000.5!
            for dt in [7_000, 7_600, 8_200] {
                ev.push(pair_book(start + dt, "999.4", "999.6", "1000.5"));
            }
            ev.push(MakerFirstEvent::Trade {
                at_ms: start + 8_300,
                source_ms: start + 8_300,
                leg: 0,
                aggressor: HedgeSide::Buy,
                price: d("1000.5"),
                contracts: d("0.5"),
            });
            ev.push(pair_book(start + 8_450, "1000.0", "1000.2", "1000.5"));
            ev.push(pair_book(start + 8_600, "1000.0", "1000.2", "1000.5"));
            ev.push(MakerFirstEvent::TradeFeed {
                at_ms: start + 9_000,
                leg: 0,
                online: false,
            });
            ev.push(pair_book(start + 27_000, "1000.0", "1000.2", "1000.5"));
            ev.push(MakerFirstEvent::End {
                at_ms: control.end_ms,
            });
            ev
        };

        let r_ctrl = run_maker_first(
            &control,
            build_tape().into_iter().map(Ok),
            &VenueRegistry::default(),
        )
        .unwrap();
        assert_eq!(
            r_ctrl.closed_cycles, 0,
            "static same-snapshot pair_hedge blocks all entries"
        );

        let r_imp = run_maker_first(
            &improved,
            build_tape().into_iter().map(Ok),
            &VenueRegistry::default(),
        )
        .unwrap();
        assert_eq!(r_imp.incomplete, None, "{r_imp:?}");
        assert_eq!(r_imp.closed_cycles, 1, "{r_imp:?}");
        assert_eq!(r_imp.cycle_attribution.paired_maker_closes, 1);
        assert_eq!(
            r_imp.cycle_attribution.closing_quotes.submitted, 1,
            "8s closing TTL prevented 800ms churn"
        );
        assert!(
            r_imp.net_pnl.unwrap() > Decimal::ZERO,
            "net_pnl={:?}",
            r_imp.net_pnl
        );
        assert!(r_imp.basis_reversion.is_some());
        assert!(r_imp.entry_flow_filter.is_some());
    }

    #[test]
    fn opening_depth_quote_dislocation_and_sub_minimum_fill_aggregation() {
        let mut cfg = config();
        let start = cfg.start_ms;
        cfg.end_ms = start + 60_000;
        cfg.pair.mode = PaperMode::PairHedge;
        cfg.pair.strategy = Strategy::BasisReversion;
        cfg.pair.limit_buffer_bps = 0.0;
        cfg.pair.planner.min_net_edge_bps = 1.5;
        cfg.pair.planner.depth_fraction = 1.0;
        cfg.maker_fee_bps = Decimal::ZERO;
        cfg.pair.legs[0].taker_fee_bps = Decimal::ZERO;
        cfg.pair.legs[1].taker_fee_bps = Decimal::ZERO;
        cfg.pair.legs[0].min_notional = Decimal::from(10);
        cfg.pair.legs[1].min_notional = Decimal::from(10);
        cfg.require_net_edge = true;
        cfg.latency_reserve_bps = d("0.5");
        cfg.quote_lifetime_ms = 5_000;
        cfg.opening_max_levels = 2;
        cfg.aggregate_sub_minimum_fills = true;
        cfg.hedged_exit = Some(HedgedExit {
            max_hold_ms: 8_000,
            min_saving_bps: Decimal::ZERO,
            max_adverse_basis_bps: d("20"),
            use_cycle_deadline: false,
            closing_quote_lifetime_ms: Some(5_000),
            patience_ms: 5_000,
            target_cycle_edge_bps: Some(d("1.0")),
            closing_max_levels: 4,
            max_hedged_book_stale_ms: None,
            max_hedged_leg_adverse_bps: None,
            hedged_leg_collateral_quote: None,
            allow_partner_step_deadband: false,
            max_hedged_both_blind_ms: None,
        });
        cfg.basis_reversion = Some(MakerBasisReversion {
            warmup_ms: 5_000,
            half_life_ms: 5_000,
            max_gap_ms: 1_000,
            min_dislocation_bps: d("4.0"),
            cancel_dislocation_bps: d("1.0"),
            reversion_fraction: d("0.8"),
            use_quote_level_dislocation: true,
            max_dislocation_bps: None,
            min_persist_ms: None,
        });
        let pair_book = |at: u64, m_bid1: &str, m_ask: &str| {
            let lvl = |p: &str, c: &str| Level {
                price: d(p),
                contracts: d(c),
            };
            MakerFirstEvent::Book {
                at_ms: at,
                books: [
                    Book {
                        market: cfg.pair.legs[0].market.clone(),
                        timestamp_ms: at,
                        tradable: true,
                        // Mid is 1000.1 (0 bps mid dislocation vs hedge 1000.1),
                        // while bid[1] at 999.4 has +7 bps quote-level dislocation!
                        bids: vec![lvl(m_bid1, "0.02"), lvl("999.4", "0.05")],
                        asks: vec![lvl(m_ask, "0.05")],
                        bounds: None,
                        available_notional: Decimal::from(150),
                    },
                    Book {
                        market: cfg.pair.legs[1].market.clone(),
                        timestamp_ms: at,
                        tradable: true,
                        bids: vec![lvl("1000.0", "1.0")],
                        asks: vec![lvl("1000.2", "1.0")],
                        bounds: None,
                        available_notional: Decimal::from(150),
                    },
                ],
                reference: None,
                equity_session: None,
                leg_oracles: None,
            }
        };
        let mut ev = vec![MakerFirstEvent::TradeFeed {
            at_ms: start,
            leg: 0,
            online: true,
        }];
        for dt in (0..=5_500).step_by(500) {
            ev.push(pair_book(start + dt, "1000.0", "1000.2"));
        }
        // Quote placed at 5_000, activated at 5_100.
        // First trade fills 0.025 (queue 0.02 + 0.005 partial = $5 < $10 min_notional).
        // With aggregate_sub_minimum_fills, resting quote stays alive for the second partial!
        ev.push(MakerFirstEvent::Trade {
            at_ms: start + 5_700,
            source_ms: start + 5_700,
            leg: 0,
            aggressor: HedgeSide::Sell,
            price: d("999.4"),
            contracts: d("0.025"),
        });
        ev.push(pair_book(start + 5_900, "1000.0", "1000.2"));
        ev.push(MakerFirstEvent::Trade {
            at_ms: start + 6_100,
            source_ms: start + 6_100,
            leg: 0,
            aggressor: HedgeSide::Sell,
            price: d("999.4"),
            contracts: d("0.10"),
        });
        for dt in (6_400..=7_300).step_by(300) {
            ev.push(pair_book(start + dt, "1000.2", "1000.6"));
        }
        ev.push(MakerFirstEvent::Trade {
            at_ms: start + 7_500,
            source_ms: start + 7_500,
            leg: 0,
            aggressor: HedgeSide::Buy,
            price: d("1000.6"),
            contracts: d("0.50"),
        });
        for dt in (7_600..=9_000).step_by(300) {
            ev.push(pair_book(start + dt, "1000.2", "1000.6"));
        }
        ev.push(pair_book(start + 57_000, "1000.0", "1000.2"));
        ev.push(pair_book(start + 58_000, "1000.0", "1000.2"));
        ev.push(MakerFirstEvent::End { at_ms: cfg.end_ms });

        let report =
            run_maker_first(&cfg, ev.into_iter().map(Ok), &VenueRegistry::default()).unwrap();
        assert_eq!(report.incomplete, None, "{report:?}");
        assert_eq!(report.closed_cycles, 1, "{report:?}");
        assert_eq!(
            report.below_minimum_hedge, 0,
            "sub-min partial aggregated cleanly"
        );
        assert!(
            report.net_pnl.unwrap() > Decimal::ZERO,
            "net_pnl={:?}",
            report.net_pnl
        );
    }

    #[test]
    fn passive_offset_adverse_move_stop_triggers_early_hedge() {
        let mut cfg = passive_config(HedgeSide::Buy);
        let t = cfg.start_ms;
        cfg.end_ms = t + 20_000;
        cfg.unhedged_timeout_ms = 5_000;
        cfg.hold_ms = 8_000;
        cfg.passive_offset.as_mut().unwrap().max_wait_ms = 4_500;
        cfg.passive_offset.as_mut().unwrap().max_adverse_move_bps = Some(d("3.0"));

        let adverse_at = |offset: u64| {
            let mut b = books(&cfg, t + offset, "999.8");
            if let MakerFirstEvent::Book { books, .. } = &mut b {
                // Companion mid drops from 1000.3 to 999.8 (-5 bps > 3 bps stop) well before 4.5s wait!
                books[1].bids[0].price = d("999.7");
                books[1].asks[0].price = d("999.9");
            }
            b
        };
        let mut events = vec![
            MakerFirstEvent::TradeFeed {
                at_ms: t,
                leg: 0,
                online: true,
            },
            books(&cfg, t, "999.8"),
            books(&cfg, t + 100, "999.8"),
            print_against(t + 110, HedgeSide::Buy, "999.7", "0.04"),
            books(&cfg, t + 210, "999.8"),
        ];
        for dt in (250..=9_500).step_by(200) {
            events.push(adverse_at(dt));
        }
        events.push(MakerFirstEvent::TradeFeed {
            at_ms: t + 10_000,
            leg: 0,
            online: false,
        });
        events.push(adverse_at(17_000));
        events.push(MakerFirstEvent::End { at_ms: cfg.end_ms });
        let report =
            run_maker_first(&cfg, events.into_iter().map(Ok), &VenueRegistry::default()).unwrap();
        assert_eq!(report.incomplete, None, "{report:?}");
        assert_eq!(report.closed_cycles, 1, "{report:?}");
        assert!(
            report.max_unhedged_ms < 1_000,
            "adverse stop must exit well before 4_500ms max_wait_ms, got {}",
            report.max_unhedged_ms
        );
    }

    #[test]
    fn sub_minimum_lighter_partial_unwinds_cleanly_via_reduce_only_ioc() {
        let mut cfg = config();
        cfg.pair.legs[0].venue = "lighter".into();
        cfg.pair.legs[1].venue = "lighter_robinhood".into();
        cfg.pair.allow_shared_risk_domain = true;
        cfg.pair.legs[0].lot = d("0.0001");
        cfg.pair.legs[0].min_contracts = d("0.002");
        cfg.pair.legs[0].min_notional = Decimal::from(10);
        cfg.pair.legs[1].lot = d("0.0001");
        cfg.pair.legs[1].min_contracts = d("0.002");
        cfg.pair.legs[1].min_notional = Decimal::from(10);
        cfg.pair.limit_buffer_bps = 0.0;
        cfg.maker_fee_bps = Decimal::ZERO;
        cfg.pair.legs[0].taker_fee_bps = Decimal::ZERO;
        cfg.pair.legs[1].taker_fee_bps = Decimal::ZERO;
        cfg.aggregate_sub_minimum_fills = true;
        cfg.unhedged_timeout_ms = 1_500;
        let t = cfg.start_ms;
        // 0.0001 ETH @ $1000 = $0.10 (< $10 opening min_notional on leg 1),
        // so opening hedge on leg 1 is rejected while reduce-only unwind IOC on leg 0 flattens cleanly.
        let mut events = vec![
            MakerFirstEvent::TradeFeed {
                at_ms: t,
                leg: 0,
                online: true,
            },
            books(&cfg, t, "1000"),
            books(&cfg, t + 100, "1000"),
            partial(t + 120, t + 115, "0.1001"), // queue 0.10 + 0.0001 partial fill
        ];
        for dt in (250..=4_500).step_by(200) {
            events.push(books(&cfg, t + dt, "1000"));
        }
        events.push(books(&cfg, t + 21_500, "1000"));
        events.push(MakerFirstEvent::TradeFeed {
            at_ms: t + 22_000,
            leg: 0,
            online: false,
        });
        events.push(MakerFirstEvent::End { at_ms: cfg.end_ms });

        let report =
            run_maker_first(&cfg, events.into_iter().map(Ok), &VenueRegistry::default()).unwrap();
        assert_eq!(report.incomplete, None, "{report:?}");
        assert_eq!(report.open_positions_base, [Decimal::ZERO; 2], "{report:?}");
        assert_eq!(report.below_minimum_unwind, 0, "{report:?}");
        assert_eq!(report.closed_cycles, 1, "{report:?}");
    }

    #[test]
    fn decoupled_hedge_protection_bps_preserves_resting_quote_across_noise() {
        let mut tight = config();
        tight.require_net_edge = true;
        tight.maker_fee_bps = Decimal::ZERO;
        tight.pair.legs[0].taker_fee_bps = Decimal::ZERO;
        tight.pair.legs[1].taker_fee_bps = Decimal::ZERO;
        tight.latency_reserve_bps = d("0.20");
        tight.quote_lifetime_ms = 5_000;

        let mut decoupled = config();
        decoupled.require_net_edge = true;
        decoupled.maker_fee_bps = Decimal::ZERO;
        decoupled.pair.legs[0].taker_fee_bps = Decimal::ZERO;
        decoupled.pair.legs[1].taker_fee_bps = Decimal::ZERO;
        decoupled.latency_reserve_bps = d("0.20");
        decoupled.quote_lifetime_ms = 5_000;
        decoupled.hedge_protection_bps = Some(d("3.00"));

        let t = tight.start_ms;
        // Companion leg moves adversely by 1.0 bps (exceeding 0.20 bps latency_reserve_bps,
        // but well within 3.00 bps hedge_protection_bps) before reverting.
        let level = |price: &str, contracts: &str| Level {
            price: d(price),
            contracts: d(contracts),
        };
        let mk_books =
            |cfg: &MakerFirstConfig, at_ms: u64, h_bid: &str, h_ask: &str| MakerFirstEvent::Book {
                at_ms,
                books: [
                    Book {
                        market: cfg.pair.legs[0].market.clone(),
                        timestamp_ms: at_ms,
                        tradable: true,
                        bids: vec![level("999.8", "10")],
                        asks: vec![level("1000.4", "10")],
                        bounds: None,
                        available_notional: Decimal::from(150),
                    },
                    Book {
                        market: cfg.pair.legs[1].market.clone(),
                        timestamp_ms: at_ms,
                        tradable: true,
                        bids: vec![level(h_bid, "10")],
                        asks: vec![level(h_ask, "10")],
                        bounds: None,
                        available_notional: Decimal::from(150),
                    },
                ],
                reference: None,
                equity_session: None,
                leg_oracles: None,
            };
        let events_for = |cfg: &MakerFirstConfig| {
            vec![
                MakerFirstEvent::TradeFeed {
                    at_ms: t,
                    leg: 0,
                    online: true,
                },
                mk_books(cfg, t, "1001.0", "1001.2"),
                mk_books(cfg, t + 100, "1001.0", "1001.2"),
                mk_books(cfg, t + 200, "1000.8", "1001.0"), // 2.0 bps move (> 0.20 bps, <= 3.00 bps)
                mk_books(cfg, t + 400, "1001.0", "1001.2"),
                mk_books(cfg, t + 1_500, "1001.0", "1001.2"),
                MakerFirstEvent::Trade {
                    at_ms: t + 1_510,
                    source_ms: t + 1_505,
                    leg: 0,
                    aggressor: HedgeSide::Sell,
                    price: d("999.8"),
                    contracts: d("11.0"),
                },
                MakerFirstEvent::End { at_ms: cfg.end_ms },
            ]
        };
        let ev_tight = events_for(&tight);
        let ev_dec = events_for(&decoupled);

        let rep_tight = run_maker_first(
            &tight,
            ev_tight.into_iter().map(Ok),
            &VenueRegistry::default(),
        )
        .unwrap();
        let rep_dec = run_maker_first(
            &decoupled,
            ev_dec.into_iter().map(Ok),
            &VenueRegistry::default(),
        )
        .unwrap();
        assert_eq!(rep_tight.maker_fills, 0, "tight={rep_tight:?}");
        assert_eq!(rep_dec.maker_fills, 1, "decoupled={rep_dec:?}");
    }

    #[test]
    fn max_hedged_book_stale_ms_withdraws_closing_quote_without_forcing_ioc_drain() {
        let mk_cfg = |max_hedged_book_stale_ms: Option<u64>| {
            let mut c = passive_config(HedgeSide::Buy);
            c.passive_offset = None;
            c.pair.limit_buffer_bps = 0.0;
            c.pair.strategy = Strategy::BasisReversion;
            c.pair.legs[0].taker_fee_bps = Decimal::ZERO;
            c.pair.legs[1].taker_fee_bps = Decimal::ZERO;
            c.pair.planner.min_net_edge_bps = 0.0;
            c.pair.planner.max_quote_age_ms = 1_000;
            c.pair.planner.max_quote_skew_ms = 500;
            c.require_net_edge = false;
            c.maker_fee_bps = Decimal::ZERO;
            c.end_ms = c.start_ms + 30_000;
            c.hedged_exit = Some(HedgedExit {
                max_hold_ms: 10_000,
                min_saving_bps: Decimal::ZERO,
                max_adverse_basis_bps: d("20"),
                use_cycle_deadline: false,
                closing_quote_lifetime_ms: Some(4_000),
                patience_ms: 0,
                target_cycle_edge_bps: None,
                closing_max_levels: 1,
                max_hedged_book_stale_ms,
                max_hedged_leg_adverse_bps: None,
                hedged_leg_collateral_quote: None,
                allow_partner_step_deadband: false,
                max_hedged_both_blind_ms: None,
            });
            c
        };
        let strict = mk_cfg(None);
        let tolerant = mk_cfg(Some(5_000));

        let start = strict.start_ms;
        let stale_companion = |cfg: &MakerFirstConfig, at_ms: u64, leg1_ts: u64, tradable: bool| {
            let mut ev = books(cfg, at_ms, "999.8");
            if let MakerFirstEvent::Book {
                books: ref mut b, ..
            } = ev
            {
                b[1].timestamp_ms = leg1_ts;
                b[1].tradable = tradable;
            }
            ev
        };
        let build_events = |cfg: &MakerFirstConfig| {
            vec![
                MakerFirstEvent::TradeFeed {
                    at_ms: start,
                    leg: 0,
                    online: true,
                },
                books(cfg, start, "999.8"),
                books(cfg, start + 100, "999.8"),
                print_against(start + 110, HedgeSide::Buy, "999.7", "0.04"),
                books(cfg, start + 210, "999.8"),
                books(cfg, start + 310, "999.8"), // Paired (`net == 0`)
                // Brief RTT-probe blip (`timestamp_ms = 0, tradable = false`) and 2,000 ms quiet companion book
                stale_companion(cfg, start + 1_200, 0, false),
                stale_companion(cfg, start + 2_350, start + 310, true),
                // Companion refreshes at +2,600 ms; closing Maker quote posts and fills cleanly!
                books(cfg, start + 2_600, "999.8"),
                books(cfg, start + 2_800, "999.8"),
                books(cfg, start + 2_900, "999.8"),
                print_against(start + 2_950, HedgeSide::Sell, "1000.3", "0.04"),
                books(cfg, start + 3_100, "999.8"),
                books(cfg, start + 3_200, "999.8"),
                MakerFirstEvent::TradeFeed {
                    at_ms: start + 3_210,
                    leg: 0,
                    online: false,
                },
                books(cfg, start + 16_000, "999.8"),
                MakerFirstEvent::End { at_ms: cfg.end_ms },
            ]
        };

        let rep_strict = run_maker_first(
            &strict,
            build_events(&strict).into_iter().map(Ok),
            &VenueRegistry::default(),
        )
        .unwrap();
        let rep_tolerant = run_maker_first(
            &tolerant,
            build_events(&tolerant).into_iter().map(Ok),
            &VenueRegistry::default(),
        )
        .unwrap();

        assert_eq!(
            rep_strict.cycle_attribution.hedged_book_stops, 1,
            "{rep_strict:?}"
        );
        assert_eq!(
            rep_strict.cycle_attribution.paired_maker_closes, 0,
            "{rep_strict:?}"
        );
        assert_eq!(
            rep_tolerant.cycle_attribution.hedged_book_stops, 0,
            "{rep_tolerant:?}"
        );
        assert_eq!(
            rep_tolerant.cycle_attribution.paired_maker_closes, 1,
            "{rep_tolerant:?}"
        );
    }

    #[test]
    fn post_only_rejections_and_opening_cancels_partition_cleanly_by_reason() {
        let mut crossed_cfg = passive_config(HedgeSide::Buy);
        crossed_cfg.passive_offset = None;
        crossed_cfg.require_net_edge = false;
        let start = crossed_cfg.start_ms;
        // Submit at start (`maker_bid = 999.8`), then shift book down before arrival (`start + 100`)
        // so `maker_ask` drops to `999.7` -> `quote.price (999.8) >= touch (999.7)` -> crossed!
        let mut crossed_arrival = books(&crossed_cfg, start + 100, "999.5");
        if let MakerFirstEvent::Book {
            books: ref mut b, ..
        } = crossed_arrival
        {
            b[0].asks = vec![Level {
                price: d("999.7"),
                contracts: d("1"),
            }];
        }
        let rep_crossed = run_maker_first(
            &crossed_cfg,
            [
                MakerFirstEvent::TradeFeed {
                    at_ms: start,
                    leg: 0,
                    online: true,
                },
                books(&crossed_cfg, start, "999.8"),
                crossed_arrival,
                MakerFirstEvent::End {
                    at_ms: crossed_cfg.end_ms,
                },
            ]
            .into_iter()
            .map(Ok),
            &VenueRegistry::default(),
        )
        .unwrap();
        assert!(
            rep_crossed.post_only_rejected_crossed > 0,
            "{rep_crossed:?}"
        );
        assert_eq!(
            rep_crossed.post_only_rejected,
            rep_crossed.post_only_rejected_crossed
                + rep_crossed.post_only_rejected_beyond_depth
                + rep_crossed.post_only_rejected_new_level
        );

        // Shift book UP before arrival so `maker_bid` jumps to `1005.0` (`worst` bid is `1005.0`)
        // while `quote.price` is `999.8` -> deeper than displayed depth (`beyond_depth`)!
        let mut beyond_arrival = books(&crossed_cfg, start + 100, "1005.0");
        if let MakerFirstEvent::Book {
            books: ref mut b, ..
        } = beyond_arrival
        {
            b[0].asks = vec![Level {
                price: d("1005.2"),
                contracts: d("1"),
            }];
        }
        let rep_beyond = run_maker_first(
            &crossed_cfg,
            [
                MakerFirstEvent::TradeFeed {
                    at_ms: start,
                    leg: 0,
                    online: true,
                },
                books(&crossed_cfg, start, "999.8"),
                beyond_arrival,
                MakerFirstEvent::End {
                    at_ms: crossed_cfg.end_ms,
                },
            ]
            .into_iter()
            .map(Ok),
            &VenueRegistry::default(),
        )
        .unwrap();
        assert!(
            rep_beyond.post_only_rejected_beyond_depth > 0,
            "{rep_beyond:?}"
        );
        assert_eq!(
            rep_beyond.post_only_rejected,
            rep_beyond.post_only_rejected_crossed
                + rep_beyond.post_only_rejected_beyond_depth
                + rep_beyond.post_only_rejected_new_level
        );

        // Opening quote TTL cancellation populates `opening_cancel_requested["quote_ttl"]`.
        crossed_cfg.quote_lifetime_ms = 250;
        let rep_ttl = run_maker_first(
            &crossed_cfg,
            [
                MakerFirstEvent::TradeFeed {
                    at_ms: start,
                    leg: 0,
                    online: true,
                },
                books(&crossed_cfg, start, "999.8"),
                books(&crossed_cfg, start + 100, "999.8"),
                books(&crossed_cfg, start + 400, "999.8"),
                MakerFirstEvent::End {
                    at_ms: crossed_cfg.end_ms,
                },
            ]
            .into_iter()
            .map(Ok),
            &VenueRegistry::default(),
        )
        .unwrap();
        assert_eq!(
            rep_ttl.opening_cancel_requested.get("quote_ttl").copied(),
            Some(1),
            "{rep_ttl:?}"
        );
    }

    #[test]
    fn maker_create_delay_ms_enforces_robinhood_standard_200_200_300_latency_profile() {
        let mut cfg = config();
        cfg.transport_ms = 10;
        cfg.maker_create_delay_ms = 200;
        cfg.maker_cancel_delay_ms = 200;
        cfg.taker_delay_ms = [300, 300];
        cfg.quote_lifetime_ms = 1_000;
        let start = cfg.start_ms;

        // Quote submitted at `start` arrives at `start + 10 + 200 = start + 210`.
        // A trade print at `start + 120` (after `transport_ms = 10`, but BEFORE `210`)
        // must NOT fill the unactivated Maker quote; once activated at `start + 210`,
        // a trade print at `start + 250` fills it cleanly and routes the `300 ms` Taker hedge!
        let rep = run_maker_first(
            &cfg,
            [
                MakerFirstEvent::TradeFeed {
                    at_ms: start,
                    leg: 0,
                    online: true,
                },
                books(&cfg, start, "999.8"),
                books(&cfg, start + 50, "999.8"),
                print_against(start + 120, HedgeSide::Buy, "999.7", "0.04"),
                books(&cfg, start + 210, "999.8"),
                print_against(start + 250, HedgeSide::Buy, "999.7", "0.04"),
                // Fill notice at `250 + 100 = 350` submits Taker hedge arriving at `350 + 10 + 300 = 660`.
                books(&cfg, start + 350, "999.8"),
                books(&cfg, start + 660, "999.8"),
                books(&cfg, start + 2_500, "999.8"),
                books(&cfg, start + 3_000, "999.8"),
                MakerFirstEvent::TradeFeed {
                    at_ms: start + 3_100,
                    leg: 0,
                    online: false,
                },
                books(&cfg, start + 3_500, "999.8"),
                MakerFirstEvent::End { at_ms: cfg.end_ms },
            ]
            .into_iter()
            .map(Ok),
            &VenueRegistry::default(),
        )
        .unwrap();
        assert_eq!(rep.quotes_accepted, 1, "{rep:?}");
        assert_eq!(rep.maker_fills, 1, "{rep:?}");
        assert!(rep.ioc_fills >= 1, "{rep:?}");
    }

    #[test]
    fn max_hedged_book_stale_ms_aborts_on_single_leg_margin_breach_or_dual_blindness() {
        let mut cfg = passive_config(HedgeSide::Buy);
        cfg.passive_offset = None;
        cfg.pair.limit_buffer_bps = 0.0;
        cfg.pair.strategy = Strategy::BasisReversion;
        cfg.pair.legs[0].taker_fee_bps = Decimal::ZERO;
        cfg.pair.legs[1].taker_fee_bps = Decimal::ZERO;
        cfg.pair.planner.min_net_edge_bps = 0.0;
        cfg.pair.planner.max_quote_age_ms = 1_000;
        cfg.pair.planner.max_quote_skew_ms = 500;
        cfg.require_net_edge = false;
        cfg.maker_fee_bps = Decimal::ZERO;
        cfg.end_ms = cfg.start_ms + 30_000;
        cfg.hedged_exit = Some(HedgedExit {
            max_hold_ms: 15_000,
            min_saving_bps: Decimal::ZERO,
            max_adverse_basis_bps: d("20"),
            use_cycle_deadline: false,
            closing_quote_lifetime_ms: Some(4_000),
            patience_ms: 0,
            target_cycle_edge_bps: None,
            closing_max_levels: 1,
            max_hedged_book_stale_ms: Some(150_000),
            max_hedged_leg_adverse_bps: Some(d("500")),
            // 0.04 base @ ~$1000 = $40 notional; $2.00 isolated collateral -> 20x leverage!
            // A +3.0% (+300 bps) rally on fresh Leg 0 implies -$1.20 loss on Short Leg 1 -> $0.80 equity
            // vs $41.20 * 2% = $0.824 MM (< 1.8x MM!), forcing immediate hedged book stop!
            hedged_leg_collateral_quote: Some(d("2.0")),
            allow_partner_step_deadband: false,
            max_hedged_both_blind_ms: None,
        });
        let start = cfg.start_ms;
        let mut stale_leg1_rally = books(&cfg, start + 2_000, "1030.0");
        if let MakerFirstEvent::Book {
            books: ref mut b, ..
        } = stale_leg1_rally
        {
            b[0].asks = vec![Level {
                price: d("1030.4"),
                contracts: d("1"),
            }];
            b[1].timestamp_ms = start + 310; // Leg 1 stale > 1,000ms while Leg 0 rallies +3%!
        }
        let rep = run_maker_first(
            &cfg,
            [
                MakerFirstEvent::TradeFeed {
                    at_ms: start,
                    leg: 0,
                    online: true,
                },
                books(&cfg, start, "999.8"),
                books(&cfg, start + 100, "999.8"),
                print_against(start + 110, HedgeSide::Buy, "999.7", "0.04"),
                books(&cfg, start + 210, "999.8"),
                books(&cfg, start + 310, "999.8"),
                stale_leg1_rally,
                books(&cfg, start + 2_200, "1030.0"),
                books(&cfg, start + 2_500, "1030.0"),
                MakerFirstEvent::TradeFeed {
                    at_ms: start + 2_600,
                    leg: 0,
                    online: false,
                },
                books(&cfg, start + 18_000, "1030.0"),
                MakerFirstEvent::End { at_ms: cfg.end_ms },
            ]
            .into_iter()
            .map(Ok),
            &VenueRegistry::default(),
        )
        .unwrap();
        assert!(
            rep.cycle_attribution.hedged_book_stops >= 1 || rep.forced_closes >= 1,
            "150s bad-book tolerance must abort immediately when isolated leg margin breaches 1.8x MM: {rep:?}"
        );
    }

    #[test]
    fn arcus_sub_minimum_partial_requotes_and_aggregates_cleanly() {
        let mut cfg = config();
        cfg.pair.strategy = Strategy::BasisReversion;
        cfg.pair.legs[0].venue = "arcus".into();
        cfg.pair.legs[0].market = "ETH-USD".into();
        cfg.pair.legs[0].lot = d("0.0000001");
        cfg.pair.legs[0].min_contracts = d("0.01");
        cfg.pair.legs[0].min_notional = Decimal::from(5);
        cfg.pair.legs[0].taker_fee_bps = d("2.25");
        cfg.pair.legs[1].venue = "trade_xyz".into();
        cfg.pair.legs[1].market = "xyz:ETH".into();
        cfg.pair.legs[1].lot = d("0.001");
        cfg.pair.legs[1].min_contracts = d("0.01");
        cfg.pair.legs[1].min_notional = Decimal::from(10);
        cfg.pair.legs[1].taker_fee_bps = d("0.90");
        cfg.pair.limit_buffer_bps = 1.0;
        cfg.pair.planner.max_quote_age_ms = 500;
        cfg.pair.planner.max_quote_skew_ms = 300;
        cfg.maker_fee_bps = Decimal::ZERO;
        cfg.require_net_edge = false;
        cfg.aggregate_sub_minimum_fills = true;
        cfg.quote_lifetime_ms = 400;
        cfg.unhedged_timeout_ms = 5_000;
        cfg.hold_ms = 8_000;
        cfg.diagnose_cycles = true;
        let t = cfg.start_ms;

        // Quote #1 activates at t+100 (target 0.04 base @ $1000 = $40).
        // At t+120, queue 0.10 + 0.0000834 partial fills (0.0000834 < 0.01 min_contracts).
        // Companion leg 1 emits a real empty disconnect book (`bids = [], asks = [], tradable = false`)
        // at t+220 (confirm notice arrives!), then Quote #1 expires at t+500. At t+700 books sync again
        // -> Quote #2 posts exact remaining 0.0399166 base, fills at t+850 -> total 0.040 hedges cleanly!
        let mut stale_leg1 = books(&cfg, t + 220, "1000");
        if let MakerFirstEvent::Book {
            books: ref mut b, ..
        } = stale_leg1
        {
            b[1].timestamp_ms = 0;
            b[1].tradable = false;
            b[1].bids.clear();
            b[1].asks.clear();
        }
        let mut events = vec![
            MakerFirstEvent::TradeFeed {
                at_ms: t,
                leg: 0,
                online: true,
            },
            books(&cfg, t, "1000"),
            books(&cfg, t + 100, "1000"),
            partial(t + 120, t + 115, "0.1000834"),
            stale_leg1,
            books(&cfg, t + 550, "1000"),
            books(&cfg, t + 700, "1000"),
            books(&cfg, t + 820, "1000"),
            partial(t + 850, t + 845, "0.20"),
        ];
        for dt in (950..=12_000).step_by(200) {
            events.push(books(&cfg, t + dt, "1000"));
        }
        events.push(books(&cfg, t + 23_500, "1000"));
        events.push(books(&cfg, t + 24_500, "1000"));
        events.push(MakerFirstEvent::End { at_ms: cfg.end_ms });

        let rep =
            run_maker_first(&cfg, events.into_iter().map(Ok), &VenueRegistry::default()).unwrap();
        assert_eq!(rep.incomplete, None, "{rep:?}");
        assert_eq!(rep.closed_cycles, 1, "{rep:?}");
        assert_eq!(rep.open_positions_base, [Decimal::ZERO, Decimal::ZERO]);
        assert!(rep.below_minimum_hedge <= 1, "{rep:?}");
        assert_eq!(rep.below_minimum_unwind, 0, "{rep:?}");
        assert!(
            rep.execution_costs
                .as_ref()
                .unwrap()
                .whole_ledger_cash_reconciled
        );
    }

    #[test]
    fn arcus_sub_minimum_stub_at_drain_unwinds_via_fok_top_up_then_reduce_only() {
        let mut cfg = config();
        cfg.pair.strategy = Strategy::BasisReversion;
        cfg.pair.legs[0].venue = "arcus".into();
        cfg.pair.legs[0].market = "ETH-USD".into();
        cfg.pair.legs[0].lot = d("0.0000001");
        cfg.pair.legs[0].min_contracts = d("0.01");
        cfg.pair.legs[0].min_notional = Decimal::from(5);
        cfg.pair.legs[0].taker_fee_bps = d("2.25");
        cfg.pair.legs[1].venue = "trade_xyz".into();
        cfg.pair.legs[1].market = "xyz:ETH".into();
        cfg.pair.legs[1].lot = d("0.001");
        cfg.pair.legs[1].min_contracts = d("0.01");
        cfg.pair.legs[1].min_notional = Decimal::from(10);
        cfg.pair.legs[1].taker_fee_bps = d("0.90");
        cfg.pair.limit_buffer_bps = 1.0;
        cfg.maker_fee_bps = Decimal::ZERO;
        cfg.pair.planner.min_net_edge_bps = 0.0;
        cfg.require_net_edge = true;
        cfg.aggregate_sub_minimum_fills = true;
        cfg.allow_sub_minimum_top_up_unwind = true;
        cfg.pair.mode = PaperMode::Turnover {
            max_turnover_quote: d("1000"),
            max_fees_quote: d("10"),
            max_realized_loss_quote: Some(d("10")),
            incentive: Some(IncentiveTurnover {
                eligible_leg: 0,
                eligible_from_ms: cfg.start_ms,
                eligible_until_ms: cfg.end_ms,
                max_cost_bps: d("50"),
            }),
        };
        cfg.unhedged_timeout_ms = 1_500;
        cfg.hold_ms = 3_000;
        cfg.diagnose_cycles = true;
        let t = cfg.start_ms;

        // Only a tiny 0.0000834 stub ($0.0834) fills, followed by empty disconnect frames on both legs
        // at t+250 before books recover at t+450; no second passive print arrives before timeout!
        let mut empty_books = books(&cfg, t + 250, "1000");
        if let MakerFirstEvent::Book {
            books: ref mut b, ..
        } = empty_books
        {
            for leg_b in b.iter_mut() {
                leg_b.timestamp_ms = 0;
                leg_b.tradable = false;
                leg_b.bids.clear();
                leg_b.asks.clear();
            }
        }
        let mut events = vec![
            MakerFirstEvent::TradeFeed {
                at_ms: t,
                leg: 0,
                online: true,
            },
            books(&cfg, t, "1000"),
            books(&cfg, t + 100, "1000"),
            partial(t + 120, t + 115, "0.1000834"),
            empty_books,
        ];
        for dt in (450..=8_000).step_by(200) {
            events.push(books(&cfg, t + dt, "1000"));
        }
        events.push(books(&cfg, t + 23_500, "1000"));
        events.push(MakerFirstEvent::End { at_ms: cfg.end_ms });

        let rep =
            run_maker_first(&cfg, events.into_iter().map(Ok), &VenueRegistry::default()).unwrap();
        assert_eq!(rep.incomplete, None, "{rep:?}");
        assert_eq!(rep.closed_cycles, 1, "{rep:?}");
        assert_eq!(rep.open_positions_base, [Decimal::ZERO, Decimal::ZERO]);
        assert_eq!(rep.below_minimum_unwind, 0, "{rep:?}");
        assert_eq!(rep.cycle_attribution.sub_min_top_up_attempts, 1, "{rep:?}");
        assert_eq!(rep.cycle_attribution.sub_min_top_up_failures, 0, "{rep:?}");
        assert_eq!(rep.cycle_attribution.sub_min_top_up_unwinds, 1, "{rep:?}");
        assert!(
            rep.cycle_attribution.sub_min_recovery_turnover[0] > Decimal::from(20),
            "{rep:?}"
        );
        assert!(
            rep.incentive_turnover
                .as_ref()
                .unwrap()
                .filled_turnover_quote
                < Decimal::ONE,
            "recovery turnover leaked into eligible_turnover: {:?}",
            rep.incentive_turnover
        );
        assert!(
            rep.execution_costs
                .as_ref()
                .unwrap()
                .whole_ledger_cash_reconciled
        );
    }

    #[test]
    fn sub_min_top_up_fok_depth_miss_consumes_single_attempt_budget() {
        let mut cfg = config();
        cfg.pair.strategy = Strategy::BasisReversion;
        cfg.pair.legs[0].venue = "arcus".into();
        cfg.pair.legs[0].market = "ETH-USD".into();
        cfg.pair.legs[0].lot = d("0.0000001");
        cfg.pair.legs[0].min_contracts = d("0.01");
        cfg.pair.legs[0].min_notional = Decimal::from(5);
        cfg.pair.legs[0].taker_fee_bps = d("2.25");
        cfg.pair.legs[1].venue = "trade_xyz".into();
        cfg.pair.legs[1].market = "xyz:ETH".into();
        cfg.pair.legs[1].lot = d("0.001");
        cfg.pair.legs[1].min_contracts = d("0.01");
        cfg.pair.legs[1].min_notional = Decimal::from(10);
        cfg.pair.legs[1].taker_fee_bps = d("0.90");
        cfg.pair.limit_buffer_bps = 1.0;
        cfg.maker_fee_bps = Decimal::ZERO;
        cfg.pair.planner.min_net_edge_bps = 0.0;
        cfg.require_net_edge = true;
        cfg.aggregate_sub_minimum_fills = true;
        cfg.allow_sub_minimum_top_up_unwind = true;
        cfg.pair.mode = PaperMode::Turnover {
            max_turnover_quote: d("1000"),
            max_fees_quote: d("10"),
            max_realized_loss_quote: Some(d("10")),
            incentive: Some(IncentiveTurnover {
                eligible_leg: 0,
                eligible_from_ms: cfg.start_ms,
                eligible_until_ms: cfg.end_ms,
                max_cost_bps: d("50"),
            }),
        };
        cfg.unhedged_timeout_ms = 1_500;
        cfg.hold_ms = 3_000;
        let t = cfg.start_ms;

        // Drain triggers at t+1620; continuation Quote #2 (posted at t+1050 during sub_min_opening)
        // cancels via drain at t+1650 (cancelled_ms=t+1850, last_receipt=t+2000), so Quote #2 clears
        // and FOK top-up submits at t+2050 (arrival t+2150).
        // At t+2250 bid depth shrinks to 0.002 so FOK misses depth; even after deep
        // liquidity returns from t+2450..8000, the 1-attempt budget blocks any retry.
        let mut thin_arrival = books(&cfg, t + 2_250, "1000");
        if let MakerFirstEvent::Book {
            books: ref mut b, ..
        } = thin_arrival
        {
            for lvl in &mut b[0].bids {
                lvl.contracts = d("0.002");
            }
        }
        let mut events = vec![
            MakerFirstEvent::TradeFeed {
                at_ms: t,
                leg: 0,
                online: true,
            },
            books(&cfg, t, "1000"),
            books(&cfg, t + 100, "1000"),
            partial(t + 120, t + 115, "0.1000834"),
        ];
        for dt in (450..=2_050).step_by(200) {
            events.push(books(&cfg, t + dt, "1000"));
        }
        events.push(thin_arrival);
        for dt in (2_450..=8_000).step_by(200) {
            events.push(books(&cfg, t + dt, "1000"));
        }
        events.push(books(&cfg, t + 23_500, "1000"));
        events.push(MakerFirstEvent::End { at_ms: cfg.end_ms });

        let rep =
            run_maker_first(&cfg, events.into_iter().map(Ok), &VenueRegistry::default()).unwrap();
        assert_eq!(rep.cycle_attribution.sub_min_top_up_attempts, 1, "{rep:?}");
        assert_eq!(rep.cycle_attribution.sub_min_top_up_failures, 1, "{rep:?}");
        assert_eq!(rep.cycle_attribution.sub_min_top_up_unwinds, 0, "{rep:?}");
        assert_eq!(
            rep.ioc_submitted, 1,
            "must not retry top-up after failure: {rep:?}"
        );
        assert_eq!(rep.incomplete, Some("residual_at_cutoff"), "{rep:?}");
    }

    #[test]
    fn sub_min_top_up_refused_when_leg2_cannot_arrive_before_window_deadline() {
        let mut cfg = config();
        cfg.pair.strategy = Strategy::BasisReversion;
        cfg.pair.legs[0].venue = "arcus".into();
        cfg.pair.legs[0].market = "ETH-USD".into();
        cfg.pair.legs[0].lot = d("0.0000001");
        cfg.pair.legs[0].min_contracts = d("0.01");
        cfg.pair.legs[0].min_notional = Decimal::from(5);
        cfg.pair.legs[0].taker_fee_bps = d("2.25");
        cfg.pair.legs[1].venue = "trade_xyz".into();
        cfg.pair.legs[1].market = "xyz:ETH".into();
        cfg.pair.legs[1].lot = d("0.001");
        cfg.pair.legs[1].min_contracts = d("0.01");
        cfg.pair.legs[1].min_notional = Decimal::from(10);
        cfg.pair.legs[1].taker_fee_bps = d("0.90");
        cfg.pair.limit_buffer_bps = 1.0;
        cfg.maker_fee_bps = Decimal::ZERO;
        cfg.pair.planner.min_net_edge_bps = 0.0;
        cfg.require_net_edge = true;
        cfg.aggregate_sub_minimum_fills = true;
        cfg.allow_sub_minimum_top_up_unwind = true;
        cfg.pair.mode = PaperMode::Turnover {
            max_turnover_quote: d("1000"),
            max_fees_quote: d("10"),
            max_realized_loss_quote: Some(d("10")),
            incentive: Some(IncentiveTurnover {
                eligible_leg: 0,
                eligible_from_ms: cfg.start_ms,
                eligible_until_ms: cfg.end_ms,
                max_cost_bps: d("50"),
            }),
        };
        cfg.taker_delay_ms = [600, 600];
        cfg.reprice_ms = 600;
        cfg.drain_ms = 4_000;
        cfg.unhedged_timeout_ms = 5_000;
        cfg.hold_ms = 8_000;
        let t = cfg.start_ms;

        // Resting quote expires at t+1000 and clears by t+1350; first post-drain fresh book
        // arrives at end_ms - 1200, leaving only 1200ms while leg1 (700ms) + leg2 (700ms) requires 1400ms!
        let events = vec![
            MakerFirstEvent::TradeFeed {
                at_ms: t,
                leg: 0,
                online: true,
            },
            books(&cfg, t, "1000"),
            books(&cfg, t + 100, "1000"),
            partial(t + 120, t + 115, "0.1000834"),
            books(&cfg, t + 1_000, "1000"),
            books(&cfg, t + 1_400, "1000"),
            books(&cfg, cfg.end_ms - 1_200, "1000"),
            books(&cfg, cfg.end_ms - 300, "1000"),
            MakerFirstEvent::End { at_ms: cfg.end_ms },
        ];

        let rep =
            run_maker_first(&cfg, events.into_iter().map(Ok), &VenueRegistry::default()).unwrap();
        assert_eq!(rep.cycle_attribution.sub_min_top_up_attempts, 1, "{rep:?}");
        assert_eq!(rep.cycle_attribution.sub_min_top_up_failures, 1, "{rep:?}");
        assert_eq!(
            rep.ioc_submitted, 0,
            "must refuse leg1 flip without leg2 arrival time: {rep:?}"
        );
        assert_eq!(rep.open_positions_base[0], d("0.0000834"), "{rep:?}");
        assert_eq!(rep.incomplete, Some("residual_at_cutoff"));
    }

    #[test]
    fn sub_min_top_up_leg2_partial_fill_and_vanished_depth_preserves_unsettled_and_separates_turnover()
     {
        let mut cfg = config();
        cfg.pair.strategy = Strategy::BasisReversion;
        cfg.pair.legs[0].venue = "arcus".into();
        cfg.pair.legs[0].market = "ETH-USD".into();
        cfg.pair.legs[0].lot = d("0.0000001");
        cfg.pair.legs[0].min_contracts = d("0.01");
        cfg.pair.legs[0].min_notional = Decimal::from(5);
        cfg.pair.legs[0].taker_fee_bps = d("2.25");
        cfg.pair.legs[1].venue = "trade_xyz".into();
        cfg.pair.legs[1].market = "xyz:ETH".into();
        cfg.pair.legs[1].lot = d("0.001");
        cfg.pair.legs[1].min_contracts = d("0.01");
        cfg.pair.legs[1].min_notional = Decimal::from(10);
        cfg.pair.legs[1].taker_fee_bps = d("0.90");
        cfg.pair.limit_buffer_bps = 1.0;
        cfg.maker_fee_bps = Decimal::ZERO;
        cfg.pair.planner.min_net_edge_bps = 0.0;
        cfg.require_net_edge = true;
        cfg.aggregate_sub_minimum_fills = true;
        cfg.allow_sub_minimum_top_up_unwind = true;
        cfg.diagnose_cycles = true;
        cfg.pair.mode = PaperMode::Turnover {
            max_turnover_quote: d("1000"),
            max_fees_quote: d("10"),
            max_realized_loss_quote: Some(d("10")),
            incentive: Some(IncentiveTurnover {
                eligible_leg: 0,
                eligible_from_ms: cfg.start_ms,
                eligible_until_ms: cfg.end_ms,
                max_cost_bps: d("50"),
            }),
        };
        cfg.unhedged_timeout_ms = 1_500;
        cfg.hold_ms = 3_000;
        let t = cfg.start_ms;

        // Continuation Quote #2 cancels via drain at t+1650 (last_receipt=t+2000); Leg 1 FOK
        // submits at t+2050 and fills at t+2250 flipping +0.0000834 -> -0.0100;
        // then depth vanishes at t+2350 before Leg 2 arrives so residual stays unsettled.
        let mut vanished = books(&cfg, t + 2_350, "1000");
        if let MakerFirstEvent::Book {
            books: ref mut b, ..
        } = vanished
        {
            b[0].tradable = false;
            b[0].bids.clear();
            b[0].asks.clear();
        }
        let mut events = vec![
            MakerFirstEvent::TradeFeed {
                at_ms: t,
                leg: 0,
                online: true,
            },
            books(&cfg, t, "1000"),
            books(&cfg, t + 100, "1000"),
            partial(t + 120, t + 115, "0.1000834"),
        ];
        for dt in (450..=2_250).step_by(200) {
            events.push(books(&cfg, t + dt, "1000"));
        }
        events.push(vanished);
        events.push(MakerFirstEvent::End { at_ms: cfg.end_ms });

        let rep =
            run_maker_first(&cfg, events.into_iter().map(Ok), &VenueRegistry::default()).unwrap();
        assert_eq!(rep.incomplete, Some("residual_at_cutoff"), "{rep:?}");
        assert_eq!(rep.closed_cycles, 0, "{rep:?}");
        assert!(rep.open_positions_base[0] < Decimal::ZERO, "{rep:?}");
        assert_eq!(rep.cycle_attribution.sub_min_top_up_unwinds, 1, "{rep:?}");
        assert!(
            rep.cycle_attribution.sub_min_recovery_turnover[0] > Decimal::from(9),
            "{rep:?}"
        );
        assert!(
            rep.incentive_turnover
                .as_ref()
                .unwrap()
                .filled_turnover_quote
                < Decimal::ONE,
            "{rep:?}"
        );
    }

    #[test]
    fn sub_min_opening_enforces_entry_edge_and_continuous_resting_cancel_protection() {
        let mut cfg = config();
        cfg.pair.strategy = Strategy::BasisReversion;
        cfg.pair.legs[0].venue = "arcus".into();
        cfg.pair.legs[0].market = "ETH-USD".into();
        cfg.pair.legs[0].lot = d("0.0000001");
        cfg.pair.legs[0].min_contracts = d("0.01");
        cfg.pair.legs[0].min_notional = Decimal::from(5);
        cfg.pair.legs[0].taker_fee_bps = Decimal::ZERO;
        cfg.pair.legs[1].venue = "trade_xyz".into();
        cfg.pair.legs[1].market = "xyz:ETH".into();
        cfg.pair.legs[1].lot = d("0.001");
        cfg.pair.legs[1].min_contracts = d("0.01");
        cfg.pair.legs[1].min_notional = Decimal::from(10);
        cfg.pair.legs[1].taker_fee_bps = Decimal::ZERO;
        cfg.pair.limit_buffer_bps = 1.0;
        cfg.maker_fee_bps = d("-5"); // 5 bps maker rebate yields positive opening_edge on tight books!
        cfg.pair.planner.min_net_edge_bps = 2.0;
        cfg.require_net_edge = true;
        cfg.aggregate_sub_minimum_fills = true;
        cfg.quote_lifetime_ms = 500;
        cfg.unhedged_timeout_ms = 10_000;
        cfg.hold_ms = 15_000;
        let t = cfg.start_ms;

        let wide_adverse = |ts: u64| {
            let mut ev = books(&cfg, ts, "1000");
            if let MakerFirstEvent::Book {
                books: ref mut b, ..
            } = ev
            {
                for lvl in &mut b[1].bids {
                    lvl.price = d("990");
                }
                for lvl in &mut b[1].asks {
                    lvl.price = d("1010");
                }
            }
            ev
        };

        let events = vec![
            MakerFirstEvent::TradeFeed {
                at_ms: t,
                leg: 0,
                online: true,
            },
            books(&cfg, t, "1000"),
            books(&cfg, t + 100, "1000"),
            partial(t + 120, t + 115, "0.1000834"),
            wide_adverse(t + 200), // adverse hedge move triggers resting hedge_protection cancel!
            wide_adverse(t + 800), // continuation re-quote attempt must be blocked by min_net_edge!
            wide_adverse(t + 1_200),
            MakerFirstEvent::End { at_ms: cfg.end_ms },
        ];

        let rep =
            run_maker_first(&cfg, events.into_iter().map(Ok), &VenueRegistry::default()).unwrap();
        assert_eq!(
            rep.quotes_submitted, 1,
            "continuation quote while self.cycle.is_some() must not bypass opening_edge: {rep:?}"
        );
        assert!(
            rep.opening_cancel_requested
                .contains_key("hedge_protection"),
            "resting quote during sub_min_opening must retain hedge_protection cancel: {rep:?}"
        );
        assert!(
            rep.entry_blocked.contains_key("entry_net_edge"),
            "continuation quote must still enforce min_net_edge_bps: {rep:?}"
        );
    }

    #[test]
    fn partner_step_deadband_eliminates_false_exposure_stops_and_closes_both_legs_flat() {
        let mk_cfg = |allow_partner_step_deadband: bool| {
            let mut cfg = config();
            cfg.pair.strategy = Strategy::BasisReversion;
            cfg.pair.legs[0].venue = "arcus".into();
            cfg.pair.legs[0].market = "BTC-USD".into();
            cfg.pair.legs[0].lot = d("0.00000001");
            cfg.pair.legs[0].min_contracts = d("0.0001");
            cfg.pair.legs[0].min_notional = Decimal::from(10);
            cfg.pair.legs[0].taker_fee_bps = d("3.5");
            cfg.pair.legs[1].venue = "lighter_robinhood".into();
            cfg.pair.legs[1].market = "BTC".into();
            cfg.pair.legs[1].lot = d("0.00001");
            cfg.pair.legs[1].min_contracts = d("0.00001");
            cfg.pair.legs[1].min_notional = Decimal::from(10);
            cfg.pair.legs[1].taker_fee_bps = Decimal::ZERO;
            cfg.pair.limit_buffer_bps = 0.0;
            cfg.pair.planner.min_net_edge_bps = 0.0;
            cfg.pair.planner.depth_fraction = 1.0;
            cfg.pair.planner.max_base_quantity = 1.0;
            cfg.pair.planner.max_leg_notional_usd = 500.0;
            cfg.pair.planner.max_unhedged_notional_usd = 50.0;
            cfg.pair.mode = PaperMode::Turnover {
                max_turnover_quote: Decimal::from(10_000),
                max_fees_quote: Decimal::from(100),
                max_realized_loss_quote: Some(Decimal::from(50)),
                incentive: Some(IncentiveTurnover {
                    eligible_leg: 0,
                    eligible_from_ms: cfg.start_ms,
                    eligible_until_ms: cfg.start_ms + 30_000,
                    max_cost_bps: Decimal::from(50),
                }),
            };
            cfg.require_net_edge = true;
            cfg.maker_fee_bps = d("0.5");
            cfg.quote_notional = Decimal::from(110);
            cfg.unhedged_timeout_ms = 2_500;
            cfg.hold_ms = 12_000;
            cfg.end_ms = cfg.start_ms + 30_000;
            cfg.diagnose_cycles = true;
            cfg.hedged_exit = Some(HedgedExit {
                max_hold_ms: 10_000,
                min_saving_bps: Decimal::ZERO,
                max_adverse_basis_bps: d("25"),
                use_cycle_deadline: false,
                closing_quote_lifetime_ms: Some(6_000),
                patience_ms: 0,
                target_cycle_edge_bps: None,
                closing_max_levels: 1,
                max_hedged_book_stale_ms: None,
                max_hedged_leg_adverse_bps: None,
                hedged_leg_collateral_quote: None,
                allow_partner_step_deadband,
                max_hedged_both_blind_ms: None,
            });
            cfg
        };

        let control = mk_cfg(false);
        let deadband = mk_cfg(true);
        let start = control.start_ms;
        let lvl = |p: &str, c: &str| Level {
            price: d(p),
            contracts: d(c),
        };
        let mk_book = |at_ms: u64| MakerFirstEvent::Book {
            at_ms,
            books: [
                Book {
                    market: "BTC-USD".into(),
                    timestamp_ms: at_ms,
                    tradable: true,
                    bids: vec![lvl("68000.0", "0.0001"), lvl("67999.0", "1.0")],
                    asks: vec![lvl("68002.0", "0.0001"), lvl("68003.0", "1.0")],
                    bounds: None,
                    available_notional: Decimal::from(500),
                },
                Book {
                    market: "BTC".into(),
                    timestamp_ms: at_ms,
                    tradable: true,
                    bids: vec![lvl("68000.5", "0.10")],
                    asks: vec![lvl("68001.5", "0.10")],
                    bounds: None,
                    available_notional: Decimal::from(500),
                },
            ],
            reference: None,
            equity_session: None,
            leg_oracles: None,
        };
        let build_tape = || {
            let mut ev = vec![MakerFirstEvent::TradeFeed {
                at_ms: start,
                leg: 0,
                online: true,
            }];
            ev.push(mk_book(start));
            ev.push(mk_book(start + 150));
            // Queue 0.0001 + partial fill 0.00068843 BTC (< 0.00073 quote, leaving 0.00000843 < 1e-5 RH step!).
            ev.push(MakerFirstEvent::Trade {
                at_ms: start + 200,
                source_ms: start + 195,
                leg: 0,
                aggressor: HedgeSide::Sell,
                price: d("68000.0"),
                contracts: d("0.00078843"),
            });
            // Advance past 2_500ms unhedged_timeout_ms up to 4_200ms while waiting for passive close.
            for dt in (350..=4_200).step_by(250) {
                ev.push(mk_book(start + dt));
            }
            // Buyer sweeps passive closing ask on Arcus for the full 0.00158843 BTC!
            ev.push(MakerFirstEvent::Trade {
                at_ms: start + 4_300,
                source_ms: start + 4_295,
                leg: 0,
                aggressor: HedgeSide::Buy,
                price: d("68002.0"),
                contracts: d("0.01000000"),
            });
            for dt in [4_450, 4_700, 5_000, 25_500] {
                ev.push(mk_book(start + dt));
            }
            ev.push(MakerFirstEvent::TradeFeed {
                at_ms: start + 25_600,
                leg: 0,
                online: false,
            });
            ev.push(MakerFirstEvent::End {
                at_ms: start + 30_000,
            });
            ev
        };

        let rep_ctrl = run_maker_first(
            &control,
            build_tape().into_iter().map(Ok),
            &VenueRegistry::default(),
        )
        .unwrap();
        assert_eq!(
            rep_ctrl.exposure_stops, 1,
            "without deadband, 0.00000843 BTC step dust triggers false exposure_stop: {rep_ctrl:?}"
        );
        assert!(rep_ctrl.below_minimum_hedge > 0);
        assert_eq!(rep_ctrl.cycle_attribution.paired_maker_closes, 0);

        let rep_db = run_maker_first(
            &deadband,
            build_tape().into_iter().map(Ok),
            &VenueRegistry::default(),
        )
        .unwrap();
        assert_eq!(rep_db.incomplete, None, "{rep_db:?}");
        assert_eq!(rep_db.exposure_stops, 0, "{rep_db:?}");
        assert_eq!(rep_db.below_minimum_hedge, 0, "{rep_db:?}");
        assert_eq!(rep_db.open_positions_base, [Decimal::ZERO; 2], "{rep_db:?}");
        assert_eq!(
            rep_db.cycle_attribution.paired_maker_closes, 1,
            "{rep_db:?}"
        );
        assert_eq!(
            rep_db.cycle_attribution.partner_step_deadband_cycles, 1,
            "{rep_db:?}"
        );
        assert!(
            rep_db
                .execution_costs
                .as_ref()
                .unwrap()
                .whole_ledger_cash_reconciled
        );
        assert_eq!(
            rep_db.turnover_breakdown.confirmed_eligible_turnover_quote,
            None
        );
        assert!(
            rep_db.turnover_breakdown.dual_venue_gross_turnover_quote
                > rep_db.turnover_breakdown.target_venue_turnover_quote
        );
    }

    #[test]
    fn equity_session_and_halt_gate_blocks_off_hours_and_attributes_rwa_regimes() {
        let mut cfg = config();
        cfg.pair.asset_class = AssetClass::Equity;
        cfg.pair.oracle_max_age_ms = 5_000;
        cfg.pair.equity_close_buffer_ms = 1_000;
        cfg.enforce_equity_session_gate = true;
        cfg.flat_at_funding = Some(FlatAtFunding {
            settlement_guard_ms: 2_000,
        });
        let start = cfg.start_ms;
        cfg.end_ms = start + 20_000;
        let lvl = |p: &str, c: &str| Level {
            price: d(p),
            contracts: d(c),
        };
        let mk_rwa_book =
            |at_ms: u64, mode: OracleMode, opens_at_ms: u64, closes_at_ms: u64, halted: bool| {
                let oracle = Oracle {
                    price: d("1000.1"),
                    timestamp_ms: at_ms,
                    mode,
                };
                MakerFirstEvent::Book {
                    at_ms,
                    books: [0, 1].map(|i| Book {
                        market: cfg.pair.legs[i].market.clone(),
                        timestamp_ms: at_ms,
                        tradable: true,
                        bids: vec![lvl("999.8", "1.0")],
                        asks: vec![lvl("1000.4", "1.0")],
                        bounds: None,
                        available_notional: Decimal::from(150),
                    }),
                    reference: Some(oracle),
                    equity_session: Some(EquitySession {
                        observed_at_ms: at_ms,
                        opens_at_ms,
                        closes_at_ms,
                        clear_until_ms: start + 100_000,
                        halted,
                    }),
                    leg_oracles: Some([oracle, oracle]),
                }
            };

        let events = vec![
            MakerFirstEvent::TradeFeed {
                at_ms: start,
                leg: 0,
                online: true,
            },
            // 1) Off-hours internal oracle -> blocked!
            mk_rwa_book(
                start + 100,
                OracleMode::Internal,
                start + 2_000,
                start + 50_000,
                false,
            ),
            // 2) Halted session -> blocked!
            mk_rwa_book(
                start + 500,
                OracleMode::LiveExternal,
                start,
                start + 50_000,
                true,
            ),
            // 3) Regular cash session with fresh LiveExternal reference -> allowed!
            mk_rwa_book(
                start + 2_500,
                OracleMode::LiveExternal,
                start + 2_000,
                start + 50_000,
                false,
            ),
            mk_rwa_book(
                start + 2_700,
                OracleMode::LiveExternal,
                start + 2_000,
                start + 50_000,
                false,
            ),
            MakerFirstEvent::TradeFeed {
                at_ms: start + 4_000,
                leg: 0,
                online: false,
            },
            mk_rwa_book(
                start + 16_500,
                OracleMode::LiveExternal,
                start + 2_000,
                start + 50_000,
                false,
            ),
            MakerFirstEvent::End { at_ms: cfg.end_ms },
        ];

        let rep =
            run_maker_first(&cfg, events.into_iter().map(Ok), &VenueRegistry::default()).unwrap();
        let rwa = rep
            .rwa_session
            .as_ref()
            .expect("equity report carries rwa_session");
        assert_eq!(rwa.off_hours_internal_or_close_frames, 1);
        assert_eq!(rwa.halted_or_uncovered_frames, 1);
        assert_eq!(rwa.regular_cash_frames, 3);
        assert_eq!(rwa.session_blocked_entries, 3);
        assert!(rep.entry_blocked.contains_key("equity_missing_session"));
        assert!(rep.entry_blocked.contains_key("equity_outside_session"));
        assert!(rep.entry_blocked.contains_key("equity_halt_or_ca_boundary"));
        assert_eq!(rep.quotes_submitted, 1, "{rep:?}");
        assert_eq!(rep.quotes_accepted, 1, "{rep:?}");
    }

    #[test]
    fn reverse_target_leg_turnover_feedback_tracks_eligible_leg_not_maker_leg() {
        let mut cfg = config();
        cfg.pair.strategy = Strategy::BasisReversion;
        cfg.pair.legs[0].venue = "arcus".into();
        cfg.pair.legs[0].market = "SPY-USD".into();
        cfg.pair.legs[0].lot = d("0.0001");
        cfg.pair.legs[0].min_contracts = d("0.01");
        cfg.pair.legs[0].min_notional = Decimal::from(5);
        cfg.pair.legs[0].taker_fee_bps = d("2.25");
        cfg.pair.legs[1].venue = "lighter_robinhood".into();
        cfg.pair.legs[1].market = "SPY".into();
        cfg.pair.legs[1].lot = d("0.0001");
        cfg.pair.legs[1].min_contracts = d("0.01");
        cfg.pair.legs[1].min_notional = Decimal::from(5);
        cfg.pair.legs[1].taker_fee_bps = Decimal::ZERO;
        cfg.pair.limit_buffer_bps = 0.0;
        cfg.pair.planner.min_net_edge_bps = 0.0;
        cfg.maker_leg = 1;
        cfg.maker_fee_bps = Decimal::ZERO;
        cfg.entry_side = Some(HedgeSide::Buy);
        cfg.quote_notional = Decimal::from(100);
        cfg.latency_reserve_bps = d("0.1");
        cfg.hedge_protection_bps = Some(d("5"));
        cfg.require_net_edge = true;
        cfg.diagnose_cycles = true;
        cfg.hold_ms = 5_000;
        cfg.drain_ms = 2_000;
        let start = cfg.start_ms;
        cfg.end_ms = start + 25_000;
        cfg.pair.mode = PaperMode::Turnover {
            max_turnover_quote: d("10000"),
            max_fees_quote: d("10"),
            max_realized_loss_quote: Some(d("5")),
            incentive: Some(IncentiveTurnover {
                eligible_leg: 0,
                eligible_from_ms: start,
                eligible_until_ms: cfg.end_ms - 1_000,
                max_cost_bps: d("10"),
            }),
        };
        cfg.turnover_feedback = Some(TurnoverFeedback {
            min_reserve_bps: d("0.25"),
            max_reserve_bps: d("2"),
            alpha: d("0.25"),
            overhead_bps: d("0.25"),
            max_quote_distance_bps: d("10"),
            fixed_overhead_usd_per_day: None,
        });
        cfg.hedged_exit = Some(HedgedExit {
            max_hold_ms: 8_000,
            min_saving_bps: Decimal::ZERO,
            max_adverse_basis_bps: d("50"),
            use_cycle_deadline: true,
            closing_quote_lifetime_ms: Some(4_000),
            patience_ms: 0,
            target_cycle_edge_bps: None,
            closing_max_levels: 1,
            max_hedged_book_stale_ms: None,
            max_hedged_leg_adverse_bps: None,
            hedged_leg_collateral_quote: None,
            allow_partner_step_deadband: false,
            max_hedged_both_blind_ms: None,
        });

        let mk_book = |at_ms: u64| {
            let lvl = |p: &str, c: &str| Level {
                price: d(p),
                contracts: d(c),
            };
            MakerFirstEvent::Book {
                at_ms,
                books: [
                    Book {
                        market: cfg.pair.legs[0].market.clone(),
                        timestamp_ms: at_ms,
                        tradable: true,
                        bids: vec![lvl("1000.0", "2.0")],
                        asks: vec![lvl("1000.2", "2.0")],
                        bounds: None,
                        available_notional: Decimal::from(500),
                    },
                    Book {
                        market: cfg.pair.legs[1].market.clone(),
                        timestamp_ms: at_ms,
                        tradable: true,
                        bids: vec![lvl("999.7", "0.10")],
                        asks: vec![lvl("1000.3", "0.10")],
                        bounds: None,
                        available_notional: Decimal::from(500),
                    },
                ],
                reference: None,
                equity_session: None,
                leg_oracles: None,
            }
        };

        let mut events = vec![
            MakerFirstEvent::TradeFeed {
                at_ms: start,
                leg: 1,
                online: true,
            },
            mk_book(start),
            mk_book(start + 150),
            MakerFirstEvent::Trade {
                at_ms: start + 200,
                source_ms: start + 195,
                leg: 1,
                aggressor: HedgeSide::Sell,
                price: d("999.7"),
                contracts: d("0.20"),
            },
        ];
        for dt in [250, 350, 500, 650, 800] {
            events.push(mk_book(start + dt));
        }
        events.push(MakerFirstEvent::Trade {
            at_ms: start + 850,
            source_ms: start + 845,
            leg: 1,
            aggressor: HedgeSide::Buy,
            price: d("1000.3"),
            contracts: d("0.20"),
        });
        for dt in [900, 1_100, 1_400, 23_500] {
            events.push(mk_book(start + dt));
        }
        events.push(MakerFirstEvent::TradeFeed {
            at_ms: start + 23_600,
            leg: 1,
            online: false,
        });
        events.push(MakerFirstEvent::End { at_ms: cfg.end_ms });

        let rep =
            run_maker_first(&cfg, events.into_iter().map(Ok), &VenueRegistry::default()).unwrap();
        assert_eq!(rep.incomplete, None, "{rep:?}");
        assert_eq!(rep.closed_cycles, 1, "{rep:?}");
        assert_eq!(rep.open_positions_base, [Decimal::ZERO; 2], "{rep:?}");
        assert!(
            rep.execution_costs
                .as_ref()
                .unwrap()
                .whole_ledger_cash_reconciled
        );
        let fb = rep
            .turnover_feedback
            .as_ref()
            .expect("turnover_feedback enabled on reverse route");
        assert_eq!(
            fb.modeled_target_turnover_quote, rep.turnover_breakdown.target_venue_turnover_quote,
            "modeled_target_turnover_quote must track eligible_leg=0 (Arcus), not maker_leg=1"
        );
        assert_ne!(
            rep.turnover_breakdown.target_venue_turnover_quote, rep.maker_turnover,
            "target leg and maker leg must have distinct notionals"
        );
    }

    #[test]
    fn sub_min_partial_fill_long_and_short_mirror_are_symmetric_under_tight_budget() {
        let build_and_run = |entry_side: HedgeSide| {
            let mut cfg = config();
            cfg.pair.strategy = Strategy::BasisReversion;
            cfg.pair.legs[0].venue = "arcus".into();
            cfg.pair.legs[0].market = "SPY-USD".into();
            cfg.pair.legs[0].tick = d("0.01");
            cfg.pair.legs[0].lot = d("0.001");
            cfg.pair.legs[0].min_contracts = d("0.01");
            cfg.pair.legs[0].min_notional = Decimal::from(10);
            cfg.pair.legs[0].taker_fee_bps = Decimal::ZERO;
            cfg.pair.legs[1].venue = "lighter_robinhood".into();
            cfg.pair.legs[1].market = "SPY".into();
            cfg.pair.legs[1].tick = d("0.01");
            cfg.pair.legs[1].lot = d("0.001");
            cfg.pair.legs[1].min_contracts = d("0.01");
            cfg.pair.legs[1].min_notional = Decimal::from(10);
            cfg.pair.legs[1].taker_fee_bps = Decimal::ZERO;
            cfg.pair.limit_buffer_bps = 0.0;
            cfg.pair.planner.min_net_edge_bps = 0.0;
            cfg.pair.planner.max_base_quantity = 10.0;
            cfg.maker_fee_bps = Decimal::ZERO;
            cfg.latency_reserve_bps = Decimal::ZERO;
            cfg.hedge_protection_bps = Some(d("5"));
            cfg.require_net_edge = true;
            cfg.entry_side = Some(entry_side);
            cfg.quote_notional = Decimal::from(100);
            cfg.aggregate_sub_minimum_fills = true;
            cfg.diagnose_cycles = true;
            cfg.quote_lifetime_ms = 300;
            cfg.unhedged_timeout_ms = 10_000;
            cfg.hold_ms = 5_000;
            cfg.drain_ms = 2_000;
            let start = cfg.start_ms;
            cfg.end_ms = start + 30_000;
            cfg.pair.mode = PaperMode::Turnover {
                max_turnover_quote: d("2000"),
                max_fees_quote: d("10"),
                // Tight loss budget ($0.50) strictly smaller than the ~$5.00 partial principal!
                max_realized_loss_quote: Some(d("0.50")),
                incentive: Some(IncentiveTurnover {
                    eligible_leg: 0,
                    eligible_from_ms: start,
                    eligible_until_ms: cfg.end_ms - 1_000,
                    max_cost_bps: d("5"),
                }),
            };
            cfg.turnover_feedback = Some(TurnoverFeedback {
                min_reserve_bps: d("0.25"),
                max_reserve_bps: d("2"),
                alpha: d("0.25"),
                overhead_bps: Decimal::ZERO,
                max_quote_distance_bps: d("10"),
                fixed_overhead_usd_per_day: None,
            });
            cfg.hedged_exit = Some(HedgedExit {
                max_hold_ms: 8_000,
                min_saving_bps: Decimal::ZERO,
                max_adverse_basis_bps: d("50"),
                use_cycle_deadline: true,
                closing_quote_lifetime_ms: Some(5_000),
                patience_ms: 0,
                target_cycle_edge_bps: None,
                closing_max_levels: 1,
                max_hedged_book_stale_ms: None,
                max_hedged_leg_adverse_bps: None,
                hedged_leg_collateral_quote: None,
                allow_partner_step_deadband: false,
                max_hedged_both_blind_ms: None,
            });

            let mk_book = |at_ms: u64| {
                let lvl = |p: &str, c: &str| Level {
                    price: d(p),
                    contracts: d(c),
                };
                MakerFirstEvent::Book {
                    at_ms,
                    books: [0, 1].map(|i| Book {
                        market: cfg.pair.legs[i].market.clone(),
                        timestamp_ms: at_ms,
                        tradable: true,
                        bids: if i == 0 {
                            vec![lvl("999.9", "0.10"), lvl("999.8", "1.0")]
                        } else {
                            vec![lvl("999.9", "2.0")]
                        },
                        asks: if i == 0 {
                            vec![lvl("1000.1", "0.10"), lvl("1000.2", "1.0")]
                        } else {
                            vec![lvl("1000.1", "2.0")]
                        },
                        bounds: None,
                        available_notional: Decimal::from(500),
                    }),
                    reference: None,
                    equity_session: None,
                    leg_oracles: None,
                }
            };

            let (open_agg, open_px, close_agg, close_px) = match entry_side {
                HedgeSide::Buy => (HedgeSide::Sell, d("999.9"), HedgeSide::Buy, d("1000.1")),
                HedgeSide::Sell => (HedgeSide::Buy, d("1000.1"), HedgeSide::Sell, d("999.9")),
            };

            let mut events = vec![
                MakerFirstEvent::TradeFeed {
                    at_ms: start,
                    leg: 0,
                    online: true,
                },
                mk_book(start),
                mk_book(start + 150),
                // Consume 0.10 queue + 0.005 partial (~$5 < 0.01 min_contracts, far > $0.50 loss limit).
                MakerFirstEvent::Trade {
                    at_ms: start + 200,
                    source_ms: start + 195,
                    leg: 0,
                    aggressor: open_agg,
                    price: open_px,
                    contracts: d("0.105"),
                },
                // Quote #1 expires at 400 (last_receipt = 750) -> Quote #2 submits at 750 under tight $0.50 loss limit!
                mk_book(start + 400),
                mk_book(start + 750),
                mk_book(start + 950),
                MakerFirstEvent::Trade {
                    at_ms: start + 1_000,
                    source_ms: start + 995,
                    leg: 0,
                    aggressor: open_agg,
                    price: open_px,
                    contracts: d("0.20"),
                },
            ];
            for dt in [1_150, 1_300, 1_450, 1_650] {
                events.push(mk_book(start + dt));
            }
            events.push(MakerFirstEvent::Trade {
                at_ms: start + 1_750,
                source_ms: start + 1_745,
                leg: 0,
                aggressor: close_agg,
                price: close_px,
                contracts: d("0.20"),
            });
            for dt in [1_900, 2_050, 28_500] {
                events.push(mk_book(start + dt));
            }
            events.push(MakerFirstEvent::TradeFeed {
                at_ms: start + 28_600,
                leg: 0,
                online: false,
            });
            events.push(MakerFirstEvent::End { at_ms: cfg.end_ms });

            run_maker_first(&cfg, events.into_iter().map(Ok), &VenueRegistry::default()).unwrap()
        };

        let long_rep = build_and_run(HedgeSide::Buy);
        let short_rep = build_and_run(HedgeSide::Sell);

        for (tag, rep) in [("long", &long_rep), ("short", &short_rep)] {
            assert_eq!(rep.incomplete, None, "{tag}: {rep:?}");
            assert_eq!(rep.closed_cycles, 1, "{tag}: {rep:?}");
            assert!(
                rep.quotes_submitted >= 3,
                "{tag} expected continuation quote #2 + closing quote: {rep:?}"
            );
            assert_eq!(
                rep.open_positions_base,
                [Decimal::ZERO; 2],
                "{tag}: {rep:?}"
            );
            assert!(
                !rep.entry_blocked.contains_key("turnover_loss_limit"),
                "{tag} must not confuse partial principal with realized loss: {rep:?}"
            );
            assert!(
                rep.execution_costs
                    .as_ref()
                    .unwrap()
                    .whole_ledger_cash_reconciled,
                "{tag}: {rep:?}"
            );
        }
        assert_eq!(long_rep.closed_cycles, short_rep.closed_cycles);
        assert_eq!(long_rep.net_pnl, short_rep.net_pnl);
        assert_eq!(long_rep.fees, short_rep.fees);
        assert_eq!(
            long_rep.turnover_breakdown.target_venue_turnover_quote,
            short_rep.turnover_breakdown.target_venue_turnover_quote
        );
        assert_eq!(
            long_rep.turnover_breakdown.dual_venue_gross_turnover_quote,
            short_rep.turnover_breakdown.dual_venue_gross_turnover_quote
        );
    }

    #[test]
    fn entry_book_filter_blocks_poor_openings_but_never_blocks_hedged_closes() {
        let mut cfg = config();
        cfg.entry_side = Some(HedgeSide::Buy);
        cfg.end_ms = cfg.start_ms + 30_000;
        cfg.hedged_exit = Some(HedgedExit {
            max_hold_ms: 8_000,
            min_saving_bps: Decimal::ZERO,
            max_adverse_basis_bps: d("30"),
            use_cycle_deadline: false,
            closing_quote_lifetime_ms: None,
            patience_ms: 0,
            target_cycle_edge_bps: None,
            closing_max_levels: 1,
            max_hedged_book_stale_ms: None,
            max_hedged_leg_adverse_bps: None,
            hedged_leg_collateral_quote: None,
            allow_partner_step_deadband: false,
            max_hedged_both_blind_ms: None,
        });
        cfg.entry_book_filter = Some(EntryBookFilter {
            min_maker_spread_bps: Some(d("3.0")),
            max_hedge_spread_bps: Some(d("2.0")),
            min_hedge_touch_notional_mult: Some(d("2.0")),
            max_adverse_book_imbalance: Some(d("0.5")),
            min_hedged_opening_edge_bps: Some(d("-5.0")),
            max_maker_queue_notional_mult: Some(d("10.0")),
        });

        let lvl = |price: &str, contracts: &str| Level {
            price: d(price),
            contracts: d(contracts),
        };
        let mk_pair = |at: u64, m_bid: &str, m_ask: &str, h_bid: &str, h_ask: &str, h_sz: &str| {
            MakerFirstEvent::Book {
                at_ms: at,
                books: [
                    Book {
                        market: cfg.pair.legs[0].market.clone(),
                        timestamp_ms: at,
                        tradable: true,
                        bids: vec![lvl(m_bid, "0.50")],
                        asks: vec![lvl(m_ask, "0.50")],
                        bounds: None,
                        available_notional: Decimal::from(500),
                    },
                    Book {
                        market: cfg.pair.legs[1].market.clone(),
                        timestamp_ms: at,
                        tradable: true,
                        bids: vec![lvl(h_bid, h_sz)],
                        asks: vec![lvl(h_ask, h_sz)],
                        bounds: None,
                        available_notional: Decimal::from(500),
                    },
                ],
                reference: None,
                equity_session: None,
                leg_oracles: None,
            }
        };

        let start = cfg.start_ms;
        let events = vec![
            MakerFirstEvent::TradeFeed {
                at_ms: start,
                leg: 0,
                online: true,
            },
            // 2 bps maker spread (< 3 bps min) -> blocked by maker_spread_too_tight.
            mk_pair(start, "999.9", "1000.1", "999.9", "1000.1", "1.0"),
            // 4 bps maker spread, 2 bps hedge spread, $99.99 touch (< 2x $100) -> blocked by hedge_touch_depth_thin.
            mk_pair(start + 100, "999.8", "1000.2", "999.9", "1000.1", "0.10"),
            // Adverse hedged opening basis (1 bps hedge spread <= 2 bps): Maker Buy @ 999.8 vs Hedge Bid @ 999.0 (-8 bps - fees < -5 bps).
            mk_pair(start + 200, "999.8", "1000.2", "999.0", "999.1", "1.0"),
            // Good opening books: 4 bps maker spread, 2 bps hedge spread, $999.90 touch depth (>=2x $100).
            mk_pair(start + 300, "999.8", "1000.2", "999.9", "1000.1", "1.0"),
            mk_pair(start + 400, "999.8", "1000.2", "999.9", "1000.1", "1.0"),
            // Cross through opening Buy quote (999.8) cleanly.
            MakerFirstEvent::Trade {
                at_ms: start + 450,
                source_ms: start + 450,
                leg: 0,
                aggressor: HedgeSide::Sell,
                price: d("999.7"),
                contracts: d("0.04"),
            },
            mk_pair(start + 550, "999.8", "1000.2", "999.9", "1000.1", "1.0"),
            // Opening Hedge Sell IOC settles at 999.9 on start + 650, where terrible opening books
            // (2 bps maker spread, 4 bps hedge spread, 0.10 thin touch) are already visible:
            // closing (`!closing == false`) MUST bypass entry_book_filter and submit Sell @ 1000.1!
            mk_pair(start + 650, "999.9", "1000.1", "999.9", "1000.3", "0.10"),
            mk_pair(start + 750, "999.9", "1000.1", "999.9", "1000.3", "0.10"),
            MakerFirstEvent::Trade {
                at_ms: start + 800,
                source_ms: start + 800,
                leg: 0,
                aggressor: HedgeSide::Buy,
                price: d("1000.2"),
                contracts: d("0.04"),
            },
            mk_pair(start + 900, "999.9", "1000.1", "999.9", "1000.3", "0.10"),
            mk_pair(start + 1_000, "999.9", "1000.1", "999.9", "1000.3", "0.10"),
            MakerFirstEvent::TradeFeed {
                at_ms: start + 1_100,
                leg: 0,
                online: false,
            },
            mk_pair(start + 1_200, "999.9", "1000.1", "999.9", "1000.3", "0.10"),
            MakerFirstEvent::End { at_ms: cfg.end_ms },
        ];

        let rep =
            run_maker_first(&cfg, events.into_iter().map(Ok), &VenueRegistry::default()).unwrap();
        let ebf = rep
            .entry_book_filter
            .as_ref()
            .expect("entry_book_filter report");
        assert_eq!(ebf.scope, "opening_maker_quotes_only");
        assert!(ebf.blocked_maker_spread >= 1, "report: {ebf:?}");
        assert!(ebf.blocked_hedge_touch_depth >= 1, "report: {ebf:?}");
        assert!(ebf.blocked_hedged_opening_edge >= 1, "report: {ebf:?}");
        assert!(
            rep.entry_blocked.contains_key("maker_spread_too_tight"),
            "{:?}",
            rep.entry_blocked
        );
        assert!(
            rep.entry_blocked
                .contains_key("adverse_hedged_opening_edge"),
            "{:?}",
            rep.entry_blocked
        );
        assert_eq!(rep.incomplete, None, "report: {rep:?}");
        assert_eq!(
            rep.closed_cycles, 1,
            "closing must bypass entry_book_filter: {rep:?}"
        );
        assert_eq!(
            rep.cycle_attribution.paired_maker_closes, 1,
            "report: {rep:?}"
        );
        assert_eq!(rep.open_positions_base, [Decimal::ZERO; 2]);
    }

    #[test]
    fn probabilistic_queue_avoids_trade_double_counting_and_tracks_fixed_overhead() {
        let build_cfg = |queue_model: Option<ProbabilisticQueueConfig>| {
            let mut cfg = passive_config(HedgeSide::Buy);
            cfg.pair.strategy = Strategy::BasisReversion;
            cfg.pair.legs[0].venue = "arcus".into();
            cfg.pair.legs[0].taker_fee_bps = Decimal::ZERO;
            cfg.pair.legs[1].taker_fee_bps = Decimal::ZERO;
            cfg.pair.limit_buffer_bps = 0.0;
            cfg.latency_reserve_bps = Decimal::ZERO;
            cfg.quote_lifetime_ms = 2_000;
            cfg.trade_delivery_grace_ms = 80;
            cfg.end_ms = cfg.start_ms + 30_000;
            cfg.diagnose_cycles = true;
            cfg.passive_offset.as_mut().unwrap().close_probability = Some([Decimal::ONE; 2]);
            cfg.pair.mode = PaperMode::Turnover {
                max_turnover_quote: d("1000"),
                max_fees_quote: d("1"),
                max_realized_loss_quote: Some(d("5")),
                incentive: Some(IncentiveTurnover {
                    eligible_leg: 0,
                    eligible_from_ms: cfg.start_ms,
                    eligible_until_ms: cfg.end_ms,
                    max_cost_bps: d("10"),
                }),
            };
            cfg.turnover_feedback = Some(TurnoverFeedback {
                min_reserve_bps: d("0.25"),
                max_reserve_bps: d("2"),
                alpha: Decimal::ZERO,
                overhead_bps: Decimal::ZERO,
                max_quote_distance_bps: d("10"),
                fixed_overhead_usd_per_day: Some(d("24")),
            });
            cfg.queue_model = queue_model;
            cfg
        };

        let lvl = |price: &str, contracts: &str| Level {
            price: d(price),
            contracts: d(contracts),
        };
        let mk_books =
            |cfg: &MakerFirstConfig, at: u64, bid_sz: &str, ask_sz: &str| MakerFirstEvent::Book {
                at_ms: at,
                books: [0, 1].map(|i| Book {
                    market: cfg.pair.legs[i].market.clone(),
                    timestamp_ms: at,
                    tradable: true,
                    bids: vec![lvl("999.8", if i == 0 { bid_sz } else { "1.0" })],
                    asks: vec![lvl("1000.2", if i == 0 { ask_sz } else { "1.0" })],
                    bounds: None,
                    available_notional: Decimal::from(500),
                }),
                reference: None,
                equity_session: None,
                leg_oracles: None,
            };

        let base_cfg = build_cfg(None);
        let t = base_cfg.start_ms;
        // Scenario:
        // 1) Bid activates at 999.8 with 1.00 visible contracts ahead.
        // 2) At t+150: trade of 0.30 prints at 999.8 -> remaining ahead = 0.70, market depth drops 1.00 -> 0.70 at t+200 (zero cancel depletion!).
        // 3) At t+300: another 0.40 joins behind us (depth = 1.10); at t+400: 0.70 cancels down to 0.40!
        //    Under Conservative, ahead stays 0.70; under uniform Probabilistic(1.0, 1.0), ahead shrinks from 0.70 -> <= 0.40!
        // 4) After the delivery allowance expires, a 0.44 touch prints at t+490:
        //    - Conservative: needs 0.70 > 0.44 -> ZERO fill!
        //    - Probabilistic: ahead <= 0.40 -> fills 0.04 Buy, then closes passively on Ask!
        let build_events = |cfg: &MakerFirstConfig| {
            vec![
                MakerFirstEvent::TradeFeed {
                    at_ms: t,
                    leg: 0,
                    online: true,
                },
                mk_books(cfg, t, "1.00", "0.02"),
                mk_books(cfg, t + 100, "1.00", "0.02"),
                MakerFirstEvent::Trade {
                    at_ms: t + 150,
                    source_ms: t + 150,
                    leg: 0,
                    aggressor: HedgeSide::Sell,
                    price: d("999.8"),
                    contracts: d("0.30"),
                },
                mk_books(cfg, t + 200, "0.70", "0.02"),
                mk_books(cfg, t + 300, "1.10", "0.02"),
                mk_books(cfg, t + 400, "0.40", "0.02"),
                mk_books(cfg, t + 485, "0.40", "0.02"),
                MakerFirstEvent::Trade {
                    at_ms: t + 490,
                    source_ms: t + 490,
                    leg: 0,
                    aggressor: HedgeSide::Sell,
                    price: d("999.8"),
                    contracts: d("0.44"),
                },
                mk_books(cfg, t + 500, "0.20", "0.02"),
                mk_books(cfg, t + 550, "0.50", "0.02"),
                mk_books(cfg, t + 650, "0.50", "0.02"),
                mk_books(cfg, t + 760, "0.50", "0.02"),
                MakerFirstEvent::Trade {
                    at_ms: t + 790,
                    source_ms: t + 790,
                    leg: 0,
                    aggressor: HedgeSide::Buy,
                    price: d("1000.2"),
                    contracts: d("0.06"),
                },
                mk_books(cfg, t + 800, "0.50", "0.02"),
                mk_books(cfg, t + 900, "0.50", "0.02"),
                mk_books(cfg, t + 22_500, "0.50", "0.02"),
                mk_books(cfg, t + 23_000, "0.50", "0.02"),
                MakerFirstEvent::TradeFeed {
                    at_ms: t + 26_000,
                    leg: 0,
                    online: false,
                },
                mk_books(cfg, t + 27_000, "0.50", "0.02"),
                MakerFirstEvent::End { at_ms: cfg.end_ms },
            ]
        };

        let cons = run_maker_first(
            &base_cfg,
            build_events(&base_cfg).into_iter().map(Ok),
            &VenueRegistry::default(),
        )
        .unwrap();
        assert_eq!(cons.incomplete, None, "{cons:?}");
        assert!(cons.queue_model.is_none());
        assert_eq!(cons.maker_fills, 0);
        assert_eq!(cons.passive_only_cycles, 0);
        let cons_fb = cons.turnover_feedback.as_ref().unwrap();
        // $24/day * 30s / 86,400s = $1 / 120 = 0.008333...
        let expected_fixed = d("24") * Decimal::from(30_000) / Decimal::from(DAY_MS);
        assert_eq!(cons_fb.fixed_window_overhead_quote, Some(expected_fixed));
        assert_eq!(
            cons_fb.net_after_fixed_window_overhead,
            Some(-expected_fixed)
        );

        let prob_cfg = build_cfg(Some(ProbabilisticQueueConfig {
            initial_ahead_fraction: Decimal::ONE,
            cancel_progress_power: 1.0,
        }));
        let prob = run_maker_first(
            &prob_cfg,
            build_events(&prob_cfg).into_iter().map(Ok),
            &VenueRegistry::default(),
        )
        .unwrap();
        let qm_rep = prob.queue_model.expect("probabilistic queue report");
        assert!(qm_rep.cancel_depletion_events >= 1);
        assert!(
            qm_rep.cancel_depleted_ahead_base > Decimal::ZERO
                && qm_rep.cancel_depleted_ahead_base < d("0.50"),
            "trade-driven book drop must not double count: {qm_rep:?}"
        );
        assert_eq!(
            prob.passive_only_cycles, 1,
            "probabilistic queue should fill on touch without blow-through: {prob:?}"
        );
        assert!(prob.net_pnl.unwrap() > Decimal::ZERO);

        // Trade-only book drop verification: if no extra cancel occurs after the 0.30 trade
        // (depth stays 0.70), ProbabilisticQueue reports zero cancel depletion.
        let trade_only_cfg = build_cfg(Some(ProbabilisticQueueConfig {
            initial_ahead_fraction: Decimal::ONE,
            cancel_progress_power: 2.0,
        }));
        for book_first in [false, true] {
            let mut trade_only_events = vec![
                MakerFirstEvent::TradeFeed {
                    at_ms: t,
                    leg: 0,
                    online: true,
                },
                mk_books(&trade_only_cfg, t, "1.00", "0.02"),
                mk_books(&trade_only_cfg, t + 100, "1.00", "0.02"),
                mk_books(&trade_only_cfg, t + 120, "1.00", "0.02"),
                MakerFirstEvent::Trade {
                    at_ms: if book_first { t + 210 } else { t + 150 },
                    source_ms: t + 150,
                    leg: 0,
                    aggressor: HedgeSide::Sell,
                    price: d("999.8"),
                    contracts: d("0.30"),
                },
                mk_books(&trade_only_cfg, t + 200, "0.70", "0.02"),
                mk_books(&trade_only_cfg, t + 290, "0.70", "0.02"),
                MakerFirstEvent::Trade {
                    at_ms: t + 310,
                    source_ms: t + 310,
                    leg: 0,
                    aggressor: HedgeSide::Sell,
                    price: d("999.8"),
                    contracts: d("0.50"),
                },
                MakerFirstEvent::TradeFeed {
                    at_ms: t + 350,
                    leg: 0,
                    online: false,
                },
                MakerFirstEvent::End {
                    at_ms: trade_only_cfg.end_ms,
                },
            ];
            if book_first {
                trade_only_events.swap(4, 5);
            }
            let trade_only = run_maker_first(
                &trade_only_cfg,
                trade_only_events.into_iter().map(Ok),
                &VenueRegistry::default(),
            )
            .unwrap();
            let to_qm = trade_only.queue_model.unwrap();
            assert_eq!(to_qm.cancel_depletion_events, 0);
            assert_eq!(to_qm.cancel_depleted_ahead_base, Decimal::ZERO);
            assert_eq!(trade_only.maker_turnover, Decimal::ZERO);
        }
    }

    #[test]
    fn entry_markout_labels_age_out_so_a_blocked_gate_cannot_stay_locked() {
        let mut policy = EntryMarkoutGate {
            horizon_ms: 1_000,
            prior_adverse_markout_bps: d("1.0"),
            min_profit_bps: d("0.5"),
            quantile_p: 0.5,
            max_samples: 16,
            include_hedge_taker_fee: true,
            max_sample_age_ms: Some(5_000),
        };
        policy.validate().unwrap();
        let label = |policy: EntryMarkoutGate| {
            let mut state = EntryMarkoutState::default();
            for k in 0..4_u64 {
                state.enqueue(1_000 + k, HedgeSide::Buy, Some(d("100")));
            }
            // Mid fell 10 bp against every opening buy.
            state.advance(1_003, Some((1_003, d("99.9"))), policy);
            state
        };
        let mut state = label(policy);
        assert_eq!(state.active_markout_bps(policy), d("10"));
        assert_eq!(state.required_distance_bps(policy, d("2")), d("12.5"));
        // No new fills arrive while blocked; labels expire and the prior returns.
        state.advance(7_000, Some((7_000, d("99.9"))), policy);
        assert_eq!(state.active_markout_bps(policy), d("1.0"));
        let rep = state.report(policy, d("2"));
        assert_eq!((rep.samples, rep.aged_out_samples), (0, 4));
        // A label without a fresh mid inside the grace window is counted, not used.
        state.enqueue(8_000, HedgeSide::Sell, Some(d("100")));
        state.advance(9_500, Some((9_500, d("100.5"))), policy);
        assert_eq!(state.report(policy, d("2")).unlabelled_expired, 1);
        assert_eq!(state.active_markout_bps(policy), d("1.0"));

        // Legacy behaviour (no expiry) keeps the stale labels indefinitely.
        policy.max_sample_age_ms = None;
        let mut legacy = label(policy);
        legacy.advance(1_000_000, Some((1_000_000, d("99.9"))), policy);
        assert_eq!(legacy.active_markout_bps(policy), d("10"));

        policy.max_sample_age_ms = Some(999);
        assert!(policy.validate().is_err());
    }

    #[test]
    fn entry_labels_require_the_hedge_venue_at_both_ends_and_a_post_horizon_book() {
        let policy = EntryMarkoutGate {
            horizon_ms: 1_000,
            prior_adverse_markout_bps: Decimal::ZERO,
            min_profit_bps: Decimal::ZERO,
            quantile_p: 0.5,
            max_samples: 4,
            include_hedge_taker_fee: true,
            max_sample_age_ms: None,
        };
        let mut state = EntryMarkoutState::default();
        state.enqueue(2_000, HedgeSide::Sell, None);
        state.enqueue(2_000, HedgeSide::Buy, Some(d("100")));
        state.advance(2_000, Some((1_999, d("100"))), policy);
        state.advance(2_010, None, policy);
        assert_eq!(state.report(policy, Decimal::ZERO).pending_labels, 1);
        state.advance(2_020, Some((2_010, d("99.9"))), policy);
        assert_eq!(state.adverse_bps.front(), Some(&(2_020, d("10"))));
        state.enqueue(4_000, HedgeSide::Sell, Some(d("100")));
        state.advance(5_001, Some((5_001, d("101"))), policy);
        let report = state.report(policy, Decimal::ZERO);
        assert_eq!(
            (report.labels_completed, report.missing_hedge_anchor),
            (1, 1)
        );
        assert_eq!((report.unlabelled_expired, report.pending_labels), (1, 0));
    }

    #[test]
    fn opening_fill_waits_for_a_horizon_observation_from_the_hedge_leg() {
        let mut cfg = passive_config(HedgeSide::Buy);
        cfg.entry_markout_gate = Some(EntryMarkoutGate {
            horizon_ms: 100,
            prior_adverse_markout_bps: Decimal::ZERO,
            min_profit_bps: Decimal::ZERO,
            include_hedge_taker_fee: false,
            max_samples: 16,
            quantile_p: 0.5,
            max_sample_age_ms: None,
        });
        let start = cfg.start_ms;
        for (hedge_timestamp, expected) in [(0, 0), (209, 0), (210, 1)] {
            let mut horizon = books(&cfg, start + 210, "999.8");
            if let MakerFirstEvent::Book { books, .. } = &mut horizon {
                books[1].timestamp_ms = if hedge_timestamp == 0 {
                    0
                } else {
                    start + hedge_timestamp
                };
            }
            let events = [
                MakerFirstEvent::TradeFeed {
                    at_ms: start,
                    leg: 0,
                    online: true,
                },
                books(&cfg, start, "999.8"),
                books(&cfg, start + 100, "999.8"),
                print_against(start + 110, HedgeSide::Buy, "999.7", "0.04"),
                horizon,
                MakerFirstEvent::End { at_ms: cfg.end_ms },
            ];
            let result =
                run_maker_first(&cfg, events.into_iter().map(Ok), &VenueRegistry::default())
                    .unwrap();
            assert_eq!(result.maker_buy_fills, 1);
            let gate = result.entry_markout_gate.unwrap();
            assert_eq!(
                (gate.labels_completed, gate.unlabelled_expired),
                (expected, 1 - expected)
            );
        }
    }

    #[test]
    fn max_hedged_both_blind_ms_survives_brief_dual_refresh_and_still_stops_on_outage() {
        let mk_cfg = |max_hedged_both_blind_ms: Option<u64>| {
            let mut c = config();
            c.pair.strategy = Strategy::BasisReversion;
            c.pair.legs[0].taker_fee_bps = Decimal::ZERO;
            c.pair.legs[1].taker_fee_bps = Decimal::ZERO;
            c.pair.planner.min_net_edge_bps = 0.0;
            c.pair.planner.max_quote_age_ms = 1_000;
            c.pair.planner.max_quote_skew_ms = 500;
            c.require_net_edge = false;
            c.maker_fee_bps = Decimal::ZERO;
            c.end_ms = c.start_ms + 30_000;
            c.hedged_exit = Some(HedgedExit {
                max_hold_ms: 10_000,
                min_saving_bps: Decimal::ZERO,
                max_adverse_basis_bps: d("20"),
                use_cycle_deadline: false,
                closing_quote_lifetime_ms: Some(4_000),
                patience_ms: 0,
                target_cycle_edge_bps: None,
                closing_max_levels: 1,
                max_hedged_book_stale_ms: Some(5_000),
                max_hedged_leg_adverse_bps: None,
                hedged_leg_collateral_quote: None,
                allow_partner_step_deadband: false,
                max_hedged_both_blind_ms,
            });
            c
        };
        let registry = VenueRegistry::default();
        let mut invalid = mk_cfg(Some(6_000));
        assert!(invalid.validate(&registry).is_err());
        invalid
            .hedged_exit
            .as_mut()
            .unwrap()
            .max_hedged_book_stale_ms = None;
        invalid
            .hedged_exit
            .as_mut()
            .unwrap()
            .max_hedged_both_blind_ms = Some(500);
        assert!(invalid.validate(&registry).is_err());

        let strict = mk_cfg(None);
        let graceful = mk_cfg(Some(500));
        let start = strict.start_ms;
        // Strict net-zero balanced hedge receives grace during a 100 ms dual refresh,
        // whereas nonzero residual dust inside `allow_partner_step_deadband` MUST still
        // trigger the paired book-stop immediately (Codex S12-2 review).
        {
            let mut cfg = mk_cfg(Some(500));
            cfg.hedged_exit
                .as_mut()
                .unwrap()
                .allow_partner_step_deadband = true;
            let mut replay = new_replay(&cfg).unwrap();
            let step = cfg.pair.legs[1 - replay.maker()].base_step();
            let dust = step / Decimal::from(10);
            assert!(dust > Decimal::ZERO && dust < step);
            replay.last_source_ms = [start, start];
            replay.last_usable_at_ms = [Some(start), Some(start)];
            replay.cycle = Some(Cycle {
                id: 0,
                posture: replay.posture.posture,
                started_ms: start,
                initial_cash: Decimal::ZERO,
                initial_turnover: Decimal::ZERO,
                initial_fees: Decimal::ZERO,
                draining: false,
                externalizing: false,
                maker_turnover: Decimal::ZERO,
                passive_side: None,
                entry_ref_mid: None,
                paired_mids: [None, None],
                paired_at_ms: Some(start),
                hedge_fill_at_ms: Some(start),
                paired_basis_bps: None,
                closing_maker_filled: false,
                forced_ioc_filled: false,
                sub_min_top_up_attempts: 0,
                sub_min_recovery_pending_base: Decimal::ZERO,
                used_partner_step_deadband: true,
                cash_exit_triggered: false,
            });
            let at = start + 100;
            assert!(replay.fresh(0, at).is_none() && replay.fresh(1, at).is_none());

            // Exact net == 0 -> rides out 100 ms dual refresh under 500 ms grace.
            replay.ledger.positions = [Decimal::ONE, -Decimal::ONE];
            assert!(replay.net().is_zero());
            assert!(!replay.paired_stop(at).2);

            // Nonzero residual below partner lot -> MUST fail closed immediately.
            replay.ledger.positions = [Decimal::ONE + dust, -Decimal::ONE];
            assert!(!replay.net().is_zero() && replay.within_partner_hedge_deadband(at));
            assert!(
                replay.paired_stop(at).2,
                "dual blindness must not grant grace to nonzero residual below partner lot"
            );

            // Quiet-book source-age case (W01/W05 ETH `blind_clock` regression, S13-2):
            // Pre-existing exchange source age is 346 ms (> grace 200 ms), while simultaneous
            // dual-blind receipt absence is only 180 ms (<= grace 200 ms) and source age stays
            // well within `max_hedged_book_stale_ms` (5_000 ms) -> survives under `grace200`,
            // then fails closed once receipt absence exceeds 200 ms OR source age exceeds 5_000 ms.
            let mut cfg200 = mk_cfg(Some(200));
            cfg200
                .hedged_exit
                .as_mut()
                .unwrap()
                .max_hedged_book_stale_ms = Some(5_000);
            let mut replay200 = new_replay(&cfg200).unwrap();
            replay200.ledger.positions = [Decimal::ONE, -Decimal::ONE];
            replay200.last_usable_at_ms = [Some(start + 1_000), Some(start + 1_000)];
            replay200.last_source_ms = [start + 754, start + 929]; // 346 ms / 171 ms old at +1_100
            replay200.cycle = replay.cycle.take();
            assert!(!replay200.paired_stop(start + 1_100).2); // receipt age 100 ms <= 200 ms
            assert!(!replay200.paired_stop(start + 1_180).2); // receipt age 180 ms <= 200 ms, src 426 ms
            assert!(replay200.paired_stop(start + 1_250).2); // receipt age 250 ms > 200 ms -> stop!

            // Source age exceeding `max_hedged_book_stale_ms` (5_000 ms) still fails closed even if
            // receipt absence is inside 200 ms!
            replay200.last_usable_at_ms = [Some(start + 6_000), Some(start + 6_000)];
            replay200.last_source_ms = [start + 900, start + 5_900]; // leg 0 src age = 5_200 ms > 5_000
            assert!(replay200.paired_stop(start + 6_100).2);
        }
        let both_empty = |cfg: &MakerFirstConfig, at_ms: u64| {
            let mut ev = books(cfg, at_ms, "999.8");
            if let MakerFirstEvent::Book {
                books: ref mut b, ..
            } = ev
            {
                for leg in b.iter_mut() {
                    leg.timestamp_ms = 0;
                    leg.tradable = false;
                    leg.bids.clear();
                    leg.asks.clear();
                }
            }
            ev
        };
        let build_events = |cfg: &MakerFirstConfig, recovery_ms: u64| {
            vec![
                MakerFirstEvent::TradeFeed {
                    at_ms: start,
                    leg: 0,
                    online: true,
                },
                books(cfg, start, "999.8"),
                books(cfg, start + 100, "999.8"),
                print_against(start + 110, HedgeSide::Buy, "999.7", "0.04"),
                books(cfg, start + 210, "999.8"),
                books(cfg, start + 310, "999.8"), // Paired (`net == 0`)
                books(cfg, start + 1_000, "999.8"),
                // Simultaneous dual-empty collector refresh at +1,020 ms (like W01/W05 ETH).
                both_empty(cfg, start + 1_020),
                both_empty(cfg, start + 1_000 + recovery_ms - 10),
                books(cfg, start + 1_000 + recovery_ms, "999.8"),
                books(cfg, start + 1_000 + recovery_ms + 300, "999.8"),
                books(cfg, start + 1_000 + recovery_ms + 500, "999.8"),
                books(cfg, start + 1_000 + recovery_ms + 650, "999.8"),
                print_against(
                    start + 1_000 + recovery_ms + 700,
                    HedgeSide::Sell,
                    "1000.3",
                    "0.04",
                ),
                books(cfg, start + 1_000 + recovery_ms + 850, "999.8"),
                books(cfg, start + 1_000 + recovery_ms + 950, "999.8"),
                MakerFirstEvent::TradeFeed {
                    at_ms: start + 1_000 + recovery_ms + 1_000,
                    leg: 0,
                    online: false,
                },
                books(cfg, start + 16_000, "999.8"),
                MakerFirstEvent::End { at_ms: cfg.end_ms },
            ]
        };

        // 150 ms simultaneous reconnect blip:
        // - strict (`None`) immediately triggers forced IOC liquidation;
        // - graceful (`Some(500)`) rides out the 150 ms blip and closes passively!
        let rep_strict = run_maker_first(
            &strict,
            build_events(&strict, 150).into_iter().map(Ok),
            &VenueRegistry::default(),
        )
        .unwrap();
        let rep_graceful = run_maker_first(
            &graceful,
            build_events(&graceful, 150).into_iter().map(Ok),
            &VenueRegistry::default(),
        )
        .unwrap();
        assert_eq!(rep_strict.cycle_attribution.forced_ioc_cycles, 1);
        assert_eq!(rep_graceful.cycle_attribution.forced_ioc_cycles, 0);
        assert_eq!(rep_graceful.cycle_attribution.closing_maker_fills, 1);

        // 700 ms dual outage (> 500 ms grace) MUST still fail closed with forced IOC!
        let rep_outage = run_maker_first(
            &graceful,
            build_events(&graceful, 700).into_iter().map(Ok),
            &VenueRegistry::default(),
        )
        .unwrap();
        assert_eq!(rep_outage.cycle_attribution.forced_ioc_cycles, 1);
    }

    #[test]
    fn passive_loop_only_and_entry_markout_gate_enforce_s9_3_arb_rules() {
        let registry = VenueRegistry::default();
        // 1) Passive-loop-only stays passive past max_wait_ms instead of triggering taker externalize.
        let mut base_cfg = passive_config(HedgeSide::Buy);
        base_cfg.unhedged_timeout_ms = 3_000;
        base_cfg.hold_ms = 5_000;
        base_cfg.end_ms = base_cfg.start_ms + 30_000;
        {
            let passive_policy = base_cfg.passive_offset.as_mut().unwrap();
            passive_policy.max_wait_ms = 1_200;
            passive_policy.min_offset_edge_bps = Some(d("3.0"));
        }
        let mk_events = |cfg: &MakerFirstConfig| {
            let t = cfg.start_ms;
            vec![
                MakerFirstEvent::TradeFeed {
                    at_ms: t,
                    leg: 0,
                    online: true,
                },
                books(cfg, t, "999.8"),
                books(cfg, t + 100, "999.8"),
                print_against(t + 110, HedgeSide::Buy, "999.7", "0.04"),
                books(cfg, t + 210, "999.8"),
                books(cfg, t + 1_350, "999.8"),
                books(cfg, t + 1_450, "999.8"),
                print_against(t + 1_500, HedgeSide::Sell, "1000.3", "0.04"),
                books(cfg, t + 1_600, "999.8"),
                MakerFirstEvent::End { at_ms: cfg.end_ms },
            ]
        };
        let active_res = run_maker_first(
            &base_cfg,
            mk_events(&base_cfg).into_iter().map(Ok),
            &registry,
        )
        .unwrap();
        assert!(
            active_res.ioc_submitted >= 1,
            "control with passive_loop_only=false must trigger taker IOC after max_wait_ms: {active_res:?}"
        );
        assert_eq!(active_res.passive_only_cycles, 0);

        let mut p_cfg = base_cfg;
        p_cfg.passive_offset.as_mut().unwrap().passive_loop_only = true;
        let p_res =
            run_maker_first(&p_cfg, mk_events(&p_cfg).into_iter().map(Ok), &registry).unwrap();
        assert_eq!(
            p_res.ioc_submitted, 0,
            "passive_loop_only must avoid taker IOCs: {p_res:?}"
        );
        assert_eq!(p_res.passive_only_cycles, 1);

        // 2) EntryMarkoutGate enforces quote_distance >= hedge_taker_fee + prior_adverse_markout + min_profit.
        let mut g_cfg = config();
        g_cfg.entry_side = Some(HedgeSide::Buy);
        g_cfg.maker_fee_bps = Decimal::ZERO;
        g_cfg.pair.legs[1].taker_fee_bps = d("3.0");
        g_cfg.entry_markout_gate = Some(EntryMarkoutGate {
            horizon_ms: 1_000,
            prior_adverse_markout_bps: d("2.0"),
            min_profit_bps: d("1.0"),
            quantile_p: 0.5,
            max_samples: 16,
            include_hedge_taker_fee: true,
            max_sample_age_ms: None,
        });
        g_cfg.end_ms = g_cfg.start_ms + 10_000;
        // Bid @ 999.8 vs hedge mid 1000.0 is 2.0 bps < required (3.0 + 2.0 + 1.0 = 6.0 bps) -> blocked!
        let g_events = vec![
            MakerFirstEvent::TradeFeed {
                at_ms: g_cfg.start_ms,
                leg: 0,
                online: true,
            },
            books(&g_cfg, g_cfg.start_ms, "999.8"),
            books(&g_cfg, g_cfg.start_ms + 500, "999.8"),
            MakerFirstEvent::End {
                at_ms: g_cfg.end_ms,
            },
        ];
        let g_res = run_maker_first(&g_cfg, g_events.into_iter().map(Ok), &registry).unwrap();
        assert_eq!(g_res.quotes_submitted, 0);
        let gate_rep = g_res.entry_markout_gate.unwrap();
        assert!(gate_rep.blocked_quote_distance > 0);
        assert_eq!(gate_rep.last_required_distance_bps, d("6.0"));
        assert!(
            g_res
                .entry_blocked
                .contains_key("entry_markout_quote_distance")
        );
    }

    #[test]
    fn entry_hedge_diagnostics_is_read_only_and_attributes_price_miss_retries() {
        let registry = VenueRegistry::default();
        let make_books = |cfg: &MakerFirstConfig, at: u64, hedge_bid: &str| {
            let level = |price: &str, contracts: &str| Level {
                price: d(price),
                contracts: d(contracts),
            };
            MakerFirstEvent::Book {
                at_ms: at,
                books: [0, 1].map(|i| Book {
                    market: cfg.pair.legs[i].market.clone(),
                    timestamp_ms: at,
                    tradable: true,
                    bids: vec![level(if i == 0 { "1000.0" } else { hedge_bid }, "1")],
                    asks: vec![level(if i == 0 { "1000.2" } else { "1000.4" }, "1")],
                    bounds: None,
                    available_notional: Decimal::from(500),
                }),
                reference: None,
                equity_session: None,
                leg_oracles: None,
            }
        };
        let mut base = config();
        base.entry_side = Some(HedgeSide::Buy);
        base.pair.limit_buffer_bps = 0.0;
        base.pair.max_depth_impact_bps = 3.0;
        base.transport_ms = 10;
        base.fill_notice_ms = 100;
        base.taker_delay_ms = [300, 300];
        base.reprice_ms = 400;
        base.hold_ms = 5_000;
        base.unhedged_timeout_ms = 4_000;
        base.end_ms = base.start_ms + 15_000;

        let t0 = base.start_ms;
        let events = |cfg: &MakerFirstConfig| {
            vec![
                MakerFirstEvent::TradeFeed {
                    at_ms: t0,
                    leg: 0,
                    online: true,
                },
                make_books(cfg, t0, "1000.2"),
                // Quote activates on the book frame at t0 + 20 (>= t0 + transport_ms = t0 + 10).
                make_books(cfg, t0 + 20, "1000.2"),
                // Maker buy fill at t0 + 200 (notice arrives at t0 + 300).
                make_books(cfg, t0 + 200, "1000.2"),
                partial(t0 + 200, t0 + 200, "1.1"),
                // Notice arrives at t0 + 300 -> IOC #1 submits with limit 1000.2, arrives at t0 + 610.
                make_books(cfg, t0 + 300, "1000.2"),
                // At t0 + 610 hedge bid drops 4 bps to 999.8 -> IOC #1 price-misses!
                // Because reprice_ms = 400 (next_ioc_ms = t0 + 700), t0 + 610 & t0 + 650 are throttled.
                make_books(cfg, t0 + 610, "999.8"),
                make_books(cfg, t0 + 650, "999.8"),
                // At t0 + 700 IOC #2 submits with limit 999.8, arrives at t0 + 1010 and fills!
                make_books(cfg, t0 + 700, "999.8"),
                make_books(cfg, t0 + 1_010, "999.8"),
                // Drain at hold_ms and flatten cleanly.
                make_books(cfg, t0 + 5_500, "999.8"),
                make_books(cfg, t0 + 6_000, "999.8"),
                make_books(cfg, t0 + 6_600, "999.8"),
                make_books(cfg, t0 + 7_200, "999.8"),
                make_books(cfg, t0 + 8_000, "999.8"),
                MakerFirstEvent::End { at_ms: cfg.end_ms },
            ]
        };

        let silent = run_maker_first(&base, events(&base).into_iter().map(Ok), &registry).unwrap();
        assert!(silent.entry_hedge_diagnostics.is_none());
        let silent_json = serde_json::to_string(&silent).unwrap();
        assert!(!silent_json.contains("entry_hedge_diagnostics"));

        let mut diag_cfg = config();
        diag_cfg.entry_side = base.entry_side;
        diag_cfg.pair.limit_buffer_bps = base.pair.limit_buffer_bps;
        diag_cfg.pair.max_depth_impact_bps = base.pair.max_depth_impact_bps;
        diag_cfg.transport_ms = base.transport_ms;
        diag_cfg.fill_notice_ms = base.fill_notice_ms;
        diag_cfg.taker_delay_ms = base.taker_delay_ms;
        diag_cfg.reprice_ms = base.reprice_ms;
        diag_cfg.hold_ms = base.hold_ms;
        diag_cfg.unhedged_timeout_ms = base.unhedged_timeout_ms;
        diag_cfg.end_ms = base.end_ms;
        diag_cfg.diagnose_entry_hedge = true;

        let diag =
            run_maker_first(&diag_cfg, events(&diag_cfg).into_iter().map(Ok), &registry).unwrap();
        // Purely read-only: identical cash, fills, misses, and unhedged time.
        assert_eq!(diag.net_pnl, silent.net_pnl);
        assert_eq!(diag.maker_fills, silent.maker_fills);
        assert_eq!(diag.ioc_fills, silent.ioc_fills);
        assert_eq!(diag.ioc_price_misses, silent.ioc_price_misses);
        assert_eq!(diag.total_unhedged_ms, silent.total_unhedged_ms);

        let report = diag
            .entry_hedge_diagnostics
            .as_ref()
            .expect("entry hedge diagnostics enabled");
        assert_eq!(report.episodes.len(), 1);
        let ep = &report.episodes[0];
        assert_eq!(ep.outcome, "hedged");
        assert_eq!(ep.opening_fill_ms, t0 + 200);
        assert_eq!(ep.first_notice_ms, Some(t0 + 300));
        assert_eq!(ep.first_ioc_submit_ms, Some(t0 + 300));
        assert_eq!(ep.first_hedge_fill_ms, Some(t0 + 1_010));
        assert_eq!(ep.fully_hedged_ms, Some(t0 + 1_010));
        assert_eq!(ep.first_available_hedge_touch_at_fill, Some(d("1000.2")));
        assert_eq!(ep.first_available_hedge_touch_at_notice, Some(d("1000.2")));
        assert_eq!(ep.first_ioc_submit_touch, Some(d("1000.2")));
        assert_eq!(ep.first_ioc_limit, Some(d("1000.2")));
        assert_eq!(ep.entry_hedge_vwap, Some(d("999.8")));
        assert_eq!(ep.ioc_price_misses, 1);
        assert_eq!(ep.ioc_submitted, 2);
        assert_eq!(ep.ioc_fills, 1);
        assert!(ep.reprice_throttle_waits >= 1);
        assert_eq!(ep.entry_unhedged_ms, Some(810));
        assert_eq!(
            ep.notice_wait_ms.unwrap()
                + ep.submit_wait_ms.unwrap()
                + ep.ioc_flight_and_retry_ms.unwrap(),
            ep.entry_unhedged_ms.unwrap()
        );
        assert_eq!(report.total_entry_unhedged_ms, 810);
        assert_eq!(ep.risk_resolved_ms, ep.fully_hedged_ms);
        assert_eq!(ep.forced_drain_triggered_ms, None);

        // The same opening fill can instead time out before any partner IOC.
        diag_cfg.unhedged_timeout_ms = 100;
        let drained =
            run_maker_first(&diag_cfg, events(&diag_cfg).into_iter().map(Ok), &registry).unwrap();
        let report = drained.entry_hedge_diagnostics.as_ref().unwrap();
        let ep = &report.episodes[0];
        assert_eq!(ep.outcome, "forced_drain");
        assert_eq!(ep.forced_drain_triggered_ms, Some(t0 + 300));
        assert_eq!(ep.risk_resolved_ms, Some(t0 + 610));
        assert_eq!(ep.entry_unhedged_ms, Some(410));
        assert_eq!(report.total_entry_unhedged_ms, drained.total_unhedged_ms);
        assert_eq!(ep.entry_hedge_base, Decimal::ZERO);
        assert_eq!(ep.fully_hedged_ms, None);
        assert_eq!(ep.ioc_flight_and_retry_ms, None);

        let no_unwind_book = events(&diag_cfg)
            .into_iter()
            .filter(|e| e.time() <= t0 + 300 || matches!(e, MakerFirstEvent::End { .. }));
        let open = run_maker_first(&diag_cfg, no_unwind_book.map(Ok), &registry).unwrap();
        let report = open.entry_hedge_diagnostics.as_ref().unwrap();
        let ep = &report.episodes[0];
        assert_eq!(ep.outcome, "open_at_cutoff");
        assert_eq!(ep.forced_drain_triggered_ms, Some(t0 + 300));
        assert_eq!(ep.fully_hedged_ms, None);
        assert_eq!(ep.risk_resolved_ms, None);
        assert_eq!(ep.entry_unhedged_ms, None);
        assert_eq!(
            ep.censored_unhedged_ms,
            Some(diag_cfg.end_ms - ep.opening_fill_ms)
        );
        assert_eq!(report.total_censored_unhedged_ms, open.total_unhedged_ms);
        assert_eq!(report.total_entry_unhedged_ms, 0);
    }

    #[test]
    fn hedge_ioc_policy_avoids_price_miss_and_cuts_unhedged_lag() {
        let registry = VenueRegistry::default();
        let make_books = |cfg: &MakerFirstConfig, at: u64, hedge_bid: &str| {
            let level = |price: &str, contracts: &str| Level {
                price: d(price),
                contracts: d(contracts),
            };
            MakerFirstEvent::Book {
                at_ms: at,
                books: [0, 1].map(|i| Book {
                    market: cfg.pair.legs[i].market.clone(),
                    timestamp_ms: at,
                    tradable: true,
                    bids: vec![level(if i == 0 { "1000.0" } else { hedge_bid }, "1")],
                    asks: vec![level(if i == 0 { "1000.2" } else { "1000.4" }, "1")],
                    bounds: None,
                    available_notional: Decimal::from(500),
                }),
                reference: None,
                equity_session: None,
                leg_oracles: None,
            }
        };
        let mut cfg = config();
        cfg.entry_side = Some(HedgeSide::Buy);
        cfg.pair.limit_buffer_bps = 0.0;
        cfg.pair.max_depth_impact_bps = 3.0;
        cfg.transport_ms = 10;
        cfg.fill_notice_ms = 100;
        cfg.taker_delay_ms = [300, 300];
        cfg.reprice_ms = 400;
        cfg.hold_ms = 5_000;
        cfg.unhedged_timeout_ms = 4_000;
        cfg.end_ms = cfg.start_ms + 15_000;
        cfg.diagnose_entry_hedge = true;
        cfg.hedge_ioc_policy = Some(HedgeIocPolicy {
            limit_buffer_bps: d("10"),
            max_depth_impact_bps: Some(d("15")),
            retry_ms: Some(25),
            entry_only: false,
        });

        let t0 = cfg.start_ms;
        let events = vec![
            MakerFirstEvent::TradeFeed {
                at_ms: t0,
                leg: 0,
                online: true,
            },
            make_books(&cfg, t0, "1000.2"),
            make_books(&cfg, t0 + 20, "1000.2"),
            make_books(&cfg, t0 + 200, "1000.2"),
            partial(t0 + 200, t0 + 200, "1.1"),
            make_books(&cfg, t0 + 300, "1000.2"),
            // Hedge bid drops 4 bps during 310 ms flight; 10 bps hedge IOC buffer absorbs it on IOC #1.
            make_books(&cfg, t0 + 610, "999.8"),
            make_books(&cfg, t0 + 5_500, "999.8"),
            make_books(&cfg, t0 + 6_000, "999.8"),
            make_books(&cfg, t0 + 6_600, "999.8"),
            make_books(&cfg, t0 + 7_200, "999.8"),
            MakerFirstEvent::End { at_ms: cfg.end_ms },
        ];

        let res = run_maker_first(&cfg, events.into_iter().map(Ok), &registry).unwrap();
        let diag = res.entry_hedge_diagnostics.as_ref().unwrap();
        assert_eq!(diag.episodes.len(), 1);
        let ep = &diag.episodes[0];
        assert_eq!(ep.outcome, "hedged");
        assert_eq!(ep.ioc_price_misses, 0);
        assert_eq!(ep.ioc_submitted, 1);
        assert_eq!(ep.ioc_fills, 1);
        assert_eq!(ep.entry_unhedged_ms, Some(410));
        assert_eq!(ep.entry_hedge_vwap, Some(d("999.8")));
        assert!(ep.first_ioc_limit.unwrap() < d("1000.2"));
    }

    #[test]
    fn hedge_ioc_policy_validation_rejects_invalid_bounds() {
        let registry = VenueRegistry::default();
        let bad_cases = [
            // limit_buffer > inherited pair.max_depth_impact_bps (5 > 3)
            HedgeIocPolicy {
                limit_buffer_bps: d("5"),
                max_depth_impact_bps: None,
                retry_ms: None,
                entry_only: false,
            },
            // limit_buffer == explicit max_depth_impact_bps (nonzero buffer needs tick headroom)
            HedgeIocPolicy {
                limit_buffer_bps: d("10"),
                max_depth_impact_bps: Some(d("10")),
                retry_ms: Some(50),
                entry_only: false,
            },
            // limit_buffer > explicit max_depth_impact_bps
            HedgeIocPolicy {
                limit_buffer_bps: d("15"),
                max_depth_impact_bps: Some(d("10")),
                retry_ms: Some(50),
                entry_only: false,
            },
            // negative buffer
            HedgeIocPolicy {
                limit_buffer_bps: d("-1"),
                max_depth_impact_bps: Some(d("10")),
                retry_ms: Some(50),
                entry_only: false,
            },
            // retry_ms == 0
            HedgeIocPolicy {
                limit_buffer_bps: d("5"),
                max_depth_impact_bps: Some(d("10")),
                retry_ms: Some(0),
                entry_only: false,
            },
            // retry_ms > 2000
            HedgeIocPolicy {
                limit_buffer_bps: d("5"),
                max_depth_impact_bps: Some(d("10")),
                retry_ms: Some(2_001),
                entry_only: false,
            },
        ];
        for policy in bad_cases {
            let mut cfg = config();
            cfg.pair.max_depth_impact_bps = 3.0;
            cfg.hedge_ioc_policy = Some(policy);
            assert!(
                cfg.validate(&registry).is_err(),
                "expected invalid hedge IOC policy to fail validation: {policy:?}"
            );
        }
    }

    #[test]
    fn hedge_ioc_policy_entry_only_widens_entry_without_widening_unwind_ioc() {
        let registry = VenueRegistry::default();
        let make_books = |cfg: &MakerFirstConfig, at: u64, maker_bid: &str, hedge_bid: &str| {
            let level = |price: &str, contracts: &str| Level {
                price: d(price),
                contracts: d(contracts),
            };
            MakerFirstEvent::Book {
                at_ms: at,
                books: [0, 1].map(|i| Book {
                    market: cfg.pair.legs[i].market.clone(),
                    timestamp_ms: at,
                    tradable: true,
                    bids: vec![level(if i == 0 { maker_bid } else { hedge_bid }, "1")],
                    asks: vec![level(if i == 0 { "1000.2" } else { "1000.4" }, "1")],
                    bounds: None,
                    available_notional: Decimal::from(500),
                }),
                reference: None,
                equity_session: None,
                leg_oracles: None,
            }
        };
        let run_case = |entry_only: bool| {
            let mut cfg = config();
            cfg.entry_side = Some(HedgeSide::Buy);
            cfg.pair.limit_buffer_bps = 0.0;
            cfg.pair.max_depth_impact_bps = 3.0;
            cfg.transport_ms = 10;
            cfg.fill_notice_ms = 100;
            cfg.taker_delay_ms = [300, 300];
            cfg.reprice_ms = 100;
            cfg.hold_ms = 5_000;
            cfg.unhedged_timeout_ms = 4_000;
            cfg.end_ms = cfg.start_ms + 15_000;
            cfg.diagnose_entry_hedge = true;
            cfg.hedge_ioc_policy = Some(HedgeIocPolicy {
                limit_buffer_bps: d("10"),
                max_depth_impact_bps: Some(d("15")),
                retry_ms: Some(25),
                entry_only,
            });
            let t0 = cfg.start_ms;
            let events = vec![
                MakerFirstEvent::TradeFeed {
                    at_ms: t0,
                    leg: 0,
                    online: true,
                },
                make_books(&cfg, t0, "1000.0", "1000.2"),
                make_books(&cfg, t0 + 20, "1000.0", "1000.2"),
                make_books(&cfg, t0 + 200, "1000.0", "1000.2"),
                partial(t0 + 200, t0 + 200, "1.1"),
                make_books(&cfg, t0 + 300, "1000.0", "1000.2"),
                // Entry hedge adverse move (-4 bps): absorbed in BOTH entry_only=false and entry_only=true.
                make_books(&cfg, t0 + 610, "1000.0", "999.8"),
                // Drain triggers at t0 + 5_500 (opening_timeout): unwind sells leg 0 bid @ 1000.0,
                // arrives at t0 + 5_810 when leg 0 bid drops 4 bps to 999.6.
                make_books(&cfg, t0 + 5_500, "1000.0", "999.8"),
                make_books(&cfg, t0 + 5_810, "999.6", "999.8"),
                // Subsequent frames let both cases finish flattening cleanly.
                make_books(&cfg, t0 + 6_200, "999.6", "999.8"),
                make_books(&cfg, t0 + 6_600, "999.6", "999.8"),
                make_books(&cfg, t0 + 7_000, "999.6", "999.8"),
                make_books(&cfg, t0 + 7_500, "999.6", "999.8"),
                MakerFirstEvent::End { at_ms: cfg.end_ms },
            ];
            run_maker_first(&cfg, events.into_iter().map(Ok), &registry).unwrap()
        };

        let all_ioc = run_case(false);
        let entry_only = run_case(true);

        // Both absorb the entry-hedge adverse move on IOC #1 with zero entry misses.
        assert_eq!(
            all_ioc.entry_hedge_diagnostics.as_ref().unwrap().episodes[0].ioc_price_misses,
            0
        );
        assert_eq!(
            entry_only
                .entry_hedge_diagnostics
                .as_ref()
                .unwrap()
                .episodes[0]
                .ioc_price_misses,
            0
        );
        assert_eq!(
            entry_only
                .entry_hedge_diagnostics
                .as_ref()
                .unwrap()
                .episodes[0]
                .entry_unhedged_ms,
            Some(410)
        );
        // Unwind IOC under entry_only=false widens and absorbs the 4 bps unwind move (0 total misses);
        // under entry_only=true unwind retains conservative pair.limit_buffer_bps=0 and records 1 miss.
        assert_eq!(all_ioc.ioc_price_misses, 0);
        assert_eq!(entry_only.ioc_price_misses, 1);
    }

    #[test]
    fn entry_only_retry_clock_never_delays_forced_drain_and_preserves_reprice_spacing() {
        let registry = VenueRegistry::default();
        let make_books = |cfg: &MakerFirstConfig, at: u64, maker_bid: &str, hedge_bid: &str| {
            let level = |price: &str, contracts: &str| Level {
                price: d(price),
                contracts: d(contracts),
            };
            MakerFirstEvent::Book {
                at_ms: at,
                books: [0, 1].map(|i| Book {
                    market: cfg.pair.legs[i].market.clone(),
                    timestamp_ms: at,
                    tradable: true,
                    bids: vec![level(if i == 0 { maker_bid } else { hedge_bid }, "1")],
                    asks: vec![level(if i == 0 { "1000.2" } else { "1000.4" }, "1")],
                    bounds: None,
                    available_notional: Decimal::from(500),
                }),
                reference: None,
                equity_session: None,
                leg_oracles: None,
            }
        };
        // 1) Codex `15:36 CST` counterexample: entry_only retry_ms=2000 must NOT delay forced
        // drain triggered at t0+1000 after an entry IOC submitted at t0+300 misses.
        let run_drain_case = |retry_ms: u64| {
            let mut cfg = config();
            cfg.entry_side = Some(HedgeSide::Buy);
            cfg.transport_ms = 10;
            cfg.fill_notice_ms = 100;
            cfg.taker_delay_ms = [300, 300];
            cfg.reprice_ms = 100;
            cfg.hold_ms = 1_300;
            cfg.unhedged_timeout_ms = 700;
            cfg.end_ms = cfg.start_ms + 15_000;
            cfg.diagnose_entry_hedge = true;
            cfg.hedge_ioc_policy = Some(HedgeIocPolicy {
                limit_buffer_bps: Decimal::ZERO,
                max_depth_impact_bps: None,
                retry_ms: Some(retry_ms),
                entry_only: true,
            });
            let t0 = cfg.start_ms;
            let mut events = vec![
                MakerFirstEvent::TradeFeed {
                    at_ms: t0,
                    leg: 0,
                    online: true,
                },
                make_books(&cfg, t0, "1000.0", "1000.2"),
                make_books(&cfg, t0 + 20, "1000.0", "1000.2"),
                make_books(&cfg, t0 + 200, "1000.0", "1000.2"),
                partial(t0 + 200, t0 + 200, "1.1"),
                make_books(&cfg, t0 + 300, "1000.0", "1000.2"),
            ];
            for ms in [
                1_000, 1_010, 1_310, 1_400, 1_600, 2_000, 2_300, 2_610, 2_800, 3_200, 4_000, 6_000,
            ] {
                events.push(make_books(&cfg, t0 + ms, "1000.0", "998.0"));
            }
            events.push(MakerFirstEvent::End { at_ms: cfg.end_ms });
            let out = run_maker_first(&cfg, events.into_iter().map(Ok), &registry).unwrap();
            let ep = &out.entry_hedge_diagnostics.as_ref().unwrap().episodes[0];
            (
                ep.first_ioc_submit_ms.unwrap() - t0,
                ep.forced_drain_triggered_ms.unwrap() - t0,
                ep.risk_resolved_ms.unwrap() - t0,
                ep.ioc_price_misses,
                out.forced_closes,
            )
        };
        let base = run_drain_case(100);
        let slow = run_drain_case(2_000);
        assert_eq!(base, (300, 1_000, 1_310, 1, 1));
        assert_eq!(
            slow, base,
            "entry_only retry_ms=2000 must not delay forced drain beyond reprice_ms"
        );

        // 2) Within the entry phase (not draining), entry_only retry_ms=2000 STILL throttles
        // subsequent entry retries while preserving reprice_ms for non-entry phases.
        let mut entry_cfg = config();
        entry_cfg.entry_side = Some(HedgeSide::Buy);
        entry_cfg.transport_ms = 10;
        entry_cfg.fill_notice_ms = 100;
        entry_cfg.taker_delay_ms = [300, 300];
        entry_cfg.reprice_ms = 100;
        entry_cfg.hold_ms = 5_000;
        entry_cfg.unhedged_timeout_ms = 4_000;
        entry_cfg.end_ms = entry_cfg.start_ms + 15_000;
        entry_cfg.diagnose_entry_hedge = true;
        entry_cfg.hedge_ioc_policy = Some(HedgeIocPolicy {
            limit_buffer_bps: Decimal::ZERO,
            max_depth_impact_bps: None,
            retry_ms: Some(2_000),
            entry_only: true,
        });
        let t0 = entry_cfg.start_ms;
        let mut entry_events = vec![
            MakerFirstEvent::TradeFeed {
                at_ms: t0,
                leg: 0,
                online: true,
            },
            make_books(&entry_cfg, t0, "1000.0", "1000.2"),
            make_books(&entry_cfg, t0 + 20, "1000.0", "1000.2"),
            make_books(&entry_cfg, t0 + 200, "1000.0", "1000.2"),
            partial(t0 + 200, t0 + 200, "1.1"),
            make_books(&entry_cfg, t0 + 300, "1000.0", "1000.2"),
            // IOC #1 misses at t0+610; t0+700 & t0+1500 must be throttled by next_entry_ioc_ms=2300.
            make_books(&entry_cfg, t0 + 610, "1000.0", "998.0"),
            make_books(&entry_cfg, t0 + 700, "1000.0", "998.0"),
            make_books(&entry_cfg, t0 + 1_500, "1000.0", "998.0"),
            // IOC #2 submits at t0+2300 and fills at t0+2610.
            make_books(&entry_cfg, t0 + 2_300, "1000.0", "998.0"),
            make_books(&entry_cfg, t0 + 2_610, "1000.0", "998.0"),
        ];
        for ms in [5_500, 6_000, 6_600, 7_200] {
            entry_events.push(make_books(&entry_cfg, t0 + ms, "1000.0", "998.0"));
        }
        entry_events.push(MakerFirstEvent::End {
            at_ms: entry_cfg.end_ms,
        });
        let entry_out =
            run_maker_first(&entry_cfg, entry_events.into_iter().map(Ok), &registry).unwrap();
        let entry_ep = &entry_out.entry_hedge_diagnostics.as_ref().unwrap().episodes[0];
        assert_eq!(entry_ep.outcome, "hedged");
        assert!(entry_ep.reprice_throttle_waits >= 2);
        assert_eq!(entry_ep.fully_hedged_ms, Some(t0 + 2_610));
    }
}
