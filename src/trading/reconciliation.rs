//! Canonical Solana transaction reconciler.
//!
//! Observes an already-submitted transaction and classifies it as exactly one of
//! `ConfirmedFill`, `ConfirmedFailure`, or `Unresolved`, deriving fill economics
//! purely from confirmed transaction metadata (pre/post SOL + token balances,
//! decimals, fee). It performs NO price estimation, NO balance polling, NO Pump
//! instruction parsing, and NO position/execution side effects.
//!
//! A returned signature is submission identity, not fill proof (INV-TX-001).

use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use solana_client::{rpc_client::RpcClient, rpc_config::RpcTransactionConfig};
use solana_sdk::{commitment_config::CommitmentConfig, signature::Signature};
use solana_transaction_status::{
    option_serializer::OptionSerializer, EncodedConfirmedTransactionWithStatusMeta,
    EncodedTransaction, UiMessage, UiTransactionEncoding, UiTransactionTokenBalance,
};

use crate::error::{Error, Result};

/// Lamports in one SOL. (This is a native-SOL constant, NOT a token-decimal
/// conversion — token decimals always come from metadata, INV-TX-005.)
const LAMPORTS_PER_SOL: f64 = 1_000_000_000.0;

/// Trade side being reconciled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReconciliationSide {
    Buy,
    Sell,
}

/// Polling configuration for the reconciler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconcileConfig {
    pub poll_interval_ms: u64,
    pub timeout_ms: u64,
}

impl Default for ReconcileConfig {
    fn default() -> Self {
        Self {
            poll_interval_ms: 250,
            timeout_ms: 15_000,
        }
    }
}

impl ReconcileConfig {
    /// Effective poll interval; never zero at runtime (clamped to >= 1ms).
    fn poll_interval(&self) -> Duration {
        Duration::from_millis(self.poll_interval_ms.max(1))
    }
}

/// A confirmed fill derived from transaction metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReconciledFill {
    pub signature: String,
    pub slot: u64,
    pub block_time: Option<i64>,

    pub wallet: String,
    pub mint: String,
    pub side: ReconciliationSide,

    /// Signed raw token-unit change for this wallet+mint.
    /// Buy => positive. Sell => negative.
    pub token_delta_raw: i128,

    pub token_decimals: u8,

    /// Signed native-SOL wallet balance change: post_lamports - pre_lamports.
    pub wallet_sol_delta_lamports: i128,

    /// RPC transaction metadata fee. Informational; do NOT subtract twice.
    pub fee_lamports: u64,

    /// Time spent waiting inside the reconciler.
    pub reconciliation_wait_ms: u64,
}

impl ReconciledFill {
    /// Absolute raw token amount, or `None` if it does not fit in `u64`.
    pub fn token_amount_raw(&self) -> Option<u64> {
        let abs = self.token_delta_raw.unsigned_abs();
        if abs <= u64::MAX as u128 {
            Some(abs as u64)
        } else {
            None
        }
    }

    /// Absolute token amount in UI units: abs(raw) / 10^decimals.
    pub fn token_amount_ui(&self) -> f64 {
        let abs = self.token_delta_raw.unsigned_abs();
        if abs == 0 {
            return 0.0;
        }
        abs as f64 / 10_f64.powi(self.token_decimals as i32)
    }

    /// Signed wallet SOL delta in SOL.
    pub fn wallet_sol_delta_sol(&self) -> f64 {
        self.wallet_sol_delta_lamports as f64 / LAMPORTS_PER_SOL
    }

    /// Transaction fee in SOL (informational).
    pub fn fee_sol(&self) -> f64 {
        self.fee_lamports as f64 / LAMPORTS_PER_SOL
    }

    /// Native-SOL economic fill price (SOL per token). `None` if token UI amount
    /// is non-positive. Sell prices may be zero/negative in fee-dominated cases
    /// and are intentionally NOT clamped.
    pub fn effective_price_sol_per_token(&self) -> Option<f64> {
        let ui = self.token_amount_ui();
        if ui <= 0.0 {
            return None;
        }
        match self.side {
            ReconciliationSide::Buy => Some(self.wallet_sol_delta_sol().abs() / ui),
            ReconciliationSide::Sell => Some(self.wallet_sol_delta_sol() / ui),
        }
    }
}

