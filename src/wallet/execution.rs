//! Execution-wallet routing and on-chain ownership probing.
//!
//! Two concerns live here:
//!
//! 1. **Routing** — [`ExecutionWalletRegistry`] classifies a wallet as either a
//!    local-signing wallet or the single Lightning wallet, with local always
//!    taking precedence. There is NO fallback: an unknown wallet routes to
//!    nowhere.
//!
//! 2. **Ownership probing** — [`WalletOwnershipProbe`] aggregates the raw token
//!    balance a wallet holds for a given mint across ALL of its token accounts,
//!    proving a zero balance by corroborating against both the SPL Token and
//!    Token-2022 programs rather than trusting a single empty query.
//!
//! Every synchronous `RpcClient` call runs under `tokio::task::spawn_blocking`
//! so the async executor is never blocked. Parse failures are hard errors —
//! we never silently treat a malformed or failed lookup as a zero balance, and
//! we never assume decimals.

use std::str::FromStr;
use std::sync::Arc;

use serde_json::Value;

use solana_client::rpc_client::RpcClient;
use solana_client::rpc_request::TokenAccountsFilter;
use solana_sdk::pubkey::Pubkey;

use crate::error::{Error, Result};

/// SPL Token program id.
const SPL_TOKEN_PROGRAM_ID: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
/// SPL Token-2022 program id.
const TOKEN_2022_PROGRAM_ID: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";

/// How an owned wallet is signed / submitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionRoute {
    /// Locally-held keypair; we sign and submit directly.
    Local,
    /// The single delegated Lightning wallet.
    Lightning,
}

/// Immutable registry of the wallets this bot is allowed to execute through.
///
/// `local_wallets` always contains `primary_local` and is deduplicated. The
/// Lightning wallet is optional and is preferred to Local only when a wallet is
/// NOT already local (Local always wins on overlap).
pub struct ExecutionWalletRegistry {
    primary_local: Pubkey,
    local_wallets: Vec<Pubkey>,
    lightning_wallet: Option<Pubkey>,
}

impl ExecutionWalletRegistry {
    /// Build a registry. `primary_local` is always present in the local set;
    /// `additional_local` is merged in with duplicates removed.
    pub fn new(
        primary_local: Pubkey,
        additional_local: &[Pubkey],
        lightning: Option<Pubkey>,
    ) -> Self {
        let mut local_wallets: Vec<Pubkey> = Vec::with_capacity(additional_local.len() + 1);
        local_wallets.push(primary_local);
        for w in additional_local {
            if !local_wallets.contains(w) {
                local_wallets.push(*w);
            }
        }
        Self {
            primary_local,
            local_wallets,
            lightning_wallet: lightning,
        }
    }

    /// True if `wallet` is in the local-signing set.
    fn is_local(&self, wallet: &Pubkey) -> bool {
        self.local_wallets.contains(wallet)
    }

    /// Route for a wallet, or `None` if this registry does not own it.
    ///
    /// Local takes precedence: a wallet that is both local and the Lightning
    /// wallet routes `Local`. No fallback — unknown wallets return `None`.
    pub fn route_for(&self, wallet: &Pubkey) -> Option<ExecutionRoute> {
        if self.is_local(wallet) {
            return Some(ExecutionRoute::Local);
        }
        if self.lightning_wallet.as_ref() == Some(wallet) {
            return Some(ExecutionRoute::Lightning);
        }
        None
    }

    /// True if this registry can execute through `wallet`.
    pub fn owns(&self, wallet: &Pubkey) -> bool {
        self.route_for(wallet).is_some()
    }

    /// All owned wallets (locals plus Lightning if any), deduplicated.
    pub fn all_wallets(&self) -> Vec<Pubkey> {
        let mut out = self.local_wallets.clone();
        if let Some(lightning) = self.lightning_wallet {
            if !out.contains(&lightning) {
                out.push(lightning);
            }
        }
        out
    }

    /// The local-signing wallet set (always includes the primary).
    pub fn local_wallets(&self) -> &[Pubkey] {
        &self.local_wallets
    }

    /// The primary local wallet.
    pub fn primary_local(&self) -> Pubkey {
        self.primary_local
    }

