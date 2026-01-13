//! Helius API client for enriched on-chain data
//!
//! Provides access to:
//! - Wallet transaction history (for creator analysis)
//! - Token holder data (for distribution scoring)
//! - Enhanced transaction parsing

use chrono::{DateTime, Utc};
use reqwest::Client;
use serde::Deserialize;
use std::time::Duration;
use tracing::debug;

use crate::error::{Error, Result};
use crate::filter::types::{TokenHolderInfo, WalletHistory, WalletTrade};

/// Helius API client
pub struct HeliusClient {
    /// HTTP client
    client: Client,
    /// API key
    api_key: String,
    /// Base URL for REST API
    rest_base_url: String,
    /// Base URL for RPC API
    rpc_base_url: String,
    /// Request timeout
    timeout: Duration,
}

impl HeliusClient {
    /// Create a new Helius client
    pub fn new(api_key: String) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("Failed to create HTTP client");

        Self {
            client,
            api_key: api_key.clone(),
            rest_base_url: "https://api.helius.xyz".to_string(),
            rpc_base_url: format!("https://mainnet.helius-rpc.com/?api-key={}", api_key),
            timeout: Duration::from_secs(10),
        }
    }

    /// Create from RPC URL (extracts API key)
    pub fn from_rpc_url(rpc_url: &str) -> Option<Self> {
        // Extract API key from URL like "https://mainnet.helius-rpc.com/?api-key=xxx"
        if let Some(key_start) = rpc_url.find("api-key=") {
            let key = &rpc_url[key_start + 8..];
            // Handle case where there might be more params after
            let key = key.split('&').next().unwrap_or(key);
            if !key.is_empty() {
                return Some(Self::new(key.to_string()));
            }
        }
        None
    }

    /// Fetch wallet transaction history
    ///
    /// Returns recent transactions for analysis of trading patterns
    pub async fn get_wallet_history(&self, address: &str, limit: u32) -> Result<WalletHistory> {
        let url = format!(
            "{}/v0/addresses/{}/transactions?api-key={}&limit={}",
            self.rest_base_url, address, self.api_key, limit
        );

        debug!("Fetching wallet history for {}", address);

        let response = self
            .client
            .get(&url)
            .timeout(self.timeout)
            .send()
            .await
            .map_err(|e| Error::Rpc(format!("Helius request failed: {}", e)))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(Error::Rpc(format!("Helius API error {}: {}", status, body)));
        }

        let transactions: Vec<HeliusTransaction> = response
            .json()
            .await
            .map_err(|e| Error::Serialization(format!("Failed to parse Helius response: {}", e)))?;

        // Convert to our internal format
        let trades = self.extract_trades_from_transactions(&transactions, address);
        let (total_trades, winning_trades, total_volume) = self.calculate_stats(&trades);

        Ok(WalletHistory {
            address: address.to_string(),
            first_seen: transactions
                .last()
                .and_then(|t| t.timestamp)
                .map(|ts| DateTime::from_timestamp(ts, 0).unwrap_or_else(|| Utc::now())),
            total_trades,
            winning_trades,
            total_volume_sol: total_volume,
            recent_trades: trades,
            // Extended stats (default - would need deeper analysis)
            tokens_deployed: 0,
            tokens_traded: 0,
            avg_holding_time_secs: 0,
            avg_position_size_sol: 0.0,
            // Behavioral patterns
            avg_time_to_first_buy_secs: None,
            sells_within_10_min: 0,
            // Risk indicators
            deployed_rug_count: 0,
            associated_wallets: Vec::new(),
            cluster_id: None,
            // Cache metadata
            fetched_at: Utc::now(),
        })
    }

    /// Fetch token holders for a mint
    ///
    /// Returns top holders and distribution metrics
    pub async fn get_token_holders(&self, mint: &str, limit: u32) -> Result<Vec<TokenHolderInfo>> {
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "helius-holders",
            "method": "getTokenAccounts",
            "params": {
                "page": 1,
                "limit": limit,
                "mint": mint,
                "options": {
                    "showZeroBalance": false
                }
            }
        });

        debug!("Fetching token holders for {}", mint);

        let response = self
            .client
            .post(&self.rpc_base_url)
            .json(&request)
            .timeout(self.timeout)
            .send()
            .await
            .map_err(|e| Error::Rpc(format!("Helius RPC request failed: {}", e)))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(Error::Rpc(format!("Helius RPC error {}: {}", status, body)));
        }

        let rpc_response: HeliusRpcResponse<TokenAccountsResult> = response
            .json()
            .await
            .map_err(|e| Error::Serialization(format!("Failed to parse RPC response: {}", e)))?;

        if let Some(error) = rpc_response.error {
            return Err(Error::Rpc(format!("Helius RPC error: {}", error.message)));
        }

        let result = rpc_response
            .result
            .ok_or_else(|| Error::Rpc("No result in Helius RPC response".to_string()))?;

        // Convert to our format
        let holders: Vec<TokenHolderInfo> = result
            .token_accounts
            .into_iter()
            .map(|account| TokenHolderInfo {
                address: account.owner,
                amount: account.amount,
                percentage: 0.0, // Will be calculated after we have total
            })
            .collect();

        // Calculate percentages
        let total: u64 = holders.iter().map(|h| h.amount).sum();
        let holders: Vec<TokenHolderInfo> = holders
            .into_iter()
            .map(|mut h| {
                h.percentage = if total > 0 {
                    (h.amount as f64 / total as f64) * 100.0
                } else {
                    0.0
                };
                h
            })
            .collect();

        Ok(holders)
    }

    /// Get mint account info to check authorities
    pub async fn get_mint_info(&self, mint: &str) -> Result<MintInfo> {
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "helius-mint",
            "method": "getAccountInfo",
            "params": [
                mint,
                {
                    "encoding": "jsonParsed"
                }
            ]
        });

        debug!("Fetching mint info for {}", mint);

        let response = self
            .client
            .post(&self.rpc_base_url)
            .json(&request)
            .timeout(self.timeout)
            .send()
            .await
            .map_err(|e| Error::Rpc(format!("Helius RPC request failed: {}", e)))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(Error::Rpc(format!("Helius RPC error {}: {}", status, body)));
        }

        let rpc_response: HeliusRpcResponse<AccountInfoResult> = response
            .json()
            .await
            .map_err(|e| Error::Serialization(format!("Failed to parse RPC response: {}", e)))?;

        if let Some(error) = rpc_response.error {
            return Err(Error::Rpc(format!("Helius RPC error: {}", error.message)));
        }

        let result = rpc_response
            .result
            .ok_or_else(|| Error::Rpc("No result in mint info response".to_string()))?;

        let value = result
            .value
            .ok_or_else(|| Error::Rpc("Mint account not found".to_string()))?;

        // Parse the mint data
        if let Some(parsed) = value.data.parsed {
            if let Some(info) = parsed.info {
                return Ok(MintInfo {
                    mint: mint.to_string(),
                    mint_authority: info.mint_authority,
                    freeze_authority: info.freeze_authority,
                    supply: info.supply.parse().unwrap_or(0),
                    decimals: info.decimals,
                });
            }
        }

        Err(Error::Rpc("Failed to parse mint info".to_string()))
    }

    /// Extract trades from Helius transactions
    fn extract_trades_from_transactions(
        &self,
        transactions: &[HeliusTransaction],
        wallet: &str,
    ) -> Vec<WalletTrade> {
        let mut trades = Vec::new();

        for tx in transactions {
            // Look for swap/trade type transactions
            if let Some(ref tx_type) = tx.r#type {
                let is_trade = tx_type.contains("SWAP")
                    || tx_type.contains("TRADE")
                    || tx_type.contains("BUY")
                    || tx_type.contains("SELL");

                if is_trade {
                    // Extract SOL amount from native transfers
                    let sol_amount = tx
                        .native_transfers
                        .as_ref()
                        .map(|transfers| {
                            transfers
                                .iter()
                                .filter(|t| {
                                    t.from_user_account.as_deref() == Some(wallet)
                                        || t.to_user_account.as_deref() == Some(wallet)
                                })
                                .map(|t| t.amount as f64 / 1e9)
                                .sum::<f64>()
                        })
                        .unwrap_or(0.0);

                    // Extract token mint from token transfers
                    let token_mint = tx
                        .token_transfers
                        .as_ref()
                        .and_then(|transfers| transfers.first().map(|t| t.mint.clone()));

                    if sol_amount > 0.0 {
                        trades.push(WalletTrade {
                            signature: tx.signature.clone(),
                            timestamp: tx.timestamp.and_then(|ts| DateTime::from_timestamp(ts, 0)),
                            is_buy: tx_type.contains("BUY")
                                || tx
                                    .native_transfers
                                    .as_ref()
                                    .map(|t| {
                                        t.iter().any(|tr| {
                                            tr.from_user_account.as_deref() == Some(wallet)
                                        })
                                    })
                                    .unwrap_or(false),
                            sol_amount,
                            token_mint,
                            profit_sol: None, // Would need more analysis
                        });
                    }
                }
            }
        }

        trades
    }

    /// Calculate trading statistics from trades
    fn calculate_stats(&self, trades: &[WalletTrade]) -> (u32, u32, f64) {
        let total_trades = trades.len() as u32;
        let winning_trades = trades
            .iter()
            .filter(|t| t.profit_sol.map(|p| p > 0.0).unwrap_or(false))
            .count() as u32;
        let total_volume = trades.iter().map(|t| t.sol_amount).sum();

        (total_trades, winning_trades, total_volume)
    }

    /// Get funding transfers (SOL received) for a wallet
    ///
    /// Used for bundled wallet detection - wallets funded from same source
    pub async fn get_funding_transfers(
        &self,
        address: &str,
        limit: u32,
    ) -> Result<Vec<SolTransfer>> {
        let url = format!(
            "{}/v0/addresses/{}/transactions?api-key={}&limit={}",
            self.rest_base_url, address, self.api_key, limit
        );

        debug!("Fetching funding transfers for {}", address);

        let response = self
            .client
            .get(&url)
            .timeout(self.timeout)
            .send()
            .await
            .map_err(|e| Error::Rpc(format!("Helius request failed: {}", e)))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(Error::Rpc(format!("Helius API error {}: {}", status, body)));
        }

        let transactions: Vec<HeliusTransaction> = response
            .json()
            .await
            .map_err(|e| Error::Serialization(format!("Failed to parse Helius response: {}", e)))?;

        // Extract SOL transfers TO this wallet (funding)
        let mut transfers = Vec::new();
        for tx in &transactions {
            if let Some(ref native) = tx.native_transfers {
                for transfer in native {
                    // Only incoming transfers (TO this wallet)
                    if transfer.to_user_account.as_deref() == Some(address) {
                        if let Some(ref from) = transfer.from_user_account {
                            transfers.push(SolTransfer {
                                signature: tx.signature.clone(),
                                from: from.clone(),
                                to: address.to_string(),
                                amount_lamports: transfer.amount,
                                amount_sol: transfer.amount as f64 / 1e9,
                                timestamp: tx.timestamp.and_then(|ts| {
                                    DateTime::from_timestamp(ts, 0)
                                }),
                            });
                        }
                    }
                }
            }
        }

        Ok(transfers)
    }
}

