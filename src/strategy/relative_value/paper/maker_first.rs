//! Public-tape maker-first replay. The ledger and protected depth model are
//! shared with the sequential IOC simulator.

use super::{Fill, IncentiveTurnoverReport, Ledger, sweep};
use crate::strategy::relative_value::*;
use std::collections::{BTreeMap, VecDeque};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MakerFirstConfig {
    pub pair: PairConfig,
    pub maker_leg: usize,
    pub start_ms: u64,
    pub end_ms: u64,
    pub transport_ms: u64,
    pub fill_notice_ms: u64,
    pub maker_cancel_delay_ms: u64,
    pub taker_delay_ms: [u64; 2],
    pub trade_clock_guard_ms: u64,
    pub trade_delivery_grace_ms: u64,
    pub quote_lifetime_ms: u64,
    pub reprice_ms: u64,
    pub hold_ms: u64,
    pub unhedged_timeout_ms: u64,
    pub drain_ms: u64,
    /// Touch control submits without a net edge gate.
    pub require_net_edge: bool,
    /// If omitted, compare both opening directions on every decision.
    #[serde(default)]
    pub entry_side: Option<HedgeSide>,
    /// Allow small confirmed inventory to close passively before routing it out.
    #[serde(default)]
    pub passive_offset: Option<PassiveOffset>,
    #[serde(with = "rust_decimal::serde::str")]
    pub maker_fee_bps: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub quote_notional: Decimal,
    /// A reserve for changes in basis and hedge prices before matching.
    #[serde(with = "rust_decimal::serde::str")]
    pub latency_reserve_bps: Decimal,
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
            | Self::End { at_ms } => at_ms,
        }
    }
}

#[derive(Debug, Default, Serialize)]
pub struct MakerFirstReport {
    pub start_ms: u64,
    pub end_ms: u64,
    pub book_frames: usize,
    pub synchronized_book_frames: usize,
    pub trade_prints: usize,
    pub entry_checks: usize,
    pub quotes_submitted: usize,
    pub quotes_accepted: usize,
    pub post_only_rejected: usize,
    pub quote_cancels: usize,
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
    pub below_minimum_hedge: usize,
    pub below_minimum_unwind: usize,
    pub incomplete: Option<&'static str>,
}

impl MakerFirstReport {
    fn blocked(&mut self, reason: &'static str) {
        *self.entry_blocked.entry(reason).or_default() += 1;
    }
}

struct Quote {
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
}

struct Candidate {
    edge_bps: Decimal,
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
}

struct Cycle {
    started_ms: u64,
    initial_cash: Decimal,
    draining: bool,
    externalizing: bool,
    maker_turnover: Decimal,
    passive_side: Option<usize>,
}

struct Replay<'a> {
    config: &'a MakerFirstConfig,
    rules: Rules,
    common_step: Decimal,
    ledger: Ledger,
    books: Option<[Book; 2]>,
    usable: [bool; 2],
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
    report: MakerFirstReport,
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

impl MakerFirstConfig {
    fn validate(&self, registry: &VenueRegistry) -> Result<()> {
        self.pair.validate(registry)?;
        ensure!(
            self.pair.asset_class == AssetClass::Crypto
                && (self.pair.mode.is_pair_hedge() || self.require_net_edge)
                && self.maker_leg < 2
                && (1..=DAY_MS).contains(&self.start_ms.abs_diff(self.end_ms))
                && self.start_ms > 0
                && self.start_ms < self.end_ms
                && (1..=2_000).contains(&self.transport_ms)
                && self.fill_notice_ms <= 2_000
                && self.maker_cancel_delay_ms <= 2_000
                && self.taker_delay_ms.iter().all(|d| *d <= 2_000)
                && self.trade_clock_guard_ms <= 1_000
                && self.trade_delivery_grace_ms <= 5_000
                && self.end_ms < u64::MAX - DAY_MS
                && (100..=10_000).contains(&self.quote_lifetime_ms)
                && (1..=2_000).contains(&self.reprice_ms)
                && (1..=60_000).contains(&self.hold_ms)
                && (100..=15_000).contains(&self.unhedged_timeout_ms)
                && (1_000..=60_000).contains(&self.drain_ms)
                && self.drain_ms + self.hold_ms + self.quote_lifetime_ms
                    < self.end_ms - self.start_ms
                && (-Decimal::from(100)..=Decimal::from(1_000)).contains(&self.maker_fee_bps)
                && self.quote_notional > Decimal::ZERO
                && bounded(self.quote_notional)
                && (Decimal::ZERO..=Decimal::from(100)).contains(&self.latency_reserve_bps),
            "invalid maker-first execution policy"
        );
        if let Some(offset) = &self.passive_offset {
            let cancel_window = self.transport_ms
                + self.maker_cancel_delay_ms
                + self.trade_delivery_grace_ms
                + self.trade_clock_guard_ms;
            ensure!(
                offset.max_net_notional > Decimal::ZERO
                    && offset.max_net_notional <= Rules::new(&self.pair)?.max_single_leg
                    && offset.max_wait_ms <= self.unhedged_timeout_ms
                    && offset.max_wait_ms < self.hold_ms
                    && offset.max_wait_ms
                        > 2 * cancel_window + self.transport_ms + self.fill_notice_ms
                    && offset.close_probability.is_none_or(|p| {
                        self.require_net_edge
                            && p.iter().all(|v| (Decimal::ZERO..=Decimal::ONE).contains(v))
                    })
                    && (self.pair.mode.incentive().is_none() || offset.close_probability.is_some()),
                "invalid passive offset size or time budget"
            );
        }
        ensure!(
            self.pair.legs.iter().all(|leg| leg
                .funding
                .window(self.start_ms, self.end_ms)
                .is_some_and(|(_, count)| count == 0)),
            "maker-first execution window crosses a funding slot"
        );
        Ok(())
    }
}

