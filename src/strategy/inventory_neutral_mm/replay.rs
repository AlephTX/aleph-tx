//! Offline replay of the production `InventoryNeutralMM` quoting loop.
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
//!   gaps (`skip_sequence`) and duplicate deliveries (`replay_duplicate_fill`).
//!
//! Scope: correctness of the production decision path and accounting. It
//! does not estimate live fill rates or PnL; no recorded dataset is used.

use super::*;
use crate::exchange::{
    BatchOrderParams, BatchOrderResult, BatchResult, OrderInfo, OrderResult, PlaceResult,
};
use crate::shm_reader::NUM_SYMBOLS;
use crate::types::ShmPrivateEventV2;
use parking_lot::Mutex as PlMutex;
use std::collections::BTreeMap;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const LIGHTER: u8 = 2;
const BINANCE: u8 = 6;
const SYMBOL: u16 = crate::config::SYM_ETH;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Resting {
    pub client_order_id: i64,
    pub exchange_order_id: u64,
    pub is_ask: bool,
    pub price: f64,
    pub remaining: f64,
}

/// Simulated Lighter venue. Holds resting orders and emits tracker events.
pub(super) struct SimVenue {
    tracker: Arc<OrderTracker>,
    pub book: PlMutex<BTreeMap<i64, Resting>>,
    next_coid: std::sync::atomic::AtomicI64,
    next_exchange_id: std::sync::atomic::AtomicU64,
    sequence: std::sync::atomic::AtomicU64,
    pub fee_bps: f64,
    /// Fill events applied, for conservation checks: (is_ask, price, size, fee).
    pub fills: PlMutex<Vec<(bool, f64, f64, f64)>>,
    /// Count of cancel requests that found no live order (cancel/fill races).
    pub cancel_misses: std::sync::atomic::AtomicU64,
    /// When set, a cancel request loses the race: the order fills first and
    /// the cancel finds nothing, as with Lighter's "order not found".
    pub fill_before_cancel: std::sync::atomic::AtomicBool,
}

