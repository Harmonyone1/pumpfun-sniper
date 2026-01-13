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
use tracing::{debug, error, info, warn};

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
        self.buy_with_pool(mint, sol_amount, slippage_pct, priority_fee, PoolType::Auto)
            .await
    }

    /// Buy tokens using Lightning API with specific pool
    ///
    /// # Arguments
    /// * `mint` - Token mint address
    /// * `sol_amount` - Amount of SOL to spend
    /// * `slippage_pct` - Slippage percentage (e.g., 25 for 25%)
    /// * `priority_fee` - Priority fee in SOL
    /// * `pool` - Pool type (Pump, Raydium, Auto)
    pub async fn buy_with_pool(
        &self,
        mint: &str,
        sol_amount: f64,
        slippage_pct: u32,
        priority_fee: f64,
        pool: PoolType,
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
            pool: Some(pool),
        };

        info!("Executing buy: {} SOL for token {}", sol_amount, mint);

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

        trade_response.signature.ok_or_else(|| {
            Error::TransactionSend(
                "No signature in response - API returned empty result".to_string(),
            )
        })
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
            pool: Some(PoolType::Auto), // Auto-detect pool (handles graduated tokens)
        };

        info!("Executing sell: {} of token {} (pool: auto)", amount, mint);

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

        // Check for error (singular)
        if let Some(error) = trade_response.error {
            return Err(Error::TransactionSend(error));
        }

        // Check for errors (plural) - API sometimes returns errors as array
        if let Some(errors) = trade_response.errors {
            if !errors.is_empty() {
                return Err(Error::TransactionSend(errors.join("; ")));
            }
        }

        trade_response.signature.ok_or_else(|| {
            Error::TransactionSend(
                "No signature in response - API returned empty result".to_string(),
            )
        })
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
            pool: Some(PoolType::Auto), // Auto-detect pool
        };

        debug!("Getting buy transaction from Local API (pool: auto)");

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
            return Err(Error::TransactionBuild(format!(
                "API error ({}): {}",
                status, text
            )));
        }

        // The API returns raw transaction bytes directly
        let tx_bytes = response
            .bytes()
            .await
            .map_err(|e| Error::TransactionBuild(format!("Failed to read response body: {}", e)))?;

        if tx_bytes.is_empty() {
            return Err(Error::TransactionBuild(
                "Empty response from API".to_string(),
            ));
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
        let denominated_in_sol = if amount.ends_with('%') {
            "false"
        } else {
            "false"
        };

        let request = LocalTradeRequest {
            action: TradeAction::Sell,
            mint: mint.to_string(),
            amount: amount.to_string(),
            denominated_in_sol: denominated_in_sol.to_string(),
            slippage: slippage_pct,
            priority_fee,
            public_key: public_key.to_string(),
            pool: Some(PoolType::Auto), // Auto-detect pool (handles graduated tokens)
        };

        debug!("Getting sell transaction from Local API (pool: auto)");

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
            return Err(Error::TransactionBuild(format!(
                "API error ({}): {}",
                status, text
            )));
        }

        // The API returns raw transaction bytes directly
        let tx_bytes = response
            .bytes()
            .await
            .map_err(|e| Error::TransactionBuild(format!("Failed to read response body: {}", e)))?;

        if tx_bytes.is_empty() {
            return Err(Error::TransactionBuild(
                "Empty response from API".to_string(),
            ));
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

        debug!(
            "Got unsigned transaction from PumpPortal ({} bytes)",
            tx_bytes.len()
        );

        // Deserialize as VersionedTransaction
        let mut tx: VersionedTransaction = bincode::deserialize(&tx_bytes).map_err(|e| {
            Error::Deserialization(format!("Failed to deserialize transaction: {}", e))
        })?;

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

    /// Execute a buy with retry logic and confirmation checking
    ///
    /// Retries with increasing slippage if transaction fails due to slippage exceeded (error 6005).
    /// Waits for transaction confirmation before returning.
    ///
    /// # Arguments
    /// * `mint` - Token mint address
    /// * `sol_amount` - Amount of SOL to spend
    /// * `initial_slippage_pct` - Starting slippage percentage (will increase on retry)
    /// * `priority_fee` - Priority fee in SOL
    /// * `keypair` - Keypair to sign the transaction
    /// * `rpc_client` - RPC client to send the transaction
    /// * `max_retries` - Maximum number of retry attempts (default 3)
    pub async fn buy_local_with_retry(
        &self,
        mint: &str,
        sol_amount: f64,
        initial_slippage_pct: u32,
        priority_fee: f64,
        keypair: &Keypair,
        rpc_client: &RpcClient,
        max_retries: u32,
    ) -> Result<String> {
        use solana_sdk::signature::Signature;
        use std::str::FromStr;
        use std::time::Duration;
        use tokio::time::sleep;

        let mut slippage = initial_slippage_pct.max(30); // Start at minimum 30%
        let slippage_increment = 15; // Add 15% on each retry
        let max_slippage = 75; // Cap at 75% for volatile memecoins

        for attempt in 0..=max_retries {
            if attempt > 0 {
                // Increase slippage for retry
                slippage = (slippage + slippage_increment).min(max_slippage);
                info!(
                    "Retry attempt {} with increased slippage: {}%",
                    attempt, slippage
                );
            }

            // Get fresh transaction with current slippage
            let public_key = keypair.pubkey().to_string();
            let tx_bytes = match self
                .get_buy_transaction(mint, sol_amount, slippage, priority_fee, &public_key)
                .await
            {
                Ok(bytes) => bytes,
                Err(e) => {
                    tracing::warn!("Failed to get transaction: {}", e);
                    if attempt < max_retries {
                        sleep(Duration::from_millis(500)).await;
                        continue;
                    }
                    return Err(e);
                }
            };

            // Deserialize and sign
            let mut tx: VersionedTransaction = bincode::deserialize(&tx_bytes)
                .map_err(|e| Error::Deserialization(format!("Failed to deserialize: {}", e)))?;

            let message_bytes = tx.message.serialize();
            let signature = keypair.sign_message(&message_bytes);
            tx.signatures[0] = signature;

            // Send transaction
            use solana_client::rpc_config::RpcSendTransactionConfig;
            use solana_sdk::commitment_config::CommitmentLevel;

            let config = RpcSendTransactionConfig {
                skip_preflight: true,
                preflight_commitment: Some(CommitmentLevel::Confirmed),
                ..Default::default()
            };

            let sig = match rpc_client.send_transaction_with_config(&tx, config) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("Failed to send transaction: {}", e);
                    if attempt < max_retries {
                        sleep(Duration::from_millis(500)).await;
                        continue;
                    }
                    return Err(Error::TransactionSend(format!("RPC send failed: {}", e)));
                }
            };

            info!("Transaction sent (attempt {}): {}", attempt + 1, sig);

            // Wait for confirmation (up to 30 seconds)
            let sig_parsed = Signature::from_str(&sig.to_string())
                .map_err(|e| Error::Deserialization(format!("Invalid signature: {}", e)))?;

            for _ in 0..30 {
                sleep(Duration::from_secs(1)).await;

                match rpc_client.get_signature_status(&sig_parsed) {
                    Ok(Some(status)) => {
                        match status {
                            Ok(()) => {
                                info!("Transaction CONFIRMED: {}", sig);
                                return Ok(sig.to_string());
                            }
                            Err(tx_err) => {
                                // Check if it's slippage error (Custom 6005)
                                let err_str = format!("{:?}", tx_err);
                                if err_str.contains("6005") || err_str.contains("Slippage") {
                                    tracing::warn!(
                                        "Slippage exceeded on attempt {}, will retry with higher slippage",
                                        attempt + 1
                                    );
                                    break; // Break inner loop to retry
                                } else {
                                    // Other error, don't retry
                                    return Err(Error::TransactionSend(format!(
                                        "Transaction failed: {:?}",
                                        tx_err
                                    )));
                                }
                            }
                        }
                    }
                    Ok(None) => {
                        // Still pending, continue waiting
                        debug!("Transaction still pending...");
                    }
                    Err(e) => {
                        tracing::warn!("Error checking status: {}", e);
                    }
                }
            }

            // If we get here, either timed out or slippage error - retry
            if attempt < max_retries {
                tracing::warn!("Transaction not confirmed, retrying...");
            }
        }

        Err(Error::TransactionSend(format!(
            "Failed after {} attempts - slippage may be too volatile",
            max_retries + 1
        )))
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

        debug!(
            "Got unsigned transaction from PumpPortal ({} bytes)",
            tx_bytes.len()
        );

        // Deserialize as VersionedTransaction
        let mut tx: VersionedTransaction = bincode::deserialize(&tx_bytes).map_err(|e| {
            Error::Deserialization(format!("Failed to deserialize transaction: {}", e))
        })?;

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

    /// Execute a buy using Jito bundles with fallback to regular RPC
    ///
    /// Tries Jito first for MEV protection, falls back to regular RPC if Jito fails.
    pub async fn buy_with_jito(
        &self,
        mint: &str,
        sol_amount: f64,
        slippage_pct: u32,
        keypair: &Keypair,
        jito_client: &crate::trading::jito::JitoClient,
        rpc_client: &RpcClient,
    ) -> Result<String> {
        use crate::trading::jito::BundleStatus;
        use solana_sdk::{message::Message, system_instruction, transaction::Transaction};

        let public_key = keypair.pubkey().to_string();

        info!(
            "Executing Jito buy: {} SOL for token {} (signer: {})",
            sol_amount, mint, public_key
        );

        // Get recommended tip from Jito
        let tip_lamports = jito_client.get_recommended_tip().await.unwrap_or(100000);
        info!(
            "Using Jito tip: {} lamports ({:.6} SOL)",
            tip_lamports,
            tip_lamports as f64 / 1e9
        );

        // Get unsigned transaction from PumpPortal
        let tx_bytes = match self
            .get_buy_transaction(mint, sol_amount, slippage_pct, 0.00001, &public_key)
            .await
        {
            Ok(bytes) => bytes,
            Err(e) => {
                error!("Failed to get transaction from PumpPortal: {}", e);
                return Err(e);
            }
        };

        debug!(
            "Got unsigned transaction from PumpPortal ({} bytes)",
            tx_bytes.len()
        );

        // Deserialize as VersionedTransaction
        let mut buy_tx: VersionedTransaction = bincode::deserialize(&tx_bytes).map_err(|e| {
            Error::Deserialization(format!("Failed to deserialize transaction: {}", e))
        })?;

        // Sign the buy transaction
        let message_bytes = buy_tx.message.serialize();
        let signature = keypair.sign_message(&message_bytes);
        buy_tx.signatures[0] = signature;

        // Try Jito first
        let jito_result = async {
            // Get a random Jito tip account
            let tip_account = jito_client.get_tip_account();

            // Get recent blockhash for tip transaction
            let blockhash = rpc_client
                .get_latest_blockhash()
                .map_err(|e| Error::TransactionBuild(format!("Failed to get blockhash: {}", e)))?;

            // Create tip transaction
            let tip_ix =
                system_instruction::transfer(&keypair.pubkey(), &tip_account, tip_lamports);

            let tip_message = Message::new(&[tip_ix], Some(&keypair.pubkey()));
            let mut tip_tx = Transaction::new_unsigned(tip_message);
            tip_tx.sign(&[keypair], blockhash);

            debug!("Created tip transaction to {}", tip_account);

            // Clone buy_tx for Jito (we need original for fallback)
            let buy_tx_clone: VersionedTransaction =
                bincode::deserialize(&bincode::serialize(&buy_tx).unwrap()).unwrap();

            // Submit bundle
            let bundle_result = jito_client
                .submit_bundle_mixed(buy_tx_clone, tip_tx)
                .await?;
            info!("Bundle submitted: {}", bundle_result.bundle_id);

            // Wait for confirmation (reduced to 15 seconds for faster fallback)
            let status = jito_client
                .wait_for_confirmation(&bundle_result.bundle_id, 15)
                .await?;

            match status {
                BundleStatus::Landed => {
                    let sig = bundle_result
                        .signatures
                        .first()
                        .cloned()
                        .unwrap_or_else(|| bundle_result.bundle_id.clone());
                    info!("Jito BUY CONFIRMED: {}", sig);
                    Ok(sig)
                }
                BundleStatus::Failed(reason) => Err(Error::JitoBundleSubmission(format!(
                    "Bundle failed: {}",
                    reason
                ))),
                _ => {
                    // Check if first signature landed on-chain
                    if let Some(sig) = bundle_result.signatures.first() {
                        use solana_sdk::signature::Signature;
                        use std::str::FromStr;
                        if let Ok(sig_parsed) = Signature::from_str(sig) {
                            if let Ok(Some(status)) = rpc_client.get_signature_status(&sig_parsed) {
                                if status.is_ok() {
                                    info!("Jito tx confirmed via RPC check: {}", sig);
                                    return Ok(sig.clone());
                                }
                            }
                        }
                    }
                    Err(Error::JitoBundleSubmission(
                        "Bundle didn't land".to_string(),
                    ))
                }
            }
        }
        .await;

        // If Jito succeeded, return
        if let Ok(sig) = jito_result {
            return Ok(sig);
        }

        // Fallback to regular RPC
        warn!("Jito bundle failed, falling back to regular RPC...");

        // Get fresh transaction for RPC (new blockhash)
        let priority_fee = jito_client.config().min_tip_lamports as f64 / 1e9;

        match self
            .buy_local_with_retry(
                mint,
                sol_amount,
                slippage_pct,
                priority_fee,
                keypair,
                rpc_client,
                3,
            )
            .await
        {
            Ok(sig) => {
                info!("RPC FALLBACK BUY CONFIRMED: {}", sig);
                Ok(sig)
            }
            Err(e) => {
                error!("RPC fallback also failed: {}", e);
                Err(e)
            }
        }
    }

    /// Execute a sell using Jito bundles with fallback to regular RPC
    ///
    /// Tries Jito first for MEV protection, falls back to regular RPC if Jito fails.
    pub async fn sell_with_jito(
        &self,
        mint: &str,
        amount: &str,
        slippage_pct: u32,
        keypair: &Keypair,
        jito_client: &crate::trading::jito::JitoClient,
        rpc_client: &RpcClient,
    ) -> Result<String> {
        use crate::trading::jito::BundleStatus;
        use solana_sdk::{message::Message, system_instruction, transaction::Transaction};

        let public_key = keypair.pubkey().to_string();

        info!(
            "Executing Jito sell: {} of token {} (signer: {})",
            amount, mint, public_key
        );

        // Get recommended tip from Jito
        let tip_lamports = jito_client.get_recommended_tip().await.unwrap_or(100000);
        info!(
            "Using Jito tip: {} lamports ({:.6} SOL)",
            tip_lamports,
            tip_lamports as f64 / 1e9
        );

        // Get unsigned transaction from PumpPortal (use minimal priority fee since we're using Jito)
        let tx_bytes = match self
            .get_sell_transaction(mint, amount, slippage_pct, 0.00001, &public_key)
            .await
        {
            Ok(bytes) => bytes,
            Err(e) => {
                error!("Failed to get transaction from PumpPortal: {}", e);
                return Err(e);
            }
        };

        debug!(
            "Got unsigned transaction from PumpPortal ({} bytes)",
            tx_bytes.len()
        );

        // Deserialize as VersionedTransaction
        let mut sell_tx: VersionedTransaction = bincode::deserialize(&tx_bytes).map_err(|e| {
            Error::Deserialization(format!("Failed to deserialize transaction: {}", e))
        })?;

        // Sign the sell transaction
        let message_bytes = sell_tx.message.serialize();
        let signature = keypair.sign_message(&message_bytes);
        sell_tx.signatures[0] = signature;

        // Try Jito first
        let jito_result = async {
            // Get a random Jito tip account
            let tip_account = jito_client.get_tip_account();

            // Get recent blockhash for tip transaction
            let blockhash = rpc_client
                .get_latest_blockhash()
                .map_err(|e| Error::TransactionBuild(format!("Failed to get blockhash: {}", e)))?;

            // Create tip transaction
            let tip_ix =
                system_instruction::transfer(&keypair.pubkey(), &tip_account, tip_lamports);

            let tip_message = Message::new(&[tip_ix], Some(&keypair.pubkey()));
            let mut tip_tx = Transaction::new_unsigned(tip_message);
            tip_tx.sign(&[keypair], blockhash);

            debug!("Created tip transaction to {}", tip_account);

            // Clone sell_tx for Jito (we need original for fallback)
            let sell_tx_clone: VersionedTransaction =
                bincode::deserialize(&bincode::serialize(&sell_tx).unwrap()).unwrap();

            // Submit bundle
            let bundle_result = jito_client
                .submit_bundle_mixed(sell_tx_clone, tip_tx)
                .await?;
            info!("Bundle submitted: {}", bundle_result.bundle_id);

            // Wait for confirmation (reduced to 15 seconds for faster fallback)
            let status = jito_client
                .wait_for_confirmation(&bundle_result.bundle_id, 15)
                .await?;

            match status {
                BundleStatus::Landed => {
                    let sig = bundle_result
                        .signatures
                        .first()
                        .cloned()
                        .unwrap_or_else(|| bundle_result.bundle_id.clone());
                    info!("Jito SELL CONFIRMED: {}", sig);
                    Ok(sig)
                }
                BundleStatus::Failed(reason) => Err(Error::JitoBundleSubmission(format!(
                    "Bundle failed: {}",
                    reason
                ))),
                _ => {
                    // Check if first signature landed on-chain
                    if let Some(sig) = bundle_result.signatures.first() {
                        use solana_sdk::signature::Signature;
                        use std::str::FromStr;
                        if let Ok(sig_parsed) = Signature::from_str(sig) {
                            if let Ok(Some(status)) = rpc_client.get_signature_status(&sig_parsed) {
                                if status.is_ok() {
                                    info!("Jito tx confirmed via RPC check: {}", sig);
                                    return Ok(sig.clone());
                                }
                            }
                        }
                    }
                    Err(Error::JitoBundleSubmission(
                        "Bundle didn't land".to_string(),
                    ))
                }
            }
        }
        .await;

        // If Jito succeeded, return
        if let Ok(sig) = jito_result {
            return Ok(sig);
        }

        // Fallback to regular RPC
        warn!("Jito bundle failed, falling back to regular RPC for sell...");

        // Get fresh transaction for RPC (new blockhash) with higher priority fee
        let priority_fee = jito_client.config().min_tip_lamports as f64 / 1e9;

        match self
            .sell_local(
                mint,
                amount,
                slippage_pct,
                priority_fee,
                keypair,
                rpc_client,
            )
            .await
        {
            Ok(sig) => {
                info!("RPC FALLBACK SELL CONFIRMED: {}", sig);
                Ok(sig)
            }
            Err(e) => {
                error!("RPC fallback also failed: {}", e);
                Err(e)
            }
        }
    }

    /// Check if a pump.fun token is tradeable (pool exists and is active)
    /// Returns true if the token can be traded via PumpPortal
    pub async fn check_pool_ready(&self, mint: &str) -> bool {
        // Verify token ends with "pump" (pump.fun convention)
        if !mint.ends_with("pump") {
            debug!(
                "Token {} does not end with 'pump' - not a pump.fun token",
                mint
            );
            return false;
        }

        // Try to get a quote by making a minimal trade request to local API
        // This will fail fast if the pool doesn't exist
        let request = LocalTradeRequest {
            action: TradeAction::Buy,
            mint: mint.to_string(),
            amount: "0.0001".to_string(), // Tiny amount just to check
            denominated_in_sol: "true".to_string(),
            slippage: 50, // High slippage for check
            priority_fee: 0.0001,
            public_key: "11111111111111111111111111111111".to_string(), // Dummy pubkey for check
            pool: Some(PoolType::Pump),
        };

        let response = self
            .client
            .post(PUMPPORTAL_LOCAL_API_URL)
            .json(&request)
            .send()
            .await;

        match response {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    debug!("Pool ready for {}", mint);
                    true
                } else {
                    let text = resp.text().await.unwrap_or_default();
                    if text.contains("Pool account not found") || text.contains("not found") {
                        warn!("Pool NOT ready for {}: {}", mint, text);
                        false
                    } else {
                        // Other errors might be temporary
                        debug!("Pool check unclear for {}: {} - {}", mint, status, text);
                        true // Assume ready if not a clear "not found" error
                    }
                }
            }
            Err(e) => {
                warn!("Pool check failed for {}: {} - assuming not ready", mint, e);
                false
            }
        }
    }
}

/// Quick buy helper - simplest way to buy
pub async fn quick_buy(api_key: &str, mint: &str, sol_amount: f64) -> Result<String> {
    let trader = PumpPortalTrader::lightning(api_key.to_string());
    trader.buy(mint, sol_amount, 25, 0.0005).await
}

/// Quick sell helper - simplest way to sell all
pub async fn quick_sell_all(api_key: &str, mint: &str) -> Result<String> {
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
