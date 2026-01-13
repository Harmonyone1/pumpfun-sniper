//! Sniper Piggyback
//!
//! Follow profitable snipers and copy their trades.
//! Track sniper performance and identify high-quality signal sources.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};

/// Sniper piggyback configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SniperPiggybackConfig {
    pub enabled: bool,
    pub min_win_rate: f64,
    pub min_trades: u32,
    pub copy_delay_ms: u64,
    pub copy_size_ratio: f64,
    pub max_tracked_snipers: usize,
    pub trade_history_limit: usize,
}

impl Default for SniperPiggybackConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_win_rate: 0.6,
            min_trades: 5,
            copy_delay_ms: 100,
            copy_size_ratio: 0.5,
            max_tracked_snipers: 100,
            trade_history_limit: 50,
        }
    }
}

/// Individual sniper trade record
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SniperTrade {
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub mint: String,
    pub is_buy: bool,
    pub sol_amount: f64,
    pub entry_price: f64,
    pub exit_price: Option<f64>,
    pub profit_pct: Option<f64>,
    pub hold_time_secs: Option<u64>,
}

/// Sniper statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SniperStat {
    pub address: String,
    pub total_trades: u32,
    pub winning_trades: u32,
    pub win_rate: f64,
    pub avg_profit_pct: f64,
    pub avg_loss_pct: f64,
    pub avg_hold_time_secs: u64,
    pub total_profit_sol: f64,
    pub last_trade_time: chrono::DateTime<chrono::Utc>,
    pub consecutive_wins: u32,
    pub consecutive_losses: u32,
    pub quality_score: f64,
}

impl SniperStat {
    /// Create a new sniper stat
    pub fn new(address: String) -> Self {
        Self {
            address,
            total_trades: 0,
            winning_trades: 0,
            win_rate: 0.0,
            avg_profit_pct: 0.0,
            avg_loss_pct: 0.0,
            avg_hold_time_secs: 0,
            total_profit_sol: 0.0,
            last_trade_time: chrono::Utc::now(),
            consecutive_wins: 0,
            consecutive_losses: 0,
            quality_score: 0.0,
        }
    }

    /// Update stats with a completed trade
    pub fn record_trade(&mut self, profit_pct: f64, hold_time_secs: u64, profit_sol: f64) {
        self.total_trades += 1;
        self.total_profit_sol += profit_sol;
        self.last_trade_time = chrono::Utc::now();

        // Update win/loss tracking
        if profit_pct > 0.0 {
            self.winning_trades += 1;
            self.consecutive_wins += 1;
            self.consecutive_losses = 0;

            // Update average profit
            let prev_wins = self.winning_trades - 1;
            self.avg_profit_pct =
                (self.avg_profit_pct * prev_wins as f64 + profit_pct) / self.winning_trades as f64;
        } else {
            self.consecutive_wins = 0;
            self.consecutive_losses += 1;

            // Update average loss
            let prev_losses = self.total_trades - self.winning_trades - 1;
            if prev_losses > 0 {
                self.avg_loss_pct = (self.avg_loss_pct * prev_losses as f64 + profit_pct.abs())
                    / prev_losses as f64
                    + 1.0;
            } else {
                self.avg_loss_pct = profit_pct.abs();
            }
        }

        // Update win rate
        self.win_rate = self.winning_trades as f64 / self.total_trades as f64;

        // Update average hold time
        let prev_total = self.total_trades - 1;
        self.avg_hold_time_secs = ((self.avg_hold_time_secs as f64 * prev_total as f64
            + hold_time_secs as f64)
            / self.total_trades as f64) as u64;

        // Recalculate quality score
        self.update_quality_score();
    }

