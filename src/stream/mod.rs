//! Stream module - Data ingestion from multiple sources
//!
//! Supports:
//! - Jito ShredStream (fastest, requires approval) - enable with `shredstream` feature
//! - PumpPortal WebSocket (new token + migration: free; token/account trade
//!   streams: authenticated + metered)

pub mod backpressure;
pub mod decoder;
pub mod pumpportal;

#[cfg(feature = "shredstream")]
pub mod shredstream;

pub use backpressure::{BackpressureChannel, DropPolicy};
pub use pumpportal::{
    MigrationEvent, PumpPortalClient, PumpPortalConfig, PumpPortalEvent, PumpPortalSubscriptionPlan,
    SubscriptionCommand,
};

#[cfg(feature = "shredstream")]
pub use shredstream::ShredStreamClient;
