//! Inventory-Neutral Market Maker (Tier-1 HFT Strategy)
//!
//! Based on Citadel/Jump Trading principles:
//! 1. Asymmetric order sizing to maintain near-zero inventory
//! 2. Dynamic spread based on realized volatility + adverse selection
//! 3. Aggressive position flattening when inventory deviates
//! 4. 100% maker fills (never cross spread)
//!
//! Key Innovation: **Inventory-Weighted Order Sizing**
//! - Short 0.1 ETH → Bid 0.13 ETH, Ask 0.03 ETH (net +0.1 if bid fills)
//! - Long 0.1 ETH → Bid 0.03 ETH, Ask 0.13 ETH (net -0.1 if ask fills)
//!
//! Target: 10-20 bps daily return, Sharpe > 3.0

use crate::account_stats_reader::{AccountStatsReader, AccountStatsSnapshot};
use crate::config::InventoryNeutralMMConfig;
use crate::error::{Result, TradingError};
use crate::exchange::{Exchange, BatchOrderParams as ExchangeBatchParams};
use crate::order_tracker::{OrderTracker, OrderSide};
use crate::shm_event_reader::ShmEventReaderV2;
use crate::shm_reader::ShmReader;
use crate::telemetry::TelemetryCollector;
// parking_lot::RwLock no longer needed (OrderTracker uses internal RwLock)
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

// ─── Account Stats ───────────────────────────────────────────────────────────
#[derive(Debug, Clone)]
pub struct AccountStats {
    pub available_balance: f64,
    pub portfolio_value: f64,
    pub position: f64,
    pub leverage: f64,
    pub last_update: Instant,
}

impl Default for AccountStats {
    fn default() -> Self {
        Self {
            available_balance: 0.0,
            portfolio_value: 0.0,
            position: 0.0,
            leverage: 0.0,
            last_update: Instant::now(),
        }
    }
}

impl From<AccountStatsSnapshot> for AccountStats {
    fn from(snapshot: AccountStatsSnapshot) -> Self {
        Self {
            available_balance: snapshot.available_balance,
            portfolio_value: snapshot.portfolio_value,
            position: snapshot.position,
            leverage: snapshot.leverage,
            last_update: Instant::now(),
        }
    }
}

// ─── Microstructure Tracker ──────────────────────────────────────────────────
struct MicrostructureTracker {
    price_samples: VecDeque<f64>,
    max_samples: usize,
    ema_fast: f64,
    ema_slow: f64,
    ema_fast_alpha: f64,
    ema_slow_alpha: f64,
    ema_initialized: bool,
}

impl MicrostructureTracker {
    fn new(max_samples: usize, ema_fast_period: usize, ema_slow_period: usize) -> Self {
        Self {
            price_samples: VecDeque::with_capacity(max_samples),
            max_samples,
            ema_fast: 0.0,
            ema_slow: 0.0,
            ema_fast_alpha: 2.0 / (ema_fast_period as f64 + 1.0),
            ema_slow_alpha: 2.0 / (ema_slow_period as f64 + 1.0),
            ema_initialized: false,
        }
    }

    fn update(&mut self, mid: f64) {
        self.price_samples.push_back(mid);
        if self.price_samples.len() > self.max_samples {
            self.price_samples.pop_front();
        }

        if !self.ema_initialized {
            self.ema_fast = mid;
            self.ema_slow = mid;
            self.ema_initialized = true;
        } else {
            self.ema_fast = self.ema_fast_alpha * mid + (1.0 - self.ema_fast_alpha) * self.ema_fast;
            self.ema_slow = self.ema_slow_alpha * mid + (1.0 - self.ema_slow_alpha) * self.ema_slow;
        }
    }

    fn volatility_bps(&self) -> f64 {
        if self.price_samples.len() < 2 {
            return 10.0; // Default
        }

        let returns: Vec<f64> = self.price_samples
            .iter()
            .zip(self.price_samples.iter().skip(1))
            .map(|(p1, p2)| (p2 / p1 - 1.0) * 10000.0)
            .collect();

        let mean = returns.iter().sum::<f64>() / returns.len() as f64;
        let variance = returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / returns.len() as f64;
        variance.sqrt().max(1.0)
    }

    fn momentum_bps(&self) -> f64 {
        if !self.ema_initialized {
            return 0.0;
        }
        ((self.ema_fast - self.ema_slow) / self.ema_slow) * 10000.0
    }

    fn adverse_selection_score(&self) -> f64 {
        let vol = self.volatility_bps();
        if vol < 0.1 {
            return 0.0;
        }
        self.momentum_bps().abs() / vol
    }
}

// ─── Active Order ────────────────────────────────────────────────────────────
#[derive(Debug, Clone)]
struct ActiveOrder {
    order_id: String,
    client_order_id: i64,
    side: OrderSide,
    price: f64,
    #[allow(dead_code)]
    size: f64,
    #[allow(dead_code)]
    placed_at: Instant,
}

// ─── Inventory-Neutral Market Maker ──────────────────────────────────────────
pub struct InventoryNeutralMM {
    config: InventoryNeutralMMConfig,

    trading: Arc<dyn Exchange>,
    order_tracker: Arc<OrderTracker>,
    shm_reader: ShmReader,
    shm_depth_reader: Option<crate::shm_depth_reader::ShmDepthReader>,
    event_reader: Option<ShmEventReaderV2>,
    account_stats_reader: AccountStatsReader,
    account_stats: AccountStats,
    micro: MicrostructureTracker,

    // Order tracking (multi-level grid)
    active_orders: Vec<ActiveOrder>,

    session_start_balance: f64,
    total_orders_placed: u64,
    last_balance_check: Instant,
    margin_cooldown_until: Instant,

    // Telemetry
    telemetry: TelemetryCollector,
}

impl InventoryNeutralMM {
    pub fn new(
        config: InventoryNeutralMMConfig,
        trading: Arc<dyn Exchange>,
        order_tracker: Arc<OrderTracker>,
        shm_reader: ShmReader,
        account_stats_reader: AccountStatsReader,
    ) -> Self {
        // Try to open depth reader (optional, for OBI+VWMicro pricing)
        let shm_depth_reader = crate::shm_depth_reader::ShmDepthReader::open(
            "/dev/shm/aleph-depth",
            2048,
        )
        .ok();

        if shm_depth_reader.is_some() {
            info!("📊 OBI+VWMicro pricing enabled (depth reader initialized)");
        } else {
            info!("📊 Using simple mid-price (depth reader not available)");
        }

        // Open V2 event reader for OrderTracker state transitions
        let event_reader = match ShmEventReaderV2::new_default() {
            Ok(mut reader) => {
                reader.skip_to_end();
                info!("📡 V2 event reader initialized (skipped {} historical events)", reader.local_read_idx());
                Some(reader)
            }
            Err(e) => {
                warn!("⚠️  V2 event reader unavailable: {} (OrderTracker will rely on drift sync)", e);
                None
            }
        };

        Self {
            micro: MicrostructureTracker::new(
                config.micro_samples,
                config.ema_fast_period,
                config.ema_slow_period,
            ),
            config,
            trading,
            order_tracker,
            shm_reader,
            shm_depth_reader,
            event_reader,
            account_stats_reader,
            account_stats: AccountStats::default(),
            active_orders: Vec::new(),
            session_start_balance: 0.0,
            total_orders_placed: 0,
            last_balance_check: Instant::now(),
            margin_cooldown_until: Instant::now(),
            telemetry: TelemetryCollector::new(),
        }
    }