impl Replay<'_> {
    fn maker(&self) -> usize {
        self.config.maker_leg
    }

    fn net(&self) -> Decimal {
        self.ledger.positions.iter().sum()
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
                && book.valid_depth(&self.config.pair.legs[i]);
            if self.usable[i] {
                self.last_source_ms[i] = book.timestamp_ms;
            }
        }
        self.books = Some(books);
        self.report.book_frames += 1;
        self.report.synchronized_book_frames += usize::from(self.pair_books(now).is_some());
        Ok(())
    }

    fn cancel(&mut self, now: u64) {
        if let Some(quote) = self.quote.as_mut()
            && quote.cancelled_ms.is_none()
        {
            quote.cancelled_ms =
                Some(now + self.config.transport_ms + self.config.maker_cancel_delay_ms);
            self.report.quote_cancels += 1;
        }
    }

    fn advance_quote(&mut self, now: u64) {
        let Some(mut quote) = self.quote.take() else {
            return;
        };
        if quote.cancelled_ms.is_some_and(|at| {
            now >= at + self.config.trade_delivery_grace_ms + self.config.trade_clock_guard_ms
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
                    return;
                }
                let same_side = book.side(opposite(quote.side));
                let worst = same_side.last().expect("valid book").price;
                if signed_cost(quote.side, quote.price, worst) < Decimal::ZERO {
                    self.report.post_only_rejected += 1;
                    return;
                }
                quote.ahead = same_side
                    .iter()
                    .find(|level| level.price == quote.price)
                    .map_or(Decimal::ZERO, |level| level.contracts)
                    * self.config.pair.legs[self.maker()].base_per_contract;
                quote.activated_ms = Some(now);
                self.report.quotes_accepted += 1;
            }
        }
        let expired = now >= quote.expires_ms;
        let hedge_side = opposite(quote.side);
        let hedge_leg = 1 - self.maker();
        let hedgeable = match quote.hedge_limit {
            Some(previous) => self.pair_books(now).is_some_and(|books| {
                if !self.config.require_net_edge {
                    return true;
                }
                self.limit(books[hedge_leg], hedge_leg, hedge_side, quote.left)
                    .is_some_and(|limit| {
                        let reference = previous * quote.left;
                        signed_cost(hedge_side, limit * quote.left, reference)
                            <= reference * self.config.latency_reserve_bps / BPS
                    })
            }),
            None => self.passive_cancel_by(now).is_some(),
        };
        self.quote = Some(quote);
        if expired || !hedgeable {
            self.cancel(now);
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
        let Some(quote) = self.quote.as_mut() else {
            return Ok(());
        };
        let guarded = source.saturating_sub(self.config.trade_clock_guard_ms);
        if quote.activated_ms.is_none_or(|at| at >= guarded)
            || quote
                .cancelled_ms
                .is_some_and(|at| source.saturating_add(self.config.trade_clock_guard_ms) >= at)
            || source > now
            || quote.side == side
            || signed_cost(quote.side, price, quote.price) > Decimal::ZERO
        {
            return Ok(());
        }
        self.report.maker_prints_touching_quote += 1;
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
        let passive_side = quote.passive_entry.then_some(side_index(side));
        let offsets_inventory = quote.hedge_limit.is_none();
        if offsets_inventory {
            let position = self.ledger.positions[i];
            ensure!(
                !position.is_zero() && side == direction(position) && base <= position.abs(),
                "passive offset would increase confirmed inventory"
            );
            self.report.passive_offset_fills += 1;
        }
        let notional = base * quote.price;
        quote.left -= base;
        if quote.left.is_zero() {
            self.quote = None;
        }
        let cycle = self.cycle.get_or_insert_with(|| {
            if let Some(i) = passive_side {
                self.report.passive_cycles_started[i] += 1;
            }
            Cycle {
                started_ms: now,
                initial_cash: self.ledger.cash,
                draining: false,
                externalizing: false,
                maker_turnover: Decimal::ZERO,
                passive_side,
            }
        });
        cycle.maker_turnover += notional;
        self.report.maker_fills += 1;
        self.report.maker_buy_fills += usize::from(side == HedgeSide::Buy);
        self.report.maker_sell_fills += usize::from(side == HedgeSide::Sell);
        self.report.maker_turnover += notional;
        self.record_eligible_fill(i, source, notional);
        if let Some(mid) = midpoint {
            self.report.maker_spread_capture -= signed_cost(side, notional, base * mid);
        }
        self.report.maker_fees +=
            self.ledger
                .trade(i, side, Fill { base, notional }, self.config.maker_fee_bps);
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
        ensure!(
            !exposure.is_zero()
                && order.side == direction(exposure)
                && order.quantity <= exposure.abs(),
            "maker-first IOC would increase confirmed exposure"
        );
        let fill = sweep(
            book,
            terms,
            order.side,
            order.limit,
            order.quantity,
            self.rules.participation,
        )?;
        if fill.base.is_zero() {
            self.report.ioc_depth_misses += 1;
            return Ok(());
        }
        let fill_time = book.timestamp_ms;
        self.consumed_ms[order.leg] = fill_time;
        self.report.ioc_fills += 1;
        self.report.ioc_partial += usize::from(fill.base < order.quantity);
        self.report.ioc_slippage += signed_cost(order.side, fill.notional, fill.base * order.touch);
        self.report.ioc_submit_touch_turnover += fill.base * order.touch;
        self.report.ioc_depth_impact += signed_cost(order.side, fill.notional, fill.base * touch);
        self.report.taker_turnover += fill.notional;
        self.record_eligible_fill(order.leg, fill_time, fill.notional);
        self.ledger
            .trade(order.leg, order.side, fill, terms.taker_fee_bps);
        Ok(())
    }

    fn record_eligible_fill(&mut self, leg: usize, time: u64, notional: Decimal) {
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
        let quantity = floor_step(position.abs().min(last.base), terms.base_step())?;
        let Ok(limit) = curve.limit(quantity) else {
            return Ok(());
        };
        let reduces_leg = !self.ledger.positions[i].is_zero()
            && direction(self.ledger.positions[i]) == side
            && quantity <= self.ledger.positions[i].abs();
        if quantity.is_zero() || quantity < min_order_base(terms, limit, reduces_leg) {
            if unwind {
                self.report.below_minimum_unwind += 1;
            } else {
                self.report.below_minimum_hedge += 1;
            }
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
            touch: book.side(side)[0].price,
            arrival_ms,
            expires_ms,
            unwind,
        });
        self.report.ioc_submitted += 1;
        Ok(())
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

    fn candidate(
        &self,
        books: [&Book; 2],
        side: HedgeSide,
        closing: bool,
    ) -> Result<Option<Candidate>> {
        let i = self.maker();
        let j = 1 - i;
        let maker = &self.config.pair.legs[i];
        let hedge = &self.config.pair.legs[j];
        let passive = books[i].side(opposite(side))[0].price;
        let mark = books[0].asks[0].price.max(books[1].asks[0].price);
        let quantity = if closing {
            self.ledger.positions[i].abs()
        } else {
            floor_step(
                (self.config.quote_notional.min(self.rules.max_single_leg) / mark)
                    .min(self.rules.max_base),
                self.common_step,
            )?
        };
        if quantity.is_zero() || quantity < maker.min_base(passive) || !books[i].in_bounds(passive)
        {
            return Ok(None);
        }
        let Some(hedge_price) = self.limit(books[j], j, opposite(side), quantity) else {
            return Ok(None);
        };
        let maker_cash = cash(side, passive * quantity, self.config.maker_fee_bps);
        let hedge_cash = cash(opposite(side), hedge_price * quantity, hedge.taker_fee_bps);
        let queue_ahead = books[i].side(opposite(side))[0].contracts;
        let mut candidate = Candidate {
            edge_bps: Decimal::ZERO,
            passive_entry: false,
            queue_ahead,
            price: passive,
            quantity,
            side,
            hedge_limit: Some(hedge_price),
            turnover: None,
        };
        let cash_edge = if let Some(cycle) = &self.cycle {
            (maker_cash + hedge_cash) + (self.ledger.cash - cycle.initial_cash)
        } else {
            let Some(edge) = self.opening_edge(books, &mut candidate, maker_cash, hedge_cash)
            else {
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
            return Some(paired_cash);
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

    fn quote(&mut self, now: u64) -> Result<()> {
        if self.quote.is_some()
            || self.ioc.is_some()
            || now < self.next_quote_ms
            || self.report.incomplete.is_some()
        {
            return Ok(());
        }
        let closing = self.cycle.is_some();
        let horizon = self.config.drain_ms + self.config.hold_ms + self.config.quote_lifetime_ms;
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
        if !closing
            && incentive.is_some_and(|p| !p.active_at(now) || now + horizon >= p.eligible_until_ms)
        {
            self.report.blocked("turnover_eligibility");
            return Ok(());
        }
        if !closing && mode.realized_loss_exhausted(self.ledger.cash) {
            self.report.blocked("turnover_loss_limit");
            return Ok(());
        }
        let budget =
            mode.remaining_budget(self.ledger.turnover, self.ledger.fees, self.ledger.cash);
        let mut best: Option<(Decimal, Candidate)> = None;
        let mut blocked = None;
        let mut best_edge = self.report.best_modeled_edge_bps;
        let mut best_cost = self.report.best_projected_turnover_cost_bps;
        for side in [HedgeSide::Buy, HedgeSide::Sell] {
            if (closing && side != direction(self.ledger.positions[self.maker()]))
                || (!closing
                    && self
                        .config
                        .entry_side
                        .is_some_and(|opening| side != opening))
            {
                continue;
            }
            let Some(candidate) = self.candidate(books, side, closing)? else {
                continue;
            };
            if !closing {
                let value = best_edge.get_or_insert(candidate.edge_bps);
                *value = (*value).max(candidate.edge_bps);
            }
            let mut rank = candidate.edge_bps;
            if let Some(projection) = &candidate.turnover {
                let budget = budget.expect("turnover mode has a budget");
                let loss = -projection.fallback_cash
                    + self.config.latency_reserve_bps / BPS * projection.gross;
                if projection.gross > budget.turnover
                    || projection.fees > budget.fees
                    || budget.loss.is_some_and(|remaining| loss > remaining)
                {
                    blocked = Some("turnover_budget");
                    continue;
                }
                if let Some(policy) = incentive {
                    if projection.eligible <= Decimal::ZERO {
                        blocked = Some("turnover_cost");
                        continue;
                    }
                    let cost_bps = loss * BPS / projection.eligible;
                    let value = best_cost.get_or_insert(cost_bps);
                    *value = (*value).min(cost_bps);
                    if !policy.within_cost(self.ledger.cash, self.ledger.eligible_turnover)
                        || !policy.within_cost(-loss, projection.eligible)
                    {
                        blocked = Some("turnover_cost");
                        continue;
                    }
                    rank = -cost_bps;
                }
            }
            if best.as_ref().is_none_or(|(prior_rank, prior)| {
                (rank, prior.queue_ahead) > (*prior_rank, candidate.queue_ahead)
            }) {
                best = Some((rank, candidate));
            }
        }
        self.report.best_modeled_edge_bps = best_edge;
        self.report.best_projected_turnover_cost_bps = best_cost;
        self.next_quote_ms = now + self.config.reprice_ms;
        let Some((_, best)) = best else {
            self.report
                .blocked(blocked.unwrap_or("size_or_hedge_depth"));
            return Ok(());
        };
        if self.config.require_net_edge
            && incentive.is_none()
            && best.edge_bps < self.rules.min_edge
        {
            self.report.blocked(if closing {
                "exit_net_edge"
            } else {
                "entry_net_edge"
            });
            return Ok(());
        }
        self.post(now, best, u64::MAX);
        Ok(())
    }

    fn edge_bps(&self, cash_edge: Decimal, notional: Decimal) -> Decimal {
        (cash_edge * BPS / notional - self.config.latency_reserve_bps).round_dp(8)
    }

    fn post(&mut self, now: u64, candidate: Candidate, cancel_by: u64) {
        let arrival_ms = now + self.config.transport_ms;
        self.quote = Some(Quote {
            side: candidate.side,
            price: candidate.price,
            left: candidate.quantity,
            arrival_ms,
            ahead: Decimal::ZERO,
            activated_ms: None,
            expires_ms: (arrival_ms + self.config.quote_lifetime_ms).min(cancel_by),
            cancelled_ms: None,
            hedge_limit: candidate.hedge_limit,
            passive_entry: candidate.passive_entry,
        });
        self.report.quotes_submitted += 1;
    }

    fn quote_passive_offset(&mut self, now: u64, cancel_by: u64) {
        if self.quote.is_some()
            || self.ioc.is_some()
            || !self.notices.is_empty()
            || now < self.next_quote_ms
            || now + self.config.transport_ms >= cancel_by
            || self.report.incomplete.is_some()
        {
            return;
        }
        self.next_quote_ms = now + self.config.reprice_ms;
        let i = self.maker();
        let Some(book) = self.fresh(i, now) else {
            return;
        };
        let position = self.ledger.positions[i];
        let side = direction(position);
        let quantity = position.abs();
        let price = book.side(opposite(side))[0].price;
        if quantity < min_order_base(&self.config.pair.legs[i], price, true)
            || !book.in_bounds(price)
        {
            return;
        }
        let cycle = self.cycle.as_ref().expect("passive inventory cycle");
        let round_trip_cash = self.ledger.cash - cycle.initial_cash
            + cash(side, quantity * price, self.config.maker_fee_bps);
        let edge_bps = self.edge_bps(round_trip_cash, quantity * price);
        if self.config.require_net_edge && edge_bps < self.rules.min_edge {
            self.report.blocked("passive_offset_edge");
            return;
        }
        self.post(
            now,
            Candidate {
                side,
                price,
                quantity,
                edge_bps,
                queue_ahead: Decimal::ZERO,
                hedge_limit: None,
                passive_entry: false,
                turnover: None,
            },
            cancel_by,
        );
    }

    fn advance(&mut self, now: u64) -> Result<()> {
        self.poll_ioc(now)?;
        let mut confirmed = false;
        while self.notices.front().is_some_and(|(at, _)| *at <= now) {
            self.notices.pop_front();
            confirmed = true;
        }
        if confirmed
            && self
                .quote
                .as_ref()
                .is_some_and(|quote| quote.hedge_limit.is_some())
        {
            self.cancel(now);
        }
        if let Some(cycle) = &mut self.cycle {
            let exposure_stop = self
                .unhedged_since
                .is_some_and(|at| now >= at + self.config.unhedged_timeout_ms);
            if exposure_stop && !cycle.draining {
                self.report.exposure_stops += 1;
            }
            cycle.draining |= now >= cycle.started_ms + self.config.hold_ms
                || now >= self.config.end_ms - self.config.drain_ms
                || self.report.incomplete.is_some()
                || exposure_stop;
        }
        if self.cycle.as_ref().is_some_and(|cycle| cycle.draining) {
            self.cancel(now);
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
                let full = self.ledger.positions[i];
                let below_minimum = self.fresh(i, now).is_some_and(|book| {
                    let terms = &self.config.pair.legs[i];
                    let touch = book.side(direction(quantity))[0].price;
                    floor_step(quantity.abs(), terms.base_step())
                        .is_ok_and(|base| base < min_order_base(terms, touch, true))
                });
                // If the residual cannot be traded, a protected full close
                // can still clear the leg when its companion remains tradable.
                let companion = self.ledger.positions[1 - i];
                let companion_closeable = self.fresh(1 - i, now).is_some_and(|book| {
                    let terms = &self.config.pair.legs[1 - i];
                    let touch = book.side(direction(companion))[0].price;
                    companion.abs() >= min_order_base(terms, touch, true)
                });
                let close = if below_minimum && full.abs() > quantity.abs() && companion_closeable {
                    full
                } else {
                    quantity
                };
                self.submit_ioc(now, i, close, true)?;
            }
        } else if !self.net().is_zero() {
            if let Some(cancel_by) = self.passive_cancel_by(now) {
                self.quote_passive_offset(now, cancel_by);
            } else {
                self.externalize(now)?;
            }
        }
        if self.ledger.is_flat()
            && self.quote.is_none()
            && self.ioc.is_none()
            && (self.notices.is_empty() || self.config.passive_offset.is_none())
            && let Some(cycle) = self.cycle.take()
        {
            let pnl = self.ledger.cash - cycle.initial_cash;
            let eligible = std::mem::take(&mut self.ledger.cycle_eligible_turnover);
            self.ledger.eligible_turnover += eligible;
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
            self.report.realized_pnl += pnl;
        }
        if self.net().is_zero()
            && (self.notices.is_empty() || self.config.passive_offset.is_none())
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
            self.cancel(now);
        } else {
            let pending: Decimal = self.notices.iter().map(|(_, quantity)| quantity).sum();
            let confirmed = self.net() - pending;
            self.submit_ioc(now, 1 - self.maker(), confirmed, false)?;
        }
        Ok(())
    }

    fn risk(&mut self, now: u64) {
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

    fn final_mark(&self) -> Result<Option<Decimal>> {
        let mut value = self.ledger.cash;
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
            if fill.base != position.abs() || fill.base < min_order_base(terms, limit, true) {
                return Ok(None);
            }
            value += cash(side, fill.notional, terms.taker_fee_bps);
        }
        Ok(Some(value))
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
                    if leg == self.maker() && self.feed_online {
                        self.trade(now, source_ms, aggressor, px, contracts)?;
                    }
                }
                MakerFirstEvent::TradeFeed { leg, online, .. } => {
                    ensure!(leg < 2, "invalid maker-first trade feed leg");
                    if leg == self.maker() {
                        self.feed_online = online;
                        if !online {
                            if self
                                .quote
                                .as_ref()
                                .is_some_and(|quote| quote.activated_ms.is_some())
                            {
                                self.report.incomplete = Some("resting_order_during_trade_gap");
                            }
                            self.cancel(now);
                        }
                    }
                }
                MakerFirstEvent::End { .. } => {
                    ensure!(
                        now == self.config.end_ms,
                        "incorrect maker-first tape cutoff"
                    );
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
                self.advance_quote(now);
            }
            self.advance(now)?;
            self.risk(now);
        }
        ensure!(
            ended && self.report.book_frames > 0,
            "incomplete maker-first tape"
        );
        if let Some(at) = self.unhedged_since {
            let elapsed = self.config.end_ms - at;
            self.report.total_unhedged_ms += elapsed;
            self.report.max_unhedged_ms = self.report.max_unhedged_ms.max(elapsed);
        }
        self.report.open_positions_base = self.ledger.positions;
        self.report.fees = self.ledger.fees;
        self.report.final_liquidation_pnl = self.final_mark()?;
        if !self.ledger.is_flat()
            || self.quote.is_some()
            || self.ioc.is_some()
            || !self.notices.is_empty()
        {
            self.report.incomplete.get_or_insert("residual_at_cutoff");
        }
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
        Ok(self.report)
    }
}