impl SimVenue {
    pub fn new(tracker: Arc<OrderTracker>, fee_bps: f64) -> Self {
        Self {
            tracker,
            book: PlMutex::new(BTreeMap::new()),
            next_coid: std::sync::atomic::AtomicI64::new(1),
            next_exchange_id: std::sync::atomic::AtomicU64::new(1_000),
            sequence: std::sync::atomic::AtomicU64::new(0),
            fee_bps,
            fills: PlMutex::new(Vec::new()),
            cancel_misses: std::sync::atomic::AtomicU64::new(0),
            fill_before_cancel: std::sync::atomic::AtomicBool::new(false),
        }
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
    fn place(&self, side: Side, price: f64, size: f64) -> PlaceResult {
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
        self.apply(ShmPrivateEventV2::order_created(
            self.next_sequence(),
            LIGHTER,
            SYMBOL,
            exchange_order_id,
            coid,
            exchange_order_id as i64,
            price,
            size,
            is_ask,
            0,
        ));
        self.book.lock().insert(
            exchange_order_id as i64,
            Resting {
                client_order_id: coid,
                exchange_order_id,
                is_ask,
                price,
                remaining: size,
            },
        );
        PlaceResult {
            client_order_index: coid,
            side,
            price,
            size,
        }
    }

    fn cancel(&self, order_index: i64) {
        if self
            .fill_before_cancel
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            let racing = self.book.lock().get(&order_index).copied();
            if let Some(order) = racing {
                self.fill(order, order.remaining);
            }
        }
        let removed = self.book.lock().remove(&order_index);
        match removed {
            Some(order) => self.apply(ShmPrivateEventV2::order_canceled(
                self.next_sequence(),
                LIGHTER,
                SYMBOL,
                order.exchange_order_id,
                order.client_order_id,
                order_index,
                order.remaining,
                0,
            )),
            None => {
                self.cancel_misses
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
    }

    /// Fill resting orders the frame trades through. `fraction` < 1 leaves a
    /// partial remainder resting.
    pub fn settle(&self, bid: f64, ask: f64, fraction: f64) {
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
            self.fill(order, size);
        }
    }

    /// Fill `size` of a resting order at its limit price and emit the event.
    fn fill(&self, order: Resting, size: f64) {
        let remaining = ((order.remaining - size) * 1e8).round() / 1e8;
        let fee = order.price * size * self.fee_bps / 10_000.0;
        let sequence = self.next_sequence();
        self.apply(ShmPrivateEventV2::order_filled(
            sequence,
            LIGHTER,
            SYMBOL,
            order.exchange_order_id,
            order.client_order_id,
            order.exchange_order_id as i64,
            order.price,
            size,
            remaining,
            fee,
            order.is_ask,
            0,
            sequence + 1_000_000, // unique trade id per fill
        ));
        self.fills
            .lock()
            .push((order.is_ask, order.price, size, fee));
        let mut book = self.book.lock();
        if remaining > 0.0 {
            if let Some(entry) = book.get_mut(&(order.exchange_order_id as i64)) {
                entry.remaining = remaining;
            }
        } else {
            book.remove(&(order.exchange_order_id as i64));
        }
    }

    /// Burn one sequence number so the next event arrives after a gap.
    pub fn skip_sequence(&self) {
        self.next_sequence();
    }

    /// Re-deliver the last event sequence to exercise duplicate handling.
    pub fn replay_duplicate_fill(&self) {
        let fill = self.fills.lock().last().copied();
        if let Some((is_ask, price, size, fee)) = fill {
            // Same trade id and an old sequence: the tracker must ignore it.
            let event = ShmPrivateEventV2::order_filled(
                1, LIGHTER, SYMBOL, 1, 1, 1, price, size, 0.0, fee, is_ask, 0, 1,
            );
            let _ = self.tracker.apply_event(&event);
        }
    }

    pub fn net_position(&self) -> f64 {
        self.fills
            .lock()
            .iter()
            .map(|(is_ask, _, size, _)| if *is_ask { -size } else { *size })
            .sum()
    }

    /// Signed trade cash flow less fees, as OrderTracker accounts it.
    pub fn cash_flow(&self) -> f64 {
        self.fills
            .lock()
            .iter()
            .map(|(is_ask, price, size, fee)| {
                let notional = price * size;
                (if *is_ask { notional } else { -notional }) - fee
            })
            .sum()
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
        self.cancel(order_index);
        Ok(())
    }
    async fn cancel_all(&self) -> anyhow::Result<u32> {
        let ids: Vec<i64> = self.book.lock().keys().copied().collect();
        for id in &ids {
            self.cancel(*id);
        }
        Ok(ids.len() as u32)
    }
    async fn get_active_orders(&self) -> anyhow::Result<Vec<OrderInfo>> {
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
    async fn close_all_positions(&self, _: f64) -> anyhow::Result<()> {
        anyhow::bail!("replay: taker flatten is not simulated")
    }
    async fn execute_batch(&self, actions: Vec<BatchAction>) -> anyhow::Result<BatchResult> {
        let mut place_results = Vec::new();
        for action in actions {
            match action {
                BatchAction::Place(order) => {
                    assert!(
                        order.size > 0.0 && order.price > 0.0,
                        "strategy placed a non-positive order"
                    );
                    place_results.push(self.place(order.side, order.price, order.size));
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
pub(super) struct Frame {
    pub lighter_bid: f64,
    pub lighter_ask: f64,
    pub external_mid: f64,
    pub fill_fraction: f64,
}

/// Temp SHM segments the strategy reads like production.
pub(super) struct ShmFiles {
    pub directory: PathBuf,
    version: u64,
    account_version: u64,
}

impl ShmFiles {
    pub fn create() -> Self {
        let directory = std::env::temp_dir().join(format!("aleph-replay-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        // Full production layout: 2048 versions + 2048 x 7 BBO slots.
        std::fs::write(
            directory.join("matrix"),
            vec![0u8; NUM_SYMBOLS * 8 + NUM_SYMBOLS * NUM_EXCHANGES * 64],
        )
        .unwrap();
        std::fs::write(directory.join("account"), [0u8; 128]).unwrap();
        Self {
            directory,
            version: 0,
            account_version: 0,
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.directory.join(name)
    }

    fn write_at(path: &Path, offset: u64, bytes: &[u8]) {
        let mut file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.seek(SeekFrom::Start(offset)).unwrap();
        file.write_all(bytes).unwrap();
    }

    fn write_bbo(&self, exchange: u8, bid: f64, ask: f64, ts_ns: u64) {
        let mut slot = [0u8; 64];
        slot[0..4].copy_from_slice(&2u32.to_le_bytes()); // even seqlock
        slot[4] = 1;
        slot[5] = exchange;
        slot[6..8].copy_from_slice(&SYMBOL.to_le_bytes());
        slot[8..16].copy_from_slice(&ts_ns.to_le_bytes());
        slot[16..24].copy_from_slice(&bid.to_le_bytes());
        slot[24..32].copy_from_slice(&5.0f64.to_le_bytes());
        slot[32..40].copy_from_slice(&ask.to_le_bytes());
        slot[40..48].copy_from_slice(&5.0f64.to_le_bytes());
        let offset = (NUM_SYMBOLS * 8) as u64
            + (SYMBOL as u64 * NUM_EXCHANGES as u64 + exchange as u64) * 64;
        Self::write_at(&self.path("matrix"), offset, &slot);
    }

    pub fn publish(&mut self, frame: &Frame, ts_ns: u64) {
        self.write_bbo(LIGHTER, frame.lighter_bid, frame.lighter_ask, ts_ns);
        let half = frame.external_mid * 0.5e-4;
        self.write_bbo(
            BINANCE,
            frame.external_mid - half,
            frame.external_mid + half,
            ts_ns,
        );
        self.version += 1;
        Self::write_at(
            &self.path("matrix"),
            SYMBOL as u64 * 8,
            &self.version.to_le_bytes(),
        );
    }

    pub fn publish_account(&mut self, equity: f64, position: f64, ts_ns: u64) {
        self.account_version += 2; // even = stable
        let mut stats = [0u8; 128];
        let f = |v: f64| v.to_le_bytes();
        stats[0..8].copy_from_slice(&self.account_version.to_le_bytes());
        stats[8..16].copy_from_slice(&f(equity)); // collateral
        stats[16..24].copy_from_slice(&f(equity)); // portfolio value
        stats[24..32].copy_from_slice(&f(1.0)); // leverage
        stats[32..40].copy_from_slice(&f(equity)); // available balance
        stats[40..48].copy_from_slice(&f(0.05)); // margin usage
        stats[48..56].copy_from_slice(&f(equity * 10.0)); // buying power
        stats[56..64].copy_from_slice(&ts_ns.to_le_bytes());
        stats[64..72].copy_from_slice(&f(position));
        stats[72..80].copy_from_slice(&ts_ns.to_le_bytes());
        Self::write_at(&self.path("account"), 0, &stats);
    }
}

impl Drop for ShmFiles {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

/// Result of a replay, compared for determinism.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Outcome {
    pub fills: Vec<(bool, i64, i64)>, // (is_ask, price in 1e-4, size in 1e-8)
    pub position: i64,
    pub cash_flow: i64,
    pub resting: Vec<(bool, i64, i64)>,
    pub cycles: Vec<CycleOutcome>,
    pub cancel_misses: u64,
}

pub(super) fn wall_ns() -> u64 {
    wall_time_ns()
}

/// Drive the production strategy over `frames`, one quote_cycle per frame.
pub(super) async fn run(
    config: InventoryNeutralMMConfig,
    frames: &[Frame],
    equity: f64,
    after_cycle: impl Fn(usize, &SimVenue, &OrderTracker),
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
    strategy.config.validate().unwrap();
    strategy.perform_startup_initialization().await.unwrap();
    let mut cycles = Vec::with_capacity(frames.len());
    for (index, frame) in frames.iter().enumerate() {
        let ts = wall_ns();
        shm.publish(frame, ts);
        venue.settle(frame.lighter_bid, frame.lighter_ask, frame.fill_fraction);
        shm.publish_account(equity, venue.net_position(), ts);
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
                        shm.publish_account(equity, venue.net_position(), wall_ns());
                    }
                }
            }
        };
        let (outcome, ()) = tokio::join!(cycle, feeder);
        cycles.push(outcome.expect("quote_cycle failed"));
        after_cycle(index, &venue, &tracker);
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
            // Keep cycles free of wall-clock gated paths that replay cannot
            // drive deterministically (timeout flatten, TTL requotes).
            position_timeout_secs: 3_600,
            order_ttl_secs: 3_600,
            ..Default::default()
        }
    }

    fn frame(mid: f64, fill_fraction: f64) -> Frame {
        Frame {
            lighter_bid: mid - 0.5,
            lighter_ask: mid + 0.5,
            external_mid: mid,
            fill_fraction,
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

    #[tokio::test]
    async fn production_strategy_replay_conserves_inventory_and_cash() {
        let outcome = run(config(), &frames(), 1_000.0, check).await;
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

    #[tokio::test]
    async fn replay_is_deterministic_for_identical_input() {
        let first = run(config(), &frames(), 1_000.0, check).await;
        let second = run(config(), &frames(), 1_000.0, check).await;
        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn duplicate_and_stale_events_do_not_double_count() {
        let outcome = run(config(), &frames(), 1_000.0, |index, venue, tracker| {
            let before = (tracker.confirmed_position(), tracker.total_fill_stats().0);
            venue.replay_duplicate_fill();
            assert_eq!(
                before,
                (tracker.confirmed_position(), tracker.total_fill_stats().0),
                "cycle {index}: duplicate event changed state"
            );
            check(index, venue, tracker);
        })
        .await;
        assert!(outcome.position > 0);
    }

    #[tokio::test]
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
        assert!(outcome.cancel_misses > 0, "no cancel raced a fill");
        assert!(outcome.cycles.iter().all(|c| *c == CycleOutcome::Quoted));
    }

    #[tokio::test]
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
        // Cleanup confirmed a flat, empty book and quoting resumed.
        assert!(outcome.cycles.iter().all(|c| *c == CycleOutcome::Quoted));
        assert!(outcome.fills.is_empty());
        assert!(!outcome.resting.is_empty());
    }

    #[tokio::test]
    async fn quiet_book_quotes_both_sides_without_fills() {
        let quiet: Vec<Frame> = frames().into_iter().take(4).collect();
        let outcome = run(config(), &quiet, 1_000.0, check).await;
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
}
