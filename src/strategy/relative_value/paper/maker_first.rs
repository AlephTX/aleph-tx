//! Public-tape maker-first replay. The ledger and protected depth model are
//! shared with the sequential IOC simulator.

use super::{Fill, IncentiveTurnoverReport, Ledger, sweep};
use crate::strategy::relative_value::*;
use std::collections::{BTreeMap, VecDeque};

mod basis_reversion;
mod basis_risk;
mod costs;
mod entry_flow;
mod funding;
mod posture;
use basis_reversion::BasisReversionState;
pub use basis_reversion::{BasisReversionReport, MakerBasisReversion};
use basis_risk::BasisRiskState;
pub use basis_risk::{BasisRiskReport, BasisRiskReserve};
use entry_flow::EntryFlowState;
pub use entry_flow::{EntryFlowFilter, EntryFlowReport};
pub use funding::{FlatAtFunding, FundingFlatReport};
use funding::{FundingState, TimerKind};
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
    pub execution_supported: bool,
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
    /// Offline diagnostics only; never changes admission or execution.
    #[serde(default)]
    pub diagnose_cycles: bool,
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
    pub conservative_cancel_guard: bool,
    pub cancel_guard_overlap_prints: usize,
    pub cancel_guard_overlap_fills: usize,
    pub maker_fills_after_cancel_request: usize,
    #[serde(with = "rust_decimal::serde::str")]
    pub cancel_guard_overlap_turnover: Decimal,
    pub start_ms: u64,
    pub end_ms: u64,
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
    pub incomplete: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub funding_flat: Option<FundingFlatReport>,
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
    pub sub_min_top_up_unwinds: usize,
    #[serde(serialize_with = "super::serialize_positions")]
    pub sub_min_recovery_turnover: [Decimal; 2],
    pub paired_drain_blocked: usize,
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

struct Cycle {
    posture: TurnoverPosture,
    started_ms: u64,
    initial_cash: Decimal,
    initial_turnover: Decimal,
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
}

