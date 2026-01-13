//! Wallet behavior signal provider
//!
//! This provider analyzes wallet behavior patterns using cached data.
//! Supports both hot-path (cached) and background (RPC) modes.

use async_trait::async_trait;
use std::sync::Arc;

use crate::filter::cache::FilterCache;
use crate::filter::signals::{Signal, SignalProvider, SignalType};
use crate::filter::types::SignalContext;

/// Wallet behavior signal provider using cached data
pub struct WalletBehaviorSignalProvider {
    /// Shared cache for wallet data
    cache: Arc<FilterCache>,
    /// Whether to operate in hot-path mode (cached only)
    hot_path_mode: bool,
}

impl WalletBehaviorSignalProvider {
    /// Create a new wallet behavior provider with shared cache
    pub fn new(cache: Arc<FilterCache>) -> Self {
        Self {
            cache,
            hot_path_mode: true,
        }
    }

    /// Create a background-mode provider (can make RPC calls)
    pub fn background_mode(cache: Arc<FilterCache>) -> Self {
        Self {
            cache,
            hot_path_mode: false,
        }
    }

    /// Check if creator is a known bad actor
    async fn check_known_actors(&self, context: &SignalContext) -> Vec<Signal> {
        let mut signals = Vec::new();
        let start = std::time::Instant::now();

        // Check known deployer blacklist
        if self.cache.is_known_deployer(&context.creator).await {
            signals.push(
                Signal::extreme_risk(SignalType::KnownDeployer, "Creator is known rug deployer")
                    .with_latency(start.elapsed())
                    .with_cached(true),
            );
        } else {
            signals.push(
                Signal::neutral(SignalType::KnownDeployer, "Creator not in deployer blacklist")
                    .with_latency(start.elapsed())
                    .with_cached(true),
            );
        }

        // Check known sniper list (creator being a sniper is suspicious)
        if self.cache.is_known_sniper(&context.creator).await {
            signals.push(
                Signal::new(
                    SignalType::KnownSniper,
                    -0.5,
                    0.9,
                    "Creator is a known sniper wallet",
                )
                .with_latency(start.elapsed())
                .with_cached(true),
            );
        }

        // Check if creator is a trusted wallet (positive signal)
        if self.cache.is_trusted(&context.creator).await {
            signals.push(
                Signal::new(
                    SignalType::WalletPriorPerformance,
                    0.7,
                    0.9,
                    "Creator is a trusted wallet",
                )
                .with_latency(start.elapsed())
                .with_cached(true),
            );
        }

        signals
    }

