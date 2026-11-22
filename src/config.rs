//! Central configuration for AlephTX strategies.
//!
//! Loads from `config.toml` at the project root.
//! All trading parameters are runtime-configurable — no recompilation needed.

pub const EXCH_HYPERLIQUID: u8 = 1;
pub const EXCH_LIGHTER: u8 = 2;
pub const EXCH_EDGEX: u8 = 3;
pub const EXCH_01: u8 = 4;
pub const EXCH_BACKPACK: u8 = 5;
pub const EXCH_BINANCE: u8 = 6;
pub const EXCH_ARCUS: u8 = 7;
pub const EXCH_LIGHTER_ROBINHOOD: u8 = 8;
pub const EXCH_ENTROPY: u8 = 9;
pub const EXCH_TRADEXYZ: u8 = 10;
pub const EXCH_EXTENDED: u8 = 11;

pub const SYM_BTC: u16 = 1001;
pub const SYM_ETH: u16 = 1002;

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

/// Round value to nearest tick/step size
#[inline]
pub fn round_to_tick(val: f64, tick: f64) -> f64 {
    if tick <= 0.0 {
        return val; // Protect against division by zero
    }
    (val / tick).round() * tick
}

// Non-power-of-ten ticks (e.g. 0.25) need more precision than ceil(-log10(tick)).
fn decimal_places(step: f64) -> usize {
    if !step.is_finite() || step <= 0.0 {
        return 0;
    }
    let mut scaled = step;
    for places in 0..=12 {
        if (scaled - scaled.round()).abs() <= 1e-9 {
            return places;
        }
        scaled *= 10.0;
    }
    12
}

/// Format price with dynamic precision based on tick size
pub fn format_price(price: f64, tick_size: f64) -> String {
    let decimals = decimal_places(tick_size);
    format!(
        "{:.prec$}",
        round_to_tick(price, tick_size),
        prec = decimals
    )
}

/// Format size with dynamic precision based on step size
pub fn format_size(size: f64, step_size: f64) -> String {
    let decimals = decimal_places(step_size);
    format!("{:.prec$}", round_to_tick(size, step_size), prec = decimals)
}

/// Exclusive upper bound on symbol IDs imposed by the SHM matrix layout.
pub const MAX_SYMBOL_ID: u16 = 2048;

/// Historical ticker -> symbol ID table used when `[symbols]` is absent.
pub fn default_symbols() -> BTreeMap<String, u16> {
    BTreeMap::from([("BTC".to_string(), SYM_BTC), ("ETH".to_string(), SYM_ETH)])
}

/// Built-in name for the historical IDs. Prefer [`AppConfig::symbol_name`],
/// which honours the configured `[symbols]` table.
pub fn symbol_name(symbol_id: u16) -> &'static str {
    match symbol_id {
        SYM_BTC => "BTC",
        SYM_ETH => "ETH",
        _ => "UNKNOWN",
    }
}

/// Per-exchange strategy configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct ExchangeConfig {
    /// Fraction of account balance to use as max position (e.g. 0.10 = 10%)
    pub risk_fraction: f64,
    /// Minimum half-spread floor in basis points
    pub min_spread_bps: f64,
    /// Spread = max(min_spread, realized_vol × vol_multiplier)
    pub vol_multiplier: f64,
    /// Stop-loss as fraction of entry price (e.g. 0.003 = 0.3%)
    pub stop_loss_pct: f64,
    /// Minimum milliseconds between re-quotes
    pub requote_interval_ms: u64,
    /// Momentum detection threshold (bps over last 5 ticks)
    #[serde(default = "default_momentum_threshold")]
    pub momentum_threshold_bps: f64,
    /// Multiply losing-side spread by this when momentum detected
    #[serde(default = "default_momentum_mult")]
    pub momentum_spread_mult: f64,
    /// Number of mid-price samples for volatility ring buffer
    #[serde(default = "default_vol_window")]
    pub vol_window: usize,
    /// How often to refresh balance (seconds)
    #[serde(default = "default_balance_refresh")]
    pub balance_refresh_secs: u64,
    /// Minimum order size (for exchanges with minimums like EdgeX)
    #[serde(default)]
    pub min_order_size: f64,
    /// Price tick size (e.g. 0.01 for $0.01 increments)
    #[serde(default = "default_tick_size")]
    pub tick_size: f64,
    /// Size step size (e.g. 0.01 for 0.01 unit increments)
    #[serde(default = "default_step_size")]
    pub step_size: f64,
    /// Avellaneda-Stoikov risk aversion parameter
    #[serde(default = "default_gamma")]
    pub gamma: f64,
    /// Avellaneda-Stoikov time horizon in seconds
    #[serde(default = "default_time_horizon")]
    pub time_horizon_sec: f64,
    /// Minimum price deviation (bps) to trigger requote (Phase 2 incremental quoting)
    #[serde(default = "default_requote_threshold")]
    pub requote_threshold_bps: f64,

    // EdgeX-specific L2 configuration
    #[serde(default)]
    pub contract_id: Option<u64>,
    #[serde(default)]
    pub synthetic_asset_id: Option<String>,
    #[serde(default)]
    pub collateral_asset_id: Option<String>,
    #[serde(default)]
    pub fee_asset_id: Option<String>,
    #[serde(default)]
    pub price_decimals: Option<u32>,
    #[serde(default)]
    pub size_decimals: Option<u32>,
    #[serde(default)]
    pub resolution: Option<u64>,
    #[serde(default)]
    pub collateral_resolution: Option<u64>,
    #[serde(default)]
    pub fee_rate: Option<f64>,
}

fn default_momentum_threshold() -> f64 {
    8.0
}
fn default_momentum_mult() -> f64 {
    2.0
}
fn default_vol_window() -> usize {
    120
}
fn default_balance_refresh() -> u64 {
    60
}
fn default_tick_size() -> f64 {
    0.01
}
fn default_step_size() -> f64 {
    0.01
}
fn default_gamma() -> f64 {
    0.1
}
fn default_time_horizon() -> f64 {
    60.0
}
fn default_requote_threshold() -> f64 {
    2.0 // 2 bps deviation threshold
}
fn default_markout_horizon_secs() -> f64 {
    5.0
}
fn default_markout_halflife_fills() -> f64 {
    30.0
}
fn default_markout_threshold_bps() -> f64 {
    -1.0
}
fn default_self_fill_half_life_ms() -> u64 {
    2000
}
fn default_min_cycle_interval_ms() -> u64 {
    50
}
fn default_poll_interval_ms() -> u64 {
    100
}
fn default_margin_cooldown_secs() -> u64 {
    5
}
fn default_max_daily_loss_pct() -> f64 {
    0.05
}
fn default_position_timeout_secs() -> u64 {
    60
}
fn default_local_staleness_ms() -> u64 {
    1000
}
fn default_stale_bbo_cancel_ms() -> u64 {
    2500
}
fn default_require_external_fair_value() -> bool {
    true
}
fn default_target_resting_margin_utilization() -> f64 {
    0.35
}
fn default_grid_levels() -> u8 {
    3
}
fn default_grid_spacing_bps() -> f64 {
    2.0
}
fn default_grid_size_decay() -> f64 {
    0.7
}
fn default_sigmoid_steepness() -> f64 {
    4.0
}
fn default_flattening_cap_mult() -> f64 {
    2.0
}
fn default_micro_samples() -> usize {
    200
}
fn default_ema_fast_period() -> usize {
    5
}
fn default_ema_slow_period() -> usize {
    20
}
fn default_use_depth_pricing() -> bool {
    true
}
fn default_vol_spread_scale() -> f64 {
    0.5
}
fn default_momentum_skew_scale() -> f64 {
    0.3
}
fn default_external_staleness_ms() -> u64 {
    2000
}
fn default_basis_tau_secs() -> f64 {
    300.0
}
fn default_max_basis_bps() -> f64 {
    20.0
}
fn default_reference_portfolio_value() -> f64 {
    100.0
}
fn default_min_portfolio_scale() -> f64 {
    0.5
}
fn default_max_portfolio_scale() -> f64 {
    20.0
}
fn default_as_gamma() -> f64 {
    5000.0
}
fn default_as_time_horizon_sec() -> f64 {
    1.0
}
fn default_as_kappa() -> f64 {
    2000.0
}
fn default_max_spread_bps() -> f64 {
    60.0
}
fn default_min_inventory_notional_usd() -> f64 {
    10.0
}
fn default_flatten_age_start_fraction() -> f64 {
    1.0
}

