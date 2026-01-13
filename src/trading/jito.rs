//! Jito bundle submission client
//!
//! Handles submitting transaction bundles to Jito block engine
//! with automatic retry and tip optimization.

use reqwest::Client;
use serde::{Deserialize, Serialize};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::transaction::{Transaction, VersionedTransaction};
use std::str::FromStr;
use std::time::Duration;
use tracing::{debug, error, info, warn};

use crate::config::JitoConfig;
use crate::error::{Error, Result};
use crate::pump::program::JITO_TIP_ACCOUNTS;

/// Jito bundle status
#[derive(Debug, Clone, PartialEq)]
pub enum BundleStatus {
    /// Bundle submitted and pending
    Pending,
    /// Bundle landed on-chain
    Landed,
    /// Bundle failed
    Failed(String),
    /// Bundle status unknown
    Unknown,
}

/// Result of bundle submission
#[derive(Debug, Clone)]
pub struct BundleResult {
    /// Bundle ID from Jito
    pub bundle_id: String,
    /// Status of the bundle
    pub status: BundleStatus,
    /// Transaction signatures in the bundle
    pub signatures: Vec<String>,
}

/// JSON-RPC request structure
#[derive(Serialize)]
struct JsonRpcRequest<T> {
    jsonrpc: &'static str,
    id: u64,
    method: &'static str,
    params: T,
}

/// JSON-RPC response structure
#[derive(Deserialize)]
struct JsonRpcResponse<T> {
    #[allow(dead_code)]
    jsonrpc: String,
    #[allow(dead_code)]
    id: u64,
    result: Option<T>,
    error: Option<JsonRpcError>,
}

#[derive(Deserialize, Debug)]
struct JsonRpcError {
    code: i64,
    message: String,
}

/// Bundle status response from Jito
#[derive(Deserialize, Debug)]
struct BundleStatusResponse {
    #[serde(default)]
    bundle_id: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    landed_slot: Option<u64>,
}

/// Alternative status response format (Jito sometimes uses this)
#[derive(Deserialize, Debug)]
struct BundleStatusContext {
    context: Option<serde_json::Value>,
    value: Option<Vec<BundleStatusItem>>,
}

#[derive(Deserialize, Debug)]
struct BundleStatusItem {
    #[serde(default)]
    bundle_id: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    confirmation_status: Option<String>,
    #[serde(default)]
    landed_slot: Option<u64>,
}

/// Jito client for bundle submission
pub struct JitoClient {
    config: JitoConfig,
    tip_accounts: Vec<Pubkey>,
    http_client: Client,
}

impl JitoClient {
    /// Create a new Jito client
    pub fn new(config: JitoConfig) -> Result<Self> {
        let tip_accounts = JITO_TIP_ACCOUNTS
            .iter()
            .map(|s| Pubkey::from_str(s))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| Error::Config(format!("Invalid tip account: {}", e)))?;

