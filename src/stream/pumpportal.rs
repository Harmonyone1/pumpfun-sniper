//! PumpPortal WebSocket client for token detection
//!
//! PumpPortal provides a real-time WebSocket Data API for pump.fun.
//! Provider contract (frozen for this packet):
//!   - new token + migration: free
//!   - token/account trade streams: authenticated + metered
//!
//! WebSocket base endpoint (BASE URL only, no query/secret):
//!   wss://pumpportal.fun/api/data
//! The configured API key is appended to the connection URL internally and is
//! NEVER logged. Do NOT encode provider billing into trading math.
//! Documentation: https://pumpportal.fun/data-api/real-time

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};
use tokio::time::sleep;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

use crate::error::{Error, Result};

/// Command to send to the websocket for dynamic subscriptions.
///
/// All variants update the persistent desired-state registry and are replayed
/// across reconnects. There is intentionally no "all trades" command.
#[derive(Debug, Clone)]
pub enum SubscriptionCommand {
    /// Subscribe to trades for specific token mints.
    SubscribeTokenTrades(Vec<String>),
    /// Unsubscribe from token trades for specific mints.
    UnsubscribeTokenTrades(Vec<String>),
    /// Subscribe to trades by specific accounts (wallets).
    SubscribeAccountTrades(Vec<String>),
    /// Unsubscribe from account trades for specific wallets.
    UnsubscribeAccountTrades(Vec<String>),
}

/// PumpPortal WebSocket BASE URL (no query, no secret). Safe to log.
pub const PUMPPORTAL_WS_URL: &str = "wss://pumpportal.fun/api/data";

/// Build the authenticated connection URL from a sanitized base and an API key.
///
/// - parses the base URL;
/// - requires scheme `ws` or `wss`;
/// - rejects a base URL whose query already carries a credential (the secret
///   must come from `api_key`, never from URL text);
/// - if `api_key` is non-empty, appends exactly `api-key=<key>`.
///
/// The returned `Url` may contain the secret and must NEVER be logged.
/// All error strings are built from the SANITIZED base only (scheme + host +
/// path, query stripped) so an accidental secret in the base never leaks.
pub fn build_connection_url(base_url: &str, api_key: &str) -> Result<url::Url> {
    let mut url = url::Url::parse(base_url)
        .map_err(|_| Error::Config("Invalid PumpPortal ws_url (could not parse base)".to_string()))?;

    // Sanitized form for any error message: scheme://host/path, no query/fragment.
    let sanitized = {
        let scheme = url.scheme();
        let host = url.host_str().unwrap_or("");
        let path = url.path();
        format!("{scheme}://{host}{path}")
    };

    let scheme = url.scheme();
    if scheme != "ws" && scheme != "wss" {
        return Err(Error::Config(format!(
            "PumpPortal ws_url must use ws or wss scheme (got sanitized base {sanitized})"
        )));
    }

    // Reject any credential-bearing query already present in the base URL.
    if query_contains_credential(&url) {
        return Err(Error::Config(format!(
            "PumpPortal ws_url must not embed an api-key/credential in the query \
             (use pumpportal.api_key; sanitized base {sanitized})"
        )));
    }

    if !api_key.is_empty() {
        url.query_pairs_mut().append_pair("api-key", api_key);
    }

    Ok(url)
}

/// Return true if the URL query contains an obvious credential-bearing key.
fn query_contains_credential(url: &url::Url) -> bool {
    url.query_pairs().any(|(k, _)| {
        let k = k.to_ascii_lowercase();
        matches!(
            k.as_str(),
            "api-key" | "api_key" | "apikey" | "key" | "token" | "auth" | "access_token"
        )
    })
}

/// Subscription message shape matching PumpPortal's JSON, e.g.
/// `{"method":"subscribeTokenTrade","keys":[...]}`.
#[derive(Debug, Clone, Serialize)]
pub struct SubscriptionMessage {
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keys: Option<Vec<String>>,
}

impl SubscriptionMessage {
    /// Subscribe to new token creation events (free).
    pub fn subscribe_new_tokens() -> Self {
        Self {
            method: "subscribeNewToken".to_string(),
            keys: None,
        }
    }

    /// Unsubscribe from new tokens.
    pub fn unsubscribe_new_tokens() -> Self {
        Self {
            method: "unsubscribeNewToken".to_string(),
            keys: None,
        }
    }

    /// Subscribe to migration events (free).
    pub fn subscribe_migration() -> Self {
        Self {
            method: "subscribeMigration".to_string(),
            keys: None,
        }
    }

    /// Unsubscribe from migration events.
    pub fn unsubscribe_migration() -> Self {
        Self {
            method: "unsubscribeMigration".to_string(),
            keys: None,
        }
    }

    /// Subscribe to trades on specific token mints (authenticated + metered).
    ///
    /// Keys are required; a keyless `subscribeTokenTrade` (the old "all trades"
    /// form) is intentionally impossible.
    pub fn subscribe_token_trades(mints: Vec<String>) -> Self {
        Self {
            method: "subscribeTokenTrade".to_string(),
            keys: Some(mints),
        }
    }

