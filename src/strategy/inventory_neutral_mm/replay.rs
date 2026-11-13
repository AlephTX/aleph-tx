//! Offline venue and shared-memory adapter for the production quoting loop.
//!
//! Each step publishes a BBO frame, settles resting orders the new frame
//! trades through (the market moves before the strategy reacts), publishes
//! the resulting account snapshot, then runs one real `quote_cycle`
//! (pricing, risk, grid, requote, cancels). Every order lifecycle event is applied through
//! the real `OrderTracker::apply_event` with consecutive sequence numbers, the
//! same path the V2 event ring drives in production.
//!
//! Matching model (deliberately conservative, documented assumptions):
//! - A resting buy fills only when the frame's best ask trades through its
//!   price (`ask <= price`); a sell when `bid >= price`. No queue position,
//!   no partial queue priority: a through-trade fills the full remaining size
//!   unless `partial_fill_fraction` is set for that step.
//! - Fills execute at the order's limit price (maker), fee = maker_fee_bps.
//! - Order acknowledgements and cancels are synchronous within a step; one
//!   step = one strategy cycle. Network latency is not modelled.
//! - Scenario hooks inject cancel/fill races (`fill_before_cancel`), event
//!   gaps (`skip_sequence`) and identity-preserving duplicate deliveries
//!   (`redeliver` with the stale sequence, `republish` with a fresh one).
//! - Timeout flatten IOCs match against the current touch at a simulated
//!   taker fee (`SIM_TAKER_FEE_BPS`); reduce-only caps them at the position.
//! - Time: runs on a paused Tokio clock; strategy, tracker and SHM source
//!   timestamps share that domain and advance only via `Frame::advance_ms`
//!   and the strategy's own sleeps.
//!
//! These assumptions apply to the synthetic tests. The recorded-data
//! runner uses a public trade tape, depth, and a separate latency model.

#[cfg(feature = "research-replay")]
pub mod recorded;

use super::*;
use crate::exchange::{
    BatchOrderParams, BatchOrderResult, BatchResult, OrderInfo, OrderResult, PlaceResult,
};
use crate::shm_reader::NUM_SYMBOLS;
use crate::types::ShmPrivateEventV2;
use parking_lot::Mutex as PlMutex;
use std::collections::BTreeMap;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;

const LIGHTER: u8 = crate::config::EXCH_LIGHTER;
#[cfg(test)]
const BINANCE: u8 = 6;
#[cfg(test)]
const SYMBOL: u16 = crate::config::SYM_ETH;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Resting {
    pub client_order_id: i64,
    pub exchange_order_id: u64,
    pub is_ask: bool,
    pub price: f64,
    pub remaining: f64,
}

/// Taker fee for offline IOC fills (timeout flatten). Assumption, not a quote.
#[cfg(test)]
const SIM_TAKER_FEE_BPS: f64 = 2.0;

/// Venue action as observed by the simulated exchange, in scaled integers so
/// traces compare exactly. `at_ms` is replay time since the run started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(test)]
pub(super) enum Action {
    Place {
        at_ms: u64,
        is_ask: bool,
        price: i64,
        size: i64,
        ioc: bool,
    },
    Cancel {
        at_ms: u64,
        exchange_order_id: u64,
    },
    CancelMiss {
        at_ms: u64,
        order_index: i64,
    },
    Fill {
        at_ms: u64,
        is_ask: bool,
        price: i64,
        size: i64,
        fee: i64,
        taker: bool,
    },
}

#[cfg(test)]
fn e4(v: f64) -> i64 {
    (v * 1e4).round() as i64
}

fn e8(v: f64) -> i64 {
    (v * 1e8).round() as i64
}

#[derive(Clone, Copy, Default)]
struct Ledger {
    position_e8: i64,
    cash: f64,
    maker_notional: f64,
    taker_notional: f64,
    maker_fees: f64,
    taker_fees: f64,
    maker_fills: u64,
    taker_fills: u64,
}

/// Simulated Lighter venue. Holds resting orders and emits tracker events.
pub(super) struct SimVenue {
    tracker: Arc<OrderTracker>,
    pub book: PlMutex<BTreeMap<i64, Resting>>,
    next_coid: std::sync::atomic::AtomicI64,
    next_exchange_id: std::sync::atomic::AtomicU64,
    sequence: std::sync::atomic::AtomicU64,
    pub fee_bps: f64,
    taker_fee_bps: f64,
    symbol: u16,
    ledger: PlMutex<Ledger>,
    #[cfg(feature = "research-replay")]
    recorded: Option<recorded::Matching>,
    /// Fill events applied, for conservation checks: (is_ask, price, size, fee).
    #[cfg(test)]
    pub fills: PlMutex<Vec<(bool, f64, f64, f64)>>,
    /// Count of cancel requests that found no live order (cancel/fill races).
    #[cfg(test)]
    pub cancel_misses: std::sync::atomic::AtomicU64,
    /// When set, a cancel request loses the race: the order fills first and
    /// the cancel finds nothing, as with Lighter's "order not found".
    #[cfg(test)]
    pub fill_before_cancel: std::sync::atomic::AtomicBool,
    /// Fill events exactly as published, for identity-preserving redelivery.
    #[cfg(test)]
    pub fill_events: PlMutex<Vec<ShmPrivateEventV2>>,
    /// Every venue action with its replay time.
    #[cfg(test)]
    pub actions: PlMutex<Vec<Action>>,
    /// Current visible touch, used to match IOC orders.
    touch: PlMutex<(f64, f64)>,
    #[cfg(test)]
    started: Instant,
}

impl SimVenue {
    #[cfg(test)]
    pub fn new(tracker: Arc<OrderTracker>, fee_bps: f64) -> Self {
        Self::for_symbol(tracker, SYMBOL, fee_bps, SIM_TAKER_FEE_BPS)
    }

    fn for_symbol(
        tracker: Arc<OrderTracker>,
        symbol: u16,
        fee_bps: f64,
        taker_fee_bps: f64,
    ) -> Self {
        Self {
            tracker,
            book: PlMutex::new(BTreeMap::new()),
            next_coid: std::sync::atomic::AtomicI64::new(1),
            next_exchange_id: std::sync::atomic::AtomicU64::new(1_000),
            sequence: std::sync::atomic::AtomicU64::new(0),
            fee_bps,
            taker_fee_bps,
            symbol,
            ledger: PlMutex::new(Ledger::default()),
            #[cfg(feature = "research-replay")]
            recorded: None,
            #[cfg(test)]
            fills: PlMutex::new(Vec::new()),
            #[cfg(test)]
            cancel_misses: std::sync::atomic::AtomicU64::new(0),
            #[cfg(test)]
            fill_before_cancel: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            fill_events: PlMutex::new(Vec::new()),
            #[cfg(test)]
            actions: PlMutex::new(Vec::new()),
            touch: PlMutex::new((0.0, 0.0)),
            #[cfg(test)]
            started: Instant::now(),
        }
    }

    #[cfg(test)]
    fn now_ms(&self) -> u64 {
        Instant::now().duration_since(self.started).as_millis() as u64
    }

    #[cfg(test)]
    fn record(&self, action: Action) {
        self.actions.lock().push(action);
    }

    fn next_sequence(&self) -> u64 {
        self.sequence
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1
    }

    fn apply(&self, event: ShmPrivateEventV2) {
        // Mirrors the production event task: apply_event marks reconciliation
        // on invalid input, so an error here is a replay invariant failure.
        self.tracker
            .apply_event(&event)
            .expect("replay emitted an event the tracker rejected");
    }

    /// Register an order the strategy placed, as LighterTrading would before
    /// submission, then acknowledge it as the private stream would.
    fn place(&self, side: Side, price: f64, size: f64, ioc: bool) -> PlaceResult {
        let order = self.reserve(side, price, size, ioc);
        self.activate(order);
        PlaceResult {
            client_order_index: order.client_order_id,
            side,
            price,
            size,
        }
    }

    fn reserve(&self, side: Side, price: f64, size: f64, _ioc: bool) -> Resting {
        let coid = self
            .next_coid
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let exchange_order_id = self
            .next_exchange_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let tracker_side = match side {
            Side::Buy => OrderSide::Buy,
            Side::Sell => OrderSide::Sell,
        };
        assert!(self.tracker.start_tracking(coid, tracker_side, price, size));
        let is_ask = side == Side::Sell;
        #[cfg(test)]
        self.record(Action::Place {
            at_ms: self.now_ms(),
            is_ask,
            price: e4(price),
            size: e8(size),
            ioc: _ioc,
        });
        Resting {
            client_order_id: coid,
            exchange_order_id,
            is_ask,
            price,
            remaining: size,
        }
    }

    fn event_time(&self) -> u64 {
        #[cfg(feature = "research-replay")]
        if self.recorded.is_some() {
            return wall_time_ns();
        }
        0
    }

    fn activate(&self, order: Resting) {
        self.apply(ShmPrivateEventV2::order_created(
            self.next_sequence(),
            LIGHTER,
            self.symbol,
            order.exchange_order_id,
            order.client_order_id,
            order.exchange_order_id as i64,
            order.price,
            order.remaining,
            order.is_ask,
            self.event_time(),
        ));
        self.book
            .lock()
            .insert(order.exchange_order_id as i64, order);
    }

