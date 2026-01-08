//! AI Advisor for wallet management
//!
//! Provides bounded AI recommendations for profit extraction and trading.
//! All AI actions operate within strict safety limits.
//!
//! # Authority Boundaries
//!
//! AI CAN:
//! - Propose profit extractions to vault (within limits)
//! - Execute small transfers automatically (< ai_max_auto_transfer_sol)
//! - Recommend position sizing
//! - Suggest pausing/resuming trading
//!
//! AI CANNOT:
//! - Drain hot wallet below minimum balance
//! - Change vault addresses
//! - Withdraw from vault
//! - Override emergency locks
//! - Execute large transfers without approval

use std::sync::Arc;

use chrono::Utc;
use tracing::{debug, info, warn};
use uuid::Uuid;

use super::manager::WalletManager;
use super::types::{
    AiProposal, InitiatedBy, ProposalStatus, ProposedAction, TransferReason,
};

/// AI Advisor configuration
#[derive(Debug, Clone)]
pub struct AdvisorConfig {
    /// Enable AI advisor
    pub enabled: bool,

    /// Profit threshold to suggest extraction (SOL)
    pub profit_extraction_trigger: f64,

    /// Balance threshold to suggest extraction (SOL)
    pub balance_extraction_trigger: f64,

    /// Losing streak threshold (number of losses)
    pub losing_streak_threshold: u32,

    /// Minimum confidence for auto-execution
    pub min_confidence_auto: f64,
}

impl Default for AdvisorConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            profit_extraction_trigger: 0.3,
            balance_extraction_trigger: 1.5,
            losing_streak_threshold: 5,
            min_confidence_auto: 0.8,
        }
    }
}

/// AI Advisor - bounded recommendations for wallet management
pub struct AiAdvisor {
    config: AdvisorConfig,
    wallet_manager: Arc<WalletManager>,
}

impl AiAdvisor {
    /// Create a new AI advisor
    pub fn new(config: AdvisorConfig, wallet_manager: Arc<WalletManager>) -> Self {
        Self {
            config,
            wallet_manager,
        }
    }

    /// Analyze current state and generate proposals
    pub async fn analyze(&self) -> Vec<AiProposal> {
        if !self.config.enabled {
            return vec![];
        }

        let mut proposals = Vec::new();

        // Get current state
        let hot_balance = match self.wallet_manager.hot_balance().await {
            Ok(b) => b,
            Err(e) => {
                warn!("AI analysis failed - cannot get hot balance: {}", e);
                return vec![];
            }
        };

        let daily_stats = self.wallet_manager.daily_stats().await;
        let remaining_allowance = self.wallet_manager.remaining_daily_allowance().await;

        debug!(
            "AI analyzing: hot_balance={}, daily_extracted={}, remaining_allowance={}",
            hot_balance, daily_stats.total_extracted_sol, remaining_allowance
        );

        // Rule 1: High balance - suggest extraction
        if hot_balance > self.config.balance_extraction_trigger {
            let extract_amount = (hot_balance - self.config.balance_extraction_trigger) * 0.5;
            let extract_amount = extract_amount.min(remaining_allowance);

            if extract_amount > 0.01 {
                proposals.push(AiProposal {
                    id: Uuid::new_v4().to_string(),
                    action: ProposedAction::ExtractToVault,
                    reasoning: format!(
                        "Hot wallet balance ({:.4} SOL) exceeds target ({:.4} SOL). \
                         Recommend extracting {:.4} SOL to vault for safety.",
                        hot_balance, self.config.balance_extraction_trigger, extract_amount
                    ),
                    confidence: 0.85,
                    amount_sol: Some(extract_amount),
                    created_at: Utc::now(),
                    status: ProposalStatus::Pending,
                    status_updated_at: None,
                });
            }
        }

        // Rule 2: Emergency low balance
        if hot_balance < 0.1 {
            proposals.push(AiProposal {
                id: Uuid::new_v4().to_string(),
                action: ProposedAction::PauseTrading,
                reasoning: format!(
                    "Hot wallet balance critically low ({:.4} SOL). \
                     Recommend pausing trading to preserve funds.",
                    hot_balance
                ),
                confidence: 0.95,
                amount_sol: None,
                created_at: Utc::now(),
                status: ProposalStatus::Pending,
                status_updated_at: None,
            });
        }

        // Rule 3: Check for patterns (placeholder for more sophisticated analysis)
        // In a real implementation, this could analyze:
        // - Win/loss patterns
        // - Time-of-day performance
        // - Token type performance
        // - Market conditions

        info!("AI generated {} proposals", proposals.len());
        proposals
    }

