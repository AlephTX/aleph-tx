//! Adaptive Market Maker Strategy
//!
//! A production-grade market making strategy with:
//! - Dynamic position sizing based on account balance
//! - Inventory skew to manage directional risk
//! - Adaptive spreads based on volatility
//! - PnL tracking and risk management
//! - Real-time account stats from shared memory

use crate::account_stats_reader::{AccountStatsReader, AccountStatsSnapshot};
use crate::error::Result;
use crate::lighter_trading::{LighterTrading, OrderParams, OrderType, Side};
use crate::shadow_ledger::ShadowLedger;
use crate::shm_reader::ShmReader;
use parking_lot::RwLock;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

/// Account statistics from Lighter WebSocket
#[derive(Debug, Clone)]
pub struct AccountStats {
    pub collateral: f64,           // Total collateral in USDC
    pub portfolio_value: f64,      // Portfolio value
    pub leverage: f64,             // Current leverage
    pub available_balance: f64,    // Available balance for trading
    pub margin_usage: f64,         // Margin usage ratio (0-1)
    pub buying_power: f64,         // Buying power
    pub position: f64,             // Net position (positive=long, negative=short)
    pub last_update: Instant,
}

impl Default for AccountStats {
    fn default() -> Self {
        Self {
            collateral: 0.0,
            portfolio_value: 0.0,
            leverage: 0.0,
            available_balance: 0.0,
            margin_usage: 0.0,
            buying_power: 0.0,
            position: 0.0,
            last_update: Instant::now(),
        }
    }
}

impl From<AccountStatsSnapshot> for AccountStats {
    fn from(snapshot: AccountStatsSnapshot) -> Self {
        Self {
            collateral: snapshot.collateral,
            portfolio_value: snapshot.portfolio_value,
            leverage: snapshot.leverage,
            available_balance: snapshot.available_balance,
            margin_usage: snapshot.margin_usage,
            buying_power: snapshot.buying_power,
            position: snapshot.position,
            last_update: Instant::now(),
        }
    }
}

/// Market volatility tracker
struct VolatilityTracker {
    price_samples: VecDeque<f64>,
    max_samples: usize,
}

impl VolatilityTracker {
    fn new(max_samples: usize) -> Self {
        Self {
            price_samples: VecDeque::with_capacity(max_samples),
            max_samples,
        }
    }

    fn add_sample(&mut self, price: f64) {
        if self.price_samples.len() >= self.max_samples {
            self.price_samples.pop_front();
        }
        self.price_samples.push_back(price);
    }

    /// Calculate realized volatility (standard deviation of returns)
    fn calculate_volatility(&self) -> f64 {
        if self.price_samples.len() < 2 {
            return 0.0;
        }

        let returns: Vec<f64> = self
            .price_samples
            .iter()
            .zip(self.price_samples.iter().skip(1))
            .map(|(p1, p2)| (p2 / p1 - 1.0).abs())
            .collect();

        let mean = returns.iter().sum::<f64>() / returns.len() as f64;
        let variance = returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / returns.len() as f64;
        variance.sqrt()
    }
}

#[derive(Debug, Clone)]
struct ActiveOrder {
    order_id: String,
    #[allow(dead_code)]
    side: Side,
    price: f64,
    #[allow(dead_code)]
    size: f64,
    placed_at: Instant,
}

pub struct AdaptiveMarketMaker {
    symbol_id: u16,
    market_id: u16,

    // Strategy parameters
    base_spread_bps: u32,          // Base spread in basis points
    min_spread_bps: u32,           // Minimum spread
    max_spread_bps: u32,           // Maximum spread
    volatility_multiplier: f64,    // Spread adjustment based on volatility

    // Position sizing
    base_order_size: f64,          // Base order size in ETH
    max_position: f64,             // Maximum position in ETH
    inventory_skew_factor: f64,    // How much to skew quotes based on inventory

    // Risk management
    max_leverage: f64,             // Maximum allowed leverage
    min_available_balance: f64,    // Minimum balance to keep available

    // Market precision
    tick_size: f64,
    step_size: f64,

    // State
    trading: Arc<LighterTrading>,
    ledger: Arc<RwLock<ShadowLedger>>,
    shm_reader: ShmReader,
    account_stats_reader: AccountStatsReader,
    account_stats: AccountStats,
    volatility_tracker: VolatilityTracker,

    // Order management
    active_bid: Option<ActiveOrder>,
    active_ask: Option<ActiveOrder>,

