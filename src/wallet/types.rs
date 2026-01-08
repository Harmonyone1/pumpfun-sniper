//! Core types for wallet management
//!
//! Defines wallet entries, transfer records, and AI proposals.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Wallet entry from wallets.json registry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletEntry {
    /// Unique identifier (lowercase, no spaces): "hot-trading"
    pub name: String,

    /// Human-readable name: "Trading Wallet"
    pub alias: String,

    /// Wallet type
    #[serde(rename = "type")]
    pub wallet_type: WalletType,

    /// Path to keypair file (relative to project root)
    /// None for external wallets
    pub keypair_path: Option<PathBuf>,

    /// Wallet address
    /// "AUTO_DERIVED" means derive from keypair
    pub address: String,

    /// When the wallet was added
    pub created_at: DateTime<Utc>,

    /// User notes about this wallet
    #[serde(default)]
    pub notes: String,
}

/// Type of wallet
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WalletType {
    /// Hot wallet for active trading (has keypair)
    Hot,

    /// Internal vault wallet (has keypair, for internal storage)
    Vault,

    /// External address (no keypair - hardware wallet, exchange, etc.)
    External,

    /// Authentication-only keypair (ShredStream, etc.)
    Auth,
}

impl WalletType {
    /// Check if this wallet type has a keypair
    pub fn has_keypair(&self) -> bool {
        matches!(self, WalletType::Hot | WalletType::Vault | WalletType::Auth)
    }

    /// Check if this wallet can be used for trading
    pub fn can_trade(&self) -> bool {
        matches!(self, WalletType::Hot)
    }

    /// Check if this wallet can receive funds (vault destination)
    pub fn can_receive(&self) -> bool {
        matches!(self, WalletType::Vault | WalletType::External)
    }
}

impl std::fmt::Display for WalletType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WalletType::Hot => write!(f, "hot"),
            WalletType::Vault => write!(f, "vault"),
            WalletType::External => write!(f, "external"),
            WalletType::Auth => write!(f, "auth"),
        }
    }
}

/// Wallet registry file structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletRegistry {
    /// Registry format version
    #[serde(default = "default_version")]
    pub version: String,

    /// List of wallet entries
    pub wallets: Vec<WalletEntry>,
}

fn default_version() -> String {
    "1.0".to_string()
}

impl Default for WalletRegistry {
    fn default() -> Self {
        Self {
            version: default_version(),
            wallets: Vec::new(),
        }
    }
}

/// Transfer record for audit trail
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferRecord {
    /// Unique transfer ID
    pub id: String,

    /// Source wallet name
    pub from_wallet: String,

    /// Destination wallet name
    pub to_wallet: String,

    /// Amount in SOL
    pub amount_sol: f64,

    /// Reason for transfer
    pub reason: TransferReason,

    /// Transaction signature
    pub signature: String,

    /// When the transfer occurred
    pub timestamp: DateTime<Utc>,

    /// Who/what initiated this transfer
    pub initiated_by: InitiatedBy,
}

/// Reason for a transfer
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TransferReason {
    /// Extracting profits to vault
    ProfitExtraction,

    /// Rebalancing between wallets
    Rebalance,

    /// Emergency withdrawal
    EmergencyWithdraw,

    /// Manual user-initiated transfer
    ManualTransfer,

    /// Funding the hot wallet
    Funding,
}

impl std::fmt::Display for TransferReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransferReason::ProfitExtraction => write!(f, "Profit Extraction"),
            TransferReason::Rebalance => write!(f, "Rebalance"),
            TransferReason::EmergencyWithdraw => write!(f, "Emergency Withdraw"),
            TransferReason::ManualTransfer => write!(f, "Manual Transfer"),
            TransferReason::Funding => write!(f, "Funding"),
        }
    }
}

/// Who initiated a transfer
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InitiatedBy {
    /// User via CLI
    User,

    /// Automatic rule
    AutoRule {
        /// Name of the rule that triggered this
        rule: String,
    },

    /// AI advisor recommendation
    AiAdvisor {
        /// Proposal ID
        proposal_id: String,
    },

    /// Emergency system action
    Emergency,
}

impl std::fmt::Display for InitiatedBy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InitiatedBy::User => write!(f, "User"),
            InitiatedBy::AutoRule { rule } => write!(f, "Auto: {}", rule),
            InitiatedBy::AiAdvisor { proposal_id } => write!(f, "AI: {}", proposal_id),
            InitiatedBy::Emergency => write!(f, "Emergency"),
        }
    }
}

/// Transfer history file structure
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TransferHistory {
    /// List of transfer records (newest first)
    pub transfers: Vec<TransferRecord>,
}

