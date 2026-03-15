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
use crate::error::TradingError;
use crate::exchange::{BatchAction, Exchange, OrderParams, Side};
use crate::order_tracker::{OrderSide, OrderTracker};
use crate::shm_reader::{NUM_EXCHANGES, ShmBboMessage, ShmReader};
use crate::telemetry::TelemetryCollector;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

const LOW_MARGIN_THRESHOLD: f64 = 100.0; // $100

// ─── Account Stats ───────────────────────────────────────────────────────────
#[derive(Debug, Clone)]
pub struct AccountStats {
    pub available_balance: f64,
    pub portfolio_value: f64,
    pub position: f64,
    pub leverage: f64,
    pub margin_usage: f64,
    pub last_update: Instant,
}

impl Default for AccountStats {
    fn default() -> Self {
        Self {
            available_balance: 0.0,
            portfolio_value: 0.0,
            position: 0.0,
            leverage: 0.0,
            margin_usage: 0.0,
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
            margin_usage: snapshot.margin_usage,
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
        let n = self.price_samples.len();
        if n < 2 {
            return 10.0; // Default
        }

        // Zero-allocation single-pass variance calculation
        let mut sum = 0.0;
        let mut sum_sq = 0.0;
        let mut count = 0;

        for i in 0..n - 1 {
            let p1 = self.price_samples[i];
            let p2 = self.price_samples[i + 1];
            let ret = (p2 / p1 - 1.0) * 10000.0;
            sum += ret;
            sum_sq += ret * ret;
            count += 1;
        }

        let mean = sum / count as f64;
        let variance = (sum_sq / count as f64) - (mean * mean);
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
    order_index: Option<i64>,
    side: OrderSide,
    price: f64,
    #[allow(dead_code)]
    size: f64,
    #[allow(dead_code)]
    placed_at: Instant,
}

// ═══════════════════════════════════════════════════════════════════
// Constants & Configuration
// ═══════════════════════════════════════════════════════════════════

const DATA_STALENESS_THRESHOLD_MS: u64 = 5000;
const RECONCILE_INTERVAL_SEC: u64 = 30;
const GC_INTERVAL_SEC: u64 = 300;
const CROSS_EXCHANGE_FALLBACK_SPREAD_TICKS: f64 = 2.0;

// ─── Inventory-Neutral Market Maker ──────────────────────────────────────────
#[derive(Debug, Clone)]
struct PricingInputs {
    mid: f64,
    pricing_mid: f64,
    vol_bps: f64,
    as_score: f64,
    consensus_mid: Option<f64>,
}


pub struct InventoryNeutralMM {
    config: InventoryNeutralMMConfig,

    trading: Arc<dyn Exchange>,
    order_tracker: Arc<OrderTracker>,
    shm_reader: ShmReader,
    shm_depth_reader: Option<crate::shm_depth_reader::ShmDepthReader>,
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

    // Runtime Control
    is_running: Arc<AtomicBool>,
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
        let shm_depth_reader =
            crate::shm_depth_reader::ShmDepthReader::open("/dev/shm/aleph-depth", 2048).ok();

        if shm_depth_reader.is_some() {
            info!("📊 OBI+VWMicro pricing enabled (depth reader initialized)");
        } else {
            info!("📊 Using simple mid-price (depth reader not available)");
        }

        // Depth reader initialized (optional)

        Self {
            micro: MicrostructureTracker::new(
                config.micro_samples,
                config.ema_fast_period,
                config.ema_slow_period,
            ),
            config,
            trading,
            order_tracker,
            shm_depth_reader,
            account_stats_reader,
            account_stats: AccountStats::default(),
            active_orders: Vec::new(),
            session_start_balance: 0.0,
            total_orders_placed: 0,
            last_balance_check: Instant::now(),
            margin_cooldown_until: Instant::now(),
            telemetry: TelemetryCollector::new(),
            is_running: Arc::new(AtomicBool::new(true)),
            shm_reader,
        }
    }

    pub async fn run(
        &mut self,
        mut shutdown: Option<tokio::sync::watch::Receiver<bool>>,
    ) -> anyhow::Result<()> {
        info!("🎯 Inventory-Neutral MM started (Institutional Mode)");

        // Phase 0: Startup Cleanup & State Sync
        self.perform_startup_initialization().await.map_err(|e| TradingError::OrderFailed(e.to_string()))?;

        info!("🚀 Starting main execution loop...");
        while self.is_running.load(Ordering::SeqCst) {
            // 0. Check shutdown signal
            if let Some(ref mut rx) = shutdown
                && *rx.borrow() {
                self.perform_graceful_shutdown().await.map_err(|e| TradingError::OrderFailed(e.to_string()))?;
                return Ok(());
            }

            // Phase 1: Institutional Housekeeping
            self.perform_periodic_housekeeping().await;

            // Margin cooldown check
            if Instant::now() < self.margin_cooldown_until {
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }

            // Phase 2: Market Data Acquisition & Staleness Check
            let (exchanges, bbo) = match self.fetch_market_state().await {
                Some(state) => state,
                None => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };

            // Phase 3: Signal Processing & Fair Price Determination
            let position = self.account_stats.position;
            let inputs = self.calculate_pricing_inputs(&exchanges, &bbo);

            if inputs.mid <= 0.0 || inputs.mid.is_nan() || inputs.mid.is_infinite() {
                tracing::warn!("⚠️ Invalid mid price: {:.4}, skipping cycle", inputs.mid);
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }

            // Phase 4: Avellaneda-Stoikov Optimal Quoting
            if let Some((our_bid, our_ask)) = self.calculate_optimal_quotes(&inputs, position) {
                // Phase 5: Sizing & Execution
                self.execute_quoting_cycle(our_bid, our_ask, position, inputs.mid).await;
            }

            tokio::time::sleep(Duration::from_millis(self.config.poll_interval_ms)).await;
        }
        
        Ok(())
    }

    /// Startup sequence: cancel orders, sync state, and perform initial initialization
    async fn perform_startup_initialization(&mut self) -> anyhow::Result<()> {
        info!("📤 Startup cleanup: canceling existing orders...");
        let _ = self.trading.cancel_all().await;
        tokio::time::sleep(Duration::from_secs(2)).await;

        // Sync authoritative account data
        let stats = self.account_stats_reader.read();
        self.account_stats = stats.into();
        self.session_start_balance = self.account_stats.portfolio_value;
        
        info!("✅ Startup complete: pos={:.4} bal=${:.2}", self.account_stats.position, self.account_stats.portfolio_value);
        Ok(())
    }

    /// Estimate fill rate κ for A-S model from recent order fills
    /// Higher κ → tighter spread (fills are easy to get)
    /// Lower κ → wider spread (fills are scarce, need more edge)
    /// Returns fills per second (matching T's time unit)
    fn estimate_fill_rate(&self) -> f64 {
        let recent_fills = self
            .order_tracker
            .filled_count_since(Duration::from_secs(300));
        // κ = fills per second, floored at 0.01 to prevent ln(1) = 0
        let fills_per_sec = recent_fills as f64 / 300.0;
        fills_per_sec.max(0.01)
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
        let sigmoid_multiplier =
            1.0 + 2.0 * (self.config.sigmoid_steepness * normalized_pos.abs()).tanh();

        // Cap inventory_offset to prevent whiplash: flattening order <= cap_mult * base_order_size
        let max_offset =
            self.config.base_order_size * (self.config.flattening_cap_mult - 1.0).max(0.5);
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

        // MARGIN MANAGEMENT: Adjust order sizes based on available balance
        // ═══════════════════════════════════════════════════════════════════
        let available = self.account_stats.available_balance;
        let equity = self.account_stats.portfolio_value;
        let margin_usage = self.account_stats.margin_usage;

        // Lighter DEX specific: if available balance is misleadingly high (missing order locks),
        // we derive the true free margin from Equity & MarginUsage
        let safe_available = if margin_usage > 0.01 && equity > 0.0 {
            // MarginUsage is (Notional / Equity). True Free = Equity * (1 - Notional / (Equity * MaxLev))
            let true_free = equity * (1.0 - margin_usage / self.config.max_leverage);
            available.min(true_free).max(0.0)
        } else {
            available
        };

        // Estimate margin required per order using configured max leverage
        let margin_per_eth = mid / self.config.max_leverage;

        // Factor in the entire grid's margin (geometric series sum)
        let r = self.config.grid_size_decay;
        let n = self.config.grid_levels as i32;
        let grid_multiplier = if (1.0 - r).abs() < 1e-4 {
            n as f64
        } else {
            (1.0 - r.powi(n)) / (1.0 - r)
        };

        // Use safe_available for scaling
        let usable_balance = safe_available * 0.7;

        let bid_margin_required = bid_size * margin_per_eth * grid_multiplier;
        let ask_margin_required = ask_size * margin_per_eth * grid_multiplier;
        let total_margin_required = bid_margin_required + ask_margin_required;

        let (bid_size, ask_size) = if total_margin_required > usable_balance {
            // Insufficient margin: scale down proportionally
            let scale_factor = (usable_balance / total_margin_required).min(1.0);

            if scale_factor < 0.1 {
                // Too little margin: cancel active orders and skip this cycle
                warn!(
                    "Insufficient margin: available=${:.2} required=${:.2} (scale={:.1}%), skipping quotes",
                    available,
                    total_margin_required,
                    scale_factor * 100.0
                );
                (0.0, 0.0)
            } else {
                // Scale down order sizes
                let scaled_bid = bid_size * scale_factor;
                let scaled_ask = ask_size * scale_factor;
                debug!(
                    "Margin constraint: scaled orders by {:.1}% (available=${:.2})",
                    scale_factor * 100.0,
                    available
                );

                // Check if scaled sizes meet minimum requirements (Lighter DEX minimum ~$11)
                let min_size = 11.0 / mid;
                let final_bid = if scaled_bid < min_size {
                    0.0
                } else {
                    scaled_bid
                };
                let final_ask = if scaled_ask < min_size {
                    0.0
                } else {
                    scaled_ask
                };

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
        if self.active_orders.is_empty() {
            return;
        }

        let mut batch_actions = Vec::new();
        for order in &self.active_orders {
            if let Some(idx) = order.order_index {
                batch_actions.push(BatchAction::Cancel(idx));
            }
            self.order_tracker.mark_failed(order.client_order_id);
        }

        if !batch_actions.is_empty() {
            let _ = self.trading.execute_batch(batch_actions).await;
        }

        self.active_orders.clear();
        debug!("Canceled all active orders and synced tracker");
    }

    async fn perform_graceful_shutdown(&mut self) -> anyhow::Result<()> {
        info!("🛑 Graceful shutdown initiated...");
        self.is_running.store(false, Ordering::SeqCst);
        self.cancel_all_orders().await;
        let _ = self.trading.cancel_all().await;
        info!("👋 Strategy stopped cleanly.");
        Ok(())
    }

    fn print_pnl_update(&self) {
        let equity = self.account_stats.portfolio_value;
        let available = self.account_stats.available_balance;
        let margin_usage = self.account_stats.margin_usage;

        // Calculate SafeAvail for logging
        let safe_avail = if margin_usage > 0.01 && equity > 0.0 {
            let true_free = equity * (1.0 - margin_usage / self.config.max_leverage);
            available.min(true_free).max(0.0)
        } else {
            available
        };

        let pnl = equity - self.session_start_balance;
        let pnl_pct = if self.session_start_balance > 0.0 {
            (pnl / self.session_start_balance) * 100.0
        } else {
            0.0
        };

        info!(
            "📊 PnL: ${:.2} ({:+.3}%) | Equity: ${:.2} | Avail (Safe/Raw): ${:.2}/${:.2} | Margin: {:.1}% | Pos: {:.4} ETH | Orders: {} | Fills: {} ({:.1}/min) | Fees: ${:.4}",
            pnl,
            pnl_pct,
            equity,
            safe_avail,
            available,
            margin_usage * 100.0,
            self.account_stats.position,
            self.total_orders_placed,
            self.telemetry.fill_count,
            self.telemetry.fill_rate(),
            self.telemetry.total_fees_paid,
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

    /// Periodic tasks: reconciliation, GC, PnL reporting, and telemetry export
    async fn perform_periodic_housekeeping(&mut self) {
        if self.last_balance_check.elapsed() > Duration::from_secs(RECONCILE_INTERVAL_SEC) {
            // Sync balance
            if let Ok(stats) = self.trading.get_account_stats().await {
                self.account_stats = stats;
            }

            // Phase 1: Reconcile active orders from OrderTracker with actual open orders
            let stale_count = self.order_tracker.reconcile_with_exchange(&*self.trading).await;
            if stale_count > 0 {
                debug!(
                    "Periodic reconcile: cleared {} stale tracker entries (strategy has {})",
                    stale_count,
                    self.active_orders.len()
                );
            }
            self.order_tracker
                .gc_completed_orders(Duration::from_secs(GC_INTERVAL_SEC));
            // Phase 2: Verify atomic exposure matches locked traversal
            self.order_tracker.debug_verify_exposure();
            // Sync fill stats from OrderTracker → Telemetry
            let (fill_count, total_fees) = self.order_tracker.total_fill_stats();
            self.telemetry.fill_count = fill_count;
            self.telemetry.total_fees_paid = total_fees;
            self.telemetry.available_balance = self.account_stats.available_balance;
            self.telemetry.portfolio_value = self.account_stats.portfolio_value;
            self.telemetry.export_metrics();
            self.print_pnl_update();
            self.last_balance_check = Instant::now();
        }
    }
    /// Fetch market state from SHM and perform staleness check
    async fn fetch_market_state(&mut self) -> Option<([(u8, ShmBboMessage); NUM_EXCHANGES], ShmBboMessage)> {
        let exchanges = self.shm_reader.read_all_exchanges(self.config.symbol_id);
        let lighter_bbo = exchanges
            .iter()
            .find(|(exch_id, _)| *exch_id == self.config.exchange_id)
            .map(|(_, msg)| *msg);

        let bbo = match lighter_bbo.filter(|b| b.bid_price > 0.0 || b.ask_price > 0.0) {
            Some(b) => b,
            None => {
                tokio::time::sleep(Duration::from_millis(10)).await;
                return None;
            }
        };

        // Staleness check
        if bbo.timestamp_ns > 0 {
            let now_ns = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64;
            let data_age_ms = now_ns.saturating_sub(bbo.timestamp_ns) / 1_000_000;
            if data_age_ms > DATA_STALENESS_THRESHOLD_MS {
                warn!("Stale BBO: age={}ms (>{}ms), canceling all orders", data_age_ms, DATA_STALENESS_THRESHOLD_MS);
                self.cancel_all_orders().await;
                tokio::time::sleep(Duration::from_secs(1)).await;
                return None;
            }
        }

        Some((exchanges, bbo))
    }

    /// Calculate signal components and pricing inputs from market state
    fn calculate_pricing_inputs(&mut self, exchanges: &[(u8, ShmBboMessage); NUM_EXCHANGES], bbo: &ShmBboMessage) -> PricingInputs {
        // Calculate external consensus mid
        let external_mids: Vec<f64> = exchanges
            .iter()
            .filter(|(exch_id, _)| *exch_id != self.config.exchange_id)
            .filter(|(_, msg)| msg.bid_price > 0.0 && msg.ask_price > 0.0)
            .map(|(_, msg)| (msg.bid_price + msg.ask_price) / 2.0)
            .collect();
        
        let consensus_mid = if !external_mids.is_empty() {
            Some(external_mids.iter().sum::<f64>() / external_mids.len() as f64)
        } else {
            None
        };

        // Calculate local mid with illiquidity fallback
        let mid = if bbo.bid_price > 0.0 && bbo.ask_price > 0.0 {
            (bbo.bid_price + bbo.ask_price) / 2.0
        } else if let Some(ext_mid) = consensus_mid {
            ext_mid
        } else if bbo.bid_price > 0.0 {
            bbo.bid_price + (self.config.tick_size * CROSS_EXCHANGE_FALLBACK_SPREAD_TICKS)
        } else if bbo.ask_price > 0.0 {
            bbo.ask_price - (self.config.tick_size * CROSS_EXCHANGE_FALLBACK_SPREAD_TICKS)
        } else {
            0.0
        };

        // Calculate VWMicro price if depth data available
        let pricing_mid = if let Some(ref depth_reader) = self.shm_depth_reader {
            if let Some(depth) = depth_reader.read_depth(self.config.symbol_id, self.config.exchange_id) {
                let bid_p = if bbo.bid_price > 0.0 { bbo.bid_price } else { mid - self.config.tick_size };
                let ask_p = if bbo.ask_price > 0.0 { bbo.ask_price } else { mid + self.config.tick_size };
                self.calculate_vw_micro_price(&depth, bid_p, ask_p)
            } else {
                mid
            }
        } else {
            mid
        };


        // Update microstructure
        self.micro.update(pricing_mid);
        let vol_bps = self.micro.volatility_bps();
        let as_score = self.micro.adverse_selection_score();

        PricingInputs {
            mid,
            pricing_mid,
            vol_bps,
            as_score,
            consensus_mid,
        }
    }

    /// Avellaneda-Stoikov pricing logic
    fn calculate_optimal_quotes(&self, inputs: &PricingInputs, q: f64) -> Option<(f64, f64)> {
        // Adverse selection filter
        if inputs.as_score > self.config.adverse_selection_threshold {
            debug!("AS filter triggered: score={:.2}", inputs.as_score);
            return None;
        }

        // Cross-exchange AS filter
        let cross_shift = if let Some(cross_mid) = inputs.consensus_mid {
            let cross_signal_bps = (cross_mid - inputs.mid) / inputs.mid * 10000.0;
            if cross_signal_bps.abs() > self.config.cross_exchange_as_threshold {
                debug!("Cross-exchange AS triggered: signal={:.1}bps", cross_signal_bps);
                return None;
            }
            cross_signal_bps * self.config.cross_exchange_scale / 10000.0 * inputs.mid
        } else {
            0.0
        };

        let gamma = self.config.as_gamma;
        let time_horizon = self.config.as_time_horizon_sec;
        let sigma = inputs.vol_bps / 10000.0;

        // Reservation price: mid shifted by inventory risk + cross-exchange signal
        let reservation_price = inputs.pricing_mid * (1.0 - gamma * sigma * sigma * q * time_horizon) + cross_shift;

        // Spread logic
        let kappa = self.config.as_kappa + self.estimate_fill_rate();
        let gamma_safe = gamma.max(1e-6);
        let optimal_spread = gamma * sigma * sigma * time_horizon + (2.0 / gamma_safe) * (1.0 + gamma_safe / kappa).ln();
        let half_spread_raw = optimal_spread / 2.0 * inputs.pricing_mid;

        // Clamping and floors
        let max_half_spread = inputs.pricing_mid * self.config.max_spread_bps / 10000.0 / 2.0;
        let fee_floor = inputs.pricing_mid * (self.config.maker_fee_bps * 2.0 + self.config.min_profit_bps) / 10000.0 / 2.0;
        let half_spread = half_spread_raw.clamp(fee_floor, max_half_spread);

        let our_bid = ((reservation_price - half_spread) / self.config.tick_size).floor() * self.config.tick_size;
        let our_ask = ((reservation_price + half_spread) / self.config.tick_size).ceil() * self.config.tick_size;

        if our_bid >= our_ask {
            return None;
        }

        Some((our_bid, our_ask))
    }

    /// Execute the quoting cycle: size orders and submit batch
    async fn execute_quoting_cycle(&mut self, our_bid: f64, our_ask: f64, position: f64, mid: f64) {
        let actual_spread_bps = ((our_ask - our_bid) / mid) * 10000.0;
        self.telemetry.update_spread_size(actual_spread_bps);
        self.telemetry.update_adverse_selection(self.micro.adverse_selection_score());

        let min_spread_bps = self.config.maker_fee_bps * 2.0 + self.config.min_profit_bps;
        if actual_spread_bps < min_spread_bps {
            return;
        }

        let (bid_size, ask_size) = self.calculate_asymmetric_sizes(position, mid);

        // Low margin check
        if bid_size < self.config.base_order_size && ask_size < self.config.base_order_size {
            if self.account_stats.available_balance < LOW_MARGIN_THRESHOLD {
                warn!("Low margin (${:.2}), clearing orders", self.account_stats.available_balance);
                let _ = self.trading.cancel_all().await;
            }
            return;
        }

        // Identify re-quote needs
        let requote_threshold = mid * self.config.requote_threshold_bps / 10000.0;
        let mut actions = Vec::new();

        self.prepare_side_actions(Side::Buy, our_bid, bid_size, requote_threshold, &mut actions).await;
        self.prepare_side_actions(Side::Sell, our_ask, ask_size, requote_threshold, &mut actions).await;

        if !actions.is_empty() {
            let _ = self.trading.execute_batch(actions).await;
        }
    }

// Removed unused legacy method: cancel_all_orders_legacy

    async fn prepare_side_actions(&mut self, side: Side, target_px: f64, total_sz: f64, threshold: f64, actions: &mut Vec<BatchAction>) {
        let order_side = match side {
            Side::Buy => OrderSide::Buy,
            Side::Sell => OrderSide::Sell,
        };
        let side_orders: Vec<_> = self.active_orders.iter()
            .filter(|o| o.side == order_side)
            .collect();
        
        let needs_requote = side_orders.is_empty() || side_orders.iter().any(|o| (o.price - target_px).abs() > threshold);

        if needs_requote {
            for o in side_orders {
                if let Ok(oid) = o.order_id.parse::<i64>() {
                    let _ = self.trading.cancel_order(oid).await;
                }
            }
            if total_sz >= self.config.base_order_size {
                self.generate_grid_places(side, target_px, total_sz, actions);
            }
        }
    }

    fn generate_grid_places(&self, side: Side, start_px: f64, total_sz: f64, actions: &mut Vec<BatchAction>) {
        let mut remaining_sz = total_sz;
        let mut current_px = start_px;

        for i in 0..self.config.grid_levels {
            let level_sz = if i == self.config.grid_levels - 1 {
                remaining_sz
            } else {
                total_sz / (self.config.grid_levels as f64) * (1.0 - self.config.grid_size_decay).powi(i as i32)
            };

            if level_sz < self.config.base_order_size { break; }

            actions.push(BatchAction::Place(OrderParams {
                size: level_sz,
                price: current_px,
                side,
                order_type: self.trading.limit_order_type(),
                reduce_only: false,
            }));

            remaining_sz -= level_sz;
            let offset = self.config.grid_spacing_bps / 10000.0 * start_px;
            if side == Side::Buy { current_px -= offset; } else { current_px += offset; }
            if remaining_sz < self.config.base_order_size { break; }
        }
    }
}


#[cfg(test)]
mod tests;
