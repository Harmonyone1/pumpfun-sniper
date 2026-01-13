//! Alpha Score Computation
//!
//! Alpha Score measures a wallet's trading quality. Formula:
//! - Win Rate (35%): Percentage of profitable trades
//! - R-Multiple (30%): Average profit / average loss ratio
//! - Early Entry (20%): Pre-Raydium buy ratio
//! - Hold Discipline (15%): Partial sells + optimal hold time

use serde::{Deserialize, Serialize};

/// Wallet quality categories
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WalletCategory {
    /// Category A: True signal wallet - follow their trades
    /// Criteria: 65%+ win rate, 2x+ R-multiple, 30+ trades, active
    TrueSignal,

    /// Category B: Bundled/team wallet - likely coordinated
    /// Detected by: same-slot buys, identical amounts, common funding
    BundledTeam,

    /// Category C: MEV bot - front-running, sandwich attacks
    /// Detected by: sub-second execution, specific patterns
    MevBot,

    /// Profitable but not elite
    Profitable,

    /// Break-even trader
    Neutral,

    /// Losing trader
    Unprofitable,

    /// Not enough data to classify
    Unknown,
}

impl Default for WalletCategory {
    fn default() -> Self {
        Self::Unknown
    }
}

/// Alpha Score for a wallet
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlphaScore {
    /// Overall alpha score (-1.0 to +1.0)
    /// Positive = profitable trader, Negative = losing trader
    pub value: f64,

    /// Wallet category classification
    pub category: WalletCategory,

    /// Component scores (all normalized 0.0 to 1.0)
    pub win_rate_score: f64,
    pub r_multiple_score: f64,
    pub early_entry_score: f64,
    pub hold_discipline_score: f64,

    /// Raw metrics
    pub raw_win_rate: f64,
    pub raw_r_multiple: f64,
    pub pre_raydium_ratio: f64,
    pub total_trades: u32,

    /// Confidence in this score (0.0 to 1.0)
    /// Higher with more trades, more recent activity
    pub confidence: f64,
}

impl Default for AlphaScore {
    fn default() -> Self {
        Self {
            value: 0.0,
            category: WalletCategory::Unknown,
            win_rate_score: 0.0,
            r_multiple_score: 0.0,
            early_entry_score: 0.0,
            hold_discipline_score: 0.0,
            raw_win_rate: 0.0,
            raw_r_multiple: 0.0,
            pre_raydium_ratio: 0.0,
            total_trades: 0,
            confidence: 0.0,
        }
    }
}

/// Configuration for alpha score thresholds
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlphaScoreConfig {
    /// Minimum win rate for elite classification
    pub elite_min_win_rate: f64,
    /// Minimum R-multiple for elite classification
    pub elite_min_r_multiple: f64,
    /// Minimum completed trades for elite classification
    pub elite_min_trades: u32,
    /// Maximum inactive days for elite classification
    pub elite_max_inactive_days: u32,

    /// Component weights (must sum to 1.0)
    pub weight_win_rate: f64,
    pub weight_r_multiple: f64,
    pub weight_early_entry: f64,
    pub weight_hold_discipline: f64,
}

impl Default for AlphaScoreConfig {
    fn default() -> Self {
        Self {
            elite_min_win_rate: 0.65,
            elite_min_r_multiple: 2.0,
            elite_min_trades: 30,
            elite_max_inactive_days: 14,
            weight_win_rate: 0.35,
            weight_r_multiple: 0.30,
            weight_early_entry: 0.20,
            weight_hold_discipline: 0.15,
        }
    }
}

impl AlphaScore {
    /// Compute alpha score from wallet metrics
    pub fn compute(
        win_rate: f64,
        r_multiple: f64,
        pre_raydium_ratio: f64,
        partial_sell_ratio: f64,
        avg_hold_time_optimal_ratio: f64, // 0-1, how close to optimal hold time
        total_trades: u32,
        days_since_last_trade: u32,
        config: &AlphaScoreConfig,
    ) -> Self {
        // Normalize components to 0.0-1.0 range
        let win_rate_score = normalize(win_rate, 0.30, 0.85); // 30% baseline, 85% excellent
        let r_multiple_score = normalize(r_multiple, 0.5, 4.0); // 0.5x baseline, 4x excellent
        let early_entry_score = normalize(pre_raydium_ratio, 0.0, 0.6); // 0% baseline, 60% max
        let hold_discipline_score = (partial_sell_ratio * 0.5 + avg_hold_time_optimal_ratio * 0.5)
            .clamp(0.0, 1.0);

        // Compute weighted alpha score
        let raw_score = (win_rate_score * config.weight_win_rate)
            + (r_multiple_score * config.weight_r_multiple)
            + (early_entry_score * config.weight_early_entry)
            + (hold_discipline_score * config.weight_hold_discipline);

        // Transform to -1.0 to +1.0 range (0.5 raw = 0 alpha)
        let value = (raw_score - 0.5) * 2.0;

        // Calculate confidence based on sample size and recency
        let trade_confidence = normalize(total_trades as f64, 5.0, 100.0);
        let recency_confidence = normalize(
            (config.elite_max_inactive_days as f64 - days_since_last_trade as f64).max(0.0),
            0.0,
            config.elite_max_inactive_days as f64,
        );
        let confidence = (trade_confidence * 0.7 + recency_confidence * 0.3).clamp(0.0, 1.0);

        // Classify category
        let category = Self::classify_category(
            win_rate,
            r_multiple,
            total_trades,
            days_since_last_trade,
            config,
        );

        Self {
            value,
            category,
            win_rate_score,
            r_multiple_score,
            early_entry_score,
            hold_discipline_score,
            raw_win_rate: win_rate,
            raw_r_multiple: r_multiple,
            pre_raydium_ratio,
            total_trades,
            confidence,
        }
    }