    /// Unsubscribe from token trades for specific mints.
    pub fn unsubscribe_token_trades(mints: Vec<String>) -> Self {
        Self {
            method: "unsubscribeTokenTrade".to_string(),
            keys: Some(mints),
        }
    }

    /// Subscribe to trades by specific accounts (authenticated + metered).
    pub fn subscribe_account_trades(wallets: Vec<String>) -> Self {
        Self {
            method: "subscribeAccountTrade".to_string(),
            keys: Some(wallets),
        }
    }

    /// Unsubscribe from account trades for specific wallets.
    pub fn unsubscribe_account_trades(wallets: Vec<String>) -> Self {
        Self {
            method: "unsubscribeAccountTrade".to_string(),
            keys: Some(wallets),
        }
    }
}

/// Desired subscription plan supplied to `start()`.
///
/// Token/account trade keys require a configured API key; new-token/migration
/// subscriptions are free.
#[derive(Debug, Clone, Default)]
pub struct PumpPortalSubscriptionPlan {
    pub new_tokens: bool,
    pub migrations: bool,
    pub token_trades: Vec<String>,
    pub account_trades: Vec<String>,
}

impl PumpPortalSubscriptionPlan {
    /// Validate all pubkeys and deduplicate the token/account trade sets.
    ///
    /// Invalid pubkeys are an error (identity validation, INV-EVT-011). If any
    /// trade subscription is requested the API key must be non-empty.
    fn validated(&self, api_key: &str) -> Result<(Vec<String>, Vec<String>)> {
        let token_trades = validate_dedup_pubkeys(&self.token_trades)?;
        let account_trades = validate_dedup_pubkeys(&self.account_trades)?;

        if (!token_trades.is_empty() || !account_trades.is_empty()) && api_key.trim().is_empty() {
            return Err(Error::Config(
                "PumpPortal trade subscriptions require pumpportal.api_key".to_string(),
            ));
        }

        Ok((token_trades, account_trades))
    }
}

/// Validate each string parses as a Solana Pubkey, dropping duplicates while
/// preserving first-seen order. Any invalid key is an error.
fn validate_dedup_pubkeys(keys: &[String]) -> Result<Vec<String>> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for k in keys {
        Pubkey::from_str(k)
            .map_err(|_| Error::Config(format!("Invalid pubkey in subscription plan: {k}")))?;
        if seen.insert(k.clone()) {
            out.push(k.clone());
        }
    }
    Ok(out)
}

/// Persistent desired-state registry. Lives in memory across reconnects so no
/// dynamic subscription is lost when the socket drops (INV-EVT-006/007/014).
#[derive(Debug, Clone, Default)]
struct SubscriptionRegistry {
    new_tokens: bool,
    migrations: bool,
    token_trades: HashSet<String>,
    account_trades: HashSet<String>,
}

impl SubscriptionRegistry {
    fn from_plan(plan: &PumpPortalSubscriptionPlan, token: &[String], account: &[String]) -> Self {
        Self {
            new_tokens: plan.new_tokens,
            migrations: plan.migrations,
            token_trades: token.iter().cloned().collect(),
            account_trades: account.iter().cloned().collect(),
        }
    }

    /// Apply a dynamic command, returning ONLY the actual delta to send to the
    /// current connection (keys that genuinely changed registry state).
    fn apply(&mut self, cmd: &SubscriptionCommand) -> Vec<SubscriptionMessage> {
        match cmd {
            SubscriptionCommand::SubscribeTokenTrades(keys) => {
                let added: Vec<String> = keys
                    .iter()
                    .filter(|k| self.token_trades.insert((*k).clone()))
                    .cloned()
                    .collect();
                if added.is_empty() {
                    vec![]
                } else {
                    vec![SubscriptionMessage::subscribe_token_trades(added)]
                }
            }
            SubscriptionCommand::UnsubscribeTokenTrades(keys) => {
                let removed: Vec<String> = keys
                    .iter()
                    .filter(|k| self.token_trades.remove(*k))
                    .cloned()
                    .collect();
                if removed.is_empty() {
                    vec![]
                } else {
                    vec![SubscriptionMessage::unsubscribe_token_trades(removed)]
                }
            }
            SubscriptionCommand::SubscribeAccountTrades(keys) => {
                let added: Vec<String> = keys
                    .iter()
                    .filter(|k| self.account_trades.insert((*k).clone()))
                    .cloned()
                    .collect();
                if added.is_empty() {
                    vec![]
                } else {
                    vec![SubscriptionMessage::subscribe_account_trades(added)]
                }
            }
            SubscriptionCommand::UnsubscribeAccountTrades(keys) => {
                let removed: Vec<String> = keys
                    .iter()
                    .filter(|k| self.account_trades.remove(*k))
                    .cloned()
                    .collect();
                if removed.is_empty() {
                    vec![]
                } else {
                    vec![SubscriptionMessage::unsubscribe_account_trades(removed)]
                }
            }
        }
    }