    /// Update quality score based on all metrics
    fn update_quality_score(&mut self) {
        let mut score = 0.0;

        // Win rate contribution (40%)
        score += self.win_rate * 0.4;

        // Profit contribution (30%) - use absolute profit or ratio
        if self.avg_loss_pct > 0.0 {
            // Profit/loss ratio when we have losses
            let profit_loss_ratio = self.avg_profit_pct / self.avg_loss_pct;
            score += (profit_loss_ratio / 3.0).min(0.3); // Cap at 0.3
        } else if self.avg_profit_pct > 0.0 {
            // No losses - use absolute profit magnitude (cap at 30%)
            // Scale: 100% profit -> 0.15, 200% profit -> 0.3
            score += (self.avg_profit_pct / 200.0).min(0.3);
        }

        // Trade volume contribution (15%)
        let volume_factor = (self.total_trades as f64 / 20.0).min(1.0);
        score += volume_factor * 0.15;

        // Recency contribution (15%)
        let hours_since_trade = chrono::Utc::now()
            .signed_duration_since(self.last_trade_time)
            .num_hours();
        let recency_factor = (1.0 - (hours_since_trade as f64 / 168.0)).max(0.0); // Decay over 1 week
        score += recency_factor * 0.15;

        self.quality_score = score;
    }

    /// Check if this sniper meets minimum quality threshold
    pub fn is_quality_sniper(&self, min_win_rate: f64, min_trades: u32) -> bool {
        self.total_trades >= min_trades && self.win_rate >= min_win_rate
    }
}

/// Piggyback signal
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PiggybackSignal {
    pub sniper_address: String,
    pub mint: String,
    pub sniper_size_sol: f64,
    pub suggested_size_sol: f64,
    pub sniper_win_rate: f64,
    pub sniper_avg_profit: f64,
    pub confidence: f64,
    pub suggested_delay_ms: u64,
}

/// Sniper piggyback tracker
pub struct SniperPiggyback {
    config: SniperPiggybackConfig,
    sniper_stats: HashMap<String, SniperStat>,
    sniper_trades: HashMap<String, VecDeque<SniperTrade>>,
    quality_snipers: Vec<String>, // Cached list of quality snipers
}

impl SniperPiggyback {
    /// Create a new sniper piggyback tracker
    pub fn new(config: SniperPiggybackConfig) -> Self {
        Self {
            config,
            sniper_stats: HashMap::new(),
            sniper_trades: HashMap::new(),
            quality_snipers: Vec::new(),
        }
    }

    /// Record a sniper buy
    pub fn record_sniper_buy(
        &mut self,
        sniper: &str,
        mint: &str,
        sol_amount: f64,
        entry_price: f64,
    ) {
        // Ensure sniper exists
        self.sniper_stats
            .entry(sniper.to_string())
            .or_insert_with(|| SniperStat::new(sniper.to_string()));

        // Record the trade
        let trades = self
            .sniper_trades
            .entry(sniper.to_string())
            .or_insert_with(VecDeque::new);

        trades.push_back(SniperTrade {
            timestamp: chrono::Utc::now(),
            mint: mint.to_string(),
            is_buy: true,
            sol_amount,
            entry_price,
            exit_price: None,
            profit_pct: None,
            hold_time_secs: None,
        });

        // Limit history
        while trades.len() > self.config.trade_history_limit {
            trades.pop_front();
        }
    }

    /// Record a sniper sell (close position)
    pub fn record_sniper_sell(
        &mut self,
        sniper: &str,
        mint: &str,
        sol_received: f64,
        exit_price: f64,
    ) {
        // Find the matching buy trade
        if let Some(trades) = self.sniper_trades.get_mut(sniper) {
            if let Some(buy_trade) = trades
                .iter_mut()
                .rev()
                .find(|t| t.mint == mint && t.is_buy && t.exit_price.is_none())
            {
                let entry_price = buy_trade.entry_price;
                let hold_time = chrono::Utc::now()
                    .signed_duration_since(buy_trade.timestamp)
                    .num_seconds() as u64;

                let profit_pct = if entry_price > 0.0 {
                    ((exit_price - entry_price) / entry_price) * 100.0
                } else {
                    0.0
                };

                let profit_sol = sol_received - buy_trade.sol_amount;

                // Update the trade record
                buy_trade.exit_price = Some(exit_price);
                buy_trade.profit_pct = Some(profit_pct);
                buy_trade.hold_time_secs = Some(hold_time);

                // Update sniper stats
                if let Some(stats) = self.sniper_stats.get_mut(sniper) {
                    stats.record_trade(profit_pct, hold_time, profit_sol);
                }

                // Refresh quality snipers list
                self.refresh_quality_snipers();
            }
        }
    }