    /// The Lightning wallet, if configured.
    pub fn lightning_wallet(&self) -> Option<Pubkey> {
        self.lightning_wallet
    }
}

/// Aggregated token balance a wallet holds for a single mint, summed across all
/// of that wallet's token accounts for the mint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalletTokenState {
    pub wallet: Pubkey,
    pub mint: Pubkey,
    /// Raw (base-unit) amount summed across every matching token account.
    pub raw_amount: u64,
    /// Token decimals, or `None` when the wallet holds no matching account.
    pub decimals: Option<u8>,
    /// Number of matching token accounts contributing to `raw_amount`.
    pub token_account_count: usize,
}

/// Result of scanning a registry for wallets that positively hold a mint.
///
/// Only strictly-positive balances count as holders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnedHolderResolution {
    /// No owned wallet holds a positive balance.
    None,
    /// Exactly one owned wallet holds a positive balance.
    Single(WalletTokenState),
    /// More than one owned wallet holds a positive balance (ambiguous).
    Multiple(Vec<WalletTokenState>),
}

/// Classify a list of wallet token states into a holder resolution.
///
/// Pure and network-free. Only strictly-positive `raw_amount` states are
/// treated as holders.
fn classify_holders(states: Vec<WalletTokenState>) -> OwnedHolderResolution {
    let mut holders: Vec<WalletTokenState> =
        states.into_iter().filter(|s| s.raw_amount > 0).collect();
    match holders.len() {
        0 => OwnedHolderResolution::None,
        1 => OwnedHolderResolution::Single(holders.pop().expect("len==1")),
        _ => OwnedHolderResolution::Multiple(holders),
    }
}

/// Aggregated raw balance parsed out of a set of token accounts.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedBalance {
    raw_amount: u64,
    decimals: Option<u8>,
    token_account_count: usize,
}

/// Parse a slice of JSON-parsed token accounts (as returned by
/// `get_token_accounts_by_owner` with the `jsonParsed` encoding) and aggregate
/// the raw balance for the exact `mint`.
///
/// Each element is expected to be the value at `account.data.parsed` — i.e. the
/// object containing `{ "info": { "mint": ..., "tokenAmount": { "amount": ...,
/// "decimals": ... } }, "type": "account" }`.
///
/// Rules (all hard errors on violation — never silently zero, never assume
/// decimals):
/// - `info.mint` must exactly equal `mint`; non-matching accounts are ignored.
/// - `info.tokenAmount.amount` parses as `u128` then must fit `u64`.
/// - `info.tokenAmount.decimals` parses as `u8`.
/// - All matching accounts must agree on decimals.
/// - The summed raw amount must not overflow `u64`.
fn parse_token_accounts(parsed_accounts: &[Value], mint: &Pubkey) -> Result<ParsedBalance> {
    let mint_str = mint.to_string();

    let mut raw_total: u64 = 0;
    let mut decimals: Option<u8> = None;
    let mut count: usize = 0;

    for parsed in parsed_accounts {
        let info = parsed.get("info").ok_or_else(|| {
            Error::Rpc("token account parsed data missing 'info' field".to_string())
        })?;

        let acct_mint = info.get("mint").and_then(Value::as_str).ok_or_else(|| {
            Error::Rpc("token account 'info.mint' missing or non-string".to_string())
        })?;

        if acct_mint != mint_str {
            // Different mint — the RPC Mint filter should exclude these, but we
            // enforce it exactly regardless.
            continue;
        }

        let token_amount = info
            .get("tokenAmount")
            .ok_or_else(|| Error::Rpc("token account 'info.tokenAmount' missing".to_string()))?;

        // amount is a decimal string of the raw base-unit balance.
        let amount_str = token_amount
            .get("amount")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                Error::Rpc("token account 'tokenAmount.amount' missing or non-string".to_string())
            })?;
        let amount_u128 = amount_str
            .parse::<u128>()
            .map_err(|e| Error::Rpc(format!("invalid token amount '{}': {}", amount_str, e)))?;
        if amount_u128 > u64::MAX as u128 {
            return Err(Error::Rpc(format!(
                "token amount '{}' exceeds u64::MAX",
                amount_str
            )));
        }
        let amount = amount_u128 as u64;

        // decimals must be an integer that fits u8.
        let decimals_u64 = token_amount
            .get("decimals")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                Error::Rpc(
                    "token account 'tokenAmount.decimals' missing or not an unsigned integer"
                        .to_string(),
                )
            })?;
        if decimals_u64 > u8::MAX as u64 {
            return Err(Error::Rpc(format!(
                "token decimals {} exceeds u8::MAX",
                decimals_u64
            )));
        }
        let dec = decimals_u64 as u8;

        match decimals {
            None => decimals = Some(dec),
            Some(existing) if existing != dec => {
                return Err(Error::Rpc(format!(
                    "inconsistent token decimals across accounts: {} vs {}",
                    existing, dec
                )));
            }
            Some(_) => {}
        }

        raw_total = raw_total
            .checked_add(amount)
            .ok_or_else(|| Error::Rpc("aggregate token balance overflows u64".to_string()))?;
        count += 1;
    }

    Ok(ParsedBalance {
        raw_amount: raw_total,
        decimals,
        token_account_count: count,
    })
}

