//! Jito tip management
//!
//! Handles dynamic tip calculation from Jito tip stream.

use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info};

use crate::config::JitoConfig;
use crate::error::Result;

/// Tip percentiles from Jito tip floor API
#[derive(Debug, Clone, Default)]
pub struct TipPercentiles {
    pub p25: u64,
    pub p50: u64,
    pub p75: u64,
    pub p95: u64,
    pub p99: u64,
    pub ema: u64,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

/// Tip manager for dynamic tip calculation
pub struct TipManager {
    config: JitoConfig,
    current_tips: Arc<RwLock<TipPercentiles>>,
}

impl TipManager {
    pub fn new(config: JitoConfig) -> Self {
        Self {
            config,
            current_tips: Arc::new(RwLock::new(TipPercentiles::default())),
        }
    }

    /// Start the tip stream listener
    pub async fn start(&self) -> Result<()> {
        info!("Starting tip manager...");

        // TODO: Connect to Jito tip stream WebSocket
        // wss://bundles.jito.wtf/api/v1/bundles/tip_stream

        Ok(())
    }

    /// Get recommended tip based on configured percentile
    pub async fn get_recommended_tip(&self) -> u64 {
        let tips = self.current_tips.read().await;

        let base_tip = match self.config.tip_percentile {
            p if p <= 25 => tips.p25,
            p if p <= 50 => tips.p50,
            p if p <= 75 => tips.p75,
            p if p <= 95 => tips.p95,
            _ => tips.p99,
        };

        // If no data yet, use minimum
        let tip = if base_tip == 0 {
            self.config.min_tip_lamports
        } else {
            base_tip
        };

        // Clamp to configured bounds
        tip.clamp(self.config.min_tip_lamports, self.config.max_tip_lamports)
    }

    /// Get current tip percentiles
    pub async fn get_percentiles(&self) -> TipPercentiles {
        self.current_tips.read().await.clone()
    }

    /// Update tip percentiles (called when new data received)
    pub async fn update_percentiles(&self, percentiles: TipPercentiles) {
        let mut tips = self.current_tips.write().await;
        *tips = percentiles;
        debug!("Updated tip percentiles: p50={}", tips.p50);
    }

    /// Fetch current tips from REST API
    pub async fn fetch_tips(&self) -> Result<TipPercentiles> {
        // TODO: Fetch from https://bundles.jito.wtf/api/v1/bundles/tip_floor

        Ok(TipPercentiles {
            p25: 1000,
            p50: 5000,
            p75: 10000,
            p95: 50000,
            p99: 100000,
            ema: 7500,
            timestamp: chrono::Utc::now(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> JitoConfig {
        JitoConfig {
            block_engine_url: "https://test".to_string(),
            regions: vec!["ny".to_string()],
            tip_percentile: 50,
            min_tip_lamports: 1000,
            max_tip_lamports: 100000,
            retry_attempts: 3,
            retry_base_delay_ms: 50,
        }
    }

    #[tokio::test]
    async fn test_tip_manager() {
        let manager = TipManager::new(test_config());

        // Should return minimum when no data
        let tip = manager.get_recommended_tip().await;
        assert_eq!(tip, 1000);

        // Update with data
        manager
            .update_percentiles(TipPercentiles {
                p25: 2000,
                p50: 5000,
                p75: 10000,
                p95: 50000,
                p99: 100000,
                ema: 7500,
                timestamp: chrono::Utc::now(),
            })
            .await;

        // Should now return p50
        let tip = manager.get_recommended_tip().await;
        assert_eq!(tip, 5000);
    }
}
