//! Adaptive Market Maker Strategy (Premium Account, Fee-Aware)
//!
//! Profitable HFT market making on Lighter DEX:
//! - Fee-aware spread: ensures spread > round-trip maker fee (0.76bps)
//! - Adverse selection filter: pauses quoting during fast price moves
//! - Aggressive inventory skew: linear skew on both sides to flatten position
//! - Batch quoting: sendTxBatch for paired bid/ask (1 API call)
//! - Premium account: 0ms maker latency, 6000 req/min, maker fee 0.0038%

use crate::account_stats_reader::{AccountStatsReader, AccountStatsSnapshot};
use crate::error::Result;
use crate::lighter_trading::{BatchOrderParams, LighterTrading, Side};
use crate::shadow_ledger::ShadowLedger;
use crate::shm_reader::ShmReader;
use parking_lot::RwLock;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

// ─── Premium Account Fee Constants (3000 LIT staked) ─────────────────────────
const MAKER_FEE_BPS: f64 = 0.38;   // 0.0038%
#[allow(dead_code)]
const TAKER_FEE_BPS: f64 = 2.66;   // 0.0266% (for reference, we aim for maker fills)
const ROUND_TRIP_FEE_BPS: f64 = MAKER_FEE_BPS * 2.0; // 0.76bps both sides maker

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

/// Market microstructure tracker: volatility, momentum, adverse selection
struct MicrostructureTracker {
    // Volatility (realized vol from returns)
    price_samples: VecDeque<f64>,
    max_samples: usize,

    // EMA for momentum / adverse selection detection
    ema_fast: f64,       // Fast EMA (5-tick)
    ema_slow: f64,       // Slow EMA (20-tick)
    ema_fast_alpha: f64,
    ema_slow_alpha: f64,
    ema_initialized: bool,

    // Trade flow imbalance (orderbook pressure)
    last_bid: f64,
    last_ask: f64,
}

impl MicrostructureTracker {
    fn new(max_samples: usize) -> Self {
        Self {
            price_samples: VecDeque::with_capacity(max_samples),
            max_samples,
            ema_fast: 0.0,
            ema_slow: 0.0,
            ema_fast_alpha: 2.0 / 6.0,   // 5-tick EMA
            ema_slow_alpha: 2.0 / 21.0,  // 20-tick EMA
            ema_initialized: false,
            last_bid: 0.0,
            last_ask: 0.0,
        }
    }

    fn update(&mut self, mid: f64, bid: f64, ask: f64) {
        // Update price samples for volatility
        if self.price_samples.len() >= self.max_samples {
            self.price_samples.pop_front();
        }
        self.price_samples.push_back(mid);

        // Update EMAs
        if !self.ema_initialized {
            self.ema_fast = mid;
            self.ema_slow = mid;
            self.ema_initialized = true;
        } else {
            self.ema_fast += self.ema_fast_alpha * (mid - self.ema_fast);
            self.ema_slow += self.ema_slow_alpha * (mid - self.ema_slow);
        }

        self.last_bid = bid;
        self.last_ask = ask;
    }

    /// Realized volatility (std dev of returns) in bps
    fn volatility_bps(&self) -> f64 {
        if self.price_samples.len() < 3 {
            return 0.0;
        }
        let returns: Vec<f64> = self.price_samples
            .iter()
            .zip(self.price_samples.iter().skip(1))
            .map(|(p1, p2)| ((p2 / p1) - 1.0) * 10000.0) // in bps
            .collect();
        let mean = returns.iter().sum::<f64>() / returns.len() as f64;
        let variance = returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / returns.len() as f64;
        variance.sqrt()
    }

    /// Momentum signal: (fast_ema - slow_ema) / mid in bps
    /// Positive = price trending up, negative = trending down
    fn momentum_bps(&self) -> f64 {
        if !self.ema_initialized || self.ema_slow == 0.0 {
            return 0.0;
        }
        ((self.ema_fast - self.ema_slow) / self.ema_slow) * 10000.0
    }