/// The three economically distinct reconciliation outcomes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ReconciliationOutcome {
    ConfirmedFill(ReconciledFill),

    ConfirmedFailure {
        signature: String,
        error: String,
        observed_after_ms: u64,
    },

    Unresolved {
        signature: String,
        reason: String,
        observed_after_ms: u64,
    },
}

// ---------------------------------------------------------------------------
// Internal normalized snapshot types (provider-neutral, deterministic tests)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct TokenBalanceSnapshot {
    owner: String,
    mint: String,
    raw_amount: u128,
    decimals: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TransactionSnapshot {
    slot: u64,
    block_time: Option<i64>,
    account_keys: Vec<String>,

    fee_lamports: u64,
    pre_balances: Vec<u64>,
    post_balances: Vec<u64>,

    pre_token_balances: Vec<TokenBalanceSnapshot>,
    post_token_balances: Vec<TokenBalanceSnapshot>,
}

// ---------------------------------------------------------------------------
// Solana response extraction
// ---------------------------------------------------------------------------

/// Convert RPC UI token balances into normalized snapshots. Entries without an
/// explicit owner are omitted (owner is never invented from account index).
fn token_balances_from_ui(
    balances: &OptionSerializer<Vec<UiTransactionTokenBalance>>,
) -> Result<Vec<TokenBalanceSnapshot>> {
    let values = match balances {
        OptionSerializer::Some(values) => values,
        OptionSerializer::None | OptionSerializer::Skip => return Ok(Vec::new()),
    };

    let mut out = Vec::with_capacity(values.len());
    for b in values {
        let owner = match &b.owner {
            OptionSerializer::Some(owner) => owner.clone(),
            OptionSerializer::None | OptionSerializer::Skip => continue,
        };
        let raw_amount = b.ui_token_amount.amount.parse::<u128>().map_err(|e| {
            Error::TransactionReconciliation(format!(
                "invalid token amount '{}': {}",
                b.ui_token_amount.amount, e
            ))
        })?;
        out.push(TokenBalanceSnapshot {
            owner,
            mint: b.mint.clone(),
            raw_amount,
            decimals: b.ui_token_amount.decimals,
        });
    }
    Ok(out)
}

/// Build a normalized snapshot from a confirmed transaction. Structural problems
/// (missing metadata, non-JSON encoding, unparseable amounts) are hard errors.
fn snapshot_from_confirmed_transaction(
    tx: &EncodedConfirmedTransactionWithStatusMeta,
) -> Result<TransactionSnapshot> {
    let meta = tx.transaction.meta.as_ref().ok_or_else(|| {
        Error::TransactionReconciliation("confirmed transaction missing metadata".to_string())
    })?;

    let account_keys = match &tx.transaction.transaction {
        EncodedTransaction::Json(ui_transaction) => match &ui_transaction.message {
            UiMessage::Parsed(parsed) => parsed
                .account_keys
                .iter()
                .map(|account| account.pubkey.clone())
                .collect::<Vec<String>>(),
            UiMessage::Raw(raw) => raw.account_keys.clone(),
        },
        _ => {
            return Err(Error::TransactionReconciliation(
                "expected JSON/JSON-parsed transaction encoding".to_string(),
            ))
        }
    };

    let pre_token_balances = token_balances_from_ui(&meta.pre_token_balances)?;
    let post_token_balances = token_balances_from_ui(&meta.post_token_balances)?;

    Ok(TransactionSnapshot {
        slot: tx.slot,
        block_time: tx.block_time,
        account_keys,
        fee_lamports: meta.fee,
        pre_balances: meta.pre_balances.clone(),
        post_balances: meta.post_balances.clone(),
        pre_token_balances,
        post_token_balances,
    })
}

// ---------------------------------------------------------------------------
// Pure delta reconciliation
// ---------------------------------------------------------------------------

fn unresolved(signature: &str, reason: &str, wait_ms: u64) -> ReconciliationOutcome {
    ReconciliationOutcome::Unresolved {
        signature: signature.to_string(),
        reason: reason.to_string(),
        observed_after_ms: wait_ms,
    }
}

