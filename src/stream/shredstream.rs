//! Jito ShredStream client for low-latency transaction detection
//!
//! ShredStream provides the fastest possible access to new transactions
//! by streaming shreds directly from validators before they're assembled
//! into blocks.

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

use crate::config::ShredStreamConfig;
use crate::error::{Error, Result};
use crate::pump::program::PUMP_PROGRAM_ID;

/// Event from ShredStream
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// New transaction detected
    Transaction(TransactionEvent),
    /// Connection status changed
    Connected,
    /// Disconnected (will attempt reconnect)
    Disconnected,
    /// Error occurred
    Error(String),
}

/// Transaction event from stream
#[derive(Debug, Clone)]
pub struct TransactionEvent {
    /// Transaction signature
    pub signature: String,
    /// Slot number
    pub slot: u64,
    /// Raw transaction data
    pub data: Vec<u8>,
    /// Account keys involved
    pub accounts: Vec<String>,
    /// Whether this involves pump.fun program
    pub is_pump_fun: bool,
    /// Timestamp when received
    pub received_at: chrono::DateTime<chrono::Utc>,
}

/// ShredStream client with automatic reconnection
pub struct ShredStreamClient {
    config: ShredStreamConfig,
    event_tx: mpsc::Sender<StreamEvent>,
    shutdown: tokio::sync::broadcast::Sender<()>,
}

impl ShredStreamClient {
    /// Create a new ShredStream client
    pub fn new(
        config: ShredStreamConfig,
        event_tx: mpsc::Sender<StreamEvent>,
    ) -> Self {
        let (shutdown, _) = tokio::sync::broadcast::channel(1);

        Self {
            config,
            event_tx,
            shutdown,
        }
    }

    /// Start the ShredStream connection
    /// This spawns a background task that handles connection and reconnection
    pub async fn start(&self) -> Result<()> {
        info!("Starting ShredStream client...");
        info!("gRPC URL: {}", self.config.grpc_url);

        let config = self.config.clone();
        let event_tx = self.event_tx.clone();
        let mut shutdown_rx = self.shutdown.subscribe();

        tokio::spawn(async move {
            let mut reconnect_attempts = 0;

            loop {
                // Check for shutdown
                if shutdown_rx.try_recv().is_ok() {
                    info!("ShredStream client shutting down");
                    break;
                }

                match Self::connect_and_stream(&config, &event_tx).await {
                    Ok(_) => {
                        // Normal disconnection
                        reconnect_attempts = 0;
                    }
                    Err(e) => {
                        error!("ShredStream error: {}", e);
                        reconnect_attempts += 1;

                        if reconnect_attempts >= config.max_reconnect_attempts {
                            error!(
                                "Max reconnect attempts ({}) reached, giving up",
                                config.max_reconnect_attempts
                            );
                            let _ = event_tx
                                .send(StreamEvent::Error(
                                    "Max reconnect attempts reached".to_string(),
                                ))
                                .await;
                            break;
                        }
                    }
                }

                // Send disconnected event
                let _ = event_tx.send(StreamEvent::Disconnected).await;

                // Wait before reconnecting
                let delay = Duration::from_millis(
                    config.reconnect_delay_ms * reconnect_attempts as u64,
                );
                warn!(
                    "Reconnecting in {:?} (attempt {}/{})",
                    delay, reconnect_attempts, config.max_reconnect_attempts
                );
                sleep(delay).await;
            }
        });

        Ok(())
    }

    /// Stop the ShredStream client
    pub fn stop(&self) {
        let _ = self.shutdown.send(());
    }

    /// Connect to ShredStream and process events
    async fn connect_and_stream(
        config: &ShredStreamConfig,
        event_tx: &mpsc::Sender<StreamEvent>,
    ) -> Result<()> {
        info!("Connecting to ShredStream at {}", config.grpc_url);

        // TODO: Implement actual gRPC connection using solana-stream-sdk
        // For now, this is a placeholder that simulates the connection
        //
        // Real implementation would:
        // 1. Create ShredstreamClient from solana-stream-sdk
        // 2. Subscribe to entries with pump.fun program filter
        // 3. Process incoming transactions

        // Example with solana-stream-sdk (pseudo-code):
        // ```
        // use solana_stream_sdk::ShredstreamClient;
        //
        // let client = ShredstreamClient::connect(&config.grpc_url).await?;
        // let request = ShredstreamClient::create_entries_request_for_account(
        //     PUMP_PROGRAM_ID.to_string()
        // );
        // let mut stream = client.subscribe_entries(request).await?;
        //
        // while let Some(entry) = stream.next().await {
        //     // Process entry, extract transactions
        //     // Filter for pump.fun program
        //     // Send to event channel
        // }
        // ```

        // Send connected event
        event_tx.send(StreamEvent::Connected).await.map_err(|e| {
            Error::ShredStreamConnection(format!("Failed to send connected event: {}", e))
        })?;

        info!("Connected to ShredStream");

        // Placeholder: simulate receiving events
        // In real implementation, this would be the gRPC stream loop
        loop {
            // Check if channel is closed
            if event_tx.is_closed() {
                break;
            }

            // Sleep to simulate waiting for events
            sleep(Duration::from_secs(60)).await;
        }

        Ok(())
    }

    /// Check if a transaction involves pump.fun program
    pub fn is_pump_fun_transaction(accounts: &[String]) -> bool {
        let pump_program = PUMP_PROGRAM_ID.to_string();
        accounts.iter().any(|a| a == &pump_program)
    }
}

/// Builder for ShredStreamClient
pub struct ShredStreamClientBuilder {
    config: Option<ShredStreamConfig>,
    channel_capacity: usize,
}

impl ShredStreamClientBuilder {
    pub fn new() -> Self {
        Self {
            config: None,
            channel_capacity: 10000,
        }
    }

    pub fn config(mut self, config: ShredStreamConfig) -> Self {
        self.config = Some(config);
        self
    }

    pub fn channel_capacity(mut self, capacity: usize) -> Self {
        self.channel_capacity = capacity;
        self
    }

    pub fn build(self) -> Result<(ShredStreamClient, mpsc::Receiver<StreamEvent>)> {
        let config = self
            .config
            .ok_or_else(|| Error::Config("ShredStream config required".to_string()))?;

        let (tx, rx) = mpsc::channel(self.channel_capacity);
        let client = ShredStreamClient::new(config, tx);

        Ok((client, rx))
    }
}

impl Default for ShredStreamClientBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_pump_fun_transaction() {
        let accounts = vec![
            "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".to_string(),
            "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P".to_string(), // Pump.fun
        ];
        assert!(ShredStreamClient::is_pump_fun_transaction(&accounts));

        let accounts_no_pump = vec![
            "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".to_string(),
        ];
        assert!(!ShredStreamClient::is_pump_fun_transaction(&accounts_no_pump));
    }
}