    fn cancel(&self, order_index: i64) {
        #[cfg(test)]
        if self
            .fill_before_cancel
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            let racing = self.book.lock().get(&order_index).copied();
            if let Some(order) = racing {
                self.fill(order, order.price, order.remaining, false);
            }
        }
        let removed = self.book.lock().remove(&order_index);
        match removed {
            Some(order) => {
                #[cfg(feature = "research-replay")]
                if let Some(matching) = &self.recorded {
                    matching.cancelled(order);
                }
                #[cfg(test)]
                self.record(Action::Cancel {
                    at_ms: self.now_ms(),
                    exchange_order_id: order.exchange_order_id,
                });
                self.apply(ShmPrivateEventV2::order_canceled(
                    self.next_sequence(),
                    LIGHTER,
                    self.symbol,
                    order.exchange_order_id,
                    order.client_order_id,
                    order_index,
                    order.remaining,
                    self.event_time(),
                ))
            }
            #[cfg(test)]
            None => {
                self.record(Action::CancelMiss {
                    at_ms: self.now_ms(),
                    order_index,
                });
                self.cancel_misses
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            #[cfg(not(test))]
            None => {}
        }
    }

    /// Fill resting orders the frame trades through. `fraction` < 1 leaves a
    /// partial remainder resting.
    #[cfg(test)]
    pub fn settle(&self, bid: f64, ask: f64, fraction: f64) {
        *self.touch.lock() = (bid, ask);
        let crossing: Vec<Resting> = self
            .book
            .lock()
            .values()
            .filter(|o| {
                if o.is_ask {
                    bid >= o.price
                } else {
                    ask <= o.price
                }
            })
            .copied()
            .collect();
        for order in crossing {
            let size = if fraction >= 1.0 {
                order.remaining
            } else {
                // Keep fills on a 1e-8 grid like the tracker's scaled units.
                ((order.remaining * fraction) * 1e8).floor() / 1e8
            };
            if size <= 0.0 {
                continue;
            }
            self.fill(order, order.price, size, false);
        }
    }

    /// Fill `size` of a booked order at `price` and emit the event. Makers
    /// fill at their limit; takers at the touch they cross.
    fn fill(&self, order: Resting, price: f64, size: f64, taker: bool) {
        let remaining = ((order.remaining - size) * 1e8).round() / 1e8;
        let fee_bps = if taker {
            self.taker_fee_bps
        } else {
            self.fee_bps
        };
        let fee = price * size * fee_bps / 10_000.0;
        let sequence = self.next_sequence();
        let event = ShmPrivateEventV2::order_filled(
            sequence,
            LIGHTER,
            self.symbol,
            order.exchange_order_id,
            order.client_order_id,
            order.exchange_order_id as i64,
            price,
            size,
            remaining,
            fee,
            order.is_ask,
            self.event_time(),
            sequence + 1_000_000, // unique trade id per fill
        );
        self.apply(event);
        {
            let mut ledger = self.ledger.lock();
            let notional = price * size;
            let sign = if order.is_ask { -1 } else { 1 };
            ledger.position_e8 += sign * e8(size);
            ledger.cash -= sign as f64 * notional + fee;
            if taker {
                ledger.taker_notional += notional;
                ledger.taker_fees += fee;
                ledger.taker_fills += 1;
            } else {
                ledger.maker_notional += notional;
                ledger.maker_fees += fee;
                ledger.maker_fills += 1;
            }
        }
        #[cfg(test)]
        self.fill_events.lock().push(event);
        #[cfg(test)]
        self.record(Action::Fill {
            at_ms: self.now_ms(),
            is_ask: order.is_ask,
            price: e4(price),
            size: e8(size),
            fee: e8(fee),
            taker,
        });
        #[cfg(test)]
        self.fills.lock().push((order.is_ask, price, size, fee));
        let mut book = self.book.lock();
        if remaining > 0.0 {
            if let Some(entry) = book.get_mut(&(order.exchange_order_id as i64)) {
                entry.remaining = remaining;
            }
        } else {
            book.remove(&(order.exchange_order_id as i64));
        }
    }

    /// Offline IOC: book, take against the current touch if it crosses, then
    /// cancel any remainder. `reduce_only` caps size at the open position.
    /// No network or signing; this exists only so timeout flatten settles
    /// through the real tracker.
    fn place_ioc(&self, order: &crate::exchange::OrderParams) -> PlaceResult {
        let position = self.net_position();
        let mut size = order.size;
        if order.reduce_only {
            let reducible = match order.side {
                Side::Sell => position.max(0.0),
                Side::Buy => (-position).max(0.0),
            };
            size = size.min(reducible);
        }
        let result = self.place(order.side, order.price, size, true);
        let booked = self
            .book
            .lock()
            .values()
            .find(|o| o.client_order_id == result.client_order_index)
            .copied();
        let Some(booked) = booked else {
            return result;
        };
        let (bid, ask) = *self.touch.lock();
        let crosses = match order.side {
            Side::Buy => ask > 0.0 && order.price >= ask,
            Side::Sell => bid > 0.0 && order.price <= bid,
        };
        if crosses && size > 0.0 {
            let price = if order.side == Side::Buy { ask } else { bid };
            self.fill(booked, price, size, true);
        }
        if self
            .book
            .lock()
            .contains_key(&(booked.exchange_order_id as i64))
        {
            self.cancel(booked.exchange_order_id as i64);
        }
        result
    }

    /// Burn one sequence number so the next event arrives after a gap.
    #[cfg(test)]
    pub fn skip_sequence(&self) {
        self.next_sequence();
    }

    /// Re-apply a previously published event verbatim: same order, client,
    /// exchange and trade identity and its original (now stale) sequence, as
    /// when a ring reader re-reads a slot.
    #[cfg(test)]
    pub fn redeliver(&self, event: &ShmPrivateEventV2) -> anyhow::Result<()> {
        self.tracker.apply_event(event)
    }

    /// Re-publish a previously published fill under a fresh sequence. The Go
    /// feeder dedupes trade ids only within one process and a bounded window
    /// (feeder/exchanges/trade_dedupe.go), so after a feeder restart or window
    /// eviction a websocket replay reaches the ring as a new sequence with an
    /// already-applied trade id.
    #[cfg(test)]
    pub fn republish(&self, event: &ShmPrivateEventV2) -> anyhow::Result<()> {
        let mut event = *event;
        event.sequence = self.next_sequence();
        self.tracker.apply_event(&event)
    }

    pub fn net_position(&self) -> f64 {
        self.ledger.lock().position_e8 as f64 / 1e8
    }

    /// Signed trade cash flow less fees, as OrderTracker accounts it.
    pub fn cash_flow(&self) -> f64 {
        self.ledger.lock().cash
    }
}

#[async_trait::async_trait]
impl Exchange for SimVenue {
    async fn buy(&self, _: f64, _: f64) -> anyhow::Result<OrderResult> {
        anyhow::bail!("replay: market orders are not simulated")
    }
    async fn sell(&self, _: f64, _: f64) -> anyhow::Result<OrderResult> {
        anyhow::bail!("replay: market orders are not simulated")
    }
    async fn place_batch(&self, _: BatchOrderParams) -> anyhow::Result<BatchOrderResult> {
        anyhow::bail!("replay: legacy batch API is not used by the strategy")
    }
    async fn cancel_order(&self, order_index: i64) -> anyhow::Result<()> {
        #[cfg(feature = "research-replay")]
        if let Some(matching) = &self.recorded {
            matching.record_cancel_request(Some(order_index));
            matching.wait_cancel().await;
        }
        self.cancel(order_index);
        Ok(())
    }
    async fn cancel_all(&self) -> anyhow::Result<u32> {
        #[cfg(feature = "research-replay")]
        if let Some(matching) = &self.recorded {
            matching.record_cancel_request(None);
            matching.wait_cancel().await;
        }
        let ids: Vec<i64> = self.book.lock().keys().copied().collect();
        for id in &ids {
            self.cancel(*id);
        }
        Ok(ids.len() as u32)
    }
    async fn get_active_orders(&self) -> anyhow::Result<Vec<OrderInfo>> {
        #[cfg(feature = "research-replay")]
        if let Some(matching) = &self.recorded {
            // A public print can arrive after the simulated cancel ACK. Do
            // not let cleanup certify a fresh position before that bounded
            // delivery window has elapsed and the print has been applied.
            matching.wait_cancelled_prints().await?;
        }
        Ok(self
            .book
            .lock()
            .values()
            .map(|o| OrderInfo {
                order_id: o.exchange_order_id.to_string(),
                client_order_index: o.client_order_id,
                side: if o.is_ask { Side::Sell } else { Side::Buy },
                price: o.price,
                size: o.remaining,
                filled: 0.0,
            })
            .collect())
    }
    async fn close_all_positions(&self, _mid: f64) -> anyhow::Result<()> {
        #[cfg(feature = "research-replay")]
        if let Some(matching) = &self.recorded {
            return matching.flatten(self, _mid).await;
        }
        anyhow::bail!("replay: taker flatten is not simulated")
    }
    async fn execute_batch(&self, actions: Vec<BatchAction>) -> anyhow::Result<BatchResult> {
        #[cfg(feature = "research-replay")]
        if let Some(matching) = &self.recorded {
            return matching.execute(self, actions).await;
        }
        let mut place_results = Vec::new();
        for action in actions {
            match action {
                BatchAction::Place(order) => {
                    assert!(
                        order.size > 0.0 && order.price > 0.0,
                        "strategy placed a non-positive order"
                    );
                    if order.order_type == OrderType::Ioc {
                        place_results.push(self.place_ioc(&order));
                    } else {
                        place_results.push(self.place(order.side, order.price, order.size, false));
                    }
                }
                BatchAction::Cancel(order_index) => self.cancel(order_index),
            }
        }
        Ok(BatchResult {
            tx_hashes: vec![],
            place_results,
        })
    }
    async fn get_account_stats(&self) -> anyhow::Result<AccountStats> {
        anyhow::bail!("replay: account stats come from SHM")
    }
    fn limit_order_type(&self) -> OrderType {
        OrderType::PostOnly
    }
}

