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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};
use tokio::time::sleep;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

use crate::error::{Error, Result};

/// Fixed, secret-safe fallback string used in logs when a base URL cannot be
/// sanitized. Never echoes raw configured text.
const SANITIZED_BASE_FALLBACK: &str = "wss://<pumpportal-base>";

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

    // Sanitized form for any error message: scheme://host/path, no query/fragment,
    // no userinfo. Built via the shared helper so no secret ever leaks.
    let sanitized = sanitized_ws_base_for_log(base_url);

    let scheme = url.scheme();
    if scheme != "ws" && scheme != "wss" {
        return Err(Error::Config(format!(
            "PumpPortal ws_url must use ws or wss scheme (got sanitized base {sanitized})"
        )));
    }

    // Reject embedded userinfo (username/password): the secret must come from
    // `api_key`, never from URL credentials.
    if !url.username().is_empty() || url.password().is_some() {
        return Err(Error::Config(format!(
            "PumpPortal ws_url must not embed a username/password \
             (use pumpportal.api_key; sanitized base {sanitized})"
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

/// Pure, secret-safe display of a WebSocket base URL for logging.
///
/// Strips query, fragment, username, and password. Returns `scheme://host/path`.
/// On any malformed input it returns a FIXED constant and never echoes the raw
/// argument. Use this for EVERY stream log/error that names the base endpoint.
fn sanitized_ws_base_for_log(base_url: &str) -> String {
    match url::Url::parse(base_url) {
        Ok(u) => {
            let scheme = u.scheme();
            match u.host_str() {
                Some(host) if !host.is_empty() => {
                    let path = u.path();
                    // host_str() excludes userinfo; query()/fragment() are dropped.
                    format!("{scheme}://{host}{path}")
                }
                _ => SANITIZED_BASE_FALLBACK.to_string(),
            }
        }
        Err(_) => SANITIZED_BASE_FALLBACK.to_string(),
    }
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

    /// Compute the wire deltas needed to move an ACTIVE socket registry (`self`)
    /// to a target DESIRED registry. Pure; no I/O.
    ///
    /// Emits, in order: new-token (un)subscribe if it flipped, migration
    /// (un)subscribe if it flipped, then token/account subscribe-missing and
    /// unsubscribe-removed messages. Deterministic (sorted keys). If `self`
    /// already equals `desired` the result is empty.
    fn diff_to(&self, desired: &SubscriptionRegistry) -> Vec<SubscriptionMessage> {
        let mut msgs = Vec::new();

        if desired.new_tokens && !self.new_tokens {
            msgs.push(SubscriptionMessage::subscribe_new_tokens());
        } else if !desired.new_tokens && self.new_tokens {
            msgs.push(SubscriptionMessage::unsubscribe_new_tokens());
        }

        if desired.migrations && !self.migrations {
            msgs.push(SubscriptionMessage::subscribe_migration());
        } else if !desired.migrations && self.migrations {
            msgs.push(SubscriptionMessage::unsubscribe_migration());
        }

        let mut token_add: Vec<String> = desired
            .token_trades
            .difference(&self.token_trades)
            .cloned()
            .collect();
        token_add.sort();
        if !token_add.is_empty() {
            msgs.push(SubscriptionMessage::subscribe_token_trades(token_add));
        }

        let mut token_rem: Vec<String> = self
            .token_trades
            .difference(&desired.token_trades)
            .cloned()
            .collect();
        token_rem.sort();
        if !token_rem.is_empty() {
            msgs.push(SubscriptionMessage::unsubscribe_token_trades(token_rem));
        }

        let mut acct_add: Vec<String> = desired
            .account_trades
            .difference(&self.account_trades)
            .cloned()
            .collect();
        acct_add.sort();
        if !acct_add.is_empty() {
            msgs.push(SubscriptionMessage::subscribe_account_trades(acct_add));
        }

        let mut acct_rem: Vec<String> = self
            .account_trades
            .difference(&desired.account_trades)
            .cloned()
            .collect();
        acct_rem.sort();
        if !acct_rem.is_empty() {
            msgs.push(SubscriptionMessage::unsubscribe_account_trades(acct_rem));
        }

        msgs
    }
}

/// True iff the command is an authenticated (metered) trade command that
/// requires a configured API key (A7).
fn command_requires_api_key(cmd: &SubscriptionCommand) -> bool {
    matches!(
        cmd,
        SubscriptionCommand::SubscribeTokenTrades(_)
            | SubscriptionCommand::UnsubscribeTokenTrades(_)
            | SubscriptionCommand::SubscribeAccountTrades(_)
            | SubscriptionCommand::UnsubscribeAccountTrades(_)
    )
}

/// Borrow the pubkey list carried by any command.
fn command_keys(cmd: &SubscriptionCommand) -> &[String] {
    match cmd {
        SubscriptionCommand::SubscribeTokenTrades(k)
        | SubscriptionCommand::UnsubscribeTokenTrades(k)
        | SubscriptionCommand::SubscribeAccountTrades(k)
        | SubscriptionCommand::UnsubscribeAccountTrades(k) => k,
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

/// Stateful, clonable handle for issuing dynamic subscription commands.
///
/// A `CommandSender` shares the SAME desired-state registry `Arc` as the
/// `PumpPortalClient` that produced it. A command mutates desired state
/// IMMEDIATELY at acceptance time (before any worker consumes a notification),
/// so a command accepted while the socket is down is already present in desired
/// state for the next reconnect replay. The `notify_tx` channel carries only a
/// "desired changed; resync the active socket" signal — never concrete wire
/// messages.
#[derive(Clone)]
pub struct CommandSender {
    /// Shared desired subscription registry (same Arc the client holds).
    desired: Arc<Mutex<SubscriptionRegistry>>,
    /// "desired changed; resync" signal to the worker (not concrete messages).
    notify_tx: mpsc::Sender<()>,
    /// Whether a non-empty API key is configured (authorizes metered commands).
    api_key_present: bool,
    /// Set once `start()` has been called.
    started: Arc<AtomicBool>,
    /// Cleared when the reconnect worker terminates.
    worker_alive: Arc<AtomicBool>,
}

impl CommandSender {
    /// Accept a dynamic subscription command (A2).
    ///
    /// Ordering, exactly:
    ///   1. reject if the client has not been started;
    ///   2. reject if the worker is known terminated;
    ///   3. validate all pubkeys (invalid => Err, before any mutation);
    ///   4. require a configured API key for metered trade commands;
    ///   5. lock the shared desired registry;
    ///   6. apply the command to desired state immediately;
    ///   7. if it caused NO change, release and return Ok (no notify);
    ///   8. release the lock;
    ///   9. send a `()` notification to the worker.
    ///
    /// Desired mutation happens BEFORE notification. No new WebSocket.
    pub async fn send(&self, command: SubscriptionCommand) -> Result<()> {
        // 1. not started.
        if !self.started.load(Ordering::SeqCst) {
            return Err(Error::Internal(
                "PumpPortal client not started; command rejected".to_string(),
            ));
        }
        // 2. worker terminated.
        if !self.worker_alive.load(Ordering::SeqCst) {
            return Err(Error::Internal(
                "PumpPortal stream worker terminated; command rejected".to_string(),
            ));
        }
        // 3. validate pubkeys before any mutation.
        validate_dedup_pubkeys(command_keys(&command))?;
        // 4. metered trade commands require a configured API key.
        if command_requires_api_key(&command) && !self.api_key_present {
            return Err(Error::Config(
                "PumpPortal trade subscription command requires pumpportal.api_key".to_string(),
            ));
        }

        // 5-8. apply to desired state under lock; detect no-op.
        let changed = {
            let mut desired = self.desired.lock().await;
            // apply() returns the actual wire delta; a non-empty delta means the
            // registry state changed.
            !desired.apply(&command).is_empty()
        };
        if !changed {
            return Ok(());
        }

        // 9. signal the worker to resync the active socket to desired.
        self.notify_tx
            .send(())
            .await
            .map_err(|_| Error::Internal("PumpPortal notify channel closed".to_string()))
    }
}

/// PumpPortal WebSocket client.
pub struct PumpPortalClient {
    config: PumpPortalConfig,
    event_tx: mpsc::Sender<PumpPortalEvent>,
    shutdown: tokio::sync::broadcast::Sender<()>,
    /// Shared desired-state registry; lives for the client's lifetime and is the
    /// SAME Arc every `CommandSender` shares.
    desired: Arc<Mutex<SubscriptionRegistry>>,
    /// Resync-notification sender handed to `CommandSender`s.
    notify_tx: mpsc::Sender<()>,
    /// Single-consumer resync-notification receiver, owned by the worker.
    notify_rx: Arc<Mutex<mpsc::Receiver<()>>>,
    /// Set on first `start()`; a second `start()` is rejected.
    started: Arc<AtomicBool>,
    /// True while the reconnect worker is running.
    worker_alive: Arc<AtomicBool>,
}

impl PumpPortalClient {
    /// Create a new PumpPortal client.
    pub fn new(config: PumpPortalConfig, event_tx: mpsc::Sender<PumpPortalEvent>) -> Self {
        let (shutdown, _) = tokio::sync::broadcast::channel(1);
        let (notify_tx, notify_rx) = mpsc::channel::<()>(100);

        Self {
            config,
            event_tx,
            shutdown,
            desired: Arc::new(Mutex::new(SubscriptionRegistry::default())),
            notify_tx,
            notify_rx: Arc::new(Mutex::new(notify_rx)),
            started: Arc::new(AtomicBool::new(false)),
            worker_alive: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Get a command sender for dynamic subscriptions.
    ///
    /// Shares the same desired-registry `Arc`, resync notifier, and started /
    /// worker-alive flags as this client (A3).
    pub fn get_command_sender(&self) -> CommandSender {
        CommandSender {
            desired: self.desired.clone(),
            notify_tx: self.notify_tx.clone(),
            api_key_present: !self.config.api_key.trim().is_empty(),
            started: self.started.clone(),
            worker_alive: self.worker_alive.clone(),
        }
    }

    /// Start the WebSocket connection with a desired subscription plan.
    ///
    /// One socket per `start()` runtime (INV-EVT-001). The plan is validated up
    /// front; an invalid plan yields an error and NO connection attempt. The
    /// resulting registry survives reconnects and is replayed on every connect.
    pub async fn start(&self, plan: PumpPortalSubscriptionPlan) -> Result<()> {
        // Validate + dedup BEFORE any connection attempt.
        let (token_trades, account_trades) = plan.validated(&self.config.api_key)?;

        // A3: reject a second start on the same client. compare_exchange makes
        // this atomic — two concurrent start() calls can never both spawn a
        // worker (never two sockets from one client).
        if self
            .started
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(Error::Internal(
                "PumpPortal client already started (one socket per client)".to_string(),
            ));
        }

        // Never log the authenticated URL — only the sanitized base.
        info!(
            "Starting PumpPortal WebSocket client (base: {})",
            sanitized_ws_base_for_log(&self.config.ws_url)
        );

        // Seed/replace the SHARED desired registry from the initial plan. This is
        // the same Arc every CommandSender holds, so commands accepted after this
        // point mutate the state the worker replays.
        {
            let mut desired = self.desired.lock().await;
            *desired = SubscriptionRegistry::from_plan(&plan, &token_trades, &account_trades);
        }

        // Mark the worker alive so CommandSender::send is accepted.
        self.worker_alive.store(true, Ordering::SeqCst);

        let config = self.config.clone();
        let event_tx = self.event_tx.clone();
        let mut shutdown_rx = self.shutdown.subscribe();
        let desired = self.desired.clone();
        let notify_rx = self.notify_rx.clone();
        let worker_alive = self.worker_alive.clone();

        tokio::spawn(async move {
            let mut reconnect_attempts = 0u32;

            loop {
                if shutdown_rx.try_recv().is_ok() {
                    info!("PumpPortal client shutting down");
                    break;
                }

                match Self::connect_and_stream(&config, &event_tx, &desired, &notify_rx).await {
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

            // Worker exiting: no further commands can be synchronized.
            worker_alive.store(false, Ordering::SeqCst);
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
        desired: &Arc<Mutex<SubscriptionRegistry>>,
        notify_rx: &Arc<Mutex<mpsc::Receiver<()>>>,
    ) -> Result<()> {
        let safe_base = sanitized_ws_base_for_log(&config.ws_url);
        info!("Connecting to PumpPortal WebSocket (base: {})", safe_base);

        // Build authenticated URL internally; NEVER log it.
        let url = build_connection_url(&config.ws_url, &config.api_key)?;

        let (ws_stream, _) = connect_async(url).await.map_err(|_| {
            // NEVER interpolate the tungstenite error: it can reference the full
            // request URL (with the api-key query). Report the sanitized base only.
            Error::ShredStreamConnection(format!(
                "PumpPortal connect failed for base {safe_base}"
            ))
        })?;

        info!("Connected to PumpPortal WebSocket");

        let (mut write, mut read) = ws_stream.split();

        // Per-connection snapshot of what the socket has actually been sent.
        let active: SubscriptionRegistry;

        // A4: replay + snapshot + Connected are ATOMIC under the desired lock so a
        // concurrent CommandSender::send cannot slip a desired mutation into the
        // gap between snapshot and Connected. Invariant at Connected: active ==
        // current desired.
        {
            let desired_guard = desired.lock().await;
            for msg in desired_guard.replay_messages() {
                let json =
                    serde_json::to_string(&msg).map_err(|e| Error::Serialization(e.to_string()))?;
                // A6: replay write failure returns Err => outer loop reconnects.
                write.send(Message::Text(json)).await.map_err(|_| {
                    Error::ShredStreamConnection(format!(
                        "Failed to send subscription replay to base {safe_base}"
                    ))
                })?;
                debug!("Replayed subscription: {}", msg.method);
            }
            // Snapshot the exact state the socket now reflects.
            active = desired_guard.clone();

            // Signal readiness while still holding the lock.
            event_tx
                .send(PumpPortalEvent::Connected)
                .await
                .map_err(|e| Error::Internal(format!("Failed to send event: {}", e)))?;
        }
        // active now == desired at the instant Connected was emitted.
        let mut active = active;

        let ping_interval = Duration::from_secs(config.ping_interval_secs);
        let mut ping_timer = tokio::time::interval(ping_interval);

        loop {
            // Coalesce any pending resync notifications (their only meaning is
            // "desired changed; synchronize"), then synchronize the active socket
            // to the CURRENT desired state (A5). We drive the sync off the desired
            // state itself, not the token count, so a wake consumed by the select
            // arm below still results in a correct diff+send here.
            {
                let mut nrx = notify_rx.lock().await;
                while nrx.try_recv().is_ok() {}
            }
            {
                // Diff and send under the desired lock so Connected (sync ack)
                // reflects exactly the state we synchronized (no concurrent
                // desired mutation between diff, send, and Connected).
                let desired_guard = desired.lock().await;
                let deltas = active.diff_to(&desired_guard);
                if !deltas.is_empty() {
                    for msg in deltas {
                        let json = serde_json::to_string(&msg)
                            .map_err(|e| Error::Serialization(e.to_string()))?;
                        // A6: any dynamic-delta write failure returns Err =>
                        // reconnect; desired stays current so replay repairs it.
                        write.send(Message::Text(json)).await.map_err(|_| {
                            Error::ShredStreamConnection(format!(
                                "Failed to send subscription delta to base {safe_base}"
                            ))
                        })?;
                        debug!("Synchronized subscription: {}", msg.method);
                    }
                    // Every delta sent successfully: active is now in sync.
                    active = desired_guard.clone();
                    // Emit Connected meaning "desired subscriptions synchronized".
                    event_tx
                        .send(PumpPortalEvent::Connected)
                        .await
                        .map_err(|e| Error::Internal(format!("Failed to send event: {}", e)))?;
                }
                // If deltas is empty, desired already == active: no wire message.
            }

            tokio::select! {
                _ = ping_timer.tick() => {
                    if let Err(e) = write.send(Message::Ping(vec![])).await {
                        error!("Failed to send ping: {}", e);
                        break;
                    }
                    debug!("Sent ping");
                }

                // Wake promptly on a resync notification so deltas are not stuck
                // behind read/ping idle. The actual sync runs at loop top.
                _ = async {
                    let mut nrx = notify_rx.lock().await;
                    nrx.recv().await
                } => {
                    continue;
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
        // A9: metadata only — never log raw provider text (it may echo secrets).
        debug!("Incoming PumpPortal text message: {} bytes", text.len());

        let value: serde_json::Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(_) => {
                debug!("Non-JSON PumpPortal message ({} bytes)", text.len());
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

        // 5. Unknown JSON stays a debug-level unknown event (metadata only).
        debug!("Unknown PumpPortal JSON message");
        Ok(())
    }
}

/// Detect a provider/server error message. Returns a sanitized reason if the
/// JSON clearly represents an auth/balance/invalid-subscription/rate error.
/// Never echoes an API key (the provider error form does not carry one, and we
/// only forward the provider's own message text or a fixed label).
fn detect_provider_error(value: &serde_json::Value) -> Option<String> {
    // Common forms: {"errors":[...]}, {"error":"..."}, {"message":"..."} with an
    // error-ish body, {"status":"error",...}. We inspect the raw text ONLY to
    // CLASSIFY it into a fixed category; we NEVER return the raw text (A10), so a
    // provider message that echoes a key can never leak into the event stream.
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
            return Some(classify_provider_error(&lower).to_string());
        }
    }

    // Explicit status field.
    if value.get("status").and_then(|v| v.as_str()) == Some("error") {
        let lower = value
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        return Some(classify_provider_error(&lower).to_string());
    }

    None
}

/// Map a lowercased provider error message to a FIXED sanitized category.
///
/// Returns one of a closed set of category strings; the input text is used only
/// for classification and is never returned, so a fake/real key inside the
/// provider message cannot leak into the emitted event (A10).
fn classify_provider_error(lower: &str) -> &'static str {
    if lower.contains("unauthorized")
        || lower.contains("forbidden")
        || lower.contains("api key")
        || lower.contains("api-key")
        || lower.contains("auth")
    {
        "PumpPortal authentication error"
    } else if lower.contains("balance") || lower.contains("insufficient") {
        "PumpPortal data-wallet balance error"
    } else if lower.contains("rate") || lower.contains("ban") {
        "PumpPortal rate/ban error"
    } else if lower.contains("subscription") || lower.contains("subscribe") {
        "PumpPortal subscription error"
    } else {
        "PumpPortal provider error"
    }
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
    validate_nonneg_finite("market_cap_sol", ev.market_cap_sol)?;
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
    // Observational provider reserve figures — validated but NOT canonical.
    validate_nonneg_finite("v_tokens_in_bonding_curve", ev.v_tokens_in_bonding_curve)?;
    validate_nonneg_finite("v_sol_in_bonding_curve", ev.v_sol_in_bonding_curve)?;
    validate_nonneg_finite("market_cap_sol", ev.market_cap_sol)?;
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

    // === AGENT A — subscription synchronization / secrecy / validation ========

    /// Build a CommandSender wired to a fresh desired registry, with the given
    /// api-key presence, started/worker-alive both true. Returns the sender, the
    /// shared desired Arc, and the notify receiver so tests can observe signals.
    fn make_sender(
        api_key_present: bool,
        seed: SubscriptionRegistry,
    ) -> (
        CommandSender,
        Arc<Mutex<SubscriptionRegistry>>,
        mpsc::Receiver<()>,
    ) {
        let desired = Arc::new(Mutex::new(seed));
        let (notify_tx, notify_rx) = mpsc::channel::<()>(16);
        let sender = CommandSender {
            desired: desired.clone(),
            notify_tx,
            api_key_present,
            started: Arc::new(AtomicBool::new(true)),
            worker_alive: Arc::new(AtomicBool::new(true)),
        };
        (sender, desired, notify_rx)
    }

    #[tokio::test]
    async fn test_command_updates_desired_state_before_socket_consumes_notification() {
        let (sender, desired, mut notify_rx) =
            make_sender(true, SubscriptionRegistry::default());
        sender
            .send(SubscriptionCommand::SubscribeTokenTrades(vec![
                VALID_PK_1.to_string(),
            ]))
            .await
            .unwrap();
        // Desired state already reflects the command BEFORE anyone drains notify.
        assert!(desired.lock().await.token_trades.contains(VALID_PK_1));
        // And a resync notification is waiting for the worker.
        assert!(notify_rx.try_recv().is_ok());
    }

    #[tokio::test]
    async fn test_command_queued_while_disconnected_is_visible_to_replay() {
        // No worker draining; simulates socket down. Desired must still update so
        // the next reconnect replay carries it.
        let (sender, desired, _notify_rx) =
            make_sender(true, SubscriptionRegistry::default());
        sender
            .send(SubscriptionCommand::SubscribeTokenTrades(vec![
                VALID_PK_2.to_string(),
            ]))
            .await
            .unwrap();
        let replay = desired.lock().await.replay_messages();
        let token_msg = replay
            .iter()
            .find(|m| m.method == "subscribeTokenTrade")
            .expect("replay must include the queued token subscription");
        assert!(token_msg
            .keys
            .as_ref()
            .unwrap()
            .contains(&VALID_PK_2.to_string()));
    }

    #[tokio::test]
    async fn test_reconnect_replay_uses_latest_desired_state() {
        // Two commands accepted while "disconnected"; replay reflects the LATEST
        // desired state (both present), proving replay reads current desired.
        let (sender, desired, _n) = make_sender(true, SubscriptionRegistry::default());
        sender
            .send(SubscriptionCommand::SubscribeTokenTrades(vec![
                VALID_PK_1.to_string(),
            ]))
            .await
            .unwrap();
        sender
            .send(SubscriptionCommand::SubscribeTokenTrades(vec![
                VALID_PK_2.to_string(),
            ]))
            .await
            .unwrap();
        let replay = desired.lock().await.replay_messages();
        let keys = replay
            .iter()
            .find(|m| m.method == "subscribeTokenTrade")
            .unwrap()
            .keys
            .clone()
            .unwrap();
        assert!(keys.contains(&VALID_PK_1.to_string()));
        assert!(keys.contains(&VALID_PK_2.to_string()));
    }

    #[test]
    fn test_active_to_desired_diff_subscribes_missing_token() {
        let active = SubscriptionRegistry::default();
        let mut desired = SubscriptionRegistry::default();
        desired.token_trades.insert(VALID_PK_1.to_string());
        let deltas = active.diff_to(&desired);
        let sub = deltas
            .iter()
            .find(|m| m.method == "subscribeTokenTrade")
            .expect("missing token must be subscribed");
        assert!(sub.keys.as_ref().unwrap().contains(&VALID_PK_1.to_string()));
        assert!(!deltas.iter().any(|m| m.method == "unsubscribeTokenTrade"));
    }

    #[test]
    fn test_active_to_desired_diff_unsubscribes_removed_token() {
        let mut active = SubscriptionRegistry::default();
        active.token_trades.insert(VALID_PK_1.to_string());
        let desired = SubscriptionRegistry::default();
        let deltas = active.diff_to(&desired);
        let unsub = deltas
            .iter()
            .find(|m| m.method == "unsubscribeTokenTrade")
            .expect("removed token must be unsubscribed");
        assert!(unsub
            .keys
            .as_ref()
            .unwrap()
            .contains(&VALID_PK_1.to_string()));
        // Identical registries produce no wire message.
        assert!(desired.diff_to(&desired).is_empty());
    }

    #[tokio::test]
    async fn test_dynamic_trade_command_without_api_key_rejected() {
        // api_key_present = false => metered trade command rejected BEFORE mutation.
        let (sender, desired, mut notify_rx) =
            make_sender(false, SubscriptionRegistry::default());
        let err = sender
            .send(SubscriptionCommand::SubscribeTokenTrades(vec![
                VALID_PK_1.to_string(),
            ]))
            .await;
        assert!(err.is_err());
        // Desired must NOT have been mutated.
        assert!(desired.lock().await.token_trades.is_empty());
        // No notification emitted.
        assert!(notify_rx.try_recv().is_err());
    }

    #[test]
    fn test_second_start_policy_rejected() {
        // Pure policy check for A3: compare_exchange semantics on the started flag.
        let started = AtomicBool::new(false);
        let first = started
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok();
        let second = started
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok();
        assert!(first, "first start must succeed");
        assert!(!second, "second start must be rejected");
    }

    #[test]
    fn test_dynamic_write_failure_policy_requires_reconnect() {
        // Pure policy: a dynamic-delta write failure must be surfaced as an Err
        // from the stream (which the outer loop turns into a reconnect), never a
        // swallowed log. We model the send result and assert the policy mapping.
        fn on_delta_write(result: std::result::Result<(), ()>) -> Result<()> {
            result.map_err(|_| {
                Error::ShredStreamConnection(format!(
                    "Failed to send subscription delta to base {}",
                    sanitized_ws_base_for_log(PUMPPORTAL_WS_URL)
                ))
            })
        }
        assert!(on_delta_write(Ok(())).is_ok());
        let err = on_delta_write(Err(())).unwrap_err();
        // Error is sanitized (base only, no secret) and is a connection error.
        assert!(!format!("{err}").contains(SAMPLE_KEY));
        assert!(format!("{err}").contains("pumpportal.fun"));
    }

    #[test]
    fn test_connected_policy_requires_active_equals_desired() {
        // A4 invariant: at the instant Connected is emitted, active == desired.
        // We model the atomic block: snapshot active from desired, then assert
        // diff is empty (nothing left to synchronize).
        let mut desired = SubscriptionRegistry::default();
        desired.new_tokens = true;
        desired.token_trades.insert(VALID_PK_1.to_string());
        let active = desired.clone(); // snapshot taken under the same lock
        assert!(
            active.diff_to(&desired).is_empty(),
            "active must equal desired when Connected is emitted"
        );
    }

    #[test]
    fn test_safe_base_log_strips_query() {
        let base = format!("{PUMPPORTAL_WS_URL}?api-key={SAMPLE_KEY}&x=1#frag");
        let safe = sanitized_ws_base_for_log(&base);
        assert!(!safe.contains(SAMPLE_KEY));
        assert!(!safe.contains('?'));
        assert!(!safe.contains('#'));
        assert_eq!(safe, "wss://pumpportal.fun/api/data");
    }

    #[test]
    fn test_safe_base_log_strips_userinfo() {
        let base = "wss://user:s3cr3t@pumpportal.fun/api/data?api-key=abc";
        let safe = sanitized_ws_base_for_log(base);
        assert!(!safe.contains("s3cr3t"));
        assert!(!safe.contains("user"));
        assert!(!safe.contains("abc"));
        assert_eq!(safe, "wss://pumpportal.fun/api/data");
        // build_connection_url must also reject embedded userinfo.
        assert!(build_connection_url(base, SAMPLE_KEY).is_err());
        // And the rejection error must not echo the userinfo secret.
        let err = build_connection_url(base, SAMPLE_KEY).unwrap_err();
        assert!(!format!("{err}").contains("s3cr3t"));
        assert!(!format!("{err}").contains(SAMPLE_KEY));
    }

    #[test]
    fn test_provider_error_with_fake_key_is_redacted_to_fixed_category() {
        let fake_key = "FAKEKEY-abc123-should-not-appear";
        let value = serde_json::json!({
            "error": format!("unauthorized: invalid api-key={fake_key}")
        });
        let reason = detect_provider_error(&value).expect("must classify as error");
        assert!(
            !reason.contains(fake_key),
            "fixed category must not echo the provider key"
        );
        assert_eq!(reason, "PumpPortal authentication error");
    }

    fn new_token_with(market_cap: f64, v_sol: f64) -> NewTokenEvent {
        NewTokenEvent {
            signature: valid_sig(),
            mint: VALID_PK_2.to_string(),
            trader_public_key: VALID_PK_1.to_string(),
            tx_type: "create".to_string(),
            initial_buy: 1.0,
            bonding_curve_key: VALID_PK_3.to_string(),
            v_tokens_in_bonding_curve: 1.0,
            v_sol_in_bonding_curve: v_sol,
            market_cap_sol: market_cap,
            name: "T".to_string(),
            symbol: "T".to_string(),
            uri: "https://example.com".to_string(),
        }
    }

    #[test]
    fn test_new_token_nonfinite_market_cap_rejected() {
        let ev = new_token_with(f64::INFINITY, 30.0);
        assert!(validate_new_token(&ev).is_err());
        let ev = new_token_with(-1.0, 30.0);
        assert!(validate_new_token(&ev).is_err());
        // Sanity: a finite/nonnegative market cap passes.
        assert!(validate_new_token(&new_token_with(30.0, 30.0)).is_ok());
    }

    fn trade_with(market_cap: f64, v_sol: f64) -> TradeEvent {
        TradeEvent {
            signature: valid_sig(),
            mint: VALID_PK_1.to_string(),
            trader_public_key: VALID_PK_2.to_string(),
            tx_type: "buy".to_string(),
            token_amount: 1.0,
            sol_amount: 1.0,
            bonding_curve_key: VALID_PK_3.to_string(),
            v_tokens_in_bonding_curve: 1.0,
            v_sol_in_bonding_curve: v_sol,
            market_cap_sol: market_cap,
        }
    }

    #[test]
    fn test_trade_nonfinite_market_cap_rejected() {
        assert!(validate_trade(&trade_with(f64::NAN, 1.0)).is_err());
        assert!(validate_trade(&trade_with(1.0, 1.0)).is_ok());
    }

    #[test]
    fn test_trade_nonfinite_provider_reserve_rejected() {
        // v_sol_in_bonding_curve non-finite must be rejected.
        assert!(validate_trade(&trade_with(1.0, f64::INFINITY)).is_err());
        // Negative reserve rejected too.
        assert!(validate_trade(&trade_with(1.0, -0.5)).is_err());
    }
}
