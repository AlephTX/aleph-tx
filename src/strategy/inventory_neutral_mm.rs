//! Inventory-aware market making with external fair value, protected
//! post-only quotes and bounded inventory recovery.

use crate::account_stats_reader::{AccountStatsReader, AccountStatsSnapshot};
use crate::config::InventoryNeutralMMConfig;
use crate::exchange::{
    AccountMarginHealth, BatchAction, Exchange, MarginCheckInputs, MarginRegime, OrderType, Side,
};
use crate::live_hedge::{LiveInventoryHedge, QuoteAllowance};
use crate::order_tracker::{OrderLifecycle, OrderSide, OrderTracker};
use crate::shm_reader::{NUM_EXCHANGES, ShmBboMessage, ShmReader};
use crate::telemetry::TelemetryCollector;
use anyhow::Context;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::time::Instant;
use tracing::{debug, info, warn};

mod components;
mod defense;
mod execution;
mod housekeeping;
mod market_state;
mod markout;
mod pricing;
#[cfg(any(test, feature = "research-replay"))]
mod replay;
mod volatility_regime;
use components::CancelReason;
use components::{
    EntryRatchetPolicy, QuoteCycleDecision, QuoteTarget, RiskSnapshot, apply_risk_limits,
    decide_quote_cycle, inventory_deadband_size, inventory_skew_ratio, position_for_quoting,
    residual_exposure_abs, safe_available_balance, scaled_base_order_size,
    scaled_inventory_urgency_threshold, scaled_max_position, scaled_min_available_balance,
    toxicity_size_scale, toxicity_spread_multiplier, usable_balance_fraction,
    utilization_floor_base_order_size,
};
use execution::{
    BatchFailureAction, InventoryContext, SideExecutionPlan,
    URGENT_SIDE_REQUOTE_REPLACEMENTS_PER_CYCLE, apply_batch_success,
    build_side_execution_plan_full, cancels_are_strictly_passive, classify_batch_failure,
    effective_resting_orders_for_micro_refresh, max_side_requote_replacements_per_cycle,
    resolve_cancel_client_order_ids, should_defer_cancel_only_refresh, should_defer_micro_refresh,
    should_defer_one_sided_requote, should_defer_post_fill_replenishment,
    size_tolerance_ratio_for_requote,
};
use housekeeping::{TelemetrySync, reconcile_interval, sync_telemetry_snapshot};
use market_state::{
    BasisTracker, CrossMarketPressure, MarketState, OrderFlow, StaleBboAction, build_market_state,
    classify_stale_bbo, data_age_ms, select_exchange_bbo,
};
use pricing::{
    AnchorParams, MakerPriceBounds, anchor_quotes_to_touch, cleanup_reference_mid,
    effective_penny_ticks, fallback_bbo_prices, fee_aware_half_spread, local_reference_mid,
    momentum_half_spreads, stabilize_crossed_quotes,
};
#[cfg(feature = "research-replay")]
pub use replay::recorded::run_recorded_maker;

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

/// Wall-clock nanoseconds, the time domain of SHM source timestamps.
///
/// Monotonic time in this strategy and `OrderTracker` is `tokio::time::Instant`,
/// which is the system monotonic clock in production. Under a paused Tokio
/// clock in tests, wall time is derived from that same monotonic clock via a
/// fixed anchor so advancing time moves both domains together; production
/// always reads the system wall clock.
#[cfg(not(test))]
fn wall_time_ns() -> u64 {
    #[cfg(feature = "research-replay")]
    if let Some(time) = replay::recorded::wall_time_ns() {
        return time;
    }
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}

#[cfg(test)]
fn wall_time_ns() -> u64 {
    #[cfg(feature = "research-replay")]
    if let Some(time) = replay::recorded::wall_time_ns() {
        return time;
    }
    static ANCHOR: std::sync::OnceLock<(std::time::Instant, u64)> = std::sync::OnceLock::new();
    let (anchor, anchor_ns) = *ANCHOR.get_or_init(|| {
        let wall = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        (std::time::Instant::now(), wall)
    });
    let now = Instant::now().into_std();
    match now.checked_duration_since(anchor) {
        Some(ahead) => anchor_ns + ahead.as_nanos() as u64,
        None => anchor_ns - anchor.duration_since(now).as_nanos() as u64,
    }
}

