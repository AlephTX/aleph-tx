//! Core Trading Engine
//! Maintains Global World State and orchestrates adapters

pub mod state;
pub mod order;
pub mod risk;

pub use state::StateMachine;
pub use order::OrderManager;
pub use risk::RiskEngine;
