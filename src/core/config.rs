//! Configuration - Type-safe, validated config

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Application configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Application settings
    pub app: AppConfig,

    /// Exchange configurations
    pub exchanges: Vec<ExchangeConfig>,

    /// Trading settings
    pub trading: TradingConfig,

    /// Risk management
    pub risk: RiskConfig,

    /// Telegram bot
    pub telegram: Option<TelegramConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// Run mode: paper or live
    pub mode: RunMode,

    /// Log level
    pub log_level: String,

    /// Data directory
    pub data_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunMode {
    Paper,
    Live,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExchangeConfig {
    /// Exchange ID (binance, okx, edgex, etc.)
    pub id: String,

    /// API key (loaded from env if not provided)
    pub api_key: Option<String>,

    /// API secret (loaded from env if not provided)
    pub api_secret: Option<String>,

    /// Use testnet
    pub testnet: bool,

    /// Enable this exchange
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradingConfig {
    /// Trading symbols
    pub symbols: Vec<String>,

    /// Max positions
    pub max_positions: usize,

    /// Default order size (as fraction of balance)
    pub order_size_fraction: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskConfig {
    /// Max portfolio risk (0.0-1.0)
    pub max_portfolio_risk: f64,

    /// Max position risk (0.0-1.0)
    pub max_position_risk: f64,

    /// Max drawdown % before stopping
    pub max_drawdown: f64,

    /// Enable emergency stop
    pub emergency_stop: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramConfig {
    /// Bot token
    pub bot_token: String,

    /// Allowed user IDs (empty = allow all)
    pub allowed_users: Vec<String>,

    /// Enable notifications
    pub notifications: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            app: AppConfig {
                mode: RunMode::Paper,
                log_level: "info".to_string(),
                data_dir: None,
            },
            exchanges: vec![],
            trading: TradingConfig {
                symbols: vec!["BTC/USDT".to_string()],
                max_positions: 3,
                order_size_fraction: 0.1,
            },
            risk: RiskConfig {
                max_portfolio_risk: 0.2,
                max_position_risk: 0.1,
                max_drawdown: 0.15,
                emergency_stop: true,
            },
            telegram: None,
        }
    }
}

impl Config {
    /// Load from TOML file
    pub fn load(path: &PathBuf) -> crate::core::Result<Self> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| crate::core::Error::Config(format!("Failed to read config: {}", e)))?;

        toml::from_str(&content)
            .map_err(|e| crate::core::Error::Config(format!("Failed to parse config: {}", e)))
    }

    /// Get exchange config by ID
    pub fn exchange(&self, id: &str) -> Option<&ExchangeConfig> {
        self.exchanges.iter().find(|e| e.id == id)
    }
}
