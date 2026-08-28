//! Canonical market price and quote truth.
//!
//! Phase-1 integration glue: exposes the pure domain/math/state-decode
//! submodules so they compile as part of the crate. Agent D expands this with
//! the on-chain oracle module and curated re-exports.

pub mod math;
pub mod pump_state;
pub mod types;
