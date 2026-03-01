//! Central configuration for AlephTX strategies.
//!
//! Loads from `config.toml` at the project root.
//! All trading parameters are runtime-configurable — no recompilation needed.

use serde::Deserialize;
use std::path::Path;

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
            },
        }
    }
}