pub fn run_maker_first(
    config: &MakerFirstConfig,
    events: impl IntoIterator<Item = Result<MakerFirstEvent>>,
    registry: &VenueRegistry,
) -> Result<MakerFirstReport> {
    config.validate(registry)?;
    let [a, b] = &config.pair.legs;
    Replay {
        config,
        rules: Rules::new(&config.pair)?,
        common_step: common_quantity_step(a.base_step(), b.base_step())?,
        ledger: Ledger::default(),
        books: None,
        usable: [false; 2],
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
        report: MakerFirstReport {
            start_ms: config.start_ms,
            end_ms: config.end_ms,
            ..Default::default()
        },
    }
    .run(events)
}

#[cfg(test)]
mod tests {
    use super::*;

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
            maker_cancel_delay_ms: 100,
            taker_delay_ms: [0; 2],
            trade_clock_guard_ms: 0,
            trade_delivery_grace_ms: 150,
            quote_lifetime_ms: 800,
            reprice_ms: 100,
            hold_ms: 2_000,
            unhedged_timeout_ms: 1_000,
            drain_ms: 4_000,
            require_net_edge: false,
            entry_side: None,
            passive_offset: None,
            maker_fee_bps: Decimal::ONE,
            quote_notional: Decimal::from(100),
            latency_reserve_bps: Decimal::ZERO,
        }
    }

    fn d(value: &str) -> Decimal {
        value.parse().unwrap()
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
}
