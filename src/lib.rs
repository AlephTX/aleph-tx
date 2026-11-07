pub mod account_stats_reader;
pub mod capture;
pub mod config;
pub mod data_plane;
pub mod error;
pub mod exchange;
pub mod exchanges;
pub mod funding_evidence;
pub mod hedging;
pub mod live_hedge;
pub mod order_tracker;
pub mod shadow_ledger;
pub mod shm_depth_reader;
pub mod shm_event_reader;
pub mod shm_path;
pub mod shm_reader;
pub mod strategy;
pub mod telemetry;
pub mod telemetry_sink;
pub mod types;
pub mod venues;

// Re-export for backward compatibility (callers can migrate incrementally)
pub use exchanges::backpack as backpack_api;
pub use exchanges::edgex as edgex_api;
pub use exchanges::lighter::ffi as lighter_ffi;
pub use exchanges::lighter::trading as lighter_trading;
