//! Holder Watcher - Real-time monitoring of top holders
//!
//! The key to survival: detect when top holders start selling and EXIT FIRST.
//! This module watches holder wallets and triggers immediate exits when they dump.

use std::collections::{HashMap, HashSet};
use std::sync::RwLock;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

/// Configuration for holder watching
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HolderWatcherConfig {
    /// Number of top holders to watch per token
    #[serde(default = "default_holders_to_watch")]
    pub holders_to_watch: usize,

    /// Minimum holding % to be considered a "top holder" worth watching
    #[serde(default = "default_min_holding_pct")]
    pub min_holding_pct: f64,

    /// Trigger exit if top holder sells more than this % of their position
    #[serde(default = "default_exit_threshold_pct")]
    pub exit_threshold_pct: f64,

    /// Trigger exit if ANY top holder sells (regardless of amount)
    #[serde(default = "default_exit_on_any_sell")]
    pub exit_on_any_sell: bool,

    /// How long to track a holder's pattern after they sell
    #[serde(default = "default_pattern_tracking_mins")]
    pub pattern_tracking_mins: u64,
}

fn default_holders_to_watch() -> usize { 10 }
fn default_min_holding_pct() -> f64 { 2.0 }
fn default_exit_threshold_pct() -> f64 { 10.0 }
fn default_exit_on_any_sell() -> bool { true }
fn default_pattern_tracking_mins() -> u64 { 30 }

impl Default for HolderWatcherConfig {
    fn default() -> Self {
        Self {
            holders_to_watch: default_holders_to_watch(),
            min_holding_pct: default_min_holding_pct(),
            exit_threshold_pct: default_exit_threshold_pct(),
            exit_on_any_sell: default_exit_on_any_sell(),
            pattern_tracking_mins: default_pattern_tracking_mins(),
        }
    }
}

/// A holder being watched for a specific token
#[derive(Debug, Clone)]
pub struct WatchedHolder {
    /// Holder wallet address
    pub address: String,
    /// Token mint they hold
    pub mint: String,
    /// Original holding amount when we started watching
    pub original_amount: u64,
    /// Original holding percentage
    pub original_pct: f64,
    /// Current estimated amount (updated on sells)
    pub current_amount: u64,
    /// When we started watching
    pub watch_started: DateTime<Utc>,
    /// Sells detected from this holder
    pub sells: Vec<HolderSell>,
}

/// A sell event from a watched holder
#[derive(Debug, Clone)]
pub struct HolderSell {
    pub timestamp: DateTime<Utc>,
    pub amount_sold: u64,
    pub pct_of_holdings: f64,
    pub sol_received: f64,
    pub signature: String,
}

/// Alert when a top holder sells
#[derive(Debug, Clone)]
pub struct HolderSellAlert {
    pub mint: String,
    pub holder: String,
    pub holder_rank: usize,  // 1 = top holder, 2 = second, etc.
    pub original_pct: f64,
    pub amount_sold: u64,
    pub pct_sold: f64,       // % of their holdings sold
    pub total_sold_pct: f64, // Total % sold across all sells
    pub is_first_sell: bool,
    pub urgency: AlertUrgency,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertUrgency {
    /// Top holder selling - EXIT NOW
    Critical,
    /// Significant holder selling - consider exit
    High,
    /// Minor holder selling - monitor
    Medium,
}

/// Holder sell pattern tracking
#[derive(Debug, Clone, Default)]
pub struct HolderPattern {
    pub address: String,
    /// Tokens this holder has dumped
    pub tokens_dumped: Vec<DumpRecord>,
    /// Average time from token creation to first sell
    pub avg_time_to_dump_secs: Option<f64>,
    /// Do they sell all at once or in chunks?
    pub sells_in_chunks: bool,
    /// Average chunk size if they chunk
    pub avg_chunk_pct: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct DumpRecord {
    pub mint: String,
    pub timestamp: DateTime<Utc>,
    pub time_held_secs: u64,
    pub pct_sold: f64,
    pub num_sells: u32,
}

/// Main holder watcher
pub struct HolderWatcher {
    config: HolderWatcherConfig,

    /// Holders being watched: mint -> list of watched holders
    watched: RwLock<HashMap<String, Vec<WatchedHolder>>>,

    /// All watched holder addresses (for quick lookup when trade comes in)
    watched_addresses: RwLock<HashSet<String>>,

    /// Holder patterns we've learned
    patterns: RwLock<HashMap<String, HolderPattern>>,

