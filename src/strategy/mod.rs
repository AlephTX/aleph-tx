pub mod arcus_xyz_basis;
pub mod basis_core;
pub mod basis_maker_hedge;
pub mod basis_replay;
pub mod cross_sectional;
pub mod cross_venue;
pub mod cross_venue_router;
pub mod defense_fsm;
pub mod directional_alpha;
pub mod funding_basis_carry;
pub mod glft;
pub mod hybrid_exit;
pub mod inventory_neutral_mm;
pub mod kalman_basis;
pub mod lead_lag_taker;
pub mod matched_settlement;
pub mod microprice;
pub mod multiday_carry;
pub mod pipeline_driver;
pub mod pipeline_tape;
pub mod portfolio_risk;
pub mod queue_priority_mm;
pub mod quote_pipeline;
pub mod relative_value;
mod replay_queue_depth;
pub mod slow_cascade;
pub mod stat_arb;
pub mod tail_risk;
pub mod track_coordinator;
pub mod track_shadow;
pub mod venue_guards;
pub mod wallet_toxicity;

use crate::shm_reader::ShmBboMessage;
use std::future::Future;
use std::pin::Pin;

/// Strategy defines a common interface for quantitative trading strategies.
/// This allows the core engine to Multiplex shared memory BBO updates to
/// diverse strategies such as cross-exchange arbitrage or single-exchange HFT.
pub trait Strategy {
    /// Returns the name of the strategy for logging purposes
    fn name(&self) -> &str;

    /// Called whenever the shared memory matrix detects a BBO change
    /// for a specific symbol on a specific exchange.
    fn on_bbo_update(&mut self, symbol_id: u16, exchange_id: u8, bbo: &ShmBboMessage);

    /// Called at the end of every poll cycle when no new data is present.
    /// Used for periodic tasks like order lifecycle management.
    fn on_idle(&mut self);

    /// Called during graceful shutdown to cancel all orders
    fn on_shutdown(&mut self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async {})
    }
}
