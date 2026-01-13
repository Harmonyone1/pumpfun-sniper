//! Safety enforcer for wallet operations
//!
//! Enforces hard limits on all transfers and AI actions.
//! These limits cannot be overridden by AI or automatic systems.

use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, warn};

use super::types::{AiProposal, DailyExtractionStats, ProposedAction, WalletType};

/// Safety configuration
#[derive(Debug, Clone)]
pub struct WalletSafetyConfig {
    /// Minimum SOL to keep in hot wallet
    pub min_hot_balance_sol: f64,

    /// Maximum single transfer to vault
    pub max_single_transfer_sol: f64,

    /// Maximum total daily extraction to vault
    pub max_daily_extraction_sol: f64,

    /// Require confirmation for transfers above this amount
    pub confirm_above_sol: f64,

    /// Emergency threshold - pause trading if hot wallet drops below
    pub emergency_threshold_sol: f64,

    /// Lock vault address (prevent changes)
    pub vault_address_locked: bool,

    /// Maximum AI can auto-execute without user approval
    pub ai_max_auto_transfer_sol: f64,
}

impl Default for WalletSafetyConfig {
    fn default() -> Self {
        Self {
            min_hot_balance_sol: 0.1,
            max_single_transfer_sol: 5.0,
            max_daily_extraction_sol: 10.0,
            confirm_above_sol: 1.0,
            emergency_threshold_sol: 0.05,
            vault_address_locked: true,
            ai_max_auto_transfer_sol: 0.5,
        }
    }
}

/// Safety violation types
#[derive(Debug, Clone, PartialEq)]
pub enum SafetyViolation {
    /// Transfer would drain hot wallet below minimum
    MinBalanceViolation { remaining: f64, minimum: f64 },

    /// Single transfer exceeds maximum
    MaxSingleTransferExceeded { amount: f64, max: f64 },

    /// Daily extraction limit reached
    DailyLimitExceeded {
        current: f64,
        requested: f64,
        max: f64,
    },

    /// Vault address is locked
    VaultAddressLocked,

    /// Cannot withdraw from vault
    VaultWithdrawalBlocked,

    /// AI action exceeds authority
    AiAuthorityExceeded { amount: f64, max: f64 },

    /// AI action is forbidden
    AiActionForbidden { action: String },

    /// Emergency lock is active
    EmergencyLockActive,

    /// Source wallet cannot send
    InvalidSourceWallet { wallet_type: String },

    /// Destination wallet cannot receive
    InvalidDestinationWallet { wallet_type: String },
}

impl std::fmt::Display for SafetyViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SafetyViolation::MinBalanceViolation { remaining, minimum } => {
                write!(
                    f,
                    "Would drain hot wallet below minimum: {} SOL remaining, {} SOL minimum",
                    remaining, minimum
                )
            }
            SafetyViolation::MaxSingleTransferExceeded { amount, max } => {
                write!(
                    f,
                    "Transfer {} SOL exceeds maximum single transfer {} SOL",
                    amount, max
                )
            }
            SafetyViolation::DailyLimitExceeded {
                current,
                requested,
                max,
            } => {
                write!(
                    f,
                    "Daily limit exceeded: {} SOL extracted + {} SOL requested > {} SOL limit",
                    current, requested, max
                )
            }
            SafetyViolation::VaultAddressLocked => {
                write!(f, "Vault address is locked and cannot be changed")
            }
            SafetyViolation::VaultWithdrawalBlocked => {
                write!(f, "Cannot withdraw from vault - vault is receive-only")
            }
            SafetyViolation::AiAuthorityExceeded { amount, max } => {
                write!(
                    f,
                    "AI cannot auto-execute {} SOL transfer (max {} SOL)",
                    amount, max
                )
            }
            SafetyViolation::AiActionForbidden { action } => {
                write!(f, "AI is not authorized to: {}", action)
            }
            SafetyViolation::EmergencyLockActive => {
                write!(f, "Emergency lock is active - operations paused")
            }
            SafetyViolation::InvalidSourceWallet { wallet_type } => {
                write!(f, "Cannot transfer from {} wallet", wallet_type)
            }
            SafetyViolation::InvalidDestinationWallet { wallet_type } => {
                write!(f, "Cannot transfer to {} wallet", wallet_type)
            }
        }
    }
}

