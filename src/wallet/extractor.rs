//! Profit extraction engine
//!
//! Monitors conditions and triggers automatic profit extraction to vault.
//! Integrates with position manager to extract realized profits.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

use super::manager::WalletManager;
use super::types::{InitiatedBy, TransferReason};
use crate::position::manager::PositionManager;

/// Extraction configuration
#[derive(Debug, Clone)]
pub struct ExtractionConfig {
    /// Enable automatic extraction
    pub auto_extract: bool,

    /// Extract when realized profit exceeds this threshold
    pub profit_threshold_sol: f64,

    /// Percentage of profits to extract (0-100)
    pub profit_percentage: f64,

    /// Extract excess when balance exceeds this ceiling
    pub balance_ceiling_sol: f64,

    /// Minimum time between extractions in seconds
    pub min_extraction_interval_secs: u64,

    /// Check interval in seconds
    pub check_interval_secs: u64,
}

impl Default for ExtractionConfig {
    fn default() -> Self {
        Self {
            auto_extract: true,
            profit_threshold_sol: 0.2,
            profit_percentage: 50.0,
            balance_ceiling_sol: 2.0,
            min_extraction_interval_secs: 3600, // 1 hour
            check_interval_secs: 60,            // 1 minute
        }
    }
}

/// Profit extractor - monitors and triggers automatic extractions
pub struct ProfitExtractor {
    config: ExtractionConfig,
    wallet_manager: Arc<WalletManager>,
    position_manager: Option<Arc<PositionManager>>,
    last_extraction: Option<chrono::DateTime<Utc>>,
}

impl ProfitExtractor {
    /// Create a new profit extractor
    pub fn new(config: ExtractionConfig, wallet_manager: Arc<WalletManager>) -> Self {
        Self {
            config,
            wallet_manager,
            position_manager: None,
            last_extraction: None,
        }
    }

    /// Create a new profit extractor with position manager integration
    pub fn with_position_manager(
        config: ExtractionConfig,
        wallet_manager: Arc<WalletManager>,
        position_manager: Arc<PositionManager>,
    ) -> Self {
        Self {
            config,
            wallet_manager,
            position_manager: Some(position_manager),
            last_extraction: None,
        }
    }

    /// Start the extraction monitoring loop
    pub async fn start(&mut self, mut shutdown: broadcast::Receiver<()>) {
        if !self.config.auto_extract {
            info!("Auto-extraction disabled");
            return;
        }

        info!(
            "Starting profit extractor (profit threshold: {} SOL, balance ceiling: {} SOL)",
            self.config.profit_threshold_sol, self.config.balance_ceiling_sol
        );

        let mut interval =
            tokio::time::interval(Duration::from_secs(self.config.check_interval_secs));

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    if let Err(e) = self.check_extraction_rules().await {
                        error!("Extraction check failed: {}", e);
                    }
                }
                _ = shutdown.recv() => {
                    info!("Profit extractor shutting down");
                    break;
                }
            }
        }
    }

    /// Check all extraction rules
    async fn check_extraction_rules(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        // Check if emergency locked
        if self.wallet_manager.is_emergency_locked().await {
            debug!("Skipping extraction check - emergency lock active");
            return Ok(());
        }

        // Check cooldown
        if let Some(last) = self.last_extraction {
            let elapsed = Utc::now().signed_duration_since(last);
            if elapsed.num_seconds() < self.config.min_extraction_interval_secs as i64 {
                debug!(
                    "Skipping extraction - cooldown active ({} seconds remaining)",
                    self.config.min_extraction_interval_secs as i64 - elapsed.num_seconds()
                );
                return Ok(());
            }
        }

        // Get current hot wallet balance
        let hot_balance = match self.wallet_manager.hot_balance().await {
            Ok(b) => b,
            Err(e) => {
                warn!("Failed to get hot balance: {}", e);
                return Ok(());
            }
        };

        // Rule 1: Balance ceiling
        if hot_balance > self.config.balance_ceiling_sol {
            let excess = hot_balance - self.config.balance_ceiling_sol;
            info!(
                "Balance ceiling exceeded: {} SOL > {} SOL ceiling, extracting {} SOL",
                hot_balance, self.config.balance_ceiling_sol, excess
            );

            self.trigger_extraction(excess, "balance_ceiling").await?;
            return Ok(());
        }

        // Rule 2: Profit threshold - extract realized profits if above threshold
        let pending_profits = if let Some(ref pm) = self.position_manager {
            pm.get_pending_extraction().await
        } else {
            0.0
        };

        if pending_profits >= self.config.profit_threshold_sol {
            // Calculate extraction amount (percentage of profits)
            let extract_amount = pending_profits * (self.config.profit_percentage / 100.0);

            // Ensure we don't extract more than available
            let safe_extract = extract_amount.min(hot_balance - 0.1); // Keep minimum 0.1 SOL

            if safe_extract > 0.01 {
                // Minimum extraction of 0.01 SOL
                info!(
                    "Realized profits threshold reached: {:.4} SOL pending, extracting {:.4} SOL ({:.0}%)",
                    pending_profits, safe_extract, self.config.profit_percentage
                );

                if self.trigger_extraction(safe_extract, "realized_profits").await.is_ok() {
                    // Mark profits as extracted in position manager
                    if let Some(ref pm) = self.position_manager {
                        pm.mark_profits_extracted(safe_extract).await;
                    }
                }
                return Ok(());
            }
        }

        debug!(
            "Extraction check complete - hot balance: {} SOL, no extraction triggered",
            hot_balance
        );

        Ok(())
    }

    /// Trigger an extraction
    async fn trigger_extraction(
        &mut self,
        amount: f64,
        rule: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        info!("Triggering extraction: {} SOL (rule: {})", amount, rule);

        let result = self
            .wallet_manager
            .extract_to_vault(
                amount,
                TransferReason::ProfitExtraction,
                InitiatedBy::AutoRule {
                    rule: rule.to_string(),
                },
                true, // Force (auto-extractions don't need confirmation)
            )
            .await;

        match result {
            Ok(record) => {
                info!(
                    "Auto-extraction successful: {} SOL (sig: {})",
                    record.amount_sol, record.signature
                );
                self.last_extraction = Some(Utc::now());
            }
            Err(e) => {
                warn!("Auto-extraction failed: {}", e);
                // Don't update last_extraction on failure
            }
        }

        Ok(())
    }

    /// Manually trigger extraction check
    pub async fn trigger_check(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.check_extraction_rules().await
    }

    /// Get time until next possible extraction
    pub fn time_until_next_extraction(&self) -> Option<Duration> {
        self.last_extraction.map(|last| {
            let elapsed = Utc::now().signed_duration_since(last);
            let remaining = self.config.min_extraction_interval_secs as i64 - elapsed.num_seconds();
            if remaining > 0 {
                Duration::from_secs(remaining as u64)
            } else {
                Duration::ZERO
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = ExtractionConfig::default();
        assert!(config.auto_extract);
        assert_eq!(config.profit_threshold_sol, 0.2);
        assert_eq!(config.profit_percentage, 50.0);
        assert_eq!(config.balance_ceiling_sol, 2.0);
    }
}
