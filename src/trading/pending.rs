//! Pending execution store.
//!
//! Durable, fail-closed record of transactions that have been SUBMITTED but not
//! yet reconciled. A signature here is submission identity, not fill proof
//! (INV-TX-001). The store carries only the intent that was requested at submit
//! time — it performs NO price estimation, NO P&L, and NO accounting. Economics
//! are derived later, exclusively by the reconciler from confirmed metadata.
//!
//! Persistence is fail-closed: a corrupt on-disk file is a hard error at load,
//! never a silent empty start, so a crashed process cannot "forget" in-flight
//! money.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::error::{Error, Result};
use crate::position::manager::EntryType;
use crate::trading::reconciliation::ReconciliationSide;

/// Buy intent captured at submission time.
///
/// Deliberately carries NO price or token-amount fields: those are only known
/// after the reconciler observes the confirmed transaction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingBuyContext {
    pub name: String,
    pub symbol: String,
    pub bonding_curve: String,
    pub entry_type: EntryType,
    pub requested_sol: f64,
}

/// Kind of pending sell being submitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingSellIntent {
    /// Full exit of the position.
    Full,
    /// Partial quick-profit skim.
    QuickProfit,
}

/// Sell intent captured at submission time.
///
/// `requested_amount` is a raw token-unit string (mirrors the on-wire amount);
/// it is intentionally not a numeric type so it round-trips exactly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingSellContext {
    pub requested_amount: String,
    pub intent: PendingSellIntent,
    pub reason: String,
}

/// Side-tagged context for a pending execution.
///
/// The tag matches the reconciliation side so context and side can never be
/// serialized into an inconsistent shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PendingExecutionContext {
    Buy(PendingBuyContext),
    Sell(PendingSellContext),
}

/// A submitted-but-unreconciled transaction.
///
/// Construct only via [`PendingExecution::buy`] / [`PendingExecution::sell`],
/// which guarantee that `side` and `context` agree. There is intentionally no
/// public field-literal constructor path that could mismatch them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingExecution {
    pub signature: String,
    pub mint: String,
    pub wallet: String,
    pub side: ReconciliationSide,
    pub submitted_at: DateTime<Utc>,
    pub context: PendingExecutionContext,
}

impl PendingExecution {
    /// Record a submitted BUY. `side` and `context` are fixed to Buy.
    pub fn buy(
        signature: String,
        mint: String,
        wallet: String,
        context: PendingBuyContext,
    ) -> Self {
        Self {
            signature,
            mint,
            wallet,
            side: ReconciliationSide::Buy,
            submitted_at: Utc::now(),
            context: PendingExecutionContext::Buy(context),
        }
    }

    /// Record a submitted SELL. `side` and `context` are fixed to Sell.
    pub fn sell(
        signature: String,
        mint: String,
        wallet: String,
        context: PendingSellContext,
    ) -> Self {
        Self {
            signature,
            mint,
            wallet,
            side: ReconciliationSide::Sell,
            submitted_at: Utc::now(),
            context: PendingExecutionContext::Sell(context),
        }
    }
}

/// Durable store of pending executions, keyed by transaction signature.
///
/// The store is a persistence boundary only: no economics, no P&L. Every
/// mutation is flushed to disk immediately so an unexpected exit leaves the
/// in-flight set on disk.
pub struct PendingExecutionStore {
    entries: Arc<RwLock<HashMap<String, PendingExecution>>>,
    persistence_path: String,
}

impl PendingExecutionStore {
    /// Create an empty store bound to `persistence_path`. Does not touch disk;
    /// call [`load`](Self::load) to hydrate.
    pub fn new(persistence_path: String) -> Self {
        Self {
            entries: Arc::new(RwLock::new(HashMap::new())),
            persistence_path,
        }
    }

    /// Hydrate from disk.
    ///
    /// - Missing file => `Ok(())` with an empty map (nothing was in flight).
    /// - File present but unreadable / invalid JSON => `Err` (FAIL CLOSED). We
    ///   never silently start empty when a record exists but cannot be parsed,
    ///   because that would drop in-flight money.
    pub async fn load(&self) -> Result<()> {
        if !std::path::Path::new(&self.persistence_path).exists() {
            let mut guard = self.entries.write().await;
            guard.clear();
            return Ok(());
        }

        let data = tokio::fs::read_to_string(&self.persistence_path)
            .await
            .map_err(|e| {
                Error::TransactionReconciliation(format!(
                    "failed to read pending execution store '{}': {}",
                    self.persistence_path, e
                ))
            })?;

        let parsed: HashMap<String, PendingExecution> =
            serde_json::from_str(&data).map_err(|e| {
                Error::TransactionReconciliation(format!(
                    "pending execution store '{}' is corrupt (invalid JSON): {}",
                    self.persistence_path, e
                ))
            })?;

        let mut guard = self.entries.write().await;
        *guard = parsed;
        Ok(())
    }

