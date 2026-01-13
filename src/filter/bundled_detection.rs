//! Bundled Wallet Detection
//!
//! Detects coordinated wallet activity (bundled wallets) that typically
//! indicates team/insider wallets that will dump together.
//!
//! Detection heuristics:
//! 1. Same-slot buys: 3+ wallets buying in the same Solana slot
//! 2. Identical amounts: Buy amounts within 1% variance
//! 3. Common funding: Same SOL source within 24h
//!
//! When bundled wallets sell together -> kill-switch

use chrono::{DateTime, Duration, Utc};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tracing::{debug, info, warn};

use crate::filter::helius::HeliusClient;

/// Configuration for bundled wallet detection
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundledDetectionConfig {
    /// Enabled flag
    #[serde(default = "default_enabled")]
    pub enabled: bool,

    /// Minimum wallets buying in same slot to flag as bundled
    #[serde(default = "default_same_slot_threshold")]
    pub same_slot_threshold: u32,

    /// Amount variance threshold for "identical amounts" (0.01 = 1%)
    #[serde(default = "default_amount_variance")]
    pub amount_variance: f64,

    /// Lookback hours for funding source detection
    #[serde(default = "default_funding_lookback_hours")]
    pub funding_lookback_hours: u64,

    /// Minimum common funding to flag as bundled
    #[serde(default = "default_common_funding_threshold")]
    pub common_funding_threshold: u32,

    /// Number of bundled wallets selling together to trigger exit
    #[serde(default = "default_sell_together_count")]
    pub sell_together_count: u32,

    /// Window in seconds for coordinated sell detection
    #[serde(default = "default_sell_window_secs")]
    pub sell_window_secs: u64,
}

fn default_enabled() -> bool {
    true
}
fn default_same_slot_threshold() -> u32 {
    3
}
fn default_amount_variance() -> f64 {
    0.01
}
fn default_funding_lookback_hours() -> u64 {
    24
}
fn default_common_funding_threshold() -> u32 {
    2
}
fn default_sell_together_count() -> u32 {
    2
}
fn default_sell_window_secs() -> u64 {
    30
}

impl Default for BundledDetectionConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            same_slot_threshold: default_same_slot_threshold(),
            amount_variance: default_amount_variance(),
            funding_lookback_hours: default_funding_lookback_hours(),
            common_funding_threshold: default_common_funding_threshold(),
            sell_together_count: default_sell_together_count(),
            sell_window_secs: default_sell_window_secs(),
        }
    }
}

/// An early buy on a token
#[derive(Debug, Clone)]
pub struct EarlyBuy {
    pub wallet: String,
    pub amount_sol: f64,
    pub slot: Option<u64>,
    pub timestamp: DateTime<Utc>,
    pub signature: String,
}

/// A detected bundle group
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleGroup {
    pub mint: String,
    pub wallets: Vec<String>,
    pub detection_reason: BundleDetectionReason,
    pub total_buy_sol: f64,
    pub detected_at: DateTime<Utc>,
    /// Recent sells by bundled wallets (for kill-switch)
    #[serde(skip)]
    pub recent_sells: Vec<BundledSell>,
}

/// Reason for bundle detection
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BundleDetectionReason {
    /// Multiple wallets bought in same slot
    SameSlotBuys { slot: u64, count: u32 },
    /// Multiple wallets bought identical amounts
    IdenticalAmounts { amount_sol: f64, count: u32, variance: f64 },
    /// Multiple wallets funded from same source
    CommonFunding { source: String, count: u32 },
    /// Multiple detection signals
    Multiple(Vec<BundleDetectionReason>),
}

/// A sell by a bundled wallet
#[derive(Debug, Clone)]
pub struct BundledSell {
    pub wallet: String,
    pub amount_sol: f64,
    pub timestamp: DateTime<Utc>,
    pub signature: String,
}

/// Bundle sell alert (for kill-switch)
#[derive(Debug, Clone)]
pub struct BundleSellAlert {
    pub mint: String,
    pub wallets_selling: u32,
    pub total_sell_sol: f64,
    pub window_secs: u64,
}

/// Bundled wallet detector
pub struct BundledDetector {
    config: BundledDetectionConfig,
    helius: Option<Arc<HeliusClient>>,
    /// Known bundles by mint
    known_bundles: DashMap<String, BundleGroup>,
    /// Funding sources cache: wallet -> [(source, amount, timestamp)]
    funding_cache: DashMap<String, Vec<(String, f64, DateTime<Utc>)>>,
}

