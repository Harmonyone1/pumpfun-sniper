//! Data enrichment service using Helius API
//!
//! Provides background enrichment of token and wallet data to populate
//! the cache and exit degraded mode.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::timeout;
use tracing::{debug, info, warn};
use crate::filter::cache::FilterCache;
use crate::filter::helius::HeliusClient;
use crate::filter::types::SignalContext;

/// Request to enrich data for a token
#[derive(Debug, Clone)]
pub struct EnrichmentRequest {
    pub mint: String,
    pub creator: String,
    pub priority: EnrichmentPriority,
}

/// Priority level for enrichment requests
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnrichmentPriority {
    /// High priority - needed for immediate trading decision
    High,
    /// Normal priority - background enrichment
    Normal,
    /// Low priority - opportunistic enrichment
    Low,
}

/// Configuration for the enrichment service
#[derive(Debug, Clone)]
pub struct EnrichmentConfig {
    /// Maximum concurrent enrichment requests
    pub max_concurrent: usize,
    /// Timeout for individual API calls
    pub api_timeout_ms: u64,
    /// Default number of holders to fetch
    pub holder_limit: u32,
    /// Default number of wallet transactions to fetch
    pub wallet_tx_limit: u32,
    /// Whether to fetch mint authority info
    pub fetch_mint_info: bool,
    /// Whether to fetch creator wallet history
    pub fetch_creator_history: bool,
    /// Whether to fetch token holders
    pub fetch_holders: bool,
}

impl Default for EnrichmentConfig {
    fn default() -> Self {
        Self {
            max_concurrent: 5,
            api_timeout_ms: 5000,
            holder_limit: 20,
            wallet_tx_limit: 50,
            fetch_mint_info: true,
            fetch_creator_history: true,
            fetch_holders: true,
        }
    }
}

/// Service that enriches token data using Helius API
pub struct EnrichmentService {
    /// Helius API client
    helius: Arc<HeliusClient>,
    /// Shared cache to populate
    cache: Arc<FilterCache>,
    /// Configuration
    config: EnrichmentConfig,
}

impl EnrichmentService {
    /// Create a new enrichment service
    pub fn new(helius: HeliusClient, cache: Arc<FilterCache>, config: EnrichmentConfig) -> Self {
        Self {
            helius: Arc::new(helius),
            cache,
            config,
        }
    }

    /// Create from RPC URL (convenience method)
    pub fn from_rpc_url(
        rpc_url: &str,
        cache: Arc<FilterCache>,
        config: EnrichmentConfig,
    ) -> Option<Self> {
        HeliusClient::from_rpc_url(rpc_url).map(|helius| Self::new(helius, cache, config))
    }

    /// Enrich data for a new token (synchronous, for hot path)
    ///
    /// This fetches critical data needed for scoring decisions.
    /// Returns true if enrichment was successful.
    pub async fn enrich_token(&self, context: &SignalContext) -> bool {
        let mint = &context.mint;
        let creator = &context.creator;

        debug!(mint = %mint, creator = %creator, "Starting token enrichment");

        let timeout_duration = Duration::from_millis(self.config.api_timeout_ms);
        let mut success_count = 0;
        let mut total_count = 0;

        // Fetch mint authority info (critical for safety)
        if self.config.fetch_mint_info && self.cache.get_mint_info(mint).is_none() {
            total_count += 1;
            match timeout(timeout_duration, self.helius.get_mint_info(mint)).await {
                Ok(Ok(info)) => {
                    debug!(
                        mint = %mint,
                        mint_authority = ?info.mint_authority,
                        freeze_authority = ?info.freeze_authority,
                        "Fetched mint info"
                    );
                    self.cache.set_mint_info(mint, info);
                    success_count += 1;
                }
                Ok(Err(e)) => {
                    warn!(mint = %mint, error = %e, "Failed to fetch mint info");
                }
                Err(_) => {
                    warn!(mint = %mint, "Mint info request timed out");
                }
            }
        }

        // Fetch creator wallet history
        if self.config.fetch_creator_history && self.cache.get_wallet(creator).is_none() {
            total_count += 1;
            match timeout(
                timeout_duration,
                self.helius.get_wallet_history(creator, self.config.wallet_tx_limit),
            )
            .await
            {
                Ok(Ok(history)) => {
                    debug!(
                        creator = %creator,
                        total_trades = history.total_trades,
                        "Fetched creator wallet history"
                    );
                    self.cache.set_wallet(creator, history);
                    success_count += 1;
                }
                Ok(Err(e)) => {
                    warn!(creator = %creator, error = %e, "Failed to fetch wallet history");
                }
                Err(_) => {
                    warn!(creator = %creator, "Wallet history request timed out");
                }
            }
        }

        // Fetch token holders
        if self.config.fetch_holders && self.cache.get_holders(mint).is_none() {
            total_count += 1;
            match timeout(
                timeout_duration,
                self.helius.get_token_holders(mint, self.config.holder_limit),
            )
            .await
            {
                Ok(Ok(holders)) => {
                    debug!(
                        mint = %mint,
                        holder_count = holders.len(),
                        "Fetched token holders"
                    );
                    self.cache.set_holders(mint, holders);
                    success_count += 1;
                }
                Ok(Err(e)) => {
                    warn!(mint = %mint, error = %e, "Failed to fetch holders");
                }
                Err(_) => {
                    warn!(mint = %mint, "Holders request timed out");
                }
            }
        }

        let success = success_count == total_count && total_count > 0;
        if success {
            debug!(
                mint = %mint,
                fetched = success_count,
                "Token enrichment complete"
            );
        } else if total_count > 0 {
            debug!(
                mint = %mint,
                fetched = success_count,
                total = total_count,
                "Partial token enrichment"
            );
        }

        success
    }

