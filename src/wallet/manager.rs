//! Wallet manager - core wallet operations
//!
//! Coordinates credential management, safety enforcement, and transfers.

use std::path::Path;
use std::sync::Arc;

use chrono::Utc;
use solana_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::error::{Error, Result};

use super::credentials::CredentialManager;
use super::safety::{PendingTransfer, SafetyEnforcer, WalletSafetyConfig};
use super::transfer::TransferExecutor;
use super::types::{
    AiProposal, InitiatedBy, ProposalStatus, TransferHistory, TransferReason, TransferRecord,
    WalletEntry, WalletStatus, WalletType,
};

/// Wallet manager configuration
#[derive(Debug, Clone)]
pub struct WalletManagerConfig {
    /// Name of hot wallet in registry
    pub hot_wallet_name: String,

    /// Name of vault wallet in registry
    pub vault_wallet_name: String,

    /// Path to credentials directory
    pub credentials_dir: String,

    /// Safety configuration
    pub safety: WalletSafetyConfig,
}

impl Default for WalletManagerConfig {
    fn default() -> Self {
        Self {
            hot_wallet_name: "hot-trading".to_string(),
            vault_wallet_name: "vault-robinhood".to_string(),
            credentials_dir: "credentials".to_string(),
            safety: WalletSafetyConfig::default(),
        }
    }
}

/// Core wallet manager
pub struct WalletManager {
    /// Credential manager
    credentials: Arc<RwLock<CredentialManager>>,

    /// Safety enforcer
    safety: Arc<SafetyEnforcer>,

    /// Transfer executor
    transfer_executor: TransferExecutor,

    /// RPC client for balance checks
    #[allow(dead_code)]
    rpc_client: Arc<RpcClient>,

    /// Configuration
    config: WalletManagerConfig,

    /// Transfer history
    history: Arc<RwLock<TransferHistory>>,

    /// Pending AI proposals
    pending_proposals: Arc<RwLock<Vec<AiProposal>>>,

    /// Path to history file
    history_path: Option<String>,
}

impl WalletManager {
    /// Create a new wallet manager
    pub async fn new(config: WalletManagerConfig, rpc_client: RpcClient) -> Result<Self> {
        let credentials_path = Path::new(&config.credentials_dir);
        let credentials = CredentialManager::load(credentials_path)?;

        // Validate permission warnings
        let warnings = credentials.validate_permissions();
        for warning in &warnings {
            warn!("{}", warning);
        }

        let mut safety = SafetyEnforcer::new(config.safety.clone());

        // Get vault address for lock validation
        let mut cred_lock = credentials;
        if let Ok(vault_addr) = cred_lock.get_address(&config.vault_wallet_name) {
            safety.set_vault_address(vault_addr);
        }

        let rpc_client = Arc::new(rpc_client);
        let transfer_executor = TransferExecutor::new(RpcClient::new_with_timeout(
            rpc_client.url(),
            std::time::Duration::from_secs(30),
        ));

        Ok(Self {
            credentials: Arc::new(RwLock::new(cred_lock)),
            safety: Arc::new(safety),
            transfer_executor,
            rpc_client,
            config,
            history: Arc::new(RwLock::new(TransferHistory::default())),
            pending_proposals: Arc::new(RwLock::new(Vec::new())),
            history_path: None,
        })
    }

    /// Set path for history persistence
    pub fn set_history_path(&mut self, path: String) {
        self.history_path = Some(path);
    }

    /// Get hot wallet balance in SOL
    pub async fn hot_balance(&self) -> Result<f64> {
        let mut creds = self.credentials.write().await;
        let address = creds.get_address(&self.config.hot_wallet_name)?;
        self.transfer_executor.get_balance_sol(&address)
    }

    /// Get vault address
    pub async fn vault_address(&self) -> Result<Pubkey> {
        let mut creds = self.credentials.write().await;
        creds.get_address(&self.config.vault_wallet_name)
    }