    /// Write the whole map to disk as pretty JSON. Caller holds no lock.
    async fn persist(&self) -> Result<()> {
        let data = {
            let guard = self.entries.read().await;
            serde_json::to_string_pretty(&*guard).map_err(|e| {
                Error::TransactionReconciliation(format!(
                    "failed to serialize pending execution store: {}",
                    e
                ))
            })?
        };

        tokio::fs::write(&self.persistence_path, data)
            .await
            .map_err(|e| {
                Error::TransactionReconciliation(format!(
                    "failed to write pending execution store '{}': {}",
                    self.persistence_path, e
                ))
            })
    }

    /// Insert or replace by signature, then persist immediately. Replacing an
    /// existing signature leaves the map length unchanged.
    pub async fn upsert(&self, execution: PendingExecution) -> Result<()> {
        {
            let mut guard = self.entries.write().await;
            guard.insert(execution.signature.clone(), execution);
        }
        self.persist().await
    }

    /// Remove by signature, then persist immediately.
    ///
    /// Returns `Ok(true)` if an entry was removed, `Ok(false)` if absent.
    pub async fn remove(&self, signature: &str) -> Result<bool> {
        let removed = {
            let mut guard = self.entries.write().await;
            guard.remove(signature).is_some()
        };
        self.persist().await?;
        Ok(removed)
    }

    /// Get a pending execution by signature.
    pub async fn get(&self, signature: &str) -> Option<PendingExecution> {
        let guard = self.entries.read().await;
        guard.get(signature).cloned()
    }

    /// First pending execution matching both `mint` and `side`.
    pub async fn get_for_mint(
        &self,
        mint: &str,
        side: ReconciliationSide,
    ) -> Option<PendingExecution> {
        let guard = self.entries.read().await;
        guard
            .values()
            .find(|e| e.mint == mint && e.side == side)
            .cloned()
    }

    /// Snapshot of all pending executions.
    pub async fn all(&self) -> Vec<PendingExecution> {
        let guard = self.entries.read().await;
        guard.values().cloned().collect()
    }

    /// Whether the store currently holds no pending executions.
    pub async fn is_empty(&self) -> bool {
        let guard = self.entries.read().await;
        guard.is_empty()
    }

