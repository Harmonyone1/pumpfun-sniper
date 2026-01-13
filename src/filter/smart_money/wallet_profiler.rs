//! Wallet Profiler - P&L Calculation and Profiling
//!
//! Analyzes wallet transaction history to compute:
//! - Realized P&L using FIFO matching
//! - Win rate and R-multiple
//! - Trading patterns and behavior
//! - Alpha Score for wallet quality

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::debug;

use crate::error::Result;
use crate::filter::helius::HeliusClient;
use crate::filter::smart_money::alpha_score::{AlphaScore, AlphaScoreConfig};
use crate::filter::types::WalletTrade;

/// Configuration for wallet profiler
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletProfilerConfig {
    /// Cache TTL in seconds
    #[serde(default = "default_cache_ttl_secs")]
    pub cache_ttl_secs: u64,

    /// Minimum trades required for profiling
    #[serde(default = "default_min_trades")]
    pub min_trades: u32,

    /// Number of recent transactions to analyze
    #[serde(default = "default_tx_limit")]
    pub tx_limit: u32,

    /// Alpha score configuration
    #[serde(default)]
    pub alpha_config: AlphaScoreConfig,
}

fn default_cache_ttl_secs() -> u64 {
    3600 // 1 hour
}

fn default_min_trades() -> u32 {
    5
}

fn default_tx_limit() -> u32 {
    100
}

impl Default for WalletProfilerConfig {
    fn default() -> Self {
        Self {
            cache_ttl_secs: default_cache_ttl_secs(),
            min_trades: default_min_trades(),
            tx_limit: default_tx_limit(),
            alpha_config: AlphaScoreConfig::default(),
        }
    }
}

/// A single completed trade (buy matched to sell)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletedTrade {
    pub token_mint: String,
    pub buy_time: DateTime<Utc>,
    pub sell_time: DateTime<Utc>,
    pub buy_sol: f64,
    pub sell_sol: f64,
    pub profit_sol: f64,
    pub profit_pct: f64,
    pub hold_time_secs: i64,
    pub r_multiple: f64,
}

/// Wallet profile with P&L and behavioral analysis
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletProfile {
    pub address: String,
    pub fetched_at: DateTime<Utc>,

    // P&L metrics
    pub total_realized_profit_sol: f64,
    pub total_volume_sol: f64,
    pub completed_trades: Vec<CompletedTrade>,
    pub open_positions: u32,

    // Performance metrics
    pub win_count: u32,
    pub loss_count: u32,
    pub total_trades: u32,
    pub win_rate: f64,
    pub avg_win_sol: f64,
    pub avg_loss_sol: f64,
    pub avg_r_multiple: f64,
    pub largest_win_sol: f64,
    pub largest_loss_sol: f64,

    // Timing metrics
    pub avg_hold_time_secs: i64,
    pub pre_raydium_trades: u32, // Trades before graduation
    pub pre_raydium_ratio: f64,

    // Behavioral metrics
    pub partial_sell_count: u32,
    pub partial_sell_ratio: f64,
    pub quick_flip_count: u32, // Sells within 5 min
    pub last_trade_time: Option<DateTime<Utc>>,

    // Alpha score
    pub alpha_score: AlphaScore,
}

impl Default for WalletProfile {
    fn default() -> Self {
        Self {
            address: String::new(),
            fetched_at: Utc::now(),
            total_realized_profit_sol: 0.0,
            total_volume_sol: 0.0,
            completed_trades: Vec::new(),
            open_positions: 0,
            win_count: 0,
            loss_count: 0,
            total_trades: 0,
            win_rate: 0.0,
            avg_win_sol: 0.0,
            avg_loss_sol: 0.0,
            avg_r_multiple: 0.0,
            largest_win_sol: 0.0,
            largest_loss_sol: 0.0,
            avg_hold_time_secs: 0,
            pre_raydium_trades: 0,
            pre_raydium_ratio: 0.0,
            partial_sell_count: 0,
            partial_sell_ratio: 0.0,
            quick_flip_count: 0,
            last_trade_time: None,
            alpha_score: AlphaScore::default(),
        }
    }
}

impl WalletProfile {
    /// Check if profile is stale and needs refresh
    pub fn is_stale(&self, ttl_secs: u64) -> bool {
        let age = Utc::now() - self.fetched_at;
        age.num_seconds() > ttl_secs as i64
    }

    /// Get days since last trade
    pub fn days_since_last_trade(&self) -> u32 {
        self.last_trade_time
            .map(|t| (Utc::now() - t).num_days().max(0) as u32)
            .unwrap_or(365) // Default to 1 year if unknown
    }

    /// Check if this is an elite wallet
    pub fn is_elite(&self) -> bool {
        self.alpha_score.is_elite()
    }

    /// Check if this wallet should be avoided
    pub fn should_avoid(&self) -> bool {
        self.alpha_score.is_avoid()
    }
}

/// Wallet Profiler - computes P&L and profiles wallets
pub struct WalletProfiler {
    helius: Arc<HeliusClient>,
    config: WalletProfilerConfig,
    cache: DashMap<String, WalletProfile>,
}