/// Extract the `account.data.parsed` JSON `Value` out of a single
/// `RpcKeyedAccount` serialized to JSON.
///
/// In solana-client 2.0.25, `get_token_accounts_by_owner` returns
/// `Vec<RpcKeyedAccount>` where `account.data` is `UiAccountData::Json(...)`.
/// Rather than depend on the internal shape, we serialize each keyed account to
/// a `serde_json::Value` and reach into `account.data.parsed`, which is the same
/// object the pure [`parse_token_accounts`] helper consumes. Non-JSON data
/// (e.g. base64 encoding) is a hard error — we never assume an empty balance.
fn parsed_value_from_keyed_account(keyed: &Value) -> Result<Value> {
    let data = keyed
        .get("account")
        .and_then(|a| a.get("data"))
        .ok_or_else(|| Error::Rpc("token account missing 'account.data'".to_string()))?;

    let parsed = data.get("parsed").ok_or_else(|| {
        Error::Rpc(
            "token account data is not jsonParsed (missing 'parsed'); refusing to assume zero"
                .to_string(),
        )
    })?;

    Ok(parsed.clone())
}

/// Probes on-chain token ownership for the bot's execution wallets.
pub struct WalletOwnershipProbe {
    rpc: Arc<RpcClient>,
}

impl WalletOwnershipProbe {
    pub fn new(rpc: Arc<RpcClient>) -> Self {
        Self { rpc }
    }

    /// Fetch keyed token accounts for `wallet` under a given filter, run under
    /// `spawn_blocking`, and return each account's `account.data.parsed` value.
    async fn fetch_parsed_accounts(
        &self,
        wallet: Pubkey,
        filter: TokenAccountsFilter,
    ) -> Result<Vec<Value>> {
        let rpc = self.rpc.clone();
        let keyed_accounts =
            tokio::task::spawn_blocking(move || rpc.get_token_accounts_by_owner(&wallet, filter))
                .await
                .map_err(|e| Error::Rpc(format!("token accounts task join failed: {}", e)))??;

        // Serialize the strongly-typed keyed accounts to JSON and pull out the
        // parsed value from each, so the pure parser can consume them.
        let mut parsed = Vec::with_capacity(keyed_accounts.len());
        for keyed in &keyed_accounts {
            let keyed_value = serde_json::to_value(keyed).map_err(|e| {
                Error::Rpc(format!("failed to serialize keyed token account: {}", e))
            })?;
            parsed.push(parsed_value_from_keyed_account(&keyed_value)?);
        }
        Ok(parsed)
    }