impl std::error::Error for SafetyViolation {}

/// Pending transfer request
#[derive(Debug, Clone)]
pub struct PendingTransfer {
    /// Source wallet name
    pub from_wallet: String,

    /// Source wallet type
    pub from_type: WalletType,

    /// Current balance of source wallet
    pub from_balance: f64,

    /// Destination wallet name
    pub to_wallet: String,

    /// Destination wallet type
    pub to_type: WalletType,

    /// Destination address
    pub to_address: solana_sdk::pubkey::Pubkey,

    /// Amount to transfer
    pub amount_sol: f64,

    /// Is this an AI-initiated transfer?
    pub is_ai_initiated: bool,
}

/// Emergency action to take
#[derive(Debug, Clone, PartialEq)]
pub enum EmergencyAction {
    /// Pause trading due to low balance
    PauseTradingLowBalance,

    /// Pause trading due to daily loss limit
    PauseTradingDailyLoss,
}

/// Safety enforcer - validates all wallet operations
pub struct SafetyEnforcer {
    config: WalletSafetyConfig,
    daily_stats: Arc<RwLock<DailyExtractionStats>>,
    emergency_lock: Arc<RwLock<bool>>,
    configured_vault_address: Option<solana_sdk::pubkey::Pubkey>,
}

impl SafetyEnforcer {
    /// Create a new safety enforcer
    pub fn new(config: WalletSafetyConfig) -> Self {
        Self {
            config,
            daily_stats: Arc::new(RwLock::new(DailyExtractionStats::new_today())),
            emergency_lock: Arc::new(RwLock::new(false)),
            configured_vault_address: None,
        }
    }

    /// Set the configured vault address (for lock validation)
    pub fn set_vault_address(&mut self, address: solana_sdk::pubkey::Pubkey) {
        self.configured_vault_address = Some(address);
    }

    /// Validate a pending transfer
    pub async fn validate_transfer(
        &self,
        transfer: &PendingTransfer,
    ) -> Result<(), SafetyViolation> {
        // Check emergency lock
        if *self.emergency_lock.read().await {
            return Err(SafetyViolation::EmergencyLockActive);
        }

        // Check source wallet type
        if transfer.from_type == WalletType::External {
            return Err(SafetyViolation::InvalidSourceWallet {
                wallet_type: "external".to_string(),
            });
        }

        if transfer.from_type == WalletType::Auth {
            return Err(SafetyViolation::InvalidSourceWallet {
                wallet_type: "auth".to_string(),
            });
        }

        // Block vault withdrawals
        if transfer.from_type == WalletType::Vault {
            return Err(SafetyViolation::VaultWithdrawalBlocked);
        }

        // Check minimum hot wallet balance
        if transfer.from_type == WalletType::Hot {
            let remaining = transfer.from_balance - transfer.amount_sol;
            if remaining < self.config.min_hot_balance_sol {
                return Err(SafetyViolation::MinBalanceViolation {
                    remaining,
                    minimum: self.config.min_hot_balance_sol,
                });
            }
        }

        // Check single transfer limit
        if transfer.amount_sol > self.config.max_single_transfer_sol {
            return Err(SafetyViolation::MaxSingleTransferExceeded {
                amount: transfer.amount_sol,
                max: self.config.max_single_transfer_sol,
            });
        }

        // Check daily limit
        let mut stats = self.daily_stats.write().await;
        stats.reset_if_new_day();

        if stats.total_extracted_sol + transfer.amount_sol > self.config.max_daily_extraction_sol {
            return Err(SafetyViolation::DailyLimitExceeded {
                current: stats.total_extracted_sol,
                requested: transfer.amount_sol,
                max: self.config.max_daily_extraction_sol,
            });
        }

        // Check vault address lock
        if self.config.vault_address_locked {
            if let Some(configured) = self.configured_vault_address {
                if transfer.to_address != configured {
                    return Err(SafetyViolation::VaultAddressLocked);
                }
            }
        }

        // Check AI authority limits
        if transfer.is_ai_initiated && transfer.amount_sol > self.config.ai_max_auto_transfer_sol {
            return Err(SafetyViolation::AiAuthorityExceeded {
                amount: transfer.amount_sol,
                max: self.config.ai_max_auto_transfer_sol,
            });
        }

        debug!(
            "Transfer validated: {} SOL from {} to {}",
            transfer.amount_sol, transfer.from_wallet, transfer.to_wallet
        );

        Ok(())
    }

