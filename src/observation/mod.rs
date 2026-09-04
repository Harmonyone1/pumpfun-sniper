//! Append-only observation research recorder + deterministic replay (P1-001).
//!
//! This module is the unbiased raw observation boundary for the research
//! dataset. It records what the provider reported, what the canonical on-chain
//! market state was, exact executable buy/sell quotes, and future path/horizon
//! outcomes — with NO strategy, filter, or trading behavior.

pub mod measurement;
pub mod measurement_runtime;
pub mod recorder;
pub mod replay;
pub mod schema;

pub use recorder::ObservationRecorder;
pub use replay::{read_observation_run, ReplayRun};
pub use schema::*;