/// One replay frame: local and external books plus optional partial fills.
#[derive(Debug, Clone, Copy)]
#[cfg(test)]
pub(super) struct Frame {
    pub lighter_bid: f64,
    pub lighter_ask: f64,
    pub external_mid: f64,
    pub fill_fraction: f64,
    /// Replay time advanced before this frame is published.
    pub advance_ms: u64,
    /// When false the BBO feed is silent: prices and source timestamps keep
    /// their previous values and only time moves.
    pub bbo_live: bool,
    /// When false no account snapshot is published for this frame or while
    /// its cycle runs, so the last snapshot ages.
    pub account_live: bool,
    /// Publish this frame's account snapshot this far in the future
    /// (a clock-skewed source); 0 for a normal snapshot.
    pub account_ahead_ms: u64,
}

/// Temp SHM segments the strategy reads like production.
pub(super) struct ShmFiles {
    pub directory: PathBuf,
    matrix: File,
    account: File,
    symbol: u16,
    version: u64,
    account_version: u64,
}

impl ShmFiles {
    #[cfg(test)]
    pub fn create() -> Self {
        Self::for_symbol(SYMBOL).unwrap()
    }

    fn for_symbol(symbol: u16) -> anyhow::Result<Self> {
        use std::os::unix::fs::DirBuilderExt;
        let directory = std::env::temp_dir().join(format!("aleph-replay-{}", uuid::Uuid::new_v4()));
        std::fs::DirBuilder::new().mode(0o700).create(&directory)?;
        let matrix = File::create_new(directory.join("matrix"))?;
        matrix.set_len((NUM_SYMBOLS * 8 + NUM_SYMBOLS * NUM_EXCHANGES * 64) as u64)?;
        let account = File::create_new(directory.join("account"))?;
        account.set_len(128)?;
        Ok(Self {
            directory,
            matrix,
            account,
            symbol,
            version: 0,
            account_version: 0,
        })
    }

    fn path(&self, name: &str) -> PathBuf {
        self.directory.join(name)
    }

    #[cfg(test)]
    fn write_at(path: &std::path::Path, offset: u64, bytes: &[u8]) {
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.write_all_at(bytes, offset).unwrap();
    }

    fn write_bbo(&self, exchange: u8, bid: (f64, f64), ask: (f64, f64), ts_ns: u64) {
        let mut slot = [0u8; 64];
        slot[0..4].copy_from_slice(&2u32.to_le_bytes()); // even seqlock
        slot[4] = 1;
        slot[5] = exchange;
        slot[6..8].copy_from_slice(&self.symbol.to_le_bytes());
        slot[8..16].copy_from_slice(&ts_ns.to_le_bytes());
        slot[16..24].copy_from_slice(&bid.0.to_le_bytes());
        slot[24..32].copy_from_slice(&bid.1.to_le_bytes());
        slot[32..40].copy_from_slice(&ask.0.to_le_bytes());
        slot[40..48].copy_from_slice(&ask.1.to_le_bytes());
        let offset = (NUM_SYMBOLS * 8) as u64
            + (self.symbol as u64 * NUM_EXCHANGES as u64 + exchange as u64) * 64;
        self.matrix.write_all_at(&slot, offset).unwrap();
    }

    #[cfg(test)]
    pub fn publish(&mut self, frame: &Frame, ts_ns: u64) {
        self.write_bbo(
            LIGHTER,
            (frame.lighter_bid, 5.0),
            (frame.lighter_ask, 5.0),
            ts_ns,
        );
        let half = frame.external_mid * 0.5e-4;
        self.write_bbo(
            BINANCE,
            (frame.external_mid - half, 5.0),
            (frame.external_mid + half, 5.0),
            ts_ns,
        );
        self.publish_version();
    }

    fn publish_version(&mut self) {
        self.version += 1;
        self.matrix
            .write_all_at(&self.version.to_le_bytes(), u64::from(self.symbol) * 8)
            .unwrap();
    }

    #[cfg(test)]
    pub fn publish_account(&mut self, equity: f64, position: f64, ts_ns: u64) {
        self.write_account(equity, equity, position, 0.05, ts_ns);
    }

    #[cfg(feature = "research-replay")]
    fn publish_account_with_margin(&mut self, equity: f64, position: f64, margin: f64, ts_ns: u64) {
        let available = (equity - margin).max(0.0);
        let usage = if equity > 0.0 {
            (margin / equity).clamp(0.0, 1.0)
        } else {
            1.0
        };
        self.write_account(equity, available, position, usage, ts_ns);
    }

    fn write_account(
        &mut self,
        equity: f64,
        available: f64,
        position: f64,
        margin_usage: f64,
        ts_ns: u64,
    ) {
        self.account_version += 2; // even = stable
        let mut stats = [0u8; 128];
        let f = |v: f64| v.to_le_bytes();
        stats[0..8].copy_from_slice(&self.account_version.to_le_bytes());
        stats[8..16].copy_from_slice(&f(equity)); // collateral
        stats[16..24].copy_from_slice(&f(equity)); // portfolio value
        stats[24..32].copy_from_slice(&f(1.0)); // leverage
        stats[32..40].copy_from_slice(&f(available));
        stats[40..48].copy_from_slice(&f(margin_usage));
        stats[48..56].copy_from_slice(&f(available * 10.0)); // buying power
        stats[56..64].copy_from_slice(&ts_ns.to_le_bytes());
        stats[64..72].copy_from_slice(&f(position));
        stats[72..80].copy_from_slice(&ts_ns.to_le_bytes());
        self.account.write_all_at(&stats, 0).unwrap();
    }
}

impl Drop for ShmFiles {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

/// Result of a replay, compared for determinism.
#[derive(Debug, Clone, PartialEq)]
#[cfg(test)]
pub(super) struct Outcome {
    pub fills: Vec<(bool, i64, i64)>, // (is_ask, price in 1e-4, size in 1e-8)
    pub position: i64,
    pub cash_flow: i64,
    pub resting: Vec<(bool, i64, i64)>,
    pub cycles: Vec<CycleOutcome>,
    pub cancel_misses: u64,
    /// First `quote_cycle` error; the replay stops there.
    pub error: Option<String>,
    /// Whether the tracker still holds unconfirmed exposure at the end.
    pub needs_reconciliation: bool,
    /// Full venue action trace with replay timestamps.
    pub actions: Vec<Action>,
    /// Per-cycle (replay ms at cycle end, position e8, cash e8, resting count).
    pub states: Vec<(u64, i64, i64, usize)>,
    /// Number of input frames; a complete run has one cycle per frame.
    pub frames: usize,
}

#[cfg(test)]
impl Outcome {
    /// A successful replay consumed every frame: no error, one cycle outcome
    /// and one state per input frame. Use for every success scenario; the
    /// cycle outcomes themselves are checked separately.
    #[track_caller]
    pub fn assert_complete(&self) {
        assert!(self.frames > 0, "replay without frames");
        assert_eq!(self.error, None, "replay stopped early");
        assert_eq!(self.cycles.len(), self.frames, "cycles != frames");
        assert_eq!(self.states.len(), self.frames, "states != frames");
    }