/// Reconcile a normalized snapshot into an outcome. No RPC. Deterministic.
fn reconcile_snapshot(
    signature: &str,
    wallet: &str,
    mint: &str,
    side: ReconciliationSide,
    snapshot: &TransactionSnapshot,
    reconciliation_wait_ms: u64,
) -> ReconciliationOutcome {
    // Exact wallet account index (no fee-payer/index-0 fallback).
    let widx = match snapshot.account_keys.iter().position(|k| k == wallet) {
        Some(i) => i,
        None => {
            return unresolved(
                signature,
                "execution wallet not present in confirmed transaction account keys",
                reconciliation_wait_ms,
            )
        }
    };

    if widx >= snapshot.pre_balances.len() || widx >= snapshot.post_balances.len() {
        return unresolved(
            signature,
            "wallet account index missing from transaction SOL balance metadata",
            reconciliation_wait_ms,
        );
    }

    let wallet_sol_delta_lamports =
        snapshot.post_balances[widx] as i128 - snapshot.pre_balances[widx] as i128;

    // Aggregate all wallet-owned balances for this mint (handles multiple ATAs,
    // ATA creation on buy, ATA closure on sell).
    let mut pre_raw: u128 = 0;
    let mut post_raw: u128 = 0;
    let mut decimals_seen: Vec<u8> = Vec::new();
    let mut matched = false;

    for b in &snapshot.pre_token_balances {
        if b.owner == wallet && b.mint == mint {
            matched = true;
            decimals_seen.push(b.decimals);
            pre_raw = match pre_raw.checked_add(b.raw_amount) {
                Some(v) => v,
                None => {
                    return unresolved(
                        signature,
                        "wallet-owned pre token balance overflow",
                        reconciliation_wait_ms,
                    )
                }
            };
        }
    }
    for b in &snapshot.post_token_balances {
        if b.owner == wallet && b.mint == mint {
            matched = true;
            decimals_seen.push(b.decimals);
            post_raw = match post_raw.checked_add(b.raw_amount) {
                Some(v) => v,
                None => {
                    return unresolved(
                        signature,
                        "wallet-owned post token balance overflow",
                        reconciliation_wait_ms,
                    )
                }
            };
        }
    }

    if !matched {
        return unresolved(
            signature,
            "no wallet-owned token balance metadata found for mint",
            reconciliation_wait_ms,
        );
    }

    let decimals = decimals_seen[0];
    if decimals_seen.iter().any(|d| *d != decimals) {
        return unresolved(
            signature,
            "inconsistent token decimals in transaction metadata",
            reconciliation_wait_ms,
        );
    }

    if pre_raw > i128::MAX as u128 || post_raw > i128::MAX as u128 {
        return unresolved(
            signature,
            "token amount exceeds i128 range",
            reconciliation_wait_ms,
        );
    }
    let token_delta_raw = post_raw as i128 - pre_raw as i128;

    match side {
        ReconciliationSide::Buy => {
            if token_delta_raw <= 0 {
                return unresolved(
                    signature,
                    "confirmed buy transaction produced no positive wallet token delta",
                    reconciliation_wait_ms,
                );
            }
            if wallet_sol_delta_lamports >= 0 {
                return unresolved(
                    signature,
                    "confirmed buy transaction produced no negative wallet SOL delta",
                    reconciliation_wait_ms,
                );
            }
        }
        ReconciliationSide::Sell => {
            if token_delta_raw >= 0 {
                return unresolved(
                    signature,
                    "confirmed sell transaction produced no negative wallet token delta",
                    reconciliation_wait_ms,
                );
            }
            // Sell intentionally does NOT require a positive SOL delta.
        }
    }

    ReconciliationOutcome::ConfirmedFill(ReconciledFill {
        signature: signature.to_string(),
        slot: snapshot.slot,
        block_time: snapshot.block_time,
        wallet: wallet.to_string(),
        mint: mint.to_string(),
        side,
        token_delta_raw,
        token_decimals: decimals,
        wallet_sol_delta_lamports,
        fee_lamports: snapshot.fee_lamports,
        reconciliation_wait_ms,
    })
}

// ---------------------------------------------------------------------------
// TradeReconciler (RPC observation only)
// ---------------------------------------------------------------------------

pub struct TradeReconciler {
    rpc: Arc<RpcClient>,
    config: ReconcileConfig,
}

impl TradeReconciler {
    pub fn new(rpc: Arc<RpcClient>) -> Self {
        Self {
            rpc,
            config: ReconcileConfig::default(),
        }
    }

    pub fn with_config(rpc: Arc<RpcClient>, config: ReconcileConfig) -> Self {
        Self { rpc, config }
    }

