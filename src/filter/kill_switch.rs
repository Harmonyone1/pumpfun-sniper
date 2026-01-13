//! Kill-Switch Module - Immediate exit triggers
//!
//! Kill-switches are non-negotiable exit conditions that override all other logic.
//! When a kill-switch fires, we EXIT immediately - no debate.
//!
//! Kill-switch triggers:
//! - Deployer sells ANY amount
//! - Top holder sells (Critical urgency)
//! - Bundled wallets selling together (future)
//! - Sniper wallets exiting before graduation (future)

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::filter::holder_watcher::{AlertUrgency, HolderWatcher, HolderWatcherConfig};

/// Kill-switch configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KillSwitchConfig {
    /// Enable kill-switches
    #[serde(default = "default_enabled")]
    pub enabled: bool,

    /// Exit if deployer sells ANY amount
    #[serde(default = "default_deployer_sell_any")]
    pub deployer_sell_any: bool,

    /// Exit if top holder sells (holder_watcher Critical alert)
    #[serde(default = "default_top_holder_sell")]
    pub top_holder_sell: bool,

    /// Number of bundled wallets selling together to trigger exit
    #[serde(default = "default_bundled_sell_count")]
    pub bundled_sell_count: u32,

    /// Window in seconds for bundled sell detection
    #[serde(default = "default_bundled_sell_window_secs")]
    pub bundled_sell_window_secs: u64,
}

fn default_enabled() -> bool { true }
fn default_deployer_sell_any() -> bool { true }
fn default_top_holder_sell() -> bool { true }
fn default_bundled_sell_count() -> u32 { 2 }
fn default_bundled_sell_window_secs() -> u64 { 30 }

impl Default for KillSwitchConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            deployer_sell_any: default_deployer_sell_any(),
            top_holder_sell: default_top_holder_sell(),
            bundled_sell_count: default_bundled_sell_count(),
            bundled_sell_window_secs: default_bundled_sell_window_secs(),
        }
    }
}

/// Kill-switch urgency level
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillSwitchUrgency {
    /// Exit NOW - no delay
    Immediate,
    /// Exit within 1-2 seconds
    High,
    /// Exit within 5 seconds
    Medium,
}

/// Kill-switch alert types
#[derive(Debug, Clone)]
pub enum KillSwitchType {
    /// Deployer/creator sold tokens
    DeployerSell {
        amount_tokens: u64,
        amount_pct: f64,
    },
    /// Top holder selling (from holder_watcher)
    TopHolderSell {
        holder: String,
        rank: usize,
        amount_pct: f64,
    },
    /// Bundled wallets selling together (future)
    BundledWalletsSelling {
        wallets_selling: u32,
        total_pct: f64,
    },
    /// Sniper wallets exiting before graduation (future)
    SniperExit {
        sniper_count: u32,
    },
}

/// Kill-switch alert
#[derive(Debug, Clone)]
pub struct KillSwitchAlert {
    pub alert_type: KillSwitchType,
    pub mint: String,
    pub urgency: KillSwitchUrgency,
    pub reason: String,
    pub auto_exit: bool,
}

/// Kill-switch decision
#[derive(Debug, Clone)]
pub enum KillSwitchDecision {
    /// Continue trading
    Continue,
    /// Exit immediately with reason
    Exit(KillSwitchAlert),
}

/// Deployer tracker - tracks which wallet deployed each token
pub struct DeployerTracker {
    /// mint -> creator address
    deployers: DashMap<String, String>,
}

impl DeployerTracker {
    pub fn new() -> Self {
        Self {
            deployers: DashMap::new(),
        }
    }

    /// Track the deployer for a token
    pub fn track(&self, mint: &str, creator: &str) {
        self.deployers.insert(mint.to_string(), creator.to_string());
        info!(mint = %mint, creator = %creator, "Tracking deployer");
    }

    /// Stop tracking a token
    pub fn untrack(&self, mint: &str) {
        self.deployers.remove(mint);
    }

    /// Check if a wallet is the deployer for a token
    pub fn is_deployer(&self, mint: &str, wallet: &str) -> bool {
        self.deployers
            .get(mint)
            .map(|d| d.value() == wallet)
            .unwrap_or(false)
    }

    /// Get deployer for a token
    pub fn get_deployer(&self, mint: &str) -> Option<String> {
        self.deployers.get(mint).map(|d| d.value().clone())
    }
}

impl Default for DeployerTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Kill-switch evaluator - checks trades for kill-switch conditions
pub struct KillSwitchEvaluator {
    config: KillSwitchConfig,
    deployer_tracker: DeployerTracker,
    holder_watcher: HolderWatcher,
}

impl KillSwitchEvaluator {
    pub fn new(config: KillSwitchConfig, holder_watcher_config: HolderWatcherConfig) -> Self {
        Self {
            config,
            deployer_tracker: DeployerTracker::new(),
            holder_watcher: HolderWatcher::new(holder_watcher_config),
        }
    }

    /// Track a new position - start monitoring deployer and holders
    pub fn watch_position(&self, mint: &str, creator: &str, holders: Vec<(String, u64, f64)>) {
        // Track deployer
        self.deployer_tracker.track(mint, creator);

        // Track top holders
        self.holder_watcher.watch_token(mint, holders);
    }

    /// Stop watching a position (we exited)
    pub fn unwatch_position(&self, mint: &str) {
        self.deployer_tracker.untrack(mint);
        self.holder_watcher.unwatch_token(mint);
    }

