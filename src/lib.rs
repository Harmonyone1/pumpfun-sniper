//! Pump.fun Sniper Bot Library
//!
//! High-performance token sniper for pump.fun using Jito ShredStream.

pub mod cli;
pub mod config;
pub mod dexscreener;
pub mod error;
pub mod filter;
pub mod position;
pub mod pump;
pub mod strategy;
pub mod stream;
pub mod trading;
pub mod wallet;

// Re-export commonly used types
pub use config::Config;
pub use error::{Error, Result};