/// Inventory Neutral Market Maker 策略配置
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InventoryNeutralMMConfig {
    // 交易所
    pub exchange_id: u8, // BBO 过滤用 (2=Lighter)
    pub symbol_id: u16,  // SHM symbol ID
    pub market_id: i64,  // Lighter market ID, independent of the SHM symbol ID
    pub tick_size: f64,  // 价格精度
    pub step_size: f64,  // 数量精度
    // 费率
    pub maker_fee_bps: f64,  // default: 0.38
    pub min_profit_bps: f64, // default: 1.0
    // 报价
    pub penny_ticks: f64,        // default: 1.0
    pub inventory_skew_bps: f64, // default: 3.0
    // Grid Quoting (Multi-Level Laddering)
    #[serde(default = "default_grid_levels")]
    pub grid_levels: u8, // default: 3 (1-5 levels per side)
    #[serde(default = "default_grid_spacing_bps")]
    pub grid_spacing_bps: f64, // default: 2.0 (spacing between levels)
    #[serde(default = "default_grid_size_decay")]
    pub grid_size_decay: f64, // default: 0.7 (size multiplier per level)
    // 仓位
    pub base_order_size: f64,             // default: 0.05
    pub max_position: f64,                // default: 0.15
    pub inventory_urgency_threshold: f64, // default: 0.08
    #[serde(default)]
    pub base_order_notional_usd: f64, // optional: quote in USD notional instead of base units
    /// Round USD-derived `base_order_size` to the nearest `step_size` instead of flooring,
    /// avoiding systematic under-sizing on coarse-step assets (e.g. BTC 0.0001 lots at $500).
    #[serde(default)]
    pub round_base_notional_nearest: bool,
    #[serde(default)]
    pub max_position_notional_usd: f64, // optional: max inventory in USD notional
    #[serde(default)]
    pub inventory_urgency_notional_usd: f64, // optional: urgency threshold in USD notional
    #[serde(default = "default_min_inventory_notional_usd")]
    pub min_inventory_notional_usd: f64, // default: 10.0 (minimum meaningful inventory deadband)
    #[serde(default = "default_reference_portfolio_value")]
    pub reference_portfolio_value: f64, // scaling anchor for legacy base-unit configs
    #[serde(default = "default_min_portfolio_scale")]
    pub min_portfolio_scale: f64, // lower clamp for equity-aware scaling
    #[serde(default = "default_max_portfolio_scale")]
    pub max_portfolio_scale: f64, // upper clamp for equity-aware scaling
    // Sigmoid SIZE偏移 (v4.0.0)
    #[serde(default = "default_sigmoid_steepness")]
    pub sigmoid_steepness: f64, // default: 4.0 (控制sigmoid曲线陡峭度)
    // Flattening hard cap: max single-order size = base_order_size * cap_mult (v5.0.0)
    #[serde(default = "default_flattening_cap_mult")]
    pub flattening_cap_mult: f64, // default: 2.0 (防止反向巨单鞭尾)
    // 微观结构参数 (v4.0.0)
    #[serde(default = "default_micro_samples")]
    pub micro_samples: usize, // default: 200 (价格样本数)
    #[serde(default = "default_ema_fast_period")]
    pub ema_fast_period: usize, // default: 5 (快速EMA周期)
    #[serde(default = "default_ema_slow_period")]
    pub ema_slow_period: usize, // default: 20 (慢速EMA周期)
    #[serde(default = "default_use_depth_pricing")]
    pub use_depth_pricing: bool, // default: true (启用OBI+VWMicro定价)
    #[serde(default = "default_vol_spread_scale")]
    pub vol_spread_scale: f64, // default: 0.5 (波动率价差缩放)
    #[serde(default = "default_momentum_skew_scale")]
    pub momentum_skew_scale: f64, // default: 0.3 (动量偏移缩放)
    // External fair value anchor (v6.0.0)
    #[serde(default = "default_external_staleness_ms")]
    pub external_staleness_ms: u64, // default: 2000 (max age for external BBO in ms)
    /// Cancel resting quotes instead of falling back to Lighter's own mid when
    /// all configured external fair-value references are stale or unavailable.
    #[serde(default = "default_require_external_fair_value")]
    pub require_external_fair_value: bool,
    /// Optional allowlist of SHM exchange IDs (1..NUM_EXCHANGES) included in the
    /// external fair-value median. When omitted, only Core Hyperliquid (1),
    /// Lighter Core (2), and Binance USD-M (6) contribute so USDG/RWA domains
    /// never enter the median unless explicitly listed.
    #[serde(default)]
    pub external_reference_exchanges: Option<Vec<u8>>,
    /// Primary external SHM exchange ID used by order-flow, cross-market, and
    /// volatility-regime reserves. Defaults to EXCH_HYPERLIQUID (1) when unset.
    #[serde(default)]
    pub primary_reference_exchange: Option<u8>,
    /// EWMA time constant for the Lighter-vs-external basis correction.
    #[serde(default = "default_basis_tau_secs")]
    pub basis_tau_secs: f64, // default: 300
    /// Clamp on the applied basis; 0 disables the correction.
    #[serde(default = "default_max_basis_bps")]
    pub max_basis_bps: f64, // default: 20
    // Avellaneda-Stoikov 定价参数 (v5.1.0)
    #[serde(default = "default_as_gamma")]
    pub as_gamma: f64, // default: 5000.0 (风险厌恶系数，fractional σ 下需要大值)
    #[serde(default = "default_as_time_horizon_sec")]
    pub as_time_horizon_sec: f64, // default: 1.0 (时间窗口秒，DEX 短周期)
    #[serde(default = "default_as_kappa")]
    pub as_kappa: f64, // default: 1.5 (订单到达强度，可配置)
    #[serde(default = "default_max_spread_bps")]
    pub max_spread_bps: f64, // default: 30.0 (A-S spread 安全上限)
    // 风控
    pub adverse_selection_threshold: f64, // default: 2.0
    pub requote_threshold_bps: f64,       // default: 1.5
    pub order_ttl_secs: u64,              // default: 5
    pub max_leverage: f64,                // default: 10.0
    pub min_available_balance: f64,       // default: 2.0
    #[serde(default = "default_margin_cooldown_secs")]
    pub margin_cooldown_secs: u64, // default: 5
    /// Fraction of the UTC-day starting equity that may be lost before the
    /// strategy halts. The baseline and breach persist across restarts.
    #[serde(default = "default_max_daily_loss_pct")]
    pub max_daily_loss_pct: f64, // default: 0.05
    /// Unhedged inventory held beyond the deadband this long is IOC-flattened.
    #[serde(default = "default_position_timeout_secs")]
    pub position_timeout_secs: u64, // default: 60
    /// Local BBO older than this freezes quoting.
    #[serde(default = "default_local_staleness_ms")]
    pub local_staleness_ms: u64, // default: 5000
    /// Local BBO stale for longer than this cancels all resting orders.
    #[serde(default = "default_stale_bbo_cancel_ms")]
    pub stale_bbo_cancel_ms: u64, // default: 15000
    /// Fraction of leveraged usable balance targeted by resting top-level quotes.
    #[serde(default = "default_target_resting_margin_utilization")]
    pub target_resting_margin_utilization: f64, // default: 0.35
    /// Optional explicit Lighter maintenance margin fraction (e.g. `0.012` for 1.2% ETH/BTC MMF).
    /// When `None`, derived conservatively from `max_leverage` as
    /// `(0.25 / max_leverage).min(0.8 / (max_leverage * pre_liquidation_safety_multiple))`.
    #[serde(default)]
    pub maintenance_margin_fraction: Option<f64>,
    /// Proactive deleverage multiple over maintenance margin (`1.8` = exit before 1% LLP fee zone).
    #[serde(default = "default_pre_liquidation_safety_multiple")]
    pub pre_liquidation_safety_multiple: f64,
    // Post-Only: use ALO (Add Liquidity Only) to guarantee maker fees
    #[serde(default)]
    pub use_post_only: bool, // default: false (GTC), true = Post-Only
    // Poll interval for main loop (ms)
    #[serde(default = "default_poll_interval_ms")]
    pub poll_interval_ms: u64, // default: 100
    /// Floor between quoting cycles. After it, a new BBO version for the
    /// symbol or a new fill starts the next cycle immediately; otherwise the
    /// loop wakes after `poll_interval_ms`. Set >= poll_interval_ms to disable.
    #[serde(default = "default_min_cycle_interval_ms")]
    pub min_cycle_interval_ms: u64, // default: 50
    /// Widen spreads when realized fill markouts show adverse selection.
    /// The multiplier is in [1, 1.5] and never tightens quotes.
    #[serde(default)]
    pub use_markout_spread: bool, // default: false
    #[serde(default = "default_markout_horizon_secs")]
    pub markout_horizon_secs: f64, // default: 5
    #[serde(default = "default_markout_halflife_fills")]
    pub markout_halflife_fills: f64, // default: 30
    /// Average markout (bps) tolerated before widening; negative = adverse.
    #[serde(default = "default_markout_threshold_bps")]
    pub markout_threshold_bps: f64, // default: -1
    /// Directional markout reserve on inventory-adding quotes.
    #[serde(default)]
    pub markout_reserve: Option<MarkoutReserveConfig>,
    /// Reserve for short-horizon pressure in the external Hyperliquid book.
    #[serde(default)]
    pub order_flow_reserve: Option<OrderFlowReserveConfig>,
    /// Directional reserve for transient cross-market price and depth pressure.
    #[serde(default)]
    pub cross_market_reserve: Option<CrossMarketReserveConfig>,
    /// Adjust inventory-adding quotes using completed external price ranges.
    #[serde(default)]
    pub volatility_regime: Option<VolatilityRegimeConfig>,
    /// Decouple resting cancel edge floor and passive inventory-flattening bounds
    /// from new-quote placement edge for 50ms+ latency regimes.
    #[serde(default)]
    pub quote_hysteresis: Option<QuoteHysteresisConfig>,
    /// Decoupled adverse-selection fair-value offset per passive fill (bps, 0 disables).
    /// Passive buy fills shift `pricing_mid` down; passive sell fills shift `pricing_mid` up.
    #[serde(default)]
    pub self_fill_alpha_bps: f64,
    /// Exponential decay half-life (ms) for `self_fill_alpha_bps` (`>= 50` when enabled).
    #[serde(default = "default_self_fill_half_life_ms")]
    pub self_fill_half_life_ms: u64,
    /// Production+replay local fair value pricing policy when external references are absent/disabled.
    #[serde(default)]
    pub local_fair_value_config: Option<LocalFairValueConfig>,
    /// Local BBO order-flow imbalance, cancel speed-bump option reserve, and local markout gating.
    #[serde(default)]
    pub local_toxicity: Option<LocalToxicityConfig>,
    /// Minimum reported venue volume quota before halting new inventory-adding quotes (`OPT-05`).
    #[serde(default)]
    pub min_volume_quota_for_quoting: Option<i64>,
    /// Maximum entry deviation (bps) from fair mid enforced pre-nonce at execution boundary (`§7.E`).
    #[serde(default)]
    pub max_order_price_band_bps: Option<f64>,
    /// Deterministic 5-state defense escalation state machine (`OPT-07`).
    #[serde(default)]
    pub defense_policy: Option<DefensePolicyConfig>,
    /// Optional notional USD unit (`q * mid / unit_usd`) normalizing A-S inventory term across assets (`VMM-07`).
    #[serde(default)]
    pub as_inventory_notional_usd: Option<f64>,
    /// Optional gap duration (seconds) after which `TimeScaledVolatility` resets prior variance (`VMM-07`).
    #[serde(default)]
    pub vol_gap_reset_secs: Option<u64>,
    /// Scale cold-start per-sample `vol_bps` by `1 / sqrt(poll_interval_secs)` (`VMM-07`).
    #[serde(default)]
    pub normalize_cold_start_sigma_by_poll: bool,
    /// Do not let `target_resting_margin_utilization` inflate `base_order_size` above the
    /// configured `base_order_size` / `base_order_notional_usd` budget (`S7-2`).
    #[serde(default)]
    pub cap_utilization_floor_to_base_notional: bool,
    /// Optional causal cap `(0.0, 5.0]` on inventory-adding top-level order size as a
    /// fraction of fresh local L1 touch notional (`S7-2`).
    #[serde(default)]
    pub max_top_book_depth_fraction: Option<f64>,
    /// Optional target A-S reservation shift (bps) at `max_position` (`S9-2`).
    /// Derives `gamma_inv = (budget_bps / 1e4) / (sigma^2 * T * max_pos_for_as)` cleanly
    /// while leaving `as_gamma / as_kappa` decoupled for spread computation.
    #[serde(default)]
    pub as_risk_budget_bps_at_max_pos: Option<f64>,
}

