//! Trading module - Transaction building and submission
//!
//! Supports multiple execution methods:
//! - Jito bundles (fastest, MEV protected)
//! - PumpPortal API (easy, 0.5% fee)
//! - Direct RPC (standard)

pub mod jito;
pub mod pumpportal_api;
pub mod simulation;
pub mod tips;
pub mod transaction;

pub use jito::JitoClient;
pub use pumpportal_api::PumpPortalTrader;
pub use transaction::TransactionBuilder;