    /// Validate AI authority for a proposal
    pub fn validate_ai_authority(&self, proposal: &AiProposal) -> Result<(), SafetyViolation> {
        // Check amount limits
        if let Some(amount) = proposal.amount_sol {
            if amount > self.config.ai_max_auto_transfer_sol {
                return Err(SafetyViolation::AiAuthorityExceeded {
                    amount,
                    max: self.config.ai_max_auto_transfer_sol,
                });
            }
        }

        // Check forbidden actions
        // AI can never:
        // - Change vault addresses (not representable as ProposedAction)
        // - Withdraw from vault (not representable as ProposedAction)
        // - Override emergency locks (not representable as ProposedAction)

        // All current ProposedAction variants are allowed within limits
        match &proposal.action {
            ProposedAction::ExtractToVault => {
                // Allowed if within amount limits (checked above)
            }
            ProposedAction::SkipTrade => {
                // Always allowed
            }
            ProposedAction::ReducePosition => {
                // Always allowed
            }
            ProposedAction::IncreaseBuyAmount => {
                // Allowed but will be constrained by trading limits
            }
            ProposedAction::PauseTrading => {
                // Always allowed
            }
            ProposedAction::ResumeTrading => {
                // Always allowed
            }
        }

        Ok(())
    }

    /// Check if a transfer requires user confirmation
    pub fn requires_confirmation(&self, amount_sol: f64) -> bool {
        amount_sol > self.config.confirm_above_sol
    }

    /// Check for emergency conditions
    pub fn check_emergency(&self, hot_balance: f64) -> Option<EmergencyAction> {
        if hot_balance < self.config.emergency_threshold_sol {
            warn!(
                "Emergency condition: hot wallet balance {} SOL below threshold {} SOL",
                hot_balance, self.config.emergency_threshold_sol
            );
            return Some(EmergencyAction::PauseTradingLowBalance);
        }
        None
    }

    /// Activate emergency lock
    pub async fn activate_emergency_lock(&self) {
        *self.emergency_lock.write().await = true;
        warn!("Emergency lock activated - all operations paused");
    }

    /// Deactivate emergency lock
    pub async fn deactivate_emergency_lock(&self) {
        *self.emergency_lock.write().await = false;
        warn!("Emergency lock deactivated - operations resumed");
    }

    /// Check if emergency lock is active
    pub async fn is_emergency_locked(&self) -> bool {
        *self.emergency_lock.read().await
    }

    /// Record a successful extraction
    pub async fn record_extraction(&self, amount_sol: f64) {
        let mut stats = self.daily_stats.write().await;
        stats.reset_if_new_day();
        stats.total_extracted_sol += amount_sol;
        stats.extraction_count += 1;
        stats.last_extraction = Some(chrono::Utc::now());

        debug!(
            "Recorded extraction: {} SOL (daily total: {} SOL)",
            amount_sol, stats.total_extracted_sol
        );
    }

    /// Get daily extraction stats
    pub async fn daily_stats(&self) -> DailyExtractionStats {
        let mut stats = self.daily_stats.write().await;
        stats.reset_if_new_day();
        stats.clone()
    }

