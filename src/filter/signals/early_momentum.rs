//! Early Momentum Signal Provider
//!
//! Detects pre-pump conditions by analyzing:
//! - Volume spikes before price moves
//! - Accumulation patterns (many buys, few sells)
//! - First trades quality (whale buys at launch)
//! - Bonding curve position (earlier = better)
//! - Creator buyback patterns

use async_trait::async_trait;
use dashmap::DashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info};

use super::{Signal, SignalProvider, SignalType};
use crate::config::EarlyDetectionConfig;
use crate::filter::types::SignalContext;

/// Trade record for tracking recent activity
#[derive(Debug, Clone)]
struct TradeRecord {
    timestamp: Instant,
    is_buy: bool,
    sol_amount: f64,
    trader: String,
}

/// Token trade history for analysis
#[derive(Debug)]
struct TokenTradeHistory {
    trades: VecDeque<TradeRecord>,
    unique_buyers: std::collections::HashSet<String>,
    unique_sellers: std::collections::HashSet<String>,
    total_buy_volume: f64,
    total_sell_volume: f64,
    first_trade_time: Option<Instant>,
    whale_buy_count: u32,
    creator_bought_back: bool,
}

impl Default for TokenTradeHistory {
    fn default() -> Self {
        Self {
            trades: VecDeque::with_capacity(100),
            unique_buyers: std::collections::HashSet::new(),
            unique_sellers: std::collections::HashSet::new(),
            total_buy_volume: 0.0,
            total_sell_volume: 0.0,
            first_trade_time: None,
            whale_buy_count: 0,
            creator_bought_back: false,
        }
    }
}

/// Early momentum signal provider
pub struct EarlyMomentumSignalProvider {
    config: EarlyDetectionConfig,
    /// Trade history per token mint
    token_history: Arc<DashMap<String, TokenTradeHistory>>,
    /// Baseline volume for comparison (rolling average)
    baseline_volumes: Arc<DashMap<String, f64>>,
}

impl EarlyMomentumSignalProvider {
    pub fn new(config: EarlyDetectionConfig) -> Self {
        Self {
            config,
            token_history: Arc::new(DashMap::new()),
            baseline_volumes: Arc::new(DashMap::new()),
        }
    }

    /// Record a trade for analysis
    pub fn record_trade(
        &self,
        mint: &str,
        is_buy: bool,
        sol_amount: f64,
        trader: &str,
        creator: Option<&str>,
    ) {
        let mut history = self.token_history.entry(mint.to_string()).or_default();

        let record = TradeRecord {
            timestamp: Instant::now(),
            is_buy,
            sol_amount,
            trader: trader.to_string(),
        };

        // Track first trade time
        if history.first_trade_time.is_none() {
            history.first_trade_time = Some(Instant::now());
        }

        // Update unique traders
        if is_buy {
            history.unique_buyers.insert(trader.to_string());
            history.total_buy_volume += sol_amount;

            // Check for whale buys
            if sol_amount >= self.config.whale_buy_threshold_sol {
                history.whale_buy_count += 1;
            }

            // Check for creator buyback
            if let Some(creator_addr) = creator {
                if trader == creator_addr {
                    history.creator_bought_back = true;
                    info!("[{}] Creator buyback detected: {} SOL", mint, sol_amount);
                }
            }
        } else {
            history.unique_sellers.insert(trader.to_string());
            history.total_sell_volume += sol_amount;
        }

        // Add to trade history (keep last 100 trades)
        history.trades.push_back(record);
        if history.trades.len() > 100 {
            history.trades.pop_front();
        }
    }

    /// Calculate volume spike signal
    fn compute_volume_spike(&self, mint: &str) -> Signal {
        let history = match self.token_history.get(mint) {
            Some(h) => h,
            None => return Signal::neutral(
                SignalType::VolumeSpike,
                "No trade history available",
            ),
        };

        let window_secs = self.config.volume_window_secs;
        let now = Instant::now();

        // Calculate recent volume
        let recent_volume: f64 = history
            .trades
            .iter()
            .filter(|t| now.duration_since(t.timestamp).as_secs() < window_secs)
            .map(|t| t.sol_amount)
            .sum();

        // Get or calculate baseline
        let baseline = self.baseline_volumes
            .get(mint)
            .map(|v| *v)
            .unwrap_or(recent_volume / 2.0); // Default to half current if no baseline

        if baseline <= 0.0 {
            return Signal::neutral(SignalType::VolumeSpike, "Insufficient baseline data");
        }

        let ratio = recent_volume / baseline;

        if ratio >= self.config.volume_spike_ratio {
            let value = ((ratio - 1.0) / 5.0).min(1.0); // Scale to 0-1
            Signal::new(
                SignalType::VolumeSpike,
                value,
                0.8,
                format!("Volume spike: {:.1}x baseline ({:.2} vs {:.2} SOL)",
                    ratio, recent_volume, baseline),
            )
        } else {
            Signal::neutral(
                SignalType::VolumeSpike,
                format!("Normal volume: {:.1}x baseline", ratio),
            )
        }
    }

