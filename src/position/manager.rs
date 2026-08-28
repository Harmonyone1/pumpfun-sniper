//! Position management
//!
//! Tracks open positions and provides P&L calculation.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info};

use crate::config::SafetyConfig;
use crate::error::{Error, Result};

/// Entry recommendation that led to opening this position
/// Used for context-aware auto-sell strategies
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryType {
    /// High conviction entry (score >= 0.65)
    StrongBuy,
    /// Standard opportunity (score >= 0.35)
    Opportunity,
    /// Probe/learning position (score 0.15-0.35)
    Probe,
    /// Legacy entry (before entry type tracking)
    Legacy,
}

impl Default for EntryType {
    fn default() -> Self {
        EntryType::Legacy
    }
}

impl EntryType {
    /// Map wallet category to entry type
    /// Elite wallets get StrongBuy (tighter stops, higher conviction)
    /// Unknown/Neutral wallets get Opportunity
    /// Avoid wallets get Probe (quick scalps only)
    pub fn from_wallet_category(category: crate::filter::smart_money::WalletCategory) -> Self {
        use crate::filter::smart_money::WalletCategory;
        match category {
            WalletCategory::TrueSignal => EntryType::StrongBuy,
            WalletCategory::Profitable => EntryType::Opportunity,
            WalletCategory::Neutral | WalletCategory::Unknown => EntryType::Opportunity,
            WalletCategory::Unprofitable | WalletCategory::BundledTeam | WalletCategory::MevBot => {
                EntryType::Probe
            }
        }
    }

    /// Get adjusted stop loss for elite wallet entries
    /// Elite wallets tend to re-enter quickly, so use tighter stops
    pub fn stop_loss_pct_for_elite(&self, is_elite: bool) -> f64 {
        if is_elite {
            // Tighter stops for elite entries - they'll re-enter if needed
            match self {
                EntryType::StrongBuy => 10.0,  // Was 15% - now 10% for elite
                EntryType::Opportunity => 12.0, // Was 15% - now 12%
                EntryType::Probe => 8.0,        // Was 12% - now 8%
                EntryType::Legacy => 12.0,
            }
        } else {
            self.stop_loss_pct()
        }
    }

    /// Get the take profit target for this entry type
    /// DATA-DRIVEN: Lowered for realistic 2-minute holds
    pub fn take_profit_pct(&self) -> f64 {
        match self {
            EntryType::StrongBuy => 15.0,   // Was 100% - now 15% realistic
            EntryType::Opportunity => 10.0, // Was 50% - now 10% for quick profit
            EntryType::Probe => 8.0,        // Was 25% - now 8% quick scalp
            EntryType::Legacy => 10.0,      // Default
        }
    }

    /// Get the QUICK profit level - exit 50% of position at this level
    /// This secures profits early before potential dump
    pub fn quick_profit_pct(&self) -> f64 {
        match self {
            EntryType::StrongBuy => 8.0,   // Take 50% off at 8% profit
            EntryType::Opportunity => 5.0, // Take 50% off at 5% profit
            EntryType::Probe => 4.0,       // Take 50% off at 4% profit (very quick)
            EntryType::Legacy => 5.0,      // Default
        }
    }

    /// Get the stop loss threshold for this entry type
    /// WIDENED: Give trades more room to breathe
    pub fn stop_loss_pct(&self) -> f64 {
        match self {
            EntryType::StrongBuy => 15.0,   // Widened from 10% to 15%
            EntryType::Opportunity => 15.0, // Widened from 12% to 15%
            EntryType::Probe => 12.0,       // Widened from 10% to 12%
            EntryType::Legacy => 15.0,      // Widened from 12% to 15%
        }
    }

    /// Get the max hold time in seconds for this entry type
    /// DISABLED: Time-based exits were causing exits right before price spikes.
    /// Let the trailing stop and take-profit do their job instead.
    pub fn max_hold_secs(&self) -> Option<u64> {
        // Return None for all entry types - rely on trailing stop and TP/SL instead
        None
    }

    /// Should use tiered exit strategy?
    pub fn use_tiered_exit(&self) -> bool {
        matches!(self, EntryType::StrongBuy)
    }
}

/// A single position in a token
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    /// Token mint address
    pub mint: String,
    /// Token name
    pub name: String,
    /// Token symbol
    pub symbol: String,
    /// Bonding curve address
    pub bonding_curve: String,
    /// Amount of tokens held.
    /// For reconciled positions this is RAW token units; legacy positions may differ.
    pub token_amount: u64,
    /// Token decimals from confirmed transaction metadata.
    /// `None` means legacy/unmigrated position whose unit semantics are not yet canonical.
    #[serde(default)]
    pub token_decimals: Option<u8>,
    /// Entry price in SOL per token
    pub entry_price: f64,
    /// Total SOL cost (including fees)
    pub total_cost_sol: f64,
    /// Entry timestamp
    pub entry_time: chrono::DateTime<chrono::Utc>,
    /// Entry transaction signature
    pub entry_signature: String,
    /// Entry type/recommendation that led to this position
    #[serde(default)]
    pub entry_type: EntryType,
    /// Whether quick partial profit has been taken (50% sell at quick_profit_pct)
    #[serde(default)]
    pub quick_profit_taken: bool,
    /// Whether second partial profit has been taken (25% sell at second_profit_pct)
    #[serde(default)]
    pub second_profit_taken: bool,
    /// Peak price seen since entry (for trailing stop)
    #[serde(default)]
    pub peak_price: f64,
    /// Current price (updated by price feed)
    #[serde(skip)]
    pub current_price: f64,
    /// Kill-switch triggered - exit immediately
    #[serde(default)]
    pub kill_switch_triggered: bool,
    /// Kill-switch reason (if triggered)
    #[serde(default)]
    pub kill_switch_reason: Option<String>,
    /// Wallet pubkey that holds this position (for multi-wallet support)
    #[serde(default)]
    pub wallet_pubkey: String,
    /// Confirmed sell signatures already applied to this position (idempotent partial exits).
    #[serde(default)]
    pub applied_exit_signatures: Vec<String>,
}

impl Position {
    /// Human-readable (UI) token amount, normalized by decimals.
    /// Returns `None` for legacy/unmigrated positions with no known decimals.
    pub fn token_amount_ui(&self) -> Option<f64> {
        match self.token_decimals {
            Some(d) => Some(self.token_amount as f64 / 10_f64.powi(d as i32)),
            None => None,
        }
    }

