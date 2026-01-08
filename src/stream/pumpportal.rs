//! PumpPortal WebSocket client for token detection
//!
//! PumpPortal provides a free WebSocket API for real-time pump.fun data.
//! This is a good alternative while waiting for ShredStream approval.
//!
//! WebSocket endpoint: wss://pumpportal.fun/api/data
//! Documentation: https://pumpportal.fun/data-api/real-time

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::sleep;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

use crate::error::{Error, Result};

/// PumpPortal WebSocket URL
pub const PUMPPORTAL_WS_URL: &str = "wss://pumpportal.fun/api/data";

/// Subscription methods
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscriptionMessage {
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keys: Option<Vec<String>>,
}

impl SubscriptionMessage {
    /// Subscribe to new token creation events
    pub fn subscribe_new_tokens() -> Self {
        Self {
            method: "subscribeNewToken".to_string(),
            keys: None,
        }
    }

    /// Subscribe to trades on specific tokens
    pub fn subscribe_token_trades(mints: Vec<String>) -> Self {
        Self {
            method: "subscribeTokenTrade".to_string(),
            keys: Some(mints),
        }
    }

    /// Subscribe to trades by specific accounts (wallets)
    pub fn subscribe_account_trades(wallets: Vec<String>) -> Self {
        Self {
            method: "subscribeAccountTrade".to_string(),
            keys: Some(wallets),
        }
    }

    /// Unsubscribe from new tokens
    pub fn unsubscribe_new_tokens() -> Self {
        Self {
            method: "unsubscribeNewToken".to_string(),
            keys: None,
        }
    }

    /// Unsubscribe from token trades
    pub fn unsubscribe_token_trades(mints: Vec<String>) -> Self {
        Self {
            method: "unsubscribeTokenTrade".to_string(),
            keys: Some(mints),
        }
    }

    /// Unsubscribe from account trades
    pub fn unsubscribe_account_trades(wallets: Vec<String>) -> Self {
        Self {
            method: "unsubscribeAccountTrade".to_string(),
            keys: Some(wallets),
        }
    }
}

/// New token event from PumpPortal
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewTokenEvent {
    pub signature: String,
    pub mint: String,
    pub trader_public_key: String,
    pub tx_type: String,
    pub initial_buy: u64,
    pub bonding_curve_key: String,
    pub v_tokens_in_bonding_curve: u64,
    pub v_sol_in_bonding_curve: u64,
    pub market_cap_sol: f64,
    pub name: String,
    pub symbol: String,
    pub uri: String,
}

/// Trade event from PumpPortal
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TradeEvent {
    pub signature: String,
    pub mint: String,
    pub trader_public_key: String,
    pub tx_type: String, // "buy" or "sell"
    pub token_amount: u64,
    pub sol_amount: u64,
    pub bonding_curve_key: String,
    pub v_tokens_in_bonding_curve: u64,
    pub v_sol_in_bonding_curve: u64,
    pub market_cap_sol: f64,
}

/// Event from PumpPortal WebSocket
#[derive(Debug, Clone)]
pub enum PumpPortalEvent {
    /// New token created
    NewToken(NewTokenEvent),
    /// Trade occurred (buy or sell)
    Trade(TradeEvent),
    /// Connected to WebSocket
    Connected,
    /// Disconnected from WebSocket
    Disconnected,
    /// Error occurred
    Error(String),
}

/// Configuration for PumpPortal client
#[derive(Debug, Clone)]
pub struct PumpPortalConfig {
    /// WebSocket URL (default: wss://pumpportal.fun/api/data)
    pub ws_url: String,
    /// Reconnect delay in milliseconds
    pub reconnect_delay_ms: u64,
    /// Maximum reconnect attempts (0 = infinite)
    pub max_reconnect_attempts: u32,
    /// Ping interval in seconds
    pub ping_interval_secs: u64,
}

impl Default for PumpPortalConfig {
    fn default() -> Self {
        Self {
            ws_url: PUMPPORTAL_WS_URL.to_string(),
            reconnect_delay_ms: 1000,
            max_reconnect_attempts: 0, // Infinite
            ping_interval_secs: 30,
        }
    }
}

/// PumpPortal WebSocket client
pub struct PumpPortalClient {
    config: PumpPortalConfig,
    event_tx: mpsc::Sender<PumpPortalEvent>,
    shutdown: tokio::sync::broadcast::Sender<()>,
}

impl PumpPortalClient {
    /// Create a new PumpPortal client
    pub fn new(config: PumpPortalConfig, event_tx: mpsc::Sender<PumpPortalEvent>) -> Self {
        let (shutdown, _) = tokio::sync::broadcast::channel(1);

        Self {
            config,
            event_tx,
            shutdown,
        }
    }