    /// Calculate accumulation pattern signal
    fn compute_accumulation(&self, mint: &str) -> Signal {
        let history = match self.token_history.get(mint) {
            Some(h) => h,
            None => return Signal::neutral(
                SignalType::AccumulationPattern,
                "No trade history available",
            ),
        };

        let unique_buyers = history.unique_buyers.len() as u32;
        let unique_sellers = history.unique_sellers.len() as u32;

        // Check minimum unique buyers
        if unique_buyers < self.config.min_unique_buyers {
            return Signal::neutral(
                SignalType::AccumulationPattern,
                format!("Only {} unique buyers (need {})",
                    unique_buyers, self.config.min_unique_buyers),
            );
        }

        // Calculate buy/sell ratio
        let buy_sell_ratio = if unique_sellers == 0 {
            unique_buyers as f64 * 2.0 // No sellers is very bullish
        } else {
            unique_buyers as f64 / unique_sellers as f64
        };

        // Also consider volume ratio
        let volume_ratio = if history.total_sell_volume > 0.0 {
            history.total_buy_volume / history.total_sell_volume
        } else {
            history.total_buy_volume * 2.0
        };

        let combined_ratio = (buy_sell_ratio + volume_ratio) / 2.0;

        if combined_ratio >= self.config.accumulation_buy_ratio {
            let value = ((combined_ratio - 1.0) / 10.0).min(1.0);
            Signal::new(
                SignalType::AccumulationPattern,
                value,
                0.85,
                format!("Accumulation: {:.1} buy/sell ratio, {} buyers vs {} sellers",
                    combined_ratio, unique_buyers, unique_sellers),
            )
        } else if combined_ratio < 1.0 {
            // More sells than buys - negative signal
            Signal::new(
                SignalType::AccumulationPattern,
                -0.3,
                0.7,
                format!("Distribution: {:.1} buy/sell ratio (more selling)", combined_ratio),
            )
        } else {
            Signal::neutral(
                SignalType::AccumulationPattern,
                format!("Neutral flow: {:.1} buy/sell ratio", combined_ratio),
            )
        }
    }

    /// Calculate first trades quality signal
    fn compute_first_trades(&self, mint: &str) -> Signal {
        let history = match self.token_history.get(mint) {
            Some(h) => h,
            None => return Signal::neutral(
                SignalType::FirstTradesQuality,
                "No trade history available",
            ),
        };

        let first_n = self.config.first_trades_count as usize;
        let first_trades: Vec<_> = history.trades.iter().take(first_n).collect();

        if first_trades.is_empty() {
            return Signal::neutral(SignalType::FirstTradesQuality, "No trades yet");
        }

        // Count whale buys in first N trades
        let whale_buys = first_trades
            .iter()
            .filter(|t| t.is_buy && t.sol_amount >= self.config.whale_buy_threshold_sol)
            .count();

        let buy_ratio = first_trades.iter().filter(|t| t.is_buy).count() as f64
            / first_trades.len() as f64;

        if whale_buys >= 2 && buy_ratio > 0.7 {
            Signal::new(
                SignalType::FirstTradesQuality,
                0.8,
                0.9,
                format!("Strong launch: {} whale buys, {:.0}% buys in first {} trades",
                    whale_buys, buy_ratio * 100.0, first_trades.len()),
            )
        } else if buy_ratio > 0.8 {
            Signal::new(
                SignalType::FirstTradesQuality,
                0.5,
                0.8,
                format!("Good launch: {:.0}% buys in first trades", buy_ratio * 100.0),
            )
        } else if buy_ratio < 0.3 {
            Signal::new(
                SignalType::FirstTradesQuality,
                -0.5,
                0.8,
                format!("Weak launch: only {:.0}% buys (heavy selling)", buy_ratio * 100.0),
            )
        } else {
            Signal::neutral(
                SignalType::FirstTradesQuality,
                format!("Normal launch: {:.0}% buys", buy_ratio * 100.0),
            )
        }
    }

