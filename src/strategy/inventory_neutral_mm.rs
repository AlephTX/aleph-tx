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
use crate::exchange::{BatchAction, Exchange, Side};
use crate::order_tracker::{OrderLifecycle, OrderSide, OrderTracker};
use crate::shm_reader::{NUM_EXCHANGES, ShmBboMessage, ShmReader};
use crate::telemetry::TelemetryCollector;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

mod components;
mod execution;
mod housekeeping;
mod market_state;
mod pricing;
use components::{
    apply_risk_limits, decide_quote_cycle, inventory_deadband_size, inventory_skew_ratio, position_for_quoting,
    residual_exposure_abs, safe_available_balance, scaled_base_order_size,
    scaled_inventory_urgency_threshold, scaled_max_position, scaled_min_available_balance,
    toxicity_size_scale, toxicity_spread_multiplier, usable_balance_fraction,
    utilization_floor_base_order_size,
    QuoteCycleDecision, QuoteTarget, RiskSnapshot,
};
use execution::{
    apply_batch_success, build_side_execution_plan, classify_batch_failure,
    max_side_requote_replacements_per_cycle, resolve_cancel_client_order_ids,
    should_defer_cancel_only_refresh, should_defer_micro_refresh,
    should_defer_one_sided_requote,
    should_defer_post_fill_replenishment,
    size_tolerance_ratio_for_requote, BatchFailureAction,
};
use housekeeping::{reconcile_interval, sync_telemetry_snapshot};
use market_state::{
    build_market_state, classify_stale_bbo, cross_exchange_offset_bps, data_age_ms,
    external_reference_mid, MarketState, StaleBboAction,
};
use pricing::{
    anchor_quotes_to_touch, cleanup_reference_mid, effective_penny_ticks,
    fallback_bbo_prices, local_reference_mid, stabilize_crossed_quotes,
};

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
    client_order_id: i64,
    order_index: Option<i64>,
    lifecycle: OrderLifecycle,
    side: OrderSide,
    price: f64,
    #[allow(dead_code)]
    size: f64,
    #[allow(dead_code)]
    placed_at: Instant,
}

impl ActiveOrder {
    fn is_resting(&self) -> bool {
        matches!(
            self.lifecycle,
            OrderLifecycle::Open | OrderLifecycle::PartiallyFilled | OrderLifecycle::PendingCancel
        )
    }
}

// ═══════════════════════════════════════════════════════════════════
// Constants & Configuration
// ═══════════════════════════════════════════════════════════════════

const DATA_STALENESS_THRESHOLD_MS: u64 = 5000;
const STALE_BBO_CANCEL_AFTER_MS: u64 = 15000;
const STATIC_TWO_SIDED_BOOK_GRACE_MS: u64 = 30000;
const STATIC_QUOTEABLE_BOOK_GRACE_MS: u64 = 12000;
const RECONCILE_INTERVAL_SEC: u64 = 30;
const ACTIVE_POSITION_RECONCILE_INTERVAL_SEC: u64 = 3;
const GC_INTERVAL_SEC: u64 = 300;
const LOCAL_FALLBACK_SPREAD_TICKS: f64 = 2.0;
const MAX_TOUCH_OFFSET_BPS: f64 = 8.0;
const EXTERNAL_OVERLAY_MAX_BPS: f64 = 2.0;
const EXTERNAL_OVERLAY_SANITY_BPS: f64 = 25.0;
// ─── Inventory-Neutral Market Maker ──────────────────────────────────────────
#[derive(Debug, Clone)]
struct PricingInputs {
    mid: f64,
    pricing_mid: f64,
    bid_touch: f64,
    ask_touch: f64,
    vol_bps: f64,
    as_score: f64,
    external_offset_bps: f64,
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
    reconcile_failures: u32,
    last_reconciled_fill_count: u64,
    last_fill_at: Option<Instant>,
    last_execution_batch_at: Option<Instant>,
    margin_cooldown_until: Instant,
    stale_bbo_since_ns: Option<u64>,

    // Telemetry
    telemetry: TelemetryCollector,

    // Runtime Control
    is_running: Arc<AtomicBool>,
}

impl InventoryNeutralMM {
    fn tracked_order_count(&self) -> usize {
        self.active_orders.len()
    }

    fn resting_order_count(&self) -> usize {
        self.active_orders
            .iter()
            .filter(|order| order.is_resting())
            .count()
    }

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
            reconcile_failures: 0,
            last_reconciled_fill_count: 0,
            last_fill_at: None,
            last_execution_batch_at: None,
            margin_cooldown_until: Instant::now(),
            stale_bbo_since_ns: None,
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

            self.refresh_active_orders_from_tracker();
            if let Some(stats) = self.account_stats_reader.read_if_updated() {
                self.account_stats = stats.into();
            }

            // Phase 1: Institutional Housekeeping
            self.perform_periodic_housekeeping().await;

            // Margin cooldown check
            if Instant::now() < self.margin_cooldown_until {
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }

            // Phase 2: Market Data Acquisition & Staleness Check
            let market_state = match self.fetch_market_state().await {
                Some(state) => state,
                None => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };

            // Phase 3: Signal Processing & Fair Price Determination
            let inputs = self.calculate_pricing_inputs(&market_state.exchanges, &market_state.bbo);

            if inputs.mid <= 0.0 || inputs.mid.is_nan() || inputs.mid.is_infinite() {
                tracing::warn!("⚠️ Invalid mid price: {:.4}, skipping cycle", inputs.mid);
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }

