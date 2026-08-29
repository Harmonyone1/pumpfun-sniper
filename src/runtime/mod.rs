//! Runtime ownership primitives.
//!
//! Provides an atomic, cross-process runtime lease so that at most one process
//! may own mutable trading state (positions, pending executions, controlled
//! wallets) for a given `credentials_dir` at a time.
//!
//! See invariants INV-RUN-001..006:
//! - Exclusion is anchored on a single `runtime.lock` file per `credentials_dir`.
//! - A stale lease is NEVER automatically stolen; operator cleanup is manual.
//! - A process removes the lease on `Drop` only if the on-disk nonce still
//!   matches the nonce it originally created.

pub mod lease;

pub use lease::{RuntimeLease, RuntimeLeaseMetadata};
