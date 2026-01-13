//! Price feed for position monitoring
//!
//! Polls bonding curve accounts to get current token prices.
//! Falls back to DexScreener API for graduated tokens.
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
use crate::dexscreener::DexScreenerClient;
use crate::error::{Error, Result};
use crate::pump::accounts::BondingCurve;

/// Token price source - bonding curve or DexScreener
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PriceSource {
    /// Token is on pump.fun bonding curve
    BondingCurve,
    /// Token graduated - using DexScreener API
    DexScreener,
}

/// Monitored token info
#[derive(Debug, Clone)]
pub struct MonitoredToken {
    pub mint: Pubkey,
    pub bonding_curve: Pubkey,
    pub source: PriceSource,
}

/// Price update event
#[derive(Debug, Clone)]
pub struct PriceUpdate {
    pub mint: Pubkey,
    pub bonding_curve: Pubkey,
    pub price: f64,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

/// Price feed that polls bonding curves for current prices
/// Falls back to DexScreener API for graduated tokens
pub struct PriceFeed {
    rpc_client: Arc<RpcClient>,
    config: AutoSellConfig,
    /// Tokens being monitored: mint -> MonitoredToken
    monitored: Arc<RwLock<HashMap<Pubkey, MonitoredToken>>>,
    /// Cached prices
    prices: Arc<RwLock<HashMap<Pubkey, f64>>>,
    /// DexScreener client for graduated tokens
    dexscreener: Arc<DexScreenerClient>,
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
            dexscreener: Arc::new(DexScreenerClient::new()),
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
            "Starting price feed with {}ms poll interval (with DexScreener fallback)",
            self.config.price_poll_interval_ms
        );

        let rpc_client = self.rpc_client.clone();
        let monitored = self.monitored.clone();
        let prices = self.prices.clone();
        let dexscreener = self.dexscreener.clone();
        let poll_interval = Duration::from_millis(self.config.price_poll_interval_ms);
        let mut shutdown_rx = self.shutdown.subscribe();

        tokio::spawn(async move {
            let mut interval = interval(poll_interval);

            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        // Get tokens to poll
                        let tokens: Vec<MonitoredToken> = {
                            let guard = monitored.read().await;
                            guard.values().cloned().collect()
                        };

                        if tokens.is_empty() {
                            continue;
                        }

                        // Poll each token using appropriate source
                        for token in tokens {
                            let price_result = match token.source {
                                PriceSource::BondingCurve => {
                                    // Try bonding curve first
                                    match Self::fetch_bonding_curve_price(&rpc_client, &token.bonding_curve).await {
                                        Ok((price, graduated)) => {
                                            if graduated {
                                                // Token has graduated, update source and try DexScreener
                                                info!("Token {} graduated, switching to DexScreener", token.mint);
                                                let mut guard = monitored.write().await;
                                                if let Some(t) = guard.get_mut(&token.mint) {
                                                    t.source = PriceSource::DexScreener;
                                                }
                                                drop(guard);
                                                Self::fetch_dexscreener_price(&dexscreener, &token.mint).await
                                            } else {
                                                Ok(price)
                                            }
                                        }
                                        Err(e) => {
                                            // Bonding curve failed, try DexScreener
                                            debug!("Bonding curve fetch failed for {}: {}, trying DexScreener", token.mint, e);
                                            let mut guard = monitored.write().await;
                                            if let Some(t) = guard.get_mut(&token.mint) {
                                                t.source = PriceSource::DexScreener;
                                            }
                                            drop(guard);
                                            Self::fetch_dexscreener_price(&dexscreener, &token.mint).await
                                        }
                                    }
                                }
                                PriceSource::DexScreener => {
                                    Self::fetch_dexscreener_price(&dexscreener, &token.mint).await
                                }
                            };

                            match price_result {
                                Ok(price) => {
                                    // Update cache
                                    {
                                        let mut cache = prices.write().await;
                                        cache.insert(token.mint, price);
                                    }

                                    // Send update
                                    let update = PriceUpdate {
                                        mint: token.mint,
                                        bonding_curve: token.bonding_curve,
                                        price,
                                        timestamp: chrono::Utc::now(),
                                    };

                                    if update_tx.send(update).await.is_err() {
                                        debug!("Price update channel closed");
                                        return;
                                    }
                                }
                                Err(e) => {
                                    warn!("Failed to fetch price for {} from any source: {}", token.mint, e);
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

    /// Add a token to monitor (starts with bonding curve, auto-switches to DexScreener if graduated)
    pub async fn add_token(&self, mint: Pubkey, bonding_curve: Pubkey) {
        let token = MonitoredToken {
            mint,
            bonding_curve,
            source: PriceSource::BondingCurve, // Start with bonding curve, auto-detect graduation
        };
        let mut monitored = self.monitored.write().await;
        monitored.insert(mint, token);
        info!("Added {} to price feed (will auto-detect graduation)", mint);
    }

    /// Add a token that's already known to be graduated
    pub async fn add_graduated_token(&self, mint: Pubkey, bonding_curve: Pubkey) {
        let token = MonitoredToken {
            mint,
            bonding_curve,
            source: PriceSource::DexScreener,
        };
        let mut monitored = self.monitored.write().await;
        monitored.insert(mint, token);
        info!("Added {} to price feed (using DexScreener)", mint);
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

    /// Get the current price source for a token
    pub async fn get_price_source(&self, mint: &Pubkey) -> Option<PriceSource> {
        let monitored = self.monitored.read().await;
        monitored.get(mint).map(|t| t.source)
    }

    /// Fetch price from bonding curve, also returns whether the curve is complete (graduated)
    async fn fetch_bonding_curve_price(
        rpc_client: &RpcClient,
        bonding_curve: &Pubkey,
    ) -> Result<(f64, bool)> {
        let account = rpc_client
            .get_account(bonding_curve)
            .map_err(|e| Error::Rpc(format!("Failed to fetch bonding curve: {}", e)))?;

        let curve = BondingCurve::try_from_slice(&account.data)?;
        let price = curve.get_price()?;
        Ok((price, curve.complete))
    }

    /// Fetch price from DexScreener API (for graduated tokens)
    async fn fetch_dexscreener_price(
        dexscreener: &DexScreenerClient,
        mint: &Pubkey,
    ) -> Result<f64> {
        let mint_str = mint.to_string();
        let token_info = dexscreener
            .get_token_info(&mint_str)
            .await
            .map_err(|e| Error::Rpc(format!("DexScreener API error: {}", e)))?;

        match token_info {
            Some(info) if info.price_native > 0.0 => {
                debug!(
                    "Got DexScreener price for {}: {} SOL",
                    mint, info.price_native
                );
                Ok(info.price_native)
            }
            Some(_) => Err(Error::Rpc("DexScreener returned zero price".to_string())),
            None => Err(Error::Rpc("Token not found on DexScreener".to_string())),
        }
    }

    /// Check if a token has graduated by querying its bonding curve
    pub async fn check_if_graduated(&self, bonding_curve: &Pubkey) -> Result<bool> {
        match Self::fetch_bonding_curve_price(&self.rpc_client, bonding_curve).await {
            Ok((_, graduated)) => Ok(graduated),
            Err(_) => {
                // If we can't fetch the bonding curve, assume it's graduated
                Ok(true)
            }
        }
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