    /// Check for piggyback opportunity when sniper buys
    pub fn on_sniper_buy(
        &self,
        sniper: &str,
        mint: &str,
        sol_amount: f64,
    ) -> Option<PiggybackSignal> {
        if !self.config.enabled {
            return None;
        }

        let stats = self.sniper_stats.get(sniper)?;

        // Check if meets quality threshold
        if !stats.is_quality_sniper(self.config.min_win_rate, self.config.min_trades) {
            return None;
        }

        // Calculate suggested size
        let suggested_size = sol_amount * self.config.copy_size_ratio;

        Some(PiggybackSignal {
            sniper_address: sniper.to_string(),
            mint: mint.to_string(),
            sniper_size_sol: sol_amount,
            suggested_size_sol: suggested_size,
            sniper_win_rate: stats.win_rate,
            sniper_avg_profit: stats.avg_profit_pct,
            confidence: stats.quality_score,
            suggested_delay_ms: self.config.copy_delay_ms,
        })
    }

    /// Get all quality snipers
    pub fn get_quality_snipers(&self) -> Vec<&SniperStat> {
        self.sniper_stats
            .values()
            .filter(|s| s.is_quality_sniper(self.config.min_win_rate, self.config.min_trades))
            .collect()
    }

    /// Get sniper stats by address
    pub fn get_sniper_stats(&self, address: &str) -> Option<&SniperStat> {
        self.sniper_stats.get(address)
    }

    /// Get recent trades for a sniper
    pub fn get_sniper_trades(&self, address: &str) -> Option<&VecDeque<SniperTrade>> {
        self.sniper_trades.get(address)
    }

    /// Get top snipers by quality score
    pub fn get_top_snipers(&self, limit: usize) -> Vec<&SniperStat> {
        let mut snipers: Vec<_> = self
            .sniper_stats
            .values()
            .filter(|s| s.total_trades >= self.config.min_trades)
            .collect();

        snipers.sort_by(|a, b| b.quality_score.partial_cmp(&a.quality_score).unwrap());
        snipers.truncate(limit);
        snipers
    }

    /// Check if address is a known quality sniper
    pub fn is_quality_sniper(&self, address: &str) -> bool {
        self.quality_snipers.contains(&address.to_string())
    }

    /// Import known sniper addresses
    pub fn import_snipers(&mut self, addresses: &[String]) {
        for addr in addresses {
            self.sniper_stats
                .entry(addr.clone())
                .or_insert_with(|| SniperStat::new(addr.clone()));
        }
    }

    /// Refresh cached list of quality snipers
    fn refresh_quality_snipers(&mut self) {
        self.quality_snipers = self
            .sniper_stats
            .iter()
            .filter(|(_, s)| s.is_quality_sniper(self.config.min_win_rate, self.config.min_trades))
            .map(|(addr, _)| addr.clone())
            .collect();
    }

    /// Clean up old/inactive snipers
    pub fn cleanup_inactive(&mut self, max_age_hours: i64) {
        let cutoff = chrono::Utc::now() - chrono::Duration::hours(max_age_hours);

        self.sniper_stats
            .retain(|_, s| s.last_trade_time > cutoff || s.total_trades >= 10);

        self.sniper_trades
            .retain(|addr, _| self.sniper_stats.contains_key(addr));

        self.refresh_quality_snipers();
    }
}