    /// Extract SOL to vault
    ///
    /// # Arguments
    /// * `amount_sol` - Amount to extract
    /// * `reason` - Reason for extraction
    /// * `initiated_by` - Who initiated this extraction
    /// * `force` - Skip confirmation requirement
    pub async fn extract_to_vault(
        &self,
        amount_sol: f64,
        reason: TransferReason,
        initiated_by: InitiatedBy,
        force: bool,
    ) -> Result<TransferRecord> {
        info!(
            "Extracting {} SOL to vault (reason: {}, by: {})",
            amount_sol, reason, initiated_by
        );

        // Get credentials
        let mut creds = self.credentials.write().await;

        let hot_wallet = creds
            .get_wallet(&self.config.hot_wallet_name)
            .ok_or_else(|| Error::Config("Hot wallet not configured".to_string()))?
            .clone();

        let vault_wallet = creds
            .get_wallet(&self.config.vault_wallet_name)
            .ok_or_else(|| Error::Config("Vault wallet not configured".to_string()))?
            .clone();

        let hot_address = creds.get_address(&self.config.hot_wallet_name)?;
        let vault_address = creds.get_address(&self.config.vault_wallet_name)?;

        // Get current balance
        let hot_balance = self.transfer_executor.get_balance_sol(&hot_address)?;

        // Build pending transfer
        let pending = PendingTransfer {
            from_wallet: hot_wallet.name.clone(),
            from_type: hot_wallet.wallet_type,
            from_balance: hot_balance,
            to_wallet: vault_wallet.name.clone(),
            to_type: vault_wallet.wallet_type,
            to_address: vault_address,
            amount_sol,
            is_ai_initiated: matches!(initiated_by, InitiatedBy::AiAdvisor { .. }),
        };

        // Validate with safety enforcer
        self.safety
            .validate_transfer(&pending)
            .await
            .map_err(|e| Error::SafetyLimitExceeded(e.to_string()))?;

        // Check confirmation requirement
        if !force && self.safety.requires_confirmation(amount_sol) {
            return Err(Error::Config(format!(
                "Transfer of {} SOL requires confirmation (use --force to skip)",
                amount_sol
            )));
        }

        // Execute transfer
        let hot_keypair = creds.get_keypair(&self.config.hot_wallet_name)?;
        let signature =
            self.transfer_executor
                .transfer_sol(hot_keypair, &vault_address, amount_sol)?;

        // Record extraction
        self.safety.record_extraction(amount_sol).await;

        // Create transfer record
        let record = TransferRecord {
            id: Uuid::new_v4().to_string(),
            from_wallet: hot_wallet.name,
            to_wallet: vault_wallet.name,
            amount_sol,
            reason,
            signature: signature.to_string(),
            timestamp: Utc::now(),
            initiated_by,
        };

        // Save to history
        {
            let mut history = self.history.write().await;
            history.transfers.insert(0, record.clone());

            // Keep only last 1000 records
            history.transfers.truncate(1000);
        }

        self.save_history().await?;

        info!(
            "Extraction complete: {} SOL (sig: {})",
            amount_sol, signature
        );

        Ok(record)
    }

    /// Get wallet status for all wallets
    pub async fn status(&self) -> Vec<WalletStatus> {
        // Collect wallet entries into owned data to release lock before iteration
        let wallets: Vec<WalletEntry> = {
            let creds = self.credentials.read().await;
            creds.list_wallets().into_iter().cloned().collect()
        };

        let mut statuses = Vec::new();

        for wallet in wallets {
            let mut status = WalletStatus {
                name: wallet.name.clone(),
                alias: wallet.alias.clone(),
                wallet_type: wallet.wallet_type,
                balance_sol: None,
                address: wallet.address.clone(),
                warnings: Vec::new(),
            };

            // Try to get actual address (requires write lock for keypair derivation)
            let mut creds_mut = self.credentials.write().await;
            if let Ok(addr) = creds_mut.get_address(&wallet.name) {
                status.address = addr.to_string();
                drop(creds_mut); // Release lock before RPC call

                // Try to get balance for non-auth wallets
                if wallet.wallet_type != WalletType::Auth {
                    match self.transfer_executor.get_balance_sol(&addr) {
                        Ok(balance) => {
                            status.balance_sol = Some(balance);

                            // Check for warnings
                            if wallet.wallet_type == WalletType::Hot {
                                if let Some(emergency) = self.safety.check_emergency(balance) {
                                    status.warnings.push(format!("{:?}", emergency));
                                }
                            }
                        }
                        Err(e) => {
                            status.warnings.push(format!("Balance fetch failed: {}", e));
                        }
                    }
                }
            } else {
                drop(creds_mut); // Always release the lock
            }

            statuses.push(status);
        }

        statuses
    }

