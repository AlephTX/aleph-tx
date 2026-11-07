//! Sequential IOC replay against later L2 publications, with realized funding.

use super::*;
use crate::hedging::{HedgeAction, HedgeMachine, OrderIntent, OrderStatus, OrderUpdate};
use std::{borrow::Borrow, collections::BTreeMap};

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
    pub frames: Vec<Frame>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitReason {
    Reverted,
    BasisStop,
    HoldingTime,
    ReferenceLost,
    SessionChanged,
    FundingDeteriorated,
    FundingUnavailable,
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
    pub submit_touch_notional_quote: Decimal,
    /// Signed cost relative to the same-side touch at submission.
    #[serde(with = "rust_decimal::serde::str")]
    pub slippage_quote: Decimal,
    /// Cost of sweeping levels beyond the arrival book's same-side touch.
    #[serde(with = "rust_decimal::serde::str")]
    pub depth_impact_quote: Decimal,
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
    /// Best conservative edge among candidates that fit size and budget limits.
    #[serde(with = "rust_decimal::serde::str_option")]
    pub best_net_edge_bps: Option<Decimal>,
}

#[derive(Debug, Default, Serialize)]
pub struct PaperReport {
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
        }
    }
}

impl Ledger {
    fn trade(&mut self, i: usize, side: HedgeSide, fill: Fill, fee_bps: Decimal) {
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

enum Phase {
    Opening(Execution),
    Holding,
    Closing(Execution),
    Halted,
}

impl Phase {
    fn label(&self) -> &'static str {
        match self {
            Self::Opening(_) => "opening",
            Self::Holding => "holding",
            Self::Closing(_) => "closing",
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
    fn advance(
        &mut self,
        execution: &mut Execution,
        frame: &Frame,
        opening: bool,
    ) -> Result<Advance> {
        let now = frame.now_ms;
        if let Some(mut pending) = execution.pending.take() {
            let i = usize::from(pending.order.venue == self.planner.config.legs[1].venue);
            let book = &frame.legs[i].book;
            let order = &pending.order;
            let (fill, unfilled_gate) = if now >= execution.deadline {
                // The simulator expires intents without a usable publication
                // before their deadline. It has no exchange-side order in flight.
                self.report.ioc_expired_without_book += 1;
                (Fill::default(), Some(pending.expiry_gate))
            } else {
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
                    execution.pending = Some(pending);
                    return Ok(Advance::Working);
                }
                let touch = book.side(order.side)[0].price;
                let marketable = match order.side {
                    HedgeSide::Buy => touch <= order.limit_price,
                    HedgeSide::Sell => touch >= order.limit_price,
                };
                if !book.in_bounds(order.limit_price) || !book.in_bounds(touch) {
                    (Fill::default(), Some(PaperIocGate::PriceBand))
                } else if !marketable {
                    (Fill::default(), Some(PaperIocGate::LimitNotMarketable))
                } else {
                    let fill = sweep(
                        book,
                        &self.planner.config.legs[i],
                        order.side,
                        order.limit_price,
                        order.base_quantity,
                        self.planner.rules.participation,
                    )?;
                    let gate = fill.base.is_zero().then_some(PaperIocGate::DepthBelowLot);
                    (fill, gate)
                }
            };
            let order = pending.order;
            let stats = self.report.execution(opening, i);
            let status = if fill.base == order.base_quantity {
                stats.ioc_filled += 1;
                OrderStatus::Filled
            } else {
                if fill.base > Decimal::ZERO {
                    stats.ioc_partial += 1;
                } else {
                    stats.ioc_unfilled += 1;
                    if let Some(gate) = unfilled_gate {
                        *stats.unfilled_by_gate.entry(gate).or_default() += 1;
                    }
                }
                OrderStatus::Canceled
            };
            execution.machine.apply_update(OrderUpdate {
                client_order_id: order.client_order_id,
                venue: order.venue,
                exchange_order_id: None,
                cumulative_filled_base: fill.base,
                status,
            })?;
            if fill.base > Decimal::ZERO {
                self.consumed[i] = Some((book.timestamp_ms, order.side));
                let side_sign = if order.side == HedgeSide::Buy {
                    Decimal::ONE
                } else {
                    -Decimal::ONE
                };
                stats.filled_base += fill.base;
                stats.filled_notional_quote += fill.notional;
                stats.max_fill_elapsed_ms =
                    stats.max_fill_elapsed_ms.max(now - pending.submitted_at_ms);
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
            self.ledger.trade(
                i,
                order.side,
                fill,
                self.planner.config.legs[i].taker_fee_bps,
            );
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
                self.report.execution(opening, i).ioc_submitted += 1;
                let submit_touch = self
                    .planner
                    .usable_book(frame, i)
                    .then(|| frame.legs[i].book.side(order.side)[0].price);
                execution.pending = Some(Pending {
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

    fn exit_execution(&self, active: &Active, frame: &Frame) -> Result<Option<Execution>> {
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
            .min(cutoff);
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
        let limit = |i: usize| {
            let curve = &curves[i][usize::from(sides[i] == HedgeSide::Sell)];
            curve.limit(base).or_else(|_| curve.last().map(|b| b.limit))
        };
        let (Ok(a), Ok(b)) = (limit(0), limit(1)) else {
            return Ok(None);
        };
        if (0..2).any(|i| base < self.planner.config.legs[i].min_base([a, b][i])) {
            return Ok(None);
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
                        self.report.halted = Some(format!("entry {reason}"));
                        active.phase = Phase::Halted;
                    }
                }
            },
            Phase::Holding => {
                let reason = active
                    .exit_reason
                    .or_else(|| self.exit_reason(active, frame));
                if let Some(reason) = reason {
                    active.exit_reason = Some(reason);
                    if frame.now_ms >= active.plan.unwind_until_ms {
                        self.report.halted = Some("exit window expired".into());
                        active.phase = Phase::Halted;
                    } else if let Some(mut exec) = self.exit_execution(active, frame)? {
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
                    self.report.halted = Some(format!("exit {reason}"));
                    active.phase = Phase::Halted;
                }
            },
            Phase::Halted => {}
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
        let mut exec = Execution::new(&plan.hedge)?;
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
            self.report.frames += 1;
            ensure!(self.report.frames <= MAX_FRAMES, "too many paper frames");
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
            } else {
                let diagnostics = &mut self.report.entry_diagnostics;
                diagnostics.idle_frames += 1;
                let plan = sample.and_then(|(signal, cutoff)| {
                    let abs_z = signal.z.abs();
                    diagnostics.max_abs_z =
                        Some(diagnostics.max_abs_z.map_or(abs_z, |max| max.max(abs_z)));
                    self.planner.plan(
                        frame,
                        signal,
                        cutoff,
                        Some(&mut diagnostics.best_net_edge_bps),
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
        self.report.open_positions_base = self.ledger.positions;
        self.report.gross_turnover = self.ledger.turnover;
        self.report.fees = self.ledger.fees;
        self.report.funding = self.ledger.funding;
        self.report.funding_upper_bound = self.ledger.funding_upper;
        self.report.witnessed_funding_payments = self.ledger.witnessed_funding_payments;
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
    run_paper_frames(
        &scenario.pair,
        scenario.one_way_latency_ms,
        scenario.trade_from_ms,
        scenario.funding_guard_ms,
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
    ensure!(
        one_way_latency_ms > 0 && one_way_latency_ms.saturating_mul(2) < pair.execution_window_ms,
        "execution window must exceed the sequential two-leg latency"
    );
    validate_funding_guard(funding_guard_ms, &pair.legs)?;
    Runner {
        planner: RelativeValue::new(pair.clone(), registry)?,
        latency: one_way_latency_ms,
        funding_guard_ms,
        trade_from_ms,
        active: None,
        ledger: Ledger::default(),
        report: PaperReport {
            funding_guard_ms,
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
