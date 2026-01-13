//! Caching layer for the adaptive filtering system
//!
//! Provides fast access to pre-computed data that would be too slow
//! to fetch during the hot path.

use dashmap::DashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

use crate::filter::helius::MintInfo;
use crate::filter::types::{TokenHolderInfo, WalletHistory};

// Submodules for specific cache types
// pub mod known_actors;
// pub mod wallet_cache;
// pub mod trade_flow;

/// Configuration for the cache system
#[derive(Debug, Clone)]
pub struct CacheConfig {
    /// Maximum entries in wallet cache
    pub wallet_cache_size: usize,
    /// TTL for wallet cache entries (seconds)
    pub wallet_cache_ttl_secs: u64,
    /// Maximum entries in score cache
    pub score_cache_size: usize,
    /// TTL for score cache entries (seconds)
    pub score_cache_ttl_secs: u64,
    /// Size of trade flow buffer per token
    pub trade_flow_buffer_size: usize,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            wallet_cache_size: 10_000,
            wallet_cache_ttl_secs: 3600, // 1 hour
            score_cache_size: 1_000,
            score_cache_ttl_secs: 300, // 5 minutes
            trade_flow_buffer_size: 1_000,
        }
    }
}

/// Entry in the wallet cache with TTL
#[derive(Clone)]
pub struct CachedWallet {
    pub history: WalletHistory,
    pub cached_at: Instant,
    pub ttl: Duration,
}

impl CachedWallet {
    pub fn new(history: WalletHistory, ttl: Duration) -> Self {
        Self {
            history,
            cached_at: Instant::now(),
            ttl,
        }
    }

    pub fn is_expired(&self) -> bool {
        self.cached_at.elapsed() > self.ttl
    }
}

/// Entry in the holder cache with TTL
#[derive(Clone)]
pub struct CachedHolders {
    pub holders: Vec<TokenHolderInfo>,
    pub cached_at: Instant,
    pub ttl: Duration,
}

impl CachedHolders {
    pub fn new(holders: Vec<TokenHolderInfo>, ttl: Duration) -> Self {
        Self {
            holders,
            cached_at: Instant::now(),
            ttl,
        }
    }

    pub fn is_expired(&self) -> bool {
        self.cached_at.elapsed() > self.ttl
    }
}

/// Entry in the mint info cache with TTL
#[derive(Clone)]
pub struct CachedMintInfo {
    pub info: MintInfo,
    pub cached_at: Instant,
    pub ttl: Duration,
}

impl CachedMintInfo {
    pub fn new(info: MintInfo, ttl: Duration) -> Self {
        Self {
            info,
            cached_at: Instant::now(),
            ttl,
        }
    }

    pub fn is_expired(&self) -> bool {
        self.cached_at.elapsed() > self.ttl
    }
}

/// Known actors (deployers, snipers, trusted wallets)
#[derive(Default)]
pub struct KnownActors {
    /// Known rug deployer addresses
    pub deployers: HashSet<String>,
    /// Known sniper bot addresses
    pub snipers: HashSet<String>,
    /// Trusted wallets for copy-trading
    pub trusted: HashSet<String>,
    /// Last refresh time
    pub last_refresh: Option<Instant>,
}

impl KnownActors {
    /// Check if a wallet is a known deployer
    pub fn is_known_deployer(&self, address: &str) -> bool {
        self.deployers.contains(address)
    }

    /// Check if a wallet is a known sniper
    pub fn is_known_sniper(&self, address: &str) -> bool {
        self.snipers.contains(address)
    }

    /// Check if a wallet is trusted
    pub fn is_trusted(&self, address: &str) -> bool {
        self.trusted.contains(address)
    }

    /// Add a deployer to the blacklist
    pub fn add_deployer(&mut self, address: String) {
        self.deployers.insert(address);
    }

    /// Add a sniper to the list
    pub fn add_sniper(&mut self, address: String) {
        self.snipers.insert(address);
    }

    /// Add a trusted wallet
    pub fn add_trusted(&mut self, address: String) {
        self.trusted.insert(address);
    }

    /// Load from files
    pub fn load_from_files(
        deployers_path: Option<&str>,
        snipers_path: Option<&str>,
        trusted_path: Option<&str>,
    ) -> Self {
        let mut actors = Self::default();

        if let Some(path) = deployers_path {
            if let Ok(content) = std::fs::read_to_string(path) {
                for line in content.lines() {
                    let addr = line.trim();
                    if !addr.is_empty() && !addr.starts_with('#') {
                        actors.deployers.insert(addr.to_string());
                    }
                }
            }
        }

        if let Some(path) = snipers_path {
            if let Ok(content) = std::fs::read_to_string(path) {
                for line in content.lines() {
                    let addr = line.trim();
                    if !addr.is_empty() && !addr.starts_with('#') {
                        actors.snipers.insert(addr.to_string());
                    }
                }
            }
        }

        if let Some(path) = trusted_path {
            if let Ok(content) = std::fs::read_to_string(path) {
                for line in content.lines() {
                    let addr = line.trim();
                    if !addr.is_empty() && !addr.starts_with('#') {
                        actors.trusted.insert(addr.to_string());
                    }
                }
            }
        }

        actors.last_refresh = Some(Instant::now());
        actors
    }

