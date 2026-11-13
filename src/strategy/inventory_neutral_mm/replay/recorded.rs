//! Replay public depth and trades through the production maker and OrderTracker.
use super::*;
use crate::config::EXCH_HYPERLIQUID;
use anyhow::{Result, ensure};
use chrono::{DateTime, Datelike};
use serde::{Deserialize, Serialize};
use std::cell::{Cell, RefCell};
use std::io::BufRead;
use tokio::sync::watch;

const HOUR_MS: u64 = 3_600_000;
const NS_PER_MS: u64 = 1_000_000;

thread_local! {
    static CLOCK: Cell<Option<(u64, Instant)>> = const { Cell::new(None) };
}

struct ReplayClock;

impl ReplayClock {
    fn open(start_ms: u64) -> Self {
        assert!(
            CLOCK
                .replace(Some((start_ms * NS_PER_MS, Instant::now())))
                .is_none()
        );
        Self
    }
}

impl Drop for ReplayClock {
    fn drop(&mut self) {
        CLOCK.set(None);
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
    order_response_ms: u64,
    trade_clock_guard_ms: u64,
    taker_fee_bps: f64,
    min_contracts: f64,
    min_notional: f64,
    depth_participation: f64,
}

impl RecordedMakerConfig {
    fn validate(&self) -> Result<()> {
        self.strategy.validate()?;
        ensure!(
            (1_600_000_000_000..2_300_000_000_000).contains(&self.start_ms)
                && self.end_ms > self.start_ms
                && self.drain_until_ms > self.end_ms
                && self.drain_until_ms - self.end_ms <= 20_000
                && self.start_ms / HOUR_MS == self.drain_until_ms / HOUR_MS,
            "maker replay must fit in one UTC funding hour"
        );
        ensure!(
            (0.0..=100_000_000.0).contains(&self.initial_equity)
                && self.initial_equity > 0.0
                && (1..=2_000).contains(&self.order_entry_ms)
                && self.venue_cancel_delay_ms <= 2_000
                && self.venue_taker_delay_ms <= 2_000
                && self.order_response_ms <= 2_000
                && self.trade_clock_guard_ms <= 1_000
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
        Ok(())
    }
}

#[derive(Default, Deserialize)]
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
            Self::Book { at_ms, .. } | Self::Trade { at_ms, .. } => at_ms,
        }
    }
}

struct Queue {
    activated_ms: u64,
    ahead: f64,
}

#[derive(Default)]
struct Market {
    book: Book,
    queue: BTreeMap<i64, Queue>,
    last_mid: Option<f64>,
    post_only_rejections: u64,
    precision_rejections: u64,
    stale_rejections: u64,
    accepted_orders: u64,
}

