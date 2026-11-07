//! Central configuration for AlephTX strategies.
//!
//! Loads from `config.toml` at the project root.
//! All trading parameters are runtime-configurable — no recompilation needed.

pub const EXCH_LIGHTER: u8 = 2;
pub const EXCH_EDGEX: u8 = 3;
pub const EXCH_HYPERLIQUID: u8 = 1;
pub const EXCH_BACKPACK: u8 = 5;

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
    5000
}
fn default_stale_bbo_cancel_ms() -> u64 {
    15000
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
    // Post-Only: use ALO (Add Liquidity Only) to guarantee maker fees
    #[serde(default)]
    pub use_post_only: bool, // default: false (GTC), true = Post-Only
    // Poll interval for main loop (ms)
    #[serde(default = "default_poll_interval_ms")]
    pub poll_interval_ms: u64, // default: 100
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
            self.exchange_id == EXCH_LIGHTER && (1..MAX_SYMBOL_ID).contains(&self.symbol_id),
            "{name}: expected Lighter and an SHM symbol ID in 1..{}",
            MAX_SYMBOL_ID - 1
        );
        anyhow::ensure!(
            crate::exchanges::lighter::valid_market_id(self.market_id),
            "{name}: invalid Lighter market ID"
        );
        anyhow::ensure!(
            2.0 * self.maker_fee_bps + self.min_profit_bps >= 0.0
                && self.max_spread_bps >= 2.0 * self.maker_fee_bps + self.min_profit_bps,
            "{name}: maximum spread is below the fee/profit floor"
        );
        anyhow::ensure!(
            self.step_size <= self.max_position
                && self.inventory_urgency_threshold <= self.max_position,
            "{name}: inventory urgency exceeds the position limit"
        );
        if self.inventory_urgency_notional_usd > 0.0 && self.max_position_notional_usd > 0.0 {
            anyhow::ensure!(
                self.inventory_urgency_notional_usd <= self.max_position_notional_usd,
                "{name}: notional urgency exceeds the position limit"
            );
        }
        Ok(())
    }
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
            use_post_only: false,
            poll_interval_ms: 100,
        }
    }
}

/// Top-level config file structure.
#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    /// Ticker -> global SHM symbol ID, shared with the Go feeder.
    #[serde(default = "default_symbols")]
    pub symbols: BTreeMap<String, u16>,
    pub backpack: ExchangeConfig,
    pub edgex: ExchangeConfig,
    #[serde(default)]
    pub inventory_neutral_mm: Option<InventoryNeutralMMConfig>,
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
        self.backpack.validate("backpack")?;
        self.edgex.validate("edgex")?;
        if let Some(config) = &self.inventory_neutral_mm {
            config.validate()?;
            anyhow::ensure!(
                self.symbol_name(config.symbol_id).is_some(),
                "inventory_neutral_mm: symbol_id {} is not in [symbols]",
                config.symbol_id
            );
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
            backpack: ExchangeConfig {
                risk_fraction: 0.10,
                min_spread_bps: 12.0,
                vol_multiplier: 3.0,
                stop_loss_pct: 0.003,
                requote_interval_ms: 2000,
                momentum_threshold_bps: 8.0,
                momentum_spread_mult: 2.0,
                vol_window: 120,
                balance_refresh_secs: 60,
                min_order_size: 0.0,
                tick_size: 0.01,
                step_size: 0.01,
                gamma: 0.1,
                time_horizon_sec: 60.0,
                requote_threshold_bps: 2.0,
                contract_id: None,
                synthetic_asset_id: None,
                collateral_asset_id: None,
                fee_asset_id: None,
                price_decimals: None,
                size_decimals: None,
                resolution: None,
                collateral_resolution: None,
                fee_rate: None,
            },
            edgex: ExchangeConfig {
                risk_fraction: 0.08,
                min_spread_bps: 20.0,
                vol_multiplier: 3.5,
                stop_loss_pct: 0.003,
                requote_interval_ms: 3000,
                momentum_threshold_bps: 8.0,
                momentum_spread_mult: 2.0,
                vol_window: 120,
                balance_refresh_secs: 60,
                min_order_size: 0.1,
                tick_size: 0.01,
                step_size: 0.01,
                gamma: 0.1,
                time_horizon_sec: 60.0,
                requote_threshold_bps: 2.0,
                contract_id: Some(1),
                synthetic_asset_id: Some("0x4554482d3130000000000000000000".to_string()),
                collateral_asset_id: Some("0x555344432d36000000000000000000".to_string()),
                fee_asset_id: Some("0x555344432d36000000000000000000".to_string()),
                price_decimals: Some(2),
                size_decimals: Some(4),
                resolution: Some(1000000000),
                collateral_resolution: Some(1000000),
                fee_rate: Some(0.0005),
            },
            inventory_neutral_mm: Some(InventoryNeutralMMConfig::default()),
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
    fn test_default_config_has_new_fields() {
        let cfg = AppConfig::default();
        assert_eq!(cfg.backpack.tick_size, 0.01);
        assert_eq!(cfg.backpack.step_size, 0.01);
        assert_eq!(cfg.backpack.gamma, 0.1);
        assert_eq!(cfg.backpack.time_horizon_sec, 60.0);
        assert_eq!(cfg.edgex.tick_size, 0.01);
        assert_eq!(cfg.edgex.gamma, 0.1);
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
        assert_eq!(config.local_staleness_ms, 5000);
        assert_eq!(config.stale_bbo_cancel_ms, 15000);
        assert_eq!(config.target_resting_margin_utilization, 0.35);
        assert_eq!(config.max_basis_bps, 20.0);

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
                stale_bbo_cancel_ms: 1000,
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
}
