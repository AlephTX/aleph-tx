//! Inventory-Neutral Market Maker (Tier-1 HFT Strategy)
//!
//! Based on Citadel/Jump Trading principles:
//! 1. Asymmetric order sizing to maintain near-zero inventory
//! 2. Dynamic spread based on realized volatility + adverse selection
//! 3. Aggressive position flattening when inventory deviates
//! 4. Passive normal quoting with reduce-only emergency flattening
//!
//! Key Innovation: **Inventory-Weighted Order Sizing**
//! - Short 0.1 ETH → Bid 0.13 ETH, Ask 0.03 ETH (net +0.1 if bid fills)
//! - Long 0.1 ETH → Bid 0.03 ETH, Ask 0.13 ETH (net -0.1 if ask fills)
//!
//! Returns depend on execution costs and market conditions; no return target is guaranteed.

use crate::account_stats_reader::{AccountStatsReader, AccountStatsSnapshot};
use crate::config::InventoryNeutralMMConfig;
use crate::exchange::{BatchAction, Exchange, OrderType, Side};
use crate::live_hedge::{LiveInventoryHedge, QuoteAllowance};
use crate::order_tracker::{OrderLifecycle, OrderSide, OrderTracker};
use crate::shm_reader::{NUM_EXCHANGES, ShmBboMessage, ShmReader};
use crate::telemetry::TelemetryCollector;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

mod components;
mod execution;
mod housekeeping;
mod market_state;
mod pricing;
use components::{
    QuoteCycleDecision, QuoteTarget, RiskSnapshot, apply_risk_limits, decide_quote_cycle,
    inventory_deadband_size, inventory_skew_ratio, position_for_quoting, residual_exposure_abs,
    safe_available_balance, scaled_base_order_size, scaled_inventory_urgency_threshold,
    scaled_max_position, scaled_min_available_balance, toxicity_size_scale,
    toxicity_spread_multiplier, usable_balance_fraction, utilization_floor_base_order_size,
};
use execution::{
    BatchFailureAction, InventoryContext, apply_batch_success, build_side_execution_plan,
    classify_batch_failure, max_side_requote_replacements_per_cycle,
    resolve_cancel_client_order_ids, should_defer_cancel_only_refresh, should_defer_micro_refresh,
    should_defer_one_sided_requote, should_defer_post_fill_replenishment,
    size_tolerance_ratio_for_requote,
};
use housekeeping::{TelemetrySync, reconcile_interval, sync_telemetry_snapshot};
use market_state::{
    MarketState, StaleBboAction, build_market_state, classify_stale_bbo, data_age_ms,
    external_fair_value_mid,
};
use pricing::{
    AnchorParams, MakerPriceBounds, anchor_quotes_to_touch, cleanup_reference_mid,
    effective_penny_ticks, fallback_bbo_prices, fee_aware_half_spread, local_reference_mid,
    momentum_half_spreads, stabilize_crossed_quotes,
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
            last_update: Instant::now() - Duration::from_secs(ACCOUNT_STALENESS_SECS + 1),
        }
    }
}

impl AccountStats {
    fn is_usable(&self) -> bool {
        [
            self.available_balance,
            self.portfolio_value,
            self.position,
            self.leverage,
            self.margin_usage,
        ]
        .iter()
        .all(|value| value.is_finite())
            && self.portfolio_value > 0.0
            && self.leverage >= 0.0
            && self.margin_usage >= 0.0
            && self.last_update.elapsed() <= Duration::from_secs(ACCOUNT_STALENESS_SECS)
    }
}

fn wall_time_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}

fn account_source_age(snapshot: &AccountStatsSnapshot, now_ns: u64) -> Duration {
    if snapshot.updated_at == 0
        || snapshot.position_updated_at == 0
        || snapshot.updated_at > now_ns
        || snapshot.position_updated_at > now_ns
    {
        return Duration::from_secs(ACCOUNT_STALENESS_SECS + 1);
    }
    Duration::from_nanos(now_ns - snapshot.updated_at.min(snapshot.position_updated_at))
}

