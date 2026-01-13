//! Pump.fun protocol module
//!
//! # WARNING: Protocol Instability
//! Pump.fun has historically changed program behavior without notice.
//! The constants and structures in this module may break silently.
//! Monitor pump.fun announcements and be prepared to update.

pub mod accounts;
pub mod instruction;
pub mod mint;
pub mod price;
pub mod program;

// Re-export commonly used types
pub use accounts::BondingCurve;
pub use instruction::{BuyInstruction, CreateInstruction, PumpInstruction, SellInstruction};
pub use price::calculate_price;
pub use program::{DISCRIMINATORS, PUMP_PROGRAM_ID};