    /// Analyze wallet history from cache
    async fn analyze_cached_history(&self, context: &SignalContext) -> Vec<Signal> {
        let mut signals = Vec::new();
        let start = std::time::Instant::now();

        // Try to get cached wallet history
        if let Some(history) = self.cache.get_wallet(&context.creator) {
            // Wallet age signal - reduced penalties for pump.fun (all wallets are new)
            let age_days = history.age_days().unwrap_or(0.0) as i64;
            let age_signal = if age_days < 1 {
                Signal::new(
                    SignalType::WalletAge,
                    -0.15,  // Reduced from -0.7 - new wallets normal on pump.fun
                    0.4,    // Low confidence - less meaningful signal
                    format!("Very new wallet: {} days old", age_days),
                )
            } else if age_days < 7 {
                Signal::new(
                    SignalType::WalletAge,
                    -0.10,  // Reduced from -0.4
                    0.4,
                    format!("New wallet: {} days old", age_days),
                )
            } else if age_days < 30 {
                Signal::new(
                    SignalType::WalletAge,
                    0.0,    // Neutral - was -0.1
                    0.5,
                    format!("Moderately new wallet: {} days old", age_days),
                )
            } else if age_days < 90 {
                Signal::new(
                    SignalType::WalletAge,
                    0.1,
                    0.6,
                    format!("Established wallet: {} days old", age_days),
                )
            } else {
                Signal::new(
                    SignalType::WalletAge,
                    0.3,
                    0.8,
                    format!("Mature wallet: {} days old", age_days),
                )
            };
            signals.push(age_signal.with_latency(start.elapsed()).with_cached(true));

            // Transaction count signal
            let tx_count = history.total_trades;
            let tx_signal = if tx_count < 5 {
                Signal::new(
                    SignalType::WalletHistory,
                    -0.5,
                    0.8,
                    format!("Very low activity: {} transactions", tx_count),
                )
            } else if tx_count < 20 {
                Signal::new(
                    SignalType::WalletHistory,
                    -0.2,
                    0.7,
                    format!("Low activity: {} transactions", tx_count),
                )
            } else if tx_count < 100 {
                Signal::neutral(
                    SignalType::WalletHistory,
                    format!("Normal activity: {} transactions", tx_count),
                )
            } else {
                Signal::new(
                    SignalType::WalletHistory,
                    0.2,
                    0.6,
                    format!("High activity: {} transactions", tx_count),
                )
            };
            signals.push(tx_signal.with_latency(start.elapsed()).with_cached(true));

            // Prior rug count (if tracked)
            if history.deployed_rug_count > 0 {
                let rug_signal = if history.deployed_rug_count >= 3 {
                    Signal::extreme_risk(
                        SignalType::DeployerPattern,
                        format!("Creator has {} prior rugs", history.deployed_rug_count),
                    )
                } else {
                    Signal::new(
                        SignalType::DeployerPattern,
                        -0.7,
                        0.85,
                        format!("Creator has {} prior rug(s)", history.deployed_rug_count),
                    )
                };
                signals.push(rug_signal.with_latency(start.elapsed()).with_cached(true));
            }

            // Win rate (if we have enough data)
            if history.tokens_traded >= 5 {
                let win_rate = history.win_rate();
                let performance_signal = if win_rate > 0.7 {
                    Signal::new(
                        SignalType::WalletPriorPerformance,
                        0.5,
                        0.7,
                        format!("High win rate: {:.0}%", win_rate * 100.0),
                    )
                } else if win_rate > 0.5 {
                    Signal::new(
                        SignalType::WalletPriorPerformance,
                        0.2,
                        0.6,
                        format!("Good win rate: {:.0}%", win_rate * 100.0),
                    )
                } else if win_rate > 0.3 {
                    Signal::neutral(
                        SignalType::WalletPriorPerformance,
                        format!("Average win rate: {:.0}%", win_rate * 100.0),
                    )
                } else {
                    Signal::new(
                        SignalType::WalletPriorPerformance,
                        -0.3,
                        0.6,
                        format!("Low win rate: {:.0}%", win_rate * 100.0),
                    )
                };
                signals.push(
                    performance_signal
                        .with_latency(start.elapsed())
                        .with_cached(true),
                );
            }
        } else {
            // No cached data - return unavailable signals with reduced confidence
            signals.push(
                Signal::unavailable(
                    SignalType::WalletAge,
                    "Wallet age data not cached",
                )
                .with_latency(start.elapsed()),
            );
            signals.push(
                Signal::unavailable(
                    SignalType::WalletHistory,
                    "Wallet history not cached",
                )
                .with_latency(start.elapsed()),
            );
        }

        signals
    }
}

