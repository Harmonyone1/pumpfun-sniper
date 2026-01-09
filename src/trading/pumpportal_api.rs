//! PumpPortal Trading API client
//!
//! PumpPortal provides a simple HTTP API for executing trades on pump.fun.
//! This is an alternative to building transactions manually.
//!
//! API Documentation: https://pumpportal.fun/trading-api/
//!
//! Fee: 0.5% per trade
//! Rate limits apply - don't spam requests

use reqwest::Client;
use serde::{Deserialize, Serialize};
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    signature::{Keypair, Signer},
    transaction::VersionedTransaction,
};
use tracing::{debug, info};

use crate::error::{Error, Result};

/// PumpPortal Lightning API endpoint
pub const PUMPPORTAL_API_URL: &str = "https://pumpportal.fun/api/trade";

/// PumpPortal Local Transaction API endpoint (build your own tx)
pub const PUMPPORTAL_LOCAL_API_URL: &str = "https://pumpportal.fun/api/trade-local";

/// Trade action
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TradeAction {
    Buy,
    Sell,
}

/// Pool type for trading
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PoolType {
    Pump,
    Raydium,
    #[serde(rename = "pump-amm")]
    PumpAmm,
    Auto,
}

impl Default for PoolType {
    fn default() -> Self {
        Self::Pump
    }
}

/// Trade request for Lightning API
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TradeRequest {
    /// "buy" or "sell"
    pub action: TradeAction,
    /// Token mint address
    pub mint: String,
    /// Amount (SOL for buy, tokens or percentage for sell)
    pub amount: String,
    /// true if amount is in SOL
    pub denominated_in_sol: String,
    /// Slippage percentage (e.g., 25 for 25%)
    pub slippage: u32,
    /// Priority fee in SOL
    pub priority_fee: f64,
    /// Pool to use
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pool: Option<PoolType>,
}

/// Trade response from Lightning API
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TradeResponse {
    /// Transaction signature (if successful)
    pub signature: Option<String>,
    /// Error message (if failed)
    pub error: Option<String>,
    /// Additional errors
    pub errors: Option<Vec<String>>,
}

/// Local trade request (returns unsigned transaction)
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalTradeRequest {
    /// "buy" or "sell"
    pub action: TradeAction,
    /// Token mint address
    pub mint: String,
    /// Amount
    pub amount: String,
    /// true if amount is in SOL
    pub denominated_in_sol: String,
    /// Slippage percentage
    pub slippage: u32,
    /// Priority fee in SOL
    pub priority_fee: f64,
    /// Public key of the trader
    pub public_key: String,
    /// Pool to use
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pool: Option<PoolType>,
}

/// Local trade response (unsigned transaction)
#[derive(Debug, Clone, Deserialize)]
pub struct LocalTradeResponse {
    /// Base64 encoded unsigned transaction
    pub transaction: Option<String>,
    /// Error message
    pub error: Option<String>,
}

/// PumpPortal trading API client
pub struct PumpPortalTrader {
    client: Client,
    api_key: Option<String>,
    #[allow(dead_code)]
    use_local_api: bool,
}

impl PumpPortalTrader {
    /// Create a new PumpPortal trader
    ///
    /// # Arguments
    /// * `api_key` - Optional API key for Lightning API (required for Lightning)
    /// * `use_local_api` - Use local API (sign transactions yourself) vs Lightning API
    pub fn new(api_key: Option<String>, use_local_api: bool) -> Self {
        Self {
            client: Client::new(),
            api_key,
            use_local_api,
        }
    }

    /// Create a trader for Lightning API (easiest, 0.5% fee)
    pub fn lightning(api_key: String) -> Self {
        Self::new(Some(api_key), false)
    }

    /// Create a trader for Local API (sign yourself, no API key needed)
    pub fn local() -> Self {
        Self::new(None, true)
    }