    /// Build the full ordered set of subscription messages to replay on every
    /// (re)connect: new tokens, migrations, then the current token/account sets.
    fn replay_messages(&self) -> Vec<SubscriptionMessage> {
        let mut msgs = Vec::new();
        if self.new_tokens {
            msgs.push(SubscriptionMessage::subscribe_new_tokens());
        }
        if self.migrations {
            msgs.push(SubscriptionMessage::subscribe_migration());
        }
        if !self.token_trades.is_empty() {
            let mut keys: Vec<String> = self.token_trades.iter().cloned().collect();
            keys.sort();
            msgs.push(SubscriptionMessage::subscribe_token_trades(keys));
        }
        if !self.account_trades.is_empty() {
            let mut keys: Vec<String> = self.account_trades.iter().cloned().collect();
            keys.sort();
            msgs.push(SubscriptionMessage::subscribe_account_trades(keys));
        }
        msgs
    }
}

/// New token event from PumpPortal.
///
/// Numeric fields (`initial_buy`, `v_tokens_in_bonding_curve`,
/// `v_sol_in_bonding_curve`) are provider observational values and are NOT
/// canonical market reserves after MPT-001. They are `f64` to accept fractional
/// JSON and are required to be finite and non-negative before emission.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewTokenEvent {
    pub signature: String,
    pub mint: String,
    pub trader_public_key: String,
    pub tx_type: String,
    /// Provider observational initial buy amount (NOT canonical reserves).
    pub initial_buy: f64,
    pub bonding_curve_key: String,
    /// Provider observational token reserve figure (NOT canonical reserves).
    pub v_tokens_in_bonding_curve: f64,
    /// Provider observational SOL reserve figure (NOT canonical reserves).
    pub v_sol_in_bonding_curve: f64,
    pub market_cap_sol: f64,
    pub name: String,
    pub symbol: String,
    pub uri: String,
}

/// Trade event from PumpPortal.
///
/// UNIT NOTES (INV-EVT-009):
///   - `token_amount` = provider UI token amount, NOT raw token units.
///   - `sol_amount` = SOL, NOT lamports.
/// Both are required to be finite and non-negative. These values must never be
/// cast/floored into raw on-chain units.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TradeEvent {
    pub signature: String,
    pub mint: String,
    pub trader_public_key: String,
    pub tx_type: String,
    /// Provider UI token amount, NOT raw token units.
    pub token_amount: f64,
    /// SOL, NOT lamports.
    pub sol_amount: f64,
    pub bonding_curve_key: String,
    pub v_tokens_in_bonding_curve: f64,
    pub v_sol_in_bonding_curve: f64,
    pub market_cap_sol: f64,
}

/// Migration event from PumpPortal (schema is not fully documented).
///
/// Provider migration price/liquidity must NOT be treated as market truth.
#[derive(Debug, Clone)]
pub struct MigrationEvent {
    pub signature: Option<String>,
    pub mint: String,
    pub pool: Option<String>,
    pub pool_id: Option<String>,
    pub received_at: chrono::DateTime<chrono::Utc>,
}

/// Event from PumpPortal WebSocket.
#[derive(Debug, Clone)]
pub enum PumpPortalEvent {
    /// New token created.
    NewToken(NewTokenEvent),
    /// Trade occurred (buy or sell).
    Trade(TradeEvent),
    /// Token migrated to a pool.
    Migration(MigrationEvent),
    /// Connected to WebSocket AND initial subscription replay completed.
    Connected,
    /// Disconnected from WebSocket.
    Disconnected,
    /// Provider/server error (auth, balance, invalid sub, rate/ban) or stream
    /// parse error. Sanitized; never contains the API key.
    Error(String),
}

/// Configuration for PumpPortal client.
///
/// `ws_url` holds the BASE URL only; the API key is supplied via `api_key` and
/// appended internally by [`build_connection_url`].
#[derive(Debug, Clone)]
pub struct PumpPortalConfig {
    /// WebSocket BASE URL (default: wss://pumpportal.fun/api/data). No secret.
    pub ws_url: String,
    /// API key for authenticated (metered) token/account trade streams.
    pub api_key: String,
    /// Reconnect delay in milliseconds.
    pub reconnect_delay_ms: u64,
    /// Maximum reconnect attempts (0 = infinite).
    pub max_reconnect_attempts: u32,
    /// Ping interval in seconds.
    pub ping_interval_secs: u64,
}

impl Default for PumpPortalConfig {
    fn default() -> Self {
        Self {
            ws_url: PUMPPORTAL_WS_URL.to_string(),
            api_key: String::new(),
            reconnect_delay_ms: 1000,
            max_reconnect_attempts: 0, // Infinite
            ping_interval_secs: 30,
        }
    }
}