    /// Aggregate the raw balance a wallet holds for a mint across all of its
    /// token accounts.
    ///
    /// Queries the mint-filtered token accounts first. When that shows no
    /// positive matching account, it corroborates a true-zero by querying both
    /// the SPL Token and Token-2022 programs — if EITHER corroborating query
    /// fails, this errors rather than reporting zero.
    pub async fn probe(&self, wallet: Pubkey, mint: Pubkey) -> Result<WalletTokenState> {
        let mint_filtered = self
            .fetch_parsed_accounts(wallet, TokenAccountsFilter::Mint(mint))
            .await?;
        let balance = parse_token_accounts(&mint_filtered, &mint)?;

        if balance.raw_amount > 0 {
            return Ok(WalletTokenState {
                wallet,
                mint,
                raw_amount: balance.raw_amount,
                decimals: balance.decimals,
                token_account_count: balance.token_account_count,
            });
        }

        // Zero (or no matching account) under the Mint filter. Prove it by
        // corroborating against both token programs. A failure here is NOT zero.
        let spl_program = Pubkey::from_str(SPL_TOKEN_PROGRAM_ID)
            .map_err(|e| Error::Internal(format!("invalid SPL Token program id: {}", e)))?;
        let token_2022_program = Pubkey::from_str(TOKEN_2022_PROGRAM_ID)
            .map_err(|e| Error::Internal(format!("invalid Token-2022 program id: {}", e)))?;

        let spl_accounts = self
            .fetch_parsed_accounts(wallet, TokenAccountsFilter::ProgramId(spl_program))
            .await?;
        let spl_balance = parse_token_accounts(&spl_accounts, &mint)?;

        let t22_accounts = self
            .fetch_parsed_accounts(wallet, TokenAccountsFilter::ProgramId(token_2022_program))
            .await?;
        let t22_balance = parse_token_accounts(&t22_accounts, &mint)?;

        // If corroboration surfaces a positive balance the Mint filter missed,
        // trust the higher (non-zero) reading and its decimals.
        if spl_balance.raw_amount > 0 || t22_balance.raw_amount > 0 {
            let raw_amount = spl_balance
                .raw_amount
                .checked_add(t22_balance.raw_amount)
                .ok_or_else(|| Error::Rpc("aggregate token balance overflows u64".to_string()))?;
            // Decimals should agree across programs for the same mint; prefer
            // whichever is present, erroring on genuine disagreement.
            let decimals = match (spl_balance.decimals, t22_balance.decimals) {
                (Some(a), Some(b)) if a != b => {
                    return Err(Error::Rpc(format!(
                        "inconsistent token decimals across programs: {} vs {}",
                        a, b
                    )))
                }
                (Some(a), _) => Some(a),
                (None, Some(b)) => Some(b),
                (None, None) => None,
            };
            return Ok(WalletTokenState {
                wallet,
                mint,
                raw_amount,
                decimals,
                token_account_count: spl_balance.token_account_count
                    + t22_balance.token_account_count,
            });
        }

        // Both corroborating queries succeeded and show no positive balance:
        // provably zero.
        Ok(WalletTokenState {
            wallet,
            mint,
            raw_amount: 0,
            decimals: None,
            token_account_count: 0,
        })
    }

