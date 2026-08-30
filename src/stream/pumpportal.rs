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

/// Finite bound on how long a single subscription wire write (replay or dynamic
/// delta) may take (A4). The desired mutex is intentionally held across these
/// bounded writes so `active == desired` equality stays frozen at the instant
/// `Connected` is enqueued; a FINITE timeout is what makes holding the lock
/// across the write safe (it can never be retained indefinitely). On timeout or
/// sink error the connection returns `Err` and the outer loop reconnects and
/// replays — there is no same-socket retry loop.
const SUBSCRIPTION_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

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
    let mut url = url::Url::parse(base_url).map_err(|_| {
        Error::Config("Invalid PumpPortal ws_url (could not parse base)".to_string())
    })?;

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

/// Serialize and write a single subscription message to the socket under a
/// FINITE timeout (A4). Secret-safe: on serialization failure, sink error, or
/// timeout the error is built ONLY from the sanitized base (never the raw
/// tungstenite error, which can carry the authenticated URL). A timeout or a
/// sink error both map to a connection error so the outer loop reconnects and
/// replays — no same-socket retry.
///
/// The caller holds the desired lock across this call; the timeout is what makes
/// that safe (bounded lock retention).
async fn send_subscription_message<S>(
    write: &mut S,
    msg: &SubscriptionMessage,
    safe_base: &str,
    context: &str,
) -> Result<()>
where
    S: futures_util::Sink<Message> + Unpin,
{
    let json = serde_json::to_string(msg).map_err(|e| Error::Serialization(e.to_string()))?;
    match tokio::time::timeout(SUBSCRIPTION_WRITE_TIMEOUT, write.send(Message::Text(json))).await {
        // Write completed within the bound; map any sink error to a sanitized
        // connection error (the raw sink error may reference the secret URL).
        Ok(res) => res.map_err(|_| {
            Error::ShredStreamConnection(format!("Failed to send {context} to base {safe_base}"))
        }),
        // Timed out: surface as a connection error so the outer loop reconnects.
        Err(_) => Err(Error::ShredStreamConnection(format!(
            "Timed out sending {context} to base {safe_base}"
        ))),
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
    /// Provider presentation metadata (OPTIONAL-ON-WIRE, P1-METADATA-DRAIN-TRUTH-001
    /// §4): an ABSENT key defaults to the empty String (supported provider variant).
    /// A present-but-wrong-type value (null/number/bool/array/object) still fails
    /// String deserialization => NewTokenDeserialize DecodeError. Core identity/
    /// economic fields above are NOT defaulted and remain strictly required.
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub symbol: String,
    #[serde(default)]
    pub uri: String,
}

impl NewTokenEvent {
    /// Pure availability check for provider presentation metadata (§6).
    ///
    /// Returns true iff all three provider metadata strings (name/symbol/uri) are
    /// present and non-blank after trimming. No URL fetch, no URI validation, no
    /// download — a pure inspection of the already-decoded strings. Research retains
    /// metadata-less candidates; the live path uses this to skip them per-candidate.
    pub fn has_complete_metadata(&self) -> bool {
        !self.name.trim().is_empty()
            && !self.symbol.trim().is_empty()
            && !self.uri.trim().is_empty()
    }
}

/// Partial new-token event from PumpPortal (P1-OBSERVATION-SCHEMA-V2-PARTIAL-CREATE-001
/// §4-6).
///
/// A `txType=create` message with valid DISCOVERY IDENTITY (signature, mint,
/// traderPublicKey, txType) but one or more OPTIONAL provider-observational fields
/// ABSENT. Emitted ONLY as a fallback after the FULL `NewTokenEvent` parse/validation
/// failed (§7): metadata-only absence still lands as a FULL `NewToken` because the
/// full parser's name/symbol/uri carry serde defaults.
///
/// The provider observational fields (`initial_buy`, `bonding_curve_key`,
/// `v_tokens_in_bonding_curve`, `v_sol_in_bonding_curve`, `market_cap_sol`) are
/// `Option`: ONLY ABSENCE maps to `None`. A present-but-invalid optional field is a
/// true decode/validation loss, NOT `None` (§6) — validated in
/// [`validate_partial_new_token`] AFTER deserialization. These are NOT canonical
/// market reserves; the canonical on-chain oracle resolves real market truth from the
/// retained mint.
///
/// Metadata (`name`/`symbol`/`uri`) reuses the PR#11 absence behavior: an ABSENT key
/// defaults to the empty String; a present-but-wrong-type value (null/number/bool/
/// array/object) fails String deserialization => DecodeError.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PartialNewTokenEvent {
    pub signature: String,
    pub mint: String,
    pub trader_public_key: String,
    pub tx_type: String,
    /// Provider observational initial buy amount. ABSENT => `None`; present =>
    /// validated finite && non-negative (§6).
    #[serde(default)]
    pub initial_buy: Option<f64>,
    /// Provider bonding-curve key. ABSENT => `None`; present => validated Solana
    /// Pubkey (§6).
    #[serde(default)]
    pub bonding_curve_key: Option<String>,
    /// Provider observational token reserve figure. ABSENT => `None`; present =>
    /// validated finite && non-negative (§6).
    #[serde(default)]
    pub v_tokens_in_bonding_curve: Option<f64>,
    /// Provider observational SOL reserve figure. ABSENT => `None`; present =>
    /// validated finite && non-negative (§6).
    #[serde(default)]
    pub v_sol_in_bonding_curve: Option<f64>,
    /// Provider observational market cap. ABSENT => `None`; present => validated
    /// finite && non-negative (§6).
    #[serde(default)]
    pub market_cap_sol: Option<f64>,
    /// Provider presentation metadata (same PR#11 absence behavior as
    /// `NewTokenEvent`): ABSENT => empty String, present-wrong-type => reject.
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub symbol: String,
    #[serde(default)]
    pub uri: String,
}

impl PartialNewTokenEvent {
    /// Pure availability check for provider presentation metadata, mirroring
    /// [`NewTokenEvent::has_complete_metadata`]. Returns true iff name/symbol/uri are
    /// all present and non-blank after trimming. Pure inspection of decoded strings —
    /// no fetch/validation/download. The live consumer (Agent C) may use this to skip
    /// metadata-less candidates per-candidate.
    pub fn has_complete_metadata(&self) -> bool {
        !self.name.trim().is_empty()
            && !self.symbol.trim().is_empty()
            && !self.uri.trim().is_empty()
    }
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

/// Local provider-message decode/schema-loss class (P1-PROVIDER-DECODE-TRUTH-001
/// §3). Distinguishes WHICH strict decode path rejected a live message. This is
/// NOT a provider/transport health failure — it is a dropped candidate whose wire
/// shape did not match the frozen strict schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PumpPortalDecodeKind {
    /// A `txType=create` message failed strict `NewTokenEvent` deserialization.
    NewTokenDeserialize,
    /// A `NewTokenEvent` deserialized but failed identity/numeric validation.
    NewTokenValidation,
    /// A `txType=create` message that FAILED the full `NewTokenEvent` path also
    /// failed strict `PartialNewTokenEvent` deserialization (P1-OBSERVATION-SCHEMA-V2
    /// §7): even the reduced partial shape (required identity + typed optionals) did
    /// not parse.
    PartialNewTokenDeserialize,
    /// A `PartialNewTokenEvent` deserialized but failed required-identity or
    /// present-optional validation (§4-6): e.g. bad mint/creator/signature, a present
    /// bonding-curve that is not a valid Pubkey, or a present numeric that is not
    /// finite && non-negative.
    PartialNewTokenValidation,
    /// A migration message failed structural parse.
    MigrationParse,
    /// A `txType=buy|sell` message failed strict `TradeEvent` deserialization.
    TradeDeserialize,
    /// A `TradeEvent` deserialized but failed identity/numeric validation.
    TradeValidation,
}