    /// Adverse selection score: abs(momentum) relative to volatility
    /// > 1.0 means directional move exceeds normal noise → toxic flow
    fn adverse_selection_score(&self) -> f64 {
        let vol = self.volatility_bps();
        if vol < 0.1 { return 0.0; }
        self.momentum_bps().abs() / vol
    }

    /// Orderbook imbalance: (bid_size - ask_size) / (bid_size + ask_size)
    /// Range [-1, 1]: positive = more bids (bullish pressure)
    fn book_imbalance(&self, bid_size: f64, ask_size: f64) -> f64 {
        let total = bid_size + ask_size;
        if total < 0.0001 { return 0.0; }
        (bid_size - ask_size) / total
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
    #[allow(dead_code)]
    market_id: u16,

    // Strategy parameters (fee-aware)
    base_spread_bps: f64,          // Base spread in bps (must > ROUND_TRIP_FEE_BPS)
    min_spread_bps: f64,           // Floor: never quote tighter than this
    max_spread_bps: f64,           // Ceiling
    volatility_scale: f64,         // How much vol widens spread

    // Position sizing
    base_order_size: f64,
    max_position: f64,
    inventory_skew_bps: f64,       // Max skew in bps at full inventory

    // Risk management
    max_leverage: f64,
    min_available_balance: f64,
    adverse_selection_threshold: f64, // Pause quoting when AS score > this

    // Market precision
    tick_size: f64,
    step_size: f64,

    // State
    trading: Arc<LighterTrading>,
    #[allow(dead_code)]
    ledger: Arc<RwLock<ShadowLedger>>,
    shm_reader: ShmReader,
    account_stats_reader: AccountStatsReader,
    account_stats: AccountStats,
    micro: MicrostructureTracker,

    // Order management
    active_bid: Option<ActiveOrder>,
    active_ask: Option<ActiveOrder>,

    // Fee-aware PnL tracking
    session_start_balance: f64,
    total_orders_placed: u64,
    total_batches: u64,
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
            // Spread: 5bps base, min 2bps (> 0.76bps round-trip fee), max 20bps
            base_spread_bps: 5.0,
            min_spread_bps: 2.0,
            max_spread_bps: 20.0,
            volatility_scale: 2.0,
            base_order_size: 0.03,         // ~$62 per order at $2080
            max_position: 0.4,             // Target ~0.3 ETH utilization
            inventory_skew_bps: 4.0,       // ±4bps skew at max inventory (stronger for larger pos)
            max_leverage: 10.0,
            min_available_balance: 2.0,
            adverse_selection_threshold: 1.5, // Pause when momentum > 1.5x vol
            tick_size: 0.01,
            step_size: 0.0001,
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
            total_batches: 0,
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
        let max_retries = 30; // Wait up to 30s for feeder to connect and push stats
        loop {
            let stats = self.account_stats_reader.read();

            // Check if data is fresh (updated within last 60 seconds at startup)
            let data_age_ns = start_time.saturating_sub(stats.updated_at);
            let data_age_secs = data_age_ns / 1_000_000_000;

            if (stats.collateral > 0.0 || stats.available_balance > 0.0) && data_age_secs < 60 {
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
        if self.account_stats.available_balance < self.min_available_balance {
            error!(
                "❌ SAFETY CHECK FAILED: Balance ${:.2} < ${:.2}",
                self.account_stats.available_balance, self.min_available_balance
            );
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

            // Step 2: Read position from account stats (updated via WS position events, <200ms latency)
            let total_exposure = self.account_stats.position;

            if total_exposure.abs() > self.max_position {
                warn!("Position {:.4} > max {:.4}, reducing only", total_exposure, self.max_position);
                self.cancel_all_orders().await;
            }

            // Step 3: Read market data
            let exchanges = self.shm_reader.read_all_exchanges(self.symbol_id);
            let lighter_bbo = exchanges
                .iter()
                .find(|(exch_id, _)| *exch_id == 2)
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

            // Step 4: Update microstructure tracker
            self.micro.update(mid, bbo.bid_price, bbo.ask_price);
            let vol_bps = self.micro.volatility_bps();
            let momentum = self.micro.momentum_bps();
            let as_score = self.micro.adverse_selection_score();
            let book_imb = self.micro.book_imbalance(bbo.bid_size, bbo.ask_size);

            // Step 5: Adverse selection filter — cancel resting orders during toxic flow
            if as_score > self.adverse_selection_threshold {
                debug!(
                    "AS filter: score={:.2} momentum={:.1}bps vol={:.1}bps — canceling + pausing",
                    as_score, momentum, vol_bps
                );
                self.cancel_all_orders().await;
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }

            // Step 6: Calculate fee-aware adaptive spread
            //   spread = max(min_spread, base + vol_component + fee_buffer)
            let vol_component = vol_bps * self.volatility_scale;
            let raw_spread_bps = self.base_spread_bps + vol_component;
            let spread_bps = raw_spread_bps.clamp(self.min_spread_bps, self.max_spread_bps);

            // Step 7: Inventory skew — linear, applied to BOTH sides
            //   Long position → lower bid (less eager to buy), lower ask (eager to sell)
            //   Short position → higher bid (eager to buy), higher ask (less eager to sell)
            let inv_ratio = (total_exposure / self.max_position).clamp(-1.0, 1.0);
            let skew_bps = inv_ratio * self.inventory_skew_bps;

            // Orderbook imbalance micro-adjustment (±0.5bps)
            // If more bids in book (bullish), slightly tighten ask
            let imb_adj_bps = book_imb * 0.5;

            // Step 8: Compute final quotes
            let half_spread = mid * spread_bps / 20000.0;
            let skew_dollars = mid * skew_bps / 10000.0;
            let imb_dollars = mid * imb_adj_bps / 10000.0;

            let our_bid = mid - half_spread - skew_dollars - imb_dollars;
            let our_ask = mid + half_spread - skew_dollars - imb_dollars;

            // Round to tick
            let our_bid = (our_bid / self.tick_size).floor() * self.tick_size;
            let our_ask = (our_ask / self.tick_size).ceil() * self.tick_size;

            // Sanity: ensure spread is positive and covers fees
            let actual_spread_bps = ((our_ask - our_bid) / mid) * 10000.0;
            if actual_spread_bps < ROUND_TRIP_FEE_BPS {
                debug!("Spread {:.1}bps < fee {:.1}bps, skipping", actual_spread_bps, ROUND_TRIP_FEE_BPS);
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }

            let order_size = self.calculate_order_size(available_balance, mid);

            debug!(
                "Mid={:.2} Sprd={:.1}bps Mkt={:.1}bps Vol={:.1} Mom={:.1} AS={:.2} Imb={:.2} Pos={:.4}",
                mid, actual_spread_bps, market_spread_bps, vol_bps, momentum, as_score, book_imb, total_exposure
            );

            // Step 9: Determine which sides to quote
            let should_requote_bid = self.should_requote(&self.active_bid, our_bid);
            let should_requote_ask = self.should_requote(&self.active_ask, our_ask);

            let can_buy = total_exposure < self.max_position * 0.9 && !leverage_too_high;
            let would_close_long = total_exposure > 0.0;
            let can_sell = total_exposure > -self.max_position * 0.9
                && (!leverage_too_high || would_close_long);

            let place_bid = should_requote_bid && can_buy;
            let place_ask = should_requote_ask && can_sell;

            // Step 10: Cancel stale orders before placing new ones
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

            // Step 11: Place orders — batch when both sides, single otherwise
            if place_bid && place_ask {
                match self.trading.place_batch(BatchOrderParams {
                    bid_price: our_bid,
                    ask_price: our_ask,
                    bid_size: order_size,
                    ask_size: order_size,
                }).await {
                    Ok(result) => {
                        info!(
                            "Batch: Bid ${:.2} / Ask ${:.2} x {:.4} sprd={:.1}bps",
                            our_bid, our_ask, order_size, actual_spread_bps
                        );
                        self.active_bid = Some(ActiveOrder {
                            order_id: result.bid_client_order_index.to_string(),
                            side: Side::Buy,
                            price: our_bid,
                            size: order_size,
                            placed_at: Instant::now(),
                        });
                        self.active_ask = Some(ActiveOrder {
                            order_id: result.ask_client_order_index.to_string(),
                            side: Side::Sell,
                            price: our_ask,
                            size: order_size,
                            placed_at: Instant::now(),
                        });
                        self.total_batches += 1;
                        self.total_orders_placed += 2;
                    }
                    Err(e) => warn!("Batch failed: {}", e),
                }
            } else if place_bid {
                match self.trading.buy(order_size, our_bid).await {
                    Ok(result) => {
                        info!("Buy: ${:.2} x {:.4}", our_bid, order_size);
                        self.active_bid = Some(ActiveOrder {
                            order_id: result.tx_hash,
                            side: Side::Buy,
                            price: our_bid,
                            size: order_size,
                            placed_at: Instant::now(),
                        });
                        self.total_orders_placed += 1;
                    }
                    Err(e) => warn!("Buy failed: {}", e),
                }
            } else if place_ask {
                match self.trading.sell(order_size, our_ask).await {
                    Ok(result) => {
                        info!("Sell: ${:.2} x {:.4}", our_ask, order_size);
                        self.active_ask = Some(ActiveOrder {
                            order_id: result.tx_hash,
                            side: Side::Sell,
                            price: our_ask,
                            size: order_size,
                            placed_at: Instant::now(),
                        });
                        self.total_orders_placed += 1;
                    }
                    Err(e) => warn!("Sell failed: {}", e),
                }
            }

            // Periodic PnL reporting (every 30s)
            if self.last_balance_check.elapsed() > Duration::from_secs(30) {
                self.print_pnl_update();
                self.last_balance_check = Instant::now();
            }

            // Premium: 6000 req/min, batch=1 req, cancel=1 each → ~3 req/cycle
            // 100ms = 10 cycles/s = ~30 req/s (well within limits)
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    fn calculate_order_size(&self, available_balance: f64, mid_price: f64) -> f64 {
        // Use 4% of available balance per order (with 10x leverage, ~40% notional)
        let size_from_balance = (available_balance * 0.04) / mid_price;
        let size = size_from_balance.max(self.base_order_size);
        // Enforce Lighter min quote amount ($11 > $10 min)
        let min_size = 11.0 / mid_price;
        let size = size.max(min_size);
        let size = size.min(0.2); // Cap per-order at 0.2 ETH
        (size / self.step_size).floor() * self.step_size
    }

    fn should_requote(&self, active_order: &Option<ActiveOrder>, new_price: f64) -> bool {
        match active_order {
            None => true, // No order on this side → must place
            Some(order) => {
                let deviation_bps = ((new_price - order.price).abs() / order.price) * 10000.0;
                let age = order.placed_at.elapsed();
                // Requote only if:
                // - Price moved significantly (>3bps) — our edge is gone
                // - Order is very stale (>10s) — refresh to stay competitive
                // This lets orders rest on the book and get filled
                deviation_bps > 3.0 || age > Duration::from_secs(10)
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
            "PnL: ${:.2} ({:+.2}%) | Bal: ${:.2} | Lev: {:.2}x | Pos: {:.4} | Orders: {} Batches: {}",
            pnl, pnl_pct,
            self.account_stats.available_balance,
            self.account_stats.leverage,
            self.account_stats.position,
            self.total_orders_placed,
            self.total_batches,
        );
    }

    fn print_session_summary(&self) {
        let pnl = self.account_stats.available_balance - self.session_start_balance;
        let pnl_pct = if self.session_start_balance > 0.0 {
            (pnl / self.session_start_balance) * 100.0
        } else {
            0.0
        };
        info!("Session: ${:.2} → ${:.2} | PnL: ${:.2} ({:+.2}%) | Orders: {} Batches: {}",
            self.session_start_balance, self.account_stats.available_balance,
            pnl, pnl_pct, self.total_orders_placed, self.total_batches);
    }
}
