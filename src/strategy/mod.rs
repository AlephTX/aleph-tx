pub mod arbitrage;
pub mod backpack_mm;
pub mod market_maker;

use crate::shm_reader::ShmBboMessage;

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
}
