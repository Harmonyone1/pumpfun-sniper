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
    /// FASTER EXITS: Don't hold too long in volatile memecoins
    pub fn max_hold_secs(&self) -> Option<u64> {
        match self {
            EntryType::StrongBuy => Some(180),   // Was None - now 3 min max
            EntryType::Opportunity => Some(120), // Was 300 - now 2 min max
            EntryType::Probe => Some(90),        // Was 60 - now 90 sec max
            EntryType::Legacy => Some(120),      // Default 2 min
        }
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
    /// Amount of tokens held
    pub token_amount: u64,
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
}

impl Position {
    /// Calculate current value in SOL
    pub fn current_value(&self) -> f64 {
        self.token_amount as f64 * self.current_price
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

/// Position manager
pub struct PositionManager {
    positions: Arc<RwLock<HashMap<String, Position>>>,
    daily_stats: Arc<RwLock<DailyStats>>,
    safety_config: SafetyConfig,
    persistence_path: Option<String>,
}

impl PositionManager {
    /// Create a new position manager
    pub fn new(safety_config: SafetyConfig, persistence_path: Option<String>) -> Self {
        Self {
            positions: Arc::new(RwLock::new(HashMap::new())),
            daily_stats: Arc::new(RwLock::new(DailyStats::new())),
            safety_config,
            persistence_path,
        }
    }

    /// Load positions from disk
    pub async fn load(&self) -> Result<()> {
        if let Some(path) = &self.persistence_path {
            if Path::new(path).exists() {
                let data = tokio::fs::read_to_string(path)
                    .await
                    .map_err(|e| Error::PositionPersistence(e.to_string()))?;

                let positions: HashMap<String, Position> = serde_json::from_str(&data)
                    .map_err(|e| Error::PositionPersistence(e.to_string()))?;

                let mut guard = self.positions.write().await;
                *guard = positions;

                info!("Loaded {} positions from {}", guard.len(), path);
            }
        }
        Ok(())
    }

    /// Save positions to disk
    pub async fn save(&self) -> Result<()> {
        if let Some(path) = &self.persistence_path {
            let positions = self.positions.read().await;
            let data = serde_json::to_string_pretty(&*positions)
                .map_err(|e| Error::PositionPersistence(e.to_string()))?;

            tokio::fs::write(path, data)
                .await
                .map_err(|e| Error::PositionPersistence(e.to_string()))?;

            debug!("Saved {} positions to {}", positions.len(), path);
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
    pub async fn update_token_amount(&self, mint: &str, actual_amount: u64) -> Result<()> {
        let mut positions = self.positions.write().await;
        if let Some(position) = positions.get_mut(mint) {
            let old_amount = position.token_amount;
            position.token_amount = actual_amount;
            // Recalculate entry price based on actual tokens received
            if actual_amount > 0 {
                position.entry_price = position.total_cost_sol / actual_amount as f64;
            }
            info!(
                "Updated {} token amount: {} -> {} (entry price adjusted to {:.10})",
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
            entry_price: 0.00000001, // 0.01 SOL for 1M tokens
            total_cost_sol: 0.01,
            entry_time: chrono::Utc::now(),
            entry_signature: "test_sig".to_string(),
            current_price: 0.000000015, // 50% profit: 0.015 SOL for 1M tokens
        }
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
}