    /// Observe and classify an already-submitted transaction.
    pub async fn reconcile(
        &self,
        signature: &str,
        wallet: &str,
        mint: &str,
        side: ReconciliationSide,
    ) -> Result<ReconciliationOutcome> {
        let sig = Signature::from_str(signature).map_err(|e| {
            Error::TransactionReconciliation(format!("invalid transaction signature: {}", e))
        })?;

        let started = Instant::now();
        let timeout = Duration::from_millis(self.config.timeout_ms);
        let mut last_rpc_error: Option<String> = None;

        while started.elapsed() < timeout {
            // Step 1 — signature status (under spawn_blocking).
            let status_res = {
                let rpc = self.rpc.clone();
                match tokio::task::spawn_blocking(move || {
                    rpc.get_signature_status_with_commitment_and_history(
                        &sig,
                        CommitmentConfig::confirmed(),
                        true,
                    )
                })
                .await
                {
                    Ok(inner) => inner,
                    Err(e) => {
                        return Err(Error::TransactionReconciliation(format!(
                            "RPC reconciliation task join failed: {}",
                            e
                        )))
                    }
                }
            };

            match status_res {
                Ok(None) => {
                    // Not visible yet — keep polling.
                }
                Ok(Some(Err(tx_error))) => {
                    return Ok(ReconciliationOutcome::ConfirmedFailure {
                        signature: signature.to_string(),
                        error: format!("{:?}", tx_error),
                        observed_after_ms: started.elapsed().as_millis() as u64,
                    });
                }
                Ok(Some(Ok(()))) => {
                    // Step 2 — fetch transaction metadata (under spawn_blocking).
                    let fetch_res = {
                        let rpc = self.rpc.clone();
                        let cfg = RpcTransactionConfig {
                            encoding: Some(UiTransactionEncoding::JsonParsed),
                            commitment: Some(CommitmentConfig::confirmed()),
                            max_supported_transaction_version: Some(0),
                        };
                        match tokio::task::spawn_blocking(move || {
                            rpc.get_transaction_with_config(&sig, cfg)
                        })
                        .await
                        {
                            Ok(inner) => inner,
                            Err(e) => {
                                return Err(Error::TransactionReconciliation(format!(
                                    "RPC reconciliation task join failed: {}",
                                    e
                                )))
                            }
                        }
                    };

                    match fetch_res {
                        Ok(tx) => {
                            let observed_after_ms = started.elapsed().as_millis() as u64;

                            // Metadata-level error defense: even after Ok status,
                            // a meta error means the transaction failed.
                            if let Some(err) =
                                tx.transaction.meta.as_ref().and_then(|m| m.err.as_ref())
                            {
                                return Ok(ReconciliationOutcome::ConfirmedFailure {
                                    signature: signature.to_string(),
                                    error: format!("{:?}", err),
                                    observed_after_ms,
                                });
                            }

                            let snapshot = snapshot_from_confirmed_transaction(&tx)?;
                            return Ok(reconcile_snapshot(
                                signature,
                                wallet,
                                mint,
                                side,
                                &snapshot,
                                observed_after_ms,
                            ));
                        }
                        Err(e) => {
                            // Transient fetch failure — record and keep polling.
                            last_rpc_error = Some(e.to_string());
                        }
                    }
                }
                Err(e) => {
                    // Transient status RPC failure — record and keep polling.
                    last_rpc_error = Some(e.to_string());
                }
            }

            tokio::time::sleep(self.config.poll_interval()).await;
        }

        // Timeout is NOT failure proof.
        let observed_after_ms = started.elapsed().as_millis() as u64;
        let reason = match last_rpc_error {
            Some(err) => format!("reconciliation timed out; last RPC error: {}", err),
            None => {
                "transaction not confirmed with readable metadata before reconciliation timeout"
                    .to_string()
            }
        };
        Ok(ReconciliationOutcome::Unresolved {
            signature: signature.to_string(),
            reason,
            observed_after_ms,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_account_decoder::parse_token::UiTokenAmount;
    use solana_transaction_status::UiTransactionTokenBalance;

    const WALLET: &str = "Wallet111111111111111111111111111111111";
    const OTHER: &str = "Other1111111111111111111111111111111111";
    const MINT: &str = "Mint11111111111111111111111111111111111";
    const SIG: &str = "sig";

    fn tb(owner: &str, mint: &str, raw: u128, decimals: u8) -> TokenBalanceSnapshot {
        TokenBalanceSnapshot {
            owner: owner.to_string(),
            mint: mint.to_string(),
            raw_amount: raw,
            decimals,
        }
    }

    /// Snapshot with wallet at account index 1 (index 0 is a non-wallet key).
    fn snap(
        pre_sol: u64,
        post_sol: u64,
        fee: u64,
        pre_tokens: Vec<TokenBalanceSnapshot>,
        post_tokens: Vec<TokenBalanceSnapshot>,
    ) -> TransactionSnapshot {
        TransactionSnapshot {
            slot: 100,
            block_time: Some(1_700_000_000),
            account_keys: vec![OTHER.to_string(), WALLET.to_string()],
            fee_lamports: fee,
            pre_balances: vec![10, pre_sol],
            post_balances: vec![10, post_sol],
            pre_token_balances: pre_tokens,
            post_token_balances: post_tokens,
        }
    }

    fn fill(outcome: ReconciliationOutcome) -> ReconciledFill {
        match outcome {
            ReconciliationOutcome::ConfirmedFill(f) => f,
            other => panic!("expected ConfirmedFill, got {:?}", other),
        }
    }

    fn assert_unresolved(outcome: &ReconciliationOutcome) {
        assert!(
            matches!(outcome, ReconciliationOutcome::Unresolved { .. }),
            "expected Unresolved, got {:?}",
            outcome
        );
    }

    #[test]
    fn test_reconcile_buy_uses_exact_wallet_and_mint_deltas() {
        let s = snap(
            2_000_000_000,
            1_880_000_000,
            5_000,
            vec![tb(WALLET, MINT, 0, 6)],
            vec![tb(WALLET, MINT, 50_000_000, 6)],
        );
        let f = fill(reconcile_snapshot(
            SIG,
            WALLET,
            MINT,
            ReconciliationSide::Buy,
            &s,
            0,
        ));
        assert_eq!(f.token_delta_raw, 50_000_000);
        assert_eq!(f.token_decimals, 6);
        assert_eq!(f.wallet_sol_delta_lamports, -120_000_000);
        assert_eq!(f.fee_lamports, 5_000);
        assert!((f.token_amount_ui() - 50.0).abs() < 1e-9);
        assert!((f.wallet_sol_delta_sol() - (-0.12)).abs() < 1e-9);
        assert!((f.effective_price_sol_per_token().unwrap() - 0.0024).abs() < 1e-9);
    }

    #[test]
    fn test_reconcile_sell_uses_exact_negative_token_delta() {
        let s = snap(
            1_000_000_000,
            1_095_000_000,
            5_000,
            vec![tb(WALLET, MINT, 50_000_000, 6)],
            vec![tb(WALLET, MINT, 20_000_000, 6)],
        );
        let f = fill(reconcile_snapshot(
            SIG,
            WALLET,
            MINT,
            ReconciliationSide::Sell,
            &s,
            0,
        ));
        assert_eq!(f.token_delta_raw, -30_000_000);
        assert!((f.token_amount_ui() - 30.0).abs() < 1e-9);
        assert!((f.wallet_sol_delta_sol() - 0.095).abs() < 1e-9);
    }

    #[test]
    fn test_confirmed_sell_allows_nonpositive_net_sol_delta() {
        // Sell reduces tokens but nets -5_000 lamports (fee-dominated).
        let s = snap(
            1_000_000_000,
            999_995_000,
            5_000,
            vec![tb(WALLET, MINT, 10_000, 6)],
            vec![tb(WALLET, MINT, 0, 6)],
        );
        let outcome = reconcile_snapshot(SIG, WALLET, MINT, ReconciliationSide::Sell, &s, 0);
        let f = fill(outcome);
        assert_eq!(f.token_delta_raw, -10_000);
        assert_eq!(f.wallet_sol_delta_lamports, -5_000);
    }

    #[test]
    fn test_missing_execution_wallet_is_unresolved() {
        let mut s = snap(
            2_000_000_000,
            1_880_000_000,
            5_000,
            vec![tb(WALLET, MINT, 0, 6)],
            vec![tb(WALLET, MINT, 50_000_000, 6)],
        );
        // Remove the wallet from account keys.
        s.account_keys = vec![OTHER.to_string()];
        s.pre_balances = vec![10];
        s.post_balances = vec![10];
        assert_unresolved(&reconcile_snapshot(
            SIG,
            WALLET,
            MINT,
            ReconciliationSide::Buy,
            &s,
            0,
        ));
    }

    #[test]
    fn test_other_wallet_token_balance_is_not_counted() {
        // OTHER gains tokens; execution wallet has no matching mint delta.
        let s = snap(
            2_000_000_000,
            1_880_000_000,
            5_000,
            vec![tb(OTHER, MINT, 0, 6)],
            vec![tb(OTHER, MINT, 50_000_000, 6)],
        );
        assert_unresolved(&reconcile_snapshot(
            SIG,
            WALLET,
            MINT,
            ReconciliationSide::Buy,
            &s,
            0,
        ));
    }

    #[test]
    fn test_other_mint_token_balance_is_not_counted() {
        let s = snap(
            2_000_000_000,
            1_880_000_000,
            5_000,
            vec![tb(WALLET, "OtherMint", 0, 6)],
            vec![tb(WALLET, "OtherMint", 50_000_000, 6)],
        );
        assert_unresolved(&reconcile_snapshot(
            SIG,
            WALLET,
            MINT,
            ReconciliationSide::Buy,
            &s,
            0,
        ));
    }

    #[test]
    fn test_multiple_wallet_owned_token_accounts_are_aggregated() {
        let s = snap(
            2_000_000_000,
            1_880_000_000,
            5_000,
            vec![tb(WALLET, MINT, 0, 6), tb(WALLET, MINT, 0, 6)],
            vec![
                tb(WALLET, MINT, 30_000_000, 6),
                tb(WALLET, MINT, 20_000_000, 6),
            ],
        );
        let f = fill(reconcile_snapshot(
            SIG,
            WALLET,
            MINT,
            ReconciliationSide::Buy,
            &s,
            0,
        ));
        assert_eq!(f.token_delta_raw, 50_000_000);
    }

    #[test]
    fn test_buy_with_no_pre_token_account_uses_zero_prebalance() {
        // ATA created on buy: no matching pre balance.
        let s = snap(
            2_000_000_000,
            1_880_000_000,
            5_000,
            vec![],
            vec![tb(WALLET, MINT, 50_000_000, 6)],
        );
        let f = fill(reconcile_snapshot(
            SIG,
            WALLET,
            MINT,
            ReconciliationSide::Buy,
            &s,
            0,
        ));
        assert_eq!(f.token_delta_raw, 50_000_000);
    }

    #[test]
    fn test_sell_with_no_post_token_account_uses_zero_postbalance() {
        // ATA closed on sell: no matching post balance.
        let s = snap(
            1_000_000_000,
            1_090_000_000,
            5_000,
            vec![tb(WALLET, MINT, 40_000_000, 6)],
            vec![],
        );
        let f = fill(reconcile_snapshot(
            SIG,
            WALLET,
            MINT,
            ReconciliationSide::Sell,
            &s,
            0,
        ));
        assert_eq!(f.token_delta_raw, -40_000_000);
    }

    #[test]
    fn test_inconsistent_decimals_is_unresolved() {
        let s = snap(
            2_000_000_000,
            1_880_000_000,
            5_000,
            vec![tb(WALLET, MINT, 0, 6)],
            vec![tb(WALLET, MINT, 50_000_000, 9)],
        );
        assert_unresolved(&reconcile_snapshot(
            SIG,
            WALLET,
            MINT,
            ReconciliationSide::Buy,
            &s,
            0,
        ));
    }

    #[test]
    fn test_buy_without_positive_token_delta_is_unresolved() {
        let s = snap(
            2_000_000_000,
            1_880_000_000,
            5_000,
            vec![tb(WALLET, MINT, 50_000_000, 6)],
            vec![tb(WALLET, MINT, 50_000_000, 6)],
        );
        assert_unresolved(&reconcile_snapshot(
            SIG,
            WALLET,
            MINT,
            ReconciliationSide::Buy,
            &s,
            0,
        ));
    }

    #[test]
    fn test_buy_without_negative_sol_delta_is_unresolved() {
        // Token gained, but wallet SOL did not decrease.
        let s = snap(
            2_000_000_000,
            2_000_000_000,
            5_000,
            vec![tb(WALLET, MINT, 0, 6)],
            vec![tb(WALLET, MINT, 50_000_000, 6)],
        );
        assert_unresolved(&reconcile_snapshot(
            SIG,
            WALLET,
            MINT,
            ReconciliationSide::Buy,
            &s,
            0,
        ));
    }

    #[test]
    fn test_sell_without_negative_token_delta_is_unresolved() {
        let s = snap(
            1_000_000_000,
            1_090_000_000,
            5_000,
            vec![tb(WALLET, MINT, 10_000_000, 6)],
            vec![tb(WALLET, MINT, 10_000_000, 6)],
        );
        assert_unresolved(&reconcile_snapshot(
            SIG,
            WALLET,
            MINT,
            ReconciliationSide::Sell,
            &s,
            0,
        ));
    }

    #[test]
    fn test_missing_wallet_balance_index_is_unresolved() {
        // Wallet key present at index 1, but balance arrays only have index 0.
        let mut s = snap(
            2_000_000_000,
            1_880_000_000,
            5_000,
            vec![tb(WALLET, MINT, 0, 6)],
            vec![tb(WALLET, MINT, 50_000_000, 6)],
        );
        s.pre_balances = vec![10];
        s.post_balances = vec![10];
        assert_unresolved(&reconcile_snapshot(
            SIG,
            WALLET,
            MINT,
            ReconciliationSide::Buy,
            &s,
            0,
        ));
    }

    #[test]
    fn test_effective_price_uses_token_decimals() {
        let s = snap(
            2_000_000_000,
            1_997_000_000, // -0.003 SOL
            5_000,
            vec![tb(WALLET, MINT, 0, 6)],
            vec![tb(WALLET, MINT, 1_500_000, 6)], // 1.5 tokens
        );
        let f = fill(reconcile_snapshot(
            SIG,
            WALLET,
            MINT,
            ReconciliationSide::Buy,
            &s,
            0,
        ));
        assert!((f.token_amount_ui() - 1.5).abs() < 1e-9);
        // 0.003 / 1.5 = 0.002 (NOT 0.003 / 1_500_000)
        assert!((f.effective_price_sol_per_token().unwrap() - 0.002).abs() < 1e-9);
    }

    #[test]
    fn test_fee_is_not_double_subtracted_from_wallet_delta() {
        let s = snap(
            2_000_000_000,
            1_880_000_000, // -0.12 SOL, already includes fee
            5_000,
            vec![tb(WALLET, MINT, 0, 6)],
            vec![tb(WALLET, MINT, 50_000_000, 6)],
        );
        let f = fill(reconcile_snapshot(
            SIG,
            WALLET,
            MINT,
            ReconciliationSide::Buy,
            &s,
            0,
        ));
        // Effective price uses absolute wallet delta (0.12) directly, NOT 0.12+fee.
        assert!((f.effective_price_sol_per_token().unwrap() - 0.0024).abs() < 1e-9);
        assert_eq!(f.fee_lamports, 5_000);
    }

    // --- Solana UI adapter tests ---

    fn ui_tb(
        owner: OptionSerializer<String>,
        amount: &str,
        decimals: u8,
    ) -> UiTransactionTokenBalance {
        UiTransactionTokenBalance {
            account_index: 0,
            mint: MINT.to_string(),
            ui_token_amount: UiTokenAmount {
                ui_amount: Some(0.0),
                decimals,
                amount: amount.to_string(),
                ui_amount_string: "0".to_string(),
            },
            owner,
            program_id: OptionSerializer::Skip,
        }
    }

    #[test]
    fn test_ui_token_balance_owner_some_is_parsed() {
        let balances = OptionSerializer::Some(vec![ui_tb(
            OptionSerializer::Some(WALLET.to_string()),
            "50000000",
            6,
        )]);
        let out = token_balances_from_ui(&balances).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].owner, WALLET);
        assert_eq!(out[0].mint, MINT);
        assert_eq!(out[0].raw_amount, 50_000_000);
        assert_eq!(out[0].decimals, 6);
    }

    #[test]
    fn test_ui_token_balance_missing_owner_is_ignored() {
        let balances = OptionSerializer::Some(vec![
            ui_tb(OptionSerializer::None, "1", 6),
            ui_tb(OptionSerializer::Skip, "2", 6),
        ]);
        let out = token_balances_from_ui(&balances).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn test_ui_token_balance_invalid_raw_amount_is_error() {
        let balances = OptionSerializer::Some(vec![ui_tb(
            OptionSerializer::Some(WALLET.to_string()),
            "not-a-number",
            6,
        )]);
        let err = token_balances_from_ui(&balances).unwrap_err();
        assert!(matches!(err, Error::TransactionReconciliation(_)));
    }
}