    /// An expected failure: `quote_cycle` failed on frame `at` (0-based)
    /// after `at` completed cycles, with an error containing `reason`.
    #[track_caller]
    pub fn assert_failed_at(&self, at: usize, reason: &str) {
        assert!(at < self.frames, "failure frame outside input");
        let error = self.error.as_deref().expect("replay did not fail");
        assert!(error.contains(reason), "unexpected error: {error}");
        assert_eq!(self.cycles.len(), at, "completed prefix");
        assert_eq!(self.states.len(), at, "completed prefix states");
    }
}

#[cfg(test)]
pub(super) fn wall_ns() -> u64 {
    wall_time_ns()
}

/// Drive the production strategy over `frames`, one quote_cycle per frame.
///
/// Must run on a paused Tokio clock (`#[tokio::test(start_paused = true)]`):
/// the strategy, `OrderTracker` and SHM timestamps then share one replay
/// time domain that advances only via `Frame::advance_ms` and the strategy's
/// own sleeps, so time-gated branches replay identically.
#[cfg(test)]
pub(super) async fn run(
    config: InventoryNeutralMMConfig,
    frames: &[Frame],
    equity: f64,
    after_cycle: impl Fn(usize, &SimVenue, &OrderTracker),
) -> Outcome {
    run_with(config, frames, equity, None, after_cycle, |_, _| {}).await
}

/// `run` with an optional telemetry sink on the strategy and a hook that
/// sees the strategy itself after each cycle (e.g. to export snapshots).
#[cfg(test)]
pub(super) async fn run_with(
    config: InventoryNeutralMMConfig,
    frames: &[Frame],
    equity: f64,
    telemetry: Option<crate::telemetry_sink::TelemetrySink>,
    after_cycle: impl Fn(usize, &SimVenue, &OrderTracker),
    mut inspect: impl FnMut(usize, &mut InventoryNeutralMM),
) -> Outcome {
    let mut shm = ShmFiles::create();
    let tracker = Arc::new(OrderTracker::new());
    let venue = Arc::new(SimVenue::new(Arc::clone(&tracker), config.maker_fee_bps));
    let ts = wall_ns();
    shm.publish(&frames[0], ts);
    shm.publish_account(equity, 0.0, ts);
    let reader = ShmReader::open(shm.path("matrix").to_str().unwrap(), NUM_SYMBOLS).unwrap();
    let account = AccountStatsReader::open(shm.path("account").to_str().unwrap()).unwrap();
    let mut strategy = InventoryNeutralMM::with_data_sources(
        config,
        Arc::clone(&venue) as Arc<dyn Exchange>,
        Arc::clone(&tracker),
        reader,
        account,
        None,
    );
    if let Some(sink) = telemetry {
        strategy = strategy.with_telemetry_sink(sink);
    }
    strategy.config.validate().unwrap();
    strategy.perform_startup_initialization().await.unwrap();
    let mut cycles = Vec::with_capacity(frames.len());
    let mut states = Vec::with_capacity(frames.len());
    let mut book = frames[0];
    let mut error = None;
    for (index, frame) in frames.iter().enumerate() {
        tokio::time::advance(Duration::from_millis(frame.advance_ms)).await;
        let ts = wall_ns();
        if frame.bbo_live {
            book = *frame;
            shm.publish(frame, ts);
        }
        venue.settle(book.lighter_bid, book.lighter_ask, frame.fill_fraction);
        if frame.account_live {
            let account_ts = ts + frame.account_ahead_ms * 1_000_000;
            shm.publish_account(equity, venue.net_position(), account_ts);
        }
        // Like the Go feeder, keep account snapshots flowing while the cycle
        // runs so cleanup barriers (snapshot newer than cancel) can resolve.
        let done = tokio::sync::Notify::new();
        let cycle = async {
            let outcome = strategy.quote_cycle(None).await;
            done.notify_one();
            outcome
        };
        let feeder = async {
            loop {
                tokio::select! {
                    _ = done.notified() => break,
                    _ = tokio::time::sleep(Duration::from_millis(20)) => {
                        if frame.account_live {
                            shm.publish_account(equity, venue.net_position(), wall_ns());
                        }
                    }
                }
            }
        };
        let (outcome, ()) = tokio::join!(cycle, feeder);
        match outcome {
            Ok(outcome) => cycles.push(outcome),
            Err(err) => {
                error = Some(format!("{err:#}"));
                break;
            }
        }
        after_cycle(index, &venue, &tracker);
        inspect(index, &mut strategy);
        states.push((
            venue.now_ms(),
            e8(venue.net_position()),
            e8(venue.cash_flow()),
            venue.book.lock().len(),
        ));
    }
    let scale = |v: f64, s: f64| (v * s).round() as i64;
    let fills = venue
        .fills
        .lock()
        .iter()
        .map(|(a, p, z, _)| (*a, scale(*p, 1e4), scale(*z, 1e8)))
        .collect();
    let resting = venue
        .book
        .lock()
        .values()
        .map(|o| (o.is_ask, scale(o.price, 1e4), scale(o.remaining, 1e8)))
        .collect();
    let outcome = Outcome {
        fills,
        position: scale(venue.net_position(), 1e8),
        cash_flow: scale(venue.cash_flow(), 1e8),
        resting,
        cycles,
        cancel_misses: venue
            .cancel_misses
            .load(std::sync::atomic::Ordering::SeqCst),
        error,
        needs_reconciliation: tracker.needs_reconciliation(),
        actions: venue.actions.lock().clone(),
        states,
        frames: frames.len(),
    };
    drop(strategy);
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> InventoryNeutralMMConfig {
        InventoryNeutralMMConfig {
            symbol_id: SYMBOL,
            tick_size: 0.01,
            step_size: 0.0001,
            base_order_size: 0.01,
            max_position: 0.1,
            inventory_urgency_threshold: 0.05,
            grid_levels: 2,
            grid_spacing_bps: 2.0,
            use_post_only: true,
            use_depth_pricing: false,
            // Production timing defaults (order_ttl_secs = min lifetime 5 s,
            // position_timeout_secs = 60 s) apply on the replay clock.
            ..Default::default()
        }
    }

    #[test]
    fn cross_market_reserve_widens_the_inventory_adding_side_only() {
        let shm = ShmFiles::create();
        let reader = ShmReader::open(shm.path("matrix").to_str().unwrap(), NUM_SYMBOLS).unwrap();
        let account = AccountStatsReader::open(shm.path("account").to_str().unwrap()).unwrap();
        let tracker = Arc::new(OrderTracker::new());
        let venue = Arc::new(SimVenue::new(Arc::clone(&tracker), 0.0));
        let strategy = InventoryNeutralMM::with_data_sources(
            config(),
            venue as Arc<dyn Exchange>,
            tracker,
            reader,
            account,
            None,
        );
        let mut inputs = super::super::PricingInputs {
            mid: 2_000.0,
            pricing_mid: 2_000.0,
            bid_touch: 1_999.9,
            ask_touch: 2_000.1,
            vol_bps: 1.0,
            sigma_per_sqrt_sec: 0.0001,
            as_score: 0.0,
            using_external_fair_value: true,
            order_flow_bps: 0.0,
            cross_market_bps: 0.0,
            volatility_pressure: 0.0,
        };
        let (bid, ask, baseline) = strategy.calculate_optimal_quotes(&inputs, 0.01).unwrap();
        inputs.cross_market_bps = -3.0;
        let (safer_bid, unchanged_ask, bounds) =
            strategy.calculate_optimal_quotes(&inputs, 0.01).unwrap();
        assert!(safer_bid < bid);
        assert_eq!(unchanged_ask, ask);
        assert_eq!(bounds.max_bid, baseline.max_bid);
        assert_eq!(bounds.min_ask, baseline.min_ask);
        inputs.cross_market_bps = 3.0;
        let (unchanged_bid, unchanged_ask, _) =
            strategy.calculate_optimal_quotes(&inputs, 0.01).unwrap();
        assert_eq!((unchanged_bid, unchanged_ask), (bid, ask));
    }

    /// Live frame one second after the previous one.
    fn frame(mid: f64, fill_fraction: f64) -> Frame {
        Frame {
            lighter_bid: mid - 0.5,
            lighter_ask: mid + 0.5,
            external_mid: mid,
            fill_fraction,
            advance_ms: 1_000,
            bbo_live: true,
            account_live: true,
            account_ahead_ms: 0,
        }
    }

    /// Quiet book around 2000, then two gap-downs that trade through resting
    /// bids (the first only half-fills), then a quiet tail.
    fn frames() -> Vec<Frame> {
        let mut frames = vec![frame(2000.0, 1.0); 4];
        frames.push(frame(1993.0, 0.5)); // gap: partial fill of the top bid
        frames.push(frame(1993.0, 1.0));
        frames.push(frame(1985.0, 1.0)); // second gap through re-placed bids
        frames.extend(vec![frame(1985.0, 1.0); 3]);
        frames
    }

    /// Exposure invariants that must hold after every cycle.
    fn check(index: usize, venue: &SimVenue, tracker: &OrderTracker) {
        let eps = 1e-7;
        // Position conservation: tracker == sum of venue fills.
        assert!(
            (tracker.confirmed_position() - venue.net_position()).abs() < eps,
            "cycle {index}: tracker position {} != venue fills {}",
            tracker.confirmed_position(),
            venue.net_position()
        );
        // Cash conservation, fees included.
        assert!(
            (tracker.trade_cash_flow() - venue.cash_flow()).abs() < 1e-6,
            "cycle {index}: cash {} != {}",
            tracker.trade_cash_flow(),
            venue.cash_flow()
        );
        // Every venue-resting order is reserved by the tracker (no hidden risk).
        let (confirmed, short, long) = tracker.exposure_bounds();
        let book = venue.book.lock();
        let resting_buy: f64 = book
            .values()
            .filter(|o| !o.is_ask)
            .map(|o| o.remaining)
            .sum();
        let resting_sell: f64 = book
            .values()
            .filter(|o| o.is_ask)
            .map(|o| o.remaining)
            .sum();
        assert!(
            long - confirmed + eps >= resting_buy,
            "cycle {index}: buy exposure under-reserved"
        );
        assert!(
            confirmed - short + eps >= resting_sell,
            "cycle {index}: sell exposure under-reserved"
        );
        // Atomic and locked exposure agree.
        assert!((tracker.worst_case_long() - tracker.worst_case_long_locked()).abs() < eps);
        // Resting quotes never cross the visible book (post-only maker).
        drop(book);
        assert!(
            !tracker.needs_reconciliation(),
            "cycle {index}: reconciliation required"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn production_strategy_replay_conserves_inventory_and_cash() {
        let outcome = run(config(), &frames(), 1_000.0, check).await;
        outcome.assert_complete();
        assert!(
            outcome.cycles.iter().all(|c| *c == CycleOutcome::Quoted),
            "{:?}",
            outcome.cycles
        );
        // The down-trend trades through resting bids: the MM ends long.
        assert!(
            !outcome.fills.is_empty(),
            "no fills in a trade-through trend"
        );
        assert!(outcome.fills.iter().all(|(is_ask, _, _)| !is_ask));
        assert!(outcome.position > 0);
        // Inventory stays within the equity-scaled cap the strategy enforces.
        let cap = components::scaled_max_position(&config(), 1_000.0, 1985.0);
        let position = outcome.position as f64 / 1e8;
        assert!(position <= cap + 1e-8, "position={position} cap={cap}");
        // A long position skews quotes toward selling: an ask rests.
        assert!(outcome.resting.iter().any(|(is_ask, _, _)| *is_ask));
    }

    #[tokio::test(start_paused = true)]
    async fn replay_is_deterministic_for_identical_input() {
        let first = run(config(), &frames(), 1_000.0, check).await;
        first.assert_complete();
        let second = run(config(), &frames(), 1_000.0, check).await;
        second.assert_complete();
        assert_eq!(first, second);
    }

    /// Everything a duplicate fill could corrupt, compared exactly.
    fn ledger(tracker: &OrderTracker) -> (u64, f64, f64, f64, (f64, f64, f64)) {
        let (fills, fees) = tracker.total_fill_stats();
        (
            fills,
            fees,
            tracker.confirmed_position(),
            tracker.trade_cash_flow(),
            tracker.exposure_bounds(),
        )
    }

    /// Duplicates keep the original order/client/exchange/trade identity.
    /// A stale-sequence redelivery and a fresh-sequence republish of both a
    /// partial and a terminal fill change nothing; a genuine second partial
    /// of the same order with a new trade id is counted exactly once.
    #[tokio::test(start_paused = true)]
    async fn redelivered_fills_keep_identity_and_never_double_count() {
        use std::cell::Cell;
        let redelivered = Cell::new(0usize);
        let (partials, terminals) = (Cell::new(0u32), Cell::new(0u32));
        let control_done = Cell::new(false);
        // The control partials the top bid twice; the gap then trades through
        // both bids, completing the top one and fully filling the second.
        let mut frames = vec![quiet(1_000)];
        frames.push(frame(1993.0, 1.0));
        frames.extend(vec![frame(1993.0, 1.0); 2]);
        let outcome = run(config(), &frames, 1_000.0, |index, venue, tracker| {
            check(index, venue, tracker);
            if !control_done.get() {
                control_done.set(true);
                // Control: two real partials of one resting bid, distinct trades.
                let order = *venue
                    .book
                    .lock()
                    .values()
                    .find(|o| !o.is_ask)
                    .expect("resting bid");
                let first = ((order.remaining * 0.4) * 1e8).floor() / 1e8;
                let before = ledger(tracker);
                venue.fill(order, order.price, first, false);
                let after_first = ledger(tracker);
                assert_eq!(after_first.0, before.0 + 1);
                assert!((after_first.2 - before.2 - first).abs() < 1e-9);
                let rest = *venue
                    .book
                    .lock()
                    .get(&(order.exchange_order_id as i64))
                    .expect("partial remainder rests");
                let second = ((rest.remaining * 0.5) * 1e8).floor() / 1e8;
                venue.fill(rest, rest.price, second, false);
                let after_second = ledger(tracker);
                assert_eq!(after_second.0, before.0 + 2, "second partial dropped");
                assert!((after_second.2 - before.2 - first - second).abs() < 1e-9);
                // Reserved buy exposure shrinks by exactly what filled.
                let reserved = |l: &(u64, f64, f64, f64, (f64, f64, f64))| l.4.2 - l.2;
                assert!(
                    (reserved(&before) - reserved(&after_second) - first - second).abs() < 1e-9
                );
            }
            let events = venue.fill_events.lock().clone();
            for event in &events[redelivered.get()..] {
                let before = ledger(tracker);
                venue
                    .redeliver(event)
                    .expect("stale redelivery rejected as invalid");
                assert_eq!(ledger(tracker), before, "cycle {index}: stale redelivery");
                venue
                    .republish(event)
                    .expect("trade-id duplicate rejected as invalid");
                assert_eq!(ledger(tracker), before, "cycle {index}: republished trade");
                assert!(!tracker.needs_reconciliation(), "duplicate forced recovery");
                if event.remaining_size > 0.0 {
                    partials.set(partials.get() + 1);
                } else {
                    terminals.set(terminals.get() + 1);
                }
            }
            redelivered.set(events.len());
        })
        .await;
        outcome.assert_complete();
        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert!(partials.get() >= 2 && terminals.get() >= 2);
        // Venue ledger (each real fill once) still equals the tracker's.
        assert_eq!(outcome.fills.len(), redelivered.get());
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_losing_race_to_fill_keeps_accounting_exact() {
        let outcome = run(config(), &frames(), 1_000.0, |index, venue, tracker| {
            check(index, venue, tracker);
            // From the first gap on, every cancel is beaten by a fill.
            if index == 3 {
                venue
                    .fill_before_cancel
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
        })
        .await;
        outcome.assert_complete();
        assert!(outcome.cancel_misses > 0, "no cancel raced a fill");
        assert!(outcome.cycles.iter().all(|c| *c == CycleOutcome::Quoted));
    }

    #[tokio::test(start_paused = true)]
    async fn event_gap_forces_confirmed_cleanup_before_requoting() {
        let quiet: Vec<Frame> = frames().into_iter().take(4).collect();
        let outcome = run(config(), &quiet, 1_000.0, |index, venue, tracker| {
            check(index, venue, tracker);
            if index == 1 {
                // A lost event, then the venue cancels one quote out of band.
                venue.skip_sequence();
                let id = *venue.book.lock().keys().next().expect("resting quote");
                venue.cancel(id);
                assert!(tracker.needs_reconciliation());
            }
        })
        .await;
        outcome.assert_complete();
        // Cleanup confirmed a flat, empty book and quoting resumed.
        assert!(outcome.cycles.iter().all(|c| *c == CycleOutcome::Quoted));
        assert!(outcome.fills.is_empty());
        assert!(!outcome.resting.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn quiet_book_quotes_both_sides_without_fills() {
        let quiet: Vec<Frame> = frames().into_iter().take(4).collect();
        let outcome = run(config(), &quiet, 1_000.0, check).await;
        outcome.assert_complete();
        assert!(outcome.fills.is_empty());
        let bids = outcome.resting.iter().filter(|(a, _, _)| !a).count();
        let asks = outcome.resting.iter().filter(|(a, _, _)| *a).count();
        assert!(bids > 0 && asks > 0, "resting={:?}", outcome.resting);
        // Maker quotes stay outside the touch.
        for (is_ask, price, _) in &outcome.resting {
            if *is_ask {
                assert!(*price as f64 / 1e4 > 1999.5);
            } else {
                assert!((*price as f64 / 1e4) < 2000.5);
            }
        }
    }

    // ─── Time-axis scenarios on the paused replay clock ─────────────────

    fn quiet(advance_ms: u64) -> Frame {
        Frame {
            advance_ms,
            ..frame(2000.0, 1.0)
        }
    }

    fn places(actions: &[Action]) -> Vec<u64> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Place { at_ms, .. } => Some(*at_ms),
                _ => None,
            })
            .collect()
    }

    fn cancels(actions: &[Action]) -> Vec<u64> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Cancel { at_ms, .. } => Some(*at_ms),
                _ => None,
            })
            .collect()
    }

    /// Calm requotes wait for max(order_ttl_secs, 8 s) of resting time:
    /// the same drifted signal is held before the boundary and replaced at it.
    #[tokio::test(start_paused = true)]
    async fn requote_waits_for_minimum_resting_lifetime() {
        // Quotes placed at t=1 s; then the fair value moves 3.5 bps.
        let mut frames = vec![quiet(1_000)];
        frames.push(Frame {
            advance_ms: 1_000,
            ..frame(2000.7, 1.0)
        });
        // Hold the moved signal, stepping to just before 1 s + 8 s.
        frames.push(Frame {
            advance_ms: 6_999,
            ..frame(2000.7, 1.0)
        });
        let before = run(config(), &frames, 1_000.0, check).await;
        before.assert_complete();
        assert!(before.error.is_none(), "{:?}", before.error);
        assert!(cancels(&before.actions).is_empty(), "{:?}", before.actions);
        // One more millisecond reaches the boundary and the requote happens.
        frames.push(Frame {
            advance_ms: 1,
            ..frame(2000.7, 1.0)
        });
        let after = run(config(), &frames, 1_000.0, check).await;
        after.assert_complete();
        assert_eq!(cancels(&after.actions).first(), Some(&9_000));
        // A shorter configured lifetime does not undercut the calm 8 s floor.
        let mut short_ttl = config();
        short_ttl.order_ttl_secs = 1;
        let floor = run(short_ttl, &frames[..3], 1_000.0, check).await;
        floor.assert_complete();
        assert!(cancels(&floor.actions).is_empty());
    }

    /// Inventory older than position_timeout_secs is flattened by a
    /// reduce-only IOC; adding to the position does not renew the clock.
    #[tokio::test(start_paused = true)]
    async fn position_timeout_flattens_at_the_boundary_and_is_not_renewed() {
        let timeout_ms = config().position_timeout_secs * 1_000;
        // Gap through both bids at t=2 s, filling 30% so the position stays
        // under its cap; the strategy then requotes bids lower.
        let mut frames = vec![quiet(1_000)];
        frames.push(Frame {
            advance_ms: 1_000,
            ..frame(1993.0, 0.3)
        });
        let opened_ms = 2_000;
        frames.push(Frame {
            advance_ms: 10_000,
            ..frame(1985.0, 1.0)
        });
        // A second gap at t=13 s fills the re-placed bids: same-side
        // inventory is added 11 s into the episode.
        frames.push(Frame {
            advance_ms: 1_000,
            ..frame(1978.0, 1.0)
        });
        let added_ms = 13_000;
        let hold = |until_ms: u64, at_ms: u64| Frame {
            advance_ms: until_ms - at_ms,
            ..frame(1978.0, 1.0)
        };
        let mut before = frames.clone();
        before.push(hold(opened_ms + timeout_ms - 1, added_ms));
        let early = run(config(), &before, 1_000.0, check).await;
        early.assert_complete();
        assert!(early.error.is_none(), "{:?}", early.error);
        // Both gaps really filled bids: the episode started at 2 s and grew at 12 s.
        let fill_times: Vec<u64> = early
            .actions
            .iter()
            .filter_map(|a| match a {
                Action::Fill {
                    at_ms,
                    is_ask: false,
                    ..
                } => Some(*at_ms),
                _ => None,
            })
            .collect();
        assert!(
            fill_times.contains(&opened_ms) && fill_times.contains(&added_ms),
            "{fill_times:?}"
        );
        assert!(
            !early
                .actions
                .iter()
                .any(|a| matches!(a, Action::Place { ioc: true, .. })),
            "flattened before timeout"
        );
        assert!(early.position > 0);
        // At the boundary measured from the first fill, not the added one.
        let mut at = frames.clone();
        at.push(hold(opened_ms + timeout_ms, added_ms));
        let flat = run(config(), &at, 1_000.0, check).await;
        flat.assert_complete();
        let ioc = flat
            .actions
            .iter()
            .find_map(|a| match a {
                Action::Place {
                    at_ms,
                    is_ask,
                    ioc: true,
                    ..
                } => Some((*at_ms, *is_ask)),
                _ => None,
            })
            .expect("no timeout flatten");
        assert_eq!(ioc, (opened_ms + timeout_ms, true));
        assert!(
            flat.actions
                .iter()
                .any(|a| matches!(a, Action::Fill { taker: true, .. }))
        );
        // IOC size is rounded down to step_size; only a sub-step dust remains.
        let step_e8 = e8(config().step_size);
        assert!(
            flat.position.abs() < step_e8,
            "reduce-only IOC left {} (e-8)",
            flat.position
        );
        assert!(!flat.needs_reconciliation);
    }

    /// Source-timestamp aging: fresh → frozen (quotes kept, no new orders) →
    /// cancel at stale_bbo_cancel_ms → requote only after the feed recovers.
    #[tokio::test(start_paused = true)]
    async fn stale_bbo_freezes_then_cancels_and_recovers() {
        let cfg = config();
        let silent = Frame {
            bbo_live: false,
            ..quiet(1_000)
        };
        // Last BBO published at t=1 s; silence until just under the cancel age.
        let mut frames = vec![quiet(1_000)];
        let quiet_secs = cfg.stale_bbo_cancel_ms / 1_000 - 2;
        frames.extend(vec![silent; quiet_secs as usize]);
        let frozen = run(cfg.clone(), &frames, 1_000.0, check).await;
        frozen.assert_complete();
        assert!(frozen.error.is_none(), "{:?}", frozen.error);
        assert!(
            cancels(&frozen.actions).is_empty(),
            "freeze must not cancel"
        );
        assert_eq!(places(&frozen.actions), vec![1_000; 4]);
        assert!(
            frozen
                .cycles
                .iter()
                .any(|c| matches!(c, CycleOutcome::Backoff(_))),
            "freeze path not reached"
        );
        // Keep the feed silent past the cancel age: every quote is pulled.
        frames.extend(vec![silent; 4]);
        let pulled = run(cfg.clone(), &frames, 1_000.0, check).await;
        pulled.assert_complete();
        let pulled_at = cancels(&pulled.actions);
        assert_eq!(pulled_at.len(), 4);
        assert!(
            pulled_at
                .iter()
                .all(|t| *t >= 1_000 + cfg.stale_bbo_cancel_ms)
        );
        assert!(pulled.resting.is_empty());
        assert_eq!(places(&pulled.actions).len(), 4, "no quotes while stale");
        // Recovery: a fresh BBO restores two-sided quoting.
        frames.push(quiet(1_000));
        let recovered = run(cfg, &frames, 1_000.0, check).await;
        recovered.assert_complete();
        assert_eq!(places(&recovered.actions).len(), 8);
        assert_eq!(recovered.cycles.last(), Some(&CycleOutcome::Quoted));
        assert_eq!(recovered.resting.len(), 4);
    }

    /// Account snapshots older than 30 s, or stamped in the future, withdraw
    /// all quotes; quoting resumes only with a fresh in-domain snapshot.
    #[tokio::test(start_paused = true)]
    async fn stale_or_future_account_snapshot_withdraws_quotes() {
        let silent = Frame {
            account_live: false,
            ..quiet(1_000)
        };
        let mut frames = vec![quiet(1_000)];
        frames.extend(vec![silent; 29]); // last snapshot at t=1 s, now t=30 s
        let aging = run(config(), &frames, 1_000.0, check).await;
        aging.assert_complete();
        assert!(aging.cycles.iter().all(|c| *c == CycleOutcome::Quoted));
        assert_eq!(aging.resting.len(), 4);
        frames.extend(vec![silent; 2]); // age crosses 30 s
        let stale = run(config(), &frames, 1_000.0, check).await;
        stale.assert_complete();
        assert!(stale.resting.is_empty(), "stale account kept quotes");
        assert_eq!(
            stale.cycles.last(),
            Some(&CycleOutcome::Backoff(Duration::from_secs(1)))
        );
        frames.push(quiet(1_000));
        let fresh = run(config(), &frames, 1_000.0, check).await;
        fresh.assert_complete();
        assert_eq!(fresh.cycles.last(), Some(&CycleOutcome::Quoted));
        assert_eq!(fresh.resting.len(), 4);

        // A snapshot from the future is not "fresher": it is rejected.
        let mut future = vec![quiet(1_000), quiet(1_000)];
        future.push(Frame {
            account_ahead_ms: 5_000,
            ..quiet(1_000)
        });
        let skewed = run(config(), &future, 1_000.0, check).await;
        skewed.assert_complete();
        assert!(skewed.resting.is_empty());
        assert_eq!(
            skewed.cycles.last(),
            Some(&CycleOutcome::Backoff(Duration::from_secs(1)))
        );
    }

    /// After an event gap, cleanup cancels everything but may release
    /// reserved exposure only once an account snapshot newer than the cancel
    /// arrives. Without one it fails closed and keeps the reservation.
    #[tokio::test(start_paused = true)]
    async fn cleanup_barrier_requires_snapshot_newer_than_cancel() {
        let gap = |index: usize, venue: &SimVenue, tracker: &OrderTracker| {
            check(index, venue, tracker);
            if index == 0 {
                venue.skip_sequence();
                let id = *venue.book.lock().keys().next().expect("resting quote");
                venue.cancel(id);
            }
        };
        // Account feed silent during the recovery cycle: only the snapshot
        // published before the cancel exists, so the barrier must hold.
        let blocked_frames = vec![
            quiet(1_000),
            Frame {
                account_live: false,
                ..quiet(1_000)
            },
        ];
        let blocked = run(config(), &blocked_frames, 1_000.0, gap).await;
        // Recovery cycle (frame 1) fails closed; frame 0 completed.
        blocked.assert_failed_at(1, "fresh equity and position snapshot");
        let error = blocked
            .error
            .expect("cleanup released without a fresh snapshot");
        assert!(
            error.contains("fresh equity and position snapshot"),
            "{error}"
        );
        assert!(blocked.needs_reconciliation, "reservation released");
        assert!(blocked.resting.is_empty(), "cleanup still pulled quotes");
        // Same gap with the feed running: a post-cancel snapshot arrives and
        // cleanup confirms, then quoting resumes.
        let live = run(config(), &[quiet(1_000), quiet(1_000)], 1_000.0, gap).await;
        live.assert_complete();
        assert!(live.error.is_none(), "{:?}", live.error);
        assert!(!live.needs_reconciliation);
        assert_eq!(live.cycles.last(), Some(&CycleOutcome::Quoted));
        assert_eq!(live.resting.len(), 4);
    }

    /// Same timed input → identical action trace and per-cycle state track.
    #[tokio::test(start_paused = true)]
    async fn timed_replay_trace_is_reproducible() {
        let mut frames = vec![quiet(1_000), frame(1993.0, 0.5), frame(1993.0, 1.0)];
        frames.extend(vec![frame(1993.0, 1.0); 70]);
        let first = run(config(), &frames, 1_000.0, check).await;
        first.assert_complete();
        let second = run(config(), &frames, 1_000.0, check).await;
        second.assert_complete();
        assert!(
            first
                .actions
                .iter()
                .any(|a| matches!(a, Action::Fill { taker: true, .. }))
        );
        assert_eq!(first.actions, second.actions);
        assert_eq!(first.states, second.states);
        assert_eq!(first, second);
    }

    /// Real sample → export → sink path: snapshots written later re-export
    /// the same price evidence; only a new accepted sample changes it.
    /// Real `calculate_pricing_inputs` over SHM BBO and depth slots whose
    /// timestamps disagree: depth-weighted marks carry the older of both
    /// inputs, missing depth falls back to the book with its own time.
    #[test]
    fn depth_mark_evidence_covers_every_input_it_used() {
        use crate::shm_depth_reader::ShmDepthReader;
        let shm = ShmFiles::create();
        let depth_path = shm.path("depth");
        std::fs::write(
            &depth_path,
            vec![0u8; NUM_SYMBOLS * 8 + NUM_SYMBOLS * NUM_EXCHANGES * 256],
        )
        .unwrap();
        let write_depth = |ts_ns: u64, size: f64| {
            let mut slot = [0u8; 256];
            slot[0..4].copy_from_slice(&2u32.to_le_bytes());
            slot[4] = LIGHTER;
            // repr(C) layout: symbol_id at 6, timestamp_ns at 16.
            slot[6..8].copy_from_slice(&SYMBOL.to_le_bytes());
            slot[16..24].copy_from_slice(&ts_ns.to_le_bytes());
            for level in 0..5 {
                let bid = 24 + level * 16;
                let ask = 104 + level * 16;
                slot[bid..bid + 8].copy_from_slice(&(1999.5 - level as f64).to_le_bytes());
                // Thin bids, heavy asks: VWMicro leans toward the bid.
                slot[bid + 8..bid + 16].copy_from_slice(&size.to_le_bytes());
                slot[ask..ask + 8].copy_from_slice(&(2000.5 + level as f64).to_le_bytes());
                slot[ask + 8..ask + 16].copy_from_slice(&(3.0 * size).to_le_bytes());
            }
            let offset = (NUM_SYMBOLS * 8) as u64
                + (SYMBOL as u64 * NUM_EXCHANGES as u64 + LIGHTER as u64) * 256;
            ShmFiles::write_at(&depth_path, offset, &slot);
        };
        let reader = ShmReader::open(shm.path("matrix").to_str().unwrap(), NUM_SYMBOLS).unwrap();
        let account = AccountStatsReader::open(shm.path("account").to_str().unwrap()).unwrap();
        let depth = ShmDepthReader::open(depth_path.to_str().unwrap(), NUM_SYMBOLS).unwrap();
        let tracker = Arc::new(OrderTracker::new());
        let venue = Arc::new(SimVenue::new(Arc::clone(&tracker), 0.0));
        let mut strategy = InventoryNeutralMM::with_data_sources(
            InventoryNeutralMMConfig {
                use_depth_pricing: true,
                poll_interval_ms: 0, // accept every sample
                ..config()
            },
            venue as Arc<dyn Exchange>,
            tracker,
            reader,
            account,
            Some(depth),
        );
        strategy.use_local_fair_value();
        let now = wall_ns();
        let ms = |ns: u64| ns / 1_000_000;
        let bbo = |ts_ns: u64| ShmBboMessage {
            bid_price: 1999.5,
            ask_price: 2000.5,
            bid_size: 5.0,
            ask_size: 5.0,
            timestamp_ns: ts_ns,
            ..Default::default()
        };
        let none = [(0u8, ShmBboMessage::default()); NUM_EXCHANGES];
        let mut sample = |book: ShmBboMessage| {
            let inputs = strategy.calculate_pricing_inputs(&none, &book);
            let evidence = strategy.price_evidence.expect("sample accepted");
            assert_eq!(evidence.mid, inputs.pricing_mid);
            evidence
        };
        // Depth newer than the BBO: the BBO touches bound the age.
        write_depth(now - 100_000_000, 1.0);
        let older_bbo = sample(bbo(now - 2_000_000_000));
        assert_eq!(older_bbo.source, "lighter_depth");
        assert!(
            older_bbo.mid < 2000.0,
            "depth weighting applied: {}",
            older_bbo.mid
        );
        assert_eq!(older_bbo.feed_ms, Some(ms(now - 2_000_000_000)));
        // BBO newer than the depth: the depth snapshot bounds the age.
        write_depth(now - 3_000_000_000, 1.0);
        let older_depth = sample(bbo(now - 100_000_000));
        assert_eq!(older_depth.source, "lighter_depth");
        assert_eq!(older_depth.mid, older_bbo.mid, "price math unchanged");
        assert_eq!(older_depth.feed_ms, Some(ms(now - 3_000_000_000)));
        // BBO time unknown (0): depth time alone cannot make it known.
        write_depth(now - 100_000_000, 1.0);
        let unknown = sample(bbo(0));
        assert_eq!(unknown.source, "lighter_depth");
        assert_eq!(unknown.feed_ms, None);
        // Depth present but without notional: the real fallback is the
        // touch mid, labelled as the book with the BBO time.
        write_depth(now - 100_000_000, 0.0);
        let fallback = sample(bbo(now - 500_000_000));
        assert_eq!((fallback.source, fallback.mid), ("lighter_book", 2000.0));
        assert_eq!(fallback.feed_ms, Some(ms(now - 500_000_000)));
        // Stale depth is skipped entirely: book source and BBO time.
        write_depth(now - 60_000_000_000, 1.0);
        let stale = sample(bbo(now - 200_000_000));
        assert_eq!(
            (stale.source, stale.feed_ms),
            ("lighter_book", Some(ms(now - 200_000_000)))
        );
        // Re-sampling the same inputs later never makes them newer.
        let again = sample(bbo(now - 200_000_000));
        assert_eq!(again.feed_ms, stale.feed_ms);
        assert!(again.sampled_ms >= stale.sampled_ms);
    }

    // ─── Execution guards on the production path ────────────────────────

    /// Wide inventory deadband (0.1 base at 2000) so a single top-of-book
    /// fill stays inside the guards' inventory limit (1.5 x deadband).
    fn guard_config() -> InventoryNeutralMMConfig {
        InventoryNeutralMMConfig {
            max_position: 1.0,
            inventory_urgency_threshold: 0.5,
            min_inventory_notional_usd: 200.0,
            ..config()
        }
    }

    fn fast(advance_ms: u64) -> Frame {
        Frame {
            advance_ms,
            ..frame(2000.0, 1.0)
        }
    }

    /// Quiet two-sided book for 6 s, then only the Lighter book dips through
    /// the top bid (fair value unchanged), then `tail` fast frames.
    fn post_fill_frames(tail: &[u64]) -> Vec<Frame> {
        let mut frames = vec![quiet(1_000); 6];
        frames.push(Frame {
            lighter_bid: 1995.3,
            lighter_ask: 1995.8,
            ..quiet(1_000)
        });
        frames.extend(tail.iter().map(|ms| fast(*ms)));
        frames
    }

    /// A real maker fill must hold small same-side replenishment for the
    /// 500 ms cooldown (boundary inclusive) and release it right after.
    #[tokio::test(start_paused = true)]
    async fn post_fill_replenishment_waits_for_the_cooldown() {
        let frames = post_fill_frames(&[100, 100, 100, 100, 100, 100, 100]);
        let outcome = run(guard_config(), &frames, 100.0, check).await;
        outcome.assert_complete();
        assert!(outcome.cycles.iter().all(|c| *c == CycleOutcome::Quoted));
        let fills: Vec<_> = outcome
            .actions
            .iter()
            .filter(|a| matches!(a, Action::Fill { .. }))
            .collect();
        assert_eq!(fills.len(), 1, "one maker fill at 7 s: {fills:?}");
        assert!(matches!(
            fills[0],
            Action::Fill {
                at_ms: 7_000,
                is_ask: false,
                taker: false,
                ..
            }
        ));
        // Replenishment of the filled bid: no placement at 7.0-7.5 s
        // (elapsed <= 500 ms), then exactly one at 7.6 s.
        let after_fill: Vec<u64> = places(&outcome.actions)
            .into_iter()
            .filter(|t| *t >= 7_000)
            .collect();
        assert_eq!(after_fill, vec![7_600], "{:?}", outcome.actions);
        assert!(cancels(&outcome.actions).is_empty());
        // Inventory stays the single fill; three quotes rest until refill.
        let (_, position, _, _) = outcome.states[6];
        assert_eq!(position, 4_570_000);
        let resting: Vec<usize> = outcome.states.iter().map(|s| s.3).collect();
        assert_eq!(resting, vec![4, 4, 4, 4, 4, 4, 3, 3, 3, 3, 3, 3, 4, 4]);
    }

    /// After a real batch (1 s) and a real fill (2 s) with three quotes
    /// still resting, the one-order refill is a micro-refresh: held until
    /// 4 s after the batch (boundary inclusive), long after the 500 ms
    /// post-fill window, then placed on the next cycle.
    #[tokio::test(start_paused = true)]
    async fn micro_refresh_waits_for_the_batch_cooldown() {
        let mut frames = vec![
            quiet(1_000),
            Frame {
                lighter_bid: 1995.3,
                lighter_ask: 1995.8,
                ..quiet(1_000)
            },
        ];
        frames.extend(vec![fast(100); 40]);
        let outcome = run(guard_config(), &frames, 100.0, check).await;
        outcome.assert_complete();
        assert!(outcome.cycles.iter().all(|c| *c == CycleOutcome::Quoted));
        assert_eq!(
            places(&outcome.actions),
            vec![1_000, 1_000, 1_000, 1_000, 5_100],
            "{:?}",
            outcome.actions
        );
        assert!(cancels(&outcome.actions).is_empty());
        // Three quotes rest from the fill (2 s) through the 5 s boundary.
        for (at, _, _, resting) in &outcome.states {
            let expected = match *at {
                ..2_000 => 4,
                2_000..=5_000 => 3,
                _ => 4,
            };
            assert_eq!(*resting, expected, "resting at {at} ms");
        }
        assert!(outcome.states.iter().skip(1).all(|s| s.1 == 4_570_000));
    }

    /// The guards are inventory-bounded optimisations: the same fill with a
    /// narrow deadband (default 10 USD) is rebalanced in the fill cycle.
    #[tokio::test(start_paused = true)]
    async fn inventory_outside_the_guard_limit_is_requoted_immediately() {
        let frames = post_fill_frames(&[100, 100]);
        let outcome = run(config(), &frames, 100.0, check).await;
        outcome.assert_complete();
        let after_fill: Vec<u64> = places(&outcome.actions)
            .into_iter()
            .filter(|t| *t >= 7_000)
            .collect();
        assert!(
            !after_fill.is_empty() && after_fill.iter().all(|t| *t == 7_000),
            "{:?}",
            outcome.actions
        );
        assert!(cancels(&outcome.actions).iter().all(|t| *t == 7_000));
        assert!(!cancels(&outcome.actions).is_empty());
    }

    /// Inside the post-fill window a fair-value jump that leaves resting
    /// quotes below the maker edge floor still cancels them at once.
    #[tokio::test(start_paused = true)]
    async fn edge_floor_cancels_during_the_post_fill_window() {
        let mut frames = post_fill_frames(&[100]);
        let jump = Frame {
            external_mid: 2012.0,
            lighter_bid: 2011.5,
            lighter_ask: 2012.5,
            advance_ms: 100,
            ..frame(2000.0, 0.0)
        };
        frames.extend([jump, jump]);
        let outcome = run(guard_config(), &frames, 100.0, check).await;
        outcome.assert_complete();
        // Fill at 7.0 s; the jump arrives at 7.2 s, inside the 500 ms window.
        assert_eq!(cancels(&outcome.actions), vec![7_200; 3]);
        assert_eq!(outcome.states[8].3, 0, "all quotes pulled at 7.2 s");
        assert_eq!(outcome.states[8].1, 4_570_000, "inventory unchanged");
    }

    /// Frozen input → identical trace, fills, cash and position, guards on.
    #[tokio::test(start_paused = true)]
    async fn guard_scenarios_replay_identically() {
        let mut frames = post_fill_frames(&[100, 100, 100, 100, 100, 100, 100]);
        frames.extend(vec![fast(500); 10]);
        let first = run(guard_config(), &frames, 100.0, check).await;
        let second = run(guard_config(), &frames, 100.0, check).await;
        first.assert_complete();
        second.assert_complete();
        assert_eq!(first, second);
    }

    #[tokio::test(start_paused = true)]
    async fn exported_mark_evidence_ages_instead_of_refreshing() {
        let cfg = config();
        let dir = std::env::temp_dir().join(format!("aleph-mark-{}", uuid::Uuid::new_v4()));
        let info = crate::telemetry_sink::SessionInfo {
            session_id: "mark".into(),
            network: "Testnet".into(),
            exchange: "lighter".into(),
            symbol: "ETH".into(),
            market_id: 0,
            account: 7,
            hedge_account: None,
        };
        let sink = crate::telemetry_sink::TelemetrySink::open(&dir, &info).unwrap();
        let silent = Frame {
            bbo_live: false,
            ..quiet(1_000)
        };
        // t=1 s live; silent until past local staleness; then a new book.
        let mut frames = vec![quiet(1_000)];
        frames.extend(vec![silent; 8]);
        frames.push(Frame {
            external_mid: 2003.0,
            ..quiet(1_000)
        });
        let mut published = Vec::new();
        let outcome = run_with(
            cfg.clone(),
            &frames,
            1_000.0,
            Some(sink.clone()),
            check,
            |index, strategy| {
                // Two exports per cycle: writing must not refresh evidence.
                strategy.export_snapshot(0.0);
                strategy.export_snapshot(0.0);
                published.push((index, wall_ns() / 1_000_000));
            },
        )
        .await;
        outcome.assert_complete();
        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        sink.stopped().await.unwrap();
        let text = std::fs::read_to_string(dir.join("mark.jsonl")).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        let snapshots: Vec<serde_json::Value> = text
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .filter(|value| value["type"] == "snapshot")
            .collect();
        let (startup, snapshots) = snapshots.split_first().expect("startup snapshot");
        assert_eq!(startup["daily_loss_start_equity"], 1_000.0);
        assert_eq!(startup["daily_loss_limit_pct"], cfg.max_daily_loss_pct);
        assert_eq!(snapshots.len(), 2 * frames.len());
        let mark = |i: usize| {
            let s = &snapshots[2 * i];
            assert_eq!(s["mid"], snapshots[2 * i + 1]["mid"]);
            assert_eq!(
                s["mark_sampled_ms"],
                snapshots[2 * i + 1]["mark_sampled_ms"]
            );
            assert_eq!(s["mark_feed_ms"], snapshots[2 * i + 1]["mark_feed_ms"]);
            (
                s["mid"].as_f64().unwrap(),
                s["mark_source"].as_str().unwrap().to_owned(),
                s["mark_sampled_ms"].as_u64().unwrap(),
                s["mark_feed_ms"].as_u64().unwrap(),
            )
        };
        let (mid0, source0, sampled0, feed0) = mark(0);
        assert_eq!(source0, "external_median");
        assert!((mid0 - 2000.0).abs() < 1.0, "{mid0}");
        // Feed time is the published book time, never later than the sample.
        assert!(feed0 <= sampled0 && sampled0 <= published[0].1);
        // External books age out after external_staleness_ms (2 s): the
        // strategy falls back to the silent local book. Samples are newer,
        // the underlying book is not.
        let fallback = (1..frames.len() - 1)
            .find(|&i| mark(i).1 == "lighter_book")
            .expect("local fallback sample");
        let (_, _, sampled_fb, feed_fb) = mark(fallback);
        assert!(sampled_fb > sampled0);
        assert_eq!(feed_fb, feed0, "silent feed must keep its timestamp");
        // Past local staleness quoting freezes: no new sample, so the
        // exported evidence stays identical while snapshots keep coming.
        let frozen = frames.len() - 2;
        let (mid_frozen, _, sampled_frozen, feed_frozen) = mark(frozen);
        assert_eq!(feed_frozen, feed0);
        // The book behind the exported mark is older than the freeze age.
        assert!(published[frozen].1 - feed_frozen > cfg.local_staleness_ms);
        assert!(published[frozen].1 > sampled_frozen);
        assert_eq!(mark(frozen - 1), mark(frozen), "frozen evidence refreshed");
        assert!(mid_frozen > 0.0);
        // A new live book is a new sample with new feed time and source.
        let (mid_new, source_new, sampled_new, feed_new) = mark(frames.len() - 1);
        assert_eq!(source_new, "external_median");
        assert!(feed_new > feed0 && sampled_new > sampled_frozen);
        assert!(mid_new > mid0);
    }
}
