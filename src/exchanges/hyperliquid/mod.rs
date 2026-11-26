//! Hyperliquid native and HIP-3 perpetual API adapter.
//!
//! Public info reads and explicit signed order/cancel submission. No strategy is
//! started by this module. Trading uses an approved agent key supplied by the
//! caller; account reads always require the actual account address, not that key's
//! address. HIP-3 IDs are resolved from perpDexs and namespaced metadata; spot is not implemented.

mod client;
mod model;
mod signing;

pub use client::{
    AccountState, AssetPosition, HyperliquidInfo, HyperliquidTrading, LocatedOrder, MarginSummary,
    OpenOrder, OrderLookup, Position, SharedNoncePool, SpotBalance, SpotClearinghouseState,
    UserFill, aggregate_multi_dex_equity, aggregate_multi_dex_margin_used,
};
pub use model::*;
pub use signing::{HyperliquidSigner, L1Signature, action_hash};

#[cfg(test)]
mod tests;