#[async_trait]
impl SignalProvider for WalletBehaviorSignalProvider {
    fn name(&self) -> &'static str {
        if self.hot_path_mode {
            "wallet_behavior_hot"
        } else {
            "wallet_behavior_background"
        }
    }

    fn signal_types(&self) -> &[SignalType] {
        &[
            SignalType::KnownDeployer,
            SignalType::KnownSniper,
            SignalType::WalletAge,
            SignalType::WalletHistory,
            SignalType::WalletPriorPerformance,
            SignalType::DeployerPattern,
        ]
    }

    fn is_hot_path(&self) -> bool {
        self.hot_path_mode
    }

    fn max_latency_ms(&self) -> u64 {
        if self.hot_path_mode {
            10 // Cached lookups should be very fast
        } else {
            2000 // Background mode can make RPC calls
        }
    }

    async fn compute_token_signals(&self, context: &SignalContext) -> Vec<Signal> {
        let mut signals = Vec::new();

        // Always check known actors (fast, cached)
        signals.extend(self.check_known_actors(context).await);

        // Check cached history
        signals.extend(self.analyze_cached_history(context).await);

        signals
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_context(creator: &str) -> SignalContext {
        SignalContext::from_new_token(
            "TestMint".to_string(),
            "Test Token".to_string(),
            "TEST".to_string(),
            "https://example.com".to_string(),
            creator.to_string(),
            "BondingCurve".to_string(),
            1000,
            1_000_000_000,
            100_000_000,
            1.0,
        )
    }

    #[tokio::test]
    async fn test_known_deployer_detection() {
        let cache = Arc::new(FilterCache::new());
        cache.add_known_deployer("bad_actor".to_string()).await;

        let provider = WalletBehaviorSignalProvider::new(cache);
        let context = make_context("bad_actor");
        let signals = provider.compute_token_signals(&context).await;

        let deployer_signal = signals
            .iter()
            .find(|s| s.signal_type == SignalType::KnownDeployer)
            .unwrap();
        assert_eq!(deployer_signal.value, -1.0, "Known deployer should be extreme risk");
    }

    #[tokio::test]
    async fn test_unknown_wallet() {
        let cache = Arc::new(FilterCache::new());
        let provider = WalletBehaviorSignalProvider::new(cache);
        let context = make_context("unknown_wallet");
        let signals = provider.compute_token_signals(&context).await;

        let deployer_signal = signals
            .iter()
            .find(|s| s.signal_type == SignalType::KnownDeployer)
            .unwrap();
        assert_eq!(deployer_signal.value, 0.0, "Unknown wallet should be neutral");
    }

    #[tokio::test]
    async fn test_known_sniper_creator() {
        let cache = Arc::new(FilterCache::new());
        cache.add_known_sniper("sniper_wallet".to_string()).await;

        let provider = WalletBehaviorSignalProvider::new(cache);
        let context = make_context("sniper_wallet");
        let signals = provider.compute_token_signals(&context).await;

        let sniper_signal = signals
            .iter()
            .find(|s| s.signal_type == SignalType::KnownSniper);
        assert!(sniper_signal.is_some(), "Should detect sniper creator");
        assert!(sniper_signal.unwrap().value < 0.0, "Sniper creator should be negative");
    }

    #[tokio::test]
    async fn test_cached_history_analysis() {
        use chrono::Utc;
        use crate::filter::types::WalletHistory;

        let cache = Arc::new(FilterCache::new());

        // Add wallet with history
        let history = WalletHistory {
            address: "test_wallet".to_string(),
            first_transaction: Some(Utc::now() - chrono::Duration::days(100)),
            total_transactions: 150,
            pump_fun_transactions: 50,
            tokens_deployed: 5,
            tokens_traded: 20,
            win_rate: 0.65,
            avg_holding_time_secs: 300,
            deployed_rug_count: 0,
            associated_wallets: vec![],
            cluster_id: None,
            fetched_at: Utc::now(),
            ..Default::default()
        };
        cache.set_wallet("test_wallet", history);

        let provider = WalletBehaviorSignalProvider::new(cache);
        let context = make_context("test_wallet");
        let signals = provider.compute_token_signals(&context).await;

        // Should have wallet age signal (positive for 100 day old wallet)
        let age_signal = signals
            .iter()
            .find(|s| s.signal_type == SignalType::WalletAge);
        assert!(age_signal.is_some(), "Should have wallet age signal");
        assert!(age_signal.unwrap().value > 0.0, "Mature wallet should be positive");

        // Should have wallet history signal (positive for high activity)
        let history_signal = signals
            .iter()
            .find(|s| s.signal_type == SignalType::WalletHistory);
        assert!(history_signal.is_some(), "Should have wallet history signal");
        assert!(history_signal.unwrap().value > 0.0, "High activity should be positive");
    }

    #[tokio::test]
    async fn test_no_cached_data() {
        let cache = Arc::new(FilterCache::new());
        let provider = WalletBehaviorSignalProvider::new(cache);
        let context = make_context("uncached_wallet");
        let signals = provider.compute_token_signals(&context).await;

        // Should have unavailable signals for age and history
        let age_signal = signals
            .iter()
            .find(|s| s.signal_type == SignalType::WalletAge);
        assert!(age_signal.is_some(), "Should have wallet age signal");
        assert_eq!(age_signal.unwrap().confidence, 0.0, "Unavailable should have 0 confidence");
    }
}