/// SOL transfer record for funding analysis
#[derive(Debug, Clone)]
pub struct SolTransfer {
    pub signature: String,
    pub from: String,
    pub to: String,
    pub amount_lamports: u64,
    pub amount_sol: f64,
    pub timestamp: Option<DateTime<Utc>>,
}

// ============ Helius API Response Types ============
// These structs are for API deserialization - not all fields are used but are required for parsing

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct HeliusTransaction {
    signature: String,
    #[serde(rename = "type")]
    r#type: Option<String>,
    timestamp: Option<i64>,
    #[serde(rename = "nativeTransfers")]
    native_transfers: Option<Vec<NativeTransfer>>,
    #[serde(rename = "tokenTransfers")]
    token_transfers: Option<Vec<TokenTransfer>>,
    fee: Option<u64>,
    #[serde(rename = "feePayer")]
    fee_payer: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct NativeTransfer {
    #[serde(rename = "fromUserAccount")]
    from_user_account: Option<String>,
    #[serde(rename = "toUserAccount")]
    to_user_account: Option<String>,
    amount: u64,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct TokenTransfer {
    #[serde(rename = "fromUserAccount")]
    from_user_account: Option<String>,
    #[serde(rename = "toUserAccount")]
    to_user_account: Option<String>,
    mint: String,
    #[serde(rename = "tokenAmount")]
    token_amount: f64,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct HeliusRpcResponse<T> {
    jsonrpc: String,
    id: String,
    result: Option<T>,
    error: Option<RpcError>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct RpcError {
    code: i32,
    message: String,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct TokenAccountsResult {
    total: u32,
    limit: u32,
    #[serde(rename = "token_accounts")]
    token_accounts: Vec<TokenAccount>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct TokenAccount {
    address: String,
    mint: String,
    owner: String,
    amount: u64,
    #[serde(rename = "delegated_amount")]
    delegated_amount: Option<u64>,
    frozen: bool,
}

#[derive(Debug, Deserialize)]
struct AccountInfoResult {
    value: Option<AccountValue>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct AccountValue {
    data: AccountData,
    owner: String,
}

#[derive(Debug, Deserialize)]
struct AccountData {
    parsed: Option<ParsedData>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct ParsedData {
    info: Option<MintInfoData>,
    #[serde(rename = "type")]
    data_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MintInfoData {
    decimals: u8,
    #[serde(rename = "freezeAuthority")]
    freeze_authority: Option<String>,
    #[serde(rename = "mintAuthority")]
    mint_authority: Option<String>,
    supply: String,
}

/// Parsed mint information
#[derive(Debug, Clone)]
pub struct MintInfo {
    pub mint: String,
    pub mint_authority: Option<String>,
    pub freeze_authority: Option<String>,
    pub supply: u64,
    pub decimals: u8,
}

impl MintInfo {
    /// Check if mint authority is active (can mint more tokens)
    pub fn has_mint_authority(&self) -> bool {
        self.mint_authority.is_some()
    }

    /// Check if freeze authority is active (can freeze accounts)
    pub fn has_freeze_authority(&self) -> bool {
        self.freeze_authority.is_some()
    }

    /// Check if all authorities are renounced
    pub fn is_fully_renounced(&self) -> bool {
        self.mint_authority.is_none() && self.freeze_authority.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_from_rpc_url() {
        let url = "https://mainnet.helius-rpc.com/?api-key=test123";
        let client = HeliusClient::from_rpc_url(url);
        assert!(client.is_some());
        assert_eq!(client.unwrap().api_key, "test123");
    }

    #[test]
    fn test_from_rpc_url_no_key() {
        let url = "https://api.mainnet-beta.solana.com";
        let client = HeliusClient::from_rpc_url(url);
        assert!(client.is_none());
    }
}