    /// Execute a proposal if within AI authority bounds
    pub async fn execute_if_authorized(
        &self,
        proposal: &AiProposal,
    ) -> Result<ProposalStatus, String> {
        // Check safety bounds
        if let Err(violation) = self
            .wallet_manager
            .safety()
            .validate_ai_authority(proposal)
        {
            warn!("AI proposal blocked by safety: {}", violation);
            return Ok(ProposalStatus::Rejected);
        }

        // Check confidence threshold for auto-execution
        if proposal.confidence < self.config.min_confidence_auto {
            debug!(
                "Proposal confidence ({}) below auto-execute threshold ({})",
                proposal.confidence, self.config.min_confidence_auto
            );
            return Ok(ProposalStatus::Pending);
        }

        // Execute based on action type
        match &proposal.action {
            ProposedAction::ExtractToVault => {
                let amount = proposal.amount_sol.ok_or("No amount specified")?;

                match self
                    .wallet_manager
                    .extract_to_vault(
                        amount,
                        TransferReason::ProfitExtraction,
                        InitiatedBy::AiAdvisor {
                            proposal_id: proposal.id.clone(),
                        },
                        true,
                    )
                    .await
                {
                    Ok(record) => {
                        info!(
                            "AI auto-executed extraction: {} SOL (sig: {})",
                            amount, record.signature
                        );
                        Ok(ProposalStatus::AutoExecuted)
                    }
                    Err(e) => {
                        warn!("AI extraction failed: {}", e);
                        Err(e.to_string())
                    }
                }
            }
            ProposedAction::PauseTrading => {
                // Always requires user approval for safety
                Ok(ProposalStatus::Pending)
            }
            ProposedAction::ResumeTrading => {
                // Always requires user approval for safety
                Ok(ProposalStatus::Pending)
            }
            ProposedAction::SkipTrade => {
                // These are advisory only
                Ok(ProposalStatus::Pending)
            }
            ProposedAction::ReducePosition => {
                // Advisory - requires user approval
                Ok(ProposalStatus::Pending)
            }
            ProposedAction::IncreaseBuyAmount => {
                // Advisory - requires user approval
                Ok(ProposalStatus::Pending)
            }
        }
    }

    /// Run analysis and process proposals
    pub async fn run_analysis_cycle(&self) -> Vec<AiProposal> {
        let proposals = self.analyze().await;

        let mut results = Vec::new();

        for mut proposal in proposals {
            // Try to auto-execute if within bounds
            match self.execute_if_authorized(&proposal).await {
                Ok(status) => {
                    proposal.status = status;
                    proposal.status_updated_at = Some(Utc::now());
                }
                Err(e) => {
                    warn!("Proposal execution error: {}", e);
                    proposal.status = ProposalStatus::Rejected;
                    proposal.status_updated_at = Some(Utc::now());
                }
            }

            // Add to wallet manager for tracking
            if proposal.status == ProposalStatus::Pending {
                self.wallet_manager.add_proposal(proposal.clone()).await;
            }

            results.push(proposal);
        }

        results
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = AdvisorConfig::default();
        assert!(config.enabled);
        assert_eq!(config.profit_extraction_trigger, 0.3);
        assert_eq!(config.min_confidence_auto, 0.8);
    }
}
