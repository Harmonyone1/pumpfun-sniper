//! Price feed for position monitoring
//!
//! Polls bonding curve accounts to get current token prices.
//! This is used for auto-sell (take-profit / stop-loss) triggers.
//!
//! WARNING: TP/SL is best-effort, not guaranteed. At 1-second polling,
//! fast rugs can gap through your stop-loss before detection.

use solana_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, RwLock};
use tokio::time::interval;
use tracing::{debug, info, warn};

use crate::config::AutoSellConfig;
use crate::error::{Error, Result};
use crate::pump::accounts::BondingCurve;

/// Price update event
#[derive(Debug, Clone)]
pub struct PriceUpdate {
    pub mint: Pubkey,
    pub bonding_curve: Pubkey,
    pub price: f64,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

/// Price feed that polls bonding curves for current prices
pub struct PriceFeed {
    rpc_client: Arc<RpcClient>,
    config: AutoSellConfig,
    /// Tokens being monitored: mint -> bonding_curve
    monitored: Arc<RwLock<HashMap<Pubkey, Pubkey>>>,
    /// Cached prices
    prices: Arc<RwLock<HashMap<Pubkey, f64>>>,
    /// Shutdown signal
    shutdown: tokio::sync::broadcast::Sender<()>,
}

impl PriceFeed {
    pub fn new(rpc_client: Arc<RpcClient>, config: AutoSellConfig) -> Self {
        let (shutdown, _) = tokio::sync::broadcast::channel(1);

        Self {
            rpc_client,
            config,
            monitored: Arc::new(RwLock::new(HashMap::new())),
            prices: Arc::new(RwLock::new(HashMap::new())),
            shutdown,
        }
    }

    /// Start the price feed polling loop
    pub async fn start(&self, update_tx: mpsc::Sender<PriceUpdate>) -> Result<()> {
        if !self.config.enabled {
            info!("Price feed disabled (auto-sell is off)");
            return Ok(());
        }

        info!(
            "Starting price feed with {}ms poll interval",
            self.config.price_poll_interval_ms
        );

        let rpc_client = self.rpc_client.clone();
        let monitored = self.monitored.clone();
        let prices = self.prices.clone();
        let poll_interval = Duration::from_millis(self.config.price_poll_interval_ms);
        let mut shutdown_rx = self.shutdown.subscribe();

        tokio::spawn(async move {
            let mut interval = interval(poll_interval);

            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        // Get tokens to poll
                        let tokens: Vec<(Pubkey, Pubkey)> = {
                            let guard = monitored.read().await;
                            guard.iter().map(|(m, b)| (*m, *b)).collect()
                        };

                        if tokens.is_empty() {
                            continue;
                        }

                        // Poll each bonding curve
                        for (mint, bonding_curve) in tokens {
                            match Self::fetch_price(&rpc_client, &bonding_curve).await {
                                Ok(price) => {
                                    // Update cache
                                    {
                                        let mut cache = prices.write().await;
                                        cache.insert(mint, price);
                                    }

                                    // Send update
                                    let update = PriceUpdate {
                                        mint,
                                        bonding_curve,
                                        price,
                                        timestamp: chrono::Utc::now(),
                                    };

                                    if update_tx.send(update).await.is_err() {
                                        debug!("Price update channel closed");
                                        return;
                                    }
                                }
                                Err(e) => {
                                    warn!("Failed to fetch price for {}: {}", mint, e);
                                }
                            }
                        }
                    }
                    _ = shutdown_rx.recv() => {
                        info!("Price feed shutting down");
                        break;
                    }
                }
            }
        });

        Ok(())
    }

    /// Stop the price feed
    pub fn stop(&self) {
        let _ = self.shutdown.send(());
    }

    /// Add a token to monitor
    pub async fn add_token(&self, mint: Pubkey, bonding_curve: Pubkey) {
        let mut monitored = self.monitored.write().await;
        monitored.insert(mint, bonding_curve);
        info!("Added {} to price feed", mint);
    }

    /// Remove a token from monitoring
    pub async fn remove_token(&self, mint: &Pubkey) {
        let mut monitored = self.monitored.write().await;
        monitored.remove(mint);

        let mut prices = self.prices.write().await;
        prices.remove(mint);

        info!("Removed {} from price feed", mint);
    }

    /// Get cached price for a token
    pub async fn get_price(&self, mint: &Pubkey) -> Option<f64> {
        let prices = self.prices.read().await;
        prices.get(mint).copied()
    }

    /// Get all cached prices
    pub async fn get_all_prices(&self) -> HashMap<Pubkey, f64> {
        self.prices.read().await.clone()
    }

    /// Fetch price from bonding curve
    async fn fetch_price(rpc_client: &RpcClient, bonding_curve: &Pubkey) -> Result<f64> {
        let account = rpc_client
            .get_account(bonding_curve)
            .map_err(|e| Error::Rpc(format!("Failed to fetch bonding curve: {}", e)))?;

        let curve = BondingCurve::try_from_slice(&account.data)?;
        curve.get_price()
    }

    /// Get number of monitored tokens
    pub async fn monitored_count(&self) -> usize {
        self.monitored.read().await.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> AutoSellConfig {
        AutoSellConfig {
            enabled: true,
            take_profit_pct: 50.0,
            stop_loss_pct: 30.0,
            partial_take_profit: false,
            price_poll_interval_ms: 1000,
        }
    }

    #[tokio::test]
    async fn test_add_remove_token() {
        let rpc = Arc::new(RpcClient::new("https://api.mainnet-beta.solana.com"));
        let feed = PriceFeed::new(rpc, test_config());

        let mint = Pubkey::new_unique();
        let curve = Pubkey::new_unique();

        feed.add_token(mint, curve).await;
        assert_eq!(feed.monitored_count().await, 1);

        feed.remove_token(&mint).await;
        assert_eq!(feed.monitored_count().await, 0);
    }
}