    /// Calculate current value in SOL
    pub fn current_value(&self) -> f64 {
        match self.token_amount_ui() {
            Some(ui) => ui * self.current_price,
            // Legacy fallback: raw token_amount treated as UI units.
            // Temporary until 001C migrates all positions to canonical decimals.
            None => self.token_amount as f64 * self.current_price,
        }
    }

    /// Calculate unrealized P&L in SOL
    pub fn unrealized_pnl(&self) -> f64 {
        self.current_value() - self.total_cost_sol
    }

    /// Calculate unrealized P&L percentage
    pub fn unrealized_pnl_pct(&self) -> f64 {
        if self.total_cost_sol == 0.0 {
            return 0.0;
        }
        (self.unrealized_pnl() / self.total_cost_sol) * 100.0
    }

    /// Check if position is in profit
    pub fn is_profitable(&self) -> bool {
        self.unrealized_pnl() > 0.0
    }
}

/// Daily trading statistics
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DailyStats {
    pub date: String,
    pub total_trades: u32,
    pub winning_trades: u32,
    pub losing_trades: u32,
    pub total_profit_sol: f64,
    pub total_loss_sol: f64,
    pub net_pnl_sol: f64,
    /// Realized profits available for extraction (not yet extracted)
    pub realized_profit_pending_extraction: f64,
    /// Total profits extracted to vault today
    pub extracted_today_sol: f64,
}

impl DailyStats {
    pub fn new() -> Self {
        Self {
            date: chrono::Utc::now().format("%Y-%m-%d").to_string(),
            ..Default::default()
        }
    }

    pub fn record_trade(&mut self, pnl_sol: f64) {
        self.total_trades += 1;
        if pnl_sol >= 0.0 {
            self.winning_trades += 1;
            self.total_profit_sol += pnl_sol;
            // Track profits available for extraction
            self.realized_profit_pending_extraction += pnl_sol;
        } else {
            self.losing_trades += 1;
            self.total_loss_sol += pnl_sol.abs();
        }
        self.net_pnl_sol = self.total_profit_sol - self.total_loss_sol;
    }

    /// Mark profits as extracted (moved to vault)
    pub fn mark_extracted(&mut self, amount: f64) {
        self.realized_profit_pending_extraction =
            (self.realized_profit_pending_extraction - amount).max(0.0);
        self.extracted_today_sol += amount;
    }

    /// Get realized profits pending extraction
    pub fn pending_extraction(&self) -> f64 {
        self.realized_profit_pending_extraction
    }

    pub fn win_rate(&self) -> f64 {
        if self.total_trades == 0 {
            return 0.0;
        }
        (self.winning_trades as f64 / self.total_trades as f64) * 100.0
    }
}

/// Result of a reconciled (signature-idempotent) position close.
#[derive(Debug, Clone, PartialEq)]
pub struct ReconciledCloseResult {
    pub pnl_sol: f64,
    pub fully_closed: bool,
    pub already_applied: bool,
    pub sold_amount: u64,
    pub remaining_amount: u64,
    pub remaining_cost_sol: f64,
}

/// A durable, canonical record of an applied exit (partial or full).
///
/// Keyed in the ledger by `signature`. This is the source of truth for
/// crash-safe idempotent replay: it survives even after the underlying
/// `Position` is removed on a full close.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppliedExitReceipt {
    pub signature: String,
    pub mint: String,
    pub sold_amount: u64,
    pub received_sol: f64,
    pub pnl_sol: f64,
    pub fully_closed: bool,
    pub remaining_amount: u64,
    pub remaining_cost_sol: f64,
}

/// Versioned on-disk snapshot: positions plus the applied-exit receipt ledger.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PositionStoreSnapshot {
    version: u32,
    positions: HashMap<String, Position>,
    #[serde(default)]
    applied_exit_receipts: HashMap<String, AppliedExitReceipt>,
}

/// Backward-compatible on-disk representation. New snapshots are the tagged
/// `PositionStoreSnapshot`; legacy files are a bare `HashMap<String, Position>`.
#[derive(Deserialize)]
#[serde(untagged)]
enum PositionStoreOnDisk {
    Snapshot(PositionStoreSnapshot),
    Legacy(HashMap<String, Position>),
}

/// Position manager
pub struct PositionManager {
    positions: Arc<RwLock<HashMap<String, Position>>>,
    daily_stats: Arc<RwLock<DailyStats>>,
    safety_config: SafetyConfig,
    persistence_path: Option<String>,
    /// Canonical ledger of applied exits, keyed by exit signature.
    applied_exit_receipts: Arc<RwLock<HashMap<String, AppliedExitReceipt>>>,
}