    /// Classify wallet into category
    fn classify_category(
        win_rate: f64,
        r_multiple: f64,
        total_trades: u32,
        days_since_last_trade: u32,
        config: &AlphaScoreConfig,
    ) -> WalletCategory {
        // Not enough data
        if total_trades < 5 {
            return WalletCategory::Unknown;
        }

        // Check for elite (TrueSignal)
        let is_elite = win_rate >= config.elite_min_win_rate
            && r_multiple >= config.elite_min_r_multiple
            && total_trades >= config.elite_min_trades
            && days_since_last_trade <= config.elite_max_inactive_days;

        if is_elite {
            return WalletCategory::TrueSignal;
        }

        // Categorize by performance
        if win_rate >= 0.55 && r_multiple >= 1.0 {
            WalletCategory::Profitable
        } else if win_rate >= 0.45 && win_rate <= 0.55 {
            WalletCategory::Neutral
        } else {
            WalletCategory::Unprofitable
        }
    }

    /// Check if this is an elite wallet worth following
    pub fn is_elite(&self) -> bool {
        self.category == WalletCategory::TrueSignal
    }

    /// Check if this wallet should be avoided
    pub fn is_avoid(&self) -> bool {
        matches!(
            self.category,
            WalletCategory::BundledTeam | WalletCategory::MevBot | WalletCategory::Unprofitable
        )
    }

    /// Get a human-readable summary
    pub fn summary(&self) -> String {
        format!(
            "Alpha: {:.2} ({:?}) - WR: {:.0}%, R: {:.1}x, Trades: {}, Conf: {:.0}%",
            self.value,
            self.category,
            self.raw_win_rate * 100.0,
            self.raw_r_multiple,
            self.total_trades,
            self.confidence * 100.0
        )
    }
}

/// Normalize a value to 0.0-1.0 range
fn normalize(value: f64, min: f64, max: f64) -> f64 {
    if max <= min {
        return 0.5;
    }
    ((value - min) / (max - min)).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize() {
        assert!((normalize(0.5, 0.0, 1.0) - 0.5).abs() < 0.001);
        assert!((normalize(0.0, 0.0, 1.0) - 0.0).abs() < 0.001);
        assert!((normalize(1.0, 0.0, 1.0) - 1.0).abs() < 0.001);
        assert!((normalize(-1.0, 0.0, 1.0) - 0.0).abs() < 0.001); // Clamped
        assert!((normalize(2.0, 0.0, 1.0) - 1.0).abs() < 0.001); // Clamped
    }

    #[test]
    fn test_elite_classification() {
        let config = AlphaScoreConfig::default();
        let score = AlphaScore::compute(
            0.70,  // 70% win rate
            2.5,   // 2.5x R-multiple
            0.3,   // 30% pre-raydium
            0.5,   // 50% partial sells
            0.7,   // 70% optimal hold time
            50,    // 50 trades
            3,     // 3 days since last trade
            &config,
        );

        assert!(score.is_elite());
        assert_eq!(score.category, WalletCategory::TrueSignal);
        assert!(score.value > 0.0);
    }

    #[test]
    fn test_unprofitable_classification() {
        let config = AlphaScoreConfig::default();
        let score = AlphaScore::compute(
            0.30,  // 30% win rate
            0.5,   // 0.5x R-multiple
            0.0,   // 0% pre-raydium
            0.0,   // 0% partial sells
            0.3,   // 30% optimal hold time
            20,    // 20 trades
            2,     // 2 days since last trade
            &config,
        );

        assert!(!score.is_elite());
        assert_eq!(score.category, WalletCategory::Unprofitable);
        assert!(score.value < 0.0);
    }
}
