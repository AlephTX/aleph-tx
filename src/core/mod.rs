//! Core module - Common types, traits, and error handling

pub mod error;
pub mod types;
pub mod traits;
pub mod config;

pub use error::{Error, Result};
pub use types::*;
pub use traits::*;
pub use config::Config;
