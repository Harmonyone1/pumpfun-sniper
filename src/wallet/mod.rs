//! Wallet management module
//!
//! Provides hot wallet + vault architecture with:
//! - Credential management (wallets.json registry)
//! - Safety enforcement (limits, locks, bounds)
//! - Profit extraction (rule-based + AI-assisted)
//! - Transfer execution
//!
//! # Architecture
//!
//! ```text
//! CredentialManager → WalletManager → SafetyEnforcer → TransferExecutor
//!                          ↑
//!                    ProfitExtractor
//!                          ↑
//!                      AiAdvisor
//! ```
//!
//! # Security
//!
//! AI operations are bounded by deterministic safety limits:
//! - Cannot drain below minimum balance
//! - Cannot exceed per-transfer or daily limits
//! - Cannot change vault addresses
//! - Cannot withdraw from vault

pub mod advisor;
pub mod credentials;
pub mod extractor;
pub mod manager;
pub mod safety;
pub mod transfer;
pub mod types;

pub use credentials::CredentialManager;
pub use manager::WalletManager;
pub use safety::{SafetyEnforcer, SafetyViolation};
pub use types::{
    AiProposal, DailyExtractionStats, InitiatedBy, ProposalStatus, ProposedAction, TransferHistory,
    TransferReason, TransferRecord, WalletEntry, WalletRegistry, WalletStatus, WalletType,
};
