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
use crate::exchange::{BatchOrderParams, Exchange};
use crate::shadow_ledger::ShadowLedger;
use crate::shm_reader::ShmReader;
use parking_lot::RwLock;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

// ─── Account Stats ───────────────────────────────────────────────────────────
#[derive(Debug, Clone)]
pub struct AccountStats {
    pub available_balance: f64,
    pub position: f64,
    pub leverage: f64,
    pub last_update: Instant,
}

impl Default for AccountStats {
    fn default() -> Self {
        Self {
            available_balance: 0.0,
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
    fn new(max_samples: usize) -> Self {
        Self {
            price_samples: VecDeque::with_capacity(max_samples),
            max_samples,
            ema_fast: 0.0,
            ema_slow: 0.0,
            ema_fast_alpha: 2.0 / 6.0,   // 5-period EMA
            ema_slow_alpha: 2.0 / 21.0,  // 20-period EMA
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
    price: f64,
    #[allow(dead_code)]
    size: f64,
    placed_at: Instant,
}

// ─── Inventory-Neutral Market Maker ──────────────────────────────────────────
pub struct InventoryNeutralMM {
    config: InventoryNeutralMMConfig,

    trading: Arc<dyn Exchange>,
    #[allow(dead_code)]
    ledger: Arc<RwLock<ShadowLedger>>,
    shm_reader: ShmReader,
    account_stats_reader: AccountStatsReader,
    account_stats: AccountStats,
    micro: MicrostructureTracker,

    active_bid: Option<ActiveOrder>,
    active_ask: Option<ActiveOrder>,

    session_start_balance: f64,
    total_orders_placed: u64,
    last_balance_check: Instant,
    margin_cooldown_until: Instant,
}

impl InventoryNeutralMM {
    pub fn new(
        config: InventoryNeutralMMConfig,
        trading: Arc<dyn Exchange>,
        ledger: Arc<RwLock<ShadowLedger>>,
        shm_reader: ShmReader,
        account_stats_reader: AccountStatsReader,
    ) -> Self {
        Self {
            config,
            trading,
            ledger,
            shm_reader,
            account_stats_reader,
            account_stats: AccountStats::default(),
            micro: MicrostructureTracker::new(200),
            active_bid: None,
            active_ask: None,
            session_start_balance: 0.0,
            total_orders_placed: 0,
            last_balance_check: Instant::now(),
            margin_cooldown_until: Instant::now(),
        }
    }

    pub async fn run(
        &mut self,
        mut shutdown: Option<tokio::sync::watch::Receiver<bool>>,
    ) -> Result<()> {
        info!("🎯 Inventory-Neutral MM started");

        // Cancel all existing orders
        if let Err(e) = self.trading.cancel_all().await {
            warn!("Failed to cancel existing orders: {:?}", e);
        }

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
                self.session_start_balance = self.account_stats.available_balance;
                info!("✅ Account stats loaded: ${:.2} available", self.account_stats.available_balance);
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }

        info!("🚀 Starting main loop...");

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
                            if let Err(e) = self.trading.close_all_positions(mid).await {
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
            let position = self.account_stats.position;

            // Periodic ledger sync: correct drift from missed events (every 30s)
            if self.last_balance_check.elapsed() > Duration::from_secs(30) {
                let delta = self.ledger.write().force_sync_position(position);
                if delta.abs() > 0.001 {
                    warn!("Ledger drift corrected: delta={:.6} ETH", delta);
                }
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
            let market_spread_bps = ((bbo.ask_price - bbo.bid_price) / mid) * 10000.0;

            // Update microstructure
            self.micro.update(mid);
            let _vol_bps = self.micro.volatility_bps();
            let _momentum_bps = self.micro.momentum_bps();
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

            // Penny jump: improve BBO by 1 tick
            let raw_bid = bbo.bid_price + penny;
            let raw_ask = bbo.ask_price - penny;

            // Inventory skew: shift both prices to encourage position flattening
            // Long → lower prices (eager to sell, reluctant to buy)
            // Short → higher prices (eager to buy, reluctant to sell)
            // Use sigmoid for smooth non-linear response
            let inv_ratio = self.sigmoid_inventory_ratio(position);
            let skew_dollars = mid * inv_ratio * self.config.inventory_skew_bps / 10000.0;

            let our_bid = ((raw_bid - skew_dollars) / self.config.tick_size).floor() * self.config.tick_size;
            let our_ask = ((raw_ask - skew_dollars) / self.config.tick_size).ceil() * self.config.tick_size;

            // Safety: never cross the spread (bid must be < ask)
            if our_bid >= our_ask {
                debug!("Crossed spread: bid={:.2} >= ask={:.2}, skipping", our_bid, our_ask);
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }

            // Safety: our spread must cover round-trip fees
            let round_trip_fee_bps = self.config.maker_fee_bps * 2.0;
            let min_spread_bps = round_trip_fee_bps + self.config.min_profit_bps;
            let actual_spread_bps = ((our_ask - our_bid) / mid) * 10000.0;
            if actual_spread_bps < min_spread_bps {
                debug!(
                    "Spread {:.1}bps < min {:.1}bps (mkt={:.1}bps), skipping",
                    actual_spread_bps, min_spread_bps, market_spread_bps
                );
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
                "BBO={:.2}/{:.2} Mkt={:.1}bps Our={:.2}/{:.2} Sprd={:.1}bps Pos={:.4} Bid={:.3} Ask={:.3}",
                bbo.bid_price, bbo.ask_price, market_spread_bps,
                our_bid, our_ask, actual_spread_bps,
                position, bid_size, ask_size
            );

            // Determine which sides to quote
            let should_requote_bid = self.should_requote(&self.active_bid, our_bid);
            let should_requote_ask = self.should_requote(&self.active_ask, our_ask);

            let place_bid = should_requote_bid && bid_size >= 0.001;
            let place_ask = should_requote_ask && ask_size >= 0.001;

            // Cancel stale orders
            if place_bid {
                if let Some(ref order) = self.active_bid {
                    if let Ok(idx) = order.order_id.parse::<i64>() {
                        let _ = self.trading.cancel_order(idx).await;
                    }
                    self.active_bid = None;
                }
            }
            if place_ask {
                if let Some(ref order) = self.active_ask {
                    if let Ok(idx) = order.order_id.parse::<i64>() {
                        let _ = self.trading.cancel_order(idx).await;
                    }
                    self.active_ask = None;
                }
            }

            // Place orders
            if place_bid && place_ask {
                // Use batch API for both sides
                match self.trading.place_batch(BatchOrderParams {
                    bid_price: our_bid,
                    ask_price: our_ask,
                    bid_size,
                    ask_size,
                }).await {
                    Ok(result) => {
                        info!(
                            "Batch: Bid ${:.2} x {:.3} / Ask ${:.2} x {:.3} (pos={:.4})",
                            our_bid, bid_size, our_ask, ask_size, position
                        );
                        self.active_bid = Some(ActiveOrder {
                            order_id: result.bid_client_order_index.to_string(),
                            price: our_bid,
                            size: bid_size,
                            placed_at: Instant::now(),
                        });
                        self.active_ask = Some(ActiveOrder {
                            order_id: result.ask_client_order_index.to_string(),
                            price: our_ask,
                            size: ask_size,
                            placed_at: Instant::now(),
                        });
                        self.total_orders_placed += 2;
                    }
                    Err(e) => {
                        warn!("Batch failed: {}", e);
                        // Handle margin errors by canceling active orders
                        if matches!(e.downcast_ref::<TradingError>(), Some(TradingError::InsufficientMargin)) {
                            warn!("Margin insufficient, canceling active orders (cooldown {}s)", self.config.margin_cooldown_secs);
                            self.cancel_all_orders().await;
                            self.margin_cooldown_until = Instant::now() + Duration::from_secs(self.config.margin_cooldown_secs);
                        }
                    }
                }
            } else if place_bid {
                match self.trading.buy(bid_size, our_bid).await {
                    Ok(result) => {
                        info!("Buy: ${:.2} x {:.3} (pos={:.4})", our_bid, bid_size, position);
                        self.active_bid = Some(ActiveOrder {
                            order_id: result.tx_hash,
                            price: our_bid,
                            size: bid_size,
                            placed_at: Instant::now(),
                        });
                        self.total_orders_placed += 1;
                    }
                    Err(e) => {
                        warn!("Buy failed: {}", e);
                        if e.to_string().contains("not enough margin") {
                            warn!("Margin insufficient, canceling active orders (cooldown 5s)");
                            self.cancel_all_orders().await;
                            self.margin_cooldown_until = Instant::now() + Duration::from_secs(5);
                        }
                    }
                }
            } else if place_ask {
                match self.trading.sell(ask_size, our_ask).await {
                    Ok(result) => {
                        info!("Sell: ${:.2} x {:.3} (pos={:.4})", our_ask, ask_size, position);
                        self.active_ask = Some(ActiveOrder {
                            order_id: result.tx_hash,
                            price: our_ask,
                            size: ask_size,
                            placed_at: Instant::now(),
                        });
                        self.total_orders_placed += 1;
                    }
                    Err(e) => {
                        warn!("Sell failed: {}", e);
                        if e.to_string().contains("not enough margin") {
                            warn!("Margin insufficient, canceling active orders (cooldown 5s)");
                            self.cancel_all_orders().await;
                            self.margin_cooldown_until = Instant::now() + Duration::from_secs(5);
                        }
                    }
                }
            }

            // Periodic PnL reporting
            if self.last_balance_check.elapsed() > Duration::from_secs(30) {
                self.print_pnl_update();
                self.last_balance_check = Instant::now();
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

    /// Calculate asymmetric order sizes to neutralize inventory
    ///
    /// Key principle: If short, bid size > ask size (eager to buy back)
    ///                If long, ask size > bid size (eager to sell)
    ///
    /// Example: position = -0.1 ETH (short)
    ///   → bid_size = 0.13 ETH (base + abs(position))
    ///   → ask_size = 0.03 ETH (base - abs(position))
    ///   If bid fills: new_pos = -0.1 + 0.13 = +0.03 (flipped to long)
    fn calculate_asymmetric_sizes(&self, position: f64, mid: f64) -> (f64, f64) {
        // Inventory urgency: how aggressively to flatten
        let urgency = if position.abs() > self.config.inventory_urgency_threshold {
            // Panic mode: very aggressive
            2.0
        } else {
            // Normal mode: moderate
            1.0
        };

        let inventory_offset = position.abs() * urgency;

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
        let _leverage = self.account_stats.leverage;

        // Estimate margin required per order (conservative: assume 10x leverage)
        let margin_per_eth = mid / 10.0;
        let bid_margin_required = bid_size * margin_per_eth;
        let ask_margin_required = ask_size * margin_per_eth;
        let total_margin_required = bid_margin_required + ask_margin_required;

        // Reserve 20% buffer for safety
        let usable_balance = available * 0.8;

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

        // Round to step size
        let bid_size = (bid_size / self.config.step_size).floor() * self.config.step_size;
        let ask_size = (ask_size / self.config.step_size).floor() * self.config.step_size;

        (bid_size, ask_size)
    }

    fn should_requote(&self, active_order: &Option<ActiveOrder>, new_price: f64) -> bool {
        match active_order {
            None => true,
            Some(order) => {
                let deviation_bps = ((new_price - order.price).abs() / order.price) * 10000.0;
                let age = order.placed_at.elapsed();
                deviation_bps > self.config.requote_threshold_bps || age > Duration::from_secs(self.config.order_ttl_secs)
            }
        }
    }

    async fn cancel_all_orders(&mut self) {
        if let Some(ref order) = self.active_bid {
            if let Ok(idx) = order.order_id.parse::<i64>() {
                let _ = self.trading.cancel_order(idx).await;
            }
        }
        if let Some(ref order) = self.active_ask {
            if let Ok(idx) = order.order_id.parse::<i64>() {
                let _ = self.trading.cancel_order(idx).await;
            }
        }
        self.active_bid = None;
        self.active_ask = None;
    }

    fn print_pnl_update(&self) {
        let pnl = self.account_stats.available_balance - self.session_start_balance;
        let pnl_pct = if self.session_start_balance > 0.0 {
            (pnl / self.session_start_balance) * 100.0
        } else {
            0.0
        };

        info!(
            "📊 PnL: ${:.2} ({:+.2}%) | Pos: {:.4} ETH | Orders: {} | Balance: ${:.2}",
            pnl,
            pnl_pct,
            self.account_stats.position,
            self.total_orders_placed,
            self.account_stats.available_balance
        );
    }
}