/// AI-generated proposal for action
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiProposal {
    /// Unique proposal ID
    pub id: String,

    /// Proposed action
    pub action: ProposedAction,

    /// AI reasoning for this proposal
    pub reasoning: String,

    /// Confidence level (0.0 - 1.0)
    pub confidence: f64,

    /// Amount in SOL (if applicable)
    pub amount_sol: Option<f64>,

    /// When the proposal was created
    pub created_at: DateTime<Utc>,

    /// Current status
    pub status: ProposalStatus,

    /// When status last changed
    pub status_updated_at: Option<DateTime<Utc>>,
}

/// Action proposed by AI
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProposedAction {
    /// Extract profits to vault
    ExtractToVault,

    /// Skip a potential trade
    SkipTrade,

    /// Reduce position size
    ReducePosition,

    /// Increase buy amount
    IncreaseBuyAmount,

    /// Pause trading
    PauseTrading,

    /// Resume trading
    ResumeTrading,
}

impl std::fmt::Display for ProposedAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProposedAction::ExtractToVault => write!(f, "Extract to Vault"),
            ProposedAction::SkipTrade => write!(f, "Skip Trade"),
            ProposedAction::ReducePosition => write!(f, "Reduce Position"),
            ProposedAction::IncreaseBuyAmount => write!(f, "Increase Buy Amount"),
            ProposedAction::PauseTrading => write!(f, "Pause Trading"),
            ProposedAction::ResumeTrading => write!(f, "Resume Trading"),
        }
    }
}

/// Status of an AI proposal
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProposalStatus {
    /// Waiting for approval
    Pending,

    /// Automatically executed (within AI authority)
    AutoExecuted,

    /// Approved by user
    Approved,

    /// Rejected by user or safety system
    Rejected,

    /// Expired (too old)
    Expired,
}

impl std::fmt::Display for ProposalStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProposalStatus::Pending => write!(f, "Pending"),
            ProposalStatus::AutoExecuted => write!(f, "Auto-Executed"),
            ProposalStatus::Approved => write!(f, "Approved"),
            ProposalStatus::Rejected => write!(f, "Rejected"),
            ProposalStatus::Expired => write!(f, "Expired"),
        }
    }
}

/// Daily extraction statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DailyExtractionStats {
    /// Date (UTC)
    pub date: String,

    /// Total extracted today
    pub total_extracted_sol: f64,

    /// Number of extractions
    pub extraction_count: u32,

    /// Last extraction time
    pub last_extraction: Option<DateTime<Utc>>,
}

impl DailyExtractionStats {
    /// Create stats for today
    pub fn new_today() -> Self {
        Self {
            date: Utc::now().format("%Y-%m-%d").to_string(),
            total_extracted_sol: 0.0,
            extraction_count: 0,
            last_extraction: None,
        }
    }

    /// Check if stats are for today
    pub fn is_today(&self) -> bool {
        self.date == Utc::now().format("%Y-%m-%d").to_string()
    }

    /// Reset if not today
    pub fn reset_if_new_day(&mut self) {
        if !self.is_today() {
            *self = Self::new_today();
        }
    }
}

/// Wallet status snapshot
#[derive(Debug, Clone)]
pub struct WalletStatus {
    /// Wallet name
    pub name: String,

    /// Wallet alias
    pub alias: String,

    /// Wallet type
    pub wallet_type: WalletType,

    /// Current balance in SOL (None if unable to fetch)
    pub balance_sol: Option<f64>,

    /// Wallet address
    pub address: String,

    /// Any warnings or errors
    pub warnings: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wallet_type_properties() {
        assert!(WalletType::Hot.has_keypair());
        assert!(WalletType::Hot.can_trade());
        assert!(!WalletType::Hot.can_receive());

        assert!(!WalletType::External.has_keypair());
        assert!(!WalletType::External.can_trade());
        assert!(WalletType::External.can_receive());
    }

    #[test]
    fn test_wallet_entry_serialization() {
        let entry = WalletEntry {
            name: "test".to_string(),
            alias: "Test Wallet".to_string(),
            wallet_type: WalletType::Hot,
            keypair_path: Some(PathBuf::from("test/keypair.json")),
            address: "AUTO_DERIVED".to_string(),
            created_at: Utc::now(),
            notes: "Test".to_string(),
        };

        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"type\":\"hot\""));
    }

    #[test]
    fn test_daily_stats_reset() {
        let mut stats = DailyExtractionStats {
            date: "2020-01-01".to_string(),
            total_extracted_sol: 5.0,
            extraction_count: 3,
            last_extraction: None,
        };

        stats.reset_if_new_day();
        assert_eq!(stats.total_extracted_sol, 0.0);
        assert_eq!(stats.extraction_count, 0);
    }
}