    /// Number of pending executions currently held.
    pub async fn len(&self) -> usize {
        let guard = self.entries.read().await;
        guard.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(suffix: &str) -> String {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "pumpfun_pending_{}_{}.json",
            std::process::id(),
            suffix
        ));
        p.to_string_lossy().into_owned()
    }

    fn cleanup(path: &str) {
        let _ = std::fs::remove_file(path);
    }

    fn buy_ctx() -> PendingBuyContext {
        PendingBuyContext {
            name: "Test Token".to_string(),
            symbol: "TEST".to_string(),
            bonding_curve: "curve111".to_string(),
            entry_type: EntryType::StrongBuy,
            requested_sol: 0.05,
        }
    }

    fn sell_ctx() -> PendingSellContext {
        PendingSellContext {
            requested_amount: "50000000".to_string(),
            intent: PendingSellIntent::QuickProfit,
            reason: "quick profit skim".to_string(),
        }
    }

    #[tokio::test]
    async fn test_pending_store_round_trip_buy() {
        let path = temp_path("round_trip_buy");
        cleanup(&path);

        let exec = PendingExecution::buy(
            "sigbuy".to_string(),
            "mint1".to_string(),
            "wallet1".to_string(),
            buy_ctx(),
        );

        let store = PendingExecutionStore::new(path.clone());
        store.load().await.unwrap();
        store.upsert(exec.clone()).await.unwrap();

        let reloaded = PendingExecutionStore::new(path.clone());
        reloaded.load().await.unwrap();
        assert_eq!(reloaded.len().await, 1);
        let got = reloaded.get("sigbuy").await.unwrap();
        assert_eq!(got, exec);
        assert_eq!(got.side, ReconciliationSide::Buy);
        assert!(matches!(got.context, PendingExecutionContext::Buy(_)));

        cleanup(&path);
    }

    #[tokio::test]
    async fn test_pending_store_round_trip_sell() {
        let path = temp_path("round_trip_sell");
        cleanup(&path);

        let exec = PendingExecution::sell(
            "sigsell".to_string(),
            "mint2".to_string(),
            "wallet2".to_string(),
            sell_ctx(),
        );

        let store = PendingExecutionStore::new(path.clone());
        store.load().await.unwrap();
        store.upsert(exec.clone()).await.unwrap();

        let reloaded = PendingExecutionStore::new(path.clone());
        reloaded.load().await.unwrap();
        let got = reloaded.get("sigsell").await.unwrap();
        assert_eq!(got, exec);
        assert_eq!(got.side, ReconciliationSide::Sell);
        assert!(matches!(got.context, PendingExecutionContext::Sell(_)));

        cleanup(&path);
    }

    #[tokio::test]
    async fn test_pending_store_upsert_same_signature_is_idempotent() {
        let path = temp_path("upsert_idempotent");
        cleanup(&path);

        let store = PendingExecutionStore::new(path.clone());
        store.load().await.unwrap();

        let first = PendingExecution::buy(
            "dup".to_string(),
            "mintA".to_string(),
            "walletA".to_string(),
            buy_ctx(),
        );
        store.upsert(first).await.unwrap();
        assert_eq!(store.len().await, 1);

        let mut ctx = buy_ctx();
        ctx.requested_sol = 0.10;
        let second = PendingExecution::buy(
            "dup".to_string(),
            "mintB".to_string(),
            "walletB".to_string(),
            ctx,
        );
        store.upsert(second.clone()).await.unwrap();

        // Same signature => replace, not grow.
        assert_eq!(store.len().await, 1);
        let got = store.get("dup").await.unwrap();
        assert_eq!(got, second);
        assert_eq!(got.mint, "mintB");

        cleanup(&path);
    }

    #[tokio::test]
    async fn test_pending_store_remove_persists() {
        let path = temp_path("remove_persists");
        cleanup(&path);

        let store = PendingExecutionStore::new(path.clone());
        store.load().await.unwrap();
        store
            .upsert(PendingExecution::buy(
                "rm".to_string(),
                "mintR".to_string(),
                "walletR".to_string(),
                buy_ctx(),
            ))
            .await
            .unwrap();

        assert!(store.remove("rm").await.unwrap());
        assert!(!store.remove("rm").await.unwrap());
        assert!(store.is_empty().await);

        // Removal is durable: a fresh load sees nothing.
        let reloaded = PendingExecutionStore::new(path.clone());
        reloaded.load().await.unwrap();
        assert!(reloaded.is_empty().await);

        cleanup(&path);
    }

    #[tokio::test]
    async fn test_pending_store_invalid_json_fails_closed() {
        let path = temp_path("invalid_json");
        cleanup(&path);

        tokio::fs::write(&path, "not json{").await.unwrap();

        let store = PendingExecutionStore::new(path.clone());
        let result = store.load().await;
        assert!(result.is_err(), "corrupt store must fail closed");
        assert!(matches!(
            result.unwrap_err(),
            Error::TransactionReconciliation(_)
        ));

        cleanup(&path);
    }

    #[tokio::test]
    async fn test_get_for_mint_is_side_specific() {
        let path = temp_path("get_for_mint_side");
        cleanup(&path);

        let store = PendingExecutionStore::new(path.clone());
        store.load().await.unwrap();

        let mint = "sharedmint";
        store
            .upsert(PendingExecution::buy(
                "sigb".to_string(),
                mint.to_string(),
                "walletb".to_string(),
                buy_ctx(),
            ))
            .await
            .unwrap();
        store
            .upsert(PendingExecution::sell(
                "sigs".to_string(),
                mint.to_string(),
                "wallets".to_string(),
                sell_ctx(),
            ))
            .await
            .unwrap();

        let buy = store
            .get_for_mint(mint, ReconciliationSide::Buy)
            .await
            .unwrap();
        assert_eq!(buy.signature, "sigb");
        assert_eq!(buy.side, ReconciliationSide::Buy);

        let sell = store
            .get_for_mint(mint, ReconciliationSide::Sell)
            .await
            .unwrap();
        assert_eq!(sell.signature, "sigs");
        assert_eq!(sell.side, ReconciliationSide::Sell);

        assert!(store
            .get_for_mint("nope", ReconciliationSide::Buy)
            .await
            .is_none());

        cleanup(&path);
    }
}