    /// Start the WebSocket connection
    pub async fn start(
        &self,
        subscribe_new_tokens: bool,
        track_wallets: Vec<String>,
    ) -> Result<()> {
        info!("Starting PumpPortal WebSocket client...");
        info!("URL: {}", self.config.ws_url);

        let config = self.config.clone();
        let event_tx = self.event_tx.clone();
        let mut shutdown_rx = self.shutdown.subscribe();
        let wallets = track_wallets;

        tokio::spawn(async move {
            let mut reconnect_attempts = 0u32;

            loop {
                // Check for shutdown
                if shutdown_rx.try_recv().is_ok() {
                    info!("PumpPortal client shutting down");
                    break;
                }

                match Self::connect_and_stream(
                    &config,
                    &event_tx,
                    subscribe_new_tokens,
                    &wallets,
                )
                .await
                {
                    Ok(_) => {
                        // Clean disconnect
                        reconnect_attempts = 0;
                    }
                    Err(e) => {
                        error!("PumpPortal WebSocket error: {}", e);
                        reconnect_attempts += 1;

                        if config.max_reconnect_attempts > 0
                            && reconnect_attempts >= config.max_reconnect_attempts
                        {
                            error!(
                                "Max reconnect attempts ({}) reached",
                                config.max_reconnect_attempts
                            );
                            let _ = event_tx
                                .send(PumpPortalEvent::Error(
                                    "Max reconnect attempts reached".to_string(),
                                ))
                                .await;
                            break;
                        }
                    }
                }

                // Send disconnected event
                let _ = event_tx.send(PumpPortalEvent::Disconnected).await;

                // Wait before reconnecting
                let delay = Duration::from_millis(config.reconnect_delay_ms);
                warn!("Reconnecting in {:?}...", delay);
                sleep(delay).await;
            }
        });

        Ok(())
    }

    /// Stop the client
    pub fn stop(&self) {
        let _ = self.shutdown.send(());
    }

    /// Connect and stream events
    async fn connect_and_stream(
        config: &PumpPortalConfig,
        event_tx: &mpsc::Sender<PumpPortalEvent>,
        subscribe_new_tokens: bool,
        track_wallets: &[String],
    ) -> Result<()> {
        info!("Connecting to PumpPortal WebSocket...");

        // Parse URL
        let url = url::Url::parse(&config.ws_url)
            .map_err(|e| Error::Config(format!("Invalid WebSocket URL: {}", e)))?;

        // Connect
        let (ws_stream, _) = connect_async(url)
            .await
            .map_err(|e| Error::ShredStreamConnection(format!("WebSocket connect failed: {}", e)))?;

        info!("Connected to PumpPortal WebSocket");

        // Send connected event
        event_tx
            .send(PumpPortalEvent::Connected)
            .await
            .map_err(|e| Error::Internal(format!("Failed to send event: {}", e)))?;

        let (mut write, mut read) = ws_stream.split();

        // Subscribe to new tokens
        if subscribe_new_tokens {
            let msg = SubscriptionMessage::subscribe_new_tokens();
            let json = serde_json::to_string(&msg)
                .map_err(|e| Error::Serialization(e.to_string()))?;
            write
                .send(Message::Text(json))
                .await
                .map_err(|e| Error::ShredStreamConnection(format!("Failed to subscribe: {}", e)))?;
            info!("Subscribed to new token events");
        }

        // Subscribe to wallet trades
        if !track_wallets.is_empty() {
            let msg = SubscriptionMessage::subscribe_account_trades(track_wallets.to_vec());
            let json = serde_json::to_string(&msg)
                .map_err(|e| Error::Serialization(e.to_string()))?;
            write
                .send(Message::Text(json))
                .await
                .map_err(|e| Error::ShredStreamConnection(format!("Failed to subscribe: {}", e)))?;
            info!("Subscribed to {} wallet(s) for trade tracking", track_wallets.len());
        }

        // Set up ping interval
        let ping_interval = Duration::from_secs(config.ping_interval_secs);
        let mut ping_timer = tokio::time::interval(ping_interval);

        // Process messages
        loop {
            tokio::select! {
                // Ping to keep connection alive
                _ = ping_timer.tick() => {
                    if let Err(e) = write.send(Message::Ping(vec![])).await {
                        error!("Failed to send ping: {}", e);
                        break;
                    }
                    debug!("Sent ping");
                }

                // Receive messages
                msg = read.next() => {
                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            if let Err(e) = Self::handle_message(&text, event_tx).await {
                                warn!("Failed to handle message: {}", e);
                            }
                        }
                        Some(Ok(Message::Pong(_))) => {
                            debug!("Received pong");
                        }
                        Some(Ok(Message::Close(_))) => {
                            info!("WebSocket closed by server");
                            break;
                        }
                        Some(Err(e)) => {
                            error!("WebSocket error: {}", e);
                            break;
                        }
                        None => {
                            info!("WebSocket stream ended");
                            break;
                        }
                        _ => {}
                    }
                }
            }
        }

        Ok(())
    }

    /// Handle incoming WebSocket message
    async fn handle_message(
        text: &str,
        event_tx: &mpsc::Sender<PumpPortalEvent>,
    ) -> Result<()> {
        // Try parsing as new token event
        if let Ok(token_event) = serde_json::from_str::<NewTokenEvent>(text) {
            if token_event.tx_type == "create" {
                debug!(
                    "New token: {} ({}) - {}",
                    token_event.name, token_event.symbol, token_event.mint
                );
                event_tx
                    .send(PumpPortalEvent::NewToken(token_event))
                    .await
                    .map_err(|e| Error::Internal(e.to_string()))?;
                return Ok(());
            }
        }

        // Try parsing as trade event
        if let Ok(trade_event) = serde_json::from_str::<TradeEvent>(text) {
            debug!(
                "Trade: {} {} {} tokens for {} SOL",
                trade_event.tx_type,
                trade_event.token_amount,
                trade_event.mint,
                trade_event.sol_amount as f64 / 1e9
            );
            event_tx
                .send(PumpPortalEvent::Trade(trade_event))
                .await
                .map_err(|e| Error::Internal(e.to_string()))?;
            return Ok(());
        }

        // Unknown message format
        debug!("Unknown message: {}", &text[..text.len().min(100)]);
        Ok(())
    }
}

