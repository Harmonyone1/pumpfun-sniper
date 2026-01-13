//! Smart Money Signal Provider
//!
//! Provides signals based on wallet profiling and alpha score:
//! - Creator alpha score influences buy decisions
//! - Elite wallets get bonus signals
//! - Bundled/team wallets get penalty signals

use async_trait::async_trait;
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, warn};

use crate::filter::signals::{Signal, SignalProvider, SignalType};
use crate::filter::smart_money::{WalletCategory, WalletProfiler};
use crate::filter::types::SignalContext;

/// Smart Money Signal Provider
pub struct SmartMoneySignalProvider {
    profiler: Arc<WalletProfiler>,
}

impl SmartMoneySignalProvider {
    /// Create a new smart money signal provider
    pub fn new(profiler: Arc<WalletProfiler>) -> Self {
        Self { profiler }
    }
}

#[async_trait]
impl SignalProvider for SmartMoneySignalProvider {
    fn name(&self) -> &'static str {
        "SmartMoney"
    }

    fn signal_types(&self) -> &[SignalType] {
        &[SignalType::WalletPriorPerformance, SignalType::DeployerPattern]
    }

    fn is_hot_path(&self) -> bool {
        // Not hot path - requires async profile computation
        false
    }

    fn max_latency_ms(&self) -> u64 {
        3000 // 3 seconds for Helius lookup
    }

    async fn compute_token_signals(&self, context: &SignalContext) -> Vec<Signal> {
        let start = Instant::now();
        let mut signals = Vec::new();

        // Get creator wallet profile
        let profile_result = self.profiler.get_or_compute(&context.creator).await;
        let latency = start.elapsed();

        match profile_result {
            Ok(profile) => {
                let alpha = &profile.alpha_score;

                // Signal 1: WalletPriorPerformance based on alpha score
                let (perf_value, perf_confidence, perf_reason) = match alpha.category {
                    WalletCategory::TrueSignal => (
                        0.8, // Strong positive signal
                        alpha.confidence,
                        format!(
                            "Elite creator: {:.0}% win rate, {:.1}x R-mult, {} trades",
                            alpha.raw_win_rate * 100.0,
                            alpha.raw_r_multiple,
                            alpha.total_trades
                        ),
                    ),
                    WalletCategory::Profitable => (
                        0.3, // Moderate positive
                        alpha.confidence,
                        format!(
                            "Profitable creator: {:.0}% win rate, {:.1}x R-mult",
                            alpha.raw_win_rate * 100.0,
                            alpha.raw_r_multiple
                        ),
                    ),
                    WalletCategory::Neutral => (
                        0.0, // Neutral
                        alpha.confidence,
                        format!(
                            "Neutral creator: {:.0}% win rate",
                            alpha.raw_win_rate * 100.0
                        ),
                    ),
                    WalletCategory::Unprofitable => (
                        -0.5, // Negative signal
                        alpha.confidence,
                        format!(
                            "Unprofitable creator: {:.0}% win rate, {:.1}x R-mult",
                            alpha.raw_win_rate * 100.0,
                            alpha.raw_r_multiple
                        ),
                    ),
                    WalletCategory::BundledTeam => (
                        -0.7, // Strong negative - likely coordinated
                        0.8,
                        "Creator appears to be part of bundled/team operation".to_string(),
                    ),
                    WalletCategory::MevBot => (
                        -0.8, // Strong negative - MEV bot creator is sus
                        0.8,
                        "Creator shows MEV bot patterns".to_string(),
                    ),
                    WalletCategory::Unknown => (
                        -0.1, // Slight negative for unknown (caution)
                        0.3,  // Low confidence
                        format!(
                            "Unknown creator: only {} trades found",
                            alpha.total_trades
                        ),
                    ),
                };

                signals.push(
                    Signal::new(
                        SignalType::WalletPriorPerformance,
                        perf_value,
                        perf_confidence,
                        perf_reason,
                    )
                    .with_latency(latency)
                    .with_cached(!profile.is_stale(60)), // Recent profile counts as cached
                );

                // Signal 2: DeployerPattern based on trading behavior
                let (deploy_value, deploy_reason) = if profile.quick_flip_count > 10 {
                    (
                        -0.6,
                        format!(
                            "Creator has {} quick flips - likely pump & dump pattern",
                            profile.quick_flip_count
                        ),
                    )
                } else if profile.pre_raydium_ratio > 0.7 {
                    (
                        -0.4,
                        format!(
                            "Creator exits {:.0}% of trades pre-Raydium - early exit pattern",
                            profile.pre_raydium_ratio * 100.0
                        ),
                    )
                } else if profile.avg_hold_time_secs < 60 && profile.total_trades > 10 {
                    (
                        -0.3,
                        format!(
                            "Creator avg hold time {}s - very short term trader",
                            profile.avg_hold_time_secs
                        ),
                    )
                } else if alpha.is_elite() {
                    (
                        0.5,
                        "Creator has disciplined trading pattern".to_string(),
                    )
                } else {
                    (0.0, "Normal deployer pattern".to_string())
                };

                signals.push(
                    Signal::new(
                        SignalType::DeployerPattern,
                        deploy_value,
                        alpha.confidence,
                        deploy_reason,
                    )
                    .with_latency(latency),
                );

                debug!(
                    creator = %&context.creator[..8],
                    alpha = %format!("{:.2}", alpha.value),
                    category = ?alpha.category,
                    signals = %signals.len(),
                    latency_ms = %latency.as_millis(),
                    "Smart money signals computed"
                );
            }
            Err(e) => {
                warn!(
                    creator = %&context.creator[..8],
                    error = %e,
                    "Failed to compute wallet profile"
                );

                // Return unavailable signals
                signals.push(
                    Signal::unavailable(
                        SignalType::WalletPriorPerformance,
                        format!("Profile unavailable: {}", e),
                    )
                    .with_latency(latency),
                );
                signals.push(
                    Signal::unavailable(
                        SignalType::DeployerPattern,
                        format!("Profile unavailable: {}", e),
                    )
                    .with_latency(latency),
                );
            }
        }

        signals
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Integration tests would require mock HeliusClient
    // Unit tests for signal computation logic

    #[test]
    fn test_signal_types() {
        // Can't construct without profiler, but can test signal types are correct
        assert!(SignalType::WalletPriorPerformance.default_weight() > 1.0);
        assert!(SignalType::DeployerPattern.default_weight() > 1.0);
    }
}