fn strategy_utc_day() -> i32 {
    #[cfg(feature = "research-replay")]
    if let Some(day) = replay::recorded::utc_day() {
        return day;
    }
    crate::daily_loss::utc_day_now()
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

/// Volatility in fractional units per sqrt(second), for Avellaneda-Stoikov.
///
/// The main loop samples at irregular intervals (100 ms to 1 s sleeps), so the
/// per-sample standard deviation is not in the same time unit as the A-S
/// horizon `as_time_horizon_sec`. This estimator normalises each log return by
/// its real elapsed time and keeps a time-decayed EWMA of `r^2 / dt`.
struct TimeScaledVolatility {
    last: Option<(f64, Instant)>,
    variance_per_sec: f64,
    weight: f64,
}

impl TimeScaledVolatility {
    /// EWMA time constant; ~1 minute of history dominates the estimate.
    const TAU_SECS: f64 = 60.0;
    /// Returns over shorter intervals are merged into the next sample.
    const MIN_DT_SECS: f64 = 0.01;
    /// Gaps longer than this (stalls, reconnects) restart the baseline.
    const MAX_DT_SECS: f64 = 10.0;
    /// Minimum accumulated weight before the estimate is trusted.
    const WARM_WEIGHT: f64 = 0.05;

    fn new() -> Self {
        Self {
            last: None,
            variance_per_sec: 0.0,
            weight: 0.0,
        }
    }

    #[cfg(test)]
    fn update(&mut self, mid: f64, now: Instant) {
        self.update_with_gap_reset(mid, now, None);
    }

    fn update_with_gap_reset(&mut self, mid: f64, now: Instant, max_gap_reset: Option<Duration>) {
        if !mid.is_finite() || mid <= 0.0 {
            return;
        }
        let Some((last_mid, last_at)) = self.last else {
            self.last = Some((mid, now));
            return;
        };
        let elapsed = now.saturating_duration_since(last_at);
        if max_gap_reset.is_some_and(|limit| elapsed >= limit) {
            // Reconnect / long outage: drop stale pre-gap variance so sigma re-warms cleanly.
            self.variance_per_sec = 0.0;
            self.weight = 0.0;
            self.last = Some((mid, now));
            return;
        }
        let dt = elapsed.as_secs_f64();
        if dt < Self::MIN_DT_SECS {
            return; // Keep the older anchor so the next dt is meaningful.
        }
        self.last = Some((mid, now));
        if dt > Self::MAX_DT_SECS {
            return;
        }
        let r = (mid / last_mid).ln();
        if !r.is_finite() {
            return;
        }
        let alpha = 1.0 - (-dt / Self::TAU_SECS).exp();
        self.variance_per_sec += alpha * (r * r / dt - self.variance_per_sec);
        self.weight += alpha * (1.0 - self.weight);
    }

    /// Sigma per sqrt(second), or `None` until enough time has been observed.
    fn sigma(&self) -> Option<f64> {
        (self.weight >= Self::WARM_WEIGHT).then(|| self.variance_per_sec.max(0.0).sqrt())
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

/// Decaying fair-value offset from realized passive fills (Vertox §5 / VMM-06).
///
/// Decouples immediate adverse-selection information ("someone just hit our bid
/// or lifted our ask") from inventory skew (`q`), which remains non-zero on old
/// inventory and drops to zero when a passive fill flattens inventory to flat.
#[derive(Clone, Copy, Debug, Default)]
struct SelfFillAlpha {
    offset_bps: f64,
    updated_at: Option<Instant>,
    last_fill_seq: u64,
}

impl SelfFillAlpha {
    fn decay_to(&mut self, at: Instant, half_life_ms: f64) {
        if let Some(prev) = self.updated_at
            && let Some(elapsed) = at.checked_duration_since(prev)
        {
            let dt_ms = elapsed.as_secs_f64() * 1000.0;
            if dt_ms > 0.0 && self.offset_bps != 0.0 {
                self.offset_bps *= 0.5_f64.powf(dt_ms / half_life_ms);
            }
            self.updated_at = Some(at);
        }
    }

    fn observe(
        &mut self,
        tracker: &OrderTracker,
        now: Instant,
        alpha_bps: f64,
        half_life_ms: u64,
    ) -> f64 {
        let half_life_ms = (half_life_ms.max(1)) as f64;
        let clamp_bps = 3.0 * alpha_bps;
        let latest = tracker.total_fill_stats().0;
        let oldest_retained = latest.saturating_sub(crate::order_tracker::RECENT_FILLS as u64 - 1);
        let next = self.last_fill_seq.saturating_add(1);
        for sequence in next.max(oldest_retained)..=latest {
            if let Some(fill) = tracker.fill_at(sequence) {
                self.decay_to(fill.at, half_life_ms);
                let impulse = match fill.side {
                    OrderSide::Sell => alpha_bps,
                    OrderSide::Buy => -alpha_bps,
                };
                self.offset_bps = (self.offset_bps + impulse).clamp(-clamp_bps, clamp_bps);
                self.updated_at = Some(self.updated_at.map_or(fill.at, |prev| prev.max(fill.at)));
            }
        }
        self.last_fill_seq = self.last_fill_seq.max(latest);
        self.decay_to(now, half_life_ms);
        self.offset_bps
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
    reduce_only: bool,
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
const RECONCILE_INTERVAL_SEC: u64 = 30;
const ACTIVE_POSITION_RECONCILE_INTERVAL_SEC: u64 = 3;
const GC_INTERVAL_SEC: u64 = 300;
const LOCAL_FALLBACK_SPREAD_TICKS: f64 = 2.0;

/// Where the latest accepted price sample came from and when. Set only where
/// the sample is taken, so exporting or re-reading never makes it newer.
#[derive(Debug, Clone, Copy, PartialEq)]
struct PriceEvidence {
    mid: f64,
    /// `external_median` (basis-corrected), `lighter_book` or `lighter_depth`.
    source: &'static str,
    /// Local wall clock (ms since epoch) when the strategy took the sample.
    sampled_ms: u64,
    /// Oldest SHM feed timestamp (ms since epoch) among all inputs of `mid`
    /// (for `lighter_depth`: the depth snapshot and the BBO touch prices).
    /// None when any input lacks a timestamp. The feeder writes venue time
    /// for Binance/Hyperliquid, receipt time for EdgeX, and venue time with a
    /// receipt-time fallback for Lighter.
    feed_ms: Option<u64>,
}
// ─── Inventory-Neutral Market Maker ──────────────────────────────────────────
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct PricingInputs {
    mid: f64,
    pricing_mid: f64,
    bid_touch: f64,
    ask_touch: f64,
    vol_bps: f64,
    /// A-S sigma in fractional units per sqrt(second).
    sigma_per_sqrt_sec: f64,
    as_score: f64,
    using_external_fair_value: bool,
    order_flow_bps: f64,
    local_ofi_bps: f64,
    cancel_bump_bps: f64,
    cross_market_bps: f64,
    volatility_pressure: f64,
    #[cfg(feature = "research-replay")]
    participation_bbo: Option<ShmBboMessage>,
}

/// Result of one quoting-loop iteration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CycleOutcome {
    /// Shutdown requested; leave the loop.
    Stop,
    /// A guard withheld quoting; retry after this delay.
    Backoff(Duration),
    /// Quotes were evaluated; wait for the next market or fill event.
    Quoted,
}

#[cfg(feature = "research-replay")]
fn cancel_reason_for_label(reason: &str) -> CancelReason {
    match reason {
        "maker edge below floor" => CancelReason::EdgeFloorCancelAll,
        "position-timeout flatten" | "shutdown" => CancelReason::TimeoutOrShutdownCancelAll,
        _ => CancelReason::RiskOrSpreadCancelAll,
    }
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
    /// Fair-mark anchor (`Decimal`) latched on the first price evaluation after each
    /// `AccountStats` update so intra-snapshot price drops immediately adjust unrealized
    /// equity in `lighter::risk::evaluate_margin_health` without double-counting Lighter's mark.
    account_stats_fair_mark_anchor: Option<rust_decimal::Decimal>,
    micro: MicrostructureTracker,
    sigma: TimeScaledVolatility,
    basis: BasisTracker,
    order_flow: OrderFlow,
    local_order_flow: OrderFlow,
    last_local_ofi_norm: f64,
    last_local_l1_notional: Option<[f64; 2]>,
    cross_market_pressure: CrossMarketPressure,
    volatility_regime: volatility_regime::VolatilityRegime,
    defense: defense::DefenseStateMachine,
    toxicity_withdrawn_until: [Option<Instant>; 2],
    toxicity_latched: [bool; 2],
    adverse_retreat_cooldown_until: [Option<Instant>; 2],
    external_entry_side_veto: Option<([bool; 2], Instant)>,
    #[cfg(feature = "research-replay")]
    quote_participation: Option<replay::recorded::QuoteParticipation>,
    #[cfg(feature = "research-replay")]
    recorded_local_fair_value:
        Option<Arc<parking_lot::Mutex<replay::recorded::local_fair_value::Signal>>>,
    #[cfg(feature = "research-replay")]
    local_participation: Option<replay::recorded::local_signal::LocalParticipation>,
    #[cfg(feature = "research-replay")]
    binance_participation: Option<replay::recorded::binance_signal::Participation>,
    /// Last per-sample microstructure update; keeps its calibration at the
    /// poll cadence even when event-driven cycles run faster.
    last_micro_sample: Option<Instant>,
    price_evidence: Option<PriceEvidence>,
    markout: markout::MarkoutTracker,
    self_fill_alpha: SelfFillAlpha,
    /// Fill count captured before this cycle read any inventory state. The
    /// wait compares against it, so a fill landing mid-cycle still wakes the
    /// next cycle instead of being folded into the wait's own baseline.
    cycle_fill_baseline: u64,

    // Order tracking (multi-level grid)
    active_orders: Vec<ActiveOrder>,

    session_start_balance: f64,
    daily_loss_state: Option<std::path::PathBuf>,
    daily_loss: Option<crate::daily_loss::DailyLossGuard>,
    /// Set when low margin hits while hedged; the loop then stops so shutdown
    /// unwinds both legs together instead of flattening Lighter alone.
    hedged_low_margin: bool,
    total_orders_placed: u64,
    last_balance_check: Instant,
    reconcile_failures: u32,
    last_reconciled_fill_count: u64,
    last_fill_at: Option<Instant>,
    last_execution_batch_at: Option<Instant>,
    margin_cooldown_until: Instant,
    stale_bbo_since_ns: Option<u64>,
    inventory_age: InventoryAge,
    episode_flat_cash_flow: Option<f64>,
    episode_direction: f64,
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
        let shm_depth_reader = crate::shm_path::shm_path("aleph-depth")
            .ok()
            .and_then(|path| crate::shm_depth_reader::ShmDepthReader::open(&path, 2048).ok());
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
            sigma: TimeScaledVolatility::new(),
            basis: BasisTracker::new(config.basis_tau_secs, config.max_basis_bps),
            order_flow: OrderFlow::default(),
            local_order_flow: OrderFlow::default(),
            last_local_ofi_norm: 0.0,
            last_local_l1_notional: None,
            cross_market_pressure: CrossMarketPressure::default(),
            volatility_regime: volatility_regime::VolatilityRegime::default(),
            defense: defense::DefenseStateMachine::default(),
            toxicity_withdrawn_until: [None; 2],
            toxicity_latched: [false; 2],
            adverse_retreat_cooldown_until: [None; 2],
            external_entry_side_veto: None,
            #[cfg(feature = "research-replay")]
            quote_participation: None,
            #[cfg(feature = "research-replay")]
            recorded_local_fair_value: None,
            #[cfg(feature = "research-replay")]
            local_participation: None,
            #[cfg(feature = "research-replay")]
            binance_participation: None,
            last_micro_sample: None,
            price_evidence: None,
            cycle_fill_baseline: 0,
            markout: markout::MarkoutTracker::new(
                config.markout_horizon_secs,
                config.markout_halflife_fills,
                config.markout_threshold_bps,
            )
            .with_side_samples(
                config
                    .markout_reserve
                    .map(|policy| Duration::from_millis(policy.max_sample_age_ms)),
            ),
            self_fill_alpha: SelfFillAlpha::default(),
            config,
            trading,
            order_tracker,
            hedge: None,
            use_external_fair_value: true,
            shm_depth_reader,
            account_stats_reader,
            account_stats: AccountStats::default(),
            account_stats_fair_mark_anchor: None,
            active_orders: Vec::with_capacity(32),
            session_start_balance: 0.0,
            daily_loss_state: None,
            daily_loss: None,
            hedged_low_margin: false,
            total_orders_placed: 0,
            last_balance_check: Instant::now(),
            reconcile_failures: 0,
            last_reconciled_fill_count: 0,
            last_fill_at: None,
            last_execution_batch_at: None,
            margin_cooldown_until: Instant::now(),
            stale_bbo_since_ns: None,
            inventory_age: InventoryAge::default(),
            episode_flat_cash_flow: None,
            episode_direction: 0.0,
            position_confirmation_after_ns: None,
            telemetry: TelemetryCollector::new(),
            telemetry_sink: None,
            is_running: Arc::new(AtomicBool::new(true)),
            shm_reader,
        }
    }

    pub fn with_inventory_hedge(mut self, mut hedge: LiveInventoryHedge) -> Self {
        hedge.set_telemetry(self.telemetry_sink.clone());
        self.hedge = Some(hedge);
        self
    }

    /// Export structured snapshots for remote monitoring.
    pub fn with_telemetry_sink(mut self, sink: crate::telemetry_sink::TelemetrySink) -> Self {
        self.telemetry_sink = Some(sink);
        self
    }

    /// Persist the daily loss baseline and breach flag at `path` so restarts
    /// cannot re-arm the loss budget within the same UTC day.
    pub fn with_daily_loss_state(mut self, path: std::path::PathBuf) -> Self {
        self.daily_loss_state = Some(path);
        self
    }

    pub fn use_local_fair_value(&mut self) {
        self.use_external_fair_value = false;
    }

    /// Apply a temporary one-sided entry veto (e.g. from `CrossSectionalEngine::mm_blocked_entry_sides`)
    /// without blocking inventory-reducing quotes or adding extra config knobs (`S8-4`).
    pub fn set_external_entry_side_veto(
        &mut self,
        block_bid_entry: bool,
        block_ask_entry: bool,
        ttl: Duration,
    ) {
        if !block_bid_entry && !block_ask_entry {
            self.external_entry_side_veto = None;
        } else {
            self.external_entry_side_veto =
                Some(([block_bid_entry, block_ask_entry], Instant::now() + ttl));
        }
    }

    pub fn clear_external_entry_side_veto(&mut self) {
        self.external_entry_side_veto = None;
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
            match self.quote_cycle(shutdown).await? {
                CycleOutcome::Stop => return Ok(()),
                CycleOutcome::Backoff(delay) => tokio::time::sleep(delay).await,
                CycleOutcome::Quoted => self.wait_for_next_cycle(shutdown).await,
            }
        }

        Ok(())
    }

    /// One iteration of the quoting loop. Pacing is left to the caller so the
    /// same production logic can be driven step by step by offline replay.
    pub(crate) async fn quote_cycle(
        &mut self,
        shutdown: Option<&tokio::sync::watch::Receiver<bool>>,
    ) -> anyhow::Result<CycleOutcome> {
        if shutdown_requested(shutdown) {
            return Ok(CycleOutcome::Stop);
        }

        // Capture before any inventory read: fills after this point are
        // unconsumed by this cycle and must wake the next one.
        self.cycle_fill_baseline = self.order_tracker.total_fill_stats().0;
        self.refresh_active_orders_from_tracker();
        if let Some(stats) = self.account_stats_reader.read_if_updated() {
            self.account_stats = stats.into();
            self.account_stats_fair_mark_anchor = None;
        }

        if self.order_tracker.needs_reconciliation() {
            self.reconcile_in_session("event-stream recovery").await?;
        }
        if self.account_stats.last_update.elapsed() <= Duration::from_secs(ACCOUNT_STALENESS_SECS)
            && self.account_stats.portfolio_value.is_finite()
            && self.account_stats.portfolio_value <= 0.0
        {
            self.check_daily_loss()?;
        }
        if !self.account_stats.is_usable() {
            warn!("Account equity/position snapshot unavailable or stale; withdrawing quotes");
            self.cancel_all_and_sync("stale account snapshot").await;
            if let Some(hedge) = &mut self.hedge {
                hedge.check_blocked(true)?;
            }
            return Ok(CycleOutcome::Backoff(Duration::from_secs(1)));
        }
        self.check_daily_loss()?;
        if !self.prepare_inventory_hedge().await? {
            return Ok(CycleOutcome::Backoff(Duration::from_millis(500)));
        }

        // Phase 1: Institutional Housekeeping
        self.perform_periodic_housekeeping().await?;

        // Margin cooldown check
        if Instant::now() < self.margin_cooldown_until {
            return Ok(CycleOutcome::Backoff(Duration::from_millis(100)));
        }

        // Phase 2: Market Data Acquisition & Staleness Check
        let market_state = match self.fetch_market_state().await {
            Some(state) => state,
            None => {
                return Ok(CycleOutcome::Backoff(Duration::from_millis(100)));
            }
        };

        // Phase 3: Signal Processing & Fair Price Determination
        let inputs = self.calculate_pricing_inputs(&market_state.exchanges, &market_state.bbo);

        if self.use_external_fair_value
            && self.config.require_external_fair_value
            && !inputs.using_external_fair_value
        {
            tracing::warn!(
                "All external fair-value references are stale or unavailable; canceling quotes instead of quoting off local Lighter book"
            );
            self.cancel_all_orders().await;
            return Ok(CycleOutcome::Backoff(Duration::from_millis(250)));
        }

        if inputs.mid <= 0.0 || inputs.mid.is_nan() || inputs.mid.is_infinite() {
            tracing::warn!("⚠️ Invalid mid price: {:.4}, skipping cycle", inputs.mid);
            return Ok(CycleOutcome::Backoff(Duration::from_millis(100)));
        }

        let quoting_position = {
            let max_position =
                scaled_max_position(&self.config, self.account_stats.portfolio_value, inputs.mid)
                    .max(self.config.step_size);
            self.trading.set_max_worst_case_position(max_position);
            let mut runtime_config = self.config.runtime_copy();
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

        self.update_episode_cost_basis();

        if let Some(band_bps) = self.config.max_order_price_band_bps {
            self.trading
                .set_reference_price_band(inputs.pricing_mid, band_bps);
        }

        let local_spread_bps =
            ((inputs.ask_touch - inputs.bid_touch).max(0.0) / inputs.mid) * 10_000.0;
        let can_recover_defense = inputs.ask_touch > inputs.bid_touch
            && self.account_stats.is_usable()
            && !self.order_tracker.needs_reconciliation();
        let defense_regime = self.defense.evaluate(
            self.config.defense_policy,
            wall_time_ns() / 1_000_000,
            local_spread_bps,
            self.micro.momentum_bps(),
            can_recover_defense,
        );
        if defense_regime == defense::DefenseRegime::Halt {
            warn!(
                local_spread_bps,
                momentum_bps = self.micro.momentum_bps(),
                "Defense state machine entered HALT; canceling all open orders"
            );
            self.cancel_all_orders().await;
            return Ok(CycleOutcome::Quoted);
        }

        if let Some((fair_mark_f64, margin_health)) = self.evaluate_lighter_margin_health(&inputs)
            && margin_health.regime != MarginRegime::Healthy
        {
            warn!(
                "Lighter Fair-Mark margin guard ({:?}): equity=${} mult={:?} fair_mark={:.4} partial_liq={:?} zero_px={:?}; deleveraging proactively",
                margin_health.regime,
                margin_health.account_value.round_dp(2),
                margin_health.maintenance_multiple.map(|m| m.round_dp(3)),
                fair_mark_f64,
                margin_health
                    .partial_liquidation_mark
                    .map(|m| m.round_dp(2)),
                margin_health.zero_price_trigger_mark.map(|m| m.round_dp(2)),
            );
            self.handle_low_margin_flatten(fair_mark_f64).await;
            if self.hedged_low_margin {
                anyhow::bail!("Low margin while hedged; unwinding both legs");
            }
            return Ok(CycleOutcome::Quoted);
        }

        // Inventory age follows the exposure episode, not the last fill.
        // Adding to a stuck position must never renew its timeout.
        let deadband = if self.hedge.is_none()
            && self
                .config
                .quote_hysteresis
                .is_some_and(|h| h.zero_deadband_flatten)
        {
            (self.config.step_size * 0.5).max(components::FLOAT_EPSILON)
        } else {
            inventory_deadband_size(
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
            )
        };
        let now = Instant::now();
        self.inventory_age.observe(quoting_position, deadband, now);
        // With a live hedge, Lighter inventory is offset on Hyperliquid;
        // a single-leg taker flatten would pay twice and unbalance the pair.
        if self.hedge.is_none()
            && self
                .inventory_age
                .is_expired(now, Duration::from_secs(self.config.position_timeout_secs))
        {
            if self.inventory_age.can_retry(now) {
                self.inventory_age.last_attempt = Some(now);
                self.execute_timeout_flatten(inputs.mid).await;
            }
        } else {
            let quote = self.calculate_optimal_quotes(&inputs, quoting_position);
            #[cfg(feature = "research-replay")]
            let changed_sides =
                replay::recorded::attribute_quote(self, &inputs, quoting_position, quote);
            if let Some((our_bid, our_ask, bounds)) = quote {
                self.execute_quoting_cycle(
                    our_bid,
                    our_ask,
                    bounds,
                    &inputs,
                    shutdown,
                    #[cfg(feature = "research-replay")]
                    changed_sides,
                )
                .await;
                if self.hedged_low_margin {
                    // perform_graceful_shutdown cleans Lighter, then
                    // hedge.finish() closes Hyperliquid against it.
                    anyhow::bail!("Low margin while hedged; unwinding both legs");
                }
            } else {
                self.cancel_all_orders().await;
            }
        }

        Ok(CycleOutcome::Quoted)
    }

    fn check_daily_loss(&mut self) -> anyhow::Result<()> {
        if let Some(guard) = &mut self.daily_loss {
            guard
                .check(self.account_stats.portfolio_value, strategy_utc_day())
                .context("Lighter daily loss check failed")?;
        }
        Ok(())
    }

    /// Evaluates venue Fair Mark Price and pre-liquidation margin health via
    /// `Exchange::evaluate_venue_margin_health`, adjusting reported `portfolio_value`
    /// for intra-snapshot Fair Mark drift while clamping spoofed local books.
    fn evaluate_lighter_margin_health(
        &mut self,
        inputs: &PricingInputs,
    ) -> Option<(f64, AccountMarginHealth)> {
        use rust_decimal::{Decimal, prelude::ToPrimitive};

        let index_px = Decimal::from_f64_retain(inputs.pricing_mid)?;
        let impact_bid = Decimal::from_f64_retain(inputs.bid_touch)?;
        let impact_ask = Decimal::from_f64_retain(inputs.ask_touch)?;
        let ema_raw = if self.micro.ema_initialized
            && self.micro.ema_slow.is_finite()
            && self.micro.ema_slow > 0.0
        {
            self.micro.ema_slow
        } else {
            inputs.mid
        };
        let ema_px = Decimal::from_f64_retain(ema_raw)?;
        let pos_f64 = if self.account_stats.position.abs() >= self.config.step_size {
            self.account_stats.position
        } else {
            0.0
        };
        let signed_position = Decimal::from_f64_retain(pos_f64)?;
        let snapshot_equity =
            Decimal::from_f64_retain(self.account_stats.portfolio_value.max(0.0))?;
        let mmf_f64 = self.config.effective_maintenance_margin_fraction();
        let maintenance_margin_fraction = Decimal::from_f64_retain(mmf_f64)?;
        let safety_multiple =
            Decimal::from_f64_retain(self.config.pre_liquidation_safety_multiple)?;

        let margin_inputs = MarginCheckInputs {
            index_px,
            impact_bid,
            impact_ask,
            ema_px,
            anchor_mark_px: self.account_stats_fair_mark_anchor,
            signed_position,
            snapshot_equity,
            maintenance_margin_fraction,
            safety_multiple,
        };
        let (fair_mark, health) = self.trading.evaluate_venue_margin_health(&margin_inputs)?;
        self.account_stats_fair_mark_anchor.get_or_insert(fair_mark);
        let fair_mark_f64 = fair_mark.to_f64().unwrap_or(inputs.pricing_mid);
        Some((fair_mark_f64, health))
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
        let refreshing = match hedge.poll().await {
            Ok(refreshing) => refreshing,
            Err(error) => {
                self.cancel_all_orders().await;
                return Err(error);
            }
        };
        let (confirmed, short, long) = self.order_tracker.exposure_bounds();
        let blocked = self.quote_allowance(confirmed, short, long).is_none();
        if blocked {
            if refreshing
                && self
                    .active_orders
                    .iter()
                    .all(|order| order.order_index.is_some())
            {
                // Known cancels can go directly to the venue while REST
                // position and book reads are outstanding.
                self.cancel_all_orders().await;
            } else if self.cancel_all_and_sync("inventory hedge").await
                && !refreshing
                && let Some(hedge) = &mut self.hedge
            {
                hedge.rebalance().await?;
            }
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
        self.daily_loss = Some(crate::daily_loss::DailyLossGuard::open(
            self.daily_loss_state.clone(),
            self.config.max_daily_loss_pct,
            self.account_stats.portfolio_value,
            strategy_utc_day(),
        )?);
        // A restart can already be near the daily limit. Publish immediately,
        // including while the quote loop is waiting for hedge reconciliation.
        self.export_snapshot(self.account_stats.position);

        info!(
            "✅ Startup complete: pos={:.4} bal=${:.2}",
            self.account_stats.position, self.account_stats.portfolio_value
        );
        Ok(())
    }

    fn build_risk_snapshot(&self, mid: f64) -> RiskSnapshot {
        let exchange_position = self.account_stats.position;
        // Quote direction should follow confirmed inventory, not pending-order imbalance.
        // Pending buy/sell exposure is still enforced via worst_case_long/short in risk limits.
        let (tracker_confirmed, tracker_short, tracker_long) = self.order_tracker.exposure_bounds();
        let raw_base_order_size =
            scaled_base_order_size(&self.config, self.account_stats.portfolio_value, mid)
                .max(self.config.step_size);
        let mut base_order_size = if self.config.round_base_notional_nearest
            && self.config.base_order_notional_usd > 0.0
        {
            components::round_nearest_to_step(raw_base_order_size, self.config.step_size)
        } else {
            components::round_down_to_step(raw_base_order_size, self.config.step_size)
        }
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

        let mut runtime_config = self.config.runtime_copy();
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
        let margin_per_unit = mid / self.config.max_leverage;
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
            margin_per_unit,
            grid_multiplier,
        }
    }

    fn update_episode_cost_basis(&mut self) {
        let half_step = (self.config.step_size * 0.5).max(components::FLOAT_EPSILON);
        let confirmed_pos = self.order_tracker.confirmed_position();
        let account_pos = self.account_stats.position;
        if confirmed_pos.abs() < half_step && account_pos.abs() < half_step {
            self.episode_flat_cash_flow = Some(self.order_tracker.trade_cash_flow());
            self.episode_direction = 0.0;
        } else if confirmed_pos.abs() >= half_step {
            if (account_pos - confirmed_pos).abs() > self.config.step_size
                || (self.episode_direction != 0.0
                    && confirmed_pos.signum() != self.episode_direction)
            {
                self.episode_flat_cash_flow = None;
            }
            self.episode_direction = confirmed_pos.signum();
        }
    }

    fn build_quote_target(
        &mut self,
        our_bid: f64,
        our_ask: f64,
        mid: f64,
        risk: &RiskSnapshot,
        volatility_pressure: f64,
    ) -> Option<QuoteTarget> {
        let spread_bps = ((our_ask - our_bid) / mid) * 10000.0;
        self.telemetry.update_spread_size(spread_bps);
        self.telemetry
            .update_adverse_selection(self.micro.adverse_selection_score());

        let hysteresis = self
            .config
            .quote_hysteresis
            .filter(|_| self.hedge.is_none());
        let passive_touch_active = self.passive_touch_flatten_reducing_sides(
            risk.position_for_quoting,
            self.current_inventory_age_fraction(),
        ) != [false, false];
        let min_spread_bps = if passive_touch_active {
            (self.config.tick_size / mid.max(self.config.tick_size)) * 10_000.0
        } else if let Some(policy) = hysteresis {
            let mut runtime_config = self.config.runtime_copy();
            runtime_config.inventory_urgency_threshold = risk.inventory_urgency_threshold;
            let urgency_ratio = inventory_skew_ratio(&runtime_config, risk.position_for_quoting);
            let ([bid_flatten, ask_flatten], _) =
                self.flattening_progress(risk.position_for_quoting, mid, urgency_ratio, policy);
            if policy.zero_deadband_flatten && (bid_flatten > 0.0 || ask_flatten > 0.0) {
                (self.config.maker_fee_bps * 2.0 + policy.flatten_min_profit_bps).max(0.0)
            } else {
                self.minimum_spread_bps()
            }
        } else {
            self.minimum_spread_bps()
        };
        if our_ask <= our_bid || spread_bps + components::FLOAT_EPSILON < min_spread_bps {
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
        let regime_scale = self.config.volatility_regime.map_or(1.0, |regime| {
            1.0 - volatility_pressure * (1.0 - regime.min_size_scale)
        });
        let position = risk.position_for_quoting;
        let bid_scale = if position >= 0.0 { regime_scale } else { 1.0 };
        let ask_scale = if position <= 0.0 { regime_scale } else { 1.0 };
        let mut bid_size = components::round_down_to_step(
            bid_size * toxicity_scale * bid_scale,
            self.config.step_size,
        );
        let mut ask_size = components::round_down_to_step(
            ask_size * toxicity_scale * ask_scale,
            self.config.step_size,
        );
        let half_step = (self.config.step_size * 0.5).max(components::FLOAT_EPSILON);
        if let Some(depth_frac) = self.config.max_top_book_depth_fraction {
            let min_bid = components::min_quotable_size(&self.config, our_bid);
            let min_ask = components::min_quotable_size(&self.config, our_ask);
            if position >= -half_step {
                let cap_sz = match self.last_local_l1_notional {
                    Some([bid_l1_usd, _]) if our_bid > 0.0 => components::round_down_to_step(
                        (depth_frac * bid_l1_usd) / our_bid,
                        self.config.step_size,
                    ),
                    _ => 0.0,
                };
                bid_size = bid_size.min(cap_sz);
                if bid_size + components::FLOAT_EPSILON < min_bid {
                    bid_size = 0.0;
                }
            }
            if position <= half_step {
                let cap_sz = match self.last_local_l1_notional {
                    Some([_, ask_l1_usd]) if our_ask > 0.0 => components::round_down_to_step(
                        (depth_frac * ask_l1_usd) / our_ask,
                        self.config.step_size,
                    ),
                    _ => 0.0,
                };
                ask_size = ask_size.min(cap_sz);
                if ask_size + components::FLOAT_EPSILON < min_ask {
                    ask_size = 0.0;
                }
            }
        }
        if let Some(max_mom) = hysteresis.and_then(|policy| policy.max_local_momentum_bps) {
            let mom = self.micro.momentum_bps();
            if mom < -max_mom && position >= -half_step {
                bid_size = 0.0;
            }
            if mom > max_mom && position <= half_step {
                ask_size = 0.0;
            }
        }
        if let Some(tox) = self.config.local_toxicity {
            if tox.withdraw_cooldown_ms > 0 || tox.withdraw_clear_ratio.is_some() {
                let now = Instant::now();
                let cooldown = Duration::from_millis(tox.withdraw_cooldown_ms);
                let clear_ratio = tox.withdraw_clear_ratio.unwrap_or(0.7);
                let vol_bps = self.micro.volatility_bps();
                let vol_trip = tox.vol_pause_bps.is_some_and(|v| vol_bps >= v);
                let vol_clear = tox.vol_pause_bps.is_none_or(|v| vol_bps < v * clear_ratio);

                let buy_ofi_trip = tox
                    .ofi_withdraw_threshold
                    .is_some_and(|t| self.last_local_ofi_norm <= -t);
                let buy_ofi_clear = tox
                    .ofi_withdraw_threshold
                    .is_none_or(|t| self.last_local_ofi_norm > -(t * clear_ratio));
                if buy_ofi_trip || vol_trip {
                    self.toxicity_latched[0] = true;
                    if tox.withdraw_cooldown_ms > 0 {
                        self.toxicity_withdrawn_until[0] = Some(now + cooldown);
                    }
                } else if buy_ofi_clear && vol_clear {
                    self.toxicity_latched[0] = false;
                }
                if position >= -half_step
                    && (buy_ofi_trip
                        || vol_trip
                        || self.toxicity_latched[0]
                        || self.toxicity_withdrawn_until[0].is_some_and(|u| now < u))
                {
                    bid_size = 0.0;
                }

                let sell_ofi_trip = tox
                    .ofi_withdraw_threshold
                    .is_some_and(|t| self.last_local_ofi_norm >= t);
                let sell_ofi_clear = tox
                    .ofi_withdraw_threshold
                    .is_none_or(|t| self.last_local_ofi_norm < t * clear_ratio);
                if sell_ofi_trip || vol_trip {
                    self.toxicity_latched[1] = true;
                    if tox.withdraw_cooldown_ms > 0 {
                        self.toxicity_withdrawn_until[1] = Some(now + cooldown);
                    }
                } else if sell_ofi_clear && vol_clear {
                    self.toxicity_latched[1] = false;
                }
                if position <= half_step
                    && (sell_ofi_trip
                        || vol_trip
                        || self.toxicity_latched[1]
                        || self.toxicity_withdrawn_until[1].is_some_and(|u| now < u))
                {
                    ask_size = 0.0;
                }
            } else {
                if let Some(thresh) = tox.ofi_withdraw_threshold {
                    if self.last_local_ofi_norm <= -thresh && position >= -half_step {
                        bid_size = 0.0;
                    }
                    if self.last_local_ofi_norm >= thresh && position <= half_step {
                        ask_size = 0.0;
                    }
                }
                if let Some(max_vol) = tox.vol_pause_bps
                    && self.micro.volatility_bps() >= max_vol
                {
                    if position >= -half_step {
                        bid_size = 0.0;
                    }
                    if position <= half_step {
                        ask_size = 0.0;
                    }
                }
            }
        }
        match self.defense.regime() {
            defense::DefenseRegime::Normal => {}
            defense::DefenseRegime::Caution => {
                if let Some(def) = self.config.defense_policy {
                    if position >= -half_step && bid_size > 0.0 {
                        bid_size = components::round_down_to_step(
                            bid_size * def.caution_size_scale,
                            self.config.step_size,
                        );
                    }
                    if position <= half_step && ask_size > 0.0 {
                        ask_size = components::round_down_to_step(
                            ask_size * def.caution_size_scale,
                            self.config.step_size,
                        );
                    }
                }
            }
            defense::DefenseRegime::SideDefense {
                block_buy,
                block_sell,
            } => {
                if block_buy && position >= -half_step {
                    bid_size = 0.0;
                }
                if block_sell && position <= half_step {
                    ask_size = 0.0;
                }
            }
            defense::DefenseRegime::ReduceOnly | defense::DefenseRegime::Halt => {
                if position >= -half_step {
                    bid_size = 0.0;
                }
                if position <= half_step {
                    ask_size = 0.0;
                }
            }
        }
        if let Some(min_q) = self.config.min_volume_quota_for_quoting
            && self
                .trading
                .volume_quota_snapshot()
                .and_then(|s| s.remaining)
                .is_some_and(|rem| rem < min_q)
        {
            if position >= -half_step {
                bid_size = 0.0;
            }
            if position <= half_step {
                ask_size = 0.0;
            }
        }
        if let Some(([veto_bid, veto_ask], until)) = self.external_entry_side_veto
            && Instant::now() < until
        {
            if veto_bid && position >= -half_step {
                bid_size = 0.0;
            }
            if veto_ask && position <= half_step {
                ask_size = 0.0;
            }
        }

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
            #[cfg(feature = "research-replay")]
            let _cancel_scope = replay::recorded::CancelReasonScope::open_default(
                CancelReason::RiskOrSpreadCancelAll,
            );
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
        if let Some(sink) = &self.telemetry_sink
            && let Err(error) = sink.stopped().await
        {
            warn!(%error, "Could not persist final telemetry records");
        }
        info!("👋 Strategy stopped cleanly.");
        Ok(())
    }

    /// Returns whether REST confirms an empty book. This alone never releases
    /// reserved exposure: cleanup also needs an independent fresh position.
    async fn cancel_all_and_sync(&mut self, reason: &str) -> bool {
        let request_started = wall_time_ns();
        #[cfg(feature = "research-replay")]
        let _cancel_scope =
            replay::recorded::CancelReasonScope::open(cancel_reason_for_label(reason));
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
                self.account_stats_fair_mark_anchor = None;
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        anyhow::bail!(
            "Cleanup cannot confirm a fresh equity and position snapshot after pending actions"
        )
    }

    async fn clean_exchange_state(&mut self, phase: &str) -> anyhow::Result<()> {
        self.recover_exchange_state(phase, true).await
    }

    async fn reconcile_in_session(&mut self, phase: &str) -> anyhow::Result<()> {
        self.recover_exchange_state(phase, false).await
    }

    async fn recover_exchange_state(
        &mut self,
        phase: &str,
        flatten_residual: bool,
    ) -> anyhow::Result<()> {
        let mut reconciliation_epoch = self.order_tracker.reconciliation_epoch();
        if self.order_tracker.has_late_fills() {
            self.position_confirmation_after_ns = Some(wall_time_ns());
            if flatten_residual {
                self.trading
                    .recover_flat_without_feed()
                    .await
                    .with_context(|| format!("{phase}: could not reconcile late Lighter fills"))?;
            } else {
                self.trading
                    .reconcile_non_flat_without_feed()
                    .await
                    .with_context(|| format!("{phase}: could not reconcile late Lighter fills"))?;
            }
            reconciliation_epoch = self.order_tracker.last_reconciled_epoch();
        }
        for attempt in 0..3 {
            let canceled = self.cancel_all_and_sync(phase).await;
            if !canceled {
                if attempt < 2 {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    continue;
                }
                if self.order_tracker.active_order_count() == 0 {
                    anyhow::bail!(
                        "{phase}: cancellation remains unconfirmed; reserved exposure retained"
                    );
                }
            }
            // Cancellation may have delivered terminal events already. If any
            // reservation remains, an empty REST list plus a fresh position is
            // insufficient: require order history and its account fill watermark.
            if self.order_tracker.active_order_count() > 0 {
                self.position_confirmation_after_ns = Some(wall_time_ns());
                let recovery_ctx = || {
                    if !canceled {
                        format!(
                            "{phase}: cancellation remains unconfirmed; reserved exposure retained"
                        )
                    } else {
                        format!("{phase}: could not reconcile reserved Lighter orders")
                    }
                };
                if flatten_residual {
                    self.trading
                        .recover_flat_without_feed()
                        .await
                        .with_context(recovery_ctx)?;
                } else {
                    self.trading
                        .reconcile_non_flat_without_feed()
                        .await
                        .with_context(recovery_ctx)?;
                }
                reconciliation_epoch = self.order_tracker.last_reconciled_epoch();
            }
            self.read_confirmed_account_stats().await?;
            if !flatten_residual || self.account_stats.position == 0.0 {
                // This only rebases an already terminal tracker. Any new active
                // order or late fill during the awaits keeps recovery blocked.
                anyhow::ensure!(
                    self.order_tracker
                        .confirm_cleanup(reconciliation_epoch, self.account_stats.position),
                    "{phase}: event stream changed during {}; exposure retained and trading halted",
                    if flatten_residual {
                        "cleanup"
                    } else {
                        "reconciliation"
                    }
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

    fn export_snapshot(&self, quote_position: f64) {
        let Some(sink) = &self.telemetry_sink else {
            return;
        };
        // The risk path falls back to 1.0 without a price sample; that is not
        // a mark. Export 0 so consumers treat the valuation as missing.
        let evidence = self.price_evidence;
        let (fill_count, total_fees) = self.order_tracker.total_fill_stats();
        let markout = self.markout.stats();
        sink.snapshot(crate::telemetry_sink::StrategySnapshot {
            equity: self.account_stats.portfolio_value,
            session_start_equity: self.session_start_balance,
            daily_loss: self.daily_loss.as_ref().map(|guard| guard.snapshot()),
            available_balance: self.account_stats.available_balance,
            margin_usage: self.account_stats.margin_usage,
            exchange_position: self.account_stats.position,
            quote_position,
            confirmed_position: self.order_tracker.confirmed_position(),
            pending_exposure: self.order_tracker.net_pending_exposure(),
            worst_case_long: self.order_tracker.worst_case_long(),
            worst_case_short: self.order_tracker.worst_case_short(),
            mid: evidence.map_or(0.0, |e| e.mid),
            mark_source: evidence.map(|e| e.source),
            mark_sampled_ms: evidence.map(|e| e.sampled_ms),
            mark_feed_ms: evidence.and_then(|e| e.feed_ms),
            spread_bps: self.telemetry.spread_size_bps,
            adverse_selection: self.telemetry.adverse_selection_score,
            trade_cash_flow: self.order_tracker.trade_cash_flow(),
            total_fees,
            fill_count,
            orders_placed: self.total_orders_placed,
            orders_rejected: self.telemetry.orders_rejected,
            active_orders: self.order_tracker.active_order_count() as u32,
            markout_marked: markout.marked,
            markout_expired: markout.expired,
            markout_overwritten: markout.overwritten,
            markout_missed: markout.missed,
            markout_last_delay_ms: markout.last_delay_ms,
            markout_ewma_bps: markout.ewma_bps,
            volume_quota_remaining: self
                .trading
                .volume_quota_snapshot()
                .and_then(|snap| snap.remaining),
            defense_regime: self
                .config
                .defense_policy
                .map(|_| self.defense.regime().as_telemetry_str()),
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

        let markout = self.markout.stats();
        info!(
            "📊 PnL: ${:.2} ({:+.3}%) | Equity: ${:.2} | Avail (Safe/Raw/Usable): ${:.2}/${:.2}/${:.2} | Margin: {:.1}% | Pos (Exch/Quote/TrkConf/Pend/Eff): {:.4}/{:.4}/{:.4}/{:+.4}/{:.4} base | Worst (L/S): {:.4}/{:.4} | Orders: {} | Fills: {} ({:.1}/min) | Fees: ${:.4} | Markout (eq/notional/marked$): {:?}/{:?}/${:.0}",
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
            markout.ewma_bps,
            markout.notional_ewma_bps,
            markout.marked_notional_usd,
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
    ) -> Option<f64> {
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

        // Avoid division by zero: no usable depth, the caller falls back to
        // the touch mid.
        if !(bid_notional + ask_notional).is_finite() || bid_notional + ask_notional < 0.001 {
            return None;
        }

        // VWMicro formula: weight L1 prices by opposite side notional
        let bid_weight = bid_notional / (bid_notional + ask_notional);
        Some(bid_weight * ask_l1 + (1.0 - bid_weight) * bid_l1)
    }

    /// Periodic tasks: reconciliation, GC, PnL reporting, and telemetry export
    async fn perform_periodic_housekeeping(&mut self) -> anyhow::Result<()> {
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
                self.account_stats_fair_mark_anchor = None;
                self.check_daily_loss()?;
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
            // Housekeeping runs before execution in the same cycle; consuming
            // a new fill here must still start the post-fill cooldown.
            if fill_count > self.last_reconciled_fill_count {
                self.last_fill_at = Some(Instant::now());
            }
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
            self.export_snapshot(risk.position_for_quoting);
            self.last_balance_check = Instant::now();
        }
        Ok(())
    }

    /// Runs every cycle: reuse the buffer and avoid cloning tracked orders
    /// (each carries a fill Vec), so the steady state performs no allocation.
    fn refresh_active_orders_from_tracker(&mut self) {
        let active = &mut self.active_orders;
        active.clear();
        self.order_tracker.for_each_active_order(|order| {
            if !order.lifecycle.is_terminal() {
                active.push(ActiveOrder {
                    client_order_id: order.client_order_id,
                    order_index: order.order_index,
                    lifecycle: order.lifecycle,
                    side: order.side,
                    price: order.price,
                    size: order.remaining_size(),
                    reduce_only: order.reduce_only,
                    placed_at: order.created_at,
                });
            }
        });
        // Tracker storage is a HashMap with per-instance iteration order.
        // Client ids are assigned monotonically, so this fixes plan order and
        // makes capped replacements cancel the oldest order first. In place,
        // no allocation.
        active.sort_unstable_by_key(|order| order.client_order_id);
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
            let now_ns = wall_time_ns();
            let action = classify_stale_bbo(
                &market_state.bbo,
                now_ns,
                self.config.local_staleness_ms,
                self.config.stale_bbo_cancel_ms,
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
                            self.config.local_staleness_ms
                        );
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    return None;
                }
                StaleBboAction::Cancel => {
                    warn!(
                        "Stale BBO persisted for >{}ms (current age={}ms), canceling all orders",
                        self.config.stale_bbo_cancel_ms,
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

    /// Preserve canonical `lighter_*` mark-source names for Lighter Core/RH sessions and
    /// generic `local_*` provenance names for any other `exchange_id`.
    fn local_mark_sources(&self) -> (&'static str, &'static str, &'static str) {
        if self.config.exchange_id == crate::config::EXCH_LIGHTER
            || self.config.exchange_id == crate::config::EXCH_LIGHTER_ROBINHOOD
        {
            ("lighter_book", "lighter_depth", "lighter_touch_micro")
        } else {
            ("local_book", "local_depth", "local_touch_micro")
        }
    }

    fn defense_caution_widen_bps(&self) -> f64 {
        if self.defense.regime() == defense::DefenseRegime::Caution {
            self.config
                .defense_policy
                .map_or(0.0, |policy| policy.caution_widen_bps)
        } else {
            0.0
        }
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
        let now_ns = wall_time_ns();
        let fresh_local_l1 = bbo.bid_price > 0.0
            && bbo.ask_price > bbo.bid_price
            && bbo.bid_size.is_finite()
            && bbo.ask_size.is_finite()
            && bbo.bid_size > 0.0
            && bbo.ask_size > 0.0
            && !market_state::is_stale_bbo(
                bbo.timestamp_ns,
                now_ns,
                self.config.local_staleness_ms,
            );
        self.last_local_l1_notional =
            fresh_local_l1.then_some([bid_touch * bbo.bid_size, ask_touch * bbo.ask_size]);
        let external_fair = if self.use_external_fair_value {
            market_state::external_fair_value_mid_filtered(
                exchanges,
                self.config.exchange_id,
                now_ns,
                self.config.external_staleness_ms,
                self.config.external_reference_mask(),
            )
        } else {
            None
        };

        // A timestamp of 0 means unknown; it must not win a min() as "oldest".
        let known = |ts_ns: u64| (ts_ns > 0).then_some(ts_ns);
        let (pricing_mid, using_external, mid, source, source_ts_ns) =
            if let Some((fair_mid, fair_ts_ns)) = external_fair {
                // Learn the venue basis only from a fresh two-sided local book.
                if bbo.bid_price > 0.0
                    && bbo.ask_price > bbo.bid_price
                    && !market_state::is_stale_bbo(
                        bbo.timestamp_ns,
                        now_ns,
                        self.config.local_staleness_ms,
                    )
                {
                    self.basis
                        .observe(bbo.bid_price * 0.5 + bbo.ask_price * 0.5, fair_mid, now_ns);
                }
                // External exchanges drive fair value, shifted onto the Lighter
                // price level; local mid is only for touch.
                (
                    self.basis.correct(fair_mid),
                    true,
                    local_mid,
                    "external_median",
                    known(fair_ts_ns),
                )
            } else {
                // Fallback: use configured local fair value, VWMicro, or simple mid when no external data
                let (book_src, depth_src, micro_src) = self.local_mark_sources();
                let (vw_mid, source, source_ts_ns) = if let Some(local_fv) =
                    self.config.local_fair_value_config
                {
                    match local_fv.mode {
                        crate::config::LocalFairValueMode::Mid => {
                            (local_mid, book_src, known(bbo.timestamp_ns))
                        }
                        crate::config::LocalFairValueMode::TouchMicro => {
                            match market_state::local_touch_micro_price(bbo, local_fv.max_shift_bps)
                            {
                                Some(price) => (price, micro_src, known(bbo.timestamp_ns)),
                                None => (local_mid, book_src, known(bbo.timestamp_ns)),
                            }
                        }
                        crate::config::LocalFairValueMode::DepthMicro => {
                            let depth_res = self
                                .shm_depth_reader
                                .as_ref()
                                .and_then(|reader| {
                                    reader
                                        .read_depth(self.config.symbol_id, self.config.exchange_id)
                                })
                                .filter(|depth| {
                                    !market_state::is_stale_bbo(
                                        depth.timestamp_ns,
                                        now_ns,
                                        self.config.local_staleness_ms,
                                    )
                                })
                                .and_then(|depth| {
                                    let raw_vw = self
                                        .calculate_vw_micro_price(&depth, bid_touch, ask_touch)?;
                                    let clamped = if local_mid.is_finite() && local_mid > 0.0 {
                                        let lo =
                                            local_mid * (1.0 - local_fv.max_shift_bps / 10_000.0);
                                        let hi =
                                            local_mid * (1.0 + local_fv.max_shift_bps / 10_000.0);
                                        raw_vw.clamp(lo, hi)
                                    } else {
                                        raw_vw
                                    };
                                    Some((
                                        clamped,
                                        depth_src,
                                        known(depth.timestamp_ns)
                                            .zip(known(bbo.timestamp_ns))
                                            .map(|(depth_ns, bbo_ns)| depth_ns.min(bbo_ns)),
                                    ))
                                });
                            depth_res.unwrap_or_else(
                                || match market_state::local_touch_micro_price(
                                    bbo,
                                    local_fv.max_shift_bps,
                                ) {
                                    Some(price) => (price, micro_src, known(bbo.timestamp_ns)),
                                    None => (local_mid, book_src, known(bbo.timestamp_ns)),
                                },
                            )
                        }
                    }
                } else if self.config.use_depth_pricing {
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
                                self.config.local_staleness_ms,
                            )
                        })
                    {
                        match self.calculate_vw_micro_price(&depth, bid_touch, ask_touch) {
                            // Depth weights applied to the BBO touches: both
                            // inputs bound the evidence age.
                            Some(price) => (
                                price,
                                depth_src,
                                known(depth.timestamp_ns)
                                    .zip(known(bbo.timestamp_ns))
                                    .map(|(depth_ns, bbo_ns)| depth_ns.min(bbo_ns)),
                            ),
                            None => (
                                (bid_touch + ask_touch) / 2.0,
                                book_src,
                                known(bbo.timestamp_ns),
                            ),
                        }
                    } else {
                        (local_mid, book_src, known(bbo.timestamp_ns))
                    }
                } else {
                    (local_mid, book_src, known(bbo.timestamp_ns))
                };
                (vw_mid, false, local_mid, source, source_ts_ns)
            };

        #[cfg(feature = "research-replay")]
        let (pricing_mid, source, source_ts_ns) = self
            .recorded_local_fair_value
            .as_ref()
            .and_then(|s| s.lock().price(now_ns / 1_000_000))
            .map(|(price, source, oldest_ms)| (price, source, oldest_ms.checked_mul(1_000_000)))
            .unwrap_or((pricing_mid, source, source_ts_ns));

        // Update microstructure tracker with pricing_mid (now driven by external exchanges)
        // Per-sample signals (EMA periods, per-sample vol, toxicity) were
        // calibrated at the poll cadence; sample them at that rate only.
        let now = Instant::now();
        let allow_local_sample = using_external
            || !self.use_external_fair_value
            || !self.config.require_external_fair_value;
        if allow_local_sample
            && self.last_micro_sample.is_none_or(|last| {
                now.saturating_duration_since(last)
                    >= Duration::from_millis(self.config.poll_interval_ms)
            })
        {
            self.micro.update(pricing_mid);
            self.last_micro_sample = Some(now);
            // Mirror the tracker: only an accepted sample becomes evidence.
            if pricing_mid.is_finite() && pricing_mid > 0.0 {
                self.price_evidence = Some(PriceEvidence {
                    mid: pricing_mid,
                    source,
                    sampled_ms: now_ns / 1_000_000,
                    feed_ms: source_ts_ns.map(|ts_ns| ts_ns / 1_000_000),
                });
            }
        }
        let vol_bps = self.micro.volatility_bps();
        if allow_local_sample {
            self.sigma.update_with_gap_reset(
                pricing_mid,
                now,
                self.config.vol_gap_reset_secs.map(Duration::from_secs),
            );
        }
        // Mark against an independent fresh external fair value when active;
        // when opted in via `local_toxicity.enable_local_markout` in local mode,
        // mark against causal Stoikov touch microprice on a fresh local BBO.
        let enable_local_markout = self
            .config
            .local_toxicity
            .is_some_and(|tox| tox.enable_local_markout);
        let markout_fair = if using_external {
            pricing_mid
        } else if enable_local_markout
            && !market_state::is_stale_bbo(bbo.timestamp_ns, now_ns, self.config.local_staleness_ms)
        {
            market_state::local_touch_micro_price(bbo, 2.0).unwrap_or(f64::NAN)
        } else {
            f64::NAN
        };
        self.markout.update(&self.order_tracker, markout_fair, now);
        let pricing_mid = if self.config.self_fill_alpha_bps > 0.0
            && pricing_mid.is_finite()
            && pricing_mid > 0.0
        {
            let decayed_offset_bps = self.self_fill_alpha.observe(
                &self.order_tracker,
                now,
                self.config.self_fill_alpha_bps,
                self.config.self_fill_half_life_ms,
            );
            pricing_mid * (1.0 + decayed_offset_bps / 10_000.0).max(0.1)
        } else {
            pricing_mid
        };
        let primary_ref = self.config.primary_reference_exchange_id();
        let order_flow_bps = self.config.order_flow_reserve.map_or(0.0, |reserve| {
            let book = if using_external {
                select_exchange_bbo(exchanges, primary_ref)
            } else {
                None
            };
            self.order_flow.observe(
                book,
                now_ns,
                self.config.external_staleness_ms,
                reserve.half_life_ms,
            ) * reserve.max_widen_bps
        });
        let cross_market_bps = self.config.cross_market_reserve.map_or(0.0, |reserve| {
            let external = using_external
                .then(|| select_exchange_bbo(exchanges, primary_ref))
                .flatten();
            self.cross_market_pressure.observe(
                bbo,
                external,
                now_ns,
                [
                    self.config.local_staleness_ms,
                    self.config.external_staleness_ms,
                ],
                reserve,
            )
        });
        let volatility_pressure = self.config.volatility_regime.map_or(0.0, |regime| {
            self.volatility_regime.observe(
                select_exchange_bbo(exchanges, primary_ref).as_ref(),
                now_ns,
                self.config.external_staleness_ms,
            );
            self.volatility_regime
                .pressure(now_ns, regime.range_trigger_bps)
        });
        // Until the time-scaled estimate warms up, fall back to the per-sample
        // figure; optionally normalize per sqrt(poll_interval_secs) for cross-cadence parity (`VMM-07`).
        let cold_start_sigma = if self.config.normalize_cold_start_sigma_by_poll {
            let poll_secs = ((self.config.poll_interval_ms.max(1) as f64) / 1000.0).max(1e-3);
            (vol_bps / 10_000.0) / poll_secs.sqrt()
        } else {
            vol_bps / 10_000.0
        };
        let sigma_per_sqrt_sec = self.sigma.sigma().unwrap_or(cold_start_sigma);
        let (norm_local_ofi, local_ofi_bps) = self
            .config
            .local_toxicity
            .filter(|tox| {
                tox.ofi_half_life_ms > 0
                    && (tox.ofi_max_widen_bps > 0.0 || tox.ofi_withdraw_threshold.is_some())
            })
            .map_or((0.0, 0.0), |tox| {
                let fresh_local = (!market_state::is_stale_bbo(
                    bbo.timestamp_ns,
                    now_ns,
                    self.config.local_staleness_ms,
                ))
                .then_some(*bbo);
                let norm = self.local_order_flow.observe(
                    fresh_local,
                    now_ns,
                    self.config.local_staleness_ms,
                    tox.ofi_half_life_ms,
                );
                (norm, norm * tox.ofi_max_widen_bps)
            });
        self.last_local_ofi_norm = norm_local_ofi;
        let cancel_bump_bps = self.config.local_toxicity.map_or(0.0, |tox| {
            market_state::cancel_speed_bump_reserve_bps(sigma_per_sqrt_sec, tox)
        });
        let as_score = self.micro.adverse_selection_score();

        PricingInputs {
            mid,
            pricing_mid,
            bid_touch,
            ask_touch,
            vol_bps,
            sigma_per_sqrt_sec,
            as_score,
            using_external_fair_value: using_external,
            order_flow_bps,
            local_ofi_bps,
            cancel_bump_bps,
            cross_market_bps,
            volatility_pressure,
            #[cfg(feature = "research-replay")]
            participation_bbo: self.quote_participation.as_ref().and_then(|policy| {
                select_exchange_bbo(
                    exchanges,
                    policy.exchange_id(self.config.exchange_id, primary_ref),
                )
            }),
        }
    }

    fn competing_directional_reserves(&self, inputs: &PricingInputs) -> [f64; 2] {
        let regime = self.config.volatility_regime.map_or(0.0, |regime| {
            regime.max_widen_bps * inputs.volatility_pressure
        });
        let local_floor = inputs
            .cancel_bump_bps
            .max(regime)
            .max(self.defense_caution_widen_bps());
        let local_bid = (-inputs.local_ofi_bps).max(local_floor);
        let local_ask = inputs.local_ofi_bps.max(local_floor);
        let use_local_markout = self
            .config
            .local_toxicity
            .is_some_and(|tox| tox.enable_local_markout);
        let [bid_markout, ask_markout] = if inputs.using_external_fair_value || use_local_markout {
            self.config.markout_reserve.map_or([0.0; 2], |policy| {
                self.markout.side_reserve_bps(
                    Instant::now(),
                    self.config.maker_fee_bps
                        + policy.close_cost_bps
                        + self.config.min_profit_bps / 2.0,
                    policy.max_widen_bps,
                )
            })
        } else {
            [0.0; 2]
        };
        if !inputs.using_external_fair_value {
            return [bid_markout.max(local_bid), ask_markout.max(local_ask)];
        }
        [
            bid_markout.max(-inputs.order_flow_bps).max(local_bid),
            ask_markout.max(inputs.order_flow_bps).max(local_ask),
        ]
    }

    fn current_inventory_age_fraction(&self) -> f64 {
        if self.config.position_timeout_secs == 0 {
            return 0.0;
        }
        let half_step = (self.config.step_size * 0.5).max(components::FLOAT_EPSILON);
        let pos = self.order_tracker.confirmed_position();
        if pos.abs() < half_step || self.inventory_age.direction != pos.signum() {
            return 0.0;
        }
        self.inventory_age.opened_at.map_or(0.0, |opened| {
            (Instant::now()
                .saturating_duration_since(opened)
                .as_secs_f64()
                / (self.config.position_timeout_secs.max(1) as f64))
                .clamp(0.0, 1.0)
        })
    }

    fn current_entry_vwap_and_age_fraction(&self) -> (Option<f64>, f64) {
        let age_fraction = self.current_inventory_age_fraction();
        let half_step = (self.config.step_size * 0.5).max(components::FLOAT_EPSILON);
        let confirmed_pos = self.order_tracker.confirmed_position();
        let entry_vwap = self.episode_flat_cash_flow.and_then(|flat_cash| {
            let delta_cash = self.order_tracker.trade_cash_flow() - flat_cash;
            if confirmed_pos >= half_step {
                let vwap = (-delta_cash) / confirmed_pos;
                (vwap.is_finite() && vwap > 0.0).then_some(vwap)
            } else if confirmed_pos <= -half_step {
                let vwap = delta_cash / (-confirmed_pos);
                (vwap.is_finite() && vwap > 0.0).then_some(vwap)
            } else {
                None
            }
        });
        (entry_vwap, age_fraction)
    }

    fn passive_touch_flatten_reducing_sides(&self, q: f64, age_fraction: f64) -> [bool; 2] {
        if self.hedge.is_some() || !q.is_finite() {
            return [false, false];
        }
        let Some(start_frac) = self
            .config
            .quote_hysteresis
            .and_then(|h| h.passive_touch_flatten_age_fraction)
        else {
            return [false, false];
        };
        if age_fraction + components::FLOAT_EPSILON < start_frac {
            return [false, false];
        }
        let half_step = (self.config.step_size * 0.5).max(components::FLOAT_EPSILON);
        let [bid_closable, ask_closable] = components::closable_reducing_sizes(
            [
                self.account_stats.position,
                self.order_tracker.confirmed_position(),
            ],
            self.config.step_size,
        );
        [
            q < -half_step && bid_closable >= half_step,
            q > half_step && ask_closable >= half_step,
        ]
    }

    /// Unambiguous reducing progress (`[bid_progress, ask_progress]`) and age
    /// urgency in `[0, 1]` when unhedged inventory exceeds the deadband and
    /// neither account nor fill tracker disagrees on position direction.
    fn flattening_progress(
        &self,
        q: f64,
        mid: f64,
        urgency_ratio: f64,
        hysteresis: crate::config::QuoteHysteresisConfig,
    ) -> ([f64; 2], f64) {
        if self.hedge.is_some() || !q.is_finite() {
            return ([0.0; 2], 0.0);
        }
        let max_pos = scaled_max_position(&self.config, self.account_stats.portfolio_value, mid);
        let urgency_threshold = scaled_inventory_urgency_threshold(
            &self.config,
            self.account_stats.portfolio_value,
            mid,
            max_pos,
        );
        let deadband = if hysteresis.zero_deadband_flatten {
            (self.config.step_size * 0.5).max(components::FLOAT_EPSILON)
        } else {
            inventory_deadband_size(
                &self.config,
                scaled_base_order_size(&self.config, self.account_stats.portfolio_value, mid)
                    .max(self.config.step_size),
                urgency_threshold,
                mid,
            )
        };
        if q.abs() <= deadband {
            return ([0.0; 2], 0.0);
        }
        let account_pos = self.account_stats.position;
        let confirmed_pos = self.order_tracker.confirmed_position();
        let step = self.config.step_size;
        let age_urgency = if hysteresis.flatten_age_start_fraction < 1.0
            && self.inventory_age.direction == q.signum()
            && let Some(opened) = self.inventory_age.opened_at
        {
            let age_frac = (Instant::now()
                .saturating_duration_since(opened)
                .as_secs_f64()
                / (self.config.position_timeout_secs.max(1) as f64))
                .clamp(0.0, 1.0);
            if age_frac > hysteresis.flatten_age_start_fraction {
                ((age_frac - hysteresis.flatten_age_start_fraction)
                    / (1.0 - hysteresis.flatten_age_start_fraction))
                    .clamp(0.0, 1.0)
            } else {
                0.0
            }
        } else {
            0.0
        };
        let size_progress = if hysteresis.zero_deadband_flatten {
            (q.abs() / urgency_threshold.max(step)).clamp(0.0, 1.0)
        } else {
            urgency_ratio.abs().clamp(0.0, 1.0)
        };
        let progress = size_progress
            .max(urgency_ratio.abs().clamp(0.0, 1.0))
            .max(age_urgency);
        if q > deadband && account_pos > -step && confirmed_pos > -step {
            ([0.0, progress], age_urgency)
        } else if q < -deadband && account_pos < step && confirmed_pos < step {
            ([progress, 0.0], age_urgency)
        } else {
            ([0.0; 2], 0.0)
        }
    }

    /// Avellaneda-Stoikov pricing logic (v6.0.0)
    ///
    /// Uses external fair value as pricing_mid. Kappa is used directly from config
    /// (no fill-rate scaling). Spread cap is dynamic vol-based instead of hard clamp.
    /// Momentum-aware asymmetric spread.
    fn calculate_optimal_quotes(
        &self,
        inputs: &PricingInputs,
        q: f64,
    ) -> Option<(f64, f64, MakerPriceBounds)> {
        let (mut sell_proceeds, mut buy_cost) = if let Some(hedge) = &self.hedge {
            let allowance = hedge.allowance()?;
            (
                inputs.pricing_mid.min(allowance.sell_proceeds),
                inputs.pricing_mid.max(allowance.buy_cost),
            )
        } else {
            (inputs.pricing_mid, inputs.pricing_mid)
        };
        let markout_mult = if self.config.use_markout_spread {
            self.markout.spread_multiplier()
        } else {
            1.0
        };
        let toxicity_spread_mult =
            toxicity_spread_multiplier(inputs.as_score, self.config.adverse_selection_threshold)
                .max(markout_mult);
        if toxicity_spread_mult > 1.0 {
            debug!(
                "AS soft filter active: score={:.2} spread_mult={:.2}",
                inputs.as_score, toxicity_spread_mult
            );
        }

        let gamma = self.config.as_gamma;
        let time_horizon = self.config.as_time_horizon_sec;
        // sigma^2 * T must share time units: sigma is per sqrt(second) and T is
        // in seconds. vol_bps (per sample) still drives the empirical caps.
        let sigma = inputs.sigma_per_sqrt_sec;

        // Reservation price: external fair value shifted by inventory risk plus
        // an explicit urgency skew to bias quotes toward flattening.
        let scaled_max_pos =
            scaled_max_position(&self.config, self.account_stats.portfolio_value, inputs.mid);
        let mut runtime_config = self.config.runtime_copy();
        runtime_config.inventory_urgency_threshold = scaled_inventory_urgency_threshold(
            &self.config,
            self.account_stats.portfolio_value,
            inputs.mid,
            scaled_max_pos,
        );
        let urgency_ratio = inventory_skew_ratio(&runtime_config, q);
        let inventory_skew = self.config.inventory_skew_bps * urgency_ratio / 10000.0;
        let q_for_as = self
            .config
            .as_inventory_notional_usd
            .map_or(q, |unit_usd| (q * inputs.pricing_mid) / unit_usd.max(1.0));
        let gamma_inv = if let Some(budget_bps) = self.config.as_risk_budget_bps_at_max_pos {
            let max_pos_for_as = self
                .config
                .as_inventory_notional_usd
                .map_or(scaled_max_pos, |unit_usd| {
                    (scaled_max_pos * inputs.pricing_mid) / unit_usd.max(1.0)
                })
                .max(self.config.step_size);
            let denom = (sigma * sigma * time_horizon * max_pos_for_as).max(1e-12);
            ((budget_bps / 10_000.0) / denom).clamp(1e-6, 1e9)
        } else {
            gamma
        };
        let reservation_price = inputs.pricing_mid
            * (1.0 - gamma_inv * sigma * sigma * q_for_as * time_horizon - inventory_skew);

        let hysteresis = self
            .config
            .quote_hysteresis
            .filter(|_| self.hedge.is_none());
        let ([bid_flatten, ask_flatten], age_urgency) = hysteresis
            .map(|policy| self.flattening_progress(q, inputs.mid, urgency_ratio, policy))
            .unwrap_or(([0.0; 2], 0.0));
        let (bid_profit_bps, ask_profit_bps) = if let Some(policy) = hysteresis {
            let span = (self.config.min_profit_bps - policy.flatten_min_profit_bps).max(0.0);
            (
                self.config.min_profit_bps - bid_flatten * span,
                self.config.min_profit_bps - ask_flatten * span,
            )
        } else {
            (self.config.min_profit_bps, self.config.min_profit_bps)
        };
        if hysteresis.is_some_and(|policy| policy.conservative_dual_anchor)
            && inputs.using_external_fair_value
            && inputs.mid.is_finite()
            && inputs.mid > 0.0
        {
            if bid_flatten == 0.0 {
                sell_proceeds = sell_proceeds.min(inputs.mid);
            }
            if ask_flatten == 0.0 {
                buy_cost = buy_cost.max(inputs.mid);
            }
        }
        let enable_local_markout = self
            .config
            .local_toxicity
            .is_some_and(|tox| tox.enable_local_markout);
        let [bid_quantile_loss_bps, ask_quantile_loss_bps] = if (inputs.using_external_fair_value
            || enable_local_markout)
            && let Some(policy) = self.config.markout_reserve
            && let Some(p) = policy.quantile_floor_p
        {
            let [b, a] = self.markout.adverse_quantile_loss_bps(
                Instant::now(),
                p,
                Some(Duration::from_millis(policy.max_sample_age_ms)),
            );
            [
                if q >= 0.0 && bid_flatten == 0.0 {
                    b.min(policy.max_widen_bps)
                } else {
                    0.0
                },
                if q <= 0.0 && ask_flatten == 0.0 {
                    a.min(policy.max_widen_bps)
                } else {
                    0.0
                },
            ]
        } else {
            [0.0; 2]
        };
        let bounds_cap_round_trip =
            (self.config.max_spread_bps - 2.0 * self.config.maker_fee_bps).max(0.0);
        let bounds_bid_profit_bps = if bid_quantile_loss_bps > 0.0 {
            (bid_profit_bps + 2.0 * bid_quantile_loss_bps)
                .min(bounds_cap_round_trip.max(bid_profit_bps))
        } else {
            bid_profit_bps
        };
        let bounds_ask_profit_bps = if ask_quantile_loss_bps > 0.0 {
            (ask_profit_bps + 2.0 * ask_quantile_loss_bps)
                .min(bounds_cap_round_trip.max(ask_profit_bps))
        } else {
            ask_profit_bps
        };
        let placement_bounds = MakerPriceBounds::new_asymmetric(
            sell_proceeds,
            buy_cost,
            self.config.maker_fee_bps,
            bounds_bid_profit_bps,
            bounds_ask_profit_bps,
            self.config.tick_size,
        )?;
        let cancel_bounds = if let Some(policy) = hysteresis {
            MakerPriceBounds::new_asymmetric(
                sell_proceeds,
                buy_cost,
                self.config.maker_fee_bps,
                policy.cancel_min_profit_bps.min(bid_profit_bps),
                policy.cancel_min_profit_bps.min(ask_profit_bps),
                self.config.tick_size,
            )?
        } else {
            placement_bounds
        };

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
        let (mut bid_half_spread, mut ask_half_spread) = momentum_half_spreads(
            half_spread,
            self.micro.momentum_bps(),
            self.config.momentum_skew_scale,
        );
        if bid_flatten > 0.0 {
            let floor_bps = (2.0 * self.config.maker_fee_bps + bid_profit_bps).max(0.0);
            let reducing_base = fee_aware_half_spread(
                half_spread_raw * toxicity_spread_mult,
                inputs.pricing_mid,
                floor_bps,
                vol_cap_bps.max(floor_bps),
            )?;
            let (mut reducing_bid, _) = momentum_half_spreads(
                reducing_base,
                self.micro.momentum_bps(),
                self.config.momentum_skew_scale,
            );
            let min_half = inputs.pricing_mid * floor_bps / 20_000.0;
            reducing_bid -= age_urgency * (reducing_bid - min_half).max(0.0);
            bid_half_spread = bid_half_spread.min(reducing_bid);
        }
        if ask_flatten > 0.0 {
            let floor_bps = (2.0 * self.config.maker_fee_bps + ask_profit_bps).max(0.0);
            let reducing_base = fee_aware_half_spread(
                half_spread_raw * toxicity_spread_mult,
                inputs.pricing_mid,
                floor_bps,
                vol_cap_bps.max(floor_bps),
            )?;
            let (_, mut reducing_ask) = momentum_half_spreads(
                reducing_base,
                self.micro.momentum_bps(),
                self.config.momentum_skew_scale,
            );
            let min_half = inputs.pricing_mid * floor_bps / 20_000.0;
            reducing_ask -= age_urgency * (reducing_ask - min_half).max(0.0);
            ask_half_spread = ask_half_spread.min(reducing_ask);
        }
        if bid_quantile_loss_bps > 0.0 {
            let floor_half_bps =
                (self.config.maker_fee_bps + bid_quantile_loss_bps + 0.5 * bid_profit_bps)
                    .min(0.5 * self.config.max_spread_bps);
            bid_half_spread = bid_half_spread.max(inputs.pricing_mid * floor_half_bps / 10_000.0);
        }
        if ask_quantile_loss_bps > 0.0 {
            let floor_half_bps =
                (self.config.maker_fee_bps + ask_quantile_loss_bps + 0.5 * ask_profit_bps)
                    .min(0.5 * self.config.max_spread_bps);
            ask_half_spread = ask_half_spread.max(inputs.pricing_mid * floor_half_bps / 10_000.0);
        }
        if let Some(policy) = hysteresis
            && let Some(be_bps) = policy.breakeven_exit_bps
            && policy.breakeven_hold_fraction > 0.0
            && let Some(flat_cash) = self.episode_flat_cash_flow
        {
            let age_frac = if self.inventory_age.direction == q.signum()
                && let Some(opened) = self.inventory_age.opened_at
            {
                (Instant::now()
                    .saturating_duration_since(opened)
                    .as_secs_f64()
                    / (self.config.position_timeout_secs.max(1) as f64))
                    .clamp(0.0, 1.0)
            } else {
                0.0
            };
            let hold_weight = if age_frac <= policy.breakeven_hold_fraction {
                1.0
            } else {
                ((1.0 - age_frac) / (1.0 - policy.breakeven_hold_fraction).max(1e-6))
                    .clamp(0.0, 1.0)
            };
            if hold_weight > 0.0 {
                let confirmed_pos = self.order_tracker.confirmed_position();
                let delta_cash = self.order_tracker.trade_cash_flow() - flat_cash;
                let max_half_frac = 0.5 * self.config.max_spread_bps / 10_000.0;
                if ask_flatten > 0.0 && confirmed_pos >= self.config.step_size {
                    let fee_factor = (1.0 - self.config.maker_fee_bps / 10_000.0).max(0.5);
                    let be_ask = ((-delta_cash)
                        + confirmed_pos * inputs.pricing_mid * (be_bps / 10_000.0))
                        / (confirmed_pos * fee_factor);
                    if be_ask.is_finite() {
                        let clamped_ask = be_ask.clamp(
                            inputs.pricing_mid,
                            inputs.pricing_mid * (1.0 + max_half_frac),
                        );
                        let base_ask = reservation_price + ask_half_spread;
                        let target_ask = base_ask + hold_weight * (clamped_ask - base_ask).max(0.0);
                        ask_half_spread = (target_ask - reservation_price).max(ask_half_spread);
                    }
                } else if bid_flatten > 0.0 && confirmed_pos <= -self.config.step_size {
                    let abs_pos = -confirmed_pos;
                    let fee_factor = (1.0 + self.config.maker_fee_bps / 10_000.0).max(0.5);
                    let be_bid = (delta_cash - abs_pos * inputs.pricing_mid * (be_bps / 10_000.0))
                        / (abs_pos * fee_factor);
                    if be_bid.is_finite() {
                        let clamped_bid = be_bid.clamp(
                            inputs.pricing_mid * (1.0 - max_half_frac).max(0.1),
                            inputs.pricing_mid,
                        );
                        let base_bid = reservation_price - bid_half_spread;
                        let target_bid = base_bid - hold_weight * (base_bid - clamped_bid).max(0.0);
                        bid_half_spread = (reservation_price - target_bid).max(bid_half_spread);
                    }
                }
            }
        }
        let regime_reserve = self.config.volatility_regime.map_or(0.0, |regime| {
            regime.max_widen_bps * inputs.volatility_pressure
        });
        if regime_reserve > 0.0
            || self.defense_caution_widen_bps() > 0.0
            || inputs.local_ofi_bps != 0.0
            || inputs.cancel_bump_bps > 0.0
            || (enable_local_markout && self.config.markout_reserve.is_some())
            || (inputs.using_external_fair_value
                && (self.config.markout_reserve.is_some()
                    || inputs.order_flow_bps != 0.0
                    || inputs.cross_market_bps != 0.0))
        {
            let [bid_competing, ask_competing] = self.competing_directional_reserves(inputs);
            // The reserves overlap. Use the largest estimate on each side.
            let bid_reserve = bid_competing.max(-inputs.cross_market_bps);
            let ask_reserve = ask_competing.max(inputs.cross_market_bps);
            // Keep the side pointing toward flat at its original price.
            let bid_reserve = if q >= 0.0 { bid_reserve } else { 0.0 };
            let ask_reserve = if q <= 0.0 { ask_reserve } else { 0.0 };
            let requested = bid_reserve + ask_reserve;
            if requested > 0.0 {
                let bps_price = inputs.pricing_mid / 10_000.0;
                let room = (self.config.max_spread_bps
                    - (bid_half_spread + ask_half_spread) / bps_price)
                    .max(0.0);
                let scale = bps_price * (room / requested).min(1.0);
                bid_half_spread += bid_reserve * scale;
                ask_half_spread += ask_reserve * scale;
            }
        }

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
                let mut bid = stable_bid.min(placement_bounds.max_bid);
                let mut ask = stable_ask.max(placement_bounds.min_ask);
                let mut effective_cancel_bounds = cancel_bounds;
                if let Some(start_frac) =
                    hysteresis.and_then(|policy| policy.passive_touch_flatten_age_fraction)
                {
                    let age_f = self.current_inventory_age_fraction();
                    let passive_sides = self.passive_touch_flatten_reducing_sides(q, age_f);
                    let touch_progress =
                        ((age_f - start_frac) / (1.0 - start_frac).max(1e-6)).clamp(0.0, 1.0);
                    if touch_progress > 0.0 && self.config.tick_size > 0.0 {
                        let tick = self.config.tick_size;
                        let floor_tick =
                            |px: f64| ((px / tick + components::FLOAT_EPSILON).floor()) * tick;
                        let ceil_tick =
                            |px: f64| ((px / tick - components::FLOAT_EPSILON).ceil()) * tick;
                        if passive_sides[0] {
                            let touch_bid = floor_tick(inputs.bid_touch)
                                .min(floor_tick(inputs.ask_touch - tick));
                            let blended_bid =
                                floor_tick(bid + touch_progress * (touch_bid - bid).max(0.0))
                                    .min(floor_tick(inputs.ask_touch - tick));
                            if blended_bid.is_finite() && blended_bid > bid && blended_bid < ask {
                                bid = blended_bid;
                                effective_cancel_bounds.max_bid =
                                    effective_cancel_bounds.max_bid.max(blended_bid);
                            }
                        }
                        if passive_sides[1] {
                            let touch_ask =
                                ceil_tick(inputs.ask_touch).max(ceil_tick(inputs.bid_touch + tick));
                            let blended_ask =
                                ceil_tick(ask - touch_progress * (ask - touch_ask).max(0.0))
                                    .max(ceil_tick(inputs.bid_touch + tick));
                            if blended_ask.is_finite() && blended_ask < ask && blended_ask > bid {
                                ask = blended_ask;
                                effective_cancel_bounds.min_ask =
                                    effective_cancel_bounds.min_ask.min(blended_ask);
                            }
                        }
                    }
                }
                Some((bid, ask, effective_cancel_bounds))
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
        mut bounds: MakerPriceBounds,
        inputs: &PricingInputs,
        shutdown: Option<&tokio::sync::watch::Receiver<bool>>,
        #[cfg(feature = "research-replay")] changed_sides: [bool; 2],
    ) {
        let mid = inputs.mid;
        self.reconcile_after_new_fill_if_needed().await;
        if shutdown_requested(shutdown) {
            return;
        }
        self.refresh_active_orders_from_tracker();
        let positions = [
            self.account_stats.position,
            self.order_tracker.confirmed_position(),
        ];
        #[cfg_attr(not(feature = "research-replay"), allow(unused_variables))]
        let (entry_vwap, age_fraction) = self.current_entry_vwap_and_age_fraction();
        let passive_touch_sides = self.passive_touch_flatten_reducing_sides(
            position_for_quoting(&self.config, positions[0], positions[1]),
            age_fraction,
        );
        #[cfg(feature = "research-replay")]
        let (local_touch_sides, local_exit_sides) =
            self.local_participation
                .as_ref()
                .map_or(([false, false], [false, false]), |policy| {
                    (
                        policy.active_touch_sides(positions, self.config.step_size),
                        policy.is_reducing_exit_active(positions, self.config.step_size),
                    )
                });
        #[cfg(not(feature = "research-replay"))]
        let (local_touch_sides, local_exit_sides) = ([false, false], [false, false]);
        if self.config.tick_size > 0.0
            && inputs.bid_touch > 0.0
            && inputs.ask_touch > inputs.bid_touch
        {
            let tick = self.config.tick_size;
            if local_touch_sides[0] || passive_touch_sides[0] {
                let one_tick_below_ask =
                    ((inputs.ask_touch - tick) / tick + components::FLOAT_EPSILON).floor() * tick;
                if one_tick_below_ask.is_finite() && one_tick_below_ask > 0.0 {
                    bounds.max_bid = bounds.max_bid.max(one_tick_below_ask);
                }
            }
            if local_touch_sides[1] || passive_touch_sides[1] {
                let one_tick_above_bid =
                    ((inputs.bid_touch + tick) / tick - components::FLOAT_EPSILON).ceil() * tick;
                if one_tick_above_bid.is_finite() {
                    bounds.min_ask = bounds.min_ask.min(one_tick_above_bid);
                }
            }
        }
        let breaching_any = self
            .active_orders
            .iter()
            .any(|order| !bounds.allows(order.side, order.price));
        let per_side_bound_cancel = self.hedge.is_none()
            && self
                .config
                .quote_hysteresis
                .is_some_and(|h| h.per_side_bound_cancel)
            && self
                .active_orders
                .iter()
                .all(|order| bounds.allows(order.side, order.price) || order.order_index.is_some());
        if breaching_any && !per_side_bound_cancel {
            // A requote tolerance must never retain a price below the
            // current edge floor, even during the minimum order lifetime.
            self.cancel_all_and_sync("maker edge below floor").await;
            return;
        }

        let risk = self.build_risk_snapshot(mid);
        let Some(target) =
            self.build_quote_target(our_bid, our_ask, mid, &risk, inputs.volatility_pressure)
        else {
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
        let mut runtime_config = self.config.runtime_copy();
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
        #[cfg(feature = "research-replay")]
        let plan = if let Some(policy) = &mut self.quote_participation {
            let mut plan = plan;
            policy.apply(
                &mut plan.target,
                inputs.participation_bbo,
                [
                    self.account_stats.position,
                    self.order_tracker.confirmed_position(),
                ],
                &runtime_config,
            );
            plan
        } else {
            plan
        };

        #[cfg(feature = "research-replay")]
        let plan = if let Some(policy) = &mut self.local_participation {
            let mut plan = plan;
            policy.apply_with_context(
                &mut plan.target,
                positions,
                &runtime_config,
                Some(replay::recorded::local_signal::LocalBookContext {
                    bid_touch: inputs.bid_touch,
                    ask_touch: inputs.ask_touch,
                    pricing_mid: inputs.pricing_mid,
                    entry_vwap,
                    age_fraction,
                }),
            );
            plan
        } else {
            plan
        };

        #[cfg(feature = "research-replay")]
        let mut plan = if let Some(policy) = &mut self.binance_participation {
            let mut plan = plan;
            policy.apply(
                &mut plan.target,
                [
                    self.account_stats.position,
                    self.order_tracker.confirmed_position(),
                ],
                &runtime_config,
            );
            plan
        } else {
            plan
        };
        #[cfg(not(feature = "research-replay"))]
        let mut plan = plan;

        let closable = components::closable_reducing_sizes(positions, self.config.step_size);
        if passive_touch_sides[0] {
            let capped = components::round_down_to_step(
                plan.target.bid_size.min(closable[0]),
                runtime_config.step_size,
            );
            plan.target.bid_size = if capped + components::FLOAT_EPSILON
                < components::min_quotable_size(&runtime_config, plan.target.bid_price)
            {
                0.0
            } else {
                capped
            };
        }
        if passive_touch_sides[1] {
            let capped = components::round_down_to_step(
                plan.target.ask_size.min(closable[1]),
                runtime_config.step_size,
            );
            plan.target.ask_size = if capped + components::FLOAT_EPSILON
                < components::min_quotable_size(&runtime_config, plan.target.ask_price)
            {
                0.0
            } else {
                capped
            };
        }
        let hysteresis = self
            .config
            .quote_hysteresis
            .filter(|_| self.hedge.is_none());
        let half_step = (self.config.step_size * 0.5).max(components::FLOAT_EPSILON);
        let enforce_ro =
            hysteresis.is_some_and(|policy| policy.enforce_reduce_only_on_reducing_side);
        let bid_reducing_cap = (local_exit_sides[0]
            || passive_touch_sides[0]
            || (enforce_ro && closable[0] >= half_step))
            .then_some(closable[0]);
        let ask_reducing_cap = (local_exit_sides[1]
            || passive_touch_sides[1]
            || (enforce_ro && closable[1] >= half_step))
            .then_some(closable[1]);

        let max_side_replacements = max_side_requote_replacements_per_cycle(&inv_ctx);
        let size_tolerance_ratio = size_tolerance_ratio_for_requote(&inv_ctx);
        let ([bid_flatten, ask_flatten], _) = hysteresis
            .map(|policy| {
                let urgency_ratio =
                    inventory_skew_ratio(&runtime_config, risk.position_for_quoting);
                self.flattening_progress(risk.position_for_quoting, mid, urgency_ratio, policy)
            })
            .unwrap_or(([0.0; 2], 0.0));
        let fast_reducing_enabled = hysteresis.is_some_and(|policy| {
            policy.reducing_order_ttl_secs.is_some()
                || policy.reducing_requote_threshold_bps.is_some()
                || policy.zero_deadband_flatten
                || policy.passive_touch_flatten_age_fraction.is_some()
        });
        let bid_fast_reducing = (fast_reducing_enabled && bid_flatten > 0.0)
            || local_exit_sides[0]
            || passive_touch_sides[0];
        let ask_fast_reducing = (fast_reducing_enabled && ask_flatten > 0.0)
            || local_exit_sides[1]
            || passive_touch_sides[1];
        let reducing_ttl = hysteresis
            .and_then(|policy| policy.reducing_order_ttl_secs)
            .map(|secs| Duration::from_secs(secs.max(1)))
            .or_else(|| {
                (local_exit_sides[0]
                    || local_exit_sides[1]
                    || passive_touch_sides[0]
                    || passive_touch_sides[1])
                    .then_some(Duration::from_secs(1))
            });
        let reducing_threshold = hysteresis
            .and_then(|policy| policy.reducing_requote_threshold_bps)
            .map(|bps| mid * bps / 10_000.0);
        let entry_threshold = hysteresis
            .and_then(|policy| policy.entry_requote_threshold_bps)
            .map(|bps| mid * bps / 10_000.0);
        let bid_threshold = if bid_fast_reducing {
            reducing_threshold.unwrap_or_else(|| {
                if local_exit_sides[0] || passive_touch_sides[0] {
                    self.config.tick_size
                } else {
                    plan.requote_threshold
                }
            })
        } else if risk.position_for_quoting > -self.config.step_size {
            entry_threshold.unwrap_or(plan.requote_threshold)
        } else {
            plan.requote_threshold
        };
        let ask_threshold = if ask_fast_reducing {
            reducing_threshold.unwrap_or_else(|| {
                if local_exit_sides[1] || passive_touch_sides[1] {
                    self.config.tick_size
                } else {
                    plan.requote_threshold
                }
            })
        } else if risk.position_for_quoting < self.config.step_size {
            entry_threshold.unwrap_or(plan.requote_threshold)
        } else {
            plan.requote_threshold
        };

        let adverse_retreat_threshold = hysteresis
            .and_then(|policy| policy.adverse_retreat_threshold_bps)
            .map(|bps| mid * bps / 10_000.0)
            .unwrap_or(plan.requote_threshold);
        let ratchet_enabled = hysteresis.is_some_and(|policy| policy.asymmetric_entry_ratchet);
        let bid_ratchet = (ratchet_enabled
            && !bid_fast_reducing
            && risk.position_for_quoting > -self.config.step_size)
            .then_some(EntryRatchetPolicy {
                favorable_hold_threshold: entry_threshold.unwrap_or(plan.requote_threshold),
                adverse_retreat_threshold,
            });
        let ask_ratchet = (ratchet_enabled
            && !ask_fast_reducing
            && risk.position_for_quoting < self.config.step_size)
            .then_some(EntryRatchetPolicy {
                favorable_hold_threshold: entry_threshold.unwrap_or(plan.requote_threshold),
                adverse_retreat_threshold,
            });

        let side_cancel_bounds = per_side_bound_cancel.then_some(bounds);
        let now_cooldown = Instant::now();
        let cooldown_ms = hysteresis.map_or(0, |policy| policy.adverse_retreat_reentry_cooldown_ms);
        let hold_safe_during_cooldown =
            hysteresis.is_some_and(|policy| policy.cooldown_hold_safe_levels);
        let entry_max_places =
            hysteresis.and_then(|policy| policy.max_initial_placements_per_cycle);
        let staged_reentry_only = hysteresis.is_some_and(|policy| policy.staged_reentry_only);
        let isolate_cooldown_cancel_churn =
            hysteresis.is_some_and(|policy| policy.isolate_cooldown_cancel_churn);
        let bid_empty = !self
            .active_orders
            .iter()
            .any(|order| order.side == OrderSide::Buy);
        let ask_empty = !self
            .active_orders
            .iter()
            .any(|order| order.side == OrderSide::Sell);
        let bid_max_places = if staged_reentry_only {
            (!bid_fast_reducing && bid_empty)
                .then_some(entry_max_places)
                .flatten()
        } else {
            (!bid_fast_reducing).then_some(entry_max_places).flatten()
        };
        let ask_max_places = if staged_reentry_only {
            (!ask_fast_reducing && ask_empty)
                .then_some(entry_max_places)
                .flatten()
        } else {
            (!ask_fast_reducing).then_some(entry_max_places).flatten()
        };
        let bid_reducing_now = risk.position_for_quoting <= -self.config.step_size;
        let ask_reducing_now = risk.position_for_quoting >= self.config.step_size;
        let bid_in_cooldown = cooldown_ms > 0
            && !bid_reducing_now
            && self.adverse_retreat_cooldown_until[0].is_some_and(|until| now_cooldown < until);
        let ask_in_cooldown = cooldown_ms > 0
            && !ask_reducing_now
            && self.adverse_retreat_cooldown_until[1].is_some_and(|until| now_cooldown < until);
        let mut bid_plan = build_side_execution_plan_full(
            &runtime_config,
            &self.active_orders,
            self.trading.limit_order_type(),
            Side::Buy,
            plan.target.bid_price,
            plan.target.bid_size,
            bid_threshold,
            size_tolerance_ratio,
            if bid_fast_reducing {
                URGENT_SIDE_REQUOTE_REPLACEMENTS_PER_CYCLE
            } else {
                max_side_replacements
            },
            side_cancel_bounds,
            bid_fast_reducing.then_some(reducing_ttl).flatten(),
            bid_fast_reducing,
            bid_ratchet,
            bid_reducing_cap,
            bid_in_cooldown && hold_safe_during_cooldown,
            bid_max_places,
        );
        let mut ask_plan = build_side_execution_plan_full(
            &runtime_config,
            &self.active_orders,
            self.trading.limit_order_type(),
            Side::Sell,
            plan.target.ask_price,
            plan.target.ask_size,
            ask_threshold,
            size_tolerance_ratio,
            if ask_fast_reducing {
                URGENT_SIDE_REQUOTE_REPLACEMENTS_PER_CYCLE
            } else {
                max_side_replacements
            },
            side_cancel_bounds,
            ask_fast_reducing.then_some(reducing_ttl).flatten(),
            ask_fast_reducing,
            ask_ratchet,
            ask_reducing_cap,
            ask_in_cooldown && hold_safe_during_cooldown,
            ask_max_places,
        );
        let bid_cancels_passive = isolate_cooldown_cancel_churn
            && cancels_are_strictly_passive(
                &self.active_orders,
                Side::Buy,
                &bid_plan.to_cancel,
                plan.target.bid_price,
            );
        let ask_cancels_passive = isolate_cooldown_cancel_churn
            && cancels_are_strictly_passive(
                &self.active_orders,
                Side::Sell,
                &ask_plan.to_cancel,
                plan.target.ask_price,
            );
        if cooldown_ms > 0 {
            let cooldown_dur = Duration::from_millis(cooldown_ms);
            if bid_plan.had_adverse_retreat && !bid_reducing_now {
                self.adverse_retreat_cooldown_until[0] = Some(now_cooldown + cooldown_dur);
                bid_plan.to_place.clear();
            } else if bid_in_cooldown {
                bid_plan.to_place.clear();
            }
            if ask_plan.had_adverse_retreat && !ask_reducing_now {
                self.adverse_retreat_cooldown_until[1] = Some(now_cooldown + cooldown_dur);
                ask_plan.to_place.clear();
            } else if ask_in_cooldown {
                ask_plan.to_place.clear();
            }
        }

        let reducing_activity = (bid_fast_reducing
            && (!bid_plan.to_cancel.is_empty() || !bid_plan.to_place.is_empty()))
            || (ask_fast_reducing
                && (!ask_plan.to_cancel.is_empty() || !ask_plan.to_place.is_empty()));
        let ro_protection_cancel = enforce_ro
            && ((bid_reducing_cap.is_some() && !bid_plan.to_cancel.is_empty())
                || (ask_reducing_cap.is_some() && !ask_plan.to_cancel.is_empty()));
        let shared_risk_reducing = (breaching_any && !per_side_bound_cancel)
            || reducing_activity
            || ro_protection_cancel
            || plan.target.bid_size < components::min_quotable_size(&runtime_config, our_bid)
            || plan.target.ask_size < components::min_quotable_size(&runtime_config, our_ask);
        let risk_reducing = breaching_any
            || shared_risk_reducing
            || bid_plan.had_adverse_retreat
            || ask_plan.had_adverse_retreat;
        let resting_orders_raw = self.resting_order_count();
        let bid_fill_in =
            staged_reentry_only && bid_plan.is_pure_grid_fill_in && !bid_plan.to_place.is_empty();
        let ask_fill_in =
            staged_reentry_only && ask_plan.is_pure_grid_fill_in && !ask_plan.to_place.is_empty();
        if bid_fill_in
            && runtime_config.tick_size > 0.0
            && let Some(min_resting_bid) = self
                .active_orders
                .iter()
                .filter(|o| o.side == OrderSide::Buy)
                .map(|o| o.price)
                .reduce(f64::min)
        {
            let tick = runtime_config.tick_size;
            let spacing = (runtime_config.grid_spacing_bps / 10_000.0 * min_resting_bid).max(tick);
            for (idx, order) in bid_plan.to_place.iter_mut().enumerate() {
                let cap_px = ((min_resting_bid - spacing * (idx + 1) as f64) / tick
                    + components::FLOAT_EPSILON)
                    .floor()
                    * tick;
                if cap_px.is_finite() && cap_px > 0.0 {
                    order.price = order.price.min(cap_px);
                }
            }
            bid_plan.to_place.retain(|order| {
                order.price.is_finite()
                    && order.price > 0.0
                    && order.size + components::FLOAT_EPSILON
                        >= components::min_quotable_size(&runtime_config, order.price)
            });
        }
        if ask_fill_in
            && runtime_config.tick_size > 0.0
            && let Some(max_resting_ask) = self
                .active_orders
                .iter()
                .filter(|o| o.side == OrderSide::Sell)
                .map(|o| o.price)
                .reduce(f64::max)
        {
            let tick = runtime_config.tick_size;
            let spacing = (runtime_config.grid_spacing_bps / 10_000.0 * max_resting_ask).max(tick);
            for (idx, order) in ask_plan.to_place.iter_mut().enumerate() {
                let floor_px = ((max_resting_ask + spacing * (idx + 1) as f64) / tick
                    - components::FLOAT_EPSILON)
                    .ceil()
                    * tick;
                if floor_px.is_finite() && floor_px > 0.0 {
                    order.price = order.price.max(floor_px);
                }
            }
            ask_plan.to_place.retain(|order| {
                order.price.is_finite()
                    && order.price > 0.0
                    && order.size + components::FLOAT_EPSILON
                        >= components::min_quotable_size(&runtime_config, order.price)
            });
        }
        let bid_fill_in = bid_fill_in && !bid_plan.to_place.is_empty();
        let ask_fill_in = ask_fill_in && !ask_plan.to_place.is_empty();
        let bid_pure_reentry = isolate_cooldown_cancel_churn
            && bid_plan.to_cancel.is_empty()
            && !bid_plan.to_place.is_empty();
        let ask_pure_reentry = isolate_cooldown_cancel_churn
            && ask_plan.to_cancel.is_empty()
            && !ask_plan.to_place.is_empty();
        let has_safety_cancel = |other: &SideExecutionPlan| {
            other.cancel_reasons.iter().any(|(_, reason)| {
                matches!(
                    reason,
                    CancelReason::BoundBreach
                        | CancelReason::AdverseRetreat
                        | CancelReason::ReducingCapOrNonRo
                        | CancelReason::SideWithdrawn
                )
            })
        };
        if isolate_cooldown_cancel_churn && !shared_risk_reducing {
            // During an active re-entry cooldown (where `to_place` is suppressed), do not flush
            // cancel-only routine requotes on surviving levels ONLY when every canceled order is
            // strictly passive relative to the new target (`S13-1`). Adversely drifted surviving
            // levels MUST cancel immediately rather than sitting un-cancelled into a trend.
            if bid_in_cooldown
                && bid_cancels_passive
                && bid_plan.to_place.is_empty()
                && !bid_plan.to_cancel.is_empty()
                && !has_safety_cancel(&bid_plan)
            {
                bid_plan = SideExecutionPlan::empty();
            }
            if ask_in_cooldown
                && ask_cancels_passive
                && ask_plan.to_place.is_empty()
                && !ask_plan.to_cancel.is_empty()
                && !has_safety_cancel(&ask_plan)
            {
                ask_plan = SideExecutionPlan::empty();
            }
        }
        let partner_side_absent_for_cooldown_or_reentry =
            bid_in_cooldown || ask_in_cooldown || bid_pure_reentry || ask_pure_reentry;
        if (staged_reentry_only && !risk_reducing)
            || (isolate_cooldown_cancel_churn && !shared_risk_reducing)
        {
            let now_defer = Instant::now();
            let empty_plan = SideExecutionPlan::empty();
            let calm_deadband = components::inventory_deadband_size(
                inv_ctx.config,
                inv_ctx.base_order_size,
                inv_ctx
                    .inventory_urgency_threshold
                    .max(inv_ctx.config.step_size),
                inv_ctx.mid,
            );
            let within_calm_defer_inv = inv_ctx.position_for_quoting.abs()
                <= (calm_deadband * 1.5).max(inv_ctx.config.step_size).min(
                    inv_ctx
                        .inventory_urgency_threshold
                        .max(inv_ctx.config.step_size),
                );
            let opp_would_defer =
                |other: &SideExecutionPlan, partner_retreating: bool, cancels_passive: bool| {
                    let resting = effective_resting_orders_for_micro_refresh(
                        resting_orders_raw,
                        runtime_config.grid_levels,
                        isolate_cooldown_cancel_churn,
                        partner_side_absent_for_cooldown_or_reentry,
                        !other.to_cancel.is_empty() && cancels_passive,
                    );
                    !has_safety_cancel(other)
                        && (should_defer_one_sided_requote(&inv_ctx, &empty_plan, other)
                            || should_defer_cancel_only_refresh(&inv_ctx, &empty_plan, other)
                            || should_defer_micro_refresh(
                                &inv_ctx,
                                &empty_plan,
                                other,
                                self.last_execution_batch_at,
                                now_defer,
                                resting,
                            )
                            || (isolate_cooldown_cancel_churn
                                && cancels_passive
                                && partner_retreating
                                && within_calm_defer_inv
                                && other.to_cancel.len() <= 1
                                && other.to_place.len() <= 1))
                };
            let bid_side_retreat_or_bound = bid_plan.had_adverse_retreat
                || (per_side_bound_cancel && has_safety_cancel(&bid_plan));
            let ask_side_retreat_or_bound = ask_plan.had_adverse_retreat
                || (per_side_bound_cancel && has_safety_cancel(&ask_plan));
            let isolate_ask = (staged_reentry_only && !risk_reducing && bid_fill_in)
                || (isolate_cooldown_cancel_churn
                    && ask_cancels_passive
                    && !shared_risk_reducing
                    && !ask_side_retreat_or_bound
                    && (bid_side_retreat_or_bound || bid_pure_reentry || bid_fill_in));
            let isolate_bid = (staged_reentry_only && !risk_reducing && ask_fill_in)
                || (isolate_cooldown_cancel_churn
                    && bid_cancels_passive
                    && !shared_risk_reducing
                    && !bid_side_retreat_or_bound
                    && (ask_side_retreat_or_bound || ask_pure_reentry || ask_fill_in));
            if isolate_ask
                && !ask_plan.to_cancel.is_empty()
                && opp_would_defer(&ask_plan, bid_side_retreat_or_bound, ask_cancels_passive)
            {
                ask_plan = SideExecutionPlan::empty();
            }
            if isolate_bid
                && !bid_plan.to_cancel.is_empty()
                && opp_would_defer(&bid_plan, ask_side_retreat_or_bound, bid_cancels_passive)
            {
                bid_plan = SideExecutionPlan::empty();
            }
        }
        let pure_grid_fill_in = staged_reentry_only
            && bid_plan.to_cancel.is_empty()
            && ask_plan.to_cancel.is_empty()
            && (!bid_plan.to_place.is_empty() || !ask_plan.to_place.is_empty())
            && (bid_fill_in || ask_fill_in);
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

        let batch_cancels_strictly_passive = (!bid_plan.to_cancel.is_empty()
            || !ask_plan.to_cancel.is_empty())
            && (bid_plan.to_cancel.is_empty() || bid_cancels_passive)
            && (ask_plan.to_cancel.is_empty() || ask_cancels_passive);
        let micro_refresh_resting_orders = effective_resting_orders_for_micro_refresh(
            resting_orders_raw,
            runtime_config.grid_levels,
            isolate_cooldown_cancel_churn,
            partner_side_absent_for_cooldown_or_reentry,
            batch_cancels_strictly_passive,
        );
        if !risk_reducing
            && !pure_grid_fill_in
            && should_defer_micro_refresh(
                &inv_ctx,
                &bid_plan,
                &ask_plan,
                self.last_execution_batch_at,
                Instant::now(),
                micro_refresh_resting_orders,
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
            let had_cancels = !pending_cancel_ids.is_empty();
            #[cfg(feature = "research-replay")]
            let _diagnostic_dispatch = replay::recorded::ChangedQuotes::open(changed_sides);
            #[cfg(feature = "research-replay")]
            let _cancel_reasons_guard = had_cancels.then(|| {
                let mut reasons = Vec::with_capacity(
                    bid_plan.cancel_reasons.len() + ask_plan.cancel_reasons.len(),
                );
                reasons.extend_from_slice(&bid_plan.cancel_reasons);
                reasons.extend_from_slice(&ask_plan.cancel_reasons);
                replay::recorded::BatchCancelReasons::open(reasons)
            });
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
                    } else if had_cancels {
                        // A failed mixed batch left existing resting quotes live; pull them
                        // over the dedicated cancel lane so stale quotes cannot linger.
                        self.cancel_all_orders().await;
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
        if self.hedge.is_some() {
            // A single-leg taker close would leave the Hyperliquid hedge naked
            // and trip the cross-venue limit before it could be unwound.
            self.hedged_low_margin = true;
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
            self.account_stats_fair_mark_anchor = None;
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

/// Granularity of the event wait; each check is one atomic load plus a read lock.
const WAKE_CHECK_INTERVAL: Duration = Duration::from_millis(5);

/// Event-driven pacing: never before `min`, at the latest at `max`, and in
/// between as soon as market data or fills changed.
fn cycle_wake_due(elapsed: Duration, min: Duration, max: Duration, changed: bool) -> bool {
    elapsed >= max || (elapsed >= min && changed)
}

impl InventoryNeutralMM {
    /// Wait until new BBO data for the symbol, a new fill, shutdown, or the
    /// poll interval. Replaces a fixed sleep so requotes follow the market.
    async fn wait_for_next_cycle(&self, shutdown: Option<&tokio::sync::watch::Receiver<bool>>) {
        // tokio's clock so tests can drive time deterministically.
        let start = tokio::time::Instant::now();
        let max = Duration::from_millis(self.config.poll_interval_ms);
        let min = Duration::from_millis(self.config.min_cycle_interval_ms).min(max);
        let symbol = self.config.symbol_id;
        let fills = self.cycle_fill_baseline;
        tokio::time::sleep(min).await;
        loop {
            let elapsed = start.elapsed();
            let changed = shutdown_requested(shutdown)
                || self.shm_reader.shared_version(symbol) != self.shm_reader.local_version(symbol)
                || self.order_tracker.total_fill_stats().0 != fills;
            if cycle_wake_due(elapsed, min, max, changed) {
                return;
            }
            tokio::time::sleep(WAKE_CHECK_INTERVAL.min(max.saturating_sub(elapsed))).await;
        }
    }
}

fn shutdown_requested(shutdown: Option<&tokio::sync::watch::Receiver<bool>>) -> bool {
    shutdown.is_some_and(|rx| *rx.borrow())
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod self_fill_alpha_tests {
    use super::*;

    fn make_test_strategy(
        self_fill_alpha_bps: f64,
        self_fill_half_life_ms: u64,
    ) -> (InventoryNeutralMM, Arc<OrderTracker>, std::path::PathBuf) {
        use crate::exchange::{
            BatchOrderParams, BatchOrderResult, BatchResult, OrderInfo, OrderResult,
        };

        struct OfflineExchange;

        #[async_trait::async_trait]
        impl Exchange for OfflineExchange {
            async fn buy(&self, _: f64, _: f64) -> anyhow::Result<OrderResult> {
                anyhow::bail!("offline")
            }
            async fn sell(&self, _: f64, _: f64) -> anyhow::Result<OrderResult> {
                anyhow::bail!("offline")
            }
            async fn place_batch(&self, _: BatchOrderParams) -> anyhow::Result<BatchOrderResult> {
                anyhow::bail!("offline")
            }
            async fn cancel_order(&self, _: i64) -> anyhow::Result<()> {
                anyhow::bail!("offline")
            }
            async fn cancel_all(&self) -> anyhow::Result<u32> {
                anyhow::bail!("offline")
            }
            async fn get_active_orders(&self) -> anyhow::Result<Vec<OrderInfo>> {
                anyhow::bail!("offline")
            }
            async fn close_all_positions(&self, _: f64) -> anyhow::Result<()> {
                anyhow::bail!("offline")
            }
            async fn execute_batch(&self, _: Vec<BatchAction>) -> anyhow::Result<BatchResult> {
                anyhow::bail!("offline")
            }
            async fn get_account_stats(&self) -> anyhow::Result<AccountStats> {
                anyhow::bail!("offline")
            }
            fn limit_order_type(&self) -> OrderType {
                OrderType::PostOnly
            }
        }

        let directory =
            std::env::temp_dir().join(format!("aleph-self-fill-alpha-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let matrix_path = directory.join("matrix");
        let account_path = directory.join("account");
        let mut matrix_init = vec![0u8; 2048 * 8 + NUM_EXCHANGES * 64];
        matrix_init[2048 * 8 + 48..2048 * 8 + 64]
            .copy_from_slice(&crate::shm_reader::SHM_MAGIC_V12);
        std::fs::write(&matrix_path, matrix_init).unwrap();
        std::fs::write(&account_path, [0u8; 128]).unwrap();
        let shm = ShmReader::open(matrix_path.to_str().unwrap(), 1).unwrap();
        let account = AccountStatsReader::open(account_path.to_str().unwrap()).unwrap();
        let tracker = Arc::new(OrderTracker::new());
        let config = InventoryNeutralMMConfig {
            symbol_id: 0,
            max_basis_bps: 0.0,
            self_fill_alpha_bps,
            self_fill_half_life_ms,
            ..InventoryNeutralMMConfig::default()
        };
        let mut strategy = InventoryNeutralMM::with_data_sources(
            config,
            Arc::new(OfflineExchange),
            Arc::clone(&tracker),
            shm,
            account,
            None,
        );
        strategy.account_stats = AccountStats {
            available_balance: 950.0,
            portfolio_value: 1_000.0,
            position: 0.0,
            leverage: 1.0,
            margin_usage: 0.05,
            last_update: Instant::now(),
        };
        (strategy, tracker, directory)
    }

    fn fresh_books(mid: f64) -> ([(u8, ShmBboMessage); NUM_EXCHANGES], ShmBboMessage) {
        let ts_ns = wall_time_ns();
        let mk_bbo = |exchange_id: u8| ShmBboMessage {
            seqlock: 2,
            msg_type: 1,
            exchange_id,
            symbol_id: 0,
            timestamp_ns: ts_ns,
            bid_price: mid - 5.0,
            bid_size: 5.0,
            ask_price: mid + 5.0,
            ask_size: 5.0,
            _reserved: [0u8; 16],
        };
        let mut exchanges = [(0u8, ShmBboMessage::default()); NUM_EXCHANGES];
        exchanges[1] = (1, mk_bbo(1));
        let local = mk_bbo(2);
        exchanges[2] = (2, local);
        (exchanges, local)
    }

    fn apply_passive_fill(
        tracker: &OrderTracker,
        coid: i64,
        side: OrderSide,
        price: f64,
        size: f64,
        sequence: u64,
    ) {
        use crate::types::ShmPrivateEventV2;
        let is_ask = side == OrderSide::Sell;
        let exchange_id = 20_000 + coid as u64;
        tracker.start_tracking(coid, side, price, size);
        let created = ShmPrivateEventV2::order_created(
            sequence,
            2,
            0,
            exchange_id,
            coid,
            coid,
            price,
            size,
            is_ask,
            0,
        );
        tracker.apply_event(&created).unwrap();
        let fill = ShmPrivateEventV2::order_filled(
            sequence + 1,
            2,
            0,
            exchange_id,
            coid,
            coid,
            price,
            size,
            0.0,
            0.0,
            is_ask,
            0,
            90_000 + coid as u64,
        );
        tracker.apply_event(&fill).unwrap();
    }

    #[tokio::test]
    async fn disabled_self_fill_alpha_keeps_pricing_bit_identical_to_baseline() {
        tokio::time::pause();
        let (mut baseline, _baseline_tracker, dir_a) = make_test_strategy(0.0, 2_000);
        let (mut filled, fill_tracker, dir_b) = make_test_strategy(0.0, 2_000);

        apply_passive_fill(&fill_tracker, 1, OrderSide::Buy, 2_000.0, 0.01, 1);
        apply_passive_fill(&fill_tracker, 2, OrderSide::Sell, 2_000.0, 0.01, 3);

        let (exchanges, local) = fresh_books(2_000.0);
        let inputs_a = baseline.calculate_pricing_inputs(&exchanges, &local);
        let inputs_b = filled.calculate_pricing_inputs(&exchanges, &local);
        assert_eq!(
            inputs_b.pricing_mid.to_bits(),
            inputs_a.pricing_mid.to_bits()
        );

        let (bid_a, ask_a, bounds_a) = baseline.calculate_optimal_quotes(&inputs_a, 0.0).unwrap();
        let (bid_b, ask_b, bounds_b) = filled.calculate_optimal_quotes(&inputs_b, 0.0).unwrap();
        assert_eq!(bid_b.to_bits(), bid_a.to_bits());
        assert_eq!(ask_b.to_bits(), ask_a.to_bits());
        assert_eq!(bounds_b.max_bid.to_bits(), bounds_a.max_bid.to_bits());
        assert_eq!(bounds_b.min_ask.to_bits(), bounds_a.min_ask.to_bits());

        std::fs::remove_dir_all(dir_a).unwrap();
        std::fs::remove_dir_all(dir_b).unwrap();
    }

    #[tokio::test]
    async fn passive_buy_fill_shifts_pricing_mid_even_at_flat_inventory_and_decays_by_half_life() {
        tokio::time::pause();
        let (mut strategy, tracker, dir) = make_test_strategy(4.0, 2_000);
        let (mut unshifted, _unshifted_tracker, dir_ref) = make_test_strategy(0.0, 2_000);

        // Acquire short -0.01 inventory 60 s ago so its own self-fill alpha decays to ~0.
        apply_passive_fill(&tracker, 1, OrderSide::Sell, 2_000.0, 0.01, 1);
        tokio::time::advance(Duration::from_secs(60)).await;
        let (exchanges_pre, local_pre) = fresh_books(2_000.0);
        let pre = strategy.calculate_pricing_inputs(&exchanges_pre, &local_pre);
        let _ = unshifted.calculate_pricing_inputs(&exchanges_pre, &local_pre);
        assert!((pre.pricing_mid - 2_000.0).abs() < 1e-6);
        assert!((tracker.confirmed_position() + 0.01).abs() < 1e-12);

        // Passive Buy fill flattens inventory (-0.01 -> 0.0 flat), yet still shifts
        // fair value down by -4.0 bps (-$0.80 at $2000).
        tokio::time::advance(Duration::from_millis(500)).await;
        apply_passive_fill(&tracker, 2, OrderSide::Buy, 2_000.0, 0.01, 3);
        assert!(tracker.confirmed_position().abs() < 1e-12);
        strategy.account_stats.position = 0.0;
        strategy.account_stats.last_update = Instant::now();
        unshifted.account_stats.last_update = Instant::now();

        let (exchanges_0, local_0) = fresh_books(2_000.0);
        let inputs_0 = strategy.calculate_pricing_inputs(&exchanges_0, &local_0);
        let ref_0 = unshifted.calculate_pricing_inputs(&exchanges_0, &local_0);
        assert!(
            (inputs_0.pricing_mid - 1_999.20).abs() < 1e-6,
            "expected -4 bps shift to 1999.20, got {}",
            inputs_0.pricing_mid
        );
        let (bid_0, ask_0, _) = strategy.calculate_optimal_quotes(&inputs_0, 0.0).unwrap();
        let (ref_bid, ref_ask, _) = unshifted.calculate_optimal_quotes(&ref_0, 0.0).unwrap();
        assert!(bid_0 < ref_bid && ask_0 < ref_ask);

        // After 1 half-life (2000 ms), offset decays by exact 50% to -2.0 bps (1999.60).
        tokio::time::advance(Duration::from_millis(2_000)).await;
        let (exchanges_half, local_half) = fresh_books(2_000.0);
        let inputs_half = strategy.calculate_pricing_inputs(&exchanges_half, &local_half);
        assert!(
            (inputs_half.pricing_mid - 1_999.60).abs() < 1e-6,
            "expected 50% decayed shift at 1999.60, got {}",
            inputs_half.pricing_mid
        );

        // After 10 half-lives total (another 18,000 ms), offset decays to 0.5^10 (< 0.004 bps, ~0).
        tokio::time::advance(Duration::from_millis(18_000)).await;
        let (exchanges_ten, local_ten) = fresh_books(2_000.0);
        let inputs_ten = strategy.calculate_pricing_inputs(&exchanges_ten, &local_ten);
        assert!(
            (inputs_ten.pricing_mid - 2_000.0).abs() < 1e-3,
            "expected ~0 offset after 10 half-lives, got {}",
            inputs_ten.pricing_mid
        );
        let residual_bps = (inputs_ten.pricing_mid / 2_000.0 - 1.0) * 10_000.0;
        assert!((residual_bps - (-4.0 / 1024.0)).abs() < 1e-6);

        std::fs::remove_dir_all(dir).unwrap();
        std::fs::remove_dir_all(dir_ref).unwrap();
    }

    #[tokio::test]
    async fn burst_of_ten_same_side_fills_is_clamped_at_three_times_alpha() {
        tokio::time::pause();
        let (mut buy_strategy, buy_tracker, dir_buy) = make_test_strategy(4.0, 2_000);
        for i in 1..=10_i64 {
            apply_passive_fill(
                &buy_tracker,
                i,
                OrderSide::Buy,
                2_000.0,
                0.01,
                (i as u64) * 2,
            );
        }
        let (exchanges, local) = fresh_books(2_000.0);
        let buy_inputs = buy_strategy.calculate_pricing_inputs(&exchanges, &local);
        // Clamped at -3 * 4.0 = -12.0 bps -> 2000 * (1 - 0.0012) = 1997.60.
        assert!(
            (buy_inputs.pricing_mid - 1_997.60).abs() < 1e-9,
            "expected -12 bps clamp at 1997.60, got {}",
            buy_inputs.pricing_mid
        );

        let (mut sell_strategy, sell_tracker, dir_sell) = make_test_strategy(4.0, 2_000);
        for i in 1..=10_i64 {
            apply_passive_fill(
                &sell_tracker,
                i,
                OrderSide::Sell,
                2_000.0,
                0.01,
                (i as u64) * 2,
            );
        }
        let sell_inputs = sell_strategy.calculate_pricing_inputs(&exchanges, &local);
        // Clamped at +3 * 4.0 = +12.0 bps -> 2000 * (1 + 0.0012) = 2002.40.
        assert!(
            (sell_inputs.pricing_mid - 2_002.40).abs() < 1e-9,
            "expected +12 bps clamp at 2002.40, got {}",
            sell_inputs.pricing_mid
        );

        std::fs::remove_dir_all(dir_buy).unwrap();
        std::fs::remove_dir_all(dir_sell).unwrap();
    }

    #[tokio::test]
    async fn reconcile_in_session_preserves_inventory_and_mixed_batch_failure_pulls_cancels() {
        use crate::exchange::{
            BatchOrderParams, BatchOrderResult, BatchResult, OrderInfo, OrderResult,
        };
        use std::io::Write;
        use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering as AtomicOrdering};

        struct RecordingExchange {
            tracker: Arc<OrderTracker>,
            account_path: std::path::PathBuf,
            stats_version: AtomicU64,
            reconciled_non_flat: AtomicUsize,
            closed_all: AtomicUsize,
            cancel_only_batches: AtomicUsize,
            fail_batch: AtomicBool,
        }

        impl RecordingExchange {
            fn write_fresh_stats(&self, position: f64) {
                let version = self.stats_version.fetch_add(2, AtomicOrdering::AcqRel);
                let ts = wall_time_ns() + 10_000_000;
                let mut buf = [0u8; 128];
                buf[0..8].copy_from_slice(&version.to_ne_bytes());
                buf[8..16].copy_from_slice(&900.0_f64.to_ne_bytes());
                buf[16..24].copy_from_slice(&1_000.0_f64.to_ne_bytes());
                buf[24..32].copy_from_slice(&1.0_f64.to_ne_bytes());
                buf[32..40].copy_from_slice(&900.0_f64.to_ne_bytes());
                buf[40..48].copy_from_slice(&0.1_f64.to_ne_bytes());
                buf[48..56].copy_from_slice(&2_000.0_f64.to_ne_bytes());
                buf[56..64].copy_from_slice(&ts.to_ne_bytes());
                buf[64..72].copy_from_slice(&position.to_ne_bytes());
                buf[72..80].copy_from_slice(&ts.to_ne_bytes());
                let mut f = std::fs::OpenOptions::new()
                    .write(true)
                    .open(&self.account_path)
                    .unwrap();
                f.write_all(&buf).unwrap();
                f.sync_data().unwrap();
            }
        }

        #[async_trait::async_trait]
        impl Exchange for RecordingExchange {
            async fn buy(&self, _: f64, _: f64) -> anyhow::Result<OrderResult> {
                anyhow::bail!("unused")
            }
            async fn sell(&self, _: f64, _: f64) -> anyhow::Result<OrderResult> {
                anyhow::bail!("unused")
            }
            async fn place_batch(&self, _: BatchOrderParams) -> anyhow::Result<BatchOrderResult> {
                anyhow::bail!("unused")
            }
            async fn cancel_order(&self, _: i64) -> anyhow::Result<()> {
                Ok(())
            }
            async fn cancel_all(&self) -> anyhow::Result<u32> {
                self.write_fresh_stats(0.25);
                Ok(1)
            }
            async fn get_active_orders(&self) -> anyhow::Result<Vec<OrderInfo>> {
                Ok(vec![])
            }
            async fn close_all_positions(&self, _: f64) -> anyhow::Result<()> {
                self.closed_all.fetch_add(1, AtomicOrdering::AcqRel);
                Ok(())
            }
            async fn reconcile_non_flat_without_feed(&self) -> anyhow::Result<f64> {
                self.reconciled_non_flat
                    .fetch_add(1, AtomicOrdering::AcqRel);
                let cois = self.tracker.recovery_cois();
                anyhow::ensure!(
                    self.tracker.confirm_recovered_position(
                        self.tracker.reconciliation_epoch(),
                        0.25,
                        &cois,
                    ),
                    "mock confirm_recovered_position failed"
                );
                self.write_fresh_stats(0.25);
                Ok(0.25)
            }
            async fn execute_batch(
                &self,
                actions: Vec<BatchAction>,
            ) -> anyhow::Result<BatchResult> {
                if !actions.is_empty()
                    && actions.iter().all(|a| matches!(a, BatchAction::Cancel(_)))
                {
                    self.cancel_only_batches
                        .fetch_add(1, AtomicOrdering::AcqRel);
                    return Ok(BatchResult {
                        tx_hashes: vec!["0xcancel".into()],
                        place_results: vec![],
                    });
                }
                if self.fail_batch.load(AtomicOrdering::Acquire) {
                    anyhow::bail!("simulated mixed batch rejection");
                }
                Ok(BatchResult {
                    tx_hashes: vec!["0xok".into()],
                    place_results: vec![],
                })
            }
            async fn get_account_stats(&self) -> anyhow::Result<AccountStats> {
                Ok(AccountStats {
                    available_balance: 900.0,
                    portfolio_value: 1_000.0,
                    position: 0.25,
                    leverage: 1.0,
                    margin_usage: 0.1,
                    last_update: Instant::now(),
                })
            }
            fn limit_order_type(&self) -> OrderType {
                OrderType::PostOnly
            }
        }

        let directory =
            std::env::temp_dir().join(format!("aleph-in-session-rec-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let matrix_path = directory.join("matrix");
        let account_path = directory.join("account");
        let mut matrix_init = vec![0u8; 2048 * 8 + NUM_EXCHANGES * 64];
        matrix_init[2048 * 8 + 48..2048 * 8 + 64]
            .copy_from_slice(&crate::shm_reader::SHM_MAGIC_V12);
        std::fs::write(&matrix_path, matrix_init).unwrap();
        std::fs::write(&account_path, [0u8; 128]).unwrap();
        let shm = ShmReader::open(matrix_path.to_str().unwrap(), 1).unwrap();
        let account = AccountStatsReader::open(account_path.to_str().unwrap()).unwrap();
        let tracker = Arc::new(OrderTracker::new());
        let exchange = Arc::new(RecordingExchange {
            tracker: Arc::clone(&tracker),
            account_path: account_path.clone(),
            stats_version: AtomicU64::new(2),
            reconciled_non_flat: AtomicUsize::new(0),
            closed_all: AtomicUsize::new(0),
            cancel_only_batches: AtomicUsize::new(0),
            fail_batch: AtomicBool::new(true),
        });
        let config = InventoryNeutralMMConfig {
            symbol_id: 0,
            max_basis_bps: 0.0,
            requote_threshold_bps: 1.0,
            order_ttl_secs: 0,
            grid_levels: 1,
            max_position: 1.0,
            ..InventoryNeutralMMConfig::default()
        };
        let mut strategy = InventoryNeutralMM::with_data_sources(
            config,
            Arc::clone(&exchange) as Arc<dyn Exchange>,
            Arc::clone(&tracker),
            shm,
            account,
            None,
        );
        strategy.account_stats = AccountStats {
            available_balance: 900.0,
            portfolio_value: 1_000.0,
            position: 0.25,
            leverage: 1.0,
            margin_usage: 0.1,
            last_update: Instant::now(),
        };

        // 1. In-session reconciliation with stuck active order reconciles without flattening 0.25.
        tracker.start_tracking(9001, OrderSide::Buy, 2_000.0, 0.05);
        tracker.require_reconciliation();
        strategy.reconcile_in_session("test").await.unwrap();
        assert_eq!(
            exchange.reconciled_non_flat.load(AtomicOrdering::Acquire),
            1
        );
        assert_eq!(exchange.closed_all.load(AtomicOrdering::Acquire), 0);
        assert!(!tracker.needs_reconciliation());
        assert!((tracker.confirmed_position() - 0.25).abs() < 1e-9);
        let _ = strategy.account_stats_reader.read_if_updated();

        // 2. Execute mixed-batch failure with a pending cancel triggers cancel_all_orders fallback.
        assert!(tracker.confirm_cleanup(tracker.reconciliation_epoch(), 0.0));
        strategy.account_stats.position = 0.0;
        tokio::time::pause();
        tracker.start_tracking(9002, OrderSide::Buy, 1_980.0, 0.01);
        let created = crate::types::events::ShmPrivateEventV2::order_created(
            1, 2, 0, 99002, 9002, 42, 1_980.0, 0.01, false, 0,
        );
        tracker.apply_event(&created).unwrap();
        tokio::time::advance(Duration::from_secs(10)).await;
        strategy.account_stats.last_update = Instant::now();
        strategy.refresh_active_orders_from_tracker();
        let (exchanges, local) = fresh_books(2_000.0);
        let inputs = strategy.calculate_pricing_inputs(&exchanges, &local);
        let bounds = pricing::MakerPriceBounds {
            max_bid: 2_000.0,
            min_ask: 2_000.0,
        };
        strategy
            .execute_quoting_cycle(
                1_999.0,
                2_001.0,
                bounds,
                &inputs,
                None,
                #[cfg(feature = "research-replay")]
                [true, true],
            )
            .await;
        assert_eq!(
            exchange.cancel_only_batches.load(AtomicOrdering::Acquire),
            1
        );

        // 3. Fresh zero equity in quote_cycle trips DailyLossGuard immediately.
        strategy.daily_loss = Some(
            crate::daily_loss::DailyLossGuard::open(None, 0.05, 1_000.0, strategy_utc_day())
                .unwrap(),
        );
        strategy.account_stats.portfolio_value = 0.0;
        strategy.account_stats.last_update = Instant::now();
        let err = strategy.quote_cycle(None).await.unwrap_err();
        assert!(
            format!("{err:#}").contains("daily loss"),
            "expected daily loss error on zero equity, got {err:#}"
        );

        std::fs::remove_dir_all(directory).unwrap();
    }
}