impl Default for SniperPiggyback {
    fn default() -> Self {
        Self::new(SniperPiggybackConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sniper_stat_recording() {
        let mut stat = SniperStat::new("sniper1".to_string());

        // Record winning trades
        stat.record_trade(50.0, 30, 0.05);
        stat.record_trade(30.0, 45, 0.03);
        stat.record_trade(-10.0, 20, -0.01);

        assert_eq!(stat.total_trades, 3);
        assert_eq!(stat.winning_trades, 2);
        assert!((stat.win_rate - 0.666).abs() < 0.01);
    }

    #[test]
    fn test_quality_sniper_check() {
        let mut stat = SniperStat::new("sniper1".to_string());

        // Not enough trades
        stat.record_trade(50.0, 30, 0.05);
        assert!(!stat.is_quality_sniper(0.6, 5));

        // Add more trades
        for _ in 0..4 {
            stat.record_trade(30.0, 30, 0.03);
        }

        assert!(stat.is_quality_sniper(0.6, 5));
    }

    #[test]
    fn test_piggyback_signal() {
        let mut piggyback = SniperPiggyback::default();

        // Create quality sniper
        for i in 0..10 {
            piggyback.record_sniper_buy("sniper1", &format!("mint{}", i), 0.1, 0.001);
            piggyback.record_sniper_sell("sniper1", &format!("mint{}", i), 0.15, 0.0015);
        }

        // Should get signal now
        let signal = piggyback.on_sniper_buy("sniper1", "new_mint", 0.2);
        assert!(signal.is_some());

        let signal = signal.unwrap();
        assert_eq!(signal.sniper_address, "sniper1");
        assert!((signal.suggested_size_sol - 0.1).abs() < 0.01); // 50% of 0.2
    }

    #[test]
    fn test_no_signal_for_unqualified() {
        let mut piggyback = SniperPiggyback::default();

        // Record only 2 trades (below minimum)
        piggyback.record_sniper_buy("sniper1", "mint1", 0.1, 0.001);
        piggyback.record_sniper_sell("sniper1", "mint1", 0.15, 0.0015);

        let signal = piggyback.on_sniper_buy("sniper1", "new_mint", 0.2);
        assert!(signal.is_none());
    }

    #[test]
    fn test_top_snipers() {
        let mut piggyback = SniperPiggyback::default();

        // Create snipers with different performance
        // Use different exit prices to create different profit percentages
        for i in 0..3 {
            let sniper = format!("sniper{}", i);
            for j in 0..10 {
                piggyback.record_sniper_buy(&sniper, &format!("mint{}{}", i, j), 0.1, 0.001);
                // Different exit prices: 0.003 (200%), 0.002 (100%), 0.0015 (50%)
                let exit_price = if i == 0 {
                    0.003
                } else if i == 1 {
                    0.002
                } else {
                    0.0015
                };
                piggyback.record_sniper_sell(&sniper, &format!("mint{}{}", i, j), 0.1, exit_price);
            }
        }

        let top = piggyback.get_top_snipers(2);
        assert_eq!(top.len(), 2);
        // Sniper0 should be first (highest profit percentage: 200%)
        assert_eq!(top[0].address, "sniper0");
    }

    #[test]
    fn test_consecutive_tracking() {
        let mut stat = SniperStat::new("sniper1".to_string());

        // Win streak
        stat.record_trade(50.0, 30, 0.05);
        stat.record_trade(30.0, 30, 0.03);
        stat.record_trade(20.0, 30, 0.02);
        assert_eq!(stat.consecutive_wins, 3);
        assert_eq!(stat.consecutive_losses, 0);

        // Break streak
        stat.record_trade(-10.0, 30, -0.01);
        assert_eq!(stat.consecutive_wins, 0);
        assert_eq!(stat.consecutive_losses, 1);
    }
}
