//! Trading module - Transaction building and submission
//!
//! Supports multiple execution methods:
//! - Jito bundles (fastest, MEV protected)
//! - PumpPortal API (easy, 0.5% fee)
//! - Direct RPC (standard)

pub mod jito;
pub mod pending;
pub mod pumpportal_api;
pub mod reconciliation;
pub mod recovery;
pub mod simulation;
pub mod tips;
pub mod transaction;

pub use jito::JitoClient;
pub use pending::{
    PendingBuyContext, PendingExecution, PendingExecutionContext,
    PendingExecutionStore, PendingSellContext, PendingSellIntent,
};
pub use pumpportal_api::PumpPortalTrader;
pub use recovery::{
    plan_pending_outcome, reconcile_pending_execution, PendingRecoveryPlan,
};
pub use reconciliation::{
    ReconcileConfig, ReconciledFill, ReconciliationOutcome, ReconciliationSide, TradeReconciler,
};
pub use transaction::TransactionBuilder;