    /// Execute a buy using Lightning API
    ///
    /// # Arguments
    /// * `mint` - Token mint address
    /// * `sol_amount` - Amount of SOL to spend
    /// * `slippage_pct` - Slippage percentage (e.g., 25 for 25%)
    /// * `priority_fee` - Priority fee in SOL
    pub async fn buy(
        &self,
        mint: &str,
        sol_amount: f64,
        slippage_pct: u32,
        priority_fee: f64,
    ) -> Result<String> {
        let api_key = self
            .api_key
            .as_ref()
            .ok_or_else(|| Error::Config("API key required for Lightning API".to_string()))?;

        let request = TradeRequest {
            action: TradeAction::Buy,
            mint: mint.to_string(),
            amount: sol_amount.to_string(),
            denominated_in_sol: "true".to_string(),
            slippage: slippage_pct,
            priority_fee,
            pool: Some(PoolType::Pump),
        };

        info!(
            "Executing buy: {} SOL for token {}",
            sol_amount, mint
        );

        let response = self
            .client
            .post(format!("{}?api-key={}", PUMPPORTAL_API_URL, api_key))
            .json(&request)
            .send()
            .await
            .map_err(|e| Error::TransactionSend(format!("HTTP request failed: {}", e)))?;

        let trade_response: TradeResponse = response
            .json()
            .await
            .map_err(|e| Error::Deserialization(format!("Failed to parse response: {}", e)))?;

        if let Some(error) = trade_response.error {
            return Err(Error::TransactionSend(error));
        }

        if let Some(errors) = trade_response.errors {
            if !errors.is_empty() {
                return Err(Error::TransactionSend(errors.join(", ")));
            }
        }

        trade_response
            .signature
            .ok_or_else(|| Error::TransactionSend("No signature in response".to_string()))
    }

    /// Execute a sell using Lightning API
    ///
    /// # Arguments
    /// * `mint` - Token mint address
    /// * `amount` - Amount to sell (can be percentage like "100%" or token amount)
    /// * `slippage_pct` - Slippage percentage
    /// * `priority_fee` - Priority fee in SOL
    pub async fn sell(
        &self,
        mint: &str,
        amount: &str,
        slippage_pct: u32,
        priority_fee: f64,
    ) -> Result<String> {
        let api_key = self
            .api_key
            .as_ref()
            .ok_or_else(|| Error::Config("API key required for Lightning API".to_string()))?;

        // Check if amount is percentage
        let denominated_in_sol = if amount.ends_with('%') {
            "false"
        } else {
            "false" // Token amount, not SOL
        };

        let request = TradeRequest {
            action: TradeAction::Sell,
            mint: mint.to_string(),
            amount: amount.to_string(),
            denominated_in_sol: denominated_in_sol.to_string(),
            slippage: slippage_pct,
            priority_fee,
            pool: Some(PoolType::Pump),
        };

        info!("Executing sell: {} of token {}", amount, mint);

        let response = self
            .client
            .post(format!("{}?api-key={}", PUMPPORTAL_API_URL, api_key))
            .json(&request)
            .send()
            .await
            .map_err(|e| Error::TransactionSend(format!("HTTP request failed: {}", e)))?;

        let trade_response: TradeResponse = response
            .json()
            .await
            .map_err(|e| Error::Deserialization(format!("Failed to parse response: {}", e)))?;

        if let Some(error) = trade_response.error {
            return Err(Error::TransactionSend(error));
        }

        trade_response
            .signature
            .ok_or_else(|| Error::TransactionSend("No signature in response".to_string()))
    }

    /// Get unsigned transaction for buy (Local API)
    ///
    /// Use this if you want to sign the transaction yourself
    /// Returns raw transaction bytes (the API returns binary data directly)
    pub async fn get_buy_transaction(
        &self,
        mint: &str,
        sol_amount: f64,
        slippage_pct: u32,
        priority_fee: f64,
        public_key: &str,
    ) -> Result<Vec<u8>> {
        let request = LocalTradeRequest {
            action: TradeAction::Buy,
            mint: mint.to_string(),
            amount: sol_amount.to_string(),
            denominated_in_sol: "true".to_string(),
            slippage: slippage_pct,
            priority_fee,
            public_key: public_key.to_string(),
            pool: Some(PoolType::Pump),
        };

        debug!("Getting buy transaction from Local API");

        let response = self
            .client
            .post(PUMPPORTAL_LOCAL_API_URL)
            .json(&request)
            .send()
            .await
            .map_err(|e| Error::TransactionBuild(format!("HTTP request failed: {}", e)))?;

        // Check for error response (JSON with error field)
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(Error::TransactionBuild(format!("API error ({}): {}", status, text)));
        }

