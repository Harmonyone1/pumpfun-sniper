//! Front-Run Detection
//!
//! Detect accumulation patterns indicating big players entering.
//! Useful for "riding the wave" of institutional or whale buying.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Front-run detector configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrontRunDetectorConfig {
    pub enabled: bool,
    pub accumulation_threshold_sol: f64,
    pub cluster_window_secs: u64,
    pub min_cluster_size: usize,
    pub whale_threshold_sol: f64,
}

impl Default for FrontRunDetectorConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            accumulation_threshold_sol: 1.0,
            cluster_window_secs: 30,
            min_cluster_size: 3,
            whale_threshold_sol: 0.5,
        }
    }
}

/// Trade record for analysis
#[derive(Debug, Clone)]
pub struct TradeRecord {
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub trader: String,
    pub is_buy: bool,
    pub sol_amount: f64,
    pub token_amount: f64,
}

/// Wallet cluster (potentially coordinated wallets)
#[derive(Debug, Clone)]
pub struct WalletCluster {
    pub wallets: Vec<String>,
    pub total_bought_sol: f64,
    pub first_buy_time: chrono::DateTime<chrono::Utc>,
    pub last_buy_time: chrono::DateTime<chrono::Utc>,
    pub buy_count: u32,
}

/// Accumulation signal
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccumulationSignal {
    pub mint: String,
    pub confidence: f64,
    pub total_accumulated_sol: f64,
    pub unique_buyers: u32,
    pub cluster_detected: bool,
    pub whale_detected: bool,
    pub recommendation: String,
}

/// Front-run detector
pub struct FrontRunDetector {
    config: FrontRunDetectorConfig,
    recent_trades: HashMap<String, Vec<TradeRecord>>, // mint -> trades
}

impl FrontRunDetector {
    /// Create a new front-run detector
    pub fn new(config: FrontRunDetectorConfig) -> Self {
        Self {
            config,
            recent_trades: HashMap::new(),
        }
    }

    /// Record a trade for analysis
    pub fn record_trade(&mut self, mint: &str, trade: TradeRecord) {
        let trades = self.recent_trades.entry(mint.to_string()).or_default();
        trades.push(trade);

        // Keep only recent trades
        let cutoff = chrono::Utc::now()
            - chrono::Duration::seconds(self.config.cluster_window_secs as i64 * 10);
        trades.retain(|t| t.timestamp > cutoff);
    }

    /// Detect accumulation patterns
    pub fn detect_accumulation(&self, mint: &str) -> Option<AccumulationSignal> {
        if !self.config.enabled {
            return None;
        }

        let trades = self.recent_trades.get(mint)?;
        if trades.is_empty() {
            return None;
        }

        let now = chrono::Utc::now();
        let window_start =
            now - chrono::Duration::seconds(self.config.cluster_window_secs as i64);

        // Filter to recent buys only
        let recent_buys: Vec<_> = trades
            .iter()
            .filter(|t| t.is_buy && t.timestamp > window_start)
            .collect();

        if recent_buys.is_empty() {
            return None;
        }

        // Group by trader
        let mut trader_totals: HashMap<&str, f64> = HashMap::new();
        for trade in &recent_buys {
            *trader_totals.entry(&trade.trader).or_default() += trade.sol_amount;
        }

        let total_accumulated: f64 = trader_totals.values().sum();
        let unique_buyers = trader_totals.len() as u32;

        // Check for whale (single large buyer)
        let whale_detected = trader_totals
            .values()
            .any(|&v| v >= self.config.whale_threshold_sol);

        // Check for cluster (multiple coordinated buyers)
        let cluster_detected = unique_buyers >= self.config.min_cluster_size as u32
            && total_accumulated >= self.config.accumulation_threshold_sol;

        // Calculate confidence
        let mut confidence: f64 = 0.0;

        if whale_detected {
            confidence += 0.4;
        }

        if cluster_detected {
            confidence += 0.3;
        }

        // Volume factor
        if total_accumulated > self.config.accumulation_threshold_sol * 2.0 {
            confidence += 0.2;
        }

        // Buyer diversity factor
        if unique_buyers >= 5 {
            confidence += 0.1;
        }

        confidence = confidence.min(0.95);

        // Only return signal if significant
        if confidence < 0.3 {
            return None;
        }

        let recommendation = if confidence > 0.7 {
            "Strong accumulation detected - consider entry".to_string()
        } else if confidence > 0.5 {
            "Moderate accumulation - monitor closely".to_string()
        } else {
            "Weak accumulation signal".to_string()
        };

        Some(AccumulationSignal {
            mint: mint.to_string(),
            confidence,
            total_accumulated_sol: total_accumulated,
            unique_buyers,
            cluster_detected,
            whale_detected,
            recommendation,
        })
    }

