//! Wallet Clustering - Group wallets by funding relationships
//!
//! Clusters wallets based on:
//! - Common funding source (same SOL sender)
//! - Behavioral correlation (similar trading patterns)
//! - Known relationships (from bundled detection)
//!
//! Clusters are treated as single entities for sell detection:
//! If any wallet in a cluster sells, the entire cluster is flagged.

use chrono::{DateTime, Duration, Utc};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::Arc;
use tracing::{debug, info, warn};

use crate::filter::helius::HeliusClient;
use crate::filter::types::ClusterType;

/// Configuration for wallet clustering
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletClusterConfig {
    /// Enable clustering
    #[serde(default = "default_enabled")]
    pub enabled: bool,

    /// Lookback hours for funding relationships
    #[serde(default = "default_lookback_hours")]
    pub lookback_hours: u64,

    /// Minimum SOL amount to consider a funding relationship
    #[serde(default = "default_min_funding_sol")]
    pub min_funding_sol: f64,

    /// Maximum cluster size (prevent runaway growth)
    #[serde(default = "default_max_cluster_size")]
    pub max_cluster_size: usize,

    /// Cache TTL for cluster data (seconds)
    #[serde(default = "default_cache_ttl_secs")]
    pub cache_ttl_secs: u64,
}

fn default_enabled() -> bool {
    true
}
fn default_lookback_hours() -> u64 {
    48
}
fn default_min_funding_sol() -> f64 {
    0.1
}
fn default_max_cluster_size() -> usize {
    50
}
fn default_cache_ttl_secs() -> u64 {
    3600
}

impl Default for WalletClusterConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            lookback_hours: default_lookback_hours(),
            min_funding_sol: default_min_funding_sol(),
            max_cluster_size: default_max_cluster_size(),
            cache_ttl_secs: default_cache_ttl_secs(),
        }
    }
}

/// A cluster of related wallets
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletCluster {
    /// Unique cluster identifier (typically the funding source)
    pub cluster_id: String,
    /// All wallets in this cluster
    pub wallets: HashSet<String>,
    /// Cluster type classification
    pub cluster_type: ClusterType,
    /// Total volume across all wallets
    pub total_volume_sol: f64,
    /// Common funding sources
    pub funding_sources: Vec<String>,
    /// Behavioral correlation score (0.0 to 1.0)
    pub behavioral_correlation: f64,
    /// When this cluster was created/updated
    pub updated_at: DateTime<Utc>,
}

impl WalletCluster {
    /// Create a new cluster with a single wallet
    pub fn new(cluster_id: String, initial_wallet: String) -> Self {
        let mut wallets = HashSet::new();
        wallets.insert(initial_wallet);

        Self {
            cluster_id,
            wallets,
            cluster_type: ClusterType::Unknown,
            total_volume_sol: 0.0,
            funding_sources: Vec::new(),
            behavioral_correlation: 0.0,
            updated_at: Utc::now(),
        }
    }

    /// Add a wallet to the cluster
    pub fn add_wallet(&mut self, wallet: String) {
        self.wallets.insert(wallet);
        self.updated_at = Utc::now();
    }

    /// Check if wallet is in this cluster
    pub fn contains(&self, wallet: &str) -> bool {
        self.wallets.contains(wallet)
    }

    /// Get cluster size
    pub fn size(&self) -> usize {
        self.wallets.len()
    }
}

/// Wallet clustering engine
pub struct WalletClusterer {
    config: WalletClusterConfig,
    helius: Option<Arc<HeliusClient>>,
    /// Clusters by ID
    clusters: DashMap<String, WalletCluster>,
    /// Wallet -> cluster ID mapping for fast lookup
    wallet_to_cluster: DashMap<String, String>,
    /// Funding graph: source -> [funded wallets]
    funding_graph: DashMap<String, Vec<String>>,
}

impl WalletClusterer {
    /// Create a new wallet clusterer
    pub fn new(config: WalletClusterConfig, helius: Option<Arc<HeliusClient>>) -> Self {
        Self {
            config,
            helius,
            clusters: DashMap::new(),
            wallet_to_cluster: DashMap::new(),
            funding_graph: DashMap::new(),
        }
    }