pub(super) struct Matching {
    market: PlMutex<Market>,
    entry: Duration,
    cancel_delay: Duration,
    taker_delay: Duration,
    response: Duration,
    clock_guard_ms: u64,
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
            market: PlMutex::new(Market::default()),
            entry: Duration::from_millis(config.order_entry_ms),
            cancel_delay: Duration::from_millis(config.venue_cancel_delay_ms),
            taker_delay: Duration::from_millis(config.venue_taker_delay_ms),
            response: Duration::from_millis(config.order_response_ms),
            clock_guard_ms: config.trade_clock_guard_ms,
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
            state.last_mid = Some(bid[0] * 0.5 + ask[0] * 0.5);
        }
        // Never replenish IOC depth from a repeated publication.
        if book.timestamp_ms != state.book.timestamp_ms {
            state.book = book;
        }
    }

    pub(super) async fn wait_cancel(&self) {
        tokio::time::sleep(self.entry + self.cancel_delay).await;
    }

    fn batch_delay(&self, actions: &[BatchAction]) -> Duration {
        // Keep the recorded model's batch ordering and hold a mixed batch
        // until its slowest action is eligible to match.
        let venue_delay = actions
            .iter()
            .map(|action| match action {
                BatchAction::Cancel(_) => self.cancel_delay,
                BatchAction::Place(params) if params.order_type == OrderType::Ioc => {
                    self.taker_delay
                }
                BatchAction::Place(_) => Duration::ZERO,
            })
            .max()
            .unwrap_or_default();
        self.entry + venue_delay
    }

    pub(super) fn forget(&self, order_index: i64) {
        self.market.lock().queue.remove(&order_index);
    }

    fn admit(&self, order: Resting, ioc: bool) -> bool {
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
                },
            );
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
    ) {
        if source > at.saturating_add(self.clock_guard_ms) {
            return;
        }
        let eligible_ms = at.min(source.saturating_sub(self.clock_guard_ms));
        let mut orders: Vec<_> = venue
            .book
            .lock()
            .values()
            .filter(|order| {
                order.is_ask == taker_buy
                    && (if taker_buy {
                        price >= order.price - self.tick * 0.1
                    } else {
                        price <= order.price + self.tick * 0.1
                    })
            })
            .copied()
            .collect();
        orders.sort_unstable_by(|a, b| {
            let order = a.price.total_cmp(&b.price);
            (if taker_buy { order } else { order.reverse() })
                .then(a.exchange_order_id.cmp(&b.exchange_order_id))
        });
        let mut remaining = size;
        for order in orders {
            let id = order.exchange_order_id as i64;
            {
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
            }
            let qty = components::round_down_to_step(order.remaining.min(remaining), self.step);
            if qty > 0.0 {
                venue.fill(order, order.price, qty, false);
                remaining = (remaining - qty).max(0.0);
                if qty >= order.remaining - self.step * 1e-6 {
                    self.forget(id);
                }
            }
            if remaining < self.step {
                break;
            }
        }
    }

    fn ioc(&self, venue: &SimVenue, order: Resting, reduce_only: bool) {
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
        let levels = if order.is_ask {
            &mut state.book.bids
        } else {
            &mut state.book.asks
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
                    if self.admit(order, ioc) {
                        venue.activate(order);
                        if ioc {
                            self.ioc(venue, order, params.reduce_only);
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
}

async fn drive(
    config: RecordedMakerConfig,
    mut input: impl BufRead,
) -> Result<RecordedMakerReport> {
    let _clock = ReplayClock::open(config.start_ms);
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
    let (stop, stop_rx) = watch::channel(false);
    let (done, mut feed_done) = watch::channel(false);
    let mut account_done = feed_done.clone();
    let mut frames = 0;
    let mut trades = 0;
    let mut last = config.start_ms;
    let feed = async {
        let result: Result<()> = async {
            let mut line = String::new();
            loop {
                line.clear();
                if input.read_line(&mut line)? == 0 {
                    break;
                }
                ensure!(line.len() <= 64*1024,"maker event exceeds line limit");
                let event: Event = serde_json::from_str(&line).context("invalid recorded maker event")?;
                let at = event.at_ms();
                ensure!(at >= last && at < config.drain_until_ms,"maker events outside ordered replay window");
                last = at;
                tokio::select! {
                    _ = feed_done.changed() => return Ok(()),
                    _ = tokio::time::sleep_until(start+Duration::from_millis(at-config.start_ms)) => {}
                }
                match event {
                    Event::Book { lighter, hyperliquid, .. } => {
                        lighter.validate(at)?;
                        hyperliquid.validate(at)?;
                        let mut shm = files.borrow_mut();
                        lighter.write_bbo(&shm,LIGHTER);
                        hyperliquid.write_bbo(&shm,EXCH_HYPERLIQUID);
                        shm.publish_version();
                        matching.on_book(lighter);
                        frames += 1;
                    }
                    Event::Trade { source_ms, price, size, aggressor, .. } => {
                        ensure!(source_ms>0 && valid_price(price) && size.is_finite() && size>0.0,"invalid recorded trade");
                        matching.on_trade(&sim,at,source_ms,price,size,matches!(aggressor,Aggressor::Buy));
                        trades += 1;
                    }
                }
            }
            tokio::select! {
                _ = feed_done.changed() => {},
                _ = tokio::time::sleep_until(start+Duration::from_millis(config.drain_until_ms-config.start_ms)) => {},
            }
            Ok(())
        }.await;
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
        let result = strategy.run(Some(stop_rx)).await;
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
    let (data, (), result, ()) = tokio::join!(feed, accounts, run, end_quotes);
    data?;
    account(
        &config,
        &sim,
        &mut files.borrow_mut(),
        &mut risk.borrow_mut(),
    );
    let ledger = *sim.ledger.lock();
    let market = matching.market.lock();
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
    let funding_safe = active_until_ms / HOUR_MS == config.start_ms / HOUR_MS;
    Ok(RecordedMakerReport {
        start_ms: config.start_ms,
        end_ms: config.end_ms,
        active_until_ms,
        initial_equity: config.initial_equity,
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

    #[tokio::test(start_paused = true)]
    async fn tape_and_iocs_spend_observed_volume_once() {
        const START: u64 = 1_800_000_000_000;
        let _clock = ReplayClock::open(START);
        let config = RecordedMakerConfig {
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
            order_response_ms: 0,
            trade_clock_guard_ms: 100,
            taker_fee_bps: 4.0,
            min_contracts: 0.001,
            min_notional: 1.0,
            depth_participation: 0.25,
        };
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
        matching.on_trade(&venue, now_ms(), START + 200, 100.0, 1.0, false);
        assert_eq!(venue.net_position(), 0.0);
        tokio::time::advance(Duration::from_millis(200)).await;
        matching.on_trade(&venue, now_ms(), now_ms(), 100.0, 0.075, false);
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
}