impl BundledDetector {
    /// Create a new bundled detector
    pub fn new(config: BundledDetectionConfig, helius: Option<Arc<HeliusClient>>) -> Self {
        Self {
            config,
            helius,
            known_bundles: DashMap::new(),
            funding_cache: DashMap::new(),
        }
    }

    /// Analyze early buyers for coordinated behavior
    pub async fn analyze_early_buyers(
        &self,
        mint: &str,
        early_buys: &[EarlyBuy],
    ) -> Option<BundleGroup> {
        if !self.config.enabled || early_buys.len() < 2 {
            return None;
        }

        let mut reasons = Vec::new();

        // Check 1: Same-slot buys
        if let Some(reason) = self.check_same_slot_buys(early_buys) {
            reasons.push(reason);
        }

        // Check 2: Identical amounts
        if let Some(reason) = self.check_identical_amounts(early_buys) {
            reasons.push(reason);
        }

        // Check 3: Common funding (if Helius available)
        if self.helius.is_some() {
            if let Some(reason) = self.check_common_funding(early_buys).await {
                reasons.push(reason);
            }
        }

        if reasons.is_empty() {
            return None;
        }

        // Collect all suspicious wallets
        let mut bundled_wallets: HashSet<String> = HashSet::new();
        for buy in early_buys {
            bundled_wallets.insert(buy.wallet.clone());
        }

        let detection_reason = if reasons.len() == 1 {
            reasons.remove(0)
        } else {
            BundleDetectionReason::Multiple(reasons)
        };

        let total_buy_sol: f64 = early_buys.iter().map(|b| b.amount_sol).sum();

        let bundle = BundleGroup {
            mint: mint.to_string(),
            wallets: bundled_wallets.into_iter().collect(),
            detection_reason,
            total_buy_sol,
            detected_at: Utc::now(),
            recent_sells: Vec::new(),
        };

        info!(
            mint = %mint,
            wallets = %bundle.wallets.len(),
            reason = ?bundle.detection_reason,
            "Bundled wallets detected"
        );

        // Cache the bundle
        self.known_bundles.insert(mint.to_string(), bundle.clone());

        Some(bundle)
    }

    /// Check for same-slot buys
    fn check_same_slot_buys(&self, early_buys: &[EarlyBuy]) -> Option<BundleDetectionReason> {
        // Group buys by slot
        let mut by_slot: HashMap<u64, Vec<&EarlyBuy>> = HashMap::new();
        for buy in early_buys {
            if let Some(slot) = buy.slot {
                by_slot.entry(slot).or_default().push(buy);
            }
        }

        // Find slots with enough buys
        for (slot, buys) in by_slot {
            if buys.len() >= self.config.same_slot_threshold as usize {
                debug!(
                    slot = %slot,
                    count = %buys.len(),
                    "Same-slot bundle detected"
                );
                return Some(BundleDetectionReason::SameSlotBuys {
                    slot,
                    count: buys.len() as u32,
                });
            }
        }

        None
    }

    /// Check for identical buy amounts
    fn check_identical_amounts(&self, early_buys: &[EarlyBuy]) -> Option<BundleDetectionReason> {
        // Group by amount (with variance tolerance)
        let mut amount_groups: Vec<Vec<&EarlyBuy>> = Vec::new();

        for buy in early_buys {
            let mut found_group = false;
            for group in &mut amount_groups {
                if let Some(first) = group.first() {
                    let variance = (buy.amount_sol - first.amount_sol).abs() / first.amount_sol;
                    if variance <= self.config.amount_variance {
                        group.push(buy);
                        found_group = true;
                        break;
                    }
                }
            }
            if !found_group {
                amount_groups.push(vec![buy]);
            }
        }

        // Find groups with suspicious identical amounts
        for group in amount_groups {
            if group.len() >= 2 {
                let avg_amount: f64 = group.iter().map(|b| b.amount_sol).sum::<f64>() / group.len() as f64;
                let max_variance = group
                    .iter()
                    .map(|b| (b.amount_sol - avg_amount).abs() / avg_amount)
                    .fold(0.0_f64, f64::max);

                if max_variance <= self.config.amount_variance {
                    debug!(
                        amount = %format!("{:.4}", avg_amount),
                        count = %group.len(),
                        variance = %format!("{:.2}%", max_variance * 100.0),
                        "Identical amount bundle detected"
                    );
                    return Some(BundleDetectionReason::IdenticalAmounts {
                        amount_sol: avg_amount,
                        count: group.len() as u32,
                        variance: max_variance,
                    });
                }
            }
        }

        None
    }