    /// Find or create a cluster for a wallet
    pub async fn find_cluster(&self, wallet: &str) -> Option<WalletCluster> {
        if !self.config.enabled {
            return None;
        }

        // Check if already in a cluster
        if let Some(cluster_id) = self.wallet_to_cluster.get(wallet) {
            if let Some(cluster) = self.clusters.get(cluster_id.value()) {
                return Some(cluster.clone());
            }
        }

        // Try to find funding relationships
        if let Some(helius) = &self.helius {
            if let Some(cluster) = self.build_cluster_from_funding(wallet, helius).await {
                return Some(cluster);
            }
        }

        None
    }

    /// Build a cluster by analyzing funding relationships
    async fn build_cluster_from_funding(
        &self,
        wallet: &str,
        helius: &HeliusClient,
    ) -> Option<WalletCluster> {
        // Fetch funding transfers for this wallet
        let transfers = match helius.get_funding_transfers(wallet, 50).await {
            Ok(t) => t,
            Err(e) => {
                warn!(wallet = %&wallet[..8], error = %e, "Failed to fetch funding");
                return None;
            }
        };

        let cutoff = Utc::now() - Duration::hours(self.config.lookback_hours as i64);

        // Find significant funding sources
        let mut funding_sources: Vec<String> = Vec::new();
        for transfer in &transfers {
            if transfer.amount_sol >= self.config.min_funding_sol {
                if let Some(ts) = transfer.timestamp {
                    if ts > cutoff {
                        funding_sources.push(transfer.from.clone());

                        // Add to funding graph
                        self.funding_graph
                            .entry(transfer.from.clone())
                            .or_default()
                            .push(wallet.to_string());
                    }
                }
            }
        }

        if funding_sources.is_empty() {
            return None;
        }

        // Use the primary funding source as cluster ID
        let cluster_id = funding_sources[0].clone();

        // Check if this funding source already has a cluster
        if let Some(existing) = self.clusters.get(&cluster_id) {
            // Add wallet to existing cluster
            let mut cluster = existing.clone();
            if cluster.size() < self.config.max_cluster_size {
                cluster.add_wallet(wallet.to_string());
                self.wallet_to_cluster
                    .insert(wallet.to_string(), cluster_id.clone());
                self.clusters.insert(cluster_id.clone(), cluster.clone());

                debug!(
                    wallet = %&wallet[..8],
                    cluster = %&cluster_id[..8],
                    size = %cluster.size(),
                    "Wallet added to existing cluster"
                );

                return Some(cluster);
            }
        }

        // Create new cluster
        let mut cluster = WalletCluster::new(cluster_id.clone(), wallet.to_string());
        cluster.funding_sources = funding_sources;

        // Find other wallets funded by the same source
        if let Some(siblings) = self.funding_graph.get(&cluster_id) {
            for sibling in siblings.value() {
                if sibling != wallet && cluster.size() < self.config.max_cluster_size {
                    cluster.add_wallet(sibling.clone());
                    self.wallet_to_cluster
                        .insert(sibling.clone(), cluster_id.clone());
                }
            }
        }

        // Store the cluster
        self.wallet_to_cluster
            .insert(wallet.to_string(), cluster_id.clone());
        self.clusters.insert(cluster_id.clone(), cluster.clone());

        if cluster.size() > 1 {
            info!(
                cluster = %&cluster_id[..8],
                size = %cluster.size(),
                "New wallet cluster created"
            );
        }

        Some(cluster)
    }

    /// Check if two wallets are related (in same cluster or have common funding)
    pub fn are_related(&self, w1: &str, w2: &str) -> bool {
        // Same wallet
        if w1 == w2 {
            return true;
        }

        // Check cluster membership
        if let (Some(c1), Some(c2)) = (
            self.wallet_to_cluster.get(w1),
            self.wallet_to_cluster.get(w2),
        ) {
            if c1.value() == c2.value() {
                return true;
            }
        }

        // Check funding graph
        for entry in self.funding_graph.iter() {
            let funded_wallets = entry.value();
            if funded_wallets.contains(&w1.to_string()) && funded_wallets.contains(&w2.to_string())
            {
                return true;
            }
        }

        false
    }