    /// Calculate bonding curve position signal
    fn compute_bonding_curve(&self, bonding_curve_pct: f64) -> Signal {
        if bonding_curve_pct <= 0.0 {
            return Signal::neutral(
                SignalType::BondingCurvePosition,
                "Bonding curve not available",
            );
        }

        if bonding_curve_pct > self.config.max_bonding_curve_pct {
            Signal::new(
                SignalType::BondingCurvePosition,
                -0.3,
                0.9,
                format!("Late entry: {:.1}% bonding curve (max: {:.0}%)",
                    bonding_curve_pct, self.config.max_bonding_curve_pct),
            )
        } else if bonding_curve_pct < 10.0 {
            // Very early - bonus!
            Signal::new(
                SignalType::BondingCurvePosition,
                self.config.early_entry_bonus + 0.3,
                0.95,
                format!("Very early entry: {:.1}% bonding curve", bonding_curve_pct),
            )
        } else if bonding_curve_pct < 20.0 {
            Signal::new(
                SignalType::BondingCurvePosition,
                self.config.early_entry_bonus,
                0.9,
                format!("Early entry: {:.1}% bonding curve", bonding_curve_pct),
            )
        } else {
            Signal::neutral(
                SignalType::BondingCurvePosition,
                format!("Normal entry: {:.1}% bonding curve", bonding_curve_pct),
            )
        }
    }

    /// Calculate creator buyback signal
    fn compute_creator_buyback(&self, mint: &str) -> Signal {
        let history = match self.token_history.get(mint) {
            Some(h) => h,
            None => return Signal::neutral(
                SignalType::CreatorBuyback,
                "No trade history available",
            ),
        };

        if history.creator_bought_back {
            Signal::new(
                SignalType::CreatorBuyback,
                0.6,
                0.85,
                "Creator buying back own token (confidence signal)",
            )
        } else {
            Signal::neutral(SignalType::CreatorBuyback, "No creator buyback detected")
        }
    }

    /// Clean old entries from history
    pub fn cleanup_old_entries(&self, max_age_secs: u64) {
        let cutoff = Instant::now() - Duration::from_secs(max_age_secs);

        self.token_history.retain(|_, history| {
            history.first_trade_time
                .map(|t| t > cutoff)
                .unwrap_or(false)
        });
    }
}

/// Static list of signal types for this provider
const EARLY_MOMENTUM_SIGNALS: &[SignalType] = &[
    SignalType::VolumeSpike,
    SignalType::AccumulationPattern,
    SignalType::FirstTradesQuality,
    SignalType::BondingCurvePosition,
    SignalType::CreatorBuyback,
];

#[async_trait]
impl SignalProvider for EarlyMomentumSignalProvider {
    fn name(&self) -> &'static str {
        "early_momentum"
    }

    fn signal_types(&self) -> &[SignalType] {
        EARLY_MOMENTUM_SIGNALS
    }

    fn is_hot_path(&self) -> bool {
        true // All signals use in-memory trade history
    }

    async fn compute_token_signals(&self, context: &SignalContext) -> Vec<Signal> {
        if !self.config.enabled {
            return vec![];
        }

        let mut signals = vec![];

        // Volume spike
        if self.config.volume_spike_enabled {
            signals.push(self.compute_volume_spike(&context.mint));
        }

        // Accumulation pattern
        if self.config.accumulation_enabled {
            signals.push(self.compute_accumulation(&context.mint));
        }

        // First trades quality
        if self.config.first_trades_enabled {
            signals.push(self.compute_first_trades(&context.mint));
        }

        // Bonding curve position (use virtual_sol_reserves as proxy if available)
        // This would need to be passed via context
        if let Some(bc_pct) = context.bonding_curve_pct {
            signals.push(self.compute_bonding_curve(bc_pct));
        }

        // Creator buyback
        if self.config.creator_buying_back {
            signals.push(self.compute_creator_buyback(&context.mint));
        }

        signals
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_early_momentum_provider() {
        let config = EarlyDetectionConfig::default();
        let provider = EarlyMomentumSignalProvider::new(config);

        // Record some trades
        provider.record_trade("test_mint", true, 1.0, "buyer1", None);
        provider.record_trade("test_mint", true, 0.5, "buyer2", None);
        provider.record_trade("test_mint", true, 2.0, "buyer3", None); // Whale buy

        // Check signals
        let accumulation = provider.compute_accumulation("test_mint");
        assert!(accumulation.value > 0.0, "Should detect accumulation");
    }
}