    /// Get statistics
    pub fn stats(&self) -> (usize, usize, usize) {
        (self.deployers.len(), self.snipers.len(), self.trusted.len())
    }
}

/// Multi-tier caching system for the adaptive filter
pub struct FilterCache {
    /// Configuration
    config: CacheConfig,

    /// Wallet data cache (concurrent hashmap)
    wallet_cache: DashMap<String, CachedWallet>,

    /// Token holder cache (mint -> holders)
    holder_cache: DashMap<String, CachedHolders>,

    /// Mint info cache (mint -> mint authority info)
    mint_info_cache: DashMap<String, CachedMintInfo>,

    /// Known actors (loaded at startup, refreshed periodically)
    known_actors: Arc<RwLock<KnownActors>>,

    /// Cache statistics
    stats: Arc<CacheStats>,
}

/// Cache statistics for monitoring
#[derive(Default)]
pub struct CacheStats {
    pub wallet_hits: std::sync::atomic::AtomicU64,
    pub wallet_misses: std::sync::atomic::AtomicU64,
    pub known_actor_checks: std::sync::atomic::AtomicU64,
}

impl CacheStats {
    pub fn record_wallet_hit(&self) {
        self.wallet_hits
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn record_wallet_miss(&self) {
        self.wallet_misses
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn record_known_actor_check(&self) {
        self.known_actor_checks
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn hit_rate(&self) -> f64 {
        let hits = self.wallet_hits.load(std::sync::atomic::Ordering::Relaxed);
        let misses = self
            .wallet_misses
            .load(std::sync::atomic::Ordering::Relaxed);
        let total = hits + misses;
        if total == 0 {
            0.0
        } else {
            hits as f64 / total as f64
        }
    }
}

impl FilterCache {
    /// Create a new cache with default configuration
    pub fn new() -> Self {
        Self::with_config(CacheConfig::default())
    }

    /// Create a new cache with custom configuration
    pub fn with_config(config: CacheConfig) -> Self {
        Self {
            wallet_cache: DashMap::with_capacity(config.wallet_cache_size),
            holder_cache: DashMap::with_capacity(config.score_cache_size),
            mint_info_cache: DashMap::with_capacity(config.score_cache_size),
            known_actors: Arc::new(RwLock::new(KnownActors::default())),
            stats: Arc::new(CacheStats::default()),
            config,
        }
    }

    /// Get wallet history from cache
    pub fn get_wallet(&self, address: &str) -> Option<WalletHistory> {
        if let Some(entry) = self.wallet_cache.get(address) {
            if !entry.is_expired() {
                self.stats.record_wallet_hit();
                return Some(entry.history.clone());
            }
            // Entry expired, remove it
            drop(entry);
            self.wallet_cache.remove(address);
        }
        self.stats.record_wallet_miss();
        None
    }

    /// Store wallet history in cache
    pub fn set_wallet(&self, address: &str, history: WalletHistory) {
        let ttl = Duration::from_secs(self.config.wallet_cache_ttl_secs);
        let entry = CachedWallet::new(history, ttl);

        // Evict if over capacity (simple random eviction)
        if self.wallet_cache.len() >= self.config.wallet_cache_size {
            // Remove ~10% of entries
            let to_remove = self.config.wallet_cache_size / 10;
            let keys: Vec<_> = self
                .wallet_cache
                .iter()
                .take(to_remove)
                .map(|r| r.key().clone())
                .collect();
            for key in keys {
                self.wallet_cache.remove(&key);
            }
        }

        self.wallet_cache.insert(address.to_string(), entry);
    }

    /// Get token holders from cache
    pub fn get_holders(&self, mint: &str) -> Option<Vec<TokenHolderInfo>> {
        if let Some(entry) = self.holder_cache.get(mint) {
            if !entry.is_expired() {
                return Some(entry.holders.clone());
            }
            drop(entry);
            self.holder_cache.remove(mint);
        }
        None
    }

    /// Store token holders in cache
    pub fn set_holders(&self, mint: &str, holders: Vec<TokenHolderInfo>) {
        let ttl = Duration::from_secs(self.config.score_cache_ttl_secs);
        let entry = CachedHolders::new(holders, ttl);
        self.holder_cache.insert(mint.to_string(), entry);
    }

    /// Get mint info from cache
    pub fn get_mint_info(&self, mint: &str) -> Option<MintInfo> {
        if let Some(entry) = self.mint_info_cache.get(mint) {
            if !entry.is_expired() {
                return Some(entry.info.clone());
            }
            drop(entry);
            self.mint_info_cache.remove(mint);
        }
        None
    }

    /// Store mint info in cache
    pub fn set_mint_info(&self, mint: &str, info: MintInfo) {
        let ttl = Duration::from_secs(self.config.score_cache_ttl_secs);
        let entry = CachedMintInfo::new(info, ttl);
        self.mint_info_cache.insert(mint.to_string(), entry);
    }

    /// Check if wallet is a known deployer (fast, cached)
    pub async fn is_known_deployer(&self, address: &str) -> bool {
        self.stats.record_known_actor_check();
        let actors = self.known_actors.read().await;
        actors.is_known_deployer(address)
    }

    /// Check if wallet is a known sniper (fast, cached)
    pub async fn is_known_sniper(&self, address: &str) -> bool {
        self.stats.record_known_actor_check();
        let actors = self.known_actors.read().await;
        actors.is_known_sniper(address)
    }

    /// Check if wallet is trusted (fast, cached)
    pub async fn is_trusted(&self, address: &str) -> bool {
        let actors = self.known_actors.read().await;
        actors.is_trusted(address)
    }

    /// Load known actors from files
    pub async fn load_known_actors(
        &self,
        deployers_path: Option<&str>,
        snipers_path: Option<&str>,
        trusted_path: Option<&str>,
    ) {
        let actors = KnownActors::load_from_files(deployers_path, snipers_path, trusted_path);
        let (d, s, t) = actors.stats();
        tracing::info!(
            deployers = d,
            snipers = s,
            trusted = t,
            "Loaded known actors"
        );
        *self.known_actors.write().await = actors;
    }

    /// Add a deployer to the blacklist
    pub async fn add_known_deployer(&self, address: String) {
        self.known_actors.write().await.add_deployer(address);
    }

    /// Add a sniper to the list
    pub async fn add_known_sniper(&self, address: String) {
        self.known_actors.write().await.add_sniper(address);
    }

    /// Get cache statistics
    pub fn stats(&self) -> &CacheStats {
        &self.stats
    }

    /// Get current cache size
    pub fn wallet_cache_size(&self) -> usize {
        self.wallet_cache.len()
    }

    /// Clear all caches
    pub async fn clear(&self) {
        self.wallet_cache.clear();
        self.holder_cache.clear();
        self.mint_info_cache.clear();
        *self.known_actors.write().await = KnownActors::default();
    }

    /// Check if we have enriched data for a token (holders + mint info)
    pub fn has_token_data(&self, mint: &str) -> bool {
        self.holder_cache.contains_key(mint) || self.mint_info_cache.contains_key(mint)
    }

    /// Get total number of cached items
    pub fn total_cached_items(&self) -> usize {
        self.wallet_cache.len() + self.holder_cache.len() + self.mint_info_cache.len()
    }
}

impl Default for FilterCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn test_known_actors() {
        let mut actors = KnownActors::default();
        actors.add_deployer("deployer1".to_string());
        actors.add_sniper("sniper1".to_string());
        actors.add_trusted("trusted1".to_string());

        assert!(actors.is_known_deployer("deployer1"));
        assert!(!actors.is_known_deployer("unknown"));
        assert!(actors.is_known_sniper("sniper1"));
        assert!(actors.is_trusted("trusted1"));
    }

    #[tokio::test]
    async fn test_filter_cache() {
        let cache = FilterCache::new();

        // Test wallet cache
        let history = WalletHistory {
            address: "test".to_string(),
            total_trades: 100,
            fetched_at: Utc::now(),
            ..Default::default()
        };

        cache.set_wallet("test", history.clone());
        let retrieved = cache.get_wallet("test");
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().total_trades, 100);
    }

    #[tokio::test]
    async fn test_known_actors_async() {
        let cache = FilterCache::new();
        cache.add_known_deployer("bad_actor".to_string()).await;

        assert!(cache.is_known_deployer("bad_actor").await);
        assert!(!cache.is_known_deployer("good_actor").await);
    }

    #[test]
    fn test_cache_stats() {
        let cache = FilterCache::new();

        // Generate some hits and misses
        cache.get_wallet("nonexistent"); // miss
        cache.set_wallet(
            "exists",
            WalletHistory {
                address: "exists".to_string(),
                fetched_at: chrono::Utc::now(),
                ..Default::default()
            },
        );
        cache.get_wallet("exists"); // hit
        cache.get_wallet("exists"); // hit

        let stats = cache.stats();
        let hits = stats.wallet_hits.load(std::sync::atomic::Ordering::Relaxed);
        let misses = stats
            .wallet_misses
            .load(std::sync::atomic::Ordering::Relaxed);

        assert_eq!(hits, 2);
        assert_eq!(misses, 1);
        assert!((stats.hit_rate() - 0.666).abs() < 0.01);
    }
}