    /// Check for common funding source
    async fn check_common_funding(&self, early_buys: &[EarlyBuy]) -> Option<BundleDetectionReason> {
        let helius = self.helius.as_ref()?;

        // Fetch funding for each wallet (with cache)
        let mut funding_sources: HashMap<String, Vec<String>> = HashMap::new(); // source -> [wallets]

        for buy in early_buys {
            let sources = self.get_funding_sources(&buy.wallet, helius).await;
            for source in sources {
                funding_sources
                    .entry(source)
                    .or_default()
                    .push(buy.wallet.clone());
            }
        }

        // Find sources funding multiple wallets
        for (source, wallets) in funding_sources {
            if wallets.len() >= self.config.common_funding_threshold as usize {
                debug!(
                    source = %&source[..8],
                    wallets = %wallets.len(),
                    "Common funding bundle detected"
                );
                return Some(BundleDetectionReason::CommonFunding {
                    source,
                    count: wallets.len() as u32,
                });
            }
        }

        None
    }

    /// Get funding sources for a wallet (cached)
    async fn get_funding_sources(&self, wallet: &str, helius: &HeliusClient) -> Vec<String> {
        // Check cache first
        if let Some(cached) = self.funding_cache.get(wallet) {
            let cutoff = Utc::now() - Duration::hours(self.config.funding_lookback_hours as i64);
            let recent: Vec<String> = cached
                .iter()
                .filter(|(_, _, ts)| *ts > cutoff)
                .map(|(source, _, _)| source.clone())
                .collect();
            if !recent.is_empty() {
                return recent;
            }
        }

        // Fetch from Helius
        match helius.get_funding_transfers(wallet, 50).await {
            Ok(transfers) => {
                let cutoff = Utc::now() - Duration::hours(self.config.funding_lookback_hours as i64);
                let sources: Vec<(String, f64, DateTime<Utc>)> = transfers
                    .into_iter()
                    .filter(|t| t.timestamp.map(|ts| ts > cutoff).unwrap_or(false))
                    .map(|t| (t.from.clone(), t.amount_sol, t.timestamp.unwrap_or_else(Utc::now)))
                    .collect();

                // Cache the results
                let source_addrs: Vec<String> = sources.iter().map(|(s, _, _)| s.clone()).collect();
                self.funding_cache.insert(wallet.to_string(), sources);

                source_addrs
            }
            Err(e) => {
                warn!(wallet = %&wallet[..8], error = %e, "Failed to fetch funding transfers");
                Vec::new()
            }
        }
    }

    /// Check if a wallet is part of a known bundle for this token
    pub fn is_bundled(&self, mint: &str, wallet: &str) -> bool {
        self.known_bundles
            .get(mint)
            .map(|bundle| bundle.wallets.contains(&wallet.to_string()))
            .unwrap_or(false)
    }

    /// Get bundle group for a token
    pub fn get_bundle(&self, mint: &str) -> Option<BundleGroup> {
        self.known_bundles.get(mint).map(|r| r.clone())
    }