impl WalletProfiler {
    /// Create a new wallet profiler
    pub fn new(helius: Arc<HeliusClient>, config: WalletProfilerConfig) -> Self {
        Self {
            helius,
            config,
            cache: DashMap::new(),
        }
    }

    /// Get cached profile if available and not stale
    pub fn get_cached(&self, address: &str) -> Option<WalletProfile> {
        self.cache.get(address).and_then(|profile| {
            if profile.is_stale(self.config.cache_ttl_secs) {
                None
            } else {
                Some(profile.clone())
            }
        })
    }

    /// Get or compute wallet profile
    pub async fn get_or_compute(&self, address: &str) -> Result<WalletProfile> {
        // Check cache first
        if let Some(cached) = self.get_cached(address) {
            debug!(address = %address, "Using cached wallet profile");
            return Ok(cached);
        }

        // Compute new profile
        let profile = self.compute_profile(address).await?;

        // Cache it
        self.cache.insert(address.to_string(), profile.clone());

        Ok(profile)
    }

    /// Compute wallet profile from transaction history
    pub async fn compute_profile(&self, address: &str) -> Result<WalletProfile> {
        debug!(address = %address, "Computing wallet profile");

        // Fetch transaction history
        let history = self
            .helius
            .get_wallet_history(address, self.config.tx_limit)
            .await?;

        // Group trades by token
        let trades_by_token = self.group_trades_by_token(&history.recent_trades);

        // Match buys to sells using FIFO
        let completed_trades = self.match_trades_fifo(&trades_by_token);

        // Calculate metrics
        let profile = self.calculate_metrics(address, completed_trades, &history.recent_trades);

        Ok(profile)
    }