fn default_defense_caution_size_scale() -> f64 {
    0.5
}

fn default_defense_cooldown_ms() -> u64 {
    3_000
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DefensePolicyConfig {
    /// Local BBO spread (bps) entering `Caution` regime.
    pub caution_spread_bps: f64,
    /// Extra additive reserve (bps) on inventory-adding sides during `Caution`.
    #[serde(default)]
    pub caution_widen_bps: f64,
    /// Multiplier in `(0.05, 1.0]` applied to inventory-adding sizes during `Caution`.
    #[serde(default = "default_defense_caution_size_scale")]
    pub caution_size_scale: f64,
    /// Causal short-horizon EMA momentum magnitude (bps) entering `SideDefense`.
    pub side_defense_momentum_bps: f64,
    /// Local BBO spread (bps) entering two-sided `ReduceOnly` (zero new entries).
    pub reduce_only_spread_bps: f64,
    /// Extreme local BBO spread (bps) entering `Halt` (immediate `cancel_all_orders`).
    pub halt_spread_bps: f64,
    /// Minimum calm hold duration (ms) before stepping down from defensive states.
    #[serde(default = "default_defense_cooldown_ms")]
    pub cooldown_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalFairValueMode {
    Mid,
    TouchMicro,
    DepthMicro,
}

fn default_local_fair_value_max_shift_bps() -> f64 {
    2.0
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalFairValueConfig {
    pub mode: LocalFairValueMode,
    #[serde(default = "default_local_fair_value_max_shift_bps")]
    pub max_shift_bps: f64,
}

fn default_cancel_option_mult() -> f64 {
    0.4
}

fn default_cancel_option_max_bps() -> f64 {
    10.0
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalToxicityConfig {
    /// Half-life (ms) for Cont-Kukanov-Stoikov BBO order-flow imbalance (`0` disables local OFI).
    #[serde(default)]
    pub ofi_half_life_ms: u64,
    /// Max directional widening (bps) on the inventory-adding side driven by local OFI.
    #[serde(default)]
    pub ofi_max_widen_bps: f64,
    /// Venue + wire cancel latency (ms, e.g. `310` on Lighter Standard) priced as lookback option reserve.
    #[serde(default)]
    pub cancel_speed_bump_ms: u64,
    /// Multiplier on `sigma_per_sqrt_sec * sqrt(cancel_speed_bump_ms / 1000)`.
    #[serde(default = "default_cancel_option_mult")]
    pub cancel_option_mult: f64,
    /// Maximum reserve (bps) added by the cancel-speed-bump option term.
    #[serde(default = "default_cancel_option_max_bps")]
    pub cancel_option_max_bps: f64,
    /// Mark passive fills against local size-weighted microprice when external fair value is inactive.
    #[serde(default)]
    pub enable_local_markout: bool,
    /// Withdraw (`size = 0`) only the vulnerable inventory-adding side when normalized
    /// local BBO OFI (`[-1, 1]`) reaches or exceeds this magnitude.
    #[serde(default)]
    pub ofi_withdraw_threshold: Option<f64>,
    /// Pause (`size = 0`) inventory-adding sides when short-horizon local volatility
    /// `vol_bps` reaches or exceeds this cap, while preserving inventory-reducing quotes.
    #[serde(default)]
    pub vol_pause_bps: Option<f64>,
    /// Minimum hold duration (ms) after `LocalToxicityConfig` withdraws an entry side.
    #[serde(default)]
    pub withdraw_cooldown_ms: u64,
    /// Schmitt-trigger unlatch fraction `(0, 1]` relative to withdraw thresholds.
    #[serde(default)]
    pub withdraw_clear_ratio: Option<f64>,
}

impl Default for LocalToxicityConfig {
    fn default() -> Self {
        Self {
            ofi_half_life_ms: 0,
            ofi_max_widen_bps: 0.0,
            cancel_speed_bump_ms: 0,
            cancel_option_mult: default_cancel_option_mult(),
            cancel_option_max_bps: default_cancel_option_max_bps(),
            enable_local_markout: false,
            ofi_withdraw_threshold: None,
            vol_pause_bps: None,
            withdraw_cooldown_ms: 0,
            withdraw_clear_ratio: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuoteHysteresisConfig {
    pub cancel_min_profit_bps: f64,
    pub flatten_min_profit_bps: f64,
    #[serde(default = "default_flatten_age_start_fraction")]
    pub flatten_age_start_fraction: f64,
    #[serde(default)]
    pub per_side_bound_cancel: bool,
    /// Optional shorter minimum order lifetime (seconds) for the unambiguous
    /// inventory-reducing side while keeping `order_ttl_secs` on entries.
    #[serde(default)]
    pub reducing_order_ttl_secs: Option<u64>,
    /// Optional tighter price tolerance (bps) for re-anchoring reducing quotes.
    #[serde(default)]
    pub reducing_requote_threshold_bps: Option<f64>,
    /// Optional wider price tolerance (bps) for non-reducing entry orders inside `cancel_bounds`
    /// to preserve FIFO queue priority across sub-second BBO updates.
    #[serde(default)]
    pub entry_requote_threshold_bps: Option<f64>,
    /// Hold FIFO queue on benign/tightening drift within `entry_requote_threshold_bps`
    /// (without `calm_mode` 2x inflation), while retreating immediately at age 0 on
    /// adverse drift beyond `adverse_retreat_threshold_bps`.
    #[serde(default)]
    pub asymmetric_entry_ratchet: bool,
    /// Optional adverse-move price tolerance (bps) before immediately pulling an entry quote.
    #[serde(default)]
    pub adverse_retreat_threshold_bps: Option<f64>,
    /// Activate passive flattening immediately for any confirmed inventory >= step_size.
    #[serde(default)]
    pub zero_deadband_flatten: bool,
    /// Optional round-trip profit floor (bps over episode average entry + fees)
    /// held during the initial `breakeven_hold_fraction` of `position_timeout_secs`.
    #[serde(default)]
    pub breakeven_exit_bps: Option<f64>,
    #[serde(default)]
    pub breakeven_hold_fraction: f64,
    /// Optional causal local EMA momentum gate (bps) blocking only inventory-adding
    /// quotes during sharp directional cascades while keeping reducing quotes active.
    #[serde(default)]
    pub max_local_momentum_bps: Option<f64>,
    /// Anchor inventory-adding bounds to `min(pricing_mid, local_mid)` on bids and
    /// `max(pricing_mid, local_mid)` on asks so entries require edge against both books.
    #[serde(default)]
    pub conservative_dual_anchor: bool,
    /// Fraction `[0, 1)` of `position_timeout_secs` at which reducing quotes chase passive BBO touch.
    #[serde(default)]
    pub passive_touch_flatten_age_fraction: Option<f64>,
    /// Cooldown (ms) holding an entry side out after `adverse_retreat` triggers a forced cancel.
    #[serde(default)]
    pub adverse_retreat_reentry_cooldown_ms: u64,
    /// When `true`, suppress normal age/threshold cancellations on resting entry levels inside
    /// `cancel_bounds` & `adverse_retreat` bounds while re-entry cooldown is active (`S6-2`).
    #[serde(default)]
    pub cooldown_hold_safe_levels: bool,
    /// Optional cap on how many entry grid levels may be placed in a single cycle when
    /// replenishing from cooldown/flat (`1..=5`, `S6-2`).
    #[serde(default)]
    pub max_initial_placements_per_cycle: Option<usize>,
    /// When `true` alongside `max_initial_placements_per_cycle`, stage only initial entry
    /// from an empty side, exempt pure deeper grid fill-in (`to_cancel.is_empty()`) from
    /// 4s `MICRO_REFRESH_COOLDOWN` starvation, and prevent deeper fill-in from bypassing
    /// opposite-side cancel-deferral guards (`S7-2`).
    #[serde(default)]
    pub staged_reentry_only: bool,
    /// When `true`, enforce `reduce_only = true` + closable exposure cap on ANY confirmed
    /// inventory-reducing grid, preventing baseline non-`exit_touch` zero-crossing fills (`S7-2`).
    #[serde(default)]
    pub enforce_reduce_only_on_reducing_side: bool,
    /// When `true`, prevent one-sided adverse retreats, active re-entry cooldowns, and
    /// empty-side post-cooldown re-entries from disabling churn-deferral guards on the
    /// surviving opposite side (never suppresses safety/bound-breach/adverse-retreat cancels, `S12-1`/`S13-1`).
    ///
    /// **Empirically falsified (`S14-2`, 2026-11-19)**: even with `S13-1` directional cancel
    /// safety, Codex's 19-window × 3-horizon (`t20/t30/t50`) matrix showed `true` is dominated
    /// by `false` in 8/9 `(cohort, horizon)` cells (1 tied), increased total cancels in `monthly`
    /// and `w07`, and degraded 30s markout (`-1.03 -> -1.72 bp` on `monthly t50`) by delaying
    /// passive-side repricing/inventory unloading. Keep `false` (default) for new configs; field
    /// retained only for historical replay reproducibility.
    #[serde(default)]
    pub isolate_cooldown_cancel_churn: bool,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarkoutReserveConfig {
    pub max_widen_bps: f64,
    pub close_cost_bps: f64,
    pub max_sample_age_ms: u64,
    /// Optional empirical adverse markout quantile `p in (0.0, 0.5]` across `[1s, 5s, 30s, 60s]`
    /// enforcing a hard quote-distance floor on non-reducing entries (`S9-2`).
    #[serde(default)]
    pub quantile_floor_p: Option<f64>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrderFlowReserveConfig {
    pub half_life_ms: u64,
    pub max_widen_bps: f64,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CrossMarketReserveConfig {
    pub half_life_ms: u64,
    pub max_quote_skew_ms: u64,
    pub max_widen_bps: f64,
    /// One-way placement delay used to discount the signal through arrival.
    #[serde(default)]
    pub execution_delay_ms: u64,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolatilityRegimeConfig {
    pub range_trigger_bps: f64,
    pub max_widen_bps: f64,
    pub min_size_scale: f64,
}

fn positive(name: &str, values: &[f64]) -> anyhow::Result<()> {
    anyhow::ensure!(
        values.iter().all(|v| v.is_finite() && *v > 0.0),
        "{name}: values must be finite and positive"
    );
    Ok(())
}

fn nonnegative(name: &str, values: &[f64]) -> anyhow::Result<()> {
    anyhow::ensure!(
        values.iter().all(|v| v.is_finite() && *v >= 0.0),
        "{name}: values must be finite and nonnegative"
    );
    Ok(())
}

impl ExchangeConfig {
    fn validate(&self, name: &str) -> anyhow::Result<()> {
        positive(
            name,
            &[
                self.risk_fraction,
                self.tick_size,
                self.step_size,
                self.gamma,
                self.time_horizon_sec,
                self.stop_loss_pct,
                self.momentum_spread_mult,
            ],
        )?;
        nonnegative(
            name,
            &[
                self.min_spread_bps,
                self.vol_multiplier,
                self.momentum_threshold_bps,
                self.min_order_size,
                self.requote_threshold_bps,
            ],
        )?;
        anyhow::ensure!(
            self.risk_fraction <= 1.0 && self.stop_loss_pct < 1.0,
            "{name}: risk_fraction must be <= 1 and stop_loss_pct < 1"
        );
        anyhow::ensure!(
            self.vol_window >= 2 && self.requote_interval_ms > 0 && self.balance_refresh_secs > 0,
            "{name}: invalid sample window or update interval"
        );
        anyhow::ensure!(
            self.price_decimals.is_none_or(|v| v <= 12)
                && self.size_decimals.is_none_or(|v| v <= 12),
            "{name}: unsupported precision"
        );
        anyhow::ensure!(
            self.resolution != Some(0) && self.collateral_resolution != Some(0),
            "{name}: resolution must be positive"
        );
        if let Some(rate) = self.fee_rate {
            nonnegative(name, &[rate])?;
        }
        Ok(())
    }
}

impl InventoryNeutralMMConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        let name = "inventory_neutral_mm";
        positive(
            name,
            &[
                self.tick_size,
                self.step_size,
                self.base_order_size,
                self.max_position,
                self.inventory_urgency_threshold,
                self.reference_portfolio_value,
                self.min_portfolio_scale,
                self.max_portfolio_scale,
                self.flattening_cap_mult,
                self.as_gamma,
                self.as_kappa,
                self.as_time_horizon_sec,
                self.basis_tau_secs,
                self.markout_horizon_secs,
                self.markout_halflife_fills,
                self.max_spread_bps,
                self.max_leverage,
                self.sigmoid_steepness,
            ],
        )?;
        nonnegative(
            name,
            &[
                self.min_profit_bps,
                self.penny_ticks,
                self.inventory_skew_bps,
                self.grid_spacing_bps,
                self.base_order_notional_usd,
                self.max_position_notional_usd,
                self.inventory_urgency_notional_usd,
                self.min_inventory_notional_usd,
                self.vol_spread_scale,
                self.momentum_skew_scale,
                self.adverse_selection_threshold,
                self.requote_threshold_bps,
                self.min_available_balance,
                self.max_basis_bps,
            ],
        )?;
        anyhow::ensure!(
            self.maker_fee_bps.is_finite(),
            "{name}: maker fee must be finite"
        );
        anyhow::ensure!(
            self.max_daily_loss_pct.is_finite()
                && self.max_daily_loss_pct > 0.0
                && self.max_daily_loss_pct < 1.0,
            "{name}: max_daily_loss_pct must be in (0, 1)"
        );
        anyhow::ensure!(
            self.markout_threshold_bps.is_finite(),
            "{name}: markout_threshold_bps must be finite"
        );
        anyhow::ensure!(
            self.self_fill_alpha_bps.is_finite()
                && (0.0..=50.0).contains(&self.self_fill_alpha_bps),
            "{name}: self_fill_alpha_bps must be finite and in [0.0, 50.0]"
        );
        if self.self_fill_alpha_bps > 0.0 {
            anyhow::ensure!(
                self.self_fill_half_life_ms >= 50,
                "{name}: self_fill_half_life_ms must be >= 50 when self_fill_alpha_bps > 0.0"
            );
        }
        if let Some(reserve) = self.markout_reserve {
            anyhow::ensure!(
                reserve.max_widen_bps > 0.0
                    && reserve.max_widen_bps <= self.max_spread_bps
                    && (0.0..=10_000.0).contains(&reserve.close_cost_bps)
                    && reserve.max_sample_age_ms > 0
                    && reserve
                        .quantile_floor_p
                        .is_none_or(|p| p.is_finite() && p > 0.0 && p <= 0.5),
                "{name}: invalid directional markout reserve"
            );
        }
        if let Some(reserve) = self.order_flow_reserve {
            anyhow::ensure!(
                reserve.half_life_ms > 0
                    && reserve.max_widen_bps > 0.0
                    && reserve.max_widen_bps <= self.max_spread_bps,
                "{name}: invalid order flow reserve"
            );
        }
        if let Some(reserve) = self.cross_market_reserve {
            anyhow::ensure!(
                (1..=60_000).contains(&reserve.half_life_ms)
                    && reserve.max_quote_skew_ms <= 60_000
                    && reserve.execution_delay_ms <= 10_000
                    && reserve.max_widen_bps > 0.0
                    && reserve.max_widen_bps <= self.max_spread_bps,
                "{name}: invalid cross-market reserve"
            );
        }
        if let Some(regime) = self.volatility_regime {
            anyhow::ensure!(
                (0.01..=1_000.0).contains(&regime.range_trigger_bps)
                    && (0.0..=self.max_spread_bps).contains(&regime.max_widen_bps)
                    && (0.0..=1.0).contains(&regime.min_size_scale),
                "{name}: invalid volatility regime"
            );
        }
        if let Some(hysteresis) = self.quote_hysteresis {
            nonnegative(
                name,
                &[
                    hysteresis.cancel_min_profit_bps,
                    hysteresis.flatten_min_profit_bps,
                ],
            )?;
            anyhow::ensure!(
                hysteresis.cancel_min_profit_bps <= self.min_profit_bps
                    && hysteresis.flatten_min_profit_bps <= self.min_profit_bps
                    && hysteresis.flatten_age_start_fraction.is_finite()
                    && (0.0..=1.0).contains(&hysteresis.flatten_age_start_fraction)
                    && hysteresis
                        .reducing_order_ttl_secs
                        .is_none_or(|secs| secs > 0 && secs <= self.order_ttl_secs)
                    && hysteresis
                        .reducing_requote_threshold_bps
                        .is_none_or(|bps| bps.is_finite()
                            && bps > 0.0
                            && bps <= self.max_spread_bps)
                    && hysteresis
                        .entry_requote_threshold_bps
                        .is_none_or(|bps| bps.is_finite()
                            && bps > 0.0
                            && bps <= self.max_spread_bps)
                    && hysteresis
                        .adverse_retreat_threshold_bps
                        .is_none_or(|bps| bps.is_finite()
                            && bps > 0.0
                            && bps <= self.max_spread_bps)
                    && (!hysteresis.asymmetric_entry_ratchet
                        || hysteresis.entry_requote_threshold_bps.is_some()
                        || hysteresis.adverse_retreat_threshold_bps.is_some())
                    && hysteresis.breakeven_exit_bps.is_none_or(
                        |bps| bps.is_finite() && (-100.0..=self.max_spread_bps).contains(&bps)
                    )
                    && hysteresis.breakeven_hold_fraction.is_finite()
                    && (0.0..=1.0).contains(&hysteresis.breakeven_hold_fraction)
                    && hysteresis
                        .max_local_momentum_bps
                        .is_none_or(|bps| bps.is_finite() && (0.1..=1_000.0).contains(&bps))
                    && hysteresis
                        .passive_touch_flatten_age_fraction
                        .is_none_or(|f| f.is_finite() && (0.0..1.0).contains(&f))
                    && hysteresis.adverse_retreat_reentry_cooldown_ms <= 60_000
                    && hysteresis
                        .max_initial_placements_per_cycle
                        .is_none_or(|n| (1..=5).contains(&n))
                    && (!hysteresis.staged_reentry_only
                        || hysteresis.max_initial_placements_per_cycle.is_some())
                    && 2.0 * self.maker_fee_bps + hysteresis.cancel_min_profit_bps >= 0.0
                    && 2.0 * self.maker_fee_bps + hysteresis.flatten_min_profit_bps >= 0.0,
                "{name}: invalid quote hysteresis"
            );
        }
        if let Some(local_fv) = self.local_fair_value_config {
            anyhow::ensure!(
                local_fv.max_shift_bps.is_finite()
                    && local_fv.max_shift_bps > 0.0
                    && local_fv.max_shift_bps <= self.max_spread_bps,
                "{name}: invalid local fair value config"
            );
        }
        if let Some(tox) = self.local_toxicity {
            nonnegative(
                name,
                &[
                    tox.ofi_max_widen_bps,
                    tox.cancel_option_mult,
                    tox.cancel_option_max_bps,
                ],
            )?;
            anyhow::ensure!(
                tox.ofi_max_widen_bps <= self.max_spread_bps
                    && ((tox.ofi_max_widen_bps == 0.0 && tox.ofi_withdraw_threshold.is_none())
                        || (10..=60_000).contains(&tox.ofi_half_life_ms))
                    && tox
                        .ofi_withdraw_threshold
                        .is_none_or(|t| t.is_finite() && t > 0.0 && t <= 1.0)
                    && tox
                        .vol_pause_bps
                        .is_none_or(|v| v.is_finite() && (0.1..=1_000.0).contains(&v))
                    && tox.withdraw_cooldown_ms <= 60_000
                    && tox
                        .withdraw_clear_ratio
                        .is_none_or(|r| r.is_finite() && r > 0.0 && r <= 1.0)
                    && tox.cancel_speed_bump_ms <= 5_000
                    && (tox.cancel_speed_bump_ms == 0 || tox.cancel_option_max_bps > 0.0)
                    && tox.cancel_option_max_bps <= self.max_spread_bps,
                "{name}: invalid local toxicity config"
            );
        }
        anyhow::ensure!(
            self.min_portfolio_scale <= self.max_portfolio_scale,
            "{name}: portfolio scale bounds reversed"
        );
        anyhow::ensure!(
            (1..=5).contains(&self.grid_levels)
                && self.grid_size_decay.is_finite()
                && self.grid_size_decay > 0.0
                && self.grid_size_decay <= 1.0,
            "{name}: invalid grid"
        );
        anyhow::ensure!(
            self.micro_samples >= 2
                && self.ema_fast_period > 0
                && self.ema_slow_period >= self.ema_fast_period,
            "{name}: invalid microstructure windows"
        );
        anyhow::ensure!(
            self.poll_interval_ms > 0
                && self.min_cycle_interval_ms > 0
                && self.order_ttl_secs > 0
                && self.external_staleness_ms > 0
                && self.margin_cooldown_secs > 0
                && self.position_timeout_secs > 0
                && self.local_staleness_ms > 0,
            "{name}: intervals must be positive"
        );
        anyhow::ensure!(
            self.stale_bbo_cancel_ms >= self.local_staleness_ms,
            "{name}: stale_bbo_cancel_ms must be >= local_staleness_ms"
        );
        anyhow::ensure!(
            self.target_resting_margin_utilization.is_finite()
                && self.target_resting_margin_utilization > 0.0
                && self.target_resting_margin_utilization <= 1.0,
            "{name}: target_resting_margin_utilization must be in (0, 1]"
        );
        anyhow::ensure!(
            self.pre_liquidation_safety_multiple.is_finite()
                && self.pre_liquidation_safety_multiple > 1.0
                && self.pre_liquidation_safety_multiple <= 10.0,
            "{name}: pre_liquidation_safety_multiple must be in (1.0, 10.0]"
        );
        if let Some(mmf) = self.maintenance_margin_fraction {
            anyhow::ensure!(
                mmf.is_finite()
                    && mmf > 0.0
                    && mmf < 0.5
                    && mmf * self.pre_liquidation_safety_multiple < 1.0,
                "{name}: maintenance_margin_fraction must be in (0.0, 0.5) with mmf * safety_multiple < 1.0"
            );
        }
        if let Some(min_q) = self.min_volume_quota_for_quoting {
            anyhow::ensure!(
                min_q > 0,
                "{name}: min_volume_quota_for_quoting must be positive"
            );
        }
        if let Some(band_bps) = self.max_order_price_band_bps {
            anyhow::ensure!(
                band_bps.is_finite() && (5.0..=2_000.0).contains(&band_bps),
                "{name}: max_order_price_band_bps must be in [5.0, 2000.0]"
            );
        }
        if let Some(frac) = self.max_top_book_depth_fraction {
            anyhow::ensure!(
                frac.is_finite() && frac > 0.0 && frac <= 5.0,
                "{name}: max_top_book_depth_fraction must be in (0.0, 5.0]"
            );
        }
        if let Some(budget_bps) = self.as_risk_budget_bps_at_max_pos {
            anyhow::ensure!(
                budget_bps.is_finite() && budget_bps > 0.0 && budget_bps <= 500.0,
                "{name}: as_risk_budget_bps_at_max_pos must be in (0.0, 500.0]"
            );
        }
        if let Some(def) = self.defense_policy {
            anyhow::ensure!(
                def.caution_spread_bps.is_finite()
                    && def.reduce_only_spread_bps.is_finite()
                    && def.halt_spread_bps.is_finite()
                    && def.caution_widen_bps.is_finite()
                    && def.caution_size_scale.is_finite()
                    && def.side_defense_momentum_bps.is_finite()
                    && def.caution_spread_bps > 0.0
                    && def.caution_spread_bps <= def.reduce_only_spread_bps
                    && def.reduce_only_spread_bps <= def.halt_spread_bps
                    && def.halt_spread_bps <= 5_000.0
                    && def.caution_widen_bps >= 0.0
                    && def.caution_widen_bps <= self.max_spread_bps
                    && (0.05..=1.0).contains(&def.caution_size_scale)
                    && (0.1..=1_000.0).contains(&def.side_defense_momentum_bps)
                    && (100..=300_000).contains(&def.cooldown_ms),
                "{name}: invalid defense_policy"
            );
        }
        if let Some(unit_usd) = self.as_inventory_notional_usd {
            anyhow::ensure!(
                unit_usd.is_finite() && (1.0..=10_000_000.0).contains(&unit_usd),
                "{name}: as_inventory_notional_usd must be in [1.0, 10_000_000.0]"
            );
        }
        if let Some(reset_secs) = self.vol_gap_reset_secs {
            anyhow::ensure!(
                (10..=86_400).contains(&reset_secs),
                "{name}: vol_gap_reset_secs must be in [10, 86400]"
            );
        }
        let max_exch = crate::shm_reader::NUM_EXCHANGES as u8;
        anyhow::ensure!(
            (1..max_exch).contains(&self.exchange_id)
                && (1..MAX_SYMBOL_ID).contains(&self.symbol_id),
            "{name}: expected SHM exchange_id in 1..{} and SHM symbol ID in 1..{}",
            max_exch - 1,
            MAX_SYMBOL_ID - 1
        );
        if let Some(primary) = self.primary_reference_exchange {
            anyhow::ensure!(
                (1..max_exch).contains(&primary) && primary != self.exchange_id,
                "{name}: primary_reference_exchange {primary} must be in 1..{} and != exchange_id",
                max_exch - 1
            );
        }
        if let Some(refs) = &self.external_reference_exchanges {
            anyhow::ensure!(
                !refs.is_empty(),
                "{name}: external_reference_exchanges must not be empty when set"
            );
            let mut seen_refs = 0u16;
            for &exch in refs {
                anyhow::ensure!(
                    (1..max_exch).contains(&exch) && exch != self.exchange_id,
                    "{name}: external_reference_exchanges entry {exch} must be in 1..{} and != exchange_id",
                    max_exch - 1
                );
                let bit = 1u16 << exch;
                anyhow::ensure!(
                    seen_refs & bit == 0,
                    "{name}: duplicate exchange {exch} in external_reference_exchanges"
                );
                seen_refs |= bit;
            }
        }
        if self.exchange_id == EXCH_LIGHTER || self.exchange_id == EXCH_LIGHTER_ROBINHOOD {
            anyhow::ensure!(
                crate::exchanges::lighter::valid_market_id(self.market_id),
                "{name}: invalid Lighter market ID"
            );
        } else {
            anyhow::ensure!(
                self.market_id >= 0,
                "{name}: market_id must be non-negative"
            );
        }
        anyhow::ensure!(
            2.0 * self.maker_fee_bps + self.min_profit_bps >= 0.0
                && self.max_spread_bps >= 2.0 * self.maker_fee_bps + self.min_profit_bps,
            "{name}: maximum spread is below the fee/profit floor"
        );
        if self.max_position_notional_usd == 0.0 {
            anyhow::ensure!(
                self.step_size <= self.max_position,
                "{name}: step_size exceeds max_position"
            );
        } else if self.base_order_notional_usd > 0.0 {
            anyhow::ensure!(
                self.base_order_notional_usd <= self.max_position_notional_usd,
                "{name}: base_order_notional_usd exceeds max_position_notional_usd"
            );
        }
        if self.inventory_urgency_notional_usd == 0.0 || self.max_position_notional_usd == 0.0 {
            anyhow::ensure!(
                self.inventory_urgency_threshold <= self.max_position,
                "{name}: inventory urgency exceeds the position limit"
            );
        }
        if self.inventory_urgency_notional_usd > 0.0 && self.max_position_notional_usd > 0.0 {
            anyhow::ensure!(
                self.inventory_urgency_notional_usd <= self.max_position_notional_usd,
                "{name}: notional urgency exceeds the position limit"
            );
        }
        Ok(())
    }

    /// Stack-friendly bitmask of SHM exchange slots eligible for the external
    /// fair-value median. Defaults to Core Hyperliquid (1), Lighter Core (2),
    /// and Binance USD-M (6) so USDG/RWA slots never enter unless opted in.
    #[inline]
    pub fn external_reference_mask(&self) -> u16 {
        match &self.external_reference_exchanges {
            None => (1u16 << EXCH_HYPERLIQUID) | (1u16 << EXCH_LIGHTER) | (1u16 << EXCH_BINANCE),
            Some(list) => list.iter().fold(0u16, |acc, &id| {
                if (id as usize) < crate::shm_reader::NUM_EXCHANGES {
                    acc | (1u16 << id)
                } else {
                    acc
                }
            }),
        }
    }

    /// Zero-allocation stack copy of all scalar sizing/risk fields for hot-path
    /// per-cycle overrides (`external_reference_exchanges` is omitted).
    #[inline]
    pub fn runtime_copy(&self) -> Self {
        Self {
            exchange_id: self.exchange_id,
            symbol_id: self.symbol_id,
            market_id: self.market_id,
            tick_size: self.tick_size,
            step_size: self.step_size,
            maker_fee_bps: self.maker_fee_bps,
            min_profit_bps: self.min_profit_bps,
            penny_ticks: self.penny_ticks,
            inventory_skew_bps: self.inventory_skew_bps,
            grid_levels: self.grid_levels,
            grid_spacing_bps: self.grid_spacing_bps,
            grid_size_decay: self.grid_size_decay,
            base_order_size: self.base_order_size,
            max_position: self.max_position,
            inventory_urgency_threshold: self.inventory_urgency_threshold,
            base_order_notional_usd: self.base_order_notional_usd,
            round_base_notional_nearest: self.round_base_notional_nearest,
            max_position_notional_usd: self.max_position_notional_usd,
            inventory_urgency_notional_usd: self.inventory_urgency_notional_usd,
            min_inventory_notional_usd: self.min_inventory_notional_usd,
            reference_portfolio_value: self.reference_portfolio_value,
            min_portfolio_scale: self.min_portfolio_scale,
            max_portfolio_scale: self.max_portfolio_scale,
            sigmoid_steepness: self.sigmoid_steepness,
            flattening_cap_mult: self.flattening_cap_mult,
            micro_samples: self.micro_samples,
            ema_fast_period: self.ema_fast_period,
            ema_slow_period: self.ema_slow_period,
            use_depth_pricing: self.use_depth_pricing,
            vol_spread_scale: self.vol_spread_scale,
            momentum_skew_scale: self.momentum_skew_scale,
            external_staleness_ms: self.external_staleness_ms,
            require_external_fair_value: self.require_external_fair_value,
            external_reference_exchanges: None,
            primary_reference_exchange: self.primary_reference_exchange,
            basis_tau_secs: self.basis_tau_secs,
            max_basis_bps: self.max_basis_bps,
            as_gamma: self.as_gamma,
            as_time_horizon_sec: self.as_time_horizon_sec,
            as_kappa: self.as_kappa,
            max_spread_bps: self.max_spread_bps,
            adverse_selection_threshold: self.adverse_selection_threshold,
            requote_threshold_bps: self.requote_threshold_bps,
            order_ttl_secs: self.order_ttl_secs,
            max_leverage: self.max_leverage,
            min_available_balance: self.min_available_balance,
            margin_cooldown_secs: self.margin_cooldown_secs,
            max_daily_loss_pct: self.max_daily_loss_pct,
            position_timeout_secs: self.position_timeout_secs,
            local_staleness_ms: self.local_staleness_ms,
            stale_bbo_cancel_ms: self.stale_bbo_cancel_ms,
            target_resting_margin_utilization: self.target_resting_margin_utilization,
            maintenance_margin_fraction: self.maintenance_margin_fraction,
            pre_liquidation_safety_multiple: self.pre_liquidation_safety_multiple,
            use_post_only: self.use_post_only,
            poll_interval_ms: self.poll_interval_ms,
            min_cycle_interval_ms: self.min_cycle_interval_ms,
            use_markout_spread: self.use_markout_spread,
            markout_horizon_secs: self.markout_horizon_secs,
            markout_halflife_fills: self.markout_halflife_fills,
            markout_threshold_bps: self.markout_threshold_bps,
            markout_reserve: self.markout_reserve,
            order_flow_reserve: self.order_flow_reserve,
            cross_market_reserve: self.cross_market_reserve,
            volatility_regime: self.volatility_regime,
            quote_hysteresis: self.quote_hysteresis,
            self_fill_alpha_bps: self.self_fill_alpha_bps,
            self_fill_half_life_ms: self.self_fill_half_life_ms,
            local_fair_value_config: self.local_fair_value_config,
            local_toxicity: self.local_toxicity,
            min_volume_quota_for_quoting: self.min_volume_quota_for_quoting,
            max_order_price_band_bps: self.max_order_price_band_bps,
            defense_policy: self.defense_policy,
            as_inventory_notional_usd: self.as_inventory_notional_usd,
            vol_gap_reset_secs: self.vol_gap_reset_secs,
            normalize_cold_start_sigma_by_poll: self.normalize_cold_start_sigma_by_poll,
            cap_utilization_floor_to_base_notional: self.cap_utilization_floor_to_base_notional,
            max_top_book_depth_fraction: self.max_top_book_depth_fraction,
            as_risk_budget_bps_at_max_pos: self.as_risk_budget_bps_at_max_pos,
        }
    }

    /// Primary external SHM slot for directional order-flow / cross-market reserves.
    #[inline]
    pub fn primary_reference_exchange_id(&self) -> u8 {
        self.primary_reference_exchange.unwrap_or(EXCH_HYPERLIQUID)
    }

    /// Maintenance margin fraction used by runtime `lighter::risk::evaluate_margin_health`.
    #[inline]
    pub fn effective_maintenance_margin_fraction(&self) -> f64 {
        self.maintenance_margin_fraction.unwrap_or_else(|| {
            let from_lev = 0.25 / self.max_leverage;
            let below_imf = 0.80 / (self.max_leverage * self.pre_liquidation_safety_multiple);
            from_lev.min(below_imf).clamp(0.005, 0.25)
        })
    }
}

fn default_pre_liquidation_safety_multiple() -> f64 {
    1.8
}

impl Default for InventoryNeutralMMConfig {
    fn default() -> Self {
        Self {
            exchange_id: 2,
            symbol_id: SYM_ETH,
            market_id: 0,
            tick_size: 0.01,
            step_size: 0.0001,
            maker_fee_bps: 0.38,
            min_profit_bps: 1.0,
            penny_ticks: 1.0,
            inventory_skew_bps: 3.0,
            grid_levels: 3,
            grid_spacing_bps: 2.0,
            grid_size_decay: 0.7,
            sigmoid_steepness: 4.0,
            flattening_cap_mult: 2.0,
            micro_samples: 200,
            ema_fast_period: 5,
            ema_slow_period: 20,
            use_depth_pricing: true,
            vol_spread_scale: 0.5,
            momentum_skew_scale: 0.3,
            external_staleness_ms: 2000,
            require_external_fair_value: default_require_external_fair_value(),
            external_reference_exchanges: None,
            primary_reference_exchange: None,
            basis_tau_secs: default_basis_tau_secs(),
            max_basis_bps: default_max_basis_bps(),
            as_gamma: 5000.0,
            as_time_horizon_sec: 1.0,
            as_kappa: 2000.0,
            max_spread_bps: 60.0,
            base_order_size: 0.05,
            max_position: 0.15,
            inventory_urgency_threshold: 0.08,
            base_order_notional_usd: 0.0,
            round_base_notional_nearest: false,
            max_position_notional_usd: 0.0,
            inventory_urgency_notional_usd: 0.0,
            min_inventory_notional_usd: default_min_inventory_notional_usd(),
            reference_portfolio_value: 100.0,
            min_portfolio_scale: 0.5,
            max_portfolio_scale: 20.0,
            adverse_selection_threshold: 2.0,
            requote_threshold_bps: 1.5,
            order_ttl_secs: 5,
            max_leverage: 10.0,
            min_available_balance: 2.0,
            margin_cooldown_secs: 5,
            max_daily_loss_pct: default_max_daily_loss_pct(),
            position_timeout_secs: default_position_timeout_secs(),
            local_staleness_ms: default_local_staleness_ms(),
            stale_bbo_cancel_ms: default_stale_bbo_cancel_ms(),
            target_resting_margin_utilization: default_target_resting_margin_utilization(),
            maintenance_margin_fraction: None,
            pre_liquidation_safety_multiple: default_pre_liquidation_safety_multiple(),
            use_post_only: false,
            poll_interval_ms: 100,
            min_cycle_interval_ms: default_min_cycle_interval_ms(),
            use_markout_spread: false,
            markout_horizon_secs: default_markout_horizon_secs(),
            markout_halflife_fills: default_markout_halflife_fills(),
            markout_threshold_bps: default_markout_threshold_bps(),
            markout_reserve: None,
            order_flow_reserve: None,
            cross_market_reserve: None,
            volatility_regime: None,
            quote_hysteresis: None,
            self_fill_alpha_bps: 0.0,
            self_fill_half_life_ms: default_self_fill_half_life_ms(),
            local_fair_value_config: None,
            local_toxicity: None,
            min_volume_quota_for_quoting: None,
            max_order_price_band_bps: None,
            defense_policy: None,
            as_inventory_notional_usd: None,
            vol_gap_reset_secs: None,
            normalize_cold_start_sigma_by_poll: false,
            cap_utilization_floor_to_base_notional: false,
            max_top_book_depth_fraction: None,
            as_risk_budget_bps_at_max_pos: None,
        }
    }
}

/// Independent strategy program owning a profile's execution state, capital, and loss budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResearchScenario {
    QuantProfit,
    HedgeArbitrage,
    IncentiveTurnover,
}

impl ResearchScenario {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::QuantProfit => "quant_profit",
            Self::HedgeArbitrage => "hedge_arbitrage",
            Self::IncentiveTurnover => "incentive_turnover",
        }
    }
}

/// Optional deployment isolation metadata preventing Lighter cross-subaccount STP
/// collisions and cross-scenario capital/ledger contamination.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentIsolationConfig {
    /// Independent program owning this profile's risk and ledger budget.
    pub scenario: ResearchScenario,
    /// Logical L1 master-wallet group (e.g. `"primary_l1"`, `"turnover_l1"`).
    /// Lighter enforces Self-Trade Prevention (STP) across all subaccounts under the
    /// same L1 address, so two profiles in the same `l1_group` cannot trade the same market.
    pub l1_group: String,
    /// Logical Lighter subaccount slot label within `l1_group` (e.g. `"sub_eth_quant"`).
    #[serde(default)]
    pub subaccount_slot: Option<String>,
    /// Optional pinned Lighter integer account index checked against `.env` at launch.
    #[serde(default)]
    pub expected_account_index: Option<i64>,
    /// Optional logical hedge account/vault slot when `[inventory_hedge]` is active.
    #[serde(default)]
    pub hedge_account_slot: Option<String>,
}

impl DeploymentIsolationConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.l1_group.trim().is_empty(),
            "deployment.l1_group must be a non-empty identifier"
        );
        if let Some(slot) = &self.subaccount_slot {
            anyhow::ensure!(
                !slot.trim().is_empty(),
                "deployment.subaccount_slot must not be empty when set"
            );
        }
        if let Some(idx) = self.expected_account_index {
            anyhow::ensure!(
                idx >= 0,
                "deployment.expected_account_index must be non-negative"
            );
        }
        if let Some(hedge_slot) = &self.hedge_account_slot {
            anyhow::ensure!(
                !hedge_slot.trim().is_empty(),
                "deployment.hedge_account_slot must not be empty when set"
            );
        }
        Ok(())
    }
}

/// Top-level config file structure.
#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    /// Ticker -> global SHM symbol ID, shared with the Go feeder.
    #[serde(default = "default_symbols")]
    pub symbols: BTreeMap<String, u16>,
    /// Legacy optional `[backpack]` section retained for profile compatibility.
    #[serde(default)]
    pub backpack: Option<ExchangeConfig>,
    /// Legacy optional `[edgex]` section retained for profile compatibility.
    #[serde(default)]
    pub edgex: Option<ExchangeConfig>,
    #[serde(default)]
    pub inventory_neutral_mm: Option<InventoryNeutralMMConfig>,
    #[serde(default)]
    pub deployment: Option<DeploymentIsolationConfig>,
}

impl AppConfig {
    /// Load config from the given TOML file path.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: AppConfig = toml::from_str(&content)?;
        config.validate()?;
        Ok(config)
    }

    /// Load an explicit configuration. Missing, unreadable or invalid files must
    /// stop startup rather than silently replace risk limits with defaults.
    pub fn load_default() -> anyhow::Result<Self> {
        let path = Path::new("config.toml");
        Self::load(path).map_err(|error| {
            anyhow::anyhow!(
                "{}: {error}; copy config.example.toml and review its limits",
                path.display()
            )
        })
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        let mut seen = std::collections::BTreeSet::new();
        for (name, id) in &self.symbols {
            anyhow::ensure!(
                !name.is_empty() && (1..MAX_SYMBOL_ID).contains(id),
                "symbols.{name}: id {id} must be in 1..{}",
                MAX_SYMBOL_ID - 1
            );
            anyhow::ensure!(seen.insert(*id), "symbols: duplicate id {id}");
        }
        if let Some(backpack) = &self.backpack {
            backpack.validate("backpack")?;
        }
        if let Some(edgex) = &self.edgex {
            edgex.validate("edgex")?;
        }
        if let Some(config) = &self.inventory_neutral_mm {
            config.validate()?;
            anyhow::ensure!(
                self.symbol_name(config.symbol_id).is_some(),
                "inventory_neutral_mm: symbol_id {} is not in [symbols]",
                config.symbol_id
            );
        }
        if let Some(deployment) = &self.deployment {
            deployment.validate()?;
        }
        Ok(())
    }

    /// Validates that a fleet of concurrent Lighter profiles is free of cross-subaccount
    /// Self-Trade Prevention (STP) collisions, shared subaccount locks, and cross-scenario
    /// budget sharing. Profiles omitting `[deployment]` default to `"default_l1"`.
    pub fn validate_fleet_isolation<'a>(
        profiles: impl IntoIterator<Item = (&'a str, &'a Self)>,
    ) -> anyhow::Result<()> {
        let items: Vec<(&str, &Self)> = profiles.into_iter().collect();
        for i in 0..items.len() {
            let (name_a, cfg_a) = items[i];
            cfg_a
                .validate()
                .map_err(|e| anyhow::anyhow!("{name_a}: {e}"))?;
            let Some(mm_a) = &cfg_a.inventory_neutral_mm else {
                continue;
            };
            let sym_a = cfg_a.symbol_name(mm_a.symbol_id).unwrap_or("UNKNOWN");
            let l1_a = cfg_a
                .deployment
                .as_ref()
                .map_or("default_l1", |d| d.l1_group.as_str());

            for &(name_b, cfg_b) in &items[(i + 1)..] {
                let Some(mm_b) = &cfg_b.inventory_neutral_mm else {
                    continue;
                };
                let sym_b = cfg_b.symbol_name(mm_b.symbol_id).unwrap_or("UNKNOWN");
                let l1_b = cfg_b
                    .deployment
                    .as_ref()
                    .map_or("default_l1", |d| d.l1_group.as_str());

                // 1. Pinned Lighter integer account_index cannot be shared by two active processes.
                if let (Some(idx_a), Some(idx_b)) = (
                    cfg_a
                        .deployment
                        .as_ref()
                        .and_then(|d| d.expected_account_index),
                    cfg_b
                        .deployment
                        .as_ref()
                        .and_then(|d| d.expected_account_index),
                ) {
                    anyhow::ensure!(
                        idx_a != idx_b,
                        "Account collision between {name_a} and {name_b}: both bind expected_account_index={idx_a}"
                    );
                }

                // 2. Owner-wide self-trade & wash-trade block:
                //    Lighter native STP only prevents self-trades within a single account
                //    (not across subaccounts), and Lighter Terms & Points rules prohibit
                //    wash/self-trading across ANY commonly-controlled accounts regardless
                //    of whether they share an L1 wallet (`l1_group`) or use separate wallets.
                //    Therefore, no two profiles in an operator-controlled fleet on the same
                //    venue domain may trade the same `market_id` or `symbol` simultaneously.
                if mm_a.exchange_id == mm_b.exchange_id {
                    anyhow::ensure!(
                        mm_a.market_id != mm_b.market_id
                            && mm_a.symbol_id != mm_b.symbol_id
                            && sym_a != sym_b,
                        "Common-control self-trade / wash-trade market collision between {name_a} ({sym_a}, market_id={}, l1_group='{l1_a}') and {name_b} ({sym_b}, market_id={}, l1_group='{l1_b}'): operator-controlled fleet profiles cannot trade the same venue market simultaneously even across separate subaccounts or L1 wallets",
                        mm_a.market_id,
                        mm_b.market_id
                    );
                }
                if l1_a == l1_b
                    && mm_a.exchange_id == mm_b.exchange_id
                    && let (Some(slot_a), Some(slot_b)) = (
                        cfg_a
                            .deployment
                            .as_ref()
                            .and_then(|d| d.subaccount_slot.as_deref()),
                        cfg_b
                            .deployment
                            .as_ref()
                            .and_then(|d| d.subaccount_slot.as_deref()),
                    )
                {
                    anyhow::ensure!(
                        slot_a != slot_b,
                        "Subaccount slot collision in L1 group '{l1_a}' between {name_a} and {name_b}: both claim subaccount_slot='{slot_a}'"
                    );
                }
            }
        }
        Ok(())
    }

    /// Ticker for a configured symbol ID.
    pub fn symbol_name(&self, symbol_id: u16) -> Option<&str> {
        self.symbols
            .iter()
            .find(|(_, id)| **id == symbol_id)
            .map(|(name, _)| name.as_str())
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            symbols: default_symbols(),
            backpack: None,
            edgex: None,
            inventory_neutral_mm: Some(InventoryNeutralMMConfig::default()),
            deployment: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_round_to_tick() {
        assert!((round_to_tick(100.123, 0.01) - 100.12).abs() < 1e-10);
        assert!((round_to_tick(100.126, 0.01) - 100.13).abs() < 1e-10);
        assert!((round_to_tick(100.5, 0.1) - 100.5).abs() < 1e-10);
        assert!((round_to_tick(100.54, 0.1) - 100.5).abs() < 1e-10);
        assert!((round_to_tick(100.56, 0.1) - 100.6).abs() < 1e-10);
        assert!((round_to_tick(0.123456, 0.0001) - 0.1235).abs() < 1e-10);
    }

    #[test]
    fn test_format_price() {
        assert_eq!(format_price(100.123, 0.01), "100.12");
        assert_eq!(format_price(100.126, 0.01), "100.13");
        assert_eq!(format_price(0.123456, 0.0001), "0.1235");
        assert_eq!(format_price(1234.5, 0.1), "1234.5");
        assert_eq!(format_price(1234.56, 1.0), "1235");
        assert_eq!(format_price(100.25, 0.25), "100.25");
        assert_eq!(format_price(100.125, 0.125), "100.125");
    }

    #[test]
    fn test_format_size() {
        assert_eq!(format_size(1.234, 0.01), "1.23");
        assert_eq!(format_size(1.236, 0.01), "1.24");
        assert_eq!(format_size(0.123456, 0.001), "0.123");
        assert_eq!(format_size(10.5, 0.1), "10.5");
    }

    #[test]
    fn test_default_config_and_legacy_optional_sections() {
        let cfg = AppConfig::default();
        assert!(cfg.backpack.is_none());
        assert!(cfg.edgex.is_none());
        assert!(cfg.inventory_neutral_mm.is_some());

        // Legacy profiles that still carry [backpack] or [edgex] continue to parse and validate.
        let legacy: AppConfig = toml::from_str(
            r#"
            [backpack]
            risk_fraction = 0.10
            min_spread_bps = 6.0
            vol_multiplier = 2.5
            stop_loss_pct = 0.005
            requote_interval_ms = 2000
            "#,
        )
        .unwrap();
        legacy.validate().unwrap();
        let bp = legacy.backpack.unwrap();
        assert_eq!(bp.tick_size, 0.01);
        assert_eq!(bp.step_size, 0.01);
        assert_eq!(bp.gamma, 0.1);
        assert_eq!(bp.time_horizon_sec, 60.0);
    }
    #[test]
    fn example_and_default_configuration_validate() {
        let config: AppConfig = toml::from_str(include_str!("../config.example.toml")).unwrap();
        config.validate().unwrap();
        AppConfig::default().validate().unwrap();
    }

    #[test]
    fn invalid_strategy_limits_are_rejected() {
        for value in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let config = InventoryNeutralMMConfig {
                tick_size: value,
                ..Default::default()
            };
            assert!(config.validate().is_err());
        }
        let mut config = InventoryNeutralMMConfig {
            min_portfolio_scale: 10.0,
            max_portfolio_scale: 1.0,
            ..Default::default()
        };
        assert!(config.validate().is_err());
        config = InventoryNeutralMMConfig {
            as_kappa: 0.0,
            ..Default::default()
        };
        assert!(config.validate().is_err());
        config = InventoryNeutralMMConfig {
            max_spread_bps: 0.5,
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn symbol_table_defaults_and_custom_entries() {
        let app = AppConfig::default();
        assert_eq!(app.symbol_name(SYM_BTC), Some("BTC"));
        assert_eq!(app.symbol_name(SYM_ETH), Some("ETH"));
        assert_eq!(app.symbol_name(1003), None);

        let mut app = AppConfig::default();
        app.symbols.insert("SOL".into(), 1003);
        app.inventory_neutral_mm = Some(InventoryNeutralMMConfig {
            symbol_id: 1003,
            ..Default::default()
        });
        assert!(app.validate().is_ok());
        assert_eq!(app.symbol_name(1003), Some("SOL"));

        // The strategy symbol must be registered.
        app.symbols.remove("SOL");
        assert!(app.validate().is_err());
    }

    #[test]
    fn invalid_symbol_tables_are_rejected() {
        for (name, id) in [("SOL", 0u16), ("SOL", MAX_SYMBOL_ID), ("DUP", SYM_ETH)] {
            let mut app = AppConfig::default();
            app.symbols.insert(name.into(), id);
            assert!(app.validate().is_err(), "{name}={id}");
        }
    }

    #[test]
    fn risk_knobs_default_to_previous_constants_and_are_validated() {
        let config = InventoryNeutralMMConfig::default();
        assert_eq!(config.max_daily_loss_pct, 0.05);
        assert_eq!(config.position_timeout_secs, 60);
        assert_eq!(config.local_staleness_ms, 1000);
        assert_eq!(config.stale_bbo_cancel_ms, 2500);
        assert!(config.require_external_fair_value);
        assert_eq!(
            config.external_reference_mask(),
            (1u16 << EXCH_HYPERLIQUID) | (1u16 << EXCH_LIGHTER) | (1u16 << EXCH_BINANCE)
        );
        assert_eq!(config.target_resting_margin_utilization, 0.35);
        assert_eq!(config.max_basis_bps, 20.0);
        assert_eq!(config.min_cycle_interval_ms, 50);

        let invalid = [
            InventoryNeutralMMConfig {
                max_daily_loss_pct: 0.0,
                ..Default::default()
            },
            InventoryNeutralMMConfig {
                max_daily_loss_pct: 1.0,
                ..Default::default()
            },
            InventoryNeutralMMConfig {
                position_timeout_secs: 0,
                ..Default::default()
            },
            InventoryNeutralMMConfig {
                stale_bbo_cancel_ms: 500,
                ..Default::default()
            },
            InventoryNeutralMMConfig {
                target_resting_margin_utilization: 1.5,
                ..Default::default()
            },
            InventoryNeutralMMConfig {
                max_basis_bps: -1.0,
                ..Default::default()
            },
            InventoryNeutralMMConfig {
                basis_tau_secs: 0.0,
                ..Default::default()
            },
        ];
        for config in invalid {
            assert!(config.validate().is_err(), "{config:?}");
        }
    }

    #[test]
    fn invalid_file_is_not_replaced_by_defaults() {
        assert!(AppConfig::load(Path::new("/nonexistent/aleph-config.toml")).is_err());
        let text = include_str!("../config.example.toml")
            .replace("poll_interval_ms = 300", "poll_interval_typo = 300");
        assert!(toml::from_str::<AppConfig>(&text).is_err());
    }

    #[test]
    fn fleet_isolation_rejects_same_market_across_any_l1_and_accepts_disjoint_markets() {
        let eth_quant = AppConfig {
            deployment: Some(DeploymentIsolationConfig {
                scenario: ResearchScenario::QuantProfit,
                l1_group: "primary_l1".into(),
                subaccount_slot: Some("sub_1_eth_quant".into()),
                expected_account_index: Some(101),
                hedge_account_slot: None,
            }),
            ..AppConfig::default()
        };

        let mut btc_hedge = AppConfig::default();
        if let Some(mm) = &mut btc_hedge.inventory_neutral_mm {
            mm.symbol_id = SYM_BTC;
            mm.market_id = 1;
        }
        btc_hedge.deployment = Some(DeploymentIsolationConfig {
            scenario: ResearchScenario::HedgeArbitrage,
            l1_group: "primary_l1".into(),
            subaccount_slot: Some("sub_2_btc_hedge".into()),
            expected_account_index: Some(102),
            hedge_account_slot: Some("hl_btc_hedge".into()),
        });

        let mut sol_turnover = AppConfig::default();
        sol_turnover.symbols.insert("SOL".into(), 1003);
        if let Some(mm) = &mut sol_turnover.inventory_neutral_mm {
            mm.symbol_id = 1003;
            mm.market_id = 2;
        }
        sol_turnover.deployment = Some(DeploymentIsolationConfig {
            scenario: ResearchScenario::IncentiveTurnover,
            l1_group: "turnover_l1".into(),
            subaccount_slot: Some("sub_1_sol_turnover".into()),
            expected_account_index: Some(201),
            hedge_account_slot: None,
        });

        // Disjoint markets pass fleet isolation across both same and separate L1 groups.
        AppConfig::validate_fleet_isolation([
            ("eth_quant", &eth_quant),
            ("btc_hedge", &btc_hedge),
            ("sol_turnover", &sol_turnover),
        ])
        .unwrap();

        // Same market under a DIFFERENT L1 wallet MUST FAIL common-control wash/self-trade block!
        let eth_turnover_other_l1 = AppConfig {
            deployment: Some(DeploymentIsolationConfig {
                scenario: ResearchScenario::IncentiveTurnover,
                l1_group: "turnover_l1".into(),
                subaccount_slot: Some("sub_1_eth_turnover".into()),
                expected_account_index: Some(201),
                hedge_account_slot: None,
            }),
            ..AppConfig::default()
        };
        let err_other_l1 = AppConfig::validate_fleet_isolation([
            ("eth_quant", &eth_quant),
            ("eth_turnover", &eth_turnover_other_l1),
        ])
        .unwrap_err();
        assert!(
            err_other_l1
                .to_string()
                .contains("Common-control self-trade / wash-trade market collision"),
            "{err_other_l1}"
        );

        // Same market under the SAME L1 wallet also fails!
        let mut eth_collision = eth_turnover_other_l1.clone();
        eth_collision.deployment.as_mut().unwrap().l1_group = "primary_l1".into();
        let err_same_l1 = AppConfig::validate_fleet_isolation([
            ("eth_quant", &eth_quant),
            ("eth_collision", &eth_collision),
        ])
        .unwrap_err();
        assert!(
            err_same_l1
                .to_string()
                .contains("Common-control self-trade / wash-trade market collision"),
            "{err_same_l1}"
        );

        // Duplicate expected_account_index fails even across disjoint markets and L1 labels!
        let mut dup_idx = sol_turnover.clone();
        dup_idx.deployment.as_mut().unwrap().expected_account_index = Some(101);
        assert!(
            AppConfig::validate_fleet_isolation([("eth_quant", &eth_quant), ("dup_idx", &dup_idx)])
                .is_err()
        );
    }

    #[test]
    fn self_fill_alpha_config_defaults_runtime_copy_and_validation() {
        let default_cfg = InventoryNeutralMMConfig::default();
        assert_eq!(default_cfg.self_fill_alpha_bps, 0.0);
        assert_eq!(default_cfg.self_fill_half_life_ms, 2000);

        let disabled_zero_hl = InventoryNeutralMMConfig {
            self_fill_alpha_bps: 0.0,
            self_fill_half_life_ms: 0,
            ..Default::default()
        };
        assert!(disabled_zero_hl.validate().is_ok());

        let valid_cfg = InventoryNeutralMMConfig {
            self_fill_alpha_bps: 3.5,
            self_fill_half_life_ms: 750,
            ..Default::default()
        };
        assert!(valid_cfg.validate().is_ok());
        let copy = valid_cfg.runtime_copy();
        assert_eq!(copy.self_fill_alpha_bps, 3.5);
        assert_eq!(copy.self_fill_half_life_ms, 750);

        for bad_alpha in [-0.01, 50.01, f64::NAN, f64::INFINITY] {
            let cfg = InventoryNeutralMMConfig {
                self_fill_alpha_bps: bad_alpha,
                ..Default::default()
            };
            assert!(cfg.validate().is_err(), "bad_alpha={bad_alpha}");
        }
        let too_short_half_life = InventoryNeutralMMConfig {
            self_fill_alpha_bps: 1.0,
            self_fill_half_life_ms: 49,
            ..Default::default()
        };
        assert!(too_short_half_life.validate().is_err());
    }

    #[test]
    fn altcoin_usd_notional_profiles_validate_when_step_size_exceeds_legacy_eth_base_defaults() {
        let mut xrp = InventoryNeutralMMConfig {
            step_size: 1.0,
            tick_size: 0.0001,
            max_position: 0.2,
            inventory_urgency_threshold: 0.08,
            base_order_notional_usd: 100.0,
            max_position_notional_usd: 150.0,
            inventory_urgency_notional_usd: 75.0,
            round_base_notional_nearest: true,
            ..Default::default()
        };
        xrp.validate().unwrap();
        assert!(xrp.runtime_copy().round_base_notional_nearest);

        xrp.base_order_notional_usd = 200.0;
        assert!(xrp.validate().is_err());

        let legacy_no_notional = InventoryNeutralMMConfig {
            step_size: 1.0,
            tick_size: 0.0001,
            max_position: 0.2,
            inventory_urgency_threshold: 0.08,
            ..Default::default()
        };
        assert!(legacy_no_notional.validate().is_err());

        // Decoupled strategy validates cleanly for non-Lighter SHM venues while rejecting 0 / out-of-range.
        for exch in [EXCH_ARCUS, EXCH_BACKPACK, EXCH_EXTENDED] {
            let mut other = InventoryNeutralMMConfig {
                exchange_id: exch,
                market_id: 999_999,
                min_volume_quota_for_quoting: Some(10),
                max_order_price_band_bps: Some(50.0),
                defense_policy: Some(DefensePolicyConfig {
                    caution_spread_bps: 8.0,
                    caution_widen_bps: 2.0,
                    caution_size_scale: 0.5,
                    side_defense_momentum_bps: 6.0,
                    reduce_only_spread_bps: 20.0,
                    halt_spread_bps: 50.0,
                    cooldown_ms: 2_000,
                }),
                ..Default::default()
            };
            other.validate().unwrap();
            other.exchange_id = 0;
            assert!(other.validate().is_err());
            other.exchange_id = crate::shm_reader::NUM_EXCHANGES as u8;
            assert!(other.validate().is_err());
        }
    }
}
