//! Canonical market price and quote truth.
//!
//! Phase-1 integration glue: exposes the pure domain/math/state-decode
//! submodules so they compile as part of the crate. Agent D expands this with
//! the on-chain oracle module and curated re-exports.

pub mod math;
pub mod oracle;
pub mod pump_state;
pub mod types;

// Curated re-exports (packet D9). The oracle is the canonical live-money entry
// point; the domain types travel with every snapshot/quote it returns.
pub use oracle::PumpMarketOracle;
pub use types::{ExecutableQuote, MarketSide, MarketSnapshot, MarketVenue, QuoteAsset};
