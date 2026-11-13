//! Replay public depth and trades through the production maker and OrderTracker.
use super::*;
use crate::config::EXCH_HYPERLIQUID;
use anyhow::{Result, ensure};
use chrono::{DateTime, Datelike};
use serde::{Deserialize, Serialize};
use std::cell::{Cell, RefCell};
use std::io::BufRead;
use tokio::sync::watch;

mod continuous;

const HOUR_MS: u64 = 3_600_000;
const NS_PER_MS: u64 = 1_000_000;
const SIGNAL_THRESHOLD_BPS: f64 = 0.01;
const MARKOUT_MS: u64 = 1_000;

thread_local! {
    static CLOCK: Cell<Option<(u64, Instant)>> = const { Cell::new(None) };
    static ATTRIBUTION: RefCell<Option<QuoteAttribution>> = const { RefCell::new(None) };
    static CHANGED_QUOTES: Cell<[bool; 2]> = const { Cell::new([false; 2]) };
}

#[derive(Default, Serialize)]
struct QuoteAttribution {
    evaluated: u64,
    signal_nonzero: u64,
    signal_at_least_0_01_bps: u64,
    reserve_dominates: u64,
    snapped_quote_changed: u64,
    snapped_side_changes: [u64; 2],
}

pub(in crate::strategy::inventory_neutral_mm) fn attribute_quote(
    strategy: &InventoryNeutralMM,
    inputs: &PricingInputs,
    position: f64,
    quoted: Option<(f64, f64, MakerPriceBounds)>,
) -> [bool; 2] {
    ATTRIBUTION.with_borrow_mut(|stats| {
        let Some(stats) = stats else {
            return [false; 2];
        };
        stats.evaluated += 1;
        let signal = inputs.cross_market_bps;
        stats.signal_nonzero += u64::from(signal != 0.0);
        stats.signal_at_least_0_01_bps += u64::from(signal.abs() >= SIGNAL_THRESHOLD_BPS);
        if signal == 0.0 || !inputs.using_external_fair_value {
            return [false; 2];
        }
        let [bid_other, ask_other] = strategy.competing_directional_reserves(inputs);
        if !((position >= 0.0 && -signal > bid_other) || (position <= 0.0 && signal > ask_other)) {
            return [false; 2];
        }
        stats.reserve_dominates += 1;
        let mut control = inputs.clone();
        control.cross_market_bps = 0.0;
        let control = strategy.calculate_optimal_quotes(&control, position);
        let changed = match (quoted, control) {
            (Some((bid, ask, _)), Some((control_bid, control_ask, _))) => {
                [bid != control_bid, ask != control_ask]
            }
            (Some(_), None) | (None, Some(_)) => [true; 2],
            (None, None) => [false; 2],
        };
        stats.snapped_quote_changed += u64::from(changed.into_iter().any(|value| value));
        for (count, changed) in stats.snapped_side_changes.iter_mut().zip(changed) {
            *count += u64::from(changed);
        }
        changed
    })
}

pub(in crate::strategy::inventory_neutral_mm) struct ChangedQuotes(Option<[bool; 2]>);

impl ChangedQuotes {
    pub(in crate::strategy::inventory_neutral_mm) fn open(sides: [bool; 2]) -> Self {
        Self(
            sides
                .into_iter()
                .any(|changed| changed)
                .then(|| CHANGED_QUOTES.replace(sides)),
        )
    }
}

impl Drop for ChangedQuotes {
    fn drop(&mut self) {
        if let Some(prior) = self.0 {
            CHANGED_QUOTES.set(prior);
        }
    }
}

struct ReplayClock;

impl ReplayClock {
    fn open(start_ms: u64, diagnose: bool) -> Self {
        assert!(
            CLOCK
                .replace(Some((start_ms * NS_PER_MS, Instant::now())))
                .is_none()
        );
        ATTRIBUTION.replace(diagnose.then(QuoteAttribution::default));
        Self
    }
}

impl Drop for ReplayClock {
    fn drop(&mut self) {
        CLOCK.set(None);
        ATTRIBUTION.replace(None);
    }
}

pub(in crate::strategy::inventory_neutral_mm) fn wall_time_ns() -> Option<u64> {
    CLOCK
        .get()
        .map(|(ns, start)| ns + start.elapsed().as_nanos() as u64)
}

pub(in crate::strategy::inventory_neutral_mm) fn utc_day() -> Option<i32> {
    wall_time_ns().map(|ns| {
        DateTime::from_timestamp_millis((ns / NS_PER_MS) as i64)
            .expect("validated replay epoch")
            .date_naive()
            .num_days_from_ce()
    })
}