/// Sender for subscription commands.
pub type CommandSender = mpsc::Sender<SubscriptionCommand>;

/// PumpPortal WebSocket client.
pub struct PumpPortalClient {
    config: PumpPortalConfig,
    event_tx: mpsc::Sender<PumpPortalEvent>,
    shutdown: tokio::sync::broadcast::Sender<()>,
    command_tx: CommandSender,
    command_rx: Arc<Mutex<mpsc::Receiver<SubscriptionCommand>>>,
}

impl PumpPortalClient {
    /// Create a new PumpPortal client.
    pub fn new(config: PumpPortalConfig, event_tx: mpsc::Sender<PumpPortalEvent>) -> Self {
        let (shutdown, _) = tokio::sync::broadcast::channel(1);
        let (command_tx, command_rx) = mpsc::channel::<SubscriptionCommand>(100);

        Self {
            config,
            event_tx,
            shutdown,
            command_tx,
            command_rx: Arc::new(Mutex::new(command_rx)),
        }
    }

    /// Get a command sender for dynamic subscriptions.
    pub fn get_command_sender(&self) -> CommandSender {
        self.command_tx.clone()
    }

    /// Start the WebSocket connection with a desired subscription plan.
    ///
    /// One socket per `start()` runtime (INV-EVT-001). The plan is validated up
    /// front; an invalid plan yields an error and NO connection attempt. The
    /// resulting registry survives reconnects and is replayed on every connect.
    pub async fn start(&self, plan: PumpPortalSubscriptionPlan) -> Result<()> {
        // Validate + dedup BEFORE any connection attempt.
        let (token_trades, account_trades) = plan.validated(&self.config.api_key)?;

        // Never log the authenticated URL — only the sanitized base.
        info!("Starting PumpPortal WebSocket client (base: {})", self.config.ws_url);

        let config = self.config.clone();
        let event_tx = self.event_tx.clone();
        let mut shutdown_rx = self.shutdown.subscribe();
        let command_rx = self.command_rx.clone();

        // Seed the persistent registry from the initial plan. Wrapped in an Arc
        // so it lives across reconnect iterations.
        let registry = Arc::new(Mutex::new(SubscriptionRegistry::from_plan(
            &plan,
            &token_trades,
            &account_trades,
        )));

        tokio::spawn(async move {
            let mut reconnect_attempts = 0u32;

            loop {
                if shutdown_rx.try_recv().is_ok() {
                    info!("PumpPortal client shutting down");
                    break;
                }

                match Self::connect_and_stream(&config, &event_tx, &registry, &command_rx).await {
                    Ok(_) => {
                        reconnect_attempts = 0;
                    }
                    Err(e) => {
                        // `e` is built from sanitized inputs only; never the key.
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

                let _ = event_tx.send(PumpPortalEvent::Disconnected).await;

                let delay = Duration::from_millis(config.reconnect_delay_ms);
                warn!("Reconnecting in {:?}...", delay);
                sleep(delay).await;
            }
        });

        Ok(())
    }

    /// Stop the client.
    pub fn stop(&self) {
        let _ = self.shutdown.send(());
    }

    /// Connect and stream events. Establishes ONE socket, replays the current
    /// registry, and only then emits `Connected`.
    async fn connect_and_stream(
        config: &PumpPortalConfig,
        event_tx: &mpsc::Sender<PumpPortalEvent>,
        registry: &Arc<Mutex<SubscriptionRegistry>>,
        command_rx: &Arc<Mutex<mpsc::Receiver<SubscriptionCommand>>>,
    ) -> Result<()> {
        info!("Connecting to PumpPortal WebSocket (base: {})", config.ws_url);

        // Build authenticated URL internally; NEVER log it.
        let url = build_connection_url(&config.ws_url, &config.api_key)?;

        let (ws_stream, _) = connect_async(url).await.map_err(|e| {
            // tungstenite connect errors reference the base host only, not the
            // query; still, do not interpolate the Url. Report the sanitized base.
            Error::ShredStreamConnection(format!(
                "PumpPortal connect failed for base {}: {}",
                config.ws_url, e
            ))
        })?;

        info!("Connected to PumpPortal WebSocket");

        let (mut write, mut read) = ws_stream.split();

        // Replay the full current desired subscription state BEFORE emitting
        // Connected. Registry survives socket drops (INV-EVT-006/007/014).
        let replay = {
            let reg = registry.lock().await;
            reg.replay_messages()
        };
        for msg in replay {
            let json =
                serde_json::to_string(&msg).map_err(|e| Error::Serialization(e.to_string()))?;
            write.send(Message::Text(json)).await.map_err(|e| {
                Error::ShredStreamConnection(format!("Failed to send subscription: {}", e))
            })?;
            debug!("Replayed subscription: {}", msg.method);
        }

        // Only now signal readiness.
        event_tx
            .send(PumpPortalEvent::Connected)
            .await
            .map_err(|e| Error::Internal(format!("Failed to send event: {}", e)))?;

        let ping_interval = Duration::from_secs(config.ping_interval_secs);
        let mut ping_timer = tokio::time::interval(ping_interval);

        loop {
            // Drain dynamic commands: update registry, send only actual delta.
            {
                let mut cmd_rx = command_rx.lock().await;
                while let Ok(cmd) = cmd_rx.try_recv() {
                    // Validate keys before touching the registry.
                    let keys = match &cmd {
                        SubscriptionCommand::SubscribeTokenTrades(k)
                        | SubscriptionCommand::UnsubscribeTokenTrades(k)
                        | SubscriptionCommand::SubscribeAccountTrades(k)
                        | SubscriptionCommand::UnsubscribeAccountTrades(k) => k,
                    };
                    if let Err(e) = validate_dedup_pubkeys(keys) {
                        error!("Rejected subscription command: {}", e);
                        continue;
                    }

                    let deltas = {
                        let mut reg = registry.lock().await;
                        reg.apply(&cmd)
                    };
                    for msg in deltas {
                        let json = match serde_json::to_string(&msg) {
                            Ok(j) => j,
                            Err(e) => {
                                error!("Failed to serialize subscription: {}", e);
                                continue;
                            }
                        };
                        if let Err(e) = write.send(Message::Text(json)).await {
                            error!("Failed to send subscription delta: {}", e);
                        } else {
                            debug!("Sent subscription delta: {}", msg.method);
                        }
                    }
                }
            }

            tokio::select! {
                _ = ping_timer.tick() => {
                    if let Err(e) = write.send(Message::Ping(vec![])).await {
                        error!("Failed to send ping: {}", e);
                        break;
                    }
                    debug!("Sent ping");
                }

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

    /// Handle an incoming WebSocket text message.
    async fn handle_message(text: &str, event_tx: &mpsc::Sender<PumpPortalEvent>) -> Result<()> {
        debug!("Incoming message: {}", &text[..text.len().min(200)]);

        let value: serde_json::Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(_) => {
                debug!("Non-JSON message: {}", &text[..text.len().min(100)]);
                return Ok(());
            }
        };

        // 1. Provider/server error form takes precedence over event parsing.
        if let Some(reason) = detect_provider_error(&value) {
            let _ = event_tx.send(PumpPortalEvent::Error(reason)).await;
            return Ok(());
        }

        let tx_type = value.get("txType").and_then(|v| v.as_str()).unwrap_or("");

        // 2. Migration. Either an explicit migrate txType, or a txType-less
        //    message carrying a mint plus pool-ish fields.
        let is_migration = tx_type == "migrate"
            || (tx_type.is_empty()
                && value.get("mint").is_some()
                && looks_like_migration(&value));
        if is_migration {
            match parse_migration(&value) {
                Ok(ev) => {
                    let _ = event_tx.send(PumpPortalEvent::Migration(ev)).await;
                }
                Err(e) => {
                    warn!("Dropped malformed migration event: {}", e);
                    let _ = event_tx
                        .send(PumpPortalEvent::Error(format!("migration parse error: {e}")))
                        .await;
                }
            }
            return Ok(());
        }

        // 3. New token.
        if tx_type == "create" {
            match serde_json::from_str::<NewTokenEvent>(text) {
                Ok(ev) => match validate_new_token(&ev) {
                    Ok(()) => {
                        debug!("New token: {} ({}) - {}", ev.name, ev.symbol, ev.mint);
                        let _ = event_tx.send(PumpPortalEvent::NewToken(ev)).await;
                    }
                    Err(e) => {
                        warn!("Dropped malformed new-token event: {}", e);
                        let _ = event_tx
                            .send(PumpPortalEvent::Error(format!("new-token parse error: {e}")))
                            .await;
                    }
                },
                Err(e) => {
                    warn!("Failed to deserialize new-token event: {}", e);
                    let _ = event_tx
                        .send(PumpPortalEvent::Error("new-token deserialize error".to_string()))
                        .await;
                }
            }
            return Ok(());
        }

        // 4. Trade (buy/sell).
        if tx_type == "buy" || tx_type == "sell" {
            match serde_json::from_str::<TradeEvent>(text) {
                Ok(ev) => match validate_trade(&ev) {
                    Ok(()) => {
                        info!(
                            "Trade parsed: {} {} (provider UI tokens) {} for {} SOL",
                            ev.tx_type, ev.token_amount, ev.mint, ev.sol_amount
                        );
                        let _ = event_tx.send(PumpPortalEvent::Trade(ev)).await;
                    }
                    Err(e) => {
                        warn!("Dropped malformed trade event: {}", e);
                        let _ = event_tx
                            .send(PumpPortalEvent::Error(format!("trade parse error: {e}")))
                            .await;
                    }
                },
                Err(e) => {
                    warn!("Failed to deserialize trade event: {}", e);
                    let _ = event_tx
                        .send(PumpPortalEvent::Error("trade deserialize error".to_string()))
                        .await;
                }
            }
            return Ok(());
        }

        // 5. Unknown JSON stays a debug-level unknown event.
        debug!("Unknown message: {}", &text[..text.len().min(100)]);
        Ok(())
    }
}

/// Detect a provider/server error message. Returns a sanitized reason if the
/// JSON clearly represents an auth/balance/invalid-subscription/rate error.
/// Never echoes an API key (the provider error form does not carry one, and we
/// only forward the provider's own message text or a fixed label).
fn detect_provider_error(value: &serde_json::Value) -> Option<String> {
    // Common forms: {"errors":[...]}, {"error":"..."}, {"message":"..."} with
    // an error-ish body, {"status":"error",...}.
    let candidate = value
        .get("error")
        .and_then(|v| v.as_str())
        .or_else(|| value.get("message").and_then(|v| v.as_str()))
        .or_else(|| {
            value
                .get("errors")
                .and_then(|v| v.as_array())
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
        });

    if let Some(msg) = candidate {
        let lower = msg.to_ascii_lowercase();
        let looks_error = lower.contains("error")
            || lower.contains("unauthorized")
            || lower.contains("invalid")
            || lower.contains("api key")
            || lower.contains("api-key")
            || lower.contains("balance")
            || lower.contains("insufficient")
            || lower.contains("rate")
            || lower.contains("ban")
            || lower.contains("forbidden")
            || lower.contains("subscription");
        if looks_error {
            return Some(sanitize_provider_message(msg));
        }
    }

    // Explicit status field.
    if value.get("status").and_then(|v| v.as_str()) == Some("error") {
        let msg = value
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("provider error");
        return Some(sanitize_provider_message(msg));
    }

    None
}

/// Redact anything that looks like a key/token from a provider message.
fn sanitize_provider_message(msg: &str) -> String {
    let mut out = String::new();
    let lower = msg.to_ascii_lowercase();
    if let Some(idx) = lower.find("api-key=").or_else(|| lower.find("api_key=")) {
        out.push_str(&msg[..idx]);
        out.push_str("api-key=***");
    } else {
        out.push_str(msg);
    }
    out
}

/// Heuristic: a message with mint + pool-ish fields but no buy/sell txType.
fn looks_like_migration(value: &serde_json::Value) -> bool {
    value.get("pool").is_some()
        || value.get("poolId").is_some()
        || value.get("pool_id").is_some()
}

/// Parse a migration message minimally/flexibly.
fn parse_migration(value: &serde_json::Value) -> Result<MigrationEvent> {
    let mint = value
        .get("mint")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::Deserialization("migration missing mint".to_string()))?
        .to_string();
    Pubkey::from_str(&mint)
        .map_err(|_| Error::Deserialization(format!("migration invalid mint: {mint}")))?;

    let signature = value
        .get("signature")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    if let Some(sig) = &signature {
        validate_signature(sig)?;
    }

    let pool = value
        .get("pool")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let pool_id = value
        .get("poolId")
        .or_else(|| value.get("pool_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    if let Some(pid) = &pool_id {
        Pubkey::from_str(pid)
            .map_err(|_| Error::Deserialization(format!("migration invalid pool_id: {pid}")))?;
    }

    Ok(MigrationEvent {
        signature,
        mint,
        pool,
        pool_id,
        received_at: chrono::Utc::now(),
    })
}

/// Validate a NewToken event's identity and numeric fields before emission.
fn validate_new_token(ev: &NewTokenEvent) -> Result<()> {
    if ev.tx_type != "create" {
        return Err(Error::Deserialization(format!(
            "unexpected new-token txType: {}",
            ev.tx_type
        )));
    }
    validate_pubkey_field("mint", &ev.mint)?;
    validate_pubkey_field("creator", &ev.trader_public_key)?;
    validate_pubkey_field("bonding_curve", &ev.bonding_curve_key)?;
    validate_signature(&ev.signature)?;
    validate_nonneg_finite("initial_buy", ev.initial_buy)?;
    validate_nonneg_finite("v_tokens_in_bonding_curve", ev.v_tokens_in_bonding_curve)?;
    validate_nonneg_finite("v_sol_in_bonding_curve", ev.v_sol_in_bonding_curve)?;
    Ok(())
}

/// Validate a Trade event's identity and numeric fields before emission.
fn validate_trade(ev: &TradeEvent) -> Result<()> {
    if ev.tx_type != "buy" && ev.tx_type != "sell" {
        return Err(Error::Deserialization(format!(
            "unexpected trade txType: {}",
            ev.tx_type
        )));
    }
    validate_pubkey_field("mint", &ev.mint)?;
    validate_pubkey_field("trader", &ev.trader_public_key)?;
    validate_pubkey_field("bonding_curve", &ev.bonding_curve_key)?;
    validate_signature(&ev.signature)?;
    // token_amount = provider UI token amount; sol_amount = SOL. Both finite,
    // non-negative. Never cast into raw units.
    validate_nonneg_finite("token_amount", ev.token_amount)?;
    validate_nonneg_finite("sol_amount", ev.sol_amount)?;
    Ok(())
}

fn validate_pubkey_field(name: &str, value: &str) -> Result<()> {
    Pubkey::from_str(value)
        .map(|_| ())
        .map_err(|_| Error::Deserialization(format!("invalid {name} pubkey: {value}")))
}

fn validate_signature(sig: &str) -> Result<()> {
    if sig.is_empty() {
        return Err(Error::Deserialization("empty signature".to_string()));
    }
    Signature::from_str(sig)
        .map(|_| ())
        .map_err(|_| Error::Deserialization(format!("invalid signature: {sig}")))
}

fn validate_nonneg_finite(name: &str, v: f64) -> Result<()> {
    if !v.is_finite() {
        return Err(Error::Deserialization(format!("{name} is not finite")));
    }
    if v < 0.0 {
        return Err(Error::Deserialization(format!("{name} is negative")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_PK_1: &str = "So11111111111111111111111111111111111111112";
    const VALID_PK_2: &str = "DYw8jCTfwHNRJhhmFcbXvVDTqWMEVFBX6ZKUmG5CNSKK";
    const VALID_PK_3: &str = "9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM";
    const SAMPLE_KEY: &str = "super-secret-sample-key-123";

    // Deterministic valid 64-byte Solana signature for parse tests.
    fn valid_sig() -> String {
        Signature::from([7u8; 64]).to_string()
    }

    #[test]
    fn test_connection_url_adds_api_key() {
        let url = build_connection_url(PUMPPORTAL_WS_URL, SAMPLE_KEY).unwrap();
        let q = url.query().unwrap();
        assert!(q.contains("api-key="));
        assert!(url.as_str().contains(SAMPLE_KEY));
    }

    #[test]
    fn test_connection_url_without_key_has_no_secret_query() {
        let url = build_connection_url(PUMPPORTAL_WS_URL, "").unwrap();
        assert!(url.query().is_none());
        assert!(!url.as_str().contains("api-key"));
    }

    #[test]
    fn test_ws_url_with_embedded_api_key_rejected() {
        let base = format!("{PUMPPORTAL_WS_URL}?api-key=embedded");
        let err = build_connection_url(&base, SAMPLE_KEY).unwrap_err();
        // Error must not echo the configured sample key.
        assert!(!format!("{err}").contains(SAMPLE_KEY));
        // Rejected for embedded credential.
        assert!(format!("{err}").to_lowercase().contains("api-key"));
    }

    #[test]
    fn test_error_does_not_echo_api_key() {
        // A bad scheme error is built from sanitized base only.
        let err = build_connection_url("http://pumpportal.fun/api/data", SAMPLE_KEY).unwrap_err();
        assert!(!format!("{err}").contains(SAMPLE_KEY));
    }

    #[test]
    fn test_trade_plan_without_api_key_rejected() {
        let plan = PumpPortalSubscriptionPlan {
            new_tokens: true,
            migrations: false,
            token_trades: vec![VALID_PK_1.to_string()],
            account_trades: vec![],
        };
        assert!(plan.validated("").is_err());
    }

    #[test]
    fn test_free_only_plan_without_api_key_allowed() {
        let plan = PumpPortalSubscriptionPlan {
            new_tokens: true,
            migrations: true,
            token_trades: vec![],
            account_trades: vec![],
        };
        assert!(plan.validated("").is_ok());
    }

    #[test]
    fn test_subscribe_token_trade_requires_keys() {
        // The builder always emits keys; there is no keyless constructor.
        let msg = SubscriptionMessage::subscribe_token_trades(vec![VALID_PK_1.to_string()]);
        assert!(msg.keys.is_some());
        assert_eq!(msg.keys.as_ref().unwrap().len(), 1);
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("subscribeTokenTrade"));
        assert!(json.contains("keys"));
    }

    #[test]
    fn test_subscription_plan_deduplicates_keys() {
        let plan = PumpPortalSubscriptionPlan {
            new_tokens: false,
            migrations: false,
            token_trades: vec![
                VALID_PK_1.to_string(),
                VALID_PK_1.to_string(),
                VALID_PK_2.to_string(),
            ],
            account_trades: vec![],
        };
        let (tokens, _) = plan.validated("k").unwrap();
        assert_eq!(tokens.len(), 2);
    }

    #[test]
    fn test_dynamic_token_subscription_survives_reconnect_registry() {
        let plan = PumpPortalSubscriptionPlan {
            new_tokens: true,
            migrations: false,
            token_trades: vec![VALID_PK_1.to_string()],
            account_trades: vec![],
        };
        let mut reg = SubscriptionRegistry::from_plan(
            &plan,
            &[VALID_PK_1.to_string()],
            &[],
        );
        // Dynamically add a second token.
        reg.apply(&SubscriptionCommand::SubscribeTokenTrades(vec![
            VALID_PK_2.to_string(),
        ]));
        // Replay (as would happen on reconnect) must include both.
        let msgs = reg.replay_messages();
        let token_msg = msgs
            .iter()
            .find(|m| m.method == "subscribeTokenTrade")
            .unwrap();
        let keys = token_msg.keys.as_ref().unwrap();
        assert!(keys.contains(&VALID_PK_1.to_string()));
        assert!(keys.contains(&VALID_PK_2.to_string()));
    }

    #[test]
    fn test_unsubscribe_persists_across_reconnect_registry() {
        let plan = PumpPortalSubscriptionPlan {
            new_tokens: false,
            migrations: false,
            token_trades: vec![VALID_PK_1.to_string(), VALID_PK_2.to_string()],
            account_trades: vec![],
        };
        let mut reg = SubscriptionRegistry::from_plan(
            &plan,
            &[VALID_PK_1.to_string(), VALID_PK_2.to_string()],
            &[],
        );
        reg.apply(&SubscriptionCommand::UnsubscribeTokenTrades(vec![
            VALID_PK_1.to_string(),
        ]));
        let msgs = reg.replay_messages();
        let token_msg = msgs
            .iter()
            .find(|m| m.method == "subscribeTokenTrade")
            .unwrap();
        let keys = token_msg.keys.as_ref().unwrap();
        assert!(!keys.contains(&VALID_PK_1.to_string()));
        assert!(keys.contains(&VALID_PK_2.to_string()));
    }

    #[test]
    fn test_migration_subscription_message() {
        let msg = SubscriptionMessage::subscribe_migration();
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("subscribeMigration"));
        assert!(!json.contains("keys"));
    }

    #[test]
    fn test_parse_migration_minimal() {
        let value: serde_json::Value = serde_json::json!({
            "txType": "migrate",
            "mint": VALID_PK_2,
            "pool": "raydium",
        });
        let ev = parse_migration(&value).unwrap();
        assert_eq!(ev.mint, VALID_PK_2);
        assert_eq!(ev.pool.as_deref(), Some("raydium"));
        assert!(ev.pool_id.is_none());
    }

    #[test]
    fn test_invalid_mint_event_rejected() {
        let ev = TradeEvent {
            signature: valid_sig(),
            mint: "not_a_pubkey".to_string(),
            trader_public_key: VALID_PK_2.to_string(),
            tx_type: "buy".to_string(),
            token_amount: 1.0,
            sol_amount: 1.0,
            bonding_curve_key: VALID_PK_3.to_string(),
            v_tokens_in_bonding_curve: 1.0,
            v_sol_in_bonding_curve: 1.0,
            market_cap_sol: 1.0,
        };
        assert!(validate_trade(&ev).is_err());
    }

    #[test]
    fn test_trade_nonfinite_amount_rejected() {
        let ev = TradeEvent {
            signature: valid_sig(),
            mint: VALID_PK_1.to_string(),
            trader_public_key: VALID_PK_2.to_string(),
            tx_type: "sell".to_string(),
            token_amount: f64::NAN,
            sol_amount: 1.0,
            bonding_curve_key: VALID_PK_3.to_string(),
            v_tokens_in_bonding_curve: 1.0,
            v_sol_in_bonding_curve: 1.0,
            market_cap_sol: 1.0,
        };
        assert!(validate_trade(&ev).is_err());
    }

    #[test]
    fn test_new_token_fractional_provider_numbers_parse() {
        let sig = valid_sig();
        let json = format!(
            r#"{{
                "signature": "{sig}",
                "mint": "{VALID_PK_2}",
                "traderPublicKey": "{VALID_PK_1}",
                "txType": "create",
                "initialBuy": 1000000.5,
                "bondingCurveKey": "{VALID_PK_3}",
                "vTokensInBondingCurve": 1000000000000.25,
                "vSolInBondingCurve": 30.5,
                "marketCapSol": 30.0,
                "name": "Test Token",
                "symbol": "TEST",
                "uri": "https://example.com"
            }}"#
        );
        let ev: NewTokenEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(ev.initial_buy, 1000000.5);
        assert_eq!(ev.v_sol_in_bonding_curve, 30.5);
        assert!(validate_new_token(&ev).is_ok());
    }

    #[test]
    fn test_no_all_trades_subscription_message() {
        // There is no "all trades" constructor. Every token-trade subscription
        // carries explicit non-empty keys.
        let msg = SubscriptionMessage::subscribe_token_trades(vec![VALID_PK_1.to_string()]);
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("keys"));
        // A hypothetical empty-key message would be a misuse; assert we never
        // build one via the registry replay when the token set is empty.
        let reg = SubscriptionRegistry::default();
        let msgs = reg.replay_messages();
        assert!(!msgs.iter().any(|m| m.method == "subscribeTokenTrade"));
    }
}