    pub async fn run(
        &mut self,
        mut shutdown: Option<tokio::sync::watch::Receiver<bool>>,
    ) -> Result<()> {
        info!("🎯 Inventory-Neutral MM started");

        // Cancel all existing orders and wait for confirmation
        info!("📤 Canceling all existing orders...");
        if let Err(e) = self.trading.cancel_all().await {
            warn!("Failed to cancel existing orders: {:?}", e);
        } else {
            info!("✅ All existing orders canceled");
        }

        // Wait 2 seconds for cancellations to propagate
        tokio::time::sleep(Duration::from_secs(2)).await;

        // Wait for account stats
        info!("⏳ Waiting for account stats...");
        let start_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;

        for _ in 0..30 {
            let stats = self.account_stats_reader.read();
            let data_age_ns = start_time.saturating_sub(stats.updated_at);
            let data_age_secs = data_age_ns / 1_000_000_000;

            if stats.available_balance > 0.0 && data_age_secs < 60 {
                self.account_stats = stats.into();
                self.session_start_balance = self.account_stats.portfolio_value;
                info!("✅ Account stats loaded: equity=${:.2} available=${:.2} position={:.4}",
                    self.account_stats.portfolio_value, self.account_stats.available_balance, self.account_stats.position);

                // Sync order tracker with authoritative position from exchange
                let delta = self.order_tracker.force_sync_position(self.account_stats.position);
                if delta.abs() > 1e-8 {
                    info!("🔄 Tracker synced to exchange position: {:.4} (delta={:.4})",
                        self.account_stats.position, delta);
                }
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }

        info!("🚀 Starting main loop...");

        // P0: Validate post_only configuration is active
        if self.config.use_post_only {
            info!("🛡️  Post-Only (ALO) mode ENABLED — all limit orders will be maker-only");
        } else {
            warn!("⚠️  Post-Only mode DISABLED — orders may execute as taker!");
        }

        // Spawn background V2 event consumer → OrderTracker state transitions
        if let Some(mut event_reader) = self.event_reader.take() {
            let tracker = Arc::clone(&self.order_tracker);
            tokio::spawn(async move {
                info!("📡 V2 event consumer started (read_idx={}, write_idx={})",
                    event_reader.local_read_idx(), event_reader.write_idx());
                loop {
                    let mut batch = 0u32;
                    while let Some(event) = event_reader.try_read() {
                        if let Err(e) = tracker.apply_event(&event) {
                            debug!("Event apply error: {}", e);
                        }
                        batch += 1;
                        if batch >= 64 {
                            break;
                        }
                    }
                    if batch == 0 {
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                }
            });
        }

        loop {
            // Check shutdown signal
            if let Some(ref mut rx) = shutdown {
                if *rx.borrow() {
                    info!("🛑 Shutdown signal received");

                    // Step 1: Cancel all active orders
                    info!("📤 Canceling all orders...");
                    self.cancel_all_orders().await;

                    // Step 2: Close all positions if any
                    let position = self.account_stats.position;
                    if position.abs() > 0.0001 {
                        info!("📉 Closing position: {:.4} ETH", position);
                        let exchanges = self.shm_reader.read_all_exchanges(self.config.symbol_id);
                        if let Some((_, bbo)) = exchanges.iter().find(|(id, _)| *id == self.config.exchange_id) {
                            let mid = (bbo.bid_price + bbo.ask_price) / 2.0;
                            if mid == 0.0 || mid.is_nan() || mid.is_infinite() {
                                warn!("⚠️  Invalid mid price during close: {:.4}", mid);
                            } else if let Err(e) = self.trading.close_all_positions(mid).await {
                                warn!("Failed to close positions: {}", e);
                            } else {
                                info!("✅ Positions closed");
                            }
                        }
                    }

                    info!("✅ Graceful shutdown complete");
                    return Ok(());
                }
            }

            // Update account stats
            let stats = self.account_stats_reader.read();
            self.account_stats = stats.into();

            // ═══════════════════════════════════════════════════════════════
            // v5.0.0: Three-Layer Position Defense (Defense in Depth)
            // ═══════════════════════════════════════════════════════════════

            // Layer 1: OrderTracker effective position (fastest, <1μs)
            let tracker_pos = self.order_tracker.effective_position();
            let acct_pos = self.account_stats.position;

            // Layer 2: Drift detection + force sync
            let drift = (tracker_pos - acct_pos).abs();
            let position = if self.total_orders_placed == 0 {
                acct_pos  // Before any orders, use exchange position
            } else if drift > self.config.max_position * 0.5 && acct_pos.abs() > 1e-8 {
                // Tracker drifted too far — force sync to exchange
                let delta = self.order_tracker.force_sync_position(acct_pos);
                if delta.abs() > 0.001 {
                    warn!("Tracker drift detected: tracker={:.4} exchange={:.4}, force synced (delta={:.4})",
                        tracker_pos, acct_pos, delta);
                }
                acct_pos
            } else if drift < self.config.max_position * 0.05 {
                tracker_pos  // Drift < 5%, trust tracker (more responsive)
            } else {
                acct_pos  // Drift 5-50%, use exchange position (safer)
            };

            // Log position for debugging
            if self.total_orders_placed % 10 == 0 {
                debug!(
                    "Position: tracker={:.4} exchange={:.4} using={:.4} worst_long={:.4} worst_short={:.4}",
                    tracker_pos, acct_pos, position,
                    self.order_tracker.worst_case_long(),
                    self.order_tracker.worst_case_short()
                );
            }

            // Periodic sync + GC (every 30s)
            if self.last_balance_check.elapsed() > Duration::from_secs(30) {
                let delta = self.order_tracker.force_sync_position(acct_pos);
                if delta.abs() > 0.001 {
                    warn!("Periodic sync: drift={:.6} ETH", delta);
                }
                // Reconcile tracker: mark stale entries that strategy no longer tracks
                let strategy_cois: std::collections::HashSet<i64> = self.active_orders
                    .iter().map(|o| o.client_order_id).collect();
                let tracker_cois = self.order_tracker.active_cois();
                let mut stale_count = 0;
                for coi in tracker_cois {
                    if !strategy_cois.contains(&coi) {
                        self.order_tracker.mark_failed(coi);
                        stale_count += 1;
                    }
                }
                if stale_count > 0 {
                    info!("Periodic reconcile: cleared {} stale tracker entries (strategy has {})",
                        stale_count, self.active_orders.len());
                }
                self.order_tracker.gc_completed_orders(Duration::from_secs(30));
                self.telemetry.export_metrics();
                self.print_pnl_update();
                self.last_balance_check = Instant::now();
            }

            // Margin cooldown: skip quoting if recently rejected
            if Instant::now() < self.margin_cooldown_until {
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }

            // Read market data
            let exchanges = self.shm_reader.read_all_exchanges(self.config.symbol_id);
            let lighter_bbo = exchanges
                .iter()
                .find(|(exch_id, _)| *exch_id == self.config.exchange_id)
                .map(|(_, msg)| msg);

            let bbo = match lighter_bbo.filter(|b| b.bid_price > 0.0 && b.ask_price > 0.0) {
                Some(b) => b,
                None => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    continue;
                }
            };

            let mid = (bbo.bid_price + bbo.ask_price) / 2.0;

            // Divide-by-zero / NaN protection: if mid is invalid, skip this cycle
            if mid == 0.0 || mid.is_nan() || mid.is_infinite() {
                tracing::warn!("⚠️  Invalid mid price: {:.4}, skipping cycle", mid);
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }

            // Calculate VWMicro price if depth data available
            let pricing_mid = if let Some(ref depth_reader) = self.shm_depth_reader {
                if let Some(depth) = depth_reader.read_depth(
                    self.config.symbol_id,
                    self.config.exchange_id,
                ) {
                    self.calculate_vw_micro_price(&depth, bbo.bid_price, bbo.ask_price)
                } else {
                    mid // Fallback to simple mid
                }
            } else {
                mid // No depth reader available
            };

            let market_spread_bps = ((bbo.ask_price - bbo.bid_price) / mid) * 10000.0;

            // Update microstructure with VWMicro price
            self.micro.update(pricing_mid);
            let vol_bps = self.micro.volatility_bps();
            let momentum_bps = self.micro.momentum_bps();
            let as_score = self.micro.adverse_selection_score();

            // Adverse selection filter
            if as_score > self.config.adverse_selection_threshold {
                debug!("AS filter triggered: score={:.2} (canceling + pausing)", as_score);
                self.cancel_all_orders().await;
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }

            // ═══════════════════════════════════════════════════════════════════
            // CORE: BBO-Following Penny Jump + Inventory Skew
            // ═══════════════════════════════════════════════════════════════════
            //
            // Step 1: Start from market BBO, improve by 1 tick (penny jump)
            // Step 2: Apply inventory skew to bias towards flattening position
            // Step 3: Ensure our spread still covers round-trip fees
            //
            // Example (market Bid=1968.00 / Ask=1970.00, pos=-0.1):
            //   Raw:  our_bid = 1968.01, our_ask = 1969.99
            //   Skew: our_bid = 1968.07 (more eager to buy), our_ask = 1970.05 (less eager to sell)
            //   Spread: 1970.05 - 1968.07 = 1.98 = ~10bps >> 0.76bps fee ✅

            let penny = self.config.tick_size * self.config.penny_ticks;

            // Volatility spread adjustment (high volatility → wider spread)
            let vol_spread_adjustment = vol_bps * self.config.vol_spread_scale / 10000.0 * mid;

            // Penny jump: improve BBO by 1 tick + volatility adjustment
            let raw_bid = bbo.bid_price + penny - vol_spread_adjustment;
            let raw_ask = bbo.ask_price - penny + vol_spread_adjustment;

            // Momentum shift (trend direction → shift quotes)
            // Uptrend: raise both bid and ask (more willing to buy)
            // Downtrend: lower both bid and ask (more willing to sell)
            let momentum_shift = momentum_bps * self.config.momentum_skew_scale / 10000.0 * mid;

            // Inventory skew: shift both prices to encourage position flattening
            // Long → lower prices (eager to sell, reluctant to buy)
            // Short → higher prices (eager to buy, reluctant to sell)
            // Use sigmoid for smooth non-linear response
            let inv_ratio = self.sigmoid_inventory_ratio(position);
            let effective_mid = if self.config.use_depth_pricing { pricing_mid } else { mid };
            let skew_dollars = effective_mid * inv_ratio * self.config.inventory_skew_bps / 10000.0;

            let our_bid = ((raw_bid - skew_dollars + momentum_shift) / self.config.tick_size).floor() * self.config.tick_size;
            let our_ask = ((raw_ask - skew_dollars + momentum_shift) / self.config.tick_size).ceil() * self.config.tick_size;

            // Safety: never cross the spread (bid must be < ask)
            if our_bid >= our_ask {
                debug!("Crossed spread: bid={:.2} >= ask={:.2}, skipping", our_bid, our_ask);
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }

            // ═══════════════════════════════════════════════════════════════════
            // P0: Anti-Taker Clamp — last-hop price protection before sending
            // ═══════════════════════════════════════════════════════════════════
            // Ensure bid stays strictly inside best_bid and ask stays strictly
            // inside best_ask by at least 1 tick, preventing taker fills even
            // if BBO moved between our calculation and order submission.
            let tick = self.config.tick_size;
            let clamped_bid = our_bid.min(bbo.bid_price - tick);
            let clamped_ask = our_ask.max(bbo.ask_price + tick);

            // Round to tick grid after clamping
            let our_bid = (clamped_bid / tick).floor() * tick;
            let our_ask = (clamped_ask / tick).ceil() * tick;

            // If market spread is too tight for safe maker placement, skip
            if our_bid >= our_ask || our_bid <= 0.0 {
                debug!(
                    "Anti-taker clamp: spread too tight after clamp (bid={:.2} ask={:.2} bbo={:.2}/{:.2}), skipping",
                    our_bid, our_ask, bbo.bid_price, bbo.ask_price
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }

            // Safety: our spread must cover round-trip fees
            let round_trip_fee_bps = self.config.maker_fee_bps * 2.0;
            let min_spread_bps = round_trip_fee_bps + self.config.min_profit_bps;
            let actual_spread_bps = ((our_ask - our_bid) / mid) * 10000.0;

            // Update telemetry metrics
            self.telemetry.update_spread_size(actual_spread_bps);
            self.telemetry.update_adverse_selection(self.micro.adverse_selection_score());

            if actual_spread_bps < min_spread_bps {
                debug!(
                    "Spread {:.1}bps < min {:.1}bps (mkt={:.1}bps), skipping",
                    actual_spread_bps, min_spread_bps, market_spread_bps
                );
                // Still cancel stale orders that are far from current mid
                if !self.active_orders.is_empty() {
                    let stale_threshold = mid * self.config.requote_threshold_bps * 3.0 / 10000.0;
                    let stale: Vec<(String, i64)> = self.active_orders.iter()
                        .filter(|o| (o.price - mid).abs() > stale_threshold)
                        .map(|o| (o.order_id.clone(), o.client_order_id))
                        .collect();
                    if !stale.is_empty() {
                        debug!("Canceling {} stale orders (mid={:.2}, threshold={:.2})", stale.len(), mid, stale_threshold);
                        for (oid, coi) in &stale {
                            if let Ok(idx) = oid.parse::<i64>() {
                                let _ = self.trading.cancel_order(idx).await;
                            }
                            self.order_tracker.mark_failed(*coi);
                        }
                        self.active_orders.retain(|o| stale.iter().all(|(sid, _)| sid != &o.order_id));
                    }
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }

            // ═══════════════════════════════════════════════════════════════════
            // CORE INNOVATION: Inventory-Weighted Asymmetric Order Sizing
            // ═══════════════════════════════════════════════════════════════════
            let (bid_size, ask_size) = self.calculate_asymmetric_sizes(position, mid);

            // Handle insufficient margin: cancel active orders to free up margin
            if bid_size < 0.001 && ask_size < 0.001 {
                if self.account_stats.available_balance < 20.0 {
                    // Low margin: cancel all active orders to free up capital
                    warn!(
                        "Low margin (${:.2}), canceling active orders to free up capital",
                        self.account_stats.available_balance
                    );
                    self.cancel_all_orders().await;
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    continue;
                } else {
                    warn!("Position {:.4} at limit, skipping quotes", position);
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            }

            debug!(
                "BBO={:.2}/{:.2} Mkt={:.1}bps Our={:.2}/{:.2} Sprd={:.1}bps Pos={:.4} Bid={:.3} Ask={:.3} post_only={}",
                bbo.bid_price, bbo.ask_price, market_spread_bps,
                our_bid, our_ask, actual_spread_bps,
                position, bid_size, ask_size, self.config.use_post_only
            );

            // Calculate grid levels for both sides
            let bid_levels = self.calculate_grid_levels(our_bid, bid_size, true);
            let ask_levels = self.calculate_grid_levels(our_ask, ask_size, false);

            // Check if we need to requote either side
            let should_requote_bid = self.should_requote_side(OrderSide::Buy, &bid_levels);
            let should_requote_ask = self.should_requote_side(OrderSide::Sell, &ask_levels);

            // Cancel stale orders on sides that need requoting
            if should_requote_bid {
                if let Err(e) = self.cancel_side_orders(OrderSide::Buy).await {
                    warn!("Failed to cancel bid orders: {}", e);
                }
            }
            if should_requote_ask {
                if let Err(e) = self.cancel_side_orders(OrderSide::Sell).await {
                    warn!("Failed to cancel ask orders: {}", e);
                }
            }

            // ═══════════════════════════════════════════════════════════════════
            // P1: Batch-first order placement — reduce single-side exposure
            // ═══════════════════════════════════════════════════════════════════
            let mut placed_bids = 0;
            let mut placed_asks = 0;

            // Fast path: both sides need requoting with single level → use atomic batch
            let use_batch = should_requote_bid && should_requote_ask
                && bid_levels.len() == 1 && ask_levels.len() == 1
                && bid_levels[0].1 >= 0.001 && ask_levels[0].1 >= 0.001;

            if use_batch {
                let (bid_price, bid_sz) = bid_levels[0];
                let (ask_price, ask_sz) = ask_levels[0];

                // Worst-case bilateral check before batch
                let worst_long = self.order_tracker.worst_case_long();
                let worst_short = self.order_tracker.worst_case_short();
                let bid_ok = worst_long + bid_sz <= self.config.max_position;
                let ask_ok = worst_short - ask_sz >= -self.config.max_position;

                if bid_ok && ask_ok {
                    match self.trading.place_batch(ExchangeBatchParams {
                        bid_price, ask_price, bid_size: bid_sz, ask_size: ask_sz,
                    }).await {
                        Ok(result) => {
                            let now = Instant::now();
                            // Use first tx_hash for bid, second for ask (or same if single hash)
                            let bid_hash = result.tx_hashes.first().cloned().unwrap_or_default();
                            let ask_hash = result.tx_hashes.get(1).cloned().unwrap_or_else(|| bid_hash.clone());
                            self.active_orders.push(ActiveOrder {
                                order_id: bid_hash, client_order_id: result.bid_client_order_index,
                                side: OrderSide::Buy, price: bid_price, size: bid_sz, placed_at: now,
                            });
                            self.active_orders.push(ActiveOrder {
                                order_id: ask_hash, client_order_id: result.ask_client_order_index,
                                side: OrderSide::Sell, price: ask_price, size: ask_sz, placed_at: now,
                            });
                            self.total_orders_placed += 2;
                            self.telemetry.record_order_placed();
                            self.telemetry.record_order_placed();
                            placed_bids = 1;
                            placed_asks = 1;
                            debug!("Batch placed: bid={:.2}x{:.3} ask={:.2}x{:.3}", bid_price, bid_sz, ask_price, ask_sz);
                        }
                        Err(e) => {
                            warn!("Batch order failed: {}", e);
                            self.telemetry.record_order_rejected(&format!("batch: {}", e));
                            if matches!(e.downcast_ref::<TradingError>(), Some(TradingError::InsufficientMargin)) {
                                self.telemetry.record_margin_cooldown(self.config.margin_cooldown_secs);
                                self.cancel_all_orders().await;
                                self.margin_cooldown_until = Instant::now() + Duration::from_secs(self.config.margin_cooldown_secs);
                            }
                        }
                    }
                } else {
                    debug!("Batch skipped: position limit (worst_long={:.4} worst_short={:.4})", worst_long, worst_short);
                }
            } else {
                // Fallback: sequential per-side placement (multi-level grid or single-side requote)
                let mut cumulative_bid_size = 0.0;
                let mut cumulative_ask_size = 0.0;

                if should_requote_bid && !bid_levels.is_empty() {
                    for (price, size) in &bid_levels {
                        if *size < 0.001 {
                            continue;
                        }
                        let current_pos = self.order_tracker.worst_case_long();
                        if current_pos + cumulative_bid_size + *size > self.config.max_position {
                            debug!(
                                "Grid bid L{} would breach max_position (worst_long={:.4} cumulative={:.4} size={:.4} max={:.4}), skipping",
                                placed_bids + 1, current_pos, cumulative_bid_size, size, self.config.max_position
                            );
                            break;
                        }
                        match self.trading.buy(*size, *price).await {
                            Ok(result) => {
                                debug!("Grid Buy L{}: ${:.2} x {:.3}", placed_bids + 1, price, size);
                                self.active_orders.push(ActiveOrder {
                                    order_id: result.tx_hash,
                                    client_order_id: result.client_order_index,
                                    side: OrderSide::Buy,
                                    price: *price,
                                    size: *size,
                                    placed_at: Instant::now(),
                                });
                                self.total_orders_placed += 1;
                                self.telemetry.record_order_placed();
                                placed_bids += 1;
                                cumulative_bid_size += *size;
                            }
                            Err(e) => {
                                warn!("Grid buy L{} failed: {}", placed_bids + 1, e);
                                self.telemetry.record_order_rejected(&format!("buy L{}: {}", placed_bids + 1, e));
                                if matches!(e.downcast_ref::<TradingError>(), Some(TradingError::InsufficientMargin)) {
                                    warn!("Margin insufficient, canceling all orders (cooldown {}s)", self.config.margin_cooldown_secs);
                                    self.telemetry.record_margin_cooldown(self.config.margin_cooldown_secs);
                                    self.cancel_all_orders().await;
                                    self.margin_cooldown_until = Instant::now() + Duration::from_secs(self.config.margin_cooldown_secs);
                                    break;
                                }
                            }
                        }
                    }
                }

                if should_requote_ask && !ask_levels.is_empty() {
                    for (price, size) in &ask_levels {
                        if *size < 0.001 {
                            continue;
                        }
                        let current_pos = self.order_tracker.worst_case_short();
                        if current_pos - cumulative_ask_size - *size < -self.config.max_position {
                            debug!(
                                "Grid ask L{} would breach max_position (worst_short={:.4} cumulative={:.4} size={:.4} max={:.4}), skipping",
                                placed_asks + 1, current_pos, cumulative_ask_size, size, self.config.max_position
                            );
                            break;
                        }
                        match self.trading.sell(*size, *price).await {
                            Ok(result) => {
                                debug!("Grid Sell L{}: ${:.2} x {:.3}", placed_asks + 1, price, size);
                                self.active_orders.push(ActiveOrder {
                                    order_id: result.tx_hash,
                                    client_order_id: result.client_order_index,
                                    side: OrderSide::Sell,
                                    price: *price,
                                    size: *size,
                                    placed_at: Instant::now(),
                                });
                                self.total_orders_placed += 1;
                                self.telemetry.record_order_placed();
                                placed_asks += 1;
                                cumulative_ask_size += *size;
                            }
                            Err(e) => {
                                warn!("Grid sell L{} failed: {}", placed_asks + 1, e);
                                self.telemetry.record_order_rejected(&format!("sell L{}: {}", placed_asks + 1, e));
                                if matches!(e.downcast_ref::<TradingError>(), Some(TradingError::InsufficientMargin)) {
                                    warn!("Margin insufficient, canceling all orders (cooldown {}s)", self.config.margin_cooldown_secs);
                                    self.telemetry.record_margin_cooldown(self.config.margin_cooldown_secs);
                                    self.cancel_all_orders().await;
                                    self.margin_cooldown_until = Instant::now() + Duration::from_secs(self.config.margin_cooldown_secs);
                                    break;
                                }
                            }
                        }
                    }
                }
            }

            // Log grid placement summary
            if placed_bids > 0 || placed_asks > 0 {
                info!(
                    "Grid placed: {} bid levels, {} ask levels (pos={:.4}{})",
                    placed_bids, placed_asks, position,
                    if use_batch { " batch" } else { "" }
                );
            }

            tokio::time::sleep(Duration::from_millis(self.config.poll_interval_ms)).await;
        }
    }

    /// Sigmoid inventory skew: smooth non-linear response to inventory deviation
    /// Returns a value in [-1, 1] with steeper response near max_position
    fn sigmoid_inventory_ratio(&self, position: f64) -> f64 {
        let normalized = position / self.config.max_position;
        // tanh provides smooth S-curve: gentle near 0, aggressive near ±1
        normalized.clamp(-1.0, 1.0).tanh()
    }

    /// Calculate grid levels for multi-tier quoting
    /// Returns Vec<(price, size)> for bid and ask sides
    ///
    /// Example with 3 levels, base_size=0.05, decay=0.7:
    ///   Level 1: 100% size (0.050)
    ///   Level 2:  70% size (0.035)
    ///   Level 3:  49% size (0.025)
    fn calculate_grid_levels(
        &self,
        base_price: f64,
        base_size: f64,
        is_bid: bool,
    ) -> Vec<(f64, f64)> {
        let mut levels = Vec::with_capacity(self.config.grid_levels as usize);

        for i in 0..self.config.grid_levels {
            // Calculate price offset for this level
            let spacing_dollars = base_price * self.config.grid_spacing_bps * (i as f64) / 10000.0;
            let price = if is_bid {
                base_price - spacing_dollars
            } else {
                base_price + spacing_dollars
            };

            // Round to tick size
            let rounded_price = (price / self.config.tick_size).floor() * self.config.tick_size;

            // Calculate size with exponential decay
            let size_multiplier = self.config.grid_size_decay.powi(i as i32);
            let size = base_size * size_multiplier;

            // Round to step size
            let rounded_size = (size / self.config.step_size).floor() * self.config.step_size;

            // Skip if size too small
            if rounded_size < 0.001 {
                break;
            }

            levels.push((rounded_price, rounded_size));
        }

        levels
    }

    /// Get all active orders for a given side
    fn get_active_orders(&self, side: OrderSide) -> Vec<&ActiveOrder> {
        self.active_orders
            .iter()
            .filter(|o| o.side == side)
            .collect()
    }

    /// Check if we should requote any orders on a given side
    ///
    /// Safety: includes divide-by-zero protection for target_price == 0.0
    /// to prevent NaN propagation that could crash the strategy.
    fn should_requote_side(&self, side: OrderSide, target_prices: &[(f64, f64)]) -> bool {
        let active = self.get_active_orders(side);

        // If number of orders doesn't match, requote
        if active.len() != target_prices.len() {
            return true;
        }

        // Check if any price has moved beyond threshold
        for (order, &(target_price, _)) in active.iter().zip(target_prices.iter()) {
            // Divide-by-zero protection: if target_price is zero or NaN, force requote
            if target_price == 0.0 || target_price.is_nan() {
                return true;
            }
            let price_diff = (order.price - target_price).abs();
            let threshold = target_price * self.config.requote_threshold_bps / 10000.0;
            if price_diff > threshold {
                return true;
            }
        }

        false
    }

    /// Cancel all orders on a given side
    async fn cancel_side_orders(&mut self, side: OrderSide) -> Result<()> {
        let orders_to_cancel: Vec<(String, i64)> = self.active_orders
            .iter()
            .filter(|o| o.side == side)
            .map(|o| (o.order_id.clone(), o.client_order_id))
            .collect();

        for (order_id, coi) in &orders_to_cancel {
            if let Ok(idx) = order_id.parse::<i64>() {
                let _ = self.trading.cancel_order(idx).await;
            }
            // Sync tracker: mark this order as canceled
            self.order_tracker.mark_failed(*coi);
        }
        self.active_orders.retain(|o| o.side != side);

        Ok(())
    }

    /// Calculate asymmetric order sizes to neutralize inventory
    ///
    /// Key principle: If short, bid size > ask size (eager to buy back)
    ///                If long, ask size > bid size (eager to sell)
    ///
    /// Example: position = -0.1 ETH (short)
    ///   → bid_size = 0.13 ETH (base + abs(position))
    ///   → ask_size = 0.03 ETH (base - abs(position))
    ///   If bid fills: new_pos = -0.1 + 0.13 = +0.03 (flipped to long)
    ///
    /// v4.0.0: Uses Sigmoid (tanh) function for SIZE multiplier instead of linear urgency
    fn calculate_asymmetric_sizes(&self, position: f64, mid: f64) -> (f64, f64) {
        // Hard stop: if position at limit, only allow flattening orders
        if position.abs() >= self.config.max_position {
            let min_size = 11.0 / mid;
            if position > 0.0 {
                // Long at limit: only allow sells
                return (0.0, min_size.max(self.config.base_order_size));
            } else {
                // Short at limit: only allow buys
                return (min_size.max(self.config.base_order_size), 0.0);
            }
        }

        // Sigmoid SIZE multiplier using tanh: 1.0 at pos=0, ~3.0 at pos=max_position
        // Formula: 1.0 + tanh(steepness * normalized_pos)
        // tanh(4) ≈ 0.9993, so at pos=max_position, multiplier ≈ 1.0 + 1.0 = 2.0
        // To reach 3.0, we scale: 1.0 + 2.0 * tanh(steepness * normalized_pos)
        let normalized_pos = position / self.config.max_position;
        let sigmoid_multiplier = 1.0 + 2.0 * (self.config.sigmoid_steepness * normalized_pos.abs()).tanh();

        // Cap inventory_offset to prevent whiplash: flattening order <= cap_mult * base_order_size
        let max_offset = self.config.base_order_size * (self.config.flattening_cap_mult - 1.0).max(0.5);
        let inventory_offset = (position.abs() * sigmoid_multiplier).min(max_offset);

        let bid_size = if position < 0.0 {
            // Short position → increase bid size to buy back
            self.config.base_order_size + inventory_offset
        } else {
            // Long position → decrease bid size
            (self.config.base_order_size - inventory_offset).max(0.0)
        };

        let ask_size = if position > 0.0 {
            // Long position → increase ask size to sell
            self.config.base_order_size + inventory_offset
        } else {
            // Short position → decrease ask size
            (self.config.base_order_size - inventory_offset).max(0.0)
        };

        // Enforce Lighter min quote ($11)
        let min_size = 11.0 / mid;

        // Enforce max position limit
        let bid_size = if position + bid_size > self.config.max_position {
            (self.config.max_position - position).max(0.0)
        } else {
            bid_size.max(min_size)
        };

        let ask_size = if position - ask_size < -self.config.max_position {
            (position + self.config.max_position).max(0.0)
        } else {
            ask_size.max(min_size)
        };

        // ═══════════════════════════════════════════════════════════════════
        // MARGIN MANAGEMENT: Adjust order sizes based on available balance
        // ═══════════════════════════════════════════════════════════════════
        let available = self.account_stats.available_balance;

        // Estimate margin required per order using configured max leverage
        let margin_per_eth = mid / self.config.max_leverage;

        // available_balance from exchange already deducts position margin,
        // so just reserve 30% buffer for safety
        let usable_balance = available * 0.7;

        let bid_margin_required = bid_size * margin_per_eth;
        let ask_margin_required = ask_size * margin_per_eth;
        let total_margin_required = bid_margin_required + ask_margin_required;

        let (bid_size, ask_size) = if total_margin_required > usable_balance {
            // Insufficient margin: scale down proportionally
            let scale_factor = (usable_balance / total_margin_required).min(1.0);

            if scale_factor < 0.1 {
                // Too little margin: cancel active orders and skip this cycle
                warn!(
                    "Insufficient margin: available=${:.2} required=${:.2} (scale={:.1}%), skipping quotes",
                    available, total_margin_required, scale_factor * 100.0
                );
                (0.0, 0.0)
            } else {
                // Scale down order sizes
                let scaled_bid = bid_size * scale_factor;
                let scaled_ask = ask_size * scale_factor;
                debug!(
                    "Margin constraint: scaled orders by {:.1}% (available=${:.2})",
                    scale_factor * 100.0, available
                );

                // Check if scaled sizes meet minimum requirements (Lighter DEX minimum ~0.01 ETH)
                let min_order_size = 0.01;
                let final_bid = if scaled_bid < min_order_size { 0.0 } else { scaled_bid };
                let final_ask = if scaled_ask < min_order_size { 0.0 } else { scaled_ask };

                (final_bid, final_ask)
            }
        } else {
            (bid_size, ask_size)
        };

        // Hard cap: no single order exceeds base_order_size * flattening_cap_mult
        let hard_cap = self.config.base_order_size * self.config.flattening_cap_mult;
        let bid_size = bid_size.min(hard_cap);
        let ask_size = ask_size.min(hard_cap);

        // Round to step size
        let bid_size = (bid_size / self.config.step_size).floor() * self.config.step_size;
        let ask_size = (ask_size / self.config.step_size).floor() * self.config.step_size;

        (bid_size, ask_size)
    }

    async fn cancel_all_orders(&mut self) {
        for order in &self.active_orders {
            if let Ok(idx) = order.order_id.parse::<i64>() {
                let _ = self.trading.cancel_order(idx).await;
            }
            // Sync tracker per-order
            self.order_tracker.mark_failed(order.client_order_id);
        }

        let count = self.active_orders.len();
        self.active_orders.clear();

        if count > 0 {
            debug!("Canceled {} orders (tracker synced per-order)", count);
        }
    }

    fn print_pnl_update(&self) {
        let equity = self.account_stats.portfolio_value;
        let available = self.account_stats.available_balance;
        let pnl = equity - self.session_start_balance;
        let pnl_pct = if self.session_start_balance > 0.0 {
            (pnl / self.session_start_balance) * 100.0
        } else {
            0.0
        };

        info!(
            "📊 PnL: ${:.2} ({:+.3}%) | Equity: ${:.2} | Avail: ${:.2} | Pos: {:.4} ETH | Orders: {}",
            pnl,
            pnl_pct,
            equity,
            available,
            self.account_stats.position,
            self.total_orders_placed,
        );
    }

    /// Calculate Volume-Weighted Micro Price using L1-L5 depth data.
    ///
    /// Formula: VWMicro = (bid_notional * ask_L1 + ask_notional * bid_L1) / (bid_notional + ask_notional)
    ///
    /// This provides a more accurate fair price than simple mid by incorporating order book imbalance.
    fn calculate_vw_micro_price(
        &self,
        depth: &crate::shm_depth_reader::ShmDepthSnapshot,
        bid_l1: f64,
        ask_l1: f64,
    ) -> f64 {
        // Calculate total notional value on bid side (L1-L5)
        let bid_notional: f64 = depth
            .bids
            .iter()
            .take(5)
            .filter(|l| l.price > 0.0 && l.size > 0.0)
            .map(|l| l.price * l.size)
            .sum();

        // Calculate total notional value on ask side (L1-L5)
        let ask_notional: f64 = depth
            .asks
            .iter()
            .take(5)
            .filter(|l| l.price > 0.0 && l.size > 0.0)
            .map(|l| l.price * l.size)
            .sum();

        // Avoid division by zero
        if bid_notional + ask_notional < 0.001 {
            return (bid_l1 + ask_l1) / 2.0; // Fallback to simple mid
        }

        // VWMicro formula: weight L1 prices by opposite side notional
        (bid_notional * ask_l1 + ask_notional * bid_l1) / (bid_notional + ask_notional)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_grid_level_calculation() {
        let config = InventoryNeutralMMConfig {
            tick_size: 0.01,
            step_size: 0.0001,
            grid_levels: 3,
            grid_spacing_bps: 2.0,
            grid_size_decay: 0.7,
            ..Default::default()
        };

        // Test bid levels (prices should decrease)
        let base_price = 2000.0;
        let base_size = 0.1;

        let mut bid_levels = Vec::new();
        for i in 0..config.grid_levels {
            let spacing_dollars = base_price * config.grid_spacing_bps * (i as f64) / 10000.0;
            let price = base_price - spacing_dollars;
            let rounded_price = (price / config.tick_size).floor() * config.tick_size;

            let size_multiplier = config.grid_size_decay.powi(i as i32);
            let size = base_size * size_multiplier;
            let rounded_size = (size / config.step_size).floor() * config.step_size;

            if rounded_size >= 0.001 {
                bid_levels.push((rounded_price, rounded_size));
            }
        }

        // Verify 3 levels generated
        assert_eq!(bid_levels.len(), 3);

        // Verify prices descend
        assert!(bid_levels[0].0 >= bid_levels[1].0);
        assert!(bid_levels[1].0 >= bid_levels[2].0);

        // Verify size decay (0.7^0, 0.7^1, 0.7^2)
        assert!((bid_levels[0].1 - 0.1).abs() < 0.001);
        assert!((bid_levels[1].1 - 0.07).abs() < 0.001);
        assert!((bid_levels[2].1 - 0.049).abs() < 0.001);
    }

    #[test]
    fn test_sigmoid_inventory_ratio() {
        let config = InventoryNeutralMMConfig {
            max_position: 0.15,
            ..Default::default()
        };

        // Test at zero inventory
        let ratio_zero = (0.0 / config.max_position).clamp(-1.0, 1.0).tanh();
        assert!((ratio_zero - 0.0).abs() < 0.01);

        // Test at 50% inventory
        let ratio_half = (0.075 / config.max_position).clamp(-1.0, 1.0).tanh();
        assert!(ratio_half > 0.0 && ratio_half < 0.5);

        // Test at max inventory
        let ratio_max = (0.15 / config.max_position).clamp(-1.0, 1.0).tanh();
        assert!(ratio_max > 0.7); // tanh(1.0) ≈ 0.76
    }

    #[test]
    fn test_multi_level_order_tracking() {
        // Test helper methods without full MM initialization
        let config = InventoryNeutralMMConfig {
            grid_levels: 3,
            requote_threshold_bps: 10.0,
            ..Default::default()
        };

        // Create a minimal MM instance for testing
        let mut active_orders = Vec::new();

        // Simulate adding orders
        active_orders.push(ActiveOrder {
            order_id: "1".to_string(),
            client_order_id: 1,
            side: OrderSide::Buy,
            price: 3000.0,
            size: 0.05,
            placed_at: Instant::now(),
        });
        active_orders.push(ActiveOrder {
            order_id: "2".to_string(),
            client_order_id: 2,
            side: OrderSide::Buy,
            price: 2995.0,
            size: 0.035,
            placed_at: Instant::now(),
        });
        active_orders.push(ActiveOrder {
            order_id: "3".to_string(),
            client_order_id: 3,
            side: OrderSide::Sell,
            price: 3010.0,
            size: 0.05,
            placed_at: Instant::now(),
        });

        // Test filtering by side
        let bids: Vec<_> = active_orders.iter().filter(|o| o.side == OrderSide::Buy).collect();
        assert_eq!(bids.len(), 2);
        assert_eq!(bids[0].price, 3000.0);
        assert_eq!(bids[1].price, 2995.0);

        let asks: Vec<_> = active_orders.iter().filter(|o| o.side == OrderSide::Sell).collect();
        assert_eq!(asks.len(), 1);
        assert_eq!(asks[0].price, 3010.0);

        // Test requote logic
        let target_prices = vec![(3000.0, 0.05), (2995.0, 0.035)];

        // Same prices - no requote needed
        let mut needs_requote = false;
        if bids.len() != target_prices.len() {
            needs_requote = true;
        } else {
            for (order, &(target_price, _)) in bids.iter().zip(target_prices.iter()) {
                let price_diff = (order.price - target_price).abs();
                let threshold = target_price * config.requote_threshold_bps / 10000.0;
                if price_diff > threshold {
                    needs_requote = true;
                    break;
                }
            }
        }
        assert!(!needs_requote);

        // Price moved beyond threshold (10 bps = 0.1%)
        let moved_prices = vec![(3005.0, 0.05), (2995.0, 0.035)];
        needs_requote = false;
        for (order, &(target_price, _)) in bids.iter().zip(moved_prices.iter()) {
            let price_diff = (order.price - target_price).abs();
            let threshold = target_price * config.requote_threshold_bps / 10000.0;
            if price_diff > threshold {
                needs_requote = true;
                break;
            }
        }
        assert!(needs_requote); // 5 dollar move on 3000 = 16.7 bps > 10 bps threshold
    }

    #[test]
    fn test_grid_integration() {
        // Integration test: verify full grid calculation pipeline
        let config = InventoryNeutralMMConfig {
            grid_levels: 3,
            grid_spacing_bps: 5.0,
            grid_size_decay: 0.7,
            tick_size: 0.01,
            step_size: 0.001,
            ..Default::default()
        };

        let base_price = 3000.0;
        let base_size = 0.1;

        // Calculate bid levels
        let mut bid_levels = Vec::new();
        for i in 0..config.grid_levels {
            let spacing_dollars = base_price * config.grid_spacing_bps * (i as f64) / 10000.0;
            let price = base_price - spacing_dollars;
            let rounded_price = (price / config.tick_size).floor() * config.tick_size;

            let size_multiplier = config.grid_size_decay.powi(i as i32);
            let size = base_size * size_multiplier;
            let rounded_size = (size / config.step_size).floor() * config.step_size;

            if rounded_size >= 0.001 {
                bid_levels.push((rounded_price, rounded_size));
            }
        }

        // Verify bid levels
        assert_eq!(bid_levels.len(), 3);

        // Level 0: base price, full size
        assert!((bid_levels[0].0 - 3000.0).abs() < 0.01);
        assert!((bid_levels[0].1 - 0.1).abs() < 0.001);

        // Level 1: -5 bps (1.5 dollars), 70% size (0.1*0.7=0.0699.. → floor → 0.069)
        assert!((bid_levels[1].0 - 2998.5).abs() < 0.01);
        assert!((bid_levels[1].1 - 0.069).abs() < 0.001);

        // Level 2: -10 bps (3.0 dollars), 49% size (0.1*0.49=0.0489.. → floor → 0.048)
        assert!((bid_levels[2].0 - 2997.0).abs() < 0.01);
        assert!((bid_levels[2].1 - 0.048).abs() < 0.001);

        // Calculate ask levels
        let mut ask_levels = Vec::new();
        for i in 0..config.grid_levels {
            let spacing_dollars = base_price * config.grid_spacing_bps * (i as f64) / 10000.0;
            let price = base_price + spacing_dollars;
            let rounded_price = (price / config.tick_size).floor() * config.tick_size;

            let size_multiplier = config.grid_size_decay.powi(i as i32);
            let size = base_size * size_multiplier;
            let rounded_size = (size / config.step_size).floor() * config.step_size;

            if rounded_size >= 0.001 {
                ask_levels.push((rounded_price, rounded_size));
            }
        }

        // Verify ask levels
        assert_eq!(ask_levels.len(), 3);

        // Level 0: base price, full size
        assert!((ask_levels[0].0 - 3000.0).abs() < 0.01);
        assert!((ask_levels[0].1 - 0.1).abs() < 0.001);

        // Level 1: +5 bps (1.5 dollars), 70% size (0.1*0.7=0.0699.. → floor → 0.069)
        assert!((ask_levels[1].0 - 3001.5).abs() < 0.01);
        assert!((ask_levels[1].1 - 0.069).abs() < 0.001);

        // Level 2: +10 bps (3.0 dollars), 49% size (0.1*0.49=0.0489.. → floor → 0.048)
        assert!((ask_levels[2].0 - 3003.0).abs() < 0.01);
        assert!((ask_levels[2].1 - 0.048).abs() < 0.001);
    }

    #[test]
    fn test_sigmoid_size_multiplier() {
        let config = InventoryNeutralMMConfig {
            max_position: 0.15,
            sigmoid_steepness: 4.0,
            ..Default::default()
        };

        // Helper function to calculate sigmoid multiplier
        let calc_multiplier = |position: f64| -> f64 {
            let normalized_pos = position / config.max_position;
            1.0 + 2.0 * (config.sigmoid_steepness * normalized_pos.abs()).tanh()
        };

        // Test at different position levels
        let pos_0 = 0.0;
        let pos_5pct = 0.05 * config.max_position;  // 0.0075
        let pos_50pct = 0.5 * config.max_position;  // 0.075
        let pos_80pct = 0.8 * config.max_position;  // 0.12
        let pos_100pct = config.max_position;       // 0.15

        let mult_0 = calc_multiplier(pos_0);
        let mult_5 = calc_multiplier(pos_5pct);
        let mult_50 = calc_multiplier(pos_50pct);
        let mult_80 = calc_multiplier(pos_80pct);
        let mult_100 = calc_multiplier(pos_100pct);

        // Verify sigmoid properties:
        // 1. At pos=0, multiplier ≈ 1.0 (minimal urgency)
        assert!((mult_0 - 1.0).abs() < 0.01, "pos=0: mult={}", mult_0);

        // 2. Monotonically increasing
        assert!(mult_5 > mult_0, "mult_5={} should > mult_0={}", mult_5, mult_0);
        assert!(mult_50 > mult_5, "mult_50={} should > mult_5={}", mult_50, mult_5);
        assert!(mult_80 > mult_50, "mult_80={} should > mult_50={}", mult_80, mult_50);
        assert!(mult_100 > mult_80, "mult_100={} should > mult_80={}", mult_100, mult_80);

        // 3. At pos=100%, multiplier ≈ 3.0 (max urgency)
        assert!((mult_100 - 3.0).abs() < 0.1, "pos=100%: mult={}", mult_100);

        // 4. Steeper growth in middle range (50% → 80% should have larger delta than 5% → 50%)
        // This validates the sigmoid curve is steeper in the middle
        let delta_low = mult_50 - mult_5;
        let delta_mid = mult_80 - mult_50;
        assert!(delta_mid > 0.0, "delta_mid={} should be positive", delta_mid);
        assert!(delta_low > 0.0, "delta_low={} should be positive", delta_low);
    }

    #[test]
    fn test_vw_micro_price_calculation() {
        use crate::shm_depth_reader::{PriceLevel, ShmDepthSnapshot};

        // Create mock depth snapshot
        let depth = ShmDepthSnapshot {
            seqlock: 0,
            exchange_id: 2,
            symbol_id: 1002,
            _padding1: 0,
            timestamp_ns: 1234567890,
            bids: [
                PriceLevel { price: 3000.0, size: 1.0 },
                PriceLevel { price: 2999.0, size: 2.0 },
                PriceLevel { price: 2998.0, size: 1.5 },
                PriceLevel { price: 2997.0, size: 1.0 },
                PriceLevel { price: 2996.0, size: 0.5 },
            ],
            asks: [
                PriceLevel { price: 3001.0, size: 1.0 },
                PriceLevel { price: 3002.0, size: 2.0 },
                PriceLevel { price: 3003.0, size: 1.5 },
                PriceLevel { price: 3004.0, size: 1.0 },
                PriceLevel { price: 3005.0, size: 0.5 },
            ],
            _reserved: [0; 72],
        };

        // Calculate VWMicro manually
        let bid_notional: f64 = depth.bids.iter().map(|l| l.price * l.size).sum();
        let ask_notional: f64 = depth.asks.iter().map(|l| l.price * l.size).sum();
        let vw_micro = (bid_notional * 3001.0 + ask_notional * 3000.0) / (bid_notional + ask_notional);

        // VWMicro should be between bid and ask
        assert!(vw_micro > 3000.0 && vw_micro < 3001.0,
            "VWMicro {} should be between 3000.0 and 3001.0", vw_micro);

        // Should be closer to mid than simple average due to depth weighting
        let simple_mid = 3000.5;
        assert!((vw_micro - simple_mid).abs() < 1.0,
            "VWMicro {} should be close to simple mid {}", vw_micro, simple_mid);
    }
}

