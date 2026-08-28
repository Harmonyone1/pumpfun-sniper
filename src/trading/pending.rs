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

    /// Structural integrity check for a single record (INV-JOURNAL-001).
    ///
    /// Enforced on every record read from disk at [`load`](PendingExecutionStore::load)
    /// so an on-disk record that could never have been produced by the
    /// constructors is a hard error, never a silently-tolerated shape:
    /// - identity fields (signature / mint / wallet) are non-empty;
    /// - `side` and `context` agree (Buy<->Buy, Sell<->Sell);
    /// - a Buy carries a finite, strictly-positive `requested_sol`;
    /// - a Sell carries a non-empty `requested_amount`.
    pub fn validate(&self) -> Result<()> {
        if self.signature.is_empty() {
            return Err(Error::TransactionReconciliation(
                "pending execution has empty signature".to_string(),
            ));
        }
        if self.mint.is_empty() {
            return Err(Error::TransactionReconciliation(format!(
                "pending execution '{}' has empty mint",
                self.signature
            )));
        }
        if self.wallet.is_empty() {
            return Err(Error::TransactionReconciliation(format!(
                "pending execution '{}' has empty wallet",
                self.signature
            )));
        }

        match (self.side, &self.context) {
            (ReconciliationSide::Buy, PendingExecutionContext::Buy(buy)) => {
                if !buy.requested_sol.is_finite() || buy.requested_sol <= 0.0 {
                    return Err(Error::TransactionReconciliation(format!(
                        "pending buy '{}' has non-positive or non-finite requested_sol: {}",
                        self.signature, buy.requested_sol
                    )));
                }
            }
            (ReconciliationSide::Sell, PendingExecutionContext::Sell(sell)) => {
                if sell.requested_amount.is_empty() {
                    return Err(Error::TransactionReconciliation(format!(
                        "pending sell '{}' has empty requested_amount",
                        self.signature
                    )));
                }
            }
            (side, _) => {
                return Err(Error::TransactionReconciliation(format!(
                    "pending execution '{}' side/context mismatch: side={:?} does not match context",
                    self.signature, side
                )));
            }
        }

        Ok(())
    }

    /// Whether two records share the same logical identity (INV-JOURNAL-003).
    ///
    /// Compares everything that defines *which* submission this is, but
    /// deliberately ignores `submitted_at`: re-recording the same submission
    /// (e.g. an idempotent retry of the journal write) must not be treated as a
    /// conflicting record just because a fresh timestamp was minted.
    fn same_logical_identity(&self, other: &PendingExecution) -> bool {
        self.signature == other.signature
            && self.mint == other.mint
            && self.wallet == other.wallet
            && self.side == other.side
            && self.context == other.context
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

    /// Path of the interrupted-write staging file: `<persistence_path>.tmp`.
    fn tmp_path(&self) -> String {
        format!("{}.tmp", self.persistence_path)
    }

    /// Validate a whole decoded map (INV-JOURNAL-001/002): every map key must
    /// equal its record's embedded signature, and every record must pass
    /// [`PendingExecution::validate`]. We never silently repair — a key/sig
    /// mismatch or a malformed record is a hard error, because either could mean
    /// an in-flight record was tampered with or truncated.
    fn validate_map(
        path: &str,
        map: &HashMap<String, PendingExecution>,
    ) -> Result<()> {
        for (map_key, execution) in map {
            if map_key != &execution.signature {
                return Err(Error::TransactionReconciliation(format!(
                    "pending execution store '{}' has key/signature mismatch: key '{}' != signature '{}'",
                    path, map_key, execution.signature
                )));
            }
            execution.validate()?;
        }
        Ok(())
    }

    /// Decode + validate a JSON journal blob into a map. Fail-closed on either
    /// invalid JSON or a violated invariant.
    fn decode_and_validate(
        path: &str,
        data: &str,
    ) -> Result<HashMap<String, PendingExecution>> {
        let parsed: HashMap<String, PendingExecution> =
            serde_json::from_str(data).map_err(|e| {
                Error::TransactionReconciliation(format!(
                    "pending execution store '{}' is corrupt (invalid JSON): {}",
                    path, e
                ))
            })?;
        Self::validate_map(path, &parsed)?;
        Ok(parsed)
    }

    /// Hydrate from disk.
    ///
    /// - Missing file AND no leftover temp => `Ok(())` with an empty map.
    /// - A leftover `<path>.tmp` from an interrupted write is NEVER silently
    ///   ignored (INV-JOURNAL-004): if it is valid, it is recovered/promoted to
    ///   the real path (a complete durable temp means the crash happened right
    ///   before the rename); if it is malformed, `load` fails closed rather than
    ///   discarding a candidate journal.
    /// - File present but unreadable / invalid JSON / invariant-violating =>
    ///   `Err` (FAIL CLOSED). We never silently start empty when a record exists
    ///   but cannot be parsed, because that would drop in-flight money.
    pub async fn load(&self) -> Result<()> {
        let tmp_path = self.tmp_path();

        // Handle a leftover interrupted-write temp first. A temp existing at all
        // means a prior persist crashed between "durable temp written" and
        // "renamed into place", so its contents are at least as fresh as the
        // real file. It must be accounted for explicitly, never ignored.
        if std::path::Path::new(&tmp_path).exists() {
            let tmp_data = tokio::fs::read_to_string(&tmp_path).await.map_err(|e| {
                Error::TransactionReconciliation(format!(
                    "failed to read interrupted-write temp '{}': {}",
                    tmp_path, e
                ))
            })?;

            // Fail closed if the temp is malformed — do not fall back to the
            // (possibly stale) main file and silently drop the temp candidate.
            let recovered = Self::decode_and_validate(&tmp_path, &tmp_data)?;

            // Temp is a complete, valid journal: promote it into place.
            Self::promote(&tmp_path, &self.persistence_path)?;

            let mut guard = self.entries.write().await;
            *guard = recovered;
            return Ok(());
        }

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

        let parsed = Self::decode_and_validate(&self.persistence_path, &data)?;

        let mut guard = self.entries.write().await;
        *guard = parsed;
        Ok(())
    }

    /// Atomically move `from` onto `to`, tolerating Windows semantics where
    /// `std::fs::rename` cannot replace an existing destination. On failure to
    /// rename directly, we remove the destination and retry — but we NEVER drop
    /// the source before a successful rename, so neither candidate journal is
    /// silently discarded. This is synchronous std::fs on purpose: the atomic
    /// replace primitive we need is on `std::fs`, and the file is tiny.
    fn promote(from: &str, to: &str) -> Result<()> {
        if std::fs::rename(from, to).is_ok() {
            return Ok(());
        }
        // Destination likely already exists (Windows). Remove it, then retry the
        // rename. The source is still intact at this point.
        if std::path::Path::new(to).exists() {
            std::fs::remove_file(to).map_err(|e| {
                Error::TransactionReconciliation(format!(
                    "failed to clear destination '{}' before promoting '{}': {}",
                    to, from, e
                ))
            })?;
        }
        std::fs::rename(from, to).map_err(|e| {
            Error::TransactionReconciliation(format!(
                "failed to promote '{}' to '{}': {}",
                from, to, e
            ))
        })
    }

    /// Write the whole map to disk as pretty JSON, crash-safe. Caller holds no
    /// lock.
    ///
    /// Interrupted-write safety: serialize, write to `<path>.tmp`, flush and
    /// fsync the temp file so a complete durable JSON exists on disk, then
    /// promote (rename) it over the real path. A crash before the rename leaves
    /// a valid `<path>.tmp` that [`load`](Self::load) recovers; a crash mid-write
    /// only ever corrupts the temp, never the live journal.
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

        let tmp_path = self.tmp_path();

        // Write + flush + fsync the temp so it is fully durable before promotion.
        {
            use tokio::io::AsyncWriteExt;
            let mut file = tokio::fs::File::create(&tmp_path).await.map_err(|e| {
                Error::TransactionReconciliation(format!(
                    "failed to create temp '{}' for pending execution store: {}",
                    tmp_path, e
                ))
            })?;
            file.write_all(data.as_bytes()).await.map_err(|e| {
                Error::TransactionReconciliation(format!(
                    "failed to write temp '{}' for pending execution store: {}",
                    tmp_path, e
                ))
            })?;
            file.flush().await.map_err(|e| {
                Error::TransactionReconciliation(format!(
                    "failed to flush temp '{}' for pending execution store: {}",
                    tmp_path, e
                ))
            })?;
            file.sync_all().await.map_err(|e| {
                Error::TransactionReconciliation(format!(
                    "failed to fsync temp '{}' for pending execution store: {}",
                    tmp_path, e
                ))
            })?;
        }

        Self::promote(&tmp_path, &self.persistence_path)
    }

    /// Materialize the current (possibly empty) journal on disk and prove it is
    /// writable BEFORE any trading happens (INV-JOURNAL-004 startup guard).
    ///
    /// Creates the parent directory of `persistence_path` if missing, then
    /// persists the current snapshot through the crash-safe path. An empty
    /// journal file therefore exists at startup, so a crash immediately after
    /// the first submission cannot be confused with "never had a journal".
    pub async fn ensure_writable(&self) -> Result<()> {
        if let Some(parent) = std::path::Path::new(&self.persistence_path).parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                tokio::fs::create_dir_all(parent).await.map_err(|e| {
                    Error::TransactionReconciliation(format!(
                        "failed to create parent dir '{}' for pending execution store: {}",
                        parent.display(),
                        e
                    ))
                })?;
            }
        }
        self.persist().await
    }

    /// Insert or idempotently confirm a pending execution, then persist
    /// (INV-JOURNAL-003).
    ///
    /// - Signature absent => insert and persist.
    /// - Signature present with the SAME logical identity (signature + mint +
    ///   wallet + side + context; `submitted_at` ignored) => idempotent: the
    ///   ORIGINAL stored record — including its original `submitted_at` — is kept
    ///   unchanged. This makes re-recording a retry a no-op on identity/timing.
    /// - Signature present with a DIFFERENT logical identity => `Err`, and the
    ///   stored record is left untouched. A signature is submission identity; two
    ///   different submissions must never share one, so we refuse to overwrite.
    pub async fn upsert(&self, execution: PendingExecution) -> Result<()> {
        {
            let mut guard = self.entries.write().await;
            match guard.get(&execution.signature) {
                None => {
                    guard.insert(execution.signature.clone(), execution);
                }
                Some(existing) => {
                    if existing.same_logical_identity(&execution) {
                        // Idempotent: preserve the original record (and its
                        // original submitted_at). Nothing to change in the map.
                    } else {
                        return Err(Error::TransactionReconciliation(format!(
                            "refusing to overwrite pending execution '{}': stored record has a different logical identity than the incoming one",
                            execution.signature
                        )));
                    }
                }
            }
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
        store.upsert(first.clone()).await.unwrap();
        assert_eq!(store.len().await, 1);

        // Re-recording the SAME logical submission (same signature + mint +
        // wallet + side + context) is idempotent and must not grow the map.
        let second = PendingExecution::buy(
            "dup".to_string(),
            "mintA".to_string(),
            "walletA".to_string(),
            buy_ctx(),
        );
        store.upsert(second).await.unwrap();

        // Same logical record => idempotent, not grow, original preserved.
        assert_eq!(store.len().await, 1);
        let got = store.get("dup").await.unwrap();
        assert_eq!(got, first);
        assert_eq!(got.mint, "mintA");

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

    #[tokio::test]
    async fn test_pending_store_rejects_conflicting_same_signature() {
        let path = temp_path("conflicting_same_sig");
        cleanup(&path);

        let store = PendingExecutionStore::new(path.clone());
        store.load().await.unwrap();

        store
            .upsert(PendingExecution::buy(
                "dupsig".to_string(),
                "mintA".to_string(),
                "walletA".to_string(),
                buy_ctx(),
            ))
            .await
            .unwrap();

        // Same signature, DIFFERENT mint => different logical identity => Err,
        // and the stored record must be left untouched.
        let conflict = PendingExecution::buy(
            "dupsig".to_string(),
            "mintB".to_string(),
            "walletA".to_string(),
            buy_ctx(),
        );
        let result = store.upsert(conflict).await;
        assert!(result.is_err(), "conflicting same-signature upsert must be rejected");
        assert!(matches!(
            result.unwrap_err(),
            Error::TransactionReconciliation(_)
        ));

        let got = store.get("dupsig").await.unwrap();
        assert_eq!(got.mint, "mintA", "stored record must not be overwritten");
        assert_eq!(store.len().await, 1);

        cleanup(&path);
    }

    #[tokio::test]
    async fn test_pending_store_same_logical_signature_preserves_original_submission_time() {
        let path = temp_path("preserve_submitted_at");
        cleanup(&path);

        let store = PendingExecutionStore::new(path.clone());
        store.load().await.unwrap();

        let mut first = PendingExecution::buy(
            "sameid".to_string(),
            "mintX".to_string(),
            "walletX".to_string(),
            buy_ctx(),
        );
        // Pin an explicit original submission time.
        first.submitted_at = Utc::now() - chrono::Duration::seconds(3600);
        let original_ts = first.submitted_at;
        store.upsert(first.clone()).await.unwrap();

        // Same logical record but with a LATER submitted_at.
        let mut second = first.clone();
        second.submitted_at = Utc::now();
        assert_ne!(second.submitted_at, original_ts);
        store.upsert(second).await.unwrap();

        // Reload from disk and confirm the original timestamp survived.
        let reloaded = PendingExecutionStore::new(path.clone());
        reloaded.load().await.unwrap();
        let got = reloaded.get("sameid").await.unwrap();
        assert_eq!(
            got.submitted_at, original_ts,
            "idempotent re-record must preserve the original submitted_at"
        );

        cleanup(&path);
    }

    #[tokio::test]
    async fn test_pending_store_rejects_map_key_signature_mismatch() {
        let path = temp_path("key_sig_mismatch");
        cleanup(&path);

        // Hand-write a map whose KEY differs from the embedded signature.
        let exec = PendingExecution::buy(
            "realsig".to_string(),
            "mintK".to_string(),
            "walletK".to_string(),
            buy_ctx(),
        );
        let mut map: HashMap<String, PendingExecution> = HashMap::new();
        map.insert("WRONGKEY".to_string(), exec);
        let data = serde_json::to_string_pretty(&map).unwrap();
        tokio::fs::write(&path, data).await.unwrap();

        let store = PendingExecutionStore::new(path.clone());
        let result = store.load().await;
        assert!(result.is_err(), "key/signature mismatch must fail closed");
        assert!(matches!(
            result.unwrap_err(),
            Error::TransactionReconciliation(_)
        ));

        cleanup(&path);
    }

    #[tokio::test]
    async fn test_pending_store_rejects_side_context_mismatch() {
        let path = temp_path("side_ctx_mismatch");
        cleanup(&path);

        // Hand-write JSON with side=buy but a SELL context. The map key matches
        // the signature, so only the side/context invariant can catch this.
        let data = r#"{
  "badsig": {
    "signature": "badsig",
    "mint": "mintS",
    "wallet": "walletS",
    "side": "buy",
    "submitted_at": "2026-01-01T00:00:00Z",
    "context": {
      "kind": "sell",
      "requested_amount": "12345",
      "intent": "full",
      "reason": "tampered"
    }
  }
}"#;
        tokio::fs::write(&path, data).await.unwrap();

        let store = PendingExecutionStore::new(path.clone());
        let result = store.load().await;
        assert!(result.is_err(), "side/context mismatch must fail closed");
        assert!(matches!(
            result.unwrap_err(),
            Error::TransactionReconciliation(_)
        ));

        cleanup(&path);
    }

    #[tokio::test]
    async fn test_ensure_writable_creates_parent_and_empty_store() {
        // Nested, non-existent parent directory under temp_dir.
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "pumpfun_pending_ensure_{}_{}",
            std::process::id(),
            "nested"
        ));
        let mut file = dir.clone();
        file.push("sub");
        file.push("pending.json");
        let path = file.to_string_lossy().into_owned();

        // Ensure a clean slate.
        let _ = std::fs::remove_dir_all(&dir);

        let store = PendingExecutionStore::new(path.clone());
        store.ensure_writable().await.unwrap();

        assert!(
            std::path::Path::new(&path).exists(),
            "ensure_writable must materialize the journal file"
        );

        // A fresh load sees an empty, valid journal.
        let reloaded = PendingExecutionStore::new(path.clone());
        reloaded.load().await.unwrap();
        assert!(reloaded.is_empty().await);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_interrupted_temp_file_fails_closed_or_recovers_latest_without_silent_loss() {
        // --- Case 1: a VALID leftover temp is recovered/promoted, not ignored.
        let path = temp_path("interrupted_valid");
        let tmp = format!("{}.tmp", path);
        cleanup(&path);
        let _ = std::fs::remove_file(&tmp);

        let exec = PendingExecution::buy(
            "tmpsig".to_string(),
            "mintT".to_string(),
            "walletT".to_string(),
            buy_ctx(),
        );
        let mut map: HashMap<String, PendingExecution> = HashMap::new();
        map.insert(exec.signature.clone(), exec);
        let good = serde_json::to_string_pretty(&map).unwrap();
        // Leftover temp exists; main file does NOT (crash right before rename).
        tokio::fs::write(&tmp, good).await.unwrap();

        let store = PendingExecutionStore::new(path.clone());
        store.load().await.unwrap();
        // Recovered from temp — not silently ignored.
        assert_eq!(store.len().await, 1);
        assert!(store.get("tmpsig").await.is_some());
        // Temp was promoted into place, so the real file now exists and the temp
        // is gone.
        assert!(std::path::Path::new(&path).exists());
        assert!(!std::path::Path::new(&tmp).exists());
        cleanup(&path);
        let _ = std::fs::remove_file(&tmp);

        // --- Case 2: a MALFORMED leftover temp fails closed (never ignored).
        let path2 = temp_path("interrupted_malformed");
        let tmp2 = format!("{}.tmp", path2);
        cleanup(&path2);
        let _ = std::fs::remove_file(&tmp2);

        // A valid, stale main file plus a corrupt temp: load must NOT silently
        // fall back to the main file and drop the temp — it must error.
        tokio::fs::write(&path2, "{}").await.unwrap();
        tokio::fs::write(&tmp2, "not json{").await.unwrap();

        let store2 = PendingExecutionStore::new(path2.clone());
        let result = store2.load().await;
        assert!(
            result.is_err(),
            "a malformed interrupted-write temp must fail closed, never be silently ignored"
        );
        assert!(matches!(
            result.unwrap_err(),
            Error::TransactionReconciliation(_)
        ));

        cleanup(&path2);
        let _ = std::fs::remove_file(&tmp2);
    }
}