    /// Pending alerts
    alerts: RwLock<Vec<HolderSellAlert>>,
}

impl HolderWatcher {
    pub fn new(config: HolderWatcherConfig) -> Self {
        Self {
            config,
            watched: RwLock::new(HashMap::new()),
            watched_addresses: RwLock::new(HashSet::new()),
            patterns: RwLock::new(HashMap::new()),
            alerts: RwLock::new(Vec::new()),
        }
    }

    /// Start watching holders for a token we just entered
    pub fn watch_token(&self, mint: &str, holders: Vec<(String, u64, f64)>) {
        let mut watched = self.watched.write().unwrap();
        let mut addresses = self.watched_addresses.write().unwrap();

        let now = Utc::now();
        let mut token_holders = Vec::new();

        for (address, amount, pct) in holders.into_iter().take(self.config.holders_to_watch) {
            if pct >= self.config.min_holding_pct {
                info!(
                    mint = %mint,
                    holder = %address,
                    pct = %format!("{:.2}%", pct),
                    "Watching holder"
                );

                addresses.insert(address.clone());
                token_holders.push(WatchedHolder {
                    address,
                    mint: mint.to_string(),
                    original_amount: amount,
                    original_pct: pct,
                    current_amount: amount,
                    watch_started: now,
                    sells: Vec::new(),
                });
            }
        }

        if !token_holders.is_empty() {
            info!(
                mint = %mint,
                count = token_holders.len(),
                "Started watching top holders"
            );
            watched.insert(mint.to_string(), token_holders);
        }
    }

    /// Stop watching holders for a token (we exited the position)
    pub fn unwatch_token(&self, mint: &str) {
        let mut watched = self.watched.write().unwrap();
        let mut addresses = self.watched_addresses.write().unwrap();

        if let Some(holders) = watched.remove(mint) {
            // Update patterns before removing
            self.update_patterns_on_exit(&holders);

            // Remove addresses (but only if not watching same address for another token)
            let still_watching: HashSet<_> = watched.values()
                .flat_map(|h| h.iter().map(|wh| wh.address.clone()))
                .collect();

            for holder in holders {
                if !still_watching.contains(&holder.address) {
                    addresses.remove(&holder.address);
                }
            }

            info!(mint = %mint, "Stopped watching holders");
        }
    }

    /// Check if an address is being watched
    pub fn is_watched(&self, address: &str) -> bool {
        self.watched_addresses.read().unwrap().contains(address)
    }

    /// Get all addresses we're watching (for subscribing to trade events)
    pub fn get_watched_addresses(&self) -> Vec<String> {
        self.watched_addresses.read().unwrap().iter().cloned().collect()
    }

    /// Process a sell event - returns alert if this is a watched holder selling
    pub fn process_sell(
        &self,
        trader: &str,
        mint: &str,
        token_amount: u64,
        sol_amount: f64,
        signature: &str,
    ) -> Option<HolderSellAlert> {
        // Quick check if this trader is watched at all
        if !self.is_watched(trader) {
            return None;
        }

        let mut watched = self.watched.write().unwrap();
        let holders = watched.get_mut(mint)?;

        // Find this holder
        let holder_idx = holders.iter().position(|h| h.address == trader)?;
        let holder = &mut holders[holder_idx];

        // Calculate sell percentage
        let pct_sold = if holder.current_amount > 0 {
            (token_amount as f64 / holder.current_amount as f64) * 100.0
        } else {
            100.0
        };

        // Record the sell
        let sell = HolderSell {
            timestamp: Utc::now(),
            amount_sold: token_amount,
            pct_of_holdings: pct_sold,
            sol_received: sol_amount,
            signature: signature.to_string(),
        };
        holder.sells.push(sell);
        holder.current_amount = holder.current_amount.saturating_sub(token_amount);

        // Calculate total sold
        let total_sold: u64 = holder.sells.iter().map(|s| s.amount_sold).sum();
        let total_sold_pct = if holder.original_amount > 0 {
            (total_sold as f64 / holder.original_amount as f64) * 100.0
        } else {
            100.0
        };

        // Determine urgency
        let urgency = if holder_idx == 0 {
            AlertUrgency::Critical  // TOP holder selling
        } else if holder_idx < 3 || holder.original_pct > 10.0 {
            AlertUrgency::High
        } else {
            AlertUrgency::Medium
        };

        let alert = HolderSellAlert {
            mint: mint.to_string(),
            holder: trader.to_string(),
            holder_rank: holder_idx + 1,
            original_pct: holder.original_pct,
            amount_sold: token_amount,
            pct_sold,
            total_sold_pct,
            is_first_sell: holder.sells.len() == 1,
            urgency,
            timestamp: Utc::now(),
        };

        // Log based on urgency
        match urgency {
            AlertUrgency::Critical => {
                warn!(
                    mint = %mint,
                    holder = %trader,
                    pct_sold = %format!("{:.1}%", pct_sold),
                    total_sold = %format!("{:.1}%", total_sold_pct),
                    "CRITICAL: TOP HOLDER SELLING - EXIT NOW"
                );
            }
            AlertUrgency::High => {
                warn!(
                    mint = %mint,
                    holder = %trader,
                    rank = holder_idx + 1,
                    pct_sold = %format!("{:.1}%", pct_sold),
                    "HIGH: Major holder selling"
                );
            }
            AlertUrgency::Medium => {
                info!(
                    mint = %mint,
                    holder = %trader,
                    pct_sold = %format!("{:.1}%", pct_sold),
                    "Holder selling"
                );
            }
        }

        // Store alert
        self.alerts.write().unwrap().push(alert.clone());

        Some(alert)
    }