        // The API returns raw transaction bytes directly
        let tx_bytes = response
            .bytes()
            .await
            .map_err(|e| Error::TransactionBuild(format!("Failed to read response body: {}", e)))?;

        if tx_bytes.is_empty() {
            return Err(Error::TransactionBuild("Empty response from API".to_string()));
        }

        Ok(tx_bytes.to_vec())
    }

    /// Get unsigned transaction for sell (Local API)
    /// Returns raw transaction bytes
    pub async fn get_sell_transaction(
        &self,
        mint: &str,
        amount: &str,
        slippage_pct: u32,
        priority_fee: f64,
        public_key: &str,
    ) -> Result<Vec<u8>> {
        let denominated_in_sol = if amount.ends_with('%') { "false" } else { "false" };

        let request = LocalTradeRequest {
            action: TradeAction::Sell,
            mint: mint.to_string(),
            amount: amount.to_string(),
            denominated_in_sol: denominated_in_sol.to_string(),
            slippage: slippage_pct,
            priority_fee,
            public_key: public_key.to_string(),
            pool: Some(PoolType::Pump),
        };

        debug!("Getting sell transaction from Local API");

        let response = self
            .client
            .post(PUMPPORTAL_LOCAL_API_URL)
            .json(&request)
            .send()
            .await
            .map_err(|e| Error::TransactionBuild(format!("HTTP request failed: {}", e)))?;

        // Check for error response
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(Error::TransactionBuild(format!("API error ({}): {}", status, text)));
        }

        // The API returns raw transaction bytes directly
        let tx_bytes = response
            .bytes()
            .await
            .map_err(|e| Error::TransactionBuild(format!("Failed to read response body: {}", e)))?;

        if tx_bytes.is_empty() {
            return Err(Error::TransactionBuild("Empty response from API".to_string()));
        }

        Ok(tx_bytes.to_vec())
    }

    /// Execute a buy using Local API (sign and send yourself)
    ///
    /// This method gets an unsigned transaction from PumpPortal, signs it locally,
    /// and submits it via the provided RPC client.
    ///
    /// # Arguments
    /// * `mint` - Token mint address
    /// * `sol_amount` - Amount of SOL to spend
    /// * `slippage_pct` - Slippage percentage (e.g., 25 for 25%)
    /// * `priority_fee` - Priority fee in SOL
    /// * `keypair` - Keypair to sign the transaction
    /// * `rpc_client` - RPC client to send the transaction
    pub async fn buy_local(
        &self,
        mint: &str,
        sol_amount: f64,
        slippage_pct: u32,
        priority_fee: f64,
        keypair: &Keypair,
        rpc_client: &RpcClient,
    ) -> Result<String> {
        let public_key = keypair.pubkey().to_string();

        info!(
            "Executing local buy: {} SOL for token {} (signer: {})",
            sol_amount, mint, public_key
        );

        // Get unsigned transaction from PumpPortal Local API (returns raw bytes)
        let tx_bytes = self
            .get_buy_transaction(mint, sol_amount, slippage_pct, priority_fee, &public_key)
            .await?;

        debug!("Got unsigned transaction from PumpPortal ({} bytes)", tx_bytes.len());

        // Deserialize as VersionedTransaction
        let mut tx: VersionedTransaction = bincode::deserialize(&tx_bytes)
            .map_err(|e| Error::Deserialization(format!("Failed to deserialize transaction: {}", e)))?;

        debug!("Deserialized transaction, signing...");

        // Sign the transaction
        // For VersionedTransaction, we need to sign the message
        let message_bytes = tx.message.serialize();
        let signature = keypair.sign_message(&message_bytes);
        tx.signatures[0] = signature;

        debug!("Transaction signed, sending...");

        // Send the signed transaction with skip_preflight to avoid simulation
        use solana_client::rpc_config::RpcSendTransactionConfig;
        use solana_sdk::commitment_config::CommitmentLevel;

        let config = RpcSendTransactionConfig {
            skip_preflight: true,
            preflight_commitment: Some(CommitmentLevel::Confirmed),
            ..Default::default()
        };

        let signature = rpc_client
            .send_transaction_with_config(&tx, config)
            .map_err(|e| Error::TransactionSend(format!("RPC send failed: {}", e)))?;

        info!("Transaction sent! Signature: {}", signature);

        Ok(signature.to_string())
    }

    /// Execute a sell using Local API (sign and send yourself)
    pub async fn sell_local(
        &self,
        mint: &str,
        amount: &str,
        slippage_pct: u32,
        priority_fee: f64,
        keypair: &Keypair,
        rpc_client: &RpcClient,
    ) -> Result<String> {
        let public_key = keypair.pubkey().to_string();

        info!(
            "Executing local sell: {} of token {} (signer: {})",
            amount, mint, public_key
        );

        // Get unsigned transaction from PumpPortal Local API (returns raw bytes)
        let tx_bytes = self
            .get_sell_transaction(mint, amount, slippage_pct, priority_fee, &public_key)
            .await?;

        debug!("Got unsigned transaction from PumpPortal ({} bytes)", tx_bytes.len());

        // Deserialize as VersionedTransaction
        let mut tx: VersionedTransaction = bincode::deserialize(&tx_bytes)
            .map_err(|e| Error::Deserialization(format!("Failed to deserialize transaction: {}", e)))?;

        debug!("Deserialized transaction, signing...");

        // Sign the transaction
        let message_bytes = tx.message.serialize();
        let signature = keypair.sign_message(&message_bytes);
        tx.signatures[0] = signature;

        debug!("Transaction signed, sending...");

        // Send the signed transaction with skip_preflight to avoid simulation
        use solana_client::rpc_config::RpcSendTransactionConfig;
        use solana_sdk::commitment_config::CommitmentLevel;

        let config = RpcSendTransactionConfig {
            skip_preflight: true,
            preflight_commitment: Some(CommitmentLevel::Confirmed),
            ..Default::default()
        };

        let signature = rpc_client
            .send_transaction_with_config(&tx, config)
            .map_err(|e| Error::TransactionSend(format!("RPC send failed: {}", e)))?;

        info!("Transaction sent! Signature: {}", signature);

        Ok(signature.to_string())
    }
}