    /// Evaluate a sell trade for kill-switch conditions
    /// Returns Some(alert) if we should exit
    pub fn evaluate_sell(
        &self,
        mint: &str,
        trader: &str,
        token_amount: u64,
        sol_amount: f64,
        signature: &str,
    ) -> KillSwitchDecision {
        if !self.config.enabled {
            return KillSwitchDecision::Continue;
        }

        // Check 1: Is deployer selling?
        if self.config.deployer_sell_any && self.deployer_tracker.is_deployer(mint, trader) {
            warn!(
                mint = %mint,
                trader = %trader,
                amount = %token_amount,
                "KILL-SWITCH: DEPLOYER SELLING - EXIT NOW"
            );
            return KillSwitchDecision::Exit(KillSwitchAlert {
                alert_type: KillSwitchType::DeployerSell {
                    amount_tokens: token_amount,
                    amount_pct: 0.0, // TODO: Calculate from total supply
                },
                mint: mint.to_string(),
                urgency: KillSwitchUrgency::Immediate,
                reason: format!("Deployer {} sold {} tokens", trader, token_amount),
                auto_exit: true,
            });
        }

        // Check 2: Is top holder selling?
        if self.config.top_holder_sell {
            if let Some(alert) = self.holder_watcher.process_sell(
                trader,
                mint,
                token_amount,
                sol_amount,
                signature,
            ) {
                // Only trigger kill-switch on Critical alerts (top holder)
                if alert.urgency == AlertUrgency::Critical {
                    warn!(
                        mint = %mint,
                        holder = %trader,
                        pct_sold = %format!("{:.1}%", alert.pct_sold),
                        "KILL-SWITCH: TOP HOLDER SELLING - EXIT NOW"
                    );
                    return KillSwitchDecision::Exit(KillSwitchAlert {
                        alert_type: KillSwitchType::TopHolderSell {
                            holder: trader.to_string(),
                            rank: alert.holder_rank,
                            amount_pct: alert.pct_sold,
                        },
                        mint: mint.to_string(),
                        urgency: KillSwitchUrgency::Immediate,
                        reason: format!(
                            "Top holder #{} ({}) sold {:.1}% of position",
                            alert.holder_rank, trader, alert.pct_sold
                        ),
                        auto_exit: true,
                    });
                }
            }
        }

        // TODO: Check 3: Bundled wallets selling together
        // TODO: Check 4: Sniper exit before graduation

        KillSwitchDecision::Continue
    }

    /// Check if we should exit based on accumulated holder activity
    pub fn should_exit(&self, mint: &str) -> KillSwitchDecision {
        if !self.config.enabled {
            return KillSwitchDecision::Continue;
        }

        if let Some(alert) = self.holder_watcher.should_exit(mint) {
            return KillSwitchDecision::Exit(KillSwitchAlert {
                alert_type: KillSwitchType::TopHolderSell {
                    holder: alert.holder.clone(),
                    rank: alert.holder_rank,
                    amount_pct: alert.total_sold_pct,
                },
                mint: mint.to_string(),
                urgency: match alert.urgency {
                    AlertUrgency::Critical => KillSwitchUrgency::Immediate,
                    AlertUrgency::High => KillSwitchUrgency::High,
                    AlertUrgency::Medium => KillSwitchUrgency::Medium,
                },
                reason: format!(
                    "Holder #{} sold {:.1}% total - exit triggered",
                    alert.holder_rank, alert.total_sold_pct
                ),
                auto_exit: true,
            });
        }

        KillSwitchDecision::Continue
    }

    /// Get reference to holder watcher for direct access
    pub fn holder_watcher(&self) -> &HolderWatcher {
        &self.holder_watcher
    }

    /// Get reference to deployer tracker for direct access
    pub fn deployer_tracker(&self) -> &DeployerTracker {
        &self.deployer_tracker
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deployer_tracker() {
        let tracker = DeployerTracker::new();

        tracker.track("token1", "creator1");
        assert!(tracker.is_deployer("token1", "creator1"));
        assert!(!tracker.is_deployer("token1", "other"));
        assert!(!tracker.is_deployer("token2", "creator1"));

        tracker.untrack("token1");
        assert!(!tracker.is_deployer("token1", "creator1"));
    }

    #[test]
    fn test_kill_switch_disabled() {
        let config = KillSwitchConfig {
            enabled: false,
            ..Default::default()
        };
        let evaluator = KillSwitchEvaluator::new(config, HolderWatcherConfig::default());

        evaluator.deployer_tracker.track("token1", "deployer1");

        // Should return Continue when disabled
        match evaluator.evaluate_sell("token1", "deployer1", 1000, 1.0, "sig1") {
            KillSwitchDecision::Continue => (),
            KillSwitchDecision::Exit(_) => panic!("Should not trigger when disabled"),
        }
    }

    #[test]
    fn test_deployer_sell_trigger() {
        let config = KillSwitchConfig::default();
        let evaluator = KillSwitchEvaluator::new(config, HolderWatcherConfig::default());

        evaluator.deployer_tracker.track("token1", "deployer1");

        // Deployer selling should trigger exit
        match evaluator.evaluate_sell("token1", "deployer1", 1000, 1.0, "sig1") {
            KillSwitchDecision::Exit(alert) => {
                assert!(matches!(alert.alert_type, KillSwitchType::DeployerSell { .. }));
                assert_eq!(alert.urgency, KillSwitchUrgency::Immediate);
                assert!(alert.auto_exit);
            }
            KillSwitchDecision::Continue => panic!("Should trigger exit"),
        }
    }
}
