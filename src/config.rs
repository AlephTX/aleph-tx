//! Central configuration for AlephTX strategies.
//!
//! Loads from `config.toml` at the project root.
//! All trading parameters are runtime-configurable — no recompilation needed.

use serde::Deserialize;
use std::path::Path;

/// Round value to nearest tick/step size
#[inline]
pub fn round_to_tick(val: f64, tick: f64) -> f64 {
    (val / tick).round() * tick
}

/// Format price with dynamic precision based on tick size
pub fn format_price(price: f64, tick_size: f64) -> String {
    let decimals = (-tick_size.log10()).ceil().max(0.0) as usize;
    format!("{:.prec$}", round_to_tick(price, tick_size), prec = decimals)
}

/// Format size with dynamic precision based on step size
pub fn format_size(size: f64, step_size: f64) -> String {
    let decimals = (-step_size.log10()).ceil().max(0.0) as usize;
    format!("{:.prec$}", round_to_tick(size, step_size), prec = decimals)
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
    2.0  // 2 bps deviation threshold
}

/// Top-level config file structure.
#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub backpack: ExchangeConfig,
    pub edgex: ExchangeConfig,
}

impl AppConfig {
    /// Load config from the given TOML file path.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: AppConfig = toml::from_str(&content)?;
        Ok(config)
    }

    /// Load from the default location (project root config.toml).
    pub fn load_default() -> Self {
        // Try multiple paths
        let candidates = [
            "config.toml",
            concat!(env!("CARGO_MANIFEST_DIR"), "/config.toml"),
        ];

        for path in &candidates {
            if let Ok(cfg) = Self::load(Path::new(path)) {
                tracing::info!("📋 Loaded config from {}", path);
                return cfg;
            }
        }

        tracing::warn!("⚠️ No config.toml found, using defaults");
        Self::default()
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
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
            },
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
}