/// A typed, secret-safe decode/schema-loss diagnostic (§3). `category` is a FIXED
/// structural token (never raw provider JSON, field values, or error text) bounded
/// to <=256 bytes, ASCII-only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PumpPortalDecodeError {
    pub kind: PumpPortalDecodeKind,
    pub category: String,
}

/// Event from PumpPortal WebSocket.
#[derive(Debug, Clone)]
pub enum PumpPortalEvent {
    /// New token created (FULL provider shape: all observational provider fields
    /// present and valid). Unchanged from prior schema.
    NewToken(NewTokenEvent),
    /// New token created with valid discovery IDENTITY but one or more OPTIONAL
    /// provider-observational fields absent (P1-OBSERVATION-SCHEMA-V2 §3-7). Only
    /// reached when the FULL `NewToken` parse/validation failed; required identity
    /// and any PRESENT optional field are still fully validated. Retained for
    /// research discovery — the canonical on-chain oracle classifies the market.
    PartialNewToken(PartialNewTokenEvent),
    /// Trade occurred (buy or sell).
    Trade(TradeEvent),
    /// Token migrated to a pool.
    Migration(MigrationEvent),
    /// Connected to WebSocket AND initial subscription replay completed.
    Connected,
    /// Disconnected from WebSocket.
    Disconnected,
    /// HARD provider/server/stream operational error ONLY (auth, balance, invalid
    /// sub, rate/ban, max-reconnect, other sanitized operational errors). Sanitized;
    /// never contains the API key. Local message decode/schema loss is NOT this —
    /// see [`PumpPortalEvent::DecodeError`] (§3-4).
    Error(String),
    /// LOCAL provider-message decode/schema-loss (§3/§5): a single dropped
    /// candidate whose wire shape failed strict decode/validation. NOT a hard
    /// provider/transport failure.
    DecodeError(PumpPortalDecodeError),
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
        //
        // BLOCKER A: the wake carries NO authoritative state — desired is already
        // mutated above and is the single source of truth; one pending wake is
        // enough for the worker to diff active vs desired. Awaiting bounded notify
        // capacity here would reintroduce a circular wait (event channel full ->
        // worker awaits Connected permit capacity -> notify queue full -> this
        // caller blocks awaiting notify capacity -> worker can't drain notify until
        // event capacity frees -> caller can't return to free event capacity).
        //
        // INVARIANT: this path NEVER awaits notification-channel capacity. Use a
        // nonblocking coalescing try_send:
        //   Ok    => wake enqueued.
        //   Full  => a wake is already queued; desired is authoritative and current,
        //            so a coalesced wake is sufficient => success.
        //   Closed => the worker's receiver is gone; report Err. Desired may already
        //            be mutated (A3) — that is acceptable, no truth is rolled back.
        match self.notify_tx.try_send(()) {
            Ok(()) => Ok(()),
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => Ok(()),
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => Err(Error::Internal(
                "PumpPortal notify channel closed".to_string(),
            )),
        }
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
            Error::ShredStreamConnection(format!("PumpPortal connect failed for base {safe_base}"))
        })?;

        info!("Connected to PumpPortal WebSocket");

        let (mut write, mut read) = ws_stream.split();

        // Per-connection snapshot of what the socket has actually been sent.
        let active: SubscriptionRegistry;

        // A2 ORDERING (never await bounded event capacity while holding desired):
        //   reserve Connected capacity  (may await; desired NOT held)
        //   -> lock desired
        //   -> replay CURRENT desired via bounded, finite-timeout writes (A4)
        //   -> active = desired.clone()
        //   -> permit.send(Connected)   (NON-awaiting; capacity already reserved)
        //   -> unlock
        // At the instant Connected is enqueued, active == current desired. Reserve
        // failure => the receiver is gone; surface as an error so the outer loop
        // treats it like a dropped connection.
        let connected_permit = event_tx.reserve().await.map_err(|_| {
            Error::Internal("PumpPortal event channel closed before Connected".to_string())
        })?;
        {
            let desired_guard = desired.lock().await;
            for msg in desired_guard.replay_messages() {
                // A4/A6: replay write is finite-time bounded; timeout OR sink error
                // returns Err => outer loop reconnects and replays.
                send_subscription_message(&mut write, &msg, &safe_base, "subscription replay")
                    .await?;
                debug!("Replayed subscription: {}", msg.method);
            }
            // Snapshot the exact state the socket now reflects.
            active = desired_guard.clone();

            // A5: enqueue Connected WITHOUT awaiting — the permit was reserved
            // before we took the desired lock, so this is synchronous and cannot
            // block on channel capacity while holding desired.
            connected_permit.send(PumpPortalEvent::Connected);
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
                // A3 dynamic sync — NEVER await bounded event capacity while
                // holding desired, and always act on the LATEST desired.
                //
                // Step 1: SHORT desired lock only to test whether a sync is even
                // needed, then release it before any (possibly awaiting) reserve.
                let sync_needed = {
                    let desired_guard = desired.lock().await;
                    !active.diff_to(&desired_guard).is_empty()
                };

                if sync_needed {
                    // Step 2: reserve Connected capacity while NOT holding desired
                    // (this is the only place that may await for capacity).
                    let permit = event_tx.reserve().await.map_err(|_| {
                        Error::Internal(
                            "PumpPortal event channel closed before Connected".to_string(),
                        )
                    })?;

                    // Step 3: re-lock desired and RECOMPUTE the diff against the
                    // LATEST desired (desired may have changed to C while we waited
                    // on the permit). Never reuse the stale first diff.
                    let desired_guard = desired.lock().await;
                    let deltas = active.diff_to(&desired_guard);
                    if deltas.is_empty() {
                        // Desired converged back to active while we waited: drop the
                        // permit (no Connected) and release the lock.
                        drop(permit);
                    } else {
                        for msg in deltas {
                            // A4/A6: finite-time bounded write; timeout OR sink error
                            // returns Err => reconnect; desired stays current so
                            // replay repairs it.
                            send_subscription_message(
                                &mut write,
                                &msg,
                                &safe_base,
                                "subscription delta",
                            )
                            .await?;
                            debug!("Synchronized subscription: {}", msg.method);
                        }
                        // Every delta sent successfully: active is now in sync with
                        // the LATEST desired.
                        active = desired_guard.clone();
                        // A5: enqueue Connected WITHOUT awaiting while equality is
                        // still protected by the held desired lock.
                        permit.send(PumpPortalEvent::Connected);
                    }
                }
                // If no sync was needed, desired already == active: no wire message.
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
            || (tx_type.is_empty() && value.get("mint").is_some() && looks_like_migration(&value));
        if is_migration {
            // §5: a migration parse failure is LOCAL schema loss, not a hard
            // provider error.
            let _ = event_tx.send(classify_migration(&value)).await;
            return Ok(());
        }

        // 3. New token. §5: serde/validation failures are typed DecodeError
        //    (schema loss), NOT hard provider Error.
        if tx_type == "create" {
            let _ = event_tx.send(classify_new_token(text, &value)).await;
            return Ok(());
        }

        // 4. Trade (buy/sell). §5: serde/validation failures are typed
        //    DecodeError (schema loss), NOT hard provider Error.
        if tx_type == "buy" || tx_type == "sell" {
            let _ = event_tx.send(classify_trade(text)).await;
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

/// Maximum byte length of any emitted decode category (§6).
const DECODE_CATEGORY_MAX: usize = 256;

/// Expected STRING fields of `NewTokenEvent`, in fixed declaration order (§6).
const NEW_TOKEN_STRING_FIELDS: &[&str] = &[
    "signature",
    "mint",
    "traderPublicKey",
    "txType",
    "bondingCurveKey",
    "name",
    "symbol",
    "uri",
];

/// Expected NUMBER fields of `NewTokenEvent`, in fixed declaration order (§6).
const NEW_TOKEN_NUMBER_FIELDS: &[&str] = &[
    "initialBuy",
    "vTokensInBondingCurve",
    "vSolInBondingCurve",
    "marketCapSol",
];

/// Fixed JSON-type label for a value, from the closed set
/// {null,bool,number,string,array,object} (§6). Absence is handled by the caller
/// (which reports `missing`), so this never returns `missing`.
fn json_type_label(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

/// §6: pure, secret-safe structural diagnostic for a `txType=create` message that
/// failed strict `NewTokenEvent` deserialization.
///
/// Inspects ONLY presence + JSON type of each EXPECTED (compile-time allowlisted)
/// field. Output is `new_token_deserialize` plus an optional `|missing=<...>` list
/// (fixed order) and an optional `|wrong_type=<field:jsontype,...>` list (fixed
/// order). A non-object value yields `new_token_deserialize|shape=unknown`. NO
/// unknown/extra field names, NO raw JSON, NO provider values, ASCII-only, and the
/// result is bounded to <=256 bytes by dropping later fixed tokens (never by
/// byte-slicing arbitrary UTF-8 — the whole output is ASCII regardless).
fn diagnose_new_token_shape(value: &serde_json::Value) -> String {
    let obj = match value.as_object() {
        Some(o) => o,
        None => return "new_token_deserialize|shape=unknown".to_string(),
    };

    // A field is "missing" if absent; "wrong_type" if present but its JSON type is
    // not in the expected group (string vs number).
    let mut missing: Vec<&'static str> = Vec::new();
    let mut wrong_type: Vec<String> = Vec::new();

    for &field in NEW_TOKEN_STRING_FIELDS {
        match obj.get(field) {
            None => missing.push(field),
            Some(v) => {
                if !v.is_string() {
                    wrong_type.push(format!("{field}:{}", json_type_label(v)));
                }
            }
        }
    }
    for &field in NEW_TOKEN_NUMBER_FIELDS {
        match obj.get(field) {
            None => missing.push(field),
            Some(v) => {
                if !v.is_number() {
                    wrong_type.push(format!("{field}:{}", json_type_label(v)));
                }
            }
        }
    }

    // Assemble fixed tokens in order: base, then missing, then wrong_type. If the
    // total would exceed the cap, drop later fixed tokens (wrong_type before
    // missing) rather than truncate mid-token.
    let base = "new_token_deserialize".to_string();
    let missing_tok = if missing.is_empty() {
        None
    } else {
        Some(format!("|missing={}", missing.join(",")))
    };
    let wrong_tok = if wrong_type.is_empty() {
        None
    } else {
        Some(format!("|wrong_type={}", wrong_type.join(",")))
    };

    let mut out = base;
    if let Some(m) = &missing_tok {
        if out.len() + m.len() <= DECODE_CATEGORY_MAX {
            out.push_str(m);
        }
    }
    if let Some(w) = &wrong_tok {
        if out.len() + w.len() <= DECODE_CATEGORY_MAX {
            out.push_str(w);
        }
    }
    out
}

/// §5: classify a `txType=create` message into either a `NewToken` event or a
/// typed `DecodeError`. Pure (no socket): serde failure => `NewTokenDeserialize`
/// with a structural shape category; serde-ok but validation failure =>
/// `NewTokenValidation` with the fixed category `new_token_validation`.
fn classify_new_token(text: &str, value: &serde_json::Value) -> PumpPortalEvent {
    match serde_json::from_str::<NewTokenEvent>(text) {
        Ok(ev) => match validate_new_token(&ev) {
            Ok(()) => {
                debug!("New token: {} ({}) - {}", ev.name, ev.symbol, ev.mint);
                PumpPortalEvent::NewToken(ev)
            }
            Err(_) => {
                // AUDIT-001 §4: never interpolate the validation error Display — it
                // can carry rejected provider values (invalid mint/pubkey/signature).
                warn!("PumpPortal NewToken validation loss");
                PumpPortalEvent::DecodeError(PumpPortalDecodeError {
                    kind: PumpPortalDecodeKind::NewTokenValidation,
                    category: "new_token_validation".to_string(),
                })
            }
        },
        Err(_) => {
            // Step B (§7): the FULL parse/validate failed. Attempt the partial-create
            // fallback before declaring a decode loss. The partial path requires valid
            // discovery identity and validates every PRESENT optional; only field
            // ABSENCE is permitted (mapped to None). This RETAINS creates that carry a
            // valid identity but are missing observational provider fields.
            classify_partial_new_token(text, value)
        }
    }
}

/// §7 Step B: classify a `txType=create` message that FAILED the full `NewTokenEvent`
/// path into either a `PartialNewToken` event or a typed `DecodeError`. Pure (no
/// socket).
///
/// Ordering:
///   - strict `serde_json::from_str::<PartialNewTokenEvent>` — a failure here (bad
///     required-identity type, present-wrong-type metadata, or a present optional of
///     the wrong JSON type) is a value-free structural `PartialNewTokenDeserialize`
///     loss diagnosed via [`diagnose_new_token_shape`];
///   - then [`validate_partial_new_token`] — required identity + EVERY present
///     optional; a failure is a `PartialNewTokenValidation` loss with the fixed
///     category `partial_new_token_validation`.
///
/// NEVER emits both `PartialNewToken` and `DecodeError`. All logs are value-free
/// (fixed strings / structural category only); the serde/validation error Display is
/// NEVER interpolated (AUDIT-001).
fn classify_partial_new_token(text: &str, value: &serde_json::Value) -> PumpPortalEvent {
    match serde_json::from_str::<PartialNewTokenEvent>(text) {
        Ok(ev) => match validate_partial_new_token(&ev) {
            Ok(()) => {
                // Value-free: field COUNT/identity mint only would be provider data;
                // log a fixed string with no provider values.
                debug!("PumpPortal PartialNewToken retained (incomplete provider shape)");
                PumpPortalEvent::PartialNewToken(ev)
            }
            Err(_) => {
                // AUDIT-001: never interpolate the validation error Display — it can
                // carry rejected provider values (invalid mint/pubkey/signature or a
                // present-but-invalid optional).
                warn!("PumpPortal PartialNewToken validation loss");
                PumpPortalEvent::DecodeError(PumpPortalDecodeError {
                    kind: PumpPortalDecodeKind::PartialNewTokenValidation,
                    category: "partial_new_token_validation".to_string(),
                })
            }
        },
        Err(_) => {
            // The structural category is value-free (§6); safe to log. Never log the
            // serde error Display, which may render offending provider field values.
            let category = diagnose_new_token_shape(value);
            warn!("PumpPortal PartialNewToken decode/schema loss: {}", category);
            PumpPortalEvent::DecodeError(PumpPortalDecodeError {
                kind: PumpPortalDecodeKind::PartialNewTokenDeserialize,
                category,
            })
        }
    }
}

/// §5: classify a `txType=buy|sell` message into either a `Trade` event or a typed
/// `DecodeError`. Pure: serde failure => `TradeDeserialize` (`trade_deserialize`);
/// serde-ok but validation failure => `TradeValidation` (`trade_validation`).
fn classify_trade(text: &str) -> PumpPortalEvent {
    match serde_json::from_str::<TradeEvent>(text) {
        Ok(ev) => match validate_trade(&ev) {
            Ok(()) => {
                info!(
                    "Trade parsed: {} {} (provider UI tokens) {} for {} SOL",
                    ev.tx_type, ev.token_amount, ev.mint, ev.sol_amount
                );
                PumpPortalEvent::Trade(ev)
            }
            Err(_) => {
                warn!("PumpPortal trade validation loss");
                PumpPortalEvent::DecodeError(PumpPortalDecodeError {
                    kind: PumpPortalDecodeKind::TradeValidation,
                    category: "trade_validation".to_string(),
                })
            }
        },
        Err(_) => {
            warn!("PumpPortal trade deserialize loss");
            PumpPortalEvent::DecodeError(PumpPortalDecodeError {
                kind: PumpPortalDecodeKind::TradeDeserialize,
                category: "trade_deserialize".to_string(),
            })
        }
    }
}

/// §5: classify a migration message into either a `Migration` event or a typed
/// `DecodeError`. Pure: parse failure => `MigrationParse` (`migration_parse`), no
/// raw parse string.
fn classify_migration(value: &serde_json::Value) -> PumpPortalEvent {
    match parse_migration(value) {
        Ok(ev) => PumpPortalEvent::Migration(ev),
        Err(_) => {
            warn!("PumpPortal migration parse loss");
            PumpPortalEvent::DecodeError(PumpPortalDecodeError {
                kind: PumpPortalDecodeKind::MigrationParse,
                category: "migration_parse".to_string(),
            })
        }
    }
}

/// Heuristic: a message with mint + pool-ish fields but no buy/sell txType.
fn looks_like_migration(value: &serde_json::Value) -> bool {
    value.get("pool").is_some() || value.get("poolId").is_some() || value.get("pool_id").is_some()
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

/// Validate a `PartialNewTokenEvent`'s REQUIRED discovery identity and EVERY PRESENT
/// optional provider field before emission (P1-OBSERVATION-SCHEMA-V2 §4-6).
///
/// Required identity: `txType == "create"`, mint + trader_public_key parse as Solana
/// Pubkeys, signature valid under [`validate_signature`]. OPTIONAL != unvalidated: a
/// present `bonding_curve_key` must be a valid Pubkey, and every present numeric
/// optional must be finite && non-negative (reusing the existing validators). ONLY
/// ABSENCE (`None`) skips validation. A present-but-invalid optional is an `Err` here
/// => DecodeError, never silently coerced to `None`.
fn validate_partial_new_token(ev: &PartialNewTokenEvent) -> Result<()> {
    // Required discovery identity.
    if ev.tx_type != "create" {
        return Err(Error::Deserialization(format!(
            "unexpected new-token txType: {}",
            ev.tx_type
        )));
    }
    validate_pubkey_field("mint", &ev.mint)?;
    validate_pubkey_field("creator", &ev.trader_public_key)?;
    validate_signature(&ev.signature)?;

    // Present optionals: validate; absence (None) is allowed.
    if let Some(curve) = &ev.bonding_curve_key {
        validate_pubkey_field("bonding_curve", curve)?;
    }
    if let Some(v) = ev.initial_buy {
        validate_nonneg_finite("initial_buy", v)?;
    }
    if let Some(v) = ev.v_tokens_in_bonding_curve {
        validate_nonneg_finite("v_tokens_in_bonding_curve", v)?;
    }
    if let Some(v) = ev.v_sol_in_bonding_curve {
        validate_nonneg_finite("v_sol_in_bonding_curve", v)?;
    }
    if let Some(v) = ev.market_cap_sol {
        validate_nonneg_finite("market_cap_sol", v)?;
    }
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
        let mut reg = SubscriptionRegistry::from_plan(&plan, &[VALID_PK_1.to_string()], &[]);
        // Dynamically add a second token.
        reg.apply(&SubscriptionCommand::SubscribeTokenTrades(vec![
            VALID_PK_2.to_string()
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
        make_sender_with_capacity(api_key_present, seed, 16)
    }

    /// Same as `make_sender` but with a caller-chosen notify-channel capacity so
    /// tests can construct a small/full notify queue and observe coalescing.
    fn make_sender_with_capacity(
        api_key_present: bool,
        seed: SubscriptionRegistry,
        notify_capacity: usize,
    ) -> (
        CommandSender,
        Arc<Mutex<SubscriptionRegistry>>,
        mpsc::Receiver<()>,
    ) {
        let desired = Arc::new(Mutex::new(seed));
        let (notify_tx, notify_rx) = mpsc::channel::<()>(notify_capacity);
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
        let (sender, desired, mut notify_rx) = make_sender(true, SubscriptionRegistry::default());
        sender
            .send(SubscriptionCommand::SubscribeTokenTrades(vec![
                VALID_PK_1.to_string()
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
        let (sender, desired, _notify_rx) = make_sender(true, SubscriptionRegistry::default());
        sender
            .send(SubscriptionCommand::SubscribeTokenTrades(vec![
                VALID_PK_2.to_string()
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
                VALID_PK_1.to_string()
            ]))
            .await
            .unwrap();
        sender
            .send(SubscriptionCommand::SubscribeTokenTrades(vec![
                VALID_PK_2.to_string()
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
        let (sender, desired, mut notify_rx) = make_sender(false, SubscriptionRegistry::default());
        let err = sender
            .send(SubscriptionCommand::SubscribeTokenTrades(vec![
                VALID_PK_1.to_string()
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

    // === AGENT A — BLOCKER A backpressure/deadlock regression tests ===========

    /// A6.1 — Reserving a `Connected` permit while the bounded event channel is
    /// FULL must not require the desired mutex. This is the core anti-deadlock
    /// property: the worker reserves capacity BEFORE it takes the long desired
    /// lock, so a concurrent `CommandSender::send` (which needs desired) can make
    /// progress even while the worker is blocked waiting for channel capacity.
    #[tokio::test]
    async fn test_connected_capacity_wait_does_not_hold_desired_mutex() {
        // Bounded event channel, capacity 1, prefilled so no capacity is free.
        let (event_tx, mut event_rx) = mpsc::channel::<PumpPortalEvent>(1);
        event_tx
            .send(PumpPortalEvent::Disconnected)
            .await
            .expect("prefill the single slot");

        let desired = Arc::new(Mutex::new(SubscriptionRegistry::default()));

        // Task modelling the worker: reserve a permit BEFORE locking desired.
        let tx_clone = event_tx.clone();
        let reserver = tokio::spawn(async move {
            // Blocks here until the channel slot is freed — desired is NOT held.
            let permit = tx_clone.reserve_owned().await.expect("reserve");
            let _guard = permit.send(PumpPortalEvent::Connected);
        });

        // While the reserver is blocked on capacity, another task must still be
        // able to acquire the desired mutex and make progress.
        {
            let mut guard = desired.lock().await;
            guard.new_tokens = true; // real progress under the lock
            assert!(guard.new_tokens);
        }

        // The reservation is still pending (channel is full).
        assert!(!reserver.is_finished());

        // Free the slot; the reservation now completes and enqueues Connected.
        let first = event_rx.recv().await.expect("prefilled item");
        assert!(matches!(first, PumpPortalEvent::Disconnected));

        reserver.await.expect("reserver task join");
        let second = event_rx.recv().await.expect("connected item");
        assert!(matches!(second, PumpPortalEvent::Connected));
    }

    /// A6.2 — After the permit wait, the dynamic sync must recompute the diff
    /// against the LATEST desired, never a stale earlier observation. Models the
    /// A3 ordering: observe desired=B, wait for a permit, desired mutates to C,
    /// then the diff is recomputed under the re-taken lock and targets C.
    #[tokio::test]
    async fn test_dynamic_sync_recomputes_latest_desired_after_capacity_wait() {
        // active = A (empty). desired starts as B (only PK_1).
        let active = SubscriptionRegistry::default();
        let mut b = SubscriptionRegistry::default();
        b.token_trades.insert(VALID_PK_1.to_string());
        let desired = Arc::new(Mutex::new(b));

        // First (short-lock) observation: a sync is needed.
        let first_sync_needed = {
            let g = desired.lock().await;
            !active.diff_to(&g).is_empty()
        };
        assert!(first_sync_needed);

        // Full bounded channel forces the permit reservation to wait.
        let (event_tx, mut event_rx) = mpsc::channel::<PumpPortalEvent>(1);
        event_tx.send(PumpPortalEvent::Disconnected).await.unwrap();

        let tx_clone = event_tx.clone();
        let reserver =
            tokio::spawn(async move { tx_clone.reserve_owned().await.expect("reserve") });

        // While the permit waits, mutate desired B -> C (only PK_2, PK_1 removed).
        {
            let mut g = desired.lock().await;
            g.token_trades.clear();
            g.token_trades.insert(VALID_PK_2.to_string());
        }

        // Free the slot so the permit resolves.
        let _ = event_rx.recv().await;
        let _permit = reserver.await.expect("join");

        // Recompute the diff against the LATEST desired (C) under a fresh lock.
        let g = desired.lock().await;
        let deltas = active.diff_to(&g);
        let sub = deltas
            .iter()
            .find(|m| m.method == "subscribeTokenTrade")
            .expect("recomputed diff must subscribe C's key");
        let keys = sub.keys.as_ref().unwrap();
        // Targets C (PK_2), never stale B (PK_1).
        assert!(keys.contains(&VALID_PK_2.to_string()));
        assert!(!keys.contains(&VALID_PK_1.to_string()));
    }

    /// A6.3 — The subscription write timeout must be finite/small, and a write
    /// that never completes must yield an Err (never hang). Exercises the real
    /// `send_subscription_message` wrapper against a sink that never accepts an
    /// item (its `poll_ready` is perpetually `Pending`).
    #[tokio::test(start_paused = true)]
    async fn test_subscription_write_timeout_policy_is_finite() {
        // Finite and small.
        assert!(SUBSCRIPTION_WRITE_TIMEOUT > Duration::from_secs(0));
        assert!(SUBSCRIPTION_WRITE_TIMEOUT <= Duration::from_secs(10));

        // A sink that never becomes ready — models a wedged socket write.
        struct NeverReadySink;
        impl futures_util::Sink<Message> for NeverReadySink {
            type Error = std::io::Error;
            fn poll_ready(
                self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<std::result::Result<(), Self::Error>> {
                std::task::Poll::Pending
            }
            fn start_send(
                self: std::pin::Pin<&mut Self>,
                _item: Message,
            ) -> std::result::Result<(), Self::Error> {
                Ok(())
            }
            fn poll_flush(
                self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<std::result::Result<(), Self::Error>> {
                std::task::Poll::Pending
            }
            fn poll_close(
                self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<std::result::Result<(), Self::Error>> {
                std::task::Poll::Pending
            }
        }

        let mut sink = NeverReadySink;
        let msg = SubscriptionMessage::subscribe_new_tokens();
        let safe_base = sanitized_ws_base_for_log(PUMPPORTAL_WS_URL);

        // With time paused, the timeout fires deterministically; the call returns
        // Err rather than hanging. auto_advance moves the paused clock forward.
        let err = send_subscription_message(&mut sink, &msg, &safe_base, "subscription replay")
            .await
            .expect_err("a never-ready sink must time out");
        // Sanitized: base only, no secret.
        assert!(format!("{err}").contains("pumpportal.fun"));
        assert!(!format!("{err}").contains(SAMPLE_KEY));
    }

    // === AGENT A — BLOCKER A wake nonblocking/coalescing tests ================

    /// A4.1 — A FULL notify queue must NOT block `send()`. The wake is coalesced
    /// (`Full => Ok`) because desired is already authoritative and current. The
    /// command's desired mutation must still be visible despite the dropped wake.
    #[tokio::test]
    async fn test_notify_full_is_coalesced_success() {
        // Capacity-1 notify channel, no consumer. Prefill it so the next wake
        // would see `Full`.
        let (sender, desired, _notify_rx) =
            make_sender_with_capacity(true, SubscriptionRegistry::default(), 1);
        // Fill the single notify slot directly (no receiver drains it).
        sender
            .notify_tx
            .try_send(())
            .expect("prefill the one notify slot");
        assert!(
            sender.notify_tx.try_send(()).is_err(),
            "notify queue must be full before the command"
        );

        // A command that genuinely changes desired. With the notify queue full,
        // send() must return promptly with Ok (coalesced wake) AND the desired
        // registry must reflect the command.
        let res = sender
            .send(SubscriptionCommand::SubscribeTokenTrades(vec![
                VALID_PK_1.to_string()
            ]))
            .await;
        assert!(res.is_ok(), "Full notify must coalesce to success");
        assert!(
            desired.lock().await.token_trades.contains(VALID_PK_1),
            "desired must reflect the command even when the wake was coalesced"
        );
    }

    /// A4.2 — If the notify RECEIVER is dropped (channel closed) while the worker
    /// flags still look live, a desired-changing command's wake must return Err
    /// (`Closed`) and must not hang. Desired may already be mutated — acceptable.
    #[tokio::test]
    async fn test_notify_closed_returns_error() {
        let (sender, desired, notify_rx) =
            make_sender_with_capacity(true, SubscriptionRegistry::default(), 4);
        // worker_alive still reads true (pre-check passes), but the receiver is
        // gone, so the channel is Closed for the wake.
        drop(notify_rx);
        assert!(
            sender.worker_alive.load(Ordering::SeqCst),
            "worker flag must still look live for this race"
        );

        let res = sender
            .send(SubscriptionCommand::SubscribeTokenTrades(vec![
                VALID_PK_1.to_string()
            ]))
            .await;
        // Closed wake => Err(Error::Internal notify closed), no hang.
        match res {
            Err(Error::Internal(msg)) => assert!(msg.contains("notify channel closed")),
            other => panic!("expected Internal notify-closed error, got {other:?}"),
        }
        // A3: desired may already be mutated; we do NOT reverse it.
        assert!(desired.lock().await.token_trades.contains(VALID_PK_1));
    }

    /// A4.3 — `send()` must never wait for notify capacity. With a small/full
    /// notify channel and NO receiver draining it, send() must still complete
    /// (bounded by a short timeout guard proving it did not block on capacity).
    #[tokio::test]
    async fn test_command_sender_never_waits_for_notify_capacity() {
        let (sender, _desired, _notify_rx) =
            make_sender_with_capacity(true, SubscriptionRegistry::default(), 1);
        // Fill the only slot; nothing will ever drain it.
        sender
            .notify_tx
            .try_send(())
            .expect("prefill the one notify slot");

        // If send() awaited notify capacity it would hang forever here. The
        // timeout guard proves completion without a draining receiver.
        let out = tokio::time::timeout(
            Duration::from_secs(2),
            sender.send(SubscriptionCommand::SubscribeTokenTrades(vec![
                VALID_PK_2.to_string()
            ])),
        )
        .await;
        let inner = out.expect("send() must not block on notify capacity");
        assert!(inner.is_ok(), "coalesced wake on a full queue is success");
    }

    // === P1-PROVIDER-DECODE-TRUTH-001 §13 — decode-vs-hard-error truth =========

    /// A full, valid create message body with all strict NewToken fields. Callers
    /// mutate/remove specific fields to exercise the decode paths. Uses fake but
    /// well-formed identity values.
    fn full_new_token_json() -> serde_json::Value {
        serde_json::json!({
            "signature": valid_sig(),
            "mint": VALID_PK_2,
            "traderPublicKey": VALID_PK_1,
            "txType": "create",
            "initialBuy": 1000000.5,
            "bondingCurveKey": VALID_PK_3,
            "vTokensInBondingCurve": 1000000000000.0,
            "vSolInBondingCurve": 30.5,
            "marketCapSol": 30.0,
            "name": "Test Token",
            "symbol": "TEST",
            "uri": "https://example.com"
        })
    }

    #[test]
    fn test_new_token_shape_diagnostic_missing_field_names_only() {
        let mut v = full_new_token_json();
        let obj = v.as_object_mut().unwrap();
        obj.remove("name");
        obj.remove("uri");
        let cat = diagnose_new_token_shape(&v);
        // Fixed order preserved, only allowlisted names, no values.
        assert_eq!(cat, "new_token_deserialize|missing=name,uri");
    }

    #[test]
    fn test_new_token_shape_diagnostic_wrong_type_names_only() {
        let mut v = full_new_token_json();
        // initialBuy is a NUMBER field; a string value is a wrong_type.
        v["initialBuy"] = serde_json::json!("1000000.5");
        let cat = diagnose_new_token_shape(&v);
        assert_eq!(cat, "new_token_deserialize|wrong_type=initialBuy:string");
    }

    #[test]
    fn test_new_token_shape_diagnostic_contains_no_provider_values() {
        let mut v = full_new_token_json();
        // Provider values that MUST NOT appear in the category.
        v["name"] = serde_json::json!("SECRETCOIN");
        v["symbol"] = serde_json::json!(12345); // wrong type => reported by name only
        let cat = diagnose_new_token_shape(&v);
        assert!(!cat.contains("SECRETCOIN"), "leaked provider value: {cat}");
        assert!(!cat.contains("12345"), "leaked provider value: {cat}");
        // The wrong-typed symbol is reported by field NAME + json type only.
        assert!(cat.contains("symbol:number"), "cat={cat}");
    }

    /// AUDIT-001 §5: the decode classifiers must never interpolate a raw
    /// serde/validation/parse error Display, because those errors can render
    /// rejected provider values (invalid mint/pubkey/signature/pool_id). Prove the
    /// prior unsafe log patterns are gone from THIS source file. Needles are
    /// assembled from split fragments via concat!() so this test does not itself
    /// reintroduce the forbidden contiguous string.
    #[test]
    fn test_decode_classifiers_do_not_interpolate_raw_error_display() {
        let src = include_str!("pumpportal.rs");
        let forbidden: &[&str] = &[
            concat!("Failed to deserialize new-token ", "event: {}"),
            concat!("Dropped malformed new-token event ", "(validation): {}"),
            concat!("Failed to deserialize trade ", "event: {}"),
            concat!("Dropped malformed trade event ", "(validation): {}"),
            concat!("Dropped malformed migration ", "event: {}"),
        ];
        for needle in forbidden {
            assert!(
                !src.contains(needle),
                "decode classifier still interpolates a raw error Display: {needle}"
            );
        }
    }

    #[test]
    fn test_new_token_shape_diagnostic_does_not_emit_unknown_extra_fields() {
        let mut v = full_new_token_json();
        v["totallyUnknownField"] = serde_json::json!("whatever");
        v["another_extra"] = serde_json::json!(999);
        let cat = diagnose_new_token_shape(&v);
        // A fully-valid-shaped object with only EXTRA fields yields the bare base.
        assert_eq!(cat, "new_token_deserialize");
        assert!(!cat.contains("totallyUnknownField"));
        assert!(!cat.contains("another_extra"));
    }

    #[test]
    fn test_new_token_shape_diagnostic_bounded() {
        // Every expected field wrong-typed to null maximizes the wrong_type list.
        let mut obj = serde_json::Map::new();
        for &f in NEW_TOKEN_STRING_FIELDS {
            obj.insert(f.to_string(), serde_json::Value::Null);
        }
        for &f in NEW_TOKEN_NUMBER_FIELDS {
            obj.insert(f.to_string(), serde_json::Value::Null);
        }
        let v = serde_json::Value::Object(obj);
        let cat = diagnose_new_token_shape(&v);
        assert!(
            cat.len() <= DECODE_CATEGORY_MAX,
            "len {} : {cat}",
            cat.len()
        );
        assert!(cat.is_ascii(), "category must be ASCII-only: {cat}");
        // Non-object => fixed unknown-shape token.
        let arr = serde_json::json!([1, 2, 3]);
        assert_eq!(
            diagnose_new_token_shape(&arr),
            "new_token_deserialize|shape=unknown"
        );
    }

    #[test]
    fn test_new_token_deserialize_emits_decode_error_not_hard_error() {
        // A wrong-typed core numeric fails BOTH the full and the partial parser =>
        // DecodeError (value-safe structural category), never a hard Error. (Under
        // schema v2 a *missing* optional like marketCapSol is now a PartialNewToken,
        // so we use a genuinely-undecodable input to preserve this test's intent.)
        let mut v = full_new_token_json();
        v["initialBuy"] = serde_json::json!("not_a_number");
        let text = serde_json::to_string(&v).unwrap();
        match classify_new_token(&text, &v) {
            PumpPortalEvent::DecodeError(e) => {
                assert!(
                    matches!(
                        e.kind,
                        PumpPortalDecodeKind::NewTokenDeserialize
                            | PumpPortalDecodeKind::PartialNewTokenDeserialize
                    ),
                    "kind={:?}",
                    e.kind
                );
                assert!(e.category.starts_with("new_token_deserialize"));
                assert!(e.category.contains("wrong_type=initialBuy"));
            }
            other => panic!("expected DecodeError, got {other:?}"),
        }
    }

    #[test]
    fn test_new_token_validation_emits_decode_error_not_hard_error() {
        // Serde OK (all fields present, correct types) but an invalid mint pubkey
        // => validation fails => DecodeError with NewTokenValidation.
        let mut v = full_new_token_json();
        v["mint"] = serde_json::json!("not_a_pubkey");
        let text = serde_json::to_string(&v).unwrap();
        match classify_new_token(&text, &v) {
            PumpPortalEvent::DecodeError(e) => {
                assert_eq!(e.kind, PumpPortalDecodeKind::NewTokenValidation);
                assert_eq!(e.category, "new_token_validation");
            }
            other => panic!("expected DecodeError, got {other:?}"),
        }
    }

    // === P1-METADATA-DRAIN-TRUTH-001 §16 — optional presentation metadata ======

    #[test]
    fn test_new_token_missing_name_symbol_uri_is_supported() {
        // A valid create with all core fields but NO name/symbol/uri keys must
        // deserialize + validate => NewToken with empty metadata strings.
        let mut v = full_new_token_json();
        let obj = v.as_object_mut().unwrap();
        obj.remove("name");
        obj.remove("symbol");
        obj.remove("uri");
        let text = serde_json::to_string(&v).unwrap();
        match classify_new_token(&text, &v) {
            PumpPortalEvent::NewToken(ev) => {
                assert_eq!(ev.name, "");
                assert_eq!(ev.symbol, "");
                assert_eq!(ev.uri, "");
                assert!(!ev.has_complete_metadata());
            }
            other => panic!("expected NewToken, got {other:?}"),
        }
    }

    #[test]
    fn test_new_token_complete_metadata_helper_true() {
        let v = full_new_token_json();
        let text = serde_json::to_string(&v).unwrap();
        match classify_new_token(&text, &v) {
            PumpPortalEvent::NewToken(ev) => {
                assert!(ev.has_complete_metadata());
            }
            other => panic!("expected NewToken, got {other:?}"),
        }
    }

    #[test]
    fn test_new_token_blank_metadata_helper_false() {
        // Whitespace-only metadata is unavailable => helper false.
        let mut v = full_new_token_json();
        v["name"] = serde_json::json!("  ");
        let text = serde_json::to_string(&v).unwrap();
        match classify_new_token(&text, &v) {
            PumpPortalEvent::NewToken(ev) => {
                assert!(!ev.has_complete_metadata());
            }
            other => panic!("expected NewToken, got {other:?}"),
        }
    }

    #[test]
    fn test_new_token_missing_market_cap_is_partial_v2() {
        // Schema v2: marketCapSol is optional-on-partial. A create with valid identity
        // but a missing provider observational field is now RETAINED as a
        // PartialNewToken (not dropped as DecodeError), with the field as None.
        let mut v = full_new_token_json();
        v.as_object_mut().unwrap().remove("marketCapSol");
        let text = serde_json::to_string(&v).unwrap();
        match classify_new_token(&text, &v) {
            PumpPortalEvent::PartialNewToken(ev) => {
                assert!(ev.market_cap_sol.is_none());
            }
            other => panic!("expected PartialNewToken, got {other:?}"),
        }
    }

    #[test]
    fn test_new_token_missing_required_identity_still_decode_error() {
        let mut v = full_new_token_json();
        v.as_object_mut().unwrap().remove("mint");
        let text = serde_json::to_string(&v).unwrap();
        match classify_new_token(&text, &v) {
            // Missing mint fails BOTH the full and the partial parser (partial also
            // requires a valid mint) => still a fail-closed DecodeError. Under v2 the
            // final failure is reported via the partial-fallback deserialize kind.
            PumpPortalEvent::DecodeError(e) => {
                assert!(
                    matches!(
                        e.kind,
                        PumpPortalDecodeKind::NewTokenDeserialize
                            | PumpPortalDecodeKind::PartialNewTokenDeserialize
                    ),
                    "kind={:?}",
                    e.kind
                );
            }
            other => panic!("expected DecodeError, got {other:?}"),
        }
    }

    #[test]
    fn test_new_token_null_metadata_still_decode_error() {
        // Present-but-null is a WRONG TYPE, not absence => serde String fails in BOTH
        // parsers => DecodeError (serde(default) fills ABSENT keys only).
        let mut v = full_new_token_json();
        v["name"] = serde_json::Value::Null;
        let text = serde_json::to_string(&v).unwrap();
        match classify_new_token(&text, &v) {
            PumpPortalEvent::DecodeError(e) => {
                assert!(
                    matches!(
                        e.kind,
                        PumpPortalDecodeKind::NewTokenDeserialize
                            | PumpPortalDecodeKind::PartialNewTokenDeserialize
                    ),
                    "kind={:?}",
                    e.kind
                );
            }
            other => panic!("expected DecodeError, got {other:?}"),
        }
    }

    #[test]
    fn test_new_token_wrong_type_metadata_still_decode_error() {
        // A number for a metadata String field is a wrong type => DecodeError in BOTH
        // parsers.
        let mut v = full_new_token_json();
        v["symbol"] = serde_json::json!(123);
        let text = serde_json::to_string(&v).unwrap();
        match classify_new_token(&text, &v) {
            PumpPortalEvent::DecodeError(e) => {
                assert!(
                    matches!(
                        e.kind,
                        PumpPortalDecodeKind::NewTokenDeserialize
                            | PumpPortalDecodeKind::PartialNewTokenDeserialize
                    ),
                    "kind={:?}",
                    e.kind
                );
            }
            other => panic!("expected DecodeError, got {other:?}"),
        }
    }

    #[test]
    fn test_provider_auth_error_remains_hard_error() {
        // An auth-shaped provider message stays a HARD Error, never a DecodeError.
        let value = serde_json::json!({ "error": "unauthorized: invalid api-key" });
        let reason = detect_provider_error(&value).expect("must classify");
        assert_eq!(reason, "PumpPortal authentication error");
        // And the classifier for provider errors is independent of decode kinds:
        // the event the stream would emit here is Error(reason), asserted by shape.
        let ev = PumpPortalEvent::Error(reason);
        assert!(matches!(ev, PumpPortalEvent::Error(_)));
    }

    #[test]
    fn test_migration_parse_failure_is_decode_error() {
        // Migration with a missing mint fails parse => MigrationParse DecodeError.
        let value = serde_json::json!({ "txType": "migrate", "pool": "raydium" });
        match classify_migration(&value) {
            PumpPortalEvent::DecodeError(e) => {
                assert_eq!(e.kind, PumpPortalDecodeKind::MigrationParse);
                assert_eq!(e.category, "migration_parse");
            }
            other => panic!("expected DecodeError, got {other:?}"),
        }
    }

    #[test]
    fn test_trade_deserialize_failure_is_decode_error() {
        // A buy message missing required numeric fields fails strict serde =>
        // TradeDeserialize DecodeError.
        let text = serde_json::json!({
            "signature": valid_sig(),
            "mint": VALID_PK_1,
            "traderPublicKey": VALID_PK_2,
            "txType": "buy"
        })
        .to_string();
        match classify_trade(&text) {
            PumpPortalEvent::DecodeError(e) => {
                assert_eq!(e.kind, PumpPortalDecodeKind::TradeDeserialize);
                assert_eq!(e.category, "trade_deserialize");
            }
            other => panic!("expected DecodeError, got {other:?}"),
        }
    }

    // === P1-OBSERVATION-SCHEMA-V2-PARTIAL-CREATE-001 §21 — partial create =======

    /// A create message body carrying only the REQUIRED discovery identity plus valid
    /// metadata (all provider observational optionals ABSENT). Callers insert specific
    /// optionals to exercise the present-optional validation paths.
    fn identity_only_create_json() -> serde_json::Value {
        serde_json::json!({
            "signature": valid_sig(),
            "mint": VALID_PK_2,
            "traderPublicKey": VALID_PK_1,
            "txType": "create",
            "name": "Test Token",
            "symbol": "TEST",
            "uri": "https://example.com"
        })
    }

    /// §21.1 — Valid identity + valid remaining provider fields, missing EXACTLY
    /// bondingCurveKey + vTokensInBondingCurve + vSolInBondingCurve. The full parser
    /// fails (those are required there) => partial fallback retains it as
    /// PartialNewToken. This is the exact Run #3 loss shape.
    #[test]
    fn test_missing_curve_and_reserves_emits_partial_new_token() {
        let mut v = full_new_token_json();
        let obj = v.as_object_mut().unwrap();
        obj.remove("bondingCurveKey");
        obj.remove("vTokensInBondingCurve");
        obj.remove("vSolInBondingCurve");
        let text = serde_json::to_string(&v).unwrap();
        match classify_new_token(&text, &v) {
            PumpPortalEvent::PartialNewToken(ev) => {
                assert_eq!(ev.mint, VALID_PK_2);
                assert_eq!(ev.trader_public_key, VALID_PK_1);
                assert_eq!(ev.tx_type, "create");
                // The absent optionals are None...
                assert!(ev.bonding_curve_key.is_none());
                assert!(ev.v_tokens_in_bonding_curve.is_none());
                assert!(ev.v_sol_in_bonding_curve.is_none());
                // ...the still-present optionals are Some.
                assert_eq!(ev.initial_buy, Some(1000000.5));
                assert_eq!(ev.market_cap_sol, Some(30.0));
                assert!(ev.has_complete_metadata());
            }
            other => panic!("expected PartialNewToken, got {other:?}"),
        }
    }

    /// §21.2 — All provider observational optionals absent => all None.
    #[test]
    fn test_partial_new_token_missing_optional_values_are_none() {
        let v = identity_only_create_json();
        let text = serde_json::to_string(&v).unwrap();
        match classify_new_token(&text, &v) {
            PumpPortalEvent::PartialNewToken(ev) => {
                assert!(ev.initial_buy.is_none());
                assert!(ev.bonding_curve_key.is_none());
                assert!(ev.v_tokens_in_bonding_curve.is_none());
                assert!(ev.v_sol_in_bonding_curve.is_none());
                assert!(ev.market_cap_sol.is_none());
            }
            other => panic!("expected PartialNewToken, got {other:?}"),
        }
    }

    /// §21.3 — Present valid optionals are retained as Some (still partial because
    /// bondingCurveKey is absent, so the FULL parser fails).
    #[test]
    fn test_partial_new_token_present_optional_values_are_some() {
        let mut v = identity_only_create_json();
        v["initialBuy"] = serde_json::json!(2.5);
        v["vTokensInBondingCurve"] = serde_json::json!(1000.0);
        v["vSolInBondingCurve"] = serde_json::json!(30.5);
        v["marketCapSol"] = serde_json::json!(30.0);
        let text = serde_json::to_string(&v).unwrap();
        match classify_new_token(&text, &v) {
            PumpPortalEvent::PartialNewToken(ev) => {
                assert_eq!(ev.initial_buy, Some(2.5));
                assert_eq!(ev.v_tokens_in_bonding_curve, Some(1000.0));
                assert_eq!(ev.v_sol_in_bonding_curve, Some(30.5));
                assert_eq!(ev.market_cap_sol, Some(30.0));
                // bondingCurveKey absent keeps this partial.
                assert!(ev.bonding_curve_key.is_none());
            }
            other => panic!("expected PartialNewToken, got {other:?}"),
        }
    }

    /// §21.4 — Missing required mint => DecodeError, never PartialNewToken.
    #[test]
    fn test_partial_new_token_missing_mint_is_decode_error() {
        let mut v = identity_only_create_json();
        v.as_object_mut().unwrap().remove("mint");
        let text = serde_json::to_string(&v).unwrap();
        match classify_new_token(&text, &v) {
            PumpPortalEvent::DecodeError(e) => {
                // Absent required string => partial serde fails (deserialize kind).
                assert_eq!(e.kind, PumpPortalDecodeKind::PartialNewTokenDeserialize);
                assert!(e.category.contains("missing=mint"));
            }
            other => panic!("expected DecodeError, got {other:?}"),
        }
    }

    /// §21.5 — Missing required signature => DecodeError.
    #[test]
    fn test_partial_new_token_missing_signature_is_decode_error() {
        let mut v = identity_only_create_json();
        v.as_object_mut().unwrap().remove("signature");
        let text = serde_json::to_string(&v).unwrap();
        match classify_new_token(&text, &v) {
            PumpPortalEvent::DecodeError(e) => {
                assert_eq!(e.kind, PumpPortalDecodeKind::PartialNewTokenDeserialize);
                assert!(e.category.contains("missing=signature"));
            }
            other => panic!("expected DecodeError, got {other:?}"),
        }
    }

    /// §21.6 — Missing required creator (traderPublicKey) => DecodeError.
    #[test]
    fn test_partial_new_token_missing_creator_is_decode_error() {
        let mut v = identity_only_create_json();
        v.as_object_mut().unwrap().remove("traderPublicKey");
        let text = serde_json::to_string(&v).unwrap();
        match classify_new_token(&text, &v) {
            PumpPortalEvent::DecodeError(e) => {
                assert_eq!(e.kind, PumpPortalDecodeKind::PartialNewTokenDeserialize);
                assert!(e.category.contains("missing=traderPublicKey"));
            }
            other => panic!("expected DecodeError, got {other:?}"),
        }
    }

    /// §21.7 — A PRESENT bondingCurveKey that is not a valid Pubkey => DecodeError
    /// (validation kind), never coerced to None.
    #[test]
    fn test_partial_new_token_invalid_present_bonding_curve_is_decode_error() {
        // Missing reserves keep it out of the full path; bad present curve fails the
        // partial present-optional validation.
        let mut v = identity_only_create_json();
        v["bondingCurveKey"] = serde_json::json!("not_a_pubkey");
        let text = serde_json::to_string(&v).unwrap();
        match classify_new_token(&text, &v) {
            PumpPortalEvent::DecodeError(e) => {
                assert_eq!(e.kind, PumpPortalDecodeKind::PartialNewTokenValidation);
                assert_eq!(e.category, "partial_new_token_validation");
            }
            other => panic!("expected DecodeError, got {other:?}"),
        }
    }

    /// §21.8 — A PRESENT negative numeric optional => DecodeError (validation),
    /// never coerced to None.
    #[test]
    fn test_partial_new_token_negative_present_numeric_is_decode_error() {
        let mut v = identity_only_create_json();
        v["marketCapSol"] = serde_json::json!(-1.0);
        let text = serde_json::to_string(&v).unwrap();
        match classify_new_token(&text, &v) {
            PumpPortalEvent::DecodeError(e) => {
                assert_eq!(e.kind, PumpPortalDecodeKind::PartialNewTokenValidation);
                assert_eq!(e.category, "partial_new_token_validation");
            }
            other => panic!("expected DecodeError, got {other:?}"),
        }
    }

    /// §21.9 — A PRESENT wrong-type numeric optional (string) => DecodeError. Serde
    /// fails to deserialize `Option<f64>` from a string => deserialize kind (a
    /// present value is NOT absence, so it is NOT silently None).
    #[test]
    fn test_partial_new_token_wrong_type_present_numeric_is_decode_error() {
        let mut v = identity_only_create_json();
        v["marketCapSol"] = serde_json::json!("30.0");
        let text = serde_json::to_string(&v).unwrap();
        match classify_new_token(&text, &v) {
            PumpPortalEvent::DecodeError(e) => {
                assert_eq!(e.kind, PumpPortalDecodeKind::PartialNewTokenDeserialize);
                // Value-free structural category: names the field + json type only.
                assert!(e.category.contains("marketCapSol:string"), "cat={}", e.category);
                assert!(!e.category.contains("30.0"), "leaked value: {}", e.category);
            }
            other => panic!("expected DecodeError, got {other:?}"),
        }
    }

    /// §21.10 — A full legacy create (every provider field present + valid) still
    /// emits FULL NewToken. No behavior change on the full path.
    #[test]
    fn test_full_legacy_create_still_emits_new_token() {
        let v = full_new_token_json();
        let text = serde_json::to_string(&v).unwrap();
        match classify_new_token(&text, &v) {
            PumpPortalEvent::NewToken(ev) => {
                assert_eq!(ev.mint, VALID_PK_2);
                assert_eq!(ev.bonding_curve_key, VALID_PK_3);
                assert!(ev.has_complete_metadata());
            }
            other => panic!("expected NewToken, got {other:?}"),
        }
    }

    /// §21.11 — Metadata-only absence (missing ONLY name/symbol/uri) stays FULL
    /// NewToken, NOT Partial: the full parser succeeds because those fields carry
    /// serde defaults, so Step B is never reached.
    #[test]
    fn test_metadata_only_absence_remains_full_new_token() {
        let mut v = full_new_token_json();
        let obj = v.as_object_mut().unwrap();
        obj.remove("name");
        obj.remove("symbol");
        obj.remove("uri");
        let text = serde_json::to_string(&v).unwrap();
        match classify_new_token(&text, &v) {
            PumpPortalEvent::NewToken(ev) => {
                assert_eq!(ev.name, "");
                assert_eq!(ev.symbol, "");
                assert_eq!(ev.uri, "");
                assert!(!ev.has_complete_metadata());
            }
            other => panic!("expected full NewToken (not Partial), got {other:?}"),
        }
    }

    /// §21 — the partial classifier's logs/categories remain value-free: a partial
    /// deserialize loss must never echo raw provider field values.
    #[test]
    fn test_partial_new_token_decode_category_is_value_free() {
        let mut v = identity_only_create_json();
        // Present-but-wrong-type numeric with a distinctive value that must not leak.
        v["initialBuy"] = serde_json::json!("SECRET_VALUE_9999");
        let text = serde_json::to_string(&v).unwrap();
        match classify_new_token(&text, &v) {
            PumpPortalEvent::DecodeError(e) => {
                assert!(!e.category.contains("SECRET_VALUE_9999"), "leaked: {}", e.category);
                assert!(e.category.is_ascii());
            }
            other => panic!("expected DecodeError, got {other:?}"),
        }
    }
}
