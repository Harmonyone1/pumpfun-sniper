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
    /// Current price (updated by price feed)
    #[serde(skip)]
    pub current_price: f64,
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
        } else {
            self.losing_trades += 1;
            self.total_loss_sol += pnl_sol.abs();
        }
        self.net_pnl_sol = self.total_profit_sol - self.total_loss_sol;
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
        let total_position_value = self.total_position_value().await;
        let new_total = total_position_value + position.total_cost_sol;

        if new_total > self.safety_config.max_position_sol {
            return Err(Error::MaxPositionExceeded {
                current: total_position_value,
                buy: position.total_cost_sol,
                max: self.safety_config.max_position_sol,
            });
        }

        // Check daily loss limit
        let stats = self.daily_stats.read().await;
        if stats.total_loss_sol >= self.safety_config.daily_loss_limit_sol {
            return Err(Error::DailyLossLimitReached {
                lost: stats.total_loss_sol,
                limit: self.safety_config.daily_loss_limit_sol,
            });
        }
        drop(stats);

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

    /// Update current price for a position
    pub async fn update_price(&self, mint: &str, price: f64) {
        let mut positions = self.positions.write().await;
        if let Some(position) = positions.get_mut(mint) {
            position.current_price = price;
        }
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

    /// Check if daily loss limit is reached
    pub async fn is_daily_loss_limit_reached(&self) -> bool {
        let stats = self.daily_stats.read().await;
        stats.total_loss_sol >= self.safety_config.daily_loss_limit_sol
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
            entry_price: 0.00000001,  // 0.01 SOL for 1M tokens
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