fn now_ms() -> u64 {
    wall_time_ns().expect("recorded replay clock") / NS_PER_MS
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct HyperliquidObiFilter {
    threshold: f64,
    max_age_ms: u64,
}

impl HyperliquidObiFilter {
    fn blocked_sides(
        self,
        book: Option<ShmBboMessage>,
        at_ns: u64,
        positions: [f64; 2],
    ) -> Option<[bool; 2]> {
        let book = book.filter(|book| {
            book.bid_price > 0.0
                && book.ask_price > book.bid_price
                && book.bid_size.is_finite()
                && book.ask_size.is_finite()
                && book.bid_size > 0.0
                && book.ask_size > 0.0
                && !market_state::is_stale_bbo(book.timestamp_ns, at_ns, self.max_age_ms)
        })?;
        let scale = book.bid_size.max(book.ask_size);
        let bid = book.bid_size / scale;
        let ask = book.ask_size / scale;
        let imbalance = (bid - ask) / (bid + ask);
        // Both account and fill evidence must agree the blocked side cannot
        // close inventory. An older position view must not suppress an exit.
        Some([
            imbalance <= -self.threshold && positions.iter().all(|&p| p >= 0.0),
            imbalance >= self.threshold && positions.iter().all(|&p| p <= 0.0),
        ])
    }
}

#[derive(Serialize)]
pub(in crate::strategy::inventory_neutral_mm) struct QuoteParticipation {
    hyperliquid_obi_filter: HyperliquidObiFilter,
    decisions: u64,
    fresh_reference_decisions: u64,
    withdrawn_bid_targets: u64,
    withdrawn_ask_targets: u64,
}

impl QuoteParticipation {
    pub(in crate::strategy::inventory_neutral_mm) fn apply(
        &mut self,
        target: &mut QuoteTarget,
        book: Option<ShmBboMessage>,
        positions: [f64; 2],
        config: &InventoryNeutralMMConfig,
    ) {
        self.decisions += 1;
        let at_ns = wall_time_ns().expect("recorded quote participation requires replay clock");
        let Some([bid, ask]) = self
            .hyperliquid_obi_filter
            .blocked_sides(book, at_ns, positions)
        else {
            return;
        };
        self.fresh_reference_decisions += 1;
        // A zero target goes through the production side planner. Resting
        // orders remain exposed until the simulated cancel takes effect.
        if bid && target.bid_size >= components::min_quotable_size(config, target.bid_price) {
            self.withdrawn_bid_targets += 1;
            target.bid_size = 0.0;
        }
        if ask && target.ask_size >= components::min_quotable_size(config, target.ask_price) {
            self.withdrawn_ask_targets += 1;
            target.ask_size = 0.0;
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordedMakerConfig {
    strategy: InventoryNeutralMMConfig,
    start_ms: u64,
    end_ms: u64,
    drain_until_ms: u64,
    initial_equity: f64,
    order_entry_ms: u64,
    #[serde(default)]
    venue_cancel_delay_ms: u64,
    #[serde(default)]
    venue_taker_delay_ms: u64,
    #[serde(default)]
    market_data_delay_ms: u64,
    #[serde(default)]
    cancel_transport_ms: Option<u64>,
    #[serde(default)]
    taker_transport_ms: Option<u64>,
    #[serde(default)]
    diagnose_cross_market: bool,
    #[serde(default)]
    hyperliquid_obi_filter: Option<HyperliquidObiFilter>,
    order_response_ms: u64,
    trade_clock_guard_ms: u64,
    /// Maximum source-to-receipt delay of the public trade tape. Evidence
    /// outside this bound invalidates the replay, even when no order is open.
    #[serde(default = "default_trade_delivery_grace_ms")]
    trade_delivery_grace_ms: u64,
    #[serde(default)]
    flat_at_funding: Option<FlatAtFunding>,
    #[serde(default)]
    trade_from_ms: Option<u64>,
    taker_fee_bps: f64,
    min_contracts: f64,
    min_notional: f64,
    depth_participation: f64,
}

fn default_trade_delivery_grace_ms() -> u64 {
    2_000
}

/// Research-only schedule. No funding cash flow is assumed: the strategy
/// must be demonstrably flat throughout every settlement guard interval.
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FlatAtFunding {
    interval_ms: u64,
    offset_ms: u64,
    drain_before_ms: u64,
    guard_ms: u64,
    max_event_gap_ms: u64,
}

impl FlatAtFunding {
    fn next_slot(&self, at: u64) -> u64 {
        if at < self.offset_ms {
            self.offset_ms
        } else {
            (at - self.offset_ms) / self.interval_ms * self.interval_ms
                + self.offset_ms
                + self.interval_ms
        }
    }

    fn phase(&self, at: u64) -> FundingPhase {
        let next = self.next_slot(at);
        let previous = next.saturating_sub(self.interval_ms);
        if at < previous.saturating_add(self.guard_ms) {
            FundingPhase::Guard(previous)
        } else if at >= next.saturating_sub(self.guard_ms) {
            FundingPhase::Guard(next)
        } else if at >= next.saturating_sub(self.guard_ms + self.drain_before_ms) {
            FundingPhase::Drain(next)
        } else {
            FundingPhase::Trading
        }
    }

    fn allows_arrival(&self, at: u64, reducing: bool) -> bool {
        matches!(self.phase(at), FundingPhase::Trading)
            || (reducing && matches!(self.phase(at), FundingPhase::Drain(_)))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FundingPhase {
    Trading,
    Drain(u64),
    Guard(u64),
}

impl RecordedMakerConfig {
    fn validate(&self) -> Result<()> {
        self.strategy.validate()?;
        ensure!(
            (1_600_000_000_000..2_300_000_000_000).contains(&self.start_ms)
                && self.end_ms > self.start_ms
                && self.drain_until_ms > self.end_ms
                && self.drain_until_ms < 2_300_000_000_000
                && self.drain_until_ms - self.end_ms <= 120_000
                && self
                    .trade_from_ms
                    .is_none_or(|at| at >= self.start_ms && at < self.end_ms),
            "invalid maker replay time range"
        );
        if let Some(schedule) = &self.flat_at_funding {
            ensure!(
                (60_000..=86_400_000).contains(&schedule.interval_ms)
                    && schedule.offset_ms < schedule.interval_ms
                    && schedule.guard_ms >= self.trade_clock_guard_ms.max(1)
                    && schedule.guard_ms <= 60_000
                    && (1_000..=120_000).contains(&schedule.drain_before_ms)
                    && schedule.drain_before_ms > self.trade_delivery_grace_ms
                    && schedule.guard_ms * 2 + schedule.drain_before_ms < schedule.interval_ms
                    && (1..=60_000).contains(&schedule.max_event_gap_ms),
                "invalid flat-at-funding schedule"
            );
        } else {
            ensure!(
                self.drain_until_ms - self.end_ms <= 20_000
                    && self.start_ms / HOUR_MS == self.drain_until_ms / HOUR_MS,
                "maker replay must fit in one UTC funding hour without flat_at_funding"
            );
        }
        ensure!(
            (0.0..=100_000_000.0).contains(&self.initial_equity)
                && self.initial_equity > 0.0
                && (1..=2_000).contains(&self.order_entry_ms)
                && self.venue_cancel_delay_ms <= 2_000
                && self.venue_taker_delay_ms <= 2_000
                && self.market_data_delay_ms <= 2_000
                && self.cancel_transport_ms.is_none_or(|value| value <= 2_000)
                && self.taker_transport_ms.is_none_or(|value| value <= 2_000)
                && self.order_response_ms <= 2_000
                && self.trade_clock_guard_ms <= 1_000
                && (1..=60_000).contains(&self.trade_delivery_grace_ms)
                && (0.0..=1_000.0).contains(&self.taker_fee_bps)
                && self.min_contracts.is_finite()
                && self.min_contracts >= self.strategy.step_size
                && self.min_notional.is_finite()
                && self.min_notional > 0.0
                && (0.0..=1.0).contains(&self.depth_participation)
                && self.depth_participation > 0.0,
            "invalid recorded maker execution parameters"
        );
        ensure!(
            self.strategy.use_post_only && !self.strategy.use_depth_pricing,
            "recorded maker requires post-only quoting and BBO pricing"
        );
        ensure!(
            self.hyperliquid_obi_filter.is_none_or(|filter| {
                filter.threshold > 0.0
                    && filter.threshold < 1.0
                    && (1..=self.strategy.external_staleness_ms).contains(&filter.max_age_ms)
            }),
            "invalid Hyperliquid OBI filter threshold or reference age"
        );
        Ok(())
    }
}

#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Book {
    timestamp_ms: u64,
    bids: Vec<[f64; 2]>,
    asks: Vec<[f64; 2]>,
}

impl Book {
    fn validate(&self, at_ms: u64) -> Result<()> {
        if self.timestamp_ms == 0 {
            ensure!(
                self.bids.is_empty() && self.asks.is_empty(),
                "invalid missing book"
            );
            return Ok(());
        }
        ensure!(
            self.timestamp_ms <= at_ms
                && self
                    .bids
                    .first()
                    .zip(self.asks.first())
                    .is_some_and(|(b, a)| b[0] < a[0])
                && [(&self.bids, true), (&self.asks, false)]
                    .into_iter()
                    .all(|(side, bid)| {
                        side.len() <= 20
                            && side
                                .iter()
                                .all(|&[p, q]| valid_price(p) && q.is_finite() && q > 0.0)
                            && side.windows(2).all(|w| {
                                if bid {
                                    w[0][0] > w[1][0]
                                } else {
                                    w[0][0] < w[1][0]
                                }
                            })
                    }),
            "invalid recorded depth"
        );
        Ok(())
    }

    fn fresh(&self, now: u64, max_age: u64) -> bool {
        self.timestamp_ms != 0 && now >= self.timestamp_ms && now - self.timestamp_ms <= max_age
    }

    fn touch(&self) -> Option<([f64; 2], [f64; 2])> {
        self.bids.first().copied().zip(self.asks.first().copied())
    }

    fn write_bbo(&self, shm: &ShmFiles, exchange: u8) {
        let (bid, ask) = self.touch().unwrap_or_default();
        shm.write_bbo(
            exchange,
            (bid[0], bid[1]),
            (ask[0], ask[1]),
            self.timestamp_ms * NS_PER_MS,
        );
    }
}

fn valid_price(price: f64) -> bool {
    price.is_finite() && price > 0.0 && price <= 1e10
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Aggressor {
    Buy,
    Sell,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Event {
    TradeFeed {
        at_ms: u64,
        online: bool,
    },
    Book {
        at_ms: u64,
        lighter: Book,
        hyperliquid: Book,
    },
    Trade {
        at_ms: u64,
        source_ms: u64,
        price: f64,
        size: f64,
        aggressor: Aggressor,
    },
}

impl Event {
    fn at_ms(&self) -> u64 {
        match *self {
            Self::Book { at_ms, .. }
            | Self::Trade { at_ms, .. }
            | Self::TradeFeed { at_ms, .. } => at_ms,
        }
    }
}

struct Queue {
    activated_ms: u64,
    ahead: f64,
    changed_side: bool,
    cancel_requested: bool,
}

struct CancelledOrder {
    order: Resting,
    cancelled_ms: u64,
    expires_ms: u64,
}

#[derive(Default, Serialize)]
struct MakerFillQuality {
    fills: u64,
    turnover: f64,
    fees: f64,
    cancel_pending_fills: u64,
    total_queue_age_ms: u64,
    marked_fills_1s: u64,
    marked_turnover_1s: f64,
    markout_quote_1s: f64,
}

impl MakerFillQuality {
    fn fill(
        &mut self,
        quantity: f64,
        price: f64,
        fee_bps: f64,
        queue_age: u64,
        cancel_pending: bool,
    ) {
        self.fills += 1;
        self.turnover += quantity * price;
        self.fees += quantity * price * fee_bps / 10_000.0;
        self.cancel_pending_fills += u64::from(cancel_pending);
        self.total_queue_age_ms += queue_age;
    }

    fn mark(&mut self, fill: &PendingMarkout, mid: f64) {
        self.marked_fills_1s += 1;
        self.marked_turnover_1s += fill.signed_quantity.abs() * fill.price;
        self.markout_quote_1s += fill.signed_quantity * (mid - fill.price);
    }
}

struct PendingMarkout {
    due_ms: u64,
    signed_quantity: f64,
    price: f64,
    changed_side: bool,
}

#[derive(Default)]
struct ExecutionAttribution {
    maker_submitted: u64,
    maker_accepted: u64,
    maker_cancel_requests: u64,
    changed_side_maker_submitted: u64,
    changed_side_maker_accepted: u64,
    all_maker_fills: MakerFillQuality,
    changed_side_maker_fills: MakerFillQuality,
    pending_marks: VecDeque<PendingMarkout>,
    ioc_cost_quote_vs_submit_mid: f64,
    ioc_turnover_with_submit_mid: f64,
}

#[derive(Default)]
struct Market {
    book: Book,
    queue: BTreeMap<i64, Queue>,
    cancelled: BTreeMap<i64, CancelledOrder>,
    late_cancel_fills: u64,
    historical_trades: u64,
    pre_order_trades: u64,
    first_maker_ms: Option<u64>,
    funding_rejections: u64,
    warmup_rejections: u64,
    trade_feed_online: Option<bool>,
    trade_feed_gaps: u64,
    last_event_ms: u64,
    last_account_publication_ms: Option<u64>,
    last_mid: Option<f64>,
    post_only_rejections: u64,
    precision_rejections: u64,
    stale_rejections: u64,
    accepted_orders: u64,
    attribution: Option<ExecutionAttribution>,
}

pub(super) struct Matching {
    market: PlMutex<Market>,
    entry: Duration,
    cancel_transport: Duration,
    taker_transport: Duration,
    cancel_delay: Duration,
    taker_delay: Duration,
    response: Duration,
    clock_guard_ms: u64,
    start_ms: u64,
    trade_delivery_grace_ms: u64,
    flat_at_funding: Option<FlatAtFunding>,
    trade_from_ms: u64,
    max_age_ms: u64,
    step: f64,
    tick: f64,
    min_contracts: f64,
    min_notional: f64,
    participation: f64,
}

impl Matching {
    fn new(config: &RecordedMakerConfig) -> Self {
        Self {
            market: PlMutex::new(Market {
                last_event_ms: config.start_ms,
                attribution: config
                    .diagnose_cross_market
                    .then(ExecutionAttribution::default),
                ..Default::default()
            }),
            entry: Duration::from_millis(config.order_entry_ms),
            cancel_transport: Duration::from_millis(
                config.cancel_transport_ms.unwrap_or(config.order_entry_ms),
            ),
            taker_transport: Duration::from_millis(
                config.taker_transport_ms.unwrap_or(config.order_entry_ms),
            ),
            cancel_delay: Duration::from_millis(config.venue_cancel_delay_ms),
            taker_delay: Duration::from_millis(config.venue_taker_delay_ms),
            response: Duration::from_millis(config.order_response_ms),
            clock_guard_ms: config.trade_clock_guard_ms,
            start_ms: config.start_ms,
            trade_delivery_grace_ms: config.trade_delivery_grace_ms,
            flat_at_funding: config.flat_at_funding.clone(),
            trade_from_ms: config.trade_from_ms.unwrap_or(config.start_ms),
            max_age_ms: config.strategy.local_staleness_ms,
            step: config.strategy.step_size,
            tick: config.strategy.tick_size,
            min_contracts: config.min_contracts,
            min_notional: config.min_notional,
            participation: config.depth_participation,
        }
    }

    fn on_book(&self, book: Book) {
        let mut state = self.market.lock();
        if let Some((bid, ask)) = book.touch() {
            let mid = bid[0] * 0.5 + ask[0] * 0.5;
            state.last_mid = Some(mid);
            if let Some(stats) = &mut state.attribution {
                let now = now_ms();
                while let Some(fill) = stats.pending_marks.front() {
                    let expired = now > fill.due_ms + MARKOUT_MS;
                    let eligible = book.timestamp_ms >= fill.due_ms && book.timestamp_ms <= now;
                    if !expired && !eligible {
                        break;
                    }
                    let fill = stats.pending_marks.pop_front().expect("checked markout");
                    if eligible && !expired {
                        stats.all_maker_fills.mark(&fill, mid);
                        if fill.changed_side {
                            stats.changed_side_maker_fills.mark(&fill, mid);
                        }
                    }
                }
            }
        }
        // Never replenish IOC depth from a repeated publication.
        if book.timestamp_ms != state.book.timestamp_ms {
            state.book = book;
        }
    }

    pub(super) async fn wait_cancel(&self) {
        tokio::time::sleep(self.cancel_transport + self.cancel_delay).await;
    }

    pub(super) fn record_cancel_request(&self, order_index: Option<i64>) {
        let mut state = self.market.lock();
        let Market {
            queue, attribution, ..
        } = &mut *state;
        if let Some(stats) = attribution {
            for (&id, order) in queue.iter_mut() {
                if order_index.is_none_or(|index| id == index) && !order.cancel_requested {
                    order.cancel_requested = true;
                    stats.maker_cancel_requests += 1;
                }
            }
        }
    }

    fn batch_delay(&self, actions: &[BatchAction]) -> Duration {
        // Keep the recorded model's batch ordering and hold a mixed batch
        // until its slowest action is eligible to match.
        actions
            .iter()
            .map(|action| match action {
                BatchAction::Cancel(_) => self.cancel_transport + self.cancel_delay,
                BatchAction::Place(params) if params.order_type == OrderType::Ioc => {
                    self.taker_transport + self.taker_delay
                }
                BatchAction::Place(_) => self.entry,
            })
            .max()
            .unwrap_or(self.entry)
    }

    pub(super) fn forget(&self, order_index: i64) {
        let mut state = self.market.lock();
        state.queue.remove(&order_index);
        state.cancelled.remove(&order_index);
    }

    pub(super) fn cancelled(&self, order: Resting) {
        let mut state = self.market.lock();
        let id = order.exchange_order_id as i64;
        // IOC remainders have no maker queue and cannot match public prints.
        if !state.queue.contains_key(&id) {
            return;
        }
        let cancelled_ms = now_ms();
        state.cancelled.insert(
            id,
            CancelledOrder {
                order,
                cancelled_ms,
                expires_ms: cancelled_ms + self.trade_delivery_grace_ms + self.clock_guard_ms,
            },
        );
    }

    fn prune_cancelled(&self, at: u64) {
        let mut state = self.market.lock();
        let expired: Vec<_> = state
            .cancelled
            .iter()
            .filter_map(|(&id, order)| (at > order.expires_ms).then_some(id))
            .collect();
        for id in expired {
            state.cancelled.remove(&id);
            state.queue.remove(&id);
        }
    }

    pub(super) async fn wait_cancelled_prints(&self) -> Result<()> {
        loop {
            self.prune_cancelled(now_ms());
            let deadline = self
                .market
                .lock()
                .cancelled
                .values()
                .map(|order| order.expires_ms)
                .max();
            let Some(deadline) = deadline else {
                break;
            };
            // One millisecond beyond the bound lets a print exactly at the
            // deadline run before cleanup advances the tracker generation.
            tokio::time::sleep(Duration::from_millis(deadline.saturating_sub(now_ms()) + 1)).await;
        }
        // A snapshot can be fresh and newer than the cancel request, yet
        // predate the last allowed print. The account publisher enables this
        // barrier before drive starts; matching-only tests have no publisher.
        if self.market.lock().last_account_publication_ms.is_some() {
            let fence_ms = now_ms();
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if self
                        .market
                        .lock()
                        .last_account_publication_ms
                        .is_some_and(|at| at > fence_ms)
                    {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .context(
                "recorded cleanup lacks an account publication after the delivery watermark",
            )?;
        }
        Ok(())
    }

    fn admit(&self, order: Resting, ioc: bool, changed_side: bool) -> bool {
        let now = now_ms();
        let mut state = self.market.lock();
        let size = order.remaining;
        let price = order.price;
        let on_grid = |v: f64, step: f64| (v / step - (v / step).round()).abs() <= 1e-5;
        if !on_grid(size, self.step)
            || !on_grid(price, self.tick)
            || (!ioc
                && (size + self.step * 1e-6 < self.min_contracts
                    || size * price < self.min_notional))
        {
            state.precision_rejections += 1;
            return false;
        }
        if !state.book.fresh(now, self.max_age_ms) {
            state.stale_rejections += 1;
            return false;
        }
        if !ioc {
            let (bid, ask) = state.book.touch().expect("validated book");
            if (order.is_ask && price <= bid[0]) || (!order.is_ask && price >= ask[0]) {
                state.post_only_rejections += 1;
                return false;
            }
            let side = if order.is_ask {
                &state.book.asks
            } else {
                &state.book.bids
            };
            let ahead = side
                .iter()
                .find(|level| (level[0] - price).abs() < self.tick * 0.1)
                .map_or_else(
                    || {
                        // The queue behind the last visible level is unknown.
                        let worst = side.last().expect("validated book")[0];
                        if (order.is_ask && price > worst) || (!order.is_ask && price < worst) {
                            f64::INFINITY
                        } else {
                            0.0
                        }
                    },
                    |level| level[1],
                );
            state.queue.insert(
                order.exchange_order_id as i64,
                Queue {
                    activated_ms: now,
                    ahead,
                    changed_side,
                    cancel_requested: false,
                },
            );
            state.first_maker_ms.get_or_insert(now);
            if let Some(stats) = &mut state.attribution {
                stats.maker_accepted += 1;
                stats.changed_side_maker_accepted += u64::from(changed_side);
            }
        }
        state.accepted_orders += 1;
        true
    }

    fn on_trade(
        &self,
        venue: &SimVenue,
        at: u64,
        source: u64,
        price: f64,
        size: f64,
        taker_buy: bool,
    ) -> Result<()> {
        // Reconnect subscriptions may include recent history. Prints proved
        // older than this simulator cannot involve any simulated order.
        if source.saturating_add(self.clock_guard_ms) < self.start_ms {
            self.market.lock().historical_trades += 1;
            return Ok(());
        }
        if at > source.saturating_add(self.trade_delivery_grace_ms) {
            let mut state = self.market.lock();
            if state
                .first_maker_ms
                .is_none_or(|first| source.saturating_add(self.clock_guard_ms) < first)
            {
                // A reconnect print during initial warmup cannot fill an
                // order that did not yet exist. Keep this exclusion visible.
                state.pre_order_trades += 1;
                return Ok(());
            }
        }
        ensure!(
            at <= source.saturating_add(self.trade_delivery_grace_ms),
            "trade delivery grace exceeded: source-to-receipt evidence incomplete"
        );
        if source > at.saturating_add(self.clock_guard_ms) {
            anyhow::bail!("trade source time exceeds clock guard");
        }
        self.prune_cancelled(at);
        let eligible_ms = at.min(source.saturating_sub(self.clock_guard_ms));
        let mut orders: Vec<_> = venue.book.lock().values().copied().collect();
        orders.extend(
            self.market
                .lock()
                .cancelled
                .values()
                // Include the clock-uncertain overlap conservatively. A print
                // unambiguously after cancellation cannot fill the tombstone.
                .filter(|cancelled| {
                    source.saturating_sub(self.clock_guard_ms) <= cancelled.cancelled_ms
                })
                .map(|cancelled| cancelled.order),
        );
        orders.retain(|order| {
            order.is_ask == taker_buy
                && (if taker_buy {
                    price >= order.price - self.tick * 0.1
                } else {
                    price <= order.price + self.tick * 0.1
                })
        });
        orders.sort_unstable_by(|a, b| {
            let order = a.price.total_cmp(&b.price);
            (if taker_buy { order } else { order.reverse() })
                .then(a.exchange_order_id.cmp(&b.exchange_order_id))
        });
        let mut remaining = size;
        for order in orders {
            let id = order.exchange_order_id as i64;
            let (activated_ms, changed_side, cancel_requested) = {
                let mut state = self.market.lock();
                let Some(queue) = state.queue.get_mut(&id) else {
                    continue;
                };
                if queue.activated_ms >= eligible_ms {
                    continue;
                }
                // A through trade is evidence of execution priority; a touch
                // trade must first consume the displayed queue ahead.
                if (price - order.price).abs() < self.tick * 0.1 {
                    let ahead = queue.ahead.min(remaining);
                    queue.ahead -= ahead;
                    remaining -= ahead;
                } else {
                    queue.ahead = 0.0;
                }
                (
                    queue.activated_ms,
                    queue.changed_side,
                    queue.cancel_requested,
                )
            };
            let qty = components::round_down_to_step(order.remaining.min(remaining), self.step);
            if qty > 0.0 {
                if let Some(stats) = &mut self.market.lock().attribution {
                    let age = at - activated_ms;
                    stats.all_maker_fills.fill(
                        qty,
                        order.price,
                        venue.fee_bps,
                        age,
                        cancel_requested,
                    );
                    if changed_side {
                        stats.changed_side_maker_fills.fill(
                            qty,
                            order.price,
                            venue.fee_bps,
                            age,
                            cancel_requested,
                        );
                    }
                    stats.pending_marks.push_back(PendingMarkout {
                        due_ms: at + MARKOUT_MS,
                        signed_quantity: if order.is_ask { -qty } else { qty },
                        price: order.price,
                        changed_side,
                    });
                }
                venue.fill(order, order.price, qty, false);
                {
                    let mut state = self.market.lock();
                    if let Some(cancelled) = state.cancelled.get_mut(&id) {
                        cancelled.order.remaining = (order.remaining - qty).max(0.0);
                        state.late_cancel_fills += 1;
                    }
                }
                remaining = (remaining - qty).max(0.0);
                if qty >= order.remaining - self.step * 1e-6 {
                    self.forget(id);
                }
            }
            if remaining < self.step {
                break;
            }
        }
        Ok(())
    }

    fn ioc(&self, venue: &SimVenue, order: Resting, reduce_only: bool, submit_mid: Option<f64>) {
        let mut remaining = order.remaining;
        if reduce_only {
            let position = venue.net_position();
            remaining = remaining.min(if order.is_ask {
                position.max(0.0)
            } else {
                (-position).max(0.0)
            });
        }
        let id = order.exchange_order_id as i64;
        let mut state = self.market.lock();
        let Market {
            book, attribution, ..
        } = &mut *state;
        let levels = if order.is_ask {
            &mut book.bids
        } else {
            &mut book.asks
        };
        // The book stores remaining executable size after prior IOC fills.
        // Subtract consumed depth in original-display units.
        for level in levels {
            let limit_crosses = if order.is_ask {
                level[0] >= order.price
            } else {
                level[0] <= order.price
            };
            if !limit_crosses {
                break;
            }
            let qty = components::round_down_to_step(
                remaining.min(level[1] * self.participation),
                self.step,
            );
            if qty > 0.0 {
                let resting = venue.book.lock().get(&id).copied();
                if let Some(resting) = resting {
                    venue.fill(resting, level[0], qty, true);
                    if let Some(stats) = attribution
                        && let Some(mid) = submit_mid
                    {
                        let cost = if order.is_ask {
                            mid - level[0]
                        } else {
                            level[0] - mid
                        };
                        stats.ioc_cost_quote_vs_submit_mid += qty * cost;
                        stats.ioc_turnover_with_submit_mid += qty * level[0];
                    }
                    level[1] = (level[1] - qty / self.participation).max(0.0);
                    remaining = (remaining - qty).max(0.0);
                }
            }
            if remaining < self.step {
                break;
            }
        }
        drop(state);
        if venue.book.lock().contains_key(&id) {
            venue.cancel(id);
        }
    }

    pub(super) async fn execute(
        &self,
        venue: &SimVenue,
        actions: Vec<BatchAction>,
    ) -> Result<BatchResult> {
        let changed_sides = CHANGED_QUOTES.get();
        let submit_mid = {
            let state = self.market.lock();
            state
                .book
                .fresh(now_ms(), self.max_age_ms)
                .then_some(state.last_mid)
                .flatten()
        };
        for action in &actions {
            match action {
                BatchAction::Cancel(id) => self.record_cancel_request(Some(*id)),
                BatchAction::Place(params) if params.order_type != OrderType::Ioc => {
                    if let Some(stats) = &mut self.market.lock().attribution {
                        stats.maker_submitted += 1;
                        stats.changed_side_maker_submitted +=
                            u64::from(changed_sides[usize::from(params.side == Side::Sell)]);
                    }
                }
                BatchAction::Place(_) => {}
            }
        }
        let reservations: Vec<_> = actions
            .iter()
            .map(|action| match action {
                BatchAction::Place(params) => Some(venue.reserve(
                    params.side,
                    params.price,
                    params.size,
                    params.order_type == OrderType::Ioc,
                )),
                BatchAction::Cancel(_) => None,
            })
            .collect();
        tokio::time::sleep(self.batch_delay(&actions)).await;
        let mut places = Vec::new();
        for (action, reservation) in actions.into_iter().zip(reservations) {
            match action {
                BatchAction::Cancel(id) => venue.cancel(id),
                BatchAction::Place(params) => {
                    let order = reservation.expect("place reservation");
                    let ioc = params.order_type == OrderType::Ioc;
                    let allowed = self.flat_at_funding.as_ref().is_none_or(|schedule| {
                        schedule.allows_arrival(now_ms(), ioc && params.reduce_only)
                    });
                    if !allowed {
                        self.market.lock().funding_rejections += 1;
                    }
                    let warmed = now_ms() >= self.trade_from_ms || (ioc && params.reduce_only);
                    if !warmed {
                        self.market.lock().warmup_rejections += 1;
                    }
                    let online = self.flat_at_funding.is_none()
                        || self.market.lock().trade_feed_online == Some(true)
                        || (ioc && params.reduce_only);
                    if allowed
                        && warmed
                        && online
                        && self.admit(order, ioc, changed_sides[usize::from(order.is_ask)])
                    {
                        venue.activate(order);
                        if ioc {
                            self.ioc(venue, order, params.reduce_only, submit_mid);
                        }
                    } else {
                        venue.apply(ShmPrivateEventV2::order_rejected(
                            venue.next_sequence(),
                            LIGHTER,
                            venue.symbol,
                            order.client_order_id,
                            venue.event_time(),
                        ));
                    }
                    places.push(PlaceResult {
                        client_order_index: order.client_order_id,
                        side: params.side,
                        price: params.price,
                        size: params.size,
                    });
                }
            }
        }
        tokio::time::sleep(self.response).await;
        Ok(BatchResult {
            tx_hashes: vec![],
            place_results: places,
        })
    }

    pub(super) async fn flatten(&self, venue: &SimVenue, mid: f64) -> Result<()> {
        // Match Lighter's reduce-only close limit. The production strategy
        // cancels and confirms independently before requesting this close.
        let position = venue.net_position();
        if position == 0.0 {
            return Ok(());
        }
        let (side, price) = if position > 0.0 {
            (Side::Sell, (mid * 0.98 / self.tick).ceil() * self.tick)
        } else {
            (Side::Buy, (mid * 1.02 / self.tick).floor() * self.tick)
        };
        self.execute(
            venue,
            vec![BatchAction::Place(crate::exchange::OrderParams {
                side,
                price,
                size: position.abs(),
                order_type: OrderType::Ioc,
                reduce_only: true,
            })],
        )
        .await?;
        Ok(())
    }
}

#[derive(Serialize)]
pub struct RecordedMakerReport {
    start_ms: u64,
    end_ms: u64,
    active_until_ms: u64,
    initial_equity: f64,
    strategy_startups: u64,
    daily_loss_start_equity: Option<f64>,
    funding_mode: &'static str,
    funding_boundaries_verified: u64,
    trade_delivery_grace_ms: u64,
    late_cancel_fills: u64,
    historical_trades: u64,
    pre_order_trades: u64,
    funding_rejections: u64,
    warmup_rejections: u64,
    trade_feed_gaps: u64,
    trade_feed_continuity_verified: bool,
    book_frames: u64,
    tape_trades: u64,
    accepted_orders: u64,
    post_only_rejections: u64,
    precision_rejections: u64,
    stale_rejections: u64,
    maker_fills: u64,
    taker_fills: u64,
    maker_turnover: f64,
    taker_turnover: f64,
    maker_fees: f64,
    taker_fees: f64,
    net_liquidation_pnl: Option<f64>,
    final_position: f64,
    max_drawdown: f64,
    max_inventory_notional: f64,
    mean_inventory_notional: f64,
    resting_orders: usize,
    needs_reconciliation: bool,
    halted: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cross_market_attribution: Option<CrossMarketAttribution>,
    #[serde(skip_serializing_if = "Option::is_none")]
    quote_participation: Option<QuoteParticipation>,
}

#[derive(Serialize)]
struct CrossMarketAttribution {
    #[serde(flatten)]
    quote: QuoteAttribution,
    maker_submitted: u64,
    maker_accepted: u64,
    maker_cancel_requests: u64,
    changed_side_maker_submitted: u64,
    changed_side_maker_accepted: u64,
    all_maker_fills: MakerFillQuality,
    changed_side_maker_fills: MakerFillQuality,
    ioc_cost_quote_vs_submit_mid: f64,
    ioc_turnover_with_submit_mid: f64,
}

#[derive(Default)]
struct Risk {
    high: f64,
    max_drawdown: f64,
    max_inventory_notional: f64,
    last_inventory_sample_ns: Option<u64>,
    last_inventory_notional: f64,
    inventory_notional_ns: f64,
    inventory_sample_ns: u64,
}

fn account(config: &RecordedMakerConfig, venue: &SimVenue, shm: &mut ShmFiles, risk: &mut Risk) {
    let mid = venue
        .recorded
        .as_ref()
        .expect("recorded matching")
        .market
        .lock()
        .last_mid;
    let position = venue.net_position();
    let equity = config.initial_equity + venue.cash_flow() + mid.unwrap_or(0.0) * position;
    let now_ns = wall_time_ns().expect("recorded clock");
    let inventory_notional = mid.unwrap_or(0.0) * position.abs();
    risk.high = risk.high.max(equity);
    risk.max_drawdown = risk.max_drawdown.max(risk.high - equity);
    risk.max_inventory_notional = risk.max_inventory_notional.max(inventory_notional);
    if let Some(previous) = risk.last_inventory_sample_ns {
        let elapsed = now_ns - previous;
        risk.inventory_notional_ns += risk.last_inventory_notional * elapsed as f64;
        risk.inventory_sample_ns += elapsed;
    }
    risk.last_inventory_sample_ns = Some(now_ns);
    risk.last_inventory_notional = inventory_notional;
    let (_, low, high) = venue.tracker.exposure_bounds();
    let margin = low.abs().max(high.abs()) * mid.unwrap_or(0.0) / config.strategy.max_leverage;
    shm.publish_account_with_margin(equity, position, margin, now_ns);
    venue
        .recorded
        .as_ref()
        .expect("recorded matching")
        .market
        .lock()
        .last_account_publication_ms = Some(now_ns / NS_PER_MS);
}

async fn drive(
    config: RecordedMakerConfig,
    mut input: impl BufRead,
) -> Result<RecordedMakerReport> {
    let _clock = ReplayClock::open(config.start_ms, config.diagnose_cross_market);
    let start = Instant::now();
    let mut files = ShmFiles::for_symbol(config.strategy.symbol_id)?;
    let tracker = Arc::new(OrderTracker::new());
    let mut sim = SimVenue::for_symbol(
        Arc::clone(&tracker),
        config.strategy.symbol_id,
        config.strategy.maker_fee_bps,
        config.taker_fee_bps,
    );
    sim.recorded = Some(Matching::new(&config));
    let sim = Arc::new(sim);
    let matching = sim.recorded.as_ref().expect("recorded matching");
    let reader = ShmReader::open(
        files
            .path("matrix")
            .to_str()
            .context("replay matrix path")?,
        NUM_SYMBOLS,
    )?;
    let reader_account = AccountStatsReader::open(
        files
            .path("account")
            .to_str()
            .context("replay account path")?,
    )?;
    let mut risk = Risk {
        high: config.initial_equity,
        ..Default::default()
    };
    account(&config, &sim, &mut files, &mut risk);
    let files = RefCell::new(files);
    let risk = RefCell::new(risk);
    let mut strategy = InventoryNeutralMM::with_data_sources(
        config.strategy.clone(),
        Arc::clone(&sim) as Arc<dyn Exchange>,
        Arc::clone(&tracker),
        reader,
        reader_account,
        None,
    );
    strategy.quote_participation =
        config
            .hyperliquid_obi_filter
            .map(|hyperliquid_obi_filter| QuoteParticipation {
                hyperliquid_obi_filter,
                decisions: 0,
                fresh_reference_decisions: 0,
                withdrawn_bid_targets: 0,
                withdrawn_ask_targets: 0,
            });
    let (stop, stop_rx) = watch::channel(false);
    let (done, mut account_done) = watch::channel(false);
    let mut frames = 0;
    let mut trades = 0;
    let mut last = config.start_ms;
    let feed = async {
        let result: Result<()> = async {
            let mut line = String::new();
            let mut delayed_books = VecDeque::<(u64, Book, Book)>::new();
            let publish = |lighter: &Book, hyperliquid: &Book| {
                let mut shm = files.borrow_mut();
                lighter.write_bbo(&shm, LIGHTER);
                hyperliquid.write_bbo(&shm, EXCH_HYPERLIQUID);
                shm.publish_version();
            };
            loop {
                line.clear();
                if input.read_line(&mut line)? == 0 {
                    break;
                }
                ensure!(line.len() <= 64 * 1024, "maker event exceeds line limit");
                let event: Event =
                    serde_json::from_str(&line).context("invalid recorded maker event")?;
                let at = event.at_ms();
                ensure!(
                    at >= last && at < config.drain_until_ms,
                    "maker events outside ordered replay window"
                );
                if let Some(schedule) = &config.flat_at_funding {
                    ensure!(
                        at - last <= schedule.max_event_gap_ms,
                        "continuous replay event gap exceeds bound"
                    );
                }
                last = at;
                while delayed_books
                    .front()
                    .is_some_and(|(at_publish, _, _)| *at_publish <= at)
                {
                    let (at_publish, lighter, hyperliquid) =
                        delayed_books.pop_front().expect("checked delayed book");
                    tokio::time::sleep_until(
                        start + Duration::from_millis(at_publish - config.start_ms),
                    )
                    .await;
                    publish(&lighter, &hyperliquid);
                }
                tokio::time::sleep_until(start + Duration::from_millis(at - config.start_ms)).await;
                matching.market.lock().last_event_ms = at;
                match event {
                    Event::TradeFeed { online, .. } => {
                        let mut state = matching.market.lock();
                        if !online && state.trade_feed_online != Some(false) {
                            state.trade_feed_gaps += 1;
                        }
                        state.trade_feed_online = Some(online);
                    }
                    Event::Book {
                        lighter,
                        hyperliquid,
                        ..
                    } => {
                        lighter.validate(at)?;
                        hyperliquid.validate(at)?;
                        let at_publish = at + config.market_data_delay_ms;
                        if config.market_data_delay_ms == 0 {
                            publish(&lighter, &hyperliquid);
                        } else if at_publish < config.drain_until_ms {
                            delayed_books.push_back((at_publish, lighter.clone(), hyperliquid));
                        }
                        matching.on_book(lighter);
                        frames += 1;
                    }
                    Event::Trade {
                        source_ms,
                        price,
                        size,
                        aggressor,
                        ..
                    } => {
                        ensure!(
                            source_ms > 0 && valid_price(price) && size.is_finite() && size > 0.0,
                            "invalid recorded trade"
                        );
                        matching.on_trade(
                            &sim,
                            at,
                            source_ms,
                            price,
                            size,
                            matches!(aggressor, Aggressor::Buy),
                        )?;
                        trades += 1;
                    }
                }
            }
            for (at_publish, lighter, hyperliquid) in delayed_books {
                tokio::time::sleep_until(
                    start + Duration::from_millis(at_publish - config.start_ms),
                )
                .await;
                publish(&lighter, &hyperliquid);
            }
            if let Some(schedule) = &config.flat_at_funding {
                ensure!(
                    config.drain_until_ms - last <= schedule.max_event_gap_ms,
                    "continuous replay ends without event coverage"
                );
            }
            tokio::time::sleep_until(
                start + Duration::from_millis(config.drain_until_ms - config.start_ms),
            )
            .await;
            Ok(())
        }
        .await;
        stop.send_replace(true);
        result
    };
    let accounts = async {
        while !*account_done.borrow() {
            tokio::select! {
                _ = account_done.changed() => break,
                _ = tokio::time::sleep(Duration::from_millis(100)) => {
                    account(&config,&sim,&mut files.borrow_mut(),&mut risk.borrow_mut());
                }
            }
        }
    };
    let run = async {
        let result = if config.flat_at_funding.is_some() {
            continuous::run(&mut strategy, &config, &sim, stop_rx).await
        } else {
            (strategy.run(Some(stop_rx)).await, 0)
        };
        done.send_replace(true);
        result
    };
    let end_quotes = async {
        let mut done = done.subscribe();
        tokio::select! {
            _ = done.changed() => {}
            _ = tokio::time::sleep_until(start+Duration::from_millis(config.end_ms-config.start_ms)) => {
                stop.send_replace(true);
            }
        }
    };
    let (data, (), (result, funding_boundaries_verified), ()) =
        tokio::join!(feed, accounts, run, end_quotes);
    data?;
    account(
        &config,
        &sim,
        &mut files.borrow_mut(),
        &mut risk.borrow_mut(),
    );
    let ledger = *sim.ledger.lock();
    let mut market = matching.market.lock();
    let resting_orders = sim.book.lock().len();
    let final_position = sim.net_position();
    ensure!(
        (final_position - tracker.confirmed_position()).abs() < 0.5e-8,
        "simulated ledger disagrees with order tracker"
    );
    let needs_reconciliation = tracker.needs_reconciliation();
    let risk = risk.into_inner();
    let halted = result.err().map(|err| format!("{err:#}"));
    let active_until_ms = now_ms();
    let trade_feed_continuity_verified =
        market.trade_feed_online == Some(true) && market.trade_feed_gaps == 0;
    let funding_safe = if config.flat_at_funding.is_some() {
        halted.is_none() && trade_feed_continuity_verified
    } else {
        active_until_ms / HOUR_MS == config.start_ms / HOUR_MS
    };
    let cross_market_attribution =
        market
            .attribution
            .take()
            .map(|execution| CrossMarketAttribution {
                quote: ATTRIBUTION
                    .replace(None)
                    .expect("enabled quote attribution"),
                maker_submitted: execution.maker_submitted,
                maker_accepted: execution.maker_accepted,
                maker_cancel_requests: execution.maker_cancel_requests,
                changed_side_maker_submitted: execution.changed_side_maker_submitted,
                changed_side_maker_accepted: execution.changed_side_maker_accepted,
                all_maker_fills: execution.all_maker_fills,
                changed_side_maker_fills: execution.changed_side_maker_fills,
                ioc_cost_quote_vs_submit_mid: execution.ioc_cost_quote_vs_submit_mid,
                ioc_turnover_with_submit_mid: execution.ioc_turnover_with_submit_mid,
            });
    Ok(RecordedMakerReport {
        start_ms: config.start_ms,
        end_ms: config.end_ms,
        active_until_ms,
        initial_equity: config.initial_equity,
        strategy_startups: 1,
        daily_loss_start_equity: strategy
            .daily_loss
            .as_ref()
            .map(|guard| guard.start_equity()),
        funding_mode: if config.flat_at_funding.is_some() {
            "flat_at_funding"
        } else {
            "single_funding_hour"
        },
        funding_boundaries_verified,
        trade_delivery_grace_ms: config.trade_delivery_grace_ms,
        late_cancel_fills: market.late_cancel_fills,
        historical_trades: market.historical_trades,
        pre_order_trades: market.pre_order_trades,
        funding_rejections: market.funding_rejections,
        warmup_rejections: market.warmup_rejections,
        trade_feed_gaps: market.trade_feed_gaps,
        trade_feed_continuity_verified,
        book_frames: frames,
        tape_trades: trades,
        accepted_orders: market.accepted_orders,
        post_only_rejections: market.post_only_rejections,
        precision_rejections: market.precision_rejections,
        stale_rejections: market.stale_rejections,
        maker_fills: ledger.maker_fills,
        taker_fills: ledger.taker_fills,
        maker_turnover: ledger.maker_notional,
        taker_turnover: ledger.taker_notional,
        maker_fees: ledger.maker_fees,
        taker_fees: ledger.taker_fees,
        net_liquidation_pnl: (final_position == 0.0
            && resting_orders == 0
            && !needs_reconciliation
            && funding_safe)
            .then_some(ledger.cash),
        final_position,
        max_drawdown: risk.max_drawdown,
        max_inventory_notional: risk.max_inventory_notional,
        mean_inventory_notional: risk.inventory_notional_ns
            / risk.inventory_sample_ns.max(1) as f64,
        resting_orders,
        needs_reconciliation,
        halted,
        cross_market_attribution,
        quote_participation: strategy.quote_participation,
    })
}

/// Run on an isolated, paused, single-threaded runtime. The inputs are public
/// events in availability order and a separate execution configuration.
pub fn run_recorded_maker(
    config: RecordedMakerConfig,
    input: impl BufRead,
) -> Result<RecordedMakerReport> {
    config.validate()?;
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .start_paused(true)
        .build()?
        .block_on(drive(config, input))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exchange::OrderParams;

    const START: u64 = 1_800_000_000_000;

    fn config() -> RecordedMakerConfig {
        RecordedMakerConfig {
            strategy: InventoryNeutralMMConfig {
                step_size: 0.001,
                ..Default::default()
            },
            start_ms: START,
            end_ms: START + 1_000,
            drain_until_ms: START + 2_000,
            initial_equity: 1_000.0,
            order_entry_ms: 250,
            venue_cancel_delay_ms: 300,
            venue_taker_delay_ms: 140,
            market_data_delay_ms: 0,
            cancel_transport_ms: None,
            taker_transport_ms: None,
            diagnose_cross_market: false,
            hyperliquid_obi_filter: None,
            order_response_ms: 0,
            trade_clock_guard_ms: 100,
            trade_delivery_grace_ms: 2_000,
            flat_at_funding: None,
            trade_from_ms: None,
            taker_fee_bps: 4.0,
            min_contracts: 0.001,
            min_notional: 1.0,
            depth_participation: 0.25,
        }
    }

    fn book() -> Book {
        Book {
            timestamp_ms: now_ms(),
            bids: vec![[99.0, 1.0]],
            asks: vec![[101.0, 1.0]],
        }
    }

    fn maker() -> BatchAction {
        BatchAction::Place(OrderParams {
            side: Side::Buy,
            price: 100.0,
            size: 0.05,
            order_type: OrderType::PostOnly,
            reduce_only: false,
        })
    }

    #[test]
    fn obi_filter_uses_only_fresh_two_sided_depth_and_preserves_the_exit_side() {
        let filter = HyperliquidObiFilter {
            threshold: 0.6,
            max_age_ms: 250,
        };
        let at = START * NS_PER_MS;
        let mut book = ShmBboMessage {
            timestamp_ns: at,
            bid_price: 1999.0,
            ask_price: 2000.0,
            bid_size: 1.0,
            ask_size: 5.0,
            ..Default::default()
        };
        assert_eq!(
            filter.blocked_sides(Some(book), at, [0.0, 0.0]),
            Some([true, false])
        );
        assert_eq!(
            filter.blocked_sides(Some(book), at, [1.0, 0.0]),
            Some([true, false])
        );
        assert_eq!(
            filter.blocked_sides(Some(book), at, [1.0, -1.0]),
            Some([false, false])
        );
        std::mem::swap(&mut book.bid_size, &mut book.ask_size);
        assert_eq!(
            filter.blocked_sides(Some(book), at, [0.0, 0.0]),
            Some([false, true])
        );
        assert_eq!(
            filter.blocked_sides(Some(book), at, [0.0, -1.0]),
            Some([false, true])
        );
        assert_eq!(
            filter.blocked_sides(Some(book), at, [1.0, -1.0]),
            Some([false, false])
        );
        assert_eq!(filter.blocked_sides(Some(book), at - 1, [0.0, 0.0]), None);
        assert_eq!(
            filter.blocked_sides(Some(book), at + 251 * NS_PER_MS, [0.0, 0.0]),
            None
        );
        book.ask_size = 0.0;
        assert_eq!(filter.blocked_sides(Some(book), at, [0.0, 0.0]), None);
    }

    #[tokio::test(start_paused = true)]
    async fn obi_filter_zero_target_withdraws_resting_order_without_suppressing_exit() {
        let _clock = ReplayClock::open(START, false);
        let mut filter = QuoteParticipation {
            hyperliquid_obi_filter: HyperliquidObiFilter {
                threshold: 0.6,
                max_age_ms: 250,
            },
            decisions: 0,
            fresh_reference_decisions: 0,
            withdrawn_bid_targets: 0,
            withdrawn_ask_targets: 0,
        };
        let mut target = QuoteTarget {
            bid_price: 1999.0,
            ask_price: 2001.0,
            bid_size: 0.05,
            ask_size: 0.04,
        };
        let book = ShmBboMessage {
            timestamp_ns: START * NS_PER_MS,
            bid_price: 1999.0,
            ask_price: 2000.0,
            bid_size: 1.0,
            ask_size: 5.0,
            ..Default::default()
        };
        let config = config().strategy;
        filter.apply(&mut target, Some(book), [0.04, 0.04], &config);
        assert_eq!((target.bid_size, target.ask_size), (0.0, 0.04));
        assert_eq!(
            (filter.withdrawn_bid_targets, filter.withdrawn_ask_targets),
            (1, 0)
        );
        let bid_plan = build_side_execution_plan(
            &config,
            &[ActiveOrder {
                client_order_id: 1,
                order_index: Some(42),
                lifecycle: OrderLifecycle::Open,
                side: OrderSide::Buy,
                price: 1999.0,
                size: 0.05,
                placed_at: Instant::now(),
            }],
            OrderType::PostOnly,
            Side::Buy,
            target.bid_price,
            target.bid_size,
            0.0,
            0.0,
            1,
        );
        assert_eq!(bid_plan.to_cancel, [42]);
        assert!(bid_plan.to_place.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn cancelled_order_matches_late_pre_cancel_prints_without_reviving_reservations() {
        let _clock = ReplayClock::open(START, false);
        let tracker = Arc::new(OrderTracker::new());
        let mut venue = SimVenue::new(Arc::clone(&tracker), 1.0);
        venue.recorded = Some(Matching::new(&config()));
        let matching = venue.recorded.as_ref().unwrap();
        matching.on_book(book());
        venue.execute_batch(vec![maker(), maker()]).await.unwrap();
        let cancelled_id = *venue.book.lock().keys().next().unwrap();
        venue.cancel_order(cancelled_id).await.unwrap();
        let source = now_ms() - 150;
        tokio::time::advance(Duration::from_millis(250)).await;
        matching
            .on_trade(&venue, now_ms(), source, 100.0, 0.075, false)
            .unwrap();
        assert_eq!(venue.net_position(), 0.075);
        assert_eq!(tracker.confirmed_position(), 0.075);
        assert!(!venue.book.lock().contains_key(&cancelled_id));
        assert_eq!(venue.book.lock().values().next().unwrap().remaining, 0.025);
        assert_eq!(matching.market.lock().late_cancel_fills, 1);
        assert_eq!(venue.ledger.lock().maker_fills, 2);
        assert!((tracker.exposure_bounds().2 - 0.1).abs() < 1e-8);
        assert!(!tracker.needs_reconciliation());
    }

    #[tokio::test(start_paused = true)]
    async fn cancelled_partial_fills_keep_remaining_volume_and_cleanup_waits_for_delivery() {
        let _clock = ReplayClock::open(START, false);
        let tracker = Arc::new(OrderTracker::new());
        let mut venue = SimVenue::new(Arc::clone(&tracker), 0.0);
        venue.recorded = Some(Matching::new(&config()));
        let matching = venue.recorded.as_ref().unwrap();
        matching.on_book(book());
        venue.execute_batch(vec![maker()]).await.unwrap();
        venue.cancel_all().await.unwrap();
        let cancelled_at = now_ms();
        let fill = async {
            tokio::time::sleep(Duration::from_millis(250)).await;
            matching
                .on_trade(&venue, now_ms(), cancelled_at - 150, 100.0, 0.02, false)
                .unwrap();
            matching
                .on_trade(&venue, now_ms(), cancelled_at - 140, 100.0, 0.03, false)
                .unwrap();
        };
        let (orders, ()) = tokio::join!(venue.get_active_orders(), fill);
        assert!(orders.unwrap().is_empty());
        assert_eq!(venue.net_position(), 0.05);
        assert_eq!(tracker.confirmed_position(), 0.05);
        assert!(matching.market.lock().cancelled.is_empty());
        assert_eq!(matching.market.lock().late_cancel_fills, 2);
    }

    #[tokio::test(start_paused = true)]
    async fn after_cancel_prints_do_not_fill_and_expired_delivery_invalidates_replay() {
        let _clock = ReplayClock::open(START, false);
        let tracker = Arc::new(OrderTracker::new());
        let mut venue = SimVenue::new(tracker, 0.0);
        venue.recorded = Some(Matching::new(&config()));
        let matching = venue.recorded.as_ref().unwrap();
        matching.on_book(book());
        venue.execute_batch(vec![maker()]).await.unwrap();
        venue.cancel_all().await.unwrap();
        let cancelled_at = now_ms();
        tokio::time::advance(Duration::from_millis(101)).await;
        matching
            .on_trade(&venue, now_ms(), now_ms(), 100.0, 0.05, false)
            .unwrap();
        assert_eq!(venue.net_position(), 0.0);
        matching.wait_cancelled_prints().await.unwrap();
        assert!(matching.market.lock().cancelled.is_empty());
        assert!(
            matching
                .on_trade(&venue, now_ms(), cancelled_at - 150, 100.0, 0.05, false)
                .unwrap_err()
                .to_string()
                .contains("delivery grace exceeded")
        );
        // Reconnect history that predates every possible simulated order is
        // counted explicitly; it must neither fill nor invalidate a run.
        matching
            .on_trade(&venue, now_ms(), START - 60_000, 100.0, 0.05, false)
            .unwrap();
        assert_eq!(matching.market.lock().historical_trades, 1);
        assert!(
            matching
                .on_trade(&venue, now_ms(), now_ms() + 101, 100.0, 0.05, false)
                .is_err()
        );
    }

    fn continuous_config(seconds: u64) -> RecordedMakerConfig {
        let mut value = config();
        value.end_ms = START + seconds * 1_000;
        value.drain_until_ms = value.end_ms + 15_000;
        value.strategy.use_post_only = true;
        value.strategy.use_depth_pricing = false;
        value.strategy.poll_interval_ms = 1_000;
        value.flat_at_funding = Some(FlatAtFunding {
            interval_ms: 60_000,
            offset_ms: 0,
            drain_before_ms: 10_000,
            guard_ms: 1_000,
            max_event_gap_ms: 2_000,
        });
        value
    }

    fn tape(config: &RecordedMakerConfig, feed: bool) -> String {
        let mut data = String::new();
        if feed {
            data.push_str(&format!(
                "{{\"kind\":\"trade_feed\",\"at_ms\":{},\"online\":true}}\n",
                config.start_ms
            ));
        }
        for at in (config.start_ms..config.drain_until_ms).step_by(1_000) {
            let book =
                serde_json::json!({"timestamp_ms":at,"bids":[[99.0,1.0]],"asks":[[101.0,1.0]]});
            let event =
                serde_json::json!({"kind":"book","at_ms":at,"lighter":book,"hyperliquid":book});
            data.push_str(&event.to_string());
            data.push('\n');
        }
        data
    }

    #[test]
    fn continuous_driver_keeps_one_strategy_and_proves_each_flat_settlement() {
        let config = continuous_config(125);
        let data = tape(&config, true);
        let report = run_recorded_maker(config, data.as_bytes()).unwrap();
        assert!(report.halted.is_none(), "{:?}", report.halted);
        assert_eq!(report.funding_boundaries_verified, 2);
        assert_eq!(report.strategy_startups, 1);
        assert_eq!(report.daily_loss_start_equity, Some(1_000.0));
        assert_eq!(report.net_liquidation_pnl, Some(0.0));
        assert_eq!(report.final_position, 0.0);
        assert_eq!(report.active_until_ms, START + 140_000);
    }

    #[test]
    fn continuous_driver_cannot_certify_missing_trade_feed_or_book_gaps() {
        let config = continuous_config(125);
        let data = tape(&config, false);
        let report = run_recorded_maker(config, data.as_bytes()).unwrap();
        assert!(
            report
                .halted
                .as_ref()
                .unwrap()
                .contains("trade-feed evidence")
        );
        assert_eq!(report.net_liquidation_pnl, None);
        assert!(!report.trade_feed_continuity_verified);
        let config = continuous_config(125);
        let data = tape(&config, true);
        let data = data
            .lines()
            .filter(|line| {
                let event: serde_json::Value = serde_json::from_str(line).unwrap();
                let at = event["at_ms"].as_u64().unwrap();
                !(START + 10_000..START + 14_000).contains(&at)
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            run_recorded_maker(config, data.as_bytes())
                .err()
                .unwrap()
                .to_string()
                .contains("event gap")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn actual_arrival_rejects_new_risk_after_drain_boundary_and_during_warmup() {
        let _clock = ReplayClock::open(START + 48_800, false);
        let mut config = continuous_config(125);
        config.trade_from_ms = Some(START + 48_900);
        let tracker = Arc::new(OrderTracker::new());
        let mut venue = SimVenue::new(tracker, 0.0);
        venue.recorded = Some(Matching::new(&config));
        let matching = venue.recorded.as_ref().unwrap();
        matching.market.lock().trade_feed_online = Some(true);
        matching.on_book(book());
        // Submission precedes the 49s drain boundary; arrival is after it.
        venue.execute_batch(vec![maker()]).await.unwrap();
        assert!(venue.book.lock().is_empty());
        assert_eq!(matching.market.lock().funding_rejections, 1);
        assert!(venue.tracker.active_orders_snapshot().is_empty());
        config.flat_at_funding = None;
        config.trade_from_ms = Some(now_ms() + 1_000);
        venue.recorded = Some(Matching::new(&config));
        let matching = venue.recorded.as_ref().unwrap();
        matching.on_book(book());
        venue.execute_batch(vec![maker()]).await.unwrap();
        assert!(venue.book.lock().is_empty());
        assert_eq!(matching.market.lock().warmup_rejections, 1);
    }

    #[test]
    fn long_replay_requires_explicit_bounded_funding_mode() {
        let mut config = continuous_config(36_000);
        config.flat_at_funding.as_mut().unwrap().interval_ms = HOUR_MS;
        config.validate().unwrap();
        config.flat_at_funding.as_mut().unwrap().guard_ms = u64::MAX;
        assert!(config.validate().is_err());
        config.flat_at_funding = None;
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("funding hour")
        );
    }

    #[test]
    fn slow_batch_cannot_skip_a_narrow_funding_guard_and_report_valid_profit() {
        let mut config = continuous_config(125);
        config.order_entry_ms = 2_000;
        config.order_response_ms = 2_000;
        config.venue_cancel_delay_ms = 2_000;
        config.strategy.order_ttl_secs = 1;
        config.trade_delivery_grace_ms = 100;
        config.trade_clock_guard_ms = 0;
        let schedule = config.flat_at_funding.as_mut().unwrap();
        schedule.guard_ms = 1;
        schedule.drain_before_ms = 1_000;
        let data = tape(&config, true);
        let report = run_recorded_maker(config, data.as_bytes()).unwrap();
        assert!(
            report
                .halted
                .as_ref()
                .is_some_and(|error| error.contains("funding")),
            "{:?}",
            report.halted
        );
        assert_eq!(report.net_liquidation_pnl, None);
    }

    #[test]
    fn narrow_guards_wake_at_the_exact_settlement_timestamp() {
        let mut config = continuous_config(125);
        config.trade_clock_guard_ms = 0;
        config.flat_at_funding.as_mut().unwrap().guard_ms = 1;
        let data = tape(&config, true);
        let report = run_recorded_maker(config, data.as_bytes()).unwrap();
        assert!(report.halted.is_none(), "{:?}", report.halted);
        assert_eq!(report.funding_boundaries_verified, 2);
    }

    #[test]
    fn ten_hours_are_one_ledger_with_ten_verified_hourly_slots() {
        let mut config = continuous_config(36_000);
        config.flat_at_funding.as_mut().unwrap().interval_ms = HOUR_MS;
        config.trade_from_ms = Some(START + 60_000);
        let data = tape(&config, true);
        let report = run_recorded_maker(config, data.as_bytes()).unwrap();
        assert!(report.halted.is_none(), "{:?}", report.halted);
        assert_eq!(report.strategy_startups, 1);
        assert_eq!(report.funding_boundaries_verified, 10);
        assert_eq!(report.daily_loss_start_equity, Some(1_000.0));
        assert_eq!(report.net_liquidation_pnl, Some(0.0));
        assert_eq!(report.active_until_ms, START + 36_015_000);
        assert!(report.warmup_rejections > 0);
    }

    fn with_feed_gap(data: String, off: u64, on: u64) -> String {
        let mut events: Vec<serde_json::Value> = data
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        events.push(serde_json::json!({"kind":"trade_feed","at_ms":off,"online":false}));
        events.push(serde_json::json!({"kind":"trade_feed","at_ms":on,"online":true}));
        events.sort_by_key(|event| event["at_ms"].as_u64().unwrap());
        events
            .into_iter()
            .map(|event| event.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn reconnect_preserves_gap_evidence_and_guard_disconnect_cannot_be_certified() {
        let config = continuous_config(125);
        let data = with_feed_gap(tape(&config, true), START + 20_000, START + 21_000);
        let report = run_recorded_maker(config, data.as_bytes()).unwrap();
        assert!(report.halted.is_none(), "{:?}", report.halted);
        assert_eq!(report.trade_feed_gaps, 1);
        assert!(!report.trade_feed_continuity_verified);
        assert_eq!(report.net_liquidation_pnl, None);
        let config = continuous_config(125);
        let data = with_feed_gap(tape(&config, true), START + 60_200, START + 62_000);
        let report = run_recorded_maker(config, data.as_bytes()).unwrap();
        assert!(
            report
                .halted
                .as_ref()
                .unwrap()
                .contains("trade-feed evidence")
        );
        assert_eq!(report.net_liquidation_pnl, None);
    }

    #[test]
    fn receipt_activity_does_not_make_stale_guard_books_fresh() {
        let config = continuous_config(125);
        let data = tape(&config, true)
            .lines()
            .map(|line| {
                let mut event: serde_json::Value = serde_json::from_str(line).unwrap();
                if event["kind"] == "book" && event["at_ms"].as_u64().unwrap() >= START + 55_000 {
                    event["lighter"]["timestamp_ms"] = (START + 55_000).into();
                }
                event.to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");
        let report = run_recorded_maker(config, data.as_bytes()).unwrap();
        assert!(report.halted.as_ref().unwrap().contains("fresh book"));
        assert_eq!(report.net_liquidation_pnl, None);
    }

    #[tokio::test(start_paused = true)]
    async fn funding_cleanup_cannot_reset_the_daily_loss_budget() {
        let mut config = continuous_config(125);
        config.strategy.max_daily_loss_pct = 0.03;
        let _clock = ReplayClock::open(START, false);
        let mut files = ShmFiles::for_symbol(config.strategy.symbol_id).unwrap();
        let tracker = Arc::new(OrderTracker::new());
        let mut venue = SimVenue::new(Arc::clone(&tracker), 0.0);
        venue.recorded = Some(Matching::new(&config));
        let venue = Arc::new(venue);
        let matching = venue.recorded.as_ref().unwrap();
        let reader = ShmReader::open(files.path("matrix").to_str().unwrap(), NUM_SYMBOLS).unwrap();
        let account_reader =
            AccountStatsReader::open(files.path("account").to_str().unwrap()).unwrap();
        let mut risk = Risk {
            high: config.initial_equity,
            ..Default::default()
        };
        account(&config, &venue, &mut files, &mut risk);
        let mut strategy = InventoryNeutralMM::with_data_sources(
            config.strategy.clone(),
            Arc::clone(&venue) as Arc<dyn Exchange>,
            tracker,
            reader,
            account_reader,
            None,
        );
        let (_stop, stop_rx) = watch::channel(false);
        let publish = async {
            for millis in (0..140_000).step_by(100) {
                if millis == 10_000 || millis == 70_000 {
                    // Synthetic cash losses on either side of settlement.
                    // Each is below the cap; together they breach it.
                    venue.ledger.lock().cash -= if millis == 10_000 { 20.0 } else { 15.0 };
                }
                let book = book();
                book.write_bbo(&files, LIGHTER);
                book.write_bbo(&files, EXCH_HYPERLIQUID);
                files.publish_version();
                matching.on_book(book);
                {
                    let mut state = matching.market.lock();
                    state.trade_feed_online = Some(true);
                    state.last_event_ms = now_ms();
                }
                account(&config, &venue, &mut files, &mut risk);
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        };
        let ((result, verified), ()) = tokio::join!(
            continuous::run(&mut strategy, &config, &venue, stop_rx),
            publish
        );
        assert_eq!(verified, 1);
        assert!(format!("{:#}", result.unwrap_err()).contains("daily loss limit breached"));
        assert_eq!(
            strategy.daily_loss.as_ref().unwrap().start_equity(),
            1_000.0
        );
        assert_eq!(venue.cash_flow(), -35.0);
    }

    #[tokio::test(start_paused = true)]
    async fn cleanup_waits_for_account_published_after_the_last_allowed_cancel_fill() {
        let mut config = config();
        config.trade_clock_guard_ms = 0;
        let _clock = ReplayClock::open(START, false);
        let mut files = ShmFiles::for_symbol(config.strategy.symbol_id).unwrap();
        let tracker = Arc::new(OrderTracker::new());
        let mut venue = SimVenue::new(Arc::clone(&tracker), 0.0);
        venue.recorded = Some(Matching::new(&config));
        let venue = Arc::new(venue);
        let matching = venue.recorded.as_ref().unwrap();
        let reader = ShmReader::open(files.path("matrix").to_str().unwrap(), NUM_SYMBOLS).unwrap();
        let account_reader =
            AccountStatsReader::open(files.path("account").to_str().unwrap()).unwrap();
        let mut risk = Risk {
            high: config.initial_equity,
            ..Default::default()
        };
        account(&config, &venue, &mut files, &mut risk);
        let mut strategy = InventoryNeutralMM::with_data_sources(
            config.strategy.clone(),
            Arc::clone(&venue) as Arc<dyn Exchange>,
            Arc::clone(&tracker),
            reader,
            account_reader,
            None,
        );
        matching.on_book(book());
        venue.execute_batch(vec![maker()]).await.unwrap();
        venue.cancel_all().await.unwrap();
        let cancelled_at = now_ms();
        let expires_at = cancelled_at + config.trade_delivery_grace_ms;
        let publish = async {
            tokio::time::sleep(Duration::from_millis(expires_at - now_ms() - 1)).await;
            account(&config, &venue, &mut files, &mut risk); // zero, but one ms before fill
            tokio::time::sleep(Duration::from_millis(1)).await;
            matching
                .on_trade(&venue, now_ms(), cancelled_at, 100.0, 0.05, false)
                .unwrap();
            assert_eq!(venue.net_position(), 0.05);
            assert_eq!(tracker.confirmed_position(), 0.05);
            for _ in 0..70 {
                tokio::time::sleep(Duration::from_millis(100)).await;
                let book = book();
                book.write_bbo(&files, LIGHTER);
                book.write_bbo(&files, EXCH_HYPERLIQUID);
                files.publish_version();
                matching.on_book(book);
                account(&config, &venue, &mut files, &mut risk);
            }
        };
        let (cleaned, ()) = tokio::join!(
            strategy.clean_exchange_state("late delivery regression"),
            publish
        );
        cleaned.unwrap();
        assert_eq!(venue.net_position(), 0.0);
        assert_eq!(tracker.confirmed_position(), 0.0);
        assert_eq!(venue.ledger.lock().maker_fills, 1);
        assert_eq!(venue.ledger.lock().taker_fills, 1);
        assert!(!tracker.needs_reconciliation());
    }

    #[tokio::test(start_paused = true)]
    async fn tape_and_iocs_spend_observed_volume_once() {
        let _clock = ReplayClock::open(START, false);
        let config = config();
        let tracker = Arc::new(OrderTracker::new());
        let mut venue = SimVenue::new(Arc::clone(&tracker), 0.0);
        venue.recorded = Some(Matching::new(&config));
        let matching = venue.recorded.as_ref().unwrap();
        let book = || Book {
            timestamp_ms: START,
            bids: vec![[99.0, 0.02]],
            asks: vec![[101.0, 1.0]],
        };
        matching.on_book(book());
        let maker = || {
            BatchAction::Place(OrderParams {
                side: Side::Buy,
                price: 100.0,
                size: 0.05,
                order_type: OrderType::PostOnly,
                reduce_only: false,
            })
        };
        assert_eq!(
            matching.batch_delay(&[BatchAction::Cancel(1), maker()]),
            Duration::from_millis(550)
        );
        venue.execute_batch(vec![maker(), maker()]).await.unwrap();
        matching
            .on_trade(&venue, now_ms(), START + 200, 100.0, 1.0, false)
            .unwrap();
        assert_eq!(venue.net_position(), 0.0);
        tokio::time::advance(Duration::from_millis(200)).await;
        matching
            .on_trade(&venue, now_ms(), now_ms(), 100.0, 0.075, false)
            .unwrap();
        assert_eq!(venue.net_position(), 0.075);
        assert_eq!(tracker.confirmed_position(), 0.075);

        let close = || {
            BatchAction::Place(OrderParams {
                side: Side::Sell,
                price: 99.0,
                size: 0.075,
                order_type: OrderType::Ioc,
                reduce_only: true,
            })
        };
        let sent_at = now_ms();
        venue.execute_batch(vec![close()]).await.unwrap();
        assert!(now_ms() - sent_at >= 390);
        // A sampled frame can repeat the same publication. It cannot refill
        // the venue depth already consumed by an earlier simulated IOC.
        matching.on_book(book());
        venue.execute_batch(vec![close()]).await.unwrap();
        assert_eq!(venue.net_position(), 0.070);
        assert_eq!(tracker.confirmed_position(), 0.070);
        assert_eq!(venue.ledger.lock().taker_fills, 1);
        let sent_at = now_ms();
        assert_eq!(venue.cancel_all().await.unwrap(), 1);
        assert!(now_ms() - sent_at >= 550);
    }

    #[tokio::test(start_paused = true)]
    async fn attributed_quote_retains_tag_and_counts_fill_during_cancel_delay() {
        let _clock = ReplayClock::open(START, true);
        let mut config = config();
        config.diagnose_cross_market = true;
        config.cancel_transport_ms = Some(400);
        config.taker_transport_ms = Some(50);
        let tracker = Arc::new(OrderTracker::new());
        let mut venue = SimVenue::new(tracker, 0.0);
        venue.recorded = Some(Matching::new(&config));
        let matching = venue.recorded.as_ref().unwrap();
        matching.on_book(Book {
            timestamp_ms: START,
            bids: vec![[99.0, 1.0]],
            asks: vec![[101.0, 1.0]],
        });
        let maker = BatchAction::Place(OrderParams {
            side: Side::Buy,
            price: 100.0,
            size: 0.05,
            order_type: OrderType::PostOnly,
            reduce_only: false,
        });
        let ioc = BatchAction::Place(OrderParams {
            side: Side::Sell,
            price: 99.0,
            size: 0.05,
            order_type: OrderType::Ioc,
            reduce_only: true,
        });
        assert_eq!(
            matching
                .batch_delay(std::slice::from_ref(&maker))
                .as_millis(),
            250
        );
        assert_eq!(
            matching.batch_delay(std::slice::from_ref(&ioc)).as_millis(),
            190
        );
        assert_eq!(
            matching
                .batch_delay(&[BatchAction::Cancel(1), maker.clone()])
                .as_millis(),
            700
        );
        {
            let _changed = ChangedQuotes::open([true, false]);
            venue.execute_batch(vec![maker]).await.unwrap();
        }
        let cancel = venue.cancel_all();
        let fill = async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            matching
                .on_trade(&venue, now_ms(), now_ms(), 100.0, 0.05, false)
                .unwrap();
        };
        let (result, ()) = tokio::join!(cancel, fill);
        assert_eq!(result.unwrap(), 0);
        tokio::time::sleep(Duration::from_millis(500)).await;
        matching.on_book(Book {
            timestamp_ms: now_ms(),
            bids: vec![[99.0, 1.0]],
            asks: vec![[100.0, 1.0]],
        });
        venue.execute_batch(vec![ioc]).await.unwrap();
        assert_eq!(venue.net_position(), 0.0);
        let market = matching.market.lock();
        let stats = market.attribution.as_ref().unwrap();
        assert_eq!(stats.changed_side_maker_submitted, 1);
        assert_eq!(stats.changed_side_maker_accepted, 1);
        assert_eq!(stats.maker_cancel_requests, 1);
        let marked = &stats.changed_side_maker_fills;
        assert_eq!(marked.fills, 1);
        assert_eq!(marked.cancel_pending_fills, 1);
        assert_eq!(marked.marked_fills_1s, 1);
        assert_eq!(marked.markout_quote_1s, -0.025);
        assert_eq!(stats.ioc_cost_quote_vs_submit_mid, 0.025);
    }
}