    /// Enrich a single wallet (for background processing)
    pub async fn enrich_wallet(&self, address: &str) -> bool {
        if self.cache.get_wallet(address).is_some() {
            return true; // Already cached
        }

        let timeout_duration = Duration::from_millis(self.config.api_timeout_ms);

        match timeout(
            timeout_duration,
            self.helius.get_wallet_history(address, self.config.wallet_tx_limit),
        )
        .await
        {
            Ok(Ok(history)) => {
                debug!(
                    address = %address,
                    total_trades = history.total_trades,
                    "Fetched wallet history"
                );
                self.cache.set_wallet(address, history);
                true
            }
            Ok(Err(e)) => {
                warn!(address = %address, error = %e, "Failed to fetch wallet history");
                false
            }
            Err(_) => {
                warn!(address = %address, "Wallet history request timed out");
                false
            }
        }
    }

    /// Get the Helius client
    pub fn helius(&self) -> &HeliusClient {
        &self.helius
    }

    /// Get the cache
    pub fn cache(&self) -> &Arc<FilterCache> {
        &self.cache
    }
}

/// Background worker that processes enrichment requests
pub struct EnrichmentWorker {
    service: Arc<EnrichmentService>,
    receiver: mpsc::Receiver<EnrichmentRequest>,
}

impl EnrichmentWorker {
    /// Create a new worker with a channel receiver
    pub fn new(service: Arc<EnrichmentService>, receiver: mpsc::Receiver<EnrichmentRequest>) -> Self {
        Self { service, receiver }
    }

    /// Run the worker (consumes self)
    pub async fn run(mut self) {
        info!("Enrichment worker started");

        while let Some(request) = self.receiver.recv().await {
            debug!(
                mint = %request.mint,
                creator = %request.creator,
                priority = ?request.priority,
                "Processing enrichment request"
            );

            // Create a minimal context for enrichment
            let context = SignalContext::from_new_token(
                request.mint.clone(),
                String::new(),
                String::new(),
                String::new(),
                request.creator.clone(),
                String::new(),
                0,
                0,
                0,
                0.0,
            );

            let success = self.service.enrich_token(&context).await;

            if success {
                debug!(mint = %request.mint, "Enrichment request completed");
            } else {
                debug!(mint = %request.mint, "Enrichment request partially failed");
            }
        }

        info!("Enrichment worker stopped");
    }
}

/// Handle for sending enrichment requests
#[derive(Clone)]
pub struct EnrichmentHandle {
    sender: mpsc::Sender<EnrichmentRequest>,
}

impl EnrichmentHandle {
    /// Create a new handle
    pub fn new(sender: mpsc::Sender<EnrichmentRequest>) -> Self {
        Self { sender }
    }

    /// Request enrichment for a token
    pub async fn request_enrichment(
        &self,
        mint: String,
        creator: String,
        priority: EnrichmentPriority,
    ) -> bool {
        let request = EnrichmentRequest {
            mint,
            creator,
            priority,
        };

        self.sender.send(request).await.is_ok()
    }
}

/// Create an enrichment service with a background worker
///
/// Returns the service (for synchronous use) and a handle (for async requests)
pub fn create_enrichment_system(
    helius: HeliusClient,
    cache: Arc<FilterCache>,
    config: EnrichmentConfig,
) -> (Arc<EnrichmentService>, EnrichmentHandle, EnrichmentWorker) {
    let (sender, receiver) = mpsc::channel(100);

    let service = Arc::new(EnrichmentService::new(helius, cache, config));
    let handle = EnrichmentHandle::new(sender);
    let worker = EnrichmentWorker::new(service.clone(), receiver);

    (service, handle, worker)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_enrichment_config_default() {
        let config = EnrichmentConfig::default();
        assert_eq!(config.max_concurrent, 5);
        assert_eq!(config.holder_limit, 20);
        assert!(config.fetch_mint_info);
    }
}