    /// Check if we should exit based on holder activity
    pub fn should_exit(&self, mint: &str) -> Option<HolderSellAlert> {
        let watched = self.watched.read().unwrap();
        let holders = watched.get(mint)?;

        for (idx, holder) in holders.iter().enumerate() {
            if holder.sells.is_empty() {
                continue;
            }

            let total_sold: u64 = holder.sells.iter().map(|s| s.amount_sold).sum();
            let total_sold_pct = if holder.original_amount > 0 {
                (total_sold as f64 / holder.original_amount as f64) * 100.0
            } else {
                100.0
            };

            // Exit if configured to exit on any sell
            if self.config.exit_on_any_sell {
                return Some(HolderSellAlert {
                    mint: mint.to_string(),
                    holder: holder.address.clone(),
                    holder_rank: idx + 1,
                    original_pct: holder.original_pct,
                    amount_sold: total_sold,
                    pct_sold: total_sold_pct,
                    total_sold_pct,
                    is_first_sell: false,
                    urgency: if idx == 0 { AlertUrgency::Critical } else { AlertUrgency::High },
                    timestamp: holder.sells.last().map(|s| s.timestamp).unwrap_or_else(Utc::now),
                });
            }

            // Exit if sold more than threshold
            if total_sold_pct >= self.config.exit_threshold_pct {
                return Some(HolderSellAlert {
                    mint: mint.to_string(),
                    holder: holder.address.clone(),
                    holder_rank: idx + 1,
                    original_pct: holder.original_pct,
                    amount_sold: total_sold,
                    pct_sold: total_sold_pct,
                    total_sold_pct,
                    is_first_sell: false,
                    urgency: AlertUrgency::High,
                    timestamp: holder.sells.last().map(|s| s.timestamp).unwrap_or_else(Utc::now),
                });
            }
        }

        None
    }

    /// Get and clear pending alerts
    pub fn take_alerts(&self) -> Vec<HolderSellAlert> {
        std::mem::take(&mut *self.alerts.write().unwrap())
    }

    /// Update pattern tracking when we exit a position
    fn update_patterns_on_exit(&self, holders: &[WatchedHolder]) {
        let mut patterns = self.patterns.write().unwrap();

        for holder in holders {
            if holder.sells.is_empty() {
                continue;
            }

            let pattern = patterns.entry(holder.address.clone())
                .or_insert_with(|| HolderPattern {
                    address: holder.address.clone(),
                    ..Default::default()
                });

            // Record this dump
            let total_sold: u64 = holder.sells.iter().map(|s| s.amount_sold).sum();
            let pct_sold = if holder.original_amount > 0 {
                (total_sold as f64 / holder.original_amount as f64) * 100.0
            } else {
                100.0
            };

            let time_held = holder.sells.first()
                .map(|s| (s.timestamp - holder.watch_started).num_seconds().max(0) as u64)
                .unwrap_or(0);

            pattern.tokens_dumped.push(DumpRecord {
                mint: holder.mint.clone(),
                timestamp: Utc::now(),
                time_held_secs: time_held,
                pct_sold,
                num_sells: holder.sells.len() as u32,
            });

            // Update averages
            if !pattern.tokens_dumped.is_empty() {
                let avg_time: f64 = pattern.tokens_dumped.iter()
                    .map(|d| d.time_held_secs as f64)
                    .sum::<f64>() / pattern.tokens_dumped.len() as f64;
                pattern.avg_time_to_dump_secs = Some(avg_time);

                pattern.sells_in_chunks = pattern.tokens_dumped.iter()
                    .any(|d| d.num_sells > 1);
            }

            debug!(
                holder = %holder.address,
                tokens_dumped = pattern.tokens_dumped.len(),
                avg_time = ?pattern.avg_time_to_dump_secs,
                "Updated holder pattern"
            );
        }
    }