impl PositionManager {
    /// Create a new position manager
    pub fn new(safety_config: SafetyConfig, persistence_path: Option<String>) -> Self {
        Self {
            positions: Arc::new(RwLock::new(HashMap::new())),
            daily_stats: Arc::new(RwLock::new(DailyStats::new())),
            safety_config,
            persistence_path,
            applied_exit_receipts: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Validate a single applied-exit receipt (called on load). Live-money
    /// accounting: a malformed ledger must fail the load rather than silently
    /// corrupt recovery.
    fn validate_receipt(key: &str, r: &AppliedExitReceipt) -> Result<()> {
        if key != r.signature {
            return Err(Error::PositionPersistence(format!(
                "receipt ledger key {} does not match receipt.signature {}",
                key, r.signature
            )));
        }
        if r.signature.is_empty() {
            return Err(Error::PositionPersistence(
                "applied exit receipt has empty signature".to_string(),
            ));
        }
        if r.mint.is_empty() {
            return Err(Error::PositionPersistence(format!(
                "applied exit receipt {} has empty mint",
                r.signature
            )));
        }
        if r.sold_amount == 0 {
            return Err(Error::PositionPersistence(format!(
                "applied exit receipt {} has sold_amount == 0",
                r.signature
            )));
        }
        if !r.received_sol.is_finite() {
            return Err(Error::PositionPersistence(format!(
                "applied exit receipt {} has non-finite received_sol",
                r.signature
            )));
        }
        if !r.pnl_sol.is_finite() {
            return Err(Error::PositionPersistence(format!(
                "applied exit receipt {} has non-finite pnl_sol",
                r.signature
            )));
        }
        if !r.remaining_cost_sol.is_finite() || r.remaining_cost_sol < 0.0 {
            return Err(Error::PositionPersistence(format!(
                "applied exit receipt {} has invalid remaining_cost_sol",
                r.signature
            )));
        }
        if r.fully_closed && r.remaining_amount != 0 {
            return Err(Error::PositionPersistence(format!(
                "applied exit receipt {} is fully_closed but remaining_amount != 0",
                r.signature
            )));
        }
        if r.fully_closed && r.remaining_cost_sol != 0.0 {
            return Err(Error::PositionPersistence(format!(
                "applied exit receipt {} is fully_closed but remaining_cost_sol != 0",
                r.signature
            )));
        }
        Ok(())
    }

    /// Load positions (and the applied-exit receipt ledger) from disk.
    pub async fn load(&self) -> Result<()> {
        if let Some(path) = &self.persistence_path {
            if Path::new(path).exists() {
                let data = tokio::fs::read_to_string(path)
                    .await
                    .map_err(|e| Error::PositionPersistence(e.to_string()))?;

                let on_disk: PositionStoreOnDisk = serde_json::from_str(&data)
                    .map_err(|e| Error::PositionPersistence(e.to_string()))?;

                let (positions, receipts) = match on_disk {
                    PositionStoreOnDisk::Snapshot(snap) => {
                        (snap.positions, snap.applied_exit_receipts)
                    }
                    PositionStoreOnDisk::Legacy(positions) => (positions, HashMap::new()),
                };

                // Validate receipt ledger before committing to in-memory state.
                for (key, receipt) in &receipts {
                    Self::validate_receipt(key, receipt)?;
                }

                let positions_len = positions.len();
                let receipts_len = receipts.len();

                {
                    let mut guard = self.positions.write().await;
                    *guard = positions;
                }
                {
                    let mut guard = self.applied_exit_receipts.write().await;
                    *guard = receipts;
                }

                info!(
                    "Loaded {} positions and {} applied-exit receipts from {}",
                    positions_len, receipts_len, path
                );
            }
        }
        Ok(())
    }

    /// Save positions and the receipt ledger to disk as a versioned snapshot.
    pub async fn save(&self) -> Result<()> {
        if let Some(path) = &self.persistence_path {
            let positions = self.positions.read().await;
            let receipts = self.applied_exit_receipts.read().await;

            let snapshot = PositionStoreSnapshot {
                version: 1,
                positions: positions.clone(),
                applied_exit_receipts: receipts.clone(),
            };

            let data = serde_json::to_string_pretty(&snapshot)
                .map_err(|e| Error::PositionPersistence(e.to_string()))?;

            tokio::fs::write(path, data)
                .await
                .map_err(|e| Error::PositionPersistence(e.to_string()))?;

            debug!(
                "Saved {} positions and {} receipts to {}",
                positions.len(),
                receipts.len(),
                path
            );
        }
        Ok(())
    }

    /// Open a new position
    pub async fn open_position(&self, position: Position) -> Result<()> {
        // Check safety limits
        self.check_risk_limits(position.total_cost_sol).await?;

        // Add position
        let mint = position.mint.clone();
        let mut positions = self.positions.write().await;
        positions.insert(mint.clone(), position);
        drop(positions);

        info!("Opened position in {}", mint);

        // Persist
        self.save().await?;

        Ok(())
    }

    /// Verify limits before sending a new buy
    pub async fn can_open_position(&self, buy_amount: f64) -> Result<()> {
        self.check_risk_limits(buy_amount).await
    }

    /// Record a position that has already been confirmed on-chain.
    ///
    /// Does NOT check risk limits: the tokens are already owned, so refusing to
    /// track them would only hide real exposure. Returns `Ok(true)` if a new
    /// position was inserted, `Ok(false)` if an identical position (same mint and
    /// entry_signature) already exists (idempotent). A mint that already exists
    /// with a DIFFERENT entry_signature is an accounting conflict and is rejected
    /// without overwriting.
    pub async fn record_confirmed_position(&self, position: Position) -> Result<bool> {
        if position.mint.is_empty() {
            return Err(Error::PositionAccounting("mint is empty".to_string()));
        }
        if position.token_amount == 0 {
            return Err(Error::PositionAccounting("token_amount must be > 0".to_string()));
        }
        if position.token_decimals.is_none() {
            return Err(Error::PositionAccounting(
                "token_decimals must be known for a confirmed position".to_string(),
            ));
        }
        if !position.total_cost_sol.is_finite() || position.total_cost_sol <= 0.0 {
            return Err(Error::PositionAccounting(
                "total_cost_sol must be finite and > 0".to_string(),
            ));
        }
        if !position.entry_price.is_finite() || position.entry_price <= 0.0 {
            return Err(Error::PositionAccounting(
                "entry_price must be finite and > 0".to_string(),
            ));
        }
        if position.wallet_pubkey.is_empty() {
            return Err(Error::PositionAccounting("wallet_pubkey is empty".to_string()));
        }
        if position.entry_signature.is_empty() {
            return Err(Error::PositionAccounting("entry_signature is empty".to_string()));
        }

        let mint = position.mint.clone();
        {
            let mut positions = self.positions.write().await;
            match positions.get(&mint) {
                None => {
                    positions.insert(mint.clone(), position);
                }
                Some(existing) => {
                    if existing.entry_signature == position.entry_signature {
                        return Ok(false);
                    }
                    return Err(Error::PositionAccounting(format!(
                        "position for {} already exists with a different entry_signature",
                        mint
                    )));
                }
            }
        }

        info!("Recorded confirmed position in {}", mint);
        self.save().await?;
        Ok(true)
    }

    /// Close a position against a confirmed sell, idempotent by exit signature.
    ///
    /// Uses the confirmed on-chain `sold_amount` and `received_sol` (net proceeds,
    /// which may be negative). Applying the same `exit_signature` twice is a no-op
    /// and does not double-count P&L. Rejects an oversell (sold_amount greater than
    /// the tracked balance) without mutating state or stats.
    pub async fn close_position_reconciled(
        &self,
        mint: &str,
        exit_signature: &str,
        sold_amount: u64,
        received_sol: f64,
    ) -> Result<ReconciledCloseResult> {
        if exit_signature.is_empty() {
            return Err(Error::PositionAccounting("exit_signature is empty".to_string()));
        }
        if sold_amount == 0 {
            return Err(Error::PositionAccounting("sold_amount must be > 0".to_string()));
        }
        if !received_sol.is_finite() {
            return Err(Error::PositionAccounting("received_sol must be finite".to_string()));
        }

        // Crash-safe durable idempotency: consult the canonical receipt ledger
        // BEFORE touching positions. This survives even a full close that
        // removed the underlying Position, so replay after removal is a no-op
        // rather than a PositionNotFound.
        {
            let receipts = self.applied_exit_receipts.read().await;
            if let Some(receipt) = receipts.get(exit_signature) {
                // The same signature must describe the same deterministic
                // reconciliation of the same confirmed transaction. Contradictory
                // economics on a replay are a hard accounting error, never an
                // idempotent no-op. (Mutate nothing on any of these paths.)
                if receipt.mint != mint {
                    return Err(Error::PositionAccounting(format!(
                        "exit signature {} conflict on field 'mint': stored {} replayed {}",
                        exit_signature, receipt.mint, mint
                    )));
                }
                if receipt.sold_amount != sold_amount {
                    return Err(Error::PositionAccounting(format!(
                        "exit signature {} conflict on field 'sold_amount': stored {} replayed {}",
                        exit_signature, receipt.sold_amount, sold_amount
                    )));
                }
                if receipt.received_sol != received_sol {
                    return Err(Error::PositionAccounting(format!(
                        "exit signature {} conflict on field 'received_sol': stored {} replayed {}",
                        exit_signature, receipt.received_sol, received_sol
                    )));
                }
                // Already applied: return the DURABLE receipt's TRUE economics
                // (what actually happened — full/partial, realized P&L, remaining
                // state). already_applied=true tells the caller these are
                // historical/replayed, not newly applied. No P&L/stats/receipt
                // mutation, no save.
                return Ok(ReconciledCloseResult {
                    pnl_sol: receipt.pnl_sol,
                    fully_closed: receipt.fully_closed,
                    already_applied: true,
                    sold_amount: receipt.sold_amount,
                    remaining_amount: receipt.remaining_amount,
                    remaining_cost_sol: receipt.remaining_cost_sol,
                });
            }
        }

        // Result computed under the lock; stats/persist happen after unlocking.
        let (result, pnl_to_record, record_stats) = {
            let mut positions = self.positions.write().await;
            let position = positions
                .get_mut(mint)
                .ok_or_else(|| Error::PositionNotFound(mint.to_string()))?;

            if position.token_amount == 0 {
                return Err(Error::PositionAccounting(format!(
                    "position for {} has zero token_amount",
                    mint
                )));
            }
            if sold_amount > position.token_amount {
                return Err(Error::PositionAccounting(format!(
                    "oversell for {}: sold {} > held {}",
                    mint, sold_amount, position.token_amount
                )));
            }

            // Idempotency: already applied this signature.
            if position.applied_exit_signatures.iter().any(|s| s == exit_signature) {
                return Ok(ReconciledCloseResult {
                    pnl_sol: 0.0,
                    fully_closed: false,
                    already_applied: true,
                    sold_amount: 0,
                    remaining_amount: position.token_amount,
                    remaining_cost_sol: position.total_cost_sol,
                });
            }

            let sold_ratio = sold_amount as f64 / position.token_amount as f64;
            let cost_basis = position.total_cost_sol * sold_ratio;
            let pnl = received_sol - cost_basis;
            let fully_closed = sold_amount == position.token_amount;

            let result = if fully_closed {
                positions.remove(mint);
                info!("Reconciled close of {} (full) P&L: {} SOL", mint, pnl);
                ReconciledCloseResult {
                    pnl_sol: pnl,
                    fully_closed: true,
                    already_applied: false,
                    sold_amount,
                    remaining_amount: 0,
                    remaining_cost_sol: 0.0,
                }
            } else {
                position.token_amount -= sold_amount;
                position.total_cost_sol -= cost_basis;
                position.applied_exit_signatures.push(exit_signature.to_string());
                info!(
                    "Reconciled partial close of {}, remaining {} tokens, P&L: {} SOL",
                    mint, position.token_amount, pnl
                );
                ReconciledCloseResult {
                    pnl_sol: pnl,
                    fully_closed: false,
                    already_applied: false,
                    sold_amount,
                    remaining_amount: position.token_amount,
                    remaining_cost_sol: position.total_cost_sol,
                }
            };

            (result, pnl, true)
        };

        // Insert the canonical receipt into the ledger. This is the durable
        // record used for crash-safe replay (canonical over per-Position
        // applied_exit_signatures, which the partial path also updates).
        {
            let mut receipts = self.applied_exit_receipts.write().await;
            receipts.insert(
                exit_signature.to_string(),
                AppliedExitReceipt {
                    signature: exit_signature.to_string(),
                    mint: mint.to_string(),
                    sold_amount: result.sold_amount,
                    received_sol,
                    pnl_sol: result.pnl_sol,
                    fully_closed: result.fully_closed,
                    remaining_amount: result.remaining_amount,
                    remaining_cost_sol: result.remaining_cost_sol,
                },
            );
        }

        // Update daily stats exactly once (same path legacy close uses).
        if record_stats {
            let mut stats = self.daily_stats.write().await;
            stats.record_trade(pnl_to_record);
            drop(stats);
        }

        self.save().await?;
        Ok(result)
    }

    /// Legacy/non-reconciled close path. Do not use for newly wired live execution.
    /// 001C will migrate remaining callers.
    ///
    /// Close a position (fully or partially)
    pub async fn close_position(
        &self,
        mint: &str,
        sold_amount: u64,
        received_sol: f64,
    ) -> Result<f64> {
        let mut positions = self.positions.write().await;

        let position = positions
            .get_mut(mint)
            .ok_or_else(|| Error::PositionNotFound(mint.to_string()))?;

        // Calculate P&L for sold portion
        let sold_ratio = sold_amount as f64 / position.token_amount as f64;
        let cost_basis = position.total_cost_sol * sold_ratio;
        let pnl = received_sol - cost_basis;

        // Update position
        position.token_amount -= sold_amount;
        position.total_cost_sol -= cost_basis;

        // Remove if fully closed
        if position.token_amount == 0 {
            positions.remove(mint);
            info!("Closed position in {} with P&L: {} SOL", mint, pnl);
        } else {
            info!(
                "Partial close in {}, remaining: {} tokens, P&L: {} SOL",
                mint, position.token_amount, pnl
            );
        }

        drop(positions);

        // Update daily stats
        let mut stats = self.daily_stats.write().await;
        stats.record_trade(pnl);
        drop(stats);

        // Persist
        self.save().await?;

        Ok(pnl)
    }

    /// Remove a position without affecting daily stats (e.g., when a fill never landed)
    pub async fn abandon_position(&self, mint: &str) -> Result<()> {
        let mut positions = self.positions.write().await;
        if positions.remove(mint).is_some() {
            info!("Abandoned position in {} without recording P&L", mint);
            drop(positions);
            self.save().await?;
        }
        Ok(())
    }

    /// Update current price for a position and track peak price
    pub async fn update_price(&self, mint: &str, price: f64) {
        let mut positions = self.positions.write().await;
        if let Some(position) = positions.get_mut(mint) {
            position.current_price = price;
            // Track peak price for trailing stop
            if price > position.peak_price {
                position.peak_price = price;
            }
        }
    }

    /// Mark quick profit as taken for a position
    pub async fn mark_quick_profit_taken(&self, mint: &str) -> Result<()> {
        let mut positions = self.positions.write().await;
        if let Some(position) = positions.get_mut(mint) {
            position.quick_profit_taken = true;
        }
        drop(positions);
        self.save().await
    }

    /// Mark second profit as taken for a position
    pub async fn mark_second_profit_taken(&self, mint: &str) -> Result<()> {
        let mut positions = self.positions.write().await;
        if let Some(position) = positions.get_mut(mint) {
            position.second_profit_taken = true;
        }
        drop(positions);
        self.save().await
    }

    /// Trigger kill-switch for a position - forces immediate exit
    pub async fn trigger_kill_switch(&self, mint: &str, reason: &str) -> Result<()> {
        let mut positions = self.positions.write().await;
        if let Some(position) = positions.get_mut(mint) {
            position.kill_switch_triggered = true;
            position.kill_switch_reason = Some(reason.to_string());
            info!(
                "KILL-SWITCH triggered for {}: {}",
                position.symbol, reason
            );
        }
        drop(positions);
        self.save().await
    }

    /// Check if kill-switch is triggered for a position
    pub async fn is_kill_switch_triggered(&self, mint: &str) -> Option<String> {
        let positions = self.positions.read().await;
        positions.get(mint).and_then(|p| {
            if p.kill_switch_triggered {
                p.kill_switch_reason.clone()
            } else {
                None
            }
        })
    }

    /// Update the token amount for a position (used when actual balance differs from estimate)
    ///
    /// IMPORTANT: We do NOT recalculate entry_price here because actual_amount may be in
    /// raw token units (with 6+ decimals) while our original entry_price was calculated
    /// using normalized human-readable amounts. Recalculating would corrupt P&L tracking.
    pub async fn update_token_amount(&self, mint: &str, actual_amount: u64) -> Result<()> {
        let mut positions = self.positions.write().await;
        if let Some(position) = positions.get_mut(mint) {
            let old_amount = position.token_amount;
            position.token_amount = actual_amount;
            // Do NOT recalculate entry_price - preserve the original price from purchase
            // The entry_price was calculated correctly at buy time using cost/estimated_tokens
            info!(
                "Updated {} token amount: {} -> {} (entry price preserved at {:.10})",
                mint, old_amount, actual_amount, position.entry_price
            );
        }
        drop(positions);
        self.save().await
    }

    /// Get a position by mint
    pub async fn get_position(&self, mint: &str) -> Option<Position> {
        let positions = self.positions.read().await;
        positions.get(mint).cloned()
    }

    /// Get an applied-exit receipt by exit signature (read-only).
    pub async fn get_applied_exit_receipt(&self, signature: &str) -> Option<AppliedExitReceipt> {
        let receipts = self.applied_exit_receipts.read().await;
        receipts.get(signature).cloned()
    }

    /// Get all positions
    pub async fn get_all_positions(&self) -> Vec<Position> {
        let positions = self.positions.read().await;
        positions.values().cloned().collect()
    }

    /// Get total value of all positions
    pub async fn total_position_value(&self) -> f64 {
        let positions = self.positions.read().await;
        positions.values().map(|p| p.total_cost_sol).sum()
    }

    /// Get total unrealized P&L
    pub async fn total_unrealized_pnl(&self) -> f64 {
        let positions = self.positions.read().await;
        positions.values().map(|p| p.unrealized_pnl()).sum()
    }

    /// Get daily statistics
    pub async fn get_daily_stats(&self) -> DailyStats {
        self.daily_stats.read().await.clone()
    }

    /// Get realized profits pending extraction
    pub async fn get_pending_extraction(&self) -> f64 {
        self.daily_stats.read().await.pending_extraction()
    }

    /// Mark profits as extracted (called after successful vault transfer)
    pub async fn mark_profits_extracted(&self, amount: f64) {
        let mut stats = self.daily_stats.write().await;
        stats.mark_extracted(amount);
        info!("Marked {} SOL as extracted to vault", amount);
    }

    /// Check if daily loss limit is reached
    pub async fn is_daily_loss_limit_reached(&self) -> bool {
        let stats = self.daily_stats.read().await;
        stats.total_loss_sol >= self.safety_config.daily_loss_limit_sol
    }

    /// Get remaining capacity for new positions
    pub async fn remaining_position_capacity(&self) -> f64 {
        let total = self.total_position_value().await;
        (self.safety_config.max_position_sol - total).max(0.0)
    }

    /// Get daily loss remaining before limit
    pub async fn remaining_daily_loss(&self) -> f64 {
        let stats = self.daily_stats.read().await;
        (self.safety_config.daily_loss_limit_sol - stats.total_loss_sol).max(0.0)
    }

    /// Reset daily stats (call at UTC midnight)
    pub async fn reset_daily_stats(&self) {
        let mut stats = self.daily_stats.write().await;
        *stats = DailyStats::new();
        info!("Daily stats reset");
    }

    /// Get position count
    pub async fn position_count(&self) -> usize {
        self.positions.read().await.len()
    }

    async fn check_risk_limits(&self, buy_amount: f64) -> Result<()> {
        let total_position_value = self.total_position_value().await;
        if total_position_value + buy_amount > self.safety_config.max_position_sol {
            return Err(Error::MaxPositionExceeded {
                current: total_position_value,
                buy: buy_amount,
                max: self.safety_config.max_position_sol,
            });
        }

        let stats = self.daily_stats.read().await;
        if stats.total_loss_sol >= self.safety_config.daily_loss_limit_sol {
            return Err(Error::DailyLossLimitReached {
                lost: stats.total_loss_sol,
                limit: self.safety_config.daily_loss_limit_sol,
            });
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_position() -> Position {
        Position {
            mint: "test_mint".to_string(),
            name: "Test Token".to_string(),
            symbol: "TEST".to_string(),
            bonding_curve: "test_curve".to_string(),
            token_amount: 1_000_000,
            token_decimals: None,
            entry_price: 0.00000001, // 0.01 SOL for 1M tokens
            total_cost_sol: 0.01,
            entry_time: chrono::Utc::now(),
            entry_signature: "test_sig".to_string(),
            entry_type: EntryType::Legacy,
            quick_profit_taken: false,
            second_profit_taken: false,
            peak_price: 0.00000001,
            kill_switch_triggered: false,
            kill_switch_reason: None,
            wallet_pubkey: "test_wallet".to_string(),
            current_price: 0.000000015, // 50% profit: 0.015 SOL for 1M tokens
            applied_exit_signatures: vec![],
        }
    }

    /// Build a manager with a very small max_position_sol so post-fill recording
    /// exceeds pre-buy risk limits.
    fn tiny_limit_manager() -> PositionManager {
        let cfg = SafetyConfig {
            require_sell_confirmation: false,
            max_position_sol: 0.001,
            daily_loss_limit_sol: 100.0,
            keypair_balance_warning_sol: 0.0,
        };
        PositionManager::new(cfg, None)
    }

    /// A confirmed position: raw units with known decimals and all invariants met.
    fn confirmed_position(mint: &str, sig: &str, token_amount: u64, cost: f64) -> Position {
        let mut p = test_position();
        p.mint = mint.to_string();
        p.token_amount = token_amount;
        p.token_decimals = Some(6);
        p.total_cost_sol = cost;
        p.entry_price = 0.00000001;
        p.entry_signature = sig.to_string();
        p.wallet_pubkey = "test_wallet".to_string();
        p.applied_exit_signatures = vec![];
        p
    }

    #[test]
    fn test_position_pnl() {
        let position = test_position();

        // Current value = 1_000_000 * 0.000000015 = 0.015 SOL
        // Cost = 0.01 SOL
        // PnL = 0.005 SOL = 50%

        assert!((position.current_value() - 0.015).abs() < 0.0001);
        assert!((position.unrealized_pnl() - 0.005).abs() < 0.0001);
        assert!((position.unrealized_pnl_pct() - 50.0).abs() < 0.1);
        assert!(position.is_profitable());
    }

    #[test]
    fn test_daily_stats() {
        let mut stats = DailyStats::new();

        stats.record_trade(0.01); // Win
        stats.record_trade(-0.005); // Loss
        stats.record_trade(0.02); // Win

        assert_eq!(stats.total_trades, 3);
        assert_eq!(stats.winning_trades, 2);
        assert_eq!(stats.losing_trades, 1);
        assert!((stats.win_rate() - 66.67).abs() < 0.1);
    }

    #[test]
    fn test_reconciled_position_current_value_uses_decimals() {
        let mut p = test_position();
        p.token_amount = 1_500_000;
        p.token_decimals = Some(6);
        p.current_price = 0.002;

        assert!((p.token_amount_ui().unwrap() - 1.5).abs() < 1e-9);
        // 1.5 * 0.002 = 0.003
        assert!((p.current_value() - 0.003).abs() < 1e-9);
    }

    #[tokio::test]
    async fn test_record_confirmed_position_records_owned_position_over_postfill_limit() {
        let mgr = tiny_limit_manager();
        let cost = 0.05; // well above max_position_sol of 0.001
        let pos = confirmed_position("mintA", "sigA", 1_000_000, cost);

        // Pre-buy check would refuse this.
        assert!(mgr.can_open_position(cost).await.is_err());

        // But recording an already-confirmed position succeeds.
        assert_eq!(mgr.record_confirmed_position(pos).await.unwrap(), true);
        assert!(mgr.get_position("mintA").await.is_some());
    }

    #[tokio::test]
    async fn test_record_confirmed_position_same_signature_is_idempotent() {
        let mgr = tiny_limit_manager();
        let pos = confirmed_position("mintB", "sigB", 1_000_000, 0.05);
        assert_eq!(mgr.record_confirmed_position(pos.clone()).await.unwrap(), true);
        // Same mint + same signature => no-op false.
        assert_eq!(mgr.record_confirmed_position(pos).await.unwrap(), false);
        assert_eq!(mgr.position_count().await, 1);
    }

    #[tokio::test]
    async fn test_record_confirmed_position_different_signature_does_not_overwrite() {
        let mgr = tiny_limit_manager();
        let first = confirmed_position("mintC", "sigC1", 1_000_000, 0.05);
        assert_eq!(mgr.record_confirmed_position(first).await.unwrap(), true);

        let second = confirmed_position("mintC", "sigC2", 2_000_000, 0.09);
        assert!(mgr.record_confirmed_position(second).await.is_err());

        // Original untouched.
        let stored = mgr.get_position("mintC").await.unwrap();
        assert_eq!(stored.entry_signature, "sigC1");
        assert_eq!(stored.token_amount, 1_000_000);
    }

    #[tokio::test]
    async fn test_reconciled_close_rejects_oversell_without_mutation() {
        let mgr = tiny_limit_manager();
        let pos = confirmed_position("mintD", "sigD", 100, 1.0);
        mgr.record_confirmed_position(pos).await.unwrap();

        let res = mgr
            .close_position_reconciled("mintD", "exitD", 101, 0.5)
            .await;
        assert!(res.is_err());

        let stored = mgr.get_position("mintD").await.unwrap();
        assert_eq!(stored.token_amount, 100);
        assert!((stored.total_cost_sol - 1.0).abs() < 1e-12);
        assert_eq!(mgr.get_daily_stats().await.total_trades, 0);
    }

    #[tokio::test]
    async fn test_reconciled_partial_close_uses_actual_ratio_and_proceeds() {
        let mgr = tiny_limit_manager();
        let pos = confirmed_position("mintE", "sigE", 100, 1.0);
        mgr.record_confirmed_position(pos).await.unwrap();

        // Sell 40 of 100 for 0.6 SOL. cost_basis = 1.0 * 0.4 = 0.4, pnl = +0.2.
        let res = mgr
            .close_position_reconciled("mintE", "exitE", 40, 0.6)
            .await
            .unwrap();

        assert!(!res.already_applied);
        assert!(!res.fully_closed);
        assert_eq!(res.sold_amount, 40);
        assert!((res.pnl_sol - 0.2).abs() < 1e-9);
        assert_eq!(res.remaining_amount, 60);
        assert!((res.remaining_cost_sol - 0.6).abs() < 1e-9);

        let stored = mgr.get_position("mintE").await.unwrap();
        assert_eq!(stored.token_amount, 60);
        assert!((stored.total_cost_sol - 0.6).abs() < 1e-9);

        assert_eq!(mgr.get_daily_stats().await.total_trades, 1);
    }

    #[tokio::test]
    async fn test_reconciled_partial_close_is_idempotent_by_signature() {
        let mgr = tiny_limit_manager();
        let pos = confirmed_position("mintF", "sigF", 100, 1.0);
        mgr.record_confirmed_position(pos).await.unwrap();

        let first = mgr
            .close_position_reconciled("mintF", "exitF", 40, 0.6)
            .await
            .unwrap();
        assert!(!first.already_applied);

        // Replay same signature: returns the durable receipt's true economics
        // (already_applied=true), but applies NO new P&L/stats/mutation.
        let replay = mgr
            .close_position_reconciled("mintF", "exitF", 40, 0.6)
            .await
            .unwrap();
        assert!(replay.already_applied);
        assert!(!replay.fully_closed);
        assert_eq!(replay.sold_amount, 40);
        assert!((replay.pnl_sol - 0.2).abs() < 1e-9);
        assert_eq!(replay.remaining_amount, 60);
        assert!((replay.remaining_cost_sol - 0.6).abs() < 1e-9);

        let stored = mgr.get_position("mintF").await.unwrap();
        assert_eq!(stored.token_amount, 60);
        assert_eq!(mgr.get_daily_stats().await.total_trades, 1);
    }

    #[tokio::test]
    async fn test_reconciled_full_close_removes_position() {
        let mgr = tiny_limit_manager();
        let pos = confirmed_position("mintG", "sigG", 100, 1.0);
        mgr.record_confirmed_position(pos).await.unwrap();

        let res = mgr
            .close_position_reconciled("mintG", "exitG", 100, 1.5)
            .await
            .unwrap();

        assert!(res.fully_closed);
        assert!((res.pnl_sol - 0.5).abs() < 1e-9);
        assert_eq!(res.remaining_amount, 0);
        assert_eq!(res.remaining_cost_sol, 0.0);
        assert!(mgr.get_position("mintG").await.is_none());
        assert_eq!(mgr.get_daily_stats().await.total_trades, 1);
    }

    #[tokio::test]
    async fn test_reconciled_close_allows_negative_net_received() {
        let mgr = tiny_limit_manager();
        let pos = confirmed_position("mintH", "sigH", 100, 1.0);
        mgr.record_confirmed_position(pos).await.unwrap();

        // Negative net proceeds are allowed (e.g. fees exceed proceeds).
        let res = mgr
            .close_position_reconciled("mintH", "exitH", 100, -0.05)
            .await
            .unwrap();

        assert!(res.fully_closed);
        // pnl = -0.05 - 1.0 = -1.05
        assert!((res.pnl_sol - (-1.05)).abs() < 1e-9);
        assert_eq!(mgr.get_daily_stats().await.losing_trades, 1);
    }

    /// Unique temp file path for a persistence-backed manager.
    fn tmp_persistence_path(tag: &str) -> String {
        let mut p = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        p.push(format!("pumpfun_posstore_{}_{}.json", tag, nanos));
        p.to_string_lossy().to_string()
    }

    fn manager_with_path(path: &str) -> PositionManager {
        let cfg = SafetyConfig {
            require_sell_confirmation: false,
            max_position_sol: 100.0,
            daily_loss_limit_sol: 100.0,
            keypair_balance_warning_sol: 0.0,
        };
        PositionManager::new(cfg, Some(path.to_string()))
    }

    #[tokio::test]
    async fn test_position_store_loads_legacy_hashmap_format() {
        let path = tmp_persistence_path("legacy");

        // Write a legacy bare HashMap<String, Position> JSON.
        let mut legacy: HashMap<String, Position> = HashMap::new();
        legacy.insert(
            "mintLegacy".to_string(),
            confirmed_position("mintLegacy", "sigLegacy", 1_000_000, 0.05),
        );
        let json = serde_json::to_string_pretty(&legacy).unwrap();
        tokio::fs::write(&path, json).await.unwrap();

        let mgr = manager_with_path(&path);
        mgr.load().await.unwrap();

        assert!(mgr.get_position("mintLegacy").await.is_some());
        // Receipt ledger empty for legacy load.
        assert!(mgr.get_applied_exit_receipt("sigLegacy").await.is_none());

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn test_position_store_round_trips_applied_exit_receipts() {
        let path = tmp_persistence_path("roundtrip");

        let mgr = manager_with_path(&path);
        let pos = confirmed_position("mintRT", "sigRT", 100, 1.0);
        mgr.record_confirmed_position(pos).await.unwrap();

        // Full close => receipt recorded and persisted.
        let res = mgr
            .close_position_reconciled("mintRT", "exitRT", 100, 1.5)
            .await
            .unwrap();
        assert!(res.fully_closed);

        // Reload into a fresh manager.
        let mgr2 = manager_with_path(&path);
        mgr2.load().await.unwrap();

        // Position gone, receipt present.
        assert!(mgr2.get_position("mintRT").await.is_none());
        let receipt = mgr2.get_applied_exit_receipt("exitRT").await.unwrap();
        assert_eq!(receipt.signature, "exitRT");
        assert_eq!(receipt.mint, "mintRT");
        assert!(receipt.fully_closed);
        assert_eq!(receipt.remaining_amount, 0);
        assert!((receipt.pnl_sol - 0.5).abs() < 1e-9);

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn test_reconciled_full_close_replay_is_idempotent_after_position_removed() {
        let path = tmp_persistence_path("fullreplay");

        let mgr = manager_with_path(&path);
        let pos = confirmed_position("mintFR", "sigFR", 100, 1.0);
        mgr.record_confirmed_position(pos).await.unwrap();

        let res = mgr
            .close_position_reconciled("mintFR", "exitFR", 100, 1.5)
            .await
            .unwrap();
        assert!(res.fully_closed);
        assert!(mgr.get_position("mintFR").await.is_none());
        assert_eq!(mgr.get_daily_stats().await.total_trades, 1);

        // Reload fresh (position absent, receipt present).
        let mgr2 = manager_with_path(&path);
        mgr2.load().await.unwrap();
        assert!(mgr2.get_position("mintFR").await.is_none());

        // Replay the same full close: already_applied returns the DURABLE receipt's
        // TRUE economics (full close, original pnl), NOT PositionNotFound and NOT a
        // contradictory fully_closed=false.
        let replay = mgr2
            .close_position_reconciled("mintFR", "exitFR", 100, 1.5)
            .await
            .unwrap();
        assert!(replay.already_applied);
        assert!(replay.fully_closed);
        assert_eq!(replay.sold_amount, 100);
        assert!((replay.pnl_sol - 0.5).abs() < 1e-9);
        assert_eq!(replay.remaining_amount, 0);
        assert!((replay.remaining_cost_sol - 0.0).abs() < 1e-9);
        // Position still absent; fresh manager's stats untouched by the replay.
        assert!(mgr2.get_position("mintFR").await.is_none());
        assert_eq!(mgr2.get_daily_stats().await.total_trades, 0);

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn test_reconciled_partial_close_replay_uses_global_receipt() {
        let path = tmp_persistence_path("partialreplay");

        let mgr = manager_with_path(&path);
        let pos = confirmed_position("mintPR", "sigPR", 100, 1.0);
        mgr.record_confirmed_position(pos).await.unwrap();

        let first = mgr
            .close_position_reconciled("mintPR", "exitPR", 40, 0.6)
            .await
            .unwrap();
        assert!(!first.already_applied);
        assert_eq!(mgr.get_daily_stats().await.total_trades, 1);

        // Receipt present in the global ledger.
        let receipt = mgr.get_applied_exit_receipt("exitPR").await.unwrap();
        assert_eq!(receipt.remaining_amount, 60);

        // Replay same signature => already_applied returns the receipt's original
        // partial economics, no 2nd mutation.
        let replay = mgr
            .close_position_reconciled("mintPR", "exitPR", 40, 0.6)
            .await
            .unwrap();
        assert!(replay.already_applied);
        assert!(!replay.fully_closed);
        assert_eq!(replay.sold_amount, 40);
        assert!((replay.pnl_sol - 0.2).abs() < 1e-9);
        assert_eq!(replay.remaining_amount, 60);
        assert!((replay.remaining_cost_sol - 0.6).abs() < 1e-9);

        // No second trade recorded, position unchanged.
        assert_eq!(mgr.get_daily_stats().await.total_trades, 1);
        let stored = mgr.get_position("mintPR").await.unwrap();
        assert_eq!(stored.token_amount, 60);

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn test_exit_signature_collision_with_different_mint_is_error() {
        let path = tmp_persistence_path("collision");

        let mgr = manager_with_path(&path);
        // Establish a receipt for sig "exitX" on mint A via a real close.
        let pos_a = confirmed_position("mintA", "sigA", 100, 1.0);
        mgr.record_confirmed_position(pos_a).await.unwrap();
        mgr.close_position_reconciled("mintA", "exitX", 100, 1.2)
            .await
            .unwrap();

        // Now a different mint B replays the SAME exit signature => error.
        let pos_b = confirmed_position("mintB", "sigB", 100, 1.0);
        mgr.record_confirmed_position(pos_b).await.unwrap();
        let res = mgr
            .close_position_reconciled("mintB", "exitX", 100, 1.0)
            .await;
        assert!(matches!(res, Err(Error::PositionAccounting(_))));

        // Mint B untouched.
        let stored = mgr.get_position("mintB").await.unwrap();
        assert_eq!(stored.token_amount, 100);

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn test_exit_signature_replay_conflicting_sold_amount_is_error() {
        let path = tmp_persistence_path("conflictamount");

        let mgr = manager_with_path(&path);
        let pos = confirmed_position("mintCA", "sigCA", 100, 1.0);
        mgr.record_confirmed_position(pos).await.unwrap();

        // First application: partial sell 40 for 0.6 SOL.
        mgr.close_position_reconciled("mintCA", "exitConflictAmount", 40, 0.6)
            .await
            .unwrap();

        // Replay SAME signature + SAME mint but a DIFFERENT sold_amount => error.
        let res = mgr
            .close_position_reconciled("mintCA", "exitConflictAmount", 41, 0.6)
            .await;
        assert!(matches!(res, Err(Error::PositionAccounting(_))));

        // Nothing mutated: position still 60 raw, one trade, receipt still 40.
        let stored = mgr.get_position("mintCA").await.unwrap();
        assert_eq!(stored.token_amount, 60);
        assert_eq!(mgr.get_daily_stats().await.total_trades, 1);
        let receipt = mgr.get_applied_exit_receipt("exitConflictAmount").await.unwrap();
        assert_eq!(receipt.sold_amount, 40);

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn test_exit_signature_replay_conflicting_received_sol_is_error() {
        let path = tmp_persistence_path("conflictsol");

        let mgr = manager_with_path(&path);
        let pos = confirmed_position("mintCS", "sigCS", 100, 1.0);
        mgr.record_confirmed_position(pos).await.unwrap();

        // First application: sold 40 for 0.6 SOL.
        mgr.close_position_reconciled("mintCS", "exitConflictSol", 40, 0.6)
            .await
            .unwrap();

        // Replay SAME signature/mint/sold_amount but DIFFERENT received_sol => error.
        let res = mgr
            .close_position_reconciled("mintCS", "exitConflictSol", 40, 0.61)
            .await;
        assert!(matches!(res, Err(Error::PositionAccounting(_))));

        // Nothing mutated: position still 60, one trade, receipt still 0.6.
        let stored = mgr.get_position("mintCS").await.unwrap();
        assert_eq!(stored.token_amount, 60);
        assert_eq!(mgr.get_daily_stats().await.total_trades, 1);
        let receipt = mgr.get_applied_exit_receipt("exitConflictSol").await.unwrap();
        assert!((receipt.received_sol - 0.6).abs() < 1e-9);

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn test_invalid_applied_exit_receipt_fails_load() {
        let path = tmp_persistence_path("invalidreceipt");

        // Snapshot with an invalid receipt: fully_closed but remaining_amount != 0.
        let mut receipts: HashMap<String, AppliedExitReceipt> = HashMap::new();
        receipts.insert(
            "exitBad".to_string(),
            AppliedExitReceipt {
                signature: "exitBad".to_string(),
                mint: "mintBad".to_string(),
                sold_amount: 100,
                received_sol: 1.0,
                pnl_sol: 0.0,
                fully_closed: true,
                remaining_amount: 5, // invalid: fully_closed => must be 0
                remaining_cost_sol: 0.0,
            },
        );
        let snapshot = PositionStoreSnapshot {
            version: 1,
            positions: HashMap::new(),
            applied_exit_receipts: receipts,
        };
        let json = serde_json::to_string_pretty(&snapshot).unwrap();
        tokio::fs::write(&path, json).await.unwrap();

        let mgr = manager_with_path(&path);
        assert!(mgr.load().await.is_err());

        let _ = tokio::fs::remove_file(&path).await;
    }
}