    /// Group trades by token mint
    fn group_trades_by_token<'a>(&self, trades: &'a [WalletTrade]) -> HashMap<String, Vec<&'a WalletTrade>> {
        let mut by_token: HashMap<String, Vec<&'a WalletTrade>> = HashMap::new();

        for trade in trades {
            if let Some(ref mint) = trade.token_mint {
                by_token.entry(mint.clone()).or_default().push(trade);
            }
        }

        by_token
    }

    /// Match buys to sells using FIFO (First In, First Out)
    fn match_trades_fifo(
        &self,
        trades_by_token: &HashMap<String, Vec<&WalletTrade>>,
    ) -> Vec<CompletedTrade> {
        let mut completed = Vec::new();

        for (mint, trades) in trades_by_token {
            // Separate buys and sells, sorted by time
            let mut buys: Vec<_> = trades
                .iter()
                .filter(|t| t.is_buy)
                .cloned()
                .collect();
            let mut sells: Vec<_> = trades
                .iter()
                .filter(|t| !t.is_buy)
                .cloned()
                .collect();

            // Sort by timestamp
            buys.sort_by_key(|t| t.timestamp);
            sells.sort_by_key(|t| t.timestamp);

            // FIFO matching
            let mut buy_idx = 0;
            for sell in &sells {
                if buy_idx >= buys.len() {
                    break;
                }

                let buy = &buys[buy_idx];
                buy_idx += 1;

                // Calculate P&L
                let profit_sol = sell.sol_amount - buy.sol_amount;
                let profit_pct = if buy.sol_amount > 0.0 {
                    (profit_sol / buy.sol_amount) * 100.0
                } else {
                    0.0
                };

                let hold_time_secs = match (buy.timestamp, sell.timestamp) {
                    (Some(buy_time), Some(sell_time)) => (sell_time - buy_time).num_seconds(),
                    _ => 0,
                };

                // R-multiple: profit / risk (buy amount as risk proxy)
                let r_multiple = if buy.sol_amount > 0.0 {
                    profit_sol / buy.sol_amount
                } else {
                    0.0
                };

                completed.push(CompletedTrade {
                    token_mint: mint.clone(),
                    buy_time: buy.timestamp.unwrap_or_else(Utc::now),
                    sell_time: sell.timestamp.unwrap_or_else(Utc::now),
                    buy_sol: buy.sol_amount,
                    sell_sol: sell.sol_amount,
                    profit_sol,
                    profit_pct,
                    hold_time_secs,
                    r_multiple,
                });
            }
        }

        completed
    }

    /// Calculate all metrics from completed trades
    fn calculate_metrics(
        &self,
        address: &str,
        completed_trades: Vec<CompletedTrade>,
        all_trades: &[WalletTrade],
    ) -> WalletProfile {
        let total_trades = completed_trades.len() as u32;

        if total_trades == 0 {
            return WalletProfile {
                address: address.to_string(),
                fetched_at: Utc::now(),
                ..Default::default()
            };
        }

        // Win/loss metrics
        let wins: Vec<_> = completed_trades.iter().filter(|t| t.profit_sol > 0.0).collect();
        let losses: Vec<_> = completed_trades.iter().filter(|t| t.profit_sol <= 0.0).collect();

        let win_count = wins.len() as u32;
        let loss_count = losses.len() as u32;
        let win_rate = win_count as f64 / total_trades as f64;

        // Average win/loss
        let avg_win_sol = if !wins.is_empty() {
            wins.iter().map(|t| t.profit_sol).sum::<f64>() / wins.len() as f64
        } else {
            0.0
        };
        let avg_loss_sol = if !losses.is_empty() {
            losses.iter().map(|t| t.profit_sol.abs()).sum::<f64>() / losses.len() as f64
        } else {
            0.0
        };

        // R-multiple
        let avg_r_multiple = completed_trades.iter().map(|t| t.r_multiple).sum::<f64>()
            / total_trades as f64;

        // Total P&L
        let total_realized_profit_sol: f64 = completed_trades.iter().map(|t| t.profit_sol).sum();
        let total_volume_sol: f64 = completed_trades.iter().map(|t| t.buy_sol + t.sell_sol).sum();

        // Largest win/loss
        let largest_win_sol = completed_trades
            .iter()
            .map(|t| t.profit_sol)
            .fold(0.0_f64, f64::max);
        let largest_loss_sol = completed_trades
            .iter()
            .map(|t| t.profit_sol)
            .fold(0.0_f64, f64::min)
            .abs();

        // Hold time
        let avg_hold_time_secs = completed_trades.iter().map(|t| t.hold_time_secs).sum::<i64>()
            / total_trades as i64;

        // Quick flips (< 5 min)
        let quick_flip_count = completed_trades
            .iter()
            .filter(|t| t.hold_time_secs < 300)
            .count() as u32;

        // Pre-Raydium ratio (placeholder - would need graduation time data)
        // For now, estimate based on hold time (very short holds suggest pre-grad sniping)
        let pre_raydium_trades = completed_trades
            .iter()
            .filter(|t| t.hold_time_secs < 600) // < 10 min as proxy
            .count() as u32;
        let pre_raydium_ratio = pre_raydium_trades as f64 / total_trades as f64;

        // Last trade time
        let last_trade_time = all_trades.iter().filter_map(|t| t.timestamp).max();

        // Days since last trade for alpha score
        let days_since_last = last_trade_time
            .map(|t| (Utc::now() - t).num_days().max(0) as u32)
            .unwrap_or(365);

        // Optimal hold time ratio (30-120 seconds is "optimal" for pump.fun)
        let optimal_holds = completed_trades
            .iter()
            .filter(|t| t.hold_time_secs >= 30 && t.hold_time_secs <= 120)
            .count();
        let hold_time_optimal_ratio = optimal_holds as f64 / total_trades as f64;

        // Partial sell ratio (placeholder - would need position tracking)
        let partial_sell_ratio = 0.0; // TODO: Track partial sells

        // Compute alpha score
        let alpha_score = AlphaScore::compute(
            win_rate,
            avg_r_multiple,
            pre_raydium_ratio,
            partial_sell_ratio,
            hold_time_optimal_ratio,
            total_trades,
            days_since_last,
            &self.config.alpha_config,
        );

        debug!(
            address = %address,
            total_trades = %total_trades,
            win_rate = %format!("{:.1}%", win_rate * 100.0),
            r_multiple = %format!("{:.2}x", avg_r_multiple),
            alpha = %format!("{:.2}", alpha_score.value),
            category = ?alpha_score.category,
            "Computed wallet profile"
        );

        WalletProfile {
            address: address.to_string(),
            fetched_at: Utc::now(),
            total_realized_profit_sol,
            total_volume_sol,
            completed_trades,
            open_positions: 0,
            win_count,
            loss_count,
            total_trades,
            win_rate,
            avg_win_sol,
            avg_loss_sol,
            avg_r_multiple,
            largest_win_sol,
            largest_loss_sol,
            avg_hold_time_secs,
            pre_raydium_trades,
            pre_raydium_ratio,
            partial_sell_count: 0,
            partial_sell_ratio,
            quick_flip_count,
            last_trade_time,
            alpha_score,
        }
    }

    /// Clear cache for a specific address
    pub fn invalidate(&self, address: &str) {
        self.cache.remove(address);
    }

    /// Clear entire cache
    pub fn clear_cache(&self) {
        self.cache.clear();
    }

    /// Get cache size
    pub fn cache_size(&self) -> usize {
        self.cache.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration as ChronoDuration;

    #[test]
    fn test_completed_trade_profit() {
        let trade = CompletedTrade {
            token_mint: "test".to_string(),
            buy_time: Utc::now(),
            sell_time: Utc::now(),
            buy_sol: 1.0,
            sell_sol: 1.5,
            profit_sol: 0.5,
            profit_pct: 50.0,
            hold_time_secs: 60,
            r_multiple: 0.5,
        };

        assert!((trade.profit_sol - 0.5).abs() < 0.001);
        assert!((trade.profit_pct - 50.0).abs() < 0.001);
    }

    #[test]
    fn test_profile_staleness() {
        let mut profile = WalletProfile::default();
        profile.fetched_at = Utc::now() - ChronoDuration::seconds(7200); // 2 hours ago

        assert!(profile.is_stale(3600)); // 1 hour TTL
        assert!(!profile.is_stale(10800)); // 3 hour TTL
    }
}