impl From<AccountStatsSnapshot> for AccountStats {
    fn from(snapshot: AccountStatsSnapshot) -> Self {
        let now = Instant::now();
        let age = account_source_age(&snapshot, wall_time_ns());
        Self {
            available_balance: snapshot.available_balance,
            portfolio_value: snapshot.portfolio_value,
            position: snapshot.position,
            leverage: snapshot.leverage,
            margin_usage: snapshot.margin_usage,
            last_update: now
                .checked_sub(age)
                .unwrap_or(now - Duration::from_secs(ACCOUNT_STALENESS_SECS + 1)),
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
    return_mean: f64,
    return_m2: f64,
}

impl MicrostructureTracker {
    fn new(max_samples: usize, ema_fast_period: usize, ema_slow_period: usize) -> Self {
        let max_samples = max_samples.max(2);
        Self {
            price_samples: VecDeque::with_capacity(max_samples),
            max_samples,
            ema_fast: 0.0,
            ema_slow: 0.0,
            ema_fast_alpha: 2.0 / (ema_fast_period as f64 + 1.0),
            ema_slow_alpha: 2.0 / (ema_slow_period as f64 + 1.0),
            ema_initialized: false,
            return_mean: 0.0,
            return_m2: 0.0,
        }
    }

    fn update(&mut self, mid: f64) {
        if !mid.is_finite() || mid <= 0.0 {
            return;
        }
        let new_return = self
            .price_samples
            .back()
            .map(|last| (mid / last - 1.0) * 10000.0);
        if new_return.is_some_and(|value| !value.is_finite()) {
            return;
        }
        // Remove before pushing so a full ring never reallocates. Maintain
        // rolling Welford moments instead of rescanning the window every tick.
        if self.price_samples.len() == self.max_samples {
            let removed = (self.price_samples[1] / self.price_samples[0] - 1.0) * 10000.0;
            let count = self.price_samples.len() - 1;
            if count > 1 {
                let new_mean = self.return_mean + (self.return_mean - removed) / (count - 1) as f64;
                self.return_m2 =
                    (self.return_m2 - (removed - self.return_mean) * (removed - new_mean)).max(0.0);
                self.return_mean = new_mean;
            } else {
                self.return_mean = 0.0;
                self.return_m2 = 0.0;
            }
            self.price_samples.pop_front();
        }
        if let Some(value) = new_return {
            let count = self.price_samples.len();
            let delta = value - self.return_mean;
            self.return_mean += delta / count as f64;
            self.return_m2 += delta * (value - self.return_mean);
        }
        self.price_samples.push_back(mid);

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

        (self.return_m2.max(0.0) / (n - 1) as f64).sqrt().max(1.0)
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

/// Tracks continuous inventory independently of fill and request activity.
#[derive(Default)]
struct InventoryAge {
    opened_at: Option<Instant>,
    direction: f64,
    last_attempt: Option<Instant>,
}

impl InventoryAge {
    fn observe(&mut self, position: f64, deadband: f64, now: Instant) {
        if position.abs() <= deadband {
            *self = Self::default();
        } else if self.opened_at.is_none() || position.signum() != self.direction {
            self.opened_at = Some(now);
            self.direction = position.signum();
            self.last_attempt = None;
        }
    }

    fn is_expired(&self, now: Instant, timeout: Duration) -> bool {
        self.opened_at
            .is_some_and(|opened| now.saturating_duration_since(opened) >= timeout)
    }

    fn can_retry(&self, now: Instant) -> bool {
        self.last_attempt
            .is_none_or(|attempt| now.saturating_duration_since(attempt) >= Duration::from_secs(5))
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

const ACCOUNT_STALENESS_SECS: u64 = 30;
const DATA_STALENESS_THRESHOLD_MS: u64 = 5000;
const STALE_BBO_CANCEL_AFTER_MS: u64 = 15000;
const RECONCILE_INTERVAL_SEC: u64 = 30;
const ACTIVE_POSITION_RECONCILE_INTERVAL_SEC: u64 = 3;
const GC_INTERVAL_SEC: u64 = 300;
const LOCAL_FALLBACK_SPREAD_TICKS: f64 = 2.0;
const POSITION_TIMEOUT_SECS: u64 = 60;
// ─── Inventory-Neutral Market Maker ──────────────────────────────────────────
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct PricingInputs {
    mid: f64,
    pricing_mid: f64,
    bid_touch: f64,
    ask_touch: f64,
    vol_bps: f64,
    as_score: f64,
    using_external_fair_value: bool,
}

pub struct InventoryNeutralMM {
    config: InventoryNeutralMMConfig,

    trading: Arc<dyn Exchange>,
    order_tracker: Arc<OrderTracker>,
    hedge: Option<LiveInventoryHedge>,
    use_external_fair_value: bool,
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
    inventory_age: InventoryAge,
    position_confirmation_after_ns: Option<u64>,

    // Telemetry
    telemetry: TelemetryCollector,
    telemetry_sink: Option<crate::telemetry_sink::TelemetrySink>,

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
        let shm_depth_reader =
            crate::shm_depth_reader::ShmDepthReader::open("/dev/shm/aleph-depth", 2048).ok();
        Self::with_data_sources(
            config,
            trading,
            order_tracker,
            shm_reader,
            account_stats_reader,
            shm_depth_reader,
        )
    }

    pub fn with_data_sources(
        config: InventoryNeutralMMConfig,
        trading: Arc<dyn Exchange>,
        order_tracker: Arc<OrderTracker>,
        shm_reader: ShmReader,
        account_stats_reader: AccountStatsReader,
        shm_depth_reader: Option<crate::shm_depth_reader::ShmDepthReader>,
    ) -> Self {
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
            hedge: None,
            use_external_fair_value: true,
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
            inventory_age: InventoryAge::default(),
            position_confirmation_after_ns: None,
            telemetry: TelemetryCollector::new(),
            telemetry_sink: None,
            is_running: Arc::new(AtomicBool::new(true)),
            shm_reader,
        }
    }

    pub fn with_inventory_hedge(mut self, hedge: LiveInventoryHedge) -> Self {
        self.hedge = Some(hedge);
        self
    }

    /// Export structured snapshots for remote monitoring.
    pub fn with_telemetry_sink(mut self, sink: crate::telemetry_sink::TelemetrySink) -> Self {
        self.telemetry_sink = Some(sink);
        self
    }

    pub fn use_local_fair_value(&mut self) {
        self.use_external_fair_value = false;
    }

    pub async fn run(
        &mut self,
        shutdown: Option<tokio::sync::watch::Receiver<bool>>,
    ) -> anyhow::Result<()> {
        self.config.validate()?;
        info!("🎯 Inventory-Neutral MM started (Institutional Mode)");

        if shutdown_requested(shutdown.as_ref()) {
            return Ok(());
        }
        self.perform_startup_initialization().await?;

        let result = self.quote_until_stopped(shutdown.as_ref()).await;
        let cleanup = self.perform_graceful_shutdown().await;
        match (result, cleanup) {
            (Ok(()), result) | (result, Ok(())) => result,
            (Err(run), Err(stop)) => Err(run.context(format!("Shutdown also failed: {stop:#}"))),
        }
    }

    async fn quote_until_stopped(
        &mut self,
        shutdown: Option<&tokio::sync::watch::Receiver<bool>>,
    ) -> anyhow::Result<()> {
        info!("🚀 Starting main execution loop...");
        while self.is_running.load(Ordering::SeqCst) {
            if shutdown_requested(shutdown) {
                return Ok(());
            }

            self.refresh_active_orders_from_tracker();
            if let Some(stats) = self.account_stats_reader.read_if_updated() {
                self.account_stats = stats.into();
            }

            if self.order_tracker.needs_reconciliation() {
                anyhow::ensure!(
                    self.hedge.is_none(),
                    "Lighter event stream requires reconciliation"
                );
                self.clean_exchange_state("event-stream recovery").await?;
            }
            if !self.account_stats.is_usable() {
                warn!("Account equity/position snapshot unavailable or stale; withdrawing quotes");
                self.cancel_all_and_sync("stale account snapshot").await;
                if let Some(hedge) = &mut self.hedge {
                    hedge.check_blocked(true)?;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
            if !self.prepare_inventory_hedge().await? {
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            }

            // Phase 1: Institutional Housekeeping
            self.perform_periodic_housekeeping().await;

            // Daily PnL circuit breaker (v6.0.2): shutdown if loss exceeds 5% of session start balance
            if self.session_start_balance > 0.0 {
                let daily_pnl_pct = (self.account_stats.portfolio_value
                    - self.session_start_balance)
                    / self.session_start_balance;
                if daily_pnl_pct < -0.05 {
                    tracing::error!(
                        "🚨 Daily loss limit breached: {:.2}% (${:.2} → ${:.2}), shutting down",
                        daily_pnl_pct * 100.0,
                        self.session_start_balance,
                        self.account_stats.portfolio_value
                    );
                    anyhow::bail!("Lighter session loss limit breached");
                }
            }

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

            // Inventory age follows the exposure episode, not the last fill.
            // Adding to a stuck position must never renew its timeout.
            let deadband = inventory_deadband_size(
                &self.config,
                scaled_base_order_size(
                    &self.config,
                    self.account_stats.portfolio_value,
                    inputs.mid,
                )
                .max(self.config.step_size),
                scaled_inventory_urgency_threshold(
                    &self.config,
                    self.account_stats.portfolio_value,
                    inputs.mid,
                    scaled_max_position(
                        &self.config,
                        self.account_stats.portfolio_value,
                        inputs.mid,
                    ),
                ),
                inputs.mid,
            );
            let now = Instant::now();
            self.inventory_age.observe(quoting_position, deadband, now);
            // With a live hedge, Lighter inventory is offset on Hyperliquid;
            // a single-leg taker flatten would pay twice and unbalance the pair.
            if self.hedge.is_none()
                && self
                    .inventory_age
                    .is_expired(now, Duration::from_secs(POSITION_TIMEOUT_SECS))
            {
                if self.inventory_age.can_retry(now) {
                    self.inventory_age.last_attempt = Some(now);
                    self.execute_timeout_flatten(inputs.mid).await;
                }
            } else if let Some((our_bid, our_ask, bounds)) =
                self.calculate_optimal_quotes(&inputs, quoting_position)
            {
                self.execute_quoting_cycle(our_bid, our_ask, bounds, inputs.mid, shutdown)
                    .await;
            } else {
                self.cancel_all_orders().await;
            }

            tokio::time::sleep(Duration::from_millis(self.config.poll_interval_ms)).await;
        }

        Ok(())
    }

    fn quote_allowance(&self, confirmed: f64, short: f64, long: f64) -> Option<QuoteAllowance> {
        let allowance = self.hedge.as_ref()?.allowance()?;
        let tolerance = 0.5e-8; // Half of the order tracker's base-unit precision.
        let (low, high) = allowance.bounds();
        ((allowance.primary - self.account_stats.position).abs() < tolerance
            && (allowance.primary - confirmed).abs() < tolerance
            && short >= low
            && long <= high)
            .then_some(allowance)
    }

    async fn prepare_inventory_hedge(&mut self) -> anyhow::Result<bool> {
        let Some(hedge) = &mut self.hedge else {
            return Ok(true);
        };
        hedge.poll().await?;
        let (confirmed, short, long) = self.order_tracker.exposure_bounds();
        let blocked = self.quote_allowance(confirmed, short, long).is_none();
        if blocked
            && self.cancel_all_and_sync("inventory hedge").await
            && let Some(hedge) = &mut self.hedge
        {
            hedge.rebalance().await?;
        }
        if let Some(hedge) = &mut self.hedge {
            hedge.check_blocked(blocked)?;
        }
        Ok(!blocked)
    }

    /// Startup sequence: cancel orders, sync state, and perform initial initialization
    async fn perform_startup_initialization(&mut self) -> anyhow::Result<()> {
        self.clean_exchange_state("startup").await?;

        self.session_start_balance = self.account_stats.portfolio_value;

        info!(
            "✅ Startup complete: pos={:.4} bal=${:.2}",
            self.account_stats.position, self.account_stats.portfolio_value
        );
        Ok(())
    }

    /// Estimate fill rate κ for A-S model from recent order fills
    /// Higher κ → tighter spread (fills are easy to get)
    /// Lower κ → wider spread (fills are scarce, need more edge)
    /// Returns fills per second (matching T's time unit)
    #[allow(dead_code)]
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
        let (tracker_confirmed, tracker_short, tracker_long) = self.order_tracker.exposure_bounds();
        let mut base_order_size = components::round_down_to_step(
            scaled_base_order_size(&self.config, self.account_stats.portfolio_value, mid)
                .max(self.config.step_size),
            self.config.step_size,
        )
        .max(self.config.step_size);
        // An untradeably small cap means no opening orders; never enlarge a
        // configured notional limit merely to make one lot tradable.
        let max_position =
            scaled_max_position(&self.config, self.account_stats.portfolio_value, mid).max(0.0);
        base_order_size = base_order_size.min(max_position);
        let inventory_urgency_threshold = scaled_inventory_urgency_threshold(
            &self.config,
            self.account_stats.portfolio_value,
            mid,
            max_position,
        );
        let min_available_balance =
            scaled_min_available_balance(&self.config, self.account_stats.portfolio_value);

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
        let mut usable_balance = available_balance
            * usable_balance_fraction(position_ratio, self.account_stats.margin_usage);
        let margin_per_eth = mid / self.config.max_leverage;
        let r = self.config.grid_size_decay;
        let n = self.config.grid_levels.max(1) as i32;
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
        base_order_size = base_order_size.min(max_position);
        usable_balance = available_balance
            * usable_balance_fraction(position_ratio, self.account_stats.margin_usage);

        RiskSnapshot {
            raw_available_balance,
            position_for_quoting,
            // A lagging account snapshot must not erase a more recent fill.
            // Reserve exposure against both observed positions until reconciled.
            worst_case_long: tracker_long + (exchange_position - tracker_confirmed).max(0.0),
            worst_case_short: tracker_short + (exchange_position - tracker_confirmed).min(0.0),
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
        risk: &RiskSnapshot,
    ) -> Option<QuoteTarget> {
        let spread_bps = ((our_ask - our_bid) / mid) * 10000.0;
        self.telemetry.update_spread_size(spread_bps);
        self.telemetry
            .update_adverse_selection(self.micro.adverse_selection_score());

        let min_spread_bps = self.minimum_spread_bps();
        if spread_bps < min_spread_bps {
            debug!(
                "Skipping quote target: spread_bps={:.2} below min_spread_bps={:.2}",
                spread_bps, min_spread_bps
            );
            return None;
        }

        let (bid_size, ask_size) = self.calculate_asymmetric_sizes(risk, mid);
        let toxicity_scale = toxicity_size_scale(
            self.micro.adverse_selection_score(),
            self.config.adverse_selection_threshold,
        );
        let bid_size =
            components::round_down_to_step(bid_size * toxicity_scale, self.config.step_size);
        let ask_size =
            components::round_down_to_step(ask_size * toxicity_scale, self.config.step_size);

        Some(QuoteTarget {
            bid_price: our_bid,
            ask_price: our_ask,
            bid_size,
            ask_size,
        })
    }

    fn minimum_spread_bps(&self) -> f64 {
        self.config.maker_fee_bps * 2.0
            + self.config.min_profit_bps
            + self
                .hedge
                .as_ref()
                .map_or(0.0, LiveInventoryHedge::round_trip_cost_bps)
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
                return apply_risk_limits(
                    config,
                    risk,
                    0.0,
                    min_size.max(risk.base_order_size),
                    mid,
                );
            } else {
                // Short at limit: only allow buys
                return apply_risk_limits(
                    config,
                    risk,
                    min_size.max(risk.base_order_size),
                    0.0,
                    mid,
                );
            }
        }

        // Small residual inventory should not collapse the book into one-sided churn.
        // Keep the quote stack two-sided inside a deadband roughly equal to one top-level order,
        // but apply a mild skew so small inventory still gets nudged back toward flat.
        if position.abs() <= inventory_deadband {
            let deadband_ratio =
                (position.abs() / inventory_deadband.max(config.step_size)).clamp(0.0, 1.0);
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
            return apply_risk_limits(config, risk, bid_size, ask_size, mid);
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
        if let Err(error) = self.clean_exchange_state("shutdown").await {
            warn!(%error, "Normal shutdown failed; checking independent exchange state");
            self.trading
                .recover_flat_without_feed()
                .await
                .map_err(|recovery| {
                    recovery.context(format!("Normal shutdown failed: {error:#}"))
                })?;
        }
        if let Some(hedge) = &mut self.hedge {
            hedge.finish().await?;
        }
        if let Some(sink) = &self.telemetry_sink {
            sink.stopped();
        }
        info!("👋 Strategy stopped cleanly.");
        Ok(())
    }

    /// Returns whether REST confirms an empty book. This alone never releases
    /// reserved exposure: cleanup also needs an independent fresh position.
    async fn cancel_all_and_sync(&mut self, reason: &str) -> bool {
        let request_started = wall_time_ns();
        match self.trading.cancel_all().await {
            Ok(count) => {
                if count > 0 || !self.order_tracker.active_orders_snapshot().is_empty() {
                    self.position_confirmation_after_ns
                        .get_or_insert(request_started);
                }
            }
            Err(err) => {
                warn!("{}: exchange cancel_all failed: {}", reason, err);
                self.cancel_all_orders().await;
                return false;
            }
        }
        self.refresh_active_orders_from_tracker();
        // An unbound submission can still be in flight and is not proved absent
        // by a single open-orders snapshot.
        if self
            .active_orders
            .iter()
            .any(|order| order.order_index.is_none())
        {
            return false;
        }
        match self.trading.get_active_orders().await {
            Ok(orders) => orders.is_empty(),
            Err(err) => {
                warn!("{}: cancellation confirmation failed: {}", reason, err);
                false
            }
        }
    }

    async fn read_confirmed_account_stats(&mut self) -> anyhow::Result<()> {
        for _ in 0..20 {
            let snapshot = self.account_stats_reader.read();
            let stats = AccountStats::from(snapshot);
            let after_actions = self
                .position_confirmation_after_ns
                .is_none_or(|required| snapshot.position_updated_at >= required);
            if stats.is_usable() && after_actions {
                self.account_stats = stats;
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        anyhow::bail!(
            "Cleanup cannot confirm a fresh equity and position snapshot after pending actions"
        )
    }

    async fn clean_exchange_state(&mut self, phase: &str) -> anyhow::Result<()> {
        let reconciliation_epoch = self.order_tracker.reconciliation_epoch();
        for attempt in 0..3 {
            let canceled = self.cancel_all_and_sync(phase).await;
            if !canceled {
                if attempt == 2 {
                    anyhow::bail!(
                        "{phase}: cancellation remains unconfirmed; reserved exposure retained"
                    );
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            }
            self.read_confirmed_account_stats().await?;
            if self.account_stats.position == 0.0 {
                // Only this independently confirmed empty-book + position barrier
                // may release reservations after an event gap or uncertain cancel.
                anyhow::ensure!(
                    self.order_tracker
                        .confirm_cleanup(reconciliation_epoch, self.account_stats.position),
                    "{phase}: event stream changed during cleanup; exposure retained and trading halted"
                );
                self.active_orders.clear();
                self.position_confirmation_after_ns = None;
                return Ok(());
            }
            anyhow::ensure!(
                attempt < 2,
                "{phase}: residual inventory remains after cleanup attempts"
            );
            let mid = self.fetch_cleanup_mid_price().await.ok_or_else(|| {
                anyhow::anyhow!("{phase}: no fresh price for residual-position cleanup")
            })?;
            self.position_confirmation_after_ns = Some(wall_time_ns());
            self.trading.close_all_positions(mid).await?;
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        anyhow::bail!("{phase}: residual inventory remains; refusing to report a clean state")
    }

    async fn fetch_cleanup_mid_price(&mut self) -> Option<f64> {
        for _ in 0..20 {
            if let Some(market_state) = self.fetch_market_state().await
                && let Some(mid) = cleanup_reference_mid(
                    &market_state.bbo,
                    self.config.tick_size,
                    LOCAL_FALLBACK_SPREAD_TICKS,
                )
            {
                return Some(mid);
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }

        None
    }

    fn export_snapshot(&self, mid: f64, quote_position: f64) {
        let Some(sink) = &self.telemetry_sink else {
            return;
        };
        let (fill_count, total_fees) = self.order_tracker.total_fill_stats();
        sink.snapshot(crate::telemetry_sink::StrategySnapshot {
            equity: self.account_stats.portfolio_value,
            session_start_equity: self.session_start_balance,
            available_balance: self.account_stats.available_balance,
            margin_usage: self.account_stats.margin_usage,
            exchange_position: self.account_stats.position,
            quote_position,
            confirmed_position: self.order_tracker.confirmed_position(),
            pending_exposure: self.order_tracker.net_pending_exposure(),
            worst_case_long: self.order_tracker.worst_case_long(),
            worst_case_short: self.order_tracker.worst_case_short(),
            mid,
            spread_bps: self.telemetry.spread_size_bps,
            adverse_selection: self.telemetry.adverse_selection_score,
            trade_cash_flow: self.order_tracker.trade_cash_flow(),
            total_fees,
            fill_count,
            orders_placed: self.total_orders_placed,
            orders_rejected: self.telemetry.orders_rejected,
            active_orders: self.order_tracker.active_order_count() as u32,
        });
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
        let position_ratio =
            (quote_position / scaled_max_position(&self.config, equity, mid)).abs();
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
            .filter(|l| l.price.is_finite() && l.size.is_finite() && l.price > 0.0 && l.size > 0.0)
            .map(|l| l.price * l.size)
            .sum();

        // Calculate total notional value on ask side (L1-L5)
        let ask_notional: f64 = depth
            .asks
            .iter()
            .take(5)
            .filter(|l| l.price.is_finite() && l.size.is_finite() && l.price > 0.0 && l.size > 0.0)
            .map(|l| l.price * l.size)
            .sum();

        // Avoid division by zero
        if !(bid_notional + ask_notional).is_finite() || bid_notional + ask_notional < 0.001 {
            return (bid_l1 + ask_l1) / 2.0; // Fallback to simple mid
        }

        // VWMicro formula: weight L1 prices by opposite side notional
        let bid_weight = bid_notional / (bid_notional + ask_notional);
        bid_weight * ask_l1 + (1.0 - bid_weight) * bid_l1
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
            let stale_count = match self
                .order_tracker
                .reconcile_with_exchange(&*self.trading)
                .await
            {
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
                &TelemetrySync {
                    fill_count,
                    total_fees,
                    confirmed_position: self.order_tracker.confirmed_position(),
                    pending_exposure: self.order_tracker.net_pending_exposure(),
                    effective_position: self.order_tracker.effective_position(),
                },
            );
            self.telemetry.export_metrics();
            self.print_pnl_update();
            self.export_snapshot(risk_mid, risk.position_for_quoting);
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

        match self
            .order_tracker
            .reconcile_with_exchange(&*self.trading)
            .await
        {
            Ok(stale_count) => {
                self.reconcile_failures = 0;
                self.last_reconciled_fill_count = fill_count;
                self.refresh_active_orders_from_tracker();
                if stale_count > 0 {
                    info!(
                        "Post-fill reconcile cleared {} stale tracker entries after fill_count advanced to {}",
                        stale_count, fill_count
                    );
                }
            }
            Err(err) => {
                self.reconcile_failures = self.reconcile_failures.saturating_add(1);
                warn!(
                    "Post-fill reconcile failed (fill_count={} attempt={}): {}",
                    fill_count, self.reconcile_failures, err
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
                // Missing or corrupt data must not leave live quotes unattended.
                self.cancel_all_orders().await;
                return None;
            }
        };

        // Staleness check
        {
            let now_ns = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64;
            let action = classify_stale_bbo(
                &market_state.bbo,
                now_ns,
                DATA_STALENESS_THRESHOLD_MS,
                STALE_BBO_CANCEL_AFTER_MS,
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
    ///
    /// New flow (v6.0.0): External exchanges (Binance, Hyperliquid) are the PRIMARY
    /// fair value anchor. Local Lighter BBO is only used for touch positioning.
    fn calculate_pricing_inputs(
        &mut self,
        exchanges: &[(u8, ShmBboMessage); NUM_EXCHANGES],
        bbo: &ShmBboMessage,
    ) -> PricingInputs {
        // Local mid — always needed for touch positioning
        let local_mid =
            local_reference_mid(bbo, self.config.tick_size, LOCAL_FALLBACK_SPREAD_TICKS);
        let (bid_touch, ask_touch) = fallback_bbo_prices(local_mid, bbo, self.config.tick_size);

        // External fair value: primary pricing anchor
        let now_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        let external_fair = if self.use_external_fair_value {
            external_fair_value_mid(
                exchanges,
                self.config.exchange_id,
                now_ns,
                self.config.external_staleness_ms,
            )
        } else {
            None
        };

        let (pricing_mid, using_external, mid) = if let Some(fair_mid) = external_fair {
            // External exchanges drive fair value; local mid is only for touch
            (fair_mid, true, local_mid)
        } else {
            // Fallback: use local VWMicro or simple mid when no external data
            let vw_mid = if self.config.use_depth_pricing {
                if let Some(depth) = self
                    .shm_depth_reader
                    .as_ref()
                    .and_then(|reader| {
                        reader.read_depth(self.config.symbol_id, self.config.exchange_id)
                    })
                    .filter(|depth| {
                        !market_state::is_stale_bbo(
                            depth.timestamp_ns,
                            now_ns,
                            DATA_STALENESS_THRESHOLD_MS,
                        )
                    })
                {
                    self.calculate_vw_micro_price(&depth, bid_touch, ask_touch)
                } else {
                    local_mid
                }
            } else {
                local_mid
            };
            (vw_mid, false, local_mid)
        };

        // Update microstructure tracker with pricing_mid (now driven by external exchanges)
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
            using_external_fair_value: using_external,
        }
    }

    /// Avellaneda-Stoikov pricing logic (v6.0.0)
    ///
    /// Uses external fair value as pricing_mid. Kappa is used directly from config
    /// (no fill-rate scaling). Spread cap is dynamic vol-based instead of hard clamp.
    /// Momentum-aware asymmetric spread replaces old inventory_adjusted_half_spreads.
    fn calculate_optimal_quotes(
        &self,
        inputs: &PricingInputs,
        q: f64,
    ) -> Option<(f64, f64, MakerPriceBounds)> {
        let (sell_proceeds, buy_cost) = if let Some(hedge) = &self.hedge {
            let allowance = hedge.allowance()?;
            (
                inputs.pricing_mid.min(allowance.sell_proceeds),
                inputs.pricing_mid.max(allowance.buy_cost),
            )
        } else {
            (inputs.pricing_mid, inputs.pricing_mid)
        };
        let bounds = MakerPriceBounds::new(
            sell_proceeds,
            buy_cost,
            self.config.maker_fee_bps,
            self.config.min_profit_bps,
            self.config.tick_size,
        )?;
        let toxicity_spread_mult =
            toxicity_spread_multiplier(inputs.as_score, self.config.adverse_selection_threshold);
        if toxicity_spread_mult > 1.0 {
            debug!(
                "AS soft filter active: score={:.2} spread_mult={:.2}",
                inputs.as_score, toxicity_spread_mult
            );
        }

        let gamma = self.config.as_gamma;
        let time_horizon = self.config.as_time_horizon_sec;
        let sigma = inputs.vol_bps / 10000.0;

        // Reservation price: external fair value shifted by inventory risk plus
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

        // Spread logic: use config kappa directly (no fill-rate scaling)
        let kappa = self.config.as_kappa;
        let gamma_safe = gamma.max(1e-6);
        let optimal_spread = gamma * sigma * sigma * time_horizon
            + (2.0 / gamma_safe) * (gamma_safe / kappa).ln_1p();
        let half_spread_raw = optimal_spread / 2.0 * inputs.pricing_mid;

        let fee_floor_bps = self.minimum_spread_bps();
        let vol_cap_bps = (inputs.vol_bps * 4.0)
            .clamp(8.0, 40.0)
            .max(fee_floor_bps)
            .min(self.config.max_spread_bps);
        let half_spread = fee_aware_half_spread(
            half_spread_raw * toxicity_spread_mult,
            inputs.pricing_mid,
            fee_floor_bps,
            vol_cap_bps,
        )?;
        let (bid_half_spread, ask_half_spread) = momentum_half_spreads(
            half_spread,
            self.micro.momentum_bps(),
            self.config.momentum_skew_scale,
        );

        let raw_bid = ((reservation_price - bid_half_spread) / self.config.tick_size).floor()
            * self.config.tick_size;
        let raw_ask = ((reservation_price + ask_half_spread) / self.config.tick_size).ceil()
            * self.config.tick_size;

        let join_penny_ticks = effective_penny_ticks(
            self.config.penny_ticks,
            inputs.as_score,
            self.config.adverse_selection_threshold,
            urgency_ratio,
        );
        let (our_bid, our_ask) = anchor_quotes_to_touch(&AnchorParams {
            raw_bid,
            raw_ask,
            bid_touch: inputs.bid_touch,
            ask_touch: inputs.ask_touch,
            tick_size: self.config.tick_size,
            penny_ticks: join_penny_ticks,
            inventory_urgency_ratio: urgency_ratio,
        });

        match stabilize_crossed_quotes(
            our_bid,
            our_ask,
            inputs.bid_touch,
            inputs.ask_touch,
            self.config.tick_size,
        ) {
            Some((stable_bid, stable_ask)) => {
                let bid = stable_bid.min(bounds.max_bid);
                let ask = stable_ask.max(bounds.min_ask);
                Some((bid, ask, bounds))
            }
            None => {
                debug!(
                    "Skipping optimal quotes: crossed quote bid={:.2} ask={:.2} mid={:.2}",
                    our_bid, our_ask, inputs.mid
                );
                None
            }
        }
    }

    /// Execute the quoting cycle: size orders and submit batch
    async fn execute_quoting_cycle(
        &mut self,
        our_bid: f64,
        our_ask: f64,
        bounds: MakerPriceBounds,
        mid: f64,
        shutdown: Option<&tokio::sync::watch::Receiver<bool>>,
    ) {
        self.reconcile_after_new_fill_if_needed().await;
        if shutdown_requested(shutdown) {
            return;
        }
        self.refresh_active_orders_from_tracker();
        if self
            .active_orders
            .iter()
            .any(|order| !bounds.allows(order.side, order.price))
        {
            // A requote tolerance must never retain a price below the
            // current edge floor, even during the minimum order lifetime.
            self.cancel_all_and_sync("maker edge below floor").await;
            return;
        }

        let risk = self.build_risk_snapshot(mid);
        let Some(target) = self.build_quote_target(our_bid, our_ask, mid, &risk) else {
            self.cancel_all_orders().await;
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
        let inv_ctx = InventoryContext {
            config: &runtime_config,
            position_for_quoting: risk.position_for_quoting,
            base_order_size: risk.base_order_size,
            inventory_urgency_threshold: risk.inventory_urgency_threshold,
            mid,
        };
        let decision =
            decide_quote_cycle(&inv_ctx, target.clone(), safe_available, residual_exposure);

        let plan = match decision {
            QuoteCycleDecision::ClearForRisk => {
                self.cancel_all_and_sync("zero risk budget").await;
                return;
            }
            QuoteCycleDecision::ClearForLowMargin => {
                warn!(
                    "Low margin (safe=${:.2}, raw=${:.2}), clearing orders",
                    safe_available, self.account_stats.available_balance
                );
                self.cancel_all_and_sync("low-margin clear").await;
                return;
            }
            QuoteCycleDecision::FlattenForLowMargin => {
                warn!(
                    "Low margin (safe=${:.2}, raw=${:.2}) with residual exposure ({:.4} base), switching to flatten-only",
                    safe_available, self.account_stats.available_balance, residual_exposure
                );
                self.handle_low_margin_flatten(mid).await;
                return;
            }
            QuoteCycleDecision::Execute(plan) => plan,
        };

        let max_side_replacements = max_side_requote_replacements_per_cycle(&inv_ctx);
        let size_tolerance_ratio = size_tolerance_ratio_for_requote(&inv_ctx);

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

        let risk_reducing = plan.target.bid_size
            < components::min_quotable_size(&runtime_config, our_bid)
            || plan.target.ask_size < components::min_quotable_size(&runtime_config, our_ask);
        if !risk_reducing && should_defer_one_sided_requote(&inv_ctx, &bid_plan, &ask_plan) {
            debug!(
                "Deferring one-sided requote churn in symmetric mode: bid(cancel/place)={}/{} ask(cancel/place)={}/{}",
                bid_plan.to_cancel.len(),
                bid_plan.to_place.len(),
                ask_plan.to_cancel.len(),
                ask_plan.to_place.len(),
            );
            return;
        }

        if !risk_reducing && should_defer_cancel_only_refresh(&inv_ctx, &bid_plan, &ask_plan) {
            debug!(
                "Deferring cancel-only refresh in symmetric mode: bid(cancel/place)={}/{} ask(cancel/place)={}/{}",
                bid_plan.to_cancel.len(),
                bid_plan.to_place.len(),
                ask_plan.to_cancel.len(),
                ask_plan.to_place.len(),
            );
            return;
        }

        if !risk_reducing
            && should_defer_post_fill_replenishment(
                &inv_ctx,
                &bid_plan,
                &ask_plan,
                self.last_fill_at,
                Instant::now(),
            )
        {
            debug!(
                "Deferring immediate post-fill replenishment: bid(cancel/place)={}/{} ask(cancel/place)={}/{}",
                bid_plan.to_cancel.len(),
                bid_plan.to_place.len(),
                ask_plan.to_cancel.len(),
                ask_plan.to_place.len(),
            );
            return;
        }

        if !risk_reducing
            && should_defer_micro_refresh(
                &inv_ctx,
                &bid_plan,
                &ask_plan,
                self.last_execution_batch_at,
                Instant::now(),
                self.resting_order_count(),
            )
        {
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
        let (confirmed, short, long) = self.order_tracker.exposure_bounds();
        let allowance = self.quote_allowance(confirmed, short, long);
        if self.hedge.is_some() && allowance.is_none() {
            self.cancel_all_and_sync("expired hedge allowance").await;
            return;
        }

        pending_cancel_ids.extend(self.prepare_side_actions(&bid_plan, &mut actions));
        pending_cancel_ids.extend(self.prepare_side_actions(&ask_plan, &mut actions));
        if let Some(allowance) = allowance {
            allowance.cap_batch(&mut actions, short, long, self.config.step_size, |price| {
                components::min_quotable_size(&self.config, price)
            });
        }

        if !actions.is_empty() {
            match self.trading.execute_batch(actions).await {
                Ok(result) => {
                    self.last_execution_batch_at = Some(Instant::now());
                    apply_batch_success(
                        &mut self.total_orders_placed,
                        &mut self.telemetry,
                        &result,
                    );
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
                    ) == BatchFailureAction::EnterMarginCooldown
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
        if !self.cancel_all_and_sync("low-margin flatten").await {
            return;
        }

        if self.account_stats.position.abs() >= self.config.step_size
            && mid.is_finite()
            && mid > 0.0
        {
            if let Err(err) = self.trading.close_all_positions(mid).await {
                warn!("Low-margin flatten failed: {}", err);
            } else {
                self.margin_cooldown_until =
                    Instant::now() + Duration::from_secs(self.config.margin_cooldown_secs);
            }
        }
    }

    /// Submit a reduce-only IOC for expired inventory and wait for confirmation.
    async fn execute_timeout_flatten(&mut self, mid: f64) {
        if !self.cancel_all_and_sync("position-timeout flatten").await {
            return;
        }

        if let Some(stats) = self.account_stats_reader.read_if_updated() {
            self.account_stats = stats.into();
        }
        let position = position_for_quoting(
            &self.config,
            self.account_stats.position,
            self.order_tracker.confirmed_position(),
        );
        if position.abs() < self.config.step_size || !mid.is_finite() || mid <= 0.0 {
            return;
        }

        // Bound the execution price to 20 bps from the current local mid.
        let slippage = mid * 0.002; // 20 bps
        let (side, price) = if position > 0.0 {
            // Long: sell aggressively below mid
            (
                crate::exchange::Side::Sell,
                ((mid - slippage) / self.config.tick_size).ceil() * self.config.tick_size,
            )
        } else {
            // Short: buy aggressively above mid
            (
                crate::exchange::Side::Buy,
                ((mid + slippage) / self.config.tick_size).floor() * self.config.tick_size,
            )
        };

        let size = components::round_down_to_step(position.abs(), self.config.step_size);
        if size < self.config.step_size {
            return;
        }

        let action = BatchAction::Place(crate::exchange::OrderParams {
            side,
            size,
            price,
            order_type: OrderType::Ioc,
            reduce_only: true,
        });

        match self.trading.execute_batch(vec![action]).await {
            Ok(result) => {
                info!(
                    "⏰ Timeout flatten submitted: {} {:.4} @ {:.2} (IOC), tx={}",
                    side,
                    size,
                    price,
                    result.tx_hashes.first().map(|s| s.as_str()).unwrap_or("?")
                );
            }
            Err(err) => {
                warn!("⏰ Timeout flatten failed: {}", err);
            }
        }

        // Keep inventory age unchanged: submission is not a confirmed fill.
    }

    // Removed unused legacy method: cancel_all_orders_legacy

    fn prepare_side_actions(
        &mut self,
        plan: &execution::SideExecutionPlan,
        actions: &mut Vec<BatchAction>,
    ) -> Vec<i64> {
        let pending_cancel_ids =
            resolve_cancel_client_order_ids(&self.active_orders, &plan.to_cancel);
        for client_order_id in &pending_cancel_ids {
            self.order_tracker.mark_pending_cancel(*client_order_id);
        }

        for oid in &plan.to_cancel {
            actions.push(BatchAction::Cancel(*oid));
        }

        for desired in &plan.to_place {
            actions.push(BatchAction::Place(desired.clone()));
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

fn shutdown_requested(shutdown: Option<&tokio::sync::watch::Receiver<bool>>) -> bool {
    shutdown.is_some_and(|rx| *rx.borrow())
}

#[cfg(test)]
mod tests;