            let quoting_position = {
                let max_position = scaled_max_position(
                    &self.config,
                    self.account_stats.portfolio_value,
                    inputs.mid,
                )
                .max(self.config.step_size);
                let mut runtime_config = self.config.clone();
                runtime_config.max_position = max_position;
                runtime_config.inventory_urgency_threshold = scaled_inventory_urgency_threshold(
                    &self.config,
                    self.account_stats.portfolio_value,
                    inputs.mid,
                    max_position,
                );
                position_for_quoting(
                    &runtime_config,
                    self.account_stats.position,
                    self.order_tracker.confirmed_position(),
                )
            };

            // Phase 4: Avellaneda-Stoikov Optimal Quoting
            if let Some((our_bid, our_ask)) = self.calculate_optimal_quotes(&inputs, quoting_position) {
                // Phase 5: Sizing & Execution
                self.execute_quoting_cycle(our_bid, our_ask, quoting_position, inputs.mid).await;
            }

            tokio::time::sleep(Duration::from_millis(self.config.poll_interval_ms)).await;
        }
        
        Ok(())
    }

    /// Startup sequence: cancel orders, sync state, and perform initial initialization
    async fn perform_startup_initialization(&mut self) -> anyhow::Result<()> {
        self.clean_exchange_state("startup").await?;

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

    fn build_risk_snapshot(&self, mid: f64) -> RiskSnapshot {
        let exchange_position = self.account_stats.position;
        // Quote direction should follow confirmed inventory, not pending-order imbalance.
        // Pending buy/sell exposure is still enforced via worst_case_long/short in risk limits.
        let mut tracker_confirmed = self.order_tracker.confirmed_position();
        let mut base_order_size = components::round_down_to_step(
            scaled_base_order_size(&self.config, self.account_stats.portfolio_value, mid)
                .max(self.config.step_size),
            self.config.step_size,
        )
        .max(self.config.step_size);
        let max_position =
            scaled_max_position(&self.config, self.account_stats.portfolio_value, mid)
                .max(base_order_size);
        let inventory_urgency_threshold = scaled_inventory_urgency_threshold(
            &self.config,
            self.account_stats.portfolio_value,
            mid,
            max_position,
        );
        let min_available_balance =
            scaled_min_available_balance(&self.config, self.account_stats.portfolio_value);

        let force_sync_threshold = (base_order_size * 2.0).max(self.config.step_size * 10.0);
        let opposite_sign = exchange_position.abs() >= self.config.step_size
            && tracker_confirmed.abs() >= self.config.step_size
            && exchange_position.signum() != tracker_confirmed.signum();
        let excessive_drift = (exchange_position - tracker_confirmed).abs() >= force_sync_threshold;
        if opposite_sign || excessive_drift {
            self.order_tracker.force_sync_position(exchange_position);
            tracker_confirmed = exchange_position;
        }

        let mut runtime_config = self.config.clone();
        runtime_config.max_position = max_position;
        runtime_config.inventory_urgency_threshold = inventory_urgency_threshold;
        let position_for_quoting =
            position_for_quoting(&runtime_config, exchange_position, tracker_confirmed);

        let raw_available_balance = self.account_stats.available_balance.max(0.0);
        let available_balance = safe_available_balance(
            raw_available_balance,
            self.account_stats.portfolio_value,
            self.account_stats.margin_usage,
            self.config.max_leverage,
        );
        let position_ratio = (position_for_quoting / max_position.max(self.config.step_size)).abs();
        let mut usable_balance =
            available_balance * usable_balance_fraction(position_ratio, self.account_stats.margin_usage);
        let margin_per_eth = mid / self.config.max_leverage;
        let r = self.config.grid_size_decay;
        let n = self.config.grid_levels as i32;
        let grid_multiplier = if (1.0 - r).abs() < 1e-4 {
            n as f64
        } else {
            (1.0 - r.powi(n)) / (1.0 - r)
        };
        base_order_size = components::round_down_to_step(
            utilization_floor_base_order_size(
                &self.config,
                base_order_size,
                usable_balance,
                grid_multiplier,
                mid,
            )
            .max(self.config.step_size),
            self.config.step_size,
        )
        .max(self.config.step_size);
        let max_position = max_position.max(base_order_size);
        usable_balance =
            available_balance * usable_balance_fraction(position_ratio, self.account_stats.margin_usage);

        RiskSnapshot {
            raw_available_balance,
            position_for_quoting,
            worst_case_long: self.order_tracker.worst_case_long(),
            worst_case_short: self.order_tracker.worst_case_short(),
            base_order_size,
            max_position,
            inventory_urgency_threshold,
            min_available_balance,
            available_balance,
            usable_balance,
            margin_per_eth,
            grid_multiplier,
        }
    }

    fn build_quote_target(
        &mut self,
        our_bid: f64,
        our_ask: f64,
        mid: f64,
    ) -> Option<QuoteTarget> {
        let spread_bps = ((our_ask - our_bid) / mid) * 10000.0;
        self.telemetry.update_spread_size(spread_bps);
        self.telemetry.update_adverse_selection(self.micro.adverse_selection_score());

        let min_spread_bps = self.config.maker_fee_bps * 2.0 + self.config.min_profit_bps;
        if spread_bps < min_spread_bps {
            debug!(
                "Skipping quote target: spread_bps={:.2} below min_spread_bps={:.2}",
                spread_bps,
                min_spread_bps
            );
            return None;
        }

        let risk = self.build_risk_snapshot(mid);
        let (bid_size, ask_size) = self.calculate_asymmetric_sizes(&risk, mid);
        let toxicity_scale = toxicity_size_scale(
            self.micro.adverse_selection_score(),
            self.config.adverse_selection_threshold,
        );
        let bid_size = components::round_down_to_step(bid_size * toxicity_scale, self.config.step_size);
        let ask_size = components::round_down_to_step(ask_size * toxicity_scale, self.config.step_size);

        Some(QuoteTarget {
            bid_price: our_bid,
            ask_price: our_ask,
            bid_size,
            ask_size,
        })
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
    fn calculate_asymmetric_sizes(&self, risk: &RiskSnapshot, mid: f64) -> (f64, f64) {
        Self::calculate_asymmetric_sizes_for_config(&self.config, risk, mid)
    }

    fn calculate_asymmetric_sizes_for_config(
        config: &InventoryNeutralMMConfig,
        risk: &RiskSnapshot,
        mid: f64,
    ) -> (f64, f64) {
        let position = risk.position_for_quoting;
        let inventory_deadband = inventory_deadband_size(
            config,
            risk.base_order_size,
            risk.inventory_urgency_threshold.max(config.step_size),
            mid,
        );
        // Hard stop: if position at limit, only allow flattening orders
        if position.abs() >= risk.max_position {
            let min_size = 11.0 / mid;
            if position > 0.0 {
                // Long at limit: only allow sells
                return (0.0, min_size.max(risk.base_order_size));
            } else {
                // Short at limit: only allow buys
                return (min_size.max(risk.base_order_size), 0.0);
            }
        }

        // Small residual inventory should not collapse the book into one-sided churn.
        // Keep the quote stack two-sided inside a deadband roughly equal to one top-level order,
        // but apply a mild skew so small inventory still gets nudged back toward flat.
        if position.abs() <= inventory_deadband {
            let deadband_ratio = (position.abs() / inventory_deadband.max(config.step_size))
                .clamp(0.0, 1.0);
            let deadband_skew = risk.base_order_size * 0.30 * deadband_ratio;
            let (bid_size, ask_size) = if position > 0.0 {
                (
                    (risk.base_order_size - deadband_skew).max(0.0),
                    risk.base_order_size + deadband_skew,
                )
            } else if position < 0.0 {
                (
                    risk.base_order_size + deadband_skew,
                    (risk.base_order_size - deadband_skew).max(0.0),
                )
            } else {
                (risk.base_order_size, risk.base_order_size)
            };
            return apply_risk_limits(
                config,
                risk,
                bid_size,
                ask_size,
                mid,
            );
        }

        // Sigmoid SIZE multiplier using tanh: 1.0 at pos=0, ~3.0 at pos=max_position
        // Formula: 1.0 + tanh(steepness * normalized_pos)
        // tanh(4) ≈ 0.9993, so at pos=max_position, multiplier ≈ 1.0 + 1.0 = 2.0
        // To reach 3.0, we scale: 1.0 + 2.0 * tanh(steepness * normalized_pos)
        let normalized_pos = position / risk.max_position.max(config.step_size);
        let sigmoid_multiplier =
            1.0 + 2.0 * (config.sigmoid_steepness * normalized_pos.abs()).tanh();
        let urgency_threshold = risk.inventory_urgency_threshold.max(config.step_size);
        let skew_progress = ((position.abs() - inventory_deadband)
            / (urgency_threshold - inventory_deadband).max(config.step_size))
            .clamp(0.0, 1.0);

        // Cap inventory_offset to prevent whiplash: flattening order <= cap_mult * base_order_size
        let max_offset = risk.base_order_size * (config.flattening_cap_mult - 1.0).max(0.5);
        let effective_position = (position.abs() - inventory_deadband).max(0.0);
        let raw_inventory_offset = effective_position * sigmoid_multiplier;
        let max_pre_urgency_offset = risk.base_order_size * 0.70 * skew_progress;
        let inventory_offset = if position.abs() < urgency_threshold {
            raw_inventory_offset.min(max_pre_urgency_offset)
        } else {
            raw_inventory_offset.min(max_offset)
        };

        let bid_size = if position < 0.0 {
            // Short position → increase bid size to buy back
            risk.base_order_size + inventory_offset
        } else {
            // Long position → decrease bid size
            (risk.base_order_size - inventory_offset).max(0.0)
        };

        let ask_size = if position > 0.0 {
            // Long position → increase ask size to sell
            risk.base_order_size + inventory_offset
        } else {
            // Short position → decrease ask size
            (risk.base_order_size - inventory_offset).max(0.0)
        };

        apply_risk_limits(config, risk, bid_size, ask_size, mid)
    }

    async fn cancel_all_orders(&mut self) {
        if self.active_orders.is_empty() {
            return;
        }

        let mut batch_actions = Vec::new();
        let mut pending_cancel_ids = Vec::new();
        for order in &self.active_orders {
            if let Some(idx) = order
                .order_index
                .or_else(|| self.order_tracker.get_order_index(order.client_order_id))
            {
                batch_actions.push(BatchAction::Cancel(idx));
                pending_cancel_ids.push(order.client_order_id);
            }
        }

        if !batch_actions.is_empty() {
            for client_order_id in &pending_cancel_ids {
                self.order_tracker.mark_pending_cancel(*client_order_id);
            }
            match self.trading.execute_batch(batch_actions).await {
                Ok(_) => {
                    self.refresh_active_orders_from_tracker();
                    debug!("Submitted cancel requests for all tracked active orders");
                }
                Err(err) => {
                    for client_order_id in pending_cancel_ids {
                        self.order_tracker.revert_pending_cancel(client_order_id);
                    }
                    warn!("Cancel-all fallback batch failed: {}", err);
                    self.refresh_active_orders_from_tracker();
                }
            }
        }
    }

    async fn perform_graceful_shutdown(&mut self) -> anyhow::Result<()> {
        info!("🛑 Graceful shutdown initiated...");
        self.is_running.store(false, Ordering::SeqCst);
        self.clean_exchange_state("shutdown").await?;
        info!("👋 Strategy stopped cleanly.");
        Ok(())
    }

    fn sync_local_after_cancel_all(&mut self) {
        self.order_tracker.cancel_all_active();
        self.active_orders.clear();
    }

    fn sync_tracker_position_to_exchange(&mut self, reason: &str) {
        let delta = self
            .order_tracker
            .force_sync_position(self.account_stats.position);
        if delta.abs() > 1e-8 {
            info!(
                "{}: synced tracker confirmed position to exchange position {:.4} base (delta={:+.4})",
                reason,
                self.account_stats.position,
                delta
            );
        }
    }

    async fn cancel_all_and_sync(&mut self, reason: &str) {
        match self.trading.cancel_all().await {
            Ok(_) => {
                self.sync_local_after_cancel_all();
            }
            Err(err) => {
                warn!(
                    "{}: exchange cancel_all failed, falling back to tracked cancels: {}",
                    reason,
                    err
                );
                self.cancel_all_orders().await;
            }
        }
    }

    async fn clean_exchange_state(&mut self, phase: &str) -> anyhow::Result<()> {
        info!("📤 {} cleanup: canceling all open orders...", phase);
        self.cancel_all_and_sync(&format!("{phase} cleanup")).await;
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Refresh account stats before deciding whether flattening is needed.
        self.account_stats = self.account_stats_reader.read().into();

        for attempt in 1..=2 {
            if self.account_stats.position.abs() < self.config.step_size {
                break;
            }

            if let Some(mid) = self.fetch_cleanup_mid_price().await {
                info!(
                    "📤 {} cleanup: flattening residual position {:.4} base @ ~${:.2} (attempt {}/{})",
                    phase,
                    self.account_stats.position,
                    mid,
                    attempt,
                    2
                );
                if let Err(err) = self.trading.close_all_positions(mid).await {
                    warn!(
                        "{} cleanup: flatten position failed for {:.4} base on attempt {}: {}",
                        phase,
                        self.account_stats.position,
                        attempt,
                        err
                    );
                    break;
                }

                tokio::time::sleep(Duration::from_secs(2)).await;
                self.cancel_all_and_sync(&format!("{phase} post-flatten cleanup")).await;
                tokio::time::sleep(Duration::from_millis(500)).await;
                self.account_stats = self.account_stats_reader.read().into();
            } else {
                warn!(
                    "{} cleanup: unable to obtain a valid Lighter mid price, skipping forced flatten for position {:.4}",
                    phase,
                    self.account_stats.position
                );
                break;
            }
        }

        if self.account_stats.position.abs() >= self.config.step_size {
            warn!(
                "{} cleanup complete with residual position still open: {:.4} base",
                phase,
                self.account_stats.position
            );
        }

        self.account_stats = self.account_stats_reader.read().into();
        self.sync_tracker_position_to_exchange(&format!("{phase} cleanup"));
        self.refresh_active_orders_from_tracker();
        Ok(())
    }

    async fn fetch_cleanup_mid_price(&mut self) -> Option<f64> {
        for _ in 0..20 {
            if let Some(market_state) = self.fetch_market_state().await {
                if let Some(mid) = cleanup_reference_mid(
                    &market_state.bbo,
                    self.config.tick_size,
                    LOCAL_FALLBACK_SPREAD_TICKS,
                ) {
                    return Some(mid);
                }
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }

        None
    }

    fn print_pnl_update(&self) {
        let equity = self.account_stats.portfolio_value;
        let raw_available = self.account_stats.available_balance;
        let margin_usage = self.account_stats.margin_usage;
        let mid = self
            .micro
            .price_samples
            .back()
            .copied()
            .unwrap_or(1.0)
            .max(self.config.tick_size);
        let risk = self.build_risk_snapshot(mid);
        let safe_avail = safe_available_balance(
            raw_available,
            equity,
            margin_usage,
            self.config.max_leverage,
        );
        let tracker_confirmed_position = self.order_tracker.confirmed_position();
        let tracker_pending_exposure = self.order_tracker.net_pending_exposure();
        let tracker_effective_position = self.order_tracker.effective_position();
        let quote_position = risk.position_for_quoting;
        let worst_case_long = self.order_tracker.worst_case_long();
        let worst_case_short = self.order_tracker.worst_case_short();
        let position_ratio = (quote_position
            / scaled_max_position(&self.config, equity, mid))
            .abs();
        let usable_balance = safe_avail * usable_balance_fraction(position_ratio, margin_usage);

        let pnl_baseline = if self.session_start_balance.is_finite()
            && self.session_start_balance > 0.0
            && (equity - self.session_start_balance).abs() <= equity.abs().max(1.0) * 0.5
        {
            self.session_start_balance
        } else {
            equity
        };
        let pnl = equity - pnl_baseline;
        let pnl_pct = if pnl_baseline > 0.0 {
            (pnl / pnl_baseline) * 100.0
        } else {
            0.0
        };

        info!(
            "📊 PnL: ${:.2} ({:+.3}%) | Equity: ${:.2} | Avail (Safe/Raw/Usable): ${:.2}/${:.2}/${:.2} | Margin: {:.1}% | Pos (Exch/Quote/TrkConf/Pend/Eff): {:.4}/{:.4}/{:.4}/{:+.4}/{:.4} base | Worst (L/S): {:.4}/{:.4} | Orders: {} | Fills: {} ({:.1}/min) | Fees: ${:.4}",
            pnl,
            pnl_pct,
            equity,
            safe_avail,
            raw_available,
            usable_balance,
            margin_usage * 100.0,
            self.account_stats.position,
            quote_position,
            tracker_confirmed_position,
            tracker_pending_exposure,
            tracker_effective_position,
            worst_case_long,
            worst_case_short,
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
        let base_interval = if self.account_stats.position.abs() >= self.config.step_size {
            ACTIVE_POSITION_RECONCILE_INTERVAL_SEC
        } else {
            RECONCILE_INTERVAL_SEC
        };
        let interval = reconcile_interval(base_interval, self.reconcile_failures);
        if self.last_balance_check.elapsed() > interval {
            // Refresh account stats from SHM. Do not overwrite with the exchange
            // fallback API here because LighterTrading currently returns placeholders.
            if let Some(stats) = self.account_stats_reader.read_if_updated() {
                self.account_stats = stats.into();
            }

            // Phase 1: Reconcile active orders from OrderTracker with actual open orders
            let stale_count = match self.order_tracker.reconcile_with_exchange(&*self.trading).await {
                Ok(stale_count) => {
                    if self.reconcile_failures > 0 {
                        info!(
                            "Reconcile recovered after {} consecutive failure(s)",
                            self.reconcile_failures
                        );
                    }
                    self.reconcile_failures = 0;
                    stale_count
                }
                Err(err) => {
                    self.reconcile_failures = self.reconcile_failures.saturating_add(1);
                    warn!(
                        "Periodic reconcile failed (attempt={} next_in={}s): {}",
                        self.reconcile_failures,
                        reconcile_interval(RECONCILE_INTERVAL_SEC, self.reconcile_failures)
                            .as_secs(),
                        err
                    );
                    0
                }
            };
            self.refresh_active_orders_from_tracker();
            if stale_count > 0 {
                debug!(
                    "Periodic reconcile: cleared {} stale tracker entries (tracked={} resting={})",
                    stale_count,
                    self.tracked_order_count(),
                    self.resting_order_count()
                );
            }
            self.order_tracker
                .gc_completed_orders(Duration::from_secs(GC_INTERVAL_SEC));
            // Phase 2: Verify atomic exposure matches locked traversal
            self.order_tracker.debug_verify_exposure();
            // Sync fill stats from OrderTracker → Telemetry
            let (fill_count, total_fees) = self.order_tracker.total_fill_stats();
            self.last_reconciled_fill_count = fill_count;
            let risk_mid = self
                .micro
                .price_samples
                .back()
                .copied()
                .filter(|mid| mid.is_finite() && *mid > 0.0)
                .unwrap_or(1.0);
            let risk = self.build_risk_snapshot(risk_mid);
            sync_telemetry_snapshot(
                &mut self.telemetry,
                &self.account_stats,
                &risk,
                fill_count,
                total_fees,
                self.order_tracker.confirmed_position(),
                self.order_tracker.net_pending_exposure(),
                self.order_tracker.effective_position(),
            );
            self.telemetry.export_metrics();
            self.print_pnl_update();
            self.last_balance_check = Instant::now();
        }
    }

    fn refresh_active_orders_from_tracker(&mut self) {
        self.active_orders = self
            .order_tracker
            .active_orders_snapshot()
            .into_iter()
            .filter(|order| !order.lifecycle.is_terminal())
            .map(|order| ActiveOrder {
                client_order_id: order.client_order_id,
                order_index: order.order_index,
                lifecycle: order.lifecycle,
                side: order.side,
                price: order.price,
                size: order.remaining_size(),
                placed_at: order.created_at,
            })
            .collect();
    }

    async fn reconcile_after_new_fill_if_needed(&mut self) {
        let (fill_count, _) = self.order_tracker.total_fill_stats();
        if fill_count <= self.last_reconciled_fill_count {
            return;
        }
        self.last_fill_at = Some(Instant::now());

        match self.order_tracker.reconcile_with_exchange(&*self.trading).await {
            Ok(stale_count) => {
                self.reconcile_failures = 0;
                self.last_reconciled_fill_count = fill_count;
                self.refresh_active_orders_from_tracker();
                if stale_count > 0 {
                    info!(
                        "Post-fill reconcile cleared {} stale tracker entries after fill_count advanced to {}",
                        stale_count,
                        fill_count
                    );
                }
            }
            Err(err) => {
                self.reconcile_failures = self.reconcile_failures.saturating_add(1);
                warn!(
                    "Post-fill reconcile failed (fill_count={} attempt={}): {}",
                    fill_count,
                    self.reconcile_failures,
                    err
                );
            }
        }
    }
    /// Fetch market state from SHM and perform staleness check
    async fn fetch_market_state(&mut self) -> Option<MarketState> {
        let exchanges = self.shm_reader.read_all_exchanges(self.config.symbol_id);
        let market_state = match build_market_state(exchanges, self.config.exchange_id) {
            Some(state) => state,
            None => {
                tokio::time::sleep(Duration::from_millis(10)).await;
                return None;
            }
        };

        // Staleness check
        if market_state.bbo.timestamp_ns > 0 {
            let now_ns = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64;
            let action = classify_stale_bbo(
                &market_state.bbo,
                now_ns,
                self.stale_bbo_since_ns,
                DATA_STALENESS_THRESHOLD_MS,
                STALE_BBO_CANCEL_AFTER_MS,
                STATIC_TWO_SIDED_BOOK_GRACE_MS,
                STATIC_QUOTEABLE_BOOK_GRACE_MS,
            );
            match action {
                StaleBboAction::Fresh => {
                    self.stale_bbo_since_ns = None;
                }
                StaleBboAction::Freeze => {
                    if self.stale_bbo_since_ns.is_none() {
                        self.stale_bbo_since_ns = Some(now_ns);
                        warn!(
                            "Stale BBO: age={}ms (>{}ms), freezing quotes and waiting for recovery",
                            data_age_ms(market_state.bbo.timestamp_ns, now_ns),
                            DATA_STALENESS_THRESHOLD_MS
                        );
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    return None;
                }
                StaleBboAction::Cancel => {
                    warn!(
                        "Stale BBO persisted for >{}ms (current age={}ms), canceling all orders",
                        STALE_BBO_CANCEL_AFTER_MS,
                        data_age_ms(market_state.bbo.timestamp_ns, now_ns),
                    );
                    self.cancel_all_orders().await;
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    return None;
                }
            }
        }

        Some(market_state)
    }

    /// Calculate signal components and pricing inputs from market state
    fn calculate_pricing_inputs(&mut self, exchanges: &[(u8, ShmBboMessage); NUM_EXCHANGES], bbo: &ShmBboMessage) -> PricingInputs {
        // Lighter MM should quote off its own book only.
        let mid = local_reference_mid(bbo, self.config.tick_size, LOCAL_FALLBACK_SPREAD_TICKS);

        // Calculate VWMicro price if depth data available
        let (bid_touch, ask_touch) = fallback_bbo_prices(mid, bbo, self.config.tick_size);
        let mut pricing_mid = if let Some(ref depth_reader) = self.shm_depth_reader {
            if let Some(depth) = depth_reader.read_depth(self.config.symbol_id, self.config.exchange_id) {
                self.calculate_vw_micro_price(&depth, bid_touch, ask_touch)
            } else {
                mid
            }
        } else {
            mid
        };

        let now_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        let external_offset_bps = external_reference_mid(
            exchanges,
            self.config.exchange_id,
            now_ns,
            DATA_STALENESS_THRESHOLD_MS,
        )
        .map(|external_mid| {
            cross_exchange_offset_bps(
                mid,
                external_mid,
                self.config.cross_exchange_as_threshold,
                self.config.cross_exchange_scale,
                EXTERNAL_OVERLAY_MAX_BPS,
                EXTERNAL_OVERLAY_SANITY_BPS,
            )
        })
        .unwrap_or(0.0);
        if external_offset_bps != 0.0 {
            pricing_mid *= 1.0 + external_offset_bps / 10000.0;
        }

        // Update microstructure
        self.micro.update(pricing_mid);
        let vol_bps = self.micro.volatility_bps();
        let as_score = self.micro.adverse_selection_score();

        PricingInputs {
            mid,
            pricing_mid,
            bid_touch,
            ask_touch,
            vol_bps,
            as_score,
            external_offset_bps,
        }
    }

    /// Avellaneda-Stoikov pricing logic
    fn calculate_optimal_quotes(&self, inputs: &PricingInputs, q: f64) -> Option<(f64, f64)> {
        let toxicity_spread_mult = toxicity_spread_multiplier(
            inputs.as_score,
            self.config.adverse_selection_threshold,
        );
        if toxicity_spread_mult > 1.0 {
            debug!(
                "AS soft filter active: score={:.2} spread_mult={:.2}",
                inputs.as_score,
                toxicity_spread_mult
            );
        }

        if inputs.external_offset_bps.abs() >= self.config.cross_exchange_as_threshold {
            debug!(
                "External divergence overlay active: offset_bps={:.2}",
                inputs.external_offset_bps
            );
        }

        let gamma = self.config.as_gamma;
        let time_horizon = self.config.as_time_horizon_sec;
        let sigma = inputs.vol_bps / 10000.0;

        // Reservation price: local microprice shifted by inventory risk plus
        // an explicit urgency skew to bias quotes toward flattening.
        let mut runtime_config = self.config.clone();
        runtime_config.inventory_urgency_threshold = scaled_inventory_urgency_threshold(
            &self.config,
            self.account_stats.portfolio_value,
            inputs.mid,
            scaled_max_position(&self.config, self.account_stats.portfolio_value, inputs.mid),
        );
        let urgency_ratio = inventory_skew_ratio(&runtime_config, q);
        let inventory_skew = self.config.inventory_skew_bps * urgency_ratio / 10000.0;
        let reservation_price =
            inputs.pricing_mid * (1.0 - gamma * sigma * sigma * q * time_horizon - inventory_skew);

        // Spread logic
        let kappa = self.config.as_kappa + self.estimate_fill_rate();
        let gamma_safe = gamma.max(1e-6);
        let optimal_spread = gamma * sigma * sigma * time_horizon + (2.0 / gamma_safe) * (1.0 + gamma_safe / kappa).ln();
        let half_spread_raw = optimal_spread / 2.0 * inputs.pricing_mid;

        // Clamping and floors
        let max_half_spread = inputs.pricing_mid * self.config.max_spread_bps / 10000.0 / 2.0;
        let fee_floor = inputs.pricing_mid * (self.config.maker_fee_bps * 2.0 + self.config.min_profit_bps) / 10000.0 / 2.0;
        let half_spread = (half_spread_raw * toxicity_spread_mult).clamp(fee_floor, max_half_spread);

        let raw_bid =
            ((reservation_price - half_spread) / self.config.tick_size).floor() * self.config.tick_size;
        let raw_ask =
            ((reservation_price + half_spread) / self.config.tick_size).ceil() * self.config.tick_size;

        let join_penny_ticks = effective_penny_ticks(
            self.config.penny_ticks,
            inputs.as_score,
            self.config.adverse_selection_threshold,
            urgency_ratio,
        );
        let (our_bid, our_ask) = anchor_quotes_to_touch(
            raw_bid,
            raw_ask,
            inputs.bid_touch,
            inputs.ask_touch,
            inputs.mid,
            self.config.tick_size,
            join_penny_ticks,
            MAX_TOUCH_OFFSET_BPS,
        );

        match stabilize_crossed_quotes(
            our_bid,
            our_ask,
            inputs.bid_touch,
            inputs.ask_touch,
            self.config.tick_size,
        ) {
            Some((stable_bid, stable_ask)) => Some((stable_bid, stable_ask)),
            None => {
                debug!(
                    "Skipping optimal quotes: crossed quote bid={:.2} ask={:.2} mid={:.2}",
                    our_bid,
                    our_ask,
                    inputs.mid
                );
                None
            }
        }
    }

    /// Execute the quoting cycle: size orders and submit batch
    async fn execute_quoting_cycle(&mut self, our_bid: f64, our_ask: f64, _position: f64, mid: f64) {
        self.reconcile_after_new_fill_if_needed().await;
        self.refresh_active_orders_from_tracker();

        let risk = self.build_risk_snapshot(mid);
        let Some(target) = self.build_quote_target(our_bid, our_ask, mid) else {
            return;
        };
        let safe_available = safe_available_balance(
            self.account_stats.available_balance,
            self.account_stats.portfolio_value,
            self.account_stats.margin_usage,
            self.config.max_leverage,
        );
        let residual_exposure = residual_exposure_abs(
            self.account_stats.position,
            self.order_tracker.effective_position(),
        );
        let mut runtime_config = self.config.clone();
        runtime_config.base_order_size = risk.base_order_size;
        runtime_config.min_available_balance = risk.min_available_balance;
        let decision = decide_quote_cycle(
            &runtime_config,
            target.clone(),
            mid,
            safe_available,
            residual_exposure,
            risk.position_for_quoting,
            risk.base_order_size,
            risk.inventory_urgency_threshold,
        );

        let plan = match decision {
            QuoteCycleDecision::Skip => {
                debug!(
                    "Skipping quote cycle: bid_size={:.4} ask_size={:.4} safe_available={:.2}",
                    target.bid_size,
                    target.ask_size,
                    safe_available
                );
                return;
            }
            QuoteCycleDecision::ClearForLowMargin => {
                warn!(
                    "Low margin (safe=${:.2}, raw=${:.2}), clearing orders",
                    safe_available,
                    self.account_stats.available_balance
                );
                self.cancel_all_and_sync("low-margin clear").await;
                return;
            }
            QuoteCycleDecision::FlattenForLowMargin => {
                warn!(
                    "Low margin (safe=${:.2}, raw=${:.2}) with residual exposure ({:.4} base), switching to flatten-only",
                    safe_available,
                    self.account_stats.available_balance,
                    residual_exposure
                );
                self.handle_low_margin_flatten(mid).await;
                return;
            }
            QuoteCycleDecision::Execute(plan) => plan,
        };

        let max_side_replacements = max_side_requote_replacements_per_cycle(
            &self.config,
            risk.position_for_quoting,
            risk.base_order_size,
            risk.inventory_urgency_threshold,
            mid,
        );
        let size_tolerance_ratio = size_tolerance_ratio_for_requote(
            &self.config,
            risk.position_for_quoting,
            risk.base_order_size,
            risk.inventory_urgency_threshold,
            mid,
        );

        let bid_plan = build_side_execution_plan(
            &runtime_config,
            &self.active_orders,
            self.trading.limit_order_type(),
            Side::Buy,
            plan.target.bid_price,
            plan.target.bid_size,
            plan.requote_threshold,
            size_tolerance_ratio,
            max_side_replacements,
        );
        let ask_plan = build_side_execution_plan(
            &runtime_config,
            &self.active_orders,
            self.trading.limit_order_type(),
            Side::Sell,
            plan.target.ask_price,
            plan.target.ask_size,
            plan.requote_threshold,
            size_tolerance_ratio,
            max_side_replacements,
        );

        if should_defer_one_sided_requote(
            &self.config,
            risk.position_for_quoting,
            risk.base_order_size,
            risk.inventory_urgency_threshold,
            mid,
            &bid_plan,
            &ask_plan,
        ) {
            debug!(
                "Deferring one-sided requote churn in symmetric mode: bid(cancel/place)={}/{} ask(cancel/place)={}/{}",
                bid_plan.to_cancel.len(),
                bid_plan.to_place.len(),
                ask_plan.to_cancel.len(),
                ask_plan.to_place.len(),
            );
            return;
        }

        if should_defer_cancel_only_refresh(
            &self.config,
            risk.position_for_quoting,
            risk.base_order_size,
            risk.inventory_urgency_threshold,
            mid,
            &bid_plan,
            &ask_plan,
        ) {
            debug!(
                "Deferring cancel-only refresh in symmetric mode: bid(cancel/place)={}/{} ask(cancel/place)={}/{}",
                bid_plan.to_cancel.len(),
                bid_plan.to_place.len(),
                ask_plan.to_cancel.len(),
                ask_plan.to_place.len(),
            );
            return;
        }

        if should_defer_post_fill_replenishment(
            &self.config,
            risk.position_for_quoting,
            risk.base_order_size,
            risk.inventory_urgency_threshold,
            mid,
            &bid_plan,
            &ask_plan,
            self.last_fill_at,
            Instant::now(),
        ) {
            debug!(
                "Deferring immediate post-fill replenishment: bid(cancel/place)={}/{} ask(cancel/place)={}/{}",
                bid_plan.to_cancel.len(),
                bid_plan.to_place.len(),
                ask_plan.to_cancel.len(),
                ask_plan.to_place.len(),
            );
            return;
        }

        if should_defer_micro_refresh(
            &self.config,
            risk.position_for_quoting,
            risk.base_order_size,
            risk.inventory_urgency_threshold,
            mid,
            &bid_plan,
            &ask_plan,
            self.last_execution_batch_at,
            Instant::now(),
            self.resting_order_count(),
        ) {
            debug!(
                "Deferring small micro-refresh: bid(cancel/place)={}/{} ask(cancel/place)={}/{}",
                bid_plan.to_cancel.len(),
                bid_plan.to_place.len(),
                ask_plan.to_cancel.len(),
                ask_plan.to_place.len(),
            );
            return;
        }

        let mut actions = Vec::new();
        let mut pending_cancel_ids = Vec::new();

        pending_cancel_ids.extend(self.prepare_side_actions(&bid_plan, &mut actions));
        pending_cancel_ids.extend(self.prepare_side_actions(&ask_plan, &mut actions));

        if !actions.is_empty() {
            match self.trading.execute_batch(actions).await {
                Ok(result) => {
                    self.last_execution_batch_at = Some(Instant::now());
                    apply_batch_success(&mut self.total_orders_placed, &mut self.telemetry, &result);
                    self.refresh_active_orders_from_tracker();
                }
                Err(e) => {
                    for client_order_id in pending_cancel_ids {
                        self.order_tracker.revert_pending_cancel(client_order_id);
                    }
                    warn!("Batch execution failed: {}", e);
                    if classify_batch_failure(
                        &mut self.telemetry,
                        &e,
                        self.config.margin_cooldown_secs,
                    )
                        == BatchFailureAction::EnterMarginCooldown
                    {
                        self.cancel_all_orders().await;
                        self.margin_cooldown_until =
                            Instant::now() + Duration::from_secs(self.config.margin_cooldown_secs);
                    }
                }
            }
        } else {
            debug!(
                "No execution actions produced: bid_size={:.4} ask_size={:.4} tracked_orders={} resting_orders={}",
                plan.target.bid_size,
                plan.target.ask_size,
                self.tracked_order_count(),
                self.resting_order_count()
            );
        }
    }

    async fn handle_low_margin_flatten(&mut self, mid: f64) {
        self.cancel_all_and_sync("low-margin flatten").await;

        if self.account_stats.position.abs() >= self.config.step_size && mid.is_finite() && mid > 0.0
        {
            if let Err(err) = self.trading.close_all_positions(mid).await {
                warn!("Low-margin flatten failed: {}", err);
            } else {
                self.margin_cooldown_until =
                    Instant::now() + Duration::from_secs(self.config.margin_cooldown_secs);
            }
        }
    }

// Removed unused legacy method: cancel_all_orders_legacy

    fn prepare_side_actions(&mut self, plan: &execution::SideExecutionPlan, actions: &mut Vec<BatchAction>) -> Vec<i64> {
        let pending_cancel_ids =
            resolve_cancel_client_order_ids(&self.active_orders, &plan.to_cancel);
        for client_order_id in &pending_cancel_ids {
            self.order_tracker.mark_pending_cancel(*client_order_id);
        }

        for oid in &plan.to_cancel {
            actions.push(BatchAction::Cancel(*oid));
        }

        for desired in plan.to_place.clone() {
            actions.push(BatchAction::Place(desired));
        }

        pending_cancel_ids
    }

    #[cfg(test)]
    fn build_grid_plan(
        config: &InventoryNeutralMMConfig,
        side: Side,
        order_type: crate::exchange::OrderType,
        start_px: f64,
        total_sz: f64,
    ) -> Vec<crate::exchange::OrderParams> {
        components::build_grid_plan(config, side, order_type, start_px, total_sz)
    }

    #[cfg(test)]
    fn reconcile_side_plan(
        existing_orders: &[ActiveOrder],
        desired_quotes: &[crate::exchange::OrderParams],
        threshold: f64,
        step_size: f64,
        size_tolerance_ratio: f64,
        min_lifetime: Duration,
        max_replacements_per_cycle: usize,
    ) -> (Vec<i64>, Vec<crate::exchange::OrderParams>) {
        components::reconcile_side_plan(
            existing_orders,
            desired_quotes,
            threshold,
            step_size,
            size_tolerance_ratio,
            min_lifetime,
            max_replacements_per_cycle,
        )
    }

    #[cfg(test)]
    fn apply_risk_limits(
        config: &InventoryNeutralMMConfig,
        risk: &RiskSnapshot,
        desired_bid_size: f64,
        desired_ask_size: f64,
        mid: f64,
    ) -> (f64, f64) {
        apply_risk_limits(config, risk, desired_bid_size, desired_ask_size, mid)
    }
}


#[cfg(test)]
mod tests;