    /// Get pattern for a holder (useful for prediction)
    pub fn get_pattern(&self, address: &str) -> Option<HolderPattern> {
        self.patterns.read().unwrap().get(address).cloned()
    }

    /// Check if a holder has a history of dumping
    pub fn is_known_dumper(&self, address: &str) -> Option<(usize, f64)> {
        let patterns = self.patterns.read().unwrap();
        let pattern = patterns.get(address)?;

        if pattern.tokens_dumped.len() >= 2 {
            let avg_time = pattern.avg_time_to_dump_secs.unwrap_or(0.0);
            Some((pattern.tokens_dumped.len(), avg_time))
        } else {
            None
        }
    }

    /// Get statistics
    pub fn stats(&self) -> HolderWatcherStats {
        let watched = self.watched.read().unwrap();
        let addresses = self.watched_addresses.read().unwrap();
        let patterns = self.patterns.read().unwrap();

        HolderWatcherStats {
            tokens_watched: watched.len(),
            total_holders_watched: addresses.len(),
            known_patterns: patterns.len(),
            known_dumpers: patterns.values().filter(|p| p.tokens_dumped.len() >= 2).count(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct HolderWatcherStats {
    pub tokens_watched: usize,
    pub total_holders_watched: usize,
    pub known_patterns: usize,
    pub known_dumpers: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_watch_and_detect_sell() {
        let watcher = HolderWatcher::new(HolderWatcherConfig::default());

        // Watch a token with holders
        let holders = vec![
            ("holder1".to_string(), 1000000, 50.0),  // 50% holder
            ("holder2".to_string(), 500000, 25.0),   // 25% holder
            ("holder3".to_string(), 200000, 10.0),   // 10% holder
        ];
        watcher.watch_token("token1", holders);

        assert!(watcher.is_watched("holder1"));
        assert!(watcher.is_watched("holder2"));
        assert_eq!(watcher.get_watched_addresses().len(), 3);

        // Simulate top holder selling
        let alert = watcher.process_sell(
            "holder1",
            "token1",
            500000,  // Selling half
            5.0,     // For 5 SOL
            "sig123",
        );

        assert!(alert.is_some());
        let alert = alert.unwrap();
        assert_eq!(alert.holder_rank, 1);
        assert_eq!(alert.urgency, AlertUrgency::Critical);
        assert!((alert.pct_sold - 50.0).abs() < 0.1);
    }

    #[test]
    fn test_should_exit_on_any_sell() {
        let config = HolderWatcherConfig {
            exit_on_any_sell: true,
            ..Default::default()
        };
        let watcher = HolderWatcher::new(config);

        let holders = vec![
            ("holder1".to_string(), 1000000, 50.0),
        ];
        watcher.watch_token("token1", holders);

        // Before sell - should not exit
        assert!(watcher.should_exit("token1").is_none());

        // After sell - should exit
        watcher.process_sell("holder1", "token1", 100000, 1.0, "sig1");
        assert!(watcher.should_exit("token1").is_some());
    }

    #[test]
    fn test_pattern_tracking() {
        let watcher = HolderWatcher::new(HolderWatcherConfig::default());

        // First token
        let holders = vec![("dumper".to_string(), 1000000, 50.0)];
        watcher.watch_token("token1", holders);
        watcher.process_sell("dumper", "token1", 1000000, 10.0, "sig1");
        watcher.unwatch_token("token1");

        // Second token - same dumper
        let holders = vec![("dumper".to_string(), 2000000, 40.0)];
        watcher.watch_token("token2", holders);
        watcher.process_sell("dumper", "token2", 2000000, 20.0, "sig2");
        watcher.unwatch_token("token2");

        // Should now be known as a dumper
        let result = watcher.is_known_dumper("dumper");
        assert!(result.is_some());
        assert_eq!(result.unwrap().0, 2); // Dumped 2 tokens
    }
}