    /// Detect potential front-running of known events
    pub fn detect_preemptive_buying(&self, mint: &str) -> Option<AccumulationSignal> {
        // Look for unusual buying patterns that might indicate
        // insider knowledge or front-running
        let trades = self.recent_trades.get(mint)?;

        let now = chrono::Utc::now();
        let window_start = now - chrono::Duration::seconds(60);

        let recent_buys: Vec<_> = trades
            .iter()
            .filter(|t| t.is_buy && t.timestamp > window_start)
            .collect();

        if recent_buys.len() < 2 {
            return None;
        }

        // Calculate buy velocity (buys per minute)
        let buy_count = recent_buys.len();
        let total_sol: f64 = recent_buys.iter().map(|t| t.sol_amount).sum();

        // Unusual if more than 5 buys/min or > 1 SOL/min
        if buy_count < 5 && total_sol < 1.0 {
            return None;
        }

        Some(AccumulationSignal {
            mint: mint.to_string(),
            confidence: 0.6,
            total_accumulated_sol: total_sol,
            unique_buyers: buy_count as u32,
            cluster_detected: buy_count >= 5,
            whale_detected: total_sol > 2.0,
            recommendation: "Unusual buy velocity detected".to_string(),
        })
    }

    /// Clear trade history for a token
    pub fn clear(&mut self, mint: &str) {
        self.recent_trades.remove(mint);
    }

    /// Clear all trade history
    pub fn clear_all(&mut self) {
        self.recent_trades.clear();
    }
}

impl Default for FrontRunDetector {
    fn default() -> Self {
        Self::new(FrontRunDetectorConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_buy_trade(trader: &str, sol_amount: f64) -> TradeRecord {
        TradeRecord {
            timestamp: chrono::Utc::now(),
            trader: trader.to_string(),
            is_buy: true,
            sol_amount,
            token_amount: sol_amount * 1000.0,
        }
    }

    #[test]
    fn test_whale_detection() {
        let mut detector = FrontRunDetector::default();

        // Record a whale buy
        detector.record_trade("mint1", create_buy_trade("whale1", 1.0));

        let signal = detector.detect_accumulation("mint1");
        assert!(signal.is_some());
        let signal = signal.unwrap();
        assert!(signal.whale_detected);
    }

    #[test]
    fn test_cluster_detection() {
        let mut detector = FrontRunDetector::default();

        // Record multiple buys from different traders
        detector.record_trade("mint1", create_buy_trade("trader1", 0.3));
        detector.record_trade("mint1", create_buy_trade("trader2", 0.3));
        detector.record_trade("mint1", create_buy_trade("trader3", 0.3));
        detector.record_trade("mint1", create_buy_trade("trader4", 0.3));

        let signal = detector.detect_accumulation("mint1");
        assert!(signal.is_some());
        let signal = signal.unwrap();
        assert!(signal.cluster_detected);
        assert_eq!(signal.unique_buyers, 4);
    }

    #[test]
    fn test_no_signal_below_threshold() {
        let mut detector = FrontRunDetector::default();

        // Small buys below threshold
        detector.record_trade("mint1", create_buy_trade("trader1", 0.1));

        let signal = detector.detect_accumulation("mint1");
        assert!(signal.is_none());
    }

    #[test]
    fn test_preemptive_buying() {
        let mut detector = FrontRunDetector::default();

        // Many rapid buys
        for i in 0..10 {
            detector.record_trade("mint1", create_buy_trade(&format!("trader{}", i), 0.15));
        }

        let signal = detector.detect_preemptive_buying("mint1");
        assert!(signal.is_some());
    }

    #[test]
    fn test_clear() {
        let mut detector = FrontRunDetector::default();

        detector.record_trade("mint1", create_buy_trade("trader1", 1.0));
        detector.clear("mint1");

        let signal = detector.detect_accumulation("mint1");
        assert!(signal.is_none());
    }
}
