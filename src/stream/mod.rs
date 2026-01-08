//! Stream module - Data ingestion from multiple sources
//!
//! Supports:
//! - Jito ShredStream (fastest, requires approval) - enable with `shredstream` feature
//! - PumpPortal WebSocket (free, no approval needed)

pub mod backpressure;
pub mod decoder;
pub mod pumpportal;

#[cfg(feature = "shredstream")]
pub mod shredstream;

pub use backpressure::{BackpressureChannel, DropPolicy};
pub use pumpportal::{PumpPortalClient, PumpPortalConfig, PumpPortalEvent};

#[cfg(feature = "shredstream")]
pub use shredstream::ShredStreamClient;