    /// Get remaining daily extraction allowance
    pub async fn remaining_daily_allowance(&self) -> f64 {
        let stats = self.daily_stats().await;
        (self.config.max_daily_extraction_sol - stats.total_extracted_sol).max(0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::pubkey::Pubkey;

    fn test_config() -> WalletSafetyConfig {
        WalletSafetyConfig {
            min_hot_balance_sol: 0.1,
            max_single_transfer_sol: 5.0,
            max_daily_extraction_sol: 10.0,
            confirm_above_sol: 1.0,
            emergency_threshold_sol: 0.05,
            vault_address_locked: false, // Disable for testing
            ai_max_auto_transfer_sol: 0.5,
        }
    }

    #[tokio::test]
    async fn test_min_balance_violation() {
        let enforcer = SafetyEnforcer::new(test_config());

        let transfer = PendingTransfer {
            from_wallet: "hot".to_string(),
            from_type: WalletType::Hot,
            from_balance: 0.15, // Only 0.15 SOL
            to_wallet: "vault".to_string(),
            to_type: WalletType::External,
            to_address: Pubkey::default(),
            amount_sol: 0.1, // Would leave only 0.05
            is_ai_initiated: false,
        };

        let result = enforcer.validate_transfer(&transfer).await;
        assert!(matches!(
            result,
            Err(SafetyViolation::MinBalanceViolation { .. })
        ));
    }

    #[tokio::test]
    async fn test_max_transfer_violation() {
        let enforcer = SafetyEnforcer::new(test_config());

        let transfer = PendingTransfer {
            from_wallet: "hot".to_string(),
            from_type: WalletType::Hot,
            from_balance: 100.0,
            to_wallet: "vault".to_string(),
            to_type: WalletType::External,
            to_address: Pubkey::default(),
            amount_sol: 10.0, // Exceeds max 5.0
            is_ai_initiated: false,
        };

        let result = enforcer.validate_transfer(&transfer).await;
        assert!(matches!(
            result,
            Err(SafetyViolation::MaxSingleTransferExceeded { .. })
        ));
    }

    #[tokio::test]
    async fn test_vault_withdrawal_blocked() {
        let enforcer = SafetyEnforcer::new(test_config());

        let transfer = PendingTransfer {
            from_wallet: "vault".to_string(),
            from_type: WalletType::Vault,
            from_balance: 10.0,
            to_wallet: "hot".to_string(),
            to_type: WalletType::Hot,
            to_address: Pubkey::default(),
            amount_sol: 1.0,
            is_ai_initiated: false,
        };

        let result = enforcer.validate_transfer(&transfer).await;
        assert!(matches!(
            result,
            Err(SafetyViolation::VaultWithdrawalBlocked)
        ));
    }

    #[tokio::test]
    async fn test_ai_authority_exceeded() {
        let enforcer = SafetyEnforcer::new(test_config());

        let transfer = PendingTransfer {
            from_wallet: "hot".to_string(),
            from_type: WalletType::Hot,
            from_balance: 10.0,
            to_wallet: "vault".to_string(),
            to_type: WalletType::External,
            to_address: Pubkey::default(),
            amount_sol: 1.0, // Exceeds AI max 0.5
            is_ai_initiated: true,
        };

        let result = enforcer.validate_transfer(&transfer).await;
        assert!(matches!(
            result,
            Err(SafetyViolation::AiAuthorityExceeded { .. })
        ));
    }

    #[tokio::test]
    async fn test_valid_transfer() {
        let enforcer = SafetyEnforcer::new(test_config());

        let transfer = PendingTransfer {
            from_wallet: "hot".to_string(),
            from_type: WalletType::Hot,
            from_balance: 10.0,
            to_wallet: "vault".to_string(),
            to_type: WalletType::External,
            to_address: Pubkey::default(),
            amount_sol: 1.0,
            is_ai_initiated: false,
        };

        let result = enforcer.validate_transfer(&transfer).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_daily_limit() {
        let enforcer = SafetyEnforcer::new(test_config());

        // Record 9 SOL extracted
        enforcer.record_extraction(9.0).await;

        let transfer = PendingTransfer {
            from_wallet: "hot".to_string(),
            from_type: WalletType::Hot,
            from_balance: 10.0,
            to_wallet: "vault".to_string(),
            to_type: WalletType::External,
            to_address: Pubkey::default(),
            amount_sol: 2.0, // Would exceed 10 SOL daily limit
            is_ai_initiated: false,
        };

        let result = enforcer.validate_transfer(&transfer).await;
        assert!(matches!(
            result,
            Err(SafetyViolation::DailyLimitExceeded { .. })
        ));
    }
}
