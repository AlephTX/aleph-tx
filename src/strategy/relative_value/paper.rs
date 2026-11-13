//! Sequential IOC replay against later L2 publications, with realized funding.

use super::*;
use crate::hedging::{HedgeAction, HedgeMachine, Leg, OrderIntent, OrderStatus, OrderUpdate};
use std::{borrow::Borrow, collections::BTreeMap};

mod maker_first;
pub use maker_first::{
    MakerFirstConfig, MakerFirstEvent, MakerFirstPreflight, MakerFirstReport, run_maker_first,
};

const MAX_FRAMES: usize = 1_000_000;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaperScenario {
    pub pair: PairConfig,
    pub one_way_latency_ms: u64,
    #[serde(default)]
    pub funding_guard_ms: u64,
    /// Earlier frames update the estimator without placing simulated orders.
    pub trade_from_ms: Option<u64>,
    /// Optional exclusive cutoff for opening new cycles; both bounds are required.
    pub trade_until_ms: Option<u64>,
    pub drain_until_ms: Option<u64>,
    pub frames: Vec<Frame>,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct PaperWindow {
    pub trade_until_ms: u64,
    pub drain_until_ms: u64,
}

impl PaperWindow {
    pub fn from_bounds(
        trade_until_ms: Option<u64>,
        drain_until_ms: Option<u64>,
    ) -> Result<Option<Self>> {
        match (trade_until_ms, drain_until_ms) {
            (None, None) => Ok(None),
            (Some(trade_until_ms), Some(drain_until_ms)) => {
                ensure!(
                    trade_until_ms > 0 && trade_until_ms < drain_until_ms,
                    "invalid paper drain window"
                );
                Ok(Some(Self {
                    trade_until_ms,
                    drain_until_ms,
                }))
            }
            _ => anyhow::bail!("paper trade and drain cutoffs must both be provided"),
        }
    }
}

#[derive(Debug, Default, Serialize)]
pub struct TerminalDrainReport {
    pub trade_until_ms: u64,
    pub drain_until_ms: u64,
    pub first_drain_frame_ms: Option<u64>,
    pub first_frame_at_or_after_drain_until_ms: Option<u64>,
    pub last_execution_frame_ms: Option<u64>,
    pub decision_frames: usize,
    pub drain_frames: usize,
    /// Requires a real boundary publication, no residual position or pending
    /// cycle, and no execution halt. A liquidation mark is not an exit fill.
    pub complete: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitReason {
    Reverted,
    TurnoverComplete,
    BasisStop,
    HoldingTime,
    ReferenceLost,
    SessionChanged,
    FundingDeteriorated,
    FundingUnavailable,
    WindowEnd,
}

#[derive(Debug, Serialize)]
pub struct PaperTrade {
    pub plan: Plan,
    pub opened_at_ms: u64,
    pub closed_at_ms: u64,
    #[serde(with = "rust_decimal::serde::str")]
    pub base_opened: Decimal,
    pub exit_reason: ExitReason,
    #[serde(with = "rust_decimal::serde::str")]
    pub fees: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub funding: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub funding_upper_bound: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub realized_pnl: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub realized_pnl_upper_bound: Decimal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PaperIocGate {
    DeadlineNoFrame,
    DeadlineBookUnavailable,
    DeadlineBookBeforeArrival,
    DeadlineSession,
    DeadlineFundingGuard,
    PriceBand,
    LimitNotMarketable,
    DepthBelowLot,
}

#[derive(Debug, Default, Serialize)]
pub struct PaperLegExecution {
    pub ioc_submitted: usize,
    pub ioc_filled: usize,
    pub ioc_partial: usize,
    pub ioc_unfilled: usize,
    pub unfilled_by_gate: BTreeMap<PaperIocGate, usize>,
    pub fills_without_submit_touch: usize,
    pub max_fill_elapsed_ms: u64,
    #[serde(with = "rust_decimal::serde::str")]
    pub filled_base: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub filled_notional_quote: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub fees_quote: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub submit_touch_notional_quote: Decimal,
    /// Signed cost relative to the same-side touch at submission.
    #[serde(with = "rust_decimal::serde::str")]
    pub slippage_quote: Decimal,
    /// Cost of sweeping levels beyond the arrival book's same-side touch.
    #[serde(with = "rust_decimal::serde::str")]
    pub depth_impact_quote: Decimal,
}

#[derive(Debug, Default, Serialize)]
pub struct PaperMarketCosts {
    pub venue: String,
    pub market: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub turnover_quote: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub fees_quote: Decimal,
    /// Fees plus signed execution slippage against the submit-time touch.
    /// Missing without turnover or when any fill lacked a submit-time touch.
    #[serde(with = "rust_decimal::serde::str_option")]
    pub execution_cost_bps: Option<Decimal>,
}

#[derive(Debug, Serialize)]
pub struct IncentiveTurnoverReport {
    pub venue: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub filled_turnover_quote: Decimal,
    /// Whole-strategy cash cost divided by fills in the reviewed venue window.
    #[serde(with = "rust_decimal::serde::str_option")]
    pub realized_cost_bps: Option<Decimal>,
}

#[derive(Debug, Default, Serialize)]
pub struct PaperEntryDiagnostics {
    pub idle_frames: usize,
    pub blocked_by_gate: BTreeMap<EntryGate, usize>,
    pub blocked_by_leg: [BTreeMap<EntryGate, usize>; 2],
    pub entry_z: f64,
    pub stop_z: f64,
    pub max_abs_z: Option<f64>,
    pub min_net_edge_bps: f64,
    /// Best conservative edge after depth and exposure limits, before turnover caps.
    #[serde(with = "rust_decimal::serde::str_option")]
    pub best_net_edge_bps: Option<Decimal>,
}

#[derive(Debug, Default, Serialize)]
pub struct PaperReport {
    #[serde(skip_serializing_if = "PaperMode::is_pair_hedge")]
    pub mode: PaperMode,
    pub frames: usize,
    pub training_frames: usize,
    pub basis_samples: usize,
    pub max_basis_samples: u32,
    pub holdout_basis_ready_frames: usize,
    pub rejected: BTreeMap<Rejection, usize>,
    pub entry_diagnostics: PaperEntryDiagnostics,
    pub entry_signals: usize,
    pub entries_submitted: usize,
    pub entries_unfilled: usize,
    pub paired_entries: usize,
    pub ioc_submitted: usize,
    pub ioc_filled: usize,
    pub ioc_partial: usize,
    pub ioc_unfilled: usize,
    pub ioc_expired_without_book: usize,
    pub opening_execution: [PaperLegExecution; 2],
    pub closing_execution: [PaperLegExecution; 2],
    pub market_costs: [PaperMarketCosts; 2],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub incentive_turnover: Option<IncentiveTurnoverReport>,
    pub reversion_exits_below_floor: usize,
    pub single_leg_recovery_started: usize,
    pub single_leg_recovery_flat: usize,
    #[serde(with = "rust_decimal::serde::str")]
    pub single_leg_recovery_pnl: Decimal,
    pub funding_guard_ms: u64,
    pub funding_guard_blocked_fills: usize,
    pub funding_guard_skipped_entries: usize,
    pub witnessed_funding_payments: usize,
    pub trades: Vec<PaperTrade>,
    #[serde(with = "rust_decimal::serde::str")]
    pub gross_turnover: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub fees: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub funding: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub funding_upper_bound: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub realized_pnl: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub realized_pnl_upper_bound: Decimal,
    /// Whole replay PnL after an estimated immediate unwind of remaining legs.
    /// Missing if the final book cannot support a liquidation mark.
    #[serde(with = "rust_decimal::serde::str_option")]
    pub final_liquidation_pnl: Option<Decimal>,
    #[serde(serialize_with = "serialize_positions")]
    pub open_positions_base: [Decimal; 2],
    #[serde(with = "rust_decimal::serde::str")]
    pub max_unhedged_base: Decimal,
    pub max_unhedged_ms: u64,
    #[serde(with = "rust_decimal::serde::str")]
    pub max_observed_drawdown: Decimal,
    pub frames_without_liquidation_mark: usize,
    pub halted: Option<String>,
    pub active_plan: Option<Plan>,
    pub open_phase: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_drain: Option<TerminalDrainReport>,
}

impl PaperReport {
    fn reject(&mut self, blocked: BlockedEntry) {
        let gate = blocked.gate;
        *self
            .entry_diagnostics
            .blocked_by_gate
            .entry(gate)
            .or_default() += 1;
        if let Some(i) = blocked.leg {
            *self.entry_diagnostics.blocked_by_leg[i]
                .entry(gate)
                .or_default() += 1;
        }
        if gate != EntryGate::FundingGuard {
            *self.rejected.entry(gate.rejection()).or_default() += 1;
        }
    }

    fn execution(&mut self, opening: bool, leg: usize) -> &mut PaperLegExecution {
        if opening {
            &mut self.opening_execution[leg]
        } else {
            &mut self.closing_execution[leg]
        }
    }
}

fn serialize_positions<S: serde::Serializer>(
    value: &[Decimal; 2],
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    value
        .map(|position| position.to_string())
        .serialize(serializer)
}

#[derive(Debug)]
struct Ledger {
    positions: [Decimal; 2],
    cash: Decimal,
    fees: Decimal,
    funding: Decimal,
    funding_upper: Decimal,
    witnessed_funding_payments: usize,
    turnover: Decimal,
    eligible_turnover: Decimal,
    cycle_eligible_turnover: Decimal,
}

impl Default for Ledger {
    fn default() -> Self {
        Self {
            positions: [Decimal::ZERO; 2],
            cash: Decimal::ZERO,
            fees: Decimal::ZERO,
            funding: Decimal::ZERO,
            funding_upper: Decimal::ZERO,
            witnessed_funding_payments: 0,
            turnover: Decimal::ZERO,
            eligible_turnover: Decimal::ZERO,
            cycle_eligible_turnover: Decimal::ZERO,
        }
    }
}

impl Ledger {
    fn trade(&mut self, i: usize, side: HedgeSide, fill: Fill, fee_bps: Decimal) -> Decimal {
        let sign = if side == HedgeSide::Buy {
            Decimal::ONE
        } else {
            -Decimal::ONE
        };
        let fee = fill.notional * fee_bps / BPS;
        self.positions[i] += sign * fill.base;
        self.cash -= sign * fill.notional + fee;
        self.fees += fee;
        self.turnover += fill.notional;
        fee
    }

    fn is_flat(&self) -> bool {
        self.positions == [Decimal::ZERO; 2]
    }

    fn book_funding(
        &mut self,
        frame: &Frame,
        prev: u64,
        terms: &[MarketTerms; 2],
        guard_ms: u64,
    ) -> Result<()> {
        for (i, leg) in frame.legs.iter().enumerate() {
            let schedule = terms[i].funding;
            let count = leg.settled.len() + leg.witnessed_settlements.len();
            ensure!(
                count <= MAX_FUNDING_POINTS
                    && leg.settled.windows(2).all(|w| w[0].at_ms < w[1].at_ms)
                    && leg
                        .witnessed_settlements
                        .windows(2)
                        .all(|w| w[0].at_ms < w[1].at_ms),
                "invalid funding event count or order"
            );
            let mut events: Vec<_> = leg
                .settled
                .iter()
                .map(FundingEvent::Price)
                .chain(leg.witnessed_settlements.iter().map(FundingEvent::Witness))
                .collect();
            events.sort_unstable_by_key(FundingEvent::at_ms);
            let mut last = prev;
            for event in &events {
                ensure!(
                    event.at_ms() > last
                        && event.at_ms() <= frame.now_ms
                        && event.valid(schedule, guard_ms),
                    "invalid realized funding event for {}",
                    terms[i].venue
                );
                last = event.at_ms();
            }
            if self.positions[i] == Decimal::ZERO {
                continue;
            }
            let (_, expected) = schedule
                .window(prev, frame.now_ms)
                .context("funding schedule overflow")?;
            ensure!(
                count as u64 == expected,
                "missing realized funding on {} between {prev} and {}",
                terms[i].venue,
                frame.now_ms
            );
            for event in events {
                // Events at frame time settle before any simulated fills at that time.
                let (low, high) = match event {
                    FundingEvent::Price(event) => {
                        let cash = -self.positions[i] * event.oracle_price * event.rate;
                        (cash, cash)
                    }
                    FundingEvent::Witness(event) => {
                        self.witnessed_funding_payments += 1;
                        event.cash_bounds(self.positions[i])
                    }
                };
                self.cash += low;
                self.funding += low;
                self.funding_upper += high;
            }
        }
        Ok(())
    }
}

enum FundingEvent<'a> {
    Price(&'a FundingSettlement),
    Witness(&'a WitnessedSettlement),
}

impl FundingEvent<'_> {
    fn at_ms(&self) -> u64 {
        match self {
            Self::Price(event) => event.at_ms,
            Self::Witness(event) => event.at_ms,
        }
    }

    fn valid(&self, schedule: FundingSchedule, guard_ms: u64) -> bool {
        match self {
            Self::Price(event) => event.valid(schedule),
            Self::Witness(event) => event.valid(schedule, guard_ms),
        }
    }
}

struct Pending {
    leg_index: usize,
    order: OrderIntent,
    submitted_at_ms: u64,
    submit_touch: Option<Decimal>,
    expiry_gate: PaperIocGate,
}

struct Execution {
    machine: HedgeMachine,
    pending: Option<Pending>,
    deadline: u64,
}

impl Execution {
    fn new(plan: &HedgePlan) -> Result<Self> {
        Ok(Self {
            machine: HedgeMachine::new(plan.clone())?,
            pending: None,
            deadline: plan.deadline_ms,
        })
    }
}

#[derive(Default)]
struct Recovery {
    pending: Option<Pending>,
    attempts: u32,
}

enum Phase {
    Opening(Execution),
    Holding,
    Closing(Execution),
    Recovering(Recovery),
    Halted,
}

impl Phase {
    fn label(&self) -> &'static str {
        match self {
            Self::Opening(_) => "opening",
            Self::Holding => "holding",
            Self::Closing(_) => "closing",
            Self::Recovering(_) => "recovering",
            Self::Halted => "halted",
        }
    }
}

struct Active {
    plan: Plan,
    phase: Phase,
    baseline_cash: Decimal,
    baseline_fees: Decimal,
    baseline_funding: Decimal,
    baseline_funding_upper: Decimal,
    session_open: Option<u64>,
    opened_at: u64,
    base_opened: Decimal,
    exit_reason: Option<ExitReason>,
}

#[derive(Default)]
struct Fill {
    base: Decimal,
    notional: Decimal,
}

/// Volume at the recorded prices, limited by the protected IOC price and by
/// our share of each displayed level. Round the filled size to the venue lot
/// before pricing it, so it can be passed to the hedge state machine.
fn sweep(
    book: &Book,
    terms: &MarketTerms,
    side: HedgeSide,
    limit: Decimal,
    requested: Decimal,
    participation: Decimal,
) -> Result<Fill> {
    let eligible = || {
        book.side(side).iter().take_while(|l| {
            book.in_bounds(l.price)
                && match side {
                    HedgeSide::Buy => l.price <= limit,
                    HedgeSide::Sell => l.price >= limit,
                }
        })
    };
    let capacity = |l: &Level| l.contracts * terms.base_per_contract * participation;
    let available: Decimal = eligible().map(capacity).sum();
    let base = floor_step(available.min(requested), terms.base_step())?;
    let mut remaining = base;
    let mut notional = Decimal::ZERO;
    for level in eligible() {
        let taken = remaining.min(capacity(level));
        notional += taken * level.price;
        remaining -= taken;
        if remaining == Decimal::ZERO {
            break;
        }
    }
    Ok(Fill { base, notional })
}

struct Runner {
    planner: RelativeValue,
    latency: u64,
    funding_guard_ms: u64,
    trade_from_ms: Option<u64>,
    window: Option<PaperWindow>,
    active: Option<Active>,
    ledger: Ledger,
    report: PaperReport,
    peak_pnl: Decimal,
    unhedged_since: Option<u64>,
    consumed: [Option<(u64, HedgeSide)>; 2],
}

enum Advance {
    Working,
    Complete,
    Halt(String),
}

impl Runner {
    fn draining(&self, now: u64) -> bool {
        self.window
            .is_some_and(|window| now >= window.trade_until_ms)
    }

    fn poll_ioc(
        &mut self,
        pending: &mut Pending,
        frame: &Frame,
        deadline: u64,
    ) -> Result<Option<(Fill, Option<PaperIocGate>)>> {
        let now = frame.now_ms;
        if now >= deadline {
            // A simulated intent expires before it reaches an eligible book.
            self.report.ioc_expired_without_book += 1;
            return Ok(Some((Fill::default(), Some(pending.expiry_gate))));
        }
        let i = pending.leg_index;
        let book = &frame.legs[i].book;
        let waiting = if self.planner.session_end(frame).is_err() {
            Some(PaperIocGate::DeadlineSession)
        } else if !self.planner.usable_book(frame, i) {
            Some(PaperIocGate::DeadlineBookUnavailable)
        } else if book.timestamp_ms < pending.submitted_at_ms.saturating_add(self.latency) {
            Some(PaperIocGate::DeadlineBookBeforeArrival)
        } else if self
            .planner
            .config
            .legs
            .iter()
            .any(|leg| leg.funding.near_payment(now, self.funding_guard_ms))
        {
            self.report.funding_guard_blocked_fills += 1;
            Some(PaperIocGate::DeadlineFundingGuard)
        } else {
            None
        };
        if let Some(gate) = waiting {
            pending.expiry_gate = gate;
            return Ok(None);
        }
        let order = &pending.order;
        let touch = book.side(order.side)[0].price;
        if !book.in_bounds(order.limit_price) || !book.in_bounds(touch) {
            return Ok(Some((Fill::default(), Some(PaperIocGate::PriceBand))));
        }
        let marketable = match order.side {
            HedgeSide::Buy => touch <= order.limit_price,
            HedgeSide::Sell => touch >= order.limit_price,
        };
        if !marketable {
            return Ok(Some((
                Fill::default(),
                Some(PaperIocGate::LimitNotMarketable),
            )));
        }
        let fill = sweep(
            book,
            &self.planner.config.legs[i],
            order.side,
            order.limit_price,
            order.base_quantity,
            self.planner.rules.participation,
        )?;
        let gate = fill.base.is_zero().then_some(PaperIocGate::DepthBelowLot);
        Ok(Some((fill, gate)))
    }

    fn record_ioc(
        &mut self,
        pending: &Pending,
        fill: Fill,
        unfilled_gate: Option<PaperIocGate>,
        frame: &Frame,
        opening: bool,
    ) {
        let i = pending.leg_index;
        let book = &frame.legs[i].book;
        let order = &pending.order;
        let stats = self.report.execution(opening, i);
        if fill.base == order.base_quantity {
            stats.ioc_filled += 1;
        } else if !fill.base.is_zero() {
            stats.ioc_partial += 1;
        } else {
            stats.ioc_unfilled += 1;
            if let Some(gate) = unfilled_gate {
                *stats.unfilled_by_gate.entry(gate).or_default() += 1;
            }
        }
        if fill.base > Decimal::ZERO {
            if self
                .planner
                .config
                .mode
                .incentive()
                .is_some_and(|policy| policy.eligible_leg == i && policy.active_at(frame.now_ms))
            {
                self.ledger.cycle_eligible_turnover += fill.notional;
            }
            self.consumed[i] = Some((book.timestamp_ms, order.side));
            let side_sign = if order.side == HedgeSide::Buy {
                Decimal::ONE
            } else {
                -Decimal::ONE
            };
            stats.filled_base += fill.base;
            stats.filled_notional_quote += fill.notional;
            stats.max_fill_elapsed_ms = stats
                .max_fill_elapsed_ms
                .max(frame.now_ms - pending.submitted_at_ms);
            let arrival_touch = book.side(order.side)[0].price;
            stats.depth_impact_quote += side_sign * (fill.notional - fill.base * arrival_touch);
            if let Some(touch) = pending.submit_touch {
                let submit_notional = fill.base * touch;
                stats.submit_touch_notional_quote += submit_notional;
                stats.slippage_quote += side_sign * (fill.notional - submit_notional);
            } else {
                stats.fills_without_submit_touch += 1;
            }
        }
        stats.fees_quote += self.ledger.trade(
            i,
            order.side,
            fill,
            self.planner.config.legs[i].taker_fee_bps,
        );
    }

    fn advance(
        &mut self,
        execution: &mut Execution,
        frame: &Frame,
        opening: bool,
    ) -> Result<Advance> {
        let now = frame.now_ms;
        if let Some(mut pending) = execution.pending.take() {
            let Some((fill, gate)) = self.poll_ioc(&mut pending, frame, execution.deadline)? else {
                execution.pending = Some(pending);
                return Ok(Advance::Working);
            };
            let order = &pending.order;
            execution.machine.apply_update(OrderUpdate {
                client_order_id: order.client_order_id.clone(),
                venue: order.venue.clone(),
                exchange_order_id: None,
                cumulative_filled_base: fill.base,
                status: if fill.base == order.base_quantity {
                    OrderStatus::Filled
                } else {
                    OrderStatus::Canceled
                },
            })?;
            self.record_ioc(&pending, fill, gate, frame, opening);
        }
        if opening
            && self.draining(now)
            && self.ledger.positions[0] + self.ledger.positions[1] == Decimal::ZERO
        {
            // Pending intents submitted before the cutoff retain their real
            // outcome. Once balanced, do not retry to increase paired size.
            return Ok(Advance::Halt("decision window ended".into()));
        }
        if opening
            && now < execution.deadline
            && self.ledger.positions[0] + self.ledger.positions[1] == Decimal::ZERO
            && self.ledger.positions[0].abs() < execution.machine.target_base()
            && (!self.planner.usable_books(frame)
                || self.planner.cutoff(frame).is_err()
                || self.planner.basis_bps(frame).is_err())
        {
            // Stop increasing paired size after data invalidation. An unmatched
            // fill can still request the preapproved opposite IOC.
            return Ok(Advance::Halt("market inputs invalidated".into()));
        }
        match execution.machine.next_action(now)? {
            HedgeAction::Submit { order } => {
                let i = usize::from(order.venue == self.planner.config.legs[1].venue);
                if opening && self.draining(now) {
                    let net = self.ledger.positions[0] + self.ledger.positions[1];
                    let change = if order.side == HedgeSide::Buy {
                        order.base_quantity
                    } else {
                        -order.base_quantity
                    };
                    if (net + change).abs() >= net.abs() {
                        return Ok(Advance::Halt("drain would increase net exposure".into()));
                    }
                }
                self.report.execution(opening, i).ioc_submitted += 1;
                let submit_touch = self
                    .planner
                    .usable_book(frame, i)
                    .then(|| frame.legs[i].book.side(order.side)[0].price);
                execution.pending = Some(Pending {
                    leg_index: i,
                    order,
                    submitted_at_ms: now,
                    submit_touch,
                    expiry_gate: PaperIocGate::DeadlineNoFrame,
                });
                Ok(Advance::Working)
            }
            HedgeAction::Complete => Ok(Advance::Complete),
            HedgeAction::Halt { reason, .. } => Ok(Advance::Halt(format!("{reason:?}"))),
            // A paper intent is either pending here or has a terminal outcome.
            _ => anyhow::bail!("inconsistent paper hedge state"),
        }
    }

    fn exit_reason(&self, active: &Active, frame: &Frame) -> Option<ExitReason> {
        let now = frame.now_ms;
        if self.draining(now) {
            return Some(ExitReason::WindowEnd);
        }
        if self.planner.cutoff(frame).is_err()
            || frame.equity_session.map(|s| s.opens_at_ms) != active.session_open
            || self.planner.cutoff(frame).is_ok_and(|cutoff| {
                now.saturating_add(self.planner.config.execution_window_ms) >= cutoff
            })
        {
            return Some(ExitReason::SessionChanged);
        }
        if now >= active.plan.hold_until_ms {
            return Some(ExitReason::HoldingTime);
        }
        if !self.planner.usable_books(frame) {
            return None;
        }
        let Ok(basis) = self.planner.basis_bps(frame) else {
            return Some(ExitReason::ReferenceLost);
        };
        let initial = active.plan.basis;
        let direction = if active.plan.buy_leg == 0 { 1.0 } else { -1.0 };
        let z = direction * (basis - initial.mean_bps) / initial.sigma_bps;
        if z >= self.planner.config.basis.stop_z {
            return Some(ExitReason::BasisStop);
        }
        if self.planner.config.mode.incentive().is_some() {
            return Some(ExitReason::TurnoverComplete);
        }
        if active.plan.strategy == Strategy::BasisReversion && z <= self.planner.config.basis.exit_z
        {
            return Some(ExitReason::Reverted);
        }
        if active.plan.strategy == Strategy::FundingCarry {
            let window = [now, active.plan.hold_until_ms, active.plan.unwind_until_ms];
            match self
                .planner
                .funding_bound(frame, active.plan.buy_leg, window)
            {
                Ok(bound) if bound < Decimal::ZERO => return Some(ExitReason::FundingDeteriorated),
                Err(_) => return Some(ExitReason::FundingUnavailable),
                Ok(_) => {}
            }
        }
        None
    }

    fn exit_execution(
        &mut self,
        active: &Active,
        frame: &Frame,
        reason: ExitReason,
    ) -> Result<Option<Execution>> {
        let Ok(cutoff) = self.planner.session_end(frame) else {
            return Ok(None);
        };
        if !self.planner.usable_books(frame) {
            return Ok(None);
        }
        let now = frame.now_ms;
        let deadline = now
            .saturating_add(self.planner.config.execution_window_ms)
            .min(active.plan.unwind_until_ms)
            .min(cutoff)
            .min(self.window.map_or(u64::MAX, |window| window.drain_until_ms));
        if deadline <= now.saturating_add(self.latency.saturating_mul(2)) {
            return Ok(None);
        }
        let sides = sides(1 - active.plan.buy_leg);
        let curves = [0, 1].map(|i| {
            [HedgeSide::Buy, HedgeSide::Sell].map(|side| {
                Depth::build(
                    &frame.legs[i].book,
                    &self.planner.config.legs[i],
                    side,
                    self.planner.rules,
                )
            })
        });
        let base = self.ledger.positions[0].abs();
        let profit_floor = (reason == ExitReason::Reverted)
            .then_some(self.planner.config.basis.min_exit_profit_bps)
            .flatten();
        let limit = |i: usize| {
            let curve = &curves[i][usize::from(sides[i] == HedgeSide::Sell)];
            let limit = curve.limit(base);
            if profit_floor.is_some() {
                limit
            } else {
                limit.or_else(|_| curve.last().map(|b| b.limit))
            }
        };
        let (Ok(a), Ok(b)) = (limit(0), limit(1)) else {
            return Ok(None);
        };
        if (0..2).any(|i| base < self.planner.config.legs[i].min_base([a, b][i])) {
            return Ok(None);
        }
        if let Some(floor) = profit_floor {
            let close_cash = [a, b]
                .into_iter()
                .enumerate()
                .map(|(i, limit)| {
                    let sign = if sides[i] == HedgeSide::Sell {
                        Decimal::ONE
                    } else {
                        -Decimal::ONE
                    };
                    limit * base * (sign - self.planner.config.legs[i].taker_fee_bps / BPS)
                })
                .sum::<Decimal>();
            let profit = self.ledger.cash + close_cash - active.baseline_cash;
            if profit < floor / BPS * base * frame.reference.price {
                self.report.reversion_exits_below_floor += 1;
                return Ok(None);
            }
        }
        let plan = self.planner.hedge_plan(
            sides,
            [a, b],
            base,
            deadline,
            format!("rv-exit-{now}"),
            &curves,
        )?;
        Ok(Some(Execution::new(&plan)?))
    }

    fn failed_execution(&mut self, execution: &Execution, reason: String) -> Phase {
        self.report.halted = Some(reason);
        if self.planner.config.recover_single_leg
            && self
                .ledger
                .positions
                .iter()
                .filter(|p| !p.is_zero())
                .count()
                == 1
            && execution.pending.is_none()
            && execution
                .machine
                .snapshot()
                .orders
                .iter()
                .all(|order| order.terminal && !order.uncertain)
        {
            self.report.single_leg_recovery_started += 1;
            Phase::Recovering(Recovery::default())
        } else {
            Phase::Halted
        }
    }

    fn advance_recovery(
        &mut self,
        recovery: &mut Recovery,
        plan: &Plan,
        frame: &Frame,
    ) -> Result<Advance> {
        let deadline = plan
            .unwind_until_ms
            .min(self.window.map_or(u64::MAX, |window| window.drain_until_ms));
        if let Some(mut pending) = recovery.pending.take() {
            let Some((fill, gate)) = self.poll_ioc(&mut pending, frame, deadline)? else {
                recovery.pending = Some(pending);
                return Ok(Advance::Working);
            };
            let position = self.ledger.positions[pending.leg_index];
            ensure!(
                !position.is_zero()
                    && (position > Decimal::ZERO) == (pending.order.side == HedgeSide::Sell)
                    && fill.base <= position.abs(),
                "paper recovery fill would increase exposure"
            );
            self.record_ioc(&pending, fill, gate, frame, false);
        }
        if self.ledger.is_flat() {
            return Ok(Advance::Complete);
        }
        let now = frame.now_ms;
        if now.saturating_add(self.latency) >= deadline
            || recovery.attempts >= plan.hedge.max_order_attempts
        {
            return Ok(Advance::Halt("single-leg recovery exhausted".into()));
        }
        let i = usize::from(self.ledger.positions[0].is_zero());
        if self.planner.session_end(frame).is_err() || !self.planner.usable_book(frame, i) {
            return Ok(Advance::Working);
        }
        let position = self.ledger.positions[i];
        let side = if position > Decimal::ZERO {
            HedgeSide::Sell
        } else {
            HedgeSide::Buy
        };
        let terms = &self.planner.config.legs[i];
        let book = &frame.legs[i].book;
        let depth = Depth::build(book, terms, side, self.planner.rules);
        let Ok(last) = depth.last() else {
            return Ok(Advance::Working);
        };
        let base = floor_step(position.abs().min(last.base), terms.base_step())?;
        let Ok(limit) = depth.limit(base) else {
            return Ok(Advance::Working);
        };
        if base < terms.min_base(limit).max(terms.base_step()) {
            return Ok(Advance::Working);
        }
        let order = OrderIntent {
            client_order_id: format!("{}-reduce-{}", plan.hedge.execution_id, recovery.attempts),
            venue: terms.venue.clone(),
            instrument: terms.underlying_id.clone(),
            leg: if terms.venue == plan.hedge.primary.venue {
                Leg::Primary
            } else {
                Leg::Hedge
            },
            side,
            base_quantity: base,
            limit_price: limit,
            immediate_or_cancel: true,
        };
        recovery.attempts += 1;
        self.report.execution(false, i).ioc_submitted += 1;
        recovery.pending = Some(Pending {
            leg_index: i,
            order,
            submitted_at_ms: now,
            submit_touch: Some(book.side(side)[0].price),
            expiry_gate: PaperIocGate::DeadlineNoFrame,
        });
        Ok(Advance::Working)
    }

    /// Returns true once the active cycle can be dropped.
    fn advance_active(&mut self, active: &mut Active, frame: &Frame) -> Result<bool> {
        match &mut active.phase {
            Phase::Opening(exec) => match self.advance(exec, frame, true)? {
                Advance::Working => {}
                Advance::Complete => {
                    self.report.paired_entries += 1;
                    active.base_opened = self.ledger.positions[0].abs();
                    active.opened_at = frame.now_ms;
                    active.phase = Phase::Holding;
                }
                Advance::Halt(_) if self.ledger.is_flat() => {
                    self.report.entries_unfilled += 1;
                    return Ok(true);
                }
                Advance::Halt(reason) => {
                    // Partial equal fills can still be managed as a smaller
                    // position, provided they share a valid close lot.
                    let a = self.ledger.positions[0];
                    if a != Decimal::ZERO
                        && a + self.ledger.positions[1] == Decimal::ZERO
                        && a.abs() % self.planner.step == Decimal::ZERO
                    {
                        self.report.paired_entries += 1;
                        active.base_opened = a.abs();
                        active.opened_at = frame.now_ms;
                        active.phase = Phase::Holding;
                    } else {
                        active.phase = self.failed_execution(exec, format!("entry {reason}"));
                    }
                }
            },
            Phase::Holding => {
                let reason = active
                    .exit_reason
                    .or_else(|| self.exit_reason(active, frame));
                if let Some(reason) = reason {
                    if reason != ExitReason::Reverted
                        || self.planner.config.basis.min_exit_profit_bps.is_none()
                    {
                        active.exit_reason = Some(reason);
                    }
                    if frame.now_ms >= active.plan.unwind_until_ms {
                        self.report.halted = Some("exit window expired".into());
                        active.phase = Phase::Halted;
                    } else if let Some(mut exec) = self.exit_execution(active, frame, reason)? {
                        active.exit_reason = Some(reason);
                        self.advance(&mut exec, frame, false)?;
                        active.phase = Phase::Closing(exec);
                    }
                }
            }
            Phase::Closing(exec) => match self.advance(exec, frame, false)? {
                Advance::Working => {}
                Advance::Complete => {
                    ensure!(self.ledger.is_flat(), "completed exit left exposure");
                    let pnl = self.ledger.cash - active.baseline_cash;
                    let funding = self.ledger.funding - active.baseline_funding;
                    let funding_upper = self.ledger.funding_upper - active.baseline_funding_upper;
                    let pnl_upper = pnl + (funding_upper - funding);
                    self.ledger.eligible_turnover +=
                        std::mem::take(&mut self.ledger.cycle_eligible_turnover);
                    self.report.realized_pnl += pnl;
                    self.report.realized_pnl_upper_bound += pnl_upper;
                    self.report.trades.push(PaperTrade {
                        plan: active.plan.clone(),
                        opened_at_ms: active.opened_at,
                        closed_at_ms: frame.now_ms,
                        base_opened: active.base_opened,
                        exit_reason: active.exit_reason.context("missing exit trigger")?,
                        fees: self.ledger.fees - active.baseline_fees,
                        funding,
                        funding_upper_bound: funding_upper,
                        realized_pnl: pnl,
                        realized_pnl_upper_bound: pnl_upper,
                    });
                    return Ok(true);
                }
                Advance::Halt(reason) => {
                    active.phase = self.failed_execution(exec, format!("exit {reason}"));
                }
            },
            Phase::Recovering(_) | Phase::Halted => {}
        }
        if let Phase::Recovering(recovery) = &mut active.phase {
            match self.advance_recovery(recovery, &active.plan, frame)? {
                Advance::Working => {}
                Advance::Complete => {
                    let pnl = self.ledger.cash - active.baseline_cash;
                    let pnl_upper = pnl
                        + (self.ledger.funding_upper - active.baseline_funding_upper)
                        - (self.ledger.funding - active.baseline_funding);
                    self.report.single_leg_recovery_flat += 1;
                    self.report.single_leg_recovery_pnl += pnl;
                    self.report.realized_pnl += pnl;
                    self.report.realized_pnl_upper_bound += pnl_upper;
                    self.ledger.cycle_eligible_turnover = Decimal::ZERO;
                    return Ok(true);
                }
                Advance::Halt(_) => active.phase = Phase::Halted,
            }
        }
        Ok(false)
    }

    fn start(&mut self, plan: Plan, frame: &Frame) -> Result<()> {
        if plan.hedge.deadline_ms <= frame.now_ms.saturating_add(self.latency.saturating_mul(2)) {
            self.report.reject(EntryGate::LatencyWindow.into());
            return Ok(());
        }
        if self.funding_guard_ms > 0
            && self.planner.config.legs.iter().any(|leg| {
                leg.funding
                    .near_payment(frame.now_ms, self.funding_guard_ms)
                    || leg
                        .funding
                        .window(
                            frame.now_ms,
                            plan.hedge.deadline_ms.saturating_add(self.funding_guard_ms),
                        )
                        .is_none_or(|(_, count)| count > 0)
            })
        {
            self.report.funding_guard_skipped_entries += 1;
            self.report.reject(EntryGate::FundingGuard.into());
            return Ok(());
        }
        if self
            .planner
            .config
            .mode
            .remaining_budget(self.ledger.turnover, self.ledger.fees, self.ledger.cash)
            .is_some_and(|budget| {
                plan.modeled_turnover_quote > budget.turnover
                    || plan.modeled_fees_quote > budget.fees
                    || budget.loss.is_some_and(|loss| -plan.net_edge > loss)
            })
        {
            self.report.reject(EntryGate::TurnoverBudget.into());
            return Ok(());
        }
        let mut exec = Execution::new(&plan.hedge)?;
        self.ledger.cycle_eligible_turnover = Decimal::ZERO;
        self.advance(&mut exec, frame, true)?;
        self.report.entries_submitted += 1;
        self.active = Some(Active {
            plan,
            phase: Phase::Opening(exec),
            baseline_cash: self.ledger.cash,
            baseline_fees: self.ledger.fees,
            baseline_funding: self.ledger.funding,
            baseline_funding_upper: self.ledger.funding_upper,
            session_open: frame.equity_session.map(|s| s.opens_at_ms),
            opened_at: 0,
            base_opened: Decimal::ZERO,
            exit_reason: None,
        });
        Ok(())
    }

    fn liquidation_mark(&self, frame: &Frame) -> Result<Option<Decimal>> {
        if self.ledger.is_flat() {
            return Ok(Some(self.ledger.cash));
        }
        if self.planner.session_end(frame).is_err()
            || (self
                .ledger
                .positions
                .iter()
                .all(|qty| *qty != Decimal::ZERO)
                && !self.planner.usable_books(frame))
        {
            return Ok(None);
        }
        let mut mark = self.ledger.cash;
        for (i, pos) in self.ledger.positions.into_iter().enumerate() {
            if pos == Decimal::ZERO {
                continue;
            }
            let side = if pos > Decimal::ZERO {
                HedgeSide::Sell
            } else {
                HedgeSide::Buy
            };
            let book = &frame.legs[i].book;
            if !self.planner.usable_book(frame, i)
                || self.consumed[i]
                    .is_some_and(|(at, filled_side)| side == filled_side && at >= book.timestamp_ms)
            {
                return Ok(None);
            }
            let terms = &self.planner.config.legs[i];
            let curve = Depth::build(book, terms, side, self.planner.rules);
            let Ok(limit) = curve.limit(pos.abs()) else {
                return Ok(None);
            };
            let fill = sweep(
                book,
                terms,
                side,
                limit,
                pos.abs(),
                self.planner.rules.participation,
            )?;
            if fill.base != pos.abs() {
                return Ok(None);
            }
            let fee = fill.notional * terms.taker_fee_bps / BPS;
            mark += if pos > Decimal::ZERO {
                fill.notional - fee
            } else {
                -fill.notional - fee
            };
        }
        Ok(Some(mark))
    }

    fn record_risk(&mut self, frame: &Frame) -> Result<()> {
        let exposure = (self.ledger.positions[0] + self.ledger.positions[1]).abs();
        self.report.max_unhedged_base = self.report.max_unhedged_base.max(exposure);
        if let Some(since) = self.unhedged_since {
            self.report.max_unhedged_ms = self.report.max_unhedged_ms.max(frame.now_ms - since);
        }
        if exposure == Decimal::ZERO {
            self.unhedged_since = None;
        } else {
            self.unhedged_since.get_or_insert(frame.now_ms);
        }
        let mark = self.liquidation_mark(frame)?;
        if let Some(mark) = mark {
            self.peak_pnl = self.peak_pnl.max(mark);
            self.report.max_observed_drawdown =
                self.report.max_observed_drawdown.max(self.peak_pnl - mark);
        } else {
            self.report.frames_without_liquidation_mark += 1;
        }
        self.report.final_liquidation_pnl = mark;
        Ok(())
    }

    fn run<T: Borrow<Frame>>(
        mut self,
        frames: impl IntoIterator<Item = Result<T>>,
    ) -> Result<PaperReport> {
        let mut previous = 0;
        for frame in frames {
            let frame = frame?;
            let frame = frame.borrow();
            ensure!(
                frame.now_ms > previous,
                "paper frame timestamps must increase"
            );
            self.report.frames += 1;
            ensure!(self.report.frames <= MAX_FRAMES, "too many paper frames");
            if let Some(drain) = &mut self.report.terminal_drain {
                if frame.now_ms >= drain.trade_until_ms {
                    drain.first_drain_frame_ms.get_or_insert(frame.now_ms);
                }
                if frame.now_ms >= drain.drain_until_ms {
                    drain.first_frame_at_or_after_drain_until_ms = Some(frame.now_ms);
                    // The boundary publication attests to elapsed time. Never
                    // execute an IOC after the configured drain deadline.
                    break;
                }
                drain.last_execution_frame_ms = Some(frame.now_ms);
                if frame.now_ms >= drain.trade_until_ms {
                    drain.drain_frames += 1;
                } else if self.trade_from_ms.is_none_or(|start| frame.now_ms >= start) {
                    drain.decision_frames += 1;
                }
            }
            let sample = self.planner.sample(frame)?;
            if sample.is_ok() || sample.is_err_and(|blocked| blocked.gate == EntryGate::Warmup) {
                self.report.basis_samples += 1;
                self.report.max_basis_samples = self
                    .report
                    .max_basis_samples
                    .max(self.planner.basis.samples);
            }
            self.ledger.book_funding(
                frame,
                previous,
                &self.planner.config.legs,
                self.funding_guard_ms,
            )?;
            previous = frame.now_ms;
            if self.trade_from_ms.is_some_and(|start| frame.now_ms < start) {
                self.report.training_frames += 1;
                continue;
            }
            self.report.holdout_basis_ready_frames += usize::from(sample.is_ok());
            if let Some(mut active) = self.active.take() {
                if !self.advance_active(&mut active, frame)? {
                    self.active = Some(active);
                }
            } else if self.report.halted.is_none() && !self.draining(frame.now_ms) {
                let diagnostics = &mut self.report.entry_diagnostics;
                diagnostics.idle_frames += 1;
                let plan = sample.and_then(|(signal, cutoff)| {
                    let mode = self.planner.config.mode;
                    if self.ledger.is_flat() {
                        if mode.realized_loss_exhausted(self.ledger.cash) {
                            return Err(EntryGate::TurnoverLossLimit.into());
                        }
                        if mode.incentive().is_some_and(|policy| {
                            !policy.within_cost(self.ledger.cash, self.ledger.eligible_turnover)
                        }) {
                            return Err(EntryGate::TurnoverCost.into());
                        }
                    }
                    let abs_z = signal.z.abs();
                    diagnostics.max_abs_z =
                        Some(diagnostics.max_abs_z.map_or(abs_z, |max| max.max(abs_z)));
                    self.planner.plan(
                        frame,
                        signal,
                        cutoff,
                        self.funding_guard_ms,
                        EntryOptions {
                            turnover_budget: mode.remaining_budget(
                                self.ledger.turnover,
                                self.ledger.fees,
                                self.ledger.cash,
                            ),
                            best_edge_bps: Some(&mut diagnostics.best_net_edge_bps),
                        },
                    )
                });
                match plan {
                    Ok(plan) => {
                        self.report.entry_signals += 1;
                        self.start(plan, frame)?;
                    }
                    Err(blocked) => self.report.reject(blocked),
                }
            }
            self.record_risk(frame)?;
        }
        ensure!(
            self.report.frames > self.report.training_frames,
            "no paper evaluation frames"
        );
        if let Some(drain) = &mut self.report.terminal_drain {
            ensure!(
                drain.decision_frames > 0,
                "no paper decision frames before cutoff"
            );
            drain.complete = drain.first_frame_at_or_after_drain_until_ms.is_some()
                && self.ledger.is_flat()
                && self.active.is_none()
                && self.report.halted.is_none();
            if !drain.complete && self.report.halted.is_none() {
                self.report.halted = Some(
                    if drain.first_frame_at_or_after_drain_until_ms.is_none() {
                        "terminal drain missing boundary publication"
                    } else {
                        "terminal drain left exposure or pending execution"
                    }
                    .into(),
                );
            }
        }
        self.report.open_positions_base = self.ledger.positions;
        self.report.gross_turnover = self.ledger.turnover;
        self.report.fees = self.ledger.fees;
        self.report.funding = self.ledger.funding;
        self.report.funding_upper_bound = self.ledger.funding_upper;
        self.report.witnessed_funding_payments = self.ledger.witnessed_funding_payments;
        if let Some(policy) = self.planner.config.mode.incentive() {
            let turnover = self.ledger.eligible_turnover;
            self.report.incentive_turnover = Some(IncentiveTurnoverReport {
                venue: self.planner.config.legs[policy.eligible_leg].venue.clone(),
                filled_turnover_quote: turnover,
                realized_cost_bps: (turnover > Decimal::ZERO
                    && self.ledger.is_flat()
                    && self.active.is_none())
                .then(|| -self.ledger.cash / turnover * BPS),
            });
        }
        self.report.market_costs = std::array::from_fn(|i| {
            let terms = &self.planner.config.legs[i];
            let entry = &self.report.opening_execution[i];
            let exit = &self.report.closing_execution[i];
            let turnover = entry.filled_notional_quote + exit.filled_notional_quote;
            let fees = entry.fees_quote + exit.fees_quote;
            let execution_cost_bps = (turnover > Decimal::ZERO
                && entry.fills_without_submit_touch == 0
                && exit.fills_without_submit_touch == 0)
                .then(|| (fees + entry.slippage_quote + exit.slippage_quote) / turnover * BPS);
            PaperMarketCosts {
                venue: terms.venue.clone(),
                market: terms.market.clone(),
                turnover_quote: turnover,
                fees_quote: fees,
                execution_cost_bps,
            }
        });
        for stats in self
            .report
            .opening_execution
            .iter()
            .chain(&self.report.closing_execution)
        {
            self.report.ioc_submitted += stats.ioc_submitted;
            self.report.ioc_filled += stats.ioc_filled;
            self.report.ioc_partial += stats.ioc_partial;
            self.report.ioc_unfilled += stats.ioc_unfilled;
        }
        if let Some(active) = self.active {
            self.report.open_phase = Some(active.phase.label());
            self.report.active_plan = Some(active.plan);
        }
        Ok(self.report)
    }
}

pub fn run_paper(scenario: &PaperScenario, registry: &VenueRegistry) -> Result<PaperReport> {
    run_paper_frames_with_window(
        &scenario.pair,
        scenario.one_way_latency_ms,
        scenario.trade_from_ms,
        scenario.funding_guard_ms,
        PaperWindow::from_bounds(scenario.trade_until_ms, scenario.drain_until_ms)?,
        scenario.frames.iter().map(Ok),
        registry,
    )
}

/// Consume frames in arrival order. Only the online state and round-trip
/// summaries are kept, so an input recording does not need to fit in memory.
pub fn run_paper_frames<T: Borrow<Frame>>(
    pair: &PairConfig,
    one_way_latency_ms: u64,
    trade_from_ms: Option<u64>,
    funding_guard_ms: u64,
    frames: impl IntoIterator<Item = Result<T>>,
    registry: &VenueRegistry,
) -> Result<PaperReport> {
    run_paper_frames_with_window(
        pair,
        one_way_latency_ms,
        trade_from_ms,
        funding_guard_ms,
        None,
        frames,
        registry,
    )
}

/// Optional terminal execution uses real post-cutoff frames; the legacy entry
/// point above deliberately preserves the original EOF-mark behavior.
pub fn run_paper_frames_with_window<T: Borrow<Frame>>(
    pair: &PairConfig,
    one_way_latency_ms: u64,
    trade_from_ms: Option<u64>,
    funding_guard_ms: u64,
    window: Option<PaperWindow>,
    frames: impl IntoIterator<Item = Result<T>>,
    registry: &VenueRegistry,
) -> Result<PaperReport> {
    ensure!(
        one_way_latency_ms > 0 && one_way_latency_ms.saturating_mul(2) < pair.execution_window_ms,
        "execution window must exceed the sequential two-leg latency"
    );
    validate_funding_guard(funding_guard_ms, &pair.legs)?;
    if let Some(window) = window {
        PaperWindow::from_bounds(Some(window.trade_until_ms), Some(window.drain_until_ms))?;
        ensure!(
            trade_from_ms.is_none_or(|start| start < window.trade_until_ms),
            "paper decision window must follow warm-up"
        );
    }
    Runner {
        planner: RelativeValue::new(pair.clone(), registry)?,
        latency: one_way_latency_ms,
        funding_guard_ms,
        trade_from_ms,
        window,
        active: None,
        ledger: Ledger::default(),
        report: PaperReport {
            mode: pair.mode,
            funding_guard_ms,
            terminal_drain: window.map(|window| TerminalDrainReport {
                trade_until_ms: window.trade_until_ms,
                drain_until_ms: window.drain_until_ms,
                ..Default::default()
            }),
            entry_diagnostics: PaperEntryDiagnostics {
                entry_z: pair.basis.entry_z,
                stop_z: pair.basis.stop_z,
                min_net_edge_bps: pair.planner.min_net_edge_bps,
                ..Default::default()
            },
            ..Default::default()
        },
        peak_pnl: Decimal::ZERO,
        unhedged_since: None,
        consumed: [None; 2],
    }
    .run(frames)
}

#[cfg(test)]
mod terminal_tests {
    use super::*;

    fn equity_window(offset: u64, drain_offset: u64) -> PaperScenario {
        let mut scenario: PaperScenario =
            serde_json::from_str(include_str!("../../../examples/equity_basis_paper.json"))
                .unwrap();
        let start = scenario.frames[0].now_ms;
        scenario.trade_until_ms = Some(start + offset);
        scenario.drain_until_ms = Some(start + drain_offset);
        let mut boundary = scenario.frames.last().unwrap().clone();
        boundary.now_ms = start + drain_offset;
        boundary.reference.timestamp_ms = boundary.now_ms;
        for leg in &mut boundary.legs {
            leg.book.timestamp_ms = boundary.now_ms;
            leg.oracle.timestamp_ms = boundary.now_ms;
        }
        scenario
            .frames
            .retain(|frame| frame.now_ms < boundary.now_ms);
        scenario.frames.push(boundary);
        scenario
    }

    #[test]
    fn terminal_window_closes_with_real_later_iocs_and_retains_fees() {
        let scenario = equity_window(600, 1500);
        let report = run_paper(&scenario, &VenueRegistry::default()).unwrap();
        assert_eq!(report.entries_submitted, 1);
        assert_eq!(report.ioc_filled, 4);
        assert_eq!(report.trades[0].exit_reason, ExitReason::WindowEnd);
        assert!(report.trades[0].closed_at_ms >= scenario.trade_until_ms.unwrap());
        assert!(report.trades[0].closed_at_ms < scenario.drain_until_ms.unwrap());
        assert_eq!(report.open_positions_base, [Decimal::ZERO; 2]);
        assert!(report.fees > Decimal::ZERO);
        assert_eq!(report.final_liquidation_pnl, Some(report.realized_pnl));
        let drain = report.terminal_drain.unwrap();
        assert!(drain.complete);
        assert_eq!(drain.first_drain_frame_ms, Some(scenario.frames[6].now_ms));
        assert!(drain.last_execution_frame_ms.unwrap() < drain.drain_until_ms);
    }

    #[test]
    fn terminal_cutoff_keeps_late_primary_fill_and_hedges_only_actual_exposure() {
        let scenario = equity_window(350, 1500);
        let report = run_paper(&scenario, &VenueRegistry::default()).unwrap();
        assert_eq!(report.entries_submitted, 1);
        assert_eq!(report.paired_entries, 1);
        assert_eq!(report.ioc_filled, 4);
        assert_eq!(report.trades[0].exit_reason, ExitReason::WindowEnd);
        assert_eq!(report.open_positions_base, [Decimal::ZERO; 2]);
        assert!(report.terminal_drain.unwrap().complete);
    }

    #[test]
    fn terminal_cutoff_prevents_new_cycles_even_when_later_signals_are_profitable() {
        let scenario = equity_window(250, 1500);
        let report = run_paper(&scenario, &VenueRegistry::default()).unwrap();
        assert_eq!(report.entries_submitted, 0);
        assert_eq!(report.ioc_submitted, 0);
        assert!(report.terminal_drain.unwrap().complete);
    }

    #[test]
    fn terminal_missing_boundary_or_insufficient_drain_time_fails_closed() {
        let mut scenario = equity_window(600, 1500);
        scenario.frames.pop();
        let report = run_paper(&scenario, &VenueRegistry::default()).unwrap();
        assert!(!report.terminal_drain.unwrap().complete);
        assert_eq!(
            report.halted.as_deref(),
            Some("terminal drain missing boundary publication")
        );

        let scenario = equity_window(600, 900);
        let report = run_paper(&scenario, &VenueRegistry::default()).unwrap();
        assert!(!report.terminal_drain.unwrap().complete);
        assert!(
            report
                .open_positions_base
                .iter()
                .any(|position| !position.is_zero())
        );
        assert_eq!(report.ioc_filled, 2);
        assert!(report.halted.is_some());
    }

    #[test]
    fn terminal_bounds_are_paired_and_must_follow_warmup() {
        assert!(PaperWindow::from_bounds(Some(100), None).is_err());
        assert!(PaperWindow::from_bounds(Some(100), Some(100)).is_err());
        let mut scenario = equity_window(600, 1500);
        scenario.trade_from_ms = scenario.trade_until_ms;
        assert!(run_paper(&scenario, &VenueRegistry::default()).is_err());
    }

    #[test]
    fn terminal_drain_does_not_waive_realized_funding_evidence() {
        let mut scenario: PaperScenario =
            serde_json::from_str(include_str!("../../../examples/crypto_funding_paper.json"))
                .unwrap();
        scenario.trade_until_ms = Some(1_799_999_999_900);
        scenario.drain_until_ms = Some(1_800_000_002_000);
        let payment = scenario
            .frames
            .iter()
            .find(|frame| frame.now_ms == 1_800_000_000_000)
            .unwrap()
            .clone();
        for now in [1_800_000_000_100, 1_800_000_000_200] {
            let mut frame = payment.clone();
            frame.now_ms = now;
            frame.reference.timestamp_ms = now;
            for leg in &mut frame.legs {
                leg.book.timestamp_ms = now;
                leg.oracle.timestamp_ms = now;
                leg.settled.clear();
            }
            scenario.frames.push(frame);
        }
        scenario.frames.sort_unstable_by_key(|frame| frame.now_ms);
        let mut boundary = scenario.frames.last().unwrap().clone();
        boundary.now_ms = scenario.drain_until_ms.unwrap();
        scenario.frames.push(boundary);
        let report = run_paper(&scenario, &VenueRegistry::default()).unwrap();
        assert!(report.funding > Decimal::ZERO);
        assert!(report.terminal_drain.unwrap().complete);
        for frame in &mut scenario.frames {
            for leg in &mut frame.legs {
                leg.settled.clear();
            }
        }
        let error = run_paper(&scenario, &VenueRegistry::default()).unwrap_err();
        assert!(error.to_string().contains("missing realized funding"));
    }
}