/// Quick buy helper - simplest way to buy
pub async fn quick_buy(
    api_key: &str,
    mint: &str,
    sol_amount: f64,
) -> Result<String> {
    let trader = PumpPortalTrader::lightning(api_key.to_string());
    trader.buy(mint, sol_amount, 25, 0.0005).await
}

/// Quick sell helper - simplest way to sell all
pub async fn quick_sell_all(
    api_key: &str,
    mint: &str,
) -> Result<String> {
    let trader = PumpPortalTrader::lightning(api_key.to_string());
    trader.sell(mint, "100%", 25, 0.0005).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_trade_request_serialization() {
        let request = TradeRequest {
            action: TradeAction::Buy,
            mint: "DYw8jCTfwHNRJhhmFcbXvVDTqWMEVFBX6ZKUmG5CNSKK".to_string(),
            amount: "0.01".to_string(),
            denominated_in_sol: "true".to_string(),
            slippage: 25,
            priority_fee: 0.0005,
            pool: Some(PoolType::Pump),
        };

        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"action\":\"buy\""));
        assert!(json.contains("\"denominatedInSol\":\"true\""));
    }

    #[test]
    fn test_sell_percentage() {
        let request = TradeRequest {
            action: TradeAction::Sell,
            mint: "test".to_string(),
            amount: "100%".to_string(),
            denominated_in_sol: "false".to_string(),
            slippage: 25,
            priority_fee: 0.0005,
            pool: None,
        };

        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"amount\":\"100%\""));
    }
}