        let http_client = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| Error::Config(format!("Failed to create HTTP client: {}", e)))?;

        info!("Jito client initialized for {}", config.block_engine_url);

        Ok(Self {
            config,
            tip_accounts,
            http_client,
        })
    }

    /// Submit a bundle of legacy transactions
    pub async fn submit_bundle(&self, transactions: Vec<Transaction>) -> Result<BundleResult> {
        if transactions.is_empty() {
            return Err(Error::JitoBundleSubmission("Empty bundle".to_string()));
        }

        if transactions.len() > 5 {
            return Err(Error::JitoBundleSubmission(
                "Bundle cannot contain more than 5 transactions".to_string(),
            ));
        }

        // Encode transactions as base58 (Jito requires base58)
        let encoded_txs: Vec<String> = transactions
            .iter()
            .map(|tx| {
                let serialized = bincode::serialize(tx)
                    .map_err(|e| Error::Serialization(format!("Failed to serialize tx: {}", e)))?;
                Ok(bs58::encode(&serialized).into_string())
            })
            .collect::<Result<Vec<_>>>()?;

        let signatures: Vec<String> = transactions
            .iter()
            .filter_map(|tx| tx.signatures.first().map(|s| s.to_string()))
            .collect();

        self.send_bundle_request(encoded_txs, signatures).await
    }

    /// Submit a bundle of versioned transactions (from PumpPortal)
    pub async fn submit_versioned_bundle(&self, transactions: Vec<VersionedTransaction>) -> Result<BundleResult> {
        if transactions.is_empty() {
            return Err(Error::JitoBundleSubmission("Empty bundle".to_string()));
        }

        if transactions.len() > 5 {
            return Err(Error::JitoBundleSubmission(
                "Bundle cannot contain more than 5 transactions".to_string(),
            ));
        }

        // Encode transactions as base58 (Jito requires base58)
        let encoded_txs: Vec<String> = transactions
            .iter()
            .map(|tx| {
                let serialized = bincode::serialize(tx)
                    .map_err(|e| Error::Serialization(format!("Failed to serialize tx: {}", e)))?;
                Ok(bs58::encode(&serialized).into_string())
            })
            .collect::<Result<Vec<_>>>()?;

        let signatures: Vec<String> = transactions
            .iter()
            .filter_map(|tx| tx.signatures.first().map(|s| s.to_string()))
            .collect();

        self.send_bundle_request(encoded_txs, signatures).await
    }

    /// Submit a mixed bundle: one versioned transaction + one legacy transaction (for tip)
    pub async fn submit_bundle_mixed(
        &self,
        versioned_tx: VersionedTransaction,
        legacy_tx: Transaction,
    ) -> Result<BundleResult> {
        // Encode versioned transaction
        let versioned_bytes = bincode::serialize(&versioned_tx)
            .map_err(|e| Error::Serialization(format!("Failed to serialize versioned tx: {}", e)))?;
        let versioned_encoded = bs58::encode(&versioned_bytes).into_string();

        // Encode legacy transaction
        let legacy_bytes = bincode::serialize(&legacy_tx)
            .map_err(|e| Error::Serialization(format!("Failed to serialize legacy tx: {}", e)))?;
        let legacy_encoded = bs58::encode(&legacy_bytes).into_string();

        let encoded_txs = vec![versioned_encoded, legacy_encoded];

        let mut signatures = Vec::new();
        if let Some(sig) = versioned_tx.signatures.first() {
            signatures.push(sig.to_string());
        }
        if let Some(sig) = legacy_tx.signatures.first() {
            signatures.push(sig.to_string());
        }

        self.send_bundle_request(encoded_txs, signatures).await
    }

    /// Send bundle request to Jito
    async fn send_bundle_request(&self, encoded_txs: Vec<String>, signatures: Vec<String>) -> Result<BundleResult> {
        let url = format!("{}/api/v1/bundles", self.config.block_engine_url);

        info!("Submitting bundle with {} transactions to Jito", encoded_txs.len());

        // Jito expects array of base64 encoded transactions
        let request = JsonRpcRequest {
            jsonrpc: "2.0",
            id: 1,
            method: "sendBundle",
            params: [&encoded_txs],
        };

        let mut last_error = None;
        let max_attempts = self.config.retry_attempts.max(5); // At least 5 attempts for rate limits

        for attempt in 0..max_attempts {
            if attempt > 0 {
                // Longer delay for rate limits
                let delay = if attempt > 1 {
                    Duration::from_millis(1000 * attempt as u64) // 1s, 2s, 3s...
                } else {
                    Duration::from_millis(self.config.retry_base_delay_ms * (1 << attempt))
                };
                tokio::time::sleep(delay).await;
                debug!("Jito retry attempt {} after {:?}", attempt + 1, delay);
            }

            match self.http_client.post(&url).json(&request).send().await {
                Ok(response) => {
                    let status = response.status();
                    let body = response.text().await.unwrap_or_default();

                    // Handle rate limiting specifically
                    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                        warn!("Jito rate limited (429), waiting longer before retry...");
                        tokio::time::sleep(Duration::from_millis(1500)).await;
                        last_error = Some(Error::JitoBundleSubmission("Rate limited".to_string()));
                        continue;
                    }

                    if status.is_success() {
                        // Parse response
                        match serde_json::from_str::<JsonRpcResponse<String>>(&body) {
                            Ok(resp) => {
                                if let Some(error) = resp.error {
                                    // Check for rate limit in RPC error
                                    if error.code == -32097 {
                                        warn!("Jito rate limit via RPC, waiting...");
                                        tokio::time::sleep(Duration::from_millis(1500)).await;
                                        last_error = Some(Error::JitoBundleSubmission("Rate limited".to_string()));
                                        continue;
                                    }
                                    warn!("Jito RPC error: {} (code: {})", error.message, error.code);
                                    last_error = Some(Error::JitoBundleSubmission(error.message));
                                    continue;
                                }

                                if let Some(bundle_id) = resp.result {
                                    info!("Bundle submitted successfully: {}", bundle_id);
                                    return Ok(BundleResult {
                                        bundle_id,
                                        status: BundleStatus::Pending,
                                        signatures,
                                    });
                                }
                            }
                            Err(e) => {
                                warn!("Failed to parse Jito response: {} - body: {}", e, body);
                                last_error = Some(Error::JitoBundleSubmission(format!(
                                    "Invalid response: {}",
                                    e
                                )));
                            }
                        }
                    } else {
                        warn!("Jito HTTP error {}: {}", status, body);
                        last_error = Some(Error::JitoBundleSubmission(format!(
                            "HTTP {}: {}",
                            status, body
                        )));
                    }
                }
                Err(e) => {
                    warn!("Jito request failed: {}", e);
                    last_error = Some(Error::JitoBundleSubmission(format!("Request failed: {}", e)));
                }
            }
        }

        Err(last_error.unwrap_or_else(|| Error::JitoBundleSubmission("Unknown error".to_string())))
    }

    /// Get bundle status
    pub async fn get_bundle_status(&self, bundle_id: &str) -> Result<BundleStatus> {
        let url = format!("{}/api/v1/bundles", self.config.block_engine_url);

        let request = JsonRpcRequest {
            jsonrpc: "2.0",
            id: 1,
            method: "getBundleStatuses",
            params: [[bundle_id]],
        };

        let response = self
            .http_client
            .post(&url)
            .json(&request)
            .send()
            .await
            .map_err(|e| Error::JitoBundleSubmission(format!("Status request failed: {}", e)))?;

        let body = response.text().await.unwrap_or_default();

        // Log raw response for debugging (truncated)
        debug!("Bundle status response: {}", if body.len() > 200 { &body[..200] } else { &body });

        // Helper to extract status from string
        let parse_status = |s: &str| -> BundleStatus {
            match s.to_lowercase().as_str() {
                "landed" => BundleStatus::Landed,
                "pending" | "processing" => BundleStatus::Pending,
                "failed" | "invalid" | "dropped" => BundleStatus::Failed(s.to_string()),
                "finalized" | "confirmed" => BundleStatus::Landed,
                _ => {
                    debug!("Unknown bundle status string: {}", s);
                    BundleStatus::Unknown
                }
            }
        };

        // Try format 1: { "result": [{ "bundle_id": ..., "status": ... }] }
        if let Ok(resp) = serde_json::from_str::<JsonRpcResponse<Vec<BundleStatusResponse>>>(&body) {
            if let Some(statuses) = resp.result {
                if let Some(status) = statuses.first() {
                    if !status.status.is_empty() {
                        return Ok(parse_status(&status.status));
                    }
                }
            }
        }

        // Try format 2: { "result": { "context": ..., "value": [...] } }
        if let Ok(resp) = serde_json::from_str::<JsonRpcResponse<BundleStatusContext>>(&body) {
            if let Some(ctx) = resp.result {
                if let Some(values) = ctx.value {
                    if let Some(item) = values.first() {
                        // Check confirmation_status first, then status
                        if let Some(conf) = &item.confirmation_status {
                            return Ok(parse_status(conf));
                        }
                        if !item.status.is_empty() {
                            return Ok(parse_status(&item.status));
                        }
                    }
                }
            }
        }

        // Try to extract status from raw JSON using serde_json::Value
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&body) {
            // Look for any "status" or "confirmation_status" field recursively
            if let Some(result) = json.get("result") {
                // Check if result has "value" array
                if let Some(value) = result.get("value") {
                    if let Some(arr) = value.as_array() {
                        if let Some(first) = arr.first() {
                            if let Some(status) = first.get("confirmation_status").and_then(|s| s.as_str()) {
                                return Ok(parse_status(status));
                            }
                            if let Some(status) = first.get("status").and_then(|s| s.as_str()) {
                                return Ok(parse_status(status));
                            }
                        }
                    }
                }
                // Check if result is an array directly
                if let Some(arr) = result.as_array() {
                    if let Some(first) = arr.first() {
                        if let Some(status) = first.get("status").and_then(|s| s.as_str()) {
                            return Ok(parse_status(status));
                        }
                    }
                }
            }
        }

        // Couldn't parse - return unknown
        debug!("Could not parse bundle status from response");
        Ok(BundleStatus::Unknown)
    }

    /// Wait for bundle confirmation with timeout
    pub async fn wait_for_confirmation(&self, bundle_id: &str, timeout_secs: u64) -> Result<BundleStatus> {
        let start = std::time::Instant::now();
        let timeout = Duration::from_secs(timeout_secs);

        while start.elapsed() < timeout {
            let status = self.get_bundle_status(bundle_id).await?;

            match &status {
                BundleStatus::Landed => {
                    info!("Bundle {} landed on-chain!", bundle_id);
                    return Ok(status);
                }
                BundleStatus::Failed(reason) => {
                    error!("Bundle {} failed: {}", bundle_id, reason);
                    return Ok(status);
                }
                BundleStatus::Pending | BundleStatus::Unknown => {
                    debug!("Bundle {} still pending...", bundle_id);
                }
            }

            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        warn!("Bundle {} confirmation timed out", bundle_id);
        Ok(BundleStatus::Unknown)
    }

    /// Get a random tip account
    pub fn get_tip_account(&self) -> Pubkey {
        use rand::Rng;
        let idx = rand::thread_rng().gen_range(0..self.tip_accounts.len());
        self.tip_accounts[idx]
    }

    /// Get recommended tip amount from Jito
    pub async fn get_recommended_tip(&self) -> Result<u64> {
        // Try to fetch from tip floor API
        let url = "https://bundles.jito.wtf/api/v1/bundles/tip_floor";

        match self.http_client.get(url).send().await {
            Ok(response) => {
                if let Ok(floors) = response.json::<Vec<serde_json::Value>>().await {
                    // Get the percentile we want
                    let percentile_key = format!("landed_tips_{}_percentile", self.config.tip_percentile);

                    if let Some(floor) = floors.first() {
                        if let Some(tip) = floor.get(&percentile_key).and_then(|v| v.as_f64()) {
                            let tip_lamports = (tip * 1e9) as u64;
                            return Ok(self.clamp_tip(tip_lamports));
                        }
                    }
                }
            }
            Err(e) => {
                debug!("Failed to fetch tip floor: {}", e);
            }
        }

        // Fallback to configured minimum
        Ok(self.config.min_tip_lamports)
    }

    /// Clamp tip to configured bounds
    pub fn clamp_tip(&self, tip: u64) -> u64 {
        tip.clamp(self.config.min_tip_lamports, self.config.max_tip_lamports)
    }

    /// Get config reference
    pub fn config(&self) -> &JitoConfig {
        &self.config
    }
}

/// Create a tip transfer instruction
pub fn create_tip_instruction(
    from: &Pubkey,
    tip_account: &Pubkey,
    lamports: u64,
) -> solana_sdk::instruction::Instruction {
    solana_sdk::system_instruction::transfer(from, tip_account, lamports)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> JitoConfig {
        JitoConfig {
            block_engine_url: "https://ny.mainnet.block-engine.jito.wtf".to_string(),
            regions: vec!["ny".to_string()],
            tip_percentile: 50,
            min_tip_lamports: 10000,
            max_tip_lamports: 1000000,
            retry_attempts: 3,
            retry_base_delay_ms: 50,
        }
    }

    #[test]
    fn test_jito_client_creation() {
        let client = JitoClient::new(test_config()).unwrap();
        assert_eq!(client.tip_accounts.len(), 8);
    }

    #[test]
    fn test_tip_clamping() {
        let client = JitoClient::new(test_config()).unwrap();

        assert_eq!(client.clamp_tip(5000), 10000); // Below min
        assert_eq!(client.clamp_tip(50000), 50000); // In range
        assert_eq!(client.clamp_tip(2000000), 1000000); // Above max
    }
}