    /// Record a sell by a potentially bundled wallet
    /// Returns Some(alert) if enough bundled wallets have sold together
    pub fn record_sell(
        &self,
        mint: &str,
        wallet: &str,
        amount_sol: f64,
        signature: &str,
    ) -> Option<BundleSellAlert> {
        if !self.config.enabled {
            return None;
        }

        // Check if wallet is part of a bundle
        let mut bundle = self.known_bundles.get_mut(mint)?;
        if !bundle.wallets.contains(&wallet.to_string()) {
            return None;
        }

        // Record the sell
        let sell = BundledSell {
            wallet: wallet.to_string(),
            amount_sol,
            timestamp: Utc::now(),
            signature: signature.to_string(),
        };
        bundle.recent_sells.push(sell);

        // Clean old sells outside window
        let cutoff = Utc::now() - Duration::seconds(self.config.sell_window_secs as i64);
        bundle.recent_sells.retain(|s| s.timestamp > cutoff);

        // Count unique wallets selling in window
        let unique_sellers: HashSet<_> = bundle.recent_sells.iter().map(|s| &s.wallet).collect();
        let wallets_selling = unique_sellers.len() as u32;

        debug!(
            mint = %mint,
            wallet = %&wallet[..8],
            wallets_selling = %wallets_selling,
            threshold = %self.config.sell_together_count,
            "Bundled wallet sell recorded"
        );

        // Check if enough have sold together
        if wallets_selling >= self.config.sell_together_count {
            let total_sell_sol: f64 = bundle.recent_sells.iter().map(|s| s.amount_sol).sum();

            warn!(
                mint = %mint,
                wallets_selling = %wallets_selling,
                total_sol = %format!("{:.4}", total_sell_sol),
                "BUNDLE SELL ALERT - Multiple bundled wallets selling together"
            );

            return Some(BundleSellAlert {
                mint: mint.to_string(),
                wallets_selling,
                total_sell_sol,
                window_secs: self.config.sell_window_secs,
            });
        }

        None
    }

    /// Stop tracking a token
    pub fn untrack(&self, mint: &str) {
        self.known_bundles.remove(mint);
    }

    /// Clear all tracked bundles
    pub fn clear(&self) {
        self.known_bundles.clear();
        self.funding_cache.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_same_slot_detection() {
        let config = BundledDetectionConfig::default();
        let detector = BundledDetector::new(config, None);

        let now = Utc::now();
        let early_buys = vec![
            EarlyBuy {
                wallet: "wallet1".to_string(),
                amount_sol: 1.0,
                slot: Some(12345),
                timestamp: now,
                signature: "sig1".to_string(),
            },
            EarlyBuy {
                wallet: "wallet2".to_string(),
                amount_sol: 1.0,
                slot: Some(12345),
                timestamp: now,
                signature: "sig2".to_string(),
            },
            EarlyBuy {
                wallet: "wallet3".to_string(),
                amount_sol: 1.0,
                slot: Some(12345),
                timestamp: now,
                signature: "sig3".to_string(),
            },
        ];

        let reason = detector.check_same_slot_buys(&early_buys);
        assert!(reason.is_some());
        match reason.unwrap() {
            BundleDetectionReason::SameSlotBuys { slot, count } => {
                assert_eq!(slot, 12345);
                assert_eq!(count, 3);
            }
            _ => panic!("Expected SameSlotBuys"),
        }
    }

    #[test]
    fn test_identical_amount_detection() {
        let config = BundledDetectionConfig::default();
        let detector = BundledDetector::new(config, None);

        let now = Utc::now();
        let early_buys = vec![
            EarlyBuy {
                wallet: "wallet1".to_string(),
                amount_sol: 1.0,
                slot: Some(1),
                timestamp: now,
                signature: "sig1".to_string(),
            },
            EarlyBuy {
                wallet: "wallet2".to_string(),
                amount_sol: 1.005, // Within 1% of 1.0
                slot: Some(2),
                timestamp: now,
                signature: "sig2".to_string(),
            },
        ];

        let reason = detector.check_identical_amounts(&early_buys);
        assert!(reason.is_some());
        match reason.unwrap() {
            BundleDetectionReason::IdenticalAmounts { count, .. } => {
                assert_eq!(count, 2);
            }
            _ => panic!("Expected IdenticalAmounts"),
        }
    }

    #[test]
    fn test_is_bundled() {
        let config = BundledDetectionConfig::default();
        let detector = BundledDetector::new(config, None);

        // Manually insert a bundle
        detector.known_bundles.insert(
            "token1".to_string(),
            BundleGroup {
                mint: "token1".to_string(),
                wallets: vec!["wallet1".to_string(), "wallet2".to_string()],
                detection_reason: BundleDetectionReason::SameSlotBuys { slot: 1, count: 2 },
                total_buy_sol: 2.0,
                detected_at: Utc::now(),
                recent_sells: Vec::new(),
            },
        );

        assert!(detector.is_bundled("token1", "wallet1"));
        assert!(detector.is_bundled("token1", "wallet2"));
        assert!(!detector.is_bundled("token1", "wallet3"));
        assert!(!detector.is_bundled("token2", "wallet1"));
    }
}