    /// Get all wallets related to a given wallet
    pub fn get_related_wallets(&self, wallet: &str) -> Vec<String> {
        let mut related = Vec::new();

        // Get from cluster
        if let Some(cluster_id) = self.wallet_to_cluster.get(wallet) {
            if let Some(cluster) = self.clusters.get(cluster_id.value()) {
                related.extend(cluster.wallets.iter().cloned());
            }
        }

        // Get from funding graph
        for entry in self.funding_graph.iter() {
            let funded_wallets = entry.value();
            if funded_wallets.contains(&wallet.to_string()) {
                related.extend(funded_wallets.iter().cloned());
            }
        }

        // Deduplicate
        related.sort();
        related.dedup();
        related.retain(|w| w != wallet);

        related
    }

    /// Get cluster for a wallet (if exists)
    pub fn get_cluster(&self, wallet: &str) -> Option<WalletCluster> {
        self.wallet_to_cluster
            .get(wallet)
            .and_then(|id| self.clusters.get(id.value()).map(|c| c.clone()))
    }

    /// Manually add a relationship (e.g., from bundled detection)
    pub fn add_relationship(&self, source: &str, funded: &str) {
        // Add to funding graph
        self.funding_graph
            .entry(source.to_string())
            .or_default()
            .push(funded.to_string());

        // Update or create cluster
        if let Some(mut cluster) = self.clusters.get_mut(source) {
            if cluster.size() < self.config.max_cluster_size {
                cluster.add_wallet(funded.to_string());
                self.wallet_to_cluster
                    .insert(funded.to_string(), source.to_string());
            }
        } else {
            let mut cluster = WalletCluster::new(source.to_string(), source.to_string());
            cluster.add_wallet(funded.to_string());
            self.wallet_to_cluster
                .insert(source.to_string(), source.to_string());
            self.wallet_to_cluster
                .insert(funded.to_string(), source.to_string());
            self.clusters.insert(source.to_string(), cluster);
        }
    }

    /// Clear all clustering data
    pub fn clear(&self) {
        self.clusters.clear();
        self.wallet_to_cluster.clear();
        self.funding_graph.clear();
    }

    /// Get statistics
    pub fn stats(&self) -> ClusteringStats {
        let total_clusters = self.clusters.len();
        let total_wallets = self.wallet_to_cluster.len();
        let largest_cluster = self
            .clusters
            .iter()
            .map(|c| c.size())
            .max()
            .unwrap_or(0);

        ClusteringStats {
            total_clusters,
            total_wallets,
            largest_cluster,
        }
    }
}

/// Clustering statistics
#[derive(Debug, Clone)]
pub struct ClusteringStats {
    pub total_clusters: usize,
    pub total_wallets: usize,
    pub largest_cluster: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cluster_creation() {
        let mut cluster = WalletCluster::new("source1".to_string(), "wallet1".to_string());
        assert_eq!(cluster.size(), 1);
        assert!(cluster.contains("wallet1"));

        cluster.add_wallet("wallet2".to_string());
        assert_eq!(cluster.size(), 2);
        assert!(cluster.contains("wallet2"));
    }

    #[test]
    fn test_are_related_direct() {
        let config = WalletClusterConfig::default();
        let clusterer = WalletClusterer::new(config, None);

        // Add relationship manually
        clusterer.add_relationship("source1", "wallet1");
        clusterer.add_relationship("source1", "wallet2");

        assert!(clusterer.are_related("wallet1", "wallet2"));
        assert!(!clusterer.are_related("wallet1", "wallet3"));
    }

    #[test]
    fn test_get_related_wallets() {
        let config = WalletClusterConfig::default();
        let clusterer = WalletClusterer::new(config, None);

        clusterer.add_relationship("source1", "wallet1");
        clusterer.add_relationship("source1", "wallet2");
        clusterer.add_relationship("source1", "wallet3");

        let related = clusterer.get_related_wallets("wallet1");
        assert!(related.contains(&"wallet2".to_string()));
        assert!(related.contains(&"wallet3".to_string()));
    }
}