    /// Get transfer history
    pub async fn history(&self, limit: usize) -> Vec<TransferRecord> {
        let history = self.history.read().await;
        history.transfers.iter().take(limit).cloned().collect()
    }

    /// Add an AI proposal
    pub async fn add_proposal(&self, proposal: AiProposal) {
        let mut proposals = self.pending_proposals.write().await;
        proposals.push(proposal);
    }

    /// Get pending proposals
    pub async fn pending_proposals(&self) -> Vec<AiProposal> {
        let proposals = self.pending_proposals.read().await;
        proposals
            .iter()
            .filter(|p| p.status == ProposalStatus::Pending)
            .cloned()
            .collect()
    }

    /// Approve a proposal
    pub async fn approve_proposal(&self, proposal_id: &str) -> Result<()> {
        let mut proposals = self.pending_proposals.write().await;

        let proposal = proposals
            .iter_mut()
            .find(|p| p.id == proposal_id && p.status == ProposalStatus::Pending)
            .ok_or_else(|| Error::Config(format!("Proposal not found: {}", proposal_id)))?;

        proposal.status = ProposalStatus::Approved;
        proposal.status_updated_at = Some(Utc::now());

        info!("Approved proposal: {}", proposal_id);
        Ok(())
    }

    /// Reject a proposal
    pub async fn reject_proposal(&self, proposal_id: &str) -> Result<()> {
        let mut proposals = self.pending_proposals.write().await;

        let proposal = proposals
            .iter_mut()
            .find(|p| p.id == proposal_id && p.status == ProposalStatus::Pending)
            .ok_or_else(|| Error::Config(format!("Proposal not found: {}", proposal_id)))?;

        proposal.status = ProposalStatus::Rejected;
        proposal.status_updated_at = Some(Utc::now());

        info!("Rejected proposal: {}", proposal_id);
        Ok(())
    }

    /// Emergency shutdown
    pub async fn emergency_shutdown(&self, reason: &str) -> Result<()> {
        warn!("Emergency shutdown initiated: {}", reason);
        self.safety.activate_emergency_lock().await;
        Ok(())
    }

    /// Resume from emergency
    pub async fn resume_operations(&self) -> Result<()> {
        info!("Resuming operations from emergency");
        self.safety.deactivate_emergency_lock().await;
        Ok(())
    }

    /// Check if emergency lock is active
    pub async fn is_emergency_locked(&self) -> bool {
        self.safety.is_emergency_locked().await
    }

    /// Get daily extraction stats
    pub async fn daily_stats(&self) -> super::types::DailyExtractionStats {
        self.safety.daily_stats().await
    }

    /// Get remaining daily extraction allowance
    pub async fn remaining_daily_allowance(&self) -> f64 {
        self.safety.remaining_daily_allowance().await
    }

    /// Get the safety enforcer
    pub fn safety(&self) -> &Arc<SafetyEnforcer> {
        &self.safety
    }

    /// Save history to file
    async fn save_history(&self) -> Result<()> {
        if let Some(path) = &self.history_path {
            let history = self.history.read().await;
            let json = serde_json::to_string_pretty(&*history).map_err(|e| {
                Error::PositionPersistence(format!("Failed to serialize history: {}", e))
            })?;

            tokio::fs::write(path, json).await.map_err(|e| {
                Error::PositionPersistence(format!("Failed to write history: {}", e))
            })?;

            debug!("Saved transfer history");
        }
        Ok(())
    }

    /// Load history from file
    pub async fn load_history(&self, path: &str) -> Result<()> {
        if let Ok(content) = tokio::fs::read_to_string(path).await {
            let loaded: TransferHistory = serde_json::from_str(&content).map_err(|e| {
                Error::PositionPersistence(format!("Failed to parse history: {}", e))
            })?;

            let mut history = self.history.write().await;
            *history = loaded;

            info!("Loaded {} transfer records", history.transfers.len());
        }
        Ok(())
    }

    /// List all wallets
    pub async fn list_wallets(&self) -> Vec<WalletEntry> {
        let creds = self.credentials.read().await;
        creds.list_wallets().into_iter().cloned().collect()
    }

    /// Add a new wallet
    pub async fn add_wallet(&self, entry: WalletEntry) -> Result<()> {
        let mut creds = self.credentials.write().await;
        creds.add_wallet(entry)
    }
}