struct Replay<'a> {
    config: &'a MakerFirstConfig,
    rules: Rules,
    common_step: Decimal,
    ledger: Ledger,
    books: Option<[Book; 2]>,
    usable: [bool; 2],
    last_usable_at_ms: [Option<u64>; 2],
    consumed_ms: [u64; 2],
    last_source_ms: [u64; 2],
    feed_online: bool,
    quote: Option<Quote>,
    ioc: Option<Ioc>,
    cycle: Option<Cycle>,
    next_quote_ms: u64,
    next_ioc_ms: u64,
    notices: VecDeque<(u64, Decimal)>,
    unhedged_since: Option<u64>,
    cost_feedback: CostFeedback,
    basis_risk: BasisRiskState,
    basis_reversion: BasisReversionState,
    entry_flow: EntryFlowState,
    posture: PostureState,
    funding: Option<FundingState>,
    report: MakerFirstReport,
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
        ensure!(
            (self.pair.asset_class == AssetClass::Crypto || self.flat_at_funding.is_some())
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
                && self.entry_horizon_ms() < self.end_ms - self.start_ms
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
                self.pair
                    .mode
                    .incentive()
                    .is_some_and(|p| p.eligible_leg == self.maker_leg)
                    && policy.min_reserve_bps > Decimal::ZERO
                    && (policy.min_reserve_bps..=max_bps).contains(&policy.max_reserve_bps)
                    && (Decimal::ZERO..=Decimal::ONE).contains(&policy.alpha)
                    && (Decimal::ZERO..=max_bps).contains(&policy.overhead_bps)
                    && (Decimal::ZERO..=max_bps).contains(&policy.max_quote_distance_bps),
                "invalid maker-first turnover feedback"
            );
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
        if let Some(policy) = self.flat_at_funding {
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
        let supported = slots == [0; 2] || self.flat_at_funding.is_some();
        Ok(MakerFirstPreflight {
            scope: "configuration_only_no_tape_validated",
            start_ms: self.start_ms,
            end_ms: self.end_ms,
            duration_ms: self.end_ms - self.start_ms,
            execution_supported: supported,
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
            required_evidence: if self.flat_at_funding.is_some() {
                vec![
                    "verified_funding_schedule_and_settlement_guard",
                    "bounded_source_to_receipt_trade_delivery",
                    "public_trade_and_book_continuity_across_payment_boundaries",
                    "flat_positions_and_resolved_orders_at_every_funding_guard",
                ]
            } else if supported {
                vec![]
            } else {
                vec![
                    "verified_realized_or_witnessed_payment_for_each_held_slot",
                    "venue_market_currency_and_funding_schedule_provenance",
                    "public_trade_and_book_continuity_across_payment_boundaries",
                ]
            },
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

    fn funding_flat(&self) -> bool {
        self.ledger.is_flat()
            && self.quote.is_none()
            && self.ioc.is_none()
            && self.notices.is_empty()
            && self.cycle.is_none()
    }

    fn funding_timers(&mut self, until: u64) -> Result<()> {
        loop {
            let timer = self.funding.as_mut().and_then(|state| {
                state.timers.front().filter(|(at, _)| *at <= until)?;
                state.timers.pop_front()
            });
            let Some((now, kind)) = timer else {
                return Ok(());
            };
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
        let cancelled_by = cycle.started_ms + policy.max_wait_ms;
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
                let maker = b[self.maker()].mid();
                let hedge = b[1 - self.maker()].mid();
                self.basis_reversion.observe(
                    policy,
                    now,
                    Some((
                        (maker - hedge) / hedge * BPS,
                        [b[0].timestamp_ms, b[1].timestamp_ms],
                    )),
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
        if quote.activated_ms.is_none() && now >= quote.arrival_ms {
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
                quote.ahead = level.map_or(Decimal::ZERO, |level| level.contracts)
                    * self.config.pair.legs[self.maker()].base_per_contract;
                quote.activated_ms = Some(now);
                self.report.quotes_accepted += 1;
                self.report.cycle_attribution.closing_quotes.accepted += usize::from(quote.closing);
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
                let limit = if paired_close {
                    self.reduce_limit(books[hedge_leg], hedge_leg, hedge_side, quote.left)
                } else {
                    self.limit(books[hedge_leg], hedge_leg, hedge_side, quote.left)
                };
                limit.is_some_and(|limit| {
                    let reference = previous * quote.left;
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
                        || signed_cost(hedge_side, limit * quote.left, reference)
                            <= reference * tol_bps / BPS
                })
            }),
            None => self.passive_cancel_by(now).is_some(),
        };
        let feedback_cancel = book_frame
            && self.config.turnover_feedback.is_some()
            && self.cycle.is_none()
            && !self.resting_turnover_within_budget(&quote, now);
        let basis_reversion_cancel = book_frame
            && self.cycle.is_none()
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
            && self.cycle.is_none()
            && self.config.entry_flow_filter.is_some_and(|p| {
                p.cancel_resting_entries && !self.entry_flow.permits_resting(p, quote.side, now)
            });
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
        } else {
            None
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
            if let Some(costs) = &mut self.report.execution_costs {
                costs.begin(now, reference_mids);
            }
            if let Some(i) = passive_side {
                self.report.passive_cycles_started[i] += 1;
            }
            self.report.turnover_posture.cycles_started[posture.index()] += 1;
            Cycle {
                posture,
                started_ms: now,
                initial_cash: self.ledger.cash,
                initial_turnover: self.ledger.turnover,
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
            }
        });
        cycle.maker_turnover += notional;
        cycle.closing_maker_filled |= closing && !offsets_inventory;
        if !closing {
            cycle.hedge_fill_at_ms = None;
        }
        self.report.maker_fills += 1;
        self.report.maker_buy_fills += usize::from(side == HedgeSide::Buy);
        self.report.maker_sell_fills += usize::from(side == HedgeSide::Sell);
        self.report.maker_turnover += notional;
        self.record_eligible_fill(i, source, notional);
        if let Some(mid) = midpoint {
            self.report.maker_spread_capture -= signed_cost(side, notional, base * mid);
        }
        let fee = self
            .ledger
            .trade(i, side, Fill { base, notional }, self.config.maker_fee_bps);
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
        }
        let signed_base = if side == HedgeSide::Buy { base } else { -base };
        self.notices
            .push_back((now + self.config.fill_notice_ms, signed_base));
        Ok(())
    }

    fn poll_ioc(&mut self, now: u64) -> Result<()> {
        let Some(order) = self.ioc.take() else {
            return Ok(());
        };
        if now >= order.expires_ms {
            self.report.ioc_expired += 1;
            return Ok(());
        }
        let Some(book) = self.fresh(order.leg, now).filter(|book| {
            book.timestamp_ms >= order.arrival_ms && book.timestamp_ms > self.consumed_ms[order.leg]
        }) else {
            self.ioc = Some(order);
            return Ok(());
        };
        let touch = book.side(order.side)[0].price;
        if !book.in_bounds(order.limit)
            || signed_cost(order.side, touch, order.limit) > Decimal::ZERO
        {
            self.report.ioc_price_misses += 1;
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
                    && self
                        .cycle
                        .as_ref()
                        .is_some_and(|c| c.sub_min_top_up_attempts < 1)
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
            return Ok(());
        }
        let fill_time = book.timestamp_ms;
        let fill_mid = book.mid();
        self.consumed_ms[order.leg] = fill_time;
        self.report.ioc_fills += 1;
        self.report.ioc_partial += usize::from(fill.base < order.quantity);
        self.report.ioc_slippage += signed_cost(order.side, fill.notional, fill.base * order.touch);
        self.report.ioc_submit_touch_turnover += fill.base * order.touch;
        self.report.ioc_depth_impact += signed_cost(order.side, fill.notional, fill.base * touch);
        self.report.taker_turnover += fill.notional;
        let notional = fill.notional;
        let fill_base = fill.base;
        let leg_before = self.ledger.positions[order.leg];
        let mut recovery_notional = Decimal::ZERO;
        if order.sub_min_top_up {
            let cycle = self
                .cycle
                .as_mut()
                .expect("sub-minimum top-up has an active cycle");
            cycle.sub_min_top_up_attempts += 1;
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
        if !order.unwind && !order.closing_hedge && self.net().is_zero() && !self.ledger.is_flat() {
            self.cycle
                .as_mut()
                .expect("opening hedge has an active cycle")
                .hedge_fill_at_ms = Some(now);
        }
        Ok(())
    }

    fn record_eligible_fill(&mut self, leg: usize, time: u64, notional: Decimal) {
        if self.config.turnover_feedback.is_some() && leg == self.maker() {
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

    fn submit_ioc(&mut self, now: u64, i: usize, position: Decimal, unwind: bool) -> Result<()> {
        if self.ioc.is_some() || position.is_zero() || now < self.next_ioc_ms {
            return Ok(());
        }
        self.next_ioc_ms = now + self.config.reprice_ms;
        let Some(book) = self.fresh(i, now) else {
            return Ok(());
        };
        let terms = &self.config.pair.legs[i];
        let side = direction(position);
        let curve = Depth::build(book, terms, side, self.rules);
        let Ok(last) = curve.last() else {
            return Ok(());
        };
        let closing_hedge = !unwind && self.cycle.as_ref().is_some_and(|c| c.closing_maker_filled);
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
        let touch = book.side(side)[0].price;
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
            && self
                .cycle
                .as_ref()
                .is_some_and(|c| c.sub_min_top_up_attempts < 1)
            && reduces_leg
            && self.ledger.positions[1 - i].is_zero()
            && leg.abs() < min_ioc_base
        {
            // Gateway-legal FOK top-up for venues (e.g. Arcus) where `reduce_only`
            // waives `min_notional` but still enforces `min_contracts`: submit one
            // non-reduce-only FOK of `min_entry + |stub|` opposing the stub so
            // post-fill inventory flips to `min_entry >= min_reduce_ioc_base`, which
            // the next drain cycle flattens to `0` via standard `reduce_only` IOC.
            let worst_price = last.limit.max(touch);
            let min_entry = min_order_base(terms, worst_price, false);
            let top_up_qty = floor_step(min_entry + leg.abs(), terms.base_step())?;
            let max_unhedged =
                Decimal::try_from(self.config.pair.planner.max_unhedged_notional_usd)
                    .unwrap_or(Decimal::ZERO);
            if top_up_qty >= min_entry + leg.abs()
                && top_up_qty <= last.base
                && let Ok(top_limit) = curve.limit(top_up_qty)
                && top_up_qty >= min_order_base(terms, top_limit, false)
                && top_up_qty * top_limit <= book.available_notional.min(self.rules.max_single_leg)
                && (top_up_qty - leg.abs()) * top_limit
                    <= self.rules.max_single_leg.min(max_unhedged)
            {
                quantity = top_up_qty;
                limit = top_limit;
                min_ioc_base = min_order_base(terms, top_limit, false);
                sub_min_top_up = true;
            }
        }
        if quantity.is_zero() || quantity < min_ioc_base {
            if unwind {
                self.report.below_minimum_unwind += 1;
            } else {
                self.report.below_minimum_hedge += 1;
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

    #[cfg(test)]
    fn candidate(
        &self,
        books: [&Book; 2],
        side: HedgeSide,
        closing: bool,
        level: &Level,
    ) -> Result<Option<Candidate>> {
        self.candidate_with_dislocation(books, side, closing, level, None)
    }

    fn candidate_with_dislocation(
        &self,
        books: [&Book; 2],
        side: HedgeSide,
        closing: bool,
        level: &Level,
        dislocation_bps: Option<Decimal>,
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
            let full_target = floor_step(
                (self.config.quote_notional.min(self.rules.max_single_leg) / mark)
                    .min(self.rules.max_base),
                self.common_step,
            )?;
            let existing = self.ledger.positions[i].abs();
            if existing > Decimal::ZERO {
                floor_step(
                    (full_target - existing).max(Decimal::ZERO),
                    maker.base_step(),
                )?
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
            && quantity * passive > books[i].available_notional.min(self.rules.max_single_leg)
        {
            return Ok(None);
        }
        let hedge_price = if hedged_exit.is_some() {
            self.reduce_limit(books[j], j, opposite(side), quantity)
        } else {
            self.limit(books[j], j, opposite(side), quantity)
        };
        let Some(hedge_price) = hedge_price else {
            return Ok(None);
        };
        let maker_cash = cash(side, passive * quantity, self.config.maker_fee_bps);
        let hedge_cash = cash(opposite(side), hedge_price * quantity, hedge.taker_fee_bps);
        let cycle_edge_bps = self.cycle.as_ref().filter(|_| closing).map(|cycle| {
            let cycle_cash = (maker_cash + hedge_cash) + (self.ledger.cash - cycle.initial_cash);
            self.edge_bps(cycle_cash, quantity * mark)
        });
        let mut candidate = Candidate {
            edge_bps: Decimal::ZERO,
            cycle_edge_bps,
            passive_entry: false,
            queue_ahead: level.contracts,
            price: passive,
            quantity,
            side,
            hedge_limit: Some(hedge_price),
            turnover: None,
        };
        let cash_edge = if hedged_exit.is_some() {
            // The paired hedge IOC is identical for both exits. Sunk cash
            // and that shared hedge cost cancel out of the comparison.
            let Some(ioc) = self.reduce_limit(books[i], i, side, quantity) else {
                return Ok(None);
            };
            maker_cash - cash(side, ioc * quantity, maker.taker_fee_bps)
        } else if let Some(cycle) = &self.cycle {
            (maker_cash + hedge_cash) + (self.ledger.cash - cycle.initial_cash)
        } else {
            let Some(edge) = self.opening_edge(
                books,
                &mut candidate,
                maker_cash,
                hedge_cash,
                dislocation_bps,
            ) else {
                return Ok(None);
            };
            edge
        };
        candidate.edge_bps = self.edge_bps(cash_edge, quantity * mark);
        Ok(Some(candidate))
    }

    fn opening_edge(
        &self,
        books: [&Book; 2],
        candidate: &mut Candidate,
        maker_cash: Decimal,
        hedge_cash: Decimal,
        dislocation_bps: Option<Decimal>,
    ) -> Option<Decimal> {
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
        let reverse = books[i].side(side)[0].price;
        let maker_fee = self.config.maker_fee_bps;
        let reverse_cash = cash(opposite(side), reverse * quantity, maker_fee);
        let passive_cash = maker_cash + reverse_cash;
        let offset = self
            .config
            .passive_offset
            .as_ref()
            .filter(|policy| quantity * books[i].asks[0].price <= policy.max_net_notional);
        if let Some(min_pe) = offset.and_then(|policy| policy.min_passive_edge_bps) {
            let passive_edge_bps = self.edge_bps(passive_cash, quantity * price);
            if passive_edge_bps < min_pe {
                return None;
            }
        }
        let probabilities = offset.and_then(|policy| policy.close_probability);
        let mode = self.config.pair.mode;
        if offset.is_some() && probabilities.is_none() && mode.is_pair_hedge() {
            candidate.passive_entry = true;
            return Some(passive_cash);
        }
        let hedge_exit = self.limit(books[j], j, side, quantity)?;
        let hedge_exit_cash = cash(side, hedge_exit * quantity, hedge.taker_fee_bps);
        let paired_cash = (maker_cash + hedge_cash) + (reverse_cash + hedge_exit_cash);
        if probabilities.is_none() && mode.is_pair_hedge() {
            let reversion_cash = match (self.config.basis_reversion, dislocation_bps) {
                (Some(policy), Some(disloc)) => {
                    policy.reversion_fraction * disloc / BPS * (quantity * books[j].mid())
                }
                _ => Decimal::ZERO,
            };
            return Some(paired_cash + reversion_cash);
        }
        let maker_exit = self.limit(books[i], i, opposite(side), quantity)?;
        let fallback_cash = maker_cash
            + hedge_cash
            + hedge_exit_cash
            + cash(opposite(side), maker_exit * quantity, maker.taker_fee_bps);
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
            let eligible = mode.incentive().map_or(Decimal::ZERO, |p| {
                if p.eligible_leg == i {
                    price + reverse.min(maker_exit)
                } else if candidate.passive_entry {
                    Decimal::ZERO
                } else {
                    hedge_round_trip
                }
            });
            let worst_cash = if candidate.passive_entry {
                paired_cash.min(fallback_cash).min(passive_cash)
            } else {
                paired_cash.min(fallback_cash)
            };
            candidate.turnover = Some(TurnoverProjection {
                gross: quantity * (price + reverse.max(maker_exit) + hedge_round_trip),
                fees: quantity
                    * (price * maker_fee.max(Decimal::ZERO)
                        + (reverse * maker_fee.max(Decimal::ZERO))
                            .max(maker_exit * maker.taker_fee_bps)
                        + hedge_round_trip * hedge.taker_fee_bps)
                    / BPS,
                fallback_cash: worst_cash,
                eligible: quantity * eligible,
            });
        }
        Some(edge)
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
        let cash =
            self.ledger.cash - feedback.map_or(Decimal::ZERO, |p| p.overhead(self.ledger.turnover));
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
        let quantity = quote.left;
        let Some(hedge) = self.limit(books[j], j, opposite(side), quantity) else {
            return false;
        };
        if quantity * quote.price > books[i].available_notional.min(self.rules.max_single_leg)
            || !books[i].in_bounds(quote.price)
        {
            return false;
        }
        let maker_cash = cash(side, quote.price * quantity, self.config.maker_fee_bps);
        let hedge_cash = cash(
            opposite(side),
            hedge * quantity,
            self.config.pair.legs[j].taker_fee_bps,
        );
        let mut candidate = Candidate {
            edge_bps: Decimal::ZERO,
            cycle_edge_bps: None,
            passive_entry: false,
            queue_ahead: quote.ahead,
            price: quote.price,
            quantity,
            side,
            hedge_limit: Some(hedge),
            turnover: None,
        };
        if self
            .opening_edge(books, &mut candidate, maker_cash, hedge_cash, None)
            .is_none()
            || candidate.passive_entry != quote.passive_entry
        {
            return false;
        }
        let mode = self.config.pair.mode;
        let overhead = self
            .config
            .turnover_feedback
            .map_or(Decimal::ZERO, |p| p.overhead(self.ledger.turnover));
        let budget = mode.remaining_budget(
            self.ledger.turnover,
            self.ledger.fees,
            self.ledger.cash - overhead,
        );
        self.turnover_rank(&candidate, budget, &mut None, now)
            .is_ok()
    }

    fn quote(&mut self, now: u64) -> Result<()> {
        if self.quote.is_some()
            || self.ioc.is_some()
            || now < self.next_quote_ms
            || self.report.incomplete.is_some()
            || self.funding_paused(now)
        {
            return Ok(());
        }
        let sub_min_opening = self.sub_min_opening_active(now);
        let closing = self.cycle.is_some() && !sub_min_opening;
        let horizon = self.config.entry_horizon_ms();
        if !closing
            && self
                .funding
                .as_ref()
                .and_then(|s| s.next_pause(now))
                .is_some_and(|at| now + horizon >= at)
        {
            self.report.blocked("funding_entry_horizon");
            return Ok(());
        }
        if !closing && now + horizon >= self.config.end_ms {
            return Ok(());
        }
        self.report.entry_checks += usize::from(!closing);
        if !self.feed_online {
            self.report.blocked("trade_feed");
            return Ok(());
        }
        let Some(books) = self.pair_books(now) else {
            self.report.blocked("book_age_or_skew");
            return Ok(());
        };
        let mode = self.config.pair.mode;
        let incentive = mode.incentive();
        let feedback = self.config.turnover_feedback.filter(|_| !closing);
        if !closing
            && incentive.is_some_and(|p| !p.active_at(now) || now + horizon >= p.eligible_until_ms)
        {
            self.report.blocked("turnover_eligibility");
            return Ok(());
        }
        let cash =
            self.ledger.cash - feedback.map_or(Decimal::ZERO, |p| p.overhead(self.ledger.turnover));
        if !closing && mode.realized_loss_exhausted(cash) {
            self.report.blocked("turnover_loss_limit");
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
        let budget = mode.remaining_budget(self.ledger.turnover, self.ledger.fees, cash);
        let mut best: Option<(Decimal, Decimal, Candidate)> = None;
        let mut blocked = None;
        let mut best_edge = self.report.best_modeled_edge_bps;
        let mut best_cost = self.report.best_projected_turnover_cost_bps;
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
            if !closing
                && let Some(policy) = self.config.entry_flow_filter
                && let Err(reason) = self.entry_flow.check_entry(policy, side, now)
            {
                blocked = Some(reason);
                continue;
            }
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
                let Some(candidate) = self.candidate_with_dislocation(
                    books,
                    side,
                    closing,
                    level,
                    mid_dislocation_bps,
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
                    .is_none_or(|(prior_distance, prior_rank, prior)| {
                        (distance, -rank, candidate.queue_ahead)
                            < (*prior_distance, -*prior_rank, prior.queue_ahead)
                    })
                {
                    best = Some((distance, rank, candidate));
                }
                // The nearest passing level preserves price priority.
                break;
            }
        }
        self.report.best_modeled_edge_bps = best_edge;
        self.report.best_projected_turnover_cost_bps = best_cost;
        self.next_quote_ms = now + self.config.reprice_ms;
        let Some((distance, _, best)) = best else {
            self.report
                .blocked(blocked.unwrap_or("size_or_hedge_depth"));
            return Ok(());
        };
        if self.config.require_net_edge
            && incentive.is_none()
            && hedged_exit.is_none()
            && best.edge_bps < self.rules.min_edge
        {
            self.report.blocked(if closing {
                "exit_net_edge"
            } else {
                "entry_net_edge"
            });
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
        let min_edge_floor = policy.min_offset_edge_bps.unwrap_or_else(|| {
            if self.config.require_net_edge {
                self.rules.min_edge
            } else {
                -Decimal::from(100_000)
            }
        });
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

    fn update_paired_state(&mut self, now: u64) {
        let stable = !self.ledger.is_flat()
            && self.net().is_zero()
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
        // Never tolerate extended bad-book waiting when BOTH legs are simultaneously stale.
        if fresh_mids[0].is_none() && fresh_mids[1].is_none() {
            return true;
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
            .filter(|_| self.net().is_zero())
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
            });
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
        let funding_pause = self.funding_paused(now);
        if funding_pause {
            self.cancel(now, "funding_guard");
        }
        self.poll_ioc(now)?;
        let mut confirmed = false;
        while self.notices.front().is_some_and(|(at, _)| *at <= now) {
            self.notices.pop_front();
            confirmed = true;
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
        let (paired_time, paired_basis, paired_book) = self.paired_stop(now);
        if let Some(cycle) = &mut self.cycle {
            let exposure_stop = self
                .unhedged_since
                .is_some_and(|at| now >= at + self.config.unhedged_timeout_ms);
            if !cycle.draining {
                self.report.exposure_stops += usize::from(exposure_stop);
                self.report.cycle_attribution.hedged_time_stops += usize::from(paired_time);
                self.report.cycle_attribution.hedged_basis_stops += usize::from(paired_basis);
                self.report.cycle_attribution.hedged_book_stops += usize::from(paired_book);
            }
            let opening_timeout = (self.config.hedged_exit.is_none()
                || cycle.paired_at_ms.is_none())
                && now >= cycle.started_ms + self.config.hold_ms;
            cycle.draining |= opening_timeout
                || funding_pause
                || paired_time
                || paired_basis
                || paired_book
                || now >= self.config.end_ms - self.config.drain_ms
                || self.report.incomplete.is_some()
                || exposure_stop;
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
                        let close = self.paired_drain_quantity(now, i, quantity)?;
                        if close.is_zero() {
                            self.report.cycle_attribution.paired_drain_blocked += 1;
                            self.next_ioc_ms = now + self.config.reprice_ms;
                        } else {
                            self.submit_ioc(now, i, close, true)?;
                        }
                    }
                } else {
                    let full = self.ledger.positions[i];
                    let below_minimum = self.fresh(i, now).is_some_and(|book| {
                        let terms = &self.config.pair.legs[i];
                        let touch = book.side(direction(quantity))[0].price;
                        floor_step(quantity.abs(), terms.base_step())
                            .is_ok_and(|base| base < min_reduce_ioc_base(terms, touch))
                    });
                    // If the residual cannot be traded, a protected full close
                    // can still clear the leg when its companion remains tradable.
                    let companion = self.ledger.positions[1 - i];
                    let companion_closeable = self.fresh(1 - i, now).is_some_and(|book| {
                        let terms = &self.config.pair.legs[1 - i];
                        let touch = book.side(direction(companion))[0].price;
                        companion.abs() >= min_reduce_ioc_base(terms, touch)
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
        } else if !self.net().is_zero() {
            let awaiting_sub_min_opening = (self.sub_min_opening_active(now)
                && self.quote.as_ref().is_none_or(|quote| !quote.closing))
                || (self.config.aggregate_sub_minimum_fills
                    && self.quote.as_ref().is_some_and(|quote| !quote.closing)
                    && self.net_below_minimum(now));
            if !awaiting_sub_min_opening {
                if let Some(cancel_by) = self.passive_cancel_by(now) {
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
            self.record_completed_cycle(&cycle, now, pnl);
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
        if (self.net().is_zero() || sub_min_opening)
            && closing_ready
            && (self.notices.is_empty()
                || (self.config.passive_offset.is_none() && self.config.hedged_exit.is_none()))
            && self.cycle.as_ref().is_none_or(|cycle| !cycle.draining)
        {
            self.quote(now)?;
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
        if net.is_zero() {
            if let Some(start) = self.unhedged_since.take() {
                let elapsed = now - start;
                self.report.total_unhedged_ms += elapsed;
                self.report.max_unhedged_ms = self.report.max_unhedged_ms.max(elapsed);
            }
        } else {
            if self.unhedged_since.is_none() {
                self.unhedged_since = Some(now);
                self.report.unhedged_episodes += 1;
            }
            if let Some(book) = self
                .fresh(self.maker(), now)
                .or_else(|| self.fresh(1 - self.maker(), now))
            {
                self.report.max_net_notional = self.report.max_net_notional.max(net * book.mid());
            }
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
            previous = now;
            self.funding_timers(now)?;
            if self.posture.expire(now) {
                self.report.turnover_posture.controls_expired += 1;
                self.posture_changed(now);
            }
            let mut book_frame = false;
            match event {
                MakerFirstEvent::Book { books, .. } => {
                    self.set_books(books, now)?;
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
                        && let Some(state) = &mut self.funding
                    {
                        if source_ms < self.config.start_ms {
                            // The replay starts with no orders or inventory. A
                            // reconnect history print cannot fill our later order.
                            state.report.ignored_prestart_prints += 1;
                            self.report.trade_prints += 1;
                            continue;
                        }
                        state.report.max_trade_delivery_ms = state
                            .report
                            .max_trade_delivery_ms
                            .max(now.saturating_sub(source_ms));
                    }
                    if self.funding.is_some()
                        && leg == self.maker()
                        && (source_ms > now
                            || now - source_ms > self.config.trade_delivery_grace_ms)
                    {
                        self.report.late_trade_prints += 1;
                        self.report.trade_prints += 1;
                        self.funding_failure("funding_trade_delivery_bound_violated");
                        self.cancel(now, "unbounded_trade_delivery");
                        continue;
                    }
                    if leg == self.maker() && self.feed_online {
                        self.trade(now, source_ms, aggressor, px, contracts)?;
                    }
                }
                MakerFirstEvent::TradeFeed { leg, online, .. } => {
                    ensure!(leg < 2, "invalid maker-first trade feed leg");
                    if leg == self.maker() {
                        self.feed_online = online;
                        if !online {
                            self.basis_risk.feed_offline();
                            self.basis_reversion.feed_offline();
                            self.entry_flow.feed_offline();
                            if self
                                .quote
                                .as_ref()
                                .is_some_and(|quote| quote.activated_ms.is_some())
                                || (self.funding.is_some() && !self.funding_flat())
                            {
                                self.report.incomplete = Some("resting_order_during_trade_gap");
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
                MakerFirstEvent::End { .. } => {
                    ensure!(
                        now == self.config.end_ms,
                        "incorrect maker-first tape cutoff"
                    );
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
            self.advance(now)?;
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
        let final_mark = if self.funding.is_some() && self.report.incomplete.is_some() {
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
        self.report.funding_flat = self.funding.map(|state| state.report);
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
        books: None,
        usable: [false; 2],
        last_usable_at_ms: [None; 2],
        consumed_ms: [0; 2],
        last_source_ms: [0; 2],
        feed_online: false,
        quote: None,
        ioc: None,
        cycle: None,
        next_quote_ms: 0,
        next_ioc_ms: 0,
        notices: VecDeque::new(),
        unhedged_since: None,
        cost_feedback: CostFeedback::default(),
        basis_risk: BasisRiskState::default(),
        basis_reversion: BasisReversionState::default(),
        entry_flow: EntryFlowState::default(),
        posture: PostureState::new(config.turnover_posture),
        funding: FundingState::new(config)?,
        report: MakerFirstReport {
            conservative_cancel_guard: config.conservative_cancel_guard,
            start_ms: config.start_ms,
            end_ms: config.end_ms,
            execution_costs: config.diagnose_cycles.then(costs::Costs::new),
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
            diagnose_cycles: false,
            maker_fee_bps: Decimal::ONE,
            quote_notional: Decimal::from(100),
            latency_reserve_bps: Decimal::ZERO,
            hedge_protection_bps: None,
            closing_hedge_protection_bps: None,
            basis_risk_reserve: None,
            basis_reversion: None,
            entry_flow_filter: None,
            opening_max_levels: 1,
            aggregate_sub_minimum_fills: false,
            allow_sub_minimum_top_up_unwind: false,
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
        assert_eq!(short.funding_slots_by_leg, [0, 0]);
        assert!(short.required_engine_work.is_empty());
        assert_eq!(short.scope, "configuration_only_no_tape_validated");
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
            let cfg = passive_config(opening);
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
        });
        cfg.basis_reversion = Some(MakerBasisReversion {
            warmup_ms: 5_000,
            half_life_ms: 5_000,
            max_gap_ms: 1_000,
            min_dislocation_bps: d("4.0"),
            cancel_dislocation_bps: d("1.0"),
            reversion_fraction: d("0.8"),
            use_quote_level_dislocation: true,
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
}