    /// Probe every wallet in the registry for `mint` and classify which ones
    /// positively hold it.
    pub async fn find_positive_holders(
        &self,
        registry: &ExecutionWalletRegistry,
        mint: Pubkey,
    ) -> Result<OwnedHolderResolution> {
        let mut states = Vec::new();
        for wallet in registry.all_wallets() {
            states.push(self.probe(wallet, mint).await?);
        }
        Ok(classify_holders(states))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const WSOL: &str = "So11111111111111111111111111111111111111112";

    fn wsol_mint() -> Pubkey {
        Pubkey::from_str(WSOL).unwrap()
    }

    // --- Registry routing tests ---

    #[test]
    fn test_registry_primary_is_local() {
        let primary = Pubkey::new_unique();
        let reg = ExecutionWalletRegistry::new(primary, &[], None);
        assert_eq!(reg.route_for(&primary), Some(ExecutionRoute::Local));
        assert!(reg.owns(&primary));
    }

    #[test]
    fn test_registry_additional_local_is_local() {
        let primary = Pubkey::new_unique();
        let extra = Pubkey::new_unique();
        let reg = ExecutionWalletRegistry::new(primary, &[extra], None);
        assert_eq!(reg.route_for(&extra), Some(ExecutionRoute::Local));
        assert_eq!(reg.local_wallets().len(), 2);
    }

    #[test]
    fn test_registry_lightning_is_lightning() {
        let primary = Pubkey::new_unique();
        let lightning = Pubkey::new_unique();
        let reg = ExecutionWalletRegistry::new(primary, &[], Some(lightning));
        assert_eq!(reg.route_for(&lightning), Some(ExecutionRoute::Lightning));
        assert_eq!(reg.lightning_wallet(), Some(lightning));
        assert_eq!(reg.all_wallets().len(), 2);
    }

    #[test]
    fn test_registry_unknown_has_no_route() {
        let primary = Pubkey::new_unique();
        let unknown = Pubkey::new_unique();
        let reg = ExecutionWalletRegistry::new(primary, &[], Some(Pubkey::new_unique()));
        assert_eq!(reg.route_for(&unknown), None);
        assert!(!reg.owns(&unknown));
    }

    #[test]
    fn test_registry_local_precedence_when_same_as_lightning() {
        // A wallet that is both local and the Lightning wallet routes Local.
        let primary = Pubkey::new_unique();
        let reg = ExecutionWalletRegistry::new(primary, &[], Some(primary));
        assert_eq!(reg.route_for(&primary), Some(ExecutionRoute::Local));
        // all_wallets dedups the overlap.
        assert_eq!(reg.all_wallets().len(), 1);
    }

    // --- Pure parser tests (no network) ---

    /// Build a synthetic `account.data.parsed` value for a token account.
    fn parsed_account(mint: &str, amount: &str, decimals: u64) -> Value {
        json!({
            "type": "account",
            "info": {
                "mint": mint,
                "tokenAmount": {
                    "amount": amount,
                    "decimals": decimals,
                    "uiAmount": 0.0,
                    "uiAmountString": "0"
                }
            }
        })
    }

    #[test]
    fn test_parser_aggregates_multiple_token_accounts() {
        let mint = wsol_mint();
        let accounts = vec![
            parsed_account(WSOL, "30000000", 6),
            parsed_account(WSOL, "20000000", 6),
        ];
        let balance = parse_token_accounts(&accounts, &mint).unwrap();
        assert_eq!(balance.raw_amount, 50_000_000);
        assert_eq!(balance.decimals, Some(6));
        assert_eq!(balance.token_account_count, 2);
    }

    #[test]
    fn test_parser_ignores_other_mints() {
        let mint = wsol_mint();
        let other = Pubkey::new_unique().to_string();
        let accounts = vec![
            parsed_account(WSOL, "40000000", 6),
            parsed_account(&other, "99999999", 9),
        ];
        let balance = parse_token_accounts(&accounts, &mint).unwrap();
        assert_eq!(balance.raw_amount, 40_000_000);
        assert_eq!(balance.decimals, Some(6));
        assert_eq!(balance.token_account_count, 1);
    }

    #[test]
    fn test_parser_rejects_inconsistent_decimals() {
        let mint = wsol_mint();
        let accounts = vec![
            parsed_account(WSOL, "10000000", 6),
            parsed_account(WSOL, "10000000", 9),
        ];
        let err = parse_token_accounts(&accounts, &mint).unwrap_err();
        assert!(matches!(err, Error::Rpc(_)));
    }

    #[test]
    fn test_parser_rejects_invalid_raw_amount() {
        let mint = wsol_mint();
        let accounts = vec![parsed_account(WSOL, "not-a-number", 6)];
        let err = parse_token_accounts(&accounts, &mint).unwrap_err();
        assert!(matches!(err, Error::Rpc(_)));
    }

    // --- Holder classification tests (pure) ---

    fn state(raw: u64) -> WalletTokenState {
        WalletTokenState {
            wallet: Pubkey::new_unique(),
            mint: wsol_mint(),
            raw_amount: raw,
            decimals: if raw > 0 { Some(6) } else { None },
            token_account_count: if raw > 0 { 1 } else { 0 },
        }
    }

    #[test]
    fn test_holder_resolution_multiple_is_ambiguous() {
        let states = vec![state(10), state(0), state(25)];
        match classify_holders(states) {
            OwnedHolderResolution::Multiple(v) => assert_eq!(v.len(), 2),
            other => panic!("expected Multiple, got {:?}", other),
        }

        // Sanity: single positive => Single, all-zero => None.
        assert!(matches!(
            classify_holders(vec![state(5), state(0)]),
            OwnedHolderResolution::Single(_)
        ));
        assert!(matches!(
            classify_holders(vec![state(0), state(0)]),
            OwnedHolderResolution::None
        ));
    }
}