    // PnL tracking
    session_start_balance: f64,
    last_balance_check: Instant,
}

impl AdaptiveMarketMaker {
    pub fn new(
        symbol_id: u16,
        market_id: u16,
        trading: Arc<LighterTrading>,
        ledger: Arc<RwLock<ShadowLedger>>,
        shm_reader: ShmReader,
        account_stats_reader: AccountStatsReader,
    ) -> Self {
        Self {
            symbol_id,
            market_id,
            base_spread_bps: 3,
            min_spread_bps: 2,
            max_spread_bps: 15,
            volatility_multiplier: 1.5,
            base_order_size: 0.001,
            max_position: 0.1,
            inventory_skew_factor: 0.05,
            max_leverage: 10.0,
            min_available_balance: 2.0,
            tick_size: 0.01,
            step_size: 0.0001,
            trading,
            ledger,
            shm_reader,
            account_stats_reader,
            account_stats: AccountStats::default(),
            volatility_tracker: VolatilityTracker::new(100),
            active_bid: None,
            active_ask: None,
            session_start_balance: 0.0,
            last_balance_check: Instant::now(),
        }
    }

    pub async fn run(
        &mut self,
        mut shutdown: Option<tokio::sync::watch::Receiver<bool>>,
    ) -> Result<()> {
        // Step 1: Cancel all existing orders before starting
        info!("Canceling all existing orders...");
        if let Err(e) = self.trading.cancel_all().await {
            warn!("Failed to cancel existing orders: {:?}", e);
        }

        // Step 2: Wait for account stats to be available (with timeout)
        info!("⏳ Waiting for account stats from feeder...");
        let start_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;

        let mut retries = 0;
        let max_retries = 15; // Increased from 10 to 15 seconds
        loop {
            let stats = self.account_stats_reader.read();

            // Check if data is fresh (updated within last 10 seconds)
            let data_age_ns = start_time.saturating_sub(stats.updated_at);
            let data_age_secs = data_age_ns / 1_000_000_000;

            if (stats.collateral > 0.0 || stats.available_balance > 0.0) && data_age_secs < 10 {
                self.account_stats = stats.into();
                self.session_start_balance = self.account_stats.available_balance;
                info!("✅ Account stats loaded: ${:.2} available (data age: {}s)",
                    self.account_stats.available_balance, data_age_secs);
                break;
            }

            retries += 1;
            if retries >= max_retries {
                error!("❌ Timeout waiting for account stats after {}s", max_retries);
                error!("   Last stats: collateral=${:.2} balance=${:.2} age={}s",
                    stats.collateral, stats.available_balance, data_age_secs);
                return Err(crate::error::TradingError::OrderFailed(
                    "Account stats not available from feeder".to_string()
                ).into());
            }

            tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
        }

        // Step 3: Check for existing positions and close them
        info!("🔍 Checking for existing positions...");
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await; // Wait for position data

        let existing_position = self.account_stats.position;

        if existing_position.abs() > 0.0001 {
            warn!(
                "Found existing position: {:.4} ETH, closing...",
                existing_position
            );

            let exchanges = self.shm_reader.read_all_exchanges(self.symbol_id);
            let lighter_bbo = exchanges
                .iter()
                .find(|(exch_id, _)| *exch_id == 2)
                .map(|(_, msg)| msg);

            if let Some(bbo) = lighter_bbo.filter(|b| b.bid_price > 0.0) {
                let mid_price = (bbo.bid_price + bbo.ask_price) / 2.0;
                match self.trading.close_all_positions(mid_price).await {
                    Ok(_) => info!("Existing position closed successfully"),
                    Err(e) => warn!("Failed to close existing position: {:?}", e),
                }
            } else {
                warn!("No valid BBO data, skipping position close");
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
        } else {
            info!("✅ No existing position found");
        }

        // Step 4: Safety check - warn if leverage is too high but allow starting
        // The strategy will automatically reduce leverage by only allowing closing orders
        if self.account_stats.leverage > 10.0 {
            warn!(
                "⚠️  WARNING: Leverage {:.2}x > 10.0x at startup",
                self.account_stats.leverage
            );
            warn!("   Strategy will only place orders to reduce leverage");
            warn!("   - Balance: ${:.2}", self.account_stats.available_balance);
            warn!("   - Leverage: {:.2}x", self.account_stats.leverage);
            warn!("   - Margin Usage: {:.1}%", self.account_stats.margin_usage * 100.0);
        }

        // Step 5: Safety check - refuse to start if balance is too low
        if self.account_stats.available_balance < 10.0 {
            error!(
                "❌ SAFETY CHECK FAILED: Balance ${:.2} < $10.00",
                self.account_stats.available_balance
            );
            error!("   Minimum balance required: $10.00");
            return Err(crate::error::TradingError::OrderFailed(
                "Balance too low to start safely".to_string()
            ).into());
        }

        info!(
            "🎯 Adaptive MM started: symbol={} market={} base_spread={}bps",
            self.symbol_id, self.market_id, self.base_spread_bps
        );
        info!(
            "💰 Initial balance: ${:.2} | Leverage: {:.2}x",
            self.account_stats.available_balance, self.account_stats.leverage
        );
        info!(
            "⚙️  Risk limits: max_leverage={:.1}x max_position={:.3} ETH",
            self.max_leverage, self.max_position
        );

        loop {
            // Check shutdown signal
            if let Some(ref mut rx) = shutdown
                && *rx.borrow()
            {
                info!("Shutdown signal received, cleaning up...");

                // Step 1: Cancel all orders (both tracked and untracked)
                info!("Canceling all orders via API...");
                if let Err(e) = self.trading.cancel_all().await {
                    warn!("Failed to cancel orders via API: {:?}", e);
                }

                self.active_bid = None;
                self.active_ask = None;

                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

                let net_pos = self.account_stats.position;

                if net_pos.abs() > 0.0001 {
                    warn!("Closing position: {:.4} ETH", net_pos);

                    let exchanges = self.shm_reader.read_all_exchanges(self.symbol_id);
                    let lighter_bbo = exchanges
                        .iter()
                        .find(|(exch_id, _)| *exch_id == 2)
                        .map(|(_, msg)| msg);

                    if let Some(bbo) = lighter_bbo.filter(|b| b.bid_price > 0.0) {
                        let mid_price = (bbo.bid_price + bbo.ask_price) / 2.0;
                        match self.trading.close_all_positions(mid_price).await {
                            Ok(_) => info!("Position closed successfully"),
                            Err(e) => error!("Failed to close position: {:?}", e),
                        }
                    } else {
                        warn!("No valid BBO data, cannot close position");
                    }
                } else {
                    info!("No position to close");
                }

                self.print_session_summary();
                return Ok(());
            }

            // Step 1: Update account stats if available
            if let Some(stats_snapshot) = self.account_stats_reader.read_if_updated() {
                self.account_stats = stats_snapshot.into();
                debug!(
                    "📊 Account updated: balance=${:.2} leverage={:.2}x margin={:.1}%",
                    self.account_stats.available_balance,
                    self.account_stats.leverage,
                    self.account_stats.margin_usage * 100.0
                );
            }

            let available_balance = self.account_stats.available_balance;
            let leverage = self.account_stats.leverage;
            let _margin_usage = self.account_stats.margin_usage;

            // Risk check: leverage too high - only allow closing orders
            let leverage_too_high = leverage > self.max_leverage;
            if leverage_too_high {
                warn!(
                    "⚠️  Leverage too high: {:.2}x > {:.2}x, will only quote to reduce leverage",
                    leverage, self.max_leverage
                );
                // Cancel existing orders to reduce risk
                self.cancel_all_orders().await;
                // Don't continue - allow closing orders below
            }

            // Risk check: insufficient balance
            if available_balance < self.min_available_balance {
                warn!(
                    "⚠️  Insufficient balance: ${:.2} < ${:.2}, skipping quotes",
                    available_balance, self.min_available_balance
                );
                tokio::time::sleep(Duration::from_millis(1000)).await;
                continue;
            }

            // Step 2: Read position from WebSocket (real) and Shadow Ledger (local tracking)
            let (real_position, shadow_position) = {
                let ledger = self.ledger.read();
                let shadow = ledger.total_exposure();
                let real = self.account_stats.position;
                (real, shadow)
            };

            if (real_position - shadow_position).abs() > 0.001 {
                debug!(
                    "Position mismatch: Real={:.4} Shadow={:.4}",
                    real_position, shadow_position
                );
            }

            // ALWAYS use real position for risk management
            let total_exposure: f64 = real_position;

            // Step 2.5: Check for trapped position (套牢检测)
            // When position exceeds max, cancel all orders but continue to normal quoting
            // The position limits (can_buy/can_sell) will prevent adding more exposure
            if total_exposure.abs() > self.max_position {
                warn!(
                    "⚠️  Position trapped: {:.4} ETH > max {:.4} ETH, will only quote to reduce position",
                    total_exposure, self.max_position
                );

                // Cancel all orders to start fresh
                self.cancel_all_orders().await;

                // Don't use continue - let normal quoting logic handle it
                // The can_buy/can_sell checks will prevent adding more exposure
            }

            // Step 3: Read market data
            let exchanges = self.shm_reader.read_all_exchanges(self.symbol_id);
            let lighter_bbo = exchanges
                .iter()
                .find(|(exch_id, _)| *exch_id == 2)
                .map(|(_, msg)| msg);

            if lighter_bbo.is_none() || lighter_bbo.unwrap().bid_price == 0.0 {
                debug!("No BBO data available, waiting...");
                tokio::time::sleep(Duration::from_millis(10)).await;
                continue;
            }

            let bbo = lighter_bbo.unwrap();
            let mid = (bbo.bid_price + bbo.ask_price) / 2.0;

            // Update volatility tracker
            self.volatility_tracker.add_sample(mid);

            // Step 4: Calculate adaptive parameters
            let order_size = self.calculate_order_size(available_balance, mid);
            let spread_bps = self.calculate_adaptive_spread();
            let (bid_skew, ask_skew) = self.calculate_inventory_skew(total_exposure);

            // Step 5: Calculate quotes with skew
            let spread = mid * (spread_bps as f64) / 10000.0;
            let our_bid = mid - spread / 2.0 - bid_skew;
            let our_ask = mid + spread / 2.0 + ask_skew;

            // Round to tick size
            let our_bid = (our_bid / self.tick_size).floor() * self.tick_size;
            let our_ask = (our_ask / self.tick_size).ceil() * self.tick_size;

            debug!(
                "📊 Mid={:.2} Spread={}bps Size={:.4} Exposure={:.4} Leverage={:.2}x",
                mid, spread_bps, order_size, total_exposure, leverage
            );

            // Step 6: Update quotes if needed (cancel stale orders inline)
            let should_requote_bid = self.should_requote(&self.active_bid, our_bid);
            let should_requote_ask = self.should_requote(&self.active_ask, our_ask);

            // Place buy order - always place unless at max long position or leverage too high
            if should_requote_bid {
                // Check if we can add more long exposure (90% threshold)
                let can_buy = total_exposure < self.max_position * 0.9 && !leverage_too_high;

                if !can_buy {
                    if leverage_too_high {
                        debug!("⏸️  Skipping buy order: leverage {:.2}x too high", leverage);
                    } else {
                        debug!("⏸️  Skipping buy order: position {:.4} >= 90% of max {:.4}",
                               total_exposure, self.max_position);
                    }
                } else {
                    if let Some(ref order) = self.active_bid {
                        if let Ok(order_index) = order.order_id.parse::<i64>() {
                            let _ = self.trading.cancel_order(order_index).await;
                        }
                        self.active_bid = None;
                    }

                    match self.trading.place_order(OrderParams {
                        size: order_size,
                        price: our_bid,
                        side: Side::Buy,
                        order_type: OrderType::Limit,
                        reduce_only: false,
                    }).await {
                        Ok(result) => {
                            info!("Buy: ${:.2} x {:.4} ETH", our_bid, order_size);
                            self.active_bid = Some(ActiveOrder {
                                order_id: result.tx_hash,
                                side: Side::Buy,
                                price: our_bid,
                                size: order_size,
                                placed_at: Instant::now(),
                            });
                        }
                        Err(e) => {
                            warn!("Buy order failed: {}", e);
                        }
                    }
                }
            }

            // Place sell order
            if should_requote_ask {
                let would_close_long = total_exposure > 0.0;
                let can_sell = total_exposure > -self.max_position * 0.9
                    && (!leverage_too_high || would_close_long);

                if !can_sell {
                    if leverage_too_high && !would_close_long {
                        debug!("Skipping sell: leverage {:.2}x too high, no long to close", leverage);
                    } else {
                        debug!("Skipping sell: position {:.4} <= -90% of max {:.4}",
                               total_exposure, self.max_position);
                    }
                } else {
                    if let Some(ref order) = self.active_ask {
                        if let Ok(order_index) = order.order_id.parse::<i64>() {
                            let _ = self.trading.cancel_order(order_index).await;
                        }
                        self.active_ask = None;
                    }

                    match self.trading.place_order(OrderParams {
                        size: order_size,
                        price: our_ask,
                        side: Side::Sell,
                        order_type: OrderType::Limit,
                        reduce_only: false,
                    }).await {
                        Ok(result) => {
                            info!("Sell: ${:.2} x {:.4} ETH", our_ask, order_size);
                            self.active_ask = Some(ActiveOrder {
                                order_id: result.tx_hash,
                                side: Side::Sell,
                                price: our_ask,
                                size: order_size,
                                placed_at: Instant::now(),
                            });
                        }
                        Err(e) => {
                            warn!("Sell order failed: {}", e);
                        }
                    }
                }
            }

            // Step 8: Periodic PnL reporting
            if self.last_balance_check.elapsed() > Duration::from_secs(60) {
                self.print_pnl_update();
                self.last_balance_check = Instant::now();
            }

            tokio::time::sleep(Duration::from_millis(200)).await;  // 200ms = 5次/秒，高频
        }
    }

    /// Calculate order size based on available balance
    fn calculate_order_size(&self, available_balance: f64, mid_price: f64) -> f64 {
        // High-frequency: use 1% of available balance per order (small orders)
        let size_from_balance = (available_balance * 0.01) / mid_price;

        // Use base size as minimum
        let size = size_from_balance.max(self.base_order_size);

        // Cap at 0.01 ETH (~$20) for high-frequency trading
        let size = size.min(0.01);

        // Round to step size
        (size / self.step_size).floor() * self.step_size
    }

    /// Calculate adaptive spread based on volatility
    fn calculate_adaptive_spread(&self) -> u32 {
        let volatility = self.volatility_tracker.calculate_volatility();

        // Increase spread in high volatility
        let adjusted_spread = self.base_spread_bps as f64
            * (1.0 + volatility * self.volatility_multiplier * 10000.0);

        // Clamp to min/max
        adjusted_spread
            .max(self.min_spread_bps as f64)
            .min(self.max_spread_bps as f64) as u32
    }

    /// Calculate inventory skew to manage directional risk
    /// Returns (bid_skew, ask_skew) in dollars
    fn calculate_inventory_skew(&self, position: f64) -> (f64, f64) {
        // Normalize position to [-1, 1]
        let normalized_pos = (position / self.max_position).clamp(-1.0, 1.0);

        // Calculate skew: if long, widen bid and tighten ask
        let skew_amount = normalized_pos * self.inventory_skew_factor;

        let bid_skew = if normalized_pos > 0.0 { skew_amount } else { 0.0 };
        let ask_skew = if normalized_pos < 0.0 { -skew_amount } else { 0.0 };

        (bid_skew, ask_skew)
    }

    fn should_requote(&self, active_order: &Option<ActiveOrder>, new_price: f64) -> bool {
        match active_order {
            None => true,
            Some(order) => {
                // 1. Price deviation check
                let price_diff = (new_price - order.price).abs();
                let deviation_bps = (price_diff / order.price) * 10000.0;

                // 2. Time-based forced refresh (HFT: refresh every 1 second)
                let age = order.placed_at.elapsed();

                deviation_bps > 1.0 || age > Duration::from_secs(1)
            }
        }
    }

    async fn cancel_all_orders(&mut self) {
        if let Some(ref order) = self.active_bid {
            info!("Canceling bid");
            if let Ok(order_index) = order.order_id.parse::<i64>() {
                let _ = self.trading.cancel_order(order_index).await;
            }
        }

        if let Some(ref order) = self.active_ask {
            info!("Canceling ask");
            if let Ok(order_index) = order.order_id.parse::<i64>() {
                let _ = self.trading.cancel_order(order_index).await;
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
            "💰 PnL: ${:.2} ({:+.2}%) | Balance: ${:.2} | Leverage: {:.2}x | Margin: {:.1}%",
            pnl,
            pnl_pct,
            self.account_stats.available_balance,
            self.account_stats.leverage,
            self.account_stats.margin_usage * 100.0
        );
    }

    fn print_session_summary(&self) {
        let pnl = self.account_stats.available_balance - self.session_start_balance;
        let pnl_pct = if self.session_start_balance > 0.0 {
            (pnl / self.session_start_balance) * 100.0
        } else {
            0.0
        };

        info!("📊 Session Summary:");
        info!("   Start Balance: ${:.2}", self.session_start_balance);
        info!("   End Balance:   ${:.2}", self.account_stats.available_balance);
        info!("   PnL:           ${:.2} ({:+.2}%)", pnl, pnl_pct);
    }
}