/// Convert NewTokenEvent to our standard TokenCreatedEvent format
impl From<NewTokenEvent> for crate::stream::decoder::TokenCreatedEvent {
    fn from(event: NewTokenEvent) -> Self {
        Self {
            signature: event.signature,
            slot: 0, // Not provided by PumpPortal
            mint: Pubkey::from_str(&event.mint).unwrap_or_default(),
            name: event.name,
            symbol: event.symbol,
            uri: event.uri,
            bonding_curve: Pubkey::from_str(&event.bonding_curve_key).unwrap_or_default(),
            associated_bonding_curve: Pubkey::default(), // Derive if needed
            creator: Pubkey::from_str(&event.trader_public_key).unwrap_or_default(),
            timestamp: chrono::Utc::now(),
        }
    }
}

/// Convert TradeEvent to our standard TokenTradeEvent format
impl From<TradeEvent> for crate::stream::decoder::TokenTradeEvent {
    fn from(event: TradeEvent) -> Self {
        Self {
            signature: event.signature,
            slot: 0,
            mint: Pubkey::from_str(&event.mint).unwrap_or_default(),
            bonding_curve: Pubkey::from_str(&event.bonding_curve_key).unwrap_or_default(),
            trader: Pubkey::from_str(&event.trader_public_key).unwrap_or_default(),
            token_amount: event.token_amount,
            sol_amount: event.sol_amount,
            is_buy: event.tx_type == "buy",
            timestamp: chrono::Utc::now(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_subscription_message_new_tokens() {
        let msg = SubscriptionMessage::subscribe_new_tokens();
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("subscribeNewToken"));
    }

    #[test]
    fn test_subscription_message_account_trades() {
        let msg = SubscriptionMessage::subscribe_account_trades(vec![
            "DYw8jCTfwHNRJhhmFcbXvVDTqWMEVFBX6ZKUmG5CNSKK".to_string(),
        ]);
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("subscribeAccountTrade"));
        assert!(json.contains("DYw8jCTfwHNRJhhmFcbXvVDTqWMEVFBX6ZKUmG5CNSKK"));
    }

    #[test]
    fn test_parse_new_token_event() {
        let json = r#"{
            "signature": "test_sig",
            "mint": "DYw8jCTfwHNRJhhmFcbXvVDTqWMEVFBX6ZKUmG5CNSKK",
            "traderPublicKey": "trader123",
            "txType": "create",
            "initialBuy": 1000000,
            "bondingCurveKey": "curve123",
            "vTokensInBondingCurve": 1000000000000,
            "vSolInBondingCurve": 30000000000,
            "marketCapSol": 30.0,
            "name": "Test Token",
            "symbol": "TEST",
            "uri": "https://example.com"
        }"#;

        let event: NewTokenEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.name, "Test Token");
        assert_eq!(event.symbol, "TEST");
        assert_eq!(event.tx_type, "create");
    }
}
